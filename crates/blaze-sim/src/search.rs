//! The search: the sans-IO engine of a check (docs/v2/plan/design/sim.md, "The design").
//!
//! A [`Search`] proposes the cases of a run and takes back what each drew and how it ended; it
//! runs nothing itself. It runs a given tape alone, or a given seed alone, or the seeds
//! `first..first + cases`. On the first failure it hands the failing tape to the
//! [`Shrinker`](crate::shrink::Shrinker), runs the least tape it found once more on a calm tail,
//! and ends on a [`Verdict`]. Every phase is bounded: by the cases, by the shrinker's runs, and
//! by the choices one case may draw.

#![blaze::sans_io]

use std::alloc::Global;

use blaze_rng::SplitMix64;

#[cfg(test)]
use crate::shrink;
use crate::shrink::Shrinker;
use crate::tape::{Record, Tail, Tape};
use crate::text::{self, TextError};

/// The setting that gives one seed to run alone.
pub const SEED: &str = "BLAZE_SIM_SEED";
/// The setting that gives how many cases to run.
pub const SEEDS: &str = "BLAZE_SIM_SEEDS";
/// The setting that gives one tape to run alone.
pub const TAPE: &str = "BLAZE_SIM_TAPE";

/// What a search runs and how far it may go.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// Cases to run: seeds `first..first + cases`.
    pub cases: u32,
    pub first: u64,
    /// Runs the shrinker may spend; 0 reports the first failure as it was found.
    pub shrink_runs: u32,
    pub most_choices: u32,
    pub given: Given,
    /// Points some case has to reach, or the check fails.
    pub must_reach: &'static [&'static str],
}

impl Config {
    /// 256 cases from seed 0, 1,000 shrink runs, `Tape::MOST_CHOICES`, nothing given.
    pub const DEFAULT: Config = Config {
        cases: 256,
        first: 0,
        shrink_runs: 1000,
        most_choices: Tape::MOST_CHOICES,
        given: Given::NOTHING,
        must_reach: &[],
    };

    /// [`Config::DEFAULT`] with `cases` cases.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn cases(cases: u32) -> Config {
        let mut config = Config::DEFAULT;
        config.cases = cases;
        config
    }

    /// The first seed of the run.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn first(mut self, seed: u64) -> Config {
        self.first = seed;
        self
    }

    /// The runs the shrinker may spend.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn shrink_runs(mut self, runs: u32) -> Config {
        self.shrink_runs = runs;
        self
    }

    /// The most choices one case may draw.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn most_choices(mut self, most: u32) -> Config {
        self.most_choices = most;
        self
    }

    /// Points some passing case has to reach.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn must_reach(mut self, points: &'static [&'static str]) -> Config {
        self.must_reach = points;
        self
    }

    /// What the person gave from outside, in place of what this config had.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::nonblocking]
    pub fn given(mut self, given: Given) -> Config {
        self.given = given;
        self
    }
}

/// What the person gave from outside. A tape excludes a seed and a count.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Given {
    /// Run this seed alone (and shrink it if it fails).
    pub seed: Option<u64>,
    /// Run this many cases in place of `Config::cases`.
    pub seeds: Option<u32>,
    /// Run this tape alone, calm past its end, with no shrinking.
    pub tape: Option<Vec<u64>>,
}

impl Given {
    pub const NOTHING: Given = Given { seed: None, seeds: None, tape: None };

