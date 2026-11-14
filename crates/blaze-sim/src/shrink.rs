//! The shrinker: from a failing tape to the simplest tape that still fails the same way
//! (docs/v2/plan/design/sim.md, "The design").
//!
//! A tape is simpler when it is shorter, and of two of one length, when its value at the first
//! difference is smaller ([`simpler`]); trailing zeros do not count, since a calm tail gives
//! them anyway. The [`Shrinker`] is a sans-IO state machine: `next` proposes a candidate, `ran`
//! takes the record its run made and whether it failed the same way. It runs passes in a fixed
//! order, as a round, until a whole round keeps nothing or its budget of runs is spent: cut the
//! tape, delete spans, delete blocks, zero blocks, lower each value, sort neighbours. It keeps
//! the candidate's record, never the candidate as proposed, and never proposes a tape twice.

#![blaze::sans_io]

use core::ops::Range;
use std::alloc::Global;

use blaze_rng::{GAMMA, mix};

#[cfg(test)]
use crate::search::Plant;
use crate::tape::{Record, Span};

/// Whether tape `a` is simpler than `b`: trailing zeros cut from both, the shorter is simpler,
/// and of two of one length the one with the smaller value at the first difference.
#[blaze::pure]
#[blaze::noalloc]
#[blaze::nonblocking]
pub fn simpler(a: &[u64], b: &[u64]) -> bool {
    let (a, b) = (trimmed(a), trimmed(b));
    match a.len().cmp(&b.len()) {
        core::cmp::Ordering::Less => true,
        core::cmp::Ordering::Greater => false,
        core::cmp::Ordering::Equal => a < b,
    }
}

/// `values` with its trailing zeros cut.
fn trimmed(values: &[u64]) -> &[u64] {
    let kept = values.iter().rposition(|&value| value != 0).map_or(0, |last| last + 1);
    &values[..kept]
}

/// The start of a fingerprint's low chain, mixed with the tape's length: the first 64 bits of
/// pi's fraction, a constant with no structure of its own to line up with a tape's values.
const LOW_START: u64 = 0x243f_6a88_85a3_08d3;

/// The start of a fingerprint's high chain, mixed with the low chain's first value: the next
/// 64 bits of pi's fraction, so the two chains start apart.
const HIGH_START: u64 = 0x1319_8a2e_0370_7344;

/// The rotation of the high chain before each value: odd, so over the steps it moves every bit
/// to every position, and far from 0 and 32, so the high chain does not track the low one.
const HIGH_ROTATION: u32 = 23;

/// A tape's 128-bit fingerprint: two SplitMix64 chains over its length and values, so two
/// tapes the shrinker proposed are told apart without keeping them.
fn fingerprint(values: &[u64]) -> u128 {
    #[expect(
        blaze::as_cast,
        reason = "a tape's length, a `usize`, widens exactly to `u64` on every target"
    )]
    let len = values.len() as u64;
    let mut low = mix(len ^ LOW_START);
    let mut high = mix(low ^ HIGH_START);
    for &value in values {
        low = mix(low ^ value).wrapping_add(GAMMA);
        high = mix(high.rotate_left(HIGH_ROTATION) ^ value ^ low);
    }
    (u128::from(high) << u64::BITS) | u128::from(low)
}

/// The block sizes the block passes try at each position, largest first.
const BLOCKS: [usize; 4] = [8, 4, 2, 1];

/// The shrinker's passes, in the order a round runs them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Pass {
    /// The best's first `len / 2`, then `len - 1` values.
    Cut,
    /// Each span of the best, longest first, its values removed.
    DeleteSpans,
    /// At each position from the back, blocks of [`BLOCKS`] values removed.
    DeleteBlocks,
    /// At each position, blocks of [`BLOCKS`] values set to 0.
    ZeroBlocks,
    /// Each nonzero value: 0, then a binary search for the least that still fails, then one less.
    Lower,
    /// Each neighbouring pair with the larger first, swapped.
    Sort,
}

/// Where the lowering of one value is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Lowering {
    /// Try 0.
    Zero,
    /// `lo` is known not to fail the same way, `hi` (the best's value) does: try between.
    Between { lo: u64, hi: u64 },
    /// Try the value less one.
    MinusOne,
}

/// Whether some value lies strictly between `lo` and `hi`, so a binary search between them has
/// a value to try.
const fn has_room(lo: u64, hi: u64) -> bool {
    hi > lo.saturating_add(1)
}

