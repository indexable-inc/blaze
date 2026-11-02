//! Weak memory and races. Each atomic keeps its stores in modification order, as Miri's
//! `concurrency/weak_memory.rs` (a port of tsan11 and of "Dynamic Race Detection for C++11",
//! with Miri's C++20 changes), and a load may read any store coherence and the SC rules leave
//! it: where Miri picks one at random, every candidate here is a branch of the path. Each
//! `UnsafeCell`, and each atomic's non-atomic accesses (`get_mut`, `into_inner`), keeps its last
//! write and each thread's last read, checked against the accessing thread's clock.

use std::{collections::VecDeque, sync::atomic::Ordering};

use super::{At, Object, Runtime};
use crate::clock::{Clock, MAX_THREADS, Stamp, Tid};

/// Stores kept per atomic (Miri's `STORE_BUFFER_LIMIT`); older ones can no longer be read.
pub(crate) const HISTORY: usize = 128;

const UNSEEN: u32 = u32::MAX;

#[derive(Clone, Debug)]
pub(crate) struct Store {
    value: u64,
    stamp: Stamp,
    /// What a load that reads it acquires.
    sync: Clock,
    /// SC, or happens before or is coherence-ordered before an SC store.
    seq_cst: bool,
    sc_loaded: bool,
    /// Each thread's clock at its first load of it.
    loaded: [u32; MAX_THREADS],
}

/// 88 bytes: a load's scan of the newest stores reads about one and a half lines per store.
const _: () = assert!(size_of::<Store>() == 88);

impl Store {
    fn initial(value: u64) -> Store {
        Store {
            value,
            stamp: Stamp::INIT,
            sync: Clock::ZERO,
            seq_cst: false,
            sc_loaded: false,
            loaded: [UNSEEN; MAX_THREADS],
        }
    }

    /// A load that happens before `clock` read it. Every lane is compared (a vector compare).
    fn read_before(&self, clock: &Clock) -> bool {
        self.loaded.iter().zip(clock.0).fold(false, |any, (&at, now)| any | ((at != UNSEEN) & (at <= now)))
    }
}

/// An atomic's state beside its stores (which are in [`Runtime::histories`] at its index).
pub(crate) struct Atomic {
    /// The last non-atomic write (its initial value, or `get_mut`), and where.
    plain: Stamp,
    plain_at: Option<At>,
    /// Each thread's last atomic access, and where.
    accessed: Clock,
    accessed_at: [Option<At>; MAX_THREADS],
    /// `get_mut` handed out the value: the next access starts the history again from it.
    pub(crate) dirty: bool,
}

#[derive(Clone)]
pub(crate) struct Cell {
    write: Stamp,
    write_at: Option<At>,
    reads: Clock,
    reads_at: [Option<At>; MAX_THREADS],
}

impl Cell {
    pub(crate) fn new() -> Cell {
        Cell { write: Stamp::INIT, write_at: None, reads: Clock::ZERO, reads_at: [None; MAX_THREADS] }
    }
}

fn acquires(ord: Ordering) -> bool {
    matches!(ord, Ordering::Acquire | Ordering::AcqRel | Ordering::SeqCst)
}

fn releases(ord: Ordering) -> bool {
    matches!(ord, Ordering::Release | Ordering::AcqRel | Ordering::SeqCst)
}

fn place(at: Option<At>) -> String {
    // alloc: a finding's or an error's text, made once, when the model stops
    at.map_or_else(|| "its creation".to_owned(), |at| at.to_string())
}

/// Starts the history at `index` again from one initial store, keeping its capacity.
fn restart(histories: &mut Vec<VecDeque<Store>>, index: usize, value: u64) {
    if histories.len() <= index {
        histories.resize_with(index + 1, VecDeque::new);
    }
    let stores = &mut histories[index];
    stores.clear();
    stores.push_back(Store::initial(value));
}

