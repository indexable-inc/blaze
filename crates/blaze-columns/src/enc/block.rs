//! The schemes of one block of up to [`BLOCK`] integers (`u64`; any narrower column widens).
//! A block's first byte is its [`Scheme`]; the header after it is fixed-size, little-endian,
//! and the packed values follow it (columns.md, "Layout"):
//!
//! | scheme | header after the scheme byte | packed | `get(j)` |
//! |---|---|---|---|
//! | `For` | width, min (8) | `n` of `x - min` | O(1): one load |
//! | `Linear` | width, base (8), slope (8) | `n` of `x - base - slope * j` | O(1): one load |
//! | `Rle` | start width, value width, runs (2), value min (8) | `runs` starts, then `runs` of `value - min` | O(log runs): a binary search over the starts |
//! | `Delta` | width, first (8), least delta (8) | `n - 1` of `delta - least` | O(j): a sum of the deltas before `j` |
//! | `Delta2` | width, first (8), first delta (8), least second delta (8) | `n - 2` of `second delta - least` | O(j): two running sums |
//!
//! Arithmetic is wrapping (mod 2^64) throughout, so every block decodes exactly whatever its
//! values; the "least" of a delta is the least as a signed number, so a column that falls as
//! well as rises packs small. Frame of reference and delta are Goldstein, Ramakrishnan and
//! Shaft (ICDE 1998) and the column stores after it; the frame against a line is the linear
//! model of LeCo (Liu et al., SIGMOD 2024) in its simplest form, one line a block from its
//! first and last value; run-length is the classic one with a searchable run index.

use blaze_int::Int;
use blaze_own::AVec;

use super::bits::{self, BLOCK};
use super::plant::{self, Plant};
use super::{EncError, put, read8, read64};

/// What `get(i)` costs a scheme, the chooser's bound (ordered: cheaper first).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum GetCost {
    /// One load of the value and the block's header.
    Constant,
    /// A binary search over a block's runs: at most 10 loads.
    Log,
    /// A pass over the block's values before `j`: at most 1,023.
    Block,
}

/// A block's scheme, its first byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Scheme {
    For,
    Linear,
    Rle,
    Delta,
    Delta2,
}

impl Scheme {
    /// Every scheme, in the order the chooser breaks a tie by.
    pub const ALL: [Scheme; 5] = [Scheme::For, Scheme::Linear, Scheme::Rle, Scheme::Delta, Scheme::Delta2];

    pub const fn id(self) -> u8 {
        match self {
            Scheme::For => 1,
            Scheme::Linear => 2,
            Scheme::Rle => 3,
            Scheme::Delta => 4,
            Scheme::Delta2 => 5,
        }
    }

    pub const fn of_id(id: u8) -> Option<Scheme> {
        match id {
            1 => Some(Scheme::For),
            2 => Some(Scheme::Linear),
            3 => Some(Scheme::Rle),
            4 => Some(Scheme::Delta),
            5 => Some(Scheme::Delta2),
            _ => None,
        }
    }

    pub const fn cost(self) -> GetCost {
        match self {
            Scheme::For | Scheme::Linear => GetCost::Constant,
            Scheme::Rle => GetCost::Log,
            Scheme::Delta | Scheme::Delta2 => GetCost::Block,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Scheme::For => "for",
            Scheme::Linear => "linear",
            Scheme::Rle => "rle",
            Scheme::Delta => "delta",
            Scheme::Delta2 => "delta2",
        }
    }
}

/// The parameters of one block under one scheme: what its header holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Plan {
    For { w: u8, min: u64 },
    Linear { w: u8, base: u64, slope: u64 },
    Rle { ws: u8, wv: u8, runs: usize, vmin: u64 },
    Delta { w: u8, first: u64, least: u64 },
    Delta2 { w: u8, first: u64, d1: u64, least: u64 },
}

