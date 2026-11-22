//! Edge values first (docs/v2/plan/design/test.md, "End state: the search itself", piece S1a):
//! the one table of integer edge values that every width reads through its own bounds, and the
//! edge sizes of a list. Pure functions of a `Tape`.
//!
//! Each choice here draws its KIND first, as one choice whose calm part (three quarters of its
//! values, 0 among them) means "no edge": the caller then draws as it did before this module, so
//! the calm tape is unchanged and shrinking a kind towards 0 leads from an edge back to the old
//! draw. The edge part of the choice picks a row, then a value of the row inside the range; a
//! lower choice is a simpler row and an earlier value, so the shrinker also moves from a complex
//! edge to a simpler one.
//!
//! Everything but `size` is behind the cargo feature `edge-first` (off by default: the rollout of
//! the section above). With the feature off `size` returns `None` and draws NOTHING, so every
//! recorded tape means what it meant.
//!
//! THE TABLE (one, for every width u8 to u128, i8 to i128, usize and isize; a value that does not
//! lie in the asked range is never produced):
//! - `Small`: 0, 1, -1.
//! - `TypeEnds`: MIN, MIN+1, MAX-1, MAX of the type.
//! - `NarrowEnds`: for each width w in 8, 16, 32, 64, 128: 2^(w-1)-1, 2^(w-1), 2^w-1, 2^w,
//!   -2^(w-1), -2^(w-1)-1 (127, 128, 255, 256, -128, -129, 32767, ...): the ends of each width
//!   seen through a wider one.
//! - `Powers`: 2^k-1, 2^k, 2^k+1 for k in 0..128.
//! - `NegativePowers`: -(2^k-1), -2^k, -(2^k+1) for k in 0..128.
//!
//! and, from the range itself: `RangeEnds` (start, start+1, end-1, end) and `Middle` (the middle
//! and its two neighbours).

#![blaze::sans_io]

use crate::tape::Tape;
#[cfg(feature = "edge-first")]
use crate::tape::{Int, SIGN};

#[cfg(test)]
mod tests;

/// The part of a kind choice that means "no edge": a kind is drawn below `CALM_PARTS + 1` times
/// the edge slots, and the first `CALM_PARTS` of those parts (0 among them) are calm, so an edge
/// is about one draw in four.
#[cfg(feature = "edge-first")]
const CALM_PARTS: u64 = 3;

/// The bits of an edge slot that pick a value of the chosen row: 4,096 slots, ten or more for each
/// value of the longest row (384), so the modulo's unevenness stays under a tenth.
#[cfg(feature = "edge-first")]
const INT_MEMBER_BITS: u32 = 12;

/// The bits of an edge slot that pick a row by weight: 256 picks over a total weight of at most
/// 16, so the modulo's unevenness stays under a sixteenth.
#[cfg(feature = "edge-first")]
const ROW_PICK_BITS: u32 = 8;

/// The edge slots of an integer kind choice.
#[cfg(feature = "edge-first")]
const INT_EDGE_SLOTS: u64 = 1 << (ROW_PICK_BITS + INT_MEMBER_BITS);

/// The bits of an edge slot that pick a size of the chosen size row: 256 slots, seven or more for
/// each value of the longest size row (33).
#[cfg(feature = "edge-first")]
const SIZE_MEMBER_BITS: u32 = 8;

/// The edge slots of a size kind choice.
#[cfg(feature = "edge-first")]
const SIZE_EDGE_SLOTS: u64 = 1 << (ROW_PICK_BITS + SIZE_MEMBER_BITS);

/// The widths whose ends `NarrowEnds` holds.
#[cfg(feature = "edge-first")]
const NARROW_WIDTHS: [u32; 5] = [8, 16, 32, 64, 128];

/// The forms `NarrowEnds` holds for each of [`NARROW_WIDTHS`].
#[cfg(feature = "edge-first")]
const NARROW_FORMS: u32 = 6;

/// The steps around each power of two in `Powers` and `NegativePowers`: one below, the power,
/// one above.
#[cfg(feature = "edge-first")]
const POWER_STEPS: u32 = 3;

