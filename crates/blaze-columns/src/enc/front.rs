//! A SORTED set of distinct byte strings, front-coded in buckets of [`BUCKET`]: a bucket's
//! first string whole, each next one as the length of the prefix it shares with the one before
//! and the rest of its bytes. String `i` decodes from its bucket alone (at most 16 entries read,
//! no neighbour bucket touched); a lookup by text binary-searches the buckets' first strings
//! where they lie, then scans one bucket comparing only the bytes each entry adds. The form for
//! a table whose readers compare or order names: an interned set's index IS its rank, so two
//! names compare as two `u32`s once each is found ([`FrontView::find`]).
//!
//! # Layout
//!
//! Little-endian, unaligned, a pure function of the strings (rule 25):
//!
//! - kind (1 byte, [`KIND_FRONT`]: none of [`super::strs`]'s kinds, so a column of the other
//!   form is refused by its first byte), count `n` (`u32`), the longest string's length
//!   (`u32`), the buckets' bytes (`u32`): a 13-byte header;
//! - `ceil(n / 16)` bucket starts (`u32` each, from the buckets' first byte, strictly
//!   increasing, the first 0);
//! - the buckets: the first entry `len, bytes`; every next one `shared, rest_len, rest bytes`;
//!   every length an unsigned LEB128 of at most 5 bytes, minimal. A bucket ends where the next
//!   starts (the last at the buckets' end), with no byte between.
//!
//! `shared` is the longest common prefix with the string before (the writer writes exactly
//! that); [`FrontView::find`]'s scan is right because of it.
//!
//! # Size against the plain column
//!
//! A plain column ([`super::strs`], `Plain`) is each string's bytes and an offsets column; the
//! blaze-own `Texts` table is the bytes and 4 bytes a string. Front coding costs, a string,
//! the bytes it does not share, one or two length bytes (one each under 128), and a quarter
//! byte of bucket start; a bucket's first string is whole.
//!
//! - Paths, sorted (`core::iter::adapters::map::Map`, about 32 bytes, neighbours sharing up to
//!   the last `::`): about 4 bytes of rest and 2 of lengths an entry, a whole first string
//!   amortised over 16 (about 2 bytes), so about 8 bytes against 36 for `Texts`: near 22%.
//!   The test `a_path_corpus_front_codes_to_under_half_of_plain` holds it under 50% of plain.
//! - Bare identifiers, sorted (`into_iter`, `Map`, about 8 bytes, sharing 2 or 3 with their
//!   neighbour): about 5.5 bytes of rest and 2 of lengths, about 8 against 12: near 65%.
//!
//! # Where it loses
//!
//! - Unsorted strings, or strings that repeat: it cannot hold them (the writer refuses them by
//!   index); a dictionary ([`super::strs`]) or FSST does.
//! - Time, not bytes: `get` reads up to 16 entries' lengths and copies the pieces of the string
//!   from up to 16 places, where a plain column is two offset loads and a borrow; a string is
//!   never borrowed in place, always copied into the caller's buffer.
//! - A table of few strings: 13 header bytes and 4 a bucket, so one string of 3 bytes takes 22
//!   bytes where `Texts` takes 7.
//! - Strings that share nothing (hashes, random keys): the saving is only the 4-byte end down
//!   to 1 or 2 length bytes; FSST or a dictionary may do better where bytes repeat inside.

use blaze_int::Int;
use blaze_own::AVec;

use super::{EncError, len32, patch32, put, read8, read32};

/// Strings a bucket: the most entries `get` reads.
pub const BUCKET: usize = 16;

/// The column's kind byte.
pub const KIND_FRONT: u8 = 3;

/// The header: kind, count, longest, buckets' bytes.
const HEADER: usize = 13;

/// The most bytes of one LEB128 length (a `u32`).
const VARINT: usize = 5;

/// Why a front-coded column was not written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrontError {
    /// String `index` is not greater than the one before it: equal (`duplicate`) or less.
    NotIncreasing { index: usize, duplicate: bool },
    /// Room, or a count past `u32`.
    Enc(EncError),
}

impl From<EncError> for FrontError {
    fn from(error: EncError) -> FrontError {
        FrontError::Enc(error)
    }
}

