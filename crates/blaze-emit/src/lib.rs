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
//!   8-aligned, at most a 4 KiB page): placement puts the code's first byte at the alignment
//!   [`Code::align`] says (at least 16, the most any row asks).
//! - No function is refused for its size below `b`'s ±128 MiB of instructions: a conditional
//!   branch, literal load or `adr` whose target may pass its ±1 MiB is reached through an
//!   island of veneers placed in the code, or a long form (pass/reach.rs).
//!
//! # The record form
//!
//! [`emit_record`] is the same pass with no address word in its output
//! (docs/v2/plan/design/run.md, "End state"): it makes a `blaze_code::record::CodeRecord`
//! whose every outside reference is an import site (`adrp; ldr`, and `blr x16` for a call)
//! naming its target by (space, symbol), and whose data holding an address is in the relro
//! part. The table of both forms is in pass/sites.rs. [`emit`] (literal words) stays until the
//! run path reads code records; both forms run equal (`record_tests`).
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
//! such a type moves both words.
//!
//! A wide pair, `{i128, iN}` with N at most 64 (the result of a 128-bit overflow operation),
//! takes 24 bytes: the `i128` in its cell (pass/wide.rs), member 1 in a third word placed
//! after all the cells, one 16-byte step per wide pair. A block argument of it moves all three
//! words. Any other compound is refused by name.
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
//! An atomic of an `i128` (pass/atomic.rs `atomic_wide`) is one exclusive-pair loop
//! (`ld{a}xp; ..; st{l}xp; cbnz`) for `atomic_load` (the pair stored back: Armv8.0's
//! 16-byte access is single-copy atomic only when the store-exclusive succeeds),
//! `atomic_store` and every `atomic_rmw`; `cmpxchg` is `casp{a,l,al}` with LSE, else the
//! loop, which stores the old pair back on a mismatch.
//!
//! # Bulk memory
//!
//! `memcpy`, `memmove` and `memset` of a length defined by an `iconst` of at most
//! [`BULK_INLINE_MOST`] bytes are inline (8-byte accesses, then 4, 2 and 1 for the tail; a
//! `memmove` loads every piece before it stores any, so it is inline only up to
//! [`MOVE_INLINE_MOST`] bytes, what fits in its registers). Any other is a call of the
//! system routine through a literal word placement fills ([`RelocTo::Routine`]). A
//! `volatile` one is refused. `memcmp` follows the same bound: inline, each piece pair
//! compared and the first unequal pair ordered by its byte-reversed words; else a call of
//! `memcmp` (pass/bulk.rs).
//!
//! # Thread-locals
//!
//! `tls_addr` is the index form of crates/blaze-build/src/tls/index.rs (THREADS.md D2;
//! `pass/tls_index.rs`): a descriptor static `{key @0, root @8, count @16, cap @24, vars @32}`
//! ([`RelocTo::TlsDesc`]), the thread-local's index as a literal word ([`RelocTo::TlsIndex`]),
//! and a getter `fn(desc, index, root_at) -> copy` (C convention, [`RelocTo::TlsGetter`])
//! called only on the cold path. The thread's table is found in line from the root: on macOS
//! ([`Os::MacOs`]) the key's TSD slot at `(tpidrro_el0 & ~7) + key * 8`, a macOS private ABI
//! (rule 45): libpthread's `_pthread_getspecific_direct` reads the thread's specific-data slots
//! there, and a key from `pthread_key_create` is its slot's index; the test
//! `the_direct_tsd_read_equals_pthread_getspecific` fails if an OS changes that layout. On Linux
//! ([`Os::Linux`]) the launcher's own thread-local at `tpidr_el0 + root`, which the getter keeps.
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
//!   `sret`, `byval` copies, at most two integer returns; a C-variadic callee's variable
//!   arguments in 8-byte stack slots on Apple and in the registers then stack slots on Linux:
//!   pass/variadic.rs);
//! - ends: `jump`, `br`, `switch`, `return`, `trap`, `unreachable`.
//!
//! Promise and hint flags are dropped (they only allow UB or say nothing); `volatile` on a
//! load, store or atomic is honoured (each is one access of its width), atomic orderings as
//! above. Refused by name: floats, vectors, `i128`, compounds other than integer pairs,
//! (arguments past `x7`/`v7` go on the stack, both ways: pass/stack.rs), a C-variadic definition, a
//! variable argument narrower than 32 bits, an `f32`, an `i128` or a compound, more than 510
//! stacked or stacked under an `invoke`, more than two returns, a `volatile` bulk
//! operation, `uninit`, `invoke` and unwinding, tail calls, `asm!` with `label` operands
//! (`asm_br`) or a template statement outside the assembler's table (pass/asm.rs, asm.rs),
//! target intrinsics outside pass/target.rs's four, and `frame_address`, `return_address`, `stack_pointer`. The provenance
//! operations are their bits: `expose` is `ptr_addr`, `from_exposed` the address itself,
//! `ptr_with_addr` the new address.
//!
//! Anything outside the subset is [`EmitError::Unsupported`], naming the instruction: never a
//! guess.

