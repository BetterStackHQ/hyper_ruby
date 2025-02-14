use magnus::{r_hash::ForEach, wrap, RHash, RString, Error as MagnusError};

use hyper::{header::HeaderName, Response as HyperResponse, StatusCode};
use http_body_util::Full;
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
}