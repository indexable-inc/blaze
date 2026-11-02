//! The checker's memory model: everything about the evaluator's bytes but the bytes. Every
//! allocation of a run with its address, size, alignment, kind and liveness; which of its bytes
//! are initialized and which carry a pointer's provenance; and Miri's checks of bounds, use after
//! free, alignment, read-only memory, frees, exposure for integer-to-pointer casts, and leaks.
//!
//! A port of Miri rule for rule: `compiler/rustc_const_eval/src/interpret/memory.rs`,
//! `compiler/rustc_middle/src/mir/interpret/allocation/{init_mask,provenance_map}.rs`,
//! `src/tools/miri/src/alloc_addresses/mod.rs` and the leak check of
//! `src/tools/miri/src/{eval,diagnostics}.rs`. Miri keeps allocations in a hash map by id and
//! addresses in a sorted vector beside it; here both are columns: an allocation's facts are at
//! its [`AllocId::index`], and the live ones by address in a sorted `(Addr, AllocId)` vector the
//! evaluator's monotonic addresses append to.

mod bytes;
mod init_mask;
mod prov_map;
#[cfg(test)]
mod tests;

use std::fmt;

use blaze_ids::bits::{BitSet, words_for};
use smallvec::SmallVec;

pub use self::bytes::Bytes;
use self::{
    bytes::{Run, offset},
    init_mask::InitMask,
    prov_map::{Frag, ProvMap},
};
use crate::ids::{AccessKind, Addr, AllocId, BorTag, Prov, Span};

/// What made an allocation: the evaluator's `MemKind`. It says how the allocation may be freed
/// and whether it may leak.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    /// A local of a frame.
    Stack,
    /// `__rust_alloc`, freed by `__rust_dealloc`.
    Rust,
    /// `malloc` and its family, freed by `free`.
    C,
    /// A static, a constant, a promoted temporary, a vtable, a string literal.
    Global,
    /// `const_allocate`.
    ConstHeap,
    /// The evaluator's own: program arguments, the environment, thread-local storage, errno
    /// cells, dispatch semaphores (freed by `dispatch_release`).
    Machine,
    /// Anonymous `mmap`, freed by `munmap`.
    Mmap,
}

impl Kind {
    /// Whether memory of this kind may still be live at the end of a run.
    // Miri: machine.rs, `MayLeak for MiriMemoryKind`; interpret/memory.rs, `MayLeak for
    // MemoryKind` (stack memory may not); const_eval/machine.rs, `MayLeak for MemoryKind`
    // (const heap memory only once made global).
    fn may_leak(self, made_global: bool) -> bool {
        match self {
            Kind::Stack | Kind::Rust | Kind::C => false,
            Kind::ConstHeap => made_global,
            Kind::Global | Kind::Machine | Kind::Mmap => true,
        }
    }
}

impl fmt::Display for Kind {
    // Miri: interpret/memory.rs and machine.rs, `Display for MemoryKind` and
    // `Display for MiriMemoryKind`; const_eval/machine.rs for the const heap.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Kind::Stack => "stack variable",
            Kind::Rust => "Rust heap",
            Kind::C => "C heap",
            Kind::Global => "global (static or const)",
            Kind::ConstHeap => "heap allocation",
            Kind::Machine => "machine-managed memory",
            Kind::Mmap => "mmap",
        })
    }
}

/// The facts of one allocation, live or freed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Info {
    pub base: Addr,
    pub size: u64,
    pub align: u64,
    pub kind: Kind,
    pub live: bool,
    /// False for constants, string literals, promoted temporaries and const heap memory made
    /// global: writing them is undefined behavior.
    pub mutable: bool,
    /// Whether a pointer to it was cast to an integer while it was live, so a pointer cast back
    /// from an integer may use it (Miri's `exposed` set).
    pub exposed: bool,
    /// `const_make_global` was called on it.
    pub made_global: bool,
}

const _: () = assert!(size_of::<Info>() == 32);

/// What an allocation's bytes hold besides their values. Freed allocations drop theirs.
#[derive(Clone, Debug)]
struct Shadow {
    init: InitMask,
    prov: ProvMap,
    /// The tags exposed from this allocation, in order.
    exposed: Vec<BorTag>,
}

impl Shadow {
    fn new(size: u64, init: bool) -> Shadow {
        Shadow { init: InitMask::new(size, init), prov: ProvMap::new(), exposed: Vec::new() }
    }
}

/// Which check found an address outside its allocation, for the error's first words.
// Miri: mir/interpret/error.rs, `CheckInAllocMsg`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InAlloc {
    MemoryAccess,
    InboundsPointerArithmetic,
    Dereferenceable,
}

impl fmt::Display for InAlloc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            InAlloc::MemoryAccess => "memory access failed",
            InAlloc::InboundsPointerArithmetic => "in-bounds pointer arithmetic failed",
            InAlloc::Dereferenceable => "pointer not dereferenceable",
        })
    }
}

/// Whether a misaligned access used the pointer itself or a place based on it.
// Miri: mir/interpret/error.rs, `CheckAlignMsg`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AlignMsg {
    /// The pointer of an intrinsic's access (`copy`, atomics, `read_unaligned`'s source).
    AccessedPtr,
    /// A place through a dereference: every typed access.
    BasedOn,
}

/// Where uninitialized memory was read: the read and the first uninitialized part of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Uninit {
    pub alloc: AllocId,
    pub access: Span,
    pub bad: Span,
}

