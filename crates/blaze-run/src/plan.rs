//! [`Plan`]: one `blaze test` request's tests, from what is asked to each test's outcome
//! (docs/v2/plan/design/test.md, "End state"). It decides which tests are kept, which are looked
//! up at a new address and which run; it starts at most `width` at once, longest last run first;
//! it gives every run a deadline and kills a run past it; it judges each run's exit against the
//! test's `should_panic`; and it says which passes are filed.
//!
//! What a test's kept result covers is its ASKS: the functions its last run entered and the
//! outside reads it made. The driver hands the moved asks (functions whose code identity moved
//! since the suite's head, reads whose digest did) and the suite's [`Postings`]; a test none of
//! whose asks moved keeps its pass by that alone (the coarse proof), and only the candidates the
//! postings name are looked up: their last asks answered now give an address, and a pass there
//! (an undone edit) is kept, else the test runs. Nothing is scanned per function and no graph is
//! held. A failure and a run not kept always run again, saying why ([`Missed`]).
//!
//! The machine reads no clock (time is a [`Nanos`] in each call), starts no process and hashes
//! nothing: the driver computes addresses, looks them up, starts and kills processes and files
//! records, as the [`Effect`]s say.

#![blaze::sans_io]

use core::fmt;

use blaze_own::{AVec, Alloc, Vec};

use crate::postings::{Ask, Postings};
use crate::{Hex, Nanos, TestId};

/// Where a result is filed: H(test id, asks, answers), computed by the driver.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Address(pub [u8; 32]);

/// What a test must do beside returning (the tests record's `ShouldPanic`; becomes
/// `blaze_scope::tests_record::ShouldPanic` when this crate depends on blaze-scope).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ShouldPanic<'r> {
    No,
    Yes,
    /// Panics with a message that holds this text.
    WithText(&'r str),
}

/// One test asked, from its unit's tests record.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Asked<'r> {
    pub test: TestId,
    /// As printed: `module::name`. Filters read it.
    pub path: &'r str,
    /// `Some` when `#[ignore]`: its reason, empty when none is written.
    pub ignore: Option<&'r str>,
    pub should_panic: ShouldPanic<'r>,
    /// `#[bench]`.
    pub bench: bool,
}

/// Why a run's pass is not kept (test-selection.md, section 2), each by name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NotKept {
    /// It read the real clock.
    Clock,
    /// It reached a peer outside its own loopback.
    Network,
    /// It wrote outside its temporary folder.
    WroteOutside,
    /// It entered code the tracer cannot see (inline assembly, an unbound foreign call).
    Foreign,
    /// It ran outside the sandbox.
    Unsandboxed,
    /// It read through a descriptor the tracer did not see opened.
    Descriptor,
    /// It started a second thread and no reach set was given for it.
    Threads,
    /// Nothing traced it: the run gave no report of what it entered and read (blaze-cli
    /// test_drive.rs until the zygote's descriptor-3 report, test.md pieces 9 and 10).
    Untraced,
}

/// A test's row in the suite's head: its last outcome and how long its last run took.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Kept {
    pub test: TestId,
    pub last: Last,
    pub micros: u64,
}

/// A test's last outcome, as the head holds it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Last {
    /// A kept pass, filed at this address.
    Passed { address: Address },
    /// A kept pass at this address, one of whose asks moved in a request that did not ask the
    /// test (filtered, ignored, a bench): every row of a head is valid at the head's state, so
    /// this one is looked up before it is used again ([`Effect::Unsettle`]).
    Unsettled { address: Address },
    Failed,
    NotKept(NotKept),
}

/// Which tests a filter lets through.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Filter<'r> {
    All,
    /// The path holds this text (libtest's positional filter).
    Substring(&'r str),
    /// The path is this text (`--exact`).
    Exact(&'r str),
}

/// What happens to `#[ignore]` tests.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ignored {
    /// Reported as ignored, not run (the default).
    Skip,
    /// Only the ignored run (`--ignored`); the rest are filtered out.
    Only,
    /// Both run (`--include-ignored`).
    Include,
}

/// One request's settings.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Config<'r> {
    /// How many tests run at once: the machine's performance cores, 1 under `--nocapture`.
    pub width: u32,
    /// Every run's bound, from its start.
    pub deadline: Nanos,
    /// `--run-all`: every test asked runs; nothing kept is used.
    pub run_all: bool,
    pub ignored: Ignored,
    /// `--benches`: each `#[bench]` runs once as a test.
    pub benches: bool,
    pub filter: Filter<'r>,
    /// `--skip`: a path holding any of these is filtered out.
    pub skip: &'r [&'r str],
}

