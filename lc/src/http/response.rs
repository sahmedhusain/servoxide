//! HTTP response model and serialization.

use super::reason_phrase;

/// A response under construction. `Content-Length` and `Connection` are added
/// automatically at serialization time, so callers never set them by hand
/// (which is how the audit's "always set Content-Length / Connection" and the
/// "compute body first" rules are guaranteed).
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub keep_alive: bool,
}

impl Response {
    pub fn new(status: u16) -> Self {
        Response {
            status,
            headers: Vec::new(),
            body: Vec::new(),
            keep_alive: true,
        }
    }

    pub fn with_body(status: u16, content_type: &str, body: Vec<u8>) -> Self {
        let mut r = Response::new(status);
        r.set_header("Content-Type", content_type);
        r.body = body;
        r
    }

    pub fn set_header(&mut self, name: &str, value: &str) {
        self.headers.push((name.to_string(), value.to_string()));
    }

    /// Serialize to the wire format. Always emits exactly one `Content-Length`
    /// (computed from the body) and one `Connection` header.
    pub fn into_bytes(self) -> Vec<u8> {
        let mut out =
            format!("HTTP/1.1 {} {}\r\n", self.status, reason_phrase(self.status)).into_bytes();
        for (name, value) in &self.headers {
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(b": ");
            out.extend_from_slice(value.as_bytes());
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(format!("Content-Length: {}\r\n", self.body.len()).as_bytes());
        let conn = if self.keep_alive { "keep-alive" } else { "close" };
        out.extend_from_slice(format!("Connection: {conn}\r\n\r\n").as_bytes());
        out.extend_from_slice(&self.body);
        out
    }
}
