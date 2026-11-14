//! The tape: the one source of a case's choices (docs/v2/plan/design/sim.md, "The design").
//!
//! A tape hands out a prefix of given values, then a tail that is calm (zeros) or the stream of
//! a seed. Every draw goes through one private function, `Tape::draw`, which takes the value
//! from the prefix (lowered to fit the draw), else from the tail, holds the bound and records the
//! choice in its canonical form, where 0 is calm. A seeded draw consumes exactly the raw draws
//! of the `SplitMix64` method of the same name and returns what that method returns, so a seeded
//! tape is that stream, and the methods turn a canonical value into their result the same way
//! whichever end it came from, so a recorded tape replays to itself.

#![blaze::sans_io]

use std::alloc::Global;
use std::sync::{Arc, TryLockError};

use blaze_rng::SplitMix64;

/// One choice a case made, in canonical form.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Choice {
    /// In `0..=most`; 0 is the calm choice.
    pub value: u64,
    pub most: u64,
    pub label: &'static str,
}

/// A stretch of a tape drawn for one thing (an element of a list, an operation):
/// `choices[start..end]`, which the shrinker deletes whole.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Span {
    pub label: &'static str,
    pub start: u32,
    pub end: u32,
}

/// What a tape hands out past its prefix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Tail {
    /// Zeros: every draw is the calm choice.
    Calm,
    /// The stream of a seed.
    Seeded(SplitMix64),
}

/// A line a case noted, and how many choices it had drawn by then.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Note {
    pub at: u32,
    pub text: String,
}

/// What a finished case's tape holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub choices: Vec<Choice>,
    /// In the order they were opened; one still open when the case ended is closed at the end.
    pub spans: Vec<Span>,
    pub notes: Vec<Note>,
    /// Each point once, in the order first reached.
    pub reached: Vec<&'static str>,
    /// `Tape::discard` ended the case.
    pub discarded: bool,
    /// The case drew more than the tape's bound.
    pub overran: bool,
    /// Prefix values lowered to fit the draw they met.
    pub clamped: u32,
    /// Prefix values the case never drew.
    pub unused: u32,
}

impl Record {
    /// Empty, allocating nothing.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::nonblocking]
    pub fn new() -> Record {
        Record {
            choices: Vec::new_in(Global),
            spans: Vec::new_in(Global),
            notes: Vec::new_in(Global),
            reached: Vec::new_in(Global),
            discarded: false,
            overran: false,
            clamped: 0,
            unused: 0,
        }
    }

    /// The values of `choices`, trailing zeros cut, into `out` (cleared first).
    #[blaze::pure]
    #[blaze::nonblocking]
    pub fn values_into(&self, out: &mut Vec<u64>) {
        out.clear();
        let kept =
            self.choices.iter().rposition(|choice| choice.value != 0).map_or(0, |last| last + 1);
        out.extend(self.choices[..kept].iter().map(|choice| choice.value));
    }
}

impl Default for Record {
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::nonblocking]
    fn default() -> Record {
        Record::new()
    }
}

/// A bug a control plants in the tape, for a test's oracle to catch (rule dst-not-review).
/// Passed in by value through `Tape::planted`; each names the draw it breaks.
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plant {
    None,
    /// `one_in(0)` draws once before it answers false; `SplitMix64`'s draws nothing.
    OneInDrawsAtZero,
    /// `shuffle` walks from the front, `1..len`, not from the back.
    ShuffleFromFront,
    /// A value from the prefix with `most == 1` is turned round: a replay answers a flag the
    /// other way from the case it was recorded from.
    ReplayFlipsFlags,
    /// `shuffle` swaps each element with the one at its canonical value, not `at - value`, so
    /// the calm value moves elements.
    ShuffleSwapsWithValue,
}

/// A prefix value as `Tape::draw` hands it out under `plant`: a flag turned round under
/// `Plant::ReplayFlipsFlags`, else the value itself.
#[cfg(test)]
fn planted_given(plant: Plant, most: u64, value: u64) -> u64 {
    if plant == Plant::ReplayFlipsFlags && most == 1 { 1 - value } else { value }
}

