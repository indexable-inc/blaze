//! The thread's query stack: what each running query read, in order, and what it is.
//!
//! One stack per thread, of frames: a **run** (a query executing, recording its reads), a
//! **verify** (a row whose edges are checked while it holds its flight), and a **barrier**
//! (pushed around a wait that may pump other jobs onto this thread: a job pumped above it
//! starts a stack of its own, so its reads and cycle checks never see the frames below).
//! Reads are `u32` nodes on one edge stack, each run's from its `edges_start`; a hit inside a
//! query is one push there and two folds into the top frame. A stretch of the stack that a
//! capture closed over, or that a replay pushed, is marked as that capture's ([`Segment`]), so a
//! capture around it names the capture instead of copying its reads. A job on a fiber keeps its stack
//! across a wait: [`take`] and [`put`] move it with the job to the thread it resumes on.
//!
//! The one unsafe concern here is the thread-local's `UnsafeCell`: [`with`] lends it for a
//! closure that never calls user code or [`with`] again (every caller below is a few loads and
//! stores), so no two `&mut` of it are alive at once.

use std::{
    any::TypeId,
    cell::UnsafeCell,
    hash::BuildHasher,
    num::NonZeroUsize,
    ops::ControlFlow,
    ptr,
    ptr::NonNull,
    sync::atomic::{AtomicU64, Ordering},
};

use blaze_hash::FixedState;
use blaze_mem::Tag;

use super::{Id, Storage, capture::REF};
use crate::table::ShortLock;

/// No head: a frame that read no provisional row.
pub(crate) const NONE: u32 = u32::MAX;

/// Buckets of the filter that says whether a node may be on the stack.
const BLOOM: usize = 1024;

/// The reads one run may record (its own and those replayed into it, counted before they are
/// deduplicated: what it holds on its thread's edge stack and what [`Local::full`] and a
/// capture's close dropped as repeats) before it fails with [`super::Runaway`] (rule 18). A stored row holds at most [`super::row::span::MAX_LEN`] (2^24) distinct reads,
/// and [`Local::read`] drops a read equal to the last, so a run past 2^28 reads 16 times more
/// than any run can keep: a loop that keeps reading. No profile yet counts a run's reads (the
/// "query stack reads" memory row now does); in run m2 of lane checkheap the edge, pending and
/// frame stacks of every thread together stayed inside the query tag's 2.3 GiB of global heap
/// on the normal runs, while the runaway grew one stack to 97 GiB. It is 1 GiB of `u32`s, so a
/// runaway is named at most 2 GiB into one stack (it is checked as the stack doubles).
pub(crate) const RUN_READS: u64 = 1 << 28;

/// Rows a task's pending list may hold past the storage's node count before the verify that
/// holds them fails with [`super::Runaway`] (rule 18). Each row has one node, and a row
/// reached again while it is pending is answered from the list ([`Local::pending_head`],
/// [`Local::repoint_pending`]), so a list longer than the rows that exist holds rows proved
/// again and again: the exponential re-verify that filled run m2's list (24-byte entries) to
/// 96 GiB. This floor keeps a small storage from tripping on a few repeats.
pub(crate) const PENDING_FLOOR: u64 = 1 << 16;

/// The reads a run holds before [`Local::full`] first drops its repeats, and the least it may
/// hold before any drop ([`next_dedup`]): 16 KiB of `u32`s, a spare's size ([`SPARE_BYTES`]),
/// so a small run is never deduplicated before it is stored and a dedup never runs every few
/// reads.
const DEDUP_FROM: u32 = (SPARE_BYTES / size_of::<u32>()) as u32;

/// The most entries one run held on its thread's edge stack at once ([`Frame::held_most`],
/// exact: taken at each point its count stops rising) and the most rows one task's pending
/// list held (sampled as the list doubles, so within a factor of 2), process-wide: the measure
/// [`RUN_READS`] and [`PENDING_FLOOR`] are set against, in the memory report ([`high_water`]). `MOST_RUN` names the run that set `MOST_RUN_READS`
/// ([`name_most_reads`]), written only as a new mark is set: the cold path.
static MOST_RUN_READS: AtomicU64 = AtomicU64::new(0);
static MOST_RUN: ShortLock<(u64, String)> = ShortLock::new("query.local.most_run", (0, String::new()));
static MOST_PENDING: AtomicU64 = AtomicU64::new(0);

/// What the query stacks held most, process-wide, sampled as they filled ([`high_water`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HighWater {
    /// The most reads one run held on its thread's edge stack (repeats dropped as it filled).
    pub run_reads: u64,
    /// The run that held them, as `` `query` of key ``; empty until one is named.
    pub run: String,
    /// The most rows one task's pending list held.
    pub pending: u64,
}

/// The most reads one run held (and which run) and the most rows one task held pending,
/// process-wide, sampled as the stacks filled: what the query stack bounds are measured against.
pub fn high_water() -> HighWater {
    let run = MOST_RUN.lock().1.clone(); // clone: a name, once a memory report
    HighWater {
        run_reads: MOST_RUN_READS.load(Ordering::Relaxed),
        run,
        pending: MOST_PENDING.load(Ordering::Relaxed),
    }
}

/// Names the run `run()` writes as the holder of the most reads one run held, `held`, unless a
/// run holding as many was named first ([`super::name_most_reads`] writes it).
pub(crate) fn name_most_reads(held: u64, run: impl FnOnce() -> String) {
    if MOST_RUN.lock().0 >= held {
        return;
    }
    // Written outside the lock (a key's `Debug` runs in `run`), then kept only if still the most.
    let run = run();
    let mut most = MOST_RUN.lock();
    if most.0 < held {
        *most = (held, run);
    }
}

/// What a read or a replay leaves its caller to do once it has let go of the stack
/// ([`super::noted`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[must_use = "a run past its bound fails and a new high-water mark is named (`super::noted`)"]
pub(crate) enum Note {
    Fine,
    /// The run on top set the most reads one run held, this many: name it.
    Most(u64),
    /// The run on top recorded more reads than [`RUN_READS`]: fail it.
    Runaway,
}

/// What one task may hold on its query stack ([`RUN_READS`], [`PENDING_FLOOR`]); a test lowers
/// them to meet the bound in a few steps. Moved with the task's stack across fibers.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Bounds {
    pub(crate) run_reads: u64,
    pub(crate) pending_floor: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    Run,
    Verify,
    /// A wait that may pump: frames below are another task's. `lifted` while the waiter
    /// itself runs the key (it holds the flight; its frames are its own again).
    Barrier {
        lifted: bool,
    },
}

