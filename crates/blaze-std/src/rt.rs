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

/// The baseline threads the native driver runs file calls on where it has a pool (kqueue, IOCP):
/// scaled up to [`MAX_FILE_THREADS`] on wide-core hosts ([`machine::file_threads`], std.md).
pub(crate) const FILE_THREADS: usize = 4;

/// Upper bound on native driver file threads on large-core servers (e.g. 16 at 128 cores).
const MAX_FILE_THREADS: usize = 16;

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
        let alloc = heap.shared();
        let io = open_native_with_telemetry(
            os,
            args,
            Vars::process(block),
            start_dir(),
            u8::MAX,
            Some(TelemetryOpts { level: chosen, alloc }),
        )?;
        Ok((io, AtExit { level: chosen, alloc }))
    });
    let (world, at_exit) = match world {
        Ok((io, at_exit)) => (Ok(io), Some(at_exit)),
        Err(why) => (Err(why), None),
    };
    run_io_with(world, at_exit, move |cx, io| main(cx, io, heap))
}

/// Telemetry registration passed into [`open_native_with_telemetry`] so the world's
/// [`Workers`](crate::workers::Workers) reservation is constructed once with telemetry configured.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TelemetryOpts {
    pub(crate) level: crate::telemetry::Level,
    pub(crate) alloc: blaze_own::SharedAlloc<'static>,
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
        Ok((workers, keys)) => (native_on(os, workers), Some(keys)),
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
                if let Some((cpu, class)) = mine.placement(at) {
                    if let Err(why) = machine::place(cpu, class) {
                        let mut buf = [0u8; 256];
                        let msg = machine::fmt_into(&mut buf, format_args!("{}", StartError { worker: at, why }));
                        super::fatal(msg);
                    }
                    let _ = blaze_mem::chunks::set_thread_node(machine::numa_node(cpu));
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
    #[cfg(target_os = "linux")]
    use super::std::fs;
    use super::std::{self, format, string::String, thread, vec::Vec as StdVec};

    use alloc::{alloc::Global, string::ToString as _};

    use blaze_own::Vec;

    use super::{FILE_THREADS, MAX_FILE_THREADS, TopologyError};
    use crate::task::threads::{Class, Group, Topology};

    /// Maximum workers placed in one `GroupState`: unified-LLC CPUs (e.g. 128-core Graviton 4/5)
    /// are split into virtual groups of at most 16 workers so all cores do not serialize on a
    /// single `bstd.task.inject` mutex.
    pub(super) const MAX_GROUP_WORKERS: usize = 16;

    /// Native file-IO thread count for a host of `cores` logical CPUs, bounded to `4..=16`.
    pub(super) const fn file_threads_for(cores: usize) -> usize {
        let scaled = cores / 8;
        if scaled < FILE_THREADS {
            FILE_THREADS
        } else if scaled > MAX_FILE_THREADS {
            MAX_FILE_THREADS
        } else {
            scaled
        }
    }

    /// Native file-IO thread count for this host (`4..=16`, spawning no threads here).
    pub(super) fn file_threads() -> usize {
        file_threads_for(logical_cpus())
    }

    /// Logical CPU count allowed for this process via one `sched_getaffinity(2)` call (no sysfs
    /// cache walk or heap allocation at process start).
    #[cfg(target_os = "linux")]
    pub(super) fn logical_cpus() -> usize {
        let mut mask = [0u8; linux::SET_BYTES];
        // SAFETY: `mask` is `SET_BYTES` writable bytes, the size passed; 0 is this thread.
        if unsafe { linux::sched_getaffinity(0, linux::SET_BYTES, mask.as_mut_ptr()) } != 0 {
            return FILE_THREADS;
        }
        let mut count = 0usize;
        for byte in mask {
            count = count.saturating_add(usize::try_from(byte.count_ones()).unwrap_or(0));
        }
        count
    }

    /// Logical CPU count via a single `hw.logicalcpu` sysctl (no `Topology` allocation or
    /// per-perflevel sysctl walk at process start).
    #[cfg(target_os = "macos")]
    pub(super) fn logical_cpus() -> usize {
        sysctl(c"hw.logicalcpu").ok().and_then(|n| usize::try_from(n).ok()).unwrap_or(FILE_THREADS)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    pub(super) fn logical_cpus() -> usize {
        FILE_THREADS
    }

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
            let node = numa_node(cpu);
            let group: StdVec<u32> = shared
                .into_iter()
                .filter(|&it| allowed(it) && !seen.contains(&it) && numa_node(it) == node)
                .collect();
            seen.extend(group.iter().copied());
            if !group.is_empty() {
                groups.push(group);
            }
        }
        Ok(made(groups.into_iter().map(|cpus| (Class::Foreground, cpus)), true))
    }

    /// Formats `args` into a caller-supplied stack buffer without allocating.
    pub(super) fn fmt_into<'b>(buf: &'b mut [u8], args: core::fmt::Arguments<'_>) -> &'b str {
        struct Writer<'b> {
            buf: &'b mut [u8],
            len: usize,
        }
        impl core::fmt::Write for Writer<'_> {
            fn write_str(&mut self, s: &str) -> core::fmt::Result {
                let bytes = s.as_bytes();
                let room = self.buf.len().saturating_sub(self.len);
                let take = bytes.len().min(room);
                if let Some(dst) = self.buf.get_mut(self.len..self.len + take)
                    && let Some(src) = bytes.get(..take)
                {
                    dst.copy_from_slice(src);
                    self.len += take;
                }
                Ok(())
            }
        }
        let mut writer = Writer { buf, len: 0 };
        let _ = core::fmt::Write::write_fmt(&mut writer, args);
        let len = writer.len;
        core::str::from_utf8(buf.get(..len).unwrap_or(&[])).unwrap_or("")
    }

    /// The NUMA node of Linux CPU `cpu` from `/sys/devices/system/cpu/cpuN/nodeM`, or `None` when
    /// sysfs exposes no node or the node is past `blaze_mem::chunks::MAX_NODES`.
    #[cfg(target_os = "linux")]
    pub(super) fn numa_node(cpu: u32) -> Option<u32> {
        let mut buf = [0u8; 64];
        let dir = fmt_into(&mut buf, format_args!("/sys/devices/system/cpu/cpu{cpu}"));
        let entries = fs::read_dir(dir).ok()?;
        for entry in entries.flatten() {
            let name = entry.file_name();
            if let Some(num) = name.to_str().and_then(|it| it.strip_prefix("node"))
                && let Ok(node) = num.parse::<u32>()
                && node < blaze_mem::chunks::MAX_NODES
            {
                return Some(node);
            }
        }
        None
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

    #[cfg(not(target_os = "linux"))]
    pub(super) fn numa_node(_cpu: u32) -> Option<u32> {
        None
    }

    fn made(groups: impl Iterator<Item = (Class, StdVec<u32>)>, pin: bool) -> Topology {
        let alloc = Global;
        let mut out = Vec::new_in(alloc);
        for (class, cpus) in groups {
            for chunk in cpus.chunks(MAX_GROUP_WORKERS) {
                let mut own = Vec::with_capacity_in(chunk.len(), alloc);
                for &cpu in chunk {
                    own.push(cpu);
                }
                out.push(Group { class, cpus: own });
            }
        }
        Topology { groups: out, pin }
    }

    #[cfg(test)]
    mod tests {
        use super::{Class, MAX_GROUP_WORKERS, StdVec, cpu_list, file_threads_for, made, std};

        #[test]
        fn a_cpu_list_reads_ranges_and_singles() {
            assert_eq!(cpu_list("0-3,8,10-11\n"), Ok(std::vec![0, 1, 2, 3, 8, 10, 11]));
            assert!(cpu_list("3-1").is_err());
        }

        #[test]
        fn unified_llc_group_splits_into_virtual_groups_of_at_most_16_and_file_threads_scale() {
            let unified_128: StdVec<u32> = (0..128).collect();
            let topo = made(core::iter::once((Class::Foreground, unified_128)), true);
            assert_eq!(topo.workers(), 128);
            assert_eq!(topo.groups.len(), 8);
            assert!(topo.groups.iter().all(|g| g.cpus.len() == MAX_GROUP_WORKERS));

            let mixed = made(
                [
                    (Class::Foreground, (0..20).collect()),
                    (Class::Background, (20..28).collect()),
                ]
                .into_iter(),
                false,
            );
            assert_eq!(mixed.workers(), 28);
            assert_eq!(mixed.groups.len(), 3);
            assert_eq!(mixed.groups[0].cpus.len(), 16);
            assert_eq!(mixed.groups[1].cpus.len(), 4);
            assert_eq!(mixed.groups[2].cpus.len(), 8);

            assert_eq!(file_threads_for(1), 4);
            assert_eq!(file_threads_for(16), 4);
            assert_eq!(file_threads_for(64), 8);
            assert_eq!(file_threads_for(128), 16);
            assert_eq!(file_threads_for(256), 16);
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
            assert!(super::logical_cpus() >= 1, "hw.logicalcpu reports at least one core");
        }
    }
}

/// The process's world, made with the root of real-OS authority `os`, or why it cannot be made.
fn native(os: &blaze_core::Os) -> Result<Io, String> {
    let block = process_start()?;
    open_native(os, ArgList::process(block), Vars::process(block), start_dir(), u8::MAX)
}

type StartError = String;

fn fmt_start_err(error: impl core::fmt::Display) -> StartError {
    // alloc: an error's message.
    error.to_string()
}

/// The process's world over pre-built `workers`, made with the root of real-OS authority `os`,
/// without constructing a throwaway bare [`Workers`](crate::workers::Workers) reservation first.
fn native_on(os: &blaze_core::Os, workers: crate::workers::Workers) -> Result<Io, StartError> {
    let block = process_start()?;
    open_native_on(os, ArgList::process(block), Vars::process(block), start_dir(), u8::MAX, Some(workers))
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
    open_native_on(os, args, env, root, above, None)
}

/// [`open_native`] with optional [`TelemetryOpts`] registered directly on the world's
/// [`Workers`](crate::workers::Workers) so startup makes one reservation instead of two.
pub(crate) fn open_native_with_telemetry(
    os: &blaze_core::Os,
    args: ArgList,
    env: Vars,
    root: &RootPath,
    above: u8,
    telemetry: Option<TelemetryOpts>,
) -> Result<Io, StartError> {
    let workers = match telemetry {
        Some(telemetry) => {
            let setup = crate::workers::WorkersSetup { limits: crate::workers::Limits::BARE, seed: None, models: 0 };
            let mut opts = crate::workers::WorkersOpts::new(setup).map_err(fmt_start_err)?;
            opts.telemetry(telemetry.level, telemetry.alloc);
            Some(crate::workers::Workers::new(opts).map_err(fmt_start_err)?)
        }
        None => None,
    };
    open_native_on(os, args, env, root, above, workers)
}

