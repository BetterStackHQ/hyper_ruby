mod request;
mod response;
mod gvl_helpers;

use hyper::header::HeaderName;
use request::Request;
use response::Response;
use gvl_helpers::nogvl;

use magnus::block::block_proc;
use magnus::r_hash::ForEach;
use magnus::typed_data::Obj;
use magnus::{function, method, prelude::*, Error as MagnusError, IntoValue, Ruby, Value};
use bytes::Bytes;

use std::cell::RefCell;
use std::net::SocketAddr;

use tokio::net::UnixListener;

use std::sync::Arc;
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;
use crossbeam_channel;

use hyper::service::service_fn;
use hyper::{Error, Request as HyperRequest, Response as HyperResponse, StatusCode};
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use hyper_util::server::conn::auto;
use http_body_util::BodyExt; // You'll need this
use http_body_util::Full;

use jemallocator::Jemalloc;

#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

#[derive(Clone)]
struct ServerConfig {
    bind_address: String,
    tokio_threads: Option<usize>,
}

impl ServerConfig {
    fn new() -> Self {
        Self {
            bind_address: String::from("127.0.0.1:3000"),
            tokio_threads: None,
        }
    }
}

// Sent on the work channel with the request, and a oneshot channel to send the response back on.
struct RequestWithCompletion {
    request: HyperRequest<Bytes>,
    // sent a response back on this thread
    response_tx: oneshot::Sender<HyperResponse<Full<Bytes>>>,
}

#[magnus::wrap(class = "HyperRuby::Server")]
struct Server {
    server_handle: Arc<Mutex<Option<JoinHandle<()>>>>,
    config: RefCell<ServerConfig>,
    work_rx: RefCell<Option<crossbeam_channel::Receiver<RequestWithCompletion>>>,
    work_tx: RefCell<Option<Arc<crossbeam_channel::Sender<RequestWithCompletion>>>>,
    runtime: RefCell<Option<Arc<tokio::runtime::Runtime>>>,
}

impl Server {
    pub fn new() -> Self {
        let (work_tx, work_rx) = crossbeam_channel::bounded(1000);
        
        Self {
            server_handle: Arc::new(Mutex::new(None)),
            config: RefCell::new(ServerConfig::new()),
            work_rx: RefCell::new(Some(work_rx)),
            work_tx: RefCell::new(Some(Arc::new(work_tx))),
            runtime: RefCell::new(None),
        }
    }

    pub fn configure(&self, config: magnus::RHash) -> Result<(), MagnusError> {
        let mut server_config = self.config.borrow_mut();
        if let Some(bind_address) = config.get(magnus::Symbol::new("bind_address")) {
            server_config.bind_address = String::try_convert(bind_address)?;
        }

        if let Some(tokio_threads) = config.get(magnus::Symbol::new("tokio_threads")) {
            server_config.tokio_threads = Some(usize::try_convert(tokio_threads)?);
        }

        Ok(())
    }