/// An undefined behavior of memory, or an internal error of the evaluator's use of it.
// Miri: mir/interpret/error.rs, `UndefinedBehaviorInfo` (the memory variants) and the
// `throw_ub_format!`s of interpret/memory.rs `deallocate_ptr` and `mem_copy_repeatedly`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MemError {
    /// `PointerOutOfBounds`.
    OutOfBounds { alloc: AllocId, alloc_size: u64, ptr_offset: i64, inbounds_size: i64, msg: InAlloc },
    /// `DanglingIntPointer`: no provenance, or a wildcard pointer at no exposed allocation.
    DanglingInt { addr: Addr, inbounds_size: i64, msg: InAlloc },
    /// `PointerUseAfterFree`.
    UseAfterFree { alloc: AllocId, msg: InAlloc },
    /// `AlignmentCheckFailed`.
    Misaligned { has: u64, required: u64, msg: AlignMsg },
    /// `WriteToReadOnly`.
    WriteToReadOnly(AllocId),
    /// `InvalidUninitBytes`: where, when the bytes are still a read of memory.
    Uninit(Option<Uninit>),
    /// `copy_nonoverlapping` of ranges that overlap.
    CopyOverlapping,
    /// A free of a pointer not at an allocation's start.
    FreeNotStart { addr: Addr, prov: Option<Prov> },
    /// A free of a static, constant or other global.
    FreeStatic(AllocId),
    /// A free of const heap memory made global.
    FreeImmutable(AllocId),
    /// A free by another kind's operation (`free` of a `Box`, `__rust_dealloc` of a local).
    FreeWrongKind { alloc: AllocId, has: Kind, used: Kind },
    /// A free with another size or alignment than the allocation's.
    FreeLayout { alloc: AllocId, size: u64, align: u64, gave_size: u64, gave_align: u64 },
    /// An allocation with an alignment that is not a power of two.
    BadAlign(u64),
    /// `const_make_global` of a pointer to no live allocation.
    MakeGlobalDangling { addr: Addr, prov: Option<Prov> },
    /// `const_make_global` of a pointer not at an allocation's start.
    MakeGlobalOffset { addr: Addr, prov: Option<Prov> },
    /// `const_make_global` of memory not from `const_allocate`.
    MakeGlobalNonHeap { addr: Addr, prov: Option<Prov> },
    /// `const_make_global` twice.
    MakeGlobalTwice(AllocId),
    /// Internal: the evaluator allocated over a live allocation.
    Overlap { addr: Addr, size: u64, with: AllocId, base: Addr, with_size: u64 },
    /// Internal: the evaluator allocated past the end of the address space.
    AddressOverflow { addr: Addr, size: u64 },
    /// Internal: more allocations than an [`AllocId`] counts.
    TooManyAllocations,
}

/// Miri's `Display for Pointer<Option<Provenance>>`: `null pointer`, `0x10[noalloc]`,
/// `0x1004[alloc12]<5>`, `0x1004[wildcard]`.
struct ShowPtr(Addr, Option<Prov>);

impl fmt::Display for ShowPtr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.1 {
            None if self.0 == 0 => f.write_str("null pointer"),
            None => write!(f, "{:#x}[noalloc]", self.0),
            Some(Prov::Concrete { alloc, tag }) => write!(f, "{:#x}[{alloc:?}]{tag:?}", self.0),
            Some(Prov::Wildcard) => write!(f, "{:#x}[wildcard]", self.0),
        }
    }
}

/// `1 byte` or `n bytes`.
struct ShowBytes(i64);

impl fmt::Display for ShowBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 == 1 { f.write_str("1 byte") } else { write!(f, "{} bytes", self.0) }
    }
}

// Miri: mir/interpret/error.rs, `fmt_in_alloc_attempt`.
fn in_alloc_attempt(f: &mut fmt::Formatter<'_>, msg: InAlloc, size: i64) -> fmt::Result {
    write!(f, "{msg}: ")?;
    match msg {
        InAlloc::MemoryAccess => write!(f, "attempting to access {}", ShowBytes(size)),
        InAlloc::InboundsPointerArithmetic => {
            write!(f, "attempting to offset pointer by {}", ShowBytes(size))
        }
        InAlloc::Dereferenceable if size == 0 => f.write_str("pointer must point to some allocation"),
        InAlloc::Dereferenceable => {
            write!(f, "pointer must be dereferenceable for {}", ShowBytes(size))
        }
    }
}

