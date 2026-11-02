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
mod peers;
mod read;
pub mod remote;
mod segment;
mod sync;
mod table;
mod words;
#[cfg(all(test, blaze_check))]
mod model;
#[cfg(all(test, not(blaze_check)))]
mod tests;

#[cfg(all(test, not(blaze_check)))]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

use std::{collections::VecDeque, fmt, sync::Arc};

use blaze_core::{Completion, Done, Effect, FileName, Mapping, Tag};
use blaze_format::{SchemaHash, tiers::BufferColumns};
use blaze_hash::{ByHash, HashMap, HashSet};
use blaze_ids::{ContentId, Hasher, radix};
use blaze_reclaim::{Grace, Limbo};
pub use blaze_reclaim::{Guard, enter};
use serde::{Deserialize, Serialize};

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
use crate::read::{ExternalView, Index, Look, PackView, Place, Seg, Shared, Spot, TierView, View};
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

/// A value shorter than this stays as it is: compressing it saves less than its index slot costs.
const DEMOTE_FROM: u64 = 4 << 10;

/// One [`Store::settle`] moves or copies at most this many bytes, so it stays a short step.
#[cfg(not(test))]
const SETTLE_MOST: u64 = 256 << 20;
#[cfg(test)]
const SETTLE_MOST: u64 = 1 << 20;

