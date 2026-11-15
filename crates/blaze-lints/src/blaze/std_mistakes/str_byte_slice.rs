//! `str_byte_slice`: a `str` sliced (`&s[a..b]`, `&s[a..]`, `&s[..b]`) or split
//! (`s.split_at(n)`) at a byte index nothing says is on a character boundary. Inside a
//! multi-byte character the slice panics: code tested on ASCII crashes on the first `é`, `—` or
//! emoji in a user's path, name or message.
//!
//! Found, outside test code: an index of a value whose type, references peeled, is `str` (a
//! `String` derefs to it) by a range one of whose bounds is not a constant; a call resolved to
//! `str`'s own `split_at` or `split_at_mut` with an argument that is not. Each is read from the
//! typed tree, never the text. A bound is taken as on a boundary, and the place is left alone,
//! when it is std's `len` of a string; when it, or what made a local it names (the `let`'s
//! initializer, the `for`'s or `if let`'s expression), calls one of std's [`BOUNDARY_METHODS`]
//! (`find`, `char_indices`, `len_utf8`, `floor_char_boundary`, ..); or when a call of std's
//! `is_char_boundary` on the same string with the same index (same by resolution) dominates
//! the slice: in an enclosing condition, or in a statement before it (`assert!` included).
//! These only ever suppress a finding, never make one.
//!
//! The fix, where the function returns an `Option` (and the slice is in no closure or `async`
//! block), is `s.get(a..b)?` for `&s[a..b]` and `s.split_at_checked(n)?` for `s.split_at(n)`,
//! machine-applicable: a panic becomes a `None` handed on, a refusal made explicit, and the
//! value is the same `&str` otherwise. With no `Option` to hand on there is no fix: `get` with
//! an error naming the string and the index is the author's.

use syntax::{
    SyntaxKind,
    view::{self as ast, AstNode as _, HasArgList as _, RangeItem as _},
};

use super::{Fix, dominated, in_test, report, returns_option, same_place, std_method, strip};
use crate::{blaze::Lint, check::Cx, diag::Applicability};

const LINT: Lint = Lint::StrByteSlice;

/// std's methods whose value is a byte index on a character boundary (or the iterator of
/// them), resolved, so a bound made from one is on a boundary.
const BOUNDARY_METHODS: &[&str] = &[
    "find",
    "rfind",
    "char_indices",
    "match_indices",
    "rmatch_indices",
    "len_utf8",
    "floor_char_boundary",
    "ceil_char_boundary",
];

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let found = match (ast::IndexExpr::cast(node), ast::MethodCallExpr::cast(node)) {
        (Some(index), _) => sliced(cx, index),
        (_, Some(call)) => split(cx, call),
        _ => None,
    };
    let Some((what, fix)) = found else { return };
    if in_test(cx, node) {
        return;
    }
    let Some(range) = cx.range(node) else { return };
    report(
        cx,
        LINT,
        node,
        range,
        format!("{what} at a byte index nothing says is on a character boundary"),
        "inside a multi-byte character (`é`, `—`, an emoji) the slice panics; code tested on ASCII crashes on the first one",
        fix,
    );
}

/// What `index`, `s[range]` of a `str`, is and its fix.
fn sliced<'t>(cx: &Cx<'_, 't>, index: ast::IndexExpr<'t>) -> Option<(&'static str, Option<Fix>)> {
    let base = index.base()?;
    let ast::Expr::RangeExpr(bounds) = strip(index.index()?) else { return None };
    if !is_str(cx, base) {
        return None;
    }
    // alloc: a range's two bounds at most
    let open: Vec<_> = [bounds.start(), bounds.end()]
        .into_iter()
        .flatten()
        .filter(|it| !super::constant(cx, *it) && !on_boundary(cx, *it))
        .collect();
    if open.is_empty() || open.iter().all(|it| checked(cx, index.syntax(), base, *it)) {
        return None;
    }
    let fix = (|| {
        let parent = ast::RefExpr::cast(super::parent_expr(index.syntax())?)?;
        if parent.mut_token().is_some() || !returns_option(cx, index.syntax()) {
            return None;
        }
        let (at, base, range) =
            (cx.range(parent.syntax())?, cx.range(base.syntax())?, cx.range(index.index()?.syntax())?);
        Some(Fix {
            message: "hand a `None` on where the index is inside a character",
            // alloc: the fix's one part
            parts: vec![(at, format!("{}.get({})?", cx.snippet(base), cx.snippet(range)))],
            applicability: Applicability::MachineApplicable,
        })
    })();
    Some(("a `str` sliced", fix))
}

