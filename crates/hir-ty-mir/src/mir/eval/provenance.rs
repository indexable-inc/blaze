//! The machine of const eval: it tracks only which bytes of memory hold a pointer with
//! provenance, so that an evaluated constant says which of its words are pointers
//! (`ComplexMemoryMap::note_pointer`), as rustc's interned allocation keeps its provenance.
//! A word that was an integer stays one, however it is laid out or what it points at. Its
//! other hooks are empty, as [`Plain`](super::machine::Plain)'s are.

use std::{collections::BTreeMap, ops::Range};

use smallvec::SmallVec;

use super::machine::{
    Access, AtomicOrd, EvalThread, MResult, Machine, MemKind, Ptr, RetagMode, Shadow, Site,
    TypedCx,
};
use crate::next_solver::Ty;

/// The pointers with provenance in a value outside memory: each one's start and length,
/// sorted by start and disjoint. Provenance is that of a whole pointer, as the test machine's
/// per-byte record has it: bytes put over a pointer's start take its provenance away, and
/// bytes put over its rest end it there.
#[derive(Clone, Debug, Default, PartialEq, Eq)] // clone: a machine's `Shadow`, which a value's copy copies
pub(super) struct Pointers {
    len: usize,
    pieces: SmallVec<[(usize, usize); 2]>,
}

impl Pointers {
    /// Takes `start..end` out of every pointer.
    fn cut(&mut self, start: usize, end: usize) {
        if start >= end {
            return;
        }
        self.pieces.retain(|(at, len)| {
            if *at >= end {
                return true;
            }
            if *at >= start {
                return false;
            }
            *len = (*len).min(start - *at);
            true
        });
    }
}

impl Shadow<()> for Pointers {
    #[inline]
    fn init(len: usize) -> Self {
        Self { len, pieces: SmallVec::new() }
    }

    #[inline]
    fn pointer(size: usize, prov: Option<()>) -> Self {
        let mut pieces = SmallVec::new();
        if prov.is_some() && size > 0 {
            pieces.push((0, size));
        }
        Self { len: size, pieces }
    }

    fn slice(&self, range: Range<usize>) -> Self {
        let mut pieces = SmallVec::new();
        pieces.extend(
            self.pieces
                .iter()
                .filter(|(at, _)| range.contains(at))
                .map(|&(at, len)| (at - range.start, len.min(range.end - at))),
        );
        Self { len: range.len(), pieces }
    }

    fn put(&mut self, at: usize, other: &Self) {
        let end = at + other.len;
        self.cut(at, end);
        let index = self.pieces.partition_point(|&(start, _)| start < at);
        self.pieces.insert_many(index, other.pieces.iter().map(|&(start, len)| (start + at, len)));
        self.len = self.len.max(end);
    }

    #[inline]
    fn prov_at(&self, at: usize, size: usize) -> Option<()> {
        self.pieces.iter().any(|&(start, len)| start == at && len == size).then_some(())
    }

    fn pointers(&self, len: usize, size: usize, mut f: impl FnMut(usize)) {
        for &(at, piece) in &self.pieces {
            if piece == size && at + size <= len {
                f(at);
            }
        }
    }
}

/// Which allocations are live and which bytes of memory hold a pointer with provenance.
/// Provenance is `()`: a pointer has it or is an integer, and what it may reach is the
/// checker's to say, not const eval's.
#[derive(Debug, Default)]
pub(super) struct Provenance {
    /// Each live allocation of a nonzero size: its start and size.
    allocations: BTreeMap<u64, u64>,
    /// Each pointer with provenance in memory: its start and length, disjoint.
    pointers: BTreeMap<u64, u64>,
}

impl Provenance {
    /// The pointers in `addr..addr + size`, as a value read from there holds them.
    fn load(&self, addr: u64, size: u64) -> Pointers {
        let end = addr + size;
        let mut pieces = SmallVec::new();
        pieces.extend(
            self.pointers
                .range(addr..end)
                .map(|(&at, &len)| ((at - addr) as usize, len.min(end - at) as usize)),
        );
        Pointers { len: size as usize, pieces }
    }

    /// Takes `start..end` out of every pointer in memory.
    fn cut(&mut self, start: u64, end: u64) {
        if start >= end {
            return;
        }
        if let Some((&at, &len)) = self.pointers.range(..start).next_back()
            && at + len > start
        {
            self.pointers.insert(at, start - at);
        }
        while let Some((&at, _)) = self.pointers.range(start..end).next() {
            self.pointers.remove(&at);
        }
    }

    /// `shadow`'s pointers stored over `addr..addr + size`.
    fn store(&mut self, addr: u64, size: u64, shadow: &Pointers) {
        self.cut(addr, addr + size);
        for &(at, len) in &shadow.pieces {
            let (at, len) = (at as u64, len as u64);
            if at < size {
                self.pointers.insert(addr + at, len.min(size - at));
            }
        }
    }
}

impl<'db> Machine<'db> for Provenance {
    type Prov = ();
    type Shadow = Pointers;

    const CHECKS: bool = true;