pub(crate) struct Frame {
    /// The storage whose row this is; null for a barrier.
    pub(crate) storage: *const Storage,
    pub(crate) kind: Kind,
    pub(crate) node: u32,
    pub(crate) ingredient: u32,
    pub(crate) id: u32,
    /// Where its reads start on the edge stack.
    pub(crate) edges_start: u32,
    /// A read equal to the last edge above this is not pushed again; the innermost capture
    /// open in this run starts here (else its reads do), so [`Local::full`] dedups from it.
    pub(crate) dedup_floor: u32,
    /// Its reads dropped from the edge stack as repeats while it ran ([`Local::full`], a
    /// capture's close): with what it holds, the reads it recorded.
    pub(crate) dropped: u64,
    /// The edge stack's length at which [`Local::full`] next drops this run's repeats: its own
    /// count, not the stack's capacity, which a thread keeps at its high-water mark, so a run on
    /// a warm thread would otherwise hold every repeat up to whatever an earlier run grew it to.
    pub(crate) dedup_at: u32,
    /// The most entries it held on the edge stack at once, nested runs' excluded (they are
    /// gone when it reads again). Its count only rises between the points that lower it or end
    /// it, so this is taken there: just before a dedup at its limit ([`Local::full`]) or at a
    /// capture's close, before a fixpoint round's reads are cut, and as it stores its edges.
    /// Kept across a head's rounds: the most of any of them.
    pub(crate) held_most: u64,
    /// The lowest durability read.
    pub(crate) durability: u8,
    /// The highest `changed` read.
    pub(crate) changed: u32,
    /// The summaries of the [`super::Reads`] it replayed, ORed into its edges' when it is written.
    pub(crate) summary: [u64; 2],
    /// The lowest stack index of a cycle head it depends on, [`NONE`] for none.
    pub(crate) head: u32,
    /// A query on this stack asked for this one: it heads a cycle.
    pub(crate) reentered: bool,
    /// An inner head of its cycle did not converge this round (on the outermost head).
    pub(crate) unconverged: bool,
    pub(crate) untracked: bool,
    /// Read a provisional row or was told [`super::unshareable_reads`]: its captures give none.
    pub(crate) unshareable: bool,
    /// Its fixpoint round, from 1.
    pub(crate) round: u32,
    /// Where its cycle's members start on the member list.
    pub(crate) members_start: u32,
    /// Where the rows its verify proved on an assumption start on the pending list.
    pub(crate) pending_start: u32,
    /// What its cycle read outside itself, over every round, while it heads one: gathered
    /// from each run of the cycle as it ends, so the converged head's edges cover every input
    /// the cycle's values came from ([`CycleReads`]).
    pub(crate) cycle: CycleReads,
    /// A number no other run of this thread had: keys its tracked structs' occurrences.
    pub(crate) serial: u64,
}

impl Frame {
    pub(crate) fn new(storage: &Storage, kind: Kind, node: u32, ingredient: u32, id: Id) -> Frame {
        Frame {
            storage: ptr::from_ref(storage),
            kind,
            node,
            ingredient,
            id: id.as_u32(),
            edges_start: 0,
            dedup_floor: 0,
            dropped: 0,
            dedup_at: 0,
            held_most: 0,
            durability: u8::MAX,
            changed: 0,
            summary: [0; 2],
            head: NONE,
            reentered: false,
            unconverged: false,
            untracked: false,
            unshareable: false,
            round: 1,
            members_start: 0,
            pending_start: 0,
            cycle: CycleReads::NONE,
            serial: 0,
        }
    }
}

/// The reads a cycle made outside itself, gathered on its head's frame over its rounds: the
/// reads themselves on [`Local::cycle_edges`] from `start` (each round's in read order, its
/// members' at the place their reader read them), and what they carry. Reads of the cycle's
/// own rows record no edge (`function::read_provisional`), so a converged cycle's head stores
/// an acyclic set of edges that a verify walks like any row's (keys cover every input: a read
/// only an early round made is in it, as a run from scratch would make it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CycleReads {
    /// Where its reads start on [`Local::cycle_edges`].
    pub(crate) start: u32,
    /// Where this round's reads go, after its earlier rounds' (and, before it runs, after what
    /// its verify was handed): what is given it from a verify this round goes above.
    pub(crate) round: u32,
    /// The lowest durability read, `u8::MAX` for none.
    pub(crate) durability: u8,
    /// A run of the cycle made an untracked read.
    pub(crate) untracked: bool,
    /// The summaries of the reads its runs replayed ([`Frame::summary`]).
    pub(crate) summary: [u64; 2],
}

impl CycleReads {
    pub(crate) const NONE: CycleReads =
        CycleReads { start: 0, round: 0, durability: u8::MAX, untracked: false, summary: [0; 2] };
}

/// A row a verify proved unchanged assuming a verify lower on the stack (a cycle's) is too:
/// marked when that one is proved, dropped when it is not. Until then a verify that reaches it
/// again is answered from here ([`Local::pending_head`]) instead of checking its edges again.
#[derive(Clone, Copy)]
pub(crate) struct Pending {
    pub(crate) header: *const super::row::Header,
    pub(crate) word: u32,
    pub(crate) node: u32,
    pub(crate) ingredient: u32,
    pub(crate) id: u32,
    /// The stack index of the verify it was proved assuming, and that frame's serial: the
    /// frame there is that verify only while the serials agree.
    pub(crate) head: u32,
    pub(crate) head_serial: u64,
}

/// A [`super::capture_reads`] frame of the run at `frame`.
pub(crate) struct Capture {
    pub(crate) frame: u32,
    pub(crate) edges_start: u32,
    pub(crate) durability: u8,
    pub(crate) changed: u32,
    pub(crate) summary: [u64; 2],
    pub(crate) unshareable: bool,
    pub(crate) dedup_floor: u32,
}

/// Edges `start..end` of the stack are capture `capture`'s reads: a capture closed over them,
/// or a replay of it pushed them. Segments are in stack order, apart, and below the top.
#[derive(Clone, Copy)]
pub(crate) struct Segment {
    start: u32,
    end: u32,
    capture: u32,
}

/// A provisional row a cycle made on this thread, to finalize or empty with its head.
#[derive(Clone, Copy)]
pub(crate) struct Member {
    pub(crate) ingredient: u32,
    pub(crate) id: u32,
}

/// One row of a claim-free verify walk.
#[derive(Clone, Copy)]
pub(crate) struct Walk {
    /// The row's header, in a page of the walked storage.
    pub(crate) header: *const super::row::Header,
    pub(crate) node: u32,
    /// The view the row was verified in, against which its edges are checked.
    pub(crate) verified: u32,
    /// The word `verified` held, which the walk's compare-and-swap expects.
    pub(crate) word: u32,
    pub(crate) next: u64,
    pub(crate) end: u64,
    /// The OR of the summaries of the edges proved so far.
    pub(crate) summary: [u64; 2],
}

