//! What bstd still takes from Rust's `std`, all of it, in one module.
//!
//! bstd is `#![no_std]`: every other module names only `core`, `alloc` and blaze's crates. The
//! few things Rust's `std` alone provides today are named here and nowhere else, and this
//! module is a row of S4's ban list (`harness/v2/nostd.json`): it shrinks as `bstd::rt` (the
//! process's entry, unwinding and exit on `blaze-sys`) is built, and is deleted with the row
//! (docs/v2/plan/design/sys.md, "Runtime" and "Migration").
//!
//! | here | used by | replaced by |
//! |---|---|---|
//! | [`RootPath`] | `NativeWorld::root`, which blaze-core's drivers take as a `std::path::Path` | a `Dir` handle from the start block, when the drivers open beneath a handle |
//! | [`process_id`] | `Host`'s pid on a native `Io` | `blaze_sys`'s `getpid` |
//! | [`thread_mark`], [`panicking`] | the debug check that a one-thread value and a started task end on the thread they began on (`sync::home`, `Cx`) | the thread's id from bstd's own thread start, and its unwinder's flag |
//! | [`exit`] | `Host::exit` on a native `Io` | `blaze_sys`'s `exit_group` |
//! | [`catch_unwind`], [`resume_unwind`] | a task's panic failing only its task; the simulated exit's unwind | bstd's own unwinder entry (on `library/unwind`) |
//! | [`LazyLock`], [`OnceLock`] | `bstd::sync`'s `Lazy` and `Once` | the same two on bstd's one wait |
//! | [`run`], [`Exit`] | the entry of a program of the blaze edition (`bstd::entry`): the native world made from the process's arguments, environment and start directory, and the status handed to Rust std's `lang_start` | bstd's own entry on `blaze-sys`: the start block read directly, no `lang_start` |
//! | [`write_out`] | `Out` on a native `Io`, both streams | an effect of blaze-core's drivers that writes to a handle the process was started with |
//! | `Owns` for `HashMap`, `HashSet`, `Mutex`, `RwLock`, `OnceLock`, `LazyLock` | `bstd::own`'s reachability walk, for code that still holds one of Rust std's | nothing: gone with the last use of those types |

extern crate std;

pub(crate) use std::{
    panic::{catch_unwind, resume_unwind},
    path::Path as RootPath,
};

use alloc::{string::{String, ToString as _}, sync::Arc};
use core::{fmt::Write as _, time::Duration};

use crate::{
    args::ArgList,
    cx::Cx,
    entry::{Outcome, Status, run_on},
    env::Vars,
    io::{Io, Limits, NativeWorld},
    mem::Heap,
    task::threads::{Plants, Topology},
};

// The command line and environment where the kernel left them.
#[expect(unsafe_code, reason = "the start block's pointers, read as the kernel left them, each with its SAFETY")]
pub(crate) mod block;
// The running image's linker id, read in place from its mapped header.
#[expect(unsafe_code, reason = "the image's header and notes, read where the loader mapped them, each with its SAFETY")]
pub(crate) mod image;
// Whether a standard stream is a terminal, and its size.
pub(crate) mod tty;
// The launcher of `#[bstd::main]`: the run spec read from file descriptor 3 (crate::grant).
mod launch;
pub use launch::{SPEC_MARK, SPEC_MARK_VALUE, launch, launch_with};
// A test's native world, from a test's root of real-OS authority (`bstd::test::native`).
#[cfg(any(test, feature = "testing"))]
pub(crate) mod testing_native;

/// The threads the native driver runs file calls on where it has a pool (kqueue, IOCP): a
/// bound bstd picks for a program's entry, named (std.md, "The second rule").
pub(crate) const FILE_THREADS: usize = 4;

/// What a program of the blaze edition hands Rust std's `lang_start`: its [`Status`], as the
/// one type of bstd that implements Rust std's `Termination`. Gone with `lang_start`, when
/// bstd's own entry reads the start block.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Exit(pub Status);

impl std::process::Termination for Exit {
    fn report(self) -> std::process::ExitCode {
        std::process::ExitCode::from(self.0.0)
    }
}

/// The process's entry for a `main` that takes its world: the native [`Io`] over the
/// directory the process was started in, with its command line, its environment and
/// [`Limits::USUAL`]; then [`run_in`]. What a failure says goes to standard error as one line,
/// `error: <what>`, and the process exits with the status (0, or 1 for a failure).
///
/// What the world is made from, each read here once and nowhere else:
/// - the command line: an argument that is not UTF-8 stops the program before `main`, naming
///   its place (`Args` holds text);
/// - the environment: a variable whose name or value is not UTF-8 is not in `Env`, which
///   holds text (a stated gap until `Env` holds bytes);
/// - the start directory, the root every `Path` is beneath;
/// - the seed of `Rng`, from the system's randomness as Rust std's hasher keys draw it;
/// - the host's name, empty: neither blaze-core nor Rust std asks the system for it yet.
///
/// `main`'s context is the root of the process's `Threads` pool (task/threads.rs): one worker
/// per core of the machine's [`topology`], read when a scope first asks about the workers,
/// each worker's thread started by the first wake that reaches it ([`pool::lazy`]). A program
/// that spawns nothing starts no thread, and one that runs k tasks at once starts about k. When `main` returns, the effects it left in flight are drained (within
/// [`POOL_STOP`], else the process fails naming them) and the process exits at once: the
/// kernel frees the threads and tables, with no teardown.
pub fn run<R: Outcome>(main: impl FnOnce(&Cx, Io) -> R) -> Exit {
    run_with(blaze_core::Os::process(), main)
}

