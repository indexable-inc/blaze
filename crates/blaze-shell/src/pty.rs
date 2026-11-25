//! Pseudo-terminal child process management on macOS: `forkpty`, non-blocking reads, writes,
//! window resize (`TIOCSWINSZ`), non-blocking `waitpid` exit status, and clean FD teardown.
//! Calls `libSystem` directly with zero `libc` dependency.

use std::ffi::{CString, c_char, c_int, c_void};

#[repr(C)]
struct Winsize {
    ws_row: u16,
    ws_col: u16,
    ws_xpixel: u16,
    ws_ypixel: u16,
}

#[repr(C)]
struct Passwd {
    pw_name: *mut c_char,
    pw_passwd: *mut c_char,
    pw_uid: u32,
    pw_gid: u32,
    pw_change: i64,
    pw_class: *mut c_char,
    pw_gecos: *mut c_char,
    pw_dir: *mut c_char,
    pw_shell: *mut c_char,
    pw_expire: i64,
}

unsafe extern "C" {
    fn forkpty(
        master: *mut c_int,
        name: *mut c_char,
        termp: *const c_void,
        winp: *const Winsize,
    ) -> c_int;
    fn execvp(file: *const c_char, argv: *const *const c_char) -> c_int;
    fn chdir(path: *const c_char) -> c_int;
    fn _exit(code: c_int) -> !;
    fn read(fd: c_int, buf: *mut c_void, n: usize) -> isize;
    fn write(fd: c_int, buf: *const c_void, n: usize) -> isize;
    fn close(fd: c_int) -> c_int;
    fn waitpid(pid: c_int, status: *mut c_int, options: c_int) -> c_int;
    fn kill(pid: c_int, sig: c_int) -> c_int;
    fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
    fn ioctl(fd: c_int, req: u64, ...) -> c_int;
    fn __error() -> *mut c_int;
    fn getpwuid(uid: u32) -> *const Passwd;
    fn getuid() -> u32;
}

const F_GETFL: c_int = 3;
const F_SETFL: c_int = 4;
const O_NONBLOCK: c_int = 4;
const TIOCSWINSZ: u64 = 0x8008_7467;
const EAGAIN: c_int = 35;
const EINTR: c_int = 4;
const WNOHANG: c_int = 1;
const SIGHUP: c_int = 1;

/// The account's login shell from `passwd` (as Terminal.app and ghostty read it).
pub fn login_shell() -> Option<String> {
    // SAFETY: `getpwuid` returns a pointer to static storage or null; copied out immediately.
    unsafe {
        let pw = getpwuid(getuid());
        if pw.is_null() || (*pw).pw_shell.is_null() {
            return None;
        }
        std::ffi::CStr::from_ptr((*pw).pw_shell)
            .to_str()
            .ok()
            .map(str::to_owned)
    }
}

/// Non-blocking PTY read result.
pub enum Read {
    Bytes(usize),
    Nothing,
    Closed,
}

/// Owning handle to a child process running on a macOS pseudo-terminal.
pub struct Pty {
    fd: c_int,
    pid: c_int,
}