    /// From the three settings' texts (`BLAZE_SIM_SEED`, `BLAZE_SIM_SEEDS`, `BLAZE_SIM_TAPE`).
    /// A number is decimal digits only; a count of 0 is refused, as is a tape with a seed or a
    /// count, and a seed with a count (the seed runs alone, so the count would mean nothing).
    #[blaze::pure]
    #[blaze::nonblocking]
    pub fn parse(
        seed: Option<&str>,
        seeds: Option<&str>,
        tape: Option<&str>,
    ) -> Result<Given, GivenError> {
        let seed = match seed {
            Some(value) => Some(number::<u64>(SEED, value)?),
            None => None,
        };
        let seeds = match seeds {
            Some(value) => Some(number::<u32>(SEEDS, value)?),
            None => None,
        };
        if seeds == Some(0) {
            return Err(GivenError::ZeroSeeds);
        }
        let tape = match tape {
            Some(text) => {
                if seed.is_some() {
                    return Err(GivenError::TapeWith { setting: SEED });
                }
                if seeds.is_some() {
                    return Err(GivenError::TapeWith { setting: SEEDS });
                }
                Some(text::parse(text).map_err(|error| GivenError::Tape { error })?)
            }
            None => None,
        };
        if seed.is_some() && seeds.is_some() {
            return Err(GivenError::SeedWithSeeds);
        }
        Ok(Given { seed, seeds, tape })
    }
}

/// A setting's value as a decimal number: digits only, no sign, no space.
#[expect(
    blaze::dropped_error,
    reason = "the value is checked to be decimal digits first, so the parse can only fail by being out of the type's range, which `GivenError::NotANumber` says"
)]
fn number<N: core::str::FromStr>(setting: &'static str, value: &str) -> Result<N, GivenError> {
    let refused = || GivenError::NotANumber { setting, value: value.to_owned() };
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(refused());
    }
    value.parse::<N>().map_err(|_| refused())
}

/// Names the setting and its value, and why it was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GivenError {
    /// The setting is not a decimal number its type holds.
    NotANumber { setting: &'static str, value: String },
    /// `BLAZE_SIM_TAPE` is not a tape.
    Tape { error: TextError },
    /// `BLAZE_SIM_TAPE` was given with `setting`, which it excludes.
    TapeWith { setting: &'static str },
    /// `BLAZE_SIM_SEEDS` is 0.
    ZeroSeeds,
    /// `BLAZE_SIM_SEED` was given with `BLAZE_SIM_SEEDS`, which it excludes.
    SeedWithSeeds,
}

impl core::fmt::Display for GivenError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            GivenError::NotANumber { setting, value } => {
                write!(f, "`{setting}` is `{value}`, which is not a decimal number in range")
            }
            GivenError::Tape { error } => write!(f, "`{TAPE}` is not a tape: {error}"),
            GivenError::TapeWith { setting } => {
                write!(f, "`{TAPE}` is a whole case, so it excludes `{setting}`: give one of them")
            }
            GivenError::ZeroSeeds => write!(f, "`{SEEDS}` is 0: a check of no cases tests nothing"),
            GivenError::SeedWithSeeds => {
                write!(
                    f,
                    "`{SEED}` runs one seed alone, so it excludes `{SEEDS}`: give one of them"
                )
            }
        }
    }
}

impl std::error::Error for GivenError {}

/// How a case ended, as the driver saw it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ended {
    Passed,
    Failed(Failure),
    Discarded,
}

/// A case's failure: where it came from and what it said.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub origin: Origin,
    pub text: String,
}

/// What broke a case.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Broke {
    /// The oracle saw the invariant broken: a finding.
    Oracle(String),
    /// The test itself went wrong (a model out of step, a bound of the harness's own).
    Harness(String),
}

impl core::fmt::Display for Broke {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Broke::Oracle(why) => write!(f, "the oracle: {why}"),
            Broke::Harness(why) => write!(f, "the harness: {why}"),
        }
    }
}

impl std::error::Error for Broke {}

/// Where a failure came from: two failures are the same when their origins are equal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Origin {
    /// A panic, at `file:line:column`.
    Panic { at: String },
    /// `Broke::Oracle`.
    Oracle,
    /// `Broke::Harness`.
    Harness,
    /// An `Err` of another type.
    Error,
    /// The case drew more than its bound.
    Overran,
}

/// Which part of a search a case belongs to: the tests' view of the order of a search's stages.
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    Given,
    Generate,
    Shrink,
    Final,
}

