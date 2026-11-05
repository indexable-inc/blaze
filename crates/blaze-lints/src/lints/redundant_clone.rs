//! `redundant_clone`: `x.clone()` of a local that is never used again, where moving `x` does.
//!
//! Follows clippy's `clippy_lints/src/redundant_clone.rs` (MIT or Apache-2.0), written from what
//! is known of its source rather than read from a checkout: no copy of clippy's source was at
//! hand. Its name, message, span, suggestion, note and applicability are clippy's.
//!
//! Clippy asks MIR: a call of `Clone::clone`, `ToOwned::to_owned` or, on a `String`,
//! `ToString::to_string`, whose argument is `&cloned` and the only borrow of `cloned` live there,
//! with `cloned` used on no path after the call (or the clone neither consumed nor mutated). This
//! asks the body's tree instead, with the resolutions, types and adjustments of
//! `hir::BodyAnalysis` (through `Cx`), and flags a subset of clippy's cases:
//! - only the first of clippy's two cases, `cloned` dropped without further use; never "cloned
//!   value is neither consumed nor mutated";
//! - only a receiver that is a local named by a path, of a type that is not a reference, not
//!   `Copy` and not `ManuallyDrop`, borrowed by autoref alone; not `to_path_buf` and
//!   `to_os_string` through `Deref`;
//! - "used on no path after" is "named nowhere after the call in the text", with the call in no
//!   loop, closure or `async` block that the binding is outside of: a use in another branch
//!   stops it where clippy's paths would not;
//! - "the only borrow live" is that every earlier use is a borrow that ends with its expression:
//!   a method receiver, a comparison operand, or `&x` handed to a call, where neither the call's
//!   value nor any place the call can write through can hold a borrow (see [`holds_no_borrow`]),
//!   or a formatting or assertion macro of std. Any other use, a use in a closure or `async`
//!   block, or a macro call whose tokens hold the name, stops it.
//!
//! Each of these only removes cases, so it flags nothing clippy wouldn't.

use hir::{Adjust, AsAssocItem as _, AutoBorrow, Mutability};
use syntax::{TextRange, TextSize, view as ast};

use ast::{AstNode as _, HasArgList as _, HasName as _};

use super::{
    implicit_clone::{STRING, TO_OWNED, TO_STRING, is_std_adt, of_trait, peel_refs},
    utils,
};
use crate::{check::Cx, diag::Applicability};

const LINT: super::Implemented = super::Implemented::RedundantClone;

const MANUALLY_DROP: &[&str] = &["core", "mem", "ManuallyDrop"];

/// std's macros that borrow what they are handed only while they run, and whose value holds no
/// borrow.
const BORROWING_MACROS: &[&str] = &[
    "assert",
    "assert_eq",
    "assert_ne",
    "debug_assert",
    "debug_assert_eq",
    "debug_assert_ne",
    "eprint",
    "eprintln",
    "format",
    "print",
    "println",
];