impl core::fmt::Display for FrontError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            FrontError::NotIncreasing { index, duplicate: true } => write!(
                f,
                "blaze_columns::enc::front: string {index} equals the one before: a front-coded column holds each string once"
            ),
            FrontError::NotIncreasing { index, duplicate: false } => write!(
                f,
                "blaze_columns::enc::front: string {index} sorts before the one before: a front-coded column is sorted by bytes"
            ),
            FrontError::Enc(error) => write!(f, "{error}"),
        }
    }
}

impl core::error::Error for FrontError {}

/// The longest common prefix of `a` and `b`.
fn lcp(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

/// `v` as minimal LEB128: the bytes and how many.
fn varint(v: u32) -> ([u8; VARINT], usize) {
    let mut out = [0u8; VARINT];
    let mut v = v;
    let mut n = 0;
    while n < VARINT {
        let low = (v & 0x7f).truncate_to::<u8>();
        v >>= 7;
        if v == 0 {
            out[n] = low;
            n += 1;
            break;
        }
        out[n] = low | 0x80;
        n += 1;
    }
    (out, n)
}

fn put_varint(out: &mut AVec<'_, u8>, v: u32) -> Result<(), EncError> {
    let (bytes, n) = varint(v);
    put(out, &bytes[..n])
}

/// The LEB128 length at `*at`, before `end`: minimal, at most 5 bytes, a `u32`; else the
/// corruption, at `base + ` its byte.
fn read_varint(data: &[u8], at: &mut usize, end: usize, base: usize) -> Result<u32, EncError> {
    let corrupt = |what, off: usize| EncError::Corrupt { what, at: base + off };
    let mut v = 0u32;
    for i in 0..VARINT {
        if *at >= end {
            return Err(corrupt("a length past its bucket's end", *at));
        }
        let byte = data[*at];
        *at += 1;
        if i == VARINT - 1 && byte & 0xf0 != 0 {
            return Err(corrupt("a length past u32", *at - 1));
        }
        v |= u32::from(byte & 0x7f) << (7 * i);
        if byte & 0x80 == 0 {
            if i > 0 && byte == 0 {
                return Err(corrupt("a length not in its fewest bytes", *at - 1));
            }
            return Ok(v);
        }
    }
    Err(corrupt("a length past u32", *at))
}

/// The LEB128 length at `*at` of a column [`FrontView::open`] checked.
#[inline]
fn varint_at(data: &[u8], at: &mut usize) -> usize {
    let mut v = 0u32;
    for i in 0..VARINT {
        let byte = data[*at];
        *at += 1;
        v |= u32::from(byte & 0x7f) << (7 * i);
        if byte & 0x80 == 0 {
            break;
        }
    }
    blaze_int::idx(v)
}

/// Appends `strs`, sorted by bytes and each once, as a front-coded column; refuses the first
/// string out of order by its index. Two passes over `strs`, no scratch: the bucket starts are
/// reserved and patched as the buckets are written. The column's bytes.
pub fn encode_front(strs: &[&[u8]], out: &mut AVec<'_, u8>) -> Result<usize, FrontError> {
    let n = strs.len();
    let count = len32(n, "a front-coded column")?;
    let mut longest = 0usize;
    for (i, pair) in strs.windows(2).enumerate() {
        match pair[0].cmp(pair[1]) {
            core::cmp::Ordering::Less => {}
            core::cmp::Ordering::Equal => return Err(FrontError::NotIncreasing { index: i + 1, duplicate: true }),
            core::cmp::Ordering::Greater => return Err(FrontError::NotIncreasing { index: i + 1, duplicate: false }),
        }
    }
    for s in strs {
        len32(s.len(), "a front-coded string")?;
        longest = longest.max(s.len());
    }
    let buckets = n.div_ceil(BUCKET);
    let start = out.len();
    put(out, &[KIND_FRONT])?;
    put(out, &count.to_le_bytes())?;
    put(out, &len32(longest, "a front-coded string")?.to_le_bytes())?;
    let data_len_at = out.len();
    put(out, &[0; 4])?;
    let starts_at = out.len();
    for _ in 0..buckets {
        put(out, &[0; 4])?;
    }
    let data_at = out.len();
    for (i, s) in strs.iter().enumerate() {
        // Each length was checked to fit a u32 above, and a shared prefix is no longer.
        if i % BUCKET == 0 {
            let at = len32(out.len() - data_at, "a front-coded column's buckets")?;
            patch32(out, starts_at + 4 * (i / BUCKET), at);
            put_varint(out, len32(s.len(), "a front-coded string")?)?;
            put(out, s)?;
        } else {
            let shared = lcp(strs[i - 1], s);
            put_varint(out, len32(shared, "a shared prefix")?)?;
            put_varint(out, len32(s.len() - shared, "a front-coded string")?)?;
            put(out, &s[shared..])?;
        }
    }
    let data_len = len32(out.len() - data_at, "a front-coded column's buckets")?;
    patch32(out, data_len_at, data_len);
    Ok(out.len() - start)
}

/// One entry of a bucket, read: its shared prefix, and where and how long its rest is.
#[derive(Clone, Copy)]
struct Entry {
    shared: usize,
    rest_at: usize,
    rest_len: usize,
}

impl Entry {
    const EMPTY: Entry = Entry { shared: 0, rest_at: 0, rest_len: 0 };

    fn len(self) -> usize {
        self.shared + self.rest_len
    }
}

/// A front-coded column, checked whole once by [`FrontView::open`]: every bucket's lengths
/// read, every rest inside its bucket, every shared prefix no longer than the string before,
/// the longest string as the header says. Reads through it check nothing again and index only
/// inside what `open` checked. The ORDER of the strings is not checked: it is the writer's,
/// held by the record's digest (the module doc of [`super`]); a column out of order still reads
/// without a panic, and `find` on it answers wrong.
#[derive(Clone, Copy, Debug)]
pub struct FrontView<'a> {
    count: usize,
    longest: usize,
    starts: &'a [u8],
    data: &'a [u8],
}

