//! The daemon's hard memory ceiling (docs/memory-budget.md section 7, "The ceiling, enforced
//! past admission", Hard): the most its footprint may be, never more than the machine budget,
//! held past admission so that blaze never takes the machine down.
//!
//! - On macOS the kernel holds it with a fatal memorystatus limit on the daemon's own pid
//!   ([`crate::raw::memory::hold_own_footprint`], private), where XNU lets this process set one
//!   (root, or entitled `com.apple.private.memorystatus`).
//! - On Linux the kernel holds it with `memory.max` (and `memory.high` at 15/16) of the daemon's
//!   own cgroup v2, where one is delegated ([`blaze_core::cgroup::hold_own`]). The footprint is
//!   then that cgroup's charge ([`crate::raw::memory::own_footprint`]), not the process's
//!   resident set.
//! - Everywhere the stop's guard (`serve::stop`) reads the footprint every [`EVERY`] and ends
//!   the daemon with [`blaze_core::exit::Exit::MemoryCeiling`] once it passes the ceiling, after
//!   writing what it held by tag and what each thread was inside. Where the kernel holds it to
//!   none, that watch is the bound, and the ceiling's line says so and why.
//!
//! Each line the daemon says of its ceiling begins `blaze serve: pid N`, then one of [`SAYS`],
//! [`NEAR`], [`EASED`] or [`STOPPED`]: the client a daemon hung up on reads them from its log
//! (`cli::daemon::ceiling_said`) to say it was stopped at its ceiling, or killed by the kernel
//! at its limit, rather than that it hung up.

use std::{
    fmt,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

/// How often the guard reads the footprint against the ceiling.
pub const EVERY: Duration = Duration::from_millis(100);

/// The ceiling in bytes, 0 before one is set: what the guard holds the footprint to.
static CEILING: AtomicU64 = AtomicU64::new(0);

/// The ceiling the guard holds the footprint to, 0 for none ([`hold`]).
pub fn ceiling() -> u64 {
    CEILING.load(Ordering::Relaxed)
}

/// What the line a ceiling is set with says after `blaze serve: pid N`, then its bytes:
/// `blaze serve: pid 7 memory ceiling 51539607552 bytes (48 GiB), held by ...; the ceiling is
/// ...`.
pub const SAYS: &str = " memory ceiling ";
/// What the line of a footprint that came within 1/8 of the ceiling says after the pid, then
/// the footprint's bytes and the ceiling's: `... memory near its ceiling: 45097156608 bytes of
/// 51539607552 bytes (42 of 48 GiB)`.
pub const NEAR: &str = " memory near its ceiling: ";
/// What the line of a footprint back under 3/4 of the ceiling, after one near it, says.
pub const EASED: &str = " memory eased from its ceiling: ";
/// What the line of a daemon its own watch ends says after the pid, then the footprint's
/// bytes and the ceiling's: `... stopped at its memory ceiling: 51539607553 bytes passed
/// 51539607552 bytes (...)`.
pub const STOPPED: &str = " stopped at its memory ceiling: ";
/// What a ceiling's line says the kernel holds it with on macOS, after its bytes.
pub const BY_JETSAM: &str = "held by the kernel (jetsam: ";
/// What a ceiling's line says the kernel holds it with on Linux, after its bytes.
pub const BY_CGROUP: &str = "held by the kernel (memory.max of ";
/// What a ceiling's line says before what made the ceiling (the budget's `from`).
pub const FROM: &str = "; the ceiling is ";

/// Bytes in GiB as a person reads them: whole where they are, else to two places (`48`,
/// `47.93`).
#[derive(Clone, Copy, Debug)]
pub struct Gib(pub u64);

impl fmt::Display for Gib {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const GIB: u64 = 1 << 30;
        match self.0 % GIB {
            0 => write!(f, "{}", self.0 / GIB),
            _ => write!(f, "{:.2}", self.0 as f64 / GIB as f64),
        }
    }
}

/// What holds the daemon to its ceiling besides its own watch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Bound {
    /// macOS: a fatal memorystatus limit of this many MiB on the daemon's own pid.
    #[cfg(target_os = "macos")]
    Jetsam { mib: i32 },
    /// Linux: `memory.max` of the daemon's own cgroup v2, this directory.
    #[cfg(target_os = "linux")]
    Cgroup { dir: std::path::PathBuf },
    /// Nothing of the kernel's: why. The daemon's own watch is the bound.
    Watch { why: String },
}

