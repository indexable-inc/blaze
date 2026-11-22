//! A tape-search test's parent side (docs/v2/plan/design/test.md, "End state: property,
//! differential and end-to-end tests", A, "The case board"; G, piece 3): the machine the test
//! runner drives over the case boards its children leave. The first child runs the test's checks
//! from a board (the given tapes, then the seed phase, in the child, with no round trip per
//! case); when it is gone the runner opens its board and hands it to [`Property::ended`]. A pass
//! is the verdict at once. A failure starts blaze-sim's sans-IO `Shrinker` here, in the parent:
//! each candidate it proposes runs in a FRESH child from the zygote, on a board whose plan gives
//! that tape alone ([`Rerun::spec`]), and comes back through [`Property::ran`]. When the shrinker
//! is done the least tape is replayed once more in a fresh child: failing the same way, it is
//! the verdict [`PropVerdict::Failed`]; otherwise it is [`PropVerdict::CaseNotAFunction`] (the
//! case read state an earlier case left), filed as a finding: never retried, never hidden.
//!
//! "The same way" is the board's own word for how the case failed: a failure the body RETURNED,
//! with its origin (oracle, harness, error, overrun), or a case that ENDED THE CHILD (a panic,
//! which ends the child where blaze builds it, or a kill at the per-case deadline), with how the
//! child ended as its driver saw it ([`ChildEnd`]). A candidate whose child ends in another check
//! than the failing one, passes, discards or draws nothing did not fail the same way.
//!
//! Sans-IO: no clock, no process, no file. The driver forks, maps and opens the boards, and passes
//! each opened [`Board`] in; the machine pushes what is to be done next into the caller's buffer.
//! The candidate's tape and the least tape are in the machine's own buffers ([`Property::tape`],
//! [`Property::least`]), handed out as values, so the least tape is a tape (text.rs form when
//! printed) and this module does not depend on the failing-tape records (tapes.rs).
//!
//! Refused by name, never guessed ([`PropertyError`]): a seeded case that ended the child (the
//! board holds its seed, not the values it drew, so there is nothing to shrink from: see
//! `SeededCaseUnrecorded`), a record the board cut to its capacity, a board that is not the
//! candidate's, a result for no candidate in flight, a child that exited 0 inside a case.

use core::fmt;

use blaze_own::{AString, AVec, Alloc, String, Vec};
use blaze_sim::board::{Board, Budget, CaseOrigin, CaseRecord, End, Progress, Seeds, Spec, TailWord};
use blaze_sim::shrink::Shrinker;
use blaze_sim::{Choice, Record, Span};

use crate::TestId;

/// The label a value from a board carries in the record the shrinker takes: the board keeps a
/// case's values and spans, not the labels its draws gave them (the shrinker reads none).
const FROM_BOARD: &str = "board";

/// How the runner shrinks. `most_shrink_runs` caps the check's own declared shrink runs (the
/// board's `Declared::shrink_runs`), so no test makes its parent fork without bound (rule 18).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PropertyConfig {
    pub most_shrink_runs: u32,
}

impl PropertyConfig {
    /// At most 4,096 candidate children per failure: blaze-sim's own default is 1,000
    /// (search.rs `Config::DEFAULT`), so a check that declares nothing is never cut.
    pub const DEFAULT: PropertyConfig = PropertyConfig { most_shrink_runs: 4096 };
}

/// How a child ended, as its driver saw it after reaping it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChildEnd {
    /// It exited with this status (0: the test passed as a process).
    Exited(i32),
    /// A signal ended it (an abort after a panic, where blaze builds without unwinding).
    Signaled(i32),
    /// The runner killed it at the per-case deadline the board's budget gives.
    Deadline,
}

impl fmt::Display for ChildEnd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChildEnd::Exited(status) => write!(f, "exited with status {status}"),
            ChildEnd::Signaled(signal) => write!(f, "was ended by signal {signal}"),
            ChildEnd::Deadline => write!(f, "was killed at its per-case deadline"),
        }
    }
}