impl<'a> FrontView<'a> {
    /// The front-coded column that is all of `bytes`.
    pub fn open(bytes: &'a [u8]) -> Result<FrontView<'a>, EncError> {
        let corrupt = |what, at: usize| EncError::Corrupt { what, at };
        if read8(bytes, 0, "a front-coded column's kind")? != KIND_FRONT {
            return Err(corrupt("not a front-coded column", 0));
        }
        let count = blaze_int::idx(read32(bytes, 1, "a front-coded column's count")?);
        let longest = blaze_int::idx(read32(bytes, 5, "a front-coded column's longest string")?);
        let data_len = blaze_int::idx(read32(bytes, 9, "a front-coded column's buckets' size")?);
        let buckets = count.div_ceil(BUCKET);
        let data_at = HEADER + 4 * buckets;
        let size = data_at.saturating_add(data_len);
        if bytes.len() < size {
            return Err(corrupt("a front-coded column shorter than its header says", bytes.len()));
        }
        if bytes.len() > size {
            return Err(corrupt("bytes after the column", size));
        }
        let view = FrontView { count, longest, starts: &bytes[HEADER..data_at], data: &bytes[data_at..] };
        if count == 0 && data_len != 0 {
            return Err(corrupt("buckets in a column of no string", 9));
        }
        let mut before = 0usize;
        for b in 0..buckets {
            let at = blaze_int::idx(read32(view.starts, 4 * b, "a bucket's start")?);
            if (b == 0 && at != 0) || (b > 0 && at <= before) || at >= data_len {
                return Err(corrupt("a bucket's start past the end or not past the one before", HEADER + 4 * b));
            }
            before = at;
        }
        let mut seen_longest = 0usize;
        for b in 0..buckets {
            let (mut at, end) = view.bucket(b);
            let mut previous = 0usize;
            for k in 0..view.bucket_len(b) {
                let shared = if k == 0 { 0 } else { blaze_int::idx(read_varint(view.data, &mut at, end, data_at)?) };
                if shared > previous {
                    return Err(corrupt("a shared prefix longer than the string before", data_at + at));
                }
                let rest_len = blaze_int::idx(read_varint(view.data, &mut at, end, data_at)?);
                if rest_len > end - at {
                    return Err(corrupt("a string's bytes past its bucket's end", data_at + at));
                }
                at += rest_len;
                previous = shared + rest_len;
                seen_longest = seen_longest.max(previous);
            }
            if at != end {
                return Err(corrupt("bytes between a bucket's last string and the next bucket", data_at + at));
            }
        }
        if seen_longest != longest {
            return Err(corrupt("a longest string other than the header's", 5));
        }
        Ok(view)
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// The longest string's length: a buffer this long holds any string ([`FrontView::walk`]).
    pub fn longest(&self) -> usize {
        self.longest
    }

    fn buckets(&self) -> usize {
        self.count.div_ceil(BUCKET)
    }

    fn bucket_len(&self, b: usize) -> usize {
        (self.count - b * BUCKET).min(BUCKET)
    }

    /// Bucket `b`'s bytes: where in the buckets it starts and ends.
    fn bucket(&self, b: usize) -> (usize, usize) {
        let start_of = |b: usize| {
            let mut word = [0u8; 4];
            word.copy_from_slice(&self.starts[4 * b..4 * b + 4]);
            blaze_int::idx(u32::from_le_bytes(word))
        };
        let end = if b + 1 < self.buckets() { start_of(b + 1) } else { self.data.len() };
        (start_of(b), end)
    }

    /// Bucket `b`'s first string, where it lies.
    fn head(&self, b: usize) -> &'a [u8] {
        let (mut at, _) = self.bucket(b);
        let len = varint_at(self.data, &mut at);
        &self.data[at..at + len]
    }

    /// String `i` (below `len`), copied into `buf`: refused by [`EncError::Buffer`] when `buf`
    /// is shorter than it, nothing cut. Reads one bucket, writing the pieces from the last
    /// entry back, so no string before it in the bucket is built (one longer than `buf` may
    /// be there).
    pub fn get<'b>(&self, i: usize, buf: &'b mut [u8]) -> Result<&'b [u8], EncError> {
        assert!(i < self.count, "blaze_columns::enc::front: string {i} of a column of {}", self.count);
        let (b, k) = (i / BUCKET, i % BUCKET);
        let mut entries = [Entry::EMPTY; BUCKET];
        let (mut at, _) = self.bucket(b);
        for (j, entry) in entries.iter_mut().enumerate().take(k + 1) {
            let shared = if j == 0 { 0 } else { varint_at(self.data, &mut at) };
            let rest_len = varint_at(self.data, &mut at);
            *entry = Entry { shared, rest_at: at, rest_len };
            at += rest_len;
        }
        let need = entries[k].len();
        if buf.len() < need {
            return Err(EncError::Buffer { index: i, need, have: buf.len() });
        }
        // Bytes `lo..end` of string `j` are the first `end - lo` of its rest when `lo` is its
        // shared prefix; `end` never passes string `j`'s length, as `open` checked every shared
        // prefix against the string before.
        let mut end = need;
        for entry in entries[..=k].iter().rev() {
            if entry.shared < end {
                let len = end - entry.shared;
                buf[entry.shared..end].copy_from_slice(&self.data[entry.rest_at..entry.rest_at + len]);
                end = entry.shared;
            }
            if end == 0 {
                break;
            }
        }
        Ok(&buf[..need])
    }

    /// The index of `key`, or where it would go: a binary search of the buckets' first strings
    /// compared where they lie, then one bucket scanned keeping only how much of `key` the
    /// string so far matches (each entry's shared prefix says the rest, comparing only the
    /// bytes it adds). Nothing is decoded or copied.
    pub fn find(&self, key: &[u8]) -> Result<usize, usize> {
        let (mut low, mut high) = (0usize, self.buckets());
        while low < high {
            let mid = low + (high - low) / 2;
            if self.head(mid) <= key {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        if low == 0 {
            return Err(0);
        }
        let b = low - 1;
        let head = self.head(b);
        if head == key {
            return Ok(b * BUCKET);
        }
        // The string before is less than `key` and matches its first `matched` bytes.
        let mut matched = lcp(head, key);
        let (mut at, _) = self.bucket(b);
        let head_len = varint_at(self.data, &mut at);
        at += head_len;
        let n = self.bucket_len(b);
        for k in 1..n {
            let i = b * BUCKET + k;
            let shared = varint_at(self.data, &mut at);
            let rest_len = varint_at(self.data, &mut at);
            let rest = &self.data[at..at + rest_len];
            at += rest_len;
            match shared.cmp(&matched) {
                // It equals the string before past where that one and `key` part: still less.
                core::cmp::Ordering::Greater => {}
                // It parts from the string before, upward, where that one still matched `key`.
                core::cmp::Ordering::Less => return Err(i),
                core::cmp::Ordering::Equal => {
                    let tail = &key[matched..];
                    let l = lcp(rest, tail);
                    if l == rest.len() && l == tail.len() {
                        return Ok(i);
                    }
                    if l == tail.len() {
                        return Err(i);
                    }
                    if l == rest.len() || rest[l] < tail[l] {
                        matched += l;
                    } else {
                        return Err(i);
                    }
                }
            }
        }
        Err(b * BUCKET + n)
    }

    /// Every string in order, each written over the last in `buf` (at least
    /// [`FrontView::longest`] long): only the bytes an entry adds are copied.
    pub fn walk<'b>(&self, buf: &'b mut [u8]) -> Result<Walk<'a, 'b>, EncError> {
        if buf.len() < self.longest {
            return Err(EncError::Buffer { index: 0, need: self.longest, have: buf.len() });
        }
        Ok(Walk { view: *self, buf, next: 0, at: 0 })
    }
}

