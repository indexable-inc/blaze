//! Shapes of collections and sequences (docs/v2/plan/design/test.md, "End state: the search
//! itself", piece S2; decision D95), behind blaze-sim's feature `edge-first` until the harvest
//! (S8) makes it the default.
//!
//! - Repetition at the tape's level, so it needs no `Clone`: the choice by which a list goes on
//!   (today's `more`; calm: it stops) may also say "replay" (above "fresh", so lowering it makes
//!   the element fresh in place); the element's closure then runs over the
//!   recorded choices of an EARLIER element's span of the same list (an equal element), or over
//!   them with the span's last choice moved by one (a neighbour).
//! - `Tape::vec_ordered`: a list that is, rarely, sorted, reversed, or all one element, where
//!   the caller gives the order.
//! - [`Swarm`]: `Tape::swarm` draws once a case which kinds of operation are on (each with
//!   chance one half, never none; calm: all on) and notes the ones that are off, so a failure
//!   shows them; `Tape::op` draws the next operation among those on, sometimes the previous one
//!   again.
//!
//! Everything is a function of the tape: the same tape gives the same list, mask and operations.
//!
//! How a replay stays a function of the tape and shrinks. The choices a list draws are kept, in
//! the order drawn, in [`Shaping`] (inside the tape's state) while the list is open. A replay
//! copies the earlier element's own choices (its span less its replay decision) into a stack of
//! values, and while the element runs every draw takes the next copied value, in step:
//! - past the prefix on a seeded tail, the copied value IS the draw (lowered to fit the draw if
//!   its bound moved);
//! - from the prefix, the prefix wins and the copied value is dropped;
//! - past the prefix on a calm tail, the draw is calm, as every draw there is.
//! Every drawn value is recorded as its own choice in the NEW element's span, so the later span
//! holds its own copy of the choices. A recorded tape replayed (its values as the prefix) gives
//! each element from its own copy; the shrinker deleting the EARLIER element's span leaves the
//! later span's copy whole, and the later element is drawn from it as before. Nothing in the
//! later element refers to the earlier one once it is recorded.

#![blaze::sans_io]

#[cfg(feature = "edge-first")]
use core::cell::Cell;
#[cfg(feature = "edge-first")]
use core::cmp::Ordering;
#[cfg(feature = "edge-first")]
use core::ops::{Range, RangeInclusive};
#[cfg(feature = "edge-first")]
use std::alloc::Global;

#[cfg(feature = "edge-first")]
use crate::tape::{Choice, Tape, list_stop_one_in};

#[cfg(test)]
mod tests;

/// Of the elements a seeded list goes on to after its first, about one in this many replays an
/// earlier one.
#[cfg(feature = "edge-first")]
const REPLAY_ONE_IN: u64 = 3;

/// The forms a replay of one earlier element takes, in a decision's value: equal, its last
/// choice up, its last choice down.
#[cfg(feature = "edge-first")]
const FORMS: u64 = 3;

/// The label of the replay decision of an element the list's size forces (no `more` is drawn
/// for it); calm: fresh.
#[cfg(feature = "edge-first")]
const REPLAY: &str = "vec: replay an earlier element";

/// A replay decision's value past "fresh" (0: fresh; `1 + FORMS * back + form`), drawn fresh
/// from a raw draw's high half: a replay one time in `REPLAY_ONE_IN`, `back` uniform below
/// `earlier`, and the form equal one time in two, up and down one in four each. 0 when there is
/// no earlier element.
#[cfg(feature = "edge-first")]
#[blaze::pure]
#[blaze::nopanic]
#[blaze::noalloc]
#[blaze::nonblocking]
fn replay_part(raw: u64, earlier: u64) -> u64 {
    let high = raw >> 32;
    if earlier == 0 || high % REPLAY_ONE_IN != 0 {
        return 0;
    }
    let rest = high / REPLAY_ONE_IN;
    let back = rest % earlier;
    let form = match (rest / earlier) % 4 {
        0 | 1 => 0,
        2 => 1,
        _ => 2,
    };
    1 + FORMS * back + form
}

