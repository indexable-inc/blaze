//! The user's home directory: the one place blaze answers "where is this user's home".
//!
//! Driver side: [`user`] reads the environment and asks the operating system, so no sans-IO
//! core calls it; a core takes the path, or the two inputs of [`decide`], from its driver.
//!
//! The answer is `HOME` when it is set and not empty (on Windows `HOME`, then `USERPROFILE`):
//! the user's own override, as every Unix program takes it. Otherwise it is the operating
//! system's record of the user the process runs as ([`record`]): on Unix the user database's
//! entry for the effective uid (`getpwuid_r`), on Windows the profile directory of the
//! process's token (`GetUserProfileDirectoryW`). Otherwise a [`NoHome`] that says what was
//! tried.
//!
//! Asking the system is not a fallback in rule no-fallback's sense: it is the same fact from
//! its authoritative source. `HOME` is a copy of that record that a login puts in the
//! environment, and a systemd unit, a CI runner, a sandbox or cron often starts a program
//! without it; the user still has a home. Nothing is guessed: no `/root`, no `/`, no `~`, no
//! current directory.
//!
//! Every other spelling is banned in `.clippy.toml` (`std::env::home_dir`, and
//! `std::env::var_os("HOME")` with the rest of `std::env::var_os`), so there stays one.

use std::{ffi::OsString, fmt, path::PathBuf};

/// The variables that override the system's record, first to last.
#[cfg(not(windows))]
pub const VARS: &[&str] = &["HOME"];
/// The variables that override the system's record, first to last: `HOME` where a shell set
/// one, then `USERPROFILE`, which Windows itself sets.
#[cfg(windows)]
pub const VARS: &[&str] = &["HOME", "USERPROFILE"];

/// Why the operating system's own record gave no home directory ([`record`]).
#[derive(Clone, PartialEq, Eq, Debug)] // clone: plain data, on an error path
pub enum NoRecord {
    /// Unix: the user database has no entry for the process's effective uid (a container
    /// started with a uid no `/etc/passwd` or directory service knows).
    NoEntry { uid: u32 },
    /// Unix: the entry is there and its home directory is empty.
    NoDir { uid: u32 },
    /// Unix: `getpwuid_r` failed with `errno`.
    Failed { uid: u32, errno: i32 },
    /// Unix: the entry did not fit in `bytes` bytes, the most [`record`] offers.
    TooLong { uid: u32, bytes: usize },
    /// Windows: `call` failed with `code` (`GetLastError`).
    Profile { call: &'static str, code: u32 },
    /// A target blaze has no call for yet (WASI).
    Unbuilt { target: &'static str },
}

impl fmt::Display for NoRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NoRecord::NoEntry { uid } => {
                write!(f, "the user database has no entry for uid {uid} (getpwuid_r)")
            }
            NoRecord::NoDir { uid } => {
                write!(f, "the user database's entry for uid {uid} names no home directory (getpwuid_r)")
            }
            NoRecord::Failed { uid, errno } => write!(
                f,
                "the user database could not be asked for uid {uid} (getpwuid_r: {})",
                std::io::Error::from_raw_os_error(*errno)
            ),
            NoRecord::TooLong { uid, bytes } => write!(
                f,
                "the user database's entry for uid {uid} does not fit in {bytes} bytes (getpwuid_r: ERANGE)"
            ),
            NoRecord::Profile { call, code } => write!(
                f,
                "Windows named no profile directory for the user this process runs as ({call}: error {code})"
            ),
            NoRecord::Unbuilt { target } => {
                write!(f, "blaze has no way to ask {target} for its user's home directory")
            }
        }
    }
}

/// No home directory: no variable of [`VARS`] is set, and the system's record gave none.
/// Its text says both, so whoever reads it knows what to set.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: plain data, on an error path
pub struct NoHome {
    /// Why the system's record gave none.
    pub record: NoRecord,
}

