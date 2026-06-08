use std::fs;
use std::path::{Component, Path};

use super::mime::mime_for;
use super::reason_phrase;
use super::response::Response;
use super::Request;

/// Methods this server actually implements.
const IMPLEMENTED: [&str; 3] = ["GET", "POST", "DELETE"];

use crate::config::{Route, ServerConfig};

/// Top-level entry: route the request within `server` and stamp the connection
/// disposition. Any error status (>= 400) forces `Connection: close`.
pub fn handle(req: &Request, server: &ServerConfig, keep_alive: bool) -> Response {
    let mut resp = route(req, server);
    resp.keep_alive = keep_alive && resp.status < 400;
    resp
}

fn route(req: &Request, server: &ServerConfig) -> Response {
    let route = match best_route(req, server) {
        Some(r) => r,
        None => return error_response(Some(server), 404),
    };

    // Redirects take effect before any filesystem work.
    if let Some((code, target)) = &route.redirect {
        let body = format!(
            "<!DOCTYPE html><html><body>Redirecting to <a href=\"{t}\">{t}</a></body></html>",
            t = target
        );
        let mut r = Response::with_body(*code, "text/html; charset=utf-8", body.into_bytes());
        r.set_header("Location", target);
        return r;
    }

    if !method_allowed(route, &req.method) {
        let mut r = error_response(Some(server), 405);
        r.set_header("Allow", &allowed_methods(route).join(", "));
        return r;
    }

    // CGI applies to whichever method targets a script with a configured
    // extension; it handles its own body, so it runs before static dispatch.
    if let Some(resp) = try_cgi(req, route, server) {
        return resp;
    }

    match req.method.as_str() {
        "GET" => serve_get(req, route, server),
        "POST" => handle_post(req, route, server),
        "DELETE" => handle_delete(req, route, server),
        _ => error_response(Some(server), 501),
    }
}

/// Longest-prefix route match, respecting path boundaries.
fn best_route<'a>(req: &Request, server: &'a ServerConfig) -> Option<&'a Route> {
    let mut best: Option<&Route> = None;
    for r in &server.routes {
        if route_matches(&r.path, &req.path) && best.map_or(true, |b| r.path.len() > b.path.len()) {
            best = Some(r);
        }
    }
    best
}

fn route_matches(route_path: &str, req_path: &str) -> bool {
    if route_path == "/" {
        return true;
    }
    req_path == route_path || req_path.starts_with(&format!("{route_path}/"))
}

fn allowed_methods(route: &Route) -> Vec<String> {
    if route.methods.is_empty() {
        IMPLEMENTED.iter().map(|s| s.to_string()).collect()
    } else {
        route.methods.clone()
    }
}

fn method_allowed(route: &Route, method: &str) -> bool {
    allowed_methods(route).iter().any(|m| m == method)
}

fn serve_get(req: &Request, route: &Route, server: &ServerConfig) -> Response {
    let root = match &route.root {
        Some(r) => r,
        None => return error_response(Some(server), 404),
    };

    let rel = relative_path(&route.path, &req.path);
    if has_traversal(&rel) {
        return error_response(Some(server), 403);
    }

    let fs_path = Path::new(root).join(&rel);
    let meta = match fs::metadata(&fs_path) {
        Ok(m) => m,
        Err(_) => return error_response(Some(server), 404),
    };

    if meta.is_dir() {
        // Try the configured default file first.
        if let Some(index) = &route.index {
            let idx = fs_path.join(index);
            if fs::metadata(&idx).map(|m| m.is_file()).unwrap_or(false) {
                return serve_file(&idx, server);
            }
        }
        if route.autoindex {
            return autoindex(&fs_path, &req.path, server);
        }
        return error_response(Some(server), 403);
    }

    serve_file(&fs_path, server)
}

fn serve_file(path: &Path, server: &ServerConfig) -> Response {
    match fs::read(path) {
        Ok(bytes) => Response::with_body(200, mime_for(&path.to_string_lossy()), bytes),
        Err(_) => error_response(Some(server), 500),
    }
}

