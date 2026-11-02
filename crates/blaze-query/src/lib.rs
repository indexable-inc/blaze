#![feature(allocator_api, allocator_ext)]
//! blaze v2's query database (docs/v2/plan/lanes/db.md section 7, docs/v2/plan/lanes/items.md
//! section 6.2).
//!
//! - A query is asked of a [`QueryKey`]: its kind and the content ID of its arguments. Results
//!   are keyed by input hashes, never by view or revision, so every view shares every equal
//!   result, and equal work in flight runs once (the plane's single flight, keyed by
//!   [`QueryKey::id`]).
//! - Each result keeps a [`Fingerprint`], and each dependency an [`Edge`]: what was read and
//!   the fingerprint it had then. A rerun that produces the same fingerprint stops there
//!   (early cutoff); fingerprints outlive evicted values, so eviction never breaks cutoff.
//!   No result is reused unless every edge's fingerprint is equal (#350).
//! - A **fact** is the narrowest thing a query read (docs/v2/plan/lanes/items.md section 6.2's table): a
//!   query of its own whose value is small and whose fingerprint is the hash it stands for (an
//!   interface hash, a layout hash, "N in S resolves to X"). The kinds of fact are
//!   `blaze-items`'s.
//! - The only per-view queries are the inputs ([`Input`]): a file of the view, the view's
//!   configuration. Everything else is a function of content.
//! - [`Why`] answers, for a query that ran, the chain of edges back to the input that changed.
//!
//! How it works (docs/v2/plan/lanes/db/query-database.md):
//!
//! - A key keeps every memo made of it, each with its edges in the order they were read
//!   (constructive traces). In a view, a memo is the key's result when each of its edges, in
//!   order, has the fingerprint it saw; verifying stops at the first that differs, so it never
//!   asks a dependency the query would not ask (a key read after a changed one may not exist
//!   any more). A query is deterministic, so at most one memo of a key holds in a view.
//! - A view is a root, which never changes: what a view resolved stays resolved, with no
//!   revision. Another view verifies the same memos against its own inputs, sharing every memo
//!   whose inputs are equal.
//! - A memo is a `u32` row of the store's columns (`memo`); its edges are one range of the
//!   edge segment of the thread that ran it, its value bytes in that thread's value segment
//!   (`segment`). A value is in memory, in a mapped file (a cold pack or the image), or
//!   evicted, its fingerprint kept; an evicted value asked for is made again and must have the
//!   fingerprint.
//! - A lookup takes no lock and writes no shared memory (docs/v2/plan/design/concurrency.md):
//!   every table is open-addressed over rows that never move, published with one release
//!   store, and a memo's value is one word (its row and that row's generation), swapped with a
//!   compare-and-swap to evict it, move it to disk or refill it. A warm hit allocates nothing.
//! - What a writer unlinks (a row, a table, a value, a memo, a closed view) is freed once every
//!   task that could have read it has ended ([`blaze_reclaim`]). Everything the database hands
//!   out borrows the task it was read in: a query's [`Cx`], or a [`Pinned`] from outside one.

pub mod db;
mod image;
mod memo;
mod segment;
mod subtree;
mod sync;
mod table;
pub mod tree;

#[cfg(test)]
mod tests;

pub use blaze_query_macros::{Supertype, Value, input, interned, tracked};

use std::{
    any::{Any, TypeId},
    cell::{Cell, RefCell},
    collections::VecDeque,
    fmt,
    sync::{Arc, OnceLock},
};

use blaze_core::{Completion, Done, Effect, FileName, Mapping, Tag};
use blaze_format::{FormatError, Persist, SchemaHash, Writer};
use blaze_graph::pump::Flights;
use blaze_ids::{
    ContentId, Fingerprint, Hasher, Idx,
    bits::{AtomicBitSlice, BitSet, BitSlice, Bits},
};
use blaze_image::{Builder, Corrupt, Image, Refused, SectionId};
use blaze_protocol::cbor;
use blaze_reclaim::{Grace, Guard};
use blaze_vfs::{View, ViewId};
use serde::{Deserialize, Serialize};

use crate::{
    image::ImageMap,
    memo::{MemoId, Shared, Slot, Store},
    segment::Span,
    sync::{
        atomic::{AtomicU32, Ordering},
        thread_local,
    },
    table::{Column, EdgeRecord, Edit, FxMap, FxSet, Map, Rows, ShortLock},
};
pub use crate::{
    subtree::TreeHash,
    tree::{MemTree, Names, Tree},
};

/// A kind of query, by number. Numbers are given in `blaze-query`'s [`kinds`] for the
/// database's own, and by the crate that defines the rest (facts are `blaze-items`'s); one
/// number is one query for every blaze that reads a given schema.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
#[serde(transparent)]
pub struct QueryKind(pub u16);

