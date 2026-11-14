//! The proof that a segment's bytes are what its generation names, made once and carried by the
//! type (decision 18 of 2026-11-13, "verify once, by construction"): every read of a segment
//! goes through a [`Verified`], and nothing that holds one checks a byte again.
//!
//! A mapped segment enters as an [`Unverified`], whose bytes cannot be read. Two functions
//! turn it into a [`Verified`], and they are the only ones (the fields are private to this
//! module):
//!
//! - [`Verified::recorded`]: its manifest row says this boot holds it whole: this process or
//!   another wrote it in this boot (the page cache holds what a write handed it, and a process
//!   crash loses none of it), or an open of this boot checked it in full and recorded the
//!   boot by a flip, or a fetch of this boot checked its statement and digest before writing
//!   it and adopted it with this boot. No byte is read for it.
//! - [`Verified::check`]: a segment of an earlier boot, checked in full once (its length, its
//!   layout, the BLAKE3 digest of every byte, the digest, namespace and count its row names),
//!   consuming the [`Unverified`]. The open's next flip records the boot, so no later process
//!   of this boot checks it again.
//!
//! The brand: a [`Verified`] holds for the [`Boot`] it was made in, and a process lives in one
//! boot, so the proof cannot outlive what it proves; a new boot is a new process, whose
//! segments of the boot before are [`Unverified`] until their one check. There is no function
//! that checks a [`Verified`]: only [`Verified::scrub`], the caller's explicit ask, evaluates
//! the per-record CRCs, to name a record whose bytes changed on the disk since.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::alloc::Global;

use blaze_core::{FileName, Mapping};
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

    /// Its length on the disk (from the mapping, not a byte of it): what its row names, or the
    /// file is not the one named.
    pub(crate) fn len(&self) -> u64 {
        self.map.bytes().len() as u64
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
    pub(crate) fn scrub(&self) -> Vec<Corrupt> {
        let run = self.run();
        // A scrub's findings, usually none: the global heap, as they outlive the scrub.
        let mut out = Vec::new_in(Global);
        for i in 0..self.seg.count as usize {
            let entry = match Entry::decode(&run[i * ENTRY..(i + 1) * ENTRY]) {
                Ok(entry) => entry,
                Err(what) => {
                    out.push(Corrupt { key: Key(blaze_ids::ContentId(run[i * ENTRY..i * ENTRY + 32].try_into().expect("32 bytes"))), pack: self.file.clone(), what: what.to_owned() });
                    continue;
                }
            };
            let end = entry.at + u64::from(entry.len);
            if end > self.seg.index_at || record_crc(&entry.key, entry.tag, self.payload(&entry)) != entry.crc {
                out.push(Corrupt {
                    key: Key(entry.key),
                    pack: self.file.clone(),
                    what: format!("its {} bytes at {} fail their CRC-32C {:08x}: changed on the disk after the segment was proved", entry.len, entry.at, entry.crc),
                });
            }
        }
        out
    }
}
