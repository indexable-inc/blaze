//! rustc's `InteriorMutableConsts` (`const_item_interior_mutations`): a method marked
//! `#[rustc_should_not_be_called_on_const_items]` (`Cell::set`, `Once::call_once`,
//! `AtomicUsize::store`, ..) called on a `const` item, `A.set(1)` or `LazyCell::force(&A)`,
//! which changes only a temporary copy of it.
//!
//! Follows rustc's `compiler/rustc_lint/src/interior_mutable_consts.rs` and
//! `compiler/rustc_lint/src/lints.rs` (`ConstItemInteriorMutationsDiag`,
//! `ConstItemInteriorMutationsSuggestionStatic`) (MIT or Apache-2.0).

use std::ops::ControlFlow;

use base_db::Db;
use blaze_items::{Fact, ItemId};
use hir::InFile;
use hir_def::{
    ConstId, FunctionId, HasModule as _, ItemContainerId, Lookup as _,
    hir::{Expr, ExprId},
    resolver::ValueNs,
    signatures::{ConstSignature, FunctionSignature},
    src::HasSource as _,
};
use hir_expand::attrs::{AstPathExt as _, expand_cfg_attr_view};
use hir_ty::Adjust;
use syntax::{
    TextRange,
    view::{self as ast, AstNode as _, HasAttrs as _, HasName as _, HasVisibility as _},
};

use crate::{
    lints::bodies::{BodyCx, ExprKinds, ExprKind, LatePass, LintSet, lint_set, types::util},
    lower::{Found, Phase},
    model::{Applicability, Diag},
};

#[derive(Default)]
pub(crate) struct InteriorMutableConsts;

impl LatePass for InteriorMutableConsts {
    const LINTS: LintSet = lint_set(["const_item_interior_mutations"]);
    const EXPRS: ExprKinds = ExprKinds::of(&[ExprKind::MethodCall, ExprKind::Call]);

    fn check_expr(&mut self, b: &BodyCx<'_, '_, '_>, expr: ExprId, out: &mut Vec<Found>) {
        let store = b.store();
        let (callee, receiver) = match store.expr(expr) {
            Expr::MethodCall { receiver, .. } => (None, receiver),
            Expr::Call { callee, args: &[first, ..] } => {
                let Expr::Ref { expr: receiver, .. } = store.expr(first) else { return };
                let Expr::Path(_) = store.expr(callee) else { return };
                (Some(callee), receiver)
            }
            _ => return,
        };
        let Expr::Path(_) = store.expr(receiver) else { return };
        let Some(ValueNs::ConstId(konst)) = util::path_value(b, receiver) else { return };
        let method = match callee {
            None => util::method_fn(b, expr),
            Some(callee) => match util::path_value(b, callee) {
                Some(ValueNs::FunctionId(it)) => Some(it),
                _ => None,
            },
        };
        let Some(method) = method else { return };
        // A deref can do anything, such as reach a thread local (rust-lang/rust#150157).
        if b.infer.expr_adjustment(receiver).is_some_and(|it| it.iter().any(|it| matches!(it.kind, Adjust::Deref(_)))) {
            return;
        }
        let db = b.db();
        if !should_not_be_called_on_const_items(db, method) {
            return;
        }
        let Some(const_name) = ConstSignature::of(db, konst).name.as_ref() else { return };
        let const_name = const_name.as_str();
        let method_signature = FunctionSignature::of(db, method);
        let method_name = method_signature.name.as_str();
        let (Some(span), Some(receiver_span), Some(node), Some(const_ty)) = (
            util::span(b, expr),
            util::span(b, receiver),
            b.expr_node(expr),
            super::print(b, receiver, util::ty(b, receiver)),
        ) else {
            return;
        };

        let mut diag = Diag::lint(
            "const_item_interior_mutations",
            // alloc: the lint's text, for a lint found
            format!("mutation of an interior mutable `const` item with call to `{method_name}`"),
            span,
        );
        diag.span_label(
            receiver_span,
            // alloc: the lint's text, for a lint found
            format!("`{const_name}` is a interior mutable `const` item of type `{const_ty}`"),
        );
        diag.note("each usage of a `const` item creates a new temporary");
        // alloc: the lint's text, for a lint found
        diag.note(format!("only the temporaries and never the original `const {const_name}` will be modified"));
        diag.help(
            "for more details on interior mutability see <https://doc.rust-lang.org/reference/interior-mutability.html>",
        );
        static_suggestion(b, &mut diag, konst, const_name);
        diag.fact(
            Fact::Signature(b.cx.key(ItemId::Function(method))),
            // alloc: the lint's text, for a lint found
            format!("the attributes of `{method_name}`"),
        );
        out.push(b.lint(diag, Phase::Late, node));
    }
}

/// rustc's `find_attr!(.., RustcShouldNotBeCalledOnConstItems)` of `method`, its `cfg_attr`s
/// expanded.
fn should_not_be_called_on_const_items(db: &Db, method: FunctionId) -> bool {
    let cfg = method.krate(db).cfg_options(db);
    let attrs = method.lookup(db).source_view(db).value.attrs();
    expand_cfg_attr_view(attrs, || cfg, |meta, _| match meta {
        ast::Meta::PathMeta(it) if it.path().is1("rustc_should_not_be_called_on_const_items") => ControlFlow::Break(()),
        _ => ControlFlow::Continue(()),
    })
    .is_some()
}

/// rustc's `ConstItemInteriorMutationsSuggestionStatic`: for a `const` item of the crate (not
/// an associated one), `static` suggested in place of `const`, or said in words when the item
/// is written by a macro.
fn static_suggestion(b: &BodyCx<'_, '_, '_>, diag: &mut Diag, konst: ConstId, const_name: &str) {
    let db = b.db();
    let loc = konst.lookup(db);
    if !matches!(loc.container, ItemContainerId::ModuleId(_)) || konst.krate(db) != b.cx.krate.base() {
        return;
    }
    // alloc: the lint's text, for a lint found
    let message = format!("for a shared instance of `{const_name}`, consider making it a `static` item instead");
    let source = loc.source_view(db);
    let range = (|| {
        if source.file_id.is_macro() {
            return None;
        }
        let item = &source.value;
        let (start, before) = match item.visibility() {
            Some(vis) => (vis.syntax().text_range().end(), " "),
            None => (item.const_token()?.text_range().start(), ""),
        };
        let end = item.name()?.syntax().text_range().start();
        Some((TextRange::new(start, end), before))
    })();
    match range {
        Some((range, before)) => {
            let span = b.cx.span(InFile::new(source.file_id, range));
            // alloc: the lint's text, for a lint found
            diag.suggestion_verbose(span, message, format!("{before}static "), Applicability::MaybeIncorrect);
        }
        None => {
            diag.help(message);
        }
    }
}