#![cfg_attr(not(test), no_std)]
#![feature(allocator_api)]
// quad_run_tests.rs calls emitted `f128` functions and names the routines' `f128` signatures.
#![cfg_attr(test, feature(f128))]


extern crate alloc;
use alloc::vec::Vec;
use core::alloc::Allocator;
use core::fmt;

use blaze_lir::{Op, Verified};

/// The `asm!` template reader and its mnemonic table (pass/asm.rs lowers the block).
mod asm;
mod compose;
mod enc;
mod enc_probe;
mod enc_tls;
mod pass;
mod record;
mod record_check;
/// The soft-float routine table a record's caller hands in, and the routines it calls
/// (INTRINSICS.md 3.5, P5b).
mod routines;
/// Scratch registers as taken and given-back tokens (D193 point 6), for both passes: the Fast
/// tier's composer today; the Reference tier's cell emitter is ported onto it (T3).
pub(crate) mod scratch;
#[cfg(test)]
mod scratch_tests;
/// Experimental non-LLVM SOTA optimizing tier (Aegraph + LICM + Strength Reduction + Chordal-SSA
/// regalloc + ISLE-style fused AArch64 emitter; default optimizing tier on AArch64).
mod sota;
pub mod table_gen;
mod tier;
pub use sota::{
    CallMode, CompiledModuleFn, ModuleFn, Sota, SotaModuleOpts, SotaModuleOut, compile_module_a64,
    emit_sota_record_measured,
};
pub use tier::Reference;
pub use asm::AsmToken;

pub use record::{Named, Names, Outside, RecordEmitError, emit_record};
pub use routines::{Called, RoutinePlaces};
pub use compose::Fast;
pub use record_check::{AddressWord, emit_record_measured, no_address_word};

#[cfg(test)]
mod tests;

#[cfg(test)]
mod record_tests;
#[cfg(test)]
mod unwind_tests;

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
mod variadic_tests;

/// Arguments past the registers, both sides (pass/stack.rs).
#[cfg(test)]
mod stack_tests;

/// `memcmp`, inline and called (pass/bulk.rs).
#[cfg(test)]
mod memcmp_tests;

#[cfg(test)]
mod tls_tests;

#[cfg(test)]
mod tls_index_tests;

#[cfg(test)]
mod enc_tls_tests;

#[cfg(test)]
mod probe_tests;

#[cfg(test)]
mod probe_place_tests;

#[cfg(test)]
mod num_tests;

#[cfg(test)]
mod target_tests;

/// `f128` and `f16` in the Reference tier (pass/float_width.rs).
#[cfg(test)]
mod quad_tests;

/// Every trap code a producer writes is blaze-code trap.rs's (its one home).
#[cfg(test)]
mod trap_codes_tests;

#[cfg(all(test, target_arch = "aarch64"))]
mod tier_tests;

/// Branches and references past ±1 MiB, run (pass/reach.rs).
#[cfg(all(test, target_arch = "aarch64"))]
mod reach_tests;

/// Data rows aligned past 16 in both forms, run (lib.rs `Code::align`, pass/sites.rs).
#[cfg(all(test, target_arch = "aarch64"))]
mod data_align_tests;