pub(crate) struct Local {
    pub(crate) frames: Vec<Frame>,
    pub(crate) edges: Vec<u32>,
    pub(crate) captures: Vec<Capture>,
    pub(crate) members: Vec<Member>,
    /// The open cycles' reads outside themselves, each head's from its frame's
    /// [`CycleReads::start`] ([`Frame::cycle`]).
    pub(crate) cycle_edges: Vec<u32>,
    pub(crate) pending: Vec<Pending>,
    /// Where each row's entry is on the pending list, by its header's address. An entry here
    /// may be stale (its row's entry was marked or dropped since); [`Local::pending_head`]
    /// checks it against the list. Cleared when the list empties ([`Local::truncate_pending`],
    /// [`Local::pop_pending`]) and when the stack does.
    pending_at: hashbrown::HashMap<usize, u32, FixedState>,
    pub(crate) walk: Vec<Walk>,
    bloom: [u16; BLOOM],
    serial: u64,
    /// Tracked structs made by each run: `(run serial, ingredient, identity hash)` → count.
    pub(crate) occurrences: hashbrown::HashMap<(u64, u32, u64), u32, FixedState>,
    /// The nodes [`Local::dedup_from`] has kept, empty between its calls.
    seen: hashbrown::HashSet<u32, FixedState>,
    /// The captures' stretches of the edge stack.
    segments: Vec<Segment>,
    /// The entries of the capture closing ([`Local::capture_entries`]), empty between closes.
    entries: Vec<u32>,
    /// The spans a replay's walk of a capture is in, and the captures it walked
    /// ([`Local::replay`]), empty between replays.
    spans: Vec<(u64, u64)>,
    walked: hashbrown::HashSet<u32, FixedState>,
    /// The database [`super::attach`] lent this task, carried with it across fibers as the
    /// rest of its stack is.
    pub(crate) attached: Option<Attached>,
    /// The task's bound on the rows it brings up to date ([`super::with_fuel`]), moved with it
    /// across fibers as the rest of its stack is.
    pub(crate) fuel: Option<Fuel>,
    /// The task's bound on the distinct steps its walk takes ([`super::with_steps`]), moved
    /// with it across fibers as the rest of its stack is.
    pub(crate) steps: Option<Steps>,
    /// The barriers on the stack that are not lifted: a job pumped above one is not the
    /// fueled task's, and is not charged.
    walls: u32,
    /// What the task may hold before the query holding it fails ([`super::Runaway`]).
    pub(crate) bounds: Bounds,
}

/// A database [`super::attach`] lent: its address, its type (so [`super::with_attached_db`]
/// hands it back only as what it is) and its storage (what the engine's own `Debug`s read).
#[derive(Clone, Copy)]
pub(crate) struct Attached {
    pub(crate) db: NonNull<()>,
    pub(crate) ty: TypeId,
    pub(crate) storage: NonNull<Storage>,
}

/// [`Local::fuel`]: the steps given and left, and how many barriers stood when it was given.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Fuel {
    pub(crate) given: u64,
    pub(crate) left: u64,
    walls: u32,
}

/// [`Local::steps`]: the most distinct steps the walk may take, the keys of those it took, and
/// how many barriers stood when it was given.
pub(crate) struct Steps {
    given: u64,
    seen: hashbrown::HashSet<u128, FixedState>,
    walls: u32,
}

impl Local {
    const fn empty() -> Local {
        Local {
            // Each buffer is empty and grows once per thread to its high-water mark.
            frames: Vec::new(),      // alloc: none, empty
            edges: Vec::new(),       // alloc: none, empty
            captures: Vec::new(),    // alloc: none, empty
            members: Vec::new(),     // alloc: none, empty
            cycle_edges: Vec::new(), // alloc: none, empty
            pending: Vec::new(),     // alloc: none, empty
            pending_at: hashbrown::HashMap::with_hasher(FixedState),
            walk: Vec::new(), // alloc: none, empty
            bloom: [0; BLOOM],
            serial: 0,
            occurrences: hashbrown::HashMap::with_hasher(FixedState),
            seen: hashbrown::HashSet::with_hasher(FixedState),
            segments: Vec::new(), // alloc: none, empty
            entries: Vec::new(),  // alloc: none, empty
            spans: Vec::new(),    // alloc: none, empty
            walked: hashbrown::HashSet::with_hasher(FixedState),
            attached: None,
            fuel: None,
            steps: None,
            walls: 0,
            bounds: Bounds { run_reads: RUN_READS, pending_floor: PENDING_FLOOR },
        }
    }

    /// Holds nothing of a task: no frame, read, capture, cycle member, pending row, walk, fuel,
    /// step bound, barrier, occurrence or attached storage. Its buffers' capacity and serial are the
    /// thread's.
    pub(crate) fn is_empty(&self) -> bool {
        self.frames.is_empty()
            && self.edges.is_empty()
            && self.segments.is_empty()
            && self.captures.is_empty()
            && self.members.is_empty()
            && self.cycle_edges.is_empty()
            && self.pending.is_empty()
            && self.walk.is_empty()
            && self.occurrences.is_empty()
            && self.attached.is_none()
            && self.fuel.is_none()
            && self.steps.is_none()
            && self.walls == 0
    }

    /// Lets go of every buffer holding more than [`SPARE_BYTES`], for an empty stack kept as a
    /// spare: what stays of one is bounded however deep a query its thread once ran.
    fn trim(&mut self) {
        fn vec<T>(buffer: &mut Vec<T>) {
            if buffer.capacity() * size_of::<T>() > SPARE_BYTES {
                *buffer = Vec::new(); // alloc: none, empty
            }
        }
        fn map<K, V>(map: &mut hashbrown::HashMap<K, V, FixedState>) {
            if map.capacity() * size_of::<(K, V)>() > SPARE_BYTES {
                *map = hashbrown::HashMap::with_hasher(FixedState);
            }
        }
        fn set<K>(set: &mut hashbrown::HashSet<K, FixedState>) {
            if set.capacity() * size_of::<K>() > SPARE_BYTES {
                *set = hashbrown::HashSet::with_hasher(FixedState);
            }
        }
        // Every field named, so a buffer added to the stack is trimmed or said not to need it.
        let Local {
            frames,
            edges,
            captures,
            members,
            cycle_edges,
            pending,
            pending_at,
            walk,
            bloom: _,
            serial: _,
            occurrences,
            seen,
            segments,
            entries,
            spans,
            walked,
            attached: _,
            fuel: _,
            steps: _,
            walls: _,
            bounds: _,
        } = self;
        vec(frames);
        vec(edges);
        vec(captures);
        vec(members);
        vec(cycle_edges);
        vec(pending);
        map(pending_at);
        vec(walk);
        map(occurrences);
        set(seen);
        vec(segments);
        vec(entries);
        vec(spans);
        set(walked);
    }

    /// Gives the task `steps` of fuel, a part of what it has if it has some: the fuel it had.
    /// A bound given below an unlifted barrier is another task's (this one was pumped onto its
    /// thread while it waits), as [`Local::burn`] has it: this one is not bounded by what that
    /// one has left.
    pub(crate) fn fuel(&mut self, steps: u64) -> Option<Fuel> {
        let walls = self.walls;
        let ours = self.fuel.filter(|outer| outer.walls == walls);
        let steps = ours.map_or(steps, |outer| outer.left.min(steps));
        self.fuel.replace(Fuel { given: steps, left: steps, walls })
    }

