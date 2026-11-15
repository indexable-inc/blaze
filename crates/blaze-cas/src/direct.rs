//! A record read into the caller's memory: the store's read path with no page pool
//! (docs/v2/plan/design/store-pool.md, "Superseded for the read path (2026-11-13)").
//!
//! The page cache is the one cache. A [`Direct`] owns no copy of any record between calls: a
//! small value the system holds is taken from its pack's mapping, and one it does not is read
//! with a positional read ([`Effect::ReadInto`], `pread`) with the bytes around it into the
//! reader's scratch; either is verified and copied to the caller's buffer, a task's frame
//! ("Records beside each other", below). A value of [`MAP_FROM`] bytes or more is
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
//! region (its own scratch, so a window's bytes never take the caller's memory), drives the
//! effects on the caller's driver, verifies, and copies each value where the caller asked. The effects are the ones every driver already serves (uring, kqueue, the
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
//! **Records beside each other, asked one by one.** The front end asks a record at a time
//! (`hir_def::nameres::frozen::entries::Entries::get`, one key a call), and one build put a
//! file's records side by side (`serve::unit::deps::Asked::take` sorts them by file, and the
//! commit writes a pack through the page cache, `Cache::Kept`). So a record of a pack is not
//! read alone. A pack is mapped before any record of it is read: a mapping reads nothing, and
//! only through it can the system be asked which pages it holds ([`Residency`]: `mincore` of
//! the record's pages in the mapping; the simulation's model of a cache). So a record whose
//! pages the system holds is taken from the mapping, checked as a read one is, and copied to
//! the caller, on the pack's first touch as on any other: no read, no ring, no wait. One the
//! system does not hold is read with its window, the blocks of [`AROUND`] bytes it lies in
//! (from a multiple of it, never past what the pack commits) less those at its ends the system
//! holds ([`Direct::window`]; its bytes there are taken from the mapping), through the page
//! cache. Against the floor (each byte of the records asked read from the device once, and no
//! wait for a record in memory): `n` records put together by one build and asked one by one,
//! in any order, cost one read and one wait per block the system did not hold, each block read
//! once (the tests' bound), plus one round of maps; a record held costs one `mincore` (one
//! call, no I/O), its check and a copy of its bytes. A block's other bytes are read for records
//! that may never be asked: at most [`AROUND`] a read, the cost of the choice. A page let go
//! between `mincore` and the copy is read by the fault, in the copy: a synchronous read of that
//! page, the one wait no span names (rare; measured by none).
//!
//! **Maps, once a store.** The mappings of a store's packs are shared by every reader of it
//! ([`Maps`], kept by the store), so a pack is mapped once a process, not once a thread. The
//! packs are known before any record is (the head lists them), so the first call that finds a
//! pack unmapped maps, in the same round, every pack of the store's views no reader mapped yet
//! (up to [`MAP_AHEAD_MOST`], [`MAPS_A_ROUND`] a wait): after a restart the first ask pays one
//! round of concurrent maps and no later ask of any thread waits on one. A call's records are
//! then asked of the system, and those it does not hold are read in one round of concurrent
//! reads: a batch of `n` records in `n` unmapped packs costs one round of maps and one of
//! reads, two waits, where asking them one at a time cost `2n`.
//!
//! **Mapping.** [`MAP_FROM`] is 16 KiB, the macOS page: below it, a copy (about 1.6 µs at most, and
//! 16 KiB of footprint until the frame is reset) beats a mapping's fault (about 0.7 µs a page,
//! after a first mapping of the file at about 15 µs); from it on, a mapping costs no footprint at
//! all, which is the order the owner set (time, then memory). These are measured on the Mac only;
//! Linux's 4 KiB pages and an uncached disk are not measured. A pack is mapped once a process (its
//! mapping kept in the store's [`Maps`] and lent to each reader's file table, made again only
//! when a record lies past its end, as the pack grew), and an external value's file once a
//! read. A map is an effect on the caller's driver like a read (an open on the ring, then the
//! `mmap` at its completion, on io_uring; a hop to the driver's file threads on kqueue), so it
//! is a wait, named by its span `store direct maps` (its files, the first's name, the bytes
//! mapped); the `mmap` itself reads no page.
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
    alloc::{Allocator, Global},
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

/// A record of a pack that is not in memory is read with the bytes around it, the blocks of
/// this many bytes (from a multiple of it, clipped to what the pack commits) it lies in, into
/// the system's page cache: the records one build put beside it (a file's entries are put
/// together, `serve::unit::deps::Asked::take`) are then asked with no read. Linux's default
/// read-ahead (`read_ahead_kb`, 128); a choice, not a measurement: the cold restart's count of
/// waits and the bytes each read decide it (the module's docs, "Against the floor").
pub const AROUND: u64 = 128 << 10;

/// A record whose bytes span more pages than this is not asked of the page cache: it is read.
const RESIDENT_PAGES: usize = 64;