/// Where a round is: the pass, the position in it (from the back for [`Pass::DeleteBlocks`]),
/// the block size, and for [`Pass::Lower`], the step at that position.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Cursor {
    pass: Pass,
    at: usize,
    size: usize,
    lowering: Lowering,
}

impl Cursor {
    const START: Cursor = Cursor::of(Pass::Cut);

    const fn of(pass: Pass) -> Cursor {
        Cursor { pass, at: 0, size: 0, lowering: Lowering::Zero }
    }
}

/// What building the candidate at the cursor gave.
enum Built {
    /// `candidate` holds the tape at the cursor.
    Candidate,
    /// The cursor moved on with no candidate (a block already zero, a pair in order).
    Moved,
    /// The pass has no position left.
    PassOver,
}

/// What one turn of `step`'s loop came to.
enum Turn {
    /// The cursor moved on: take another turn.
    Again,
    /// A candidate is ready in `candidate`, and proposed.
    Ready,
    /// The shrinker is done.
    Done,
}

/// How a candidate's run ended, against the failure being shrunk.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Fails {
    /// It failed the same way: its record may become the best.
    TheSameWay,
    /// It passed, or failed another way.
    Otherwise,
}

/// What became of the candidate at the cursor.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Fate {
    /// Its record became the best.
    Kept,
    /// It ran and was not kept, or was passed over unrun.
    Dropped,
}

/// The shrinker of one failing case. `next` proposes, `ran` takes the result; it runs nothing.
pub struct Shrinker {
    /// The simplest failing tape so far, as its run recorded it, trailing zeros cut.
    best: Vec<u64, Global>,
    /// The best's spans, longest first.
    spans: Vec<Span, Global>,
    candidate: Vec<u64, Global>,
    /// The last run's values, swapped into `best` when it is kept.
    values: Vec<u64, Global>,
    /// Fingerprints of every tape proposed (and of each best), sorted. A sorted `Vec`, since
    /// `BTreeSet::new_in` needs the unstable `btreemap_alloc`.
    seen: Vec<u128, Global>,
    budget: u32,
    runs: u32,
    cursor: Cursor,
    /// Whether this round kept a candidate, so another round is worth running.
    kept_in_round: bool,
    /// A candidate was proposed and its run has not come back.
    waiting: bool,
    done: bool,
    #[cfg(test)]
    plant: Plant,
}

impl Shrinker {
    /// From the failing case's record (its values and spans), with a budget of runs.
    #[blaze::pure]
    #[blaze::nonblocking]
    pub fn new(record: &Record, runs: u32) -> Shrinker {
        let mut shrinker = Shrinker {
            best: Vec::new_in(Global),
            spans: Vec::new_in(Global),
            candidate: Vec::new_in(Global),
            values: Vec::new_in(Global),
            seen: Vec::new_in(Global),
            budget: runs,
            runs: 0,
            cursor: Cursor::START,
            kept_in_round: false,
            waiting: false,
            done: false,
            #[cfg(test)]
            plant: Plant::None,
        };
        record.values_into(&mut shrinker.best);
        shrinker.take_spans(record);
        let first = fingerprint(&shrinker.best);
        shrinker.remember(first);
        shrinker
    }

    /// `new` with a bug planted, for a control.
    #[cfg(test)]
    pub(crate) fn planted(record: &Record, runs: u32, plant: Plant) -> Shrinker {
        let mut shrinker = Shrinker::new(record, runs);
        shrinker.plant = plant;
        shrinker
    }

    /// The next candidate, or `None` when a whole round of passes kept nothing or the budget is
    /// spent. Never proposes a tape it proposed before, nor one that is not simpler than the best.
    /// Called again before `ran`, it gives the same candidate.
    #[blaze::pure]
    #[blaze::nonblocking]
    pub fn next(&mut self) -> Option<&[u64]> {
        if self.step() { Some(&self.candidate) } else { None }
    }