/// Strip the route prefix off the request path to get a path relative to root.
fn relative_path(route_path: &str, req_path: &str) -> String {
    let rel = if route_path == "/" {
        req_path
    } else {
        &req_path[route_path.len().min(req_path.len())..]
    };
    rel.trim_start_matches('/').to_string()
}

/// Reject anything that could escape the configured root.
fn has_traversal(rel: &str) -> bool {
    Path::new(rel).components().any(|c| {
        matches!(
            c,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    })
}

fn autoindex(dir: &Path, url_path: &str, server: &ServerConfig) -> Response {
    let read = match fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return error_response(Some(server), 500),
    };

    let mut items: Vec<(String, bool)> = Vec::new();
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        items.push((name, is_dir));
    }
    items.sort();

    let base = if url_path.ends_with('/') {
        url_path.to_string()
    } else {
        format!("{url_path}/")
    };

    let mut html = format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><title>Index of {url_path}</title>\
         </head><body><h1>Index of {url_path}</h1><ul>"
    );
    if url_path != "/" {
        html.push_str("<li><a href=\"../\">../</a></li>");
    }
    for (name, is_dir) in items {
        let slash = if is_dir { "/" } else { "" };
        let enc = html_escape(&name);
        html.push_str(&format!(
            "<li><a href=\"{base}{enc}{slash}\">{enc}{slash}</a></li>"
        ));
    }
    html.push_str("</ul></body></html>");
    Response::with_body(200, "text/html; charset=utf-8", html.into_bytes())
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// ---------------------------------------------------------------------------
// CGI
// ---------------------------------------------------------------------------

/// If the target resolves to a script with a configured CGI extension, run it
/// and return its parsed response. Returns `None` when the route has no CGI or
/// the target is not a CGI script (so static handling proceeds).
fn try_cgi(req: &Request, route: &Route, server: &ServerConfig) -> Option<Response> {
    if route.cgi.is_empty() {
        return None;
    }
    let root = route.root.as_ref()?;
    let rel = relative_path(&route.path, &req.path);
    if has_traversal(&rel) {
        return Some(error_response(Some(server), 403));
    }

    let fs_path = Path::new(root).join(&rel);
    let ext = fs_path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{e}"))?;
    let interpreter = route.cgi.get(&ext)?; // not a CGI extension -> static

    // Resolve to an absolute path for PATH_INFO and execve; also confirms it
    // exists.
    let abs = match fs::canonicalize(&fs_path) {
        Ok(p) => p,
        Err(_) => return Some(error_response(Some(server), 404)),
    };

    match crate::cgi::execute(interpreter, &abs, req) {
        Ok(output) => Some(parse_cgi_output(&output)),
        Err(_) => Some(error_response(Some(server), 500)),
    }
}

/// Parse a CGI script's raw stdout (header block + body) into a [`Response`].
/// `Status:` sets the code; `Content-Length`/`Connection`/`Transfer-Encoding`
/// from the script are dropped since the responder sets them itself.
fn parse_cgi_output(raw: &[u8]) -> Response {
    let (header_bytes, body): (&[u8], &[u8]) = if let Some(i) = find(raw, b"\r\n\r\n") {
        (&raw[..i], &raw[i + 4..])
    } else if let Some(i) = find(raw, b"\n\n") {
        (&raw[..i], &raw[i + 2..])
    } else {
        (&[], raw) // no header separator: treat all output as the body
    };

    let header_text = String::from_utf8_lossy(header_bytes);
    let mut status = 200u16;
    let mut headers: Vec<(String, String)> = Vec::new();
    let mut has_content_type = false;

    for line in header_text.split('\n') {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            let (k, v) = (k.trim(), v.trim());
            if k.eq_ignore_ascii_case("status") {
                status = v
                    .split_whitespace()
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(200);
            } else if k.eq_ignore_ascii_case("content-length")
                || k.eq_ignore_ascii_case("connection")
                || k.eq_ignore_ascii_case("transfer-encoding")
            {
                // dropped: the responder manages framing itself
            } else {
                if k.eq_ignore_ascii_case("content-type") {
                    has_content_type = true;
                }
                headers.push((k.to_string(), v.to_string()));
            }
        }
    }

    let mut resp = Response::new(status);
    if !has_content_type {
        resp.set_header("Content-Type", "text/plain; charset=utf-8");
    }
    for (k, v) in headers {
        resp.set_header(&k, &v);
    }
    resp.body = body.to_vec();
    resp
}