/// Whether a tape keeps what a case drew (choices, spans, notes, reach points) and holds its
/// bound on choices.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Recording {
    /// Kept and bounded: a tape of a check.
    Kept,
    /// Nothing kept and no bound: `Tape::seeded`, a tape outside a check.
    Skipped,
}

/// The names the tape's own operations hold its lock under, which a refusal of two holders at
/// once prints in place of a draw's label: each the operation's method.
const RESET: &str = "reset";
/// See [`RESET`].
const FINISH: &str = "finish";
/// See [`RESET`].
const NOTE: &str = "note";
/// See [`RESET`].
const DISCARD: &str = "discard";

/// `SplitMix64::unit_f64`'s precision: the bits of a draw it keeps, as many as an `f64`'s
/// significand holds, so each value is exact.
const UNIT_BITS: u32 = f64::MANTISSA_DIGITS;

/// How far a draw is shifted right to keep its high `UNIT_BITS` bits.
const UNIT_SHIFT: u32 = u64::BITS - UNIT_BITS;

/// The widest range `int_in` draws uniformly: one of at most 256 values (a `u8`'s worth), where
/// every value is common enough to reach.
const UNIFORM_VALUES: u64 = 256;

/// The kinds a wider range's draw in `int_in` is one of, each one in eight: kinds
/// `0..SMALL_FROM_END`, a small offset from the start; [`SMALL_FROM_END`], a small offset back
/// from the end; [`UNIFORM_FROM`] on, uniform over the range.
const INT_KINDS: u64 = 8;

/// The kind of `int_in`'s draw that is a small offset back from the range's end (one in eight).
const SMALL_FROM_END: u64 = 5;

/// The first kind of `int_in`'s draw that is uniform over the whole range (two in eight).
const UNIFORM_FROM: u64 = 6;

/// The least chance a list in `Tape::vec` ends at each element past its least size: one in 16
/// at most, so a seeded list is short more often than long whatever its sizes allow.
const MOST_STOP_ONE_IN: u64 = 16;

/// The tape's state behind its lock.
struct State {
    /// The prefix values not yet handed out, in reverse: the next is the last, popped as drawn.
    prefix: Vec<u64>,
    tail: Tail,
    most_choices: u32,
    choices: Vec<Choice>,
    spans: Vec<Span>,
    /// Indices into `spans` of the spans still open, innermost last.
    open: Vec<usize>,
    notes: Vec<Note>,
    reached: Vec<&'static str>,
    discarded: bool,
    overran: bool,
    clamped: u32,
}

impl State {
    /// Choices drawn so far; never more than `most_choices`, so it fits a `u32` (and saturates
    /// if it did not).
    fn drawn(&self) -> u32 {
        count(self.choices.len())
    }

    fn start(&mut self, prefix: &[u64], tail: Tail) {
        self.prefix.clear();
        self.prefix.extend(prefix.iter().rev());
        self.tail = tail;
        self.choices.clear();
        self.spans.clear();
        self.open.clear();
        self.notes.clear();
        self.reached.clear();
        self.discarded = false;
        self.overran = false;
        self.clamped = 0;
    }

    /// The next prefix value lowered to `most` (a lowered one counted in `clamped`), or `None`
    /// past the prefix.
    fn next_given(&mut self, most: u64) -> Option<u64> {
        let given = self.prefix.pop()?;
        if given > most {
            self.clamped += 1;
            Some(most)
        } else {
            Some(given)
        }
    }
}

/// A length as a `u32` count, saturating.
#[blaze::pure]
#[blaze::nopanic]
#[blaze::noalloc]
#[blaze::nonblocking]
#[expect(
    blaze::dropped_error,
    reason = "the only failure is a length past `u32::MAX`, which saturates: the bound on a case's choices, a `u32`, is met long before"
)]
fn count(len: usize) -> u32 {
    u32::try_from(len).unwrap_or(u32::MAX)
}

