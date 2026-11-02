//! The round trip every column encoding must make exactly (docs/v2/plan/design/data.md,
//! "Compression follows the columns"): hot fixed-width, warm frame-of-reference bit-packing,
//! cold run-length, delta and dictionary codecs. [`round_trips`] runs an encoder and decoder
//! over [`shapes`], the columns those encodings are chosen for and their edges, and names the
//! first that does not come back as it went in; [`reads_in_place`] checks the tiers read
//! without decoding element by element. The owning lanes call them from their codecs' tests
//! (format's encodings in blaze-format; store's tier moves, a record moved hot to warm to cold
//! and back).

use blaze_ids::radix;

use crate::Rng;

/// Columns of `u32` by what they look like, from `seed`: empty and single, constant, runs (tag
/// columns), sorted with repeats and strictly increasing (CSR offsets, sorted relocations and
/// keys), each narrow range (`u8`, `u16`, one below and at a power of two), a frame far from
/// zero, the extremes, and uniform over all of `u32`.
pub fn shapes(seed: u64) -> Vec<(&'static str, Vec<u32>)> {
    let mut rng = Rng::new(seed);
    // alloc: a test fixture, made once a codec's test; only tests call this module.
    let mut random = |n: usize, below: u64| -> Vec<u32> { (0..n).map(|_| rng.below(below) as u32).collect() };
    let lengths = [1, 7, 64, 127, 128, 129, 1000, 4096 + 3];
    // alloc: as above.
    let mut out: Vec<(&'static str, Vec<u32>)> = vec![("empty", vec![]), ("single", vec![42])];
    for &n in &lengths {
        for (name, value) in [("constant", 7), ("zeros", 0), ("max", u32::MAX)] {
            // alloc: as above.
            out.push((name, vec![value; n]));
        }
        out.push(("u8 range", random(n, 1 << 8)));
        out.push(("u16 range", random(n, 1 << 16)));
        out.push(("below a power of two", random(n, (1 << 13) - 1)));
        out.push(("one bit", random(n, 2)));
        out.push(("u32 range", random(n, 1 << 32)));
        let base = random(1, 1 << 32)[0].saturating_sub(1 << 12);
        // alloc: as above.
        out.push(("frame far from zero", random(n, 1 << 12).into_iter().map(|v| base + v).collect()));
        let mut runs = Vec::with_capacity(n);
        while runs.len() < n {
            let (tag, len) = (random(1, 16)[0], random(1, 40)[0] as usize + 1);
            runs.extend(std::iter::repeat_n(tag, len.min(n - runs.len())));
        }
        out.push(("runs", runs));
        let mut sorted = random(n, 1 << 20);
        radix::sort(&mut sorted);
        out.push(("sorted with repeats", sorted));
        let mut offsets = Vec::with_capacity(n);
        let mut at = 0u32;
        for step in random(n, 9) {
            offsets.push(at);
            at += step;
        }
        out.push(("CSR offsets", offsets));
        let mut increasing = random(n, 1000);
        let mut sum = 0u32;
        for d in &mut increasing {
            sum += *d + 1;
            *d = sum;
        }
        out.push(("strictly increasing", increasing));
        // alloc: as above.
        out.push(("alternating extremes", (0..n).map(|i| if i % 2 == 0 { 0 } else { u32::MAX }).collect()));
    }
    out
}

/// `Ok` when `get(encode(column), i) == column[i]` for every index of every one of
/// [`shapes`]`(seed)`: an encoding read in place (hot fixed-width, warm frame of reference)
/// gives each element without decoding the column. Else the first shape, length and index read
/// otherwise.
pub fn reads_in_place<E, G, Bytes>(encode: E, get: G, seed: u64) -> Result<usize, String>
where
    E: Fn(&[u32]) -> Bytes,
    G: Fn(&Bytes, usize) -> u32,
{
    let all = shapes(seed);
    for (name, column) in &all {
        let encoded = encode(column);
        if let Some(at) = (0..column.len()).find(|&i| get(&encoded, i) != column[i]) {
            // alloc: error path, and the test fails.
            return Err(format!(
                "{name} of {}: element {at} reads as {} in place, not {}",
                column.len(),
                get(&encoded, at),
                column[at]
            ));
        }
    }
    Ok(all.len())
}

/// `Ok` when `decode(encode(column)) == column` for every one of [`shapes`]`(seed)`; else the
/// first shape and length that came back otherwise, and the first index that differs.
pub fn round_trips<E, D, Bytes, Error>(encode: E, decode: D, seed: u64) -> Result<usize, String>
where
    E: Fn(&[u32]) -> Bytes,
    D: Fn(&Bytes) -> Result<Vec<u32>, Error>,
    Error: std::fmt::Display,
{
    let all = shapes(seed);
    for (name, column) in &all {
        // alloc: error path, and the test fails.
        let back = decode(&encode(column)).map_err(|e| format!("{name} of {}: decoding failed: {e}", column.len()))?;
        if back != *column {
            let at = column.iter().zip(&back).position(|(a, b)| a != b).unwrap_or(column.len().min(back.len()));
            // alloc: error path, and the test fails.
            return Err(format!(
                "{name} of {}: came back with {} values, first apart at {at} ({:?} for {:?})",
                column.len(),
                back.len(),
                back.get(at),
                column.get(at)
            ));
        }
    }
    Ok(all.len())
}
