//! Revision arenas, double-buffered (docs/v2/plan/design/memory.md, "a view's revision"): the
//! next revision is built in one arena while readers read the current one from the other.
//! Publishing flips which is current with one release store; the arena of the revision before
//! is reused only once no task can still be reading it, which [`blaze_reclaim`] says at task
//! boundaries (docs/v2/plan/design/concurrency.md "Task boundaries reclaim memory").
//!
//! A revision's value borrows its arena: [`Built::Value<'a>`] may hold `&'a` references into
//! memory it allocated there. Readers get it for as long as their task's guard, never longer.
//!
//! Cost: a read is one acquire load of the current revision's record (its number and value,
//! side by side in its arena) and no shared write; a publish is one arena reset (its chunks
//! back to the cache), the build, and two release stores.

use std::{
    cell::UnsafeCell,
    marker::PhantomData,
    ptr,
    sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering},
};

use blaze_reclaim::{Grace, Guard};

use crate::{Arena, ArenaLocal, Tag};

/// What one revision is: the type of its value, which borrows the revision's arena.
pub trait Built: 'static {
    type Value<'a>: Send + Sync;

    /// Shortens the value's borrow of its arena. Write it as `fn shorten<'s, 'l: 's>(value:
    /// &'s Self::Value<'l>) -> &'s Self::Value<'s> { value }`: it compiles only if `Value` is
    /// covariant in its lifetime, which reading a revision for less than its life relies on.
    fn shorten<'s, 'l: 's>(value: &'s Self::Value<'l>) -> &'s Self::Value<'s>;
}

/// Why a publish did not happen.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PublishError {
    /// A task that began before the last publish may still read the revision before it, whose
    /// arena the next revision would reuse: publish again after that task's boundary.
    Readers,
    /// Another thread is publishing.
    Busy,
}

impl std::fmt::Display for PublishError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            PublishError::Readers => {
                "the revision before the current one may still be read by a task"
            }
            PublishError::Busy => "another thread is publishing a revision",
        })
    }
}

impl std::error::Error for PublishError {}

/// A revision as placed in its arena: what a reader reaches with one load.
struct Placed<'a, B: Built> {
    number: u64,
    value: B::Value<'a>,
}

struct Slot<B: Built> {
    arena: UnsafeCell<Arena>,
    /// The revision's record, in its arena; null when the slot holds none.
    value: AtomicPtr<Placed<'static, B>>,
    /// Started when the slot stopped being current; its arena is reused once it has passed.
    retired: UnsafeCell<Option<Grace>>,
}

/// Two arenas, one revision current in each turn.
pub struct Revisions<B: Built> {
    slots: [Slot<B>; 2],
    /// The current revision's record; null before the first publish. What readers load.
    current: AtomicPtr<Placed<'static, B>>,
    /// The current revision's number shifted left by one, its slot in the low bit; 0 before
    /// the first publish. The publisher's, and [`Revisions::number`]'s.
    state: AtomicU64,
    publishing: AtomicBool,
    _built: PhantomData<B>,
}

// SAFETY: readers share only `&B::Value`, which is `Sync`; a slot's arena and grace are touched
// by the one publisher (`publishing`) and only while no reader can reach that slot (its grace
// has passed or it never held a value); values are `Send`, built on the publisher's thread and
// dropped on whichever thread publishes or drops later.
unsafe impl<B: Built> Sync for Revisions<B> {}
// SAFETY: as for `Sync`.
unsafe impl<B: Built> Send for Revisions<B> {}

/// The current revision, read for as long as the reader's task guard lives.
pub struct Current<'g, B: Built> {
    pub number: u64,
    pub value: &'g B::Value<'g>,
}

impl<B: Built> Revisions<B> {
    pub const fn new(tag: Tag) -> Revisions<B> {
        Revisions {
            slots: [Slot::new(tag), Slot::new(tag)],
            current: AtomicPtr::new(ptr::null_mut()),
            state: AtomicU64::new(0),
            publishing: AtomicBool::new(false),
            _built: PhantomData,
        }
    }

