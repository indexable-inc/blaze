//! Compile-time memory: allocations with provenance (consteval.md, "Values and memory").
//!
//! An address is an allocation and an offset ([`Pointer`]), never a number. Each allocation
//! keeps its bytes, which of them were written, and where pointers are stored in it (a pointer
//! slot holds its offset as eight little-endian bytes and a relocation naming the allocation).
//! Reading bytes never written, outside the allocation, from an allocation that has ended, or
//! part of a pointer is a typed refusal; so is reading a pointer as an integer.
//!
//! The target is little-endian with 64-bit pointers (aarch64 and x86-64, the hosts in scope).

use core::alloc::Allocator;

use blaze_build::InstanceKey;
use blaze_own::Vec;
use blaze_typeck::ItemKey;

use crate::error::Refusal;

/// The bytes of a pointer.
pub const PTR: u64 = 8;

/// An allocation of one evaluation, by its index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AllocId(pub u32);

/// An address: an allocation and an offset in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pointer {
    pub alloc: AllocId,
    pub offset: u64,
}

impl Pointer {
    pub(crate) fn at(self, delta: u64) -> Pointer {
        Pointer { alloc: self.alloc, offset: self.offset.wrapping_add(delta) }
    }
}

/// A scalar as memory holds it: integer bits (of the width the type says) or a pointer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scalar {
    Int(u128),
    Ptr(Pointer),
}

/// Where an allocation's memory came from: what it may become in the record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// A temporary, a literal or a value read from another constant's record: interned into
    /// this record as one of its own allocations if the value points to it.
    Data,
    /// A local binding of a frame: ends when its frame returns.
    Local,
    /// A static, by item: its bytes are read from its record when first used (`loaded`).
    Static(ItemKey),
    /// A fn instance (a fn pointer's target): no bytes.
    Fn(InstanceKey),
}

/// One allocation: a region of the evaluation's heap.
pub struct Allocation {
    /// Where its bytes start in `Memory::heap`.
    start: u64,
    len: u64,
    pub(crate) align_log2: u8,
    pub(crate) mutable: bool,
    pub(crate) live: bool,
    pub(crate) origin: Origin,
    /// A static's bytes have been read from its record.
    pub(crate) loaded: bool,
}

impl Allocation {
    pub(crate) fn len(&self) -> u64 {
        self.len
    }
}

/// The memory of one evaluation, under a bound on live bytes. Every allocation's bytes are a
/// region of one heap, with one bit per heap byte for written or not. Regions are handed out
/// at the heap's end and given back in stack order: a dead region at the end is cut off, as
/// temporaries and locals die last made, first ended. An allocation's id is never reused, so a
/// pointer to an ended one stays `Dangling` whatever its region holds now.
///
/// One `Memory` serves every evaluation of a unit, kept with its code ([`crate::Codes`]):
/// [`Memory::reset`] empties it and keeps its vectors' room, so an evaluation grows nothing it
/// grew before.
pub struct Memory<A: Allocator + Clone> {
    pub(crate) allocs: Vec<Allocation, A>,
    /// Each allocation's pointer slots, sorted by offset, by id. The vectors outlive
    /// [`Memory::reset`] with their room, so storing a pointer allocates nothing once warm.
    relocs: Vec<Vec<(u64, AllocId), A>, A>,
    heap: Vec<u8, A>,
    /// One bit per heap byte: written or not.
    init: Vec<u64, A>,
    /// The allocations with a region, in heap order.
    regions: Vec<AllocId, A>,
    /// Pointer slots in transit (`copy`).
    scratch: Vec<(u64, AllocId), A>,
    live_bytes: u64,
    limit: u64,
    alloc: A,
}

impl<A: Allocator + Clone> Memory<A> {
    pub(crate) fn new_in(limit: u64, alloc: A) -> Memory<A> {
        Memory {
            allocs: Vec::new_in(alloc.clone()),
            relocs: Vec::new_in(alloc.clone()),
            heap: Vec::new_in(alloc.clone()),
            init: Vec::new_in(alloc.clone()),
            regions: Vec::new_in(alloc.clone()),
            scratch: Vec::new_in(alloc.clone()),
            live_bytes: 0,
            limit,
            alloc,
        }
    }

    /// Empty, for a new evaluation under `limit`: no allocation is live and none is known.
    pub(crate) fn reset(&mut self, limit: u64) {
        let used = self.allocs.len().min(self.relocs.len());
        for r in self.relocs[..used].iter_mut() {
            r.clear();
        }
        self.allocs.clear();
        self.heap.clear();
        self.init.clear();
        self.regions.clear();
        self.scratch.clear();
        self.live_bytes = 0;
        self.limit = limit;
    }