/// The kind numbers of one crate's queries: `RANGE`, a constant for each kind, [`ALL`] with
/// each query's name, `of(name)`, and `name(kind)` from a table built when compiled, which
/// refuses a number outside `RANGE` or given twice.
#[macro_export]
macro_rules! query_kinds {
    (
        range: $range:expr, owner: $owner:literal;
        $($(#[$meta:meta])* $name:ident = $number:literal, $query:literal;)*
    ) => {
        pub use $crate::QueryKind;

        #[doc = ::core::concat!("The numbers ", $owner, " may give.")]
        pub const RANGE: ::core::ops::Range<u16> = $range;

        $($(#[$meta])* pub const $name: QueryKind = QueryKind($number);)*

        /// Every kind, with its query as its crate names it: a function's name, or
        /// `Type::method` for a method of a tracked `impl`.
        pub const ALL: &[(QueryKind, &str)] = &[$(($name, $query)),*];

        /// The kind of the query named `name`, as [`ALL`] names it.
        pub fn of(name: &str) -> Option<QueryKind> {
            match name {
                $($query => Some($name),)*
                _ => None,
            }
        }

        const NAMES: [Option<&str>; (RANGE.end - RANGE.start) as usize] = {
            let mut names = [None; (RANGE.end - RANGE.start) as usize];
            let mut at = 0;
            while at < ALL.len() {
                let (kind, name) = ALL[at];
                ::core::assert!(
                    RANGE.start <= kind.0 && kind.0 < RANGE.end,
                    ::core::concat!("a kind outside ", $owner, "'s range"),
                );
                let slot = (kind.0 - RANGE.start) as usize;
                ::core::assert!(names[slot].is_none(), "a kind given to two queries");
                names[slot] = Some(name);
                at += 1;
            }
            names
        };

        /// The query of `kind`, if it is this crate's: one bounds check and one load.
        pub fn name(kind: QueryKind) -> Option<&'static str> {
            NAMES.get(kind.0.wrapping_sub(RANGE.start) as usize).copied().flatten()
        }
    };
}

/// The database's own kinds, and the ranges given to others.
pub mod kinds {
    use super::QueryKind;

    /// A file of a view, by path: its content ID.
    pub const FILE: QueryKind = QueryKind(1);
    /// A directory of a view, by path: the content ID of its sorted names.
    pub const DIR: QueryKind = QueryKind(2);
    /// The view's configuration (target, profile, features asked for): its content ID.
    pub const CONFIG: QueryKind = QueryKind(3);
    /// The hash of everything below a directory of a view ([`crate::Cx::tree`]), one per
    /// directory: the database's own query, not an input.
    pub const TREE: QueryKind = QueryKind(4);
    /// A file of an immutable root other than the view's, by root and path: its content ID.
    pub const FILE_AT: QueryKind = QueryKind(5);
    /// A directory of an immutable root other than the view's, by root and path: the content
    /// ID of its sorted names.
    pub const DIR_AT: QueryKind = QueryKind(6);
    /// Facts (`blaze-items`): `0x1000..0x2000`.
    pub const FACTS: std::ops::Range<u16> = 0x1000..0x2000;
    /// The front end's queries (parse, expand, resolve, infer, MIR): `0x2000..0x4000`.
    pub const FRONT: std::ops::Range<u16> = 0x2000..0x4000;
    /// Codegen and link: `0x4000..0x5000`.
    pub const BACK: std::ops::Range<u16> = 0x4000..0x5000;
    /// The front end's syntax (parse), within [`FRONT`]: `0x2000..0x2200`. These sub-ranges are
    /// how `blaze_graph::admission::tag_of` places a kind in its subsystem.
    pub const SYNTAX: std::ops::Range<u16> = 0x2000..0x2200;
    /// Names (resolve): `0x2200..0x2400`.
    pub const NAMES: std::ops::Range<u16> = 0x2200..0x2400;
    /// Signatures, bodies and inference: `0x2400..0x2c00`.
    pub const INFER: std::ops::Range<u16> = 0x2400..0x2c00;
    /// Macros (expand): `0x2c00..0x3000`.
    pub const EXPAND: std::ops::Range<u16> = 0x2c00..0x3000;
    /// MIR, its passes, borrow checking and const eval: `0x3000..0x4000`.
    pub const MIR: std::ops::Range<u16> = 0x3000..0x4000;
    /// Codegen, within [`BACK`]: `0x4000..0x4800`.
    pub const CODEGEN: std::ops::Range<u16> = 0x4000..0x4800;
    /// Link: `0x4800..0x5000`.
    pub const LINK: std::ops::Range<u16> = 0x4800..0x5000;
    /// Build scripts, proc macros and tests: `0x5000..0x6000`.
    pub const USER: std::ops::Range<u16> = 0x5000..0x6000;
    /// The workspace load and the unit graph (lane cargo): `0x6000..0x6800`.
    pub const CARGO: std::ops::Range<u16> = 0x6000..0x6800;
}

fn is_input(kind: QueryKind) -> bool {
    matches!(kind, kinds::FILE | kinds::DIR | kinds::CONFIG | kinds::FILE_AT | kinds::DIR_AT)
}

/// What a query is asked of: its kind and the content ID of its arguments, which are content
/// IDs themselves (never a dense index, a view or a revision).
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct QueryKey {
    pub kind: QueryKind,
    pub input: ContentId,
}

impl QueryKey {
    pub const DOMAIN: &'static str = "blaze v2 query key 1";

    /// The key's own content ID: the plane's task key and the store's key of its result.
    pub fn id(&self) -> ContentId {
        blaze_ids::Hasher::new(QueryKey::DOMAIN)
            .update(&self.kind.0.to_le_bytes())
            .id(&self.input)
            .finish()
    }

    pub fn task_key(&self) -> blaze_graph::Key {
        blaze_graph::Key(self.id().0)
    }
}

/// One dependency of a result: the query it read, and that query's fingerprint when it did.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Edge {
    pub saw: Fingerprint,
    pub read: QueryKey,
}

/// What the database keeps of one result.
#[derive(PartialEq, Eq, Debug)]
pub struct Memo {
    pub key: QueryKey,
    pub fingerprint: Fingerprint,
    /// In the order they were read: verifying walks them in this order and stops at the first
    /// that differs.
    pub edges: Vec<Edge>,
    /// The schema of the stored value and the content ID of the code that produced it (#1439):
    /// a value of another schema or producer is not this one.
    pub schema: SchemaHash,
    pub producer: ContentId,
    /// Where the value is: in memory, on disk in the store, or gone (its fingerprint kept).
    pub value: Stored,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stored {
    Memory,
    /// In the store, under this key.
    Disk(ContentId),
    Evicted,
}

/// What a query reads of a view directly.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Hash, Debug)]
pub enum Input {
    File(String),
    Dir(String),
    Config,
    /// A file under an immutable root of the tree other than the view's ([`Cx::file_at`]).
    FileAt { root: ContentId, path: String },
    /// A directory under an immutable root of the tree other than the view's ([`Cx::dir_at`]).
    DirAt { root: ContentId, path: String },
}

/// The domain of an input's path in its [`QueryKey`].
pub const PATH_DOMAIN: &str = "blaze v2 db input path 1";
/// The domain of an immutable root and a path under it in an input's [`QueryKey`].
pub const ROOT_PATH_DOMAIN: &str = "blaze v2 db input root path 1";
pub const CONFIG_DOMAIN: &str = "blaze v2 db input config 1";
/// The domain of a value's bytes: its content ID, and the first 128 bits of it its default
/// fingerprint.
pub const VALUE_DOMAIN: &str = "blaze v2 db value 1";
const ABSENT_DOMAIN: &str = "blaze v2 db input absent 1";
const NAMES_DOMAIN: &str = "blaze v2 db input names 1";
const FILE_DOMAIN: &str = "blaze v2 db input file 1";

impl Input {
    pub fn key(&self) -> QueryKey {
        match self {
            Input::File(path) => Input::file_key(path),
            Input::Dir(path) => Input::dir_key(path),
            Input::Config => Input::config_key(),
            Input::FileAt { root, path } => Input::file_at_key(*root, path),
            Input::DirAt { root, path } => Input::dir_at_key(*root, path),
        }
    }

    /// [`Input::FileAt`]'s key, made without the input.
    pub fn file_at_key(root: ContentId, path: &str) -> QueryKey {
        QueryKey { kind: kinds::FILE_AT, input: Hasher::new(ROOT_PATH_DOMAIN).id(&root).part(path.as_bytes()).finish() }
    }

    /// [`Input::DirAt`]'s key, made without the input.
    pub fn dir_at_key(root: ContentId, path: &str) -> QueryKey {
        QueryKey { kind: kinds::DIR_AT, input: Hasher::new(ROOT_PATH_DOMAIN).id(&root).part(path.as_bytes()).finish() }
    }

    /// [`Input::File`]'s key, made without the input.
    pub fn file_key(path: &str) -> QueryKey {
        QueryKey { kind: kinds::FILE, input: ContentId::of(PATH_DOMAIN, path.as_bytes()) }
    }

    /// [`Input::Dir`]'s key, made without the input.
    pub fn dir_key(path: &str) -> QueryKey {
        QueryKey { kind: kinds::DIR, input: ContentId::of(PATH_DOMAIN, path.as_bytes()) }
    }

    pub fn config_key() -> QueryKey {
        static KEY: OnceLock<QueryKey> = OnceLock::new();
        *KEY.get_or_init(|| QueryKey { kind: kinds::CONFIG, input: ContentId::of(CONFIG_DOMAIN, b"") })
    }
}

/// A query: a pure function of its key, read through [`Cx`], which records every edge.
pub trait Query: 'static {
    const KIND: QueryKind;
    const NAME: &'static str;
    /// The content ID of the code of the query's function, for [`Memo::producer`]; set by the
    /// build (blaze's own item hashes of it).
    const PRODUCER: ContentId;
    /// The subsystem its runs allocate for: each run is a `blaze_mem::frame::task` of it, so
    /// what the run's code takes through `blaze_mem::frame::current` counts there.
    const TAG: blaze_mem::Tag = blaze_mem::Tag::Query;
    /// Equal values of this kind are kept once, whichever view or key made them: a memo of an
    /// equal value (its schema and fingerprint, then its bytes) names the one value row. Two
    /// values of one fingerprint whose bytes differ panic, naming the query. For kinds whose
    /// values often repeat across keys (docs/v2/plan/lanes/fastcompile/hashcons.md, "Which
    /// values"); a kind that leaves it costs nothing.
    const SHARE: bool = false;
    /// Kept as bytes (deterministic CBOR) beside the key's memos and in the image, so a
    /// dependency no one names again can still be run to verify an edge on it.
    type Key: Clone + Send + Sync + Serialize + serde::de::DeserializeOwned + 'static;
    type Value: Persist;
    /// The content ID of `key`, for [`QueryKey::input`].
    fn input(key: &Self::Key) -> ContentId;
    fn execute(cx: &mut Cx<'_>, key: &Self::Key) -> Result<Self::Value, Cancelled>;
    /// The value's fingerprint, given the value and its bytes in the format (written once,
    /// as the database keeps them): by default the hash of the bytes.
    fn fingerprint(_value: &Self::Value, bytes: &[u8]) -> Fingerprint {
        value_fingerprint(bytes)
    }
}

/// `value` written into a buffer of its own, as the database keeps it.
pub fn write_value<V: Persist>(value: &V) -> Vec<u8> {
    let mut out = Writer::new(V::SCHEMA);
    let root = value.write(&mut out);
    out.finish(root)
}

/// The fingerprint of a value's bytes.
pub fn value_fingerprint(bytes: &[u8]) -> Fingerprint {
    value_id(bytes).fingerprint()
}

pub(crate) fn value_id(bytes: &[u8]) -> ContentId {
    ContentId::of(VALUE_DOMAIN, bytes)
}

/// A key handed to a query's runner: the caller's own, or one read back once from the bytes
/// the database kept ([`Database::keys`]).
// Kinds register at run time into a table of `Runner` fn pointers, so the key is erased and
// read back by one `TypeId` compare when the query runs.
// dyn: only when a query runs, never on a lookup that finds its memo.
type AnyKey = dyn Any + Send + Sync;

/// A run's value, written once into a buffer of this thread's (see [`BUFFERS`]); copied once
/// into the thread's value segment by the store.
struct Computed {
    bytes: Vec<u8>,
    fingerprint: Fingerprint,
}

/// How many value buffers a thread keeps, and the largest it keeps: runs nest, each holding
/// one buffer until its value is copied, and one huge value should not stay pinned.
const BUFFERS_KEPT: usize = 8;
const BUFFER_KEPT_BYTES: usize = 1 << 20;

impl Drop for Computed {
    fn drop(&mut self) {
        let bytes = std::mem::take(&mut self.bytes);
        if (1..=BUFFER_KEPT_BYTES).contains(&bytes.capacity()) {
            keep_buffer(bytes);
        }
    }
}

/// Gives a value buffer back to this thread's ([`BUFFERS`]). They are per-call scratch, never
/// held across a query call, so a job on a fiber does not carry them: a buffer taken on one
/// thread may come back to another's.
#[inline(never)]
fn keep_buffer(bytes: Vec<u8>) {
    // A thread being torn down has no buffers left to keep it in; the buffer is freed.
    let _ = BUFFERS.try_with(|buffers| {
        let mut buffers = buffers.borrow_mut();
        if buffers.len() < BUFFERS_KEPT {
            buffers.push(bytes);
        }
    });
}

#[inline(never)]
fn spare_buffer() -> Vec<u8> {
    BUFFERS.with(|buffers| buffers.borrow_mut().pop()).unwrap_or_default()
}

/// Runs one kind's query.
type Runner = fn(&mut Cx<'_>, &AnyKey) -> Result<Computed, Cancelled>;

fn run_query<Q: Query>(cx: &mut Cx<'_>, key: &AnyKey) -> Result<Computed, Cancelled> {
    let key = key
        .downcast_ref::<Q::Key>()
        .unwrap_or_else(|| panic!("query {} was handed a key of another type", Q::NAME));
    let value = Q::execute(cx, key)?;
    let buffer = spare_buffer();
    let mut out = Writer::with_buffer(<Q::Value as Persist>::SCHEMA, buffer);
    let root = value.write(&mut out);
    let bytes = out.finish(root);
    let fingerprint = Q::fingerprint(&value, &bytes);
    Ok(Computed { bytes, fingerprint })
}

/// Reads one kind's key back from the bytes the database kept: `None` when they don't decode
/// (a key of another schema, from an image), which makes the memo that named it unverifiable,
/// never wrong.
type KeyDecoder = fn(&[u8]) -> Option<Box<AnyKey>>;

fn decode_key<Q: Query>(bytes: &[u8]) -> Option<Box<AnyKey>> {
    // alloc: once a key, kept in `Database::keys` while its slot lives.
    cbor::from_slice::<Q::Key>(bytes).ok().map(|key| Box::new(key) as Box<AnyKey>)
}

struct KindInfo {
    name: &'static str,
    producer: ContentId,
    schema: SchemaHash,
    /// The store's number of `(schema, producer)`: a memo of another is never reused (#1439).
    code: u32,
    query: TypeId,
    run: Runner,
    decode: KeyDecoder,
    /// The tag of the frame task each run is ([`Query::TAG`]).
    tag: blaze_mem::Tag,
    /// Set for a [`Query::SHARE`] kind.
    share: Option<Shared>,
    /// The symbols a value of the kind names (`Persist::symbols`), marked live when an image
    /// is written.
    symbols: fn(&[u8], &mut Vec<u32>) -> Result<(), FormatError>,
}

/// The registered queries by kind number: a kind's row is at its number in one reserved
/// region, so finding it is a load of its live flag and of its row, independent of each
/// other, with no hash probed (every ask finds its kind). Written under `&mut`, read under
/// `&`, never removed.
struct Kinds {
    rows: Rows<KindInfo>,
    /// The kinds registered, for a settle to walk.
    numbers: Vec<QueryKind>,
}

impl Kinds {
    fn new() -> Kinds {
        // alloc: grows once per kind registered, at setup.
        Kinds { rows: Rows::sized(1 << 16), numbers: Vec::new() }
    }

    #[inline]
    fn get(&self, kind: QueryKind) -> Option<&KindInfo> {
        // SAFETY: a kind's row is written by `insert` under `&mut self`, which every `&self`
        // follows, and dropped only with the rows.
        unsafe { self.rows.written(u32::from(kind.0)) }
    }

    fn insert(&mut self, kind: QueryKind, info: KindInfo) {
        assert!(self.get(kind).is_none(), "kind {} registered twice", kind.0);
        // SAFETY: `&mut self` excludes every reader, and the row holds no value (just checked).
        unsafe { self.rows.put(u32::from(kind.0), info) };
        self.numbers.push(kind);
    }

    fn for_each_mut(&mut self, mut f: impl FnMut(&mut KindInfo)) {
        for &kind in &self.numbers {
            // SAFETY: registered, so written; `&mut self` excludes readers.
            f(unsafe { self.rows.get_mut(u32::from(kind.0)) });
        }
    }
}

/// This thread's runs: the keys they are running, outermost first, and the edges each
/// running query has read so far, a query's above those of the query that asked it. Runs
/// nest as calls do (a query asks another inline), so each takes
/// the top of both stacks and gives it back when it returns; the buffers stay, so a run
/// allocates nothing once they have grown.
struct Stacks {
    keys: Vec<QueryKey>,
    edges: Vec<EdgeRecord>,
}

thread_local! {
    /// Whether this thread's task runs a query. Carried with a job on a fiber: another job
    /// the thread runs while it waits is a task of its own, free to ask the database.
    static INSIDE: Cell<bool> = const { Cell::new(false) };
    // alloc: empty until a run grows them, then reused by every run of the thread.
    static STACKS: RefCell<Stacks> = const { RefCell::new(Stacks { keys: Vec::new(), edges: Vec::new() }) };
    /// Value buffers a run writes into and gives back once the store copied its value, so a
    /// run allocates nothing for its value once they have grown.
    static BUFFERS: RefCell<Vec<Vec<u8>>> = const { RefCell::new(Vec::new()) }; // alloc: as `STACKS`
    /// The file a run on this thread could not read, set as it stops the run with
    /// [`Cancelled`] and taken by the [`Pinned::get`] that run is under, which fails with it.
    static UNREADABLE: Cell<Option<Corrupt>> = const { Cell::new(None) };
}

// Every read of a thread-local below sits in an `#[inline(never)]` function: a job on a fiber may
// wait between two reads in one function and resume on another thread, and an inlined read
// could keep the first thread's address across the wait.

/// Stops a run on what it could not read, for its request to fail with ([`stopped`]).
#[inline(never)]
fn unreadable(corrupt: Corrupt) -> Cancelled {
    UNREADABLE.with(|it| it.set(Some(corrupt)));
    Cancelled
}

/// What a [`Cancelled`] run's request fails with: the file it could not read, if that is why
/// it stopped.
#[inline(never)]
fn stopped(_: Cancelled) -> DbError {
    match take_unreadable_corrupt() {
        Some(corrupt) => DbError::Corrupt(corrupt),
        None => DbError::Cancelled,
    }
}

#[inline(never)]
fn take_unreadable_corrupt() -> Option<Corrupt> {
    UNREADABLE.with(Cell::take)
}

#[inline(never)]
fn inside_a_query() -> bool {
    INSIDE.with(Cell::get)
}

/// Marks this thread's task as running a query while it lives.
struct Inside(bool);

impl Inside {
    #[inline(never)]
    fn enter() -> Inside {
        Inside(INSIDE.with(|it| it.replace(true)))
    }
}

impl Drop for Inside {
    #[inline(never)]
    fn drop(&mut self) {
        INSIDE.with(|it| it.set(self.0));
    }
}

/// This thread's stacks, for `f`.
#[inline(never)]
fn with_stacks<R>(f: impl FnOnce(&RefCell<Stacks>) -> R) -> R {
    STACKS.with(f)
}

/// [`with_stacks`], `None` on a thread being torn down.
#[inline(never)]
fn try_with_stacks<R>(f: impl FnOnce(&RefCell<Stacks>) -> R) -> Option<R> {
    STACKS.try_with(f).ok()
}

/// Empty stacks threads left when a job's were put on them, for the next job to switch out.
// alloc: none: an empty `Vec` in a static; it grows to the most jobs switched out at once.
static SPARE_STACKS: ShortLock<Vec<Box<Stacks>>> = ShortLock::new(Vec::new());

/// Moves a thread-local of a crate above the engine with each job on a fiber across its waits,
/// as [`carry_with_fiber_jobs`] does the engine's: one a job sets and reads again after a query
/// call, which may resume it on another thread.
pub use blaze_graph::sched::{Carried, carry};

/// What is told of each query run, once it ran ([`on_each_run`]).
static ON_RUN: OnceLock<fn()> = OnceLock::new();

/// Has `hook` called on the running thread after each query's function runs (never on a hit
/// or a verify): the daemon's stall watchdog counts it as work moving, since a long check can
/// run for minutes inside queries with no span closing. Once per process; a second hook is
/// refused and handed back.
pub fn on_each_run(hook: fn()) -> Result<(), fn()> {
    ON_RUN.set(hook)
}

#[inline]
pub(crate) fn ran_one() {
    if let Some(hook) = ON_RUN.get() {
        hook();
    }
}

/// Carries this engine's per-run thread-locals with each job on a fiber across its waits
/// ([`carry`]): the query stacks (`db`'s and [`STACKS`]), [`INSIDE`] and
/// [`UNREADABLE`]. Once per process; every database's construction calls it, before a driver
/// can run one of its queries.
pub fn carry_with_fiber_jobs() {
    static CARRIED: std::sync::Once = std::sync::Once::new();
    CARRIED.call_once(|| {
        carry(db::carried());
        carry(Carried { take: take_stacks, put: put_stacks });
        carry(Carried { take: take_inside, put: put_inside });
        carry(Carried { take: take_unreadable, put: put_unreadable });
    });
}

/// Moves the switching job's [`STACKS`] off this thread, leaving empty ones.
#[inline(never)]
fn take_stacks() -> usize {
    let spare = SPARE_STACKS.lock().pop();
    // alloc: stacks once per job switched out at once, then kept in `SPARE_STACKS`.
    let mut moved = spare.unwrap_or_else(|| Box::new(Stacks { keys: Vec::new(), edges: Vec::new() }));
    STACKS.with(|stacks| std::mem::swap(&mut *stacks.borrow_mut(), &mut *moved));
    Box::into_raw(moved).expose_provenance()
}

/// Puts the stacks [`take_stacks`] moved off back, on the thread the job resumes on.
///
/// # Panics
///
/// If the thread's own stacks are not empty: a job left keys or edges when it ended.
#[inline(never)]
fn put_stacks(taken: usize) {
    // SAFETY: `taken` is the box `take_stacks` gave up for this job, which the driver puts back
    // once.
    let mut left = unsafe { Box::from_raw(std::ptr::with_exposed_provenance_mut::<Stacks>(taken)) };
    STACKS.with(|stacks| std::mem::swap(&mut *stacks.borrow_mut(), &mut *left));
    assert!(
        left.keys.is_empty() && left.edges.is_empty(),
        "a job resumed on a thread that still runs {} keys",
        left.keys.len()
    );
    // alloc: none once warm: `SPARE_STACKS` grows to its high-water mark.
    SPARE_STACKS.lock().push(left);
}

/// The switching job's [`INSIDE`]: 1 when it runs a query.
#[inline(never)]
fn take_inside() -> usize {
    usize::from(INSIDE.with(Cell::take))
}

#[inline(never)]
fn put_inside(taken: usize) {
    INSIDE.with(|it| it.set(taken != 0));
}

/// The switching job's [`UNREADABLE`]: 0 for none, else a box of it.
#[inline(never)]
fn take_unreadable() -> usize {
    // A run stopped on a file it could not read, switching out before its request fails with it.
    // alloc: only on that error path.
    UNREADABLE.with(Cell::take).map_or(0, |corrupt| Box::into_raw(Box::new(corrupt)).expose_provenance())
}

#[inline(never)]
fn put_unreadable(taken: usize) {
    let corrupt = (taken != 0).then(|| {
        // SAFETY: a nonzero `taken` is the box `take_unreadable` made for this job, which the
        // driver puts back once.
        *unsafe { Box::from_raw(std::ptr::with_exposed_provenance_mut::<Corrupt>(taken)) }
    });
    UNREADABLE.with(|it| it.set(corrupt));
}

/// One request's root and everything it runs inline, on one task: the keys of this thread's
/// stack from `base` on. A job the thread runs while a query's job waits carries its own.
#[derive(Clone, Copy)]
struct Run {
    base: usize,
    /// A run ahead of its asker ([`Cx::get_all`]): it never waits for a key another task runs
    /// but gives way at once, so the asker, which waits for it unseen by the cycle check, never
    /// waits on a chain that leads back to itself.
    ahead: bool,
}

impl Run {
    fn new() -> Run {
        Run { base: with_stacks(|stacks| stacks.borrow().keys.len()), ahead: false }
    }

    fn ahead() -> Run {
        Run { ahead: true, ..Run::new() }
    }

    /// Panics, naming the chain, if `key` is running in this run already.
    fn check(self, key: QueryKey) {
        with_stacks(|stacks| {
            let stacks = stacks.borrow();
            let running = &stacks.keys[self.base.min(stacks.keys.len())..];
            if running.contains(&key) {
                panic!(
                    "a query cycle: {} asks {key:?} while running it",
                    // alloc: the panic's message.
                    running.iter().map(|it| format!("{it:?}")).collect::<Vec<_>>().join(" -> ")
                );
            }
        });
    }
}

/// A key on this thread's stack while it lives.
struct Frame;

impl Frame {
    fn push(key: QueryKey) -> Frame {
        with_stacks(|stacks| stacks.borrow_mut().keys.push(key));
        Frame
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        _ = try_with_stacks(|stacks| {
            if let Ok(mut stacks) = stacks.try_borrow_mut() {
                stacks.keys.pop();
            }
        });
    }
}

/// The edges one execution read: this thread's edge stack from `base` on, given back (cut to
/// `base`) when it drops, whether the run made a memo, failed or unwound.
struct Recording {
    base: usize,
}

impl Recording {
    fn start() -> Recording {
        Recording { base: with_stacks(|stacks| stacks.borrow().edges.len()) }
    }

    fn with<R>(&self, f: impl FnOnce(&[EdgeRecord]) -> R) -> R {
        with_stacks(|stacks| f(&stacks.borrow().edges[self.base..]))
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        _ = try_with_stacks(|stacks| {
            if let Ok(mut stacks) = stacks.try_borrow_mut() {
                stacks.edges.truncate(self.base);
            }
        });
    }
}

/// A task's view of the database while it runs a query: everything read through it is an
/// edge of the result, pushed onto this thread's edge stack and copied once, with the result,
/// into its edge segment.
///
/// What it hands out lives as long as `'db`: the task the query runs in, which keeps whatever
/// it read from being freed.
pub struct Cx<'db> {
    db: &'db Database,
    task: &'db Guard,
    state: &'db ViewState,
    run: Run,
}

impl<'db> Cx<'db> {
    pub fn view(&self) -> ViewId {
        self.state.id
    }

    pub fn database(&self) -> &'db Database {
        self.db
    }

    fn record(&mut self, read: QueryKey, saw: Fingerprint) {
        with_stacks(|stacks| stacks.borrow_mut().edges.push(EdgeRecord::new(read, saw)));
    }

    /// Another query's value: reused, waited on if running, or run inline. Records the edge.
    pub fn get<Q: Query>(
        &mut self,
        key: &Q::Key,
    ) -> Result<<Q::Value as Persist>::Ref<'db>, Cancelled> {
        let (db, task, state) = (self.db, self.task, self.state);
        if db.kind_of::<Q>().is_none() {
            panic!("query {} (kind {}) is asked but was never registered", Q::NAME, Q::KIND.0);
        }
        let query = QueryKey { kind: Q::KIND, input: Q::input(key) };
        let memo = db.find::<Q>(task, state, query, self.run, key)?;
        self.found::<Q>(query, memo, key)
    }

    /// Records the edge on `query`, whose memo in the view is `memo`, and reads its value.
    fn found<Q: Query>(
        &mut self,
        query: QueryKey,
        memo: MemoId,
        key: &Q::Key,
    ) -> Result<<Q::Value as Persist>::Ref<'db>, Cancelled> {
        let (db, task, state) = (self.db, self.task, self.state);
        self.record(query, db.store.fingerprint(task, memo));
        let bytes = db.value_of(task, state, query, memo, self.run, key)?;
        Ok(read_value::<Q>(bytes))
    }

    /// Each key's value, as [`Cx::get`] of each in turn would give it, with the same edges in
    /// the same order (so verification and [`Database::why`] see what a loop of `get` makes).
    /// The keys the view has not resolved run at once: from the first, each other is a job of
    /// the plane's pool that resolves it ahead, while this task asks them in order and waits
    /// (holding no thread, a wait of the plane's) for one still running. A job ahead never waits: a key
    /// that needs one another task runs is left for this task to ask. The first key that is
    /// [`Cancelled`] cancels the call; what the jobs resolved stays as memos.
    ///
    /// When the view has resolved every key, it allocates only the list it returns.
    pub fn get_all<Q: Query>(
        &mut self,
        keys: &[Q::Key],
    ) -> Result<Vec<<Q::Value as Persist>::Ref<'db>>, Cancelled> {
        let (db, task, state) = (self.db, self.task, self.state);
        if db.kind_of::<Q>().is_none() {
            panic!("query {} (kind {}) is asked but was never registered", Q::NAME, Q::KIND.0);
        }
        // alloc: the values, one list a call.
        let mut values = Vec::with_capacity(keys.len());
        for (at, key) in keys.iter().enumerate() {
            let query = QueryKey { kind: Q::KIND, input: Q::input(key) };
            let Some(resolved) = state.results.get(task, &query) else {
                return self.get_rest::<Q>(&keys[at..], values);
            };
            let memo = resolved.memo;
            values.push(self.found::<Q>(query, memo, key)?);
        }
        Ok(values)
    }

    /// [`Cx::get_all`] from its first key the view has not resolved, `keys[0]`: the rest the
    /// view has not resolved either are resolved ahead by the pool while this task asks every
    /// key in order.
    fn get_rest<Q: Query>(
        &mut self,
        keys: &[Q::Key],
        mut values: Vec<<Q::Value as Persist>::Ref<'db>>,
    ) -> Result<Vec<<Q::Value as Persist>::Ref<'db>>, Cancelled> {
        let (db, task, state) = (self.db, self.task, self.state);
        let unresolved = |key: &Q::Key| state.results.get(task, &QueryKey { kind: Q::KIND, input: Q::input(key) }).is_none();
        if !keys[1..].iter().any(unresolved) {
            for key in keys {
                values.push(self.get::<Q>(key)?);
            }
            return Ok(values);
        }
        let near = blaze_graph::pool::group();
        blaze_graph::exec::shared().scope(|scope| {
            for key in keys[1..].iter().filter(|key| unresolved(*key)) {
                // A job that panics or gives way leaves its key unresolved: this task asks it
                // in its turn below, runs it itself and meets the same panic.
                scope.submit(near, move || db.ahead::<Q>(state, key), |_| {});
            }
            for key in keys {
                values.push(self.get::<Q>(key)?);
            }
            Ok(values)
        })
    }

    /// A file of the view: its content ID, mode and bytes; `None` for none. Records the edge.
    pub fn file(&mut self, path: &str) -> Result<Option<blaze_vfs::File<'db>>, Cancelled> {
        let key = Input::file_key(path);
        // alloc: the path, only when the database first sees the input.
        let value = self.db.input(self.task, self.state, key, || Input::File(path.to_owned()))?;
        self.record(key, value.fingerprint);
        Ok(match &value.data {
            InputData::File(Some((id, mode, bytes))) => {
                Some(blaze_vfs::File { id: *id, mode: *mode, bytes: &**bytes })
            }
            _ => None,
        })
    }

    /// The names in a directory of the view, sorted, each with whether it is a directory;
    /// `None` for none or a file. Records the edge.
    pub fn dir(&mut self, path: &str) -> Result<Option<&'db [(String, bool)]>, Cancelled> {
        let key = Input::dir_key(path);
        // alloc: the path, only when the database first sees the input.
        let value = self.db.input(self.task, self.state, key, || Input::Dir(path.to_owned()))?;
        self.record(key, value.fingerprint);
        Ok(match &value.data {
            InputData::Dir(Some(names)) => Some(self.db.names(self.state.view.root, path, names)),
            _ => None,
        })
    }

    /// The file at `path` under `root`, a root of the database's tree other than the view's
    /// (a registry package's source, the standard library's `library/`): its content ID, mode
    /// and bytes; `None` for none. A root never changes, so this is a pure function of
    /// `(root, path)`. Records the edge, keyed by both, whose fingerprint is the file's.
    pub fn file_at(&mut self, root: ContentId, path: &str) -> Result<Option<blaze_vfs::File<'db>>, Cancelled> {
        let key = Input::file_at_key(root, path);
        // alloc: the path, only when the database first sees the input.
        let value = self.db.input(self.task, self.state, key, || Input::FileAt { root, path: path.to_owned() })?;
        self.record(key, value.fingerprint);
        Ok(match &value.data {
            InputData::File(Some((id, mode, bytes))) => {
                Some(blaze_vfs::File { id: *id, mode: *mode, bytes: &**bytes })
            }
            _ => None,
        })
    }

    /// The names in a directory under `root`, as [`Cx::dir`] and [`Cx::file_at`]. Records
    /// the edge.
    pub fn dir_at(&mut self, root: ContentId, path: &str) -> Result<Option<&'db [(String, bool)]>, Cancelled> {
        let key = Input::dir_at_key(root, path);
        // alloc: the path, only when the database first sees the input.
        let value = self.db.input(self.task, self.state, key, || Input::DirAt { root, path: path.to_owned() })?;
        self.record(key, value.fingerprint);
        Ok(match &value.data {
            InputData::Dir(Some(names)) => Some(self.db.names(root, path, names)),
            _ => None,
        })
    }

    /// The hash of everything below `dir` in the view, `None` for no directory there; with
    /// `package`, a package's tree as cargo watches it (no name starting with `.`, no top
    /// `target`). One query per directory, so an edit reruns only the directories above it.
    /// Records one edge, on `dir`'s query.
    pub fn tree(&mut self, dir: &str, package: bool) -> Result<Option<[u8; 32]>, Cancelled> {
        // alloc: the key's path, once per call; its runs make their children's the same way.
        let key = subtree::SubtreeKey { dir: dir.trim_end_matches('/').to_owned(), package, top: true };
        self.get::<subtree::Subtree>(&key)
    }

    /// The view's configuration, as it was opened with ([`Database::open_with`]). Records the
    /// edge.
    pub fn config(&mut self) -> Result<&'db [u8], Cancelled> {
        let state = self.state;
        let key = Input::config_key();
        let value = self.db.input(self.task, state, key, || Input::Config)?;
        self.record(key, value.fingerprint);
        Ok(&*state.config)
    }

    /// The database's context of type `T`, as [`Database::provide`] gave it; `None` for none.
    /// Records no edge: a context changes no value.
    pub fn context<T: Send + Sync + 'static>(&self) -> Option<&'db T> {
        self.db.contexts.get(&TypeId::of::<T>()).and_then(|context| (**context).downcast_ref())
    }

    /// Records that the query read `fact`, whose fingerprint was `saw`: for facts computed
    /// without asking a query (an index the caller holds).
    pub fn read(&mut self, fact: QueryKey, saw: Fingerprint) {
        self.record(fact, saw);
    }
}

