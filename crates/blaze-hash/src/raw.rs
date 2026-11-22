//! The owning SwissTable under [`Map`](crate::Map), [`Set`](crate::Set) and
//! [`OrderMap`](crate::OrderMap)'s index: values of `T` in slots of groups of [`SLOTS`], each
//! group's tags in one control word, probed by `blaze_swiss`'s [`find`] and [`vacant`]. It
//! hashes nothing itself: every call that places a value takes the value's hash, and every
//! call that may move values takes the hasher as a closure, so one table serves a map of keys
//! and a table of indices into a column.
//!
//! One allocation in the table's allocator holds the control words, then the slots. A slot's
//! tag is [`EMPTY`], [`DEAD`] (its value was removed) or [`live_tag`] of its value's hash.
//! [`find`] stops at the first group with an empty slot, so the table never fills: at most
//! 7 of each group's 8 slots are used on average ([`limit`]), counting dead ones.
//!
//! Removal and the probe's stop: a value is placed in the first group from its home that had an
//! empty slot then, so every group its probe passed was full when it was placed. A removal
//! leaves its slot [`EMPTY`] only if its group already had an empty slot, and [`DEAD`] if the
//! group was full; so a group that was ever full gets no empty slot back until the table is
//! made again, and no probe stops short of the value it seeks. Dead slots are reclaimed when
//! the table is made again: at the same size when at most half its limit is live, else twice.
//!
//! Iteration is in slot order, which is a function of the inserts, the removals and the
//! hashes alone: the same calls with the same hasher give the same order on every run.

use core::{
    alloc::{Allocator, Layout},
    marker::PhantomData,
    mem,
    ptr::{self, NonNull},
};

use blaze_own::AllocFailure;
use blaze_swiss::{At, Ctrl, DEAD, EMPTY, Slots, find, find_or_vacant, home, live_tag, vacant};

/// Slots per group: one control word holds eight tags.
pub(crate) const SLOTS: usize = 8;

/// The cache line: 128 bytes on Apple's arm64 cores, 64 elsewhere.
const LINE: usize =
    if cfg!(all(target_arch = "aarch64", target_vendor = "apple")) { 128 } else { 64 };

/// How many keys the slice forms (`get_many_into`) prefetch ahead of the one they probe:
/// with a home word and a slot line each, eight keys keep about the 30 misses an Apple core
/// overlaps in flight. `examples/hash_cost.rs batch` on an M-series Mac, 10^7 entries, hits:
/// depth 8 at 8.9 to 9.3 ns a lookup, depth 16 at 10.2, one at a time 11.9.
pub(crate) const BATCH: usize = 8;

type Word = Ctrl<SLOTS>;

/// The values a table of `groups` groups holds before it is made again: seven of every eight
/// slots, so at least one slot of every eight stays empty and every probe ends.
#[inline]
const fn limit(groups: usize) -> usize {
    groups * (SLOTS - 1)
}

/// What a table of values of `T` asks for when it cannot have memory: its name in the message.
const WHAT: &str = "blaze_hash table";

/// Stops with `failure`, as blaze-own's infallible calls do (rule panics-fail-one-job).
#[cold]
#[inline(never)]
#[track_caller]
pub(crate) fn fail(failure: AllocFailure) -> ! {
    panic!("blaze_hash: {failure}")
}

/// The groups that hold `capacity` values: 0 for none, else a power of two.
fn groups_for(capacity: usize) -> Result<usize, AllocFailure> {
    if capacity == 0 {
        return Ok(0);
    }
    let overflow = AllocFailure::CapacityOverflow { what: WHAT, len: 0, additional: capacity };
    capacity.div_ceil(SLOTS - 1).checked_next_power_of_two().ok_or(overflow)
}

/// The allocation of `groups` groups of `T`, and the offset of the slots in it.
fn layout<T>(groups: usize) -> Result<(Layout, usize), AllocFailure> {
    let overflow = AllocFailure::CapacityOverflow { what: WHAT, len: groups, additional: 0 };
    let words = Layout::array::<u64>(groups).map_err(|_| overflow)?;
    let slots = groups.checked_mul(SLOTS).ok_or(overflow)?;
    let slots = Layout::array::<T>(slots).map_err(|_| overflow)?;
    words.extend(slots).map_err(|_| overflow)
}

