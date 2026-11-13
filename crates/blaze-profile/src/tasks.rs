//! What every task of the daemon is doing now, read for the task watchdog ([`crate::stuck`],
//! the sans-IO core): this is its reader, the IO half.
//!
//! A look reads, with nothing allocated once its vectors have grown:
//! - each live thread running a scheduler job: its innermost open span and since when, its
//!   build and plane node, and its own CPU clock ([`flight::running_into`]); a thread running
//!   no job (a worker between jobs, a driver's thread) is no task, its waits are for work;
//! - each thread's wait in the lock module's wait graph, with the lock's holder (the thread
//!   mark its acquire stamped, the profiler's tid) and a deadline where the wait has one
//!   ([`blaze_core::lock::graph::snapshot_into`]);
//! - each parked scheduler job and the job it joins or whose key's flight it waits on
//!   ([`blaze_graph::sched::waits_now_into`]). A wait on a scope's jobs, a pool's stop or an
//!   [`blaze_graph::sched::Event`] (no job holds it) is left out: the first ends when jobs the
//!   watchdog sees end, the others are waits on the world outside, and a job that waits on a
//!   job that is only queued, not running, waits for a core, which is no stall of its own;
//! - each child process a seen thread waits for in place, judged by the child's own CPU
//!   ([`blaze_core::children::each_child`]).
//!
//! [`Reader::told`] says a [`Stuck`] for people: what the task is doing, what it waits on, who
//! holds that and what the holder is inside, from [`flight::threads_now`] (only when something
//! is stuck, so that read's allocations are off the quiet path).

use std::alloc::Global;

use blaze_core::lock::stall::{Blocked, WaitRow};
use blaze_graph::sched::StallWait;

use crate::{
    flight::{self, OpenSpan, Ring, Running, ThreadNow},
    stuck::{Stuck, Task, TaskSeen, WaitsOn, Why},
    watchdog,
};

/// What the driver needs to act on a task it saw, beside its [`TaskSeen`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct About {
    /// The scheduler job it runs or is, as `blaze_graph::sched::observed` numbers it.
    pub job: Option<u64>,
    /// The plane node of the daemon `task` span it is inside, if any.
    pub node: Option<u64>,
}

/// The reader, its scratch reused look to look.
#[derive(Default)]
pub struct Reader {
    rings: Vec<&'static Ring, Global>,
    running: Vec<Running, Global>,
    waits: Vec<WaitRow, Global>,
    parked: Vec<(u64, StallWait, Option<u64>), Global>,
    children: Vec<(libc::pid_t, Option<u64>), Global>,
    /// The tasks of the last look, and what the driver needs of each, index for index.
    pub seen: Vec<TaskSeen, Global>,
    pub about: Vec<About, Global>,
}

impl Reader {
    /// Reads every task now into [`Reader::seen`], on [`crate::clock::mono_ns`].
    pub fn read(&mut self) {
        flight::running_into(&mut self.rings, &mut self.running);
        blaze_core::lock::graph::snapshot_into(&mut self.waits);
        blaze_graph::sched::waits_now_into(&mut self.parked);
        self.children.clear();
        #[cfg(unix)]
        blaze_core::children::each_child(|pid, waiter| self.children.push((pid, waiter)));
        self.seen.clear();
        self.about.clear();
        let now_ns = crate::clock::mono_ns() as u64;
        for run in &self.running {
            let waits = self.waits_of(run.tid);
            self.seen.push(TaskSeen {
                task: Task::Thread(run.tid),
                build: run.build,
                span: run.span,
                span_since_ns: run.span_since_ns,
                cpu_ns: run.cpu_ns,
                waits,
                bound_ns: None,
            });
            self.about.push(About { job: Some(run.job), node: run.node });
        }
        for &(pid, waiter) in &self.children {
            let Some(waiter) = waiter else { continue };
            // Of its waiter's build and node: the child is that task's work.
            let Some(run) = self.running.iter().find(|it| it.tid == waiter) else {
                continue;
            };
            let (build, node) = (run.build, run.node);
            self.seen.push(TaskSeen {
                task: Task::Child(pid as u32),
                build,
                span: 0,
                span_since_ns: now_ns,
                cpu_ns: child_cpu_ns(pid),
                waits: WaitsOn::Nothing,
                bound_ns: None,
            });
            self.about.push(About { job: None, node });
        }
        for &(job, wait, on) in &self.parked {
            let Some(on) = on else { continue };
            let target = match self.running.iter().find(|it| it.job == on) {
                Some(run) => Task::Thread(run.tid),
                None if self.parked.iter().any(|it| it.0 == on) => Task::Job(on),
                None => continue,
            };
            // A parked job moves only as its wait does: its span is its wait.
            let span = match wait {
                StallWait::Job(_) => 1,
                _ => 2,
            } ^ on.rotate_left(2);
            self.seen.push(TaskSeen {
                task: Task::Job(job),
                build: 0,
                span,
                span_since_ns: now_ns,
                cpu_ns: None,
                waits: WaitsOn::Join { task: target },
                bound_ns: None,
            });
            self.about.push(About { job: Some(job), node: None });
        }
    }