/// Atomics of an `i128`, run (pass/atomic.rs `atomic_wide`).
#[cfg(all(test, target_arch = "aarch64"))]
mod atomic_wide_tests;

/// `f128` operations as calls of the test binary's own compiler_builtins routines, run
/// (blaze-lir legalize/quad.rs, routines.rs).
#[cfg(all(test, target_arch = "aarch64"))]
mod quad_run_tests;

/// `asm!` blocks: the template reader and the operands' registers (asm.rs, pass/asm.rs).
#[cfg(test)]
mod asm_tests;

/// The `asm!` mnemonic table against the A64 layouts (asm/table.rs).
#[cfg(test)]
mod asm_table_tests;

/// `asm!` rows run: CRC32, `fsqrt`, `tst`+`csel`, explicit registers, `cntvct_el0`.
#[cfg(all(test, target_arch = "aarch64"))]
mod asm_run_tests;


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

/// A routine of the system's C library or its compiler runtime the code calls (C convention,
/// AAPCS64: a 128-bit integer in an `x` register pair, low word first, starting at `x0` or
/// `x2` here, so Apple's and Linux's pair rules agree; a float in `d0`/`s0`, `d1`/`s1`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Routine {
    /// `memcpy(dst, src, len)`.
    Memcpy,
    /// `memmove(dst, src, len)`.
    Memmove,
    /// `memset(dst, byte, len)`.
    Memset,
    /// `memcmp(a, b, len) -> int`: `memcmp` past the inline bound (pass/bulk.rs); C's contract
    /// is the sign of the first differing byte pair compared as unsigned, 0 for `len == 0`.
    Memcmp,
    /// `fmod(f64, f64) -> f64`: `frem` at `f64` (the remainder takes the dividend's sign).
    Fmod,
    /// `fmodf(f32, f32) -> f32`: `frem` at `f32`.
    Fmodf,
    /// `__udivti3(u128, u128) -> u128`: `udiv` at `i128` (the divisor checked non-zero first).
    Udivti3,
    /// `__umodti3(u128, u128) -> u128`: `urem` at `i128`.
    Umodti3,
    /// `__divti3(i128, i128) -> i128`: `sdiv` at `i128` (zero and `MIN / -1` trapped first).
    Divti3,
    /// `__modti3(i128, i128) -> i128`: `srem` at `i128` (`MIN % -1` is 0, as LIR says).
    Modti3,
    /// `__floattidf(i128) -> f64`.
    Floattidf,
    /// `__floatuntidf(u128) -> f64`.
    Floatuntidf,
    /// `__floattisf(i128) -> f32`.
    Floattisf,
    /// `__floatuntisf(u128) -> f32`.
    Floatuntisf,
    /// `__fixdfti(f64) -> i128` (a `NaN` is made 0 before the call: pass/wide.rs).
    Fixdfti,
    /// `__fixunsdfti(f64) -> u128`.
    Fixunsdfti,
    /// `__fixsfti(f32) -> i128`.
    Fixsfti,
    /// `__fixunssfti(f32) -> u128`.
    Fixunssfti,
    /// `_Unwind_Resume(exception)`: a landing block's `resume` (pass/unwind.rs); it does not
    /// return. macOS: libSystem (its libunwind); Linux: libgcc_s.
    UnwindResume,
}

impl Routine {
    /// The symbol placement binds, as C writes it (`dlsym` adds Mach-O's leading underscore).
    /// macOS: libSystem (`libsystem_m` for `fmod`/`fmodf`, `libcompiler_rt` for the `__*ti*`
    /// family); Linux: libc, libm (`fmod`, `fmodf`) and libgcc_s (the `__*ti*` family).
    pub fn symbol(self) -> &'static str {
        match self {
            Routine::Memcpy => "memcpy",
            Routine::Memmove => "memmove",
            Routine::Memset => "memset",
            Routine::Memcmp => "memcmp",
            Routine::Fmod => "fmod",
            Routine::Fmodf => "fmodf",
            Routine::Udivti3 => "__udivti3",
            Routine::Umodti3 => "__umodti3",
            Routine::Divti3 => "__divti3",
            Routine::Modti3 => "__modti3",
            Routine::Floattidf => "__floattidf",
            Routine::Floatuntidf => "__floatuntidf",
            Routine::Floattisf => "__floattisf",
            Routine::Floatuntisf => "__floatuntisf",
            Routine::Fixdfti => "__fixdfti",
            Routine::Fixunsdfti => "__fixunsdfti",
            Routine::Fixsfti => "__fixsfti",
            Routine::Fixunssfti => "__fixunssfti",
            Routine::UnwindResume => "_Unwind_Resume",
        }
    }
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
    /// The program's thread-local getter, `fn(desc, index, root_at) -> copy` (C convention).
    TlsGetter,
    /// Not an address: the run's index of thread-local `place` (`RefKind::Tls`) in the run's
    /// thread-local table (blaze-build `tls::index`): the word holds the index plus `addend`.
    TlsIndex(u32),
}

