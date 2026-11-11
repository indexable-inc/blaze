//! blaze v2's store (docs/v2/plan/design/store.md). Replaces `blaze-store`.
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
//!   directory. Each pack has a CRC-32C per 4 KiB block beside it (`checks-<n>`, [`checks`]),
//!   durable before the head that names the blocks: the open checks every block its recovery
//!   scan reads, and a reload into the store's buffer pool is checked by them
//!   (docs/v2/plan/design/store-pool.md).
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
//! The layout on disk and why it survives a crash at any point: docs/v2/plan/design/store.md.
//!
//! Its in-memory tables and segments sit on `blaze_mem` regions tagged `Tag::Store`
//! (docs/v2/plan/design/memory.md): a lookup that hits and a warm put allocate nothing.

#![feature(allocator_api)]
// `#![blaze::sans_io]` marks a sans-IO core for `blaze::sans_io_core` (the pool's loads and sweep).
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]

mod checks;
pub mod chunk;
mod cold;
mod copy;
pub mod derived;
use blaze_ids::crc32c as crc;
mod disk;
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
mod driven;
mod flags;
mod handle;
#[cfg_attr(
    not(test),
    allow(dead_code, reason = "docs/v2/plan/design/store-pool.md: a pinned read through a pump (`Hot::pin_pack`) is a test's on the simulation; the index slots and files of their own are not on the pool yet")
)]
mod hot;
mod index;
#[cfg(all(test, blaze_check))]
mod model;
mod peers;
// The store's buffer pool (docs/v2/plan/design/store-pool.md): its core and its DST. Its atomics
// are `std`'s under the model checker too (`cfg(blaze_check)`): they are laid over reserved
// memory, which the model checker's do not model, so a model run does not schedule the pool's
// state words or its pins. Their proof is the pool's own DST (`pool/tests.rs`), not a model.
#[cfg_attr(
    any(not(test), blaze_check),
    allow(dead_code, reason = "lane 4b of docs/v2/plan/design/store-pool.md: the store commits through the pool (`hot`); its reads move onto it next, and its tests are not built under the model checker")
)]
mod pool;
mod publish;
mod pump;
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

use blaze_core::{Cache, Completion, Done, Durable, Effect, FileName, Mapping, Tag};
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
pub use crate::pool::counts::{PoolCounts, pool_counts};
pub use crate::pump::{Inline, Pump, PumpError};
pub use crate::read::Pinned;
pub use crate::publish::Sparse;
use crate::publish::{Content, Flushed, Patching};
use crate::read::{
    ExternalView, Index, Look, PackView, Place, Seg, Shared, Spot, TierView, View, carry_flags,
};
#[cfg(feature = "test-hooks")]
pub use crate::segment::HeldPut;
pub use crate::segment::Writer;
use crate::segment::{COPY, Chunk, ChunkRef, FRAME, Own, Segment};
use crate::table::{Held, Table};

/// The format of everything in a store's directory. A store of another refuses to open, naming
/// both; nothing reads it as this one, and there is no migration: the store is rebuilt from
/// what made it. Format 4 added each pack's block checks ([`checks`], `checks-<n>`).
pub(crate) const FORMAT: u32 = 4;

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
/// beside its packs (`placed/<content>`, [`Store::place`]), written the first time one is placed
/// and cloned from once it is written back: every build dir that holds the output after that,
/// a second checkout's among them, shares its blocks, and it is the base the next image
/// derived from this one is made of. (The places that come before it is written back are the
/// value's bytes written where they go: a clone of a file just written waits for all of it
/// to be written out.) A shorter one is always written where it goes: it is a few blocks, and
/// a file of its own would cost the store an inode each.
const CLONE_FROM: u64 = 16 << 10;

/// What the name of a derived value's whole file ends with, after its key
/// ([`Kind::Derived`]): a file so named is never taken from an earlier run.
const DERIVED_SUFFIX: &str = ".derived";

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
    /// The recipe of a whole output that is another the store holds with some ranges of its
    /// bytes written over ([`derived`]): a linked image a few blocks from the last one. Its
    /// value is the recipe, a few kilobytes; a place of it places the file the recipe gives,
    /// which the store makes of a clone of its base's whole file and keeps as a whole file to
    /// clone ([`Store::place`]). How its file is checked is how it is made: never taken from
    /// an earlier run, always the base's bytes with the ranges written.
    Derived,
}

impl Kind {
    pub const ALL: [Kind; 8] = [
        Kind::Query,
        Kind::Code,
        Kind::Debug,
        Kind::Run,
        Kind::Output,
        Kind::Image,
        Kind::Machine,
        Kind::Derived,
    ];

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
            Kind::Derived => 7,
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
            7 => Some(Kind::Derived),
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
            Kind::Derived => "derived",
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
    /// Tags the loads of its buffer pool ask with: `load_tags..load_tags + 2^32`, a range
    /// apart from `tags`'s (a load's tag names its slot and its read, so the loads core takes
    /// a whole range of its own; `pool::Loads`). A store whose two ranges overlap is refused
    /// as it is made.
    pub load_tags: u64,
    /// How its reads' misses are loaded.
    pub loader: Loader,
}

impl Config {
    /// Whether its two tag ranges, each 2^32 wide, are apart and inside a `u64`.
    fn tags_apart(&self) -> bool {
        const WIDE: u64 = 1 << 32;
        let inside = |from: u64| from.checked_add(WIDE).is_some();
        inside(self.tags) && inside(self.load_tags) && self.tags.abs_diff(self.load_tags) >= WIDE
    }
}

/// How a store loads the pages of its packs a read needs that are not in memory
/// (docs/v2/plan/design/store-pool.md, lane 4b): every read of a pack goes through the
/// store's buffer pool, and no pack is mapped.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Loader {
    /// A thread of the store's own drives the pool's reads on the target's native driver over
    /// `root` (the root of the driver that drives the store: its file names are under it),
    /// and a task that reads waits for its load in a span naming the pages: the daemon's.
    /// Pages are loaded as they are read and given back by the sweep within the pool's
    /// budget.
    Thread { root: std::path::PathBuf },
    /// The store starts no thread, and whoever drives its effects carries out its loads: a
    /// test on the simulation, a tool. A read can carry out no effect, so the store asks for
    /// what each pack commits as the pack opens, and holds it in memory from then on.
    Pumped,
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
    /// Of an output made of a clone of its base with its ranges written, the file's length
    /// less the ranges.
    pub cloned: u64,
    /// Written into the store, once, as the whole file later places clone ([`CLONE_FROM`]):
    /// the value's bytes, or for a derived value ([`Kind::Derived`]) the bytes of its ranges,
    /// which are all that making its file writes.
    pub written: u64,
    /// Written where they were placed from the store's bytes: a value too short to clone, one
    /// whose whole file is not yet there and written back (it is written where it goes, never
    /// waited for), or the ranges written into an output's clone of its base.
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
    /// No place of an output: the derived whole file this waits to be made from the one it
    /// waits on, its base ([`PlacedFile::Basing`]); `to` is then the name the base is cloned
    /// under to be written into ([`Store::deriving_file`]).
    base_of: Option<ContentId>,
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
    /// Anything else (an earlier run's temporary files, and the whole files of derived
    /// values, which this run makes again), removed.
    pub swept: u64,
}

/// Where the whole file of an output placed by cloning is ([`CLONE_FROM`]), by content (a
/// derived value's, by its key). Not in the head, and nothing of it is synced: it is derived
/// data, written with [`Effect::WriteUnsynced`] (or made of a clone and writes at offsets) and
/// renamed to its name whole, so a reader of this run sees all of it or none, and a crash may
/// leave any part of it. One this run did not make is therefore never cloned from as it is: a
/// file named for a content is hashed against its name first ([`PlacedFile::Checking`]), and a
/// derived value's is removed as the store opens and made again.
enum PlacedFile {
    /// Asked whether it is there, a file of the value's length (after an open, which knows
    /// none of them).
    Probing(Vec<Waiter>),
    /// There at the value's length but not written by this run: its bytes hashed against its
    /// name before anything clones it.
    Checking(Vec<Waiter>),
    Writing(Vec<Waiter>),
    /// A derived value's ([`Kind::Derived`]), waiting for the whole file of its base, `base`,
    /// to be there to clone.
    Basing {
        waiting: Vec<Waiter>,
        base: ContentId,
    },
    /// A derived value's, being made under another name ([`Store::deriving_file`]): its base
    /// cloned there, then its ranges written and its length set (`left` of those effects still
    /// in flight, `failed` the first that failed), then renamed to its own name. `written`,
    /// the bytes written so far to make it and, where it had to be made, its base. `span`,
    /// the derive, open until the rename.
    Deriving {
        waiting: Vec<Waiter>,
        left: u32,
        failed: Option<String>,
        written: u64,
        span: tracing::Span,
    },
    /// Written (`written` bytes) or checked (none), being made read-only before anything
    /// clones it.
    Sealing {
        waiting: Vec<Waiter>,
        written: u64,
    },
    /// Read-only, its write-back begun and not known complete ([`publish::Written`]): it is
    /// not cloned from, since a clone of a file with data not written back waits for all of
    /// it. `waiting`, what can be made of nothing but a clone of it (a derived file's wait for
    /// its base, and a place the store has no other way to make); `doomed`, let go of
    /// meanwhile ([`Store::unplace`]). `span`, the write-back, open until it completes.
    Flushing {
        waiting: Vec<Waiter>,
        written: u64,
        file: publish::Written,
        doomed: bool,
        span: tracing::Span,
    },
    /// There, read-only and written back (`file`, the one handle a clone is asked through,
    /// [`Flushed::clone_to`]): `cloning` clones of it are in flight, and it is removed once they
    /// end when `doomed` ([`Store::unplace`]) or `lost` (a clone found it gone); `after`, the
    /// places that wait to be made from it afresh once it is. `owed`, the bytes written to
    /// make it that no place has reported yet (one made before any place asked for it), which
    /// the next place of it reports.
    There {
        file: Flushed,
        cloning: u32,
        doomed: bool,
        lost: bool,
        after: Vec<Waiter>,
        owed: u64,
    },
    /// Being removed; what waits is written afresh once it is gone.
    Removing(Vec<Waiter>),
}

/// A safeguard of placement taken out, for the control of the test that it is one
/// (`tests::place_crash`).
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PlacePlant {
    None,
    /// A whole file found at the value's length after an open is cloned from without its
    /// bytes being hashed against its name.
    UncheckedFile,
    /// A derived value's whole file is renamed to its name and cloned from before its ranges
    /// are written.
    RenameBeforeRanges,
    /// A derive leaves out its last range.
    SkipsARange,
    /// A derived value's whole file left by an earlier run is kept as the store opens and
    /// cloned from as it is.
    KeepsDerived,
    /// A whole file is cloned from as soon as it is sealed, its write-back begun but not
    /// waited for: the clone forces the file's dirty data out and waits for it.
    ClonesUnflushed,
    /// An output placed from its maker's bytes ([`Store::place_made`]) is written into a file
    /// under `placed/` and cloned from it to its path at once ([`publish::Plant`]).
    MadeClonesUnflushed,
    /// As [`PlacePlant::MadeClonesUnflushed`], with the file's write-back waited for before
    /// the clone: the answer waits for a write-back.
    MadeWaitsForWriteBack,
}

/// Whether a value can be the base of a derived one ([`Reader::derivable`]).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Derivable {
    /// It can: a recipe naming it ([`derived::encode`]) places as its file with the ranges
    /// written.
    Yes,
    /// It cannot, and why: the caller puts the whole file.
    No(NoBase),
}

/// Why a value is no base for a derived one. Not an error: a first build, a value evicted or
/// one too short to be kept as a whole file has no base, and its file is put whole.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum NoBase {
    /// The store has no value under the key.
    Absent,
    /// Its value is none the store keeps a whole file of: not an output of [`CLONE_FROM`]
    /// bytes or more, nor a derived one.
    NotWhole { kind: Kind, len: u64 },
    /// It is the last of this many derived values in a row ([`derived::CHAIN_MOST`] bounds
    /// them): one more whole file put ends the chain.
    Chain(u32),
    /// This run wrote its whole file and has not seen that file's write-back complete: a
    /// clone of it now would force the write-back and wait for it (the dirty-base rule,
    /// [`Reader::derivable`]). The caller puts its whole file, which is written where it goes.
    Unflushed,
    /// The value, or one its chain derives from, could not be read: the store's own words.
    Unreadable(String),
}

impl fmt::Display for NoBase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NoBase::Absent => f.write_str("the store has no value under its key"),
            NoBase::NotWhole { kind, len } => write!(
                f,
                "it is a {} value of {len} bytes, and the store keeps a whole file only of an \
                 output of {CLONE_FROM} bytes or more and of a derived value",
                kind.name()
            ),
            NoBase::Chain(links) => write!(
                f,
                "it is the last of {links} derived values in a row, of at most {} before a \
                 whole file is put",
                derived::CHAIN_MOST / 2
            ),
            NoBase::Unflushed => f.write_str(
                "the store wrote its whole file and that file's write-back has not completed: \
                 a clone of it now would wait for the write-back",
            ),
            NoBase::Unreadable(why) => f.write_str(why),
        }
    }
}

/// How the value under `key` is kept as a whole file, if it is: the file's id (its content,
/// or for a derived value its key's hash), its length, and for a derived value its base.
struct Whole {
    id: ContentId,
    len: u64,
    base: Option<Key>,
}

