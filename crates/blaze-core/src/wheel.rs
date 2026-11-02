//! A hierarchical timing wheel (Varghese and Lauck's, laid out as tokio's timer is): the timers
//! of a driver, keyed by a deadline in `u64` ticks (nanoseconds since the driver began).
//!
//! Eleven levels of 64 slots cover every `u64`: a timer sits at the level of the highest 6-bit
//! group in which its deadline differs from the wheel's time, in the slot that group names.
//! Inserting is a shift and a link, cancelling an unlink through its [`Handle`], and advancing
//! walks the occupied slots in deadline order through one bitmap per level, moving a higher
//! slot's timers down once time reaches it (each timer moves at most ten times). Timers of one
//! deadline fire in the order they were inserted.
//!
//! The timers sit in a [`blaze_mem::Slab`] tagged [`MemTag::Io`] (docs/v2/plan/design/memory.md,
//! "Generational handles"): a slot's list links are the generational handles of its timers, and
//! a [`Handle`] is one of them, so a handle to a timer that fired or was cancelled is detected
//! as stale even once its slot holds another timer. The slab reserves room for
//! [`MAX_TIMERS`] up front and commits pages as the working set grows: a warm arm, cancel and
//! fire make no call to the global allocator.
//!
//! Cost: 32 bytes of links per timer beside its value (and the slab's 8), 11 KiB of slot heads,
//! and O(1) insert, cancel and step of an advance.

use std::time::Duration;

use blaze_mem::{MemError, Slab, Tag as MemTag};

#[cfg(test)]
mod differential_tests;
#[cfg(test)]
mod reference;

const BITS: u32 = 6;
const SLOTS: usize = 1 << BITS;
/// 11 groups of 6 bits cover 64.
const LEVELS: usize = 11;
/// A timer's `slot` before it is placed.
const UNPLACED: u32 = u32::MAX;
/// The bits of no timer: index `u32::MAX`, which no slab issues.
const NONE: u64 = u64::MAX;

/// The most timers one wheel holds at once: its slab's reservation, committed only as used.
pub const MAX_TIMERS: u32 = 1 << 22;

