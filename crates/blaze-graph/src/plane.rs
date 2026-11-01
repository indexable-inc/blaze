//! The plane (docs/v2/plan/lanes/core/plane.md, "The plane"): one scheduler for every request a daemon has under
//! way, as a state machine that does no IO.
//!
//! A request (a build, a test run, an edit) is opened with a weight, adds nodes, and is closed;
//! it finishes once each of its nodes is settled. A node may have a [`Key`], the hash of
//! everything its value depends on. The plane keeps no results (docs/v2/plan/lanes/core.md, "One memo, the
//! database's"): before it runs a node with a key it asks the [`Memo`], and a key the database
//! has settles the node at once, running nothing. A node whose key another node is running or
//! about to run joins it (**single flight**): one task runs, and every node of it settles when
//! it ends. A task not yet started is ready in the request of each of its nodes, at that node's
//! priority, and the first request to start it runs it.
//!
//! Of the requests with ready work, the next thread of a pool goes to the one that started the
//! fewest tasks of the pool for its weight (**fair share**, stride scheduling: each start moves
//! a request's virtual time on by the inverse of its weight, and the least virtual time goes
//! next; a request that had no ready work comes back at the plane's virtual time, not behind
//! it). Ties go to the heavier request, then to the one opened first. Within a request, the
//! ready task with the longest path of expected cost to the end of the request runs first
//! (**critical path**), ties to the node added first.
//!
//! A request opened **idle** (work ahead of need) is a class below the others: its ready task
//! starts only when no request not idle has one ready that fits, its starts move only the
//! virtual time idle requests share, it never holds back another request's task for memory,
//! and when a task of a request not idle waits for nothing but the threads or memory of
//! running idle tasks the plane asks the newest of them to give way ([`PlaneOutput::Yield`]).
//! [`PlaneInput::Promote`] makes it a request like the others.
//!
//! With a [`Budget`], a task starts only when the predicted peaks of what runs (or what the
//! process is said to hold, if more), plus its own, fit in it (**memory admission**). Each time
//! a task with memory starts while another, ready in the same pool, does not fit, that one is
//! passed over; the first passed over [`PlaneConfig::starve_after`] times holds back every task
//! with memory, of every pool and request, until it starts, so a large task is never starved
//! ([`Plane::holding`]). A task predicted the whole budget runs alone: it starts once no task
//! granted memory runs, whatever the process holds besides, and while it runs no other task
//! granted memory fits (a program never measured, `admission::kinds::PROGRAM`). A task whose
//! prediction alone is more than the budget fails before it starts, saying so; so does one
//! that cannot fit while nothing runs. Nothing is retried with less at once.
//!
//! A request may be cancelled ([`Plane::cancel`]): what of it has not started is skipped, what
//! runs is let finish, and it finishes failed. A plane lives as long as the daemon: a request
//! [`Plane::release`]d gives its nodes' room back once it finishes.
//!
//! **Layout** (docs/v2/plan/design/data.md). Nodes, tasks and requests are slots into columns
//! reserved up front ([`RegionVec`], tagged [`MemTag::Plane`]), a hot row of what scheduling
//! reads (64 bytes a node, 32 a task) beside colder ones. Which slots are live is a
//! [`Slab`] of each (docs/v2/plan/design/memory.md, "Generational handles"): a slot freed is
//! taken again first, and what names a slot across lifetimes (a node's id in the index, a
//! request's, a key in flight, a node's task, the task held back, the idle tasks running)
//! holds its [`Handle`], so a handle to a slot that ended is stale, not the slot's next
//! occupant. A request's edges are one flat array: a node's deps a range of it, the
//! nodes that wait for a node a list threaded through it. A task's members are a list threaded
//! through the node rows. A request's ready nodes of a pool are a 4-ary heap of 24-byte entries
//! that each node knows its place in, so a start, a raise and a leave are `O(log n)` with no
//! allocation. A key is its own hash: the flight table indexes by its first eight bytes. What a
//! step needs for a moment (a batch's order, the requests touched, the tasks passed over) is a
//! buffer kept on the plane, so in steady state a task costs the plane no allocation:
//! [`Plane::step_into`] appends to the caller's buffer of outputs.

#![forbid(unsafe_code)]

use std::{
    borrow::Cow,
    cmp::Reverse,
    collections::{BinaryHeap, hash_map::Entry},
    fmt,
    hash::{Hash, Hasher},
};

use blaze_hash::{ByHash, HashMap};
use blaze_mem::{Handle, RegionVec, Slab, Tag as MemTag};
use blaze_protocol::events::{Event, Level, NodeKind, NodeState, Record};
use blaze_radix as radix;

use serde::{Deserialize, Serialize};

use crate::machine::{Error, NodeId, Outcome, Pool, Spec};

/// A request's name, chosen by whoever opens it.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(transparent)]
pub struct RequestId(pub u32);

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "request {}", self.0)
    }
}

/// The hash of everything a node's value depends on: two nodes with one key are one piece of
/// work.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[serde(transparent)]
pub struct Key(pub [u8; 32]);

/// A key is already a content hash: its first eight bytes are its hash, for any hasher.
impl Hash for Key {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_u64(self.prefix());
    }
}

impl Key {
    fn prefix(&self) -> u64 {
        let [a, b, c, d, e, f, g, h, ..] = self.0;
        u64::from_le_bytes([a, b, c, d, e, f, g, h])
    }
}

/// What the query database has of a key: the one memo of results (docs/v2/plan/lanes/core.md, "One memo,
/// the database's"). `Some` settles a node of the key at once, running nothing: `Built` as
/// cached on this machine, `Cached` as it says, `Failed` as failed. A task's work puts its
/// value in the database before its [`PlaneInput::Done`], so a node of its key that becomes
/// ready after finds it. Answers must be the same for the same database, or a trace does not
/// replay.
pub trait Memo {
    fn settled(&self, key: Key) -> Option<Outcome>;
}

impl<F: Fn(Key) -> Option<Outcome>> Memo for F {
    fn settled(&self, key: Key) -> Option<Outcome> {
        self(key)
    }
}

/// No database: nothing is settled before it runs. The memo of every [`Plane::new`].
#[derive(Debug, Clone, Copy, Default)]
pub struct NoMemo;

impl Memo for NoMemo {
    #[inline]
    fn settled(&self, _key: Key) -> Option<Outcome> {
        None
    }
}

/// The memory the plane may grant, and what says so, for the error of a task that cannot fit.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)] // clone: a plane's config, at setup and in tests
pub struct Budget {
    pub bytes: u64,
    pub from: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct PlaneConfig {
    /// How many tasks of [`Pool::Io`] run at once.
    pub io: usize,
    /// How many tasks of [`Pool::Work`] run at once: the cores.
    pub work: usize,
    pub budget: Option<Budget>,
    /// How many times a task may be passed over for memory before it holds back the tasks
    /// behind it.
    pub starve_after: u32,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
#[cfg_attr(test, derive(Clone))] // clone: the differential tests feed one input to two planes
pub enum PlaneInput {
    /// A request begins. Its `weight` is its share of the threads against the others'; with
    /// `keep_going` false its first failure skips what of it has not started. An `idle`
    /// request runs on what no other can use: its ready task starts only when no task of a
    /// request not idle is ready and fits that pool and memory, its starts move only the
    /// virtual time of idle requests, and its running tasks give way ([`PlaneOutput::Yield`])
    /// to a task of a request not idle that waits for nothing but their threads or memory.
    Open {
        at_ns: u64,
        request: RequestId,
        weight: u32,
        keep_going: bool,
        #[serde(default)]
        idle: bool,
    },
    /// The idle `request` is idle no more: it takes its share from now on, coming in at the
    /// plane's virtual time as a request that had nothing ready does. Of a request not idle,
    /// or finished (and so, once released, of one not known), it does nothing.
    Promote { at_ns: u64, request: RequestId },
    /// The task the plane asked to run as `id` gave way with no result, as a
    /// [`PlaneOutput::Yield`] asked: its thread and memory are free again and its nodes ready
    /// again, each in its own request (skipped, of a request cancelled, or failed and not
    /// keeping going).
    Yielded { at_ns: u64, id: NodeId },
    /// Nodes of the request, besides those it had. Their deps are nodes of the same request.
    Add { at_ns: u64, request: RequestId, nodes: Vec<Spec> },
    /// [`PlaneInput::Add`] of a run of ids as columns, one allocation a column rather than
    /// two a node: the same nodes in the same order say the same, but that one
    /// [`Event::Nodes`] of the columns, before anything else, says them all in place of an
    /// [`Event::Node`] a node.
    AddNodes { at_ns: u64, request: RequestId, nodes: Nodes },
    /// No more nodes are added to the request but by [`PlaneInput::Done`].
    Close { at_ns: u64, request: RequestId },
    /// The task the plane asked to run as `id` is done, on the thread group `group` where the
    /// executor has groups; its work found the nodes `added`, of `id`'s request. `peak` is the
    /// most its arena held, measured, for [`crate::admission::Estimates::observe`]; 0 when
    /// not measured.
    Done {
        at_ns: u64,
        id: NodeId,
        outcome: Outcome,
        added: Vec<Spec>,
        group: Option<u32>,
        #[serde(default)]
        peak: u64,
    },
    /// The bytes the process holds now, measured.
    Held { at_ns: u64, bytes: u64 },
}

impl PlaneInput {
    pub fn at_ns(&self) -> u64 {
        match self {
            PlaneInput::Open { at_ns, .. }
            | PlaneInput::Add { at_ns, .. }
            | PlaneInput::AddNodes { at_ns, .. }
            | PlaneInput::Close { at_ns, .. }
            | PlaneInput::Done { at_ns, .. }
            | PlaneInput::Held { at_ns, .. }
            | PlaneInput::Promote { at_ns, .. }
            | PlaneInput::Yielded { at_ns, .. } => *at_ns,
        }
    }
}

/// Nodes with the ids `first`, `first + 1`, and on, as columns: the names one string, the deps
/// one array, each cut by offsets. Built by [`Nodes::push`], or read from a trace and checked,
/// so the columns always agree.
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
#[cfg_attr(test, derive(Clone))] // clone: inside `PlaneInput`, for the differential tests
#[serde(try_from = "Columns")]
pub struct Nodes {
    first: NodeId,
    kinds: Vec<NodeKind>,
    /// Node `i`'s name is `names[name_at[i]..name_at[i + 1]]`.
    names: String,
    name_at: Vec<u32>,
    /// Node `i`'s deps are `deps[deps_at[i]..deps_at[i + 1]]`.
    deps: Vec<NodeId>,
    deps_at: Vec<u32>,
    pools: Vec<Pool>,
    costs: Vec<u64>,
    keys: Vec<Option<Key>>,
    memory: Vec<u64>,
}

/// A node for [`Nodes::push`]: a [`Spec`] whose id is the next of the run, borrowing its name
/// and deps.
#[derive(Debug, Clone, Copy)]
pub struct NodeRef<'a> {
    pub kind: NodeKind,
    pub name: &'a str,
    pub deps: &'a [NodeId],
    pub pool: Pool,
    pub cost: u64,
    pub key: Option<Key>,
    pub memory: u64,
}

impl Nodes {
    pub fn new(first: NodeId) -> Nodes {
        Nodes::with_capacity(first, 0, 0, 0)
    }

    /// Room for `nodes` nodes, `name_bytes` bytes of names and `deps` deps in all.
    pub fn with_capacity(first: NodeId, nodes: usize, name_bytes: usize, deps: usize) -> Nodes {
        let mut name_at = Vec::with_capacity(nodes + 1);
        name_at.push(0);
        let mut deps_at = Vec::with_capacity(nodes + 1);
        deps_at.push(0);
        Nodes {
            first,
            kinds: Vec::with_capacity(nodes),
            names: String::with_capacity(name_bytes),
            name_at,
            deps: Vec::with_capacity(deps),
            deps_at,
            pools: Vec::with_capacity(nodes),
            costs: Vec::with_capacity(nodes),
            keys: Vec::with_capacity(nodes),
            memory: Vec::with_capacity(nodes),
        }
    }

