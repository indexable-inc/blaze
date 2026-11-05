//! blaze v2's store (docs/v2/plan/lanes/store.md). Replaces `blaze-store`.
//!
//! - A value is kept under a [`Key`]: the BLAKE3 hash of exactly what it was made from (the
//!   source, cfg, features, target, toolchain, the layout facts read, the schema and code of
//!   what made it). A key that misses an input is a bug.
//! - Values live in **packs**: aligned zero-copy records, mmapped and read in place; hot ones
//!   uncompressed, cold ones zstd with a dictionary per [`Kind`]; large outputs chunked by
//!   content so near-identical binaries share bytes. A pack is appended to and its index
//!   written into the inactive of two slots, then the header flipped: one barrier, not a sync
//!   per entry (#1298).
//! - The **index** is an mmapped open-addressed table on keys: one or two cache misses a
//!   lookup.
//! - **Verified**: [`Store::get`] checks a value's content hash before handing it out; a value
//!   that fails is [`Corrupt`], never served. Paths of archives and outputs can't escape their
//!   directory.
//! - **Only the daemon writes the store**: user code's outputs are hashed and checked by the
//!   daemon before they are put. Each tenant has a store; only the read-only base layer of
//!   public dependencies is shared.
//! - [`Store`] does no IO: it maps and appends through [`blaze_core::Effect`]s.
//! - **Reads take no lock and write no shared line** (docs/v2/plan/design/concurrency.md): a
//!   [`Reader`] reads from any task, through tables the store publishes once and frees at task
//!   boundaries ([`blaze_reclaim`]). Each worker puts through a [`Writer`] of its own, into a
//!   segment of its own; the store copies the segments into its packs at a commit and points
//!   each key at its pack with one swap.
//!
//! The layout on disk and why it survives a crash at any point: docs/v2-store.md.
//!
//! Its in-memory tables and segments sit on `blaze_mem` regions tagged `Tag::Store`
//! (docs/v2/plan/lanes/mem/backend-cas.md): a lookup that hits and a warm put allocate nothing.

#![feature(allocator_api)]

pub mod chunk;
mod cold;
mod copy;
use blaze_ids::crc32c as crc;
mod disk;
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
mod driven;
mod flags;
mod handle;
mod index;
#[cfg(all(test, blaze_check))]
mod model;
mod peers;
mod read;
pub mod remote;
mod segment;
mod sync;
mod table;
#[cfg(all(test, not(blaze_check)))]
mod tests;
mod words;

#[cfg(all(test, not(blaze_check)))]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

use std::{collections::VecDeque, fmt, sync::Arc};

use blaze_core::{Completion, Done, Effect, Extent, FileName, Mapping, Tag};
use blaze_format::{SchemaHash, tiers::BufferColumns};
use blaze_hash::{ByHash, ByHashSet, HashMap, HashSet};
use blaze_ids::{ContentId, Hasher, radix};
use blaze_reclaim::{Grace, Limbo};
pub use blaze_reclaim::{Guard, enter};
use serde::{Deserialize, Serialize};
use std::alloc::Global;

pub use crate::disk::content_of;
use crate::disk::{
    DICTIONARY_DOMAIN, DictionaryRef, ExternalRef, Form, HEAD_FILE, Head, IndexRef, Loc,
    NO_DICTIONARY, PACK_HEADER, PackRef, RECORD_HEADER, RecordHeader, SCHEMA_PREFIX,
};
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub use crate::driven::Driven;
use crate::flags::{FLAGS, Flag, Row};
pub use crate::handle::Handle;
pub use crate::peers::SharedConfig;
pub use crate::read::Pinned;
use crate::read::{
    ExternalView, Index, Look, PackView, Place, Seg, Shared, Spot, TierView, View, carry_flags,
};
#[cfg(feature = "test-hooks")]
pub use crate::segment::HeldPut;
pub use crate::segment::Writer;
use crate::segment::{COPY, Chunk, ChunkRef, FRAME, Own, Segment};
use crate::table::{Held, Table};

/// The format of everything in a store's directory. A store of another refuses to open, naming
/// both; nothing reads it as this one.
pub(crate) const FORMAT: u32 = 3;

/// A pack is closed for appends once it is this long; a record longer than that has a pack of
/// its own.
#[cfg(not(test))]
const PACK_MOST: u64 = 1 << 30;
#[cfg(test)]
const PACK_MOST: u64 = 256 << 10;

/// A value shorter than this stays as it is: compressing it saves less than its index slot
/// costs. A cold kind's value is the exception once its kind has a dictionary: values of one
/// kind share most of their bytes, which the dictionary takes out of even a short one.
const DEMOTE_FROM: u64 = 4 << 10;

/// One [`Store::settle`] moves or copies at most this many bytes, so it stays a short step.
#[cfg(not(test))]
const SETTLE_MOST: u64 = 256 << 20;
#[cfg(test)]
const SETTLE_MOST: u64 = 1 << 20;

/// Of a [`Store::settle`]'s [`SETTLE_MOST`], at most this much goes to short cold records
/// (each counted as [`DEMOTE_FROM`]), so a backlog of them does not take whole settles.
const SHORT_MOST: u64 = SETTLE_MOST / 4;

/// Outputs from this long on are kept as content-defined chunks.
const CHUNK_FROM: usize = 256 << 10;

/// Outputs from this long on are placed by cloning a whole file of their bytes the store keeps
/// beside its packs (`placed/<content>`, [`Store::place`]), written the first time one is placed:
/// every build dir that holds the output, a second checkout's among them, shares its blocks. A
/// shorter one is written where it goes: it is a few blocks, and a file of its own would cost
/// the store an inode each.
const CLONE_FROM: u64 = 16 << 10;

/// The mode of a whole file kept to be cloned ([`CLONE_FROM`]): no one writes it, so a build
/// dir's clone, which shares its blocks copy-on-write, is the only thing a program can change.
const PLACED_MODE: u32 = 0o444;

/// Hot values (not outputs) from this long on are files of their own, so they are placed by
/// sharing blocks and mapped on their own.
#[cfg(not(test))]
const EXTERNAL_FROM: usize = 4 << 20;
#[cfg(test)]
const EXTERNAL_FROM: usize = 96 << 10;

/// A new index table is written once the records not in the mapped one reach this many.
#[cfg(not(test))]
const INDEX_AFTER: usize = 1 << 16;
#[cfg(test)]
const INDEX_AFTER: usize = 48;

/// Slots of a delta table's first level: room for what comes before a new index takes it.
const DELTA_FIRST: usize = (INDEX_AFTER * 2).next_power_of_two();

/// Values made whole to be read are kept until they are this many bytes, then let go of at
/// task boundaries.
#[cfg(not(test))]
const OWNED_MOST: u64 = 256 << 20;
#[cfg(test)]
const OWNED_MOST: u64 = 1 << 20;

/// IO that fails this many times in a row breaks the store.
const FAILURES_MOST: u32 = 8;

/// The packs of one store; a record's place in memory is its tier, pack and offset in 64 bits.
const PACKS_MOST: usize = 1 << 16;

// A settle sorts its demotions by kind, then known or not resident (bit 95), then pack and
// offset (`Store::settle`): a pack's number stays below that bit.
const _: () = assert!(PACKS_MOST <= 1 << 31);

/// The most records one settle asks the page cache about ([`Effect::Residency`]): at most this
/// many `cachestat` calls of the driver's, each of microseconds, a settle.
const RESIDENCY_MOST: usize = 1 << 14;

/// What a value was made from, hashed.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Key(pub ContentId);

impl Key {
    /// The key of what `producer` makes from `inputs` (#1439): `schema` is the schema of the
    /// value made, `producer` the content ID of the code that made it (the code in its own
    /// dependency closure, never a hash of all of blaze's sources), `inputs` the content ID of
    /// every input it read, in the order it read them. An input left out is a bug: the value
    /// would be reused for inputs it was not made from.
    pub fn of(schema: SchemaHash, producer: ContentId, inputs: &[ContentId]) -> Key {
        let mut hasher = Hasher::new("blaze v2 store key 1");
        hasher.id(&schema.0).id(&producer).update(&(inputs.len() as u64).to_le_bytes());
        for input in inputs {
            hasher.id(input);
        }
        Key(hasher.finish())
    }
}

/// What kind of value: picks its compression dictionary and its pack.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Kind {
    /// A query's result in the zero-copy format.
    Query,
    /// A compiled function's blob (`blaze-codegen`).
    Code,
    /// Debug info and panic locations of a compiled function, by its key.
    Debug,
    /// A build script's or proc macro's run: its outputs and traced inputs.
    Run,
    /// A whole output (an executable, a library), as content-defined chunks.
    Output,
    /// The daemon's image.
    Image,
    /// A whole module of machine code its loader maps (a Wasmtime module's): always a file of
    /// its own, never compressed, so [`Pinned::file`] names it and the loader maps it
    /// copy-on-write, its pages read only as its code runs.
    Machine,
}

impl Kind {
    pub const ALL: [Kind; 7] =
        [Kind::Query, Kind::Code, Kind::Debug, Kind::Run, Kind::Output, Kind::Image, Kind::Machine];

    /// The kind's byte on disk and on the wire.
    pub const fn code(self) -> u8 {
        match self {
            Kind::Query => 0,
            Kind::Code => 1,
            Kind::Debug => 2,
            Kind::Run => 3,
            Kind::Output => 4,
            Kind::Image => 5,
            Kind::Machine => 6,
        }
    }

    pub const fn from_code(code: u8) -> Option<Kind> {
        match code {
            0 => Some(Kind::Query),
            1 => Some(Kind::Code),
            2 => Some(Kind::Debug),
            3 => Some(Kind::Run),
            4 => Some(Kind::Output),
            5 => Some(Kind::Image),
            6 => Some(Kind::Machine),
            _ => None,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Kind::Query => "query",
            Kind::Code => "code",
            Kind::Debug => "debug",
            Kind::Run => "run",
            Kind::Output => "output",
            Kind::Image => "image",
            Kind::Machine => "machine",
        }
    }

    /// Read rarely (debug info on a panic or in a debugger; a run's outputs once per hit), so
    /// kept compressed. The rest are read in place on every warm path.
    const fn cold(self) -> bool {
        matches!(self, Kind::Debug | Kind::Run)
    }
}

/// A value as the store holds it, read in place.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Entry<'a> {
    pub key: Key,
    pub kind: Kind,
    /// The BLAKE3 hash of the value's bytes as they were put, checked.
    pub content: ContentId,
    /// The value's bytes as they were put; from [`Store::get_in_place`], what its [`Heat`] says.
    pub bytes: &'a [u8],
}

/// Format's encodings of the values of one schema (`blaze_format::tiers`), by which
/// [`Store::settle`] moves a value between tiers. The store decides when, by how recently the
/// value was read; what its columns are and how each is packed is the encoding's. A value is of
/// a schema when it is a `blaze_format` buffer: its header names the schema.
///
/// Each direction must give back exactly what it was given; the store decodes every encoding
/// it makes and compares before keeping it.
enum Columns {
    /// What every schema the store is given has ([`Store::encode_columns`]).
    Buffer(BufferColumns),
    /// The tests' own, which can be made to give back other values than it was given.
    #[cfg(all(test, not(blaze_check)))]
    Test(tests::OneColumn),
}

impl Columns {
    /// The schema of the values it encodes.
    fn schema(&self) -> SchemaHash {
        match self {
            Columns::Buffer(it) => it.schema(),
            #[cfg(all(test, not(blaze_check)))]
            Columns::Test(it) => it.schema(),
        }
    }

    /// `hot` with its columns bit-packed by frame of reference: random access, read in place.
    fn warm(&self, hot: &[u8]) -> Result<Vec<u8>, String> {
        match self {
            // alloc: the message of an encoding that failed
            Columns::Buffer(it) => it.warm(hot).map_err(|why| why.to_string()),
            #[cfg(all(test, not(blaze_check)))]
            Columns::Test(it) => it.warm(hot),
        }
    }

    /// The value [`Columns::warm`] was given.
    fn hot_from_warm(&self, warm: &[u8]) -> Result<Vec<u8>, String> {
        match self {
            // alloc: the message of a decoding that failed
            Columns::Buffer(it) => it.hot_from_warm(warm).map_err(|why| why.to_string()),
            #[cfg(all(test, not(blaze_check)))]
            Columns::Test(it) => it.hot_from_warm(warm),
        }
    }

    /// `hot` with a codec per column, then zstd: decoded when read.
    fn cold(&self, hot: &[u8]) -> Result<Vec<u8>, String> {
        match self {
            // alloc: the message of an encoding that failed
            Columns::Buffer(it) => it.cold(hot).map_err(|why| why.to_string()),
            #[cfg(all(test, not(blaze_check)))]
            Columns::Test(it) => it.cold(hot),
        }
    }

    /// The value [`Columns::cold`] was given.
    fn hot_from_cold(&self, cold: &[u8]) -> Result<Vec<u8>, String> {
        match self {
            // alloc: the message of a decoding that failed
            Columns::Buffer(it) => it.hot_from_cold(cold).map_err(|why| why.to_string()),
            #[cfg(all(test, not(blaze_check)))]
            Columns::Test(it) => it.hot_from_cold(cold),
        }
    }
}

/// The encodings the store was given, by schema. Each behind an `Arc` so publishing a new map
/// copies no encoding (a [`blaze_format::tiers::ColdDictionary`] rebuilds its tables to clone).
type Encodings = ByHash<SchemaHash, Arc<Columns>, Global>; // alloc: the store's own table, for its life

/// The encodings of the schema `bytes` are a buffer of, when the store was given them.
fn encodings_of<'e>(encodings: &'e Encodings, bytes: &[u8]) -> Option<&'e Columns> {
    encodings.get(&header_schema(bytes)?).map(|it| &**it)
}

/// The form of the bytes [`Store::get_in_place`] gave.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Heat {
    /// The value's bytes as they were put.
    Hot,
    /// [`BufferColumns::warm`] of them, by the encoding of the value's schema.
    Warm,
}

/// A crate version, as the store numbers it: the records of one unit go in packs of its own,
/// one mapped file per crate version (#1298), so what a crate's build reads sits together and
/// a crate version's records can go as a whole. [`Store::unit`] gives one.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Unit(u32);

impl Unit {
    /// What no one crate version owns: chunks of outputs (shared by every output that holds
    /// them), dictionaries, and values put with [`Store::put`].
    pub const SHARED: Unit = Unit(0);
}

/// A put's place in line for durability: its writer's segment and the put's number in it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Ticket(pub u64);

impl Ticket {
    /// What a put of a value that is durable already gets.
    pub const DURABLE: Ticket = Ticket(0);

    const PUT_BITS: u32 = 40;

    pub(crate) fn of(segment: u32, put: u64) -> Ticket {
        assert!(put < 1 << Ticket::PUT_BITS, "a writer of 2^40 puts");
        Ticket((u64::from(segment) << Ticket::PUT_BITS) | put)
    }

    pub(crate) fn parts(self) -> (u32, u64) {
        ((self.0 >> Ticket::PUT_BITS) as u32, self.0 & ((1 << Ticket::PUT_BITS) - 1))
    }

    pub(crate) fn segment(self) -> u32 {
        self.parts().0
    }
}

/// The puts a waiter waits on to be durable: the last ticket of each segment they came from.
/// Tickets of one segment are durable in order, so its last stands for the rest; tickets of
/// different segments never compare, and a put of a value kept already gets the ticket of the
/// put that kept it, from any writer's segment. So one `max` of them all is no bound: this
/// keeps one per segment, and is durable once each is.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct Tickets(Vec<Ticket>); // alloc: one per segment waited on, at most one per writer

impl Tickets {
    /// [`Tickets::add`] of a put that may have made no ticket.
    pub fn add_some(&mut self, ticket: Option<Ticket>) {
        if let Some(ticket) = ticket {
            self.add(ticket);
        }
    }

    /// `ticket` waited on too; [`Ticket::DURABLE`] is nothing to wait on.
    pub fn add(&mut self, ticket: Ticket) {
        if ticket == Ticket::DURABLE {
            return;
        }
        let segment = ticket.segment();
        match self.0.iter_mut().find(|it| it.segment() == segment) {
            Some(last) => *last = (*last).max(ticket),
            None => self.0.push(ticket),
        }
    }

    /// Every ticket `other` waits on, waited on too.
    pub fn extend(&mut self, other: &Tickets) {
        for ticket in &other.0 {
            self.add(*ticket);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The last ticket of each segment, in the order their segments were first waited on.
    pub fn iter(&self) -> impl Iterator<Item = Ticket> + '_ {
        self.0.iter().copied()
    }

    /// The first of them that `durable` says is not durable yet: what a waiter names.
    pub fn pending(&self, durable: impl Fn(Ticket) -> bool) -> Option<Ticket> {
        self.0.iter().copied().find(|it| !durable(*it))
    }

    /// The durable ones let go: what is left to wait on.
    pub fn retain_pending(&mut self, durable: impl Fn(Ticket) -> bool) {
        self.0.retain(|it| !durable(*it));
    }
}

impl std::fmt::Display for Ticket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (segment, put) = self.parts();
        write!(f, "put {put} of segment {segment}")
    }
}

/// A value whose bytes are not what the store recorded.
#[derive(PartialEq, Eq, Debug)]
pub struct Corrupt {
    pub key: Key,
    pub pack: FileName,
    pub what: String,
}

impl fmt::Display for Corrupt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the value of {:?} in {} is corrupt: {}", self.key.0, self.pack, self.what)
    }
}

#[derive(PartialEq, Eq, Debug)]
pub struct Config {
    /// The store's directory, under the driver's root.
    pub dir: FileName,
    /// Read-only stores under it, searched after it: the shared base layer.
    pub layers: Vec<FileName>,
    /// Tags it asks with are `tags..tags + 2^32`.
    pub tags: u64,
}

/// Why a store stopped: it could not open, or its IO failed [`FAILURES_MOST`] times in a row.
/// What it has mapped it still serves; nothing it is given is made durable.
#[derive(PartialEq, Eq, Debug)]
pub struct Broken {
    pub file: FileName,
    pub what: String,
}

impl fmt::Display for Broken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the store cannot go on: {}: {}", self.file, self.what)
    }
}

/// Which of a store's directories a value [`Reader::get_from`] found is kept in.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Origin {
    /// Its own: put by it, by one of its writers, or verified from a [`Store::fetch`]. Of
    /// those, only its own puts make a [`Kind::Machine`]: a fetch refuses one.
    Own,
    /// A store behind its own, named: a read-only layer, or another writer of a shared
    /// directory.
    Behind(FileName),
}

/// How a [`Store::fetch`] from the remote tier ended.
#[derive(PartialEq, Eq, Debug)]
pub enum Fetched {
    /// Verified and kept, durable once its ticket is.
    Kept(Ticket),
    /// The remote has no value of the key.
    Missing,
    /// Not kept: the remote could not be reached, or sent what is not the value it named.
    Refused(String),
}

/// How a [`Store::place`] ended.
#[derive(PartialEq, Eq, Debug)]
pub struct Placed {
    /// The tag `place` returned.
    pub tag: Tag,
    pub to: FileName,
    pub result: Result<(), String>,
    /// How its bytes got there; all zero when it failed.
    pub bytes: PlacedBytes,
}

/// How the bytes of what [`Store::place`] placed got there: what a build reports, so a copy
/// where a clone was meant shows.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct PlacedBytes {
    /// Placed by sharing the store's blocks (`FICLONE`, `clonefile`): no new bytes on disk.
    pub cloned: u64,
    /// Written into the store, once, as the whole file later places clone ([`CLONE_FROM`]).
    pub written: u64,
    /// Written where they were placed from the store's bytes: a value too short to clone, or
    /// one not yet a file of its own.
    pub copied: u64,
}

impl std::ops::AddAssign for PlacedBytes {
    fn add_assign(&mut self, it: PlacedBytes) {
        self.cloned += it.cloned;
        self.written += it.written;
        self.copied += it.copied;
    }
}

/// A place waiting on the whole file it clones ([`PlacedFile`]).
struct Waiter {
    /// The tag [`Store::place`] returned, which its clone's first effect carries.
    asked: Tag,
    to: FileName,
    /// The key it was asked of, read when the file is to be written.
    key: Key,
    raw: u64,
    /// Whether it was put back once already, for the file being gone or no regular file at its
    /// clone ([`Store::rechecked`]) or its seal.
    retried: bool,
}

/// The whole file a clone is made from, and the place it is for ([`Op::Clear`]).
#[derive(Clone, Copy)]
struct Source {
    content: ContentId,
    key: Key,
    retried: bool,
}

/// A place's clone, made under a name of its own beside `to` and then renamed onto it, so that
/// a concurrent place of the same value at the same path is idempotent: two daemons with
/// different homes share output directories (the machine's std cache), and a place that removed
/// `to` and then cloned onto it lost to the other's clone landing in between (`File exists`).
/// A rename replaces `to` atomically on every driver (`renameat` on io_uring, `rename` on Unix
/// and WASI, `MoveFileExW` with `MOVEFILE_REPLACE_EXISTING` on Windows), so whichever place
/// renames last leaves `to` holding the same bytes. A rename that is refused all the same (on
/// Windows, a `to` that is read-only or open without sharing its delete) is a success only once
/// `to` is mapped and its bytes hash to `content` ([`Op::Verify`]), never otherwise.
///
/// A crash between the clone and the rename leaves the clone behind under its hidden name in
/// the output's directory. It is not swept: the store lists only its own directories
/// ([`Op::Swept`] is for `placed/`), never a build's outputs, and nothing reads a `.placing`
/// name. It shares its blocks with the store's file; a later place of the same store with the
/// same tag removes it before it clones ([`Op::Clear`]), and a clean of the output directory
/// removes it with the rest.
#[derive(Clone)]
struct Staged {
    /// [`Store::staging_name`].
    temp: FileName,
    /// The value's content: what `to` must hash to for a refused rename to be a success.
    content: ContentId,
}

/// The domain of the hash of a store's directory that its staged names carry
/// ([`Store::staging_name`]).
const STAGING_DOMAIN: &str = "blaze v2 store staging 1";
/// The most bytes of `to`'s own name a staged name keeps, so a long name stays under the file
/// system's limit with the store's hash and the tag added.
const STAGING_LEAF_MOST: usize = 160;