/// [`Whole`] of `key` as `look` finds it, or for a value there that is kept as no whole file
/// its kind and length; `Err` for a key with no value, or one that cannot be read.
fn whole_of(look: Look<'_>, key: &Key) -> Result<Result<Whole, (Kind, u64)>, NoBase> {
    let found = match look.find(key) {
        None => return Err(NoBase::Absent),
        // alloc: the error of a corrupt value
        Some(Err(corrupt)) => return Err(NoBase::Unreadable(corrupt.to_string())),
        Some(Ok(found)) => found,
    };
    // alloc: the error of a corrupt value
    let unreadable = |corrupt: Corrupt| NoBase::Unreadable(corrupt.to_string());
    let (header, _) = look.record(found, key).map_err(unreadable)?;
    if header.kind == Kind::Derived {
        let entry = look.read(found, key).map_err(unreadable)?;
        let recipe = derived::decode(entry.bytes)
            // alloc: the error of a value that is no recipe
            .map_err(|why| NoBase::Unreadable(format!("the derived value {key:?} holds {why}")))?;
        return Ok(Ok(Whole { id: key.0, len: recipe.len, base: Some(recipe.base) }));
    }
    Ok(match clones_from_file(&header, found.place.form()) {
        true => Ok(Whole { id: header.content, len: header.raw, base: None }),
        false => Err((header.kind, header.raw)),
    })
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
    /// Its buffer pool, and which of its files are open in it ([`hot`]).
    hot: hot::Hot,
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
    /// Each place asked and not yet answered: its span (`place`), open from [`Store::place`]
    /// to its [`Placed`], which says how the bytes got there and how many.
    place_spans: HashMap<Tag, tracing::Span, Global>, // alloc: the store's own table, for its life
    /// The outputs being made of a clone of their base with ranges written
    /// ([`Content::Patch`]), by the tag their place was asked with.
    patching: HashMap<Tag, Patching, Global>, // alloc: the store's own table, for its life
    /// The outputs being written from the runs their maker holds ([`Content::Sparse`],
    /// [`Store::place_made`]), by the tag their place was asked with.
    writing: HashMap<Tag, publish::Writing, Global>, // alloc: the store's own table, for its life
    /// The whole files of outputs placed by cloning ([`CLONE_FROM`]), by content.
    files: ByHash<ContentId, PlacedFile, Global>, // alloc: the store's own table, for its life
    /// The whole files that are derived values' ([`Kind::Derived`]), each by its key's hash
    /// with its length: what tells its name ([`Store::placed_file`]) and how it is made from
    /// a file named for its content. Kept for the store's life, a few words a derived value
    /// placed or let go of.
    derived: ByHash<ContentId, u64, Global>, // alloc: the store's own table, for its life
    /// How many derived whole files are being begun inside one another, each for its base
    /// ([`Store::derive_file`]): bounded by [`derived::CHAIN_MOST`].
    derive_depth: u32,
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
    /// Planted bugs for the placement's crash tests (`tests::place_crash`).
    #[cfg(test)]
    plant: PlacePlant,
    /// A planted bug for the commit's crash tests (docs/v2/plan/design/store-pool.md, section 7,
    /// bug 7): the head is written once a commit's writes have completed, before the syncs of
    /// its packs and their checks have, so a crash can leave a head naming bytes that are not
    /// durable.
    #[cfg(test)]
    plant_head_before_syncs: bool,
    /// A planted bug for the refinement against a map (`tests::model_check`): a commit drains
    /// no writer's segment, so a flush makes nothing durable and a reopen has lost every put.
    #[cfg(test)]
    plant_no_drain: bool,
    /// A test's hold on the moves of the batches committed while it is set, as a mapping
    /// still to come held them before packs were read through the pool: the window between
    /// a commit's head and its moves, which a file of its own still opens, kept open until
    /// [`Store::release_moves`].
    #[cfg(test)]
    hold_moves: bool,
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

    /// A hint that the values of `keys` are about to be read: the pages of the store's pool
    /// their records are in and that are not in memory are read ahead of the reads, as one
    /// plan, records that are neighbours in a pack in one read
    /// (docs/v2/plan/design/store-pool.md, section 5). What a step that knows what it is
    /// about to read calls first (codegen, with the templates of the functions it compiles).
    ///
    /// It only plans: it reads nothing itself and waits for nothing. The pool's loader issues
    /// the reads while no read a task waits for is in flight, and within the prefetch budget
    /// (an eighth of the pool's budget, and never more than the room left in it), so a hint
    /// neither delays a demand read nor makes one give pages back. A plan replaces the plan
    /// before it. On a store with no loader (its misses pumped) the packs are in memory
    /// already, and nothing is handed on.
    pub fn prefetch(&self, guard: &Guard, keys: &[Key]) -> Prefetch {
        let runs = self.look(guard).runs_of(keys);
        let mut hint = Prefetch::of(&runs);
        hint.handed = self.shared.side().is_some_and(|side| side.prefetch(runs));
        hint
    }

    /// Whether the value of `base` can be the base of a derived one ([`Kind::Derived`]): it
    /// is there, kept as a whole file (an output long enough, or a derived value), and fewer
    /// than half of [`derived::CHAIN_MOST`] derived values in a row end at it, so no chain
    /// reaches the bound a place refuses. Unverified, as [`Reader::has`] is: for planning. A
    /// caller told [`Derivable::No`] puts its whole file.
    ///
    /// The dirty-base rule: a base whose whole file this run wrote (or derived) and has not
    /// seen written back is no base ([`NoBase::Unflushed`]). A derive clones the base's file,
    /// and a clone of a file with data not written back waits for all of it (1.3 s for
    /// 162 MB), where the whole file written where it goes takes 29 ms. Decided by what the
    /// store knows (it began the file's write and has not had its write-back's completion),
    /// never by asking the kernel. Its bound: the store begins a file's write-back as soon
    /// as the file is whole, so a base is refused only for as long as one write-back of one
    /// file takes, and an edit in that time writes one whole file, once; the next edit after
    /// it derives again. A base no run has a file of yet, or one written back, is a base.
    pub fn derivable(&self, guard: &Guard, base: &Key) -> Derivable {
        let most = derived::CHAIN_MOST / 2;
        let mut at = *base;
        // The bound of the walk: a chain of `most` is refused, so it takes no more steps.
        for links in 0..=most {
            match whole_of(self.look(guard), &at) {
                Err(why) => return Derivable::No(why),
                Ok(Err((kind, len))) => return Derivable::No(NoBase::NotWhole { kind, len }),
                // The file a derive would clone: the base's own, the first of the walk.
                Ok(Ok(Whole { id, .. })) if links == 0 && self.shared.is_unflushed(&id) => {
                    return Derivable::No(NoBase::Unflushed);
                }
                Ok(Ok(Whole { base: None, .. })) => return Derivable::Yes,
                Ok(Ok(Whole { base: Some(next), .. })) if links < most => at = next,
                Ok(Ok(Whole { base: Some(_), .. })) => break,
            }
        }
        Derivable::No(NoBase::Chain(most))
    }

    /// Whether the put that got `ticket` is durable.
    pub fn durable(&self, ticket: Ticket) -> bool {
        self.shared.durable(ticket)
    }

    fn look<'g>(&'g self, guard: &'g Guard) -> Look<'g> {
        Look::new(&self.shared, Held::task(guard))
    }
}

/// A store's pool as one walk of its pages' state words found it ([`Store::pool_census`]):
/// a sample, exact only while nothing else runs. Bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct PoolCensus {
    /// In memory or on their way in or out: the pool's charge.
    pub loaded: u64,
    /// Of `loaded`, pinned by a reader, a value held past its task or a commit.
    pub pinned: u64,
    /// Of `loaded`, what a sweep could give back now.
    pub evictable: u64,
    /// Of `loaded`, prefetched and not yet read.
    pub unread: u64,
}

/// What a prefetch hint planned ([`Reader::prefetch`], [`Store::prefetch`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Prefetch {
    /// Runs of pages planned: one read each, where a run is at most a megabyte.
    pub runs: usize,
    /// Bytes of the pages in them.
    pub bytes: u64,
    /// Whether the plan was handed to the pool's loads.
    pub handed: bool,
}

