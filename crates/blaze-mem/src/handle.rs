//! Generational handles (docs/v2/plan/design/memory.md, "Generational handles"): objects that
//! are freed and their slots reused (plane tasks, I/O operations, timers, watch slots,
//! live-patch sites) are named by `(index, generation)`. A handle to a freed object is
//! detected, not dereferenced, even after its slot holds another.
//!
//! A [`Slab<T>`] keeps its entries in a [`RegionVec`], so it grows without moving, and its free
//! slots on an intrusive list, newest first (warm in cache). An entry's generation is odd while
//! it holds a value and even while free; a slot whose generation would wrap is retired, never
//! reused, so a stale handle can never match again.
//!
//! Cost: insert, remove and lookup are O(1) with no allocation once the slab has grown to its
//! working set: a lookup is a bounds check, one compare of the generation and a load.

use std::{
    cmp::Ordering,
    fmt,
    hash::{Hash, Hasher},
    marker::PhantomData,
    mem::{self, MaybeUninit},
};

use crate::{MemError, RegionVec, Tag, error::fail};

/// A `T` in a [`Slab<T>`]: its slot and the generation the slot had when the value went in.
#[repr(C)]
pub struct Handle<T> {
    index: u32,
    generation: u32,
    _of: PhantomData<fn() -> T>,
}

const _: () = assert!(mem::size_of::<Handle<String>>() == 8);

impl<T> Handle<T> {
    pub const fn index(self) -> u32 {
        self.index
    }

    pub const fn generation(self) -> u32 {
        self.generation
    }

    /// The handle as one `u64`: generation high, index low.
    pub const fn to_bits(self) -> u64 {
        ((self.generation as u64) << 32) | self.index as u64
    }

    /// The handle [`Handle::to_bits`] gave. Any `u64` is a handle; one no slab issued is just
    /// stale.
    pub const fn from_bits(bits: u64) -> Handle<T> {
        Handle { index: bits as u32, generation: (bits >> 32) as u32, _of: PhantomData }
    }
}

impl<T> Clone for Handle<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Handle<T> {}

impl<T> PartialEq for Handle<T> {
    fn eq(&self, other: &Self) -> bool {
        self.to_bits() == other.to_bits()
    }
}

impl<T> Eq for Handle<T> {}

impl<T> PartialOrd for Handle<T> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<T> Ord for Handle<T> {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.index, self.generation).cmp(&(other.index, other.generation))
    }
}

impl<T> Hash for Handle<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_u64(self.to_bits());
    }
}

impl<T> fmt::Debug for Handle<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}v{}", self.index, self.generation)
    }
}

const NONE: u32 = u32::MAX;

struct Entry<T> {
    /// Odd: holds a value; even: free.
    generation: u32,
    /// The next free slot while free.
    next: u32,
    value: MaybeUninit<T>,
}

impl<T> Entry<T> {
    fn occupied(&self) -> bool {
        self.generation & 1 == 1
    }

    /// Whether the entry holds the value `handle` names. The occupied check is what keeps a
    /// handle made by [`Handle::from_bits`] with an even generation off a free slot.
    #[inline]
    fn holds(&self, handle: Handle<T>) -> bool {
        self.generation == handle.generation && self.occupied()
    }
}

/// Values named by generational [`Handle`]s, for one owner.
pub struct Slab<T> {
    entries: RegionVec<Entry<T>>,
    free: u32,
    len: u32,
}

impl<T> Slab<T> {
    /// A slab of at most `max` values for `tag`, reserving their room and committing none.
    pub fn new(max: u32, tag: Tag) -> Result<Slab<T>, MemError> {
        Ok(Slab { entries: RegionVec::new(max.min(NONE) as usize, tag)?, free: NONE, len: 0 })
    }