/// The strings of a [`FrontView`] in order, through one buffer ([`FrontView::walk`]).
#[derive(Debug)]
pub struct Walk<'a, 'b> {
    view: FrontView<'a>,
    buf: &'b mut [u8],
    next: usize,
    /// Where the next entry starts in the buckets: they are end to end.
    at: usize,
}

impl Walk<'_, '_> {
    /// The next string, borrowed from the buffer until the next call; `None` after the last.
    pub fn next_text(&mut self) -> Option<&[u8]> {
        if self.next >= self.view.count {
            return None;
        }
        let data = self.view.data;
        let shared = if self.next % BUCKET == 0 { 0 } else { varint_at(data, &mut self.at) };
        let rest_len = varint_at(data, &mut self.at);
        // `shared + rest_len` is a string's length, never past `longest`, which `buf` holds.
        self.buf[shared..shared + rest_len].copy_from_slice(&data[self.at..self.at + rest_len]);
        self.at += rest_len;
        self.next += 1;
        Some(&self.buf[..shared + rest_len])
    }
}

#[cfg(test)]
mod tests {
    //! Every sorted set round-trips (`get`, `walk`), `find` agrees with a binary search over
    //! the strings for present and absent keys, equal input writes equal bytes, the writer
    //! refuses an unsorted or repeated string by index, `open` refuses damage by name, a short
    //! buffer is `EncError::Buffer`, and a path corpus codes under half of the plain column.

