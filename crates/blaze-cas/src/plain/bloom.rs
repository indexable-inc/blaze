//! Persistent per-segment cache-line blocked Bloom filter sidecar (`bloom`).
//!
//! # Soundness contract
//!
//! A Bloom filter is a pure acceleration structure over content-addressed segments:
//! - It NEVER produces false negatives: if a key is in segment `seg`, any valid filter bound to
//!   `seg.digest` returns `true` from `may_contain`.
//! - Every on-disk record is bound to the segment's full 32-byte BLAKE3 `ContentId` digest and
//!   protected by a CRC-32C covering its header and body.
//! - Each mapped record's CRC is verified lazily once before its first negative answer is trusted.
//!   A missing, truncated, wrong-version, wrong-magic, or CRC-failing record is ignored (treated
//!   as `may_contain == true`), so the reader falls back to probing the segment normally.
//! - Because segments are immutable by digest, a verified record for `seg.digest` stays valid
//!   across generations and processes.
//!
//! # Layout (append-only framed records, 16-byte aligned)
//!
//! ```text
//! Header (48 bytes):
//!   0..4    MAGIC ("BLPB")
//!   4..8    VERSION (1u32 LE)
//!   8..40   segment digest ([u8; 32])
//!   40..44  blocks (u32 LE; number of 64-byte blocks, 0 for an empty segment)
//!   44..48  crc (u32 LE; CRC-32C of header[0..44] extended with body[0..blocks * 64])
//! Body (blocks * 64 bytes):
//!   blocks x 64-byte (512-bit) cache-line filter blocks (8 x u64 LE words)
//! ```

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use core::sync::atomic::{AtomicU8, Ordering};

use blaze_core::Mapping;
use blaze_hash::{BuildKeyHasher, Map, MapOpts};
use blaze_ids::{ContentId, crc32c};
use blaze_keyed::store::ENTRY;
use blaze_mem::{Kept, Tag as MemTag};
use blaze_own::{Arc, Share, Vec};

use super::format::Segment;

/// Shared store-tagged allocator for owned in-memory Bloom buffers (`Send + Sync`, zero `Global`).
pub(crate) const BLOOM_ALLOC: Kept = Kept(MemTag::Store);

/// Magic bytes at the start of every Bloom record in the `bloom` sidecar.
pub(crate) const MAGIC: [u8; 4] = *b"BLPB";

/// Format version of a Bloom record.
pub(crate) const VERSION: u32 = 1;

/// Fixed record header size in bytes (16-byte aligned).
pub(crate) const RECORD_HEADER: usize = 48;
const RECORD_HEADER_U64: u64 = 48;

/// Size of one cache-line Bloom block in bytes (512 bits = 8 x u64).
pub(crate) const BLOCK_BYTES: usize = 64;
const BLOCK_BYTES_U64: u64 = 64;

/// Target bits per key: 12 bits/key with 7 probes in a 512-bit block yields ~0.48% false-positive
/// rate (~1.5 bytes/key), skipping >99.5% of segments on negative or deep lookups.
const BITS_PER_KEY: u64 = 12;

/// Number of bits probed per key within its selected 512-bit block (7 x 9 bits = 63 bits from one u64).
const PROBES: usize = 7;

/// Maximum blocks in one segment's Bloom filter (~128 MiB cap; covers >89 million entries in one
/// segment, well above the 4 GiB / 48-byte segment entry cap of ~89.4M).
const BLOCKS_MOST: u32 = 2_200_000;

/// Verification state of a mapped Bloom record.
const STATE_UNVERIFIED: u8 = 0;
const STATE_VALID: u8 = 1;
const STATE_CORRUPT: u8 = 2;

