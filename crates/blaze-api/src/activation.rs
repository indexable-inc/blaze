//! Socket activation (docs/v2/plan/design/restore.md, "The daemon starts on demand";
//! docs/v2/plan/lanes/frontend/activate.md): the service manager listens on the daemon's
//! socket, starts `blaze serve --activated` at the first connection and passes it the
//! listening socket.
//!
//! - **systemd**: `LISTEN_PID`, `LISTEN_FDS` and `LISTEN_FDNAMES`, the descriptors from
//!   [`FIRST_FD`]; read, checked against this process and [`SOCKET`], and cleared.
//! - **launchd**: `launch_activate_socket` by the name of the plist's `Sockets` entry,
//!   [`SOCKET`].
//!
//! A daemon started `--activated` listens only on what was passed: an activation that passed
//! nothing, too much, or a socket of another name or process is an error naming it, never a
//! socket made in its place. The parsing is pure ([`systemd`], [`launchd`]); taking the
//! descriptors ([`take`]) is the one unsafe step.

use std::fmt;

/// The socket's name: systemd's `FileDescriptorName=` and the key of launchd's `Sockets`.
pub const SOCKET: &str = "blaze";
/// systemd's first passed descriptor (`SD_LISTEN_FDS_START`).
pub const FIRST_FD: i32 = 3;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Manager {
    Systemd,
    Launchd,
}

impl Manager {
    /// The manager this system's user services run under; `None` where blaze has none.
    pub const fn here() -> Option<Manager> {
        if cfg!(target_os = "linux") {
            Some(Manager::Systemd)
        } else if cfg!(target_os = "macos") {
            Some(Manager::Launchd)
        } else {
            None
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Manager::Systemd => "systemd",
            Manager::Launchd => "launchd",
        }
    }
}

impl fmt::Display for Manager {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Why the daemon cannot listen on what its manager passed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ActivationError {
    pub what: String,
}

impl ActivationError {
    fn new(what: String) -> ActivationError {
        ActivationError { what }
    }
}

impl fmt::Display for ActivationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "blaze serve --activated: {}", self.what)
    }
}

impl std::error::Error for ActivationError {}

/// systemd's variables as the environment has them; `None` for one that is not set.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Listen<'a> {
    pub pid: Option<&'a str>,
    pub fds: Option<&'a str>,
    pub names: Option<&'a str>,
}

/// The descriptor systemd passed process `own` for the socket named `want`: exactly one,
/// [`FIRST_FD`], named `want`, for this process.
pub fn systemd(listen: Listen<'_>, own: u32, want: &str) -> Result<i32, ActivationError> {
    let (Some(pid), Some(fds)) = (listen.pid, listen.fds) else {
        let missing = match (listen.pid, listen.fds) {
            (None, None) => "LISTEN_PID and LISTEN_FDS",
            (None, Some(_)) => "LISTEN_PID",
            _ => "LISTEN_FDS",
        };
        // alloc: error path, a daemon that cannot start.
        return Err(ActivationError::new(format!(
            "systemd passed no socket: {missing} not set (is blaze.service started by blaze.socket?)"
        )));
    };
    // alloc: error path, a daemon that cannot start.
    let pid = decimal(pid).ok_or_else(|| ActivationError::new(format!("LISTEN_PID={pid:?} is no process ID")))?;
    if pid != u64::from(own) {
        // alloc: error path, a daemon that cannot start.
        return Err(ActivationError::new(format!(
            "LISTEN_PID={pid} names another process than this one ({own}): its sockets are not this daemon's"
        )));
    }
    // alloc: error path, a daemon that cannot start.
    let count = decimal(fds).ok_or_else(|| ActivationError::new(format!("LISTEN_FDS={fds:?} is no count")))?;
    match count {
        // alloc: error path, a daemon that cannot start.
        0 => return Err(ActivationError::new("systemd passed no socket (LISTEN_FDS=0)".to_owned())),
        1 => {}
        n => {
            let last = u64::from(FIRST_FD.unsigned_abs()).saturating_add(n - 1);
            // alloc: error path, a daemon that cannot start.
            return Err(ActivationError::new(format!(
                "systemd passed {n} sockets (fds {FIRST_FD} to {last}); blaze.socket listens on one, named {want}"
            )));
        }
    }
    let Some(names) = listen.names else {
        // alloc: error path, a daemon that cannot start.
        return Err(ActivationError::new(format!(
            "systemd named no socket (no LISTEN_FDNAMES); blaze.socket names its one {want} (FileDescriptorName={want})"
        )));
    };
    let given = names.split(':').count();
    if given != 1 {
        // alloc: error path, a daemon that cannot start.
        return Err(ActivationError::new(format!(
            "LISTEN_FDNAMES={names:?} names {given} sockets and LISTEN_FDS says 1"
        )));
    }
    if names != want {
        // alloc: error path, a daemon that cannot start.
        return Err(ActivationError::new(format!(
            "systemd passed fd {FIRST_FD} named {names:?}, not {want:?}: it is another unit's socket"
        )));
    }
    Ok(FIRST_FD)
}

