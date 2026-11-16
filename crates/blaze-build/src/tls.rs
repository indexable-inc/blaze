//! Thread-locals (`#[thread_local] static`): one block per thread holding every thread-local
//! of the program, made on a thread's first access from one template, found through one POSIX
//! thread-specific key.
//!
//! # The scheme, and why
//!
//! - The program's thread-locals are laid out into one block ([`template`]): each at its
//!   alignment, in item-key order. The template is a static of the program (placed with the
//!   other statics, [`crate::data::StaticImage`]); its relocations are filled once at load, so
//!   every thread's copy starts with the same absolute addresses. A thread-local's initialiser
//!   cannot point at a thread-local (Rust refuses to take one's address in a constant), so
//!   nothing in a block points into a block.
//! - A descriptor static ([`descriptor`]) holds the key, the block's size and alignment and the
//!   template's address: `{ key: u64, size: u64, align: u64, template: *const u8 }`. Load
//!   creates the key (`pthread_key_create(&desc.key, free)`) before any code runs.
//! - An access is LIR's `tls_addr` of the thread-local, which the backend lowers (blaze-emit's
//!   `pass/tls.rs`). On macOS it reads the key's slot directly: `mrs tpidrro_el0`, clear the low
//!   bits, and one load at `key * 8`, with no call. The getter runs only when that slot is empty.
//!   Elsewhere every access calls the getter ([`getter`], a LIR function built here). The getter
//!   does `pthread_getspecific`; on a thread's first access it also does `aligned_alloc` of the
//!   block, `memcpy` of the template and `pthread_setspecific`. A thread's exit frees its block
//!   (the key's destructor).
//!
//! Why: code blaze places is not a dyld image or an ELF module, so neither Mach-O thread-local
//! variables (`__thread_vars`, set up by dyld per image) nor ELF TLS (`tpidr_el0` offsets the
//! static linker fixes) can reach it; a thread-specific key is the one mechanism both systems
//! offer to code loaded at run time. It is also what Mach-O's own thread-locals cost: every
//! access there is a call through the variable's descriptor to `tlv_get_addr`, which reads
//! the thread's specific-data slots. The macOS form above is that floor with no call. It relies
//! on the private layout `_pthread_getspecific_direct` reads (rule 45), and blaze-emit's
//! `tls_tests.rs` pins that the direct read equals `pthread_getspecific`.
//! - The block is allocated lazily, so a thread that touches no thread-local allocates nothing,
//!   and a program with no thread-locals has no key ([`template`] gives `None`).

use core::alloc::Allocator;

use blaze_lir::builder::{MemAttrs, Param, Variadic};
use blaze_lir::func::{BlockKind, Conv, Func, RefKind};
use blaze_lir::{BuildError, Builder, Flags, Full, IntCond, Ty};
use blaze_typeck::ItemKey;

use crate::data::{DataAlloc, DataError, DataRecord, DataReloc, DataTarget, MOST_ALIGN};

/// The descriptor's fields: byte offsets, and its size.
pub const DESC_KEY: u32 = 0;
pub const DESC_SIZE: u32 = 8;
pub const DESC_ALIGN: u32 = 16;
pub const DESC_TEMPLATE: u32 = 24;
pub const DESC_BYTES: usize = 32;

/// The getter's references, by place: each an outside function called directly.
pub const IMPORTS: [&str; 4] = ["pthread_getspecific", "aligned_alloc", "memcpy", "pthread_setspecific"];

/// The trap code of a failed block allocation.
pub const NO_MEMORY_TRAP: u32 = 0xfe01;

/// Where each thread-local is in the block.
pub struct TlsLayout<A: Allocator> {
    /// The block's size, a multiple of its alignment (as `aligned_alloc` asks).
    pub size: u64,
    /// log2 of the alignment.
    pub align: u8,
    /// Each thread-local's offset in the block, sorted by key.
    pub statics: Vec<(ItemKey, u32), A>,
}

impl<A: Allocator> TlsLayout<A> {
    pub fn offset(&self, key: ItemKey) -> Option<u32> {
        self.statics.binary_search_by_key(&key, |r| r.0).ok().map(|k| self.statics[k].1)
    }
}

