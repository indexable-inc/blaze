//! Which tier compiles an instance first (INTRINSICS.md 3.3, P8), by what the instance holds.
//!
//! - A target intrinsic, an `asm!` block (`Op::TargetIntrinsic`, `Op::Asm`, `Op::AsmBr`), or a
//!   target feature beyond the target's base set (the LIR `features` column): Optimized. The
//!   Reference and Fast tiers emit base code only, and LIR's evaluator has no semantics for
//!   these, so no Fast template can be admitted for them (D100.14). Until the Optimized tier
//!   exists, its caller stops such an instance at its first call with
//!   `TierError::Refused { tier: Reference, what: "a target intrinsic" }`: no other route.
//! - Otherwise a dependency's instance: Reference, never Fast (D100.12: shared instances never
//!   pass through Fast; an Optimized record of it from the shared cache is a lookup made before
//!   any tier runs, not this route's choice).
//! - Otherwise: Fast.
//!
//! The `features` column is LLVM's `"target-features"` form (`+avx2,+fma`, blaze-lir func.rs).
//! A function's set is within base when every entry is `+<name>` with `<name>` among the LLVM
//! names [`base`] turns on. Anything else (a name of no base feature, a `-` entry turning a base
//! feature off, an entry of no form) is beyond base and goes to Optimized, whose LLVM reads the
//! string as written and refuses what it does not know there, by name.

use blaze_lir::{Op, View};

use crate::{Arch, Machine, TierName, features::base};

/// The tier that compiles `func` first on `machine`; `dependency` when the instance is of a
/// dependency (or shared) rather than of the workspace.
///
/// Recommended hot-path pipeline on AArch64: `Fast` (stencils, microseconds cold start + PGO
/// profiling) -> [`optimizing_tier`] (`TierName::Sota` by default on AArch64, no LLVM `-O3`).
pub fn first_tier(func: View<'_>, machine: Machine, dependency: bool) -> TierName {
    let target_intrinsic = Op::TargetIntrinsic as u8;
    let asm = Op::Asm as u8;
    let asm_br = Op::AsmBr as u8;
    if func.ops.iter().any(|&op| op == target_intrinsic || op == asm || op == asm_br) {
        return TierName::Optimized;
    }
    if !within_base(func.features, machine) {
        return TierName::Optimized;
    }
    if dependency { TierName::Reference } else { TierName::Fast }
}

/// The default optimizing tier for tier-up (from `Fast` stencils) and release/export builds on
/// `machine`.
///
/// Routes through Blaze's native non-LLVM [`TierName::Sota`] tier by default on both AArch64 and
/// x86-64, falling back to [`TierName::Optimized`] only when the function requires raw `asm!`,
/// target intrinsics, or non-base target features (e.g. `+sve`, `+avx512f`).
pub fn optimizing_tier(func: View<'_>, machine: Machine) -> TierName {
    let target_intrinsic = Op::TargetIntrinsic as u8;
    let asm = Op::Asm as u8;
    let asm_br = Op::AsmBr as u8;
    if func.ops.iter().any(|&op| op == target_intrinsic || op == asm || op == asm_br) {
        return TierName::Optimized;
    }
    if !within_base(func.features, machine) {
        return TierName::Optimized;
    }
    match machine.arch {
        Arch::Arm64 | Arch::X86_64 => TierName::Sota,
    }
}

/// Whether the LLVM feature string `features` turns on only what `machine`'s base set has.
fn within_base(features: &[u8], machine: Machine) -> bool {
    if features.is_empty() {
        return true;
    }
    let base = base(machine.arch, machine.os);
    features.split(|&b| b == b',').all(|entry| match entry.split_first() {
        Some((b'+', name)) => base.llvm_names().any(|it| it.as_bytes() == name),
        _ => false,
    })
}