/// An 8-byte word of the code that placement fills: the address of `to` plus `addend`,
/// little-endian (for [`RelocTo::TlsIndex`], the index plus `addend`). A literal word the
/// code loads (`data_addr` of a static, a routine, a thread-local's descriptor, getter and
/// index, `func_addr` of a slot function; `addend` 0), and each relocation of the function's
/// own data rows (`DataRelocRec`), are one each.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Reloc {
    /// The word's offset from the code's first byte (8-byte aligned).
    pub at: u32,
    pub to: RelocTo,
    pub addend: i64,
}

/// A 4-byte field of the code that placement fills with a source position (rule 71: the code
/// holds zero there, and the item and the byte in its text, never a line or a column): the
/// line or the column (each from 1, the column in chars) of byte `site` of item `item`'s
/// text, little-endian. A data row's `DataPositionRec`, placed with the row.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PositionFix {
    /// The field's offset from the code's first byte (4-byte aligned only if its row put it so).
    pub at: u32,
    /// The item's key (blaze-typeck `ItemKey`).
    pub item: u128,
    /// The byte offset in the item's text.
    pub site: u32,
    pub field: blaze_lir::PositionField,
}

/// One function's machine code.
pub struct Code<A: Allocator> {
    pub bytes: Vec<u8, A>,
    pub fixups: Vec<Fixup, A>,
    /// Absolute address words placement fills (after the fixups, in any order).
    pub relocs: Vec<Reloc, A>,
    /// Source position fields placement fills, by offset.
    pub positions: Vec<PositionFix, A>,
    /// log2 of the alignment placement gives `bytes[0]`: at least [`CODE_ALIGN`] (16), and
    /// the most any data row of the function asks (up to a 4 KiB page), so a row aligned to 64
    /// is at an address that is a multiple of 64.
    pub align: u8,
}

/// The least alignment, as log2, of a literal-word form's code: 16 bytes.
pub const CODE_ALIGN: u8 = 4;

