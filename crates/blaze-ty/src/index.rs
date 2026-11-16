//! The hash-cons index of a column: open addressing over `u32` row ids with a 7-bit tag per
//! slot, probed sixteen at a time (SwissTable's group probe, with `core::simd`: NEON on arm64,
//! SSE2 on x86-64).
//!
//! Why not blaze-swiss: its group is one 64-bit word of eight tags read through an atomic
//! load (for tables shared between threads), and its `Index` publishes with atomics. A task's
//! arena is owned by one thread, and its slots are 4-byte ids, so a 16-byte group of tags
//! beside 64 bytes of ids is one compare, one mask and usually one id check.
//!
//! No removal, so the first empty slot met on a key's probe path is where it goes: a miss's
//! probe gives the vacancy, and an insert after it does not probe again unless the table grows.
//! Hashes are not stored: a grow asks the caller for each id's hash (`rehash`), which is
//! recomputed from the row in one multiply.

use core::simd::Simd;
use core::simd::cmp::SimdPartialEq;

use blaze_int::Int;
use blaze_own::{Alloc, Vec};

const GROUP: usize = 16;
const EMPTY: u8 = 0;

/// The tag of a hash: its top seven bits, with the high bit set so no tag is `EMPTY`.
#[inline(always)]
fn tag(hash: u64) -> u8 {
    0x80 | (hash >> 57).truncate_to::<u8>()
}

/// A probe's answer.
pub(crate) enum Probe {
    Found(u32),
    /// Absent; where it goes.
    Vacant(Vacant),
}

/// A slot for a key found absent, valid until the next insert.
#[derive(Clone, Copy)]
pub(crate) struct Vacant {
    pos: usize,
    hash: u64,
}

pub(crate) struct Index<'a> {
    ctrl: Vec<u8, Alloc<'a>>,
    slots: Vec<u32, Alloc<'a>>,
    /// Groups minus one; the group count is a power of two.
    mask: usize,
    len: usize,
}

impl<'a> Index<'a> {
    /// An index with room for about `rows` ids before it grows.
    pub(crate) fn with_capacity_in(rows: usize, alloc: Alloc<'a>) -> Index<'a> {
        let groups = (rows.saturating_mul(8) / 7 / GROUP + 1).next_power_of_two();
        let mut ctrl = Vec::new_in(alloc);
        ctrl.resize(groups * GROUP, EMPTY);
        let mut slots = Vec::new_in(alloc);
        slots.resize(groups * GROUP, 0);
        Index { ctrl, slots, mask: groups - 1, len: 0 }
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// Bytes held.
    pub(crate) fn bytes(&self) -> usize {
        self.ctrl.capacity() + self.slots.capacity() * 4
    }

    /// The id whose `is` holds among those of `hash`'s tag, or where an id of `hash` goes.
    #[inline]
    pub(crate) fn probe(&self, hash: u64, mut is: impl FnMut(u32) -> bool) -> Probe {
        let t = Simd::<u8, GROUP>::splat(tag(hash));
        let empty = Simd::<u8, GROUP>::splat(EMPTY);
        let mut g = hash.truncate_to::<usize>() & self.mask;
        let mut stride = 0;
        loop {
            let base = g * GROUP;
            let group = Simd::<u8, GROUP>::from_slice(&self.ctrl[base..base + GROUP]);
            let mut hits = group.simd_eq(t).to_bitmask();
            while hits != 0 {
                let at = base + hits.trailing_zeros().widen_to::<usize>();
                let id = self.slots[at];
                if is(id) {
                    return Probe::Found(id);
                }
                hits &= hits - 1;
            }
            let free = group.simd_eq(empty).to_bitmask();
            if free != 0 {
                let at = base + free.trailing_zeros().widen_to::<usize>();
                return Probe::Vacant(Vacant { pos: at, hash });
            }
            // Triangular probing visits every group of a power-of-two table.
            stride += 1;
            g = (g + stride) & self.mask;
        }
    }

    /// Puts `id` where `v` says; `rehash` gives the hash of any id already in, should the
    /// table grow first.
    #[inline]
    pub(crate) fn fill(&mut self, v: Vacant, id: u32, rehash: impl Fn(u32) -> u64) {
        let cap = self.ctrl.len();
        if (self.len + 1) * 8 > cap * 7 {
            self.grow(rehash);
            self.place(v.hash, id);
        } else {
            self.ctrl[v.pos] = tag(v.hash);
            self.slots[v.pos] = id;
        }
        self.len += 1;
    }

    /// Puts `id` in the first empty slot of `hash`'s probe path.
    fn place(&mut self, hash: u64, id: u32) {
        let empty = Simd::<u8, GROUP>::splat(EMPTY);
        let mut g = hash.truncate_to::<usize>() & self.mask;
        let mut stride = 0;
        loop {
            let base = g * GROUP;
            let group = Simd::<u8, GROUP>::from_slice(&self.ctrl[base..base + GROUP]);
            let free = group.simd_eq(empty).to_bitmask();
            if free != 0 {
                let at = base + free.trailing_zeros().widen_to::<usize>();
                self.ctrl[at] = tag(hash);
                self.slots[at] = id;
                return;
            }
            stride += 1;
            g = (g + stride) & self.mask;
        }
    }

    /// Doubles the groups and puts every id back.
    #[cold]
    fn grow(&mut self, rehash: impl Fn(u32) -> u64) {
        let alloc = *self.ctrl.allocator();
        let groups = (self.mask + 1) * 2;
        let mut ctrl = Vec::new_in(alloc);
        ctrl.resize(groups * GROUP, EMPTY);
        let mut slots = Vec::new_in(alloc);
        slots.resize(groups * GROUP, 0);
        let old_ctrl = core::mem::replace(&mut self.ctrl, ctrl);
        let old_slots = core::mem::replace(&mut self.slots, slots);
        self.mask = groups - 1;
        for (c, id) in old_ctrl.iter().zip(old_slots.iter()) {
            if *c != EMPTY {
                self.place(rehash(*id), *id);
            }
        }
    }
}