/// A thread-local's initialiser that points into its own value: one thread's block would
/// point into another's.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SelfPointer {
    pub static_: ItemKey,
}

/// Why no template.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TlsError {
    Data(DataError),
    SelfPointer(SelfPointer),
}

impl core::fmt::Display for TlsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            TlsError::Data(e) => write!(f, "a thread-local's initialiser: {e}"),
            TlsError::SelfPointer(s) => write!(f, "thread-local {:#x} points into its own value", s.static_.0),
        }
    }
}

impl core::error::Error for TlsError {}

impl From<DataError> for TlsError {
    fn from(e: DataError) -> TlsError {
        TlsError::Data(e)
    }
}

/// The template of the program's thread-locals (each its static item and record) and where
/// each is; `None` when there are none.
pub fn template<A: Allocator + Clone, B: Allocator>(
    tls: &[(ItemKey, &DataRecord<B>)],
    alloc: A,
) -> Result<Option<(DataRecord<A>, TlsLayout<A>)>, TlsError> {
    if tls.is_empty() {
        return Ok(None);
    }
    let mut order: Vec<usize, A> = Vec::with_capacity_in(tls.len(), alloc.clone());
    order.extend(0..tls.len());
    order.sort_by_key(|&i| tls[i].0);
    for w in order.windows(2) {
        if tls[w[0]].0 == tls[w[1]].0 {
            return Err(DataError::Twice { static_: tls[w[0]].0 }.into());
        }
    }
    let mut statics = Vec::with_capacity_in(tls.len(), alloc.clone());
    let mut end = 0u64;
    // At least a pointer's alignment: macOS's `aligned_alloc` refuses less (EINVAL).
    let mut align = 3u8;
    for &i in order.iter() {
        let (key, record) = tls[i];
        record.check()?;
        let root = &record.allocs[0];
        let a = 1u64 << root.align;
        let at = end.div_ceil(a) * a;
        end = at + root.bytes.len() as u64;
        if end > u64::from(u32::MAX) {
            return Err(DataError::TooLarge.into());
        }
        align = align.max(root.align);
        statics.push((key, at as u32));
    }
    let a = 1u64 << align;
    let size = end.max(1).div_ceil(a) * a;
    // The block is allocation 0; each record's inner allocations follow, renumbered.
    let mut block = Vec::with_capacity_in(size as usize, alloc.clone());
    block.resize(size as usize, 0);
    let mut block_relocs = Vec::new_in(alloc.clone());
    let mut allocs = Vec::new_in(alloc.clone());
    allocs.push(DataAlloc { bytes: Vec::new_in(alloc.clone()), align, mutable: true, relocs: Vec::new_in(alloc.clone()) });
    for (n, &i) in order.iter().enumerate() {
        let (key, record) = tls[i];
        let at = statics[n].1;
        // Allocation k > 0 of this record is allocation `base + k - 1` of the template.
        let base = allocs.len() as u32;
        let target = |t: DataTarget| match t {
            DataTarget::Local(0) => Err(TlsError::SelfPointer(SelfPointer { static_: key })),
            DataTarget::Local(k) => Ok(DataTarget::Local(base + k - 1)),
            other => Ok(other),
        };
        let root = &record.allocs[0];
        block[at as usize..at as usize + root.bytes.len()].copy_from_slice(&root.bytes);
        for r in root.relocs.iter() {
            block_relocs.push(DataReloc { offset: at + r.offset, target: target(r.target)? });
        }
        for inner in record.allocs[1..].iter() {
            let mut bytes = Vec::with_capacity_in(inner.bytes.len(), alloc.clone());
            bytes.extend_from_slice(&inner.bytes);
            let mut relocs = Vec::with_capacity_in(inner.relocs.len(), alloc.clone());
            for r in inner.relocs.iter() {
                relocs.push(DataReloc { offset: r.offset, target: target(r.target)? });
            }
            allocs.push(DataAlloc { bytes, align: inner.align, mutable: inner.mutable, relocs });
        }
    }
    allocs[0].bytes = block;
    allocs[0].relocs = block_relocs;
    let record = DataRecord { allocs };
    record.check()?;
    debug_assert!(align <= MOST_ALIGN);
    Ok(Some((record, TlsLayout { size, align, statics })))
}