impl<A: Allocator + Clone> Code<A> {
    pub fn new_in(alloc: A) -> Code<A> {
        Code {
            align: CODE_ALIGN,
            bytes: Vec::new_in(alloc.clone()),
            fixups: Vec::new_in(alloc.clone()),
            relocs: Vec::new_in(alloc.clone()),
            positions: Vec::new_in(alloc),
        }
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
    /// The record form ([`emit_record`]) has no import site or relro word for something the
    /// function names: refused, never emitted as a literal word.
    Record { why: &'static str },
    /// blaze-lir's legalize refused the function before the pass: the literal-word form
    /// ([`emit`]) takes legalized LIR as the record form does (a vector made scalar, `f16`
    /// widened), and binds no soft-float routine, so an `f128` operation is refused here naming
    /// its routine ([`blaze_lir::legalize::LegalizeError::NoRoutine`]).
    Legalize(blaze_lir::legalize::LegalizeError),
    /// The template of the `asm` at instruction `inst` (asm.rs): line `line` of it (from 1) is
    /// not assembled, for `why`, at `token` (a mnemonic, a directive, a label or an operand).
    Asm { inst: u32, line: u32, why: &'static str, token: AsmToken },
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
            EmitError::Record { why } => write!(f, "the code record form (run.md, \"The code record\") does not emit {why}"),
            EmitError::Legalize(why) => why.fmt(f),
            EmitError::Asm { inst, line, why, token } => {
                write!(f, "instruction {inst} (`asm`): line {line} of its template is not assembled: {why}: {token}")
            }
        }
    }
}

impl core::error::Error for EmitError {}

/// Emits `func` into `out`, which is cleared first, for a core and system `isa` says. `func`
/// is the proof `blaze_lir::verify` answered (or a `VerifiedFunc`'s): unchecked LIR cannot be
/// handed in, and nothing here checks it again (D163, rule 74). `via` has one entry per
/// function reference place the function names.
pub fn emit<A: Allocator>(func: Verified<'_>, via: &[Via], isa: Isa, out: &mut Code<A>) -> Result<(), EmitError> {
    emit_inline_up_to(func, via, &RoutinePlaces::NONE, isa, BULK_INLINE_MOST, out).map(|_| ())
}

/// [`emit`] with the soft-float routines `routines` binds (routines.rs): an `f128` (or `f16`)
/// operation becomes a call of its routine at the place `routines` gives it, which `via` covers
/// like every other reference place (the caller appends one place per bound routine to its
/// references, as the record path does, front2/builtins_shim.rs `Bound::bind`). Answers the
/// routines the code calls. The one rule of both paths: the in-process build and the record
/// emitter legalize with the same table.
pub fn emit_with_routines<A: Allocator>(func: Verified<'_>, via: &[Via], routines: &RoutinePlaces, isa: Isa, out: &mut Code<A>) -> Result<Called, EmitError> {
    emit_inline_up_to(func, via, routines, isa, BULK_INLINE_MOST, out)
}

/// [`emit`], inlining a bulk operation of a constant length up to `inline_most` bytes (the
/// measurement compares both sides of [`BULK_INLINE_MOST`]).
pub(crate) fn emit_inline_up_to<A: Allocator>(
    func: Verified<'_>,
    via: &[Via],
    bound: &RoutinePlaces,
    isa: Isa,
    inline_most: u64,
    out: &mut Code<A>,
) -> Result<Called, EmitError> {
    emit_reaching(func, via, bound, isa, inline_most, pass::Reach::Islands, out)
}

/// [`emit_inline_up_to`] with the pass's `reach` (pass/reach.rs): [`pass::Reach::ShortOnly`]
/// is the reach tests' control, the pass with no island and no long form.
pub(crate) fn emit_reaching<A: Allocator>(
    func: Verified<'_>,
    via: &[Via],
    bound: &RoutinePlaces,
    isa: Isa,
    inline_most: u64,
    reach: pass::Reach,
    out: &mut Code<A>,
) -> Result<Called, EmitError> {
    // Below the entry the pass reads the view; the proof was the entry's to take.
    let func = func.view();
    out.bytes.clear();
    out.fixups.clear();
    out.relocs.clear();
    out.positions.clear();
    // The pass takes legalized LIR in both forms (record.rs `emit_record_up_to` does the same
    // with the caller's routines): a function holding a vector, `f16` or `f128` value is made
    // scalar on a copy first, in `Global` (the scratch the pass names for its own rows). Each
    // bound routine's reference row names a place the caller gave (`bound`), so `via` (one
    // entry per reference place) still covers every reference of the copy.
    let heap = blaze_own::Erased::new(alloc::alloc::Global);
    let (legal, called) = routines::legalized(func, bound, heap.handle()).map_err(EmitError::Legalize)?;
    let func = match &legal {
        None => func,
        Some(owned) => owned.view(),
    };
    // The pass's own rows (the pending short fields, the islands' copies) in the same scratch.
    pass::run(func, via, isa, inline_most, &mut out.bytes, &mut out.fixups, &mut out.relocs, &mut out.positions, None, reach, heap.handle())?;
    // What the code asks of its placement: the most any of its data rows asks (each is placed
    // at a multiple of its own alignment from the code's first byte), at least 16. Over every
    // row rather than the placed ones: a row the code never reaches only over-aligns it.
    // A row past a page was refused by the pass if placed, and is capped if not.
    let page = blaze_code::record::MOST_PART_ALIGN;
    out.align = func.datas.iter().fold(CODE_ALIGN, |most, d| most.max(d.align.min(u32::from(page)) as u8));
    Ok(called)
}