/// [`run`] with the root of real-OS authority handed down by the program's entry (its `fn
/// main`, where `blaze_core::Os::process()` is taken, plan port27/bstd-capabilities.md P3): the
/// form an entry calls. [`run`], which reaches the root itself, goes in batch B1.
pub fn run_with<R: Outcome>(os: &'static blaze_core::Os, main: impl FnOnce(&Cx, Io) -> R) -> Exit {
    run_io(native(os), main)
}

/// [`run_with`] for an entry that is also handed the process's heap as a grant (D102, D120):
/// the closure form of `fn main(cx: &Cx, .., heap: Heap)` for a plain binary's `main` (the
/// `blaze` command's). The heap is taken by bstd's entry (`entry::process_heap`), once; nothing
/// below `main` reaches for it, it is handed down.
pub fn run_with_heap<R: Outcome>(os: &'static blaze_core::Os, main: impl FnOnce(&Cx, Io, Heap) -> R) -> Exit {
    let heap = crate::entry::process_heap();
    run_io(native(os), move |cx, io| main(cx, io, heap))
}

/// [`run_with_heap`] with the run's telemetry level chosen by the program from its arguments
/// (D178; [`crate::telemetry`], "Wiring", decision 3): `level` is called once, after the
/// command line is read and before the world's workers or the pool exist, so every worker's
/// cell is made at that level and nothing is raised after the start. It is a plain `fn`: it
/// sees the arguments and nothing else (no environment, no clock; nothing ambient). From
/// [`Level::Spans`](crate::telemetry::Level::Spans) the report is written to standard error as
/// text when the run ends: `main` returned, failed, or panicked; at `Totals` it is kept and
/// written nowhere.
pub fn run_with_heap_at<R: Outcome>(
    os: &'static blaze_core::Os,
    level: fn(&crate::args::Args) -> crate::telemetry::Level,
    main: impl FnOnce(&Cx, Io, Heap) -> R,
) -> Exit {
    let heap = crate::entry::process_heap();
    let world = process_start().and_then(|block| {
        let args = ArgList::process(block);
        // clone: the arguments' list (a borrow of the start block), read once by `level`.
        let chosen = level(&crate::args::Args::new(args.clone()));
        let io = open_native(os, args, Vars::process(block), start_dir(), u8::MAX)?;
        let alloc = heap.shared();
        Ok((with_telemetry(io, chosen, alloc)?, AtExit { level: chosen, alloc }))
    });
    let (world, at_exit) = match world {
        Ok((io, at_exit)) => (Ok(io), Some(at_exit)),
        Err(why) => (Err(why), None),
    };
    run_io_with(world, at_exit, move |cx, io| main(cx, io, heap))
}

/// `io` over a `Workers` of the same setup whose cells keep telemetry at `level` in `alloc`
/// (the bare one `open_native` made is dropped: one sparse reservation, nothing backed).
fn with_telemetry(io: Io, level: crate::telemetry::Level, alloc: blaze_own::SharedAlloc<'static>) -> Result<Io, String> {
    let setup = io.workers().setup();
    // alloc: an error's message.
    let mut opts = crate::workers::WorkersOpts::new(setup).map_err(|error| error.to_string())?;
    opts.telemetry(level, alloc);
    // alloc: an error's message.
    let workers = crate::workers::Workers::new(opts).map_err(|error| error.to_string())?;
    Ok(io.on_workers(workers))
}

/// What the runtime does with the run's telemetry when it ends: the level the cells keep and
/// the allocator the report is made in (the process heap's shared handle).
#[derive(Clone, Copy, Debug)]
pub(crate) struct AtExit {
    level: crate::telemetry::Level,
    alloc: blaze_own::SharedAlloc<'static>,
}

/// The run's report, armed from the pool's start to the exit: [`Reporting::end`] writes it
/// on the ways out that return; its drop writes it, as `Panicked`, on the unwind of a `main`
/// that panicked. Below `Spans` nothing is gathered or written.
struct Reporting {
    at: AtExit,
    world: Io,
    began_ns: u64,
    begun: blaze_meter::Process,
    armed: bool,
}

impl Reporting {
    fn new(at: AtExit, world: &Io) -> Reporting {
        // clone: the world kept to gather its workers' cells at the end.
        Reporting { at, world: world.clone(), began_ns: ns_of(world.now()), begun: blaze_meter::Process::now(), armed: true }
    }

    /// Gathers every worker's cell (each stopped at now) into one report, finishes it as
    /// `exit`, reconciles it against the process's totals, and writes its text to standard
    /// error. Once: a second call does nothing.
    fn end(&mut self, exit: crate::telemetry::Exit) -> Result<(), String> {
        use crate::telemetry::{Level, Report, ReportOpts};
        if !core::mem::replace(&mut self.armed, false) || !self.at.level.keeps(Level::Spans) {
            return Ok(());
        }
        let workers = self.world.workers();
        let now = ns_of(self.world.now());
        let opts = ReportOpts { level: self.at.level, most_workers: workers.cells() };
        // alloc: an error's message (each `to_string` below).
        let mut report = Report::new(opts, self.at.alloc).map_err(|error| error.to_string())?;
        let mut merged = Ok(());
        workers
            .each_made_held(|w| {
                if let Some((tally, cell)) = w.telemetry() {
                    cell.stop(now);
                    if merged.is_ok() {
                        merged = report.merge(tally, cell);
                    }
                }
            })
            // alloc: an error's message.
            .map_err(|busy| alloc::format!("bstd::rt: a worker's cell was held at the run's end: {busy:?}"))?;
        merged.map_err(|error| error.to_string())?;
        report.finish(exit).map_err(|error| error.to_string())?;
        report.reconcile(now.saturating_sub(self.began_ns), &self.begun, &blaze_meter::Process::now()).map_err(|error| error.to_string())?;
        let mut text = blaze_own::Vec::new_in(self.at.alloc);
        crate::telemetry::text_into(&report, &mut text).map_err(|error| error.to_string())?;
        write_out(crate::out::Stream::Err, &[&text[..]])
    }
}