    /// Takes the task's fuel away, `outer` (what [`Local::fuel`] gave back) in its place, less
    /// what was spent when `outer` is this task's (given above the same barriers): the fuel
    /// taken. Another task's bound below a barrier is put back as it was.
    pub(crate) fn unfuel(&mut self, outer: Option<Fuel>) -> Option<Fuel> {
        let ours = std::mem::replace(&mut self.fuel, outer);
        if let (Some(outer), Some(ours)) = (&mut self.fuel, ours)
            && outer.walls == ours.walls
        {
            outer.left = outer.left.saturating_sub(ours.given - ours.left);
        }
        ours
    }

    /// One step of the fueled task, if this is its part of the stack: `Some(steps spent)` when
    /// none was left.
    #[inline]
    pub(crate) fn burn(&mut self) -> Option<u64> {
        let walls = self.walls;
        let fuel = self.fuel.as_mut().filter(|it| it.walls == walls)?;
        if fuel.left == 0 {
            return Some(fuel.given);
        }
        fuel.left -= 1;
        None
    }

    /// Gives the task a bound of `given` distinct steps, a part of what it has left if it has
    /// one: the bound it had. As with [`Local::fuel`], a bound given below an unlifted barrier
    /// is another task's, and this one is not bounded by it.
    pub(crate) fn steps(&mut self, given: u64) -> Option<Steps> {
        let walls = self.walls;
        let given = match &self.steps {
            Some(outer) if outer.walls == walls => {
                given.min(outer.given.saturating_sub(outer.seen.len() as u64))
            }
            _ => given,
        };
        self.steps.replace(Steps {
            given,
            seen: hashbrown::HashSet::with_hasher(FixedState),
            walls,
        })
    }

    /// Takes the task's step bound away, `outer` (what [`Local::steps`] gave back) in its place:
    /// the steps taken under it. When `outer` is this task's, the steps taken are its too.
    pub(crate) fn unsteps(&mut self, outer: Option<Steps>) -> u64 {
        let ours = std::mem::replace(&mut self.steps, outer);
        let taken = ours.as_ref().map_or(0, |it| it.seen.len() as u64);
        if let (Some(outer), Some(ours)) = (&mut self.steps, ours)
            && outer.walls == ours.walls
        {
            // alloc: none past the outer bound's own steps: its set grows by the inner's
            outer.seen.extend(ours.seen);
        }
        taken
    }

    /// Whether the task is bounded by steps here ([`Local::steps`]): whether a step given now
    /// is counted, so its key is worth making.
    #[inline]
    pub(crate) fn stepping(&self) -> bool {
        self.steps.as_ref().is_some_and(|it| it.walls == self.walls)
    }

    /// The step `key` of the bounded task, if this is its part of the stack: taken once however
    /// often it is given; `Some(steps taken)` when it is a new one and none is left.
    #[inline]
    pub(crate) fn step(&mut self, key: u128) -> Option<u64> {
        let walls = self.walls;
        let steps = self.steps.as_mut().filter(|it| it.walls == walls)?;
        if steps.seen.contains(&key) {
            return None;
        }
        if steps.seen.len() as u64 >= steps.given {
            return Some(steps.given);
        }
        // alloc: one entry per distinct step of a bounded walk, freed with its bound
        steps.seen.insert(key);
        None
    }

    /// Puts a row a verify proved on an assumption on the pending list: `true`, and the row
    /// not put, when the list is past its bound for a storage of `rows` nodes
    /// ([`Local::pending_bound`]), and the verify holding it must fail
    /// ([`super::runaway_pending`]).
    #[must_use = "a pending list past its bound fails its verify (`runaway_pending`)"]
    pub(crate) fn push_pending(&mut self, pending: Pending, rows: u64) -> bool {
        if self.pending.len() == self.pending.capacity() && self.past_pending(rows) {
            return true;
        }
        // alloc: none once warm: the map grows to the pending list's high-water mark
        self.pending_at.insert(pending.header.addr(), self.pending.len() as u32);
        // alloc: none once warm: the pending list grows to its high-water mark once per thread
        self.pending.push(pending);
        false
    }

    /// The most rows the pending list may hold for a storage of `rows` nodes.
    pub(crate) fn pending_bound(&self, rows: u64) -> u64 {
        rows.saturating_add(self.bounds.pending_floor)
    }

    /// Whether the pending list, about to grow, is past its bound; records its high-water mark.
    /// Checked only as the list grows, so it costs nothing per row.
    #[cold]
    #[inline(never)]
    fn past_pending(&self, rows: u64) -> bool {
        let held = self.pending.len() as u64;
        MOST_PENDING.fetch_max(held, Ordering::Relaxed);
        held >= self.pending_bound(rows)
    }

    /// The verify at `at` was proved unchanged assuming the verify at `head` below it: the rows
    /// it proved on its own assumption (pending from `from` with their head at `at`) are now
    /// proved on `head`'s, as they hold if it does and it holds if `head` does. They stay
    /// answerable from the list ([`Local::pending_head`]) after the frame at `at` is popped,
    /// instead of naming a frame that is gone and being verified again by the next path that
    /// reaches them. No entry names a frame above `at`: each frame popped before it re-pointed
    /// its own, or marked or dropped them.
    pub(crate) fn repoint_pending(&mut self, from: u32, at: u32, head: u32) {
        let serial = self.frames[head as usize].serial;
        for pending in &mut self.pending[from as usize..] {
            if pending.head >= at {
                pending.head = head;
                pending.head_serial = serial;
            }
        }
    }

    /// Where the frame at `at` is on its task's part of the stack (above the last unlifted
    /// barrier below it): its place among the frames running or verifying there, outermost 1,
    /// and how many there are.
    pub(crate) fn task_depth(&self, at: usize) -> (usize, usize) {
        let walled = |frame: &Frame| frame.kind == Kind::Barrier { lifted: false };
        let counted = |frame: &&Frame| matches!(frame.kind, Kind::Run | Kind::Verify);
        let floor = self.frames[..=at].iter().rposition(walled).map_or(0, |it| it + 1);
        let depth = self.frames[floor..=at].iter().filter(counted).count();
        let above =
            self.frames[at + 1..].iter().take_while(|it| !walled(*it)).filter(counted).count();
        (depth, depth + above)
    }

    /// The query running or verifying on top of the task's part of the stack (above the last
    /// unlifted barrier): its ingredient and its key's bits. `None` when the task runs none.
    pub(crate) fn top_query(&self) -> Option<(u32, u32)> {
        let walled = |frame: &Frame| frame.kind == Kind::Barrier { lifted: false };
        self.frames
            .iter()
            .rev()
            .take_while(|it| !walled(*it))
            .find(|it| matches!(it.kind, Kind::Run | Kind::Verify))
            .map(|it| (it.ingredient, it.id))
    }

    /// The frames running or verifying on the task's part of the stack.
    pub(crate) fn task_frames(&self) -> usize {
        match self.frames.len() {
            0 => 0,
            len => self.task_depth(len - 1).1,
        }
    }

