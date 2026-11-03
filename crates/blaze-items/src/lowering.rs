//! A MIR lowering error in words a person reads, for the error a build stops with when an item
//! has no hash ([`crate::Unhashable::describe`]) and for the evaluator's stops: a call with no
//! function names its callee as written, where it is (`file:line:column`) and why inference
//! resolved it to nothing; any other error is its `Debug`.

use std::fmt::Write as _;

use base_db::Db;
use hir_def::{
    DefWithBodyId, HasModule,
    expr_store::{ExpressionStore, pretty::print_expr_hir},
    hir::{Expr, ExprId, ExprOrPatIdPacked},
    span_names,
};
use hir_expand::InFile;
use hir_ty::{
    InferBodyId, InferenceDiagnostic, InferenceResult,
    display::{DisplayTarget, HirDisplay},
    mir::{MirLowerError, MirSpan},
};
use span::{FileId, TextRange};

/// `error` in words, written to `out`. Types print through the attached database, so this
/// attaches `db`.
pub fn write(db: &Db, error: &MirLowerError<'_>, out: &mut String) {
    match error {
        MirLowerError::MissingFunctionDefinition(owner, call) => {
            hir_ty::attach_db(db, || missing_function(db, *owner, *call, out));
        }
        other => _ = write!(out, "{other:?}"),
    }
}

/// The call `call` of `owner`'s body, which lowering found no function for: its callee as
/// written, where the call is, and why inference resolved the callee to nothing. The caller
/// attaches `db`.
pub fn missing_function(db: &Db, owner: InferBodyId<'_>, call: ExprId, out: &mut String) {
    let store_owner = owner.expression_store_owner(db);
    let store = ExpressionStore::of(db, store_owner);
    let callee = match store.expr(call) {
        Expr::Call { callee, .. } => callee,
        _ => call,
    };
    let edition = DisplayTarget::from_crate(db, store_owner.krate(db)).edition;
    _ = write!(
        out,
        "the call of `{}` at {} has no function: ",
        print_expr_hir(db, store, store_owner, callee, edition),
        place(db, owner, MirSpan::ExprId(call)),
    );
    why_unresolved(db, owner, callee, out);
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

/// `owner` by its path, as its query's span names it: a body's `crate::module::item`, a constant
/// `item::{const#N}` under the item it is written in.
pub fn owner_path(db: &Db, owner: InferBodyId<'_>) -> String {
    match owner {
        InferBodyId::DefWithBodyId(def) => body_path(db, def),
        InferBodyId::AnonConstId(it) => {
            let loc = it.loc(db);
            // alloc: error path
            let mut path = String::new();
            span_names::write_inner_path(db, loc.owner, loc.expr, span_names::InnerBody::Const, &mut path);
            path
        }
    }
}

/// `def` by its path, `crate::module::item`.
pub fn body_path(db: &Db, def: DefWithBodyId) -> String {
    // alloc: error path
    let mut path = String::new();
    span_names::write_body_path(db, def, &mut path);
    path
}

/// Where `span` of `owner`'s body was written, `file:line:column`; in a macro's output, where
/// the macro was called.
pub fn place(db: &Db, owner: InferBodyId<'_>, span: MirSpan) -> String {
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

/// `range` of `file` as `path:line:column`, both counted from 1. Counted in the text the
/// compiler reads, so a place in text blaze appended to a crate's root (its wasm macro glue, a
/// test harness) is past the end of the file on disk.
pub fn file_place(db: &Db, file: FileId, range: TextRange) -> String {
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