// ---------------------------------------------------------------------------
// POST (upload) and DELETE
// ---------------------------------------------------------------------------

fn handle_post(req: &Request, route: &Route, server: &ServerConfig) -> Response {
    let root = match &route.root {
        Some(r) => r,
        None => return error_response(Some(server), 404),
    };
    let rel = relative_path(&route.path, &req.path);
    if has_traversal(&rel) {
        return error_response(Some(server), 403);
    }

    let content_type = req
        .headers
        .get("content-type")
        .map(String::as_str)
        .unwrap_or("");

    // multipart/form-data: save each file part under the route root.
    if let Some(boundary) = multipart_boundary(content_type) {
        return save_multipart(&req.body, boundary.as_bytes(), Path::new(root), server);
    }

    // Raw upload: the URL must name the destination file.
    if rel.is_empty() {
        return error_response(Some(server), 400);
    }
    let dest = Path::new(root).join(&rel);
    if let Some(parent) = dest.parent() {
        if fs::create_dir_all(parent).is_err() {
            return error_response(Some(server), 500);
        }
    }
    match fs::write(&dest, &req.body) {
        Ok(()) => {
            let body = format!("created {} ({} bytes)\n", req.path, req.body.len());
            let mut r = Response::with_body(201, "text/plain; charset=utf-8", body.into_bytes());
            r.set_header("Location", &req.path);
            r
        }
        Err(_) => error_response(Some(server), 500),
    }
}

fn handle_delete(req: &Request, route: &Route, server: &ServerConfig) -> Response {
    let root = match &route.root {
        Some(r) => r,
        None => return error_response(Some(server), 404),
    };
    let rel = relative_path(&route.path, &req.path);
    if has_traversal(&rel) || rel.is_empty() {
        return error_response(Some(server), 403);
    }
    let path = Path::new(root).join(&rel);
    match fs::metadata(&path) {
        Err(_) => error_response(Some(server), 404),
        Ok(m) if m.is_dir() => error_response(Some(server), 403),
        Ok(_) => match fs::remove_file(&path) {
            Ok(()) => Response::new(204), // No Content
            Err(_) => error_response(Some(server), 500),
        },
    }
}

/// Extract the `--boundary` delimiter from a `multipart/form-data` content-type.
fn multipart_boundary(content_type: &str) -> Option<String> {
    if !content_type
        .to_ascii_lowercase()
        .starts_with("multipart/form-data")
    {
        return None;
    }
    for part in content_type.split(';') {
        let p = part.trim();
        if p.to_ascii_lowercase().starts_with("boundary=") {
            let value = &p[p.find('=').unwrap() + 1..];
            return Some(format!("--{}", value.trim_matches('"')));
        }
    }
    None
}

/// Parse a multipart body and write every part that carries a filename to
/// `root`. Returns 201 with the saved names, or 400 if no file parts were found.
fn save_multipart(body: &[u8], delim: &[u8], root: &Path, server: &ServerConfig) -> Response {
    let positions = find_all(body, delim);
    let mut saved: Vec<String> = Vec::new();

    for w in positions.windows(2) {
        let mut seg = &body[w[0] + delim.len()..w[1]];
        if seg.starts_with(b"--") {
            continue; // closing delimiter, not a part
        }
        if seg.starts_with(b"\r\n") {
            seg = &seg[2..];
        }
        if seg.ends_with(b"\r\n") {
            seg = &seg[..seg.len() - 2];
        }
        let Some(h) = find(seg, b"\r\n\r\n") else {
            continue;
        };
        let (headers, content) = (&seg[..h], &seg[h + 4..]);
        if let Some(filename) = part_filename(headers) {
            let safe = sanitize_filename(&filename);
            if safe.is_empty() {
                continue;
            }
            if fs::write(root.join(&safe), content).is_ok() {
                saved.push(safe);
            } else {
                return error_response(Some(server), 500);
            }
        }
    }

    if saved.is_empty() {
        return error_response(Some(server), 400);
    }
    let body = format!("uploaded: {}\n", saved.join(", "));
    Response::with_body(201, "text/plain; charset=utf-8", body.into_bytes())
}

