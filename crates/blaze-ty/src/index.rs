//! The hash-cons index of a column: open addressing over `u32` row ids with a 7-bit tag per
//! slot, probed a group at a time with `core::simd` (SwissTable's group probe: NEON on arm64,
//! SSE2/AVX2 on x86-64).
//!
//! A group is one 128-byte cache line (one line on Apple's cores, two adjacent ones on
//! x86-64): 24 tags, 8 bytes of padding, then the 24 ids. A probe that hits reads the tags and
//! the id from the same line, so a hit on a table past the cache waits for one line of index
//! and one row; with tags and ids in separate arrays it waited for two lines of index, which
//! the cost example measured as most of a cold hit. 128 bytes for 24 slots is 5.3 bytes a
//! slot, against 5 for split arrays.
//!
//! Why not blaze-swiss: its group is one 64-bit word of eight tags read through an atomic
//! load (for tables shared between threads), and its `Index` publishes with atomics. A task's
//! arena is owned by one thread, and its slots are 4-byte ids.
//!
//! No removal, so the first empty slot met on a key's probe path is where it goes: a miss's
//! probe gives the vacancy, and an insert after it does not probe again unless the table grows.
//! Hashes are not stored: a grow asks the caller for each id's hash (`rehash`), which is
//! recomputed from the row in one multiply.

use core::simd::cmp::SimdPartialEq;
use core::simd::{Select, Simd};

use blaze_int::Int;
use blaze_own::{Alloc, Vec};

/// Slots a group holds.
const SLOTS: usize = 24;
const EMPTY: u8 = 0;
/// The padding lanes' byte: neither empty nor any tag (tags have the high bit set).
const PAD: u8 = 0x7f;

#[derive(Clone, Copy)]
#[repr(C, align(128))]
struct Group {
    /// 24 tags, then 8 bytes of `PAD`: one 32-lane compare covers the group.
    tags: [u8; 32],
    ids: [u32; SLOTS],
}

const BLANK: Group = {
    let mut tags = [EMPTY; 32];
    let mut i = SLOTS;
    while i < 32 {
        tags[i] = PAD;
        i += 1;
    }
    Group { tags, ids: [0; SLOTS] }
};

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
    group: usize,
    slot: usize,
    hash: u64,
}

pub(crate) struct Index<'a> {
    groups: Vec<Group, Alloc<'a>>,
    /// Groups minus one; the group count is a power of two.
    mask: usize,
    len: usize,
}

/// The lanes of `g` holding `byte`, one bit a lane at its lane's top bit (bit `8 * lane + 7`
/// of the lane's word), as three words for lanes 0..8, 8..16 and 16..24. The compare is one
/// 32-lane `core::simd` compare; the mask is read as bytes, not with `to_bitmask`, which
/// NEON has no instruction for (the cost example measured it at about 3 ns of a 13 ns hit).
#[inline(always)]
fn matches(g: &Group, byte: u8) -> [u64; 3] {
    let eq = Simd::<u8, 32>::from_array(g.tags).simd_eq(Simd::splat(byte));
    let m = eq.select(Simd::<u8, 32>::splat(0x80), Simd::splat(0)).to_array();
    let w = |i: usize| u64::from_le_bytes([m[i], m[i + 1], m[i + 2], m[i + 3], m[i + 4], m[i + 5], m[i + 6], m[i + 7]]);
    [w(0), w(8), w(16)]
}

/// The first lane of a [`matches`] answer, if any.
#[inline(always)]
fn first(m: &[u64; 3]) -> Option<usize> {
    for (k, w) in m.iter().enumerate() {
        if *w != 0 {
            return Some(k * 8 + w.trailing_zeros().widen_to::<usize>() / 8);
        }
    }
    None
}

impl<'a> Index<'a> {
    /// An index with room for about `rows` ids before it grows.
    pub(crate) fn with_capacity_in(rows: usize, alloc: Alloc<'a>) -> Index<'a> {
        let groups = (rows.saturating_mul(8) / 7 / SLOTS + 1).next_power_of_two();
        let mut g = Vec::new_in(alloc);
        g.resize(groups, BLANK);
        Index { groups: g, mask: groups - 1, len: 0 }
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// Bytes held.
    pub(crate) fn bytes(&self) -> usize {
        self.groups.capacity() * core::mem::size_of::<Group>()
    }

    /// Starts loading `hash`'s home group, for a batch that probes it soon (the first phase of
    /// `Tys::intern_many`, as of blaze-swiss's `Index::find_many`).
    #[inline(always)]
    pub(crate) fn prefetch(&self, hash: u64) {
        let g = &self.groups[hash.truncate_to::<usize>() & self.mask];
        core::hint::prefetch_read(core::ptr::from_ref(g), core::hint::Locality::L1);
    }

    /// The id in the first slot of `hash`'s tag in its home group: the row a batch prefetches
    /// before probing (a hint: another key's tag makes it wasted, never wrong).
    #[inline(always)]
    pub(crate) fn ahead(&self, hash: u64) -> Option<u32> {
        let g = &self.groups[hash.truncate_to::<usize>() & self.mask];
        first(&matches(g, tag(hash))).map(|at| g.ids[at % SLOTS])
    }

    /// The id whose `is` holds among those of `hash`'s tag, or where an id of `hash` goes.
    #[inline]
    pub(crate) fn probe(&self, hash: u64, mut is: impl FnMut(u32) -> bool) -> Probe {
        let t = tag(hash);
        let mut g = hash.truncate_to::<usize>() & self.mask;
        let mut stride = 0;
        loop {
            let group = &self.groups[g];
            let hits = matches(group, t);
            for (k, w) in hits.iter().enumerate() {
                let mut w = *w;
                while w != 0 {
                    // A lane of the three words is under 24.
                    let at = (k * 8 + w.trailing_zeros().widen_to::<usize>() / 8) % SLOTS;
                    let id = group.ids[at];
                    if is(id) {
                        return Probe::Found(id);
                    }
                    w &= w - 1;
                }
            }
            if let Some(at) = first(&matches(group, EMPTY)) {
                return Probe::Vacant(Vacant { group: g, slot: at % SLOTS, hash });
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
        let cap = self.groups.len() * SLOTS;
        if (self.len + 1) * 8 > cap * 7 {
            self.grow(rehash);
            self.place(v.hash, id);
        } else {
            let g = &mut self.groups[v.group];
            g.tags[v.slot] = tag(v.hash);
            g.ids[v.slot] = id;
        }
        self.len += 1;
    }

    /// Puts `id` in the first empty slot of `hash`'s probe path.
    fn place(&mut self, hash: u64, id: u32) {
        let mut g = hash.truncate_to::<usize>() & self.mask;
        let mut stride = 0;
        loop {
            let group = &mut self.groups[g];
            if let Some(at) = first(&matches(group, EMPTY)) {
                let at = at % SLOTS;
                group.tags[at] = tag(hash);
                group.ids[at] = id;
                return;
            }
            stride += 1;
            g = (g + stride) & self.mask;
        }
    }

    /// Doubles the groups and puts every id back.
    #[cold]
    fn grow(&mut self, rehash: impl Fn(u32) -> u64) {
        let alloc = *self.groups.allocator();
        let count = (self.mask + 1) * 2;
        let mut groups = Vec::new_in(alloc);
        groups.resize(count, BLANK);
        let old = core::mem::replace(&mut self.groups, groups);
        self.mask = count - 1;
        for g in old.iter() {
            for (t, id) in g.tags[..SLOTS].iter().zip(g.ids.iter()) {
                if *t != EMPTY {
                    self.place(rehash(*id), *id);
                }
            }
        }
    }
}
