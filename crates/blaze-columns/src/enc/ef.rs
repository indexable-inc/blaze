//! A sorted list of ids in Elias-Fano form (Elias 1974, Fano 1971; Vigna, "Quasi-succinct
//! indices", WSDM 2013): a strictly increasing sequence of `n` ids below a stated universe
//! bound `u` (a `u64`; a `u32` caller widens), read in place by value: `next_geq(x)` (the first
//! id not below `x`, the skip that makes intersections and "is any of these dirty" fast),
//! `contains`, `get(i)`, a forward iterator, and a leapfrog intersection. The record's reader
//! lists, a module's items and a read list are such lists.
//!
//! # Form
//!
//! Each id is split at `l = floor(log2(u / n))` bits (0 when `n` is 0 or `u / n` is below 2).
//! The low `l` bits of id `i` are packed by [`bits`](super::bits) at bit `i * l`. The high part
//! `h = id >> l` is written in unary: bit `h + i` of the upper bits is set. So the upper bits
//! hold `n` ones and one zero ending each bucket `0..=h_last`: the zero numbered `j` (from 0)
//! follows every id whose high part is at most `j`. Id `i` is `(select1(i) - i) << l | low(i)`.
//! To answer `select1` and `select0` with a short scan, the position of every [`QUANTUM`]-th one
//! and every [`QUANTUM`]-th zero (from the first) is kept as a `u32`: a scan then starts at the
//! sampled bit and counts whole words (one `count_ones` each) to the word that holds its target,
//! then selects within that word in at most 16 steps.
//!
//! # Layout
//!
//! Little-endian, unaligned, a pure function of the ids and the universe (rule 25):
//!
//! | bytes | what |
//! |---|---|
//! | 1 | kind 4 (the form number; `ints` uses 0 and 1, `ids` 3) |
//! | 1 | `l`, the low bits' width |
//! | 4 | `n` |
//! | 8 | `u`, the universe bound |
//! | 4 | the upper bits' length, `n + h_last + 1` (0 when `n` is 0) |
//! | `ceil(upper / 8)` | the upper bits, bit `k` at bit `k % 8` of byte `k / 8`, unused bits zero |
//! | `4 * ceil(n / 256)` | the one hints: the position of one `256 k` |
//! | `4 * ceil(zeros / 256)` | the zero hints: the position of zero `256 k` |
//! | `ceil(n * l / 8)` | the low bits ([`bits::pack`](super::bits::pack)) |
//! | [`PAD`] | zero bytes, so the last low value's load stays inside (as every `enc` column ends) |
//!
//! Hints are written only when the upper bits are more than [`SHORT_WORDS`] words: a list that
//! short is scanned whole, faster than a hint is loaded. [`SortedIds::open`] checks the header,
//! the length its counts give, every padding bit, the ones' count, the last two upper bits, that
//! the ids increase and stay below `u`, and every hint, once (rule 74); a read after `open`
//! checks nothing again, cannot leave the bytes, and allocates nothing.
//!
//! # Size against the floor
//!
//! The floor for a list of `n` ids from `u` is about `2 + log2(u / n)` bits an id (Elias-Fano's
//! own bound; the information-theoretic `log2 C(u, n)` is about `1.44 + log2(u / n)`). The upper
//! bits cost at most `2` an id (`n` ones and at most `u >> l < 2n` zeros), the low bits `l`, the
//! hints at most `32 / 256 * (1 + zeros / n)`, under 0.375, and the header and pad 26 bytes.
//! With the ids spread to `u - 1` (the most upper bits a list of that `n` and `u` can take):
//!
//! | `u` | `n` | bytes | bits an id | floor | ratio |
//! |---|---|---|---|---|---|
//! | 2^20 | 10 | 50 | 40.0 | 18.68 | 2.14 |
//! | 2^20 | 1,000 | 1,561 | 12.49 | 12.03 | 1.04 |
//! | 2^20 | 100,000 | 70,022 | 5.60 | 5.39 | 1.04 |
//! | 2^32 | 10 | 65 | 52.0 | 30.68 | 1.69 |
//! | 2^32 | 1,000 | 3,061 | 24.49 | 24.03 | 1.02 |
//! | 2^32 | 100,000 | 220,022 | 17.60 | 17.39 | 1.01 |
//!
//! Bytes are `18 + ceil(upper / 8) + 4 * hints + ceil(n * l / 8) + 8`; the test
//! `sizes_are_the_module_docs_table` holds every row. [`SortedIds::most_bytes`] gives the same
//! bound for any `n` and `u`, so a caller can choose before it builds.
//!
//! # When a plain array is better
//!
//! The 26 bytes of header and pad dominate a short list. Against a plain array of `u32` ids (4
//! bytes an id) this form is smaller from 16 ids when `u` is 2^20 and from about 56 (58 and up
//! always; rounding makes 55 to 57 tie or alternate) when `u` is 2^32; against `u64` ids, from 5
//! and from 7. Below those, a plain array is smaller, and for a list of a few dozen ids, one or
//! two cache lines, a binary or linear search over it is also faster than a select (not
//! measured here; the reason is the work: a compare a probe against a word scan and a low load).
//!
//! Against an `ints` column of `block`s (frame of reference or delta over blocks of 1,024): a
//! block's size depends on how regular the values are, this form's only on `n` and `u`. A list
//! that is a line (`a + b * i`) or one run packs to almost nothing as a `Linear` block and should
//! be one; a list of scattered ids does not, and there a block pays its widest gap in every
//! value: `Delta` about `log2(u / n) + 3` to `4` bits an id for uniformly scattered ids (the
//! largest of 1,024 gaps is near 7 times the mean), `For` about `log2(u / n) + 10`, against this
//! form's `2 + log2(u / n)` and a fraction (a model, not a measurement). And a block is read by
//! position: `next_geq` there is a binary search over the block heads (`log2(n / 1024)` entry
//! loads) and then a decode of one block of up to 1,024 values (a `Delta` block's values are a
//! running sum, so all before the target), where here it is one hint load, a scan of a few words
//! (on average half of 256 zeros and the ones among them) and one low load an id of the bucket.
//! So: a column read by row stays an `ints` column; a set read by value is this form.