/// The greatest cap whose size and its neighbour below are a common edge (`SizeRow::Cap`); a
/// greater cap's are rare (`SizeRow::LargeCap`).
#[cfg(feature = "edge-first")]
const SMALL_CAP_MOST: usize = 4097;

/// The greatest cap `SizeRow::LargeCap` gives a size near: a list longer than a recorded tape's
/// default bound of choices (`Tape::MOST_CHOICES`, 2^20) overruns the tape when each element draws,
/// and when none draws it is a list of a million elements the test never asked to be likely.
#[cfg(feature = "edge-first")]
const LARGE_CAP_MOST: usize = 1 << 20;

/// The least exponent of `SizeRow::Powers`: 2^2-1 = 3 is the first size the small row lacks.
#[cfg(feature = "edge-first")]
const SIZE_POWER_LEAST: u32 = 2;

/// The greatest exponent of `SizeRow::Powers`: 4095, 4096, 4097.
#[cfg(feature = "edge-first")]
const SIZE_POWER_MOST: u32 = 12;

/// A type's place on the ordered line of `Int::to_ordered`: its bit width and its sign.
#[cfg(feature = "edge-first")]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Width {
    pub(crate) bits: u32,
    pub(crate) signed: bool,
}

#[cfg(feature = "edge-first")]
impl Width {
    /// `T`'s width.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn of<T: Int>() -> Width {
        Width { bits: T::BITS, signed: T::SIGNED }
    }

    /// Where 0 sits on the ordered line: [`SIGN`] for a signed type, 0 for an unsigned one.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    fn zero(self) -> u128 {
        if self.signed { SIGN } else { 0 }
    }

    /// The type's least value, ordered.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    fn min(self) -> u128 {
        if self.signed { SIGN - (1 << (self.bits - 1)) } else { 0 }
    }

    /// The type's greatest value, ordered.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    fn max(self) -> u128 {
        if self.signed {
            SIGN + ((1 << (self.bits - 1)) - 1)
        } else {
            u128::MAX >> (u128::BITS - self.bits)
        }
    }

    /// The number `-magnitude` (when `negative`) or `magnitude`, ordered; `None` when it lies
    /// past the ordered line, where it is outside every type.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    #[expect(
        blaze::bool_param,
        reason = "a sign and a magnitude are the number: the table's rows name both halves at each call"
    )]
    fn number(self, negative: bool, magnitude: u128) -> Option<u128> {
        if negative {
            self.zero().checked_sub(magnitude)
        } else {
            self.zero().checked_add(magnitude)
        }
    }
}

/// Which rows a test's choice reads: all of them, or a table with a planted bug, for a control
/// to catch. Outside tests every choice reads all of them.
#[cfg(all(test, feature = "edge-first"))]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Rows {
    All,
    /// `Powers` loses its neighbour below each power of two (7, 15, 31, ..).
    WithoutBelowPowers,
    /// A row's value is taken whether or not it lies in the range.
    Unfiltered,
}

/// A row of integer edge values: the table's, then the range's own.
#[cfg(feature = "edge-first")]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Row {
    Small,
    RangeEnds,
    TypeEnds,
    Middle,
    NarrowEnds,
    Powers,
    NegativePowers,
}

/// The rows in the order of simplicity, each with its weight out of 16: the narrower widths' ends
/// (the boundaries fuzzers keep in their dictionaries) take half; a row with no value in the range
/// gives its weight up to the others.
#[cfg(feature = "edge-first")]
const INT_ROWS: [(Row, u64); 7] = [
    (Row::Small, 1),
    (Row::RangeEnds, 1),
    (Row::TypeEnds, 1),
    (Row::Middle, 1),
    (Row::NarrowEnds, 8),
    (Row::Powers, 2),
    (Row::NegativePowers, 2),
];

/// Where an integer choice is: the asked range (ordered, `lo <= hi`), the type's width, and the
/// rows read.
#[cfg(feature = "edge-first")]
#[derive(Clone, Copy, Debug)]
struct IntAt {
    lo: u128,
    hi: u128,
    width: Width,
    #[cfg(test)]
    rows: Rows,
}

