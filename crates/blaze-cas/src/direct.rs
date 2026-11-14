//! A record read into the caller's memory: the store's read path with no page pool
//! (docs/v2/plan/design/store-pool.md, "Superseded for the read path (2026-11-13)").
//!
//! The page cache is the one cache. A [`Direct`] owns no copy of any record between calls: a
//! small value is read with a positional read ([`Effect::ReadInto`], `pread`) straight into the
//! caller's buffer, a task's frame, and verified there; a value of [`MAP_FROM`] bytes or more is
//! read in place through a shared read-only mapping ([`Direct::map`]). Measured on the Mac (APFS,
//! 16 KiB pages, a 2 GiB file of 256-byte records, cached, a fresh process per sample): a record
//! at a random offset costs 593 ns by `pread` and 707 ns by a mapping, and 10,000 of them leave
//! 4.8 MB resident and 4.2 MB of footprint by `pread` against 160 MB resident and 2.6 MB of
//! footprint by mapping; opening and mapping a file costs about 15 µs.
//!
//! **Finding a key.** The store's index and deltas, exactly as every other read finds it
//! (`Look::find`): the own tiers first, then the live layer's. A key in a writer's segment is
//! memory already and is copied out under the epoch guard (`Look::value`, which never touches
//! the pool for a segment's record); a key in a pack becomes a want of `RECORD_HEADER + stored`
//! bytes at its offset of the pack's file, checked first to lie inside what the pack commits.
//!
//! **The core.** `plan::Plan` is sans-IO (rule `sans-io-cores`): told what records are wanted,
//! it sorts them by (file, offset), joins those at most `plan::GAP_MOST` apart into runs of at
//! most `plan::RUN_MOST`, lays the runs end to end in one region, hands back one
//! [`Effect::ReadInto`] per part of a run, takes their completions, and says for each record
//! where its bytes are or why they are not. The shell ([`Direct`]) finds keys, registers the
//! region, drives the effects on the caller's driver, verifies, and puts each value where the
//! caller asked. The effects are the ones every driver already serves (uring, kqueue, the
//! simulation): `ReadInto` with `Cache::Kept` and `Priority::Demand`, and [`Effect::Map`].
//!
//! **The verification rule.** Every record is checked whole on every read and every map, with
//! nothing remembered: its 96-byte header and its payload match the record's CRC-32C (format 9,
//! [`RecordHeader::check`]), it names the key asked, it agrees with the index on its form, stored
//! and raw lengths, and its payload decodes to exactly `raw` bytes. The CRC (hardware, some
//! 0.03 ns a byte: about 20 ns of a 256-byte record's 0.6 µs read, 3%) replaces the BLAKE3 of the
//! value (about 1 GB/s a core, some 0.25 µs, 40%) on the read path. BLAKE3 stays where no record's
//! CRC covers the bytes: a format-8 record (its CRC covers its header only, read forward), a value
//! in a file of its own, a format-8 chunked value made whole; and for the scrub and for bytes
//! fetched from elsewhere. A chunked value is its list (a record, checked) and its chunks (each a
//! record, checked), read in a second round of the call by their keys; a file of its own is read
//! whole in that round.
//!
//! **Coalescing.** Two records of one file at most `plan::GAP_MOST` (4 KiB) apart are read as
//! one run: the gap's bytes cost about 0.4 µs of copy at the page cache's ~10 GB/s, less than the
//! 0.6 µs a second read costs, and 4 KiB is one Linux page, so on Linux a run never touches a page
//! that two reads would not. A run stops at `plan::RUN_MOST` (1 MiB, the pool's `READ_MOST`): a
//! read an NVMe drive takes as one command at full speed, and no more of the caller's memory held
//! for gaps than a fraction of that. Against the floor (one read per record, of its bytes and no
//! more): a batch of `n` records in `r` runs costs `r` reads instead of `n`, plus at most
//! `(n - r) * GAP_MOST` bytes of gap copied, each gap cheaper than the read it saves.
//!
//! **Mapping.** [`MAP_FROM`] is 16 KiB, the macOS page: below it, a copy (about 1.6 µs at most, and
//! 16 KiB of footprint until the frame is reset) beats a mapping's fault (about 0.7 µs a page,
//! after a first mapping of the file at about 15 µs); from it on, a mapping costs no footprint at
//! all, which is the order the owner set (time, then memory). These are measured on the Mac only;
//! Linux's 4 KiB pages and an uncached disk are not measured. A pack is mapped once a process (its
//! mapping kept in the file table, made again only when a record lies past its end, as the pack
//! grew), and an external value's file once a read.
//!
//! **The collector.** Only a pack's committed bytes are read or mapped, and the store cuts a pack
//! only back to what it committed, never below (`write_pack`), so no truncate takes bytes a read
//! or a mapping touches. A pack the collector retires is unlinked: a mapping of it stays readable
//! until it is dropped, as an open descriptor does; a read of it by name after the unlink fails,
//! naming the file and the offset, and is never answered from anywhere else. A [`Mapped`] keeps
//! its mapping, and so its bytes, alive until it is dropped.
//!
//! **Files.** A file is entered in the table the first time a call reads it and kept for later
//! calls, at most [`FILES_MOST`] between calls, the least recently used dropped first (one call
//! holds every file it reads). What blaze-core's effects let a reader keep is a file's name and
//! its mapping: the descriptor a `ReadInto` reads through is the driver's (io_uring keeps one per
//! file; kqueue opens the file for each read, see the lane's report), so `files_opened` counts
//! the files entered, what an open-once effect would open.