/// What [`Store::open`] found of the whole files outputs are cloned from.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct PlacedOnOpen {
    /// Files named for a content, kept: each is checked against its name before it is first
    /// cloned from.
    pub kept: u64,
    pub kept_bytes: u64,
    /// Anything else (an earlier run's temporary files), removed.
    pub swept: u64,
}

/// Where the whole file of an output placed by cloning is ([`CLONE_FROM`]), by content. Not in
/// the head: the file is named by its content and written whole with one [`Effect::Write`], so
/// one that is there is all there, and none is anything another value depends on.
enum PlacedFile {
    /// Asked whether it is there, a file of the value's length (after an open, which knows
    /// none of them).
    Probing(Vec<Waiter>),
    /// There at the value's length but not written by this run: its bytes hashed against its
    /// name before anything clones it.
    Checking(Vec<Waiter>),
    Writing(Vec<Waiter>),
    /// Written (`wrote`) or checked, being made read-only before anything clones it.
    Sealing {
        waiting: Vec<Waiter>,
        wrote: bool,
    },
    /// There and read-only: `cloning` clones of it are in flight, and it is removed once they
    /// end when `doomed` ([`Store::unplace`]) or `lost` (a clone found it gone); `after`, the
    /// places that wait to be made from it afresh once it is.
    There {
        cloning: u32,
        doomed: bool,
        lost: bool,
        after: Vec<Waiter>,
    },
    /// Being removed; what waits is written afresh once it is gone.
    Removing(Vec<Waiter>),
}

/// One store, as a state machine.
///
/// Only the daemon holds a store, and one daemon a directory (#686): it is the one writer of its
/// packs and its head, and every build's reads go through its one mapped index. Readers and
/// writers on other threads go through [`Store::reader`] and [`Store::writer`], with no lock.
pub struct Store {
    config: Config,
    /// Its own directory first, then the layers in order.
    tiers: Vec<Tier>,
    tags: Tags,
    broken: Option<Broken>,
    /// IO failures in a row.
    failures: u32,
    /// What it publishes for readers and writers.
    shared: Arc<Shared>,
    /// Its own writer (segment 0): its puts, and the records a settle moves.
    own: Own,
    /// Each writer's segment as far as it has been drained, by number.
    drains: Vec<Drain>,
    /// What was drained since the last commit began, for the next one.
    draining: Batch,
    /// Batches committed whose mappings have not all come, oldest first.
    pending: VecDeque<Batch>,
    /// Each key a drained record stands in for another's as the home of, while its move waits
    /// in `draining` or `pending`, with the place the move is from: a copy (`copy_home`), or
    /// a writer's record of the value another writer's record not yet drained holds the key
    /// for (`record_home`). The record there, drained after, is a duplicate and never written
    /// to a pack (`drain_record`). At most one a key: a key stays where it is until a move is
    /// made, and a second record standing in from there is a duplicate too.
    copy_homes: ByHash<Key, Seg, Global>, // alloc: the store's own table, for its life
    next_batch: u64,
    commit: Option<Commit>,
    /// A flush asked for while a commit was in flight.
    flush_wanted: bool,
    /// Steps of a commit that have answered (a truncate, a write, a sync, the head), ever:
    /// [`Store::commit_steps`].
    commit_steps: u64,
    /// A new index table was written and is not yet in use: its mapping has not come, or the
    /// records it holds are not all mapped yet.
    index_mapping: bool,
    /// The new index table, mapped, waiting for the batches up to the one named to be mapped.
    flip: Option<(Arc<Index>, u64)>,
    /// An index swap left the delta before it in `prev`, where a writer that loaded the view
    /// before the swap may still put; it is folded in once this grace has passed.
    migrating: Option<Grace>,
    /// What it unlinked, freed at task boundaries.
    limbo: Limbo,
    training: [Training; Kind::ALL.len()],
    problems: Vec<String>,
    fetching: ByHash<Key, Tag, Global>, // alloc: the store's own table, for its life
    fetched: Vec<(Key, Fetched)>,
    placed: Vec<Placed>,
    /// The whole files of outputs placed by cloning ([`CLONE_FROM`]), by content.
    files: ByHash<ContentId, PlacedFile, Global>, // alloc: the store's own table, for its life
    /// What the open found of `placed/`.
    placed_on_open: PlacedOnOpen,
    /// Whether the open's look through `placed/` is done; until it is, places that clone from
    /// a whole file wait in `held`, so no write of one is begun that the look would sweep, and
    /// the whole files let go of wait in `held_unplaces`, so no remove fails its listing.
    placed_looked: bool,
    held: Vec<(ContentId, Waiter)>,
    held_unplaces: Vec<ContentId>,
    /// Open while anything waits in `held` or `held_unplaces`, from the first held to the
    /// look's end or the store's break: the wait, named (every-wait-says-what).
    look_wait: Option<tracing::Span>,
    /// Times the look through `placed/` has failed: its own bound ([`FAILURES_MOST`]), which
    /// other IO succeeding does not reset.
    look_tries: u32,
    /// Keys the batches put into packs since [`Store::new_keys`] last took them; `None` until
    /// [`Store::watch_keys`].
    new_keys: Option<Vec<Key>>,
    /// Batches whose moves are made.
    epoch: u64,
    /// How many times the own tier's read flags have rotated since its rows were last
    /// numbered afresh: a record is unread for two periods only once there have been two.
    periods: u32,
    /// What the page cache holds of the own packs' records, as settles last asked.
    residency: Residency,
    /// The encodings of each schema whose values it keeps in columns.
    columns: Arc<Encodings>,
    /// The other writers of its directory's shared one ([`Store::open_shared`]).
    peers: Option<peers::Peers>,
    /// What tells this store's staged names from every other store's ([`Store::staging_name`]).
    staging: u64,
    /// A test's hook, run with a move's key when the move of `prev`'s spot found the delta
    /// lacking the key and is about to put its pack spot there (`Store::finish_batches`).
    #[cfg(test)]
    move_probed: Option<Box<dyn FnMut(Key) + Send>>,
    /// A planted bug for the place race's DST (`tests::place_race`): a clone made by removing
    /// `to` and cloning onto it, as before [`Staged`], which a concurrent place of the same
    /// value at the same path can lose to.
    #[cfg(test)]
    plant_clear_then_clone: bool,
}

/// A way to read a store from any thread, with no lock: [`Store::reader`].
#[derive(Clone)]
pub struct Reader {
    shared: Arc<Shared>,
}

impl Reader {
    /// The value of `key`, verified, as [`Store::get`] gives it; valid while `guard`'s task runs.
    pub fn get<'g>(&'g self, guard: &'g Guard, key: &Key) -> Result<Option<Entry<'g>>, Corrupt> {
        self.look(guard).get(key)
    }

    /// The value of `key` as [`Store::get_in_place`] gives it.
    pub fn get_in_place<'g>(
        &'g self,
        guard: &'g Guard,
        key: &Key,
    ) -> Result<Option<(Heat, Entry<'g>)>, Corrupt> {
        self.look(guard).get_in_place(key)
    }

    /// [`Reader::get`], with the directory the value is kept in: what a reader that must
    /// not take a value from elsewhere (machine code it runs) checks before it uses it.
    pub fn get_from<'g>(
        &'g self,
        guard: &'g Guard,
        key: &Key,
    ) -> Result<Option<(Origin, Entry<'g>)>, Corrupt> {
        self.look(guard).get_from(key)
    }

    /// The value of `key`, verified as [`Reader::get`] verifies it, held past the task:
    /// in place, at a 16-byte boundary, when it is a raw record of a mapped pack or a file of
    /// its own, else one aligned copy ([`Pinned`]).
    pub fn pinned(&self, guard: &Guard, key: &Key) -> Result<Option<Pinned>, Corrupt> {
        self.look(guard).pinned(key)
    }

    /// Whether it has a value of `key`, unverified: for planning, never for use.
    pub fn has(&self, guard: &Guard, key: &Key) -> bool {
        self.look(guard).has(key)
    }

    /// Whether the put that got `ticket` is durable.
    pub fn durable(&self, ticket: Ticket) -> bool {
        self.shared.durable(ticket)
    }

    fn look<'g>(&'g self, guard: &'g Guard) -> Look<'g> {
        Look::new(&self.shared, Held::task(guard))
    }
}

/// A writer's segment, drained up to `at` of `chunk`.
struct Drain {
    segment: Arc<Segment>,
    chunk: ChunkRef,
    at: u64,
    /// Puts that ended in the chunks before `chunk`.
    puts: u64,
}

impl Drain {
    fn new(segment: Arc<Segment>) -> Drain {
        Drain { chunk: segment.first(), segment, at: 0, puts: 0 }
    }
}

/// What one commit copied out of the segments.
#[derive(Default)]
struct Batch {
    id: u64,
    moves: Vec<Move>,
    /// Chunks drained to their end, with their segment's number, oldest first: freed once the
    /// moves are made.
    chunks: Vec<(usize, ChunkRef)>,
    /// Each segment's puts drained in all, by number.
    puts: Vec<u64>,
    /// Mappings still to come before the moves are made.
    maps: usize,
}

/// A record copied from a segment into a pack, its key to be pointed at the pack.
#[derive(Clone, Copy)]
struct Move {
    key: Key,
    from: Seg,
    to: Loc,
}

impl Move {
    /// Whether this move points `spot` at its pack: the spot is still where the move's record
    /// was drained from. The one rule of which moves are made: `finish_batches` makes each move
    /// that takes its key's spot, oldest batch first and in drain order, so of a key's moves
    /// from one place the first is made and the rest find the key moved, and a move from a
    /// place the key left is made nowhere. `index_entries` names the record of the move this
    /// picks among the waiting ones, so an index never leaves out or misnames a key whose
    /// moves wait.
    fn takes(&self, spot: &Spot) -> bool {
        spot.key == self.key && spot.place == Place::Segment(self.from)
    }

    /// Makes this move in `table` if it takes the table's spot of its key: gives the spot's row
    /// and the spot swapped out, which stays readable until a collect after its grace.
    fn make_in<'a>(&self, table: &'a Table<Spot>, held: Held<'a>) -> Option<(u64, &'a Spot)> {
        let (row, spot) = table.get(held, &self.key)?;
        if !self.takes(spot) {
            return None;
        }
        _ = table.replace(held, spot.at(Place::Pack(self.to)));
        Some((row, spot))
    }
}

/// A move `finish_batches` could not make whole, which stops the store: its batch's chunks
/// stay linked, and no later batch is taken.
enum Lost {
    /// Into a pack a settle retired: `retirable` leaves out every pack a waiting batch moves
    /// records into, so this is that rule broken. The move is not made (its key would point
    /// into a removed pack).
    Retired { batch: u64, key: Key, to: Loc },
    /// Of `prev`'s spot during an index swap, while the delta held the key at another place
    /// than the record moved: the delta holds a key only where `prev` does (`Own::claim`).
    Apart { batch: u64, key: Key, from: Seg, to: Loc, place: Place, content: ContentId },
}

/// What the page cache holds of the own packs' records ([`Effect::Residency`]), which a settle
/// reads alongside the read flags: a record whose every page is cached is warm whatever its
/// flags say. A settle asks of the records it may move and uses what the last whole round
/// answered; it never waits for one (a round still out is not asked again, and its answers are
/// used once they are all in). Asked only where the system can say it
/// ([`blaze_core::RESIDENCY`]): elsewhere every record's is unknown, and the flags alone decide.
struct Residency {
    /// Whether settles ask.
    asks: bool,
    /// By pack and offset, whether every page of the record there was cached, as the last
    /// whole round said.
    known: HashMap<(u32, u64), bool, Global>, // alloc: the store's own table, for its life
    /// The answers of the round out so far, which become `known` once it is all in.
    answering: HashMap<(u32, u64), bool, Global>, // alloc: the store's own table, for its life
    /// The round out, or the last.
    round: u64,
    /// Its effects not yet answered.
    waiting: usize,
}

impl Residency {
    fn new() -> Residency {
        Residency {
            asks: blaze_core::RESIDENCY,
            known: HashMap::new_in(Global), // alloc: the store's own table, for its life
            answering: HashMap::new_in(Global), // alloc: the store's own table, for its life
            round: 0,
            waiting: 0,
        }
    }

    /// Whether the record at `loc` was resident, where the last round said.
    fn of(&self, loc: &Loc) -> Option<bool> {
        self.known.get(&(loc.pack, loc.offset)).copied()
    }
}

/// Records a settle moves: each key, where its record is and its row.
type Moves = Vec<(Key, Loc, Row)>;

/// What one [`Store::settle`] did.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Settled {
    /// Records moved a tier colder (to warm or cold columns, or to zstd): nothing read them for
    /// two periods.
    pub demoted: u64,
    /// Records moved to the hot tier: read in two periods running.
    pub promoted: u64,
    /// Packs whose live records were copied on; each is removed once that is durable.
    pub retired: u64,
    /// It stopped at its budget, or could not start while a commit was in flight: call again.
    pub more: bool,
}

/// Why a record's bytes are wanted, which decides whether its check is remembered and
/// whether it counts as read.
#[derive(Clone, Copy)]
pub(crate) enum Use {
    Read,
    Check,
    Move,
}

/// The tags a store asks with, and what each asked for.
struct Tags {
    range: blaze_core::Tags,
    ops: HashMap<Tag, Op, Global>, // alloc: the store's own table, for its life
}

impl Tags {
    fn fresh(&mut self) -> Tag {
        self.range.next()
    }

    fn ask(&mut self, op: Op) -> Tag {
        let tag = self.fresh();
        self.ops.insert(tag, op);
        tag
    }
}

/// What a fetch keeps ([`Store::fetch`]): `Ok` for a value of this kind and bytes
/// that may be kept, else why not.
pub type Admit = fn(Kind, &[u8]) -> Result<(), String>;

enum Op {
    /// The store's directory checked to be the user's alone, before anything in it is read.
    Private,
    Head {
        tier: usize,
    },
    Map {
        tier: usize,
        file: FileName,
        what: Mapped,
    },
    Truncate {
        pack: usize,
    },
    WritePack {
        pack: usize,
    },
    Sync {
        pack: usize,
    },
    WriteExternal {
        content: ContentId,
    },
    WriteIndex,
    WriteHead,
    /// A pack whose live records were copied on, removed once the head no longer names it.
    RemovePack {
        pack: usize,
    },
    /// `staged`'s name removed (a clone an earlier run of this store left there under the same
    /// tag) so `from` can be cloned there, to be renamed onto `to`; `of`, the content of the
    /// whole file of [`PlacedFile`] it clones.
    Clear {
        asked: Tag,
        from: FileName,
        to: FileName,
        staged: Staged,
        of: Option<Source>,
        bytes: PlacedBytes,
    },
    /// The clone into `staged`'s name, or (`staged` none) the write of `to` itself, which
    /// [`Effect::Write`] makes beside it and renames over it already.
    Place {
        asked: Tag,
        to: FileName,
        staged: Option<Staged>,
        of: Option<Source>,
        bytes: PlacedBytes,
    },
    /// `staged`'s clone renamed onto `to`.
    Commit {
        asked: Tag,
        to: FileName,
        staged: Staged,
        bytes: PlacedBytes,
    },
    /// `to` mapped to be hashed, after the rename of `staged` onto it failed with `why`.
    Verify {
        asked: Tag,
        to: FileName,
        staged: Staged,
        bytes: PlacedBytes,
        why: String,
    },
    /// A staged clone not renamed onto its path, removed.
    Discard {
        file: FileName,
    },
    /// Whether the whole file of `content` is there ([`PlacedFile::Probing`]).
    Probe {
        content: ContentId,
    },
    /// The whole file of `content` mapped to be hashed ([`PlacedFile::Checking`]).
    Check {
        content: ContentId,
    },
    /// Whether the whole file of `content` is still there, after a clone of it for `waiter`
    /// failed with `why`.
    Recheck {
        content: ContentId,
        waiter: Waiter,
        why: String,
    },
    /// Whether the directory of whole files is there, as the store opens.
    LookPlaced,
    /// The directory of whole files listed as the store opens.
    ListPlaced,
    /// A temporary file of it removed.
    Swept {
        file: FileName,
    },
    /// The whole file of `content` written ([`PlacedFile::Writing`]).
    MakeFile {
        content: ContentId,
    },
    /// The whole file of `content` made read-only ([`PlacedFile::Sealing`]).
    Seal {
        content: ContentId,
    },
    /// The whole file of `content` removed ([`PlacedFile::Removing`]).
    Unplace {
        content: ContentId,
    },
    /// A fetch of `key`, whose value is kept only once `admit` takes it ([`Store::fetch`]).
    Fetch {
        key: Key,
        admit: Admit,
    },
    /// The page-cache residency of the records at `offsets` of own pack `pack`, of the
    /// settles' round `round` ([`Residency`]).
    Residency {
        round: u64,
        file: FileName,
        pack: u32,
        offsets: Vec<u64>,
    },
    /// The shared directory listed, for the other writers' stores in it.
    Peers,
    /// Its own empty head written, so the other writers find a store in its directory.
    Announce,
}

#[derive(Clone, Copy, Debug)]
enum Mapped {
    /// `covers` is `None` for the table the store opened with, else the last batch it holds.
    Index {
        covers: Option<u64>,
    },
    /// `usable` is how much of the file is the store's as of the commit that asked; `batch`,
    /// the batch of moves waiting for it.
    Pack {
        pack: usize,
        usable: u64,
        batch: Option<u64>,
    },
    External {
        content: ContentId,
        batch: Option<u64>,
    },
}

impl Mapped {
    fn usable(&self) -> u64 {
        match self {
            Mapped::Pack { usable, .. } => *usable,
            _ => 0,
        }
    }
}

/// A store's directory as it was read and as it is being written.
struct Tier {
    dir: FileName,
    /// The head as last read or durably written.
    head: Head,
    head_read: bool,
    opened: bool,
    /// Mappings of the opening still to come.
    waiting: usize,
    /// The index table, mapped and checked.
    index: Option<Arc<Index>>,
    packs: Vec<Pack>,
    /// Records the index table does not hold: read from the packs' tails at opening, or put
    /// since.
    delta: Arc<Table<Spot>>,
    /// The delta an index swap left, while a writer may still put into it.
    prev: Option<Arc<Table<Spot>>>,
    externals: ByHash<ContentId, External, Global>, // alloc: the store's own table, for its life
    /// By number, which a compressed record names.
    dictionaries: Vec<Dictionary>,
    /// Each unit's crate version by its number less one, and the number of each.
    units: Vec<ContentId>,
    unit_of: ByHash<ContentId, Unit, Global>, // alloc: the store's own table, for its life
    /// Another writer's store as it was last open, which readers read while it is opened again
    /// as its head now says.
    before: Option<Box<Tier>>,
}

impl Tier {
    fn new(dir: FileName) -> Tier {
        Tier {
            dir,
            head: Head::default(),
            head_read: false,
            opened: false,
            waiting: 0,
            index: None,
            // alloc: empty, allocates nothing
            packs: Vec::new(),
            // alloc: once per tier, at opening
            delta: Arc::new(Table::new(DELTA_FIRST, FLAGS)),
            prev: None,
            externals: ByHash::new_in(Global), // alloc: the store's own table, for its life
            // alloc: empty, allocates nothing
            dictionaries: Vec::new(),
            // alloc: empty, allocates nothing
            units: Vec::new(),
            unit_of: ByHash::new_in(Global), // alloc: the store's own table, for its life
            before: None,
        }
    }

    fn file(&self, name: &str) -> FileName {
        // alloc: a file name for an effect, which owns it; once per file asked for, never per get or put
        FileName::new(&format!("{}/{name}", self.dir))
            .expect("a store's file names stay in its directory")
    }

    fn index_file(&self) -> FileName {
        self.file(&disk::index_file(self.head.index.map_or(0, |it| it.slot)))
    }

    /// It as readers see it. Made once a published view (an opening, a commit's mappings, an
    /// index swap, a dictionary or encoding given), never per get or put; readers share it.
    fn view(&self) -> TierView {
        if let Some(before) = &self.before {
            return before.view();
        }
        TierView {
            // clone: the published view owns its names; once per view
            dir: self.dir.clone(),
            index_file: self.index_file(),
            // clone: an `Arc` per tier, once per view
            index: self.index.clone(),
            // clone: an `Arc` per tier, once per view
            delta: self.delta.clone(),
            // clone: an `Arc` per tier, once per view
            prev: self.prev.clone(),
            packs: self
                .packs
                .iter()
                // clone: the published view owns its names and mappings; once per pack a view
                .map(|it| PackView {
                    file: it.file.clone(),
                    mapping: it.mapping.clone(),
                    usable: it.usable,
                })
                // alloc: the view's packs, exactly sized; once per view
                .collect(),
            externals: self
                .externals
                .iter()
                // clone: the published view owns its names and mappings; once per external a view
                .map(|(content, it)| {
                    (*content, ExternalView { file: it.file.clone(), mapping: it.mapping.clone() })
                })
                // alloc: the view's externals; once per view
                .collect(),
            // clone: an `Arc` (or a dictionary's error) per dictionary, into the view's list; once per view
            dictionaries: self.dictionaries.iter().map(|it| (it.kind, it.bytes.clone())).collect(),
        }
    }
}

struct Pack {
    /// Its [`Unit`]'s number.
    unit: u32,
    file: FileName,
    mapping: Option<Mapping>,
    /// How much of `mapping` is the store's.
    usable: u64,
    /// The bytes from `usable` to `end`, held until a mapping covers them. Only the store
    /// reads them: a record not yet mapped is read from its segment.
    memory: Vec<u8>,
    /// What the head on disk says is the store's.
    committed: u64,
    indexed: u64,
    end: u64,
    dead: u64,
    removed: bool,
    /// Its live records are copied on; the next head names it removed.
    retiring: bool,
}

impl Pack {
    fn bytes(&self, at: u64, len: u64) -> Option<&[u8]> {
        let end = at.checked_add(len)?;
        if end <= self.usable {
            let bytes = self.mapping.as_ref()?.bytes();
            bytes.get(usize::try_from(at).ok()?..usize::try_from(end).ok()?)
        } else if at >= self.usable && end <= self.end {
            self.memory.get((at - self.usable) as usize..(end - self.usable) as usize)
        } else {
            None
        }
    }
}

