//! The plain store as blaze-keyed's [`Store`]: the handle one process holds.
//!
//! Reads take no shared lock. Every segment this process mapped (the generation it opened on,
//! then what each of its commits and compactions added) is in an insert-only table, each under
//! the index it was mapped at and never removed or replaced: a read probes the segments of the
//! namespaces it asks, in their order, each namespace's newest first, and returns the bytes in
//! place (`map`) or copied into the caller's frame (`read`), with the segment's [`Proof`]. No
//! read checks a byte (decision 18): each segment was proved once, when it was mapped.
//!
//! Puts take no shared lock either (store-next.md, "Puts on every worker"). Each worker puts
//! into its own lane: one [`Held`] batch per namespace written, behind a lock only its worker
//! and the commit take. A put asks the mapped segments (lock-free) and its own lane, nothing
//! else. The one shared step is the commit: under the writer's lock it takes every lane's
//! batches (each lane's lock held for a swap, not for the write), lays them out in key order
//! as one segment per namespace, writes and flips them, and publishes the new segments to the
//! table. A record is visible to its own worker from its put, and to every other reader from
//! its commit's flip; never before.

use std::{
    alloc::{Allocator, Global},
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    thread::ThreadId,
};

use blaze_core::{Driver, Mapping, Native, lock::Lock};
use blaze_hash::SharedMap;
use blaze_keyed::{
    kind::{Key as KeyOf, Kind, Namespace, Tag},
    store::{Compacted, ENTRY, Generation, Put, Read, RootRef, Store, StoreError},
};

use super::{
    core::{Batch, Held, PutError, machine_in},
    driven::Driven,
    format::{self, Entry, Segment},
    settings::Settings,
    verified::Proof,
};
use crate::{Key, Mismatch, content_of};

/// At most this many lanes (worker threads that put) in one process: a cap, not a guess at
/// the machine (128 cores with room for a pool's spares).
pub const LANES_MOST: usize = 4096;

/// One segment this process mapped: its row, its mapping (a clone, kept for the life of the
/// handle: never unmapped while the process runs), and its proof.
struct Mapped {
    seg: Segment,
    map: Mapping,
    proof: Proof,
}

impl Mapped {
    fn run(&self) -> &[u8] {
        self.seg.run(self.map.bytes())
    }

    fn payload(&self, entry: &Entry) -> &[u8] {
        &self.map.bytes()[entry.at as usize..(entry.at + u64::from(entry.len)) as usize]
    }
}

/// A worker's lane, by index: what [`PlainStore::lane`] made.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LaneId(usize);

/// One worker's puts not yet committed. `open` takes its puts; `flushing` is what the running
/// commit took from it, kept readable by its worker until the commit's segments are in the
/// table (the global heap: shared with the commit, which borrows its batches).
struct Lane {
    open: Vec<Held>,
    flushing: Option<Arc<Vec<Held>, Global>>,
}

impl Lane {
    fn get(&self, namespace: Namespace, key: &Key) -> Option<(Tag, &[u8])> {
        let flushing = self.flushing.iter().flat_map(|it| it.iter());
        self.open.iter().chain(flushing).filter(|it| it.batch().namespace() == namespace).find_map(|it| it.get(key))
    }

    fn get_any(&self, spaces: &[Namespace], key: &Key) -> Option<(Tag, &[u8])> {
        spaces.iter().find_map(|space| self.get(*space, key))
    }

    fn holds(&self) -> bool {
        self.open.iter().any(|it| !it.batch().is_empty())
    }
}

/// The plain store, as one process holds it: lock-free reads, a lane per worker for puts, one
/// writer for the commit and compaction.
pub struct PlainStore<D: Driver = Native> {
    writer: Lock<Driven<D>>,
    /// Index `i` is the `i`th segment mapped; written under the writer's lock, read without it.
    table: SharedMap<usize, Mapped>,
    /// How many of `table`'s indexes are in: published with `Release` after their inserts.
    len: AtomicUsize,
    /// The namespaces a read asks, in order.
    reads: Vec<Namespace>,
    /// The namespaces written, in order: each lane holds one batch for each.
    writes: Vec<Namespace>,
    /// What names this machine, as the store was opened with it: a put to a shared namespace
    /// holding one is refused at the put. The commit scans again with the writer's own list,
    /// which [`Driven::machine`] may have grown since.
    needles: Vec<Vec<u8>>,
    /// The lanes, by index; insert-only, each a lock its worker and the commit take.
    lanes: SharedMap<usize, Lock<Lane>>,
    /// The next lane's index.
    next_lane: AtomicUsize,
    /// The lane of each thread that put through [`Store::put`].
    by_thread: SharedMap<ThreadId, usize>,
    /// The generation the table holds, for the proof of a record read from a lane.
    generation: AtomicU64,
    /// Commits whose flushed batches were let go: a read that missed in the table and then in
    /// its lane while one ended asks the table again.
    released: AtomicU64,
}