/// The scratch a reader reads a call's runs into is let go past this, between calls.
const REGION_KEPT: usize = 4 << 20;

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
    /// Each record asked is a read of its own: no window around it, and no record served from
    /// the pages the system holds.
    OneByOne,
    /// A pack's first touch reads its record's window before the page cache is asked: the
    /// order before 2026-11-14, when residency was asked only of a mapping a read had made.
    ReadBeforeMap,
    /// Each file a round maps is mapped in a wait of its own: the packs of a batch one after
    /// another.
    MapEach,
    /// A pack's mapping is taken from the shared table under another pack's name.
    OtherPacksMapping,
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

    /// The part number a call's first mapping asks with, its `i`th `MAP_PART - i`: counted
    /// down from the top while reads count up from 0, and never a read's (`plan` asserts the
    /// two do not meet).
    const MAP_PART: u64 = (1 << PART_BITS) - 1;

    /// A record wanted: `len` bytes at `at` of the caller's file `file`, read as part of the
    /// bytes `from .. to` around it (its window, `super::AROUND`; the record alone when
    /// `from == at` and `to == at + len`).
    #[derive(Clone, Copy, Debug)]
    struct Want {
        file: u32,
        at: u64,
        len: u64,
        from: u64,
        to: u64,
        /// Its run, once planned.
        run: u32,
        /// Where its first byte is in the region, once planned.
        into: u64,
    }

    /// One file's bytes `at .. at + len`, read into the region at `into`, by the parts
    /// `first .. first + parts`. `need` is where the last record wanted in it ends: the rest
    /// of the run is window, read for the page cache's sake.
    #[derive(Clone, Copy, Debug)]
    struct Run {
        file: u32,
        at: u64,
        len: u64,
        need: u64,
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
        /// Mappings this call asked for, each with a tag of its own ([`Plan::map_tag`]).
        maps: u64,
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
                maps: 0,
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
            self.maps = 0;
        }

        /// `len` bytes at `at` of file `file` wanted, and no more read for it: the want's
        /// number.
        pub(crate) fn want(&mut self, file: u32, at: u64, len: u64) -> u32 {
            self.around(file, at, len, at, at + len)
        }

        /// `len` bytes at `at` of file `file` wanted, read as part of its bytes `from .. to`
        /// (`from <= at` and `at + len <= to`): the want's number. The window's other bytes
        /// are read once, into the system's page cache, for the records beside this one.
        pub(crate) fn around(&mut self, file: u32, at: u64, len: u64, from: u64, to: u64) -> u32 {
            assert!(
                from <= at && at + len <= to,
                "a window {from}..{to} of the store's direct reader that does not hold its record {at}+{len}"
            );
            let n = self.wants.len() as u32;
            self.wants.push(Want { file, at, len, from, to, run: 0, into: 0 });
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
                (want.file, want.from, n)
            });
            self.runs.clear();
            let mut into = 0;
            for at in 0..self.order.len() {
                let n = self.order[at] as usize;
                let want = self.wants[n];
                let (end, need) = (want.to, want.at + want.len);
                let joins = self.runs.last().is_some_and(|run| {
                    let run_end = run.at + run.len;
                    run.file == want.file
                        && want.from <= run_end + GAP_MOST
                        && end.max(run_end) - run.at <= RUN_MOST
                });
                if joins {
                    let run = self.runs.last_mut().expect("a run to join");
                    run.len = run.len.max(end - run.at);
                    run.need = run.need.max(need);
                } else {
                    if let Some(last) = self.runs.last() {
                        into = last.into + last.len;
                    }
                    let len = end - want.from;
                    let run = Run { file: want.file, at: want.from, len, need, into, first: 0, parts: 0 };
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
                // The planted bug reads a run one byte short of its last record's end; the
                // window past it is the page cache's, and no record's.
                let len = match cut {
                    0 => run.len,
                    cut => (run.need - run.at).saturating_sub(cut),
                };
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
                (self.parts.len() as u64) < MAP_PART - self.maps,
                "a call of the store's direct reader reads {} parts and maps {} files, more than its tags name",
                self.parts.len(),
                self.maps
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

        /// The tag of this call's next mapping, and its number among the call's mappings.
        /// Asked before the call's reads are planned ([`Plan::plan`] asserts the two ranges do
        /// not meet).
        pub(crate) fn map_tag(&mut self) -> (usize, Tag) {
            let n = self.maps;
            assert!(
                n < MAP_PART && self.parts.is_empty(),
                "a call of the store's direct reader maps {n} files, or maps after its reads were planned"
            );
            self.maps += 1;
            (n as usize, self.tag(MAP_PART - n))
        }

        /// The number of this call's mapping `tag` names.
        pub(crate) fn map_of(&self, tag: Tag) -> Option<usize> {
            let rel = tag.0.checked_sub(self.tags).filter(|it| *it < TAGS)?;
            let back = MAP_PART - (rel & MAP_PART);
            (rel >> PART_BITS == self.call && back < self.maps).then_some(back as usize)
        }

        /// Blocks of `block` bytes (from a multiple of it) this call's runs read: the device
        /// blocks a round costs, each once.
        pub(crate) fn blocks(&self, block: u64) -> u64 {
            self.runs.iter().map(|run| (run.at + run.len).div_ceil(block) - run.at / block).sum()
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

        /// Records wanted by this call.
        pub(crate) fn wanted(&self) -> usize {
            self.wants.len()
        }

        /// Bytes this call's reads ask for.
        pub(crate) fn asked_bytes(&self) -> u64 {
            self.parts.iter().map(|part| u64::from(part.len)).sum()
        }

        /// The longest run of this call, in bytes.
        pub(crate) fn longest_run(&self) -> u64 {
            self.runs.iter().map(|run| run.len).max().unwrap_or(0)
        }

        /// What this call's reads read, each `(file, at, len)`: the bytes now in the system's
        /// page cache.
        pub(crate) fn read(&self) -> impl Iterator<Item = (u32, u64, u64)> + '_ {
            self.parts.iter().filter_map(|part| match part.state {
                State::Read(len) => Some((self.runs[part.run as usize].file, part.at, u64::from(len))),
                _ => None,
            })
        }
    }
}

/// The counters of every direct reader of the process, as [`counts`] reads them; each reader
/// keeps its own too ([`Direct::own_counts`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Counts {
    /// Records taken into memory, verified or not: read, or taken from a pack's mapping
    /// (`hits`, a part of these).
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
    /// Records of a pack asked (each key a call locates in a pack), whether or not they read.
    pub asked: u64,
    /// Of those, records served from pages the system held: no read, no wait.
    pub hits: u64,
    /// Bytes of the records served from pages the system held.
    pub hit_bytes: u64,
    /// Waits on the driver for a call's reads (each one span `store direct reads`).
    pub waits: u64,
    /// The longest of those waits, in nanoseconds of the driver's clock: a maximum, not a sum.
    pub longest_wait_ns: u64,
    /// The longest run read, in bytes: a maximum, not a sum.
    pub longest_run: u64,
    /// Blocks of [`AROUND`] bytes read from the device (each a block a round's runs cover,
    /// [`plan::Plan::blocks`]): what the page cache did not hold.
    pub blocks_read: u64,
    /// Blocks of the record a pack was first asked for in this process (its first touch)
    /// lies in, asked of the page cache after the pack was mapped.
    pub first_touch_blocks: u64,
    /// Of those, the blocks the page cache held: served with no device read. With
    /// `first_touch_blocks`, how much of a restart the page cache served.
    pub first_touch_held: u64,
    /// Of `waits`, those on a round of mappings (each one span `store direct maps`).
    pub map_waits: u64,
    /// The most reads one wait waited on: the width of the widest round of reads. A maximum.
    pub widest_reads: u64,
    /// The most files one wait mapped: the width of the widest round of maps. A maximum.
    pub widest_maps: u64,
}

static BLOCKS_READ: AtomicU64 = AtomicU64::new(0);
static FIRST_TOUCH_BLOCKS: AtomicU64 = AtomicU64::new(0);
static FIRST_TOUCH_HELD: AtomicU64 = AtomicU64::new(0);
static MAP_WAITS: AtomicU64 = AtomicU64::new(0);
static WIDEST_READS: AtomicU64 = AtomicU64::new(0);
static WIDEST_MAPS: AtomicU64 = AtomicU64::new(0);
static RECORDS: AtomicU64 = AtomicU64::new(0);
static ASKED: AtomicU64 = AtomicU64::new(0);
static HITS: AtomicU64 = AtomicU64::new(0);
static HIT_BYTES: AtomicU64 = AtomicU64::new(0);
static WAITS: AtomicU64 = AtomicU64::new(0);
static LONGEST_WAIT_NS: AtomicU64 = AtomicU64::new(0);
static LONGEST_RUN: AtomicU64 = AtomicU64::new(0);
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
        asked: get(&ASKED),
        hits: get(&HITS),
        hit_bytes: get(&HIT_BYTES),
        waits: get(&WAITS),
        longest_wait_ns: get(&LONGEST_WAIT_NS),
        longest_run: get(&LONGEST_RUN),
        blocks_read: get(&BLOCKS_READ),
        first_touch_blocks: get(&FIRST_TOUCH_BLOCKS),
        first_touch_held: get(&FIRST_TOUCH_HELD),
        map_waits: get(&MAP_WAITS),
        widest_reads: get(&WIDEST_READS),
        widest_maps: get(&WIDEST_MAPS),
    }
}

/// `n` added to a reader's own counter and the process's.
fn count(own: &mut u64, all: &AtomicU64, n: u64) {
    *own += n;
    all.fetch_add(n, Ordering::Relaxed);
}

/// The process's longest wait (ns) and longest run (bytes) since the last call, each then
/// started again from 0: a profile that plots them at each sample has each interval's maximum,
/// and a build's is the largest of its samples'. [`counts`] reads the same two, since that call.
pub fn take_longest() -> (u64, u64) {
    (LONGEST_WAIT_NS.swap(0, Ordering::Relaxed), LONGEST_RUN.swap(0, Ordering::Relaxed))
}

