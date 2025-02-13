use magnus::{gc, value::{qnil, Opaque, ReprValue}, DataTypeFunctions, IntoValue, RString, Ruby, TypedData, Value};

use bytes::Bytes;
use hyper::Request as HyperRequest;

use rb_sys::{rb_str_resize, rb_str_cat, VALUE};

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

    pub fn fill_body(&self, buffer: RString) -> usize {
        let body = self.request.body();
        let body_len = body.len();

        // Access the ruby string VALUE directly, and resize to 0 (keeping the capacity), 
        // then copy our buffer into it.
        unsafe {
            let rb_value = buffer.as_value();
            let inner: VALUE = std::ptr::read(&rb_value as *const _ as *const VALUE);
            rb_str_resize(inner, 0);
            if body_len > 0 {
                rb_str_cat(inner, body.as_ptr() as *const i8, body.len().try_into().unwrap());
            }
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