    /// What thread `tid` waits on: a child it started and waits for in place, else its row in
    /// the wait graph, else nothing named.
    fn waits_of(&self, tid: u64) -> WaitsOn {
        if let Some(&(pid, _)) = self.children.iter().find(|it| it.1 == Some(tid)) {
            return WaitsOn::Child { pid: pid as u32 };
        }
        let Some(row) = self.waits.iter().find(|it| it.thread == tid) else {
            return WaitsOn::Nothing;
        };
        match row.on {
            Blocked::Lock { name, .. } => {
                WaitsOn::Lock { name, holder: row.holder.map(Task::Thread) }
            }
            Blocked::Signal(what) => WaitsOn::Named { what, deadline_ns: row.deadline_ns },
            Blocked::Wait(what) if what.starts_with("store") => WaitsOn::Store { what },
            Blocked::Wait(what) => WaitsOn::Named { what, deadline_ns: row.deadline_ns },
        }
    }

    /// What the driver knows of `task` from the last look.
    pub fn about(&self, task: Task) -> About {
        self.seen.iter().position(|it| it.task == task).map(|i| self.about[i]).unwrap_or_default()
    }

    /// `stuck` said for people, with every live thread's spans read now (`threads`, from
    /// [`flight::threads_now`]).
    #[expect(blaze::implicit_allocator, reason = "a stuck task's report, once per stuck task")]
    pub fn told(&self, stuck: &Stuck, threads: &[ThreadNow]) -> Told {
        let what = Self::what(stuck.task, threads);
        let waits = said_wait(&stuck.waits, threads);
        let held_by = stuck.held_by.map(|it| Self::what(it, threads));
        let inside = stuck.held_by.and_then(|it| inside(it, threads));
        let mut cycle = Vec::new_in(Global);
        for edge in &stuck.cycle {
            cycle.push(format!(
                "{} waits on {}, held by {}",
                Self::what(edge.task, threads),
                said_wait(&edge.on, threads),
                Self::what(edge.held_by, threads),
            ));
        }
        let about = self.about(stuck.task);
        Told {
            still_ns: stuck.still_ns,
            build: stuck.build,
            task: stuck.task,
            job: about.job,
            node: about.node,
            what,
            waits,
            held_by,
            inside,
            why: stuck.why,
            cycle,
        }
    }

    /// What `task` is doing: its working-on span (one that names a function, unit or crate),
    /// else its innermost span, else what it is.
    #[expect(blaze::implicit_allocator, reason = "a stuck task's report, once per stuck task")]
    pub fn what(task: Task, threads: &[ThreadNow]) -> String {
        match task {
            Task::Thread(tid) => match threads.iter().find(|it| it.tid == tid) {
                Some(thread) => match working_on(&thread.open) {
                    Some(span) => said_span(span),
                    None => format!("thread {} (tid {tid})", thread.name),
                },
                None => format!("thread tid {tid}"),
            },
            Task::Job(job) => format!("parked job {job:#x}"),
            Task::Child(pid) => format!("child process {pid}"),
            Task::Loop => "the daemon's event loop".to_owned(),
        }
    }
}

/// The innermost span of `open` that says what it is of, else the innermost.
fn working_on(open: &[OpenSpan]) -> Option<&OpenSpan> {
    open.iter()
        .rev()
        .find(|it| it.function.is_some() || it.unit.is_some() || it.krate.is_some())
        .or(open.last())
}

#[expect(blaze::implicit_allocator, reason = "a stuck task's report, once per stuck task")]
fn said_span(span: &OpenSpan) -> String {
    let mut out = String::new();
    // Writing to a `String` does not fail.
    _ = watchdog::open_span(&mut out, span);
    out
}

