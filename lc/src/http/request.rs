//! HTTP/1.1 request parsing.
//!
//! Parsing is split in two so the server can make a decision in between: first
//! [`parse_head`] reads the request line and headers (which reveal the `Host`
//! and thus which `server` block and body-size limit apply), then
//! [`decode_body`] consumes the body according to `Content-Length` or
//! `Transfer-Encoding: chunked`.
//!
//! Both operate on the whole accumulated buffer and report [`HeadStatus::NeedMore`]
//! / [`BodyStatus::NeedMore`] when more bytes are required, so the caller can
//! feed one `read` per event and retry without keeping a hand-rolled state
//! machine. Header sections are small, so re-scanning per read is cheap.

use std::collections::HashMap;

/// Upper bound on the request line + headers section. Beyond this we give up
/// with `400` rather than buffer unboundedly.
const MAX_HEAD_BYTES: usize = 64 * 1024;

/// A fully parsed request, handed to the router.
pub struct Request {
    pub method: String,
    pub path: String,
    pub query: String,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

/// The request line and headers, before the body is known/consumed.
pub struct RequestHead {
    pub method: String,
    pub path: String,
    pub query: String,
    pub version: String,
    pub headers: HashMap<String, String>,
    /// Offset in the source buffer where the body begins.
    pub head_len: usize,
}

impl RequestHead {
    pub fn host(&self) -> Option<&str> {
        self.headers.get("host").map(String::as_str)
    }

