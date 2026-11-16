//! The guided phase: a search that keeps the tapes that reached something new and draws the next
//! cases as mutations of them (docs/v2/plan/design/test.md, "### A", "Coverage"; piece 8 of
//! "### G. Pieces").
//!
//! A case's COVERAGE is an input here, never measured: a bitmap of
//! [`GuidedConfig::coverage_words`] words, bit `i` of word `w` set when the case entered point
//! `64 * w + i` (a function at function grain, a counter at branch grain). Who numbers the points
//! is the caller's business: blaze-launch-rt's `mark` bitmap under `blaze test`, a test's own
//! marks in this crate's tests. Every case of one phase reports the same points.
//!
//! A case whose coverage sets a bit no earlier case set keeps its tape in the CORPUS (at most
//! [`GuidedConfig::corpus_tapes`] tapes, the oldest replaced when full; each cut to
//! [`GuidedConfig::tape_choices`] choices, its trailing calm choices dropped). The next case is
//! FRESH (an empty prefix on a seeded tail: always while the corpus is empty, then one case in
//! [`GuidedConfig::fresh_one_in`]) or a MUTATION of a corpus tape: the newest tape half the time,
//! else any; at its last choice half the time, else any; one of
//!
//! - set: a new value within the choice's bound (weight 2),
//! - cut: the tape up to the choice, the rest fresh (weight 2),
//! - splice: the tape up to the choice, another corpus tape's choices from it (weight 1),
//! - raise and lower: by 1 to 16, within the bound (weight 1 each),
//!
//! and every mutated case runs on past its prefix on a seeded tail. Every decision is drawn from
//! one `SplitMix64` of [`GuidedConfig::seed`] in a fixed order, so the same seed and the same
//! coverage answers give the same tapes on any machine (rule 75: the randomness is passed in).
//!
//! On the first failure the phase hands the failing record to [`Search`]'s shrinker (through
//! `Search::after_failure`): the verdict is that search's, the least tape replayed once on a calm
//! tail, a [`Verdict`] like any check's. A run with no failure ends on `Verdict::Passed` (or
//! `DrewNothing`, `AllDiscarded`); `Config::must_reach` belongs to the plain phase and is not
//! asked here.
//!
//! Memory is the caller's: one slice of words, [`GuidedConfig::memory_words`] long, holds the
//! corpus, the coverage seen and the tape being proposed; nothing here allocates (the shrink
//! after a failure is search.rs's). The corpus's bytes are a form whose first word is
//! [`CORPUS_FORM`], its version: tapes only, values only, since point numbers and bounds move with
//! every edit while any tape stays a valid case. A loaded corpus is replayed first, each tape once
//! on a calm tail, its coverage rebuilding what was seen and its record replacing the stored tape.

#![blaze::sans_io]

use blaze_rng::SplitMix64;

#[cfg(test)]
use crate::search::Phase;
use crate::search::{Config, Ended, Next, Passed, Proposal, Search, Verdict};
use crate::tape::{Record, Tail, Tape};

#[cfg(test)]
mod tests;

/// The first word of a corpus's bytes: the form and its version (the last byte, 1). Bytes that
/// start with any other word are refused by name ([`CorpusError::Form`]), never read.
pub const CORPUS_FORM: u64 = u64::from_le_bytes(*b"bzsimcp\x01");

/// The most a raise or a lower moves a value.
const STEP: u64 = 16;

/// The sum of the mutations' weights: set 2, cut 2, splice 1, raise 1, lower 1.
const WEIGHTS: u64 = 7;

/// What a guided phase runs and how much it keeps.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuidedConfig {
    /// Cases to run, the replays of a loaded corpus among them.
    pub cases: u32,
    /// The seed of every decision the phase draws.
    pub seed: u64,
    /// Runs the shrinker may spend on a failure; 0 reports it as found.
    pub shrink_runs: u32,
    pub most_choices: u32,
    /// The most tapes the corpus keeps (rule 18); test.md's corpus record holds 1,024.
    pub corpus_tapes: u32,
    /// The most choices a kept tape holds; a longer one is cut.
    pub tape_choices: u32,
    /// Words of coverage every case reports.
    pub coverage_words: u32,
    /// Once the corpus holds a tape, one case in this many is fresh; 0: none is.
    pub fresh_one_in: u32,
}

