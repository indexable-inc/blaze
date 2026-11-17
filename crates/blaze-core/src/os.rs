//! The authority to touch the real operating system, as a capability (rule 75 "nothing
//! ambient", decision D104; the plan port27/bstd-capabilities.md, P2).
//!
//! A [`Driver`](crate::Driver) over the real disk, the system's wall clock and (later) a
//! spawn or a signal reach past every simulation: code that can make one can read and write
//! anything the process may. So making one takes an [`Os`], the root of all real-OS
//! authority, in the pattern of `blaze_own::Heap` (D102): the field is private, so no code
//! makes an `Os`; the one value is [`OS`], and only two functions hand out a reference to it:
//! [`Os::process`], which the program's entry calls, and [`testing::os`], a test's entry (the
//! `testing` feature). Every function that touches the real system takes `&Os` from its
//! caller, up to the entry.
//!
//! The proof that no code makes a native driver without the root is the type: `Os` has a
//! private field and implements neither `Clone`, `Copy` nor `Default`, so outside this file
//! the only `&Os` there is comes from [`Os::process`] or [`testing::os`]; [`native_in`] and
//! [`system_wall_ns_in`] take one. Which code may call [`Os::process`] is held by the reach
//! ratchet (tools/ratchets/ambient-reach.txt, counted like `Heap::process()`: free inside the
//! top-level `fn main` of an entry file, counted everywhere else), not by a flag at run time.

use std::{io, path::Path, time::SystemTime, time::UNIX_EPOCH};

use crate::Native;

/// The root of the authority to touch the real operating system: a native driver over a
/// root ([`native_in`]), the system's wall clock ([`system_wall_ns_in`]); later the spawn of
/// a real process and a signal's delivery. Holding a `&Os` is the proof of that authority.
/// A simulated world needs none: it grants nothing real.
pub struct Os {
    _only_here: (),
}

/// The one root.
static OS: Os = Os { _only_here: () };

impl Os {
    /// The process's root, for the program's entry only: `bstd::rt::run`, `rt::launch` and
    /// `guest::start`'s caller in bstd, a plain binary's `fn main`. A library never calls it;
    /// it takes `&Os` from its caller. Which code may call it is held by the reach ratchet
    /// (tools/ratchets/ambient-reach.txt counts every call outside an entry file's top-level
    /// `fn main`, per crate), not by a flag at run time, as `blaze_own::Heap::process` is:
    /// taking it costs nothing.
    #[inline]
    pub const fn process() -> &'static Os {
        &OS
    }
}

impl core::fmt::Debug for Os {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("Os")
    }
}

/// This target's [`Native`] driver over `root`, made with the root of real-OS authority
/// `_os` (the parameter is the proof; it carries nothing). `threads` sizes the pool kqueue
/// and IOCP run file calls on; io_uring's off-ring errands start their own.
#[cfg(target_os = "linux")]
pub fn native_in(_os: &Os, root: &Path, _threads: usize) -> io::Result<Native> {
    crate::uring::Uring::new(root)
}
/// This target's [`Native`] driver over `root`, made with the root of real-OS authority
/// `_os` (the parameter is the proof; it carries nothing). `threads` sizes the pool kqueue
/// runs file calls on.
#[cfg(target_os = "macos")]
pub fn native_in(_os: &Os, root: &Path, threads: usize) -> io::Result<Native> {
    crate::kqueue::Kqueue::new(root, threads)
}
/// This target's [`Native`] driver over `root`, made with the root of real-OS authority
/// `_os` (the parameter is the proof; it carries nothing). `threads` sizes the pool IOCP
/// runs file calls on.
#[cfg(windows)]
pub fn native_in(_os: &Os, root: &Path, threads: usize) -> io::Result<Native> {
    crate::iocp::Iocp::new(root, threads)
}
/// This target's [`Native`] driver over `root`, made with the root of real-OS authority
/// `_os`. `threads` is not used: WASI starts none.
#[cfg(target_os = "wasi")]
pub fn native_in(_os: &Os, root: &Path, threads: usize) -> io::Result<Native> {
    Ok(crate::threads::Threads::new(root, threads))
}

/// The system's wall clock now, in nanoseconds since the Unix epoch, read with the root of
/// real-OS authority `_os`: what every native [`Driver::wall_ns`](crate::Driver::wall_ns)
/// reads, and what code that stamps the real disk outside a driver (a start's look at its own
/// executable, before any loop) compares those stamps with. A question on a loop asks its
/// driver instead, so a simulation answers it. A reading before the epoch is a clock no stamp
/// can be compared with: it panics, naming the reading, as a window measured from it would
/// trust every stamp.
pub fn system_wall_ns_in(_os: &Os) -> u64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        // A reading past 2554 saturates: every stamp is then within its window, none trusted.
        Ok(since) => u64::try_from(since.as_nanos()).unwrap_or(u64::MAX),
        Err(error) => panic!(
            "the system's wall clock reads {:?} before the Unix epoch: no file's change time can be compared with it",
            error.duration()
        ),
    }
}

/// A test's root of real-OS authority: the process's [`Os`], for a test, which is its own
/// entry. The one way to it for tests; a crate turns it on in its dev-dependencies
/// (`blaze-core = { workspace = true, features = ["testing"] }`). A use outside test code is a
/// bug: the reach ratchet counts it as it counts [`Os::process`].
#[cfg(any(test, feature = "testing"))]
pub mod testing {
    /// The root, for a test.
    pub fn os() -> &'static super::Os {
        &super::OS
    }
}

#[cfg(test)]
mod tests {
    use super::{Os, system_wall_ns_in, testing};

    /// 2020-01-01T00:00:00Z in nanoseconds since the Unix epoch: any honest reading of the
    /// wall clock today is past it.
    const YEAR_2020_NS: u64 = 1_577_836_800_000_000_000;

    /// One root: the entry's and a test's are the same value, so a test's world is made by
    /// the same authority as a program's. And `system_wall_ns_in` is the one home of the wall
    /// clock: the old `system_wall_ns` delegates to it, so the two read within a second of
    /// each other. The control: a reading of 0 (a stub, or a clock read from nowhere that
    /// answers nothing) fails the year-2020 floor, so the check is not vacuous.
    /// Expected: `test os::tests::the_root_is_one_value_and_the_test_root_is_it ... ok`.
    #[test]
    fn the_root_is_one_value_and_the_test_root_is_it() {
        assert!(core::ptr::eq(Os::process(), testing::os()), "the process's root and a test's root are one value");
        let given = system_wall_ns_in(testing::os());
        let old = crate::system_wall_ns();
        assert!(given > YEAR_2020_NS, "the wall clock read through the root says {given} ns, before 2020");
        assert!(given.abs_diff(old) < 1_000_000_000, "the root's clock {given} and the old one {old} differ by a second or more");
        assert_eq!(format!("{:?}", Os::process()), "Os");
    }

    /// A native driver is made through the root over a fresh directory: the same driver
    /// `native` made, now with its authority named.
    /// Expected: `test os::tests::a_native_driver_is_made_through_the_root ... ok`.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_native_driver_is_made_through_the_root() {
        let dir = std::env::temp_dir().join(format!("blaze-core-os-root-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a fresh directory under the system's temp dir");
        let made = super::native_in(testing::os(), &dir, 1);
        let _ = std::fs::remove_dir_all(&dir);
        made.expect("a native driver over a fresh directory, made through the root");
    }
}
