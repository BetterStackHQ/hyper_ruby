// Syslog transport listeners: a stream listener (Unix socket or TCP) that frames
// messages with RFC 6587 octet counting and a newline fallback, and a UDP
// listener where one datagram is one message. Complete messages are handed to
// the same Ruby worker queue the HTTP path uses.

mod framing;
mod proxy;

use std::io;
use std::net::SocketAddr;
use std::os::unix::fs::MetadataExt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::{Bytes, BytesMut};
use crossbeam_channel::TrySendError;
use log::{debug, info, warn};
use magnus::{Error as MagnusError, RHash, Symbol, TryConvert};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::net::{TcpListener, UdpSocket, UnixListener};
use tokio::sync::{broadcast, mpsc, oneshot, OwnedSemaphorePermit, Semaphore};
use tokio::time::{sleep, timeout};
use tokio_util::task::TaskTracker;

use framing::{Framer, RejectReason};
use proxy::{ProxyError, ProxyHeader};

/// Largest PROXY v2 header (including TLVs) we will read.
const MAX_PROXY_HEADER_BYTES: usize = 1024;
/// How long to wait for space on the Ruby worker queue before trying again.
const WORKER_QUEUE_RETRY: Duration = Duration::from_millis(10);
/// Backoff bounds used when a handler refuses a message.
const MIN_REFUSAL_DELAY: Duration = Duration::from_millis(5);
const MAX_REFUSAL_DELAY: Duration = Duration::from_millis(500);
/// How long to pause after a failed accept.
const ACCEPT_ERROR_DELAY: Duration = Duration::from_millis(10);
/// Read buffer growth increment for stream connections.
const STREAM_READ_CHUNK: usize = 8 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Transport {
    Tcp,
    Udp,
}

impl Transport {
    pub(crate) fn symbol(self) -> Symbol {
        match self {
            Transport::Tcp => Symbol::new("tcp"),
            Transport::Udp => Symbol::new("udp"),
        }
    }
}

#[derive(Clone)]
pub(crate) struct SyslogConfig {
    pub stream_path: Option<String>,
    pub stream_bind: Option<String>,
    pub udp_bind: Option<String>,
    pub proxy_protocol: bool,
    pub proxy_header_timeout: u64,
    pub max_frame_bytes: usize,
    pub max_pending: usize,
    pub udp_recv_buffer_bytes: usize,
    pub udp_socket_recv_buffer_bytes: Option<usize>,
}

impl SyslogConfig {
    pub(crate) fn new() -> Self {
        Self {
            stream_path: None,
            stream_bind: None,
            udp_bind: None,
            proxy_protocol: false,
            proxy_header_timeout: 5000,
            max_frame_bytes: 102400,
            max_pending: 1000,
            udp_recv_buffer_bytes: 65536,
            udp_socket_recv_buffer_bytes: None,
        }
    }

    pub(crate) fn enabled(&self) -> bool {
        self.stream_path.is_some() || self.stream_bind.is_some() || self.udp_bind.is_some()
    }

    /// Read the syslog keys out of the server configuration hash.
    pub(crate) fn apply(&mut self, config: &RHash) -> Result<(), MagnusError> {
        if let Some(value) = config.get(Symbol::new("syslog_stream_path")) {
            self.stream_path = Some(String::try_convert(value)?);
        }
        if let Some(value) = config.get(Symbol::new("syslog_stream_bind")) {
            self.stream_bind = Some(String::try_convert(value)?);
        }
        if let Some(value) = config.get(Symbol::new("syslog_udp_bind")) {
            self.udp_bind = Some(String::try_convert(value)?);
        }
        if let Some(value) = config.get(Symbol::new("syslog_proxy_protocol")) {
            self.proxy_protocol = bool::try_convert(value)?;
        }
        if let Some(value) = config.get(Symbol::new("syslog_proxy_header_timeout")) {
            self.proxy_header_timeout = u64::try_convert(value)?;
        }
        if let Some(value) = config.get(Symbol::new("syslog_max_frame_bytes")) {
            self.max_frame_bytes = usize::try_convert(value)?;
        }
        if let Some(value) = config.get(Symbol::new("syslog_max_pending")) {
            self.max_pending = usize::try_convert(value)?.max(1);
        }
        if let Some(value) = config.get(Symbol::new("syslog_udp_recv_buffer_bytes")) {
            self.udp_recv_buffer_bytes = usize::try_convert(value)?.max(1);
        }
        if let Some(value) = config.get(Symbol::new("syslog_udp_socket_recv_buffer_bytes")) {
            self.udp_socket_recv_buffer_bytes = Some(usize::try_convert(value)?);
        }
        Ok(())
    }
}