use core::fmt;

use blaze_int::{Int, idx};
use blaze_own::{AVec, AllocFailure};

use super::bits::{self, PAD};
use super::{EncError, len32, patch32, put};

/// The form number, the column's first byte.
const KIND_SORTED_IDS: u8 = 4;

/// Bytes of the header: kind, `l`, `n`, `u`, the upper bits' length.
pub const HEADER: usize = 18;

/// One hint every this many ones and every this many zeros. Above 64, so at most one sample
/// falls in a word of the upper bits.
pub const QUANTUM: usize = 256;
const _: () = assert!(QUANTUM > 64);

/// Upper bits of at most this many 64-bit words carry no hints: they are scanned whole.
pub const SHORT_WORDS: usize = 8;

/// The most ids a list holds: its upper bits, at most `3n + 1`, stay positions a `u32` holds.
pub const MOST_IDS: usize = 1 << 30;

/// What `open` refuses, each named by its `EncError::Corrupt { what, .. }`.
pub const SHORT: &str = "a sorted id list shorter than its header";
pub const NOT_IDS: &str = "not a sorted id list (another form number)";
pub const COUNT: &str = "a sorted id list of more ids than its universe or than one holds";
pub const WIDTH: &str = "a sorted id list whose low-bit width is not the one its count and universe give";
pub const UPPER: &str = "a sorted id list whose upper bits do not fit its count and universe";
pub const LENGTH: &str = "a sorted id list whose length is not the one its header's counts give";
pub const PADDING: &str = "a sorted id list with a padding bit set";
pub const ONES: &str = "a sorted id list whose upper bits do not hold one one an id";
pub const END: &str = "a sorted id list whose upper bits do not end at its last id";
pub const ORDER: &str = "a sorted id list whose ids do not increase below its universe";
pub const HINT: &str = "a sorted id list with a select hint its upper bits do not give";

/// Why a list was not written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BuildError {
    /// Id `index` is `id`, not above the one before it, `previous`.
    NotIncreasing { index: usize, previous: u64, id: u64 },
    /// Id `index` is `id`, not below the universe bound.
    NotBelow { index: usize, id: u64, universe: u64 },
    /// No room, or more ids than a list holds.
    Enc(EncError),
}

impl From<EncError> for BuildError {
    fn from(error: EncError) -> BuildError {
        BuildError::Enc(error)
    }
}

impl From<AllocFailure> for BuildError {
    fn from(failure: AllocFailure) -> BuildError {
        BuildError::Enc(EncError::Alloc(failure))
    }
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BuildError::NotIncreasing { index, previous, id } => write!(
                f,
                "blaze_columns::enc::ef: id {index} of a sorted id list is {id}, not above the one before it, {previous}"
            ),
            BuildError::NotBelow { index, id, universe } => write!(
                f,
                "blaze_columns::enc::ef: id {index} of a sorted id list is {id}, not below its universe bound {universe}"
            ),
            BuildError::Enc(error) => write!(f, "{error}"),
        }
    }
}

impl core::error::Error for BuildError {}

/// `l` for `n` ids below `universe`.
pub fn low_width_for(n: usize, universe: u64) -> u8 {
    if n == 0 {
        return 0;
    }
    match universe / n.widen_to::<u64>() {
        0 => 0,
        // `ilog2` of a `u64` is at most 63.
        quotient => quotient.ilog2().truncate_to::<u8>(),
    }
}

/// Where each part of a list lies, from its header's counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Layout {
    upper_bytes: usize,
    one_hints: usize,
    zero_hints: usize,
    /// The start of the upper bits, the one hints, the zero hints and the low bits.
    upper_at: usize,
    ones_at: usize,
    zeros_at: usize,
    low_at: usize,
    total: usize,
}

impl Layout {
    /// The layout of `n` ids with low bits of `w` and upper bits of `upper_bits`; `None` when
    /// a size overflows.
    fn of(n: usize, w: u8, upper_bits: usize) -> Option<Layout> {
        let zeros = upper_bits.checked_sub(n)?;
        let low_bytes = n.checked_mul(usize::from(w))?.div_ceil(8);
        let upper_bytes = upper_bits.div_ceil(8);
        let (one_hints, zero_hints) = match upper_bits.div_ceil(64) > SHORT_WORDS {
            true => (n.div_ceil(QUANTUM), zeros.div_ceil(QUANTUM)),
            false => (0, 0),
        };
        let upper_at = HEADER;
        let ones_at = upper_at.checked_add(upper_bytes)?;
        let zeros_at = ones_at.checked_add(one_hints.checked_mul(4)?)?;
        let low_at = zeros_at.checked_add(zero_hints.checked_mul(4)?)?;
        let total = low_at.checked_add(low_bytes)?.checked_add(PAD)?;
        Some(Layout { upper_bytes, one_hints, zero_hints, upper_at, ones_at, zeros_at, low_at, total })
    }
}