impl fmt::Display for NoHome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let vars = match VARS {
            [one] => format!("{one} is"), // alloc: error path
            many => format!("{} are", many.join(" and ")), // alloc: error path
        };
        write!(f, "no home directory was found: {vars} unset or empty, and {}", self.record)
    }
}

impl std::error::Error for NoHome {}

/// The decision, with no IO of its own: the first of [`VARS`] that `var` has set and not
/// empty, else what `record` answers. `record` is asked only when no variable answers, so a
/// process with `HOME` never opens the user database.
///
/// # Errors
///
/// [`NoHome`] with `record`'s reason when no variable is set and `record` has none.
pub fn decide(
    var: impl Fn(&str) -> Option<OsString>,
    record: impl FnOnce() -> Result<PathBuf, NoRecord>,
) -> Result<PathBuf, NoHome> {
    for name in VARS {
        if let Some(home) = var(name).filter(|it| !it.is_empty()) {
            return Ok(PathBuf::from(home));
        }
    }
    record().map_err(|record| NoHome { record })
}

/// The home directory of the user this process runs as: [`decide`] over the process's
/// environment and the system's [`record`].
///
/// # Errors
///
/// [`NoHome`], saying what was tried.
pub fn user() -> Result<PathBuf, NoHome> {
    decide(|name| std::env::var_os(name), record)
}

/// [`user`] for an environment that is not this process's own (a request's, captured by the
/// client of the same user): [`decide`] over `var` and the system's [`record`].
///
/// # Errors
///
/// [`NoHome`], saying what was tried.
pub fn user_in(var: impl Fn(&str) -> Option<OsString>) -> Result<PathBuf, NoHome> {
    decide(var, record)
}

/// The operating system's own record of the home directory of the user this process runs as,
/// with no variable read.
///
/// # Errors
///
/// [`NoRecord`], naming the call and what it said.
pub fn record() -> Result<PathBuf, NoRecord> {
    os::record()
}

#[cfg(unix)]
#[allow(unsafe_code, reason = "geteuid, sysconf and getpwuid_r, each one call whose buffers outlive it")]
mod os {
    use std::{
        ffi::{CStr, OsStr},
        os::unix::ffi::OsStrExt as _,
        path::PathBuf,
    };

    use super::NoRecord;

    /// The buffer's first size when the system suggests none.
    const FIRST: usize = 1024;
    /// The most bytes an entry is given: a directory service's entry with a long `gecos` is a
    /// few kilobytes, and past this the entry is refused, not grown for ever.
    const MOST: usize = 1 << 20;
    /// How often an interrupted call is made again.
    const INTERRUPTED_MOST: u32 = 8;

    pub(super) fn record() -> Result<PathBuf, NoRecord> {
        // SAFETY: `geteuid` takes nothing and always succeeds.
        let uid = unsafe { libc::geteuid() };
        // SAFETY: `sysconf` takes a name by value. It answers -1 where the system has no
        // limit (musl), and its answer is a first size only: `ERANGE` below grows the buffer.
        let hint = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
        let mut len = usize::try_from(hint).ok().filter(|&it| it > 0).unwrap_or(FIRST).min(MOST);
        let mut interrupted = 0;
        loop {
            let mut buffer = vec![0 as libc::c_char; len]; // alloc: once per ask, only when no variable names the home
            // SAFETY: `passwd` is integers and pointers, for which all zeroes is a value.
            let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
            let mut found: *mut libc::passwd = std::ptr::null_mut();
            // SAFETY: `entry`, `buffer` (of `buffer.len()` bytes) and `found` are live and
            // exclusively borrowed for the call; `getpwuid_r` writes the entry into `entry`,
            // its strings into `buffer`, and `&mut entry` or null into `found`, and keeps none
            // of them.
            let said = unsafe {
                libc::getpwuid_r(uid, &raw mut entry, buffer.as_mut_ptr(), buffer.len(), &raw mut found)
            };
            match said {
                0 if found.is_null() => return Err(NoRecord::NoEntry { uid }),
                0 => {
                    if entry.pw_dir.is_null() {
                        return Err(NoRecord::NoDir { uid });
                    }
                    // SAFETY: on success `pw_dir` points at a NUL-terminated string inside
                    // `buffer`, which is alive and unchanged until the bytes are copied here.
                    let dir = unsafe { CStr::from_ptr(entry.pw_dir) }.to_bytes();
                    if dir.is_empty() {
                        return Err(NoRecord::NoDir { uid });
                    }
                    return Ok(PathBuf::from(OsStr::from_bytes(dir)));
                }
                libc::ERANGE if len < MOST => len = (len * 2).min(MOST),
                libc::ERANGE => return Err(NoRecord::TooLong { uid, bytes: len }),
                libc::EINTR if interrupted < INTERRUPTED_MOST => interrupted += 1,
                // What some systems say for "no such user" in place of a null result.
                libc::ENOENT | libc::ESRCH => return Err(NoRecord::NoEntry { uid }),
                errno => return Err(NoRecord::Failed { uid, errno }),
            }
        }
    }
}