#[derive(Default)]
pub(crate) struct SyslogCounters {
    messages_delivered: AtomicU64,
    deliveries_refused: AtomicU64,
    rejected_oversize: AtomicU64,
    rejected_invalid_utf8: AtomicU64,
    rejected_invalid_length: AtomicU64,
    udp_truncated: AtomicU64,
    udp_dropped: AtomicU64,
    connections_opened: AtomicU64,
    connections_closed: AtomicU64,
    proxy_header_errors: AtomicU64,
}

impl SyslogCounters {
    fn record_rejection(&self, reason: RejectReason) {
        let counter = match reason {
            RejectReason::Oversize => &self.rejected_oversize,
            RejectReason::InvalidUtf8 => &self.rejected_invalid_utf8,
            RejectReason::InvalidLength => &self.rejected_invalid_length,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn to_hash(&self) -> Result<RHash, MagnusError> {
        let rejected = RHash::new();
        rejected.aset(
            Symbol::new(RejectReason::Oversize.as_str()),
            self.rejected_oversize.load(Ordering::Relaxed),
        )?;
        rejected.aset(
            Symbol::new(RejectReason::InvalidUtf8.as_str()),
            self.rejected_invalid_utf8.load(Ordering::Relaxed),
        )?;
        rejected.aset(
            Symbol::new(RejectReason::InvalidLength.as_str()),
            self.rejected_invalid_length.load(Ordering::Relaxed),
        )?;

        let stats = RHash::new();
        stats.aset(
            Symbol::new("messages_delivered"),
            self.messages_delivered.load(Ordering::Relaxed),
        )?;
        stats.aset(
            Symbol::new("deliveries_refused"),
            self.deliveries_refused.load(Ordering::Relaxed),
        )?;
        stats.aset(Symbol::new("frames_rejected"), rejected)?;
        stats.aset(
            Symbol::new("udp_truncated"),
            self.udp_truncated.load(Ordering::Relaxed),
        )?;
        stats.aset(
            Symbol::new("udp_dropped"),
            self.udp_dropped.load(Ordering::Relaxed),
        )?;
        stats.aset(
            Symbol::new("connections_opened"),
            self.connections_opened.load(Ordering::Relaxed),
        )?;
        stats.aset(
            Symbol::new("connections_closed"),
            self.connections_closed.load(Ordering::Relaxed),
        )?;
        stats.aset(
            Symbol::new("proxy_header_errors"),
            self.proxy_header_errors.load(Ordering::Relaxed),
        )?;
        Ok(stats)
    }
}

/// A complete message on its way to a Ruby worker thread.
pub(crate) struct SyslogDelivery {
    pub message: Bytes,
    /// Stream frames are validated UTF-8; datagrams are handed over as binary.
    pub utf8: bool,
    pub peer: Arc<str>,
    pub transport: Transport,
    pub received_at_ns: u64,
    pub result_tx: oneshot::Sender<bool>,
}

enum DeliveryOutcome {
    Accepted,
    Refused,
    /// No worker can answer any more; the listener should give up.
    Unavailable,
}

/// Everything a listener needs to hand a message to the Ruby workers.
#[derive(Clone)]
struct Dispatcher {
    work_tx: Arc<crossbeam_channel::Sender<SyslogDelivery>>,
    counters: Arc<SyslogCounters>,
    credits: Arc<Semaphore>,
    tracker: TaskTracker,
}

impl Dispatcher {
    async fn deliver(
        &self,
        message: Bytes,
        utf8: bool,
        peer: Arc<str>,
        transport: Transport,
        received_at_ns: u64,
    ) -> DeliveryOutcome {
        let (result_tx, result_rx) = oneshot::channel();
        let mut delivery = SyslogDelivery {
            message,
            utf8,
            peer,
            transport,
            received_at_ns,
            result_tx,
        };

        loop {
            match self.work_tx.try_send(delivery) {
                Ok(()) => break,
                Err(TrySendError::Full(returned)) => {
                    delivery = returned;
                    sleep(WORKER_QUEUE_RETRY).await;
                }
                Err(TrySendError::Disconnected(_)) => return DeliveryOutcome::Unavailable,
            }
        }

        match result_rx.await {
            Ok(true) => {
                self.counters
                    .messages_delivered
                    .fetch_add(1, Ordering::Relaxed);
                DeliveryOutcome::Accepted
            }
            Ok(false) => {
                self.counters
                    .deliveries_refused
                    .fetch_add(1, Ordering::Relaxed);
                DeliveryOutcome::Refused
            }
            Err(_) => DeliveryOutcome::Unavailable,
        }
    }
}

/// Handle on the running listeners, used to drain them at shutdown.
pub(crate) struct SyslogRuntime {
    tracker: TaskTracker,
    listening: Arc<AtomicBool>,
    socket_path: Option<String>,
    // (dev, ino) of the Unix socket file we bound, so we only unlink our own.
    socket_ident: Option<(u64, u64)>,
}

impl SyslogRuntime {
    pub(crate) fn listening(&self) -> bool {
        self.listening.load(Ordering::Relaxed)
    }

    /// Wait for the listeners to finish delivering already framed messages.
    pub(crate) async fn drain(&self, limit: Duration) {
        self.listening.store(false, Ordering::Relaxed);
        if timeout(limit, self.tracker.wait()).await.is_err() {
            warn!("Timed out waiting for syslog listeners to drain");
        }
    }

    pub(crate) fn remove_socket_file(&self) {
        remove_socket_file(self.socket_path.as_deref(), self.socket_ident);
    }
}

/// Unlink a bound Unix socket file, unless a replacement server has taken the
/// path over in the meantime.
fn remove_socket_file(path: Option<&str>, ident: Option<(u64, u64)>) {
    let (Some(path), Some((dev, ino))) = (path, ident) else {
        return;
    };
    match std::fs::symlink_metadata(path) {
        Ok(meta) if (meta.dev(), meta.ino()) == (dev, ino) => {
            std::fs::remove_file(path)
                .unwrap_or_else(|e| warn!("Failed to remove syslog socket file: {:?}", e));
        }
        Ok(_) => info!(
            "Syslog socket file {} was replaced by another server; leaving it in place",
            path
        ),
        Err(_) => debug!("Syslog socket file {} already removed", path),
    }
}

/// Bind the configured listeners and start serving. Must be called from within
/// the Tokio runtime.
pub(crate) fn start(
    config: &SyslogConfig,
    counters: Arc<SyslogCounters>,
    work_tx: Arc<crossbeam_channel::Sender<SyslogDelivery>>,
    shutdown: &broadcast::Sender<()>,
) -> io::Result<SyslogRuntime> {
    let mut socket = (None, None);
    let started = start_listeners(config, counters, work_tx, shutdown, &mut socket);
    if started.is_err() {
        // Leave no socket file behind for listeners that never came up.
        remove_socket_file(socket.0.as_deref(), socket.1);
    }
    started
}

fn start_listeners(
    config: &SyslogConfig,
    counters: Arc<SyslogCounters>,
    work_tx: Arc<crossbeam_channel::Sender<SyslogDelivery>>,
    shutdown: &broadcast::Sender<()>,
    socket: &mut (Option<String>, Option<(u64, u64)>),
) -> io::Result<SyslogRuntime> {
    let tracker = TaskTracker::new();
    let dispatcher = Dispatcher {
        work_tx,
        counters,
        credits: Arc::new(Semaphore::new(config.max_pending)),
        tracker: tracker.clone(),
    };
    if let Some(path) = &config.stream_path {
        let (listener, ident) = bind_unix_listener(path)?;
        *socket = (Some(path.clone()), ident);
        spawn_stream_listener(
            StreamListener::Unix(listener),
            config.clone(),
            dispatcher.clone(),
            shutdown,
            &tracker,
        );
    }

    if let Some(address) = &config.stream_bind {
        let address: SocketAddr = address
            .parse()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("{}", e)))?;
        let listener = TcpListener::from_std(bind_std_tcp(address)?)?;
        spawn_stream_listener(
            StreamListener::Tcp(listener),
            config.clone(),
            dispatcher.clone(),
            shutdown,
            &tracker,
        );
    }

