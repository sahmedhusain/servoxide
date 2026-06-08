//! The runtime server: turns a parsed [`Config`] into bound listeners and
//! drives them through the single event loop.
//!
//! Multiple `server` blocks may share one `host:port` (virtual hosting), so we
//! bind exactly one listener per unique address and, on each request, select
//! the matching `server` block by its `Host:` header (falling back to the
//! first block for that address — the default).

use std::collections::HashMap;
use std::io;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::cookies::SessionStore;
use crate::http::{self, BodyStatus, HeadStatus, Request};
use crate::net;
use crate::poll::{Event, Interest, Poller, RawFd};

/// A unique bind address and the `server` blocks that answer on it.
struct Address {
    host: [u8; 4],
    port: u16,
    label: String,
    /// Indices into `Config::servers`, in declaration order (first = default).
    servers: Vec<usize>,
}

/// Per-connection state. Grows in later milestones (real request, keep-alive,
/// timeouts). For now it accumulates the request and buffers the response.
struct Client {
    /// Which [`Address`] this connection arrived on.
    addr_idx: usize,
    inbuf: Vec<u8>,
    outbuf: Vec<u8>,
    written: usize,
    /// Whether the in-flight response keeps the connection alive.
    keep_alive: bool,
    /// Bytes of `inbuf` the in-flight request occupies, drained on completion.
    pending_len: usize,
    /// Last time this connection made I/O progress, for timeout sweeps.
    last_activity: Instant,
}

impl Client {
    fn new(addr_idx: usize) -> Self {
        Client {
            addr_idx,
            inbuf: Vec::new(),
            outbuf: Vec::new(),
            written: 0,
            keep_alive: false,
            pending_len: 0,
            last_activity: Instant::now(),
        }
    }
}

/// Idle keep-alive connections are closed after this long with no activity.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// A connection that has sent a partial request but not finished it within this
/// window gets a `408` and is closed.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub struct Server {
    config: Config,
    poller: Poller,
    addresses: Vec<Address>,
    /// listener fd -> index into `addresses`.
    listener_addr: HashMap<RawFd, usize>,
    clients: HashMap<RawFd, Client>,
    events: Vec<Event>,
    sessions: SessionStore,
}