/// Where the cloned local is named, other than as the clone's receiver.
#[derive(Clone, Copy)]
enum Mention<'t> {
    Path(ast::PathExpr<'t>),
    Macro(ast::MacroCall<'t>),
}

impl<'t> Mention<'t> {
    fn syntax(self) -> ast::Node<'t> {
        match self {
            Mention::Path(it) => it.syntax(),
            Mention::Macro(it) => it.syntax(),
        }
    }
}

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    // In an expansion the uses would be searched in a tree other than the one the clone is in.
    if !cx.on(LINT) || cx.expansion().is_some() {
        return;
    }
    let Some(method) = ast::MethodCallExpr::cast(node) else { return };
    let expr = ast::Expr::MethodCallExpr(method);
    let Some(call) = utils::method_call(cx, expr) else { return };
    if !call.args().is_empty() || !matches!(call.name, "clone" | "to_owned" | "to_string") {
        return;
    }
    let ast::Expr::PathExpr(receiver) = call.receiver else { return };
    let Some(local) = utils::local_of(cx, receiver) else { return };
    let Some(function) = cx.method(method) else { return };
    let (Some(ty), Some(returned)) = (cx.ty(call.receiver), cx.ty(expr)) else { return };
    let db = cx.db();
    let from_borrow = match call.name {
        "clone" => {
            let clone = cx.lang_trait(hir::LangItem::Clone);
            clone.is_some()
                && function.as_assoc_item(db).and_then(|it| it.container_or_implemented_trait(db))
                    == clone
        }
        "to_owned" => of_trait(cx, function, TO_OWNED),
        _ => of_trait(cx, function, TO_STRING) && is_std_adt(cx, &ty, STRING),
    };
    if !from_borrow
        || ty.is_reference()
        || ty.is_raw_ptr()
        || returned != ty
        || ty.is_copy(db)
        || is_std_adt(cx, &ty, MANUALLY_DROP)
    {
        return;
    }
    // `arg = &cloned; clone(move arg)`: the receiver is only borrowed to be cloned.
    let autoref = cx.adjustments(call.receiver).is_some_and(|it| {
        matches!(
            it.as_slice(),
            [hir::Adjustment { kind: Adjust::Borrow(AutoBorrow::Ref(Mutability::Shared)), .. }]
        )
    });
    if !autoref {
        return;
    }
    let name = local.name(db);
    let name = name.as_str();
    let root = cx.root();
    let binding = root
        .descendants()
        .filter_map(ast::IdentPat::cast)
        .find(|it| it.name().is_some_and(|it| it.text() == name) && cx.local(*it) == Some(local))
        .map(|it| it.syntax().text_range());
    // A parameter's binding is outside the tree, and so outside every loop and closure in it.
    let encloses =
        |outer: ast::Node<'t>| binding.is_some_and(|it| outer.text_range().contains_range(it));
    if node.ancestors().skip(1).any(|it| (runs_again(it) || defers(it)) && !encloses(it)) {
        return;
    }
    let at = node.text_range().start();
    for element in root.descendants() {
        let mention = if let Some(path) = ast::PathExpr::cast(element) {
            if path.syntax() == receiver.syntax() || utils::local_of(cx, path) != Some(local) {
                continue;
            }
            Mention::Path(path)
        } else if let Some(field) = ast::RecordExprField::cast(element) {
            if field.expr().is_some() || field.name_ref().is_none_or(|it| it.text() != name) {
                continue;
            }
            return;
        } else if let Some(macro_call) = ast::MacroCall::cast(element) {
            // A macro may name the local where no path of the tree does (`format!("{x}")`).
            if macro_call.token_tree().is_none_or(|it| !it.syntax().text().contains(name)) {
                continue;
            }
            Mention::Macro(macro_call)
        } else {
            continue;
        };
        if mention.syntax().text_range().end() > at || !ends_borrowed(cx, mention, at) {
            return;
        }
    }
    let Some(range) = cx.range(node) else { return };
    let snippet = cx.snippet(range);
    let Some(dot) = snippet.rfind('.') else {
        let span = cx.span(range);
        cx.lint(LINT, node, span, "redundant clone");
        return;
    };
    let dot_at = range.start() + TextSize::from(dot as u32);
    let called = &snippet[dot + 1..];
    let applicability = match called.strip_suffix("()").map(str::trim) {
        Some(it) if it.bytes().all(|b| b.is_ascii_alphabetic() || b == b'_') => {
            Applicability::MachineApplicable
        }
        _ => Applicability::MaybeIncorrect,
    };
    let sugg_span = cx.span(TextRange::new(dot_at, range.end()));
    let cloned = cx.span(TextRange::new(range.start(), dot_at));
    cx.lint(LINT, node, sugg_span.clone(), "redundant clone")
        .suggestion(sugg_span, "remove this", String::new(), applicability)
        .span_note(cloned, "this value is dropped without further use");
}

/// A node whose contents may run more than once for one run of what is around it.
fn runs_again(node: ast::Node<'_>) -> bool {
    let kind = node.kind();
    ast::LoopExpr::can_cast(kind) || ast::WhileExpr::can_cast(kind) || ast::ForExpr::can_cast(kind)
}

/// A node whose contents run later, holding what they name: a closure or an `async` or `gen`
/// block.
fn defers(node: ast::Node<'_>) -> bool {
    if ast::ClosureExpr::can_cast(node.kind()) {
        return true;
    }
    ast::BlockExpr::cast(node).is_some_and(|it| {
        matches!(
            it.modifier(),
            Some(
                ast::BlockModifier::Async(_)
                    | ast::BlockModifier::AsyncGen(_)
                    | ast::BlockModifier::Gen(_)
            )
        )
    })
}