/// The case to run next.
pub struct Proposal<'s> {
    /// The part of the search this case belongs to: the tests' view of the order of a search's
    /// stages, read by nothing else, so it exists only in test builds.
    #[cfg(test)]
    pub phase: Phase,
    pub prefix: &'s [u64],
    pub tail: Tail,
}

/// What a search wants next: a case run, or nothing, since it is done.
pub enum Next<'s> {
    Run(Proposal<'s>),
    Done(&'s Verdict),
}

/// A run that found no failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Passed {
    pub cases: u32,
    pub discarded: u32,
    pub choices: u64,
}

/// A run that found a failure, with the least tape it shrank it to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failed {
    /// The seed of the case that failed first; `None` for a given tape.
    pub seed: Option<u64>,
    /// Which case it was, from 1, of `cases`.
    pub case: u32,
    pub cases: u32,
    pub failure: Failure,
    /// Choices the failing case drew before shrinking.
    pub first_len: u32,
    /// The least failing case, as its last run recorded it.
    pub least: Record,
    pub shrink_runs: u32,
}

/// What a search found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Passed(Passed),
    Failed(Failed),
    /// The least tape, run once more, did not fail the same way: the case reads something
    /// outside its tape.
    NotAFunction {
        seed: Option<u64>,
        tape: Vec<u64>,
        first: Failure,
        again: Ended,
    },
    /// No case drew a choice: it is no property test.
    DrewNothing {
        cases: u32,
    },
    AllDiscarded {
        cases: u32,
    },
    /// Points of `Config::must_reach` no passing case reached.
    Unreached {
        cases: u32,
        missing: Vec<&'static str>,
    },
    /// A given tape that did not fit the case: values lowered, or never drawn.
    Misfit {
        clamped: u32,
        unused: u32,
        ended: Ended,
    },
}

/// A bug a control plants in the search or its shrinker, for a test's oracle to catch (rule
/// dst-not-review). Passed in by value through [`Search::planted`].
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plant {
    None,
    /// The shrinker keeps a simpler candidate whether or not it failed.
    KeepsPassing,
    /// The shrinker keeps a failing candidate without asking whether its record is simpler, and
    /// the search runs each candidate on a seeded tail, so a record is longer than its candidate.
    KeepsNotSimpler,
    /// The search reports the least case as proposed, its choices cut to the prefix it was
    /// given, not as its run recorded it.
    ReportsProposed,
    /// The search skips the final run and reports the shrinker's best as a failure.
    SkipsFinalRun,
}

/// The shrinker's half of a search's plant: the bug it plants in the shrinker, if any.
#[cfg(test)]
fn shrinker_plant(plant: Plant) -> shrink::Plant {
    match plant {
        Plant::KeepsPassing => shrink::Plant::KeepsPassing,
        Plant::KeepsNotSimpler => shrink::Plant::KeepsNotSimpler,
        Plant::None | Plant::ReportsProposed | Plant::SkipsFinalRun => shrink::Plant::None,
    }
}