use std::{
    alloc::Allocator,
    borrow::Cow,
    fmt,
    ops::Deref,
    ptr::NonNull,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use blaze_core::{
    ABANDONED_SPAN, Completion, Done, Driver, Effect, Extent, FileName, IDLE_STALL, Mapping, Spans,
    spans::SpansFull, wait::Deadline,
};
use blaze_format::SchemaHash;
use blaze_graph::wait::{WaitOn, wait};
use blaze_ids::ContentId;

use crate::{
    Key, Reader, Use, cold,
    disk::{self, Form, Loc, NO_DICTIONARY, RECORD_HEADER, RecordHeader, SCHEMA_PREFIX, Sealed, content_of},
    read::{Found, Look, Place, Shared, View, whole_by_crc},
    table::Held,
};

#[cfg(test)]
mod tests;

/// A value of this many bytes or more is mapped ([`Direct::map`]); a shorter one is read into
/// memory. The macOS page (see the module's docs).
pub const MAP_FROM: u64 = 16 << 10;

/// Files kept in a reader's table between calls.
pub const FILES_MOST: usize = 64;

/// The tags a [`Direct`] asks with: `tags .. tags + TAGS` of the base it is given.
pub const TAGS: u64 = plan::TAGS;

/// A bug planted for the oracle's controls: each makes the reader wrong in one way.
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plant {
    None,
    /// The values of a batch are put in the order their records lie on disk, while the places
    /// handed back say the caller's order.
    SortedOrder,
    /// A record is not checked: neither its CRC (format 9's covers the payload) nor a hash of
    /// its value.
    NoCheck,
    /// A run is read one byte short of its end.
    RunEnd,
    /// A chunked value's first chunk is put last.
    ChunkOrder,
    /// A value made whole in the second round moves only the places that start strictly after
    /// its own, so a neighbour at the same position keeps pointing at its bytes.
    StrictlyAfter,
}

/// The sans-IO core: the reads a batch of records needs, and what came of them.
mod plan {
    #![blaze::sans_io]
    #![deny(blaze::sans_io_core)]

    use blaze_core::{
        ABANDONED_SPAN, Cache, Completion, Done, Effect, FileName, IoError, Priority, Span,
        SpanRegion, Tag,
    };

    #[cfg(test)]
    use super::Plant;

    /// Two records of one file at most this far apart are read in one run (the module's docs).
    pub(crate) const GAP_MOST: u64 = 4 << 10;

    /// The most one run spans, and the most one read reads: a record longer than this is a run
    /// of its own, read in parts of it.
    pub(crate) const RUN_MOST: u64 = 1 << 20;

    /// A tag is `tags + (call << PART_BITS) + part`: a completion of an earlier call is told
    /// from this one's by its call.
    const PART_BITS: u32 = 24;

    /// Calls counted before the count wraps.
    const CALLS: u64 = 1 << 16;

    /// Tags a plan uses from its base.
    pub(crate) const TAGS: u64 = CALLS << PART_BITS;

    /// The part number a call's one mapping asks with: never a read's.
    const MAP_PART: u64 = (1 << PART_BITS) - 1;

    /// A record wanted: `len` bytes at `at` of the caller's file `file`.
    #[derive(Clone, Copy, Debug)]
    struct Want {
        file: u32,
        at: u64,
        len: u64,
        /// Its run, once planned.
        run: u32,
        /// Where its first byte is in the region, once planned.
        into: u64,
    }

    /// One file's bytes `at .. at + len`, read into the region at `into`, by the parts
    /// `first .. first + parts`.
    #[derive(Clone, Copy, Debug)]
    struct Run {
        file: u32,
        at: u64,
        len: u64,
        into: u64,
        first: u32,
        parts: u32,
    }

    /// One read: `len` bytes at `at` of its run's file into the region at `into`.
    #[derive(Debug)]
    struct Part {
        run: u32,
        at: u64,
        into: u64,
        len: u32,
        state: State,
    }

    #[derive(Debug)]
    enum State {
        Asked,
        Read(u32),
        Failed(IoError),
    }

    /// Why a record's bytes are not in the region.
    #[derive(Clone, PartialEq, Eq, Debug)]
    pub(crate) enum Missed {
        /// The read at `at` of the file failed.
        Failed { at: u64, why: String },
        /// The read at `at` of the file read `read` of the `asked` bytes, short of the record.
        Short { at: u64, asked: u64, read: u64 },
        /// The read at `at` of the file has not completed.
        Unfinished { at: u64 },
    }

    pub(crate) struct Plan {
        tags: u64,
        call: u64,
        // alloc: the plan's tables, kept and cleared between calls, so a warm call allocates
        // nothing
        wants: Vec<Want>,
        // alloc: as `wants`
        order: Vec<u32>,
        // alloc: as `wants`
        runs: Vec<Run>,
        // alloc: as `wants`
        parts: Vec<Part>,
        left: usize,
        #[cfg(test)]
        pub(crate) plant: Plant,
    }

    impl Plan {
        pub(crate) fn new(tags: u64) -> Plan {
            Plan {
                tags,
                call: 0,
                // alloc: empty, grown by the first call
                wants: Vec::new(),
                // alloc: empty
                order: Vec::new(),
                // alloc: empty
                runs: Vec::new(),
                // alloc: empty
                parts: Vec::new(),
                left: 0,
                #[cfg(test)]
                plant: Plant::None,
            }
        }

        /// A new call: nothing wanted, and the tags of every earlier call no longer its own.
        pub(crate) fn begin(&mut self) {
            self.call = (self.call + 1) % CALLS;
            self.wants.clear();
            self.order.clear();
            self.runs.clear();
            self.parts.clear();
            self.left = 0;
        }

        /// `len` bytes at `at` of file `file` wanted: the want's number.
        pub(crate) fn want(&mut self, file: u32, at: u64, len: u64) -> u32 {
            let n = self.wants.len() as u32;
            self.wants.push(Want { file, at, len, run: 0, into: 0 });
            n
        }

        /// The wants laid out as runs, sorted by (file, offset), near neighbours joined: how
        /// many bytes of region they need.
        pub(crate) fn plan(&mut self) -> u64 {
            self.order.clear();
            self.order.extend(0..self.wants.len() as u32);
            let wants = &self.wants;
            self.order.sort_unstable_by_key(|&n| {
                let want = wants[n as usize];
                (want.file, want.at, n)
            });
            self.runs.clear();
            let mut into = 0;
            for at in 0..self.order.len() {
                let n = self.order[at] as usize;
                let want = self.wants[n];
                let end = want.at + want.len;
                let joins = self.runs.last().is_some_and(|run| {
                    let run_end = run.at + run.len;
                    run.file == want.file
                        && want.at <= run_end + GAP_MOST
                        && end.max(run_end) - run.at <= RUN_MOST
                });
                if joins {
                    let run = self.runs.last_mut().expect("a run to join");
                    run.len = run.len.max(end - run.at);
                } else {
                    if let Some(last) = self.runs.last() {
                        into = last.into + last.len;
                    }
                    let run = Run { file: want.file, at: want.at, len: want.len, into, first: 0, parts: 0 };
                    self.runs.push(run);
                }
                let r = self.runs.len() - 1;
                let run = self.runs[r];
                self.wants[n].run = r as u32;
                self.wants[n].into = run.into + (want.at - run.at);
            }
            let cut = self.cut();
            self.parts.clear();
            for (r, run) in self.runs.iter_mut().enumerate() {
                run.first = self.parts.len() as u32;
                let len = run.len.saturating_sub(cut);
                let mut done = 0;
                // Bounded: each turn reads at least one byte of the run, up to `RUN_MOST`.
                while done < len {
                    let part = (len - done).min(RUN_MOST);
                    self.parts.push(Part {
                        run: r as u32,
                        at: run.at + done,
                        into: run.into + done,
                        len: part as u32,
                        state: State::Asked,
                    });
                    done += part;
                }
                run.parts = self.parts.len() as u32 - run.first;
            }
            assert!(
                (self.parts.len() as u64) < MAP_PART,
                "a call of the store's direct reader reads {} parts, more than its tags name",
                self.parts.len()
            );
            self.runs.last().map_or(0, |run| run.into + run.len)
        }

        /// How much short of its end a run is read: nothing, but for the planted bug.
        fn cut(&self) -> u64 {
            #[cfg(test)]
            if self.plant == Plant::RunEnd {
                return 1;
            }
            0
        }

        /// The reads of every part into `region`, each of the file `name_of` names.
        pub(crate) fn issue(
            &mut self,
            region: SpanRegion,
            name_of: impl Fn(u32) -> FileName,
            effects: &mut Vec<Effect>,
        ) {
            for (n, part) in self.parts.iter().enumerate() {
                let file = self.runs[part.run as usize].file;
                effects.push(Effect::ReadInto {
                    file: name_of(file),
                    at: part.at,
                    into: Span { region, offset: part.into, len: part.len },
                    cache: Cache::Kept,
                    priority: Priority::Demand,
                    tag: self.tag(n as u64),
                });
            }
            self.left = self.parts.len();
        }

        fn tag(&self, part: u64) -> Tag {
            Tag(self.tags + (self.call << PART_BITS) + part)
        }

        /// The tag this call's mapping asks with.
        pub(crate) fn map_tag(&self) -> Tag {
            self.tag(MAP_PART)
        }

        /// Whether `tag` is one of the reader's, of this call or an earlier one.
        pub(crate) fn ours(&self, tag: Tag) -> bool {
            tag.0.checked_sub(self.tags).is_some_and(|it| it < TAGS)
        }

        /// The part of this call `tag` names, still asked.
        fn part_of(&self, tag: Tag) -> Option<usize> {
            let rel = tag.0.checked_sub(self.tags).filter(|it| *it < TAGS)?;
            let n = (rel & MAP_PART) as usize;
            let mine = rel >> PART_BITS == self.call
                && self.parts.get(n).is_some_and(|it| matches!(it.state, State::Asked));
            mine.then_some(n)
        }

        /// Reads asked and not completed.
        pub(crate) fn left(&self) -> usize {
            self.left
        }

        /// A completion taken: `Some` of it back when it is no read of this call's.
        pub(crate) fn complete(&mut self, completion: Completion) -> Option<Completion> {
            let Some(n) = self.part_of(completion.tag) else { return Some(completion) };
            let part = &mut self.parts[n];
            part.state = match completion.done {
                Done::ReadInto { len } => State::Read(len.min(part.len)),
                Done::Failed(why) => State::Failed(why),
                // alloc: the error's message
                other => State::Failed(IoError(format!("the driver answered a read with {other:?}"))),
            };
            self.left -= 1;
            None
        }

        /// Whether a read failed with its span abandoned to the kernel: the region is never to
        /// be used again.
        pub(crate) fn abandoned(&self) -> bool {
            self.parts.iter().any(|it| matches!(&it.state, State::Failed(why) if why.0.contains(ABANDONED_SPAN)))
        }

        /// Where want `n`'s bytes start in the region, or why they are not there.
        pub(crate) fn outcome(&self, n: u32) -> Result<u64, Missed> {
            let want = self.wants[n as usize];
            let run = self.runs[want.run as usize];
            let (from, to) = (want.into, want.into + want.len);
            let parts = run.first as usize..(run.first + run.parts) as usize;
            for part in &self.parts[parts] {
                let (start, end) = (part.into, part.into + u64::from(part.len));
                if end <= from || start >= to {
                    continue;
                }
                match &part.state {
                    State::Asked => return Err(Missed::Unfinished { at: part.at }),
                    // clone: the error's message
                    State::Failed(why) => return Err(Missed::Failed { at: part.at, why: why.0.clone() }),
                    State::Read(read) if start + u64::from(*read) < end.min(to) => {
                        return Err(Missed::Short {
                            at: part.at,
                            asked: u64::from(part.len),
                            read: u64::from(*read),
                        });
                    }
                    State::Read(_) => {}
                }
            }
            Ok(want.into)
        }

        /// Runs of this call.
        pub(crate) fn runs(&self) -> u64 {
            self.runs.len() as u64
        }

        /// Reads of this call.
        pub(crate) fn parts(&self) -> u64 {
            self.parts.len() as u64
        }
    }
}

/// The counters of every direct reader of the process, as [`counts`] reads them; each reader
/// keeps its own too ([`Direct::own_counts`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Counts {
    /// Records read into memory, verified or not.
    pub records: u64,
    /// Bytes read into memory: records and the gaps read with them.
    pub bytes: u64,
    /// Reads asked ([`Effect::ReadInto`]).
    pub reads: u64,
    /// Runs read, each one or more neighbouring records.
    pub runs: u64,
    /// Files entered in a reader's table: what an open-once effect opens.
    pub files_opened: u64,
    /// Files mapped.
    pub maps: u64,
    /// Bytes of values read in place through a mapping.
    pub mapped_bytes: u64,
    /// Bytes of callers' buffers given up to a read that stalled or was abandoned in flight.
    pub leaked_bytes: u64,
}

