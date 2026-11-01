//! The table under [`crate::Symbol`] and [`crate::Mapped`]: interned values as byte strings
//! with `u32` IDs that mean the same in every process that opens one image
//! (docs/v2/plan/design/restore.md, "IDs are stable across restarts").
//!
//! An ID is one of three kinds:
//!
//! - `1..=statics`: compiled in (the declared symbols). The section's schema names the list,
//!   so an image is only restored by a blaze with the same one.
//! - `first..base.end` with its live bit set: restored from the image, read in place.
//! - any other below `next`: made by this process, in the overlay: an append-only arena behind
//!   [`Records`]' lock-free index, and a column from ID to record.
//!
//! A lookup probes the mapped index (SwissTable groups of eight tag bytes beside eight IDs,
//! read only), then the overlay's. A new value takes an ID the image's GC freed first, then the
//! next: no ID of a live value is ever given to another, and nothing is renumbered.
//!
//! GC is [`Table::write`]: the image keeps the values whose IDs the owners of the other
//! sections marked [`Live`], each at its own ID; every other ID becomes free, for the values
//! the next process makes. The process that writes keeps everything it has: its handles stay
//! good until it exits.
//!
//! Cost: a value new in this process takes a 16-byte header beside its bytes and an 8-byte
//! column slot; a value in the image takes 12 bytes (its start and order key), one live bit
//! and 5 to 10 bytes of index. A lookup is one hash of the bytes, one probe of the mapped index
//! (a control word, then the bytes compared) and, for a value not in the image, one of the
//! overlay's. Reading a value's bytes is a range check, one live bit and two starts.
//!
//! Safe code: the overlay's records and ID column are [`crate::typed`]'s. The image's bytes are
//! untrusted until [`Table::restore`] has checked them: every column is read through
//! `blaze_image`'s bounds- and alignment-checked views, and [`Table::validate`] checks each
//! entry against the others before any lookup reads one, so a damaged section is a
//! [`RestoreError`] naming the byte that failed, never a panic or a read out of bounds.

use std::fmt;

use blaze_format::SchemaHash;
use blaze_ids::bits::{BitSet, BitSlice};
use blaze_image::{Corrupt, Image, Refused, Section, SectionId, SectionWriter, Stable};

use crate::{
    swiss::{self, Ctrl},
    sync::atomic::{AtomicU32, AtomicUsize, Ordering},
    typed::{Record, Records},
};

/// What every table of one kind shares.
pub(crate) struct Shape {
    /// IDs `1..=statics` are compiled in.
    pub(crate) statics: u32,
    /// The bytes of compiled-in ID `id`.
    pub(crate) static_bytes: fn(u32) -> &'static [u8],
    /// The order key of a value's bytes.
    pub(crate) key: fn(&[u8]) -> u64,
    /// The alignment of a value's bytes, at most [`MAX_ALIGN`].
    pub(crate) align: usize,
    /// Every value's length is a multiple of this, itself a nonzero multiple of `align`.
    pub(crate) size: usize,
    /// Whether every value is UTF-8, checked when an image is restored.
    pub(crate) text: bool,
}

pub(crate) const MAX_ALIGN: usize = 16;

/// Slots in a group of the mapped index: tag bytes 0 to 7 of its control word.
const GROUP: usize = 8;

/// The hash of a value's bytes, in every table, mapped or not: [`blaze_phf::name_hash`],
/// fixed so an image's index means the same to every blaze that opens it. FxHash's
/// multiply-rotate over 8-byte words, the length first, then murmur3's 64-bit finalizer so the
/// tag (the top 7 bits) and the home group (the low ones) are both well mixed. It is
/// `blaze_phf`'s so the compiled-in symbols' perfect hash is keyed by it too, and a caller that
/// hashed a name already (the lexer) hands the hash over ([`Table::intern_hashed`]).
#[inline]
pub(crate) fn value_hash(bytes: &[u8]) -> u64 {
    blaze_phf::name_hash(bytes)
}

