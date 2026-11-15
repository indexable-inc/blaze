//! One arena of a snapshot (a [`Slot`]): its memory, its table, its column of ids, the set of
//! arenas it borrowed from, and who may write to it.

use std::{
    alloc::Global,
    cell::UnsafeCell,
    mem::MaybeUninit,
    ptr,
    sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, Ordering},
};

use blaze_core::lock::Lock;
use blaze_mem::{Arena, ArenaLocal, Tag};

use crate::{
    ArenaId, SPILL,
    trie::{Head, Trie},
};

/// The column's first segment holds `1 << FIRST` ids; segment `k` holds `1 << (FIRST + k)`.
const FIRST: u32 = 6;
const SEGMENTS: usize = 26;

/// How long [`Slot::freeze`] waits out writers, in spins of a few nanoseconds each: a writer
/// holds the count across one table insert, never across a run.
const FREEZE_SPINS: u64 = 1 << 32;

/// Each arena's serial, so a thread's cached [`ArenaLocal`] is never used for another arena
/// made at the same address.
static SERIALS: AtomicU64 = AtomicU64::new(1);

/// A destructor of an input's fields, run when its arena drops (an input is the driver's and
/// may own memory; a memo value never does).
struct InputDrop {
    at: *mut u8,
    drop: unsafe fn(*mut u8),
}

pub(crate) struct Slot {
    pub(crate) id: ArenaId,
    serial: u64,
    /// The epoch of the one snapshot that may write here; a predecessor that shares it writes
    /// to its spill table instead.
    epoch: AtomicU64,
    /// Writers between their epoch check and their insert's end.
    writers: AtomicU32,
    pub(crate) trie: Trie,
    /// Index `local` to its entry's head.
    segments: [AtomicPtr<AtomicPtr<Head>>; SEGMENTS],
    next_local: AtomicU32,
    /// One bit per arena index this arena's entries read from, in the arena.
    borrows: *const AtomicU64,
    borrow_words: usize,
    /// The inputs' destructors, filled while the snapshot is built.
    drops: Lock<Vec<InputDrop, Global>>,
    /// Declared last: dropped after every field that points into it.
    arena: Arena,
}

// SAFETY: every field written after the slot is shared is atomic or behind `drops`' lock; the
// raw pointers point into `arena`, which the slot owns.
unsafe impl Send for Slot {}
// SAFETY: as for `Send`.
unsafe impl Sync for Slot {}

impl Slot {
    /// An empty arena `id` for the snapshot of `epoch`, with a borrow set for `capacity`
    /// arena indices.
    pub(crate) fn new(id: ArenaId, tag: Tag, epoch: u64, capacity: u32) -> Slot {
        let arena = Arena::new(tag);
        let borrow_words = (capacity as usize).div_ceil(64);
        let borrows = {
            let local = arena.local();
            local.alloc_slice_fill_with(borrow_words, |_| AtomicU64::new(0)).as_ptr()
        };
        Slot {
            id,
            serial: SERIALS.fetch_add(1, Ordering::Relaxed),
            epoch: AtomicU64::new(epoch),
            writers: AtomicU32::new(0),
            trie: Trie::new(),
            segments: [const { AtomicPtr::new(ptr::null_mut()) }; SEGMENTS],
            next_local: AtomicU32::new(0),
            borrows,
            borrow_words,
            drops: Lock::new("memo.arena.drops", Vec::new_in(Global)),
            arena,
        }
    }

    pub(crate) fn held(&self) -> usize {
        self.arena.held()
    }

    fn borrow_words(&self) -> &[AtomicU64] {
        // SAFETY: `borrow_words` words in the slot's own arena, made in `new`.
        unsafe { std::slice::from_raw_parts(self.borrows, self.borrow_words) }
    }

    /// Notes that an entry of this arena read from arena `from`, for the snapshot of `epoch`.
    /// Written once per pair: a set bit is only read again.
    pub(crate) fn borrow(&self, from: ArenaId, epoch: u64) {
        if from.index() == self.id.index() || self.epoch.load(Ordering::Relaxed) != epoch {
            return;
        }
        let index = from.index() as usize;
        let words = self.borrow_words();
        let Some(word) = words.get(index / 64) else {
            panic!(
                "{:?} read {from:?}, past the {} arena indices the session was made for \
                 (`Config::arenas`)",
                self.id,
                words.len() * 64
            );
        };
        let bit = 1 << (index % 64);
        if word.load(Ordering::Relaxed) & bit == 0 {
            word.fetch_or(bit, Ordering::AcqRel);
        }
    }

    /// Whether this arena borrowed from any arena index set in `dropped`.
    pub(crate) fn borrowed_any(&self, dropped: &[u64]) -> bool {
        self.borrow_words()
            .iter()
            .zip(dropped)
            .any(|(word, dropped)| word.load(Ordering::Acquire) & dropped != 0)
    }

    pub(crate) fn writable_by(&self, epoch: u64) -> bool {
        self.epoch.load(Ordering::Acquire) == epoch
    }