    /// The candidate's run. It becomes the best when `same` and its recorded values are simpler
    /// than the best's; its spans replace the best's.
    #[blaze::pure]
    #[blaze::nonblocking]
    #[expect(
        blaze::bool_param,
        reason = "public API of the crate, kept as it is: `same` is the caller's comparison of failure origins, and it becomes `Fails` at once"
    )]
    pub fn ran(&mut self, record: &Record, same: bool) {
        assert!(
            self.waiting,
            "blaze-sim: `Shrinker::ran` with no candidate proposed: `next` proposes the case `ran` takes"
        );
        self.waiting = false;
        self.runs += 1;
        record.values_into(&mut self.values);
        let fails = if same { Fails::TheSameWay } else { Fails::Otherwise };
        let fate = self.keeps(fails);
        if fate == Fate::Kept {
            core::mem::swap(&mut self.best, &mut self.values);
            self.take_spans(record);
            self.kept_in_round = true;
            let best = fingerprint(&self.best);
            self.remember(best);
        }
        self.move_on(fate);
    }

    /// The simplest failing tape so far, as recorded, trailing zeros cut.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn best(&self) -> &[u64] {
        &self.best
    }

    /// The runs spent.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn runs(&self) -> u32 {
        self.runs
    }

    /// The candidate `step` made ready.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn candidate(&self) -> &[u64] {
        &self.candidate
    }

    /// Makes the next candidate ready in `candidate`; false when the shrinker is done. Bounded:
    /// every turn of its loop moves the cursor on or starts the one new round a call may start,
    /// so two rounds' positions bound it.
    #[blaze::pure]
    #[blaze::nonblocking]
    pub(crate) fn step(&mut self) -> bool {
        if self.waiting {
            return true;
        }
        if self.done {
            return false;
        }
        let bound = 2 * round_bound(self.best.len(), self.spans.len()) + STEP_MARGIN;
        for _ in 0..bound {
            match self.turn() {
                Turn::Again => {}
                Turn::Ready => return true,
                Turn::Done => return false,
            }
        }
        panic!(
            "blaze-sim: the shrinker found no candidate and no end in {bound} turns over a tape of {} values: its passes did not end",
            self.best.len()
        )
    }

    /// One turn of `step`: the budget, then the candidate at the cursor.
    fn turn(&mut self) -> Turn {
        if self.runs >= self.budget {
            self.done = true;
            return Turn::Done;
        }
        match self.build() {
            Built::Moved => Turn::Again,
            Built::PassOver => self.pass_over(),
            Built::Candidate => self.offer(),
        }
    }

    /// On to the next pass; after the last, a new round if this one kept a candidate, else done.
    fn pass_over(&mut self) -> Turn {
        if self.next_pass() {
            return Turn::Again;
        }
        if !self.kept_in_round {
            self.done = true;
            return Turn::Done;
        }
        self.kept_in_round = false;
        self.cursor = Cursor::START;
        Turn::Again
    }

    /// Proposes the built candidate, trailing zeros cut, when it is simpler than the best and
    /// was not proposed before; else drops it and moves on.
    fn offer(&mut self) -> Turn {
        let kept = trimmed(&self.candidate).len();
        self.candidate.truncate(kept);
        if !self.proposes() {
            self.move_on(Fate::Dropped);
            return Turn::Again;
        }
        let print = fingerprint(&self.candidate);
        if !self.remember(print) {
            // Proposed before, and not kept then: it does not fail the same way.
            self.move_on(Fate::Dropped);
            return Turn::Again;
        }
        self.waiting = true;
        Turn::Ready
    }

    /// Whether the candidate may be proposed: it is simpler than the best.
    fn proposes(&self) -> bool {
        simpler(&self.candidate, &self.best)
    }

    /// Whether the run in `values` is kept: it failed the same way and is simpler than the best.
    fn keeps(&self, fails: Fails) -> Fate {
        let simpler = simpler(&self.values, &self.best);
        #[cfg(test)]
        let (fails, simpler) = match self.plant {
            Plant::KeepsPassing => (Fails::TheSameWay, simpler),
            Plant::KeepsNotSimpler => (fails, true),
            Plant::None | Plant::ReportsProposed | Plant::SkipsFinalRun => (fails, simpler),
        };
        match fails {
            Fails::TheSameWay if simpler => Fate::Kept,
            Fails::TheSameWay | Fails::Otherwise => Fate::Dropped,
        }
    }

    /// Adds a fingerprint; false when it was there already.
    fn remember(&mut self, print: u128) -> bool {
        match self.seen.binary_search(&print) {
            Ok(_) => false,
            Err(at) => {
                self.seen.insert(at, print);
                true
            }
        }
    }

    /// The record's spans as the best's, longest first, then by start.
    fn take_spans(&mut self, record: &Record) {
        self.spans.clear();
        self.spans.extend_from_slice(&record.spans);
        self.spans.sort_unstable_by(|a, b| {
            let (a_len, b_len) = (a.end.saturating_sub(a.start), b.end.saturating_sub(b.start));
            b_len.cmp(&a_len).then(a.start.cmp(&b.start)).then(a.end.cmp(&b.end))
        });
    }

    /// The candidate at the cursor, into `candidate`.
    fn build(&mut self) -> Built {
        let Cursor { pass, at, size, lowering } = self.cursor;
        match pass {
            Pass::Cut => self.build_cut(at),
            Pass::DeleteSpans => self.build_delete_span(at),
            Pass::DeleteBlocks => self.build_delete_block(at, size),
            Pass::ZeroBlocks => self.build_zero_block(at, size),
            Pass::Lower => self.build_lower(at, lowering),
            Pass::Sort => self.build_sort(at),
        }
    }

    /// [`Pass::Cut`]: the best's first half at position 0, all but its last value at 1.
    fn build_cut(&mut self, at: usize) -> Built {
        let len = self.best.len();
        let keep = match at {
            0 => len / 2,
            1 => len.saturating_sub(1),
            _ => return Built::PassOver,
        };
        self.candidate.clear();
        self.candidate.extend_from_slice(&self.best[..keep]);
        Built::Candidate
    }

    /// [`Pass::DeleteSpans`]: the best with span `at` removed, cut to the best's length.
    fn build_delete_span(&mut self, at: usize) -> Built {
        let len = self.best.len();
        let Some(span) = self.spans.get(at) else { return Built::PassOver };
        #[expect(
            blaze::as_cast,
            reason = "a span's bounds are `u32` positions in a tape, and a `usize` holds every `u32` on every target"
        )]
        let (start, end) = ((span.start as usize).min(len), (span.end as usize).min(len));
        if start >= end {
            self.move_on(Fate::Dropped);
            return Built::Moved;
        }
        self.remove(start..end);
        Built::Candidate
    }

    /// [`Pass::DeleteBlocks`]: the best with the block of size `BLOCKS[size]` that ends `at`
    /// values from its back removed.
    fn build_delete_block(&mut self, at: usize, size: usize) -> Built {
        let len = self.best.len();
        if at >= len {
            return Built::PassOver;
        }
        let end = len - at;
        let start = end.saturating_sub(BLOCKS[size]);
        self.remove(start..end);
        Built::Candidate
    }

    /// [`Pass::ZeroBlocks`]: the best with the block of size `BLOCKS[size]` at `at` set to 0,
    /// unless it is 0 already.
    fn build_zero_block(&mut self, at: usize, size: usize) -> Built {
        let len = self.best.len();
        if at >= len {
            return Built::PassOver;
        }
        let end = (at + BLOCKS[size]).min(len);
        if self.best[at..end].iter().all(|&value| value == 0) {
            self.move_on(Fate::Dropped);
            return Built::Moved;
        }
        self.candidate.clear();
        self.candidate.extend_from_slice(&self.best);
        self.candidate[at..end].fill(0);
        Built::Candidate
    }

    /// [`Pass::Lower`]: the best with its value at `at` lowered as `lowering` says.
    fn build_lower(&mut self, at: usize, lowering: Lowering) -> Built {
        let Some(&value) = self.best.get(at) else { return Built::PassOver };
        if value == 0 {
            self.cursor.at += 1;
            self.cursor.lowering = Lowering::Zero;
            return Built::Moved;
        }
        let to = match lowering {
            Lowering::Zero => 0,
            Lowering::Between { lo, hi } => {
                if !has_room(lo, hi) {
                    self.cursor.lowering = Lowering::MinusOne;
                    return Built::Moved;
                }
                lo + (hi - lo) / 2
            }
            Lowering::MinusOne => value - 1,
        };
        self.candidate.clear();
        self.candidate.extend_from_slice(&self.best);
        self.candidate[at] = to;
        Built::Candidate
    }

    /// [`Pass::Sort`]: the best with the pair at `at` swapped, when its larger value is first.
    fn build_sort(&mut self, at: usize) -> Built {
        let (Some(&left), Some(&right)) = (self.best.get(at), self.best.get(at + 1)) else {
            return Built::PassOver;
        };
        if left <= right {
            self.move_on(Fate::Dropped);
            return Built::Moved;
        }
        self.candidate.clear();
        self.candidate.extend_from_slice(&self.best);
        self.candidate.swap(at, at + 1);
        Built::Candidate
    }

    /// The best with `cut` removed, into `candidate`.
    fn remove(&mut self, cut: Range<usize>) {
        self.candidate.clear();
        self.candidate.extend_from_slice(&self.best[..cut.start]);
        self.candidate.extend_from_slice(&self.best[cut.end..]);
    }

    /// The cursor after the candidate at it ran (or was passed over, as dropped). A kept
    /// candidate leaves the cursor where it was, so the pass goes on from the same position
    /// against the new best; one dropped moves it to the next.
    fn move_on(&mut self, fate: Fate) {
        match self.cursor.pass {
            Pass::Cut | Pass::DeleteSpans | Pass::Sort => match fate {
                Fate::Kept => {}
                Fate::Dropped => self.cursor.at += 1,
            },
            Pass::DeleteBlocks | Pass::ZeroBlocks => match fate {
                Fate::Kept => {}
                Fate::Dropped => {
                    self.cursor.size += 1;
                    if self.cursor.size == BLOCKS.len() {
                        self.cursor.size = 0;
                        self.cursor.at += 1;
                    }
                }
            },
            Pass::Lower => self.lowered(fate),
        }
    }

    /// The lowering of the value at the cursor after a try.
    fn lowered(&mut self, fate: Fate) {
        let at = self.cursor.at;
        let next = match self.cursor.lowering {
            Lowering::Zero => match fate {
                Fate::Kept => None,
                Fate::Dropped => match self.best.get(at) {
                    Some(&value) if has_room(0, value) => {
                        Some(Lowering::Between { lo: 0, hi: value })
                    }
                    _ => None,
                },
            },
            Lowering::Between { lo, hi } => {
                let (lo, hi) = match fate {
                    // The best now holds what the run recorded there, at most the try.
                    Fate::Kept => (lo, self.best.get(at).copied().unwrap_or(0)),
                    Fate::Dropped => (lo + (hi - lo) / 2, hi),
                };
                match has_room(lo, hi) {
                    false => Some(Lowering::MinusOne),
                    true => Some(Lowering::Between { lo, hi }),
                }
            }
            Lowering::MinusOne => None,
        };
        match next {
            Some(lowering @ (Lowering::Zero | Lowering::Between { .. } | Lowering::MinusOne)) => {
                self.cursor.lowering = lowering;
            }
            None => {
                self.cursor.at += 1;
                self.cursor.lowering = Lowering::Zero;
            }
        }
    }

    /// On to the next pass of the round; false after the last.
    fn next_pass(&mut self) -> bool {
        let next = match self.cursor.pass {
            Pass::Cut => Pass::DeleteSpans,
            Pass::DeleteSpans => Pass::DeleteBlocks,
            Pass::DeleteBlocks => Pass::ZeroBlocks,
            Pass::ZeroBlocks => Pass::Lower,
            Pass::Lower => Pass::Sort,
            Pass::Sort => return false,
        };
        self.cursor = Cursor::of(next);
        true
    }
}

