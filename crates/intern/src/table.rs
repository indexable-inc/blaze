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
//! read only), then the overlay's. Each is placed by SipHash-1-3 of the bytes under its own
//! key ([`blaze_hash::SipKey`]), never by a fixed hash: the values are names a source file, a
//! macro or a stored tree picks, and a hash anyone can compute lets them pick thousands with
//! one hash, each insert then probing past all the others under the shard's lock (review
//! finding S5: `blaze_phf::name_hash` inverts step by step). The overlay's key is drawn at
//! random when the table is made and never leaves the process; the mapped index's is in the
//! image's [`Root`], BLAKE3 of the values it keeps, so the image's bytes are a function of its
//! values (AGENTS.md rule 25) and no value can be picked to collide before the key exists. A new value takes an ID the image's GC freed first, then the
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
use blaze_hash::SipKey;
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
    /// Where the key the overlay places values by comes from.
    pub(crate) overlay: Overlay,
}

/// How a table draws the key its overlay places values by.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Overlay {
    /// From the operating system for each table, never shown: a table whose values a source
    /// file, a macro or a stored tree picks, as every table a process interns into is. Under
    /// the model checker (`cfg(blaze_check)`) the fixed key instead: the checker runs a model
    /// many times and needs each run to read the same, and a placement that moved from run to
    /// run would move which shard, group and control word the racing threads touch.
    Secret,
    /// The fixed key: a table only a fuzz target's hooks fill (`crate::fuzz`), where one input
    /// must place its values the same way in every run for its coverage to mean anything.
    /// Chosen by the shape, never by a feature: a fuzz build turns on the `fuzz` feature of
    /// every crate in it, and that must leave every `Secret` table secret.
    #[cfg_attr(
        not(any(all(test, not(blaze_check)), feature = "fuzz")),
        expect(dead_code, reason = "made by the fuzz hooks' shape and the unit tests alone")
    )]
    Fixed,
}

/// [`Overlay::Fixed`]'s key, and [`Overlay::Secret`]'s under the model checker.
const FIXED_OVERLAY: SipKey = SipKey::new(0x6f76_6572_6c61_7930, 0x6f76_6572_6c61_7931);

impl Overlay {
    fn key(self) -> SipKey {
        match self {
            Overlay::Secret if !cfg!(blaze_check) => SipKey::random(),
            Overlay::Secret | Overlay::Fixed => FIXED_OVERLAY,
        }
    }
}

pub(crate) const MAX_ALIGN: usize = 16;

/// Slots in a group of the mapped index: tag bytes 0 to 7 of its control word.
const GROUP: usize = 8;

/// The key of the mapped index of an image keeping `values`, in ID order
/// ([`blaze_ids::index_key`] in this table's domain).
pub(crate) fn index_key<'a>(values: impl IntoIterator<Item = &'a [u8]>) -> SipKey {
    blaze_ids::index_key("blaze intern index key", values)
}

blaze_format::record! {
    /// What the image holds of one table, at its section's start: where each column is in the
    /// section, and the counts.
    #[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
    struct Root: 2 {
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
        /// The index's [`SipKey`]: [`index_key`] of the kept values.
        key0: u64,
        key1: u64,
        first: u32,
        count: u32,
        groups: u32,
        frees: u32,
        statics: u32,
        align: u32,
    }
}

/// What a table's section means beyond [`Root`]: the columns, the index and its hash.
/// A change to any of them changes this text, and so every table's schema.
pub(crate) const LAYOUT: &str = "intern table 2: Root at 0; starts u32, keys u64, live u64 bits, \
     ctrl u64 groups of 8 tags (0x80 | hash >> 57) probed linearly from hash & mask, ids u32, \
     free u32 ascending, bytes; hash SipHash-1-3 of the bytes keyed by Root key0 key1, the \
     first 16 bytes of BLAKE3 'blaze intern index key' over each kept value's u64 LE length \
     and bytes in ID order";

/// [`Root`]'s record schema.
pub(crate) const ROOT: SchemaHash = <Root as blaze_format::Record>::SCHEMA;