/// A tick count for `duration`: nanoseconds, the last tick for one past what `u64` holds
/// (some 584 years).
pub fn ticks(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

/// Names a timer in the wheel until it fires or is cancelled; a stale one names nothing, even
/// after its slot is reused.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Handle(u64);

/// A timer in the wheel: its deadline, its place and the links of its slot's list, in
/// insertion order.
pub struct Timer<T> {
    deadline: u64,
    prev: u64,
    next: u64,
    /// `level * SLOTS + slot`.
    slot: u32,
    value: T,
}

type Id<T> = blaze_mem::Handle<Timer<T>>;

#[derive(Clone, Copy)]
struct List {
    head: u64,
    tail: u64,
}

const EMPTY: List = List { head: NONE, tail: NONE };

pub struct Wheel<T> {
    /// Every timer due at or before this has fired.
    now: u64,
    /// Bit `slot` of level `level`: that slot's list is not empty.
    occupied: [u64; LEVELS],
    slots: Box<[List; LEVELS * SLOTS]>,
    timers: Slab<Timer<T>>,
    /// The most timers it holds: the slab's reservation rounds up to whole pages, so it
    /// would hold more.
    max: u32,
    /// The soonest deadline, while known; recomputed after the timer that had it leaves.
    soonest: Option<u64>,
}

impl<T> Default for Wheel<T> {
    fn default() -> Wheel<T> {
        Wheel::new()
    }
}

impl<T> Wheel<T> {
    /// A wheel of at most [`MAX_TIMERS`] timers; stops when the address space for them cannot
    /// be reserved.
    #[track_caller]
    pub fn new() -> Wheel<T> {
        match Wheel::try_new(MAX_TIMERS) {
            Ok(wheel) => wheel,
            Err(error) => panic!("{error}"),
        }
    }

    /// A wheel of at most `max` timers, reserving their room and committing none.
    pub fn try_new(max: u32) -> Result<Wheel<T>, MemError> {
        Ok(Wheel {
            now: 0,
            occupied: [0; LEVELS],
            slots: Box::new([EMPTY; LEVELS * SLOTS]), // alloc: once per wheel, at setup
            timers: Slab::new(max, MemTag::Io)?,
            max,
            soonest: None,
        })
    }

    pub fn len(&self) -> usize {
        self.timers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.timers.is_empty()
    }

    /// The timers made so far, set or free: what the slab holds, which a steady stream of
    /// timers does not grow.
    pub fn slots(&self) -> usize {
        self.timers.slots()
    }

    /// The wheel's time: what the last [`Wheel::advance`] went to.
    pub fn now(&self) -> u64 {
        self.now
    }

    /// `value` due at `deadline`; one already passed is due at the next advance. Stops when
    /// the wheel holds [`MAX_TIMERS`] (its `max`) already.
    #[track_caller]
    pub fn insert(&mut self, deadline: u64, value: T) -> Handle {
        match self.try_insert(deadline, value) {
            Ok(handle) => handle,
            Err(error) => panic!("{error}"),
        }
    }

    /// As [`Wheel::insert`], or the error naming the subsystem when the wheel is full.
    pub fn try_insert(&mut self, deadline: u64, value: T) -> Result<Handle, MemError> {
        let deadline = deadline.max(self.now);
        if self.timers.len() >= self.max as usize {
            return Err(MemError::TooLarge {
                tag: MemTag::Io,
                what: "a timer wheel's timer count",
                wanted: self.timers.len() + 1,
            });
        }
        let id = self.timers.try_insert(Timer {
            deadline,
            prev: NONE,
            next: NONE,
            slot: UNPLACED,
            value,
        })?;
        self.place(id);
        self.soonest = match (self.timers.len(), self.soonest) {
            (1, _) => Some(deadline),
            (_, Some(soonest)) => Some(soonest.min(deadline)),
            (_, None) => None,
        };
        Ok(Handle(id.to_bits()))
    }

    /// The timer's deadline, while it is in the wheel.
    pub fn deadline(&self, handle: Handle) -> Option<u64> {
        self.timers.get(Id::from_bits(handle.0)).map(|timer| timer.deadline)
    }

    pub fn get_mut(&mut self, handle: Handle) -> Option<&mut T> {
        self.timers.get_mut(Id::from_bits(handle.0)).map(|timer| &mut timer.value)
    }

    /// The timer taken out before it fires; `None` once it has fired or been cancelled.
    pub fn cancel(&mut self, handle: Handle) -> Option<T> {
        let id = Id::from_bits(handle.0);
        let deadline = self.timers.get(id)?.deadline;
        self.unlink(id);
        if self.soonest == Some(deadline) {
            self.soonest = None;
        }
        self.timers.remove(id).map(|timer| timer.value)
    }

    /// The soonest deadline of a timer in the wheel.
    pub fn next_deadline(&mut self) -> Option<u64> {
        if self.timers.is_empty() {
            return None;
        }
        if let Some(soonest) = self.soonest {
            return Some(soonest);
        }
        let (level, slot) = self.first().expect("a timer is in the wheel");
        let soonest = match level {
            // A slot of level 0 is one tick.
            0 => self.start(0, slot),
            _ => {
                let mut soonest = u64::MAX;
                let mut at = self.slots[level * SLOTS + slot].head;
                while at != NONE {
                    let timer = self.linked(at);
                    soonest = soonest.min(timer.deadline);
                    at = timer.next;
                }
                soonest
            }
        };
        self.soonest = Some(soonest);
        Some(soonest)
    }

    /// Time moved on to `to`: each timer due by then handed to `fire` with its deadline, in
    /// deadline order and, for one deadline, in the order they were inserted. A `to` behind the
    /// wheel's time fires nothing.
    pub fn advance(&mut self, to: u64, mut fire: impl FnMut(u64, T)) {
        while let Some((level, slot)) = self.first() {
            let start = self.start(level, slot);
            if start > to {
                break;
            }
            self.now = start;
            let list = std::mem::replace(&mut self.slots[level * SLOTS + slot], EMPTY);
            self.occupied[level] &= !(1 << slot);
            let mut at = list.head;
            while at != NONE {
                let next = self.linked(at).next;
                match level {
                    0 => {
                        let timer = self
                            .timers
                            .remove(Id::from_bits(at))
                            .expect("a linked timer is in the wheel");
                        if self.soonest.is_some_and(|it| it <= timer.deadline) {
                            self.soonest = None;
                        }
                        fire(timer.deadline, timer.value);
                    }
                    // Every level below is empty, so each goes to the tail of an empty list in
                    // the order it was inserted.
                    _ => self.place(Id::from_bits(at)),
                }
                at = next;
            }
        }
        self.now = self.now.max(to);
    }

    /// Every timer taken out, in deadline order, the wheel's time left as it is.
    pub fn drain(&mut self, mut each: impl FnMut(u64, T)) {
        let now = self.now;
        self.advance(u64::MAX, &mut each);
        self.now = now;
    }

    /// The timer a list link names: every link is to a timer in the wheel.
    fn linked(&self, bits: u64) -> &Timer<T> {
        self.timers.get(Id::from_bits(bits)).expect("a linked timer is in the wheel")
    }

    fn linked_mut(&mut self, bits: u64) -> &mut Timer<T> {
        self.timers.get_mut(Id::from_bits(bits)).expect("a linked timer is in the wheel")
    }

    /// The occupied slot of the lowest level: every timer of a lower level is due before any
    /// of a higher (they share the wheel's time above their level), and within a level a slot
    /// at or before the wheel's position holds nothing, so its lowest bit is the soonest.
    fn first(&self) -> Option<(usize, usize)> {
        let level = self.occupied.iter().position(|&it| it != 0)?;
        Some((level, self.occupied[level].trailing_zeros() as usize))
    }

    /// The first tick of `slot` of `level` in the wheel's current block of that level.
    fn start(&self, level: usize, slot: usize) -> u64 {
        let above = u64::MAX.checked_shl(BITS * (level as u32 + 1)).unwrap_or(0);
        (self.now & above) | ((slot as u64) << (BITS * level as u32))
    }

    /// Links `id` at the tail of the slot its deadline names from the wheel's time.
    fn place(&mut self, id: Id<T>) {
        let bits = id.to_bits();
        let deadline = self.linked(bits).deadline;
        let differ = (self.now ^ deadline) | (SLOTS as u64 - 1);
        let level = ((63 - differ.leading_zeros()) / BITS) as usize;
        let slot = ((deadline >> (BITS * level as u32)) as usize) & (SLOTS - 1);
        let at = level * SLOTS + slot;
        let tail = self.slots[at].tail;
        let timer = self.linked_mut(bits);
        timer.prev = tail;
        timer.next = NONE;
        timer.slot = at as u32;
        match tail {
            NONE => self.slots[at].head = bits,
            tail => self.linked_mut(tail).next = bits,
        }
        self.slots[at].tail = bits;
        self.occupied[level] |= 1 << slot;
    }

    fn unlink(&mut self, id: Id<T>) {
        let timer = self.linked(id.to_bits());
        let (prev, next, slot) = (timer.prev, timer.next, timer.slot as usize);
        match prev {
            NONE => self.slots[slot].head = next,
            prev => self.linked_mut(prev).next = next,
        }
        match next {
            NONE => self.slots[slot].tail = prev,
            next => self.linked_mut(next).prev = prev,
        }
        if self.slots[slot].head == NONE {
            self.occupied[slot / SLOTS] &= !(1 << (slot % SLOTS));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::Rng;

    /// The model: every timer with its deadline and insertion number, fired by sorting.
    #[derive(Default)]
    struct Model {
        now: u64,
        timers: Vec<(u64, u64, u32)>,
    }

    impl Model {
        fn advance(&mut self, to: u64) -> Vec<(u64, u32)> {
            self.now = self.now.max(to);
            let mut due: Vec<_> = self.timers.iter().copied().filter(|it| it.0 <= to).collect();
            self.timers.retain(|it| it.0 > to);
            due.sort_unstable();
            due.into_iter().map(|(deadline, _, value)| (deadline, value)).collect()
        }
    }

    fn deadline(rng: &mut Rng, now: u64) -> u64 {
        match rng.below(8) {
            0 => now,
            1 => now.saturating_sub(rng.below(100)),
            2 => u64::MAX - rng.below(3),
            3 => now.saturating_add(rng.below(4)),
            4 => now.saturating_add(rng.below(1 << 12)),
            5 => now.saturating_add(rng.below(1 << 30)),
            6 => now.saturating_add(rng.next_u64() >> rng.below(64)),
            _ => now.saturating_add(1 << (6 * rng.below(11))),
        }
    }

    /// Against the model and the `Vec`-backed wheel it replaced, on every seed: inserts
    /// (passed, now, near, far, at the end of time, on level boundaries, many on one deadline),
    /// cancels of live and stale handles, the soonest deadline, and advances small, large and
    /// backwards. All three fire the same timers in the same order.
    #[test]
    fn fires_as_sorting_by_deadline_then_insertion_does() {
        for seed in 0..400 {
            let mut rng = Rng::new(seed);
            let mut wheel = Wheel::new();
            let mut old = reference::Wheel::new();
            let mut model = Model::default();
            let mut handles: Vec<(Handle, reference::Handle, u32)> = Vec::new();
            let mut inserted = 0u64;
            for _ in 0..600 {
                match rng.below(10) {
                    0..=4 => {
                        let at = deadline(&mut rng, model.now);
                        let value = inserted as u32;
                        let handle = wheel.insert(at, value);
                        let old_handle = old.insert(at, value);
                        assert_eq!(wheel.deadline(handle), Some(at.max(model.now)), "seed {seed}");
                        assert_eq!(wheel.deadline(handle), old.deadline(old_handle), "seed {seed}");
                        model.timers.push((at.max(model.now), inserted, value));
                        handles.push((handle, old_handle, value));
                        inserted += 1;
                    }
                    5 | 6 if !handles.is_empty() => {
                        let (handle, old_handle, value) =
                            handles.swap_remove(rng.below(handles.len() as u64) as usize);
                        let live = model.timers.iter().position(|it| it.2 == value);
                        assert_eq!(wheel.cancel(handle), live.map(|_| value), "seed {seed}");
                        assert_eq!(old.cancel(old_handle), live.map(|_| value), "seed {seed}");
                        if let Some(at) = live {
                            model.timers.remove(at);
                        }
                        assert_eq!(wheel.cancel(handle), None, "seed {seed}: cancelled twice");
                        assert_eq!(wheel.deadline(handle), None, "seed {seed}: stale");
                    }
                    7 => {
                        let soonest = model.timers.iter().map(|it| it.0).min();
                        assert_eq!(wheel.next_deadline(), soonest, "seed {seed}");
                        assert_eq!(old.next_deadline(), soonest, "seed {seed}");
                    }
                    _ => {
                        let to = match rng.below(5) {
                            0 => model.now.saturating_sub(rng.below(10)),
                            1 => wheel.next_deadline().unwrap_or(model.now),
                            2 => u64::MAX,
                            _ => deadline(&mut rng, model.now),
                        };
                        let mut fired = Vec::new();
                        wheel.advance(to, |at, value| fired.push((at, value)));
                        let mut old_fired = Vec::new();
                        old.advance(to, |at, value| old_fired.push((at, value)));
                        assert_eq!(fired, model.advance(to), "seed {seed}");
                        assert_eq!(fired, old_fired, "seed {seed}");
                        assert_eq!(wheel.now(), model.now, "seed {seed}");
                        assert_eq!(old.now(), model.now, "seed {seed}");
                    }
                }
                assert_eq!(wheel.len(), model.timers.len(), "seed {seed}");
                assert_eq!(old.len(), model.timers.len(), "seed {seed}");
            }
            let mut rest = Vec::new();
            wheel.drain(|at, value| rest.push((at, value)));
            assert_eq!(rest, model.advance(u64::MAX), "seed {seed}");
            assert!(wheel.is_empty());
        }
    }

    #[test]
    fn one_deadline_fires_in_insertion_order_across_levels() {
        let mut wheel = Wheel::new();
        let due = (1 << 40) + 5;
        for value in 0..41u32 {
            wheel.insert(due, value);
            // Closer each time, so the same deadline is placed from every level as the
            // earlier ones are moved down.
            wheel.advance(due - (1 << (40 - value)), |_, _| panic!("not due"));
        }
        let mut fired = Vec::new();
        wheel.advance(due, |at, value| fired.push((at, value)));
        assert_eq!(fired, (0..41).map(|it| (due, it)).collect::<Vec<_>>());
    }

    #[test]
    fn a_stale_handle_names_nothing_after_its_entry_is_reused() {
        let mut wheel = Wheel::new();
        let first = wheel.insert(10, "a");
        wheel.advance(10, |_, _| {});
        let second = wheel.insert(20, "b");
        assert_ne!(first, second, "the reused slot has a new generation");
        assert_eq!((wheel.cancel(first), wheel.deadline(first)), (None, None));
        assert_eq!(wheel.get_mut(first), None);
        assert_eq!(wheel.get_mut(second).copied(), Some("b"));
        assert_eq!(wheel.cancel(second), Some("b"));
    }

    #[test]
    fn a_full_wheel_is_an_error_naming_io() {
        let mut wheel = Wheel::try_new(2).unwrap();
        wheel.try_insert(1, ()).unwrap();
        wheel.try_insert(2, ()).unwrap();
        let error = wheel.try_insert(3, ()).unwrap_err();
        assert_eq!(error.tag(), MemTag::Io, "{error}");
        assert_eq!(wheel.len(), 2);
    }

    /// Once the slab has grown to the working set, arming, cancelling and firing timers make
    /// no call to the global allocator.
    #[test]
    fn a_warm_arm_and_fire_allocates_nothing() {
        let mut wheel = Wheel::new();
        for at in 0..64u64 {
            wheel.insert(at, at);
        }
        wheel.advance(64, |_, _| {});
        let mut fired = 0u64;
        blaze_alloc_count::assert_at_most(0, "a warm timer arm, cancel and fire", || {
            for round in 0..10_000u64 {
                let now = wheel.now();
                let far = wheel.insert(now + 3 + (round << 20), round);
                let near = wheel.insert(now + 2, round);
                assert_eq!(wheel.cancel(far), Some(round));
                wheel.advance(now + 2, |_, value| {
                    assert_eq!(value, round);
                    fired += 1;
                });
                assert_eq!(wheel.deadline(near), None);
            }
        });
        assert_eq!(fired, 10_000);
    }
}