/// What `call`, `str::split_at` or `split_at_mut`, is and its fix.
fn split<'t>(cx: &Cx<'_, 't>, call: ast::MethodCallExpr<'t>) -> Option<(&'static str, Option<Fix>)> {
    let f = cx.method(call)?;
    let db = cx.db();
    let checked_name = match f.name(db).as_str() {
        "split_at" => "split_at_checked",
        "split_at_mut" => "split_at_mut_checked",
        _ => return None,
    };
    let hir::AssocItemContainer::Impl(imp) = hir::AsAssocItem::as_assoc_item(f, db)?.container(db)
    else {
        return None;
    };
    if imp.trait_(db).is_some() || !imp.self_ty(db).is_str() {
        return None;
    }
    let mut args = call.arg_list()?.args();
    let (Some(at), None) = (args.next(), args.next()) else { return None };
    let receiver = call.receiver()?;
    if super::constant(cx, at) || on_boundary(cx, at) || checked(cx, call.syntax(), receiver, at) {
        return None;
    }
    let name = call.name_ref()?;
    let fix = (|| {
        if !returns_option(cx, call.syntax()) {
            return None;
        }
        let (whole, name_at) = (cx.range(call.syntax())?, cx.range(name.syntax())?);
        Some(Fix {
            message: "hand a `None` on where the index is inside a character",
            // alloc: the fix's two parts
            parts: vec![
                (name_at, checked_name.to_owned()),
                (syntax::TextRange::empty(whole.end()), "?".to_owned()),
            ],
            applicability: Applicability::MachineApplicable,
        })
    })();
    Some(("a `str` split", fix))
}

/// Whether `expr`'s type, references peeled, is `str`, or a `String`, which an index derefs to
/// `str`.
fn is_str<'t>(cx: &Cx<'_, 't>, expr: ast::Expr<'t>) -> bool {
    let Some(ty) = cx.ty(expr) else { return false };
    let ty = ty.strip_references();
    if ty.is_str() {
        return true;
    }
    let Some(hir::ModuleDef::Adt(string)) = cx.std_def(&["alloc", "string", "String"]) else {
        return false;
    };
    ty.as_adt() == Some(string)
}

/// Whether a call of std's `is_char_boundary` on `string` with `index`, the same places by
/// resolution, dominates `at`.
fn checked<'t>(cx: &Cx<'_, 't>, at: ast::Node<'t>, string: ast::Expr<'t>, index: ast::Expr<'t>) -> bool {
    let check = |node: ast::Node<'t>| {
        let Some(call) = ast::MethodCallExpr::cast(node) else { return false };
        if !std_method(cx, call, "is_char_boundary") {
            return false;
        }
        let mut args = call.arg_list().into_iter().flat_map(|it| it.args());
        call.receiver().is_some_and(|it| same_place(cx, it, string))
            && args.next().is_some_and(|it| same_place(cx, it, index))
    };
    dominated(cx, at, &check)
}

/// Whether `scope` calls one of std's [`BOUNDARY_METHODS`], resolved.
fn reads_boundary<'t>(cx: &Cx<'_, 't>, scope: ast::Node<'t>) -> bool {
    scope
        .descendants()
        .filter_map(ast::MethodCallExpr::cast)
        .any(|call| BOUNDARY_METHODS.iter().any(|name| std_method(cx, call, name)))
}

/// Whether the bound `expr` is taken as on a character boundary: std's `len` of a string, or it
/// or what made a local it names calls one of [`BOUNDARY_METHODS`].
fn on_boundary<'t>(cx: &Cx<'_, 't>, expr: ast::Expr<'t>) -> bool {
    if let ast::Expr::MethodCallExpr(call) = strip(expr)
        && std_method(cx, call, "len")
        && call.receiver().is_some_and(|it| is_str(cx, it))
    {
        return true;
    }
    if reads_boundary(cx, expr.syntax()) {
        return true;
    }
    let Some(body) = super::body_of(expr.syntax(), false) else { return false };
    for node in expr.syntax().descendants() {
        let Some(path) = ast::PathExpr::cast(node) else { continue };
        let Some(hir::PathResolution::Local(local)) = path.path().and_then(|it| cx.path(it)) else {
            continue;
        };
        let binding = body
            .descendants()
            .filter_map(ast::IdentPat::cast)
            .find(|it| cx.local(*it) == Some(local));
        let Some(binding) = binding else { continue };
        if made_by(binding.syntax()).is_some_and(|it| reads_boundary(cx, it.syntax())) {
            return true;
        }
    }
    false
}

/// The expression whose value the pattern `pat` is in binds: a `let`'s initializer, a `for`'s
/// iterable, an `if let`'s or `while let`'s scrutinee.
fn made_by(pat: ast::Node<'_>) -> Option<ast::Expr<'_>> {
    let mut child = pat;
    for ancestor in pat.ancestors().skip(1) {
        match ancestor.kind() {
            SyntaxKind::LET_STMT => return ast::LetStmt::cast(ancestor)?.initializer(),
            SyntaxKind::FOR_EXPR => {
                let it = ast::ForExpr::cast(ancestor)?;
                return it.pat().is_some_and(|it| it.syntax() == child).then(|| it.iterable())?;
            }
            SyntaxKind::LET_EXPR => return ast::LetExpr::cast(ancestor)?.expr(),
            k if ast::Pat::can_cast(k) => {}
            _ => return None,
        }
        child = ancestor;
    }
    None
}