static RECORDS: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);
static READS: AtomicU64 = AtomicU64::new(0);
static RUNS: AtomicU64 = AtomicU64::new(0);
static FILES_OPENED: AtomicU64 = AtomicU64::new(0);
static MAPS: AtomicU64 = AtomicU64::new(0);
static MAPPED_BYTES: AtomicU64 = AtomicU64::new(0);
static LEAKED_BYTES: AtomicU64 = AtomicU64::new(0);

/// The counters of every direct reader of this process.
pub fn counts() -> Counts {
    let get = |it: &AtomicU64| it.load(Ordering::Relaxed);
    Counts {
        records: get(&RECORDS),
        bytes: get(&BYTES),
        reads: get(&READS),
        runs: get(&RUNS),
        files_opened: get(&FILES_OPENED),
        maps: get(&MAPS),
        mapped_bytes: get(&MAPPED_BYTES),
        leaked_bytes: get(&LEAKED_BYTES),
    }
}

/// `n` added to a reader's own counter and the process's.
fn count(own: &mut u64, all: &AtomicU64, n: u64) {
    *own += n;
    all.fetch_add(n, Ordering::Relaxed);
}

/// Why a record was not read. Each names the key, the file and the offset, and the rule that
/// refused it; none is retried or answered another way.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Failed {
    /// The read of the record's bytes failed.
    Read { key: Key, file: FileName, at: u64, why: String },
    /// The read of the record's bytes ended short of what the pack commits.
    Short { key: Key, file: FileName, at: u64, asked: u64, read: u64 },
    /// The bytes read are not the record the index names, or not the value it recorded.
    Corrupt { key: Key, file: FileName, at: u64, what: String },
    /// The index itself is corrupt where it names the key.
    Index { key: Key, file: FileName, what: String },
    /// The record is kept in a form this reader does not read into memory.
    Form { key: Key, file: Option<FileName>, at: u64, form: &'static str, rule: &'static str },
    /// The value is not one a mapping serves.
    NotMappable { key: Key, file: Option<FileName>, at: u64, why: String },
    /// The driver completed nothing for [`IDLE_STALL`] with the record's read in flight.
    Stalled { key: Key, file: FileName, at: u64, waited: Duration },
    /// The value is kept in chunks, and the read of one of them failed as `chunk` says.
    Chunk { key: Key, chunk: Box<Failed> },
}

impl Failed {
    pub fn key(&self) -> Key {
        match self {
            Failed::Read { key, .. }
            | Failed::Short { key, .. }
            | Failed::Corrupt { key, .. }
            | Failed::Index { key, .. }
            | Failed::Form { key, .. }
            | Failed::NotMappable { key, .. }
            | Failed::Stalled { key, .. }
            | Failed::Chunk { key, .. } => *key,
        }
    }
}

/// Where a record is, for an error: a file, or a writer's segment.
struct In<'a>(&'a Option<FileName>);

impl fmt::Display for In<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(file) => write!(f, "{file}"),
            None => f.write_str("a writer's segment"),
        }
    }
}

impl fmt::Display for Failed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Failed::Read { key, file, at, why } => write!(
                f,
                "reading the record of {:x} at {at} of {file}: {why} (a direct read is one read of \
                 the record's file and no other route)",
                key.0
            ),
            Failed::Short { key, file, at, asked, read } => write!(
                f,
                "reading the record of {:x} at {at} of {file}: {read} of the {asked} bytes asked \
                 were read, inside what the pack commits (a committed record is read whole)",
                key.0
            ),
            Failed::Corrupt { key, file, at, what } => write!(
                f,
                "the record of {:x} at {at} of {file} is corrupt: {what} (every record is checked \
                 whole on every direct read)",
                key.0
            ),
            Failed::Index { key, file, what } => {
                write!(f, "the index {file} is corrupt where it names {:x}: {what}", key.0)
            }
            Failed::Form { key, file, at, form, rule } => write!(
                f,
                "the record of {:x} at {at} of {} is kept {form}: {rule}",
                key.0,
                In(file)
            ),
            Failed::NotMappable { key, file, at, why } => write!(
                f,
                "the value of {:x} at {at} of {} is not mapped: {why} (values of {MAP_FROM} bytes \
                 or more kept raw are mapped; the rest are read)",
                key.0,
                In(file)
            ),
            Failed::Stalled { key, file, at, waited } => write!(
                f,
                "reading the record of {:x} at {at} of {file}: nothing completed for {waited:?} \
                 (blaze_core::IDLE_STALL); the buffer it read into is given up to the kernel",
                key.0
            ),
            Failed::Chunk { key, chunk } => {
                write!(f, "the value of {:x} is kept in chunks, and one is not read: {chunk}", key.0)
            }
        }
    }
}

impl std::error::Error for Failed {}

/// What [`Direct::read`] found.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Read {
    /// The value, verified, is the last `len` bytes of the buffer.
    Found { len: usize },
    /// The store keeps nothing under the key.
    Absent,
    Failed(Failed),
}

/// What [`Direct::read_many`] found of one key.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Placed {
    /// The value, verified, is the buffer's `at .. at + len`.
    Found { at: usize, len: usize },
    Absent,
    Failed(Failed),
}

/// A value read in place through a shared read-only mapping: its bytes stay as they are until
/// it is dropped, which unmaps the file once no other clone of the mapping lives. Never written
/// through.
pub struct Mapped {
    mapping: Mapping,
    start: usize,
    len: usize,
}

impl Deref for Mapped {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.mapping.bytes()[self.start..self.start + self.len]
    }
}

impl fmt::Debug for Mapped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Mapped").field("start", &self.start).field("len", &self.len).finish()
    }
}

/// A file of the reader's table, and its mapping once one was asked for.
struct Open {
    name: FileName,
    used: u64,
    mapping: Option<Mapping>,
}

/// What a key was found to be, before its bytes are read.
enum Where {
    Absent,
    Failed(Failed),
    /// A segment's value, verified and copied to the reader's scratch at `at`.
    Held { at: usize, len: usize },
    Want(Want),
}

/// A record of a pack to read.
struct Want {
    key: Key,
    file: FileName,
    /// Its number in the plan.
    n: u32,
    /// Its number among the round's targets.
    target: u32,
    loc: Loc,
    tier: usize,
    /// Found in the live layer's tiers, not the reader's own.
    live: bool,
}

/// How a call's reads went, as a whole.
enum Fetched {
    /// Every read completed; each record's own outcome says how.
    Read,
    /// Nothing completed for this long with reads in flight: the region was given up.
    Stalled(Duration),
    /// A read was abandoned to the kernel: the region was given up.
    Abandoned,
    /// The region could not be registered.
    Unregistered(SpansFull),
}

