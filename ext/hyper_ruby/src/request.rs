use magnus::{value::{qnil, ReprValue}, RString, Value};
use bytes::Bytes;
use warp::http::HeaderMap;

// Type passed to ruby giving access to the request properties.
#[derive(Debug)]
#[magnus::wrap(class = "HyperRuby::Request", free_immediately)]
pub struct Request {
    pub method: warp::http::Method,
    pub path: String,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl Request {
    pub fn method(&self) -> String {
        self.method.to_string()
    }

    pub fn path(&self) -> RString {
        RString::new(&self.path)
    }

    pub fn header(&self, key: String) -> Value {
        match self.headers.get(key) {
            Some(value) => match value.to_str() {
                Ok(value) => RString::new(value).as_value(),
                Err(_) => qnil().as_value(),
            },
            None => qnil().as_value(),
        }
    }

    pub fn body_size(&self) -> usize {
        self.body.len()
    }

    pub fn body(&self) -> Value {
        if self.body.is_empty() {
            return qnil().as_value();
        }

        let result = RString::buf_new(self.body_size());
        result.cat(self.body.as_ref());
        result.as_value()
    }
} 