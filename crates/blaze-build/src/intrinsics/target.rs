//! Target intrinsics: the closed set of `llvm.*` link names every backend implements
//! (THREADS.md D1; one home for the set, rule 30).
//!
//! The library reaches the processor's barrier and hint instructions through foreign fns of
//! an `"unadjusted"` block named by `link_name`: the pinned library's stdarch
//! `arm_shared/barrier/mod.rs` declares
//!
//! ```text
//! #[cfg_attr(any(target_arch = "aarch64", target_arch = "arm64ec"), link_name = "llvm.aarch64.dmb")]
//! fn dmb(_: i32);
//! ... link_name = "llvm.aarch64.dsb" ... fn dsb(_: i32);
//! ... link_name = "llvm.aarch64.isb" ... fn isb(_: i32);
//! ```
//!
//! and `arm_shared/hints.rs` `link_name = "llvm.aarch64.hint"` `fn hint(_: i32)`; core's
//! `hint::spin_loop` (core/src/hint.rs: "Emits a machine instruction to signal the processor
//! that it is running in a busy-wait spin-loop") is `crate::arch::aarch64::__isb(SY)` on
//! aarch64. Each is `fn(i32 imm)` returning nothing, the immediate a constant (the library
//! passes `arg::SY` and the like). An `llvm.*` name outside [`ALL`] is refused by name
//! ([`UnknownTarget`]), never called as a C function. The builder emits LIR's existing
//! `target_intrinsic` operation with the immediate as its one operand; the emitter encodes it
//! (blaze-emit pass/target.rs) and the reference evaluator gives it no effect (blaze-lir
//! eval/target.rs). It is not a fence: `Op::Fence` stays the atomic fence.

use core::alloc::Allocator;
use core::fmt;

use blaze_lir::builder::{Param, Variadic};
use blaze_lir::func::Conv;
use blaze_lir::{Builder, Full, Op, Ty, Value};

use crate::BuildError;

pub mod float;
#[cfg(test)]
mod float_tests;
pub mod neon;
#[cfg(test)]
mod neon_tests;
pub mod sha2;
#[cfg(test)]
mod sha2_tests;
pub mod sha3;
#[cfg(test)]
mod sha3_tests;
#[cfg(test)]
mod tests;

/// A target intrinsic blaze builds: a barrier or hint, `fn(i32 imm)` returning nothing (the
/// four below), a generic float function, built as its LIR float operation
/// ([`Target::Float`], target/float.rs), a NEON integer function, built as LIR vector
/// operations ([`Target::Neon`], target/neon.rs), or a SHA3 bitwise function, built as LIR
/// vector operations too ([`Target::Sha3`], target/sha3.rs).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Target {
    /// `llvm.<fn>.<shape>` of target/float.rs (`llvm.roundeven.v4f32` and kin): LIR's float
    /// operation lane by lane, no `target_intrinsic` (INTRINSICS.md 3.3: "a generic name with
    /// an LIR op maps to it").
    Float(float::FloatCall),
    /// `llvm.aarch64.neon.<fn>.<shape>` of target/neon.rs (`llvm.aarch64.neon.tbl1.v16i8`,
    /// `llvm.aarch64.neon.ushl.v2i64` and kin): LIR vector operations, no `target_intrinsic`.
    Neon(neon::NeonCall),
    /// `llvm.aarch64.crypto.{eor3,bcax}{u,s}.<shape>`, `llvm.aarch64.crypto.rax1` and
    /// `llvm.aarch64.crypto.xar` of target/sha3.rs: LIR vector operations, no
    /// `target_intrinsic`.
    Sha3(sha3::Sha3Call),
    /// `llvm.aarch64.crypto.sha256{h,h2,su0,su1}` and `llvm.aarch64.crypto.sha512{h,h2,su0,su1}`
    /// of target/sha2.rs: LIR scalar operations on the lanes, no `target_intrinsic`.
    Sha2(sha2::Sha2Fn),
    /// `llvm.aarch64.isb`: instruction synchronization barrier, `isb #imm`.
    Aarch64Isb,
    /// `llvm.aarch64.dmb`: data memory barrier, `dmb #imm`.
    Aarch64Dmb,
    /// `llvm.aarch64.dsb`: data synchronization barrier, `dsb #imm`.
    Aarch64Dsb,
    /// `llvm.aarch64.hint`: a hint instruction, `hint #imm` (`yield` is `hint #1`).
    Aarch64Hint,
}

/// Every barrier and hint, in declaration order (the float functions are target/float.rs's
/// `FNS` at its `SHAPES`).
pub const ALL: [Target; 4] = [Target::Aarch64Isb, Target::Aarch64Dmb, Target::Aarch64Dsb, Target::Aarch64Hint];

