//! The thread's query stack: what each running query read, in order, and what it is.
//!
//! One stack per thread, of frames: a **run** (a query executing, recording its reads), a
//! **verify** (a row whose edges are checked while it holds its flight), and a **barrier**
//! (pushed around a wait that may pump other jobs onto this thread: a job pumped above it
//! starts a stack of its own, so its reads and cycle checks never see the frames below).
//! Reads are `u32` nodes on one edge stack, each run's from its `edges_start`; a hit inside a
//! query is one push there and two folds into the top frame. A job on a fiber keeps its stack
//! across a wait: [`take`] and [`put`] move it with the job to the thread it resumes on.
//!
//! The one unsafe concern here is the thread-local's `UnsafeCell`: [`with`] lends it for a
//! closure that never calls user code or [`with`] again (every caller below is a few loads and
//! stores), so no two `&mut` of it are alive at once.

use std::{cell::UnsafeCell, hash::BuildHasher, ptr};

use blaze_hash::FixedState;

use super::{Id, Storage};
use crate::table::ShortLock;

/// No head: a frame that read no provisional row.
pub(crate) const NONE: u32 = u32::MAX;

/// Buckets of the filter that says whether a node may be on the stack.
const BLOOM: usize = 1024;

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
    /// A read equal to the last edge above this is not pushed again.
    pub(crate) dedup_floor: u32,
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
            serial: 0,
        }
    }
}

/// A row a verify proved unchanged assuming a verify lower on the stack (a cycle's) is too:
/// marked when that one is proved, dropped when it is not.
#[derive(Clone, Copy)]
pub(crate) struct Pending {
    pub(crate) header: *const super::row::Header,
    pub(crate) word: u32,
    pub(crate) node: u32,
    pub(crate) ingredient: u32,
    pub(crate) id: u32,
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
    pub(crate) pending: Vec<Pending>,
    pub(crate) walk: Vec<Walk>,
    bloom: [u16; BLOOM],
    serial: u64,
    /// Tracked structs made by each run: `(run serial, ingredient, identity hash)` → count.
    pub(crate) occurrences: hashbrown::HashMap<(u64, u32, u64), u32, FixedState>,
    /// The nodes [`Local::dedup_from`] has kept, empty between its calls.
    seen: hashbrown::HashSet<u32, FixedState>,
    pub(crate) attached: *const Storage,
}