/// The little-endian `u64` at byte `at` of `bytes`, zero past their end: the upper bits have
/// no pad, and their unused bits are zero (checked by `open`), so a word past them is empty.
#[inline(always)]
fn word_at(bytes: &[u8], at: usize) -> u64 {
    let mut word = [0u8; 8];
    if let Some(tail) = bytes.get(at..) {
        let take = tail.len().min(8);
        word[..take].copy_from_slice(&tail[..take]);
    }
    u64::from_le_bytes(word)
}

/// The `u32` at byte `at`, if it is inside `bytes`.
#[inline(always)]
fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    bytes.get(at..).and_then(<[u8]>::first_chunk::<4>).map(|b| u32::from_le_bytes(*b))
}

/// The low `bits` bits set, 0 to 64.
#[inline(always)]
fn low_mask(bits: usize) -> u64 {
    if bits >= 64 { u64::MAX } else { (1u64 << bits.truncate_to::<u32>()) - 1 }
}

/// The bits of a word at and above `at`'s place in it.
#[inline(always)]
fn from_place(at: usize) -> u64 {
    u64::MAX << (at % 64).truncate_to::<u32>()
}

#[inline(always)]
fn ones(word: u64) -> usize {
    idx(word.count_ones())
}

/// The place of set bit `rank` (from 0) of `word`, `rank` below its count of ones: a byte at a
/// time to the byte that holds it (at most 8 steps), then at most 7 bits cleared in that byte.
#[inline]
fn select_in_word(word: u64, rank: usize) -> usize {
    let (mut rest, mut bits, mut base) = (rank, word, 0usize);
    for _ in 0..8 {
        let here = ones(bits & 0xFF);
        if rest < here {
            break;
        }
        rest -= here;
        bits >>= 8;
        base += 8;
    }
    for _ in 0..rest.min(7) {
        bits &= bits.wrapping_sub(1);
    }
    base + idx(bits.trailing_zeros())
}

/// The clear bits of upper word `w` that lie inside the upper bits, as set bits.
#[inline(always)]
fn zeros_in(word: u64, w: usize, upper_bits: usize) -> u64 {
    let start = w.saturating_mul(64);
    if start >= upper_bits {
        return 0;
    }
    !word & low_mask(upper_bits - start)
}

/// Which bits a sample counts.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Bit {
    One,
    Zero,
}

/// Calls `each(rank, position)` for every [`QUANTUM`]-th one (or zero) of the upper bits, from
/// the first: the hints. One home for the encoder, which writes them, and `open`, which checks
/// them. One pass over the words; at most one sample a word (`QUANTUM > 64`).
fn samples<E>(
    word: impl Fn(usize) -> u64,
    upper_bits: usize,
    bit: Bit,
    mut each: impl FnMut(usize, usize) -> Result<(), E>,
) -> Result<(), E> {
    let (mut seen, mut next, mut rank) = (0usize, 0usize, 0usize);
    for w in 0..upper_bits.div_ceil(64) {
        let bits = match bit {
            Bit::One => word(w),
            Bit::Zero => zeros_in(word(w), w, upper_bits),
        };
        let here = ones(bits);
        if next < seen + here {
            each(rank, w * 64 + select_in_word(bits, next - seen))?;
            rank += 1;
            next += QUANTUM;
        }
        seen += here;
    }
    Ok(())
}