const FOR_HEAD: usize = 10;
const LINEAR_HEAD: usize = 18;
const RLE_HEAD: usize = 13;
const DELTA_HEAD: usize = 18;
const DELTA2_HEAD: usize = 26;

/// The least of `values` as signed numbers, and the width of every value's distance above it.
fn signed_frame(values: impl Iterator<Item = u64> + Clone) -> (u64, u8) {
    let least = values.clone().map(u64::cast_signed).min().unwrap_or(0).cast_unsigned();
    let spread = values.map(|v| v.wrapping_sub(least)).max().unwrap_or(0);
    (least, bits::width_of(spread))
}

impl Plan {
    /// The plan of `values` (1 to [`BLOCK`] of them) under `scheme`.
    pub(crate) fn of(scheme: Scheme, values: &[u64]) -> Plan {
        let n = values.len();
        match scheme {
            Scheme::For => {
                let min = values.iter().copied().min().unwrap_or(0);
                let max = values.iter().copied().max().unwrap_or(0);
                Plan::For { w: bits::width_of(max - min), min }
            }
            Scheme::Linear => {
                let slope = linear_slope(values);
                let (base, w) = signed_frame(
                    values.iter().enumerate().map(move |(j, &v)| v.wrapping_sub(slope.wrapping_mul(j.widen_to::<u64>()))),
                );
                Plan::Linear { w, base, slope }
            }
            Scheme::Rle => {
                let runs = 1 + values.windows(2).filter(|p| p[0] != p[1]).count();
                let last_start = values.windows(2).rposition(|p| p[0] != p[1]).map_or(0, |at| at + 1);
                let vmin = values.iter().copied().min().unwrap_or(0);
                let vmax = values.iter().copied().max().unwrap_or(0);
                Plan::Rle { ws: bits::width_of(last_start.widen_to::<u64>()), wv: bits::width_of(vmax - vmin), runs, vmin }
            }
            Scheme::Delta => {
                let first = values.first().copied().unwrap_or(0);
                let (least, w) = signed_frame(values.windows(2).map(|p| p[1].wrapping_sub(p[0])));
                Plan::Delta { w, first, least }
            }
            Scheme::Delta2 => {
                let first = values.first().copied().unwrap_or(0);
                let d1 = if n > 1 { values[1].wrapping_sub(values[0]) } else { 0 };
                let (least, w) = signed_frame(
                    values.windows(3).map(|t| t[2].wrapping_sub(t[1]).wrapping_sub(t[1].wrapping_sub(t[0]))),
                );
                Plan::Delta2 { w, first, d1, least }
            }
        }
    }

    pub(crate) fn scheme(&self) -> Scheme {
        match self {
            Plan::For { .. } => Scheme::For,
            Plan::Linear { .. } => Scheme::Linear,
            Plan::Rle { .. } => Scheme::Rle,
            Plan::Delta { .. } => Scheme::Delta,
            Plan::Delta2 { .. } => Scheme::Delta2,
        }
    }

    /// Bytes the block of `n` values takes under this plan.
    pub(crate) fn size(&self, n: usize) -> usize {
        match *self {
            Plan::For { w, .. } => FOR_HEAD + bits::packed_len(n, w),
            Plan::Linear { w, .. } => LINEAR_HEAD + bits::packed_len(n, w),
            Plan::Rle { ws, wv, runs, .. } => RLE_HEAD + bits::packed_len(runs, ws) + bits::packed_len(runs, wv),
            Plan::Delta { w, .. } => DELTA_HEAD + bits::packed_len(n.saturating_sub(1), w),
            Plan::Delta2 { w, .. } => DELTA2_HEAD + bits::packed_len(n.saturating_sub(2), w),
        }
    }