#[cfg(feature = "edge-first")]
impl Row {
    /// How many values the row lists before the range filters them.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    fn listed(self) -> u32 {
        match self {
            Row::Small | Row::Middle => 3,
            Row::RangeEnds | Row::TypeEnds => 4,
            Row::NarrowEnds => 5 * NARROW_FORMS,
            Row::Powers | Row::NegativePowers => u128::BITS * POWER_STEPS,
        }
    }

    /// The row's `index`th value, ordered, if it exists on the line (`None` past it, or dropped
    /// by a planted bug). Not yet filtered by the range.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    fn listed_at(self, index: u32, at: IntAt) -> Option<u128> {
        let width = at.width;
        match self {
            Row::Small => match index {
                0 => width.number(false, 0),
                1 => width.number(false, 1),
                _ => width.number(true, 1),
            },
            Row::RangeEnds => match index {
                0 => Some(at.lo),
                1 => at.lo.checked_add(1),
                2 => at.hi.checked_sub(1),
                _ => Some(at.hi),
            },
            Row::TypeEnds => match index {
                0 => Some(width.min()),
                1 => width.min().checked_add(1),
                2 => width.max().checked_sub(1),
                _ => Some(width.max()),
            },
            Row::Middle => {
                let middle = at.lo + (at.hi - at.lo) / 2;
                match index {
                    0 => Some(middle),
                    1 => middle.checked_sub(1),
                    _ => middle.checked_add(1),
                }
            }
            Row::NarrowEnds => {
                let bits = NARROW_WIDTHS.get(usize::try_from(index / NARROW_FORMS).ok()?)?;
                // 2^(w-1) always fits; 2^w does not when w is 128 (`checked_shl` says `None`).
                let half = 1u128 << (bits - 1);
                match index % NARROW_FORMS {
                    0 => width.number(false, half - 1),
                    1 => width.number(false, half),
                    2 => width.number(false, half - 1 + half),
                    3 => width.number(false, 1u128.checked_shl(*bits)?),
                    4 => width.number(true, half),
                    _ => width.number(true, half.checked_add(1)?),
                }
            }
            Row::Powers | Row::NegativePowers => {
                let power = 1u128 << (index / POWER_STEPS);
                let negative = self == Row::NegativePowers;
                match index % POWER_STEPS {
                    // -(2^0 - 1) is 0 again, which `Small` holds: listing it here would give an
                    // unsigned range a `NegativePowers` row of one value and its weight to 0.
                    0 if negative && power == 1 => None,
                    0 => {
                        #[cfg(test)]
                        if at.rows == Rows::WithoutBelowPowers && !negative {
                            return None;
                        }
                        width.number(negative, power - 1)
                    }
                    1 => width.number(negative, power),
                    _ => width.number(negative, power.checked_add(1)?),
                }
            }
        }
    }

    /// The row's `index`th value if it lies in the range.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    fn inside_at(self, index: u32, at: IntAt) -> Option<u128> {
        let value = self.listed_at(index, at)?;
        #[cfg(test)]
        if at.rows == Rows::Unfiltered {
            return Some(value);
        }
        (at.lo <= value && value <= at.hi).then_some(value)
    }

    /// How many of the row's values lie in the range.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    fn inside(self, at: IntAt) -> u64 {
        let mut count = 0;
        for index in 0..self.listed() {
            if self.inside_at(index, at).is_some() {
                count += 1;
            }
        }
        count
    }

    /// Whether some value of the row lies in the range: stops at the first, so a choice tests
    /// every row this way and counts only the row it picks.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    fn holds(self, at: IntAt) -> bool {
        (0..self.listed()).any(|index| self.inside_at(index, at).is_some())
    }

    /// The `slot`th value of the row inside the range, counting round; `None` when none is.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    fn pick(self, slot: u64, at: IntAt) -> Option<u128> {
        let count = self.inside(at);
        if count == 0 {
            return None;
        }
        let mut left = slot % count;
        for index in 0..self.listed() {
            if let Some(value) = self.inside_at(index, at) {
                if left == 0 {
                    return Some(value);
                }
                left -= 1;
            }
        }
        None
    }
}