/// A SwissTable of `T` in `A`.
pub(crate) struct RawTable<T, A: Allocator> {
    /// `groups` control words; dangling when `groups` is 0.
    ctrl: NonNull<u64>,
    /// `groups * SLOTS` slots, initialized exactly where the slot's tag is live.
    slots: NonNull<T>,
    /// 0, or a power of two.
    groups: usize,
    /// Live values.
    items: usize,
    /// Empty slots that may still be filled before the table is made again.
    growth_left: usize,
    alloc: A,
    marker: PhantomData<T>,
}

// SAFETY: the table owns its values and its allocator as a `Vec<T, A>` does.
unsafe impl<T: Send, A: Allocator + Send> Send for RawTable<T, A> {}
// SAFETY: `&RawTable` hands out only `&T` and `&A`.
unsafe impl<T: Sync, A: Allocator + Sync> Sync for RawTable<T, A> {}

impl<T, A: Allocator> RawTable<T, A> {
    /// An empty table: no allocation.
    #[inline]
    pub(crate) const fn new_in(alloc: A) -> RawTable<T, A> {
        RawTable {
            ctrl: NonNull::dangling(),
            slots: NonNull::dangling(),
            groups: 0,
            items: 0,
            growth_left: 0,
            alloc,
            marker: PhantomData,
        }
    }

    /// An empty table with room for `capacity` values; a refused allocation is a panic naming
    /// it.
    pub(crate) fn with_capacity_in(capacity: usize, alloc: A) -> RawTable<T, A> {
        match RawTable::try_with_capacity_in(capacity, alloc) {
            Ok(table) => table,
            Err(failure) => fail(failure),
        }
    }

