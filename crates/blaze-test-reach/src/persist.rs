//! Zero-reindex persistent Structure-of-Arrays (SoA) change-impact index (`BTI1`).
//!
//! Serializes an [`ImpactIndex`] into a deterministic, cross-worktree-shareable binary image
//! keyed solely by `(CellKind, Digest)` (no machine paths or worktree-local IDs). After a cold
//! reboot or process restart, [`ImpactView::open`] validates the header and section bounds in
//! `O(1)` with **zero allocations and zero reindexing**, and [`ImpactView::select`] runs
//! 64-bit-prefix interpolation search directly over the memory-mapped `cell_names` column in
//! `O(|changed| * (log log U + fanout))`.
//!
//! Layout (`BTI1` v1, little-endian integers, no pointers):
//! - Header (`72` bytes):
//!   - `0..4`:   magic `b"BTI1"`
//!   - `4..8`:   version `1u32`
//!   - `8..12`:  `test_count: u32`
//!   - `12..16`: `key_count: u32`
//!   - `16..20`: `posting_count: u32`
//!   - `20..24`: `always_count: u32`
//!   - `24..28`: `code_keys_end: u32`
//!   - `28..32`: `kind_ends: [u8; 4]` (unused/zero reserved; kind bands computed via binary search)
//!   - `32..40`: `total_bytes: u64`
//!   - `40..72`: `digest: [u8; 32]` (BLAKE3 derive-key over `bytes[72..total_bytes]`)
//! - SoA Columns (`bytes[72..]`):
//!   1. `test_digests`:       `test_count * 32`
//!   2. `cell_kinds`:         `key_count * 1`
//!   3. `cell_names`:         `key_count * 32`
//!   4. `cell_offsets`:       `(key_count + 1) * 4`
//!   5. `posting_test_idx`:   `posting_count * 4`
//!   6. `posting_expected`:   `posting_count * 32`
//!   7. `posting_row_order`:  `posting_count * 4`
//!   8. `always_rerun`:       `always_count * 40` (`test_idx: u32`, `tag: u8`, `aux: u8`, `[0; 2]`, `digest: [u8; 32]`)

use core::alloc::Allocator;
use core::fmt;

use blaze_own::Vec;

use crate::decide::{Because, NotReusable};
use crate::impact::{ImpactIndex, SelectedTest, Selection};
use crate::record::RecordError;
use crate::{CellKind, Digest, Read};

const MAGIC: [u8; 4] = *b"BTI1";
const VERSION: u32 = 1;
const HEADER_BYTES: usize = 72;
const ALWAYS_ROW_BYTES: usize = 40;
const DIGEST_DOMAIN: &str = "blaze-test-reach impact file 1";

// Tags for encoded `Because` in `always_rerun`.
const TAG_NO_RECORD: u8 = 1;
const TAG_NOT_REUSABLE: u8 = 2;
const TAG_OTHER_TEST: u8 = 3;
const TAG_UNREADABLE: u8 = 4;

/// Why a persisted `BTI1` impact file could not be opened or verified.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PersistError {
    TooShort,
    BadMagic,
    UnsupportedVersion { found: u32 },
    LengthMismatch { expected: u64, actual: usize },
    CorruptOffsets,
    BadDigest,
    BadAlwaysTag { tag: u8 },
}

impl fmt::Display for PersistError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PersistError::TooShort => f.write_str("impact file shorter than 72-byte header"),
            PersistError::BadMagic => f.write_str("impact file magic is not BTI1"),
            PersistError::UnsupportedVersion { found } => write!(f, "unsupported impact file version {found}"),
            PersistError::LengthMismatch { expected, actual } => {
                write!(f, "impact file length mismatch: header says {expected}, got {actual}")
            }
            PersistError::CorruptOffsets => f.write_str("impact file CSR offsets out of bounds"),
            PersistError::BadDigest => f.write_str("impact file BLAKE3 digest mismatch"),
            PersistError::BadAlwaysTag { tag } => write!(f, "invalid always-rerun tag {tag}"),
        }
    }
}

