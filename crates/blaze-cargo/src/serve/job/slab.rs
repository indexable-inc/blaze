//! Generational indexes: a value's id is its slot and the slot's generation when it was put
//! there, so a lookup is one index and one compare, and an id kept past its value's removal
//! finds nothing (`slotmap`, `thunderdome`, a game engine's entity table).
//!
//! [`Tagged`] keeps values in a [`blaze_mem::Slab`] (free slots reused newest first, so still
//! in cache; a reserved region, so it grows without moving), each one's handle packed into the
//! low bits of a [`blaze_core::Tag`]. [`Table`] holds values under ids handed out elsewhere
//! (the session's `JobId`).

use blaze_mem::{Handle, MemError, Slab, Tag as MemTag};

/// A value's place in a [`Table`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Key {
    pub slot: u32,
    pub generation: u32,
}

const _: () = assert!(std::mem::size_of::<Key>() == 8);

/// Values in a slab of at most `2^slot_bits`, each named in `slot_bits + generation_bits` bits:
/// its slot, and its generation's low bits above. A slab's generation steps by two per reuse
/// (odd while a slot holds a value), so a packed id is refused once its value is removed
/// until its slot has been reused `2^(generation_bits - 1)` times.
pub struct Tagged<T> {
    slab: Slab<T>,
    slot_bits: u32,
    generation_mask: u32,
}

impl<T> Tagged<T> {
    /// Reserves the room of `2^slot_bits` values, committing none.
    pub fn new(slot_bits: u32, generation_bits: u32, tag: MemTag) -> Result<Tagged<T>, MemError> {
        assert!(slot_bits < 32 && generation_bits <= 32 && slot_bits + generation_bits <= 64);
        let generation_mask = if generation_bits == 32 { u32::MAX } else { (1 << generation_bits) - 1 };
        Ok(Tagged { slab: Slab::new(1 << slot_bits, tag)?, slot_bits, generation_mask })
    }

    /// `value` in a free slot: its handle and the bits it packs into; an error once every slot
    /// is taken.
    pub fn insert(&mut self, value: T) -> Result<(Handle<T>, u64), MemError> {
        // The region may hold more than asked (a page's worth); while fewer than `2^slot_bits`
        // are live, every slot ever made is under `2^slot_bits`, so its index packs.
        let wanted = self.slab.len() + 1;
        if wanted > 1 << self.slot_bits {
            return Err(MemError::TooLarge { tag: self.slab.tag(), what: "values of a tagged slab at once", wanted });
        }
        let handle = self.slab.try_insert(value)?;
        Ok((handle, self.pack(handle)))
    }

    fn pack(&self, handle: Handle<T>) -> u64 {
        u64::from(handle.index()) | u64::from(handle.generation() & self.generation_mask) << self.slot_bits
    }

    /// The value `packed` names: its slot holds one whose generation's low bits are `packed`'s.
    /// Bits no handle of this slab packs into find nothing.
    pub fn find(&self, packed: u64) -> Option<Handle<T>> {
        let generation = packed >> self.slot_bits;
        if generation > u64::from(self.generation_mask) {
            return None;
        }
        let slot = (packed & ((1 << self.slot_bits) - 1)) as u32;
        self.slab.handle_at(slot).filter(|it| u64::from(it.generation() & self.generation_mask) == generation)
    }

    pub fn get(&self, handle: Handle<T>) -> Option<&T> {
        self.slab.get(handle)
    }

    pub fn get_mut(&mut self, handle: Handle<T>) -> Option<&mut T> {
        self.slab.get_mut(handle)
    }

    pub fn remove(&mut self, handle: Handle<T>) -> Option<T> {
        self.slab.remove(handle)
    }

    /// Every value, by slot.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (Handle<T>, &mut T)> {
        self.slab.iter_mut()
    }
}

struct Slot<T> {
    generation: u32,
    value: Option<T>,
}

impl<T> Slot<T> {
    fn get(&self, generation: u32) -> Option<&T> {
        self.value.as_ref().filter(|_| self.generation == generation)
    }

    fn get_mut(&mut self, generation: u32) -> Option<&mut T> {
        self.value.as_mut().filter(|_| self.generation == generation)
    }
}

/// Values under ids handed out elsewhere: a value's slot is its id's, and the one who hands
/// them out puts a slot's next value in only once its last one is removed.
pub struct Table<T> {
    slots: Vec<Slot<T>>,
    len: u32,
}

impl<T> Table<T> {
    pub fn new() -> Table<T> {
        // alloc: none until the first put
        Table { slots: Vec::new(), len: 0 }
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// `value` under `key`; given back while its slot holds another value, or holds a later
    /// generation than `key`'s (a stale id).
    pub fn put(&mut self, key: Key, value: T) -> Result<(), T> {
        let at = key.slot as usize;
        if at >= self.slots.len() {
            // alloc: the table grows to the session's most slots once, then is reused
            self.slots.resize_with(at + 1, || Slot { generation: 0, value: None });
        }
        let slot = &mut self.slots[at];
        if slot.value.is_some() || slot.generation > key.generation {
            return Err(value);
        }
        *slot = Slot { generation: key.generation, value: Some(value) };
        self.len += 1;
        Ok(())
    }

    pub fn get(&self, key: Key) -> Option<&T> {
        self.slots.get(key.slot as usize)?.get(key.generation)
    }

    pub fn get_mut(&mut self, key: Key) -> Option<&mut T> {
        self.slots.get_mut(key.slot as usize)?.get_mut(key.generation)
    }

    pub fn remove(&mut self, key: Key) -> Option<T> {
        let slot = self.slots.get_mut(key.slot as usize)?;
        slot.get(key.generation)?;
        self.len -= 1;
        slot.value.take()
    }

    /// Every value, by slot.
    pub fn values(&self) -> impl Iterator<Item = &T> {
        self.slots.iter().filter_map(|it| it.value.as_ref())
    }

    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut T> {
        self.slots.iter_mut().filter_map(|it| it.value.as_mut())
    }
}

impl<T> Default for Table<T> {
    fn default() -> Table<T> {
        Table::new()
    }
}
