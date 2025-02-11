use magnus::{value::{qnil, ReprValue}, RString, Value};
use bytes::Bytes;
use hyper::Request as HyperRequest;

// Type passed to ruby giving access to the request properties.
#[derive(Debug)]
#[magnus::wrap(class = "HyperRuby::Request", free_immediately)]
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

    pub fn header(&self, key: String) -> Value {
        match self.request.headers().get(key) {
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

    pub fn body(&self) -> Value {
        let body = self.request.body();
        if body.is_empty() {
            return qnil().as_value();
        }

        let result = RString::buf_new(body.len());
        result.cat(body.as_ref());
        result.as_value()
    }

    pub fn inspect(&self) -> RString {
        let method = self.request.method().to_string();
        let path = self.request.uri().path();
        let body_size = self.request.body().len();
        RString::new(&format!("#<HyperRuby::Request method={} path={} body_size={}>", method, path, body_size))
    }
} 