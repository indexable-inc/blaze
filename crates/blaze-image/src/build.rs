//! Writing an image: sections appended in order, each from a group boundary, then the CRC table
//! and the spine. The bytes go to disk as one `blaze_core::Effect::Write` (a temporary file,
//! synced, renamed over the old image), so no reader sees a part of them.

#![forbid(unsafe_code)]

use std::{
    fmt,
    sync::atomic::{AtomicU32, Ordering::Relaxed},
};

use blaze_format::{Record, SchemaHash};
use blaze_ids::crc32c::crc32c;

use crate::{
    Section, SectionId, raw,
    spine::{self, DEFAULT_GROUP_SHIFT, MAX_GROUP_SHIFT, MIN_GROUP_SHIFT, Spine},
};

/// An image being written. Group 0 is held for the spine from the start, so every section is
/// written once, where it stays.
pub struct Builder {
    shift: u32,
    out: Vec<u8>,
    sections: Vec<Section>,
}

/// The image would need a spine larger than its first group: more than 960 sections, or more
/// than about 16 TiB of image at 64 KiB groups.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TooLarge {
    pub spine: usize,
    pub group: usize,
}

impl fmt::Display for TooLarge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "an image whose spine of {} bytes outgrows its {}-byte group", self.spine, self.group)
    }
}

impl std::error::Error for TooLarge {}

impl Default for Builder {
    fn default() -> Builder {
        Builder::new()
    }
}

impl Builder {
    /// An image of 64 KiB groups.
    pub fn new() -> Builder {
        Builder::with_group_shift(DEFAULT_GROUP_SHIFT)
    }

    /// An image of `1 << shift`-byte groups. A smaller group checks less on a first read of
    /// a few bytes; a larger one has fewer CRCs.
    pub fn with_group_shift(shift: u32) -> Builder {
        assert!(
            (MIN_GROUP_SHIFT..=MAX_GROUP_SHIFT).contains(&shift),
            "an image's groups are 2^{MIN_GROUP_SHIFT} to 2^{MAX_GROUP_SHIFT} bytes, not 2^{shift}"
        );
        // alloc: one output buffer an image written, at the daemon's stop
        Builder { shift, out: vec![0; 1 << shift], sections: Vec::new() }
    }

    fn group(&self) -> usize {
        1 << self.shift
    }

    /// Starts section `id`, of `schema`, after the last one: what the writer appends is its
    /// bytes, each offset from the section's start. Panics on a second section of one id.
    pub fn section(&mut self, id: SectionId, schema: SchemaHash) -> SectionWriter<'_> {
        assert!(
            self.sections.iter().all(|it| it.id != id),
            "an image written with two sections of {id}"
        );
        let group = self.group();
        let at = self.out.len().next_multiple_of(group);
        self.out.resize(at, 0);
        let first = (at / group - 1) as u32;
        self.sections.push(Section { id, schema, at: at as u64, len: 0, first, groups: 0 });
        SectionWriter { builder: self, start: at }
    }

    /// Section `id` holding `bytes`.
    pub fn add(&mut self, id: SectionId, schema: SchemaHash, bytes: &[u8]) {
        self.section(id, schema).bytes(bytes);
    }

    /// The image's bytes, its CRCs computed on this thread: for small images and tests.
    pub fn finish(self) -> Result<Vec<u8>, TooLarge> {
        self.finish_on(&crate::serial)
    }

    /// The image's bytes. The groups' CRCs are computed here, once each, spread over `jobs`
    /// (the daemon's pool): one core's CRC32C is about 28 GB/s, a sixteenth of what the machine's
    /// memory streams, so a 4 GiB image is 9 ms of CRCs on a socket where one core takes 150.
    pub fn finish_on(mut self, jobs: crate::Jobs<'_>) -> Result<Vec<u8>, TooLarge> {
        let group = self.group();
        let data_end = self.out.len().next_multiple_of(group);
        self.out.resize(data_end, 0);
        let groups = u32::try_from(data_end / group - 1).map_err(|_| TooLarge { spine: usize::MAX, group })?;
        let tops = spine::tops_for(groups, self.shift);
        let spine_len = spine::spine_len(self.sections.len(), tops);
        if spine_len > group {
            return Err(TooLarge { spine: spine_len, group });
        }
        let crc_at = data_end;
        let crcs = crcs_on(jobs, &self.out[group..data_end], group);
        self.out.reserve(tops as usize * group);
        for crc in crcs {
            self.out.extend_from_slice(&crc.to_le_bytes());
        }
        self.out.resize(crc_at + tops as usize * group, 0);
        let tops: Box<[u32]> =
            // alloc: the CRC table's own few CRCs, once an image written
            self.out[crc_at..].chunks_exact(group).map(crc32c).collect();
        let spine = Spine {
            hash: Default::default(),
            len: self.out.len() as u64,
            shift: self.shift,
            groups,
            crc_at: crc_at as u64,
            sections: self.sections,
            tops,
        };
        spine::encode(&spine, &mut self.out[..group]);
        Ok(self.out)
    }
}

