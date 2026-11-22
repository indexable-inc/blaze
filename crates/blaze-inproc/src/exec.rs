//! In-process JIT execution primitives:
//!
//! - [`Executable`]: one-shot private code + writable data written once and sealed RX.
//! - [`JitPool`]: a dual-mapped W^X in-process JIT slab backed by a single [`sys::Object`] mapped
//!   once `Access::Shared` (RW) and once `Access::Execute` (RX) — never RWX and never `MAP_JIT` —
//!   with sub-page TLSF allocation ([`crate::alloc::CodeHeap`], 16-byte grain), multi-function
//!   relocation batch linking ([`JitPool::link_batch`]), and atomic hot-patch tier-up trampolines
//!   ([`TierSlot`], [`JitPool::retarget`]).

use std::ops::Range;
use std::sync::atomic::Ordering;

use crate::alloc::{self, CodeHeap};
pub use crate::link::{LinkUnit, LinkedBatch, Reloc, RelocKind, RelocTarget, apply_reloc};
use crate::{Access, CodeRange, Error, sys};

#[cfg(test)]
mod tests;

pub struct Executable {
    memory: sys::Exec,
}

impl Executable {
    /// `code`, executable, and `data_len` zeroed writable bytes after it.
    pub fn new(code: &[u8], data_len: usize) -> Result<Executable, Error> {
        Executable::written(code.len(), data_len, |out, _| out.copy_from_slice(code))
    }

    /// `code_len` bytes of code and `data_len` zeroed writable bytes after them, the data
    /// within 1 GiB of every code byte (in PC-relative reach both ways), written by `write`:
    /// for code whose bytes depend on where it and its data are (a linker's relocations).
    ///
    /// `write` is given the code and the data, writable, at the addresses they run at, on the
    /// calling thread; when it returns the code is made executable. Other `Executable`s run
    /// meanwhile, on this thread too.
    pub fn written(
        code_len: usize,
        data_len: usize,
        write: impl FnOnce(&mut [u8], &mut [u8]),
    ) -> Result<Executable, Error> {
        Ok(Executable { memory: sys::Exec::new(code_len, data_len, write)? })
    }

    /// Where the code's first byte is.
    pub fn code_address(&self) -> u64 {
        self.memory.code_address()
    }

    /// Where the data's first byte is.
    pub fn data_address(&self) -> u64 {
        self.memory.data_address()
    }

    /// Reinterprets the executable entry point at `code_address()` as a typed function pointer `F`.
    ///
    /// # Safety
    /// - `F` must be a function pointer type (`extern "C" fn(...) -> ...`) with pointer size & alignment.
    /// - The sealed code inside this `Executable` must match `F`'s ABI and remain live while called.
    #[inline]
    pub unsafe fn as_fn<F: Copy>(&self) -> F {
        const {
            assert!(core::mem::size_of::<F>() == core::mem::size_of::<*const u8>(), "as_fn target must have pointer size");
            assert!(core::mem::align_of::<F>() == core::mem::align_of::<*const u8>(), "as_fn target must have pointer alignment");
        }
        let ptr = self.memory.code_address() as usize as *const u8;
        // SAFETY: `F` has pointer size & alignment (asserted at compile time) and caller guarantees
        // the sealed RX code matches `F`'s ABI.
        unsafe { core::ptr::read((&raw const ptr).cast::<F>()) }
    }

    pub fn data(&mut self) -> &mut [u8] {
        self.memory.data()
    }
}

/// Handle to a sub-page allocation inside a [`JitPool`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct CodeHandle {
    range: CodeRange,
    /// Exact requested byte length (`<= range.len`, which is rounded up to the 16-byte grain).
    len: usize,
}

impl CodeHandle {
    /// Byte offset from the start of the pool.
    pub const fn offset(&self) -> u64 {
        self.range.offset
    }

    /// Requested byte length of the code or data block.
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether the handle has zero requested length.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Allocated byte length in the pool's TLSF heap (rounded up to 16 bytes).
    pub const fn allocated_len(&self) -> u64 {
        self.range.len
    }

    /// Backing [`CodeRange`] within the pool.
    pub const fn range(&self) -> CodeRange {
        self.range
    }
}

