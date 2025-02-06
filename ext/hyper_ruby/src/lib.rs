use hyper_util::rt::TokioIo;
use magnus::{function, method, prelude::*, Error as MagnusError, Ruby, Value};

use http_body_util::Full;
use hyper::body::Bytes;
//#[cfg(feature = "server")]
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use std::cell::RefCell;
use std::convert::Infallible;
use std::error::Error;
use std::net::SocketAddr;
use tokio::net::{TcpListener, UnixListener};

use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
// This would normally come from the `hyper-util` crate, but we can't depend
// on that here because it would be a cyclical dependency.
use tokio;


#[derive(Clone)]
// An Executor that uses the tokio runtime.
pub struct TokioExecutor;

// Implement the `hyper::rt::Executor` trait for `TokioExecutor` so that it can be used to spawn
// tasks in the hyper runtime.
// An Executor allows us to manage execution of tasks which can help us improve the efficiency and
// scalability of the server.
impl<F> hyper::rt::Executor<F> for TokioExecutor
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    fn execute(&self, fut: F) {
        tokio::task::spawn(fut);
    }
}

#[derive(Clone)]
struct ServerConfig {
    bind_address: String,
    worker_threads: usize,
}

impl ServerConfig {
    fn new() -> Self {
        Self {
            bind_address: String::from("127.0.0.1:3000"),
            worker_threads: 4,
        }
    }
}

enum Listener {
    Unix(UnixListener),
    Tcp(TcpListener),
}

// Add this trait
trait AsyncStream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin> AsyncStream for T {}

// Update the type alias
type BoxedStream = Box<dyn AsyncStream>;

#[magnus::wrap(class = "HyperRuby::Server")]
struct Server {
    server_handle: Arc<Mutex<Option<JoinHandle<()>>>>,
    config: RefCell<ServerConfig>,
}

impl Server {
    pub fn new() -> Self {
        Self {
            server_handle: Arc::new(Mutex::new(None)),
            config: RefCell::new(ServerConfig::new()),
        }
    }

    pub fn configure(&self, config: magnus::RHash) -> Result<(), MagnusError> {
        let mut server_config = self.config.borrow_mut();
        if let Some(bind_address) = config.get(magnus::Symbol::new("bind_address")) {
            server_config.bind_address = String::try_convert(bind_address)?;
        }
        if let Some(worker_threads) = config.get(magnus::Symbol::new("worker_threads")) {
            server_config.worker_threads = usize::try_convert(worker_threads)?;
        }
        Ok(())
    }

    async fn handler(_: Request<hyper::body::Incoming>) -> Result<Response<Full<Bytes>>, Infallible> {
        let mut response = Response::new(Full::new(Bytes::from("Hello, World!")));
        response.headers_mut().insert(
            hyper::header::SERVER,
            hyper::header::HeaderValue::from_static("hyper-ruby"),
        );
        Ok(response)
    }

    async fn accept_connection(listener: &Listener) -> Option<BoxedStream> {
        match listener {
            Listener::Unix(l) => match l.accept().await {
                Ok((stream, _)) => {
                    Some(Box::new(stream))
                },
                Err(e) => {
                    eprintln!("Failed to accept Unix connection: {}", e);
                    None
                }
            },
            Listener::Tcp(l) => match l.accept().await {
                Ok((stream, _)) => {
                    Some(Box::new(stream))
                },
                Err(e) => {
                    eprintln!("Failed to accept TCP connection: {}", e);
                    None
                }
            },
        }
    }

    pub fn start(&self) -> Result<(), MagnusError> {
        let server_handle = self.server_handle.clone();
            
        let config = self.config.borrow().clone();

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(config.worker_threads)
            .enable_all()
            .build()
            .map_err(|e| MagnusError::new(magnus::exception::runtime_error(), e.to_string()))?;

        println!("Starting server");
        
        // Enter the runtime context and block on the initialization
        rt.block_on(async {
            println!("Binding to: {}", config.bind_address);
            let addr = &config.bind_address;
            let listener = if addr.starts_with('/') {
                match UnixListener::bind(addr) {
                    Ok(l) => Listener::Unix(l),
                    Err(e) => {
                        eprintln!("Failed to bind Unix socket: {}", e);
                        return Result::<(), MagnusError>::Ok(());
                    }
                }
            } else {
                match addr.parse::<SocketAddr>() {
                    Ok(sock_addr) => match TcpListener::bind(sock_addr).await {
                        Ok(l) => Listener::Tcp(l),
                        Err(e) => {
                            eprintln!("Failed to bind TCP socket: {}", e);
                            return Ok(());
                        }
                    },
                    Err(e) => {
                        eprintln!("Invalid socket address: {}", e);
                        return Ok(());
                    }
                }
            };

            println!("Listening on: {}", addr);

            let server_task = tokio::spawn(async move {
                loop {
                    let stream = match Self::accept_connection(&listener).await {
                        Some(stream) => stream,
                        None => continue,
                    };

                    let io = TokioIo::new(stream);
                    let handler = Self::handler;

                    tokio::spawn(async move {
                        let service = service_fn(handler);
                        match http1::Builder::new()
                            .serve_connection(io, service)
                            .await
                        {
                            Ok(_) => {},
                            Err(err) => {
                                eprintln!("Error serving HTTP/1.1 connection: {:?}", err);
                                if let Some(source) = err.source() {
                                    eprintln!("Caused by: {:?}", source);
                                }
                            }
                        }
                    });
                }
            });

            let mut handle = server_handle.lock().await;
            *handle = Some(server_task);
            
            Ok(())
        })?;

        // Keep the runtime alive
        std::thread::spawn(move || {
            rt.block_on(async {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            });
        });

        Ok(())
    }

    pub fn stop(&self) -> Result<(), MagnusError> {
        let rt = tokio::runtime::Runtime::new()
            .map_err(|e| MagnusError::new(magnus::exception::runtime_error(), e.to_string()))?;

        rt.block_on(async {
            let mut handle = self.server_handle.lock().await;
            if let Some(task) = handle.take() {
                task.abort();
            }
        });

        Ok(())
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

    Ok(())
}