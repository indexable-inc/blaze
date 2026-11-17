//! Lookup in fixed-width rows sorted by a digest key, read in place: the one home for it (rule
//! 30). A record that keeps rows sorted by a key that is a hash (a content id, an item key, a
//! store entry's key) needs no index bytes to be found in: the keys are spread evenly, so the
//! position of a key is predicted from its value.
//!
//! [`seek_from`] interpolates on the keys' first eight bytes (big-endian, so their order is the
//! bytes' order), keeping for the range left the least and the most prefix its rows can hold,
//! both learnt from probes: the first guess interpolates over the whole key space, so no row is
//! read but the ones probed (a cold run's first and last pages are not touched). After
//! [`INTERPOLATIONS`] guesses it bisects. So a lookup is never worse than a binary search plus a
//! constant: at most [`most_probes`]`(n) = INTERPOLATIONS + ceil(log2(n + 1))` probes, whatever
//! the keys (clustered keys, one 8-byte prefix shared by all rows).
//!
//! The floor on keys spread evenly: the first guess misses by about `sqrt(n) / 2` rows (a key's
//! rank given its value is binomial), each later guess by about the square root of the error
//! before it, so a lookup over a million rows takes some five probes, and they land on two pages
//! (the first guess's and the key's) where a binary search touches some twenty rows over about
//! ten pages. No comparison lookup without index bytes does better in probes; a perfect hash
//! would spend about 3 bits a key to make it one.
//!
//! [`Seeker`] seeks a batch of keys in ascending order, each from where the one before it ended,
//! so the rows' pages are touched in ascending order and a page the keys share is probed once.

use core::cmp::Ordering;

/// The interpolated guesses a lookup makes before it bisects.
/// The last of them is a fence when the range is still long (see [`seek_from`]).
pub const INTERPOLATIONS: u32 = 6;

/// How far past the last guess the fence probes, in rows: the last guesses miss by a row or
/// two on keys spread evenly (each by about the square root of the miss before it).
const FENCE: usize = 16;

/// The most probes a lookup among `n` rows makes: [`INTERPOLATIONS`] guesses, then a bisection
/// of what is left, at most `ceil(log2(n + 1))` probes.
pub const fn most_probes(n: usize) -> u32 {
    INTERPOLATIONS + (usize::BITS - n.leading_zeros())
}

/// A key's first eight bytes, big-endian, zeros past a shorter key's end: monotone in the
/// bytes' order for keys of one length.
#[inline]
pub fn prefix(key: &[u8]) -> u64 {
    let mut word = [0u8; 8];
    let take = key.len().min(8);
    word[..take].copy_from_slice(&key[..take]);
    u64::from_be_bytes(word)
}

/// The key of row `i` of `rows`: `key_len` bytes `key_at` bytes into it, rows `width` bytes
/// each.
#[inline]
fn key_in(rows: &[u8], width: usize, key_at: usize, key_len: usize, i: usize) -> &[u8] {
    let at = i * width + key_at;
    &rows[at..at + key_len]
}

/// The row of `rows` whose key is `key` (`Ok`), or where a row of that key would go (`Err`):
/// rows `width` bytes each, sorted by the `key.len()` bytes at `key_at` in each. `rows.len()` is
/// a multiple of `width` and `key_at + key.len() <= width`, which the record's one check
/// established. Two rows of one key: either is found.
pub fn find_row(rows: &[u8], width: usize, key_at: usize, key: &[u8]) -> Result<usize, usize> {
    debug_assert!(width > 0 && rows.len().is_multiple_of(width) && key_at + key.len() <= width);
    let n = rows.len() / width;
    seek_from(0, n, 0, key, |i| key_in(rows, width, key_at, key.len(), i))
}