    pub(crate) fn try_with_capacity_in(
        capacity: usize,
        alloc: A,
    ) -> Result<RawTable<T, A>, AllocFailure> {
        let groups = groups_for(capacity)?;
        let mut table = RawTable::new_in(alloc);
        if groups != 0 {
            let (ctrl, slots) = allocate::<T, A>(&table.alloc, groups)?;
            table.ctrl = ctrl;
            table.slots = slots;
            table.groups = groups;
            table.growth_left = limit(groups);
        }
        Ok(table)
    }

    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.items
    }

    /// The values it holds before it is made again.
    #[inline]
    pub(crate) fn capacity(&self) -> usize {
        self.items + self.growth_left
    }

    #[inline]
    pub(crate) fn allocator(&self) -> &A {
        &self.alloc
    }

    /// Group `group`'s control word.
    #[inline]
    fn word(&self, group: usize) -> Word {
        debug_assert!(group < self.groups);
        // SAFETY: `group < groups`, and the table's allocation holds `groups` words, written
        // when it was made.
        Ctrl::<SLOTS>(unsafe { *self.ctrl.as_ptr().add(group) })
    }

    #[inline]
    fn set_word(&mut self, group: usize, word: Word) {
        debug_assert!(group < self.groups);
        // SAFETY: as `word`, with `&mut self`.
        unsafe { *self.ctrl.as_ptr().add(group) = word.0 };
    }

    /// Slot `index`'s address.
    #[inline]
    fn slot_ptr(&self, index: usize) -> *mut T {
        debug_assert!(index < self.groups * SLOTS);
        // SAFETY: `index` is below the allocation's `groups * SLOTS` slots.
        unsafe { self.slots.as_ptr().add(index) }
    }

    /// The value in slot `index`, which must be live (a [`RawTable::find`] or iteration result
    /// since the last change).
    #[inline]
    pub(crate) fn get(&self, index: usize) -> &T {
        debug_assert!(self.word(index / SLOTS).live().any());
        // SAFETY: the caller's index is a live slot, whose value is initialized.
        unsafe { &*self.slot_ptr(index) }
    }

    /// [`RawTable::get`] to change.
    #[inline]
    pub(crate) fn get_mut(&mut self, index: usize) -> &mut T {
        // SAFETY: as `get`, with `&mut self`.
        unsafe { &mut *self.slot_ptr(index) }
    }

    /// The live slot of `hash` whose value `eq` accepts.
    #[inline]
    pub(crate) fn find(&self, hash: u64, mut eq: impl FnMut(&T) -> bool) -> Option<usize> {
        if self.groups == 0 {
            return None;
        }
        find(hash, live_tag(hash), self.groups, |group| self.word(group), |at| {
            let index = at.index();
            eq(self.get(index)).then_some(index)
        })
    }

    /// Whether a group's slots are at most two lines, so a prefetch loads them all before its
    /// word is read; wider groups prefetch only the slot the word's first tag match names.
    const SLOTS_AHEAD: bool = size_of::<T>() * SLOTS <= 2 * LINE;

    /// Starts loading what a [`RawTable::find`] of `hash` reads first: the home group's word
    /// and, for a group of at most two lines, its slots. A hint: it reads nothing.
    #[inline]
    pub(crate) fn prefetch(&self, hash: u64) {
        if self.groups == 0 {
            return;
        }
        let group = home(hash, self.groups);
        blaze_swiss::prefetch(self.ctrl.as_ptr().wrapping_add(group).cast_const());
        let bytes = size_of::<T>() * SLOTS;
        if Self::SLOTS_AHEAD && bytes > 0 {
            let first = self.slots.as_ptr().wrapping_add(group * SLOTS).cast::<u8>().cast_const();
            let mut offset = 0;
            while offset < bytes {
                blaze_swiss::prefetch(first.wrapping_add(offset));
                offset += LINE;
            }
            // The group need not start a line: its last byte may be one line further.
            blaze_swiss::prefetch(first.wrapping_add(bytes - 1));
        }
    }

    /// [`RawTable::prefetch`] of every hash, then, for groups too wide to load whole, the
    /// slot each home word's first tag match names: what a batch of finds runs first, so its
    /// misses overlap rather than queue.
    #[inline]
    pub(crate) fn prefetch_all(&self, hashes: &[u64]) {
        for &hash in hashes {
            self.prefetch(hash);
        }
        if Self::SLOTS_AHEAD || self.groups == 0 {
            return;
        }
        for &hash in hashes {
            let group = home(hash, self.groups);
            if let Some(slot) = self.word(group).matching(live_tag(hash)).first() {
                blaze_swiss::prefetch(
                    self.slots.as_ptr().wrapping_add(group * SLOTS + slot).cast_const(),
                );
            }
        }
    }

    /// How many more values go in before the table is made again.
    #[inline]
    pub(crate) fn growth_left(&self) -> usize {
        self.growth_left
    }

    /// Puts `value` of `hash` in a new slot, which it returns; `hasher` hashes a held value if
    /// the table is made again first. The caller has checked no equal value is held.
    #[inline]
    pub(crate) fn insert_unique(
        &mut self,
        hash: u64,
        value: T,
        hasher: impl Fn(&T) -> u64,
    ) -> usize {
        if self.growth_left == 0 {
            self.reserve(1, hasher);
        }
        self.insert_no_grow(hash, value)
    }

    /// Puts `value` in the first empty slot from its home: there is room (`growth_left > 0`).
    #[inline]
    fn insert_no_grow(&mut self, hash: u64, value: T) -> usize {
        debug_assert!(self.growth_left > 0);
        let at = vacant(hash, self.groups, |group| self.word(group));
        self.insert_at(at, hash, value)
    }

    /// The live slot of `hash` whose value `eq` accepts, or else the empty slot an insert of
    /// it takes ([`RawTable::insert_at`]), from one probe. The table has room for one more
    /// (`growth_left > 0`), so the vacancy needs no growth first.
    #[inline]
    pub(crate) fn find_or_vacant(
        &self,
        hash: u64,
        mut eq: impl FnMut(&T) -> bool,
    ) -> Result<usize, At<SLOTS>> {
        debug_assert!(self.growth_left > 0);
        find_or_vacant(hash, live_tag(hash), self.groups, |group| self.word(group), |at| {
            let index = at.index();
            eq(self.get(index)).then_some(index)
        })
    }

    /// Puts `value` of `hash` in the empty slot `at`, which a [`RawTable::find_or_vacant`] of
    /// `hash` returned with no change to the table since: its index.
    #[inline]
    pub(crate) fn insert_at(&mut self, at: At<SLOTS>, hash: u64, value: T) -> usize {
        debug_assert!(self.growth_left > 0);
        // Checked, not assumed: a vacancy of another table or from before a change would
        // write out of bounds or over a value. One compare on a word in cache.
        assert!(
            at.group < self.groups && at.slot < SLOTS && self.word(at.group).tag_at(at.slot) == EMPTY,
            "blaze_hash: an insert at a slot that is not this table's vacancy"
        );
        let index = at.index();
        // SAFETY: slot `index` is in the allocation (`group < groups`, `slot < SLOTS`) and its
        // tag is EMPTY, so it holds no value to overwrite.
        unsafe { self.slot_ptr(index).write(value) };
        self.set_word(at.group, at.tagged(live_tag(hash)));
        self.items += 1;
        self.growth_left -= 1;
        index
    }

    /// Takes the value out of live slot `index`; see the module's documentation for the tag
    /// it leaves.
    pub(crate) fn remove(&mut self, index: usize) -> T {
        let (group, slot) = (index / SLOTS, index % SLOTS);
        let word = self.word(group);
        debug_assert!(word.live().any() && word.tag_at(slot) != DEAD && word.tag_at(slot) != EMPTY);
        let left = if word.empty().any() {
            self.growth_left += 1;
            EMPTY
        } else {
            DEAD
        };
        self.set_word(group, word.with(slot, left));
        self.items -= 1;
        // SAFETY: the slot was live, and its tag no longer is, so the value is read once.
        unsafe { self.slot_ptr(index).read() }
    }

    /// Room for `additional` more values without making the table again.
    pub(crate) fn reserve(&mut self, additional: usize, hasher: impl Fn(&T) -> u64) {
        if let Err(failure) = self.try_reserve(additional, hasher) {
            fail(failure);
        }
    }

    pub(crate) fn try_reserve(
        &mut self,
        additional: usize,
        hasher: impl Fn(&T) -> u64,
    ) -> Result<(), AllocFailure> {
        if additional <= self.growth_left {
            return Ok(());
        }
        let overflow =
            AllocFailure::CapacityOverflow { what: WHAT, len: self.items, additional };
        let wanted = self.items.checked_add(additional).ok_or(overflow)?;
        let full = limit(self.groups);
        // Mostly dead slots: the same size again reclaims them. Else twice the size at least,
        // so a run of inserts makes the table again a logarithmic number of times.
        let capacity = if wanted <= full / 2 { full } else { wanted.max(full + 1) };
        self.resize(capacity, hasher)
    }

    /// Shrinks to the fewest groups that hold `min` and every value held.
    pub(crate) fn shrink_to(&mut self, min: usize, hasher: impl Fn(&T) -> u64) {
        let groups = match groups_for(min.max(self.items)) {
            Ok(groups) => groups,
            Err(failure) => fail(failure),
        };
        if groups < self.groups
            && let Err(failure) = self.resize(min.max(self.items), hasher)
        {
            fail(failure);
        }
    }

    /// Makes the table again with room for `capacity` values, moving every value into the new
    /// allocation by its hash.
    fn resize(&mut self, capacity: usize, hasher: impl Fn(&T) -> u64) -> Result<(), AllocFailure> {
        debug_assert!(capacity >= self.items);
        let groups = groups_for(capacity)?;
        if groups == 0 {
            self.free();
            self.ctrl = NonNull::dangling();
            self.slots = NonNull::dangling();
            self.groups = 0;
            self.growth_left = 0;
            return Ok(());
        }
        let (ctrl, slots) = allocate::<T, A>(&self.alloc, groups)?;
        // The new table owns no value until the move is done: if `hasher` panics part way,
        // the guard frees only the new allocation and the old table still owns every value.
        let mut grown = Moving { table: &mut *self, ctrl, groups, items: 0 };
        let mut live = grown.table.live();
        while let Some(index) = live.next(&*grown.table) {
            let value = grown.table.slot_ptr(index);
            // SAFETY: `index` is live, so its value is initialized and readable by reference.
            let hash = hasher(unsafe { &*value });
            let at = vacant(hash, groups, |group| {
                // SAFETY: `group < groups`, the new allocation's words, all written.
                Ctrl::<SLOTS>(unsafe { *ctrl.as_ptr().add(group) })
            });
            // SAFETY: `at` is an empty slot of the new allocation; the value is copied, and
            // the old slot is never read or dropped again once the move completes.
            unsafe { ptr::copy_nonoverlapping(value, slots.as_ptr().add(at.index()), 1) };
            // SAFETY: as above, `at.group < groups`.
            unsafe { *ctrl.as_ptr().add(at.group) = at.tagged(live_tag(hash)).0 };
            grown.items += 1;
        }
        let items = grown.finish();
        debug_assert_eq!(items, self.items);
        self.free();
        self.ctrl = ctrl;
        self.slots = slots;
        self.groups = groups;
        self.growth_left = limit(groups) - items;
        Ok(())
    }

    /// Frees the allocation; the values must already be dropped or moved.
    fn free(&mut self) {
        if self.groups != 0 {
            let (layout, _) = layout::<T>(self.groups).expect("the layout this table was made with");
            // SAFETY: `ctrl` is the start of the allocation made in `alloc` with this layout.
            unsafe { self.alloc.deallocate(self.ctrl.cast(), layout) };
        }
    }

    /// Drops every value and marks every slot empty; the allocation is kept.
    pub(crate) fn clear(&mut self) {
        let mut live = self.live();
        // Counted first: a value whose drop panics leaks the rest, and none is dropped twice.
        self.items = 0;
        while let Some(index) = live.next(self) {
            // SAFETY: `index` was live before this clear, and is dropped once.
            unsafe { self.slot_ptr(index).drop_in_place() };
        }
        self.clear_no_drop();
    }

    /// Marks every slot empty, the values already moved out.
    fn clear_no_drop(&mut self) {
        for group in 0..self.groups {
            self.set_word(group, Ctrl::<SLOTS>(0));
        }
        self.items = 0;
        self.growth_left = limit(self.groups);
    }

    /// The live slots now, for [`Live::next`] to walk.
    #[inline]
    pub(crate) fn live(&self) -> Live {
        Live {
            group: 0,
            slots: if self.groups == 0 { Ctrl::<SLOTS>(0).live() } else { self.word(0).live() },
            left: self.items,
        }
    }

    /// A walk of the values by reference.
    #[inline]
    pub(crate) fn iter(&self) -> Iter<'_, T> {
        let (ctrl, groups, slots) = (self.ctrl, self.groups, self.slots);
        Iter { live: self.live(), ctrl, groups, slots, marker: PhantomData }
    }

    /// A walk of the values to change.
    #[inline]
    pub(crate) fn iter_mut(&mut self) -> IterMut<'_, T> {
        let (ctrl, groups, slots) = (self.ctrl, self.groups, self.slots);
        IterMut { live: self.live(), ctrl, groups, slots, marker: PhantomData }
    }

    /// Keeps the values `keep` says to.
    pub(crate) fn retain(&mut self, mut keep: impl FnMut(&mut T) -> bool) {
        let mut live = self.live();
        while let Some(index) = live.next(self) {
            if !keep(self.get_mut(index)) {
                drop(self.remove(index));
            }
        }
    }

    /// Every value taken out, the allocation kept. The drain holds the allocation while it
    /// walks and the table looks empty meanwhile, so a drain that is leaked (`mem::forget`)
    /// leaks the allocation and the values not yet taken, and the table stays a valid empty
    /// one: it never sees a slot whose value was moved out.
    #[inline]
    pub(crate) fn drain(&mut self) -> Drain<'_, T, A> {
        let live = self.live();
        let held = Held { ctrl: self.ctrl, slots: self.slots, groups: self.groups };
        self.ctrl = NonNull::dangling();
        self.slots = NonNull::dangling();
        self.groups = 0;
        self.items = 0;
        self.growth_left = 0;
        Drain { live, held, table: self }
    }
}