    /// Appends the block of `values` under this plan (made by [`Plan::of`] of the same values).
    pub(crate) fn write(&self, values: &[u64], out: &mut AVec<'_, u8>) -> Result<(), EncError> {
        let n = values.len();
        out.try_reserve(self.size(n))?;
        put(out, &[self.scheme().id()])?;
        match *self {
            Plan::For { w, min } => {
                put(out, &[w])?;
                put(out, &min.to_le_bytes())?;
                bits::pack(n, w, out, |j| values[j] - min)
            }
            Plan::Linear { w, base, slope } => {
                put(out, &[w])?;
                put(out, &base.to_le_bytes())?;
                put(out, &slope.to_le_bytes())?;
                bits::pack(n, w, out, |j| {
                    values[j].wrapping_sub(slope.wrapping_mul(j.widen_to::<u64>())).wrapping_sub(base)
                })
            }
            Plan::Rle { ws, wv, runs, vmin } => {
                put(out, &[ws, wv])?;
                put(out, &runs.truncate_to::<u16>().to_le_bytes())?;
                put(out, &vmin.to_le_bytes())?;
                // Run `r` starts at `starts[r]`; the runs are where the value changes.
                let mut starts = [0u16; BLOCK];
                let mut r = 1;
                for j in 1..n {
                    if values[j] != values[j - 1] {
                        starts[r] = j.truncate_to::<u16>();
                        r += 1;
                    }
                }
                bits::pack(runs, ws, out, |r| u64::from(starts[r]))?;
                bits::pack(runs, wv, out, |r| values[usize::from(starts[r])] - vmin)
            }
            Plan::Delta { w, first, least } => {
                put(out, &[w])?;
                put(out, &first.to_le_bytes())?;
                put(out, &least.to_le_bytes())?;
                bits::pack(n.saturating_sub(1), w, out, |k| values[k + 1].wrapping_sub(values[k]).wrapping_sub(least))
            }
            Plan::Delta2 { w, first, d1, least } => {
                put(out, &[w])?;
                put(out, &first.to_le_bytes())?;
                put(out, &d1.to_le_bytes())?;
                put(out, &least.to_le_bytes())?;
                bits::pack(n.saturating_sub(2), w, out, |k| {
                    let (a, b, c) = (values[k], values[k + 1], values[k + 2]);
                    c.wrapping_sub(b).wrapping_sub(b.wrapping_sub(a)).wrapping_sub(least)
                })
            }
        }
    }
}

/// One line through a block: the slope from its first value to its last, rounded toward zero,
/// as a wrapping `u64`.
fn linear_slope(values: &[u64]) -> u64 {
    match (values.first(), values.last()) {
        (Some(&first), Some(&last)) if values.len() > 1 => {
            let rise = last.widen_to::<i128>() - first.widen_to::<i128>();
            let run = (values.len() - 1).widen_to::<u64>().widen_to::<i128>();
            (rise / run).truncate_to::<i64>().cast_unsigned()
        }
        _ => 0,
    }
}

/// Checks the header of the block of `n` values at the start of `block` (which runs to the
/// column's end, its padding included) and returns its size; what [`get`] and [`decode`] read
/// is then inside `block`. `at` names the block's place in the column in an error.
pub(crate) fn check(block: &[u8], n: usize, at: usize) -> Result<usize, EncError> {
    let corrupt = |what| EncError::Corrupt { what, at };
    let scheme = Scheme::of_id(read8(block, 0, "a block's scheme")?).ok_or(corrupt("an unknown block scheme"))?;
    let width = |w: u8| if w > 64 { Err(corrupt("a width past 64 bits")) } else { Ok(w) };
    let size = match scheme {
        Scheme::For => FOR_HEAD + bits::packed_len(n, width(read8(block, 1, "a width")?)?),
        Scheme::Linear => LINEAR_HEAD + bits::packed_len(n, width(read8(block, 1, "a width")?)?),
        Scheme::Rle => {
            let ws = width(read8(block, 1, "a width")?)?;
            let wv = width(read8(block, 2, "a width")?)?;
            let runs = usize::from(u16::from_le_bytes([read8(block, 3, "runs")?, read8(block, 4, "runs")?]));
            if runs == 0 || runs > n {
                return Err(corrupt("a run count outside the block"));
            }
            RLE_HEAD + bits::packed_len(runs, ws) + bits::packed_len(runs, wv)
        }
        Scheme::Delta => DELTA_HEAD + bits::packed_len(n.saturating_sub(1), width(read8(block, 1, "a width")?)?),
        Scheme::Delta2 => DELTA2_HEAD + bits::packed_len(n.saturating_sub(2), width(read8(block, 1, "a width")?)?),
    };
    // The packed values and the padding their last load reads.
    if size + bits::PAD > block.len() {
        return Err(corrupt("a block past the column's end"));
    }
    Ok(size)
}