impl PlainStore<Native> {
    /// The plain store in `root/plain`, with the namespaces `settings` name.
    pub fn open(root: &std::path::Path, settings: &Settings) -> Result<PlainStore<Native>, StoreError> {
        Driven::open(root, settings).map(PlainStore::new)
    }
}

impl<D: Driver> PlainStore<D> {
    /// The handle over an opened store.
    pub fn new(driven: Driven<D>) -> PlainStore<D> {
        let mut reads = Vec::with_capacity_in(driven.core().reads().len(), Global);
        reads.extend_from_slice(driven.core().reads());
        let mut writes = Vec::with_capacity_in(driven.core().writes().len(), Global);
        writes.extend_from_slice(driven.core().writes());
        let mut needles = Vec::with_capacity_in(driven.core().needles().len(), Global);
        needles.extend(driven.core().needles().iter().cloned());
        let it = PlainStore {
            writer: Lock::new("store.plain.writer", driven),
            table: SharedMap::default(),
            len: AtomicUsize::new(0),
            reads,
            writes,
            needles,
            lanes: SharedMap::default(),
            next_lane: AtomicUsize::new(0),
            by_thread: SharedMap::default(),
            generation: AtomicU64::new(0),
            released: AtomicU64::new(0),
        };
        it.refresh(&it.writer.lock());
        it
    }

    /// The writer back, the handle let go of (a test's simulation, crashed and opened again).
    /// Puts not committed are dropped with their lanes.
    pub fn into_writer(self) -> Driven<D> {
        self.writer.into_inner()
    }

    /// What the writer mapped since the last refresh, into the table; called under the
    /// writer's lock, so one refresher at a time.
    fn refresh(&self, writer: &Driven<D>) {
        let core = writer.core();
        let generation = Generation(core.generation());
        let from = self.len.load(Ordering::Relaxed);
        let table = self.table.pin();
        for at in from..core.mapped_count() {
            let proved = core.mapped_at(at);
            let replaced = table.insert(at, Mapped { seg: *proved.seg(), map: proved.mapping().clone(), proof: proved.proof(generation) });
            assert!(replaced.is_none(), "the table is insert-only: index {at} is inserted once");
        }
        self.generation.store(core.generation(), Ordering::Relaxed);
        self.len.store(core.mapped_count(), Ordering::Release);
    }

    /// The segment and entry of `key` in `spaces`, newest first within each namespace; no
    /// lock, no check.
    fn find(&self, spaces: &[Namespace], key: &blaze_ids::ContentId) -> Option<(&Mapped, Entry)> {
        let len = self.len.load(Ordering::Acquire);
        let table = self.table.pin();
        for space in spaces {
            for at in (0..len).rev() {
                let Some(mapped) = table.get(&at) else { continue };
                if mapped.seg.namespace != *space {
                    continue;
                }
                let run = mapped.run();
                if let Some(i) = format::find(run, key) {
                    // SAFETY: the table is insert-only: each index is inserted once, under the
                    // writer's lock, and never removed or replaced, so papaya never retires the
                    // allocation of a value while the map lives; values are allocated one by
                    // one and a resize moves pointers, not values. The map lives as long as
                    // `self`, so the value outlives the guard's borrow for `&self`'s lifetime.
                    // Its mapping's bytes are the segment's for as long as the clone lives.
                    let mapped: &Mapped = unsafe { &*(mapped as *const Mapped) };
                    return Some((mapped, Entry::read(&run[i * ENTRY..(i + 1) * ENTRY])));
                }
            }
        }
        None
    }