impl Runtime {
    /// The atomic `id` names, made with the value `initial` holds now if it names none.
    pub(crate) fn atomic(&mut self, id: &std::sync::atomic::AtomicU64, initial: u64) -> u32 {
        self.object(id, |rt| {
            let index = rt.objects.len();
            restart(&mut rt.histories, index, initial);
            Object::Atomic(Atomic {
                plain: Stamp::INIT,
                plain_at: None,
                accessed: Clock::ZERO,
                accessed_at: [None; MAX_THREADS],
                dirty: false,
            })
        })
    }

    fn atomic_mut(&mut self, obj: u32) -> &mut Atomic {
        match &mut self.objects[obj as usize].obj {
            Object::Atomic(atomic) => atomic,
            _ => unreachable!("blaze-check: an atomic operation on another object"),
        }
    }

    /// Checks an atomic access against the last non-atomic write, records it, and starts the
    /// history again from `real` if `get_mut` changed the value.
    fn atomic_access(&mut self, obj: u32, thread: Tid, at: At, real: u64) -> Result<(), String> {
        let clock = self.threads[thread].clock;
        let atomic = self.atomic_mut(obj);
        if !atomic.plain.before(&clock) {
            let (writer, plain_at) = (atomic.plain.thread, atomic.plain_at);
            // alloc: a finding's or an error's text, made once, when the model stops
            return Err(format!(
                "data race: an atomic access at {at} by thread {thread} races with a non-atomic \
                 write (`get_mut`) at {} by thread {writer}",
                place(plain_at)
            ));
        }
        let dirty = std::mem::replace(&mut atomic.dirty, false);
        atomic.accessed.set(thread, clock.get(thread));
        atomic.accessed_at[thread] = Some(at);
        if dirty {
            restart(&mut self.histories, obj as usize, real);
        }
        Ok(())
    }

    /// An atomic load: which store it reads is a branch (Miri's `fetch_store`).
    pub(crate) fn load(&mut self, obj: u32, ord: Ordering, at: At, real: u64) -> Result<u64, String> {
        let thread = self.current();
        self.atomic_access(obj, thread, at, real)?;
        let seq_cst = ord == Ordering::SeqCst;
        if seq_cst {
            let fence = self.last_sc_fence;
            self.threads[thread].read_seqcst.join(&fence);
        }
        let mut candidates = std::mem::take(&mut self.candidates);
        candidates.clear();
        let th = &self.threads[thread];
        let stores = &self.histories[obj as usize];
        let newest = stores.len() - 1;
        let mut found_sc = false;
        for (index, store) in stores.iter().enumerate().rev() {
            let include = if seq_cst && store.seq_cst {
                // An SC load ignores all but the last SC store (stores not SC aren't affected).
                !std::mem::replace(&mut found_sc, true)
            } else {
                true
            };
            // A thread that yielded doesn't read again an old store it read before the yield,
            // so a loop that waits for a newer one ends (loom's `is_seen_before_yield`).
            let seen = store.loaded[thread];
            let stale = index != newest && seen != UNSEEN && th.last_yield.is_some_and(|it| seen <= it);
            if include && !stale {
                candidates.push(index as u16);
            }
            // Nothing earlier in modification order may be read past a store that:
            let hides = store.stamp.before(&th.clock) // happens before the load (CoWR)
                || store.read_before(&th.clock) // a load before this one read (CoRR)
                // an SC fence before the load saw (C++17 32.4 [atomics.order] 4)
                || (store.seq_cst && store.stamp.before(&th.write_seqcst))
                // was before an SC fence an SC load saw (paragraph 5)
                || (seq_cst && store.stamp.before(&th.read_seqcst))
                // an SC load read, for an SC load (C++20 coherence-ordered-before, Miri #2301)
                || (seq_cst && store.sc_loaded);
            if hides {
                break;
            }
        }
        let choice = self.path.branch_load(candidates.len()).map_err(|stuck| self.stuck(stuck));
        let index = choice.map(|choice| candidates[choice] as usize);
        self.candidates = candidates;
        let index = index?;
        let clock = self.threads[thread].clock;
        let store = &mut self.histories[obj as usize][index];
        if store.loaded[thread] == UNSEEN {
            store.loaded[thread] = clock.get(thread);
        }
        store.sc_loaded |= seq_cst;
        let (value, sync) = (store.value, store.sync);
        let th = &mut self.threads[thread];
        match acquires(ord) {
            true => th.clock.join(&sync),
            false => th.fence_acquire.join(&sync),
        }
        Ok(value)
    }