    /// Adds the next node of the run: its id.
    pub fn push(&mut self, node: NodeRef<'_>) -> NodeId {
        let id = u32::try_from(self.kinds.len())
            .ok()
            .and_then(|at| self.first.0.checked_add(at))
            .unwrap_or_else(|| panic!("ids from {} past u32", self.first.0));
        self.names.push_str(node.name);
        self.name_at.push(u32::try_from(self.names.len()).expect("a batch's names under 4 GiB"));
        self.deps.extend_from_slice(node.deps);
        self.deps_at.push(u32::try_from(self.deps.len()).expect("a batch's deps under 2^32"));
        self.kinds.push(node.kind);
        self.pools.push(node.pool);
        self.costs.push(node.cost);
        self.keys.push(node.key);
        self.memory.push(node.memory);
        NodeId(id)
    }

    pub fn first(&self) -> NodeId {
        self.first
    }

    pub fn len(&self) -> usize {
        self.kinds.len()
    }

    pub fn is_empty(&self) -> bool {
        self.kinds.is_empty()
    }

    /// The id of the node at `at`.
    pub fn id(&self, at: usize) -> NodeId {
        assert!(at < self.len(), "node {at} of {}", self.len());
        NodeId(self.first.0 + at as u32)
    }

    pub fn name(&self, at: usize) -> &str {
        &self.names[self.name_at[at] as usize..self.name_at[at + 1] as usize]
    }

    pub fn deps(&self, at: usize) -> &[NodeId] {
        &self.deps[self.deps_at[at] as usize..self.deps_at[at + 1] as usize]
    }

    /// The node at `at` as a [`Spec`].
    pub fn spec(&self, at: usize) -> Spec {
        Spec {
            id: self.id(at),
            kind: self.kinds[at],
            // alloc: a `Spec` owns its name and deps; the plane reads `Nodes` without this
            name: self.name(at).to_owned(),
            // alloc: as above
            deps: self.deps(at).to_vec(),
            pool: self.pools[at],
            cost: self.costs[at],
            key: self.keys[at],
            memory: self.memory[at],
        }
    }

    /// What the plane takes of the node at `at`: its name borrowed, and no record of its own,
    /// since the batch's [`Nodes::record`] says it.
    fn parts(&self, at: usize) -> Parts<'_> {
        Parts {
            id: self.id(at),
            kind: self.kinds[at],
            name: Cow::Borrowed(self.name(at)),
            deps: None,
            pool: self.pools[at],
            cost: self.costs[at],
            key: self.keys[at],
            memory: self.memory[at],
        }
    }

    /// The batch as one [`Event::Nodes`], its columns taken: the nodes as their
    /// [`Event::Node`]s would say them, in the order given.
    fn record(self) -> Event {
        let Nodes { first, kinds, names, name_at, deps, deps_at, .. } = self;
        // alloc: none: collected in place, `NodeId` being a `u32`
        let deps = deps.into_iter().map(|it| it.0).collect();
        Event::Nodes { first: first.0, kinds, names, name_at, deps, deps_at }
    }
}

/// [`Nodes`] as a trace holds it, not yet checked.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Columns {
    first: NodeId,
    kinds: Vec<NodeKind>,
    names: String,
    name_at: Vec<u32>,
    deps: Vec<NodeId>,
    deps_at: Vec<u32>,
    pools: Vec<Pool>,
    costs: Vec<u64>,
    keys: Vec<Option<Key>>,
    memory: Vec<u64>,
}

impl TryFrom<Columns> for Nodes {
    type Error = String;

    fn try_from(it: Columns) -> Result<Nodes, String> {
        let len = it.kinds.len();
        let columns = [
            ("pools", it.pools.len()),
            ("costs", it.costs.len()),
            ("keys", it.keys.len()),
            ("memory", it.memory.len()),
        ];
        for (column, its) in columns {
            if its != len {
                // alloc: a refused trace's message, once
                return Err(format!("{len} nodes but {its} {column}"));
            }
        }
        if u32::try_from(len).ok().and_then(|len| it.first.0.checked_add(len)).is_none() {
            // alloc: a refused trace's message, once
            return Err(format!("{len} ids from {} past u32", it.first.0));
        }
        let cuts = |column: &str, at: &[u32], end: usize| {
            if at.len() != len + 1 || at[0] != 0 || at[len] as usize != end {
                let offsets = at.len();
                // alloc: a refused trace's message, once
                return Err(format!("{column} of {offsets} offsets for {len} nodes and {end} entries"));
            }
            match at.windows(2).position(|pair| pair[0] > pair[1]) {
                // alloc: a refused trace's message, once
                Some(i) => Err(format!("{column} goes back at node {i}")),
                None => Ok(()),
            }
        };
        cuts("name_at", &it.name_at, it.names.len())?;
        cuts("deps_at", &it.deps_at, it.deps.len())?;
        if let Some(i) = it.name_at.iter().position(|&at| !it.names.is_char_boundary(at as usize)) {
            // alloc: a refused trace's message, once
            return Err(format!("name_at cuts a character at node {i}"));
        }
        Ok(Nodes {
            first: it.first,
            kinds: it.kinds,
            names: it.names,
            name_at: it.name_at,
            deps: it.deps,
            deps_at: it.deps_at,
            pools: it.pools,
            costs: it.costs,
            keys: it.keys,
            memory: it.memory,
        })
    }
}

/// What [`Plane::add_one`] takes of a node, from a [`Spec`] or from [`Nodes`].
struct Parts<'a> {
    id: NodeId,
    kind: NodeKind,
    name: Cow<'a, str>,
    /// The deps its own [`Event::Node`] says, with its name; `None` for a node of [`Nodes`],
    /// which its batch's [`Event::Nodes`] says.
    deps: Option<Vec<u32>>,
    pool: Pool,
    cost: u64,
    key: Option<Key>,
    memory: u64,
}

impl From<Spec> for Parts<'static> {
    fn from(spec: Spec) -> Parts<'static> {
        let Spec { id, kind, name, deps, pool, cost, key, memory } = spec;
        // alloc: none: collected in place, `NodeId` being a `u32`
        let deps = Some(deps.into_iter().map(|it| it.0).collect());
        Parts { id, kind, name: Cow::Owned(name), deps, pool, cost, key, memory }
    }
}

/// The nodes of one input as [`Plane::check`] reads them.
trait Given {
    fn len(&self) -> usize;
    fn id(&self, at: usize) -> NodeId;
    fn deps(&self, at: usize) -> &[NodeId];
}

impl Given for [Spec] {
    fn len(&self) -> usize {
        <[Spec]>::len(self)
    }

    fn id(&self, at: usize) -> NodeId {
        self[at].id
    }

    fn deps(&self, at: usize) -> &[NodeId] {
        &self[at].deps
    }
}

impl Given for Nodes {
    fn len(&self) -> usize {
        Nodes::len(self)
    }

    fn id(&self, at: usize) -> NodeId {
        Nodes::id(self, at)
    }

    fn deps(&self, at: usize) -> &[NodeId] {
        Nodes::deps(self, at)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum PlaneOutput {
    /// A record of a request: a node, a move, its critical path.
    Said { request: RequestId, record: Record },
    /// Run the work of node `id` on `pool`, near the thread group `near` if the executor has
    /// one, then give the plane a [`PlaneInput::Done`] of it. `critical`: its path of expected
    /// cost to the end of its request is the longest of the request's ready or running work,
    /// so a machine with fast and slow cores gives it a fast one.
    Run { id: NodeId, pool: Pool, near: Option<u32>, critical: bool },
    /// Every node of the request is settled. Nothing more comes of it.
    Finished { request: RequestId, ok: bool },
    /// Stop the task run as `id`, whose nodes are all of idle requests, and give the plane a
    /// [`PlaneInput::Yielded`] of it (or its [`PlaneInput::Done`], if it ends first): a task
    /// of a request not idle is ready and waits for nothing but the thread or memory it
    /// holds, which it holds until then. Each task is named once.
    Yield { id: NodeId },
}

/// What the plane has done, for a person and for the tests.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Counts {
    /// Tasks started.
    pub ran: u64,
    /// Nodes settled from the [`Memo`], running nothing.
    pub remembered: u64,
    /// Nodes that joined a task another node had.
    pub joined: u64,
    /// The most memory granted at once.
    pub granted_most: u64,
}

/// What a start moves a request's virtual time on by, over its weight: divisible by every
/// weight up to 16, so for those the shares are exact.
const SHARE: u128 = 720_720 << 32;

/// What one request did, for its proof of work ([`Plane::stats`]).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RequestStats {
    /// Its nodes that ran their task (whatever came of it, a cache hit of the work too).
    pub ran: u64,
    /// Its nodes settled by the [`Memo`], running nothing.
    pub cached: u64,
    /// Its nodes that settled with a task another node ran.
    pub joined: u64,
    /// The longest chain through its nodes, each after its deps, of measured run times: from
    /// the step that said a node's `Run` to the step of its `Done`. A cached or joined node's
    /// own time is 0 here; one skipped ends no chain.
    pub critical_path_ns: u64,
    /// Its nodes' measured run times, as in [`RequestStats::critical_path_ns`], summed by
    /// kind: `steps_ns[kind as usize]`, in [`NodeKind::ALL`]'s order.
    pub steps_ns: [u64; NodeKind::ALL.len()],
}

/// No node, edge or place: the end of a list, a node in no ready heap.
const NONE: u32 = u32::MAX;

/// A batch's dep on a node of the same batch, at the place in the low bits. Slots are below
/// it.
const NEW: u32 = 1 << 31;

/// The most nodes, tasks and requests a plane holds at once: the room its columns reserve,
/// address space only, committed as they fill. One more stops the plane naming
/// [`MemTag::Plane`] (no-fallback).
const MAX_NODES: u32 = 1 << 24;
const MAX_TASKS: u32 = MAX_NODES;
const MAX_REQUESTS: u32 = 1 << 16;

const _: () = assert!(MAX_NODES < NEW && MAX_TASKS < NEW && MAX_REQUESTS < NEW);

/// The slot kinds, for the handles that name them.
#[derive(Default)]
pub(crate) struct NodeSlot;
#[derive(Default)]
pub(crate) struct TaskSlot;
#[derive(Default)]
pub(crate) struct RequestSlot;

type NodeHandle = Handle<NodeSlot>;
pub(crate) type TaskHandle = Handle<TaskSlot>;
type RequestHandle = Handle<RequestSlot>;

/// No task: a slot index no slab gives.
const NO_TASK: TaskHandle = Handle::from_bits(u64::MAX);

/// The row of `task` in the task columns.
fn row_of(task: TaskHandle) -> usize {
    task.index() as usize
}

/// In the index while a batch is checked: the node at `place` of the batch, as a handle of
/// generation 0, which no slot ever has.
fn place(place: usize) -> NodeHandle {
    Handle::from_bits(place as u64)
}

/// The place of a node of the batch being checked, if `handle` is one ([`place`]).
fn placed(handle: NodeHandle) -> Option<u32> {
    (handle.generation() == 0).then_some(handle.index())
}

/// Which slots of one kind are live: the slab keeps each slot's generation and the free list,
/// the last freed taken first; the rows are the plane's columns, by the slot's index, made as
/// [`Slots::made`] grows.
pub(crate) struct Slots<M> {
    slab: Slab<M>,
}

impl<M: Default> Slots<M> {
    #[track_caller]
    pub(crate) fn new(max: u32) -> Slots<M> {
        Slots { slab: Slab::new(max, MemTag::Plane).unwrap_or_else(|error| panic!("{error}")) }
    }

    pub(crate) fn take(&mut self) -> Handle<M> {
        self.slab.insert(M::default())
    }

    pub(crate) fn live(&self, handle: Handle<M>) -> bool {
        self.slab.contains(handle)
    }

    #[track_caller]
    pub(crate) fn give(&mut self, handle: Handle<M>) {
        assert!(self.slab.remove(handle).is_some(), "slot {handle:?} given back, but it is not live");
    }

