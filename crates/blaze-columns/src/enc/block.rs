//! The schemes of one block of up to [`BLOCK`] integers (`u64`; any narrower column widens).
//! A block is a fixed-size directory [`Entry`] that holds its scheme and every parameter, and
//! a body that holds only packed values (columns.md, "Layout"), so a constant-time `get` is an
//! entry load and a value load, with no header parsed and no scheme to dispatch on: `For` is
//! `Linear` with a slope of 0, one formula, `base + slope * j + packed[j]`.
//!
//! | scheme | `base` | `slope` | `w` | `w2` | body | `get(j)` |
//! |---|---|---|---|---|---|---|
//! | `For` | min | 0 | width | 0 | `n` of `x - min` | O(1): one load |
//! | `Linear` | base | slope | width | 0 | `n` of `x - base - slope * j` | O(1): one load |
//! | `Rle` | value min | runs | start width | value width | `runs` starts, then `runs` of `value - min` | O(log runs): a binary search over the starts |
//! | `Delta` | first | least delta | width | 0 | `n - 1` of `delta - least` | O(j): a sum of the deltas before `j` |
//! | `Delta2` | first | first delta | width | 0 | least second delta (8), `n - 2` of `second delta - least` | O(j): two running sums |
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
use super::{EncError, put};

/// What `get(i)` costs a scheme, the chooser's bound (ordered: cheaper first).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum GetCost {
    /// One load of the block's entry and one of the value.
    Constant,
    /// A binary search over a block's runs: at most 10 loads.
    Log,
    /// A pass over the block's values before `j`: at most 1,023.
    Block,
}

/// A block's scheme, its entry's `scheme` byte.
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

/// Bytes of a directory entry: `base` (8), `slope` (8), `at` (4: the body's start, counted
/// from the first body's), `w`, `scheme`, `w2`, a zero byte.
pub(crate) const ENTRY: usize = 24;

/// The highest scheme id whose `get` is the line formula (`For` and `Linear`).
pub(crate) const LINE_MAX: u8 = 2;

/// One block's directory entry, as [`Entry::read`] reads it (`check` accepted it first).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) base: u64,
    pub(crate) slope: u64,
    /// The body's start in the column's bodies.
    pub(crate) at: usize,
    pub(crate) w: u8,
    pub(crate) scheme: u8,
    pub(crate) w2: u8,
}

impl Entry {
    #[inline(always)]
    pub(crate) fn read(e: &[u8; ENTRY]) -> Entry {
        // Constant ranges of a fixed array: the copies compile to plain loads.
        let (mut base, mut slope, mut at) = ([0u8; 8], [0u8; 8], [0u8; 4]);
        base.copy_from_slice(&e[0..8]);
        slope.copy_from_slice(&e[8..16]);
        at.copy_from_slice(&e[16..20]);
        Entry {
            base: u64::from_le_bytes(base),
            slope: u64::from_le_bytes(slope),
            at: blaze_int::idx(u32::from_le_bytes(at)),
            w: e[20],
            scheme: e[21],
            w2: e[22],
        }
    }

    /// The value at `j` of a `For` or `Linear` block, its body in `bodies`.
    #[inline(always)]
    pub(crate) fn line(&self, bodies: &[u8], j: usize) -> u64 {
        let packed = bits::get_bit(bodies, (self.at << 3) + j * usize::from(self.w), self.w);
        self.base.wrapping_add(self.slope.wrapping_mul(j.widen_to::<u64>())).wrapping_add(packed)
    }
}

/// The parameters of one block under one scheme: what its entry holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Plan {
    For { w: u8, min: u64 },
    Linear { w: u8, base: u64, slope: u64 },
    Rle { ws: u8, wv: u8, runs: usize, vmin: u64 },
    Delta { w: u8, first: u64, least: u64 },
    Delta2 { w: u8, first: u64, d1: u64, least: u64 },
}

/// The least of `values` as signed numbers, and the width of every value's distance above it.
fn signed_frame(values: impl Iterator<Item = u64> + Clone) -> (u64, u8) {
    let least = values.clone().map(u64::cast_signed).min().unwrap_or(0).cast_unsigned();
    let spread = values.map(|v| v.wrapping_sub(least)).max().unwrap_or(0);
    (least, bits::width_of(spread))
}