/// How a case failed: what "failed the same way" compares.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fault {
    /// The body returned a failure of this origin; the board holds the case's record.
    Returned(CaseOrigin),
    /// The case ended the child (a panic, an abort, the deadline): the board says it was
    /// running, on a tape whose prefix is the case.
    EndedInCase(ChildEnd),
}

/// One child to run from the zygote: a fresh board whose plan gives [`Property::tape`] alone
/// to check `check` (the check that failed), with the first board's budget and capacities.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rerun {
    /// From 1 per test; [`Property::ran`] takes it back.
    pub id: u32,
    pub check: u32,
    pub budget: Budget,
    pub record_cap: u32,
    pub coverage_bytes: u32,
}

impl Rerun {
    /// The board's plan for this run: `given` is `[property.tape()]`; no seeds, so the child runs
    /// the given tape alone, on a calm tail.
    pub const fn spec<'s>(&self, given: &'s [&'s [u64]]) -> Spec<'s> {
        Spec {
            target_check: self.check,
            given,
            seeds: Seeds::Range { first: 0, count: 0 },
            budget: self.budget,
            record_cap: self.record_cap,
            coverage_bytes: self.coverage_bytes,
        }
    }
}

/// Why a test's run said nothing about its property: test.md's later section, "vacuity a
/// failure". The runner reports each as a failure of the test.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Vacuity {
    /// No case drew a choice.
    DrewNothing { cases: u32 },
    /// Every case discarded itself.
    AllDiscarded { cases: u32 },
    /// Points no passing case reached: [`Property::text`] names them, joined by `, `.
    Unreached { cases: u32 },
    /// A tape the check's own config gave did not fit the case.
    Misfit { clamped: u32, unused: u32 },
}

/// What the runner files for the test.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PropVerdict {
    /// Every case of the last check ran and passed, and the child exited 0.
    Passed { cases: u32 },
    /// The least tape ([`Property::least`]) failed the same way on its replay. `case` is the
    /// first child's failing case (from 1, across the given tapes and the seeds); the failure's
    /// text, when the body returned one, is [`Property::text`].
    Failed { check: u32, case: u32, fault: Fault, shrink_runs: u32 },
    /// The least tape's replay in a fresh child did not fail as the first child's case did
    /// (`replay` is how it failed, or `None` when it did not): the `CaseNotAFunction` finding.
    CaseNotAFunction { check: u32, case: u32, fault: Fault, replay: Option<Fault> },
    /// The checks ran and said nothing.
    Vacuous(Vacuity),
    /// The child never opened the board: an opaque test after all (A, "Two kinds"); the head
    /// row takes the kind this run showed, and the child's end is the test's result.
    Opaque { how: ChildEnd },
    /// No case failed, yet the child did not exit 0 (it failed after or between its checks):
    /// a failure of the test that no tape reproduces.
    FailedOutside { how: ChildEnd },
}

/// What the driver does next.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PropAct {
    /// Run [`Property::tape`] (a shrink candidate) in a fresh child, then call `ran`.
    RunCandidate(Rerun),
    /// Run [`Property::tape`] (the least tape) once in a fresh child, then call `ran`.
    Replay(Rerun),
    Done(PropVerdict),
}

