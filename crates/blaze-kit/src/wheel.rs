//! [`TimerWheel<T, A>`]: a hierarchical timing wheel over a caller's clock (Varghese and Lauck,
//! 1987, in the layout tokio's timer uses): deadlines for the scheduler's waits and the
//! watchdog, where most timers are cancelled before they fire. In place of a
//! `BinaryHeap<Reverse<(deadline, id)>>` with lazy deletion.
//!
//! Time is a `u64` count of ticks the caller defines; the wheel never reads a clock (rule 75):
//! [`TimerWheel::poll`] is told what time it is. Eleven levels of 64 slots cover every `u64`:
//! level `L`'s slot `s` holds the timers whose deadline shares `elapsed`'s bits above digit `L`
//! (a digit is 6 bits) and has digit `L` equal to `s`. A timer whose deadline is at or before
//! `elapsed` waits in a due list. Each slot is a doubly linked list through a slab of entries,
//! and each level keeps a 64-bit word of which slots are occupied.
//!
//! A poll takes from the due list first; else it finds the lowest level with an occupied slot
//! at or after `elapsed`'s digit there (its start is the earliest any timer can fire), and when
//! that start is at or before `now` moves `elapsed` to it and places that slot's timers again,
//! one level down or into the due list. A timer is moved at most once per level: 11 times in
//! all, and none for a deadline under 64 ticks away.
//!
//! # Floor
//!
//! An entry is the value plus 24 bytes (deadline, two links, a generation and its list), in a
//! slab that reuses freed entries; the wheel itself is 705 list heads and 11 words, about 2.9
//! KB. Insert and cancel are O(1): one entry and at most two neighbours' lines, one head and
//! one occupancy word. A poll that fires is O(1) amortized plus the cascades, each O(1) per
//! timer moved; a poll that fires nothing reads at most 11 occupancy words.
//!
//! # Order
//!
//! Timers placed in the wheel fire in deadline order (a level-0 slot is one tick, so ties fire
//! in the order they reached it). Timers inserted with a deadline at or before `elapsed` fire
//! first, in insertion order.
//!
//! # Measured (`examples/heaps.rs`, release, one Mac run, 2026-11-15)
//!
//! ns per timer inserted and fired, at the example's three sizes: 15.2, 18.6, 62.6 against a
//! `BinaryHeap` with a set of cancelled timers beside it: 45.8, 106.4, 126.6.

use core::{alloc::Allocator, any::type_name};

use blaze_int::Int;
use blaze_own::{AllocFailure, Refused, Vec};

use crate::{
    error::{fail, overflow},
    plant::{self, Plant},
    table::MOST,
};

/// Levels: 11 digits of 6 bits cover 64 bits.
const LEVELS: usize = 11;

/// Slots per level.
const SLOTS: usize = 64;

/// The list of timers already due.
const DUE: usize = LEVELS * SLOTS;

/// No entry.
const NONE: u32 = u32::MAX;

/// A timer in the wheel, to cancel it: an entry and the generation it had when made, so a
/// handle to a timer that fired or was cancelled names nothing even after its entry is reused
/// (until that one entry is reused 2^32 times, when the generation wraps).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Timer {
    index: u32,
    generation: u32,
}

struct Entry<T> {
    deadline: u64,
    prev: u32,
    /// The next entry in its list, or in the free list while free.
    next: u32,
    generation: u32,
    /// The list it is on (a slot, or [`DUE`]).
    list: u32,
    /// `None` while free.
    value: Option<T>,
}

/// Timers holding `T`, in `A` (see the module).
pub struct TimerWheel<T, A: Allocator> {
    entries: Vec<Entry<T>, A>,
    free: u32,
    heads: [u32; DUE + 1],
    occupied: [u64; LEVELS],
    elapsed: u64,
    len: usize,
}

impl<T, A: Allocator> TimerWheel<T, A> {
    /// An empty wheel at time `now`.
    pub fn new_in(now: u64, alloc: A) -> TimerWheel<T, A> {
        TimerWheel {
            entries: Vec::new_in(alloc),
            free: NONE,
            heads: [NONE; DUE + 1],
            occupied: [0; LEVELS],
            elapsed: now,
            len: 0,
        }
    }