fn encode_record_error(err: RecordError) -> (u8, [u8; 32]) {
    let mut buf = [0u8; 32];
    let sub = match err {
        RecordError::Short { len } => {
            buf[0..8].copy_from_slice(&(len as u64).to_le_bytes());
            1
        }
        RecordError::Magic => 2,
        RecordError::Version { found } => {
            buf[0..4].copy_from_slice(&found.to_le_bytes());
            3
        }
        RecordError::OutTag { found } => {
            buf[0] = found;
            4
        }
        RecordError::ReusableTag { found } => {
            buf[0] = found;
            5
        }
        RecordError::NotZero { at } => {
            buf[0..8].copy_from_slice(&(at as u64).to_le_bytes());
            6
        }
        RecordError::TooManyReads { count } => {
            buf[0..4].copy_from_slice(&count.to_le_bytes());
            7
        }
        RecordError::Length { len, want } => {
            buf[0..8].copy_from_slice(&(len as u64).to_le_bytes());
            buf[8..16].copy_from_slice(&(want as u64).to_le_bytes());
            8
        }
        RecordError::RowKind { row, found } => {
            buf[0..8].copy_from_slice(&(row as u64).to_le_bytes());
            buf[8] = found;
            9
        }
        RecordError::RowPadding { row } => {
            buf[0..8].copy_from_slice(&(row as u64).to_le_bytes());
            10
        }
        RecordError::Order { row } => {
            buf[0..8].copy_from_slice(&(row as u64).to_le_bytes());
            11
        }
        RecordError::TwoValues { row } => {
            buf[0..8].copy_from_slice(&(row as u64).to_le_bytes());
            12
        }
        RecordError::Digest => 13,
    };
    (sub, buf)
}

fn decode_record_error(sub: u8, buf: &[u8; 32]) -> Result<RecordError, PersistError> {
    Ok(match sub {
        1 => RecordError::Short { len: read_u64_le(buf, 0) as usize },
        2 => RecordError::Magic,
        3 => RecordError::Version { found: read_u32_le(buf, 0) },
        4 => RecordError::OutTag { found: buf[0] },
        5 => RecordError::ReusableTag { found: buf[0] },
        6 => RecordError::NotZero { at: read_u64_le(buf, 0) as usize },
        7 => RecordError::TooManyReads { count: read_u32_le(buf, 0) },
        8 => RecordError::Length { len: read_u64_le(buf, 0) as usize, want: read_u64_le(buf, 8) as usize },
        9 => RecordError::RowKind { row: read_u64_le(buf, 0) as usize, found: buf[8] },
        10 => RecordError::RowPadding { row: read_u64_le(buf, 0) as usize },
        11 => RecordError::Order { row: read_u64_le(buf, 0) as usize },
        12 => RecordError::TwoValues { row: read_u64_le(buf, 0) as usize },
        13 => RecordError::Digest,
        _ => return Err(PersistError::BadAlwaysTag { tag: TAG_UNREADABLE }),
    })
}