/// The process's widest round of reads and of maps since the last call, each then started
/// again from 0, as [`take_longest`]'s two.
pub fn take_widest() -> (u64, u64) {
    (WIDEST_READS.swap(0, Ordering::Relaxed), WIDEST_MAPS.swap(0, Ordering::Relaxed))
}

/// `n` kept in a reader's own maximum and the process's, where it is larger.
fn most(own: &mut u64, all: &AtomicU64, n: u64) {
    *own = (*own).max(n);
    all.fetch_max(n, Ordering::Relaxed);
}

impl Counts {
    /// What moved from `before` to `self`, two readings of one reader's or the process's
    /// counts: each sum as the difference, each maximum as the later reading.
    pub fn since(&self, before: &Counts) -> Counts {
        Counts {
            records: self.records - before.records,
            bytes: self.bytes - before.bytes,
            reads: self.reads - before.reads,
            runs: self.runs - before.runs,
            files_opened: self.files_opened - before.files_opened,
            maps: self.maps - before.maps,
            mapped_bytes: self.mapped_bytes - before.mapped_bytes,
            leaked_bytes: self.leaked_bytes - before.leaked_bytes,
            asked: self.asked - before.asked,
            hits: self.hits - before.hits,
            hit_bytes: self.hit_bytes - before.hit_bytes,
            waits: self.waits - before.waits,
            longest_wait_ns: self.longest_wait_ns,
            longest_run: self.longest_run,
            blocks_read: self.blocks_read - before.blocks_read,
            first_touch_blocks: self.first_touch_blocks - before.first_touch_blocks,
            first_touch_held: self.first_touch_held - before.first_touch_held,
            map_waits: self.map_waits - before.map_waits,
            widest_reads: self.widest_reads,
            widest_maps: self.widest_maps,
        }
    }

    /// `other` added in: each sum added, each maximum the larger.
    pub fn add(&mut self, other: &Counts) {
        self.records += other.records;
        self.bytes += other.bytes;
        self.reads += other.reads;
        self.runs += other.runs;
        self.files_opened += other.files_opened;
        self.maps += other.maps;
        self.mapped_bytes += other.mapped_bytes;
        self.leaked_bytes += other.leaked_bytes;
        self.asked += other.asked;
        self.hits += other.hits;
        self.hit_bytes += other.hit_bytes;
        self.waits += other.waits;
        self.longest_wait_ns = self.longest_wait_ns.max(other.longest_wait_ns);
        self.longest_run = self.longest_run.max(other.longest_run);
        self.blocks_read += other.blocks_read;
        self.first_touch_blocks += other.first_touch_blocks;
        self.first_touch_held += other.first_touch_held;
        self.map_waits += other.map_waits;
        self.widest_reads = self.widest_reads.max(other.widest_reads);
        self.widest_maps = self.widest_maps.max(other.widest_maps);
    }
}

/// How a reader tells whether a record's bytes are in memory, so it takes them from a pack's
/// mapping with no read and no wait: the system's page cache, asked ([`Residency::Kernel`]),
/// or, in the simulation, which has none, a model of it ([`Residency::model`]).
pub enum Residency {
    /// The system's page cache, asked with `mincore` over the record's pages in the pack's
    /// mapping, as `blaze_core`'s prewarm asks it of a file (Linux and macOS: a page of a
    /// shared file mapping is told in core when the cache holds it, mapped into this process
    /// yet or not). Elsewhere it says no, and every record is read.
    Kernel,
    /// The pages this reader's own reads read, as a page cache that keeps all it was given:
    /// the simulation's, where a mapping is a copy and no page is ever let go.
    Model(Model),
}

/// [`Residency::Model`]'s pages: `(file, page)` of [`MODEL_PAGE`] bytes each.
pub struct Model {
    pages: blaze_hash::HashSet<(FileName, u64), Global>,
}

/// The page of [`Residency::Model`].
pub const MODEL_PAGE: u64 = 4 << 10;

impl Residency {
    /// A model of a page cache that holds nothing yet.
    pub fn model() -> Residency {
        Residency::Model(Model { pages: blaze_hash::HashSet::new_in(Global) })
    }

    /// Whether every page of `record`, the bytes at `at` of `file` (a slice of its mapping), is
    /// in memory.
    fn holds(&self, file: &FileName, record: &[u8], at: u64) -> bool {
        match self {
            Residency::Kernel => in_core(record),
            Residency::Model(model) => {
                let len = record.len() as u64;
                let (first, last) = (at / MODEL_PAGE, (at + len.max(1) - 1) / MODEL_PAGE);
                if last - first >= RESIDENT_PAGES as u64 {
                    return false;
                }
                // Bounded: at most `RESIDENT_PAGES` pages, just above.
                // clone: a refcount, a lookup's key
                (first..=last).all(|page| model.pages.contains(&(file.clone(), page)))
            }
        }
    }

    /// `len` bytes at `at` of `file` were read through the page cache.
    fn read(&mut self, file: &FileName, at: u64, len: u64) {
        let Residency::Model(model) = self else { return };
        if len == 0 {
            return;
        }
        // Every page the read touched: the system reads a page whole into its cache, and a
        // byte of one past the file's end is no record's.
        let (first, end) = (at / MODEL_PAGE, (at + len).div_ceil(MODEL_PAGE));
        for page in first..end {
            // clone: a refcount, once a page read
            model.pages.insert((file.clone(), page));
        }
    }
}

/// Whether every page `bytes` lies on (a slice of a shared file mapping) is in memory: one
/// `mincore` of those pages, no read of them. `false` past [`RESIDENT_PAGES`] pages: such a
/// record is read.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn in_core(bytes: &[u8]) -> bool {
    use std::sync::OnceLock;
    static PAGE: OnceLock<usize> = OnceLock::new();
    let page =*PAGE.get_or_init(|| {
        // SAFETY: `sysconf` reads a constant of the system.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        usize::try_from(page)
            .ok()
            .filter(|it| *it > 0)
            .unwrap_or_else(|| panic!("sysconf(_SC_PAGESIZE) is {page}: no page size, so no page of a pack is asked about"))
    });
    if bytes.is_empty() {
        return true;
    }
    let start = bytes.as_ptr() as usize;
    let first = start - start % page;
    let len = start + bytes.len() - first;
    let pages = len.div_ceil(page);
    if pages > RESIDENT_PAGES {
        return false;
    }
    let mut held = [0u8; RESIDENT_PAGES];
    // SAFETY: `first .. first + len` is the whole pages `bytes` lies on, inside the mapping
    // that holds `bytes` (a mapping is whole pages from a page boundary), so the range is
    // mapped; `held` has a byte for each of its `pages` pages, which is all `mincore` writes.
    let asked = unsafe { libc::mincore(first as *mut libc::c_void, len, held.as_mut_ptr().cast()) };
    // Failing here is the invariant above broken (the range not mapped): loud, never a quiet
    // "not held".
    assert!(
        asked == 0,
        "mincore of {pages} pages of a pack's mapping, at {first:#x}, failed: {}",
        std::io::Error::last_os_error()
    );
    held[..pages].iter().all(|it| it & 1 != 0)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn in_core(_bytes: &[u8]) -> bool {
    false
}

/// The most files one wait of a round of maps maps ([`Direct::map_round`]): each holds a
/// descriptor from its open until its map, so a round stays well inside a process's limit of
/// open files; a store with more unmapped packs takes a wait every this many.
pub const MAPS_A_ROUND: usize = 256;

