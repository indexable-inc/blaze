//! An evaluator's error in words a person reads: what stopped it, each function by its path,
//! each place as `file:line:column`, a call that has no function by its callee as written and
//! why inference gave it none, then the frames it stopped in, innermost first.

use std::fmt::Write as _;

use base_db::{Crate, Db};
use blaze_items::lowering::{self, body_path, file_place, owner_path, place};
use hir_def::{DefWithBodyId, HasModule};
use hir_ty::{
    display::DisplayTarget,
    mir::{Budget, MirEvalError, MirLowerError, MirSpan},
};
use hir_ty_mir::mir_pretty::MirLowerErrorPretty;

/// `error` in words. Its types print through the attached database, so the caller attaches
/// `db`.
pub(crate) fn describe(db: &Db, error: &MirEvalError<'_>) -> String {
    // alloc: error path
    let mut out = String::from("stopped in the evaluator: ");
    // alloc: error path, the stacks from the outermost run in
    let mut stacks = Vec::new();
    let mut leaf = error;
    while let MirEvalError::InFunction(inner, stack) = leaf {
        stacks.push(stack);
        leaf = inner;
    }
    match leaf {
        MirEvalError::MirLowerError(func, lower) => {
            let def = DefWithBodyId::FunctionId(*func);
            _ = write!(out, "the MIR of `{}` could not be built: ", body_path(db, def));
            lowering(db, lower, def.krate(db), &mut out);
        }
        MirEvalError::MirLowerErrorForClosure(closure, lower) => {
            let loc = closure.loc(db);
            _ = write!(
                out,
                "the MIR of the closure at {} in `{}` could not be built: ",
                place(db, loc.owner, MirSpan::ExprId(loc.expr)),
                owner_path(db, loc.owner),
            );
            lowering(db, lower, loc.owner.expression_store_owner(db).krate(db), &mut out);
        }
        MirEvalError::OverBudget { spent, at } => {
            match spent {
                Budget::Steps(steps) => {
                    _ = write!(
                        out,
                        "the expansion ran {steps} steps (basic blocks), all one expansion may, without finishing"
                    )
                }
                Budget::Memory(bytes) => {
                    _ = write!(
                        out,
                        "the expansion would use more than {bytes} bytes of memory, all one expansion may"
                    )
                }
            }
            if let Some((owner, span)) = *at {
                _ = write!(out, "; it was in `{}`", owner_path(db, owner));
                if !matches!(span, MirSpan::Unknown) {
                    _ = write!(out, " at {}", place(db, owner, span));
                }
            }
        }
        other => _ = write!(out, "{other:?}"),
    }
    for stack in stacks.iter().rev() {
        for &(func, span, caller) in stack.iter() {
            match func.left() {
                Some(func) => _ = write!(out, "\n  in `{}`", body_path(db, func.into())),
                None => out.push_str("\n  in a closure"),
            }
            if !matches!(span, MirSpan::Unknown) {
                _ = write!(
                    out,
                    ", called from `{}` at {}",
                    owner_path(db, caller),
                    place(db, caller, span)
                );
            }
        }
    }
    out
}

/// What lowering `lower` hit. A call with no function names its callee and, where inference
/// recorded one, why it resolved to nothing (`blaze_items::lowering`).
fn lowering(db: &Db, lower: &MirLowerError<'_>, krate: Crate, out: &mut String) {
    let MirLowerError::MissingFunctionDefinition(owner, call) = lower else {
        // alloc: error path
        let mut text = String::new();
        let target = DisplayTarget::from_crate(db, krate);
        match lower.pretty_print(&mut text, db, |file, range| file_place(db, file, range), target) {
            Ok(()) => out.push_str(text.trim_end()),
            Err(_) => _ = write!(out, "{lower:?}"),
        }
        return;
    };
    lowering::missing_function(db, *owner, *call, out);
}

#[cfg(test)]
mod tests;
