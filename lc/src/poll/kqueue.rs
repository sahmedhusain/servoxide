//! macOS `kqueue` backend. Level-triggered (the kqueue default).
//!
//! kqueue tracks read and write readiness as two separate filters per fd, so
//! a single [`Interest`] maps onto two `EVFILT_READ` / `EVFILT_WRITE`
//! registrations. We enable the wanted filter and delete the unwanted one.

use std::io;
use std::ptr;
use std::time::Duration;

use libc::c_int;

use super::{Event, Interest, RawFd};

const MAX_EVENTS: usize = 1024;

pub struct Poller {
    kq: RawFd,
    raw: Vec<libc::kevent>,
}

fn make_kevent(fd: RawFd, filter: i16, flags: u16) -> libc::kevent {
    libc::kevent {
        ident: fd as usize,
        filter,
        flags,
        fflags: 0,
        data: 0,
        udata: ptr::null_mut(),
    }
}

impl Poller {
    pub fn new() -> io::Result<Self> {
        let kq = unsafe { libc::kqueue() };
        if kq < 0 {
            return Err(io::Error::last_os_error());
        }
        unsafe { libc::fcntl(kq, libc::F_SETFD, libc::FD_CLOEXEC) };
        Ok(Self {
            kq,
            raw: vec![make_kevent(0, 0, 0); MAX_EVENTS],
        })
    }

    /// Apply a single filter change. Deleting a filter that was never
    /// registered yields `ENOENT`, which is benign and ignored.
    fn change(&self, fd: RawFd, filter: i16, flags: u16) -> io::Result<()> {
        let kev = make_kevent(fd, filter, flags);
        let r = unsafe { libc::kevent(self.kq, &kev, 1, ptr::null_mut(), 0, ptr::null()) };
        if r < 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ENOENT) && (flags & libc::EV_DELETE != 0) {
                return Ok(());
            }
            return Err(err);
        }
        Ok(())
    }

    fn apply(&self, fd: RawFd, interest: Interest) -> io::Result<()> {
        let read_flags = if interest.readable {
            libc::EV_ADD | libc::EV_ENABLE
        } else {
            libc::EV_DELETE
        };
        let write_flags = if interest.writable {
            libc::EV_ADD | libc::EV_ENABLE
        } else {
            libc::EV_DELETE
        };
        self.change(fd, libc::EVFILT_READ, read_flags)?;
        self.change(fd, libc::EVFILT_WRITE, write_flags)?;
        Ok(())
    }

    pub fn add(&mut self, fd: RawFd, interest: Interest) -> io::Result<()> {
        self.apply(fd, interest)
    }

    pub fn modify(&mut self, fd: RawFd, interest: Interest) -> io::Result<()> {
        self.apply(fd, interest)
    }

    pub fn delete(&mut self, fd: RawFd) -> io::Result<()> {
        self.change(fd, libc::EVFILT_READ, libc::EV_DELETE)?;
        self.change(fd, libc::EVFILT_WRITE, libc::EV_DELETE)?;
        Ok(())
    }

    /// The single multiplexing call. Fills `events` with the ready fds.
    pub fn poll(&mut self, events: &mut Vec<Event>, timeout: Option<Duration>) -> io::Result<()> {
        let ts;
        let tsp = match timeout {
            Some(d) => {
                ts = libc::timespec {
                    tv_sec: d.as_secs() as libc::time_t,
                    tv_nsec: d.subsec_nanos() as libc::c_long,
                };
                &ts as *const libc::timespec
            }
            None => ptr::null(),
        };
        let n = unsafe {
            libc::kevent(
                self.kq,
                ptr::null(),
                0,
                self.raw.as_mut_ptr(),
                self.raw.len() as c_int,
                tsp,
            )
        };
        events.clear();
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                return Ok(());
            }
            return Err(err);
        }
        for kev in &self.raw[..n as usize] {
            // Each kevent carries exactly one filter. EV_EOF still surfaces as
            // a readable/writable event so the handler does the I/O and detects
            // the close via the syscall return.
            events.push(Event {
                fd: kev.ident as RawFd,
                readable: kev.filter == libc::EVFILT_READ,
                writable: kev.filter == libc::EVFILT_WRITE,
            });
        }
        Ok(())
    }
}

impl Drop for Poller {
    fn drop(&mut self) {
        unsafe { libc::close(self.kq) };
    }
}
