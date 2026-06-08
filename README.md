# Localhost

A single-process, single-threaded HTTP/1.1 server written in Rust, using only
the `libc` crate for syscalls. All I/O is non-blocking and multiplexed through
a single poller call per loop tick (epoll on Linux, kqueue on macOS).

## Build & run

```bash
cd lc
cargo build --release
./target/release/localhost config/default.conf   # defaults to config/default.conf
```

## Test

```bash
cd lc
cargo test            # unit tests (parser, request, router, sessions)
./tests/run_tests.sh  # end-to-end functional suite (27 checks)
./tests/stress.sh     # siege availability test (needs: brew install siege)
```

## Features

- HTTP/1.1: GET, POST, DELETE; keep-alive; correct status codes
- Single `epoll`/`kqueue` call per tick; one `read`/`write` syscall per event;
  non-blocking sockets; every return value checked; clients removed on error
- Config file: multiple servers/ports, virtual hosts (by `Host`), per-route
  methods, roots, default index, autoindex, redirects, CGI, custom error pages,
  `client_max_body_size`; duplicate-port detection; graceful degradation
- Request parsing with `Content-Length` and chunked transfer decoding; `413`
  enforcement before buffering the body
- Static file serving with MIME types, directory listings, path-traversal
  protection
- File uploads (raw + `multipart/form-data`) and deletion
- CGI (Python) with chunked and unchunked bodies; `fork`/`execve`, env vars,
  `chdir` to the script directory, `waitpid` with kill-on-timeout
- Cookies and in-memory sessions with TTL eviction
- Custom + built-in error pages for 400/403/404/405/408/413/500
- Connection timeouts (idle close + `408` for stalled requests)

## Layout

```
lc/src/
  main.rs            entry: parse config, bind, run
  poll/              poller abstraction (epoll.rs / kqueue.rs)
  net.rs             non-blocking socket + listener helpers
  config/            grammar tokenizer + recursive-descent parser
  http/              request.rs, response.rs, router.rs, mime.rs
  server/            the event loop, client state, vhost selection
  cgi/               fork/exec CGI runner
  cookies/           session store
```

## Verified

- siege `-b -c 50 -t 20S`: **100% availability**, 0 failed of ~495k transactions
- No fd leak (fd count flat under sustained load) and stable RSS (~2 MB)
