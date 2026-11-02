//! E0426: a `break` or `continue` naming a label no loop or block around it has, with rustc's
//! suggestion of a label of a similar name.
//!
//! Follows rustc's `compiler/rustc_resolve/src/late.rs` (`resolve_label`, the label ribs and
//! their barriers: `with_label_rib`, `with_constant_rib`, `is_label_valid_from_rib`),
//! `late/diagnostics.rs` (`suggestion_for_label_in_rib`), `diagnostics/impls.rs` and
//! `diagnostics/mod.rs` (`UndeclaredLabel`) (MIT or Apache-2.0). The front end lowers each body
//! on its own, so a label of the body around an item in it is undeclared there; rustc's ribs go
//! on through the item, and it is unreachable (E0767).

use hir::{HirFileId, InFile};
use span::SyntaxContext;
use syntax::{
    TextRange,
    view::{self as ast, AstNode as _, HasLoopBody as _},
};

use crate::{
    cx::Cx,
    lints::{code_fact, edit_distance::find_best_match_for_name},
    lower::{Found, Phase, break_outside_of_loop::edges, unreachable_label},
    model::{Applicability, Diag},
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::UndeclaredLabel) -> Vec<Found> {
    let node = cx.node_view(d.node);
    let name = node.value.text();
    let span = cx.span_of_ptr(d.node);
    let mut diag = match resolve(cx, node.map(|it| it.syntax()), name) {
        Resolution::Unreachable { defined, similar } => {
            unreachable_label::unreachable(cx, span, name, Some(defined), similar)
        }
        Resolution::Undeclared { similar } => undeclared(cx, span, name, similar),
        Resolution::Found => undeclared(cx, span, name, None),
    };
    code_fact(cx, &mut diag, d.node.map(|it| it.syntax_node_ptr()));
    vec![Found::new(diag, Phase::Resolve)]
}

/// rustc's `UndeclaredLabel`.
fn undeclared(cx: &Cx<'_, '_>, span: crate::model::Span, name: &str, similar: Option<Similar>) -> Diag {
    let mut diag = Diag::error(Some("E0426"), format!("use of undeclared label `{name}`"), span.clone());
    diag.span_label(span.clone(), format!("undeclared label `{name}`"));
    match similar {
        Some(similar) if similar.reachable => {
            diag.span_label(cx.span(similar.at), "a label with a similar name is reachable");
            diag.suggestion(span, "try using similarly named label", similar.name, Applicability::MaybeIncorrect);
        }
        Some(similar) => {
            diag.span_label(cx.span(similar.at), "a label with a similar name exists but is unreachable");
        }
        None => {}
    }
    diag
}

/// A label of a name like the one asked for.
pub(crate) struct Similar {
    pub(crate) name: String,
    pub(crate) at: InFile<TextRange>,
    pub(crate) reachable: bool,
}

/// What rustc's `resolve_label` finds for a label used at `node`.
pub(crate) enum Resolution {
    Found,
    Unreachable { defined: InFile<TextRange>, similar: Option<Similar> },
    Undeclared { similar: Option<Similar> },
}

/// A rib of rustc's label ribs: a label, or a barrier labels are not reachable through
/// (a function, a closure, an `async` or `gen` block, a constant).
enum Rib<'t> {
    Label(ast::Lifetime<'t>),
    Barrier,
}

/// rustc's `resolve_label` of the label `name` used at `node`: a label is the one used when
/// their names are the same as `macro_rules` hygiene sees them (`normalize_to_macro_rules`);
/// one is similar only in the same syntax context (`eq_ctxt`). The ribs are read as the walk
/// goes up, innermost first, and it stops at the label used; a label's syntax context is
/// looked up only when its name is the one used or like it.
pub(crate) fn resolve<'db>(cx: &Cx<'_, 'db>, node: InFile<ast::Node<'db>>, name: &str) -> Resolution {
    let used = ctxt(cx, node.file_id, node.value.text_range());
    let used_hygiene = used.opaque_and_semiopaque(cx.db);
    let mut similar: Option<Similar> = None;
    let mut reachable = true;
    for (it, child) in edges(cx, node) {
        let lifetime = match rib(it.value, child.value) {
            None => continue,
            Some(Rib::Barrier) => {
                reachable = false;
                continue;
            }
            Some(Rib::Label(lifetime)) => lifetime,
        };
        let label = lifetime.text();
        let same = label == name;
        let close = similar.is_none() && find_best_match_for_name(&[label], name, None).is_some();
        if !same && !close {
            continue;
        }
        let range = lifetime.syntax().text_range();
        let at = it.with_value(range);
        let ctxt = ctxt(cx, it.file_id, range);
        if same && ctxt.opaque_and_semiopaque(cx.db) == used_hygiene {
            return match reachable {
                true => Resolution::Found,
                false => Resolution::Unreachable { defined: at, similar },
            };
        }
        if close && ctxt == used {
            similar = Some(Similar { name: label.to_owned(), at, reachable });
        }
    }
    Resolution::Undeclared { similar }
}

/// The syntax context of the token at the start of `range` in `file`.
fn ctxt(cx: &Cx<'_, '_>, file: HirFileId, range: TextRange) -> SyntaxContext {
    file.span_map(cx.db).span_for_range(range).ctx
}

/// The label rib `it` makes for what is in its `child`, if any.
fn rib<'t>(it: ast::Node<'t>, child: ast::Node<'_>) -> Option<Rib<'t>> {
    let label = |label: Option<ast::Label<'t>>| Some(Rib::Label(label?.lifetime()?));
    match ast::Expr::cast(it) {
        Some(ast::Expr::LoopExpr(loop_)) => label(loop_.label()),
        Some(ast::Expr::WhileExpr(loop_)) => label(loop_.label()),
        Some(ast::Expr::ForExpr(loop_)) => {
            let in_body = loop_.loop_body().is_some_and(|it| it.syntax() == child);
            if in_body { label(loop_.label()) } else { None }
        }
        Some(ast::Expr::BlockExpr(block)) => {
            let coroutine = block.async_token().is_some() || block.gen_token().is_some();
            let constant = block.const_token().is_some();
            match coroutine || constant {
                true => Some(Rib::Barrier),
                false => label(block.label()),
            }
        }
        Some(ast::Expr::ClosureExpr(closure)) => {
            closure.body().is_some_and(|it| it.syntax() == child).then_some(Rib::Barrier)
        }
        _ => {
            let barrier = ast::Fn::can_cast(it.kind())
                || ast::Const::can_cast(it.kind())
                || ast::Static::can_cast(it.kind())
                || ast::ConstArg::can_cast(it.kind())
                || ast::ArrayExpr::cast(it).is_some_and(|array| {
                    array.semicolon_token().is_some() && array.exprs().nth(1).is_some_and(|len| len.syntax() == child)
                });
            barrier.then_some(Rib::Barrier)
        }
    }
}