    /// Slots made so far, live or free: the rows each column has.
    pub(crate) fn made(&self) -> usize {
        self.slab.slots()
    }
}

/// A column of `max` rows reserved for the plane.
#[track_caller]
fn column<T>(max: u32) -> RegionVec<T> {
    RegionVec::new(max as usize, MemTag::Plane).unwrap_or_else(|error| panic!("{error}"))
}

/// [`NodeCold::flags`]: it has a key; it settled (`done_at` holds); its task started as it
/// (`started_at` holds); its task ran on a thread group (`group` holds).
const KEYED: u8 = 1;
const DONE: u8 = 2;
const STARTED: u8 = 4;
const GROUP: u8 = 8;

/// What adding, readying, starting and settling a node read: 64 bytes, one cache line, found
/// by its slot.
#[derive(Clone, Copy)]
struct NodeRow {
    /// The longest path of expected cost from it to the end of its request, its own included.
    priority: u64,
    /// Its place in the order nodes were added to the plane.
    seq: u64,
    cost: u64,
    id: NodeId,
    request: u32,
    /// How many of its deps are not done.
    waiting: u32,
    /// Its place in its request's ready heap of its task's pool.
    heap_at: u32,
    /// The member of its task that joined after it.
    next_member: u32,
    /// The first and last of its request's edges whose dep it is: the nodes that wait for it,
    /// in the order they were added.
    first_out: u32,
    last_out: u32,
    /// Its task, [`NO_TASK`] for none.
    task: TaskHandle,
}

const _: () = assert!(size_of::<NodeRow>() == 64);

/// What a node needs now and then: as it becomes ready, as its task starts, as it settles,
/// as its request ends. 64 bytes, found by its slot.
#[derive(Clone, Copy)]
struct NodeCold {
    memory: u64,
    done_at: u64,
    started_at: u64,
    /// The longest chain of measured run times through its request's nodes to it, its own
    /// included, once it settled ([`RequestStats::critical_path_ns`]).
    path_ns: u64,
    /// Its deps: `deps` edges of its request from `deps_at`.
    deps_at: u32,
    deps: u32,
    /// The dep that was done last when it became ready.
    blocker: u32,
    /// The thread group its task ran on.
    group: u32,
    /// Its name: a range of its request's `names`.
    name_at: u32,
    name_len: u32,
    pool: Pool,
    flags: u8,
    kind: NodeKind,
}

const _: () = assert!(size_of::<NodeCold>() == 64);

/// A dep of a node in its request's edges: the dep, the node that waits for it (`owner`), and
/// the next edge of the same dep. A node's deps are one range of edges (CSR); the nodes that
/// wait for one are a list through `next`. 12 bytes an edge.
#[derive(Clone, Copy)]
struct Edge {
    dep: u32,
    owner: u32,
    next: u32,
}

const _: () = assert!(size_of::<Edge>() == 12);

/// A task: 32 bytes, found by its slot; its key, if `keyed`, in a column beside.
#[derive(Clone, Copy)]
struct TaskRow {
    memory: u64,
    /// The node whose id the task runs as, and whose request it counts against: the member
    /// that started it, once it runs.
    runner: u32,
    /// Every node that settles when it does, a list through [`NodeRow::next_member`] in the
    /// order they joined. While it is not running, each is ready in its own request.
    first: u32,
    last: u32,
    /// How many times a task with memory started while it did not fit.
    passed: u32,
    /// The last pass over it counted ([`Plane::pass`]), so a pass counts it once.
    stamp: u32,
    pool: Pool,
    running: bool,
    keyed: bool,
    /// Running, and asked to give way ([`PlaneOutput::Yield`]).
    yielding: bool,
}

const _: () = assert!(size_of::<TaskRow>() == 32);

/// A ready node in its request's heap: the least `(rank, seq)` runs first, `rank` being the
/// complement of its priority, so the longest path goes first and ties to the node added
/// first. 24 bytes.
#[derive(Clone, Copy)]
struct Ready {
    rank: u64,
    seq: u64,
    node: u32,
}

const _: () = assert!(size_of::<Ready>() == 24);

impl Ready {
    fn before(&self, other: &Ready) -> bool {
        (self.rank, self.seq) < (other.rank, other.seq)
    }
}

struct Request {
    id: RequestId,
    /// Its place in the order requests were opened.
    seq: u64,
    weight: u32,
    /// What a start moves its virtual time on by: [`SHARE`] over its weight.
    stride: u128,
    keep_going: bool,
    /// Of the idle class ([`PlaneInput::Open`]), until promoted.
    idle: bool,
    closed: bool,
    cancelled: bool,
    finished: bool,
    /// Whether its room is given back once it finishes.
    release: bool,
    /// How many of its nodes are not settled.
    open: u32,
    /// Its first node that failed.
    failed: u32,
    /// Its ready nodes of each pool, each one of a task not running, as a 4-ary heap: the
    /// first to run first.
    ready: [Vec<Ready>; 2],
    /// Its place in the plane's `active` requests of each pool, while its heap of it is not
    /// empty.
    active_at: [u32; 2],
    /// Its virtual time in each pool, for fair share.
    vtime: [u128; 2],
    /// Its running nodes as `(priority, seq, slot)`, the most first, for
    /// [`PlaneOutput::Run`]'s `critical`. An entry goes stale when its node stops running or
    /// rises (a raise pushes a new one); a stale one is dropped when it comes to the top, and
    /// all of them once they outnumber the live ones ([`running_live`]).
    running: BinaryHeap<(u64, u64, u32)>,
    /// How many of its nodes are running.
    running_now: u32,
    nodes: Vec<u32>,
    edges: Vec<Edge>,
    names: String,
    stats: RequestStats,
}

impl Request {
    /// A request's room, made once per request slot: its buffers grow to what its requests
    /// need and are kept for the next request of the slot ([`Request::open`]), never freed.
    fn empty() -> Request {
        Request {
            id: RequestId(0),
            seq: 0,
            weight: 1,
            stride: SHARE,
            keep_going: false,
            idle: false,
            closed: false,
            cancelled: false,
            finished: false,
            release: false,
            open: 0,
            failed: NONE,
            ready: [Vec::new(), Vec::new()], // alloc: none, empty; kept for the slot's next request
            active_at: [NONE; 2],
            vtime: [0; 2],
            running: BinaryHeap::new(),
            running_now: 0,
            nodes: Vec::new(), // alloc: none, empty; kept for the slot's next request
            edges: Vec::new(), // alloc: none, empty; kept for the slot's next request
            names: String::new(), // alloc: none, empty; kept for the slot's next request
            stats: RequestStats::default(),
        }
    }

    /// Opens it in its room, keeping what its buffers hold room for, at `vclock`, the virtual
    /// time of its class.
    fn open(
        &mut self,
        id: RequestId,
        seq: u64,
        weight: u32,
        keep_going: bool,
        idle: bool,
        vclock: [u128; 2],
    ) {
        debug_assert!(self.nodes.is_empty() && self.edges.is_empty() && self.names.is_empty());
        debug_assert!(self.ready.iter().all(Vec::is_empty));
        self.id = id;
        self.seq = seq;
        self.weight = weight;
        self.stride = SHARE / u128::from(weight);
        self.keep_going = keep_going;
        self.idle = idle;
        self.closed = false;
        self.cancelled = false;
        self.finished = false;
        self.release = false;
        self.open = 0;
        self.failed = NONE;
        self.active_at = [NONE; 2];
        self.vtime = vclock;
        debug_assert_eq!(self.running_now, 0);
        self.running.clear();
        self.stats = RequestStats::default();
    }

    fn deps(&self, cold: &NodeCold) -> &[Edge] {
        &self.edges[cold.deps_at as usize..(cold.deps_at + cold.deps) as usize]
    }

    fn name(&self, cold: &NodeCold) -> &str {
        &self.names[cold.name_at as usize..(cold.name_at + cold.name_len) as usize]
    }
}

/// What [`Plane::check`] found of a batch of nodes, for [`Plane::add`]; its buffers are kept
/// for the next batch.
#[derive(Default)]
struct Batch {
    /// Each node's deps, by its place in the batch: a slot, or [`NEW`] and the place of a node
    /// of the batch.
    deps: Vec<u32>,
    /// Where each place's deps begin in `deps`, and the end.
    deps_at: Vec<u32>,
    /// The places in the order they are added, when that is not the order given.
    order: Vec<u32>,
    /// The slot each place took, once added.
    slots: Vec<u32>,
    /// For Kahn's order: each place's deps of the batch not yet placed; the places that wait
    /// for each, as CSR; the places that can go, least first.
    waiting: Vec<u32>,
    then: radix::Csr<u32>,
    /// `then`'s rebuild cursors, kept.
    cursors: Vec<u32>,
    can: BinaryHeap<Reverse<u32>>,
}

impl Batch {
    /// Kahn's order over a batch with a dep on a later node of it, into `order`, the first
    /// given first among those that can go; or the ids of the nodes in a cycle, sorted.
    fn sort(&mut self, nodes: &(impl Given + ?Sized)) -> Result<(), Vec<NodeId>> {
        let len = nodes.len();
        let (deps, deps_at) = (&self.deps, &self.deps_at);
        let of = |at: usize| {
            let deps = &deps[deps_at[at] as usize..deps_at[at + 1] as usize];
            deps.iter().filter(|&&dep| dep & NEW != 0)
        };
        self.waiting.clear();
        self.waiting.extend((0..len).map(|at| of(at).count() as u32));
        let pairs = (0..len).flat_map(|at| of(at).map(move |&dep| (dep & !NEW, at as u32)));
        self.then.rebuild(len, pairs, &mut self.cursors);
        self.can.clear();
        self.can.extend((0..len as u32).filter(|&at| self.waiting[at as usize] == 0).map(Reverse));
        self.order.clear();
        while let Some(Reverse(at)) = self.can.pop() {
            self.order.push(at);
            let at = at as usize;
            for &next in self.then.row(at) {
                let next = next as usize;
                self.waiting[next] -= 1;
                if self.waiting[next] == 0 {
                    self.can.push(Reverse(next as u32));
                }
            }
        }
        if self.order.len() < len {
            let mut cycle: Vec<NodeId> = // alloc: the payload of the batch's refusal
                (0..len).filter(|&at| self.waiting[at] > 0).map(|at| nodes.id(at)).collect();
            radix::sort_copy_by_key(&mut cycle, |it| it.0);
            self.order.clear();
            return Err(cycle);
        }
        Ok(())
    }
}

/// Keys of tasks, by the key's own bits: a [`Key`] hashes as its first word.
type ByKey<V> = ByHash<Key, V>;

/// A node of an open request that has not settled ([`Plane::unsettled`]).
#[derive(Clone, Copy, Debug)]
pub struct Unsettled<'a> {
    pub request: RequestId,
    pub id: NodeId,
    pub name: &'a str,
    pub state: &'a NodeState,
    /// A dep it waits for that has not settled, if any.
    pub on: Option<NodeId>,
}

