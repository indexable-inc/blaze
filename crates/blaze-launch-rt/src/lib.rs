//! blaze's launcher under the `edit` goal, step 2 (docs/v2/plan/design/launcher.md): the
//! drivers. `blaze-launch` decides; this crate does.
//!
//! | module | what |
//! |---|---|
//! | [`sys`] | memory that code runs from and one thread writes: macOS `MAP_JIT` with the thread's write switch, Linux a memfd mapped twice; a thread's stack bounds |
//! | [`arch`] | the launcher's own machine code, AArch64 and x86-64: a gate's entry, the lazy stubs, the resolver |
//! | [`table`] | what a program thread touches: the slot words, the published epoch, a stub's wait for code, a safepoint's report of the thread's own stack |
//! | [`drive`] | [`Driver`]: the core over real memory, each `Write` carried out in order; an edit as one transaction |
//! | [`control`] | the control thread: the patch protocol over a Unix socket, the stubs' needs, the threads' reports |
//!
//! Who touches what, which is the whole of the design's safety:
//!
//! - **The control thread** owns the [`Driver`] and is the only writer of code, words, gate
//!   flags and the epoch. Code is written only where nothing can be running (ranges the core
//!   handed out and no word names yet); a word, a flag and the epoch are each one aligned
//!   atomic store.
//! - **A program thread** loads a word and jumps (machine code), waits in a stub, and reports
//!   its own stack. It writes nothing the control thread reads but the queue.
//!
//! Not built here, each named where it is missing: the scan of a thread from outside
//! (`thread_suspend`, a signal), so a thread that never reaches a safepoint holds every batch;
//! unwind entries and symbols for placed code (`Write::Forget` takes nothing back yet); the
//! store's records ([`drive::Records`] is the seam); `Probe` and `Restart`. The crate is on
//! `std`, not bstd, for the reason blaze-launch's documentation gives.
//!
//! Built for AArch64 macOS and for x86-64 and AArch64 Linux; on any other target the crate is
//! empty, so nothing can ask it for a launcher it does not have.

#![cfg(any(
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")),
))]
#![feature(allocator_api)]

pub mod arch;
pub mod control;
pub mod drive;
pub mod sys;
pub mod table;

#[cfg(test)]
mod tests;

pub use control::{ControlError, run};
pub use drive::{Config, Driver, EditError, Error, Records};
pub use table::{Birth, Bound, Event, Mutator, Shared, Told};