/// The most bytes of an unknown name [`UnknownTarget`] keeps.
const KEPT: usize = 64;

/// An `llvm.*` link name that is not a target intrinsic blaze knows. Holds the name's first
/// [`KEPT`] bytes (cut at a character boundary) so the error stays `Copy` and borrows nothing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct UnknownTarget {
    bytes: [u8; KEPT],
    len: u8,
    /// The name was longer than what is kept.
    cut: bool,
}

impl UnknownTarget {
    fn of(name: &str) -> UnknownTarget {
        let mut len = name.len().min(KEPT);
        // A work list of at most four steps: a UTF-8 character is at most four bytes.
        while !name.is_char_boundary(len) {
            len -= 1;
        }
        let mut bytes = [0u8; KEPT];
        bytes[..len].copy_from_slice(&name.as_bytes()[..len]);
        UnknownTarget { bytes, len: len as u8, cut: len < name.len() }
    }

    /// The kept part of the name.
    pub fn name(&self) -> &str {
        // The bytes are a prefix of a `&str` cut at a character boundary (`of`).
        core::str::from_utf8(&self.bytes[..usize::from(self.len)]).expect("a prefix cut at a character boundary")
    }
}

impl fmt::Display for UnknownTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let more = if self.cut { "..." } else { "" };
        write!(f, "`{}{more}` is not a target intrinsic blaze knows", self.name())
    }
}

impl core::error::Error for UnknownTarget {}

/// The target intrinsic whose link name is exactly `link_name`.
pub fn named(link_name: &str) -> Result<Target, UnknownTarget> {
    if let Some(call) = float::named(link_name) {
        return Ok(Target::Float(call));
    }
    if let Some(call) = neon::named(link_name) {
        return Ok(Target::Neon(call));
    }
    if let Some(call) = sha3::named(link_name) {
        return Ok(Target::Sha3(call));
    }
    if let Some(f) = sha2::named(link_name) {
        return Ok(Target::Sha2(f));
    }
    ALL.iter().copied().find(|t| t.link_name() == link_name).ok_or_else(|| UnknownTarget::of(link_name))
}

impl Target {
    /// The `link_name` the library declares it by.
    pub fn link_name(self) -> &'static str {
        match self {
            Target::Aarch64Isb => "llvm.aarch64.isb",
            Target::Aarch64Dmb => "llvm.aarch64.dmb",
            Target::Aarch64Dsb => "llvm.aarch64.dsb",
            Target::Aarch64Hint => "llvm.aarch64.hint",
            Target::Float(call) => call.link_name(),
            Target::Neon(call) => call.link_name(),
            Target::Sha3(call) => call.link_name(),
            Target::Sha2(f) => f.link_name(),
        }
    }
}

/// Builds a call of `t` with immediate `imm`: declares the intrinsic once per function (the
/// builder interns it by name and signature) as `fn(i32)` in `Conv::C`, then emits
/// `target_intrinsic`. `imm` must be an `i32` made by `iconst`: every backend encodes the
/// immediate in the instruction, so any other value is refused by name here, before a
/// backend sees it.
pub fn build<A: Allocator + Clone>(b: &mut Builder<A, Full>, t: Target, imm: Value) -> Result<(), BuildError> {
    // A float function is its LIR operation, built by the walk (walk/target_intr.rs), never a
    // `target_intrinsic` of `fn(i32)`.
    match t {
        Target::Float(_) => {
            return Err(BuildError::Target { target: t, expr: None, why: "a generic float function built as a barrier (a build bug)" });
        }
        Target::Neon(_) => {
            return Err(BuildError::Target { target: t, expr: None, why: "a NEON integer function built as a barrier (a build bug)" });
        }
        Target::Sha3(_) => {
            return Err(BuildError::Target { target: t, expr: None, why: "a SHA3 function built as a barrier (a build bug)" });
        }
        Target::Sha2(_) => {
            return Err(BuildError::Target { target: t, expr: None, why: "a SHA-2 function built as a barrier (a build bug)" });
        }
        Target::Aarch64Isb | Target::Aarch64Dmb | Target::Aarch64Dsb | Target::Aarch64Hint => {}
    }
    // `ty` answers `Ty::NONE` for a value past the function's, so the index below is in range
    // once the type is `i32`.
    if b.ty(imm) != Ty::I32 {
        return Err(BuildError::Target { target: t, expr: None, why: "an immediate that is not an `i32`" });
    }
    if b.func().view().op(imm.index()) != Op::Iconst {
        return Err(BuildError::Target { target: t, expr: None, why: "an immediate that is not a constant" });
    }
    let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I32)], &[]);
    let id = b.intrinsic(t.link_name(), sig);
    b.target_intrinsic(id, &[imm]);
    Ok(())
}
