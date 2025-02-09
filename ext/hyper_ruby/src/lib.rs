use magnus::block::block_proc;
use magnus::r_hash::ForEach;
use magnus::typed_data::Obj;
use magnus::value::Opaque;
use magnus::{function, gc, method, prelude::*, DataTypeFunctions, Error as MagnusError, RString, Ruby, TypedData, Value};
use bytes::Bytes;

use std::{ffi::c_void, mem::MaybeUninit, ptr::null_mut};

use rb_sys::rb_thread_call_without_gvl;

use warp::Filter;
use warp::http::Response as WarpResponse;
use std::cell::RefCell;
use std::net::SocketAddr;

use tokio::net::UnixListener;
use tokio_stream::wrappers::UnixListenerStream;

use std::sync::Arc;
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;
use crossbeam_channel;

#[derive(Clone)]
struct ServerConfig {
    bind_address: String,
}

impl ServerConfig {
    fn new() -> Self {
        Self {
            bind_address: String::from("127.0.0.1:3000"),
        }
    }
}

// Request type that will be sent to worker threads
#[derive(Debug)]
struct WorkRequest {
    method: warp::http::Method,
    path: String,
    headers: warp::http::HeaderMap,
    body: Bytes,
    // sent a response back on this thread
    response_tx: oneshot::Sender<WarpResponse<Vec<u8>>>,
}


#[derive(TypedData)]
#[magnus(class = "HyperRuby::Response", mark)]
struct Response {
    status: u16,
    headers: Opaque<magnus::RHash>,
    body: Opaque<magnus::RString>,
}

impl DataTypeFunctions for Response {
    fn mark(&self, marker: &gc::Marker) {
        marker.mark(self.headers);
        marker.mark(self.body);
    }
}

impl Response {
    pub fn new(status: u16, headers: magnus::RHash, body: RString) -> Self {
        Self {
            status,
            headers: headers.into(),
            body: body.into(),
        }
    }
}

#[magnus::wrap(class = "HyperRuby::Server")]
struct Server {
    server_handle: Arc<Mutex<Option<JoinHandle<()>>>>,
    config: RefCell<ServerConfig>,
    work_rx: RefCell<Option<crossbeam_channel::Receiver<WorkRequest>>>,
    work_tx: RefCell<Option<Arc<crossbeam_channel::Sender<WorkRequest>>>>,
}


pub(crate) fn nogvl<F, R>(mut func: F) -> R
where
    F: FnMut() -> R,
    R: Sized,
{
    unsafe extern "C" fn call_without_gvl<F, R>(arg: *mut c_void) -> *mut c_void
    where
        F: FnMut() -> R,
        R: Sized,
    {
        let arg = arg as *mut (&mut F, &mut MaybeUninit<R>);
        let (func, result) = unsafe { &mut *arg };
        result.write(func());
        null_mut()
    }
    let result = MaybeUninit::uninit();
    let arg_ptr = &(&mut func, &result) as *const _ as *mut c_void;
    unsafe {
        rb_thread_call_without_gvl(Some(call_without_gvl::<F, R>), arg_ptr, None, null_mut());
        result.assume_init()
    }
}

impl Server {
    pub fn new() -> Self {
        let (work_tx, work_rx) = crossbeam_channel::bounded(1000);
        
        Self {
            server_handle: Arc::new(Mutex::new(None)),
            config: RefCell::new(ServerConfig::new()),
            work_rx: RefCell::new(Some(work_rx)),
            work_tx: RefCell::new(Some(Arc::new(work_tx))),
        }
    }

