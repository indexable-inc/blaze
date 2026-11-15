//! `unchecked_index_in_loop`: a `for i in 0..v.len()` loop that indexes with `i` where an
//! iterator or `get` says what it means and can't panic:
//!
//! - `v[i]` and nothing else of `i`: the loop is `for item in v.iter()`, with no index to get
//!   wrong and no bounds check per item.
//! - `w[i]` of another sequence `w`: nothing says `w` is as long as `v`, and a shorter `w`
//!   panics at its end. `v.iter().zip(w)` stops at the shorter; `w.get(i)` hands the `None` on.
//! - `v[i + k]` or `v[i - k]`: the loop reaches `v.len() - 1`, so `i + k` passes the end on its
//!   last turns and `i - k` underflows on its first. `v.windows(k + 1)`, `v.get(i + k)`, or a
//!   range that stops where the index does (`0..v.len() - k`, `k..v.len()`).
//!
//! `v` is a slice, an array, a `Vec` or a `VecDeque` (through references), as a local or a
//! field: the types whose `iter` yields `v[0]`, `v[1]`, ... in that order. The same `v` is the
//! same place written the same way.
//!
//! The fix, for the first shape only, when every use of `i` is `&v[i]`, `v[i].field`,
//! `v[i].method(..)` or `v[i]` read, and `v` is named nowhere else in the loop: `for i in
//! 0..v.len()` becomes `for item in v.iter()` and the uses `item`, `item.field`,
//! `item.method(..)` and `*item`. Machine-applicable when `v` is a local: nothing in the loop
//! can change its length then, and the items are the same in the same order. When `v` is a
//! field, maybe-incorrect: a call in the loop may change it through `self`, which the borrow of
//! `iter` then refuses. A `v[i]` assigned or borrowed `&mut` gets no fix (`iter_mut` is the
//! author's). Outside test code.

use syntax::{
    SyntaxKind, TextRange,
    view::{
        self as ast, ArithOp, AstNode as _, BinaryOp, HasArgList as _, HasLoopBody as _,
        RangeItem as _, RangeOp,
    },
};

use super::{Fix, bound, in_test, parent_expr, report, strip, uses_of};
use crate::{blaze::Lint, check::Cx, diag::Applicability};

const LINT: Lint = Lint::UncheckedIndexInLoop;

/// The name the fix gives the item, when the loop does not use it already.
const ITEM: &str = "item";

