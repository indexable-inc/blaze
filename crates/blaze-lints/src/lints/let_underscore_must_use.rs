//! `let_underscore_must_use`: `let _ = value;` of a `#[must_use]` type or of the result of a
//! `#[must_use]` function: the value is dropped unread. For a `Result` that is a dropped error,
//! the silent fallback AGENTS.md rule 3 bans.
//!
//! Follows clippy's `clippy_lints/src/let_underscore.rs` (`LET_UNDERSCORE_MUST_USE`) and
//! `clippy_utils`' `is_must_use_ty` and `is_must_use_func_call` (MIT or Apache-2.0), read from
//! indexable-inc's fork at c2308e38 (nightly-2026-05-27), the nearest checkout to the pinned
//! nightly's (rust-lang/rust 12433c30). As in clippy: a `let` whose pattern is `_`, written in
//! the file, whose initializer is not a future ([`super::let_underscore_future`] reports those);
//! the type is `#[must_use]` through references, slices, arrays, raw pointers and tuples, a
//! `Result` with an uninhabited error or a `ControlFlow` with an uninhabited break as what they
//! hold, and an `impl Trait` or `dyn Trait` of a `#[must_use]` trait; otherwise a call of a
//! `#[must_use]` function. Unlike clippy's, the message names the type or the function, and the
//! attribute's own message is a note. clippy's `let_underscore_lock` is not ported (a guard is
//! `#[must_use]`, and this reports it).

use hir::{HirDisplay as _, PathResolution};
use hir_def::{AdtId, AttrDefId, TraitId, attrs::AttrFlags};
use syntax::view::{self as ast, AstNode as _};

use super::{
    Implemented,
    let_underscore_future::{is_future, wildcard_init},
    utils,
};
use crate::check::Cx;

const LINT: Implemented = Implemented::LetUnderscoreMustUse;

/// How deep [`must_use_ty`] looks into what a type holds.
const DEEPEST: u32 = 8;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, stmt: ast::LetStmt<'t>) {
    if !cx.on(LINT) {
        return;
    }
    let Some(init) = wildcard_init(stmt) else { return };
    let Some(ty) = cx.ty(init) else { return };
    if is_future(cx, &ty) {
        return;
    }
    let db = cx.db();
    let target = cx.krate().to_display_target(db);
    let written = cx.source_of(init.syntax());
    let (message, reason) = match must_use_ty(cx, &ty, 0) {
        Some(reason) => (
            format!("`let _ =` drops the `{}` of `{written}`, a `#[must_use]` type", ty.display(db, target)),
            reason,
        ),
        None => {
            let Some(function) = callee(cx, init.peel_parens()) else { return };
            let Ok(id) = hir_def::FunctionId::try_from(function) else { return };
            let Some(reason) = AttrFlags::must_use_message(db, AttrDefId::FunctionId(id)) else { return };
            (
                format!("`let _ =` drops the result of `{}`, which is `#[must_use]`", function.name(db).as_str()),
                reason,
            )
        }
    };
    let is_result = utils::option_or_result(cx, &ty) == Some("Result") && !ty.is_reference();
    let Some(range) = cx.range(stmt.syntax()) else { return };
    let span = cx.span(range);
    let diagnostic = cx.lint(LINT, stmt.syntax(), span, message);
    if let Some(reason) = reason {
        diagnostic.note(reason);
    }
    if is_result {
        diagnostic.help(
            "handle the error (`?`, a `match`); if dropping it is right here, say why: `#[expect(clippy::let_underscore_must_use, reason = \"..\")]`",
        );
    } else {
        diagnostic.help("use the value, or `drop(..)` it by name and say why");
    }
}

/// clippy's `is_must_use_ty`: the attribute's message when `ty` is `#[must_use]` (`Some(None)`
/// for one without a message).
fn must_use_ty<'q>(cx: &Cx<'q, '_>, ty: &hir::Type<'q>, depth: u32) -> Option<Option<&'q str>> {
    if depth > DEEPEST {
        return None;
    }
    let db = cx.db();
    let trait_must_use = |it: hir::Trait| AttrFlags::must_use_message(db, AttrDefId::TraitId(TraitId::from(it)));
    if let Some(adt) = ty.as_adt() {
        if let hir::Adt::Enum(it) = adt {
            let mut args = ty.type_arguments();
            if let (Some(first), Some(second)) = (args.next(), args.next()) {
                if cx.famous().core_result_Result() == Some(it) && is_uninhabited(cx, &second) {
                    return must_use_ty(cx, &first, depth + 1);
                }
                if control_flow(cx) == Some(it) && is_uninhabited(cx, &first) {
                    return must_use_ty(cx, &second, depth + 1);
                }
            }
        }
        return AttrFlags::must_use_message(db, AttrDefId::AdtId(AdtId::from(adt)));
    }
    if let Some(inner) = ty.as_reference_inner().or_else(|| ty.as_slice()).or_else(|| ty.remove_raw_ptr()) {
        return must_use_ty(cx, &inner, depth + 1);
    }
    if let Some(inner) = ty.as_array_element(db) {
        return must_use_ty(cx, &inner, depth + 1);
    }
    if ty.is_tuple() {
        return ty.tuple_fields(db).iter().find_map(|it| must_use_ty(cx, it, depth + 1));
    }
    if let Some(mut traits) = ty.as_impl_traits(db) {
        return traits.find_map(trait_must_use);
    }
    ty.as_dyn_trait().and_then(trait_must_use)
}