fn open_native_on(
    os: &blaze_core::Os,
    args: ArgList,
    env: Vars,
    root: &RootPath,
    above: u8,
    workers: Option<crate::workers::Workers>,
) -> Result<Io, StartError> {
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
        threads: machine::file_threads(),
        env,
        args,
        // alloc: once per process, the host's name (empty: see `run`).
        host_name: Arc::from(""),
        // Drawn at the first `Io::rng`, not here: the start draws no randomness.
        seed: None,
        limits,
    };
    let opened = match workers {
        Some(workers) => Io::native_in_on(os, world, workers),
        None => Io::native_in(os, world),
    };
    // alloc: an error's message.
    opened.map_err(|error| error.to_string())
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

#[cfg(target_os = "linux")]
const MAX_REAPERS: usize = 64;
#[cfg(target_os = "linux")]
const MAX_REAPER_PENDING: usize = 64;

/// Typed failure when reserving a stalled-reader reaper slot at [`RootReader::new`] admission time,
/// distinguishing backlog capacity exhaustion from OS thread creation failure.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ReaperReserveError {
    BacklogFull,
    SpawnFailed,
}

#[cfg(target_os = "linux")]
struct ReaperPoolState {
    core: self::std::sync::Mutex<crate::procs::hermetic::StandbyReaperCore<(crate::io::Io, crate::fs::Dir<crate::fs::ReadOnly>)>>,
    wake: self::std::sync::Condvar,
}

#[cfg(target_os = "linux")]
fn reaper_pool_state() -> &'static ReaperPoolState {
    use self::std::sync::{Condvar, Mutex, OnceLock};
    use crate::procs::hermetic::{StandbyReaperCore, StandbyReaperPlant};
    static STATE: OnceLock<ReaperPoolState> = OnceLock::new();
    STATE.get_or_init(|| ReaperPoolState {
        core: Mutex::new(StandbyReaperCore::new(MAX_REAPERS, MAX_REAPER_PENDING, StandbyReaperPlant::None)),
        wake: Condvar::new(),
    })
}