/// A call the machine refuses, naming the thing (test.md, A, "The case board").
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PropertyError {
    /// `ended` after the first child's board was taken, or `ran` after the verdict.
    NotFirst,
    /// `ran` for a run that is not the one in flight.
    NotWaiting { id: u32, waiting: Option<u32> },
    /// The board handed to `ran` is not the one `Rerun::spec` gave for run `id`.
    NotTheCandidate { id: u32 },
    /// A case on a seeded tail ended the child: the board holds its seed and its prefix, not the
    /// values it drew, so the runner has no tape to shrink or replay. Wants the board to keep a
    /// case's choices as they are drawn (test.md A: "the record of that case's choices as they
    /// are drawn"; board.rs writes the record only when a case returns).
    SeededCaseUnrecorded { check: u32, case: u32, seed: u64 },
    /// A failed case drew more than the board's record capacity: shrinking from the cut values
    /// would shrink another case.
    RecordCut { check: u32, case: u32, cap: u32 },
    /// The board says a case was running, yet the child exited 0.
    ExitedInCase { check: u32, case: u32 },
    /// A tape or a record longer than a `u32` counts.
    TooLong { what: &'static str },
}

impl fmt::Display for PropertyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const RULE: &str = "test.md, A, \"The case board\"";
        match self {
            PropertyError::NotFirst => {
                write!(f, "a property test's first board was given twice, or after its verdict ({RULE})")
            }
            PropertyError::NotWaiting { id, waiting: Some(waiting) } => {
                write!(f, "run {id} came back while run {waiting} is the one in flight ({RULE})")
            }
            PropertyError::NotWaiting { id, waiting: None } => {
                write!(f, "run {id} came back with no run in flight ({RULE})")
            }
            PropertyError::NotTheCandidate { id } => {
                write!(f, "the board given for run {id} does not plan that run's tape alone ({RULE})")
            }
            PropertyError::SeededCaseUnrecorded { check, case, seed } => write!(
                f,
                "check {check}, case {case} (seed {seed}) ended the child, and the board keeps no values it drew: nothing to shrink ({RULE}: the record of a case's choices as they are drawn)"
            ),
            PropertyError::RecordCut { check, case, cap } => write!(
                f,
                "check {check}, case {case} drew more than the board's record capacity of {cap} values: its record is cut ({RULE})"
            ),
            PropertyError::ExitedInCase { check, case } => {
                write!(f, "the board says check {check}, case {case} was running, yet the child exited 0 ({RULE})")
            }
            PropertyError::TooLong { what } => write!(f, "{what} is longer than a u32 counts ({RULE})"),
        }
    }
}

impl core::error::Error for PropertyError {}

/// A bug a control plants, for the simulation (sim/property.rs).
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PropPlant {
    None,
    /// A candidate whose child did not fail is taken as failing the same way.
    AcceptUnfailed,
}

/// The first child's failing case, which every later run is compared with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct First {
    check: u32,
    case: u32,
    fault: Fault,
    budget: Budget,
    record_cap: u32,
    coverage_bytes: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    /// The first child's board has not come.
    First,
    /// Candidate `id` is in flight.
    Shrinking { id: u32 },
    /// The least tape's replay, run `id`, is in flight.
    Replay { id: u32 },
    Done,
}

/// One tape-search test's run, from its first child's board to its verdict.
pub struct Property<'a> {
    test: TestId,
    config: PropertyConfig,
    phase: Phase,
    first: Option<First>,
    shrinker: Option<Shrinker>,
    /// The last board's case as the shrinker takes it (values and spans).
    record: Record,
    /// The tape of the run in flight.
    tape: AVec<'a, u64>,
    /// The least tape, once the shrinker is done.
    least: AVec<'a, u64>,
    /// The first failure's text, or an unreached run's missing points.
    text: AString<'a>,
    next_id: u32,
    #[cfg(test)]
    plant: PropPlant,
}

