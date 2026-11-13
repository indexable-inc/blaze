//! The task watchdog's driver in the daemon: one thread, `blaze-task-watch`, that looks at every
//! task each [`LOOK_EVERY`] (blaze-profile's [`Reader`], the IO half, into its sans-IO core
//! [`Watch`]) and ends each task the core finds stuck. No thread per task: one look reads every
//! running job's ring, every wait in the lock module's graph, the scheduler's parked joins and
//! the children threads wait for in place, and allocates nothing once its tables have grown.
//!
//! For each task found stuck, once until it moves:
//! - its line (`stuck 4 min: codegen of seq-macro 0.3.6 waits on lock store.packs, held by
//!   thread blaze-worker-3 (tid 81) inside ...`) is logged and printed on the daemon's stderr,
//!   and one flight recording is written for the look;
//! - the loop is told ([`Pulse::say_stuck`]), which puts it in its build's live model
//!   (`Now::stuck`), so the client prints it and `blaze status` shows it first;
//! - the task is ended by what it is ([`Fate`]): a parked scheduler job is woken failing with
//!   a typed error at its wait, the safe cancel point (`blaze_graph::sched::fail_stuck`); a
//!   child process using no CPU is killed, so the thread that waits for it wakes and its job
//!   fails; a thread stuck in a lock or a signal, which nothing can unwind, is a lost worker:
//!   the loop fails its plane node with the line (`Plane::abandon`) and drops its task's late
//!   end, if one ever comes.
//!
//! Lost workers are counted, and past [`lost_bound`] (a quarter of the workers, at least two)
//! the daemon ends with [`Exit::WorkersLost`], having written what each thread was inside. That
//! is right rather than serving on: a lost thread still holds what it held (its lock, its
//! memory charge, its pool slot), so builds after it convoy behind the same stuck holder, and
//! with a quarter of the pool gone the daemon is slower than a fresh one by more than a restart
//! costs; the next command starts a whole daemon.

#![forbid(unsafe_code)]

use std::{alloc::Global, sync::Arc, thread::JoinHandle};

use blaze_core::{
    exit::Exit,
    lock::{Lock, Signal},
};
use blaze_profile::{
    flight,
    stuck::{LOOK_EVERY, Stuck, Task, Watch, Why},
    tasks::{Reader, Told},
};
use blaze_protocol::now;

use super::{
    flightrec,
    pulse::{Pulse, StuckSaid},
    signals,
};

/// What `blaze serve`'s log says before a stuck task's line: `blaze serve: pid 7 stuck: ...`.
pub const SAID: &str = " task stuck: ";

/// The most threads the daemon loses to stuck waits before it ends ([`Exit::WorkersLost`]): a
/// quarter of its workers, at least two. One lost thread is a bug said and survived; a quarter
/// of the pool gone is a daemon slower than a fresh one, still holding what they hold.
#[must_use]
pub fn lost_bound(workers: usize) -> usize {
    (workers / 4).max(2)
}

/// What the watchdog does with a task it found stuck.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fate {
    /// A parked scheduler job: woken failing at its wait, `observed` as the scheduler numbers it.
    FailJob { observed: u64 },
    /// A child process: killed, so its waiter's job fails.
    Kill { pid: u32 },
    /// A thread nothing can unwind: its build's task fails at the plane, and it is lost.
    Lost { tid: u64 },
    /// A task behind another the watchdog ends this look (a job or a child): it moves once that
    /// one fails, so nothing is done to it.
    Behind,
    /// The loop: its own guard ends the daemon (`serve::stop`); never a task of this reader.
    Loop,
}

/// What to do with `task`, stuck for `why`. A thread behind a job or a child that is failed
/// this look is left to move once that ends; one behind a thread, or in or behind a cycle, can
/// only be lost.
#[must_use]
pub fn fate(task: Task, why: Why) -> Fate {
    match task {
        Task::Job(observed) => Fate::FailJob { observed },
        Task::Child(pid) => Fate::Kill { pid },
        Task::Loop => Fate::Loop,
        Task::Thread(tid) => match why {
            Why::Behind { end: Task::Job(_) | Task::Child(_) } => Fate::Behind,
            _ => Fate::Lost { tid },
        },
    }
}