    /// Timers waiting.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The time the wheel has reached: no poll is told an earlier one.
    pub fn elapsed(&self) -> u64 {
        self.elapsed
    }

    /// Room for `additional` more timers without allocating.
    pub fn try_reserve(&mut self, additional: usize) -> Result<(), AllocFailure> {
        self.entries.try_reserve(additional)
    }

    /// The list a timer of `deadline` goes on now.
    #[inline]
    fn place(&self, deadline: u64) -> usize {
        if deadline <= self.elapsed {
            return DUE;
        }
        let masked = (self.elapsed ^ deadline) | 63;
        let level = (63 - masked.leading_zeros()) / 6;
        let slot = (deadline >> (6 * level)) & 63;
        blaze_int::idx(level) * SLOTS + slot.truncate_to::<usize>()
    }

    /// Puts entry `at` at the head of `list`.
    #[inline]
    fn link(&mut self, at: u32, list: usize) {
        let head = self.heads[list];
        {
            let entry = &mut self.entries[blaze_int::idx(at)];
            entry.prev = NONE;
            entry.next = head;
            entry.list = list.truncate_to::<u32>();
        }
        if head != NONE {
            self.entries[blaze_int::idx(head)].prev = at;
        }
        self.heads[list] = at;
        if list < DUE {
            self.occupied[list / SLOTS] |= 1 << (list % SLOTS);
        }
    }

    /// Takes entry `at` off its list.
    #[inline]
    fn unlink(&mut self, at: u32) {
        let (prev, next, list) = {
            let entry = &self.entries[blaze_int::idx(at)];
            (entry.prev, entry.next, blaze_int::idx(entry.list))
        };
        if prev == NONE {
            self.heads[list] = next;
            if next == NONE && list < DUE {
                self.occupied[list / SLOTS] &= !(1 << (list % SLOTS));
            }
        } else {
            self.entries[blaze_int::idx(prev)].next = next;
        }
        if next != NONE {
            self.entries[blaze_int::idx(next)].prev = prev;
        }
    }

    /// Adds a timer firing at `deadline` (at once when it is at or before
    /// [`TimerWheel::elapsed`]); refused with the value back and nothing changed.
    pub fn try_insert(&mut self, deadline: u64, value: T) -> Result<Timer, Refused<T>> {
        let at = if self.free != NONE {
            let at = self.free;
            let entry = &mut self.entries[blaze_int::idx(at)];
            self.free = entry.next;
            entry.deadline = deadline;
            entry.value = Some(value);
            at
        } else {
            let len = self.entries.len();
            let raw = match len.narrow::<u32>() {
                Ok(raw) if raw < MOST => raw,
                _ => return Err(Refused { value, why: overflow(type_name::<Self>(), len, 1) }),
            };
            let entry = Entry { deadline, prev: NONE, next: NONE, generation: 0, list: 0, value: Some(value) };
            if let Err(refused) = self.entries.try_push(entry) {
                let value = match refused.value.value {
                    Some(value) => value,
                    None => unreachable!("the entry was made with its value"),
                };
                return Err(Refused { value, why: refused.why });
            }
            raw
        };
        let list = self.place(deadline);
        self.link(at, list);
        self.len += 1;
        Ok(Timer { index: at, generation: self.entries[blaze_int::idx(at)].generation })
    }

    /// [`TimerWheel::try_insert`], panicking with the refusal.
    pub fn insert(&mut self, deadline: u64, value: T) -> Timer {
        match self.try_insert(deadline, value) {
            Ok(timer) => timer,
            Err(refused) => fail(refused.why),
        }
    }

    /// Frees entry `at` (off its list already), giving its value back.
    fn release(&mut self, at: u32) -> T {
        let free = self.free;
        let entry = &mut self.entries[blaze_int::idx(at)];
        entry.generation = entry.generation.wrapping_add(1);
        entry.next = free;
        self.free = at;
        self.len -= 1;
        match entry.value.take() {
            Some(value) => value,
            None => unreachable!("a linked entry holds its value"),
        }
    }