    /// An atomic store, last in modification order (Miri's `store_impl`).
    pub(crate) fn store(&mut self, obj: u32, value: u64, ord: Ordering, at: At, real: u64) -> Result<(), String> {
        let thread = self.current();
        self.atomic_access(obj, thread, at, real)?;
        let th = &self.threads[thread];
        // C++20: a relaxed store ends a release sequence; it releases only what a release
        // fence before it did.
        let sync = if releases(ord) { th.clock } else { th.fence_release };
        self.push_store(obj, thread, value, sync, ord == Ordering::SeqCst);
        Ok(())
    }

    fn push_store(&mut self, obj: u32, thread: Tid, value: u64, sync: Clock, seq_cst: bool) {
        let clock = self.threads[thread].clock;
        if seq_cst {
            self.last_sc_write.set(thread, clock.get(thread));
        }
        let stores = &mut self.histories[obj as usize];
        if seq_cst {
            // What happens before or is coherence-ordered before an SC store is SC for later
            // SC loads.
            for store in stores.iter_mut() {
                if store.stamp.before(&clock) || store.read_before(&clock) {
                    store.seq_cst = true;
                }
            }
        }
        if stores.len() == HISTORY {
            stores.pop_front();
        }
        stores.push_back(Store {
            value,
            stamp: Stamp { thread: thread as u8, at: clock.get(thread) },
            sync,
            seq_cst,
            sc_loaded: false,
            loaded: [UNSEEN; MAX_THREADS],
        });
    }

    /// A read-modify-write, or a compare-exchange: it reads the newest store. `f` gives the
    /// value to write, or `None` (a failed compare-exchange: a load with `failure`).
    pub(crate) fn rmw(
        &mut self,
        obj: u32,
        success: Ordering,
        failure: Ordering,
        at: At,
        real: u64,
        f: impl FnOnce(u64) -> Option<u64>,
    ) -> Result<Result<u64, u64>, String> {
        let thread = self.current();
        self.atomic_access(obj, thread, at, real)?;
        let clock = self.threads[thread].clock;
        let newest = self.histories[obj as usize].back_mut().expect("an atomic has a store");
        let old = newest.value;
        let new = f(old);
        let ord = if new.is_some() { success } else { failure };
        let seq_cst = ord == Ordering::SeqCst;
        if newest.loaded[thread] == UNSEEN {
            newest.loaded[thread] = clock.get(thread);
        }
        newest.sc_loaded |= seq_cst;
        let sync = newest.sync;
        if seq_cst {
            let fence = self.last_sc_fence;
            self.threads[thread].read_seqcst.join(&fence);
        }
        let th = &mut self.threads[thread];
        match acquires(ord) {
            true => th.clock.join(&sync),
            false => th.fence_acquire.join(&sync),
        }
        let Some(new) = new else { return Ok(Err(old)) };
        // A release sequence goes on through an RMW: it releases what the store it replaces
        // did, and its own.
        let mut released = sync;
        released.join(if releases(success) { &th.clock } else { &th.fence_release });
        self.push_store(obj, thread, new, released, seq_cst);
        Ok(Ok(old))
    }

    /// A fence (Miri's `atomic_fence`). An SC fence is an AcqRel RMW of one global clock.
    pub(crate) fn fence(&mut self, ord: Ordering) {
        let thread = self.current();
        let th = &mut self.threads[thread];
        if ord != Ordering::Release {
            let acquired = th.fence_acquire;
            th.clock.join(&acquired);
        }
        if ord == Ordering::SeqCst {
            self.last_sc_fence.join(&th.clock);
            th.clock.join(&self.last_sc_fence);
            th.write_seqcst.join(&self.last_sc_write);
        }
        if ord != Ordering::Acquire {
            th.fence_release = th.clock;
        }
    }

