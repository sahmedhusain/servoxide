# Getting Started — build, run, test & audit Localhost

This guide takes you from a fresh machine to a fully tested server.
It covers **both macOS and Linux**, every prerequisite, and a copy-paste command.

> The server is in the `lc/` directory. Unless stated otherwise, run all commands
> from inside `lc/`:
> ```bash
> cd lc
> ```

---

## Table of contents

1. [Prerequisites & packages](#1-prerequisites--packages)
2. [Platform notes (macOS vs Linux)](#2-platform-notes-macos-vs-linux)
3. [The Python (CGI) binary path](#3-the-python-cgi-binary-path)
4. [Build](#4-build)
5. [Run](#5-run)
6. [Automated tests](#6-automated-tests)
7. [Manual tests — every audit case by hand](#7-manual-tests--every-audit-case-by-hand)
8. [Stress test + leak monitoring](#8-stress-test--leak-monitoring)
9. [Browser testing](#9-browser-testing)
10. [Configuration-error tests](#10-configuration-error-tests)
11. [Troubleshooting](#12-troubleshooting)

---

## 1. Prerequisites & packages

You need: **Rust toolchain**, **curl**, **netcat (nc)**, **python3** (for CGI),
**siege** (stress test), and a way to watch fds/memory (**lsof**, optionally
**watch**).

### Install Rust (both platforms)

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
# then restart the shell, or:
source "$HOME/.cargo/env"
rustc --version    # confirm it works
```

### macOS packages (Homebrew)

```bash
# Homebrew itself, if you don't have it:
/bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"

brew install siege          # stress tester
brew install watch          # optional: live monitor (macOS has no watch by default)
# curl, nc, python3, lsof ship with macOS already
```

### Linux packages (Debian/Ubuntu)

```bash
sudo apt update
sudo apt install -y build-essential curl netcat-openbsd siege python3 lsof procps
# 'watch' is part of procps; 'siege' is in the default repos
```

### Linux packages (Fedora/RHEL)

```bash
sudo dnf install -y gcc curl nmap-ncat siege python3 lsof procps-ng
```

### Verify everything is present

```bash
for tool in cargo curl nc python3 siege lsof; do
  printf "%-8s " "$tool"; command -v "$tool" || echo "MISSING"
done
```

---

## 2. Platform notes (macOS vs Linux)

The server picks its I/O multiplexer **at compile time** — you don't configure
anything:

| OS | Multiplexer | Selected by |
|---|---|---|
| Linux | `epoll` | `#[cfg(target_os = "linux")]` |
| macOS | `kqueue` | `#[cfg(target_os = "macos")]` |

So the exact same source builds and runs on both. **The audit is graded on
Linux**, so if you develop on macOS, do at least one full
`cargo build && ./tests/run_tests.sh` on a Linux box before the audit.

---

## 3. The Python (CGI) binary path

CGI runs whatever interpreter the config points to. The default config uses:

```nginx
cgi .py /usr/bin/python3;
```

**Find your actual path:**

```bash
which python3
```

- **Linux:** almost always `/usr/bin/python3` ✅ (matches the default)
- **macOS (system):** `/usr/bin/python3` ✅
- **macOS (Homebrew):** may be `/opt/homebrew/bin/python3` (Apple Silicon) or
  `/usr/local/bin/python3` (Intel)

If `which python3` prints something **other than** `/usr/bin/python3`, edit the
CGI line in `config/default.conf` to match:

```bash
# example: point CGI at a Homebrew python
#   route /cgi { ... cgi .py /opt/homebrew/bin/python3; }
```

---

## 4. Build

```bash
cd lc

# development build (faster compile, what the test scripts use)
cargo build

# release build (optimized, what you run for siege)
cargo build --release

# run the unit tests while you're at it
cargo test
```

Binaries land at:
- `target/debug/localhost`
- `target/release/localhost`

---

## 5. Run

```bash
# with the default config
./target/release/localhost config/default.conf

# config path is optional — it defaults to config/default.conf
./target/release/localhost

# with your own config
./target/release/localhost /path/to/your.conf
```

On start it prints the addresses it bound, e.g.:

```
listening on http://127.0.0.1:8080
```

Stop it with `Ctrl-C`, or from another terminal:

```bash
pkill -f target/release/localhost
```

The default config serves:

| URL | What |
|---|---|
| `http://127.0.0.1:8080/` | static site (`www/index.html`) |
| `/listing/` | directory listing (autoindex) |
| `/uploads/` | GET/POST/DELETE file area |
| `/cgi/hello.py` | Python CGI |
| `/old` | 301 redirect |
| `Host: example.com` | second virtual host |

---

## 6. Automated tests

```bash
cd lc

# 1) unit tests (parser, HTTP request, router, sessions)
cargo test

# 2) full end-to-end functional suite (starts the server itself, 27 checks)
./tests/run_tests.sh

# 3) stress / availability (needs siege)
./tests/stress.sh                       # defaults: 50 clients, 30s, /
./tests/stress.sh http://127.0.0.1:8080/ 100 60S   # custom
```

`run_tests.sh` prints `PASS`/`FAIL` per case and a final tally. It also runs the
configuration-error fixtures in `tests/configs/`.

---

## 7. Manual tests — every audit case by hand

Start the server first (in one terminal):

```bash
./target/release/localhost config/default.conf
```

Then run these in another terminal. Each maps to an audit question.

### Methods & status codes

```bash
# GET 200
curl -i http://127.0.0.1:8080/

# 404 (and it serves the custom error page)
curl -i http://127.0.0.1:8080/does-not-exist

# 405 Method Not Allowed + Allow header (DELETE on a GET-only route)
curl -i -X DELETE http://127.0.0.1:8080/

# wrong/unknown method
curl -i -X PATCH http://127.0.0.1:8080/
```

### Bad / malformed request (server must stay up)

```bash
printf 'GET /\r\n\r\n' | nc -w1 127.0.0.1 8080      # → 400 Bad Request
curl -i http://127.0.0.1:8080/                       # still works afterwards
```

### Body size limit (413)

```bash
# small body is accepted
curl -i -X POST -H "Content-Type: text/plain" --data "small" \
  http://127.0.0.1:8080/uploads/small.txt

# 2 MB body exceeds client_max_body_size (1m) → 413
head -c 2000000 /dev/zero | curl -i -X POST --data-binary @- \
  http://127.0.0.1:8080/uploads/big
```

### Redirect

```bash
curl -i http://127.0.0.1:8080/old        # → 301 + Location
curl -iL http://127.0.0.1:8080/old        # follow it
```

### Static files, directory default file, autoindex

```bash
curl -i http://127.0.0.1:8080/            # serves index.html (directory default)
curl -s http://127.0.0.1:8080/listing/    # autoindex listing of a directory
```

### Upload → download (integrity) → delete

```bash
# raw upload
curl -i -X POST --data "round-trip data" http://127.0.0.1:8080/uploads/note.txt
# download it back
curl -s http://127.0.0.1:8080/uploads/note.txt
# multipart/form-data upload (-F)
echo "file body" > /tmp/up.txt
curl -i -F "file=@/tmp/up.txt" http://127.0.0.1:8080/uploads
# DELETE it → 204, then GET → 404
curl -i -X DELETE http://127.0.0.1:8080/uploads/note.txt
curl -i http://127.0.0.1:8080/uploads/note.txt

# binary integrity check (must be IDENTICAL)
head -c 100000 /dev/urandom > /tmp/blob.bin
curl -s -X POST --data-binary @/tmp/blob.bin http://127.0.0.1:8080/uploads/blob.bin
curl -s http://127.0.0.1:8080/uploads/blob.bin -o /tmp/blob.out
cmp /tmp/blob.bin /tmp/blob.out && echo "IDENTICAL" || echo "CORRUPTED"
```

### Chunked transfer encoding

```bash
printf "chunked-payload" | curl -i -X POST -H "Transfer-Encoding: chunked" \
  --data-binary @- http://127.0.0.1:8080/cgi/hello.py
```

### CGI (chunked & unchunked)

```bash
# GET with query string
curl -s "http://127.0.0.1:8080/cgi/hello.py?name=sayed&x=1"
# POST unchunked
curl -s -X POST --data "unchunked-body" http://127.0.0.1:8080/cgi/hello.py
# POST chunked
printf "chunked-body" | curl -s -X POST -H "Transfer-Encoding: chunked" \
  --data-binary @- http://127.0.0.1:8080/cgi/hello.py
```

### Cookies & sessions

```bash
# first visit: Set-Cookie + X-Session-Visits: 1
curl -i -c /tmp/jar http://127.0.0.1:8080/ | grep -iE "Set-Cookie|X-Session-Visits"
# return visits with the cookie: counter increments
curl -i -b /tmp/jar http://127.0.0.1:8080/ | grep -i "X-Session-Visits"
curl -i -b /tmp/jar http://127.0.0.1:8080/ | grep -i "X-Session-Visits"
```

### Virtual hosts (same IP:port, different hostnames)

```bash
# the audit's exact example: --resolve
curl -s --resolve example.com:8080:127.0.0.1 http://example.com:8080/
# equivalently, with a Host header
curl -s -H "Host: example.com" http://127.0.0.1:8080/
# unknown host → falls back to the default (first) server
curl -s -H "Host: nope.invalid" http://127.0.0.1:8080/
```

### Path-traversal safety

```bash
curl -i --path-as-is "http://127.0.0.1:8080/../../etc/passwd"   # → 403
```

---

## 8. Stress test + leak monitoring

The headline requirement: **availability ≥ 99.5%** under `siege -b`.

**Terminal 1 — server (use the release build):**

```bash
./target/release/localhost config/default.conf
```

**Terminal 2 — siege:**

```bash
# the audit command
siege -b http://127.0.0.1:8080/

# bounded version (recommended): 50 clients for 30 seconds
siege -b -c 50 -t 30S http://127.0.0.1:8080/
```

Look for `Availability: 100.00 %` and `Failed transactions: 0`.

**Terminal 3 — watch for fd / memory leaks while siege runs:**

macOS (no `watch` by default — use a shell loop):

```bash
while true; do
  PID=$(pgrep -f target/release/localhost)
  clear; echo "fds=$(lsof -p $PID | wc -l)  rss=$(ps -o rss= -p $PID)KB"
  sleep 1
done
```

Linux (or macOS after `brew install watch`):

```bash
watch -n1 'PID=$(pgrep -f target/release/localhost); echo fds=$(ls /proc/$PID/fd | wc -l) rss=$(ps -o rss= -p $PID)KB'
```

**Pass criteria:** fd count and RSS rise slightly, then **stay flat** — they must
not climb forever (that would be a leak). They should also drop back after siege
stops.

---

## 9. Browser testing

The audit explicitly says to use a real browser + DevTools. Do this pass yourself:

1. Start the server, open `http://127.0.0.1:8080/` in Chrome/Firefox.
2. Open DevTools → **Network** tab, reload.
3. Click the request and check **Response Headers**: `Content-Type`,
   `Content-Length`, `Connection`, `Set-Cookie` on the first load.
4. Visit a bad URL (`/nope`) → custom 404 page renders.
5. Visit `/listing/` → directory listing renders, links work.
6. Visit `/old` → browser follows the 301 redirect.
7. DevTools → **Application → Cookies** → see `session_id`; reload and watch
   `X-Session-Visits` increase in the Network tab.

---

## 10. Configuration-error tests

These prove the server detects misconfigurations and degrades gracefully.

```bash
cd lc

# Duplicate host:port:server_name → detected, reported, that server dropped
./target/debug/localhost tests/configs/duplicate_port.conf
# (prints "duplicate binding ..." then serves the surviving servers)

# One broken server + one valid: bad one is logged, the valid one still serves
./target/debug/localhost tests/configs/partially_broken.conf &
sleep 1
curl -i http://127.0.0.1:8091/        # the good server (port 8091) responds
pkill -f target/debug/localhost
```

**Why does the server still work if one config is broken?** Each `server` block
is validated independently; an invalid one is logged and skipped, and the loop
runs with whatever valid servers remain. One typo doesn't take everything down.

---

## 11. Troubleshooting

| Symptom | Cause / fix |
|---|---|
| `Address already in use` / server exits at start | An old instance is still bound. `pkill -f target/.*/localhost`, then retry. |
| `curl` returns `000` / empty | Server isn't running (likely failed to bind). Check its terminal output. |
| CGI returns 500 or 404 | Wrong python path in config — run `which python3` and fix the `cgi` line. |
| Edited code but behavior unchanged | `cargo test` doesn't rebuild the binary — run `cargo build` (or `--release`) before running. |
| `siege: command not found` | `brew install siege` (macOS) / `sudo apt install siege` (Linux). |
| `watch: command not found` (macOS) | Use the shell `while` loop in §8, or `brew install watch`. |
| Build error mentioning epoll/kqueue | You're cross-compiling for the wrong OS; build natively on the target. |
| Uploads fail with 404/500 | The `www/uploads/` directory must exist and be writable. |

---

That's everything needed to build, run, exercise, and audit the server on either
platform. For the architecture and design rationale, see
[README.md](README.md).