/// Value `j` of the block `check` accepted at the start of `block`.
#[inline]
pub(crate) fn get(block: &[u8], j: usize) -> u64 {
    // `check` read every header byte below: the reads cannot fail.
    let h64 = |at: usize| read64(block, at, "").unwrap_or(0);
    let w = block[1];
    match block[0] {
        1 => h64(2).wrapping_add(bits::get(&block[FOR_HEAD..], j, w)),
        2 => {
            let (base, slope) = (h64(2), h64(10));
            base.wrapping_add(slope.wrapping_mul(j.widen_to::<u64>())).wrapping_add(bits::get(&block[LINEAR_HEAD..], j, w))
        }
        3 => {
            let (ws, wv) = (block[1], block[2]);
            let runs = usize::from(u16::from_le_bytes([block[3], block[4]]));
            let starts = &block[RLE_HEAD..];
            // The last run starting at or before `j`: run 0 starts at 0.
            let (mut lo, mut hi) = (0usize, runs);
            while hi - lo > 1 {
                let mid = lo + (hi - lo) / 2;
                if bits::get(starts, mid, ws) <= j.widen_to::<u64>() {
                    lo = mid;
                } else {
                    hi = mid;
                }
            }
            if plant::on(Plant::RunSearchLeft) {
                lo = lo.saturating_sub(1);
            }
            let values = &block[RLE_HEAD + bits::packed_len(runs, ws)..];
            h64(5).wrapping_add(bits::get(values, lo, wv))
        }
        4 => {
            let (first, least) = (h64(2), h64(10));
            let mut sum = 0u64;
            let take = if plant::on(Plant::DeltaGetSumsShort) { j.saturating_sub(1) } else { j };
            each_of_first(&block[DELTA_HEAD..], w, take, |d| sum = sum.wrapping_add(d));
            first.wrapping_add(least.wrapping_mul(j.widen_to::<u64>())).wrapping_add(sum)
        }
        _ => {
            let (first, d1, least) = (h64(2), h64(10), h64(18));
            if j == 0 {
                return first;
            }
            let (mut x, mut d) = (first.wrapping_add(d1), d1);
            each_of_first(&block[DELTA2_HEAD..], w, j - 1, |p| {
                d = d.wrapping_add(least).wrapping_add(p);
                x = x.wrapping_add(d);
            });
            x
        }
    }
}

/// Calls `each` on the first `take` values of width `w` packed at the start of `data`, in
/// order, unpacking them 64 at a time into the stack (a chunk of 64 starts on a byte).
#[inline]
fn each_of_first(data: &[u8], w: u8, take: usize, mut each: impl FnMut(u64)) {
    const CHUNK: usize = 64;
    let mut chunk = [0u64; CHUNK];
    let mut from = 0;
    while from < take {
        let m = (take - from).min(CHUNK);
        bits::unpack(&data[from * usize::from(w) / 8..], w, &mut chunk[..m]);
        for &v in &chunk[..m] {
            each(v);
        }
        from += m;
    }
}