/// What `launch_activate_socket` answered.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Launchd {
    /// It passed this many descriptors.
    Fds(usize),
    /// `ESRCH`: no launchd job started this process.
    NotManaged,
    /// `ENOENT`: the job's plist has no socket of the name.
    NoSuchSocket,
    /// `EALREADY`: the socket was taken already.
    Already,
    Other(i32),
}

/// Whether launchd's answer for the socket `want` is the one listening socket.
pub fn launchd(said: Launchd, want: &str) -> Result<(), ActivationError> {
    let what = match said {
        Launchd::Fds(1) => return Ok(()),
        // alloc: error path, a daemon that cannot start, as is each arm below.
        Launchd::Fds(0) => format!("launchd passed no descriptor for its socket {want}"),
        // alloc: error path, as above.
        Launchd::Fds(n) => format!(
            "launchd passed {n} descriptors for its socket {want}; the plist's Sockets entry {want} is one Unix socket"
        ),
        Launchd::NotManaged => {
            "no launchd job started this process (launch_activate_socket: ESRCH); `blaze daemon install` loads one"
                // alloc: error path, as above.
                .to_owned()
        }
        // alloc: error path, as above.
        Launchd::NoSuchSocket => format!("the launchd job that started it has no socket named {want} in its plist's Sockets"),
        // alloc: error path, as above.
        Launchd::Already => format!("launchd's socket {want} was taken already in this process"),
        // alloc: error path, as above.
        Launchd::Other(code) => format!(
            "launch_activate_socket({want}) failed: {}",
            std::io::Error::from_raw_os_error(code)
        ),
    };
    Err(ActivationError::new(what))
}

/// A decimal number of digits only, as systemd writes them.
fn decimal(text: &str) -> Option<u64> {
    if text.is_empty() || text.len() > 19 || !text.bytes().all(|it| it.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

#[cfg(unix)]
pub use passed::{Passed, take};

/// This process's user ID: launchd's domain `gui/<uid>`.
#[cfg(unix)]
pub fn uid() -> u32 {
    sys::uid()
}

#[cfg(unix)]
#[allow(unsafe_code)]
mod passed {
    use std::{
        fmt,
        os::fd::{AsRawFd as _, OwnedFd, RawFd},
    };

    use super::{ActivationError, Manager};

    /// A listening socket a service manager passed, owned: it is closed when this is dropped.
    pub struct Passed {
        fd: OwnedFd,
        by: Manager,
    }

    impl Passed {
        pub fn new(fd: OwnedFd, by: Manager) -> Passed {
            Passed { fd, by }
        }

        pub fn by(&self) -> Manager {
            self.by
        }

        /// The descriptor, for [`blaze_core::Address::Inherited`]: the driver listens on a copy
        /// of its own, so this stays open while the copy is made.
        pub fn fd(&self) -> RawFd {
            self.fd.as_raw_fd()
        }
    }

    impl fmt::Debug for Passed {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("Passed").field("fd", &self.fd()).field("by", &self.by).finish()
        }
    }

    /// The socket [`super::SOCKET`] that `by` passed this process, made close-on-exec so no
    /// process the daemon starts inherits it. systemd's variables are cleared, whatever they
    /// said.
    ///
    /// # Safety
    ///
    /// Call it once, before any other thread of the process runs: it changes the environment.
    /// The descriptors the manager passed (systemd's from [`super::FIRST_FD`]) must be owned
    /// by nothing else in the process: the one returned is the caller's to close.
    pub unsafe fn take(by: Manager) -> Result<Passed, ActivationError> {
        let fd = match by {
            // SAFETY: the caller's contract, passed on.
            Manager::Systemd => unsafe { super::sys::systemd(super::SOCKET)? },
            Manager::Launchd => super::sys::launchd(super::SOCKET)?,
        };
        Ok(Passed { fd, by })
    }
}

#[cfg(unix)]
#[allow(unsafe_code)]
mod sys;

#[cfg(test)]
mod tests;
