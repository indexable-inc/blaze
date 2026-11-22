//! The system's temporary directory from values its caller read: the environment-free form of
//! [`super::base`]. A caller with a world reads `TMPDIR` from its environment capability and
//! passes it here with the platform's own directory ([`platform`], a fact of the system, not of
//! the environment), so nothing here reads the process's environment.
//!
//! What `std::env::temp_dir` answers on unix (the pinned library's `sys/paths/unix.rs`):
//! "`TMPDIR` if it is set, else `darwin_temp_dir()` on Apple targets (`confstr` of
//! `_CS_DARWIN_USER_TEMP_DIR`), `/data/local/tmp` on Android, `/tmp` elsewhere". One change:
//! a `TMPDIR` that is set and empty is taken as not set, where std answers the empty path (a
//! path relative to the working directory, so a "temporary" directory would be made wherever
//! the process stands).

use super::{Path, PathBuf};

/// The temporary directory: `tmpdir` (the value of `TMPDIR`, as a path) when it is given and
/// not empty, else `platform` (the system's own, [`platform`] for this host).
pub fn base_in(tmpdir: Option<&Path>, platform: &Path) -> PathBuf {
    match tmpdir {
        Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
        _ => platform.to_path_buf(),
    }
}

/// The system's own temporary directory, with no variable read: on macOS the user's (`confstr`
/// of `_CS_DARWIN_USER_TEMP_DIR`, under `/var/folders`), on Android `/data/local/tmp`, else
/// `/tmp`.
///
/// # Panics
///
/// On macOS, when `confstr` refuses (std answers `/tmp` there, a directory shared with every
/// user; blaze names the refusal instead, rule no-fallback), or answers a path that is not
/// UTF-8 (the system's are `/var/folders/<two>/<id>/T/`, ASCII).
#[cfg(target_vendor = "apple")]
#[allow(unsafe_code, reason = "confstr into a stack buffer it is told the length of, then errno read")]
pub fn platform() -> PathBuf {
    // PATH_MAX on macOS: the longest path `confstr` can answer.
    let mut buffer = [0 as core::ffi::c_char; 1024];
    #[cfg(target_os = "macos")]
    use blaze_sys::posix::thread::{_CS_DARWIN_USER_TEMP_DIR, confstr};
    #[cfg(target_os = "macos")]
    // SAFETY: the buffer is writable for the given byte count, including the terminating NUL.
    let needed = unsafe { confstr(_CS_DARWIN_USER_TEMP_DIR, buffer.as_mut_ptr(), buffer.len()) };
    #[cfg(not(target_os = "macos"))]
    // SAFETY: `buffer` is `buffer.len()` writable bytes; `confstr` writes at most that many,
    // NUL included, and returns the length it needed (NUL included) or 0 on a refusal.
    let needed = unsafe { libc::confstr(libc::_CS_DARWIN_USER_TEMP_DIR, buffer.as_mut_ptr(), buffer.len()) };
    if needed == 0 || needed > buffer.len() {
        #[cfg(target_os = "macos")]
        let errno = blaze_sys::posix::signal::errno().raw();
        #[cfg(not(target_os = "macos"))]
        // SAFETY: `__error` returns the calling thread's `errno` slot, valid for the thread's
        // life; it is read at once, before any other call can set it.
        let errno = unsafe { *libc::__error() };
        panic!("blaze_core::temp::platform: confstr(_CS_DARWIN_USER_TEMP_DIR) refused or needs {needed} bytes (errno {errno})");
    }
    // `needed` counts the NUL; the bytes before it are the path. `c_char` is one byte whose
    // bits are the path's byte (`as` reinterprets `i8` as `u8`).
    let bytes: [u8; 1024] = buffer.map(|byte| byte as u8);
    match core::str::from_utf8(&bytes[..needed - 1]) {
        Ok(text) => PathBuf::from(text),
        Err(error) => panic!("blaze_core::temp::platform: confstr(_CS_DARWIN_USER_TEMP_DIR) answered a path that is not UTF-8: {error}"),
    }
}

/// See the Apple form: `/data/local/tmp` on Android.
#[cfg(all(unix, target_os = "android"))]
pub fn platform() -> PathBuf {
    PathBuf::from("/data/local/tmp")
}

/// See the Apple form: `/tmp` on every other unix.
#[cfg(all(unix, not(target_vendor = "apple"), not(target_os = "android")))]
pub fn platform() -> PathBuf {
    PathBuf::from("/tmp")
}

#[cfg(test)]
mod tests {
    use super::{Path, base_in};

    /// `TMPDIR` given and not empty is the directory; given empty, or not given, the platform's.
    ///
    /// Expected line: `test temp::given::tests::base_in_takes_tmpdir_and_refuses_empty ... ok`.
    #[test]
    fn base_in_takes_tmpdir_and_refuses_empty() {
        let platform = Path::new("/tmp");
        assert_eq!(base_in(Some(Path::new("/x")), platform), Path::new("/x"));
        assert_eq!(base_in(Some(Path::new("")), platform), platform);
        assert_eq!(base_in(None, platform), platform);
        // Control: the platform's directory is the one passed, not a constant.
        assert_eq!(base_in(None, Path::new("/var/folders/q")), Path::new("/var/folders/q"));
    }
}