impl fmt::Display for MemError {
    // Miri: mir/interpret/error.rs, `Display for UndefinedBehaviorInfo`, and the messages of
    // interpret/memory.rs and const_eval/error.rs.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            MemError::OutOfBounds { alloc, alloc_size, ptr_offset, inbounds_size, msg } => {
                in_alloc_attempt(f, msg, inbounds_size)?;
                write!(f, ", but got {alloc:?}")?;
                if ptr_offset > 0 {
                    write!(f, "+{ptr_offset:#x}")?;
                } else if ptr_offset < 0 {
                    write!(f, "-{:#x}", ptr_offset.unsigned_abs())?;
                }
                f.write_str(" which ")?;
                if ptr_offset < 0 {
                    f.write_str("points to before the beginning of the allocation")
                } else if inbounds_size < 0 {
                    if ptr_offset == 0 {
                        f.write_str("is at the beginning of the allocation")
                    } else {
                        write!(f, "is only {ptr_offset} bytes from the beginning of the allocation")
                    }
                } else if ptr_offset as u64 >= alloc_size {
                    write!(
                        f,
                        "is at or beyond the end of the allocation of size {}",
                        ShowBytes(alloc_size as i64)
                    )
                } else {
                    let dist = alloc_size - ptr_offset as u64;
                    write!(f, "is only {} from the end of the allocation", ShowBytes(dist as i64))
                }
            }
            MemError::DanglingInt { addr, inbounds_size, msg } => {
                in_alloc_attempt(f, msg, inbounds_size)?;
                if addr == 0 {
                    f.write_str(", but got null pointer")
                } else {
                    write!(
                        f,
                        ", but got {} which is a dangling pointer (it has no provenance)",
                        ShowPtr(addr, None)
                    )
                }
            }
            MemError::UseAfterFree { alloc, msg } => {
                write!(f, "{msg}: {alloc:?} has been freed, so this pointer is dangling")
            }
            MemError::Misaligned { has, required, msg } => write!(
                f,
                "{} with alignment {has}, but alignment {required} is required",
                match msg {
                    AlignMsg::AccessedPtr => "accessing memory",
                    AlignMsg::BasedOn => "accessing memory based on pointer",
                }
            ),
            MemError::WriteToReadOnly(alloc) => write!(f, "writing to {alloc:?} which is read-only"),
            MemError::Uninit(None) => {
                f.write_str("using uninitialized data, but this operation requires initialized memory")
            }
            MemError::Uninit(Some(Uninit { alloc, access, bad })) => write!(
                f,
                "reading memory at {alloc:?}[{:#x}..{:#x}], but memory is uninitialized at \
                 [{:#x}..{:#x}], and this operation requires initialized memory",
                access.start,
                access.end(),
                bad.start,
                bad.end(),
            ),
            MemError::CopyOverlapping => f.write_str("`copy_nonoverlapping` called on overlapping ranges"),
            MemError::FreeNotStart { addr, prov } => write!(
                f,
                "deallocating {} which does not point to the beginning of an object",
                ShowPtr(addr, prov)
            ),
            MemError::FreeStatic(alloc) => write!(f, "deallocating {alloc:?}, which is static memory"),
            MemError::FreeImmutable(alloc) => write!(f, "deallocating immutable allocation {alloc:?}"),
            MemError::FreeWrongKind { alloc, has, used } => write!(
                f,
                "deallocating {alloc:?}, which is {has} memory, using {used} deallocation operation"
            ),
            MemError::FreeLayout { alloc, size, align, gave_size, gave_align } => write!(
                f,
                "incorrect layout on deallocation: {alloc:?} has size {size} and alignment {align}, \
                 but gave size {gave_size} and alignment {gave_align}"
            ),
            MemError::BadAlign(align) => {
                write!(f, "creating allocation with non-power-of-two alignment {align}")
            }
            MemError::MakeGlobalDangling { addr, prov } => write!(
                f,
                "pointer passed to `const_make_global` is dangling: {}",
                ShowPtr(addr, prov)
            ),
            MemError::MakeGlobalOffset { addr, prov } => write!(
                f,
                "making {} global which does not point to the beginning of an object",
                ShowPtr(addr, prov)
            ),
            MemError::MakeGlobalNonHeap { addr, prov } => write!(
                f,
                "pointer passed to `const_make_global` does not point to a heap allocation: {}",
                ShowPtr(addr, prov)
            ),
            MemError::MakeGlobalTwice(alloc) => write!(
                f,
                "attempting to call `const_make_global` twice on the same allocation {alloc:?}"
            ),
            MemError::Overlap { addr, size, with, base, with_size } => write!(
                f,
                "internal error: the evaluator allocated {size} bytes at {addr:#x}, over live \
                 {with:?} of {with_size} bytes at {base:#x}"
            ),
            MemError::AddressOverflow { addr, size } => write!(
                f,
                "internal error: the evaluator allocated {size} bytes at {addr:#x}, past the end \
                 of the address space"
            ),
            MemError::TooManyAllocations => {
                f.write_str("internal error: more allocations in one run than an allocation id counts")
            }
        }
    }
}

/// One allocation still live at the end of a run and reachable from no root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Leak {
    pub alloc: AllocId,
    pub kind: Kind,
    pub size: u64,
    pub align: u64,
}

/// Every leaked allocation, by id (the order they were made in, as Miri's).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Leaks {
    pub leaked: Vec<Leak>,
}

impl fmt::Display for Leaks {
    /// Miri's `report_leaks` titles, one per leak, the first without its `memory leaked: ` (which
    /// [`crate::Ub`]'s display adds).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, leak) in self.leaked.iter().enumerate() {
            if i > 0 {
                f.write_str("\nmemory leaked: ")?;
            }
            write!(
                f,
                "{:?} ({}, size: {}, align: {})",
                leak.alloc, leak.kind, leak.size, leak.align
            )?;
        }
        Ok(())
    }
}