/// Encodes `index` into a self-contained, zero-copy SoA `BTI1` byte buffer in `alloc`.
pub fn encode_impact<I: Allocator, A: Allocator>(index: &ImpactIndex<I>, alloc: A) -> Vec<u8, A> {
    let tests = index.tests_slice();
    let keys = index.keys_slice();
    let offsets = index.offsets_slice();
    let postings = index.postings_slice();
    let always = index.always_rerun_slice();

    let test_count = u32::try_from(tests.len()).expect("tests fit u32");
    let key_count = u32::try_from(keys.len()).expect("keys fit u32");
    let posting_count = u32::try_from(postings.len()).expect("postings fit u32");
    let always_count = u32::try_from(always.len()).expect("always fit u32");
    let code_keys_end = u32::try_from(index.code_keys_end()).expect("code_keys_end fits u32");

    let payload_len = tests.len() * 32
        + keys.len()
        + keys.len() * 32
        + (keys.len() + 1) * 4
        + postings.len() * 4
        + postings.len() * 32
        + postings.len() * 4
        + always.len() * ALWAYS_ROW_BYTES;
    let total_bytes = HEADER_BYTES + payload_len;

    let mut out = Vec::with_capacity_in(total_bytes, alloc);
    out.resize(HEADER_BYTES, 0);

    // 1. test_digests
    for t in tests {
        out.extend_from_slice(&t.0);
    }
    // 2. cell_kinds
    for &(kind, _) in keys {
        out.push(kind as u8);
    }
    // 3. cell_names
    for &(_, name) in keys {
        out.extend_from_slice(&name.0);
    }
    // 4. cell_offsets
    for &off in offsets {
        out.extend_from_slice(&u32::try_from(off).expect("offset fits u32").to_le_bytes());
    }
    // 5. posting_test_idx
    for p in postings {
        out.extend_from_slice(&p.test_idx.to_le_bytes());
    }
    // 6. posting_expected
    for p in postings {
        out.extend_from_slice(&p.expected_value.0);
    }
    // 7. posting_row_order
    for p in postings {
        out.extend_from_slice(&p.row_order.to_le_bytes());
    }
    // 8. always_rerun
    for &(test_idx, because) in always {
        out.extend_from_slice(&test_idx.to_le_bytes());
        match because {
            Because::NoRecord => {
                out.extend_from_slice(&[TAG_NO_RECORD, 0, 0, 0]);
                out.extend_from_slice(&[0u8; 32]);
            }
            Because::NotReusable(why) => {
                out.extend_from_slice(&[TAG_NOT_REUSABLE, why as u8, 0, 0]);
                out.extend_from_slice(&[0u8; 32]);
            }
            Because::OtherTest { found } => {
                out.extend_from_slice(&[TAG_OTHER_TEST, 0, 0, 0]);
                out.extend_from_slice(&found.0);
            }
            Because::Unreadable(err) => {
                let (sub, buf) = encode_record_error(err);
                out.extend_from_slice(&[TAG_UNREADABLE, sub, 0, 0]);
                out.extend_from_slice(&buf);
            }
            Because::Moved(_) => {
                out.extend_from_slice(&[TAG_UNREADABLE, 2, 0, 0]);
                out.extend_from_slice(&[0u8; 32]);
            }
        }
    }

    debug_assert_eq!(out.len(), total_bytes);
    let digest = blaze_blake3::derive_key(DIGEST_DOMAIN, &out[HEADER_BYTES..]);

    out[0..4].copy_from_slice(&MAGIC);
    out[4..8].copy_from_slice(&VERSION.to_le_bytes());
    out[8..12].copy_from_slice(&test_count.to_le_bytes());
    out[12..16].copy_from_slice(&key_count.to_le_bytes());
    out[16..20].copy_from_slice(&posting_count.to_le_bytes());
    out[20..24].copy_from_slice(&always_count.to_le_bytes());
    out[24..28].copy_from_slice(&code_keys_end.to_le_bytes());
    out[28..32].copy_from_slice(&[0u8; 4]);
    out[32..40].copy_from_slice(&(total_bytes as u64).to_le_bytes());
    out[40..72].copy_from_slice(&digest);

    out
}

/// Zero-copy, zero-allocation SoA view over a persisted `BTI1` byte slice (e.g., an `mmap`'d
/// file immediately after a reboot).
#[derive(Clone, Copy, Debug)]
pub struct ImpactView<'a> {
    raw: &'a [u8],
    test_count: usize,
    key_count: usize,
    posting_count: usize,
    always_count: usize,
    code_keys_end: usize,
    digest: [u8; 32],
    test_digests: &'a [u8],
    cell_kinds: &'a [u8],
    cell_names: &'a [u8],
    cell_offsets: &'a [u8],
    posting_test_idx: &'a [u8],
    posting_expected: &'a [u8],
    posting_row_order: &'a [u8],
    always_rerun: &'a [u8],
}

#[inline]
fn read_u32_le(bytes: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]])
}

#[inline]
fn read_u64_le(bytes: &[u8], off: usize) -> u64 {
    u64::from_le_bytes([
        bytes[off],
        bytes[off + 1],
        bytes[off + 2],
        bytes[off + 3],
        bytes[off + 4],
        bytes[off + 5],
        bytes[off + 6],
        bytes[off + 7],
    ])
}