pub struct Plane<M = NoMemo> {
    config: PlaneConfig,
    /// The node columns, by slot, and which slots are live.
    rows: RegionVec<NodeRow>,
    cold: RegionVec<NodeCold>,
    states: RegionVec<NodeState>,
    keys: RegionVec<Key>,
    node_slots: Slots<NodeSlot>,
    /// Each node by its id: its slot, or while a batch is checked its [`place`].
    index: HashMap<NodeId, NodeHandle>,
    /// The task columns, by slot, and which slots are live.
    tasks: RegionVec<TaskRow>,
    task_keys: RegionVec<Key>,
    task_slots: Slots<TaskSlot>,
    /// Each request's room, by slot, and which slots are live.
    requests: RegionVec<Request>,
    request_slots: Slots<RequestSlot>,
    request_index: HashMap<RequestId, RequestHandle>,
    next_seq: u64,
    /// The database's memo, asked once a keyed node is ready and its key is not in flight.
    memo: M,
    /// Keys of tasks not yet settled.
    flight: ByKey<TaskHandle>,
    running: [usize; 2],
    granted: u64,
    held: u64,
    /// The virtual time of each pool: that of the request not idle that started a task last.
    vclock: [u128; 2],
    /// The same, of the idle requests, which share it among themselves alone.
    idle_vclock: [u128; 2],
    /// The running tasks an idle request started, in the order they started.
    idle_running: Vec<TaskHandle>,
    /// The running tasks asked to give way, of each pool, and the memory they were granted.
    yielding: [usize; 2],
    yielding_memory: u64,
    /// The tasks a step asks to give way, a buffer.
    yields: Vec<TaskHandle>,
    /// The requests whose ready heap of each pool is not empty, in no order.
    active: [Vec<u32>; 2],
    /// The task passed over for memory long enough that it holds back the others.
    hold: Option<TaskHandle>,
    /// The count of passes over tasks ([`TaskRow::stamp`]).
    passes: u32,
    /// Nodes to settle once the move under way is done, as the memo or the budget says, in
    /// order: settling them where they are found would nest without bound.
    deferred: Vec<(u32, NodeState)>,
    last_ns: u64,
    counts: Counts,
    /// Buffers of one step, kept for the next.
    batch: Batch,
    touched: Vec<u32>,
    added: Vec<u32>,
    rising: Vec<(u32, u64)>,
    skipping: Vec<u32>,
    order: Vec<u32>,
    passed: Vec<TaskHandle>,
    scan: BinaryHeap<Reverse<(u64, u64, u32)>>,
    /// The radix sort's room for `touched`.
    sorting: Vec<u32>,
}

fn pool_at(pool: Pool) -> usize {
    match pool {
        Pool::Io => 0,
        Pool::Work => 1,
    }
}

fn gb(bytes: u64) -> String {
    // alloc: for a message of a node refused or held back for memory, once each
    format!("{:.2} GB", bytes as f64 / f64::from(1 << 30))
}

impl Plane {
    /// A plane with no database: every node with a key runs, joined only to a task of its key
    /// that has not settled. For one build of a graph, as [`crate::machine::Machine`] is.
    pub fn new(config: PlaneConfig) -> Result<Plane, Error> {
        Plane::with_memo(config, NoMemo)
    }
}

impl<M: Memo> Plane<M> {
    /// A plane that asks `memo` before it runs a node with a key: one database for the plane's
    /// life, so the ask is a static call.
    pub fn with_memo(config: PlaneConfig, memo: M) -> Result<Plane<M>, Error> {
        for (pool, threads) in [(Pool::Io, config.io), (Pool::Work, config.work)] {
            if threads == 0 {
                return Err(Error::NoThreads(pool));
            }
        }
        Ok(Plane {
            config,
            rows: column(MAX_NODES),
            cold: column(MAX_NODES),
            states: column(MAX_NODES),
            keys: column(MAX_NODES),
            node_slots: Slots::new(MAX_NODES),
            index: HashMap::default(),
            tasks: column(MAX_TASKS),
            task_keys: column(MAX_TASKS),
            task_slots: Slots::new(MAX_TASKS),
            requests: column(MAX_REQUESTS),
            request_slots: Slots::new(MAX_REQUESTS),
            request_index: HashMap::default(),
            next_seq: 0,
            memo,
            flight: ByKey::default(),
            running: [0; 2],
            granted: 0,
            held: 0,
            vclock: [0; 2],
            idle_vclock: [0; 2],
            // Each `Vec` here is empty scratch the plane keeps: it grows to the most a step needs
            // and is reused, so a warm step allocates nothing.
            idle_running: Vec::new(), // alloc: kept scratch
            yielding: [0; 2],
            yielding_memory: 0,
            yields: Vec::new(), // alloc: kept scratch
            active: [Vec::new(), Vec::new()], // alloc: kept scratch
            hold: None,
            passes: 0,
            deferred: Vec::new(), // alloc: kept scratch
            last_ns: 0,
            counts: Counts::default(),
            batch: Batch::default(),
            touched: Vec::new(), // alloc: kept scratch
            added: Vec::new(), // alloc: kept scratch
            rising: Vec::new(), // alloc: kept scratch
            skipping: Vec::new(), // alloc: kept scratch
            order: Vec::new(), // alloc: kept scratch
            passed: Vec::new(), // alloc: kept scratch
            scan: BinaryHeap::new(),
            sorting: Vec::new(), // alloc: kept scratch
        })
    }

    pub fn config(&self) -> &PlaneConfig {
        &self.config
    }

    pub fn counts(&self) -> Counts {
        self.counts
    }

    /// The memory granted to the tasks running now.
    pub fn granted(&self) -> u64 {
        self.granted
    }

    /// The state of `id`, if it was added.
    pub fn state(&self, id: NodeId) -> Option<&NodeState> {
        let node = *self.index.get(&id)?;
        debug_assert!(self.node_slots.live(node), "the index holds a stale handle of {id}");
        Some(&self.states[node.index() as usize])
    }

    /// Whether `request` is known and not finished: one [`Plane::cancel_into`] takes.
    pub fn is_open(&self, request: RequestId) -> bool {
        self.request_index.get(&request).is_some_and(|r| !self.requests[r.index() as usize].finished)
    }

    /// What `request` did so far, if it is known: open, or finished and not released.
    pub fn stats(&self, request: RequestId) -> Option<RequestStats> {
        let r = *self.request_index.get(&request)?;
        debug_assert!(self.request_slots.live(r), "the index holds a stale handle of {request}");
        Some(self.requests[r.index() as usize].stats)
    }

    /// How many node, task and request slots the plane has made, free or not.
    #[cfg(test)]
    pub(crate) fn slots(&self) -> (usize, usize, usize) {
        (self.node_slots.made(), self.task_slots.made(), self.request_slots.made())
    }

    /// The handle of the task `id` is a member of, if it is a node with one: for the tests
    /// that make a handle go stale.
    #[cfg(test)]
    pub(crate) fn task_of(&self, id: NodeId) -> Option<TaskHandle> {
        let node = *self.index.get(&id)?;
        Some(self.rows[node.index() as usize].task).filter(|&task| task != NO_TASK)
    }

    /// Whether `task` names a live task slot.
    #[cfg(test)]
    pub(crate) fn task_live(&self, task: TaskHandle) -> bool {
        self.task_slots.live(task)
    }

    /// A node of the task that holds back every other task with memory until it fits, if one
    /// does.
    pub fn holding(&self) -> Option<NodeId> {
        let hold = self.hold?;
        if !self.task_slots.live(hold) {
            return None;
        }
        let task = &self.tasks[row_of(hold)];
        if task.running || task.first == NONE || !self.wanted(hold) {
            return None;
        }
        Some(self.rows[task.first as usize].id)
    }

    /// Each node of a request not finished that has not settled, to `each`, in no order: what
    /// a stall names.
    pub fn unsettled<'a>(&'a self, mut each: impl FnMut(Unsettled<'a>)) {
        for handle in self.request_index.values() {
            let request = &self.requests[handle.index() as usize];
            if request.finished {
                continue;
            }
            for &me in &request.nodes {
                let m = me as usize;
                let state = &self.states[m];
                if state.settled() {
                    continue;
                }
                let cold = &self.cold[m];
                let on = request
                    .deps(cold)
                    .iter()
                    .find(|edge| !self.states[edge.dep as usize].settled())
                    .map(|edge| self.rows[edge.dep as usize].id);
                each(Unsettled { request: request.id, id: self.rows[m].id, name: request.name(cold), state, on });
            }
        }
    }

    /// Whether a node of `task` is of a request not idle.
    fn wanted(&self, task: TaskHandle) -> bool {
        let mut member = self.tasks[row_of(task)].first;
        while member != NONE {
            let row = &self.rows[member as usize];
            if !self.requests[row.request as usize].idle {
                return true;
            }
            member = row.next_member;
        }
        false
    }

    /// Gives back the room of `request` once it has finished (now, if it has): its nodes are
    /// no longer known, by [`Plane::state`] or as deps, and its id is unknown, so it may be
    /// opened again. What a long-lived plane holds is then what its open requests hold.
    pub fn release(&mut self, request: RequestId) -> Result<(), Error> {
        let &handle = self.request_index.get(&request).ok_or(Error::UnknownRequest(request.0))?;
        let r = handle.index();
        self.requests[r as usize].release = true;
        if self.requests[r as usize].finished {
            self.free_request(r);
        }
        Ok(())
    }

    /// The room of finished request `r` and its nodes, for the next to take: its buffers kept,
    /// its slots freed, so each handle to it or to a node of it is stale from now on. Each of
    /// its nodes is settled, so no task, ready heap or node of another request points at them.
    fn free_request(&mut self, r: u32) {
        let request = &mut self.requests[r as usize];
        debug_assert!(request.finished && request.open == 0);
        let handle = self.request_index.remove(&request.id).expect("a request in the index");
        debug_assert_eq!(handle.index(), r);
        for &me in &request.nodes {
            let m = me as usize;
            debug_assert!(self.states[m].settled() && self.rows[m].task == NO_TASK);
            let node = self.index.remove(&self.rows[m].id).expect("a node of a request in the index");
            debug_assert_eq!(node.index(), me);
            self.node_slots.give(node);
            self.states[m] = NodeState::Waiting;
        }
        request.nodes.clear();
        request.edges.clear();
        request.names.clear();
        self.request_slots.give(handle);
    }

    fn free_task(&mut self, task: TaskHandle) {
        if self.hold == Some(task) {
            self.hold = None;
        }
        self.task_slots.give(task);
    }

    fn seq(&mut self) -> u64 {
        self.next_seq += 1;
        self.next_seq
    }

    pub fn step(&mut self, input: PlaneInput) -> Result<Vec<PlaneOutput>, Error> {
        // alloc: the caller asked for the outputs as their own; `step_into` takes a kept buffer
        let mut out = Vec::new();
        self.step_into(input, &mut out)?;
        Ok(out)
    }

    /// [`Plane::step`], with its outputs appended to `out`: a caller that keeps one buffer and
    /// drains it gets them with no allocation of its own per step. An input refused appends
    /// nothing and changes nothing.
    pub fn step_into(&mut self, input: PlaneInput, out: &mut Vec<PlaneOutput>) -> Result<(), Error> {
        // A time before the last is the last: completions race the clock that stamps them.
        let at = input.at_ns().max(self.last_ns);
        let mut touched = std::mem::take(&mut self.touched);
        let taken = self.take(at, input, out, &mut touched);
        if taken.is_ok() {
            self.after(at, out, &mut touched);
        }
        touched.clear();
        self.touched = touched;
        taken
    }