    /// Cuts the pending list to its first `len` rows, the rest dropped unmarked (the verify they
    /// assumed was refuted or unwound).
    pub(crate) fn truncate_pending(&mut self, len: usize) {
        self.pending.truncate(len);
        self.forget_pending();
    }

    /// The last row of the pending list, taken off it, if the list is longer than `from`: the
    /// next row to mark once the verify at `from` is proved.
    pub(crate) fn pop_pending(&mut self, from: u32) -> Option<Pending> {
        let pending = (self.pending.len() as u32 > from).then(|| self.pending.pop()).flatten();
        self.forget_pending();
        pending
    }

    /// Drops the list's index once the list is empty: its entries are all stale then, and a
    /// long query that verifies many cycles one after another would otherwise keep one for
    /// every row it ever proved on an assumption.
    fn forget_pending(&mut self) {
        if self.pending.is_empty() {
            self.pending_at.clear();
        }
    }

    /// The head of the row of `header`, whose `verified` word is `word`, when it is on the
    /// pending list with that same word and the top verify may assume that head
    /// ([`Local::may_assume`]): what verifying the row again would find, as nothing was
    /// written since it was proved, the row is in the state it was proved in (not run,
    /// emptied or made provisional since, by this task or another), and the verify it
    /// assumed is still open. A
    /// verify that reaches a pending row by a second path is answered here, so it checks each
    /// row's edges once and not once per path to it: in a cycle of rows read along many paths
    /// (a recursive function's summaries) that is the difference between linear and
    /// exponential work.
    #[inline]
    pub(crate) fn pending_head(&self, header: *const super::row::Header, word: u32) -> Option<u32> {
        if self.pending.is_empty() {
            return None;
        }
        let at = *self.pending_at.get(&header.addr())?;
        let pending =
            self.pending.get(at as usize).filter(|it| it.header == header && it.word == word)?;
        let frame = self.frames.get(pending.head as usize)?;
        (frame.serial == pending.head_serial && self.may_assume(pending.head))
            .then_some(pending.head)
    }
}

thread_local! {
    static LOCAL: UnsafeCell<Local> = const { UnsafeCell::new(Local::empty()) };
}

/// Lends the thread's stack to `f`, which must not call user code or `with`.
///
/// Not inlined, and `f` reads or writes the stack inside: a job on a fiber may wait between
/// two `with`s of one function and resume on another thread, and an inlined one could keep the
/// first thread's address across the wait (as could a call that only returned the address,
/// which the compiler may merge with the next).
#[inline(never)]
pub(crate) fn with<R>(f: impl FnOnce(&mut Local) -> R) -> R {
    LOCAL.with(|local| {
        // SAFETY: the cell is this thread's; `f` never re-enters `with` nor runs code that
        // could (every caller in this module and the engine is loads, stores and pushes), nor
        // waits, so this is the only reference to it while `f` runs.
        f(unsafe { &mut *local.get() })
    })
}

/// An empty stack a thread left when a job's was put on it, kept for the next job to switch
/// out there.
struct Spare(Box<Local>);

// SAFETY: a spare is empty ([`Local::is_empty`], checked as it is kept: `attached` none and no
// frame), so it points at no storage.
unsafe impl Send for Spare {}

// alloc: none: an empty `Vec` in a static; it grows to the most jobs switched out at once, at
// most `SPARES_KEPT`.
static SPARES: ShortLock<Vec<Spare>> = ShortLock::new("query.local.spares", Vec::new());

/// The most bytes one buffer of a spare stack keeps ([`Local::trim`]): past it the buffer is let
/// go as the stack is kept, and the next job to take the spare grows its own. 16 KiB is 4,096
/// reads or about 200 frames, more than a job switched out inside a query holds but in a
/// crate-wide walk, so a job takes a spare without growing it; a spare kept at what the deepest
/// job on its thread ever needed (millions of reads, tens of MiB) held that for the rest of the
/// process, once per job switched out at once, and counted it to whatever task grew it
/// (memcap6, B5: `mir` heap no release lowered). A stack of 13 buffers keeps at most 208 KiB.
const SPARE_BYTES: usize = 16 << 10;

/// The most spares kept ([`keep_spare`]): a spare is taken by a job switching out and given back
/// as one resumes, so the list holds about as many as are switched out at once; past this many
/// a stack given back is freed, and a later switch makes a new one (one box, its buffers empty).
/// 256 trimmed spares keep at most 52 MiB, and usually a few kilobytes each.
const SPARES_KEPT: usize = 256;

/// Keeps `left`, an empty stack trimmed for keeping ([`Local::trim`]), among `spares`, or gives
/// it back when [`SPARES_KEPT`] are kept already, to be freed with no lock held.
fn keep_spare(spares: &mut Vec<Spare>, left: Box<Local>) -> Option<Box<Local>> {
    if spares.len() >= SPARES_KEPT {
        return Some(left);
    }
    // alloc: none once warm: the list grows to at most `SPARES_KEPT`
    spares.push(Spare(left));
    None
}

/// Moves the stack of the job switching out off this thread, leaving an empty one
/// ([`blaze_graph::sched::Carried`]): the job's frames, reads and cycle state are its
/// own wherever it resumes, and a job that runs here meanwhile starts a stack of its own.
///
/// An empty stack ([`Local::is_empty`]) is not moved: `None`, and the thread keeps it with its
/// buffers. A job holds nothing in it that another could see, and whichever thread it resumes
/// on lends it that thread's empty stack. Every job starts and ends with an empty stack, so a
/// switch moves a box only for a job that waits inside a query; one a job still holds as it
/// ends is freed by [`end`].
#[inline(never)]
pub(crate) fn take() -> Option<NonZeroUsize> {
    if with(|local| local.is_empty()) {
        return None;
    }
    let spare = SPARES.lock().pop().map(|it| it.0);
    // alloc: a stack once per job switched out at once holding frames, then kept in `SPARES`.
    let mut moved = spare.unwrap_or_else(|| Box::new(Local::empty()));
    // A switch happens in no `with`, so `with` lends the only reference to the stack.
    with(|local| std::mem::swap(local, &mut *moved));
    NonZeroUsize::new(Box::into_raw(moved).expose_provenance())
}

/// Puts the stack [`take`] moved off back, on the thread the job resumes on.
///
/// # Panics
///
/// If the thread's own stack is not empty: a job left frames on it when it ended or switched.
#[inline(never)]
pub(crate) fn put(taken: NonZeroUsize) {
    // SAFETY: `taken` is the box `take` gave up for this job, which the driver puts back once.
    let mut left = unsafe { Box::from_raw(ptr::with_exposed_provenance_mut::<Local>(taken.get())) };
    with(|local| std::mem::swap(local, &mut *left));
    assert!(
        left.is_empty(),
        "a job resumed on a thread whose query stack was not empty: {} frames, {} reads, {} walk rows",
        left.frames.len(),
        left.edges.len(),
        left.walk.len()
    );
    // The buffers past what a spare keeps are freed here, with no lock held, as is a stack
    // past the spares kept.
    left.trim();
    let unkept = keep_spare(&mut SPARES.lock(), left);
    drop(unkept);
}

