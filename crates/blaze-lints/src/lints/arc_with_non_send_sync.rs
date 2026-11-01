//! `arc_with_non_send_sync`: `Arc::new(value)` of a value that is not both `Send` and `Sync`:
//! the `Arc` can't cross a thread, so its atomic count buys nothing an `Rc` doesn't. AGENTS.md
//! rule 9: one choice per need.
//!
//! Follows clippy's `clippy_lints/src/arc_with_non_send_sync.rs` (MIT or Apache-2.0), read from
//! indexable-inc's fork at c2308e38 (nightly-2026-05-27), the nearest checkout to the pinned
//! nightly's (rust-lang/rust 12433c30). As in clippy: a call written in the file of `new` on a
//! path naming std's `Arc`, with one argument whose type has no type parameter in it and is not
//! `Send`, not `Sync` or neither. Unlike clippy, which says "usage of an `Arc` that is not
//! `Send` and `Sync`" and puts the type in a note, the message names the type and what it lacks.

use hir::{HirDisplay as _, PathResolution};
use syntax::view::{self as ast, AstNode as _, HasArgList as _};

use super::{
    Implemented,
    box_collection::{Std, std_adt},
};
use crate::check::Cx;

const LINT: Implemented = Implemented::ArcWithNonSendSync;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, expr: ast::Expr<'t>) {
    if !cx.on(LINT) {
        return;
    }
    let ast::Expr::CallExpr(call) = expr else { return };
    let Some(arg) = arc_new_arg(cx, call) else { return };
    let Some(ty) = cx.ty(arg) else { return };
    let db = cx.db();
    let mut generic = false;
    ty.walk(db, |it| generic |= it.as_type_param(db).is_some());
    if generic || ty.contains_unknown() {
        return;
    }
    let marker = |name: &str| match cx.std_def(&["core", "marker", name]) {
        Some(hir::ModuleDef::Trait(it)) => Some(ty.impls_trait(db, it, &[])),
        _ => None,
    };
    let (Some(send), Some(sync)) = (marker("Send"), marker("Sync")) else { return };
    let lacks = match (send, sync) {
        (false, false) => "neither `Send` nor `Sync`",
        (false, true) => "not `Send`",
        (true, false) => "not `Sync`",
        (true, true) => return,
    };
    let Some(range) = cx.range(expr.syntax()) else { return };
    let shown = ty.display(db, cx.krate().to_display_target(db)).to_string();
    let span = cx.span(range);
    let message = format!("`Arc<{shown}>` can't be shared across threads: `{shown}` is {lacks}");
    cx.lint(LINT, expr.syntax(), span, message)
        .help("on one thread, an `Rc`")
        .help(format!("across threads, make `{shown}` `Send` and `Sync`, or put it in a `Mutex`"));
}

/// The argument of `call`, when it is `Arc::new(arg)` written in the file.
fn arc_new_arg<'t>(cx: &Cx<'_, 't>, call: ast::CallExpr<'t>) -> Option<ast::Expr<'t>> {
    let ast::Expr::PathExpr(callee) = call.expr()? else { return None };
    let path = callee.path()?;
    if path.segment()?.name_ref()?.text() != "new" {
        return None;
    }
    let PathResolution::Def(hir::ModuleDef::Adt(adt)) = cx.path(path.qualifier()?)? else { return None };
    if std_adt(cx, adt) != Some(Std::Arc) {
        return None;
    }
    let mut args = call.arg_list()?.args();
    let arg = args.next()?;
    (args.next().is_none() && cx.is_written(arg.syntax())).then_some(arg)
}

#[cfg(test)]
mod tests {
    use super::super::more_rules_tests::lint_only;

    const LINT: &str = "arc_with_non_send_sync";
    const FOUND: &str = "can't be shared across threads";

    #[test]
    fn an_arc_of_a_value_not_send_or_sync_is_found() {
        let found = lint_only(
            LINT,
            r#"
use std::{rc::Rc, sync::Arc};
pub struct Local(Rc<u8>);
pub struct OnlySend(Rc<u8>);
unsafe impl core::marker::Send for OnlySend {}
pub fn f() {
    let _a = Arc::new(Rc::new(1u8));
    let _b = Arc::new(Local(Rc::new(1)));
    let _c = Arc::new(OnlySend(Rc::new(1)));
    let _d = Arc::new(1u8);
}
pub fn g<T>(t: T) -> Arc<T> { Arc::new(t) }
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 3, "{found}");
        assert!(found.contains("`Arc<Local>` can't be shared across threads: `Local` is neither `Send` nor `Sync`"), "{found}");
        assert!(found.contains("`OnlySend` is not `Sync`"), "{found}");
    }

    #[test]
    fn an_arc_through_a_renamed_import_and_one_a_macro_is_handed_are_found() {
        let found = lint_only(
            LINT,
            r#"
use std::{rc::Rc, sync::Arc as Shared};
pub struct Local(Rc<u8>);
macro_rules! id { ($e:expr) => { $e }; }
pub fn f() {
    let _a = Shared::new(Local(Rc::new(1)));
    let _b = id!(Shared::new(Local(Rc::new(1))));
}
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 2, "{found}");
    }
}
