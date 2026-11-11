//! Which driver this process's waits use, chosen once. Linux has two: the thread's wait ring
//! (`wait/ring.rs`) and the direct system calls of a process that runs where io_uring is
//! refused (`wait/direct.rs`). The sandbox launcher marks what it launches ([`SANDBOXED`] in
//! the program's environment, which every program it runs inherits), and blaze's `main` reads
//! the mark before it starts a thread or waits ([`choose_from_env`]). The ring is never tried
//! to find out (rule no-fallback): with no mark the ring is used, and a kernel or a filter
//! that refuses it fails the wait with an error naming it.
//!
//! The choice is the process's, set once: a second choice, or one made after a thread has
//! waited, is a panic naming both. A process that never chooses (a test binary, a library's
//! user) waits on the ring from its first wait on. Elsewhere than Linux there is one driver,
//! and the choice changes nothing.

use std::{
    ffi::OsString,
    fmt,
    sync::atomic::{AtomicU8, Ordering},
};

/// The variable blaze's sandbox launcher sets, to `1`, in the environment of the program it
/// runs on Linux (`blaze-cargo/src/sandbox/linux.rs`): the program is inside the sandbox, whose
/// seccomp filter refuses io_uring, so a blaze there (as the `rustc` a build script runs) waits
/// with direct system calls.
pub const SANDBOXED: &str = "__BLAZE_SANDBOXED";

/// A driver of the kernel side of this process's waits.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Waits {
    /// Each thread's own io_uring (`wait/ring.rs`).
    Ring,
    /// Direct system calls (`wait/direct.rs`): `futex`, `ppoll`, `pidfd_open`, `waitid`,
    /// `clock_nanosleep`.
    Direct,
}

const UNCHOSEN: u8 = 0;
const RING: u8 = 1;
const DIRECT: u8 = 2;
/// A thread waited with nothing chosen: the ring, from then on.
const WAITED: u8 = 3;

static CHOSEN: AtomicU8 = AtomicU8::new(UNCHOSEN);

/// [`SANDBOXED`] with a value the launcher never sets.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error, off every hot path
pub struct MarkRefused(pub OsString);

impl fmt::Display for MarkRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "`{SANDBOXED}={}` in this process's environment: blaze's sandbox launcher sets it to `1` and to nothing \
             else, and which system calls this process waits with is not guessed from another value (rule no-fallback)",
            self.0.to_string_lossy()
        )
    }
}

impl std::error::Error for MarkRefused {}

/// Chooses this process's driver from its environment: [`Waits::Direct`] where the sandbox
/// launcher marked it ([`SANDBOXED`] is `1`), [`Waits::Ring`] where the variable is not set.
/// What `main` calls first, before any thread exists and before any wait.
///
/// # Errors
///
/// The variable set to anything but `1`: nothing is chosen.
///
/// # Panics
///
/// As [`choose`].
#[allow(
    clippy::disallowed_methods,
    reason = "the launcher's mark, read once at the entry point, typed, with no default for another value"
)]
pub fn choose_from_env() -> Result<Waits, MarkRefused> {
    let waits = match std::env::var_os(SANDBOXED) {
        None => Waits::Ring,
        Some(mark) if mark == "1" => Waits::Direct,
        Some(mark) => return Err(MarkRefused(mark)),
    };
    choose(waits);
    Ok(waits)
}

/// Chooses this process's driver, once.
///
/// # Panics
///
/// When a driver was chosen already, or a thread of the process has waited: a wait under way
/// on one driver can't be woken through the other.
pub fn choose(waits: Waits) {
    let to = match waits {
        Waits::Ring => RING,
        Waits::Direct => DIRECT,
    };
    match CHOSEN.compare_exchange(UNCHOSEN, to, Ordering::SeqCst, Ordering::SeqCst) {
        Ok(_) => {}
        Err(WAITED) => panic!(
            "blaze_core::wait::choose({waits:?}): a thread of this process has waited already, on its wait ring; the \
             driver of a process's waits is chosen once, in `main`, before any thread waits"
        ),
        Err(was) => panic!(
            "blaze_core::wait::choose({waits:?}): this process's waits were given to {} already; the driver is chosen \
             once, in `main`",
            match was {
                DIRECT => "direct system calls",
                _ => "the wait ring",
            }
        ),
    }
}

/// The driver this process's waits use: the one chosen, or the ring from a first wait made
/// with none chosen, after which none can be.
#[cfg(target_os = "linux")]
#[inline]
pub(crate) fn chosen() -> Waits {
    match CHOSEN.load(Ordering::Relaxed) {
        DIRECT => Waits::Direct,
        RING | WAITED => Waits::Ring,
        _ => first_wait(),
    }
}

#[cfg(target_os = "linux")]
#[cold]
fn first_wait() -> Waits {
    match CHOSEN.compare_exchange(UNCHOSEN, WAITED, Ordering::SeqCst, Ordering::SeqCst) {
        Err(DIRECT) => Waits::Direct,
        _ => Waits::Ring,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The mark's refusal names the variable, the value and the rule. (The choice itself is
    /// the process's: its tests are the ones that run a process, `wait/filtered.rs`'s
    /// `every_public_wait_ends_in_a_sandboxed_process` and blaze-graph's
    /// `a_sandboxed_process_parks_its_workers_with_no_io_uring`.)
    #[test]
    fn a_mark_of_another_value_is_refused_naming_it() {
        let said = MarkRefused(OsString::from("yes")).to_string();
        assert!(said.contains("`__BLAZE_SANDBOXED=yes`") && said.contains("no-fallback"), "{said}");
    }
}
