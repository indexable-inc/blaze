//! The suite's zygote and the parent side of its children (docs/v2/plan/design/test.md, "End
//! state", "One process per test"; piece 9). Per suite one zygote: the launcher with the
//! suite's code placed and its statics initialised, ONE thread, before any test runs. For each
//! test the plan asks it forks a child (child.rs); the child runs one row and reports once on
//! descriptor 3 (wire.rs); [`Pool`] is the zygote's side of all its children at once: at most
//! `width` running, each one's report and output read, its deadline kept, its end reaped, and
//! each result handed on in the order the tests END, with the test's row.
//!
//! A panic, a crash or a hang fails ONE test, typed: a signal is [`How::Signal`]; past its
//! deadline the child is asked for its stacks ([`Effect::Stacks`]), given [`GRACE`] to write
//! them, then its group killed ([`Effect::Kill`]) and the result is [`How::TimedOut`] whatever
//! it reported (a report that came after the deadline is not its result); an exit with no whole
//! report (none came, it was cut, its digest did not match, it named another row, a second one
//! came) is [`How::NoReport`]. Nothing is retried (rule 3).
//!
//! **Why fork is sound here.** POSIX allows only async-signal-safe calls in the child of a
//! multi-threaded process (a lock another thread held at the fork stays held for ever). So the
//! zygote forks only while it is ONE thread, and proves it each time: the driver counts the
//! process's threads (macOS `task_threads` on `mach_task_self()`, Linux the `Threads:` count of
//! `/proc/self/status`) right before each fork and hands the count to [`Gate::check`], whose
//! [`OneThread`] proof the driver's fork takes by value (rule 74: a proof type, not a comment).
//! The zygote runs no test code itself and starts no thread: the launcher, in zygote mode,
//! starts its control thread only in each child ([`crate::child::Step::Control`]), and threads
//! the suite's static initialisers started are joined before the first fork (else the count
//! refuses it, naming the count). Counting costs microseconds against a fork's 0.3 ms (Linux)
//! to 1 ms (macOS), so it is done before every fork, not once.
//!
//! **macOS arm64, what a child must not touch** (each to verify by a test unless said):
//! - CoreFoundation refuses use in a forked child (its
//!   `__THE_PROCESS_HAS_FORKED_AND_YOU_CANNOT_USE_THIS_COREFOUNDATION_FUNCTIONALITY___YOU_MUST_EXEC__`
//!   trap) once the parent used it;
//! - libdispatch marks a child of a process that used it as unsafe and traps on use, and a
//!   process that used it has worker threads, which the count refuses anyway;
//! - Mach ports, so every XPC connection and every service looked up by the parent, are not
//!   inherited: a child sees dead names (Security, the keychain, `getpwuid` through
//!   opendirectoryd, the notify and logging daemons);
//! - the Objective-C runtime aborts a child whose fork came while a class's `+initialize` ran
//!   on another thread (one thread rules it out).
//! The supported test program has only libSystem loaded (what `blaze run` places: no system
//! framework); libSystem's own atfork handlers (malloc's among them) make the allocator usable
//! in the child. A test program that links a framework is refused by the driver by name (it
//! checks the loaded images before the first fork: `_dyld_image_count`).
//! **Linux**: fork of a one-thread process is unrestricted; the code area's `MAP_SHARED` run
//! view is the parent's pages (sys.rs, "Fork"), so the zygote must not decommit a body while a
//! child runs it; the zygote writes no code at all.
//!
//! **Stacks at the deadline.** No cheap way exists to read another process's stacks on macOS
//! (`task_for_pid` needs an entitlement or root) or without ptrace on Linux. So the child reads
//! its own: `SIGQUIT` (the JVM's convention for a thread dump) runs a handler installed by
//! [`crate::child::Step::Signals`] that walks the interrupted thread's frame-pointer chain
//! (AArch64 macOS always keeps frame pointers; the builder keeps them on Linux too), bounded by
//! [`crate::wire::FRAMES_CAP`] frames and the thread's stack bounds, and writes one `Stacks`
//! message with no allocation and no lock. A child that does not answer within [`GRACE`] is
//! killed all the same. Only the interrupted thread's stack: a test that started threads gives
//! one stack of several.
//!
//! The machine reads no clock, makes no call and starts nothing: the driver forks, reads,
//! reaps and kills as the [`Effect`]s say, and gives each event with its time.