/// A reader of a store's files that owns no cache: see the module's docs.
pub struct Direct {
    reader: Reader,
    plan: plan::Plan,
    // alloc: the file table, at most `FILES_MOST` between calls, for the reader's life
    files: Vec<Open>,
    tick: u64,
    // alloc: the call's targets, kept and cleared between calls
    targets: Vec<Where>,
    // alloc: the call's answers, as `targets`
    answers: Vec<Option<Result<(usize, usize), Failed>>>,
    // alloc: the visiting order of a call's targets, as `targets`
    order: Vec<u32>,
    // alloc: segments' values copied out under the guard, kept and cleared between calls
    held: Vec<u8>,
    // alloc: a driver's completions, kept between calls
    out: Vec<Completion>,
    // alloc: effects to submit, kept between calls
    effects: Vec<Effect>,
    // alloc: completions of others' effects the driver handed this reader, kept for them
    others: Vec<Completion>,
    // alloc: `read`'s one answer, kept between calls
    one: Vec<Placed>,
    mapped: Option<Done>,
    mapping: bool,
    // alloc: the call's second-round values (chunked, a file of their own), as `targets`
    follows: Vec<(u32, Follow)>,
    // alloc: a value made whole in the second round, kept and cleared between calls
    whole: Vec<u8>,
    // alloc: a chunked value's list, copied out of the caller's buffer, kept between calls
    list: Vec<u8>,
    // alloc: a chunked value's chunk keys, kept between calls
    chunk_keys: Vec<Key>,
    // alloc: the answers of a chunk round, kept between calls
    chunk_places: Vec<Placed>,
    /// A round of chunks: only a chunk's forms are read.
    chunk_round: bool,
    counts: Counts,
    #[cfg(test)]
    plant: Plant,
}

impl Direct {
    /// A reader of `reader`'s store, asking its driver with the tags `tags .. tags + TAGS`.
    pub fn new(reader: Reader, tags: u64) -> Direct {
        assert!(
            tags.checked_add(TAGS).is_some(),
            "the store's direct reader asks with {TAGS} tags from {tags}, past the last"
        );
        Direct {
            reader,
            plan: plan::Plan::new(tags),
            // alloc: empty, grown by the first call
            files: Vec::new(),
            tick: 0,
            // alloc: empty
            targets: Vec::new(),
            // alloc: empty
            answers: Vec::new(),
            // alloc: empty
            order: Vec::new(),
            // alloc: empty
            held: Vec::new(),
            // alloc: empty
            out: Vec::new(),
            // alloc: empty
            effects: Vec::new(),
            // alloc: empty
            others: Vec::new(),
            // alloc: empty
            one: Vec::new(),
            mapped: None,
            mapping: false,
            // alloc: empty
            follows: Vec::new(),
            // alloc: empty
            whole: Vec::new(),
            // alloc: empty
            list: Vec::new(),
            // alloc: empty
            chunk_keys: Vec::new(),
            // alloc: empty
            chunk_places: Vec::new(),
            chunk_round: false,
            counts: Counts::default(),
            #[cfg(test)]
            plant: Plant::None,
        }
    }

    #[cfg(test)]
    pub(crate) fn plant(&mut self, plant: Plant) {
        self.plant = plant;
        self.plan.plant = plant;
    }

    /// This reader's counters.
    pub fn own_counts(&self) -> Counts {
        self.counts
    }

    /// Completions the driver handed this reader of effects that are not its own: their
    /// owner's to take.
    pub fn others(&mut self) -> Vec<Completion> {
        std::mem::take(&mut self.others)
    }

    /// The value of `key`, read whole, verified and appended to `into`.
    pub fn read<D: Driver, A: Allocator + Clone>(
        &mut self,
        driver: &mut D,
        key: &Key,
        into: &mut Vec<u8, A>,
    ) -> Read {
        let mut one = std::mem::take(&mut self.one);
        one.clear();
        self.read_many(driver, std::slice::from_ref(key), into, &mut one);
        let read = match one.pop() {
            Some(Placed::Found { len, .. }) => Read::Found { len },
            Some(Placed::Absent) => Read::Absent,
            Some(Placed::Failed(failed)) => Read::Failed(failed),
            None => unreachable!("a direct read answers every key it is asked"),
        };
        self.one = one;
        read
    }

    /// The values of `keys`, each read whole and verified, appended to `into`; `places` gets
    /// one answer per key, in the order of `keys`, saying where in `into` its value is. The
    /// records are read in file order, neighbours in one read (the module's docs); what was in
    /// `into` before stays as it was.
    ///
    /// The invariant: each found answer's place is `[at, at + len)`, the places are in the
    /// batch's order and do not overlap.
    pub fn read_many<D: Driver, A: Allocator + Clone, B: Allocator>(
        &mut self,
        driver: &mut D,
        keys: &[Key],
        into: &mut Vec<u8, A>,
        places: &mut Vec<Placed, B>,
    ) {
        self.begin();
        let base = places.len();
        self.read_round(driver, keys, into, places);
        self.follow_up(driver, keys, into, base, places);
        self.end();
    }

    /// One round of reads: `keys` located, their records read and checked, each value (a
    /// chunked value's list, a file of its own's nothing, until `follow_up`) appended to `into`.
    fn read_round<D: Driver, A: Allocator + Clone, B: Allocator>(
        &mut self,
        driver: &mut D,
        keys: &[Key],
        into: &mut Vec<u8, A>,
        places: &mut Vec<Placed, B>,
    ) {
        self.plan.begin();
        self.targets.clear();
        self.held.clear();
        // clone: a refcount, so a look borrows nothing of `self`
        let shared = Arc::clone(&self.reader.shared);
        {
            let guard = blaze_reclaim::enter();
            for key in keys {
                let target = self.locate(&shared, &guard, key);
                self.targets.push(target);
            }
        }
        let total = self.plan.plan();
        let start = into.len();
        let fetched = self.fetch(driver, into, start, total);
        self.assemble(&shared, into, start, &fetched, places);
    }

    /// The call's second round: each chunked value's chunks read by key and put together in
    /// its list's place, and each file of its own read whole into its place; a failure of
    /// either answers its key, and the places after it move with the bytes.
    fn follow_up<D: Driver, A: Allocator + Clone, B: Allocator>(
        &mut self,
        driver: &mut D,
        keys: &[Key],
        into: &mut Vec<u8, A>,
        base: usize,
        places: &mut Vec<Placed, B>,
    ) {
        if self.follows.is_empty() {
            return;
        }
        let mut follows = std::mem::take(&mut self.follows);
        let mut whole = std::mem::take(&mut self.whole);
        for (n, follow) in follows.drain(..) {
            let n = base + n as usize;
            let Placed::Found { at, len } = places[n] else { continue };
            let key = keys[n - base];
            whole.clear();
            let made = match follow {
                Follow::Chunks { file, at: offset, content, hash } => {
                    self.list.clear();
                    self.list.extend_from_slice(&into[at..at + len]);
                    self.chunks_into(driver, key, (file, offset), content, hash, &mut whole)
                }
                Follow::External { file, raw, content } => self.external_into(driver, key, file, raw, content, &mut whole),
            };
            let made_len = if made.is_ok() { whole.len() } else { 0 };
            // The value in the list's place: what follows it moved by the difference, in place.
            let tail = at + len..into.len();
            if made_len > len {
                into.resize(into.len() + (made_len - len), 0);
                into.copy_within(tail, at + made_len);
            } else if made_len < len {
                let end = into.len() - (len - made_len);
                into.copy_within(tail, at + made_len);
                into.truncate(end);
            }
            into[at..at + made_len].copy_from_slice(&whole[..made_len]);
            // The places after this one in the batch's order start at or past its end, and those
            // before it end at or before its start (`read_many`'s invariant), so the batch's
            // order, not the position, says which move: a placeholder of no bytes shares its
            // position with its neighbours. `follows` is in no set order, and the rule needs none.
            let strictly = self.plant_strictly_after();
            let after = if strictly { base } else { n + 1 };
            for place in &mut places[after..] {
                if let Placed::Found { at: there, .. } = place
                    && (!strictly || *there > at)
                {
                    *there = *there - len + made_len;
                }
            }
            places[n] = match made {
                Ok(()) => Placed::Found { at, len: made_len },
                Err(failed) => Placed::Failed(failed),
            };
        }
        self.follows = follows;
        self.whole = whole;
    }

