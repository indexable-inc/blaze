//! What a build dir (or a standard library's store) says of itself, in `.blaze-record`: the
//! checkout it builds, and when it was last used. The daemon writes it as a job takes the
//! dir's lease ([`touch`]); the collector reads it to find a dir whose checkout is gone or that
//! has not been used for its TTL, and `blaze clean --list` shows it.
//!
//! Written whole: a temporary beside it (`.blaze-record.<pid>.<count>.tmp`), synced, then renamed over
//! it, so a reader sees the old record or the new one, never a part.

use std::{
    io::Write as _,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use blaze_serde::{Deserialize, Serialize};

/// The record's file in its directory.
pub const RECORD: &str = ".blaze-record";

/// The record's format. A record of another is refused, naming both; the collector then
/// leaves its directory alone ([`crate::plan::Keep::Refused`]).
pub const FORMAT: u32 = 1;

/// A record is rewritten for a use only once its time is this old (ns), so a burst of builds
/// writes it once.
pub const TOUCH_EVERY_NS: u64 = 60_000_000_000;

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Record {
    pub format: u32,
    /// The workspace root the dir is the build dir of, absolute; none for a standard library's
    /// store, which is of a blaze build, not a checkout.
    pub checkout: Option<String>,
    /// When it was last used, ns since the Unix epoch.
    pub used_ns: u64,
    /// Each directory in it a build used (a profile's, `debug`, or a target's, `<triple>`), by
    /// name, with when: a build that writes nothing still counts as a use of it. Sorted by name.
    /// Absent in a record written before it was: none.
    #[serde(default)]
    pub profiles: Vec<(String, u64)>,
}

pub fn encode(record: &Record) -> Vec<u8> {
    // A record is a struct of a number, an optional string and a number: serializing it fails
    // for no input.
    serde_json::to_vec_pretty(record).expect("a record serializes")
}

pub fn decode(bytes: &[u8]) -> Result<Record, String> {
    let record: Record = serde_json::from_slice(bytes)
        .map_err(|error| format!("it does not read as a record: {error}"))?;
    if record.format != FORMAT {
        return Err(format!(
            "it is of format {}, and this blaze reads format {FORMAT}",
            record.format
        ));
    }
    Ok(record)
}

pub fn path_of(dir: &Path) -> PathBuf {
    dir.join(RECORD)
}

/// `dir`'s record; `None` when it has none.
pub fn read(dir: &Path) -> Result<Option<Record>, String> {
    let path = path_of(dir);
    match std::fs::read(&path) {
        Ok(bytes) => decode(&bytes).map(Some).map_err(|why| format!("{}: {why}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("reading {}: {error}", path.display())),
    }
}

/// A temporary's name for `base` that no other writer, in this process or another, has at once:
/// `<base>.<pid>.<count>.tmp`.
pub fn temporary_name(base: &str) -> String {
    static MADE: AtomicU64 = AtomicU64::new(0);
    format!("{base}.{}.{}.tmp", std::process::id(), MADE.fetch_add(1, Ordering::Relaxed))
}

/// Writes `bytes` at `path`, whole: a temporary beside it, synced, renamed over it. A
/// directory that is not there (yet) is made and the write tried once more.
pub fn write_whole(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let dir = path.parent().ok_or_else(|| format!("{} is in no directory", path.display()))?;
    let base = path.file_name().map(|it| it.to_string_lossy().into_owned()).unwrap_or_default();
    let mut last = None;
    for _ in 0..2 {
        let temporary = dir.join(temporary_name(&base));
        let written = (|| -> std::io::Result<()> {
            let mut file =
                std::fs::OpenOptions::new().write(true).create_new(true).open(&temporary)?;
            file.write_all(bytes)?;
            // `touch` writes a record as a job takes a lease, so this sync is on a build's path.
            // durable: review: the collector decides what to delete from these files; a torn one must not be read
            file.sync_all()?;
            std::fs::rename(&temporary, path)
        })();
        match written {
            Ok(()) => return Ok(()),
            Err(error) => {
                _ = std::fs::remove_file(&temporary);
                let missing = error.kind() == std::io::ErrorKind::NotFound;
                last = Some(error);
                if !missing || std::fs::create_dir_all(dir).is_err() {
                    break;
                }
            }
        }
    }
    let error = last.map_or_else(|| "no attempt".to_owned(), |it| it.to_string());
    Err(format!("writing {}: {error}", path.display()))
}

/// Writes `record` as `dir`'s, whole.
pub fn write(dir: &Path, record: &Record) -> Result<(), String> {
    write_whole(&path_of(dir), &encode(record))
}

/// cargo's and the Cache Directory Tagging Specification's tag: a directory holding it is a
/// cache, which backups skip and which `blaze clean` may remove.
pub const CACHEDIR_TAG: &str = "CACHEDIR.TAG";
pub const CACHEDIR_SIGNATURE: &str = "Signature: 8a477f597d28d172789f06886806bc55";

/// Writes `dir`'s `CACHEDIR.TAG` when it has none.
pub fn tag_cache_dir(dir: &Path) -> Result<(), String> {
    let path = dir.join(CACHEDIR_TAG);
    if path.is_file() {
        return Ok(());
    }
    let text = format!(
        "{CACHEDIR_SIGNATURE}\n# This file is a cache directory tag created by blaze.\n# For information about cache directory tags see https://bford.info/cachedir/\n"
    );
    write_whole(&path, text.as_bytes())
}

/// Whether `dir` says it is a build dir blaze or cargo made: a lease, a record, or a
/// `CACHEDIR.TAG` that starts with the signature. Only such a directory is removed by
/// `blaze clean`.
pub fn is_marked(dir: &Path) -> bool {
    if crate::lease::has_lease(dir) || path_of(dir).is_file() {
        return true;
    }
    std::fs::read(dir.join(CACHEDIR_TAG))
        .is_ok_and(|it| it.starts_with(CACHEDIR_SIGNATURE.as_bytes()))
}

/// `dir` was used at `now_ns` for `checkout`, and so were its directories `profiles`: its record
/// rewritten when the checkout differs or a time is [`TOUCH_EVERY_NS`] old. A record that does
/// not read is written afresh, and the error it had is returned with the write's outcome so the
/// caller can say it.
pub fn touch(
    dir: &Path,
    checkout: Option<&str>,
    profiles: &[&str],
    now_ns: u64,
) -> Result<Option<String>, String> {
    let (had, unread) = match read(dir) {
        Ok(had) => (had, None),
        Err(why) => (None, Some(why)),
    };
    let recent = |at: u64| now_ns.saturating_sub(at) < TOUCH_EVERY_NS;
    let fresh = had.as_ref().is_some_and(|it| {
        it.checkout.as_deref() == checkout
            && recent(it.used_ns)
            && profiles
                .iter()
                .all(|name| it.profiles.iter().any(|(it, at)| it == name && recent(*at)))
    });
    if fresh {
        return Ok(unread);
    }
    let mut kept = had.map(|it| it.profiles).unwrap_or_default();
    kept.retain(|(it, _)| !profiles.contains(&it.as_str()));
    kept.extend(profiles.iter().map(|name| ((*name).to_owned(), now_ns)));
    kept.sort();
    write(
        dir,
        &Record {
            format: FORMAT,
            checkout: checkout.map(str::to_owned),
            used_ns: now_ns,
            profiles: kept,
        },
    )?;
    Ok(unread)
}

/// When `record` says its directory `name` was last used.
pub fn profile_used(record: &Record, name: &str) -> Option<u64> {
    record.profiles.iter().find(|(it, _)| it == name).map(|(_, at)| *at)
}

/// Now, in ns since the Unix epoch: a driver's clock, which the planner is given.
pub fn now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |it| u64::try_from(it.as_nanos()).unwrap_or(u64::MAX))
}

/// A clock that is never set and counts time asleep, in ns from an arbitrary start: what the
/// collector checks the wall clock against ([`crate::Collector::advance_clocks`]).
/// `CLOCK_BOOTTIME` on Linux, `CLOCK_MONOTONIC` on macOS (which counts sleep there); `None`
/// elsewhere, where the wall clock goes unchecked but for the ledger's check.
#[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
#[allow(unsafe_code)]
pub fn boot_ns() -> Option<u64> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let clock = libc::CLOCK_BOOTTIME;
    #[cfg(target_os = "macos")]
    let clock = libc::CLOCK_MONOTONIC;
    // SAFETY: `clock_gettime` writes a `timespec`, plain data.
    let mut at: libc::timespec = unsafe { std::mem::zeroed() };
    // SAFETY: as above; `at` is alive across the call.
    if unsafe { libc::clock_gettime(clock, &mut at) } != 0 {
        return None;
    }
    #[allow(
        clippy::unnecessary_cast,
        clippy::useless_conversion,
        reason = "the field widths differ by target"
    )]
    let (secs, nanos) = (at.tv_sec as u64, at.tv_nsec as u64);
    Some(secs.saturating_mul(1_000_000_000).saturating_add(nanos))
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
pub fn boot_ns() -> Option<u64> {
    None
}