impl<T: Clone, A: Allocator + Clone> Clone for RawTable<T, A> {
    /// The same values in the same slots, so the copy iterates in the same order.
    fn clone(&self) -> Self {
        let mut copy = RawTable::new_in(self.alloc.clone());
        if self.groups == 0 {
            return copy;
        }
        let (ctrl, slots) = match allocate::<T, A>(&copy.alloc, self.groups) {
            Ok(it) => it,
            Err(failure) => fail(failure),
        };
        copy.ctrl = ctrl;
        copy.slots = slots;
        copy.groups = self.groups;
        // Each slot is tagged once its value is written, so a clone that panics leaves a copy
        // that drops exactly the values cloned.
        let mut live = self.live();
        while let Some(index) = live.next(self) {
            // SAFETY: `index` is a slot of the copy's allocation, which has the same groups.
            unsafe { copy.slot_ptr(index).write(self.get(index).clone()) };
            let group = index / SLOTS;
            let tag = self.word(group).tag_at(index % SLOTS);
            let word = copy.word(group).with(index % SLOTS, tag);
            copy.set_word(group, word);
            copy.items += 1;
        }
        // Dead slots are copied as dead: the copy's probes stop where the original's do.
        for group in 0..self.groups {
            let source = self.word(group);
            let mut word = copy.word(group);
            for slot in source.full() {
                if source.tag_at(slot) == DEAD {
                    word = word.with(slot, DEAD);
                }
            }
            copy.set_word(group, word);
        }
        copy.growth_left = self.growth_left;
        copy
    }
}