fn read_value<'db, Q: Query>(bytes: &'db [u8]) -> <Q::Value as Persist>::Ref<'db> {
    Q::Value::read(bytes)
        .unwrap_or_else(|error| panic!("the kept value of query {} does not read: {error}", Q::NAME))
}

/// The query was cancelled: a nested request needs what an outer one holds (#1301), or its
/// request ended. It is reposted, never answered with a partial value. The plane's own, as a
/// query's wait is one of the plane's waits.
pub use blaze_graph::Cancelled;

/// Why a query reran: from it, each edge whose fingerprint differed, back to an input.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)] // clone: an inspection's answer, owned by its caller
pub struct Why {
    /// The first is what `query` read that changed, the last an input.
    pub chain: Vec<Changed>,
    /// The input at the end, as the view names it.
    pub input: Option<Input>,
    pub query: QueryKey,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub struct Changed {
    pub key: QueryKey,
    pub now: Fingerprint,
    pub was: Fingerprint,
}

/// Why the database could not answer.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum DbError {
    Cancelled,
    /// The view is not open.
    NoView(ViewId),
    /// A query kind asked that no one registered.
    Unknown(QueryKind),
    /// The image has no database section of this layout.
    Refused(Refused),
    /// A group of the image failed its check: one the database reads whole at start (its
    /// inputs), or the one holding a file a run read. Never kept: the next request reads it
    /// again.
    Corrupt(Corrupt),
    /// A database section of a shape [`Database::write_image`] never writes.
    Invalid { section: SectionId, what: &'static str },
    /// The symbol table was not restored from this image (`intern::restore_symbols`), so a
    /// value naming a symbol by ID can't be read.
    Symbols,
}

impl DbError {
    /// Whether the database could not use an image, and starts empty instead (rebuilt from
    /// the store's results).
    pub fn is_image(&self) -> bool {
        matches!(
            self,
            DbError::Refused(_) | DbError::Corrupt(_) | DbError::Invalid { .. } | DbError::Symbols
        )
    }
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DbError::Cancelled => write!(f, "cancelled"),
            DbError::NoView(view) => write!(f, "view {view:?} is not open"),
            DbError::Unknown(kind) => write!(f, "no query of kind {} is registered", kind.0),
            DbError::Refused(refused) => write!(f, "the daemon image: {refused}"),
            DbError::Corrupt(corrupt) => write!(f, "the daemon image: {corrupt}"),
            DbError::Invalid { section, what } => write!(f, "the daemon image's {section}: {what}"),
            DbError::Symbols => write!(f, "the symbol table was not restored from the daemon image"),
        }
    }
}

/// How a database reads and where it keeps what it writes.
#[derive(Clone)]
pub struct Config {
    /// The files of every view; `None` for a database no query of which reads a file.
    // The file source, chosen once when the database is made: the VFS's reader, or a `MemTree`.
    // dyn: called once per input per view, beside hashing what it returns.
    pub tree: Option<Arc<dyn Tree>>,
    /// Its directory under the driver's root, for its cold packs (`<dir>/cold/<n>`). `None`
    /// keeps every value in memory. The image is the daemon's to write ([`Database::write_image`]).
    pub dir: Option<FileName>,
    /// Tags it asks with are `tags..tags + 2^32`.
    pub tags: u64,
}