/// How a use of the loop's index `i` reads.
enum Use<'t> {
    /// `v[i]` of the loop's own `v`, at the expression the fix rewrites, with what it becomes.
    Own(TextRange, &'static str),
    /// `v[i]` of the loop's own `v` in a place the fix can't rewrite (`v[i] = x`, `&mut v[i]`).
    OwnMut,
    /// `w[i]` of another sequence.
    Other(ast::IndexExpr<'t>),
    /// `v[i + k]`, `v[i - k]` of the loop's own `v`.
    Arith(ast::IndexExpr<'t>),
    /// Anything else: `i` printed, compared, passed.
    Free,
}

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let Some(for_) = ast::ForExpr::cast(node) else { return };
    let Some(ast::Expr::RangeExpr(range)) = for_.iterable().map(strip) else { return };
    if range.op_kind() != Some(RangeOp::Exclusive) || !range.start().is_some_and(is_zero) {
        return;
    }
    let Some(ast::Expr::MethodCallExpr(len)) = range.end().map(strip) else { return };
    if len.arg_list().is_some_and(|it| it.args().next().is_some()) {
        return;
    }
    let Some(f) = cx.method(len) else { return };
    if f.name(cx.db()).as_str() != "len" {
        return;
    }
    let Some(seq) = len.receiver() else { return };
    if !place(strip(seq)) || !sequence(cx, seq) {
        return;
    }
    let Some(local) = for_.pat().and_then(|it| bound(cx, it)) else { return };
    let Some(body) = for_.loop_body() else { return };
    let seq_text = squeeze(&cx.source_of(strip(seq).syntax()));
    let uses: Vec<Use<'t>> = uses_of(cx, body.syntax(), local)
        .into_iter()
        .map(|it| classify(cx, it, &seq_text))
        .collect(); // alloc: the uses of one loop's index, a few
    if in_test(cx, node) {
        return;
    }
    for use_ in &uses {
        let (index, message, note) = match use_ {
            Use::Other(index) => (
                *index,
                "this index is the loop counter of another sequence's length, and nothing says this one is as long",
                "a shorter sequence panics at its end: `iter().zip(..)` stops at the shorter, `get(i)` hands the `None` on",
            ),
            Use::Arith(index) => (
                *index,
                "this index steps past the loop's range: the counter reaches the last item, and `+`/`-` of it leaves the sequence",
                "`windows(k + 1)`, `get(i + k)`, or a range that stops where the index does (`0..v.len() - k`, `k..v.len()`)",
            ),
            _ => continue,
        };
        let Some(range) = cx.range(index.syntax()) else { continue };
        report(cx, LINT, index.syntax(), range, message.to_owned(), note, None);
    }
    if uses.is_empty() || !uses.iter().all(|it| matches!(it, Use::Own(..) | Use::OwnMut)) {
        return;
    }
    let (Some(pat), Some(iterable)) = (for_.pat(), for_.iterable()) else { return };
    let (Some(pat_range), Some(iterable_range)) =
        (cx.range(pat.syntax()), cx.range(iterable.syntax()))
    else {
        return;
    };
    let fix = fix(cx, &uses, &seq_text, body, pat_range, iterable_range, strip(seq));
    report(
        cx,
        LINT,
        node,
        iterable_range,
        format!("this loop counts over `{seq_text}` only to index it"),
        "iterate the items: no index to get wrong and no bounds check per item",
        fix,
    );
}

/// The fix of a loop whose every use of the index is `Own`: `None` when one is `OwnMut`, the
/// loop names the sequence otherwise, or `item` is taken.
fn fix<'t>(
    cx: &Cx<'_, 't>,
    uses: &[Use<'t>],
    seq_text: &str,
    body: ast::BlockExpr<'t>,
    pat: TextRange,
    iterable: TextRange,
    seq: ast::Expr<'t>,
) -> Option<Fix> {
    // alloc: the fix's parts, one per use
    let mut parts = vec![(pat, ITEM.to_owned()), (iterable, format!("{seq_text}.iter()"))];
    for use_ in uses {
        let Use::Own(range, text) = use_ else { return None };
        parts.push((*range, (*text).to_owned()));
    }
    // Each own use names the sequence once, as its index's base; one more expression written
    // as the sequence is a use of it the loop's borrow would meet.
    let mentions = body
        .syntax()
        .descendants()
        .filter(|it| ast::Expr::cast(*it).is_some() && squeeze(&cx.source_of(*it)) == seq_text)
        .count();
    if mentions != uses.len() {
        return None;
    }
    let taken = body.syntax().descendants().any(|it| {
        (it.kind() == SyntaxKind::NAME || it.kind() == SyntaxKind::NAME_REF) && it.text() == ITEM
    });
    if taken {
        return None;
    }
    let local = matches!(
        seq,
        ast::Expr::PathExpr(path)
            if matches!(path.path().and_then(|it| cx.path(it)), Some(hir::PathResolution::Local(_)))
    );
    Some(Fix {
        message: "iterate the items",
        parts,
        applicability: if local {
            Applicability::MachineApplicable
        } else {
            Applicability::MaybeIncorrect
        },
    })
}

/// How the use `path` of the index reads, against the loop's sequence written `seq_text`.
fn classify<'t>(cx: &Cx<'_, 't>, path: ast::PathExpr<'t>, seq_text: &str) -> Use<'t> {
    let mut at = path.syntax();
    let mut arith = false;
    if let Some(parent) = parent_expr(at)
        && let Some(bin) = ast::BinExpr::cast(parent)
        && matches!(bin.op_kind(), Some(BinaryOp::ArithOp(ArithOp::Add | ArithOp::Sub)))
        && bin.rhs().is_some_and(|it| matches!(strip(it), ast::Expr::Literal(_)))
    {
        at = parent;
        arith = true;
    }
    let Some(index) = parent_expr(at).and_then(ast::IndexExpr::cast) else { return Use::Free };
    if index.index().map(strip).map(|it| it.syntax()) != Some(at) {
        return Use::Free;
    }
    let Some(base) = index.base() else { return Use::Free };
    if !sequence(cx, base) {
        return Use::Free;
    }
    let own = squeeze(&cx.source_of(strip(base).syntax())) == seq_text;
    match (own, arith) {
        (true, true) => Use::Arith(index),
        (false, true) => Use::Free,
        (false, false) => Use::Other(index),
        (true, false) => own_use(cx, index),
    }
}