    /// The value of `key`, kept in the chunks `self.list` names, put together in `whole`: each
    /// chunk read by its key in one round, checked as every record is.
    fn chunks_into<D: Driver>(
        &mut self,
        driver: &mut D,
        key: Key,
        (file, at): (FileName, u64),
        content: ContentId,
        hash: bool,
        whole: &mut Vec<u8>,
    ) -> Result<(), Failed> {
        // clone: the error's file name, the list's record's
        let corrupt = |what: String| Failed::Corrupt { key, file: file.clone(), at, what };
        let mut chunk_keys = std::mem::take(&mut self.chunk_keys);
        chunk_keys.clear();
        // The list was checked whole when it was read (`chunk_list`).
        // alloc: the error's message
        let listed = disk::decode_chunks(&self.list).map_err(|what| corrupt(what.to_owned()))?;
        chunk_keys.extend(listed.map(|(chunk, _)| chunk));
        let mut chunk_places = std::mem::take(&mut self.chunk_places);
        chunk_places.clear();
        self.chunk_round = true;
        self.read_round(driver, &chunk_keys, whole, &mut chunk_places);
        self.chunk_round = false;
        let mut cursor = 0;
        let mut made = Ok(());
        let listed = disk::decode_chunks(&self.list).expect("decoded just above");
        for ((chunk, len), place) in listed.zip(&chunk_places) {
            made = match place {
                // In list order, end to end: what `assemble` lays out for a round.
                Placed::Found { at: there, len: read } if *read as u64 == len && *there == cursor => {
                    cursor += *read;
                    Ok(())
                }
                // alloc: the error's message
                Placed::Found { .. } => Err(corrupt(format!("its chunk {:x} is of another length", chunk.0))),
                // alloc: the error's message
                Placed::Absent => Err(corrupt(format!("its chunk {:x} is not in the store", chunk.0))),
                // clone: the chunk's failure, named under its value
                Placed::Failed(failed) => Err(of_chunk(key, failed.clone())),
            };
            if made.is_err() {
                break;
            }
        }
        self.chunk_keys = chunk_keys;
        self.chunk_places = chunk_places;
        made?;
        #[cfg(test)]
        if self.plant == Plant::ChunkOrder
            && let Some(first) = self.list.get(32..40)
        {
            // The planted bug: the first chunk put last.
            let first = u64::from_le_bytes(first.try_into().expect("eight bytes")) as usize;
            if first < whole.len() {
                whole.rotate_left(first);
            }
        }
        if hash {
            let hashed = content_of(whole);
            if hashed != content {
                // alloc: the error's message
                return Err(corrupt(format!("its chunks hash to {hashed:x}, not the {content:x} recorded")));
            }
        }
        Ok(())
    }

    /// The value of `key`, kept in a file of its own, read whole into `whole` and hashed: no
    /// record's CRC covers a file of its own.
    fn external_into<D: Driver>(
        &mut self,
        driver: &mut D,
        key: Key,
        file: FileName,
        raw: u64,
        content: ContentId,
        whole: &mut Vec<u8>,
    ) -> Result<(), Failed> {
        self.plan.begin();
        let slot = self.slot(&file);
        let n = self.plan.want(slot, 0, raw);
        let total = self.plan.plan();
        let fetched = self.fetch(driver, whole, 0, total);
        // clone: the error's file name
        let named = || file.clone();
        match fetched {
            Fetched::Read => {}
            Fetched::Stalled(waited) => return Err(Failed::Stalled { key, file: named(), at: 0, waited }),
            // alloc: the error's message
            Fetched::Abandoned => return Err(Failed::Read { key, file: named(), at: 0, why: ABANDONED_SPAN.into() }),
            // alloc: the error's message
            Fetched::Unregistered(full) => return Err(Failed::Read { key, file: named(), at: 0, why: full.to_string() }),
        }
        let off = self.plan.outcome(n).map_err(|missed| match missed {
            plan::Missed::Failed { at, why } => Failed::Read { key, file: named(), at, why },
            plan::Missed::Short { at, asked, read } => Failed::Short { key, file: named(), at, asked, read },
            plan::Missed::Unfinished { at } => Failed::Stalled { key, file: named(), at, waited: Duration::ZERO },
        })?;
        debug_assert_eq!(off, 0, "one want is the region's start");
        if self.checks() {
            let hashed = content_of(&whole[..]);
            if hashed != content {
                return Err(Failed::Corrupt {
                    key,
                    file: named(),
                    at: 0,
                    // alloc: the error's message
                    what: format!("its file's bytes hash to {hashed:x}, not the {content:x} recorded"),
                });
            }
        }
        count(&mut self.counts.records, &RECORDS, 1);
        Ok(())
    }

    /// The value of `key` read in place through a mapping: `None` when the store keeps nothing
    /// under it. Only a value of [`MAP_FROM`] bytes or more kept raw in a pack, or kept as a
    /// file of its own, is mapped; any other is [`Failed::NotMappable`], to be read.
    pub fn map<D: Driver>(&mut self, driver: &mut D, key: &Key) -> Result<Option<Mapped>, Failed> {
        self.begin();
        let mapped = self.map_inner(driver, key);
        self.end();
        mapped
    }

    fn map_inner<D: Driver>(&mut self, driver: &mut D, key: &Key) -> Result<Option<Mapped>, Failed> {
        // clone: a refcount, so a look borrows nothing of `self`
        let shared = Arc::clone(&self.reader.shared);
        let (loc, file, slot, tier, live) = {
            let guard = blaze_reclaim::enter();
            let Some((look, live, found)) = find(&shared, &guard, key) else { return Ok(None) };
            let found = found.map_err(|corrupt| Failed::Index { key: *key, file: corrupt.pack, what: corrupt.what })?;
            let loc = match found.place {
                Place::Pack(loc) => loc,
                Place::Segment(seg) => {
                    return Err(Failed::NotMappable {
                        key: *key,
                        file: None,
                        at: seg.offset,
                        // alloc: the error's message
                        why: "its record is still in a writer's segment, in memory".into(),
                    });
                }
            };
            let file = committed(look, found, key, loc)?;
            match loc.form {
                Form::Raw if loc.raw >= MAP_FROM => {}
                Form::Raw => {
                    return Err(Failed::NotMappable {
                        key: *key,
                        file: Some(file),
                        at: loc.offset,
                        // alloc: the error's message
                        why: format!("it is {} bytes", loc.raw),
                    });
                }
                Form::External => {}
                form => {
                    return Err(Failed::NotMappable {
                        key: *key,
                        file: Some(file),
                        at: loc.offset,
                        // alloc: the error's message
                        why: format!("it is kept {}, so it is decoded into memory", form_name(form)),
                    });
                }
            }
            look.was_read(found);
            let slot = self.slot(&file);
            (loc, file, slot, found.tier, live)
        };
        let end = loc.offset + RECORD_HEADER + loc.stored;
        let pack = self.mapping_of(driver, key, slot, end)?;
        let record = &pack.bytes()[loc.offset as usize..end as usize];
        let (header, sealed) = checked_header(record, key, loc, self.checks())
            .map_err(|what| Failed::Corrupt { key: *key, file: file.clone(), at: loc.offset, what })?; // clone: the error's file name
        // A raw value in its pack is covered by its format-9 record's CRC, checked just above;
        // a file of its own, or a format-8 record, is hashed below. Taken before `pack` moves.
        let covered = loc.form == Form::Raw && whole_by_crc(&header, &record[RECORD_HEADER as usize..], sealed);
        let (mapping, start) = match loc.form {
            Form::External => {
                let external = {
                    let guard = blaze_reclaim::enter();
                    let view = view_of(&shared, Held::task(&guard), live);
                    // clone: refcounts of the external's name and mapping, so no guard is held
                    // past this block
                    view.and_then(|it| it.tiers.get(tier)?.externals.get(&header.content))
                        .map(|it| (it.file.clone(), it.mapping.clone()))
                };
                match external {
                    Some((_, Some(mapping))) => (mapping, 0),
                    Some((name, None)) => {
                        let slot = self.slot(&name);
                        (self.mapping_of(driver, key, slot, header.raw)?, 0)
                    }
                    None => {
                        return Err(Failed::Corrupt {
                            key: *key,
                            file,
                            at: loc.offset,
                            // alloc: the error's message
                            what: format!("its file {:x} is not in the head", header.content),
                        });
                    }
                }
            }
            _ => (pack, (loc.offset + RECORD_HEADER) as usize),
        };
        let len = header.raw as usize;
        let Some(value) = mapping.bytes().get(start..start + len) else {
            return Err(Failed::Corrupt {
                key: *key,
                file,
                at: loc.offset,
                // alloc: the error's message
                what: format!("its value of {len} bytes is not all mapped"),
            });
        };
        if loc.form == Form::External && mapping.bytes().len() != len {
            return Err(Failed::Corrupt {
                key: *key,
                file,
                at: loc.offset,
                // alloc: the error's message
                what: format!("its file is {} bytes, not the {len} recorded", mapping.bytes().len()),
            });
        }
        if self.checks() && !covered {
            let hashed = content_of(value);
            if hashed != header.content {
                return Err(Failed::Corrupt {
                    key: *key,
                    file,
                    at: loc.offset,
                    // alloc: the error's message
                    what: format!("its bytes hash to {hashed:x}, not the {:x} recorded", header.content),
                });
            }
        }
        count(&mut self.counts.mapped_bytes, &MAPPED_BYTES, len as u64);
        Ok(Some(Mapped { mapping, start, len }))
    }