    if let Some(address) = &config.udp_bind {
        let address: SocketAddr = address
            .parse()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("{}", e)))?;
        let socket = UdpSocket::from_std(bind_std_udp(address, config)?)?;
        let config = config.clone();
        let dispatcher = dispatcher.clone();
        let shutdown_rx = shutdown.subscribe();
        tracker.spawn(serve_udp(socket, config, dispatcher, shutdown_rx));
    }

    tracker.close();

    Ok(SyslogRuntime {
        tracker,
        listening: Arc::new(AtomicBool::new(true)),
        socket_path: socket.0.clone(),
        socket_ident: socket.1,
    })
}

fn bind_unix_listener(path: &str) -> io::Result<(UnixListener, Option<(u64, u64)>)> {
    // Bind a unique temporary path and rename over the target, so the path
    // always points at a live socket even when a replacement server takes over
    // a path an older, still draining server bound.
    static SOCKET_TMP_SEQ: AtomicU64 = AtomicU64::new(0);
    let tmp_path = format!(
        "{}.{}.{}.tmp",
        path,
        std::process::id(),
        SOCKET_TMP_SEQ.fetch_add(1, Ordering::Relaxed)
    );

    let listener = UnixListener::bind(&tmp_path)?;
    let ident = std::fs::symlink_metadata(&tmp_path)
        .ok()
        .map(|meta| (meta.dev(), meta.ino()));

    if let Err(e) = std::fs::rename(&tmp_path, path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e);
    }

    Ok((listener, ident))
}