#[cfg(windows)]
#[allow(unsafe_code, reason = "the process's token and GetUserProfileDirectoryW, the buffer its own")]
mod os {
    use std::{ffi::OsString, os::windows::ffi::OsStringExt as _, path::PathBuf};

    use windows_sys::Win32::{
        Foundation::{CloseHandle, ERROR_INSUFFICIENT_BUFFER, GetLastError, HANDLE},
        Security::TOKEN_QUERY,
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };

    use super::NoRecord;

    // The documented call for a user's profile directory (userenv.h). windows-sys has it under
    // `Win32_UI_Shell`, a feature blaze does not otherwise build; declared here it brings none.
    #[link(name = "userenv")]
    unsafe extern "system" {
        fn GetUserProfileDirectoryW(token: HANDLE, dir: *mut u16, len: *mut u32) -> i32;
    }

    pub(super) fn record() -> Result<PathBuf, NoRecord> {
        let failed = |call: &'static str| {
            // SAFETY: `GetLastError` takes nothing and reads the thread's last error.
            NoRecord::Profile { call, code: unsafe { GetLastError() } }
        };
        let mut token: HANDLE = std::ptr::null_mut();
        // SAFETY: the pseudo-handle of this process needs no close, and `token` is a live
        // out-pointer the call writes one handle to.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) } == 0 {
            return Err(failed("OpenProcessToken"));
        }
        let dir = (|| {
            let mut len: u32 = 0;
            // SAFETY: a null buffer with a zero length is the documented way to ask for the
            // length: the call fails with ERROR_INSUFFICIENT_BUFFER and writes it to `len`.
            let asked = unsafe { GetUserProfileDirectoryW(token, std::ptr::null_mut(), &raw mut len) };
            // SAFETY: as in `failed`.
            if asked != 0 || unsafe { GetLastError() } != ERROR_INSUFFICIENT_BUFFER || len == 0 {
                return Err(failed("GetUserProfileDirectoryW"));
            }
            let mut buffer = vec![0u16; len as usize]; // alloc: once per ask, only when no variable names the home
            // SAFETY: `buffer` has `len` units, which the call is told; it writes at most that
            // many, NUL-terminated.
            if unsafe { GetUserProfileDirectoryW(token, buffer.as_mut_ptr(), &raw mut len) } == 0 {
                return Err(failed("GetUserProfileDirectoryW"));
            }
            let end = buffer.iter().position(|&it| it == 0).unwrap_or(buffer.len());
            if end == 0 {
                return Err(NoRecord::Profile { call: "GetUserProfileDirectoryW", code: 0 });
            }
            Ok(PathBuf::from(OsString::from_wide(&buffer[..end])))
        })();
        // SAFETY: `token` was opened above and is closed once, here.
        unsafe { CloseHandle(token) };
        dir
    }
}