/// Fills `out` (the block's `n` values) from the block `check` accepted at the start of
/// `block`: the packed values by the vector [`bits::unpack`], or by the scalar one when
/// `vector` is false (the twin the tests hold equal).
pub(crate) fn decode(block: &[u8], out: &mut [u64], vector: bool) {
    let n = out.len();
    let unpack = |data: &[u8], w: u8, out: &mut [u64]| {
        if vector { bits::unpack(data, w, out) } else { bits::unpack_scalar(data, w, out) }
    };
    let h64 = |at: usize| read64(block, at, "").unwrap_or(0);
    let w = block[1];
    match block[0] {
        1 => {
            let min = h64(2);
            unpack(&block[FOR_HEAD..], w, out);
            for v in out.iter_mut() {
                *v = v.wrapping_add(min);
            }
        }
        2 => {
            let (base, slope) = (h64(2), h64(10));
            unpack(&block[LINEAR_HEAD..], w, out);
            let mut line = base;
            for v in out.iter_mut() {
                *v = v.wrapping_add(line);
                line = line.wrapping_add(slope);
            }
        }
        3 => {
            let (ws, wv) = (block[1], block[2]);
            let runs = usize::from(u16::from_le_bytes([block[3], block[4]]));
            let vmin = h64(5);
            let starts = &block[RLE_HEAD..];
            let values = &block[RLE_HEAD + bits::packed_len(runs, ws)..];
            // Starts out of order (bytes `check` cannot see into) fill nothing twice and
            // nothing past the block.
            let mut from = 0usize;
            for r in 0..runs {
                let end = if r + 1 < runs { usize::try_from(bits::get(starts, r + 1, ws)).unwrap_or(n).clamp(from, n) } else { n };
                out[from..end].fill(vmin.wrapping_add(bits::get(values, r, wv)));
                from = end;
            }
        }
        4 => {
            let (first, least) = (h64(2), h64(10));
            if n == 0 {
                return;
            }
            unpack(&block[DELTA_HEAD..], w, &mut out[1..]);
            out[0] = first;
            for k in 1..n {
                out[k] = out[k - 1].wrapping_add(least).wrapping_add(out[k]);
            }
        }
        _ => {
            let (first, d1, least) = (h64(2), h64(10), h64(18));
            if n == 0 {
                return;
            }
            out[0] = first;
            if n == 1 {
                return;
            }
            unpack(&block[DELTA2_HEAD..], w, &mut out[2..]);
            out[1] = first.wrapping_add(d1);
            let mut d = d1;
            for k in 2..n {
                d = d.wrapping_add(least).wrapping_add(out[k]);
                out[k] = out[k - 1].wrapping_add(d);
            }
        }
    }
}

/// What patched frame of reference (PFOR: Zukowski, Héman, Nes and Boncz, ICDE 2006) would
/// save on a block over plain frame of reference, estimated: the width `b` that minimizes
/// `n * b` plus, for each value that does not fit `b` bits, an exception of a 10-bit position
/// and its remaining high bits; the bytes it would take and the plain `For` bytes. Not a
/// scheme: the cost example prints it so the sample says whether exceptions pay.
pub fn pfor_estimate(values: &[u64]) -> (usize, usize) {
    let n = values.len();
    let min = values.iter().copied().min().unwrap_or(0);
    let max = values.iter().copied().max().unwrap_or(0);
    let full = bits::width_of(max - min);
    let mut by_width = [0usize; 65];
    for &v in values {
        by_width[usize::from(bits::width_of(v - min))] += 1;
    }
    let plain = FOR_HEAD + bits::packed_len(n, full);
    let mut best = plain;
    let mut above = 0usize;
    for b in (0..=usize::from(full)).rev() {
        // Values wider than `b`: the exceptions at this width.
        let bits = n * b + above * (10 + usize::from(full) - b);
        // A header byte for the exception count's place beside `For`'s.
        best = best.min(FOR_HEAD + 2 + bits.div_ceil(8));
        above += by_width[b];
    }
    (best, plain)
}