    /// The lane at `id`.
    fn lane_at(&self, id: LaneId) -> &Lock<Lane> {
        let lanes = self.lanes.pin();
        let lane = lanes.get(&id.0).expect("a lane id is made with its lane");
        // SAFETY: as for `table` in `find`: `lanes` is insert-only (each index inserted once,
        // by `lane`, never removed or replaced), its values allocated one by one, and the map
        // lives as long as `self`.
        unsafe { &*(lane as *const Lock<Lane>) }
    }

    /// A new lane, for one worker: its puts touch only it until the commit. A worker that puts
    /// through [`Store::put`] gets one on its first put; a pool that names its workers (a
    /// simulation's) asks for them here.
    pub fn lane(&self) -> Result<LaneId, StoreError> {
        let id = self.next_lane.fetch_add(1, Ordering::Relaxed);
        if id >= LANES_MOST {
            return Err(StoreError::Other { what: "more threads put into one plain store than its lane cap, LANES_MOST" });
        }
        let mut open = Vec::with_capacity_in(self.writes.len(), Global);
        open.extend(self.writes.iter().map(|it| Held::new(*it)));
        let replaced = self.lanes.pin().insert(id, Lock::new("store.plain.lane", Lane { open, flushing: None }));
        assert!(replaced.is_none(), "lane {id} is made once");
        Ok(LaneId(id))
    }

    /// The calling thread's lane, made on its first put.
    fn thread_lane(&self) -> Result<LaneId, StoreError> {
        let thread = std::thread::current().id();
        if let Some(&id) = self.by_thread.pin().get(&thread) {
            return Ok(LaneId(id));
        }
        let id = self.lane()?;
        self.by_thread.pin().insert(thread, id.0);
        Ok(id)
    }

    /// The calling thread's lane when it has put anything; a read never makes one.
    fn own_lane(&self) -> Option<LaneId> {
        self.by_thread.pin().get(&std::thread::current().id()).map(|&id| LaneId(id))
    }

    /// `value` under `key` in `namespace`, into `lane`: no shared lock. The same record
    /// committed or held in the lane is [`Put::Same`]; other bytes under the key there are a
    /// [`PutError::Mismatch`]. Another lane holding the key is not looked at: the commit lays
    /// one record for equal bytes, and refuses other bytes as two values.
    pub fn put_in(&self, lane: LaneId, key: Key, namespace: Namespace, tag: Tag, value: &[u8]) -> Result<Put, PutError> {
        if !self.writes.contains(&namespace) {
            return Err(PutError::NotWritable { namespace });
        }
        if namespace != Namespace::LOCAL
            && let Some((at, needle)) = machine_in(&self.needles, value)
        {
            return Err(PutError::Machine { key, namespace, at, needle });
        }
        if let Some((mapped, entry)) = self.find(std::slice::from_ref(&namespace), &key.0) {
            let kept = mapped.payload(&entry);
            if entry.tag == tag && kept == value {
                return Ok(Put::Same);
            }
            return Err(PutError::Mismatch(Mismatch { key, kept: content_of(kept), given: content_of(value) }));
        }
        let mut state = self.lane_at(lane).lock();
        if let Some((kept_tag, kept)) = state.flushing.iter().flat_map(|it| it.iter()).filter(|it| it.batch().namespace() == namespace).find_map(|it| it.get(&key)) {
            if kept_tag == tag && kept == value {
                return Ok(Put::Same);
            }
            return Err(PutError::Mismatch(Mismatch { key, kept: content_of(kept), given: content_of(value) }));
        }
        let held = state.open.iter_mut().find(|it| it.batch().namespace() == namespace).expect("a lane holds a batch for each namespace written");
        held.put(key, tag, value)
    }

    /// The record under `key` in `lane`'s puts not yet committed, in `self.reads`' order,
    /// copied into `frame`; the frame back on a miss.
    fn lane_copy<A: Allocator>(&self, lane: LaneId, key: &Key, frame: A) -> Result<(Tag, Vec<u8, A>), A> {
        let state = self.lane_at(lane).lock();
        match state.get_any(&self.reads, key) {
            Some((tag, bytes)) => {
                let mut out = Vec::with_capacity_in(bytes.len(), frame);
                out.extend_from_slice(bytes);
                Ok((tag, out))
            }
            None => Err(frame),
        }
    }