impl GuidedConfig {
    /// 10,000 cases from seed 0, 1,000 shrink runs, `Tape::MOST_CHOICES`, a corpus of 1,024
    /// tapes of 256 choices, one fresh case in 16, and `coverage_words` words of coverage.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn new(coverage_words: u32) -> GuidedConfig {
        GuidedConfig {
            cases: 10_000,
            seed: 0,
            shrink_runs: 1000,
            most_choices: Tape::MOST_CHOICES,
            corpus_tapes: 1024,
            tape_choices: 256,
            coverage_words,
            fresh_one_in: 16,
        }
    }

    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn cases(mut self, cases: u32) -> GuidedConfig {
        self.cases = cases;
        self
    }

    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn seed(mut self, seed: u64) -> GuidedConfig {
        self.seed = seed;
        self
    }

    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn shrink_runs(mut self, runs: u32) -> GuidedConfig {
        self.shrink_runs = runs;
        self
    }

    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn most_choices(mut self, most: u32) -> GuidedConfig {
        self.most_choices = most;
        self
    }

    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn corpus_tapes(mut self, tapes: u32) -> GuidedConfig {
        self.corpus_tapes = tapes;
        self
    }

    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn tape_choices(mut self, choices: u32) -> GuidedConfig {
        self.tape_choices = choices;
        self
    }

    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn fresh_one_in(mut self, n: u32) -> GuidedConfig {
        self.fresh_one_in = n;
        self
    }

    /// The words of memory a phase of this config needs: a slot per corpus tape (its length,
    /// then a value and a bound per choice), the coverage seen, and the tape being proposed.
    #[blaze::pure]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn memory_words(&self) -> Result<usize, GuidedError> {
        Layout::of(self).map(|layout| layout.total)
    }
}

/// Where each part of a phase's memory starts.
#[derive(Clone, Copy, Debug)]
struct Layout {
    /// Words per corpus slot: 1 + 2 * `tape_choices`.
    slot_words: usize,
    seen_at: usize,
    scratch_at: usize,
    total: usize,
}

impl Layout {
    fn of(config: &GuidedConfig) -> Result<Layout, GuidedError> {
        for (bound, value) in [
            ("corpus_tapes", config.corpus_tapes),
            ("tape_choices", config.tape_choices),
            ("coverage_words", config.coverage_words),
        ] {
            if value == 0 {
                return Err(GuidedError::Zero { bound });
            }
        }
        let too_large = GuidedError::TooLarge {
            tapes: config.corpus_tapes,
            choices: config.tape_choices,
            coverage_words: config.coverage_words,
        };
        let slot_words = words(config.tape_choices)
            .checked_mul(2)
            .and_then(|pairs| pairs.checked_add(1))
            .ok_or(too_large)?;
        let seen_at = slot_words.checked_mul(words(config.corpus_tapes)).ok_or(too_large)?;
        let scratch_at = seen_at.checked_add(words(config.coverage_words)).ok_or(too_large)?;
        let total = scratch_at.checked_add(words(config.tape_choices)).ok_or(too_large)?;
        Ok(Layout { slot_words, seen_at, scratch_at, total })
    }
}

/// What the guided phase refuses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuidedError {
    /// A bound of the config is 0 where the phase needs at least one.
    Zero { bound: &'static str },
    /// The config's memory does not fit the address space.
    TooLarge { tapes: u32, choices: u32, coverage_words: u32 },
    /// The memory given is shorter than `GuidedConfig::memory_words`.
    Memory { needs: usize, has: usize },
    /// A case's coverage is not `GuidedConfig::coverage_words` long.
    CoverageWords { expected: u32, got: usize },
    /// `ran` with no case proposed by `next` since the last `ran`.
    NotProposed,
    /// `ran` after the phase was done.
    RanWhenDone,
    /// The corpus given to `Guided::with_corpus`.
    Corpus(CorpusError),
}