blaze_format::record! {
    /// What the image holds of one table, at its section's start: where each column is in the
    /// section, and the counts.
    #[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
    struct Root: 1 {
        /// `count + 1` `u32`s: value `i`'s bytes are `starts[i]..starts[i + 1]` of `bytes`.
        starts: u64,
        /// `count` `u64` order keys.
        keys: u64,
        /// `ceil(count / 64)` `u64`s: bit `i` is set when ID `first + i` holds a value.
        live: u64,
        /// `groups` control words of the index.
        ctrl: u64,
        /// `groups * 8` IDs of the index.
        ids: u64,
        /// `frees` free IDs, ascending.
        free: u64,
        bytes: u64,
        bytes_len: u64,
        first: u32,
        count: u32,
        groups: u32,
        frees: u32,
        statics: u32,
        align: u32,
    }
}

/// What a table's section means beyond [`Root`]: the columns, the index and [`value_hash`].
/// A change to any of them changes this text, and so every table's schema.
pub(crate) const LAYOUT: &str = "intern table 1: Root at 0; starts u32, keys u64, live u64 bits, \
     ctrl u64 groups of 8 tags (0x80 | hash >> 57) probed linearly from hash & mask, ids u32, \
     free u32 ascending, bytes; value_hash multiply-rotate with murmur3 finalizer";

/// [`Root`]'s record schema.
pub(crate) const ROOT: SchemaHash = <Root as blaze_format::Record>::SCHEMA;

/// The table's part in the image this process restored, read in place; empty for a fresh one.
#[derive(Default)]
struct Base {
    /// One past the last ID the image holds.
    end: u32,
    starts: &'static [u32],
    keys: &'static [u64],
    live: BitSlice<'static>,
    ctrl: &'static [u64],
    ids: &'static [u32],
    free: &'static [u32],
    bytes: &'static [u8],
}

impl Base {
    fn is_live(&self, index: usize) -> bool {
        self.live.contains(index)
    }
}

/// The IDs the owners of an image's other sections still name, marked before the image is
/// written: a value whose ID is not marked is not kept.
#[derive(Default, Debug, PartialEq, Eq)]
pub struct Live {
    bits: BitSet<u32>,
}

impl Live {
    pub fn new() -> Live {
        Live::default()
    }

    pub fn mark_id(&mut self, id: u32) {
        self.bits.insert_growing(id);
    }

    pub fn contains(&self, id: u32) -> bool {
        self.bits.contains(id)
    }

    /// How many IDs are marked.
    pub fn count(&self) -> usize {
        self.bits.count()
    }
}

/// Why a table was not restored from an image. The table starts empty, and so must every
/// section whose values name its IDs.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RestoreError {
    Refused(Refused),
    /// A group of the table's section failed its CRC: the whole table is dropped.
    Corrupt(Corrupt),
    /// The section's bytes are not a table of this kind: `what` failed at byte `at` of the
    /// section (the root's for its counts and shape, else the column entry's).
    Invalid { section: SectionId, what: &'static str, at: u64 },
    /// Values were interned in this process before the restore, under IDs the image may hold.
    Started { section: SectionId },
}

impl From<Corrupt> for RestoreError {
    fn from(corrupt: Corrupt) -> RestoreError {
        RestoreError::Corrupt(corrupt)
    }
}

impl From<Refused> for RestoreError {
    fn from(refused: Refused) -> RestoreError {
        RestoreError::Refused(refused)
    }
}

impl fmt::Display for RestoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RestoreError::Refused(refused) => refused.fmt(f),
            RestoreError::Corrupt(corrupt) => corrupt.fmt(f),
            RestoreError::Invalid { section, what, at } => {
                write!(f, "{section} is not a table: {what}, at byte {at} of the section")
            }
            RestoreError::Started { section } => {
                write!(f, "{section} was restored after values were interned: restore it first")
            }
        }
    }
}

impl std::error::Error for RestoreError {}

pub use blaze_image::{Jobs, serial};

