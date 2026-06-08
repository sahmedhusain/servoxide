//! Linux `epoll` backend. Level-triggered.

use std::io;
use std::time::Duration;

use libc::c_int;

use super::{Event, Interest, RawFd};

const MAX_EVENTS: usize = 1024;

pub struct Poller {
    epfd: RawFd,
    raw: Vec<libc::epoll_event>,
}

impl Poller {
    pub fn new() -> io::Result<Self> {
        let epfd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if epfd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            epfd,
            raw: vec![libc::epoll_event { events: 0, u64: 0 }; MAX_EVENTS],
        })
    }

    fn mask(interest: Interest) -> u32 {
        let mut m = 0u32;
        if interest.readable {
            m |= libc::EPOLLIN as u32;
        }
        if interest.writable {
            m |= libc::EPOLLOUT as u32;
        }
        m
    }

    fn ctl(&self, op: c_int, fd: RawFd, interest: Interest) -> io::Result<()> {
        let mut ev = libc::epoll_event {
            events: Self::mask(interest),
            u64: fd as u64,
        };
        let r = unsafe { libc::epoll_ctl(self.epfd, op, fd, &mut ev) };
        if r < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub fn add(&mut self, fd: RawFd, interest: Interest) -> io::Result<()> {
        self.ctl(libc::EPOLL_CTL_ADD, fd, interest)
    }

    pub fn modify(&mut self, fd: RawFd, interest: Interest) -> io::Result<()> {
        self.ctl(libc::EPOLL_CTL_MOD, fd, interest)
    }

    pub fn delete(&mut self, fd: RawFd) -> io::Result<()> {
        // The event pointer is ignored on modern kernels but must be non-null
        // on pre-2.6.9 ones; pass a dummy to be safe.
        let mut ev = libc::epoll_event { events: 0, u64: 0 };
        let r = unsafe { libc::epoll_ctl(self.epfd, libc::EPOLL_CTL_DEL, fd, &mut ev) };
        if r < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    /// The single multiplexing call. Fills `events` with the ready fds.
    pub fn poll(&mut self, events: &mut Vec<Event>, timeout: Option<Duration>) -> io::Result<()> {
        let ms = match timeout {
            Some(d) => d.as_millis().min(i32::MAX as u128) as c_int,
            None => -1,
        };
        let n = unsafe {
            libc::epoll_wait(self.epfd, self.raw.as_mut_ptr(), self.raw.len() as c_int, ms)
        };
        events.clear();
        if n < 0 {
            let err = io::Error::last_os_error();
            // Interrupted by a signal: not fatal, just retry next tick.
            if err.raw_os_error() == Some(libc::EINTR) {
                return Ok(());
            }
            return Err(err);
        }
        for slot in &self.raw[..n as usize] {
            let e = slot.events;
            // Treat HUP/ERR as readable so the handler performs the read,
            // observes EOF/error, and removes the client.
            let readable =
                e & (libc::EPOLLIN as u32 | libc::EPOLLHUP as u32 | libc::EPOLLERR as u32) != 0;
            let writable = e & (libc::EPOLLOUT as u32) != 0;
            events.push(Event {
                fd: slot.u64 as RawFd,
                readable,
                writable,
            });
        }
        Ok(())
    }
}

impl Drop for Poller {
    fn drop(&mut self) {
        unsafe { libc::close(self.epfd) };
    }
}
