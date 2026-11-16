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

use alloc::{string::{String, ToString as _}, sync::Arc, vec::Vec};
use core::{fmt::Write as _, time::Duration};

use crate::{
    cx::Cx,
    entry::{Outcome, Status, run_on},
    io::{Io, Limits, NativeWorld},
    task::threads::{Plants, Topology},
};

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
/// per core of the machine's [`topology`], started here and stopped when `main` returns, each
/// given [`POOL_STOP`] to leave; a worker that does not is reported by name and the process
/// fails.
pub fn run<R: Outcome>(main: impl FnOnce(&Cx, Io) -> R) -> Exit {
    // alloc: once per process, what a failure says.
    let mut said = String::new();
    let status = match native() {
        Ok(io) => match topology().map_err(pool::StartError::from).and_then(|machine| pool::start(&machine, Plants::NONE)) {
            Ok(running) => {
                let status = run_on(io, running.pool().clone(), &mut said, main);
                match running.stop(POOL_STOP) {
                    Ok(()) => status,
                    Err(hung) => {
                        if !said.is_empty() {
                            said.push_str("; ");
                        }
                        let _ = write!(said, "{hung}");
                        Status::FAILED
                    }
                }
            }
            Err(why) => {
                let _ = write!(said, "{why}");
                Status::FAILED
            }
        },
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

    use std::{format, string::String, sync::mpsc, thread, vec::Vec as StdVec};

    use alloc::string::ToString as _;
    use core::time::Duration;

    use blaze_core::wait::{Clock, Deadline, Monotonic, Parker};

    use super::machine;
    use crate::task::threads::{Hung, Plants, Pool, Topology};

    /// Each worker's stack: tasks run on it to their end, and a waiting worker runs its own
    /// queued tasks on top of the wait, so it is deeper than a thread's usual 2 MiB.
    pub(crate) const WORKER_STACK: usize = 16 << 20;

    /// How long the pool's threads have to start and report their pinning.
    pub(crate) const START_WAIT: Duration = Duration::from_secs(5);

    /// A running pool and its threads.
    pub(crate) struct Running {
        pool: Pool,
        threads: StdVec<Option<thread::JoinHandle<()>>>,
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

    /// The pool of `topology`, its threads started, each pinned or given its QoS first.
    pub(crate) fn start(topology: &Topology, plants: Plants) -> Result<Running, StartError> {
        let pool = Pool::new(topology, Monotonic::new(), plants);
        let (told, heard) = mpsc::channel::<(u32, Result<(), String>)>();
        let mut threads = StdVec::new();
        let mut at = 0u32;
        for group in &topology.groups {
            for &cpu in group.cpus.iter() {
                let (pool, told, class, pin) = (pool.clone(), told.clone(), group.class, topology.pin);
                let me = at;
                let spawned = thread::Builder::new()
                    .name(format!("bstd-worker-{me}"))
                    .stack_size(WORKER_STACK)
                    .spawn(move || {
                        let placed = if pin { machine::place(cpu, class) } else { Ok(()) };
                        let ok = placed.is_ok();
                        let _ = told.send((me, placed));
                        drop(told);
                        if ok {
                            pool.work(me);
                        }
                    });
                match spawned {
                    Ok(handle) => threads.push(Some(handle)),
                    Err(error) => {
                        let running = Running { pool, threads };
                        let _ = running.stop(super::POOL_STOP);
                        return Err(StartError { worker: me, why: error.to_string() });
                    }
                }
                at += 1;
            }
        }
        drop(told);
        let mut failed = None;
        for _ in 0..at {
            match heard.recv_timeout(START_WAIT) {
                Ok((_, Ok(()))) => {}
                Ok((worker, Err(why))) => {
                    failed.get_or_insert(StartError { worker, why });
                }
                Err(error) => {
                    failed.get_or_insert(StartError { worker: at, why: format!("no word within {START_WAIT:?}: {error}") });
                    break;
                }
            }
        }
        let running = Running { pool, threads };
        match failed {
            None => Ok(running),
            Some(failed) => {
                let _ = running.stop(super::POOL_STOP);
                Err(failed)
            }
        }
    }

    impl Running {
        pub(crate) fn pool(&self) -> &Pool {
            &self.pool
        }

        /// Stops every worker once it finds nothing to run, and joins each that left its loop
        /// within `within`; the rest are named, with the task each still runs, and left
        /// detached (the process is ending).
        pub(crate) fn stop(mut self, within: Duration) -> Result<(), Hung> {
            self.pool.stop();
            let clock = Monotonic::new();
            let until = Deadline::after(clock.now(), within);
            let nap = Parker::new();
            let count = u32::try_from(self.threads.len()).unwrap_or(u32::MAX);
            while !(0..count).all(|at| self.pool.exited(at)) && !until.passed(clock.now()) {
                // Nobody unparks it: a timed nap of the one kernel wait (rule 44).
                let slice = Deadline::after(clock.now(), Duration::from_millis(1));
                if nap.park("bstd.rt.pool.stop", Some(slice), &clock).is_err() {
                    break;
                }
            }
            for (at, slot) in self.threads.iter_mut().enumerate() {
                let at = u32::try_from(at).unwrap_or(u32::MAX);
                if self.pool.exited(at)
                    && let Some(handle) = slot.take()
                {
                    // A worker's loop does not panic (its tasks' panics are caught): a join
                    // error is the pool's own bug, reported as the worker's.
                    if handle.join().is_err() {
                        panic!("bstd::rt: the pool's worker {at} panicked outside a task");
                    }
                }
            }
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

    use std::{format, fs, string::String, vec::Vec as StdVec};

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
        let online = fs::read_to_string("/sys/devices/system/cpu/online").map_err(|error| failed("/sys/devices/system/cpu/online", error))?;
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
            let Ok(level) = fs::read_to_string(format!("{dir}/level")) else {
                break;
            };
            let level: u32 = level.trim().parse().map_err(|error| failed("a cache's level", error))?;
            let shared = fs::read_to_string(format!("{dir}/shared_cpu_list")).map_err(|error| failed("a cache's shared_cpu_list", error))?;
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
        use core::ffi::{c_char, c_int, c_void};

        /// `QOS_CLASS_USER_INITIATED` (`<sys/qos.h>`): what a foreground worker runs at.
        pub(crate) const QOS_USER_INITIATED: u32 = 0x19;
        /// `QOS_CLASS_UTILITY`: a background worker, which the scheduler keeps to efficiency
        /// cores when the performance cores are busy.
        pub(crate) const QOS_UTILITY: u32 = 0x11;

        unsafe extern "C" {
            /// `sysctlbyname(3)`.
            pub(crate) fn sysctlbyname(
                name: *const c_char,
                oldp: *mut c_void,
                oldlenp: *mut usize,
                newp: *mut c_void,
                newlen: usize,
            ) -> c_int;
            /// `pthread_set_qos_class_self_np` (`<pthread/qos.h>`): Darwin's own call, not
            /// POSIX (rule 45): it is how a thread says what kind of work it does, which is
            /// what places it on a performance or an efficiency core; there is no affinity
            /// call on macOS. Its symbol is tested to exist (rt's tests).
            pub(crate) fn pthread_set_qos_class_self_np(class: u32, relative: c_int) -> c_int;
        }
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
        use super::cpu_list;

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

/// The process's world, or why it cannot be made.
fn native() -> Result<Io, String> {
    // alloc: once per process, its command line.
    let mut args = Vec::new();
    for (at, arg) in std::env::args_os().enumerate() {
        match arg.into_string() {
            Ok(arg) => args.push(arg),
            Err(arg) => {
                // alloc: an error's message.
                let mut why = String::new();
                let _ = write!(
                    why,
                    "bstd::entry: argument {at} of the command line is not UTF-8 ({}): `Args` holds text",
                    arg.to_string_lossy()
                );
                return Err(why);
            }
        }
    }
    // alloc: once per process, its environment.
    let env: Vec<(String, String)> = std::env::vars_os()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
        .collect();
    let root = std::env::current_dir().map_err(|error| {
        // alloc: an error's message.
        let mut why = String::new();
        let _ = write!(why, "bstd::entry: the directory the program was started in: {error}");
        why
    })?;
    let seed = {
        use std::hash::BuildHasher as _;
        std::hash::RandomState::new().hash_one(0u8)
    };
    let world = NativeWorld {
        root: &root,
        threads: FILE_THREADS,
        env,
        args,
        // alloc: once per process, the host's name (empty: see `run`).
        host_name: Arc::from(""),
        seed,
        limits: Limits::USUAL,
    };
    // alloc: an error's message.
    Io::native(world).map_err(|error| error.to_string())
}

/// `said` on standard error, as `error: <said>` and a line feed. A standard error that
/// cannot be written to changes nothing: the status still says the program failed.
fn error_line(said: &str) {
    use std::io::Write as _;
    let mut err = std::io::stderr().lock();
    let _ = err.write_all(b"error: ").and_then(|()| err.write_all(said.as_bytes())).and_then(|()| err.write_all(b"\n"));
}

/// `parts`, in order, on the process's `stream`, with the operating system when this
/// returns: all of them or the host's refusal as it says it. Standard output goes through
/// Rust std's handle, whose buffer is flushed here, so `parts` reach the system as one write
/// when they fit it and never wait in it; standard error has no buffer.
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
