//! The baseline tier (docs/v2/plan/design/backend-next.md): one LIR function to AArch64
//! machine code in one forward pass, with no optimization. It is the first backend that makes
//! `blaze run` run a program (crates/blaze-cli/tests/ladder/README.md); it does not replace the
//! copy-and-patch stencil backend (crates/blaze-stencil, parked), which is the fast tier this
//! one stands in for until it is back.
//!
//! # The code it makes
//!
//! - Every LIR value (an instruction's result or a block parameter) has its own 16-byte stack
//!   cell, and every LIR stack slot its own aligned stretch of the frame. An instruction loads
//!   its operands into scratch registers, computes, and stores its result to its cell.
//! - The frame: `stp x29, x30, [sp, #-16]!; mov x29, sp; sub sp, sp, #frame`; the epilogue
//!   undoes it. Parameters arrive and results leave in the platform's C convention (AAPCS64,
//!   Apple's variant on macOS), which is also the convention of Rust-ABI functions over their
//!   lowered signatures (blaze-lir's `Conv`).
//! - A call of reference `place` loads an 8-byte literal word placed after the code, then:
//!   for [`Via::Slot`], loads the slot word that literal points at and branches to it (the
//!   launcher's slot table, docs/v2/plan/design/run.md); for [`Via::Address`], branches to
//!   the literal's value (an outside symbol). Placement fills each word ([`Fixup`]).
//! - The function's own data (`data_addr` of a `RefKind::Data` reference) is copied after the
//!   literal words and reached PC-relatively (`adr`): no fixup.
//!
//! - The code is followed by its literal words (8-aligned) and its data rows (each at least
//!   8-aligned, at most 16): placement puts the code's first byte at a 16-aligned address.
//!
//! # Values
//!
//! One invariant holds for every cell, kept on every write: **a cell's 8 low bytes hold the
//! value zero-extended to 64 bits** (an `i1` is 0 or 1, an `i8` 0..=255; a pointer is its 64
//! bits). An operation computes in whole 64-bit registers: one that only needs the low bits
//! right (`add`, `sub`, `mul`, `neg`, `not`, `shl`, `trunc`) zero-extends its result back to the
//! width; one that reads the sign (`sdiv`, `srem`, `ashr`, a signed `icmp`, `sext`) sign-extends
//! its operands first. A parameter or a call's return arrives with its bits above the width
//! unspecified and is zero-extended at once.
//!
//! # Traps
//!
//! A LIR `trap` is `brk #code` (codes below 0xff00); division by zero is `brk #0xff00`,
//! `sdiv` of `MIN` by -1 `brk #0xff01`, `unreachable` `brk #0xff02`.
//!
//! # Pairs
//!
//! A compound of two integer or pointer members (the result of an overflow operation, of
//! `cmpxchg`, or of a call returning two integers) lives in its 16-byte cell: member `k` at
//! byte `8 * k`, each zero-extended as above. `project` loads one member. A block argument of
//! such a type moves both words. Any other compound is refused by name.
//!
//! # Calls and signatures (AAPCS64; Apple's variant on macOS)
//!
//! - Integer and pointer parameters in `x0..x7`, in order; an `sret` parameter is not among
//!   them: it arrives in `x8` (6.9, "indirect result location register") and a call passes it
//!   there. A `byval` parameter is passed as the address of a copy the caller makes in its own
//!   frame (B.4: a composite past 16 bytes is replaced by a pointer to a caller-made copy);
//!   the callee receives that pointer in the next integer register. Its size and alignment
//!   are the parameter's facts (`ParamFactsRec::size`, `align`).
//! - One integer return in `x0`, or two in `x0` and `x1` (C.12: a 9..=16-byte composite).
//! - `call` reaches its callee through a literal word ([`Via`]); `call_indirect` through the
//!   pointer value: `blr` of it.
//! - `func_addr` of a [`Via::Slot`] function is the address of its slot stub
//!   ([`RelocTo::SlotStub`]): `ldr x16, <slot word>; br x16`, which placement makes once per
//!   slot, so the address stays the same however often the slot is patched. A function
//!   pointer to a slot function in data is the same stub.
//!
//! # Atomics
//!
//! The mapping of LIR's orderings (an ordering may be raised, never lowered: `SeqCst` is
//! served by the acquire-release forms, which are RCsc on Armv8):
//!
//! | LIR | without LSE (`Isa { lse: false }`) | with LSE (FEAT_LSE, Armv8.1) |
//! |---|---|---|
//! | `atomic_load` relaxed | `ldr` | the same |
//! | `atomic_load` acquire, seq_cst | `ldar` | the same |
//! | `atomic_store` relaxed | `str` | the same |
//! | `atomic_store` release, seq_cst | `stlr` | the same |
//! | `atomic_rmw` xchg, add, or, xor | `ld{a}xr; op; st{l}xr; cbnz` loop | `swp`, `ldadd`, `ldset`, `ldeor` |
//! | `atomic_rmw` sub | the loop | `neg`, then `ldadd` |
//! | `atomic_rmw` and | the loop | `mvn`, then `ldclr` (clears the complement's bits) |
//! | `atomic_rmw` max, min, umax, umin | the loop (`cmp; csel`) | `ldsmax`, `ldsmin`, `ldumax`, `ldumin` |
//! | `atomic_rmw` nand | the loop | the loop (LSE has no nand) |
//! | `cmpxchg` | `ld{a}xr; cmp; b.ne; st{l}xr; cbnz` loop, `clrex` on failure | `cas{a,l,al}` |
//! | `fence` acquire | `dmb ishld` | the same |
//! | `fence` release, acq_rel, seq_cst | `dmb ish` | the same |
//! | `fence` single_thread | nothing | the same |
//!
//! The suffixes by ordering: relaxed none, acquire `a` (`ldaxr`), release `l` (`stlxr`),
//! acq_rel and seq_cst both. A `cmpxchg` acquires when its success or its failure ordering
//! does, and releases when its success ordering does. A weak `cmpxchg` is emitted strong (a
//! strengthening). A single-thread fence (`compiler_fence`) is no instruction: this tier
//! keeps every memory access in program order (each is its own instruction, nothing is
//! cached in a register across one), so there is nothing for it to order. Sizes 8, 16, 32
//! and 64 bits (and `ptr`); an atomic of an `i1` is refused.
//!
//! # Bulk memory
//!
//! `memcpy`, `memmove` and `memset` of a length defined by an `iconst` of at most
//! [`BULK_INLINE_MOST`] bytes are inline (8-byte accesses, then 4, 2 and 1 for the tail; a
//! `memmove` loads every piece before it stores any, so it is inline only up to
//! [`MOVE_INLINE_MOST`] bytes, what fits in its registers). Any other is a call of the
//! system routine through a literal word placement fills ([`RelocTo::Routine`]). A
//! `volatile` one is refused.
//!
//! # Thread-locals
//!
//! `tls_addr` follows the scheme of crates/blaze-build/src/tls.rs: one block per thread found
//! by a pthread key, a descriptor static `{key @0, size @8, align @16, template @24}`
//! ([`RelocTo::TlsDesc`]), and a getter `fn(desc) -> block` (C convention,
//! [`RelocTo::TlsGetter`]) that allocates a thread's block on its first use. The thread-local's
//! offset in the block is a literal word too ([`RelocTo::TlsOffset`]).
//!
//! - On macOS ([`Os::MacOs`]) the block is found in line, the getter only called on a thread's
//!   first access: `ldr x9, =desc; ldr x10, [x9]; mrs x11, tpidrro_el0; and x11, x11, #~7;
//!   ldr x0, [x11, x10, lsl #3]; cbnz x0, 1f; mov x0, x9; ldr x16, =getter; blr x16;
//!   1: ldr x10, =offset; add x9, x0, x10`. This is a macOS private ABI (rule 45): libpthread's
//!   `_pthread_getspecific_direct` reads the thread's specific-data slots at
//!   `tpidrro_el0 & ~7`, and a key from `pthread_key_create` is its slot's index. The test
//!   `the_direct_tsd_read_equals_pthread_getspecific` fails if an OS changes that layout.
//! - On Linux ([`Os::Linux`]) every access calls the getter: the portable twin, the same
//!   result through `pthread_getspecific`.
//!
//! # The subset
//!
//! Types `i1`, `i8`, `i16`, `i32`, `i64` and `ptr` (64-bit targets), and the pairs above.
//! Operations:
//!
//! - values: `param`, `iconst`, `iconst_wide`, `null`, `freeze`, `select`, `project`;
//! - integers: `add`, `sub`, `mul`, `neg`, `udiv`, `sdiv`, `urem`, `srem`, `and`, `or`,
//!   `xor`, `not`, `shl`, `lshr`, `ashr`, `rotl`, `rotr`, `umulhi`, `smulhi`, `umin`, `umax`,
//!   `smin`, `smax`, `uadd_sat`, `sadd_sat`, `usub_sat`, `ssub_sat`, `icmp`, `zext`, `sext`,
//!   `trunc`; the overflow operations `uadd_overflow`, `sadd_overflow`, `usub_overflow`,
//!   `ssub_overflow`, `umul_overflow`, `smul_overflow` (Rust's `overflowing_*`); the bit
//!   operations `clz`, `ctz`, `popcnt` (through `cnt` of a vector register: no CSSC on M1),
//!   `bswap` (of an `i1` or `i8`, the value itself), `bitrev`;
//! - pointers and memory: `slot_addr`, `slot_live`, `slot_dead`, `ptr_add`, `ptr_addr`,
//!   `load`, `store`, `memcpy`, `memmove`, `memset`, `atomic_load`, `atomic_store`,
//!   `atomic_rmw` (every kind), `cmpxchg`, `fence`, `data_addr` (own data, its relocations as
//!   [`Reloc`]s; a static through a literal word placement fills, also a [`Reloc`]),
//!   `func_addr`, `tls_addr`;
//! - calls: `call` and `call_indirect` (C convention, at most 8 integer arguments past an
//!   `sret`, `byval` copies, at most two integer returns, not variadic);
//! - ends: `jump`, `br`, `switch`, `return`, `trap`, `unreachable`.
//!
//! Promise and hint flags are dropped (they only allow UB or say nothing); `volatile` on a
//! load, store or atomic is honoured (each is one access of its width), atomic orderings as
//! above. Refused by name: floats, vectors, `i128`, compounds other than integer pairs,
//! more than 8 integer arguments (the rest go on the stack), a variadic signature (Apple
//! passes the variable arguments on the stack), more than two returns, a `volatile` bulk
//! operation, `memcmp`, `uninit`, `invoke` and unwinding, tail calls, `asm!`, target
//! intrinsics, and `frame_address`, `return_address`, `stack_pointer`. The provenance
//! operations are their bits: `expose` is `ptr_addr`, `from_exposed` the address itself,
//! `ptr_with_addr` the new address.
//!
//! Anything outside the subset is [`EmitError::Unsupported`], naming the instruction: never a
//! guess.