/// Groups one job of a restore checks: 1 MiB at 64 KiB groups, about 37 µs of CRC32C, so
/// handing a job out is a few percent of it.
const CHUNK_GROUPS: usize = 16;
/// Entries of a column one job of a restore validates: about 16k values at about 25
/// instructions each, near 40 µs, so a million symbols are 64 jobs, enough to fill a socket.
const CHUNK: usize = 1 << 14;

/// `check` over `0..total` in chunks of `chunk`, run by `jobs`. The error is the first chunk's
/// that failed, so the same bytes give the same error however many workers ran.
fn in_chunks<E: Send>(
    jobs: Jobs<'_>,
    total: usize,
    chunk: usize,
    check: impl Fn(std::ops::Range<usize>) -> Result<(), E> + Sync,
) -> Result<(), E> {
    let n = total.div_ceil(chunk);
    if n <= 1 {
        return check(0..total);
    }
    #[allow(clippy::disallowed_types, reason = "taken only by a chunk that failed, an error path")]
    let failed = parking_lot::Mutex::new(None::<(usize, E)>);
    jobs(n, &|job| {
        if let Err(error) = check(job * chunk..((job + 1) * chunk).min(total)) {
            let mut failed = failed.lock();
            if failed.as_ref().is_none_or(|(at, _)| job < *at) {
                *failed = Some((job, error));
            }
        }
    });
    match failed.into_inner() {
        Some((_, error)) => Err(error),
        None => Ok(()),
    }
}

pub(crate) struct Table {
    shape: Shape,
    /// The first ID not compiled in.
    first: u32,
    base: Base,
    /// The next ID no value has had.
    next: AtomicU32,
    /// How many of `base.free` were taken.
    reused: AtomicUsize,
    /// The values this process made, and their ID column.
    records: Records,
}

impl Table {
    /// An empty table, its compiled-in values in the overlay's index.
    pub(crate) fn new(shape: Shape) -> Table {
        let first = shape.statics + 1;
        let table = Table {
            first,
            base: Base { end: first, ..Base::default() },
            next: AtomicU32::new(first),
            reused: AtomicUsize::new(0),
            records: Records::new(first),
            shape,
        };
        for id in 1..first {
            let bytes = (table.shape.static_bytes)(id);
            // A value compiled in twice is the first.
            table.records.intern(value_hash(bytes), bytes, || (id, (table.shape.key)(bytes)));
        }
        table
    }

    /// The table `section` of `image` holds, read in place: its section checked whole, then
    /// its columns checked against each other (every start in bounds and in order, every live
    /// value UTF-8 for text, every index slot a live ID, every free ID not live).
    pub(crate) fn restore<S: Stable>(
        shape: Shape,
        image: &'static Image<S>,
        section: &Section,
        jobs: Jobs<'_>,
    ) -> Result<Table, RestoreError> {
        let groups = section.groups as usize;
        in_chunks(jobs, groups, CHUNK_GROUPS, |at| image.check(section, at.start as u32..at.end as u32))?;
        let root = *image.value::<Root>(section, 0)?;
        let invalid = |(what, at)| RestoreError::Invalid { section: section.id, what, at };
        if root.statics != shape.statics || root.first != shape.statics + 1 || root.align as usize != shape.align {
            return Err(invalid(("a table of another shape", 0)));
        }
        let end = root.first.checked_add(root.count).filter(|&end| end < u32::MAX);
        let end = end.ok_or(invalid(("more IDs than a u32 holds", 0)))?;
        let count = u64::from(root.count);
        let groups = u64::from(root.groups);
        if !groups.is_power_of_two() {
            return Err(invalid(("an index that is not a power of two of groups", 0)));
        }
        let base = Base {
            end,
            starts: image.slice::<u32>(section, root.starts, count + 1)?,
            keys: image.slice::<u64>(section, root.keys, count)?,
            live: BitSlice::from_words(image.slice::<u64>(section, root.live, count.div_ceil(64))?),
            ctrl: image.slice::<u64>(section, root.ctrl, groups)?,
            ids: image.slice::<u32>(section, root.ids, groups * GROUP as u64)?,
            free: image.slice::<u32>(section, root.free, u64::from(root.frees))?,
            bytes: image.range(section, root.bytes, root.bytes_len)?,
        };
        let table = Table {
            first: root.first,
            base,
            next: AtomicU32::new(end),
            reused: AtomicUsize::new(0),
            records: Records::new(root.first),
            shape,
        };
        table.validate(&root, jobs).map_err(invalid)?;
        Ok(table)
    }