/// Bijective 64-bit mixer so even sequential synthetic keys in tests get full avalanche.
#[inline]
fn mix64(mut x: u64) -> u64 {
    x ^= x >> 30;
    x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// Derives two independent 64-bit hashes `(block_hash, bits_hash)` from a 32-byte key.
#[inline]
fn key_hashes(key: &[u8; 32]) -> (u64, u64) {
    let [
        b0, b1, b2, b3, b4, b5, b6, b7,
        b8, b9, b10, b11, b12, b13, b14, b15,
        b16, b17, b18, b19, b20, b21, b22, b23,
        b24, b25, b26, b27, b28, b29, b30, b31,
    ] = *key;
    let w0 = u64::from_le_bytes([b0, b1, b2, b3, b4, b5, b6, b7]);
    let w1 = u64::from_le_bytes([b8, b9, b10, b11, b12, b13, b14, b15]);
    let w2 = u64::from_le_bytes([b16, b17, b18, b19, b20, b21, b22, b23]);
    let w3 = u64::from_le_bytes([b24, b25, b26, b27, b28, b29, b30, b31]);
    let a = mix64(w0 ^ w2.rotate_left(17));
    let b = mix64(w1 ^ w3.rotate_left(29) ^ 0x9e37_79b9_7f4a_7c15);
    (a, b)
}

/// Lemire fast range reduction: maps uniform `hash` into `0..blocks`.
#[inline]
fn block_index(hash: u64, blocks: u32) -> usize {
    let prod = u128::from(hash) * u128::from(blocks);
    usize::try_from(prod >> 64).unwrap_or(0)
}

/// Number of 64-byte blocks sized for `count` keys.
pub(crate) fn blocks_for(count: u32) -> u32 {
    if count == 0 {
        return 0;
    }
    let blocks = u64::from(count).saturating_mul(BITS_PER_KEY).div_ceil(512).max(1);
    u32::try_from(blocks).unwrap_or(BLOCKS_MOST).min(BLOCKS_MOST)
}

/// Probes `key` against a raw filter body of `blocks * BLOCK_BYTES` bytes.
#[inline]
pub(crate) fn probe_bytes(body: &[u8], blocks: u32, key: &[u8; 32]) -> bool {
    if blocks == 0 {
        return false;
    }
    let (a, b) = key_hashes(key);
    let base = block_index(a, blocks).saturating_mul(BLOCK_BYTES);
    let Some(block) = body.get(base..base.saturating_add(BLOCK_BYTES)) else {
        return true;
    };
    let mut shift = 0u32;
    for _ in 0..PROBES {
        let bit = usize::try_from((b >> shift) & 511).unwrap_or(0);
        let Some(&byte) = block.get(bit >> 3) else {
            return true;
        };
        let mask = 1u8 << (bit & 7);
        if byte & mask == 0 {
            return false;
        }
        shift += 9;
    }
    true
}

/// Builder for a single segment's blocked Bloom filter.
pub(crate) struct BloomBuilder {
    blocks: u32,
    body: Vec<u8, Kept>,
}

impl BloomBuilder {
    /// Allocates a zeroed filter sized for `count` keys.
    pub(crate) fn new(count: u32) -> BloomBuilder {
        let blocks = blocks_for(count);
        let byte_len = usize::try_from(blocks).unwrap_or(0).saturating_mul(BLOCK_BYTES);
        let mut body = Vec::with_capacity_in(byte_len, BLOOM_ALLOC);
        body.resize(byte_len, 0);
        BloomBuilder { blocks, body }
    }

    /// Inserts one 32-byte key into the filter.
    #[inline]
    pub(crate) fn insert(&mut self, key: &[u8; 32]) {
        if self.blocks == 0 {
            return;
        }
        let (a, b) = key_hashes(key);
        let base = block_index(a, self.blocks).saturating_mul(BLOCK_BYTES);
        let Some(block) = self.body.get_mut(base..base.saturating_add(BLOCK_BYTES)) else {
            return;
        };
        let mut shift = 0u32;
        for _ in 0..PROBES {
            let bit = usize::try_from((b >> shift) & 511).unwrap_or(0);
            if let Some(slot) = block.get_mut(bit >> 3) {
                *slot |= 1u8 << (bit & 7);
            }
            shift += 9;
        }
    }

    /// Builds a complete framed record directly from a segment's sorted `run` of `ENTRY`-byte rows.
    pub(crate) fn from_run(run: &[u8], digest: &ContentId) -> Vec<u8, Kept> {
        let count = u32::try_from(run.len() / ENTRY).unwrap_or(u32::MAX);
        let mut builder = BloomBuilder::new(count);
        let mut at = 0usize;
        while let Some(chunk) = run.get(at..at.saturating_add(32)) {
            if let Ok(key) = <[u8; 32]>::try_from(chunk) {
                builder.insert(&key);
            }
            at = at.saturating_add(ENTRY);
        }
        builder.finish(digest)
    }

    /// Encodes this filter as one framed record (`RECORD_HEADER + body`).
    pub(crate) fn finish(self, digest: &ContentId) -> Vec<u8, Kept> {
        let mut out = Vec::with_capacity_in(RECORD_HEADER.saturating_add(self.body.len()), BLOOM_ALLOC);
        self.finish_into(digest, &mut out);
        out
    }

    /// Appends this filter as one framed record into `out`.
    pub(crate) fn finish_into(&self, digest: &ContentId, out: &mut Vec<u8, Kept>) {
        let mut head = [0u8; RECORD_HEADER];
        if let Some(slot) = head.get_mut(0..4) {
            slot.copy_from_slice(&MAGIC);
        }
        if let Some(slot) = head.get_mut(4..8) {
            slot.copy_from_slice(&VERSION.to_le_bytes());
        }
        if let Some(slot) = head.get_mut(8..40) {
            slot.copy_from_slice(&digest.0);
        }
        if let Some(slot) = head.get_mut(40..44) {
            slot.copy_from_slice(&self.blocks.to_le_bytes());
        }
        let prefix = head.get(0..44).unwrap_or(&[]);
        let crc = crc32c::extend(crc32c::crc32c(prefix), &self.body);
        if let Some(slot) = head.get_mut(44..48) {
            slot.copy_from_slice(&crc.to_le_bytes());
        }
        out.extend_from_slice(&head);
        out.extend_from_slice(&self.body);
    }
}

/// Computes the expected CRC-32C for a framed record at `record[..RECORD_HEADER + body_len]`.
#[inline]
fn record_crc_valid(record: &[u8], expected: u32) -> bool {
    let Some(prefix) = record.get(0..44) else {
        return false;
    };
    let Some(body) = record.get(RECORD_HEADER..) else {
        return false;
    };
    crc32c::extend(crc32c::crc32c(prefix), body) == expected
}

/// Parsed record header from a mapped `bloom` file.
#[derive(Clone, Copy)]
struct ParsedHeader {
    digest: ContentId,
    blocks: u32,
    crc: u32,
    record_len: usize,
}

fn parse_header(bytes: &[u8], at: usize) -> Option<ParsedHeader> {
    let head = bytes.get(at..at.checked_add(RECORD_HEADER)?)?;
    if head.get(0..4)? != MAGIC {
        return None;
    }
    let version = u32::from_le_bytes(<[u8; 4]>::try_from(head.get(4..8)?).ok()?);
    if version != VERSION {
        return None;
    }
    let blocks = u32::from_le_bytes(<[u8; 4]>::try_from(head.get(40..44)?).ok()?);
    if blocks > BLOCKS_MOST {
        return None;
    }
    let body_len = usize::try_from(blocks).ok()?.checked_mul(BLOCK_BYTES)?;
    let record_len = RECORD_HEADER.checked_add(body_len)?;
    if at.checked_add(record_len)? > bytes.len() {
        return None;
    }
    let digest = <[u8; 32]>::try_from(head.get(8..40)?).ok()?;
    let crc = u32::from_le_bytes(<[u8; 4]>::try_from(head.get(44..48)?).ok()?);
    Some(ParsedHeader { digest: ContentId(digest), blocks, crc, record_len })
}

#[inline]
fn segment_id_of(digest: &ContentId) -> u64 {
    let [b0, b1, b2, b3, b4, b5, b6, b7, ..] = digest.0;
    u64::from_le_bytes([b0, b1, b2, b3, b4, b5, b6, b7])
}

/// One segment's filter entry inside [`BloomIndex`].
enum BloomSlot {
    /// Located in `BloomIndex::mapping` at `offset..offset + RECORD_HEADER + blocks * BLOCK_BYTES`.
    Mapped {
        digest: ContentId,
        offset: usize,
        blocks: u32,
        crc: u32,
        state_idx: u32,
    },
    /// Built in memory during this process (after a commit or merge), already valid.
    Owned {
        digest: ContentId,
        blocks: u32,
        /// Complete framed record (`RECORD_HEADER + body`), shared zero-copy with [`BloomRef`].
        record: Arc<Vec<u8, Kept>, Kept>,
    },
}

/// Shared mapped `bloom` sidecar and per-slot atomic verification states carried once by a
/// `Snapshot` so taking a snapshot does not clone a `Mapping` per segment or eagerly CRC-hash
/// filters for segments that are never probed.
pub(crate) struct BloomMapped {
    map: Mapping,
    states: Arc<Vec<AtomicU8, Kept>, Kept>,
}

/// Read-only, `Send + Sync`, zero-copy handle to a single segment Bloom filter, carried
/// inside `Snapshot`'s listed segments so worker threads skip non-matching segments in ~2 ns
/// while verifying each mapped record's CRC lazily once on first probe.
pub(crate) enum BloomRef {
    Mapped {
        offset: usize,
        blocks: u32,
        crc: u32,
        state_idx: u32,
        trust_corrupt: bool,
    },
    Owned {
        record: Arc<Vec<u8, Kept>, Kept>,
        blocks: u32,
    },
}

impl BloomRef {
    /// Returns `false` only if `key` is guaranteed absent from this segment.
    #[inline]
    pub(crate) fn may_contain(&self, shared: Option<&BloomMapped>, key: &[u8; 32]) -> bool {
        match self {
            BloomRef::Mapped { offset, blocks, crc, state_idx, trust_corrupt } => {
                let Some(shared) = shared else {
                    return true;
                };
                let Some(body_len) = usize::try_from(*blocks).ok().and_then(|b| b.checked_mul(BLOCK_BYTES)) else {
                    return true;
                };
                let Some(end) = offset.checked_add(RECORD_HEADER).and_then(|it| it.checked_add(body_len)) else {
                    return true;
                };
                let Some(record) = shared.map.bytes().get(*offset..end) else {
                    return true;
                };
                let Some(slot_state) = usize::try_from(*state_idx).ok().and_then(|i| shared.states.get(i)) else {
                    return true;
                };
                let state = slot_state.load(Ordering::Acquire);
                if state == STATE_CORRUPT {
                    return true;
                }
                if state == STATE_UNVERIFIED && !*trust_corrupt {
                    if record_crc_valid(record, *crc) {
                        slot_state.store(STATE_VALID, Ordering::Release);
                    } else {
                        slot_state.store(STATE_CORRUPT, Ordering::Release);
                        return true;
                    }
                }
                let Some(body) = record.get(RECORD_HEADER..) else {
                    return true;
                };
                probe_bytes(body, *blocks, key)
            }
            BloomRef::Owned { record, blocks } => {
                let Some(body) = record.get(RECORD_HEADER..) else {
                    return true;
                };
                probe_bytes(body, *blocks, key)
            }
        }
    }
}

/// Per-store index of Bloom filters for segments in the manifest.
pub(crate) struct BloomIndex {
    mapping: Option<Mapping>,
    /// Shared per-slot atomic verification states for mapped records (1 allocation per open).
    states: Option<Arc<Vec<AtomicU8, Kept>, Kept>>,
    /// Keyed by segment `id` (`u64::from_le_bytes(digest[0..8])`).
    entries: Map<u64, BloomSlot, BuildKeyHasher, Kept>,
    /// Total file size of the mapped `bloom` file when loaded (for dead-byte compaction heuristic).
    mapped_bytes: u64,
}

impl BloomIndex {
    pub(crate) fn new() -> BloomIndex {
        BloomIndex {
            mapping: None,
            states: None,
            entries: Map::new(MapOpts { hasher: BuildKeyHasher::default(), alloc: BLOOM_ALLOC, capacity: None }),
            mapped_bytes: 0,
        }
    }

    /// Attaches a mapped `bloom` file read at open time. Indexing happens in [`Self::index_manifest`].
    pub(crate) fn set_mapping(&mut self, map: Mapping) {
        self.mapped_bytes = u64::try_from(map.bytes().len()).unwrap_or(u64::MAX);
        self.mapping = Some(map);
    }

    /// Returns a shared handle to the mapped `bloom` file and verification states for `Snapshot`.
    pub(crate) fn shared_mapped(&self) -> Option<BloomMapped> {
        Some(BloomMapped { map: self.mapping.as_ref()?.share(), states: self.states.as_ref()?.share() })
    }

    /// Walks framed headers in the mapped `bloom` file in O(records + segments) (touching only 48
    /// bytes per record, zero body CRC checks) and indexes records whose `digest` matches a
    /// segment in `segments`. Any trailing torn/foreign bytes stop the walk cleanly without
    /// failing open.
    pub(crate) fn index_manifest(&mut self, segments: &[Segment]) {
        let Some(map) = self.mapping.as_ref() else {
            return;
        };
        let bytes = map.bytes();
        let mut by_seg_id: Option<Map<u64, usize, BuildKeyHasher, Kept>> = if segments.len() > 16 {
            let mut m = Map::new(MapOpts { hasher: BuildKeyHasher::default(), alloc: BLOOM_ALLOC, capacity: Some(segments.len()) });
            for (i, seg) in segments.iter().enumerate() {
                m.insert(seg.id, i);
            }
            Some(m)
        } else {
            None
        };
        let mut states = Vec::with_capacity_in(segments.len(), BLOOM_ALLOC);
        let mut at = 0usize;
        while at < bytes.len() {
            let Some(hdr) = parse_header(bytes, at) else {
                break;
            };
            let id = segment_id_of(&hdr.digest);
            let matched = if let Some(ref map_ids) = by_seg_id {
                match map_ids.get(&id) {
                    Some(&idx) => {
                        segments.get(idx).is_some_and(|seg| seg.digest == hdr.digest)
                            || segments.iter().any(|seg| seg.id == id && seg.digest == hdr.digest)
                    }
                    None => false,
                }
            } else {
                segments.iter().any(|seg| seg.id == id && seg.digest == hdr.digest)
            };
            if matched {
                let state_idx = u32::try_from(states.len()).unwrap_or(u32::MAX);
                if state_idx != u32::MAX {
                    states.push(AtomicU8::new(STATE_UNVERIFIED));
                    self.entries.insert(
                        id,
                        BloomSlot::Mapped {
                            digest: hdr.digest,
                            offset: at,
                            blocks: hdr.blocks,
                            crc: hdr.crc,
                            state_idx,
                        },
                    );
                }
            }
            at = at.saturating_add(hdr.record_len);
        }
        let _ = by_seg_id.take();
        self.states = Some(Arc::new_in(states, BLOOM_ALLOC));
    }

    /// Inserts a newly built framed record (from a commit or merge in this process) into memory.
    pub(crate) fn insert_owned(&mut self, record_bytes: Vec<u8, Kept>) {
        let Some(hdr) = parse_header(&record_bytes, 0) else {
            return;
        };
        let id = segment_id_of(&hdr.digest);
        let record = Arc::new_in(record_bytes, BLOOM_ALLOC);
        self.entries.insert(id, BloomSlot::Owned { digest: hdr.digest, blocks: hdr.blocks, record });
    }

    /// Prunes entries whose segment is no longer present in `segments`, preventing long-lived
    /// processes from retaining owned Bloom buffers for retired segments.
    pub(crate) fn prune(&mut self, segments: &[Segment]) {
        self.entries.retain(|&id, slot| {
            let digest = match slot {
                BloomSlot::Mapped { digest, .. } | BloomSlot::Owned { digest, .. } => *digest,
            };
            segments.iter().any(|seg| seg.id == id && seg.digest == digest)
        });
    }

    /// Updates tracked sidecar byte length after a successful Bloom write (`replaced = true` for
    /// full compaction/backfill rewrite, `false` for append).
    pub(crate) fn note_written(&mut self, replaced: bool, len: u64) {
        if replaced {
            self.mapped_bytes = len;
        } else {
            self.mapped_bytes = self.mapped_bytes.saturating_add(len);
        }
    }

    /// Returns `false` ONLY when a verified filter for `seg.digest` proves `key` is absent.
    /// Returns `true` if no filter exists, the digest mismatches, or the record fails CRC.
    /// When `trust_corrupt` is true (planted DST bug control only), skips the CRC check.
    #[inline]
    pub(crate) fn may_contain(&self, seg: &Segment, key: &[u8; 32], trust_corrupt: bool) -> bool {
        let Some(slot) = self.entries.get(&seg.id) else {
            return true;
        };
        match slot {
            BloomSlot::Mapped { digest, offset, blocks, crc, state_idx } => {
                if *digest != seg.digest {
                    return true;
                }
                let Some(map) = self.mapping.as_ref() else {
                    return true;
                };
                let Some(slot_state) = self.states.as_ref().and_then(|s| usize::try_from(*state_idx).ok().and_then(|i| s.get(i))) else {
                    return true;
                };
                let Some(body_len) = usize::try_from(*blocks).ok().and_then(|b| b.checked_mul(BLOCK_BYTES)) else {
                    return true;
                };
                let Some(end) = offset.checked_add(RECORD_HEADER).and_then(|it| it.checked_add(body_len)) else {
                    return true;
                };
                let Some(record) = map.bytes().get(*offset..end) else {
                    return true;
                };
                let state = slot_state.load(Ordering::Acquire);
                if state == STATE_CORRUPT {
                    return true;
                }
                if state == STATE_UNVERIFIED && !trust_corrupt {
                    if record_crc_valid(record, *crc) {
                        slot_state.store(STATE_VALID, Ordering::Release);
                    } else {
                        slot_state.store(STATE_CORRUPT, Ordering::Release);
                        return true;
                    }
                }
                let Some(body) = record.get(RECORD_HEADER..) else {
                    return true;
                };
                probe_bytes(body, *blocks, key)
            }
            BloomSlot::Owned { digest, blocks, record } => {
                if *digest != seg.digest {
                    return true;
                }
                let Some(body) = record.get(RECORD_HEADER..) else {
                    return true;
                };
                probe_bytes(body, *blocks, key)
            }
        }
    }

    /// Produces a lightweight `Send + Sync` [`BloomRef`] for `seg` to attach to a `Snapshot`.
    /// Defers mapped-record CRC verification lazily to the first probe of `seg` (zero page faults
    /// and zero CRC hashing during `Snapshot::of`); returns `None` if absent or already known corrupt.
    pub(crate) fn snapshot_ref(&self, seg: &Segment, trust_corrupt: bool) -> Option<BloomRef> {
        let slot = self.entries.get(&seg.id)?;
        match slot {
            BloomSlot::Mapped { digest, offset, blocks, crc, state_idx } => {
                if *digest != seg.digest {
                    return None;
                }
                let slot_state = self.states.as_ref().and_then(|s| usize::try_from(*state_idx).ok().and_then(|i| s.get(i)))?;
                if !trust_corrupt && slot_state.load(Ordering::Acquire) == STATE_CORRUPT {
                    return None;
                }
                Some(BloomRef::Mapped { offset: *offset, blocks: *blocks, crc: *crc, state_idx: *state_idx, trust_corrupt })
            }
            BloomSlot::Owned { digest, blocks, record } => {
                if *digest != seg.digest {
                    return None;
                }
                Some(BloomRef::Owned { record: record.share(), blocks: *blocks })
            }
        }
    }

    /// Returns the raw verified framed record bytes for `seg` if already present and valid.
    pub(crate) fn raw_record(&self, seg: &Segment) -> Option<&[u8]> {
        let slot = self.entries.get(&seg.id)?;
        match slot {
            BloomSlot::Mapped { digest, offset, blocks, crc, state_idx } => {
                if *digest != seg.digest {
                    return None;
                }
                let map = self.mapping.as_ref()?;
                let slot_state = self.states.as_ref().and_then(|s| usize::try_from(*state_idx).ok().and_then(|i| s.get(i)))?;
                let body_len = usize::try_from(*blocks).ok()?.checked_mul(BLOCK_BYTES)?;
                let end = offset.checked_add(RECORD_HEADER)?.checked_add(body_len)?;
                let record = map.bytes().get(*offset..end)?;
                let state = slot_state.load(Ordering::Acquire);
                if state == STATE_CORRUPT {
                    return None;
                }
                if state == STATE_UNVERIFIED {
                    if record_crc_valid(record, *crc) {
                        slot_state.store(STATE_VALID, Ordering::Release);
                    } else {
                        slot_state.store(STATE_CORRUPT, Ordering::Release);
                        return None;
                    }
                }
                Some(record)
            }
            BloomSlot::Owned { digest, record, .. } => {
                if *digest != seg.digest {
                    return None;
                }
                Some(record.as_slice())
            }
        }
    }

    /// Whether `seg` already has a known (non-corrupt) Bloom entry matching `seg.digest`.
    pub(crate) fn has(&self, seg: &Segment) -> bool {
        let Some(slot) = self.entries.get(&seg.id) else {
            return false;
        };
        match slot {
            BloomSlot::Mapped { digest, state_idx, .. } => {
                *digest == seg.digest
                    && self
                        .states
                        .as_ref()
                        .and_then(|s| usize::try_from(*state_idx).ok().and_then(|i| s.get(i)))
                        .is_some_and(|st| st.load(Ordering::Acquire) != STATE_CORRUPT)
            }
            BloomSlot::Owned { digest, .. } => *digest == seg.digest,
        }
    }

    /// Whether the on-disk `bloom` file has accumulated enough dead bytes from retired segments
    /// (more than 2x live + 64 KiB) to warrant rewriting compactly.
    pub(crate) fn needs_compact(&self, segments: &[Segment]) -> bool {
        let mut live = 0u64;
        for seg in segments.iter().filter(|it| it.local) {
            if let Some(slot) = self.entries.get(&seg.id) {
                let blocks = match slot {
                    BloomSlot::Mapped { digest, blocks, .. } if *digest == seg.digest => *blocks,
                    BloomSlot::Owned { digest, blocks, .. } if *digest == seg.digest => *blocks,
                    _ => continue,
                };
                live = live.saturating_add(RECORD_HEADER_U64.saturating_add(u64::from(blocks).saturating_mul(BLOCK_BYTES_U64)));
            }
        }
        self.mapped_bytes > live.saturating_mul(2).saturating_add(64 << 10)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn previous_bloom_version_is_ignored() {
        let digest = ContentId::of("bloom version test", b"seg");
        let mut record = BloomBuilder::new(4).finish(&digest);
        // Overwrite VERSION (bytes 4..8) with version 0 (a previous layout).
        record[4..8].copy_from_slice(&0u32.to_le_bytes());
        assert!(parse_header(&record, 0).is_none(), "a previous bloom version must be ignored");
        // Overwrite with version 2 (a future layout).
        record[4..8].copy_from_slice(&2u32.to_le_bytes());
        assert!(parse_header(&record, 0).is_none(), "a future bloom version must be ignored");
    }

    #[test]
    fn zero_false_negatives_and_low_false_positives() {
        let count = 10_000u32;
        let mut builder = BloomBuilder::new(count);
        for i in 0..count {
            let key = ContentId::of("bloom present", &i.to_le_bytes());
            builder.insert(&key.0);
        }
        let digest = ContentId::of("bloom seg", b"10k");
        let record = builder.finish(&digest);
        let hdr = parse_header(&record, 0).expect("valid record");
        assert!(record_crc_valid(&record, hdr.crc));
        let body = &record[RECORD_HEADER..];
        for i in 0..count {
            let key = ContentId::of("bloom present", &i.to_le_bytes());
            assert!(probe_bytes(body, hdr.blocks, &key.0), "false negative on key {i}");
        }
        let mut fp = 0u32;
        let probes = 20_000u32;
        for i in 0..probes {
            let key = ContentId::of("bloom absent", &i.to_le_bytes());
            if probe_bytes(body, hdr.blocks, &key.0) {
                fp += 1;
            }
        }
        // Expected ~0.5% (~100 of 20,000); allow generous margin < 1.5% (300).
        assert!(fp < 300, "false positive rate too high: {fp} / {probes}");
    }

    #[test]
    fn empty_segment_rejects_everything() {
        let digest = ContentId::of("bloom empty", b"0");
        let record = BloomBuilder::new(0).finish(&digest);
        let hdr = parse_header(&record, 0).expect("valid empty record");
        assert_eq!(hdr.blocks, 0);
        assert!(record_crc_valid(&record, hdr.crc));
        let key = ContentId::of("any", b"k");
        assert!(!probe_bytes(&record[RECORD_HEADER..], hdr.blocks, &key.0));
    }

    #[test]
    fn note_written_tracks_growth_and_prune_drops_retired_segments() {
        let mut idx = BloomIndex::new();
        let d1 = ContentId::of("seg", b"1");
        let d2 = ContentId::of("seg", b"2");
        let mut b1 = BloomBuilder::new(2);
        b1.insert(&ContentId::of("k", b"1").0);
        let r1 = b1.finish(&d1);
        let mut b2 = BloomBuilder::new(2);
        b2.insert(&ContentId::of("k", b"2").0);
        let r2 = b2.finish(&d2);
        let r1_len = u64::try_from(r1.len()).unwrap();
        let r2_len = u64::try_from(r2.len()).unwrap();
        idx.insert_owned(r1);
        idx.insert_owned(r2);
        idx.note_written(false, r1_len);
        idx.note_written(false, r2_len);
        assert_eq!(idx.mapped_bytes, r1_len + r2_len);

        let live_seg = Segment {
            id: segment_id_of(&d1),
            digest: d1,
            len: 0,
            index_at: 0,
            count: 1,
            namespace: crate::plain::Namespace::LOCAL,
            generation: 1,
            boot: 1,
            local: true,
            published: false,
            sealed: true,
            touched: 0,
        };
        idx.prune(core::slice::from_ref(&live_seg));
        assert_eq!(idx.entries.len(), 1);
        assert!(idx.entries.contains_key(&live_seg.id));
        idx.note_written(true, r1_len);
        assert_eq!(idx.mapped_bytes, r1_len);
        assert!(!idx.needs_compact(core::slice::from_ref(&live_seg)));
    }
}