fn bind_std_tcp(address: SocketAddr) -> io::Result<std::net::TcpListener> {
    let socket = Socket::new(Domain::for_address(address), Type::STREAM, Some(Protocol::TCP))?;
    socket.set_reuse_address(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&address.into())?;
    socket.listen(1024)?;
    Ok(socket.into())
}

fn bind_std_udp(address: SocketAddr, config: &SyslogConfig) -> io::Result<std::net::UdpSocket> {
    let socket = Socket::new(Domain::for_address(address), Type::DGRAM, Some(Protocol::UDP))?;
    // Several processes can bind the same port; the kernel hands each datagram
    // to one of them.
    socket.set_reuse_address(true)?;
    socket.set_reuse_port(true)?;
    if let Some(size) = config.udp_socket_recv_buffer_bytes {
        socket.set_recv_buffer_size(size)?;
    }
    socket.set_nonblocking(true)?;
    socket.bind(&address.into())?;
    Ok(socket.into())
}

enum StreamListener {
    Unix(UnixListener),
    Tcp(TcpListener),
}

impl StreamListener {
    async fn accept(&self) -> io::Result<(Box<dyn AsyncReadStream>, Arc<str>)> {
        match self {
            StreamListener::Unix(listener) => {
                let (stream, _) = listener.accept().await?;
                Ok((Box::new(stream), Arc::from("")))
            }
            StreamListener::Tcp(listener) => {
                let (stream, address) = listener.accept().await?;
                Ok((Box::new(stream), Arc::from(address.ip().to_string())))
            }
        }
    }
}

trait AsyncReadStream: AsyncRead + Unpin + Send {}
impl<T: AsyncRead + Unpin + Send> AsyncReadStream for T {}