/// Appends `ids`, strictly increasing and each below `universe`, as a sorted id list in
/// `out`'s allocator; refused with nothing written past what a refusal names. The bytes
/// written. Equal ids and universe give equal bytes. A caller with an iterator collects it
/// into a slice first: the low bits are packed by index.
pub fn encode_sorted_ids(ids: &[u64], universe: u64, out: &mut AVec<'_, u8>) -> Result<usize, BuildError> {
    let n = ids.len();
    if n > MOST_IDS {
        return Err(BuildError::Enc(EncError::TooLong { what: "a sorted id list", len: n }));
    }
    let mut previous: Option<u64> = None;
    for (index, &id) in ids.iter().enumerate() {
        if id >= universe {
            return Err(BuildError::NotBelow { index, id, universe });
        }
        if let Some(previous) = previous {
            if id <= previous {
                return Err(BuildError::NotIncreasing { index, previous, id });
            }
        }
        previous = Some(id);
    }
    let w = low_width_for(n, universe);
    // Every id is below the universe and `n` at most `MOST_IDS`, so a high part is below `2n`
    // and fits a `usize`; one that does not is refused as too long, never cut.
    let too_long = || BuildError::Enc(EncError::TooLong { what: "a sorted id list's upper bits", len: n });
    let high = |id: u64| (id >> w).narrow::<usize>().map_err(|_| too_long());
    let upper_bits = match ids.last() {
        None => 0,
        Some(&last) => n.checked_add(high(last)?).and_then(|b| b.checked_add(1)).ok_or_else(too_long)?,
    };
    let layout = Layout::of(n, w, upper_bits).ok_or_else(too_long)?;
    let start = out.len();
    out.try_reserve(layout.total)?;
    put(out, &[KIND_SORTED_IDS, w])?;
    put(out, &len32(n, "a sorted id list")?.to_le_bytes())?;
    put(out, &universe.to_le_bytes())?;
    put(out, &len32(upper_bits, "a sorted id list's upper bits")?.to_le_bytes())?;
    // Reserved above: neither `resize` allocates.
    let upper_at = out.len();
    out.resize(upper_at + layout.upper_bytes + 4 * (layout.one_hints + layout.zero_hints), 0);
    let (upper, hints) = out[upper_at..].split_at_mut(layout.upper_bytes);
    for (index, &id) in ids.iter().enumerate() {
        let at = high(id)? + index;
        upper[at / 8] |= 1u8 << (at % 8).truncate_to::<u32>();
    }
    if layout.one_hints > 0 {
        let upper: &[u8] = upper;
        let word = |w: usize| word_at(upper, w * 8);
        for (bit, first) in [(Bit::One, 0), (Bit::Zero, 4 * layout.one_hints)] {
            samples(word, upper_bits, bit, |rank, at| {
                patch32(hints, first + 4 * rank, len32(at, "a sorted id list's upper bits")?);
                Ok::<(), EncError>(())
            })?;
        }
    }
    bits::pack(n, w, out, |j| ids[j] & bits::mask(w))?;
    put(out, &[0; PAD])?;
    debug_assert_eq!(out.len() - start, layout.total, "a sorted id list's layout and its bytes disagree");
    Ok(out.len() - start)
}

/// A sorted id list, checked once by [`SortedIds::open`]: reads through it check nothing
/// again and allocate nothing.
#[derive(Clone, Copy, Debug)]
pub struct SortedIds<'a> {
    n: usize,
    w: u8,
    universe: u64,
    upper_bits: usize,
    upper: &'a [u8],
    one_hints: &'a [u8],
    zero_hints: &'a [u8],
    /// The low bits and the pad after them, so `bits::get` of the last stays inside.
    low: &'a [u8],
}

fn corrupt(what: &'static str, at: usize) -> EncError {
    EncError::Corrupt { what, at }
}