// SAFETY (`may_dangle`, as hashbrown's and std's tables): the drop only drops the `T`s it owns
// (`marker: PhantomData<T>` tells dropck so) and frees its memory; it reads no borrow a `T`
// holds, so a table of references may outlive what they point to by its drop, as a caller's
// set of `&str` borrowed from the map it then returns (blaze-resolve's `summary.rs`).
unsafe impl<#[may_dangle] T, A: Allocator> Drop for RawTable<T, A> {
    fn drop(&mut self) {
        if mem::needs_drop::<T>() && self.items != 0 {
            let mut live = self.live();
            self.items = 0;
            while let Some(index) = live.next(self) {
                // SAFETY: `index` was live, and is dropped once.
                unsafe { self.slot_ptr(index).drop_in_place() };
            }
        }
        self.free();
    }
}

/// A new allocation being filled from `table` by [`RawTable::resize`]: dropped before
/// [`Moving::finish`], it frees the allocation and leaves every value to `table`.
struct Moving<'a, T, A: Allocator> {
    table: &'a mut RawTable<T, A>,
    ctrl: NonNull<u64>,
    groups: usize,
    items: usize,
}

impl<T, A: Allocator> Moving<'_, T, A> {
    /// The move is done: the new allocation is the table's to take, with this many values.
    fn finish(self) -> usize {
        mem::ManuallyDrop::new(self).items
    }
}