    pub fn configure(&self, config: magnus::RHash) -> Result<(), MagnusError> {
        let mut server_config = self.config.borrow_mut();
        if let Some(bind_address) = config.get(magnus::Symbol::new("bind_address")) {
            server_config.bind_address = String::try_convert(bind_address)?;
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
                        
                        // Create Ruby hash with request data
                        let req_hash = magnus::RHash::new();
                        req_hash.aset(magnus::Symbol::new("method"), work_request.method.to_string())?;
                        req_hash.aset(magnus::Symbol::new("path"), work_request.path)?;
                        
                        // Convert headers to Ruby hash
                        let headers_hash = magnus::RHash::new();
                        for (key, value) in work_request.headers.iter() {
                            if let Ok(value_str) = value.to_str() {
                                headers_hash.aset(key.as_str(), value_str)?;
                            }
                        }
                        req_hash.aset(magnus::Symbol::new("headers"), headers_hash)?;
                        
                        // Convert body to Ruby string
                        req_hash.aset(magnus::Symbol::new("body"), magnus::RString::from_slice(&work_request.body[..]))?;
                            
                        // Call the Ruby block and handle the response
                        let warp_response = match block.call::<_, Value>([req_hash]) {
                            Ok(result) => {
                                let ref_response = Obj::<Response>::try_convert(result).unwrap();

                                let mut response: WarpResponse<Vec<u8>>;
                                let ruby = Ruby::get().unwrap(); // errors on non-Ruby thread
                                let response_body = ruby.get_inner(ref_response.body);
                                let ruby_response_headers = ruby.get_inner(ref_response.headers);
                                
                                // safe because RString will not be cleared here before we copy the bytes into our own Vector.
                                unsafe {
                                    // copy directly to bytes here so we don't have to worry about encoding checks
                                    let rust_body = Vec::from(response_body.as_slice());
                                    
                                    response = WarpResponse::new(rust_body);
                                }

                                *response.status_mut() = warp::http::StatusCode::from_u16(ref_response.status).unwrap();
                                let response_headers = response.headers_mut();
                                
                                ruby_response_headers.foreach(|key: String, value: String| {
                                    if let Ok(header_name) = warp::http::header::HeaderName::from_bytes(key.as_bytes()) {
                                        response_headers.insert(header_name, warp::http::HeaderValue::from_str(&value).unwrap());
                                    }
                                    else {
                                        MagnusError::new(magnus::exception::runtime_error(), "Invalid header name");
                                    }
                                    Ok(ForEach::Continue)
                                }).unwrap();

                                response
                            },
                            Err(e) => {
                                println!("Block call failed: {:?}", e);
                                create_error_response("Block call failed")
                            }
                        };

                        match work_request.response_tx.send(warp_response) {
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
 
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| MagnusError::new(magnus::exception::runtime_error(), e.to_string()))?;

        println!("Starting server");
        
        rt.block_on(async {
            let work_tx = work_tx.clone();
            
            let server_task = tokio::spawn(async move {
                let any_route = warp::any()
                    .and(warp::filters::method::method())
                    .and(warp::filters::path::full())
                    .and(warp::header::headers_cloned())
                    .and(warp::body::bytes())
                    .and(warp::any().map(move || work_tx.clone()))
                    .and_then(handle_request);

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

        // Drop the channel to signal workers to shut down
        self.work_tx.borrow_mut().take();

        Ok(())
    }
}


// Helper function to create error responses
fn create_error_response(error_message: &str) -> WarpResponse<Vec<u8>> {
    let mut response = WarpResponse::new(format!(r#"{{"error": "{}"}}"#, error_message).into_bytes());
    *response.status_mut() = warp::http::StatusCode::INTERNAL_SERVER_ERROR;
    response.headers_mut().insert(
        warp::http::header::CONTENT_TYPE,
        warp::http::HeaderValue::from_static("application/json")
    );
    response
}

async fn handle_request(
    method: warp::http::Method,
    path: warp::path::FullPath,
    headers: warp::http::HeaderMap,
    body: Bytes,
    work_tx: Arc<crossbeam_channel::Sender<WorkRequest>>,
) -> Result<WarpResponse<Vec<u8>>, warp::Rejection> {
    let (response_tx, response_rx) = oneshot::channel();

    let work_request = WorkRequest {
        method,
        path: path.as_str().to_string(),
        headers,
        body,
        response_tx,
    };

    if let Err(_) = work_tx.send(work_request) {
        return Err(warp::reject::reject());
    }

    match response_rx.await {
        Ok(response) => Ok(response),
        Err(_) => Err(warp::reject::reject()),
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

    Ok(())
}