    /// `get_mut`: a non-atomic write, which every atomic access must happen before. Gives the
    /// newest value, which the caller writes where `&mut` points.
    pub(crate) fn plain_write(&mut self, obj: u32, at: At) -> Result<u64, String> {
        let thread = self.tick();
        let clock = self.threads[thread].clock;
        let atomic = self.atomic_mut(obj);
        if let Some(other) = clock.behind(&atomic.accessed) {
            // alloc: a finding's or an error's text, made once, when the model stops
            return Err(format!(
                "data race: a non-atomic write (`get_mut`) at {at} by thread {thread} races with an \
                 atomic access at {} by thread {other}",
                place(atomic.accessed_at[other])
            ));
        }
        atomic.plain = Stamp { thread: thread as u8, at: clock.get(thread) };
        atomic.plain_at = Some(at);
        atomic.dirty = true;
        Ok(self.histories[obj as usize].back().expect("an atomic has a store").value)
    }

    /// `into_inner`: a non-atomic read of the newest value, which every store must happen
    /// before.
    pub(crate) fn plain_read(&mut self, obj: u32, at: At, real: u64) -> Result<u64, String> {
        let thread = self.tick();
        let clock = self.threads[thread].clock;
        if self.atomic_mut(obj).dirty {
            return Ok(real);
        }
        let newest = self.histories[obj as usize].back().expect("an atomic has a store");
        if !newest.stamp.before(&clock) {
            // alloc: a finding's or an error's text, made once, when the model stops
            return Err(format!(
                "data race: a non-atomic read (`into_inner`) at {at} by thread {thread} races \
                 with an atomic store by thread {}",
                newest.stamp.thread
            ));
        }
        Ok(newest.value)
    }

    fn cell_mut(&mut self, obj: u32) -> &mut Cell {
        match &mut self.objects[obj as usize].obj {
            Object::Cell(cell) => cell,
            _ => unreachable!("blaze-check: a cell access to another object"),
        }
    }

    /// A read of an `UnsafeCell`: the last write must happen before it.
    pub(crate) fn cell_read(&mut self, obj: u32, at: At) -> Result<(), String> {
        let thread = self.tick();
        let clock = self.threads[thread].clock;
        // alloc: called only to report a race
        self.cell_mut(obj).read(thread, &clock, at, &|| "an `UnsafeCell`".to_owned())
    }

    /// A write of an `UnsafeCell`: the last write and every read must happen before it.
    pub(crate) fn cell_write(&mut self, obj: u32, at: At) -> Result<(), String> {
        let thread = self.tick();
        let clock = self.threads[thread].clock;
        // alloc: called only to report a race
        self.cell_mut(obj).write(thread, &clock, at, &|| "an `UnsafeCell`".to_owned())
    }