/// A hot-patchable trampoline slot in a [`JitPool`] for atomic tier-up.
///
/// The slot's RX entry address ([`JitPool::tier_slot_ptr`]) remains constant across tier-ups;
/// [`JitPool::retarget`] atomically redirects subsequent calls to a new target without
/// changing the entry address or invoking `mprotect`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct TierSlot {
    handle: CodeHandle,
    /// Pool byte offset of the 8-byte-aligned indirect target word.
    target_word_offset: u64,
}

impl TierSlot {
    /// The underlying [`CodeHandle`] allocated for this trampoline stub.
    pub const fn handle(&self) -> CodeHandle {
        self.handle
    }

    /// Byte offset of the trampoline entry point inside the pool.
    pub const fn offset(&self) -> u64 {
        self.handle.offset()
    }
}

/// AArch64 `NOP` (`0xd503_201f`), concurrently modifiable per ARM ARM B2.2.5.
#[cfg(target_arch = "aarch64")]
const AARCH64_NOP: u32 = 0xd503_201f;
/// AArch64 `LDR x16, [pc, #12]` — loads the 8-byte target word at `+0x10` from `+0x04`.
#[cfg(target_arch = "aarch64")]
const AARCH64_LDR_X16_PC12: u32 = 0x5800_0070;
/// AArch64 `BR x16` — indirect branch to `x16`.
#[cfg(target_arch = "aarch64")]
const AARCH64_BR_X16: u32 = 0xd61f_0200;
/// AArch64 `BRK #0` — unreachable padding between `BR x16` and the 8-byte target word.
#[cfg(target_arch = "aarch64")]
const AARCH64_BRK0: u32 = 0xd420_0000;
/// AArch64 tier slot layout: `[B/NOP (4B) | LDR x16, #12 (4B) | BR x16 (4B) | BRK #0 (4B) | .quad target (8B)]`.
#[cfg(target_arch = "aarch64")]
const AARCH64_TIER_SLOT_BYTES: usize = 24;
#[cfg(target_arch = "aarch64")]
const AARCH64_TIER_TARGET_DELTA: u64 = 16;

/// x86-64 `JMP QWORD PTR [RIP + 2]; INT3; INT3` — jumps via the 8-byte-aligned target word at `+0x08`.
#[cfg(target_arch = "x86_64")]
const X86_64_TIER_STUB_HEAD: [u8; 8] = [0xff, 0x25, 0x02, 0x00, 0x00, 0x00, 0xcc, 0xcc];
#[cfg(target_arch = "x86_64")]
const X86_64_TIER_SLOT_BYTES: usize = 16;
#[cfg(target_arch = "x86_64")]
const X86_64_TIER_TARGET_DELTA: u64 = 8;

/// Encodes an AArch64 unconditional `B <to_rx>` from `from_rx` if in ±128 MiB and 4-byte aligned.
#[cfg(target_arch = "aarch64")]
fn encode_aarch64_b(from_rx: u64, to_rx: u64) -> Option<u32> {
    if !from_rx.is_multiple_of(4) || !to_rx.is_multiple_of(4) {
        return None;
    }
    let delta = i128::from(to_rx) - i128::from(from_rx);
    let imm26 = delta >> 2;
    const MIN_IMM26: i128 = -(1i128 << 25);
    const MAX_IMM26: i128 = (1i128 << 25) - 1;
    if !(MIN_IMM26..=MAX_IMM26).contains(&imm26) {
        return None;
    }
    Some(0x1400_0000u32 | ((imm26 as u32) & 0x03ff_ffff))
}

/// Dual-mapped W^X in-process JIT code pool.
///
/// One OS memory object ([`sys::Object`]) is mapped twice in this process:
/// - `rw` with [`Access::Shared`] (read-write, never executable)
/// - `rx` with [`Access::Execute`] (read-execute, never writable)
///
/// Sub-page allocations are managed by a TLSF [`CodeHeap`] at 16-byte granularity so many small
/// functions share pages with zero `mmap`/`mprotect`/`munmap` syscalls per function. Physical
/// instruction-cache coherence across the dual mapping is verified upfront by
/// [`sys::check_icache`].
pub struct JitPool {
    _object: sys::Object,
    rw: sys::View,
    rx: sys::View,
    heap: CodeHeap,
    capacity: u64,
    page: u64,
    /// Dirty RW byte spans awaiting [`JitPool::publish`]. Reused across publications.
    dirty: Vec<Range<usize>>,
}

