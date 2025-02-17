use futures::FutureExt;
use magnus::{r_hash::ForEach, wrap, RHash, RString, Error as MagnusError};

use hyper::{header::HeaderName, Response as HyperResponse};
use http_body_util::{BodyExt, Full};
use bytes::Bytes;

// Response object returned to Ruby; holds reference to the opaque ruby types for the headers and body.
#[wrap(class = "HyperRuby::Response")]
pub struct Response {
    pub response: HyperResponse<Full<Bytes>>
}

impl Response {
    pub fn new(status: u16, headers: RHash, body: RString) -> Result<Self, MagnusError> {
        let mut builder = HyperResponse::builder()
            .status(status);

        let builder_headers = builder.headers_mut().unwrap();
        headers.foreach(|key: String, value: String| {
            let header_name = HeaderName::try_from(key).unwrap();
            builder_headers.insert(header_name, value.try_into().unwrap());
            Ok(ForEach::Continue)
        }).unwrap();
                
        if body.len() > 0 {
            // safe because RString will not be cleared here before we copy the bytes into our own Vector.
            unsafe {
                // copy directly to bytes here so we don't have to worry about encoding checks
                let rust_body = Bytes::copy_from_slice(body.as_slice());
                match builder.body(Full::new(rust_body)) {
                    Ok(response) => Ok(Self { response }),
                    Err(_) => Err(MagnusError::new(magnus::exception::runtime_error(), "Failed to create response"))
                }
            }
        } else {
            match builder.body(Full::new(Bytes::new())) {
                Ok(response) => Ok(Self { response }),
                Err(_) => Err(MagnusError::new(magnus::exception::runtime_error(), "Failed to create response"))
            }
        }
    }

    pub fn status(&self) -> u16 {
        self.response.status().into()
    }

    pub fn headers(&self) -> RHash {
        // map back from the hyper headers to the ruby hash; doesn't need to be performant,
        // only used in tests
        let headers = RHash::new();
        for (name, value) in self.response.headers() {
            headers.aset(name.to_string(), value.to_str().unwrap().to_string()).unwrap();
        }
        headers
    }

    pub fn body(&self) -> RString {
        // copy back from the hyper body to the ruby string; doesn't need to be performant,
        // only used in tests
        let body = self.response.body();
        match body.clone().frame().now_or_never() {
            Some(frame) => {
                let data_chunk = frame.unwrap().unwrap().into_data().unwrap();
                RString::from_slice(data_chunk.iter().as_slice())
            }
            None => RString::buf_new(0),
        }
    }
}