/// The descriptor static of a program whose template is static `template`.
pub fn descriptor<A: Allocator + Clone, B: Allocator>(template: ItemKey, layout: &TlsLayout<B>, alloc: A) -> DataRecord<A> {
    let mut bytes = Vec::with_capacity_in(DESC_BYTES, alloc.clone());
    bytes.resize(DESC_BYTES, 0);
    bytes[DESC_SIZE as usize..][..8].copy_from_slice(&layout.size.to_le_bytes());
    bytes[DESC_ALIGN as usize..][..8].copy_from_slice(&(1u64 << layout.align).to_le_bytes());
    let mut relocs = Vec::with_capacity_in(1, alloc.clone());
    relocs.push(DataReloc { offset: DESC_TEMPLATE, target: DataTarget::Static(template) });
    let mut allocs = Vec::with_capacity_in(1, alloc);
    allocs.push(DataAlloc { bytes, align: 3, mutable: true, relocs });
    DataRecord { allocs }
}

/// The getter: `fn(desc: *mut Desc) -> *mut u8` (C convention), the calling thread's block.
pub fn getter<A: Allocator + Clone>(alloc: A) -> Result<Func<A>, BuildError> {
    let mut b = Builder::<A, Full>::new_in(64, alloc);
    let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR)], &[Param::of(Ty::PTR)]);
    b.set_signature(sig);
    let ptr_of_i64 = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::PTR)]);
    let alloc_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64), Param::of(Ty::I64)], &[Param::of(Ty::PTR)]);
    let copy_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR), Param::of(Ty::PTR), Param::of(Ty::I64)], &[Param::of(Ty::PTR)]);
    let set_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64), Param::of(Ty::PTR)], &[Param::of(Ty::I32)]);
    let [get, aligned_alloc, memcpy, set] = [0, 1, 2, 3].map(|p| b.reference(RefKind::Func, p));
    let (entry, first, failed, done) = (
        b.create_block(BlockKind::Normal),
        b.create_block(BlockKind::Cold),
        b.create_block(BlockKind::Cold),
        b.create_block(BlockKind::Normal),
    );
    b.begin(entry);
    let desc = b.param(Ty::PTR);
    let at8 = MemAttrs::aligned(3);
    let key = b.load(Ty::I64, Flags::NONE, desc, DESC_KEY as i32, at8);
    let block = b.call(Flags::NONE, get, ptr_of_i64, &[key]);
    let null = b.null();
    let none = b.icmp(IntCond::Eq, block, null);
    b.br(Flags::EXPECT_FALSE, none, (first, &[]), (done, &[block]));
    b.begin(first);
    let size = b.load(Ty::I64, Flags::NONE, desc, DESC_SIZE as i32, at8);
    let align = b.load(Ty::I64, Flags::NONE, desc, DESC_ALIGN as i32, at8);
    let template = b.load(Ty::PTR, Flags::NONE, desc, DESC_TEMPLATE as i32, at8);
    let fresh = b.call(Flags::NONE, aligned_alloc, alloc_sig, &[align, size]);
    let out = b.icmp(IntCond::Eq, fresh, null);
    let after = b.create_block(BlockKind::Normal);
    b.br(Flags::EXPECT_FALSE, out, (failed, &[]), (after, &[]));
    b.begin(failed);
    b.trap(NO_MEMORY_TRAP);
    b.begin(after);
    b.call(Flags::NONE, memcpy, copy_sig, &[fresh, template, size]);
    b.call(Flags::NONE, set, set_sig, &[key, fresh]);
    b.jump((done, &[fresh]));
    b.begin(done);
    let found = b.param(Ty::PTR);
    b.ret(&[found]);
    b.finish_checked()
}

#[cfg(test)]
mod tests;