#![blaze::sans_io]

use core::fmt;

use blaze_own::{AString, AVec, Alloc, String, Vec, VecDeque};

use crate::plan::{Exit, Failure, NotKept, Ran};
use crate::postings::{FnName, ReadName};
use crate::result::{Captured, Ended, OUTPUT_CAP, Place, Refusal, Why};
use crate::wire::{self, Frame, Message, Outcome, STREAM_CAP};
use crate::{Nanos, Plant, TestId};

/// How long a child past its deadline has to write its stacks before its group is killed; also
/// how long a child that ended is waited for when something it started still holds its pipes.
pub const GRACE: Nanos = Nanos(200_000_000);

/// How many children run at once: `--test-threads`/`threads = n` when asked, else the machine's
/// performance cores (test.md, "Width"). The one rule; the plan's width is this number too.
pub fn width(performance_cores: u32, asked: Option<u32>) -> Result<u32, PoolError> {
    match asked {
        Some(0) => Err(PoolError::NoWidth),
        Some(n) => Ok(n),
        None => Ok(performance_cores.max(1)),
    }
}

/// The proof that the zygote is one thread now: made only by [`Gate::check`], taken by value by
/// the driver's fork.
#[derive(Debug)]
pub struct OneThread {
    _sealed: (),
}

/// The counted assertion before every fork.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Gate {
    forks: u64,
}

/// A fork refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ForkError {
    /// The zygote has this many threads; a fork now could leave a lock held for ever in the
    /// child. `forks` children were made before.
    Threads { count: u32, forks: u64 },
}

impl fmt::Display for ForkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            ForkError::Threads { count, forks } => write!(
                f,
                "the test zygote has {count} threads after {forks} forks; it forks only as one thread \
                 (a suite's static initialiser started a thread that was not joined)"
            ),
        }
    }
}

impl core::error::Error for ForkError {}

impl Gate {
    pub const fn new() -> Gate {
        Gate { forks: 0 }
    }

    /// `threads` is the process's thread count, read by the driver immediately before the
    /// fork; the proof only when it is one.
    pub fn check(&mut self, threads: u32) -> Result<OneThread, ForkError> {
        if threads != 1 {
            return Err(ForkError::Threads { count: threads, forks: self.forks });
        }
        self.forks += 1;
        Ok(OneThread { _sealed: () })
    }

    pub const fn forks(&self) -> u64 {
        self.forks
    }
}

/// A child, by its process id (the driver's; the sim's are its own numbers).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ChildId(pub u32);

/// How a reaped child ended, as `waitpid` says.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Exited { code: i32 },
    Signaled { number: i32 },
}

/// A child's pipe.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stream {
    /// Descriptor 3: the report.
    Report,
    /// Descriptors 1 and 2: the test's output.
    Output,
}

/// What the driver is to do, in order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Effect {
    /// Check the gate, fork, set the child's process group, run `child::Child`'s steps in it,
    /// and call [`Pool::forked`].
    Fork { row: u32 },
    /// Past its deadline: send `SIGQUIT` to the child (its handler writes its stacks).
    Stacks { child: ChildId },
    /// Kill the child's process group (`SIGKILL`).
    Kill { child: ChildId },
}

/// What the report said, when one counts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Said {
    Returned,
    Panicked { at: Option<Place> },
    Refused { refusal: Refusal },
}

/// How a run ended, as the zygote decides it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum How {
    /// A whole report, then exit code 0, before the deadline.
    Reported(Said),
    Signal { number: i32 },
    /// Past its deadline: killed this long after its start.
    TimedOut { after: Nanos },
    /// Exited by itself with this code and no whole report; `torn` bytes came on descriptor 3.
    NoReport { code: i32, torn: u64 },
}