impl Config {
    /// No tree and no directory: everything in memory.
    pub fn memory() -> Config {
        Config { tree: None, dir: None, tags: 0 }
    }
}

/// What a view resolved: every key's memo, every input it read, and why each key it ran
/// again did.
///
/// A result is one 64-byte line of row (the key, the memo's ID and its fingerprint) and at
/// most 16 bytes of places; finding one is a shard's table load, about 1.5 place loads and one
/// row read, with no hash computed. Verifying an edge to a key the view resolved reads that
/// line alone, never the memo's.
struct ViewState {
    id: ViewId,
    view: View,
    config: Arc<[u8]>,
    results: Map<Resolved>,
    inputs: Map<InputValue>,
    reruns: Map<Rerun>,
}

/// How many edges ahead [`Database::verify`] starts a lookup: the misses a core keeps in flight
/// (20) over the two each lookup takes.
const AHEAD: usize = 10;

/// From how many memos [`Database::verify`] prefetches: 2^16 memos are about 8 MB of memo and
/// result lines, past a core's L2, so lookups start to miss.
const PREFETCH_FROM: u32 = 1 << 16;

/// From how many memos [`Database::collect`] marks on the plane's workers: below it the walk
/// stays in cache and takes less than the jobs' start and join (about 10 µs).
const MARK_PARALLEL_FROM: u32 = 1 << 16;

/// Memos a marking worker takes at a time, from the roots or from what another handed off; it
/// hands off half its stack once that holds twice as many and a worker has none.
const MARK_BATCH: usize = 1024;

/// How [`Database::mark`] walks.
#[derive(Clone, Copy, Debug)]
enum Mark {
    Serial,
    Parallel { batch: usize },
}

/// A collection's marks, a bit a memo, in its frame.
type Marks<'f> = Bits<Vec<u64, &'f blaze_mem::frame::Frame>, MemoId>;
/// The input keys a collection keeps, in its frame.
type KeptInputs<'f> = hashbrown::HashSet<QueryKey, blaze_hash::FixedState, &'f blaze_mem::frame::Frame>;

/// What the workers of a parallel mark share.
struct Marking<'a> {
    roots: &'a [MemoId],
    /// The first root no worker has taken.
    next: std::sync::atomic::AtomicUsize,
    /// The open views, whose results are roots too: taken a shard of one view's results at a
    /// time, unit `u` being shard `u % shards` of view `u / shards`.
    views: &'a [&'a ViewState],
    shards: usize,
    /// The first unit no worker has taken.
    next_unit: std::sync::atomic::AtomicUsize,
    batch: usize,
    /// A bit a memo below its capacity; a memo past it (made while the walk ran) goes to `late`.
    reached: AtomicBitSlice<'a>,
    /// A bit for each memo a view's results name, as `reached`'s; one past it goes to `late_hot`.
    hot: AtomicBitSlice<'a>,
    late_hot: ShortLock<Vec<MemoId>>,
    /// Memos a worker handed off for others to walk.
    spill: ShortLock<Vec<MemoId>>,
    /// `spill`'s length, written under its lock, read with none by workers looking for work.
    spilled: std::sync::atomic::AtomicUsize,
    /// Set by a worker with nothing to walk; cleared by the one that hands off.
    hungry: std::sync::atomic::AtomicBool,
    /// Workers holding memos to walk, or looking for some to take.
    busy: std::sync::atomic::AtomicUsize,
    /// The input keys the walk read, each worker's once.
    inputs: ShortLock<Vec<QueryKey>>,
    late: ShortLock<Vec<MemoId>>,
}

/// Runs `job` once on each of the plane's workers, at most `most` times, and returns once
/// every run has ended; then the first panic of one goes on here. For a collection's passes,
/// which split their work among the runs themselves.
pub(crate) fn on_workers(most: usize, job: impl Fn() + Sync) {
    let pool = blaze_graph::exec::shared();
    // dyn: a panic's payload, as `catch_unwind` gives it
    let mut panic: ShortLock<Option<Box<dyn Any + Send>>> = ShortLock::new(None);
    pool.scope(|scope| {
        for _ in 0..pool.workers().min(most) {
            scope.submit(None, &job, |ran| {
                if let Err(payload) = ran {
                    panic.lock().get_or_insert(payload);
                }
            });
        }
    });
    if let Some(payload) = panic.get_mut().take() {
        std::panic::resume_unwind(payload);
    }
}

/// A key's order in 8 bytes: its kind, then its input's first 6 bytes. Inputs are content
/// hashes, so among 10M keys about one pair shares them.
fn key_prefix(key: &QueryKey) -> u64 {
    let mut bytes = [0u8; 8];
    bytes[..2].copy_from_slice(&key.kind.0.to_be_bytes());
    bytes[2..].copy_from_slice(&key.input.0[..6]);
    u64::from_be_bytes(bytes)
}

/// Sorts `keys` ascending, as `sort_unstable` would: a radix sort of their [`key_prefix`]es on
/// the plane's workers, then each run of keys sharing one (rare, and equal keys) by comparison.
fn sort_keys(keys: &mut [QueryKey]) {
    blaze_ids::radix::on(blaze_graph::exec::shared()).sort_by_key(keys, key_prefix);
    for run in keys.chunk_by_mut(|a, b| key_prefix(a) == key_prefix(b)) {
        if run.len() > 1 {
            run.sort_unstable();
        }
    }
}

/// Adds to `unlinked` the memos of `old` that `new` (`None`: the slot was removed) no longer
/// lists.
fn unlinked_from<A: std::alloc::Allocator>(unlinked: &mut Vec<MemoId, A>, old: &Slot, new: Option<&Slot>) {
    let still = new.map_or(&[][..], Slot::memos);
    unlinked.extend(old.memos().iter().copied().filter(|memo| !still.contains(memo)));
}

/// A worker counted in [`Marking::busy`] until dropped, unwinding too, so a panic in one
/// never leaves the others waiting for it.
struct Busy<'a>(&'a std::sync::atomic::AtomicUsize);

impl<'a> Busy<'a> {
    fn enter(busy: &'a std::sync::atomic::AtomicUsize) -> Busy<'a> {
        busy.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Busy(busy)
    }
}

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Marking<'_> {
    fn units(&self) -> usize {
        self.views.len() * self.shards
    }

    /// Moves the next batch of roots, else a shard of a view's results (marking each hot, or
    /// adding it to `late_hot` past the marks), else of what was handed off, onto `stack`, if
    /// any is left.
    fn take(
        &self,
        task: &Guard,
        stack: &mut Vec<MemoId, &blaze_mem::frame::Frame>,
        late_hot: &mut Vec<MemoId, &blaze_mem::frame::Frame>,
    ) {
        use std::sync::atomic::Ordering::{Relaxed, SeqCst};
        if self.next.load(SeqCst) < self.roots.len() {
            let at = self.next.fetch_add(self.batch, SeqCst);
            if at < self.roots.len() {
                stack.extend_from_slice(&self.roots[at..(at + self.batch).min(self.roots.len())]);
                return;
            }
        }
        if self.next_unit.load(SeqCst) < self.units() {
            let unit = self.next_unit.fetch_add(1, SeqCst);
            if unit < self.units() {
                self.views[unit / self.shards].results.for_each_in(task, unit % self.shards, |_, resolved| {
                    let memo = resolved.memo;
                    if (memo as usize) < self.hot.capacity() {
                        self.hot.insert_if_clear(memo as usize, Relaxed);
                    } else {
                        late_hot.push(memo);
                    }
                    stack.push(memo);
                });
                return;
            }
        }
        if self.spilled.load(SeqCst) == 0 {
            return;
        }
        let mut spill = self.spill.lock();
        let from = spill.len().saturating_sub(self.batch);
        stack.extend(spill.drain(from..));
        self.spilled.store(spill.len(), SeqCst);
    }

    /// Hands off the older half of `stack`, the memos nearer the roots, to the workers with none.
    fn give(&self, stack: &mut Vec<MemoId, &blaze_mem::frame::Frame>) {
        use std::sync::atomic::Ordering::{Relaxed, SeqCst};
        let half = stack.len() / 2;
        let mut spill = self.spill.lock();
        spill.extend_from_slice(&stack[..half]);
        self.spilled.store(spill.len(), SeqCst);
        drop(spill);
        stack.drain(..half);
        self.hungry.store(false, Relaxed);
    }

    /// Whether roots, views' results or handed-off memos are left to take.
    fn has_work(&self) -> bool {
        use std::sync::atomic::Ordering::SeqCst;
        self.next.load(SeqCst) < self.roots.len()
            || self.next_unit.load(SeqCst) < self.units()
            || self.spilled.load(SeqCst) > 0
    }

    /// Whether every memo the walk reaches was walked, read by a worker holding none. `busy`
    /// first: only a busy worker hands off, before it is counted out, so whoever then finds
    /// `busy` at 0 sees the hand-off. A worker that enters `busy` after that read takes what
    /// is left itself, and takes back what it hands off if no other worker is left to.
    fn done(&self) -> bool {
        use std::sync::atomic::Ordering::SeqCst;
        self.busy.load(SeqCst) == 0 && !self.has_work()
    }
}

/// A key's memo in a view, with the memo's fingerprint beside it.
#[derive(Clone, Copy)]
#[repr(C)]
struct Resolved {
    memo: MemoId,
    /// [`Fingerprint`], as two words so the entry is 8-byte aligned.
    fingerprint: [u64; 2],
}

impl Resolved {
    fn new(store: &Store, task: &Guard, memo: MemoId) -> Resolved {
        let Fingerprint(fingerprint) = store.fingerprint(task, memo);
        Resolved { memo, fingerprint: [fingerprint as u64, (fingerprint >> 64) as u64] }
    }

    fn fingerprint(&self) -> Fingerprint {
        Fingerprint(u128::from(self.fingerprint[0]) | u128::from(self.fingerprint[1]) << 64)
    }
}

const _: () = assert!(size_of::<table::Entry<Resolved>>() == 64);
const _: () = assert!(size_of::<table::Entry<u64>>() == 48);

impl ViewState {
    fn new(id: ViewId, view: View, config: Arc<[u8]>) -> ViewState {
        ViewState { id, view, config, results: Map::hot(), inputs: Map::new(), reruns: Map::new() }
    }

    fn sweep(&self) {
        self.results.sweep();
        self.inputs.sweep();
        self.reruns.sweep();
    }

    fn reclaim(&mut self) {
        self.results.reclaim();
        self.inputs.reclaim();
        self.reruns.reclaim();
    }

    /// Empties it for another view, its maps' tables kept: no reader can be in it.
    fn reset(&mut self) {
        self.results.reset();
        self.inputs.reset();
        self.reruns.reset();
    }

    /// Sizes its tables for as many results and inputs as `like` holds now: for an empty state
    /// about to serve a view of a tree like `like`'s, whose keys go in with no table grown.
    fn size_like(&mut self, like: &ViewState) {
        self.results.size_like(&like.results);
        self.inputs.size_like(&like.inputs);
    }

    /// Makes it the state of `view`, as `ViewState::new` would.
    fn reuse(&mut self, id: ViewId, view: View, config: &[u8]) {
        self.id = id;
        self.view = view;
        if *self.config != *config {
            // alloc: a configuration other than the one of the last view this state served.
            self.config = Arc::from(config);
        }
    }
}

/// Closed views' states, kept to be the next views opened: each held until no task can still
/// read it, then emptied, its maps' tables kept, so opening a view and resolving in it
/// allocates nothing once the database has had as many views open at once.
#[derive(Default)]
struct SpareViews {
    /// Oldest first.
    held: VecDeque<(Grace, Box<ViewState>)>,
    ready: Vec<Box<ViewState>>,
}

/// Emptied states kept; one more closed is freed.
const SPARE_VIEWS: usize = 8;

impl SpareViews {
    /// Empties those whose grace has passed, keeping up to [`SPARE_VIEWS`] and freeing the rest.
    fn sweep(&mut self) {
        while self.held.front().is_some_and(|(grace, _)| grace.passed()) {
            let Some((_, mut state)) = self.held.pop_front() else { break };
            if self.ready.len() < SPARE_VIEWS {
                state.reset();
                self.ready.push(state);
            }
        }
    }

    /// An emptied state, emptying first those whose grace has passed.
    fn take(&mut self) -> Option<Box<ViewState>> {
        self.sweep();
        self.ready.pop()
    }

    /// [`SpareViews::sweep`] of every state held, past its grace or not: for a database
    /// borrowed `&mut`, which no task reads.
    fn reclaim(&mut self) {
        while let Some((_, mut state)) = self.held.pop_front() {
            if self.ready.len() < SPARE_VIEWS {
                state.reset();
                self.ready.push(state);
            }
        }
    }
}

/// Why a key ran in a view: the newest memo it had then (its fingerprint) and that memo's
/// first edge that differed, `None` where the edge's query could not be run by key; no memo
/// at all for a first run.
#[derive(Clone, Copy)]
struct Rerun {
    previous: Option<(Fingerprint, Option<Changed>)>,
}

#[derive(Clone)]
struct InputValue {
    fingerprint: Fingerprint,
    data: InputData,
}

#[derive(Clone)]
enum InputData {
    File(Option<(ContentId, blaze_vfs::Mode, blaze_vfs::Bytes)>),
    /// A directory's names (`None` for no directory): hashed as the tree lent them, and kept
    /// only once a query of the view reads them ([`Database::names`]), so verifying an edge
    /// to a directory copies nothing.
    Dir(Option<OnceLock<Box<[(String, bool)]>>>),
    Config,
}

enum Verdict {
    Valid,
    Changed(Changed),
    Unknown,
}

/// A value a cold pack holds: its memo, pinned in the store until the pack is mapped, and the
/// word that named the value when it was copied, which [`Store::to_disk`] swaps only if the
/// memo's word still is.
struct ColdEntry {
    memo: MemoId,
    word: u64,
    at: usize,
    len: usize,
    id: ContentId,
}

enum Pending {
    ColdWrite { file: FileName, entries: Vec<ColdEntry> },
    ColdMap { file: FileName, entries: Vec<ColdEntry> },
    /// A cold pack written or mapped for a store a settle has dropped: its entries name that
    /// store's memos, whose values the image holds, so once the I/O is done it is removed.
    Stale { file: FileName, doing: &'static str },
    Remove { file: FileName },
}

impl Pending {
    /// What the database asked, and of which file: for the message of a failure.
    fn what(&self) -> (&'static str, &FileName) {
        match self {
            Pending::ColdWrite { file, .. } => ("write its cold pack", file),
            Pending::ColdMap { file, .. } => ("map its cold pack", file),
            Pending::Stale { file, doing } => (doing, file),
            Pending::Remove { file } => ("remove", file),
        }
    }

