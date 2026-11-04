//! `cold_error`: an error that costs on the path that doesn't fail. Two rules:
//!
//! - **Large error type** (everywhere, test code not): a function returning `Result<T, E>`
//!   whose `E`'s layout is over [`LIMIT`] bytes. Every `Result` it returns is at least that
//!   big, so the `Ok` path writes and moves the error's size too. clippy's `result_large_err`
//!   is in blaze-lints' registry but not implemented, so this rule is its port, at clippy's
//!   default threshold. A method of a trait impl is not checked (its signature is the trait's),
//!   nor a type whose layout depends on a parameter it was not given.
//! - **Error built inline** (only where `blaze::hot` is on: its group turns on `hot_clone` and
//!   `hot_dyn`, which nothing in the workspace warns on alone): in a function that is neither
//!   `#[cold]` nor `#[inline(never)]`, an error made in place with a `String` (`format!`,
//!   `to_string`, `to_owned`, `String::from`: a call of a function that is not itself `#[cold]`
//!   or `#[inline(never)]` and returns `String` or `blaze_mem::text::String`) or a struct
//!   literal over [`LIMIT`] bytes. The places an error is made are the argument of `Err(..)`
//!   (`return Err(..)` included), of `.ok_or(..)` and `.context(..)`, and the closure body of
//!   `.map_err(..)`, `.ok_or_else(..)` and `.with_context(..)` on a `Result` or an `Option`: the
//!   branch `?` converts. Its code sits in the hot function, growing it and spreading it over
//!   more lines of the instruction cache, and it blocks inlining; in a `#[cold]
//!   #[inline(never)] fn` that returns the error, the hot path keeps one call.
//!
//! No fix: where the error's inputs come from is the author's. Keep one with
//! `#[expect(blaze::cold_error, reason = "..")]`.

use base_db::Db;
use hir::{AsAssocItem as _, HirDisplay as _};
use hir_def::{
    AttrDefId, DefWithBodyId, HasModule as _,
    attrs::AttrFlags,
    expr_store::{Body, scope::ExprScopes},
    hir::{Expr, ExprId},
    lang_item::lang_items,
    resolver::{ValueNs, resolver_for_scope},
    signatures::FunctionSignature,
};
use syntax::view::{self as ast, AstNode as _};

use super::{Finding, Lint, alloc::written_callee, facts};
use crate::check::Cx;

const LINT: Lint = Lint::ColdError;

/// 128 bytes: clippy's `result_large_err` default (`large-error-threshold`), two 64-byte lines.
/// An error under it rides in the `Result` for a copy or two; over it, every `Ok` pays.
const LIMIT: u64 = 128;

/// The size an error may be: `large-error-threshold` in `clippy.toml`, clippy's own key for
/// `result_large_err`, or [`LIMIT`].
fn limit(cx: &Cx<'_, '_>) -> u64 {
    u64::try_from(cx.conf().int("large-error-threshold", LIMIT as i64)).unwrap_or(LIMIT)
}

/// The methods whose one argument is the error, made whether or not it is used.
const EAGER: &[&str] = &["ok_or", "context"];

/// The methods whose one closure argument makes the error.
const LAZY: &[&str] = &["map_err", "ok_or_else", "with_context"];

/// A function's return type: the large error rule.
pub(super) fn check_fn<'t>(cx: &mut Cx<'_, 't>, f: ast::Fn<'t>) {
    if !cx.on_blaze(LINT) || cx.expansion().is_some() || !cx.is_written(f.syntax()) {
        return;
    }
    let db = cx.db();
    let Some(func) = cx.fn_def(f) else { return };
    if func.as_assoc_item(db).is_some_and(|it| it.implemented_trait(db).is_some())
        || cx.is_in_test(f.syntax())
    {
        return;
    }
    let Some(ret) = f.ret_type().and_then(|it| it.ty()) else { return };
    let Some(result) = cx.famous().core_result_Result() else { return };
    let ty = func.ret_type(db);
    if ty.as_adt() != Some(hir::Adt::Enum(result)) {
        return;
    }
    let Some(error) = ty.type_arguments().nth(1) else { return };
    let Ok(layout) = error.layout(db) else { return };
    let size = layout.size();
    let limit = limit(cx);
    if size <= limit {
        return;
    }
    let target = cx.krate().to_display_target(db);
    let name = func.name(db);
    let message = format!(
        "`fn {}` returns a `Result` whose error `{}` is {size} bytes: every `Ok` it returns is that big too",
        name.as_str(),
        error.display(db, target)
    );
    let Some(range) = cx.range(ret.syntax()) else { return };
    let span = cx.span(range);
    cx.blaze(LINT, ret.syntax(), span, message)
        .note(format!(
            "box the error (`Box<{}>`) or its large variant's payload, so the `Result` holds a pointer; \
             over {limit} bytes is the threshold (`large-error-threshold` in `clippy.toml`)",
            error.display(db, target)
        ))
        .help(LINT.help())
        .help(format!("for further information visit {}", LINT.docs_url()));
}