impl<T, A: Allocator> Drop for Moving<'_, T, A> {
    fn drop(&mut self) {
        let (layout, _) = layout::<T>(self.groups).expect("the layout just allocated");
        // SAFETY: the allocation was made in the table's allocator with this layout, and the
        // values copied into it are still owned by the old table.
        unsafe { self.table.alloc.deallocate(self.ctrl.cast(), layout) };
    }
}

/// `groups` groups' memory in `alloc`, every control word written empty.
fn allocate<T, A: Allocator>(
    alloc: &A,
    groups: usize,
) -> Result<(NonNull<u64>, NonNull<T>), AllocFailure> {
    let (layout, offset) = layout::<T>(groups)?;
    let block = alloc.allocate(layout).map_err(|_| AllocFailure::Refused { what: WHAT, layout })?;
    let ctrl: NonNull<u64> = block.cast();
    // SAFETY: the block holds `groups` words at its start, aligned for `u64` by the layout.
    unsafe { ctrl.as_ptr().write_bytes(0, groups) };
    // SAFETY: `offset` is within the block, where the layout put the slots.
    let slots = unsafe { block.cast::<u8>().add(offset) }.cast::<T>();
    Ok((ctrl, slots))
}

/// A walk of a table's live slots, by index, against the table it was made from: it holds
/// no borrow, so the walker may remove the slot it is at (`retain`). Removing a
/// slot it has not reached is not allowed.
#[derive(Clone)]
pub(crate) struct Live {
    group: usize,
    slots: Slots,
    /// Live slots not yet walked.
    left: usize,
}

impl Live {
    /// The next live slot of `table`.
    #[inline]
    pub(crate) fn next<T, A: Allocator>(&mut self, table: &RawTable<T, A>) -> Option<usize> {
        // SAFETY of the loop: while `left > 0` a live slot is ahead, so `group` stays below
        // `table.groups`.
        self.next_in(table.ctrl, table.groups)
    }

