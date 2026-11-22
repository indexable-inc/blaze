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
use std::ops::Range;
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
/// `choices[span.range()]`, which the shrinker deletes whole.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Span {
    pub label: &'static str,
    pub start: u32,
    pub end: u32,
}

impl Span {
    /// The positions of a record's `choices` the span covers: `choices[span.range()]` is
    /// what was drawn for it.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    #[expect(
        blaze::as_cast,
        reason = "a span's ends are `u32` counts of choices, and a `usize` holds every `u32` on every target blaze builds for"
    )]
    pub fn range(&self) -> Range<usize> {
        self.start as usize..self.end as usize
    }
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
    /// What `vec` keeps to replay an earlier element's choices (src/shape.rs, S2): the choices
    /// drawn while a list is open, and the values a replayed element draws past the prefix.
    #[cfg(feature = "edge-first")]
    shaping: crate::shape::Shaping,
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
        // A list a panic left open, or a replay it left installed, ends with its case.
        #[cfg(feature = "edge-first")]
        self.shaping.clear();
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

/// The stop rule of a list of `min..=max` elements: past `min` it ends once in this many turns,
/// half its free room plus one, at most [`MOST_STOP_ONE_IN`]. `min <= max`.
#[blaze::pure]
#[blaze::noalloc]
#[blaze::nonblocking]
#[expect(
    blaze::as_cast,
    reason = "a count of elements, `usize` to `u64`, widens on every target blaze builds"
)]
pub(crate) fn list_stop_one_in(min: usize, max: usize) -> u64 {
    (((max - min) / 2 + 1) as u64).min(MOST_STOP_ONE_IN)
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
            #[cfg(feature = "edge-first")]
            shaping: crate::shape::Shaping::new(),
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
    #[expect(
        blaze::constant_argument,
        reason = "the general constructor of a recorded tape: tests build recorded seeded tapes with it (`Tail::Seeded`), and the driver gives each case its tail through `reset`, so the crate's own non-test callers all pass `Tail::Calm`"
    )]
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

    /// `body` on what `vec` keeps for replay (src/shape.rs), under the lock as `with`.
    #[cfg(feature = "edge-first")]
    pub(crate) fn with_shaping<R>(
        &self,
        label: &'static str,
        body: impl FnOnce(&mut crate::shape::Shaping) -> R,
    ) -> R {
        self.with(label, |state| body(&mut state.shaping))
    }

    /// `draw` for src/shape.rs: one choice in `0..=most` (calm 0), `canonical` of a raw draw on
    /// a seeded tail; recorded, bounded and replayed as every draw is.
    #[cfg(feature = "edge-first")]
    pub(crate) fn draw_shaped(
        &self,
        label: &'static str,
        most: u64,
        canonical: impl FnOnce(u64) -> u64,
    ) -> u64 {
        self.draw(label, most, canonical)
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
            // While `vec` replays an earlier element, every draw takes the next of its copied
            // values, so the copy stays in step whichever source gives this draw (S2,
            // src/shape.rs): the prefix still wins, and a calm tail stays calm, so a recorded
            // tape replays to itself and a shrunk one is what its values say.
            #[cfg(feature = "edge-first")]
            let replayed = state.shaping.next_replayed();
            let value = match state.next_given(most) {
                Some(value) => {
                    #[cfg(test)]
                    let value = planted_given(plant, most, value);
                    value
                }
                None => match &mut state.tail {
                    Tail::Calm => 0,
                    Tail::Seeded(rng) => {
                        #[cfg(feature = "edge-first")]
                        let value = match replayed {
                            // Already canonical; lowered to fit a draw that changed its bound.
                            Some(copied) => copied.min(most),
                            None => canonical(rng.next_u64()),
                        };
                        #[cfg(not(feature = "edge-first"))]
                        let value = canonical(rng.next_u64());
                        value
                    }
                },
            };
            if recording == Recording::Kept {
                state.choices.push(Choice { value, most, label });
            }
            #[cfg(feature = "edge-first")]
            state.shaping.drew(Choice { value, most, label });
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
    #[expect(
        blaze::parallel_collections,
        reason = "`open` is not kept the length of `spans`: it is the stack of the spans still open, each an index into `spans`, pushed as a span opens and popped as it closes"
    )]
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
    ///
    /// With blaze-sim's feature `edge-first` (S2, src/shape.rs): the length may be an edge size
    /// (`edge_ints::size`), and an element may replay an earlier element's choices (an equal
    /// element, no `Clone` needed) or them with the last one moved by one (a neighbour). Without
    /// it, exactly the choices of the stop rule alone, in the same order.
    pub fn vec<T>(
        &self,
        sizes: core::ops::RangeInclusive<usize>,
        label: &'static str,
        element: impl FnMut(&Tape) -> T,
    ) -> Vec<T> {
        self.vec_now(sizes, label, element)
    }

    /// `vec` with `edge-first` on: the shaped list of src/shape.rs, replay on.
    #[cfg(feature = "edge-first")]
    fn vec_now<T>(
        &self,
        sizes: core::ops::RangeInclusive<usize>,
        label: &'static str,
        element: impl FnMut(&Tape) -> T,
    ) -> Vec<T> {
        crate::shape::vec_shaped(
            self,
            sizes,
            label,
            element,
            crate::shape::Elements::Free,
            crate::shape::Repetition::On,
        )
    }

    /// `vec` with `edge-first` off: the list as it has always been drawn.
    #[cfg(not(feature = "edge-first"))]
    fn vec_now<T>(
        &self,
        sizes: core::ops::RangeInclusive<usize>,
        label: &'static str,
        mut element: impl FnMut(&Tape) -> T,
    ) -> Vec<T> {
        let (min, max) = (*sizes.start(), *sizes.end());
        assert!(min <= max, "the list `{label}` has no size: {min}..={max} is empty");
        let stop = list_stop_one_in(min, max);
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
    ///
    /// With the feature `edge-first`, an edge value of the range first, about one draw in four
    /// (`edge_ints::in_range`: the range's ends, their neighbours, its middle, and every value of
    /// the one table that lies inside it); the calm kind leaves the draw above unchanged.
    pub fn int_in<T: Int>(&self, range: core::ops::RangeInclusive<T>, label: &'static str) -> T {
        let (lo, hi) = (range.start().to_ordered(), range.end().to_ordered());
        assert!(lo <= hi, "the range of `{label}` is empty");
        #[cfg(feature = "edge-first")]
        if let Some(edge) =
            crate::edge_ints::in_range(self, lo, hi, crate::edge_ints::Width::of::<T>(), label)
        {
            return T::from_ordered(edge);
        }
        T::from_ordered(self.ordered_in(lo, hi, label))
    }

    /// `int_in`'s draw without its edge values, on the ordered line (`lo <= hi`): uniform when
    /// the range has at most 256 values; else one in eight a small offset back from the end, two
    /// in eight uniform, the rest a small offset from the start. For a range of at most 2^64
    /// values it draws what `int_in` drew before 128-bit integers and the edge values, choice for
    /// choice, so every recorded tape replays to the same values.
    pub(crate) fn ordered_in(&self, lo: u128, hi: u128, label: &'static str) -> u128 {
        let width = hi - lo;
        if width < u128::from(UNIFORM_VALUES) {
            // `width + 1` is at most 256, one `below`.
            return lo + self.below_wide(Some(width + 1), label);
        }
        let kind = self.below(INT_KINDS, label);
        if kind >= UNIFORM_FROM {
            // A width of `u128::MAX` is the whole `u128` range: `None` values past it.
            return lo + self.below_wide(width.checked_add(1), label);
        }
        // The bit length of `width`, 9 to 128 here.
        let length = u128::BITS - width.leading_zeros();
        let bits = self.below(u64::from(length) + 1, label);
        // 2^bits offsets; `bits` is at most 128, and 2^128 is `None` (any `u128`). A `bits` past
        // a `u32` cannot be drawn: it too is "past the line", `None`.
        let offsets = u32::try_from(bits).ok().and_then(|bits| 1u128.checked_shl(bits));
        let offset = self.below_wide(offsets, label).min(width);
        if kind == SMALL_FROM_END { hi - offset } else { lo + offset }
    }

    /// Below `values` (`None` is 2^128, every `u128`; `Some(0)` is 0, drawing as `below(0)`
    /// does); calm is 0. Up to 2^64 values it is one `below`, or at exactly 2^64 one raw draw,
    /// as `int_in` always drew; past it, two raw draws, the high one first, taken modulo `values`
    /// (uneven by at most `values / 2^128`).
    fn below_wide(&self, values: Option<u128>, label: &'static str) -> u128 {
        const RAW: u128 = 1 << u64::BITS;
        match values {
            Some(values) if values < RAW => {
                // Below 2^64, so it fits a `u64`.
                u128::from(self.below(u64::try_from(values).unwrap_or(u64::MAX), label))
            }
            Some(RAW) => u128::from(self.next_u64(label)),
            values => {
                let high = u128::from(self.next_u64(label));
                let low = u128::from(self.next_u64(label));
                let raw = (high << u64::BITS) | low;
                match values {
                    Some(values) => raw % values,
                    None => raw,
                }
            }
        }
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

    /// Notes that the case got to the typed reach point `point` (limits.rs): `reached` of its
    /// id's path, so [`Config::must_reach_points`] can require it by type.
    ///
    /// [`Config::must_reach_points`]: crate::search::Config::must_reach_points
    pub fn reach(&self, point: impl crate::limits::Point) {
        self.reached(point.id().path());
    }

    /// A limit between `smallest` (the least legal value) and `shipped`, drawn for a simulation
    /// (D165; limits.rs): seven draws in sixteen give `smallest` (calm is `smallest`, so a
    /// shrunk failure is at the tightest cap), four `smallest + 1`, two `smallest + 2`, one a
    /// uniform value of the whole range and two `shipped`; never past `shipped`. When `smallest`
    /// equals `shipped` the limit is fixed and nothing is drawn. A `smallest` above `shipped`
    /// panics naming `label`.
    pub fn limit<T: Int>(&self, shipped: T, smallest: T, label: &'static str) -> T {
        let (low, high) = (smallest.to_ordered(), shipped.to_ordered());
        assert!(
            low <= high,
            "the limit `{label}`: its smallest legal value is above the value it ships with"
        );
        if low == high {
            return shipped;
        }
        let kind = self.below(crate::limits::LIMIT_KINDS, label);
        let value = match crate::limits::limit_of_kind(kind, low, high) {
            Some(value) => value,
            None => self.ordered_in(low, high, label),
        };
        T::from_ordered(value)
    }

    /// A component's limits drawn as one span (`Limits::draw`, limits.rs).
    pub fn limits<L: crate::limits::Limits>(&self) -> L {
        self.span("limits", L::draw)
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

/// The integers `Tape::int` draws: u8, u16, u32, u64, u128, usize, i8, i16, i32, i64, i128,
/// isize. Sealed.
pub trait Int: Copy + sealed::Sealed {
    /// The type's width in bits.
    const BITS: u32;
    /// Whether the type is signed.
    const SIGNED: bool;
    /// The value as a `u128` in the same order as the type's values, so the distance between two
    /// values is the difference of theirs.
    fn to_ordered(self) -> u128;
    /// The inverse of `to_ordered`, for a `u128` it gives.
    fn from_ordered(ordered: u128) -> Self;
    /// `Tape::int` for this type.
    fn draw_int(tape: &Tape, label: &'static str) -> Self;
}

/// The bit that turns a two's complement `i128` into a `u128` of the same order: where 0 sits on
/// the ordered line of a signed type (`edge_ints` places the table's values from it).
pub(crate) const SIGN: u128 = 1 << 127;

macro_rules! unsigned {
    ($($t:ty),*) => {$(
        impl sealed::Sealed for $t {}

        #[allow(clippy::unnecessary_cast, reason = "one body for every width, u128 among them")]
        impl Int for $t {
            const BITS: u32 = <$t>::BITS;
            const SIGNED: bool = false;

            fn to_ordered(self) -> u128 {
                self as u128
            }

            fn from_ordered(ordered: u128) -> $t {
                ordered as $t
            }

            /// `int_in` over the whole type, which with `edge-first` reads the table first.
            fn draw_int(tape: &Tape, label: &'static str) -> $t {
                tape.int_in(0..=<$t>::MAX, label)
            }
        }
    )*};
}

macro_rules! signed {
    ($($t:ty => $u:ty),*) => {$(
        impl sealed::Sealed for $t {}

        #[allow(clippy::unnecessary_cast, reason = "one body for every width, i128 among them")]
        impl Int for $t {
            const BITS: u32 = <$t>::BITS;
            const SIGNED: bool = true;

            fn to_ordered(self) -> u128 {
                (self as i128 as u128) ^ SIGN
            }

            fn from_ordered(ordered: u128) -> $t {
                (ordered ^ SIGN) as i128 as $t
            }

            /// With `edge-first`, an edge value of the whole signed type first (about one draw in
            /// four: 0, -1, MIN, -129, 2^k-1, ..); then, as always, the unsigned draw of the same
            /// width without its own edge values, zigzag-decoded (0, -1, 1, -2, ..), so calm is 0
            /// and small magnitudes are common. With the feature off it draws exactly what it drew.
            fn draw_int(tape: &Tape, label: &'static str) -> $t {
                #[cfg(feature = "edge-first")]
                if let Some(edge) = crate::edge_ints::in_range(
                    tape,
                    <$t>::MIN.to_ordered(),
                    <$t>::MAX.to_ordered(),
                    crate::edge_ints::Width::of::<$t>(),
                    label,
                ) {
                    return <$t>::from_ordered(edge);
                }
                let zigzag = <$u>::from_ordered(tape.ordered_in(0, <$u>::MAX.to_ordered(), label));
                ((zigzag >> 1) as $t) ^ -((zigzag & 1) as $t)
            }
        }
    )*};
}

unsigned!(u8, u16, u32, u64, u128, usize);
signed!(i8 => u8, i16 => u16, i32 => u32, i64 => u64, i128 => u128, isize => usize);