impl<'a> Property<'a> {
    pub fn new(test: TestId, config: PropertyConfig, alloc: Alloc<'a>) -> Property<'a> {
        Property {
            test,
            config,
            phase: Phase::First,
            first: None,
            shrinker: None,
            record: Record::new(),
            tape: Vec::new_in(alloc),
            least: Vec::new_in(alloc),
            text: String::new_in(alloc),
            next_id: 1,
            #[cfg(test)]
            plant: PropPlant::None,
        }
    }

    /// `new` with a bug planted, for a control.
    #[cfg(test)]
    pub(crate) fn planted(test: TestId, config: PropertyConfig, alloc: Alloc<'a>, plant: PropPlant) -> Property<'a> {
        let mut property = Property::new(test, config, alloc);
        property.plant = plant;
        property
    }

    pub const fn test(&self) -> TestId {
        self.test
    }

    /// The tape of the run in flight (`RunCandidate`, `Replay`).
    pub fn tape(&self) -> &[u64] {
        &self.tape
    }

    /// The least tape, after `Failed` or `CaseNotAFunction`; empty before.
    pub fn least(&self) -> &[u64] {
        &self.least
    }

    /// The first failure's text (when the body returned one), or an unreached run's points.
    pub fn text(&self) -> &str {
        self.text.as_str()
    }

    /// The first child is gone and `board` is its board, opened; `how` is how it ended.
    pub fn ended(&mut self, board: &Board<'_>, how: ChildEnd, out: &mut AVec<'_, PropAct>) -> Result<(), PropertyError> {
        if self.phase != Phase::First {
            return Err(PropertyError::NotFirst);
        }
        let shape = board.shape();
        let (counts, case, fault) = match board.progress() {
            Progress::Untouched => return self.done(PropVerdict::Opaque { how }, out),
            Progress::Begun(_) => return self.done(PropVerdict::FailedOutside { how }, out),
            Progress::Running { counts, case, tape } => {
                if how == ChildEnd::Exited(0) {
                    return Err(PropertyError::ExitedInCase { check: counts.check, case });
                }
                match tape.tail {
                    TailWord::Seeded(seed) => {
                        return Err(PropertyError::SeededCaseUnrecorded { check: counts.check, case, seed });
                    }
                    TailWord::Calm => fill_from_values(&mut self.record, tape.prefix()),
                }
                (counts, case, Fault::EndedInCase(how))
            }
            Progress::Ended { counts, case, end, .. } => match end {
                End::Passed if how == ChildEnd::Exited(0) => {
                    return self.done(PropVerdict::Passed { cases: counts.tally.passed }, out);
                }
                End::Passed => return self.done(PropVerdict::FailedOutside { how }, out),
                End::Failed { origin, record, text, .. } => {
                    if record.cut {
                        return Err(PropertyError::RecordCut { check: counts.check, case, cap: shape.record_cap });
                    }
                    fill_from_record(&mut self.record, &record)?;
                    self.text.clear();
                    self.text.push_str(text);
                    (counts, case, Fault::Returned(origin))
                }
                End::DrewNothing => return self.done(PropVerdict::Vacuous(Vacuity::DrewNothing { cases: case }), out),
                End::AllDiscarded => {
                    return self.done(PropVerdict::Vacuous(Vacuity::AllDiscarded { cases: case }), out);
                }
                End::Unreached { missing, .. } => {
                    self.text.clear();
                    self.text.push_str(missing);
                    return self.done(PropVerdict::Vacuous(Vacuity::Unreached { cases: case }), out);
                }
                End::Misfit { clamped, unused } => {
                    return self.done(PropVerdict::Vacuous(Vacuity::Misfit { clamped, unused }), out);
                }
            },
        };
        let budget = Budget { most_choices: counts.declared.most_choices, ..shape.budget };
        self.first = Some(First {
            check: counts.check,
            case,
            fault,
            budget,
            record_cap: shape.record_cap,
            coverage_bytes: shape.coverage_bytes,
        });
        let runs = counts.declared.shrink_runs.min(self.config.most_shrink_runs);
        self.shrinker = Some(Shrinker::new(&self.record, runs));
        self.propose(out)
    }

    /// Run `id` (a candidate or the replay) is gone; `board` is its board, opened.
    pub fn ran(&mut self, id: u32, board: &Board<'_>, how: ChildEnd, out: &mut AVec<'_, PropAct>) -> Result<(), PropertyError> {
        let waiting = match self.phase {
            Phase::Shrinking { id } | Phase::Replay { id } => Some(id),
            Phase::First | Phase::Done => None,
        };
        if waiting != Some(id) {
            return Err(PropertyError::NotWaiting { id, waiting });
        }
        let Some(first) = self.first else { return Err(PropertyError::NotWaiting { id, waiting: None }) };
        if !self.is_candidate_board(board, first.check) {
            return Err(PropertyError::NotTheCandidate { id });
        }
        let seen = self.fault_of(board, how, first.check)?;
        #[cfg(test)]
        let seen = match (self.plant, seen) {
            (PropPlant::AcceptUnfailed, None) => {
                fill_from_values(&mut self.record, self.tape.iter().copied());
                Some(first.fault)
            }
            (PropPlant::None | PropPlant::AcceptUnfailed, seen) => seen,
        };
        let same = seen == Some(first.fault);
        match self.phase {
            Phase::Shrinking { .. } => {
                let Some(shrinker) = self.shrinker.as_mut() else {
                    return Err(PropertyError::NotWaiting { id, waiting: None });
                };
                shrinker.ran(&self.record, same);
                self.propose(out)
            }
            Phase::Replay { .. } | Phase::First | Phase::Done => {
                let verdict = if same {
                    let shrink_runs = self.shrinker.as_ref().map_or(0, Shrinker::runs);
                    PropVerdict::Failed { check: first.check, case: first.case, fault: first.fault, shrink_runs }
                } else {
                    PropVerdict::CaseNotAFunction { check: first.check, case: first.case, fault: first.fault, replay: seen }
                };
                self.done(verdict, out)
            }
        }
    }

    /// The verdict's line, for the run report: `FAILED <path>: least tape <tape>` with the
    /// tape in text.rs's form.
    pub fn write_verdict(&self, out: &mut impl fmt::Write, path: &str, verdict: &PropVerdict) -> fmt::Result {
        match verdict {
            PropVerdict::Passed { cases } => write!(out, "ok {path}: {cases} cases"),
            PropVerdict::Failed { .. } => {
                write!(out, "FAILED {path}: least tape ")?;
                write_tape(out, &self.least)
            }
            PropVerdict::CaseNotAFunction { .. } => {
                write!(out, "FAILED {path}: case not a function: least tape ")?;
                write_tape(out, &self.least)?;
                write!(out, " did not fail the same way when replayed alone")
            }
            PropVerdict::Vacuous(Vacuity::DrewNothing { cases }) => {
                write!(out, "FAILED {path}: {cases} cases drew nothing")
            }
            PropVerdict::Vacuous(Vacuity::AllDiscarded { cases }) => {
                write!(out, "FAILED {path}: all {cases} cases discarded themselves")
            }
            PropVerdict::Vacuous(Vacuity::Unreached { cases }) => {
                write!(out, "FAILED {path}: {cases} cases never reached {}", self.text.as_str())
            }
            PropVerdict::Vacuous(Vacuity::Misfit { clamped, unused }) => {
                write!(out, "FAILED {path}: a given tape did not fit ({clamped} values clamped, {unused} unused)")
            }
            PropVerdict::Opaque { how } => write!(out, "{path}: never opened its case board; the child {how}"),
            PropVerdict::FailedOutside { how } => write!(out, "FAILED {path}: no case failed, yet the child {how}"),
        }
    }

    /// The shrinker's next candidate, or the least tape's replay when it has none.
    fn propose(&mut self, out: &mut AVec<'_, PropAct>) -> Result<(), PropertyError> {
        let Some(first) = self.first else { return Err(PropertyError::NotFirst) };
        let Some(shrinker) = self.shrinker.as_mut() else { return Err(PropertyError::NotFirst) };
        let id = self.next_id;
        self.next_id += 1;
        let candidate = shrinker.next();
        let replay = candidate.is_none();
        self.tape.clear();
        match candidate {
            Some(tape) => self.tape.extend_from_slice(tape),
            None => {
                self.least.clear();
                self.least.extend_from_slice(shrinker.best());
                self.tape.extend_from_slice(shrinker.best());
            }
        }
        let len = u32::try_from(self.tape.len()).map_err(|_| PropertyError::TooLong { what: "a candidate tape" })?;
        let rerun = Rerun {
            id,
            check: first.check,
            budget: first.budget,
            record_cap: first.record_cap.max(len),
            coverage_bytes: first.coverage_bytes,
        };
        if replay {
            self.phase = Phase::Replay { id };
            out.push(PropAct::Replay(rerun));
        } else {
            self.phase = Phase::Shrinking { id };
            out.push(PropAct::RunCandidate(rerun));
        }
        Ok(())
    }

    fn done(&mut self, verdict: PropVerdict, out: &mut AVec<'_, PropAct>) -> Result<(), PropertyError> {
        self.phase = Phase::Done;
        out.push(PropAct::Done(verdict));
        Ok(())
    }

    /// The board plans the tape in flight alone, for `check`.
    fn is_candidate_board(&self, board: &Board<'_>, check: u32) -> bool {
        let plan = board.plan();
        if board.shape().target_check != check || plan.tapes_len() != 1 {
            return false;
        }
        plan.tapes().next().is_some_and(|given| given.values().eq(self.tape.iter().copied()))
    }

    /// How the run's case failed, with `self.record` its record; `None` when it did not fail in
    /// `check` (it passed, discarded, drew nothing, or ended in another check).
    fn fault_of(&mut self, board: &Board<'_>, how: ChildEnd, check: u32) -> Result<Option<Fault>, PropertyError> {
        clear(&mut self.record);
        match board.progress() {
            Progress::Running { counts, case, tape } if counts.check == check => {
                if how == ChildEnd::Exited(0) {
                    return Err(PropertyError::ExitedInCase { check, case });
                }
                match tape.tail {
                    TailWord::Seeded(seed) => Err(PropertyError::SeededCaseUnrecorded { check, case, seed }),
                    TailWord::Calm => {
                        fill_from_values(&mut self.record, tape.prefix());
                        Ok(Some(Fault::EndedInCase(how)))
                    }
                }
            }
            Progress::Ended { counts, case, end: End::Failed { origin, record, .. }, .. } if counts.check == check => {
                if record.cut {
                    return Err(PropertyError::RecordCut { check, case, cap: board.shape().record_cap });
                }
                fill_from_record(&mut self.record, &record)?;
                Ok(Some(Fault::Returned(origin)))
            }
            Progress::Untouched | Progress::Begun(_) | Progress::Running { .. } | Progress::Ended { .. } => Ok(None),
        }
    }
}

/// Empties `record` keeping its buffers.
fn clear(record: &mut Record) {
    record.choices.clear();
    record.spans.clear();
    record.notes.clear();
    record.reached.clear();
    record.discarded = false;
    record.overran = false;
    record.clamped = 0;
    record.unused = 0;
}

/// `record` as the case's values with no spans (a case that ended the child: its prefix).
fn fill_from_values(record: &mut Record, values: impl Iterator<Item = u64>) {
    clear(record);
    record.choices.extend(values.map(|value| Choice { value, most: u64::MAX, label: FROM_BOARD }));
}

/// `record` as a failed case's record on the board: its values and its spans.
fn fill_from_record(record: &mut Record, from: &CaseRecord<'_>) -> Result<(), PropertyError> {
    fill_from_values(record, from.values());
    for (start, end) in from.spans() {
        record.spans.push(Span { label: FROM_BOARD, start, end });
    }
    // The board's spans lie inside its values (board.rs `Writer::end` keeps only those).
    u32::try_from(record.choices.len()).map_err(|_| PropertyError::TooLong { what: "a case's record" })?;
    Ok(())
}

/// A tape in blaze-sim's text form (text.rs `print`): decimal values, one space between, a run
/// of two or more zeros as `0*n`, trailing zeros cut. Written here through `fmt::Write` so the
/// report allocates nothing; sim/property.rs checks it equals `blaze_sim::text::print`. Goes when
/// text.rs gives a `Display` form of its own (one home for the form, rule 30).
pub fn write_tape(out: &mut impl fmt::Write, values: &[u64]) -> fmt::Result {
    let kept = values.iter().rposition(|&value| value != 0).map_or(0, |last| last + 1);
    let values = &values[..kept];
    let mut at = 0;
    while at < values.len() {
        if at > 0 {
            out.write_char(' ')?;
        }
        let zeros = values[at..].iter().take_while(|&&value| value == 0).count();
        if zeros >= 2 {
            write!(out, "0*{zeros}")?;
            at += zeros;
        } else {
            write!(out, "{}", values[at])?;
            at += 1;
        }
    }
    Ok(())
}