struct External {
    len: u64,
    file: FileName,
    mapping: Option<Mapping>,
    /// Held until it is written and mapped.
    memory: Option<Vec<u8>>,
    committed: bool,
}

struct Dictionary {
    kind: Kind,
    pack: u32,
    offset: u64,
    bytes: Result<Arc<cold::Prepared>, String>,
}

/// The raw values of a cold kind kept so far, to train its dictionary on.
#[derive(Default)]
struct Training {
    /// Distinct values: a value is sampled once however often it is put or a settle meets its
    /// record, so what a dictionary is trained on does not depend on how many settles ran.
    samples: Vec<Vec<u8>>,
    /// The content of each sample.
    sampled: ByHashSet<ContentId, Global>, // alloc: at most `SAMPLES_MOST` a kind, freed once it trains or gives up
    bytes: u64,
    /// No training before this many samples: doubled after one fails.
    after: usize,
    /// No training can be had while the store is open: the samples are full and did not train
    /// ([`Store::give_up_training`], in the problems). Nothing more is sampled.
    given_up: bool,
}

impl Training {
    /// Whether to train on the samples kept: [`cold::TRAIN_VALUES`] distinct values (or twice
    /// as many as a training that failed had) and [`cold::TRAIN_BYTES`] of them, or as many
    /// samples as are ever kept ([`cold::SAMPLES_MOST`]). Without the last, a kind whose
    /// values are shorter than `TRAIN_BYTES / SAMPLES_MOST` on average (debug info, a few
    /// hundred bytes a function) never reaches `TRAIN_BYTES`, as no sample is kept past
    /// `SAMPLES_MOST`, and is never trained nor compressed.
    fn ready(&self) -> bool {
        self.samples.len() >= cold::TRAIN_VALUES.max(self.after)
            && (self.bytes >= cold::TRAIN_BYTES || self.full())
    }

    /// Whether no more samples are kept: [`cold::SAMPLES_MOST`] of them, or
    /// [`cold::SAMPLE_BYTES_MOST`] bytes.
    fn full(&self) -> bool {
        self.samples.len() >= cold::SAMPLES_MOST || self.bytes >= cold::SAMPLE_BYTES_MOST
    }

    /// Whether a new value would be kept as a sample.
    fn wants_samples(&self) -> bool {
        !self.given_up && !self.full()
    }
}

/// One group commit: each pack cut to what is committed, appended to, synced; new externals
/// and a new index table written; then the head.
struct Commit {
    /// Each pack written and the end it is written to.
    packs: Vec<(usize, u64)>,
    externals: Vec<ContentId>,
    /// The slot of the index table it wrote, and the last batch that table holds.
    index: Option<(u8, u64)>,
    head: Head,
    /// What it copied out of the segments: durable with it.
    batch: Batch,
    steps: usize,
    failed: Option<Broken>,
    head_sent: bool,
}

impl Commit {
    fn to(&self, pack: usize) -> Option<u64> {
        self.packs.iter().find(|(it, _)| *it == pack).map(|(_, to)| *to)
    }
}

/// The schema a `blaze_format` buffer's header names; `None` for bytes that are not one.
fn header_schema(bytes: &[u8]) -> Option<SchemaHash> {
    let magic = bytes.get(..8)?;
    let schema = bytes.get(8..8 + SCHEMA_PREFIX)?;
    (magic == blaze_format::MAGIC)
        .then(|| SchemaHash(ContentId(schema.try_into().expect("32 bytes"))))
}

/// `encoded` behind the schema of the encodings that made it: a columns record's payload.
fn columns_payload(columns: &Columns, encoded: &[u8]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(SCHEMA_PREFIX + encoded.len());
    payload.extend_from_slice(&columns.schema().0.0);
    payload.extend_from_slice(encoded);
    payload
}

/// The cold columns payload of `hot`, decoded back and compared.
fn cold_columns(columns: &Columns, hot: &[u8]) -> Result<Vec<u8>, String> {
    let payload = columns_payload(columns, &columns.cold(hot)?);
    if columns.hot_from_cold(&payload[SCHEMA_PREFIX..])? != hot {
        // alloc: the message of an encoding that does not decode back
        return Err(format!(
            "the cold columns of schema {:x} do not decode to the value",
            columns.schema().0
        ));
    }
    Ok(payload)
}

/// The next colder columns of `hot`, kept at `loc`: warm from hot (or cold, where warm saves
/// less than an eighth), cold from warm; `None` when that saves less than an eighth too.
fn colder_columns(
    columns: &Columns,
    loc: Loc,
    hot: &[u8],
) -> Result<Option<(Form, Vec<u8>)>, String> {
    let saves = |payload: &[u8]| payload.len() as u64 + loc.stored / 8 <= loc.stored;
    if loc.form != Form::Warm {
        let payload = columns_payload(columns, &columns.warm(hot)?);
        if columns.hot_from_warm(&payload[SCHEMA_PREFIX..])? != hot {
            // alloc: the message of an encoding that does not decode back
            return Err(format!(
                "the warm columns of schema {:x} do not decode to the value",
                columns.schema().0
            ));
        }
        if saves(&payload) {
            return Ok(Some((Form::Warm, payload)));
        }
    }
    let payload = cold_columns(columns, hot)?;
    Ok(saves(&payload).then_some((Form::ColdColumns, payload)))
}

/// A record's place in the store, as one number.
fn location(tier: usize, pack: u32, offset: u64) -> u64 {
    ((tier as u64) << 56) | (u64::from(pack) << 40) | offset
}

/// Why an effect failed, as a message: only ever for an error.
/// Whether a value of `header` kept as `form` is placed by cloning the whole file of it the
/// store keeps ([`CLONE_FROM`]): an output long enough, unless it is a file of its own already.
fn clones_from_file(header: &RecordHeader, form: Form) -> bool {
    header.kind == Kind::Output && header.raw >= CLONE_FROM && form != Form::External
}

fn describe(done: &Done) -> String {
    match done {
        // clone: the message of an error
        Done::Failed(error) => error.0.clone(),
        Done::Read(_) => "it was answered with a read".into(),
        Done::Mapped(_) => "it was answered with a mapping".into(),
        _ => "it was answered as another kind of effect".into(),
    }
}

impl Store {
    /// A store that is opening: it asks to map its index and packs.
    pub fn open(config: Config, effects: &mut Vec<Effect>) -> Store {
        let mut store = Store::new(config, None);
        store.ask_private(effects);
        store
    }

    fn new(config: Config, peers: Option<peers::Peers>) -> Store {
        // alloc: setup, once per store
        let mut tiers = vec![Tier::new(config.dir.clone())];
        tiers.extend(config.layers.iter().cloned().map(Tier::new));
        let columns = Arc::<Encodings>::default();
        // clone: setup, once per store
        let view = View { tiers: tiers.iter().map(Tier::view).collect(), columns: columns.clone() };
        // alloc: setup, once per store
        let shared = Arc::new(Shared::new(view));
        let own = Own::new(0);
        // clone: setup, the store's own segment's `Arc`, once per store
        shared.add_segment(own.segment().clone());
        // clone: setup, once per store
        let drains = vec![Drain::new(own.segment().clone())];
        // One daemon a directory (#686), so the hash of the store's own directory, as the driver
        // names it, tells its staged names from those of any other daemon's store.
        let staging = ContentId::of(STAGING_DOMAIN, config.dir.as_str().as_bytes());
        let staging = u64::from_le_bytes(staging.0[..8].try_into().expect("8 bytes"));
        Store {
            tags: Tags { range: blaze_core::Tags::new(config.tags), ops: HashMap::new_in(Global) }, // alloc: the store's own table, for its life
            config,
            tiers,
            broken: None,
            failures: 0,
            shared,
            own,
            drains,
            draining: Batch::default(),
            pending: VecDeque::new(),
            copy_homes: ByHash::new_in(Global), // alloc: the store's own table, for its life
            next_batch: 0,
            commit: None,
            flush_wanted: false,
            commit_steps: 0,
            index_mapping: false,
            flip: None,
            migrating: None,
            limbo: Limbo::new(),
            training: Default::default(),
            // alloc: empty, allocates nothing
            problems: Vec::new(),
            fetching: ByHash::new_in(Global), // alloc: the store's own table, for its life
            // alloc: empty, allocates nothing
            fetched: Vec::new(),
            // alloc: empty, allocates nothing
            placed: Vec::new(),
            files: ByHash::new_in(Global), // alloc: the store's own table, for its life
            placed_on_open: PlacedOnOpen::default(),
            placed_looked: false,
            held: Vec::new(),          // alloc: empty, allocates nothing
            held_unplaces: Vec::new(), // alloc: empty, allocates nothing
            look_wait: None,
            look_tries: 0,
            new_keys: None,
            epoch: 0,
            periods: 0,
            residency: Residency::new(),
            columns,
            peers,
            staging,
            #[cfg(test)]
            move_probed: None,
            #[cfg(test)]
            plant_clear_then_clone: false,
        }
    }

    /// Asks that the store's directory be made the user's alone, and checked to be with all in
    /// it: it keeps machine code that is run, and a value is checked against its own hash,
    /// never against what its key says made it, so whoever else may write there could plant
    /// code under any key. A layer is read-only and may be shared, so is not asked of. A
    /// shared store asks it of the shared directory, every other writer's store in it.
    fn ask_private(&mut self, effects: &mut Vec<Effect>) {
        let tag = self.tags.ask(Op::Private);
        // clone: setup, once per open
        effects.push(Effect::Private { dir: self.private_dir().clone(), tag });
    }

    fn read_heads(&mut self, effects: &mut Vec<Effect>) {
        for tier in 0..self.tiers.len() {
            let file = self.tiers[tier].file(HEAD_FILE);
            let tag = self.tags.ask(Op::Head { tier });
            effects.push(Effect::ReadAll { file, tag });
        }
    }

    /// Whether it has mapped what it needs to answer gets.
    pub fn ready(&self) -> bool {
        self.broken.is_none()
            && self.peers_ready().unwrap_or_else(|| self.tiers.iter().all(|it| it.opened))
    }

    /// Why it stopped, if it has.
    pub fn broken(&self) -> Option<&Broken> {
        self.broken.as_ref()
    }

    /// The last put of every segment that is not durable yet: its own writer's ([`Store::put`]
    /// and the rest on the store's own thread, segment 0) and every [`Writer`]'s (a
    /// [`crate::Handle`]'s too), each counted before this call. A put is counted before any key
    /// of it is readable, so every value a reader could have read by then is covered. Once all
    /// are durable, so is every put made by then, as a segment's puts turn durable in order.
    /// What a wait for "everything put so far" waits on, so no put is missed for want of a
    /// caller handing its ticket over, on any thread. A put counted mid-way is not yet
    /// published to the drain: the wait flushes again until a commit takes it (the daemon's
    /// gate does, `flush_for_wait` in `serve/job.rs`).
    pub fn last_puts(&self) -> Tickets {
        let mut tickets = Tickets::default();
        for drain in &self.drains {
            tickets.add_some(drain.segment.last_pending());
        }
        tickets
    }

    /// Whether a commit is in flight: a [`Store::flush`] now is taken at its end.
    pub fn committing(&self) -> bool {
        self.commit.is_some()
    }

    /// How many steps of its commits have answered, ever: it moves only when the commit that
    /// makes puts durable moves (a truncate, a pack write, a sync, the head), never on a read,
    /// a mapping or a placement. A wait for durability that sees it stand still is a commit
    /// that stopped; one that sees it move is a commit still going.
    pub fn commit_steps(&self) -> u64 {
        self.commit_steps
    }

    /// What it could not do and went on without: a dictionary it could not train.
    pub fn problems(&self) -> &[String] {
        &self.problems
    }

    /// Whether the put that got `ticket` is durable.
    pub fn durable(&self, ticket: Ticket) -> bool {
        self.shared.durable(ticket)
    }

    /// A reader of it for any thread: what it reads is valid while the task it reads in runs.
    pub fn reader(&self) -> Reader {
        // clone: one `Arc` per reader made, once per thread that reads; its gets clone nothing
        Reader { shared: self.shared.clone() }
    }

    /// A writer of it for one worker: its puts go into a segment of its own, read at once by
    /// every reader and made durable by the store's next commit.
    pub fn writer(&mut self) -> Writer {
        assert!(self.tiers[0].opened, "a writer of a store that is not open");
        let own = Own::new(self.drains.len() as u32);
        // clone: setup, the segment's `Arc`, once per writer made
        self.shared.add_segment(own.segment().clone());
        // clone: setup, the segment's `Arc`, once per writer made
        self.drains.push(Drain::new(own.segment().clone()));
        // clone: setup, once per writer made; its puts clone nothing
        Writer { shared: self.shared.clone(), own }
    }