    /// The live bytes now: what the bound is held against.
    pub fn live_bytes(&self) -> u64 {
        self.live_bytes
    }

    fn is_init(&self, i: u64) -> bool {
        self.init.get((i / 64) as usize).is_some_and(|w| w >> (i % 64) & 1 == 1)
    }

    /// Whether every heap byte of `from..to` is written: a word at a time.
    fn all_init(&self, from: u64, to: u64) -> bool {
        let mut i = from;
        while i < to {
            let (w, b) = ((i / 64) as usize, i % 64);
            let n = (64 - b).min(to - i);
            let m = if n == 64 { u64::MAX } else { ((1u64 << n) - 1) << b };
            if self.init.get(w).is_none_or(|&x| x & m != m) {
                return false;
            }
            i += n;
        }
        true
    }

    /// Zeroes the bytes of `out` (a copy of allocation `id`'s) that were never written.
    pub(crate) fn zero_unwritten(&self, id: AllocId, out: &mut [u8]) {
        let Some(a) = self.allocs.get(id.0 as usize) else { return };
        if self.all_init(a.start, a.start + a.len) {
            return;
        }
        // A word of written-bits at a time; only a word with a gap is looked at bit by bit.
        let mut k = 0u64;
        while k < a.len {
            let i = a.start + k;
            let (w, bit) = ((i / 64) as usize, i % 64);
            let n = (64 - bit).min(a.len - k);
            let m = if n == 64 { u64::MAX } else { (1u64 << n) - 1 };
            let word = self.init.get(w).copied().unwrap_or(0) >> bit;
            if word & m != m {
                for j in 0..n {
                    if word >> j & 1 == 0 {
                        out[(k + j) as usize] = 0;
                    }
                }
            }
            k += n;
        }
    }

    /// Marks heap bytes `from..to` written (`on`) or not.
    fn set_init(&mut self, from: u64, to: u64, on: bool) {
        let mut i = from;
        while i < to {
            let (w, b) = ((i / 64) as usize, i % 64);
            let n = (64 - b).min(to - i);
            let m = if n == 64 { u64::MAX } else { ((1u64 << n) - 1) << b };
            if on {
                self.init[w] |= m;
            } else {
                self.init[w] &= !m;
            }
            i += n;
        }
    }

