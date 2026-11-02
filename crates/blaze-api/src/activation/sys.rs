//! The system calls of [`super::take`]: systemd's variables read and cleared, launchd asked, and
//! each descriptor checked open and made close-on-exec before it is owned.

use std::os::fd::{FromRawFd as _, OwnedFd, RawFd};

use super::{ActivationError, FIRST_FD, Launchd, Listen, Manager};

const VARIABLES: [&str; 3] = ["LISTEN_PID", "LISTEN_FDS", "LISTEN_FDNAMES"];

/// # Safety
///
/// As [`super::take`]'s: no other thread runs, and the descriptors from [`FIRST_FD`] are owned
/// by nothing else.
pub(super) unsafe fn systemd(want: &str) -> Result<OwnedFd, ActivationError> {
    let mut read: [Option<String>; 3] = [None, None, None];
    for (value, name) in read.iter_mut().zip(VARIABLES) {
        if let Some(raw) = std::env::var_os(name) {
            let text = raw.into_string().map_err(|it| {
                // alloc: error path, a daemon that cannot start.
                ActivationError::new(format!("{name}={:?} is not UTF-8", it.to_string_lossy()))
            })?;
            *value = Some(text);
        }
    }
    for name in VARIABLES {
        // SAFETY: the caller's contract: no other thread reads or writes the environment.
        unsafe { std::env::remove_var(name) };
    }
    let [pid, fds, names] = &read;
    let listen = Listen { pid: pid.as_deref(), fds: fds.as_deref(), names: names.as_deref() };
    let fd = super::systemd(listen, std::process::id(), want)?;
    debug_assert_eq!(fd, FIRST_FD);
    // SAFETY: the caller's contract: systemd passed `fd` for this process (LISTEN_PID is this
    // process, checked above) and nothing else in it owns it.
    unsafe { adopt(fd, Manager::Systemd) }
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    /// `<launch.h>`, in libSystem: on success `*fds` is a `malloc`ed array of `*cnt`
    /// descriptors, the caller's to close and free.
    fn launch_activate_socket(name: *const libc::c_char, fds: *mut *mut libc::c_int, cnt: *mut libc::size_t)
    -> libc::c_int;
}

#[cfg(target_os = "macos")]
pub(super) fn launchd(want: &str) -> Result<OwnedFd, ActivationError> {
    let name = std::ffi::CString::new(want)
        // alloc: error path, a daemon that cannot start.
        .map_err(|_| ActivationError::new(format!("the socket name {want:?} holds a NUL")))?;
    let mut fds: *mut libc::c_int = std::ptr::null_mut();
    let mut count: libc::size_t = 0;
    // SAFETY: `name` is NUL-terminated and outlives the call; `fds` and `count` are valid for
    // writes.
    let code = unsafe { launch_activate_socket(name.as_ptr(), &raw mut fds, &raw mut count) };
    // alloc: launchd's descriptors, once per daemon start.
    let mut owned = Vec::new();
    let mut failed = None;
    if code == 0 && !fds.is_null() {
        // SAFETY: on success launchd wrote `count` descriptors to the array at `fds`.
        let given = unsafe { std::slice::from_raw_parts(fds, count) };
        for &fd in given {
            // SAFETY: launchd passed `fd` to this process for it to own, once.
            match unsafe { adopt(fd, Manager::Launchd) } {
                Ok(fd) => owned.push(fd),
                Err(error) => failed = failed.or(Some(error)),
            }
        }
        // SAFETY: `fds` is launchd's `malloc`ed array, freed once, here, after its last read.
        unsafe { libc::free(fds.cast()) };
    }
    if let Some(error) = failed {
        return Err(error);
    }
    let said = match code {
        0 => Launchd::Fds(owned.len()),
        libc::ESRCH => Launchd::NotManaged,
        libc::ENOENT => Launchd::NoSuchSocket,
        libc::EALREADY => Launchd::Already,
        other => Launchd::Other(other),
    };
    super::launchd(said, want)?;
    Ok(owned.pop().expect("launchd passed one descriptor, checked above"))
}

#[cfg(not(target_os = "macos"))]
pub(super) fn launchd(_: &str) -> Result<OwnedFd, ActivationError> {
    // alloc: error path, a daemon that cannot start.
    Err(ActivationError::new("launchd is macOS's, and this is not macOS".to_owned()))
}

pub(super) fn uid() -> u32 {
    // SAFETY: `getuid` takes nothing, touches no memory and cannot fail.
    unsafe { libc::getuid() }
}

/// `fd`, checked open and made close-on-exec, owned.
///
/// # Safety
///
/// `fd` was passed to this process to own and nothing else owns it.
unsafe fn adopt(fd: RawFd, by: Manager) -> Result<OwnedFd, ActivationError> {
    let failed = |what: &str| {
        // alloc: error path, a daemon that cannot start.
        ActivationError::new(format!("fd {fd}, which {by} passed: {what}: {}", std::io::Error::last_os_error()))
    };
    // SAFETY: no pointers; an fd that is not open fails with EBADF.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(failed("not open"));
    }
    // SAFETY: no pointers; `fd` is open (above).
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
        return Err(failed("made close-on-exec"));
    }
    // SAFETY: `fd` is open, and by the contract owned by nothing else: owned once, here.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}