/// Groups one job CRCs: 1 MiB at 64 KiB groups, about 37 µs, so handing a job out is a few
/// percent of it.
const GROUPS_A_JOB: usize = 16;

/// The CRC32C of each `group`-byte group of `bytes`, the groups spread over `jobs`.
fn crcs_on(jobs: crate::Jobs<'_>, bytes: &[u8], group: usize) -> Vec<u32> {
    let count = bytes.len() / group;
    // alloc: one CRC slot a group, once an image written
    let crcs: Vec<AtomicU32> = (0..count).map(|_| AtomicU32::new(0)).collect();
    jobs(count.div_ceil(GROUPS_A_JOB), &|job| {
        for at in job * GROUPS_A_JOB..((job + 1) * GROUPS_A_JOB).min(count) {
            crcs[at].store(crc32c(&bytes[at * group..(at + 1) * group]), Relaxed);
        }
    });
    // `jobs` returns after every job finished, which orders their stores before these loads.
    // alloc: in place over the atomics' buffer (same layout), once an image written
    crcs.into_iter().map(AtomicU32::into_inner).collect()
}

/// The section a [`Builder`] is writing: it ends when this is dropped.
pub struct SectionWriter<'a> {
    builder: &'a mut Builder,
    start: usize,
}

impl SectionWriter<'_> {
    /// How many bytes the section holds so far: where the next one goes.
    pub fn len(&self) -> u64 {
        (self.builder.out.len() - self.start) as u64
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Pads with zeros to a multiple of `align` (a power of two, at most a page) from the
    /// section's start, which is page aligned: where a value of that alignment goes next.
    pub fn align(&mut self, align: usize) -> u64 {
        assert!(align.is_power_of_two() && align <= 1 << MIN_GROUP_SHIFT, "an alignment of {align}");
        let at = self.len().next_multiple_of(align as u64);
        self.builder.out.resize(self.start + at as usize, 0);
        self.finish_len();
        at
    }

    /// Appends `bytes`, returning where they start in the section.
    pub fn bytes(&mut self, bytes: &[u8]) -> u64 {
        let at = self.len();
        self.builder.out.extend_from_slice(bytes);
        self.finish_len();
        at
    }

    /// Appends `len` zero bytes, returning where they start.
    pub fn zeros(&mut self, len: usize) -> u64 {
        let at = self.len();
        self.builder.out.resize(self.builder.out.len() + len, 0);
        self.finish_len();
        at
    }

    /// Appends `values` at their alignment, returning where the first is.
    pub fn slice<T: Record>(&mut self, values: &[T]) -> u64 {
        let at = self.align(align_of::<T>());
        self.builder.out.extend_from_slice(raw::bytes_of(values));
        self.finish_len();
        at
    }

    pub fn value<T: Record>(&mut self, value: &T) -> u64 {
        self.slice(std::slice::from_ref(value))
    }

    /// Writes `bytes` over bytes the section already holds at `at`: a root written first and
    /// filled in once what it names is written.
    pub fn patch(&mut self, at: u64, bytes: &[u8]) {
        let end = at.checked_add(bytes.len() as u64).filter(|&end| end <= self.len());
        let Some(end) = end else {
            panic!("a patch of {} bytes at {at} of a section of {} bytes", bytes.len(), self.len())
        };
        self.builder.out[self.start + at as usize..self.start + end as usize].copy_from_slice(bytes);
    }

    /// [`SectionWriter::patch`] of a record's bytes.
    pub fn patch_value<T: Record>(&mut self, at: u64, value: &T) {
        self.patch(at, raw::bytes_of(std::slice::from_ref(value)));
    }

    fn finish_len(&mut self) {
        let len = self.len();
        let group = self.builder.group() as u64;
        let section = self.builder.sections.last_mut().expect("the section being written");
        section.len = len;
        section.groups = len.div_ceil(group) as u32;
    }
}
