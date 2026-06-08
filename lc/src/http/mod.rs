mod mime;
mod request;
mod response;
mod router;

pub use request::{BodyStatus, HeadStatus, Request, decode_body, parse_head};
pub use router::{error_response, handle};

/// Reason phrase for a status code. Covers the codes this server emits.
pub fn reason_phrase(code: u16) -> &'static str {
    match code {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        413 => "Payload Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        _ => "Error",
    }
}

/// Decide whether to keep the connection alive after this request.
/// HTTP/1.1 defaults to keep-alive unless `Connection: close`; older versions
/// require an explicit `Connection: keep-alive`.
pub fn wants_keep_alive(version: &str, connection: Option<&str>) -> bool {
    let conn = connection.map(|c| c.to_ascii_lowercase());
    match version {
        "HTTP/1.1" => conn.as_deref() != Some("close"),
        _ => conn.as_deref() == Some("keep-alive"),
    }
}
