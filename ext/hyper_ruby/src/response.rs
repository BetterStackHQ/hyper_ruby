use magnus::{gc, DataTypeFunctions, RHash, RString, TypedData, value::Opaque};

// Response object returned to Ruby; holds reference to the opaque ruby types for the headers and body.
#[derive(TypedData)]
#[magnus(class = "HyperRuby::Response", mark)]
pub struct Response {
    pub status: u16,
    pub headers: Opaque<RHash>,
    pub body: Opaque<RString>,
}

impl DataTypeFunctions for Response {
    fn mark(&self, marker: &gc::Marker) {
        marker.mark(self.headers);
        marker.mark(self.body);
    }
}

impl Response {
    pub fn new(status: u16, headers: RHash, body: RString) -> Self {
        Self {
            status,
            headers: headers.into(),
            body: body.into(),
        }
    }
}