    /// Where the payload of `key`'s record lies, when it is in a pack: its file and offset.
    #[cfg(test)]
    pub(crate) fn where_of(&self, key: &Key) -> Option<(FileName, u64, Form)> {
        let guard = blaze_reclaim::enter();
        let (look, _, found) = find(&self.reader.shared, &guard, key)?;
        let found = found.ok()?;
        let Place::Pack(loc) = found.place else { return None };
        let pack = look.view().tiers[found.tier].packs.get(loc.pack as usize)?;
        // clone: a test's file name
        Some((pack.file.clone(), loc.offset + RECORD_HEADER, loc.form))
    }

    fn checks(&self) -> bool {
        #[cfg(test)]
        if self.plant == Plant::NoCheck {
            return false;
        }
        true
    }

    fn begin(&mut self) {
        self.tick += 1;
        self.plan.begin();
        self.targets.clear();
        self.answers.clear();
        self.order.clear();
        self.held.clear();
        self.mapped = None;
        self.mapping = false;
        self.follows.clear();
        self.chunk_round = false;
    }

    /// The call's end: the file table back within [`FILES_MOST`], the least recently used
    /// dropped first.
    fn end(&mut self) {
        // Bounded: each turn drops one file.
        while self.files.len() > FILES_MOST {
            let oldest = (0..self.files.len()).min_by_key(|&at| self.files[at].used).expect("a file");
            self.files.swap_remove(oldest);
        }
    }

    /// `name`'s place in the file table, entered there the first time.
    fn slot(&mut self, name: &FileName) -> u32 {
        if let Some(at) = self.files.iter().position(|it| it.name == *name) {
            self.files[at].used = self.tick;
            return at as u32;
        }
        count(&mut self.counts.files_opened, &FILES_OPENED, 1);
        // clone: a refcount
        self.files.push(Open { name: name.clone(), used: self.tick, mapping: None });
        (self.files.len() - 1) as u32
    }

    /// What `key` is: absent, failed, a segment's value copied out, or a record to read.
    fn locate(&mut self, shared: &Shared, guard: &blaze_reclaim::Guard, key: &Key) -> Where {
        let Some((look, live, found)) = find(shared, guard, key) else { return Where::Absent };
        let found = match found {
            Ok(found) => found,
            Err(corrupt) => {
                return Where::Failed(Failed::Index { key: *key, file: corrupt.pack, what: corrupt.what });
            }
        };
        let form = found.place.form();
        if self.chunk_round && !matches!(form, Form::Raw | Form::Lz4 | Form::Zstd) {
            let at = match found.place {
                Place::Pack(loc) => loc.offset,
                Place::Segment(seg) => seg.offset,
            };
            return Where::Failed(Failed::Form { key: *key, file: None, at, form: form_name(form), rule: NOT_A_CHUNK });
        }
        let loc = match found.place {
            Place::Pack(loc) => loc,
            // A chunked value's list is memory already: copied out checked, its chunks read
            // in the call's second round (`follow_up`).
            Place::Segment(seg) if seg.form == Form::Chunked => {
                let listed = look.record_sealed(found, key).map_err(|corrupt| corrupt.what).and_then(|(header, list, sealed)| {
                    chunk_list(&header, list)?;
                    Ok((header, list, sealed))
                });
                return match listed {
                    Ok((header, list, sealed)) => {
                        look.was_read(found);
                        let at = self.held.len();
                        self.held.extend_from_slice(list);
                        // The target this is: `read_round` pushes each as it is located.
                        let n = self.targets.len() as u32;
                        let hash = sealed != Sealed::Whole && self.checks();
                        let file = look.file_of(found);
                        self.follows.push((n, Follow::Chunks { file, at: seg.offset, content: header.content, hash }));
                        Where::Held { at, len: list.len() }
                    }
                    Err(what) => Where::Failed(Failed::Corrupt {
                        key: *key,
                        file: look.file_of(found),
                        at: seg.offset,
                        what,
                    }),
                };
            }
            Place::Segment(seg) => {
                return match look.value(found, key, Use::Read) {
                    Ok((_, bytes)) => {
                        let at = self.held.len();
                        self.held.extend_from_slice(&bytes);
                        Where::Held { at, len: bytes.len() }
                    }
                    Err(corrupt) => Where::Failed(Failed::Corrupt {
                        key: *key,
                        file: corrupt.pack,
                        at: seg.offset,
                        what: corrupt.what,
                    }),
                };
            }
        };
        let file = match committed(look, found, key, loc) {
            Ok(file) => file,
            Err(failed) => return Where::Failed(failed),
        };
        // A dictionary's record is the store's, named by the records compressed with it: no
        // key's value, as `Look::value` refuses it too.
        if loc.form == Form::Dictionary {
            return Where::Failed(Failed::Form {
                key: *key,
                file: Some(file),
                at: loc.offset,
                form: form_name(loc.form),
                rule: "a compression dictionary is the store's, no key's value",
            });
        }
        look.was_read(found);
        let slot = self.slot(&file);
        let n = self.plan.want(slot, loc.offset, RECORD_HEADER + loc.stored);
        Where::Want(Want { key: *key, file, n, target: self.targets.len() as u32, loc, tier: found.tier, live })
    }

    /// The call's reads, into `total` bytes of `into` from `start`.
    fn fetch<D: Driver, A: Allocator + Clone>(
        &mut self,
        driver: &mut D,
        into: &mut Vec<u8, A>,
        start: usize,
        total: u64,
    ) -> Fetched {
        if total == 0 {
            return Fetched::Read;
        }
        let total = usize::try_from(total).expect("a region the caller's memory holds");
        // Zeroed: a short read leaves the rest as it was, and nothing reads it unchecked.
        into.resize(start + total, 0);
        // SAFETY: `start < into.len()`, so the pointer is inside the buffer, and not null.
        let base = NonNull::new(unsafe { into.as_mut_ptr().add(start) }).expect("a buffer is not null");
        // SAFETY: `base .. base + total` is `into`'s, which this call borrows mutably and does
        // not touch until every read of it has completed: the guard is dropped then, or, when a
        // read stalls or is abandoned, forgotten with the buffer, which is given up to the
        // kernel and never freed (below).
        let registered = match unsafe { Spans::register("store direct reads", base, total) } {
            Ok(registered) => registered,
            Err(full) => {
                into.truncate(start);
                return Fetched::Unregistered(full);
            }
        };
        let files = &self.files;
        // clone: a refcount per read
        self.plan.issue(registered.region(), |slot| files[slot as usize].name.clone(), &mut self.effects);
        count(&mut self.counts.runs, &RUNS, self.plan.runs());
        count(&mut self.counts.reads, &READS, self.plan.parts());
        count(&mut self.counts.bytes, &BYTES, total as u64);
        let stalled = self.drive(driver).err();
        if stalled.is_none() && !self.plan.abandoned() {
            drop(registered);
            return Fetched::Read;
        }
        // A read may still write the region: it is never freed or used again.
        std::mem::forget(registered);
        // alloc: the caller's bytes before the call, moved to a buffer of their own
        let mut fresh = Vec::with_capacity_in(start, into.allocator().clone());
        fresh.extend_from_slice(&into[..start]);
        std::mem::forget(std::mem::replace(into, fresh));
        count(&mut self.counts.leaked_bytes, &LEAKED_BYTES, total as u64);
        match stalled {
            Some(waited) => Fetched::Stalled(waited),
            None => Fetched::Abandoned,
        }
    }