/// Whether `mention`, written before `before`, borrows the local only until its expression
/// ends there, so that no borrow of it is live at the clone.
fn ends_borrowed<'t>(cx: &Cx<'_, 't>, mention: Mention<'t>, before: TextSize) -> bool {
    if mention.syntax().ancestors().skip(1).any(defers) {
        return false;
    }
    let path = match mention {
        Mention::Macro(macro_call) => {
            return cx
                .std_macro(macro_call)
                .is_some_and(|it| BORROWING_MACROS.contains(&it.as_str()));
        }
        Mention::Path(path) => path,
    };
    // The place named: the local, or a field of it.
    let mut place = path.syntax();
    let parent = loop {
        let Some(parent) = place.parent() else { return false };
        let field_of_place = ast::FieldExpr::cast(parent)
            .and_then(|it| it.expr())
            .is_some_and(|it| it.syntax() == place);
        if ast::ParenExpr::can_cast(parent.kind()) || field_of_place {
            place = parent;
            continue;
        }
        break parent;
    };
    let ends_before = |node: ast::Node<'t>| node.text_range().end() <= before;
    match ast::Expr::cast(parent) {
        Some(ast::Expr::MethodCallExpr(call))
            if call.receiver().is_some_and(|it| it.syntax() == place) =>
        {
            ends_before(call.syntax())
                && call_keeps_no_borrow(cx, ast::Expr::MethodCallExpr(call), place)
        }
        Some(ast::Expr::BinExpr(bin)) if matches!(bin.op_kind(), Some(ast::BinaryOp::CmpOp(_))) => {
            ends_before(bin.syntax())
        }
        Some(ast::Expr::RefExpr(reference)) if reference.raw_token().is_none() => {
            let mut arg = reference.syntax();
            while let Some(paren) = arg.parent().filter(|it| ast::ParenExpr::can_cast(it.kind())) {
                arg = paren;
            }
            let Some(list) = arg.parent().and_then(ast::ArgList::cast) else { return false };
            let Some(call) = list.syntax().parent().and_then(ast::Expr::cast) else {
                return false;
            };
            matches!(call, ast::Expr::CallExpr(_) | ast::Expr::MethodCallExpr(_))
                && ends_before(call.syntax())
                && call_keeps_no_borrow(cx, call, arg)
        }
        _ => false,
    }
}

/// Whether the call `call`, handed a borrow at `borrowed` (its receiver or one of its arguments),
/// can keep none: its value holds no borrow, and nothing else it is handed, nor its callee, is a
/// place a borrow can be written to.
fn call_keeps_no_borrow<'t>(cx: &Cx<'_, 't>, call: ast::Expr<'t>, borrowed: ast::Node<'t>) -> bool {
    let db = cx.db();
    if cx.ty(call).is_none_or(|it| !holds_no_borrow(db, &it)) {
        return false;
    }
    let handed = |expr: ast::Expr<'t>| {
        expr.syntax() == borrowed
            || cx.adjusted_ty(expr).is_some_and(|it| holds_no_borrow(db, &peel_refs(it).0))
    };
    let (first, args) = match call {
        ast::Expr::MethodCallExpr(it) => (it.receiver(), it.arg_list()),
        ast::Expr::CallExpr(it) => {
            let callee = it.expr().and_then(|it| cx.ty(it));
            if callee.is_none_or(|it| !it.is_fn() || !holds_no_borrow(db, &it)) {
                return false;
            }
            (None, it.arg_list())
        }
        _ => return false,
    };
    let Some(args) = args else { return false };
    first.is_none_or(&handed) && args.args().all(&handed)
}

/// Whether no value of `ty` can hold a borrow: it is made only of scalars, `str`, `()`, `!`,
/// tuples, arrays, slices, fn items and pointers, and ADTs without lifetime parameters. A
/// reference, raw pointer, `dyn`, `impl Trait`, closure, type parameter or projection anywhere in
/// it may.
fn holds_no_borrow(db: &base_db::Db, ty: &hir::Type<'_>) -> bool {
    let mut free = true;
    ty.walk(db, |it| {
        free &= it.is_scalar()
            || it.is_str()
            || it.is_unit()
            || it.is_never()
            || it.is_tuple()
            || it.is_array()
            || it.is_slice()
            || it.is_fn()
            || it
                .as_adt()
                .is_some_and(|adt| hir::GenericDef::Adt(adt).lifetime_params(db).is_empty());
    });
    free
}
