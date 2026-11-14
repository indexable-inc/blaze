//! Near-duplicates: functions whose shingles mostly agree. Each body of at least
//! [`NEAR_FLOOR`] tokens is read as its set of [`WIDTH`]-token shingles (locals by their class
//! alone, so a rename is no difference) and sketched by [`HASHES`] MinHash values (Broder);
//! locality-sensitive hashing over [`BANDS`] bands of [`ROWS`] picks the pairs worth comparing
//! (a pair agreeing on a fraction `s` of its shingles shares a band with probability
//! `1 - (1 - s^4)^8`: 0.98 at `s = 0.85`, 0.18 at `s = 0.5`), and the exact Jaccard index of the
//! two sets, at least [`JACCARD_FLOOR`], decides. Pure: the index feeds it the bodies.

use std::{
    alloc::Global,
    hash::{Hash as _, Hasher as _},
};

use blaze_hash::FixedHasher;

/// The fewest tokens a body is sketched at: below it a near-copy is a few lines that differ,
/// which `same_block` and `paste_mismatch` read better.
pub(crate) const NEAR_FLOOR: u32 = 80;
/// The tokens of a shingle: long enough that common idioms (`let x = y.z()?;` is 8) don't
/// match by themselves, short enough that one changed token spoils few.
pub(crate) const WIDTH: usize = 6;
/// The MinHash values of a sketch.
pub(crate) const HASHES: usize = 32;
/// The bands of the sketch, of [`ROWS`] values each.
pub(crate) const BANDS: usize = 8;
pub(crate) const ROWS: usize = HASHES / BANDS;
/// The fewest sketch values of [`HASHES`] two bodies agree on to be compared exactly: an
/// estimate of 0.7, under the exact floor by more than two standard errors (0.08 at 32).
pub(crate) const SKETCH_FLOOR: usize = 23;
/// The exact Jaccard index of two bodies' shingles a near-duplicate has, in percent: past it
/// a body differs from the other by a few statements in tens.
pub(crate) const JACCARD_FLOOR: u32 = 85;
/// The most bodies a band's bucket is read at: a bigger one is an idiom every body shares
/// (an empty `match` over one enum), not a copy, and would cost its square.
pub(crate) const BUCKET_CAP: usize = 32;

/// The MinHash sketch of a sorted set of shingles: per seed, the least of the shingles hashed
/// with it.
pub(crate) fn sketch(shingles: &[u64]) -> Box<[u32], Global> {
    let mut mins = [u32::MAX; HASHES];
    for &shingle in shingles {
        for (seed, min) in mins.iter_mut().enumerate() {
            let mut hasher = FixedHasher::with_seed(0x6e65_6172 ^ u64::try_from(seed).unwrap_or(0));
            shingle.hash(&mut hasher);
            let value = u32::try_from(hasher.finish() >> 32).unwrap_or(u32::MAX);
            if value < *min {
                *min = value;
            }
        }
    }
    let mut out = Vec::with_capacity_in(HASHES, Global);
    out.extend_from_slice(&mins);
    out.into_boxed_slice()
}

/// The key of band `band` of `sketch`.
pub(crate) fn band_key(sketch: &[u32], band: usize) -> u64 {
    let mut hasher = FixedHasher::with_seed(3);
    band.hash(&mut hasher);
    for value in sketch.iter().skip(band * ROWS).take(ROWS) {
        value.hash(&mut hasher);
    }
    hasher.finish()
}

/// How many values two sketches agree on.
pub(crate) fn agreement(a: &[u32], b: &[u32]) -> usize {
    a.iter().zip(b).filter(|(x, y)| x == y).count()
}

/// The Jaccard index of two sorted sets without repeats, in percent, rounded down.
pub(crate) fn jaccard(a: &[u64], b: &[u64]) -> u32 {
    let (mut i, mut j, mut both) = (0, 0, 0usize);
    while let (Some(x), Some(y)) = (a.get(i), b.get(j)) {
        match x.cmp(y) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                both += 1;
                i += 1;
                j += 1;
            }
        }
    }
    let either = a.len() + b.len() - both;
    if either == 0 {
        return 100;
    }
    u32::try_from(both * 100 / either).unwrap_or(0)
}

/// The most lines of a body the diff reads: past it the diff's table would be a large cost for
/// a message, and the message says the bodies differ without the lines.
const DIFF_LINES: usize = 400;
/// The most lines of a diff a message shows.
const DIFF_SHOWN: usize = 12;

/// The lines of `b` not in `a` (`+`) and of `a` not in `b` (`-`), by their longest common
/// subsequence, each trimmed, in the order of `b` with `a`'s interleaved where they fall; at
/// most [`DIFF_SHOWN`] lines, and `None` for bodies longer than [`DIFF_LINES`].
pub(crate) fn diff(a: &str, b: &str) -> Option<String> {
    let a: Vec<&str, Global> = lines(a);
    let b: Vec<&str, Global> = lines(b);
    if a.len() > DIFF_LINES || b.len() > DIFF_LINES {
        return None;
    }
    let (n, m) = (a.len(), b.len());
    // The LCS lengths of every pair of suffixes, row-major.
    let mut table: Vec<u32, Global> = Vec::with_capacity_in((n + 1) * (m + 1), Global);
    table.resize((n + 1) * (m + 1), 0);
    let at = |i: usize, j: usize| i * (m + 1) + j;
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            table[at(i, j)] = if a[i] == b[j] {
                table[at(i + 1, j + 1)] + 1
            } else {
                table[at(i + 1, j)].max(table[at(i, j + 1)])
            };
        }
    }
    let mut out = String::new(); // alloc: the message's diff, once per finding
    let (mut i, mut j, mut shown) = (0, 0, 0);
    let mut push = |sign: char, line: &str, out: &mut String| {
        if shown < DIFF_SHOWN {
            out.push_str("\n  ");
            out.push(sign);
            out.push(' ');
            out.push_str(line);
        }
        shown += 1;
    };
    while i < n || j < m {
        if i < n && j < m && a[i] == b[j] {
            i += 1;
            j += 1;
        } else if j < m && (i == n || table[at(i, j + 1)] >= table[at(i + 1, j)]) {
            push('+', b[j], &mut out);
            j += 1;
        } else {
            push('-', a[i], &mut out);
            i += 1;
        }
    }
    if shown > DIFF_SHOWN {
        out.push_str(&format!("\n  ... and {} more lines", shown - DIFF_SHOWN)); // alloc: the message's
    }
    Some(out)
}

/// The non-empty lines of `text`, trimmed.
fn lines(text: &str) -> Vec<&str, Global> {
    let mut out = Vec::new_in(Global);
    out.extend(text.lines().map(str::trim).filter(|it| !it.is_empty()));
    out
}