/// One test's run, ended: what the loop hands to the plan ([`Finished::ran`]), to the result
/// record ([`Finished::ended`], [`Finished::captured`]) and to the postings (the asks).
pub struct Finished<'a> {
    pub row: u32,
    pub test: TestId,
    pub micros: u64,
    pub how: How,
    /// What the wrappers saw (only from a counted report; else kept, and irrelevant: a failure
    /// is never kept).
    pub keep: Result<(), NotKept>,
    /// The functions the run entered, sorted, each once.
    pub entered: AVec<'a, FnName>,
    /// Its outside reads, sorted, each once.
    pub reads: AVec<'a, ReadName>,
    /// A hung run's return addresses, innermost first (empty unless it answered `SIGQUIT`).
    pub stacks: AVec<'a, u64>,
    text: AString<'a>,
    tail: AVec<'a, u8>,
    total: u64,
    digest: [u8; 32],
}

/// The plan's failure does not fit how the run ended: the loop paired the wrong values.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Mismatch {
    pub row: u32,
    pub why: Failure,
    pub how: How,
}

impl fmt::Display for Mismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ended = match self.how {
            How::Reported(Said::Returned) => "it returned",
            How::Reported(Said::Panicked { .. }) => "it panicked",
            How::Reported(Said::Refused { .. }) => "the sandbox refused it",
            How::Signal { .. } => "a signal ended it",
            How::TimedOut { .. } => "it was killed past its deadline",
            How::NoReport { .. } => "it exited with no whole report",
        };
        write!(f, "test row {}: the plan's verdict \"{}\" does not fit how the run ended: {ended}", self.row, Why(self.why))
    }
}

impl core::error::Error for Mismatch {}

impl<'a> Finished<'a> {
    /// The panic message or what was refused; empty otherwise.
    pub fn text(&self) -> &str {
        self.text.as_str()
    }

    /// How the run ended, as the plan reads it.
    pub fn exit(&self) -> Exit<'_> {
        match self.how {
            How::Reported(Said::Returned) => Exit::Returned,
            How::Reported(Said::Panicked { .. }) => Exit::Panicked { text: self.text.as_str() },
            How::Reported(Said::Refused { .. }) => Exit::Refused,
            How::Signal { number } => Exit::Signal { number },
            How::TimedOut { .. } => Exit::Killed,
            How::NoReport { code, .. } => Exit::NoReport { code },
        }
    }

    /// The plan's input for [`crate::plan::Plan::ended`].
    pub fn ran(&self) -> Ran<'_> {
        Ran { exit: self.exit(), keep: self.keep, micros: self.micros }
    }

    /// The result record's ending for the plan's verdict `why` on this run.
    pub fn ended(&self, why: Failure) -> Result<Ended<'_>, Mismatch> {
        let message = self.text.as_str();
        let ended = match (why, self.how) {
            (Failure::Panicked, How::Reported(Said::Panicked { at })) => Ended::Panicked { message, at },
            (Failure::PanicTextMissing, How::Reported(Said::Panicked { at })) => Ended::PanicTextMissing { message, at },
            (Failure::DidNotPanic, How::Reported(Said::Returned)) => Ended::DidNotPanic,
            (Failure::Refused, How::Reported(Said::Refused { refusal })) => Ended::Refused { refusal, what: message },
            (Failure::Signal { number }, How::Signal { .. }) => Ended::Signal { number },
            // The plan's own measure of the overrun: it decided the kill.
            (Failure::TimedOut { after }, How::TimedOut { .. }) => Ended::TimedOut { after },
            (Failure::NoReport { code }, How::NoReport { torn, .. }) => Ended::NoReport { code, torn },
            _ => return Err(Mismatch { row: self.row, why, how: self.how }),
        };
        Ok(ended)
    }

    /// The run's output: all of it digested and counted, its last [`OUTPUT_CAP`] bytes kept.
    pub fn captured(&self) -> Captured<'_> {
        Captured { digest: self.digest, total: self.total, tail: self.tail.as_slice() }
    }
}

