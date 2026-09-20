mod request;
mod response;
mod gvl_helpers;
mod grpc;
mod syslog;

use hyper_util::server::graceful::GracefulShutdown;
use request::{Request, GrpcRequest};
use response::{Response, GrpcResponse};
use gvl_helpers::nogvl;

use magnus::block::block_proc;
use magnus::typed_data::Obj;
use magnus::{function, method, prelude::*, value::Opaque, Error as MagnusError, IntoValue, RHash, Ruby, Value, RString};
use bytes::Bytes;
use tokio::io::{AsyncRead, AsyncWrite};

use std::cell::RefCell;
use std::net::SocketAddr;
use std::os::unix::fs::MetadataExt;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::net::{TcpListener, UnixListener};

use std::sync::Arc;
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;
use crossbeam_channel;

use hyper::service::service_fn;
use hyper::{Error, Request as HyperRequest, Response as HyperResponse, StatusCode};
use hyper::body::{Body, Incoming};
use hyper_util::rt::TokioIo;
use hyper_util::server::conn::auto;
use http_body_util::BodyExt;

use jemallocator::Jemalloc;

use log::{debug, info, warn, error};

use env_logger;
use crate::response::BodyWithTrailers;
use std::sync::Once;
use tokio::time::timeout;

use std::io;

use tokio::sync::broadcast;

static LOGGER_INIT: Once = Once::new();

// How long stop() waits for the syslog listeners to finish delivering messages
// they have already framed.
const SYSLOG_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

trait AsyncStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> AsyncStream for T {}

enum Listener {
    Unix(UnixListener),
    Tcp(TcpListener),
}

impl Listener {
    async fn accept(&self) -> io::Result<(Box<dyn AsyncStream>, SocketAddr)> {
        match self {
            Listener::Unix(l) => {
                let (stream, _) = l.accept().await?;
                Ok((Box::new(stream), "0.0.0.0:0".parse().unwrap()))
            }
            Listener::Tcp(l) => {
                let (stream, addr) = l.accept().await?;
                Ok((Box::new(stream), addr))
            }
        }
    }
}

#[derive(Clone)]
struct ServerConfig {
    bind_address: String,
    tokio_threads: Option<usize>,
    debug: bool,
    recv_timeout: u64,
    channel_capacity: usize,
    send_timeout: u64,
    max_connection_age: Option<u64>,
    syslog: syslog::SyslogConfig,
}

impl ServerConfig {
    fn new() -> Self {
        Self {
            bind_address: String::from("127.0.0.1:3000"),
            tokio_threads: None,
            debug: false,
            recv_timeout: 30000, // Default 30 second timeout
            channel_capacity: 5000, // Default capacity for worker channel
            send_timeout: 1000, // Default 1 second timeout for send backpressure
            max_connection_age: None, // No limit by default
            syslog: syslog::SyslogConfig::new(),
        }
    }
}

// Sent on the work channel with the request, and a oneshot channel to send the response back on.
struct RequestWithCompletion {
    request: HyperRequest<Bytes>,
    response_tx: oneshot::Sender<HyperResponse<BodyWithTrailers>>,
}

// A unit of work for a Ruby worker thread; HTTP requests and syslog messages
// have their own channels but share the worker threads.
enum WorkItem {
    Http(RequestWithCompletion),
    Syslog(syslog::SyslogDelivery),
}

#[magnus::wrap(class = "HyperRuby::Server")]
struct Server {
    server_handle: Arc<Mutex<Option<JoinHandle<()>>>>,
    config: RefCell<ServerConfig>,
    work_rx: RefCell<Option<crossbeam_channel::Receiver<RequestWithCompletion>>>,
    work_tx: RefCell<Option<Arc<crossbeam_channel::Sender<RequestWithCompletion>>>>,
    runtime: RefCell<Option<Arc<tokio::runtime::Runtime>>>,
    shutdown: RefCell<Option<broadcast::Sender<()>>>,
    total_connections: Arc<AtomicU64>,
    // (dev, ino) of the Unix socket file this server bound, so stop() only removes
    // the file if a replacement server hasn't taken over the path in the meantime.
    socket_ident: RefCell<Option<(u64, u64)>>,
    syslog_work_rx: RefCell<Option<crossbeam_channel::Receiver<syslog::SyslogDelivery>>>,
    syslog_work_tx: RefCell<Option<Arc<crossbeam_channel::Sender<syslog::SyslogDelivery>>>>,
    syslog_handler: RefCell<Option<Opaque<Value>>>,
    syslog_counters: Arc<syslog::SyslogCounters>,
    syslog_runtime: RefCell<Option<syslog::SyslogRuntime>>,
}