/// Frees the stack [`take`] moved off a job that ended holding it (or whose parked record was
/// dropped): its frames name no storage anyone reads again.
#[inline(never)]
pub(crate) fn end(taken: NonZeroUsize) {
    // SAFETY: `taken` is the box `take` gave up for this job, which the driver ends once.
    drop(unsafe { Box::from_raw(ptr::with_exposed_provenance_mut::<Local>(taken.get())) });
}

/// Pushes `value` onto `buffer`, one of a stack's, growing it in a task of [`Tag::Query`]: a
/// stack is the query engine's, kept by its thread or among the spares for the next job, so its
/// growth counts to `query`, not to whatever task's reads first filled it (memcap6, B5: they
/// counted as `mir` heap that no release of MIR lowered). A free goes back to the tag the block
/// was allocated for, from any thread, so a buffer let go leaves `query`.
#[inline(always)]
fn push_kept<T>(buffer: &mut Vec<T>, value: T) {
    if buffer.len() == buffer.capacity() {
        grow(buffer);
    }
    buffer.push(value);
}

#[cold]
#[inline(never)]
fn grow<T>(buffer: &mut Vec<T>) {
    // alloc: a stack's buffer, doubling to its thread's high-water mark (a spare's trimmed to
    // `SPARE_BYTES`)
    blaze_mem::frame::task(Tag::Query, |_| buffer.reserve(1));
}

/// The stack length at which a run whose reads start at `start`, holding `kept` after its last
/// dedup, next drops its repeats ([`Frame::dedup_at`]): once it holds `max(DEDUP_FROM, 2 x
/// kept)`. So a run never holds more than that at once, whatever it read before, and each
/// dedup's pass over its reads is paid for by at least half as many pushes since the last.
pub(crate) fn next_dedup(start: usize, kept: usize) -> u32 {
    u32::try_from(start + (2 * kept).max(DEDUP_FROM as usize)).unwrap_or(u32::MAX)
}

/// Raises the process-wide most one run held at once to `held`; [`Note::Most`] when that set a
/// new mark, for the caller to name the run ([`super::noted`]). A load first, so a run that
/// sets no mark writes no shared line.
fn most_reads(held: u64) -> Note {
    if MOST_RUN_READS.load(Ordering::Relaxed) < held
        && MOST_RUN_READS.fetch_max(held, Ordering::Relaxed) < held
    {
        Note::Most(held)
    } else {
        Note::Fine
    }
}

#[inline(always)]
fn bucket(node: u32) -> usize {
    (node as usize).wrapping_mul(0x9e37_79b9) >> 7 & (BLOOM - 1)
}

impl Local {
    /// The top frame if it is a run of `storage`: what a read is recorded into.
    #[inline(always)]
    pub(crate) fn recording(&mut self, storage: *const Storage) -> Option<&mut Frame> {
        self.frames.last_mut().filter(|frame| frame.kind == Kind::Run && frame.storage == storage)
    }

    /// Records a read of `node` into the running query of `storage`, if any. What the caller
    /// does next, once it has let go of the stack ([`super::noted`]): [`Note::Runaway`], and
    /// the read not recorded, when that run recorded more reads than [`RUN_READS`].
    #[inline(always)]
    pub(crate) fn read(
        &mut self,
        storage: *const Storage,
        node: u32,
        durability: u8,
        changed: u32,
    ) -> Note {
        let edges_len = self.edges.len() as u32;
        let last = self.edges.last().copied();
        let Some(frame) = self.recording(storage) else { return Note::Fine };
        frame.durability = frame.durability.min(durability);
        frame.changed = frame.changed.max(changed);
        if edges_len > frame.dedup_floor && last == Some(node) {
            return Note::Fine;
        }
        let mut note = Note::Fine;
        if edges_len >= frame.dedup_at {
            note = self.full();
            if note == Note::Runaway {
                return note;
            }
        }
        // alloc: none once warm: the edge stack grows to its high-water mark once per thread
        push_kept(&mut self.edges, node);
        note
    }

    /// The recording run on top reached its [`Frame::dedup_at`]. Drops its repeated reads from
    /// its [`Frame::dedup_floor`] up (a node read again is proved by its first read, so the run
    /// holds each node it read about once, not once a read: a pass over many rows re-reading
    /// the same inputs, or replays of captures that share their reads, held 2^27 reads in one
    /// run of coord-120's check), and sets the next limit once it has added as many reads again
    /// ([`next_dedup`]). The limit is the run's own, never the stack's capacity: a thread keeps
    /// its stack at its high-water mark, and a run on it must drop its repeats all the same.
    /// Below the floor are reads an open capture does not hold, kept as they are, so its start
    /// stays put. Fails the run ([`Note::Runaway`]) once it recorded [`RUN_READS`], counting
    /// the reads dropped, and says when it set the high-water mark. Run only at the limit, so
    /// a read costs one compare.
    #[cold]
    #[inline(never)]
    fn full(&mut self) -> Note {
        let at = self.frames.len() - 1;
        let peak = self.held_peak(at);
        let frame = &self.frames[at];
        let (start, floor) = (frame.edges_start as usize, frame.dedup_floor as usize);
        let before = self.edges.len();
        self.dedup_from(floor);
        let len = self.edges.len();
        let frame = &mut self.frames[at];
        frame.dropped += (before - len) as u64;
        frame.dedup_at = next_dedup(start, len - start);
        let recorded = (len - start) as u64 + frame.dropped;
        if recorded >= self.bounds.run_reads {
            return Note::Runaway;
        }
        most_reads(peak)
    }

    /// Takes what the run at `at` holds now into its [`Frame::held_most`], at a point its count
    /// stops rising; returns that most.
    pub(crate) fn held_peak(&mut self, at: usize) -> u64 {
        let len = self.edges.len();
        let frame = &mut self.frames[at];
        frame.held_most = frame.held_most.max((len - frame.edges_start as usize) as u64);
        frame.held_most
    }

    /// [`Local::held_peak`] of the run at `at`, and whether that set the process-wide mark.
    pub(crate) fn note_peak(&mut self, at: usize) -> (u64, Note) {
        let peak = self.held_peak(at);
        (peak, most_reads(peak))
    }

    /// Drops every edge from `start` on that repeats one before it there, the first of each
    /// kept in order: a node read again in one run is proved by its first read, so a row's
    /// span and a capture hold each node once, however often it was read (a pass over a body's
    /// calls, the reads a dozen replayed goals share). The edges from `start` must be the top
    /// frame's; the segments over them are dropped, as the edges move.
    pub(crate) fn dedup_from(&mut self, start: usize) {
        self.drop_segments_from(start);
        once_each(&mut self.edges, start, &mut self.seen);
    }

