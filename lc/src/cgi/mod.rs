use std::ffi::CString;
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::http::Request;

/// Maximum wall-clock time a CGI child may run before it is killed.
const CGI_TIMEOUT: Duration = Duration::from_secs(10);

/// Run `interpreter script_abs`, passing `req`'s body on stdin and the standard
/// CGI variables in the environment. Returns the child's raw stdout (CGI
/// headers + body) for the router to parse, or an error if the child could not
/// be spawned / timed out.
pub fn execute(interpreter: &str, script_abs: &Path, req: &Request) -> io::Result<Vec<u8>> {
    let script_dir = script_abs
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| Path::new(".").to_path_buf());

    // Prepare everything that allocates *before* the fork, so the child only
    // performs async-signal-safe syscalls.
    let c_interp = cstring(interpreter)?;
    let c_script = cstring(&script_abs.to_string_lossy())?;
    let c_dir = cstring(&script_dir.to_string_lossy())?;

    let argv: Vec<*const libc::c_char> =
        vec![c_interp.as_ptr(), c_script.as_ptr(), std::ptr::null()];

    let env_strings = build_env(script_abs, req)?;
    let mut envp: Vec<*const libc::c_char> = env_strings.iter().map(|c| c.as_ptr()).collect();
    envp.push(std::ptr::null());

    // stdin: parent writes -> child reads. stdout: child writes -> parent reads.
    let stdin = Pipe::new()?;
    let stdout = Pipe::new()?;

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(io::Error::last_os_error());
    }

    if pid == 0 {
        // ---- child ----
        unsafe {
            libc::dup2(stdin.read, libc::STDIN_FILENO);
            libc::dup2(stdout.write, libc::STDOUT_FILENO);
            // Close every original pipe fd now that they're duplicated.
            libc::close(stdin.read);
            libc::close(stdin.write);
            libc::close(stdout.read);
            libc::close(stdout.write);

            if libc::chdir(c_dir.as_ptr()) != 0 {
                libc::_exit(127);
            }
            libc::execve(c_interp.as_ptr(), argv.as_ptr(), envp.as_ptr());
            // execve only returns on failure.
            libc::_exit(127);
        }
    }

    // ---- parent ----
    // Close the ends we don't use.
    unsafe {
        libc::close(stdin.read);
        libc::close(stdout.write);
    }

    // Feed the body, then signal EOF by closing the write end.
    write_all(stdin.write, &req.body);
    unsafe { libc::close(stdin.write) };

    let output = read_until_eof(stdout.read, pid, CGI_TIMEOUT);
    unsafe { libc::close(stdout.read) };

    // Reap the child (it should already be done or have been killed).
    let mut status = 0;
    unsafe { libc::waitpid(pid, &mut status, 0) };

    output
}

/// A self-closing pipe pair. Once a fd is handed to `dup2`/closed manually, the
/// struct's own fds are set to -1 conceptually via explicit closes above; we do
/// not rely on Drop for the fds that cross the fork boundary.
struct Pipe {
    read: libc::c_int,
    write: libc::c_int,
}

impl Pipe {
    fn new() -> io::Result<Pipe> {
        let mut fds = [0 as libc::c_int; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Pipe {
            read: fds[0],
            write: fds[1],
        })
    }
}

/// Write the whole buffer to `fd`, tolerating short writes and a child that has
/// already exited (EPIPE).
fn write_all(fd: libc::c_int, mut data: &[u8]) {
    while !data.is_empty() {
        let n = unsafe { libc::write(fd, data.as_ptr() as *const libc::c_void, data.len()) };
        if n > 0 {
            data = &data[n as usize..];
        } else {
            // EPIPE (child gone) or a hard error: stop writing.
            break;
        }
    }
}

/// Read `fd` until EOF using `poll` with a deadline. If the child overruns the
/// deadline it is SIGKILLed and whatever was read so far is returned as an error.
fn read_until_eof(fd: libc::c_int, pid: libc::pid_t, timeout: Duration) -> io::Result<Vec<u8>> {
    let deadline = Instant::now() + timeout;
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            unsafe { libc::kill(pid, libc::SIGKILL) };
            return Err(io::Error::new(io::ErrorKind::TimedOut, "CGI script timed out"));
        }

        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = remaining.as_millis().min(i32::MAX as u128) as libc::c_int;
        let r = unsafe { libc::poll(&mut pfd, 1, ms) };
        if r < 0 {
            if io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(io::Error::last_os_error());
        }
        if r == 0 {
            unsafe { libc::kill(pid, libc::SIGKILL) };
            return Err(io::Error::new(io::ErrorKind::TimedOut, "CGI script timed out"));
        }

        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n > 0 {
            out.extend_from_slice(&buf[..n as usize]);
        } else if n == 0 {
            return Ok(out); // EOF: child closed stdout
        } else if io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            return Err(io::Error::last_os_error());
        }
    }
}

/// Build the CGI/1.1 environment (`KEY=VALUE` C strings).
fn build_env(script_abs: &Path, req: &Request) -> io::Result<Vec<CString>> {
    let mut vars: Vec<(String, String)> = vec![
        ("GATEWAY_INTERFACE".into(), "CGI/1.1".into()),
        ("SERVER_PROTOCOL".into(), "HTTP/1.1".into()),
        ("SERVER_SOFTWARE".into(), "localhost".into()),
        ("REQUEST_METHOD".into(), req.method.clone()),
        ("QUERY_STRING".into(), req.query.clone()),
        ("SCRIPT_NAME".into(), req.path.clone()),
        ("PATH_INFO".into(), script_abs.to_string_lossy().into_owned()),
        (
            "PATH_TRANSLATED".into(),
            script_abs.to_string_lossy().into_owned(),
        ),
        // Some interpreters (php-cgi) require this to run in CGI mode.
        ("REDIRECT_STATUS".into(), "200".into()),
        ("CONTENT_LENGTH".into(), req.body.len().to_string()),
    ];
    if let Some(ct) = req.headers.get("content-type") {
        vars.push(("CONTENT_TYPE".into(), ct.clone()));
    }

    let mut out = Vec::with_capacity(vars.len());
    for (k, v) in vars {
        out.push(cstring(&format!("{k}={v}"))?);
    }
    Ok(out)
}

fn cstring(s: &str) -> io::Result<CString> {
    CString::new(s).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "interior NUL byte"))
}