    /// Makes `cell`'s table the one `image`'s section `id` holds. Refused once `cell` has one:
    /// its IDs may be the image's.
    pub(crate) fn restore_into<S: Stable>(
        cell: &std::sync::OnceLock<Table>,
        shape: Shape,
        image: &'static Image<S>,
        id: SectionId,
        schema: SchemaHash,
        jobs: Jobs<'_>,
    ) -> Result<(), RestoreError> {
        let started = RestoreError::Started { section: id };
        if cell.get().is_some() {
            return Err(started);
        }
        let section = image.section(id, schema)?;
        let table = Table::restore(shape, image, &section, jobs)?;
        cell.set(table).map_err(|_| started)
    }

    /// The checks that make every read of the restored columns in bounds, in O(IDs + slots)
    /// with no value hashed, each column split into chunks run by `jobs`. That the index
    /// reaches every live value under its own hash is the section's CRC's to guard: a slot that
    /// does not costs a second ID for that value, never a wrong value. An error is what failed
    /// and the byte of the section it failed at (`root`'s column offsets, bounds-checked by the
    /// views that read them).
    fn validate(&self, root: &Root, jobs: Jobs<'_>) -> Result<(), (&'static str, u64)> {
        let base = &self.base;
        let count = (base.end - self.first) as usize;
        let align = self.shape.align;
        let start_at = |index: usize| root.starts + 4 * index as u64;
        if !(base.bytes.as_ptr() as usize).is_multiple_of(align) {
            return Err(("values misaligned", root.bytes));
        }
        if base.starts[0] != 0 {
            return Err(("starts that do not begin at the values", start_at(0)));
        }
        if base.starts[count] as usize != base.bytes.len() {
            return Err(("starts that do not end with the values", start_at(count)));
        }
        if let Some(past) = base.live.first_in(count..base.live.capacity()) {
            return Err(("live bits past the last ID", root.live + 8 * (past / 64) as u64));
        }
        in_chunks(jobs, count, CHUNK, |indices| {
            for index in indices {
                let (start, end) = (base.starts[index] as usize, base.starts[index + 1] as usize);
                // `end <= len` here, not only through the order of every later start: the UTF-8
                // check below slices the bytes before a later chunk has been checked.
                if end < start || end > base.bytes.len() {
                    return Err(("a value's bytes out of order or past the values", start_at(index + 1)));
                }
                if !start.is_multiple_of(align) || !(end - start).is_multiple_of(self.shape.size) {
                    return Err(("a value's bytes misaligned", start_at(index + 1)));
                }
                match base.is_live(index) {
                    true if self.shape.text && std::str::from_utf8(&base.bytes[start..end]).is_err() => {
                        return Err(("a value that is not UTF-8", root.bytes + start as u64));
                    }
                    true => {}
                    false if end != start => return Err(("bytes for a free ID", start_at(index + 1))),
                    false => {}
                }
            }
            Ok(())
        })?;
        in_chunks(jobs, base.free.len(), CHUNK, |at| {
            for at in at {
                let id = base.free[at];
                let in_range = id >= self.first && id < base.end;
                let ascending = at == 0 || base.free[at - 1] < id;
                if !in_range || !ascending || base.is_live((id - self.first) as usize) {
                    return Err(("a free ID that is live, repeated or out of range", root.free + 4 * at as u64));
                }
            }
            Ok(())
        })?;
        let full = std::sync::atomic::AtomicUsize::new(0);
        in_chunks(jobs, base.ctrl.len(), CHUNK / GROUP, |groups| {
            let mut here = 0;
            for group in groups {
                let word = base.ctrl[group];
                for byte in 0..GROUP {
                    let tag = (word >> (byte * 8)) & 0xff;
                    if tag == 0 {
                        continue;
                    }
                    let slot = group * GROUP + byte;
                    let id = base.ids[slot];
                    let known = (1..self.first).contains(&id)
                        || (id >= self.first && id < base.end && base.is_live((id - self.first) as usize));
                    if tag & 0x80 == 0 || !known {
                        return Err(("an index slot that names no value", root.ids + 4 * slot as u64));
                    }
                    here += 1;
                }
            }
            full.fetch_add(here, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        })?;
        let full = full.into_inner();
        if full * 2 > base.ids.len() || (self.first > 1 && full == 0) {
            return Err(("an index more than half full, or without the compiled-in values", root.ctrl));
        }
        Ok(())
    }

    /// The ID of `bytes`, made if it has none.
    pub(crate) fn intern(&self, bytes: &[u8]) -> u32 {
        self.intern_hashed(value_hash(bytes), bytes)
    }

    /// [`Table::intern`] of `bytes` whose [`value_hash`] is `hash`.
    pub(crate) fn intern_hashed(&self, hash: u64, bytes: &[u8]) -> u32 {
        debug_assert_eq!(hash, value_hash(bytes), "the hash given is not the bytes' value_hash");
        if let Some(id) = self.base_find(hash, bytes) {
            return id;
        }
        self.records.intern(hash, bytes, || (self.take_id(), (self.shape.key)(bytes))).id()
    }

    /// The ID of `bytes`, if it has one.
    pub(crate) fn find(&self, bytes: &[u8]) -> Option<u32> {
        self.find_hashed(value_hash(bytes), bytes)
    }

    /// [`Table::find`] of `bytes` whose [`value_hash`] is `hash`.
    pub(crate) fn find_hashed(&self, hash: u64, bytes: &[u8]) -> Option<u32> {
        debug_assert_eq!(hash, value_hash(bytes), "the hash given is not the bytes' value_hash");
        self.base_find(hash, bytes).or_else(|| self.records.find(hash, bytes).map(Record::id))
    }

    fn take_id(&self) -> u32 {
        let free = self.base.free;
        if self.reused.load(Ordering::Relaxed) < free.len() {
            let at = self.reused.fetch_add(1, Ordering::Relaxed);
            if let Some(&id) = free.get(at) {
                return id;
            }
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        assert!(id < u32::MAX, "more than 2^32 - 2 values interned in one table");
        id
    }

    fn base_find(&self, hash: u64, bytes: &[u8]) -> Option<u32> {
        let ctrl = self.base.ctrl;
        if ctrl.is_empty() {
            return None;
        }
        swiss::find(hash, swiss::tag(hash), ctrl.len(), |at| Ctrl::<GROUP>(ctrl[at]), |at| {
            let id = self.base.ids[at.index()];
            (self.get(id) == Some(bytes)).then_some(id)
        })
    }

    /// The bytes of `id`, `None` when it names no value: never made, or freed by the GC of the
    /// image this process restored and not made again.
    #[inline]
    pub(crate) fn get(&self, id: u32) -> Option<&'static [u8]> {
        if id == 0 {
            return None;
        }
        if id < self.first {
            return Some((self.shape.static_bytes)(id));
        }
        let index = (id - self.first) as usize;
        let base = &self.base;
        if id < base.end && base.is_live(index) {
            return Some(&base.bytes[base.starts[index] as usize..base.starts[index + 1] as usize]);
        }
        self.records.get(id).map(Record::bytes)
    }

    /// [`Table::get`] of an ID a handle holds, which must name a value.
    #[inline]
    pub(crate) fn bytes(&self, id: u32) -> &'static [u8] {
        self.get(id).unwrap_or_else(|| self.lost(id))
    }