/// The lock a tape's handles share. It is only ever tried: a second draw while one is under way
/// is a refusal, never a wait.
#[expect(
    clippy::disallowed_types,
    reason = "blaze-sim sits below blaze-core and cannot name blaze_core::lock; the lock is only ever tried, never waited for"
)]
type Shared = Arc<std::sync::Mutex<State>, Global>;

/// The handle every part of a case draws from. `Clone` is the same tape.
#[derive(Clone)]
pub struct Tape {
    state: Shared,
    /// Whether choices, spans, notes, reach points and the bound are kept: skipped only for
    /// `Tape::seeded`. Fixed for the tape's life, so it is read without the lock.
    recording: Recording,
    #[cfg(test)]
    plant: Plant,
}

impl core::fmt::Debug for Tape {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_tuple(core::any::type_name::<Self>()).field(&self.recording).finish_non_exhaustive()
    }
}

/// `SplitMix64::unit_f64`'s scale: the high `UNIT_BITS` bits of a draw over `2^UNIT_BITS`.
#[expect(
    blaze::as_cast,
    reason = "the cast is the operation: an integer below 2^53, and 2^53 itself, made into the `f64` of the same value, which is exact"
)]
fn unit(bits: u64) -> f64 {
    bits as f64 * (1.0 / (1u64 << UNIT_BITS) as f64)
}

impl Tape {
    /// The most choices a case draws by default.
    pub const MOST_CHOICES: u32 = 1 << 20;

