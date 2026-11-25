//! Async-signal-safe path remapping for compile-time `env!("CARGO_BIN_EXE_<name>")` guest paths
//! (`/target/debug/bin/<package>/<name>`) to the test run's host launcher stubs (`test.md` piece 17).
//!
//! Installed once on the launcher's main thread before any guest thread starts (from
//! `BLAZE_LAUNCH_BIN_MAP`, which is removed from the process environment immediately after).
//! Every wrapper is lock-free and allocation-free so `execve`/`execv`/`execvp`/`execvpe` remain
//! async-signal-safe after `fork()` in multithreaded tests.

use core::ffi::{CStr, c_char, c_int};
use core::ptr;
use core::sync::atomic::{AtomicPtr, Ordering};

use blaze_own::{ABox, AVec, Alloc, Box, Vec};

/// Fixed compile-time guest prefix emitted by `blaze_scope::crate_env::bin_exe`.
const GUEST_PREFIX: &[u8] = b"/target/debug/bin/";

struct Entry {
    guest: AVec<'static, u8>,
    /// NUL-terminated host stub path bytes.
    host: AVec<'static, u8>,
}

struct RemapTable {
    entries: AVec<'static, Entry>,
}

static MAP: AtomicPtr<RemapTable> = AtomicPtr::new(ptr::null_mut());

/// Why `BLAZE_LAUNCH_BIN_MAP` was refused before guest code started.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BinRemapError {
    /// A line lacked the `<guest>\t<host>` tab separator or had an empty side.
    MalformedLine,
    /// The guest path did not begin with `/target/debug/bin/`.
    BadGuestPrefix,
    /// Either path contained an embedded NUL byte.
    NulByte,
}

impl core::fmt::Display for BinRemapError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            BinRemapError::MalformedLine => f.write_str("BLAZE_LAUNCH_BIN_MAP holds a line without `<guest>\\t<host>`"),
            BinRemapError::BadGuestPrefix => f.write_str("BLAZE_LAUNCH_BIN_MAP guest path does not start with `/target/debug/bin/`"),
            BinRemapError::NulByte => f.write_str("BLAZE_LAUNCH_BIN_MAP holds a path with an embedded NUL byte"),
        }
    }
}

/// Parses `raw` (`<guest>\t<host>\n` lines) and installs the process-wide immutable remap table.
/// Returns `Ok(true)` if at least one mapping was installed, `Ok(false)` if `raw` was empty.
pub fn install(raw: &[u8], alloc: Alloc<'static>) -> Result<bool, BinRemapError> {
    let entries = parse_entries(raw, alloc)?;
    if entries.is_empty() {
        return Ok(false);
    }
    let table: ABox<'static, RemapTable> = Box::new_in(RemapTable { entries }, alloc);
    let (ptr, alloc) = Box::into_raw_with_allocator(table);
    if MAP
        .compare_exchange(ptr::null_mut(), ptr, Ordering::Release, Ordering::Relaxed)
        .is_err()
    {
        // SAFETY: `ptr` was never published; reclaim it with the same `alloc`.
        drop(unsafe { Box::from_raw_in(ptr, alloc) });
    }
    Ok(true)
}

fn parse_entries(raw: &[u8], alloc: Alloc<'static>) -> Result<AVec<'static, Entry>, BinRemapError> {
    let mut out = Vec::new_in(alloc);
    for line in raw.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let Some(tab) = line.iter().position(|&b| b == b'\t') else {
            return Err(BinRemapError::MalformedLine);
        };
        let Some(guest) = line.get(..tab) else {
            return Err(BinRemapError::MalformedLine);
        };
        let Some(host) = line.get(tab + 1..) else {
            return Err(BinRemapError::MalformedLine);
        };
        if guest.is_empty() || host.is_empty() {
            return Err(BinRemapError::MalformedLine);
        }
        if !guest.starts_with(GUEST_PREFIX) {
            return Err(BinRemapError::BadGuestPrefix);
        }
        if guest.contains(&0) || host.contains(&0) {
            return Err(BinRemapError::NulByte);
        }
        let mut guest_buf = Vec::with_capacity_in(guest.len(), alloc);
        guest_buf.extend_from_slice(guest);
        let mut host_buf = Vec::with_capacity_in(host.len() + 1, alloc);
        host_buf.extend_from_slice(host);
        host_buf.push(0);
        out.push(Entry { guest: guest_buf, host: host_buf });
    }
    Ok(out)
}