impl JitPool {
    /// Creates a new dual-mapped W^X JIT pool of at least `capacity` bytes (rounded up to the
    /// OS mapping granule).
    pub fn new(capacity: u64) -> Result<JitPool, Error> {
        JitPool::named("jit-pool", capacity)
    }

    /// Creates a named dual-mapped W^X JIT pool of at least `capacity` bytes (rounded up to the
    /// OS mapping granule).
    pub fn named(name: &str, capacity: u64) -> Result<JitPool, Error> {
        if capacity == 0 {
            return Err(Error::new("a jit pool of no bytes"));
        }
        let page = sys::page();
        let capacity = capacity
            .checked_next_multiple_of(page)
            .ok_or_else(|| Error::new("jit pool capacity overflows"))?;
        usize::try_from(capacity)
            .map_err(|_| Error::new(format!("jit pool capacity {capacity:#x} exceeds usize")))?;
        sys::check_icache()?;
        let object = sys::Object::create(name, capacity)?;
        let rw = object.map_here(Access::Shared)?;
        let rx = object.map_here(Access::Execute)?;
        let heap = CodeHeap::new(0, capacity);
        Ok(JitPool { _object: object, rw, rx, heap, capacity, page, dirty: Vec::new() })
    }

    /// Total byte capacity of the pool (a multiple of [`JitPool::page`]).
    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    /// OS mapping granule in bytes.
    pub fn page(&self) -> u64 {
        self.page
    }

    /// Currently free bytes available in the pool's TLSF heap.
    pub fn free_bytes(&self) -> u64 {
        self.heap.free()
    }

    /// Size of the largest contiguous free block in the pool.
    pub fn largest_free(&self) -> u64 {
        self.heap.largest()
    }

    /// Base address of the read-execute (`RX`) mapping.
    pub fn rx_base(&self) -> u64 {
        self.rx.address()
    }

    /// Base address of the read-write (`RW`) mapping.
    pub fn rw_base(&self) -> u64 {
        self.rw.address()
    }

    /// Emits `bytes` into the pool with default 16-byte alignment.
    ///
    /// Does not flush the instruction cache immediately so multiple emissions can share a single
    /// [`JitPool::publish`] call.
    pub fn emit(&mut self, bytes: &[u8]) -> Result<CodeHandle, Error> {
        self.emit_aligned(bytes, alloc::GRAIN)
    }

    /// Emits `bytes` into the pool aligned to `align` (a power of two, clamped to at least 16).
    ///
    /// Call [`JitPool::publish`] before executing emitted code.
    pub fn emit_aligned(&mut self, bytes: &[u8], align: u64) -> Result<CodeHandle, Error> {
        if bytes.is_empty() {
            return Err(Error::new("cannot emit empty code into JitPool"));
        }
        let handle = self.reserve(bytes.len(), align)?;
        let start = handle.range.offset as usize;
        let end = start + bytes.len();
        self.rw.bytes_mut(start..end)?.copy_from_slice(bytes);
        self.mark_dirty(start..end);
        Ok(handle)
    }

    /// Allocates `len` zeroed bytes aligned to `align` (a power of two) from the pool's TLSF
    /// heap without performing any OS mapping syscalls.
    pub fn reserve(&mut self, len: usize, align: u64) -> Result<CodeHandle, Error> {
        if len == 0 || !align.is_power_of_two() {
            return Err(Error::new(format!(
                "invalid JitPool reservation of {len} bytes aligned to {align}"
            )));
        }
        let range = self.heap.take(len as u64, align).ok_or_else(|| {
            Error::new(format!(
                "JitPool is full: requested {len} bytes aligned to {align}, {} free (largest {})",
                self.heap.free(),
                self.heap.largest()
            ))
        })?;
        let start = range.offset as usize;
        let end = range.end() as usize;
        self.rw.bytes_mut(start..end)?.fill(0);
        Ok(CodeHandle { range, len })
    }