impl core::fmt::Display for GuidedError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            GuidedError::Zero { bound } => write!(
                f,
                "blaze-sim guided phase: `GuidedConfig::{bound}` is 0, and the phase needs at least one (rule 18: every table has a cap, and a cap of 0 keeps nothing)"
            ),
            GuidedError::TooLarge { tapes, choices, coverage_words } => write!(
                f,
                "blaze-sim guided phase: a corpus of {tapes} tapes of {choices} choices with {coverage_words} words of coverage does not fit this machine's address space (rule 18)"
            ),
            GuidedError::Memory { needs, has } => write!(
                f,
                "blaze-sim guided phase: the memory given holds {has} words, and the corpus, the coverage seen and the proposed tape need {needs} (`GuidedConfig::memory_words`)"
            ),
            GuidedError::CoverageWords { expected, got } => write!(
                f,
                "blaze-sim guided phase: a case's coverage is {got} words, and this phase counts {expected} (`GuidedConfig::coverage_words`): every case reports the same points"
            ),
            GuidedError::NotProposed => write!(
                f,
                "blaze-sim guided phase: `Guided::ran` with no case proposed: call `next` first"
            ),
            GuidedError::RanWhenDone => write!(
                f,
                "blaze-sim guided phase: `Guided::ran` after the phase was done: no case was proposed"
            ),
            GuidedError::Corpus(error) => {
                write!(f, "blaze-sim guided phase: the corpus given: {error}")
            }
        }
    }
}

impl core::error::Error for GuidedError {}

/// What reading or writing a corpus's bytes refuses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CorpusError {
    /// The bytes are not a whole number of 8-byte words.
    NotWords { len: usize },
    /// A word the form needs, at byte `at`, is past the end, `len`.
    Cut { at: usize, len: usize },
    /// The first word is not [`CORPUS_FORM`]: another form or version.
    Form { found: u64 },
    /// More tapes than the phase keeps.
    TooManyTapes { count: u64, cap: u32 },
    /// A tape longer than the phase keeps.
    TapeTooLong { tape: u32, len: u64, cap: u32 },
    /// Words after the last tape.
    Trailing { bytes: usize },
    /// The buffer given to `Guided::write_corpus` is shorter than `Guided::corpus_bytes`.
    Short { needs: usize, has: usize },
}

impl core::fmt::Display for CorpusError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            CorpusError::NotWords { len } => {
                write!(f, "{len} bytes are not a whole number of 8-byte words")
            }
            CorpusError::Cut { at, len } => write!(
                f,
                "the bytes are cut: the form needs a word at byte {at}, and there are {len} bytes"
            ),
            CorpusError::Form { found } => write!(
                f,
                "its first word is {found:#018x}, not the corpus form {CORPUS_FORM:#018x} (version 1); another form is refused, never read"
            ),
            CorpusError::TooManyTapes { count, cap } => write!(
                f,
                "it holds {count} tapes, and this phase keeps at most {cap} (`GuidedConfig::corpus_tapes`, rule 18)"
            ),
            CorpusError::TapeTooLong { tape, len, cap } => write!(
                f,
                "its tape {tape} holds {len} choices, and this phase keeps at most {cap} a tape (`GuidedConfig::tape_choices`, rule 18)"
            ),
            CorpusError::Trailing { bytes } => {
                write!(f, "{bytes} bytes follow its last tape")
            }
            CorpusError::Short { needs, has } => write!(
                f,
                "the buffer holds {has} bytes, and the corpus's form needs {needs} (`Guided::corpus_bytes`)"
            ),
        }
    }
}

impl core::error::Error for CorpusError {}

/// A bug a control plants in the phase, for a test's oracle to catch (rule dst-not-review).
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plant {
    None,
    /// The phase ignores the coverage signal: it keeps every case's tape, new bits or not.
    IgnoresCoverage,
}

/// Which part of the phase the next case belongs to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Stage {
    /// Replaying the loaded corpus's tape `index`, oldest first.
    Replay { index: u32 },
    /// Fresh and mutated cases.
    Guide,
    /// Shrinking a failure: `Guided::shrink` decides.
    Shrink,
    Done,
}

/// The mutation a case applies to a corpus tape.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mutation {
    Set,
    Cut,
    Splice,
    Raise,
    Lower,
}