/// `key`'s position among rows `lo..n` (`Ok`), or where it would go (`Err`), the `i`th row's key
/// read by `key_of` (which returns exactly `key.len()` bytes): what [`find_row`] and [`Seeker`]
/// answer with, for a caller whose rows are not one slice (a test counting the rows it reads,
/// pages fetched one by one). Every row of `lo..n` has a key of prefix at least `lo_prefix`,
/// which is at most `key`'s. At most [`most_probes`]`(n - lo)` calls of `key_of`.
pub fn seek_from<'a>(
    lo: usize,
    n: usize,
    lo_prefix: u64,
    key: &[u8],
    mut key_of: impl FnMut(usize) -> &'a [u8],
) -> Result<usize, usize> {
    let want = prefix(key);
    debug_assert!(lo_prefix <= want, "a seek from prefix {lo_prefix:#x} for a key of {want:#x}");
    let (mut lo, mut hi) = (lo, n);
    // Every row of `lo..hi` has a prefix in `least..=most`, and `least <= want <= most`.
    let (mut least, mut most) = (lo_prefix.min(want), u64::MAX);
    let mut guesses = 0;
    // The side the last probe moved, `Less` (`lo` moved up to it) or `Greater`.
    let mut moved = None;
    while lo < hi {
        let at = if guesses + 1 < INTERPOLATIONS && most > least {
            guesses += 1;
            // `want - least <= most - least < span`, so the guess is in `lo..hi`.
            let span = u128::from(most - least) + 1;
            lo + ((u128::from(want - least) * (hi - lo) as u128) / span) as usize
        } else if guesses + 1 == INTERPOLATIONS && hi - lo > 2 * FENCE && moved.is_some() {
            // The last guess's slot is a fence: guesses that all landed on one side of the key
            // leave the other end of the range where it began (the run's end, a whole run
            // away), and bisecting that reads a page a probe. The key is within a few rows of
            // the last guess, so a probe `FENCE` rows past it, on the side not yet bounded
            // near, brackets it; the bisection then stays on that guess's page.
            guesses += 1;
            match moved {
                Some(Ordering::Less) => lo + FENCE,
                _ => hi - 1 - FENCE,
            }
        } else {
            lo + (hi - lo) / 2
        };
        let probe = key_of(at);
        match probe.cmp(key) {
            Ordering::Equal => return Ok(at),
            Ordering::Less => {
                lo = at + 1;
                least = prefix(probe);
                moved = Some(Ordering::Less);
            }
            Ordering::Greater => {
                hi = at;
                most = prefix(probe);
                moved = Some(Ordering::Greater);
            }
        }
    }
    Err(lo)
}

/// Seeks keys given in ascending order among `n` rows, each from where the one before ended:
/// every row before that point has a key below the last one sought, so it is never probed again.
/// A key equal to the last one sought must not be sought again (after `Ok(i)` the next seek
/// starts at `i + 1`): the caller answers a repeat from the answer before.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Seeker {
    lo: usize,
    lo_prefix: u64,
}

impl Seeker {
    /// A seeker at the first row.
    pub const fn new() -> Seeker {
        Seeker { lo: 0, lo_prefix: 0 }
    }

    /// [`seek_from`] for `key` from where the last seek ended; `key` is above every key sought
    /// before. At most [`most_probes`]`(n)` calls of `key_of`.
    pub fn seek<'a>(&mut self, n: usize, key: &[u8], key_of: impl FnMut(usize) -> &'a [u8]) -> Result<usize, usize> {
        let answer = seek_from(self.lo.min(n), n, self.lo_prefix, key, key_of);
        self.lo = match answer {
            Ok(at) => at + 1,
            Err(at) => at,
        };
        self.lo_prefix = prefix(key);
        answer
    }
}

impl Default for Seeker {
    fn default() -> Seeker {
        Seeker::new()
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, vec::Vec};

    use blaze_sim::Config;

    use super::*;

