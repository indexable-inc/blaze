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

use std::alloc::Global;

use blaze_rng::mix;

#[cfg(test)]
use crate::search::Plant;
use crate::tape::{Record, Span};

/// Whether tape `a` is simpler than `b`: trailing zeros cut from both, the shorter is simpler,
/// and of two of one length the one with the smaller value at the first difference.
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

/// A tape's 128-bit fingerprint: two SplitMix64 chains over its length and values, so two
/// tapes the shrinker proposed are told apart without keeping them.
fn fingerprint(values: &[u64]) -> u128 {
    let mut low = mix(values.len() as u64 ^ 0x243f_6a88_85a3_08d3);
    let mut high = mix(low ^ 0x1319_8a2e_0370_7344);
    for &value in values {
        low = mix(low ^ value).wrapping_add(0x9e37_79b9_7f4a_7c15);
        high = mix(high.rotate_left(23) ^ value ^ low);
    }
    (u128::from(high) << 64) | u128::from(low)
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

/// Where a round is: the pass, the position in it (from the back for `DeleteBlocks`), the
/// block size, and for `Lower`, the step at that position.
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
    pub fn next(&mut self) -> Option<&[u64]> {
        if self.step() { Some(&self.candidate) } else { None }
    }

    /// The candidate's run. It becomes the best when `same` and its recorded values are simpler
    /// than the best's; its spans replace the best's.
    pub fn ran(&mut self, record: &Record, same: bool) {
        assert!(self.waiting, "blaze-sim: `Shrinker::ran` with no candidate proposed: `next` proposes the case `ran` takes");
        self.waiting = false;
        self.runs += 1;
        record.values_into(&mut self.values);
        let kept = self.keeps(same);
        if kept {
            core::mem::swap(&mut self.best, &mut self.values);
            self.take_spans(record);
            self.kept_in_round = true;
            let best = fingerprint(&self.best);
            self.remember(best);
        }
        self.move_on(kept);
    }

    /// The simplest failing tape so far, as recorded, trailing zeros cut.
    pub fn best(&self) -> &[u64] {
        &self.best
    }

    /// The runs spent.
    pub fn runs(&self) -> u32 {
        self.runs
    }

    /// The candidate `step` made ready.
    pub(crate) fn candidate(&self) -> &[u64] {
        &self.candidate
    }

    /// Makes the next candidate ready in `candidate`; false when the shrinker is done. Bounded:
    /// every turn of its loop moves the cursor on or starts the one new round a call may start,
    /// so two rounds' positions bound it.
    pub(crate) fn step(&mut self) -> bool {
        if self.waiting {
            return true;
        }
        if self.done {
            return false;
        }
        let bound = 2 * round_bound(self.best.len(), self.spans.len()) + 4;
        for _ in 0..bound {
            if self.runs >= self.budget {
                self.done = true;
                return false;
            }
            match self.build() {
                Built::Moved => {}
                Built::PassOver => {
                    if !self.next_pass() {
                        if !self.kept_in_round {
                            self.done = true;
                            return false;
                        }
                        self.kept_in_round = false;
                        self.cursor = Cursor::START;
                    }
                }
                Built::Candidate => {
                    let kept = trimmed(&self.candidate).len();
                    self.candidate.truncate(kept);
                    if !self.proposes() {
                        self.move_on(false);
                        continue;
                    }
                    let print = fingerprint(&self.candidate);
                    if !self.remember(print) {
                        // Proposed before, and not kept then: it does not fail the same way.
                        self.move_on(false);
                        continue;
                    }
                    self.waiting = true;
                    return true;
                }
            }
        }
        panic!(
            "blaze-sim: the shrinker found no candidate and no end in {bound} turns over a tape of {} values: its passes did not end",
            self.best.len()
        )
    }

    /// Whether the candidate may be proposed: it is simpler than the best.
    fn proposes(&self) -> bool {
        simpler(&self.candidate, &self.best)
    }

    /// Whether the run in `values` is kept: it failed the same way and is simpler than the best.
    fn keeps(&self, same: bool) -> bool {
        let simpler = simpler(&self.values, &self.best);
        #[cfg(test)]
        let (same, simpler) = match self.plant {
            Plant::KeepsPassing => (true, simpler),
            Plant::KeepsNotSimpler => (same, true),
            _ => (same, simpler),
        };
        same && simpler
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
        let len = self.best.len();
        let Cursor { pass, at, size, lowering } = self.cursor;
        match pass {
            Pass::Cut => {
                let keep = match at {
                    0 => len / 2,
                    1 => len.saturating_sub(1),
                    _ => return Built::PassOver,
                };
                self.candidate.clear();
                self.candidate.extend_from_slice(&self.best[..keep]);
                Built::Candidate
            }
            Pass::DeleteSpans => {
                let Some(span) = self.spans.get(at) else { return Built::PassOver };
                let start = (span.start as usize).min(len);
                let end = (span.end as usize).min(len);
                if start >= end {
                    self.move_on(false);
                    return Built::Moved;
                }
                self.remove(start, end);
                Built::Candidate
            }
            Pass::DeleteBlocks => {
                if at >= len {
                    return Built::PassOver;
                }
                let end = len - at;
                let start = end.saturating_sub(BLOCKS[size]);
                self.remove(start, end);
                Built::Candidate
            }
            Pass::ZeroBlocks => {
                if at >= len {
                    return Built::PassOver;
                }
                let end = (at + BLOCKS[size]).min(len);
                if self.best[at..end].iter().all(|&value| value == 0) {
                    self.move_on(false);
                    return Built::Moved;
                }
                self.candidate.clear();
                self.candidate.extend_from_slice(&self.best);
                self.candidate[at..end].fill(0);
                Built::Candidate
            }
            Pass::Lower => {
                let Some(&value) = self.best.get(at) else { return Built::PassOver };
                if value == 0 {
                    self.cursor.at += 1;
                    self.cursor.lowering = Lowering::Zero;
                    return Built::Moved;
                }
                let to = match lowering {
                    Lowering::Zero => 0,
                    Lowering::Between { lo, hi } => {
                        if hi <= lo.saturating_add(1) {
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
            Pass::Sort => {
                if at + 1 >= len {
                    return Built::PassOver;
                }
                if self.best[at] <= self.best[at + 1] {
                    self.move_on(false);
                    return Built::Moved;
                }
                self.candidate.clear();
                self.candidate.extend_from_slice(&self.best);
                self.candidate.swap(at, at + 1);
                Built::Candidate
            }
        }
    }

    /// The best with `start..end` removed, into `candidate`.
    fn remove(&mut self, start: usize, end: usize) {
        self.candidate.clear();
        self.candidate.extend_from_slice(&self.best[..start]);
        self.candidate.extend_from_slice(&self.best[end..]);
    }

    /// The cursor after the candidate at it ran (or was passed over, as not kept). A kept
    /// candidate leaves the cursor where it was, so the pass goes on from the same position
    /// against the new best; one not kept moves it to the next.
    fn move_on(&mut self, kept: bool) {
        match self.cursor.pass {
            Pass::Cut | Pass::DeleteSpans | Pass::Sort => {
                if !kept {
                    self.cursor.at += 1;
                }
            }
            Pass::DeleteBlocks | Pass::ZeroBlocks => {
                if !kept {
                    self.cursor.size += 1;
                    if self.cursor.size == BLOCKS.len() {
                        self.cursor.size = 0;
                        self.cursor.at += 1;
                    }
                }
            }
            Pass::Lower => self.lowered(kept),
        }
    }

    /// The lowering of the value at the cursor after a try.
    fn lowered(&mut self, kept: bool) {
        let at = self.cursor.at;
        let next = match self.cursor.lowering {
            Lowering::Zero if kept => None,
            Lowering::Zero => match self.best.get(at) {
                Some(&value) if value >= 2 => Some(Lowering::Between { lo: 0, hi: value }),
                _ => None,
            },
            Lowering::Between { lo, hi } => {
                let (lo, hi) = match kept {
                    // The best now holds what the run recorded there, at most the try.
                    true => (lo, self.best.get(at).copied().unwrap_or(0)),
                    false => (lo + (hi - lo) / 2, hi),
                };
                match hi <= lo.saturating_add(1) {
                    true => Some(Lowering::MinusOne),
                    false => Some(Lowering::Between { lo, hi }),
                }
            }
            Lowering::MinusOne => None,
        };
        match next {
            Some(lowering) => self.cursor.lowering = lowering,
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

/// The most turns one round of passes takes over a tape of `len` values with `spans` spans:
/// each position's tries and moves in each pass, and a pass-over per pass.
fn round_bound(len: usize, spans: usize) -> u64 {
    let positions = len as u64 + 1;
    // Cut: 2 tries. Spans: a try or a move each. Blocks, twice: 4 sizes a position. Lower: 0,
    // at most 64 halvings and a move, and one less. Sort: a try or a move a position.
    3 + 2 * spans as u64 + 2 * 4 * positions + 68 * positions + positions + 6
}