/// The memory of one run. See the module docs.
#[derive(Clone, Debug)]
pub struct Memory {
    ptr_size: u64,
    /// By [`AllocId::index`]: hot facts.
    infos: Vec<Info>,
    /// By [`AllocId::index`]: init masks, provenance, exposed tags.
    shadows: Vec<Shadow>,
    /// Live allocations of at least one byte, by base address. They are disjoint.
    live: Vec<(Addr, AllocId)>,
    /// Live zero-sized allocations, by base address: several may share one (the evaluator puts
    /// each at its alignment), so they are kept apart from `live`.
    zsts: Vec<(Addr, AllocId)>,
    /// What the caller adds to the leak check's roots.
    roots: Vec<AllocId>,
    scratch_ptrs: Vec<(u64, Prov)>,
    scratch_frags: Vec<(u64, Frag)>,
    scratch_init: Vec<u64>,
}

impl Default for Memory {
    fn default() -> Memory {
        Memory::new()
    }
}

impl Memory {
    /// Memory of a 64-bit target.
    pub fn new() -> Memory {
        Memory::with_pointer_size(8)
    }

    /// Memory of a target whose pointers are `ptr_size` bytes.
    pub fn with_pointer_size(ptr_size: u64) -> Memory {
        assert!(matches!(ptr_size, 2 | 4 | 8), "a pointer of {ptr_size} bytes");
        Memory {
            ptr_size,
            infos: Vec::new(),
            shadows: Vec::new(),
            live: Vec::new(),
            zsts: Vec::new(),
            roots: Vec::new(),
            scratch_ptrs: Vec::new(),
            scratch_frags: Vec::new(),
            scratch_init: Vec::new(),
        }
    }

    pub fn pointer_size(&self) -> u64 {
        self.ptr_size
    }

    /// The facts of `alloc`, live or freed.
    #[inline]
    pub fn info(&self, alloc: AllocId) -> &Info {
        &self.infos[alloc.index()]
    }

    /// How many allocations the run has made.
    pub fn count(&self) -> usize {
        self.infos.len()
    }

    /// `v` as the target's `usize`.
    #[inline]
    fn truncate(&self, v: u64) -> u64 {
        if self.ptr_size == 8 { v } else { v & ((1u64 << (self.ptr_size * 8)) - 1) }
    }

    /// `v` as the target's `isize`.
    #[inline]
    fn sign_extend(&self, v: u64) -> i64 {
        let unused = 64 - self.ptr_size * 8;
        ((v << unused) as i64) >> unused
    }

    /// A new allocation at `addr`. The evaluator picks addresses and reuses those of freed
    /// blocks; two live allocations of at least one byte never overlap.
    pub fn alloc(
        &mut self,
        addr: Addr,
        size: u64,
        align: u64,
        kind: Kind,
        zeroed: bool,
    ) -> Result<AllocId, MemError> {
        if !align.is_power_of_two() {
            return Err(MemError::BadAlign(align));
        }
        let n = u32::try_from(self.infos.len() + 1).map_err(|_| MemError::TooManyAllocations)?;
        let id = AllocId::new(n).ok_or(MemError::TooManyAllocations)?;
        if size == 0 {
            let at = if self.zsts.last().is_none_or(|&(b, _)| b <= addr) {
                self.zsts.len()
            } else {
                self.zsts.partition_point(|&(b, _)| b <= addr)
            };
            self.zsts.insert(at, (addr, id));
        } else {
            let end = addr.checked_add(size).ok_or(MemError::AddressOverflow { addr, size })?;
            let at = if self.live.last().is_none_or(|&(b, _)| b < addr) {
                self.live.len()
            } else {
                self.live.partition_point(|&(b, _)| b < end)
            };
            if let Some(&(base, with)) = at.checked_sub(1).map(|i| &self.live[i]) {
                let with_size = self.infos[with.index()].size;
                if base + with_size > addr {
                    return Err(MemError::Overlap { addr, size, with, base, with_size });
                }
            }
            self.live.insert(at, (addr, id));
        }
        self.infos.push(Info {
            base: addr,
            size,
            align,
            kind,
            live: true,
            mutable: true,
            exposed: false,
            made_global: false,
        });
        self.shadows.push(Shadow::new(size, zeroed));
        Ok(id)
    }

    /// Frees the allocation `ptr` points to the start of, by an operation of `kind` that says
    /// the size and alignment it was made with when it knows them.
    // Miri: interpret/memory.rs, `deallocate_ptr`; alloc_addresses/mod.rs, `free_alloc_id`.
    pub fn free(
        &mut self,
        ptr: (Addr, Option<Prov>),
        size: Option<u64>,
        align: Option<u64>,
        kind: Kind,
    ) -> Result<AllocId, MemError> {
        let (addr, prov) = ptr;
        let (alloc, off) = self.get_alloc(addr, prov, 0).ok_or(MemError::DanglingInt {
            addr,
            inbounds_size: 0,
            msg: InAlloc::Dereferenceable,
        })?;
        if off != 0 {
            return Err(MemError::FreeNotStart { addr, prov });
        }
        let info = self.infos[alloc.index()];
        if !info.live {
            return Err(MemError::UseAfterFree { alloc, msg: InAlloc::MemoryAccess });
        }
        if info.kind == Kind::Global {
            return Err(MemError::FreeStatic(alloc));
        }
        if !info.mutable {
            return Err(MemError::FreeImmutable(alloc));
        }
        if info.kind != kind {
            return Err(MemError::FreeWrongKind { alloc, has: info.kind, used: kind });
        }
        if size.is_some() || align.is_some() {
            let gave_size = size.unwrap_or(info.size);
            let gave_align = align.unwrap_or(info.align);
            if gave_size != info.size || gave_align != info.align {
                return Err(MemError::FreeLayout {
                    alloc,
                    size: info.size,
                    align: info.align,
                    gave_size,
                    gave_align,
                });
            }
        }
        let map = if info.size == 0 { &mut self.zsts } else { &mut self.live };
        let pos = map
            .binary_search(&(info.base, alloc))
            .expect("a live allocation is in the address map");
        map.remove(pos);
        let info = &mut self.infos[alloc.index()];
        info.live = false;
        info.exposed = false;
        self.shadows[alloc.index()] = Shadow::new(0, false);
        Ok(alloc)
    }