    pub fn len(&self) -> usize {
        self.len as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Slots made so far, live, free or retired.
    pub fn slots(&self) -> usize {
        self.entries.len()
    }

    pub fn tag(&self) -> Tag {
        self.entries.tag()
    }

    /// Places the value `make` builds from its own handle.
    pub fn try_insert_with(&mut self, make: impl FnOnce(Handle<T>) -> T) -> Result<Handle<T>, MemError> {
        if self.free != NONE {
            let index = self.free;
            let generation = self.entries[index as usize].generation + 1;
            let handle = Handle { index, generation, _of: PhantomData };
            let value = make(handle);
            let entry = &mut self.entries[index as usize];
            self.free = entry.next;
            entry.value.write(value);
            entry.generation = generation;
            self.len += 1;
            return Ok(handle);
        }
        let index = self.entries.len();
        let index = u32::try_from(index).ok().filter(|&index| index != NONE).ok_or(
            MemError::TooLarge { tag: self.tag(), what: "a slab's slot count", wanted: index },
        )?;
        let handle = Handle { index, generation: 1, _of: PhantomData };
        self.entries.reserve(1)?;
        let value = make(handle);
        self.entries.push(Entry { generation: 1, next: NONE, value: MaybeUninit::new(value) });
        self.len += 1;
        Ok(handle)
    }

    #[track_caller]
    pub fn insert_with(&mut self, make: impl FnOnce(Handle<T>) -> T) -> Handle<T> {
        match self.try_insert_with(make) {
            Ok(handle) => handle,
            Err(error) => fail(&error),
        }
    }

    #[track_caller]
    pub fn insert(&mut self, value: T) -> Handle<T> {
        self.insert_with(|_| value)
    }

    pub fn try_insert(&mut self, value: T) -> Result<Handle<T>, MemError> {
        self.try_insert_with(|_| value)
    }

    fn entry(&self, handle: Handle<T>) -> Option<&Entry<T>> {
        self.entries.get(handle.index as usize).filter(|entry| entry.holds(handle))
    }

    /// The value `handle` names, if it is still there.
    #[inline]
    pub fn get(&self, handle: Handle<T>) -> Option<&T> {
        let entry = self.entry(handle)?;
        // SAFETY: `holds` checked the generations match and the entry's is odd, so it holds a
        // value.
        Some(unsafe { entry.value.assume_init_ref() })
    }

    #[inline]
    pub fn get_mut(&mut self, handle: Handle<T>) -> Option<&mut T> {
        let entry = self.entries.get_mut(handle.index as usize)?;
        if !entry.holds(handle) {
            return None;
        }
        // SAFETY: as for `get`.
        Some(unsafe { entry.value.assume_init_mut() })
    }

    pub fn contains(&self, handle: Handle<T>) -> bool {
        self.entry(handle).is_some()
    }

    /// The handle of the value in slot `index`, if the slot holds one: for an owner that names
    /// its values by slot alone, such as an I/O tag that completes exactly once.
    pub fn handle_at(&self, index: u32) -> Option<Handle<T>> {
        let entry = self.entries.get(index as usize)?;
        entry.occupied().then_some(Handle { index, generation: entry.generation, _of: PhantomData })
    }

    /// Takes the value `handle` names out; every handle to it is stale from now on.
    pub fn remove(&mut self, handle: Handle<T>) -> Option<T> {
        let entry = self.entries.get_mut(handle.index as usize)?;
        if !entry.holds(handle) {
            return None;
        }
        // SAFETY: as for `get`; the generation moves on below, so the value is read out once.
        let value = unsafe { entry.value.assume_init_read() };
        if entry.generation == u32::MAX {
            // Its next generation would wrap to one an old handle may hold: retire the slot.
            entry.generation = 0;
        } else {
            entry.generation += 1;
            entry.next = self.free;
            self.free = handle.index;
        }
        self.len -= 1;
        Some(value)
    }

    /// Every value with its handle, in slot order.
    pub fn iter(&self) -> impl Iterator<Item = (Handle<T>, &T)> {
        self.entries.iter().enumerate().filter(|(_, entry)| entry.occupied()).map(|(index, entry)| {
            let handle = Handle { index: index as u32, generation: entry.generation, _of: PhantomData };
            // SAFETY: the entry is occupied, so it holds a value.
            (handle, unsafe { entry.value.assume_init_ref() })
        })
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = (Handle<T>, &mut T)> {
        self.entries.iter_mut().enumerate().filter(|(_, entry)| entry.occupied()).map(
            |(index, entry)| {
                let handle =
                    Handle { index: index as u32, generation: entry.generation, _of: PhantomData };
                // SAFETY: the entry is occupied, so it holds a value.
                (handle, unsafe { entry.value.assume_init_mut() })
            },
        )
    }

    /// Removes every value; every handle becomes stale.
    pub fn clear(&mut self) {
        for index in 0..self.entries.len() {
            let entry = &self.entries[index];
            if entry.occupied() {
                let handle =
                    Handle { index: index as u32, generation: entry.generation, _of: PhantomData };
                drop(self.remove(handle));
            }
        }
    }
}

impl<T> Drop for Slab<T> {
    fn drop(&mut self) {
        if mem::needs_drop::<T>() {
            for entry in self.entries.iter_mut() {
                if entry.occupied() {
                    // SAFETY: the entry holds a value, dropped once: the slab is going away.
                    unsafe { entry.value.assume_init_drop() };
                }
            }
        }
    }
}

impl<T: fmt::Debug> fmt::Debug for Slab<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}