    fn take(
        &mut self,
        at: u64,
        input: PlaneInput,
        out: &mut Vec<PlaneOutput>,
        touched: &mut Vec<u32>,
    ) -> Result<(), Error> {
        match input {
            PlaneInput::Open { request, weight, keep_going, idle, .. } => {
                if self.request_index.contains_key(&request) {
                    return Err(Error::RequestTwice(request.0));
                }
                if weight == 0 {
                    return Err(Error::NoWeight(request.0));
                }
                let seq = self.seq();
                let handle = self.request_slots.take();
                let r = handle.index();
                if r as usize == self.requests.len() {
                    self.requests.push(Request::empty());
                }
                let vclock = if idle { self.idle_vclock } else { self.vclock };
                self.requests[r as usize].open(request, seq, weight, keep_going, idle, vclock);
                self.request_index.insert(request, handle);
            }
            PlaneInput::Promote { request, .. } => {
                // A request released once it finished is no longer known.
                let Some(&handle) = self.request_index.get(&request) else { return Ok(()) };
                let r = handle.index();
                let it = &mut self.requests[r as usize];
                if it.idle && !it.finished {
                    it.idle = false;
                    it.vtime = self.vclock;
                    let (rows, tasks) = (&self.rows, &self.tasks);
                    self.idle_running.retain(|&task| rows[tasks[row_of(task)].runner as usize].request != r);
                }
            }
            PlaneInput::Yielded { id, .. } => {
                let task = self.running_task(id)?;
                self.yielded(at, task, out, touched);
            }
            PlaneInput::Add { request, nodes, .. } => {
                let r = self.open_request(request)?;
                if self.requests[r as usize].closed {
                    return Err(Error::Closed);
                }
                self.check(r, &nodes[..])?;
                self.add(at, r, nodes, out);
                touched.push(r);
            }
            PlaneInput::AddNodes { request, nodes, .. } => {
                let r = self.open_request(request)?;
                if self.requests[r as usize].closed {
                    return Err(Error::Closed);
                }
                self.check(r, &nodes)?;
                // The batch's record goes before any of its nodes moves; it takes the columns
                // once the nodes are added from them, into the place kept for it.
                let record_at = out.len();
                if !nodes.is_empty() {
                    // Empty, so allocating nothing: the place the record takes below.
                    let kept = Event::Nodes {
                        first: 0,
                        kinds: Vec::new(), // alloc: none, empty
                        names: String::new(), // alloc: none, empty
                        name_at: Vec::new(), // alloc: none, empty
                        deps: Vec::new(), // alloc: none, empty
                        deps_at: Vec::new(), // alloc: none, empty
                    };
                    out.push(said(request, at, kept));
                    self.add_with(at, r, nodes.len(), |place| nodes.parts(place), out);
                    out[record_at] = said(request, at, nodes.record());
                }
                touched.push(r);
            }
            PlaneInput::Close { request, .. } => {
                let r = self.open_request(request)?;
                if self.requests[r as usize].closed {
                    return Err(Error::Closed);
                }
                self.requests[r as usize].closed = true;
                touched.push(r);
            }
            PlaneInput::Done { id, outcome, added, group, .. } => {
                let task = self.running_task(id)?;
                let runner = self.tasks[row_of(task)].runner;
                let r = self.rows[runner as usize].request;
                self.check(r, &added[..])?;
                self.finish_task(at, task, outcome, group, out, touched);
                self.add(at, r, added, out);
                touched.push(r);
            }
            PlaneInput::Held { bytes, .. } => self.held = bytes,
        }
        Ok(())
    }

    /// The task the plane asked to run as `id`, running. A `Done` or `Yielded` of a task that
    /// has ended names no running task, whatever took its slot since: the node's handle and
    /// then its task's are checked against their slots' generations before either is read.
    fn running_task(&self, id: NodeId) -> Result<TaskHandle, Error> {
        let refused = Error::NotRunning(id);
        let Some(&node) = self.index.get(&id) else { return Err(refused) };
        if !self.node_slots.live(node) {
            return Err(refused);
        }
        let me = node.index();
        if !matches!(self.states[me as usize], NodeState::Running) {
            return Err(refused);
        }
        let task = self.rows[me as usize].task;
        if !self.task_slots.live(task) {
            return Err(refused);
        }
        let t = &self.tasks[row_of(task)];
        if !(t.running && t.runner == me) {
            return Err(refused);
        }
        Ok(task)
    }

    /// Cancels `request`: each of its nodes not started is skipped, because of the request's
    /// first node (the protocol has no state of its own for it yet), after a note that says
    /// the request is cancelled. A task it runs is let finish, and its [`PlaneInput::Done`] is
    /// taken as ever: its nodes of other requests settle with it, and what it finds is added
    /// skipped. A node of it that joined another request's running task leaves it, skipped,
    /// and that task runs on for the rest. Once what it runs is done the request finishes,
    /// not ok. It takes no more [`PlaneInput::Add`] or [`PlaneInput::Close`]; a second cancel
    /// does nothing.
    pub fn cancel(&mut self, at_ns: u64, request: RequestId) -> Result<Vec<PlaneOutput>, Error> {
        // alloc: as for `step`
        let mut out = Vec::new();
        self.cancel_into(at_ns, request, &mut out)?;
        Ok(out)
    }

    /// [`Plane::cancel`], with its outputs appended to `out`, as [`Plane::step_into`].
    pub fn cancel_into(
        &mut self,
        at_ns: u64,
        request: RequestId,
        out: &mut Vec<PlaneOutput>,
    ) -> Result<(), Error> {
        let at = at_ns.max(self.last_ns);
        let r = self.open_request(request)?;
        let ri = r as usize;
        if !self.requests[ri].cancelled {
            self.requests[ri].cancelled = true;
            self.requests[ri].closed = true;
            out.push(said(
                request,
                at,
                // alloc: the payload of a request's one cancel note
                Event::Said { level: Level::Note, text: format!("{request} is cancelled") },
            ));
            if let Some(&first) = self.requests[ri].nodes.first() {
                let because = self.rows[first as usize].id.0;
                for at_node in 0..self.requests[ri].nodes.len() {
                    let me = self.requests[ri].nodes[at_node];
                    let skip = match self.states[me as usize] {
                        NodeState::Waiting | NodeState::Ready => true,
                        NodeState::Running => {
                            let task = self.rows[me as usize].task;
                            task != NO_TASK && self.tasks[row_of(task)].runner != me
                        }
                        _ => false,
                    };
                    if skip {
                        self.leave_task(me);
                        self.moved(at, me, NodeState::Skipped { because }, out);
                    }
                }
            }
        }
        let mut touched = std::mem::take(&mut self.touched);
        touched.push(r);
        self.after(at, out, &mut touched);
        touched.clear();
        self.touched = touched;
        Ok(())
    }

    /// What follows any input: the deferred nodes settled, the threads given work, and the
    /// requests `touched` that are over finished.
    fn after(&mut self, at: u64, out: &mut Vec<PlaneOutput>, touched: &mut Vec<u32>) {
        self.settle_deferred(at, out, touched);
        self.last_ns = at;
        self.dispatch(at, out, touched);
        let requests = &self.requests;
        radix::sort_copy_by_key_in(
            touched,
            |&r| radix::pack_64(requests[r as usize].seq, u64::from(r)),
            &mut self.sorting,
        );
        touched.dedup();
        for &r in touched.iter() {
            let request = &self.requests[r as usize];
            if request.closed && request.open == 0 && !request.finished {
                self.requests[r as usize].finished = true;
                let path = self.critical(r);
                let request = &self.requests[r as usize];
                let id = request.id;
                let ok = request.failed == NONE && !request.cancelled;
                let release = request.release;
                out.push(said(id, at, Event::Critical { path }));
                out.push(PlaneOutput::Finished { request: id, ok });
                if release {
                    self.free_request(r);
                }
            }
        }
    }

    fn open_request(&self, request: RequestId) -> Result<u32, Error> {
        match self.request_index.get(&request).map(|it| it.index()) {
            Some(r) if !self.requests[r as usize].finished => Ok(r),
            Some(_) => Err(Error::Over),
            None => Err(Error::UnknownRequest(request.0)),
        }
    }

    /// Whether `nodes` can be added to request `r`: none of their ids known or given twice,
    /// each dep one of them or a node of `r`, and no cycle among them. If so, their ids are in
    /// the index as places of the batch ([`place`]), and [`Plane::batch`] says each one's deps
    /// and the order to add them in: each after its deps of them, the first given first among
    /// those that can go. If not, nothing is changed.
    fn check(&mut self, r: u32, nodes: &(impl Given + ?Sized)) -> Result<(), Error> {
        let batch = &mut self.batch;
        batch.deps.clear();
        batch.deps_at.clear();
        batch.order.clear();
        let len = nodes.len();
        if len == 0 {
            return Ok(());
        }
        assert!(len < NEW as usize, "{len} nodes in one input");
        for at in 0..len {
            let id = nodes.id(at);
            let fresh = match self.index.entry(id) {
                Entry::Occupied(_) => false,
                Entry::Vacant(it) => {
                    it.insert(place(at));
                    true
                }
            };
            if !fresh {
                unindex(&mut self.index, nodes, at);
                return Err(Error::Twice(id));
            }
        }
        let mut in_order = true;
        for at in 0..len {
            batch.deps_at.push(batch.deps.len() as u32);
            for dep in nodes.deps(at) {
                let found = self.index.get(dep).map(|&it| (it, placed(it)));
                match found {
                    Some((_, Some(place))) => {
                        in_order &= place < at as u32;
                        batch.deps.push(NEW | place);
                    }
                    Some((it, None)) if self.rows[it.index() as usize].request == r => {
                        debug_assert!(self.node_slots.live(it), "the index holds a stale handle of {dep}");
                        batch.deps.push(it.index());
                    }
                    _ => {
                        unindex(&mut self.index, nodes, len);
                        return Err(Error::UnknownDep { node: nodes.id(at), dep: *dep });
                    }
                }
            }
        }
        batch.deps_at.push(batch.deps.len() as u32);
        // Given each after its deps of them, the order is the order given.
        if !in_order && let Err(cycle) = batch.sort(nodes) {
            unindex(&mut self.index, nodes, len);
            return Err(Error::Cycle(cycle));
        }
        Ok(())
    }

    /// Adds the nodes [`Plane::check`] took to request `r`.
    fn add(&mut self, at: u64, r: u32, mut nodes: Vec<Spec>, out: &mut Vec<PlaneOutput>) {
        if !self.batch.order.is_empty() {
            permute(&mut nodes, &mut self.batch.order);
        }
        let len = nodes.len();
        let mut nodes = nodes.into_iter();
        self.add_with(at, r, len, |_| Parts::from(nodes.next().expect("a node a place")), out);
    }

