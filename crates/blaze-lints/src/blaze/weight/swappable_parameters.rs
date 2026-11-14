//! `swappable_parameters`: two adjacent parameters of one primitive or string type
//! (`fn copy(src: &str, dst: &str)`, `fn resize(width: u32, height: u32)`): a call that passes
//! them in each other's place compiles and is wrong, and nothing but the names can catch it
//! (`swapped_arguments` does, where the arguments have names). The declaration side of the same
//! mistake: the right form makes the swap a type error, a newtype for each (`Src`, `Dst`), or
//! one struct of named fields the caller writes by name.
//!
//! Found on the function's declaration, its parameters after `self` as inference types them:
//! an integer, a float, a `char`, a `str` or a `String` (behind any references), the same for
//! both. A `bool` pair is `bool_param`'s. One finding per function, at its first such pair. Not
//! found: a private function with one call site (the one caller is its whole risk, read once),
//! a trait's function or an impl's of one (the trait's signature decides), `extern`, `main` and
//! test functions, a parameter named `_..`.
//!
//! No fix: the newtypes and their names are the author's.

use syntax::view::{self as ast, AstNode as _};

use super::{
    Lint, facts, fn_subject, judged_fn,
    rows::Way,
};
use crate::{blaze::facts as type_facts, check::Cx};

const LINT: Lint = Lint::SwappableParameters;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, f: ast::Fn<'t>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let Some(func) = judged_fn(cx, f) else { return };
    let db = cx.db();
    let params = func.params_without_self(db);
    if params.len() < 2 {
        return;
    }
    let Some(list) = f.param_list() else { return };
    // alloc: the function's few parameters
    let written: Vec<ast::Param<'t>> = list.params().collect();
    if written.len() != params.len() {
        return;
    }
    let pair = (0..params.len() - 1).find(|&i| {
        let (a, b) = (params[i].ty(), params[i + 1].ty());
        a == b && swappable(db, a) && !named_quiet(cx, written[i]) && !named_quiet(cx, written[i + 1])
    });
    let Some(i) = pair else { return };
    if !cx.is_exported(f.syntax()) {
        let calls = fn_subject(cx, func).map_or(0, |it| facts(cx, it).all(Way::Call));
        if calls <= 1 {
            return;
        }
    }
    let (Some(first), Some(second)) = (cx.range(written[i].syntax()), cx.range(written[i + 1].syntax())) else {
        return;
    };
    let message = format!(
        "`{}` and `{}` are adjacent parameters of `fn {}` of one type: a call that swaps them compiles",
        cx.snippet(first),
        cx.snippet(second),
        func.name(db).as_str()
    );
    super::report(
        cx,
        LINT,
        written[i].syntax(),
        first.cover(second),
        message,
        "give each a newtype, or take one struct of named fields, so that a swap is a type error",
        None,
    );
}

/// Whether `ty` is a type a swap of hides: an integer, a float, a `char`, a `str` or a
/// `String`, behind any references.
fn swappable(db: &base_db::Db, ty: &hir::Type<'_>) -> bool {
    let ty = ty.strip_references();
    ty.is_int_or_uint()
        || ty.is_float()
        || ty.is_char()
        || ty.is_str()
        || ty.as_adt().is_some_and(|adt| type_facts::adt_is(db, adt, "alloc", "String"))
}

/// Whether the parameter's pattern is `_` or `_name`, which says it is not used.
fn named_quiet<'t>(cx: &Cx<'_, 't>, param: ast::Param<'t>) -> bool {
    param
        .pat()
        .is_some_and(|it| cx.source_of(it.syntax()).trim_start_matches("mut ").starts_with('_'))
}
