mod request;
mod response;
mod gvl_helpers;
mod grpc;

use request::{Request, GrpcRequest};
use response::{Response, GrpcResponse};
use gvl_helpers::nogvl;

use magnus::block::block_proc;
use magnus::typed_data::Obj;
use magnus::{function, method, prelude::*, Error as MagnusError, IntoValue, Ruby, Value, RString};
use bytes::Bytes;

use std::cell::RefCell;
use std::net::SocketAddr;

use tokio::net::UnixListener;

use std::sync::Arc;
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;
use crossbeam_channel;

use hyper::service::service_fn;
use hyper::{Error, Request as HyperRequest, Response as HyperResponse, StatusCode, Method, header::HeaderMap};
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use hyper_util::server::conn::auto;
use http_body_util::BodyExt;

use jemallocator::Jemalloc;

use log::{debug, info, warn};

use env_logger;
use crate::response::BodyWithTrailers;

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
    response_tx: oneshot::Sender<HyperResponse<BodyWithTrailers>>,
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
                // try getting the next request without yielding the GVL, if there's nothing, wait for one
                let work_request = match work_rx.try_recv() {
                    Ok(work_request) => Ok(work_request),
                    Err(crossbeam_channel::TryRecvError::Empty) => {
                        nogvl(|| work_rx.recv())
                    },
                    Err(crossbeam_channel::TryRecvError::Disconnected) => {
                        break;
                    }
                };

                match work_request {
                    Ok(work_request) => {
                        let hyper_request = work_request.request;
                        
                        println!("\nProcessing request:");
                        println!("  Method: {}", hyper_request.method());
                        println!("  Path: {}", hyper_request.uri().path());
                        println!("  Headers: {:?}", hyper_request.headers());
                        
                        // Convert to appropriate request type
                        let value = if grpc::is_grpc_request(&hyper_request) {
                            println!("Request identified as gRPC");
                            if let Some(grpc_request) = GrpcRequest::new(hyper_request) {
                                grpc_request.into_value()
                            } else {
                                println!("Failed to create GrpcRequest");
                                // Invalid gRPC request path
                                let response = GrpcResponse::error(3_u32.into_value(), RString::new("Invalid gRPC request path")).unwrap()
                                    .into_hyper_response();
                                work_request.response_tx.send(response).unwrap_or_else(|e| println!("Failed to send response: {:?}", e));
                                continue;
                            }
                        } else {
                            println!("Request identified as HTTP");
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
                                    println!("Block returned invalid response type");
                                    create_error_response("Internal server error")
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

async fn handle_request(
    req: HyperRequest<Incoming>,
    work_tx: Arc<crossbeam_channel::Sender<RequestWithCompletion>>,
) -> Result<HyperResponse<BodyWithTrailers>, Error> {
    debug!("Received request: {:?}", req);
    debug!("HTTP version: {:?}", req.version());
    debug!("Headers: {:?}", req.headers());

    let (parts, body) = req.into_parts();
    
    // Collect the body
    let body_bytes = match body.collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(e) => {
            debug!("Error collecting body: {:?}", e);
            return Err(e);
        }
    };
    
    debug!("Collected body size: {}", body_bytes.len());

    let hyper_request = HyperRequest::from_parts(parts, body_bytes);
    let is_grpc = grpc::is_grpc_request(&hyper_request);
    debug!("Is gRPC: {}", is_grpc);

    let (response_tx, response_rx) = oneshot::channel();

    let with_completion = RequestWithCompletion {
        request: hyper_request,
        response_tx,
    };

    if work_tx.send(with_completion).is_err() {
        warn!("Failed to send request to worker");
        return Ok(if is_grpc {
            grpc::create_grpc_error_response(500, 13, "Failed to process request")
        } else {
            create_error_response("Failed to process request")
        });
    }

    match response_rx.await {
        Ok(response) => {
            debug!("Got response: {:?}", response);
            Ok(response)
        }
        Err(_) => {
            warn!("Failed to receive response from worker");
            Ok(if is_grpc {
                grpc::create_grpc_error_response(500, 13, "Failed to get response")
            } else {
                create_error_response("Failed to get response")
            })
        }
    }
}

async fn handle_connection(
    stream: impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    work_tx: Arc<crossbeam_channel::Sender<RequestWithCompletion>>,
) {
    info!("New connection established");
    
    let service = service_fn(move |req: HyperRequest<Incoming>| {
        debug!("Service handling request");
        let work_tx = work_tx.clone();
        handle_request(req, work_tx)
    });

    let io = TokioIo::new(stream);
    
    debug!("Setting up HTTP/2 connection");
    let builder = auto::Builder::new(hyper_util::rt::TokioExecutor::new());
    
    if let Err(err) = builder
        .serve_connection(io, service)
        .await
    {
        warn!("Error serving connection: {:?}", err);
    }
}

// Helper function to create error responses
fn create_error_response(error_message: &str) -> HyperResponse<BodyWithTrailers> {
    // For non-gRPC requests, return a plain HTTP error
    let builder = HyperResponse::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .header("content-type", "text/plain");

    let trailers = HeaderMap::new();
    
    builder.body(BodyWithTrailers::new(Bytes::from(error_message.to_string()), trailers))
        .unwrap()
}

#[magnus::init]
fn init(ruby: &Ruby) -> Result<(), MagnusError> {
    // Initialize logging
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("hyper=debug,h2=debug"))
        .write_style(env_logger::WriteStyle::Always)
        .init();

    let module = ruby.define_module("HyperRuby")?;

    let server_class = module.define_class("Server", ruby.class_object())?;
    server_class.define_singleton_method("new", function!(Server::new, 0))?;
    server_class.define_method("configure", method!(Server::configure, 1))?;
    server_class.define_method("start", method!(Server::start, 0))?;
    server_class.define_method("stop", method!(Server::stop, 0))?;
    server_class.define_method("run_worker", method!(Server::run_worker, 0))?;

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
    grpc_request_class.define_method("inspect", method!(GrpcRequest::inspect, 0))?;

    Ok(())
}