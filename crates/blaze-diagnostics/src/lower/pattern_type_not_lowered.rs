//! The pattern of a pattern type (`#![feature(pattern_types)]`) that is not lowered: rustc's
//! "pattern not supported in pattern types" for a pattern that is neither a range, `!null` nor
//! `|`, and a "blaze cannot" error for the range ends rustc completes with anon consts hir-def
//! does not make yet (an exclusive end, `RangeSub::sub_one`; an open one, `RangeMin::MIN` or
//! `RangeMax::MAX`).
//!
//! Follows rustc's `compiler/rustc_builtin_macros/src/pattern_type.rs` (`pat_to_ty_pat`) and
//! `compiler/rustc_ast_lowering/src/pat.rs` (`lower_ty_pat_mut`) (MIT or Apache-2.0).
//!
//! blaze: a set of enum variants (`Conn is Conn::Open { .. }`) that is refused, under or
//! without `#![blaze::pattern_types]`.

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
    let written = ty.value.syntax().text();
    // blaze: the reasons of a set of enum variants (`#![blaze::pattern_types]`,
    // docs/v2/plan/design/edition.md, section 8) name the type and the rule.
    let message = match d.reason {
        PatternTypeUnsupported::Pattern => "pattern not supported in pattern types".to_owned(),
        PatternTypeUnsupported::VariantsNeedGate => format!(
            "`{written}` names enum variants, which needs `#![blaze::pattern_types]` at the crate \
             root (rule pattern-types-gate)"
        ),
        PatternTypeUnsupported::VariantsOfNonEnum => format!(
            "`{written}` names variants of a type that is not an enum (rule pattern-types-variants)"
        ),
        PatternTypeUnsupported::NotAVariant => format!(
            "the pattern of `{written}` is not a variant of its base enum \
             (rule pattern-types-variants)"
        ),
        PatternTypeUnsupported::VariantFieldNotWildcard => format!(
            "the pattern of `{written}` holds something other than `_` and `..` inside a variant: \
             a type's pattern says which variant, never what its fields hold \
             (rule pattern-types-variants)"
        ),
        PatternTypeUnsupported::VariantShape => format!(
            "the pattern of `{written}` does not have the shape of its variant \
             (rule pattern-types-variants)"
        ),
    };
    // alloc: one error, for an error hir found
    vec![Found::new(Diag::error(None, message, span), Phase::Items)]
}