    #[expect(
        clippy::disallowed_types,
        reason = "blaze-sim sits below blaze-core and cannot name blaze_core::lock; the lock is only ever tried, never waited for"
    )]
    #[expect(
        blaze::std_use,
        reason = "bstd's named `Lock` is not in this tree and blaze-sim sits below blaze-core; the lock is only ever tried, never waited for, so it neither blocks nor needs a deadline"
    )]
    fn make(prefix: &[u64], tail: Tail, most_choices: u32, recording: Recording) -> Tape {
        let mut state = State {
            prefix: Vec::with_capacity_in(prefix.len(), Global),
            tail: Tail::Calm,
            most_choices,
            choices: Vec::new_in(Global),
            spans: Vec::new_in(Global),
            open: Vec::new_in(Global),
            notes: Vec::new_in(Global),
            reached: Vec::new_in(Global),
            discarded: false,
            overran: false,
            clamped: 0,
        };
        state.start(prefix, tail);
        Tape {
            state: Arc::new_in(std::sync::Mutex::new(state), Global),
            recording,
            #[cfg(test)]
            plant: Plant::None,
        }
    }

    /// A recorded tape: hands out `prefix` (each value lowered to fit its draw), then `tail`.
    /// The draw after `most_choices` choices sets `overran` and panics, naming the bound and the
    /// label.
    #[blaze::pure]
    #[blaze::nonblocking]
    pub fn new(prefix: &[u64], tail: Tail, most_choices: u32) -> Tape {
        Tape::make(prefix, tail, most_choices, Recording::Kept)
    }

    /// A tape outside a check: the seed's stream, exactly `SplitMix64::new(seed)`'s, and not
    /// recorded (no choices, spans, notes or bound are kept).
    #[blaze::pure]
    #[blaze::nonblocking]
    pub fn seeded(seed: u64) -> Tape {
        Tape::make(&[], Tail::Seeded(SplitMix64::new(seed)), u32::MAX, Recording::Skipped)
    }

    /// Recorded, every draw calm: `new(&[], Tail::Calm, MOST_CHOICES)`.
    #[blaze::pure]
    #[blaze::nonblocking]
    pub fn calm() -> Tape {
        Tape::new(&[], Tail::Calm, Tape::MOST_CHOICES)
    }

    /// Recorded: `new(values, Tail::Calm, MOST_CHOICES)`.
    #[blaze::pure]
    #[blaze::nonblocking]
    pub fn replay(values: &[u64]) -> Tape {
        Tape::new(values, Tail::Calm, Tape::MOST_CHOICES)
    }

    /// `new` with a bug planted, for a control.
    #[cfg(test)]
    pub(crate) fn planted(prefix: &[u64], tail: Tail, most_choices: u32, plant: Plant) -> Tape {
        let mut tape = Tape::new(prefix, tail, most_choices);
        tape.plant = plant;
        tape
    }

    /// Runs `body` on the state under the lock. A lock already held is two draws at once, which
    /// is refused; a poisoned one is taken anyway, since every hold is short and leaves the state
    /// whole.
    #[expect(
        blaze::std_use,
        reason = "bstd's named `Lock` is not in this tree and blaze-sim sits below blaze-core; `try_lock` never waits, and a poisoned hold is taken since every hold leaves the state whole"
    )]
    fn with<R>(&self, label: &'static str, body: impl FnOnce(&mut State) -> R) -> R {
        let mut guard = match self.state.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => panic!(
                "two draws at once on one tape (`{label}`): a case that draws from two threads at a time is not a function of its tape"
            ),
        };
        body(&mut *guard)
    }

    /// The one draw every method makes: one choice in `0..=most`. From the prefix, the next
    /// value lowered to `most` (counted in `clamped`); past it, 0 on a calm tail, or `canonical`
    /// of one raw `SplitMix64` draw on a seeded one. A recorded tape holds its bound first and
    /// records the choice. The overrun panics after the lock is let go.
    fn draw(&self, label: &'static str, most: u64, canonical: impl FnOnce(u64) -> u64) -> u64 {
        let recording = self.recording;
        #[cfg(test)]
        let plant = self.plant;
        let drawn = self.with(label, |state| {
            if recording == Recording::Kept && state.drawn() >= state.most_choices {
                state.overran = true;
                return Err(state.most_choices);
            }
            let value = match state.next_given(most) {
                Some(value) => {
                    #[cfg(test)]
                    let value = planted_given(plant, most, value);
                    value
                }
                None => match &mut state.tail {
                    Tail::Calm => 0,
                    Tail::Seeded(rng) => canonical(rng.next_u64()),
                },
            };
            if recording == Recording::Kept {
                state.choices.push(Choice { value, most, label });
            }
            Ok(value)
        });
        match drawn {
            Ok(value) => value,
            Err(bound) => panic!("a case drew more than {bound} choices (the last: `{label}`)"),
        }
    }

    /// Back to the start with a new prefix and tail, keeping the buffers.
    pub fn reset(&self, prefix: &[u64], tail: Tail) {
        self.with(RESET, |state| state.start(prefix, tail));
    }

    /// What the case drew, copied into `into` (its buffers reused); closes open spans, counts
    /// `unused`.
    pub fn finish(&self, into: &mut Record) {
        self.with(FINISH, |state| {
            let end = state.drawn();
            for &open in &state.open {
                state.spans[open].end = end;
            }
            state.open.clear();
            into.choices.clear();
            into.choices.extend_from_slice(&state.choices);
            into.spans.clear();
            into.spans.extend_from_slice(&state.spans);
            // `clone_from` keeps `into`'s note buffers, the strings' among them.
            into.notes.clone_from(&state.notes);
            into.reached.clear();
            into.reached.extend_from_slice(&state.reached);
            into.discarded = state.discarded;
            into.overran = state.overran;
            into.clamped = state.clamped;
            // What is left of the prefix is what the case never drew; a prefix past `u32::MAX`
            // values saturates.
            into.unused = count(state.prefix.len());
        });
    }

    /// One raw draw: `SplitMix64::next_u64`.
    pub fn next_u64(&self, label: &'static str) -> u64 {
        self.draw(label, u64::MAX, |r| r)
    }

    /// The high half of a draw: `SplitMix64::next_u32`.
    #[expect(
        blaze::dropped_error,
        reason = "the high half of a `u64` always fits a `u32`: there is no error to drop"
    )]
    pub fn next_u32(&self, label: &'static str) -> u32 {
        let high = self.draw(label, u64::from(u32::MAX), |r| r >> u32::BITS);
        // The draw's bound is `u32::MAX`, so it always fits and nothing is clamped.
        u32::try_from(high).unwrap_or(u32::MAX)
    }

    /// Below `n`, 0 when `n` is 0 (which still draws, as `SplitMix64::below` does); calm is 0.
    pub fn below(&self, n: u64, label: &'static str) -> u64 {
        self.draw(label, n.saturating_sub(1), |r| if n == 0 { 0 } else { r % n })
    }

    /// `below` for an index into something `len` long; calm is 0.
    #[expect(
        blaze::as_cast,
        reason = "a length widened from `usize` to `u64` for the draw, and the draw, below that length, back to `usize`, where it fits"
    )]
    pub fn index(&self, len: usize, label: &'static str) -> usize {
        self.below(len as u64, label) as usize
    }

    /// In `start..end`, or `start` when the range is empty; calm is `start`.
    pub fn range(&self, range: core::ops::Range<u64>, label: &'static str) -> u64 {
        range.start + self.below(range.end.saturating_sub(range.start), label)
    }

    /// True once in `n` on average, never (and drawing nothing) when `n` is 0; calm is false.
    pub fn one_in(&self, n: u64, label: &'static str) -> bool {
        if n == 0 {
            #[cfg(test)]
            {
                if self.plant == Plant::OneInDrawsAtZero {
                    self.draw(label, 1, |_| 0);
                }
            }
            return false;
        }
        self.draw(label, 1, |r| u64::from(r % n == 0)) != 0
    }

    /// True with probability `p`: never at `p <= 0` and always at `p >= 1`, neither drawing;
    /// calm is false.
    pub fn chance(&self, p: f64, label: &'static str) -> bool {
        if p <= 0.0 {
            false
        } else if p >= 1.0 {
            true
        } else {
            self.draw(label, 1, |r| u64::from(unit(r >> UNIT_SHIFT) < p)) != 0
        }
    }

    /// The low bit of a draw; calm is false.
    pub fn coin(&self, label: &'static str) -> bool {
        self.draw(label, 1, |r| r & 1) != 0
    }

    /// Uniform in `[0, 1)`, from the high 53 bits of a draw; calm is 0.0.
    pub fn unit_f64(&self, label: &'static str) -> f64 {
        unit(self.draw(label, (1 << UNIT_BITS) - 1, |r| r >> UNIT_SHIFT))
    }

    /// One of `items`, `None` (and no draw) when it is empty; calm is the first.
    pub fn pick<'a, T>(&self, items: &'a [T], label: &'static str) -> Option<&'a T> {
        if items.is_empty() {
            return None;
        }
        items.get(self.index(items.len(), label))
    }

    /// `out` filled with draws, each little-endian, the last cut to fit; calm is zeros.
    pub fn fill(&self, out: &mut [u8], label: &'static str) {
        const WORD: usize = size_of::<u64>();
        let mut chunks = out.chunks_exact_mut(WORD);
        for chunk in &mut chunks {
            chunk.copy_from_slice(&self.draw(label, u64::MAX, |r| r).to_le_bytes());
        }
        let rest = chunks.into_remainder();
        if !rest.is_empty() {
            let len = rest.len();
            // The low `len` bytes all ones: the most a draw of `len` bytes is.
            let mut ones = [0u8; WORD];
            ones[..len].fill(u8::MAX);
            let mask = u64::from_le_bytes(ones);
            rest.copy_from_slice(&self.draw(label, mask, |r| r & mask).to_le_bytes()[..len]);
        }
    }

    /// `items` in an order of the tape's (Fisher-Yates, from the back). Each choice is how far
    /// the element at `at` moves, so calm leaves the order alone.
    #[expect(
        blaze::as_cast,
        reason = "an index widened from `usize` to `u64` for the draw, and the draw, at most that index, back to `usize`, where it fits"
    )]
    pub fn shuffle<T>(&self, items: &mut [T], label: &'static str) {
        #[cfg(test)]
        {
            if self.plant == Plant::ShuffleFromFront {
                for at in 1..items.len() {
                    let reach = at as u64;
                    let moved = self.draw(label, reach, |r| reach - r % (reach + 1));
                    items.swap(at, at - moved as usize);
                }
                return;
            }
        }
        for at in (1..items.len()).rev() {
            let reach = at as u64;
            let moved = self.draw(label, reach, |r| reach - r % (reach + 1));
            let other = at - moved as usize;
            #[cfg(test)]
            let other =
                if self.plant == Plant::ShuffleSwapsWithValue { moved as usize } else { other };
            items.swap(at, other);
        }
    }

    /// `body`'s draws as one span. The lock is held only to note the start and the end, never
    /// while `body` runs; a span `body` leaves by a panic stays open until `finish` closes it.
    pub fn span<R>(&self, label: &'static str, body: impl FnOnce(&Tape) -> R) -> R {
        if self.recording == Recording::Skipped {
            return body(self);
        }
        self.with(label, |state| {
            let start = state.drawn();
            state.open.push(state.spans.len());
            state.spans.push(Span { label, start, end: start });
        });
        let result = body(self);
        let closed = self.with(label, |state| {
            let end = state.drawn();
            match state.open.pop() {
                Some(open) => {
                    state.spans[open].end = end;
                    true
                }
                None => false,
            }
        });
        assert!(closed, "the span `{label}` ended after the tape was reset inside it");
        result
    }

    /// Whether a list goes on: calm is that it ends. Seeded, it ends once in `stop_one_in`
    /// (`stop_one_in` of 0 or 1 ends it always).
    pub fn more(&self, stop_one_in: u64, label: &'static str) -> bool {
        self.draw(label, 1, |r| u64::from(stop_one_in > 1 && r % stop_one_in != 0)) != 0
    }

    /// A list of `sizes` elements, each `element`'s draws in a span, so deleting a span deletes
    /// an element. Calm is the least size.
    pub fn vec<T>(
        &self,
        sizes: core::ops::RangeInclusive<usize>,
        label: &'static str,
        mut element: impl FnMut(&Tape) -> T,
    ) -> Vec<T> {
        let (min, max) = (*sizes.start(), *sizes.end());
        assert!(min <= max, "the list `{label}` has no size: {min}..={max} is empty");
        #[expect(
            blaze::as_cast,
            reason = "a count of elements, `usize` to `u64`, widens on every target blaze builds"
        )]
        let stop = (((max - min) / 2 + 1) as u64).min(MOST_STOP_ONE_IN);
        let mut out = Vec::with_capacity_in(min, Global);
        // Bounded by `max`, and past `min` each turn draws, so a recorded tape's bound holds it.
        while out.len() < max {
            if out.len() >= min && !self.more(stop, label) {
                break;
            }
            out.push(self.span(label, &mut element));
        }
        out
    }

    /// Any `T`, small values and the ends more often than a uniform draw gives; calm is 0.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn int<T: Int>(&self, label: &'static str) -> T {
        T::draw_int(self, label)
    }

    /// In `range`; uniform when it has at most 256 values, else small offsets from its start and
    /// its ends more often; calm is its start. An empty range panics, naming the label.
    pub fn int_in<T: Int>(&self, range: core::ops::RangeInclusive<T>, label: &'static str) -> T {
        let (lo, hi) = (range.start().to_ordered(), range.end().to_ordered());
        assert!(lo <= hi, "the range of `{label}` is empty");
        let width = hi - lo;
        if width < UNIFORM_VALUES {
            return T::from_ordered(lo + self.below(width + 1, label));
        }
        let kind = self.below(INT_KINDS, label);
        if kind >= UNIFORM_FROM {
            // A width of `u64::MAX` is the whole `u64` range, one raw draw.
            let offset = match width.checked_add(1) {
                Some(values) => self.below(values, label),
                None => self.next_u64(label),
            };
            return T::from_ordered(lo + offset);
        }
        // The bit length of `width`, at least 9 here.
        let length = u64::from(u64::BITS - width.leading_zeros());
        let bits = self.below(length + 1, label);
        // An offset of `u64::BITS` bits is any `u64`, one raw draw.
        let offset = if bits == u64::from(u64::BITS) {
            self.next_u64(label)
        } else {
            self.below(1 << bits, label)
        };
        let offset = offset.min(width);
        T::from_ordered(if kind == SMALL_FROM_END { hi - offset } else { lo + offset })
    }

    /// Notes a line for the report, at the number of choices drawn so far.
    pub fn note(&self, text: core::fmt::Arguments<'_>) {
        if self.recording == Recording::Skipped {
            return;
        }
        // Formatted before the lock is taken: a `Display` may be anything.
        let text = std::fmt::format(text);
        self.with(NOTE, |state| {
            let at = state.drawn();
            state.notes.push(Note { at, text });
        });
    }

    /// Notes that the case got to `what`; each point is kept once, in the order first reached.
    pub fn reached(&self, what: &'static str) {
        if self.recording == Recording::Skipped {
            return;
        }
        self.with(what, |state| {
            if !state.reached.contains(&what) {
                state.reached.push(what);
            }
        });
    }

    /// Ends the case as not counted: sets `discarded` and panics.
    #[blaze::noalloc]
    pub fn discard(&self) -> ! {
        self.with(DISCARD, |state| state.discarded = true);
        panic!("discarded")
    }

    /// `discard` unless `holds`.
    #[blaze::noalloc]
    #[expect(
        blaze::bool_param,
        reason = "the bool is the condition a case assumes, written at the call (`tape.assume(x < 10)`), as every property-testing library takes it; public, called by many crates' tests"
    )]
    pub fn assume(&self, holds: bool) {
        if !holds {
            self.discard();
        }
    }
}

