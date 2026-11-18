//! The cell index shared by a phase's workers. A view borrows it immutably. Insertion,
//! replacement, removal and growth all require an exclusive borrow on the engine thread.

use blaze_own::SharedAlloc;

use crate::engine::{CellId, SVec};

// NameCell::key and recorder::bucket_cell take the first 16 bytes of domain-separated
// BLAKE3 output. Thus cell[0..8] already supplies uniform bucket bits within one definer.
// WHOLE and the other fixed selectors instead get those bits from the defining name's
// subject (e.g. Name::file hashes the path). A signature_name made from an ItemKey shares
// subject[0..8] with its crate, so include the item's digest bits in subject[8..16] too.
// All three words are little-endian. No key is serialized or hashed here.
fn tag(id: &CellId) -> u64 {
    let (subject, _) = id.of.subject.0.as_chunks::<8>();
    let (selector, _) = id.cell.0.as_chunks::<8>();
    u64::from_le_bytes(subject[0]) ^ u64::from_le_bytes(subject[1]).rotate_left(23) ^ u64::from_le_bytes(selector[0])
}

// Equal tags, selectors and subject[8..] imply equal subject[0..8] by cancellation in
// the XOR above. Together with the kind this checks every bit of CellId.
fn same_remainder(a: &CellId, b: &CellId) -> bool {
    a.of.kind == b.of.kind && a.cell == b.cell && a.of.subject.0[8..] == b.of.subject.0[8..]
}

#[derive(Clone, Copy)]
struct Slot {
    tag: u64,
    row: usize,
}

impl Slot {
    const EMPTY: Self = Self { tag: 0, row: usize::MAX };
}

/// A dense cell column indexed by linear probing. An allocated slot array has power-of-two
/// length and at least half its slots empty. A slot names exactly one live row. The full
/// digest and value occupy space only for live rows. Iteration uses a separate index in
/// ascending CellId order, shared across a finished working set's traversals.
pub struct CellTable<'a, V> {
    slots: SVec<'a, Slot>,
    rows: SVec<'a, (CellId, V)>,
    order: SVec<'a, usize>,
}

impl<'a, V> CellTable<'a, V> {
    pub fn new_in(alloc: SharedAlloc<'a>) -> Self {
        Self { slots: SVec::new_in(alloc), rows: SVec::new_in(alloc), order: SVec::new_in(alloc) }
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    // On a populated slot array, find the exact entry or its first empty slot. The empty
    // half bounds the walk even when all the keys have the same tag.
    fn probe<const TAG_ONLY: bool>(&self, id: &CellId, tag: u64) -> Result<(usize, usize), usize> {
        let mask = self.slots.len() - 1;
        let mut at = tag as usize & mask;
        loop {
            let slot = self.slots[at];
            if slot.row == usize::MAX {
                return Err(at);
            }
            if slot.tag == tag && (TAG_ONLY || same_remainder(&self.rows[slot.row].0, id)) {
                return Ok((at, slot.row));
            }
            at = (at + 1) & mask;
        }
    }

    pub fn get(&self, id: &CellId) -> Option<&V> {
        self.lookup::<false>(id)
    }

    fn lookup<const TAG_ONLY: bool>(&self, id: &CellId) -> Option<&V> {
        if self.is_empty() {
            return None;
        }
        let (_, row) = self.probe::<TAG_ONLY>(id, tag(id)).ok()?;
        Some(&self.rows[row].1)
    }

    pub fn contains_key(&self, id: &CellId) -> bool {
        self.get(id).is_some()
    }

    fn grow(&mut self) {
        // Slot size bounds any allocated length well below usize::MAX / 2.
        let size = (self.slots.len() * 2).max(2);
        let mut slots = SVec::from_elem_in(Slot::EMPTY, size, *self.slots.allocator());
        for slot in self.slots.iter().filter(|slot| slot.row != usize::MAX) {
            let mut at = slot.tag as usize & (size - 1);
            while slots[at].row != usize::MAX {
                at = (at + 1) & (size - 1);
            }
            slots[at] = *slot;
        }
        self.slots = slots;
    }

    pub fn insert(&mut self, id: CellId, value: V) -> Option<V> {
        let tag = tag(&id);
        if self.slots.is_empty() {
            self.grow();
        }
        let mut at = match self.probe::<false>(&id, tag) {
            Ok((_, row)) => return Some(core::mem::replace(&mut self.rows[row].1, value)),
            Err(at) => at,
        };
        if self.len() == self.slots.len() / 2 {
            self.grow();
            at = self.probe::<false>(&id, tag).expect_err("growing the cell table preserves the missing cell");
        }
        let row = self.len();
        // A cold answer is filled by the sorted fold. Keep its order as we append, so
        // encoding it needs no second sort. Unordered writes are sorted together at fold.
        if self.order.len() == row {
            if self.order.last().is_none_or(|&last| self.rows[last].0 < id) {
                self.order.push(row);
            } else {
                self.order.clear();
            }
        }
        self.rows.push((id, value));
        self.slots[at] = Slot { tag, row };
        None
    }

    pub fn remove(&mut self, id: &CellId) -> Option<V> {
        if self.is_empty() {
            return None;
        }
        let (mut hole, row) = self.probe::<false>(id, tag(id)).ok()?;
        let mask = self.slots.len() - 1;
        let mut next = (hole + 1) & mask;
        while self.slots[next].row != usize::MAX {
            let slot = self.slots[next];
            let home = slot.tag as usize & mask;
            // Shift a slot back if its probe from home crosses the hole, including wraparound.
            if (hole.wrapping_sub(home) & mask) < (next.wrapping_sub(home) & mask) {
                self.slots[hole] = slot;
                hole = next;
            }
            next = (next + 1) & mask;
        }
        self.slots[hole] = Slot::EMPTY;
        let last = self.len() - 1;
        if row != last {
            let moved = &self.rows[last].0;
            let (at, _) = self.probe::<false>(moved, tag(moved)).expect("the last cell has a slot before its row moves");
            self.slots[at].row = row;
        }
        self.order.clear();
        Some(self.rows.swap_remove(row).1)
    }

    fn sorted_index(&self) -> SVec<'a, usize> {
        let mut order = SVec::with_capacity_in(self.len(), *self.rows.allocator());
        order.extend(0..self.len());
        order.sort_unstable_by(|&a, &b| self.rows[a].0.cmp(&self.rows[b].0));
        order
    }

    pub(crate) fn finish_order(&mut self) {
        if self.order.len() != self.len() {
            self.order = self.sorted_index();
        }
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&CellId, &V)> + DoubleEndedIterator {
        // Newly assembled answers can also be iterated directly. Their iterator owns the
        // index until it is dropped. Point reads never build an index or take a lock.
        let scratch = (self.order.len() != self.len()).then(|| self.sorted_index());
        (0..self.len()).map(move |at| {
            let row = &self.rows[scratch.as_ref().unwrap_or(&self.order)[at]];
            (&row.0, &row.1)
        })
    }

    pub fn keys(&self) -> impl ExactSizeIterator<Item = &CellId> + DoubleEndedIterator {
        self.iter().map(|(id, _)| id)
    }
}

impl<V: PartialEq> PartialEq for CellTable<'_, V> {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len() && self.rows.iter().all(|(id, value)| other.get(id) == Some(value))
    }
}

impl<V: Eq> Eq for CellTable<'_, V> {}

#[cfg(test)]
#[path = "cell_table_sim.rs"]
mod tests;