impl Drop for Reporting {
    fn drop(&mut self) {
        if self.armed
            && let Err(why) = self.end(crate::telemetry::Exit::Panicked)
        {
            error_line(&why);
        }
    }
}

/// A reading of the world's clock as nanoseconds.
fn ns_of(at: Duration) -> u64 {
    u64::try_from(at.as_nanos()).unwrap_or(u64::MAX)
}

/// [`run_with_heap`] for a program that keeps state per worker (D171, [`crate::workers`]):
/// `register` adds every slot, with the process's heap to name their allocators, before the
/// world or the pool exists, and what it returns (the keys) is handed to `main` beside it.
/// `main`'s context and every task under it hold cells of these `Workers`. A registration
/// refused (too many slots, a cell too large) or a reservation the system refuses fails the
/// process before `main`, naming it.
pub fn run_with_workers<S, R: Outcome>(
    os: &'static blaze_core::Os,
    setup: crate::workers::WorkersSetup,
    register: impl FnOnce(&mut crate::workers::WorkersOpts, Heap) -> S,
    main: impl FnOnce(&Cx, Io, Heap, S) -> R,
) -> Exit {
    let heap = crate::entry::process_heap();
    let made = crate::workers::WorkersOpts::new(setup).and_then(|mut opts| {
        let keys = register(&mut opts, heap);
        crate::workers::Workers::new(opts).map(|workers| (workers, keys))
    });
    let (world, keys) = match made {
        Ok((workers, keys)) => (native(os).map(|io| io.on_workers(workers)), Some(keys)),
        // alloc: an error's message, once.
        Err(error) => (Err(error.to_string()), None),
    };
    // `keys` is `None` only with the world refused, so `main` never runs without them.
    run_io(world, move |cx, io| match keys {
        Some(keys) => main(cx, io, heap, keys),
        None => unreachable!("bstd::rt::run_with_workers: main ran over a refused world"),
    })
}

/// [`run`] in the world `world` made, or saying why it could not be: the one home of the
/// pool's start and stop for a program's `main` ([`run`], and `launch`'s spec-made world).
pub(crate) fn run_io<R: Outcome>(world: Result<Io, String>, main: impl FnOnce(&Cx, Io) -> R) -> Exit {
    run_io_with(world, None, main)
}

/// [`run_io`] with the run's telemetry written at its end ([`run_with_heap_at`]): after the
/// drain, on every way out of `main` (returned, failed, panicked: the last by the guard's drop).
/// A report that cannot be gathered or written fails the run, naming why.
pub(crate) fn run_io_with<R: Outcome>(world: Result<Io, String>, at_exit: Option<AtExit>, main: impl FnOnce(&Cx, Io) -> R) -> Exit {
    // alloc: once per process, what a failure says.
    let mut said = String::new();
    let status = match world {
        Ok(io) => {
            let running = pool::lazy(Plants::NONE);
            // clone: the world kept past `main`, to drain what it left in flight.
            let kept = io.clone();
            let mut reporting = at_exit.map(|at| Reporting::new(at, &kept));
            let status = run_on(io, running.pool().clone(), &mut said, main);
            // A command answers and exits (rule 70): every scope of `main` has ended, so no
            // task runs; what the driver still holds is drained, and then the process exits
            // with no teardown of what the kernel frees (the pool's threads, their stacks, the
            // driver's threads and tables, the frames' reservations).
            let until = blaze_core::wait::Deadline::after(kept.now(), POOL_STOP);
            let status = match kept.drain(until) {
                Ok(()) => status,
                Err(stopped) => {
                    if !said.is_empty() {
                        said.push_str("; ");
                    }
                    let _ = write!(said, "{stopped}");
                    Status::FAILED
                }
            };
            let exit = if status.0 == Status::OK.0 { crate::telemetry::Exit::Returned } else { crate::telemetry::Exit::Failed };
            let status = match reporting.as_mut().map(|reporting| reporting.end(exit)) {
                Some(Err(why)) => {
                    if !said.is_empty() {
                        said.push_str("; ");
                    }
                    let _ = write!(said, "bstd::telemetry: the run's report: {why}");
                    Status::FAILED
                }
                Some(Ok(())) | None => status,
            };
            if !said.is_empty() {
                error_line(&said);
            }
            exit_now(status, running)
        }
        Err(why) => {
            said.push_str(&why);
            Status::FAILED
        }
    };
    if !said.is_empty() {
        error_line(&said);
    }
    Exit(status)
}

/// The process ends with `status` at once: the world's stream buffers were written when
/// `main` ended (`entry::run_on`), and `error_line` writes through, so nothing waits in a
/// buffer here, and nothing is torn
/// down that the kernel frees: the pool `running` is not stopped or joined, and no destructor
/// runs. `std::process::exit` is libc's `exit`: the C library's atexit handlers and stdio
/// flush, nothing of Rust's.
fn exit_now(status: Status, running: pool::Running) -> ! {
    core::mem::forget(running);
    std::process::exit(i32::from(status.0))
}