    /// The value of `key`, verified; `None` when it has none.
    pub fn get(&self, key: &Key) -> Result<Option<Entry<'_>>, Corrupt> {
        self.look().get(key)
    }

    /// The value of `key` as [`Store::get`] gives it, except that a value kept in warm columns
    /// is given in them, read in place ([`Heat::Warm`]), for readers of its schema's warm form.
    /// Checked as `get` checks: decoded and hashed once a record.
    pub fn get_in_place(&self, key: &Key) -> Result<Option<(Heat, Entry<'_>)>, Corrupt> {
        self.look().get_in_place(key)
    }

    /// The value of `key` as [`Reader::pinned`] gives it.
    pub fn pinned(&self, key: &Key) -> Result<Option<Pinned>, Corrupt> {
        self.look().pinned(key)
    }

    /// Gives it the encodings of one schema's values: [`Store::settle`] moves values of the
    /// schema through warm and cold columns, and a value kept in them is read through these.
    /// Until it is given them, a value of the schema kept in columns is refused, naming it.
    pub fn encode_columns(&mut self, columns: BufferColumns) {
        self.add_columns(Columns::Buffer(columns));
    }

    fn add_columns(&mut self, columns: Columns) {
        let schema = columns.schema();
        // alloc: setup, once per schema the store is given.
        let old = Arc::make_mut(&mut self.columns).insert(schema, Arc::new(columns));
        assert!(old.is_none(), "two encodings of schema {:x}", schema.0);
        self.publish();
    }

    /// Whether it has a value of `key`, unverified: for planning, never for use.
    pub fn has(&self, key: &Key) -> bool {
        self.look().has(key)
    }

    /// Its own reads: what they find is valid while it is borrowed, since only its `&mut`
    /// methods free anything.
    fn look(&self) -> Look<'_> {
        Look::new(&self.shared, Held::writer(&*self.shared))
    }

    /// The unit of `crate_version` (the content ID of a crate's name, version and source, as
    /// the caller names crate versions), numbered the first time it is asked for and kept in
    /// the head from the next commit.
    pub fn unit(&mut self, crate_version: ContentId) -> Unit {
        assert!(crate_version != ContentId::default(), "a crate version of no crate");
        let own = &mut self.tiers[0];
        let next = Unit(own.units.len() as u32 + 1);
        *own.unit_of.entry(crate_version).or_insert_with(|| {
            own.units.push(crate_version);
            next
        })
    }

    /// [`Store::put_in`] of [`Unit::SHARED`]: for values of no one crate version.
    pub fn put(&mut self, key: Key, kind: Kind, bytes: &[u8]) -> Result<Ticket, Mismatch> {
        self.put_in(Unit::SHARED, key, kind, bytes)
    }

    /// Keeps `bytes` under `key` in the packs of `unit`; readable at once, durable once
    /// [`Store::complete`] says its ticket is. A second put of a key keeps the first value
    /// wherever it is, and a put whose bytes differ from the value kept is an error the caller
    /// sees (a key that misses an input).
    pub fn put_in(
        &mut self,
        unit: Unit,
        key: Key,
        kind: Kind,
        bytes: &[u8],
    ) -> Result<Ticket, Mismatch> {
        let content = content_of(bytes);
        self.put_hashed(unit, key, kind, content, bytes)
    }

    /// [`Store::put_in`] of `bytes` whose hash the caller has: `content`.
    fn put_hashed(
        &mut self,
        unit: Unit,
        key: Key,
        kind: Kind,
        content: ContentId,
        bytes: &[u8],
    ) -> Result<Ticket, Mismatch> {
        assert!(
            unit.0 as usize <= self.tiers[0].units.len(),
            "{unit:?} is not one this store gave"
        );
        assert!(self.tiers[0].opened, "a put into a store that is not open");
        self.tidy();
        let look = self.look();
        let sample = kind.cold()
            && bytes.len() < cold::LONG_FROM
            && look.kept(&key).is_none()
            && look.dictionary_of(kind).is_none()
            && look.columns_of(bytes).is_none();
        if sample {
            self.sample(kind, content, bytes);
        }
        let look = Look::new(&self.shared, Held::writer(&*self.shared));
        let put = self.own.put(look, unit, key, kind, content, bytes);
        if let Some(why) = self.own.segment().failed() {
            // clone: the error that breaks the store
            let (dir, why) = (self.tiers[0].dir.clone(), why.to_owned());
            self.fail(dir, why);
        }
        put
    }

    /// Asks for what puts made since the last flush (by it and by every writer) need to be
    /// durable.
    pub fn flush(&mut self, effects: &mut Vec<Effect>) {
        assert!(self.tiers[0].opened, "a flush of a store that is not open");
        self.tidy();
        if self.broken.is_some() {
            return;
        }
        if self.commit.is_some() {
            self.flush_wanted = true;
        } else {
            self.start_commit(effects);
        }
    }

    /// Moves records between tiers by how recently they were read, copies the live records of
    /// packs that are half replaced bytes into new packs, and commits as [`Store::flush`] does.
    ///
    /// Each call ends a period of reading. A record unread for two periods running moves a tier
    /// colder: a value of a schema it was given encodings for ([`Store::encode_columns`]) goes
    /// from hot to warm columns (or straight to cold where warm saves too little) and from warm
    /// to cold columns; any other goes from raw or LZ4 to zstd. A record read in two periods
    /// running moves hot.
    ///
    /// Where the system says what is in the page cache ([`blaze_core::RESIDENCY`]), a settle
    /// also asks it of the records it may move ([`Effect::Residency`]) and the next uses the
    /// answers: a record whose pages are all cached stays where it is however long it went
    /// unread, and comes hot when it was read in one of the two periods; of a kind, the
    /// records known to be out of the cache go colder first. It never waits for an answer.
    /// The daemon
    /// calls it when idle, never on the way of a get or a put, and it moves a bounded number of
    /// bytes a call.
    pub fn settle(&mut self, effects: &mut Vec<Effect>) -> Settled {
        assert!(self.tiers[0].opened, "a settle of a store that is not open");
        self.tidy();
        let mut settled = Settled::default();
        if self.broken.is_some() {
            return settled;
        }
        if self.commit.is_some() || self.index_mapping || self.migrating.is_some() {
            settled.more = true;
            return settled;
        }
        let mut spent: u64 = 0;
        // What short cold records took of `spent`, and whether some were left for the next.
        let (mut spent_short, mut more_short) = (0u64, false);
        let encodings = Arc::clone(&self.columns);
        let (mut demote, mut promote, asks) = self.tier_moves();
        let residency = &self.residency;
        radix::sort_by_key(&mut demote, |(_, loc, _)| {
            // Of a kind, the records known to be out of the page cache first, then those whose
            // residency is unknown.
            let unknown = residency.of(loc).is_none();
            (u128::from(loc.kind.code()) << 96)
                | (u128::from(unknown) << 95)
                | radix::pack_64(u64::from(loc.pack), loc.offset)
        });
        radix::sort_by_key(&mut promote, |(_, loc, _)| {
            radix::pack_64(u64::from(loc.pack), loc.offset)
        });
        'kinds: for group in demote.chunk_by(|a, b| a.1.kind == b.1.kind) {
            let kind = group[0].1.kind;
            if self.dictionary_of(kind).is_none() {
                // Long records are not sampled, as `put` does not sample them: a few would fill the
                // samples with too few values and give the kind up.
                let long = cold::LONG_FROM as u64;
                for &(_, loc, _) in
                    group.iter().filter(|(_, loc, _)| loc.form == Form::Raw && loc.raw < long)
                {
                    // The samples are full (and trained, or gave up) or the kind gave up: a
                    // copy would be kept as no sample.
                    if !self.training[kind.code() as usize].wants_samples() {
                        break;
                    }
                    let pack = &self.tiers[0].packs[loc.pack as usize];
                    let payload = loc.offset.checked_add(RECORD_HEADER);
                    let Some(raw) = payload.and_then(|at| pack.bytes(at, loc.raw)) else {
                        continue;
                    };
                    // A record met by an earlier settle, or put while the store is open, is
                    // sampled already: a settle that meets it again adds nothing.
                    let content = content_of(raw);
                    if self.training[kind.code() as usize].sampled.contains(&content) {
                        continue;
                    }
                    // alloc: a settle (idle), a value not sampled yet, until a dictionary is trained; `sample` keeps a copy and may append to the pack this borrows
                    let bytes = raw.to_vec();
                    self.sample(kind, content, &bytes);
                    if self.dictionary_of(kind).is_some() {
                        break;
                    }
                }
            }
            let number = self.dictionary_of(kind);
            let dictionary = number.map(|number| {
                self.tiers[0].dictionaries[number as usize]
                    .bytes
                    // clone: an `Arc`, once per kind a settle (idle), held while it appends
                    .clone()
                    .expect("a dictionary of its own is held")
            });
            let mut batch = match cold::Batch::new(dictionary.as_deref()) {
                Ok(batch) => batch,
                Err(why) => {
                    self.cannot_compress(kind, &why);
                    return settled;
                }
            };
            for &(key, loc, row) in group {
                let short = loc.raw < DEMOTE_FROM;
                // A short cold record is listed while its kind has no dictionary only to be
                // sampled (`tier_moves`). It is not moved yet: without the dictionary it saves
                // too little, and would be marked tried and never moved once one is trained.
                if short && number.is_none() {
                    continue;
                }
                // A short record costs a record's work: counted as `DEMOTE_FROM` bytes, so a
                // settle of many short records stays a short step too. They have at most
                // `SHORT_MOST` of a settle, so a backlog of them leaves the rest of the step to
                // longer records, promotion and retiring.
                let cost = loc.raw.max(DEMOTE_FROM);
                if short && spent_short.saturating_add(cost) > SHORT_MOST {
                    more_short = true;
                    continue;
                }
                if spent > 0 && spent.saturating_add(cost) > SETTLE_MOST {
                    settled.more = true;
                    break 'kinds;
                }
                let Some((content, bytes)) = self.movable(key, loc) else { continue };
                spent += cost;
                if short {
                    spent_short += cost;
                }
                if let Some(columns) = encodings_of(&encodings, &bytes) {
                    match colder_columns(columns, loc, &bytes) {
                        Ok(Some((form, payload))) => {
                            self.replace(key, loc, form, 0, content, &payload);
                            settled.demoted += 1;
                        }
                        Ok(None) => self.set_flag(row, Flag::Tried),
                        Err(why) => {
                            // alloc: a problem reported, an encoding that failed
                            self.problems.push(format!("{:x} is not moved colder: {why}", key.0));
                            self.set_flag(row, Flag::Tried);
                        }
                    }
                    continue;
                }
                let long = bytes.len() >= cold::LONG_FROM;
                let stored =
                    if long { cold::compress_long(&bytes) } else { batch.compress(&bytes) };
                let stored = match stored {
                    Ok(stored) => stored,
                    Err(why) => {
                        self.cannot_compress(kind, &why);
                        return settled;
                    }
                };
                if stored.len() as u64 + loc.raw / 8 > loc.raw {
                    self.set_flag(row, Flag::Tried);
                    continue;
                }
                let dictionary = if long { NO_DICTIONARY } else { number.unwrap_or(NO_DICTIONARY) };
                self.replace(key, loc, Form::Zstd, dictionary, content, &stored);
                settled.demoted += 1;
            }
        }
        for (key, loc, row) in promote {
            if settled.more || (spent > 0 && spent.saturating_add(loc.raw) > SETTLE_MOST) {
                settled.more = true;
                break;
            }
            let Some((content, bytes)) = self.movable(key, loc) else { continue };
            spent += loc.raw;
            match (loc.kind == Kind::Output).then(|| cold::lz4(&bytes)).flatten() {
                Some(stored) => self.replace(key, loc, Form::Lz4, 0, content, &stored),
                None => self.replace(key, loc, Form::Raw, 0, content, &bytes),
            }
            settled.promoted += 1;
        }
        for at in self.retirable() {
            if settled.more {
                break;
            }
            let Some(live) = self.live_records(at) else { continue };
            let bytes: u64 = live.iter().map(|(header, _)| header.span()).sum();
            if spent > 0 && spent + bytes > SETTLE_MOST {
                settled.more = true;
                break;
            }
            spent += bytes;
            let unit = Unit(self.tiers[0].packs[at].unit);
            for (header, payload) in live {
                self.rehome(unit, header, &payload);
            }
            self.tiers[0].packs[at].retiring = true;
            settled.retired += 1;
        }
        // Short records left by their share are more to do, said only now: `settled.more` set
        // earlier would stop promotion and retiring in this settle.
        settled.more |= more_short;
        let own = &self.tiers[0];
        if let Some(index) = &own.index {
            index.bits.rotate();
        }
        own.delta.rotate(Flag::Read as usize, Flag::ReadBefore as usize);
        own.delta.rotate(Flag::New as usize, Flag::NewBefore as usize);
        self.periods = self.periods.saturating_add(1);
        self.ask_residency(asks, effects);
        self.start_commit(effects);
        settled
    }

    /// Asks the page cache of the records at `asks` (own pack, offset, span), one
    /// [`Effect::Residency`] a pack, by pack and offset and at most [`RESIDENCY_MOST`], unless
    /// a round is still out or this store does not ask. A pack this settle retired is left out.
    fn ask_residency(&mut self, mut asks: Vec<(u32, u64, u64)>, effects: &mut Vec<Effect>) {
        if !self.residency.asks || self.residency.waiting > 0 {
            return;
        }
        let own = &self.tiers[0];
        asks.retain(|&(pack, _, _)| {
            own.packs.get(pack as usize).is_some_and(|it| !it.removed && !it.retiring)
        });
        if asks.is_empty() {
            return;
        }
        radix::sort_by_key(&mut asks, |&(pack, at, _)| radix::pack_64(u64::from(pack), at));
        asks.truncate(RESIDENCY_MOST);
        self.residency.round += 1;
        self.residency.answering.clear();
        let round = self.residency.round;
        for group in asks.chunk_by(|a, b| a.0 == b.0) {
            let pack = group[0].0;
            // clone: the effect and the op each own the name; once per pack a settle (idle)
            let file = self.tiers[0].packs[pack as usize].file.clone();
            // alloc: once per pack a settle (idle), the ranges the effect carries
            let ranges = group.iter().map(|&(_, at, len)| Extent { at, len }).collect();
            // alloc: once per pack a settle (idle), the offsets its answer is kept by
            let offsets = group.iter().map(|&(_, at, _)| at).collect();
            // clone: a name's refcount, for the effect
            let named = file.clone();
            let tag = self.tags.ask(Op::Residency { round, file, pack, offsets });
            effects.push(Effect::Residency { file: named, ranges, tag });
            self.residency.waiting += 1;
        }
    }

    /// Takes the answer of an [`Effect::Residency`] of `file` (own pack `pack`) for the
    /// records at `offsets`, of round `round`. One that failed leaves those records' residency
    /// unknown, named in the problems: nothing is guessed of them.
    fn residency_answered(
        &mut self,
        round: u64,
        file: FileName,
        pack: u32,
        offsets: &[u64],
        done: Done,
    ) {
        let current = round == self.residency.round;
        match done {
            Done::Resident(pages) if pages.len() == offsets.len() => {
                if current {
                    for (&at, pages) in offsets.iter().zip(&pages) {
                        self.residency.answering.insert((pack, at), pages.resident());
                    }
                }
            }
            // alloc: a problem reported
            Done::Resident(pages) => self.problems.push(format!(
                "{file}: the page-cache residency of {} records came back as {} counts, so \
                 settles move them by their reads alone until it is asked again",
                offsets.len(),
                pages.len()
            )),
            // alloc: a problem reported
            done => self.problems.push(format!(
                "{file}: the page-cache residency of {} records is unknown, so settles move \
                 them by their reads alone until it is asked again: {}",
                offsets.len(),
                describe(&done)
            )),
        }
        if current {
            self.residency.waiting -= 1;
            if self.residency.waiting == 0 {
                std::mem::swap(&mut self.residency.known, &mut self.residency.answering);
                self.residency.answering.clear();
            }
        }
    }

    /// The own tier's records in its packs to move cold and to move hot, as of the period
    /// ending now, and those to ask the page cache about (pack, offset, span) for the next. A
    /// record still in a segment is new, and stays.
    fn tier_moves(&self) -> (Moves, Moves, Vec<(u32, u64, u64)>) {
        let own = &self.tiers[0];
        // alloc: once a settle (idle, never on the way of a get or a put); empty unless a slot is not whole
        let mut bad = Vec::new();
        let rows = self.look().rows(0, &mut bad);
        // alloc: once a settle (idle), the records it moves, sorted after
        let (mut demote, mut promote) = (Vec::new(), Vec::new());
        // alloc: once a settle (idle), the records it asks the page cache about
        let mut asks = Vec::new();
        // By kind code: whether a short record of the kind is listed. A cold kind's is, to be
        // moved colder once the kind has a dictionary and, before, to be sampled while its
        // training wants samples (after a reopen, the only way its records already kept are).
        let short_listed: [bool; Kind::ALL.len()] = std::array::from_fn(|code| {
            Kind::from_code(code as u8).is_some_and(|kind| {
                kind.cold()
                    && (self.dictionary_of(kind).is_some() || self.training[code].wants_samples())
            })
        });
        for (key, found) in rows {
            let Place::Pack(loc) = found.place else { continue };
            let Some(pack) = own.packs.get(loc.pack as usize) else { continue };
            // A short record goes colder only as `short_listed` says, and never comes back hot.
            let short = loc.raw < DEMOTE_FROM;
            if pack.removed || pack.retiring || (short && !short_listed[loc.kind.code() as usize]) {
                continue;
            }
            let flag = |flag| found.at.get(flag);
            let (read, before) = (flag(Flag::Read), flag(Flag::ReadBefore));
            // Whether its flags would move it colder once it is old enough, or hotter with
            // one more period of reading or with its pages cached.
            let colder = matches!(loc.form, Form::Raw | Form::Lz4 | Form::Warm)
                && !read
                && !before
                && !flag(Flag::Tried);
            let hotter =
                matches!(loc.form, Form::Zstd | Form::Warm | Form::ColdColumns) && !short;
            if self.residency.asks
                && (colder || (hotter && (read || before)))
                && let Some(span) = disk::span_of(loc.stored)
            {
                asks.push((loc.pack, loc.offset, span));
            }
            let resident = self.residency.of(&loc) == Some(true);
            match loc.form {
                Form::Raw | Form::Lz4 | Form::Warm
                    if self.periods >= 2
                        && colder
                        && !flag(Flag::New)
                        && !flag(Flag::NewBefore)
                        && !resident =>
                {
                    demote.push((key, loc, found.at.row()));
                }
                Form::Zstd | Form::Warm | Form::ColdColumns
                    if hotter && ((read && before) || ((read || before) && resident)) =>
                {
                    promote.push((key, loc, found.at.row()));
                }
                _ => {}
            }
        }
        (demote, promote, asks)
    }

    /// Sets a flag of an own record by its row: rows hold while the store swaps no table, as
    /// within a settle.
    fn set_flag(&self, row: Row, flag: Flag) {
        let own = &self.tiers[0];
        match row {
            Row::Slot(slot) => {
                if let Some(index) = &own.index {
                    index.bits.set(slot, flag);
                }
            }
            Row::Delta(row) => own.delta.set_flag(row, flag as usize),
        }
    }

    /// The bytes of the record of `key` at `loc`, verified, or `None` with why in the problems.
    fn movable(&mut self, key: Key, loc: Loc) -> Option<(ContentId, Vec<u8>)> {
        let look = self.look();
        let value = match look.find(&key) {
            Some(Ok(found)) if found.place == Place::Pack(loc) => look
                .value(found, &key, Use::Move)
                .map(|(header, bytes)| (header, bytes.into_owned())),
            Some(Err(corrupt)) => Err(corrupt),
            _ => return None,
        };
        match value {
            // `replace` counts the old record's bytes dead by `loc`, which came from an index
            // slot nothing checked: it must be the record that was read.
            Ok((header, _)) if header.stored != loc.stored || header.raw != loc.raw => {
                // alloc: a problem reported
                self.problems.push(format!(
                    "{:x} is not moved: its record is not the one its index names",
                    key.0
                ));
                None
            }
            Ok((header, bytes)) => Some((header.content, bytes)),
            Err(corrupt) => {
                // alloc: a problem reported
                self.problems.push(format!("not moved between tiers: {corrupt}"));
                None
            }
        }
    }

    /// Writes another record of `key` in place of the one at `old`, whose bytes are then dead.
    fn replace(
        &mut self,
        key: Key,
        old: Loc,
        form: Form,
        dictionary: u32,
        content: ContentId,
        payload: &[u8],
    ) {
        let unit = Unit(self.tiers[0].packs[old.pack as usize].unit);
        let header = RecordHeader {
            kind: old.kind,
            form,
            dictionary,
            stored: payload.len() as u64,
            raw: old.raw,
            key,
            content,
        };
        self.rehome(unit, header, payload);
        self.tiers[0].packs[old.pack as usize].dead +=
            disk::span_of(old.stored).expect("`movable` checked `old` against the record it read");
    }

    /// Writes a record the store moves into its own segment and points its key at it. The
    /// value was durable where it was, so it is as durable now. A record too long for a
    /// segment chunk is refused by `write`, which stops the store: the key stays where it is,
    /// and the settle's `start_commit` fails before any commit removes a pack.
    fn rehome(&mut self, unit: Unit, header: RecordHeader, payload: &[u8]) {
        let Some((seg, span)) = self.own.write(unit, &header, payload, 0) else { return };
        self.own.keep(span);
        let spot = Spot::new(header.key, header.content, Place::Segment(seg), Ticket::DURABLE);
        let held = Held::writer(&*self.shared);
        let delta = &self.tiers[0].delta;
        let row = match delta.replace(held, spot) {
            Ok(_) => delta.get(held, &header.key).expect("a key replaced just now").0,
            // Only in the index table, or (a slot not whole) put meanwhile by a writer.
            Err(spot) => match delta.insert(held, spot) {
                Ok((row, _)) | Err((_, (row, _))) => row,
            },
        };
        delta.set_flag(row, Flag::New as usize);
    }

    /// The own packs that are at least half dead bytes, wholly committed, not the one their
    /// kind appends to, holding no dictionary, and no pack a batch whose moves wait moves
    /// records into. Maps answer in any order (a driver answers each as it ends; the sim
    /// shuffles what is due), so such a pack can be mapped while its batch waits on another
    /// file's map: its records are not yet what their keys find, so `live_records` would
    /// leave them out, and the batch would then point their keys into the removed pack.
    fn retirable(&self) -> Vec<usize> {
        let own = &self.tiers[0];
        // alloc: once a settle (idle), a flag a pack
        let mut moved_into = vec![false; own.packs.len()];
        for it in self.pending.iter().chain([&self.draining]).flat_map(|it| &it.moves) {
            moved_into[it.to.pack as usize] = true;
        }
        (0..own.packs.len())
            .filter(|&at| {
                let pack = &own.packs[at];
                !moved_into[at]
                    && !pack.removed
                    && !pack.retiring
                    && pack.end == pack.committed
                    && pack.usable == pack.committed
                    && pack.committed > PACK_HEADER
                    && pack.dead * 2 >= pack.committed - PACK_HEADER
                    && own.packs.iter().rposition(|it| it.unit == pack.unit && !it.removed)
                        != Some(at)
                    && !own.dictionaries.iter().any(|it| it.pack as usize == at)
            })
            // alloc: once a settle (idle), the packs it may rewrite, which it appends to as it goes
            .collect()
    }

    /// The records of own pack `at` that are what their key finds, with their payloads; `None`
    /// with why in the problems when the pack cannot be read through.
    fn live_records(&mut self, at: usize) -> Option<Vec<(RecordHeader, Vec<u8>)>> {
        let own = &self.tiers[0];
        let pack = &own.packs[at];
        // alloc: once per pack a settle (idle) rewrites, its live records
        let mut live = Vec::new();
        let mut offset = PACK_HEADER;
        let mut why = None;
        while offset < pack.committed {
            let Some(start) = pack.bytes(offset, 4) else {
                // alloc: a problem reported
                why = Some(format!("at {offset} is not mapped"));
                break;
            };
            if start == [0; 4] {
                let next = offset.next_multiple_of(disk::BLOCK);
                if next == offset {
                    // alloc: a problem reported
                    why = Some(format!("at {offset} is neither a record nor padding"));
                    break;
                }
                offset = next;
                continue;
            }
            let header = match pack.bytes(offset, RECORD_HEADER).map(RecordHeader::decode) {
                Some(Ok(header)) => header,
                Some(Err(what)) => {
                    // alloc: a problem reported
                    why = Some(format!("the record at {offset}: {what}"));
                    break;
                }
                None => {
                    // alloc: a problem reported
                    why = Some(format!("the record at {offset} is outside the pack"));
                    break;
                }
            };
            let here = matches!(
                self.look().find(&header.key),
                Some(Ok(found)) if matches!(found.place, Place::Pack(loc) if loc.pack as usize == at && loc.offset == offset)
            );
            if here && header.form != Form::Dictionary {
                let Some(payload) = pack.bytes(offset + RECORD_HEADER, header.stored) else {
                    // alloc: a problem reported
                    why = Some(format!("the record at {offset} runs past the pack"));
                    break;
                };
                // alloc: a settle (idle) rewriting a half-dead pack; `rehome` takes `&mut self` while this borrows it
                live.push((header, payload.to_vec()));
            }
            offset += header.span();
        }
        match why {
            None => Some(live),
            Some(why) => {
                // alloc: a problem reported
                self.problems.push(format!("{} is not rewritten: {why}", pack.file));
                None
            }
        }
    }

    fn remove_pack(&mut self, pack: usize, effects: &mut Vec<Effect>) {
        let tag = self.tags.ask(Op::RemovePack { pack });
        // clone: the effect owns its file name; once per pack removed
        effects.push(Effect::Remove { file: self.tiers[0].packs[pack].file.clone(), tag });
    }

    /// Checks every value of `kind` (all, for `None`): what a scrub or the oracle runs.
    pub fn verify(&self, kind: Option<Kind>) -> Vec<Corrupt> {
        // alloc: a scrub's report, empty unless something is corrupt
        let mut out = Vec::new();
        let nothing = Key(ContentId::default());
        let look = self.look();
        for (tier_at, tier) in self.tiers.iter().enumerate() {
            if let (None, Some(index), Some(table)) = (kind, tier.head.index, &tier.index) {
                let found = crc::crc32c(table.mapping.bytes());
                if found != index.check {
                    out.push(Corrupt {
                        key: nothing,
                        pack: tier.index_file(),
                        // alloc: a corrupt index reported
                        what: format!(
                            "the index's CRC-32C is {found:08x}, not the {:08x} its head names",
                            index.check
                        ),
                    });
                }
            }
            for dictionary in &tier.dictionaries {
                if let (true, Err(why)) =
                    (kind.is_none_or(|it| it == dictionary.kind), &dictionary.bytes)
                {
                    out.push(Corrupt {
                        key: nothing,
                        // clone: a corrupt value reported
                        pack: tier.packs[dictionary.pack as usize].file.clone(),
                        // alloc: a corrupt value reported
                        what: format!("its {} dictionary {why}", dictionary.kind.name()),
                    });
                }
            }
            // alloc: a scrub's slots that are not whole, once per tier; empty unless one is not
            let mut bad = Vec::new();
            let mut entries = look.rows(tier_at, &mut bad);
            for (key, what) in bad {
                out.push(Corrupt { key, pack: tier.index_file(), what: what.into() });
            }
            entries.sort_unstable_by_key(|(key, found)| match found.place {
                Place::Pack(loc) => (0, u64::from(loc.pack), loc.offset, *key),
                Place::Segment(seg) => (1, u64::from(seg.segment), seg.offset, *key),
            });
            for (key, found) in entries {
                if kind.is_some_and(|it| it != found.place.kind()) {
                    continue;
                }
                if let Err(corrupt) = look.value(found, &key, Use::Check) {
                    out.push(corrupt);
                }
            }
        }
        out
    }

    /// Asks for the value of `key` to be placed at `to`, sharing its blocks (reflink,
    /// `clonefile`) where it is long enough to be worth it.
    ///
    /// How it went, and how many bytes it cloned, wrote and copied, comes through
    /// [`Store::placed`]. What a value's kind, form and length decide is how it is placed, never
    /// a try of one way then another: a file of its own is cloned; an output of [`CLONE_FROM`]
    /// bytes or more is cloned from the whole file of it the store keeps, which the first place
    /// of it writes; anything else is written. A clone fails where the file system cannot share
    /// blocks, naming it, and nothing copies instead.
    pub fn place(&mut self, key: &Key, to: FileName, effects: &mut Vec<Effect>) -> Option<Tag> {
        assert!(
            !self.tiers.iter().any(|tier| to.is_under(&tier.dir)),
            "{to} is in the store: values are placed outside it"
        );
        self.tidy();
        enum Plan {
            Clone(FileName, u64, ContentId),
            File(ContentId, u64),
            Write(Vec<u8>),
            Refused(String),
        }
        let look = self.look();
        let found = look.find(key)?;
        let plan = match found.and_then(|found| Ok((found, look.record(found, key)?.0))) {
            // alloc: the error of a corrupt value
            Err(corrupt) => Plan::Refused(corrupt.to_string()),
            Ok((found, header)) if clones_from_file(&header, found.place.form()) => {
                Plan::File(header.content, header.raw)
            }
            Ok((found, _)) => match look.read(found, key) {
                // alloc: the error of a corrupt value
                Err(corrupt) => Plan::Refused(corrupt.to_string()),
                Ok(entry) => match self.tiers[found.tier].externals.get(&entry.content) {
                    Some(external)
                        if matches!(found.place, Place::Pack(loc) if loc.form == Form::External)
                            && external.committed =>
                    {
                        // clone: the clone effect owns the name it clones from; once per place
                        Plan::Clone(external.file.clone(), entry.bytes.len() as u64, entry.content)
                    }
                    _ => {
                        let mut bytes = Vec::with_capacity(entry.bytes.len());
                        copy::append(&mut bytes, entry.bytes);
                        Plan::Write(bytes)
                    }
                },
            },
        };
        let tag = self.tags.fresh();
        match plan {
            Plan::Clone(from, raw, content) => {
                let bytes = PlacedBytes { cloned: raw, ..PlacedBytes::default() };
                effects.push(self.clear_staged(tag, from, to, content, None, bytes));
            }
            Plan::File(content, raw) => {
                let waiter = Waiter { asked: tag, to, key: *key, raw, retried: false };
                match (&self.broken, self.placed_looked) {
                    // alloc: one waiter refused, with why
                    (Some(broken), _) => self.refuse_waiting(vec![waiter], &broken.to_string()),
                    (None, true) => self.place_file(content, waiter, effects),
                    (None, false) => {
                        self.waiting_for_look();
                        self.held.push((content, waiter));
                    }
                }
            }
            Plan::Write(bytes) => {
                let placed = PlacedBytes { copied: bytes.len() as u64, ..PlacedBytes::default() };
                // No staged name: every driver makes a write under a temporary name of its own
                // process beside `to` and renames it over `to` (`write_atomically`, io_uring's
                // `push_temporary`), so two writes of the same bytes there both end with them.
                // clone: the effect and the op each own the name; once per place
                effects.push(Effect::Write { file: to.clone(), bytes, tag });
                let op = Op::Place { asked: tag, to, staged: None, of: None, bytes: placed };
                self.tags.ops.insert(tag, op);
            }
            Plan::Refused(why) => self.placed.push(Placed {
                tag,
                to,
                result: Err(why),
                bytes: PlacedBytes::default(),
            }),
        }
        Some(tag)
    }

    /// Lets go of the whole file kept to place `key`'s value by cloning ([`CLONE_FROM`]), for a
    /// path that now holds `now`'s value instead, once no clone of it is in flight: so a store
    /// whose outputs are built again and again does not keep a whole file of each. Nothing goes
    /// when `now`'s bytes are `key`'s (two keys of one output). Only that file goes, never the
    /// value (its record and chunks stay): placing it again writes it again, and every clone of
    /// it already placed keeps its blocks.
    pub fn unplace(&mut self, key: &Key, now: &Key, effects: &mut Vec<Effect>) {
        self.tidy();
        let look = self.look();
        let content_of_key = |key: &Key| {
            let found = look.find(key)?.ok()?;
            let (header, _) = look.record(found, key).ok()?;
            Some((header.content, clones_from_file(&header, found.place.form())))
        };
        let Some((content, true)) = content_of_key(key) else { return };
        if content_of_key(now).is_some_and(|(it, _)| it == content) {
            return;
        }
        match (self.placed_looked, &self.broken) {
            (true, _) => self.let_go(content, effects),
            // alloc: once per output built again while the open's look runs
            (false, None) => {
                self.waiting_for_look();
                self.held_unplaces.push(content);
            }
            (false, Some(_)) => {}
        }
    }

    /// The whole file of `content` removed, once no clone of it is in flight ([`Store::unplace`]).
    fn let_go(&mut self, content: ContentId, effects: &mut Vec<Effect>) {
        match self.files.get_mut(&content) {
            Some(PlacedFile::There { lost: true, .. }) => {}
            Some(PlacedFile::There { cloning, doomed, .. }) if *cloning > 0 => *doomed = true,
            Some(PlacedFile::There { .. }) | None => self.remove_file(content, Vec::new(), effects), // alloc: empty, allocates nothing
            // Wanted again, or going already.
            Some(_) => {}
        }
    }

    /// What the open found of the whole files it places from: kept for this run, which checks
    /// each one's bytes against its name before it first clones it, and the partial writes of an
    /// earlier run it removed.
    pub fn placed_on_open(&self) -> PlacedOnOpen {
        self.placed_on_open
    }

    /// The directory of the whole files outputs are cloned from ([`CLONE_FROM`]).
    fn placed_dir(&self) -> FileName {
        // alloc: a file name for an effect, which owns it; once per open
        self.tiers[0].file("placed")
    }

    /// Asks whether the directory of whole files is there, as the store opens, to list it.
    fn ask_placed(&mut self, effects: &mut Vec<Effect>) {
        let tag = self.tags.ask(Op::LookPlaced);
        effects.push(Effect::Stat { file: self.placed_dir(), tag });
    }

    /// A place or an unplace is held for the open's look through `placed/`: the wait's span
    /// opens with the first, naming the directory looked through; it closes as the look ends
    /// or the store breaks, saying how many places and unplaces waited.
    fn waiting_for_look(&mut self) {
        if self.look_wait.is_none() {
            let dir = self.placed_dir();
            // No parent: a request span current at the first hold would otherwise stay open
            // until the look ends, stretching its profile row.
            self.look_wait = Some(tracing::info_span!(
                target: "blaze",
                parent: None,
                "places wait for the open's look through placed/",
                %dir,
                places = tracing::field::Empty,
                unplaces = tracing::field::Empty,
            ));
        }
    }

    /// The span of the wait for the look closed, saying how many waited.
    fn look_wait_over(&mut self) {
        if let Some(span) = self.look_wait.take() {
            span.record("places", self.held.len());
            span.record("unplaces", self.held_unplaces.len());
        }
    }

    /// The look through `placed/` ended: the files let go of meanwhile go, then the places held
    /// for it go on. When it could not be done (`problem`, why), the store is broken, naming the
    /// directory: nothing places by cloning from a directory whose leftovers were not swept,
    /// and the places held are refused ([`Store::fail`]).
    fn placed_look_done(&mut self, problem: Option<String>, effects: &mut Vec<Effect>) {
        let dir = self.placed_dir();
        if let Some(why) = problem {
            // alloc: the error that breaks the store
            let what = format!(
                "looking through it as the store opened, to sweep an earlier run's leftovers: {why}"
            );
            tracing::error!(target: "blaze", %dir, "{what}");
            return self.fail(dir, what);
        }
        let PlacedOnOpen { kept, kept_bytes, swept } = self.placed_on_open;
        let held = self.held.len();
        tracing::info!(target: "blaze", %dir, kept, kept_bytes, swept, held, "placed files found at open");
        self.look_wait_over();
        self.placed_looked = true;
        for content in std::mem::take(&mut self.held_unplaces) {
            self.let_go(content, effects);
        }
        for (content, waiter) in std::mem::take(&mut self.held) {
            self.place_file(content, waiter, effects);
        }
    }

    /// The look through `placed/` failed with `done`: asked again from the start, until it has
    /// failed [`FAILURES_MOST`] times (its own count, `look_tries`), which breaks the store
    /// naming the directory, what the look was for and the last error.
    fn placed_look_failed(&mut self, done: &Done, effects: &mut Vec<Effect>) {
        self.look_tries += 1;
        match self.look_tries < FAILURES_MOST {
            true => self.ask_placed(effects),
            // alloc: the error that breaks the store
            false => self.placed_look_done(
                Some(format!("failed {FAILURES_MOST} times, the last: {}", describe(done))),
                effects,
            ),
        }
    }

    /// A problem with a whole file kept to clone from, logged as it is found (it may be a disk
    /// that damages files, or a writer outside the daemon) and kept in [`Store::problems`].
    fn placed_problem(&mut self, what: String) {
        tracing::warn!(target: "blaze", "{what}");
        self.problems.push(what);
    }

    /// The directory of whole files as the open found it: a file named for a content stays and
    /// is checked before it is first cloned from; anything else (an earlier run's temporary file
    /// of a write a crash cut short) goes. Nothing of this run's is there yet: its places wait
    /// for this ([`Store::place`]).
    fn placed_listed(&mut self, done: Done, effects: &mut Vec<Effect>) {
        let Done::Listed(found) = done else {
            return self.placed_look_failed(&done, effects);
        };
        for (file, meta) in found {
            if meta.dir {
                continue;
            }
            let name = file.as_str().rsplit('/').next().unwrap_or("");
            let named = name.len() == 2 * size_of::<ContentId>()
                && name.bytes().all(|it| matches!(it, b'0'..=b'9' | b'a'..=b'f'));
            if named {
                self.placed_on_open.kept += 1;
                self.placed_on_open.kept_bytes += meta.len;
            } else {
                self.placed_on_open.swept += 1;
                // clone: the effect and the op each own the name; once per file swept
                let tag = self.tags.ask(Op::Swept { file: file.clone() });
                effects.push(Effect::Remove { file, tag });
            }
        }
        self.placed_look_done(None, effects);
    }

    /// The whole file of `content`, which outputs of [`CLONE_FROM`] bytes or more are cloned from.
    fn placed_file(&self, content: &ContentId) -> FileName {
        // alloc: a file name for an effect, which owns it; once per place
        self.tiers[0].file(&format!("placed/{content:x}"))
    }

    /// `waiter` placed as a clone of the whole file of `content`, once it is there.
    fn place_file(&mut self, content: ContentId, waiter: Waiter, effects: &mut Vec<Effect>) {
        match self.files.get_mut(&content) {
            Some(PlacedFile::There { lost: true, after, .. }) => after.push(waiter),
            Some(PlacedFile::There { cloning, doomed, .. }) => {
                *cloning += 1;
                *doomed = false;
                self.clone_file(content, waiter, 0, effects);
            }
            Some(
                PlacedFile::Probing(waiting)
                | PlacedFile::Checking(waiting)
                | PlacedFile::Writing(waiting)
                | PlacedFile::Sealing { waiting, .. }
                | PlacedFile::Removing(waiting),
            ) => waiting.push(waiter),
            None => {
                let file = self.placed_file(&content);
                // alloc: the places waiting on one file, once per file the store has not looked for since it opened
                self.files.insert(content, PlacedFile::Probing(vec![waiter]));
                // Of the name itself: a link put there is a file of its own length, below
                // `CLONE_FROM`, so it is written again, never hashed or sealed through.
                let tag = self.tags.ask(Op::Probe { content });
                effects.push(Effect::Lstat { file, tag });
            }
        }
    }

    /// `waiter`'s path cleared and the whole file of `content` cloned there; `written`, the
    /// bytes it wrote to make that file.
    fn clone_file(
        &mut self,
        content: ContentId,
        waiter: Waiter,
        written: u64,
        effects: &mut Vec<Effect>,
    ) {
        let from = self.placed_file(&content);
        let Waiter { asked, to, key, raw, retried } = waiter;
        let bytes = PlacedBytes { cloned: raw, written, copied: 0 };
        let of = Some(Source { content, key, retried });
        effects.push(self.clear_staged(asked, from, to, content, of, bytes));
    }

    /// The place `asked` of `from`'s clone at `to` begun ([`Staged`]): its staged name cleared
    /// by the effect it returns, then `from` cloned there ([`Op::Clear`]) and renamed onto `to`
    /// ([`Op::Commit`]).
    fn clear_staged(
        &mut self,
        asked: Tag,
        from: FileName,
        to: FileName,
        content: ContentId,
        of: Option<Source>,
        bytes: PlacedBytes,
    ) -> Effect {
        let temp = self.staging_name(&to, asked);
        // The planted bug: `to` itself removed and cloned onto, as before staged names.
        #[cfg(test)]
        let temp = match self.plant_clear_then_clone {
            // clone: a test's planted bug; the effect and the op each own the name
            true => to.clone(),
            false => temp,
        };
        // clone: the effect and the op each own the name; once per place
        let clear = Effect::Remove { file: temp.clone(), tag: asked };
        let staged = Staged { temp, content };
        self.tags.ops.insert(asked, Op::Clear { asked, from, to, staged, of, bytes });
        clear
    }

    /// The hidden name beside `to` that the place `asked` clones into before it renames it onto
    /// `to` ([`Staged`]): in `to`'s directory, so the rename never crosses a volume and is one
    /// atomic step. Unique to this store and this place: two daemons with different homes share
    /// an output directory, and their stores' directories differ (one daemon a directory, #686),
    /// so the store's hash (`staging`) tells their names apart and the tag this store's places.
    /// Only this store makes names with its hash, so it may remove one it finds there: a tag is
    /// given again after a reopen, and a crash may have left that name's clone ([`Op::Clear`]).
    fn staging_name(&self, to: &FileName, asked: Tag) -> FileName {
        let (dir, leaf) = match to.as_str().rsplit_once('/') {
            Some((dir, leaf)) => (Some(dir), leaf),
            None => (None, to.as_str()),
        };
        let mut cut = leaf.len().min(STAGING_LEAF_MOST);
        while !leaf.is_char_boundary(cut) {
            cut -= 1;
        }
        let (leaf, id, n) = (&leaf[..cut], self.staging, asked.0);
        let name = match dir {
            // alloc: a file name for an effect, which owns it; once per place
            Some(dir) => format!("{dir}/.{leaf}.{id:016x}-{n:x}.placing"),
            // alloc: a file name for an effect, which owns it; once per place
            None => format!(".{leaf}.{id:016x}-{n:x}.placing"),
        };
        FileName::new(&name).expect("a hidden name beside a file's name is a name")
    }

    /// A clone of the whole file of `content` ended: it goes once it is let go of (or found gone)
    /// and no other clone of it is in flight.
    fn cloned(&mut self, content: ContentId, effects: &mut Vec<Effect>) {
        let Some(PlacedFile::There { cloning, doomed, lost, after }) = self.files.get_mut(&content)
        else {
            return;
        };
        *cloning -= 1;
        if *cloning == 0 && (*doomed || *lost) {
            let after = std::mem::take(after);
            self.remove_file(content, after, effects);
        }
    }

    /// A clone from the whole file of `content` failed with `why`: when the file is gone (removed
    /// outside the daemon) or no regular file (a symbolic link, a FIFO), it is written again and
    /// `waiter` placed from it, once; when it is there, the clone's failure is the place's.
    fn rechecked(
        &mut self,
        content: ContentId,
        waiter: Waiter,
        why: &str,
        done: &Done,
        effects: &mut Vec<Effect>,
    ) {
        let file = self.placed_file(&content);
        let link = why.contains(blaze_core::CLONE_THROUGH_A_LINK)
            || why.contains(blaze_core::CLONE_OF_NO_FILE);
        if !link && !matches!(done, Done::Stat(None)) {
            // alloc: one waiter refused, its error naming the file it cloned
            return self.refuse_waiting(vec![waiter], &format!("cloning {file}: {why}"));
        }
        // alloc: a problem reported
        self.placed_problem(format!(
            "{file} could not be cloned from ({why}), and is written again"
        ));
        match self.files.get_mut(&content) {
            Some(PlacedFile::There { cloning, lost, after, .. }) => {
                *lost = true;
                after.push(waiter);
                if *cloning == 0 {
                    let after = std::mem::take(after);
                    self.remove_file(content, after, effects);
                }
            }
            _ => self.place_file(content, waiter, effects),
        }
    }

    fn remove_file(&mut self, content: ContentId, waiting: Vec<Waiter>, effects: &mut Vec<Effect>) {
        let file = self.placed_file(&content);
        self.files.insert(content, PlacedFile::Removing(waiting));
        let tag = self.tags.ask(Op::Unplace { content });
        effects.push(Effect::Remove { file, tag });
    }

    /// The whole file of `content` written from the value of a place waiting on it (the first
    /// whose key still has it), read and verified now.
    fn make_file(&mut self, content: ContentId, waiting: Vec<Waiter>, effects: &mut Vec<Effect>) {
        let mut read = Err(String::new());
        for waiter in &waiting {
            read = match self.look().get(&waiter.key) {
                Ok(Some(entry)) if entry.content == content => {
                    let mut bytes = Vec::with_capacity(entry.bytes.len());
                    copy::append(&mut bytes, entry.bytes);
                    Ok(bytes)
                }
                // alloc: an error, once per place that fails
                Ok(_) => Err(format!(
                    "the store no longer has the value of {:?} it was asked to place",
                    waiter.key
                )),
                // alloc: the error of a corrupt value
                Err(corrupt) => Err(corrupt.to_string()),
            };
            if read.is_ok() {
                break;
            }
        }
        match read {
            Ok(bytes) => {
                let file = self.placed_file(&content);
                self.files.insert(content, PlacedFile::Writing(waiting));
                let tag = self.tags.ask(Op::MakeFile { content });
                effects.push(Effect::Write { file, bytes, tag });
            }
            Err(why) => self.refuse_waiting(waiting, &why),
        }
    }

    fn refuse_waiting(&mut self, waiting: Vec<Waiter>, why: &str) {
        for Waiter { asked, to, .. } in waiting {
            // alloc: an error, once per place that fails
            self.placed.push(Placed {
                tag: asked,
                to,
                result: Err(why.to_owned()),
                bytes: PlacedBytes::default(),
            });
        }
    }

    /// `waiting` placed from the whole file of `content` once it is read-only again; `wrote`,
    /// whether this run wrote it.
    fn seal(
        &mut self,
        content: ContentId,
        waiting: Vec<Waiter>,
        wrote: bool,
        effects: &mut Vec<Effect>,
    ) {
        let file = self.placed_file(&content);
        self.files.insert(content, PlacedFile::Sealing { waiting, wrote });
        let tag = self.tags.ask(Op::Seal { content });
        effects.push(Effect::Seal { file, mode: PLACED_MODE, tag });
    }

    /// How a step of the whole file of `content` went ([`PlacedFile`]).
    fn file_step(&mut self, op: Op, done: Done, effects: &mut Vec<Effect>) {
        let content = match op {
            Op::Probe { content }
            | Op::Check { content }
            | Op::MakeFile { content }
            | Op::Seal { content }
            | Op::Unplace { content } => content,
            _ => unreachable!("not a step of a placed file"),
        };
        let Some(state) = self.files.remove(&content) else { return };
        let file = self.placed_file(&content);
        match (op, state, done) {
            (Op::Probe { .. }, PlacedFile::Probing(waiting), Done::Stat(Some(meta)))
                if !meta.dir && meta.len == waiting[0].raw =>
            {
                // A file of the right length this run did not write (an earlier run's, or one
                // changed outside): its bytes are checked against its name before anything
                // clones it.
                self.files.insert(content, PlacedFile::Checking(waiting));
                let tag = self.tags.ask(Op::Check { content });
                effects.push(Effect::Map { file, tag });
            }
            (Op::Probe { .. }, PlacedFile::Probing(waiting), Done::Stat(Some(meta))) => {
                // A write is whole or not there, so this is a file blaze did not write. It goes
                // before it is written again: a rename over a read-only file is refused on Windows.
                // alloc: a problem reported
                self.placed_problem(format!(
                    "{file} is {} bytes, not the {} of the value it is named for, and is written again",
                    meta.len, waiting[0].raw
                ));
                self.remove_file(content, waiting, effects);
            }
            (Op::Probe { .. }, PlacedFile::Probing(waiting), Done::Stat(None)) => {
                self.make_file(content, waiting, effects);
            }
            (Op::Check { .. }, PlacedFile::Checking(waiting), Done::Mapped(mapping)) => {
                let bytes = mapping.bytes().len();
                let found = tracing::info_span!(target: "blaze", "placed check", %file, bytes)
                    .in_scope(|| content_of(mapping.bytes()));
                drop(mapping);
                if found == content {
                    self.seal(content, waiting, false, effects);
                } else {
                    // alloc: a problem reported
                    self.placed_problem(format!(
                        "{file} holds bytes of {found:x}, not of its name, and is written again"
                    ));
                    self.remove_file(content, waiting, effects);
                }
            }
            (Op::MakeFile { .. }, PlacedFile::Writing(waiting), Done::Written) => {
                self.seal(content, waiting, true, effects);
            }
            (Op::Seal { .. }, PlacedFile::Sealing { waiting, wrote }, Done::Moded) => {
                // alloc: empty, allocates nothing
                let there = PlacedFile::There {
                    cloning: waiting.len() as u32,
                    doomed: false,
                    lost: false,
                    after: Vec::new(),
                };
                self.files.insert(content, there);
                for (at, waiter) in waiting.into_iter().enumerate() {
                    // The first place made the file: its bytes are written once, for all.
                    let written = if at == 0 && wrote { waiter.raw } else { 0 };
                    self.clone_file(content, waiter, written, effects);
                }
            }
            (Op::Seal { .. }, PlacedFile::Sealing { waiting, .. }, done) => {
                // A file not made read-only is not cloned from: it goes. Where the name was a
                // link or no regular file (swapped in after the write or the check), it is
                // written again for the places waiting, once each, as a clone that finds one
                // ([`Store::rechecked`]); otherwise they are refused and the next place writes
                // it again.
                let why = describe(&done);
                let swapped = why.contains(blaze_core::SEAL_THROUGH_A_LINK)
                    || why.contains(blaze_core::SEAL_OF_NO_FILE);
                // alloc: the places split, once per seal that fails
                let (mut again, refused): (Vec<Waiter>, Vec<Waiter>) =
                    waiting.into_iter().partition(|it| swapped && !it.retried);
                // alloc: an error, once per place that fails
                let why = format!("making {file} read-only: {why}");
                if !refused.is_empty() {
                    self.refuse_waiting(refused, &why);
                }
                if !again.is_empty() {
                    // alloc: a problem reported
                    self.placed_problem(format!("{why}, and is written again"));
                    for waiter in &mut again {
                        waiter.retried = true;
                    }
                }
                self.remove_file(content, again, effects);
            }
            (Op::Unplace { .. }, PlacedFile::Removing(waiting), Done::Removed) => {
                if !waiting.is_empty() {
                    self.make_file(content, waiting, effects);
                }
            }
            (Op::Unplace { .. }, PlacedFile::Removing(waiting), done) => {
                // alloc: a problem reported
                self.placed_problem(format!("{file} was left behind: {}", describe(&done)));
                if !waiting.is_empty() {
                    self.make_file(content, waiting, effects);
                }
            }
            (
                _,
                PlacedFile::Probing(waiting)
                | PlacedFile::Checking(waiting)
                | PlacedFile::Writing(waiting),
                done,
            ) => {
                // alloc: an error, once per place that fails
                let why = format!("{file}: {}", describe(&done));
                self.refuse_waiting(waiting, &why);
            }
            (_, state, _) => _ = self.files.insert(content, state),
        }
    }

    /// How the places asked for since the last call went.
    pub fn placed(&mut self) -> Vec<Placed> {
        std::mem::take(&mut self.placed)
    }

    /// Asks `remote` for the value of `key`, to be verified and kept when it comes. The answer
    /// is a completion of the tag returned, handed to [`Store::complete`]; how it went comes
    /// through [`Store::fetched`]. `None` when it has a value of `key` already or is fetching
    /// it, and when `remote` knew without asking that it has none, which [`Store::fetched`]
    /// says at once as [`Fetched::Missing`].
    ///
    /// The value is kept only when `admit` takes its kind and bytes, checked once they hash as
    /// sent: one it refuses is [`Fetched::Refused`] with its reason and is never kept, so
    /// nothing reads it. Every caller says what it lets in from elsewhere (the global cache's
    /// client refuses a value no machine may publish, one verified across checkouts among
    /// them, `blaze_global::public::fetchable`). A value of `key` the store has already is
    /// never fetched, so `admit` never refuses what this machine made.
    pub fn fetch(
        &mut self,
        key: &Key,
        remote: &mut impl Remote,
        admit: Admit,
        effects: &mut Vec<Effect>,
    ) -> Option<Tag> {
        if self.has(key) || self.fetching.contains_key(key) {
            return None;
        }
        let tag = self.tags.fresh();
        match remote.fetch(key, tag, effects) {
            Asked::Sent => {
                self.tags.ops.insert(tag, Op::Fetch { key: *key, admit });
                self.fetching.insert(*key, tag);
                Some(tag)
            }
            Asked::Missing => {
                self.fetched.push((*key, Fetched::Missing));
                None
            }
        }
    }

    /// Every key it has a value of in any tier, unverified, in no order, a key in two places
    /// perhaps twice: what the remote tier's filter of it is built of. Call
    /// [`Store::watch_keys`] first, so a key put while it is read is in a later
    /// [`Store::new_keys`].
    pub fn keys(&self) -> Vec<Key> {
        let held = Held::writer(&*self.shared);
        // alloc: once per full filter of the store, built for the remote tier's first peer and as its deltas grow
        let mut out = Vec::new();
        for tier in &self.tiers {
            if let Some(index) = &tier.index {
                out.extend(index.table().iter().map(|(_, key, _)| key));
            }
            for table in std::iter::once(&tier.delta).chain(&tier.prev) {
                out.extend(table.iter(held).map(|(_, spot)| spot.key));
            }
        }
        out
    }

    /// The keys its own tier's puts (by it and by every writer) made readable and no mapped
    /// pack holds yet, perhaps some twice: what [`Store::get`] already gives and
    /// [`Store::new_keys`] lists only once their batch is packed. The remote tier's filter
    /// deltas take them so a client never rules out a key the server would answer.
    pub fn unpacked_keys(&self) -> Vec<Key> {
        let held = Held::writer(&*self.shared);
        let own = &self.tiers[0];
        // alloc: once per refresh of the remote tier's filter, the puts not yet packed
        let mut out = Vec::new();
        for table in std::iter::once(&own.delta).chain(&own.prev) {
            out.extend(
                table
                    .iter(held)
                    .filter(|(_, spot)| matches!(spot.place, Place::Segment(_)))
                    .map(|(_, spot)| spot.key),
            );
        }
        out
    }

    /// Starts keeping the keys each batch puts into packs, for [`Store::new_keys`] to take:
    /// the remote tier's filter deltas. A store no server reads keeps none.
    pub fn watch_keys(&mut self) {
        // alloc: empty, allocates nothing; once per store a remote server reads
        self.new_keys.get_or_insert_with(Vec::new);
    }

    /// Whether [`Store::watch_keys`] was called.
    pub fn watching_keys(&self) -> bool {
        self.new_keys.is_some()
    }

    /// The batches whose puts are in packs: the epoch a filter of [`Store::keys`] covers.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The keys put by the batches into packs since the last call, perhaps some twice, with
    /// [`Store::epoch`]; empty unless [`Store::watch_keys`] was called.
    pub fn new_keys(&mut self) -> (u64, Vec<Key>) {
        // alloc: empty, allocates nothing; the keys taken are the caller's
        (self.epoch, self.new_keys.as_mut().map(std::mem::take).unwrap_or_default())
    }

    /// How the fetches answered since the last call went.
    pub fn fetched(&mut self) -> Vec<(Key, Fetched)> {
        std::mem::take(&mut self.fetched)
    }

    pub fn owns(&self, tag: Tag) -> bool {
        self.tags.range.owns(tag)
    }

    /// Takes a completion of its own: a mapping, an append, a sync. Says which puts its
    /// commits made durable. Not every one: [`Store::flush`] with nothing to write can also
    /// move a segment's durable count (puts that ended empty, such as a claim's loser) with
    /// no completion, so a waiter checks [`Store::durable`] rather than wait on this list.
    pub fn complete(&mut self, completion: Completion, effects: &mut Vec<Effect>) -> Vec<Ticket> {
        self.tidy();
        // alloc: empty, allocates nothing
        let Some(op) = self.tags.ops.remove(&completion.tag) else { return Vec::new() };
        let done = completion.done;
        match op {
            Op::Private => match done {
                Done::Private(None) => {
                    self.failures = 0;
                    self.read_heads(effects);
                    self.ask_placed(effects);
                }
                Done::Private(Some((file, access))) => {
                    // alloc: the error that breaks the store
                    self.fail(
                        file,
                        format!("{access}, and the store and all in it must be the user's alone"),
                    )
                }
                done => {
                    // clone: the name of what failed, on an error path
                    let dir = self.private_dir().clone();
                    if self.retry(&dir, &done) {
                        self.ask_private(effects);
                    }
                }
            },
            Op::Head { tier } => match done {
                Done::Read(bytes) => {
                    self.failures = 0;
                    match self.peers.is_some() {
                        true => self.peer_head(tier, &bytes, effects),
                        false => self.read_head(tier, &bytes, effects),
                    }
                }
                done => {
                    let file = self.tiers[tier].file(HEAD_FILE);
                    if self.retry(&file, &done) {
                        let tag = self.tags.ask(Op::Head { tier });
                        effects.push(Effect::ReadAll { file, tag });
                    }
                }
            },
            Op::Map { tier, file, what } => match done {
                Done::Mapped(mapping) => self.install(tier, file, what, mapping),
                done if self.is_peer(tier) => self.peer_failed(tier, file, describe(&done)),
                done => {
                    if self.retry(&file, &done) {
                        self.ask_map(tier, file, what, effects);
                    }
                }
            },
            Op::Clear { asked, from, to, staged, of, bytes } => match done {
                Done::Removed => {
                    // clone: the effect and the op each own the name; once per place
                    let into = staged.temp.clone();
                    let op = Op::Place { asked, to, staged: Some(staged), of, bytes };
                    let tag = self.tags.ask(op);
                    effects.push(Effect::Clone { from, to: into, tag });
                }
                done => {
                    if let Some(source) = of {
                        self.cloned(source.content, effects);
                    }
                    self.placed.push(Placed {
                        tag: asked,
                        to,
                        result: Err(describe(&done)),
                        bytes: PlacedBytes::default(),
                    });
                }
            },
            Op::Place { asked, to, staged, of, bytes } => {
                if let Some(source) = of {
                    self.cloned(source.content, effects);
                }
                match (done, of, staged) {
                    // The clone is whole under its own name: renamed onto `to`, it replaces
                    // whatever is there in one step, a concurrent place's clone of the same
                    // bytes too. (Under the planted bug the clone was made at `to` itself.)
                    (Done::Cloned, _, Some(staged)) if !(cfg!(test) && staged.temp == to) => {
                        // clone: the effect and the op each own the names; once per place
                        let (from, into) = (staged.temp.clone(), to.clone());
                        let tag = self.tags.ask(Op::Commit { asked, to, staged, bytes });
                        effects.push(Effect::Rename { from, to: into, tag });
                    }
                    (Done::Cloned | Done::Written, _, _) => {
                        self.placed.push(Placed { tag: asked, to, result: Ok(()), bytes })
                    }
                    (done, Some(Source { content, key, retried: false }), _) => {
                        // Whether the file it cloned from is gone or no file, which is made
                        // again once.
                        let waiter = Waiter { asked, to, key, raw: bytes.cloned, retried: true };
                        let why = describe(&done);
                        if why.contains(blaze_core::CLONE_THROUGH_A_LINK)
                            || why.contains(blaze_core::CLONE_OF_NO_FILE)
                        {
                            self.rechecked(content, waiter, &why, &done, effects);
                        } else {
                            let file = self.placed_file(&content);
                            let tag = self.tags.ask(Op::Recheck { content, waiter, why });
                            effects.push(Effect::Stat { file, tag });
                        }
                    }
                    (done, _, _) => {
                        let result = Err(describe(&done));
                        self.placed.push(Placed {
                            tag: asked,
                            to,
                            result,
                            bytes: PlacedBytes::default(),
                        });
                    }
                }
            }
            Op::Recheck { content, waiter, why } => {
                self.rechecked(content, waiter, &why, &done, effects)
            }
            Op::Commit { asked, to, staged, bytes } => match done {
                Done::Renamed => {
                    self.placed.push(Placed { tag: asked, to, result: Ok(()), bytes })
                }
                done => {
                    // Refused (on Windows, a `to` that is read-only or open without sharing
                    // its delete): a success only if `to` already holds the value's bytes.
                    let why = describe(&done);
                    // clone: the effect and the op each own the name; once per refused rename
                    let file = to.clone();
                    let tag = self.tags.ask(Op::Verify { asked, to, staged, bytes, why });
                    effects.push(Effect::Map { file, tag });
                }
            },
            Op::Verify { asked, to, staged, bytes, why } => {
                let Staged { temp, content } = staged;
                let result = match done {
                    Done::Mapped(mapping) => {
                        let found = tracing::info_span!(target: "blaze", "placed verify", %to)
                            .in_scope(|| content_of(mapping.bytes()));
                        drop(mapping);
                        match found == content {
                            true => Ok(()),
                            // alloc: the error of a place that fails
                            false => Err(format!(
                                "renaming {temp} over {to}: {why}; {to} holds bytes of \
                                 {found:x}, not of {content:x}"
                            )),
                        }
                    }
                    // alloc: the error of a place that fails
                    done => Err(format!(
                        "renaming {temp} over {to}: {why}; {to} could not be read to check \
                         its bytes: {}",
                        describe(&done)
                    )),
                };
                let bytes = if result.is_ok() { bytes } else { PlacedBytes::default() };
                self.placed.push(Placed { tag: asked, to, result, bytes });
                // clone: the effect and the op each own the name; once per refused rename
                let tag = self.tags.ask(Op::Discard { file: temp.clone() });
                effects.push(Effect::Remove { file: temp, tag });
            }
            Op::Discard { file } => {
                if !matches!(done, Done::Removed) {
                    // alloc: a problem reported
                    self.placed_problem(format!(
                        "{file}, a place's clone not renamed onto its path, was left behind: {}",
                        describe(&done)
                    ));
                }
            }
            Op::LookPlaced => match done {
                // None yet: nothing placed.
                Done::Stat(None) => self.placed_look_done(None, effects),
                Done::Stat(Some(meta)) if meta.dir => {
                    let tag = self.tags.ask(Op::ListPlaced);
                    effects.push(Effect::List { dir: self.placed_dir(), tag });
                }
                Done::Stat(Some(_)) => {
                    self.placed_look_done(Some("it is a file, not a directory".into()), effects)
                }
                done => self.placed_look_failed(&done, effects),
            },
            Op::ListPlaced => self.placed_listed(done, effects),
            Op::Swept { file } => {
                if !matches!(done, Done::Removed) {
                    // alloc: a problem reported
                    self.placed_problem(format!(
                        "{file}, a temporary file of an earlier run, was left behind: {}",
                        describe(&done)
                    ));
                }
            }
            op @ (Op::Probe { .. }
            | Op::Check { .. }
            | Op::MakeFile { .. }
            | Op::Seal { .. }
            | Op::Unplace { .. }) => {
                self.file_step(op, done, effects);
            }
            Op::Fetch { key, admit } => {
                self.fetching.remove(&key);
                let fetched = self.take_fetched(key, admit, done);
                self.fetched.push((key, fetched));
            }
            Op::RemovePack { pack } => {
                if !matches!(done, Done::Removed) {
                    let file = &self.tiers[0].packs[pack].file;
                    // alloc: a problem reported
                    self.problems.push(format!(
                        "{file} was left behind (it is removed at the next open): {}",
                        describe(&done)
                    ));
                }
            }
            Op::Residency { round, file, pack, offsets } => {
                self.residency_answered(round, file, pack, &offsets, done)
            }
            Op::Peers => self.peers_listed(done, effects),
            Op::Announce => self.announced(&done, effects),
            op => return self.step(&op, &done, effects),
        }
        self.peers_ask(effects);
        // alloc: empty, allocates nothing
        Vec::new()
    }

    /// Takes a mapping it asked for. Separate from [`Store::complete`] for drivers that map
    /// by themselves at start.
    pub fn mapped(&mut self, file: &FileName, mapping: Mapping) {
        self.tidy();
        let asked = self
            .tags
            .ops
            .iter()
            .filter_map(|(tag, op)| match op {
                Op::Map { file: it, what, .. } if it == file => Some((what.usable(), *tag)),
                _ => None,
            })
            .max();
        let Some((_, tag)) = asked else { return };
        if let Some(Op::Map { tier, file, what }) = self.tags.ops.remove(&tag) {
            self.install(tier, file, what, mapping);
        }
    }

    // Writing.

    /// The own pack of `kind` a record of `span` bytes goes into, made if none has room.
    fn writable_pack(&mut self, unit: Unit, span: u64) -> usize {
        let own = &mut self.tiers[0];
        if let Some(at) = own.packs.iter().rposition(|it| it.unit == unit.0 && !it.removed) {
            let pack = &own.packs[at];
            if pack.end == PACK_HEADER || pack.end + span <= PACK_MOST {
                return at;
            }
        }
        assert!(own.packs.len() < PACKS_MOST, "the store has {PACKS_MOST} packs: it is full");
        let file = own.file(&disk::pack_file(own.packs.len()));
        own.packs.push(Pack {
            unit: unit.0,
            file,
            mapping: None,
            usable: 0,
            // alloc: once per pack made (a unit's first, or one past `PACK_MOST`); appends then reuse it
            memory: disk::pack_header(unit.0).to_vec(),
            committed: 0,
            indexed: 0,
            end: PACK_HEADER,
            dead: 0,
            removed: false,
            retiring: false,
        });
        own.packs.len() - 1
    }

    fn append_record(&mut self, unit: Unit, header: RecordHeader, payload: &[u8]) -> Loc {
        let span = header.span();
        let at = self.writable_pack(unit, span);
        let pack = &mut self.tiers[0].packs[at];
        let offset = pack.end;
        pack.memory.extend_from_slice(&header.encode());
        pack.memory.extend_from_slice(payload);
        pack.memory.resize(pack.memory.len() + (span - RECORD_HEADER - header.stored) as usize, 0);
        pack.end += span;
        Loc {
            pack: at as u32,
            kind: header.kind,
            form: header.form,
            offset,
            stored: header.stored,
            raw: header.raw,
        }
    }

    /// Copies what every writer has published since the last drain into the packs, for the
    /// commit beginning: the keys keep pointing at the segments until the packs are mapped.
    fn drain(&mut self) {
        let mut batch = std::mem::take(&mut self.draining);
        batch.puts.resize(self.drains.len(), 0);
        for number in 0..self.drains.len() {
            loop {
                let at_chunk = self.drains[number].chunk;
                // SAFETY: the store frees a segment's chunks only behind its drain, in
                // `finish_batches`, never while it drains.
                let chunk = unsafe { at_chunk.get() };
                // Loaded before what it published: once there is a next, that is final.
                let next = chunk.next();
                let (bytes, puts) = chunk.published();
                let mut at = self.drains[number].at;
                while at < bytes {
                    at += self.drain_record(number, at_chunk, chunk, at, &mut batch);
                }
                let drain = &mut self.drains[number];
                drain.at = at;
                match next {
                    Some(next) => {
                        batch.chunks.push((number, drain.chunk));
                        drain.puts += puts;
                        drain.chunk = next;
                        drain.at = 0;
                    }
                    None => {
                        batch.puts[number] = drain.puts + puts;
                        break;
                    }
                }
            }
        }
        self.draining = batch;
    }

    /// Copies the record at `at` of a segment's chunk into its pack; gives its length there.
    fn drain_record(
        &mut self,
        number: usize,
        at_chunk: ChunkRef,
        chunk: &Chunk,
        at: u64,
        batch: &mut Batch,
    ) -> u64 {
        // SAFETY: `drain` passes an `at` below what the chunk published, and a writer publishes
        // only past whole records (`Own::end_put`, `Own::keep`, `Own::roll` publish after `at`
        // moved past the record), so the record at `at` is all below what was published.
        let frame = unsafe { chunk.bytes(at, FRAME) }.expect("a published record is in its chunk");
        let unit = u32::from_le_bytes(*frame.first_chunk().expect("a frame is 16 bytes"));
        let copy = frame[4] == COPY;
        // SAFETY: as above, for the header after the frame.
        let header = unsafe { chunk.bytes(at + FRAME, RECORD_HEADER) }
            .map(RecordHeader::decode)
            .expect("a published record is in its chunk")
            .expect("a writer writes whole records");
        // SAFETY: as above, for the payload after the header, whose length that header gives.
        let payload = unsafe { chunk.bytes(at + FRAME + RECORD_HEADER, header.stored) }
            .expect("a published record is in its chunk");
        assert!(
            unit as usize <= self.tiers[0].units.len(),
            "a put into unit {unit}, which this store did not give"
        );
        let here = Seg {
            chunk: at_chunk,
            segment: number as u32,
            offset: at + FRAME,
            kind: header.kind,
            form: header.form,
            stored: header.stored,
            raw: header.raw,
        };
        // Invariants: a key a durable payload names resolves after a crash, and a record goes
        // into a pack only as a key's home, so a pack's dead bytes are its records no key
        // finds, each counted once, by the settle that replaces or rewrites it. A duplicate (a
        // copy of a record already drained, a record drained after its key's home, or the
        // record a home drained first stands for) is not written. Counted dead, a duplicate
        // could be what a reopen finds (the last record of a key past the index), and its next
        // replace would count it again, until the pack's dead bytes pass its length and its
        // head no longer opens (`Head::decode`).
        let home = match copy {
            true => Ok(self.copy_home(number, at_chunk, at, &header.key, batch)),
            false => self.record_home(number, at_chunk, at, &header, here, batch),
        };
        let from = match home {
            Ok(Some(from)) => from,
            Ok(None) => return FRAME + header.span(),
            Err(why) => {
                // clone: the error that breaks the store
                let dir = self.tiers[0].dir.clone();
                self.fail(dir, why);
                return FRAME + header.span();
            }
        };
        let loc = if header.form == Form::External && header.stored > 0 {
            let own = &mut self.tiers[0];
            let file = own.file(&disk::external_file(&header.content));
            own.externals.entry(header.content).or_insert_with(|| External {
                len: header.raw,
                file,
                mapping: None,
                // alloc: a value of `EXTERNAL_FROM` bytes or more, held past its segment's chunk until its own file is written and mapped
                memory: Some(payload.to_vec()),
                committed: false,
            });
            self.append_record(Unit(unit), RecordHeader { stored: 0, ..header }, &[])
        } else {
            self.append_record(Unit(unit), header, payload)
        };
        if copy {
            // A copy is written because its key's record was another put's, not yet durable,
            // which may be only in that writer's unpublished memory: this copy is then the
            // key's one durable record. It is the key's home (a move, like a real record's), so
            // the index and a retire's rewrite (`live_records`), which keep only what a key
            // finds, keep it: found only by the scan past the index, it would be lost to the
            // next index write or retire.
            self.copy_homes.insert(header.key, from);
        } else {
            if from != here {
                // A record that is the home of another writer's record of the same value, not
                // drained yet (`record_home`): that record, drained after, is the duplicate.
                self.copy_homes.insert(header.key, from);
            }
            if number != 0 && header.kind.cold() && header.form == Form::Raw {
                // The store's own puts were sampled as they were put.
                self.sample(header.kind, header.content, payload);
            }
        }
        batch.moves.push(Move { key: header.key, from, to: loc });
        FRAME + header.span()
    }

    /// Where a writer's record at `here` moves from; `None` when it is a duplicate, never
    /// written to a pack. Judged by the first table holding the key, as a reader finds it
    /// ([`Look::find`]):
    ///
    /// - It points at `here`: the record is the key's home (`prev`'s spot at `here`, during an
    ///   index swap, is taken by the same move).
    /// - It points at another segment place with the same value, whose record is not drained
    ///   yet and no copy stands for: this record is the home of that place, as a copy is
    ///   (`copy_home`), and the record there, drained after, is the duplicate (`copy_homes`).
    ///   A claim reaches this only from a view `end_migration` left stale (`Own::claim`).
    /// - Otherwise the key's home is elsewhere: a copy of it waiting (`copy_homes`), a record
    ///   drained before (a settle's or another writer's, its move waiting), or a pack (a move
    ///   made, durable). The record is a duplicate. So is a record of another value than the
    ///   one the key keeps: its put was refused (a stale view's claim, as above).
    ///
    /// An error where the claim's rule is broken: the key is in no table (a kept record's key
    /// names it until its move), or `prev` points at `here` and the delta elsewhere (a claim
    /// goes into `prev` and then the delta, and the swap and a move keep the two at one place).
    /// A duplicate written for `prev`'s spot would be dropped with it by `end_migration`, and
    /// skipped, the key could be left with no durable record.
    fn record_home(
        &self,
        number: usize,
        at_chunk: ChunkRef,
        at: u64,
        header: &RecordHeader,
        here: Seg,
        batch: &Batch,
    ) -> Result<Option<Seg>, String> {
        let key = &header.key;
        if self.copy_homes.get(key) == Some(&here) {
            return Ok(None);
        }
        let held = Held::writer(&*self.shared);
        let own = &self.tiers[0];
        let mut spots = std::iter::once(&own.delta)
            .chain(&own.prev)
            .filter_map(|table| table.get(held, key).map(|(_, spot)| spot));
        let Some(first) = spots.next() else {
            // alloc: the error that breaks the store
            return Err(format!(
                "segment {number}: its record of {:x} at {} drained with the key in neither the delta nor `prev`: \
                 a kept record's key points at it until its move (`Own::claim`)",
                key.0,
                Place::Segment(here),
            ));
        };
        if first.place == Place::Segment(here) {
            return Ok(Some(here));
        }
        if spots.any(|it| it.place == Place::Segment(here)) {
            // alloc: the error that breaks the store
            return Err(format!(
                "segment {number}: its record of {:x} at {} drained with `prev` at it and the delta at {}: \
                 a claim during an index swap goes into `prev` and then the delta, so the two hold one place (`Own::claim`)",
                key.0,
                Place::Segment(here),
                first.place,
            ));
        }
        Ok(match first.place {
            Place::Segment(other)
                if first.content == header.content
                    && !self.drained(number, at_chunk, at, other, batch)
                    && !self.copy_homes.contains_key(key) =>
            {
                Some(other)
            }
            _ => None,
        })
    }

    /// Where a drained copy of `key` moves from: the record of another put it copies, which
    /// the key points at (see [`segment::COPY`]). `None` when the copy is a duplicate: the key
    /// points at a pack (durable, found there after a crash) or is only in the index, the
    /// record it points at was drained already (its move waits in a batch committed with or
    /// before this copy's), or another record stands in for it (`copy_homes`). `at` is where
    /// the drain of segment `number`, in `at_chunk`, is ([`Store::drained`]).
    fn copy_home(
        &self,
        number: usize,
        at_chunk: ChunkRef,
        at: u64,
        key: &Key,
        batch: &Batch,
    ) -> Option<Seg> {
        let held = Held::writer(&*self.shared);
        let own = &self.tiers[0];
        // The first table that holds the key, as a reader finds it.
        let place = std::iter::once(&own.delta)
            .chain(&own.prev)
            .find_map(|table| table.get(held, key).map(|(_, spot)| spot.place));
        let Some(Place::Segment(seg)) = place else { return None };
        (!self.drained(number, at_chunk, at, seg, batch) && !self.copy_homes.contains_key(key))
            .then_some(seg)
    }

    /// Whether the record at `seg` was drained: its move (or a copy's standing for it) waits
    /// in a batch committed with or before the one draining. `at` is where the drain of
    /// segment `number`, in `at_chunk`, is: its own records before `at` are drained.
    fn drained(&self, number: usize, at_chunk: ChunkRef, at: u64, seg: Seg, batch: &Batch) -> bool {
        let segment = seg.segment as usize;
        let drain = &self.drains[segment];
        if segment == number && seg.chunk == at_chunk {
            seg.offset < at
        } else if seg.chunk == drain.chunk {
            seg.offset < drain.at
        } else {
            // A chunk behind the drain is in the batch that drained it to its end, until that
            // batch's moves are made; one ahead of it is in no batch.
            self.pending
                .iter()
                .chain([batch])
                .flat_map(|it| &it.chunks)
                .any(|&(it, chunk)| it == segment && chunk == seg.chunk)
        }
    }

    /// Makes the moves of every batch whose mappings have all come, oldest first: publishes a
    /// view with the mappings, points each key at its pack, and frees the old spots and the
    /// chunks drained to their end at task boundaries.
    fn finish_batches(&mut self) {
        // A broken store makes no more moves: a batch whose move was lost (below) left its
        // chunks linked, so the next batch's chunks are not first in their segments.
        if self.broken.is_some() {
            return;
        }
        if self.pending.front().is_some_and(|it| it.maps == 0) {
            self.publish();
            // alloc: one list a commit of the chunks drained, held in the limbo past their grace
            let mut chunks = Vec::new();
            // A move not made whole stops the store below (`Lost`).
            let mut lost = None;
            while lost.is_none() && self.pending.front().is_some_and(|it| it.maps == 0) {
                let batch = self.pending.pop_front().expect("a batch at the front");
                let held = Held::writer(&*self.shared);
                let Tier { delta, prev, packs, .. } = &mut self.tiers[0];
                for it in &batch.moves {
                    let pack = &packs[it.to.pack as usize];
                    if pack.removed || pack.retiring {
                        lost.get_or_insert(Lost::Retired {
                            batch: batch.id,
                            key: it.key,
                            to: it.to,
                        });
                        continue;
                    }
                    // A move that takes no spot is of a record whose key left its place another
                    // way than by a move. None is known to: a record is written only as a home
                    // (`record_home`), and `end_migration` drops no spot of `prev` the delta
                    // does not hold at the same place. Were one made, its bytes are not counted
                    // dead: a reopen reads it past the index and may find its key there, and a
                    // replace would then count it twice. So dead bytes never pass the records
                    // no key finds.
                    // The spot swapped out stays readable in its table until a collect after
                    // its grace, which begins after this swap and before the chunks'.
                    let in_prev =
                        prev.as_deref().and_then(|table| Some((table, it.make_in(table, held)?)));
                    if it.make_in(delta, held).is_none()
                        && let Some((prev, (row, spot))) = in_prev
                    {
                        #[cfg(test)]
                        if let Some(hook) = &mut self.move_probed {
                            hook(it.key);
                        }
                        // A move of `prev`'s spot during a swap puts the pack spot into the
                        // delta too, where a claim that lost `prev`'s slot before the move may
                        // put the segment spot (`Own::lost_in_prev`): the two meet at the
                        // delta's one slot, so the delta never keeps a spot into a chunk this
                        // batch frees. A copy put first is replaced here; put after, it is
                        // refused and answers with this spot.
                        match delta.insert(held, spot.at(Place::Pack(it.to))) {
                            Ok((at, _)) => carry_flags(prev, row, delta, at),
                            Err((_, (_, there))) if it.takes(there) => {
                                _ = delta.replace(held, there.at(Place::Pack(it.to)));
                            }
                            // Another place: a claim copies only `prev`'s spot and every move
                            // takes `prev` first, so the delta holds a key only where `prev`
                            // does, and nothing else puts it there during a swap (`rehome`
                            // waits for the swap's end). Readers find the delta first, so the
                            // store stops now, naming the move.
                            Err((_, (_, there))) => {
                                lost.get_or_insert(Lost::Apart {
                                    batch: batch.id,
                                    key: it.key,
                                    from: it.from,
                                    to: it.to,
                                    place: there.place,
                                    content: there.content,
                                });
                            }
                        }
                    }
                    if self.copy_homes.get(&it.key) == Some(&it.from) {
                        self.copy_homes.remove(&it.key);
                    }
                }
                if lost.is_some() {
                    // The lost move's key still points into this batch's chunks: they stay
                    // linked, and the batch is not counted packed. No later batch is taken.
                    break;
                }
                for &(number, chunk) in &batch.chunks {
                    chunks.push(self.drains[number].segment.unlink_first(chunk));
                }
                self.epoch = batch.id + 1;
                if let Some(keys) = &mut self.new_keys {
                    keys.extend(batch.moves.iter().map(|it| it.key));
                }
            }
            self.limbo.hold(Grace::start(), chunks);
            match lost {
                None => {}
                Some(Lost::Retired { batch, key, to }) => {
                    // clone: the error that breaks the store
                    let file = self.tiers[0].packs[to.pack as usize].file.clone();
                    return self.fail(
                        file.clone(),
                        // alloc: the error that breaks the store
                        format!(
                            "batch {batch} moved {:x} into {file} after a settle retired it",
                            key.0
                        ),
                    );
                }
                Some(Lost::Apart { batch, key, from, to, place, content }) => {
                    // clone: the error that breaks the store
                    let dir = self.tiers[0].dir.clone();
                    return self.fail(
                        dir,
                        // alloc: the error that breaks the store
                        format!(
                            "batch {batch} moved {:x} from {} to {} during an index swap, and the delta holds it \
                             as {content:x} at {place}: the delta holds a key only where `prev` does, and every \
                             move takes `prev`'s spot first (`Store::finish_batches`)",
                            key.0,
                            Place::Segment(from),
                            Place::Pack(to),
                        ),
                    );
                }
            }
        }
        self.try_flip();
    }

    /// Puts the new index table in use once every record it holds is mapped: a new delta takes
    /// what the table does not hold, and the old one stays readable as `prev` while a writer
    /// that loaded the view before may still put into it.
    fn try_flip(&mut self) {
        let Some((_, covers)) = &self.flip else { return };
        if self.pending.front().is_some_and(|it| it.id <= *covers) {
            return;
        }
        let (index, _) = self.flip.take().expect("a flip waiting");
        let held = Held::writer(&*self.shared);
        let own = &mut self.tiers[0];
        let fresh: Table<Spot> = Table::new(DELTA_FIRST, FLAGS);
        let table = index.table();
        for (row, spot) in own.delta.iter(held) {
            let indexed = matches!(
                (spot.place, table.get(&spot.key)),
                (Place::Pack(loc), Some((_, Ok(at)))) if at == loc
            );
            if !indexed && let Ok((at, _)) = fresh.insert(Held::writer(&fresh), spot.at(spot.place))
            {
                carry_flags(&own.delta, row, &fresh, at);
            }
        }
        // alloc: once per index swap (every `INDEX_AFTER` records), the delta readers are handed
        let old = std::mem::replace(&mut own.delta, Arc::new(fresh));
        own.index = Some(index);
        let writers = self.drains.len() > 1;
        if writers {
            own.prev = Some(old);
        }
        self.periods = 0;
        self.index_mapping = false;
        self.publish();
        if writers {
            self.migrating = Some(Grace::start());
        }
    }

    /// Folds the delta an index swap left into the new one once no writer that loaded the view
    /// before the swap can put into it any more: what a writer put there after the swap is
    /// copied over. A key both hold is at one place in both (`Own::claim` goes into `prev` and
    /// then the delta, the swap copied `prev`'s spots, a move takes both): anything else breaks
    /// the store, naming the key, rather than drop `prev`'s spot, which may be the key's one
    /// durable record while the delta's is not yet durable.
    fn end_migration(&mut self) {
        let Some(grace) = self.migrating else { return };
        if !grace.passed() {
            return;
        }
        self.migrating = None;
        let held = Held::writer(&*self.shared);
        let own = &mut self.tiers[0];
        let Some(prev) = own.prev.take() else { return };
        let table = own.index.as_ref().map(|it| it.table());
        let mut split = None;
        for (row, spot) in prev.iter(held) {
            // The delta's spot of the key: found, or met by the copy, which a writer still in
            // the swap's view can beat (claiming the key in `prev` and then the delta, or
            // copying `prev`'s spot after it lost there).
            let kept = match own.delta.get(held, &spot.key) {
                Some((_, kept)) => kept,
                None => {
                    let indexed = matches!(
                        (spot.place, table.as_ref().and_then(|it| it.get(&spot.key))),
                        (Place::Pack(loc), Some((_, Ok(at)))) if at == loc
                    );
                    if indexed {
                        continue;
                    }
                    match own.delta.insert(held, spot.at(spot.place)) {
                        Ok((at, _)) => {
                            carry_flags(&prev, row, &own.delta, at);
                            continue;
                        }
                        Err((_, (_, kept))) => kept,
                    }
                }
            };
            if kept.place != spot.place || kept.content != spot.content {
                // alloc: the error that breaks the store
                split = Some(format!(
                    "the end of an index swap: {:x} is {:x} at {} in the delta and {:x} at {} in `prev`: \
                     a claim during the swap goes into `prev` and then the delta, so the two hold one place (`Own::claim`)",
                    spot.key.0, kept.content, kept.place, spot.content, spot.place
                ));
                break;
            }
        }
        if let Some(why) = split {
            // `prev` stays the tier's, so a reader still finds what it holds.
            own.prev = Some(prev);
            // clone: the error that breaks the store
            let dir = own.dir.clone();
            return self.fail(dir, why);
        }
        self.publish();
    }

    /// Publishes what readers see, and frees the view it replaces at task boundaries.
    fn publish(&mut self) {
        // clone: the view readers share, made once a publish (see `Tier::view`), with an `Arc` of the encodings
        let view = View {
            tiers: self.tiers.iter().map(Tier::view).collect(),
            columns: self.columns.clone(),
        };
        let old = self.shared.publish(view);
        self.limbo.retire(old);
    }

    /// What every `&mut` entry point does first, before it finds anything: frees what passed
    /// its grace, ends an index swap whose writers are gone, and lets go of the values made
    /// whole once they hold too much.
    fn tidy(&mut self) {
        // alloc: none, `Limbo::collect` frees what passed its grace; it is not `Iterator::collect`
        self.limbo.collect();
        let own = &self.tiers[0];
        for table in std::iter::once(&own.delta).chain(&own.prev) {
            // alloc: none, `Table::collect` drops swapped-out values and keeps their slots; it is not `Iterator::collect`
            table.collect();
        }
        self.end_migration();
        if self.shared.owned(Held::writer(&*self.shared)).bytes() > OWNED_MOST {
            let old = self.shared.fresh_owned();
            self.limbo.retire(old);
        }
    }

    /// zstd refusing a value is a broken build of it (a missing feature), not something a
    /// value can cause: the store stops rather than keep the value another way.
    fn cannot_compress(&mut self, kind: Kind, why: &str) {
        // clone: the error that breaks the store
        let dir = self.tiers[0].dir.clone();
        // alloc: the error that breaks the store
        self.fail(dir, format!("compressing a {} value: {why}", kind.name()));
    }

    fn dictionary_of(&self, kind: Kind) -> Option<u32> {
        self.tiers[0]
            .dictionaries
            .iter()
            .rposition(|it| it.kind == kind && it.bytes.is_ok())
            .map(|at| at as u32)
    }

    /// Keeps `bytes`, whose hash is `content`, as a sample of `kind` unless it is one already,
    /// and trains the kind's dictionary once the samples are [`Training::ready`]: or, when they
    /// are full and not ready, gives up training until the store is opened again.
    fn sample(&mut self, kind: Kind, content: ContentId, bytes: &[u8]) {
        let training = &mut self.training[kind.code() as usize];
        if !training.wants_samples() || !training.sampled.insert(content) {
            return;
        }
        // alloc: a training sample, at most `SAMPLES_MOST` a kind and only before its dictionary is trained
        training.samples.push(bytes.to_vec());
        training.bytes += bytes.len() as u64;
        if training.ready() {
            self.train(kind);
        } else if training.full() {
            // alloc: a problem reported, once a kind while the store is open
            let why = format!(
                "its samples are full ({} values, {} bytes) with fewer than the {} values a training needs",
                training.samples.len(),
                training.bytes,
                cold::TRAIN_VALUES.max(training.after),
            );
            self.give_up_training(kind, &why);
        }
    }

    /// No dictionary of `kind` is trained while the store is open: its samples are freed and
    /// no more are kept, and the problems say so, naming the kind. Its values are put raw and
    /// its records shorter than [`DEMOTE_FROM`] stay raw, until a store opened again trains.
    fn give_up_training(&mut self, kind: Kind, why: &str) {
        self.training[kind.code() as usize] = Training { given_up: true, ..Training::default() };
        // alloc: a problem reported
        self.problems.push(format!(
            "the {} values under {DEMOTE_FROM} bytes stay raw until the store is opened again: no dictionary is trained: {why}",
            kind.name()
        ));
    }

    fn train(&mut self, kind: Kind) {
        let trained = {
            let samples: Vec<&[u8]> =
                // alloc: once per dictionary trained, the samples as slices for zstd
                self.training[kind.code() as usize].samples.iter().map(Vec::as_slice).collect();
            cold::train(&samples).and_then(|dictionary| {
                cold::Prepared::new(dictionary)
                    // alloc: the error's message
                    .map_err(|why| {
                        format!("the dictionary trained on {} values {why}", samples.len())
                    })
            })
        };
        let training = &mut self.training[kind.code() as usize];
        match trained {
            Ok(prepared) => {
                *training = Training::default();
                let dictionary = prepared.bytes();
                let header = RecordHeader {
                    kind,
                    form: Form::Dictionary,
                    dictionary: 0,
                    stored: dictionary.len() as u64,
                    raw: dictionary.len() as u64,
                    key: Key(ContentId::of(DICTIONARY_DOMAIN, dictionary)),
                    content: content_of(dictionary),
                };
                let loc = self.append_record(Unit::SHARED, header, dictionary);
                self.tiers[0].dictionaries.push(Dictionary {
                    kind,
                    pack: loc.pack,
                    offset: loc.offset,
                    // alloc: once per dictionary trained
                    bytes: Ok(Arc::new(prepared)),
                });
                self.publish();
            }
            // Full samples get no more to train on twice as many: it cannot be tried again.
            Err(why) if training.full() => self.give_up_training(kind, &why),
            Err(why) => {
                training.after = training.samples.len() * 2;
                // alloc: a problem reported
                self.problems.push(format!("the {} values are kept raw: {why}", kind.name()));
            }
        }
    }

    fn start_commit(&mut self, effects: &mut Vec<Effect>) {
        debug_assert!(self.commit.is_none());
        // Every caller checks first but a retry of a failed commit; a broken store drains and
        // writes nothing more.
        if self.broken.is_some() {
            return;
        }
        // In a task of the store's tag, so the packs' bytes the drain copies are named on the
        // daemon's memory line, whichever thread commits.
        blaze_mem::frame::task(blaze_mem::Tag::Store, |_| self.drain());
        // A record the drain found against the claim's rule broke the store (`record_home`).
        if self.broken.is_some() {
            return;
        }
        // alloc: the error that breaks the store; `None` otherwise
        let failed = self.drains.iter().find_map(|it| it.segment.failed().map(str::to_owned));
        if let Some(why) = failed {
            // clone: the error that breaks the store
            let dir = self.tiers[0].dir.clone();
            return self.fail(dir, why);
        }
        for pack in &mut self.tiers[0].packs {
            if pack.end > pack.committed {
                let padded = pack.end.next_multiple_of(disk::BLOCK);
                pack.memory.resize(pack.memory.len() + (padded - pack.end) as usize, 0);
                pack.end = padded;
            }
        }
        let own = &self.tiers[0];
        let packs: Vec<(usize, u64)> = own
            .packs
            .iter()
            .enumerate()
            .filter(|(_, it)| it.end > it.committed)
            .map(|(at, it)| (at, it.end))
            // alloc: once per commit (a group of puts), the packs it writes, held in the `Commit`
            .collect();
        let mut externals: Vec<ContentId> =
            // alloc: once per commit, the files it writes, sorted, held in the `Commit`
            own.externals.iter().filter(|(_, it)| !it.committed).map(|(it, _)| *it).collect();
        externals.sort_unstable();
        let compact =
            !self.index_mapping && self.migrating.is_none() && own.delta.len() >= INDEX_AFTER;
        if packs.is_empty() && externals.is_empty() && !compact {
            // Nothing to write and no commit in flight: every record drained is in what a
            // commit wrote already, and a put that wrote none (it lost its key to another
            // writer's put of the same bytes, or panicked before it wrote) has nothing to make
            // durable. Its count is durable now, or it waits on a commit nothing starts.
            for (number, &puts) in self.draining.puts.iter().enumerate() {
                let segment = &self.drains[number].segment;
                if puts > segment.durable() {
                    segment.set_durable(puts);
                }
            }
            return;
        }
        let mut all_externals: Vec<ExternalRef> = own
            .externals
            .iter()
            .map(|(content, it)| ExternalRef { content: *content, len: it.len })
            // alloc: once per commit, the head it writes
            .collect();
        all_externals.sort_unstable_by_key(|it| it.content);
        let mut head = Head {
            generation: own.head.generation + 1,
            index: own.head.index,
            packs: own
                .packs
                .iter()
                .map(|it| PackRef {
                    unit: it.unit,
                    committed: it.end,
                    indexed: it.indexed,
                    dead: it.dead,
                    removed: it.removed || it.retiring,
                })
                // alloc: once per commit, the head it writes
                .collect(),
            dictionaries: own
                .dictionaries
                .iter()
                .map(|it| DictionaryRef { kind: it.kind, pack: it.pack, offset: it.offset })
                // alloc: once per commit, the head it writes
                .collect(),
            externals: all_externals,
            // clone: once per commit, the head it writes
            units: own.units.clone(),
        };
        let mut index = None;
        if compact {
            // alloc: once per index written (every `INDEX_AFTER` records); empty unless a slot is not whole
            let mut bad = Vec::new();
            let entries = self.index_entries(&mut bad);
            for (_, what) in bad {
                self.problems
                    // alloc: a problem reported
                    .push(format!(
                        "a slot of {} left out of the next index: {what}",
                        own.index_file()
                    ));
            }
            let bytes = index::build_index(&entries);
            let slot = match own.head.index {
                Some(it) => 1 - it.slot,
                None => 0,
            };
            head.index = Some(IndexRef {
                slot,
                len: bytes.len() as u64,
                check: crc::crc32c(&bytes),
                count: entries.len() as u64,
            });
            for pack in &mut head.packs {
                pack.indexed = pack.committed;
            }
            index = Some((slot, bytes));
        }
        let mut steps = 0;
        for &(at, _) in &packs {
            let pack = &own.packs[at];
            let tag = self.tags.ask(Op::Truncate { pack: at });
            // clone: the effect owns its file name; once per pack a commit
            effects.push(Effect::Truncate { file: pack.file.clone(), len: pack.committed, tag });
            steps += 1;
        }
        for content in &externals {
            let external = &own.externals[content];
            // clone: the write owns its bytes, and a commit that fails writes them again from these
            let bytes = external.memory.clone().expect("a file not yet written is held");
            let tag = self.tags.ask(Op::WriteExternal { content: *content });
            // clone: the effect owns its file name; once per file a commit
            effects.push(Effect::Write { file: external.file.clone(), bytes, tag });
            steps += 1;
        }
        let mut batch = std::mem::take(&mut self.draining);
        batch.id = self.next_batch;
        self.next_batch += 1;
        let index = index.map(|(slot, bytes)| {
            let tag = self.tags.ask(Op::WriteIndex);
            effects.push(Effect::Write { file: own.file(&disk::index_file(slot)), bytes, tag });
            steps += 1;
            (slot, batch.id)
        });
        self.commit = Some(Commit {
            packs,
            externals,
            index,
            head,
            batch,
            steps,
            failed: None,
            head_sent: false,
        });
    }

    /// What the next index table holds: every own record a lookup finds, at its place in a
    /// pack. A record copied out of its segment into a pack of this commit or an earlier one is
    /// held at its pack, though its key still points at the segment; one still only in a
    /// segment is not held, nor does it shadow the record the table held before. A record
    /// standing in as the home of a key still in another writer's segment (a copy, or a
    /// record of the same value, `copy_homes`) is held so: the head this index goes with scans
    /// no pack below what it indexes. Of a key's waiting moves,
    /// the record held is the one `finish_batches` makes: the first that takes the key's spot
    /// ([`Move::takes`]), never a key's first move from a place it left nor a later one from
    /// where it is. A slot that is not whole is in `bad`.
    fn index_entries(&self, bad: &mut Vec<(Key, &'static str)>) -> Vec<(Key, Loc)> {
        // The moves and the keys taken live only while the entries are listed: a frame's.
        blaze_mem::frame::task(blaze_mem::Tag::Store, |frame| {
            // Each key's waiting moves, oldest batch first and in drain order, as they are made.
            let mut moves: HashMap<Key, Vec<&Move, &blaze_mem::Frame>, &blaze_mem::Frame> =
                HashMap::new_in(frame);
            for it in self.pending.iter().chain([&self.draining]).flat_map(|it| &it.moves) {
                moves.entry(it.key).or_insert_with(|| Vec::new_in(frame)).push(it);
            }
            let held = Held::writer(&*self.shared);
            let own = &self.tiers[0];
            let indexed = own.index.as_ref().map_or(0, |it| it.table().count() as usize);
            let mut out = Vec::with_capacity(own.delta.len() + indexed);
            let mut taken = HashSet::new_in(frame);
            for (_, spot) in own.delta.iter(held) {
                let loc = match spot.place {
                    Place::Pack(loc) => Some(loc),
                    Place::Segment(_) => moves
                        .get(&spot.key)
                        .and_then(|it| it.iter().find(|it| it.takes(spot)))
                        .map(|it| it.to),
                };
                if let Some(loc) = loc {
                    taken.insert(spot.key);
                    out.push((spot.key, loc));
                }
            }
            if let Some(index) = &own.index {
                for (_, key, loc) in index.table().iter() {
                    match loc {
                        Ok(_) if taken.contains(&key) => {}
                        Ok(loc) => out.push((key, loc)),
                        Err(what) => bad.push((key, what)),
                    }
                }
            }
            out
        })
    }

    fn step(&mut self, op: &Op, done: &Done, effects: &mut Vec<Effect>) -> Vec<Ticket> {
        // alloc: empty, allocates nothing
        let Some(commit) = self.commit.as_mut() else { return Vec::new() };
        commit.steps -= 1;
        self.commit_steps += 1;
        let own = &self.tiers[0];
        let file_of = |op: &Op| match op {
            Op::Truncate { pack } | Op::WritePack { pack } | Op::Sync { pack } => {
                // clone: the error of a step that failed
                own.packs[*pack].file.clone()
            }
            // clone: the error of a step that failed
            Op::WriteExternal { content } => own.externals[content].file.clone(),
            Op::WriteIndex => own.file(&disk::index_file(commit.index.map_or(0, |it| it.0))),
            _ => own.file(HEAD_FILE),
        };
        match (op, done) {
            (Op::Truncate { pack }, Done::Truncated) => {
                if commit.failed.is_none() {
                    let at = *pack;
                    let pack = &own.packs[at];
                    let to = commit.to(at).expect("a pack the commit truncated it writes");
                    let bytes = pack.memory
                        [(pack.committed - pack.usable) as usize..(to - pack.usable) as usize]
                        // alloc: the write owns its bytes, and the pack keeps its memory until a mapping covers it; once per pack a commit
                        .to_vec();
                    // `committed` and `to` are block ends (each commit pads its packs), so on a
                    // driver that can, a long write skips the page cache.
                    let tag = self.tags.ask(Op::WritePack { pack: at });
                    // clone: the effect owns its file name; once per pack a commit
                    effects.push(Effect::WriteAt {
                        file: pack.file.clone(),
                        at: pack.committed,
                        bytes,
                        tag,
                    });
                    commit.steps += 1;
                }
            }
            (Op::WritePack { pack }, Done::Written) => {
                if commit.failed.is_none() {
                    let tag = self.tags.ask(Op::Sync { pack: *pack });
                    // clone: the effect owns its file name; once per pack a commit
                    effects.push(Effect::Sync { file: own.packs[*pack].file.clone(), tag });
                    commit.steps += 1;
                }
            }
            (Op::Sync { .. }, Done::Synced)
            | (Op::WriteExternal { .. } | Op::WriteIndex, Done::Written) => {}
            (Op::WriteHead, Done::Written) => return self.finish_commit(effects),
            (op, done) => {
                if commit.failed.is_none() {
                    commit.failed = Some(Broken { file: file_of(op), what: describe(done) });
                }
            }
        }
        if commit.steps > 0 {
            // alloc: empty, allocates nothing
            return Vec::new();
        }
        if let Some(failed) = commit.failed.take() {
            // What it drained is still in the packs' memory: the next commit writes it again.
            self.draining = self.commit.take().expect("a commit in flight").batch;
            self.failures += 1;
            if self.failures >= FAILURES_MOST {
                self.fail(failed.file, failed.what);
            } else {
                self.start_commit(effects);
            }
            // alloc: empty, allocates nothing
            return Vec::new();
        }
        // A commit in flight when the store broke writes no head: its packs' bytes are written
        // past what the last head names, which a reopen does not read as committed.
        if self.broken.is_some() {
            // alloc: empty, allocates nothing
            return Vec::new();
        }
        if !commit.head_sent {
            commit.head_sent = true;
            commit.steps += 1;
            let tag = self.tags.ask(Op::WriteHead);
            effects.push(Effect::Write {
                file: own.file(HEAD_FILE),
                bytes: commit.head.encode(),
                tag,
            });
        }
        // alloc: empty, allocates nothing
        Vec::new()
    }

    fn finish_commit(&mut self, effects: &mut Vec<Effect>) -> Vec<Ticket> {
        let commit = self.commit.take().expect("a commit in flight");
        let own = &mut self.tiers[0];
        for &(at, to) in &commit.packs {
            own.packs[at].committed = to;
        }
        for (pack, written) in own.packs.iter_mut().zip(&commit.head.packs) {
            pack.indexed = written.indexed;
        }
        own.head = commit.head;
        for at in 0..self.tiers[0].packs.len() {
            let pack = &mut self.tiers[0].packs[at];
            if std::mem::take(&mut pack.retiring) {
                pack.removed = true;
                pack.mapping = None;
                // alloc: empty, allocates nothing; it lets go of the pack's memory
                pack.memory = Vec::new();
                self.remove_pack(at, effects);
            }
        }
        let mut batch = commit.batch;
        for content in &commit.externals {
            let external =
                self.tiers[0].externals.get_mut(content).expect("a file the commit wrote");
            external.committed = true;
            // clone: the op and the effect own the name; once per file a commit
            let file = external.file.clone();
            self.ask_map(
                0,
                file,
                Mapped::External { content: *content, batch: Some(batch.id) },
                effects,
            );
        }
        for &(at, to) in &commit.packs {
            // clone: the op and the effect own the name; once per pack a commit
            let file = self.tiers[0].packs[at].file.clone();
            self.ask_map(
                0,
                file,
                Mapped::Pack { pack: at, usable: to, batch: Some(batch.id) },
                effects,
            );
        }
        batch.maps = commit.externals.len() + commit.packs.len();
        if let Some((slot, covers)) = commit.index {
            let file = self.tiers[0].file(&disk::index_file(slot));
            self.ask_map(0, file, Mapped::Index { covers: Some(covers) }, effects);
            self.index_mapping = true;
        }
        // alloc: the tickets returned, once per commit; empty when no put became durable
        let mut tickets = Vec::new();
        for (number, &puts) in batch.puts.iter().enumerate() {
            let segment = &self.drains[number].segment;
            let before = segment.durable();
            if puts > before {
                segment.set_durable(puts);
                tickets.extend((before + 1..=puts).map(|put| Ticket::of(number as u32, put)));
            }
        }
        self.pending.push_back(batch);
        self.finish_batches();
        self.failures = 0;
        // A break in `finish_batches` (a lost move) ends the commits: none starts after it.
        if std::mem::take(&mut self.flush_wanted) && self.broken.is_none() {
            self.start_commit(effects);
        }
        tickets
    }

    // Opening and mapping.

    /// The store broken, naming `file` and `what` went wrong (the first break is kept). Places
    /// held for the open's look through `placed/` are refused with it: the look never ends now.
    fn fail(&mut self, file: FileName, what: impl Into<String>) {
        self.broken.get_or_insert(Broken { file, what: what.into() });
        self.look_wait_over();
        self.held_unplaces.clear();
        if self.held.is_empty() {
            return;
        }
        let broken = self.broken.as_ref().map(ToString::to_string).unwrap_or_default();
        // alloc: once, as the store breaks
        let waiting =
            std::mem::take(&mut self.held).into_iter().map(|(_, waiter)| waiter).collect();
        self.refuse_waiting(waiting, &broken);
    }

    /// Whether to ask again after IO failed.
    fn retry(&mut self, file: &FileName, done: &Done) -> bool {
        self.failures += 1;
        if self.failures >= FAILURES_MOST {
            // clone: the error that breaks the store
            self.fail(file.clone(), describe(done));
            return false;
        }
        true
    }

    fn ask_map(&mut self, tier: usize, file: FileName, what: Mapped, effects: &mut Vec<Effect>) {
        // clone: the op keeps the name to match `Store::mapped` and to ask again; once per file mapped
        let tag = self.tags.ask(Op::Map { tier, file: file.clone(), what });
        effects.push(Effect::Map { file, tag });
    }

    fn read_head(&mut self, at: usize, bytes: &[u8], effects: &mut Vec<Effect>) {
        let file = self.tiers[at].file(HEAD_FILE);
        let head = match (bytes.is_empty(), at) {
            (true, 0) => Head::default(),
            (true, at) if self.is_peer(at) => Head::default(),
            (true, _) => return self.fail(file, "it is not there: a layer is a store"),
            (false, _) => match Head::decode(bytes) {
                Ok(head) => head,
                Err(why) => return self.fail(file, why),
            },
        };
        if head.packs.len() > PACKS_MOST {
            // alloc: the error that breaks the store
            return self.fail(file, format!("it names more than {PACKS_MOST} packs"));
        }
        let tier = &mut self.tiers[at];
        let packs: Vec<Pack> = head
            .packs
            .iter()
            .enumerate()
            .map(|(number, it)| Pack {
                unit: it.unit,
                file: tier.file(&disk::pack_file(number)),
                mapping: None,
                usable: it.committed,
                // alloc: empty, allocates nothing
                memory: Vec::new(),
                committed: it.committed,
                indexed: it.indexed,
                end: it.committed,
                dead: it.dead,
                removed: it.removed,
                retiring: false,
            })
            // alloc: opening, once per tier
            .collect();
        let externals: ByHash<ContentId, External, Global> = head // alloc: the store's own table, for its life
            .externals
            .iter()
            .map(|it| {
                (
                    it.content,
                    External {
                        len: it.len,
                        file: tier.file(&disk::external_file(&it.content)),
                        mapping: None,
                        memory: None,
                        committed: true,
                    },
                )
            })
            // alloc: opening, once per tier
            .collect();
        // alloc: opening, once per tier, the mappings it asks for
        let mut maps = Vec::new();
        if let Some(index) = head.index {
            maps.push((tier.file(&disk::index_file(index.slot)), Mapped::Index { covers: None }));
        }
        for (pack, it) in packs.iter().enumerate().filter(|(_, it)| !it.removed) {
            // clone: opening, once per pack; the op and the effect own the name
            maps.push((it.file.clone(), Mapped::Pack { pack, usable: it.committed, batch: None }));
        }
        let removed: Vec<usize> =
            // alloc: opening, once per tier
            packs.iter().enumerate().filter(|(_, it)| it.removed).map(|(at, _)| at).collect();
        // alloc: opening, once per tier, sorted so the mappings are asked for in one order
        let mut contents: Vec<&ContentId> = externals.keys().collect();
        contents.sort_unstable();
        for content in contents {
            // clone: opening, once per external; the op and the effect own the name
            maps.push((
                externals[content].file.clone(),
                Mapped::External { content: *content, batch: None },
            ));
        }
        tier.dictionaries = head
            .dictionaries
            .iter()
            .map(|it| Dictionary {
                kind: it.kind,
                pack: it.pack,
                offset: it.offset,
                bytes: Err("was not read".into()),
            })
            // alloc: opening, once per tier
            .collect();
        tier.packs = packs;
        tier.externals = externals;
        // alloc: opening, once per tier
        tier.unit_of =
            head.units.iter().enumerate().map(|(at, it)| (*it, Unit(at as u32 + 1))).collect();
        // clone: opening, once per tier
        tier.units = head.units.clone();
        tier.head = head;
        tier.head_read = true;
        tier.waiting = maps.len();
        for (file, what) in maps {
            self.ask_map(at, file, what, effects);
        }
        if at == 0 {
            for pack in removed {
                self.remove_pack(pack, effects);
            }
        }
        if self.tiers[at].waiting == 0 {
            self.finish_open(at);
        }
    }

    fn install(&mut self, at: usize, file: FileName, what: Mapped, mapping: Mapping) {
        let opening = !self.tiers[at].opened;
        let len = mapping.bytes().len() as u64;
        let tier = &mut self.tiers[at];
        match what {
            Mapped::Pack { pack, usable, batch } => {
                if len < usable {
                    return self.tier_failed(
                        at,
                        file,
                        // alloc: the error that breaks the store
                        format!("it is {len} bytes, shorter than the {usable} its head names"),
                    );
                }
                let pack = &mut tier.packs[pack];
                if !pack.removed && usable >= pack.usable {
                    pack.memory.drain(..(usable - pack.usable) as usize);
                    pack.usable = usable;
                    pack.mapping = Some(mapping);
                }
                self.mapped_for(batch);
            }
            Mapped::External { content, batch } => {
                if let Some(external) = tier.externals.get_mut(&content) {
                    let named = external.len;
                    if len != named {
                        return self.tier_failed(
                            at,
                            file,
                            // alloc: the error that breaks the store
                            format!("it is {len} bytes, not the {named} its head names"),
                        );
                    }
                    external.mapping = Some(mapping);
                    external.memory = None;
                }
                self.mapped_for(batch);
            }
            Mapped::Index { covers } => {
                let index = match Index::new(mapping) {
                    // alloc: once per index table mapped (opening, or every `INDEX_AFTER` records)
                    Ok(index) => Arc::new(index),
                    Err(why) => return self.tier_failed(at, file, why.into()),
                };
                match covers {
                    None => tier.index = Some(index),
                    Some(covers) => {
                        self.flip = Some((index, covers));
                        self.try_flip();
                    }
                }
            }
        }
        self.failures = 0;
        if opening {
            let tier = &mut self.tiers[at];
            tier.waiting -= 1;
            if tier.waiting == 0 && tier.head_read {
                self.finish_open(at);
            }
        }
    }

    fn finish_open(&mut self, at: usize) {
        match open_tier(&mut self.tiers[at]) {
            Ok(()) => {
                let tier = &mut self.tiers[at];
                tier.opened = true;
                tier.before = None;
                self.publish();
                if self.is_peer(at) {
                    self.peer_settled(at);
                }
            }
            Err((file, why)) => self.tier_failed(at, file, why),
        }
    }

    /// One mapping of batch `batch` came: its moves are made once all have.
    fn mapped_for(&mut self, batch: Option<u64>) {
        let Some(id) = batch else { return };
        if let Some(batch) = self.pending.iter_mut().find(|it| it.id == id) {
            batch.maps -= 1;
        }
        self.finish_batches();
    }

    /// A fetch's answer, as its frame came (its length, then the answer), read in place: the
    /// value is hashed once, and copied once, into the store's segment.
    fn take_fetched(&mut self, key: Key, admit: Admit, done: Done) -> Fetched {
        let frame = match done {
            Done::Read(frame) => frame,
            done => return Fetched::Refused(describe(&done)),
        };
        let Some(answer) = frame.get(8..) else {
            return Fetched::Refused("its frame is shorter than its length".into());
        };
        match remote::parse(answer) {
            Err(why) => Fetched::Refused(why),
            Ok(remote::Parsed::Missing { key: it }) if it == key => Fetched::Missing,
            Ok(remote::Parsed::Value { key: it, kind: Kind::Machine, .. }) if it == key => Fetched::Refused(
                "it is machine code, which the daemon runs as it is: only the daemon's own compile keeps machine code, never a fetch"
                    .into(),
            ),
            Ok(remote::Parsed::Value { key: it, kind, content, bytes }) if it == key => {
                let found = content_of(bytes);
                if found != content {
                    // alloc: the error of a refused fetch
                    return Fetched::Refused(format!(
                        "its bytes hash to {found:x}, not the {content:x} they were sent as"
                    ));
                }
                if !self.tiers[0].opened || self.broken.is_some() {
                    return Fetched::Refused("the store is not open".into());
                }
                if let Err(why) = admit(kind, bytes) {
                    return Fetched::Refused(why);
                }
                match self.put_hashed(Unit::SHARED, key, kind, content, bytes) {
                    Ok(ticket) => Fetched::Kept(ticket),
                    // alloc: the error of a refused fetch
                    Err(mismatch) => Fetched::Refused(format!(
                        "the store keeps {:x} under it, not the {:x} sent",
                        mismatch.kept, mismatch.given
                    )),
                }
            }
            Ok(_) => Fetched::Refused("the answer is for another key".into()),
        }
    }
}

