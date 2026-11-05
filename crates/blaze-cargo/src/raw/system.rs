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
