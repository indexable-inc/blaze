//! libFuzzer's mutations (`FuzzerMutate.cpp`): bit and byte flips, inserts and erases, copies
//! of parts, integers in text and in binary, the target's dictionary, the operands of the
//! comparisons the last runs made, and a splice of two corpus inputs. Up to five stacked on a
//! copy of a corpus input, as libFuzzer's `MutateDepth`.

use blaze_ids::rng::SplitMix64;

use crate::raw;

/// Seeded: a run with the same seed and corpus makes the same inputs.
pub(crate) struct Random(SplitMix64);

impl Random {
    pub(crate) fn new(seed: u64) -> Self {
        Random(SplitMix64::new(seed))
    }

    pub(crate) fn next(&mut self) -> u64 {
        self.0.next_u64()
    }

    /// A number below `bound` (0 when `bound` is 0).
    pub(crate) fn below(&mut self, bound: usize) -> usize {
        self.0.index(bound)
    }

    /// A number below `bound` (0 when `bound` is 0).
    pub(crate) fn below_u64(&mut self, bound: u64) -> u64 {
        self.0.below(bound)
    }

    fn chance(&mut self, one_in: usize) -> bool {
        self.0.one_in(one_in as u64)
    }

    fn byte(&mut self) -> u8 {
        self.next() as u8
    }
}

/// The ways an input changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mutation {
    EraseBytes,
    InsertByte,
    InsertRepeatedBytes,
    ChangeByte,
    ChangeBit,
    ShuffleBytes,
    ChangeAsciiInteger,
    ChangeBinaryInteger,
    CopyPart,
    CrossOver,
    Dictionary,
    RecentCompare,
    RecentBytes,
}

pub(crate) const MUTATIONS: [Mutation; 13] = [
    Mutation::EraseBytes,
    Mutation::InsertByte,
    Mutation::InsertRepeatedBytes,
    Mutation::ChangeByte,
    Mutation::ChangeBit,
    Mutation::ShuffleBytes,
    Mutation::ChangeAsciiInteger,
    Mutation::ChangeBinaryInteger,
    Mutation::CopyPart,
    Mutation::CrossOver,
    Mutation::Dictionary,
    Mutation::RecentCompare,
    Mutation::RecentBytes,
];

/// Where the operands of recent comparisons come from: the instrumented run's tables, or none
/// (the tests, and a build without trace-cmp).
pub(crate) trait Recent {
    /// A comparison's operands and width in bytes (0: none in that slot).
    fn compare(&self, slot: usize) -> (u64, u64, usize);
    /// A byte comparison's operands and their length (0: none in that slot).
    fn bytes(&self, slot: usize) -> ([u8; raw::WORD], [u8; raw::WORD], usize);
}

/// The tables the comparison callbacks fill.
pub(crate) struct Traced;

impl Recent for Traced {
    fn compare(&self, slot: usize) -> (u64, u64, usize) {
        raw::recent_compare(slot)
    }

    fn bytes(&self, slot: usize) -> ([u8; raw::WORD], [u8; raw::WORD], usize) {
        raw::recent_bytes(slot)
    }
}

pub(crate) struct Mutator<'a, R> {
    pub(crate) random: Random,
    dictionary: &'a [&'a [u8]],
    recent: R,
}