impl<'a> SortedIds<'a> {
    /// The list that is all of `bytes`, checked whole (see the module).
    pub fn open(bytes: &'a [u8]) -> Result<SortedIds<'a>, EncError> {
        let Some(header) = bytes.first_chunk::<HEADER>() else {
            return Err(corrupt(SHORT, bytes.len()));
        };
        let [kind, w, n0, n1, n2, n3, u0, u1, u2, u3, u4, u5, u6, u7, b0, b1, b2, b3] = *header;
        if kind != KIND_SORTED_IDS {
            return Err(corrupt(NOT_IDS, 0));
        }
        let n = idx(u32::from_le_bytes([n0, n1, n2, n3]));
        let universe = u64::from_le_bytes([u0, u1, u2, u3, u4, u5, u6, u7]);
        let upper_bits = idx(u32::from_le_bytes([b0, b1, b2, b3]));
        if n > MOST_IDS || n.widen_to::<u64>() > universe {
            return Err(corrupt(COUNT, 2));
        }
        if w != low_width_for(n, universe) {
            return Err(corrupt(WIDTH, 1));
        }
        let fits = match n {
            0 => upper_bits == 0,
            // `universe` is at least `n`, so at least 1 here.
            _ => upper_bits > n && (upper_bits - n).widen_to::<u64>() <= ((universe - 1) >> w) + 1,
        };
        if !fits {
            return Err(corrupt(UPPER, 14));
        }
        let Some(layout) = Layout::of(n, w, upper_bits) else {
            return Err(corrupt(LENGTH, bytes.len()));
        };
        if bytes.len() != layout.total {
            return Err(corrupt(LENGTH, bytes.len().min(layout.total)));
        }
        let part = |from: usize, to: usize| bytes.get(from..to).ok_or(corrupt(LENGTH, from));
        let ids = SortedIds {
            n,
            w,
            universe,
            upper_bits,
            upper: part(layout.upper_at, layout.ones_at)?,
            one_hints: part(layout.ones_at, layout.zeros_at)?,
            zero_hints: part(layout.zeros_at, layout.low_at)?,
            low: part(layout.low_at, layout.total)?,
        };
        ids.check(&layout)?;
        Ok(ids)
    }

    /// The bytes a list of `n` ids below `universe` takes at most (its ids spread to the
    /// bound); `None` when no such list exists (`n` above `universe` or [`MOST_IDS`]).
    pub fn most_bytes(n: usize, universe: u64) -> Option<usize> {
        if n > MOST_IDS || n.widen_to::<u64>() > universe {
            return None;
        }
        let w = low_width_for(n, universe);
        let upper_bits = match n {
            0 => 0,
            _ => n.checked_add((((universe - 1) >> w) + 1).narrow::<usize>().ok()?)?,
        };
        Layout::of(n, w, upper_bits).map(|layout| layout.total)
    }

    /// Everything `open` promises past the header's counts, checked in one pass each.
    fn check(&self, layout: &Layout) -> Result<(), EncError> {
        // Unused bits zero, so equal ids give equal bytes and a word past the upper bits reads
        // empty.
        let upper_used = self.upper_bits % 8;
        if let Some(&last) = self.upper.last() {
            if upper_used != 0 && last >> upper_used.truncate_to::<u32>() != 0 {
                return Err(corrupt(PADDING, layout.ones_at - 1));
            }
        }
        let low_bits = self.n * usize::from(self.w);
        let low_bytes = low_bits.div_ceil(8);
        if low_bits % 8 != 0 {
            if let Some(&last) = self.low.get(low_bytes - 1) {
                if last >> (low_bits % 8).truncate_to::<u32>() != 0 {
                    return Err(corrupt(PADDING, layout.low_at + low_bytes - 1));
                }
            }
        }
        if let Some(at) = self.low.get(low_bytes..).and_then(|pad| pad.iter().position(|&b| b != 0)) {
            return Err(corrupt(PADDING, layout.low_at + low_bytes + at));
        }
        let words = self.upper_bits.div_ceil(64);
        let found: usize = (0..words).map(|w| ones(self.word(w))).sum();
        if found != self.n {
            return Err(corrupt(ONES, layout.upper_at));
        }
        // The last bit is the zero that ends the last bucket, the one before it the last id.
        if self.n > 0 && (self.bit(self.upper_bits - 1) || !self.bit(self.upper_bits - 2)) {
            return Err(corrupt(END, layout.upper_at + (self.upper_bits - 1) / 8));
        }
        let mut iter = self.iter();
        let mut previous: Option<u64> = None;
        for _ in 0..self.n {
            let Some((index, id)) = iter.step() else {
                return Err(corrupt(ONES, layout.upper_at));
            };
            if id >= self.universe || previous.is_some_and(|p| id <= p) {
                return Err(corrupt(ORDER, layout.low_at + index * usize::from(self.w) / 8));
            }
            previous = Some(id);
        }
        if layout.one_hints > 0 {
            for (bit, stored, count, at) in [
                (Bit::One, self.one_hints, layout.one_hints, layout.ones_at),
                (Bit::Zero, self.zero_hints, layout.zero_hints, layout.zeros_at),
            ] {
                let mut seen = 0;
                samples(|w| self.word(w), self.upper_bits, bit, |rank, position| {
                    let want = position.narrow::<u32>().ok();
                    if want.is_none() || u32_at(stored, 4 * rank) != want {
                        return Err(corrupt(HINT, at + 4 * rank));
                    }
                    seen += 1;
                    Ok(())
                })?;
                if seen != count {
                    return Err(corrupt(HINT, at + 4 * seen));
                }
            }
        }
        Ok(())
    }

    /// Ids in the list.
    pub fn len(&self) -> usize {
        self.n
    }

    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// The bound every id is below.
    pub fn universe(&self) -> u64 {
        self.universe
    }

    #[inline(always)]
    fn word(&self, w: usize) -> u64 {
        word_at(self.upper, w.saturating_mul(8))
    }

    #[inline(always)]
    fn bit(&self, at: usize) -> bool {
        (self.word(at / 64) >> (at % 64).truncate_to::<u32>()) & 1 == 1
    }

    /// Id `index` from its one's position `at` in the upper bits.
    #[inline(always)]
    fn value(&self, at: usize, index: usize) -> u64 {
        // `open` saw each one at or after its index: no id's high part is negative.
        (at.saturating_sub(index).widen_to::<u64>() << self.w) | bits::get(self.low, index, self.w)
    }

    /// The position of the one (or zero) of rank `rank` in the upper bits: from the sample
    /// before it (or from bit 0 in a list without hints), whole words counted, then a select
    /// within the word. At most the upper bits' words.
    fn select(&self, bit: Bit, rank: usize) -> Option<usize> {
        let hints = match bit {
            Bit::One => self.one_hints,
            Bit::Zero => self.zero_hints,
        };
        let bits_of = |w: usize| match bit {
            Bit::One => self.word(w),
            Bit::Zero => zeros_in(self.word(w), w, self.upper_bits),
        };
        let (mut w, mut rest, mut bits) = match hints.is_empty() {
            true => (0, rank, bits_of(0)),
            false => {
                let sample = rank / QUANTUM;
                let at = idx(u32_at(hints, 4 * sample)?);
                (at / 64, rank - sample * QUANTUM, bits_of(at / 64) & from_place(at))
            }
        };
        for _ in 0..self.upper_bits.div_ceil(64) {
            let here = ones(bits);
            if rest < here {
                return Some(w * 64 + select_in_word(bits, rest));
            }
            rest -= here;
            w += 1;
            bits = bits_of(w);
        }
        None
    }

    /// Id `index`, or `None` at or past `len`.
    pub fn get(&self, index: usize) -> Option<u64> {
        if index >= self.n {
            return None;
        }
        Some(self.value(self.select(Bit::One, index)?, index))
    }

    /// The first id not below `x`, and its index; `None` when every id is below `x`. The
    /// bucket of `x`'s high part starts after zero `high - 1` (one select on the zeros); its
    /// ids are then read forward, and the first past the bucket is above `x`.
    pub fn next_geq(&self, x: u64) -> Option<(usize, u64)> {
        if self.n == 0 || x >= self.universe {
            return None;
        }
        let buckets = self.upper_bits - self.n;
        let high = x >> self.w;
        if high >= buckets.widen_to::<u64>() {
            return None;
        }
        // Below `buckets`, a `usize`.
        let high = high.narrow::<usize>().ok()?;
        let start = match high {
            0 => 0,
            _ => self.select(Bit::Zero, high - 1)? + 1,
        };
        // Before `start` lie `high` zeros, so `start - high` ones: the first id's index.
        let mut ids = Iter::at(*self, start, start - high);
        for _ in 0..self.n {
            let (index, id) = ids.step()?;
            if id >= x {
                return Some((index, id));
            }
        }
        None
    }

    pub fn contains(&self, x: u64) -> bool {
        self.next_geq(x).is_some_and(|(_, id)| id == x)
    }

    /// The ids in order, decoded forward a word of the upper bits at a time.
    pub fn iter(&self) -> Iter<'a> {
        Iter::at(*self, 0, 0)
    }

