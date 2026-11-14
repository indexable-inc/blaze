//! `clone_to_satisfy`: a `.clone()` of a value that owns heap memory (`String`, `Vec`,
//! `VecDeque`, `Box`, a hash or B-tree collection, `PathBuf`, `OsString`) made only to be
//! borrowed at once, the shape of a clone added to quiet the borrow checker:
//!
//! - `&x.clone()`: a reference to the clone, where a reference to `x` is the same value.
//! - `x.clone().m(..)`, `m` a method taking `&self`: the method reads the clone and the clone
//!   is dropped at the end of the statement.
//!
//! Each allocates and copies the whole value for a read. The fix borrows `x` instead: `&x` (or
//! `x` itself when it is already a `&T`, `&*x` when it is a `&mut T`), and `x.m(..)`. The value
//! read is equal (std's `Clone` of these types copies every element), and only the allocation
//! goes. Machine-applicable for `&x.clone()` handed to a call as an argument where the call
//! names `x` nowhere else, so the longer borrow can't meet a `&mut x` there; else
//! maybe-incorrect: a borrow of `x` that lasts longer than the clone did (`let r = &x.clone();`,
//! a borrowing method's result kept) may then meet a later `&mut x`, which the compiler refuses.
//!
//! Not found: `let y = x.clone();` with `y` only borrowed afterwards (that walk is
//! `arc_clone_only_borrowed`'s, for an `Arc`; `redundant_clone` finds the clone of a value
//! never used again). Outside test code.

use syntax::{
    SyntaxKind,
    view::{self as ast, AstNode as _, HasArgList as _},
};

use super::{
    Fix, in_test, of_trait, parent_expr, receiver_of, receiver_text, report, strip, uses_of,
};
use crate::{blaze::Lint, check::Cx, diag::Applicability};

const LINT: Lint = Lint::CloneToSatisfy;

/// The std types that own heap memory a clone copies.
const HEAP: &[&[&str]] = &[
    &["alloc", "string", "String"],
    &["alloc", "vec", "Vec"],
    &["alloc", "collections", "VecDeque"],
    &["alloc", "collections", "BTreeMap"],
    &["alloc", "collections", "BTreeSet"],
    &["alloc", "boxed", "Box"],
    &["std", "collections", "HashMap"],
    &["std", "collections", "HashSet"],
    &["std", "path", "PathBuf"],
    &["std", "ffi", "OsString"],
];

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let Some(clone) = ast::MethodCallExpr::cast(node) else { return };
    if clone.arg_list().is_some_and(|it| it.args().next().is_some()) {
        return;
    }
    let Some(f) = cx.method(clone) else { return };
    if f.name(cx.db()).as_str() != "clone" || !of_trait(cx, f, &["core", "clone", "Clone"]) {
        return;
    }
    let Some(receiver) = clone.receiver() else { return };
    let Some(made) = cx.ty(ast::Expr::MethodCallExpr(clone)) else { return };
    let Some(heap) = heap_type(cx, &made) else { return };
    let Some(had) = cx.ty(receiver) else { return };
    let Some(receiver_range) = cx.range(receiver.syntax()) else { return };
    let text = cx.snippet(receiver_range).to_owned();
    // `x` is the clone's type itself, a `&` of it or a `&mut` of it.
    let (borrowed, same) = if had == made {
        (format!("&{}", receiver_text(cx, receiver, receiver_range)), true)
    } else {
        match had.as_reference() {
            Some((inner, hir::Mutability::Shared)) if inner == made => (text.clone(), false),
            Some((inner, hir::Mutability::Mut)) if inner == made => {
                (format!("&*{}", receiver_text(cx, receiver, receiver_range)), false)
            }
            _ => return,
        }
    };
    let parent = parent_expr(node);
    let (at, message, fix) = if let Some(reference) = parent.and_then(ast::RefExpr::cast)
        && reference.mut_token().is_none()
        && reference.raw_token().is_none()
    {
        let Some(range) = cx.range(reference.syntax()) else { return };
        let applicability = if handed_alone(cx, reference.syntax(), receiver) {
            Applicability::MachineApplicable
        } else {
            Applicability::MaybeIncorrect
        };
        let message =
            format!("this `&` of a clone of a `{heap}` borrows a copy of what it could borrow");
        let fix = Fix {
            message: "borrow the value itself",
            // alloc: the fix's one part
            parts: vec![(range, borrowed)],
            applicability,
        };
        (reference.syntax(), message, fix)
    } else if let Some(outer) = receiver_of(node)
        && let Some(m) = cx.method(outer)
        && m.self_param(cx.db()).is_some_and(|it| it.access(cx.db()) == hir::Access::Shared)
        && same
    {
        let Some(range) = cx.range(node) else { return };
        let name = m.name(cx.db());
        let message = format!(
            "this clone of a `{heap}` is only read by `.{}(..)`, which borrows it",
            name.as_str()
        );
        let fix = Fix {
            message: "call the method on the value itself",
            // alloc: the fix's one part
            parts: vec![(range, text)],
            applicability: Applicability::MaybeIncorrect,
        };
        (node, message, fix)
    } else {
        return;
    };
    if in_test(cx, node) {
        return;
    }
    let Some(range) = cx.range(at) else { return };
    report(
        cx,
        LINT,
        at,
        range,
        message,
        "the clone allocates and copies the whole value for one read, and is dropped at once",
        Some(fix),
    );
}

/// Which of [`HEAP`] `ty` is, by its name.
fn heap_type(cx: &Cx<'_, '_>, ty: &hir::Type<'_>) -> Option<&'static str> {
    let adt = ty.as_adt()?;
    HEAP.iter().find_map(|path| match cx.std_def(path) {
        Some(hir::ModuleDef::Adt(it)) if it == adt => path.last().copied(),
        _ => None,
    })
}

/// Whether `reference`, `&x.clone()`, is an argument of a call that names `x` (a local)
/// nowhere else, so borrowing `x` for the call can't meet a `&mut x` the call also takes.
fn handed_alone<'t>(cx: &Cx<'_, 't>, reference: ast::Node<'t>, receiver: ast::Expr<'t>) -> bool {
    let Some(list) = parent_expr(reference) else { return false };
    if list.kind() != SyntaxKind::ARG_LIST {
        return false;
    }
    let Some(call) = list.parent() else { return false };
    let ast::Expr::PathExpr(path) = strip(receiver) else { return false };
    let Some(hir::PathResolution::Local(local)) = path.path().and_then(|it| cx.path(it)) else {
        return false;
    };
    uses_of(cx, call, local).len() == 1
}