    #[cold]
    #[inline(never)]
    fn lost(&self, id: u32) -> ! {
        panic!(
            "interned ID {id} names no value: the image's GC freed it while a value still named \
             it, or it was never made (IDs below {} were)",
            self.next.load(Ordering::Relaxed)
        )
    }

    /// The order key of `id`'s value.
    pub(crate) fn key(&self, id: u32) -> u64 {
        if id < self.first {
            return (self.shape.key)(self.bytes(id));
        }
        let index = (id - self.first) as usize;
        if id < self.base.end && self.base.is_live(index) {
            return self.base.keys[index];
        }
        self.records.get(id).map_or_else(|| self.lost(id), Record::key)
    }

    /// One past the highest ID made so far.
    pub(crate) fn end(&self) -> u32 {
        self.next.load(Ordering::Acquire)
    }

    /// Writes the table into `out`, an empty section: every value whose ID `live` marks, at its
    /// ID, and every other ID free. Values interned while this runs, after it read `next`, are
    /// not in it: nothing written before named them.
    pub(crate) fn write(&self, live: &Live, out: &mut SectionWriter<'_>) {
        let end = self.end();
        let count = (end - self.first) as usize;
        let mut starts = Vec::with_capacity(count + 1);
        let mut keys = Vec::with_capacity(count);
        // alloc: `Table::write`'s columns, once an image written at the daemon's stop
        let mut live_bits = BitSet::<usize>::new(count);
        // alloc: as the starts
        let mut bytes = Vec::new();
        // alloc: as the starts
        let mut free = Vec::new();
        let mut kept = Vec::with_capacity(live.count().min(count));
        starts.push(0u32);
        for index in 0..count {
            let id = self.first + index as u32;
            if live.contains(id) {
                bytes.extend_from_slice(self.bytes(id));
                keys.push(self.key(id));
                live_bits.insert(index);
                kept.push(id);
            } else {
                keys.push(0);
                free.push(id);
            }
            starts.push(u32::try_from(bytes.len()).expect("a table of 4 GiB of values or more"));
        }

        let slots = (self.first as usize - 1 + kept.len()) * 2;
        let groups = slots.div_ceil(GROUP).next_power_of_two().max(1);
        // alloc: `Table::write`'s index, once an image written
        let mut ctrl = vec![0u64; groups];
        // alloc: as the index
        let mut ids = vec![0u32; groups * GROUP];
        let mut put = |id: u32, value: &[u8], dedupe: bool| {
            let hash = value_hash(value);
            let tag = swiss::tag(hash);
            let word = |at: usize| Ctrl::<GROUP>(ctrl[at]);
            if dedupe && swiss::find(hash, tag, groups, word, |at| (self.get(ids[at.index()]) == Some(value)).then_some(())).is_some() {
                return;
            }
            let at = swiss::vacant(hash, groups, |at| Ctrl::<GROUP>(ctrl[at]));
            ctrl[at.group] = at.tagged(tag).0;
            ids[at.index()] = id;
        };
        // A value compiled in twice is the first, as in the overlay.
        for id in 1..self.first {
            put(id, (self.shape.static_bytes)(id), true);
        }
        for &id in &kept {
            put(id, self.bytes(id), false);
        }

        let root = out.value(&Root::default());
        assert_eq!(root, 0, "a table is written into an empty section");
        let at = Root {
            starts: out.slice(&starts),
            keys: out.slice(&keys),
            live: out.slice(live_bits.words()),
            ctrl: out.slice(&ctrl),
            ids: out.slice(&ids),
            free: out.slice(&free),
            bytes: out.align(MAX_ALIGN.max(self.shape.align)),
            bytes_len: bytes.len() as u64,
            first: self.first,
            count: count as u32,
            groups: groups as u32,
            frees: free.len() as u32,
            statics: self.first - 1,
            align: self.shape.align as u32,
        };
        out.bytes(&bytes);
        out.patch_value(root, &at);
    }
}

#[cfg(all(test, not(blaze_check)))]
mod tests;

#[cfg(all(test, blaze_check))]
mod model;