impl Server {
    pub fn new() -> Self {
        let config = ServerConfig::new();
        Self {
            server_handle: Arc::new(Mutex::new(None)),
            config: RefCell::new(config),
            work_rx: RefCell::new(None),
            work_tx: RefCell::new(None),
            runtime: RefCell::new(None),
            shutdown: RefCell::new(None),
            total_connections: Arc::new(AtomicU64::new(0)),
            socket_ident: RefCell::new(None),
            syslog_work_rx: RefCell::new(None),
            syslog_work_tx: RefCell::new(None),
            syslog_handler: RefCell::new(None),
            syslog_counters: Arc::new(syslog::SyslogCounters::default()),
            syslog_runtime: RefCell::new(None),
        }
    }

    pub fn total_connections(&self) -> u64 {
        self.total_connections.load(Ordering::Relaxed)
    }

    pub fn configure(&self, config: magnus::RHash) -> Result<(), MagnusError> {
        let mut server_config = self.config.borrow_mut();
        if let Some(bind_address) = config.get(magnus::Symbol::new("bind_address")) {
            server_config.bind_address = String::try_convert(bind_address)?;
        }

        if let Some(tokio_threads) = config.get(magnus::Symbol::new("tokio_threads")) {
            server_config.tokio_threads = Some(usize::try_convert(tokio_threads)?);
        }

        if let Some(debug) = config.get(magnus::Symbol::new("debug")) {
            server_config.debug = bool::try_convert(debug)?;
        }

        if let Some(recv_timeout) = config.get(magnus::Symbol::new("recv_timeout")) {
            server_config.recv_timeout = u64::try_convert(recv_timeout)?;
        }

        if let Some(channel_capacity) = config.get(magnus::Symbol::new("channel_capacity")) {
            server_config.channel_capacity = usize::try_convert(channel_capacity)?;
        }
        
        if let Some(send_timeout) = config.get(magnus::Symbol::new("send_timeout")) {
            server_config.send_timeout = u64::try_convert(send_timeout)?;
        }

        if let Some(max_connection_age) = config.get(magnus::Symbol::new("max_connection_age")) {
            server_config.max_connection_age = Some(u64::try_convert(max_connection_age)?);
        }

        server_config.syslog.apply(&config)?;

        if let Some(handler) = config.get(magnus::Symbol::new("syslog_handler")) {
            // Worker threads hold no other reference to the handler, so keep it
            // marked for the life of the process.
            magnus::gc::register_mark_object(handler);
            *self.syslog_handler.borrow_mut() = Some(Opaque::from(handler));
        }

        // Initialize logging if not already initialized
        LOGGER_INIT.call_once(|| {
            let mut builder = env_logger::Builder::from_env(env_logger::Env::default());
            
            // Always enable warn and error levels
            builder.filter_level(log::LevelFilter::Warn);
            
            // If debug is enabled, show all log levels
            if server_config.debug {
                builder.filter_level(log::LevelFilter::Debug);
            }
            
            builder.write_style(env_logger::WriteStyle::Always)
                .init();
        });

        Ok(())
    }