/// Why a test ran although kept results were asked for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Missed {
    RunAll,
    /// The head has no row for it.
    NoRecord,
    LastFailed,
    LastNotKept(NotKept),
    /// This ask moved and no pass was filed at the address its asks answer now.
    Moved { ask: Ask },
    /// Its row was unsettled by an earlier request and no pass was filed at the address its
    /// asks answer now.
    Unsettled,
}

/// What a kept pass rested on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Basis {
    /// None of its asks moved.
    Unchanged,
    /// Asks moved, and a pass was filed at the address they answer now (an undone edit).
    Answers,
}

/// How a run ended, as the driver saw it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Exit<'e> {
    /// The test's function returned.
    Returned,
    /// It panicked with this message (read from descriptor 3, never from stderr).
    Panicked { text: &'e str },
    /// The process ended by this signal (a crash, a stack overflow, an abort with no panic).
    Signal { number: i32 },
    /// The sandbox refused something it did, and the run was stopped.
    Refused,
    /// The driver killed it, as a [`Effect::Kill`] asked.
    Killed,
    /// The process exited by itself with this code and without a whole report on descriptor
    /// 3 (the test called `exit`, or its report was torn): what it did is unknown, so it failed.
    NoReport { code: i32 },
}

/// One run, ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Ran<'e> {
    pub exit: Exit<'e>,
    /// Whether its pass may be kept: what the launcher's wrappers and the sandbox saw.
    pub keep: Result<(), NotKept>,
    pub micros: u64,
}

/// Why a run failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Failure {
    Panicked,
    /// `should_panic`, and it returned.
    DidNotPanic,
    /// `should_panic(expected = ..)`, and the message does not hold the text.
    PanicTextMissing,
    Signal { number: i32 },
    Refused,
    /// Past its deadline: killed this long after its start.
    TimedOut { after: Nanos },
    /// It exited with this code and no whole report ([`Exit::NoReport`]).
    NoReport { code: i32 },
}

/// A test's outcome in this request.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// A kept pass stands; nothing ran.
    Cached(Basis),
    Passed { ran: Missed, kept: Result<(), NotKept> },
    Failed { ran: Missed, why: Failure },
    /// `#[ignore]`, not run.
    Ignored,
    /// `#[bench]`, not run.
    Bench,
}

/// What the driver is to do, in order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Effect {
    /// Print this outcome. Kept passes come at once; failures as they end.
    Report { test: TestId, outcome: Outcome },
    /// Answer the test's last asks now, look its address up, and call [`Plan::looked`].
    Look { test: TestId },
    /// Start the test's process; kill it if it has not ended by `deadline`.
    Start { test: TestId, deadline: Nanos },
    /// The test is past its deadline: write its threads' stacks, kill its process group, then
    /// call [`Plan::ended`] with [`Exit::Killed`].
    Kill { test: TestId },
    /// File this run's pass at the address its asks answer now, and its row in the head.
    File { test: TestId },
    /// The test was not asked this time but one of its asks moved: its head row becomes
    /// [`Last::Unsettled`], so the head stays valid at the new state.
    Unsettle { test: TestId },
}

/// A call the machine refuses: the driver broke the protocol. Each names the test.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PlanError {
    /// `width` is zero: nothing could ever start.
    NoWidth,
    /// A test is asked twice.
    TwoAsked { test: TestId },
    /// The head's rows are not sorted by test, each once (row `row`).
    HeadOrder { row: usize },
    /// An answer for a test that is not waiting for it.
    NotLooking { test: TestId },
    /// An end for a test that is not running.
    NotRunning { test: TestId },
    /// A test ended as killed that the machine never asked to kill.
    KilledUnasked { test: TestId },
    /// `decide` was called twice on one plan.
    Decided,
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            PlanError::NoWidth => write!(f, "a test plan of width 0 would start nothing (rule 18)"),
            PlanError::TwoAsked { test } => write!(f, "test {} is asked twice", Hex(&test.0)),
            PlanError::HeadOrder { row } => write!(f, "the suite head's row {row} is not after the row before it"),
            PlanError::NotLooking { test } => write!(f, "an answer for test {}, which waits for none", Hex(&test.0)),
            PlanError::NotRunning { test } => write!(f, "an end for test {}, which is not running", Hex(&test.0)),
            PlanError::KilledUnasked { test } => {
                write!(f, "test {} ended killed, but its deadline had not passed (rule 18: only the plan kills)", Hex(&test.0))
            }
            PlanError::Decided => write!(f, "a test plan decides once"),
        }
    }
}

impl core::error::Error for PlanError {}

