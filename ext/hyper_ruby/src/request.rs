use std::os::raw::c_char;

use magnus::{value::{qnil, ReprValue}, RString, Value, RHash};

use bytes::Bytes;
use hyper::Request as HyperRequest;

use rb_sys::{rb_str_set_len, rb_str_modify, rb_str_modify_expand, rb_str_capacity, RSTRING_PTR, VALUE};

use crate::grpc;
use log::debug;
use form_urlencoded;

// Trait for common buffer filling behavior
trait FillBuffer {
    // Get the bytes to be copied into the buffer
    fn get_body_bytes(&self) -> Bytes;
    
    // Get the size of the body
    fn get_body_size(&self) -> usize;

    // Common implementation for filling a Ruby string buffer
    fn fill_buffer(&self, buffer: RString) -> i64 {
        let body_bytes = self.get_body_bytes();
        let body_len: i64 = body_bytes.len().try_into().unwrap();

        unsafe {
            let rb_value = buffer.as_value();
            let inner: VALUE = std::ptr::read(&rb_value as *const _ as *const VALUE);
            let existing_capacity = rb_str_capacity(inner) as i64;

            if existing_capacity < body_len {
                rb_str_modify_expand(inner, body_len);
            } else {
                rb_str_modify(inner);
            }

            if body_len > 0 {
                let body_ptr = body_bytes.as_ptr() as *const c_char;
                let rb_string_ptr = RSTRING_PTR(inner) as *mut c_char;
                std::ptr::copy(body_ptr, rb_string_ptr, body_len as usize);
            }

            rb_str_set_len(inner, body_len);
        }

        body_len
    }
}

// Base HTTP request type
#[derive(Debug)]
#[magnus::wrap(class = "HyperRuby::Request")]
pub struct Request {
    request: HyperRequest<Bytes>
}

// Specialized gRPC request type
#[derive(Debug)]
#[magnus::wrap(class = "HyperRuby::GrpcRequest")]
pub struct GrpcRequest {
    request: HyperRequest<Bytes>,
    service: String,
    method: String
}

impl FillBuffer for Request {
    fn get_body_bytes(&self) -> Bytes {
        self.request.body().clone()
    }

    fn get_body_size(&self) -> usize {
        self.request.body().len()
    }
}

impl FillBuffer for GrpcRequest {
    fn get_body_bytes(&self) -> Bytes {
        if let Some((_, message)) = grpc::decode_grpc_frame(self.request.body()) {
            message
        } else {
            Bytes::new()
        }
    }

    fn get_body_size(&self) -> usize {
        if let Some((_, message)) = grpc::decode_grpc_frame(self.request.body()) {
            message.len()
        } else {
            0
        }
    }
}

impl Request {
    pub fn new(request: HyperRequest<Bytes>) -> Self {
        Self { request }
    }

    pub fn method(&self) -> String {
        self.request.method().to_string()
    }

    pub fn path(&self) -> RString {
        RString::new(self.request.uri().path())
    }

    pub fn query_params(&self) -> RHash {
        let params = RHash::new();
        if let Some(query) = self.request.uri().query() {
            for (key, value) in form_urlencoded::parse(query.as_bytes()) {
                params.aset(key.to_string(), value.to_string()).unwrap();
            }
        }
        params
    }

    pub fn query_param(&self, key: RString) -> Value {
        let key_str = unsafe { key.as_str().unwrap() };
        if let Some(query) = self.request.uri().query() {
            for (param_key, value) in form_urlencoded::parse(query.as_bytes()) {
                if param_key == key_str {
                    return RString::new(&value).as_value();
                }
            }
        }
        qnil().as_value()
    }

    pub fn header(&self, key: RString) -> Value {
        let key_str = unsafe { key.as_str().unwrap() };
        match self.request.headers().get(key_str) {
            Some(value) => match value.to_str() {
                Ok(value) => RString::new(value).as_value(),
                Err(_) => qnil().as_value(),
            },
            None => qnil().as_value(),
        }
    }

    pub fn headers(&self) -> RHash {
        let headers = RHash::new();
        for (name, value) in self.request.headers() {
            if let Ok(value_str) = value.to_str() {
                headers.aset(name.to_string(), value_str.to_string()).unwrap();
            }
        }
        headers
    }

    pub fn body_size(&self) -> usize {
        self.get_body_size()
    }

    pub fn body(&self) -> RString {        
        let buffer = RString::buf_new(self.body_size());
        self.fill_body(buffer);
        buffer
    }

    pub fn fill_body(&self, buffer: RString) -> i64 {
        self.fill_buffer(buffer)
    }

    pub fn inspect(&self) -> RString {
        let method = self.request.method().to_string();
        let path = self.request.uri().path();
        let query = self.request.uri().query().unwrap_or("");
        let query_display = if !query.is_empty() { format!("?{}", query) } else { String::new() };
        let body_size = self.body_size();
        RString::new(&format!("#<HyperRuby::Request method={} path={}{} body_size={}>", 
            method, path, query_display, body_size))
    }
}

impl GrpcRequest {
    pub fn new(request: HyperRequest<Bytes>) -> Option<Self> {
        debug!("Creating GrpcRequest from path: {}", request.uri().path());
        
        // Path format could be "/Echo" or "/echo.Echo/Echo" - handle both
        let path = request.uri().path();
        let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        debug!("  Path parts: {:?}", parts);
        
        if parts.is_empty() {
            debug!("  Failed: Empty path");
            return None;
        }

        // If we have two parts, use them as service/method
        // If we have one part, use it as both
        let (service, method) = if parts.len() >= 2 {
            (parts[0].to_string(), parts[1].to_string())
        } else {
            (format!("echo.{}", parts[0]), parts[0].to_string())
        };

        debug!("  Extracted service: {}, method: {}", service, method);
        
        Some(Self {
            request,
            service,
            method
        })
    }

    pub fn service(&self) -> RString {
        RString::new(&self.service)
    }

    pub fn method(&self) -> RString {
        RString::new(&self.method)
    }

    pub fn header(&self, key: RString) -> Value {
        let key_str = unsafe { key.as_str().unwrap() };
        match self.request.headers().get(key_str) {
            Some(value) => match value.to_str() {
                Ok(value) => RString::new(value).as_value(),
                Err(_) => qnil().as_value(),
            },
            None => qnil().as_value(),
        }
    }

    pub fn headers(&self) -> RHash {
        let headers = RHash::new();
        for (name, value) in self.request.headers() {
            if let Ok(value_str) = value.to_str() {
                headers.aset(name.to_string(), value_str.to_string()).unwrap();
            }
        }
        headers
    }

    pub fn body_size(&self) -> usize {
        self.get_body_size()
    }

    pub fn body(&self) -> RString {        
        let buffer = RString::buf_new(self.body_size());
        self.fill_body(buffer);
        buffer
    }

    pub fn fill_body(&self, buffer: RString) -> i64 {
        self.fill_buffer(buffer)
    }

    pub fn is_compressed(&self) -> bool {
        if let Some((compressed, _)) = grpc::decode_grpc_frame(self.request.body()) {
            compressed
        } else {
            false
        }
    }

    pub fn inspect(&self) -> RString {
        let body_size = self.body_size();
        RString::new(&format!("#<HyperRuby::GrpcRequest service={} method={} body_size={}>", self.service, self.method, body_size))
    }
} 