/// The task a cycle is broken at: its least job, else its least child, else none (a cycle of
/// threads only, which is lost whole).
#[must_use]
pub fn breaks_at(stuck: &Stuck) -> Option<Task> {
    let mut members = stuck.cycle.iter().map(|edge| edge.task);
    let jobs = members.clone().filter(|it| matches!(it, Task::Job(_))).min();
    jobs.or_else(|| members.find(|it| matches!(it, Task::Child(_))))
}

/// The watchdog's counts of threads lost for good: each once, by tid.
#[derive(Debug)]
pub struct Lost {
    bound: usize,
    tids: Vec<u64, Global>,
}

impl Lost {
    #[must_use]
    pub fn new(workers: usize) -> Lost {
        Lost { bound: lost_bound(workers), tids: Vec::new_in(Global) }
    }

    /// `tid` is lost: whether that takes the count past the bound.
    pub fn lose(&mut self, tid: u64) -> bool {
        if !self.tids.contains(&tid) {
            self.tids.push(tid);
        }
        self.tids.len() > self.bound
    }

    #[must_use]
    pub fn count(&self) -> usize {
        self.tids.len()
    }

    #[must_use]
    pub fn bound(&self) -> usize {
        self.bound
    }
}

/// The watchdog's thread ([`arm`]).
pub struct TaskWatch {
    over: Arc<Over>,
    thread: JoinHandle<()>,
}

/// The word that ends the watchdog, and the signal its look waits on.
struct Over {
    done: Lock<bool>,
    told: Signal,
}

impl TaskWatch {
    /// The daemon stops: the watchdog ends at once.
    pub fn disarm(self) {
        let TaskWatch { over, thread } = self;
        *over.done.lock() = true;
        over.told.notify_all();
        // A watchdog that panicked has nothing more to say than its panic, which it printed.
        _ = thread.join();
    }
}

/// Starts the task watchdog of a daemon with `workers` pool workers, telling the loop through
/// `pulse`.
pub fn arm(pulse: Arc<Pulse>, workers: usize) -> anyhow::Result<TaskWatch> {
    use anyhow::Context as _;
    let over = Arc::new(Over {
        done: Lock::new("serve.stuck.done", false),
        told: Signal::new("serve.stuck.next_look"),
    });
    let theirs = Arc::clone(&over);
    let thread = std::thread::Builder::new()
        .name("blaze-task-watch".to_owned()) // alloc: once a daemon
        .spawn(move || watch(&pulse, workers, &theirs))
        .context("starting the task watchdog")?;
    Ok(TaskWatch { over, thread })
}

fn now_ns() -> u64 {
    u64::try_from(blaze_profile::clock::mono_ns()).unwrap_or(0)
}

/// The watchdog's loop: a look each [`LOOK_EVERY`] until [`TaskWatch::disarm`]. Bounded: each
/// look reads what is alive now, and the wait between looks has its deadline.
fn watch(pulse: &Pulse, workers: usize, over: &Over) {
    let mut reader = Reader::default();
    let mut core = Watch::new();
    let mut lost = Lost::new(workers);
    let pid = std::process::id();
    loop {
        {
            let mut done = over.done.lock();
            if *done {
                return;
            }
            let _wait = tracing::info_span!(
                target: "blaze",
                "wait for the next look at every task",
                awaits = "the task watchdog's next look, or the daemon's stop"
            )
            .entered();
            _ = over.told.wait_while_for(&mut done, |done| !*done, LOOK_EVERY);
            if *done {
                return;
            }
        }
        let _look = tracing::info_span!(target: "blaze", "look at every task").entered();
        reader.read();
        let stuck = core.look(now_ns(), &reader.seen);
        if stuck.is_empty() {
            continue;
        }
        let threads = flight::threads_now();
        // alloc: a look that found a task stuck, once per stuck task
        let mut lines = String::new();
        let mut over_bound = false;
        for it in &stuck {
            let told = reader.told(it, &threads);
            let (done, lost_now) = end(&reader, it, &told, &mut lost, pulse);
            over_bound |= lost_now;
            let said = protocol(told);
            let line = format!("{said}; {done}"); // alloc: as above
            tracing::error!(target: "blaze", build = it.build, "{line}");
            // logged: the event above
            eprintln!("blaze serve: pid {pid}{SAID}{line}");
            lines.push_str(&line);
            lines.push('\n');
        }
        _ = flightrec::record(flightrec::Why::Stuck, &lines, flightrec::WINDOW);
        if over_bound {
            // alloc: once, as the process ends
            let said = format!(
                "blaze serve: pid {pid}{SAID}{} threads are stuck where nothing can unwind them, past the bound of {} (a quarter of the {workers} workers, at least two): each failed its build, but they hold what they held, so the daemon ends with exit status {}",
                lost.count(),
                lost.bound(),
                Exit::WorkersLost
            );
            tracing::error!(target: "blaze", "{said}");
            // logged: the event above
            eprintln!("{said}");
            _ = flightrec::record(flightrec::Why::Stuck, &said, flightrec::WINDOW);
            signals::exit_now(Exit::WorkersLost);
        }
    }
}