/// A replay decision's value past "fresh" read back: `None` for 0 (fresh), else how far back
/// (0: the element just before) and how the last choice moves. The value is at most
/// `FORMS * earlier` (the draw's bound), so `back` is below `earlier`.
#[cfg(feature = "edge-first")]
#[expect(
    blaze::as_cast,
    reason = "`back` is below the count of earlier elements, a `usize`, so it fits back"
)]
fn decoded(value: u64) -> Option<(usize, Last)> {
    let replay = value.checked_sub(1)?;
    let last = match replay % FORMS {
        0 => Last::Same,
        1 => Last::Up,
        _ => Last::Down,
    };
    Some(((replay / FORMS) as usize, last))
}

/// A count of elements as a `u64`.
#[cfg(feature = "edge-first")]
#[expect(
    blaze::as_cast,
    reason = "a count of elements, `usize` to `u64`, widens on every target blaze builds"
)]
fn wide(count: usize) -> u64 {
    count as u64
}

/// The decision of an element the list's size forces, after `earlier` elements: no choice for
/// the first; else one, labelled [`REPLAY`], calm fresh.
#[cfg(feature = "edge-first")]
fn forced(tape: &Tape, earlier: usize) -> Option<(usize, Last)> {
    if earlier == 0 {
        return None;
    }
    let n = wide(earlier);
    decoded(tape.draw_shaped(REPLAY, FORMS * n, |raw| replay_part(raw, n)))
}

/// Whether a list past its least size goes on, and how: today's `more` widened. `None`: it stops
/// (0, calm); `Some(None)`: a fresh element (1); `Some(Some(..))`: a replay (2 on). With no
/// earlier element it is `more` exactly: bound 1, the same canonical value.
#[cfg(feature = "edge-first")]
fn going_on(
    tape: &Tape,
    stop: u64,
    label: &'static str,
    earlier: usize,
) -> Option<Option<(usize, Last)>> {
    let n = wide(earlier);
    let value = tape.draw_shaped(label, 1 + FORMS * n, |raw| {
        if stop > 1 && raw % stop != 0 { 1 + replay_part(raw, n) } else { 0 }
    });
    value.checked_sub(1).map(decoded)
}

/// `vec_ordered`'s shape is one of this many kinds: [`SORTED`], [`REVERSED`] and [`ALL_ONE`]
/// one each, every other kind (calm among them) the list as drawn: one list in eight is shaped.
#[cfg(feature = "edge-first")]
const ORDER_KINDS: u64 = 24;
/// See [`ORDER_KINDS`].
#[cfg(feature = "edge-first")]
const SORTED: u64 = 21;
/// See [`ORDER_KINDS`].
#[cfg(feature = "edge-first")]
const REVERSED: u64 = 22;
/// See [`ORDER_KINDS`].
#[cfg(feature = "edge-first")]
const ALL_ONE: u64 = 23;
/// The label of `vec_ordered`'s shape.
#[cfg(feature = "edge-first")]
const ORDER: &str = "vec_ordered: sorted, reversed, all one, or as drawn";

/// `op` repeats the previous operation about once in this many.
#[cfg(feature = "edge-first")]
const OP_REPEAT_ONE_IN: u64 = 4;
/// The label of the choice whether `op` repeats the previous operation; calm: it does not.
#[cfg(feature = "edge-first")]
const OP_REPEAT: &str = "op: the previous operation again";

/// The most kinds a swarm has: one bit of a `u64` mask each (`u64::BITS`).
#[cfg(feature = "edge-first")]
const MOST_KINDS: usize = 64;

/// What a list keeps to replay an element, held in the tape's state (`State::shaping`) and
/// cleared with every case. See the module's doc for how a replay stays a function of the tape.
#[cfg(feature = "edge-first")]
pub(crate) struct Shaping {
    /// Every choice drawn while a list is open, in order, recorded or not (a `Tape::seeded`
    /// tape keeps no choices of its own, and still replays). An element's own choices are a
    /// range of it; cleared when the outermost open list closes, so an inner list's elements
    /// stay inside its outer element's range.
    taken: Vec<Choice>,
    /// Lists open, nested.
    lists: u32,
    /// The copied values a replayed element draws, in reverse: the next is the last.
    replay: Vec<u64>,
    /// An element is being replayed: every draw takes one copied value, in step.
    replaying: bool,
}