/// The process cannot go on (a worker's thread the system refused, from any thread): `said`
/// on standard error, and the process exits failed at once, as at the end of `main`.
pub(crate) fn fatal(said: &str) -> ! {
    error_line(said);
    std::process::exit(i32::from(Status::FAILED.0))
}

/// How long the pool's workers have to leave their loops at the end of `main`: they leave
/// once they find nothing to run, so only a task that ignores its cancellation takes longer.
pub(crate) const POOL_STOP: Duration = Duration::from_secs(1);

/// A task that will not end while its scope must (crate::task, "A scope never ends while a
/// task of it runs"): the process stops, naming it, rather than free what the task borrows.
pub(crate) fn hang(scope: &'static str, task: &'static str) -> ! {
    // alloc: once, the process's last words.
    let mut said = String::new();
    let _ = write!(
        said,
        "bstd::task {task} of a scope in {scope} is still running past its deadline and the abandon wait, with the \
         scope cancelled: a hang (rule bounded-progress); the process stops rather than free what the task borrows"
    );
    error_line(&said);
    std::process::abort()
}

/// The machine as the pool sees it (task/threads.rs, `Topology`): its cores in groups that
/// share a last-level cache, each pinned (Linux) or given its class's QoS (macOS).
///
/// - Linux: the cores this process may run on (`sched_getaffinity`), grouped by the
///   `shared_cpu_list` of each core's highest-level cache in
///   `/sys/devices/system/cpu/cpuN/cache/indexM/` (`index3` on EPYC; Graviton names its
///   last level the same way where the firmware exposes it, else its L2 groups it).
/// - macOS: one group per L2 cluster of each performance level (`hw.perflevelN.logicalcpu`,
///   `hw.perflevelN.cpusperl2`): level 0's foreground, the others background. macOS has no
///   pinning; a worker's QoS steers it.
pub(crate) fn topology() -> Result<Topology, TopologyError> {
    machine::read()
}

/// Why the machine's cores could not be read.
#[derive(Debug)]
pub(crate) struct TopologyError {
    pub(crate) what: &'static str,
    pub(crate) why: String,
}

impl core::fmt::Display for TopologyError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "bstd::rt: the machine's cores ({}): {}", self.what, self.why)
    }
}

pub(crate) mod pool {
    //! Starting and stopping the pool's threads: the driver of task/threads.rs.

    // `std` is this module's (bstd is `no_std`: rt.rs is where Rust std is named).
    use super::std::{format, string::String, thread};

    use alloc::string::ToString as _;
    use core::time::Duration;

    use blaze_core::wait::{Clock, Deadline, Monotonic};

    use super::machine;
    use crate::task::threads::{Hung, Plants, Pool, Reader, Starter, Topology};

    /// Each worker's stack: tasks run on it to their end, and a waiting worker runs its own
    /// queued tasks on top of the wait, so it is deeper than a thread's usual 2 MiB.
    pub(crate) const WORKER_STACK: usize = 16 << 20;

    /// A pool and the stop of its threads, each started by the first wake that reaches its
    /// worker ([`starter`]).
    pub(crate) struct Running {
        pool: Pool,
    }

    /// Why the pool did not start: the worker, and what refused.
    #[derive(Debug)]
    pub(crate) struct StartError {
        pub(crate) worker: u32,
        pub(crate) why: String,
    }

    impl core::fmt::Display for StartError {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            write!(f, "bstd::rt: the pool's worker {} did not start: {}", self.worker, self.why)
        }
    }

    impl From<super::TopologyError> for StartError {
        fn from(error: super::TopologyError) -> StartError {
            StartError { worker: 0, why: error.to_string() }
        }
    }

    /// The machine's pool, nothing of it made: the topology is read when the pool is first
    /// asked about its workers ([`read`]), a worker's thread starts at the first wake that
    /// reaches it ([`starter`]), so a program that makes no scope reads no topology, one that
    /// spawns nothing makes no thread, and one with k tasks runnable at once makes about k.
    pub(crate) fn lazy(plants: Plants) -> Running {
        lazy_on(read, starter, plants)
    }

    /// [`lazy`] with its reader and starter given: a test's, to count starts or fix the
    /// topology.
    pub(crate) fn lazy_on(read: Reader, starter: Starter, plants: Plants) -> Running {
        Running { pool: Pool::lazy(read, Monotonic::new(), plants, starter) }
    }

    /// The pool of `topology`, every worker's thread started now (each parked until a wake
    /// reaches it): a test's pool, whose tests want every worker up.
    pub(crate) fn start(topology: &Topology, plants: Plants) -> Result<Running, StartError> {
        let running = Running { pool: Pool::new(topology, Monotonic::new(), plants, starter) };
        running.pool.start_all();
        Ok(running)
    }

    /// The pool's [`Reader`](crate::task::threads::Reader): the machine's topology, or the
    /// process stops naming why (a pool cannot be made without its cores).
    fn read() -> Topology {
        match super::topology() {
            Ok(topology) => topology,
            Err(why) => super::fatal(&why.to_string()),
        }
    }

    /// The pool's [`Starter`](crate::task::threads::Starter): the calling thread (the waker
    /// whose wake first reached worker `at`) starts that worker's thread and returns at once,
    /// without waiting for it. A burst of spawns fans out one worker at a time (task/idle.rs),
    /// so the threads come up as the work asks, each by the worker before it. The woken task
    /// waits on a worker that is parked until its thread runs (task/idle.rs). A thread the
    /// system refuses, or a placement it refuses, stops the process naming the worker (rule
    /// no-fallback): a pool with a hole in it would hold the tasks reserved to that worker
    /// forever.
    pub(crate) fn starter(pool: &Pool, at: u32) {
        // clone: one handle on the pool per thread.
        let mine = pool.clone();
        let spawned = thread::Builder::new()
            // alloc: once per thread, its name.
            .name(format!("bstd-worker-{at}"))
            .stack_size(WORKER_STACK)
            .spawn(move || {
                if let Some((cpu, class)) = mine.placement(at)
                    && let Err(why) = machine::place(cpu, class)
                {
                    super::fatal(&StartError { worker: at, why }.to_string());
                }
                blaze_mem::frame::size_shelf(blaze_mem::frame::ShelfSize::for_threads(mine.workers() as usize));
                mine.work(at);
            });
        // The handle is dropped: the thread is detached, and a stop waits on the workers'
        // own count of who left ([`Pool::wait_left`]), not on a join.
        if let Err(error) = spawned {
            super::fatal(&StartError { worker: at, why: error.to_string() }.to_string());
        }
    }

    impl Running {
        pub(crate) fn pool(&self) -> &Pool {
            &self.pool
        }

        /// Stops every worker once it finds nothing to run, and waits until the last has left
        /// its loop (it wakes this wait) or `within` passes; the rest are named, with the task
        /// each still runs. A pool whose threads never began stops at once. A program's end
        /// does not call this: it exits ([`super::exit_now`]).
        pub(crate) fn stop(self, within: Duration) -> Result<(), Hung> {
            self.pool.stop();
            let until = Deadline::after(self.pool.clock().now(), within);
            // A wait the kernel refused ends the wait early; what it waited for is then read
            // as it is: every worker that has not left is named below, so nothing is lost.
            let _left = self.pool.wait_left(until);
            match self.pool.hung() {
                None => Ok(()),
                Some(hung) => Err(hung),
            }
        }
    }
}