/// A safeguard taken out, for the simulation's planted-bug controls. Always `None` outside tests.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg_attr(not(test), allow(dead_code, reason = "only the tests plant a bug"))]
pub(crate) enum Plant {
    None,
    /// Only the first moved ask is looked up in the postings.
    FirstMovedOnly,
    /// A last failure is reported as a kept pass.
    KeepFailures,
    /// `tick` never kills a run past its deadline.
    NoDeadline,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    /// Waiting for [`Plan::looked`]; `missed` is why it runs if nothing is found.
    Looking { missed: Missed },
    Queued,
    Running { started: Nanos, deadline: Nanos },
    /// Killed past its deadline; its end is awaited.
    Killing { after: Nanos },
    Done,
}

#[derive(Clone, Copy, Debug)]
struct Row<'r> {
    test: TestId,
    should_panic: ShouldPanic<'r>,
    /// Why it runs, once it is queued.
    missed: Missed,
    /// Its last run's micros; unknown runs first.
    priority: u64,
    state: State,
}

/// One request's tests. See the module's documentation.
pub struct Plan<'a, 'r> {
    config: Config<'r>,
    /// The tests that passed the filter, in the order asked; `by_test` indexes them by id.
    rows: AVec<'a, Row<'r>>,
    by_test: AVec<'a, (TestId, usize)>,
    /// Rows waiting to start, sorted so the next to start is LAST: (priority, reversed id order).
    queue: AVec<'a, (u64, TestId, usize)>,
    running: u32,
    filtered: u32,
    decided: bool,
    alloc: Alloc<'a>,
    pub(crate) plant: Plant,
}

impl<'a, 'r> Plan<'a, 'r> {
    pub fn new(config: Config<'r>, alloc: Alloc<'a>) -> Result<Plan<'a, 'r>, PlanError> {
        if config.width == 0 {
            return Err(PlanError::NoWidth);
        }
        Ok(Plan {
            config,
            rows: Vec::new_in(alloc),
            by_test: Vec::new_in(alloc),
            queue: Vec::new_in(alloc),
            running: 0,
            filtered: 0,
            decided: false,
            alloc,
            plant: Plant::None,
        })
    }