mod sealed {
    pub trait Sealed {}
}

/// The integers `Tape::int` draws: u8, u16, u32, u64, usize, i8, i16, i32, i64, isize. Sealed.
pub trait Int: Copy + sealed::Sealed {
    /// The value as a `u64` in the same order as the type's values, so the distance between two
    /// values is the difference of theirs.
    fn to_ordered(self) -> u64;
    /// The inverse of `to_ordered`, for a `u64` it gives.
    fn from_ordered(ordered: u64) -> Self;
    /// `Tape::int` for this type.
    fn draw_int(tape: &Tape, label: &'static str) -> Self;
}

/// The bit that turns a two's complement `i64` into a `u64` of the same order.
const SIGN: u64 = 1 << 63;

macro_rules! unsigned {
    ($($t:ty),*) => {$(
        impl sealed::Sealed for $t {}

        #[allow(clippy::unnecessary_cast, reason = "one body for every width, u64 among them")]
        impl Int for $t {
            fn to_ordered(self) -> u64 {
                self as u64
            }

            fn from_ordered(ordered: u64) -> $t {
                ordered as $t
            }

            fn draw_int(tape: &Tape, label: &'static str) -> $t {
                tape.int_in(0..=<$t>::MAX, label)
            }
        }
    )*};
}

macro_rules! signed {
    ($($t:ty => $u:ty),*) => {$(
        impl sealed::Sealed for $t {}

        #[allow(clippy::unnecessary_cast, reason = "one body for every width, i64 among them")]
        impl Int for $t {
            fn to_ordered(self) -> u64 {
                (self as i64 as u64) ^ SIGN
            }

            fn from_ordered(ordered: u64) -> $t {
                (ordered ^ SIGN) as i64 as $t
            }

            /// The unsigned draw of the same width, zigzag-decoded (0, -1, 1, -2, ..), so calm
            /// is 0 and small magnitudes are common.
            fn draw_int(tape: &Tape, label: &'static str) -> $t {
                let zigzag: $u = tape.int_in(0..=<$u>::MAX, label);
                ((zigzag >> 1) as $t) ^ -((zigzag & 1) as $t)
            }
        }
    )*};
}

unsigned!(u8, u16, u32, u64, usize);
signed!(i8 => u8, i16 => u16, i32 => u32, i64 => u64, isize => usize);