impl Mutation {
    /// The mutation of a draw below [`WEIGHTS`].
    fn of(draw: u64) -> Mutation {
        match draw {
            0 | 1 => Mutation::Set,
            2 | 3 => Mutation::Cut,
            4 => Mutation::Splice,
            5 => Mutation::Raise,
            _ => Mutation::Lower,
        }
    }
}

/// The guided phase of a check: `next` proposes a case, `ran` takes how it went and what it
/// covered, until `next` says it is done and gives the verdict. Sans-IO: it runs nothing.
pub struct Guided<'m> {
    config: GuidedConfig,
    layout: Layout,
    /// The caller's: the corpus's slots, the coverage seen, the proposed tape.
    memory: &'m mut [u64],
    rng: SplitMix64,
    /// Tapes the corpus holds, at most `corpus_tapes`.
    kept: u32,
    /// The slot of the oldest kept tape: the corpus is the ring `oldest..oldest + kept`.
    oldest: u32,
    stage: Stage,
    /// Whether the case `next` proposes is in the scratch words and `tail`.
    ready: bool,
    /// The proposed prefix's length.
    len: usize,
    tail: Tail,
    ran: u32,
    passed: u32,
    discarded: u32,
    choices: u64,
    drew: bool,
    /// The shrink of the first failure, from `Stage::Shrink` on.
    shrink: Option<Search>,
    verdict: Option<Verdict>,
    #[cfg(test)]
    plant: Plant,
}