    /// The live allocation of at least one byte that holds `addr`.
    #[inline]
    fn sized_at(&self, addr: Addr) -> Option<AllocId> {
        let pos = self.live.partition_point(|&(b, _)| b <= addr);
        let &(base, id) = self.live.get(pos.checked_sub(1)?)?;
        (addr - base < self.infos[id.index()].size).then_some(id)
    }

    /// The live zero-sized allocations at `addr`, oldest first.
    #[inline]
    fn zsts_at(&self, addr: Addr) -> &[(Addr, AllocId)] {
        let lo = self.zsts.partition_point(|&(b, _)| b < addr);
        let hi = lo + self.zsts[lo..].partition_point(|&(b, _)| b <= addr);
        &self.zsts[lo..hi]
    }

    /// The live allocation `addr` is in, exposed or not: the root of a pointer the evaluator
    /// makes itself into memory it laid out.
    pub fn live_at(&self, addr: Addr) -> Option<AllocId> {
        self.sized_at(addr).or_else(|| self.zsts_at(addr).last().map(|&(_, id)| id))
    }

    /// The exposed live allocation a wildcard pointer at `addr` uses for an access of `size`
    /// bytes (negative: to its left).
    // Miri: alloc_addresses/mod.rs, `alloc_id_from_addr`: the allocation `addr` is strictly in,
    // or one starting at it. Zero-sized ones sharing an address are tried, newest first, after
    // the sized one that holds it.
    fn alloc_from_addr(&self, addr: Addr, size: i64) -> Option<AllocId> {
        let addr = if size >= 0 { addr } else { addr.saturating_sub(1) };
        if let Some(id) = self.sized_at(addr)
            && self.infos[id.index()].exposed
        {
            return Some(id);
        }
        self.zsts_at(addr).iter().rev().map(|&(_, id)| id).find(|id| self.infos[id.index()].exposed)
    }

    /// The exposed allocation an integer cast to a pointer at `addr` would use.
    pub fn exposed_at(&self, addr: Addr) -> Option<AllocId> {
        self.alloc_from_addr(addr, 0)
    }

    /// The allocation and offset a pointer names, or `None` for one with no provenance or a
    /// wildcard at no exposed allocation.
    // Miri: interpret/memory.rs, `ptr_try_get_alloc_id`; alloc_addresses/mod.rs, `ptr_get_alloc`.
    #[inline]
    fn get_alloc(&self, addr: Addr, prov: Option<Prov>, size: i64) -> Option<(AllocId, u64)> {
        let alloc = match prov? {
            Prov::Concrete { alloc, .. } => alloc,
            Prov::Wildcard => self.alloc_from_addr(addr, size)?,
        };
        let base = self.infos[alloc.index()].base;
        Some((alloc, self.truncate(addr.wrapping_sub(base))))
    }

    /// Whether `size` bytes from `offset` (to its left when negative) are in an allocation of
    /// `alloc_size`.
    // Miri: interpret/memory.rs, `check_and_deref_ptr`.
    #[inline]
    fn check_bounds(
        &self,
        alloc: AllocId,
        alloc_size: u64,
        offset: u64,
        size: i64,
        msg: InAlloc,
    ) -> Result<(), MemError> {
        let (begin, end) = if size >= 0 {
            (Some(offset), offset.checked_add(size as u64))
        } else {
            (offset.checked_sub(size.unsigned_abs()), Some(offset))
        };
        if begin.is_some() && end.is_some_and(|e| e <= alloc_size) {
            Ok(())
        } else {
            Err(MemError::OutOfBounds {
                alloc,
                alloc_size,
                ptr_offset: self.sign_extend(offset),
                inbounds_size: size,
                msg,
            })
        }
    }

    /// Checks `addr` against `align`, by address (Miri without symbolic alignment).
    // Miri: interpret/memory.rs, `is_ptr_misaligned` with `OFFSET_IS_ADDR`.
    #[inline]
    pub fn check_align(&self, addr: Addr, align: u64, msg: AlignMsg) -> Result<(), MemError> {
        if align <= 1 || addr & (align - 1) == 0 {
            return Ok(());
        }
        Err(MemError::Misaligned { has: 1 << addr.trailing_zeros(), required: align, msg })
    }

    /// Checks an access of `size` bytes at `addr` of a typed place (`*p`, `(*p).f`) and names
    /// the allocation and offset it touches; `None` for a zero-sized access, which needs none.
    pub fn access(
        &self,
        addr: Addr,
        prov: Option<Prov>,
        size: u64,
        align: u64,
        kind: AccessKind,
    ) -> Result<Option<(AllocId, u64)>, MemError> {
        self.access_as(addr, prov, size, align, kind, AlignMsg::BasedOn)
    }

