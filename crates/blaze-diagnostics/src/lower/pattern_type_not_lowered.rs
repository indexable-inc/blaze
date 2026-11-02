//! The pattern of a pattern type (`#![feature(pattern_types)]`) that is not lowered: rustc's
//! "pattern not supported in pattern types" for a pattern that is neither a range, `!null` nor
//! `|`, and a "blaze cannot" error for the range ends rustc completes with anon consts hir-def
//! does not make yet (an exclusive end, `RangeSub::sub_one`; an open one, `RangeMin::MIN` or
//! `RangeMax::MAX`).
//!
//! Follows rustc's `compiler/rustc_builtin_macros/src/pattern_type.rs` (`pat_to_ty_pat`) and
//! `compiler/rustc_ast_lowering/src/pat.rs` (`lower_ty_pat_mut`) (MIT or Apache-2.0).

use hir::PatternTypeUnsupported;
use syntax::view::AstNode as _;

use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::Diag,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::PatternTypeNotLowered) -> Vec<Found> {
    let ty = cx.node_view(d.ty);
    let span = cx.span(ty.with_value(ty.value.syntax().text_range()));
    let message = match d.reason {
        PatternTypeUnsupported::Pattern => "pattern not supported in pattern types",
    };
    // alloc: one error, for an error hir found
    vec![Found::new(Diag::error(None, message, span), Phase::Items)]
}