fn spawn_stream_listener(
    listener: StreamListener,
    config: SyslogConfig,
    dispatcher: Dispatcher,
    shutdown: &broadcast::Sender<()>,
    tracker: &TaskTracker,
) {
    let mut shutdown_rx = shutdown.subscribe();
    let shutdown = shutdown.clone();
    let connections = tracker.clone();

    tracker.spawn(async move {
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (stream, peer) = match accepted {
                        Ok(accepted) => accepted,
                        Err(e) => {
                            // Transient accept failures (descriptor limits) must
                            // not turn into a hot loop.
                            warn!("Failed to accept syslog connection: {:?}", e);
                            sleep(ACCEPT_ERROR_DELAY).await;
                            continue;
                        }
                    };

                    dispatcher.counters.connections_opened.fetch_add(1, Ordering::Relaxed);
                    connections.spawn(serve_stream(
                        stream,
                        peer,
                        config.clone(),
                        dispatcher.clone(),
                        shutdown.subscribe(),
                    ));
                },
                _ = shutdown_rx.recv() => {
                    debug!("Syslog stream listener shutting down");
                    break;
                }
            }
        }
    });
}

/// A framed message waiting for its turn with a Ruby worker. The permit is held
/// until delivery finishes, which is what bounds undelivered messages overall.
struct PendingMessage {
    message: Bytes,
    received_at_ns: u64,
    _permit: OwnedSemaphorePermit,
}

async fn serve_stream(
    mut stream: Box<dyn AsyncReadStream>,
    peer: Arc<str>,
    config: SyslogConfig,
    dispatcher: Dispatcher,
    mut shutdown_rx: broadcast::Receiver<()>,
) {
    let mut buffer = BytesMut::with_capacity(STREAM_READ_CHUNK);

    let peer = if config.proxy_protocol {
        match read_proxy_header(&mut stream, &mut buffer, &config).await {
            Ok(Some(source)) => Arc::from(source.to_string()),
            Ok(None) => peer,
            Err(reason) => {
                dispatcher
                    .counters
                    .proxy_header_errors
                    .fetch_add(1, Ordering::Relaxed);
                debug!("Rejecting syslog connection: PROXY header {}", reason);
                dispatcher
                    .counters
                    .connections_closed
                    .fetch_add(1, Ordering::Relaxed);
                return;
            }
        }
    } else {
        peer
    };

    // Reading and delivery are separate so that a slow handler stops the reads
    // through the bounded channel rather than blocking the reactor.
    let (pending_tx, pending_rx) = mpsc::channel::<PendingMessage>(config.max_pending);
    let delivery = tokio::spawn(deliver_stream_messages(
        pending_rx,
        peer,
        dispatcher.clone(),
    ));

    let mut framer = Framer::new(config.max_frame_bytes);
    let mut eof = false;

    'read: loop {
        // Emit everything the buffer already holds before asking for more.
        loop {
            let decoded = if eof {
                framer.decode_eof(&mut buffer)
            } else {
                framer.decode(&mut buffer)
            };

            match decoded {
                Ok(Some(message)) => {
                    let received_at_ns = now_nanos();
                    let Ok(permit) = dispatcher.credits.clone().acquire_owned().await else {
                        break 'read;
                    };
                    let pending = PendingMessage {
                        message,
                        received_at_ns,
                        _permit: permit,
                    };
                    if pending_tx.send(pending).await.is_err() {
                        break 'read;
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    dispatcher.counters.record_rejection(error.reason);
                    if error.fatal {
                        debug!("Closing syslog connection: {} frame", error.reason.as_str());
                        break 'read;
                    }
                }
            }
        }

        if eof {
            break;
        }

        tokio::select! {
            read = stream.read_buf(&mut buffer) => {
                match read {
                    Ok(0) => eof = true,
                    Ok(_) => (),
                    Err(e) => {
                        debug!("Syslog connection read failed: {:?}", e);
                        break;
                    }
                }
            },
            _ = shutdown_rx.recv() => {
                debug!("Syslog connection stopping reads for shutdown");
                break;
            }
        }
    }

    // Dropping the sender lets the delivery task finish what it already has.
    drop(pending_tx);
    let _ = delivery.await;
    dispatcher
        .counters
        .connections_closed
        .fetch_add(1, Ordering::Relaxed);
}