#![feature(allocator_api)]

use core::alloc::Allocator;
use core::fmt;

use blaze_lir::{Op, View};

mod enc;
mod pass;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod reloc_tests;

#[cfg(test)]
mod enc_tests;

#[cfg(test)]
mod int_tests;

#[cfg(test)]
mod atomic_tests;

#[cfg(test)]
mod bulk_tests;

#[cfg(test)]
mod abi_tests;

#[cfg(test)]
mod tls_tests;

/// The most bytes a `memcpy` or `memset` of a constant length is emitted inline; past it, the
/// system routine is called. Measured on this Mac (Apple M-series, 2026-11-16) by
/// `bulk_tests::inline_or_call` (`#[ignore]`d): 10^7 copies of each size in an emitted loop,
/// ns per copy inline / through `memcpy`:
///
/// | bytes | 8 | 32 | 64 | 96 | 112 | 120 | 128 | 136 | 160 | 192 | 256 |
/// |---|---|---|---|---|---|---|---|---|---|---|---|
/// | inline | 1.39 | 1.79 | 2.25 | 3.05 | 3.46 | 3.66 | 3.88 | 3.86 | 4.44 | 4.62 | 6.55 |
/// | call | 1.62 | 2.37 | 2.82 | 3.37 | 3.47 | 3.90 | 3.79 | 3.79 | 4.24 | 4.69 | 5.32 |
///
/// Inline wins through 120 bytes, the call from 128 on (and its code is three words, not up
/// to thirty).
pub const BULK_INLINE_MOST: u64 = 120;