/// The table's part in the image this process restored, read in place; empty for a fresh one.
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
    /// What `ctrl` and `ids` are placed by; unused while they are empty.
    key: SipKey,
}

impl Default for Base {
    /// A fresh table's: no ID restored, an empty index.
    fn default() -> Base {
        Base {
            end: 0,
            starts: &[],
            keys: &[],
            live: BitSlice::default(),
            ctrl: &[],
            ids: &[],
            free: &[],
            bytes: &[],
            key: SipKey::new(0, 0),
        }
    }
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
    Invalid {
        section: SectionId,
        what: &'static str,
        at: u64,
    },
    /// Values were interned in this process before the restore, under IDs the image may hold.
    Started {
        section: SectionId,
    },
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
/// Values one job of a restore looks up in the index: about 1k at about 50 ns each, near the
/// 40 µs of the other jobs.
const LOOKUPS: usize = 1 << 10;

/// The shortest run of consecutive full groups (no empty slot) [`Table::validate`] refuses in
/// an index of `groups`, at most half full: a run that a writer placing by a keyed hash makes
/// with odds under 2^-64, so a crafted image costs a lookup that misses at most this many
/// control words, where half full alone allowed `groups / 2`.
///
/// A run of `k` full groups is the end of `m >= k` groups whose `8m` slots hold only values
/// homed in them (the group before them has an empty slot, which no probe passes). At most
/// half full, `m` groups are home to `4m` values on average, so that needs twice the mean,
/// which by Chernoff's bound has odds under `e^(-r m)` with `r = GROUP (ln 2 - 1/2)`, about
/// 1.55: `GROUP (α - 1 - ln α)` at the load α = 1/2. Summed over every `m >= k` (under twice
/// the first term) and every first group, the odds are under `2 groups e^(-r k)`, at most
/// 2^-64 once `r k >= ln 2 (log2 groups + 65)`. For 2^18 groups (a million values) that is
/// 38 groups.
fn refused_run(groups: usize) -> usize {
    const RATE: f64 = GROUP as f64 * (std::f64::consts::LN_2 - 0.5);
    debug_assert!(groups.is_power_of_two());
    let bits = f64::from(groups.trailing_zeros() + 65);
    (bits * std::f64::consts::LN_2 / RATE).ceil() as usize
}

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
    #[expect(clippy::disallowed_types, reason = "taken only by a chunk that failed, an error path")]
    let failed = blaze_core::lock::Lock::new("intern.table.check_failed", None::<(usize, E)>);
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
    /// What `records` places values by: [`Shape::overlay`]'s key, for a process's tables drawn
    /// at random for each and never shown.
    overlay: SipKey,
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
            overlay: shape.overlay.key(),
            shape,
        };
        for id in 1..first {
            let bytes = (table.shape.static_bytes)(id);
            // A value compiled in twice is the first.
            table
                .records
                .intern(table.overlay.hash(bytes), bytes, || (id, (table.shape.key)(bytes)));
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
        in_chunks(jobs, groups, CHUNK_GROUPS, |at| {
            image.check(section, at.start as u32..at.end as u32)
        })?;
        let root = *image.value::<Root>(section, 0)?;
        let invalid = |(what, at)| RestoreError::Invalid { section: section.id, what, at };
        if root.statics != shape.statics
            || root.first != shape.statics + 1
            || root.align as usize != shape.align
        {
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
            live: BitSlice::from_words(image.slice::<u64>(
                section,
                root.live,
                count.div_ceil(64),
            )?),
            ctrl: image.slice::<u64>(section, root.ctrl, groups)?,
            ids: image.slice::<u32>(section, root.ids, groups * GROUP as u64)?,
            free: image.slice::<u32>(section, root.free, u64::from(root.frees))?,
            bytes: image.range(section, root.bytes, root.bytes_len)?,
            key: SipKey::new(root.key0, root.key1),
        };
        let table = Table {
            first: root.first,
            base,
            next: AtomicU32::new(end),
            reused: AtomicUsize::new(0),
            records: Records::new(root.first),
            overlay: shape.overlay.key(),
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

    /// The checks that make every read of the restored columns in bounds and every lookup
    /// short, in O(IDs + slots), each column split into chunks run by `jobs`: no run of full
    /// groups longer than [`refused_run`], and the index placed by its key, checked by finding
    /// the compiled-in values under it (in a table with none, every kept value). A key the
    /// layout was not placed by misses almost all of them, where each miss would give a value a
    /// second ID; that one slot reaches its value is the section's CRC's to guard. An error is
    /// what failed and the byte of the section it failed at (`root`'s column offsets,
    /// bounds-checked by the views that read them, or the key's in the root).
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
                    return Err((
                        "a value's bytes out of order or past the values",
                        start_at(index + 1),
                    ));
                }
                if !start.is_multiple_of(align) || !(end - start).is_multiple_of(self.shape.size) {
                    return Err(("a value's bytes misaligned", start_at(index + 1)));
                }
                match base.is_live(index) {
                    true if self.shape.text
                        && std::str::from_utf8(&base.bytes[start..end]).is_err() =>
                    {
                        return Err(("a value that is not UTF-8", root.bytes + start as u64));
                    }
                    true => {}
                    false if end != start => {
                        return Err(("bytes for a free ID", start_at(index + 1)));
                    }
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
                    return Err((
                        "a free ID that is live, repeated or out of range",
                        root.free + 4 * at as u64,
                    ));
                }
            }
            Ok(())
        })?;
        // std's atomics, not the model checker's: a restore is one thread's, before any race.
        use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
        let full = AtomicUsize::new(0);
        let (groups, chunk) = (base.ctrl.len(), CHUNK / GROUP);
        let refused = refused_run(groups);
        let too_long = |first: usize| {
            (
                "a run of full index groups longer than half full allows",
                root.ctrl + 8 * first as u64,
            )
        };
        // Each chunk's full groups at its start and at its end, for the runs across chunks.
        // alloc: two words a chunk of the index, once a restore
        let edges: Vec<[AtomicUsize; 2]> =
            (0..groups.div_ceil(chunk)).map(|_| Default::default()).collect();
        in_chunks(jobs, groups, chunk, |range| {
            let mut here = 0;
            let (mut run, mut lead) = (0, None);
            for group in range.clone() {
                let word = base.ctrl[group];
                if Ctrl::<GROUP>(word).empty().any() {
                    lead.get_or_insert(run);
                    run = 0;
                } else {
                    run += 1;
                    if run >= refused {
                        return Err(too_long(group + 1 - run));
                    }
                }
                for byte in 0..GROUP {
                    let tag = (word >> (byte * 8)) & 0xff;
                    if tag == 0 {
                        continue;
                    }
                    let slot = group * GROUP + byte;
                    let id = base.ids[slot];
                    let known = (1..self.first).contains(&id)
                        || (id >= self.first
                            && id < base.end
                            && base.is_live((id - self.first) as usize));
                    if tag & 0x80 == 0 || !known {
                        return Err((
                            "an index slot that names no value",
                            root.ids + 4 * slot as u64,
                        ));
                    }
                    here += 1;
                }
            }
            full.fetch_add(here, Relaxed);
            let [first, last] = &edges[range.start / chunk];
            first.store(lead.unwrap_or(run), Relaxed);
            last.store(run, Relaxed);
            Ok(())
        })?;
        let full = full.into_inner();
        if full * 2 > base.ids.len() || (self.first > 1 && full == 0) {
            return Err((
                "an index more than half full, or without the compiled-in values",
                root.ctrl,
            ));
        }
        // The runs across chunks, walked twice round so the one through the last group into the
        // first is whole: at most half full, some group has an empty slot and ends every run.
        let (mut run, mut first) = (0, 0);
        for at in 0..2 * edges.len() {
            let start = at % edges.len() * chunk;
            let len = chunk.min(groups - start);
            let [lead, trail] = edges[at % edges.len()].each_ref().map(|it| it.load(Relaxed));
            if run == 0 {
                first = start;
            }
            run += lead;
            if run >= refused {
                return Err(too_long(first));
            }
            if lead < len {
                (run, first) = (trail, start + len - trail);
            }
        }
        // The index is placed by its key: a key it was not placed by finds almost none of these.
        let misplaced = || {
            ("an index whose key does not find its values", std::mem::offset_of!(Root, key0) as u64)
        };
        if self.first > 1 {
            let statics = (self.first - 1) as usize;
            in_chunks(jobs, statics, LOOKUPS, |ids| {
                let found = ids
                    .into_iter()
                    .all(|at| self.base_find((self.shape.static_bytes)(at as u32 + 1)).is_some());
                found.then_some(()).ok_or_else(misplaced)
            })?;
        } else {
            in_chunks(jobs, count, LOOKUPS, |indices| {
                let found = indices.filter(|&index| base.is_live(index)).all(|index| {
                    self.base_find(
                        &base.bytes[base.starts[index] as usize..base.starts[index + 1] as usize],
                    )
                    .is_some()
                });
                found.then_some(()).ok_or_else(misplaced)
            })?;
        }
        Ok(())
    }

    /// The ID of `bytes`, made if it has none.
    pub(crate) fn intern(&self, bytes: &[u8]) -> u32 {
        if let Some(id) = self.base_find(bytes) {
            return id;
        }
        let hash = self.overlay.hash(bytes);
        self.records.intern(hash, bytes, || (self.take_id(), (self.shape.key)(bytes))).id()
    }

    /// The ID of `bytes`, if it has one.
    pub(crate) fn find(&self, bytes: &[u8]) -> Option<u32> {
        self.base_find(bytes)
            .or_else(|| self.records.find(self.overlay.hash(bytes), bytes).map(Record::id))
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

    fn base_find(&self, bytes: &[u8]) -> Option<u32> {
        let ctrl = self.base.ctrl;
        if ctrl.is_empty() {
            return None;
        }
        let hash = self.base.key.hash(bytes);
        let word = |at: usize| {
            probes::count();
            Ctrl::<GROUP>(ctrl[at])
        };
        swiss::find(hash, swiss::tag(hash), ctrl.len(), word, |at| {
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

        let key = index_key(kept.iter().map(|&id| self.bytes(id)));
        let slots = (self.first as usize - 1 + kept.len()) * 2;
        let groups = slots.div_ceil(GROUP).next_power_of_two().max(1);
        // alloc: `Table::write`'s index, once an image written
        let mut ctrl = vec![0u64; groups];
        // alloc: as the index
        let mut ids = vec![0u32; groups * GROUP];
        let mut put = |id: u32, value: &[u8], dedupe: bool| {
            let hash = key.hash(value);
            let tag = swiss::tag(hash);
            let word = |at: usize| {
                probes::count();
                Ctrl::<GROUP>(ctrl[at])
            };
            if dedupe
                && swiss::find(hash, tag, groups, word, |at| {
                    (self.get(ids[at.index()]) == Some(value)).then_some(())
                })
                .is_some()
            {
                return;
            }
            let at = swiss::vacant(hash, groups, |at| {
                probes::count();
                Ctrl::<GROUP>(ctrl[at])
            });
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
            key0: key.words()[0],
            key1: key.words()[1],
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

/// The control words the tables' probes load on this thread, counted in tests so a test can
/// bound the work of interning values made to collide; nothing in other builds.
pub(crate) mod probes {
    #[cfg(test)]
    thread_local! {
        static LOADED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    }

    #[inline(always)]
    pub(crate) fn count() {
        #[cfg(test)]
        LOADED.with(|it| it.set(it.get() + 1));
    }

    /// The control words loaded since the last call.
    #[cfg(all(test, not(blaze_check)))]
    pub(crate) fn take() -> u64 {
        LOADED.with(|it| it.replace(0))
    }
}

#[cfg(all(test, not(blaze_check)))]
mod tests;

#[cfg(all(test, blaze_check))]
mod model;