impl Server {
    /// Build the set of unique addresses, bind a listener for each, and
    /// register them with the poller. A listener that fails to bind (e.g. the
    /// port is already in use) is logged and skipped so the others still come
    /// up; if none bind, this is a hard error.
    pub fn bind(config: Config) -> io::Result<Server> {
        let mut addresses: Vec<Address> = Vec::new();
        let mut index_of: HashMap<([u8; 4], u16), usize> = HashMap::new();

        for (si, s) in config.servers.iter().enumerate() {
            for &port in &s.ports {
                let key = (s.host, port);
                let idx = *index_of.entry(key).or_insert_with(|| {
                    let i = addresses.len();
                    let [a, b, c, d] = s.host;
                    addresses.push(Address {
                        host: s.host,
                        port,
                        label: format!("{a}.{b}.{c}.{d}:{port}"),
                        servers: Vec::new(),
                    });
                    i
                });
                addresses[idx].servers.push(si);
            }
        }

        let mut poller = Poller::new()?;
        let mut listener_addr = HashMap::new();

        for (ai, addr) in addresses.iter().enumerate() {
            match net::bind_listener(addr.host, addr.port) {
                Ok(fd) => {
                    if let Err(e) = poller.add(fd, Interest::READABLE) {
                        eprintln!("failed to register listener {}: {e}", addr.label);
                        unsafe { libc::close(fd) };
                        continue;
                    }
                    listener_addr.insert(fd, ai);
                    println!("listening on http://{}", addr.label);
                }
                Err(e) => eprintln!("failed to bind {}: {e}", addr.label),
            }
        }

        if listener_addr.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                "no listeners could be bound",
            ));
        }

        Ok(Server {
            config,
            poller,
            addresses,
            listener_addr,
            clients: HashMap::new(),
            events: Vec::with_capacity(1024),
            sessions: SessionStore::new(),
        })
    }

    /// The main loop: one `poll` call per tick drives every accept/read/write.
    pub fn run(&mut self) -> io::Result<()> {
        loop {
            if let Err(e) = self
                .poller
                .poll(&mut self.events, Some(Duration::from_millis(1000)))
            {
                eprintln!("poll error: {e}");
                continue;
            }

            // `events` is owned by self and not touched during dispatch, and
            // Event is Copy — so indexing copies each event out and leaves self
            // free to be mutated (poller/clients) inside the handlers.
            for i in 0..self.events.len() {
                let ev = self.events[i];
                let listener_idx = self.listener_addr.get(&ev.fd).copied();
                match listener_idx {
                    Some(ai) => self.accept_all(ev.fd, ai),
                    None if ev.readable => self.handle_read(ev.fd),
                    None if ev.writable => self.handle_write(ev.fd),
                    None => {}
                }
            }

            self.sweep_timeouts(Instant::now());
        }
    }

    /// Close connections that have gone idle, and 408 those that started a
    /// request but never finished it. Keeps the fd table from filling with
    /// abandoned or slow connections (audit: "no hanging connections").
    fn sweep_timeouts(&mut self, now: Instant) {
        let mut to_close = Vec::new();
        let mut to_timeout = Vec::new();
        for (&fd, c) in &self.clients {
            // A connection mid-write is making progress; leave it alone.
            if !c.outbuf.is_empty() {
                continue;
            }
            let idle = now.duration_since(c.last_activity);
            if !c.inbuf.is_empty() {
                if idle >= REQUEST_TIMEOUT {
                    to_timeout.push(fd);
                }
            } else if idle >= IDLE_TIMEOUT {
                to_close.push(fd);
            }
        }
        for fd in to_close {
            self.remove_client(fd);
        }
        for fd in to_timeout {
            self.respond_error(fd, 408, None);
        }
    }

    /// Accept every pending connection on `listener`, tagging each with the
    /// address it arrived on. Looping until `EAGAIN` is the standard listener
    /// pattern; the "one syscall per event" rule applies to per-client I/O.
    fn accept_all(&mut self, listener: RawFd, addr_idx: usize) {
        loop {
            let fd = unsafe { libc::accept(listener, std::ptr::null_mut(), std::ptr::null_mut()) };
            if fd < 0 {
                break; // EAGAIN/EWOULDBLOCK => drained; anything else => stop.
            }
            if net::set_nonblocking(fd).is_err() {
                unsafe { libc::close(fd) };
                continue;
            }
            if self.poller.add(fd, Interest::READABLE).is_err() {
                unsafe { libc::close(fd) };
                continue;
            }
            self.clients.insert(fd, Client::new(addr_idx));
        }
    }

    /// Exactly one `read` syscall per readable event.
    fn handle_read(&mut self, fd: RawFd) {
        let mut buf = [0u8; 4096];
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };

        if n == 0 {
            self.remove_client(fd); // peer closed
            return;
        }
        if n < 0 {
            // EAGAIN == EWOULDBLOCK: nothing available right now.
            if io::Error::last_os_error().raw_os_error() != Some(libc::EAGAIN) {
                self.remove_client(fd);
            }
            return;
        }

        // Append the freshly read bytes, then try to parse a complete request.
        match self.clients.get_mut(&fd) {
            Some(c) => {
                c.inbuf.extend_from_slice(&buf[..n as usize]);
                c.last_activity = Instant::now();
            }
            None => return,
        }
        self.try_dispatch(fd);
    }

    /// Attempt to parse a complete request from a client's buffer and respond.
    /// Returns without doing anything if more bytes are still needed. Each
    /// borrow of `self` is scoped so we never hold two at once.
    fn try_dispatch(&mut self, fd: RawFd) {
        let addr_idx = match self.clients.get(&fd) {
            Some(c) => c.addr_idx,
            None => return,
        };

        // 1. Parse the request line + headers.
        let head = {
            let c = self.clients.get(&fd).unwrap();
            match http::parse_head(&c.inbuf) {
                HeadStatus::NeedMore => return,
                HeadStatus::Bad(code) => return self.respond_error(fd, code, None),
                HeadStatus::Ok(h) => h,
            }
        };

        // 2. Pick the server block (by Host) and thus the body-size limit.
        let server_idx = self.select_server(addr_idx, head.host());
        let max_body = self.config.servers[server_idx].client_max_body_size;

        // 3. Determine and decode the body.
        let mode = match head.body_mode() {
            Ok(m) => m,
            Err(code) => return self.respond_error(fd, code, Some(server_idx)),
        };
        let (body, consumed) = {
            let c = self.clients.get(&fd).unwrap();
            let body_buf = &c.inbuf[head.head_len..];
            match http::decode_body(body_buf, &mode, max_body) {
                BodyStatus::NeedMore => return,
                BodyStatus::TooLarge => return self.respond_error(fd, 413, Some(server_idx)),
                BodyStatus::Bad(code) => return self.respond_error(fd, code, Some(server_idx)),
                BodyStatus::Complete { body, consumed } => (body, consumed),
            }
        };

        let keep_alive = http::wants_keep_alive(
            &head.version,
            head.headers.get("connection").map(String::as_str),
        );
        let request_len = head.head_len + consumed;

        let request = Request {
            method: head.method,
            path: head.path,
            query: head.query,
            headers: head.headers,
            body,
        };

        // 4. Route to a response.
        let mut response = http::handle(&request, &self.config.servers[server_idx], keep_alive);

        // 5. Session handling: ensure the client has a session, count the visit,
        // and issue a Set-Cookie for brand-new sessions. The visit count is
        // surfaced as a header so the session lifecycle is demoable.
        let session = self
            .sessions
            .touch(request.headers.get("cookie").map(String::as_str));
        if session.is_new {
            response.set_header("Set-Cookie", &SessionStore::set_cookie_header(&session.id));
        }
        response.set_header("X-Session-Visits", &session.visits.to_string());

        let resolved_keep_alive = response.keep_alive;
        let bytes = response.into_bytes();

        if let Some(c) = self.clients.get_mut(&fd) {
            c.keep_alive = resolved_keep_alive;
            c.pending_len = request_len;
        }
        self.queue_response(fd, bytes);
    }

    /// Exactly one `write` syscall per writable event.
    fn handle_write(&mut self, fd: RawFd) {
        let (ptr, len) = match self.clients.get(&fd) {
            Some(c) => {
                let remaining = &c.outbuf[c.written..];
                if remaining.is_empty() {
                    self.remove_client(fd);
                    return;
                }
                (remaining.as_ptr(), remaining.len())
            }
            None => return,
        };

        let n = unsafe { libc::write(fd, ptr as *const libc::c_void, len) };

        if n > 0 {
            let done = match self.clients.get_mut(&fd) {
                Some(c) => {
                    c.written += n as usize;
                    c.last_activity = Instant::now();
                    c.written >= c.outbuf.len()
                }
                None => return,
            };
            if done {
                self.finish_response(fd);
            }
        } else if n < 0 && io::Error::last_os_error().raw_os_error() != Some(libc::EAGAIN) {
            self.remove_client(fd);
        }
    }

    /// Called once a full response has been written. Closes the connection, or
    /// for keep-alive resets it for the next request — draining the bytes of
    /// the request just answered and immediately attempting to parse any
    /// pipelined request already in the buffer.
    fn finish_response(&mut self, fd: RawFd) {
        let (keep_alive, pending_len) = match self.clients.get(&fd) {
            Some(c) => (c.keep_alive, c.pending_len),
            None => return,
        };

        if !keep_alive {
            self.remove_client(fd);
            return;
        }

        if let Some(c) = self.clients.get_mut(&fd) {
            if pending_len <= c.inbuf.len() {
                c.inbuf.drain(..pending_len);
            } else {
                c.inbuf.clear();
            }
            c.outbuf.clear();
            c.written = 0;
            c.keep_alive = false;
            c.pending_len = 0;
        }

        if self.poller.modify(fd, Interest::READABLE).is_err() {
            self.remove_client(fd);
            return;
        }
        // A pipelined request may already be buffered; try to handle it now.
        self.try_dispatch(fd);
    }

    /// Pick the `server` block for an address: match `Host` against
    /// `server_name`, else fall back to the first (default) block.
    fn select_server(&self, addr_idx: usize, host_header: Option<&str>) -> usize {
        let addr = &self.addresses[addr_idx];
        if let Some(h) = host_header {
            let name = h.split(':').next().unwrap_or("").trim();
            for &si in &addr.servers {
                if self.config.servers[si]
                    .server_names
                    .iter()
                    .any(|n| n.eq_ignore_ascii_case(name))
                {
                    return si;
                }
            }
        }
        addr.servers[0]
    }

    /// Buffer a response and switch the connection to writing.
    fn queue_response(&mut self, fd: RawFd, response: Vec<u8>) {
        if let Some(c) = self.clients.get_mut(&fd) {
            c.outbuf = response;
            c.written = 0;
        }
        if self.poller.modify(fd, Interest::WRITABLE).is_err() {
            self.remove_client(fd);
        }
    }

    /// Send an error response, using the server's configured error page when a
    /// server block has been resolved. Error responses always close.
    fn respond_error(&mut self, fd: RawFd, code: u16, server_idx: Option<usize>) {
        let bytes = {
            let server = server_idx.map(|i| &self.config.servers[i]);
            http::error_response(server, code).into_bytes()
        };
        if let Some(c) = self.clients.get_mut(&fd) {
            c.keep_alive = false;
            c.pending_len = 0;
        }
        self.queue_response(fd, bytes);
    }

    /// Deregister, close, and drop a client so no fd or buffer ever leaks.
    fn remove_client(&mut self, fd: RawFd) {
        let _ = self.poller.delete(fd);
        unsafe { libc::close(fd) };
        self.clients.remove(&fd);
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        for (&fd, _) in &self.listener_addr {
            unsafe { libc::close(fd) };
        }
    }
}