#[cfg(feature = "edge-first")]
impl Shaping {
    /// Empty, allocating nothing.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::nonblocking]
    pub(crate) fn new() -> Shaping {
        Shaping { taken: Vec::new_in(Global), lists: 0, replay: Vec::new_in(Global), replaying: false }
    }

    /// Back to no list open and no replay, keeping the buffers: a new case.
    pub(crate) fn clear(&mut self) {
        self.taken.clear();
        self.lists = 0;
        self.replay.clear();
        self.replaying = false;
    }

    /// The copied value this draw takes while an element is replayed (popped whether or not
    /// the draw uses it, so the copy stays in step), `None` when no element is or the copy ran
    /// out (the element drew more than the one it copies: the tail gives the rest).
    pub(crate) fn next_replayed(&mut self) -> Option<u64> {
        if self.replaying { self.replay.pop() } else { None }
    }

    /// A choice the tape drew, kept while a list is open.
    pub(crate) fn drew(&mut self, choice: Choice) {
        if self.lists > 0 {
            self.taken.push(choice);
        }
    }

    fn open_list(&mut self) {
        self.lists += 1;
    }

    fn close_list(&mut self) {
        self.lists = self.lists.saturating_sub(1);
        if self.lists == 0 {
            self.taken.clear();
        }
    }

    fn taken_len(&mut self) -> usize {
        self.taken.len()
    }

    /// Installs the choices of `taken[from]`, the last moved as `last` says, as the values the
    /// next element draws; false (nothing installed) while an outer element is replayed, whose
    /// copy already decides every draw of this one.
    fn begin_replay(&mut self, from: Range<usize>, last: Last) -> bool {
        if self.replaying {
            return false;
        }
        self.replay.clear();
        if let Some((&final_choice, rest)) = self.taken[from].split_last() {
            self.replay.push(nudged(final_choice, last));
            self.replay.extend(rest.iter().rev().map(|choice| choice.value));
        }
        self.replaying = true;
        true
    }

    fn end_replay(&mut self) {
        self.replaying = false;
        self.replay.clear();
    }
}

/// How a replay treats the copied span's last choice.
#[cfg(feature = "edge-first")]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Last {
    /// As it was: an equal element.
    Same,
    /// One more (one less at its bound): a neighbour.
    Up,
    /// One less (one more at 0): a neighbour.
    Down,
}

/// `choice`'s value moved as `last` says, kept in `0..=most`; a choice whose `most` is 0 has
/// no neighbour and stays.
#[cfg(feature = "edge-first")]
#[blaze::pure]
#[blaze::nopanic]
#[blaze::noalloc]
#[blaze::nonblocking]
fn nudged(choice: Choice, last: Last) -> u64 {
    match last {
        Last::Same => choice.value,
        Last::Up if choice.value < choice.most => choice.value + 1,
        Last::Up => choice.value.saturating_sub(1),
        Last::Down if choice.value > 0 => choice.value - 1,
        Last::Down => choice.most.min(1),
    }
}

/// Whether a list's elements are drawn freely or all as its first.
#[cfg(feature = "edge-first")]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Elements {
    /// Each element may replay an earlier one by a choice.
    Free,
    /// Every element after the first replays the first, with no choice drawn: all one element.
    AllLikeFirst,
}

/// Whether a chosen replay is installed. `Off` is the planted bug of this module's tests (the
/// replay decision is drawn and then ignored); `Tape::vec` always passes `On`.
#[cfg(feature = "edge-first")]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Repetition {
    On,
    #[cfg_attr(not(test), expect(dead_code, reason = "the planted bug of the module's tests"))]
    Off,
}

