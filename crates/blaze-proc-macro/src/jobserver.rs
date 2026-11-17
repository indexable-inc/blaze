//! The command's jobserver: GNU make's protocol in its fifo form, which the `jobserver` crate
//! (`cc`'s `parallel` feature), cmake and make read from `CARGO_MAKEFLAGS`
//! (`blaze_build_script::env::makeflags`). It lets a native build script's C compiles use the
//! cores blaze's command was given, never more: RocksDB's few hundred C++ files compile on every
//! worker instead of one at a time (`NUM_JOBS` was `1`: tempo's cold check spent 470 s in
//! build scripts, check-g050256).
//!
//! - ONE a command, made at its first `Script` task (blaze-cli scripts.rs) with the command's
//!   worker count: the fifo `<work>/jobs/<id>` (`id` the command's process id, handed from the
//!   edge), mode 0600 in a folder of mode 0700, so nothing of another user's reaches it. It holds
//!   `workers - 1` tokens: a native script's own worker is its implicit token (the protocol's
//!   "every client holds one"), so one script's compiles are at most `workers` at once.
//! - blaze holds one descriptor to it, read and write, non-blocking, for the whole command: a
//!   fifo's bytes live only while it is open. blaze never blocks on it (every call is
//!   non-blocking). The one wait is its own lock, `scripts.jobserver`, held only across
//!   those calls and waited for at most `LOCK_MOST` in the lock's named span (rules 15, 18).
//! - A child that exits holding tokens (killed at its deadline, a crash) cannot hang the command:
//!   each run is [`Jobserver::lend`]t, and when the last running child of the command has
//!   exited ([`Lent::end`]), no process but blaze can hold a token, so blaze counts what the fifo
//!   holds and writes back the missing ones: tokens are returned by count ([`Returned`]). While
//!   tokens are missing every script still has its implicit token: progress is never lost.
//! - [`Jobserver::end`] closes the descriptor and removes the fifo at the command's end.
//!
//! The job count is never in a key or a record (rules 21, 25): `blaze_build_script::env::Jobs`.
//! Owed (the scheduler's side): blaze's own pool workers take a token each while they run and
//! give it back when they park, so C compiles and blaze's checks together stay at the cores; then
//! the fifo holds `workers` tokens and an idle blaze lends RocksDB every core.

use core::num::NonZeroU32;
use std::{
    fs::{File, OpenOptions},
    io::{ErrorKind, Read, Write},
    os::unix::fs::{FileTypeExt, OpenOptionsExt, PermissionsExt},
};

use blaze_build_script::Jobs;
use blaze_core::lock::{Lock, LockGuard};

/// The longest the jobserver's lock is waited for. It is held only for a few non-blocking
/// calls (a count, a fill), so a wait this long is a bug, refused by name (rule 18).
const LOCK_MOST: core::time::Duration = core::time::Duration::from_secs(10);

/// The byte each token is (GNU make writes `+`).
pub const TOKEN: u8 = b'+';

/// The folder under the native work folder that holds each command's fifo.
pub const FOLDER: &str = "jobs";

/// What [`Jobserver::new`] is made from.
#[derive(Clone, Copy, Debug)]
pub struct JobserverOpts<'a> {
    /// The command's native work folder (`NativeSetting::work`), canonical: Seatbelt matches
    /// resolved paths, and the fifo is granted by its path.
    pub work: &'a str,
    /// The command's workers: the parallelism its scripts' children share.
    pub workers: NonZeroU32,
    /// What names this command's fifo among concurrent commands of one home: its process id.
    pub id: u32,
}

/// Why the jobserver could not be made or used; each names the fifo.
#[derive(Debug)]
pub enum JobserverError {
    /// The folder or the fifo could not be made, opened or filled.
    Io { path: std::string::String, what: &'static str, error: std::io::Error },
    /// What is at the fifo's path is not the fifo blaze made.
    NotFifo { path: std::string::String },
    /// The command's jobserver was ended (the command is over), or a give-back failed earlier.
    Ended { path: std::string::String, why: std::string::String },
    /// Its lock was held past [`LOCK_MOST`] (it is held only for non-blocking calls: a bug).
    Busy { path: std::string::String },
}

impl core::fmt::Display for JobserverError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            JobserverError::Io { path, what, error } => write!(f, "{path}: {what}: {error} (the command's jobserver, jobserver.rs)"),
            JobserverError::NotFifo { path } => write!(f, "{path}: not the fifo this command made for its jobserver (jobserver.rs)"),
            JobserverError::Ended { path, why } => write!(f, "{path}: the command's jobserver is ended: {why} (jobserver.rs)"),
            JobserverError::Busy { path } => write!(f, "{path}: the command's jobserver lock was held past {LOCK_MOST:?}, though only non-blocking calls hold it (jobserver.rs, rule 18)"),
        }
    }
}