impl<'m> Guided<'m> {
    /// A phase of `config` over `memory` (at least `config.memory_words()` long; its words are
    /// cleared), with an empty corpus.
    #[blaze::pure]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn new(config: GuidedConfig, memory: &'m mut [u64]) -> Result<Guided<'m>, GuidedError> {
        let layout = Layout::of(&config)?;
        let Some(used) = memory.get_mut(..layout.total) else {
            return Err(GuidedError::Memory { needs: layout.total, has: memory.len() });
        };
        used.fill(0);
        let rng = SplitMix64::new(config.seed);
        let mut guided = Guided {
            config,
            layout,
            memory,
            rng,
            kept: 0,
            oldest: 0,
            stage: Stage::Guide,
            ready: false,
            len: 0,
            tail: Tail::Calm,
            ran: 0,
            passed: 0,
            discarded: 0,
            choices: 0,
            drew: false,
            shrink: None,
            verdict: None,
            #[cfg(test)]
            plant: Plant::None,
        };
        if guided.config.cases == 0 {
            guided.finish();
        }
        Ok(guided)
    }

    /// `new` with the corpus `bytes` hold (the form [`Guided::write_corpus`] writes), replayed
    /// first.
    #[blaze::pure]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn with_corpus(
        config: GuidedConfig,
        memory: &'m mut [u64],
        bytes: &[u8],
    ) -> Result<Guided<'m>, GuidedError> {
        let mut guided = Guided::new(config, memory)?;
        guided.load(bytes).map_err(GuidedError::Corpus)?;
        if guided.kept > 0 && guided.stage == Stage::Guide {
            guided.stage = Stage::Replay { index: 0 };
        }
        Ok(guided)
    }

    /// `new` with a bug planted, for a control.
    #[cfg(test)]
    pub(crate) fn planted(
        config: GuidedConfig,
        memory: &'m mut [u64],
        plant: Plant,
    ) -> Result<Guided<'m>, GuidedError> {
        let mut guided = Guided::new(config, memory)?;
        guided.plant = plant;
        Ok(guided)
    }

    /// The most choices a case of this phase may draw.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn most_choices(&self) -> u32 {
        self.config.most_choices
    }

    /// The most cases this phase proposes: its cases, its shrink runs and the final run.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn most_runs(&self) -> u64 {
        u64::from(self.config.cases) + u64::from(self.config.shrink_runs) + 1
    }

    /// Tapes the corpus holds now.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn kept(&self) -> u32 {
        self.kept
    }

    /// The case to run next, or the verdict once the phase is done. Called again before `ran`,
    /// it proposes the same case.
    #[blaze::pure]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn next(&mut self) -> Next<'_> {
        match self.stage {
            Stage::Shrink => {
                return self
                    .shrink
                    .as_mut()
                    .expect("a guided phase that shrinks holds its search")
                    .next();
            }
            Stage::Done => {
                return Next::Done(
                    self.verdict.as_ref().expect("a done guided phase holds its verdict"),
                );
            }
            Stage::Replay { .. } | Stage::Guide => {}
        }
        if !self.ready {
            self.prepare();
            self.ready = true;
        }
        let at = self.layout.scratch_at;
        Next::Run(Proposal {
            #[cfg(test)]
            phase: Phase::Generate,
            prefix: &self.memory[at..at + self.len],
            tail: self.tail.clone(),
        })
    }

    /// What the case `next` proposed drew, how it ended, and what it covered (`coverage_words`
    /// words; read only before a failure: a shrink's cases are judged by their end alone).
    #[blaze::pure]
    #[blaze::nonblocking]
    pub fn ran(
        &mut self,
        record: &Record,
        ended: Ended,
        coverage: &[u64],
    ) -> Result<(), GuidedError> {
        match self.stage {
            Stage::Shrink => {
                self.shrink
                    .as_mut()
                    .expect("a guided phase that shrinks holds its search")
                    .ran(record, ended);
                return Ok(());
            }
            Stage::Done => return Err(GuidedError::RanWhenDone),
            Stage::Replay { .. } | Stage::Guide => {}
        }
        let expected = self.config.coverage_words;
        if coverage.len() != words(expected) {
            return Err(GuidedError::CoverageWords { expected, got: coverage.len() });
        }
        if !self.ready {
            return Err(GuidedError::NotProposed);
        }
        self.ready = false;
        self.ran += 1;
        self.choices += count_of(record.choices.len());
        self.drew |= !record.choices.is_empty();
        match ended {
            Ended::Failed(failure) => {
                let config = Config::cases(self.ran)
                    .shrink_runs(self.config.shrink_runs)
                    .most_choices(self.config.most_choices);
                self.shrink = Some(Search::after_failure(config, record, failure, self.ran));
                self.stage = Stage::Shrink;
                return Ok(());
            }
            Ended::Passed => self.passed += 1,
            Ended::Discarded => self.discarded += 1,
        }
        let new = self.see(coverage);
        match self.stage {
            Stage::Replay { index } => {
                let base = self.slot(index);
                self.write_tape(base, record);
                let next = index + 1;
                self.stage = if next < self.kept { Stage::Replay { index: next } } else { Stage::Guide };
            }
            Stage::Guide => {
                #[cfg(test)]
                let keep = new || self.plant == Plant::IgnoresCoverage;
                #[cfg(not(test))]
                let keep = new;
                if keep {
                    self.keep(record);
                }
            }
            Stage::Shrink | Stage::Done => {}
        }
        if self.ran == self.config.cases {
            self.finish();
        }
        Ok(())
    }

    /// The length of the corpus's bytes: the form word, the count, and per tape its length and
    /// values.
    #[blaze::pure]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn corpus_bytes(&self) -> usize {
        let mut words = 2;
        for index in 0..self.kept {
            words += 1 + self.len_at(self.slot(index));
        }
        words * 8
    }

    /// The corpus as bytes into `out`: [`CORPUS_FORM`], the count of tapes, then each tape,
    /// oldest first, as its length and its values, every word little-endian. Gives the bytes
    /// written.
    #[blaze::pure]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn write_corpus(&self, out: &mut [u8]) -> Result<usize, CorpusError> {
        let needs = self.corpus_bytes();
        if out.len() < needs {
            return Err(CorpusError::Short { needs, has: out.len() });
        }
        let mut at = 0;
        put(out, &mut at, CORPUS_FORM);
        put(out, &mut at, u64::from(self.kept));
        for index in 0..self.kept {
            let base = self.slot(index);
            let len = self.len_at(base);
            put(out, &mut at, count_of(len));
            for choice in 0..len {
                put(out, &mut at, self.memory[base + 1 + 2 * choice]);
            }
        }
        Ok(at)
    }

    /// Reads a corpus's bytes into the slots, each choice's bound unknown (`u64::MAX`) until its
    /// replay records it.
    fn load(&mut self, bytes: &[u8]) -> Result<(), CorpusError> {
        if !bytes.len().is_multiple_of(8) {
            return Err(CorpusError::NotWords { len: bytes.len() });
        }
        let mut reader = Words { bytes, at: 0 };
        let form = reader.take()?;
        if form != CORPUS_FORM {
            return Err(CorpusError::Form { found: form });
        }
        let cap = self.config.corpus_tapes;
        let count = reader.take()?;
        let Some(count) = u32::try_from(count).ok().filter(|&tapes| tapes <= cap) else {
            return Err(CorpusError::TooManyTapes { count, cap });
        };
        let most = self.config.tape_choices;
        for tape in 0..count {
            let len = reader.take()?;
            let Some(len) = u32::try_from(len).ok().filter(|&choices| choices <= most) else {
                return Err(CorpusError::TapeTooLong { tape, len, cap: most });
            };
            let base = self.layout.slot_words * words(tape);
            let len = words(len);
            self.memory[base] = count_of(len);
            for choice in 0..len {
                self.memory[base + 1 + 2 * choice] = reader.take()?;
                self.memory[base + 2 + 2 * choice] = u64::MAX;
            }
        }
        if reader.at != bytes.len() {
            return Err(CorpusError::Trailing { bytes: bytes.len() - reader.at });
        }
        self.kept = count;
        self.oldest = 0;
        Ok(())
    }

    /// Puts the next case in the scratch words and `tail`.
    fn prepare(&mut self) {
        match self.stage {
            Stage::Replay { index } => {
                self.len = self.copy_to_scratch(self.slot(index));
                self.tail = Tail::Calm;
            }
            Stage::Guide => {
                if self.kept == 0 || self.rng.one_in(u64::from(self.config.fresh_one_in)) {
                    self.len = 0;
                    self.tail = Tail::Seeded(SplitMix64::new(self.rng.next_u64()));
                } else {
                    self.mutate();
                }
            }
            Stage::Shrink | Stage::Done => {}
        }
    }

    /// A mutation of a corpus tape into the scratch words, on a seeded tail.
    fn mutate(&mut self) {
        let kept = self.kept;
        let pick = if self.rng.coin() { kept - 1 } else { below(&mut self.rng, kept) };
        let base = self.slot(pick);
        let len = self.copy_to_scratch(base);
        if len == 0 {
            // A calm tape: nothing to change but what follows it.
            self.len = 0;
            self.tail = Tail::Seeded(SplitMix64::new(self.rng.next_u64()));
            return;
        }
        let at = if self.rng.coin() { len - 1 } else { self.rng.index(len) };
        let scratch = self.layout.scratch_at;
        let value = self.memory[scratch + at];
        let most = self.memory[base + 2 + 2 * at];
        let mut new_len = len;
        match Mutation::of(self.rng.below(WEIGHTS)) {
            Mutation::Set => {
                self.memory[scratch + at] = match most.checked_add(1) {
                    Some(bound) => self.rng.below(bound),
                    None => self.rng.next_u64(),
                };
            }
            Mutation::Cut => new_len = at,
            Mutation::Splice => {
                let other_pick = below(&mut self.rng, kept);
                let other = self.slot(other_pick);
                let other_len = self.len_at(other);
                for choice in at..other_len {
                    self.memory[scratch + choice] = self.memory[other + 1 + 2 * choice];
                }
                new_len = at.max(other_len);
            }
            Mutation::Raise => {
                let step = 1 + self.rng.below(STEP);
                self.memory[scratch + at] = value.saturating_add(step).min(most);
            }
            Mutation::Lower => {
                let step = 1 + self.rng.below(STEP);
                self.memory[scratch + at] = value.saturating_sub(step);
            }
        }
        self.len = new_len;
        self.tail = Tail::Seeded(SplitMix64::new(self.rng.next_u64()));
    }

    /// Adds `coverage` to what was seen; whether it set a bit no case set before.
    fn see(&mut self, coverage: &[u64]) -> bool {
        let seen = self.layout.seen_at;
        let mut new = false;
        for (word, &bits) in coverage.iter().enumerate() {
            new |= bits & !self.memory[seen + word] != 0;
            self.memory[seen + word] |= bits;
        }
        new
    }

    /// Keeps `record`'s tape in the corpus: in a free slot, else in the oldest's.
    fn keep(&mut self, record: &Record) {
        let base = if self.kept < self.config.corpus_tapes {
            self.kept += 1;
            self.slot(self.kept - 1)
        } else {
            let base = self.slot(0);
            self.oldest = (self.oldest + 1) % self.config.corpus_tapes;
            base
        };
        self.write_tape(base, record);
    }

    /// `record`'s choices into the slot at `base`: trailing calm choices dropped, then cut to
    /// `tape_choices`.
    fn write_tape(&mut self, base: usize, record: &Record) {
        let drawn =
            record.choices.iter().rposition(|choice| choice.value != 0).map_or(0, |last| last + 1);
        let len = drawn.min(words(self.config.tape_choices));
        self.memory[base] = count_of(len);
        for (choice, drawn) in record.choices[..len].iter().enumerate() {
            self.memory[base + 1 + 2 * choice] = drawn.value;
            self.memory[base + 2 + 2 * choice] = drawn.most;
        }
    }

    /// The values of the slot at `base` into the scratch words; their count.
    fn copy_to_scratch(&mut self, base: usize) -> usize {
        let len = self.len_at(base);
        let scratch = self.layout.scratch_at;
        for choice in 0..len {
            self.memory[scratch + choice] = self.memory[base + 1 + 2 * choice];
        }
        len
    }

    /// The first word of the corpus's tape `index`, oldest first.
    fn slot(&self, index: u32) -> usize {
        let physical =
            (words(self.oldest) + words(index)) % words(self.config.corpus_tapes);
        physical * self.layout.slot_words
    }

    /// The length the slot at `base` holds, which this phase wrote, at most `tape_choices`.
    #[expect(
        blaze::as_cast,
        reason = "a slot's length is one this phase wrote, at most `tape_choices`, a `u32`, and a `usize` holds every `u32` on every target blaze builds for"
    )]
    fn len_at(&self, base: usize) -> usize {
        self.memory[base] as usize
    }

    /// The verdict of a run with no failure: nothing drawn, nothing passed, or passed.
    fn finish(&mut self) {
        let cases = self.ran;
        let verdict = if !self.drew {
            Verdict::DrewNothing { cases }
        } else if self.passed == 0 {
            Verdict::AllDiscarded { cases }
        } else {
            Verdict::Passed(Passed { cases, discarded: self.discarded, choices: self.choices })
        };
        self.verdict = Some(verdict);
        self.stage = Stage::Done;
    }
}

