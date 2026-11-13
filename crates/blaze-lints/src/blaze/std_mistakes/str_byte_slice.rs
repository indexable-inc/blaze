//! `str_byte_slice`: a `str` sliced (`&s[a..b]`, `&s[a..]`, `&s[..b]`) or split
//! (`s.split_at(n)`) at a byte index nothing says is on a character boundary. Inside a
//! multi-byte character the slice panics: code tested on ASCII crashes on the first `é`, `—` or
//! emoji in a user's path, name or message.
//!
//! Found, outside test code: an index of a value whose type, references peeled, is `str` (a
//! `String` derefs to it) by a range one of whose bounds is not a constant; a call resolved to
//! `str`'s own `split_at` or `split_at_mut` with an argument that is not. A bound is taken as on
//! a boundary, and the place is left alone, when it is `x.len()` of a string, when it or the
//! `let` that made the local it names reads `find`, `rfind`, `char_indices`, `match_indices`,
//! `len_utf8` or `floor_char_boundary`, or when the function calls `is_char_boundary`: these
//! read the function's text, so they suppress a finding, never make one.
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

use super::{Fix, in_test, report, returns_option, strip};
use crate::{blaze::Lint, check::Cx, diag::Applicability};

const LINT: Lint = Lint::StrByteSlice;

/// What, read in a bound or the `let` of a local it names, puts the index on a boundary.
const BOUNDARY_READS: &[&str] =
    &["find(", "rfind(", "char_indices(", "match_indices(", "len_utf8(", "char_boundary("];

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
    if !cx.ty(base)?.strip_references().is_str() && !derefs_to_str(cx, base) {
        return None;
    }
    let mut bounds = [bounds.start(), bounds.end()].into_iter().flatten();
    if bounds.all(|it| super::constant(cx, it) || on_boundary(cx, it)) {
        return None;
    }
    let checked = index
        .syntax()
        .ancestors()
        .find_map(ast::Fn::cast)
        .is_some_and(|f| f.syntax().text().contains("is_char_boundary("));
    if checked {
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
    let name = call.name_ref()?;
    let what = match name.text() {
        "split_at" => "a `str` split",
        "split_at_mut" => "a `str` split",
        _ => return None,
    };
    let f = cx.method(call)?;
    let db = cx.db();
    let hir::AssocItemContainer::Impl(imp) = hir::AsAssocItem::as_assoc_item(f, db)?.container(db)
    else {
        return None;
    };
    if imp.trait_(db).is_some() || !imp.self_ty(db).is_str() {
        return None;
    }
    let mut args = call.arg_list()?.args();
    let (Some(at), None) = (args.next(), args.next()) else { return None };
    let checked = call
        .syntax()
        .ancestors()
        .find_map(ast::Fn::cast)
        .is_some_and(|f| f.syntax().text().contains("is_char_boundary("));
    if super::constant(cx, at) || on_boundary(cx, at) || checked {
        return None;
    }
    let fix = (|| {
        if !returns_option(cx, call.syntax()) {
            return None;
        }
        let (whole, name_at) = (cx.range(call.syntax())?, cx.range(name.syntax())?);
        let checked = match name.text() {
            "split_at" => "split_at_checked",
            _ => "split_at_mut_checked",
        };
        Some(Fix {
            message: "hand a `None` on where the index is inside a character",
            // alloc: the fix's two parts
            parts: vec![(name_at, checked.to_owned()), (syntax::TextRange::empty(whole.end()), "?".to_owned())],
            applicability: Applicability::MachineApplicable,
        })
    })();
    Some((what, fix))
}

/// Whether `expr` is a `String` (or a reference to one), which an index derefs to `str`.
fn derefs_to_str<'t>(cx: &Cx<'_, 't>, expr: ast::Expr<'t>) -> bool {
    let Some(ty) = cx.ty(expr) else { return false };
    let Some(hir::ModuleDef::Adt(string)) = cx.std_def(&["alloc", "string", "String"]) else {
        return false;
    };
    ty.strip_references().as_adt() == Some(string)
}

/// Whether the bound `expr` is taken as on a character boundary: `x.len()` of a string, or it
/// or the `let` of a local it names reads one of [`BOUNDARY_READS`].
fn on_boundary<'t>(cx: &Cx<'_, 't>, expr: ast::Expr<'t>) -> bool {
    let reads = |text: &str| BOUNDARY_READS.iter().any(|it| text.contains(it));
    if let ast::Expr::MethodCallExpr(call) = strip(expr)
        && call.name_ref().is_some_and(|it| it.text() == "len")
        && let Some(receiver) = call.receiver()
        && (cx.ty(receiver).is_some_and(|it| it.strip_references().is_str()) || derefs_to_str(cx, receiver))
    {
        return true;
    }
    if reads(expr.syntax().text()) {
        return true;
    }
    let Some(body) = super::body_of(expr.syntax(), false) else { return false };
    for node in expr.syntax().descendants() {
        let Some(path) = ast::PathExpr::cast(node) else { continue };
        let Some(hir::PathResolution::Local(local)) = path.path().and_then(|it| cx.path(it)) else {
            continue;
        };
        let made_there = body.descendants().filter(|it| it.kind() == SyntaxKind::LET_STMT).any(|it| {
            let Some(stmt) = ast::LetStmt::cast(it) else { return false };
            let binds = stmt.pat().into_iter().flat_map(|pat| pat.syntax().descendants()).any(|it| {
                ast::IdentPat::cast(it).and_then(|it| cx.local(it)) == Some(local)
            });
            binds && stmt.initializer().is_some_and(|it| reads(it.syntax().text()))
        });
        if made_there {
            return true;
        }
    }
    false
}