#[expect(unsafe_code, reason = "the kernel's calls that place a thread and read the machine's cores: each states its contract")]
mod machine {
    //! The machine's cores, and placing a worker on one (rule 45 for macOS's).

    // `std` is this module's (bstd is `no_std`: rt.rs is where Rust std is named).
    use super::std::{self, format, string::String, vec::Vec as StdVec};

    use alloc::{alloc::Global, string::ToString as _};

    use blaze_own::Vec;

    use super::TopologyError;
    use crate::task::threads::{Class, Group, Topology};

    fn failed(what: &'static str, why: impl core::fmt::Display) -> TopologyError {
        TopologyError { what, why: why.to_string() }
    }

    /// `0-3,8,10-11` as its numbers.
    pub(super) fn cpu_list(text: &str) -> Result<StdVec<u32>, String> {
        let mut cpus = StdVec::new();
        for part in text.trim().split(',').filter(|part| !part.is_empty()) {
            let (low, high) = match part.split_once('-') {
                Some((low, high)) => (low, high),
                None => (part, part),
            };
            let low: u32 = low.parse().map_err(|error| format!("{part:?} in {text:?}: {error}"))?;
            let high: u32 = high.parse().map_err(|error| format!("{part:?} in {text:?}: {error}"))?;
            if high < low {
                return Err(format!("{part:?} in {text:?}: a range that runs backwards"));
            }
            cpus.extend(low..=high);
        }
        Ok(cpus)
    }

    #[cfg(target_os = "linux")]
    mod linux {
        /// glibc's and musl's `cpu_set_t`: 1024 bits.
        pub(super) const SET_BYTES: usize = 128;

        unsafe extern "C" {
            /// `sched_getaffinity(2)`: the cores thread `pid` (0: the caller) may run on.
            pub(super) fn sched_getaffinity(pid: i32, size: usize, mask: *mut u8) -> i32;
            /// `sched_setaffinity(2)`: lets thread `pid` (0: the caller) run only on `mask`.
            pub(super) fn sched_setaffinity(pid: i32, size: usize, mask: *const u8) -> i32;
        }
    }

    /// The machine's cores in last-level-cache groups, all foreground, pinned.
    #[cfg(target_os = "linux")]
    pub(super) fn read() -> Result<Topology, TopologyError> {
        let mut mask = [0u8; linux::SET_BYTES];
        // SAFETY: `mask` is `SET_BYTES` writable bytes, the size passed.
        if unsafe { linux::sched_getaffinity(0, linux::SET_BYTES, mask.as_mut_ptr()) } != 0 {
            return Err(failed("sched_getaffinity", std::io::Error::last_os_error()));
        }
        let allowed = |cpu: u32| (cpu as usize) < linux::SET_BYTES * 8 && mask[cpu as usize / 8] & (1 << (cpu % 8)) != 0;
        let online = std::fs::read_to_string("/sys/devices/system/cpu/online").map_err(|error| failed("/sys/devices/system/cpu/online", error))?;
        let online = cpu_list(&online).map_err(|why| failed("/sys/devices/system/cpu/online", why))?;
        let mut groups: StdVec<StdVec<u32>> = StdVec::new();
        let mut seen: StdVec<u32> = StdVec::new();
        for cpu in online.into_iter().filter(|&cpu| allowed(cpu)) {
            if seen.contains(&cpu) {
                continue;
            }
            let shared = last_level(cpu)?;
            let group: StdVec<u32> = shared.into_iter().filter(|&it| allowed(it) && !seen.contains(&it)).collect();
            seen.extend(group.iter().copied());
            if !group.is_empty() {
                groups.push(group);
            }
        }
        Ok(made(groups.into_iter().map(|cpus| (Class::Foreground, cpus)), true))
    }