/// A corpus's bytes read a word at a time.
struct Words<'b> {
    bytes: &'b [u8],
    at: usize,
}

impl Words<'_> {
    fn take(&mut self) -> Result<u64, CorpusError> {
        let word = self
            .bytes
            .get(self.at..self.at + 8)
            .and_then(|word| <[u8; 8]>::try_from(word).ok())
            .ok_or(CorpusError::Cut { at: self.at, len: self.bytes.len() })?;
        self.at += 8;
        Ok(u64::from_le_bytes(word))
    }
}

/// `word` little-endian at `out[*at..]`, which the caller sized.
fn put(out: &mut [u8], at: &mut usize, word: u64) {
    out[*at..*at + 8].copy_from_slice(&word.to_le_bytes());
    *at += 8;
}

/// Below `n`, drawn from `rng`.
#[expect(
    blaze::as_cast,
    reason = "a draw below a `u32` is below `u32::MAX`, so it narrows back to `u32` exactly"
)]
fn below(rng: &mut SplitMix64, n: u32) -> u32 {
    rng.below(u64::from(n)) as u32
}

/// A `u32` bound as a count of words.
#[expect(
    blaze::as_cast,
    reason = "a `usize` holds every `u32` on every target blaze builds for"
)]
fn words(n: u32) -> usize {
    n as usize
}

/// A count of words or choices as a `u64`.
#[expect(
    blaze::as_cast,
    reason = "a `usize` widens exactly to `u64` on every target blaze builds for"
)]
fn count_of(len: usize) -> u64 {
    len as u64
}