    /// A non-atomic access of the bytes `addr..addr + len` (`cell::read`, `write`, `free`),
    /// checked as a cell's against every span of them, to the byte.
    pub(crate) fn bytes_access(&mut self, addr: usize, len: usize, at: At, kind: Bytes) -> Result<(), String> {
        let Some(end) = addr.checked_add(len) else {
            // alloc: a finding's or an error's text, made once, when the model stops
            return Err(format!("`cell::{kind:?}` at {at} of {len} bytes at {addr:#x} wraps the address space"));
        };
        if len == 0 {
            return Ok(());
        }
        let thread = self.tick();
        let clock = self.threads[thread].clock;
        self.bytes.split(addr);
        self.bytes.split(end);
        let (from, to) = self.bytes.range(addr, end);
        for span in &mut self.bytes.spans[from..to] {
            let start = span.start;
            // alloc: called only to report a race
            let what = || format!("the bytes at {start:#x}..{:#x}", span.end);
            if span.freed {
                let found = match kind {
                    Bytes::Read => "use after free: a read",
                    Bytes::Free => "double free: a free",
                    Bytes::Write => continue,
                };
                // alloc: a finding's or an error's text, made once, when the model stops
                return Err(format!(
                    "{found} at {at} by thread {thread} of {} freed at {} by thread {}",
                    what(),
                    place(span.shadow.write_at),
                    span.shadow.write.thread
                ));
            }
            match kind {
                Bytes::Read => span.shadow.read(thread, &clock, at, &what)?,
                Bytes::Write | Bytes::Free => span.shadow.write(thread, &clock, at, &what)?,
            }
        }
        match kind {
            Bytes::Read => self.bytes.fill(addr, end, thread, &clock, at),
            // A write over freed bytes is a new allocation there: the allocator that reused them
            // ordered it after the free, out of the model's sight.
            Bytes::Write | Bytes::Free => {
                let mut shadow = Cell::new();
                shadow.write = Stamp { thread: thread as u8, at: clock.get(thread) };
                shadow.write_at = Some(at);
                self.bytes.replace(addr, end, Span { start: addr, end, shadow, freed: kind == Bytes::Free });
            }
        }
        Ok(())
    }
}

impl Cell {
    fn read(&mut self, thread: Tid, clock: &Clock, at: At, what: &impl Fn() -> String) -> Result<(), String> {
        if !self.write.before(clock) {
            // alloc: a finding's or an error's text, made once, when the model stops
            return Err(format!(
                "data race on {}: a read at {at} by thread {thread} races with the write at {} by \
                 thread {}",
                what(),
                place(self.write_at),
                self.write.thread
            ));
        }
        self.reads.set(thread, clock.get(thread));
        self.reads_at[thread] = Some(at);
        Ok(())
    }

    fn write(&mut self, thread: Tid, clock: &Clock, at: At, what: &impl Fn() -> String) -> Result<(), String> {
        if !self.write.before(clock) {
            // alloc: a finding's or an error's text, made once, when the model stops
            return Err(format!(
                "data race on {}: a write at {at} by thread {thread} races with the write at {} by \
                 thread {}",
                what(),
                place(self.write_at),
                self.write.thread
            ));
        }
        if !self.reads.le(clock) {
            let other = clock.behind(&self.reads).expect("a read not before the clock has a thread");
            // alloc: a finding's or an error's text, made once, when the model stops
            return Err(format!(
                "data race on {}: a write at {at} by thread {thread} races with the read at {} by \
                 thread {other}",
                what(),
                place(self.reads_at[other])
            ));
        }
        self.write = Stamp { thread: thread as u8, at: clock.get(thread) };
        self.write_at = Some(at);
        Ok(())
    }
}

/// What a `cell::read`, `write` or `free` of raw bytes does.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Bytes {
    Read,
    Write,
    Free,
}

/// A run of bytes `start..end` with one shadow: every byte in it was last written, and read by
/// each thread, at the same events.
#[derive(Clone)]
pub(crate) struct Span {
    start: usize,
    end: usize,
    shadow: Cell,
    /// Freed by `cell::free`: a read is a use after free, a write a new allocation.
    freed: bool,
}

/// The tracked raw bytes of an execution, as runs sorted by their first address, none
/// overlapping: a write of a whole buffer is one run, and a run is split only where accesses
/// differ (an interval map, where a shadow per byte or per 8-byte granule would cost an entry
/// each, or race falsely across a published boundary inside a granule). A sorted `Vec` cleared
/// per execution, so a warm execution allocates nothing for it; a model tracks tens of runs,
/// where a binary search and a short move beat a tree's nodes.
#[derive(Default)]
pub(crate) struct ByteMap {
    spans: Vec<Span>,
    /// The gaps one read fills, reused.
    scratch: Vec<(usize, usize)>,
}

impl ByteMap {
    pub(crate) fn clear(&mut self) {
        self.spans.clear();
    }