impl<'a, R: Recent> Mutator<'a, R> {
    pub(crate) fn new(seed: u64, dictionary: &'a [&'a [u8]], recent: R) -> Self {
        Mutator { random: Random::new(seed), dictionary, recent }
    }

    /// Changes `data` by one to five mutations, never past `max_len` bytes (or its own length,
    /// when that is longer). `other` is the corpus input to cross over with.
    pub(crate) fn mutate(&mut self, data: &mut Vec<u8>, other: &[u8], max_len: usize) {
        let limit = max_len.max(data.len()).max(1);
        let depth = 1 + self.random.below(5);
        for _ in 0..depth {
            // A mutation that can't apply (an erase of nothing) gives way to another.
            for _ in 0..16 {
                let mutation = MUTATIONS[self.random.below(MUTATIONS.len())];
                if self.apply(mutation, data, other, limit) {
                    break;
                }
            }
        }
        data.truncate(limit);
    }

    /// One mutation; false when it doesn't apply to `data`.
    pub(crate) fn apply(&mut self, mutation: Mutation, data: &mut Vec<u8>, other: &[u8], limit: usize) -> bool {
        let r = &mut self.random;
        let len = data.len();
        match mutation {
            Mutation::EraseBytes => {
                if len < 2 {
                    return false;
                }
                let n = 1 + r.below(len / 2);
                let at = r.below(len - n + 1);
                data.drain(at..at + n);
            }
            Mutation::InsertByte => {
                if len >= limit {
                    return false;
                }
                let byte = r.byte();
                data.insert(r.below(len + 1), byte);
            }
            Mutation::InsertRepeatedBytes => {
                if len + 3 > limit {
                    return false;
                }
                let n = 3 + r.below((limit - len - 3).min(125) + 1);
                let byte = match r.below(3) {
                    0 => 0,
                    1 => 0xff,
                    _ => r.byte(),
                };
                let at = r.below(len + 1);
                data.splice(at..at, std::iter::repeat_n(byte, n));
            }
            Mutation::ChangeByte => {
                if len == 0 {
                    return false;
                }
                let at = r.below(len);
                data[at] = r.byte();
            }
            Mutation::ChangeBit => {
                if len == 0 {
                    return false;
                }
                let at = r.below(len);
                data[at] ^= 1 << r.below(8);
            }
            Mutation::ShuffleBytes => {
                if len < 2 {
                    return false;
                }
                let n = 2 + r.below(len.min(8) - 1);
                let at = r.below(len - n + 1);
                for i in (1..n).rev() {
                    let j = r.below(i + 1);
                    data.swap(at + i, at + j);
                }
            }
            Mutation::ChangeAsciiInteger => return ascii_integer(r, data, limit),
            Mutation::ChangeBinaryInteger => return binary_integer(r, data),
            Mutation::CopyPart => {
                if len == 0 {
                    return false;
                }
                let start = r.below(len);
                if r.chance(2) {
                    let n = 1 + r.below(len - start);
                    let at = r.below(len - n + 1);
                    data.copy_within(start..start + n, at);
                } else {
                    if len >= limit {
                        return false;
                    }
                    let n = 1 + r.below((len - start).min(limit - len));
                    let at = r.below(len + 1);
                    data.extend_from_within(start..start + n);
                    data[at..].rotate_right(n);
                }
            }
            Mutation::CrossOver => {
                if other.is_empty() {
                    return false;
                }
                if r.chance(3) {
                    // A splice: this input's head, the other's tail.
                    let head = r.below(len + 1);
                    let tail = r.below(other.len());
                    data.truncate(head);
                    data.extend_from_slice(&other[tail..other.len().min(tail + limit.saturating_sub(head))]);
                    return true;
                }
                return part_of(r, data, other, limit);
            }
            Mutation::Dictionary => {
                if self.dictionary.is_empty() {
                    return false;
                }
                let word = self.dictionary[r.below(self.dictionary.len())];
                return put_word(r, data, word, limit);
            }
            Mutation::RecentCompare => {
                let (a, b, width) = self.recent.compare(r.below(raw::TORC));
                if width == 0 || width > 8 {
                    return false;
                }
                let (from, to) = if r.chance(2) { (a, b) } else { (b, a) };
                let big = r.chance(2);
                let bytes = |value: u64| {
                    let le = value.to_le_bytes();
                    let mut word = [0u8; 8];
                    word[..width].copy_from_slice(&le[..width]);
                    if big {
                        word[..width].reverse();
                    }
                    word
                };
                let (from, to) = (bytes(from), bytes(to));
                return replace_or_put(r, data, &from[..width], &to[..width], limit);
            }
            Mutation::RecentBytes => {
                let (a, b, n) = self.recent.bytes(r.below(raw::TORC_WORDS));
                if n == 0 {
                    return false;
                }
                let (from, to) = if r.chance(2) { (a, b) } else { (b, a) };
                return replace_or_put(r, data, &from[..n], &to[..n], limit);
            }
        }
        true
    }
}

