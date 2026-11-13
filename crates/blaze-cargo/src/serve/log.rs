//! The daemon's log, bounded: `serve.log` in its state directory, which whoever starts the
//! daemon opens for appending and hands it as its standard output and error
//! (`blaze_client::start`, `cli::daemon`). Nothing bounded it until 2026-11-11, when a day's
//! work left one of 2.2 GB (about 150,000 caught panics, a backtrace each).
//!
//! The bound: `serve.log` and the one file before it, [`BEFORE`], are together at most
//! [`CAP_BYTES`], and what is written between two checks. Once `serve.log` is half the cap
//! ([`due`]) it is renamed to [`BEFORE`] (the file that held that name is deleted), a new
//! `serve.log` is made, and the daemon's standard error (and output, when it is the same
//! file) is pointed at it; its first line says so, with the bytes that went and where the
//! lines before are, so nothing leaves without a line saying it did. The check is one `fstat`
//! after each line the daemon says ([`super::said::logged`]) and after each panic's printing
//! (`flightrec::on_panic`), the two writers of the log; a line written with a bare
//! `eprintln!` (one whose event is beside it) is counted by the next check.
//!
//! A standard error that is not the state directory's `serve.log` (a terminal, a pipe, a
//! service manager's journal) is not the daemon's to bound, and is left alone.

use std::{
    fs,
    io::Write as _,
    os::{
        fd::{AsRawFd as _, RawFd},
        unix::fs::{MetadataExt as _, OpenOptionsExt as _},
    },
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

/// The log's name in the state directory.
pub const LOG: &str = "serve.log";

/// The one file kept of the log before the last rotation.
pub const BEFORE: &str = "serve.log.1";

/// The most `serve.log` and [`BEFORE`] hold together: 16 MiB, about 100,000 lines. A build
/// says a few lines and a daemon's start a few dozen, so this is weeks of a daemon that works
/// and the last hours of one that says the same thing in a loop.
pub const CAP_BYTES: u64 = 16 << 20;

#[expect(
    clippy::disallowed_types,
    reason = "where the daemon's log is, set once as it starts: the panic hook checks it with no handle to the daemon"
)]
static AT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// One rotation at a time: a second thread that finds the log due meanwhile leaves it to the
/// first.
static ROTATING: AtomicBool = AtomicBool::new(false);

/// Says where this daemon's log is: `serve.log` under `state`. Once, as it starts.
pub fn bound(state: &Path) {
    _ = AT.set(state.join(LOG));
}

/// Whether a log of `len` bytes is rotated under a bound of `cap`: at half of it, so the two
/// files together stay within it.
pub const fn due(len: u64, cap: u64) -> bool {
    len >= cap / 2
}

/// Rotates the daemon's log when it is due. A rotation that fails is said on the log as it
/// is and as an event, and tried again at the next check.
pub fn check() {
    let Some(log) = AT.get() else { return };
    if ROTATING.swap(true, Ordering::AcqRel) {
        return;
    }
    let rotated = rotate(log, libc::STDERR_FILENO, &[libc::STDOUT_FILENO], CAP_BYTES);
    ROTATING.store(false, Ordering::Release);
    if let Err(why) = rotated {
        tracing::error!(target: "blaze", "{why}");
        // logged: the event above
        eprintln!("blaze serve: {why}");
    }
}

/// The device and inode of what `fd` is open on, and its length; `None` when it is no regular
/// file.
fn file_of(fd: RawFd) -> Option<(u64, u64, u64)> {
    // SAFETY: an all-zero `stat` is a valid one for `fstat` to fill.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `fstat` writes the `stat` given, which lives across the call; a closed `fd` is
    // its error.
    if unsafe { libc::fstat(fd, &raw mut stat) } != 0 {
        return None;
    }
    (stat.st_mode & libc::S_IFMT == libc::S_IFREG)
        .then(|| (stat.st_dev as u64, stat.st_ino as u64, u64::try_from(stat.st_size).unwrap_or(0)))
}