    /// What a read from `lane` sees (`None`: a reader that put nothing): the committed record
    /// under `key`, else the lane's own. The lane is asked after the table, and the table
    /// again when a commit let go of its flushed batches between the two: their records are
    /// in the table by then, as the commit publishes the table before it lets go.
    pub fn get<A: Allocator>(&self, lane: Option<LaneId>, key: &Key, frame: A) -> Option<(Tag, Vec<u8, A>, Proof)> {
        let released = self.released.load(Ordering::Acquire);
        if let Some((mapped, entry)) = self.find(&self.reads, &key.0) {
            return Some(copied(mapped, &entry, frame));
        }
        let frame = match self.lane_copy(lane?, key, frame) {
            Ok((tag, bytes)) => return Some((tag, bytes, own_proof(self.generation.load(Ordering::Relaxed)))),
            Err(frame) => frame,
        };
        if self.released.load(Ordering::Acquire) != released
            && let Some((mapped, entry)) = self.find(&self.reads, &key.0)
        {
            return Some(copied(mapped, &entry, frame));
        }
        None
    }

    /// Every lane's batches committed at once (the one shared step): see the module's docs.
    pub fn commit_lanes(&self) -> Result<Generation, StoreError> {
        let mut writer = self.writer.lock();
        // Each lane's batches, moved out for this commit and kept readable by its worker; a
        // lane's lock is held for the swap only, so its worker puts on into a fresh batch.
        let mut taken: Vec<(usize, Arc<Vec<Held>, Global>)> = Vec::new_in(Global);
        for (&id, lane) in self.lanes.pin().iter() {
            let mut state = lane.lock();
            debug_assert!(state.flushing.is_none(), "a commit lets go of what it took before the next starts");
            if !state.holds() {
                continue;
            }
            let mut fresh = Vec::with_capacity_in(self.writes.len(), Global);
            fresh.extend(self.writes.iter().map(|it| Held::new(*it)));
            let open = std::mem::replace(&mut state.open, fresh);
            let flushing = Arc::new_in(open, Global);
            state.flushing = Some(Arc::clone(&flushing));
            taken.push((id, flushing));
        }
        let mut batches: Vec<&Batch> = Vec::new_in(Global);
        for (_, held) in &taken {
            batches.extend(held.iter().map(Held::batch).filter(|it| !it.is_empty()));
        }
        let outcome = writer.commit_with(&batches);
        drop(batches);
        if outcome.is_ok() {
            // The new segments in the table before any lane lets go of its flushed batches.
            self.refresh(&writer);
        }
        for (id, flushing) in taken {
            let mut state = self.lane_at(LaneId(id)).lock();
            state.flushing = None;
            if outcome.is_err() {
                // Given back, ahead of the puts made while the commit ran.
                let mut back = Arc::try_unwrap(flushing).unwrap_or_else(|_| unreachable!("the commit and the lane held the only two"));
                for (back, since) in back.iter_mut().zip(state.open.iter()) {
                    back.absorb(since);
                }
                state.open = back;
            }
        }
        self.released.fetch_add(1, Ordering::Release);
        outcome.map(|it| Generation(it.generation))
    }

    /// The writer, for what the trait does not cover (pull, publish, scrub).
    pub fn writer(&self) -> blaze_core::lock::LockGuard<'_, Driven<D>> {
        self.writer.lock()
    }
}

/// A record put and not yet committed has no segment: its proof is this process's own segment
/// to be, which a segment this process wrote is trusted as (decision 18).
fn own_proof(generation: u64) -> Proof {
    Proof::own(Generation(generation))
}

/// `entry`'s payload in `mapped`, copied into `frame`, with its tag and proof.
fn copied<A: Allocator>(mapped: &Mapped, entry: &Entry, frame: A) -> (Tag, Vec<u8, A>, Proof) {
    let payload = mapped.payload(entry);
    let mut bytes = Vec::with_capacity_in(payload.len(), frame);
    bytes.extend_from_slice(payload);
    (entry.tag, bytes, mapped.proof)
}

impl<D: Driver> Store for PlainStore<D> {
    type Held<A: Allocator> = Vec<u8, A>;
    type Verified = Proof;