    /// Drops the repeats of the cycle reads from `start` on, the first of each kept in read
    /// order, and returns them: what a converged head stores as its edges, and between rounds
    /// what keeps a head's gathering at the reads it made, not the reads times its rounds.
    pub(crate) fn cycle_union(&mut self, start: usize) -> &[u32] {
        let Local { cycle_edges, seen, .. } = self;
        once_each(cycle_edges, start, seen);
        &cycle_edges[start..]
    }

    /// Drops the edges from `len` on, and the segments over them.
    pub(crate) fn truncate_edges(&mut self, len: usize) {
        self.edges.truncate(len);
        self.drop_segments_from(len);
    }

    fn drop_segments_from(&mut self, at: usize) {
        while self.segments.last().is_some_and(|it| it.end as usize > at) {
            self.segments.pop();
        }
    }

    /// Marks the edges from `start` to the top as capture `capture`'s (it closed over them, or
    /// a replay of it pushed them): a capture closed around them names it.
    pub(crate) fn mark_capture(&mut self, start: usize, capture: u32) {
        self.drop_segments_from(start);
        if capture != 0 && self.edges.len() > start {
            // alloc: none once warm: the segment list grows to its high-water mark once per thread
            self.segments.push(Segment {
                start: start as u32,
                end: self.edges.len() as u32,
                capture,
            });
        }
    }

    /// What a capture of the edges from `start` holds ([`Storage::push_capture`]), in read
    /// order, each once: a node read in it as the node, and a stretch a capture inside it
    /// closed over or a replay pushed as that capture's number with [`REF`]. So a read is
    /// held by the innermost capture that made it, and a capture of `n` captures nested in
    /// turn holds about `2n` entries, not `n^2 / 2`.
    pub(crate) fn capture_entries(&mut self, start: usize) -> &[u32] {
        let Local { edges, segments, entries, seen, .. } = self;
        entries.clear();
        let first = segments.partition_point(|it| (it.start as usize) < start);
        let mut at = start;
        for segment in &segments[first..] {
            // alloc: none once warm: the entry list grows to its high-water mark once per thread
            entries.extend_from_slice(&edges[at..segment.start as usize]);
            // alloc: none once warm, as above
            entries.push(REF | segment.capture);
            at = segment.end as usize;
        }
        // alloc: none once warm, as above
        entries.extend_from_slice(&edges[at..]);
        once_each(entries, 0, seen);
        entries
    }

    /// Pushes every node capture `capture` read onto the edge stack, marked as its, for the
    /// recording run on top: a replay. What the caller does next, as for a read
    /// ([`Local::read`]): a replay that took the run past its limit drops its repeats
    /// ([`Local::full`]).
    pub(crate) fn replay(&mut self, storage: &Storage, capture: u32) -> Note {
        let start = self.edges.len();
        let Local { edges, spans, walked, .. } = self;
        let _: Option<()> = walk_capture(storage, capture, spans, walked, |node| {
            // alloc: none once warm: the edge stack grows to its high-water mark once per thread
            push_kept(edges, node);
            ControlFlow::Continue(())
        });
        self.mark_capture(start, capture);
        let limit = self.frames.last().map_or(u32::MAX, |it| it.dedup_at);
        if (self.edges.len() as u32) < limit {
            return Note::Fine;
        }
        self.full()
    }

    pub(crate) fn push(&mut self, mut frame: Frame) -> u32 {
        let at = self.frames.len() as u32;
        frame.edges_start = self.edges.len() as u32;
        frame.dedup_floor = frame.edges_start;
        frame.dropped = 0;
        frame.held_most = 0;
        frame.dedup_at = next_dedup(frame.edges_start as usize, 0);
        frame.members_start = self.members.len() as u32;
        frame.pending_start = self.pending.len() as u32;
        let gathered = self.cycle_edges.len() as u32;
        frame.cycle = CycleReads { start: gathered, round: gathered, ..CycleReads::NONE };
        self.serial += 1;
        frame.serial = self.serial;
        self.walls += u32::from(frame.kind == Kind::Barrier { lifted: false });
        if matches!(frame.kind, Kind::Run | Kind::Verify) {
            let slot = &mut self.bloom[bucket(frame.node)];
            *slot =
                slot.checked_add(1).expect("more than 65535 frames of one node bucket on a stack");
        }
        // alloc: none once warm: the frame stack grows to its high-water mark once per thread
        push_kept(&mut self.frames, frame);
        at
    }

    /// Pops the frames from `at` up, and their reads.
    pub(crate) fn pop(&mut self, at: u32) {
        while self.frames.len() > at as usize {
            let frame = self.frames.pop().expect("a frame above `at`");
            self.walls -= u32::from(frame.kind == Kind::Barrier { lifted: false });
            if matches!(frame.kind, Kind::Run | Kind::Verify) {
                self.bloom[bucket(frame.node)] -= 1;
            }
            self.truncate_edges(frame.edges_start as usize);
            while self.captures.last().is_some_and(|it| it.frame >= at) {
                self.captures.pop();
            }
        }
        if self.frames.is_empty() {
            self.occurrences.clear();
            self.pending_at.clear();
            // No head is left to gather for: what a frame that ended outside its cycle left
            // goes with the stack.
            self.cycle_edges.clear();
        }
    }

    /// Gives the run at `at` a fresh serial: a fixpoint's next round counts its tracked
    /// structs' occurrences from 0 again, so it makes the same ones.
    pub(crate) fn renew_serial(&mut self, at: u32) {
        self.serial += 1;
        self.frames[at as usize].serial = self.serial;
    }

    /// Turns the verify frame at `at` into a run: its row runs, holding the same flight.
    pub(crate) fn verify_to_run(&mut self, at: u32) {
        let edges = self.edges.len() as u32;
        let frame = &mut self.frames[at as usize];
        debug_assert_eq!(frame.kind, Kind::Verify);
        frame.kind = Kind::Run;
        frame.edges_start = edges;
        frame.dedup_floor = edges;
        frame.dropped = 0;
        frame.held_most = 0;
        frame.dedup_at = next_dedup(edges as usize, 0);
        frame.durability = u8::MAX;
        frame.changed = 0;
        frame.summary = [0; 2];
        frame.untracked = false;
        frame.head = NONE;
        // What its verify was handed (a cycle that met it) comes before its run's reads.
        frame.cycle.round = self.cycle_edges.len() as u32;
        let pending = frame.pending_start as usize;
        self.truncate_pending(pending);
    }

    /// Whether a verify may assume the verify at `at` unchanged: every frame above it is a
    /// verify (a run between them read values, which an assumption can't give).
    pub(crate) fn may_assume(&self, at: u32) -> bool {
        self.frames[at as usize].kind == Kind::Verify
            && self.frames[at as usize + 1..]
                .iter()
                .all(|it| matches!(it.kind, Kind::Verify | Kind::Barrier { lifted: true }))
    }