/// The most bytes a `memmove` of a constant length is emitted inline: it loads every piece
/// into a register before it stores any (its source and destination may overlap), and the
/// registers it takes (`x0..x7`, `x12..x15`, `x17`) hold eight 8-byte pieces and a tail.
pub const MOVE_INLINE_MOST: u64 = 64;

/// What the target core has, as far as the code this tier makes depends on it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Isa {
    /// FEAT_LSE (Armv8.1): the atomic memory operations and `cas`.
    pub lse: bool,
    pub os: Os,
}

/// The operating system the code runs under, as far as the code depends on it (thread-locals).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Os {
    MacOs,
    Linux,
}

/// A routine of the system's C library the code calls.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Routine {
    /// `memcpy(dst, src, len)`.
    Memcpy,
    /// `memmove(dst, src, len)`.
    Memmove,
    /// `memset(dst, byte, len)`.
    Memset,
}

/// How a call of one function reference is made.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Via {
    /// Through a launcher slot: the literal word holds the slot word's address.
    Slot,
    /// Directly: the literal word holds the function's address (an outside symbol).
    Address,
}

/// An 8-byte literal word of the code that placement fills.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Fixup {
    /// The word's offset from the code's first byte (8-byte aligned).
    pub at: u32,
    /// The function reference's place (`RefKind::Func`) whose address goes there: for
    /// [`Via::Slot`] its slot word's address, for [`Via::Address`] the function's.
    pub place: u32,
}