/// Ends `stuck` by its [`Fate`], telling the loop: what was done, in words, and whether a lost
/// thread took the count past its bound.
fn end(
    reader: &Reader,
    stuck: &Stuck,
    told: &Told,
    lost: &mut Lost,
    pulse: &Pulse,
) -> (String, bool) {
    let said = protocol(told.clone()); // clone: a stuck task, once
    let tell = |task: Task, lost: bool| {
        let about = reader.about(task);
        let build = reader.seen.iter().find(|it| it.task == task).map_or(stuck.build, |it| it.build);
        pulse.say_stuck(StuckSaid { build, node: about.node, lost, said: said.clone() }); // clone: as above
    };
    // A cycle is broken at a job or a child where it has one; a cycle of threads is lost whole.
    if matches!(stuck.why, Why::Cycle) {
        if let Some(at) = breaks_at(stuck) {
            tell(stuck.task, false);
            return (ended(fate(at, Why::Cycle), stuck.still_ns), false);
        }
        let mut past = false;
        for edge in &stuck.cycle {
            if let Task::Thread(tid) = edge.task {
                past |= lost.lose(tid);
                tell(edge.task, true);
            }
        }
        // alloc: once per stuck task
        let done = format!(
            "nothing can unwind a cycle of threads: each one's build task fails and the {} threads are lost ({} of the bound {})",
            stuck.cycle.len(),
            lost.count(),
            lost.bound()
        );
        return (done, past);
    }
    let fate = fate(stuck.task, stuck.why);
    match fate {
        Fate::Lost { tid } => {
            let past = lost.lose(tid);
            tell(stuck.task, true);
            // alloc: once per stuck task
            let done = format!(
                "nothing can unwind it: its build task fails and the thread is lost ({} of the bound {})",
                lost.count(),
                lost.bound()
            );
            (done, past)
        }
        _ => {
            tell(stuck.task, false);
            (ended(fate, stuck.still_ns), false)
        }
    }
}

/// Ends a job or a child by `fate`, still for `still_ns`: what was done, in words.
fn ended(fate: Fate, still_ns: u64) -> String {
    let done = match fate {
        Fate::FailJob { observed } => {
            if blaze_graph::sched::fail_stuck(observed, still_ns) {
                "its job is woken failing at its wait with a typed error"
            } else {
                "its job ended before it could be failed"
            }
        }
        Fate::Kill { pid } => match i32::try_from(pid) {
            Ok(pid) if blaze_core::children::kill_listed(pid) => {
                "it is killed, so the job waiting for it fails"
            }
            _ => "it was reaped before it could be killed",
        },
        Fate::Behind => "it moves once what it waits behind is failed",
        Fate::Loop => "the loop's own guard ends the daemon",
        Fate::Lost { .. } => "it is lost",
    };
    done.to_owned() // alloc: once per stuck task
}

/// `told` as the protocol says it to clients.
fn protocol(told: Told) -> now::Stuck {
    now::Stuck {
        what: told.what,
        cycle: told.cycle,
        waits: told.waits,
        for_ms: told.still_ns / 1_000_000,
        inside: told.inside,
        held_by: told.held_by,
    }
}

#[cfg(test)]
mod tests;