/// Atomically reserves a slot in the stalled-reader reaper backlog when admitting a new
/// [`RootReader`] and pre-spawns a parked standby `bstd-io-reaper` worker if needed so
/// [`retire_stalled_io`] NEVER needs `thread::spawn`, NEVER fails, and NEVER loops retrying.
#[cfg(target_os = "linux")]
pub(crate) fn try_reserve_reaper_slot() -> Result<(), ReaperReserveError> {
    use crate::procs::hermetic::StandbyWorkerStep;

    let state = reaper_pool_state();
    let need_spawn = {
        let mut guard = match state.core.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        match guard.begin_reserve() {
            Ok(need) => need,
            Err(()) => return Err(ReaperReserveError::BacklogFull),
        }
    };
    if need_spawn {
        let spawned = self::std::thread::Builder::new()
            .name(self::std::string::String::from("bstd-io-reaper"))
            .spawn(move || {
                let state = reaper_pool_state();
                {
                    let mut guard = match state.core.lock() {
                        Ok(g) => g,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    guard.on_worker_started();
                }
                loop {
                    let item = {
                        let mut guard = match state.core.lock() {
                            Ok(g) => g,
                            Err(poisoned) => poisoned.into_inner(),
                        };
                        loop {
                            match guard.worker_poll() {
                                StandbyWorkerStep::Work(item) => break Some(item),
                                StandbyWorkerStep::Exit => break None,
                                StandbyWorkerStep::Wait => {
                                    guard = match state.wake.wait(guard) {
                                        Ok(g) => g,
                                        Err(poisoned) => poisoned.into_inner(),
                                    };
                                }
                            }
                        }
                    };
                    let Some((retired_io, retired_root)) = item else {
                        break;
                    };
                    drop(retired_root);
                    drop(retired_io);
                    let mut guard = match state.core.lock() {
                        Ok(g) => g,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    guard.worker_finished_drop();
                }
            });
        if spawned.is_err() {
            let mut guard = match state.core.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.abort_spawn_failed();
            state.wake.notify_all();
            return Err(ReaperReserveError::SpawnFailed);
        }
    }
    Ok(())
}

/// Releases a previously reserved slot when a [`RootReader`] closes cleanly without stalling,
/// waking any surplus parked standby worker so it exits immediately.
#[cfg(target_os = "linux")]
pub(crate) fn release_reaper_slot() {
    let state = reaper_pool_state();
    let wake_surplus = {
        let mut guard = match state.core.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.release_reservation()
    };
    if wake_surplus {
        state.wake.notify_one();
    }
}

/// Retires an abandoned native `Io` (whose worker thread is still blocked in an uninterruptible
/// kernel read after cancellation/deadline) into the pre-spawned standby reaper pool:
/// 1. Caller stack-drop NEVER drops synchronously and NEVER calls `thread::spawn` (a standby
///    worker was already spawned at admission or all `MAX_REAPERS` workers are active and will
///    drain `pending` directly upon finishing),
/// 2. Up to `MAX_REAPERS = 64` independent reaper threads run concurrently so one permanently
///    wedged mount never blocks cleanup of later retired drivers, and
/// 3. Total live + wedged backlog is atomically bounded at admission by `try_reserve_reaper_slot()`
///    (`MAX_REAPERS + MAX_REAPER_PENDING`).
#[cfg(target_os = "linux")]
pub(crate) fn retire_stalled_io(io: crate::io::Io, root: crate::fs::Dir<crate::fs::ReadOnly>) {
    let state = reaper_pool_state();
    {
        let mut guard = match state.core.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.retire_reserved((io, root));
    }
    state.wake.notify_one();
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct OpenedRootDir {
    fd: blaze_sys::posix::io::OwnedFd,
    dev: u64,
    ino: u64,
}

/// Opened kernel directory descriptor (`O_DIRECTORY | O_CLOEXEC`) plus its `(dev, ino)` identity
/// and original path cached when a native Unix world opens (or lazily on first lock if the root
/// folder did not yet exist when the world was constructed, matching `blaze-core` lazy folder
/// creation on fresh `$HOME` setups). Once pinned, off-thread advisory locks and locked I/O
/// resolve strictly beneath the opened descriptor without traversing symbolic links AND verify the
/// driver's root pathname still resolves to the exact same directory inode.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) struct NativeRootDir {
    cpath: blaze_own::Vec<u8, blaze_own::SharedAlloc<'static>>,
    opened: self::std::sync::Mutex<Option<blaze_own::Arc<OpenedRootDir, blaze_own::SharedAlloc<'static>>>>,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn fd_dev_ino(fd: blaze_sys::Fd) -> Result<(u64, u64), blaze_sys::Errno> {
    #[cfg(target_os = "linux")]
    {
        let st = blaze_sys::linux::task::fstat(fd)?;
        Ok((st.st_dev, st.st_ino))
    }
    #[cfg(target_os = "macos")]
    #[expect(unsafe_code, reason = "POSIX `fstat` FFI on an open descriptor with its SAFETY comment")]
    {
        let mut st = blaze_sys::posix::proc::stat::zeroed();
        // SAFETY: `fstat` writes to valid mutable `stat` buffer for open descriptor `fd`.
        let rc = unsafe { blaze_sys::posix::proc::fstat(fd.raw(), core::ptr::addr_of_mut!(st)) };
        if rc != 0 {
            return Err(blaze_sys::Errno::Io);
        }
        let dev = u64::try_from(st.st_dev).map_err(|_| blaze_sys::Errno::Overflow)?;
        Ok((dev, st.st_ino))
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl NativeRootDir {
    pub(crate) fn open(root: &RootPath) -> Result<Self, blaze_sys::Errno> {
        use self::std::os::unix::ffi::OsStrExt as _;
        let raw = root.as_os_str().as_bytes();
        if raw.is_empty() || raw.contains(&0) {
            return Err(blaze_sys::Errno::Inval);
        }
        let mut buf = blaze_own::Vec::with_capacity_in(raw.len() + 1, blaze_own::SharedAlloc::global());
        buf.extend_from_slice(raw);
        buf.push(0);
        let cstr = core::ffi::CStr::from_bytes_with_nul(buf.as_slice()).map_err(|_| blaze_sys::Errno::Inval)?;
        let flags = blaze_sys::posix::io::OpenFlags::DIRECTORY | blaze_sys::posix::io::OpenFlags::CLOSE_ON_EXEC;
        let opened = match blaze_sys::posix::io::openat(blaze_sys::posix::io::CWD, blaze_sys::Path::new(cstr), flags, 0) {
            Ok(fd) => {
                let (dev, ino) = fd_dev_ino(fd.fd())?;
                Some(blaze_own::Arc::new_in(OpenedRootDir { fd, dev, ino }, blaze_own::SharedAlloc::global()))
            }
            // Match `blaze_core::native_in`: a native world may be opened over a not-yet-created
            // directory (e.g. `~/.blaze/v2/proc-macro/expansions` on a fresh `$HOME`) and lazily
            // created on first write; pin the directory descriptor on first lock instead.
            Err(blaze_sys::Errno::NoEnt) => None,
            Err(errno) => return Err(errno),
        };
        Ok(Self { cpath: buf, opened: self::std::sync::Mutex::new(opened) })
    }

    fn require_opened(&self) -> Result<blaze_own::Arc<OpenedRootDir, blaze_own::SharedAlloc<'static>>, blaze_own::String<blaze_own::SharedAlloc<'static>>> {
        let guard = match self.opened.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        guard
            .as_ref()
            .map(blaze_own::Share::share)
            .ok_or_else(|| blaze_own::String::from_str_in("native root directory has not been opened", blaze_own::SharedAlloc::global()))
    }

    fn verify_unchanged_against(&self, expected: &OpenedRootDir) -> Result<(), blaze_own::String<blaze_own::SharedAlloc<'static>>> {
        let Ok(cstr) = core::ffi::CStr::from_bytes_with_nul(self.cpath.as_slice()) else {
            return Err(blaze_own::String::from_str_in(
                "invalid native root path",
                blaze_own::SharedAlloc::global(),
            ));
        };
        let flags = blaze_sys::posix::io::OpenFlags::DIRECTORY | blaze_sys::posix::io::OpenFlags::CLOSE_ON_EXEC;
        let cur = blaze_sys::posix::io::openat(blaze_sys::posix::io::CWD, blaze_sys::Path::new(cstr), flags, 0)
            .map_err(|e| lock_err_errno("openat (verify native root unchanged)", e))?;
        let (cur_dev, cur_ino) = fd_dev_ino(cur.fd())
            .map_err(|e| lock_err_errno("fstat (verify native root unchanged)", e))?;
        if cur_dev != expected.dev || cur_ino != expected.ino {
            return Err(blaze_own::String::from_str_in(
                "native world root directory was replaced since opening; refusing split-brain lock",
                blaze_own::SharedAlloc::global(),
            ));
        }
        Ok(())
    }

    /// Returns the pinned [`OpenedRootDir`] (opening and pinning `self.cpath` once if it did not
    /// exist when the world was constructed) and verifies `self.cpath` still resolves to that
    /// exact directory inode `(dev, ino)`.
    fn opened_and_verified(&self) -> Result<blaze_own::Arc<OpenedRootDir, blaze_own::SharedAlloc<'static>>, blaze_own::String<blaze_own::SharedAlloc<'static>>> {
        let existing = {
            let guard = match self.opened.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            guard.as_ref().map(blaze_own::Share::share)
        };
        if let Some(opened) = existing {
            self.verify_unchanged_against(&opened)?;
            return Ok(opened);
        }
        let Ok(cstr) = core::ffi::CStr::from_bytes_with_nul(self.cpath.as_slice()) else {
            return Err(blaze_own::String::from_str_in(
                "invalid native root path",
                blaze_own::SharedAlloc::global(),
            ));
        };
        let flags = blaze_sys::posix::io::OpenFlags::DIRECTORY | blaze_sys::posix::io::OpenFlags::CLOSE_ON_EXEC;
        let fd = blaze_sys::posix::io::openat(blaze_sys::posix::io::CWD, blaze_sys::Path::new(cstr), flags, 0)
            .map_err(|e| lock_err_errno("openat (open native root dir)", e))?;
        let (dev, ino) = fd_dev_ino(fd.fd())
            .map_err(|e| lock_err_errno("fstat (open native root dir)", e))?;
        let candidate = blaze_own::Arc::new_in(OpenedRootDir { fd, dev, ino }, blaze_own::SharedAlloc::global());
        let mut guard = match self.opened.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if let Some(winner) = guard.as_ref() {
            let winner_arc = blaze_own::Share::share(winner);
            drop(guard);
            if winner_arc.dev != dev || winner_arc.ino != ino {
                return Err(blaze_own::String::from_str_in(
                    "native world root directory was replaced since opening; refusing split-brain lock",
                    blaze_own::SharedAlloc::global(),
                ));
            }
            return Ok(winner_arc);
        }
        *guard = Some(blaze_own::Share::share(&candidate));
        Ok(candidate)
    }
}

/// Planted bugs for deterministic interleaving DSTs of [`LockRuntime`].
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LockPlant {
    None,
    /// Control 1: on unlock worker-cap / spawn failure, drops the handle synchronously on the
    /// caller thread instead of retiring through the bounded reaper pool.
    SyncDropOnUnlockCap,
    /// Control 2: on caller-abandon race, skips unlocking the acquired handle before closing it.
    SkipAbandonUnlock,
}

/// Completion notification signal used by [`LockRuntime`] and [`LockBackend::wait_signal`]:
/// in production uses a `Mutex<bool>` + `Condvar` (`wait_timeout`); in deterministic simulation
/// lets the single-threaded scheduler step queued workers without OS threads or `thread::sleep`.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub struct AttemptSignal {
    done: self::std::sync::Mutex<bool>,
    cvar: self::std::sync::Condvar,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl AttemptSignal {
    pub fn new() -> Self {
        Self {
            done: self::std::sync::Mutex::new(false),
            cvar: self::std::sync::Condvar::new(),
        }
    }

    fn notify(&self) {
        let mut guard = match self.done.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        *guard = true;
        self.cvar.notify_one();
    }

    pub fn is_notified(&self) -> bool {
        let guard = match self.done.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        *guard
    }

    pub fn wait_timeout(&self, timeout: core::time::Duration) {
        let guard = match self.done.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if !*guard {
            let _ = self.cvar.wait_timeout(guard, timeout);
        }
    }
}

/// Platform seam driven by [`LockRuntime`] in both production (`RealOpBackend`) and
/// 100% single-threaded deterministic discrete-event simulation (`SimLockBackend`).
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub trait LockBackend<H: Send + 'static>: Send + Sync + 'static {
    fn open_and_try_lock(&self) -> Result<Option<H>, blaze_own::String<blaze_own::SharedAlloc<'static>>>;
    fn unlock_and_close(&self, handle: H) -> Result<(), blaze_own::String<blaze_own::SharedAlloc<'static>>>;
    fn spawn_worker<F: FnOnce() + Send + 'static>(&self, name: &str, f: F) -> Result<(), ()>;
    fn wait_signal(&self, signal: &AttemptSignal);

    /// Parks a standby reaper worker waiting for retired handles or surplus exit. Real OS backends
    /// block on `cvar.wait(guard)` and return `Some(guard)`; single-threaded DST backends record a
    /// parked worker and return `None` to yield back to the deterministic scheduler.
    fn park_reaper<'a>(
        &self,
        cvar: &self::std::sync::Condvar,
        guard: self::std::sync::MutexGuard<'a, crate::procs::hermetic::StandbyReaperCore<H>>,
    ) -> Option<self::std::sync::MutexGuard<'a, crate::procs::hermetic::StandbyReaperCore<H>>> {
        Some(match cvar.wait(guard) {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        })
    }

    /// Wakes one (`all == false`) or all (`all == true`) parked standby reaper workers.
    fn notify_reaper(&self, cvar: &self::std::sync::Condvar, all: bool) {
        if all {
            cvar.notify_all();
        } else {
            cvar.notify_one();
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct RuntimeSlotGuard<'a, H: Send + 'static, B: LockBackend<H>> {
    rt: &'a LockRuntime<H>,
    backend: &'a B,
    held: bool,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl<H: Send + 'static, B: LockBackend<H>> Drop for RuntimeSlotGuard<'_, H, B> {
    fn drop(&mut self) {
        if self.held {
            self.rt.release_slot(self.backend);
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct LockRuntimeInner<H: Send + 'static> {
    reaper: self::std::sync::Mutex<crate::procs::hermetic::StandbyReaperCore<H>>,
    reaper_wake: self::std::sync::Condvar,
    active_workers: core::sync::atomic::AtomicUsize,
    max_workers: usize,
    plant: LockPlant,
}

/// Production off-thread advisory lock + pre-spawned standby reaper coordinator, generic over
/// handle `H` and [`LockBackend`] so deterministic interleaving DSTs execute the exact production
/// abandon handshake, `active_workers` cap, and [`StandbyReaperCore`] standby worker pool.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub struct LockRuntime<H: Send + 'static> {
    inner: blaze_own::Arc<LockRuntimeInner<H>, blaze_own::SharedAlloc<'static>>,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl<H: Send + 'static> Clone for LockRuntime<H> {
    fn clone(&self) -> Self {
        Self { inner: blaze_own::Share::share(&self.inner) }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl<H: Send + 'static> blaze_own::Share for LockRuntime<H> {}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl<H: Send + 'static> LockRuntime<H> {
    pub fn new(max_workers: usize, max_pending: usize, plant: LockPlant) -> Self {
        Self {
            inner: blaze_own::Arc::new_in(
                LockRuntimeInner {
                    reaper: self::std::sync::Mutex::new(crate::procs::hermetic::StandbyReaperCore::new(
                        max_workers,
                        max_pending,
                        crate::procs::hermetic::StandbyReaperPlant::None,
                    )),
                    reaper_wake: self::std::sync::Condvar::new(),
                    active_workers: core::sync::atomic::AtomicUsize::new(0),
                    max_workers: max_workers.max(1),
                    plant,
                },
                blaze_own::SharedAlloc::global(),
            ),
        }
    }

    pub fn active_workers(&self) -> usize {
        self.inner.active_workers.load(core::sync::atomic::Ordering::Acquire)
    }

    pub fn reaper_active(&self) -> usize {
        match self.inner.reaper.lock() {
            Ok(g) => g.active(),
            Err(p) => p.into_inner().active(),
        }
    }

    pub fn reaper_idle(&self) -> usize {
        match self.inner.reaper.lock() {
            Ok(g) => g.idle(),
            Err(p) => p.into_inner().idle(),
        }
    }

    pub fn reaper_spawning(&self) -> usize {
        match self.inner.reaper.lock() {
            Ok(g) => g.spawning(),
            Err(p) => p.into_inner().spawning(),
        }
    }

    pub fn reaper_pending(&self) -> usize {
        match self.inner.reaper.lock() {
            Ok(g) => g.pending_len(),
            Err(p) => p.into_inner().pending_len(),
        }
    }

    pub fn reaper_reserved(&self) -> usize {
        match self.inner.reaper.lock() {
            Ok(g) => g.reserved(),
            Err(p) => p.into_inner().reserved(),
        }
    }

    /// Runs the standby lock-reaper loop for one already-started worker until it exits (or yields
    /// back to a single-threaded DST scheduler via [`LockBackend::park_reaper`]).
    pub fn run_standby_reaper_loop<B: LockBackend<H>>(&self, backend: &B) {
        use crate::procs::hermetic::StandbyWorkerStep;
        loop {
            let item = {
                let mut guard = match self.inner.reaper.lock() {
                    Ok(g) => g,
                    Err(p) => p.into_inner(),
                };
                loop {
                    match guard.worker_poll() {
                        StandbyWorkerStep::Work(item) => break Some(item),
                        StandbyWorkerStep::Exit => break None,
                        StandbyWorkerStep::Wait => {
                            let Some(next_guard) = backend.park_reaper(&self.inner.reaper_wake, guard) else {
                                return;
                            };
                            guard = next_guard;
                        }
                    }
                }
            };
            let Some(retired) = item else {
                break;
            };
            let _ = backend.unlock_and_close(retired);
            let mut guard = match self.inner.reaper.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            guard.worker_finished_drop();
        }
    }

    fn reserve_slot<B: LockBackend<H>>(
        &self,
        backend: &blaze_own::Arc<B, blaze_own::SharedAlloc<'static>>,
    ) -> Result<(), NativeLockErr> {
        let need_spawn = {
            let mut guard = match self.inner.reaper.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            match guard.begin_reserve() {
                Ok(need) => need,
                Err(()) => return Err(lock_err_static("file-lock reaper backlog full")),
            }
        };
        if need_spawn {
            let rt_w = blaze_own::Share::share(self);
            let backend_w = blaze_own::Share::share(backend);
            let spawned = backend.spawn_worker("bstd-lock-reaper", move || {
                {
                    let mut guard = match rt_w.inner.reaper.lock() {
                        Ok(g) => g,
                        Err(p) => p.into_inner(),
                    };
                    guard.on_worker_started();
                }
                rt_w.run_standby_reaper_loop(&*backend_w);
            });
            if spawned.is_err() {
                let mut guard = match self.inner.reaper.lock() {
                    Ok(g) => g,
                    Err(p) => p.into_inner(),
                };
                guard.abort_spawn_failed();
                drop(guard);
                backend.notify_reaper(&self.inner.reaper_wake, true);
                return Err(lock_err_static("file-lock reaper worker spawn failed"));
            }
        }
        Ok(())
    }

    fn release_slot<B: LockBackend<H>>(&self, backend: &B) {
        let wake_surplus = {
            let mut guard = match self.inner.reaper.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            guard.release_reservation()
        };
        if wake_surplus {
            backend.notify_reaper(&self.inner.reaper_wake, false);
        }
    }

    /// Retires an owned lock handle into the pre-spawned standby reaper pool: NEVER unlocks or
    /// closes `handle` synchronously on the caller thread and NEVER calls `spawn_worker`.
    pub fn retire_handle<B: LockBackend<H>>(
        &self,
        handle: H,
        backend: &blaze_own::Arc<B, blaze_own::SharedAlloc<'static>>,
    ) {
        {
            let mut guard = match self.inner.reaper.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            guard.retire_reserved(handle);
        }
        backend.notify_reaper(&self.inner.reaper_wake, false);
    }

    pub fn lock_exclusive<B: LockBackend<H>>(
        &self,
        backend: &blaze_own::Arc<B, blaze_own::SharedAlloc<'static>>,
        is_stopped: &mut dyn FnMut() -> Option<crate::Stopped>,
        sleep_backoff: &mut dyn FnMut() -> Result<(), crate::Stopped>,
    ) -> Result<H, NativeLockErr> {
        use core::sync::atomic::Ordering;
        use self::std::sync::Mutex;

        type LockMsg<H> = Result<Option<H>, blaze_own::String<blaze_own::SharedAlloc<'static>>>;
        enum AttemptState<H> {
            Running,
            Done(LockMsg<H>),
            Abandoned,
            Taken,
        }

        self.reserve_slot(backend)?;
        let mut slot_guard = RuntimeSlotGuard { rt: self, backend: &**backend, held: true };
        loop {
            if let Some(stopped) = is_stopped() {
                return Err(NativeLockErr::Stopped(stopped));
            }
            let prev = self.inner.active_workers.fetch_add(1, Ordering::AcqRel);
            if prev >= self.inner.max_workers {
                self.inner.active_workers.fetch_sub(1, Ordering::AcqRel);
                return Err(lock_err_static("file-lock worker cap reached (stalled filesystem backlog full)"));
            }
            // Single `Mutex<AttemptState<H>>` publishes `Done(res)` atomically in the exact same
            // critical section as the state transition — impossible for cancellation to block on
            // separate channel delivery after the worker marks completion.
            let attempt = blaze_own::Arc::new_in(Mutex::new(AttemptState::<H>::Running), blaze_own::SharedAlloc::global());
            let signal = blaze_own::Arc::new_in(AttemptSignal::new(), blaze_own::SharedAlloc::global());
            let attempt_w = blaze_own::Share::share(&attempt);
            let signal_w = blaze_own::Share::share(&signal);
            let rt_w = blaze_own::Share::share(self);
            let backend_w = blaze_own::Share::share(backend);
            let plant = self.inner.plant;
            let spawned = backend.spawn_worker("bstd-lock-op", move || {
                let res: LockMsg<H> = backend_w.open_and_try_lock();
                let abandoned = {
                    let mut guard = match attempt_w.lock() {
                        Ok(g) => g,
                        Err(p) => p.into_inner(),
                    };
                    match *guard {
                        AttemptState::Running => {
                            *guard = AttemptState::Done(res);
                            None
                        }
                        AttemptState::Abandoned | AttemptState::Done(_) | AttemptState::Taken => Some(res),
                    }
                };
                if let Some(abandoned_res) = abandoned {
                    if let Ok(Some(h)) = abandoned_res {
                        if plant != LockPlant::SkipAbandonUnlock {
                            let _ = backend_w.unlock_and_close(h);
                        } else {
                            drop(h);
                        }
                    }
                    rt_w.release_slot(&*backend_w);
                } else {
                    signal_w.notify();
                }
                rt_w.inner.active_workers.fetch_sub(1, Ordering::AcqRel);
            });
            if spawned.is_err() {
                self.inner.active_workers.fetch_sub(1, Ordering::AcqRel);
                return Err(lock_err_static("failed to spawn off-thread file-lock worker"));
            }
            loop {
                backend.wait_signal(&signal);
                let stopped = is_stopped();
                let ready = {
                    let mut guard = match attempt.lock() {
                        Ok(g) => g,
                        Err(p) => p.into_inner(),
                    };
                    if matches!(*guard, AttemptState::Done(_)) {
                        match core::mem::replace(&mut *guard, AttemptState::Taken) {
                            AttemptState::Done(res) => Some(res),
                            AttemptState::Running | AttemptState::Abandoned | AttemptState::Taken => None,
                        }
                    } else if stopped.is_some() {
                        *guard = AttemptState::Abandoned;
                        slot_guard.held = false;
                        None
                    } else {
                        None
                    }
                };
                if let Some(res) = ready {
                    if let Some(stopped) = stopped {
                        if let Ok(Some(h)) = res {
                            slot_guard.held = false;
                            self.retire_handle(h, backend);
                        }
                        return Err(NativeLockErr::Stopped(stopped));
                    }
                    match res {
                        Ok(Some(h)) => {
                            slot_guard.held = false;
                            return Ok(h);
                        }
                        Ok(None) => break, // Contended: backoff and retry.
                        Err(msg) => return Err(NativeLockErr::Driver(msg)),
                    }
                } else if let Some(stopped) = stopped {
                    return Err(NativeLockErr::Stopped(stopped));
                }
            }
            sleep_backoff().map_err(NativeLockErr::Stopped)?;
        }
    }

    pub fn unlock<B: LockBackend<H>>(
        &self,
        handle: H,
        backend: &blaze_own::Arc<B, blaze_own::SharedAlloc<'static>>,
        is_stopped: &mut dyn FnMut() -> Option<crate::Stopped>,
    ) -> Result<(), NativeLockErr> {
        use core::sync::atomic::Ordering;
        use self::std::sync::Mutex;

        type UnlockMsg = Result<(), blaze_own::String<blaze_own::SharedAlloc<'static>>>;

        let prev = self.inner.active_workers.fetch_add(1, Ordering::AcqRel);
        if prev >= self.inner.max_workers {
            self.inner.active_workers.fetch_sub(1, Ordering::AcqRel);
            if self.inner.plant == LockPlant::SyncDropOnUnlockCap {
                drop(handle);
                self.release_slot(&**backend);
            } else {
                self.retire_handle(handle, backend);
            }
            return Err(lock_err_static("file-unlock worker cap reached (retired off-thread)"));
        }
        let result_slot = blaze_own::Arc::new_in(Mutex::new(Option::<UnlockMsg>::None), blaze_own::SharedAlloc::global());
        let signal = blaze_own::Arc::new_in(AttemptSignal::new(), blaze_own::SharedAlloc::global());
        let fd_slot = blaze_own::Arc::new_in(Mutex::new(Some(handle)), blaze_own::SharedAlloc::global());
        let result_w = blaze_own::Share::share(&result_slot);
        let signal_w = blaze_own::Share::share(&signal);
        let worker_slot = blaze_own::Share::share(&fd_slot);
        let rt_w = blaze_own::Share::share(self);
        let backend_w = blaze_own::Share::share(backend);
        let spawned = backend.spawn_worker("bstd-unlock-op", move || {
            let owned = match worker_slot.lock() {
                Ok(mut g) => g.take(),
                Err(p) => p.into_inner().take(),
            };
            let res = match owned {
                Some(h) => backend_w.unlock_and_close(h),
                None => Ok(()),
            };
            rt_w.release_slot(&*backend_w);
            rt_w.inner.active_workers.fetch_sub(1, Ordering::AcqRel);
            {
                let mut g = match result_w.lock() {
                    Ok(g) => g,
                    Err(p) => p.into_inner(),
                };
                *g = Some(res);
            }
            signal_w.notify();
        });
        if spawned.is_err() {
            self.inner.active_workers.fetch_sub(1, Ordering::AcqRel);
            let recovered = match fd_slot.lock() {
                Ok(mut g) => g.take(),
                Err(p) => p.into_inner().take(),
            };
            if let Some(h) = recovered {
                if self.inner.plant == LockPlant::SyncDropOnUnlockCap {
                    drop(h);
                    self.release_slot(&**backend);
                } else {
                    self.retire_handle(h, backend);
                }
            } else {
                self.release_slot(&**backend);
            }
            return Err(lock_err_static("failed to spawn off-thread file-unlock worker (retired off-thread)"));
        }
        loop {
            backend.wait_signal(&signal);
            let ready = {
                let mut g = match result_slot.lock() {
                    Ok(g) => g,
                    Err(p) => p.into_inner(),
                };
                g.take()
            };
            if let Some(res) = ready {
                return res.map_err(NativeLockErr::Driver);
            }
            if let Some(stopped) = is_stopped() {
                return Err(NativeLockErr::Stopped(stopped));
            }
        }
    }
}

/// Exclusive advisory file lock acquired off the driver's polling thread on native Unix hosts so
/// stalled FUSE/NFS `openat2`/`flock` calls never freeze driver polling, always honor
/// `Cx::cancelled()` and `Cx::deadline()`, and NEVER close/drop a lock fd synchronously on the
/// caller thread even under worker-cap overflow or `thread::spawn` failure.
#[cfg(any(target_os = "linux", target_os = "macos"))]
struct LockedFd {
    fd: self::std::sync::Mutex<Option<blaze_sys::posix::io::OwnedFd>>,
    in_flight: blaze_own::Arc<core::sync::atomic::AtomicUsize, blaze_own::SharedAlloc<'static>>,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Drop for LockedFd {
    fn drop(&mut self) {
        let fd = match self.fd.lock() {
            Ok(mut g) => g.take(),
            Err(p) => p.into_inner().take(),
        };
        if let Some(fd) = fd {
            global_lock_runtime().retire_handle(fd, real_retire_backend());
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) struct NativeLock {
    fd: Option<blaze_own::Arc<LockedFd, blaze_own::SharedAlloc<'static>>>,
    root_dir: blaze_own::Arc<NativeRootDir, blaze_own::SharedAlloc<'static>>,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
const MAX_LOCK_WORKERS: usize = 64;
#[cfg(any(target_os = "linux", target_os = "macos"))]
const MAX_LOCK_REAPER_PENDING: usize = 64;

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn global_lock_runtime() -> &'static LockRuntime<blaze_sys::posix::io::OwnedFd> {
    use self::std::sync::OnceLock;
    static RT: OnceLock<LockRuntime<blaze_sys::posix::io::OwnedFd>> = OnceLock::new();
    RT.get_or_init(|| LockRuntime::new(MAX_LOCK_WORKERS, MAX_LOCK_REAPER_PENDING, LockPlant::None))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct RealRetireBackend;

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl LockBackend<blaze_sys::posix::io::OwnedFd> for RealRetireBackend {
    fn open_and_try_lock(&self) -> Result<Option<blaze_sys::posix::io::OwnedFd>, blaze_own::String<blaze_own::SharedAlloc<'static>>> {
        Ok(None)
    }

    fn unlock_and_close(&self, fd: blaze_sys::posix::io::OwnedFd) -> Result<(), blaze_own::String<blaze_own::SharedAlloc<'static>>> {
        let res = blaze_sys::posix::io::flock(fd.fd(), blaze_sys::posix::io::FlockOp::Unlock)
            .map_err(|e| lock_err_errno("flock (unlock)", e));
        drop(fd);
        res
    }

    fn spawn_worker<F: FnOnce() + Send + 'static>(&self, name: &str, f: F) -> Result<(), ()> {
        self::std::thread::Builder::new()
            .name(self::std::string::String::from(name))
            .spawn(f)
            .map(|_| ())
            .map_err(|_| ())
    }

    fn wait_signal(&self, signal: &AttemptSignal) {
        signal.wait_timeout(core::time::Duration::from_millis(10));
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn real_retire_backend() -> &'static blaze_own::Arc<RealRetireBackend, blaze_own::SharedAlloc<'static>> {
    use self::std::sync::OnceLock;
    static B: OnceLock<blaze_own::Arc<RealRetireBackend, blaze_own::SharedAlloc<'static>>> = OnceLock::new();
    B.get_or_init(|| blaze_own::Arc::new_in(RealRetireBackend, blaze_own::SharedAlloc::global()))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Drop for NativeLock {
    fn drop(&mut self) {
        let _ = self.fd.take();
    }
}

/// Error returned by [`lock_exclusive_off_thread`] / [`unlock_off_thread`].
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Debug)]
pub enum NativeLockErr {
    Stopped(crate::Stopped),
    Driver(blaze_own::String<blaze_own::SharedAlloc<'static>>),
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn lock_err_static(msg: &str) -> NativeLockErr {
    NativeLockErr::Driver(blaze_own::String::from_str_in(msg, blaze_own::SharedAlloc::global()))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn lock_err_errno(call: &str, errno: blaze_sys::Errno) -> blaze_own::String<blaze_own::SharedAlloc<'static>> {
    let mut out = blaze_own::String::new_in(blaze_own::SharedAlloc::global());
    let _ = core::fmt::write(&mut out, format_args!("{call} failed (errno {})", errno.raw()));
    out
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn with_component_cstr<R>(
    part: &str,
    f: impl FnOnce(blaze_sys::Path<'_>) -> Result<R, blaze_own::String<blaze_own::SharedAlloc<'static>>>,
) -> Result<R, blaze_own::String<blaze_own::SharedAlloc<'static>>> {
    let bytes = part.as_bytes();
    if bytes.is_empty() || bytes == b"." || bytes == b".." || bytes.contains(&0) || bytes.contains(&b'/') {
        return Err(blaze_own::String::from_str_in("invalid lock path component", blaze_own::SharedAlloc::global()));
    }
    if bytes.len() < 256 {
        let mut stack = [0u8; 256];
        if let Some(dst) = stack.get_mut(..bytes.len()) {
            dst.copy_from_slice(bytes);
        }
        let cstr = core::ffi::CStr::from_bytes_with_nul(stack.get(..=bytes.len()).unwrap_or(&[0]))
            .map_err(|_| blaze_own::String::from_str_in("invalid NUL in lock path", blaze_own::SharedAlloc::global()))?;
        return f(blaze_sys::Path::new(cstr));
    }
    let mut heap = blaze_own::Vec::with_capacity_in(bytes.len() + 1, blaze_own::SharedAlloc::global());
    heap.extend_from_slice(bytes);
    heap.push(0);
    let cstr = core::ffi::CStr::from_bytes_with_nul(heap.as_slice())
        .map_err(|_| blaze_own::String::from_str_in("invalid NUL in lock path", blaze_own::SharedAlloc::global()))?;
    f(blaze_sys::Path::new(cstr))
}

#[cfg(target_os = "linux")]
fn open_dir_beneath_no_symlink(
    parent: blaze_sys::Fd,
    part: &str,
) -> Result<blaze_sys::posix::io::OwnedFd, blaze_own::String<blaze_own::SharedAlloc<'static>>> {
    use blaze_sys::linux::{OpenFlags, OpenHow, Resolve, mkdirat, openat2};
    let resolve = Resolve::BENEATH.with(Resolve::NO_SYMLINKS).with(Resolve::NO_MAGICLINKS);
    let how = OpenHow::new(OpenFlags::DIRECTORY | OpenFlags::CLOSE_ON_EXEC, 0, resolve);
    with_component_cstr(part, |cpath| match openat2(parent, cpath, &how) {
        Ok(fd) => Ok(fd),
        Err(blaze_sys::Errno::NoEnt) => {
            match mkdirat(parent, cpath, 0o755) {
                Ok(()) | Err(blaze_sys::Errno::Exist) => {}
                Err(errno) => return Err(lock_err_errno("mkdirat (lock parent)", errno)),
            }
            openat2(parent, cpath, &how).map_err(|e| lock_err_errno("openat2 (lock parent)", e))
        }
        Err(errno) => Err(lock_err_errno("openat2 (lock parent)", errno)),
    })
}

#[cfg(target_os = "macos")]
fn open_dir_beneath_no_symlink(
    parent: blaze_sys::Fd,
    part: &str,
) -> Result<blaze_sys::posix::io::OwnedFd, blaze_own::String<blaze_own::SharedAlloc<'static>>> {
    use blaze_sys::posix::io::{OpenFlags, mkdirat, openat};
    let flags = OpenFlags::DIRECTORY | OpenFlags::NO_FOLLOW | OpenFlags::NO_FOLLOW_ANY | OpenFlags::CLOSE_ON_EXEC;
    with_component_cstr(part, |cpath| match openat(parent, cpath, flags, 0) {
        Ok(fd) => Ok(fd),
        Err(blaze_sys::Errno::NoEnt) => {
            match mkdirat(parent, cpath, 0o755) {
                Ok(()) | Err(blaze_sys::Errno::Exist) => {}
                Err(errno) => return Err(lock_err_errno("mkdirat (lock parent)", errno)),
            }
            openat(parent, cpath, flags, 0).map_err(|e| lock_err_errno("openat (lock parent)", e))
        }
        Err(errno) => Err(lock_err_errno("openat (lock parent)", errno)),
    })
}

#[cfg(target_os = "linux")]
fn open_leaf_beneath_no_symlink(
    parent: blaze_sys::Fd,
    leaf: &str,
) -> Result<blaze_sys::posix::io::OwnedFd, blaze_own::String<blaze_own::SharedAlloc<'static>>> {
    use blaze_sys::linux::{OpenFlags, OpenHow, Resolve, openat2};
    let flags = OpenFlags::READ_WRITE | OpenFlags::CREATE | OpenFlags::CLOSE_ON_EXEC | OpenFlags::NONBLOCK;
    let resolve = Resolve::BENEATH.with(Resolve::NO_SYMLINKS).with(Resolve::NO_MAGICLINKS);
    let how = OpenHow::new(flags, 0o600, resolve);
    with_component_cstr(leaf, |cpath| {
        openat2(parent, cpath, &how).map_err(|e| lock_err_errno("openat2 (lock file)", e))
    })
}

#[cfg(target_os = "macos")]
fn open_leaf_beneath_no_symlink(
    parent: blaze_sys::Fd,
    leaf: &str,
) -> Result<blaze_sys::posix::io::OwnedFd, blaze_own::String<blaze_own::SharedAlloc<'static>>> {
    use blaze_sys::posix::io::{OpenFlags, openat};
    let flags = OpenFlags::READ_WRITE
        | OpenFlags::CREATE
        | OpenFlags::NO_FOLLOW
        | OpenFlags::NO_FOLLOW_ANY
        | OpenFlags::CLOSE_ON_EXEC
        | OpenFlags::NONBLOCK;
    with_component_cstr(leaf, |cpath| {
        openat(parent, cpath, flags, 0o600).map_err(|e| lock_err_errno("openat (lock file)", e))
    })
}

/// Opens `rel` strictly beneath the already-opened `root_dir` without traversing any symbolic link
/// in any intermediate directory or at the final leaf, verifies `root_dir` still matches the
/// driver's root directory inode both before and after acquiring a non-blocking exclusive `flock`,
/// and returns `Ok(None)` on contention (`EAGAIN`/`EWOULDBLOCK`).
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn open_and_try_lock_beneath(
    root_dir: &NativeRootDir,
    rel: &str,
) -> Result<Option<blaze_sys::posix::io::OwnedFd>, blaze_own::String<blaze_own::SharedAlloc<'static>>> {
    let opened = root_dir.opened_and_verified()?;
    let root_fd = opened.fd.fd();
    let mut parts = rel.split('/').peekable();
    let mut owned_dir: Option<blaze_sys::posix::io::OwnedFd> = None;
    let mut leaf: Option<&str> = None;
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            leaf = Some(part);
            break;
        }
        let cur = owned_dir.as_ref().map_or(root_fd, |d| d.fd());
        let next = open_dir_beneath_no_symlink(cur, part)?;
        owned_dir = Some(next);
    }
    let Some(leaf_name) = leaf else {
        return Err(blaze_own::String::from_str_in("empty lock path", blaze_own::SharedAlloc::global()));
    };
    let cur = owned_dir.as_ref().map_or(root_fd, |d| d.fd());
    let fd = open_leaf_beneath_no_symlink(cur, leaf_name)?;
    match blaze_sys::posix::io::flock(fd.fd(), blaze_sys::posix::io::FlockOp::ExclusiveNonBlock) {
        Ok(()) => {
            if let Err(err) = root_dir.verify_unchanged_against(&opened) {
                let _ = blaze_sys::posix::io::flock(fd.fd(), blaze_sys::posix::io::FlockOp::Unlock);
                drop(fd);
                return Err(err);
            }
            Ok(Some(fd))
        }
        Err(blaze_sys::Errno::Again) => {
            drop(fd);
            Ok(None)
        }
        Err(errno) => {
            drop(fd);
            Err(lock_err_errno("flock (exclusive)", errno))
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct RealOpBackend {
    root_dir: Option<blaze_own::Arc<NativeRootDir, blaze_own::SharedAlloc<'static>>>,
    rel: blaze_own::String<blaze_own::SharedAlloc<'static>>,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl LockBackend<blaze_sys::posix::io::OwnedFd> for RealOpBackend {
    fn open_and_try_lock(&self) -> Result<Option<blaze_sys::posix::io::OwnedFd>, blaze_own::String<blaze_own::SharedAlloc<'static>>> {
        let Some(root_dir) = &self.root_dir else {
            return Err(blaze_own::String::from_str_in("missing native root dir", blaze_own::SharedAlloc::global()));
        };
        open_and_try_lock_beneath(root_dir, self.rel.as_str())
    }

    fn unlock_and_close(&self, fd: blaze_sys::posix::io::OwnedFd) -> Result<(), blaze_own::String<blaze_own::SharedAlloc<'static>>> {
        let res = blaze_sys::posix::io::flock(fd.fd(), blaze_sys::posix::io::FlockOp::Unlock)
            .map_err(|e| lock_err_errno("flock (unlock)", e));
        drop(fd);
        res
    }

    fn spawn_worker<F: FnOnce() + Send + 'static>(&self, name: &str, f: F) -> Result<(), ()> {
        self::std::thread::Builder::new()
            .name(self::std::string::String::from(name))
            .spawn(f)
            .map(|_| ())
            .map_err(|_| ())
    }

    fn wait_signal(&self, signal: &AttemptSignal) {
        signal.wait_timeout(core::time::Duration::from_millis(10));
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn lock_exclusive_off_thread(
    cx: &crate::cx::Cx,
    root_dir: blaze_own::Arc<NativeRootDir, blaze_own::SharedAlloc<'static>>,
    rel: &str,
) -> Result<NativeLock, NativeLockErr> {
    let backend = blaze_own::Arc::new_in(
        RealOpBackend {
            root_dir: Some(blaze_own::Share::share(&root_dir)),
            rel: blaze_own::String::from_str_in(rel, blaze_own::SharedAlloc::global()),
        },
        blaze_own::SharedAlloc::global(),
    );
    let backoff = core::time::Duration::from_millis(2);
    let mut is_stopped = || cx.stopped_by(cx.io().now()).err();
    let mut sleep_backoff = || {
        cx.io()
            .call(cx, |tag| blaze_core::Effect::Timer { after: backoff, tag })
            .map(|_| ())
    };
    let fd = global_lock_runtime().lock_exclusive(&backend, &mut is_stopped, &mut sleep_backoff)?;
    let locked = blaze_own::Arc::new_in(
        LockedFd {
            fd: self::std::sync::Mutex::new(Some(fd)),
            in_flight: blaze_own::Arc::new_in(
                core::sync::atomic::AtomicUsize::new(0),
                blaze_own::SharedAlloc::global(),
            ),
        },
        blaze_own::SharedAlloc::global(),
    );
    Ok(NativeLock { fd: Some(locked), root_dir })
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn unlock_off_thread(cx: &crate::cx::Cx, lock: &mut NativeLock) -> Result<(), NativeLockErr> {
    use core::sync::atomic::Ordering;
    let Some(locked) = lock.fd.take() else {
        return Ok(());
    };
    // If an abandoned locked-I/O worker is still blocked in the kernel after a deadline/cancel,
    // keep the flock held until that worker drops its `Arc<LockedFd>` (whose `Drop` retires the
    // fd through `global_lock_runtime()`) rather than unlocking while a publish may still be live.
    if locked.in_flight.load(Ordering::Acquire) > 0 {
        drop(locked);
        return Err(lock_err_static(
            "locked I/O worker still in flight after stop; lock release deferred until worker exits",
        ));
    }
    let fd = {
        let mut guard = match locked.fd.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        guard.take()
    };
    let Some(fd) = fd else {
        return Ok(());
    };
    let backend = blaze_own::Arc::new_in(
        RealOpBackend {
            root_dir: None,
            rel: blaze_own::String::new_in(blaze_own::SharedAlloc::global()),
        },
        blaze_own::SharedAlloc::global(),
    );
    let mut is_stopped = || cx.stopped_by(cx.io().now()).err();
    global_lock_runtime().unlock(fd, &backend, &mut is_stopped)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) enum NativeBoundedRead<'a> {
    Absent,
    Oversized,
    Grew,
    Whole(blaze_own::Vec<u8, blaze_own::Alloc<'a>>),
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
enum SharedBoundedRead {
    Absent,
    Oversized,
    Grew,
    Whole(blaze_own::Vec<u8, blaze_own::SharedAlloc<'static>>),
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn fd_len_u64(fd: blaze_sys::Fd) -> Result<u64, blaze_sys::Errno> {
    #[cfg(target_os = "linux")]
    {
        let st = blaze_sys::linux::task::fstat(fd)?;
        u64::try_from(st.st_size).map_err(|_| blaze_sys::Errno::Overflow)
    }
    #[cfg(target_os = "macos")]
    #[expect(unsafe_code, reason = "POSIX `fstat` FFI on an open descriptor with its SAFETY comment")]
    {
        let mut st = blaze_sys::posix::proc::stat::zeroed();
        // SAFETY: `fstat` writes to valid mutable `stat` buffer for open descriptor `fd`.
        let rc = unsafe { blaze_sys::posix::proc::fstat(fd.raw(), core::ptr::addr_of_mut!(st)) };
        if rc != 0 {
            return Err(blaze_sys::Errno::Io);
        }
        u64::try_from(st.st_size).map_err(|_| blaze_sys::Errno::Overflow)
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn combine_io_and_cleanup(
    primary: blaze_own::String<blaze_own::SharedAlloc<'static>>,
    cleanup: Result<(), blaze_own::String<blaze_own::SharedAlloc<'static>>>,
) -> blaze_own::String<blaze_own::SharedAlloc<'static>> {
    match cleanup {
        Ok(()) => primary,
        Err(unlink_err) => {
            let mut out = blaze_own::String::new_in(blaze_own::SharedAlloc::global());
            let _ = core::fmt::write(
                &mut out,
                format_args!("{}; temp-file cleanup also failed: {}", primary.as_str(), unlink_err.as_str()),
            );
            out
        }
    }
}

#[cfg(target_os = "linux")]
fn open_existing_dir_beneath_no_symlink(
    parent: blaze_sys::Fd,
    part: &str,
) -> Result<Option<blaze_sys::posix::io::OwnedFd>, blaze_own::String<blaze_own::SharedAlloc<'static>>> {
    use blaze_sys::linux::{OpenFlags, OpenHow, Resolve, openat2};
    let flags = OpenFlags::DIRECTORY | OpenFlags::CLOSE_ON_EXEC;
    let resolve = Resolve::BENEATH.with(Resolve::NO_SYMLINKS).with(Resolve::NO_MAGICLINKS);
    let how = OpenHow::new(flags, 0, resolve);
    with_component_cstr(part, |cpath| match openat2(parent, cpath, &how) {
        Ok(fd) => Ok(Some(fd)),
        Err(blaze_sys::Errno::NoEnt) => Ok(None),
        Err(errno) => Err(lock_err_errno("openat2 (read parent)", errno)),
    })
}

#[cfg(target_os = "macos")]
fn open_existing_dir_beneath_no_symlink(
    parent: blaze_sys::Fd,
    part: &str,
) -> Result<Option<blaze_sys::posix::io::OwnedFd>, blaze_own::String<blaze_own::SharedAlloc<'static>>> {
    use blaze_sys::posix::io::{OpenFlags, openat};
    let flags = OpenFlags::DIRECTORY | OpenFlags::NO_FOLLOW | OpenFlags::NO_FOLLOW_ANY | OpenFlags::CLOSE_ON_EXEC;
    with_component_cstr(part, |cpath| match openat(parent, cpath, flags, 0) {
        Ok(fd) => Ok(Some(fd)),
        Err(blaze_sys::Errno::NoEnt) => Ok(None),
        Err(errno) => Err(lock_err_errno("openat (read parent)", errno)),
    })
}

#[cfg(target_os = "linux")]
fn open_existing_file_beneath_no_symlink(
    parent: blaze_sys::Fd,
    leaf: &str,
) -> Result<Option<blaze_sys::posix::io::OwnedFd>, blaze_own::String<blaze_own::SharedAlloc<'static>>> {
    use blaze_sys::linux::{OpenFlags, OpenHow, Resolve, openat2};
    let flags = OpenFlags::READ | OpenFlags::CLOSE_ON_EXEC | OpenFlags::NONBLOCK;
    let resolve = Resolve::BENEATH.with(Resolve::NO_SYMLINKS).with(Resolve::NO_MAGICLINKS);
    let how = OpenHow::new(flags, 0, resolve);
    with_component_cstr(leaf, |cpath| match openat2(parent, cpath, &how) {
        Ok(fd) => Ok(Some(fd)),
        Err(blaze_sys::Errno::NoEnt) => Ok(None),
        Err(errno) => Err(lock_err_errno("openat2 (locked read)", errno)),
    })
}

#[cfg(target_os = "macos")]
fn open_existing_file_beneath_no_symlink(
    parent: blaze_sys::Fd,
    leaf: &str,
) -> Result<Option<blaze_sys::posix::io::OwnedFd>, blaze_own::String<blaze_own::SharedAlloc<'static>>> {
    use blaze_sys::posix::io::{OpenFlags, openat};
    let flags = OpenFlags::READ | OpenFlags::NO_FOLLOW | OpenFlags::NO_FOLLOW_ANY | OpenFlags::CLOSE_ON_EXEC | OpenFlags::NONBLOCK;
    with_component_cstr(leaf, |cpath| match openat(parent, cpath, flags, 0) {
        Ok(fd) => Ok(Some(fd)),
        Err(blaze_sys::Errno::NoEnt) => Ok(None),
        Err(errno) => Err(lock_err_errno("openat (locked read)", errno)),
    })
}

#[cfg(target_os = "linux")]
fn create_exclusive_file_beneath_no_symlink(
    parent: blaze_sys::Fd,
    leaf: &str,
) -> Result<blaze_sys::posix::io::OwnedFd, blaze_sys::Errno> {
    use blaze_sys::linux::{OpenFlags, OpenHow, Resolve, openat2};
    let flags = OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::EXCLUSIVE | OpenFlags::CLOSE_ON_EXEC;
    let resolve = Resolve::BENEATH.with(Resolve::NO_SYMLINKS).with(Resolve::NO_MAGICLINKS);
    let how = OpenHow::new(flags, 0o600, resolve);
    let bytes = leaf.as_bytes();
    if bytes.is_empty() || bytes.len() >= 256 || bytes.contains(&0) {
        return Err(blaze_sys::Errno::Inval);
    }
    let mut buf = [0u8; 256];
    let Some(dst) = buf.get_mut(..bytes.len()) else {
        return Err(blaze_sys::Errno::Inval);
    };
    dst.copy_from_slice(bytes);
    let Some(slice) = buf.get(..=bytes.len()) else {
        return Err(blaze_sys::Errno::Inval);
    };
    let Ok(cstr) = core::ffi::CStr::from_bytes_with_nul(slice) else {
        return Err(blaze_sys::Errno::Inval);
    };
    openat2(parent, blaze_sys::Path::new(cstr), &how)
}

#[cfg(target_os = "macos")]
fn create_exclusive_file_beneath_no_symlink(
    parent: blaze_sys::Fd,
    leaf: &str,
) -> Result<blaze_sys::posix::io::OwnedFd, blaze_sys::Errno> {
    use blaze_sys::posix::io::{OpenFlags, openat};
    let flags = OpenFlags::WRITE
        | OpenFlags::CREATE
        | OpenFlags::EXCLUSIVE
        | OpenFlags::NO_FOLLOW
        | OpenFlags::NO_FOLLOW_ANY
        | OpenFlags::CLOSE_ON_EXEC;
    let bytes = leaf.as_bytes();
    if bytes.is_empty() || bytes.len() >= 256 || bytes.contains(&0) {
        return Err(blaze_sys::Errno::Inval);
    }
    let mut buf = [0u8; 256];
    let Some(dst) = buf.get_mut(..bytes.len()) else {
        return Err(blaze_sys::Errno::Inval);
    };
    dst.copy_from_slice(bytes);
    let Some(slice) = buf.get(..=bytes.len()) else {
        return Err(blaze_sys::Errno::Inval);
    };
    let Ok(cstr) = core::ffi::CStr::from_bytes_with_nul(slice) else {
        return Err(blaze_sys::Errno::Inval);
    };
    openat(parent, blaze_sys::Path::new(cstr), flags, 0o600)
}

/// Planted bug for deterministic interleaving tests of [`LockedIoHandshake`].
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LockedIoPlant {
    None,
    /// Control: publishes `Some(res)` before decrementing `in_flight`, letting a fast caller call
    /// `FileLock::unlock` while `in_flight > 0` and spuriously deferring lock release.
    PublishBeforeDecrement,
}

/// Completion/counter handshake shared between [`run_locked_io_off_thread`] and its deterministic
/// interleaving test: guarantees `in_flight` and `active_workers` are decremented and any held
/// lock reference is released BEFORE publishing `Some(res)` to the caller.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub struct LockedIoHandshake<R: Send + 'static> {
    in_flight: blaze_own::Arc<core::sync::atomic::AtomicUsize, blaze_own::SharedAlloc<'static>>,
    active_workers: &'static core::sync::atomic::AtomicUsize,
    slot: blaze_own::Arc<self::std::sync::Mutex<Option<R>>, blaze_own::SharedAlloc<'static>>,
    signal: blaze_own::Arc<AttemptSignal, blaze_own::SharedAlloc<'static>>,
    plant: LockedIoPlant,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl<R: Send + 'static> LockedIoHandshake<R> {
    pub fn new(
        in_flight: blaze_own::Arc<core::sync::atomic::AtomicUsize, blaze_own::SharedAlloc<'static>>,
        active_workers: &'static core::sync::atomic::AtomicUsize,
        slot: blaze_own::Arc<self::std::sync::Mutex<Option<R>>, blaze_own::SharedAlloc<'static>>,
        signal: blaze_own::Arc<AttemptSignal, blaze_own::SharedAlloc<'static>>,
        plant: LockedIoPlant,
    ) -> Self {
        Self { in_flight, active_workers, slot, signal, plant }
    }

    /// Runs the completion handshake: under [`LockedIoPlant::None`], decrements `in_flight` and
    /// `active_workers` BEFORE storing `Some(res)` and waking `signal` (and invokes `between` right
    /// after publishing, before any deferred teardown, so a deterministic test can model a fast
    /// caller racing `unlock` against worker teardown).
    pub fn complete_with_hook<F: FnOnce()>(&self, res: R, between: F) {
        use core::sync::atomic::Ordering;
        match self.plant {
            LockedIoPlant::None => {
                self.in_flight.fetch_sub(1, Ordering::AcqRel);
                self.active_workers.fetch_sub(1, Ordering::AcqRel);
                {
                    let mut guard = match self.slot.lock() {
                        Ok(g) => g,
                        Err(p) => p.into_inner(),
                    };
                    *guard = Some(res);
                }
                self.signal.notify();
                between();
            }
            LockedIoPlant::PublishBeforeDecrement => {
                {
                    let mut guard = match self.slot.lock() {
                        Ok(g) => g,
                        Err(p) => p.into_inner(),
                    };
                    *guard = Some(res);
                }
                self.signal.notify();
                between();
                self.in_flight.fetch_sub(1, Ordering::AcqRel);
                self.active_workers.fetch_sub(1, Ordering::AcqRel);
            }
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn run_locked_io_off_thread<R, F>(
    cx: &crate::cx::Cx,
    lock: &NativeLock,
    op: F,
) -> Result<R, NativeLockErr>
where
    R: Send + 'static,
    F: FnOnce(&NativeRootDir, &core::sync::atomic::AtomicBool) -> Result<R, blaze_own::String<blaze_own::SharedAlloc<'static>>>
        + Send
        + 'static,
{
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use self::std::sync::Mutex;

    static ACTIVE_LOCKED_IO: AtomicUsize = AtomicUsize::new(0);

    if let Err(stopped) = cx.stopped_by(cx.io().now()) {
        return Err(NativeLockErr::Stopped(stopped));
    }
    let Some(locked_fd) = lock.fd.as_ref() else {
        return Err(lock_err_static("cannot perform locked I/O on an already-unlocked FileLock"));
    };
    let prev = ACTIVE_LOCKED_IO.fetch_add(1, Ordering::AcqRel);
    if prev >= MAX_LOCK_WORKERS {
        ACTIVE_LOCKED_IO.fetch_sub(1, Ordering::AcqRel);
        return Err(lock_err_static("locked-I/O worker cap reached (stalled filesystem backlog full)"));
    }
    locked_fd.in_flight.fetch_add(1, Ordering::AcqRel);

    // Panic-only safety net: on normal completion `disarm_for_handshake()` drops `locked` and
    // transfers counter decrement + completion publication to `LockedIoHandshake::complete_with_hook`.
    struct WorkerGuards {
        locked: Option<blaze_own::Arc<LockedFd, blaze_own::SharedAlloc<'static>>>,
        signal: blaze_own::Arc<AttemptSignal, blaze_own::SharedAlloc<'static>>,
        armed: bool,
    }
    impl WorkerGuards {
        fn disarm_for_handshake(&mut self) {
            self.armed = false;
            let _ = self.locked.take();
        }
    }
    impl Drop for WorkerGuards {
        fn drop(&mut self) {
            use core::sync::atomic::Ordering;
            if self.armed {
                self.armed = false;
                if let Some(locked) = self.locked.take() {
                    locked.in_flight.fetch_sub(1, Ordering::AcqRel);
                    drop(locked);
                }
                ACTIVE_LOCKED_IO.fetch_sub(1, Ordering::AcqRel);
                self.signal.notify();
            }
        }
    }

    let signal = blaze_own::Arc::new_in(AttemptSignal::new(), blaze_own::SharedAlloc::global());
    let cancelled = blaze_own::Arc::new_in(AtomicBool::new(false), blaze_own::SharedAlloc::global());
    let slot: blaze_own::Arc<
        Mutex<Option<Result<R, blaze_own::String<blaze_own::SharedAlloc<'static>>>>>,
        blaze_own::SharedAlloc<'static>,
    > = blaze_own::Arc::new_in(Mutex::new(None), blaze_own::SharedAlloc::global());

    let root_w = blaze_own::Share::share(&lock.root_dir);
    let locked_w = blaze_own::Share::share(locked_fd);
    let in_flight_w = blaze_own::Share::share(&locked_fd.in_flight);
    let signal_w = blaze_own::Share::share(&signal);
    let cancelled_w = blaze_own::Share::share(&cancelled);
    let slot_w = blaze_own::Share::share(&slot);

    let spawned = self::std::thread::Builder::new()
        .name(self::std::string::String::from("bstd-locked-io"))
        .spawn(move || {
            let mut guards = WorkerGuards { locked: Some(locked_w), signal: blaze_own::Share::share(&signal_w), armed: true };
            let res = op(&root_w, &cancelled_w);
            guards.disarm_for_handshake();
            let handshake = LockedIoHandshake::new(
                in_flight_w,
                &ACTIVE_LOCKED_IO,
                slot_w,
                signal_w,
                LockedIoPlant::None,
            );
            handshake.complete_with_hook(res, || {});
        });
    if spawned.is_err() {
        locked_fd.in_flight.fetch_sub(1, Ordering::AcqRel);
        ACTIVE_LOCKED_IO.fetch_sub(1, Ordering::AcqRel);
        return Err(lock_err_static("locked-I/O worker spawn failed"));
    }

    loop {
        {
            let mut guard = match slot.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            if let Some(res) = guard.take() {
                return res.map_err(NativeLockErr::Driver);
            }
        }
        if let Err(stopped) = cx.stopped_by(cx.io().now()) {
            cancelled.store(true, Ordering::Release);
            return Err(NativeLockErr::Stopped(stopped));
        }
        signal.wait_timeout(core::time::Duration::from_millis(10));
    }
}

/// Reads `rel` (bounded at `max_bytes` before allocating) strictly beneath `lock.root_dir.fd()`
/// on a bounded off-thread worker that honors `cx` deadline/cancellation without traversing
/// symlinks, sharing the exact opened directory inode holding the flock.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn read_bounded_under_lock_off_thread<'a>(
    cx: &crate::cx::Cx,
    lock: &NativeLock,
    rel: &str,
    max_bytes: u64,
    single_read_plant: bool,
    alloc: blaze_own::Alloc<'a>,
) -> Result<NativeBoundedRead<'a>, NativeLockErr> {
    let rel_owned = blaze_own::String::from_str_in(rel, blaze_own::SharedAlloc::global());
    let shared = run_locked_io_off_thread(cx, lock, move |root_dir, cancelled| {
        read_bounded_under_lock_sync(root_dir, rel_owned.as_str(), max_bytes, single_read_plant, cancelled)
    })?;
    match shared {
        SharedBoundedRead::Absent => Ok(NativeBoundedRead::Absent),
        SharedBoundedRead::Oversized => Ok(NativeBoundedRead::Oversized),
        SharedBoundedRead::Grew => Ok(NativeBoundedRead::Grew),
        SharedBoundedRead::Whole(shared_bytes) => {
            let mut out = blaze_own::Vec::with_capacity_in(shared_bytes.len(), alloc);
            out.extend_from_slice(shared_bytes.as_slice());
            Ok(NativeBoundedRead::Whole(out))
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn read_bounded_under_lock_sync(
    root_dir: &NativeRootDir,
    rel: &str,
    max_bytes: u64,
    single_read_plant: bool,
    cancelled: &core::sync::atomic::AtomicBool,
) -> Result<SharedBoundedRead, blaze_own::String<blaze_own::SharedAlloc<'static>>> {
    use core::sync::atomic::Ordering;

    if cancelled.load(Ordering::Acquire) {
        return Err(blaze_own::String::from_str_in("locked read cancelled", blaze_own::SharedAlloc::global()));
    }
    let opened = root_dir.require_opened()?;
    let root_fd = opened.fd.fd();
    let mut parts = rel.split('/').peekable();
    let mut owned_dir: Option<blaze_sys::posix::io::OwnedFd> = None;
    let mut leaf: Option<&str> = None;
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            leaf = Some(part);
            break;
        }
        let cur = owned_dir.as_ref().map_or(root_fd, |d| d.fd());
        let Some(next) = open_existing_dir_beneath_no_symlink(cur, part)? else {
            return Ok(SharedBoundedRead::Absent);
        };
        owned_dir = Some(next);
    }
    let Some(leaf_name) = leaf else {
        return Err(blaze_own::String::from_str_in("empty locked read path", blaze_own::SharedAlloc::global()));
    };
    let cur = owned_dir.as_ref().map_or(root_fd, |d| d.fd());
    let Some(file) = open_existing_file_beneath_no_symlink(cur, leaf_name)? else {
        return Ok(SharedBoundedRead::Absent);
    };
    let len_u64 = fd_len_u64(file.fd()).map_err(|e| lock_err_errno("fstat (locked read)", e))?;
    if len_u64 > max_bytes {
        return Ok(SharedBoundedRead::Oversized);
    }
    let len = usize::try_from(len_u64)
        .map_err(|_| blaze_own::String::from_str_in("locked read length overflow", blaze_own::SharedAlloc::global()))?;
    let cap = len
        .checked_add(1)
        .ok_or_else(|| blaze_own::String::from_str_in("locked read capacity overflow", blaze_own::SharedAlloc::global()))?;
    let mut bytes = blaze_own::Vec::with_capacity_in(cap, blaze_own::SharedAlloc::global());
    bytes.resize(cap, 0u8);
    let mut filled = 0usize;
    while filled < cap {
        if cancelled.load(Ordering::Acquire) {
            return Err(blaze_own::String::from_str_in("locked read cancelled", blaze_own::SharedAlloc::global()));
        }
        let step_off = u64::try_from(filled)
            .map_err(|_| blaze_own::String::from_str_in("locked read offset overflow", blaze_own::SharedAlloc::global()))?;
        let Some(dst) = bytes.as_mut_slice().get_mut(filled..) else {
            break;
        };
        let n = match blaze_sys::posix::io::pread(file.fd(), dst, step_off) {
            Ok(n) => n,
            Err(blaze_sys::Errno::Intr) => continue,
            Err(errno) => return Err(lock_err_errno("pread (locked read)", errno)),
        };
        if n == 0 {
            break;
        }
        filled += n;
        if single_read_plant {
            break;
        }
    }
    if filled > len {
        return Ok(SharedBoundedRead::Grew);
    }
    bytes.truncate(filled);
    Ok(SharedBoundedRead::Whole(bytes))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn sync_file_for_publish(
    fd: blaze_sys::Fd,
    synced: bool,
) -> Result<(), blaze_own::String<blaze_own::SharedAlloc<'static>>> {
    if !synced {
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    {
        blaze_sys::posix::io::fsync(fd).map_err(|e| lock_err_errno("fsync (locked publish)", e))
    }
    #[cfg(target_os = "macos")]
    {
        blaze_sys::posix::io::full_fsync(fd).map_err(|e| lock_err_errno("F_FULLFSYNC (locked publish)", e))
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn sync_dir_for_publish(
    parent_fd: blaze_sys::Fd,
    synced: bool,
) -> Result<(), blaze_own::String<blaze_own::SharedAlloc<'static>>> {
    if !synced {
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    {
        blaze_sys::posix::io::fsync(parent_fd).map_err(|e| lock_err_errno("fsync dir (locked publish)", e))
    }
    #[cfg(target_os = "macos")]
    {
        blaze_sys::posix::io::full_fsync(parent_fd)
            .map_err(|e| lock_err_errno("F_FULLFSYNC dir (locked publish)", e))
    }
}

/// Atomically publishes `whole` at `rel` strictly beneath `lock.root_dir.fd()` on a bounded
/// off-thread worker that honors `cx` deadline/cancellation, sharing the exact opened directory
/// inode holding the flock. When `synced` is `true`, syncs file bytes before rename and parent
/// directory after; when `false`, performs no device sync (`Durable::No`).
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn publish_under_lock_off_thread(
    cx: &crate::cx::Cx,
    lock: &NativeLock,
    rel: &str,
    whole: &[u8],
    synced: bool,
) -> Result<(), NativeLockErr> {
    let rel_owned = blaze_own::String::from_str_in(rel, blaze_own::SharedAlloc::global());
    let mut whole_owned = blaze_own::Vec::with_capacity_in(whole.len(), blaze_own::SharedAlloc::global());
    whole_owned.extend_from_slice(whole);
    run_locked_io_off_thread(cx, lock, move |root_dir, cancelled| {
        publish_under_lock_sync(root_dir, rel_owned.as_str(), whole_owned.as_slice(), synced, cancelled)
    })
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn publish_under_lock_sync(
    root_dir: &NativeRootDir,
    rel: &str,
    whole: &[u8],
    synced: bool,
    cancelled: &core::sync::atomic::AtomicBool,
) -> Result<(), blaze_own::String<blaze_own::SharedAlloc<'static>>> {
    use core::sync::atomic::{AtomicU64, Ordering};
    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
    const OPEN_TEMP_CALL: &str = if cfg!(target_os = "linux") {
        "openat2 (locked publish temp)"
    } else {
        "openat (locked publish temp)"
    };

    if cancelled.load(Ordering::Acquire) {
        return Err(blaze_own::String::from_str_in("locked publish cancelled", blaze_own::SharedAlloc::global()));
    }
    let opened = root_dir.require_opened()?;
    let root_fd = opened.fd.fd();
    let mut parts = rel.split('/').peekable();
    let mut owned_dir: Option<blaze_sys::posix::io::OwnedFd> = None;
    let mut leaf: Option<&str> = None;
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            leaf = Some(part);
            break;
        }
        let cur = owned_dir.as_ref().map_or(root_fd, |d| d.fd());
        let next = open_dir_beneath_no_symlink(cur, part)?;
        owned_dir = Some(next);
    }
    let Some(leaf_name) = leaf else {
        return Err(blaze_own::String::from_str_in("empty locked publish path", blaze_own::SharedAlloc::global()));
    };
    let parent_fd = owned_dir.as_ref().map_or(root_fd, |d| d.fd());
    let pid = self::std::process::id();
    let mut tmp_name = blaze_own::String::new_in(blaze_own::SharedAlloc::global());
    let mut tmp_fd: Option<blaze_sys::posix::io::OwnedFd> = None;
    for _ in 0..64u32 {
        let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        tmp_name.clear();
        let _ = core::fmt::write(&mut tmp_name, format_args!(".tmp.{pid}.{seq}"));
        match create_exclusive_file_beneath_no_symlink(parent_fd, tmp_name.as_str()) {
            Ok(fd) => {
                tmp_fd = Some(fd);
                break;
            }
            Err(blaze_sys::Errno::Exist) => continue,
            Err(errno) => return Err(lock_err_errno(OPEN_TEMP_CALL, errno)),
        }
    }
    let Some(fd) = tmp_fd else {
        return Err(blaze_own::String::from_str_in(
            "failed to allocate unique temp file for locked publish",
            blaze_own::SharedAlloc::global(),
        ));
    };
    let unlink_tmp = |name: &str| -> Result<(), blaze_own::String<blaze_own::SharedAlloc<'static>>> {
        with_component_cstr(name, |cpath| {
            blaze_sys::posix::proc::unlink_at(
                parent_fd,
                cpath,
                blaze_sys::posix::proc::Unlink::File,
            )
            .map_err(|e| lock_err_errno("unlinkat (locked publish temp)", e))
        })
    };
    let mut written = 0usize;
    while written < whole.len() {
        if cancelled.load(Ordering::Acquire) {
            drop(fd);
            let cleanup = unlink_tmp(tmp_name.as_str());
            return Err(combine_io_and_cleanup(
                blaze_own::String::from_str_in("locked publish cancelled", blaze_own::SharedAlloc::global()),
                cleanup,
            ));
        }
        let Some(chunk) = whole.get(written..) else {
            break;
        };
        match blaze_sys::posix::io::write(fd.fd(), chunk) {
            Ok(0) => {
                drop(fd);
                let cleanup = unlink_tmp(tmp_name.as_str());
                return Err(combine_io_and_cleanup(
                    blaze_own::String::from_str_in(
                        "zero-byte write during locked publish",
                        blaze_own::SharedAlloc::global(),
                    ),
                    cleanup,
                ));
            }
            Ok(n) => written += n,
            Err(blaze_sys::Errno::Intr) => continue,
            Err(errno) => {
                drop(fd);
                let cleanup = unlink_tmp(tmp_name.as_str());
                return Err(combine_io_and_cleanup(lock_err_errno("write (locked publish)", errno), cleanup));
            }
        }
    }
    if let Err(err) = sync_file_for_publish(fd.fd(), synced) {
        drop(fd);
        let cleanup = unlink_tmp(tmp_name.as_str());
        return Err(combine_io_and_cleanup(err, cleanup));
    }
    drop(fd);
    if cancelled.load(Ordering::Acquire) {
        let cleanup = unlink_tmp(tmp_name.as_str());
        return Err(combine_io_and_cleanup(
            blaze_own::String::from_str_in("locked publish cancelled before rename", blaze_own::SharedAlloc::global()),
            cleanup,
        ));
    }
    let renamed = with_component_cstr(tmp_name.as_str(), |from_cpath| {
        with_component_cstr(leaf_name, |to_cpath| {
            blaze_sys::posix::proc::rename_at(parent_fd, from_cpath, parent_fd, to_cpath)
                .map_err(|e| lock_err_errno("renameat (locked publish)", e))
        })
    });
    if let Err(err) = renamed {
        let cleanup = unlink_tmp(tmp_name.as_str());
        return Err(combine_io_and_cleanup(err, cleanup));
    }
    sync_dir_for_publish(parent_fd, synced)?;
    Ok(())
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

#[cfg(test)]
mod tests {
    #[cfg(any(target_os = "linux", target_os = "macos", windows, target_os = "wasi"))]
    #[test]
    fn open_native_with_telemetry_configures_workers_in_one_shot() {
        use super::{ArgList, TelemetryOpts, Vars, open_native_with_telemetry, start_dir};

        let bare = open_native_with_telemetry(
            blaze_core::testing::os(),
            ArgList::given(alloc::vec::Vec::new()),
            Vars::given(alloc::vec::Vec::new()),
            start_dir(),
            0,
            None,
        )
        .expect("bare native world");
        let bare_lease = bare.workers().lease().expect("bare caller cell");
        let mut bare_held = bare.workers().hold(bare_lease.id()).expect("hold bare cell");
        assert!(bare_held.worker().telemetry().is_none(), "bare native world has no telemetry cell");
        drop((bare_held, bare_lease));

        let with_tel = open_native_with_telemetry(
            blaze_core::testing::os(),
            ArgList::given(alloc::vec::Vec::new()),
            Vars::given(alloc::vec::Vec::new()),
            start_dir(),
            0,
            Some(TelemetryOpts {
                level: crate::telemetry::Level::Spans,
                alloc: blaze_own::testing::heap().shared(),
            }),
        )
        .expect("telemetry native world");
        let tel_lease = with_tel.workers().lease().expect("telemetry caller cell");
        let mut tel_held = with_tel.workers().hold(tel_lease.id()).expect("hold telemetry cell");
        assert!(tel_held.worker().telemetry().is_some(), "single-shot native world keeps telemetry cell");
        drop((tel_held, tel_lease));
    }
}