    /// The cores sharing `cpu`'s highest-level cache: itself alone when sysfs names no cache.
    #[cfg(target_os = "linux")]
    fn last_level(cpu: u32) -> Result<StdVec<u32>, TopologyError> {
        let mut best: Option<(u32, String)> = None;
        for index in 0..16 {
            let dir = format!("/sys/devices/system/cpu/cpu{cpu}/cache/index{index}");
            let Ok(level) = std::fs::read_to_string(format!("{dir}/level")) else {
                break;
            };
            let level: u32 = level.trim().parse().map_err(|error| failed("a cache's level", error))?;
            let shared = std::fs::read_to_string(format!("{dir}/shared_cpu_list")).map_err(|error| failed("a cache's shared_cpu_list", error))?;
            if best.as_ref().is_none_or(|(kept, _)| level > *kept) {
                best = Some((level, shared));
            }
        }
        match best {
            Some((_, shared)) => cpu_list(&shared).map_err(|why| failed("a cache's shared_cpu_list", why)),
            None => Ok(std::vec![cpu]),
        }
    }

    /// Pins the calling thread to `cpu` (Linux has no QoS class: `class` places nothing).
    #[cfg(target_os = "linux")]
    pub(super) fn place(cpu: u32, _class: Class) -> Result<(), String> {
        let mut mask = [0u8; linux::SET_BYTES];
        let at = cpu as usize;
        if at >= linux::SET_BYTES * 8 {
            return Err(format!("core {cpu} is past cpu_set_t's 1024"));
        }
        mask[at / 8] |= 1 << (at % 8);
        // SAFETY: `mask` is `SET_BYTES` readable bytes, the size passed; 0 is this thread.
        match unsafe { linux::sched_setaffinity(0, linux::SET_BYTES, mask.as_ptr()) } {
            0 => Ok(()),
            _ => Err(format!("sched_setaffinity to core {cpu}: {}", std::io::Error::last_os_error())),
        }
    }

    #[cfg(target_os = "macos")]
    pub(super) mod darwin {
        /// `QOS_CLASS_USER_INITIATED` (`<sys/qos.h>`): what a foreground worker runs at.
        pub(crate) const QOS_USER_INITIATED: u32 = 0x19;
        /// `QOS_CLASS_UTILITY`: a background worker, which the scheduler keeps to efficiency
        /// cores when the performance cores are busy.
        pub(crate) const QOS_UTILITY: u32 = 0x11;

        /// `sysctlbyname(3)`, and `pthread_set_qos_class_self_np` (`<pthread/qos.h>`):
        /// Darwin's own call, not POSIX (rule 45): it is how a thread says what kind of work
        /// it does, which is what places it on a performance or an efficiency core; there is
        /// no affinity call on macOS. Both are found at their first call through blaze-sys's
        /// table (`blaze_sys::macos::cold`), not bound at load: a program that starts no
        /// worker pays for neither. Their symbols are tested to exist there and in rt's tests.
        pub(crate) use blaze_sys::macos::cold::{pthread_set_qos_class_self_np, sysctlbyname};
    }

    /// A sysctl's 32-bit value, by name.
    #[cfg(target_os = "macos")]
    fn sysctl(name: &'static core::ffi::CStr) -> Result<u32, TopologyError> {
        let mut value: u32 = 0;
        let mut len = size_of::<u32>();
        // SAFETY: `name` is NUL-terminated; `value` is `len` writable bytes; nothing is set.
        let said = unsafe {
            darwin::sysctlbyname(name.as_ptr(), (&raw mut value).cast(), &raw mut len, core::ptr::null_mut(), 0)
        };
        if said != 0 || len != size_of::<u32>() {
            return Err(failed("sysctlbyname", format!("{name:?}: {}", std::io::Error::last_os_error())));
        }
        Ok(value)
    }

    /// The machine's cores: a group per L2 cluster of each performance level.
    #[cfg(target_os = "macos")]
    pub(super) fn read() -> Result<Topology, TopologyError> {
        let levels = sysctl(c"hw.nperflevels")?;
        let mut groups = StdVec::new();
        let mut next = 0;
        for level in 0..levels.min(2) {
            let (cpus, per_l2) = match level {
                0 => (sysctl(c"hw.perflevel0.logicalcpu")?, sysctl(c"hw.perflevel0.cpusperl2")?),
                _ => (sysctl(c"hw.perflevel1.logicalcpu")?, sysctl(c"hw.perflevel1.cpusperl2")?),
            };
            let class = if level == 0 { Class::Foreground } else { Class::Background };
            let per_l2 = per_l2.clamp(1, cpus.max(1));
            let mut left = cpus;
            while left > 0 {
                let size = left.min(per_l2);
                groups.push((class, (next..next + size).collect::<StdVec<u32>>()));
                next += size;
                left -= size;
            }
        }
        Ok(made(groups.into_iter(), true))
    }