    // Method that Ruby worker threads will call with a block
    pub fn run_worker(&self) -> Result<(), MagnusError> {
        let block = block_proc().unwrap();
        let ruby = Ruby::get().map_err(|e| {
            MagnusError::new(magnus::exception::runtime_error(), format!("Workers must run on a Ruby thread: {:?}", e))
        })?;

        // Check if we have a work_rx channel, error out if not
        let work_rx = self.work_rx.borrow().as_ref().ok_or_else(|| {
            MagnusError::new(magnus::exception::runtime_error(), "Server must be started before running workers")
        })?.clone();

        let syslog_rx = self.syslog_work_rx.borrow().as_ref().ok_or_else(|| {
            MagnusError::new(magnus::exception::runtime_error(), "Server must be started before running workers")
        })?.clone();

        let syslog_handler = self.syslog_handler.borrow().map(|handler| ruby.get_inner(handler));
        let syslog_enabled = self.config.borrow().syslog.enabled();
        let mut prefer_syslog = false;

        loop {
            let next = if syslog_enabled {
                next_work_item(&work_rx, &syslog_rx, &mut prefer_syslog)
            } else {
                next_http_work_item(&work_rx)
            };

            let work_item = match next {
                Some(work_item) => work_item,
                None => break,
            };

            match work_item {
                WorkItem::Http(work_request) => {
                    let hyper_request = work_request.request;
                    
                    debug!("Processing request:");
                    debug!("  Method: {}", hyper_request.method());
                    debug!("  Path: {}", hyper_request.uri().path());
                    debug!("  Headers: {:?}", hyper_request.headers());
                    
                    // Convert to appropriate request type
                    let value = if grpc::is_grpc_request(&hyper_request) {
                        debug!("Request identified as gRPC");
                        if let Some(grpc_request) = GrpcRequest::new(hyper_request) {
                            grpc_request.into_value()
                        } else {
                            error!("Failed to create GrpcRequest due to invalid path - returning gRPC error");
                            // Invalid gRPC request path
                            let response = GrpcResponse::error(3_u32.into_value(), RString::new("Invalid gRPC request path")).unwrap()
                                .into_hyper_response();
                            work_request.response_tx.send(response).unwrap_or_else(|e| error!("Failed to send response: {:?}", e));
                            continue;
                        }
                    } else {
                        debug!("Request identified as HTTP");
                        Request::new(hyper_request).into_value()
                    };

                    let hyper_response = match block.call::<_, Value>([value]) {
                        Ok(result) => {
                            // Try to convert to either Response or GrpcResponse
                            if let Ok(grpc_response) = Obj::<GrpcResponse>::try_convert(result) {
                                (*grpc_response).clone().into_hyper_response()
                            } else if let Ok(http_response) = Obj::<Response>::try_convert(result) {
                                (*http_response).clone().into_hyper_response()
                            } else {
                                error!("Block returned invalid response type - returning 500 Internal Server Error");
                                create_error_response("Internal server error")
                            }
                        },
                        Err(e) => {
                            error!("Block call failed with error: {:?} - returning 500 Internal Server Error", e);
                            create_error_response("Internal server error")
                        }
                    };

                    match work_request.response_tx.send(hyper_response) {
                        Ok(_) => (),
                        Err(e) => error!("Failed to send response back to client: {:?} - response dropped", e),
                    }
                }
                WorkItem::Syslog(delivery) => {
                    let accepted = call_syslog_handler(syslog_handler, &delivery);
                    if delivery.result_tx.send(accepted).is_err() {
                        debug!("Syslog listener stopped waiting for an admission result");
                    }
                }
            }
        }

        Ok(())
    }

