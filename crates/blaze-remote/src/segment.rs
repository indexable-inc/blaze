//! A plain store segment, as the bucket needs it: a push takes the record list from it (to index
//! it and to digest each value), and a pull checks a fetched segment before handing it on.
//!
//! The vocabulary has one home, blaze-keyed's store (crates/blaze-keyed/src/store.rs): the
//! segment's geometry, [`SegmentRef`] (56 bytes in a bundle or an outcome), [`SegmentLen`] and
//! the [`Statement`](blaze_keyed::store::Statement). The layout has one reader and one writer,
//! the plain store's (crates/blaze-cas/src/plain/format.rs): this module reads a segment only
//! through `check_segment` and the store's entry decoder, and writes one only through
//! `seal_segment` (rule 30). What is this crate's own is the digest of a record's value the
//! index carries ([`value_digest`]).

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::alloc::Global;

use blaze_cas::plain::{self, ENTRY, FOOTER, HEADER, Namespace, Segment};
use blaze_ids::ContentId;
pub use blaze_keyed::store::{SegmentId, SegmentLen, SegmentRef};
use blaze_keyed::{exec::RecordRef, kind::Tag, store::StoreError};

/// One record of a segment: its key, where its payload is, its kind's tag and the digest of
/// (tag, payload) the index carries ([`value_digest`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rec {
    pub key: [u8; 32],
    pub at: u64,
    pub len: u32,
    pub tag: Tag,
    pub value: ContentId,
}

impl Rec {
    /// The record as the core names it.
    pub fn record(&self) -> RecordRef {
        RecordRef { tag: self.tag, key: ContentId(self.key) }
    }
}

/// A whole segment, checked by the store's `check_segment`, and its records.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Read {
    pub namespace: Namespace,
    pub segment: SegmentRef,
    pub records: Vec<Rec>,
}

/// The digest of a record's value as the index carries it: the tag (u16, little-endian), then
/// the payload. Two builders of one key must produce equal ones (rule deterministic-output).
pub fn value_digest(tag: Tag, payload: &[u8]) -> ContentId {
    let mut hasher = blaze_ids::Hasher::new("blaze-remote value 2");
    hasher.update(&tag.0.to_le_bytes()).update(payload);
    hasher.finish()
}

/// Checks a whole segment with the store's own check and reads its records with the store's own
/// entry decoder. Why not, as the store says it.
pub fn read(bytes: &[u8]) -> Result<Read, StoreError> {
    let len = bytes.len() as u64;
    let described = if crate::planted(crate::Plant::SkipSegmentDigest) {
        // The plant: the header and footer only, as a ranged read sees them, its digest unchecked.
        (bytes.len() >= HEADER + FOOTER)
            .then(|| plain::describe(&bytes[..HEADER], &bytes[bytes.len() - FOOTER..], len))
            .flatten()
            .ok_or(StoreError::Other { what: "its header and footer are not a plain segment's" })?
    } else {
        plain::check_segment(bytes)?
    };
    crate::trust::note(crate::trust::Check::Digest, described.digest.0);
    let id = SegmentId(described.digest);
    let segment = SegmentRef::new(id, len, described.index_at, described.count).map_err(|why| StoreError::Wire { what: "a segment's reference", why })?;
    let run = Segment::of(&described, len).run(bytes);
    let mut records = Vec::with_capacity_in(described.count as usize, Global);
    for (i, chunk) in run.chunks(ENTRY).enumerate() {
        let at = described.index_at + (i * ENTRY) as u64;
        let entry = plain::Entry::decode(chunk).map_err(|what| StoreError::Layout { segment: id, what, at })?;
        let payload = entry
            .at
            .checked_add(u64::from(entry.len))
            .and_then(|end| bytes.get(entry.at as usize..end as usize))
            .ok_or(StoreError::Layout { segment: id, what: "an entry places its payload outside the segment", at })?;
        records.push(Rec { key: entry.key.0, at: entry.at, len: entry.len, tag: entry.tag, value: value_digest(entry.tag, payload) });
    }
    Ok(Read { namespace: described.namespace, segment, records })
}

/// A segment of `namespace` holding `records` (each record's kind and key, its payload), written
/// by the store's own writer: what a test or a remote worker hands a push.
pub fn make(namespace: Namespace, records: &[(RecordRef, Vec<u8>)]) -> Result<Vec<u8>, StoreError> {
    let mut refs = Vec::with_capacity_in(records.len(), Global);
    refs.extend(records.iter().map(|(record, payload)| (*record, payload.as_slice())));
    let mut out = Vec::new_in(Global);
    plain::seal_segment(namespace, &refs, &mut out)?;
    Ok(out)
}