    /// Gives the calling thread its class's QoS (macOS places by QoS, not by core).
    #[cfg(target_os = "macos")]
    pub(super) fn place(_cpu: u32, class: Class) -> Result<(), String> {
        let qos = match class {
            Class::Foreground => darwin::QOS_USER_INITIATED,
            Class::Background => darwin::QOS_UTILITY,
        };
        // SAFETY: a plain call on the calling thread with a valid class and priority 0.
        match unsafe { darwin::pthread_set_qos_class_self_np(qos, 0) } {
            0 => Ok(()),
            errno => Err(format!("pthread_set_qos_class_self_np({qos:#x}): errno {errno}")),
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    pub(super) fn read() -> Result<Topology, TopologyError> {
        let cores = std::thread::available_parallelism().map_err(|error| failed("available_parallelism", error))?;
        let cores = u32::try_from(cores.get()).unwrap_or(u32::MAX);
        let mut topology = Topology::flat(cores);
        topology.pin = false;
        Ok(topology)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    pub(super) fn place(_cpu: u32, _class: Class) -> Result<(), String> {
        Ok(())
    }

    fn made(groups: impl Iterator<Item = (Class, StdVec<u32>)>, pin: bool) -> Topology {
        let mut out = Vec::new_in(Global);
        for (class, cpus) in groups {
            let mut own = Vec::with_capacity_in(cpus.len(), Global);
            for cpu in cpus {
                own.push(cpu);
            }
            out.push(Group { class, cpus: own });
        }
        Topology { groups: out, pin }
    }

    #[cfg(test)]
    mod tests {
        use super::{cpu_list, std};

        #[test]
        fn a_cpu_list_reads_ranges_and_singles() {
            assert_eq!(cpu_list("0-3,8,10-11\n"), Ok(std::vec![0, 1, 2, 3, 8, 10, 11]));
            assert!(cpu_list("3-1").is_err());
        }

        /// Rule 45: the Darwin calls the pool makes exist, and the QoS call works on this
        /// thread.
        #[cfg(target_os = "macos")]
        #[test]
        fn the_darwin_symbols_exist() {
            use super::darwin;
            // SAFETY: as `place`'s.
            assert_eq!(unsafe { darwin::pthread_set_qos_class_self_np(darwin::QOS_USER_INITIATED, 0) }, 0);
            let topology = super::read().expect("the machine's performance levels");
            assert!(topology.workers() >= 1, "at least one core");
        }
    }
}

/// The process's world, made with the root of real-OS authority `os`, or why it cannot be made.
fn native(os: &blaze_core::Os) -> Result<Io, String> {
    let block = process_start()?;
    open_native(os, ArgList::process(block), Vars::process(block), start_dir(), u8::MAX)
}

/// The root of a world over the directory the process was started in: `.`, so the start reads
/// no directory (std.md, "What a program pays for at start"); a call that needs an absolute
/// path makes it from this when it is made.
pub(crate) fn start_dir() -> &'static RootPath {
    RootPath::new(".")
}

/// Read-only view of the process's start block (arguments and raw environment entries), readable
/// before [`run_with_heap_at`] initializes the runtime (e.g. a thin Unix-socket daemon client).
#[derive(Clone, Copy)]
pub struct StartBlock {
    pub args: &'static [&'static str],
    pub env: &'static [&'static core::ffi::CStr],
}

/// Returns the process's start block without starting the bstd runtime (`None` if non-UTF-8 args
/// or unsupported platform).
pub fn start_block() -> Option<StartBlock> {
    let block = process_start().ok()?;
    Some(StartBlock { args: &block.args, env: &block.env })
}

/// What the process was started with: its command line (each argument UTF-8, or a refusal
/// naming it) and its environment, borrowed where the kernel left them (`block`).
pub(crate) fn process_start() -> Result<&'static block::Block, String> {
    block::read()
}

/// The native world over `root` with `args` and `env`: the process's, or a run spec's; it may
/// read `above` folders above `root` (`u8::MAX`: up to the filesystem's root). Made with the
/// root of real-OS authority `os`, handed down from the entry.
pub(crate) fn open_native(os: &blaze_core::Os, args: ArgList, env: Vars, root: &RootPath, above: u8) -> Result<Io, String> {
    // Global `Trace` recording is off by default (`Limits::QUIET`, lock-free fast-path) so
    // `Cx::span` / `Cx::event` never contend on the global world lock in production runs; set
    // `BLAZE_TRACE=1` (any non-empty value other than `0`) to capture global trace records.
    let limits = match env.get("BLAZE_TRACE") {
        Some(v) if !v.is_empty() && v != "0" => Limits::USUAL,
        _ => Limits::QUIET,
    };
    let world = NativeWorld {
        root,
        above,
        threads: FILE_THREADS,
        env,
        args,
        // alloc: once per process, the host's name (empty: see `run`).
        host_name: Arc::from(""),
        // Drawn at the first `Io::rng`, not here: the start draws no randomness.
        seed: None,
        limits,
    };
    // alloc: an error's message.
    Io::native_in(os, world).map_err(|error| error.to_string())
}

/// `said` on standard error, as `error: <said>` and a line feed. A standard error that
/// cannot be written to changes nothing: the status still says the program failed.
fn error_line(said: &str) {
    use std::io::Write as _;
    let mut err = std::io::stderr().lock();
    let _ = err.write_all(b"error: ").and_then(|()| err.write_all(said.as_bytes())).and_then(|()| err.write_all(b"\n"));
}