    pub fn start(&self) -> Result<(), MagnusError> {
        let config = self.config.borrow().clone();
        
        // Create the channel with the current configuration
        let (work_tx, work_rx) = crossbeam_channel::bounded(config.channel_capacity);
        debug!("Created channel with capacity: {}", config.channel_capacity);
        
        // Store the channel
        *self.work_rx.borrow_mut() = Some(work_rx);
        let work_tx = Arc::new(work_tx);
        *self.work_tx.borrow_mut() = Some(work_tx.clone());

        // Syslog messages queue separately but are drained by the same workers.
        let (syslog_work_tx, syslog_work_rx) = crossbeam_channel::bounded(config.channel_capacity);
        *self.syslog_work_rx.borrow_mut() = Some(syslog_work_rx);
        let syslog_work_tx = Arc::new(syslog_work_tx);
        *self.syslog_work_tx.borrow_mut() = Some(syslog_work_tx.clone());

        let (shutdown_tx, shutdown_rx) = broadcast::channel(1);
        *self.shutdown.borrow_mut() = Some(shutdown_tx.clone());

        let total_connections = self.total_connections.clone();

        let mut rt_builder = tokio::runtime::Builder::new_multi_thread();
            
        rt_builder.enable_all();

        if let Some(tokio_threads) = config.tokio_threads {
            rt_builder.worker_threads(tokio_threads);
        }

        let rt = Arc::new(rt_builder
            .build()
            .map_err(|e| MagnusError::new(magnus::exception::runtime_error(), e.to_string()))?);

        *self.runtime.borrow_mut() = Some(rt.clone());


        let syslog_config = config.syslog.clone();
        let syslog_counters = self.syslog_counters.clone();
        let syslog_shutdown_tx = shutdown_tx.clone();

        rt.block_on(async move {
            // Instead of spawning a task, we'll run the server setup inline first to catch binding errors
            // Setup listener and http server components
            let timer = hyper_util::rt::TokioTimer::new();
            let mut builder = auto::Builder::new(hyper_util::rt::TokioExecutor::new());
            builder.http1()
                .header_read_timeout(std::time::Duration::from_millis(config.recv_timeout))
                .timer(timer.clone());
            builder.http2()
                .keep_alive_interval(std::time::Duration::from_secs(10))
                .timer(timer);

            // Create the listener with proper error handling
            let listener = if config.bind_address.starts_with("unix:") {
                let path = config.bind_address.trim_start_matches("unix:");

                // Bind to a unique temp path and atomically rename over the target, so the
                // path always points at a live socket even when a replacement server takes
                // over a path an older, still-draining server bound.
                static SOCKET_TMP_SEQ: AtomicU64 = AtomicU64::new(0);
                let tmp_path = format!("{}.{}.{}.tmp", path, std::process::id(), SOCKET_TMP_SEQ.fetch_add(1, Ordering::Relaxed));

                let listener = match UnixListener::bind(&tmp_path) {
                    Ok(listener) => listener,
                    Err(e) => {
                        error!("Failed to bind to Unix socket {}: {}", tmp_path, e);
                        return Err(MagnusError::new(
                            magnus::exception::runtime_error(),
                            format!("Failed to bind to Unix socket {}: {}", tmp_path, e)
                        ));
                    }
                };

                // The socket file's identity survives the rename; stop() compares against it
                // so an older server generation never unlinks a newer generation's socket.
                let ident = std::fs::symlink_metadata(&tmp_path).ok().map(|m| (m.dev(), m.ino()));

                if let Err(e) = std::fs::rename(&tmp_path, path) {
                    let _ = std::fs::remove_file(&tmp_path);
                    error!("Failed to install Unix socket file {}: {}", path, e);
                    return Err(MagnusError::new(
                        magnus::exception::runtime_error(),
                        format!("Failed to install Unix socket file {}: {}", path, e)
                    ));
                }

                *self.socket_ident.borrow_mut() = ident;

                Listener::Unix(listener)
            } else {
                match config.bind_address.parse::<SocketAddr>() {
                    Ok(addr) => {
                        match TcpListener::bind(addr).await {
                            Ok(listener) => Listener::Tcp(listener),
                            Err(e) => {
                                error!("Failed to bind to address {}: {}", addr, e);
                                return Err(MagnusError::new(
                                    magnus::exception::runtime_error(),
                                    format!("Failed to bind to address {}: {}", addr, e)
                                ));
                            }
                        }
                    },
                    Err(e) => {
                        error!("Invalid address format {}: {}", config.bind_address, e);
                        return Err(MagnusError::new(
                            magnus::exception::runtime_error(),
                            format!("Invalid address format {}: {}", config.bind_address, e)
                        ));
                    }
                }
            };

            // Now that we have successfully bound, spawn the server task
            let max_connection_age = config.max_connection_age;
            let server_task = tokio::spawn(async move {
                let graceful_shutdown = GracefulShutdown::new();
                let mut shutdown_rx = shutdown_rx;

                loop {
                    tokio::select! {
                        Ok((stream, _)) = listener.accept() => {
                            total_connections.fetch_add(1, Ordering::Relaxed);
                            info!("New connection established");

                            let io = TokioIo::new(stream);

                            debug!("Setting up connection");

                            let builder = builder.clone();
                            let work_tx = work_tx.clone();
                            let conn = builder.serve_connection(io, service_fn(move |req: HyperRequest<Incoming>| {
                                debug!("Service handling request");
                                handle_request(req, work_tx.clone(), config.recv_timeout, config.send_timeout)
                            }));
                            // If max_connection_age is set, handle the connection with a timeout
                            // but still integrate with server-wide graceful shutdown via broadcast channel
                            if let Some(max_age_ms) = max_connection_age {
                                let conn = conn.into_owned();
                                let mut conn_shutdown_rx = shutdown_tx.subscribe();
                                tokio::task::spawn(async move {
                                    tokio::pin!(conn);
                                    let sleep = tokio::time::sleep(std::time::Duration::from_millis(max_age_ms));
                                    tokio::pin!(sleep);
                                    let mut graceful_shutdown_started = false;

                                    loop {
                                        tokio::select! {
                                            result = conn.as_mut() => {
                                                if let Err(err) = result {
                                                    warn!("Error serving connection: {:?}", err);
                                                }
                                                break;
                                            }
                                            _ = &mut sleep, if !graceful_shutdown_started => {
                                                debug!("Connection reached max age ({}ms), sending GOAWAY", max_age_ms);
                                                conn.as_mut().graceful_shutdown();
                                                graceful_shutdown_started = true;
                                                // Continue the loop to let the connection drain
                                            }
                                            _ = conn_shutdown_rx.recv(), if !graceful_shutdown_started => {
                                                debug!("Server shutdown requested, sending GOAWAY to connection");
                                                conn.as_mut().graceful_shutdown();
                                                graceful_shutdown_started = true;
                                                // Continue the loop to let the connection drain
                                            }
                                        }
                                    }
                                });
                            } else {
                                // No max age, use the graceful shutdown watcher
                                let fut = graceful_shutdown.watch(conn.into_owned());
                                tokio::task::spawn(async move {
                                    if let Err(err) = fut.await {
                                        warn!("Error serving connection: {:?}", err);
                                    }
                                });
                            }
                        },
                        _ = shutdown_rx.recv() => {
                            debug!("Graceful shutdown requested; shutting down");
                            break;
                        }
                    }
                }

                tokio::select! {
                    _ = graceful_shutdown.shutdown() => {
                        debug!("all connections gracefully closed");
                    },
                    _ = tokio::time::sleep(std::time::Duration::from_secs(10)) => {
                        error!("timed out wait for all connections to close");
                    }
                }
            });

            if syslog_config.enabled() {
                match syslog::start(&syslog_config, syslog_counters, syslog_work_tx, &syslog_shutdown_tx) {
                    Ok(syslog_runtime) => *self.syslog_runtime.borrow_mut() = Some(syslog_runtime),
                    Err(e) => {
                        let _ = syslog_shutdown_tx.send(());
                        error!("Failed to start syslog listeners: {}", e);
                        return Err(MagnusError::new(
                            magnus::exception::runtime_error(),
                            format!("Failed to start syslog listeners: {}", e)
                        ));
                    }
                }
            }

            let mut handle = self.server_handle.lock().await;
            *handle = Some(server_task);

            Ok::<(), MagnusError>(())
        })?;
            
        Ok(())
    }