/// The edge part of a kind drawn below `(CALM_PARTS + 1) * slots`: `None` for the calm parts.
#[cfg(feature = "edge-first")]
#[blaze::nonblocking]
fn edge_slot(tape: &Tape, slots: u64, label: &'static str) -> Option<u64> {
    let kind = tape.below((CALM_PARTS + 1) * slots, label);
    kind.checked_sub(CALM_PARTS * slots)
}

/// The row `pick` (in `0..2^ROW_PICK_BITS`) names among `rows` by weight, counting only the rows
/// that hold a value inside the range (`holds`, in the order of `rows`), round. `None` when no
/// row holds one.
#[cfg(feature = "edge-first")]
#[blaze::pure]
#[blaze::nopanic]
#[blaze::noalloc]
#[blaze::nonblocking]
fn weighted<R: Copy, const N: usize>(
    rows: &[(R, u64); N],
    holds: &[bool; N],
    pick: u64,
) -> Option<R> {
    let mut total = 0;
    for (&(_, weight), &held) in rows.iter().zip(holds) {
        if held {
            total += weight;
        }
    }
    if total == 0 {
        return None;
    }
    let mut left = pick % total;
    for (&(row, weight), &held) in rows.iter().zip(holds) {
        if held {
            if left < weight {
                return Some(row);
            }
            left -= weight;
        }
    }
    None
}

/// An edge value of the range `lo..=hi` (ordered, `lo <= hi`) of a type of `width`, or `None`
/// when the old draw should decide: what `Tape::int_in` and the signed `Int::draw_int` call first
/// with the feature on. Draws one choice.
#[cfg(feature = "edge-first")]
#[blaze::nonblocking]
pub(crate) fn in_range(
    tape: &Tape,
    lo: u128,
    hi: u128,
    width: Width,
    label: &'static str,
) -> Option<u128> {
    in_range_rows(
        tape,
        IntAt {
            lo,
            hi,
            width,
            #[cfg(test)]
            rows: Rows::All,
        },
        label,
    )
}

/// [`in_range`] reading `rows`: the planted tables of the tests come in here.
#[cfg(feature = "edge-first")]
#[blaze::nonblocking]
fn in_range_rows(tape: &Tape, at: IntAt, label: &'static str) -> Option<u128> {
    let slot = edge_slot(tape, INT_EDGE_SLOTS, label)?;
    let member = slot & ((1 << INT_MEMBER_BITS) - 1);
    let pick = slot >> INT_MEMBER_BITS;
    // `RangeEnds` always holds the range's start, so some row holds a value.
    let holds = INT_ROWS.map(|(row, _)| row.holds(at));
    let row = weighted(&INT_ROWS, &holds, pick)?;
    row.pick(member, at)
}

/// [`in_range`] over a planted table, for the tests' controls.
#[cfg(all(test, feature = "edge-first"))]
pub(crate) fn in_range_planted(
    tape: &Tape,
    lo: u128,
    hi: u128,
    width: Width,
    rows: Rows,
    label: &'static str,
) -> Option<u128> {
    in_range_rows(tape, IntAt { lo, hi, width, rows }, label)
}

/// A row of edge sizes of a list whose sizes are `min..=max`.
#[cfg(feature = "edge-first")]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SizeRow {
    /// The least size, one more, and 0, 1, 2.
    Small,
    /// The cap and the cap less one, when the cap is at most [`SMALL_CAP_MOST`].
    Cap,
    /// 2^k-1, 2^k, 2^k+1 for k in [`SIZE_POWER_LEAST`]..=[`SIZE_POWER_MOST`]: 3, 4, 5, 7, 8, 9,
    /// .., 4095, 4096, 4097.
    Powers,
    /// The cap and the cap less one, when the cap is past [`SMALL_CAP_MOST`] and at most
    /// [`LARGE_CAP_MOST`]: rare.
    LargeCap,
}

/// The size rows in the order of simplicity, each with its weight out of 16.
#[cfg(feature = "edge-first")]
const SIZE_ROWS: [(SizeRow, u64); 4] =
    [(SizeRow::Small, 3), (SizeRow::Cap, 2), (SizeRow::Powers, 10), (SizeRow::LargeCap, 1)];