/// `core::ops::ControlFlow`, by the lang item of its `Continue` variant.
fn control_flow(cx: &Cx<'_, '_>) -> Option<hir::Enum> {
    let items = hir_def::lang_item::lang_items(cx.db(), cx.krate().base());
    Some(hir::EnumVariant::from(items.ControlFlowContinue?).parent_enum(cx.db()))
}

/// Whether `ty` has no values: `!`, or an enum with no variants (`Infallible`).
fn is_uninhabited(cx: &Cx<'_, '_>, ty: &hir::Type<'_>) -> bool {
    ty.is_never()
        || matches!(ty.as_adt(), Some(hir::Adt::Enum(it)) if it.variants(cx.db()).is_empty())
}

/// clippy's `is_must_use_func_call`'s callee: the function a call names by path, or a method
/// call resolves to.
fn callee<'t>(cx: &Cx<'_, 't>, expr: ast::Expr<'t>) -> Option<hir::Function> {
    match expr {
        ast::Expr::CallExpr(call) => {
            let ast::Expr::PathExpr(path) = call.expr()? else { return None };
            match cx.path(path.path()?)? {
                PathResolution::Def(hir::ModuleDef::Function(it)) => Some(it),
                _ => None,
            }
        }
        ast::Expr::MethodCallExpr(call) => cx.method(call),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::super::more_rules_tests::lint_only;

    const LINT: &str = "let_underscore_must_use";
    const TYPE: &str = "a `#[must_use]` type";
    const FUNCTION: &str = "which is `#[must_use]`";

    #[test]
    fn a_dropped_must_use_value_or_result_is_found_and_an_ordinary_one_is_not() {
        let found = lint_only(
            LINT,
            r#"
#[must_use = "a token is spent by `redeem`"]
pub struct Token;
#[must_use]
pub fn count() -> u8 { 0 }
pub fn token() -> Token { Token }
pub fn plain() -> u8 { 0 }
pub fn f() {
    let _ = token();
    let _ = (1u8, token());
    let _ = &token();
    let _ = count();
    let _ = plain();
    let _named = token();
}
"#,
        );
        assert_eq!(found.matches(TYPE).count(), 3, "{found}");
        assert!(found.contains("drops the `Token` of `token()`"), "{found}");
        assert!(found.contains("a token is spent by `redeem`"), "the attribute's message is a note:\n{found}");
        assert_eq!(found.matches(FUNCTION).count(), 1, "{found}");
        assert!(found.contains("drops the result of `count`"), "{found}");
    }

    #[test]
    fn a_result_with_an_uninhabited_error_is_what_it_holds() {
        let found = lint_only(
            LINT,
            r#"
pub enum Never {}
#[must_use]
pub struct Token;
pub fn plain() -> Result<u8, Never> { Ok(0) }
pub fn token() -> Result<Token, Never> { Ok(Token) }
pub fn f() {
    let _ = plain();
    let _ = token();
}
"#,
        );
        assert_eq!(found.matches(TYPE).count(), 1, "{found}");
        assert!(found.contains("of `token()`"), "{found}");
    }

    #[test]
    fn a_must_use_type_through_a_renamed_import_and_one_a_macro_is_handed_are_found() {
        let found = lint_only(
            LINT,
            r#"
mod tokens {
    #[must_use]
    pub struct Token;
    pub fn make() -> Token { Token }
}
use tokens::make as mint;
macro_rules! stmt { ($($t:tt)*) => { $($t)* }; }
pub fn f() {
    let _ = mint();
    stmt! { let _ = mint(); }
}
"#,
        );
        assert!(found.matches(TYPE).count() >= 1, "{found}");
        assert!(found.contains("of `mint()`"), "{found}");
    }

    #[test]
    fn a_future_is_left_to_let_underscore_future() {
        let found = lint_only(
            LINT,
            r#"
pub async fn work() -> u8 { 0 }
pub async fn f() {
    let _ = work();
}
"#,
        );
        assert!(found.is_empty(), "{found}");
    }
}