/// What the fix makes of `v[i]`, by where it is.
fn own_use<'t>(cx: &Cx<'_, 't>, index: ast::IndexExpr<'t>) -> Use<'t> {
    let Some(range) = cx.range(index.syntax()) else { return Use::OwnMut };
    let Some(parent) = parent_expr(index.syntax()) else { return Use::OwnMut };
    match parent.kind() {
        SyntaxKind::REF_EXPR => {
            let Some(reference) = ast::RefExpr::cast(parent) else { return Use::OwnMut };
            if reference.mut_token().is_some() || reference.raw_token().is_some() {
                return Use::OwnMut;
            }
            match cx.range(parent) {
                Some(it) => Use::Own(it, ITEM),
                None => Use::OwnMut,
            }
        }
        SyntaxKind::FIELD_EXPR => Use::Own(range, ITEM),
        SyntaxKind::METHOD_CALL_EXPR => {
            let Some(call) = ast::MethodCallExpr::cast(parent) else { return Use::OwnMut };
            let shared = cx.method(call).and_then(|f| f.self_param(cx.db())).is_some_and(|it| {
                matches!(it.access(cx.db()), hir::Access::Shared | hir::Access::Owned)
            });
            if shared { Use::Own(range, ITEM) } else { Use::OwnMut }
        }
        SyntaxKind::BIN_EXPR => {
            let assigned = ast::BinExpr::cast(parent).is_some_and(|bin| {
                matches!(bin.op_kind(), Some(BinaryOp::Assignment { .. }))
                    && bin.lhs().map(strip).map(|it| it.syntax()) == Some(index.syntax())
            });
            if assigned { Use::OwnMut } else { Use::Own(range, "*item") }
        }
        SyntaxKind::INDEX_EXPR => Use::OwnMut,
        _ => Use::Own(range, "*item"),
    }
}

/// Whether `expr` is a place written plainly: a path, or fields of one.
fn place(expr: ast::Expr<'_>) -> bool {
    match expr {
        ast::Expr::PathExpr(_) => true,
        ast::Expr::FieldExpr(field) => field.expr().map(strip).is_some_and(place),
        _ => false,
    }
}

/// Whether `expr` is a slice, an array, a `Vec` or a `VecDeque`, through references.
fn sequence<'t>(cx: &Cx<'_, 't>, expr: ast::Expr<'t>) -> bool {
    let Some(ty) = cx.ty(expr) else { return false };
    let ty = ty.strip_references();
    if ty.as_slice().is_some() || ty.as_array(cx.db()).is_some() {
        return true;
    }
    let Some(adt) = ty.as_adt() else { return false };
    [&["alloc", "vec", "Vec"][..], &["alloc", "collections", "VecDeque"]]
        .iter()
        .any(|path| cx.std_def(path) == Some(hir::ModuleDef::Adt(adt)))
}

fn is_zero(expr: ast::Expr<'_>) -> bool {
    matches!(strip(expr), ast::Expr::Literal(it) if it.syntax().text() == "0")
}

/// `text` without its whitespace.
fn squeeze(text: &str) -> String {
    text.chars().filter(|it| !it.is_whitespace()).collect()
}