    /// The ids in both lists, in order, by leapfrogging `next_geq` from one to the other.
    pub fn intersection<'b>(&self, other: &SortedIds<'b>) -> Intersection<'a, 'b> {
        Intersection { left: *self, right: *other, from: Some(0) }
    }

    /// Whether any id is in both lists: the first step of [`SortedIds::intersection`].
    pub fn intersects(&self, other: &SortedIds<'_>) -> bool {
        self.intersection(other).next().is_some()
    }
}

/// A list's ids in order ([`SortedIds::iter`]).
#[derive(Clone, Debug)]
pub struct Iter<'a> {
    ids: SortedIds<'a>,
    /// The upper word read, and its ones not yet taken.
    word: usize,
    bits: u64,
    index: usize,
}

impl<'a> Iter<'a> {
    /// Reading from bit `at` of the upper bits, whose next one is id `index`.
    fn at(ids: SortedIds<'a>, at: usize, index: usize) -> Iter<'a> {
        let word = at / 64;
        Iter { ids, word, bits: ids.word(word) & from_place(at), index }
    }

    /// The next id and its index.
    #[inline]
    fn step(&mut self) -> Option<(usize, u64)> {
        if self.index >= self.ids.n {
            return None;
        }
        let words = self.ids.upper_bits.div_ceil(64);
        // Bounded: the word grows to the last.
        while self.bits == 0 {
            self.word += 1;
            if self.word >= words {
                return None;
            }
            self.bits = self.ids.word(self.word);
        }
        let at = self.word * 64 + idx(self.bits.trailing_zeros());
        self.bits &= self.bits - 1;
        let index = self.index;
        self.index += 1;
        Some((index, self.ids.value(at, index)))
    }
}

impl Iterator for Iter<'_> {
    type Item = u64;

    fn next(&mut self) -> Option<u64> {
        self.step().map(|(_, id)| id)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let left = self.ids.n.saturating_sub(self.index);
        (left, Some(left))
    }
}

impl ExactSizeIterator for Iter<'_> {}

/// The ids two lists share, in order ([`SortedIds::intersection`]).
#[derive(Clone, Debug)]
pub struct Intersection<'a, 'b> {
    left: SortedIds<'a>,
    right: SortedIds<'b>,
    /// The least id not yet answered; `None` once done.
    from: Option<u64>,
}

impl Iterator for Intersection<'_, '_> {
    type Item = u64;

    fn next(&mut self) -> Option<u64> {
        let mut x = self.from?;
        // Each round that does not answer moves `x` to a larger id of `right`: at most its
        // length of rounds.
        for _ in 0..=self.right.len() {
            let found = self.left.next_geq(x).and_then(|(_, a)| self.right.next_geq(a).map(|(_, b)| (a, b)));
            let Some((a, b)) = found else {
                break;
            };
            if a == b {
                // `a` is below a universe bound, so `a + 1` does not overflow.
                self.from = a.checked_add(1);
                return Some(a);
            }
            x = b;
        }
        self.from = None;
        None
    }
}

#[cfg(test)]
mod tests {
    //! Each list against a plain sorted array as the oracle, over lists of the shapes id lists
    //! take (sparse, dense, clustered, runs, ids at 0 and at the bound); the bytes `open`
    //! refuses, each by name; the builder's refusals; the module doc's size table.

    use blaze_int::Int;
    use blaze_own::{AVec, Alloc, Vec};
    use blaze_sim::{Broke, Config, Tape};

    use super::super::EncError;
    use super::super::bits::{PAD, packed_len};
    use super::*;

    fn oracle(why: String) -> Broke {
        Broke::Oracle(why)
    }

