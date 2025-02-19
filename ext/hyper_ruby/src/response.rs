use magnus::{r_hash::ForEach, RHash, RString, Error as MagnusError, Value, TryConvert};
use hyper::{header::{HeaderName, HeaderMap}, Response as HyperResponse};
use hyper::body::{Frame, Body};
use bytes::Bytes;
use std::pin::Pin;
use crate::grpc;

#[derive(Debug, Clone)]
pub struct ResponseError(String);

impl std::fmt::Display for ResponseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ResponseError: {}", self.0)
    }
}

impl std::error::Error for ResponseError {}

// Define a custom body type that can include trailers
#[derive(Debug, Clone)]
pub struct BodyWithTrailers {
    data: Bytes,
    data_sent: bool,
    trailers_sent: bool,
    trailers: Option<HeaderMap>,
}

impl BodyWithTrailers {
    pub fn new(data: Bytes, trailers: Option<HeaderMap>) -> Self {
        Self {
            data,
            data_sent: false,
            trailers_sent: false,
            trailers,
        }
    }

    pub fn get_data(&self) -> &Bytes {
        &self.data
    }
}

impl Body for BodyWithTrailers {
    type Data = Bytes;
    type Error = ResponseError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        if !self.data_sent && !self.data.is_empty() {
            self.data_sent = true;
            let data = self.data.clone();
            return std::task::Poll::Ready(Some(Ok(Frame::data(data))));
        }
        
        if !self.trailers_sent {
            self.trailers_sent = true;
            if let Some(trailers) = &self.trailers {
                return std::task::Poll::Ready(Some(Ok(Frame::trailers(trailers.clone()))));
            }
        }
        
        std::task::Poll::Ready(None)
    }
}

#[derive(Debug, Clone)]
#[magnus::wrap(class = "HyperRuby::Response")]
pub struct Response {
    response: HyperResponse<BodyWithTrailers>
}

#[derive(Debug, Clone)]
#[magnus::wrap(class = "HyperRuby::GrpcResponse")]
pub struct GrpcResponse {
    response: HyperResponse<BodyWithTrailers>
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
            unsafe {
                let rust_body = Bytes::copy_from_slice(body.as_slice());
                builder_headers.insert(
                    HeaderName::from_static("content-length"),
                    rust_body.len().to_string().try_into().unwrap()
                );
                match builder.body(BodyWithTrailers::new(rust_body, None)) {
                    Ok(response) => Ok(Self { response }),
                    Err(_) => Err(MagnusError::new(magnus::exception::runtime_error(), "Failed to create response"))
                }
            }
        } else {
            builder_headers.insert(
                HeaderName::from_static("content-length"),
                "0".try_into().unwrap()
            );
            match builder.body(BodyWithTrailers::new(Bytes::new(), None)) {
                Ok(response) => Ok(Self { response }),
                Err(_) => Err(MagnusError::new(magnus::exception::runtime_error(), "Failed to create response"))
            }
        }
    }

    pub fn status(&self) -> u16 {
        self.response.status().into()
    }

    pub fn headers(&self) -> RHash {
        let headers = RHash::new();
        for (name, value) in self.response.headers() {
            headers.aset(name.to_string(), value.to_str().unwrap().to_string()).unwrap();
        }
        headers
    }

    pub fn body(&self) -> RString {
        // For non-gRPC responses, just return the data part
        let body = self.response.body().get_data();
        RString::from_slice(body.as_ref())
    }

    pub fn into_hyper_response(self) -> HyperResponse<BodyWithTrailers> {
        self.response
    }
}

impl GrpcResponse {
    pub fn new(status: u16, body: RString) -> Result<Self, MagnusError> {
        let builder = HyperResponse::builder()
            .status(200)  // Always 200 for gRPC
            .header("content-type", "application/grpc+proto");

        let body_bytes = unsafe { Bytes::copy_from_slice(body.as_slice()) };
        let framed_message = grpc::encode_grpc_frame(&body_bytes);
        
        let mut trailers = HeaderMap::new();
        trailers.insert("grpc-status", status.to_string().parse().unwrap());
        trailers.insert("grpc-accept-encoding", "identity,gzip,deflate,zstd".parse().unwrap());
        
        Ok(Self { response: builder.body(BodyWithTrailers::new(framed_message, Some(trailers))).unwrap() })
    }

    pub fn error(status: Value, message: RString) -> Result<Self, MagnusError> {
        let status_num = u32::try_convert(status)?;
        let message_str = unsafe { message.as_str().unwrap() };
        
        let builder = HyperResponse::builder()
            .status(200)  // Always 200 for gRPC
            .header("content-type", "application/grpc+proto");
        
        let mut trailers = HeaderMap::new();
        trailers.insert("grpc-status", status_num.to_string().parse().unwrap());
        trailers.insert("grpc-accept-encoding", "identity,gzip,deflate,zstd".parse().unwrap());
        
        if !message_str.is_empty() {
            trailers.insert("grpc-message", message_str.parse().unwrap());
        }

        Ok(Self { response: builder.body(BodyWithTrailers::new(Bytes::new(), Some(trailers))).unwrap() })
    }

    pub fn status(&self) -> u16 {
        // For gRPC, we need to look at the grpc-status header
        if let Some(status) = self.response.headers().get("grpc-status") {
            if let Ok(status_str) = status.to_str() {
                if let Ok(status_num) = status_str.parse::<u16>() {
                    return status_num;
                }
            }
        }
        0 // Default to OK if no status found
    }

    pub fn headers(&self) -> RHash {
        let headers = RHash::new();
        for (name, value) in self.response.headers() {
            headers.aset(name.to_string(), value.to_str().unwrap().to_string()).unwrap();
        }
        headers
    }

    pub fn body(&self) -> RString {
        // For gRPC responses, decode the frame
        let body = self.response.body().get_data();
        if let Some((_, message)) = grpc::decode_grpc_frame(body) {
            RString::from_slice(message.as_ref())
        } else {
            RString::new("")
        }
    }

    pub fn into_hyper_response(self) -> HyperResponse<BodyWithTrailers> {
        self.response
    }
}