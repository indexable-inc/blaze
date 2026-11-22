//! blaze's launcher under the `edit` goal, step 2 (docs/v2/plan/design/launcher.md): the
//! drivers. `blaze-launch` decides; this crate does.
//!
//! | module | what |
//! |---|---|
//! | [`sys`] | memory that code runs from and one thread writes, with a writable data region beside it ([`sys::Code::data_base`]): macOS an anonymous write view `mach_vm_remap`s read+execute at the code address, Linux a memfd mapped twice; a thread's stack bounds |
//! | [`arch`] | the launcher's own machine code, AArch64 and x86-64: a gate's entry, the lazy stubs, the resolver |
//! | [`observe`] | the per-function observe stubs (count, mark, hook) a slot's word points at for a while, and their body word; emitted into the code area, their tables in the data region |
//! | [`fault`] | the fault handler's body: the retired-body or wait a faulting address names, reported to stderr before the process stops (installed by [`signals`]) |
//! | [`signals`] | the launcher's SIGSEGV/SIGBUS handler, installed before the program's first instruction and chained to the program's own action (the program's `sigaction` and `signal` are the launcher's, seqlocked rows); the one table of symbols the launcher defines for a placed program (`own`) |
//! | [`table`] | what a program thread touches: the slot words, the published epoch, a stub's wait for code, a safepoint's report of the thread's own stack |
//! | [`drive`] | [`Driver`]: the core over real memory, each `Write` carried out in order; an edit as one transaction |
//! | [`control`] | the control thread: the patch protocol over a Unix socket, the stubs' needs, the threads' reports |
//! | [`records`] | [`Loaded`]: the records the daemon sends, checked against their ids; the program's data and statics in a mapping of their own; outside symbols by `dlsym` |
//! | [`tls`] | the program's thread-locals: a table a thread, a copy made on first touch, reached through a record's pool |
//! | [`threads`] | the program's own threads told to the core: its `pthread_create` announces each birth, a key's destructor each end |
//! | [`symbols`] | the names of placed code where the program's own backtrace looks: one more image of `dl_iterate_phdr`, whose file is a symbol table (Linux) |
//! | [`unwind`] | a placed function's unwind entry, told to the system's unwinder and taken back when the body is freed |
//! | [`retired`] | code given back and made inaccessible: an append-only table a fault handler reads without a lock, and the stop line it writes |
//! | [`sample`] | heat by sampling (D100 point 4): macOS stops one program thread at a time from the control thread (`thread_suspend`, `thread_get_state`), Linux a per-thread CPU timer's signal; one `SampleEntry` each into a fixed ring, the frame record read only inside the thread's stack bounds |
//! | [`waits`] | which thread waits for what since when, a seqlocked row per thread in the region the monitor reads: why a run hangs |
//! | [`zygote`] | `blaze test`'s system calls: the zygote forks one child per test (one thread at each fork, counted), a child's setup steps and its report on descriptor 3, the pipes polled and children reaped under each test's deadline; the decisions are blaze-run's `zygote` and `child` machines |
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
//! thread-locals on x86-64 (`tls`); `Probe` and `Restart`. The crate is on
//! `std`, not bstd, for the reason blaze-launch's documentation gives.
//!
//! Built for AArch64 macOS and for x86-64 and AArch64 Linux; on any other target the crate is
//! empty, so nothing can ask it for a launcher it does not have.

#![cfg(any(
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")),
))]
#![feature(allocator_api)]
// The launcher's own root word on Linux (run_mode/thread_locals.rs `ROOT`).
#![cfg_attr(target_os = "linux", feature(thread_local))]
// `#![blaze::trust]` below names blaze's tool; inert under rustc.
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::trust(
    unsafe,
    reason = "a launcher runs code it placed itself: memory that code runs from (an anonymous write view remapped read+execute at the code address on macOS, a memfd mapped twice on Linux), its own machine code (gates, stubs, the resolver, the observe stubs), slot words, body words and thread-locals the program's threads read, the counter table and mark bitmap they write, `dlsym` and the system's unwinder; the control thread is the only writer of code and of every word, and writes code only where nothing can be running"
)]

pub mod arch;
pub mod code_unwind;
pub mod control;
pub mod drive;
pub mod fault;
pub mod native_lib;
pub mod observe;
pub mod poll;
pub mod records;
pub mod retired;
pub mod run_mode;
pub mod sample;
pub mod signals;
pub mod stub_wait;
pub mod symbols;
pub mod sys;
pub mod table;
pub mod telemetry;
pub mod threads;
pub mod tls;
pub mod jit_debug;
pub mod unwind;
pub mod unwind_reg;
pub mod viewer;
pub mod waits;
pub mod zygote;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod poll_tests;
#[cfg(test)]
mod unwind_pairs_tests;

pub use control::{ControlError, Ready, Start, ready, run};
pub use drive::{Config, Driver, EditError, Error, Place, REACH, Records};
pub use records::Loaded;
pub use stub_wait::{Answer, Waiters};
pub use table::{Birth, Event, Mutator, Shared, Told};