/// `Tape::vec` with `edge-first` on. The length: an edge size from `edge_ints::size` (S1a)
/// when it gives one, exactly that many elements and no stop choices; else today's stop rule.
/// In a free list the replay decision is the choice by which the list goes on (`going_on`,
/// today's `more` widened), or, for an element the size forces after the first, one choice of
/// its own (`forced`). Either sits before the element's span, outside it, and its calm and
/// lowered values are "stop" or "fresh" in place, so the shrinker never has to move a choice to
/// undo a replay. An all-one list (`vec_ordered`) draws `more` as today and replays its first.
#[cfg(feature = "edge-first")]
pub(crate) fn vec_shaped<T>(
    tape: &Tape,
    sizes: RangeInclusive<usize>,
    label: &'static str,
    mut element: impl FnMut(&Tape) -> T,
    elements: Elements,
    repetition: Repetition,
) -> Vec<T> {
    let (min, max) = (*sizes.start(), *sizes.end());
    assert!(min <= max, "the list `{label}` has no size: {min}..={max} is empty");
    let edge = crate::edge_ints::size(tape, min, max, label);
    if let Some(size) = edge {
        assert!(
            min <= size && size <= max,
            "the edge size {size} of the list `{label}` is outside its sizes {min}..={max} (edge_ints::size, test.md S1a)"
        );
    }
    let stop = list_stop_one_in(min, max);
    let mut out = Vec::with_capacity_in(edge.unwrap_or(min), Global);
    // Where each element's own choices lie in the shaping's `taken`.
    let mut owns: Vec<Range<usize>> = Vec::with_capacity_in(edge.unwrap_or(min), Global);
    tape.with_shaping(label, Shaping::open_list);
    // Bounded by the edge size or by `max`, and past `min` each turn draws, so a recorded
    // tape's bound holds it.
    loop {
        let len = out.len();
        // Decisions are drawn in a free list only; an all-one list counts no earlier element.
        let earlier = if elements == Elements::Free { len } else { 0 };
        let decided = match edge {
            Some(size) if len < size => Some(forced(tape, earlier)),
            Some(_) => None,
            None if len >= max => None,
            None if len < min => Some(forced(tape, earlier)),
            None => going_on(tape, stop, label, earlier),
        };
        let Some(replay) = decided else {
            break;
        };
        let source = match elements {
            Elements::Free => replay.map(|(back, last)| (owns[len - 1 - back].clone(), last)),
            Elements::AllLikeFirst => owns.first().map(|first| (first.clone(), Last::Same)),
        };
        let (item, own) = one_element(tape, label, source, &mut element, repetition);
        out.push(item);
        owns.push(own);
    }
    // A panic in an element leaves the list open: the next case's `reset` clears it.
    tape.with_shaping(label, Shaping::close_list);
    out
}

/// One element, its decision already drawn: `element` in its span over the copy of `source`
/// (an earlier element's own choices, the last moved as it says) or fresh, and the range of the
/// shaping's `taken` its own choices fill.
#[cfg(feature = "edge-first")]
fn one_element<T>(
    tape: &Tape,
    label: &'static str,
    source: Option<(Range<usize>, Last)>,
    element: &mut impl FnMut(&Tape) -> T,
    repetition: Repetition,
) -> (T, Range<usize>) {
    let installed = match (source, repetition) {
        (Some((from, last)), Repetition::On) => {
            tape.with_shaping(REPLAY, |shaping| shaping.begin_replay(from, last))
        }
        (Some(_), Repetition::Off) | (None, _) => false,
    };
    let drawn = tape.span(label, |tape| {
        let start = tape.with_shaping(label, Shaping::taken_len);
        let item = element(tape);
        let end = tape.with_shaping(label, Shaping::taken_len);
        (item, start..end)
    });
    if installed {
        tape.with_shaping(REPLAY, Shaping::end_replay);
    }
    drawn
}


/// Which kinds of operation are on for one case: drawn once by `Tape::swarm`, read by
/// `Tape::op`. Kinds are the positions of the slice `swarm` was given.
#[cfg(feature = "edge-first")]
#[derive(Debug)]
pub struct Swarm {
    /// Bit `k` set: kind `k` is on. Never 0.
    on: u64,
    /// How many kinds there are.
    kinds: usize,
    /// How many are on.
    count: usize,
    /// The kind `op` gave last, which it may give again.
    previous: Cell<Option<usize>>,
}