/// The first failure, kept while it is shrunk.
struct Found {
    seed: Option<u64>,
    case: u32,
    failure: Failure,
    first_len: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Stage {
    Given,
    Generate,
    Shrink,
    Final,
    Done,
}

/// The engine of one check: `next` proposes a case, `ran` takes how it went, until `next` says
/// it is done and gives the verdict.
pub struct Search {
    config: Config,
    /// The seed of the first case; the run is `first_seed..first_seed + count`.
    first_seed: u64,
    /// Cases the run has: one for a given seed or tape.
    count: u32,
    stage: Stage,
    /// Cases run so far, before any failure.
    ran: u32,
    passed: u32,
    discarded: u32,
    choices: u64,
    /// Whether some case drew a choice.
    drew: bool,
    /// The points passing cases reached, each once.
    reached: Vec<&'static str, Global>,
    found: Option<Found>,
    shrinker: Option<Shrinker>,
    /// The tape the final run replays: the shrinker's best.
    least: Vec<u64, Global>,
    verdict: Option<Verdict>,
    #[cfg(test)]
    plant: Plant,
}

impl Search {
    /// A search of `config`. A `Given` with a tape and a seed or a count (which `Given::parse`
    /// refuses) panics, naming both.
    #[blaze::pure]
    #[blaze::nonblocking]
    pub fn new(config: Config) -> Search {
        let given = &config.given;
        if let Some(tape) = &given.tape {
            if let Some(seed) = given.seed {
                panic!(
                    "blaze-sim: a given tape excludes a given seed, and this `Given` has both: a tape of {} values and seed {seed}",
                    tape.len()
                );
            }
            if let Some(seeds) = given.seeds {
                panic!(
                    "blaze-sim: a given tape excludes a given count, and this `Given` has both: a tape of {} values and a count of {seeds}",
                    tape.len()
                );
            }
        }
        let (stage, first_seed, count) = match (&given.tape, given.seed) {
            (Some(_), _) => (Stage::Given, 0, 1),
            (None, Some(seed)) => (Stage::Generate, seed, 1),
            (None, None) => (Stage::Generate, config.first, given.seeds.unwrap_or(config.cases)),
        };
        let mut search = Search {
            config,
            first_seed,
            count,
            stage,
            ran: 0,
            passed: 0,
            discarded: 0,
            choices: 0,
            drew: false,
            reached: Vec::new_in(Global),
            found: None,
            shrinker: None,
            least: Vec::new_in(Global),
            verdict: None,
            #[cfg(test)]
            plant: Plant::None,
        };
        if search.stage == Stage::Generate && search.count == 0 {
            search.finish(Reach::Asked);
        }
        search
    }

    /// `new` with a bug planted, for a control.
    #[cfg(test)]
    pub(crate) fn planted(config: Config, plant: Plant) -> Search {
        let mut search = Search::new(config);
        search.plant = plant;
        search
    }

    /// The most choices a case of this search may draw.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn most_choices(&self) -> u32 {
        self.config.most_choices
    }