    // Method that Ruby worker threads will call with a block
    pub fn run_worker(&self) -> Result<(), MagnusError> {
        let block = block_proc().unwrap();
        if let Some(work_rx) = self.work_rx.borrow().as_ref() {
           
            loop {
                 // Use nogvl to wait for requests outside the GVL
                 let work_request = nogvl(|| work_rx.recv());
                
                 match work_request {
                    Ok(work_request) => {
                        let request = Request {
                            request: work_request.request,
                        };
                        let value = request.into_value();
                        let hyper_response = match block.call::<_, Value>([value]) {
                            Ok(result) => {
                                let ref_response = Obj::<Response>::try_convert(result).unwrap();

                                let mut builder = HyperResponse::builder()
                                    .status(ref_response.status);

                                // safe because the block result will only ever be called on the ruby thread
                                let ruby = unsafe { Ruby::get_unchecked() };

                                let ruby_response_headers = ruby.get_inner(ref_response.headers);
                                let builder_headers = builder.headers_mut().unwrap();
                                ruby_response_headers.foreach(|key: String, value: String| {
                                    let header_name = HeaderName::try_from(key).unwrap();
                                    builder_headers.insert(header_name, value.try_into().unwrap());
                                    Ok(ForEach::Continue)
                                }).unwrap();

                                let response_body = ruby.get_inner(ref_response.body);
                                let response: Result<HyperResponse<Full<Bytes>>, hyper::http::Error>;

                                if response_body.len() > 0 {
                                    // safe because RString will not be cleared here before we copy the bytes into our own Vector.
                                    unsafe {
                                        // copy directly to bytes here so we don't have to worry about encoding checks
                                        let rust_body = Bytes::copy_from_slice(response_body.as_slice());
                                        response = builder.body(Full::new(rust_body));
                                    }
                                } else {
                                    response = builder.body(Full::new(Bytes::new()));
                                }

                                match response {
                                    Ok(response) => response,
                                    Err(e) => {
                                        println!("HTTP request build failed {:?}", e);
                                        create_error_response("Internal server error")
                                    }
                                }
                            },
                            Err(e) => {
                                println!("Block call failed: {:?}", e);
                                create_error_response("Internal server error")
                            }
                        };

                        match work_request.response_tx.send(hyper_response) {
                            Ok(_) => (),
                            Err(e) => println!("Failed to send response: {:?}", e),
                        }
                    }
                    Err(_) => {
                        // Channel closed, exit thread
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    pub fn start(&self) -> Result<(), MagnusError> {
        let config = self.config.borrow().clone();
        let work_tx = self.work_tx.borrow()
            .as_ref()
            .ok_or_else(|| MagnusError::new(magnus::exception::runtime_error(), "Work channel not initialized"))?
            .clone();

        let mut rt_builder = tokio::runtime::Builder::new_multi_thread();
            
        rt_builder.enable_all();

        if let Some(tokio_threads) = config.tokio_threads {
            rt_builder.worker_threads(tokio_threads);
        }

        let rt = Arc::new(rt_builder
            .build()
            .map_err(|e| MagnusError::new(magnus::exception::runtime_error(), e.to_string()))?);

        *self.runtime.borrow_mut() = Some(rt.clone());

        rt.block_on(async {
            let work_tx = work_tx.clone();
            
            let server_task = tokio::spawn(async move {
                if config.bind_address.starts_with("unix:") {
                    let path = config.bind_address.trim_start_matches("unix:");
                    let listener = UnixListener::bind(path).unwrap();

                    loop {
                        let (stream, _) = listener.accept().await.unwrap();
                        let work_tx = work_tx.clone();

                        tokio::task::spawn(async move {
                            handle_connection(stream, work_tx).await;
                        });
                    }
                } else {
                    let addr: SocketAddr = config.bind_address.parse()
                        .expect("invalid address format");
                    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();

                    loop {
                        let (stream, _) = listener.accept().await.unwrap();
                        let work_tx = work_tx.clone();

                        tokio::task::spawn(async move {
                            handle_connection(stream, work_tx).await;
                        });
                    }
                }
            });

            let mut handle = self.server_handle.lock().await;
            *handle = Some(server_task);
            
            Ok::<(), MagnusError>(())
        })?;

        Ok(())
    }

    pub fn stop(&self) -> Result<(), MagnusError> {
        // Use the stored runtime instead of creating a new one
        if let Some(rt) = self.runtime.borrow().as_ref() {
            rt.block_on(async {
                let mut handle = self.server_handle.lock().await;
                if let Some(task) = handle.take() {
                    task.abort();
                }
            });
        }

        // Drop the channel and runtime
        self.work_tx.borrow_mut().take();
        self.runtime.borrow_mut().take();

        let bind_address = self.config.borrow().bind_address.clone();
        if bind_address.starts_with("unix:") {
            let path = bind_address.trim_start_matches("unix:");
            std::fs::remove_file(path).unwrap_or_else(|e| {
                println!("Failed to remove socket file: {:?}", e);
            });
        }

        Ok(())
    }
}


// Helper function to create error responses
fn create_error_response(error_message: &str) -> HyperResponse<Full<Bytes>> {
    HyperResponse::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(format!(r#"{{"error": "{}"}}"#, error_message))))
        .unwrap()
}

async fn handle_request(
    req: HyperRequest<Incoming>,
    work_tx: Arc<crossbeam_channel::Sender<RequestWithCompletion>>,
) -> Result<HyperResponse<Full<Bytes>>, Error> {

    let (parts, body) = req.into_parts();
    let body_bytes = body.collect().await?.to_bytes();

    let (response_tx, response_rx) = oneshot::channel();

    let with_completion = RequestWithCompletion {
        request: HyperRequest::from_parts(parts, body_bytes),
        response_tx,
    };

    if work_tx.send(with_completion).is_err() {
        return Ok(create_error_response("Failed to process request"));
    }

    match response_rx.await {
        Ok(response) => { Ok(response) }        
        Err(_) => Ok(create_error_response("Failed to get response")),
    }
}

async fn handle_connection(
    stream: impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    work_tx: Arc<crossbeam_channel::Sender<RequestWithCompletion>>,
) {
    let service = service_fn(move |req: HyperRequest<Incoming>| {
        let work_tx = work_tx.clone();
        handle_request(req, work_tx)
    });

    let io = TokioIo::new(stream);
    
    if let Err(err) = auto::Builder::new(hyper_util::rt::TokioExecutor::new())
        .serve_connection(io, service)
        .await
    {
        eprintln!("Error serving connection: {:?}", err);
    }
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

    let response_class = module.define_class("Response", ruby.class_object())?;
    response_class.define_singleton_method("new", function!(Response::new, 3))?;

    let request_class = module.define_class("Request", ruby.class_object())?;
    request_class.define_method("http_method", method!(Request::method, 0))?;
    request_class.define_method("path", method!(Request::path, 0))?;
    request_class.define_method("header", method!(Request::header, 1))?;
    request_class.define_method("body", method!(Request::body, 0))?;
    request_class.define_method("fill_body", method!(Request::fill_body, 1))?;
    request_class.define_method("body_size", method!(Request::body_size, 0))?;
    request_class.define_method("inspect", method!(Request::inspect, 0))?;

    Ok(())
}