    /// Adds the `len` nodes [`Plane::check`] took to request `r`, in its order: `part` gives the
    /// node at each place, called once a place.
    fn add_with<'p>(
        &mut self,
        at: u64,
        r: u32,
        len: usize,
        mut part: impl FnMut(usize) -> Parts<'p>,
        out: &mut Vec<PlaneOutput>,
    ) {
        if len == 0 {
            return;
        }
        let mut batch = std::mem::take(&mut self.batch);
        batch.slots.clear();
        batch.slots.resize(len, NONE);
        let mut added = std::mem::take(&mut self.added);
        added.clear();
        for at_node in 0..len {
            let place = match batch.order.get(at_node) {
                Some(&place) => (place & !NEW) as usize,
                None => at_node,
            };
            let me = self.add_one(at, r, part(place), &batch, place, out);
            batch.slots[place] = me;
            added.push(me);
        }
        self.batch = batch;
        // A node's priority is its cost and the most of its dependents'. A node is added after
        // its deps, so from the last added to the first each new one's dependents, all new,
        // have theirs before it. An older node's priority can only rise, and only through a
        // dep of a new one: what rises is carried up through its deps, and no further.
        for &me in added.iter().rev() {
            let edges = &self.requests[r as usize].edges;
            let row = &self.rows[me as usize];
            let mut after = 0;
            let mut edge = row.first_out;
            while edge != NONE {
                let it = edges[edge as usize];
                after = after.max(self.rows[it.owner as usize].priority);
                edge = it.next;
            }
            let priority = row.cost.saturating_add(after);
            self.raise(me, priority);
        }
        let mut rising = std::mem::take(&mut self.rising);
        rising.clear();
        let request = &self.requests[r as usize];
        for &me in &added {
            let priority = self.rows[me as usize].priority;
            let deps = request.deps(&self.cold[me as usize]);
            rising.extend(deps.iter().map(|it| (it.dep, priority)));
        }
        while let Some((me, after)) = rising.pop() {
            let priority = self.rows[me as usize].cost.saturating_add(after);
            if priority <= self.rows[me as usize].priority {
                continue;
            }
            self.raise(me, priority);
            let deps = self.requests[r as usize].deps(&self.cold[me as usize]);
            rising.extend(deps.iter().map(|it| (it.dep, priority)));
        }
        self.rising = rising;
        self.added = added;
    }

    /// Adds the node at `place` of the batch to request `r`, each of its deps added before it:
    /// its slot.
    fn add_one(
        &mut self,
        at: u64,
        r: u32,
        parts: Parts<'_>,
        batch: &Batch,
        place: usize,
        out: &mut Vec<PlaneOutput>,
    ) -> u32 {
        let Parts { id, kind, name, deps, pool, cost, key, memory } = parts;
        let handle = self.node_slots.take();
        let me = handle.index();
        if me as usize == self.rows.len() {
            // A slot not made before: its rows, filled below.
            self.rows.push(NodeRow {
                priority: 0,
                seq: 0,
                cost: 0,
                id,
                request: r,
                waiting: 0,
                heap_at: NONE,
                next_member: NONE,
                first_out: NONE,
                last_out: NONE,
                task: NO_TASK,
            });
            self.cold.push(NodeCold {
                memory: 0,
                done_at: 0,
                started_at: 0,
                path_ns: 0,
                deps_at: 0,
                deps: 0,
                blocker: NONE,
                group: 0,
                name_at: 0,
                name_len: 0,
                pool,
                flags: 0,
                kind,
            });
            self.states.push(NodeState::Waiting);
            self.keys.push(Key([0; 32]));
        }
        debug_assert_eq!(self.rows.len(), self.node_slots.made());
        let request = &mut self.requests[r as usize];
        let deps_at = request.edges.len() as u32;
        let mut waiting = 0;
        // The id of the node it is skipped because of.
        let mut skipped: Option<u32> = None;
        let mut blocker = NONE;
        let later = |rows: &[NodeRow], cold: &[NodeCold], it: u32| {
            (cold[it as usize].done_at, rows[it as usize].seq)
        };
        for &dep in &batch.deps[batch.deps_at[place] as usize..batch.deps_at[place + 1] as usize] {
            let dep = if dep & NEW != 0 { batch.slots[(dep & !NEW) as usize] } else { dep };
            match &self.states[dep as usize] {
                NodeState::Built | NodeState::Cached { .. } => {
                    if blocker == NONE
                        || later(&self.rows, &self.cold, dep) > later(&self.rows, &self.cold, blocker)
                    {
                        blocker = dep;
                    }
                }
                NodeState::Failed { .. } => _ = skipped.get_or_insert(self.rows[dep as usize].id.0),
                NodeState::Skipped { because } => _ = skipped.get_or_insert(*because),
                _ => waiting += 1,
            }
            let edge = request.edges.len() as u32;
            request.edges.push(Edge { dep, owner: me, next: NONE });
            let row = &mut self.rows[dep as usize];
            if row.last_out == NONE {
                row.first_out = edge;
            } else {
                request.edges[row.last_out as usize].next = edge;
            }
            row.last_out = edge;
        }
        let deps_len = request.edges.len() as u32 - deps_at;
        if !request.keep_going && skipped.is_none() && request.failed != NONE {
            skipped = Some(self.rows[request.failed as usize].id.0);
        }
        if request.cancelled
            && skipped.is_none()
            && let Some(&first) = request.nodes.first()
        {
            skipped = Some(self.rows[first as usize].id.0);
        }
        let name_at = u32::try_from(request.names.len()).expect("a request's names under 4 GiB");
        request.names.push_str(&name);
        request.nodes.push(me);
        request.open += 1;
        if let Some(deps) = deps {
            let name = name.into_owned();
            out.push(said(request.id, at, Event::Node { id: id.0, kind, name, deps }));
        }
        *self.index.get_mut(&id).expect("a checked node in the index") = handle;
        let seq = self.seq();
        let m = me as usize;
        self.rows[m] = NodeRow {
            priority: cost,
            seq,
            cost,
            id,
            request: r,
            waiting,
            heap_at: NONE,
            next_member: NONE,
            first_out: NONE,
            last_out: NONE,
            task: NO_TASK,
        };
        self.cold[m] = NodeCold {
            memory,
            done_at: 0,
            started_at: 0,
            path_ns: 0,
            deps_at,
            deps: deps_len,
            blocker,
            group: 0,
            name_at,
            name_len: self.requests[r as usize].names.len() as u32 - name_at,
            pool,
            flags: if key.is_some() { KEYED } else { 0 },
            kind,
        };
        self.keys[m] = key.unwrap_or(Key([0; 32]));
        self.states[m] = NodeState::Waiting;
        match skipped {
            Some(because) => self.moved(at, me, NodeState::Skipped { because }, out),
            None if waiting == 0 => self.became_ready(at, me, out),
            None => self.moved(at, me, NodeState::Waiting, out),
        }
        me
    }

    /// Sets `me`'s priority, higher than it was, keeping its place among its request's ready
    /// nodes.
    fn raise(&mut self, me: u32, priority: u64) {
        let row = &mut self.rows[me as usize];
        if row.priority == priority {
            return;
        }
        debug_assert!(row.priority < priority, "a priority only rises");
        row.priority = priority;
        let (at, r, task) = (row.heap_at, row.request, row.task);
        if at == NONE {
            if self.states[me as usize] == NodeState::Running {
                self.track_running(me);
            }
            return;
        }
        let p = pool_at(self.tasks[row_of(task)].pool);
        let heap = &mut self.requests[r as usize].ready[p];
        heap[at as usize].rank = !priority;
        sift_up(heap, &mut self.rows, at as usize);
    }

    /// Says `me` is now `state`, and keeps what follows from it for its request.
    fn moved(&mut self, at: u64, me: u32, state: NodeState, out: &mut Vec<PlaneOutput>) {
        let row = &self.rows[me as usize];
        let request = &mut self.requests[row.request as usize];
        if state.settled() {
            request.open -= 1;
        }
        let was = self.states[me as usize] == NodeState::Running;
        let now = state == NodeState::Running;
        if was && !now {
            request.running_now -= 1;
        }
        // clone: a `NodeState` but `Failed` holds no heap; the event owns its copy
        out.push(said(request.id, at, Event::Moved { id: row.id.0, state: state.clone() }));
        self.states[me as usize] = state;
        if now && !was {
            self.requests[self.rows[me as usize].request as usize].running_now += 1;
            self.track_running(me);
        }
    }

    /// Puts running `me` at its priority among its request's running nodes.
    fn track_running(&mut self, me: u32) {
        let row = self.rows[me as usize];
        let request = &mut self.requests[row.request as usize];
        request.running.push((row.priority, row.seq, me));
        if request.running.len() > 2 * request.running_now as usize + 16 {
            let (rows, states) = (&self.rows, &self.states);
            request.running.retain(|&it| running_live(rows, states, it));
        }
    }

    /// Whether the task `me` starts is on its request's critical path: its priority is the
    /// most of its request's ready and running nodes. Called once `me` runs.
    fn on_critical_path(&mut self, me: u32) -> bool {
        let row = self.rows[me as usize];
        let request = &mut self.requests[row.request as usize];
        let (rows, states) = (&self.rows, &self.states);
        let running = loop {
            match request.running.peek().copied() {
                Some(top) if !running_live(rows, states, top) => _ = request.running.pop(),
                top => break top.map_or(0, |it| it.0),
            }
        };
        let ready = request.ready.iter().filter_map(|it| it.first()).map(|it| !it.rank).max();
        row.priority >= running.max(ready.unwrap_or(0))
    }

    /// Puts `me`, of a task not running, among its request's ready nodes of pool `p`. A
    /// request that had none comes back at the pool's virtual time of its class: what it did
    /// not use while it had nothing to run is not owed to it.
    fn make_ready(&mut self, me: u32, p: usize) {
        let row = self.rows[me as usize];
        let request = &mut self.requests[row.request as usize];
        if request.ready[p].is_empty() {
            let clock = if request.idle { self.idle_vclock[p] } else { self.vclock[p] };
            request.vtime[p] = request.vtime[p].max(clock);
            request.active_at[p] = self.active[p].len() as u32;
            self.active[p].push(row.request);
        }
        let entry = Ready { rank: !row.priority, seq: row.seq, node: me };
        let heap = &mut request.ready[p];
        heap.push(entry);
        let last = heap.len() - 1;
        sift_up(heap, &mut self.rows, last);
    }

    /// Takes `me` from its request's ready nodes of pool `p`.
    fn unready(&mut self, me: u32, p: usize) {
        let row = &self.rows[me as usize];
        let (r, at) = (row.request, row.heap_at);
        debug_assert!(at != NONE, "a node not ready taken from its ready nodes");
        let request = &mut self.requests[r as usize];
        heap_remove(&mut request.ready[p], &mut self.rows, at as usize);
        if request.ready[p].is_empty() {
            let place = request.active_at[p];
            request.active_at[p] = NONE;
            self.active[p].swap_remove(place as usize);
            if let Some(&moved) = self.active[p].get(place as usize) {
                self.requests[moved as usize].active_at[p] = place;
            }
        }
    }

    /// `me`'s deps are done: it joins a task of its key, is settled by what the database has
    /// of the key, or is a task of its own, ready to run.
    fn became_ready(&mut self, at: u64, me: u32, out: &mut Vec<PlaneOutput>) {
        let m = me as usize;
        let cold = self.cold[m];
        let r = self.rows[m].request as usize;
        let task = if cold.flags & KEYED == 0 {
            if let Some(why) = too_big(&self.config, &self.requests[r], &cold) {
                self.deferred.push((me, NodeState::Failed { why }));
                return;
            }
            new_task(&mut self.tasks, &mut self.task_keys, &mut self.task_slots, me, &cold, None)
        } else {
            let key = self.keys[m];
            // A key in flight is not yet in the database: its task puts it there before its
            // `Done`, so asking first would only ask in vain.
            let vacant = match self.flight.entry(key) {
                Entry::Occupied(it) => {
                    let task = *it.get();
                    debug_assert!(self.task_slots.live(task), "a key in flight names a task that ended");
                    self.join(at, me, task, out);
                    return;
                }
                Entry::Vacant(it) => it,
            };
            if let Some(outcome) = self.memo.settled(key) {
                self.counts.remembered += 1;
                self.requests[r].stats.cached += 1;
                let state = match outcome {
                    Outcome::Built => NodeState::Cached { remote: false },
                    Outcome::Cached { remote } => NodeState::Cached { remote },
                    Outcome::Failed { why } => NodeState::Failed { why },
                };
                self.deferred.push((me, state));
                return;
            }
            if let Some(why) = too_big(&self.config, &self.requests[r], &cold) {
                self.deferred.push((me, NodeState::Failed { why }));
                return;
            }
            let task = new_task(
                &mut self.tasks,
                &mut self.task_keys,
                &mut self.task_slots,
                me,
                &cold,
                Some(key),
            );
            vacant.insert(task);
            task
        };
        self.rows[m].task = task;
        self.make_ready(me, pool_at(cold.pool));
        self.moved(at, me, NodeState::Ready, out);
    }

    /// `me` joins `task`, of its key, as its last member.
    fn join(&mut self, at: u64, me: u32, task: TaskHandle, out: &mut Vec<PlaneOutput>) {
        self.counts.joined += 1;
        let t = &mut self.tasks[row_of(task)];
        debug_assert!(t.last != NONE, "a task in flight has a member");
        self.rows[t.last as usize].next_member = me;
        t.last = me;
        let (running, pool) = (t.running, t.pool);
        let row = &mut self.rows[me as usize];
        row.next_member = NONE;
        row.task = task;
        if running {
            self.moved(at, me, NodeState::Running, out);
        } else {
            self.make_ready(me, pool_at(pool));
            self.moved(at, me, NodeState::Ready, out);
        }
    }

    fn settle_deferred(&mut self, at: u64, out: &mut Vec<PlaneOutput>, touched: &mut Vec<u32>) {
        let mut next = 0;
        while next < self.deferred.len() {
            let me = self.deferred[next].0;
            let state = std::mem::replace(&mut self.deferred[next].1, NodeState::Waiting);
            next += 1;
            // Skipped by a failure of the same step before it was settled.
            if self.states[me as usize].settled() {
                continue;
            }
            touched.push(self.rows[me as usize].request);
            self.settle(at, me, state, out);
        }
        self.deferred.clear();
    }

    fn settle(&mut self, at: u64, me: u32, state: NodeState, out: &mut Vec<PlaneOutput>) {
        let failed = matches!(state, NodeState::Failed { .. });
        let m = me as usize;
        let (row, cold) = (self.rows[m], self.cold[m]);
        let r = row.request as usize;
        // Each dep settled before `me` was ready, so its path is known.
        let own = if cold.flags & STARTED != 0 { at.saturating_sub(cold.started_at) } else { 0 };
        let request = &mut self.requests[r];
        let before = request.deps(&cold).iter().map(|it| self.cold[it.dep as usize].path_ns).max();
        let path = own.saturating_add(before.unwrap_or(0));
        request.stats.critical_path_ns = request.stats.critical_path_ns.max(path);
        let step = &mut request.stats.steps_ns[cold.kind as usize];
        *step = step.saturating_add(own);
        self.cold[m].path_ns = path;
        self.cold[m].done_at = at;
        self.cold[m].flags |= DONE;
        self.moved(at, me, state, out);
        if !failed {
            let mut edge = row.first_out;
            while edge != NONE {
                let it = self.requests[r].edges[edge as usize];
                edge = it.next;
                let dependent = &mut self.rows[it.owner as usize];
                dependent.waiting -= 1;
                if dependent.waiting == 0 && matches!(self.states[it.owner as usize], NodeState::Waiting) {
                    self.cold[it.owner as usize].blocker = me;
                    self.became_ready(at, it.owner, out);
                }
            }
            return;
        }
        let because = row.id.0;
        let request = &mut self.requests[r];
        if request.failed == NONE {
            request.failed = me;
        }
        let mut skipping = std::mem::take(&mut self.skipping);
        skipping.clear();
        push_dependents(&mut skipping, &self.requests[r].edges, row.first_out);
        if !self.requests[r].keep_going {
            skipping.extend_from_slice(&self.requests[r].nodes);
        }
        let mut at_skipping = 0;
        while let Some(&it) = skipping.get(at_skipping) {
            at_skipping += 1;
            if matches!(self.states[it as usize], NodeState::Waiting | NodeState::Ready) {
                self.leave_task(it);
                self.moved(at, it, NodeState::Skipped { because }, out);
                let first_out = self.rows[it as usize].first_out;
                push_dependents(&mut skipping, &self.requests[r].edges, first_out);
            }
        }
        self.skipping = skipping;
    }

    /// `me` no longer waits for its task: a node of a task not running is no longer ready, and
    /// a task with no node left is dropped. A running task runs on without it; its runner
    /// never leaves it.
    fn leave_task(&mut self, me: u32) {
        let Some((task, t)) = self.unlink(me) else { return };
        debug_assert!(!t.running || t.runner != me, "the runner of a running task left it");
        if t.running {
            return;
        }
        self.unready(me, pool_at(t.pool));
        self.drop_if_empty(task);
    }

    /// Takes `me` out of its task's members, if it has a task: the task, as it was.
    fn unlink(&mut self, me: u32) -> Option<(TaskHandle, TaskRow)> {
        let m = me as usize;
        let task = self.rows[m].task;
        if task == NO_TASK {
            return None;
        }
        debug_assert!(self.task_slots.live(task), "node {} names a task that ended", self.rows[m].id);
        self.rows[m].task = NO_TASK;
        let t = self.tasks[row_of(task)];
        let next = self.rows[m].next_member;
        self.rows[m].next_member = NONE;
        let mut before = NONE;
        let mut member = t.first;
        while member != me {
            before = member;
            member = self.rows[member as usize].next_member;
        }
        if before == NONE {
            self.tasks[row_of(task)].first = next;
        } else {
            self.rows[before as usize].next_member = next;
        }
        if t.last == me {
            self.tasks[row_of(task)].last = before;
        }
        Some((task, t))
    }

    /// Drops `task`, not running, once no node is left of it.
    fn drop_if_empty(&mut self, task: TaskHandle) {
        let t = self.tasks[row_of(task)];
        if t.first == NONE {
            if t.keyed {
                let key = self.task_keys[row_of(task)];
                self.flight.remove(&key);
            }
            self.free_task(task);
        }
    }

    /// Running `task` gave way ([`PlaneInput::Yielded`]): its thread and memory are free, it
    /// is no start of its runner's request, and each of its nodes is ready again in its own
    /// request at its own priority, or skipped as a node added to that request now would be.
    fn yielded(
        &mut self,
        at: u64,
        task: TaskHandle,
        out: &mut Vec<PlaneOutput>,
        touched: &mut Vec<u32>,
    ) {
        let t = self.tasks[row_of(task)];
        let p = pool_at(t.pool);
        self.running[p] -= 1;
        self.granted -= t.memory;
        if t.yielding {
            self.yielding[p] -= 1;
            self.yielding_memory -= t.memory;
        }
        self.idle_running.retain(|&it| it != task);
        self.tasks[row_of(task)].running = false;
        self.tasks[row_of(task)].yielding = false;
        let runner = t.runner as usize;
        self.requests[self.rows[runner].request as usize].stats.ran -= 1;
        self.cold[runner].flags &= !STARTED;
        let mut member = t.first;
        while member != NONE {
            let next = self.rows[member as usize].next_member;
            let r = self.rows[member as usize].request;
            touched.push(r);
            let request = &self.requests[r as usize];
            let because = if !request.keep_going && request.failed != NONE {
                Some(self.rows[request.failed as usize].id.0)
            } else if request.cancelled {
                request.nodes.first().map(|&it| self.rows[it as usize].id.0)
            } else {
                None
            };
            match because {
                Some(because) => {
                    self.unlink(member);
                    self.moved(at, member, NodeState::Skipped { because }, out);
                }
                None => {
                    self.make_ready(member, p);
                    self.moved(at, member, NodeState::Ready, out);
                }
            }
            member = next;
        }
        self.drop_if_empty(task);
    }

    fn finish_task(
        &mut self,
        at: u64,
        task: TaskHandle,
        outcome: Outcome,
        group: Option<u32>,
        out: &mut Vec<PlaneOutput>,
        touched: &mut Vec<u32>,
    ) {
        let t = self.tasks[row_of(task)];
        self.tasks[row_of(task)].running = false;
        let p = pool_at(t.pool);
        self.running[p] -= 1;
        self.granted -= t.memory;
        if t.yielding {
            self.tasks[row_of(task)].yielding = false;
            self.yielding[p] -= 1;
            self.yielding_memory -= t.memory;
        }
        if !self.idle_running.is_empty() {
            self.idle_running.retain(|&it| it != task);
        }
        if t.keyed {
            let key = self.task_keys[row_of(task)];
            self.flight.remove(&key);
        }
        self.free_task(task);
        let mut state = match outcome {
            Outcome::Built => NodeState::Built,
            Outcome::Cached { remote } => NodeState::Cached { remote },
            Outcome::Failed { why } => NodeState::Failed { why },
        };
        let mut member = t.first;
        while member != NONE {
            let m = member as usize;
            let next = self.rows[m].next_member;
            let r = self.rows[m].request;
            if member != t.runner {
                self.requests[r as usize].stats.joined += 1;
            }
            let cold = &mut self.cold[m];
            match group {
                Some(group) => {
                    cold.group = group;
                    cold.flags |= GROUP;
                }
                None => cold.flags &= !GROUP,
            }
            self.rows[m].task = NO_TASK;
            self.rows[m].next_member = NONE;
            touched.push(r);
            // Every member is running: a node that leaves a running task is taken out of it.
            debug_assert_eq!(self.states[m], NodeState::Running);
            let state = // clone: each member settles with the outcome; the last takes it
                if next == NONE { std::mem::replace(&mut state, NodeState::Waiting) } else { state.clone() };
            self.settle(at, member, state, out);
            member = next;
        }
    }

    fn memory_free(&self) -> Option<u64> {
        let budget = self.config.budget.as_ref()?;
        Some(budget.bytes.saturating_sub(self.granted.max(self.held)))
    }

    /// The task that holds back the others, while it is still to start and wanted by a
    /// request not idle.
    fn hold(&mut self) -> Option<TaskHandle> {
        if let Some(task) = self.hold
            && (!self.task_slots.live(task)
                || self.tasks[row_of(task)].running
                || self.tasks[row_of(task)].first == NONE
                || !self.wanted(task))
        {
            self.hold = None;
        }
        self.hold
    }

    /// The request of `active` whose turn it is in pool `p`: one not idle, then the least
    /// virtual time, then the heavier, then the first opened.
    fn turn(&self, r: u32, p: usize) -> (bool, u128, Reverse<u32>, u64) {
        let request = &self.requests[r as usize];
        (request.idle, request.vtime[p], Reverse(request.weight), request.seq)
    }

    fn dispatch(&mut self, at: u64, out: &mut Vec<PlaneOutput>, touched: &mut Vec<u32>) {
        for pool in [Pool::Io, Pool::Work] {
            let (p, limit) = (pool_at(pool), self.threads(pool));
            if self.config.budget.is_none() {
                // With no budget the first ready node of the request whose turn it is starts.
                while self.running[p] < limit {
                    let Some(r) = self.active[p].iter().copied().min_by_key(|&r| self.turn(r, p))
                    else {
                        break;
                    };
                    if self.requests[r as usize].idle && self.yield_for(p, out) {
                        break;
                    }
                    let me = self.requests[r as usize].ready[p][0].node;
                    self.start(at, me, out);
                }
                self.yield_for(p, out);
                continue;
            }
            while self.running[p] < limit {
                let mut order = std::mem::take(&mut self.order);
                order.clear();
                order.extend_from_slice(&self.active[p]);
                order.sort_unstable_by_key(|&r| self.turn(r, p));
                let mut passed = std::mem::take(&mut self.passed);
                passed.clear();
                let mut picked = Pick::None;
                for &r in &order {
                    picked = self.pick(r, p, &mut passed);
                    if !matches!(picked, Pick::None) {
                        break;
                    }
                }
                self.order = order;
                if let Pick::Start(me) = picked
                    && self.requests[self.rows[me as usize].request as usize].idle
                    && self.yield_for(p, out)
                {
                    self.passed = passed;
                    break;
                }
                match picked {
                    Pick::Start(me) => {
                        let task = self.rows[me as usize].task;
                        if self.tasks[row_of(task)].memory > 0 {
                            self.pass(&passed);
                        }
                        self.passed = passed;
                        self.start(at, me, out);
                    }
                    Pick::Fail(me, why) => {
                        self.passed = passed;
                        self.leave_task(me);
                        touched.push(self.rows[me as usize].request);
                        self.deferred.push((me, NodeState::Failed { why }));
                        self.settle_deferred(at, out, touched);
                    }
                    Pick::None => {
                        self.passed = passed;
                        break;
                    }
                }
            }
            self.yield_for(p, out);
        }
    }

    /// Asks running idle tasks to give way, the newest first, when a task of a request not
    /// idle that would start next in pool `p` waits for nothing but their threads or memory,
    /// and as few as that takes: none, if what is already giving way is enough, or if giving
    /// all of them would not be. Memory counts what is granted; a task waiting for what the
    /// process holds beyond that waits for no idle task. Whether such a task waits for idle
    /// tasks to give way, so no other idle task may start in `p` now.
    fn yield_for(&mut self, p: usize, out: &mut Vec<PlaneOutput>) -> bool {
        if self.idle_running.is_empty() {
            return false;
        }
        let Some(wants) = self.waiting(p) else { return false };
        let mut thread = self.running[p] - self.yielding[p] >= self.threads_of(p);
        let mut memory = 0;
        if let Some(budget) = &self.config.budget {
            let needs = self.tasks[row_of(wants)].memory;
            if needs > budget.bytes.saturating_sub(self.held) {
                return false;
            }
            memory = (self.granted - self.yielding_memory).saturating_sub(budget.bytes - needs);
        }
        if !thread && memory == 0 {
            return true;
        }
        let mut yields = std::mem::take(&mut self.yields);
        yields.clear();
        for &task in self.idle_running.iter().rev() {
            if !thread && memory == 0 {
                break;
            }
            let t = &self.tasks[row_of(task)];
            if t.yielding || self.wanted(task) {
                continue;
            }
            let frees_thread = thread && pool_at(t.pool) == p;
            let frees_memory = memory > 0 && t.memory > 0;
            if frees_thread || frees_memory {
                yields.push(task);
                thread &= !frees_thread;
                memory = memory.saturating_sub(t.memory);
            }
        }
        let enough = !thread && memory == 0;
        if enough {
            for &task in &yields {
                let t = &mut self.tasks[row_of(task)];
                t.yielding = true;
                self.yielding[pool_at(t.pool)] += 1;
                self.yielding_memory += t.memory;
                out.push(PlaneOutput::Yield { id: self.rows[t.runner as usize].id });
            }
        }
        self.yields = yields;
        enough
    }

    /// The task of a request not idle that starts next in pool `p` once it can: the held
    /// task, if it is of `p`; else the first ready node of the request not idle whose turn it
    /// is, unless the held task holds it back.
    fn waiting(&mut self, p: usize) -> Option<TaskHandle> {
        let hold = self.hold();
        if let Some(held) = hold
            && pool_at(self.tasks[row_of(held)].pool) == p
        {
            return Some(held);
        }
        let r = self.active[p]
            .iter()
            .copied()
            .filter(|&r| !self.requests[r as usize].idle)
            .min_by_key(|&r| self.turn(r, p))?;
        let task = self.rows[self.requests[r as usize].ready[p][0].node as usize].task;
        match hold {
            Some(held) if held != task && self.tasks[row_of(task)].memory > 0 => None,
            _ => Some(task),
        }
    }

    fn threads_of(&self, p: usize) -> usize {
        if p == 0 { self.config.io } else { self.config.work }
    }

    /// Each of the tasks `passed` over was so once more; the first of them to have been so
    /// [`PlaneConfig::starve_after`] times holds back the rest, if none does.
    fn pass(&mut self, passed: &[TaskHandle]) {
        if passed.is_empty() {
            return;
        }
        self.passes = self.passes.wrapping_add(1);
        if self.passes == 0 {
            for task in self.tasks.iter_mut() {
                task.stamp = 0;
            }
            self.passes = 1;
        }
        for &task in passed {
            let t = &mut self.tasks[row_of(task)];
            if t.stamp == self.passes {
                continue;
            }
            t.stamp = self.passes;
            t.passed = t.passed.saturating_add(1);
            let starved = t.passed >= self.config.starve_after;
            if starved && self.hold().is_none() {
                self.hold = Some(task);
            }
        }
    }

    fn threads(&self, pool: Pool) -> usize {
        match pool {
            Pool::Io => self.config.io,
            Pool::Work => self.config.work,
        }
    }

    /// The node of request `r` whose task starts next on pool `p`, as memory allows; the tasks
    /// of it that did not fit, before that one, are added to `passed`. Only with a budget.
    fn pick(&mut self, r: u32, p: usize, passed: &mut Vec<TaskHandle>) -> Pick {
        let free = self.memory_free().expect("a budget");
        // A task predicted the whole budget runs alone: once nothing granted memory runs,
        // whatever the process holds besides, and while it runs nothing granted memory fits.
        let whole = self.config.budget.as_ref().expect("a budget").bytes;
        let alone = self.granted == 0;
        let nothing_runs = self.running == [0, 0];
        let hold = self.hold();
        // An idle request's task passed over holds back no other.
        let counts = !self.requests[r as usize].idle;
        // The heap in order, best first: from its root, the least of the places seen whose
        // parent was taken.
        let mut scan = std::mem::take(&mut self.scan);
        scan.clear();
        let heap = &self.requests[r as usize].ready[p];
        if let Some(first) = heap.first() {
            scan.push(Reverse((first.rank, first.seq, 0)));
        }
        let mut unfit = NONE;
        let mut picked = NONE;
        while let Some(Reverse((_, _, at))) = scan.pop() {
            let at = at as usize;
            let me = heap[at].node;
            let task = self.rows[me as usize].task;
            let memory = self.tasks[row_of(task)].memory;
            match hold {
                // Only it, and what takes no memory, may start until it does.
                Some(held) if held != task => {
                    if memory == 0 {
                        picked = me;
                        break;
                    }
                }
                _ if memory <= free || (memory == whole && alone) => {
                    picked = me;
                    break;
                }
                Some(_) => unfit = me,
                None => {
                    if unfit == NONE {
                        unfit = me;
                    }
                    if counts {
                        passed.push(task);
                    }
                }
            }
            for child in 4 * at + 1..(4 * at + 5).min(heap.len()) {
                let it = &heap[child];
                scan.push(Reverse((it.rank, it.seq, child as u32)));
            }
        }
        self.scan = scan;
        if picked != NONE {
            return Pick::Start(picked);
        }
        if unfit == NONE || !nothing_runs {
            return Pick::None;
        }
        let budget = self.config.budget.as_ref().expect("a budget");
        let cold = &self.cold[unfit as usize];
        let request = &self.requests[r as usize];
        Pick::Fail(
            unfit, // alloc: below, the message of a node that fails for memory, once
            format!(
                "{} needs {} of memory as predicted, and with nothing of blaze's running \
                 the process holds {} of the {} it may use ({})",
                request.name(cold),
                gb(cold.memory),
                gb(self.held),
                gb(budget.bytes),
                budget.from
            ),
        )
    }

    fn start(&mut self, at: u64, me: u32, out: &mut Vec<PlaneOutput>) {
        let m = me as usize;
        let task = self.rows[m].task;
        let t = self.tasks[row_of(task)];
        let p = pool_at(t.pool);
        let mut member = t.first;
        while member != NONE {
            self.unready(member, p);
            member = self.rows[member as usize].next_member;
        }
        self.cold[m].started_at = at;
        self.cold[m].flags |= STARTED;
        let request = &mut self.requests[self.rows[m].request as usize];
        request.stats.ran += 1;
        let clock = if request.idle { &mut self.idle_vclock[p] } else { &mut self.vclock[p] };
        *clock = (*clock).max(request.vtime[p]);
        request.vtime[p] += request.stride;
        if request.idle {
            self.idle_running.push(task);
        }
        self.tasks[row_of(task)].runner = me;
        self.tasks[row_of(task)].running = true;
        self.running[p] += 1;
        self.granted += t.memory;
        self.counts.granted_most = self.counts.granted_most.max(self.granted);
        self.counts.ran += 1;
        let mut member = t.first;
        while member != NONE {
            self.moved(at, member, NodeState::Running, out);
            member = self.rows[member as usize].next_member;
        }
        let blocker = self.cold[m].blocker;
        let near = (blocker != NONE && self.cold[blocker as usize].flags & GROUP != 0)
            .then(|| self.cold[blocker as usize].group);
        let critical = self.on_critical_path(me);
        out.push(PlaneOutput::Run { id: self.rows[m].id, pool: t.pool, near, critical });
    }

    /// From request `r`'s node that settled last, back through what each one waited for last:
    /// their ids.
    fn critical(&self, r: u32) -> Vec<u32> {
        let last = self.requests[r as usize]
            .nodes
            .iter()
            .copied()
            .filter(|&it| self.cold[it as usize].flags & DONE != 0)
            .max_by_key(|&it| (self.cold[it as usize].done_at, Reverse(self.rows[it as usize].seq)));
        let blocker = |it: &u32| Some(self.cold[*it as usize].blocker).filter(|&it| it != NONE);
        let mut path: Vec<u32> = std::iter::successors(last, blocker)
            .map(|it| self.rows[it as usize].id.0)
            .collect(); // alloc: the payload of the request's one `Event::Critical`
        path.reverse();
        path
    }
}