    /// Runs the call's effects until its reads and its mapping have completed, or until nothing
    /// has completed for [`IDLE_STALL`]: how long it waited, then.
    fn drive<D: Driver>(&mut self, driver: &mut D) -> Result<(), Duration> {
        let span = tracing::info_span!(
            target: "blaze",
            "store direct reads",
            reads = self.plan.left(),
            map = self.mapping,
        );
        let _in = span.enter();
        driver.submit(&mut self.effects);
        let mut since = driver.now();
        // Bounded: each turn completes one of the call's effects, or waits out part of the
        // stall, which ends it.
        while self.plan.left() > 0 || self.mapping {
            let stall = Deadline::after(since, IDLE_STALL);
            let out = &mut self.out;
            let got = wait(WaitOn::Io("store direct reads: a record's bytes"), None, || driver.poll(out, stall));
            if got > 0 {
                since = driver.now();
            } else if stall.passed(driver.now()) {
                return Err(driver.now().saturating_sub(since));
            }
            for completion in self.out.drain(..) {
                if self.mapping && completion.tag == self.plan.map_tag() {
                    self.mapping = false;
                    self.mapped = Some(completion.done);
                    continue;
                }
                let Some(completion) = self.plan.complete(completion) else { continue };
                // A read of an earlier call that stalled: its region was given up.
                if !self.plan.ours(completion.tag) {
                    self.others.push(completion);
                }
            }
            driver.submit(&mut self.effects);
        }
        Ok(())
    }

    /// The mapping of the file at `slot`, at least `end` bytes long: the table's, or a new one.
    fn mapping_of<D: Driver>(&mut self, driver: &mut D, key: &Key, slot: u32, end: u64) -> Result<Mapping, Failed> {
        let open = &self.files[slot as usize];
        if let Some(mapping) = &open.mapping
            && mapping.bytes().len() as u64 >= end
        {
            return Ok(mapping.clone()); // clone: a refcount
        }
        let file = open.name.clone(); // clone: a refcount
        // clone: a refcount
        self.effects.push(Effect::Map { file: file.clone(), extent: Extent::Whole, tag: self.plan.map_tag() });
        self.mapping = true;
        if let Err(waited) = self.drive(driver) {
            self.mapping = false;
            return Err(Failed::Stalled { key: *key, file, at: 0, waited });
        }
        let mapping = match self.mapped.take() {
            Some(Done::Mapped(mapping)) => mapping,
            Some(Done::Failed(why)) => return Err(Failed::Read { key: *key, file, at: 0, why: why.0 }),
            other => {
                return Err(Failed::Read {
                    key: *key,
                    file,
                    at: 0,
                    // alloc: the error's message
                    why: format!("the driver answered a map with {other:?}"),
                });
            }
        };
        count(&mut self.counts.maps, &MAPS, 1);
        let len = mapping.bytes().len() as u64;
        if len < end {
            return Err(Failed::Short { key: *key, file, at: 0, asked: end, read: len });
        }
        // clone: a refcount, kept in the table
        self.files[slot as usize].mapping = Some(mapping.clone());
        Ok(mapping)
    }

    /// Each target's value checked and appended to `into`, and its answer pushed to `places`
    /// in the caller's order; the region read into is then closed up.
    fn assemble<A: Allocator, B: Allocator>(
        &mut self,
        shared: &Shared,
        into: &mut Vec<u8, A>,
        start: usize,
        fetched: &Fetched,
        places: &mut Vec<Placed, B>,
    ) {
        let region = into.len() - start;
        let guard = blaze_reclaim::enter();
        let held = Held::task(&guard);
        let (own, live) = (view_of(shared, held, false), view_of(shared, held, true));
        self.order.clear();
        self.order.extend(0..self.targets.len() as u32);
        if self.plant_sorted() {
            let targets = &self.targets;
            let plan = &self.plan;
            self.order.sort_by_key(|&n| match &targets[n as usize] {
                Where::Want(want) => plan.outcome(want.n).unwrap_or(u64::MAX),
                _ => u64::MAX,
            });
        }
        self.answers.clear();
        self.answers.resize_with(self.targets.len(), || None);
        for at in 0..self.order.len() {
            let n = self.order[at] as usize;
            let answer = match std::mem::replace(&mut self.targets[n], Where::Absent) {
                Where::Absent => None,
                Where::Failed(failed) => Some(Err(failed)),
                Where::Held { at, len } => {
                    let out = into.len();
                    into.extend_from_slice(&self.held[at..at + len]);
                    Some(Ok((out, len)))
                }
                Where::Want(want) => {
                    let view = if want.live { live } else { own };
                    Some(self.take(view, &want, into, start, fetched))
                }
            };
            self.answers[n] = answer;
        }
        let (mut cursor, sorted) = (start, self.plant_sorted());
        for answer in self.answers.drain(..) {
            places.push(match answer {
                None => Placed::Absent,
                Some(Err(failed)) => Placed::Failed(failed),
                Some(Ok((at, len))) => {
                    // The planted bug's places say the caller's order, its values lie in
                    // the disk's.
                    let at = if sorted { cursor } else { at - region };
                    cursor += len;
                    Placed::Found { at, len }
                }
            });
        }
        if region > 0 {
            into.copy_within(start + region.., start);
            into.truncate(into.len() - region);
        }
    }

    fn plant_sorted(&self) -> bool {
        #[cfg(test)]
        if self.plant == Plant::SortedOrder {
            return true;
        }
        false
    }

    fn plant_strictly_after(&self) -> bool {
        #[cfg(test)]
        if self.plant == Plant::StrictlyAfter {
            return true;
        }
        false
    }

    /// `want`'s record, read into the region, checked and its value appended to `into`: where
    /// the value is, before the region is closed up.
    fn take<A: Allocator>(
        &mut self,
        view: Option<&View>,
        want: &Want,
        into: &mut Vec<u8, A>,
        start: usize,
        fetched: &Fetched,
    ) -> Result<(usize, usize), Failed> {
        let (key, loc) = (want.key, want.loc);
        // clone: the error's file name
        let file = || want.file.clone();
        match fetched {
            Fetched::Read => {}
            Fetched::Stalled(waited) => {
                return Err(Failed::Stalled { key, file: file(), at: loc.offset, waited: *waited });
            }
            Fetched::Abandoned => {
                // alloc: the error's message
                return Err(Failed::Read { key, file: file(), at: loc.offset, why: ABANDONED_SPAN.into() });
            }
            Fetched::Unregistered(full) => {
                // alloc: the error's message
                return Err(Failed::Read { key, file: file(), at: loc.offset, why: full.to_string() });
            }
        }
        let off = self.plan.outcome(want.n).map_err(|missed| match missed {
            plan::Missed::Failed { at, why } => Failed::Read { key, file: file(), at, why },
            plan::Missed::Short { at, asked, read } => Failed::Short { key, file: file(), at, asked, read },
            plan::Missed::Unfinished { at } => Failed::Stalled { key, file: file(), at, waited: Duration::ZERO },
        })?;
        let corrupt = |what: String| Failed::Corrupt { key, file: file(), at: loc.offset, what };
        let len = (RECORD_HEADER + loc.stored) as usize;
        let at = start + off as usize;
        let record = &into[at..at + len];
        let (header, sealed) = checked_header(record, &key, loc, self.checks()).map_err(corrupt)?;
        let Some(view) = view else {
            // alloc: the error's message
            return Err(corrupt("the live layer it was found in is gone".into()));
        };
        let payload = &record[RECORD_HEADER as usize..];
        let follow = match header.form {
            Form::Chunked => {
                chunk_list(&header, payload).map_err(corrupt)?;
                let hash = sealed != Sealed::Whole && self.checks();
                Some(Follow::Chunks { file: file(), at: loc.offset, content: header.content, hash })
            }
            Form::External => {
                let external = view.tiers.get(want.tier).and_then(|it| it.externals.get(&header.content));
                let Some(external) = external else {
                    // alloc: the error's message
                    return Err(corrupt(format!("its file {:x} is not in the head", header.content)));
                };
                // clone: a refcount of the file's name
                Some(Follow::External { file: external.file.clone(), raw: header.raw, content: header.content })
            }
            _ => None,
        };
        if let Some(follow) = follow {
            // The list (a chunked value's) or nothing (a file of its own's) stands in its
            // place until `follow_up` reads the value.
            let out = into.len();
            let len = if header.form == Form::Chunked { payload.len() } else { 0 };
            let from = at + RECORD_HEADER as usize;
            into.extend_from_within(from..from + len);
            self.follows.push((want.target, follow));
            count(&mut self.counts.records, &RECORDS, 1);
            return Ok((out, len));
        }
        let value = decode(view, want.tier, &header, payload).map_err(corrupt)?;
        if value.len() as u64 != header.raw {
            // alloc: the error's message
            return Err(corrupt(format!("it is {} bytes, not the {} recorded", value.len(), header.raw)));
        }
        // A format-9 record's CRC, checked above, covered the bytes the value is decoded from;
        // a format-8 record's value is hashed.
        if self.checks() && !whole_by_crc(&header, payload, sealed) {
            let hashed = content_of(&value);
            if hashed != header.content {
                return Err(corrupt(format!(
                    "its bytes hash to {hashed:x}, not the {:x} recorded",
                    header.content
                )));
            }
        }
        let value = match value {
            Cow::Borrowed(_) => None,
            Cow::Owned(value) => Some(value),
        };
        let out = into.len();
        match value {
            None => {
                let from = at + RECORD_HEADER as usize;
                into.extend_from_within(from..from + header.raw as usize);
            }
            Some(value) => into.extend_from_slice(&value),
        }
        count(&mut self.counts.records, &RECORDS, 1);
        Ok((out, header.raw as usize))
    }
}