/// Where `from` is in `data` (from a random place on), `to` over it; else `to` put anywhere.
/// `from` and `to` are as long as each other.
fn replace_or_put(r: &mut Random, data: &mut Vec<u8>, from: &[u8], to: &[u8], limit: usize) -> bool {
    if !from.is_empty() && data.len() >= from.len() {
        let last = data.len() - from.len();
        let start = r.below(last + 1);
        if let Some(at) = find_from(data, from, start) {
            data[at..at + to.len()].copy_from_slice(to);
            return true;
        }
    }
    put_word(r, data, to, limit)
}

/// Where `from` (not empty) first is in `data` at or after `start`, else first before it;
/// `start` is at most `data.len() - from.len()`.
pub(crate) fn find_from(data: &[u8], from: &[u8], start: usize) -> Option<usize> {
    raw::__sanitizer_blaze_fuzzer_find(data, from, start)
}

/// `word` over bytes of `data`, or inserted into it.
fn put_word(r: &mut Random, data: &mut Vec<u8>, word: &[u8], limit: usize) -> bool {
    if word.is_empty() {
        return false;
    }
    if data.len() >= word.len() && r.chance(2) {
        let at = r.below(data.len() - word.len() + 1);
        data[at..at + word.len()].copy_from_slice(word);
        return true;
    }
    if data.len() + word.len() > limit {
        return false;
    }
    let at = r.below(data.len() + 1);
    data.splice(at..at, word.iter().copied());
    true
}

/// Bytes of `from` copied over a part of `data` or inserted into it.
fn part_of(r: &mut Random, data: &mut Vec<u8>, from: &[u8], limit: usize) -> bool {
    let len = data.len();
    if from.is_empty() {
        return false;
    }
    let start = r.below(from.len());
    if len > 0 && r.chance(2) {
        let n = 1 + r.below((from.len() - start).min(len));
        let at = r.below(len - n + 1);
        data[at..at + n].copy_from_slice(&from[start..start + n]);
        return true;
    }
    if len >= limit {
        return false;
    }
    let n = 1 + r.below((from.len() - start).min(limit - len));
    let at = r.below(len + 1);
    data.splice(at..at, from[start..start + n].iter().copied());
    true
}

/// A run of ASCII digits, changed as a number: plus or minus one, halved, doubled, or another.
fn ascii_integer(r: &mut Random, data: &mut Vec<u8>, limit: usize) -> bool {
    if data.is_empty() {
        return false;
    }
    let from = r.below(data.len());
    let Some(start) = (from..data.len()).find(|&at| data[at].is_ascii_digit()) else {
        return false;
    };
    let end = (start..data.len()).find(|&at| !data[at].is_ascii_digit()).unwrap_or(data.len());
    let value = data[start..end].iter().fold(0u64, |n, &d| n.wrapping_mul(10).wrapping_add((d - b'0') as u64));
    let value = match r.below(5) {
        0 => value.wrapping_add(1),
        1 => value.wrapping_sub(1),
        2 => value / 2,
        3 => value.wrapping_mul(2),
        _ => r.next() % 1_000_000,
    };
    let mut buffer = [0u8; 20];
    let digits = decimal(value, &mut buffer);
    if data.len() - (end - start) + digits.len() > limit {
        return false;
    }
    data.splice(start..end, digits.iter().copied());
    true
}

/// `value` in decimal, written into the end of `buffer` (a `u64` has at most 20 digits).
pub(crate) fn decimal(mut value: u64, buffer: &mut [u8; 20]) -> &[u8] {
    let mut at = buffer.len();
    loop {
        at -= 1;
        buffer[at] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            return &buffer[at..];
        }
    }
}

/// An integer of 1, 2, 4 or 8 bytes, either byte order, moved by up to 10 or negated.
fn binary_integer(r: &mut Random, data: &mut [u8]) -> bool {
    let width = 1 << r.below(4);
    if data.len() < width {
        return false;
    }
    let at = r.below(data.len() - width + 1);
    let big = r.chance(2);
    let mut word = [0u8; 8];
    word[..width].copy_from_slice(&data[at..at + width]);
    if big {
        word[..width].reverse();
    }
    let value = u64::from_le_bytes(word);
    let value = if r.chance(4) {
        value.wrapping_neg()
    } else {
        value.wrapping_add((r.below(21) as u64).wrapping_sub(10))
    };
    let mut word = value.to_le_bytes();
    if big {
        word[..width].reverse();
    }
    data[at..at + width].copy_from_slice(&word[..width]);
    true
}