#[cfg(feature = "edge-first")]
impl Swarm {
    /// Whether kind `kind` is on this case (false past the kinds).
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn is_on(&self, kind: usize) -> bool {
        kind < self.kinds && self.on & (1u64 << kind) != 0
    }

    /// How many kinds are on: at least one.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn on_count(&self) -> usize {
        self.count
    }

    /// The `nth` kind that is on, counting from 0.
    fn nth_on(&self, nth: usize) -> usize {
        let mut left = nth;
        for kind in 0..self.kinds {
            if self.is_on(kind) {
                if left == 0 {
                    return kind;
                }
                left -= 1;
            }
        }
        panic!("a swarm with {} kinds on has no kind on at {nth}", self.count)
    }
}

#[cfg(feature = "edge-first")]
impl Tape {
    /// A list of `sizes` elements in the caller's order `cmp`: one list in eight is shaped,
    /// equally often sorted by `cmp`, sorted against it (reversed), or all one element (every
    /// element after the first replays the first's choices: equal with no `Clone`); else, and
    /// calm, as `vec` draws it. The shape is one choice drawn before the list.
    pub fn vec_ordered<T>(
        &self,
        sizes: RangeInclusive<usize>,
        label: &'static str,
        element: impl FnMut(&Tape) -> T,
        mut cmp: impl FnMut(&T, &T) -> Ordering,
    ) -> Vec<T> {
        let order = self.below(ORDER_KINDS, ORDER);
        let elements = if order == ALL_ONE { Elements::AllLikeFirst } else { Elements::Free };
        let mut list = vec_shaped(self, sizes, label, element, elements, Repetition::On);
        // Unstable sorts: no buffer allocated, and the same input gives the same order.
        if order == SORTED {
            list.sort_unstable_by(|a, b| cmp(a, b));
        } else if order == REVERSED {
            list.sort_unstable_by(|a, b| cmp(b, a));
        }
        list
    }

    /// Which of `kinds` are on this case: each off with chance one half (calm: on), and when
    /// every one came off, one drawn to stay on, so never none. Each kind that is off is noted
    /// (`swarm `<label>`: `<kind>` is off this case`), so a failure's notes show what the case
    /// could not do. More than 64 kinds, or none, is refused naming the label.
    pub fn swarm(&self, kinds: &[&'static str], label: &'static str) -> Swarm {
        assert!(!kinds.is_empty(), "the swarm `{label}` has no kinds: it needs one at least");
        assert!(
            kinds.len() <= MOST_KINDS,
            "the swarm `{label}` has {} kinds, more than the {MOST_KINDS} its mask holds",
            kinds.len()
        );
        let mut on = 0u64;
        for kind in 0..kinds.len() {
            if !self.coin(label) {
                on |= 1u64 << kind;
            }
        }
        if on == 0 {
            on = 1u64 << self.index(kinds.len(), label);
        }
        let mut count = 0;
        for (kind, name) in kinds.iter().enumerate() {
            if on & (1u64 << kind) == 0 {
                self.note(format_args!("swarm `{label}`: `{name}` is off this case"));
            } else {
                count += 1;
            }
        }
        Swarm { on, kinds: kinds.len(), count, previous: Cell::new(None) }
    }

    /// The next operation: the position in `swarm`'s kinds of one that is on. About one in four,
    /// the previous operation again; else the one drawn among those on (calm: the first on).
    /// Both choices are drawn every time, so an operation's span has one shape whatever came
    /// before it, and a replayed one stays in step.
    pub fn op(&self, swarm: &Swarm, label: &'static str) -> usize {
        let repeat = self.one_in(OP_REPEAT_ONE_IN, OP_REPEAT);
        let nth = self.index(swarm.count, label);
        let kind = match swarm.previous.get() {
            Some(previous) if repeat => previous,
            Some(_) | None => swarm.nth_on(nth),
        };
        swarm.previous.set(Some(kind));
        kind
    }
}
