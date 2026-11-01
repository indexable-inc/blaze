//! `untyped_error`: a function's signature whose error is a type that can't name what failed for
//! the caller to match on (AGENTS.md rule 14): `Result<_, String>`, `Result<_, Box<dyn Error>>`
//! (any auto traits beside), and `anyhow::Error` (`anyhow::Result<T>` is `Result<T,
//! anyhow::Error>`), anywhere in the return type or a parameter's type.
//!
//! Checked in a library target's functions and in a binary's or example's outside its crate
//! root, the `main` module, which may report to a person and exit. A test, bench or build
//! script target, code under `#[cfg(test)]` or `#[test]`, and a method of a trait impl (whose
//! signature is the trait's, found at the trait) are not checked. Types are resolved, so a
//! renamed import or an alias of one of them is found.

use hir::{AsAssocItem as _, HirDisplay as _};
use hir_def::lang_item::lang_items;
use syntax::view::{self as ast, AstNode as _};

use super::{Lint, facts};
use crate::check::Cx;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, f: ast::Fn<'t>) {
    if !cx.on_blaze(Lint::UntypedError) {
        return;
    }
    let db = cx.db();
    let Some(func) = cx.fn_def(f) else { return };
    let module = func.module(db);
    let checked = match cx.krate().target_kind(db) {
        base_db::TargetKind::Lib { .. } => true,
        base_db::TargetKind::Bin | base_db::TargetKind::Example => !module.is_crate_root(db),
        _ => false,
    };
    if !checked || func.as_assoc_item(db).is_some_and(|it| it.implemented_trait(db).is_some()) {
        return;
    }
    if cx.is_in_test(f.syntax()) {
        return;
    }
    let Some(kinds) = Kinds::of(cx) else { return };
    let target = cx.krate().to_display_target(db);
    let name = func.name(db);
    // alloc: a function's return and parameters, a few; `None` is the return
    let mut places = Vec::with_capacity(4);
    if let Some(ret) = f.ret_type().and_then(|it| it.ty()) {
        places.push((ret.syntax(), func.ret_type(db), None));
    }
    let written = f.param_list().into_iter().flat_map(|it| it.params());
    for (param, hir_param) in written.zip(func.params_without_self(db)) {
        let Some(ty) = param.ty() else { continue };
        places.push((ty.syntax(), hir_param.ty().clone(), Some(param.pat()))); // clone: the parameter's type, once per parameter
    }
    for (node, ty, param) in places {
        let Some(found) = kinds.untyped(cx, &ty) else { continue };
        // A signature a macro wrote from nothing it was handed has no range of its own: the
        // finding is at the outermost macro call of the file that wrote it.
        let Some(range) = cx.range(node).or(cx.call_at()) else { continue };
        let what = match param {
            None => "returns".to_owned(),
            Some(Some(pat)) => format!("takes `{}` as", cx.source_of(pat.syntax())),
            Some(None) => "takes".to_owned(),
        };
        let message = format!(
            "`{}` {what} `{}`, whose error is {found}: an error the caller can't match on",
            name.as_str(),
            ty.display(db, target)
        );
        let span = cx.span(range);
        cx.blaze(Lint::UntypedError, node, span, message)
            .note("an error names the item it is about and the rule that refused it; an enum of what can fail lets the caller tell them apart")
            .help(Lint::UntypedError.help())
            .help(format!("for further information visit {}", Lint::UntypedError.docs_url()));
    }
}

/// The types an untyped error is made of, in the crate linted.
struct Kinds {
    result: hir::Adt,
    string: Option<hir::Adt>,
    boxed: Option<hir::Adt>,
    error: Option<hir::Trait>,
}

impl Kinds {
    fn of(cx: &Cx<'_, '_>) -> Option<Kinds> {
        let db = cx.db();
        let Some(hir::ModuleDef::Adt(result)) = cx.std_def(&["core", "result", "Result"]) else { return None };
        let lang = lang_items(db, cx.krate().base());
        Some(Kinds {
            result,
            string: lang.String.map(|it| hir::Adt::Struct(it.into())),
            boxed: lang.OwnedBox.map(|it| hir::Adt::Struct(it.into())),
            error: cx.std_trait(&["core", "error", "Error"]),
        })
    }

    /// What makes `ty` hold an untyped error, said for the message, if anything does.
    fn untyped(&self, cx: &Cx<'_, '_>, ty: &hir::Type<'_>) -> Option<&'static str> {
        let db = cx.db();
        let mut found = None;
        ty.walk(db, |it| {
            if found.is_some() {
                return;
            }
            let Some(adt) = it.as_adt() else { return };
            if facts::adt_is(db, adt, "anyhow", "Error") {
                found = Some("`anyhow::Error`");
                return;
            }
            if adt != self.result {
                return;
            }
            let Some(error) = it.type_arguments().nth(1) else { return };
            let error_adt = error.as_adt();
            if error_adt.is_some() && error_adt == self.string {
                found = Some("a `String`");
            } else if error_adt.is_some()
                && error_adt == self.boxed
                && let Some(inner) = error.type_arguments().next()
                && self.error.is_some_and(|it| inner.as_dyn_trait() == Some(it))
            {
                found = Some("a `Box<dyn Error>`");
            }
        });
        found
    }
}