/// A call the pool refuses: the driver broke the protocol. Each names the row or the child.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PoolError {
    /// Width zero: nothing could ever run.
    NoWidth,
    /// A row asked while it is queued or running.
    TwoAsked { row: u32 },
    /// `forked` for a row no fork was asked for.
    NotForking { row: u32 },
    /// An event for a child the pool does not hold.
    UnknownChild { child: ChildId },
    /// A second end for one child.
    ExitedTwice { child: ChildId },
    /// A second close of one stream, or bytes after its close.
    Closed { child: ChildId, stream: Stream },
}

impl fmt::Display for PoolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            PoolError::NoWidth => f.write_str("a test pool of width 0 could never start a test"),
            PoolError::TwoAsked { row } => write!(f, "test row {row} asked while it is queued or running"),
            PoolError::NotForking { row } => write!(f, "test row {row} forked, but no fork was asked for it"),
            PoolError::UnknownChild { child } => write!(f, "an event for child {}, which the test pool does not hold", child.0),
            PoolError::ExitedTwice { child } => write!(f, "child {} reaped twice", child.0),
            PoolError::Closed { child, stream } => {
                let name = match stream {
                    Stream::Report => "report (descriptor 3)",
                    Stream::Output => "output",
                };
                write!(f, "child {}'s {name} stream used after its close", child.0)
            }
        }
    }
}

impl core::error::Error for PoolError {}

/// A run's output as it comes: digested and counted whole, its tail kept in at most twice
/// [`OUTPUT_CAP`] bytes.
struct Capture<'a> {
    tail: AVec<'a, u8>,
    total: u64,
    hasher: blaze_blake3::Hasher,
}