/// Find `filename="..."` in a part's headers (Content-Disposition).
fn part_filename(headers: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(headers);
    let lower = text.to_ascii_lowercase();
    let idx = lower.find("filename=")?;
    let rest = &text[idx + "filename=".len()..];
    let rest = rest.trim_start_matches('"');
    let end = rest.find('"').unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

/// Reduce an uploaded filename to a safe basename (no directory components).
fn sanitize_filename(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("").trim();
    if base == "." || base == ".." {
        String::new()
    } else {
        base.to_string()
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    (0..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}

fn find_all(hay: &[u8], needle: &[u8]) -> Vec<usize> {
    let mut out = Vec::new();
    if needle.is_empty() || hay.len() < needle.len() {
        return out;
    }
    let mut i = 0;
    while i + needle.len() <= hay.len() {
        if &hay[i..i + needle.len()] == needle {
            out.push(i);
            i += needle.len();
        } else {
            i += 1;
        }
    }
    out
}

/// Build an error response: serve the server's configured error page for this
/// code if it exists and is readable, otherwise a built-in default. Error
/// responses always close the connection.
pub fn error_response(server: Option<&ServerConfig>, code: u16) -> Response {
    if let Some(s) = server {
        if let Some(path) = s.error_pages.get(&code) {
            if let Ok(bytes) = fs::read(path) {
                let mut r = Response::with_body(code, mime_for(path), bytes);
                r.keep_alive = false;
                return r;
            }
        }
    }
    let mut r = Response::with_body(
        code,
        "text/html; charset=utf-8",
        default_error_html(code).into_bytes(),
    );
    r.keep_alive = false;
    r
}

fn default_error_html(code: u16) -> String {
    let phrase = reason_phrase(code);
    format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><title>{code} {phrase}</title></head>\
         <body><h1>{code} {phrase}</h1><hr><p>localhost</p></body></html>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn longest_prefix_wins() {
        assert!(route_matches("/", "/anything"));
        assert!(route_matches("/uploads", "/uploads"));
        assert!(route_matches("/uploads", "/uploads/file.txt"));
        assert!(!route_matches("/uploads", "/uploadsX"));
    }

    #[test]
    fn relative_path_strips_prefix() {
        assert_eq!(relative_path("/uploads", "/uploads/a/b.txt"), "a/b.txt");
        assert_eq!(relative_path("/", "/index.html"), "index.html");
    }

    #[test]
    fn traversal_is_rejected() {
        assert!(has_traversal("../etc/passwd"));
        assert!(has_traversal("a/../../b"));
        assert!(!has_traversal("a/b/c.txt"));
    }

    #[test]
    fn parses_multipart_boundary() {
        assert_eq!(
            multipart_boundary("multipart/form-data; boundary=XYZ"),
            Some("--XYZ".to_string())
        );
        assert_eq!(
            multipart_boundary("multipart/form-data; boundary=\"a b\""),
            Some("--a b".to_string())
        );
        assert_eq!(multipart_boundary("text/plain"), None);
    }

    #[test]
    fn extracts_part_filename() {
        let h = b"Content-Disposition: form-data; name=\"file\"; filename=\"hi.txt\"";
        assert_eq!(part_filename(h), Some("hi.txt".to_string()));
    }

    #[test]
    fn sanitizes_filenames() {
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_filename("a/b/c.txt"), "c.txt");
        assert_eq!(sanitize_filename(".."), "");
    }
}