/// Why a record named by a chunked value's list is refused when it is no chunk.
const NOT_A_CHUNK: &str = "a chunked value's list names it as a chunk, and a chunk is kept raw, \
                           lz4 or zstd (`segment.rs`, a put of an output)";

/// A chunked value's list of chunks checked against its header: whole entries, each no longer
/// than a chunk, their lengths summing to the value's.
fn chunk_list(header: &RecordHeader, list: &[u8]) -> Result<(), String> {
    // alloc: the error's message
    let chunks = disk::decode_chunks(list).map_err(|what| what.to_owned())?;
    let mut total = 0u64;
    for (chunk, len) in chunks {
        if len > crate::chunk::MAX as u64 {
            // alloc: the error's message
            return Err(format!("its chunk {:x} is {len} bytes, longer than a chunk", chunk.0));
        }
        // alloc: the error's message
        total = total.checked_add(len).ok_or_else(|| "its chunks' lengths overflow".to_owned())?;
    }
    if total != header.raw {
        // alloc: the error's message
        return Err(format!("its chunks are {total} bytes, not the {} recorded", header.raw));
    }
    Ok(())
}

/// A value read in a call's second round: what its first round read stands for.
enum Follow {
    /// A list of chunks (the value's record's payload, checked), each a record of its own,
    /// read by key; `hash`: the whole value is hashed against `content` once made (a format-8
    /// list, whose CRC did not cover it).
    Chunks { file: FileName, at: u64, content: ContentId, hash: bool },
    /// A file of its own, `raw` bytes, read whole and hashed against `content`: no record's
    /// CRC covers it.
    External { file: FileName, raw: u64, content: ContentId },
}

/// `failed`, a read of a chunk of `key`'s value, as a failure of `key`.
fn of_chunk(key: Key, failed: Failed) -> Failed {
    // alloc: the chunk's failure, boxed under its value's
    Failed::Chunk { key, chunk: Box::new(failed) }
}

/// `key` found in `shared`'s own tiers, else in its live layer's: the look it was found by,
/// whether that is the live layer's, and what was found.
fn find<'g>(
    shared: &'g Shared,
    guard: &'g blaze_reclaim::Guard,
    key: &Key,
) -> Option<(Look<'g>, bool, Result<Found<'g>, crate::Corrupt>)> {
    let held = Held::task(guard);
    let own = Look::new(shared, held);
    if let Some(found) = own.find(key) {
        return Some((own, false, found));
    }
    let behind = Look::new(shared.live()?, held);
    behind.find(key).map(|found| (behind, true, found))
}

/// The view of `shared`'s own tiers, or of its live layer's.
fn view_of<'g>(shared: &'g Shared, held: Held<'g>, live: bool) -> Option<&'g View> {
    match live {
        false => Some(Look::new(shared, held).view()),
        true => Some(Look::new(shared.live()?, held).view()),
    }
}

/// The file of the pack `loc` is in, once its record is checked to lie inside what the pack
/// commits.
fn committed(look: Look<'_>, found: Found<'_>, key: &Key, loc: Loc) -> Result<FileName, Failed> {
    let tier = &look.view().tiers[found.tier];
    let Some(pack) = tier.packs.get(loc.pack as usize) else {
        return Err(Failed::Index {
            key: *key,
            file: tier.index_file.clone(), // clone: the error's file name
            // alloc: the error's message
            what: format!("it names pack {} its head does not have", loc.pack),
        });
    };
    let len = RECORD_HEADER + loc.stored;
    if loc.offset.checked_add(len).is_none_or(|end| end > pack.usable) {
        return Err(Failed::Corrupt {
            key: *key,
            file: pack.file.clone(), // clone: the error's file name
            at: loc.offset,
            // alloc: the error's message
            what: format!("its record of {len} bytes runs past the {} bytes the pack commits", pack.usable),
        });
    }
    Ok(pack.file.clone()) // clone: a refcount
}

/// The header of `record` (header and payload), checked: whole (its CRC, over the payload too
/// from format 9), of `key`, and as the index says it is; and what its CRC covered.
/// `check` false is the planted bug's (`Plant::NoCheck`): the header decoded, nothing checked.
fn checked_header(record: &[u8], key: &Key, loc: Loc, check: bool) -> Result<(RecordHeader, Sealed), String> {
    let Some((header, payload)) = record.split_at_checked(RECORD_HEADER as usize) else {
        // alloc: the error's message
        return Err("it is shorter than a record's header".to_owned());
    };
    let checked = match check {
        true => RecordHeader::check(header, payload),
        false => RecordHeader::decode(header).map(|it| (it, Sealed::Header)),
    };
    // alloc: the error's message
    let (header, sealed) = checked.map_err(|what| format!("its record: {what}"))?;
    if header.key != *key {
        // alloc: the error's message
        return Err(format!("it is the record of {:x}, another key", header.key.0));
    }
    if (header.form, header.stored, header.raw) != (loc.form, loc.stored, loc.raw) {
        // alloc: the error's message
        return Err(format!(
            "its header says {} with {} bytes stored of {}, its index {} with {} of {}",
            form_name(header.form),
            header.stored,
            header.raw,
            form_name(loc.form),
            loc.stored,
            loc.raw
        ));
    }
    Ok((header, sealed))
}

/// The value a record's payload holds, decoded as its form says. As `Look::value` decodes
/// (the two merge once the pool's path is gone), with the view of after the read.
fn decode<'p>(view: &View, tier: usize, header: &RecordHeader, payload: &'p [u8]) -> Result<Cow<'p, [u8]>, String> {
    Ok(match header.form {
        Form::Raw => Cow::Borrowed(payload),
        Form::Lz4 => Cow::Owned(cold::unlz4(payload, header.raw)?),
        Form::Zstd if header.dictionary == NO_DICTIONARY => Cow::Owned(cold::decompress_long(payload, header.raw)?),
        Form::Zstd => {
            let dictionary = view
                .tiers
                .get(tier)
                .and_then(|it| it.dictionaries.get(header.dictionary as usize))
                // alloc: the error's message
                .ok_or_else(|| format!("it names dictionary {} the store does not have", header.dictionary))?;
            // alloc: the error's message
            let dictionary = dictionary.1.as_deref().map_err(|why| format!("its dictionary {why}"))?;
            Cow::Owned(cold::decompress(dictionary, payload, header.raw)?)
        }
        Form::Warm | Form::ColdColumns => {
            let Some((schema, encoded)) = payload.split_at_checked(SCHEMA_PREFIX) else {
                return Err("its payload is shorter than the schema it starts with".into());
            };
            let schema = SchemaHash(ContentId(schema.try_into().expect("a schema is 32 bytes")));
            let columns = view.columns.get(&schema).ok_or_else(|| {
                // alloc: the error's message
                format!("it is kept in the columns of schema {:x}, and the store was given no encoding of it", schema.0)
            })?;
            let hot = match header.form {
                Form::Warm => columns.hot_from_warm(encoded),
                _ => columns.hot_from_cold(encoded),
            };
            // alloc: the error's message
            Cow::Owned(hot.map_err(|why| format!("its columns do not decode: {why}"))?)
        }
        // alloc: the error's message
        form => return Err(format!("it is kept {}, which is not read into memory", form_name(form))),
    })
}

fn form_name(form: Form) -> &'static str {
    match form {
        Form::Raw => "raw",
        Form::Zstd => "zstd",
        Form::Chunked => "chunked",
        Form::External => "as a file of its own",
        Form::Dictionary => "as a dictionary",
        Form::Lz4 => "lz4",
        Form::Warm => "in warm columns",
        Form::ColdColumns => "in cold columns",
    }
}