    use blaze_own::{AVec, Alloc};
    use blaze_sim::{Config, Tape};

    use super::{BUCKET, FrontError, FrontView, HEADER, encode_front};
    use crate::enc::EncError;
    use crate::enc::strs::{StrPolicy, StrScheme, encode_strs};

    fn global() -> Alloc<'static> {
        blaze_own::testing::heap().alloc()
    }

    fn column(strs: &[&[u8]]) -> AVec<'static, u8> {
        let mut out = AVec::new_in(global());
        encode_front(strs, &mut out).expect("a sorted set is written");
        out
    }

    /// Mostly a few bytes (so neighbours share prefixes), sometimes any byte, 0xFF and 0 among
    /// them.
    fn byte(tape: &Tape) -> u8 {
        const COMMON: [u8; 5] = [b'a', b'b', b':', 0x00, 0xFF];
        if tape.coin("any byte") {
            u8::try_from(tape.below(256, "byte")).unwrap_or(0)
        } else {
            COMMON[tape.index(COMMON.len(), "common byte")]
        }
    }

    fn draw_set(tape: &Tape) -> Vec<Vec<u8>> {
        let mut strings = tape.vec(0..=70, "strings", |t| {
            let most = if t.one_in(16, "long") { 300 } else { 24 };
            t.vec(0..=most, "bytes", byte)
        });
        strings.sort();
        strings.dedup();
        strings
    }

    /// Invariant: a sorted set of any bytes (empty strings, 0xFF, more strings than a
    /// bucket, one string, none) reads back string for string through `get` and `walk`;
    /// `find` of each is its index, and of any other key what `binary_search` says; the same
    /// set writes the same bytes. Prints nothing; a failure prints its tape.
    #[test]
    fn a_sorted_set_round_trips_and_finds_as_a_binary_search() {
        blaze_sim::check(Config::cases(300), |tape| {
            let owned = draw_set(tape);
            let strs: Vec<&[u8]> = owned.iter().map(Vec::as_slice).collect();
            let bytes = column(&strs);
            assert_eq!(bytes.as_slice(), column(&strs).as_slice(), "equal input, equal bytes");
            let view = FrontView::open(&bytes).expect("what was written opens");
            assert_eq!(view.len(), strs.len());
            let mut buf = vec![0u8; view.longest()];
            for (i, s) in strs.iter().enumerate() {
                assert_eq!(view.get(i, &mut buf).expect("`longest` holds every string"), *s, "string {i}");
                assert_eq!(view.find(s), Ok(i), "find of string {i}");
                let mut longer = s.to_vec();
                longer.push(0);
                assert_eq!(view.find(&longer), strs.binary_search(&longer.as_slice()), "{longer:?}");
                if let Some((_, shorter)) = s.split_last() {
                    assert_eq!(view.find(shorter), strs.binary_search(&shorter), "{shorter:?}");
                }
            }
            let probes = tape.vec(0..=12, "probes", |t| t.vec(0..=26, "probe", byte));
            for probe in &probes {
                assert_eq!(view.find(probe), strs.binary_search(&probe.as_slice()), "probe {probe:?}");
            }
            let mut walk = view.walk(&mut buf).expect("`longest` holds every string");
            let mut i = 0;
            while let Some(s) = walk.next_text() {
                assert_eq!(s, strs[i], "walked string {i}");
                i += 1;
            }
            assert_eq!(i, strs.len(), "the walk ends after the last string");
        });
    }

    /// The writer refuses the first string not greater than the one before, inside a bucket
    /// and across a bucket's end, naming it and whether it repeats.
    #[test]
    fn the_writer_refuses_an_unsorted_or_repeated_string_by_index() {
        let mut out = AVec::new_in(global());
        let unsorted: [&[u8]; 2] = [b"b", b"a"];
        assert_eq!(encode_front(&unsorted, &mut out), Err(FrontError::NotIncreasing { index: 1, duplicate: false }));
        let repeated: [&[u8]; 3] = [b"a", b"b", b"b"];
        assert_eq!(encode_front(&repeated, &mut out), Err(FrontError::NotIncreasing { index: 2, duplicate: true }));
        let owned: Vec<Vec<u8>> = (0..=BUCKET).map(|i| vec![b'a' + u8::try_from(i).unwrap_or(0)]).collect();
        let mut across: Vec<&[u8]> = owned.iter().map(Vec::as_slice).collect();
        across.swap(BUCKET - 1, BUCKET);
        assert_eq!(encode_front(&across, &mut out), Err(FrontError::NotIncreasing { index: BUCKET, duplicate: false }));
    }

    /// The empty set and a one-string set: 13 and 13 + 4 + 1 + len bytes; `find` before, at
    /// and after the one string.
    #[test]
    fn empty_and_one_string_columns() {
        let empty = column(&[]);
        assert_eq!(empty.len(), HEADER);
        let view = FrontView::open(&empty).expect("the empty column opens");
        assert_eq!((view.len(), view.find(b"a"), view.find(b"")), (0, Err(0), Err(0)));
        let only: [&[u8]; 1] = [b"only"];
        let one = column(&only);
        assert_eq!(one.len(), HEADER + 4 + 1 + 4);
        let view = FrontView::open(&one).expect("a one-string column opens");
        assert_eq!((view.find(b"a"), view.find(b"only"), view.find(b"z")), (Err(0), Ok(0), Err(1)));
        let mut buf = [0u8; 4];
        assert_eq!(view.get(0, &mut buf), Ok(&b"only"[..]));
    }

    /// A buffer one byte short, or empty, is `EncError::Buffer` with the string's length; an
    /// exact one reads it; an empty string needs no buffer; a walk's buffer is held to the
    /// longest string.
    #[test]
    fn a_short_buffer_is_refused_never_cut() {
        let long = [b'z'; 300];
        let strs: [&[u8]; 4] = [b"", b"a", b"core::iter", &long];
        let bytes = column(&strs);
        let view = FrontView::open(&bytes).expect("opens");
        assert_eq!(view.get(0, &mut []), Ok(&b""[..]));
        for (i, s) in strs.iter().enumerate().skip(1) {
            let mut short = vec![0u8; s.len() - 1];
            assert_eq!(view.get(i, &mut short), Err(EncError::Buffer { index: i, need: s.len(), have: s.len() - 1 }));
            assert_eq!(view.get(i, &mut []), Err(EncError::Buffer { index: i, need: s.len(), have: 0 }));
            let mut exact = vec![0u8; s.len()];
            assert_eq!(view.get(i, &mut exact), Ok(*s));
        }
        let mut short = vec![0u8; 299];
        assert!(matches!(view.walk(&mut short), Err(EncError::Buffer { need: 300, have: 299, .. })));
    }

    /// `open` refuses, each by what it names: a cut header, a cut or longer column, another
    /// kind, a bucket start past the end, and a shared prefix longer than the string before;
    /// the column as written opens (the control).
    #[test]
    fn open_refuses_damage_by_name() {
        let owned: Vec<String> = (0..40).map(|i| format!("core::m{:02}::Item", i)).collect();
        let strs: Vec<&[u8]> = owned.iter().map(String::as_bytes).collect();
        let bytes = column(&strs);
        assert!(FrontView::open(&bytes).is_ok(), "the control: the column as written");
        let what = |bytes: &[u8]| match FrontView::open(bytes) {
            Err(EncError::Corrupt { what, .. }) => what,
            other => panic!("not refused as corrupt: {other:?}"),
        };
        assert_eq!(what(&bytes[..7]), "a front-coded column's longest string");
        assert_eq!(what(&bytes[..bytes.len() - 1]), "a front-coded column shorter than its header says");
        let mut longer = bytes.to_vec();
        longer.push(0);
        assert_eq!(what(&longer), "bytes after the column");
        let mut kind = bytes.to_vec();
        kind[0] = 1;
        assert_eq!(what(&kind), "not a front-coded column");
        let mut past = bytes.to_vec();
        let data_len = u32::from_le_bytes([bytes[9], bytes[10], bytes[11], bytes[12]]);
        past[HEADER + 4..HEADER + 8].copy_from_slice(&(data_len + 5).to_le_bytes());
        assert_eq!(what(&past), "a bucket's start past the end or not past the one before");
        // The second entry of the first bucket: after the head's one length byte and its
        // bytes, its shared prefix's one byte, set past the head's length.
        let mut shared = bytes.to_vec();
        let second = HEADER + 4 * 3 + 1 + strs[0].len();
        shared[second] = 100;
        assert_eq!(what(&shared), "a shared prefix longer than the string before");
    }

    /// 3 x 8 x 6 x 10 x 12 = 17,280 sorted paths like `core::iter::adapters::map::Map`
    /// (about 32 bytes each). Prints `path corpus: 17280 strings, plain P bytes, front F (x% of
    /// plain), best strs column B (scheme)`; F is expected near a quarter of P (module doc).
    #[test]
    fn a_path_corpus_front_codes_to_under_half_of_plain() {
        let crates = ["core", "alloc", "std"];
        let mods = ["iter", "slice", "str", "fmt", "ops", "cell", "sync", "collections"];
        let subs = ["adapters", "traits", "range", "sources", "pattern", "builders"];
        let leaves = ["map", "filter", "zip", "chain", "rev", "step_by", "take", "skip", "peekable", "fuse"];
        let items = ["Map", "Filter", "Zip", "Chain", "Rev", "Iter", "IntoIter", "Item", "new", "next", "fold", "len"];
        let mut owned = Vec::new();
        for c in crates {
            for m in mods {
                for s in subs {
                    for l in leaves {
                        for i in items {
                            owned.push(format!("{c}::{m}::{s}::{l}::{i}"));
                        }
                    }
                }
            }
        }
        owned.sort();
        owned.dedup();
        let strs: Vec<&[u8]> = owned.iter().map(String::as_bytes).collect();
        let front = column(&strs);
        let mut plain = AVec::new_in(global());
        encode_strs(&strs, StrPolicy { only: Some(StrScheme::Plain), ..StrPolicy::RECORD }, global(), &mut plain)
            .expect("the plain column is written");
        let mut best = AVec::new_in(global());
        let choice = encode_strs(&strs, StrPolicy::RECORD, global(), &mut best).expect("the chooser's column is written");
        println!(
            "path corpus: {} strings, plain {} bytes, front {} ({}% of plain), best strs column {} ({})",
            strs.len(),
            plain.len(),
            front.len(),
            front.len() * 100 / plain.len(),
            choice.bytes,
            choice.scheme.name()
        );
        assert!(front.len() * 2 <= plain.len(), "front {} is not under half of plain {}", front.len(), plain.len());
        let view = FrontView::open(&front).expect("opens");
        let mut buf = vec![0u8; view.longest()];
        for i in (0..strs.len()).step_by(97) {
            assert_eq!(view.get(i, &mut buf), Ok(strs[i]));
            assert_eq!(view.find(strs[i]), Ok(i));
        }
    }
}
