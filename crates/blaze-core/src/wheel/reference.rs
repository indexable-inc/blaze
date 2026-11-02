//! The wheel as it was before its timers moved onto a `blaze_mem::Slab`: links and values in
//! two `Vec`s with a free list of their own. Kept only as the reference the slab-backed wheel
//! is compared with.

const BITS: u32 = 6;
const SLOTS: usize = 1 << BITS;
const LEVELS: usize = 11;
const NIL: u32 = u32::MAX;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(super) struct Handle {
    index: u32,
    generation: u32,
}

#[derive(Clone, Copy)]
struct Link {
    deadline: u64,
    prev: u32,
    next: u32,
    generation: u32,
    slot: u32,
}

#[derive(Clone, Copy)]
struct List {
    head: u32,
    tail: u32,
}

const EMPTY: List = List { head: NIL, tail: NIL };

pub(super) struct Wheel<T> {
    now: u64,
    occupied: [u64; LEVELS],
    slots: Box<[List; LEVELS * SLOTS]>,
    links: Vec<Link>,
    values: Vec<Option<T>>,
    free: u32,
    len: usize,
    soonest: Option<u64>,
}

impl<T> Wheel<T> {
    pub(super) fn new() -> Wheel<T> {
        Wheel {
            now: 0,
            occupied: [0; LEVELS],
            // alloc: the old wheel, kept only as the differential test's oracle.
            slots: Box::new([EMPTY; LEVELS * SLOTS]), // alloc: the test oracle's.
            links: Vec::new(), // alloc: the test oracle's.
            values: Vec::new(),
            free: NIL,
            len: 0,
            soonest: None,
        }
    }

    pub(super) fn len(&self) -> usize {
        self.len
    }

    pub(super) fn now(&self) -> u64 {
        self.now
    }

    pub(super) fn insert(&mut self, deadline: u64, value: T) -> Handle {
        let deadline = deadline.max(self.now);
        let index = match self.free {
            NIL => {
                let index = u32::try_from(self.links.len()).expect("fewer than 2^32 timers");
                self.links.push(Link { deadline, prev: NIL, next: NIL, generation: 0, slot: NIL });
                self.values.push(Some(value));
                index
            }
            index => {
                self.free = self.links[index as usize].next;
                self.links[index as usize].deadline = deadline;
                self.values[index as usize] = Some(value);
                index
            }
        };
        self.place(index);
        self.len += 1;
        self.soonest = match (self.len, self.soonest) {
            (1, _) => Some(deadline),
            (_, Some(soonest)) => Some(soonest.min(deadline)),
            (_, None) => None,
        };
        Handle { index, generation: self.links[index as usize].generation }
    }

    pub(super) fn deadline(&self, handle: Handle) -> Option<u64> {
        self.live(handle).then(|| self.links[handle.index as usize].deadline)
    }

    pub(super) fn cancel(&mut self, handle: Handle) -> Option<T> {
        if !self.live(handle) {
            return None;
        }
        let index = handle.index;
        let deadline = self.links[index as usize].deadline;
        self.unlink(index);
        if self.soonest == Some(deadline) {
            self.soonest = None;
        }
        Some(self.release(index))
    }

    pub(super) fn next_deadline(&mut self) -> Option<u64> {
        if self.len == 0 {
            return None;
        }
        if let Some(soonest) = self.soonest {
            return Some(soonest);
        }
        let (level, slot) = self.first().expect("a timer is in the wheel");
        let soonest = match level {
            0 => self.start(0, slot),
            _ => {
                let mut soonest = u64::MAX;
                let mut at = self.slots[level * SLOTS + slot].head;
                while at != NIL {
                    let link = &self.links[at as usize];
                    soonest = soonest.min(link.deadline);
                    at = link.next;
                }
                soonest
            }
        };
        self.soonest = Some(soonest);
        Some(soonest)
    }

    pub(super) fn advance(&mut self, to: u64, mut fire: impl FnMut(u64, T)) {
        while let Some((level, slot)) = self.first() {
            let start = self.start(level, slot);
            if start > to {
                break;
            }
            self.now = start;
            let list = std::mem::replace(&mut self.slots[level * SLOTS + slot], EMPTY);
            self.occupied[level] &= !(1 << slot);
            let mut at = list.head;
            while at != NIL {
                let next = self.links[at as usize].next;
                match level {
                    0 => {
                        let deadline = self.links[at as usize].deadline;
                        if self.soonest.is_some_and(|it| it <= deadline) {
                            self.soonest = None;
                        }
                        let value = self.release(at);
                        fire(deadline, value);
                    }
                    _ => self.place(at),
                }
                at = next;
            }
        }
        self.now = self.now.max(to);
    }

    fn live(&self, handle: Handle) -> bool {
        self.links
            .get(handle.index as usize)
            .is_some_and(|it| it.generation == handle.generation && it.slot != NIL)
    }

    fn first(&self) -> Option<(usize, usize)> {
        let level = self.occupied.iter().position(|&it| it != 0)?;
        Some((level, self.occupied[level].trailing_zeros() as usize))
    }

    fn start(&self, level: usize, slot: usize) -> u64 {
        let above = u64::MAX.checked_shl(BITS * (level as u32 + 1)).unwrap_or(0);
        (self.now & above) | ((slot as u64) << (BITS * level as u32))
    }

    fn place(&mut self, index: u32) {
        let deadline = self.links[index as usize].deadline;
        let differ = (self.now ^ deadline) | (SLOTS as u64 - 1);
        let level = ((63 - differ.leading_zeros()) / BITS) as usize;
        let slot = ((deadline >> (BITS * level as u32)) as usize) & (SLOTS - 1);
        let at = level * SLOTS + slot;
        let tail = self.slots[at].tail;
        let link = &mut self.links[index as usize];
        link.prev = tail;
        link.next = NIL;
        link.slot = at as u32;
        match tail {
            NIL => self.slots[at].head = index,
            tail => self.links[tail as usize].next = index,
        }
        self.slots[at].tail = index;
        self.occupied[level] |= 1 << slot;
    }

    fn unlink(&mut self, index: u32) {
        let Link { prev, next, slot, .. } = self.links[index as usize];
        let list = &mut self.slots[slot as usize];
        match prev {
            NIL => list.head = next,
            prev => self.links[prev as usize].next = next,
        }
        match next {
            NIL => list.tail = prev,
            next => self.links[next as usize].prev = prev,
        }
        if list.head == NIL {
            self.occupied[slot as usize / SLOTS] &= !(1 << (slot as usize % SLOTS));
        }
    }

    fn release(&mut self, index: u32) -> T {
        let link = &mut self.links[index as usize];
        link.slot = NIL;
        link.generation = link.generation.wrapping_add(1);
        link.next = self.free;
        self.free = index;
        self.len -= 1;
        self.values[index as usize].take().expect("a timer in the wheel has its value")
    }
}