    // True while the configured syslog listeners are bound and accepting.
    pub fn syslog_listening(&self) -> bool {
        self.syslog_runtime.borrow().as_ref().map(|runtime| runtime.listening()).unwrap_or(false)
    }

    pub fn syslog_stats(&self) -> Result<RHash, MagnusError> {
        self.syslog_counters.to_hash()
    }

    pub fn stop(&self) -> Result<(), MagnusError> {
        if let Some(rt) = self.runtime.borrow().as_ref() {
            if let Some(shutdown) = self.shutdown.borrow().as_ref() {
                let _ = shutdown.send(());
            }

            rt.block_on(async {
                let mut handle = self.server_handle.lock().await;
                if let Some(task) = handle.take() {
                    task.await.unwrap_or_else(|e| warn!("Server task failed: {:?}", e));
                }
            });

            if let Some(syslog_runtime) = self.syslog_runtime.borrow().as_ref() {
                // Release the GVL so worker threads can accept the messages the
                // listeners have already framed.
                nogvl(|| rt.block_on(syslog_runtime.drain(SYSLOG_DRAIN_TIMEOUT)));
            }
        }

        if let Some(syslog_runtime) = self.syslog_runtime.borrow_mut().take() {
            syslog_runtime.remove_socket_file();
        }

        // Drop the channel and runtime
        self.work_tx.borrow_mut().take();
        self.syslog_work_tx.borrow_mut().take();
        self.runtime.borrow_mut().take();
        self.shutdown.borrow_mut().take();

        let bind_address = self.config.borrow().bind_address.clone();
        if bind_address.starts_with("unix:") {
            let path = bind_address.trim_start_matches("unix:");
            // Only remove the socket file if it's still the one this server bound; a
            // replacement server may have taken over the path while we were draining.
            match (self.socket_ident.borrow_mut().take(), std::fs::symlink_metadata(path)) {
                (Some((dev, ino)), Ok(meta)) if (meta.dev(), meta.ino()) == (dev, ino) => {
                    std::fs::remove_file(path).unwrap_or_else(|e| {
                        warn!("Failed to remove socket file: {:?}", e);
                    });
                }
                (Some(_), Ok(_)) => info!("Socket file {} was replaced by another server; leaving it in place", path),
                (Some(_), Err(_)) => debug!("Socket file {} already removed", path),
                (None, _) => {}
            }
        }

        Ok(())
    }
}

// Take the next request for a Ruby worker when no syslog listener is running.
fn next_http_work_item(
    work_rx: &crossbeam_channel::Receiver<RequestWithCompletion>,
) -> Option<WorkItem> {
    // try getting the next request without yielding the GVL, if there's nothing, wait for one
    match work_rx.try_recv() {
        Ok(request) => Some(WorkItem::Http(request)),
        Err(crossbeam_channel::TryRecvError::Empty) => {
            nogvl(|| work_rx.recv()).ok().map(WorkItem::Http)
        },
        Err(crossbeam_channel::TryRecvError::Disconnected) => None,
    }
}