/// Maps `path` to its host launcher stub C string when it matches an installed guest binary path;
/// otherwise returns `path` unchanged. Lock-free, zero-allocation, and async-signal-safe.
#[inline]
pub(crate) unsafe fn remap_cstr(path: *const c_char) -> *const c_char {
    if path.is_null() {
        return path;
    }
    // Fast-path exit for every relative path (single byte load).
    // SAFETY: caller passes a valid NUL-terminated C string pointer when non-null.
    if unsafe { *path.cast::<u8>() } != b'/' {
        return path;
    }
    let ptr = MAP.load(Ordering::Acquire);
    if ptr.is_null() {
        return path;
    }
    // SAFETY: `ptr` was published once and never freed for the life of the process.
    let entries = unsafe { (*ptr).entries.as_slice() };
    // SAFETY: caller passes a valid NUL-terminated C string pointer.
    let bytes = unsafe { CStr::from_ptr(path) }.to_bytes();
    if !bytes.starts_with(GUEST_PREFIX) {
        return path;
    }
    for entry in entries.iter() {
        if entry.guest.as_slice() == bytes {
            return entry.host.as_ptr().cast::<c_char>();
        }
    }
    path
}

pub(crate) extern "C" fn remap_execve(
    path: *const c_char,
    argv: *const *const c_char,
    envp: *const *const c_char,
) -> c_int {
    // SAFETY: `remap_cstr` returns either `path` or a `'static` NUL-terminated C string.
    unsafe { libc::execve(remap_cstr(path), argv, envp) }
}

pub(crate) extern "C" fn remap_execv(path: *const c_char, argv: *const *const c_char) -> c_int {
    // SAFETY: `remap_cstr` returns either `path` or a `'static` NUL-terminated C string.
    unsafe { libc::execv(remap_cstr(path), argv) }
}

pub(crate) extern "C" fn remap_execvp(file: *const c_char, argv: *const *const c_char) -> c_int {
    // SAFETY: `remap_cstr` returns either `file` or a `'static` NUL-terminated C string.
    unsafe { libc::execvp(remap_cstr(file), argv) }
}

#[cfg(target_os = "linux")]
pub(crate) extern "C" fn remap_execvpe(
    file: *const c_char,
    argv: *const *const c_char,
    envp: *const *const c_char,
) -> c_int {
    // SAFETY: `remap_cstr` returns either `file` or a `'static` NUL-terminated C string.
    unsafe { libc::execvpe(remap_cstr(file), argv, envp) }
}

pub(crate) extern "C" fn remap_posix_spawn(
    pid: *mut libc::pid_t,
    path: *const c_char,
    file_actions: *const libc::posix_spawn_file_actions_t,
    attrp: *const libc::posix_spawnattr_t,
    argv: *const *mut c_char,
    envp: *const *mut c_char,
) -> c_int {
    // SAFETY: `remap_cstr` returns either `path` or a `'static` NUL-terminated C string.
    unsafe { libc::posix_spawn(pid, remap_cstr(path), file_actions, attrp, argv, envp) }
}

pub(crate) extern "C" fn remap_posix_spawnp(
    pid: *mut libc::pid_t,
    file: *const c_char,
    file_actions: *const libc::posix_spawn_file_actions_t,
    attrp: *const libc::posix_spawnattr_t,
    argv: *const *mut c_char,
    envp: *const *mut c_char,
) -> c_int {
    // SAFETY: `remap_cstr` returns either `file` or a `'static` NUL-terminated C string.
    unsafe { libc::posix_spawnp(pid, remap_cstr(file), file_actions, attrp, argv, envp) }
}

pub(crate) extern "C" fn remap_stat(path: *const c_char, buf: *mut libc::stat) -> c_int {
    // SAFETY: `remap_cstr` returns either `path` or a `'static` NUL-terminated C string.
    unsafe { libc::stat(remap_cstr(path), buf) }
}

pub(crate) extern "C" fn remap_lstat(path: *const c_char, buf: *mut libc::stat) -> c_int {
    // SAFETY: `remap_cstr` returns either `path` or a `'static` NUL-terminated C string.
    unsafe { libc::lstat(remap_cstr(path), buf) }
}

pub(crate) extern "C" fn remap_fstatat(
    dirfd: c_int,
    path: *const c_char,
    buf: *mut libc::stat,
    flags: c_int,
) -> c_int {
    // SAFETY: `remap_cstr` returns either `path` or a `'static` NUL-terminated C string.
    unsafe { libc::fstatat(dirfd, remap_cstr(path), buf, flags) }
}

#[cfg(target_os = "linux")]
pub(crate) extern "C" fn remap_stat64(path: *const c_char, buf: *mut libc::stat64) -> c_int {
    // SAFETY: `remap_cstr` returns either `path` or a `'static` NUL-terminated C string.
    unsafe { libc::stat64(remap_cstr(path), buf) }
}

#[cfg(target_os = "linux")]
pub(crate) extern "C" fn remap_lstat64(path: *const c_char, buf: *mut libc::stat64) -> c_int {
    // SAFETY: `remap_cstr` returns either `path` or a `'static` NUL-terminated C string.
    unsafe { libc::lstat64(remap_cstr(path), buf) }
}