#[cfg(not(any(unix, windows)))]
mod os {
    use std::path::PathBuf;

    use super::NoRecord;

    pub(super) fn record() -> Result<PathBuf, NoRecord> {
        Err(NoRecord::Unbuilt { target: std::env::consts::OS })
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<OsString> {
        move |name: &str| pairs.iter().find(|(key, _)| *key == name).map(|(_, value)| OsString::from(*value))
    }

    fn none() -> Result<PathBuf, NoRecord> {
        Err(NoRecord::NoEntry { uid: 4242 })
    }

    fn recorded() -> Result<PathBuf, NoRecord> {
        Ok(PathBuf::from("/recorded"))
    }

    /// The invariant: the home is `HOME` when it is set and not empty, else the system's
    /// record, else an error that names what was tried; the record is asked only when no
    /// variable answers.
    #[test]
    fn home_is_the_variable_then_the_systems_record_then_an_error() {
        // Set: the variable, and the record is never asked.
        let asked = Cell::new(0);
        let counted = || {
            asked.set(asked.get() + 1);
            recorded()
        };
        assert_eq!(decide(env(&[("HOME", "/u")]), counted), Ok(PathBuf::from("/u")));
        assert_eq!(asked.get(), 0, "HOME answered, and the user database was still asked");
        // Empty is unset.
        assert_eq!(decide(env(&[("HOME", "")]), recorded), Ok(PathBuf::from("/recorded")));
        // Unset, and the system has a record.
        assert_eq!(decide(env(&[]), recorded), Ok(PathBuf::from("/recorded")));
        // Unset, and the system has none: the error carries the record's reason.
        let refused = decide(env(&[]), none).unwrap_err();
        assert_eq!(refused, NoHome { record: NoRecord::NoEntry { uid: 4242 } });
        let said = refused.to_string();
        assert!(said.contains("HOME"), "{said}");
        assert!(said.contains("no entry for uid 4242"), "{said}");
        // Another variable is not the home.
        assert_eq!(decide(env(&[("BLAZE_HOME", "/b")]), none), Err(refused));
    }

    /// The control (rule test-the-cause): with the record the same, taking `HOME` away is what
    /// moves the answer, and with `HOME` the same, changing the record moves nothing.
    #[test]
    fn the_variable_is_what_overrides_the_record() {
        let with = decide(env(&[("HOME", "/u")]), recorded);
        let without = decide(env(&[]), recorded);
        assert_ne!(with, without);
        assert_eq!(with, decide(env(&[("HOME", "/u")]), none));
        assert_ne!(without, decide(env(&[]), none));
    }

    #[cfg(windows)]
    #[test]
    fn on_windows_userprofile_answers_after_home() {
        let both = env(&[("HOME", "/h"), ("USERPROFILE", "C:\\Users\\u")]);
        assert_eq!(decide(both, none), Ok(PathBuf::from("/h")));
        let profile = env(&[("HOME", ""), ("USERPROFILE", "C:\\Users\\u")]);
        assert_eq!(decide(profile, none), Ok(PathBuf::from("C:\\Users\\u")));
    }

    /// Needs the real system: the user database (or the Windows profile) of the machine the
    /// test runs on, asked for the user the test runs as, with no variable read. That user
    /// has an entry wherever blaze's tests run (the Mac, an ix VM, CI's runner), and its home
    /// is a directory that is there.
    #[cfg(any(unix, windows))]
    #[test]
    fn the_systems_record_of_this_user_is_a_directory_that_is_there() {
        let home = record().expect("the user the tests run as has a home in the system's record");
        assert!(home.is_absolute(), "{}", home.display());
        assert!(home.is_dir(), "{} is not a directory", home.display());
        // And it is what `user` answers when no variable does.
        assert_eq!(decide(|_| None, record), Ok(home));
    }
}