    /// A fresh region of `size` bytes at the heap's end, none written.
    fn region(&mut self, size: u64) -> Result<u64, Refusal<'static>> {
        let start = self.heap.len() as u64;
        let end = start.checked_add(size).and_then(|e| usize::try_from(e).ok()).ok_or(Refusal::MemoryLimit { limit: self.limit })?;
        self.heap.resize(end, 0u8);
        let words = end.div_ceil(64);
        if self.init.len() < words {
            self.init.resize(words, 0u64);
        }
        self.set_init(start, end as u64, false);
        Ok(start)
    }

    /// A new allocation of `size` bytes, none written.
    pub(crate) fn allocate(&mut self, size: u64, align_log2: u8, mutable: bool, origin: Origin) -> Result<AllocId, Refusal<'static>> {
        let live = self.live_bytes.saturating_add(size);
        if live > self.limit {
            return Err(Refusal::MemoryLimit { limit: self.limit });
        }
        let id = AllocId(u32::try_from(self.allocs.len()).map_err(|_| Refusal::MemoryLimit { limit: self.limit })?);
        let start = self.region(size)?;
        self.allocs.push(Allocation { start, len: size, align_log2, mutable, live: true, origin, loaded: true });
        if self.relocs.len() < self.allocs.len() {
            self.relocs.push(Vec::new_in(self.alloc.clone()));
        }
        self.regions.push(id);
        self.live_bytes = live;
        Ok(id)
    }

    /// Ends an allocation: later uses of a pointer into it are `Dangling`. Dead regions at the
    /// heap's end go back to it.
    pub(crate) fn kill(&mut self, id: AllocId) {
        if let Some(a) = self.allocs.get_mut(id.0 as usize) {
            if a.live {
                self.live_bytes = self.live_bytes.saturating_sub(a.len);
                a.live = false;
                self.relocs[id.0 as usize].clear();
            }
        }
        while let Some(&top) = self.regions.last() {
            let a = &self.allocs[top.0 as usize];
            if a.live {
                break;
            }
            self.heap.truncate(a.start as usize);
            self.regions.pop();
        }
    }

    /// The pointer slots of allocation `id`, sorted by offset.
    pub(crate) fn relocs(&self, id: AllocId) -> Result<&[(u64, AllocId)], Refusal<'static>> {
        self.relocs.get(id.0 as usize).map(|r| r.as_slice()).ok_or(Refusal::Malformed { what: "a pointer to no allocation" })
    }

    /// The bytes of a live allocation.
    pub(crate) fn bytes(&self, id: AllocId) -> Result<&[u8], Refusal<'static>> {
        let a = self.get(id)?;
        if !a.live {
            return Err(Refusal::Dangling);
        }
        Ok(&self.heap[a.start as usize..(a.start + a.len) as usize])
    }

    pub(crate) fn get(&self, id: AllocId) -> Result<&Allocation,Refusal<'static>> {
        self.allocs.get(id.0 as usize).ok_or(Refusal::Malformed { what: "a pointer to no allocation" })
    }

    pub(crate) fn get_mut(&mut self, id: AllocId) -> Result<&mut Allocation,Refusal<'static>> {
        self.allocs.get_mut(id.0 as usize).ok_or(Refusal::Malformed { what: "a pointer to no allocation" })
    }

    /// The allocation `p` points into, checked live and holding `size` bytes at its offset.
    fn checked(&self, p: Pointer, size: u64) -> Result<&Allocation,Refusal<'static>> {
        let a = self.get(p.alloc)?;
        if !a.live {
            return Err(Refusal::Dangling);
        }
        if p.offset.checked_add(size).is_none_or(|end| end > a.len()) {
            return Err(Refusal::OutOfBounds { offset: p.offset, size, len: a.len() });
        }
        Ok(a)
    }

    /// Checks that `size` bytes at `p` may be read (the offset is in bounds of a live allocation).
    pub(crate) fn check_access(&self, p: Pointer, size: u64) -> Result<(), Refusal<'static>> {
        self.checked(p, size).map(|_| ())
    }

    /// Reads a scalar of `size` bytes at `p`. A pointer slot read whole at size 8 is a pointer.
    pub(crate) fn read(&self, p: Pointer, size: u64) -> Result<Scalar, Refusal<'static>> {
        let a = self.checked(p, size)?;
        let end = p.offset + size;
        if !self.all_init(a.start + p.offset, a.start + end) {
            let first = (p.offset..end).find(|&i| !self.is_init(a.start + i)).unwrap_or(p.offset);
            return Err(Refusal::Uninit { offset: first });
        }
        let mut ptr = None;
        for &(at, target) in self.relocs[p.alloc.0 as usize].iter() {
            if at + PTR <= p.offset || at >= end {
                continue;
            }
            if at == p.offset && size == PTR {
                ptr = Some(target);
            } else {
                return Err(Refusal::PartialPointer { offset: at });
            }
        }
        let s = &self.heap[(a.start + p.offset) as usize..(a.start + end) as usize];
        // The scalar sizes as fixed-size loads; any other size byte by byte.
        let bits = if let Ok(w) = <[u8; 8]>::try_from(s) {
            u128::from(u64::from_le_bytes(w))
        } else if let Ok(w) = <[u8; 4]>::try_from(s) {
            u128::from(u32::from_le_bytes(w))
        } else if let Ok(w) = <[u8; 16]>::try_from(s) {
            u128::from_le_bytes(w)
        } else {
            let mut bits: u128 = 0;
            for (k, &b) in s.iter().enumerate() {
                bits |= u128::from(b) << (8 * k);
            }
            bits
        };
        Ok(match ptr {
            Some(alloc) => Scalar::Ptr(Pointer { alloc, offset: bits as u64 }),
            None => Scalar::Int(bits),
        })
    }

    /// The start of a writable allocation's region, `size` bytes at `p` checked.
    fn writable(&self, p: Pointer, size: u64) -> Result<u64, Refusal<'static>> {
        let a = self.checked(p, size)?;
        if !a.mutable {
            return Err(Refusal::WriteToImmutable);
        }
        Ok(a.start)
    }

    /// Writes scalar `v` in `size` bytes at `p`.
    pub(crate) fn write(&mut self, p: Pointer, size: u64, v: Scalar) -> Result<(), Refusal<'static>> {
        let start = self.writable(p, size)?;
        let end = p.offset + size;
        let r = &mut self.relocs[p.alloc.0 as usize];
        if !r.is_empty() {
            r.retain(|&(at, _)| at + PTR <= p.offset || at >= end);
        }
        let bits = match v {
            Scalar::Int(b) => b,
            Scalar::Ptr(q) => {
                if size != PTR {
                    return Err(Refusal::PointerAsInt);
                }
                let at = r.iter().position(|&(o, _)| o > p.offset).unwrap_or(r.len());
                r.insert(at, (p.offset, q.alloc));
                u128::from(q.offset)
            }
        };
        let at = (start + p.offset) as usize;
        let b = bits.to_le_bytes();
        // The scalar sizes as fixed-size stores, not a `memcpy` call each.
        match size {
            1 => self.heap[at] = b[0],
            2 => self.heap[at..at + 2].copy_from_slice(&b[..2]),
            4 => self.heap[at..at + 4].copy_from_slice(&b[..4]),
            8 => self.heap[at..at + 8].copy_from_slice(&b[..8]),
            16 => self.heap[at..at + 16].copy_from_slice(&b),
            _ => self.heap[at..at + size as usize].copy_from_slice(&b[..size as usize]),
        }
        self.set_init(start + p.offset, start + end, true);
        Ok(())
    }

    /// Copies `size` bytes from `src` to `dst`, with which were written and the pointers in them.
    pub(crate) fn copy(&mut self, src: Pointer, dst: Pointer, size: u64) -> Result<(), Refusal<'static>> {
        if size == 0 {
            return Ok(());
        }
        let from = self.checked(src, size)?.start + src.offset;
        self.scratch.clear();
        for &(at, t) in self.relocs[src.alloc.0 as usize].iter() {
            if at >= src.offset && at + PTR <= src.offset + size {
                self.scratch.push((at - src.offset, t));
            } else if at + PTR > src.offset && at < src.offset + size {
                return Err(Refusal::PartialPointer { offset: at });
            }
        }
        let to = self.writable(dst, size)? + dst.offset;
        self.heap.copy_within(from as usize..(from + size) as usize, to as usize);
        // The written bits: whole runs when the source is all written, as nearly always.
        if self.all_init(from, from + size) {
            self.set_init(to, to + size, true);
        } else {
            for k in 0..size {
                let on = self.is_init(from + k);
                self.set_init(to + k, to + k + 1, on);
            }
        }
        let d = &mut self.relocs[dst.alloc.0 as usize];
        if !d.is_empty() {
            d.retain(|&(at, _)| at + PTR <= dst.offset || at >= dst.offset + size);
        }
        for &(at, t) in self.scratch.iter() {
            let o = dst.offset + at;
            let pos = d.iter().position(|&(x, _)| x > o).unwrap_or(d.len());
            d.insert(pos, (o, t));
        }
        Ok(())
    }

    /// Fills an allocation's bytes from a record: every byte written, the pointer slots given.
    pub(crate) fn fill(&mut self, id: AllocId, bytes: &[u8], relocs: &[(u64, AllocId)]) -> Result<(), Refusal<'static>> {
        let (start, len) = {
            let a = self.get(id)?;
            (a.start, a.len)
        };
        if bytes.len() as u64 != len {
            return Err(Refusal::Malformed { what: "a record's allocation of another size than its type" });
        }
        self.heap[start as usize..(start + len) as usize].copy_from_slice(bytes);
        self.set_init(start, start + len, true);
        let r = &mut self.relocs[id.0 as usize];
        r.clear();
        r.extend_from_slice(relocs);
        self.allocs[id.0 as usize].loaded = true;
        Ok(())
    }

    /// Gives a not yet loaded static's allocation (of no bytes) a region of its record's size.
    pub(crate) fn resize_unloaded(&mut self, id: AllocId, size: u64) -> Result<(), Refusal<'static>> {
        let live = self.live_bytes.saturating_add(size);
        if live > self.limit {
            return Err(Refusal::MemoryLimit { limit: self.limit });
        }
        if self.get(id)?.len != 0 {
            return Err(Refusal::Malformed { what: "a static's allocation resized twice" });
        }
        self.live_bytes = live;
        let start = self.region(size)?;
        // Its region is now the heap's last: keep `regions` in heap order.
        if let Some(i) = self.regions.iter().rposition(|&r| r == id) {
            self.regions.remove(i);
        }
        self.regions.push(id);
        let a = &mut self.allocs[id.0 as usize];
        a.start = start;
        a.len = size;
        Ok(())
    }
}