/// Whether an entry of a request's running nodes is still what it says: the same node (a slot
/// reused has another `seq`), running, at that priority.
fn running_live(rows: &[NodeRow], states: &[NodeState], (priority, seq, node): (u64, u64, u32)) -> bool {
    let row = &rows[node as usize];
    row.seq == seq && row.priority == priority && states[node as usize] == NodeState::Running
}

enum Pick {
    Start(u32),
    /// It cannot fit even with nothing running.
    Fail(u32, String),
    None,
}

fn said(request: RequestId, at_ns: u64, event: Event) -> PlaneOutput {
    PlaneOutput::Said { request, record: Record { at_ns, event } }
}

/// Why a node of `request` cannot run at all as the budget stands: its prediction alone is
/// more.
fn too_big(config: &PlaneConfig, request: &Request, cold: &NodeCold) -> Option<String> {
    let budget = config.budget.as_ref()?;
    (cold.memory > budget.bytes).then(|| {
        // alloc: the message of a node refused for memory, once
        format!(
            "{} needs {} of memory as predicted, more than blaze may use here: {} ({})",
            request.name(cold),
            gb(cold.memory),
            gb(budget.bytes),
            budget.from
        )
    })
}

/// A task of `me` alone, not running, in a free slot (the last freed) or a new one: its handle.
fn new_task(
    tasks: &mut RegionVec<TaskRow>,
    task_keys: &mut RegionVec<Key>,
    slots: &mut Slots<TaskSlot>,
    me: u32,
    cold: &NodeCold,
    key: Option<Key>,
) -> TaskHandle {
    let row = TaskRow {
        memory: cold.memory,
        runner: me,
        first: me,
        last: me,
        passed: 0,
        stamp: 0,
        pool: cold.pool,
        running: false,
        keyed: key.is_some(),
        yielding: false,
    };
    let key = key.unwrap_or(Key([0; 32]));
    let task = slots.take();
    if row_of(task) == tasks.len() {
        tasks.push(row);
        task_keys.push(key);
    } else {
        tasks[row_of(task)] = row;
        task_keys[row_of(task)] = key;
    }
    debug_assert_eq!(tasks.len(), slots.made());
    task
}

