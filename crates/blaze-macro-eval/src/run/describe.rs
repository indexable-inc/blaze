//! An evaluator's error in words a person reads: what stopped it, each function by its path,
//! each place as `file:line:column`, a call that has no function by its callee as written and
//! why inference gave it none, then the frames it stopped in, innermost first.

use std::fmt::Write as _;

use base_db::{Crate, Db};
use hir_def::{
    DefWithBodyId, ExpressionStoreOwnerId, HasModule,
    expr_store::{ExpressionStore, pretty::print_expr_hir},
    hir::{Expr, ExprId, ExprOrPatIdPacked},
    span_names,
};
use hir_expand::InFile;
use hir_ty::{
    InferBodyId, InferenceDiagnostic, InferenceResult,
    display::{DisplayTarget, HirDisplay},
    mir::{Budget, MirEvalError, MirLowerError, MirSpan},
};
use hir_ty_mir::mir_pretty::MirLowerErrorPretty;
use span::{FileId, TextRange};

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
                Budget::Steps(steps) => _ = write!(
                    out,
                    "the expansion ran {steps} steps (basic blocks), all one expansion may, without finishing"
                ),
                Budget::Memory(bytes) => _ = write!(
                    out,
                    "the expansion would use more than {bytes} bytes of memory, all one expansion may"
                ),
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
                _ = write!(out, ", called from `{}` at {}", owner_path(db, caller), place(db, caller, span));
            }
        }
    }
    out
}

/// What lowering `lower` hit. A call with no function names its callee and, where inference
/// recorded one, why it resolved to nothing.
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
    let store_owner = owner.expression_store_owner(db);
    let store = ExpressionStore::of(db, store_owner);
    let callee = match store.expr(*call) {
        Expr::Call { callee, .. } => callee,
        _ => *call,
    };
    let edition = DisplayTarget::from_crate(db, store_owner.krate(db)).edition;
    _ = write!(
        out,
        "the call of `{}` at {} has no function: ",
        print_expr_hir(db, store, store_owner, callee, edition),
        place(db, *owner, MirSpan::ExprId(*call)),
    );
    why_unresolved(db, *owner, callee, out);
}

/// Why inference of `owner` resolved the path `callee` to nothing, as it recorded it.
fn why_unresolved(db: &Db, owner: InferBodyId<'_>, callee: ExprId, out: &mut String) {
    let at = ExprOrPatIdPacked::from(callee);
    let target = DisplayTarget::from_crate(db, owner.expression_store_owner(db).krate(db));
    for diagnostic in InferenceResult::of(db, owner).diagnostics() {
        match diagnostic {
            InferenceDiagnostic::UnresolvedAssocItem { id, ty, name } if *id == at => {
                _ = write!(
                    out,
                    "inference found no associated item `{}` on `{}`",
                    name.display(db, target.edition),
                    ty.as_ref().display(db, target),
                );
                return;
            }
            InferenceDiagnostic::UnresolvedPath { id, segment } if *id == at => {
                _ = write!(out, "its path resolves to nothing from its segment {segment}");
                return;
            }
            InferenceDiagnostic::UnresolvedIdent { id } if *id == at => {
                out.push_str("its name resolves to nothing");
                return;
            }
            _ => {}
        }
    }
    out.push_str("inference gave its callee no type and recorded no reason");
}

/// `owner` by its path: a body's `crate::module::item`, a constant in a signature by its crate.
fn owner_path(db: &Db, owner: InferBodyId<'_>) -> String {
    match (owner, owner.expression_store_owner(db)) {
        (InferBodyId::DefWithBodyId(def), _) => body_path(db, def),
        (InferBodyId::AnonConstId(_), ExpressionStoreOwnerId::Body(def)) => {
            let mut path = body_path(db, def);
            path.push_str("::{const}");
            path
        }
        (InferBodyId::AnonConstId(_), store_owner) => {
            // alloc: error path
            format!("a constant of `{}`", span_names::crate_name(db, store_owner.krate(db)))
        }
    }
}

fn body_path(db: &Db, def: DefWithBodyId) -> String {
    // alloc: error path
    let mut path = String::new();
    span_names::write_body_path(db, def, &mut |it| path.push_str(it));
    path
}

/// Where `span` of `owner`'s body was written, `file:line:column`; in a macro's output, where
/// the macro was called.
fn place(db: &Db, owner: InferBodyId<'_>, span: MirSpan) -> String {
    let (_, source_map) = ExpressionStore::with_source_map(db, owner.expression_store_owner(db));
    let source: Option<InFile<TextRange>> = match span {
        MirSpan::ExprId(it) | MirSpan::ExprEnd(it) => source_map.expr_syntax(it).ok().map(|it| it.map(|it| it.text_range())),
        MirSpan::PatId(it) => source_map.pat_syntax(it).ok().map(|it| it.map(|it| it.text_range())),
        MirSpan::BindingId(_) | MirSpan::SelfParam | MirSpan::Unknown | MirSpan::Inlined(_) => None,
    };
    match source {
        Some(source) => {
            let range = source.original_node_file_range_rooted(db);
            file_place(db, range.file_id.file_id(db), range.range)
        }
        // alloc: error path
        None => format!("{span:?} of `{}`", owner_path(db, owner)),
    }
}

/// `range` of `file` as `path:line:column`, both counted from 1.
fn file_place(db: &Db, file: FileId, range: TextRange) -> String {
    let start = usize::from(range.start());
    let text = db.file_text(file);
    let before = text.text(db).get(..start).unwrap_or_default();
    let line = before.matches('\n').count() + 1;
    let column = before.len() - before.rfind('\n').map_or(0, |it| it + 1) + 1;
    match db.path_for_file(file) {
        // alloc: error path
        Some(path) => format!("{path}:{line}:{column}"),
        // alloc: error path
        None => format!("{{file {}}}:{line}:{column}", file.index()),
    }
}

#[cfg(test)]
mod tests;
