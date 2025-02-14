use std::os::raw::c_char;

use magnus::{value::{qnil, ReprValue}, RString, Value};

use bytes::Bytes;
use hyper::Request as HyperRequest;

use rb_sys::{rb_str_set_len, rb_str_modify, rb_str_modify_expand, rb_str_capacity, RSTRING_PTR, VALUE};

// Type passed to ruby giving access to the request properties.
#[magnus::wrap(class = "HyperRuby::Request")]
pub struct Request {
    pub request: HyperRequest<Bytes>
}

impl Request {
    pub fn method(&self) -> String {
        self.request.method().to_string()
    }

    pub fn path(&self) -> RString {
        RString::new(&self.request.uri().path())
    }

    pub fn header(&self, key: RString) -> Value {
        // Avoid allocating a new header key string
        let key_str = unsafe { key.as_str().unwrap() };
        match self.request.headers().get(key_str) {
            Some(value) => match value.to_str() {
                Ok(value) => RString::new(value).as_value(),
                Err(_) => qnil().as_value(),
            },
            None => qnil().as_value(),
        }
    }

    pub fn body_size(&self) -> usize {
        self.request.body().len()
    }

    pub fn body(&self) -> RString {        
        let buffer = RString::buf_new(self.body_size());
        self.fill_body(buffer);
        buffer
    }

    pub fn fill_body(&self, buffer: RString) -> i64 {
        let body = self.request.body();
        let body_len: i64 = body.len().try_into().unwrap();

        // Access the ruby string VALUE directly, and resize to 0 (keeping the capacity), 
        // then copy our buffer into it.
        unsafe {
            let rb_value = buffer.as_value();
            let inner: VALUE = std::ptr::read(&rb_value as *const _ as *const VALUE);
            let existing_capacity = rb_str_capacity(inner) as i64;

            // If the buffer is too small, expand it.
            if existing_capacity < body_len.try_into().unwrap() {
                rb_str_modify_expand(inner, body_len);
            }
            else {
                rb_str_modify(inner);
            }

            if body_len > 0 {
                let body_ptr = body.as_ptr() as *const c_char;
                let rb_string_ptr = RSTRING_PTR(inner) as *mut c_char;
                std::ptr::copy(body_ptr, rb_string_ptr, body_len as usize);
            }

            rb_str_set_len(inner, body_len);
        }

        body_len
    }

    pub fn inspect(&self) -> RString {
        let method = self.request.method().to_string();
        let path = self.request.uri().path();
        let body_size = self.request.body().len();
        RString::new(&format!("#<HyperRuby::Request method={} path={} body_size={}>", method, path, body_size))
    }
} 