    /// SplitMix64's `i`th output from `start`.
    fn mix(start: u64, i: u64) -> u64 {
        let mut z = start.wrapping_add((i + 1).wrapping_mul(0x9e37_79b9_7f4a_7c15));
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Rows of `WIDTH` bytes, a `KEY`-byte key `AT` bytes in, a row's other bytes its index.
    const WIDTH: usize = 40;
    const AT: usize = 4;
    const KEY: usize = 16;

    fn rows_of(keys: &[[u8; KEY]]) -> Vec<u8> {
        let mut rows = Vec::with_capacity(keys.len() * WIDTH);
        for (i, key) in keys.iter().enumerate() {
            let mut row = [0xa5u8; WIDTH];
            row[..AT].copy_from_slice(&(i as u32).to_le_bytes());
            row[AT..AT + KEY].copy_from_slice(key);
            rows.extend_from_slice(&row);
        }
        rows
    }

    /// The oracle: a plain binary search's insertion point.
    fn insertion(keys: &[[u8; KEY]], key: &[u8; KEY]) -> usize {
        keys.partition_point(|k| k < key)
    }

    /// One answer held to the oracle, and the probe bound.
    fn holds(keys: &[[u8; KEY]], rows: &[u8], key: &[u8; KEY]) {
        let mut probes = 0u32;
        let got = seek_from(0, keys.len(), 0, key, |i| {
            probes += 1;
            &rows[i * WIDTH + AT..i * WIDTH + AT + KEY]
        });
        let at = insertion(keys, key);
        match got {
            Ok(i) => assert_eq!(&keys[i], key, "found row {i} for {key:x?}"),
            Err(i) => {
                assert_eq!(i, at, "insertion point of {key:x?}");
                assert!(at == keys.len() || &keys[at] != key, "{key:x?} is at {at} and was not found");
            }
        }
        assert!(probes <= most_probes(keys.len()), "{probes} probes among {} rows", keys.len());
        assert_eq!(find_row(rows, WIDTH, AT, key), got);
    }

    /// A key of `shape`: spread evenly, clustered near a base, or one 8-byte prefix with the
    /// rest drawn.
    fn draw_key(tape: &blaze_sim::Tape, shape: usize, base: u64) -> [u8; KEY] {
        let mut key = [0u8; KEY];
        let (head, tail) = match shape {
            0 => (tape.next_u64("head"), tape.next_u64("tail")),
            1 => (base.wrapping_add(tape.int_in(0..=64u64, "near")), tape.next_u64("tail")),
            _ => (base, tape.int_in(0..=40u64, "tail")),
        };
        key[..8].copy_from_slice(&head.to_be_bytes());
        key[8..].copy_from_slice(&tail.to_be_bytes());
        key
    }

    /// Every answer is the binary search's, within the bound: keys spread evenly, clustered (which
    /// defeats interpolation), sharing one 8-byte prefix, repeated; tables of none and one row;
    /// keys below the first and above the last. Prints nothing.
    #[test]
    fn answers_are_binary_searchs_within_the_bound() {
        blaze_sim::check(Config::DEFAULT, |tape| {
            let shape = tape.index(3, "shape");
            let base = tape.next_u64("base");
            let mut keys: Vec<[u8; KEY]> = tape.vec(0..=400, "rows", |tape| draw_key(tape, shape, base));
            keys.sort_unstable();
            let rows = rows_of(&keys);
            for key in &keys {
                holds(&keys, &rows, key);
            }
            let sought: Vec<[u8; KEY]> = tape.vec(0..=40, "sought", |tape| draw_key(tape, shape, base));
            for key in sought.iter().chain([&[0u8; KEY], &[0xffu8; KEY]]) {
                holds(&keys, &rows, key);
            }
        });
    }

    /// A batch sought in ascending order by one [`Seeker`] answers what a lookup a key does, for
    /// rows of distinct keys. Prints nothing.
    #[test]
    fn a_seeker_answers_what_lookups_do() {
        blaze_sim::check(Config::DEFAULT, |tape| {
            let shape = tape.index(3, "shape");
            let base = tape.next_u64("base");
            let set: BTreeSet<[u8; KEY]> = tape.vec(0..=300, "rows", |tape| draw_key(tape, shape, base)).into_iter().collect();
            let keys: Vec<[u8; KEY]> = set.into_iter().collect();
            let rows = rows_of(&keys);
            let mut sought: Vec<[u8; KEY]> = tape.vec(0..=60, "sought", |tape| draw_key(tape, shape, base));
            sought.extend(keys.iter().step_by(3).copied());
            sought.sort_unstable();
            sought.dedup();
            let mut seeker = Seeker::new();
            for key in &sought {
                let got = seeker.seek(keys.len(), key, |i| &rows[i * WIDTH + AT..i * WIDTH + AT + KEY]);
                assert_eq!(got, find_row(&rows, WIDTH, AT, key), "{key:x?}");
            }
        });
    }

    /// Over 1,000,000 rows of 48 bytes with keys spread evenly, the rows a lookup reads and the
    /// 16 KiB pages they are on, for 20,000 held keys and 20,000 absent: every lookup within
    /// [`most_probes`], under 3 pages a lookup on average (the floor: the guess's page and the
    /// key's), under 8 probes (expected about 5; a binary search makes 20). Prints `held: P
    /// probes, G pages a lookup; absent: P probes, G pages a lookup`.
    #[test]
    fn a_million_even_keys_take_few_probes() {
        const ROW: usize = 48;
        const PAGE: usize = 16 << 10;
        let n = 1_000_000u64;
        let mut keys: Vec<[u8; 32]> = (0..n)
            .map(|i| {
                let mut key = [0u8; 32];
                for w in 0..4 {
                    key[w * 8..w * 8 + 8].copy_from_slice(&mix(w as u64, i).to_be_bytes());
                }
                key
            })
            .collect();
        keys.sort_unstable();
        let mut rows = Vec::with_capacity(keys.len() * ROW);
        for key in &keys {
            rows.extend_from_slice(key);
            rows.extend_from_slice(&[0u8; ROW - 32]);
        }
        let count = keys.len();
        let measure = |sought: &mut dyn Iterator<Item = [u8; 32]>, held: bool| {
            let (mut probes, mut pages, mut lookups) = (0u64, 0u64, 0u64);
            for key in sought {
                let mut touched = BTreeSet::new();
                let mut here = 0u32;
                let got = seek_from(0, count, 0, &key, |i| {
                    here += 1;
                    touched.insert(i * ROW / PAGE);
                    &rows[i * ROW..i * ROW + 32]
                });
                assert_eq!(got.is_ok(), held, "{key:x?}");
                assert!(here <= most_probes(count), "{here} probes");
                probes += u64::from(here);
                pages += touched.len() as u64;
                lookups += 1;
            }
            (probes as f64 / lookups as f64, pages as f64 / lookups as f64)
        };
        let (held_probes, held_pages) = measure(&mut keys.iter().step_by(50).copied(), true);
        let (absent_probes, absent_pages) = measure(
            &mut (0..20_000u64).map(|i| {
                let mut key = [0u8; 32];
                for w in 0..4 {
                    key[w * 8..w * 8 + 8].copy_from_slice(&mix(100 + w as u64, i).to_be_bytes());
                }
                key
            }),
            false,
        );
        std::println!(
            "held: {held_probes:.2} probes, {held_pages:.2} pages a lookup; absent: {absent_probes:.2} probes, {absent_pages:.2} pages a lookup"
        );
        assert!(held_pages < 3.0 && absent_pages < 3.0, "pages a lookup: {held_pages:.2} held, {absent_pages:.2} absent");
        assert!(held_probes < 8.0 && absent_probes < 8.0, "probes a lookup: {held_probes:.2} held, {absent_probes:.2} absent");
    }

    /// The bound is a binary search's plus the guesses. Prints nothing.
    #[test]
    fn the_bound() {
        assert_eq!(most_probes(0), INTERPOLATIONS);
        assert_eq!(most_probes(1), INTERPOLATIONS + 1);
        assert_eq!(most_probes(1_000_000), INTERPOLATIONS + 20);
        assert_eq!(prefix(&[1, 2]), 0x0102_0000_0000_0000);
    }
}