impl<'a> Capture<'a> {
    fn new(alloc: Alloc<'a>) -> Capture<'a> {
        Capture { tail: Vec::new_in(alloc), total: 0, hasher: blaze_blake3::Hasher::new() }
    }

    fn take(&mut self, bytes: &[u8]) {
        self.hasher.update(bytes);
        self.total += bytes.len() as u64;
        if bytes.len() >= OUTPUT_CAP {
            self.tail.clear();
            self.tail.extend_from_slice(&bytes[bytes.len() - OUTPUT_CAP..]);
            return;
        }
        self.tail.extend_from_slice(bytes);
        if self.tail.len() > 2 * OUTPUT_CAP {
            self.cut();
        }
    }

    /// The tail cut to its last `min(total, OUTPUT_CAP)` bytes.
    fn cut(&mut self) {
        let len = self.tail.len();
        if len > OUTPUT_CAP {
            self.tail.copy_within(len - OUTPUT_CAP.., 0);
            self.tail.truncate(OUTPUT_CAP);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Forking,
    Running,
    /// Past its deadline, its stacks asked for, killed at `until`.
    Dumping { until: Nanos },
    /// Its group killed at `at`.
    Killed { at: Nanos },
}

struct Live<'a> {
    row: u32,
    test: TestId,
    budget: Nanos,
    child: ChildId,
    started: Nanos,
    deadline: Nanos,
    state: State,
    report: AVec<'a, u8>,
    /// More than [`STREAM_CAP`] came on descriptor 3: nothing of it counts.
    report_over: bool,
    report_open: bool,
    output_open: bool,
    capture: Capture<'a>,
    exited: Option<Status>,
    /// It exited while something it started still held a pipe: its group was killed then, and
    /// the run is decided at this time with what came.
    leftover: Option<Nanos>,
    /// Its result is handed on; it stays until it is reaped.
    decided: bool,
}

struct Queued {
    row: u32,
    test: TestId,
    budget: Nanos,
}

/// The zygote's children: at most `width` running at once.
pub struct Pool<'a> {
    width: u32,
    queue: VecDeque<Queued, Alloc<'a>>,
    live: AVec<'a, Live<'a>>,
    done: VecDeque<Finished<'a>, Alloc<'a>>,
    alloc: Alloc<'a>,
    pub(crate) plant: Plant,
}

impl<'a> Pool<'a> {
    pub fn new(width: u32, alloc: Alloc<'a>) -> Result<Pool<'a>, PoolError> {
        if width == 0 {
            return Err(PoolError::NoWidth);
        }
        Ok(Pool {
            width,
            queue: VecDeque::new_in(alloc),
            live: Vec::new_in(alloc),
            done: VecDeque::new_in(alloc),
            alloc,
            plant: Plant::None,
        })
    }

    /// Queue test row `row`, to be killed `budget` after its fork.
    pub fn ask(&mut self, row: u32, test: TestId, budget: Nanos) -> Result<(), PoolError> {
        let queued = self.queue.iter().any(|queued| queued.row == row);
        let live = self.live.iter().any(|live| live.row == row && !live.decided);
        if queued || live {
            return Err(PoolError::TwoAsked { row });
        }
        self.queue.push_back(Queued { row, test, budget });
        Ok(())
    }

    /// Children running or being forked: what the width bounds.
    pub fn running(&self) -> usize {
        self.live.iter().filter(|live| !live.decided).count()
    }

    /// Starts what fits.
    pub fn pump(&mut self, out: &mut AVec<'_, Effect>) {
        while self.running() < self.width as usize {
            let Some(next) = self.queue.pop_front() else { return };
            self.live.push(Live {
                row: next.row,
                test: next.test,
                budget: next.budget,
                child: ChildId(0),
                started: Nanos(0),
                deadline: Nanos(0),
                state: State::Forking,
                report: Vec::new_in(self.alloc),
                report_over: false,
                report_open: true,
                output_open: true,
                capture: Capture::new(self.alloc),
                exited: None,
                leftover: None,
                decided: false,
            });
            out.push(Effect::Fork { row: next.row });
        }
    }

    /// The fork of `row` made `child` at `now`; its deadline starts.
    pub fn forked(&mut self, row: u32, child: ChildId, now: Nanos) -> Result<(), PoolError> {
        let Some(live) = self.live.iter_mut().find(|live| live.row == row && live.state == State::Forking) else {
            return Err(PoolError::NotForking { row });
        };
        live.child = child;
        live.started = now;
        live.deadline = now.after(live.budget);
        live.state = State::Running;
        Ok(())
    }

    fn find(&self, child: ChildId) -> Result<usize, PoolError> {
        self.live
            .iter()
            .position(|live| live.child == child && live.state != State::Forking)
            .ok_or(PoolError::UnknownChild { child })
    }

    /// Bytes read from one of `child`'s pipes.
    pub fn bytes(
        &mut self,
        child: ChildId,
        stream: Stream,
        bytes: &[u8],
        now: Nanos,
        out: &mut AVec<'_, Effect>,
    ) -> Result<(), PoolError> {
        let at = self.find(child)?;
        let live = &mut self.live[at];
        match stream {
            Stream::Report if !live.report_open => return Err(PoolError::Closed { child, stream }),
            Stream::Output if !live.output_open => return Err(PoolError::Closed { child, stream }),
            Stream::Output => {
                live.capture.take(bytes);
                return Ok(());
            }
            Stream::Report => {}
        }
        if live.report_over || live.report.len() + bytes.len() > STREAM_CAP {
            live.report_over = true;
            live.report.clear();
            return Ok(());
        }
        live.report.extend_from_slice(bytes);
        // Asked for its stacks and they came whole: kill now rather than at the grace's end.
        if matches!(live.state, State::Dumping { .. }) && holds_stacks(live.report.as_slice()) {
            live.state = State::Killed { at: now };
            out.push(Effect::Kill { child });
        }
        Ok(())
    }

    /// End of one of `child`'s pipes.
    pub fn closed(&mut self, child: ChildId, stream: Stream, now: Nanos) -> Result<(), PoolError> {
        let at = self.find(child)?;
        let live = &mut self.live[at];
        let open = match stream {
            Stream::Report => &mut live.report_open,
            Stream::Output => &mut live.output_open,
        };
        if !*open {
            return Err(PoolError::Closed { child, stream });
        }
        *open = false;
        self.settle_if_whole(at, now);
        Ok(())
    }

    /// `child` was reaped with `status`.
    pub fn exited(&mut self, child: ChildId, status: Status, now: Nanos, out: &mut AVec<'_, Effect>) -> Result<(), PoolError> {
        let at = self.find(child)?;
        let live = &mut self.live[at];
        if live.exited.is_some() {
            return Err(PoolError::ExitedTwice { child });
        }
        live.exited = Some(status);
        if live.decided {
            // Decided at a kill that did not end it in time: reaped now, gone.
            self.live.remove_swap(at);
            return Ok(());
        }
        if (live.report_open || live.output_open) && live.leftover.is_none() {
            // Something it started holds a pipe: kill the group; decide by the grace's end.
            live.leftover = Some(now.after(GRACE));
            out.push(Effect::Kill { child });
        }
        self.settle_if_whole(at, now);
        Ok(())
    }

    /// Deadlines: asks a late child for its stacks, kills it after the grace, and decides a
    /// run whose pipes outlived it.
    pub fn tick(&mut self, now: Nanos, out: &mut AVec<'_, Effect>) {
        let mut at = 0;
        while at < self.live.len() {
            let live = &mut self.live[at];
            if live.decided {
                at += 1;
                continue;
            }
            match live.state {
                State::Running if now >= live.deadline && live.exited.is_none() => {
                    live.state = State::Dumping { until: now.after(GRACE) };
                    out.push(Effect::Stacks { child: live.child });
                }
                State::Dumping { until } if now >= until => {
                    live.state = State::Killed { at: now };
                    out.push(Effect::Kill { child: live.child });
                }
                _ => {}
            }
            let live = &self.live[at];
            let leftover_due = live.leftover.is_some_and(|due| now >= due);
            let kill_due = matches!(live.state, State::Killed { at: killed } if now >= killed.after(GRACE));
            if leftover_due || kill_due {
                self.settle(at, now);
            }
            if self.live[at].decided && self.live[at].exited.is_some() {
                self.live.remove_swap(at);
                continue;
            }
            at += 1;
        }
    }

    /// When [`Pool::tick`] next has work: the earliest deadline, grace end or leftover end.
    pub fn next_wake(&self) -> Option<Nanos> {
        self.live
            .iter()
            .filter(|live| !live.decided)
            .filter_map(|live| {
                let state = match live.state {
                    State::Forking => None,
                    State::Running if live.exited.is_none() => Some(live.deadline),
                    State::Running => None,
                    State::Dumping { until } => Some(until),
                    State::Killed { at } => Some(at.after(GRACE)),
                };
                match (state, live.leftover) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                }
            })
            .min()
    }

