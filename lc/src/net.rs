use std::io;
use std::mem;

use crate::poll::RawFd;

/// Put a file descriptor into non-blocking mode. Every socket we touch must be
/// non-blocking so that `read`/`write`/`accept` return `EAGAIN` instead of
/// stalling the single-threaded event loop.
pub fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL, 0);
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Create a non-blocking TCP listener bound to `addr:port` and start listening.
/// `addr` is the four octets of an IPv4 address in order (e.g. `[127, 0, 0, 1]`).
pub fn bind_listener(addr: [u8; 4], port: u16) -> io::Result<RawFd> {
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }

        let one: libc::c_int = 1;
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_REUSEADDR,
            &one as *const _ as *const libc::c_void,
            mem::size_of::<libc::c_int>() as libc::socklen_t,
        );

        if let Err(e) = set_nonblocking(fd) {
            libc::close(fd);
            return Err(e);
        }

        let mut sin: libc::sockaddr_in = mem::zeroed();
        sin.sin_family = libc::AF_INET as libc::sa_family_t;
        sin.sin_port = port.to_be();
        // s_addr is stored in network byte order; from_ne_bytes lays the octets
        // out in memory exactly as given.
        sin.sin_addr.s_addr = u32::from_ne_bytes(addr);
        #[cfg(target_os = "macos")]
        {
            sin.sin_len = mem::size_of::<libc::sockaddr_in>() as u8;
        }

        let r = libc::bind(
            fd,
            &sin as *const _ as *const libc::sockaddr,
            mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        );
        if r < 0 {
            let e = io::Error::last_os_error();
            libc::close(fd);
            return Err(e);
        }

        if libc::listen(fd, 128) < 0 {
            let e = io::Error::last_os_error();
            libc::close(fd);
            return Err(e);
        }

        Ok(fd)
    }
}
