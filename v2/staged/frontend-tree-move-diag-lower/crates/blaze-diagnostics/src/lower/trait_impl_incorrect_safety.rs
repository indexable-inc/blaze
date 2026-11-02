//! E0199, E0200 and E0569: an impl whose `unsafe` does not match its trait (hir's
//! `TraitImplIncorrectSafety`): an `unsafe` impl of a safe trait, a safe impl of an `unsafe`
//! trait, and a safe `Drop` impl with a `#[may_dangle]` parameter. hir joins two of rustc's
//! AST validation errors to these, which `checks::ast_validation` gives as its walk comes to
//! the impl: E0198 (an `unsafe` negative impl) and E0197 (an `unsafe` inherent impl).
//!
//! Follows rustc's `compiler/rustc_hir_analysis/src/coherence/unsafety.rs` (`check_item`)
//! (MIT or Apache-2.0).

use blaze_items::{Fact, ItemId};
use hir::InFile;

use crate::{
    cx::Cx,
    lower::{
        Found, Phase,
        incoherent_impl::{ImplAt, unresolved},
    },
    model::{Applicability, Diag},
};

const INVARIANTS: &str = "enforces invariants that the compiler can't check. Review the trait documentation and make sure this implementation upholds those invariants before adding the `unsafe` keyword";

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::TraitImplIncorrectSafety) -> Vec<Found> {
    let Some(at) = ImplAt::of_ptr(cx, d.file_id, d.impl_) else {
        if validations(&cx.node(InFile::new(d.file_id, d.impl_)).value, d) {
            // alloc: empty, allocates nothing
            return Vec::new();
        }
        return unresolved(cx, d.file_id, d.impl_, "an impl safety error");
    };
    if validations(&at.node.value, d) {
        // alloc: empty, allocates nothing
        return Vec::new();
    }
    let db = cx.db;
    let Some(trait_ref) = at.impl_.trait_ref(db) else {
        return unresolved(cx, d.file_id, d.impl_, "an impl safety error");
    };
    let trait_ = trait_ref.trait_();
    let printed = crate::print::trait_path(cx, &trait_ref);
    let span = at.span(cx);
    let mut diag = if d.should_be_safe {
        let mut diag =
            // alloc: the error's text, for an error hir found
            Diag::error(Some("E0199"), format!("implementing the trait `{printed}` is not unsafe"), span.clone());
        diag.suggestion_verbose(
            span.with_hi(span.lo + "unsafe ".len() as u32),
            "remove `unsafe` from this trait implementation",
            "",
            Applicability::MachineApplicable,
        );
        diag
    } else if trait_.is_unsafe(db) {
        let mut diag = Diag::error(
            Some("E0200"),
            // alloc: the error's text, for an error hir found
            format!("the trait `{printed}` requires an `unsafe impl` declaration"),
            // clone: on the error path, for an error hir found
            span.clone(),
        );
        // alloc: the error's text, for an error hir found
        diag.note(format!("the trait `{printed}` {INVARIANTS}"));
        add_unsafe(&mut diag, &span);
        diag
    } else {
        let mut diag = Diag::error(
            Some("E0569"),
            "requires an `unsafe impl` declaration due to `#[may_dangle]` attribute",
            // clone: on the error path, for an error hir found
            span.clone(),
        );
        // alloc: the error's text, for an error hir found
        diag.note(format!("the trait `{printed}` {INVARIANTS}"));
        add_unsafe(&mut diag, &span);
        diag
    };
    diag.fact(at.coherence(cx), "the header of the impl");
    // alloc: the error's text, for an error hir found
    diag.fact(Fact::Signature(cx.key(ItemId::Trait(trait_.into()))), format!("whether `{printed}` is unsafe"));
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Items)]
}

fn add_unsafe(diag: &mut Diag, span: &crate::model::Span) {
    diag.suggestion_verbose(
        span.shrink_to_lo(),
        "add `unsafe` to this trait implementation",
        "unsafe ",
        Applicability::MaybeIncorrect,
    );
}

/// Whether `d` of the impl `node` is one of AST validation's errors: an inherent impl (E0197
/// of an `unsafe` one; hir has no other safety error of it) or an `unsafe` negative impl
/// (E0198).
fn validations(node: &syntax::ast::Impl, d: &hir::TraitImplIncorrectSafety) -> bool {
    node.trait_().is_none() || d.should_be_safe && node.unsafe_token().is_some() && node.excl_token().is_some()
}