    /// The same I/O once a settle has dropped the store its cold entries name.
    fn settled(self) -> Pending {
        match self {
            Pending::ColdWrite { file, .. } => Pending::Stale { file, doing: "write its cold pack" },
            Pending::ColdMap { file, .. } => Pending::Stale { file, doing: "map its cold pack" },
            pending @ (Pending::Stale { .. } | Pending::Remove { .. }) => pending,
        }
    }
}

#[derive(Default)]
struct Io {
    next: u64,
    pending: FxMap<Tag, Pending>,
    /// Cold packs mapped, kept while values are read from them: as long as the database.
    packs: Vec<(FileName, Mapping)>,
    cold_files: u64,
}

impl Io {
    fn tag(&mut self, base: u64) -> Tag {
        let tag = Tag(base + (self.next & 0xffff_ffff));
        self.next += 1;
        tag
    }
}

/// The one database of a daemon: every view's results, the memo of each key.
///
/// Fields drop in order: the tables first (a slot holds spans of the store's key segments),
/// then the store, then the mappings its rows name.
pub struct Database {
    config: Config,
    kinds: Kinds,
    /// Every key's memos made or touched since start; the rest are in the image.
    slots: Map<Slot>,
    /// Keys read back from their slot's bytes, for runs no asker handed the key to (a
    /// dependency run again while a dependent is verified): each decoded once, while its slot
    /// lives, so a warm miss allocates no key.
    keys: Map<Box<AnyKey>>, // dyn: see `AnyKey`
    /// What each input key a memo read names, to read it again in another view.
    inputs: Map<Input>,
    /// By [`ViewId`]; a closed one is taken out.
    views: Column<ViewState>,
    /// One past the index of the view opened last; 0 before the first. A view opened is
    /// sized like it.
    newest: AtomicU32,
    /// What [`Database::provide`] gave, by type: set only with `&mut`, so read with no lock.
    contexts: FxMap<TypeId, Box<dyn Any + Send + Sync>>, // dyn: a context's type is its owner's
    /// One run of a key at a time, whatever view asked, on the plane's wait graph (#1301).
    flights: Flights<()>,
    /// Closed views, until every task that could hold them has ended, then kept for the next
    /// views opened.
    spare_views: ShortLock<SpareViews>,
    /// Bumped by each collection; a key given a memo since one started is kept by it whole.
    /// Each memo's cold row holds the generation its key last resolved to it in.
    generation: AtomicU32,
    store: Store,
    image: Option<ImageMap>,
    /// Every key of `image` has a slot ([`Database::slot_image_keys`] ran).
    image_slotted: std::sync::atomic::AtomicBool,
    io: ShortLock<Io>,
}

/// The database read from outside any query: a request's root. What it hands out borrows it,
/// and it keeps this thread in a task ([`blaze_reclaim::enter`]) so nothing read is freed
/// under it. Hold it for one request: what the database lets go of meanwhile is freed only
/// once it is dropped.
pub struct Pinned<'db> {
    db: &'db Database,
    task: Guard,
}

impl Pinned<'_> {
    /// A query's value in `view`.
    pub fn get<Q: Query>(
        &self,
        view: ViewId,
        key: &Q::Key,
    ) -> Result<<Q::Value as Persist>::Ref<'_>, DbError> {
        let (db, task) = (self.db, &self.task);
        assert!(
            !inside_a_query(),
            "Pinned::get of {} from inside a query: ask it through Cx::get, which records it",
            Q::NAME
        );
        if db.kind_of::<Q>().is_none() {
            return Err(DbError::Unknown(Q::KIND));
        }
        let state = db.view_state(task, view).ok_or(DbError::NoView(view))?;
        let query = QueryKey { kind: Q::KIND, input: Q::input(key) };
        let run = Run::new();
        _ = take_unreadable_corrupt();
        let memo = db.find::<Q>(task, state, query, run, key).map_err(stopped)?;
        let bytes = db.value_of(task, state, query, memo, run, key).map_err(stopped)?;
        Ok(read_value::<Q>(bytes))
    }
}

impl Database {
    fn blank(config: Config) -> Database {
        carry_with_fiber_jobs();
        let mut db = Database {
            config,
            kinds: Kinds::new(),
            slots: Map::hot(),
            keys: Map::new(),
            inputs: Map::new(),
            views: Column::new(),
            newest: AtomicU32::new(0),
            contexts: FxMap::default(),
            flights: Flights::new(),
            spare_views: ShortLock::default(),
            generation: AtomicU32::new(0),
            store: Store::new(),
            image: None,
            image_slotted: std::sync::atomic::AtomicBool::new(false),
            io: ShortLock::default(),
        };
        db.register::<subtree::Subtree>();
        db
    }

    /// A database with nothing in it, in memory, that reads no files: for queries of content
    /// only and tests. A daemon's is [`Database::new`].
    pub fn empty() -> Database {
        Database::blank(Config::memory())
    }

    pub fn new(config: Config) -> Database {
        Database::blank(config)
    }

    /// The database in a mapped image, used in place.
    pub fn from_image(image: Arc<Image<Mapping>>) -> Result<Database, DbError> {
        Database::from_image_with(Config::memory(), image)
    }

    /// The database in a mapped image, used in place, reading and writing as `config` says.
    /// The daemon restores the symbol table from the same image first
    /// (`intern::restore_symbols`), since values name symbols by their IDs in it; on any
    /// error the database starts empty.
    pub fn from_image_with(config: Config, image: Arc<Image<Mapping>>) -> Result<Database, DbError> {
        if intern::restored_from() != Some(image.hash()) {
            return Err(DbError::Symbols);
        }
        Database::open_image(config, image)
    }

    /// [`Database::from_image_with`] with this process's symbol table as it is: for an image
    /// this process wrote (a settle), whose symbols are the ones it holds.
    fn open_image(config: Config, image: Arc<Image<Mapping>>) -> Result<Database, DbError> {
        let image = ImageMap::open(image)?;
        let inputs = image.inputs().map_err(DbError::Corrupt)?;
        let db = Database::blank(config);
        {
            let task = blaze_reclaim::enter();
            for (key, input) in inputs {
                db.inputs.insert(&task, key, input);
            }
        }
        Ok(Database { image: Some(image), ..db })
    }

    /// The groups of the image found corrupt since the last call, each once: the requests that
    /// read their keys failed with them. For the daemon to report by section and group.
    pub fn take_corrupt(&self) -> Vec<Corrupt> {
        self.image.as_ref().map(ImageMap::take_corrupt).unwrap_or_default()
    }

    pub fn register<Q: Query>(&mut self) {
        assert!(!is_input(Q::KIND), "query {} takes kind {}, an input's", Q::NAME, Q::KIND.0);
        if let Some(info) = self.kinds.get(Q::KIND) {
            assert!(
                info.query == TypeId::of::<Q>(),
                "queries {} and {} both take kind {}",
                info.name,
                Q::NAME,
                Q::KIND.0
            );
            return;
        }
        self.kinds.insert(
            Q::KIND,
            KindInfo {
                name: Q::NAME,
                producer: Q::PRODUCER,
                schema: Q::Value::SCHEMA,
                code: self.store.code(Q::Value::SCHEMA, Q::PRODUCER),
                query: TypeId::of::<Q>(),
                run: run_query::<Q>,
                decode: decode_key::<Q>,
                tag: Q::TAG,
                share: Q::SHARE.then_some(Shared { name: Q::NAME, schema: Q::Value::SCHEMA }),
                symbols: <Q::Value as Persist>::symbols,
            },
        );
    }