/// Takes the ids of the first `len` nodes of a batch refused back out of the index.
fn unindex(index: &mut HashMap<NodeId, NodeHandle>, nodes: &(impl Given + ?Sized), len: usize) {
    for at in 0..len {
        index.remove(&nodes.id(at));
    }
}

/// Puts `nodes` in `order`: the node at `i` is then the one that was at `order[i]`. Each place
/// of `order` is marked [`NEW`] as it is filled.
fn permute(nodes: &mut [Spec], order: &mut [u32]) {
    for start in 0..order.len() {
        if order[start] & NEW != 0 {
            continue;
        }
        let mut at = start;
        loop {
            let from = order[at] as usize;
            order[at] |= NEW;
            if from == start {
                break;
            }
            nodes.swap(at, from);
            at = from;
        }
    }
}

/// The nodes that wait for a node, from its first edge on, after `into`'s.
fn push_dependents(into: &mut Vec<u32>, edges: &[Edge], first: u32) {
    let mut edge = first;
    while edge != NONE {
        let it = edges[edge as usize];
        into.push(it.owner);
        edge = it.next;
    }
}

fn sift_up(heap: &mut [Ready], rows: &mut [NodeRow], mut at: usize) {
    let entry = heap[at];
    while at > 0 {
        let parent = (at - 1) / 4;
        if !entry.before(&heap[parent]) {
            break;
        }
        heap[at] = heap[parent];
        rows[heap[at].node as usize].heap_at = at as u32;
        at = parent;
    }
    heap[at] = entry;
    rows[entry.node as usize].heap_at = at as u32;
}

fn sift_down(heap: &mut [Ready], rows: &mut [NodeRow], mut at: usize) {
    let entry = heap[at];
    loop {
        let first = 4 * at + 1;
        if first >= heap.len() {
            break;
        }
        let mut least = first;
        for child in first + 1..(first + 4).min(heap.len()) {
            if heap[child].before(&heap[least]) {
                least = child;
            }
        }
        if !heap[least].before(&entry) {
            break;
        }
        heap[at] = heap[least];
        rows[heap[at].node as usize].heap_at = at as u32;
        at = least;
    }
    heap[at] = entry;
    rows[entry.node as usize].heap_at = at as u32;
}

fn heap_remove(heap: &mut Vec<Ready>, rows: &mut [NodeRow], at: usize) {
    rows[heap[at].node as usize].heap_at = NONE;
    let last = heap.pop().expect("a ready node in its heap");
    if at == heap.len() {
        return;
    }
    heap[at] = last;
    if at > 0 && last.before(&heap[(at - 1) / 4]) {
        sift_up(heap, rows, at);
    } else {
        sift_down(heap, rows, at);
    }
}