    #[inline(always)]
    fn step(&mut self, _: Site<'db>) {}
    fn alloc(&mut self, addr: u64, size: u64, _: u64, _: MemKind, _: bool) -> MResult<()> {
        if size > 0 {
            self.cut(addr, addr + size);
            self.allocations.insert(addr, size);
        }
        Ok(())
    }
    fn free(&mut self, ptr: Ptr<()>, size: Option<u64>, _: Option<u64>, _: MemKind) -> MResult<()> {
        let live = self.allocations.remove(&ptr.addr);
        if let Some(size) = size.or(live) {
            self.cut(ptr.addr, ptr.addr + size);
        }
        Ok(())
    }
    /// A pointer the evaluator lays out to `addr` has provenance when `addr` is in a live
    /// allocation or one past its end.
    fn root(&mut self, addr: u64) -> Option<()> {
        let (&start, &size) = self.allocations.range(..=addr).next_back()?;
        (addr <= start + size).then_some(())
    }
    #[inline]
    fn read(&mut self, ptr: Ptr<()>, size: u64, _: u64, _: Access) -> MResult<Pointers> {
        Ok(self.load(ptr.addr, size))
    }
    #[inline]
    fn write(
        &mut self,
        ptr: Ptr<()>,
        size: u64,
        _: u64,
        _: Access,
        shadow: &Pointers,
    ) -> MResult<()> {
        self.store(ptr.addr, size, shadow);
        Ok(())
    }
    fn copy(&mut self, dst: Ptr<()>, src: Ptr<()>, size: u64, _: u64, _: bool) -> MResult<()> {
        let shadow = self.load(src.addr, size);
        self.store(dst.addr, size, &shadow);
        Ok(())
    }
    #[inline(always)]
    fn require_init(&mut self, _: &Pointers, _: Range<usize>) -> MResult<()> {
        Ok(())
    }
    #[inline(always)]
    fn atomic_load(
        &mut self,
        _: Ptr<()>,
        _: u64,
        _: AtomicOrd,
        _: &[u8],
    ) -> MResult<Option<Vec<u8>>> {
        Ok(None)
    }
    #[inline(always)]
    fn atomic_store(&mut self, _: Ptr<()>, _: AtomicOrd, _: &[u8], _: &[u8]) -> MResult<()> {
        Ok(())
    }
    #[inline(always)]
    fn fence(&mut self, _: AtomicOrd, _: bool) -> MResult<()> {
        Ok(())
    }
    #[inline(always)]
    fn offset_from(&mut self, _: Ptr<()>, _: Ptr<()>, _: bool) -> MResult<()> {
        Ok(())
    }
    #[inline(always)]
    fn typed(
        &mut self,
        _: &TypedCx<'_, 'db>,
        _: Ptr<()>,
        _: Ty<'db>,
        _: &[u8],
        _: RetagMode,
    ) -> MResult<()> {
        Ok(())
    }
    #[inline(always)]
    fn offset(&mut self, _: Ptr<()>, _: i64) -> MResult<()> {
        Ok(())
    }
    #[inline(always)]
    fn expose(&mut self, _: Ptr<()>) -> MResult<()> {
        Ok(())
    }
    /// An integer cast to a pointer has no provenance in const eval, as in rustc's.
    #[inline(always)]
    fn from_exposed(&mut self, _: u64) -> Option<()> {
        None
    }
    /// A function pointer has provenance, as rustc's `GlobalAlloc::Function` does.
    #[inline(always)]
    fn function(&mut self, _: u64) -> Option<()> {
        Some(())
    }
    #[inline(always)]
    fn freeze(&mut self, _: u64) {}
    #[inline(always)]
    fn initialized(&mut self, _: u64) {}
    #[inline(always)]
    fn call(&mut self) -> MResult<()> {
        Ok(())
    }
    #[inline(always)]
    fn protect(&mut self, _: Ptr<()>, _: u64) -> MResult<()> {
        Ok(())
    }
    #[inline(always)]
    fn ret(&mut self) -> MResult<()> {
        Ok(())
    }
    #[inline(always)]
    fn spawn(&mut self, _: EvalThread) -> MResult<()> {
        Ok(())
    }
    /// As [`Plain`](super::machine::Plain)'s: the running thread goes on while it can.
    #[inline(always)]
    fn schedule(&mut self, running: EvalThread, runnable: &[EvalThread]) -> MResult<EvalThread> {
        Ok(match runnable.contains(&running) {
            true => running,
            false => runnable.first().copied().unwrap_or(running),
        })
    }
    #[inline(always)]
    fn join(&mut self, _: EvalThread) -> MResult<()> {
        Ok(())
    }
    #[inline(always)]
    fn detach(&mut self, _: EvalThread) -> MResult<()> {
        Ok(())
    }
    #[inline(always)]
    fn exit(&mut self) -> MResult<()> {
        Ok(())
    }
    #[inline(always)]
    fn yield_now(&mut self) {}
    #[inline(always)]
    fn thread_name(&mut self, _: EvalThread, _: &[u8]) {}
    #[inline(always)]
    fn release(&mut self, _: u64) -> MResult<()> {
        Ok(())
    }
    #[inline(always)]
    fn acquire(&mut self, _: u64) -> MResult<()> {
        Ok(())
    }
    #[inline(always)]
    fn finish(&mut self) -> MResult<()> {
        Ok(())
    }
}