    /// Decides every asked test: filtered out (counted, not reported), reported now (ignored, a
    /// bench, a kept pass), looked up, or queued to run; then starts what fits.
    ///
    /// `kept` is the suite head's rows, sorted by test; `moved` the asks whose answers moved since
    /// the head (in any order); `postings` the suite's postings record.
    pub fn decide(
        &mut self,
        asked: &[Asked<'r>],
        kept: &[Kept],
        moved: &[Ask],
        postings: &Postings<'_>,
        now: Nanos,
        out: &mut AVec<'_, Effect>,
    ) -> Result<(), PlanError> {
        if self.decided {
            return Err(PlanError::Decided);
        }
        self.decided = true;
        for row in 1..kept.len() {
            if kept[row - 1].test >= kept[row].test {
                return Err(PlanError::HeadOrder { row });
            }
        }
        // The candidates: each test a moved ask names, with the least such ask (for `Missed`).
        let mut named: AVec<'_, TestId> = Vec::new_in(self.alloc);
        let mut candidates: AVec<'_, (TestId, Ask)> = Vec::new_in(self.alloc);
        let looked_up = match self.plant {
            Plant::FirstMovedOnly => moved.len().min(1),
            _ => moved.len(),
        };
        for ask in &moved[..looked_up] {
            named.clear();
            postings.tests_of(*ask, &mut named);
            for test in named.iter() {
                candidates.push((*test, *ask));
            }
        }
        candidates.sort_unstable();
        candidates.dedup_by(|later, first| later.0 == first.0);

        for one in asked {
            let head = kept.binary_search_by(|row| row.test.cmp(&one.test)).ok().map(|row| kept[row]);
            let candidate = candidates.binary_search_by(|(test, _)| test.cmp(&one.test)).ok().map(|found| candidates[found].1);
            // A kept pass of a test not decided here, whose asks moved, is unsettled so the head
            // stays valid at the new state.
            let unsettle = matches!((head, candidate), (Some(Kept { last: Last::Passed { .. }, .. }), Some(_)));
            if !self.lets_through(one) {
                self.filtered += 1;
                if unsettle {
                    out.push(Effect::Unsettle { test: one.test });
                }
                continue;
            }
            let at = self.rows.len();
            match self.by_test.binary_search_by(|(test, _)| test.cmp(&one.test)) {
                Ok(_) => return Err(PlanError::TwoAsked { test: one.test }),
                Err(slot) => self.by_test.insert_shift(slot, (one.test, at)),
            }
            let priority = head.map_or(u64::MAX, |row| row.micros);
            self.rows.push(Row { test: one.test, should_panic: one.should_panic, missed: Missed::NoRecord, priority, state: State::Done });
            let not_run = match (one.bench && !self.config.benches, one.ignore.is_some() && self.config.ignored == Ignored::Skip) {
                (true, _) => Some(Outcome::Bench),
                (false, true) => Some(Outcome::Ignored),
                (false, false) => None,
            };
            if let Some(outcome) = not_run {
                if unsettle {
                    out.push(Effect::Unsettle { test: one.test });
                }
                out.push(Effect::Report { test: one.test, outcome });
                continue;
            }
            let last = match (self.config.run_all, head) {
                (true, _) => Err(Missed::RunAll),
                (false, None) => Err(Missed::NoRecord),
                (false, Some(row)) => Ok(row.last),
            };
            match (last, candidate) {
                (Err(missed), _) => self.enqueue(at, missed),
                (Ok(Last::Failed), _) if self.plant == Plant::KeepFailures => {
                    out.push(Effect::Report { test: one.test, outcome: Outcome::Cached(Basis::Unchanged) });
                }
                (Ok(Last::Failed), _) => self.enqueue(at, Missed::LastFailed),
                (Ok(Last::NotKept(why)), _) => self.enqueue(at, Missed::LastNotKept(why)),
                (Ok(Last::Passed { .. }), Some(ask)) => {
                    self.rows[at].state = State::Looking { missed: Missed::Moved { ask } };
                    out.push(Effect::Look { test: one.test });
                }
                (Ok(Last::Unsettled { .. }), _) => {
                    self.rows[at].state = State::Looking { missed: Missed::Unsettled };
                    out.push(Effect::Look { test: one.test });
                }
                (Ok(Last::Passed { .. }), None) => {
                    out.push(Effect::Report { test: one.test, outcome: Outcome::Cached(Basis::Unchanged) });
                }
            }
        }
        self.pump(now, out);
        Ok(())
    }

    /// Whether the filter, `--skip` and the ignore mode let `one` through: the one home of the
    /// filter, read by `decide` and by `blaze test --list`.
    pub fn lets_through(&self, one: &Asked<'_>) -> bool {
        let named = match self.config.filter {
            Filter::All => true,
            Filter::Substring(text) => one.path.contains(text),
            Filter::Exact(text) => one.path == text,
        };
        let skipped = self.config.skip.iter().any(|text| one.path.contains(*text));
        let ignore_mode = self.config.ignored != Ignored::Only || one.ignore.is_some();
        named && !skipped && ignore_mode
    }

    /// Queues row `at` to run because of `missed`.
    fn enqueue(&mut self, at: usize, missed: Missed) {
        let row = &mut self.rows[at];
        row.missed = missed;
        row.state = State::Queued;
        // The next to start is last: the longest last run (unknown first), then the least id.
        let entry = (row.priority, TestId(invert(&row.test.0)), at);
        let slot = self.queue.partition_point(|queued| (queued.0, queued.1) < (entry.0, entry.1));
        self.queue.insert_shift(slot, entry);
    }

    /// Starts queued runs while fewer than `width` run.
    fn pump(&mut self, now: Nanos, out: &mut AVec<'_, Effect>) {
        while self.running < self.config.width {
            let Some((_, _, at)) = self.queue.pop() else { break };
            let deadline = now.after(self.config.deadline);
            self.rows[at].state = State::Running { started: now, deadline };
            self.running += 1;
            out.push(Effect::Start { test: self.rows[at].test, deadline });
        }
    }

    fn row_of(&self, test: TestId) -> Option<usize> {
        let found = self.by_test.binary_search_by(|(id, _)| id.cmp(&test)).ok()?;
        Some(self.by_test[found].1)
    }

    /// The answer to [`Effect::Look`]: whether a pass is filed at the address the test's last
    /// asks answer now.
    pub fn looked(&mut self, test: TestId, found: bool, now: Nanos, out: &mut AVec<'_, Effect>) -> Result<(), PlanError> {
        let Some(at) = self.row_of(test) else { return Err(PlanError::NotLooking { test }) };
        let State::Looking { missed } = self.rows[at].state else { return Err(PlanError::NotLooking { test }) };
        if found {
            self.rows[at].state = State::Done;
            out.push(Effect::Report { test, outcome: Outcome::Cached(Basis::Answers) });
        } else {
            self.enqueue(at, missed);
            self.pump(now, out);
        }
        Ok(())
    }

    /// A run ended. Judges it, says whether to file it, reports it, and starts the next.
    pub fn ended(&mut self, test: TestId, ran: Ran<'_>, now: Nanos, out: &mut AVec<'_, Effect>) -> Result<(), PlanError> {
        let Some(at) = self.row_of(test) else { return Err(PlanError::NotRunning { test }) };
        let row = self.rows[at];
        let judged = match (row.state, ran.exit) {
            (State::Killing { after }, _) => Err(Failure::TimedOut { after }),
            (State::Running { .. }, Exit::Killed) => return Err(PlanError::KilledUnasked { test }),
            (State::Running { .. }, exit) => judge(row.should_panic, exit),
            _ => return Err(PlanError::NotRunning { test }),
        };
        self.rows[at].state = State::Done;
        self.rows[at].priority = ran.micros;
        self.running -= 1;
        let outcome = match judged {
            Ok(()) => {
                if ran.keep.is_ok() {
                    out.push(Effect::File { test });
                }
                Outcome::Passed { ran: row.missed, kept: ran.keep }
            }
            Err(why) => Outcome::Failed { ran: row.missed, why },
        };
        out.push(Effect::Report { test, outcome });
        self.pump(now, out);
        Ok(())
    }

    /// A run the machine started ([`Effect::Start`]) that the driver could not start at all (no
    /// code for the test's body yet, no runner on this host). Nothing ran, so nothing is judged,
    /// reported or filed here: the caller reports it as not run, and it is never a pass. Its slot
    /// is freed and the next queued run starts.
    pub fn not_run(&mut self, test: TestId, now: Nanos, out: &mut AVec<'_, Effect>) -> Result<(), PlanError> {
        let Some(at) = self.row_of(test) else { return Err(PlanError::NotRunning { test }) };
        if !matches!(self.rows[at].state, State::Running { .. } | State::Killing { .. }) {
            return Err(PlanError::NotRunning { test });
        }
        self.rows[at].state = State::Done;
        self.running -= 1;
        self.pump(now, out);
        Ok(())
    }

    /// The driver's clock reached `now`: every run past its deadline is killed, in row order.
    pub fn tick(&mut self, now: Nanos, out: &mut AVec<'_, Effect>) {
        if self.plant == Plant::NoDeadline {
            return;
        }
        for row in self.rows.iter_mut() {
            if let State::Running { started, deadline } = row.state
                && deadline <= now
            {
                row.state = State::Killing { after: Nanos(now.0.saturating_sub(started.0)) };
                out.push(Effect::Kill { test: row.test });
            }
        }
    }

    /// The earliest deadline of a run not yet killed: when the driver must call [`Plan::tick`].
    pub fn next_deadline(&self) -> Option<Nanos> {
        self.rows
            .iter()
            .filter_map(|row| match row.state {
                State::Running { deadline, .. } => Some(deadline),
                _ => None,
            })
            .min()
    }

    /// Every test let through has its outcome.
    pub fn done(&self) -> bool {
        self.decided && self.rows.iter().all(|row| row.state == State::Done)
    }

    /// How many asked tests the filter, `--skip` or `--ignored` left out (libtest's "filtered out").
    pub const fn filtered_out(&self) -> u32 {
        self.filtered
    }
}

/// The verdict of a run that ended by itself, by the Reference's "Testing attributes": a test
/// passes by returning, or, with `should_panic`, only by panicking, and with an expected text
/// only by a panic whose message holds it.
pub fn judge(should_panic: ShouldPanic<'_>, exit: Exit<'_>) -> Result<(), Failure> {
    match (should_panic, exit) {
        (ShouldPanic::No, Exit::Returned) => Ok(()),
        (ShouldPanic::No, Exit::Panicked { .. }) => Err(Failure::Panicked),
        (ShouldPanic::Yes | ShouldPanic::WithText(_), Exit::Returned) => Err(Failure::DidNotPanic),
        (ShouldPanic::Yes, Exit::Panicked { .. }) => Ok(()),
        (ShouldPanic::WithText(want), Exit::Panicked { text }) => match text.contains(want) {
            true => Ok(()),
            false => Err(Failure::PanicTextMissing),
        },
        (_, Exit::Signal { number }) => Err(Failure::Signal { number }),
        (_, Exit::Refused) => Err(Failure::Refused),
        (_, Exit::Killed) => Err(Failure::TimedOut { after: Nanos(0) }),
        (_, Exit::NoReport { code }) => Err(Failure::NoReport { code }),
    }
}

/// The bytes' complement: sorts ids in reverse, so the least id is last in the queue.
fn invert(bytes: &[u8; 32]) -> [u8; 32] {
    let mut out = [0; 32];
    for (to, from) in out.iter_mut().zip(bytes) {
        *to = !*from;
    }
    out
}