/// A body's errors: the inline error rule.
pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(LINT) || !hot(cx) {
        return;
    }
    let found = find(cx);
    facts::report(cx, found);
}

/// Whether the group `blaze::hot` is on where the body is: `hot_clone` and `hot_dyn` are both
/// on, which in this workspace only the group does (neither is warned on alone).
fn hot(cx: &Cx<'_, '_>) -> bool {
    cx.on_blaze(Lint::HotClone) && cx.on_blaze(Lint::HotDyn)
}

/// Whether `f` is `#[cold]` or `#[inline(never)]`.
fn cold(db: &Db, f: hir_def::FunctionId) -> bool {
    AttrFlags::query(db, AttrDefId::FunctionId(f))
        .intersects(AttrFlags::COLD | AttrFlags::INLINE_NEVER)
}

fn find(cx: &Cx<'_, '_>) -> Vec<Finding> {
    // alloc: the findings of one body, reported at the edge
    let mut found = Vec::new();
    let Some(analysis) = cx.analysis() else { return found };
    let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return found };
    let DefWithBodyId::FunctionId(own) = def else { return found };
    let db = cx.db();
    if cold(db, own) || cx.is_in_test(cx.root()) {
        return found;
    }
    let Some(err) = lang_items(db, def.krate(db)).ResultErr else { return found };
    let limit = limit(cx);
    let famous = cx.famous();
    let result = famous.core_result_Result().map(hir::Adt::Enum);
    let option = famous.core_option_Option().map(hir::Adt::Enum);
    let string = cx.krate().diagnostic_item_adt(db, &intern::Symbol::intern("String"));
    let body = analysis.body();
    let infer = analysis.infer();
    let name = hir::Function::from(own).name(db);
    let name = name.as_str();
    let is_string = |ty: &hir::Type<'_>| {
        ty.as_adt()
            .is_some_and(|adt| Some(adt) == string || facts::adt_is(db, adt, "blaze_mem", "String"))
    };
    let builds = |at: ExprId| -> Option<String> {
        // alloc: the worklist of one error's expressions
        let mut stack = vec![at];
        while let Some(it) = stack.pop() {
            match body.expr(it) {
                Expr::Call { .. } | Expr::MethodCall { .. } => {
                    if let Some((callee, _)) = written_callee(infer, body, it)
                        && !cold(db, callee)
                        && is_string(&analysis.ty(infer.expr_ty(it)))
                    {
                        let called = FunctionSignature::of(db, callee).name.as_str().to_owned(); // alloc: a message's part
                        return Some(match called.as_str() {
                            "format" => "a `String` (`format!`)".to_owned(), // alloc: a message's part
                            _ => format!("a `String` (`{called}`)"),
                        });
                    }
                }
                Expr::RecordLit { .. } => {
                    if let Ok(layout) = analysis.ty(infer.expr_ty(it)).layout(db)
                        && layout.size() > limit
                    {
                        return Some(format!("a struct literal of {} bytes", layout.size()));
                    }
                }
                _ => {}
            }
            body.walk_child_exprs(it, |child| stack.push(child));
        }
        None
    };
    for (id, expr) in body.exprs() {
        let (made, site) = match expr {
            Expr::Call { callee, args: [arg] } if is_variant(db, def, body, callee, err) => {
                (*arg, "this `Err(..)`")
            }
            Expr::MethodCall { receiver, method_name, args: [arg], .. } => {
                let method = method_name.as_str();
                let of = analysis.ty(infer.expr_ty(receiver)).strip_references().as_adt();
                if of.is_none() || !(of == result || of == option) {
                    continue;
                }
                if EAGER.contains(&method) {
                    (*arg, "the error of this call")
                } else if LAZY.contains(&method)
                    && let Expr::Closure { body: closure, .. } = body.expr(*arg)
                {
                    (closure, "the error of this call")
                } else {
                    continue;
                }
            }
            _ => continue,
        };
        let Some(what) = builds(made) else { continue };
        let Some(range) = analysis.expr_range(id) else { continue };
        let message =
            format!("{site} builds its error inline with {what} in `fn {name}`, on a hot path");
        let note = "the error's code sits in the hot function, growing it and spreading it over more of the \
                    instruction cache, and keeps it from being inlined: build the error in a `#[cold] \
                    #[inline(never)] fn` that returns it, so the hot path keeps one call";
        facts::push(&mut found, LINT, range, message, Some(note.to_owned()));
    }
    found
}

