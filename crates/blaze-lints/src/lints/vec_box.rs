//! `vec_box`: `Vec<Box<T>>` of a sized `T` smaller than `vec-box-size-threshold` (4096 bytes
//! by default): the `Vec` is on the heap already. Run by clippy's `Types` pass, in
//! [`super::box_collection`].
//!
//! Follows clippy's `clippy_lints/src/types/vec_box.rs` (MIT or Apache-2.0), ported from what is
//! known of its source at rust-lang/rust's nightly of 2026-10: no reference checkout of clippy
//! was at hand.

use syntax::view::{self as ast, AstNode as _};

use super::{
    Implemented,
    box_collection::{Std, last_args, std_of},
};
use crate::{check::Cx, diag::Applicability};

const LINT: Implemented = Implemented::VecBox;

/// clippy's `vec_box::check` of the path type `path`, which names `outer`: whether it is
/// `Vec<Box<T>>` as the lint has it, reported when the lint is on.
pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, path: ast::PathType<'t>, outer: Std) -> bool {
    if outer != Std::Vec {
        return false;
    }
    let Some(args) = last_args(path) else { return false };
    let mut args = args.generic_args();
    let Some(ast::GenericArg::TypeArg(first)) = args.next() else { return false };
    let vec_alloc = args.next();
    let Some(boxed @ ast::Type::PathType(boxed_path)) = first.ty() else { return false };
    if std_of(cx, boxed) != Some(Std::Box) {
        return false;
    }
    let Some(box_args) = last_args(boxed_path) else { return false };
    let mut box_args = box_args.generic_args();
    let Some(ast::GenericArg::TypeArg(boxed_ty)) = box_args.next() else { return false };
    let Some(boxed_ty) = boxed_ty.ty() else { return false };
    let box_alloc = box_args.next();
    let db = cx.db();
    let Some(ty) = cx.resolve_type(boxed_ty) else { return false };
    if !ty.is_sized(db) {
        return false;
    }
    let threshold = u64::try_from(cx.conf().int("vec-box-size-threshold", 4096)).expect("a count, checked as the configuration loads");
    let Ok(layout) = ty.layout(db) else { return false };
    if layout.size() >= threshold {
        return false;
    }
    // https://github.com/rust-lang/rust-clippy/issues/7114: the allocators must agree.
    let same_alloc = match (vec_alloc, box_alloc) {
        (None, None) => true,
        (None, Some(ast::GenericArg::TypeArg(inner)))
        | (Some(ast::GenericArg::TypeArg(inner)), None) => {
            inner.ty().is_some_and(|it| std_of(cx, it) == Some(Std::Global))
        }
        (Some(ast::GenericArg::TypeArg(l)), Some(ast::GenericArg::TypeArg(r))) => {
            match (l.ty().and_then(|it| cx.resolve_type(it)), r.ty().and_then(|it| cx.resolve_type(it))) {
                (Some(l), Some(r)) => l == r,
                _ => false,
            }
        }
        _ => false,
    };
    if !same_alloc {
        return false;
    }
    if !cx.on(LINT) {
        return true;
    }
    let (Some(range), Some(boxed_range)) = (cx.range(path.syntax()), cx.range(boxed_ty.syntax()))
    else {
        return true;
    };
    let alloc = match vec_alloc {
        Some(ast::GenericArg::TypeArg(it)) => match cx.range(it.syntax()) {
            Some(range) => format!(", {}", cx.snippet(range)),
            None => ", ..".to_owned(),
        },
        _ => String::new(),
    };
    let code = format!("Vec<{}{alloc}>", cx.snippet(boxed_range));
    let span = cx.span(range);
    cx.lint(LINT, path.syntax(), span.clone(), "`Vec<T>` is already on the heap, the boxing is unnecessary")
        .suggestion(span, "try", code, Applicability::Unspecified);
    true
}