    /// Writes `bytes` at `offset` within an existing `handle` in the RW view and marks the span
    /// dirty for the next [`JitPool::publish`].
    pub fn write_at(
        &mut self,
        handle: CodeHandle,
        offset: usize,
        bytes: &[u8],
    ) -> Result<(), Error> {
        self.check_handle(handle)?;
        let end = offset.checked_add(bytes.len()).ok_or_else(|| {
            Error::new(format!("write offset {offset:#x} + {:#x} overflows", bytes.len()))
        })?;
        if end > handle.len {
            return Err(Error::new(format!(
                "write {offset:#x}..{end:#x} exceeds handle length {:#x}",
                handle.len
            )));
        }
        let abs_start = handle.range.offset as usize + offset;
        let abs_end = abs_start + bytes.len();
        self.rw.bytes_mut(abs_start..abs_end)?.copy_from_slice(bytes);
        self.mark_dirty(abs_start..abs_end);
        Ok(())
    }

    /// Flushes instruction caches for all dirty writes accumulated since the last publication.
    ///
    /// Adjacent and overlapping dirty spans are coalesced so sequential emissions flush in one
    /// cache maintenance pass.
    pub fn publish(&mut self) {
        if self.dirty.is_empty() {
            return;
        }
        if self.dirty.len() == 1 {
            let span = self.dirty[0].clone();
            self.dirty.clear();
            self.rw.publish(span);
            return;
        }
        self.dirty.sort_unstable_by_key(|r| (r.start, r.end));
        let mut current = self.dirty[0].clone();
        for i in 1..self.dirty.len() {
            let next = self.dirty[i].clone();
            // Coalesce spans within one cache line (64 bytes) to minimize icache calls.
            if next.start <= current.end.saturating_add(64) {
                current.end = current.end.max(next.end);
            } else {
                self.rw.publish(current);
                current = next;
            }
        }
        self.rw.publish(current);
        self.dirty.clear();
    }

    /// Returns the RX executable pointer for `handle`.
    ///
    /// Panics if `handle` does not lie within this pool's capacity.
    pub fn fn_ptr(&self, handle: CodeHandle) -> *const u8 {
        self.rx_address(handle) as *const u8
    }

    /// Returns the RX executable address for `handle`.
    ///
    /// Panics if `handle` does not lie within this pool's capacity.
    pub fn rx_address(&self, handle: CodeHandle) -> u64 {
        assert!(
            handle.range.end() <= self.capacity && handle.range.len > 0,
            "CodeHandle {:?} is out of bounds for JitPool capacity {:#x}",
            handle.range,
            self.capacity
        );
        self.rx.address() + handle.range.offset
    }

    /// Reinterprets the RX entry point of `handle` as a typed callable function pointer `F`.
    ///
    /// # Safety
    ///
    /// - `F` must be a function pointer type (`extern "C" fn(...) -> ...`, etc.) whose size and
    ///   alignment match `*const u8`.
    /// - `handle` must hold valid machine code matching `F`'s ABI and signature, published via
    ///   [`JitPool::publish`] (or [`JitPool::link_batch`] / [`JitPool::alloc_tier_slot`]), and
    ///   must remain live in this pool for as long as the returned pointer is called.
    pub unsafe fn typed_fn<F: Copy>(&self, handle: CodeHandle) -> F {
        const {
            assert!(
                core::mem::size_of::<F>() == core::mem::size_of::<*const u8>(),
                "typed_fn target must have pointer size"
            );
            assert!(
                core::mem::align_of::<F>() == core::mem::align_of::<*const u8>(),
                "typed_fn target must have pointer alignment"
            );
        }
        let ptr = self.fn_ptr(handle);
        // SAFETY: the caller guarantees `F` is a function pointer matching the published code at
        // `ptr`, and pointer size/alignment were proved at compile time above.
        unsafe { core::ptr::read((&raw const ptr).cast::<F>()) }
    }

    /// Reads the current bytes of `handle` through the RW view.
    pub fn rw_bytes(&self, handle: CodeHandle) -> Result<&[u8], Error> {
        self.check_handle(handle)?;
        let start = handle.range.offset as usize;
        self.rw.bytes(start..start + handle.len)
    }