/// The most pack mappings a store's table ([`Maps`]) is filled to ahead of any ask of them: a
/// mapping costs a range of address space and a kernel map entry, and no page.
pub const MAP_AHEAD_MOST: usize = 1024;

/// The mappings of a store's packs, shared by every direct reader of it (the daemon keeps one
/// a thread a store): a pack is mapped once a process, and its mapping lent to each reader that
/// asks a record of it, so no thread waits to map a pack another mapped. The store keeps it
/// ([`crate::Store::reader`] hands it to each reader made). Filled by a reader's round of maps
/// ([`Direct::map_round`]); a pack its views no longer hold is dropped at the next round.
#[derive(Default)]
pub struct Maps {
    packs: blaze_hash::SharedMap<FileName, PackMap>,
}

/// A pack's mapping in [`Maps`], and whether a record of it was asked yet in this process.
struct PackMap {
    mapping: Mapping,
    asked: std::sync::atomic::AtomicBool,
}

impl Maps {
    /// The pack `file`'s mapping, when it reaches `end`, and whether this is the process's
    /// first ask of a record of it (the pack's first touch).
    fn lend(&self, file: &FileName, end: u64) -> Option<(Mapping, bool)> {
        let packs = self.packs.pin();
        let pack = packs.get(file).filter(|it| it.mapping.bytes().len() as u64 >= end)?;
        let first = !pack.asked.swap(true, Ordering::Relaxed);
        // clone: a refcount
        Some((pack.mapping.clone(), first))
    }

    /// The planted bug's lend: a mapping of another pack reaching `end`.
    #[cfg(test)]
    fn other_than(&self, file: &FileName, end: u64) -> Option<(Mapping, bool)> {
        let packs = self.packs.pin();
        let (_, other) = packs.iter().find(|(name, it)| *name != file && it.mapping.bytes().len() as u64 >= end)?;
        // clone: a refcount
        Some((other.mapping.clone(), false))
    }

    /// Whether the table maps `file`, at any length.
    fn has(&self, file: &FileName) -> bool {
        self.packs.pin().contains_key(file)
    }

    /// Whether this is the process's first ask of a record of the pack `file` (marked asked).
    fn first_ask(&self, file: &FileName) -> bool {
        self.packs.pin().get(file).is_some_and(|it| !it.asked.swap(true, Ordering::Relaxed))
    }

    /// `mapping` kept as `file`'s, unless the table holds one at least as long (a pack only
    /// grows): two readers that mapped one pack at once keep one, the other's dropped with it.
    fn keep(&self, file: FileName, mapping: Mapping) {
        let packs = self.packs.pin();
        if packs.get(&file).is_some_and(|it| it.mapping.bytes().len() >= mapping.bytes().len()) {
            return;
        }
        let asked = packs.get(&file).is_some_and(|it| it.asked.load(Ordering::Relaxed));
        packs.insert(file, PackMap { mapping, asked: std::sync::atomic::AtomicBool::new(asked) });
    }

    fn len(&self) -> usize {
        self.packs.len()
    }

    /// Every mapping dropped. The tests' use: the simulation's mapping is a copy of the file
    /// when mapped, so one taken while a byte was rotted would keep it after the disk is put
    /// back; dropping them all models a page cache that let the rotted page go.
    #[cfg(test)]
    pub(crate) fn clear(&self) {
        self.packs.pin().clear();
    }

    /// Every pack `live` says no to dropped from the table.
    fn retain(&self, live: impl Fn(&FileName) -> bool) {
        self.packs.pin().retain(|file, _| live(file));
    }
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
    /// The live layer the record was found in is gone before its read resolved.
    LayerGone { key: Key, file: FileName, at: u64 },
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
            | Failed::LayerGone { key, .. }
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
            Failed::LayerGone { key, file, at } => write!(
                f,
                "the record of {:x} at {at} of {file} is corrupt: the live layer it was found in is \
                 gone (every record is checked whole on every direct read)",
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
    /// Its mapping was lent to this reader by an ask that was the process's first of a record
    /// of the pack ([`Maps::lend`]): the next record resolved from it counts its blocks
    /// ([`Direct::first_touch`]).
    first: bool,
}

/// What a key was found to be, before its bytes are read.
enum Where {
    Absent,
    Failed(Failed),
    /// A segment's value, verified and copied to the reader's scratch at `at`.
    Held { at: usize, len: usize },
    Want(Want),
    /// A record of a pack no reader of the store has mapped far enough: resolved, to `Held`
    /// or `Want`, once the call's one round of maps is done ([`Direct::resolve_pending`]).
    Pending(PackAsk),
}

/// A record of a pack, located: what [`Direct::resolve`] needs to take it from the pack's
/// mapping or want it read.
struct PackAsk {
    key: Key,
    file: FileName,
    slot: u32,
    loc: Loc,
    tier: usize,
    live: bool,
    /// The bytes its pack commits.
    usable: u64,
    /// Its number among the round's targets.
    target: u32,
}

/// A file a round maps ([`Direct::map_round`]).
struct MapAsk {
    file: FileName,
    /// Its place in the reader's table, for a file a record of the call lies in; `None` for a
    /// pack mapped ahead of any ask, into the store's shared table alone.
    slot: Option<u32>,
    /// The least length the mapping must have: past the last record the call asks of it.
    end: u64,
    /// A pack (kept in the store's shared table), or a value's file of its own (kept in the
    /// reader's table alone).
    pack: bool,
    done: Option<Done>,
}

/// Why a file the call needed was not mapped: each record of it fails, naming the map.
enum MapMiss {
    /// Nothing completed for the driver's stall bound.
    Stalled(Duration),
    /// The map failed: the driver's error.
    Failed(String),
    /// The driver answered the map with another effect's answer.
    Answered(Done),
    /// The round ended with the map not answered.
    Unanswered,
    /// The mapping is shorter than the record asked of it.
    Short { asked: u64, read: u64 },
}

impl MapMiss {
    fn failed(&self, key: Key, file: &FileName) -> Failed {
        // clone: the error's file name
        let file = file.clone();
        match self {
            MapMiss::Stalled(waited) => Failed::Stalled { key, file, at: 0, waited: *waited },
            MapMiss::Short { asked, read } => Failed::Short { key, file, at: 0, asked: *asked, read: *read },
            // alloc: the error's message
            miss => Failed::Read { key, file, at: 0, why: format!("mapping its file: {miss}") },
        }
    }
}

impl fmt::Display for MapMiss {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MapMiss::Stalled(waited) => write!(f, "nothing completed for {waited:?} (blaze_core::IDLE_STALL)"),
            MapMiss::Failed(why) => f.write_str(why),
            MapMiss::Answered(other) => write!(f, "the driver answered a map with {other:?}"),
            MapMiss::Unanswered => f.write_str("the round ended with the map not answered"),
            MapMiss::Short { asked, read } => write!(f, "mapped {read} bytes, short of the {asked} asked"),
        }
    }
}

/// What a wait of the reader waits on: a round of reads, or of maps (how many files).
#[derive(Clone, Copy)]
enum Waiting {
    Reads,
    Maps(usize),
}