/// Turns a call of `step` may take past two rounds' bound: a margin over [`round_bound`]'s
/// count, so that count being exact is not what keeps `step` from its panic.
const STEP_MARGIN: u64 = 4;

/// The turns of [`Pass::Cut`]: its two tries (half the tape, all but its last value) and its
/// pass-over.
const CUT_TURNS: u64 = 3;

/// The most turns [`Pass::Lower`] takes at one position: the try of 0, at most 64 halvings of
/// the range (one per bit of a `u64`), the move to `Lowering::MinusOne` and the try of one
/// less: 67, and one to spare.
const LOWER_TURNS: u64 = 68;

/// One pass-over for each of a round's six passes (the variants of [`Pass`]).
const PASS_OVERS: u64 = 6;

/// The most turns one round of passes takes over a tape of `len` values with `spans` spans:
/// each position's tries and moves in each pass, and a pass-over per pass.
#[expect(
    blaze::as_cast,
    reason = "lengths and counts, `usize`s, widen exactly to `u64` on every target"
)]
fn round_bound(len: usize, spans: usize) -> u64 {
    let positions = len as u64 + 1;
    let block_sizes = BLOCKS.len() as u64;
    // Spans: a try or a move each. Blocks, twice: each size a position. Sort: a try or a move a
    // position.
    CUT_TURNS
        + 2 * spans as u64
        + 2 * block_sizes * positions
        + LOWER_TURNS * positions
        + positions
        + PASS_OVERS
}