impl Prefetch {
    fn of(runs: &[pool::Run]) -> Prefetch {
        let bytes = runs.iter().map(|it| u64::from(it.pages) * pool::PAGE).sum();
        Prefetch { runs: runs.len(), bytes, handed: false }
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
    /// Mappings still to come before the moves are made: those of the files of their own
    /// the commit wrote. (A pack is not mapped: what a commit wrote of it is read through
    /// the pool as soon as the head names it.)
    maps: usize,
    /// A test holds its moves back ([`Store::hold_moves`]), counted in `maps`.
    #[cfg(test)]
    held: bool,
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
    /// A pack's block checks read as its tier opens ([`checks`]).
    Checks {
        tier: usize,
        pack: usize,
    },
    /// A pack's checks cut back to those of its committed blocks, before a commit adds to them.
    TruncateChecks {
        pack: usize,
    },
    /// The checks of the blocks a commit adds to a pack, written after those committed.
    WriteChecks {
        pack: usize,
    },
    SyncChecks {
        pack: usize,
    },
    /// The checks of a pack removed, removed with it.
    RemoveChecks {
        pack: usize,
    },
    /// A sync the planted commit does not wait for (`Store::plant_head_before_syncs`).
    #[cfg(test)]
    Unwaited,
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
        from: Flushed,
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
    /// The whole file of `content` written ([`PlacedFile::Writing`]); `written`, its bytes
    /// where no place reported them yet.
    MakeFile {
        content: ContentId,
        written: u64,
    },
    /// The whole file of `content` made read-only ([`PlacedFile::Sealing`]).
    Seal {
        content: ContentId,
    },
    /// The write-back of the whole file of `content` ([`PlacedFile::Flushing`]).
    WriteBack {
        content: ContentId,
    },
    /// The staged name of the output place `asked` derives ([`Patching`]) removed, so its
    /// base, `from`, can be cloned there.
    PatchClear {
        asked: Tag,
        from: Flushed,
    },
    /// The base cloned to the staged name of the output place `asked` derives.
    PatchClone {
        asked: Tag,
    },
    /// One of the ranges of the output place `asked` derives written, or its length set.
    PatchWrite {
        asked: Tag,
    },
    /// The output place `asked` derives renamed from its staged name onto its path.
    PatchRename {
        asked: Tag,
    },
    /// A step of the output place `asked` written from its maker's runs
    /// ([`publish::Writing`]).
    Runs {
        asked: Tag,
        step: publish::Step,
    },
    /// The name the derived whole file `id` is made under ([`Store::deriving_file`]) removed,
    /// so the whole file of its base, `base` (`from`), can be cloned there.
    DeriveClear {
        id: ContentId,
        base: ContentId,
        from: Flushed,
    },
    /// The whole file of `base` cloned to the name `id` is made under.
    DeriveClone {
        id: ContentId,
        base: ContentId,
    },
    /// One of the derived file `id`'s ranges written into its clone, or its length set
    /// ([`PlacedFile::Deriving`]).
    DeriveWrite {
        id: ContentId,
    },
    /// The derived file `id` renamed from the name it was made under to its own.
    DeriveRename {
        id: ContentId,
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
    /// `batch` is the batch of moves waiting for it.
    External {
        content: ContentId,
        batch: Option<u64>,
    },
}

/// A store's directory as it was read and as it is being written.
struct Tier {
    dir: FileName,
    /// The head as last read or durably written.
    head: Head,
    head_read: bool,
    opened: bool,
    /// Mappings and checks of the opening still to come.
    waiting: usize,
    /// Packs of the opening a pumped store is still loading into its pool
    /// ([`hot::Hot::load_whole`]): the opening goes on once all are in.
    loading: usize,
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
            loading: 0,
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
                .map(|it| PackView { file: it.file.clone(), base: it.base, usable: it.usable })
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
    /// The byte of the store's pool its first byte lives at, while it is open there: its
    /// committed bytes are read through the pool, never mapped.
    base: Option<u64>,
    /// How much of it readers read through the pool: what is committed.
    usable: u64,
    /// The bytes from `usable` to `end`: what was drained and no commit has made durable
    /// yet, which the next commit writes through the pool. Only the store reads them: a
    /// record not yet committed is read from its segment.
    memory: Vec<u8>,
    /// Why a page of it could not be loaded as a pumped store opened it, the first: what the
    /// open says when its scan cannot read the pack.
    load_failed: Option<String>,
    /// What the head on disk says is the store's.
    committed: u64,
    /// The check of each block of its committed bytes ([`checks`]): what `checks-<n>` holds as
    /// far as the head names, kept for the pack's life.
    checks: Vec<u32>,
    indexed: u64,
    end: u64,
    dead: u64,
    removed: bool,
    /// Its live records are copied on; the next head names it removed.
    retiring: bool,
}

impl Pack {
    /// Its `len` bytes at `at`: committed, pinned through the store's pool for as long as
    /// `look`'s table of pins lives; or drained and not yet committed, its own. `None` when
    /// they are neither, or cannot be read (the pin's error is the reader's to report:
    /// [`Pack::committed_bytes`]).
    fn bytes<'a>(&'a self, look: Look<'a>, at: u64, len: u64) -> Option<&'a [u8]> {
        let end = at.checked_add(len)?;
        if end <= self.usable {
            self.committed_bytes(look, at, len).ok()
        } else if at >= self.usable && end <= self.end {
            self.memory.get((at - self.usable) as usize..(end - self.usable) as usize)
        } else {
            None
        }
    }
}

impl Pack {
    /// Its `len` committed bytes at `at`, pinned through the store's pool.
    fn committed_bytes<'a>(&self, look: Look<'a>, at: u64, len: u64) -> Result<&'a [u8], String> {
        let end = at.checked_add(len).filter(|&end| end <= self.usable);
        let (Some(_), Some(base)) = (end, self.base) else {
            // alloc: the error of a read that fails
            return Err(format!(
                "bytes {at}..{} are not among the {} it commits in the store's pool",
                at.saturating_add(len),
                self.usable
            ));
        };
        // alloc: the error of a read that fails
        look.pinned_at(base + at, len).map_err(|why| why.to_string())
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
    /// The check of each block it adds to each of `packs`, in their order.
    checks: Vec<Vec<u32>>,
    /// Its append to each of `packs`, in their order, once begun: the pins of the pool's pages
    /// its bytes are written from, held until the head names them or the commit is given up.
    appends: Vec<Option<hot::PackAppend>>,
    /// The writes of each of `packs` not yet answered: its sync is asked after its last.
    writes: Vec<u32>,
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
    /// The checks of the blocks it adds to `pack`.
    fn checks_of(&self, pack: usize) -> Option<&[u32]> {
        let at = self.packs.iter().position(|(it, _)| *it == pack)?;
        self.checks.get(at).map(Vec::as_slice)
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

/// The op a commit's sync is asked as: `op`, which the head waits for, or (`waited` false, only
/// ever under a test's planted bug) one it does not.
fn sync_op(op: Op, waited: bool) -> Op {
    #[cfg(test)]
    let op = match waited {
        true => op,
        false => Op::Unwaited,
    };
    debug_assert!(waited || cfg!(test), "a commit's head waits for every sync");
    op
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
        assert!(
            config.tags_apart(),
            "the store in {} is given tags from {} and its pool's loads tags from {}: the two \
             ranges, 2^32 wide each, overlap (Config::tags, Config::load_tags)",
            config.dir,
            config.tags,
            config.load_tags
        );
        // alloc: setup, once per store
        let mut tiers = vec![Tier::new(config.dir.clone())];
        tiers.extend(config.layers.iter().cloned().map(Tier::new));
        let columns = Arc::<Encodings>::default();
        // clone: setup, once per store
        let view = View { tiers: tiers.iter().map(Tier::view).collect(), columns: columns.clone() };
        let hot = hot::Hot::new(config.load_tags, &config.loader);
        // alloc: setup, once per store
        let shared = Arc::new(Shared::new(view, hot.side()));
        let own = Own::new(0);
        // clone: setup, the store's own segment's `Arc`, once per store
        shared.add_segment(own.segment().clone());
        // clone: setup, once per store
        let drains = vec![Drain::new(own.segment().clone())];
        // One daemon a directory (#686), so the hash of the store's own directory, as the driver
        // names it, tells its staged names from those of any other daemon's store.
        let staging = ContentId::of(STAGING_DOMAIN, config.dir.as_str().as_bytes());
        let staging = u64::from_le_bytes(staging.0[..8].try_into().expect("8 bytes"));
        let mut store = Store {
            tags: Tags { range: blaze_core::Tags::new(config.tags), ops: HashMap::new_in(Global) }, // alloc: the store's own table, for its life
            config,
            tiers,
            broken: None,
            failures: 0,
            shared,
            hot,
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
            place_spans: HashMap::new_in(Global), // alloc: the store's own table, for its life
            patching: HashMap::new_in(Global), // alloc: the store's own table, for its life
            writing: HashMap::new_in(Global), // alloc: the store's own table, for its life
            files: ByHash::new_in(Global), // alloc: the store's own table, for its life
            derived: ByHash::new_in(Global), // alloc: the store's own table, for its life
            derive_depth: 0,
            placed_on_open: PlacedOnOpen::default(),
            placed_looked: false,
            held: Vec::new(),          // alloc: empty, allocates nothing
            held_unplaces: Vec::new(), // alloc: empty, allocates nothing
            look_wait: None,
            look_tries: 0,
            new_keys: None,
            epoch: 0,
            periods: 0,
            columns,
            peers,
            staging,
            #[cfg(test)]
            move_probed: None,
            #[cfg(test)]
            plant_clear_then_clone: false,
            #[cfg(test)]
            plant: PlacePlant::None,
            #[cfg(test)]
            plant_head_before_syncs: false,
            #[cfg(test)]
            plant_no_drain: false,
            #[cfg(test)]
            hold_moves: false,
        };
        // A pool the system refused to reserve stops the store, naming why: nothing is read
        // another way.
        // alloc: the error that breaks the store
        if let Some(why) = store.hot.refused().map(str::to_owned) {
            // clone: the error that breaks the store
            let dir = store.config.dir.clone();
            store.fail(dir, why);
        }
        store
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
        self.publish_view();
    }

    /// Whether it has a value of `key`, unverified: for planning, never for use.
    pub fn has(&self, key: &Key) -> bool {
        self.look().has(key)
    }

    /// [`Reader::prefetch`], from the store's own thread. On a store whose misses are pumped
    /// the plan's reads go into `effects`, for its driver: its packs are in memory from their
    /// opening, so they read only what a sweep gave back since.
    pub fn prefetch(&mut self, keys: &[Key], effects: &mut Vec<Effect>) -> Prefetch {
        let runs = self.look().runs_of(keys);
        let mut hint = Prefetch::of(&runs);
        hint.handed = match self.hot.pumped() {
            true => self.hot.prefetch_pumped(runs, hot::Side::prefetch_budget(), effects) > 0,
            false => self.shared.side().is_some_and(|side| side.prefetch(runs)),
        };
        hint
    }

    /// At least `bytes` of its pool's pages given back to the kernel, if the sweep finds that
    /// many neither pinned nor read since it last came by: what the daemon's ceiling guard
    /// asks before it gives up, and the pool's part of the one evictor
    /// (docs/v2/plan/design/store-pool.md, section 4). The bytes given back. The pins of
    /// tasks that have ended are let go first, so their pages are the sweep's to take.
    ///
    /// On a store whose misses are pumped nothing loads a page again but its opening, so it
    /// gives none back: 0.
    pub fn evict_pool(&mut self, bytes: u64) -> u64 {
        self.tidy();
        if self.hot.pumped() {
            return 0;
        }
        // The readers' table of pins is let go, whatever it holds: the pins of every task
        // that has ended go back as its grace passes (at once, when no task runs), and
        // those of running tasks stay.
        let old = self.shared.fresh_owned();
        self.limbo.retire(old);
        self.limbo.collect();
        self.shared.pins_let_go();
        self.hot.evict(bytes).map_or(0, |swept| swept.freed)
    }

    /// Its pool's pages now, from one walk of their state words: bytes loaded, bytes of them
    /// pinned, and bytes a sweep could give back. `None` when it has no pool.
    pub fn pool_census(&self) -> Option<PoolCensus> {
        self.hot.census().map(|it| PoolCensus {
            loaded: it.loaded,
            pinned: it.pinned,
            evictable: it.evictable,
            unread: it.unread,
        })
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
    /// running moves hot. The daemon
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
        let (mut demote, mut promote) = self.tier_moves();
        radix::sort_by_key(&mut demote, |(_, loc, _)| {
            (u128::from(loc.kind.code()) << 96) | radix::pack_64(u64::from(loc.pack), loc.offset)
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
                    let look = Look::new(&self.shared, Held::writer(&*self.shared));
                    let Some(raw) = payload.and_then(|at| pack.bytes(look, at, loc.raw)) else {
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
        self.start_commit(effects);
        settled
    }

    /// The own tier's records in its packs to move cold and to move hot, as of the period
    /// ending now. A record still in a segment is new, and stays.
    fn tier_moves(&self) -> (Vec<(Key, Loc, Row)>, Vec<(Key, Loc, Row)>) {
        let own = &self.tiers[0];
        // alloc: once a settle (idle, never on the way of a get or a put); empty unless a slot is not whole
        let mut bad = Vec::new();
        let rows = self.look().rows(0, &mut bad);
        // alloc: once a settle (idle), the records it moves, sorted after
        let (mut demote, mut promote) = (Vec::new(), Vec::new());
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
            match loc.form {
                Form::Raw | Form::Lz4 | Form::Warm
                    if self.periods >= 2
                        && !read
                        && !before
                        && !flag(Flag::New)
                        && !flag(Flag::NewBefore)
                        && !flag(Flag::Tried) =>
                {
                    demote.push((key, loc, found.at.row()));
                }
                Form::Zstd | Form::Warm | Form::ColdColumns if read && before && !short => {
                    promote.push((key, loc, found.at.row()));
                }
                _ => {}
            }
        }
        (demote, promote)
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
        let look = Look::new(&self.shared, Held::writer(&*self.shared));
        // alloc: once per pack a settle (idle) rewrites, its live records
        let mut live = Vec::new();
        let mut offset = PACK_HEADER;
        let mut why = None;
        while offset < pack.committed {
            let Some(start) = pack.bytes(look, offset, 4) else {
                // alloc: a problem reported
                why = Some(format!("at {offset} could not be read through the store's pool"));
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
            let header = match pack.bytes(look, offset, RECORD_HEADER).map(RecordHeader::decode) {
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
                let Some(payload) = pack.bytes(look, offset + RECORD_HEADER, header.stored) else {
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

    /// Asks that the file of the own pack `pack`, which the head names removed, be removed, and
    /// its checks with it.
    fn remove_pack(&mut self, pack: usize, effects: &mut Vec<Effect>) {
        let tag = self.tags.ask(Op::RemovePack { pack });
        // clone: the effect owns its file name; once per pack removed
        effects.push(Effect::Remove { file: self.tiers[0].packs[pack].file.clone(), tag });
        let tag = self.tags.ask(Op::RemoveChecks { pack });
        let file = self.tiers[0].file(&disk::checks_file(pack));
        effects.push(Effect::Remove { file, tag });
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
    /// [`Store::placed`]. What a value's kind, form and length, and what the store knows of
    /// its files, decide is how it is placed ([`Store::publish`]), never a try of one way then
    /// another: a file of its own is cloned; an output of [`CLONE_FROM`] bytes or more is
    /// cloned from the whole file of it the store keeps where that is there and written back,
    /// and until it is, written where it goes from the value's bytes (the store's file is
    /// written beside that, for the places after); a derived value ([`Kind::Derived`]) is
    /// cloned from the whole file its recipe gives where that is there and written back, and
    /// until it is, made of a clone of its base's whole file with the recipe's ranges written
    /// (as the store's own file of it is, beside that); anything else is written. A clone
    /// fails where the file system cannot share blocks, naming it, and nothing copies instead.
    ///
    /// No clone is ever of a file with data not written back: a file the store wrote is
    /// cloned from only once its write-back completed ([`publish::Flushed`]). A place that can
    /// be made of nothing else (a derived value whose base's file is not there and written
    /// back) waits for that, and its span says so.
    ///
    /// A place waits for no sync ([`Durable::No`]): the whole file, a write of `to`, a clone
    /// under a staged name and the rename of that onto `to` are none of them made durable.
    /// The place is done ([`Store::placed`]) once `to` holds the value's bytes whole, and until
    /// then `to` holds what it held. What a crash leaves is checked or made again: a whole
    /// file is hashed against its name before it is next cloned from (a derived value's is
    /// made again), and `to` holds its old file, the new one, or what the power left of the
    /// new one (it is derived data), to be placed again by the build after the restart, which
    /// knows of no file it placed.
    pub fn place(&mut self, key: &Key, to: FileName, effects: &mut Vec<Effect>) -> Option<Tag> {
        assert!(
            !self.tiers.iter().any(|tier| to.is_under(&tier.dir)),
            "{to} is in the store: values are placed outside it"
        );
        self.tidy();
        enum Plan {
            Clone(FileName, u64, ContentId),
            File(ContentId, u64),
            /// The file a recipe gives, of this length ([`Kind::Derived`]).
            Derived(u64),
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
            // A recipe is never placed as its own bytes: what is placed is the file it gives.
            Ok((found, header)) if header.kind == Kind::Derived => {
                match look.read(found, key).map(|entry| derived::decode(entry.bytes)) {
                    Ok(Ok(recipe)) => Plan::Derived(recipe.len),
                    // alloc: the error of a value that is no recipe
                    Ok(Err(why)) => Plan::Refused(format!("the derived value {key:?} holds {why}")),
                    // alloc: the error of a corrupt value
                    Err(corrupt) => Plan::Refused(corrupt.to_string()),
                }
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
        let plan = match plan {
            Plan::Derived(len) => {
                self.derived.insert(key.0, len);
                Plan::File(key.0, len)
            }
            plan => plan,
        };
        // The place's span, open until its answer ([`Store::answer`]): how the bytes get to
        // `to` is said where it is chosen ([`Store::publish`]), and what it had to wait for
        // where it waits. No parent: it ends on a later completion, under whatever request
        // is current then.
        let span = tracing::info_span!(
            target: "blaze",
            parent: None,
            "place",
            %to,
            how = tracing::field::Empty,
            bytes = tracing::field::Empty,
            waits = tracing::field::Empty,
        );
        self.place_spans.insert(tag, span);
        match plan {
            // A value kept as a file of its own, which the store's commit synced.
            Plan::Clone(from, raw, content) => {
                let content = Content::File { from: Flushed::synced(from), content, of: None, len: raw };
                self.publish(tag, to, content, Durable::No, 0, effects);
            }
            Plan::Derived(_) => unreachable!("made a file's plan above"),
            Plan::File(content, raw) => {
                let waiter =
                    Waiter { asked: tag, to, key: *key, raw, retried: false, base_of: None };
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
            // Not synced: an output is placed again by the build that finds it missing or
            // changed, and the answer of this one waits for no disk.
            Plan::Write(bytes) => {
                self.publish(tag, to, Content::Bytes(bytes), Durable::No, 0, effects)
            }
            Plan::Refused(why) => self.answer(Placed {
                tag,
                to,
                result: Err(why),
                bytes: PlacedBytes::default(),
            }),
        }
        Some(tag)
    }

    /// Asks for `file`, an output the build that asks just made, to be written at `to` from
    /// the bytes its maker holds: the edit path's place. How it went comes through
    /// [`Store::placed`], under the tag returned.
    ///
    /// Nothing of the store is read, cloned or waited for: the runs are written into a file
    /// of holes under a staged name beside `to`, which is then renamed onto `to`
    /// ([`publish::Writing`]); none of it is synced ([`Durable::No`]). So the answer of the
    /// build is held by no clone of a file with data not written back and by no write-back,
    /// whatever state the store's own files are in: there is no state of them this reads.
    /// The value itself is the caller's to put ([`Store::put`], a recipe of what the link
    /// changed or the whole file), and becomes durable with the store's next commit, after
    /// the answer; a place of it with no bytes at hand (another checkout's, a record replayed
    /// after a restart) is [`Store::place`].
    ///
    /// What is at `to` after a crash is its old file, the new one, or (the rename not being
    /// synced, nor the data) what the power left of the new one: an output is derived data
    /// that the build after the restart places again, since a daemon that starts knows of no
    /// file it placed. Nothing records `to` as holding the value until this place is done.
    pub fn place_made(&mut self, to: FileName, file: Sparse, effects: &mut Vec<Effect>) -> Tag {
        assert!(
            !self.tiers.iter().any(|tier| to.is_under(&tier.dir)),
            "{to} is in the store: values are placed outside it"
        );
        self.tidy();
        let tag = self.tags.fresh();
        // As [`Store::place`]'s: open until the answer, with no parent.
        let span = tracing::info_span!(
            target: "blaze",
            parent: None,
            "place",
            %to,
            how = tracing::field::Empty,
            bytes = tracing::field::Empty,
            waits = tracing::field::Empty,
        );
        self.place_spans.insert(tag, span);
        self.publish(tag, to, Content::Sparse(file), Durable::No, 0, effects);
        tag
    }

    /// How a step of an output written from its maker's runs went ([`publish::Writing`]): the
    /// next effects asked, or its place answered. One that fails removes what it made under
    /// its staged name and refuses the place, naming the path and the step.
    fn runs_step(&mut self, asked: Tag, step: publish::Step, done: &Done, effects: &mut Vec<Effect>) {
        let Some(writing) = self.writing.get_mut(&asked) else { return };
        let tags = &mut self.tags;
        let next = writing.step(step, done, |step| tags.ask(Op::Runs { asked, step }), effects);
        let result = match next {
            publish::Next::Going => return,
            publish::Next::Done(wrote) => Ok(wrote),
            publish::Next::Failed(why) => Err(why),
        };
        let Some(publish::Writing { to, temp, .. }) = self.writing.remove(&asked) else { return };
        match result {
            Ok(wrote) => {
                let bytes = PlacedBytes { cloned: 0, written: 0, copied: wrote };
                self.answer(Placed { tag: asked, to, result: Ok(()), bytes });
            }
            Err(why) => {
                // clone: the effect and the op each own the name; once per place that fails
                let tag = self.tags.ask(Op::Discard { file: temp.clone() });
                effects.push(Effect::Remove { file: temp, tag });
                // alloc: an error, once per place that fails
                let result = Err(format!("writing {to} from its maker's bytes: {why}"));
                self.answer(Placed { tag: asked, to, result, bytes: PlacedBytes::default() });
            }
        }
    }

    /// Lets go of the whole file kept to place `key`'s value by cloning ([`CLONE_FROM`]), for
    /// a path that now holds an output placed from its maker's bytes ([`Store::place_made`]),
    /// once no clone of it is in flight. As [`Store::unplace`], but nothing is begun in its
    /// place: the new output was written where it goes, so no whole file of it is made, and
    /// none is cloned, derived or written back on the edit path. Only a file this run knows
    /// of goes (one it made, checked or is making): a value placed from its maker's bytes
    /// has none, and one an earlier run left under `placed/` is the open's to keep.
    pub fn unplace_made(&mut self, key: &Key, effects: &mut Vec<Effect>) {
        self.tidy();
        let Ok(Ok(Whole { id, len, base })) = whole_of(self.look(), key) else { return };
        if !self.files.contains_key(&id) {
            return;
        }
        if base.is_some() {
            self.derived.insert(id, len);
        }
        match (self.placed_looked, &self.broken) {
            (true, _) => self.let_go(id, effects),
            // alloc: once per output built again while the open's look runs
            (false, None) => {
                self.waiting_for_look();
                self.held_unplaces.push(id);
            }
            (false, Some(_)) => {}
        }
    }

    /// A place answered: its span ends, saying how many bytes its path was given.
    fn answer(&mut self, placed: Placed) {
        if let Some(span) = self.place_spans.remove(&placed.tag) {
            span.record("bytes", placed.bytes.cloned + placed.bytes.copied);
        }
        self.placed.push(placed);
    }

    /// Makes `to` hold `content`, replaced in one step for a reader, for the place `asked`:
    /// the one entry every place goes through, and the one place that picks how. What the
    /// content is decides it, never a try of one way then another: bytes are written where
    /// they go; a file written back is cloned under a staged name and renamed onto the path;
    /// a file written back and a derived value's ranges are a clone of the file with the
    /// ranges written and its length set, renamed onto the path. The place's span says which
    /// (`how`: `write`, `clone`, `derive`). No hard link is ever made: an output is its
    /// user's to replace, write to and make executable, and a link is the store's own inode.
    ///
    /// `durable` is how far the path's new bytes are taken toward the disk, and has no
    /// default. An output is [`Durable::No`]: the build that finds it missing or changed
    /// places it again. Bytes may be asked [`Durable::Yes`]; a clone or a derive made durable,
    /// and a write-back begun of an output ([`Durable::Started`]), are refused by name:
    /// nothing asks for one, and none is made as something less.
    ///
    /// `written`, the bytes written into the store to make the file this is cloned from
    /// that no place reported yet.
    fn publish(
        &mut self,
        asked: Tag,
        to: FileName,
        content: Content,
        durable: Durable,
        written: u64,
        effects: &mut Vec<Effect>,
    ) {
        if let Some(span) = self.place_spans.get(&asked) {
            span.record("how", content.how());
        }
        let takes = match (&content, durable) {
            (_, Durable::No) | (Content::Bytes(_), Durable::Yes) => true,
            (_, Durable::Started | Durable::Yes) => false,
        };
        if !takes {
            // What the content counted as cloning is counted back.
            match &content {
                Content::File { of: Some(source), .. } => self.cloned(source.content, effects),
                Content::Patch { base_id, .. } => self.cloned(*base_id, effects),
                Content::File { of: None, .. } | Content::Bytes(_) | Content::Sparse(_) => {}
            }
            // alloc: an error, once per place refused
            let why = format!(
                "{to} was to be published by a {} taken {durable:?} toward the disk: the store \
                 makes a clone or a derive durable for no one, and begins no write-back of an \
                 output",
                content.how()
            );
            return self.answer(Placed {
                tag: asked,
                to,
                result: Err(why),
                bytes: PlacedBytes::default(),
            });
        }
        match content {
            Content::Bytes(bytes) => {
                let placed = PlacedBytes { cloned: 0, written, copied: bytes.len() as u64 };
                // No staged name: every driver makes a write under a temporary name of its own
                // process beside `to` and renames it over `to` (`write_unsynced`, io_uring's
                // `push_temporary`), so two writes of the same bytes there both end with them.
                // clone: the effect and the op each own the name; once per place
                effects.push(publish::write(to.clone(), bytes, durable, asked));
                let op = Op::Place { asked, to, staged: None, of: None, bytes: placed };
                self.tags.ops.insert(asked, op);
            }
            Content::Sparse(file) => {
                if let Err(why) = file.check() {
                    // alloc: an error, once per place refused
                    let why = format!("{to} was to be written from runs that are no file: {why}");
                    return self.answer(Placed {
                        tag: asked,
                        to,
                        result: Err(why),
                        bytes: PlacedBytes::default(),
                    });
                }
                let temp = self.staging_name(&to, asked);
                // The planted bugs: written under `placed/`, as the store's own files are,
                // and cloned from there.
                #[cfg(test)]
                let (temp, plant) = match self.plant {
                    PlacePlant::MadeClonesUnflushed => (
                        self.tiers[0].file(&format!("placed/made-{:x}", asked.0)),
                        Some(publish::Plant::ClonesUnflushed),
                    ),
                    PlacePlant::MadeWaitsForWriteBack => (
                        self.tiers[0].file(&format!("placed/made-{:x}", asked.0)),
                        Some(publish::Plant::WaitsForWriteBack),
                    ),
                    _ => (temp, None),
                };
                let (writing, clear) = publish::Writing::begin(to, temp, file, asked);
                #[cfg(test)]
                let writing = writing.planted_with(plant);
                self.writing.insert(asked, writing);
                self.tags.ops.insert(asked, Op::Runs { asked, step: publish::Step::Clear });
                effects.push(clear);
            }
            Content::File { from, content, of, len } => {
                let bytes = PlacedBytes { cloned: len, written, copied: 0 };
                effects.push(self.clear_staged(asked, from, to, content, of, bytes));
            }
            Content::Patch { base, base_id, id, len } => {
                let temp = self.staging_name(&to, asked);
                let patching = Patching {
                    to,
                    // clone: the effect and the state each own the name; once per place
                    temp: temp.clone(),
                    base_id,
                    id,
                    len,
                    left: 0,
                    failed: None,
                    wrote: 0,
                    written,
                };
                self.patching.insert(asked, patching);
                self.tags.ops.insert(asked, Op::PatchClear { asked, from: base });
                effects.push(Effect::Remove { file: temp, tag: asked });
            }
        }
    }

    /// How a step of an output's derive went ([`Content::Patch`], [`Patching`]): its staged
    /// name cleared, its base cloned there, its ranges written and its length set, then the
    /// whole renamed onto its path, which until then holds what it held. A step that fails
    /// removes the staged file and refuses the place, naming the path and the step.
    fn patch_step(&mut self, op: Op, done: Done, effects: &mut Vec<Effect>) {
        match op {
            Op::PatchClear { asked, from } => {
                let Some(patching) = self.patching.get(&asked) else { return };
                // clone: the effect owns the name it clones to; once per place
                let (temp, base_id) = (patching.temp.clone(), patching.base_id);
                match done {
                    Done::Removed => {
                        let tag = self.tags.ask(Op::PatchClone { asked });
                        effects.push(from.clone_to(temp, tag));
                    }
                    done => {
                        self.cloned(base_id, effects);
                        // alloc: an error, once per place that fails
                        let why =
                            format!("clearing {temp} to clone its base there: {}", describe(&done));
                        self.patch_failed(asked, &why, effects);
                    }
                }
            }
            Op::PatchClone { asked } => {
                let Some(patching) = self.patching.get(&asked) else { return };
                // clone: each write's effect owns the name; once per place
                let (temp, base_id, id) = (patching.temp.clone(), patching.base_id, patching.id);
                self.cloned(base_id, effects);
                if !matches!(done, Done::Cloned) {
                    let from = self.placed_file(&base_id);
                    // alloc: an error, once per place that fails
                    let why = format!("cloning its base {from} to {temp}: {}", describe(&done));
                    return self.patch_failed(asked, &why, effects);
                }
                match self.derive_writes(id, &temp, effects, || Op::PatchWrite { asked }) {
                    Ok((count, bytes)) => {
                        if let Some(patching) = self.patching.get_mut(&asked) {
                            patching.left = count;
                            patching.wrote = bytes;
                        }
                    }
                    Err(why) => self.patch_failed(asked, &why, effects),
                }
            }
            Op::PatchWrite { asked } => {
                let Some(patching) = self.patching.get_mut(&asked) else { return };
                patching.left = patching.left.saturating_sub(1);
                if !matches!(done, Done::Written | Done::Truncated) && patching.failed.is_none() {
                    patching.failed = Some(describe(&done));
                }
                if patching.left > 0 {
                    return;
                }
                match patching.failed.take() {
                    None => {
                        // clone: the effect owns the names; once per place
                        let (from, to) = (patching.temp.clone(), patching.to.clone());
                        let tag = self.tags.ask(Op::PatchRename { asked });
                        // Not synced: the path holds its old file or the new one, whole.
                        effects.push(publish::rename(from, to, Durable::No, tag));
                    }
                    Some(why) => {
                        // alloc: an error, once per place that fails
                        let why = format!("writing its ranges into its base's clone: {why}");
                        self.patch_failed(asked, &why, effects)
                    }
                }
            }
            Op::PatchRename { asked } => match done {
                Done::Renamed => {
                    let Some(Patching { to, len, wrote, written, .. }) =
                        self.patching.remove(&asked)
                    else {
                        return;
                    };
                    let bytes =
                        PlacedBytes { cloned: len.saturating_sub(wrote), written, copied: wrote };
                    self.answer(Placed { tag: asked, to, result: Ok(()), bytes });
                }
                // A derived value's file has no hash of its own to check the path against,
                // so a rename refused is the place's failure.
                done => {
                    // alloc: an error, once per place that fails
                    let why = format!("renaming it onto its path: {}", describe(&done));
                    self.patch_failed(asked, &why, effects)
                }
            },
            _ => unreachable!("not a step of an output's derive"),
        }
    }

    /// The output's derive `asked` failed with `why`: what it made under its staged name
    /// goes, and its place is refused, naming the path.
    fn patch_failed(&mut self, asked: Tag, why: &str, effects: &mut Vec<Effect>) {
        let Some(Patching { to, temp, .. }) = self.patching.remove(&asked) else { return };
        // clone: the effect and the op each own the name; once per derive that fails
        let tag = self.tags.ask(Op::Discard { file: temp.clone() });
        effects.push(Effect::Remove { file: temp, tag });
        // alloc: an error, once per place that fails
        let result = Err(format!("deriving {to} from a clone of its base: {why}"));
        self.answer(Placed { tag: asked, to, result, bytes: PlacedBytes::default() });
    }

    /// Lets go of the whole file kept to place `key`'s value by cloning ([`CLONE_FROM`]), for a
    /// path that now holds `now`'s value instead, once no clone of it is in flight: so a store
    /// whose outputs are built again and again does not keep a whole file of each. Nothing goes
    /// when `now`'s bytes are `key`'s (two keys of one output). Only that file goes, never the
    /// value (its record and chunks stay): placing it again writes it again, and every clone of
    /// it already placed keeps its blocks. When `now` is derived from `key`'s value
    /// ([`Kind::Derived`]), `now`'s whole file is made of a clone of this one first.
    pub fn unplace(&mut self, key: &Key, now: &Key, effects: &mut Vec<Effect>) {
        self.tidy();
        let look = self.look();
        // The whole file each is placed from, if one is kept of it; else its content alone.
        let file_of = |key: &Key| match whole_of(look, key) {
            Ok(Ok(whole)) => Some((whole.id, Some(whole))),
            Ok(Err(_)) => {
                let found = look.find(key)?.ok()?;
                Some((look.record(found, key).ok()?.0.content, None))
            }
            Err(_) => None,
        };
        let Some((content, Some(old))) = file_of(key) else { return };
        let new = file_of(now);
        if new.as_ref().is_some_and(|(it, _)| *it == content) {
            return;
        }
        if old.base.is_some() {
            self.derived.insert(content, old.len);
        }
        // What replaces it is derived from it: its file is begun now, so that it clones this
        // one before this one goes (the file then goes once that clone has ended), and a
        // build that edits one image again and again never writes a whole file of it twice.
        if let Some((id, Some(Whole { len, base: Some(base), .. }))) = new
            && base == *key
            && self.placed_looked
            && self.broken.is_none()
        {
            self.derived.insert(id, len);
            if !self.files.contains_key(&id) {
                // alloc: empty, allocates nothing
                self.make_file(id, Vec::new(), effects);
            }
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
            // Its write-back is in flight: it goes once that ended, unless wanted by then.
            Some(PlacedFile::Flushing { doomed, .. }) => *doomed = true,
            // Wanted again, or going already.
            Some(_) => {}
        }
    }

    /// What the open found of the whole files it places from: kept for this run, which checks
    /// each one's bytes against its name before it first clones it, and what it removed: the
    /// partial writes of an earlier run and the whole files of derived values
    /// ([`Kind::Derived`]), which are made again from their recipes.
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
            // A derived value's whole file is not named so and goes with the rest: nothing
            // says its bytes are its base's with its ranges written but having made it, and
            // an earlier run's making may have been cut by a crash at any write. (The planted
            // bug keeps it.)
            #[cfg(test)]
            let named = named
                || (self.plant == PlacePlant::KeepsDerived && name.ends_with(DERIVED_SUFFIX));
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

    /// The whole file of `content`, which outputs of [`CLONE_FROM`] bytes or more are cloned
    /// from: named for its content, which it is checked against, or for a derived value
    /// ([`Store::derived`]) for its key with [`DERIVED_SUFFIX`], which says that it is never
    /// taken from an earlier run.
    fn placed_file(&self, content: &ContentId) -> FileName {
        match self.derived.contains_key(content) {
            // alloc: a file name for an effect, which owns it; once per place
            true => self.tiers[0].file(&format!("placed/{content:x}{DERIVED_SUFFIX}")),
            // alloc: a file name for an effect, which owns it; once per place
            false => self.tiers[0].file(&format!("placed/{content:x}")),
        }
    }

    /// The name the derived whole file `id` is made under, in `placed/`: its base's clone,
    /// written into, renamed to [`Store::placed_file`] only once it is whole. One a crash
    /// left goes as the store opens, and is removed before the next clone there.
    fn deriving_file(&self, id: &ContentId) -> FileName {
        // alloc: a file name for an effect, which owns it; once per derive
        self.tiers[0].file(&format!("placed/{id:x}.deriving"))
    }

    /// `waiter` placed from the whole file of `content`: cloned from it where it is there and
    /// written back ([`PlacedFile::There`]). Where it is not, an output's place does not wait
    /// for it ([`Store::direct`]): the value's bytes are written where they go, or its base's
    /// file is cloned and its ranges written, and the store's own file is made beside that,
    /// for the next place. Only what can be made of nothing but a clone of the file waits
    /// for it.
    fn place_file(&mut self, content: ContentId, waiter: Waiter, effects: &mut Vec<Effect>) {
        // A derived value's is never taken as an earlier run left it: it is made, and its
        // rename replaces whatever is under its name. (The planted bug looks for one.)
        let made_unseen = self.derived.contains_key(&content) && !self.keeps_derived();
        enum At {
            There(Flushed, u64),
            Lost,
            /// Being made: `writing`, by this run from the value's bytes, which are known.
            Busy { writing: bool },
            Unseen,
        }
        let at = match self.files.get_mut(&content) {
            Some(PlacedFile::There { lost: true, .. }) => At::Lost,
            Some(PlacedFile::There { file, cloning, doomed, owed, .. }) => {
                *cloning += 1;
                *doomed = false;
                // clone: a file's name, a reference count: the clone holds its source's
                At::There(file.clone(), std::mem::take(owed))
            }
            Some(
                PlacedFile::Writing(_) | PlacedFile::Sealing { .. } | PlacedFile::Flushing { .. },
            ) => At::Busy { writing: true },
            Some(_) => At::Busy { writing: false },
            None => At::Unseen,
        };
        match at {
            At::There(file, written) => self.serve(content, &file, waiter, written, effects),
            At::Lost => self.wait_on(content, waiter),
            At::Busy { writing } => {
                if let Some(waiter) = self.direct(content, waiter, writing, effects) {
                    self.wait_on(content, waiter);
                }
            }
            At::Unseen if made_unseen => {
                // alloc: the places waiting on one file, once per derived file made
                let waiting: Vec<Waiter> =
                    self.direct(content, waiter, false, effects).into_iter().collect();
                if let Some(span) = waiting.first().and_then(|it| self.place_spans.get(&it.asked))
                {
                    span.record("waits", "the whole file it is cloned from: derived and written back");
                }
                self.make_file(content, waiting, effects);
            }
            At::Unseen => {
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

    /// `waiter` waits for the whole file of `content`, which is being made, checked, written
    /// back or removed: its place's span says so (`waits`), since the answer it is part of
    /// is held for it (every-wait-says-what). Bounded by the file's steps, each an effect
    /// that completes or fails, and a failure refuses the waiter by name.
    fn wait_on(&mut self, content: ContentId, waiter: Waiter) {
        if waiter.base_of.is_none()
            && let Some(span) = self.place_spans.get(&waiter.asked)
        {
            span.record("waits", "the whole file it is cloned from: made, sealed and written back");
        }
        match self.files.get_mut(&content) {
            Some(PlacedFile::There { after, .. }) => after.push(waiter),
            Some(PlacedFile::Flushing { waiting, doomed, .. }) => {
                *doomed = false;
                waiting.push(waiter);
            }
            Some(
                PlacedFile::Probing(waiting)
                | PlacedFile::Checking(waiting)
                | PlacedFile::Writing(waiting)
                | PlacedFile::Basing { waiting, .. }
                | PlacedFile::Deriving { waiting, .. }
                | PlacedFile::Sealing { waiting, .. }
                | PlacedFile::Removing(waiting),
            ) => waiting.push(waiter),
            // alloc: one waiter refused, with why
            None => self.refuse_waiting(
                vec![waiter],
                "the whole file it was to be cloned from went while its place was begun",
            ),
        }
    }

    /// `waiter`'s place made without the whole file of `content`, which is not there to be
    /// cloned (being written, written back, derived or not yet made): `None` once begun,
    /// else the waiter back, to wait for the file.
    ///
    /// A derived value's place is a clone of its base's whole file, where that is there and
    /// written back, with the value's ranges written: the same recipe the store's own file
    /// of it is made by, onto the output's path. A whole value's place, while this run is
    /// writing its file (`writing`: the value's bytes are the file's), is those bytes written
    /// where they go. Neither waits for a write-back, and neither clones a file that has
    /// data to write back. A derived file's wait for its base (`base_of`) is never made
    /// here: it needs the base's file itself.
    fn direct(
        &mut self,
        content: ContentId,
        waiter: Waiter,
        writing: bool,
        effects: &mut Vec<Effect>,
    ) -> Option<Waiter> {
        if waiter.base_of.is_some() {
            return Some(waiter);
        }
        if self.derived.contains_key(&content) {
            let Some(base_id) = self.base_file_of(&content) else { return Some(waiter) };
            let base = match self.files.get_mut(&base_id) {
                Some(PlacedFile::There { file, cloning, lost: false, .. }) => {
                    // Counted, so the base stays until this clone of it ended; one let go
                    // of meanwhile ([`Store::unplace`]) stays let go of.
                    *cloning += 1;
                    // clone: a file's name, a reference count: the clone holds its source's
                    file.clone()
                }
                _ => return Some(waiter),
            };
            let Waiter { asked, to, raw, .. } = waiter;
            let content = Content::Patch { base, base_id, id: content, len: raw };
            self.publish(asked, to, content, Durable::No, 0, effects);
            return None;
        }
        if !writing {
            return Some(waiter);
        }
        let key = waiter.key;
        let bytes = match self.look().get(&key) {
            Ok(Some(entry)) if entry.content == content => {
                // alloc: the value's bytes, which the write's effect owns; once per place
                let mut bytes = Vec::with_capacity(entry.bytes.len());
                copy::append(&mut bytes, entry.bytes);
                bytes
            }
            // Not readable now: the wait for the file says why when it ends.
            _ => return Some(waiter),
        };
        let Waiter { asked, to, .. } = waiter;
        self.publish(asked, to, Content::Bytes(bytes), Durable::No, 0, effects);
        None
    }

    /// The whole file the derived value `id` is made from: its base's, by that file's id.
    /// `None` when `id`'s recipe or its base cannot be read, or the base is kept as no whole
    /// file (the wait for `id`'s own file says which).
    fn base_file_of(&self, id: &ContentId) -> Option<ContentId> {
        let look = self.look();
        let entry = look.get(&Key(*id)).ok()??;
        if entry.kind != Kind::Derived {
            return None;
        }
        let base = derived::decode(entry.bytes).ok()?.base;
        match whole_of(look, &base) {
            Ok(Ok(whole)) => Some(whole.id),
            _ => None,
        }
    }

    /// Whether a derived value's whole file found under its name is looked at and kept: never,
    /// but under the planted bug.
    fn keeps_derived(&self) -> bool {
        #[cfg(test)]
        {
            self.plant == PlacePlant::KeepsDerived
        }
        #[cfg(not(test))]
        {
            false
        }
    }

    /// The whole file of `content` is there, written back (`file`) and counted as cloned by
    /// `waiter`: cloned to its path, or, for one that waits on it as its base, to where the
    /// file derived from it is made. `written`, the bytes written to make the file that
    /// `waiter` is the first to report.
    fn serve(
        &mut self,
        content: ContentId,
        file: &Flushed,
        waiter: Waiter,
        written: u64,
        effects: &mut Vec<Effect>,
    ) {
        match waiter.base_of {
            None => self.clone_file(content, file, waiter, written, effects),
            Some(id) => self.derive_from(content, id, file, waiter, written, effects),
        }
    }

    /// `waiter`'s path cleared and the whole file of `content` cloned there; `written`, the
    /// bytes it wrote to make that file.
    fn clone_file(
        &mut self,
        content: ContentId,
        file: &Flushed,
        waiter: Waiter,
        written: u64,
        effects: &mut Vec<Effect>,
    ) {
        let Waiter { asked, to, key, raw, retried, base_of: _ } = waiter;
        let of = Some(Source { content, key, retried });
        // clone: a file's name, a reference count: the clone's op holds its source's
        let content = Content::File { from: file.clone(), content, of, len: raw };
        self.publish(asked, to, content, Durable::No, written, effects);
    }

    /// The place `asked` of `from`'s clone at `to` begun ([`Staged`]): its staged name cleared
    /// by the effect it returns, then `from` cloned there ([`Op::Clear`]) and renamed onto `to`
    /// ([`Op::Commit`]).
    fn clear_staged(
        &mut self,
        asked: Tag,
        from: Flushed,
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
        let Some(PlacedFile::There { cloning, doomed, lost, after, .. }) =
            self.files.get_mut(&content)
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
        // Not there to be cloned from, so not a file whose write-back anything waits for.
        self.shared.settled(&content);
        let file = self.placed_file(&content);
        self.files.insert(content, PlacedFile::Removing(waiting));
        let tag = self.tags.ask(Op::Unplace { content });
        effects.push(Effect::Remove { file, tag });
    }

    /// The whole file of `content` written from the value of a place waiting on it (the first
    /// whose key still has it), read and verified now.
    fn make_file(&mut self, content: ContentId, waiting: Vec<Waiter>, effects: &mut Vec<Effect>) {
        if self.derived.contains_key(&content) {
            return self.derive_file(content, waiting, effects);
        }
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
                // Each output waiting is written where it goes from the value's bytes, now:
                // none waits for the store's file, its seal or its write-back, and none is a
                // clone of a file just written (which would wait for all of it to be written
                // out). What waits on is a derived file's wait for this one, its base.
                // alloc: the places split, once per whole file written
                let (outputs, waiting): (Vec<Waiter>, Vec<Waiter>) =
                    waiting.into_iter().partition(|it| it.base_of.is_none());
                // The first place reports the bytes written into the store; none, the file.
                let mut written = bytes.len() as u64;
                for Waiter { asked, to, .. } in outputs {
                    // alloc: a copy of the value for the path's own write, once per path
                    // clone: the output's write owns its bytes, as the store's file's does
                    let copy = Content::Bytes(bytes.clone());
                    let written = std::mem::take(&mut written);
                    self.publish(asked, to, copy, Durable::No, written, effects);
                }
                let file = self.placed_file(&content);
                self.shared.unflushed(content);
                self.files.insert(content, PlacedFile::Writing(waiting));
                let tag = self.tags.ask(Op::MakeFile { content, written });
                // Not synced: the file is derived data, renamed to its name whole, and one
                // an earlier run left is hashed against its name before it is cloned from.
                // Its write-back is begun once it is sealed ([`Store::flush_file`]).
                effects.push(publish::write(file, bytes, Durable::Started, tag));
            }
            Err(why) => self.refuse_waiting(waiting, &why),
        }
    }

    /// The derived whole file `id` begun for `waiting` (none: made ahead of its place,
    /// [`Store::unplace`]): its recipe read, and the whole file of its base waited for, which
    /// is made first where it is not there (written from its value, or derived in its turn).
    /// A recipe that cannot be read, a base the store keeps no whole file of, and a chain of
    /// more than [`derived::CHAIN_MOST`] derived values with no whole file refuse `waiting`,
    /// naming which.
    fn derive_file(&mut self, id: ContentId, waiting: Vec<Waiter>, effects: &mut Vec<Effect>) {
        let key = Key(id);
        let look = self.look();
        let base = match look.get(&key) {
            Ok(Some(entry)) if entry.kind == Kind::Derived => derived::decode(entry.bytes)
                .map(|recipe| recipe.base)
                // alloc: an error, once per place that fails
                .map_err(|why| format!("the derived value {key:?} holds {why}")),
            // alloc: an error, once per place that fails
            Ok(_) => Err(format!(
                "the store no longer has the derived value {key:?} it was asked to place"
            )),
            // alloc: the error of a corrupt value
            Err(corrupt) => Err(corrupt.to_string()),
        };
        let whole = base.and_then(|base| match whole_of(look, &base) {
            Ok(Ok(whole)) => Ok((base, whole)),
            // alloc: an error, once per place that fails
            Ok(Err((kind, len))) => Err(format!(
                "the derived value {key:?} is made from {base:?}, a {} value of {len} bytes: \
                 the store keeps a whole file to clone only of an output of {CLONE_FROM} bytes \
                 or more and of a derived value",
                kind.name()
            )),
            // alloc: an error, once per place that fails
            Err(why) => Err(format!("the derived value {key:?} is made from {base:?}: {why}")),
        });
        // A value derived from itself through others would wait for itself: refused, as a
        // chain past the bound is.
        let whole = whole.and_then(|(base, whole)| {
            let mut at = whole.id;
            for _ in 0..=derived::CHAIN_MOST {
                if at == id {
                    // alloc: an error, once per place that fails
                    return Err(format!(
                        "the derived value {key:?} is made from {base:?}, which is derived \
                         from it in turn: a derived value's chain ends at a whole file"
                    ));
                }
                match self.files.get(&at) {
                    Some(PlacedFile::Basing { base: next, .. }) => at = *next,
                    _ => break,
                }
            }
            match self.derive_depth < derived::CHAIN_MOST {
                true => Ok((base, whole)),
                // alloc: an error, once per place that fails
                false => Err(format!(
                    "the derived value {key:?} is the last of more than {} derived values in \
                     a row with no whole file kept of any: a chain that long is not made",
                    derived::CHAIN_MOST
                )),
            }
        });
        match whole {
            Err(why) => self.refuse_waiting(waiting, &why),
            Ok((base, Whole { id: base_id, len, base: base_base })) => {
                if base_base.is_some() {
                    self.derived.insert(base_id, len);
                }
                self.files.insert(id, PlacedFile::Basing { waiting, base: base_id });
                let waiter = Waiter {
                    asked: self.tags.fresh(),
                    to: self.deriving_file(&id),
                    key: base,
                    raw: len,
                    retried: false,
                    base_of: Some(id),
                };
                self.derive_depth += 1;
                self.place_file(base_id, waiter, effects);
                self.derive_depth -= 1;
            }
        }
    }

    /// The whole file of `base` is there, counted as cloned by `waiter`, the derived file
    /// `id`'s wait for it: the derive begins. The name `id` is made under is cleared, then the
    /// base cloned there ([`Op::DeriveClear`], [`Op::DeriveClone`]). `written`, the bytes
    /// written to make the base, where this is the first to report them.
    fn derive_from(
        &mut self,
        base: ContentId,
        id: ContentId,
        from: &Flushed,
        waiter: Waiter,
        written: u64,
        effects: &mut Vec<Effect>,
    ) {
        let waiting = match self.files.remove(&id) {
            Some(PlacedFile::Basing { waiting, .. }) => waiting,
            // The derive ended meanwhile (refused), or another wait of it came first: the
            // base is not cloned for this one.
            other => {
                if let Some(state) = other {
                    self.files.insert(id, state);
                }
                return self.cloned(base, effects);
            }
        };
        let file = self.placed_file(&id);
        // No parent: the derive ends on a later completion, under whatever request is
        // current then.
        let span = tracing::info_span!(
            target: "blaze",
            parent: None,
            "store derive",
            %file,
            ranges = tracing::field::Empty,
            bytes = tracing::field::Empty,
        );
        let state = PlacedFile::Deriving { waiting, left: 0, failed: None, written, span };
        self.files.insert(id, state);
        // Written to from here until its write-back completes: no base of a derive.
        self.shared.unflushed(id);
        // clone: a file's name, a reference count: the clone's op holds its source's
        self.tags.ops.insert(waiter.asked, Op::DeriveClear { id, base, from: from.clone() });
        effects.push(Effect::Remove { file: waiter.to, tag: waiter.asked });
    }

    /// The derived file `id`'s ranges written into `into` and its length set there, each an
    /// effect of its own, asked as `op` gives ([`Op::DeriveWrite`] for the store's own file,
    /// [`Op::PatchWrite`] for an output's): no range ends past the length (a recipe's rule),
    /// so their order does not matter. How many effects, and the bytes they write.
    fn derive_writes(
        &mut self,
        id: ContentId,
        into: &FileName,
        effects: &mut Vec<Effect>,
        op: impl Fn() -> Op,
    ) -> Result<(u32, u64), String> {
        let key = Key(id);
        let (len, mut writes) = match self.look().get(&key) {
            Ok(Some(entry)) if entry.kind == Kind::Derived => match derived::decode(entry.bytes) {
                Ok(recipe) => {
                    // alloc: each range's bytes, which its effect owns
                    let writes: Vec<(u64, Vec<u8>)> =
                        recipe.ranges().map(|(at, bytes)| (at, bytes.to_vec())).collect();
                    (recipe.len, writes)
                }
                // alloc: an error, once per place that fails
                Err(why) => return Err(format!("the derived value {key:?} holds {why}")),
            },
            // alloc: an error, once per place that fails
            Ok(_) => return Err(format!("the store no longer has the derived value {key:?}")),
            // alloc: the error of a corrupt value
            Err(corrupt) => return Err(corrupt.to_string()),
        };
        // The planted bug: the last range is left out.
        #[cfg(test)]
        if self.plant == PlacePlant::SkipsARange {
            writes.pop();
        }
        let mut bytes = 0;
        let count = writes.len() as u32 + 1;
        for (at, range) in writes.drain(..) {
            bytes += range.len() as u64;
            let tag = self.tags.ask(op());
            // clone: each effect owns the name of the file it writes
            effects.push(Effect::WriteAt { file: into.clone(), at, bytes: range, tag });
        }
        let tag = self.tags.ask(op());
        // clone: the effect owns the name of the file it cuts
        effects.push(Effect::Truncate { file: into.clone(), len, tag });
        Ok((count, bytes))
    }

    /// The derive of `id` failed with `why` after its clone was begun: what it made under its
    /// other name goes, and the places waiting on it are refused, naming the file.
    fn derive_failed(&mut self, id: ContentId, why: &str, effects: &mut Vec<Effect>) {
        let temp = self.deriving_file(&id);
        // clone: the effect and the op each own the name; once per derive that fails
        let tag = self.tags.ask(Op::Discard { file: temp.clone() });
        effects.push(Effect::Remove { file: temp, tag });
        self.derive_refused(id, why);
    }

    /// The derived file `id` is not made, for `why`: the places waiting on it are refused,
    /// and it is begun afresh by the next that asks.
    fn derive_refused(&mut self, id: ContentId, why: &str) {
        let waiting = match self.files.remove(&id) {
            Some(PlacedFile::Basing { waiting, .. } | PlacedFile::Deriving { waiting, .. }) => {
                waiting
            }
            Some(state) => {
                self.files.insert(id, state);
                return;
            }
            None => return,
        };
        // Not made: no file whose write-back anything waits for.
        self.shared.settled(&id);
        let file = self.placed_file(&id);
        // alloc: an error, once per place that fails
        self.refuse_waiting(waiting, &format!("deriving {file}: {why}"));
    }

    /// How a step of the derive of `id` went ([`PlacedFile::Deriving`]).
    fn derive_step(&mut self, op: Op, done: Done, effects: &mut Vec<Effect>) {
        match op {
            Op::DeriveClear { id, base, from } => match done {
                Done::Removed => {
                    let to = self.deriving_file(&id);
                    let tag = self.tags.ask(Op::DeriveClone { id, base });
                    effects.push(from.clone_to(to, tag));
                }
                done => {
                    self.cloned(base, effects);
                    let temp = self.deriving_file(&id);
                    // alloc: an error, once per place that fails
                    let why = format!("clearing {temp} to clone its base there: {}", describe(&done));
                    self.derive_refused(id, &why);
                }
            },
            Op::DeriveClone { id, base } => {
                self.cloned(base, effects);
                let temp = self.deriving_file(&id);
                if !matches!(done, Done::Cloned) {
                    let from = self.placed_file(&base);
                    // alloc: an error, once per place that fails
                    let why = format!("cloning its base {from} to {temp}: {}", describe(&done));
                    return self.derive_failed(id, &why, effects);
                }
                // The planted bug: renamed to its name now, its ranges written after.
                #[cfg(test)]
                if self.plant == PlacePlant::RenameBeforeRanges {
                    return self.derive_rename(id, effects);
                }
                match self.derive_writes(id, &temp, effects, || Op::DeriveWrite { id }) {
                    Ok((count, bytes)) => {
                        if let Some(PlacedFile::Deriving { left, written, span, .. }) =
                            self.files.get_mut(&id)
                        {
                            *left = count;
                            *written += bytes;
                            span.record("ranges", count - 1);
                            span.record("bytes", bytes);
                        }
                    }
                    Err(why) => self.derive_failed(id, &why, effects),
                }
            }
            Op::DeriveWrite { id } => {
                let Some(PlacedFile::Deriving { left, failed, .. }) = self.files.get_mut(&id)
                else {
                    return;
                };
                *left = left.saturating_sub(1);
                if !matches!(done, Done::Written | Done::Truncated) && failed.is_none() {
                    *failed = Some(describe(&done));
                }
                if *left > 0 {
                    return;
                }
                match failed.take() {
                    None => self.derive_rename(id, effects),
                    Some(why) => {
                        // alloc: an error, once per place that fails
                        let why = format!("writing its ranges into its base's clone: {why}");
                        self.derive_failed(id, &why, effects)
                    }
                }
            }
            Op::DeriveRename { id } => {
                if !matches!(done, Done::Renamed) {
                    let temp = self.deriving_file(&id);
                    // alloc: an error, once per place that fails
                    let why = format!("renaming {temp} to its name: {}", describe(&done));
                    return self.derive_failed(id, &why, effects);
                }
                let (waiting, written) = match self.files.remove(&id) {
                    // The span of the derive ends here, with the file under its name.
                    Some(PlacedFile::Deriving { waiting, written, .. }) => (waiting, written),
                    other => {
                        if let Some(state) = other {
                            self.files.insert(id, state);
                        }
                        return;
                    }
                };
                // The planted bug: its ranges are written only now, into the file under its
                // name, which the seal below lets places clone at once.
                #[cfg(test)]
                if self.plant == PlacePlant::RenameBeforeRanges {
                    let file = self.placed_file(&id);
                    _ = self.derive_writes(id, &file, effects, || Op::DeriveWrite { id });
                }
                self.seal(id, waiting, written, effects);
            }
            _ => unreachable!("not a step of a derive"),
        }
    }

    /// The derived file `id`, whole under the name it was made under, renamed to its own:
    /// one step, so a clone of that name is of the whole file or fails. Not synced.
    fn derive_rename(&mut self, id: ContentId, effects: &mut Vec<Effect>) {
        let (from, to) = (self.deriving_file(&id), self.placed_file(&id));
        let tag = self.tags.ask(Op::DeriveRename { id });
        effects.push(publish::rename(from, to, Durable::No, tag));
    }

    /// The places in `waiting` refused with `why`; a derived file that waited among them for
    /// its base is not made, and its own places are refused with it.
    fn refuse_waiting(&mut self, waiting: Vec<Waiter>, why: &str) {
        for Waiter { asked, to, base_of, .. } in waiting {
            if let Some(id) = base_of {
                self.derive_refused(id, why);
                continue;
            }
            // alloc: an error, once per place that fails
            self.answer(Placed {
                tag: asked,
                to,
                result: Err(why.to_owned()),
                bytes: PlacedBytes::default(),
            });
        }
    }

    /// `waiting` placed from the whole file of `content` once it is read-only again;
    /// `written`, the bytes this run wrote to make it (none for one it checked).
    fn seal(
        &mut self,
        content: ContentId,
        waiting: Vec<Waiter>,
        written: u64,
        effects: &mut Vec<Effect>,
    ) {
        let file = self.placed_file(&content);
        self.files.insert(content, PlacedFile::Sealing { waiting, written });
        let tag = self.tags.ask(Op::Seal { content });
        effects.push(Effect::Seal { file, mode: PLACED_MODE, tag });
    }

    /// The whole file of `content`, sealed, has its write-back begun ([`Durable::Started`]):
    /// nothing a build answers waits for it, and until it completes the file is a
    /// [`publish::Written`], which nothing clones. `waiting`, what can be made only of a
    /// clone of it, served once it is written back ([`Store::there`]).
    fn flush_file(
        &mut self,
        content: ContentId,
        waiting: Vec<Waiter>,
        written: u64,
        effects: &mut Vec<Effect>,
    ) {
        let file = publish::Written::wrote(self.placed_file(&content));
        let tag = self.tags.ask(Op::WriteBack { content });
        effects.push(file.flush(tag));
        // The planted bug: cloned from now, its write-back begun and not waited for.
        #[cfg(test)]
        if self.plant == PlacePlant::ClonesUnflushed {
            return self.there(content, file.planted_flushed(), waiting, written, false, effects);
        }
        // No parent: the write-back ends on a later completion, under whatever request is
        // current then.
        let span = tracing::info_span!(
            target: "blaze",
            parent: None,
            "flush started",
            file = %file.name(),
            waiting = waiting.len(),
        );
        let state = PlacedFile::Flushing { waiting, written, file, doomed: false, span };
        self.files.insert(content, state);
    }

    /// The whole file of `content` is there and written back (`file`): a base for a derive
    /// again ([`Reader::derivable`]), and cloned for each of `waiting`. One let go of
    /// meanwhile (`doomed`) goes, at once when nothing waits for it, else once its clones
    /// ended. `written`, the bytes this run wrote to make it that no place reported.
    fn there(
        &mut self,
        content: ContentId,
        file: Flushed,
        waiting: Vec<Waiter>,
        written: u64,
        doomed: bool,
        effects: &mut Vec<Effect>,
    ) {
        self.shared.settled(&content);
        if doomed && waiting.is_empty() {
            // alloc: empty, allocates nothing
            return self.remove_file(content, Vec::new(), effects);
        }
        // alloc: empty, allocates nothing
        let there = PlacedFile::There {
            // clone: a file's name, a reference count: each clone below holds its source's
            file: file.clone(),
            cloning: waiting.len() as u32,
            doomed,
            lost: false,
            after: Vec::new(),
            // Made ahead of any place: the first to come reports what making it wrote.
            owed: if waiting.is_empty() { written } else { 0 },
        };
        self.files.insert(content, there);
        for (at, waiter) in waiting.into_iter().enumerate() {
            // The first place made the file: its bytes are written once, for all.
            let written = if at == 0 { written } else { 0 };
            self.serve(content, &file, waiter, written, effects);
        }
    }

    /// How a step of the whole file of `content` went ([`PlacedFile`]).
    fn file_step(&mut self, op: Op, done: Done, effects: &mut Vec<Effect>) {
        let content = match op {
            Op::Probe { content }
            | Op::Check { content }
            | Op::MakeFile { content, .. }
            | Op::Seal { content }
            | Op::WriteBack { content }
            | Op::Unplace { content } => content,
            _ => unreachable!("not a step of a placed file"),
        };
        let Some(state) = self.files.remove(&content) else { return };
        let file = self.placed_file(&content);
        match (op, state, done) {
            // A derived value's is looked at only under the planted bug, which takes one of
            // the right length as it is; else it is made again whatever is there.
            (Op::Probe { .. }, PlacedFile::Probing(waiting), Done::Stat(Some(meta)))
                if self.derived.contains_key(&content) =>
            {
                match self.keeps_derived() && !meta.dir && meta.len == waiting[0].raw {
                    true => self.seal(content, waiting, 0, effects),
                    false => self.remove_file(content, waiting, effects),
                }
            }
            (Op::Probe { .. }, PlacedFile::Probing(waiting), Done::Stat(Some(meta)))
                if !meta.dir && meta.len == waiting[0].raw =>
            {
                // The planted bug: cloned from as it is found.
                #[cfg(test)]
                if self.plant == PlacePlant::UncheckedFile {
                    return self.seal(content, waiting, 0, effects);
                }
                // A file of the right length this run did not write (an earlier run's, or one
                // changed outside): its bytes are checked against its name before anything
                // clones it. Nothing of it was synced, so a crash may have left any part.
                self.files.insert(content, PlacedFile::Checking(waiting));
                let tag = self.tags.ask(Op::Check { content });
                effects.push(Effect::Map { file, tag });
            }
            (Op::Probe { .. }, PlacedFile::Probing(waiting), Done::Stat(Some(meta))) => {
                // A write this run saw end is whole, so this is what a crash left of one (it
                // was not synced) or a file blaze did not write. It goes before it is written
                // again: a rename over a read-only file is refused on Windows.
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
                    self.seal(content, waiting, 0, effects);
                } else {
                    // alloc: a problem reported
                    self.placed_problem(format!(
                        "{file} holds bytes of {found:x}, not of its name, and is written again"
                    ));
                    self.remove_file(content, waiting, effects);
                }
            }
            (Op::MakeFile { written, .. }, PlacedFile::Writing(waiting), Done::Written) => {
                self.seal(content, waiting, written, effects);
            }
            (Op::Seal { .. }, PlacedFile::Sealing { waiting, written }, Done::Moded) => {
                self.flush_file(content, waiting, written, effects);
            }
            (
                Op::WriteBack { .. },
                PlacedFile::Flushing { waiting, written, file, doomed, span },
                done,
            ) => {
                // The write-back's span ends here.
                drop(span);
                // The witness: only this completion, of this file's write-back, makes the
                // file one a clone is asked of ([`publish::Written::flushed`]).
                match file.flushed(&done) {
                    // The file is clean, and may be cloned.
                    Ok(file) => self.there(content, file, waiting, written, doomed, effects),
                    Err(file) => {
                        // Not known written back, so never cloned from: it goes, what waited
                        // for it is refused by name, and the next place writes it again.
                        // alloc: an error, once per place that fails
                        let why = format!(
                            "writing {} back before it is cloned from: {}",
                            file.name(),
                            describe(&done)
                        );
                        self.refuse_waiting(waiting, &why);
                        // alloc: empty, allocates nothing
                        self.remove_file(content, Vec::new(), effects);
                    }
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
            // Being derived, or waiting for its base: a step of an earlier state of it that
            // comes now changes nothing.
            (_, state @ (PlacedFile::Basing { .. } | PlacedFile::Deriving { .. }), _) => {
                self.files.insert(content, state);
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
                // Not made: no file whose write-back anything waits for.
                self.shared.settled(&content);
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
        self.tags.range.owns(tag) || self.hot.owns(tag)
    }

    /// Takes a completion of its own: a mapping, an append, a sync. Says which puts its
    /// commits made durable. Not every one: [`Store::flush`] with nothing to write can also
    /// move a segment's durable count (puts that ended empty, such as a claim's loser) with
    /// no completion, so a waiter checks [`Store::durable`] rather than wait on this list.
    pub fn complete(&mut self, completion: Completion, effects: &mut Vec<Effect>) -> Vec<Ticket> {
        self.tidy();
        if self.hot.owns(completion.tag) {
            return self.tail_loaded(completion, effects);
        }
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
                Done::Mapped(mapping) => self.install(tier, file, what, mapping, effects),
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
                    effects.push(from.clone_to(into, tag));
                }
                done => {
                    if let Some(source) = of {
                        self.cloned(source.content, effects);
                    }
                    self.answer(Placed {
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
                        // Not synced: the path holds its old file or the clone, whole, and
                        // a crash that undoes the rename leaves the old one, placed again by
                        // the build that finds it.
                        effects.push(publish::rename(from, into, Durable::No, tag));
                    }
                    (Done::Cloned | Done::Written, _, _) => {
                        self.answer(Placed { tag: asked, to, result: Ok(()), bytes })
                    }
                    (done, Some(Source { content, key, retried: false }), _) => {
                        // Whether the file it cloned from is gone or no file, which is made
                        // again once.
                        let waiter = Waiter {
                            asked,
                            to,
                            key,
                            raw: bytes.cloned,
                            retried: true,
                            base_of: None,
                        };
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
                        self.answer(Placed {
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
                    self.answer(Placed { tag: asked, to, result: Ok(()), bytes })
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
                    // A derived value's file has no hash of its own to check `to` against.
                    Done::Mapped(_) if self.derived.contains_key(&content) => Err(format!(
                        "renaming {temp} over {to}: {why}; {to} is placed from a derived \
                         value's whole file, which is known by how it was made and not by a \
                         hash of its bytes, so what {to} holds cannot be checked"
                    )),
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
                self.answer(Placed { tag: asked, to, result, bytes });
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
            | Op::WriteBack { .. }
            | Op::Unplace { .. }) => {
                self.file_step(op, done, effects);
            }
            op @ (Op::PatchClear { .. }
            | Op::PatchClone { .. }
            | Op::PatchWrite { .. }
            | Op::PatchRename { .. }) => {
                self.patch_step(op, done, effects);
            }
            Op::Runs { asked, step } => self.runs_step(asked, step, &done, effects),
            op @ (Op::DeriveClear { .. }
            | Op::DeriveClone { .. }
            | Op::DeriveWrite { .. }
            | Op::DeriveRename { .. }) => {
                self.derive_step(op, done, effects);
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
            Op::RemoveChecks { pack } => {
                if !matches!(done, Done::Removed) {
                    let file = self.tiers[0].file(&disk::checks_file(pack));
                    // alloc: a problem reported
                    self.problems.push(format!(
                        "{file} was left behind (it is removed at the next open): {}",
                        describe(&done)
                    ));
                }
            }
            Op::Checks { tier, pack } => self.checks_read(tier, pack, done, effects),
            // The planted commit wrote its head without it.
            #[cfg(test)]
            Op::Unwaited => {}
            Op::Peers => self.peers_listed(done, effects),
            Op::Announce => self.announced(&done, effects),
            op => return self.step(&op, &done, effects),
        }
        self.peers_ask(effects);
        // alloc: empty, allocates nothing
        Vec::new()
    }

    /// Takes a mapping it asked for (of an index table or a file of its own; no pack is
    /// mapped). Separate from [`Store::complete`] for drivers that map by themselves at
    /// start. What the opening asks next (a pumped store's loads of its packs) goes into
    /// `effects`.
    pub fn mapped(&mut self, file: &FileName, mapping: Mapping, effects: &mut Vec<Effect>) {
        self.tidy();
        let asked = self
            .tags
            .ops
            .iter()
            .filter_map(|(tag, op)| match op {
                Op::Map { file: it, .. } if it == file => Some(*tag),
                _ => None,
            })
            .max();
        let Some(tag) = asked else { return };
        if let Some(Op::Map { tier, file, what }) = self.tags.ops.remove(&tag) {
            self.install(tier, file, what, mapping, effects);
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
            base: None,
            usable: 0,
            // alloc: once per pack made (a unit's first, or one past `PACK_MOST`); appends then reuse it
            memory: disk::pack_header(unit.0).to_vec(),
            load_failed: None,
            committed: 0,
            // alloc: empty, allocates nothing
            checks: Vec::new(),
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
            self.publish_view();
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
        self.publish_view();
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
        self.publish_view();
    }

    /// Lets go of a test's hold ([`Store::hold_moves`]) on the moves of batch `id`.
    #[cfg(test)]
    pub(crate) fn release_moves(&mut self, id: u64) {
        if let Some(batch) = self.pending.iter_mut().find(|it| it.id == id && it.held) {
            batch.held = false;
            batch.maps -= 1;
        }
        self.finish_batches();
    }

    /// Publishes what readers see, and frees the view it replaces at task boundaries.
    fn publish_view(&mut self) {
        // clone: the view readers share, made once a publish (see `Tier::view`), with an `Arc` of the encodings
        let view = View {
            tiers: self.tiers.iter().map(Tier::view).collect(),
            columns: self.columns.clone(),
        };
        let old = self.shared.publish(view);
        self.limbo.retire(old);
        // What was removed before this view is in no view a task can load from now on: its
        // pages go back once the tasks that began before have ended.
        self.hot.published();
    }

    /// What every `&mut` entry point does first, before it finds anything: frees what passed
    /// its grace, ends an index swap whose writers are gone, and lets go of the values made
    /// whole once they hold too much.
    fn tidy(&mut self) {
        // alloc: none, `Limbo::collect` frees what passed its grace; it is not `Iterator::collect`
        self.limbo.collect();
        // A table of pins freed just now let its pages go: a read waiting at the pinned bound
        // looks again.
        self.shared.pins_let_go();
        let own = &self.tiers[0];
        for table in std::iter::once(&own.delta).chain(&own.prev) {
            // alloc: none, `Table::collect` drops swapped-out values and keeps their slots; it is not `Iterator::collect`
            table.collect();
        }
        self.end_migration();
        // The table also holds the pins of the packs' pages its readers read: let go once
        // they are a quarter of the pinned bound, so the pins of tasks that ended are given
        // back long before running tasks reach the bound.
        let owned = self.shared.owned(Held::writer(&*self.shared));
        let pins_most = pool::counts::pinned_bound(pool::counts::budget()) / 4;
        if owned.bytes() > OWNED_MOST || owned.pinned() > pins_most {
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
                self.publish_view();
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
        #[cfg(test)]
        let drains = !self.plant_no_drain;
        #[cfg(not(test))]
        let drains = true;
        // In a task of the store's tag, so the packs' bytes the drain copies are named on the
        // daemon's memory line, whichever thread commits.
        if drains {
            blaze_mem::frame::task(blaze_mem::Tag::Store, |_| self.drain());
        }
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
        // The check of each block this commit adds to each pack: `committed` and the end are
        // block ends, so they are whole blocks, and none of them changes after.
        let checks: Vec<Vec<u32>> = packs
            .iter()
            .map(|&(at, to)| {
                let pack = &own.packs[at];
                let added = (pack.committed - pack.usable) as usize..(to - pack.usable) as usize;
                checks::of(&pack.memory[added])
            })
            // alloc: once per commit, the checks it writes, held in the `Commit`
            .collect();
        let mut steps = 0;
        for content in &externals {
            let external = &own.externals[content];
            // clone: the write owns its bytes, and a commit that fails writes them again from these
            let bytes = external.memory.clone().expect("a file not yet written is held");
            let tag = self.tags.ask(Op::WriteExternal { content: *content });
            // clone: the effect owns its file name; once per file a commit
            effects.push(Effect::Write { file: external.file.clone(), bytes, tag }); // durable: the store's commit: a file the head will name is on disk before the head is
            steps += 1;
        }
        let mut batch = std::mem::take(&mut self.draining);
        batch.id = self.next_batch;
        self.next_batch += 1;
        let index = index.map(|(slot, bytes)| {
            let tag = self.tags.ask(Op::WriteIndex);
            // durable: the store's commit: the index the head will name is on disk before the head is
            effects.push(Effect::Write { file: own.file(&disk::index_file(slot)), bytes, tag });
            steps += 1;
            (slot, batch.id)
        });
        let written = packs.len();
        self.commit = Some(Commit {
            packs,
            checks,
            // alloc: once per commit, a place for each pack's append
            appends: (0..written).map(|_| None).collect(),
            // alloc: once per commit, a count for each pack
            writes: vec![0; written],
            externals,
            index,
            head,
            batch,
            steps,
            failed: None,
            head_sent: false,
        });
        for n in 0..written {
            self.begin_pack(n, effects);
        }
        // Every append failed to begin with nothing else asked: no completion will come to
        // move the commit, so it is tried again (or the store stopped) from here.
        if self.commit.as_ref().is_some_and(|it| it.steps == 0 && it.failed.is_some()) {
            _ = self.commit_moves(effects);
        }
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
        // Whether the head waits for the syncs of the packs and their checks: always, but
        // under the planted bug.
        #[cfg(test)]
        let waited = !self.plant_head_before_syncs;
        #[cfg(not(test))]
        let waited = true;
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
            Op::TruncateChecks { pack } | Op::WriteChecks { pack } | Op::SyncChecks { pack } => {
                own.file(&disk::checks_file(*pack))
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
                    let n = commit
                        .packs
                        .iter()
                        .position(|(it, _)| *it == at)
                        .expect("a pack the commit truncated it writes");
                    // Written from the pool's pages its append put the bytes into, around the
                    // page cache: `committed` and the end are block ends (each commit pads its
                    // packs), and the pages are the write's buffer, pinned by the append until
                    // the commit ends.
                    let parts =
                        commit.appends[n].as_ref().expect("a pack cut by the commit has its append").parts();
                    commit.writes[n] = parts.len() as u32;
                    for (at, from) in parts {
                        let tag = self.tags.ask(Op::WritePack { pack: commit.packs[n].0 });
                        // clone: the effect owns its file name; once per write of a pack a commit
                        effects.push(Effect::WriteFrom {
                            file: pack.file.clone(),
                            at,
                            from,
                            cache: Cache::Bypassed,
                            tag,
                        });
                        commit.steps += 1;
                    }
                }
            }
            (Op::WritePack { pack }, Done::Written) => {
                let n = commit.packs.iter().position(|(it, _)| it == pack);
                let last = n.is_none_or(|n| {
                    commit.writes[n] = commit.writes[n].saturating_sub(1);
                    commit.writes[n] == 0
                });
                if commit.failed.is_none() && last {
                    let tag = self.tags.ask(sync_op(Op::Sync { pack: *pack }, waited));
                    // clone: the effect owns its file name; once per pack a commit
                    effects.push(Effect::Sync { file: own.packs[*pack].file.clone(), tag }); // durable: the store's commit: a pack's appended bytes are on disk before the head names them
                    commit.steps += usize::from(waited);
                    if !waited {
                        // The planted commit waits for no sync: its checks follow the write.
                        let tag = self.tags.ask(Op::TruncateChecks { pack: *pack });
                        effects.push(Effect::Truncate {
                            file: own.file(&disk::checks_file(*pack)),
                            len: checks::len_of(own.packs[*pack].committed),
                            tag,
                        });
                        commit.steps += 1;
                    }
                }
            }
            // A pack's checks follow its own sync, one effect at a time: cut to those of what
            // is committed (a torn append of an earlier commit that failed, or that a crash cut
            // short, goes first), written, synced. In a chain behind the pack rather than beside
            // it, so the store model's search over a commit's orders stays within its bound
            // (docs/v2/plan/design/store-pool.md, lane 4b).
            (Op::Sync { pack }, Done::Synced) => {
                if commit.failed.is_none() {
                    let tag = self.tags.ask(Op::TruncateChecks { pack: *pack });
                    effects.push(Effect::Truncate {
                        file: own.file(&disk::checks_file(*pack)),
                        len: checks::len_of(own.packs[*pack].committed),
                        tag,
                    });
                    commit.steps += 1;
                }
            }
            (Op::TruncateChecks { pack }, Done::Truncated) => {
                if commit.failed.is_none() {
                    let at = *pack;
                    let added =
                        commit.checks_of(at).expect("a pack whose checks it cut the commit writes");
                    let tag = self.tags.ask(Op::WriteChecks { pack: at });
                    effects.push(Effect::WriteAt {
                        file: own.file(&disk::checks_file(at)),
                        at: checks::len_of(own.packs[at].committed),
                        bytes: checks::encode(added),
                        tag,
                    });
                    commit.steps += 1;
                }
            }
            (Op::WriteChecks { pack }, Done::Written) => {
                if commit.failed.is_none() {
                    let tag = self.tags.ask(sync_op(Op::SyncChecks { pack: *pack }, waited));
                    let file = own.file(&disk::checks_file(*pack));
                    // durable: the store's commit: a pack's checks are on disk before the head names its new length
                    effects.push(Effect::Sync { file, tag });
                    commit.steps += usize::from(waited);
                }
            }
            (Op::SyncChecks { .. }, Done::Synced)
            | (Op::WriteExternal { .. } | Op::WriteIndex, Done::Written) => {}
            (Op::WriteHead, Done::Written) => return self.finish_commit(effects),
            (op, done) => {
                if commit.failed.is_none() {
                    commit.failed = Some(Broken { file: file_of(op), what: describe(done) });
                }
            }
        }
        self.commit_moves(effects)
    }

    /// What a commit does once a step of it has answered and none is in flight: after a
    /// failure, the commit again (or, [`FAILURES_MOST`] in a row, the store stopped); else the
    /// head, once everything it names is durable.
    fn commit_moves(&mut self, effects: &mut Vec<Effect>) -> Vec<Ticket> {
        // alloc: empty, allocates nothing
        let Some(commit) = self.commit.as_mut() else { return Vec::new() };
        if commit.steps > 0 {
            // alloc: empty, allocates nothing
            return Vec::new();
        }
        if let Some(failed) = commit.failed.take() {
            // What it drained is still in the packs' memory: the next commit writes it again.
            // Its appends go with it: the pages they took are given back.
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
            // durable: the store's head is its commit point: what it names is what a reopen after a crash holds
            effects.push(Effect::Write {
                file: self.tiers[0].file(HEAD_FILE),
                bytes: commit.head.encode(),
                tag,
            });
        }
        // alloc: empty, allocates nothing
        Vec::new()
    }

    /// The append of the commit in flight to its `n`th pack begun, and its bytes written into
    /// the pool; or, when the page the pack's committed bytes end in must be read first, that
    /// read asked, as a step of the commit ([`Store::tail_loaded`] goes on from it).
    fn begin_pack(&mut self, n: usize, effects: &mut Vec<Effect>) {
        let commit = self.commit.as_mut().expect("a commit in flight");
        let (at, to) = commit.packs[n];
        let file = &self.tiers[0].packs[at].file;
        match self.hot.begin_append(at, file, to, effects) {
            Ok(Some(append)) => self.write_pack(n, append, effects),
            Ok(None) => commit.steps += 1,
            Err(why) => {
                // clone: the error of a commit that fails
                commit.failed.get_or_insert(Broken { file: file.clone(), what: why });
            }
        }
    }

    /// The bytes the commit in flight adds to its `n`th pack put into the pack's window through
    /// `append`, at the offsets they will have on disk, and the pack's file asked to be cut to
    /// what is committed: its write, from the pool's pages, follows the cut.
    fn write_pack(&mut self, n: usize, mut append: hot::PackAppend, effects: &mut Vec<Effect>) {
        let commit = self.commit.as_mut().expect("a commit in flight");
        let (at, to) = commit.packs[n];
        let pack = &self.tiers[0].packs[at];
        let added = (pack.committed - pack.usable) as usize..(to - pack.usable) as usize;
        if let Err(err) = append.put(&pack.memory[added]) {
            // clone: the error of a commit that fails
            // alloc: the error of a commit that fails
            commit.failed.get_or_insert(Broken { file: pack.file.clone(), what: err.to_string() });
            return;
        }
        debug_assert_eq!(append.end(), to, "{}", pack.file);
        commit.appends[n] = Some(append);
        let tag = self.tags.ask(Op::Truncate { pack: at });
        // clone: the effect owns its file name; once per pack a commit
        effects.push(Effect::Truncate { file: pack.file.clone(), len: pack.committed, tag });
        commit.steps += 1;
    }

    /// A read of the pool's loads completed, on a pumped store: of a pack's last committed
    /// page, which an append of the commit in flight waits to pin (the append goes on, or the
    /// commit fails with why); or of what a pack commits, which its tier's opening waits for.
    fn tail_loaded(&mut self, completion: Completion, effects: &mut Vec<Effect>) -> Vec<Ticket> {
        for loaded in self.hot.loaded(completion, effects) {
            let (pack, begun) = match loaded {
                hot::Loaded::Append(pack, begun) => (pack, begun),
                hot::Loaded::Opened { tier, opening, pack, failed } => {
                    self.pack_loaded(tier, opening, pack, failed);
                    continue;
                }
            };
            // No commit in flight: the append, begun for none, gives its pin back.
            let Some(commit) = self.commit.as_mut() else { continue };
            commit.steps -= 1;
            self.commit_steps += 1;
            let n = commit.packs.iter().position(|(it, _)| *it == pack);
            match (begun, n) {
                (Ok(append), Some(n)) if commit.failed.is_none() => {
                    self.write_pack(n, append, effects);
                }
                (Ok(_), _) => {}
                (Err(why), _) => {
                    // clone: the error of a commit that fails
                    let file = self.tiers[0].packs[pack].file.clone();
                    commit.failed.get_or_insert(Broken { file, what: why });
                }
            }
        }
        self.commit_moves(effects)
    }

    fn finish_commit(&mut self, effects: &mut Vec<Effect>) -> Vec<Ticket> {
        let mut commit = self.commit.take().expect("a commit in flight");
        let own = &mut self.tiers[0];
        for (&(at, to), added) in commit.packs.iter().zip(&commit.checks) {
            let pack = &mut own.packs[at];
            pack.committed = to;
            pack.checks.extend_from_slice(added);
            debug_assert_eq!(pack.checks.len() as u64, to / disk::BLOCK, "{}", pack.file);
        }
        for (pack, written) in own.packs.iter_mut().zip(&commit.head.packs) {
            pack.indexed = written.indexed;
        }
        own.head = commit.head;
        // What the commit wrote of each pack is durable and named: the pages it wrote from are
        // the pack's loaded pages, read in place from now on.
        // No mapping is asked for: readers read them through the pool as far as `usable`, once
        // the view that says so is published (`finish_batches`), and what the pack held of
        // them in memory for the write goes.
        for (n, &(at, to)) in commit.packs.iter().enumerate() {
            let append = commit.appends[n].take().expect("a pack the commit wrote has its append");
            let base = self.hot.committed(at, append, &commit.checks[n]);
            let pack = &mut self.tiers[0].packs[at];
            pack.base = Some(base);
            if to >= pack.usable {
                pack.memory.drain(..(to - pack.usable) as usize);
                pack.usable = to;
            }
        }
        self.hot.commit_ended();
        for at in 0..self.tiers[0].packs.len() {
            let pack = &mut self.tiers[0].packs[at];
            if std::mem::take(&mut pack.retiring) {
                pack.removed = true;
                pack.base = None;
                // alloc: empty, allocates nothing; it lets go of the pack's memory
                pack.memory = Vec::new();
                // alloc: empty, allocates nothing; it lets go of the pack's checks
                pack.checks = Vec::new();
                self.remove_pack(at, effects);
                // Its pages go back once no reader can reach them (`Hot::published`).
                self.hot.pack_removed(0, at);
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
        batch.maps = commit.externals.len();
        #[cfg(test)]
        if self.hold_moves {
            batch.held = true;
            batch.maps += 1;
        }
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

    /// Asks for the block checks of pack `pack` of tier `tier`, whole: 1/1024 of their pack,
    /// kept for the pack's life.
    fn ask_checks(&mut self, tier: usize, pack: usize, effects: &mut Vec<Effect>) {
        let file = self.tiers[tier].file(&disk::checks_file(pack));
        let tag = self.tags.ask(Op::Checks { tier, pack });
        effects.push(Effect::ReadAll { file, tag });
    }

    /// The block checks of pack `number` of tier `at` came, as its tier opens: kept as far as
    /// the head's committed length. A file that holds fewer stops the opening, naming it.
    fn checks_read(&mut self, at: usize, number: usize, done: Done, effects: &mut Vec<Effect>) {
        let file = self.tiers[at].file(&disk::checks_file(number));
        let bytes = match done {
            Done::Read(bytes) => bytes,
            done if self.is_peer(at) => return self.peer_failed(at, file, describe(&done)),
            done => {
                if self.retry(&file, &done) {
                    self.ask_checks(at, number, effects);
                }
                return;
            }
        };
        let tier = &mut self.tiers[at];
        let opening = !tier.opened;
        let Some(pack) = tier.packs.get_mut(number) else { return };
        match checks::decode(&bytes, pack.committed) {
            Ok(checks) => pack.checks = checks,
            Err(why) => return self.tier_failed(at, file, why),
        }
        self.failures = 0;
        if opening {
            let tier = &mut self.tiers[at];
            tier.waiting -= 1;
            if tier.waiting == 0 && tier.head_read {
                self.finish_open(at, effects);
            }
        }
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
                base: None,
                usable: it.committed,
                // alloc: empty, allocates nothing
                memory: Vec::new(),
                load_failed: None,
                committed: it.committed,
                // alloc: empty, allocates nothing: read from its file below
                checks: Vec::new(),
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
        let removed: Vec<usize> =
            // alloc: opening, once per tier
            packs.iter().enumerate().filter(|(_, it)| it.removed).map(|(at, _)| at).collect();
        let checked: Vec<usize> =
            // alloc: opening, once per tier, the packs whose checks it reads
            packs.iter().enumerate().filter(|(_, it)| !it.removed).map(|(at, _)| at).collect();
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
        tier.waiting = maps.len() + checked.len();
        for (file, what) in maps {
            self.ask_map(at, file, what, effects);
        }
        for pack in checked {
            self.ask_checks(at, pack, effects);
        }
        if at == 0 {
            for pack in removed {
                self.remove_pack(pack, effects);
            }
        }
        if self.tiers[at].waiting == 0 {
            self.finish_open(at, effects);
        }
    }

    fn install(
        &mut self,
        at: usize,
        file: FileName,
        what: Mapped,
        mapping: Mapping,
        effects: &mut Vec<Effect>,
    ) {
        let opening = !self.tiers[at].opened;
        let len = mapping.bytes().len() as u64;
        let tier = &mut self.tiers[at];
        match what {
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
                self.finish_open(at, effects);
            }
        }
    }

    /// Tier `at`'s head, index table, files of their own and checks are read: its packs are
    /// opened in the store's pool, and the opening goes on ([`Store::finish_open_loaded`]),
    /// at once where a loader's thread loads what the opening's scan reads, or, on a pumped
    /// store, once what each pack commits is in memory (asked here, into `effects`).
    fn finish_open(&mut self, at: usize, effects: &mut Vec<Effect>) {
        if let Err((file, why)) = self.open_in_pool(at) {
            return self.tier_failed(at, file, why);
        }
        let mut loading = 0;
        if self.hot.pumped() {
            for number in 0..self.tiers[at].packs.len() {
                if !self.tiers[at].packs[number].removed && self.hot.load_whole(at, number, effects) {
                    loading += 1;
                }
            }
        }
        self.tiers[at].loading = loading;
        if loading == 0 {
            self.finish_open_loaded(at);
        }
    }

    /// A pumped store's load of what pack `pack` of tier `at` commits ended, for the opening
    /// `opening` of the tier: the tier's opening goes on once every pack's has.
    fn pack_loaded(&mut self, at: usize, opening: u8, pack: usize, failed: Option<String>) {
        // Of an opening given up (another writer's tier, read again): not this one's.
        if self.hot.opening_of(at) != Some(opening) {
            return;
        }
        let tier = &mut self.tiers[at];
        if let (Some(why), Some(pack)) = (failed, tier.packs.get_mut(pack)) {
            pack.load_failed.get_or_insert(why);
        }
        if tier.opened || tier.loading == 0 {
            return;
        }
        tier.loading -= 1;
        if tier.loading == 0 {
            self.finish_open_loaded(at);
        }
    }

    fn finish_open_loaded(&mut self, at: usize) {
        let look = Look::new(&self.shared, Held::writer(&*self.shared));
        match open_tier(&mut self.tiers[at], look) {
            Ok(()) => {
                let tier = &mut self.tiers[at];
                tier.opened = true;
                tier.before = None;
                self.publish_view();
                if self.is_peer(at) {
                    self.peer_settled(at);
                }
            }
            Err((file, why)) => self.tier_failed(at, file, why),
        }
    }

    /// Tier `at`, read and checked, opened in the pool: each pack with its blocks' checks, in
    /// place of what the pool held of the tier as it was open before (another writer's store
    /// whose head changed is opened afresh).
    fn open_in_pool(&mut self, at: usize) -> Result<(), (FileName, String)> {
        let tier = &mut self.tiers[at];
        // clone: the error that stops the opening
        self.hot.tier_closed(at).map_err(|why| (tier.dir.clone(), why))?;
        for (number, pack) in tier.packs.iter_mut().enumerate().filter(|(_, it)| !it.removed) {
            let base = self
                .hot
                .pack_opened(at, number, &pack.file, pack.committed, &pack.checks)
                // clone: the error that stops the opening
                .map_err(|why| (pack.file.clone(), why))?;
            pack.base = Some(base);
        }
        Ok(())
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

/// Checks what a tier's head names against what was mapped of its index table, and reads,
/// through the store's pool, the records past its index table and its dictionaries: of each
/// pack its header and what it commits past what the index holds, never the whole of it.
fn open_tier(tier: &mut Tier, look: Look<'_>) -> Result<(), (FileName, String)> {
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
        // What a pin of its bytes failed for: on a pumped store, what its load said of the
        // first page that could not be read (the pin itself only says the page is refused).
        let unread = |why: String| {
            let why = pack.load_failed.clone().unwrap_or(why); // clone: the error that stops the opening
            // alloc: the error that stops the opening
            fail(format!("{why}; its blocks are checked against {}", disk::checks_file(number)))
        };
        if pack.committed < PACK_HEADER {
            // alloc: the error that stops the opening
            return Err(fail(format!(
                "its head names {} bytes of it, shorter than a pack's header",
                pack.committed
            )));
        }
        let header = pack.committed_bytes(look, 0, PACK_HEADER).map_err(unread)?;
        disk::check_pack_header(header, pack.unit).map_err(|why| fail(why.into()))?;
        // The scan's bytes, from the block it starts in: `all[n]` is the pack's byte
        // `from + n`.
        let first = pack.indexed.max(PACK_HEADER).min(pack.committed);
        let from = first / disk::BLOCK * disk::BLOCK;
        let all = pack.committed_bytes(look, from, pack.committed - from).map_err(unread)?;
        let get = |at: u64, len: u64| {
            let start = usize::try_from(at.checked_sub(from)?).ok()?;
            all.get(start..start.checked_add(usize::try_from(len).ok()?)?)
        };
        let mut at = pack.indexed.max(PACK_HEADER);
        while at < pack.committed {
            if get(at, 4).is_none_or(|start| start == [0; 4]) {
                let next = at.next_multiple_of(disk::BLOCK);
                if next == at || get(at, next - at).is_none_or(|it| it.iter().any(|&it| it != 0)) {
                    // alloc: the error that stops the opening
                    return Err(fail(format!(
                        "at {at} is neither a record nor the zeros that end a commit"
                    )));
                }
                at = next;
                continue;
            }
            let header = RecordHeader::decode(&all[(at - from) as usize..])
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
        // Every block the scan read, against the pack's checks, as the pool checked each as
        // it loaded it: a record's header is checked by its own CRC above, and this is what
        // says its payload is the bytes committed. `all` starts at block `from / BLOCK`.
        let skipped = from / disk::BLOCK;
        let scanned = 0..pack.committed / disk::BLOCK - skipped;
        let checks = pack.checks.get(skipped as usize..).unwrap_or_default();
        if let Some(block) = checks::first_bad(checks, all, scanned) {
            let block = block + skipped;
            // alloc: the error that stops the opening
            return Err(fail(format!(
                "its block {block} (bytes {}..{}) does not match its check in {}",
                block * disk::BLOCK,
                (block + 1) * disk::BLOCK,
                disk::checks_file(number)
            )));
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
            .bytes(look, at, RECORD_HEADER)
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
                        .bytes(look, at + RECORD_HEADER, header.stored)
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