    #[inline]
    fn next_in(&mut self, ctrl: NonNull<u64>, groups: usize) -> Option<usize> {
        loop {
            if self.left == 0 {
                return None;
            }
            if let Some(slot) = self.slots.next() {
                self.left -= 1;
                return Some(self.group * SLOTS + slot);
            }
            self.group += 1;
            assert!(self.group < groups, "a table walk past its last group");
            // SAFETY: `group < groups`, a word of the table's allocation.
            self.slots = Ctrl::<SLOTS>(unsafe { *ctrl.as_ptr().add(self.group) }).live();
        }
    }

}

/// The values of a table by reference, in slot order.
pub(crate) struct Iter<'a, T> {
    live: Live,
    ctrl: NonNull<u64>,
    groups: usize,
    slots: NonNull<T>,
    marker: PhantomData<&'a T>,
}

impl<T> Clone for Iter<'_, T> {
    fn clone(&self) -> Self {
        Iter {
            live: self.live.clone(),
            ctrl: self.ctrl,
            groups: self.groups,
            slots: self.slots,
            marker: PhantomData,
        }
    }
}

// SAFETY: an `Iter` is a `&RawTable` in effect.
unsafe impl<T: Sync> Send for Iter<'_, T> {}
// SAFETY: as above.
unsafe impl<T: Sync> Sync for Iter<'_, T> {}

impl<'a, T> Iterator for Iter<'a, T> {
    type Item = &'a T;

    #[inline]
    fn next(&mut self) -> Option<&'a T> {
        let index = self.live.next_in(self.ctrl, self.groups)?;
        // SAFETY: `index` is a live slot of the table borrowed for `'a`.
        Some(unsafe { &*self.slots.as_ptr().add(index) })
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.live.left, Some(self.live.left))
    }
}

impl<T> ExactSizeIterator for Iter<'_, T> {}
impl<T> core::iter::FusedIterator for Iter<'_, T> {}

/// The values of a table to change, in slot order.
pub(crate) struct IterMut<'a, T> {
    live: Live,
    ctrl: NonNull<u64>,
    groups: usize,
    slots: NonNull<T>,
    marker: PhantomData<&'a mut T>,
}

// SAFETY: an `IterMut` is a `&mut RawTable` in effect.
unsafe impl<T: Send> Send for IterMut<'_, T> {}
// SAFETY: as above.
unsafe impl<T: Sync> Sync for IterMut<'_, T> {}

impl<T> IterMut<'_, T> {
    /// The values not yet walked, by reference: what `Debug` lists.
    pub(crate) fn rest(&self) -> Iter<'_, T> {
        let (ctrl, groups, slots) = (self.ctrl, self.groups, self.slots);
        Iter { live: self.live.clone(), ctrl, groups, slots, marker: PhantomData }
    }
}

impl<'a, T> Iterator for IterMut<'a, T> {
    type Item = &'a mut T;

    #[inline]
    fn next(&mut self) -> Option<&'a mut T> {
        let index = self.live.next_in(self.ctrl, self.groups)?;
        // SAFETY: `index` is a live slot of the table borrowed mutably for `'a`, and each slot
        // is yielded once.
        Some(unsafe { &mut *self.slots.as_ptr().add(index) })
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.live.left, Some(self.live.left))
    }
}

impl<T> ExactSizeIterator for IterMut<'_, T> {}
impl<T> core::iter::FusedIterator for IterMut<'_, T> {}

/// A table's values taken by value; the allocation is freed when it is dropped.
pub(crate) struct IntoIter<T, A: Allocator> {
    live: Live,
    table: RawTable<T, A>,
}

impl<T, A: Allocator> IntoIterator for RawTable<T, A> {
    type Item = T;
    type IntoIter = IntoIter<T, A>;

    #[inline]
    fn into_iter(mut self) -> IntoIter<T, A> {
        let live = self.live();
        // The walk owns the values now: the table's drop frees only the allocation.
        self.items = 0;
        IntoIter { live, table: self }
    }
}