/// The span chain of `task`, outermost first, if it is a thread inside spans.
#[expect(blaze::implicit_allocator, reason = "a stuck task's report, once per stuck task")]
fn inside(task: Task, threads: &[ThreadNow]) -> Option<String> {
    let Task::Thread(tid) = task else { return None };
    let thread = threads.iter().find(|it| it.tid == tid)?;
    if thread.open.is_empty() {
        return None;
    }
    let mut out = String::new();
    for (i, span) in thread.open.iter().enumerate() {
        if i > 0 {
            out.push_str(" > ");
        }
        _ = watchdog::open_span(&mut out, span);
    }
    Some(out)
}

#[expect(blaze::implicit_allocator, reason = "a stuck task's report, once per stuck task")]
fn said_wait(waits: &WaitsOn, threads: &[ThreadNow]) -> String {
    match *waits {
        WaitsOn::Nothing => "nothing named: no CPU and the same span".to_owned(),
        WaitsOn::Named { what, deadline_ns: None } => format!("signal {what}"),
        WaitsOn::Named { what, deadline_ns: Some(_) } => {
            format!("signal {what}, its deadline passed")
        }
        WaitsOn::Lock { name, .. } => format!("lock {name}"),
        WaitsOn::Join { task } => format!("task {}", Reader::what(task, threads)),
        WaitsOn::Child { pid } => format!("child process {pid}"),
        WaitsOn::Store { what } => format!("store read {what}"),
    }
}

/// A stuck task said for people (the daemon says it as `blaze_protocol::now::Stuck`, whose
/// line is the one form), and what the driver acts on.
#[derive(Clone, Debug, PartialEq, Eq)] // clone: a report, once per stuck task
#[expect(blaze::implicit_allocator, reason = "a stuck task's report, once per stuck task")]
pub struct Told {
    pub still_ns: u64,
    pub build: u32,
    pub task: Task,
    pub job: Option<u64>,
    pub node: Option<u64>,
    pub what: String,
    pub waits: String,
    pub held_by: Option<String>,
    pub inside: Option<String>,
    pub why: Why,
    /// For a cycle, each edge as `A waits on X, held by B`.
    pub cycle: Vec<String, Global>,
}

/// A child process's own CPU time: Linux, `clock_gettime` of its process CPU clock
/// (`clock_getcpuclockid`); macOS, `proc_pidinfo(PROC_PIDTASKINFO)`'s user and system time in
/// Mach time units, scaled by `mach_timebase_info`. `None` where it cannot be read (it exited,
/// or the system refused).
#[cfg(target_os = "linux")]
fn child_cpu_ns(pid: libc::pid_t) -> Option<u64> {
    let mut clock: libc::clockid_t = 0;
    // SAFETY: `clock` is a clockid to fill; a pid of no process fails with ESRCH.
    if unsafe { libc::clock_getcpuclockid(pid, &mut clock) } != 0 {
        return None;
    }
    let mut at = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `at` is a timespec to fill.
    let got = unsafe { libc::clock_gettime(clock, &mut at) };
    (got == 0).then(|| at.tv_sec as u64 * 1_000_000_000 + at.tv_nsec as u64)
}

#[cfg(target_os = "macos")]
fn child_cpu_ns(pid: libc::pid_t) -> Option<u64> {
    let mut info = std::mem::MaybeUninit::<libc::proc_taskinfo>::zeroed();
    let size = size_of::<libc::proc_taskinfo>() as libc::c_int;
    // SAFETY: `info` is a `proc_taskinfo` of `size` bytes for the kernel to fill.
    let got =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTASKINFO, 0, info.as_mut_ptr().cast(), size) };
    if got != size {
        return None;
    }
    // SAFETY: the kernel filled all `size` bytes; a zeroed `proc_taskinfo` is valid anyway.
    let info = unsafe { info.assume_init() };
    /// `struct mach_timebase_info`: a Mach time unit is `numer / denom` nanoseconds.
    #[repr(C)]
    struct Timebase {
        numer: u32,
        denom: u32,
    }
    unsafe extern "C" {
        fn mach_timebase_info(info: *mut Timebase) -> libc::c_int;
    }
    let mut base = Timebase { numer: 0, denom: 0 };
    // SAFETY: a struct for the kernel to fill.
    let got = unsafe { mach_timebase_info(&mut base) };
    if got != 0 || base.denom == 0 {
        return None;
    }
    let units = u128::from(info.pti_total_user + info.pti_total_system);
    Some((units * u128::from(base.numer) / u128::from(base.denom)) as u64)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn child_cpu_ns(_pid: libc::pid_t) -> Option<u64> {
    None
}