    /// Runs `insert` if the snapshot of `epoch` still owns this arena: `None` when a successor
    /// took it, and the caller writes to its spill table.
    pub(crate) fn write<R>(&self, epoch: u64, insert: impl FnOnce() -> R) -> Option<R> {
        struct Out<'a>(&'a AtomicU32);
        impl Drop for Out<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::AcqRel);
            }
        }
        self.writers.fetch_add(1, Ordering::SeqCst);
        let _out = Out(&self.writers);
        if self.epoch.load(Ordering::SeqCst) != epoch {
            return None;
        }
        Some(insert())
    }

    /// Gives this arena to the snapshot of `epoch`: after it returns, no predecessor writes to
    /// it. Waits out the writers already past their check, each inside one insert.
    pub(crate) fn freeze(&self, epoch: u64) {
        self.epoch.store(epoch, Ordering::SeqCst);
        let mut spins = 0_u64;
        while self.writers.load(Ordering::SeqCst) != 0 {
            spins += 1;
            assert!(
                spins < FREEZE_SPINS,
                "{:?}: a writer held its insert for {FREEZE_SPINS} spins while the next snapshot \
                 was made: an insert ran user code or stopped",
                self.id
            );
            std::hint::spin_loop();
        }
    }

    /// A fresh index in this arena's column (the spill table's carry [`SPILL`]).
    pub(crate) fn reserve_local(&self, spill: bool) -> u32 {
        let local = self.next_local.fetch_add(1, Ordering::Relaxed);
        assert!(local < SPILL, "{:?} holds 2^31 interned values", self.id);
        if spill { local | SPILL } else { local }
    }

    /// Where `local`'s head is kept.
    fn place(&self, local: u32, make: bool) -> Option<&AtomicPtr<Head>> {
        let at = (local & !SPILL) + (1 << FIRST);
        let segment = (31 - at.leading_zeros() - FIRST) as usize;
        let offset = (at - (1 << (segment as u32 + FIRST))) as usize;
        let cell = self.segments.get(segment)?;
        let mut base = cell.load(Ordering::Acquire);
        if base.is_null() {
            if !make {
                return None;
            }
            let len = 1_usize << (segment as u32 + FIRST);
            let made = with_local(self, |local| {
                local
                    .alloc_slice_fill_with(len, |_| AtomicPtr::new(ptr::null_mut()))
                    .as_mut_ptr()
            });
            base = match cell.compare_exchange(
                ptr::null_mut(),
                made,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => made,
                Err(theirs) => theirs,
            };
        }
        // SAFETY: segment `segment` holds `1 << (segment + FIRST)` cells, `offset` below it.
        Some(unsafe { &*base.add(offset) })
    }

    /// Keeps `head` as `local`'s, before any reader can be handed `local`.
    pub(crate) fn set_local(&self, local: u32, head: &Head) {
        let cell = self.place(local, true).expect("a column cell for every id under 2^31");
        cell.store(ptr::from_ref(head).cast_mut(), Ordering::Release);
    }

    pub(crate) fn local(&self, local: u32) -> Option<&Head> {
        let head = self.place(local, false)?.load(Ordering::Acquire);
        // SAFETY: a stored head is published by release and lives as long as the arena.
        (!head.is_null()).then(|| unsafe { &*head })
    }

    /// Runs `drop` on `at` when the arena drops.
    pub(crate) fn drop_with(&self, at: *mut u8, drop: unsafe fn(*mut u8)) {
        self.drops.lock().push(InputDrop { at, drop });
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        for it in self.drops.get_mut().drain(..) {
            // SAFETY: each was registered once, for fields in this arena not dropped before.
            unsafe { (it.drop)(it.at) };
        }
    }
}

/// How many arenas a thread keeps a bump allocator into at once.
const WAYS: usize = 8;

/// A thread's bump allocators, one per recently used arena: a run allocates into its home
/// arena through the thread's, so a 64 KiB block is taken per (thread, arena), not per run.
/// The cost: an arena a thread stops using strands at most one block's unused tail, and
/// replacing a way strands that way's tail.
struct Locals {
    serial: [u64; WAYS],
    local: [MaybeUninit<ArenaLocal<'static>>; WAYS],
    next: usize,
    busy: bool,
}

thread_local! {
    static LOCALS: UnsafeCell<Locals> = const {
        UnsafeCell::new(Locals {
            serial: [0; WAYS],
            local: [const { MaybeUninit::uninit() }; WAYS],
            next: 0,
            busy: false,
        })
    };
}

/// Runs `f` with this thread's bump allocator into `slot`'s arena. `f` runs no user code (an
/// allocation, a table insert); a nested call panics.
pub(crate) fn with_local<R>(slot: &Slot, f: impl FnOnce(&ArenaLocal<'_>) -> R) -> R {
    LOCALS.with(|cell| {
        // SAFETY: the thread's own, and `busy` refuses a second borrow below.
        let locals = unsafe { &mut *cell.get() };
        assert!(!locals.busy, "a memo allocation inside another on one thread");
        locals.busy = true;
        struct Free<'a>(&'a mut bool);
        impl Drop for Free<'_> {
            fn drop(&mut self) {
                *self.0 = false;
            }
        }
        let way = match locals.serial.iter().position(|it| *it == slot.serial) {
            Some(way) => way,
            None => {
                let way = locals.next;
                locals.next = (way + 1) % WAYS;
                let local = slot.arena.local();
                // SAFETY: the local is used only while its serial is `slot.serial`, which no
                // other arena has, and only by a caller holding the slot, so the arena it points
                // into is alive whenever it is used; `ArenaLocal` has no destructor, so an
                // entry left for a dead arena is never touched again.
                let local: ArenaLocal<'static> = unsafe { std::mem::transmute(local) };
                locals.local[way] = MaybeUninit::new(local);
                locals.serial[way] = slot.serial;
                way
            }
        };
        // SAFETY: written above or when the serial was set.
        let local = unsafe { locals.local[way].assume_init_ref() };
        let _free = Free(&mut locals.busy);
        f(local)
    })
}