async fn deliver_stream_messages(
    mut pending_rx: mpsc::Receiver<PendingMessage>,
    peer: Arc<str>,
    dispatcher: Dispatcher,
) {
    while let Some(pending) = pending_rx.recv().await {
        let mut delay = MIN_REFUSAL_DELAY;
        loop {
            match dispatcher
                .deliver(
                    pending.message.clone(),
                    true,
                    peer.clone(),
                    Transport::Tcp,
                    pending.received_at_ns,
                )
                .await
            {
                DeliveryOutcome::Accepted => break,
                DeliveryOutcome::Refused => {
                    // Hold the message and try again; reads stall behind the
                    // bounded pending channel while we wait.
                    sleep(delay).await;
                    delay = (delay * 2).min(MAX_REFUSAL_DELAY);
                }
                DeliveryOutcome::Unavailable => return,
            }
        }
    }
}

/// Read and consume a PROXY v2 header, returning the source address it carries.
async fn read_proxy_header(
    stream: &mut Box<dyn AsyncReadStream>,
    buffer: &mut BytesMut,
    config: &SyslogConfig,
) -> Result<Option<std::net::IpAddr>, &'static str> {
    let deadline = Duration::from_millis(config.proxy_header_timeout);
    timeout(deadline, async {
        loop {
            match proxy::parse(buffer, MAX_PROXY_HEADER_BYTES) {
                Ok(ProxyHeader::Complete { source, length }) => {
                    let _ = buffer.split_to(length);
                    return Ok(source);
                }
                Ok(ProxyHeader::Incomplete) => match stream.read_buf(buffer).await {
                    Ok(0) => return Err(ProxyError::Signature.as_str()),
                    Ok(_) => continue,
                    Err(_) => return Err(ProxyError::Signature.as_str()),
                },
                Err(error) => return Err(error.as_str()),
            }
        }
    })
    .await
    .unwrap_or(Err("timeout"))
}

async fn serve_udp(
    socket: UdpSocket,
    config: SyslogConfig,
    dispatcher: Dispatcher,
    mut shutdown_rx: broadcast::Receiver<()>,
) {
    let mut buffer = vec![0u8; config.udp_recv_buffer_bytes];

    loop {
        tokio::select! {
            received = socket.recv_from(&mut buffer) => {
                let (length, peer) = match received {
                    Ok(received) => received,
                    Err(e) => {
                        warn!("Syslog datagram receive failed: {:?}", e);
                        continue;
                    }
                };
                let received_at_ns = now_nanos();

                // A datagram that fills the buffer was almost certainly cut
                // short by it, and a truncated prefix must not pass as a message.
                if length == buffer.len() {
                    dispatcher.counters.udp_truncated.fetch_add(1, Ordering::Relaxed);
                    continue;
                }

                let Ok(permit) = dispatcher.credits.clone().try_acquire_owned() else {
                    dispatcher.counters.udp_dropped.fetch_add(1, Ordering::Relaxed);
                    continue;
                };

                let message = Bytes::copy_from_slice(&buffer[..length]);
                let peer: Arc<str> = Arc::from(peer.ip().to_string());
                let dispatcher = dispatcher.clone();
                dispatcher.tracker.clone().spawn(async move {
                    let _permit = permit;
                    // A datagram sender cannot be asked to slow down, so a
                    // refusal drops the message.
                    match dispatcher
                        .deliver(message, false, peer, Transport::Udp, received_at_ns)
                        .await
                    {
                        DeliveryOutcome::Accepted => (),
                        DeliveryOutcome::Refused | DeliveryOutcome::Unavailable => {
                            dispatcher.counters.udp_dropped.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                });
            },
            _ = shutdown_rx.recv() => {
                debug!("Syslog datagram listener shutting down");
                break;
            }
        }
    }
}

fn now_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or(0)
}
