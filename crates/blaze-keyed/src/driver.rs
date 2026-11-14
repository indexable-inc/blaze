//! The driver of one request: a sans-IO state machine. Its host performs each [`Effect`] (asks
//! the store, answers facts now over a universe of mapped tables, runs a task, files a record)
//! and gives back the answer as an [`Input`], in any order.
//!
//! Records are read-addressed (docs/v2/plan/design/read-addressed.md): a body or a surface is
//! filed at the address of its key and the answers its work saw, so a record is found by what
//! it read, nothing is refiled under a moved key, and an undone edit finds its old record at its
//! old address. One request, in one [`Snapshot`]:
//!
//! - The certificate, an outside record ([`crate::outside`]): a value at the address of the
//!   request's outside answers is [`Certified`], and nothing below is checked.
//! - Else per unit in dependency order: key it from text (a task); look up its [`Mark`] under its
//!   text key and its view (its direct dependencies' universe digests). A hit settles the unit:
//!   one lookup, and nothing it names is read.
//! - On a miss, with the manifest the last certificate named (`L`): read its root; answer its
//!   dictionary's rows owned by other units now, which gives the changed facts; keep the surface
//!   when its key is `L`'s and its shape read none of them, else find it by address; answer the
//!   unit's own rows when the surface's result moved; walk the postings of every changed fact to
//!   the entries that read it. Those items, and items whose key moved, are examined: each tries
//!   its last shape, then its key's shape trie, answering a shape now and reading the address it
//!   makes; only a miss of every candidate runs the work. Everything else stands as `L` has it.
//! - Facts a body makes (a const's value, a hidden type, what escapes a body, its effects) are
//!   asked of their maker's table, which starts as `L`'s: a reader's shape is answered only once
//!   the maker of each made fact in it settled (a maker `L` names, examined; for a fact no row
//!   names, every other examined item), so an address asked, and a hit redone under audit, see
//!   one made table; at the end every examined reader is checked against the made table, and one
//!   that read another digest is redone (an edge found only on a redo, or a run with no shape
//!   known before it); an item settled more than [`SETTLES_MOST`] times is
//!   [`Failure::MadeCycle`].
//! - A unit files only what moved: the records its runs made (at their addresses), the shape
//!   nodes their walks ended at, the chunks of the files whose entries moved, the manifest root,
//!   and its mark; a unit whose manifest stands files its mark alone. The certificate is filed
//!   after every mark, at the address of what the request read.
//!
//! Floors (rule 67; read-addressed.md, "Floors"), per path, `r` the rows of a unit's dictionary,
//! `c` the changed facts, `p` their postings, `e` the items examined:
//! - **Mark lookup**: one hash of the view and one read: the floor of settling a unit.
//! - **Dictionary merge**: one answer per dependency-owned row of `L` (`r` lookups in mapped
//!   tables), one per own row when the surface's result moved. The floor of finding what changed
//!   is `c`; this is `r` (the rows are not indexed by the dependency fact that moved), the one
//!   path above the floor, bounded by the unit's own reads.
//! - **Posting walk**: one read per chunk holding a reader of a changed fact, one binary search
//!   per changed fact in it, then the `p` readers: the floor.
//! - **Address lookup**: per examined item, one answer per fact of a candidate shape and one read
//!   of the address: the floor of proving a record by what it read. A walk is bounded by
//!   [`WALK_MOST`] nodes; past it the work runs ([`Checks::walks_capped`]).
//! - **Filing**: the moved chunks and the root, rebuilt in one pass over their entries, and one
//!   mark: `O(e + moved files' entries + r)`.
//!
//! Code (instances, programs) is blaze-program's, on the same vocabulary: `check` makes none.

#![blaze::sans_io]

use core::alloc::Allocator;
use std::{alloc::Global, collections::VecDeque, marker::PhantomData};

use blaze_format::Aligned;
use blaze_ids::ContentId;

use crate::{
    Plant,
    exec::{Executor, LOCAL_TASKS},
    front::FileItems,
    kind::{Body, Certificate, Key, Kind, Layout, Mark, MarkInputs, Namespace, Shape, Surface, Tag, UnitChunk, UnitManifest},
    memo::{Arena, Memo},
    outside::{self, Filer, Found, Lookup, OutsideError, Want},
    planted,
    pool::Ticket,
    proof::{self, AnyHolds, Certified, Checks, Holds, Snapshot},
    record::{self, ChunkParts, ChunkRoot, Entry, Fact, FileChunk, ManifestParts, ManifestRoot, Owner, RecordError, Route, StableId, Traced},
    store::Put,
};

/// The most shape nodes one item's lookup walks before it runs the work instead.
pub const WALK_MOST: u32 = 64;

/// The most times one item is settled in a request: past it its made facts and their readers
/// do not agree, [`Failure::MadeCycle`].
pub const SETTLES_MOST: u32 = 3;

/// The item a unit's surface is in [`Driver::proofs`].
pub const SURFACE_ITEM: StableId = StableId(ContentId([0; 32]));

/// The unit of an effect about the request itself (its certificate).
pub const NO_UNIT: u32 = u32::MAX;

/// The slot of a unit's surface; its bodies are `1..`.
const SURFACE: u32 = 0;

const ZERO: ContentId = ContentId([0; 32]);

/// One unit of the request: its dependencies, by index below its own, the manifest root the
/// last certified build filed for it (the certificate's list), its owner and its namespace.
#[derive(Clone, Copy, Debug)]
pub struct UnitPlan<'p> {
    pub deps: &'p [u32],
    pub last: Option<Key<UnitManifest>>,
    /// The namespace the unit's records are filed in: [`crate::kind::namespace_of`] of its
    /// package, computed once per package by the host.
    pub namespace: Namespace,
    /// The first eight bytes of the unit's crate key: what the ids of the facts it owns begin
    /// with ([`record::Owner`]).
    pub owner: Owner,
}

#[derive(Clone, Copy, Debug)]
pub struct Plan<'p> {
    /// The request's base: the key its certificate's shapes are filed under.
    pub certificate: Key<Certificate>,
    /// The namespace the certificate is filed in.
    pub namespace: Namespace,
    pub units: &'p [UnitPlan<'p>],
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PlanError {
    DepOrder { unit: u32, dep: u32 },
    /// Unit `unit`'s owner is reserved ([`Owner::ASKER`], [`Owner::OUTSIDE`]) or another unit's.
    Owner { unit: u32 },
}

/// A task the host runs on the pool.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Task {
    /// Key unit `unit`: its text key, its surface's and its bodies' keys, from text.
    Key { unit: u32 },
    Surface { unit: u32, key: Key<Surface> },
    Body { unit: u32, item: StableId, key: Key<Body> },
}

/// What a task gives back; bytes borrowed from the worker's frame, 16-aligned.
#[derive(Clone, Copy, Debug)]
pub enum Done<'a> {
    /// [`crate::front::Keyed`]'s fields.
    Keyed {
        manifest: Key<UnitManifest>,
        surface: Key<Surface>,
        files: &'a [FileItems],
        items: &'a [(StableId, Key<Body>)],
    },
    Wrote {
        record: &'a [u8],
    },
}

/// A task that did not finish.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TaskFailed {
    /// The task panicked: only it fails (rule 20).
    Panicked { what: &'static str },
    /// The task refused its input, naming what.
    Refused { what: &'static str },
}

/// A store failure, by name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StoreFault {
    pub what: &'static str,
}

/// One unit's tables as an effect names them: its surface record's address and its manifest
/// root's address (`None` for the asking unit's own, whose made facts come with the effect, and
/// for its surface until it is settled). The host reads each in place.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TableRef {
    pub unit: u32,
    pub owner: Owner,
    pub surface: Option<Key<Surface>>,
    pub manifest: Option<Key<UnitManifest>>,
}