impl Pty {
    /// Start `argv[0]` (looked up on `PATH`) in `dir` on a new PTY of `cols` x `rows`.
    pub fn spawn(
        argv: &[&str],
        dir: &str,
        cols: u16,
        rows: u16,
        px: (u16, u16),
    ) -> Result<Pty, String> {
        if argv.is_empty() {
            return Err("empty argv".to_owned());
        }
        let args: Vec<CString> = argv
            .iter()
            .map(|a| CString::new(*a).map_err(|e| e.to_string()))
            .collect::<Result<_, _>>()?;
        let mut ptrs: Vec<*const c_char> = args.iter().map(|a| a.as_ptr()).collect();
        ptrs.push(std::ptr::null());
        let dir = CString::new(dir).map_err(|e| e.to_string())?;
        let ws = Winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: px.0,
            ws_ypixel: px.1,
        };
        let mut fd: c_int = -1;
        // SAFETY: `fd` and `ws` outlive `forkpty`; in the child branch only async-signal-safe
        // calls (`chdir`, `execvp`, `_exit`) touch buffers prepared before the fork.
        let pid = unsafe { forkpty(&mut fd, std::ptr::null_mut(), std::ptr::null(), &ws) };
        if pid < 0 {
            return Err("forkpty failed".to_owned());
        }
        if pid == 0 {
            // SAFETY: child process; `dir` and `ptrs` are NUL-terminated arrays prepared above.
            unsafe {
                chdir(dir.as_ptr());
                execvp(ptrs[0], ptrs.as_ptr());
                _exit(127);
            }
        }
        // SAFETY: `fd` is a valid master PTY descriptor returned by `forkpty`.
        unsafe {
            let fl = fcntl(fd, F_GETFL);
            if fl >= 0 {
                fcntl(fd, F_SETFL, fl | O_NONBLOCK);
            }
        }
        Ok(Pty { fd, pid })
    }

    /// OS process ID of the spawned child process.
    #[inline]
    pub fn pid(&self) -> u32 {
        self.pid.max(0) as u32
    }

    pub fn read(&self, buf: &mut [u8]) -> Read {
        // SAFETY: `buf` is writable for `buf.len()` bytes and `self.fd` is an open PTY fd.
        let n = unsafe { read(self.fd, buf.as_mut_ptr().cast(), buf.len()) };
        if n > 0 {
            return Read::Bytes(n as usize);
        }
        if n == 0 {
            return Read::Closed;
        }
        // SAFETY: `__error` returns a thread-local pointer to `errno`.
        let e = unsafe { *__error() };
        if e == EAGAIN || e == EINTR { Read::Nothing } else { Read::Closed }
    }

    pub fn write_all(&self, mut bytes: &[u8]) {
        let mut spins = 0usize;
        while !bytes.is_empty() {
            // SAFETY: `bytes` is readable for `bytes.len()` bytes and `self.fd` is open.
            let n = unsafe { write(self.fd, bytes.as_ptr().cast(), bytes.len()) };
            if n > 0 {
                bytes = &bytes[n as usize..];
                continue;
            }
            spins += 1;
            if spins > 10_000 {
                return;
            }
            std::thread::yield_now();
        }
    }

    pub fn resize(&self, cols: u16, rows: u16, px: (u16, u16)) {
        let ws = Winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: px.0,
            ws_ypixel: px.1,
        };
        // SAFETY: `TIOCSWINSZ` reads one `Winsize` struct from `&ws`.
        unsafe { ioctl(self.fd, TIOCSWINSZ, &ws as *const Winsize) };
    }

    /// Non-blocking check for whether the child process has exited, returning its exit code.
    pub fn try_wait(&self) -> Option<i32> {
        let mut status: c_int = 0;
        // SAFETY: `status` is a valid stack local; `WNOHANG` returns immediately if running.
        let rc = unsafe { waitpid(self.pid, &mut status, WNOHANG) };
        if rc == self.pid {
            let sig = status & 0x7f;
            if sig == 0 {
                Some((status >> 8) & 0xff)
            } else {
                Some(128 + sig)
            }
        } else {
            None
        }
    }

    /// Send `SIGHUP` to the child process (used when closing the window with a live PTY).
    pub fn hangup(&self) {
        if self.pid > 0 {
            // SAFETY: `self.pid` is a child PID spawned by `forkpty`.
            unsafe {
                kill(self.pid, SIGHUP);
            }
        }
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        if self.fd >= 0 {
            // SAFETY: `self.fd` is exclusively owned by this `Pty` and closed once on drop.
            unsafe {
                close(self.fd);
            }
            self.fd = -1;
        }
        let _ = self.try_wait();
    }
}