/// What an absolute address word of the code ([`Reloc`]) holds the address of.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RelocTo {
    /// The static of reference place `place` (`RefKind::Static`).
    Static(u32),
    /// The vtable of reference place `place` (`RefKind::Vtable`): the builder's
    /// `Built::vtables` names its (concrete type, principal trait) record.
    Vtable(u32),
    /// Byte `offset` of this code itself (data placed with the code, reached from other data).
    Code(u32),
    /// The function of reference place `place` (`RefKind::Func`), called [`Via::Address`]:
    /// its address.
    Func(u32),
    /// The slot stub of the function of reference place `place`, called [`Via::Slot`]: the
    /// address of `ldr x16, <its slot word>; br x16`, which placement makes once per slot. It
    /// is the function's address as a value (`func_addr`, a function pointer in data): it
    /// stays the same however often the slot word is patched.
    SlotStub(u32),
    /// The system routine's address.
    Routine(Routine),
    /// The program's thread-local descriptor static (crates/blaze-build/src/tls.rs).
    TlsDesc,
    /// The program's thread-local getter, `fn(desc) -> block` (C convention).
    TlsGetter,
    /// Not an address: the byte offset of the thread-local of reference place `place`
    /// (`RefKind::Tls`) in the program's thread-local block. Placement writes that offset
    /// plus `addend` as the word itself.
    TlsOffset(u32),
}

/// An 8-byte word of the code that placement fills: the address of `to` plus `addend`,
/// little-endian (for [`RelocTo::TlsOffset`], the offset plus `addend`). A literal word the
/// code loads (`data_addr` of a static, a routine, a thread-local's descriptor, getter and
/// offset, `func_addr` of a slot function; `addend` 0), and each relocation of the function's
/// own data rows (`DataRelocRec`), are one each.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Reloc {
    /// The word's offset from the code's first byte (8-byte aligned).
    pub at: u32,
    pub to: RelocTo,
    pub addend: i64,
}

/// One function's machine code.
pub struct Code<A: Allocator> {
    pub bytes: Vec<u8, A>,
    pub fixups: Vec<Fixup, A>,
    /// Absolute address words placement fills (after the fixups, in any order).
    pub relocs: Vec<Reloc, A>,
}

impl<A: Allocator + Clone> Code<A> {
    pub fn new_in(alloc: A) -> Code<A> {
        Code { bytes: Vec::new_in(alloc.clone()), fixups: Vec::new_in(alloc.clone()), relocs: Vec::new_in(alloc) }
    }
}

/// Why a function was not emitted.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EmitError {
    /// Instruction `inst` is outside the baseline tier's subset.
    Unsupported { inst: u32, op: Op, why: &'static str },
    /// A reference `place` the function names has no entry in the `via` table given.
    NoVia { place: u32 },
    /// The frame or the code passed a bound of the encoding.
    TooLarge { what: &'static str },
    /// The function's own signature, or its target, is outside the subset.
    Signature { why: &'static str },
    /// Data row `row` holds an address the baseline tier cannot place.
    Data { row: u32, why: &'static str },
}

impl fmt::Display for EmitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EmitError::Unsupported { inst, op, why } => {
                write!(f, "instruction {inst} (`{}`): the baseline tier does not emit it: {why}", op.name())
            }
            EmitError::NoVia { place } => write!(f, "function reference {place} has no call kind (`Via`) given"),
            EmitError::TooLarge { what } => write!(f, "{what} is past what the baseline tier encodes"),
            EmitError::Signature { why } => write!(f, "the function's signature: the baseline tier does not emit {why}"),
            EmitError::Data { row, why } => write!(f, "data row {row}: the baseline tier does not place {why}"),
        }
    }
}

impl core::error::Error for EmitError {}

/// Emits `func` (verified by `blaze_lir::verify`) into `out`, which is cleared first, for a
/// core and system `isa` says. `via` has one entry per function reference place the function
/// names.
pub fn emit<A: Allocator>(func: View<'_>, via: &[Via], isa: Isa, out: &mut Code<A>) -> Result<(), EmitError> {
    emit_inline_up_to(func, via, isa, BULK_INLINE_MOST, out)
}

/// [`emit`], inlining a bulk operation of a constant length up to `inline_most` bytes (the
/// measurement compares both sides of [`BULK_INLINE_MOST`]).
pub(crate) fn emit_inline_up_to<A: Allocator>(
    func: View<'_>,
    via: &[Via],
    isa: Isa,
    inline_most: u64,
    out: &mut Code<A>,
) -> Result<(), EmitError> {
    out.bytes.clear();
    out.fixups.clear();
    out.relocs.clear();
    pass::run(func, via, isa, inline_most, &mut out.bytes, &mut out.fixups, &mut out.relocs)
}
