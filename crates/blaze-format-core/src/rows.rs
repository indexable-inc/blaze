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
#![blaze::sans_io]

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
///
/// And for keys of any lengths, one way: `prefix(a) < prefix(b)` implies `a < b` in the bytes'
/// (lexicographic) order. At the first of the eight places where the zero-padded words differ,
/// either both keys have a byte there and `a`'s is the smaller, or `a` has ended (its pad is 0)
/// and `b` has a byte above 0 there, so `a` is a proper prefix of `b`; `b` ending first cannot
/// make its pad the larger. So [`order_by_prefix`] is the bytes' order with the bytes compared
/// only where the words tie.
#[inline]
pub fn prefix(key: &[u8]) -> u64 {
    if let Some(head) = key.first_chunk::<8>() {
        return u64::from_be_bytes(*head);
    }
    let mut word = [0u8; 8];
    word[..key.len()].copy_from_slice(key);
    u64::from_be_bytes(word)
}

/// [`prefix`] of `bytes[start..start + len]`, which lies inside `bytes`: read as one eight-byte
/// load where `bytes` holds eight bytes from `start` (a short key's bytes past its end masked
/// off), so a name in a string table costs no byte loop and no call. Panics where the range is
/// not inside `bytes`, as indexing does.
#[inline]
pub fn prefix_in(bytes: &[u8], start: usize, len: usize) -> u64 {
    let key = &bytes[start..start + len];
    if let Some(head) = key.first_chunk::<8>() {
        return u64::from_be_bytes(*head);
    }
    // `len` is below 8 here: the shift is below 64, and at 0 the mask keeps nothing.
    match bytes[start..].first_chunk::<8>() {
        Some(eight) => u64::from_be_bytes(*eight) & !(u64::MAX >> (8 * len)),
        None => prefix(key),
    }
}

/// `a.cmp(b)`, the bytes' order, with `a`'s [`prefix`] already in hand as `a_prefix` and `b`'s
/// as `b_prefix`: the words decide unless they tie, and only then are the bytes compared (see
/// [`prefix`] for why the two orders agree). For keys that are digests the words tie only on
/// the row sought, so a search over them calls `memcmp` once.
#[inline]
pub fn order_by_prefix(a_prefix: u64, a: &[u8], b_prefix: u64, b: &[u8]) -> Ordering {
    a_prefix.cmp(&b_prefix).then_with(|| a.cmp(b))
}

/// Estimates `distance * n / span` for a prefix distance in `0..=span` and a nonzero span.
/// Keeping the span's top 32 bits makes the product fit in a u64. When bits are dropped,
/// the shifted span is at least 2^31 and each operand loses less than one shifted unit.
/// The ratio changes by less than 1 / 2^31, so for n < 2^32 the integer estimates differ
/// by at most ceil(n / 2^32) + 1 rows.
#[inline]
pub fn interpolated_offset(distance: u64, span: u64, n: u32) -> u64 {
    debug_assert!(span != 0 && distance <= span);
    let shift = (u64::BITS - span.leading_zeros()).saturating_sub(u32::BITS);
    ((distance >> shift) * u64::from(n)) / (span >> shift)
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
            let distance = want - least;
            let count = hi - lo;
            // The inclusive span of the whole prefix space is 2^64. Its exact quotient
            // is the high half of the product, including for counts wider than u32.
            let offset = match (most - least).checked_add(1) {
                None => ((u128::from(distance) * count as u128) >> u64::BITS) as usize,
                Some(span) => match u32::try_from(count) {
                    Ok(n) => interpolated_offset(distance, span, n) as usize,
                    Err(_) => ((u128::from(distance) * count as u128) / u128::from(span)) as usize,
                },
            };
            lo + offset.min(count - 1)
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
        // The probe's prefix is wanted for the bounds anyway; it decides the order too, and the
        // bytes are compared only when it ties with the key's (`order_by_prefix`: the same order
        // as `probe.cmp(key)`, so the same probes and the same answer).
        let probe_prefix = prefix(probe);
        match order_by_prefix(probe_prefix, probe, want, key) {
            Ordering::Equal => return Ok(at),
            Ordering::Less => {
                lo = at + 1;
                least = probe_prefix;
                moved = Some(Ordering::Less);
            }
            Ordering::Greater => {
                hi = at;
                most = probe_prefix;
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

    /// A byte of a drawn key: few values, zero among them, so keys share prefixes, one ends
    /// where another has a zero byte, and words tie often.
    fn key_byte(tape: &blaze_sim::Tape) -> u8 {
        [0u8, 1, b'a', b'b', 0xff][tape.index(5, "byte")]
    }

    /// The prefix-first order is the bytes' order for keys of ANY two lengths (0 to 12 bytes),
    /// and `prefix_in` is `prefix` of the range at every start and length of a drawn buffer,
    /// with eight bytes after the range and without. The twin of the comparison `seek_from` and
    /// `ScopeView::get` make: the old one was `a.cmp(b)`. Prints nothing.
    #[test]
    fn the_prefix_order_is_the_bytes_order() {
        blaze_sim::check(Config::DEFAULT, |tape| {
            let a: Vec<u8> = tape.vec(0..=12, "a", key_byte);
            let b: Vec<u8> = if tape.coin("b from a") {
                let mut b: Vec<u8> = Vec::new();
                b.extend_from_slice(&a[..tape.index(a.len() + 1, "cut")]);
                b.extend(tape.vec(0..=4, "b tail", key_byte));
                b
            } else {
                tape.vec(0..=12, "b", key_byte)
            };
            assert_eq!(order_by_prefix(prefix(&a), &a, prefix(&b), &b), a.cmp(&b), "{a:x?} against {b:x?}");
            if prefix(&a) < prefix(&b) {
                assert!(a < b, "{a:x?} has the smaller prefix but is not below {b:x?}");
            }
            let buffer: Vec<u8> = tape.vec(0..=24, "buffer", key_byte);
            for start in 0..=buffer.len() {
                for len in 0..=buffer.len() - start {
                    assert_eq!(prefix_in(&buffer, start, len), prefix(&buffer[start..start + len]), "{start}+{len} of {buffer:x?}");
                }
            }
        });
    }

    /// A planted comparison that drops the tie-break (words alone) must be caught by the order
    /// oracle above: keys of one prefix and different tails differ under it. Prints nothing.
    #[test]
    fn the_prefix_order_oracle_catches_a_missing_tie_break() {
        let (a, b) = (*b"abcdefgh1", *b"abcdefgh2");
        assert_eq!(order_by_prefix(prefix(&a), &a, prefix(&b), &b), Ordering::Less);
        assert_ne!(prefix(&a).cmp(&prefix(&b)), a.cmp(&b), "the plant: words alone tie here");
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
