use lucchetto::{without_gvl, GvlSafe};
use magnus::{block, function, method, prelude::*, Error as MagnusError, RClass, RObject, Ruby, Value};
use bytes::Bytes;

use warp::Filter;
use std::cell::RefCell;
use std::net::SocketAddr;

use tokio::net::UnixListener;
use tokio_stream::wrappers::UnixListenerStream;

use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use parking_lot;

struct ThreadSafeValue(magnus::Value);
unsafe impl Send for ThreadSafeValue {}
unsafe impl Sync for ThreadSafeValue {}

#[derive(Clone)]
struct ServerConfig {
    bind_address: String,
    worker_threads: usize,
}

impl ServerConfig {
    fn new() -> Self {
        Self {
            bind_address: String::from("127.0.0.1:3000"),
            worker_threads: 4
        }
    }
}

#[magnus::wrap(class = "HyperRuby::Server")]
struct Server {
    server_handle: Arc<Mutex<Option<JoinHandle<()>>>>,
    config: RefCell<ServerConfig>,
    handler: Option<ThreadSafeValue>
}

impl GvlSafe for Server {}

impl Server {
    pub fn new() -> Self {
        Self {
            server_handle: Arc::new(Mutex::new(None)),
            config: RefCell::new(ServerConfig::new()),
            handler: None
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

    #[without_gvl]
    pub fn start(slf: Server) -> Result<(), MagnusError> {
        let config = slf.config.borrow().clone();
 

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(config.worker_threads)
            .enable_all()
            .build()
            .map_err(|e| MagnusError::new(magnus::exception::runtime_error(), e.to_string()))?;

        println!("Starting server");
        
        rt.block_on(async {
            let server_task = tokio::spawn(async move {
                let handler = handler.clone();
                let any_route = warp::any()
                    .and(warp::filters::method::method())
                    .and(warp::filters::path::full())
                    .and(warp::header::headers_cloned())
                    .and(warp::body::bytes())
                    .map(move |method: warp::http::Method, 
                            path: warp::path::FullPath,
                            headers: warp::http::HeaderMap,
                            body: bytes::Bytes| {
                        let handler = handler.read();
                        // // Create a hash to store request info
                        // let req_info = magnus::RHash::new();
                        // req_info.aset(magnus::Symbol::new("method"), method.as_str()).unwrap();
                        // req_info.aset(magnus::Symbol::new("path"), path.as_str()).unwrap();
                        
                        // // Convert headers to a hash
                        // let header_hash = magnus::RHash::new();
                        // for (key, value) in headers.iter() {
                        //     if let Ok(v) = value.to_str() {
                        //         header_hash.aset(key.as_str(), v).unwrap();
                        //     }
                        // }
                        // req_info.aset(magnus::Symbol::new("headers"), header_hash).unwrap();
                        
                        // // Convert body to string and add to hash
                        // req_info.aset(magnus::Symbol::new("body"), String::from_utf8_lossy(&body).into_owned()).unwrap();
                        
                        let result = match handler.0.funcall("call", ("req_info",)) {
                            Ok(response) => {
                                if let Ok(response_str) = String::try_convert(response) {
                                    response_str
                                } else {
                                    "Error: Handler response could not be converted to string".to_string()
                                }
                            }
                            Err(_) => "Error: Handler call failed".to_string()
                        };

                        warp::reply::with_status(
                            result,
                            warp::http::StatusCode::OK
                        )
                    });

                if config.bind_address.starts_with("unix:") {
                    let path = config.bind_address.trim_start_matches("unix:");
                
                    let listener = UnixListener::bind(path).unwrap();
                    let incoming = UnixListenerStream::new(listener);
                    warp::serve(any_route)
                        .run_incoming(incoming)
                        .await
                } else {
                    let addr: SocketAddr = config.bind_address.parse()
                        .expect("invalid address format");
                    warp::serve(any_route)
                        .run(addr)
                        .await;
                }
            });

            let mut handle = self.server_handle.lock().await;
            *handle = Some(server_task);
            
            Ok::<(), MagnusError>(())
        })?;

        // Keep the runtime alive
        std::thread::spawn(move || {
            magnus::embed::init_from_rust();  // Still need this for the background thread
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
    server_class.define_method("start", method!(Server::start, 1))?;
    server_class.define_method("stop", method!(Server::stop, 0))?;

    Ok(())
}