    /// Whether `timer` still waits.
    pub fn contains(&self, timer: Timer) -> bool {
        self.entries
            .get(blaze_int::idx(timer.index))
            .is_some_and(|entry| entry.generation == timer.generation && entry.value.is_some())
    }

    /// The deadline of `timer`, while it waits.
    pub fn deadline(&self, timer: Timer) -> Option<u64> {
        self.contains(timer).then(|| self.entries[blaze_int::idx(timer.index)].deadline)
    }

    /// Takes `timer` out before it fires; its value, or `None` when it fired or was cancelled.
    pub fn cancel(&mut self, timer: Timer) -> Option<T> {
        if !self.contains(timer) {
            return None;
        }
        self.unlink(timer.index);
        Some(self.release(timer.index))
    }

    /// The lowest level's earliest occupied slot at or after `elapsed`, and its start.
    fn next_slot(&self) -> Option<(usize, u64)> {
        for level in 0..LEVELS {
            let shift = 6 * level;
            let digit = (self.elapsed >> shift) & 63;
            let bits = self.occupied[level] & (!0u64 << digit);
            if bits == 0 {
                continue;
            }
            let slot = blaze_int::idx(bits.trailing_zeros());
            let above = shift + 6;
            let base = if above >= 64 { 0 } else { self.elapsed & !((1u64 << above) - 1) };
            let start = base + (slot.widen_to::<u64>() << shift);
            return Some((level * SLOTS + slot, start));
        }
        None
    }

    /// A lower bound on the earliest deadline waiting: the time to poll next. `None` when no
    /// timer waits.
    pub fn next_deadline(&self) -> Option<u64> {
        if self.heads[DUE] != NONE {
            return Some(self.elapsed);
        }
        self.next_slot().map(|(_, start)| start.max(self.elapsed))
    }

    /// A timer due at `now` and its value, or `None` when none is; the wheel moves to `now`.
    /// Call until `None` to fire all. A `now` before [`TimerWheel::elapsed`] is a bug and
    /// panics naming both.
    #[track_caller]
    pub fn poll(&mut self, now: u64) -> Option<(Timer, T)> {
        if now < self.elapsed {
            panic!("blaze_kit: `{}` polled at {now}, before {} it reached", type_name::<Self>(), self.elapsed);
        }
        loop {
            let due = self.heads[DUE];
            if due != NONE {
                self.unlink(due);
                let generation = self.entries[blaze_int::idx(due)].generation;
                let value = self.release(due);
                return Some((Timer { index: due, generation }, value));
            }
            let Some((list, start)) = self.next_slot() else {
                self.elapsed = now;
                return None;
            };
            if start > now {
                self.elapsed = now;
                return None;
            }
            self.elapsed = if plant::on(Plant::WheelCascadesLate) { now } else { start.max(self.elapsed) };
            // Place the slot's timers again from the new `elapsed`: lower, or due.
            let mut at = self.heads[list];
            self.heads[list] = NONE;
            self.occupied[list / SLOTS] &= !(1 << (list % SLOTS));
            while at != NONE {
                let (next, deadline) = {
                    let entry = &self.entries[blaze_int::idx(at)];
                    (entry.next, entry.deadline)
                };
                let to = self.place(deadline);
                self.link(at, to);
                at = next;
            }
        }
    }

    /// Drops every timer, the room kept; their handles name nothing after (the entries stay,
    /// free, with their generations raised).
    pub fn clear(&mut self) {
        self.heads = [NONE; DUE + 1];
        self.occupied = [0; LEVELS];
        self.free = NONE;
        self.len = 0;
        for (at, entry) in self.entries.iter_mut().enumerate().rev() {
            if entry.value.take().is_some() {
                entry.generation = entry.generation.wrapping_add(1);
            }
            entry.next = self.free;
            // Entries number below `MOST`.
            self.free = at.truncate_to::<u32>();
        }
    }
}