    /// The next result, in the order the runs ended.
    pub fn next_finished(&mut self) -> Option<Finished<'a>> {
        self.done.pop_front()
    }

    /// Nothing queued, running or left to reap.
    pub fn idle(&self) -> bool {
        self.queue.is_empty() && self.live.is_empty() && self.done.is_empty()
    }

    fn settle_if_whole(&mut self, at: usize, now: Nanos) {
        let live = &self.live[at];
        if live.decided || live.exited.is_none() || live.report_open || live.output_open {
            return;
        }
        self.settle(at, now);
        self.live.remove_swap(at);
    }

    /// Decides the run at `at` and hands its result on.
    fn settle(&mut self, at: usize, now: Nanos) {
        let alloc = self.alloc;
        let check_digest = self.plant != Plant::TrustTornReport;
        let killed_stands = self.plant == Plant::KilledReportStands;
        let live = &mut self.live[at];
        live.decided = true;
        let report = core::mem::replace(&mut live.report, Vec::new_in(alloc));
        let timed_out = match live.state {
            State::Dumping { .. } => Some(now),
            State::Killed { at } => Some(at),
            State::Forking | State::Running => None,
        };
        let read = read_stream(report.as_slice(), live.row, check_digest, timed_out.is_some());
        let read = match live.report_over {
            true => Read::Bad,
            false => read,
        };

        let mut finished = Finished {
            row: live.row,
            test: live.test,
            micros: now.0.saturating_sub(live.started.0) / 1_000,
            how: How::NoReport { code: 0, torn: report.len() as u64 },
            keep: Ok(()),
            entered: Vec::new_in(alloc),
            reads: Vec::new_in(alloc),
            stacks: Vec::new_in(alloc),
            text: String::new_in(alloc),
            tail: Vec::new_in(alloc),
            total: 0,
            digest: [0; 32],
        };
        if let Read::Good { stacks: Some(stacks), .. } = &read {
            for frame in stacks.frames() {
                finished.stacks.push(frame);
            }
        }
        let ended = match &read {
            Read::Good { ended, .. } => *ended,
            Read::Bad => None,
        };
        finished.how = match (timed_out, live.exited, ended) {
            // The planted bug: the report of a run killed past its deadline counted.
            (Some(_), _, Some(report)) if killed_stands => take(&mut finished, &report),
            (Some(killed), _, _) => How::TimedOut { after: Nanos(killed.0.saturating_sub(live.started.0)) },
            (None, Some(Status::Signaled { number }), _) => How::Signal { number },
            (None, Some(Status::Exited { code: 0 }), Some(report)) => take(&mut finished, &report),
            (None, Some(Status::Exited { code }), _) => How::NoReport { code, torn: report.len() as u64 },
            // Not reaped and not killed: only a leftover pipe's end decides that, after a reap.
            (None, None, _) => How::NoReport { code: -1, torn: report.len() as u64 },
        };
        live.capture.cut();
        finished.total = live.capture.total;
        finished.digest = live.capture.hasher.finalize();
        finished.tail.extend_from_slice(live.capture.tail.as_slice());
        self.done.push_back(finished);
    }
}

