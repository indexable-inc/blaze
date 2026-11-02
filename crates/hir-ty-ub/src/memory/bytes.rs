//! [`Bytes`]: the shadow of a value the evaluator holds outside memory (an operand, a computed
//! scalar, an aggregate being built): which of its bytes are initialized and which carry a
//! pointer's provenance. The same two facts as an allocation's [`super::init_mask::InitMask`]
//! and [`super::prov_map::ProvMap`], kept small: a value of up to 64 bytes with one pointer run
//! lives inline, and a fully initialized value of any size holds no bits at all.

use std::ops::Range;

use blaze_ids::bits::{locate, words_for};
use smallvec::SmallVec;

use super::{MemError, Uninit, init_mask};
use crate::ids::{AllocId, Prov, Span};

/// Bytes `at..at + len` are bytes `idx..idx + len` of a pointer with provenance `prov`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Run {
    pub(super) at: u32,
    pub(super) idx: u8,
    pub(super) len: u8,
    /// A whole pointer, as an entry of Miri's `ProvenanceMap::ptrs`, rather than fragments of
    /// one (`ProvenanceMap::bytes`): reading a pointer that overlaps it anywhere but exactly has
    /// no provenance, and fragments of a wildcard pointer need not be in order.
    pub(super) whole: bool,
    pub(super) prov: Prov,
}

const _: () = assert!(size_of::<Run>() == 24);

impl Run {
    #[inline]
    fn end(&self) -> u32 {
        self.at + u32::from(self.len)
    }

    /// Whether `next` continues this run: the next bytes of the same pointer, both fragments.
    #[inline]
    pub(super) fn joins(&self, next: &Run) -> bool {
        !self.whole
            && !next.whole
            && self.end() == next.at
            && self.prov == next.prov
            && u16::from(self.idx) + u16::from(self.len) == u16::from(next.idx)
            && u16::from(self.len) + u16::from(next.len) <= u16::from(u8::MAX)
    }
}

/// The initialization and provenance of a value's bytes. See the module docs.
#[derive(Clone, Default, Debug)]
pub struct Bytes {
    pub(super) len: usize,
    /// Empty: every byte is initialized. Otherwise bit `i` is byte `i`, and bits past `len` are
    /// clear. Words, not a `blaze_ids::bits` set: empty is a state of its own, and the words
    /// move by `init_mask`'s blits, as an allocation's mask does.
    pub(super) init: SmallVec<[u64; 1]>,
    /// Sorted by `at`, disjoint, and joined where they continue each other.
    pub(super) prov: SmallVec<[Run; 1]>,
    /// The allocation and offset these bytes were read from, while they are still exactly that
    /// read: it names the memory in an error about uninitialized bytes, as Miri's.
    pub(super) origin: Option<(AllocId, u64)>,
}

/// The bits of a word holding the last `len % 64` bytes.
#[inline]
fn tail_mask(len: usize) -> u64 {
    match len % 64 {
        0 => u64::MAX,
        tail => u64::MAX >> (64 - tail),
    }
}

/// `u32` offset of a value's byte: values held outside memory are far smaller than 4 GiB.
#[inline]
pub(super) fn offset(at: usize) -> u32 {
    u32::try_from(at).expect("a value held outside memory is smaller than 4 GiB")
}

impl Bytes {
    /// Bytes from their parts, with init bits that say every byte is initialized dropped.
    pub(super) fn from_parts(
        len: usize,
        mut init: SmallVec<[u64; 1]>,
        prov: SmallVec<[Run; 1]>,
        origin: Option<(AllocId, u64)>,
    ) -> Bytes {
        if let Some((&last, full)) = init.split_last()
            && last == tail_mask(len)
            && full.iter().all(|&w| w == u64::MAX)
        {
            init.clear();
        }
        Bytes { len, init, prov, origin }
    }

    /// Whether no byte is initialized (and there is at least one).
    pub(super) fn all_uninit(&self) -> bool {
        !self.init.is_empty() && self.init.iter().all(|&w| w == 0)
    }

    /// The init bits: empty when every byte is initialized.
    pub(super) fn init_bits(&self) -> &[u64] {
        &self.init
    }

    /// `len` initialized bytes without provenance: a value the evaluator computed.
    pub fn init(len: usize) -> Bytes {
        Bytes { len, init: SmallVec::new(), prov: SmallVec::new(), origin: None }
    }

    /// `len` uninitialized bytes.
    pub fn uninit(len: usize) -> Bytes {
        let mut init = SmallVec::new();
        init.resize(words_for(len), 0);
        if len == 0 {
            return Bytes::init(0);
        }
        Bytes { len, init, prov: SmallVec::new(), origin: None }
    }