impl<T, A: Allocator> IntoIter<T, A> {
    /// The values not yet taken, by reference: what `Debug` lists.
    pub(crate) fn rest(&self) -> Iter<'_, T> {
        let (ctrl, groups, slots) = (self.table.ctrl, self.table.groups, self.table.slots);
        Iter { live: self.live.clone(), ctrl, groups, slots, marker: PhantomData }
    }
}

impl<T, A: Allocator> Iterator for IntoIter<T, A> {
    type Item = T;

    #[inline]
    fn next(&mut self) -> Option<T> {
        let index = self.live.next(&self.table)?;
        // SAFETY: `index` was live and is read once; the table counts none of its values.
        Some(unsafe { self.table.slot_ptr(index).read() })
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.live.left, Some(self.live.left))
    }
}

impl<T, A: Allocator> ExactSizeIterator for IntoIter<T, A> {}
impl<T, A: Allocator> core::iter::FusedIterator for IntoIter<T, A> {}

// SAFETY (`may_dangle`): as the table's: it drops the `T`s it owns, through the table it
// holds, and reads nothing they borrow.
unsafe impl<#[may_dangle] T, A: Allocator> Drop for IntoIter<T, A> {
    fn drop(&mut self) {
        while let Some(index) = self.live.next(&self.table) {
            // SAFETY: `index` was live and not yet taken; dropped once.
            unsafe { self.table.slot_ptr(index).drop_in_place() };
        }
    }
}

/// The allocation a [`Drain`] took from its table, given back by its drop.
struct Held<T> {
    ctrl: NonNull<u64>,
    slots: NonNull<T>,
    groups: usize,
}

/// A table's values taken by value, leaving it empty with its allocation.
pub(crate) struct Drain<'a, T, A: Allocator> {
    live: Live,
    held: Held<T>,
    table: &'a mut RawTable<T, A>,
}

// SAFETY: a `Drain` owns the values it has not yet taken and borrows the table mutably, as
// a `&mut RawTable` would.
unsafe impl<T: Send, A: Allocator + Send> Send for Drain<'_, T, A> {}
// SAFETY: as above; `&Drain` reaches no value.
unsafe impl<T: Sync, A: Allocator + Sync> Sync for Drain<'_, T, A> {}

impl<T, A: Allocator> Drain<'_, T, A> {
    /// The values not yet taken, by reference: what `Debug` lists.
    pub(crate) fn rest(&self) -> Iter<'_, T> {
        let Held { ctrl, slots, groups } = self.held;
        Iter { live: self.live.clone(), ctrl, groups, slots, marker: PhantomData }
    }
}

impl<T, A: Allocator> Iterator for Drain<'_, T, A> {
    type Item = T;

    #[inline]
    fn next(&mut self) -> Option<T> {
        let index = self.live.next_in(self.held.ctrl, self.held.groups)?;
        // SAFETY: `index` was live in the held allocation and is read once; the drop marks
        // every slot empty before the table sees the allocation again.
        Some(unsafe { self.held.slots.as_ptr().add(index).read() })
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.live.left, Some(self.live.left))
    }
}

impl<T, A: Allocator> ExactSizeIterator for Drain<'_, T, A> {}
impl<T, A: Allocator> core::iter::FusedIterator for Drain<'_, T, A> {}

impl<T, A: Allocator> Drop for Drain<'_, T, A> {
    fn drop(&mut self) {
        /// Gives the allocation back to the table, every slot empty, however the drops below
        /// end: a value whose drop panics leaks the rest, and none is dropped twice.
        struct GiveBack<'b, T, A: Allocator> {
            table: &'b mut RawTable<T, A>,
            ctrl: NonNull<u64>,
            slots: NonNull<T>,
            groups: usize,
        }
        impl<T, A: Allocator> Drop for GiveBack<'_, T, A> {
            fn drop(&mut self) {
                self.table.ctrl = self.ctrl;
                self.table.slots = self.slots;
                self.table.groups = self.groups;
                self.table.clear_no_drop();
            }
        }
        let Held { ctrl, slots, groups } = self.held;
        let mut live = self.live.clone();
        let _give_back = GiveBack { table: &mut *self.table, ctrl, slots, groups };
        while let Some(index) = live.next_in(ctrl, groups) {
            // SAFETY: `index` was live in the held allocation and not yet taken; dropped once.
            unsafe { slots.as_ptr().add(index).drop_in_place() };
        }
    }
}