/// Whether the path `callee` of `def`'s body names the enum variant `variant`.
fn is_variant(
    db: &Db,
    def: DefWithBodyId,
    body: &Body,
    callee: ExprId,
    variant: hir_def::EnumVariantId,
) -> bool {
    let Expr::Path(path) = body.expr(callee) else { return false };
    let scope = ExprScopes::of(db, def).scope_for(callee);
    matches!(
        resolver_for_scope(db, def, scope).resolve_path_in_value_ns_fully(db, path, body.expr_path_hygiene(callee)),
        Some(ValueNs::EnumVariantId(it)) if it == variant
    )
}

#[cfg(test)]
mod tests {
    use crate::tests::lint;

    const FIXTURE: &str = r#"
//- minicore: sized, copy, clone, result, option
//- /std/lib.rs crate:std
pub mod prelude {
    pub mod v1 {
        pub use crate::string::String;
        pub use core::prelude::v1::*;
    }
    pub mod rust_2015 { pub use super::v1::*; }
    pub mod rust_2018 { pub use super::v1::*; }
    pub mod rust_2021 { pub use super::v1::*; }
    pub mod rust_2024 { pub use super::v1::*; }
}
pub mod string {
    #[rustc_diagnostic_item = "String"]
    pub struct String { ptr: *const u8, cap: usize, len: usize }
}
pub mod fmt {
    pub fn format(text: &str) -> crate::string::String { loop {} }
}
#[macro_export]
macro_rules! format {
    ($text:literal) => { $crate::fmt::format($text) };
}
//- /src/lib.rs crate:main deps:std
pub struct Big { bytes: [u8; 256] }
pub fn parse(x: u32) -> Result<u32, Big> { Ok(x) }
pub fn small(x: u32) -> Result<u32, u8> { Ok(x) }
pub fn hot(x: u32) -> Result<u32, String> {
    if x == 0 {
        return Err(std::format!("zero"));
    }
    Ok(x)
}
#[cold]
#[inline(never)]
fn zero() -> String { std::format!("zero") }
pub fn outlined(x: u32) -> Result<u32, String> {
    if x == 0 {
        return Err(zero());
    }
    Ok(x)
}
"#;

    #[test]
    fn a_large_error_everywhere_and_an_inline_one_on_a_hot_path_are_found() {
        let warm = lint(FIXTURE.trim_start(), "-A clippy::all --warn=blaze::cold_error");
        assert!(found_large(&warm), "{warm}");
        assert!(!warm.contains("`fn small` returns"), "{warm}");
        assert!(
            !warm.contains("builds its error inline"),
            "only a hot path is checked inline:\n{warm}"
        );
        let hot =
            lint(FIXTURE.trim_start(), "-A clippy::all --deny=blaze::hot --warn=blaze::cold_error");
        assert!(found_large(&hot), "{hot}");
        assert!(
            hot.contains(
                "this `Err(..)` builds its error inline with a `String` (`format!`) in `fn hot`"
            ),
            "{hot}"
        );
        assert!(!hot.contains("in `fn outlined`"), "a `#[cold]` builder is the fix:\n{hot}");
        assert!(!hot.contains("in `fn zero`"), "a `#[cold]` function is not checked:\n{hot}");
    }

    fn found_large(found: &str) -> bool {
        found.contains("`fn parse` returns a `Result` whose error `Big` is 256 bytes")
    }
}