/// Rotates the log at `log`, which `fd` appends to, once it is [`due`] under `cap`: `log`
/// becomes [`BEFORE`] beside it, a new `log` is made saying so, and `fd` (and each of `also`
/// that is open on the same file) is pointed at the new one. The bytes the log held when it
/// was rotated; `None` when it is not due or `fd` is not open on `log`.
pub fn rotate(log: &Path, fd: RawFd, also: &[RawFd], cap: u64) -> Result<Option<u64>, String> {
    let Some((dev, ino, len)) = file_of(fd) else { return Ok(None) };
    if !due(len, cap) {
        return Ok(None);
    }
    match fs::metadata(log) {
        Ok(at) if at.dev() == dev && at.ino() == ino => {}
        // The daemon's standard error is some other file: not this log.
        _ => return Ok(None),
    }
    let before = log.with_file_name(BEFORE);
    let failed = |doing: &str, why: std::io::Error| {
        // alloc: an error
        format!(
            "the daemon's log {} holds {len} bytes, past half its bound of {cap}, and was not rotated \
             ({doing}): {why}",
            log.display()
        )
    };
    fs::rename(log, &before).map_err(|why| failed("renaming it to `serve.log.1`", why))?;
    let made = (|| {
        let mut new = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(log)?;
        writeln!(
            new,
            "blaze serve: pid {} the log reached {len} bytes (it and the one before it are kept to {cap}) \
             and goes on here; the lines before this are in {}, and what that name held before is deleted",
            std::process::id(),
            before.display()
        )?;
        Ok(new)
    })();
    let new = match made {
        Ok(new) => new,
        Err(why) => {
            // Back where it was: the daemon still appends to it, and the next check tries again.
            _ = fs::rename(&before, log);
            return Err(failed("making the new one", why));
        }
    };
    for target in std::iter::once(fd).chain(also.iter().copied()) {
        if target != fd && file_of(target).is_none_or(|(d, i, _)| (d, i) != (dev, ino)) {
            continue;
        }
        // SAFETY: `dup2` of an open file onto a descriptor this process has open on the old
        // log: atomic, and a writer on another thread writes one file or the other.
        if unsafe { libc::dup2(new.as_raw_fd(), target) } < 0 {
            return Err(failed("pointing the daemon's output at the new one", std::io::Error::last_os_error()));
        }
    }
    Ok(Some(len))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test's directory, removed when the guard drops, and its path.
    fn scratch(name: &str) -> (blaze_core::temp::TempDir, PathBuf) {
        let made = blaze_core::temp::TempDir::for_test(&format!("blaze-serve-log-{name}-"));
        let dir = made.path().to_path_buf();
        (made, dir)
    }

    fn open(log: &Path) -> fs::File {
        fs::OpenOptions::new().create(true).append(true).open(log).unwrap()
    }

    const CAP: u64 = 64 << 10;
    /// A line of the test's daemon, and the most the two files may be past the cap: what is
    /// written between two checks (one line) and the new log's first line.
    const LINE: &[u8] = &[b'x'; 199];
    const SLACK: u64 = 1024;

    /// What the bound promises, read from the directory: the log and the one before it are
    /// together within the cap (and the slack), and no third file is kept.
    fn within(dir: &Path) -> Result<u64, String> {
        let mut total = 0;
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name();
            if name != LOG && name != BEFORE {
                return Err(format!("a third file is kept: {}", name.display()));
            }
            total += entry.metadata().unwrap().len();
        }
        if total > CAP + SLACK {
            return Err(format!("the log and the one before it hold {total} bytes, past their bound of {CAP}"));
        }
        Ok(total)
    }

    /// The invariant: however much a daemon says, `serve.log` and `serve.log.1` together stay
    /// within the cap; every line said is in one of them until it is the older file's turn to
    /// go; and each new log starts by saying where the lines before it are.
    #[test]
    fn the_log_and_the_one_before_it_stay_within_the_cap() {
        let (_made, dir) = scratch("bound");
        let log = dir.join(LOG);
        let file = open(&log);
        let also = open(&log);
        let other = open(&dir.join("elsewhere"));
        fs::remove_file(dir.join("elsewhere")).unwrap();
        let mut rotations = 0;
        for n in 0..2_000u64 {
            // The daemon's own writes go through its descriptor, whatever it now points at.
            (&file).write_all(format!("{n:08} ").as_bytes()).unwrap();
            (&file).write_all(LINE).unwrap();
            (&file).write_all(b"\n").unwrap();
            let rotated = rotate(&log, file.as_raw_fd(), &[also.as_raw_fd(), other.as_raw_fd()], CAP).unwrap();
            if let Some(len) = rotated {
                rotations += 1;
                assert!(due(len, CAP) && len < CAP / 2 + SLACK, "{len}");
                let new = fs::read_to_string(&log).unwrap();
                assert!(new.starts_with("blaze serve: pid ") && new.contains(BEFORE), "{new}");
                assert_eq!(new.lines().count(), 1);
                // The line that tipped it is the last of the file before.
                let before = fs::read_to_string(dir.join(BEFORE)).unwrap();
                assert!(before.lines().last().unwrap().starts_with(&format!("{n:08} ")), "line {n}");
                // A second descriptor on the old log follows; one on another file does not.
                (&also).write_all(b"also\n").unwrap();
                (&other).write_all(b"other\n").unwrap();
                assert!(fs::read_to_string(&log).unwrap().ends_with("also\n"));
            }
            within(&dir).unwrap();
        }
        assert!(rotations >= 10, "{rotations} rotations of 2,000 lines of 209 bytes under {CAP}");
        // The newest line is in the log.
        assert!(fs::read_to_string(&log).unwrap().contains("00001999 "));
        _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_oracle_catches_a_log_that_is_never_rotated() {
        let (_made, dir) = scratch("plant");
        let log = dir.join(LOG);
        let file = open(&log);
        for _ in 0..2_000 {
            (&file).write_all(LINE).unwrap();
        }
        let said = within(&dir).expect_err("400,000 bytes under a bound of 65,536");
        assert!(said.contains("past their bound"), "{said}");
        _ = fs::remove_dir_all(&dir);
    }

    /// A descriptor that is not open on the log (the daemon run in a terminal, or under a
    /// service manager) is left alone, and so is a log not yet due.
    #[test]
    fn a_standard_error_that_is_not_the_log_is_left_alone() {
        let (_made, dir) = scratch("elsewhere");
        let log = dir.join(LOG);
        let file = open(&log);
        (&file).write_all(&[b'x'; 100]).unwrap();
        assert_eq!(rotate(&log, file.as_raw_fd(), &[], CAP), Ok(None), "not due");
        let other = open(&dir.join("terminal"));
        (&other).write_all(&vec![b'x'; CAP as usize]).unwrap();
        assert_eq!(rotate(&log, other.as_raw_fd(), &[], CAP), Ok(None));
        assert!(!dir.join(BEFORE).exists());
        _ = fs::remove_dir_all(&dir);
    }
}