impl fmt::Display for Bound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let every = EVERY.as_millis();
        match self {
            #[cfg(target_os = "macos")]
            Bound::Jetsam { mib } => write!(
                f,
                "{BY_JETSAM}a fatal memorystatus limit of {mib} MiB on its footprint, active and inactive), and by its own watch every {every} ms"
            ),
            #[cfg(target_os = "linux")]
            Bound::Cgroup { dir } => write!(
                f,
                "{BY_CGROUP}{}, memory.high at 15/16 of it, no swap), and by its own watch every {every} ms",
                dir.display()
            ),
            Bound::Watch { why } => write!(
                f,
                "held by the daemon's own watch alone, every {every} ms, as the kernel holds it to no limit: {why}"
            ),
        }
    }
}

/// Holds the daemon to `bytes`, its ceiling, made of `from` (its budget's `from`): the kernel's
/// limit where this process may set one ([`Bound`]), and the guard's watch ([`ceiling`]). Said
/// once in the daemon's log, with which bound applies. Called at the daemon's start, and again
/// whenever the ceiling moves: the kernel's limit is set at the ceiling itself, never at an
/// allowance under it. An error where the kernel's limit could be set and was not, naming the
/// call: never a ceiling the line says is held that is not.
pub fn hold(bytes: u64, from: &str) -> anyhow::Result<Bound> {
    let bound = kernel(bytes)?;
    CEILING.store(bytes, Ordering::Relaxed);
    let pid = std::process::id();
    // alloc: once a ceiling is set
    let said = format!("blaze serve: pid {pid}{SAYS}{bytes} bytes ({} GiB), {bound}{FROM}{from}", Gib(bytes));
    eprintln!("{said}");
    tracing::info!(target: "blaze", "{said}");
    Ok(bound)
}

#[cfg(target_os = "macos")]
fn kernel(bytes: u64) -> anyhow::Result<Bound> {
    match crate::raw::memory::hold_own_footprint(bytes) {
        Ok(mib) => Ok(Bound::Jetsam { mib }),
        // XNU's `memorystatus_control` refuses it to a process neither root nor entitled
        // (bsd/kern/kern_memorystatus.c): no limit of the kernel's is this daemon's to set, which
        // its line says. Any other refusal is a call made wrong, and stops the start.
        Err(error) if error.raw_os_error() == Some(libc::EPERM) => Ok(Bound::Watch {
            // alloc: once a ceiling is set
            why: format!(
                "memorystatus_control(MEMORYSTATUS_CMD_SET_MEMLIMIT_PROPERTIES) on its own pid was refused ({error}): \
                 XNU lets only root or a process entitled com.apple.private.memorystatus set a memory limit"
            ),
        }),
        Err(error) => Err(anyhow::anyhow!(
            "memorystatus_control(MEMORYSTATUS_CMD_SET_MEMLIMIT_PROPERTIES) setting the daemon's fatal memory limit of {} MiB: {error}",
            bytes >> 20
        )),
    }
}

#[cfg(target_os = "linux")]
fn kernel(bytes: u64) -> anyhow::Result<Bound> {
    use anyhow::Context as _;
    let held = blaze_core::cgroup::hold_own(bytes).with_context(|| {
        // alloc: an error's message, once
        format!("holding the daemon's own cgroup to its memory ceiling of {bytes} bytes")
    })?;
    Ok(match held {
        blaze_core::cgroup::OwnHeld::Held { dir } => {
            // The kernel holds the ceiling on this cgroup's charge, so that charge is the
            // footprint from here on (rule one-memory-measure).
            crate::raw::memory::count_own_cgroup(&dir);
            Bound::Cgroup { dir }
        }
        blaze_core::cgroup::OwnHeld::NotDelegated(why) => Bound::Watch { why },
    })
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn kernel(_: u64) -> anyhow::Result<Bound> {
    Ok(Bound::Watch {
        // alloc: once a ceiling is set
        why: "blaze sets the kernel's memory limit on macOS and Linux alone".to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gib_is_whole_where_it_is() {
        assert_eq!(Gib(48 << 30).to_string(), "48");
        assert_eq!(Gib((48 << 30) + (1 << 29)).to_string(), "48.50");
        assert_eq!(Gib(0).to_string(), "0");
    }
}