#[cfg(feature = "edge-first")]
impl SizeRow {
    /// How many sizes the row lists before `min..=max` filters them.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    fn listed(self) -> u32 {
        match self {
            SizeRow::Small => 5,
            SizeRow::Cap | SizeRow::LargeCap => 2,
            SizeRow::Powers => (SIZE_POWER_MOST - SIZE_POWER_LEAST + 1) * POWER_STEPS,
        }
    }

    /// The row's `index`th size if it lies in `min..=max`.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    fn inside_at(self, index: u32, min: usize, max: usize) -> Option<usize> {
        let size = match self {
            SizeRow::Small => match index {
                0 => Some(min),
                1 => min.checked_add(1),
                2 => Some(0),
                3 => Some(1),
                _ => Some(2),
            },
            SizeRow::Cap | SizeRow::LargeCap => {
                let large = SMALL_CAP_MOST < max;
                let wanted = if self == SizeRow::Cap { !large } else { large && max <= LARGE_CAP_MOST };
                if !wanted {
                    return None;
                }
                if index == 0 { Some(max) } else { max.checked_sub(1) }
            }
            SizeRow::Powers => {
                let power = 1usize << (SIZE_POWER_LEAST + index / POWER_STEPS);
                match index % POWER_STEPS {
                    0 => Some(power - 1),
                    1 => Some(power),
                    _ => Some(power + 1),
                }
            }
        }?;
        (min <= size && size <= max).then_some(size)
    }

    /// How many of the row's sizes lie in `min..=max`.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    fn inside(self, min: usize, max: usize) -> u64 {
        let mut count = 0;
        for index in 0..self.listed() {
            if self.inside_at(index, min, max).is_some() {
                count += 1;
            }
        }
        count
    }

    /// The `slot`th size of the row inside `min..=max`, counting round; `None` when none is.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    fn pick(self, slot: u64, min: usize, max: usize) -> Option<usize> {
        let count = self.inside(min, max);
        if count == 0 {
            return None;
        }
        let mut left = slot % count;
        for index in 0..self.listed() {
            if let Some(size) = self.inside_at(index, min, max) {
                if left == 0 {
                    return Some(size);
                }
                left -= 1;
            }
        }
        None
    }
}

/// An edge size for a list whose sizes are `min..=max` (`min <= max`): `Some(n)` with
/// `min <= n <= max` when this list takes an edge size this case (0, 1, 2, the least size and one
/// more, the cap and the cap less one, a power of two or its neighbour up to 4097, rarely a size
/// near a cap up to 2^20), `None` when the old stop rule (`Tape::more`) should decide. Draws its
/// kind first, one choice whose calm part means `None`. `Tape::vec` (piece S2) calls it.
///
/// With the feature off: `None`, and nothing is drawn.
#[cfg(not(feature = "edge-first"))]
#[blaze::pure]
#[blaze::nopanic]
#[blaze::noalloc]
#[blaze::nonblocking]
#[allow(
    dead_code,
    reason = "piece S2's `Tape::vec` is its caller; until that call is written nothing else is"
)]
pub(crate) fn size(_tape: &Tape, _min: usize, _max: usize, _label: &'static str) -> Option<usize> {
    None
}

/// See the other `size` (the feature is on here): `Some(n)` with `min <= n <= max` about one
/// list in four, else `None`. One choice drawn.
#[cfg(feature = "edge-first")]
#[blaze::nonblocking]
#[allow(
    dead_code,
    reason = "piece S2's `Tape::vec` is its caller; until that call is written only the tests call it"
)]
pub(crate) fn size(tape: &Tape, min: usize, max: usize, label: &'static str) -> Option<usize> {
    assert!(min <= max, "the list `{label}` has no size: {min}..={max} is empty");
    let slot = edge_slot(tape, SIZE_EDGE_SLOTS, label)?;
    let member = slot & ((1 << SIZE_MEMBER_BITS) - 1);
    let pick = slot >> SIZE_MEMBER_BITS;
    // `Small` always holds `min`, so some row holds a size.
    let holds = SIZE_ROWS.map(|(row, _)| row.inside(min, max) > 0);
    let row = weighted(&SIZE_ROWS, &holds, pick)?;
    row.pick(member, min, max)
}
