//! I/O multiplexing abstraction.
//!
//! The whole server is driven by a single call to [`Poller::poll`] per loop
//! tick. Every `accept`, `read`, and `write` is reached only from a readiness
//! event returned here. The concrete backend is chosen at compile time:
//! `epoll` on Linux (the audit target), `kqueue` on macOS (local dev).
//!
//! Both backends are used in **level-triggered** mode. That is deliberate: it
//! lets us do exactly one `read`/`write` syscall per event and rely on the
//! poller to re-fire while data remains, instead of looping until `EAGAIN`
//! (which would risk starving other clients).

use libc::c_int;

pub type RawFd = c_int;

/// What a file descriptor is currently waiting for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Interest {
    pub readable: bool,
    pub writable: bool,
}

impl Interest {
    pub const READABLE: Interest = Interest { readable: true, writable: false };
    pub const WRITABLE: Interest = Interest { readable: false, writable: true };
}

/// A readiness notification for a single fd produced by [`Poller::poll`].
#[derive(Clone, Copy, Debug)]
pub struct Event {
    pub fd: RawFd,
    pub readable: bool,
    pub writable: bool,
}

#[cfg(target_os = "linux")]
mod epoll;
#[cfg(target_os = "linux")]
pub use epoll::Poller;

#[cfg(target_os = "macos")]
mod kqueue;
#[cfg(target_os = "macos")]
pub use kqueue::Poller;
