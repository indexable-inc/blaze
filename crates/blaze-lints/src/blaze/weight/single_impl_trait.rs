//! `single_impl_trait`: a trait the whole workspace implements once. The trait is an
//! abstraction with one user: every reader goes through an indirection to find the one body,
//! every change edits two signatures, and a generic or `dyn` over it buys nothing. The right
//! form is the type with inherent methods; when a second implementation comes, the trait comes
//! with it.
//!
//! The impls are counted over every crate that can name the trait ([`super::rows`]): the
//! trait's own crate in each of its configurations (so a test's mock is a second impl) and the
//! workspace crates that depend on it, each impl once by where it is written. Counted as more
//! than one: a blanket impl (`impl<T: ..> Tr for T`), an impl for a type of another crate (an
//! extension trait adds methods to a foreign type, which an inherent impl cannot), an impl for
//! a simulation's type (rule 64: the sans-IO core's sim stands beside the real driver), and a
//! trait that bounds an ADT's type parameter (rule 78: a strategy chosen by type, whose second
//! instantiation may be in waiting). Not found: a trait with no impl (another finding), an
//! `unsafe` or `auto` trait, a trait with a supertrait or generic parameters of its own, test
//! code, a trait a macro made.
//!
//! No fix: moving the methods to an inherent impl and the callers off the trait is a design
//! change, not a rewrite.

use hir_def::TraitId;
use syntax::view::{self as ast, AstNode as _, HasGenericParams as _, HasName as _, HasTypeBounds as _};

use super::{
    Lint, facts,
    rows::{Subject, Way},
    test_code,
};
use crate::check::Cx;

const LINT: Lint = Lint::SingleImplTrait;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, item: ast::Trait<'t>) {
    if !cx.on_blaze(LINT) || test_code::is_test_code(cx, item.syntax()) {
        return;
    }
    if item.unsafe_token().is_some() || item.auto_token().is_some() || item.type_bound_list().is_some() {
        return;
    }
    if item.generic_param_list().is_some_and(|it| it.generic_params().next().is_some()) {
        return;
    }
    let db = cx.db();
    let Some(hir::ModuleDef::Trait(def)) = cx.def(item.syntax()) else { return };
    let Some(subject) = Subject::of_trait(db, TraitId::from(def)) else { return };
    let seen = facts(cx, subject);
    if seen.impls.len() != 1 || seen.open || seen.all(Way::Bound) > 0 {
        return;
    }
    let Some(name) = item.name() else { return };
    let Some(range) = cx.range(name.syntax()) else { return };
    let implementor = hir::Impl::all_for_trait(db, cx.krate(), def)
        .first()
        .and_then(|it| it.self_ty(db).as_adt())
        .map(|it| it.name(db).as_str().to_owned());
    let trait_name = name.text();
    let note = "the trait is an abstraction with one user: make its methods the one type's inherent methods \
                and call them; the trait comes back with a second implementation";
    let message = match implementor {
        Some(ty) => format!("trait `{trait_name}` has one impl in the workspace, for `{ty}`"),
        None => format!("trait `{trait_name}` has one impl in the workspace"),
    };
    super::report(cx, LINT, item.syntax(), range, message, note, None);
}