    /// [`Memory::access`], naming the pointer in an alignment error as `msg` says.
    // Miri: interpret/memory.rs, `get_ptr_alloc` and `get_ptr_alloc_mut` (liveness, then
    // read-only memory for a write, then bounds), then the alignment check the place does after
    // them (interpret/place.rs, `get_place_alloc`).
    pub fn access_as(
        &self,
        addr: Addr,
        prov: Option<Prov>,
        size: u64,
        align: u64,
        kind: AccessKind,
        msg: AlignMsg,
    ) -> Result<Option<(AllocId, u64)>, MemError> {
        let found = if size == 0 {
            None
        } else {
            let size = i64::try_from(size).unwrap_or(i64::MAX);
            let (alloc, off) = self.get_alloc(addr, prov, size).ok_or(MemError::DanglingInt {
                addr,
                inbounds_size: size,
                msg: InAlloc::MemoryAccess,
            })?;
            let info = &self.infos[alloc.index()];
            if !info.live {
                return Err(MemError::UseAfterFree { alloc, msg: InAlloc::MemoryAccess });
            }
            if kind == AccessKind::Write && !info.mutable {
                return Err(MemError::WriteToReadOnly(alloc));
            }
            self.check_bounds(alloc, info.size, off, size, InAlloc::MemoryAccess)?;
            Some((alloc, off))
        };
        self.check_align(addr, align, msg)?;
        Ok(found)
    }

    /// Checks that offsetting `addr` by `by` bytes stays in its allocation, as an inbounds GEP
    /// requires.
    // Miri: intrinsics.rs, `ptr_offset_inbounds`; interpret/memory.rs, `check_ptr_access_signed`.
    pub fn offset_inbounds(&self, addr: Addr, prov: Option<Prov>, by: i64) -> Result<(), MemError> {
        if by == 0 {
            return Ok(());
        }
        let msg = InAlloc::InboundsPointerArithmetic;
        let (alloc, off) = self
            .get_alloc(addr, prov, by)
            .ok_or(MemError::DanglingInt { addr, inbounds_size: by, msg })?;
        let info = &self.infos[alloc.index()];
        if !info.live {
            return Err(MemError::UseAfterFree { alloc, msg });
        }
        self.check_bounds(alloc, info.size, off, by, msg)
    }

    /// Makes `alloc` read-only: a constant, a string literal, a promoted temporary.
    pub fn set_immutable(&mut self, alloc: AllocId) {
        self.infos[alloc.index()].mutable = false;
    }

    /// `const_make_global`: makes the const heap allocation `ptr` points to the start of an
    /// immutable global, which may then not leak.
    // Miri: interpret/memory.rs, `make_const_heap_ptr_global`.
    pub fn make_global(&mut self, ptr: (Addr, Option<Prov>)) -> Result<AllocId, MemError> {
        let (addr, prov) = ptr;
        let (alloc, off) = self.get_alloc(addr, prov, 0).ok_or(MemError::DanglingInt {
            addr,
            inbounds_size: 0,
            msg: InAlloc::Dereferenceable,
        })?;
        if off != 0 {
            return Err(MemError::MakeGlobalOffset { addr, prov });
        }
        let info = &mut self.infos[alloc.index()];
        if info.kind == Kind::Global {
            return Err(MemError::MakeGlobalNonHeap { addr, prov });
        }
        if !info.live {
            return Err(MemError::MakeGlobalDangling { addr, prov });
        }
        if info.kind != Kind::ConstHeap {
            return Err(MemError::MakeGlobalNonHeap { addr, prov });
        }
        if info.made_global {
            return Err(MemError::MakeGlobalTwice(alloc));
        }
        info.made_global = true;
        info.mutable = false;
        Ok(alloc)
    }

    /// The init mask and provenance of `span` of `alloc` (live, and `span` in bounds).
    pub fn read(&self, alloc: AllocId, span: Span) -> Bytes {
        let shadow = &self.shadows[alloc.index()];
        debug_assert!(span.end() <= shadow.init.len());
        let len = span.len as usize;
        let mut init = SmallVec::new();
        match shadow.init.lazy_state() {
            Some(true) => {}
            Some(false) => init.resize(words_for(len), 0),
            None => {
                init.resize(words_for(len), 0);
                shadow.init.bits(span.range(), &mut init);
            }
        }
        let prov = self.runs(&shadow.prov, span);
        Bytes::from_parts(len, init, prov, Some((alloc, span.start)))
    }

    /// The provenance of `span` as runs from its start.
    fn runs(&self, prov: &ProvMap, span: Span) -> SmallVec<[Run; 1]> {
        let mut runs: SmallVec<[Run; 1]> = SmallVec::new();
        if span.is_empty() {
            return runs;
        }
        let ps = self.ptr_size;
        let start = span.start;
        let end = span.end();
        let mut ptrs = prov.range_ptrs(start..end, ps).iter().peekable();
        let mut frags = prov.range_bytes(start..end).iter().peekable();
        let mut push = |run: Run| match runs.last_mut() {
            Some(last) if last.joins(&run) => last.len += run.len,
            _ => runs.push(run),
        };
        loop {
            let take_ptr = match (ptrs.peek(), frags.peek()) {
                (Some(p), Some(b)) => p.0 < b.0,
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (None, None) => break,
            };
            if take_ptr {
                let &(pos, p) = ptrs.next().expect("peeked");
                let a = pos.max(start);
                let b = (pos + ps).min(end);
                push(Run {
                    at: offset((a - start) as usize),
                    idx: (a - pos) as u8,
                    len: (b - a) as u8,
                    whole: a == pos && b == pos + ps,
                    prov: p,
                });
            } else {
                let &(pos, frag) = frags.next().expect("peeked");
                push(Run {
                    at: offset((pos - start) as usize),
                    idx: frag.idx,
                    len: 1,
                    whole: false,
                    prov: frag.prov,
                });
            }
        }
        runs
    }