impl Drop for Store {
    /// A reader or writer may outlive the store and still be in a task that loaded what its
    /// limbo holds: the limbo goes with what they share, dropped when the last lets go.
    fn drop(&mut self) {
        self.shared.orphan(std::mem::take(&mut self.limbo));
    }
}

/// Checks what a tier's head names against what was mapped, and reads the records past its
/// index table and its dictionaries.
fn open_tier(tier: &mut Tier) -> Result<(), (FileName, String)> {
    if let Some(index) = tier.head.index {
        let file = tier.index_file();
        // clone: the error that stops the opening
        let mapped =
            tier.index.as_ref().ok_or_else(|| (file.clone(), "it was not mapped".to_owned()))?;
        let bytes = mapped.mapping.bytes();
        if bytes.len() as u64 != index.len {
            // alloc: the error that stops the opening
            return Err((file, format!("it is not the {} bytes its head names", index.len)));
        }
        let check = crc::crc32c(bytes);
        if check != index.check {
            // alloc: the error that stops the opening
            return Err((
                file,
                format!("its CRC-32C is {check:08x}, not the {:08x} its head names", index.check),
            ));
        }
        let count = mapped.table().count();
        if count != index.count {
            // alloc: the error that stops the opening
            return Err((
                file,
                format!("it holds {count} values, not the {} its head names", index.count),
            ));
        }
    }
    // alloc: opening, once per tier, the records past its index table
    let mut found = Vec::new();
    for (number, pack) in tier.packs.iter().enumerate().filter(|(_, it)| !it.removed) {
        // clone: the error that stops the opening
        let fail = |why: String| (pack.file.clone(), why);
        let all = pack.mapping.as_ref().ok_or_else(|| fail("it was not mapped".into()))?.bytes();
        let all = usize::try_from(pack.committed)
            .ok()
            .and_then(|committed| all.get(..committed))
            .ok_or_else(
            // alloc: the error that stops the opening
            || {
                fail(format!(
                    "it is {} bytes, shorter than the {} its head names",
                    all.len(),
                    pack.committed
                ))
            },
        )?;
        disk::check_pack_header(all, pack.unit).map_err(|why| fail(why.into()))?;
        let mut at = pack.indexed.max(PACK_HEADER);
        while at < pack.committed {
            if all.get(at as usize..at as usize + 4).is_none_or(|start| start == [0; 4]) {
                let next = at.next_multiple_of(disk::BLOCK);
                if next == at || all[at as usize..next as usize].iter().any(|&it| it != 0) {
                    // alloc: the error that stops the opening
                    return Err(fail(format!(
                        "at {at} is neither a record nor the zeros that end a commit"
                    )));
                }
                at = next;
                continue;
            }
            let header = RecordHeader::decode(&all[at as usize..])
                // alloc: the error that stops the opening
                .map_err(|why| fail(format!("the record at {at}: {why}")))?;
            let end =
                at.checked_add(header.span()).filter(|&end| end <= pack.committed).ok_or_else(
                    // alloc: the error that stops the opening
                    || {
                        fail(format!(
                            "the record at {at} runs past the {} bytes its head names",
                            pack.committed
                        ))
                    },
                )?;
            if header.form != Form::Dictionary {
                let loc = Loc {
                    pack: number as u32,
                    kind: header.kind,
                    form: header.form,
                    offset: at,
                    stored: header.stored,
                    raw: header.raw,
                };
                found.push(Spot::new(
                    header.key,
                    header.content,
                    Place::Pack(loc),
                    Ticket::DURABLE,
                ));
            }
            at = end;
        }
    }
    let delta: Table<Spot> = Table::new(DELTA_FIRST.max(found.len() * 2), FLAGS);
    let held = Held::writer(&delta);
    // A settle writes a key's new record after its old one, so the last of a key is its live
    // one: inserted first, it keeps the key.
    for spot in found.into_iter().rev() {
        _ = delta.insert(held, spot);
    }
    // alloc: opening, once per tier
    tier.delta = Arc::new(delta);
    let packs = &tier.packs;
    for dictionary in &mut tier.dictionaries {
        let pack = &packs[dictionary.pack as usize];
        let at = dictionary.offset;
        dictionary.bytes = pack
            .bytes(at, RECORD_HEADER)
            // alloc: the error of a dictionary that cannot be read
            .ok_or_else(|| format!("at {at} is outside {}", pack.file))
            // alloc: the error of a dictionary that cannot be read
            .and_then(|bytes| {
                RecordHeader::decode(bytes)
                    .map_err(|why| format!("at {at} of {}: {why}", pack.file))
            })
            .and_then(|header| {
                match header.form == Form::Dictionary && header.kind == dictionary.kind {
                    true => pack
                        .bytes(at + RECORD_HEADER, header.stored)
                        .filter(|bytes| content_of(bytes) == header.content)
                        // alloc: opening, once per dictionary, whose bytes the prepared dictionary keeps
                        .map(|bytes| bytes.to_vec())
                        // alloc: the error of a dictionary that cannot be read
                        .ok_or_else(|| format!("at {at} of {} is not what was recorded", pack.file))
                        .and_then(|bytes| {
                            cold::Prepared::new(bytes)
                                .map(Arc::new)
                                // alloc: the error of a dictionary that cannot be read
                                .map_err(|why| format!("at {at} of {} {why}", pack.file))
                        }),
                    // alloc: the error of a dictionary that cannot be read
                    false => {
                        Err(format!("at {at} of {} is not a dictionary of its kind", pack.file))
                    }
                }
            });
    }
    Ok(())
}

/// A put of a key the store has with other bytes.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Mismatch {
    pub key: Key,
    pub kept: ContentId,
    pub given: ContentId,
}

/// The remote tier, for machines outside the pool (a laptop): read-through, never on the
/// VM's hot path. A value from it is verified as any other before it is kept.
pub trait Remote {
    /// Asks for the value of `key`; when it says [`Asked::Sent`], the answer comes as a
    /// completion of `tag`.
    fn fetch(&mut self, key: &Key, tag: Tag, effects: &mut Vec<Effect>) -> Asked;
}

/// What a [`Remote`] did with a fetch.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Asked {
    /// It asked: the answer comes as a completion of the fetch's tag.
    Sent,
    /// It knows without asking that the far store has no value of the key, as of what it last
    /// heard from it, and asked nothing.
    Missing,
}