    fn encode<'a>(ids: &[u64], universe: u64, alloc: Alloc<'a>) -> Result<AVec<'a, u8>, Broke> {
        let mut out = Vec::new_in(alloc);
        encode_sorted_ids(ids, universe, &mut out).map_err(|e| oracle(format!("encode of {} ids refused: {e}", ids.len())))?;
        Ok(out)
    }

    fn draw_universe(tape: &Tape) -> u64 {
        const SHAPES: [u64; 8] = [1, 2, 64, 1000, 1 << 20, 1 << 32, u64::MAX, 0];
        match *tape.one_of(&SHAPES, "universe") {
            0 => tape.int_in(1u64..=1 << 40, "any universe"),
            universe => universe,
        }
    }

    /// Ids below `universe` (at least 1) in one of the shapes id lists take, sorted, distinct.
    fn draw_ids(tape: &Tape, universe: u64) -> std::vec::Vec<u64> {
        let mut ids = std::vec::Vec::new();
        let shape = tape.below(5, "shape");
        for _ in 0..tape.int_in(0usize..=12, "pieces") {
            match shape {
                0 => {
                    for _ in 0..tape.int_in(0usize..=400, "sparse") {
                        ids.push(tape.below(universe, "id"));
                    }
                }
                // A run of consecutive ids: whole (3) or with ids dropped (1).
                1 | 3 => {
                    let start = tape.below(universe, "run start");
                    let len = tape.int_in(0u64..=1500, "run length").min(universe - start);
                    for k in 0..len {
                        if shape == 3 || !tape.one_in(3, "dropped") {
                            ids.push(start + k);
                        }
                    }
                }
                2 => {
                    let centre = tape.below(universe, "centre");
                    for _ in 0..tape.int_in(0usize..=300, "cluster") {
                        ids.push(centre.saturating_add(tape.below(4096, "offset")).min(universe - 1));
                    }
                }
                _ => {
                    ids.extend_from_slice(&[0, universe - 1]);
                    ids.push(tape.below(universe, "id"));
                }
            }
        }
        if tape.coin("at zero") {
            ids.push(0);
        }
        if tape.coin("at the bound") {
            ids.push(universe - 1);
        }
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    fn oracle_next_geq(ids: &[u64], x: u64) -> Option<(usize, u64)> {
        let i = ids.partition_point(|&v| v < x);
        ids.get(i).map(|&v| (i, v))
    }

    /// `ids` (and `other`, for the intersection) as lists: the same bytes twice, and every read
    /// equal to the array's.
    fn check_list(ids: &[u64], universe: u64, other: &[u64], alloc: Alloc<'_>) -> Result<(), Broke> {
        let bytes = encode(ids, universe, alloc)?;
        if encode(ids, universe, alloc)?.as_slice() != bytes.as_slice() {
            return Err(oracle(format!("deterministic: two encodes of {} ids differ", ids.len())));
        }
        if SortedIds::most_bytes(ids.len(), universe).is_none_or(|most| bytes.len() > most) {
            return Err(oracle(format!("most_bytes: {} ids took {} bytes, above the bound", ids.len(), bytes.len())));
        }
        let view = SortedIds::open(&bytes).map_err(|e| oracle(format!("open refused {} ids: {e}", ids.len())))?;
        if view.len() != ids.len() || view.iter().len() != ids.len() {
            return Err(oracle(format!("len: {} ids read as {}", ids.len(), view.len())));
        }
        if !view.iter().eq(ids.iter().copied()) {
            return Err(oracle(String::from("iteration: the ids read forward are not the array")));
        }
        if view.get(ids.len()).is_some() {
            return Err(oracle(String::from("get: an id past the end")));
        }
        let mut probes = std::vec::Vec::new();
        for (i, &id) in ids.iter().enumerate() {
            if view.get(i) != Some(id) {
                return Err(oracle(format!("get({i}) is {:?}, not {id}", view.get(i))));
            }
            probes.push(id);
            probes.push(id.saturating_add(1));
            probes.push(id.saturating_sub(1));
        }
        probes.extend_from_slice(&[0, universe, universe.saturating_sub(1), u64::MAX]);
        if let Some(&last) = ids.last() {
            probes.push(last.saturating_add(1));
        }
        for x in probes {
            let (got, want) = (view.next_geq(x), oracle_next_geq(ids, x));
            if got != want {
                return Err(oracle(format!("next_geq({x}) is {got:?}, not {want:?}")));
            }
            if view.contains(x) != ids.binary_search(&x).is_ok() {
                return Err(oracle(format!("contains({x}) is {}", view.contains(x))));
            }
        }
        let other_bytes = encode(other, universe, alloc)?;
        let other_view = SortedIds::open(&other_bytes).map_err(|e| oracle(format!("open refused the other list: {e}")))?;
        let want: std::vec::Vec<u64> = ids.iter().copied().filter(|id| other.binary_search(id).is_ok()).collect();
        let got: std::vec::Vec<u64> = view.intersection(&other_view).collect();
        if got != want {
            return Err(oracle(format!("intersection: {} ids, not {}", got.len(), want.len())));
        }
        if view.intersects(&other_view) == want.is_empty() || other_view.intersects(&view) == want.is_empty() {
            return Err(oracle(format!("intersects: says {} with {} shared", view.intersects(&other_view), want.len())));
        }
        Ok(())
    }

    /// The property: passes its 300 cases (the engine prints a tape on the first failure).
    #[test]
    fn sorted_ids_answer_as_a_sorted_array() {
        let alloc = blaze_own::testing::heap().alloc();
        blaze_sim::check(Config::cases(300), |tape: &Tape| {
            let universe = draw_universe(tape);
            let ids = draw_ids(tape, universe);
            let mut other: std::vec::Vec<u64> = ids.iter().copied().filter(|_| tape.coin("shared")).collect();
            other.extend(draw_ids(tape, universe));
            other.sort_unstable();
            other.dedup();
            check_list(&ids, universe, &other, alloc)
        });
    }

    /// The empty list (of the universe 0 too) and lists of one id, at 0 and at the bound.
    #[test]
    fn empty_and_one_element_lists_answer_as_arrays() {
        let alloc = blaze_own::testing::heap().alloc();
        let cases: [(&[u64], u64); 6] =
            [(&[], 0), (&[], 10), (&[0], 1), (&[5], 1 << 32), (&[u64::MAX - 1], u64::MAX), (&[(1 << 20) - 1], 1 << 20)];
        for (ids, universe) in cases {
            if let Err(broke) = check_list(ids, universe, ids, alloc) {
                panic!("{ids:?} below {universe}: {broke:?}");
            }
        }
    }

    /// The builder names the id it refuses.
    #[test]
    fn the_builder_refuses_an_id_by_name() {
        let alloc = blaze_own::testing::heap().alloc();
        let mut out = Vec::new_in(alloc);
        assert_eq!(
            encode_sorted_ids(&[3, 3], 10, &mut out),
            Err(BuildError::NotIncreasing { index: 1, previous: 3, id: 3 })
        );
        assert_eq!(
            encode_sorted_ids(&[5, 4], 10, &mut out),
            Err(BuildError::NotIncreasing { index: 1, previous: 5, id: 4 })
        );
        assert_eq!(encode_sorted_ids(&[0, 10], 10, &mut out), Err(BuildError::NotBelow { index: 1, id: 10, universe: 10 }));
        assert_eq!(encode_sorted_ids(&[0], 0, &mut out), Err(BuildError::NotBelow { index: 0, id: 0, universe: 0 }));
        assert!(out.is_empty(), "a refused list wrote {} bytes", out.len());
    }

    fn refused(bytes: &[u8]) -> &'static str {
        match SortedIds::open(bytes) {
            Err(EncError::Corrupt { what, .. }) => what,
            Err(other) => panic!("refused, but not as corrupt: {other}"),
            Ok(_) => "opened",
        }
    }

    /// `open` refuses each fault by its name: a short payload, another form number, a width,
    /// counts that do not match the payload, a padding bit, a lost one, ids out of order, a hint.
    #[test]
    fn open_refuses_each_fault_by_name() {
        let alloc = blaze_own::testing::heap().alloc();
        let n = 1000usize;
        let ids: std::vec::Vec<u64> = (0..n).map(|i| i.widen_to::<u64>() * 37 + (i % 5).widen_to::<u64>()).collect();
        let good = encode(&ids, 1 << 20, alloc).unwrap_or_else(|e| panic!("{e:?}"));
        let good = good.as_slice();
        assert_eq!(refused(good), "opened");
        let with = |change: &dyn Fn(&mut std::vec::Vec<u8>)| {
            let mut bytes = good.to_vec();
            change(&mut bytes);
            refused(&bytes)
        };
        assert_eq!(refused(&good[..10]), SHORT);
        assert_eq!(with(&|b| b[0] = 9), NOT_IDS);
        assert_eq!(with(&|b| b[1] ^= 1), WIDTH);
        // 1,001 ids in the header: the same width, another length.
        assert_eq!(with(&|b| b[2] = b[2].wrapping_add(1)), LENGTH);
        assert_eq!(with(&|b| drop(b.pop())), LENGTH);
        assert_eq!(with(&|b| b.push(0)), LENGTH);
        assert_eq!(with(&|b| *b.last_mut().unwrap_or_else(|| panic!("empty")) = 1), PADDING);
        // Id 0's one is bit 0 of the upper bits.
        assert_eq!(with(&|b| b[HEADER] &= !1), ONES);
        // Id 1 (38, bucket 0) with its low bits (10..20) cleared: 0, not above id 0.
        let low_at = good.len() - PAD - packed_len(n, 10);
        assert_eq!(
            with(&|b| {
                b[low_at + 1] &= 0b11;
                b[low_at + 2] &= !0x0F;
            }),
            ORDER
        );
        // The first one hint (id 0's one, at bit 0), right after the upper bits.
        let upper_bytes = (n + (ids[n - 1] >> 10).narrow::<usize>().unwrap_or(0) + 1).div_ceil(8);
        assert_eq!(with(&|b| b[HEADER + upper_bytes] ^= 1), HINT);
    }

    /// The module doc's table, byte for byte, and `most_bytes` equal to it: ids spread to the
    /// bound, `floor((i + 1) * u / n) - 1`.
    #[test]
    fn sizes_are_the_module_docs_table() {
        let alloc = blaze_own::testing::heap().alloc();
        let rows: [(u64, usize, usize); 6] = [
            (1 << 20, 10, 50),
            (1 << 20, 1000, 1561),
            (1 << 20, 100_000, 70_022),
            (1 << 32, 10, 65),
            (1 << 32, 1000, 3061),
            (1 << 32, 100_000, 220_022),
        ];
        for (universe, n, want) in rows {
            let ids: std::vec::Vec<u64> = (0..n)
                .map(|i| {
                    let spread = (i + 1).widen_to::<u128>() * u128::from(universe) / n.widen_to::<u128>();
                    spread.truncate_to::<u64>() - 1
                })
                .collect();
            let bytes = encode(&ids, universe, alloc).unwrap_or_else(|e| panic!("{e:?}"));
            assert_eq!(bytes.len(), want, "{n} ids below {universe}");
            assert_eq!(SortedIds::most_bytes(n, universe), Some(want), "most_bytes of {n} below {universe}");
        }
    }
}
