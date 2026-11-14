//! A plain store segment, read as the bucket needs it. A push takes the record list from it (to
//! index it and to digest each value), and a pull checks a fetched segment before handing it on.
//!
//! The layout is the plain store's (crates/blaze-cas/src/plain/format.rs, whose reader is
//! `pub(crate)` there). This is a second reader of one format, to be replaced by
//! `blaze_cas::plain` exporting `check_segment` and the entry decoder. The report lists this. A
//! test builds segments with [`make`] and the plain store's `LocalDir::put`, which checks them
//! with its own reader, so the two readers are held to one format.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use blaze_cas::plain::{Namespace, SEGMENT_MOST};
use blaze_ids::ContentId;

use crate::wire::{id_at, key_at, u32_at, u64_at};

pub(crate) const HEADER: usize = 128;
pub(crate) const FOOTER: usize = 48;
pub(crate) const ENTRY: usize = 48;
const FORMAT: u32 = 1;
const DIGEST_DOMAIN: &str = "blaze plain store segment 1";

/// One record of a segment: its key, where its payload is, its kind byte and the digest of
/// (kind, payload) the index carries ([`value_digest`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rec {
    pub key: [u8; 32],
    pub at: u64,
    pub len: u32,
    pub kind: u8,
    pub value: ContentId,
}

/// A whole segment, checked: its header and footer agree, its digest is right, its entries are
/// sorted and inside its payloads.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Read {
    pub namespace: Namespace,
    pub digest: ContentId,
    pub len: u64,
    pub index_at: u64,
    pub records: Vec<Rec>,
}

/// The digest of a record's value as the index carries it: kind byte, then payload. Two builders
/// of one key must produce equal ones (rule deterministic-output).
pub fn value_digest(kind: u8, payload: &[u8]) -> ContentId {
    let mut hasher = blaze_ids::Hasher::new("blaze-remote value 1");
    hasher.update(&[kind]).update(payload);
    hasher.finish()
}

/// The digest a segment's footer names, of `bytes[..len - 32]`.
pub fn digest_of(bytes: &[u8]) -> Option<ContentId> {
    (bytes.len() >= HEADER + FOOTER).then(|| ContentId::of(DIGEST_DOMAIN, &bytes[..bytes.len() - 32]))
}

/// Checks a whole segment and reads its records. Why not, as a fixed phrase.
pub fn read(bytes: &[u8]) -> Result<Read, &'static str> {
    let len = bytes.len();
    if len < HEADER + FOOTER || len as u64 > SEGMENT_MOST {
        return Err("its length is outside a segment's");
    }
    if bytes[0..4] != *b"BLPS" || u32_at(bytes, 4) != FORMAT {
        return Err("it is not a plain segment of format 1");
    }
    let foot = &bytes[len - FOOTER..];
    let (count, index_at) = (u32_at(bytes, 16), u64_at(bytes, 24));
    if foot[0..4] != *b"BLPE" || u32_at(foot, 4) != count || u64_at(foot, 8) != index_at {
        return Err("its footer does not say what its header says");
    }
    let end = index_at.checked_add(u64::from(count) * ENTRY as u64).and_then(|it| it.checked_add(FOOTER as u64));
    if index_at < HEADER as u64 || index_at % 16 != 0 || end != Some(len as u64) {
        return Err("its index does not end where the file does");
    }
    let digest = id_at(foot, 16);
    crate::trust::note(crate::trust::Check::Digest, digest.0);
    if !crate::planted(crate::Plant::SkipSegmentDigest) && digest_of(bytes) != Some(digest) {
        return Err("its bytes are not the ones its digest names");
    }
    let mut records = Vec::with_capacity(count as usize);
    let run = &bytes[index_at as usize..index_at as usize + count as usize * ENTRY];
    for i in 0..count as usize {
        let entry = &run[i * ENTRY..(i + 1) * ENTRY];
        let key = key_at(entry, 0);
        let place = u64_at(entry, 32);
        if (place >> 40) & 0xffff != 0 {
            return Err("an entry's place has bits set that must be zero");
        }
        let at = (place & ((1 << 40) - 1)) * 16;
        let kind = (place >> 56) as u8;
        let rlen = u32_at(entry, 40);
        if at < HEADER as u64 || at + u64::from(rlen) > index_at {
            return Err("an entry places its payload outside the payloads");
        }
        if i > 0 && records.last().is_some_and(|it: &Rec| it.key >= key) {
            return Err("its index is not sorted");
        }
        let payload = &bytes[at as usize..at as usize + rlen as usize];
        records.push(Rec { key, at, len: rlen, kind, value: value_digest(kind, payload) });
    }
    Ok(Read { namespace: Namespace(u64_at(bytes, 8)), digest, len: len as u64, index_at, records })
}

/// A segment in the plain store's layout holding `records` (key, kind, payload), sorted here.
/// What a test or a remote worker writes. The plain store's own writer is the store's commit.
pub fn make(namespace: Namespace, records: &[([u8; 32], u8, Vec<u8>)]) -> Vec<u8> {
    let mut sorted: Vec<&([u8; 32], u8, Vec<u8>)> = records.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    sorted.dedup_by(|a, b| a.0 == b.0);
    let mut out = vec![0u8; HEADER];
    let mut entries = Vec::with_capacity(sorted.len() * ENTRY);
    for (key, kind, payload) in &sorted {
        let at = out.len() as u64;
        out.extend_from_slice(payload);
        while out.len() % 16 != 0 {
            out.push(0);
        }
        let crc = blaze_ids::crc32c::extend(blaze_ids::crc32c::extend(blaze_ids::crc32c::crc32c(key), &[*kind]), payload);
        entries.extend_from_slice(key);
        entries.extend_from_slice(&((at / 16) | (u64::from(*kind) << 56)).to_le_bytes());
        entries.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        entries.extend_from_slice(&crc.to_le_bytes());
    }
    let index_at = out.len() as u64;
    let count = sorted.len() as u32;
    out[0..4].copy_from_slice(b"BLPS");
    out[4..8].copy_from_slice(&FORMAT.to_le_bytes());
    out[8..16].copy_from_slice(&namespace.0.to_le_bytes());
    out[16..20].copy_from_slice(&count.to_le_bytes());
    out[24..32].copy_from_slice(&index_at.to_le_bytes());
    if let (Some(first), Some(last)) = (sorted.first(), sorted.last()) {
        out[32..64].copy_from_slice(&first.0);
        out[64..96].copy_from_slice(&last.0);
    }
    out.extend_from_slice(&entries);
    out.extend_from_slice(b"BLPE");
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&index_at.to_le_bytes());
    let digest = ContentId::of(DIGEST_DOMAIN, &out);
    out.extend_from_slice(&digest.0);
    out
}