    /// Writes the shadow of a value to `alloc` at `at` (live, and in bounds).
    // Miri: allocation.rs, `write_scalar` (clear the provenance of the range, mark it
    // initialized, insert the pointer's), with the init bits of a value that is partly
    // initialized written as they are.
    pub fn write(&mut self, alloc: AllocId, at: u64, bytes: &Bytes) {
        let len = bytes.len() as u64;
        if len == 0 {
            return;
        }
        let ps = self.ptr_size;
        let mut ptrs = std::mem::take(&mut self.scratch_ptrs);
        let mut frags = std::mem::take(&mut self.scratch_frags);
        let shadow = &mut self.shadows[alloc.index()];
        debug_assert!(at + len <= shadow.init.len());
        if bytes.init_bits().is_empty() {
            shadow.init.set_range(at..at + len, true);
        } else if bytes.all_uninit() {
            shadow.init.set_range(at..at + len, false);
        } else {
            shadow.init.set_bits(at, bytes.init_bits(), len);
        }
        shadow.prov.clear(at..at + len, ps);
        if !bytes.runs().is_empty() {
            ptrs.clear();
            frags.clear();
            for run in bytes.runs() {
                let pos = at + u64::from(run.at);
                if run.whole && u64::from(run.len) == ps {
                    ptrs.push((pos, run.prov));
                } else {
                    frags.extend((0..run.len).map(|k| {
                        (pos + u64::from(k), Frag { idx: run.idx + k, prov: run.prov })
                    }));
                }
            }
            shadow.prov.insert_ptrs_presorted(&ptrs);
            shadow.prov.insert_bytes_presorted(&frags);
        }
        self.scratch_ptrs = ptrs;
        self.scratch_frags = frags;
    }

    /// Copies the shadow of `len` bytes from `src` to `dst` (both live and in bounds); the
    /// ranges may overlap.
    // Miri: interpret/memory.rs, `mem_copy_repeatedly` with one copy: the source's provenance
    // and init mask are taken first, then the destination is cleared and they are put there.
    pub fn copy(&mut self, dst: (AllocId, u64), src: (AllocId, u64), len: u64) {
        if len == 0 {
            return;
        }
        let ps = self.ptr_size;
        let mut ptrs = std::mem::take(&mut self.scratch_ptrs);
        let mut frags = std::mem::take(&mut self.scratch_frags);
        let mut ranges = std::mem::take(&mut self.scratch_init);
        let source = &self.shadows[src.0.index()];
        source.prov.prepare_copy(src.1..src.1 + len, ps, &mut ptrs, &mut frags);
        let initial = source.init.prepare_copy(src.1..src.1 + len, &mut ranges);
        let dest = &mut self.shadows[dst.0.index()];
        let range = dst.1..dst.1 + len;
        dest.init.set_range(range.clone(), true);
        dest.prov.clear(range.clone(), ps);
        if !initial && ranges.len() == 1 {
            dest.init.set_range(range, false);
        } else {
            dest.init.apply_copy(initial, &ranges, dst.1, len, 1);
            dest.prov.apply_copy(&ptrs, &frags, dst.1, len, 1);
        }
        self.scratch_ptrs = ptrs;
        self.scratch_frags = frags;
        self.scratch_init = ranges;
    }

    /// `copy_nonoverlapping`'s check, after the bounds checks of both sides.
    // Miri: interpret/memory.rs, `mem_copy_repeatedly`: a copy of no initialized byte returns
    // before the overlap check.
    pub fn check_nonoverlapping(
        &self,
        dst: (AllocId, u64),
        src: (AllocId, u64),
        len: u64,
    ) -> Result<(), MemError> {
        if len == 0 || dst.0 != src.0 {
            return Ok(());
        }
        let init = &self.shadows[src.0.index()].init;
        if init.chunks(src.1..src.1 + len).all(|c| !c.is_init()) {
            return Ok(());
        }
        let (s, d) = (src.1, dst.1);
        if (s <= d && s + len > d) || (d <= s && d + len > s) {
            return Err(MemError::CopyOverlapping);
        }
        Ok(())
    }

    /// Makes `span` of `alloc` uninitialized and without provenance (padding reset).
    // Miri: allocation.rs, `write_uninit`.
    pub fn uninit(&mut self, alloc: AllocId, span: Span) {
        let ps = self.ptr_size;
        let shadow = &mut self.shadows[alloc.index()];
        shadow.init.set_range(span.range(), false);
        shadow.prov.clear(span.range(), ps);
    }

    /// Removes the provenance of `span` of `alloc` (bytes of a value that are not a pointer).
    // Miri: allocation.rs, `clear_provenance`.
    pub fn clear_prov(&mut self, alloc: AllocId, span: Span) {
        let ps = self.ptr_size;
        self.shadows[alloc.index()].prov.clear(span.range(), ps);
    }