#[inline]
fn read_u64_be(bytes: &[u8]) -> u64 {
    u64::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7]])
}

#[inline]
fn read_digest(bytes: &[u8], idx: usize) -> Digest {
    let off = idx * 32;
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes[off..off + 32]);
    Digest(out)
}

impl<'a> ImpactView<'a> {
    /// Opens a persisted `BTI1` buffer in `O(1)` with zero allocations and zero reindexing.
    ///
    /// Call [`ImpactView::verify_digest`] if you also want full-payload BLAKE3 verification.
    pub fn open(bytes: &'a [u8]) -> Result<ImpactView<'a>, PersistError> {
        if bytes.len() < HEADER_BYTES {
            return Err(PersistError::TooShort);
        }
        if bytes[0..4] != MAGIC {
            return Err(PersistError::BadMagic);
        }
        let version = read_u32_le(bytes, 4);
        if version != VERSION {
            return Err(PersistError::UnsupportedVersion { found: version });
        }
        let test_count = read_u32_le(bytes, 8) as usize;
        let key_count = read_u32_le(bytes, 12) as usize;
        let posting_count = read_u32_le(bytes, 16) as usize;
        let always_count = read_u32_le(bytes, 20) as usize;
        let code_keys_end = read_u32_le(bytes, 24) as usize;
        let declared_len = read_u64_le(bytes, 32);
        let mut digest = [0u8; 32];
        digest.copy_from_slice(&bytes[40..72]);

        if code_keys_end > key_count {
            return Err(PersistError::CorruptOffsets);
        }

        let len_tests = test_count.checked_mul(32).ok_or(PersistError::CorruptOffsets)?;
        let len_kinds = key_count;
        let len_names = key_count.checked_mul(32).ok_or(PersistError::CorruptOffsets)?;
        let len_offsets = key_count.checked_add(1).and_then(|n| n.checked_mul(4)).ok_or(PersistError::CorruptOffsets)?;
        let len_p_test = posting_count.checked_mul(4).ok_or(PersistError::CorruptOffsets)?;
        let len_p_exp = posting_count.checked_mul(32).ok_or(PersistError::CorruptOffsets)?;
        let len_p_row = posting_count.checked_mul(4).ok_or(PersistError::CorruptOffsets)?;
        let len_always = always_count.checked_mul(ALWAYS_ROW_BYTES).ok_or(PersistError::CorruptOffsets)?;

        let expected_total = HEADER_BYTES
            .checked_add(len_tests)
            .and_then(|n| n.checked_add(len_kinds))
            .and_then(|n| n.checked_add(len_names))
            .and_then(|n| n.checked_add(len_offsets))
            .and_then(|n| n.checked_add(len_p_test))
            .and_then(|n| n.checked_add(len_p_exp))
            .and_then(|n| n.checked_add(len_p_row))
            .and_then(|n| n.checked_add(len_always))
            .ok_or(PersistError::CorruptOffsets)?;

        if declared_len != expected_total as u64 || bytes.len() != expected_total {
            return Err(PersistError::LengthMismatch { expected: declared_len, actual: bytes.len() });
        }

        let mut at = HEADER_BYTES;
        let test_digests = &bytes[at..at + len_tests];
        at += len_tests;
        let cell_kinds = &bytes[at..at + len_kinds];
        at += len_kinds;
        let cell_names = &bytes[at..at + len_names];
        at += len_names;
        let cell_offsets = &bytes[at..at + len_offsets];
        at += len_offsets;
        let posting_test_idx = &bytes[at..at + len_p_test];
        at += len_p_test;
        let posting_expected = &bytes[at..at + len_p_exp];
        at += len_p_exp;
        let posting_row_order = &bytes[at..at + len_p_row];
        at += len_p_row;
        let always_rerun = &bytes[at..at + len_always];

        // O(1) endpoint sanity check on CSR offsets.
        if read_u32_le(cell_offsets, 0) != 0 || read_u32_le(cell_offsets, key_count * 4) as usize != posting_count {
            return Err(PersistError::CorruptOffsets);
        }

        Ok(ImpactView {
            raw: bytes,
            test_count,
            key_count,
            posting_count,
            always_count,
            code_keys_end,
            digest,
            test_digests,
            cell_kinds,
            cell_names,
            cell_offsets,
            posting_test_idx,
            posting_expected,
            posting_row_order,
            always_rerun,
        })
    }

    /// Verifies the full payload against the header's BLAKE3 digest.
    pub fn verify_digest(&self) -> Result<(), PersistError> {
        let got = blaze_blake3::derive_key(DIGEST_DOMAIN, &self.raw[HEADER_BYTES..]);
        if got != self.digest {
            return Err(PersistError::BadDigest);
        }
        Ok(())
    }

    pub fn test_count(&self) -> usize {
        self.test_count
    }

    pub fn key_count(&self) -> usize {
        self.key_count
    }

    pub fn posting_count(&self) -> usize {
        self.posting_count
    }

    pub fn code_keys_end(&self) -> usize {
        self.code_keys_end
    }

    pub fn digest(&self) -> Digest {
        Digest(self.digest)
    }

    /// Locates `(kind, name)` in the sorted SoA `(cell_kinds, cell_names)` columns via 64-bit
    /// big-endian prefix interpolation search with binary-search fallback (`O(log log U)` average
    /// on uniform BLAKE3 digests, `O(log U)` worst-case).
    pub fn find_cell(&self, kind: CellKind, name: Digest) -> Option<usize> {
        if self.key_count == 0 {
            return None;
        }
        let target_kind = kind as u8;
        // Narrow to the contiguous band of `target_kind` first.
        let (mut lo, mut hi) = if target_kind == 0 {
            (0usize, self.code_keys_end)
        } else {
            let start = self.cell_kinds[self.code_keys_end..].partition_point(|&k| k < target_kind) + self.code_keys_end;
            let end = self.cell_kinds[start..].partition_point(|&k| k <= target_kind) + start;
            (start, end)
        };
        if lo >= hi {
            return None;
        }

        let target_prefix = read_u64_be(&name.0);
        let mut stale = 0u8;

        // Bounded: every iteration strictly narrows `[lo, hi)`.
        while lo < hi {
            let span = hi - lo;
            let mid = if span <= 8 || stale >= 3 {
                lo + span / 2
            } else {
                let lo_p = read_u64_be(&self.cell_names[lo * 32..lo * 32 + 8]);
                let hi_p = read_u64_be(&self.cell_names[(hi - 1) * 32..(hi - 1) * 32 + 8]);
                if target_prefix < lo_p || target_prefix > hi_p {
                    return None;
                }
                if hi_p == lo_p {
                    lo + span / 2
                } else {
                    let num = u128::from(target_prefix - lo_p) * u128::from((span - 1) as u64);
                    let den = u128::from(hi_p - lo_p);
                    let guess = lo + (num / den) as usize;
                    guess.clamp(lo, hi - 1)
                }
            };

            let mid_name = &self.cell_names[mid * 32..mid * 32 + 32];
            match mid_name.cmp(&name.0) {
                core::cmp::Ordering::Equal => return Some(mid),
                core::cmp::Ordering::Less => {
                    let next_lo = mid + 1;
                    if hi - next_lo > span / 2 {
                        stale = stale.saturating_add(1);
                    } else {
                        stale = 0;
                    }
                    lo = next_lo;
                }
                core::cmp::Ordering::Greater => {
                    if mid - lo > span / 2 {
                        stale = stale.saturating_add(1);
                    } else {
                        stale = 0;
                    }
                    hi = mid;
                }
            }
        }
        None
    }

    /// Selects affected tests directly over the zero-copy `BTI1` columns with zero reindexing.
    ///
    /// Produces the exact same [`Selection`] (ascending `test_idx`, early cutoff when
    /// `new_value == expected_value`, exact first moved cell by `row_order`) as
    /// [`ImpactIndex::select`].
    pub fn select<S: Allocator + Clone>(
        &self,
        changed: &[(CellKind, Digest, Digest)],
        scratch: S,
    ) -> Result<Selection<S>, PersistError> {
        #[derive(Clone, Copy)]
        struct Hit {
            test_idx: u32,
            row_order: u32,
            moved: Read,
        }

        let mut hits: Vec<Hit, S> = Vec::new_in(scratch.clone());
        for &(kind, name, new_value) in changed {
            let Some(k) = self.find_cell(kind, name) else {
                continue;
            };
            let start = read_u32_le(self.cell_offsets, k * 4) as usize;
            let end = read_u32_le(self.cell_offsets, (k + 1) * 4) as usize;
            if start > end || end > self.posting_count {
                return Err(PersistError::CorruptOffsets);
            }
            for p in start..end {
                let expected = read_digest(self.posting_expected, p);
                if new_value == expected {
                    continue;
                }
                let test_idx = read_u32_le(self.posting_test_idx, p * 4);
                if test_idx as usize >= self.test_count {
                    return Err(PersistError::CorruptOffsets);
                }
                let row_order = read_u32_le(self.posting_row_order, p * 4);
                hits.push(Hit { test_idx, row_order, moved: Read { kind, name, value: expected } });
            }
        }

        hits.sort_unstable_by(|a, b| a.test_idx.cmp(&b.test_idx).then(a.row_order.cmp(&b.row_order)));

        // Deduplicate hits by `test_idx`, keeping the minimum `row_order` (first moved cell).
        let mut unique_moved: Vec<SelectedTest, S> = Vec::new_in(scratch.clone());
        let mut prev_test: Option<u32> = None;
        for h in hits.iter() {
            if prev_test == Some(h.test_idx) {
                continue;
            }
            prev_test = Some(h.test_idx);
            unique_moved.push(SelectedTest {
                test_idx: h.test_idx,
                test: read_digest(self.test_digests, h.test_idx as usize),
                because: Because::Moved(h.moved),
            });
        }

        // Merge with `always_rerun` (already sorted ascending by `test_idx` and disjoint).
        let mut rerun: Vec<SelectedTest, S> = Vec::with_capacity_in(unique_moved.len() + self.always_count, scratch);
        let mut mi = 0usize;
        let mut ai = 0usize;
        while mi < unique_moved.len() && ai < self.always_count {
            let (a_idx, a_because) = self.read_always(ai)?;
            if unique_moved[mi].test_idx < a_idx {
                rerun.push(unique_moved[mi]);
                mi += 1;
            } else {
                rerun.push(SelectedTest {
                    test_idx: a_idx,
                    test: read_digest(self.test_digests, a_idx as usize),
                    because: a_because,
                });
                ai += 1;
            }
        }
        while mi < unique_moved.len() {
            rerun.push(unique_moved[mi]);
            mi += 1;
        }
        while ai < self.always_count {
            let (a_idx, a_because) = self.read_always(ai)?;
            rerun.push(SelectedTest {
                test_idx: a_idx,
                test: read_digest(self.test_digests, a_idx as usize),
                because: a_because,
            });
            ai += 1;
        }

        Ok(Selection::from_parts(self.test_count, rerun))
    }

    fn read_always(&self, idx: usize) -> Result<(u32, Because), PersistError> {
        let off = idx * ALWAYS_ROW_BYTES;
        let test_idx = read_u32_le(self.always_rerun, off);
        if test_idx as usize >= self.test_count {
            return Err(PersistError::CorruptOffsets);
        }
        let tag = self.always_rerun[off + 4];
        let aux = self.always_rerun[off + 5];
        let mut d = [0u8; 32];
        d.copy_from_slice(&self.always_rerun[off + 8..off + 40]);
        let because = match tag {
            TAG_NO_RECORD => Because::NoRecord,
            TAG_NOT_REUSABLE => {
                let why = NotReusable::from_byte(aux).ok_or(PersistError::BadAlwaysTag { tag })?;
                Because::NotReusable(why)
            }
            TAG_OTHER_TEST => Because::OtherTest { found: Digest(d) },
            TAG_UNREADABLE => Because::Unreadable(decode_record_error(aux, &d)?),
            _ => return Err(PersistError::BadAlwaysTag { tag }),
        };
        Ok((test_idx, because))
    }
}