impl std::error::Error for JobserverError {}

struct State {
    /// The command's descriptor: read and write, non-blocking. `None` once ended.
    file: Option<File>,
    /// The native runs lent and not yet ended.
    running: u32,
    /// Tokens written back by count over the command (a child kept them).
    restored: u64,
    /// A give-back that failed in a drop (it cannot return its error): the next lend fails
    /// naming it (rule 3).
    broken: Option<std::string::String>,
}

/// The command's jobserver ([`JobserverOpts`]).
pub struct Jobserver {
    fifo: std::string::String,
    workers: NonZeroU32,
    /// `workers - 1`: what the fifo holds at rest.
    tokens: u32,
    state: Lock<State>,
}

impl core::fmt::Debug for Jobserver {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Jobserver").field("fifo", &self.fifo).field("workers", &self.workers).field("tokens", &self.tokens).finish()
    }
}

fn io<'p>(path: &'p str, what: &'static str) -> impl FnOnce(std::io::Error) -> JobserverError + 'p {
    move |error| JobserverError::Io { path: path.to_owned(), what, error }
}

/// Every byte the fifo holds now, read without blocking.
fn drain(mut file: &File, path: &str) -> Result<u64, JobserverError> {
    let mut found = 0u64;
    let mut buf = [0u8; 256];
    loop {
        match file.read(&mut buf) {
            // A fifo blaze holds open for writing never ends; zero is the kernel's "nothing".
            Ok(0) => return Ok(found),
            Ok(n) => found += n as u64,
            Err(error) if error.kind() == ErrorKind::WouldBlock => return Ok(found),
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => return Err(io(path, "counting the tokens at rest")(error)),
        }
    }
}

/// `count` tokens written without blocking (they fit: a fifo holds at least `PIPE_BUF`, 512
/// bytes, and far more on macOS and Linux; a command has at most a few hundred workers).
fn fill(mut file: &File, count: u32, path: &str) -> Result<(), JobserverError> {
    let buf = [TOKEN; 256];
    let mut left = count as usize;
    while left > 0 {
        let n = left.min(buf.len());
        file.write_all(&buf[..n]).map_err(io(path, "writing its tokens"))?;
        left -= n;
    }
    Ok(())
}