impl Local {
    const fn empty() -> Local {
        Local {
            // Each buffer is empty and grows once per thread to its high-water mark.
            frames: Vec::new(), // alloc: none, empty
            edges: Vec::new(), // alloc: none, empty
            captures: Vec::new(), // alloc: none, empty
            members: Vec::new(), // alloc: none, empty
            pending: Vec::new(), // alloc: none, empty
            walk: Vec::new(), // alloc: none, empty
            bloom: [0; BLOOM],
            serial: 0,
            occurrences: hashbrown::HashMap::with_hasher(FixedState),
            seen: hashbrown::HashSet::with_hasher(FixedState),
            attached: ptr::null(),
        }
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

// SAFETY: a spare is empty (no frame, `attached` null: checked as it is kept), so it points at
// no storage.
unsafe impl Send for Spare {}

// alloc: none: an empty `Vec` in a static; it grows to the most jobs switched out at once.
static SPARES: ShortLock<Vec<Spare>> = ShortLock::new(Vec::new());

/// Moves the stack of the job switching out off this thread, leaving an empty one
/// ([`blaze_graph::sched::Carried`]): the job's frames, reads and cycle state are its
/// own wherever it resumes, and a job that runs here meanwhile starts a stack of its own.
#[inline(never)]
pub(crate) fn take() -> usize {
    let spare = SPARES.lock().pop().map(|it| it.0);
    // alloc: a stack once per job switched out at once, then kept in `SPARES`.
    let mut moved = spare.unwrap_or_else(|| Box::new(Local::empty()));
    // A switch happens in no `with`, so `with` lends the only reference to the stack.
    with(|local| std::mem::swap(local, &mut *moved));
    Box::into_raw(moved).expose_provenance()
}

/// Puts the stack [`take`] moved off back, on the thread the job resumes on.
///
/// # Panics
///
/// If the thread's own stack is not empty: a job left frames on it when it ended or switched.
#[inline(never)]
pub(crate) fn put(taken: usize) {
    // SAFETY: `taken` is the box `take` gave up for this job, which the driver puts back once.
    let mut left = unsafe { Box::from_raw(ptr::with_exposed_provenance_mut::<Local>(taken)) };
    with(|local| std::mem::swap(local, &mut *left));
    assert!(
        left.frames.is_empty() && left.attached.is_null(),
        "a job resumed on a thread whose query stack still holds {} frames",
        left.frames.len()
    );
    // alloc: none once warm: `SPARES` grows to its high-water mark.
    SPARES.lock().push(Spare(left));
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

    /// Records a read of `node` into the running query of `storage`, if any.
    #[inline(always)]
    pub(crate) fn read(
        &mut self,
        storage: *const Storage,
        node: u32,
        durability: u8,
        changed: u32,
    ) {
        let edges_len = self.edges.len() as u32;
        let last = self.edges.last().copied();
        let Some(frame) = self.recording(storage) else { return };
        frame.durability = frame.durability.min(durability);
        frame.changed = frame.changed.max(changed);
        if edges_len > frame.dedup_floor && last == Some(node) {
            return;
        }
        // alloc: none once warm: the edge stack grows to its high-water mark once per thread
        self.edges.push(node);
    }

    /// Drops every edge from `start` on that repeats one before it there, the first of each
    /// kept in order: a node read again in one run is proved by its first read, and a run
    /// that reads a few thousand nodes in turn (a pass over a body's calls) has its edges
    /// once each instead of once a read. Short runs, which the read's own check of the last
    /// edge keeps small, are left as they are. The edges from `start` must be the top frame's.
    pub(crate) fn dedup_from(&mut self, start: usize) {
        const LONG: usize = 1 << 12;
        if self.edges.len() - start <= LONG {
            return;
        }
        let mut kept = start;
        for at in start..self.edges.len() {
            let node = self.edges[at];
            // alloc: none once warm: the set grows to its high-water mark once per thread
            if self.seen.insert(node) {
                self.edges[kept] = node;
                kept += 1;
            }
        }
        self.edges.truncate(kept);
        self.seen.clear();
    }

    pub(crate) fn push(&mut self, mut frame: Frame) -> u32 {
        let at = self.frames.len() as u32;
        frame.edges_start = self.edges.len() as u32;
        frame.dedup_floor = frame.edges_start;
        frame.members_start = self.members.len() as u32;
        frame.pending_start = self.pending.len() as u32;
        self.serial += 1;
        frame.serial = self.serial;
        if matches!(frame.kind, Kind::Run | Kind::Verify) {
            let slot = &mut self.bloom[bucket(frame.node)];
            *slot =
                slot.checked_add(1).expect("more than 65535 frames of one node bucket on a stack");
        }
        // alloc: none once warm: the frame stack grows to its high-water mark once per thread
        self.frames.push(frame);
        at
    }

    /// Pops the frames from `at` up, and their reads.
    pub(crate) fn pop(&mut self, at: u32) {
        while self.frames.len() > at as usize {
            let frame = self.frames.pop().expect("a frame above `at`");
            if matches!(frame.kind, Kind::Run | Kind::Verify) {
                self.bloom[bucket(frame.node)] -= 1;
            }
            self.edges.truncate(frame.edges_start as usize);
            while self.captures.last().is_some_and(|it| it.frame >= at) {
                self.captures.pop();
            }
        }
        if self.frames.is_empty() {
            self.occurrences.clear();
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
        frame.durability = u8::MAX;
        frame.changed = 0;
        frame.summary = [0; 2];
        frame.untracked = false;
        frame.head = NONE;
        let pending = frame.pending_start as usize;
        self.pending.truncate(pending);
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