    /// Determine how to read the body, or `Err(status)` for an unacceptable
    /// framing (unsupported transfer-encoding, bad content-length).
    pub fn body_mode(&self) -> Result<BodyMode, u16> {
        let te = self.headers.get("transfer-encoding");
        let cl = self.headers.get("content-length");
        match (te, cl) {
            (Some(te), _) => {
                // We only implement `chunked`. Per RFC 7230 a present TE takes
                // precedence over any Content-Length.
                let chunked = te
                    .to_ascii_lowercase()
                    .split(',')
                    .any(|t| t.trim() == "chunked");
                if chunked {
                    Ok(BodyMode::Chunked)
                } else {
                    Err(400)
                }
            }
            (None, Some(cl)) => match cl.trim().parse::<usize>() {
                Ok(0) => Ok(BodyMode::None),
                Ok(n) => Ok(BodyMode::Length(n)),
                Err(_) => Err(400),
            },
            (None, None) => Ok(BodyMode::None),
        }
    }
}

pub enum HeadStatus {
    NeedMore,
    Ok(RequestHead),
    Bad(u16),
}

pub enum BodyMode {
    None,
    Length(usize),
    Chunked,
}

pub enum BodyStatus {
    NeedMore,
    /// Body fully decoded. `consumed` is how many bytes of the body region were
    /// used, so the caller can drain the finished request for keep-alive.
    Complete { body: Vec<u8>, consumed: usize },
    TooLarge,
    Bad(u16),
}

/// Parse the request line and headers from the front of `buf`.
pub fn parse_head(buf: &[u8]) -> HeadStatus {
    let end = match find_from(buf, 0, b"\r\n\r\n") {
        Some(i) => i,
        None => {
            if buf.len() > MAX_HEAD_BYTES {
                return HeadStatus::Bad(400);
            }
            return HeadStatus::NeedMore;
        }
    };
    let head_len = end + 4;

    let text = match std::str::from_utf8(&buf[..end]) {
        Ok(t) => t,
        Err(_) => return HeadStatus::Bad(400),
    };

    let mut lines = text.split("\r\n");

    // Request line: METHOD SP request-target SP HTTP-version
    let request_line = match lines.next() {
        Some(l) => l,
        None => return HeadStatus::Bad(400),
    };
    let mut parts = request_line.split(' ');
    let (method, target, version, extra) =
        (parts.next(), parts.next(), parts.next(), parts.next());
    let (method, target, version) = match (method, target, version, extra) {
        (Some(m), Some(t), Some(v), None) if !m.is_empty() && !t.is_empty() => (m, t, v),
        _ => return HeadStatus::Bad(400),
    };
    if !version.starts_with("HTTP/") {
        return HeadStatus::Bad(400);
    }

    // Headers
    let mut headers = HashMap::new();
    for line in lines {
        let (name, value) = match line.split_once(':') {
            Some(pair) => pair,
            None => return HeadStatus::Bad(400),
        };
        // A valid field-name is a non-empty token with no whitespace.
        if name.is_empty() || name.chars().any(|c| c == ' ' || c == '\t') {
            return HeadStatus::Bad(400);
        }
        headers.insert(name.to_ascii_lowercase(), value.trim().to_string());
    }

    let (path_raw, query) = match target.split_once('?') {
        Some((p, q)) => (p, q.to_string()),
        None => (target, String::new()),
    };

    HeadStatus::Ok(RequestHead {
        method: method.to_string(),
        path: percent_decode(path_raw),
        query,
        version: version.to_string(),
        headers,
        head_len,
    })
}

/// Decode the body sitting at the front of `body_buf` (i.e. the bytes after the
/// header section) according to `mode`, enforcing `max_body`.
pub fn decode_body(body_buf: &[u8], mode: &BodyMode, max_body: usize) -> BodyStatus {
    match mode {
        BodyMode::None => BodyStatus::Complete {
            body: Vec::new(),
            consumed: 0,
        },
        BodyMode::Length(n) => {
            let n = *n;
            // Reject before buffering the whole body (audit: check the
            // Content-Length value, not the bytes already read).
            if n > max_body {
                return BodyStatus::TooLarge;
            }
            if body_buf.len() < n {
                BodyStatus::NeedMore
            } else {
                BodyStatus::Complete {
                    body: body_buf[..n].to_vec(),
                    consumed: n,
                }
            }
        }
        BodyMode::Chunked => decode_chunked(body_buf, max_body),
    }
}

/// Decode `Transfer-Encoding: chunked` from the accumulated buffer.
fn decode_chunked(buf: &[u8], max_body: usize) -> BodyStatus {
    let mut out: Vec<u8> = Vec::new();
    let mut pos = 0;

    loop {
        // chunk-size line (with optional ;extension we ignore)
        let line_end = match find_from(buf, pos, b"\r\n") {
            Some(i) => i,
            None => return BodyStatus::NeedMore,
        };
        let size_str = match std::str::from_utf8(&buf[pos..line_end]) {
            Ok(s) => s.split(';').next().unwrap_or("").trim(),
            Err(_) => return BodyStatus::Bad(400),
        };
        let size = match usize::from_str_radix(size_str, 16) {
            Ok(n) => n,
            Err(_) => return BodyStatus::Bad(400),
        };
        pos = line_end + 2;

        if size == 0 {
            // Last chunk: consume optional trailer headers up to a blank line.
            loop {
                let te = match find_from(buf, pos, b"\r\n") {
                    Some(i) => i,
                    None => return BodyStatus::NeedMore,
                };
                if te == pos {
                    return BodyStatus::Complete {
                        body: out,
                        consumed: te + 2, // include the terminating CRLF
                    };
                }
                pos = te + 2;
            }
        }

        // Need `size` data bytes plus the trailing CRLF.
        if buf.len() < pos + size + 2 {
            return BodyStatus::NeedMore;
        }
        if out.len() + size > max_body {
            return BodyStatus::TooLarge;
        }
        out.extend_from_slice(&buf[pos..pos + size]);
        pos += size;
        if &buf[pos..pos + 2] != b"\r\n" {
            return BodyStatus::Bad(400);
        }
        pos += 2;
    }
}

/// Find `needle` in `haystack` at or after `start`.
fn find_from(haystack: &[u8], start: usize, needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < start + needle.len() {
        return None;
    }
    (start..=haystack.len() - needle.len()).find(|&i| &haystack[i..i + needle.len()] == needle)
}

/// Decode `%XX` escapes in a path. Invalid escapes are passed through verbatim.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex_val(b[i + 1]), hex_val(b[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head_ok(buf: &[u8]) -> RequestHead {
        match parse_head(buf) {
            HeadStatus::Ok(h) => h,
            HeadStatus::NeedMore => panic!("needed more"),
            HeadStatus::Bad(c) => panic!("bad: {c}"),
        }
    }

    #[test]
    fn parses_request_line_and_headers() {
        let h = head_ok(b"GET /a/b?x=1&y=2 HTTP/1.1\r\nHost: example.com\r\nAccept: */*\r\n\r\n");
        assert_eq!(h.method, "GET");
        assert_eq!(h.path, "/a/b");
        assert_eq!(h.query, "x=1&y=2");
        assert_eq!(h.version, "HTTP/1.1");
        assert_eq!(h.host(), Some("example.com"));
        assert_eq!(h.headers.get("accept").map(String::as_str), Some("*/*"));
    }

    #[test]
    fn percent_decodes_path() {
        let h = head_ok(b"GET /a%20b%2Fc HTTP/1.1\r\nHost: x\r\n\r\n");
        assert_eq!(h.path, "/a b/c");
    }

    #[test]
    fn need_more_until_headers_complete() {
        assert!(matches!(parse_head(b"GET / HTTP/1.1\r\nHost: x"), HeadStatus::NeedMore));
    }

    #[test]
    fn malformed_request_line_is_400() {
        assert!(matches!(parse_head(b"GET /\r\n\r\n"), HeadStatus::Bad(400)));
        assert!(matches!(parse_head(b"JUST GARBAGE LINE HERE\r\n\r\n"), HeadStatus::Bad(400)));
    }

    #[test]
    fn content_length_body() {
        let h = head_ok(b"POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\n\r\nhello");
        let mode = h.body_mode().unwrap();
        assert!(matches!(mode, BodyMode::Length(5)));
        let body = &b"POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\n\r\nhello"[h.head_len..];
        match decode_body(body, &mode, 1024) {
            BodyStatus::Complete { body, consumed } => {
                assert_eq!(body, b"hello");
                assert_eq!(consumed, 5);
            }
            _ => panic!("expected complete body"),
        }
    }

    #[test]
    fn content_length_needs_more() {
        let mode = BodyMode::Length(5);
        assert!(matches!(decode_body(b"hel", &mode, 1024), BodyStatus::NeedMore));
    }

    #[test]
    fn content_length_too_large() {
        let mode = BodyMode::Length(2000);
        assert!(matches!(decode_body(b"", &mode, 1024), BodyStatus::TooLarge));
    }

    #[test]
    fn chunked_body_decodes() {
        // "Wikipedia in\r\n\r\nchunks." style
        let raw = b"4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n";
        match decode_body(raw, &BodyMode::Chunked, 1024) {
            BodyStatus::Complete { body, consumed } => {
                assert_eq!(body, b"Wikipedia");
                assert_eq!(consumed, raw.len());
            }
            _ => panic!("expected complete chunked body"),
        }
    }

    #[test]
    fn chunked_needs_more() {
        let raw = b"4\r\nWi"; // incomplete chunk data
        assert!(matches!(decode_body(raw, &BodyMode::Chunked, 1024), BodyStatus::NeedMore));
    }

    #[test]
    fn chunked_too_large() {
        let raw = b"5\r\nhello\r\n0\r\n\r\n";
        assert!(matches!(decode_body(raw, &BodyMode::Chunked, 3), BodyStatus::TooLarge));
    }

    #[test]
    fn chunked_with_extension_is_ignored() {
        let raw = b"4;foo=bar\r\nWiki\r\n0\r\n\r\n";
        match decode_body(raw, &BodyMode::Chunked, 1024) {
            BodyStatus::Complete { body, .. } => assert_eq!(body, b"Wiki"),
            _ => panic!("expected complete body"),
        }
    }

    #[test]
    fn transfer_encoding_takes_precedence() {
        let h = head_ok(b"POST / HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\nContent-Length: 9\r\n\r\n");
        assert!(matches!(h.body_mode().unwrap(), BodyMode::Chunked));
    }
}
