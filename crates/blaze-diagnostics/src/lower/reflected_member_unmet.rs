//! hir's `ReflectedMemberUnmet`: a field binder `for<F: Field<C>> P(F)` or `for<V: Variant<C>>
//! P(V)` (`#![feature(blaze_comptime)]`, `docs/v2/plan/design/comptime.md`) that does not hold
//! because `P` fails of one member of `C`. The error names that member, the first failing one in
//! declaration order, and its type: "field `b` of `S` has type `NotHash`, which does not
//! implement `Hash`"; the error `P` gives for it follows as a note with its own notes. blaze's
//! own extension, so no rustc text to follow.
//!
//! Cost: nothing until the solver reports the error; then the inner error lowered once.

use crate::{
    cx::Cx,
    lints::code_fact,
    lower::{Found, Phase, lower as lower_any},
    model::Diag,
};

pub(crate) fn lower<'db>(cx: &Cx<'_, 'db>, d: hir::ReflectedMemberUnmet<'db>) -> Vec<Found> {
    let at = d.span.map(|it| it.syntax_node_ptr());
    let span = cx.span_of(at);
    let container = cx.ty(&d.container);
    // alloc: each arm, the error's text, for an error hir found
    let (member, domain) = match d.member {
        hir::ReflectedMember::Field(field) => {
            (format!("field `{}`", field.name(cx.db).display(cx.db, cx.edition)), "field")
        }
        hir::ReflectedMember::TupleField(index) => (format!("field `{index}`"), "field"),
        hir::ReflectedMember::Variant(variant) => {
            (format!("variant `{}`", variant.name(cx.db).display(cx.db, cx.edition)), "variant")
        }
    };
    let member_ty = d.member_ty.as_ref().map(|ty| cx.ty(ty));
    // The trait the member's own type misses, when that is why the bound fails.
    let missing_trait = match &d.inner {
        hir::AnyDiagnostic::UnimplementedTrait(unimplemented)
            if unimplemented.trait_predicate.polarity() == hir::ClausePolarity::Positive =>
        {
            let trait_ref = unimplemented.trait_predicate.trait_ref();
            // The self type is the trait ref's first type argument.
            (trait_ref.get_type_argument(0).map(|self_ty| cx.ty(&self_ty)) == member_ty)
                .then(|| trait_ref.trait_().name(cx.db).display(cx.db, cx.edition).to_string())
        }
        _ => None,
    };
    // alloc: each arm, the error's text, for an error hir found
    let message = match (&member_ty, &missing_trait) {
        (Some(ty), Some(trait_)) => {
            format!(
                "{member} of `{container}` has type `{ty}`, which does not implement `{trait_}`"
            )
        }
        (Some(ty), None) => {
            format!(
                "{member} of `{container}`, of type `{ty}`, does not meet the bound over every {domain} of `{container}`"
            )
        }
        (None, _) => format!(
            "{member} of `{container}` does not meet the bound over every {domain} of `{container}`"
        ),
    };
    // clone: on the error path, for an error hir found
    let mut diag = Diag::error(None, message, span.clone());
    // alloc: the label's text, for an error hir found
    diag.span_label(span, format!("required of every {domain} of `{container}` by this bound"));
    for inner in lower_any(cx, d.inner) {
        diag.note(inner.diag.message);
        diag.children.extend(inner.diag.children);
    }
    code_fact(cx, &mut diag, at);
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Bodies)]
}