    /// Where the run or verify of `node` of `storage` is on this task's part of the stack.
    #[inline]
    pub(crate) fn find(&self, storage: *const Storage, node: u32) -> Option<u32> {
        if node == 0 || self.bloom[bucket(node)] == 0 {
            return None;
        }
        for (at, frame) in self.frames.iter().enumerate().rev() {
            match frame.kind {
                Kind::Barrier { lifted: false } => return None,
                Kind::Barrier { lifted: true } => {}
                Kind::Run | Kind::Verify => {
                    if frame.node == node && frame.storage == storage {
                        return Some(at as u32);
                    }
                }
            }
        }
        None
    }

    /// The occurrence number of the next tracked struct of `identity_hash` the top run makes.
    pub(crate) fn occurrence(&mut self, ingredient: u32, identity_hash: u64) -> Option<u32> {
        let serial = self.frames.last().filter(|it| it.kind == Kind::Run)?.serial;
        let count = self.occurrences.entry((serial, ingredient, identity_hash)).or_insert(0);
        let occurrence = *count;
        *count += 1;
        Some(occurrence)
    }

    /// The hash the occurrence table keys an identity by.
    pub(crate) fn identity_hash<T: std::hash::Hash>(identity: &T) -> u64 {
        FixedState.hash_one(identity)
    }
}

/// Drops every entry of `list` from `start` on that repeats one before it there, the first of
/// each kept in order. A short tail is checked against itself; a long one goes through `seen`,
/// which is left empty.
fn once_each(list: &mut Vec<u32>, start: usize, seen: &mut hashbrown::HashSet<u32, FixedState>) {
    /// Entries checked against each other: at most `SHORT^2 / 2` compares, cheaper than hashing.
    const SHORT: usize = 32;
    let mut kept = start;
    if list.len() - start <= SHORT {
        for at in start..list.len() {
            let entry = list[at];
            if !list[start..kept].contains(&entry) {
                list[kept] = entry;
                kept += 1;
            }
        }
    } else {
        for at in start..list.len() {
            let entry = list[at];
            // alloc: none once warm: the set grows to its high-water mark once per thread
            if seen.insert(entry) {
                list[kept] = entry;
                kept += 1;
            }
        }
        seen.clear();
    }
    list.truncate(kept);
}

/// Calls `f` with each node capture `capture` (of `storage`'s view) read, in read order: a
/// capture it names is walked where it is named, each capture once, so a capture named by
/// several of those inside it costs one walk. `spans` and `walked` are the walk's, left empty.
pub(crate) fn walk_capture<B>(
    storage: &Storage,
    capture: u32,
    spans: &mut Vec<(u64, u64)>,
    walked: &mut hashbrown::HashSet<u32, FixedState>,
    mut f: impl FnMut(u32) -> ControlFlow<B>,
) -> Option<B> {
    use super::row::span;
    let open = |spans: &mut Vec<(u64, u64)>, capture: u32| {
        let at = storage.capture_span(capture);
        let start = span::start(at);
        // alloc: none once warm (a thread's own lists) or the caller's (`Reads::for_each_read`)
        spans.push((start, start + span::len(at)));
    };
    let mut out = None;
    // The root is not put in `walked`: a capture names only captures closed before it, so it
    // never meets itself, and a capture that names none walks with no set at all.
    if capture != 0 {
        open(spans, capture);
    }
    while let Some(top) = spans.last_mut() {
        if top.0 == top.1 {
            spans.pop();
            continue;
        }
        let entry = storage.captured(top.0);
        top.0 += 1;
        if entry & REF != 0 {
            let inner = entry & !REF;
            // alloc: none once warm, as above
            if walked.insert(inner) {
                open(spans, inner);
            }
        } else if let ControlFlow::Break(it) = f(entry) {
            out = Some(it);
            break;
        }
    }
    spans.clear();
    walked.clear();
    out
}

/// Pushes a barrier for a wait that may pump, popped when the guard drops.
pub(crate) fn barrier() -> Barrier {
    Barrier { at: with(|local| local.push(Frame::new_barrier())) }
}

impl Frame {
    fn new_barrier() -> Frame {
        Frame {
            storage: ptr::null(),
            kind: Kind::Barrier { lifted: false },
            node: 0,
            ingredient: 0,
            id: 0,
            edges_start: 0,
            dedup_floor: 0,
            dropped: 0,
            dedup_at: 0,
            held_most: 0,
            durability: u8::MAX,
            changed: 0,
            summary: [0; 2],
            head: NONE,
            reentered: false,
            unconverged: false,
            untracked: false,
            unshareable: false,
            round: 1,
            members_start: 0,
            pending_start: 0,
            cycle: CycleReads::NONE,
            serial: 0,
        }
    }
}

pub(crate) struct Barrier {
    at: u32,
}

impl Barrier {
    /// Lifts the barrier while the waiter runs the key itself, until the guard drops.
    pub(crate) fn lift(&self) -> Lifted {
        set_lifted(self.at, true);
        Lifted { at: self.at }
    }
}

fn set_lifted(at: u32, lifted: bool) {
    with(|local| {
        if let Some(frame) = local.frames.get_mut(at as usize) {
            debug_assert!(matches!(frame.kind, Kind::Barrier { .. }));
            if frame.kind != (Kind::Barrier { lifted }) {
                local.walls = if lifted { local.walls - 1 } else { local.walls + 1 };
            }
            frame.kind = Kind::Barrier { lifted };
        }
    });
}

impl Drop for Barrier {
    fn drop(&mut self) {
        with(|local| local.pop(self.at));
    }
}

pub(crate) struct Lifted {
    at: u32,
}

impl Drop for Lifted {
    fn drop(&mut self) {
        set_lifted(self.at, false);
    }
}

#[cfg(test)]
mod tests {
    use super::{Local, SPARE_BYTES, SPARES_KEPT, keep_spare};

    /// A stack kept as a spare keeps no buffer past [`SPARE_BYTES`] (one under it stays, for the
    /// next job), and no more than [`SPARES_KEPT`] spares are kept: what a deep query once grew
    /// is not held for the rest of the process.
    #[test]
    fn a_spare_keeps_no_buffer_past_its_cap_and_spares_are_capped() {
        let mut spares = Vec::new();
        let mut unkept = 0;
        for _ in 0..SPARES_KEPT + 2 {
            let mut left = Box::new(Local::empty());
            // alloc: a test's buffers, two past the cap (four bytes a read or node) and one under
            left.edges.reserve(SPARE_BYTES);
            left.walked.reserve(SPARE_BYTES);
            left.frames.reserve(1);
            let frames = left.frames.capacity();
            left.trim();
            assert_eq!(left.edges.capacity(), 0, "the edge stack past the cap is let go");
            assert_eq!(left.walked.capacity(), 0, "the walked set past the cap is let go");
            assert_eq!(left.frames.capacity(), frames, "the frame stack under the cap stays");
            unkept += usize::from(keep_spare(&mut spares, left).is_some());
        }
        assert_eq!(spares.len(), SPARES_KEPT);
        assert_eq!(unkept, 2, "a stack given back past the spares kept is freed");
    }
}
