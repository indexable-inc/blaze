//! The proof that a segment's bytes are what its generation names, made once and carried by the
//! type (decision 18 of 2026-11-13, "verify once, by construction"): every read of a segment
//! goes through a [`Verified`], and nothing that holds one checks a byte again.
//!
//! A mapped segment enters as an [`Unverified`], whose bytes cannot be read. Three functions
//! turn it into a [`Verified`], and they are the only ones (the fields are private to this
//! module):
//!
//! - [`Verified::recorded`]: its manifest row says this boot holds it whole: this process or
//!   another wrote it in this boot (the page cache holds what a write handed it, and a process
//!   crash loses none of it), or an open of this boot checked it in full and recorded the
//!   boot by a flip, or a fetch of this boot checked its statement and digest before writing
//!   it and adopted it with this boot. No byte is read for it.
//! - [`Verified::sealed`]: a segment of an earlier boot its row names sealed: a barrier
//!   covered its file before the flip that said so (the exit tail, store-next.md), so the
//!   generation that names it is proof its bytes reached the media first. No byte is read.
//! - [`Verified::check`]: a segment of an earlier boot not sealed, checked in full once (its length, its
//!   layout, the BLAKE3 digest of every byte, the digest, namespace and count its row names),
//!   consuming the [`Unverified`]. The open's next flip records the boot, so no later process
//!   of this boot checks it again.
//!
//! The brand: a [`Verified`] holds for the [`Boot`] it was made in, and a process lives in one
//! boot, so the proof cannot outlive what it proves; a new boot is a new process, whose
//! segments of the boot before are [`Unverified`] until their one check. There is no function
//! that checks a [`Verified`]: only [`Verified::scrub_into`], the caller's explicit ask, evaluates
//! the per-record CRCs, to name a record whose bytes changed on the disk since.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::alloc::Allocator;

use blaze_core::{FileName, Mapping};
use blaze_own::Share;
use blaze_keyed::store::{ENTRY, Generation, SegmentId, SegmentProof, StoreError};

use super::format::{self, Entry, Segment, record_crc};
use crate::{Corrupt, Key};

/// What a read returns as its segment's proof ([`blaze_keyed::store::SegmentProof`]): the
/// segment and the generation the reading process mapped, made only from a [`Verified`]
/// ([`Verified::proof`]). A `Verified` itself cannot be `Copy` (it holds the mapping), so the
/// trait's proof is this brand of it: two words, no pointer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Proof {
    segment: SegmentId,
    generation: Generation,
}

impl Proof {
    /// The proof of a record this process put and has not committed: its segment is this
    /// process's own, not yet written (the zero id names none), trusted as written.
    pub(crate) fn own(generation: Generation) -> Proof {
        Proof { segment: SegmentId(blaze_ids::ContentId([0; 32])), generation }
    }

    /// Whether this is [`Proof::own`]'s: a record this process put and has not committed.
    pub(crate) fn is_own(&self) -> bool {
        self.segment.0.0 == [0; 32]
    }
}

impl SegmentProof for Proof {
    fn segment(&self) -> SegmentId {
        self.segment
    }

    fn generation(&self) -> Generation {
        self.generation
    }
}

/// The boot this process runs in (a hash of the kernel's boot id): what a proof holds for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Boot(pub(crate) u64);

/// A segment mapped and not yet proved: its bytes are not readable.
pub(crate) struct Unverified {
    seg: Segment,
    file: FileName,
    map: Mapping,
}

impl Unverified {
    pub(crate) fn new(seg: Segment, file: FileName, map: Mapping) -> Unverified {
        Unverified { seg, file, map }
    }

    pub(crate) fn seg(&self) -> &Segment {
        &self.seg
    }

    pub(crate) fn into_file(self) -> FileName {
        self.file
    }

    /// Its length on the disk (from the mapping, not a byte of it): what its row names, or the
    /// file is not the one named.
    pub(crate) fn len(&self) -> u64 {
        self.map.bytes().len() as u64
    }

    /// Whether its index, NOT YET PROVED, names `key`: read only to decide whether the one
    /// full check is due now (its first hit, probe.rs); nothing read here is returned or
    /// trusted. Bounded on any bytes: the index's place is its row's (a manifest's, checked),
    /// the length was compared with the row, and the search stays inside the run.
    pub(crate) fn names(&self, key: &Key) -> bool {
        let bytes = self.map.bytes();
        if bytes.len() as u64 != self.seg.file_len() {
            return false;
        }
        format::find(self.seg.run(bytes), &key.0).is_some()
    }
}

/// A segment whose bytes are proved, for its [`Boot`]: read in place with no check.
pub(crate) struct Verified {
    seg: Segment,
    file: FileName,
    map: Mapping,
    boot: Boot,
}

/// How many full checks one process made: one per segment of an earlier boot at most.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Checks {
    /// Segments checked in full (length, layout, digest of every byte).
    pub full: u64,
    /// Records read through a proof.
    pub reads: u64,
    /// Checks a read made: none exists in the code (a read takes a `Verified`, and nothing
    /// that holds one checks), so this stays zero; counted so the oracle states it.
    pub per_read: u64,
}

impl Verified {
    /// The manifest row says `boot` holds it whole (see the module's docs); its length is
    /// still compared with the row, a number already in hand. `Err` hands it back, to be
    /// [`Verified::check`]ed or dropped.
    pub(crate) fn recorded(it: Unverified, boot: Boot) -> Result<Verified, Unverified> {
        if it.seg.boot != boot.0 || it.len() != it.seg.file_len() {
            return Err(it);
        }
        Ok(Verified { seg: it.seg, file: it.file, map: it.map, boot })
    }