    /// The most cases this search proposes: its cases, its shrink runs and the final run.
    #[blaze::pure]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn most_runs(&self) -> u64 {
        u64::from(self.count) + u64::from(self.config.shrink_runs) + 1
    }

    /// The case to run next, or the verdict once the search is done. Called again before `ran`,
    /// it proposes the same case.
    #[blaze::pure]
    #[blaze::nonblocking]
    pub fn next(&mut self) -> Next<'_> {
        if self.stage == Stage::Shrink {
            self.step_shrink();
        }
        match self.stage {
            Stage::Given => {
                let prefix = self
                    .config
                    .given
                    .tape
                    .as_deref()
                    .expect("a search of a given tape holds the tape");
                Next::Run(Proposal {
                    #[cfg(test)]
                    phase: Phase::Given,
                    prefix,
                    tail: Tail::Calm,
                })
            }
            Stage::Generate => {
                let seed = self.seed_of(self.ran);
                Next::Run(Proposal {
                    #[cfg(test)]
                    phase: Phase::Generate,
                    prefix: &[],
                    tail: Tail::Seeded(SplitMix64::new(seed)),
                })
            }
            Stage::Shrink => {
                let shrinker =
                    self.shrinker.as_ref().expect("a search that shrinks holds its shrinker");
                #[cfg(test)]
                let tail = match self.plant {
                    Plant::KeepsNotSimpler => {
                        Tail::Seeded(SplitMix64::new(u64::from(shrinker.runs())))
                    }
                    Plant::None
                    | Plant::KeepsPassing
                    | Plant::ReportsProposed
                    | Plant::SkipsFinalRun => Tail::Calm,
                };
                #[cfg(not(test))]
                let tail = Tail::Calm;
                Next::Run(Proposal {
                    #[cfg(test)]
                    phase: Phase::Shrink,
                    prefix: shrinker.candidate(),
                    tail,
                })
            }
            Stage::Final => Next::Run(Proposal {
                #[cfg(test)]
                phase: Phase::Final,
                prefix: &self.least,
                tail: Tail::Calm,
            }),
            Stage::Done => {
                Next::Done(self.verdict.as_ref().expect("a done search holds its verdict"))
            }
        }
    }

    /// What the case `next` proposed drew, and how it ended.
    #[blaze::pure]
    #[blaze::nonblocking]
    pub fn ran(&mut self, record: &Record, ended: Ended) {
        match self.stage {
            Stage::Given => self.ran_given(record, ended),
            Stage::Generate => self.ran_case(record, ended),
            Stage::Shrink => {
                let found =
                    self.found.as_ref().expect("a search that shrinks holds its first failure");
                let same = matches!(&ended, Ended::Failed(failure) if failure.origin == found.failure.origin);
                self.shrinker
                    .as_mut()
                    .expect("a search that shrinks holds its shrinker")
                    .ran(record, same);
            }
            Stage::Final => self.ran_final(record, ended),
            Stage::Done => {
                panic!("blaze-sim: `Search::ran` after the search was done: no case was proposed")
            }
        }
    }

    /// The seed of case `index` (from 0) of the run.
    fn seed_of(&self, index: u32) -> u64 {
        self.first_seed.wrapping_add(u64::from(index))
    }

    /// Moves the shrinker on to its next candidate; when it has none, on to the final run.
    fn step_shrink(&mut self) {
        let shrinker = self.shrinker.as_mut().expect("a search that shrinks holds its shrinker");
        if shrinker.step() {
            return;
        }
        self.least.clear();
        self.least.extend_from_slice(shrinker.best());
        #[cfg(test)]
        let skips_final_run = self.plant == Plant::SkipsFinalRun;
        #[cfg(not(test))]
        let skips_final_run = false;
        if skips_final_run {
            let shrink_runs = shrinker.runs();
            let found = self.found.take().expect("a search that shrinks holds its first failure");
            self.done(Verdict::Failed(Failed {
                seed: found.seed,
                case: found.case,
                cases: self.count,
                failure: found.failure,
                first_len: found.first_len,
                least: Record::new(),
                shrink_runs,
            }));
            return;
        }
        self.stage = Stage::Final;
    }

    fn ran_given(&mut self, record: &Record, ended: Ended) {
        if record.clamped > 0 || record.unused > 0 {
            self.done(Verdict::Misfit { clamped: record.clamped, unused: record.unused, ended });
            return;
        }
        self.count_case(record);
        match ended {
            Ended::Failed(failure) => self.done(Verdict::Failed(Failed {
                seed: None,
                case: 1,
                cases: 1,
                failure,
                first_len: choices_of(record),
                least: record.clone(),
                shrink_runs: 0,
            })),
            Ended::Passed => {
                self.passed += 1;
                self.finish(Reach::Waived);
            }
            Ended::Discarded => {
                self.discarded += 1;
                self.finish(Reach::Waived);
            }
        }
    }

    fn ran_case(&mut self, record: &Record, ended: Ended) {
        self.count_case(record);
        match ended {
            Ended::Passed => {
                self.passed += 1;
                for point in &record.reached {
                    if !self.reached.contains(point) {
                        self.reached.push(*point);
                    }
                }
            }
            Ended::Discarded => self.discarded += 1,
            Ended::Failed(failure) => {
                let found = Found {
                    seed: Some(self.seed_of(self.ran - 1)),
                    case: self.ran,
                    failure,
                    first_len: choices_of(record),
                };
                if self.config.shrink_runs == 0 {
                    self.done(Verdict::Failed(Failed {
                        seed: found.seed,
                        case: found.case,
                        cases: self.count,
                        failure: found.failure,
                        first_len: found.first_len,
                        least: record.clone(),
                        shrink_runs: 0,
                    }));
                    return;
                }
                #[cfg(test)]
                let shrinker =
                    Shrinker::planted(record, self.config.shrink_runs, shrinker_plant(self.plant));
                #[cfg(not(test))]
                let shrinker = Shrinker::new(record, self.config.shrink_runs);
                self.found = Some(found);
                self.shrinker = Some(shrinker);
                self.stage = Stage::Shrink;
                return;
            }
        }
        if self.ran == self.count {
            self.finish(Reach::Asked);
        }
    }

    fn ran_final(&mut self, record: &Record, ended: Ended) {
        let found = self.found.take().expect("a search's final run follows its first failure");
        let shrink_runs = self.shrinker.as_ref().map_or(0, Shrinker::runs);
        let verdict = match ended {
            Ended::Failed(failure) if failure.origin == found.failure.origin => {
                #[cfg(test)]
                let least = {
                    let mut least = record.clone();
                    if self.plant == Plant::ReportsProposed {
                        least.choices.truncate(self.least.len());
                    }
                    least
                };
                #[cfg(not(test))]
                let least = record.clone();
                Verdict::Failed(Failed {
                    seed: found.seed,
                    case: found.case,
                    cases: self.count,
                    failure,
                    first_len: found.first_len,
                    least,
                    shrink_runs,
                })
            }
            again @ (Ended::Passed | Ended::Failed(..) | Ended::Discarded) => {
                Verdict::NotAFunction {
                    seed: found.seed,
                    tape: self.least.clone(),
                    first: found.failure,
                    again,
                }
            }
        };
        self.done(verdict);
    }

    /// Counts a case of the run, whatever its end.
    fn count_case(&mut self, record: &Record) {
        self.ran += 1;
        #[expect(
            blaze::as_cast,
            reason = "a record's length, a `usize`, widens exactly to `u64` on every target"
        )]
        let drawn = record.choices.len() as u64;
        self.choices += drawn;
        self.drew |= !record.choices.is_empty();
    }

    /// The verdict of a run with no failure, in this order: nothing drawn, nothing passed, a
    /// point missed (when `reach` asks), passed.
    fn finish(&mut self, reach: Reach) {
        let cases = self.count;
        let verdict = if !self.drew {
            Verdict::DrewNothing { cases }
        } else if self.passed == 0 {
            Verdict::AllDiscarded { cases }
        } else {
            let missing = self.missing(reach);
            if missing.is_empty() {
                Verdict::Passed(Passed { cases, discarded: self.discarded, choices: self.choices })
            } else {
                Verdict::Unreached { cases, missing }
            }
        };
        self.done(verdict);
    }

    /// The points of `Config::must_reach` no passing case reached, each once, in their order;
    /// none when `reach` waives them.
    fn missing(&self, reach: Reach) -> Vec<&'static str, Global> {
        let mut missing = Vec::new_in(Global);
        match reach {
            Reach::Asked => {
                for point in self.config.must_reach {
                    if !self.reached.contains(point) && !missing.contains(point) {
                        missing.push(*point);
                    }
                }
            }
            Reach::Waived => {}
        }
        missing
    }

    fn done(&mut self, verdict: Verdict) {
        self.verdict = Some(verdict);
        self.stage = Stage::Done;
    }
}

/// Whether a run's verdict asks for the points of `Config::must_reach`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Reach {
    /// A run of cases: a point no passing case reached fails it.
    Asked,
    /// One replayed case, which is not a run: the points are not asked of it.
    Waived,
}

/// The choices a record holds; a case draws at most `u32::MAX` (its bound is a `u32`).
#[expect(
    blaze::dropped_error,
    reason = "a case draws at most `Config::most_choices`, a `u32`, so the conversion cannot fail; `u32::MAX` is that bound"
)]
fn choices_of(record: &Record) -> u32 {
    u32::try_from(record.choices.len()).unwrap_or(u32::MAX)
}