/// Outputs from this long on are kept as content-defined chunks.
const CHUNK_FROM: usize = 256 << 10;

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
type Encodings = ByHash<SchemaHash, Arc<Columns>>;

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
    next_batch: u64,
    commit: Option<Commit>,
    /// A flush asked for while a commit was in flight.
    flush_wanted: bool,
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
    fetching: ByHash<Key, Tag>,
    fetched: Vec<(Key, Fetched)>,
    placed: Vec<Placed>,
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
    pub fn get_in_place<'g>(&'g self, guard: &'g Guard, key: &Key) -> Result<Option<(Heat, Entry<'g>)>, Corrupt> {
        self.look(guard).get_in_place(key)
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
    ops: HashMap<Tag, Op>,
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

enum Op {
    /// The store's directory checked to be the user's alone, before anything in it is read.
    Private,
    Head { tier: usize },
    Map { tier: usize, file: FileName, what: Mapped },
    Truncate { pack: usize },
    WritePack { pack: usize },
    Sync { pack: usize },
    WriteExternal { content: ContentId },
    WriteIndex,
    WriteHead,
    /// A pack whose live records were copied on, removed once the head no longer names it.
    RemovePack { pack: usize },
    /// `to` removed so `from` can be cloned there.
    Clear { asked: Tag, from: FileName, to: FileName },
    Place { asked: Tag, to: FileName },
    Fetch { key: Key },
    /// The shared directory listed, for the other writers' stores in it.
    Peers,
    /// Its own empty head written, so the other writers find a store in its directory.
    Announce,
}

#[derive(Clone, Copy, Debug)]
enum Mapped {
    /// `covers` is `None` for the table the store opened with, else the last batch it holds.
    Index { covers: Option<u64> },
    /// `usable` is how much of the file is the store's as of the commit that asked; `batch`,
    /// the batch of moves waiting for it.
    Pack { pack: usize, usable: u64, batch: Option<u64> },
    External { content: ContentId, batch: Option<u64> },
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
    externals: ByHash<ContentId, External>,
    /// By number, which a compressed record names.
    dictionaries: Vec<Dictionary>,
    /// Each unit's crate version by its number less one, and the number of each.
    units: Vec<ContentId>,
    unit_of: ByHash<ContentId, Unit>,
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
            externals: ByHash::default(),
            // alloc: empty, allocates nothing
            dictionaries: Vec::new(),
            // alloc: empty, allocates nothing
            units: Vec::new(),
            unit_of: ByHash::default(),
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
                .map(|it| PackView { file: it.file.clone(), mapping: it.mapping.clone(), usable: it.usable })
                // alloc: the view's packs, exactly sized; once per view
                .collect(),
            externals: self
                .externals
                .iter()
                // clone: the published view owns its names and mappings; once per external a view
                .map(|(content, it)| (*content, ExternalView { file: it.file.clone(), mapping: it.mapping.clone() }))
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
    samples: Vec<Vec<u8>>,
    bytes: u64,
    /// No training before this many samples: doubled after one fails.
    after: usize,
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
    (magic == blaze_format::MAGIC).then(|| SchemaHash(ContentId(schema.try_into().expect("32 bytes"))))
}

/// `encoded` behind the schema of the encodings that made it: a columns record's payload.
fn columns_payload(columns: &Columns, encoded: Vec<u8>) -> Vec<u8> {
    let mut payload = Vec::with_capacity(SCHEMA_PREFIX + encoded.len());
    payload.extend_from_slice(&columns.schema().0.0);
    payload.extend_from_slice(&encoded);
    payload
}

/// The cold columns payload of `hot`, decoded back and compared.
fn cold_columns(columns: &Columns, hot: &[u8]) -> Result<Vec<u8>, String> {
    let payload = columns_payload(columns, columns.cold(hot)?);
    if columns.hot_from_cold(&payload[SCHEMA_PREFIX..])? != hot {
        // alloc: the message of an encoding that does not decode back
        return Err(format!("the cold columns of schema {:x} do not decode to the value", columns.schema().0));
    }
    Ok(payload)
}

/// The next colder columns of `hot`, kept at `loc`: warm from hot (or cold, where warm saves
/// less than an eighth), cold from warm; `None` when that saves less than an eighth too.
fn colder_columns(columns: &Columns, loc: Loc, hot: &[u8]) -> Result<Option<(Form, Vec<u8>)>, String> {
    let saves = |payload: &[u8]| payload.len() as u64 + loc.stored / 8 <= loc.stored;
    if loc.form != Form::Warm {
        let payload = columns_payload(columns, columns.warm(hot)?);
        if columns.hot_from_warm(&payload[SCHEMA_PREFIX..])? != hot {
            // alloc: the message of an encoding that does not decode back
            return Err(format!("the warm columns of schema {:x} do not decode to the value", columns.schema().0));
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
        Store {
            tags: Tags { range: blaze_core::Tags::new(config.tags), ops: HashMap::default() },
            config,
            tiers,
            broken: None,
            failures: 0,
            shared,
            own,
            drains,
            draining: Batch::default(),
            pending: VecDeque::new(),
            next_batch: 0,
            commit: None,
            flush_wanted: false,
            index_mapping: false,
            flip: None,
            migrating: None,
            limbo: Limbo::new(),
            training: Default::default(),
            // alloc: empty, allocates nothing
            problems: Vec::new(),
            fetching: ByHash::default(),
            // alloc: empty, allocates nothing
            fetched: Vec::new(),
            // alloc: empty, allocates nothing
            placed: Vec::new(),
            new_keys: None,
            epoch: 0,
            periods: 0,
            columns,
            peers,
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
        self.broken.is_none() && self.peers_ready().unwrap_or_else(|| self.tiers.iter().all(|it| it.opened))
    }

    /// Why it stopped, if it has.
    pub fn broken(&self) -> Option<&Broken> {
        self.broken.as_ref()
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
    pub fn put(&mut self, key: Key, kind: Kind, bytes: Vec<u8>) -> Result<Ticket, Mismatch> {
        self.put_in(Unit::SHARED, key, kind, bytes)
    }

    /// Keeps `bytes` under `key` in the packs of `unit`; readable at once, durable once
    /// [`Store::complete`] says its ticket is. A second put of a key keeps the first value
    /// wherever it is, and a put whose bytes differ from the value kept is an error the caller
    /// sees (a key that misses an input).
    pub fn put_in(&mut self, unit: Unit, key: Key, kind: Kind, bytes: Vec<u8>) -> Result<Ticket, Mismatch> {
        let content = content_of(&bytes);
        self.put_hashed(unit, key, kind, content, &bytes)
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
        assert!(unit.0 as usize <= self.tiers[0].units.len(), "{unit:?} is not one this store gave");
        assert!(self.tiers[0].opened, "a put into a store that is not open");
        self.tidy();
        let look = self.look();
        let sample = kind.cold()
            && bytes.len() < cold::LONG_FROM
            && look.kept(&key).is_none()
            && look.dictionary_of(kind).is_none()
            && look.columns_of(bytes).is_none();
        if sample {
            self.sample(kind, bytes);
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
                for &(_, loc, _) in group.iter().filter(|(_, loc, _)| loc.form == Form::Raw) {
                    let pack = &self.tiers[0].packs[loc.pack as usize];
                    let payload = loc.offset.checked_add(RECORD_HEADER);
                    // alloc: a settle (idle), until a dictionary is trained; `sample` keeps a copy and may append to the pack this borrows
                    let Some(bytes) = payload.and_then(|at| pack.bytes(at, loc.raw)).map(<[u8]>::to_vec) else {
                        continue;
                    };
                    self.sample(kind, &bytes);
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
                    self.cannot_compress(kind, why);
                    return settled;
                }
            };
            for &(key, loc, row) in group {
                if spent > 0 && spent.saturating_add(loc.raw) > SETTLE_MOST {
                    settled.more = true;
                    break 'kinds;
                }
                let Some((content, bytes)) = self.movable(key, loc) else { continue };
                spent += loc.raw;
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
                let stored = if long { cold::compress_long(&bytes) } else { batch.compress(&bytes) };
                let stored = match stored {
                    Ok(stored) => stored,
                    Err(why) => {
                        self.cannot_compress(kind, why);
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
        for (key, found) in rows {
            let Place::Pack(loc) = found.place else { continue };
            let Some(pack) = own.packs.get(loc.pack as usize) else { continue };
            if pack.removed || pack.retiring || loc.raw < DEMOTE_FROM {
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
                Form::Zstd | Form::Warm | Form::ColdColumns if read && before => {
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
                self.problems.push(format!("{:x} is not moved: its record is not the one its index names", key.0));
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
    /// value was durable where it was, so it is as durable now.
    fn rehome(&mut self, unit: Unit, header: RecordHeader, payload: &[u8]) {
        let (seg, span) = self.own.write(unit, &header, payload, 0);
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
    /// kind appends to, and holding no dictionary.
    fn retirable(&self) -> Vec<usize> {
        let own = &self.tiers[0];
        (0..own.packs.len())
            .filter(|&at| {
                let pack = &own.packs[at];
                !pack.removed
                    && !pack.retiring
                    && pack.end == pack.committed
                    && pack.usable == pack.committed
                    && pack.committed > PACK_HEADER
                    && pack.dead * 2 >= pack.committed - PACK_HEADER
                    && own.packs.iter().rposition(|it| it.unit == pack.unit && !it.removed) != Some(at)
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
    /// `clonefile`); an output split in chunks is written whole.
    ///
    /// How it went comes through [`Store::placed`]. What a value's form decides is how it is
    /// placed, never a try of one way then another: a file of its own is cloned (and fails
    /// where the file system cannot share blocks), anything else written.
    pub fn place(&mut self, key: &Key, to: FileName, effects: &mut Vec<Effect>) -> Option<Tag> {
        assert!(
            !self.tiers.iter().any(|tier| to.is_under(&tier.dir)),
            "{to} is in the store: values are placed outside it"
        );
        self.tidy();
        enum Plan {
            Clone(FileName),
            Write(Vec<u8>),
            Refused(String),
        }
        let look = self.look();
        let found = look.find(key)?;
        let plan = match found.and_then(|found| Ok((found, look.read(found, key)?))) {
            // alloc: the error of a corrupt value
            Err(corrupt) => Plan::Refused(corrupt.to_string()),
            Ok((found, entry)) => match self.tiers[found.tier].externals.get(&entry.content) {
                Some(external)
                    if matches!(found.place, Place::Pack(loc) if loc.form == Form::External)
                        && external.committed =>
                {
                    // clone: the clone effect owns the name it clones from; once per place
                    Plan::Clone(external.file.clone())
                }
                _ => {
                    let mut bytes = Vec::with_capacity(entry.bytes.len());
                    copy::append(&mut bytes, entry.bytes);
                    Plan::Write(bytes)
                }
            },
        };
        let tag = self.tags.fresh();
        match plan {
            Plan::Clone(from) => {
                // clone: the effect and the op each own the name; once per place
                effects.push(Effect::Remove { file: to.clone(), tag });
                self.tags.ops.insert(tag, Op::Clear { asked: tag, from, to });
            }
            Plan::Write(bytes) => {
                // clone: the effect and the op each own the name; once per place
                effects.push(Effect::Write { file: to.clone(), bytes, tag });
                self.tags.ops.insert(tag, Op::Place { asked: tag, to });
            }
            Plan::Refused(why) => self.placed.push(Placed { tag, to, result: Err(why) }),
        }
        Some(tag)
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
    pub fn fetch(
        &mut self,
        key: &Key,
        remote: &mut impl Remote,
        effects: &mut Vec<Effect>,
    ) -> Option<Tag> {
        if self.has(key) || self.fetching.contains_key(key) {
            return None;
        }
        let tag = self.tags.fresh();
        match remote.fetch(key, tag, effects) {
            Asked::Sent => {
                self.tags.ops.insert(tag, Op::Fetch { key: *key });
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

    /// Takes a completion of its own: a mapping, an append, a sync. Says which puts are now
    /// durable.
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
                }
                Done::Private(Some((file, access))) => {
                    // alloc: the error that breaks the store
                    self.fail(file, format!("{access}, and the store and all in it must be the user's alone"))
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
                        true => self.peer_head(tier, bytes, effects),
                        false => self.read_head(tier, bytes, effects),
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
            Op::Clear { asked, from, to } => match done {
                Done::Removed => {
                    // clone: the effect and the op each own the name; once per place
                    let tag = self.tags.ask(Op::Place { asked, to: to.clone() });
                    effects.push(Effect::Clone { from, to, tag });
                }
                done => self.placed.push(Placed { tag: asked, to, result: Err(describe(&done)) }),
            },
            Op::Place { asked, to } => {
                let result = match done {
                    Done::Cloned | Done::Written => Ok(()),
                    done => Err(describe(&done)),
                };
                self.placed.push(Placed { tag: asked, to, result });
            }
            Op::Fetch { key } => {
                self.fetching.remove(&key);
                let fetched = self.take_fetched(key, done);
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
            Op::Peers => self.peers_listed(done, effects),
            Op::Announce => self.announced(done, effects),
            op => return self.step(op, done, effects),
        }
        self.peers_ask(effects);
        // alloc: empty, allocates nothing
        Vec::new()
    }

    /// Takes a mapping it asked for. Separate from [`Store::complete`] for drivers that map
    /// by themselves at start.
    pub fn mapped(&mut self, file: FileName, mapping: Mapping) {
        self.tidy();
        let asked = self
            .tags
            .ops
            .iter()
            .filter_map(|(tag, op)| match op {
                Op::Map { file: it, what, .. } if *it == file => Some((what.usable(), *tag)),
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
    fn drain_record(&mut self, number: usize, at_chunk: ChunkRef, chunk: &Chunk, at: u64, batch: &mut Batch) -> u64 {
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
        assert!(unit as usize <= self.tiers[0].units.len(), "a put into unit {unit}, which this store did not give");
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
            self.tiers[0].packs[loc.pack as usize].dead += header.span();
        } else {
            // The store's own puts were sampled as they were put.
            if number != 0 && header.kind.cold() && header.form == Form::Raw {
                self.sample(header.kind, payload);
            }
            let from = Seg {
                chunk: at_chunk,
                segment: number as u32,
                offset: at + FRAME,
                kind: header.kind,
                form: header.form,
                stored: header.stored,
                raw: header.raw,
            };
            batch.moves.push(Move { key: header.key, from, to: loc });
        }
        FRAME + header.span()
    }

    /// Makes the moves of every batch whose mappings have all come, oldest first: publishes a
    /// view with the mappings, points each key at its pack, and frees the old spots and the
    /// chunks drained to their end at task boundaries.
    fn finish_batches(&mut self) {
        if self.pending.front().is_some_and(|it| it.maps == 0) {
            self.publish();
            // alloc: one list a commit of the chunks drained, held in the limbo past their grace
            let mut chunks = Vec::new();
            while self.pending.front().is_some_and(|it| it.maps == 0) {
                let batch = self.pending.pop_front().expect("a batch at the front");
                let held = Held::writer(&*self.shared);
                let own = &self.tiers[0];
                for it in &batch.moves {
                    for table in std::iter::once(&own.delta).chain(&own.prev) {
                        // The spot swapped out stays readable in its table until a collect
                        // after its grace, which begins after this swap and before the chunks'.
                        if let Some((_, spot)) = table.get(held, &it.key)
                            && spot.place == Place::Segment(it.from)
                        {
                            _ = table.replace(held, spot.at(Place::Pack(it.to)));
                        }
                    }
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
        for (_, spot) in own.delta.iter(held) {
            let indexed = matches!(
                (spot.place, table.get(&spot.key)),
                (Place::Pack(loc), Some((_, Ok(at)))) if at == loc
            );
            if !indexed {
                _ = fresh.insert(Held::writer(&fresh), spot.at(spot.place));
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

    /// Folds the delta an index swap left into the new one once no writer can put into it any
    /// more: what a writer put there after the swap is copied over.
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
        for (_, spot) in prev.iter(held) {
            match own.delta.get(held, &spot.key) {
                // alloc: a problem reported
                Some((_, kept)) if kept.content != spot.content => self.problems.push(format!(
                    "{:x} was put twice across an index swap, as {:x} and {:x}: the first is kept",
                    spot.key.0, kept.content, spot.content
                )),
                Some(_) => {}
                None => {
                    let indexed = matches!(
                        (spot.place, table.as_ref().and_then(|it| it.get(&spot.key))),
                        (Place::Pack(loc), Some((_, Ok(at)))) if at == loc
                    );
                    if !indexed {
                        _ = own.delta.insert(held, spot.at(spot.place));
                    }
                }
            }
        }
        self.publish();
    }

    /// Publishes what readers see, and frees the view it replaces at task boundaries.
    fn publish(&mut self) {
        // clone: the view readers share, made once a publish (see `Tier::view`), with an `Arc` of the encodings
        let view = View { tiers: self.tiers.iter().map(Tier::view).collect(), columns: self.columns.clone() };
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
    fn cannot_compress(&mut self, kind: Kind, why: String) {
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

    fn sample(&mut self, kind: Kind, bytes: &[u8]) {
        let training = &mut self.training[kind.code() as usize];
        if training.samples.len() < cold::SAMPLES_MOST && training.bytes < cold::SAMPLE_BYTES_MOST {
            // alloc: a training sample, at most `SAMPLES_MOST` a kind and only before its dictionary is trained
            training.samples.push(bytes.to_vec());
            training.bytes += bytes.len() as u64;
        }
        if training.samples.len() >= cold::TRAIN_VALUES.max(training.after)
            && training.bytes >= cold::TRAIN_BYTES
        {
            self.train(kind);
        }
    }

    fn train(&mut self, kind: Kind) {
        let trained = {
            let samples: Vec<&[u8]> =
                // alloc: once per dictionary trained, the samples as slices for zstd
                self.training[kind.code() as usize].samples.iter().map(Vec::as_slice).collect();
            cold::train(&samples).and_then(|dictionary| {
                cold::Prepared::new(dictionary)
                    // alloc: the error's message
                    .map_err(|why| format!("the dictionary trained on {} values {why}", samples.len()))
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
            Err(why) => {
                training.after = training.samples.len() * 2;
                // alloc: a problem reported
                self.problems.push(format!("the {} values are kept raw: {why}", kind.name()));
            }
        }
    }

    fn start_commit(&mut self, effects: &mut Vec<Effect>) {
        debug_assert!(self.commit.is_none());
        self.drain();
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
        let compact = !self.index_mapping && self.migrating.is_none() && own.delta.len() >= INDEX_AFTER;
        if packs.is_empty() && externals.is_empty() && !compact {
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
                    .push(format!("a slot of {} left out of the next index: {what}", own.index_file()));
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
    /// segment is not held, nor does it shadow the record the table held before. A slot that
    /// is not whole is in `bad`.
    fn index_entries(&self, bad: &mut Vec<(Key, &'static str)>) -> Vec<(Key, Loc)> {
        let mut moved: HashMap<Key, (Seg, Loc)> = HashMap::default();
        for it in self.pending.iter().chain([&self.draining]).flat_map(|it| &it.moves) {
            moved.insert(it.key, (it.from, it.to));
        }
        let held = Held::writer(&*self.shared);
        let own = &self.tiers[0];
        let indexed = own.index.as_ref().map_or(0, |it| it.table().count() as usize);
        let mut out = Vec::with_capacity(own.delta.len() + indexed);
        let mut taken = HashSet::default();
        for (_, spot) in own.delta.iter(held) {
            let loc = match spot.place {
                Place::Pack(loc) => Some(loc),
                Place::Segment(seg) => {
                    moved.get(&spot.key).filter(|(from, _)| *from == seg).map(|(_, to)| *to)
                }
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
    }

    fn step(&mut self, op: Op, done: Done, effects: &mut Vec<Effect>) -> Vec<Ticket> {
        // alloc: empty, allocates nothing
        let Some(commit) = self.commit.as_mut() else { return Vec::new() };
        commit.steps -= 1;
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
        match (&op, &done) {
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
                    effects.push(Effect::WriteAt { file: pack.file.clone(), at: pack.committed, bytes, tag });
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
                self.broken.get_or_insert(failed);
            } else {
                self.start_commit(effects);
            }
            // alloc: empty, allocates nothing
            return Vec::new();
        }
        if !commit.head_sent {
            commit.head_sent = true;
            commit.steps += 1;
            let tag = self.tags.ask(Op::WriteHead);
            effects.push(Effect::Write { file: own.file(HEAD_FILE), bytes: commit.head.encode(), tag });
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
            let external = self.tiers[0].externals.get_mut(content).expect("a file the commit wrote");
            external.committed = true;
            // clone: the op and the effect own the name; once per file a commit
            let file = external.file.clone();
            self.ask_map(0, file, Mapped::External { content: *content, batch: Some(batch.id) }, effects);
        }
        for &(at, to) in &commit.packs {
            // clone: the op and the effect own the name; once per pack a commit
            let file = self.tiers[0].packs[at].file.clone();
            self.ask_map(0, file, Mapped::Pack { pack: at, usable: to, batch: Some(batch.id) }, effects);
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
        if std::mem::take(&mut self.flush_wanted) {
            self.start_commit(effects);
        }
        tickets
    }

    // Opening and mapping.

    fn fail(&mut self, file: FileName, what: impl Into<String>) {
        self.broken.get_or_insert(Broken { file, what: what.into() });
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

    fn read_head(&mut self, at: usize, bytes: Vec<u8>, effects: &mut Vec<Effect>) {
        let file = self.tiers[at].file(HEAD_FILE);
        let head = match (bytes.is_empty(), at) {
            (true, 0) => Head::default(),
            (true, at) if self.is_peer(at) => Head::default(),
            (true, _) => return self.fail(file, "it is not there: a layer is a store"),
            (false, _) => match Head::decode(&bytes) {
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
        let externals: ByHash<ContentId, External> = head
            .externals
            .iter()
            .map(|it| {
                (it.content, External {
                    len: it.len,
                    file: tier.file(&disk::external_file(&it.content)),
                    mapping: None,
                    memory: None,
                    committed: true,
                })
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
            maps.push((externals[content].file.clone(), Mapped::External { content: *content, batch: None }));
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
        tier.unit_of = head.units.iter().enumerate().map(|(at, it)| (*it, Unit(at as u32 + 1))).collect();
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
    fn take_fetched(&mut self, key: Key, done: Done) -> Fetched {
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
        let mapped = tier.index.as_ref().ok_or_else(|| (file.clone(), "it was not mapped".to_owned()))?;
        let bytes = mapped.mapping.bytes();
        if bytes.len() as u64 != index.len {
            // alloc: the error that stops the opening
            return Err((file, format!("it is not the {} bytes its head names", index.len)));
        }
        let check = crc::crc32c(bytes);
        if check != index.check {
            // alloc: the error that stops the opening
            return Err((file, format!("its CRC-32C is {check:08x}, not the {:08x} its head names", index.check)));
        }
        let count = mapped.table().count();
        if count != index.count {
            // alloc: the error that stops the opening
            return Err((file, format!("it holds {count} values, not the {} its head names", index.count)));
        }
    }
    // alloc: opening, once per tier, the records past its index table
    let mut found = Vec::new();
    for (number, pack) in tier.packs.iter().enumerate().filter(|(_, it)| !it.removed) {
        // clone: the error that stops the opening
        let fail = |why: String| (pack.file.clone(), why);
        let all = pack.mapping.as_ref().ok_or_else(|| fail("it was not mapped".into()))?.bytes();
        let all = usize::try_from(pack.committed).ok().and_then(|committed| all.get(..committed)).ok_or_else(
            // alloc: the error that stops the opening
            || fail(format!("it is {} bytes, shorter than the {} its head names", all.len(), pack.committed)),
        )?;
        disk::check_pack_header(all, pack.unit).map_err(|why| fail(why.into()))?;
        let mut at = pack.indexed.max(PACK_HEADER);
        while at < pack.committed {
            if all.get(at as usize..at as usize + 4).is_none_or(|start| start == [0; 4]) {
                let next = at.next_multiple_of(disk::BLOCK);
                if next == at || all[at as usize..next as usize].iter().any(|&it| it != 0) {
                    // alloc: the error that stops the opening
                    return Err(fail(format!("at {at} is neither a record nor the zeros that end a commit")));
                }
                at = next;
                continue;
            }
            let header = RecordHeader::decode(&all[at as usize..])
                // alloc: the error that stops the opening
                .map_err(|why| fail(format!("the record at {at}: {why}")))?;
            let end = at.checked_add(header.span()).filter(|&end| end <= pack.committed).ok_or_else(
                // alloc: the error that stops the opening
                || fail(format!("the record at {at} runs past the {} bytes its head names", pack.committed)),
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
                found.push(Spot::new(header.key, header.content, Place::Pack(loc), Ticket::DURABLE));
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
            .and_then(|bytes| RecordHeader::decode(bytes).map_err(|why| format!("at {at} of {}: {why}", pack.file)))
            .and_then(|header| match header.form == Form::Dictionary && header.kind == dictionary.kind {
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
                false => Err(format!("at {at} of {} is not a dictionary of its kind", pack.file)),
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