#[cfg(target_os = "linux")]
pub(crate) extern "C" fn remap_fstatat64(
    dirfd: c_int,
    path: *const c_char,
    buf: *mut libc::stat64,
    flags: c_int,
) -> c_int {
    // SAFETY: `remap_cstr` returns either `path` or a `'static` NUL-terminated C string.
    unsafe { libc::fstatat64(dirfd, remap_cstr(path), buf, flags) }
}

#[cfg(target_os = "linux")]
pub(crate) extern "C" fn remap_statx(
    dirfd: c_int,
    path: *const c_char,
    flags: c_int,
    mask: libc::c_uint,
    buf: *mut libc::statx,
) -> c_int {
    // SAFETY: `remap_cstr` returns either `path` or a `'static` NUL-terminated C string.
    unsafe { libc::statx(dirfd, remap_cstr(path), flags, mask, buf) }
}

pub(crate) extern "C" fn remap_access(path: *const c_char, amode: c_int) -> c_int {
    // SAFETY: `remap_cstr` returns either `path` or a `'static` NUL-terminated C string.
    unsafe { libc::access(remap_cstr(path), amode) }
}

pub(crate) extern "C" fn remap_faccessat(
    dirfd: c_int,
    path: *const c_char,
    amode: c_int,
    flags: c_int,
) -> c_int {
    // SAFETY: `remap_cstr` returns either `path` or a `'static` NUL-terminated C string.
    unsafe { libc::faccessat(dirfd, remap_cstr(path), amode, flags) }
}

pub(crate) extern "C" fn remap_realpath(
    path: *const c_char,
    resolved: *mut c_char,
) -> *mut c_char {
    // SAFETY: `remap_cstr` returns either `path` or a `'static` NUL-terminated C string.
    unsafe { libc::realpath(remap_cstr(path), resolved) }
}

pub(crate) extern "C" fn remap_readlink(
    path: *const c_char,
    buf: *mut c_char,
    bufsiz: libc::size_t,
) -> libc::ssize_t {
    // SAFETY: `remap_cstr` returns either `path` or a `'static` NUL-terminated C string.
    unsafe { libc::readlink(remap_cstr(path), buf, bufsiz) }
}

pub(crate) extern "C" fn remap_readlinkat(
    dirfd: c_int,
    path: *const c_char,
    buf: *mut c_char,
    bufsiz: libc::size_t,
) -> libc::ssize_t {
    // SAFETY: `remap_cstr` returns either `path` or a `'static` NUL-terminated C string.
    unsafe { libc::readlinkat(dirfd, remap_cstr(path), buf, bufsiz) }
}

pub(crate) extern "C" fn remap_fopen(
    path: *const c_char,
    mode: *const c_char,
) -> *mut libc::FILE {
    // SAFETY: `remap_cstr` returns either `path` or a `'static` NUL-terminated C string.
    unsafe { libc::fopen(remap_cstr(path), mode) }
}

pub(crate) unsafe extern "C" fn remap_open(
    path: *const c_char,
    oflag: c_int,
    mut args: ...
) -> c_int {
    let remapped = unsafe { remap_cstr(path) };
    if (oflag & libc::O_CREAT) != 0 {
        let mode: libc::c_uint = unsafe { args.next_arg() };
        unsafe { libc::open(remapped, oflag, mode) }
    } else {
        unsafe { libc::open(remapped, oflag) }
    }
}

pub(crate) unsafe extern "C" fn remap_openat(
    dirfd: c_int,
    path: *const c_char,
    oflag: c_int,
    mut args: ...
) -> c_int {
    let remapped = unsafe { remap_cstr(path) };
    if (oflag & libc::O_CREAT) != 0 {
        let mode: libc::c_uint = unsafe { args.next_arg() };
        unsafe { libc::openat(dirfd, remapped, oflag, mode) }
    } else {
        unsafe { libc::openat(dirfd, remapped, oflag) }
    }
}

#[cfg(test)]
mod tests {
    use super::{BinRemapError, parse_entries};

    #[test]
    fn parse_valid_and_rejects_malformed() {
        let alloc = blaze_own::testing::heap().alloc();
        let ok = parse_entries(b"/target/debug/bin/app/cli\t/tmp/stub/cli\n\n/target/debug/bin/app/srv\t/tmp/stub/srv\n", alloc)
            .expect("valid entries");
        assert_eq!(ok.len(), 2);
        assert_eq!(ok[0].guest.as_slice(), b"/target/debug/bin/app/cli");
        assert_eq!(ok[0].host.as_slice(), b"/tmp/stub/cli\0");
        assert_eq!(parse_entries(b"no-tab", alloc).err(), Some(BinRemapError::MalformedLine));
        assert_eq!(parse_entries(b"/usr/bin/foo\t/tmp/foo", alloc).err(), Some(BinRemapError::BadGuestPrefix));
        assert_eq!(parse_entries(b"/target/debug/bin/a/b\t/tmp/a\0b", alloc).err(), Some(BinRemapError::NulByte));
    }
}