/// What a stream of reports came to.
enum Read<'r> {
    /// Every byte made whole messages: at most one `TestEnded` for this row and, when its
    /// stacks were asked, at most one `Stacks`.
    Good { ended: Option<wire::TestEnded<'r>>, stacks: Option<wire::Stacks<'r>> },
    /// A cut, a digest that did not match, a refused field, a second report, another row's.
    Bad,
}

fn read_stream(bytes: &[u8], row: u32, check_digest: bool, stacks_asked: bool) -> Read<'_> {
    let mut ended = None;
    let mut stacks = None;
    let mut from = 0;
    while from < bytes.len() {
        let len = match wire::frame(&bytes[from..]) {
            Ok(Frame::Whole { len }) => len,
            Ok(Frame::Need { .. }) | Err(_) => return Read::Bad,
        };
        match wire::open_with(&bytes[from..from + len], check_digest) {
            Ok(Message::Ended(report)) if report.row == row && ended.is_none() => ended = Some(report),
            Ok(Message::Stacks(frames)) if frames.row == row && stacks.is_none() && stacks_asked => stacks = Some(frames),
            Ok(_) | Err(_) => return Read::Bad,
        }
        from += len;
    }
    Read::Good { ended, stacks }
}

/// Holds whole messages ending in a `Stacks` (checked fully when the run is decided).
fn holds_stacks(bytes: &[u8]) -> bool {
    let mut from = 0;
    let mut saw = false;
    while from < bytes.len() {
        match wire::frame(&bytes[from..]) {
            Ok(Frame::Whole { len }) => {
                // The kind byte of a whole header (wire.rs: 1 is `Stacks`).
                saw |= bytes[from + 8] == 1;
                from += len;
            }
            Ok(Frame::Need { .. }) | Err(_) => return false,
        }
    }
    saw
}

/// A counted report copied out into the result.
fn take(finished: &mut Finished<'_>, report: &wire::TestEnded<'_>) -> How {
    finished.keep = report.keep;
    finished.entered.reserve(report.entered_len());
    for name in report.entered() {
        finished.entered.push(name);
    }
    finished.reads.reserve(report.reads_len());
    for name in report.reads() {
        finished.reads.push(name);
    }
    let said = match report.outcome {
        Outcome::Returned => Said::Returned,
        Outcome::Panicked { message, at } => {
            finished.text.push_str(message);
            Said::Panicked { at }
        }
        Outcome::Refused { refusal, what } => {
            finished.text.push_str(what);
            Said::Refused { refusal }
        }
    };
    How::Reported(said)
}
