//! One arena of a snapshot (a [`Slot`]): its memory, its table, its column of ids, the set of
//! arenas it borrowed from, and who may write to it.

use std::{
    alloc::Global,
    cell::UnsafeCell,
    mem::MaybeUninit,
    ptr,
    sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering},
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

/// A destructor on the arena's drop list, run when the arena drops: an input's fields, an
/// `owns` value, a `keep_owned` value ([`Slot::drop_owned`], the one way onto the list).
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
    /// One bit per arena index the driver declared this arena borrows from
    /// ([`crate::Inputs::borrows`]), in the arena: its direct dependencies and the session.
    declared: *const AtomicU64,
    /// The closure of `declared` over the declared sets of the arenas it names, made once when
    /// the snapshot that added this arena is built ([`Slot::close`]): the arenas a key homed
    /// here may name, and the arenas a run of an imaged query homed here may read.
    reach: *const AtomicU64,
    borrow_words: usize,
    /// Whether `reach` is made: set once, by the build of the snapshot that added the arena.
    closed: AtomicBool,
    /// The driver sealed this arena ([`crate::Inputs::seal`]): its sources are pinned. A write
    /// of its inputs names it changed like any arena, and [`crate::Memo::next`] drops it with
    /// its borrowers ([`crate::Memo::changed`]).
    sealed: AtomicBool,
    /// The driver opened this arena ([`crate::Inputs::open`]): a key homed in any arena may
    /// name its ids, each such entry borrowing it when written ([`crate::home::mark_borrows`]).
    open: AtomicBool,
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
        let (borrows, declared, reach) = {
            let local = arena.local();
            let words =
                || local.alloc_slice_fill_with(borrow_words, |_| AtomicU64::new(0)).as_ptr();
            (words(), words(), words())
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
            declared,
            reach,
            borrow_words,
            closed: AtomicBool::new(false),
            sealed: AtomicBool::new(false),
            open: AtomicBool::new(false),
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

    fn declared_words(&self) -> &[AtomicU64] {
        // SAFETY: as for `borrow_words`.
        unsafe { std::slice::from_raw_parts(self.declared, self.borrow_words) }
    }

    fn reach_words(&self) -> &[AtomicU64] {
        // SAFETY: as for `borrow_words`.
        unsafe { std::slice::from_raw_parts(self.reach, self.borrow_words) }
    }

    /// Notes the driver's declaration that this arena's inputs and entries may name and read
    /// arena `from` (a direct dependency, or the session): a borrow, and a member of the closure
    /// [`Slot::reaches`] answers for.
    ///
    /// # Panics
    ///
    /// Where `from`'s index is not below this arena's: the driver numbers a crate's arena
    /// `1 + its topological position` (0 is the session's), so a dependency's index is always
    /// below its dependent's. This is the cheap check of that contract.
    pub(crate) fn declare(&self, from: ArenaId, epoch: u64) {
        assert!(
            from.index() < self.id.index(),
            "{:?} declared a borrow of {from:?}, whose index is not below its own: the driver \
             gives each crate the arena index 1 + its topological position (0 is the session's), \
             so a dependency comes before its dependent",
            self.id
        );
        assert!(
            !self.closed.load(Ordering::Acquire),
            "{:?} declared a borrow of {from:?} after its snapshot was built",
            self.id
        );
        let index = from.index() as usize;
        let Some(word) = self.declared_words().get(index / 64) else {
            panic!("{:?} declared {from:?}, past the session's arena indices", self.id);
        };
        word.fetch_or(1 << (index % 64), Ordering::Relaxed);
        self.borrow(from, epoch);
    }

    /// The declared indices, ascending.
    pub(crate) fn declared(&self) -> impl Iterator<Item = u32> + '_ {
        bits(self.declared_words())
    }

    /// Makes this arena's closure from its declared set and the closures of the arenas it
    /// names (each closed before it: their indices are below this one's). Once per arena.
    pub(crate) fn close<'a>(&self, of: impl Fn(u32) -> &'a Slot) {
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        let reach = self.reach_words();
        for index in self.declared() {
            let dep = of(index);
            debug_assert!(dep.closed.load(Ordering::Acquire), "a dependency is closed first");
            reach[index as usize / 64].fetch_or(1 << (index % 64), Ordering::Relaxed);
            for (word, theirs) in reach.iter().zip(dep.reach_words()) {
                word.fetch_or(theirs.load(Ordering::Relaxed), Ordering::Relaxed);
            }
        }
        self.closed.store(true, Ordering::Release);
    }

    /// Whether `arena` is this arena or in its declared closure: what a key homed here may name.
    #[inline]
    pub(crate) fn reaches(&self, arena: ArenaId) -> bool {
        let index = arena.index();
        // Its own; the session's (index 0) only where declared, as any other (a settings change
        // drops the session arena, and a carried arena naming its ids would hold dead ones).
        if index == self.id.index() {
            return true;
        }
        self.reach_words()
            .get(index as usize / 64)
            .is_some_and(|word| word.load(Ordering::Relaxed) & (1 << (index % 64)) != 0)
    }

    pub(crate) fn seal(&self) {
        self.sealed.store(true, Ordering::Release);
    }

    pub(crate) fn is_sealed(&self) -> bool {
        self.sealed.load(Ordering::Acquire)
    }

    /// Opens this arena to every arena's keys ([`crate::Inputs::open`]).
    ///
    /// # Panics
    ///
    /// Where the snapshot that added it is built already.
    pub(crate) fn open(&self) {
        assert!(!self.closed.load(Ordering::Acquire), "{:?} opened after its snapshot was built", self.id);
        self.open.store(true, Ordering::Release);
    }

    pub(crate) fn is_open(&self) -> bool {
        self.open.load(Ordering::Acquire)
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
        // Below `SPILL - 1`, so a spill id's local plus one still fits (`Gid`'s niche).
        assert!(local < SPILL - 1, "{:?} holds 2^31 interned values", self.id);
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
        self.set_local_raw(local, ptr::from_ref(head).cast_mut());
    }

    /// [`Slot::set_local`] of a pointer that keeps its allocation's write permission (an
    /// input's, made from the arena's `&mut` and never reborrowed shared): what
    /// [`Slot::local_raw`] gives [`crate::Memo::input_mut`] back.
    pub(crate) fn set_local_raw(&self, local: u32, head: *mut Head) {
        let cell = self.place(local, true).expect("a column cell for every id under 2^31");
        cell.store(head, Ordering::Release);
    }

    /// The pointer `local`'s head was stored as, if it was.
    pub(crate) fn local_raw(&self, local: u32) -> Option<*mut Head> {
        let head = self.place(local, false)?.load(Ordering::Acquire);
        (!head.is_null()).then_some(head)
    }

    pub(crate) fn local(&self, local: u32) -> Option<&Head> {
        let head = self.place(local, false)?.load(Ordering::Acquire);
        // SAFETY: a stored head is published by release and lives as long as the arena.
        (!head.is_null()).then(|| unsafe { &*head })
    }

    /// Runs `drop` on `at` when the arena drops.
    fn drop_with(&self, at: *mut u8, drop: unsafe fn(*mut u8)) {
        self.drops.lock().push(InputDrop { at, drop });
    }

    /// Drops the `T` at `at` when the arena drops, where `T` needs it: an `owns` value
    /// (`#[tracked(owns)]`, `#[interned(owns)]`), a [`crate::keep_owned`] value, an input's
    /// fields. The one mechanism for all of them.
    ///
    /// # Safety
    ///
    /// `at` is a `T` in this arena, registered once, and never dropped or moved out otherwise.
    pub(crate) unsafe fn drop_owned<T>(&self, at: *const T) {
        if std::mem::needs_drop::<T>() {
            // SAFETY (of the destructor): called once, when the arena drops, on this value.
            unsafe fn drop_value<T>(at: *mut u8) {
                // SAFETY: the caller of `drop_owned`'s: a `T` not dropped before.
                unsafe { ptr::drop_in_place(at.cast::<T>()) };
            }
            self.drop_with(at.cast_mut().cast(), drop_value::<T>);
        }
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        // Last listed, first dropped: a value can borrow only what was made before it, so each
        // destructor runs while everything its value may borrow in this arena is undropped.
        for it in self.drops.get_mut().drain(..).rev() {
            // SAFETY: each was registered once, for fields in this arena not dropped before.
            unsafe { (it.drop)(it.at) };
        }
    }
}

/// The indices of the bits set in `words`, ascending.
fn bits(words: &[AtomicU64]) -> impl Iterator<Item = u32> + '_ {
    words.iter().enumerate().flat_map(|(at, word)| {
        let mut left = word.load(Ordering::Acquire);
        std::iter::from_fn(move || {
            if left == 0 {
                return None;
            }
            let bit = left.trailing_zeros();
            left &= left - 1;
            Some(at as u32 * 64 + bit)
        })
    })
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
///
/// Never inlined, as every read of a thread-local in this crate is (`task::innermost_ptr` says
/// why): a fetch allocates before its run's function and after it, the function may park the
/// fiber and resume on another worker, and an inlined copy may keep the first worker's
/// `LOCALS` address for the second allocation: two threads in one bump allocator.
#[inline(never)]
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