// Take the next piece of work for a Ruby worker, taking whatever is already
// queued so we only release the GVL when both channels are empty. The channels
// alternate which one is asked first, and the blocking select picks uniformly
// between them, so neither transport starves the other.
fn next_work_item(
    work_rx: &crossbeam_channel::Receiver<RequestWithCompletion>,
    syslog_rx: &crossbeam_channel::Receiver<syslog::SyslogDelivery>,
    prefer_syslog: &mut bool,
) -> Option<WorkItem> {
    let syslog_first = *prefer_syslog;
    *prefer_syslog = !syslog_first;

    if syslog_first {
        match try_syslog_work_item(syslog_rx) {
            TryWork::Empty => (),
            outcome => return outcome.into_work_item(),
        }
    }

    match try_http_work_item(work_rx) {
        TryWork::Empty => (),
        outcome => return outcome.into_work_item(),
    }

    if !syslog_first {
        match try_syslog_work_item(syslog_rx) {
            TryWork::Empty => (),
            outcome => return outcome.into_work_item(),
        }
    }

    nogvl(|| {
        crossbeam_channel::select! {
            recv(work_rx) -> request => request.ok().map(WorkItem::Http),
            recv(syslog_rx) -> delivery => delivery.ok().map(WorkItem::Syslog),
        }
    })
}

enum TryWork {
    Found(WorkItem),
    Empty,
    Closed,
}

impl TryWork {
    fn into_work_item(self) -> Option<WorkItem> {
        match self {
            TryWork::Found(work_item) => Some(work_item),
            // A closed channel stops the worker, as it always has.
            TryWork::Empty | TryWork::Closed => None,
        }
    }
}

fn try_http_work_item(work_rx: &crossbeam_channel::Receiver<RequestWithCompletion>) -> TryWork {
    match work_rx.try_recv() {
        Ok(request) => TryWork::Found(WorkItem::Http(request)),
        Err(crossbeam_channel::TryRecvError::Empty) => TryWork::Empty,
        Err(crossbeam_channel::TryRecvError::Disconnected) => TryWork::Closed,
    }
}

fn try_syslog_work_item(syslog_rx: &crossbeam_channel::Receiver<syslog::SyslogDelivery>) -> TryWork {
    match syslog_rx.try_recv() {
        Ok(delivery) => TryWork::Found(WorkItem::Syslog(delivery)),
        Err(crossbeam_channel::TryRecvError::Empty) => TryWork::Empty,
        Err(crossbeam_channel::TryRecvError::Disconnected) => TryWork::Closed,
    }
}

// Hand one syslog message to the configured handler; anything other than a
// truthy result means the message was not admitted.
fn call_syslog_handler(handler: Option<Value>, delivery: &syslog::SyslogDelivery) -> bool {
    let Some(handler) = handler else {
        error!("Syslog message received but no syslog_handler is configured - refusing");
        return false;
    };

    // Stream frames are validated during framing; datagrams stay binary.
    let message = match std::str::from_utf8(&delivery.message) {
        Ok(text) if delivery.utf8 => RString::new(text),
        _ => RString::from_slice(&delivery.message),
    };

    let args = (
        message,
        RString::new(&delivery.peer),
        delivery.transport.symbol(),
        delivery.received_at_ns,
    );

    match handler.funcall::<_, _, Value>("call", args) {
        Ok(result) => result.to_bool(),
        Err(e) => {
            error!("Syslog handler raised {:?} - treating the message as refused", e);
            false
        }
    }
}