    /// A pointer of `size` bytes with provenance `prov`, initialized.
    pub fn pointer(size: usize, prov: Option<Prov>) -> Bytes {
        let mut bytes = Bytes::init(size);
        if let Some(prov) = prov
            && size > 0
        {
            let len = u8::try_from(size).expect("a pointer is at most 255 bytes");
            bytes.prov.push(Run { at: 0, idx: 0, len, whole: true, prov });
        }
        bytes
    }

    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether byte `i` is initialized (bytes past the end are not).
    #[inline]
    pub fn is_init(&self, i: usize) -> bool {
        i < self.len && (self.init.is_empty() || {
            let (word, mask) = locate(i);
            self.init[word] & mask != 0
        })
    }

    /// Whether any byte of `range` carries provenance.
    pub fn has_prov(&self, range: Range<usize>) -> bool {
        let (lo, hi) = self.runs_overlapping(range);
        lo < hi
    }

    /// The word `w` of the init bits, as if they were materialized.
    #[inline]
    fn word(&self, w: usize) -> u64 {
        if self.init.is_empty() {
            if w + 1 == words_for(self.len) { tail_mask(self.len) } else { u64::MAX }
        } else {
            self.init[w]
        }
    }

    fn materialize(&mut self) {
        if self.init.is_empty() && self.len > 0 {
            let n = words_for(self.len);
            self.init.resize(n, u64::MAX);
            self.init[n - 1] = tail_mask(self.len);
        }
    }

    /// Sets bytes `range` (in bounds) to `state`, on materialized bits.
    fn set_init(&mut self, range: Range<usize>, state: bool) {
        let mut pos = range.start;
        while pos < range.end {
            let n = (range.end - pos).min(64);
            let value = if state { u64::MAX } else { 0 };
            init_mask::blit(&mut self.init, pos as u64, value, n as u64);
            pos += n;
        }
    }

    /// Runs `lo..hi` of `self.prov` overlap `range`.
    #[inline]
    fn runs_overlapping(&self, range: Range<usize>) -> (usize, usize) {
        let start = offset(range.start);
        let end = offset(range.end);
        let lo = self.prov.partition_point(|r| r.end() <= start);
        let hi = lo + self.prov[lo..].partition_point(|r| r.at < end);
        (lo, hi)
    }

    /// Bytes `range` of these.
    pub fn slice(&self, range: Range<usize>) -> Bytes {
        debug_assert!(range.start <= range.end && range.end <= self.len);
        let len = range.end - range.start;
        let mut init = SmallVec::new();
        if !self.init.is_empty() && len > 0 {
            init.resize(words_for(len), 0);
            init_mask::extract(&self.init, range.start as u64, &mut init);
            let n = init.len();
            init[n - 1] &= tail_mask(len);
        }
        let start = offset(range.start);
        let end = offset(range.end);
        let (lo, hi) = self.runs_overlapping(range.clone());
        let prov = self.prov[lo..hi]
            .iter()
            .map(|r| {
                let a = r.at.max(start);
                let b = r.end().min(end);
                Run {
                    at: a - start,
                    idx: r.idx + (a - r.at) as u8,
                    len: (b - a) as u8,
                    whole: r.whole && a == r.at && b == r.end(),
                    prov: r.prov,
                }
            })
            .collect();
        let origin = self.origin.map(|(alloc, at)| (alloc, at + range.start as u64));
        Bytes::from_parts(len, init, prov, origin)
    }

    /// `other` put over these at `at`, growing them when it ends past them (bytes between the
    /// old end and `at` are uninitialized).
    pub fn put(&mut self, at: usize, other: &Bytes) {
        let end = at + other.len;
        let old_len = self.len;
        self.origin = if at == 0 && end >= old_len {
            other.origin
        } else {
            match (self.origin, other.origin) {
                (Some((a, s)), Some((b, t))) if a == b && t == s + at as u64 => Some((a, s)),
                _ => None,
            }
        };
        if end > old_len {
            if !self.init.is_empty() || at > old_len || !other.init.is_empty() {
                self.materialize();
                self.init.resize(words_for(end), 0);
            }
            self.len = end;
        }
        if other.init.is_empty() {
            if !self.init.is_empty() {
                self.set_init(at..end, true);
            }
        } else {
            self.materialize();
            let mut pos = at;
            for &word in &other.init {
                let n = (end - pos).min(64);
                init_mask::blit(&mut self.init, pos as u64, word, n as u64);
                pos += n;
            }
        }
        self.cut(at..end);
        if !other.prov.is_empty() {
            let shift = offset(at);
            let lo = self.prov.partition_point(|r| r.at < shift);
            self.prov.insert_many(lo, other.prov.iter().map(|r| Run { at: r.at + shift, ..*r }));
            let hi = lo + other.prov.len();
            if hi < self.prov.len() && self.prov[hi - 1].joins(&self.prov[hi]) {
                self.prov[hi - 1].len += self.prov[hi].len;
                self.prov.remove(hi);
            }
            if lo > 0 && self.prov[lo - 1].joins(&self.prov[lo]) {
                self.prov[lo - 1].len += self.prov[lo].len;
                self.prov.remove(lo);
            }
        }
    }