    /// The current revision, if one was published. `guard` is the calling thread's task (a
    /// `Guard` is not `Send`, so it is this thread's): the value stays valid while it lives.
    pub fn read<'g>(&'g self, _guard: &'g Guard) -> Option<Current<'g, B>> {
        let placed = self.current.load(Ordering::Acquire);
        if placed.is_null() {
            return None;
        }
        // SAFETY: `placed` was published with a release store after it was built and is
        // dropped only after a grace started once it stopped being current has passed; this
        // thread is in a task (`guard`) that began before such a grace or sees the newer
        // revision, so the record outlives `'g`. It was built borrowing its arena, which lives
        // as long as the slot, so reading it as `'static` and shortening to `'g` is sound.
        let placed: &'g Placed<'static, B> = unsafe { &*placed };
        Some(Current { number: placed.number, value: B::shorten(&placed.value) })
    }

    /// The number of the current revision; 0 before the first publish.
    pub fn number(&self) -> u64 {
        self.state.load(Ordering::Acquire) >> 1
    }

    /// Builds the next revision with `build`, in the arena the revision before the current
    /// one used, and makes it current. Errs, building nothing, when a task may still read that
    /// revision or another thread publishes at once. Returns the new revision's number.
    pub fn publish(
        &self,
        build: impl for<'a> FnOnce(&ArenaLocal<'a>) -> B::Value<'a>,
    ) -> Result<u64, PublishError> {
        if self.publishing.swap(true, Ordering::Acquire) {
            return Err(PublishError::Busy);
        }
        struct Unlock<'r>(&'r AtomicBool);
        impl Drop for Unlock<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }
        let _unlock = Unlock(&self.publishing);

        let current = self.state.load(Ordering::Acquire);
        let next_slot = if current == 0 { 0 } else { 1 - (current & 1) as usize };
        let slot = &self.slots[next_slot];
        // SAFETY: only the publisher (this thread, by `publishing`) reads or writes `retired`.
        let retired = unsafe { &mut *slot.retired.get() };
        if let Some(grace) = *retired {
            if !grace.passed() {
                return Err(PublishError::Readers);
            }
            *retired = None;
        }
        // SAFETY: the slot is not current and its grace has passed (or it never held a value),
        // so no reader holds its value or anything in its arena; the publisher is alone here.
        let arena = unsafe { &mut *slot.arena.get() };
        let old = slot.value.swap(ptr::null_mut(), Ordering::Relaxed);
        if !old.is_null() {
            // SAFETY: `old` is the record this slot's last publish placed in its arena, unread
            // now (above), dropped once.
            unsafe { ptr::drop_in_place(old) };
        }
        arena.reset();

        let arena: &Arena = arena;
        let local = arena.local();
        let number = (current >> 1) + 1;
        let value = build(&local);
        let placed: *mut Placed<'_, B> = local.alloc(Placed { number, value });
        drop(local);
        // The record borrows the arena, which lives as long as `self`; it is stored with its
        // lifetime erased and handed out again only through `read`, shortened to a guard's.
        let placed = placed.cast::<Placed<'static, B>>();
        slot.value.store(placed, Ordering::Relaxed);

        // Release: the record's bytes are seen by a reader that loads the pointer.
        self.current.store(placed, Ordering::Release);
        self.state.store((number << 1) | next_slot as u64, Ordering::Release);
        if current != 0 {
            let before = &self.slots[(current & 1) as usize];
            // SAFETY: the publisher alone touches `retired`.
            unsafe { *before.retired.get() = Some(Grace::start()) };
        }
        Ok(number)
    }
}

impl<B: Built> Slot<B> {
    const fn new(tag: Tag) -> Slot<B> {
        Slot {
            arena: UnsafeCell::new(Arena::new(tag)),
            value: AtomicPtr::new(ptr::null_mut()),
            retired: UnsafeCell::new(None),
        }
    }
}

impl<B: Built> Drop for Revisions<B> {
    fn drop(&mut self) {
        for slot in &mut self.slots {
            let value = *slot.value.get_mut();
            if !value.is_null() {
                // SAFETY: `&mut self`: no reader is left, and the value is dropped once,
                // before its arena.
                unsafe { ptr::drop_in_place(value) };
            }
            slot.arena.get_mut().reset();
        }
    }
}