    /// Fails unless every byte of `span` of `alloc` is initialized.
    // Miri: allocation.rs, `read_scalar`'s first check.
    pub fn require_init(&self, alloc: AllocId, span: Span) -> Result<(), MemError> {
        self.shadows[alloc.index()].init.is_range_initialized(span.range()).map_err(|bad| {
            MemError::Uninit(Some(Uninit {
                alloc,
                access: span,
                bad: Span::new(bad.start, bad.end - bad.start),
            }))
        })
    }

    /// The provenance of the pointer at `at` of `alloc`, whole or put back together from
    /// fragments.
    // Miri: provenance_map.rs, `read_ptr`.
    pub fn prov_at(&self, alloc: AllocId, at: u64) -> Option<Prov> {
        self.shadows[alloc.index()].prov.read_ptr(at, self.ptr_size)
    }

    /// Puts `prov` on the pointer of `size` bytes at `at` of `alloc`: a retag rewriting a
    /// pointer's tag in place.
    // Miri: allocation.rs, `write_scalar` of the retagged pointer.
    pub fn set_prov(&mut self, alloc: AllocId, at: u64, size: u64, prov: Prov) {
        let ps = self.ptr_size;
        let shadow = &mut self.shadows[alloc.index()];
        shadow.init.set_range(at..at + size, true);
        shadow.prov.clear(at..at + size, ps);
        if size == ps {
            shadow.prov.insert_ptr(at, prov, ps);
        } else {
            let mut frags = std::mem::take(&mut self.scratch_frags);
            frags.clear();
            frags.extend((0..size).map(|k| (at + k, Frag { idx: k as u8, prov })));
            self.shadows[alloc.index()].prov.insert_bytes_presorted(&frags);
            self.scratch_frags = frags;
        }
    }

    /// Exposes `tag` of `alloc`: a pointer to it was cast to an integer. A freed allocation is
    /// not exposed, since no integer can reach it.
    // Miri: alloc_addresses/mod.rs, `expose_provenance`.
    pub fn expose(&mut self, alloc: AllocId, tag: BorTag) {
        let info = &mut self.infos[alloc.index()];
        if !info.live {
            return;
        }
        info.exposed = true;
        let tags = &mut self.shadows[alloc.index()].exposed;
        if !tags.contains(&tag) {
            tags.push(tag);
        }
    }

    /// The tags exposed from `alloc` while it was live.
    pub fn exposed_tags(&self, alloc: AllocId) -> &[BorTag] {
        &self.shadows[alloc.index()].exposed
    }

    /// Adds `alloc` to the roots of the leak check (a static the evaluator registers, a
    /// thread-local's memory).
    pub fn add_root(&mut self, alloc: AllocId) {
        self.roots.push(alloc);
    }

    /// The leak check: memory that may not leak, still live, and reachable through provenance
    /// from no live global or machine allocation and no added root.
    // Miri: interpret/memory.rs, `take_leaked_allocations`; diagnostics.rs, `report_leaks`.
    pub fn leaks(&self) -> Result<(), Leaks> {
        let n = self.infos.len();
        let mut reachable = BitSet::<usize>::new(n);
        let mut todo: Vec<AllocId> = self
            .live
            .iter()
            .chain(&self.zsts)
            .map(|&(_, id)| id)
            .filter(|id| matches!(self.infos[id.index()].kind, Kind::Global | Kind::Machine))
            .chain(self.roots.iter().copied())
            .collect();
        while let Some(id) = todo.pop() {
            let i = id.index();
            if !reachable.insert(i) {
                continue;
            }
            if self.infos[i].live {
                todo.extend(self.shadows[i].prov.provenances().filter_map(|p| match p {
                    Prov::Concrete { alloc, .. } => Some(alloc),
                    Prov::Wildcard => None,
                }));
            }
        }
        let leaked: Vec<Leak> = self
            .infos
            .iter()
            .enumerate()
            .filter(|&(i, info)| {
                info.live
                    && !info.kind.may_leak(info.made_global)
                    && !reachable.contains(i)
            })
            .map(|(i, info)| Leak {
                alloc: AllocId::new(i as u32 + 1).expect("an index of the column"),
                kind: info.kind,
                size: info.size,
                align: info.align,
            })
            .collect();
        if leaked.is_empty() { Ok(()) } else { Err(Leaks { leaked }) }
    }

    /// Every tag memory holds, with the allocation it is a tag of: the pointers stored in live
    /// allocations and the tags exposed from them. Tree Borrows keeps these when it collects
    /// unreachable tags.
    // Miri: provenance_gc.rs, `VisitProvenance for Allocation` (its provenance map).
    pub fn tags(&self, out: &mut impl FnMut(AllocId, BorTag)) {
        for &(_, id) in self.live.iter().chain(&self.zsts) {
            let shadow = &self.shadows[id.index()];
            for prov in shadow.prov.provenances() {
                if let Prov::Concrete { alloc, tag } = prov {
                    out(alloc, tag);
                }
            }
            for &tag in &shadow.exposed {
                out(id, tag);
            }
        }
    }

    /// Whether `span` of `alloc` holds any provenance.
    pub fn has_prov(&self, alloc: AllocId, span: Span) -> bool {
        !self.shadows[alloc.index()].prov.range_empty(span.range(), self.ptr_size)
    }

    /// Whether `alloc` holds any provenance at all.
    pub fn holds_pointers(&self, alloc: AllocId) -> bool {
        !self.shadows[alloc.index()].prov.is_empty()
    }
}