    /// A segment of an earlier boot whose row says it is sealed ([`Segment::sealed`]): a
    /// barrier ordered its bytes before the flip that set the flag, so a generation that names
    /// it sealed was written after them, and a disk that kept the generation kept them (or an
    /// open of a later boot read them whole from the disk and recorded so). No byte is read;
    /// its length is still compared with the row. `Err` hands it back, to be checked.
    pub(crate) fn sealed(it: Unverified, boot: Boot) -> Result<Verified, Unverified> {
        if !it.seg.sealed || !it.seg.local || it.len() != it.seg.file_len() {
            return Err(it);
        }
        Ok(Verified { seg: it.seg, file: it.file, map: it.map, boot })
    }

    /// The one full check of a segment of an earlier boot: its length, layout and the digest of
    /// every byte, and that it is the segment its row names. Consumes it: a proved segment is
    /// never checked again. `Err` names what failed, with the segment and file.
    pub(crate) fn check(it: Unverified, boot: Boot, checks: &mut Checks) -> Result<Verified, (Segment, FileName, StoreError)> {
        let len = it.len();
        let segment = SegmentId(it.seg.digest);
        if len != it.seg.file_len() {
            return Err((it.seg, it.file, StoreError::Layout { segment, what: "its file's length is not the one its generation names", at: len }));
        }
        checks.full += 1;
        let checked = format::check_segment(it.map.bytes()).and_then(|described| {
            (described.digest == it.seg.digest && described.namespace == it.seg.namespace && described.count == it.seg.count)
                .then_some(())
                .ok_or(StoreError::Layout { segment, what: "it is a whole segment, but not the one its generation names", at: 0 })
        });
        match checked {
            Ok(()) => Ok(Verified { seg: it.seg, file: it.file, map: it.map, boot }),
            Err(why) => Err((it.seg, it.file, why)),
        }
    }

    /// Its proof as a read returns it: this segment, branded with the generation the reading
    /// process mapped.
    pub(crate) fn proof(&self, generation: Generation) -> Proof {
        Proof { segment: SegmentId(self.seg.digest), generation }
    }

    /// The mapping, for a reader that keeps it alive beside the proof (the store handle's
    /// table, never unmapped while the process runs).
    pub(crate) fn mapping(&self) -> &Mapping {
        &self.map
    }

    /// The planted bug of `super::tests` (`Plant::TrustBoot`): a segment of an earlier boot
    /// taken as proved without its check. Only tests can make one this way.
    #[cfg(test)]
    pub(crate) fn unchecked_for_a_plant(it: Unverified, boot: Boot) -> Verified {
        Verified { seg: it.seg, file: it.file, map: it.map, boot }
    }

    pub(crate) fn seg(&self) -> &Segment {
        &self.seg
    }

    pub(crate) fn file(&self) -> &FileName {
        &self.file
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn boot(&self) -> Boot {
        self.boot
    }

    /// Every byte, proved.
    pub(crate) fn bytes(&self) -> &[u8] {
        self.map.bytes()
    }

    /// The sorted index, proved.
    pub(crate) fn run(&self) -> &[u8] {
        self.seg.run(self.map.bytes())
    }

    /// The payload `entry` (of this segment's index) places: no check, the segment is proved.
    pub(crate) fn payload(&self, entry: &Entry) -> &[u8] {
        &self.map.bytes()[entry.at as usize..(entry.at + u64::from(entry.len)) as usize]
    }

    /// The caller's explicit ask, never a read's: every record's CRC-32C evaluated against its
    /// bytes as they lie now, naming each that changed on the disk after its proof was made.
    /// Findings go to `out`, the caller's list in its allocator.
    pub(crate) fn scrub_into<B: Allocator>(&self, out: &mut Vec<Corrupt, B>) {
        self.scrub_from(0, u64::MAX, out);
    }

    /// [`Verified::scrub_into`] from entry `from`, stopping after the entry that takes the payload
    /// bytes walked to `budget` or more (at least one entry): the background pass's slice
    /// (store-next.md). Findings go to `out`; returns the next entry and the bytes walked.
    pub(crate) fn scrub_from<B: Allocator>(&self, from: usize, budget: u64, out: &mut Vec<Corrupt, B>) -> (usize, u64) {
        let run = self.run();
        let mut walked = 0u64;
        let mut i = from;
        while i < self.seg.count as usize && (i == from || walked < budget) {
            self.scrub_entry(run, i, out);
            walked += u64::from(u32::from_le_bytes(run[i * ENTRY + 40..i * ENTRY + 44].try_into().expect("4 bytes"))) + ENTRY as u64;
            i += 1;
        }
        (i, walked)
    }

    /// One entry of the index checked against its payload's CRC-32C, a finding to `out` when
    /// it fails.
    fn scrub_entry<B: Allocator>(&self, run: &[u8], i: usize, out: &mut Vec<Corrupt, B>) {
        let entry = match Entry::decode(&run[i * ENTRY..(i + 1) * ENTRY]) {
            Ok(entry) => entry,
            Err(what) => {
                out.push(Corrupt { key: Key(blaze_ids::ContentId(run[i * ENTRY..i * ENTRY + 32].try_into().expect("32 bytes"))), pack: self.file.share(), what: what.to_owned() });
                return;
            }
        };
        let end = entry.at + u64::from(entry.len);
        if end > self.seg.index_at || record_crc(&entry.key, entry.tag, self.payload(&entry)) != entry.crc {
            out.push(Corrupt {
                key: Key(entry.key),
                pack: self.file.share(),
                what: format!("its {} bytes at {} fail their CRC-32C {:08x}: changed on the disk after the segment was proved", entry.len, entry.at, entry.crc),
            });
        }
    }
}