async fn handle_request(
    req: HyperRequest<Incoming>,
    work_tx: Arc<crossbeam_channel::Sender<RequestWithCompletion>>,
    recv_timeout: u64,
    send_timeout: u64,
) -> Result<HyperResponse<BodyWithTrailers>, Error> {
    debug!("Received request: {:?}", req);
    debug!("HTTP version: {:?}", req.version());
    debug!("Headers: {:?}", req.headers());

    let (parts, body) = req.into_parts();

    // Capture the declared body length before consuming the body. Hyper's
    // `Incoming::size_hint().exact()` mirrors the inbound Content-Length
    // (populated from the header for H2 as well as H1), and is the only
    // signal we have here: hyper converts `RST_STREAM(NO_ERROR|CANCEL)` —
    // the codes browsers send on navigation cancellation — into a clean
    // end-of-body rather than surfacing the reset (see
    // hyper-1.6.0/src/body/incoming.rs:~249). We therefore validate the
    // declared length against what we actually collected.
    let declared_len = body.size_hint().exact();

    // Collect the body with timeout
    let body_bytes = match timeout(
        std::time::Duration::from_millis(recv_timeout),
        body.collect()
    ).await {
        Ok(Ok(collected)) => collected.to_bytes(),
        Ok(Err(e)) => {
            debug!("Error collecting body: {:?}", e);
            return Err(e);
        },
        Err(_) => {
            debug!("Timeout collecting body");
            return Ok(create_timeout_response());
        }
    };

    debug!("Collected body size: {}", body_bytes.len());

    if let Some(declared) = declared_len {
        if declared != body_bytes.len() as u64 {
            warn!(
                "Body truncated: declared {} bytes, received {} — likely RST_STREAM(CANCEL); rejecting request",
                declared, body_bytes.len()
            );
            return Ok(create_bad_request_response("Body length does not match Content-Length"));
        }
    }

    let hyper_request = HyperRequest::from_parts(parts, body_bytes);
    let is_grpc = grpc::is_grpc_request(&hyper_request);
    debug!("Is gRPC: {}", is_grpc);

    let (response_tx, response_rx) = oneshot::channel();

    let with_completion = RequestWithCompletion {
        request: hyper_request,
        response_tx,
    };

    // First try non-blocking send
    match work_tx.try_send(with_completion) {
        Ok(()) => {
            debug!("Successfully queued request (fast path)");
        },
        Err(crossbeam_channel::TrySendError::Full(mut completion)) => {
            // Channel is full, implement polling with short delays
            debug!("Channel full, attempting to send with polling");
            
            // Use polling with sleep to implement backpressure
            let start = std::time::Instant::now();
            let max_wait = std::time::Duration::from_millis(send_timeout);
            let delay_ms = 10; // 10ms delay between attempts
            
            // Create a new oneshot channel for each attempt to avoid cloning
            let mut attempts = 0;
            loop {
                // Check if we've reached the timeout
                if start.elapsed() >= max_wait {
                    warn!("Channel full after timeout - returning 429 Too Many Requests");
                    return Ok(if is_grpc {
                        grpc::create_grpc_error_response(429, 8, "Server too busy, try again later") // RESOURCE_EXHAUSTED = 8
                    } else {
                        create_too_many_requests_response("Server too busy, try again later")
                    });
                }
                
                // Sleep for a short delay before trying again
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                
                // Try to send again
                attempts += 1;
                debug!("Retry attempt {} after {}ms", attempts, delay_ms);
                match work_tx.try_send(completion) {
                    Ok(()) => {
                        debug!("Successfully queued request after {} polling attempts", attempts);
                        break;
                    },
                    Err(crossbeam_channel::TrySendError::Full(returned_completion)) => {
                        // Get our completion request back and try again
                        completion = returned_completion;
                    },
                    Err(crossbeam_channel::TrySendError::Disconnected(_)) => {
                        error!("Worker channel disconnected - server is shutting down, returning 500");
                        return Ok(if is_grpc {
                            grpc::create_grpc_error_response(500, 13, "Server shutting down")
                        } else {
                            create_error_response("Server shutting down")
                        });
                    }
                }
            }
        },
        Err(crossbeam_channel::TrySendError::Disconnected(_)) => {
            error!("Worker channel disconnected - server is shutting down, returning 500");
            return Ok(if is_grpc {
                grpc::create_grpc_error_response(500, 13, "Server shutting down")
            } else {
                create_error_response("Server shutting down")
            });
        }
    }

    match response_rx.await {
        Ok(response) => {
            debug!("Got response: {:?}", response);
            Ok(response)
        }
        Err(_) => {
            error!("Failed to receive response from worker - returning 500 Internal Server Error");
            Ok(if is_grpc {
                grpc::create_grpc_error_response(500, 13, "Failed to get response")
            } else {
                create_error_response("Failed to get response")
            })
        }
    }
}

fn create_timeout_response() -> HyperResponse<BodyWithTrailers> {
    let builder = HyperResponse::builder()
        .status(StatusCode::REQUEST_TIMEOUT)
        .header("content-type", "text/plain");
    
    builder.body(BodyWithTrailers::new(Bytes::from("Request timed out while receiving body"), None))
        .unwrap()
}

