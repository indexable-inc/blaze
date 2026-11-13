//! What the system says of the terminal, the clock and the user's directories, with no call
//! of std's that asks it.

/// The columns of the terminal standard error is, None when it is none.
#[cfg(unix)]
pub fn stderr_columns() -> Option<u16> {
    let mut size = libc::winsize { ws_row: 0, ws_col: 0, ws_xpixel: 0, ws_ypixel: 0 };
    // SAFETY: `TIOCGWINSZ` writes one `winsize` through its third argument, which `&raw mut size`
    // is for the call; on failure it writes nothing and `size` stays zeros.
    let got = unsafe { libc::ioctl(libc::STDERR_FILENO, libc::TIOCGWINSZ, &raw mut size) };
    (got == 0 && size.ws_col > 0).then_some(size.ws_col)
}

/// The columns and rows of the terminal standard error is, None when it is none.
#[cfg(unix)]
pub fn stderr_size() -> Option<(u16, u16)> {
    let mut size = libc::winsize { ws_row: 0, ws_col: 0, ws_xpixel: 0, ws_ypixel: 0 };
    // SAFETY: `TIOCGWINSZ` writes one `winsize` through its third argument, which `&raw mut size`
    // is for the call; on failure it writes nothing and `size` stays zeros.
    let got = unsafe { libc::ioctl(libc::STDERR_FILENO, libc::TIOCGWINSZ, &raw mut size) };
    (got == 0 && size.ws_col > 0 && size.ws_row > 0).then_some((size.ws_col, size.ws_row))
}

/// The terminal standard error is stops echoing the keys typed into it; what puts it back is
/// returned, None when stderr is no terminal or its settings can't be read. Its line
/// discipline is otherwise left as it is: Ctrl-C still interrupts, and what is typed still
/// reaches whoever reads the terminal next.
#[cfg(unix)]
pub fn stderr_echo_off() -> Option<libc::termios> {
    // SAFETY: `termios` is plain integers and arrays of them, for which zeroes are a value.
    let mut now: libc::termios = unsafe { std::mem::zeroed() };
    // SAFETY: `tcgetattr` writes one `termios` through its second argument, which `&raw mut
    // now` is for the call; on failure it writes nothing.
    if unsafe { libc::tcgetattr(libc::STDERR_FILENO, &raw mut now) } != 0 {
        return None;
    }
    let saved = now;
    now.c_lflag &= !libc::ECHO;
    // SAFETY: `tcsetattr` reads one `termios` through its third argument, live for the call.
    (unsafe { libc::tcsetattr(libc::STDERR_FILENO, libc::TCSANOW, &raw const now) } == 0)
        .then_some(saved)
}

/// The terminal standard error is as `saved` has it ([`stderr_echo_off`]'s).
#[cfg(unix)]
pub fn stderr_restore(saved: &libc::termios) {
    // SAFETY: `tcsetattr` reads one `termios` through its third argument, live for the call;
    // a failure leaves the terminal as it is, and there is no one to tell.
    unsafe { libc::tcsetattr(libc::STDERR_FILENO, libc::TCSANOW, std::ptr::from_ref(saved)) };
}

/// The step of the coarse real-time clock, which the kernel stamps the files it writes with.
#[cfg(target_os = "linux")]
pub fn coarse_clock_step() -> Option<std::time::Duration> {
    let mut tick = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `tick` is a live, writable `timespec`, all `clock_getres` writes; a clock the
    // kernel does not know is refused.
    match unsafe { libc::clock_getres(libc::CLOCK_REALTIME_COARSE, &raw mut tick) } {
        0 => Some(std::time::Duration::new(tick.tv_sec as u64, tick.tv_nsec as u32)),
        _ => None,
    }
}

/// The per-user cache directory of macOS, as the system names it.
#[cfg(target_os = "macos")]
pub fn user_cache_dir() -> Option<std::ffi::OsString> {
    use std::os::unix::ffi::OsStrExt as _;
    let mut buffer = [0u8; 1024];
    // SAFETY: `confstr` writes at most the length it is given, `buffer.len()`, to the pointer,
    // and `buffer` is writable for all of it; a length past the buffer's would have it write
    // past its end. What it says is bounds-checked against the buffer below.
    let len = unsafe {
        libc::confstr(libc::_CS_DARWIN_USER_CACHE_DIR, buffer.as_mut_ptr().cast(), buffer.len())
    };
    if len == 0 || len > buffer.len() {
        return None;
    }
    Some(std::ffi::OsStr::from_bytes(&buffer[..len - 1]).to_owned())
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    #[test]
    fn the_coarse_clock_steps() {
        assert!(super::coarse_clock_step().is_some_and(|it| !it.is_zero()));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_user_has_a_cache_dir() {
        assert!(super::user_cache_dir().is_some_and(|it| !it.is_empty()));
    }

    #[cfg(unix)]
    #[test]
    fn columns_are_some_or_none_never_zero() {
        assert_ne!(super::stderr_columns(), Some(0));
    }
}