    /// Gives every query `context`, which [`Cx::context`] hands it: what a query needs of the
    /// database's owner that is no input, such as a code generator's front end and backends or
    /// the daemon's memo of compiled templates. A context must not change any query's value,
    /// since no edge records it: what a value depends on is its key and the files, configuration
    /// and queries it reads. One of each type; a second replaces the first.
    pub fn provide<T: Send + Sync + 'static>(&mut self, context: T) {
        // alloc: once a context, when the database is set up.
        self.contexts.insert(TypeId::of::<T>(), Box::new(context));
    }

    pub fn open(&self, view: View) -> ViewId {
        self.open_with(view, &[])
    }

    /// Opens `view` with its configuration (target, profile, features asked for), what
    /// [`Cx::config`] reads.
    ///
    /// Its tables are sized like those of the view opened last, if still open: a view of the
    /// same tree then resolves and verifies with no table grown, allocating nothing.
    pub fn open_with(&self, view: View, config: &[u8]) -> ViewId {
        let spare = self.spare_views.lock().take();
        let index = self.views.push_boxed(|index| {
            let mut state = match spare {
                Some(mut state) => {
                    state.reuse(Idx::from_raw(index), view, config);
                    state
                }
                // alloc: a view's state, until as many views were closed as are open at once.
                None => Box::new(ViewState::new(Idx::from_raw(index), view, Arc::from(config))),
            };
            let task = blaze_reclaim::enter();
            if let Some(newest) = self.newest.load(Ordering::Relaxed).checked_sub(1)
                && let Some(like) = self.views.get(&task, newest)
            {
                state.size_like(like);
            }
            state
        });
        self.newest.fetch_max(index + 1, Ordering::Relaxed);
        Idx::from_raw(index)
    }

    pub fn close(&self, view: ViewId) {
        if let Some(state) = self.views.take(view.raw()) {
            // The take unlinked it: a grace begun now covers every task that could read it.
            self.spare_views.lock().held.push_back((Grace::start(), state));
        }
    }

    /// Emptied view states ready for the next views opened.
    #[cfg(test)]
    fn ready_views(&self) -> usize {
        let mut spare = self.spare_views.lock();
        spare.sweep();
        spare.ready.len()
    }

    /// The database for one request from outside any query: see [`Pinned`].
    pub fn pin(&self) -> Pinned<'_> {
        Pinned { db: self, task: blaze_reclaim::enter() }
    }

    pub fn memo(&self, key: &QueryKey) -> Option<Memo> {
        let task = blaze_reclaim::enter();
        let &memo = self.slot(&task, key).ok()??.memos().last()?;
        let hot = self.store.hot(&task, memo);
        let (schema, producer) = self.store.parts(hot.code());
        Some(Memo {
            key: *key,
            fingerprint: hot.fingerprint(),
            // alloc: an inspection's answer, owned by its caller.
            edges: self.store.edges(&task, memo).iter().map(EdgeRecord::edge).collect(),
            schema,
            producer,
            value: self.store.stored(&task, memo),
        })
    }

    pub fn why(&self, view: ViewId, key: &QueryKey) -> Option<Why> {
        let task = blaze_reclaim::enter();
        let state = self.view_state(&task, view)?;
        let rerun = state.reruns.get(&task, key)?;
        // alloc: an inspection's answer, owned by its caller.
        let mut why = Why { query: *key, chain: Vec::new(), input: None };
        let Some((_, Some(mut changed))) = rerun.previous else { return Some(why) };
        let mut seen = FxSet::default();
        loop {
            why.chain.push(changed);
            if is_input(changed.key.kind) {
                why.input = self.inputs.get(&task, &changed.key).cloned();
                break;
            }
            if !seen.insert(changed.key) {
                break;
            }
            match self.why_changed(&task, state, changed) {
                Some(next) => changed = next,
                None => break,
            }
        }
        Some(why)
    }

    /// Drops what no open view and no key of `kept` reaches; moves cold values to the store.
    pub fn collect(&self, kept: &[QueryKey], effects: &mut Vec<Effect>) {
        let task = blaze_reclaim::enter();
        let started = self.generation.fetch_add(1, Ordering::SeqCst);
        assert!(started < u32::MAX, "2^32 collections of one database");
        blaze_mem::frame::task(blaze_mem::Tag::Query, |frame| {
            // Before the views are walked: a memo unlinked earlier is checked against what
            // they hold now (see `memo`'s comment).
            let mut dying: Vec<MemoId, _> = Vec::new_in(frame);
            self.store.to_check(&mut dying);
            // A bit a memo each.
            let marks = || {
                let mut marks = Bits::<_, MemoId>::from_words(Vec::<u64, _>::new_in(frame));
                marks.grow(self.store.end() as usize);
                marks
            };
            let (mut reached, mut hot) = (marks(), marks());
            let mut inputs_kept = hashbrown::HashSet::with_hasher_in(blaze_hash::FixedState, frame);
            let mut work: Vec<MemoId, _> = Vec::new_in(frame);
            let mut views: Vec<&ViewState, _> = Vec::new_in(frame);
            self.views.for_each(&task, |state| {
                views.push(state);
                state.inputs.for_each(&task, |key, _| {
                    inputs_kept.insert(*key);
                });
            });
            for key in kept {
                if is_input(key.kind) {
                    inputs_kept.insert(*key);
                } else if let Ok(Some(slot)) = self.slot(&task, key) {
                    work.extend_from_slice(slot.memos());
                }
            }
            let how = if self.store.end() >= MARK_PARALLEL_FROM {
                Mark::Parallel { batch: MARK_BATCH }
            } else {
                Mark::Serial
            };
            self.mark(&task, frame, &views, &mut work, &mut reached, &mut hot, &mut inputs_kept, how);
            self.store.checked(&dying, |memo| hot.contains(memo));

            let marks = reached.as_slice();
            if matches!(how, Mark::Parallel { .. }) {
                // alloc: a parallel collection's list of what young keys read, once a collection
                let young_inputs: ShortLock<Vec<QueryKey>> = ShortLock::new(Vec::new());
                self.slots.edit_all_parallel(
                    || (blaze_mem::scratch::lend_u32s(), blaze_mem::scratch::lend_u32s()),
                    |(live, _), task, key, slot| {
                        self.sweep_slot(task, started, &marks, key, slot, &mut **live, |input| {
                            young_inputs.lock().push(input);
                        })
                    },
                    |(_, unlinked), old, new| unlinked_from(&mut **unlinked, old, new),
                    |(_, unlinked)| self.store.unlinked(&unlinked),
                );
                inputs_kept.extend(young_inputs.lock().drain(..));
            } else {
                let mut unlinked: Vec<MemoId, _> = Vec::new_in(frame);
                // Reused for every slot.
                let mut live: Vec<MemoId, _> = Vec::new_in(frame);
                self.slots.edit_all_with(
                    &task,
                    |key, slot| {
                        self.sweep_slot(&task, started, &marks, key, slot, &mut live, |input| {
                            inputs_kept.insert(input);
                        })
                    },
                    |old, new| unlinked_from(&mut unlinked, old, new),
                );
                self.store.unlinked(&unlinked);
            }
            // Not once a group is found corrupt: an empty slot would let its keys be computed
            // again, where each read of them must fail naming the group.
            if let Some(image) = &self.image
                && !self.image_slotted.load(std::sync::atomic::Ordering::Acquire)
                && image.first_corrupt().is_none()
            {
                self.slot_image_keys(&task, image);
                self.image_slotted.store(true, std::sync::atomic::Ordering::Release);
            }
            self.inputs.retain(&task, |key, _| inputs_kept.contains(key));
            self.keys.retain(&task, |key, _| self.slots.get(&task, key).is_some());

            if self.config.dir.is_some() {
                let mut cold: Vec<(QueryKey, Fingerprint, MemoId), _> = Vec::new_in(frame);
                cold.extend(
                    reached
                        .iter()
                        .filter(|&memo| !hot.contains(memo) && self.store.stored(&task, memo) == Stored::Memory)
                        .map(|memo| (self.store.key(&task, memo), self.store.fingerprint(&task, memo), memo)),
                );
                cold.sort_unstable();
                self.write_cold(&task, cold.into_iter().map(|(_, _, memo)| memo), effects);
            }
            self.collect_retired(&task);
        });
    }

    /// Marks in `hot` every memo the results of `views` name, and in `reached` every memo of
    /// those and of `work` and every memo their edges reach (a memo of the key an edge read
    /// with the fingerprint it saw), and keeps in `inputs` each input key an edge read; `work`
    /// is left empty. Both ways reach the same sets, `hot` the results' memos and `reached`
    /// their closure with `work`'s under the edges: a memo is walked once, by whoever sets its
    /// bit.
    #[allow(clippy::too_many_arguments)]
    fn mark<'f>(
        &self,
        task: &Guard,
        frame: &'f blaze_mem::frame::Frame,
        views: &[&ViewState],
        work: &mut Vec<MemoId, &'f blaze_mem::frame::Frame>,
        reached: &mut Marks<'f>,
        hot: &mut Marks<'f>,
        inputs: &mut KeptInputs<'f>,
        how: Mark,
    ) {
        match how {
            Mark::Parallel { batch } => self.mark_parallel(frame, views, work, reached, hot, inputs, batch),
            Mark::Serial => {
                for state in views {
                    state.results.for_each(task, |_, resolved| {
                        hot.insert_growing(resolved.memo);
                        work.push(resolved.memo);
                    });
                }
            }
        }
        while let Some(memo) = work.pop() {
            if !reached.insert_growing(memo) {
                continue;
            }
            self.mark_edges(task, memo, |key| {
                inputs.insert(key);
            }, |it| work.push(it));
        }
    }

    /// `memo`'s edges, for marking: each input key it read to `input`, and each memo of a key it
    /// read with the fingerprint it saw to `reach`.
    #[inline]
    fn mark_edges(&self, task: &Guard, memo: MemoId, mut input: impl FnMut(QueryKey), mut reach: impl FnMut(MemoId)) {
        for edge in self.store.edges(task, memo) {
            let read = edge.read();
            if is_input(read.kind) {
                input(read);
                continue;
            }
            // A key whose image memos are corrupt has none in memory to mark: the slot read
            // noted it for `take_corrupt`, and every request that reads the key fails with it.
            if let Ok(Some(slot)) = self.slot(task, &read) {
                let saw = edge.saw();
                for &it in slot.memos() {
                    if self.store.fingerprint(task, it) == saw {
                        reach(it);
                    }
                }
            }
        }
    }

    /// [`Database::mark`] on every worker of the plane: each takes `batch` roots, or a shard
    /// of a view's results, at a time and walks them depth first, setting each memo's bit with
    /// one `fetch_or` (after a load finds it clear), so a memo is walked by one worker. A worker
    /// whose stack outgrows two batches while another has none hands off half. Leaves in
    /// `work` the memos past `reached`'s capacity it met, made while it ran, for the serial
    /// walk to finish; `hot` grows to hold those the results name.
    #[allow(clippy::too_many_arguments)]
    fn mark_parallel<'f>(
        &self,
        frame: &'f blaze_mem::frame::Frame,
        views: &[&ViewState],
        work: &mut Vec<MemoId, &'f blaze_mem::frame::Frame>,
        reached: &mut Marks<'f>,
        hot: &mut Marks<'f>,
        inputs: &mut KeptInputs<'f>,
        batch: usize,
    ) {
        let atomic = |marks: &Marks<'f>| {
            let mut words: Vec<std::sync::atomic::AtomicU64, _> = Vec::with_capacity_in(marks.words().len(), frame);
            words.extend(marks.words().iter().map(|&word| std::sync::atomic::AtomicU64::new(word)));
            words
        };
        let plain = |mut words: Vec<std::sync::atomic::AtomicU64, &'f blaze_mem::frame::Frame>| {
            let mut marked = Vec::with_capacity_in(words.len(), frame);
            marked.extend(words.iter_mut().map(|word| *word.get_mut()));
            Bits::from_words(marked)
        };
        let (reached_words, hot_words) = (atomic(reached), atomic(hot));
        let mut marking = Marking {
            roots: work,
            next: std::sync::atomic::AtomicUsize::new(0),
            views,
            shards: views.first().map_or(0, |state| state.results.shard_count()),
            next_unit: std::sync::atomic::AtomicUsize::new(0),
            batch: batch.max(1),
            reached: AtomicBitSlice::from_words(&reached_words),
            hot: AtomicBitSlice::from_words(&hot_words),
            // alloc: here and below, a parallel collection's shared lists, once each a collection
            late_hot: ShortLock::new(Vec::new()),
            spill: ShortLock::new(Vec::new()), // alloc: see above
            spilled: std::sync::atomic::AtomicUsize::new(0),
            hungry: std::sync::atomic::AtomicBool::new(false),
            busy: std::sync::atomic::AtomicUsize::new(0),
            inputs: ShortLock::new(Vec::new()), // alloc: see above
            late: ShortLock::new(Vec::new()),   // alloc: see above
        };
        on_workers(usize::MAX, || self.mark_job(&marking));
        let found = std::mem::take(marking.inputs.get_mut());
        let late = std::mem::take(marking.late.get_mut());
        let late_hot = std::mem::take(marking.late_hot.get_mut());
        drop(marking);
        *reached = plain(reached_words);
        *hot = plain(hot_words);
        for &memo in &late_hot {
            hot.insert_growing(memo);
        }
        inputs.extend(found);
        work.clear();
        work.extend_from_slice(&late);
    }

    /// One worker of [`Database::mark_parallel`]: walks what it takes until nothing is left to
    /// take and no worker holds any.
    fn mark_job(&self, marking: &Marking<'_>) {
        use std::sync::atomic::Ordering::Relaxed;
        let task = blaze_reclaim::enter();
        blaze_mem::frame::task(blaze_mem::Tag::Query, |frame| {
            let mut stack: Vec<MemoId, _> = Vec::new_in(frame);
            let mut late: Vec<MemoId, _> = Vec::new_in(frame);
            let mut late_hot: Vec<MemoId, _> = Vec::new_in(frame);
            let mut inputs: KeptInputs<'_> = hashbrown::HashSet::with_hasher_in(blaze_hash::FixedState, frame);
            let reached = marking.reached;
            let mut idle = blaze_graph::spin::SpinWait::new();
            loop {
                if !marking.has_work() {
                    if marking.done() {
                        break;
                    }
                    marking.hungry.store(true, Relaxed);
                    if !idle.spin() {
                        std::thread::yield_now();
                    }
                    continue;
                }
                idle = blaze_graph::spin::SpinWait::new();
                let busy = Busy::enter(&marking.busy);
                marking.take(&task, &mut stack, &mut late_hot);
                while let Some(memo) = stack.pop() {
                    if memo as usize >= reached.capacity() {
                        late.push(memo);
                        continue;
                    }
                    if !reached.insert_if_clear(memo as usize, Relaxed) {
                        continue;
                    }
                    self.mark_edges(&task, memo, |key| {
                        inputs.insert(key);
                    }, |it| stack.push(it));
                    if stack.len() >= 2 * marking.batch && marking.hungry.load(Relaxed) {
                        marking.give(&mut stack);
                    }
                }
                drop(busy);
            }
            if !inputs.is_empty() {
                let mut all = marking.inputs.lock();
                all.reserve(inputs.len());
                all.extend(inputs.iter().copied());
            }
            if !late.is_empty() {
                marking.late.lock().extend_from_slice(&late);
            }
            if !late_hot.is_empty() {
                marking.late_hot.lock().extend_from_slice(&late_hot);
            }
        });
    }

    /// What a collection does to `key`'s slot, from what its walk `reached`. A key given a memo
    /// since the collection began (`started`) is kept whole, and each input key its memos read
    /// goes to `input`: the walk may have passed its views before it resolved. A key whose
    /// memos were all reached is kept either way, their inputs kept by the walk, so only a key
    /// with one unreached reads its memos' cold rows. `live` is scratch.
    #[allow(clippy::too_many_arguments)]
    fn sweep_slot<A: std::alloc::Allocator>(
        &self,
        task: &Guard,
        started: u32,
        reached: &BitSlice<'_, MemoId>,
        key: &QueryKey,
        slot: &Slot,
        live: &mut Vec<MemoId, A>,
        mut input: impl FnMut(QueryKey),
    ) -> Edit<Slot> {
        let memos = slot.memos();
        if memos.iter().all(|&memo| reached.contains(memo)) {
            return Edit::Keep;
        }
        if memos.iter().any(|&memo| self.store.born(task, memo) > started) {
            for &memo in memos {
                for edge in self.store.edges(task, memo) {
                    if is_input(edge.read().kind) {
                        input(edge.read());
                    }
                }
            }
            return Edit::Keep;
        }
        live.clear();
        live.extend(memos.iter().copied().filter(|&memo| reached.contains(memo)));
        if !live.is_empty() {
            Edit::Replace(self.store.slot(slot.share_key(), &live[..]))
        } else if self.image.as_ref().is_some_and(|image| !matches!(image.find(key), Ok(None))) {
            // Left empty, not removed, so the image's memos of the key stay dropped.
            Edit::Replace(self.store.slot(slot.share_key(), &[]))
        } else {
            Edit::Remove
        }
    }

    /// Gives every key of the image a slot, empty if it has none: an image memo no slot took
    /// in by now was reached by nothing, and is dropped. A sweep leaves an image key's slot
    /// empty and never removes it, so once every key has one this has nothing left to do, and
    /// [`Database::collect`] runs it once an image. A large table is split among the plane's
    /// workers by groups, each key named by one slot of it.
    fn slot_image_keys(&self, task: &Guard, image: &ImageMap) {
        let len = image.table_len();
        if len < MARK_PARALLEL_FROM as usize {
            image.for_each_key(|key| {
                self.slots.get_or_insert_with(task, key, Slot::empty);
            });
            return;
        }
        let chunk = image.slots_per_group().max(1) * 16;
        let next = std::sync::atomic::AtomicUsize::new(0);
        on_workers(len.div_ceil(chunk), || {
            let task = blaze_reclaim::enter();
            loop {
                let from = next.fetch_add(chunk, std::sync::atomic::Ordering::Relaxed);
                if from >= len {
                    break;
                }
                image.for_each_key_in(from..from + chunk, |key| {
                    self.slots.get_or_insert_with(&task, key, Slot::empty);
                });
            }
        });
    }

    /// How many closed views and values let go of are still held, after freeing those past
    /// their grace.
    #[cfg(test)]
    fn held(&self) -> usize {
        let mut spare = self.spare_views.lock();
        spare.sweep();
        spare.held.len() + self.store.held()
    }

    /// Frees what the database's tables and store let go of whose grace has passed: the
    /// tables' rows first, as a slot's row holds spans of the store's segments.
    fn collect_retired(&self, task: &Guard) {
        self.spare_views.lock().sweep();
        self.slots.sweep();
        self.keys.sweep();
        self.inputs.sweep();
        self.views.for_each(task, ViewState::sweep);
        self.store.sweep();
    }

    /// Writes the whole database into the daemon's image: its five sections into `builder`,
    /// and each symbol a value names into `live`, so the symbol table the daemon writes last
    /// (`intern::write_symbols`) keeps it. The daemon writes the image as one `Effect::Write`,
    /// maps it and hands it to [`Database::settle`]. Every kind with a memo must be registered,
    /// as its values' symbols are found through it: [`DbError::Unknown`] names one that is
    /// not, before anything is written.
    pub fn write_image(&self, builder: &mut Builder, live: &mut intern::Live) -> Result<(), DbError> {
        let task = blaze_reclaim::enter();
        let entries = self.image_entries(&task).map_err(DbError::Corrupt)?;
        let unknown = entries.keys.iter().find(|entry| {
            !entry.memos.is_empty() && !is_input(entry.key.kind) && self.kinds.get(entry.key.kind).is_none()
        });
        if let Some(entry) = unknown {
            return Err(DbError::Unknown(entry.key.kind));
        }
        // alloc: one list of a value's symbols, reused for every value of the image.
        let mut ids = Vec::new();
        image::write(&task, &self.store, &entries, &self.image_inputs(&task), builder, |key, bytes| {
            let Some(info) = self.kinds.get(key.kind) else { return };
            ids.clear();
            if let Err(error) = (info.symbols)(bytes, &mut ids) {
                panic!("a value of query {} does not read back as its schema: {error}", info.name)
            }
            for &id in &ids {
                live.mark_id(id);
            }
        });
        Ok(())
    }

    pub fn complete(&self, completion: Completion, effects: &mut Vec<Effect>) {
        let mut io = self.io.lock();
        let Some(pending) = io.pending.remove(&completion.tag) else {
            panic!("the database did not ask for {:?}, which completed as {:?}", completion.tag, completion.done)
        };
        match (pending, completion.done) {
            (Pending::ColdWrite { file, entries }, Done::Written) => {
                let tag = io.tag(self.config.tags);
                effects.push(Effect::Map { file: file.clone(), tag }); // clone: a name, once an I/O
                io.pending.insert(tag, Pending::ColdMap { file, entries });
            }
            (Pending::ColdMap { file, entries }, Done::Mapped(mapping)) => {
                drop(io);
                let task = blaze_reclaim::enter();
                for entry in entries {
                    let bytes = mapping.bytes().get(entry.at..entry.at + entry.len);
                    // The value still in memory is compared byte for byte (a `memcmp`, not a
                    // second hash); one swapped out since is checked by its content ID.
                    let wrote = self.store.in_memory(&task, entry.memo).filter(|(word, _)| *word == entry.word);
                    let holds = |bytes: &&[u8]| match wrote {
                        Some((_, wrote)) => *bytes == wrote,
                        None => value_id(bytes) == entry.id,
                    };
                    let Some(bytes) = bytes.filter(holds) else {
                        panic!(
                            "the database's cold pack {file} does not hold at byte {} the value it wrote",
                            entry.at
                        );
                    };
                    // SAFETY: `mapping` goes into `io.packs` below, which the database keeps
                    // while it lives (settle drops the packs only with the store).
                    unsafe { self.store.to_disk(&task, entry.memo, entry.word, bytes, entry.id) };
                    self.store.unpin(entry.memo);
                }
                self.io.lock().packs.push((file, mapping));
            }
            (Pending::Stale { file, .. }, Done::Written | Done::Mapped(_)) => {
                let tag = io.tag(self.config.tags);
                effects.push(Effect::Remove { file: file.clone(), tag }); // clone: a name, once an I/O
                io.pending.insert(tag, Pending::Remove { file });
            }
            (Pending::Remove { .. }, Done::Removed) => {}
            (pending, Done::Failed(error)) => {
                let (doing, file) = pending.what();
                panic!("the database could not {doing} {file}: {error}")
            }
            (pending, done) => {
                let (doing, file) = pending.what();
                panic!("the database asked to {doing} {file} and was told {done:?}")
            }
        }
    }
}