    /// Releases `handle` back to the pool's TLSF [`CodeHeap`] for immediate reuse.
    ///
    /// The caller must ensure no thread is executing or will execute `handle` after release.
    pub fn release(&mut self, handle: CodeHandle) -> Result<(), Error> {
        self.check_handle(handle)?;
        self.heap.give(handle.range);
        Ok(())
    }

    /// Releases every handle in `batch` back to the pool's TLSF [`CodeHeap`].
    pub fn release_batch(&mut self, batch: &LinkedBatch) -> Result<(), Error> {
        for &handle in batch.handles() {
            self.check_handle(handle)?;
        }
        for &handle in batch.handles() {
            self.heap.give(handle.range);
        }
        Ok(())
    }

    /// Allocates a batch of functions and/or data blocks in the pool, resolves all inter-unit,
    /// existing-handle, and external relocations to their RX addresses, patches them in the RW
    /// view, and publishes the entire linked span with ONE instruction-cache flush.
    ///
    /// Atomic on error: if allocation or any relocation check fails, all allocations made for
    /// this batch are returned to the heap and no dirty state is left behind.
    pub fn link_batch(&mut self, units: &[LinkUnit<'_>]) -> Result<LinkedBatch, Error> {
        if units.is_empty() {
            return Ok(LinkedBatch::new(Vec::new()));
        }

        // Phase 0: upfront validation before mutating the heap.
        for (unit_idx, unit) in units.iter().enumerate() {
            if unit.bytes.is_empty() {
                return Err(Error::new(format!("link unit {unit_idx} has no bytes")));
            }
            if !unit.align.is_power_of_two() {
                return Err(Error::new(format!(
                    "link unit {unit_idx} alignment {} is not a power of two",
                    unit.align
                )));
            }
            for reloc in unit.relocs {
                match reloc.target {
                    RelocTarget::Unit(target_idx) if target_idx >= units.len() => {
                        return Err(Error::new(format!(
                            "link unit {unit_idx} references out-of-bounds unit {target_idx} (batch len {})",
                            units.len()
                        )));
                    }
                    RelocTarget::Handle(h) => self.check_handle(h)?,
                    RelocTarget::Unit(_) | RelocTarget::Address(_) => {}
                }
            }
        }

        // Phase 1: allocate all units in the slab (rolling back on OOM).
        let mut handles = Vec::with_capacity(units.len());
        for (unit_idx, unit) in units.iter().enumerate() {
            match self.heap.take(unit.bytes.len() as u64, unit.align) {
                Some(range) => handles.push(CodeHandle { range, len: unit.bytes.len() }),
                None => {
                    for h in handles {
                        self.heap.give(h.range);
                    }
                    return Err(Error::new(format!(
                        "JitPool OOM while allocating link unit {unit_idx} ({} bytes, align {}): {} free (largest {})",
                        unit.bytes.len(),
                        unit.align,
                        self.heap.free(),
                        self.heap.largest()
                    )));
                }
            }
        }

        // Phase 2 & 3: copy raw bytes into RW view and patch all relocations against RX addresses.
        let rx_base = self.rx.address();
        let patch_result: Result<(), Error> = (|| {
            for (unit, &handle) in units.iter().zip(&handles) {
                let start = handle.range.offset as usize;
                let end = start + unit.bytes.len();
                self.rw.bytes_mut(start..end)?.copy_from_slice(unit.bytes);
            }
            for (unit, &handle) in units.iter().zip(&handles) {
                let start = handle.range.offset as usize;
                let end = start + unit.bytes.len();
                let block_rx = rx_base + handle.range.offset;
                let slice = self.rw.bytes_mut(start..end)?;
                for reloc in unit.relocs {
                    let target_rx = match reloc.target {
                        RelocTarget::Unit(idx) => rx_base + handles[idx].range.offset,
                        RelocTarget::Handle(h) => rx_base + h.range.offset,
                        RelocTarget::Address(addr) => addr,
                    };
                    apply_reloc(slice, block_rx, reloc.offset, reloc.kind, target_rx, reloc.addend)?;
                }
            }
            Ok(())
        })();

        if let Err(err) = patch_result {
            for h in handles {
                self.heap.give(h.range);
            }
            return Err(err);
        }

        // Phase 4: ONE icache flush covering the entire linked span.
        let mut span_start = usize::MAX;
        let mut span_end = 0usize;
        for &h in &handles {
            span_start = span_start.min(h.range.offset as usize);
            span_end = span_end.max(h.range.end() as usize);
        }
        if span_start < span_end {
            self.rw.publish(span_start..span_end);
        }

        Ok(LinkedBatch::new(handles))
    }

    /// Allocates an atomic tier-up trampoline stub initially branching to `initial` and publishes
    /// it immediately.
    pub fn alloc_tier_slot(&mut self, initial: CodeHandle) -> Result<TierSlot, Error> {
        self.check_handle(initial)?;
        let target_addr = self.rx_address(initial);
        self.alloc_tier_slot_address(target_addr)
    }

    /// Allocates an atomic tier-up trampoline stub initially targeting absolute RX address
    /// `target_addr` and publishes it immediately.
    pub fn alloc_tier_slot_address(&mut self, target_addr: u64) -> Result<TierSlot, Error> {
        #[cfg(target_arch = "aarch64")]
        {
            let handle = self.reserve(AARCH64_TIER_SLOT_BYTES, alloc::GRAIN)?;
            let offset = handle.range.offset;
            let off_usize = offset as usize;
            let target_word_offset = offset + AARCH64_TIER_TARGET_DELTA;
            let slot_rx = self.rx.address() + offset;

            // Populate fallback sequence: `ldr x16, [pc, #12]; br x16; brk #0`.
            {
                let bytes = self.rw.bytes_mut(off_usize..off_usize + AARCH64_TIER_SLOT_BYTES)?;
                bytes[4..8].copy_from_slice(&AARCH64_LDR_X16_PC12.to_le_bytes());
                bytes[8..12].copy_from_slice(&AARCH64_BR_X16.to_le_bytes());
                bytes[12..16].copy_from_slice(&AARCH64_BRK0.to_le_bytes());
            }
            // Store the 8-byte target word before publishing the entry instruction.
            self.rw.word(target_word_offset as usize).store(target_addr, Ordering::Release);
            let entry_insn = encode_aarch64_b(slot_rx, target_addr).unwrap_or(AARCH64_NOP);
            if !crate::is_concurrently_modifiable(entry_insn) {
                self.heap.give(handle.range);
                return Err(Error::new(format!(
                    "tier slot entry instruction {entry_insn:#010x} is not concurrently modifiable"
                )));
            }
            self.rw.word32(off_usize).store(entry_insn, Ordering::Release);
            self.rw.publish(off_usize..off_usize + AARCH64_TIER_SLOT_BYTES);
            Ok(TierSlot { handle, target_word_offset })
        }
        #[cfg(target_arch = "x86_64")]
        {
            let handle = self.reserve(X86_64_TIER_SLOT_BYTES, alloc::GRAIN)?;
            let offset = handle.range.offset;
            let off_usize = offset as usize;
            let target_word_offset = offset + X86_64_TIER_TARGET_DELTA;
            self.rw.bytes_mut(off_usize..off_usize + 8)?.copy_from_slice(&X86_64_TIER_STUB_HEAD);
            self.rw.word(target_word_offset as usize).store(target_addr, Ordering::Release);
            self.rw.publish(off_usize..off_usize + X86_64_TIER_SLOT_BYTES);
            Ok(TierSlot { handle, target_word_offset })
        }
        #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
        {
            let _ = target_addr;
            Err(Error::new("tier slots are supported only on AArch64 and x86-64"))
        }
    }

    /// Returns the stable RX entry pointer of `slot`.
    pub fn tier_slot_ptr(&self, slot: TierSlot) -> *const u8 {
        self.fn_ptr(slot.handle)
    }

    /// Returns the stable RX entry address of `slot`.
    pub fn tier_slot_address(&self, slot: TierSlot) -> u64 {
        self.rx_address(slot.handle)
    }

    /// Atomically retargets `slot` to jump to `new_target`.
    ///
    /// - On AArch64 within ±128 MiB: verifies [`crate::is_concurrently_modifiable`], atomically
    ///   patches the 4-byte `B` instruction via `AtomicU32::store(..., Ordering::Release)`, and
    ///   publishes the 4-byte instruction line.
    /// - On AArch64 beyond ±128 MiB: atomically updates the 8-byte target word via
    ///   `AtomicU64::store(..., Ordering::Release)` and patches the entry word to `NOP` (if not
    ///   already `NOP`) so execution falls through to `LDR x16, [pc, #12]; BR x16`.
    /// - On x86-64: atomically swaps the 8-byte target word via
    ///   `AtomicU64::store(..., Ordering::Release)`.
    pub fn retarget(&mut self, slot: TierSlot, new_target: CodeHandle) -> Result<(), Error> {
        self.check_handle(new_target)?;
        let target_addr = self.rx_address(new_target);
        self.retarget_address(slot, target_addr)
    }

    /// Atomically retargets `slot` to jump to absolute RX address `target_addr`.
    pub fn retarget_address(&mut self, slot: TierSlot, target_addr: u64) -> Result<(), Error> {
        self.check_handle(slot.handle)?;
        #[cfg(target_arch = "aarch64")]
        {
            let expected_word = slot.handle.range.offset + AARCH64_TIER_TARGET_DELTA;
            if slot.target_word_offset != expected_word
                || slot.handle.len < AARCH64_TIER_SLOT_BYTES
            {
                return Err(Error::new("invalid AArch64 TierSlot metadata"));
            }
            let offset = slot.handle.range.offset as usize;
            let slot_rx = self.rx.address() + slot.handle.range.offset;

            // Update the 8-byte indirect target word first with Release ordering so any thread
            // falling through a `NOP` sees a valid target address (old or new).
            self.rw.word(slot.target_word_offset as usize).store(target_addr, Ordering::Release);

            if let Some(b_insn) = encode_aarch64_b(slot_rx, target_addr) {
                if !crate::is_concurrently_modifiable(b_insn) {
                    return Err(Error::new(format!(
                        "encoded branch {b_insn:#010x} is not concurrently modifiable"
                    )));
                }
                self.rw.word32(offset).store(b_insn, Ordering::Release);
                self.rw.publish(offset..offset + 4);
            } else {
                if !crate::is_concurrently_modifiable(AARCH64_NOP) {
                    return Err(Error::new("AArch64 NOP is not concurrently modifiable"));
                }
                let entry_word = self.rw.word32(offset);
                if entry_word.load(Ordering::Relaxed) != AARCH64_NOP {
                    entry_word.store(AARCH64_NOP, Ordering::Release);
                    self.rw.publish(offset..offset + 4);
                }
            }
            Ok(())
        }
        #[cfg(target_arch = "x86_64")]
        {
            let expected_word = slot.handle.range.offset + X86_64_TIER_TARGET_DELTA;
            if slot.target_word_offset != expected_word || slot.handle.len < X86_64_TIER_SLOT_BYTES
            {
                return Err(Error::new("invalid x86-64 TierSlot metadata"));
            }
            self.rw.word(slot.target_word_offset as usize).store(target_addr, Ordering::Release);
            Ok(())
        }
        #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
        {
            let _ = (slot, target_addr);
            Err(Error::new("tier slots are supported only on AArch64 and x86-64"))
        }
    }

    /// Releases a [`TierSlot`] back to the pool's TLSF [`CodeHeap`].
    pub fn release_tier_slot(&mut self, slot: TierSlot) -> Result<(), Error> {
        self.release(slot.handle)
    }

    fn check_handle(&self, handle: CodeHandle) -> Result<(), Error> {
        let range = handle.range;
        if range.len == 0
            || !range.offset.is_multiple_of(alloc::GRAIN)
            || range.end() > self.capacity
            || handle.len as u64 > range.len
        {
            return Err(Error::new(format!(
                "invalid CodeHandle ({:#x}..{:#x}, len {:#x}) for JitPool of {:#x} bytes",
                range.offset,
                range.end(),
                handle.len,
                self.capacity
            )));
        }
        Ok(())
    }

    fn mark_dirty(&mut self, span: Range<usize>) {
        if span.is_empty() {
            return;
        }
        if let Some(last) = self.dirty.last_mut()
            && span.start <= last.end.saturating_add(64)
            && last.start <= span.end.saturating_add(64)
        {
            last.start = last.start.min(span.start);
            last.end = last.end.max(span.end);
            return;
        }
        self.dirty.push(span);
    }
}