/// Bytes of the body of `n` values of `scheme` with widths `w` and `w2` (`runs` for `Rle`).
fn body_len(scheme: Scheme, n: usize, w: u8, w2: u8, runs: usize) -> usize {
    match scheme {
        Scheme::For | Scheme::Linear => bits::packed_len(n, w),
        Scheme::Rle => bits::packed_len(runs, w) + bits::packed_len(runs, w2),
        Scheme::Delta => bits::packed_len(n.saturating_sub(1), w),
        Scheme::Delta2 => 8 + bits::packed_len(n.saturating_sub(2), w),
    }
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

    /// The entry of this plan's block, its body at `at`.
    fn entry(&self, at: usize) -> Entry {
        let (base, slope, w, w2) = match *self {
            Plan::For { w, min } => (min, 0, w, 0),
            Plan::Linear { w, base, slope } => (base, slope, w, 0),
            Plan::Rle { ws, wv, runs, vmin } => (vmin, runs.widen_to::<u64>(), ws, wv),
            Plan::Delta { w, first, least } => (first, least, w, 0),
            Plan::Delta2 { w, first, d1, .. } => (first, d1, w, 0),
        };
        Entry { base, slope, at, w, scheme: self.scheme().id(), w2 }
    }

    /// Bytes the block of `n` values takes under this plan: its entry and its body.
    pub(crate) fn size(&self, n: usize) -> usize {
        ENTRY + self.body_size(n)
    }

    fn body_size(&self, n: usize) -> usize {
        let e = self.entry(0);
        let runs = if let Plan::Rle { runs, .. } = *self { runs } else { 0 };
        body_len(self.scheme(), n, e.w, e.w2, runs)
    }

    /// Writes the entry of this plan's block (its body at `at`) to `dir`, which is [`ENTRY`]
    /// bytes long.
    pub(crate) fn write_entry(&self, at: u32, dir: &mut [u8]) {
        let e = self.entry(0);
        let mut bytes = [0u8; ENTRY];
        bytes[0..8].copy_from_slice(&e.base.to_le_bytes());
        bytes[8..16].copy_from_slice(&e.slope.to_le_bytes());
        bytes[16..20].copy_from_slice(&at.to_le_bytes());
        bytes[20] = e.w;
        bytes[21] = e.scheme;
        bytes[22] = e.w2;
        dir.copy_from_slice(&bytes);
    }

    /// Appends the body of `values`' block under this plan (made by [`Plan::of`] of the same
    /// values).
    pub(crate) fn write_body(&self, values: &[u64], out: &mut AVec<'_, u8>) -> Result<(), EncError> {
        let n = values.len();
        out.try_reserve(self.body_size(n))?;
        match *self {
            Plan::For { w, min } => bits::pack(n, w, out, |j| values[j] - min),
            Plan::Linear { w, base, slope } => bits::pack(n, w, out, |j| {
                values[j].wrapping_sub(slope.wrapping_mul(j.widen_to::<u64>())).wrapping_sub(base)
            }),
            Plan::Rle { ws, wv, runs, vmin } => {
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
            Plan::Delta { w, least, .. } => {
                bits::pack(n.saturating_sub(1), w, out, |k| values[k + 1].wrapping_sub(values[k]).wrapping_sub(least))
            }
            Plan::Delta2 { w, least, .. } => {
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

/// Checks the entry `e` of a block of `n` values, its body in `bodies` (which runs to the
/// column's end, its padding included), and returns the body's size; what [`get`] and
/// [`decode`] read is then inside `bodies`. Fields a scheme does not use must be zero, so a
/// block has one encoding (rule 25). `at` names the entry's place in the column in an error.
pub(crate) fn check(e: &Entry, bodies: &[u8], n: usize, at: usize) -> Result<usize, EncError> {
    let corrupt = |what| EncError::Corrupt { what, at };
    let scheme = Scheme::of_id(e.scheme).ok_or(corrupt("an unknown block scheme"))?;
    if e.w > 64 || e.w2 > 64 {
        return Err(corrupt("a width past 64 bits"));
    }
    let unused = match scheme {
        Scheme::For => e.slope != 0 || e.w2 != 0,
        Scheme::Linear | Scheme::Delta | Scheme::Delta2 => e.w2 != 0,
        Scheme::Rle => e.slope == 0 || e.slope > n.widen_to::<u64>(),
    };
    if unused {
        return Err(corrupt("an entry field its scheme does not use, or a run count outside the block"));
    }
    let runs = if scheme == Scheme::Rle { blaze_int::idx(e.slope.truncate_to::<u32>()) } else { 0 };
    let size = body_len(scheme, n, e.w, e.w2, runs);
    // The packed values and the padding their last load reads.
    if e.at + size + bits::PAD > bodies.len() {
        return Err(corrupt("a block past the column's end"));
    }
    Ok(size)
}

/// Value `j` of the block whose entry `check` accepted, its body in `bodies`.
#[inline]
pub(crate) fn get(e: &Entry, bodies: &[u8], j: usize) -> u64 {
    if e.scheme <= LINE_MAX { e.line(bodies, j) } else { get_searched(e, bodies, j) }
}

/// [`get`] of the schemes that search or sum: out of line, so the line path stays small.
#[inline(never)]
fn get_searched(e: &Entry, bodies: &[u8], j: usize) -> u64 {
    let body = &bodies[e.at..];
    let w = e.w;
    match Scheme::of_id(e.scheme) {
        Some(Scheme::Rle) => {
            let (ws, wv) = (e.w, e.w2);
            let runs = blaze_int::idx(e.slope.truncate_to::<u32>());
            // The last run starting at or before `j`: run 0 starts at 0.
            let (mut lo, mut hi) = (0usize, runs);
            while hi - lo > 1 {
                let mid = lo + (hi - lo) / 2;
                if bits::get(body, mid, ws) <= j.widen_to::<u64>() {
                    lo = mid;
                } else {
                    hi = mid;
                }
            }
            if plant::on(Plant::RunSearchLeft) {
                lo = lo.saturating_sub(1);
            }
            let values = &body[bits::packed_len(runs, ws)..];
            e.base.wrapping_add(bits::get(values, lo, wv))
        }
        Some(Scheme::Delta) => {
            let (first, least) = (e.base, e.slope);
            let mut sum = 0u64;
            let take = if plant::on(Plant::DeltaGetSumsShort) { j.saturating_sub(1) } else { j };
            each_of_first(body, w, take, |d| sum = sum.wrapping_add(d));
            first.wrapping_add(least.wrapping_mul(j.widen_to::<u64>())).wrapping_add(sum)
        }
        Some(Scheme::Delta2) => {
            let (first, d1, least) = (e.base, e.slope, least_of(body));
            if j == 0 {
                return first;
            }
            let (mut x, mut d) = (first.wrapping_add(d1), d1);
            each_of_first(&body[8..], w, j - 1, |p| {
                d = d.wrapping_add(least).wrapping_add(p);
                x = x.wrapping_add(d);
            });
            x
        }
        Some(Scheme::For | Scheme::Linear) | None => e.line(bodies, j),
    }
}

/// A `Delta2` body's least second delta, its first 8 bytes (`check` saw them).
fn least_of(body: &[u8]) -> u64 {
    body.first_chunk::<8>().map_or(0, |b| u64::from_le_bytes(*b))
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

/// Fills `out` (the block's `n` values) from the block whose entry `check` accepted, its body
/// in `bodies`: the packed values by the vector [`bits::unpack`], or by the scalar one when
/// `vector` is false (the twin the tests hold equal).
pub(crate) fn decode(e: &Entry, bodies: &[u8], out: &mut [u64], vector: bool) {
    let n = out.len();
    let unpack = |data: &[u8], w: u8, out: &mut [u64]| {
        if vector { bits::unpack(data, w, out) } else { bits::unpack_scalar(data, w, out) }
    };
    let body = &bodies[e.at..];
    let w = e.w;
    match Scheme::of_id(e.scheme) {
        Some(Scheme::For) => {
            unpack(body, w, out);
            for v in out.iter_mut() {
                *v = v.wrapping_add(e.base);
            }
        }
        Some(Scheme::Linear) => {
            unpack(body, w, out);
            let mut line = e.base;
            for v in out.iter_mut() {
                *v = v.wrapping_add(line);
                line = line.wrapping_add(e.slope);
            }
        }
        Some(Scheme::Rle) => {
            let (ws, wv) = (e.w, e.w2);
            let runs = blaze_int::idx(e.slope.truncate_to::<u32>());
            let values = &body[bits::packed_len(runs, ws)..];
            // Starts out of order (bytes `check` cannot see into) fill nothing twice and
            // nothing past the block.
            let mut from = 0usize;
            for r in 0..runs {
                let end = if r + 1 < runs { usize::try_from(bits::get(body, r + 1, ws)).unwrap_or(n).clamp(from, n) } else { n };
                out[from..end].fill(e.base.wrapping_add(bits::get(values, r, wv)));
                from = end;
            }
        }
        Some(Scheme::Delta) => {
            let (first, least) = (e.base, e.slope);
            if n == 0 {
                return;
            }
            unpack(body, w, &mut out[1..]);
            out[0] = first;
            for k in 1..n {
                out[k] = out[k - 1].wrapping_add(least).wrapping_add(out[k]);
            }
        }
        Some(Scheme::Delta2) => {
            let (first, d1, least) = (e.base, e.slope, least_of(body));
            if n == 0 {
                return;
            }
            out[0] = first;
            if n == 1 {
                return;
            }
            unpack(&body[8..], w, &mut out[2..]);
            out[1] = first.wrapping_add(d1);
            let mut d = d1;
            for k in 2..n {
                d = d.wrapping_add(least).wrapping_add(out[k]);
                out[k] = out[k - 1].wrapping_add(d);
            }
        }
        None => out.fill(0),
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
    let plain = ENTRY + bits::packed_len(n, full);
    let mut best = plain;
    let mut above = 0usize;
    for b in (0..=usize::from(full)).rev() {
        // Values wider than `b`: the exceptions at this width.
        let bits = n * b + above * (10 + usize::from(full) - b);
        // Two bytes for the exception count beside `For`'s entry.
        best = best.min(ENTRY + 2 + bits.div_ceil(8));
        above += by_width[b];
    }
    (best, plain)
}