impl Database {
    /// Whether a completion is of an effect this asked for.
    pub fn owns(&self, tag: Tag) -> bool {
        tag.0.wrapping_sub(self.config.tags) < 1 << 32
    }

    /// The settle swap (#1299): once the image [`Database::write_image`] wrote is on disk and
    /// mapped, the database becomes the image's, used in place, and every arena of the cold
    /// build is dropped. Open views stay open with what they resolved; queries stay
    /// registered. A cold pack still being written or mapped is removed once that is done:
    /// it names the dropped store's memos. On an error the database is left as it was.
    pub fn settle(&mut self, image: Arc<Image<Mapping>>, effects: &mut Vec<Effect>) -> Result<(), DbError> {
        // Settle runs once per cold build, and copies each view's inputs (`Arc`s) and reruns
        // into the new database.
        // clone: once a settle, as is every clone and box below.
        let mut new = Database::open_image(self.config.clone(), image)?;
        std::mem::swap(&mut new.kinds, &mut self.kinds);
        std::mem::swap(&mut new.contexts, &mut self.contexts);
        new.newest = AtomicU32::new(self.newest.load(Ordering::Relaxed));
        let store = &new.store;
        new.kinds.for_each_mut(|info| info.code = store.code(info.schema, info.producer));
        let (next, views) = self.views.take_all();
        let task = blaze_reclaim::enter();
        let views = views
            .into_iter()
            .map(|old| {
                let fresh = ViewState::new(old.id, old.view, old.config.clone()); // clone: see above
                old.inputs.for_each(&task, |key, value| {
                    fresh.inputs.insert(&task, *key, value.clone()); // clone: see above
                });
                old.reruns.for_each(&task, |key, rerun| {
                    fresh.reruns.insert(&task, *key, *rerun);
                });
                old.results.for_each(&task, |key, resolved| {
                    let same = new.slot(&task, key).ok().flatten().and_then(|slot| {
                        slot.memos().iter().copied().find(|&it| new.store.same(&task, it, &self.store, resolved.memo))
                    });
                    if let Some(same) = same {
                        fresh.results.insert(&task, *key, Resolved { memo: same, ..*resolved });
                    }
                });
                Box::new(fresh) // alloc: see above
            })
            .collect(); // alloc: see above
        drop(task);
        new.views.restore(next, views, |state| state.id.raw());
        let old_io = std::mem::take(self.io.get_mut());
        let new_io = new.io.get_mut();
        new_io.next = old_io.next;
        new_io.cold_files = old_io.cold_files;
        // alloc: see above
        new_io.pending.extend(old_io.pending.into_iter().map(|(tag, pending)| (tag, pending.settled())));
        for (file, _) in &old_io.packs {
            let tag = new_io.tag(new.config.tags);
            effects.push(Effect::Remove { file: file.clone(), tag }); // clone: see above
            new_io.pending.insert(tag, Pending::Remove { file: file.clone() }); // clone: see above
        }
        *self = new;
        // The old store, whose rows named the packs' bytes, is gone with the old database.
        drop(old_io.packs);
        Ok(())
    }

    /// Frees everything let go of, past its grace or not: with `&mut`, nothing reads it.
    pub fn reclaim(&mut self) {
        self.spare_views.get_mut().reclaim();
        self.slots.reclaim();
        self.keys.reclaim();
        self.inputs.reclaim();
        // alloc: one bitset a reclaim, 1 bit a memo.
        let mut held = BitSet::<MemoId>::new(self.store.end() as usize);
        {
            let task = blaze_reclaim::enter();
            self.views.for_each(&task, |state| {
                state.results.for_each(&task, |_, resolved| {
                    held.insert_growing(resolved.memo);
                });
            });
        }
        self.views.values_mut().for_each(ViewState::reclaim);
        self.store.reclaim(|memo| held.contains(memo));
    }

    /// Drops the values of `key`'s memos from memory, keeping their fingerprints and edges: a
    /// value already kept elsewhere (compiled code in the store, #806). Asked again, it is made
    /// again and must have its fingerprint. A reader still holding one keeps it until its task
    /// ends. Returns how many were dropped.
    pub fn evict(&self, key: &QueryKey) -> usize {
        let task = blaze_reclaim::enter();
        let Ok(Some(slot)) = self.slot(&task, key) else { return 0 };
        slot.memos().iter().filter(|&&memo| self.store.evict(&task, memo)).count()
    }

    fn dir(&self, doing: &str) -> &FileName {
        self.config
            .dir
            .as_ref()
            .unwrap_or_else(|| panic!("the database has no directory to {doing} in: make it with Database::new"))
    }

    // dyn: see `Config::tree`.
    fn tree(&self) -> &dyn Tree {
        self.config.tree.as_deref().unwrap_or_else(|| {
            panic!("the database has no file tree to read a view's files from: make it with Database::new")
        })
    }