/// A record of a pack to read.
struct Want {
    key: Key,
    file: FileName,
    /// Its file's place in the reader's table.
    slot: u32,
    /// Its number in the plan.
    n: u32,
    /// Its number among the round's targets.
    target: u32,
    loc: Loc,
    tier: usize,
    /// Found in the live layer's tiers, not the reader's own.
    live: bool,
    /// Bytes of the record's start, and of its end, taken from its pack's mapping: they lie in
    /// blocks the system held when it was asked ([`Direct::window`]); the rest is read.
    head: u64,
    tail: u64,
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
    /// The files a round maps, and what came of each ([`Direct::map_round`]); kept and
    /// cleared per round.
    to_map: Vec<MapAsk, Global>,
    /// Maps of the round not yet completed.
    maps_left: usize,
    /// The call's number ([`plan::Plan::map_tag`]) of the round's first map: `to_map[i]` is
    /// the call's map `map_base + i`.
    map_base: usize,
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
    /// How a record is told to be in memory, so it is taken from its pack's mapping.
    residency: Residency,
    /// The scratch a round's runs (records and the windows around them) are read into, and
    /// each value copied out of to the caller: kept between calls up to [`REGION_KEPT`].
    region: Vec<u8, Global>,
    /// The files of the reader's table a round could not map, `(slot, why)`: each record of one
    /// fails, naming the map. Kept and cleared per round.
    unmapped: Vec<(u32, MapMiss), Global>,
    /// A record put together from its pack's mapping and the bytes read of it (`Want::head`),
    /// kept and cleared between records.
    stitch: Vec<u8, Global>,
    counts: Counts,
    #[cfg(test)]
    plant: Plant,
}