    /// Copied from its mapped segment into `frame` (the segment is mapped already, so this is
    /// the page cache's bytes with no system call; `Driven::get_into` is the `pread` path for a
    /// caller with no mapping), else from the calling thread's own lane.
    fn read<K: Kind, A: Allocator>(&self, key: &KeyOf<K>, frame: A) -> Result<Option<Read<Proof, Vec<u8, A>>>, StoreError> {
        let lane = if self.lanes.is_empty() { None } else { self.own_lane() };
        match self.get(lane, &Key(key.id()), frame) {
            None => Ok(None),
            Some((tag, _, _)) if tag != K::TAG => Err(StoreError::TwoValues { tag: K::TAG, key: key.id() }),
            Some((_, bytes, from)) => Ok(Some(Read { bytes, from })),
        }
    }

    /// In place in its mapped segment. A record put and not yet committed is in no segment, so
    /// it is not mapped: [`StoreError::Uncommitted`] names it, never a quiet `None`.
    fn map<K: Kind>(&self, key: &KeyOf<K>) -> Result<Option<Read<Proof, &[u8]>>, StoreError> {
        let released = self.released.load(Ordering::Acquire);
        if let Some((mapped, entry)) = self.find(&self.reads, &key.id()) {
            if entry.tag != K::TAG {
                return Err(StoreError::TwoValues { tag: K::TAG, key: key.id() });
            }
            return Ok(Some(Read { bytes: mapped.payload(&entry), from: mapped.proof }));
        }
        if let Some(lane) = self.own_lane()
            && self.lane_at(lane).lock().get_any(&self.reads, &Key(key.id())).is_some()
        {
            return Err(StoreError::Uncommitted { tag: K::TAG, key: key.id() });
        }
        if self.released.load(Ordering::Acquire) != released
            && let Some((mapped, entry)) = self.find(&self.reads, &key.id())
        {
            if entry.tag != K::TAG {
                return Err(StoreError::TwoValues { tag: K::TAG, key: key.id() });
            }
            return Ok(Some(Read { bytes: mapped.payload(&entry), from: mapped.proof }));
        }
        Ok(None)
    }

    /// Into the calling thread's lane: no shared lock ([`PlainStore::put_in`]).
    fn put<K: Kind>(&self, key: &KeyOf<K>, namespace: Namespace, bytes: &[u8]) -> Result<Put, StoreError> {
        let lane = self.thread_lane()?;
        match self.put_in(lane, Key(key.id()), namespace, K::TAG, bytes) {
            Ok(put) => Ok(put),
            // Other bytes under the key: an answer, not a failure.
            Err(PutError::Mismatch(_)) => Ok(Put::Other),
            Err(refused) => Err(refused.into_store(K::TAG)),
        }
    }

    fn commit(&self) -> Result<Generation, StoreError> {
        self.commit_lanes()
    }

    /// Roots `roots`' records under the root `compact` (each record kept, with its key: the
    /// store cannot read what a record of another kind names, so what a root's record names is
    /// the caller's to root too), sets the cap, commits, then compacts in bounded slices on the
    /// calling thread as it is (the caller runs it beside builds, on a thread it lowered).
    fn compact(&self, roots: &[RootRef], cap: u64) -> Result<Compacted, StoreError> {
        // The lanes' puts first, so the compaction's commit holds only its root.
        self.commit_lanes()?;
        let mut writer = self.writer.lock();
        let mut keys = Vec::with_capacity_in(roots.len(), Global);
        keys.extend(roots.iter().map(|it| Key(it.key)));
        writer.root("compact", &keys)?;
        writer.core_mut().set_cap(cap);
        writer.commit_with(&[])?;
        // Each slice replaces at least one segment, so one a segment (and one to say there is
        // no more) bounds them.
        let most = writer.core().segments().len() as u32 + 1;
        let slices = writer.slices(most)?;
        self.refresh(&writer);
        let mut out = Compacted::default();
        for it in &slices {
            out.kept += it.kept;
            out.freed += it.dropped;
            out.kept_bytes += it.bytes_out;
            out.freed_bytes += it.bytes_in.saturating_sub(it.bytes_out);
        }
        Ok(out)
    }
}
