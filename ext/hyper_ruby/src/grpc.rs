use bytes::{Bytes, BytesMut, BufMut};
use hyper::{
    Request as HyperRequest,
    Response as HyperResponse,
    Method,
    header::HeaderMap,
};
use log::debug;
use crate::response::BodyWithTrailers;

const GRPC_HEADER_SIZE: usize = 5;

pub fn is_grpc_request(request: &HyperRequest<Bytes>) -> bool {
    debug!("Validating gRPC request: {} {}", request.method(), request.uri().path());
    debug!("Headers: {:?}", request.headers());

    // Check required headers according to spec
    if request.method() != Method::POST {
        debug!("Not a gRPC request: wrong method");
        return false;
    }

    // Check content-type starts with application/grpc
    if let Some(content_type) = request.headers().get("content-type") {
        if let Ok(content_type_str) = content_type.to_str() {
            if !content_type_str.starts_with("application/grpc") {
                debug!("Not a gRPC request: invalid content-type");
                return false;
            }
        } else {
            debug!("Not a gRPC request: invalid content-type encoding");
            return false;
        }
    } else {
        debug!("Not a gRPC request: missing content-type");
        return false;
    }

    // Check TE header is present with "trailers"
    if let Some(te) = request.headers().get("te") {
        if let Ok(te_str) = te.to_str() {
            if !te_str.contains("trailers") {
                debug!("Not a gRPC request: TE header missing 'trailers'");
                return false;
            }
        } else {
            debug!("Not a gRPC request: invalid TE header encoding");
            return false;
        }
    } else {
        debug!("Not a gRPC request: missing TE header");
        return false;
    }

    // Accept any path for now, but extract service and method if possible
    let path = request.uri().path();
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    
    if parts.is_empty() {
        debug!("Not a gRPC request: empty path");
        return false;
    }

    debug!("Valid gRPC request with path parts: {:?}", parts);
    true
}

pub fn decode_grpc_frame(bytes: &[u8]) -> Option<Bytes> {
    if bytes.len() < GRPC_HEADER_SIZE {
        return None;
    }

    // GRPC frame format:
    // Compressed-Flag (1 byte) | Message-Length (4 bytes) | Message
    let compressed = bytes[0] != 0;
    if compressed {
        // We don't support compression yet
        return None;
    }

    let message_len = u32::from_be_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]) as usize;
    if bytes.len() < GRPC_HEADER_SIZE + message_len {
        return None;
    }

    Some(Bytes::copy_from_slice(&bytes[GRPC_HEADER_SIZE..GRPC_HEADER_SIZE + message_len]))
}

pub fn encode_grpc_frame(message: &[u8]) -> Bytes {
    let mut frame = BytesMut::with_capacity(GRPC_HEADER_SIZE + message.len());
    
    // Compressed flag (0 = not compressed)
    frame.put_u8(0);
    // Message length (4 bytes, big endian)
    frame.put_u32(message.len() as u32);
    // Message
    frame.put_slice(message);
    
    frame.freeze()
}

pub fn create_grpc_error_response(http_status: u16, grpc_status: u32, message: &str) -> HyperResponse<BodyWithTrailers> {
    // For protocol-level errors (e.g. HTTP/2 issues), use the provided HTTP status
    // For application-level errors, use 200 and communicate via grpc-status
    let status = if http_status == 200 || (http_status >= 400 && http_status < 500) {
        200 // Use 200 for application-level errors
    } else {
        http_status // Keep protocol-level error status codes
    };

    let builder = HyperResponse::builder()
        .status(status)
        .header("content-type", "application/grpc+proto");  // Use +proto suffix
    
    // Create trailers
    let mut trailers = HeaderMap::new();
    trailers.insert("grpc-status", grpc_status.to_string().parse().unwrap());
    trailers.insert("grpc-accept-encoding", "identity,gzip,deflate,zstd".parse().unwrap());
    
    // Add grpc-message if provided
    if !message.is_empty() {
        trailers.insert("grpc-message", message.parse().unwrap());
    }
    
    // Create response with custom body that includes trailers
    builder.body(BodyWithTrailers::new(Bytes::new(), Some(trailers))).unwrap()
} 