// Helper function to create error responses
fn create_error_response(error_message: &str) -> HyperResponse<BodyWithTrailers> {
    // For non-gRPC requests, return a plain HTTP error
    let builder = HyperResponse::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .header("content-type", "text/plain");
    
    builder.body(BodyWithTrailers::new(Bytes::from(error_message.to_string()), None))
        .unwrap()
}

// Helper function to create too many requests responses
fn create_too_many_requests_response(error_message: &str) -> HyperResponse<BodyWithTrailers> {
    let builder = HyperResponse::builder()
        .status(StatusCode::TOO_MANY_REQUESTS)
        .header("content-type", "text/plain");

    builder.body(BodyWithTrailers::new(Bytes::from(error_message.to_string()), None))
        .unwrap()
}

fn create_bad_request_response(error_message: &str) -> HyperResponse<BodyWithTrailers> {
    HyperResponse::builder()
        .status(StatusCode::BAD_REQUEST)
        .header("content-type", "text/plain")
        .body(BodyWithTrailers::new(Bytes::from(error_message.to_string()), None))
        .unwrap()
}

#[magnus::init]
fn init(ruby: &Ruby) -> Result<(), MagnusError> {
    let module = ruby.define_module("HyperRuby")?;

    let server_class = module.define_class("Server", ruby.class_object())?;
    server_class.define_singleton_method("new", function!(Server::new, 0))?;
    server_class.define_method("configure", method!(Server::configure, 1))?;
    server_class.define_method("start", method!(Server::start, 0))?;
    server_class.define_method("stop", method!(Server::stop, 0))?;
    server_class.define_method("run_worker", method!(Server::run_worker, 0))?;
    server_class.define_method("total_connections", method!(Server::total_connections, 0))?;
    server_class.define_method("syslog_listening?", method!(Server::syslog_listening, 0))?;
    server_class.define_method("syslog_stats", method!(Server::syslog_stats, 0))?;

    let response_class = module.define_class("Response", ruby.class_object())?;
    response_class.define_singleton_method("new", function!(Response::new, 3))?;
    response_class.define_method("status", method!(Response::status, 0))?;
    response_class.define_method("headers", method!(Response::headers, 0))?;
    response_class.define_method("body", method!(Response::body, 0))?;

    let grpc_response_class = module.define_class("GrpcResponse", ruby.class_object())?;
    grpc_response_class.define_singleton_method("new", function!(GrpcResponse::new, 2))?;
    grpc_response_class.define_singleton_method("error", function!(GrpcResponse::error, 2))?;
    grpc_response_class.define_method("status", method!(GrpcResponse::status, 0))?;
    grpc_response_class.define_method("headers", method!(GrpcResponse::headers, 0))?;
    grpc_response_class.define_method("body", method!(GrpcResponse::body, 0))?;

    let request_class = module.define_class("Request", ruby.class_object())?;
    request_class.define_method("http_method", method!(Request::method, 0))?;
    request_class.define_method("path", method!(Request::path, 0))?;
    request_class.define_method("query_params", method!(Request::query_params, 0))?;
    request_class.define_method("query_param", method!(Request::query_param, 1))?;
    request_class.define_method("host", method!(Request::host, 0))?;
    request_class.define_method("header", method!(Request::header, 1))?;
    request_class.define_method("headers", method!(Request::headers, 0))?;
    request_class.define_method("body", method!(Request::body, 0))?;
    request_class.define_method("fill_body", method!(Request::fill_body, 1))?;
    request_class.define_method("body_size", method!(Request::body_size, 0))?;
    request_class.define_method("inspect", method!(Request::inspect, 0))?;

    let grpc_request_class = module.define_class("GrpcRequest", ruby.class_object())?;
    grpc_request_class.define_method("service", method!(GrpcRequest::service, 0))?;
    grpc_request_class.define_method("method", method!(GrpcRequest::method, 0))?;
    grpc_request_class.define_method("header", method!(GrpcRequest::header, 1))?;
    grpc_request_class.define_method("headers", method!(GrpcRequest::headers, 0))?;
    grpc_request_class.define_method("body", method!(GrpcRequest::body, 0))?;
    grpc_request_class.define_method("fill_body", method!(GrpcRequest::fill_body, 1))?;
    grpc_request_class.define_method("body_size", method!(GrpcRequest::body_size, 0))?;
    grpc_request_class.define_method("compressed?", method!(GrpcRequest::is_compressed, 0))?;
    grpc_request_class.define_method("inspect", method!(GrpcRequest::inspect, 0))?;

    Ok(())
}