    /// The runs that start in `start..end`, as a range of `spans`.
    fn range(&self, start: usize, end: usize) -> (usize, usize) {
        let from = self.spans.partition_point(|it| it.start < start);
        let to = from + self.spans[from..].partition_point(|it| it.start < end);
        (from, to)
    }

    /// Makes `at` a boundary between runs, if a run covers it.
    fn split(&mut self, at: usize) {
        let after = self.spans.partition_point(|it| it.start < at);
        let Some(before) = after.checked_sub(1) else { return };
        let span = &mut self.spans[before];
        if span.end <= at {
            return;
        }
        // clone: a split run's right half, a plain copy of its shadow
        let mut right = span.clone();
        span.end = at;
        right.start = at;
        self.spans.insert(after, right);
    }

    /// Puts one run over `start..end`, whose edges are boundaries.
    fn replace(&mut self, start: usize, end: usize, span: Span) {
        debug_assert!(span.start == start && span.end == end);
        let (from, to) = self.range(start, end);
        if from == to {
            self.spans.insert(from, span);
        } else {
            self.spans[from] = span;
            self.spans.drain(from + 1..to);
        }
    }

    /// Records a read by `thread` over `start..end`, whose edges are boundaries: never-written
    /// gaps get runs of their own first (an initial value happens before everything).
    fn fill(&mut self, start: usize, end: usize, thread: Tid, clock: &Clock, at: At) {
        self.scratch.clear();
        let (from, to) = self.range(start, end);
        let mut cursor = start;
        for span in &self.spans[from..to] {
            if span.start > cursor {
                self.scratch.push((cursor, span.start));
            }
            cursor = span.end;
        }
        if cursor < end {
            self.scratch.push((cursor, end));
        }
        for &(gap, gap_end) in &self.scratch {
            let index = self.spans.partition_point(|it| it.start < gap);
            self.spans.insert(index, Span { start: gap, end: gap_end, shadow: Cell::new(), freed: false });
        }
        let (from, to) = self.range(start, end);
        for span in &mut self.spans[from..to] {
            span.shadow.reads.set(thread, clock.get(thread));
            span.shadow.reads_at[thread] = Some(at);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::panic::Location;

    use super::Bytes;
    use crate::{
        clock::Stamp,
        path::{Mode, Path},
        rng::Rng,
        rt::Runtime,
    };

    /// The runs against a shadow per byte (its last write, and thread 0's last read), over
    /// seeded writes and reads of a 64-byte buffer.
    #[test]
    fn the_byte_map_keeps_what_a_shadow_per_byte_keeps() {
        const BASE: usize = 0x1000;
        for seed in 0..32 {
            let mut rt = Runtime::new(Path::new(Mode::Explore, None, 100), 1);
            rt.begin();
            let mut rng = Rng::new(seed);
            let mut bytes: [Option<(Stamp, u32)>; 64] = [None; 64];
            for _ in 0..200 {
                let start = rng.index(64);
                let end = start + 1 + rng.index(64 - start);
                let write = rng.coin();
                let kind = if write { Bytes::Write } else { Bytes::Read };
                assert!(rt.bytes_access(BASE + start, end - start, Location::caller(), kind).is_ok());
                let now = rt.threads[0].clock.get(0);
                for byte in &mut bytes[start..end] {
                    *byte = match (write, *byte) {
                        (true, _) => Some((Stamp { thread: 0, at: now }, 0)),
                        (false, None) => Some((Stamp::INIT, now)),
                        (false, Some((wrote, _))) => Some((wrote, now)),
                    };
                }
                let spans = &rt.bytes.spans;
                assert!(spans.iter().all(|it| it.start < it.end));
                assert!(spans.windows(2).all(|it| it[0].end <= it[1].start), "runs overlap");
                for (at, &byte) in bytes.iter().enumerate() {
                    let span = spans.iter().find(|it| it.start <= BASE + at && BASE + at < it.end);
                    let kept = span.map(|it| (it.shadow.write, it.shadow.reads.get(0)));
                    assert_eq!(kept, byte, "byte {at} after seed {seed}");
                }
            }
        }
    }
}