/// The answer to an effect. Record bytes are 16-aligned.
#[derive(Clone, Copy, Debug)]
pub enum Input<'a> {
    /// To [`Effect::Get`].
    Got { ticket: Ticket, record: Option<&'a [u8]> },
    /// To [`Effect::Answer`] and [`Effect::Outside`]: the answer now of each ask, in order.
    Answers { ticket: Ticket, now: &'a [ContentId] },
    Ran { ticket: Ticket, outcome: Result<Done<'a>, TaskFailed> },
    Put { ticket: Ticket, put: Result<Put, StoreFault> },
    /// To [`Effect::Reads`]: the request's outside reads, as a shape (the asks' bytes), their
    /// answers when the request began, and their answers now.
    Reads { ticket: Ticket, shape: &'a [u8], then: &'a [ContentId], now: &'a [ContentId] },
}

impl Input<'_> {
    fn ticket(&self) -> Ticket {
        match *self {
            Input::Got { ticket, .. }
            | Input::Answers { ticket, .. }
            | Input::Ran { ticket, .. }
            | Input::Put { ticket, .. }
            | Input::Reads { ticket, .. } => ticket,
        }
    }
}

/// What the host is to do.
#[derive(Clone, Copy, Debug)]
pub enum Effect<'d> {
    /// Read the record of `tag` filed at `key`, for unit `unit` ([`NO_UNIT`] for the
    /// certificate): by pread for a flat one, mapped for a plain one.
    Get { ticket: Ticket, unit: u32, tag: Tag, key: ContentId, layout: Layout },
    /// Answer each ask of `shape`, a shape filed under the outside record `key` of kind `tag`,
    /// as it is now: [`Input::Answers`].
    Outside { ticket: Ticket, tag: Tag, key: ContentId, shape: &'d [u8] },
    /// Answer each fact id of `asks` now, as unit `unit` sees it: over `universe` (sorted by
    /// owner) and, for the unit's own made facts, `made` (sorted by id), by
    /// [`crate::front::Universe::answer`]: [`Input::Answers`].
    Answer { ticket: Ticket, unit: u32, asks: &'d [ContentId], universe: &'d [TableRef], made: &'d [Fact] },
    /// Run `task` over `universe` and `made` (as [`Effect::Answer`]'s): on the local pool, or,
    /// when `remote`, through the executor's [`RemoteService`](crate::exec::RemoteService).
    Run { ticket: Ticket, task: Task, remote: bool, universe: &'d [TableRef], made: &'d [Fact] },
    /// File `bytes` at `tag`, `key` in `namespace` ([`crate::store::Store::put`]).
    Put { ticket: Ticket, tag: Tag, key: ContentId, namespace: Namespace, bytes: &'d [u8] },
    /// Give the request's outside reads: [`Input::Reads`].
    Reads { ticket: Ticket },
}

#[derive(Clone, Copy, Debug)]
pub enum Step<'d> {
    Do(Effect<'d>),
    /// Nothing to do until an answer comes: what is waited for.
    Wait(&'static str),
    Exit(&'d Outcome),
}

/// What a request did. Equal under every memo but [`crate::memo::Audit`] (the refinement
/// oracle compares it).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Ran {
    pub keyed: u32,
    pub surfaces: u32,
    pub bodies: u32,
    /// Records found at their address (none ran).
    pub found: u32,
    /// Units settled by their mark.
    pub marked: u32,
    /// Units whose manifest stood: only a mark filed.
    pub stood: u32,
    /// Manifest roots filed.
    pub manifests: u32,
    /// Chunks filed (a file's entries moved).
    pub chunks: u32,
    /// Shape nodes read past a key's root: lookups a value-dependent shape sent down the trie.
    pub walked: u32,
    /// Items settled again because a fact their unit's bodies make moved after they read it.
    pub redone: u32,
    pub checks: Checks,
}

/// Asks, which differ by memo.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Asks {
    pub gets: u32,
    pub memo_hits: u32,
}

/// The first thing that differs between a record found and the one its work made again.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum First {
    /// A fact of the shape (asked by one and not the other, or seen with another digest).
    Read(ContentId),
    /// A fact the work makes.
    Made(ContentId),
    /// The payload, or a record that is not traced (a mark, the certificate's value).
    Payload,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Failure {
    Task { tag: Tag, unit: u32, failed: TaskFailed },
    /// Other bytes are filed at a key or address this request made: a key that misses an input.
    TwoValues { tag: Tag, key: ContentId },
    Store(StoreFault),
    /// A record that is not what its key says.
    Record { tag: Tag, key: ContentId, why: RecordError },
    /// The certificate's lookup or filing.
    Outside(OutsideError),
    /// An outside read of the request moved while it ran (ask `at` of its shape): nothing is
    /// certified; the next request builds against what is there then.
    Raced { at: u32 },
    /// The facts unit `unit`'s bodies make and the bodies that read them did not agree after
    /// [`SETTLES_MOST`] settles of `items`: a cycle through made facts (a const reading a const
    /// that reads it), which the front end must refuse by name.
    MadeCycle { unit: u32, items: Vec<StableId, Global> },
    /// Unit `unit`'s records saw two digests of fact `id`: one fact answered at two moments.
    TwoDigests { unit: u32, id: ContentId },
    /// Under [`crate::memo::Audit`]: the record of `tag` found at `address`, made again, is other
    /// bytes, first at `first`: a shape that leaves out a read, a key that leaves out an input,
    /// or a task that is not deterministic.
    Audit { tag: Tag, address: ContentId, first: First },
    /// The host broke the protocol: an answer to no effect, or of the wrong form.
    Misused(&'static str),
}

/// How the certificate was filed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Filed {
    /// At the address of the request's answers.
    Certificate,
    /// Every one of the [`outside::MAX_SHAPES`] shape slots of the request's base holds another
    /// shape: nothing certifies this request, and the next one settles every unit through its
    /// mark (one lookup each) instead of one certificate lookup. Costs a rebuild of nothing.
    Full,
    /// Under [`crate::memo::Audit`], the certificate held and the request built again to the
    /// same value: nothing filed.
    Audited,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// The certificate held: nothing ran, and nothing below it was checked.
    Certified { checks: Checks },
    Built { ran: Ran, filed: Filed },
    Failed { ran: Ran, why: Failure },
    /// Nothing ready and nothing asked: a bug of the driver, said by name.
    Stalled { ran: Ran },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Work {
    CertShape,
    CertAnswers,
    CertValue,
    Reads,
    FileRead,
    FilePut,
    Key(u32),
    GetMark(u32),
    GetLast(u32),
    GetChunk(u32, u32),
    /// The unit's dictionary rows being answered.
    Rows(u32),
    Probe(u32, u32),
    GetAddress(u32, u32),
    GetNode(u32, u32),
    Run(u32, u32),
    PutRecord(u32, u32),
    PutNode(u32, u32),
    PutChunk(u32, u32),
    PutManifest(u32),
    PutMark(u32),
    /// The planted check after a mark: a read of the unit's surface.
    UnderMark(u32),
}

impl Work {
    fn unit(self) -> Option<u32> {
        match self {
            Work::Key(u)
            | Work::GetMark(u)
            | Work::GetLast(u)
            | Work::GetChunk(u, _)
            | Work::Rows(u)
            | Work::Probe(u, _)
            | Work::GetAddress(u, _)
            | Work::GetNode(u, _)
            | Work::Run(u, _)
            | Work::PutRecord(u, _)
            | Work::PutNode(u, _)
            | Work::PutChunk(u, _)
            | Work::PutManifest(u)
            | Work::PutMark(u)
            | Work::UnderMark(u) => Some(u),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum UnitState {
    Waiting,
    Keying,
    Marking,
    Last,
    /// The dependency rows of `L`'s dictionary being answered.
    DepRows,
    Surface,
    OwnRows,
    Chunks,
    Items,
    Writing,
    Complete,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SlotState {
    /// Stands as the last manifest has it; not examined.
    Kept,
    Waiting,
    Asking,
    Getting,
    Walking,
    Running,
    Putting,
    Settled,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Probing {
    Old,
    Node,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Filing {
    Chunks,
    Root,
    Mark,
}

/// What the last manifest said of an item: where its record was and the shape it asked.
struct OldEntry<A: Allocator> {
    address: ContentId,
    shape: Vec<ContentId, A>,
}

struct Slot<'s, A: Allocator> {
    item: StableId,
    key: ContentId,
    file: u32,
    old: Option<OldEntry<A>>,
    state: SlotState,
    examined: bool,
    /// Started without waiting for its known makers: the unit stalled on them.
    released: bool,
    /// Waiting with its candidate shape in `probe`, unanswered: a fact of it is made by an item
    /// of the unit not settled yet ([`Driver::waits_on_maker`]); resumed at its probe.
    parked: bool,
    settles: u32,
    probing: Probing,
    probe: Vec<ContentId, A>,
    /// The address asked last.
    asked: ContentId,
    depth: u32,
    /// Where a run's shape is filed: the node the walk found empty.
    node: Option<Key<Shape>>,
    address: ContentId,
    result: ContentId,
    reads: Vec<Fact, A>,
    made: Vec<Fact, A>,
    bytes: Option<Aligned>,
    node_bytes: Option<Aligned>,
    /// Under audit: the bytes found, until the work made again is compared with them.
    audit: Option<Aligned>,
    holds: Option<AnyHolds<'s>>,
}

/// The last manifest's root, copied out once.
struct Last<A: Allocator> {
    address: Key<UnitManifest>,
    text: ContentId,
    surface_key: ContentId,
    surface: ContentId,
    surface_result: ContentId,
    files: Vec<FileChunk, A>,
    dictionary: Vec<Fact, A>,
    readers: Vec<u32, A>,
    reader_at: Vec<u32, A>,
    surface_shape: Vec<u32, A>,
    made: Vec<Fact, A>,
    makers: Vec<ContentId, A>,
}

impl<A: Allocator> Last<A> {
    fn readers(&self, row: usize) -> &[u32] {
        &self.readers[self.reader_at[row] as usize..self.reader_at[row + 1] as usize]
    }

    fn row(&self, id: &ContentId) -> Option<usize> {
        self.dictionary.binary_search_by(|fact| fact.id.cmp(id)).ok()
    }
}

/// An old chunk, copied out once.
struct OldChunk<A: Allocator> {
    entries: Vec<Entry, A>,
    shapes: Vec<u32, A>,
    dictionary: Vec<ContentId, A>,
    postings: Vec<u32, A>,
    posting_at: Vec<u32, A>,
}

impl<A: Allocator + Clone> OldChunk<A> {
    fn entry(&self, item: &StableId) -> Option<usize> {
        self.entries.binary_search_by(|entry| entry.item.cmp(&item.0)).ok()
    }

    fn shape_of(&self, at: usize, alloc: A) -> Vec<ContentId, A> {
        let entry = &self.entries[at];
        let ords = &self.shapes[entry.shape_at as usize..(entry.shape_at + entry.shape_len) as usize];
        let mut ids = Vec::with_capacity_in(ords.len(), alloc);
        ids.extend(ords.iter().map(|&ord| self.dictionary[ord as usize]));
        ids
    }

    fn readers_of(&self, id: &ContentId) -> &[u32] {
        match self.dictionary.binary_search(id) {
            Ok(k) => &self.postings[self.posting_at[k] as usize..self.posting_at[k + 1] as usize],
            Err(_) => &[],
        }
    }
}

enum ChunkState<A: Allocator> {
    Unread,
    Wanted,
    Asked,
    Held(OldChunk<A>),
    Gone,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RowAsk {
    Deps,
    Own,
}

struct Unit<'s, A: Allocator> {
    state: UnitState,
    waiting: u32,
    deps: Vec<u32, A>,
    owner: Owner,
    namespace: Namespace,
    plan_last: Option<Key<UnitManifest>>,
    text: Key<UnitManifest>,
    surface_key: Key<Surface>,
    files: Vec<FileItems, A>,
    mark: Key<Mark>,
    audit_mark: Option<Aligned>,
    last: Option<Last<A>>,
    /// Per row of `last`'s dictionary: its digest now, and whether that moved.
    now: Vec<ContentId, A>,
    changed: Vec<bool, A>,
    row_ask: RowAsk,
    asked_rows: Vec<u32, A>,
    asked_ids: Vec<ContentId, A>,
    /// Per new file: the old file it continues.
    old_of: Vec<Option<u32>, A>,
    /// Per old file.
    chunks: Vec<ChunkState<A>, A>,
    /// Facts whose readers are to be walked once the chunks holding them are read.
    walks: Vec<ContentId, A>,
    slots: Vec<Slot<'s, A>, A>,
    /// Every body slot by stable id.
    index: Vec<(StableId, u32), A>,
    /// The unit's own made table now, sorted by id, and each row's maker.
    made: Vec<Fact, A>,
    makers: Vec<ContentId, A>,
    deps_universe: Vec<TableRef, A>,
    own_universe: Vec<TableRef, A>,
    surface_kept: bool,
    surface_result: ContentId,
    manifest: Option<Key<UnitManifest>>,
    universe: Option<ContentId>,
    mark_holds: Option<Holds<'s, Mark>>,
    /// Every table the unit's dependents may read through it: its closure and itself.
    closure: Vec<TableRef, A>,
    filing: Filing,
    chunk_out: Vec<(Key<UnitChunk>, Option<Aligned>), A>,
    manifest_out: Option<Aligned>,
    mark_out: Option<Aligned>,
    puts: u32,
}

enum Phase {
    Start,
    Certifying,
    Building,
    Filing,
    Done(Outcome),
}

/// The driver's placement rule, whatever the executor: a key task, and every task while fewer
/// than [`LOCAL_TASKS`] are queued or running (the edit loop), run locally; a round trip costs
/// more than the task. Only a cold fan-out goes remote, in batches.
pub fn stays_local(task: &Task, queued: u32) -> bool {
    matches!(task, Task::Key { .. }) || queued < LOCAL_TASKS
}

fn layout_of(tag: Tag) -> Layout {
    match tag {
        t if t == Surface::TAG => Surface::LAYOUT,
        t if t == Body::TAG => Body::LAYOUT,
        t if t == Mark::TAG => Mark::LAYOUT,
        t if t == Shape::TAG => Shape::LAYOUT,
        t if t == UnitChunk::TAG => UnitChunk::LAYOUT,
        t if t == Certificate::TAG => Certificate::LAYOUT,
        _ => UnitManifest::LAYOUT,
    }
}

fn slot_tag(slot: u32) -> Tag {
    match slot {
        SURFACE => Surface::TAG,
        _ => Body::TAG,
    }
}

fn memo_get<M: Memo>(memo: &M, arena: Arena, tag: Tag, key: ContentId) -> Option<Aligned> {
    let found = match tag {
        t if t == Mark::TAG => memo.get::<Mark>(arena, &Key::from_id(key)),
        t if t == UnitManifest::TAG => memo.get::<UnitManifest>(arena, &Key::from_id(key)),
        t if t == UnitChunk::TAG => memo.get::<UnitChunk>(arena, &Key::from_id(key)),
        t if t == Shape::TAG => memo.get::<Shape>(arena, &Key::from_id(key)),
        t if t == Body::TAG => memo.get::<Body>(arena, &Key::from_id(key)),
        t if t == Surface::TAG => memo.get::<Surface>(arena, &Key::from_id(key)),
        _ => None,
    };
    found.map(Aligned::copy_from)
}

fn memo_keep<M: Memo>(memo: &mut M, arena: Arena, holds: AnyHolds<'_>, bytes: &[u8]) {
    if let Some(holds) = holds.get::<Mark>() {
        memo.keep(arena, &holds, bytes);
    } else if let Some(holds) = holds.get::<UnitManifest>() {
        memo.keep(arena, &holds, bytes);
    } else if let Some(holds) = holds.get::<UnitChunk>() {
        memo.keep(arena, &holds, bytes);
    } else if let Some(holds) = holds.get::<Body>() {
        memo.keep(arena, &holds, bytes);
    } else if let Some(holds) = holds.get::<Surface>() {
        memo.keep(arena, &holds, bytes);
    }
}

/// The checked record a slot found or made: its address and root.
fn read_slot(slot: u32, key: ContentId, bytes: &[u8]) -> Result<(ContentId, &Traced), RecordError> {
    match slot {
        SURFACE => record::check_read::<Surface>(bytes, Key::from_id(key)).map(|(at, root)| (at.id(), root)),
        _ => record::check_read::<Body>(bytes, Key::from_id(key)).map(|(at, root)| (at.id(), root)),
    }
}

fn holds_slot<'s>(snap: Snapshot<'s>, slot: u32, address: ContentId, bytes: &[u8]) -> Result<AnyHolds<'s>, RecordError> {
    match slot {
        SURFACE => proof::hit::<Surface>(snap, Key::from_id(address), bytes).map(|hit| hit.holds().into()),
        _ => proof::hit::<Body>(snap, Key::from_id(address), bytes).map(|hit| hit.holds().into()),
    }
}

fn first_fact(a: &[Fact], b: &[Fact]) -> Option<ContentId> {
    for at in 0..a.len().max(b.len()) {
        match (a.get(at), b.get(at)) {
            (Some(x), Some(y)) if x == y => continue,
            (Some(x), Some(y)) => return Some(x.id.min(y.id)),
            (Some(x), None) | (None, Some(x)) => return Some(x.id),
            (None, None) => break,
        }
    }
    None
}

/// The first difference between a traced record found and the one made again.
pub fn first_difference(found: &[u8], made: &[u8]) -> First {
    let open = |bytes: &[u8]| -> Option<(Vec<Fact, Global>, Vec<Fact, Global>)> {
        let root = blaze_format::open::<Traced>(bytes).ok()?;
        let mut reads = Vec::new_in(Global);
        reads.extend_from_slice(root.reads.follow(bytes).ok()?);
        let mut facts = Vec::new_in(Global);
        facts.extend_from_slice(root.facts.follow(bytes).ok()?);
        Some((reads, facts))
    };
    match (open(found), open(made)) {
        (Some((found_reads, found_facts)), Some((made_reads, made_facts))) => {
            if let Some(id) = first_fact(&found_reads, &made_reads) {
                return First::Read(id);
            }
            match first_fact(&found_facts, &made_facts) {
                Some(id) => First::Made(id),
                None => First::Payload,
            }
        }
        _ => First::Payload,
    }
}

fn copy_in<T: Copy, A: Allocator>(from: &[T], alloc: A) -> Vec<T, A> {
    let mut out = Vec::with_capacity_in(from.len(), alloc);
    out.extend_from_slice(from);
    out
}

/// The driver of one request in snapshot `'s`, over memo `M` and executor `X`, allocating in the
/// request's frame `A`.
///
/// The invariants (tests.rs):
/// - Every record a request settles is the one a from-scratch build makes now: a record found
///   at an address was made by a run whose answers to its shape are the answers now, and a run
///   that gets the same answers asks the same questions (by induction from its key), so it is
///   that run's record.
/// - A unit whose text and view are unchanged is settled by one lookup, and nothing it names is
///   read.
/// - Nothing is refiled or copied; a unit files only what moved.
/// - No task runs for a record the store or the memo holds at the address its shape makes now.
/// - At most `max(workers, 1)` tasks run at once.
/// - A record is filed before the chunk naming it, chunks before their root, the root before the
///   mark, every mark before the certificate.
/// - Every request ends in an [`Outcome`]: the host's loop is bounded by the effects asked.
pub struct Driver<'s, 'm, M: Memo, X: Executor, A: Allocator + Clone> {
    snap: Snapshot<'s>,
    memo: &'m mut M,
    executor: X,
    workers: u32,
    running: u32,
    phase: Phase,
    units: Vec<Unit<'s, A>, A>,
    dependents: Vec<Vec<u32, A>, A>,
    ready: VecDeque<Work, A>,
    runs: VecDeque<Work, A>,
    pending: Vec<(Ticket, Work), A>,
    next_ticket: u64,
    manifests: Vec<Key<UnitManifest>, A>,
    complete: u32,
    ran: Ran,
    asks: Asks,
    ended: bool,
    alloc: A,
    certificate: Key<Certificate>,
    namespace: Namespace,
    lookup: Lookup<'s, Certificate>,
    certified: Option<Certified<'s>>,
    filer: Option<Filer<Certificate>>,
    cert_shape: Option<Aligned>,
    cert_shape_record: Option<Aligned>,
    cert_value: Option<Aligned>,
    /// Under audit: the certificate value found, compared at filing.
    audit_cert: Option<Aligned>,
    last_value: Option<Aligned>,
}

impl<'s, 'm, M: Memo, X: Executor, A: Allocator + Clone> Driver<'s, 'm, M, X, A> {
    /// A driver of one request in `snap`, over memo `M` and executor `X`, both static policies;
    /// at most `executor.capacity()` tasks run at once.
    pub fn new(snap: Snapshot<'s>, memo: &'m mut M, executor: X, plan: Plan<'_>, alloc: A) -> Result<Self, PlanError> {
        let count = plan.units.len();
        let mut units = Vec::with_capacity_in(count, alloc.clone());
        let mut dependents = Vec::with_capacity_in(count, alloc.clone());
        for _ in 0..count {
            dependents.push(Vec::new_in(alloc.clone()));
        }
        let mut owners = Vec::with_capacity_in(count, alloc.clone());
        for (index, unit) in plan.units.iter().enumerate() {
            if !unit.owner.is_crate() {
                return Err(PlanError::Owner { unit: index as u32 });
            }
            owners.push((unit.owner, index as u32));
            for &dep in unit.deps {
                if dep as usize >= index {
                    return Err(PlanError::DepOrder { unit: index as u32, dep });
                }
                dependents[dep as usize].push(index as u32);
            }
            units.push(Unit {
                state: UnitState::Waiting,
                waiting: unit.deps.len() as u32,
                deps: copy_in(unit.deps, alloc.clone()),
                owner: unit.owner,
                namespace: unit.namespace,
                plan_last: unit.last,
                text: Key::from_id(ZERO),
                surface_key: Key::from_id(ZERO),
                files: Vec::new_in(alloc.clone()),
                mark: Key::from_id(ZERO),
                audit_mark: None,
                last: None,
                now: Vec::new_in(alloc.clone()),
                changed: Vec::new_in(alloc.clone()),
                row_ask: RowAsk::Deps,
                asked_rows: Vec::new_in(alloc.clone()),
                asked_ids: Vec::new_in(alloc.clone()),
                old_of: Vec::new_in(alloc.clone()),
                chunks: Vec::new_in(alloc.clone()),
                walks: Vec::new_in(alloc.clone()),
                slots: Vec::new_in(alloc.clone()),
                index: Vec::new_in(alloc.clone()),
                made: Vec::new_in(alloc.clone()),
                makers: Vec::new_in(alloc.clone()),
                deps_universe: Vec::new_in(alloc.clone()),
                own_universe: Vec::new_in(alloc.clone()),
                surface_kept: false,
                surface_result: ZERO,
                manifest: None,
                universe: None,
                mark_holds: None,
                closure: Vec::new_in(alloc.clone()),
                filing: Filing::Chunks,
                chunk_out: Vec::new_in(alloc.clone()),
                manifest_out: None,
                mark_out: None,
                puts: 0,
            });
        }
        owners.sort_unstable();
        if let Some(pair) = owners.windows(2).find(|pair| pair[0].0 == pair[1].0) {
            return Err(PlanError::Owner { unit: pair[1].1.max(pair[0].1) });
        }
        let mut manifests = Vec::with_capacity_in(count, alloc.clone());
        manifests.resize(count, Key::from_id(ZERO));
        Ok(Driver {
            snap,
            memo,
            workers: executor.capacity().max(1),
            executor,
            running: 0,
            phase: Phase::Start,
            units,
            dependents,
            ready: VecDeque::new_in(alloc.clone()),
            runs: VecDeque::new_in(alloc.clone()),
            pending: Vec::new_in(alloc.clone()),
            next_ticket: 0,
            manifests,
            complete: 0,
            ran: Ran::default(),
            asks: Asks::default(),
            ended: false,
            alloc,
            certificate: plan.certificate,
            namespace: plan.namespace,
            lookup: Lookup::new(snap, plan.certificate),
            certified: None,
            filer: None,
            cert_shape: None,
            cert_shape_record: None,
            cert_value: None,
            audit_cert: None,
            last_value: None,
        })
    }

    pub fn executor(&self) -> &X {
        &self.executor
    }

    pub fn asks(&self) -> Asks {
        self.asks
    }

    pub fn ran(&self) -> Ran {
        self.ran
    }

    /// The certificate's proof, when it held.
    pub fn certified(&self) -> Option<Certified<'s>> {
        self.certified
    }

    /// The certificate's value record, when it held: the manifest roots it names, 32 bytes each
    /// in plan order, are its payload.
    pub fn certified_value(&self) -> Option<&[u8]> {
        self.certified.and(self.last_value.as_deref())
    }

    /// Unit `unit`'s manifest root, once its mark is held or filed: proved by the mark.
    pub fn manifest(&self, unit: u32) -> Option<Holds<'s, UnitManifest>> {
        let unit = self.units.get(unit as usize)?;
        Some(Holds::named_by(&unit.mark_holds?, unit.manifest?))
    }

    /// Unit `unit`'s universe digest, once settled.
    pub fn universe(&self, unit: u32) -> Option<ContentId> {
        self.units.get(unit as usize).and_then(|unit| unit.universe)
    }

    /// The records of unit `unit` proved in this request, by item ([`SURFACE_ITEM`] for its
    /// surface, named by its mark when the mark held): what a later stage (blaze-program, an
    /// action's reader) takes instead of checking again.
    pub fn proofs(&self, unit: u32) -> impl Iterator<Item = (StableId, AnyHolds<'s>)> + '_ {
        let named = self.units.get(unit as usize).and_then(|unit| {
            let mark = unit.mark_holds?;
            let surface = unit.own_universe.iter().find(|table| table.owner == unit.owner)?.surface?;
            unit.slots
                .first()
                .is_none_or(|slot| slot.holds.is_none())
                .then(|| (SURFACE_ITEM, AnyHolds::from(Holds::named_by(&mark, surface))))
        });
        named.into_iter().chain(
            self.units
                .get(unit as usize)
                .into_iter()
                .flat_map(|unit| unit.slots.iter().filter_map(|slot| slot.holds.map(|holds| (slot.item, holds)))),
        )
    }

    /// The next thing to do.
    pub fn next(&mut self) -> Step<'_> {
        match self.plan_next() {
            Some((work, ticket)) => Step::Do(self.effect(work, ticket)),
            None => match &self.phase {
                Phase::Done(outcome) => Step::Exit(outcome),
                _ => Step::Wait("the answers of the effects asked"),
            },
        }
    }

    fn ticket(&mut self, work: Work) -> Ticket {
        let ticket = Ticket(self.next_ticket);
        self.next_ticket += 1;
        self.pending.push((ticket, work));
        ticket
    }

    fn finish(&mut self, outcome: Outcome) {
        if matches!(self.phase, Phase::Done(_)) {
            return;
        }
        self.phase = Phase::Done(outcome);
        if !self.ended {
            self.ended = true;
            self.memo.end_request();
        }
    }

    fn fail(&mut self, why: Failure) {
        let ran = self.ran;
        self.finish(Outcome::Failed { ran, why });
    }

    fn done(&self) -> bool {
        matches!(self.phase, Phase::Done(_))
    }

    /// The next work with its ticket, owned, so the effect can borrow the driver after.
    fn plan_next(&mut self) -> Option<(Work, Ticket)> {
        match self.phase {
            Phase::Done(_) => return None,
            Phase::Start => {
                self.phase = Phase::Certifying;
                self.cert_step();
            }
            _ => {}
        }
        loop {
            if self.done() {
                return None;
            }
            if let Some(work) = self.ready.pop_front() {
                if self.begin(work) {
                    return Some((work, self.ticket(work)));
                }
                continue;
            }
            if self.running < self.workers || planted(Plant::OverCommitsPool) {
                if let Some(work) = self.runs.pop_front() {
                    self.running += 1;
                    match work {
                        Work::Key(_) => self.ran.keyed += 1,
                        Work::Run(_, slot) => {
                            self.ran.checks.records_run += 1;
                            match slot {
                                SURFACE => self.ran.surfaces += 1,
                                _ => self.ran.bodies += 1,
                            }
                        }
                        _ => {}
                    }
                    return Some((work, self.ticket(work)));
                }
            }
            break;
        }
        if self.pending.is_empty() && !self.done() {
            let ran = self.ran;
            self.finish(Outcome::Stalled { ran });
        }
        None
    }

    /// What a get work reads: its unit, kind and key.
    fn get_of(&self, work: Work) -> Option<(u32, Tag, ContentId)> {
        let unit = |u: u32| &self.units[u as usize];
        Some(match work {
            Work::GetMark(u) => (u, Mark::TAG, unit(u).mark.id()),
            Work::GetLast(u) => (u, UnitManifest::TAG, unit(u).plan_last?.id()),
            Work::GetChunk(u, o) => (u, UnitChunk::TAG, unit(u).last.as_ref()?.files.get(o as usize)?.chunk),
            Work::GetAddress(u, s) => (u, slot_tag(s), unit(u).slots[s as usize].asked),
            Work::GetNode(u, s) => (u, Shape::TAG, unit(u).slots[s as usize].node?.id()),
            Work::UnderMark(u) => {
                let surface = unit(u).own_universe.iter().find(|table| table.owner == unit(u).owner)?.surface?;
                (u, Surface::TAG, surface.id())
            }
            _ => return None,
        })
    }

    /// Whether `work` needs an effect; a memo hit is answered here.
    fn begin(&mut self, work: Work) -> bool {
        if matches!(work, Work::CertShape | Work::CertValue | Work::FileRead) {
            self.asks.gets += 1;
            return true;
        }
        if let Work::UnderMark(_) = work {
            self.ran.checks.rechecks += 1;
        }
        let Some((unit, tag, key)) = self.get_of(work) else {
            return true;
        };
        self.ran.checks.key_lookups += 1;
        match memo_get(&*self.memo, unit, tag, key) {
            None => {
                self.asks.gets += 1;
                true
            }
            Some(bytes) => {
                self.asks.memo_hits += 1;
                self.got(work, Some(&bytes));
                if !self.done() {
                    self.progress(unit);
                }
                false
            }
        }
    }

    fn effect(&self, work: Work, ticket: Ticket) -> Effect<'_> {
        let cert_get = |key: Key<Certificate>| Effect::Get {
            ticket,
            unit: NO_UNIT,
            tag: Certificate::TAG,
            key: key.id(),
            layout: Certificate::LAYOUT,
        };
        if let Some((unit, tag, key)) = self.get_of(work) {
            return Effect::Get { ticket, unit, tag, key, layout: layout_of(tag) };
        }
        match work {
            Work::CertShape | Work::CertValue => match self.lookup.want() {
                Want::ReadShape(at) | Want::ReadValue(at) => cert_get(at),
                _ => cert_get(Key::from_id(ZERO)),
            },
            Work::CertAnswers => Effect::Outside {
                ticket,
                tag: Certificate::TAG,
                key: self.certificate.id(),
                shape: self.cert_shape.as_deref().unwrap_or(&[]),
            },
            Work::Reads => Effect::Reads { ticket },
            Work::FileRead | Work::FilePut => {
                let want = self.filer.as_ref().map_or(Want::Done, |filer| filer.want());
                let (at, bytes): (Key<Certificate>, Option<&[u8]>) = match want {
                    Want::ReadShape(at) => return cert_get(at),
                    Want::PutShape(at) => (at, self.cert_shape_record.as_deref()),
                    Want::PutValue(at) => (at, self.cert_value.as_deref()),
                    _ => return cert_get(Key::from_id(ZERO)),
                };
                Effect::Put { ticket, tag: Certificate::TAG, key: at.id(), namespace: self.namespace, bytes: bytes.unwrap_or(&[]) }
            }
            Work::Key(unit) => Effect::Run { ticket, task: Task::Key { unit }, remote: false, universe: &[], made: &[] },
            Work::Rows(u) => {
                let unit = &self.units[u as usize];
                Effect::Answer { ticket, unit: u, asks: &unit.asked_ids, universe: &unit.own_universe, made: &unit.made }
            }
            Work::Probe(u, s) => {
                let unit = &self.units[u as usize];
                let (universe, made): (&[TableRef], &[Fact]) = match s {
                    SURFACE => (&unit.deps_universe, &[]),
                    _ => (&unit.own_universe, &unit.made),
                };
                Effect::Answer { ticket, unit: u, asks: &unit.slots[s as usize].probe, universe, made }
            }
            Work::Run(u, s) => {
                let unit = &self.units[u as usize];
                let slot = &unit.slots[s as usize];
                let (task, universe, made): (Task, &[TableRef], &[Fact]) = match s {
                    SURFACE => (Task::Surface { unit: u, key: Key::from_id(slot.key) }, &unit.deps_universe, &[]),
                    _ => (Task::Body { unit: u, item: slot.item, key: Key::from_id(slot.key) }, &unit.own_universe, &unit.made),
                };
                let queued = self.runs.len() as u32 + self.running;
                Effect::Run { ticket, task, remote: X::REMOTE && !stays_local(&task, queued), universe, made }
            }
            Work::PutRecord(u, s) => {
                let unit = &self.units[u as usize];
                let slot = &unit.slots[s as usize];
                let bytes = slot.bytes.as_deref().unwrap_or(&[]);
                Effect::Put { ticket, tag: slot_tag(s), key: slot.address, namespace: unit.namespace, bytes }
            }
            Work::PutNode(u, s) => {
                let unit = &self.units[u as usize];
                let slot = &unit.slots[s as usize];
                let key = slot.node.map_or(ZERO, |node| node.id());
                Effect::Put { ticket, tag: Shape::TAG, key, namespace: unit.namespace, bytes: slot.node_bytes.as_deref().unwrap_or(&[]) }
            }
            Work::PutChunk(u, f) => {
                let unit = &self.units[u as usize];
                let (key, bytes) = &unit.chunk_out[f as usize];
                Effect::Put { ticket, tag: UnitChunk::TAG, key: key.id(), namespace: unit.namespace, bytes: bytes.as_deref().unwrap_or(&[]) }
            }
            Work::PutManifest(u) => {
                let unit = &self.units[u as usize];
                let key = unit.manifest.map_or(ZERO, |key| key.id());
                Effect::Put { ticket, tag: UnitManifest::TAG, key, namespace: unit.namespace, bytes: unit.manifest_out.as_deref().unwrap_or(&[]) }
            }
            Work::PutMark(u) => {
                let unit = &self.units[u as usize];
                Effect::Put { ticket, tag: Mark::TAG, key: unit.mark.id(), namespace: unit.namespace, bytes: unit.mark_out.as_deref().unwrap_or(&[]) }
            }
            // Every get was answered above.
            _ => cert_get(Key::from_id(ZERO)),
        }
    }

    /// Gives the answer to an effect.
    pub fn give(&mut self, input: Input<'_>) {
        if self.done() {
            return;
        }
        let ticket = input.ticket();
        let Ok(at) = self.pending.binary_search_by_key(&ticket, |(ticket, _)| *ticket) else {
            return self.fail(Failure::Misused("an answer to no effect asked"));
        };
        let (_, work) = self.pending.remove(at);
        match (work, input) {
            (Work::CertShape, Input::Got { record, .. }) => match self.lookup.shape(record, &mut self.ran.checks) {
                Ok(shape) => {
                    self.cert_shape = shape.map(Aligned::copy_from);
                    self.cert_step();
                }
                Err(why) => self.fail(Failure::Outside(why)),
            },
            (Work::CertAnswers, Input::Answers { now, .. }) => match self.lookup.answers(now, &mut self.ran.checks) {
                Ok(()) => self.cert_step(),
                Err(why) => self.fail(Failure::Outside(why)),
            },
            (Work::CertValue, Input::Got { record, .. }) => {
                self.last_value = record.map(Aligned::copy_from);
                match self.lookup.value(record, &mut self.ran.checks) {
                    Ok(()) => self.cert_step(),
                    Err(why) => self.fail(Failure::Outside(why)),
                }
            }
            (Work::Reads, Input::Reads { shape, then, now, .. }) => self.reads(shape, then, now),
            (Work::FileRead, Input::Got { record, .. }) => {
                let shape = self.cert_shape.as_deref().unwrap_or(&[]);
                let read = match self.filer.as_mut() {
                    Some(filer) => filer.read(record, shape, &mut self.ran.checks),
                    None => Err(OutsideError::Misused("a certificate read with no filing")),
                };
                match read {
                    Ok(()) => self.file_step(),
                    Err(why) => self.filing_failed(why),
                }
            }
            (Work::FilePut, Input::Put { put, .. }) => {
                let filed = match (put, self.filer.as_mut()) {
                    (Ok(put), Some(filer)) => filer.put(put),
                    (Err(fault), _) => return self.fail(Failure::Store(fault)),
                    (_, None) => Err(OutsideError::Misused("a certificate put with no filing")),
                };
                match filed {
                    Ok(()) => self.file_step(),
                    Err(why) => self.filing_failed(why),
                }
            }
            (Work::Key(u), Input::Ran { outcome, .. }) => {
                self.running -= 1;
                match outcome {
                    Ok(Done::Keyed { manifest, surface, files, items }) => self.keyed(u, manifest, surface, files, items),
                    Ok(Done::Wrote { .. }) => self.fail(Failure::Misused("a key task that wrote a record")),
                    Err(failed) => self.fail(Failure::Task { tag: UnitManifest::TAG, unit: u, failed }),
                }
            }
            (
                Work::GetMark(_) | Work::GetLast(_) | Work::GetChunk(..) | Work::GetAddress(..) | Work::GetNode(..) | Work::UnderMark(_),
                Input::Got { record, .. },
            ) => self.got(work, record),
            (Work::Rows(u), Input::Answers { now, .. }) => self.rows_answered(u, now),
            (Work::Probe(u, s), Input::Answers { now, .. }) => self.probed(u, s, now),
            (Work::Run(u, s), Input::Ran { outcome, .. }) => {
                self.running -= 1;
                match outcome {
                    Ok(Done::Wrote { record }) => self.ran_slot(u, s, record),
                    Ok(Done::Keyed { .. }) => self.fail(Failure::Misused("a record task that keyed")),
                    Err(failed) => self.fail(Failure::Task { tag: slot_tag(s), unit: u, failed }),
                }
            }
            (Work::PutRecord(u, s) | Work::PutNode(u, s), Input::Put { put, .. }) => self.slot_put(u, s, work, put),
            (Work::PutChunk(u, _) | Work::PutManifest(u) | Work::PutMark(u), Input::Put { put, .. }) => self.unit_put(u, work, put),
            _ => self.fail(Failure::Misused("an answer of another form than its effect")),
        }
        if let Some(u) = work.unit()
            && !self.done()
        {
            self.progress(u);
        }
    }

    fn got(&mut self, work: Work, record: Option<&[u8]>) {
        match work {
            Work::GetMark(u) => self.got_mark(u, record),
            Work::GetLast(u) => self.got_last(u, record),
            Work::GetChunk(u, o) => self.got_chunk(u, o, record),
            Work::GetAddress(u, s) => self.got_address(u, s, record),
            Work::GetNode(u, s) => self.got_node(u, s, record),
            _ => {}
        }
    }

    // ---- the certificate ----

    /// The certificate's lookup, one step: the next read or answer, or its end.
    fn cert_step(&mut self) {
        match self.lookup.want() {
            Want::ReadShape(_) => self.ready.push_back(Work::CertShape),
            Want::ReadValue(_) => self.ready.push_back(Work::CertValue),
            Want::Answers => self.ready.push_back(Work::CertAnswers),
            _ => match self.lookup.found() {
                Some(Found::Hit(holds)) if M::AUDIT => {
                    self.ran.checks.audits += 1;
                    self.audit_cert = self.last_value.take();
                    let _ = holds;
                    self.cert_shape = None;
                    self.start();
                }
                Some(Found::Hit(holds)) => {
                    self.certified = Some(holds.certified());
                    let checks = self.ran.checks;
                    self.finish(Outcome::Certified { checks });
                }
                _ => {
                    self.cert_shape = None;
                    self.start();
                }
            },
        }
    }

    fn start(&mut self) {
        self.phase = Phase::Building;
        for unit in 0..self.units.len() {
            if self.units[unit].waiting == 0 {
                self.units[unit].state = UnitState::Keying;
                self.runs.push_back(Work::Key(unit as u32));
            }
        }
        if self.units.is_empty() {
            self.phase = Phase::Filing;
            self.ready.push_back(Work::Reads);
        }
    }

    /// The request's outside reads, settled: its certificate is filed at their address, its
    /// value every unit's manifest root.
    fn reads(&mut self, shape: &[u8], then: &[ContentId], now: &[ContentId]) {
        let settled = match outside::settle(self.certificate, shape, then, now, &mut self.ran.checks) {
            Ok(settled) => settled,
            Err(raced) => return self.fail(Failure::Raced { at: raced.at }),
        };
        let mut payload = Vec::with_capacity_in(self.manifests.len() * 32, self.alloc.clone());
        self.manifests.iter().for_each(|key| payload.extend_from_slice(&key.id().0));
        let value = outside::write_value(&settled.address, &[], &payload);
        if let Some(found) = self.audit_cert.take() {
            let ran = self.ran;
            return match *found == value[..] {
                true => self.finish(Outcome::Built { ran, filed: Filed::Audited }),
                false => self.fail(Failure::Audit { tag: Certificate::TAG, address: settled.address.id(), first: First::Payload }),
            };
        }
        // The lookup missed in this snapshot: it read the slots, so filing reads none again.
        let filer = match self.lookup.filer(settled, shape) {
            Ok(filer) => filer,
            Err(why) => return self.filing_failed(why),
        };
        self.cert_value = Some(Aligned::copy_from(&value));
        self.cert_shape = Some(Aligned::copy_from(shape));
        self.filer = Some(filer);
        self.file_step();
    }

    /// A filing that stopped: every slot taken is a built request whose next one settles
    /// through marks; anything else fails it.
    fn filing_failed(&mut self, why: OutsideError) {
        match why {
            OutsideError::TooManyShapes { .. } => {
                let ran = self.ran;
                self.finish(Outcome::Built { ran, filed: Filed::Full });
            }
            why => self.fail(Failure::Outside(why)),
        }
    }

    fn file_step(&mut self) {
        let want = self.filer.as_ref().map_or(Want::Done, |filer| filer.want());
        match want {
            Want::ReadShape(_) => self.ready.push_back(Work::FileRead),
            Want::PutShape(at) => {
                let shape = self.cert_shape.as_deref().unwrap_or(&[]);
                self.cert_shape_record = Some(Aligned::copy_from(&outside::write_shape(&at, shape)));
                self.ready.push_back(Work::FilePut);
            }
            Want::PutValue(_) => self.ready.push_back(Work::FilePut),
            _ => {
                let ran = self.ran;
                self.finish(Outcome::Built { ran, filed: Filed::Certificate });
            }
        }
    }

    // ---- a unit: key and mark ----

    fn keyed(
        &mut self,
        u: u32,
        manifest: Key<UnitManifest>,
        surface: Key<Surface>,
        files: &[FileItems],
        items: &[(StableId, Key<Body>)],
    ) {
        let alloc = self.alloc.clone();
        // The files' ranges cover the items in order, each once.
        let mut next = 0u32;
        for file in files {
            if file.at != next || file.at.checked_add(file.len).is_none_or(|end| end as usize > items.len()) {
                return self.fail(Failure::Misused("a key task whose files do not cover its items in order"));
            }
            next = file.at + file.len;
        }
        if next as usize != items.len() {
            return self.fail(Failure::Misused("a key task whose files do not cover its items in order"));
        }
        let view = {
            let deps = &self.units[u as usize].deps;
            let mut uds = Vec::with_capacity_in(deps.len(), alloc.clone());
            for &dep in deps.iter() {
                match self.units[dep as usize].universe {
                    Some(ud) => uds.push(ud),
                    None => return self.fail(Failure::Misused("a unit keyed before a dependency settled")),
                }
            }
            record::view(&uds)
        };
        let mut closure = Vec::new_in(alloc.clone());
        for &dep in self.units[u as usize].deps.iter() {
            closure.extend_from_slice(&self.units[dep as usize].closure);
        }
        closure.sort_by_key(|table: &TableRef| table.unit);
        closure.dedup_by_key(|table| table.unit);
        closure.sort_by_key(|table| table.owner);
        let unit = &mut self.units[u as usize];
        unit.text = manifest;
        unit.surface_key = surface;
        unit.files = copy_in(files, alloc.clone());
        unit.slots.clear();
        unit.slots.push(new_slot(SURFACE_ITEM, surface.id(), u32::MAX, alloc.clone()));
        for (f, file) in files.iter().enumerate() {
            for &(item, key) in &items[file.at as usize..(file.at + file.len) as usize] {
                unit.slots.push(new_slot(item, key.id(), f as u32, alloc.clone()));
            }
        }
        unit.index.clear();
        unit.index.extend(items.iter().enumerate().map(|(at, (item, _))| (*item, at as u32 + 1)));
        unit.index.sort_unstable_by_key(|(item, _)| *item);
        if unit.index.windows(2).any(|pair| pair[0].0 == pair[1].0) {
            return self.fail(Failure::Misused("a key task that gave one item twice"));
        }
        let mut own = copy_in(&closure, alloc.clone());
        own.push(TableRef { unit: u, owner: unit.owner, surface: None, manifest: None });
        own.sort_by_key(|table| table.owner);
        unit.deps_universe = closure;
        unit.own_universe = own;
        unit.mark = Mark::key(&MarkInputs { manifest, view, of: PhantomData });
        unit.state = UnitState::Marking;
        self.ready.push_back(Work::GetMark(u));
    }

    fn got_mark(&mut self, u: u32, record: Option<&[u8]>) {
        let Some(bytes) = record else {
            return self.miss(u);
        };
        let key = self.units[u as usize].mark;
        let hit = match proof::hit::<Mark>(self.snap, key, bytes) {
            Ok(hit) => hit,
            Err(why) => return self.fail(Failure::Record { tag: Mark::TAG, key: key.id(), why }),
        };
        let (holds, root) = (hit.holds(), *hit.root());
        memo_keep(&mut *self.memo, u, holds.into(), bytes);
        if M::AUDIT {
            self.ran.checks.audits += 1;
            self.units[u as usize].audit_mark = Some(Aligned::copy_from(bytes));
            return self.miss(u);
        }
        self.ran.marked += 1;
        let unit = &mut self.units[u as usize];
        unit.mark_holds = Some(holds);
        unit.manifest = Some(Key::from_id(root.manifest));
        unit.universe = Some(root.universe);
        set_own_surface(unit, Key::from_id(root.surface));
        if planted(Plant::LooksUpUnderMark) {
            self.ready.push_back(Work::UnderMark(u));
        }
        self.unit_complete(u);
    }

    /// No mark: from the last manifest, or (none, or under audit, which redoes every unit from
    /// scratch so its manifest and mark are compared whole) every item by its key alone.
    fn miss(&mut self, u: u32) {
        let unit = &mut self.units[u as usize];
        match unit.plan_last {
            Some(_) if !M::AUDIT => {
                unit.state = UnitState::Last;
                self.ready.push_back(Work::GetLast(u));
            }
            _ => self.cold(u),
        }
    }

    /// No last manifest: every item is examined, by its key's shape trie alone.
    fn cold(&mut self, u: u32) {
        let unit = &mut self.units[u as usize];
        unit.last = None;
        unit.state = UnitState::Surface;
        self.examine(u, SURFACE);
    }

    fn got_last(&mut self, u: u32, record: Option<&[u8]>) {
        let Some(bytes) = record else {
            return self.cold(u);
        };
        let Some(address) = self.units[u as usize].plan_last else {
            return self.fail(Failure::Misused("a last manifest read for a unit with none"));
        };
        let last = match read_last(address, bytes, self.alloc.clone()) {
            Ok(last) => last,
            Err(why) => return self.fail(Failure::Record { tag: UnitManifest::TAG, key: address.id(), why }),
        };
        if let Ok(hit) = proof::hit::<UnitManifest>(self.snap, address, bytes) {
            memo_keep(&mut *self.memo, u, hit.holds().into(), bytes);
        }
        let alloc = self.alloc.clone();
        let unit = &mut self.units[u as usize];
        let rows = last.dictionary.len();
        unit.now = Vec::with_capacity_in(rows, alloc.clone());
        unit.now.extend(last.dictionary.iter().map(|row| row.digest));
        unit.changed = Vec::with_capacity_in(rows, alloc.clone());
        unit.changed.resize(rows, false);
        unit.chunks = Vec::with_capacity_in(last.files.len(), alloc.clone());
        for _ in 0..last.files.len() {
            unit.chunks.push(ChunkState::Unread);
        }
        unit.asked_rows.clear();
        unit.asked_ids.clear();
        for (row, fact) in last.dictionary.iter().enumerate() {
            let other = match record::route(&fact.id) {
                Ok(Route::Outside) => true,
                Ok(Route::Surface(owner) | Route::Made(owner)) => owner != unit.owner,
                Ok(Route::Asker) => false,
                Err(_) => {
                    let why = RecordError::Malformed { at: row as u32, what: "dictionary" };
                    return self.fail(Failure::Record { tag: UnitManifest::TAG, key: address.id(), why });
                }
            };
            if other {
                unit.asked_rows.push(row as u32);
                unit.asked_ids.push(fact.id);
            }
        }
        unit.last = Some(last);
        // The made table starts as the last manifest's, before any item asks of it: a fact a
        // kept body makes is answered by its row, not as absent.
        self.refresh_made(u);
        let unit = &mut self.units[u as usize];
        match unit.asked_ids.is_empty() {
            true => self.after_deps(u),
            false => {
                unit.state = UnitState::DepRows;
                unit.row_ask = RowAsk::Deps;
                self.ready.push_back(Work::Rows(u));
            }
        }
    }

    fn rows_answered(&mut self, u: u32, now: &[ContentId]) {
        let unit = &mut self.units[u as usize];
        if now.len() != unit.asked_rows.len() {
            return self.fail(Failure::Misused("rows answered with another number of facts than asked"));
        }
        self.ran.checks.read_lists += 1;
        self.ran.checks.facts_compared += now.len() as u32;
        for (at, &row) in unit.asked_rows.iter().enumerate() {
            if unit.now[row as usize] != now[at] {
                unit.now[row as usize] = now[at];
                unit.changed[row as usize] = true;
            }
        }
        match unit.row_ask {
            RowAsk::Deps => self.after_deps(u),
            RowAsk::Own => self.after_own(u),
        }
    }

    /// The dependency rows answered: the surface stands, or is examined.
    fn after_deps(&mut self, u: u32) {
        let alloc = self.alloc.clone();
        let unit = &mut self.units[u as usize];
        let Some(last) = unit.last.as_ref() else {
            return self.fail(Failure::Misused("a dictionary answered with no last manifest"));
        };
        let moved = unit.surface_key.id() != last.surface_key
            || last.surface_shape.iter().any(|&row| unit.changed[row as usize]);
        let mut shape = Vec::with_capacity_in(last.surface_shape.len(), alloc);
        shape.extend(last.surface_shape.iter().map(|&row| last.dictionary[row as usize].id));
        shape.sort_unstable();
        let (address, result) = (last.surface, last.surface_result);
        unit.state = UnitState::Surface;
        match moved {
            false => {
                unit.surface_kept = true;
                unit.surface_result = result;
                let slot = &mut unit.slots[SURFACE as usize];
                slot.state = SlotState::Settled;
                slot.address = address;
                slot.result = result;
                set_own_surface(unit, Key::from_id(address));
                self.after_surface(u);
            }
            true => {
                unit.slots[SURFACE as usize].old = Some(OldEntry { address, shape });
                self.examine(u, SURFACE);
            }
        }
    }

    /// The surface settled: the unit's own rows are answered if its result moved.
    fn after_surface(&mut self, u: u32) {
        let unit = &mut self.units[u as usize];
        let (address, result) = {
            let slot = &unit.slots[SURFACE as usize];
            (slot.address, slot.result)
        };
        unit.surface_result = result;
        set_own_surface(unit, Key::from_id(address));
        let Some(last) = unit.last.as_ref() else {
            // Cold: every body is examined.
            unit.state = UnitState::Items;
            let mut all = Vec::with_capacity_in(unit.slots.len(), self.alloc.clone());
            all.extend(1..unit.slots.len() as u32);
            return self.examine_all(u, &all);
        };
        if unit.surface_kept || result == last.surface_result {
            return self.after_own(u);
        }
        unit.asked_rows.clear();
        unit.asked_ids.clear();
        for (row, fact) in last.dictionary.iter().enumerate() {
            let own = match record::route(&fact.id) {
                Ok(Route::Asker) => true,
                Ok(Route::Surface(owner)) => owner == unit.owner,
                _ => false,
            };
            if own {
                unit.asked_rows.push(row as u32);
                unit.asked_ids.push(fact.id);
            }
        }
        match unit.asked_ids.is_empty() {
            true => self.after_own(u),
            false => {
                unit.state = UnitState::OwnRows;
                unit.row_ask = RowAsk::Own;
                self.ready.push_back(Work::Rows(u));
            }
        }
    }

    /// Every row but the made ones answered: which chunks are read, and which facts walked.
    fn after_own(&mut self, u: u32) {
        let alloc = self.alloc.clone();
        let unit = &mut self.units[u as usize];
        unit.state = UnitState::Chunks;
        let Some(last) = unit.last.as_ref() else {
            return self.fail(Failure::Misused("rows answered with no last manifest"));
        };
        let mut used = Vec::with_capacity_in(last.files.len(), alloc.clone());
        used.resize(last.files.len(), false);
        unit.old_of = Vec::with_capacity_in(unit.files.len(), alloc.clone());
        for file in unit.files.iter() {
            let old = last.files.iter().enumerate().position(|(o, chunk)| !used[o] && chunk.file == file.image.id());
            if let Some(o) = old {
                used[o] = true;
            }
            unit.old_of.push(old.map(|o| o as u32));
        }
        if unit.files.len() == last.files.len() {
            for f in 0..unit.files.len() {
                if unit.old_of[f].is_none() && !used[f] {
                    used[f] = true;
                    unit.old_of[f] = Some(f as u32);
                }
            }
        }
        // A file whose image or item keys moved: its old chunk tells which items did.
        for f in 0..unit.files.len() {
            let Some(o) = unit.old_of[f] else { continue };
            let file = unit.files[f];
            let items = &unit.slots[1 + file.at as usize..1 + (file.at + file.len) as usize];
            if last.files[o as usize].file != file.image.id() || last.files[o as usize].keys != slot_keys(items) {
                want(&mut unit.chunks, o);
            }
        }
        for row in 0..last.dictionary.len() {
            if !unit.changed[row] {
                continue;
            }
            unit.walks.push(last.dictionary[row].id);
            for &chunk in last.readers(row) {
                if (chunk as usize) < last.files.len() {
                    want(&mut unit.chunks, chunk);
                }
            }
        }
        self.request_chunks(u);
    }

    fn request_chunks(&mut self, u: u32) {
        let unit = &mut self.units[u as usize];
        for (o, chunk) in unit.chunks.iter_mut().enumerate() {
            if let ChunkState::Wanted = chunk {
                *chunk = ChunkState::Asked;
                self.ready.push_back(Work::GetChunk(u, o as u32));
            }
        }
    }

    fn got_chunk(&mut self, u: u32, o: u32, record: Option<&[u8]>) {
        let alloc = self.alloc.clone();
        let Some(address) = self.units[u as usize].last.as_ref().and_then(|last| last.files.get(o as usize)).map(|file| file.chunk)
        else {
            return self.fail(Failure::Misused("a chunk read of a unit with no such old file"));
        };
        let state = match record {
            None => ChunkState::Gone,
            Some(bytes) => match read_chunk(Key::from_id(address), bytes, alloc) {
                Ok(chunk) => {
                    if let Ok(hit) = proof::hit::<UnitChunk>(self.snap, Key::from_id(address), bytes) {
                        memo_keep(&mut *self.memo, u, hit.holds().into(), bytes);
                    }
                    ChunkState::Held(chunk)
                }
                Err(why) => return self.fail(Failure::Record { tag: UnitChunk::TAG, key: address, why }),
            },
        };
        self.units[u as usize].chunks[o as usize] = state;
    }

    /// The first time every chunk asked is held: each file's items whose key moved, or whose
    /// file has no old chunk, are examined; then the walks.
    fn map_files(&mut self, u: u32) {
        let alloc = self.alloc.clone();
        let mut examine = Vec::new_in(alloc.clone());
        {
            let unit = &mut self.units[u as usize];
            unit.state = UnitState::Items;
            for f in 0..unit.files.len() {
                let file = unit.files[f];
                let range = 1 + file.at as usize..1 + (file.at + file.len) as usize;
                let chunk = match unit.old_of[f] {
                    None => None,
                    Some(o) => match &unit.chunks[o as usize] {
                        ChunkState::Held(chunk) => Some(chunk),
                        ChunkState::Gone => None,
                        // Unread: the file's image and keys are equal; nothing in it moved.
                        _ => continue,
                    },
                };
                for s in range {
                    let slot = &mut unit.slots[s];
                    match chunk.and_then(|chunk| chunk.entry(&slot.item).map(|at| (chunk, at))) {
                        Some((chunk, at)) => {
                            if chunk.entries[at].key != slot.key {
                                slot.old = Some(OldEntry { address: chunk.entries[at].address, shape: chunk.shape_of(at, alloc.clone()) });
                                examine.push(s as u32);
                            }
                        }
                        None => examine.push(s as u32),
                    }
                }
            }
        }
        self.examine_all(u, &examine);
    }

    /// Walks the postings of every fact in `walks` whose chunks are held: each kept reader is
    /// examined with its old entry.
    fn walk(&mut self, u: u32) {
        let alloc = self.alloc.clone();
        let mut examine = Vec::new_in(alloc.clone());
        {
            let unit = &mut self.units[u as usize];
            let Some(last) = unit.last.as_ref() else { return };
            let walks = core::mem::replace(&mut unit.walks, Vec::new_in(alloc.clone()));
            for id in walks.iter() {
                let Some(row) = last.row(id) else { continue };
                for &c in last.readers(row) {
                    let Some(ChunkState::Held(chunk)) = unit.chunks.get(c as usize) else { continue };
                    for &e in chunk.readers_of(id) {
                        let entry = &chunk.entries[e as usize];
                        let Ok(at) = unit.index.binary_search_by(|(item, _)| item.0.cmp(&entry.item)) else { continue };
                        let s = unit.index[at].1;
                        let slot = &mut unit.slots[s as usize];
                        if slot.state == SlotState::Kept && !slot.examined {
                            slot.old = Some(OldEntry { address: entry.address, shape: chunk.shape_of(e as usize, alloc.clone()) });
                            examine.push(s);
                        }
                    }
                }
            }
        }
        self.examine_all(u, &examine);
    }

    // ---- an item: candidates, the walk, the run ----

    fn examine(&mut self, u: u32, s: u32) {
        self.examine_all(u, &[s]);
    }

    /// Examines every item of `list` at once: all are marked first, so a reader sees its maker
    /// examined (and waits for it) whichever comes first in the list.
    fn examine_all(&mut self, u: u32, list: &[u32]) {
        for &s in list {
            let slot = &mut self.units[u as usize].slots[s as usize];
            if slot.examined && slot.state != SlotState::Settled && slot.state != SlotState::Kept {
                continue;
            }
            slot.examined = true;
            slot.released = false;
            slot.parked = false;
            slot.state = SlotState::Waiting;
        }
        for &s in list {
            self.try_start(u, s);
        }
    }

    /// Whether item `s` waits for a maker of a fact of its candidate shape (its parked probe when
    /// `parked`, else its old entry's shape): an answer of a fact the unit's own bodies make is
    /// final only once its maker settled, so a shape is answered only then (else the address
    /// asked, and under audit the work redone, see another made table than the one the record
    /// is checked against). A fact in the made table waits for its maker when that maker is
    /// examined and not settled (its row is provisional: the last manifest's, or a redone
    /// maker's last); a fact in no row waits while any other item of the unit is examined and
    /// not settled (no maker is known yet: cold, or under audit). A stall of every waiting item
    /// is released lowest first ([`Driver::progress`]); the readers that saw another digest are
    /// redone at quiesce.
    fn waits_on_maker(&self, u: u32, s: u32, parked: bool) -> bool {
        let unit = &self.units[u as usize];
        let slot = &unit.slots[s as usize];
        if s == SURFACE || slot.released || planted(Plant::MakerAfterReader) {
            return false;
        }
        let shape: &[ContentId] = match (parked, slot.old.as_ref()) {
            (true, _) => &slot.probe,
            (false, Some(old)) => &old.shape,
            (false, None) => return false,
        };
        let unsettled = |m: u32| m != s && unit.slots[m as usize].examined && unit.slots[m as usize].state != SlotState::Settled;
        shape.iter().any(|id| {
            if record::route(id) != Ok(Route::Made(unit.owner)) {
                return false;
            }
            match unit.made.binary_search_by(|fact| fact.id.cmp(id)) {
                Ok(at) => {
                    let maker = unit.makers[at];
                    let Ok(found) = unit.index.binary_search_by(|(item, _)| item.0.cmp(&maker)) else { return false };
                    unsettled(unit.index[found].1)
                }
                Err(_) => (1..unit.slots.len() as u32).any(&unsettled),
            }
        })
    }

    fn try_start(&mut self, u: u32, s: u32) {
        if s != SURFACE && self.units[u as usize].state < UnitState::Items {
            return;
        }
        if self.units[u as usize].slots[s as usize].state != SlotState::Waiting {
            return;
        }
        if self.units[u as usize].slots[s as usize].parked {
            if self.waits_on_maker(u, s, true) {
                return;
            }
            self.units[u as usize].slots[s as usize].parked = false;
            return self.ask_probe(u, s);
        }
        if self.waits_on_maker(u, s, false) {
            return;
        }
        let alloc = self.alloc.clone();
        let slot = &mut self.units[u as usize].slots[s as usize];
        match slot.old.as_ref() {
            Some(old) if planted(Plant::CacheHitNoReplay) => {
                slot.asked = old.address;
                slot.probing = Probing::Old;
                slot.state = SlotState::Getting;
                self.ready.push_back(Work::GetAddress(u, s));
            }
            Some(old) => {
                slot.probe = copy_in(&old.shape, alloc);
                slot.probing = Probing::Old;
                self.probe(u, s);
            }
            None => self.walk_from_root(u, s),
        }
    }

    fn walk_from_root(&mut self, u: u32, s: u32) {
        let slot = &mut self.units[u as usize].slots[s as usize];
        slot.depth = 0;
        slot.node = Some(match s {
            SURFACE => record::shape_root::<Surface>(Key::from_id(slot.key)),
            _ => record::shape_root::<Body>(Key::from_id(slot.key)),
        });
        slot.state = SlotState::Walking;
        self.ready.push_back(Work::GetNode(u, s));
    }

    /// Answers the slot's candidate shape now (none to answer: straight to its address), once
    /// every maker of a made fact of it settled: until then the slot is parked.
    fn probe(&mut self, u: u32, s: u32) {
        if self.waits_on_maker(u, s, true) {
            let slot = &mut self.units[u as usize].slots[s as usize];
            slot.parked = true;
            slot.state = SlotState::Waiting;
            return;
        }
        self.ask_probe(u, s);
    }

    fn ask_probe(&mut self, u: u32, s: u32) {
        let slot = &mut self.units[u as usize].slots[s as usize];
        match slot.probe.is_empty() {
            true => self.probed(u, s, &[]),
            false => {
                slot.state = SlotState::Asking;
                self.ready.push_back(Work::Probe(u, s));
            }
        }
    }

    fn probed(&mut self, u: u32, s: u32, now: &[ContentId]) {
        let alloc = self.alloc.clone();
        let slot = &mut self.units[u as usize].slots[s as usize];
        if now.len() != slot.probe.len() {
            return self.fail(Failure::Misused("a shape answered with another number of facts than it asks"));
        }
        if !now.is_empty() {
            self.ran.checks.read_lists += 1;
            self.ran.checks.facts_compared += now.len() as u32;
        }
        let mut facts = Vec::with_capacity_in(now.len(), alloc);
        facts.extend(slot.probe.iter().zip(now).map(|(id, digest)| Fact { id: *id, digest: *digest }));
        slot.asked = match s {
            SURFACE => record::address::<Surface>(Key::from_id(slot.key), &facts).id(),
            _ => record::address::<Body>(Key::from_id(slot.key), &facts).id(),
        };
        slot.state = SlotState::Getting;
        self.ready.push_back(Work::GetAddress(u, s));
    }

    fn got_address(&mut self, u: u32, s: u32, record: Option<&[u8]>) {
        let slot = &mut self.units[u as usize].slots[s as usize];
        let (key, asked) = (slot.key, slot.asked);
        let Some(bytes) = record else {
            return match slot.probing {
                Probing::Old => self.walk_from_root(u, s),
                Probing::Node => {
                    slot.depth += 1;
                    if slot.depth >= WALK_MOST {
                        self.ran.checks.walks_capped += 1;
                        slot.node = None;
                        return self.run(u, s);
                    }
                    slot.node = Some(match s {
                        SURFACE => record::shape_next::<Surface>(Key::from_id(asked)),
                        _ => record::shape_next::<Body>(Key::from_id(asked)),
                    });
                    slot.state = SlotState::Walking;
                    self.ready.push_back(Work::GetNode(u, s));
                }
            };
        };
        let address = match read_slot(s, key, bytes) {
            Ok((address, _)) => address,
            Err(why) => return self.fail(Failure::Record { tag: slot_tag(s), key: asked, why }),
        };
        if address != asked {
            let why = RecordError::Address { found: address, wanted: asked };
            return self.fail(Failure::Record { tag: slot_tag(s), key: asked, why });
        }
        match holds_slot(self.snap, s, address, bytes) {
            Ok(holds) => {
                memo_keep(&mut *self.memo, u, holds, bytes);
                self.units[u as usize].slots[s as usize].holds = Some(holds);
            }
            Err(why) => return self.fail(Failure::Record { tag: slot_tag(s), key: address, why }),
        }
        if M::AUDIT {
            self.ran.checks.audits += 1;
            self.units[u as usize].slots[s as usize].audit = Some(Aligned::copy_from(bytes));
            return self.run(u, s);
        }
        self.ran.found += 1;
        self.settle(u, s, address, bytes);
    }

    fn got_node(&mut self, u: u32, s: u32, record: Option<&[u8]>) {
        let alloc = self.alloc.clone();
        let slot = &mut self.units[u as usize].slots[s as usize];
        let Some(node) = slot.node else {
            return self.fail(Failure::Misused("a shape node read with no node asked"));
        };
        let Some(bytes) = record else {
            return self.run(u, s);
        };
        let ids = match record::open::<Shape>(bytes, &node).and_then(|root| root.ids.follow(bytes).map_err(RecordError::Format)) {
            Ok(ids) => ids,
            Err(why) => return self.fail(Failure::Record { tag: Shape::TAG, key: node.id(), why }),
        };
        if let Err(why) = record::sorted_ids(ids) {
            return self.fail(Failure::Record { tag: Shape::TAG, key: node.id(), why });
        }
        if slot.depth > 0 {
            self.ran.walked += 1;
        }
        slot.probe = copy_in(ids, alloc);
        slot.probing = Probing::Node;
        self.probe(u, s);
    }

    fn run(&mut self, u: u32, s: u32) {
        self.units[u as usize].slots[s as usize].state = SlotState::Running;
        self.runs.push_back(Work::Run(u, s));
    }

    fn ran_slot(&mut self, u: u32, s: u32, record: &[u8]) {
        let snap = self.snap;
        let alloc = self.alloc.clone();
        let slot = &mut self.units[u as usize].slots[s as usize];
        let (key, asked) = (slot.key, slot.asked);
        let (address, root) = match read_slot(s, key, record) {
            Ok(read) => read,
            Err(why) => return self.fail(Failure::Record { tag: slot_tag(s), key, why }),
        };
        if let Some(found) = slot.audit.take() {
            if *found != *record {
                let first = first_difference(&found, record);
                return self.fail(Failure::Audit { tag: slot_tag(s), address: asked, first });
            }
            self.ran.found += 1;
            return self.settle(u, s, address, record);
        }
        let reads = match root.reads.follow(record) {
            Ok(reads) => reads,
            Err(error) => return self.fail(Failure::Record { tag: slot_tag(s), key: address, why: RecordError::Format(error) }),
        };
        let holds = match holds_slot(snap, s, address, record) {
            Ok(holds) => holds,
            Err(why) => return self.fail(Failure::Record { tag: slot_tag(s), key: address, why }),
        };
        // The shape is filed at the node the walk ended at, unless it is the shape of the node
        // whose address missed (its record was gone; it is filed again now). The node goes
        // first: a record in the store is reachable from its key's root (a kill between the
        // two leaves a node whose address misses, which a walk passes).
        let same = slot.probing == Probing::Node && slot.probe.len() == reads.len() && slot.probe.iter().zip(reads).all(|(id, read)| *id == read.id);
        slot.holds = Some(holds);
        slot.asked = address;
        slot.address = address;
        slot.bytes = Some(Aligned::copy_from(record));
        slot.state = SlotState::Putting;
        match slot.node.filter(|_| !same) {
            Some(node) => {
                let mut ids = Vec::with_capacity_in(reads.len(), alloc);
                ids.extend(reads.iter().map(|read| read.id));
                slot.node_bytes = Some(Aligned::copy_from(&record::write_shape(&node, &ids)));
                self.ready.push_back(Work::PutNode(u, s));
            }
            None => self.ready.push_back(Work::PutRecord(u, s)),
        }
    }

    fn slot_put(&mut self, u: u32, s: u32, work: Work, put: Result<Put, StoreFault>) {
        let slot = &mut self.units[u as usize].slots[s as usize];
        match (work, put) {
            (_, Err(fault)) => return self.fail(Failure::Store(fault)),
            (Work::PutNode(..), Ok(put)) => {
                if put == Put::Other {
                    self.ran.checks.shape_races += 1;
                }
                slot.node_bytes = None;
                self.ready.push_back(Work::PutRecord(u, s));
                return;
            }
            (_, Ok(Put::Other)) => {
                let address = slot.address;
                return self.fail(Failure::TwoValues { tag: slot_tag(s), key: address });
            }
            (_, Ok(_)) => {
                if let (Some(holds), Some(bytes)) = (slot.holds, slot.bytes.as_deref()) {
                    memo_keep(&mut *self.memo, u, holds, bytes);
                }
            }
        }
        let Some(bytes) = slot.bytes.take() else {
            return self.fail(Failure::Misused("a record put with no bytes held"));
        };
        let address = slot.address;
        self.settle(u, s, address, &bytes);
    }

    /// Item `s` holds the record at `address`: its shape, its made facts and its result.
    fn settle(&mut self, u: u32, s: u32, address: ContentId, bytes: &[u8]) {
        let alloc = self.alloc.clone();
        let root = match blaze_format::open::<Traced>(bytes) {
            Ok(root) => root,
            Err(error) => return self.fail(Failure::Record { tag: slot_tag(s), key: address, why: RecordError::Format(error) }),
        };
        let (reads, facts, result) = match (root.reads.follow(bytes), root.facts.follow(bytes), record::result_digest(root, bytes)) {
            (Ok(reads), Ok(facts), Ok(result)) => (reads, facts, result),
            _ => return self.fail(Failure::Record { tag: slot_tag(s), key: address, why: RecordError::Malformed { at: 0, what: "root" } }),
        };
        let unit = &mut self.units[u as usize];
        let owner = unit.owner;
        let slot = &mut unit.slots[s as usize];
        slot.state = SlotState::Settled;
        slot.settles += 1;
        slot.address = address;
        slot.result = result;
        slot.reads = copy_in(reads, alloc.clone());
        if s != SURFACE {
            let mut made = Vec::with_capacity_in(facts.len(), alloc);
            made.extend(facts.iter().copied().filter(|fact| record::route(&fact.id) == Ok(Route::Made(owner))));
            slot.made = made;
        }
        if s == SURFACE {
            return self.after_surface(u);
        }
        self.refresh_made(u);
        // Items waiting for this maker may start.
        for w in 1..self.units[u as usize].slots.len() as u32 {
            if self.units[u as usize].slots[w as usize].state == SlotState::Waiting {
                self.try_start(u, w);
            }
        }
    }

    /// The unit's own made table now: the last manifest's rows of makers not examined, the
    /// examined makers' rows as last made (provisionally, until they settle), and the settled
    /// makers' own.
    fn refresh_made(&mut self, u: u32) {
        let unit = &mut self.units[u as usize];
        let mut rows: Vec<(Fact, ContentId), A> = Vec::new_in(self.alloc.clone());
        if let Some(last) = unit.last.as_ref() {
            for (fact, maker) in last.made.iter().zip(last.makers.iter()) {
                let settled = unit
                    .index
                    .binary_search_by(|(item, _)| item.0.cmp(maker))
                    .is_ok_and(|at| unit.slots[unit.index[at].1 as usize].settles > 0);
                if !settled {
                    rows.push((*fact, *maker));
                }
            }
        }
        for slot in unit.slots.iter().skip(1) {
            if slot.settles > 0 {
                rows.extend(slot.made.iter().map(|fact| (*fact, slot.item.0)));
            }
        }
        rows.sort_unstable_by_key(|(fact, _)| fact.id);
        rows.dedup_by_key(|(fact, _)| fact.id);
        unit.made.clear();
        unit.makers.clear();
        for (fact, maker) in rows {
            unit.made.push(fact);
            unit.makers.push(maker);
        }
    }

    /// After every event of unit `u`: starts what can start, releases a stall on makers, and
    /// when nothing of the unit is in flight, settles what its made facts moved, then files it.
    fn progress(&mut self, u: u32) {
        if self.done() {
            return;
        }
        let state = self.units[u as usize].state;
        if state == UnitState::Chunks || state == UnitState::Items {
            let asked = self.units[u as usize].chunks.iter().any(|chunk| matches!(chunk, ChunkState::Asked | ChunkState::Wanted));
            if asked {
                return;
            }
            if state == UnitState::Chunks {
                self.map_files(u);
            }
            self.walk(u);
        } else {
            return;
        }
        if self.done() {
            return;
        }
        let unit = &self.units[u as usize];
        let busy = unit.slots.iter().any(|slot| {
            matches!(slot.state, SlotState::Asking | SlotState::Getting | SlotState::Walking | SlotState::Running | SlotState::Putting)
        });
        if busy {
            return;
        }
        if let Some(w) = unit.slots.iter().position(|slot| slot.state == SlotState::Waiting) {
            // Every waiting item waits for a maker that waits too: the lowest goes first.
            self.units[u as usize].slots[w].released = true;
            return self.try_start(u, w as u32);
        }
        self.quiesce(u);
    }

    /// Nothing of the unit in flight: its made facts are final for what settled. Readers they
    /// moved are examined; when none is, the unit is filed.
    fn quiesce(&mut self, u: u32) {
        self.refresh_made(u);
        let alloc = self.alloc.clone();
        // The rows of the unit's own made facts, now, against the last manifest's.
        {
            let unit = &mut self.units[u as usize];
            if let Some(last) = unit.last.as_ref() {
                for row in 0..last.dictionary.len() {
                    let id = last.dictionary[row].id;
                    if record::route(&id) != Ok(Route::Made(unit.owner)) {
                        continue;
                    }
                    let now = made_answer(&unit.made, &id);
                    if now == unit.now[row] {
                        continue;
                    }
                    unit.now[row] = now;
                    unit.changed[row] = true;
                    unit.walks.push(id);
                    for &chunk in last.readers(row) {
                        if (chunk as usize) < last.files.len() {
                            want(&mut unit.chunks, chunk);
                        }
                    }
                }
            }
        }
        if !self.units[u as usize].walks.is_empty() {
            self.request_chunks(u);
            if self.units[u as usize].chunks.iter().any(|chunk| matches!(chunk, ChunkState::Asked)) {
                return;
            }
            self.walk(u);
            if self.units[u as usize].slots.iter().any(|slot| slot.state != SlotState::Settled && slot.state != SlotState::Kept) {
                return;
            }
        }
        // Readers examined in this request against the made table now.
        if !planted(Plant::MakerAfterReader) {
            let mut redo = Vec::new_in(alloc.clone());
            {
                let unit = &self.units[u as usize];
                for (s, slot) in unit.slots.iter().enumerate().skip(1) {
                    if slot.state != SlotState::Settled || !slot.examined {
                        continue;
                    }
                    let stale = slot.reads.iter().any(|read| {
                        record::route(&read.id) == Ok(Route::Made(unit.owner)) && read.digest != made_answer(&unit.made, &read.id)
                    });
                    if stale {
                        redo.push(s as u32);
                    }
                }
            }
            if !redo.is_empty() {
                let unit = &mut self.units[u as usize];
                if redo.iter().any(|&s| unit.slots[s as usize].settles >= SETTLES_MOST) {
                    let mut items = Vec::with_capacity_in(redo.len(), Global);
                    items.extend(redo.iter().map(|&s| unit.slots[s as usize].item));
                    return self.fail(Failure::MadeCycle { unit: u, items });
                }
                self.ran.redone += redo.len() as u32;
                for &s in redo.iter() {
                    let slot = &mut unit.slots[s as usize];
                    let mut shape = Vec::with_capacity_in(slot.reads.len(), alloc.clone());
                    shape.extend(slot.reads.iter().map(|read| read.id));
                    slot.old = Some(OldEntry { address: slot.address, shape });
                    slot.holds = None;
                }
                return self.examine_all(u, &redo);
            }
        }
        // The old chunks of every file to rebuild.
        {
            let unit = &mut self.units[u as usize];
            for f in 0..unit.files.len() {
                let Some(o) = unit.old_of.get(f).copied().flatten() else { continue };
                let file = unit.files[f];
                let examined = unit.slots[1 + file.at as usize..1 + (file.at + file.len) as usize].iter().any(|slot| slot.examined);
                if examined && matches!(unit.chunks[o as usize], ChunkState::Unread) {
                    unit.chunks[o as usize] = ChunkState::Wanted;
                }
            }
        }
        self.request_chunks(u);
        if self.units[u as usize].chunks.iter().any(|chunk| matches!(chunk, ChunkState::Asked)) {
            return;
        }
        // A file whose old chunk is not in the store: its items are found by address instead.
        let mut gone = Vec::new_in(alloc);
        {
            let unit = &self.units[u as usize];
            for f in 0..unit.files.len() {
                let Some(o) = unit.old_of.get(f).copied().flatten() else { continue };
                if !matches!(unit.chunks[o as usize], ChunkState::Gone) {
                    continue;
                }
                let file = unit.files[f];
                for s in 1 + file.at..1 + file.at + file.len {
                    if !unit.slots[s as usize].examined {
                        gone.push(s);
                    }
                }
            }
        }
        if !gone.is_empty() {
            return self.examine_all(u, &gone);
        }
        self.write(u);
    }

    // ---- filing a unit ----

    fn write(&mut self, u: u32) {
        let alloc = self.alloc.clone();
        let deps_uds = {
            let deps = &self.units[u as usize].deps;
            let mut uds = Vec::with_capacity_in(deps.len(), alloc.clone());
            uds.extend(deps.iter().map(|&dep| self.units[dep as usize].universe.unwrap_or(ZERO)));
            uds
        };
        let unit = &mut self.units[u as usize];
        unit.state = UnitState::Writing;
        let examined = unit.slots.iter().any(|slot| slot.examined);
        let stands = match unit.last.as_ref() {
            Some(last) => {
                !examined && unit.surface_kept && last.text == unit.text.id() && !unit.changed.iter().any(|changed| *changed)
            }
            None => false,
        };
        let (manifest, surface, surface_result, made_digest) = if let (true, Some(last)) = (stands, unit.last.as_ref()) {
            self.ran.stood += 1;
            unit.chunk_out.clear();
            (last.address, last.surface, last.surface_result, record::table_digest(&last.made))
        } else {
            match build_manifest(u, unit, alloc.clone()) {
                Ok((key, bytes)) => {
                    self.ran.manifests += 1;
                    self.ran.chunks += unit.chunk_out.iter().filter(|(_, bytes)| bytes.is_some()).count() as u32;
                    unit.manifest_out = Some(Aligned::copy_from(&bytes));
                    let slot = &unit.slots[SURFACE as usize];
                    (key, slot.address, slot.result, record::table_digest(&unit.made))
                }
                Err(why) => return self.fail(why),
            }
        };
        let universe = record::universe(surface_result, made_digest, &deps_uds);
        let mark = unit.mark;
        let mark_bytes = record::write_mark(&mark, manifest.id(), surface, universe);
        if let Some(found) = unit.audit_mark.take()
            && *found != mark_bytes[..]
        {
            return self.fail(Failure::Audit { tag: Mark::TAG, address: mark.id(), first: First::Payload });
        }
        match proof::hit::<Mark>(self.snap, mark, &mark_bytes) {
            Ok(hit) => unit.mark_holds = Some(hit.holds()),
            Err(why) => return self.fail(Failure::Record { tag: Mark::TAG, key: mark.id(), why }),
        }
        unit.manifest = Some(manifest);
        unit.universe = Some(universe);
        unit.surface_result = surface_result;
        set_own_surface(unit, Key::from_id(surface));
        unit.mark_out = Some(Aligned::copy_from(&mark_bytes));
        unit.filing = Filing::Chunks;
        self.file_unit(u);
    }

    /// Files the unit's moved chunks, then its root, then its mark, each stage after the last
    /// is in the store (all at once under the planted bug).
    fn file_unit(&mut self, u: u32) {
        let early = planted(Plant::ManifestBeforeRecords);
        let unit = &mut self.units[u as usize];
        loop {
            match unit.filing {
                Filing::Chunks => {
                    for (f, (_, bytes)) in unit.chunk_out.iter().enumerate() {
                        if bytes.is_some() {
                            unit.puts += 1;
                            self.ready.push_back(Work::PutChunk(u, f as u32));
                        }
                    }
                    unit.filing = Filing::Root;
                    if unit.puts > 0 && !early {
                        return;
                    }
                }
                Filing::Root => {
                    if unit.manifest_out.is_some() {
                        unit.puts += 1;
                        self.ready.push_back(Work::PutManifest(u));
                    }
                    unit.filing = Filing::Mark;
                    if unit.puts > 0 && !early {
                        return;
                    }
                }
                Filing::Mark => {
                    if unit.mark_out.is_some() {
                        unit.puts += 1;
                        self.ready.push_back(Work::PutMark(u));
                        return;
                    }
                    return self.unit_complete(u);
                }
            }
        }
    }

    fn unit_put(&mut self, u: u32, work: Work, put: Result<Put, StoreFault>) {
        let (tag, key) = {
            let unit = &self.units[u as usize];
            match work {
                Work::PutChunk(_, f) => (UnitChunk::TAG, unit.chunk_out[f as usize].0.id()),
                Work::PutManifest(_) => (UnitManifest::TAG, unit.manifest.map_or(ZERO, |key| key.id())),
                _ => (Mark::TAG, unit.mark.id()),
            }
        };
        match put {
            Err(fault) => return self.fail(Failure::Store(fault)),
            Ok(Put::Other) => return self.fail(Failure::TwoValues { tag, key }),
            Ok(_) => {}
        }
        let unit = &mut self.units[u as usize];
        unit.puts -= 1;
        if work == Work::PutMark(u) {
            if let (Some(holds), Some(bytes)) = (unit.mark_holds, unit.mark_out.take()) {
                memo_keep(&mut *self.memo, u, holds.into(), &bytes);
            }
            return self.unit_complete(u);
        }
        if unit.puts == 0 {
            self.file_unit(u);
        }
    }

    fn unit_complete(&mut self, u: u32) {
        let alloc = self.alloc.clone();
        let unit = &mut self.units[u as usize];
        if unit.state == UnitState::Complete {
            return;
        }
        unit.state = UnitState::Complete;
        let Some(manifest) = unit.manifest else {
            return self.fail(Failure::Misused("a unit complete with no manifest"));
        };
        let surface = unit.own_universe.iter().find(|table| table.owner == unit.owner).and_then(|table| table.surface);
        let own = TableRef { unit: u, owner: unit.owner, surface, manifest: Some(manifest) };
        let mut closure = copy_in(&unit.deps_universe, alloc);
        closure.push(own);
        closure.sort_by_key(|table| table.owner);
        unit.closure = closure;
        self.manifests[u as usize] = manifest;
        self.complete += 1;
        for at in 0..self.dependents[u as usize].len() {
            let dependent = self.dependents[u as usize][at] as usize;
            self.units[dependent].waiting -= 1;
            if self.units[dependent].waiting == 0 {
                self.units[dependent].state = UnitState::Keying;
                self.runs.push_back(Work::Key(dependent as u32));
            }
        }
        if self.complete as usize == self.units.len() {
            self.phase = Phase::Filing;
            self.ready.push_back(Work::Reads);
        }
    }
}

fn new_slot<'s, A: Allocator + Clone>(item: StableId, key: ContentId, file: u32, alloc: A) -> Slot<'s, A> {
    Slot {
        item,
        key,
        file,
        old: None,
        state: SlotState::Kept,
        examined: false,
        released: false,
        parked: false,
        settles: 0,
        probing: Probing::Old,
        probe: Vec::new_in(alloc.clone()),
        asked: ZERO,
        depth: 0,
        node: None,
        address: ZERO,
        result: ZERO,
        reads: Vec::new_in(alloc.clone()),
        made: Vec::new_in(alloc),
        bytes: None,
        node_bytes: None,
        audit: None,
        holds: None,
    }
}

fn want<A: Allocator>(chunks: &mut [ChunkState<A>], o: u32) {
    if let Some(chunk @ ChunkState::Unread) = chunks.get_mut(o as usize) {
        *chunk = ChunkState::Wanted;
    }
}

fn set_own_surface<A: Allocator>(unit: &mut Unit<'_, A>, surface: Key<Surface>) {
    let owner = unit.owner;
    if let Some(table) = unit.own_universe.iter_mut().find(|table| table.owner == owner) {
        table.surface = Some(surface);
    }
}

/// The answer now of an own made fact: its row, or its absence.
fn made_answer(made: &[Fact], id: &ContentId) -> ContentId {
    match made.binary_search_by(|fact| fact.id.cmp(id)) {
        Ok(at) => made[at].digest,
        Err(_) => record::absent(id),
    }
}

/// The keys digest of a file's slots, as [`record::keys_digest`] of its items.
fn slot_keys<A: Allocator>(slots: &[Slot<'_, A>]) -> ContentId {
    let mut hasher = blaze_ids::Hasher::new("blaze-keyed file keys");
    hasher.update(&(slots.len() as u64).to_le_bytes());
    for slot in slots {
        hasher.id(&slot.item.0).id(&slot.key);
    }
    hasher.finish()
}

fn malformed(at: usize, what: &'static str) -> RecordError {
    RecordError::Malformed { at: at as u32, what }
}

/// The last manifest's root, checked and copied out: every index in its bounds.
fn read_last<A: Allocator + Clone>(address: Key<UnitManifest>, bytes: &[u8], alloc: A) -> Result<Last<A>, RecordError> {
    let root: &ManifestRoot = record::open::<UnitManifest>(bytes, &address)?;
    let follow = |error| RecordError::Format(error);
    let files = root.files.follow(bytes).map_err(follow)?;
    let dictionary = root.dictionary.follow(bytes).map_err(follow)?;
    let readers = root.readers.follow(bytes).map_err(follow)?;
    let reader_at = root.reader_at.follow(bytes).map_err(follow)?;
    let surface_shape = root.surface_shape.follow(bytes).map_err(follow)?;
    let made = root.made.follow(bytes).map_err(follow)?;
    let makers = root.makers.follow(bytes).map_err(follow)?;
    record::sorted(dictionary)?;
    record::sorted(made)?;
    if reader_at.len() != dictionary.len() + 1 {
        return Err(malformed(reader_at.len(), "reader_at"));
    }
    if let Some(at) = reader_at.windows(2).position(|pair| pair[0] > pair[1]) {
        return Err(malformed(at, "reader_at"));
    }
    if reader_at.last().is_some_and(|&end| end as usize > readers.len()) {
        return Err(malformed(dictionary.len(), "reader_at"));
    }
    if let Some(at) = readers.iter().position(|&chunk| chunk as usize > files.len()) {
        return Err(malformed(at, "readers"));
    }
    if let Some(at) = surface_shape.iter().position(|&row| row as usize >= dictionary.len()) {
        return Err(malformed(at, "surface_shape"));
    }
    if makers.len() != made.len() {
        return Err(malformed(makers.len(), "makers"));
    }
    Ok(Last {
        address,
        text: root.key,
        surface_key: root.surface_key,
        surface: root.surface,
        surface_result: root.surface_result,
        files: copy_in(files, alloc.clone()),
        dictionary: copy_in(dictionary, alloc.clone()),
        readers: copy_in(readers, alloc.clone()),
        reader_at: copy_in(reader_at, alloc.clone()),
        surface_shape: copy_in(surface_shape, alloc.clone()),
        made: copy_in(made, alloc.clone()),
        makers: copy_in(makers, alloc),
    })
}

/// An old chunk, checked and copied out.
fn read_chunk<A: Allocator + Clone>(address: Key<UnitChunk>, bytes: &[u8], alloc: A) -> Result<OldChunk<A>, RecordError> {
    let root: &ChunkRoot = record::open::<UnitChunk>(bytes, &address)?;
    let follow = |error| RecordError::Format(error);
    let entries = root.entries.follow(bytes).map_err(follow)?;
    let shapes = root.shapes.follow(bytes).map_err(follow)?;
    let dictionary = root.dictionary.follow(bytes).map_err(follow)?;
    let postings = root.postings.follow(bytes).map_err(follow)?;
    let posting_at = root.posting_at.follow(bytes).map_err(follow)?;
    record::sorted_ids(dictionary)?;
    if let Some(at) = entries.windows(2).position(|pair| pair[0].item >= pair[1].item) {
        return Err(RecordError::Unsorted { at: at as u32 + 1 });
    }
    if let Some(at) = entries.iter().position(|entry| entry.shape_at as usize + entry.shape_len as usize > shapes.len()) {
        return Err(malformed(at, "entries"));
    }
    if let Some(at) = shapes.iter().position(|&ord| ord as usize >= dictionary.len()) {
        return Err(malformed(at, "shapes"));
    }
    if posting_at.len() != dictionary.len() + 1 || posting_at.windows(2).any(|pair| pair[0] > pair[1]) {
        return Err(malformed(posting_at.len(), "posting_at"));
    }
    if posting_at.last().is_some_and(|&end| end as usize > postings.len()) {
        return Err(malformed(dictionary.len(), "posting_at"));
    }
    if let Some(at) = postings.iter().position(|&entry| entry as usize >= entries.len()) {
        return Err(malformed(at, "postings"));
    }
    Ok(OldChunk {
        entries: copy_in(entries, alloc.clone()),
        shapes: copy_in(shapes, alloc.clone()),
        dictionary: copy_in(dictionary, alloc.clone()),
        postings: copy_in(postings, alloc.clone()),
        posting_at: copy_in(posting_at, alloc),
    })
}

/// One entry being filed: its item's identity and the ids of its shape.
struct NewEntry<A: Allocator> {
    entry: Entry,
    shape: Vec<ContentId, A>,
}

/// A chunk of `entries`: its dictionary of every id the entries' shapes ask, their shapes as
/// ordinals, and the postings. Floor: one sort of the ids, one pass.
fn build_chunk<A: Allocator + Clone>(unit: ContentId, file: ContentId, entries: &mut [NewEntry<A>], alloc: A) -> (Key<UnitChunk>, Vec<u8>) {
    let mut dictionary = Vec::new_in(alloc.clone());
    for entry in entries.iter() {
        dictionary.extend_from_slice(&entry.shape);
    }
    dictionary.sort_unstable();
    dictionary.dedup();
    if planted(Plant::DictionaryMissesFact) {
        dictionary.pop();
    }
    let mut shapes = Vec::new_in(alloc.clone());
    let mut rows = Vec::with_capacity_in(entries.len(), alloc.clone());
    let mut readers: Vec<(u32, u32), A> = Vec::new_in(alloc.clone());
    for (e, entry) in entries.iter_mut().enumerate() {
        let at = shapes.len() as u32;
        for id in entry.shape.iter() {
            if let Ok(ord) = dictionary.binary_search(id) {
                shapes.push(ord as u32);
                readers.push((ord as u32, e as u32));
            }
        }
        entry.entry.shape_at = at;
        entry.entry.shape_len = shapes.len() as u32 - at;
        rows.push(entry.entry);
    }
    readers.sort_unstable();
    let mut postings = Vec::with_capacity_in(readers.len(), alloc.clone());
    let mut posting_at = Vec::with_capacity_in(dictionary.len() + 1, alloc);
    let mut next = 0;
    for k in 0..dictionary.len() as u32 {
        posting_at.push(postings.len() as u32);
        while next < readers.len() && readers[next].0 == k {
            postings.push(readers[next].1);
            next += 1;
        }
    }
    posting_at.push(postings.len() as u32);
    record::write_chunk(&ChunkParts { unit, file, entries: &rows, shapes: &shapes, dictionary: &dictionary, postings: &postings, posting_at: &posting_at })
}

/// The digest now of fact `id` as a kept item read it: its row of the last dictionary.
fn kept_digest<A: Allocator>(unit: &Unit<'_, A>, id: &ContentId) -> Option<ContentId> {
    let last = unit.last.as_ref()?;
    last.row(id).map(|row| unit.now[row])
}

/// The unit's chunks (each reused when nothing in its file moved, else rebuilt) and its root:
/// `unit.chunk_out` is filled, and the root's address and bytes returned.
fn build_manifest<A: Allocator + Clone>(u: u32, unit: &mut Unit<'_, A>, alloc: A) -> Result<(Key<UnitManifest>, Vec<u8>), Failure> {
    let n = unit.files.len();
    let unit_id = unit.text.id();
    let mut files = Vec::with_capacity_in(n, alloc.clone());
    // (id, digest, reader): every fact a record of the unit read, by the chunk that holds it.
    let mut rows: Vec<(ContentId, ContentId, u32), A> = Vec::new_in(alloc.clone());
    let old_files = unit.last.as_ref().map_or(0, |last| last.files.len());
    let mut reused: Vec<Option<u32>, A> = Vec::with_capacity_in(old_files, alloc.clone());
    reused.resize(old_files, None);
    unit.chunk_out.clear();
    for f in 0..n {
        let file = unit.files[f];
        let range = 1 + file.at as usize..1 + (file.at + file.len) as usize;
        let keys = slot_keys(&unit.slots[range.clone()]);
        let examined = unit.slots[range.clone()].iter().any(|slot| slot.examined);
        let old = unit.old_of.get(f).copied().flatten();
        if let (Some(o), Some(last), false) = (old, unit.last.as_ref(), examined) {
            let chunk = last.files[o as usize];
            if chunk.file == file.image.id() && chunk.keys == keys {
                reused[o as usize] = Some(f as u32);
                files.push(FileChunk { file: file.image.id(), chunk: chunk.chunk, keys, items: file.len, pad: 0 });
                unit.chunk_out.push((Key::from_id(chunk.chunk), None));
                continue;
            }
        }
        let held = match old {
            Some(o) => match &unit.chunks[o as usize] {
                ChunkState::Held(chunk) => Some(chunk),
                _ => None,
            },
            None => None,
        };
        let mut entries = Vec::with_capacity_in(file.len as usize, alloc.clone());
        for slot in &unit.slots[range] {
            let entry = |address, result| Entry {
                item: slot.item.0,
                key: slot.key,
                address,
                result,
                shape_at: 0,
                shape_len: 0,
                tag: Body::TAG.0,
                pad: 0,
                pad2: 0,
            };
            if slot.examined {
                let mut shape = Vec::with_capacity_in(slot.reads.len(), alloc.clone());
                shape.extend(slot.reads.iter().map(|read| read.id));
                for read in slot.reads.iter() {
                    rows.push((read.id, read.digest, f as u32));
                }
                entries.push(NewEntry { entry: entry(slot.address, slot.result), shape });
                continue;
            }
            let Some((chunk, at)) = held.and_then(|chunk| chunk.entry(&slot.item).map(|at| (chunk, at))) else {
                return Err(Failure::Misused("an item kept with no entry of its last chunk held"));
            };
            let shape = chunk.shape_of(at, alloc.clone());
            for id in shape.iter() {
                let Some(digest) = kept_digest(unit, id) else {
                    return Err(Failure::Misused("a kept item's fact is not in its last dictionary"));
                };
                rows.push((*id, digest, f as u32));
            }
            let old = chunk.entries[at];
            entries.push(NewEntry { entry: entry(old.address, old.result), shape });
        }
        let (address, bytes) = build_chunk(unit_id, file.image.id(), &mut entries, alloc.clone());
        files.push(FileChunk { file: file.image.id(), chunk: address.id(), keys, items: file.len, pad: 0 });
        unit.chunk_out.push((address, Some(Aligned::copy_from(&bytes))));
    }
    // The rows of reused chunks, as the last dictionary has them now.
    if let Some(last) = unit.last.as_ref() {
        for row in 0..last.dictionary.len() {
            for &c in last.readers(row) {
                if let Some(Some(f)) = reused.get(c as usize) {
                    rows.push((last.dictionary[row].id, unit.now[row], *f));
                }
            }
        }
    }
    // The surface's shape.
    let surface = &unit.slots[SURFACE as usize];
    match (unit.surface_kept, unit.last.as_ref()) {
        (true, Some(last)) => {
            for &row in last.surface_shape.iter() {
                rows.push((last.dictionary[row as usize].id, unit.now[row as usize], n as u32));
            }
        }
        _ => {
            for read in surface.reads.iter() {
                rows.push((read.id, read.digest, n as u32));
            }
        }
    }
    rows.sort_unstable();
    rows.dedup();
    let mut dictionary = Vec::new_in(alloc.clone());
    let mut readers = Vec::with_capacity_in(rows.len(), alloc.clone());
    let mut reader_at = Vec::new_in(alloc.clone());
    for (at, &(id, digest, reader)) in rows.iter().enumerate() {
        match dictionary.last().map(|fact: &Fact| (fact.id, fact.digest)) {
            Some((last, seen)) if last == id => {
                if seen != digest {
                    return Err(Failure::TwoDigests { unit: u, id });
                }
                if rows[at - 1].2 != reader {
                    readers.push(reader);
                }
            }
            _ => {
                reader_at.push(readers.len() as u32);
                dictionary.push(Fact { id, digest });
                readers.push(reader);
            }
        }
    }
    reader_at.push(readers.len() as u32);
    let mut surface_shape = Vec::new_in(alloc.clone());
    match (unit.surface_kept, unit.last.as_ref()) {
        (true, Some(last)) => {
            for &row in last.surface_shape.iter() {
                let id = last.dictionary[row as usize].id;
                if let Ok(at) = dictionary.binary_search_by(|fact: &Fact| fact.id.cmp(&id)) {
                    surface_shape.push(at as u32);
                }
            }
        }
        _ => {
            for read in surface.reads.iter() {
                if let Ok(at) = dictionary.binary_search_by(|fact: &Fact| fact.id.cmp(&read.id)) {
                    surface_shape.push(at as u32);
                }
            }
        }
    }
    surface_shape.sort_unstable();
    let parts = ManifestParts {
        key: unit.text,
        surface_key: unit.surface_key.id(),
        surface: surface.address,
        surface_result: surface.result,
        files: &files,
        dictionary: &dictionary,
        readers: &readers,
        reader_at: &reader_at,
        surface_shape: &surface_shape,
        made: &unit.made,
        makers: &unit.makers,
    };
    Ok(record::write_manifest(&parts))
}
