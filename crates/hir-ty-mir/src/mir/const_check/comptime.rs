//! The nightly's rule for a compile-time-only function (`#[rustc_comptime]`: `TypeId::info`
//! and the rest of `core::mem::type_info`, the reflection intrinsics, `size_of`): only what
//! runs at compile time may call one, which is the value of a `const`, a `static` or an enum
//! discriminant, a `const { .. }` block, another anonymous constant, or the body of another
//! such function. rustc refuses any other call with "comptime functions can only be called at
//! compile time", in a `const fn` too, since a `const fn` may be called at run time.
//!
//! No code is compiled for such a function, so a call this check let through was refused only
//! when the crate was built, as the intrinsic "which has no body to run" at the end of the
//! chain, with nothing naming the call that was wrong (bstd's ownership walk, 2026-11-12).
//!
//! The check reads the MIR lowering builds, where a `const { .. }` block of a function is
//! already a constant of its own and not a call of the function: what is left in the body, and
//! in the bodies of its closures, runs when the function does.

use base_db::Db;
use hir_def::{CallableDefId, DefWithBodyId, FunctionId, attrs::AttrFlags};
use hir_ty::{
    InferBodyId, InferenceResult,
    db::{HirDatabase, InternedClosureId},
    next_solver::TyKind,
};

use super::{MirBody, MirSpan, TerminatorKind, operand_ty};

/// rustc's message, which has no error code.
pub const MESSAGE: &str = "comptime functions can only be called at compile time";

/// A call of the compile-time-only `callee` in a body that runs.
#[derive(Debug, Clone, PartialEq, Eq, blaze_query::Value)]
pub struct ComptimeCall {
    pub callee: FunctionId,
    /// The call, in the expression store of the function it is written in (a closure's
    /// expressions are its function's).
    pub span: MirSpan,
}

/// The calls of compile-time-only functions in the body of `def` and of its closures that are
/// not at compile time: none unless `def` is a function, and none in a function that is
/// itself `#[rustc_comptime]`.
#[derive(Debug, Clone, PartialEq, Eq, Default, blaze_query::Value)]
pub struct ComptimeCalls {
    pub calls: Vec<ComptimeCall>,
}

/// Whether `func` is `#[rustc_comptime]`.
pub fn is_comptime(db: &Db, func: FunctionId) -> bool {
    AttrFlags::query(db, func.into()).contains(AttrFlags::RUSTC_COMPTIME)
}

/// What rustc refuses as a compile-time-only function called where the call would run. A body
/// inference refused is not looked at: its calls are not known.
#[blaze_query::tracked(returns(ref))]
pub fn comptime_calls<'db>(db: &'db Db, def: DefWithBodyId) -> ComptimeCalls {
    let DefWithBodyId::FunctionId(func) = def else { return ComptimeCalls::default() };
    if is_comptime(db, func) {
        return ComptimeCalls::default();
    }
    let owner = InferBodyId::DefWithBodyId(def);
    let infer = InferenceResult::of(db, owner);
    if infer.has_type_mismatches() || infer.is_erroneous() || !infer.diagnostics().is_empty() {
        return ComptimeCalls::default();
    }
    let Ok(body) = db.mir_body(owner) else { return ComptimeCalls::default() };
    let mut calls = Vec::new(); // alloc: none until an error
    in_body(db, body, &mut calls);
    // A closure's closures are in its own body's list; the list of ones still to read is
    // empty for nearly every function.
    let mut closures: Vec<InternedClosureId<'db>> = body.closures.clone(); // clone: ids, none for most bodies
    while let Some(closure) = closures.pop() {
        let Ok(body) = db.mir_body_for_closure(closure) else { continue };
        in_body(db, body, &mut calls);
        closures.extend_from_slice(&body.closures);
    }
    ComptimeCalls { calls }
}

fn in_body(db: &Db, body: &MirBody<'_>, calls: &mut Vec<ComptimeCall>) {
    for (_, block) in body.basic_blocks.iter() {
        let Some(terminator) = &block.terminator else { continue };
        let (TerminatorKind::Call { func, .. } | TerminatorKind::TailCall { func, .. }) =
            &terminator.kind
        else {
            continue;
        };
        let Some(TyKind::FnDef(callee, _)) = operand_ty(body, func) else { continue };
        let CallableDefId::FunctionId(callee) = callee.0 else { continue };
        // What lowering made and nobody wrote has no place to report, and is lowering's to
        // get right.
        if is_comptime(db, callee) && matches!(terminator.span, MirSpan::ExprId(_)) {
            calls.push(ComptimeCall { callee, span: terminator.span });
        }
    }
}