    /// Removes the provenance of `range`; runs that stick out keep their bytes outside it.
    fn cut(&mut self, range: Range<usize>) {
        if range.start >= range.end {
            return;
        }
        let (lo, hi) = self.runs_overlapping(range.clone());
        if lo == hi {
            return;
        }
        let start = offset(range.start);
        let end = offset(range.end);
        let first = self.prov[lo];
        let last = self.prov[hi - 1];
        self.prov.drain(lo..hi);
        if last.end() > end {
            let cut = (end - last.at) as u8;
            let rest = Run { at: end, idx: last.idx + cut, len: last.len - cut, whole: false, ..last };
            self.prov.insert(lo, rest);
        }
        if first.at < start {
            self.prov.insert(lo, Run { len: (start - first.at) as u8, whole: false, ..first });
        }
    }

    /// Makes bytes `range` carry no provenance (a pointer partly in it keeps the rest).
    pub fn clear_prov(&mut self, range: Range<usize>) {
        self.cut(range);
    }

    /// Makes bytes `range` uninitialized and without provenance: padding reset.
    pub fn set_uninit(&mut self, range: Range<usize>) {
        if range.start >= range.end {
            return;
        }
        debug_assert!(range.end <= self.len);
        self.materialize();
        self.set_init(range.clone(), false);
        self.cut(range);
    }

    /// The provenance of the pointer of `size` bytes at `at`: a whole pointer there, or
    /// fragments that put one back together.
    // Miri: provenance_map.rs, `read_ptr`.
    pub fn prov_at(&self, at: usize, size: usize) -> Option<Prov> {
        let (lo, hi) = self.runs_overlapping(at..at + size);
        if lo == hi {
            return None;
        }
        let start = offset(at);
        let end = offset(at + size);
        let first = &self.prov[lo];
        if first.whole && first.at == start && first.end() == end {
            return Some(first.prov);
        }
        let mut pos = start;
        let mut expected = None;
        for run in &self.prov[lo..hi] {
            if run.whole || run.at > pos {
                return None;
            }
            let idx = u32::from(run.idx) + (pos - run.at);
            let next = run.end().min(end);
            if run.prov != Prov::Wildcard {
                if idx != pos - start {
                    return None;
                }
                match expected {
                    Some(prov) if prov != run.prov => return None,
                    Some(_) => {}
                    None => expected = Some(run.prov),
                }
            }
            pos = next;
        }
        if pos < end {
            return None;
        }
        Some(expected.unwrap_or(Prov::Wildcard))
    }

    /// Fails unless every byte of `range` is initialized.
    pub fn require_init(&self, range: Range<usize>) -> Result<(), MemError> {
        let bad = self.first_uninit(range.clone());
        match bad {
            None => Ok(()),
            Some(bad) => Err(MemError::Uninit(self.origin.map(|(alloc, at)| Uninit {
                alloc,
                access: Span::new(at + range.start as u64, (range.end - range.start) as u64),
                bad: Span::new(at + bad.start as u64, (bad.end - bad.start) as u64),
            }))),
        }
    }

    /// The first run of uninitialized bytes in `range` (Miri's `InitMask::is_range_initialized`,
    /// which names everything past the end when the range runs past it).
    fn first_uninit(&self, range: Range<usize>) -> Option<Range<usize>> {
        if range.end > self.len {
            return Some(self.len..range.end);
        }
        if self.init.is_empty() || range.start >= range.end {
            return None;
        }
        let start = self.next_state(range.start, range.end, false)?;
        Some(start..self.next_init(start, range.end))
    }

    fn next_init(&self, from: usize, end: usize) -> usize {
        self.next_state(from, end, true).unwrap_or(end)
    }

    /// The first byte of `from..end` in `state`, a word at a time.
    fn next_state(&self, from: usize, end: usize, state: bool) -> Option<usize> {
        let mut w = from / 64;
        let mut mask = u64::MAX << (from % 64);
        while w * 64 < end {
            let word = if state { self.word(w) } else { !self.word(w) };
            let bits = word & mask;
            if bits != 0 {
                let i = w * 64 + bits.trailing_zeros() as usize;
                return (i < end).then_some(i);
            }
            mask = u64::MAX;
            w += 1;
        }
        None
    }

    /// The provenance runs, for the memory they are written to.
    pub(super) fn runs(&self) -> &[Run] {
        &self.prov
    }

    /// Every tag these bytes hold.
    pub fn provenances(&self) -> impl Iterator<Item = Prov> + '_ {
        self.prov.iter().map(|r| r.prov)
    }
}

impl PartialEq for Bytes {
    /// Equal initialization and provenance; where the bytes came from is not compared.
    fn eq(&self, other: &Bytes) -> bool {
        self.len == other.len
            && (0..words_for(self.len)).all(|w| self.word(w) == other.word(w))
            && self.prov == other.prov
    }
}

impl Eq for Bytes {}