    /// The names of the directory at `path` under `root`, as `names` keeps them for a view:
    /// listed from the tree again the first time a query of the view reads them.
    fn names<'g>(&self, root: ContentId, path: &str, names: &'g OnceLock<Box<[(String, bool)]>>) -> &'g [(String, bool)] {
        names.get_or_init(|| {
            // alloc: a directory's names, once per directory per view whose queries read them.
            let names = self.tree().list(root, path);
            names.unwrap_or_else(|| panic!("the tree lost directory {path:?} of root {root:x} while a view read it")).into_boxed_slice()
        })
    }

    fn view_state<'g>(&'g self, task: &'g Guard, view: ViewId) -> Option<&'g ViewState> {
        self.views.get(task, view.raw())
    }

    /// `Q`'s kind, if registered; a panic if another query took its kind.
    fn kind_of<Q: Query>(&self) -> Option<&KindInfo> {
        let info = self.kinds.get(Q::KIND)?;
        if info.query != TypeId::of::<Q>() {
            panic!("query {} is asked, but kind {} is {}'s", Q::NAME, Q::KIND.0, info.name);
        }
        Some(info)
    }

    /// `query`'s memo in a view: a hit is one lookup in the view's results. Otherwise the
    /// key's bytes are kept (once) and the memo resolved.
    fn find<Q: Query>(
        &self,
        task: &Guard,
        state: &ViewState,
        query: QueryKey,
        run: Run,
        key: &Q::Key,
    ) -> Result<MemoId, Cancelled> {
        if let Some(resolved) = state.results.get(task, &query) {
            return Ok(resolved.memo);
        }
        self.learn_key::<Q>(task, query, key);
        Ok(self
            .resolve(task, state, query, run, Some(key))?
            .expect("a registered query asked with its key always resolves"))
    }

    /// A key's memos and key bytes, read in from the image the first time. A key whose memos
    /// are in a corrupt group is [`Corrupt`], every time it is read: never an empty slot that
    /// would compute it again.
    fn slot<'g>(&'g self, task: &'g Guard, key: &QueryKey) -> Result<Option<&'g Slot>, Corrupt> {
        if let Some(slot) = self.slots.get(task, key) {
            return Ok(Some(slot));
        }
        let Some(image) = self.image.as_ref() else { return Ok(None) };
        let read = || -> Result<Option<_>, Corrupt> {
            let Some(found) = image.find(key)? else { return Ok(None) };
            // alloc: once a key read in from the image, as is the slot's list of its memos.
            let mut memos = Vec::with_capacity(found.len());
            for index in found.start..found.end {
                memos.push(image.memo(index)?);
            }
            Ok(Some((memos, image.key_bytes(found.start)?)))
        };
        let (memos, key_bytes) = match read() {
            Ok(Some(read)) => read,
            Ok(None) => return Ok(None),
            Err(corrupt) => {
                image.note(corrupt);
                return Err(corrupt);
            }
        };
        let mut made = Vec::with_capacity(memos.len());
        let slot = self.slots.get_or_insert_with(task, *key, || {
            for memo in &memos {
                let code = self.store.code(memo.schema, memo.producer);
                // SAFETY: the image's mapping is kept by the database as long as its store.
                made.push(unsafe {
                    self.store.make_mapped(memo.key, memo.fingerprint, code, memo.edges, memo.value)
                });
            }
            // SAFETY: as above.
            let key_bytes = key_bytes.map(|bytes| unsafe { Span::mapped(bytes) });
            self.store.slot(key_bytes, &made)
        });
        if slot.memos() != made.as_slice() {
            for memo in made {
                self.store.discard(memo);
            }
        }
        Ok(Some(slot))
    }

    /// Keeps `key`'s bytes beside its memos, unless they are kept already. A key whose memos
    /// are corrupt gets no slot here: the resolve that follows fails with them.
    fn learn_key<Q: Query>(&self, task: &Guard, query: QueryKey, key: &Q::Key) {
        match self.slot(task, &query) {
            Ok(Some(slot)) if slot.key().is_some() => return,
            Err(_) => return,
            Ok(_) => {}
        }
        let mut bytes = blaze_mem::scratch::lend_bytes();
        cbor::append(&mut bytes, key)
            .unwrap_or_else(|error| panic!("the key of query {} has no encoding: {error}", Q::NAME));
        self.slots.update(task, query, |slot| match slot {
            Some(slot) if slot.key().is_some() => None,
            Some(slot) => Some(self.store.slot(Some(self.store.key_bytes(&bytes)), slot.memos())),
            None => Some(self.store.slot(Some(self.store.key_bytes(&bytes)), &[])),
        });
    }

    /// Adds `made` to its key's memos and says which memo the key has for it: `made`, or an
    /// equal one it had already (then `made`, never published, is freed). The key's memos in
    /// the image being corrupt frees `made` and is the error.
    fn add_memo(&self, task: &Guard, key: QueryKey, made: MemoId) -> Result<MemoId, Corrupt> {
        if let Err(corrupt) = self.slot(task, &key) {
            self.store.discard(made);
            return Err(corrupt);
        }
        // Stamped before `made` is published, so a collection that finds it in the slot sees
        // the stamp; an equal memo found instead is restamped, only ever raised: a giver that
        // loaded the generation before `collect` bumped it must not lower the stamp of one that
        // loaded it after, or `collect` drops a young memo.
        let now = self.generation.load(Ordering::SeqCst);
        self.store.born_in(task, made, now);
        let kept = Cell::new(made);
        self.slots.update(task, key, |slot| {
            let memos = slot.map_or(&[][..], Slot::memos);
            if let Some(&same) = memos.iter().find(|&&it| self.store.same(task, it, &self.store, made)) {
                self.store.reborn_in(task, same, now);
                kept.set(same);
                return None;
            }
            kept.set(made);
            Some(blaze_mem::scratch::with_u32s(|all| {
                all.extend_from_slice(memos);
                all.push(made);
                self.store.slot(slot.and_then(Slot::share_key), all)
            }))
        });
        let kept = kept.get();
        if kept != made {
            self.store.discard(made);
        }
        Ok(kept)
    }

    /// An input's value in a view, read through the tree the first time the view asks.
    fn input<'g>(
        &'g self,
        task: &'g Guard,
        state: &'g ViewState,
        key: QueryKey,
        make: impl FnOnce() -> Input,
    ) -> Result<&'g InputValue, Cancelled> {
        if let Some(value) = state.inputs.get(task, &key) {
            return Ok(value);
        }
        let input = self.inputs.get_or_insert_with(task, key, make);
        self.keep_input(task, state, key, input)
    }

    fn input_value<'g>(
        &'g self,
        task: &'g Guard,
        state: &'g ViewState,
        key: QueryKey,
    ) -> Result<Option<&'g InputValue>, Cancelled> {
        if let Some(value) = state.inputs.get(task, &key) {
            return Ok(Some(value));
        }
        let Some(input) = self.inputs.get(task, &key) else { return Ok(None) };
        self.keep_input(task, state, key, input).map(Some)
    }

    /// Reads `input` in the view and keeps it there (a racing read's, if it kept one first). A
    /// file the tree can't read stops the run as a cancellation does, keeping nothing: its
    /// request fails naming the image's section and group ([`UNREADABLE`]), and the next one
    /// reads the file again.
    fn keep_input<'g>(
        &'g self,
        task: &'g Guard,
        state: &'g ViewState,
        key: QueryKey,
        input: &Input,
    ) -> Result<&'g InputValue, Cancelled> {
        match self.read_input(state, input) {
            Ok(value) => Ok(state.inputs.get_or_insert_with(task, key, || value)),
            Err(corrupt) => Err(unreadable(corrupt)),
        }
    }

    fn read_input(&self, state: &ViewState, input: &Input) -> Result<InputValue, Corrupt> {
        let absent = |what: &str| ContentId::of(ABSENT_DOMAIN, what.as_bytes()).fingerprint();
        let root = match input {
            Input::FileAt { root, .. } | Input::DirAt { root, .. } => *root,
            _ => state.view.root,
        };
        match input {
            Input::File(path) | Input::FileAt { path, .. } => {
                let file = self.tree().file(root, path)?;
                let fingerprint = match &file {
                    Some((id, mode, _)) => Hasher::new(FILE_DOMAIN)
                        .id(id)
                        .update(&mode.git().to_le_bytes())
                        .finish()
                        .fingerprint(),
                    None => absent("file"),
                };
                Ok(InputValue { fingerprint, data: InputData::File(file) })
            }
            Input::Dir(path) | Input::DirAt { path, .. } => {
                let mut hasher = Hasher::new(NAMES_DOMAIN);
                let listed = self.tree().read_dir(root, path, &mut |names| {
                    let mut last: Option<&str> = None;
                    for (name, dir) in names {
                        assert!(
                            last.is_none_or(|last| last < name),
                            "the tree listed directory {path:?} of root {root:x} out of order: {name:?} after {last:?}"
                        );
                        hasher.part(name.as_bytes()).update(&[u8::from(dir)]);
                        last = Some(name);
                    }
                });
                let fingerprint = if listed { hasher.finish().fingerprint() } else { absent("directory") };
                Ok(InputValue { fingerprint, data: InputData::Dir(listed.then(OnceLock::new)) })
            }
            Input::Config => Ok(InputValue {
                fingerprint: ContentId::of(CONFIG_DOMAIN, &state.config).fingerprint(),
                data: InputData::Config,
            }),
        }
    }

    /// A key's fingerprint in a view: an input's, or its memo's, verified or made.
    fn fingerprint_in(&self, task: &Guard, state: &ViewState, key: QueryKey, run: Run) -> Result<Option<Fingerprint>, Cancelled> {
        if is_input(key.kind) {
            return Ok(self.input_value(task, state, key)?.map(|value| value.fingerprint));
        }
        if let Some(resolved) = state.results.get(task, &key) {
            return Ok(Some(resolved.fingerprint()));
        }
        Ok(self.resolve(task, state, key, run, None)?.map(|memo| self.store.fingerprint(task, memo)))
    }

    /// The memo of `key` in a view: the one it resolved, a kept memo every edge of which still
    /// holds, or a new one made by running the query. `None` for a key the database can't run
    /// (a kind not registered, a key whose bytes it does not have).
    fn resolve(
        &self,
        task: &Guard,
        state: &ViewState,
        key: QueryKey,
        run: Run,
        typed: Option<&AnyKey>,
    ) -> Result<Option<MemoId>, Cancelled> {
        if let Some(resolved) = state.results.get(task, &key) {
            return Ok(Some(resolved.memo));
        }
        let Some(info) = self.kinds.get(key.kind) else { return Ok(None) };
        loop {
            let mut ran = None;
            self.single(key, run, || {
                ran = Some(self.resolve_held(task, state, info, key, run, typed)?);
                Ok(())
            })?;
            if let Some(found) = ran {
                return Ok(found);
            }
            if let Some(resolved) = state.results.get(task, &key) {
                return Ok(Some(resolved.memo));
            }
        }
    }

    /// Runs `work` as the one run of `key`, or waits (holding no thread) for the run under way and
    /// returns without running it, for the caller to look again. A wait that would close a
    /// cycle of tasks is [`Cancelled`]; a run ahead never waits, and a key another task runs
    /// is [`Cancelled`] to it at once.
    fn single(&self, key: QueryKey, run: Run, work: impl FnOnce() -> Result<(), Cancelled>) -> Result<(), Cancelled> {
        run.check(key);
        if run.ahead {
            return self.flights.run_if_idle(key.task_key(), work).unwrap_or(Err(Cancelled));
        }
        let awaited = |it: &mut String| {
            use std::fmt::Write as _;
            _ = write!(it, "{key:?}");
        };
        self.flights.get_or_run_named(key.task_key(), awaited, work)
    }

    /// Resolves `key` in the view as a job of [`Cx::get_all`]'s, in a task of its own, ahead of
    /// the task that asked, which asks it again in its turn: whatever stops this run (a key
    /// another task runs, a cancel, a file it can't read) leaves the key for that task.
    fn ahead<Q: Query>(&self, state: &ViewState, key: &Q::Key) {
        let task = blaze_reclaim::enter();
        let query = QueryKey { kind: Q::KIND, input: Q::input(key) };
        if state.results.get(&task, &query).is_some() {
            return;
        }
        self.learn_key::<Q>(&task, query, key);
        if self.resolve(&task, state, query, Run::ahead(), Some(key)).is_err() {
            // Not this job's to report: the asker meets the same file and fails with it.
            _ = take_unreadable_corrupt();
        }
    }

    /// [`Database::resolve`] while holding `key`.
    fn resolve_held(
        &self,
        task: &Guard,
        state: &ViewState,
        info: &KindInfo,
        key: QueryKey,
        run: Run,
        typed: Option<&AnyKey>,
    ) -> Result<Option<MemoId>, Cancelled> {
        if let Some(resolved) = state.results.get(task, &key) {
            return Ok(Some(resolved.memo));
        }
        let _frame = Frame::push(key);
        let slot = self.slot(task, &key).map_err(unreadable)?;
        let mut previous = None;
        for &memo in slot.map_or(&[][..], Slot::memos).iter().rev() {
            if self.store.hot(task, memo).code() != info.code {
                continue;
            }
            let change = match self.verify(task, state, memo, run)? {
                Verdict::Valid => return Ok(Some(self.resolved(task, state, key, memo))),
                Verdict::Changed(changed) => Some(changed),
                Verdict::Unknown => None,
            };
            previous.get_or_insert((self.store.fingerprint(task, memo), change));
        }
        let typed = match typed {
            Some(typed) => typed,
            None => match self.keys.get(task, &key) {
                Some(decoded) => &**decoded,
                None => {
                    let Some(bytes) = slot.and_then(Slot::key) else { return Ok(None) };
                    let Some(decoded) = (info.decode)(bytes) else { return Ok(None) };
                    &**self.keys.get_or_insert_with(task, key, || decoded)
                }
            },
        };
        let (computed, recording) = self.execute(task, state, info, run, typed)?;
        let made = recording.with(|edges| match info.share {
            Some(shared) => self.store.make_shared(key, computed.fingerprint, info.code, edges, &computed.bytes, shared),
            None => self.store.make(key, computed.fingerprint, info.code, edges, &computed.bytes),
        });
        drop(recording);
        let memo = self.add_memo(task, key, made).map_err(unreadable)?;
        state.reruns.insert(task, key, Rerun { previous });
        Ok(Some(self.resolved(task, state, key, memo)))
    }

    /// Records `memo` as `key`'s in the view, and says which memo the view has for it (a
    /// racing run's, if it recorded first).
    fn resolved(&self, task: &Guard, state: &ViewState, key: QueryKey, memo: MemoId) -> MemoId {
        state.results.insert(task, key, Resolved::new(&self.store, task, memo)).memo
    }

    /// Whether every edge of `memo`, in order, still holds in the view: one sequential walk of
    /// its range of the edge segment, each edge's key and fingerprint read in place. As it
    /// checks one edge it prefetches the place of the edge `AHEAD` on and the row of the edge
    /// `AHEAD / 2` on, so their misses overlap; it still stops at the first edge that changed.
    /// A database small enough to stay in cache (`PREFETCH_FROM`) skips the prefetches.
    fn verify(&self, task: &Guard, state: &ViewState, memo: MemoId, run: Run) -> Result<Verdict, Cancelled> {
        let edges = self.store.edges(task, memo);
        let far = self.store.end() >= PREFETCH_FROM;
        for (at, edge) in edges.iter().enumerate() {
            if far && let Some(ahead) = edges.get(at + AHEAD) {
                let read = ahead.read();
                if is_input(read.kind) {
                    state.inputs.prefetch_place(task, &read);
                } else {
                    state.results.prefetch_place(task, &read);
                }
            }
            if far && let Some(ahead) = edges.get(at + AHEAD / 2) {
                let read = ahead.read();
                if is_input(read.kind) {
                    state.inputs.prefetch_entry(task, &read);
                } else {
                    state.results.prefetch_entry(task, &read);
                }
            }
            let read = edge.read();
            match self.fingerprint_in(task, state, read, run)? {
                Some(now) if now == edge.saw() => {}
                Some(now) => return Ok(Verdict::Changed(Changed { key: read, was: edge.saw(), now })),
                None => return Ok(Verdict::Unknown),
            }
        }
        Ok(Verdict::Valid)
    }

    /// Runs the query, its edges recorded on this thread's edge stack while the returned
    /// [`Recording`] lives.
    fn execute(
        &self,
        task: &Guard,
        state: &ViewState,
        info: &KindInfo,
        run: Run,
        key: &AnyKey,
    ) -> Result<(Computed, Recording), Cancelled> {
        let recording = Recording::start();
        let mut cx = Cx { db: self, task, state, run };
        let _inside = Inside::enter();
        let computed = blaze_mem::frame::task(info.tag, |_| (info.run)(&mut cx, key))?;
        Ok((computed, recording))
    }

    /// The bytes of a memo's value, made again if it was evicted.
    fn value_of<'g>(
        &'g self,
        task: &'g Guard,
        state: &'g ViewState,
        key: QueryKey,
        memo: MemoId,
        run: Run,
        typed: &AnyKey,
    ) -> Result<&'g [u8], Cancelled> {
        loop {
            if let Some(bytes) = self.store.bytes(task, memo) {
                return Ok(bytes);
            }
            let Some(info) = self.kinds.get(key.kind) else {
                unreachable!("a memo of kind {} was made, but the kind is not registered", key.kind.0)
            };
            self.single(key, run, || {
                if self.store.bytes(task, memo).is_some() {
                    return Ok(());
                }
                let _frame = Frame::push(key);
                let (computed, _edges) = self.execute(task, state, info, run, typed)?;
                let fingerprint = self.store.fingerprint(task, memo);
                if computed.fingerprint != fingerprint {
                    panic!(
                        "query {} is not deterministic: made again after its value was evicted, \
                         {key:?} has fingerprint {:?}, not {fingerprint:?}",
                        info.name, computed.fingerprint
                    );
                }
                match info.share {
                    Some(shared) => self.store.refill_shared(task, memo, &computed.bytes, shared),
                    None => self.store.refill(task, memo, &computed.bytes),
                }
                Ok(())
            })?;
        }
    }

    /// The fingerprint a view already has for `key`, computing nothing.
    fn known(&self, task: &Guard, state: &ViewState, key: QueryKey) -> Option<Fingerprint> {
        if is_input(key.kind) {
            state.inputs.get(task, &key).map(|value| value.fingerprint)
        } else {
            state.results.get(task, &key).map(Resolved::fingerprint)
        }
    }

    /// What `changed.key` read that made it change from `changed.was`.
    fn why_changed(&self, task: &Guard, state: &ViewState, changed: Changed) -> Option<Changed> {
        if let Some(Rerun { previous: Some((was, Some(next))) }) = state.reruns.get(task, &changed.key)
            && *was == changed.was
        {
            return Some(*next);
        }
        let slot = self.slot(task, &changed.key).ok()??;
        let old = slot.memos().iter().rev().copied().find(|&memo| self.store.fingerprint(task, memo) == changed.was)?;
        self.store.edges(task, old).iter().find_map(|edge| {
            let (read, saw) = (edge.read(), edge.saw());
            let now = self.known(task, state, read)?;
            (now != saw).then_some(Changed { key: read, was: saw, now })
        })
    }

    fn write_cold(&self, task: &Guard, memos: impl Iterator<Item = MemoId>, effects: &mut Vec<Effect>) {
        let dir = self.dir("move cold values");
        // alloc: one pack, its entries and its name a collection that moves values.
        let mut bytes = Vec::new();
        let mut entries = Vec::new(); // alloc: see above
        // Equal values (a shared row's memos, or equal bytes of any kind) are written once.
        let mut written: FxMap<ContentId, usize> = FxMap::default(); // alloc: see above
        for memo in memos {
            let Some((word, value)) = self.store.in_memory(task, memo) else { continue };
            let (len, id) = (value.len(), value_id(value));
            let at = *written.entry(id).or_insert_with(|| {
                bytes.resize(bytes.len().next_multiple_of(blaze_format::ALIGN), 0);
                let at = bytes.len();
                bytes.extend_from_slice(value);
                at
            });
            entries.push(ColdEntry { memo, word, at, len, id });
        }
        if entries.is_empty() {
            return;
        }
        self.store.pin(entries.iter().map(|entry| entry.memo));
        let mut io = self.io.lock();
        let file = FileName::new(&format!("{dir}/cold/{}", io.cold_files)) // alloc: see above
            .expect("a name under the database's directory");
        io.cold_files += 1;
        let tag = io.tag(self.config.tags);
        effects.push(Effect::Write { file: file.clone(), bytes, tag }); // clone: see above
        io.pending.insert(tag, Pending::ColdWrite { file, entries });
    }

    /// Every key's memos, for a new image: a corrupt group of the old one is the error, never
    /// an image without what it held.
    fn image_entries<'g>(&'g self, task: &'g Guard) -> Result<image::Entries<'g>, Corrupt> {
        // One list for every key, one for every memo, and one sort buffer reused across keys.
        // alloc: once per image, as are the lists below.
        let mut keys: Vec<QueryKey> = Vec::new();
        self.slots.for_each(task, |key, _| keys.push(*key));
        if let Some(image) = &self.image {
            image.for_each_key(|key| keys.push(key));
        }
        sort_keys(&mut keys);
        keys.dedup();
        // Reading a key's memos in from the image may add codes, so a code past the snapshot
        // takes a new one.
        let mut codes = self.store.codes();
        let mut entries = image::Entries { keys: Vec::with_capacity(keys.len()), memos: Vec::with_capacity(keys.len()) };
        let mut sorting: Vec<(Fingerprint, ContentId, SchemaHash, &[EdgeRecord], MemoId)> = Vec::new(); // alloc: see above
        if let Some(corrupt) = self.image.as_ref().and_then(ImageMap::first_corrupt) {
            return Err(corrupt);
        }
        for key in keys {
            let Some(slot) = self.slot(task, &key)? else { continue };
            if slot.memos().is_empty() {
                continue;
            }
            sorting.clear();
            for &memo in slot.memos() {
                let hot = self.store.hot(task, memo);
                if hot.code() as usize >= codes.len() {
                    codes = self.store.codes();
                }
                let (schema, producer) = codes[hot.code() as usize];
                sorting.push((hot.fingerprint(), producer, schema, self.store.edges(task, memo), memo));
            }
            sorting.sort_by(|a, b| {
                (a.0, a.1, a.2, a.3.len())
                    .cmp(&(b.0, b.1, b.2, b.3.len()))
                    .then_with(|| a.3.iter().map(EdgeRecord::order).cmp(b.3.iter().map(EdgeRecord::order)))
            });
            let start = entries.memos.len();
            entries.memos.extend(sorting.iter().map(|&(.., memo)| memo));
            entries.keys.push(image::Entry { key, memos: start..entries.memos.len(), key_bytes: slot.key() });
        }
        Ok(entries)
    }

    fn image_inputs(&self, task: &Guard) -> Vec<(QueryKey, Input)> {
        let mut inputs = self.inputs.snapshot(task);
        inputs.sort_by_key(|(key, _)| *key);
        inputs
    }
}