impl Direct {
    /// A reader of `reader`'s store, asking its driver with the tags `tags .. tags + TAGS`,
    /// taking a record from its pack's mapping, with no read, when `residency` says its pages
    /// are in memory.
    pub fn new(reader: Reader, tags: u64, residency: Residency) -> Direct {
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
            to_map: Vec::new_in(Global),
            maps_left: 0,
            map_base: 0,
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
            residency,
            region: Vec::new_in(Global),
            unmapped: Vec::new_in(Global),
            stitch: Vec::new_in(Global),
            counts: Counts::default(),
            #[cfg(test)]
            plant: Plant::None,
        }
    }

    /// Whether each record asked is a read of its own (the planted bug `Plant::OneByOne`).
    fn one_by_one(&self) -> bool {
        #[cfg(test)]
        if self.plant == Plant::OneByOne {
            return true;
        }
        false
    }

    /// Whether a pack's first touch reads before it asks the page cache (the planted bug
    /// `Plant::ReadBeforeMap`).
    fn plant_read_before_map(&self) -> bool {
        #[cfg(test)]
        if self.plant == Plant::ReadBeforeMap {
            return true;
        }
        false
    }

    /// How many files one wait of a round maps: [`MAPS_A_ROUND`], or one (the planted bug
    /// `Plant::MapEach`).
    fn maps_a_wait(&self) -> usize {
        #[cfg(test)]
        if self.plant == Plant::MapEach {
            return 1;
        }
        MAPS_A_ROUND
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
        self.unmapped.clear();
        // clone: a refcount, so a look borrows nothing of `self`
        let shared = Arc::clone(&self.reader.shared);
        {
            let guard = blaze_reclaim::enter();
            for key in keys {
                let target = self.locate(&shared, &guard, key);
                self.targets.push(target);
            }
        }
        // The packs of the call's records no reader had mapped, all in one round, and then
        // each record asked of the page cache: one wait for every pack the call first touches.
        self.to_map.clear();
        for target in &self.targets {
            let Where::Pending(ask) = target else { continue };
            let end = ask.loc.offset + RECORD_HEADER + ask.loc.stored;
            // Bounded: one entry per file of the call.
            match self.to_map.iter_mut().find(|it| it.slot == Some(ask.slot)) {
                Some(it) => it.end = it.end.max(end),
                // clone: a refcount
                None => self.to_map.push(MapAsk { file: ask.file.clone(), slot: Some(ask.slot), end, pack: true, done: None }),
            }
        }
        if !self.to_map.is_empty() {
            self.map_round(driver, &shared);
            self.resolve_pending(&shared);
        }
        let total = self.plan.plan();
        let mut region = std::mem::take(&mut self.region);
        region.clear();
        let fetched = self.fetch(driver, &mut region, 0, total);
        if matches!(fetched, Fetched::Read) {
            self.after_read();
        }
        self.assemble(&shared, into, &region, &fetched, places);
        region.clear();
        if region.capacity() > REGION_KEPT {
            region = Vec::new_in(Global);
        }
        self.region = region;
    }

    /// After a round's reads: what they read is in the page cache, told to the model where it
    /// is one (the kernel's needs no telling). Each file they read was mapped before (the
    /// round's maps come first), so the records beside them are taken from the mapping by the
    /// calls after this one.
    fn after_read(&mut self) {
        for (slot, at, len) in self.plan.read() {
            self.residency.read(&self.files[slot as usize].name, at, len);
        }
    }

    /// Maps, in one round of concurrent maps (one wait, one span `store direct maps`), every
    /// file of `self.to_map`, and with them every pack of the store's views no reader of it has
    /// mapped yet (up to [`MAP_AHEAD_MOST`] in the store's table): the packs a restarted
    /// build will touch are known (the head lists them) though the records it will ask are
    /// not, so the first ask maps them all and no later ask of any thread waits on a map. A
    /// mapping reads nothing: no page of a pack is read for being mapped. Each asked file's
    /// mapping goes to the reader's table (and a pack's to the store's, [`Maps`]); each that
    /// failed is recorded in `self.unmapped`, so each record of it fails naming the map. A pack
    /// mapped ahead that failed is said in the log and mapped again when a record of it is
    /// asked, which then fails by name if it fails again.
    fn map_round<D: Driver>(&mut self, driver: &mut D, shared: &Shared) {
        if self.to_map.iter().any(|it| it.pack) {
            self.ahead(shared);
        }
        let mut to_map = std::mem::take(&mut self.to_map);
        let first = to_map.first().map(|it| it.file.clone()); // clone: a refcount, for the span
        let span = tracing::info_span!(
            target: "blaze",
            "store direct maps",
            files = to_map.len() as u64,
            asked = to_map.iter().filter(|it| it.slot.is_some()).count() as u64,
            file = first.as_ref().map(FileName::as_str),
            bytes = tracing::field::Empty,
        );
        let _in = span.enter();
        let mut stalled = None;
        let step = self.maps_a_wait().max(1);
        let mut at = 0;
        // Bounded: each turn asks the next `step` maps, and ends at the first stall.
        while at < to_map.len() && stalled.is_none() {
            let upto = (at + step).min(to_map.len());
            for (n, ask) in to_map[at..upto].iter_mut().enumerate() {
                // The round's maps are numbered in its order from the call's first of the
                // round (`map_base`): `drive` reads each back to its place in `to_map`.
                let (number, tag) = self.plan.map_tag();
                if at + n == 0 {
                    self.map_base = number;
                }
                assert_eq!(number, self.map_base + at + n, "a round's maps are the call's, numbered in order");
                // clone: a refcount
                self.effects.push(Effect::Map { file: ask.file.clone(), extent: Extent::Whole, tag });
                ask.done = None;
            }
            self.maps_left = upto - at;
            self.to_map = to_map;
            let driven = self.drive(driver, Waiting::Maps(upto - at));
            to_map = std::mem::take(&mut self.to_map);
            if let Err(waited) = driven {
                stalled = Some(waited);
                self.maps_left = 0;
            }
            at = upto;
        }
        let mut bytes = 0;
        for ask in to_map.drain(..) {
            let missed = match ask.done {
                Some(Done::Mapped(mapping)) => {
                    let len = mapping.bytes().len() as u64;
                    count(&mut self.counts.maps, &MAPS, 1);
                    bytes += len;
                    if ask.pack {
                        // clone: a refcount, kept in the store's table
                        self.reader.maps.keep(ask.file.clone(), mapping.clone());
                    }
                    match ask.slot {
                        Some(_) if len < ask.end => Some(MapMiss::Short { asked: ask.end, read: len }),
                        Some(slot) => {
                            let first = ask.pack && self.reader.maps.first_ask(&ask.file);
                            let open = &mut self.files[slot as usize];
                            open.first |= first;
                            open.mapping = Some(mapping);
                            None
                        }
                        None => None,
                    }
                }
                Some(Done::Failed(why)) => Some(MapMiss::Failed(why.0)),
                Some(other) => Some(MapMiss::Answered(other)),
                None => Some(match stalled {
                    Some(waited) => MapMiss::Stalled(waited),
                    None => MapMiss::Unanswered,
                }),
            };
            let Some(miss) = missed else { continue };
            match ask.slot {
                Some(slot) => self.unmapped.push((slot, miss)),
                None => tracing::warn!(
                    target: "blaze",
                    file = ask.file.as_str(),
                    why = %miss,
                    "a pack mapped ahead of any ask was not mapped: a record of it asked later maps it again, and fails by name if that fails"
                ),
            }
        }
        span.record("bytes", bytes);
        self.to_map = to_map;
        self.prune(shared);
    }

    /// Every pack of the store's views (its own tiers' and its live layer's) that is not
    /// retired, not in the store's table of mappings and not asked already, added to the
    /// round's maps, while the table and the round together stay within [`MAP_AHEAD_MOST`].
    fn ahead(&mut self, shared: &Shared) {
        let room = MAP_AHEAD_MOST.saturating_sub(self.reader.maps.len() + self.to_map.len());
        if room == 0 {
            return;
        }
        let asked = self.to_map.len();
        let guard = blaze_reclaim::enter();
        let held = Held::task(&guard);
        // Bounded: the views' packs, at most `room` taken.
        'views: for live in [false, true] {
            let Some(view) = view_of(shared, held, live) else { continue };
            for tier in &view.tiers {
                for pack in &tier.packs {
                    if self.to_map.len() - asked >= room {
                        break 'views;
                    }
                    if pack.gone || pack.usable == 0 || self.reader.maps.has(&pack.file) {
                        continue;
                    }
                    // Bounded: the asked files, a few a call.
                    if self.to_map[..asked].iter().any(|it| it.file == pack.file) {
                        continue;
                    }
                    self.to_map.push(MapAsk {
                        // clone: a refcount
                        file: pack.file.clone(),
                        slot: None,
                        end: pack.usable,
                        pack: true,
                        done: None,
                    });
                }
            }
        }
    }

    /// The store's table of mappings without the packs its views no longer hold (retired by
    /// the collector, or of a writer the live layer no longer lists): dropping the mapping lets
    /// the unlinked file's blocks go once no reader holds a clone of it.
    fn prune(&mut self, shared: &Shared) {
        let guard = blaze_reclaim::enter();
        let held = Held::task(&guard);
        let views = [view_of(shared, held, false), view_of(shared, held, true)];
        // Bounded: each kept mapping looked up among the views' packs, once a round of maps
        // (at most MAP_AHEAD_MOST mappings), with no table built for it.
        self.reader.maps.retain(|file| {
            views.iter().flatten().any(|view| {
                view.tiers.iter().any(|tier| tier.packs.iter().any(|it| !it.gone && it.file == *file))
            })
        });
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
        let pack = self.mapping_of(driver, &shared, key, slot, end, true)?;
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
                        (self.mapping_of(driver, &shared, key, slot, header.raw, false)?, 0)
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

    /// Where `key`'s record lies, when it is in a pack: its file, its offset, its length
    /// (header and payload) and the bytes its pack commits.
    #[cfg(test)]
    pub(crate) fn record_of(&self, key: &Key) -> Option<(FileName, u64, u64, u64)> {
        let guard = blaze_reclaim::enter();
        let (look, _, found) = find(&self.reader.shared, &guard, key)?;
        let found = found.ok()?;
        let Place::Pack(loc) = found.place else { return None };
        let pack = look.view().tiers[found.tier].packs.get(loc.pack as usize)?;
        // clone: a test's file name
        Some((pack.file.clone(), loc.offset, RECORD_HEADER + loc.stored, pack.usable))
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
        self.to_map.clear();
        self.maps_left = 0;
        self.unmapped.clear();
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
        self.files.push(Open { name: name.clone(), used: self.tick, mapping: None, first: false });
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
        count(&mut self.counts.asked, &ASKED, 1);
        let len = RECORD_HEADER + loc.stored;
        let end = loc.offset + len;
        // The target this is: `read_round` pushes each as it is located.
        let target = self.targets.len() as u32;
        if self.one_by_one() {
            let n = self.plan.want(slot, loc.offset, len);
            return Where::Want(Want { key: *key, file, slot, n, target, loc, tier: found.tier, live, head: 0, tail: 0 });
        }
        let usable = look.view().tiers[found.tier].packs[loc.pack as usize].usable;
        let ask = PackAsk { key: *key, file, slot, loc, tier: found.tier, live, usable, target };
        // The pack mapped first, then its pages asked of the system: a record whose pages the
        // system holds is never read from the device, on the pack's first touch as on any
        // other. A pack this reader, or another of the store's, mapped already is asked now; one
        // no reader mapped waits for the call's one round of maps (`map_round`).
        match self.adopt(slot, end) {
            true => self.resolve(look.view(), ask),
            false => Where::Pending(ask),
        }
    }

    /// Whether the file at `slot` is mapped to at least `end` in this reader's table: its own
    /// mapping, or the store's shared one ([`Maps`]) taken into the table now.
    fn adopt(&mut self, slot: u32, end: u64) -> bool {
        let open = &mut self.files[slot as usize];
        if open.mapping.as_ref().is_some_and(|it| it.bytes().len() as u64 >= end) {
            return true;
        }
        let maps = &self.reader.maps;
        // The planted bug: the table's mapping of some other pack, where it has one.
        #[cfg(test)]
        let lent = match self.plant {
            Plant::OtherPacksMapping => maps.other_than(&open.name, end),
            _ => None,
        };
        #[cfg(not(test))]
        let lent = None;
        let Some((mapping, first)) = lent.or_else(|| maps.lend(&open.name, end)) else { return false };
        open.mapping = Some(mapping);
        open.first |= first;
        true
    }

    /// `ask`, a record of a pack mapped to past its end in the reader's table: taken from the
    /// mapping when the system holds its pages (checked as a read one is, with no read and no
    /// wait), else wanted with its window ([`Direct::window`]) for the call's one round of
    /// reads. `view` is the one its key was found in.
    fn resolve(&mut self, view: &View, ask: PackAsk) -> Where {
        let PackAsk { key, file, slot, loc, tier, live, usable, target } = ask;
        let len = RECORD_HEADER + loc.stored;
        let end = loc.offset + len;
        let open = &mut self.files[slot as usize];
        // clone: a refcount
        let mapping = open.mapping.clone().expect("a record is resolved once its pack is mapped past it");
        let first = std::mem::take(&mut open.first);
        let record = &mapping.bytes()[loc.offset as usize..end as usize];
        if first {
            self.first_touch(&file, &mapping, loc.offset, end, usable);
        }
        // The planted bug: a pack's first touch read from the device, the system not asked.
        let read_first = first && self.plant_read_before_map();
        if !read_first && self.residency.holds(&file, record, loc.offset) {
            count(&mut self.counts.records, &RECORDS, 1);
            count(&mut self.counts.hits, &HITS, 1);
            count(&mut self.counts.hit_bytes, &HIT_BYTES, len);
            let (at, checks) = (self.held.len(), self.checks());
            return match finish(view, tier, key, loc, &file, record, checks, &mut self.held) {
                Ok((len, follow)) => {
                    if let Some(follow) = follow {
                        self.follows.push((target, follow));
                    }
                    Where::Held { at, len }
                }
                Err(failed) => Where::Failed(failed),
            };
        }
        let (from, to) = match read_first {
            // The planted bug: the record's whole window, as when no mapping was asked first.
            true => (loc.offset - loc.offset % AROUND, ((end - 1) - (end - 1) % AROUND + AROUND).min(usable)),
            false => self.window(&file, slot, loc.offset, end, usable),
        };
        // The part of the record the window holds; the rest lies in blocks the system holds,
        // and is taken from the mapping (`take`).
        let (part_at, part_end) = (loc.offset.max(from), end.min(to));
        let n = self.plan.around(slot, part_at, part_end - part_at, from, to);
        Where::Want(Want { key, file, slot, n, target, loc, tier, live, head: part_at - loc.offset, tail: end - part_end })
    }

    /// The first ask of a record of a pack in this process: each block of [`AROUND`] bytes the
    /// record at `at .. end` lies in, counted, and counted again when the system holds it
    /// (`Counts::first_touch_blocks`, `Counts::first_touch_held`).
    fn first_touch(&mut self, file: &FileName, mapping: &Mapping, at: u64, end: u64, usable: u64) {
        let limit = usable.min(mapping.bytes().len() as u64);
        let mut block = at - at % AROUND;
        // Bounded: each turn passes one block of the record's.
        while block < end {
            let bytes = &mapping.bytes()[block as usize..(block + AROUND).min(limit) as usize];
            count(&mut self.counts.first_touch_blocks, &FIRST_TOUCH_BLOCKS, 1);
            if self.residency.holds(file, bytes, block) {
                count(&mut self.counts.first_touch_held, &FIRST_TOUCH_HELD, 1);
            }
            block += AROUND;
        }
    }

    /// The call's records of packs no reader had mapped far enough (`Where::Pending`), each
    /// resolved now that the call's round of maps is done: or failed, naming the map, where its
    /// pack could not be mapped.
    fn resolve_pending(&mut self, shared: &Shared) {
        let guard = blaze_reclaim::enter();
        let held = Held::task(&guard);
        let (own, live) = (view_of(shared, held, false), view_of(shared, held, true));
        for at in 0..self.targets.len() {
            if !matches!(self.targets[at], Where::Pending(_)) {
                continue;
            }
            let Where::Pending(ask) = std::mem::replace(&mut self.targets[at], Where::Absent) else {
                unreachable!("matched just above")
            };
            let end = ask.loc.offset + RECORD_HEADER + ask.loc.stored;
            let view = if ask.live { live } else { own };
            let miss = self.unmapped.iter().find(|it| it.0 == ask.slot).map(|it| it.1.failed(ask.key, &ask.file));
            let target = if let Some(failed) = miss {
                Where::Failed(failed)
            } else if let Some(view) = view {
                // The round mapped the pack past `end`, or recorded why not (`map_round`).
                match self.adopt(ask.slot, end) {
                    true => self.resolve(view, ask),
                    false => {
                        let read = self.files[ask.slot as usize].mapping.as_ref().map_or(0, |it| it.bytes().len() as u64);
                        Where::Failed(Failed::Short { key: ask.key, file: ask.file, at: 0, asked: end, read })
                    }
                }
            } else {
                Where::Failed(Failed::LayerGone { key: ask.key, file: ask.file, at: ask.loc.offset })
            };
            self.targets[at] = target;
        }
    }

    /// The window a record at `at .. end` of the pack at `slot` (`usable` bytes committed) is
    /// read with: the blocks of [`AROUND`] bytes (from a multiple of it, the last clipped to
    /// `usable`) it lies in, less those at either end whose pages the system holds, when the
    /// pack's mapping reaches them (the record's bytes there are taken from it). Each block is
    /// then read whole or not at all, so a block is read once however its records are asked:
    /// the record's blocks the system holds are at its ends only (a block inside a record holds
    /// no other record's bytes, so it is read with this record or never).
    fn window(&self, file: &FileName, slot: u32, at: u64, end: u64, usable: u64) -> (u64, u64) {
        let block = |it: u64| it - it % AROUND;
        let (mut first, mut last) = (block(at), block(end - 1));
        let whole = (first, (last + AROUND).min(usable));
        let Some(mapping) = &self.files[slot as usize].mapping else { return whole };
        if (mapping.bytes().len() as u64) < whole.1 {
            return whole;
        }
        let held = |it: u64| {
            let bytes = &mapping.bytes()[it as usize..(it + AROUND).min(usable) as usize];
            self.residency.holds(file, bytes, it)
        };
        // Bounded: each turn passes one block of the record's, of which there are at most
        // `len / AROUND + 2`.
        while first < last && held(first) {
            first += AROUND;
        }
        while last > first && held(last) {
            last -= AROUND;
        }
        // One block left that the system holds whole: a page of the record let go between the
        // record's ask and the block's (or the record is wholly in it); read it all.
        if first == last && held(first) {
            return whole;
        }
        (first, (last + AROUND).min(usable))
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
        let blocks = self.plan.blocks(AROUND);
        count(&mut self.counts.runs, &RUNS, self.plan.runs());
        count(&mut self.counts.reads, &READS, self.plan.parts());
        count(&mut self.counts.bytes, &BYTES, total as u64);
        count(&mut self.counts.blocks_read, &BLOCKS_READ, blocks);
        most(&mut self.counts.longest_run, &LONGEST_RUN, self.plan.longest_run());
        most(&mut self.counts.widest_reads, &WIDEST_READS, self.plan.parts());
        // A device read's span: how many reads at once, the blocks and bytes they read, and
        // the records they were for.
        let span = tracing::info_span!(
            target: "blaze",
            "store direct reads",
            reads = self.plan.parts(),
            blocks,
            records = self.plan.wanted(),
            bytes = self.plan.asked_bytes(),
        );
        let stalled = span.in_scope(|| self.drive(driver, Waiting::Reads)).err();
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

    /// Runs the call's effects until its reads and its maps have completed, or until nothing
    /// has completed for [`IDLE_STALL`]: how long it waited, then. Each is one wait, inside
    /// its caller's span (`store direct reads` or `store direct maps`, which say what it waits
    /// for), counted in [`Counts::waits`] (and [`Counts::map_waits`]) and its length kept in
    /// [`Counts::longest_wait_ns`] by the driver's clock.
    fn drive<D: Driver>(&mut self, driver: &mut D, waiting: Waiting) -> Result<(), Duration> {
        let began = driver.now();
        let driven = self.drive_inner(driver, waiting);
        let took = driver.now().saturating_sub(began);
        count(&mut self.counts.waits, &WAITS, 1);
        if let Waiting::Maps(files) = waiting {
            count(&mut self.counts.map_waits, &MAP_WAITS, 1);
            most(&mut self.counts.widest_maps, &WIDEST_MAPS, files as u64);
        }
        let took = u64::try_from(took.as_nanos()).unwrap_or(u64::MAX);
        most(&mut self.counts.longest_wait_ns, &LONGEST_WAIT_NS, took);
        driven
    }

    fn drive_inner<D: Driver>(&mut self, driver: &mut D, waiting: Waiting) -> Result<(), Duration> {
        let on = match waiting {
            Waiting::Reads => WaitOn::Io("store direct reads: a record's bytes"),
            Waiting::Maps(_) => WaitOn::Io("store direct maps: a pack's mapping"),
        };
        driver.submit(&mut self.effects);
        let mut since = driver.now();
        // Bounded: each turn completes one of the call's effects, or waits out part of the
        // stall, which ends it.
        while self.plan.left() > 0 || self.maps_left > 0 {
            let stall = Deadline::after(since, IDLE_STALL);
            let out = &mut self.out;
            let got = wait(on, None, || driver.poll(out, stall));
            if got > 0 {
                since = driver.now();
            } else if stall.passed(driver.now()) {
                return Err(driver.now().saturating_sub(since));
            }
            for completion in self.out.drain(..) {
                if let Some(n) = self.plan.map_of(completion.tag)
                    && let Some(ask) = n.checked_sub(self.map_base).and_then(|at| self.to_map.get_mut(at))
                    && ask.done.is_none()
                {
                    ask.done = Some(completion.done);
                    self.maps_left -= 1;
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

    /// The mapping of the file at `slot`, at least `end` bytes long: the reader's table's, the
    /// store's shared one (a `pack`), or a new one, made in a round of its own
    /// ([`Direct::map_round`], which maps the store's other unmapped packs with a pack). A value
    /// read in place this way is no record resolved from its pack, so it counts no first touch.
    fn mapping_of<D: Driver>(
        &mut self,
        driver: &mut D,
        shared: &Shared,
        key: &Key,
        slot: u32,
        end: u64,
        pack: bool,
    ) -> Result<Mapping, Failed> {
        let file = self.files[slot as usize].name.clone(); // clone: a refcount
        if !self.adopt(slot, end) {
            self.to_map.clear();
            self.unmapped.clear();
            // clone: a refcount
            self.to_map.push(MapAsk { file: file.clone(), slot: Some(slot), end, pack, done: None });
            self.map_round(driver, shared);
            if let Some((_, miss)) = self.unmapped.iter().find(|it| it.0 == slot) {
                return Err(miss.failed(*key, &file));
            }
        }
        let open = &mut self.files[slot as usize];
        open.first = false;
        match &open.mapping {
            Some(mapping) if mapping.bytes().len() as u64 >= end => Ok(mapping.clone()), // clone: a refcount
            other => {
                let read = other.as_ref().map_or(0, |it| it.bytes().len() as u64);
                Err(Failed::Short { key: *key, file, at: 0, asked: end, read })
            }
        }
    }

    /// Each target's value checked, from the round's `region` or the reader's scratch, and
    /// appended to `into`, and its answer pushed to `places` in the caller's order.
    fn assemble<A: Allocator, B: Allocator>(
        &mut self,
        shared: &Shared,
        into: &mut Vec<u8, A>,
        region: &[u8],
        fetched: &Fetched,
        places: &mut Vec<Placed, B>,
    ) {
        let start = into.len();
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
                    Some(self.take(view, &want, into, region, fetched))
                }
                Where::Pending(_) => unreachable!("a round resolves every record pending on its maps before it reads"),
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
                    let at = if sorted { cursor } else { at };
                    cursor += len;
                    Placed::Found { at, len }
                }
            });
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

    /// `want`'s record, read into `region`, checked and its value appended to `into`: where
    /// the value is.
    fn take<A: Allocator>(
        &mut self,
        view: Option<&View>,
        want: &Want,
        into: &mut Vec<u8, A>,
        region: &[u8],
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
        let Some(view) = view else {
            // alloc: the error's message
            let what = "the live layer it was found in is gone".into();
            return Err(Failed::Corrupt { key, file: file(), at: loc.offset, what });
        };
        let len = RECORD_HEADER + loc.stored;
        let at = off as usize;
        let part = &region[at..at + (len - want.head - want.tail) as usize];
        let mut stitch = std::mem::take(&mut self.stitch);
        let record = match (want.head, want.tail) {
            (0, 0) => part,
            (head, tail) => {
                // `window` trims only blocks the mapping reached, and a mapping is replaced
                // only by a longer one of the same committed bytes.
                let open = &self.files[want.slot as usize];
                let mapping = open.mapping.as_ref().expect("a record read in part has its pack mapped");
                let (start, end) = (loc.offset as usize, (loc.offset + len) as usize);
                stitch.clear();
                stitch.extend_from_slice(&mapping.bytes()[start..start + head as usize]);
                stitch.extend_from_slice(part);
                stitch.extend_from_slice(&mapping.bytes()[end - tail as usize..end]);
                &stitch[..]
            }
        };
        let out = into.len();
        let finished = finish(view, want.tier, key, loc, &want.file, record, self.checks(), into);
        self.stitch = stitch;
        let (len, follow) = finished?;
        if let Some(follow) = follow {
            self.follows.push((want.target, follow));
        }
        count(&mut self.counts.records, &RECORDS, 1);
        Ok((out, len))
    }
}

/// `record` (header and payload), `key`'s at `loc` of `file`, checked and its value appended
/// to `out`, read from a round's region or from a pack's mapping alike: the value's length, and
/// what the call's second round reads for it. A chunked value's list, or nothing for a file of
/// its own, stands in its place until `follow_up` reads it. `out` is untouched on a failure.
#[expect(clippy::too_many_arguments, reason = "a record's place, its bytes and where they go")]
fn finish<A: Allocator>(
    view: &View,
    tier: usize,
    key: Key,
    loc: Loc,
    file: &FileName,
    record: &[u8],
    check: bool,
    out: &mut Vec<u8, A>,
) -> Result<(usize, Option<Follow>), Failed> {
    // clone: the error's file name
    let corrupt = |what: String| Failed::Corrupt { key, file: file.clone(), at: loc.offset, what };
    let (header, sealed) = checked_header(record, &key, loc, check).map_err(corrupt)?;
    let payload = &record[RECORD_HEADER as usize..];
    let follow = match header.form {
        Form::Chunked => {
            chunk_list(&header, payload).map_err(corrupt)?;
            let hash = sealed != Sealed::Whole && check;
            // clone: a refcount of the file's name
            Some(Follow::Chunks { file: file.clone(), at: loc.offset, content: header.content, hash })
        }
        Form::External => {
            let external = view.tiers.get(tier).and_then(|it| it.externals.get(&header.content));
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
        let len = if header.form == Form::Chunked { payload.len() } else { 0 };
        out.extend_from_slice(&payload[..len]);
        return Ok((len, Some(follow)));
    }
    let value = decode(view, tier, &header, payload).map_err(corrupt)?;
    if value.len() as u64 != header.raw {
        // alloc: the error's message
        return Err(corrupt(format!("it is {} bytes, not the {} recorded", value.len(), header.raw)));
    }
    // A format-9 record's CRC, checked above, covered the bytes the value is decoded from;
    // a format-8 record's value is hashed.
    if check && !whole_by_crc(&header, payload, sealed) {
        let hashed = content_of(&value);
        if hashed != header.content {
            return Err(corrupt(format!("its bytes hash to {hashed:x}, not the {:x} recorded", header.content)));
        }
    }
    out.extend_from_slice(&value);
    Ok((value.len(), None))
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