/// `parts`, in order, on the process's `stream`, with the operating system when this
/// returns: all of them or the host's refusal as it says it. On unix they go to the file
/// descriptor itself in vectored writes, one system call when the kernel takes them whole:
/// the world's own buffer ([`crate::out`]) already made them a buffer's worth, and Rust std's
/// `LineWriter` in between would split a chunk that does not end in a line feed into two
/// writes and copy it. What std's handle holds is written first, so anything printed through
/// it stays before. Elsewhere it goes through std's handles, flushed here.
#[cfg(unix)]
pub(crate) fn write_out(stream: crate::out::Stream, parts: &[&[u8]]) -> Result<(), String> {
    use std::{
        io::{IoSlice, Write as _},
        os::fd::FromRawFd as _,
    };
    let fd = match stream {
        crate::out::Stream::Out => {
            // alloc: an error's message.
            std::io::stdout().lock().flush().map_err(|error| error.to_string())?;
            1
        }
        crate::out::Stream::Err => 2,
    };
    #[expect(
        unsafe_code,
        reason = "a File over the process's standard stream, never closed: writes go to the descriptor \
                  with no buffer of std's between"
    )]
    // SAFETY: descriptors 1 and 2 are open for the process's life (the runtime closes neither),
    // and `ManuallyDrop` keeps this `File` from closing it: it is a view, owning nothing.
    let mut file = core::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(fd) });
    // At most this many parts a call: a write, the buffer and one more piece, has two.
    const AT_ONCE: usize = 4;
    let mut at = 0;
    while at < parts.len() {
        let mut slices = [IoSlice::new(&[]); AT_ONCE];
        let mut n = 0;
        while n < AT_ONCE && at + n < parts.len() {
            slices[n] = IoSlice::new(parts[at + n]);
            n += 1;
        }
        at += n;
        let mut left: &mut [IoSlice<'_>] = &mut slices[..n];
        IoSlice::advance_slices(&mut left, 0);
        while !left.is_empty() {
            match file.write_vectored(left) {
                // alloc: an error's message.
                Ok(0) => return Err(std::io::Error::from(std::io::ErrorKind::WriteZero).to_string()),
                Ok(wrote) => IoSlice::advance_slices(&mut left, wrote),
                // A signal broke the call before it wrote: the same write again, as std's
                // `write_all` does; nothing was written, so nothing is said twice.
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                // alloc: an error's message.
                Err(error) => return Err(error.to_string()),
            }
        }
    }
    Ok(())
}

/// See the unix `write_out`: here through std's handles, flushed.
#[cfg(not(unix))]
pub(crate) fn write_out(stream: crate::out::Stream, parts: &[&[u8]]) -> Result<(), String> {
    use std::io::Write as _;
    fn all(mut to: impl std::io::Write, parts: &[&[u8]]) -> std::io::Result<()> {
        for part in parts {
            to.write_all(part)?;
        }
        to.flush()
    }
    let written = match stream {
        crate::out::Stream::Out => all(std::io::stdout().lock(), parts),
        crate::out::Stream::Err => all(std::io::stderr().lock(), parts),
    };
    // alloc: an error's message.
    written.map_err(|error| error.to_string())
}

/// The process's id.
#[blaze::nopanic]
#[blaze::noalloc]
#[blaze::nonblocking]
pub(crate) fn process_id() -> u32 {
    std::process::id()
}

/// Ends the process with `status`.
#[blaze::pure]
pub(crate) fn exit(status: i32) -> ! {
    std::process::exit(status)
}

/// Which OS thread this is: what a one-thread type ([`crate::sync::home`]) and a task's
/// context remember at their start, to check in a debug build that they end where they began.
pub(crate) type ThreadMark = std::thread::ThreadId;

/// The calling thread's mark.
#[blaze::noalloc]
#[blaze::nonblocking]
pub(crate) fn thread_mark() -> ThreadMark {
    std::thread::current().id()
}

/// Whether the calling thread is unwinding: a debug check in a destructor says nothing then,
/// since a second panic would abort the process.
#[blaze::nopanic]
#[blaze::noalloc]
#[blaze::nonblocking]
pub(crate) fn panicking() -> bool {
    std::thread::panicking()
}

// How the containers only Rust's `std` has hold their type arguments, for `bstd::own`'s walk
// (`own.rs` states core's and alloc's, with the rule): the maps behind a pointer they alone
// own, the locks and the once-cells in place, changeable through a shared reference.
#[expect(
    clippy::disallowed_types,
    reason = "how each of std's containers holds its contents is stated once, for the names \
              bstd replaces too: code that still holds one is walked the same way"
)]
#[expect(unsafe_code, reason = "the trusted `Owns` assertions for std's own containers, with their SAFETY")]
mod owns {
    // `super::std`: bstd is `#![no_std]`, so `std` is a name only where `rt` declares it.
    use super::std::{
        collections::{HashMap, HashSet},
        sync::{LazyLock, Mutex, OnceLock, RwLock},
    };

    use crate::own::{Mutably, Owns, Solely};

    // As `own.rs` says of the others: each keeps a value of each type argument alive the way
    // its impl says, and keeps alive nothing else that owns.

    // SAFETY: a `HashMap` keeps each key and value in a table behind a pointer it alone
    // owns, and its hasher `S` in place (walked as solely owned too, which only refuses more).
    unsafe impl<K, V, S> Owns<Solely> for HashMap<K, V, S> {}
    // SAFETY: a `HashSet` is a `HashMap<T, (), S>`: its values behind a pointer it alone owns.
    unsafe impl<T, S> Owns<Solely> for HashSet<T, S> {}
    // SAFETY: a `Mutex` holds its value in place in an `UnsafeCell`; `lock` through a shared
    // reference gives `&mut T`, so the value is changeable after it exists.
    unsafe impl<T: ?Sized> Owns<Mutably> for Mutex<T> {}
    // SAFETY: as `Mutex`: the value in place in an `UnsafeCell`, `write` through a shared
    // reference gives `&mut T`.
    unsafe impl<T: ?Sized> Owns<Mutably> for RwLock<T> {}
    // SAFETY: a `OnceLock` holds its value in place and `set` writes it through a shared
    // reference, once, after the cell exists.
    unsafe impl<T> Owns<Mutably> for OnceLock<T> {}
    // SAFETY: a `LazyLock` holds its initializer `F` and then its value `T` in place, the
    // one replaced by the other through a shared reference on first use.
    unsafe impl<T, F> Owns<Mutably> for LazyLock<T, F> {}
}