impl Jobserver {
    /// The command's jobserver: the fifo made (a stale one of a dead command with the same id
    /// removed first), opened and filled with `workers - 1` tokens.
    pub fn new(opts: JobserverOpts<'_>) -> Result<Jobserver, JobserverError> {
        let folder = format!("{}/{FOLDER}", opts.work);
        std::fs::create_dir_all(&folder).map_err(io(&folder, "making the jobservers' folder"))?;
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o700)).map_err(io(&folder, "making the jobservers' folder its owner's alone"))?;
        let fifo = format!("{folder}/{}", opts.id);
        match std::fs::remove_file(&fifo) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(io(&fifo, "removing a dead command's fifo")(error)),
        }
        let text = std::ffi::CString::new(fifo.as_bytes()).map_err(|_| JobserverError::NotFifo { path: fifo.clone() })?;
        // SAFETY: `text` is a NUL-terminated path borrowed for the call; the mode is an integer.
        if unsafe { libc::mkfifo(text.as_ptr(), 0o600) } != 0 {
            return Err(io(&fifo, "making the fifo")(std::io::Error::last_os_error()));
        }
        // The umask can only take bits away; set exactly 0600 all the same.
        std::fs::set_permissions(&fifo, std::fs::Permissions::from_mode(0o600)).map_err(io(&fifo, "making the fifo its owner's alone"))?;
        // Read and write: an open of a fifo for one side alone waits for the other.
        let file = OpenOptions::new().read(true).write(true).custom_flags(libc::O_NONBLOCK).open(&fifo).map_err(io(&fifo, "opening the fifo"))?;
        let kind = file.metadata().map_err(io(&fifo, "looking at the opened fifo"))?;
        if !kind.file_type().is_fifo() {
            return Err(JobserverError::NotFifo { path: fifo });
        }
        let tokens = opts.workers.get() - 1;
        fill(&file, tokens, &fifo)?;
        Ok(Jobserver { fifo, workers: opts.workers, tokens, state: Lock::new("scripts.jobserver", State { file: Some(file), running: 0, restored: 0, broken: None }) })
    }

    /// The fifo's path (granted read and write to each native run).
    pub fn fifo(&self) -> &str {
        &self.fifo
    }

    /// What a native run is told ([`blaze_build_script::env::job_vars`]).
    pub fn jobs(&self) -> Jobs<'_> {
        Jobs::Shared { workers: self.workers, fifo: &self.fifo }
    }

    /// The tokens the fifo holds at rest: `workers - 1`.
    pub fn tokens(&self) -> u32 {
        self.tokens
    }

    /// The state, its lock waited for at most [`LOCK_MOST`] (rule 18).
    fn state(&self) -> Result<LockGuard<'_, State>, JobserverError> {
        self.state.try_lock_for(LOCK_MOST).ok_or_else(|| JobserverError::Busy { path: self.fifo.clone() })
    }

    /// Tokens written back by count so far (each a token a child exited holding).
    pub fn restored(&self) -> Result<u64, JobserverError> {
        Ok(self.state()?.restored)
    }

    /// One native run's lease, taken before its child starts and ended after it (and its
    /// process group) is gone ([`Lent::end`]).
    pub fn lend(&self) -> Result<Lent<'_>, JobserverError> {
        let mut state = self.state()?;
        if let Some(why) = &state.broken {
            return Err(JobserverError::Ended { path: self.fifo.clone(), why: why.clone() });
        }
        if state.file.is_none() {
            return Err(JobserverError::Ended { path: self.fifo.clone(), why: "the command is over".to_owned() });
        }
        state.running += 1;
        Ok(Lent { server: self, ended: false })
    }

    /// One run ended: when it was the last running, the tokens at rest counted and the missing
    /// ones written back. Under the lock, so no run starts between the count and the fill.
    fn give_back(&self) -> Result<Returned, JobserverError> {
        let mut state = self.state()?;
        state.running -= 1;
        if state.running > 0 {
            return Ok(Returned { kept: 0 });
        }
        let Some(file) = &state.file else { return Ok(Returned { kept: 0 }) };
        let found = drain(file, &self.fifo)?;
        // Every token blaze holds goes back: what was found, and what a child kept. More than
        // were made (a child that wrote what it never took) is cut back to the count.
        fill(file, self.tokens, &self.fifo)?;
        // Below `tokens` (a `u32`) when anything is missing, so the narrowing is exact.
        let kept = if found >= u64::from(self.tokens) { 0 } else { self.tokens - found as u32 };
        state.restored += u64::from(kept);
        Ok(Returned { kept })
    }

    /// The command is over: the descriptor closed, the fifo removed.
    pub fn end(&self) -> Result<(), JobserverError> {
        let file = self.state()?.file.take();
        drop(file);
        match std::fs::remove_file(&self.fifo) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(io(&self.fifo, "removing the fifo at the command's end")(error)),
        }
    }
}

/// What ending a run gave back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Returned {
    /// Tokens the command's children had kept when the last of them exited, written back.
    pub kept: u32,
}

/// One native run's lease of the jobserver ([`Jobserver::lend`]).
#[must_use = "a lease is ended after its child exits (Lent::end)"]
pub struct Lent<'j> {
    server: &'j Jobserver,
    ended: bool,
}

impl Lent<'_> {
    /// The run's child and its process group are gone: its tokens are given back by count when
    /// no other run is running.
    pub fn end(mut self) -> Result<Returned, JobserverError> {
        self.ended = true;
        self.server.give_back()
    }
}

impl Drop for Lent<'_> {
    /// A lease dropped unended (a panic between the lend and the end): given back all the same,
    /// and a failure kept so the next lend names it.
    fn drop(&mut self) {
        if self.ended {
            return;
        }
        if let Err(error) = self.server.give_back() {
            // Unkept only when the lock itself is stuck, which `give_back` named: the next
            // lend then fails on the same lock, so nothing is lost quietly.
            if let Ok(mut state) = self.server.state() {
                state.broken = Some(error.to_string());
            }
        }
    }
}

#[cfg(test)]
#[path = "jobserver_tests.rs"]
mod tests;
