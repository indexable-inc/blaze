//! The errors rustc gives of impls that hir reports nothing of, in rustc's order: coherence
//! trait by trait (`coherent_trait`: the explicit impls of a trait that forbids them, the
//! orphan rules where hir's check takes what rustc refuses, the builtin traits' checks), then
//! each impl's items: duplicates (E0201 of a trait impl, which the resolver gives; E0592 of an
//! inherent one) and each trait impl item against its trait's (`compare_impl_item`).
//!
//! What needs trait solving or inference is the front end's and asked of it
//! (`v2/requests/diagnostics.md`, `diagnostics-items-impls`): overlap (E0119, E0592 across
//! impls), unconstrained parameters (E0207), signatures that differ in type (E0053), lifetime
//! parameters that differ in being early or late bound (E0195), `Drop` impls with more bounds
//! than their type (E0367), the coercion traits (E0374 to E0378).
//!
//! Follows rustc's `compiler/rustc_hir_analysis/src/coherence/mod.rs` (`coherent_trait`,
//! `check_impl`, `enforce_trait_manually_implementable`) and
//! `compiler/rustc_hir_analysis/src/coherence/orphan.rs` (`orphan_check_impl`) (MIT or
//! Apache-2.0).

mod builtin;
mod compare;

use blaze_hash::HashMap;
use blaze_ids::radix;
use blaze_items::{Fact, ItemId};
use hir_def::{BuiltinDeriveImplId, attrs::AttrFlags, lang_item::lang_items};
use syntax::view::{AstNode as _, HasName as _};

use crate::{
    cx::{Cx, item_range_view},
    lower::{Found, Phase, incoherent_impl::ImplAt, trait_impl_orphan},
    model::{Diag, Expansion, MacroKind, Span},
};

/// Where an impl is: written in source, or made by a builtin derive of a type.
pub(super) enum Origin<'db> {
    Written(ImplAt<'db>),
    Derive { id: BuiltinDeriveImplId, adt: hir::Adt },
}

impl<'db> Origin<'db> {
    /// The span rustc gives the impl's self type: as written, or for a derive the type's name
    /// in the derive's expansion.
    pub(super) fn self_ty_span(&self, cx: &Cx<'_, '_>) -> Option<Span> {
        match self {
            Origin::Written(at) => Some(at.self_ty_span(cx)),
            Origin::Derive { id, adt } => {
                let source = cx.vsema.source(*adt)?;
                let name = source.value.name()?;
                let mut span = cx.span(source.with_value(name.syntax().text_range()));
                let loc = id.loc(cx.db);
                let derive = loc.trait_.derive_macro(lang_items(cx.db, cx.krate.base()));
                let def_site = derive
                    .and_then(|it| cx.vsema.source(hir::Macro::from(it)))
                    .map(|it| cx.span(it.map(|it| item_range_view(it.either(|it| it.syntax(), |it| it.syntax())))));
                // alloc: the error's span; called only on an error path (`builtin`'s E0204, E0206, E0184)
                span.expansion = Some(Box::new(Expansion {
                    call_site: cx.span(loc.source(cx.db)),
                    kind: MacroKind::Derive,
                    // alloc: as above
                    name: loc.trait_.name().as_str().to_owned(),
                    def_site,
                    opaque: true,
                }));
                Some(span)
            }
        }
    }

    /// The fact of the impl's coherence: its own, or for a derive the derives of its type.
    pub(super) fn coherence(&self, cx: &Cx<'_, '_>) -> Fact {
        match self {
            Origin::Written(at) => at.coherence(cx),
            Origin::Derive { adt, .. } => Fact::Derive(cx.key(ItemId::Adt((*adt).into()))),
        }
    }

    pub(super) fn written(&self) -> Option<&ImplAt<'db>> {
        match self {
            Origin::Written(at) => Some(at),
            Origin::Derive { .. } => None,
        }
    }
}

/// `scopes` is `defs::scopes`, made once per check.
pub(crate) fn check(cx: &Cx<'_, '_>, scopes: &[hir::Module]) -> Vec<Found> {
    let db = cx.db;
    let frame = cx.frame();
    let impls = impls(cx, scopes);
    // alloc: empty until a diagnostic fires.
    let mut found = Vec::new();
    // rustc's `local_trait_impls`: the traits in the order their first impl is written, each
    // impl's trait by its place among them (one probe per impl), and the impls of each trait
    // as a CSR list in impl order, all in the check's frame.
    let mut slots: HashMap<hir::Trait, u32> = HashMap::default();
    let mut traits: Vec<hir::Trait, _> = Vec::new_in(frame);
    let mut of_impl: Vec<Option<u32>, _> = Vec::with_capacity_in(impls.len(), frame);
    for (impl_, _) in &impls {
        let slot = impl_.trait_(db).map(|trait_| {
            *slots.entry(trait_).or_insert_with(|| {
                traits.push(trait_);
                u32::try_from(traits.len() - 1).expect("a crate implements fewer than 2^32 traits")
            })
        });
        of_impl.push(slot);
    }
    let mut starts: Vec<u32, _> = Vec::with_capacity_in(traits.len() + 1, frame);
    starts.resize(traits.len() + 1, 0);
    for &slot in of_impl.iter().flatten() {
        starts[slot as usize + 1] += 1;
    }
    for at in 1..starts.len() {
        starts[at] += starts[at - 1];
    }
    let mut next: Vec<u32, _> = Vec::with_capacity_in(traits.len(), frame);
    next.extend_from_slice(&starts[..traits.len()]);
    let mut members: Vec<u32, _> = Vec::with_capacity_in(starts[traits.len()] as usize, frame);
    members.resize(starts[traits.len()] as usize, 0);
    for (index, slot) in of_impl.iter().enumerate() {
        let Some(slot) = *slot else { continue };
        members[next[slot as usize] as usize] = u32::try_from(index).expect("a crate has fewer than 2^32 impls");
        next[slot as usize] += 1;
    }
    let mut builtin = builtin::Builtin::new(cx);
    let mut incoherent: Vec<bool, _> = Vec::with_capacity_in(traits.len(), frame);
    incoherent.resize(traits.len(), false);
    for (slot, trait_) in traits.iter().enumerate() {
        let denied = denies_explicit_impls(cx, *trait_);
        let mut coherent = true;
        for &index in &members[starts[slot] as usize..starts[slot + 1] as usize] {
            let (impl_, origin) = &impls[index as usize];
            let (errors, hir_error) = coherence(cx, &mut builtin, *impl_, origin, *trait_, denied);
            coherent &= !hir_error && !errors.iter().any(is_error);
            found.extend(errors);
        }
        incoherent[slot] = !coherent;
    }
    for ((_, origin), slot) in impls.iter().zip(&of_impl) {
        // rustc compares an impl's items with its trait's only when the trait is coherent
        // (`check_item_type`).
        if slot.is_some_and(|it| incoherent[it as usize]) {
            continue;
        }
        if let Origin::Written(at) = origin {
            found.extend(compare::check(cx, at));
        }
    }
    found
}

/// rustc's `coherent_trait` of `trait_`, as far as it is decided here (overlap is the front
/// end's, `v2/requests/diagnostics.md`): whether no impl of it in the crate has an error of
/// coherence, here or hir's (an orphan impl, an impl whose safety is not the trait's). When it
/// has one, rustc checks no impl's items against the trait (E0046 and `compare_impl_item`).
pub(crate) fn coherent(cx: &Cx<'_, '_>, trait_: hir::Trait) -> bool {
    let denied = denies_explicit_impls(cx, trait_);
    let mut builtin = builtin::Builtin::new(cx);
    impls(cx, &super::defs::scopes(cx)).iter().filter(|(impl_, _)| impl_.trait_(cx.db) == Some(trait_)).all(|(impl_, origin)| {
        let (errors, hir_error) = coherence(cx, &mut builtin, *impl_, origin, trait_, denied);
        !hir_error && !errors.iter().any(is_error)
    })
}

/// The coherence checks of one impl of `trait_` (`coherent_trait`'s loop): what is given here,
/// and whether hir gives an error of it (an orphan impl, a safety that is not the trait's).
fn coherence<'db>(
    cx: &Cx<'_, 'db>,
    builtin: &mut builtin::Builtin<'_, '_, 'db>,
    impl_: hir::Impl,
    origin: &Origin<'db>,
    trait_: hir::Trait,
    denied: bool,
) -> (Vec<Found>, bool) {
    // alloc: empty until a diagnostic fires.
    let mut found = Vec::new();
    let mut hir_error = false;
    if let Origin::Written(at) = origin {
        if denied {
            found.extend(explicit_impl(cx, at, trait_));
        }
        hir_error |= incorrect_safety(cx, at, trait_);
        if impl_.check_orphan_rules(cx.db) {
            found.extend(trait_impl_orphan::report(cx, at));
        } else {
            hir_error = true;
        }
    }
    found.extend(builtin.check(impl_, origin, trait_));
    (found, hir_error)
}

/// Whether hir gives `TraitImplIncorrectSafety` of the impl (rustc's `unsafety::check_item`):
/// the same rule as hir's (`crates/hir/src/diagnostics.rs`), so that both agree.
fn incorrect_safety(cx: &Cx<'_, '_>, at: &ImplAt<'_>, trait_: hir::Trait) -> bool {
    let db = cx.db;
    let impl_unsafe = at.impl_.is_unsafe(db);
    let negative = at.impl_.is_negative(db);
    let trait_unsafe = trait_.is_unsafe(db);
    let is_drop = lang_items(db, cx.krate.base()).Drop == Some(trait_.into());
    let may_dangle = is_drop && {
        let (lifetimes, types) = AttrFlags::query_generic_params(db, hir_def::GenericDefId::from(at.id));
        lifetimes.values().chain(types.values()).any(|it| it.contains(AttrFlags::MAY_DANGLE))
    };
    match (impl_unsafe, trait_unsafe, negative, may_dangle) {
        (true, _, true, _) | (true, false, _, false) => true,
        (false, true, false, _) | (false, false, _, true) => true,
        _ => false,
    }
}

fn is_error(found: &Found) -> bool {
    found.diag.is_error() && !found.diag.is_lint()
}

/// The impls of the crate, in rustc's order: each scope's (`defs::scopes`), as they are
/// written, a derive's where its type is.
fn impls<'db>(cx: &Cx<'_, 'db>, scopes: &[hir::Module]) -> Vec<(hir::Impl, Origin<'db>)> {
    let db = cx.db;
    // alloc: the crate's impls, and one buffer reused for each scope's, once per check.
    let mut all = Vec::new();
    // alloc: see above.
    let mut here: Vec<(u32, hir::Impl, Origin<'db>)> = Vec::new();
    for &module in scopes {
        let id = hir_def::ModuleId::from(module);
        let scope = id.def_map(db).module(db, id).scope(db);
        for impl_id in scope.impls() {
            let impl_ = hir::Impl::from(impl_id);
            let Some(at) = ImplAt::of(cx, impl_) else { continue };
            here.push((at.span(cx).lo, impl_, Origin::Written(at)));
        }
        for derive in scope.builtin_derive_impls() {
            let adt = hir::Adt::from(derive.loc(db).adt);
            let at = cx.span(derive.loc(db).source(db)).lo;
            here.push((at, hir::Impl::from(derive), Origin::Derive { id: derive, adt }));
        }
        radix::sort_by_key(&mut here, |(at, _, _)| *at);
        all.extend(here.drain(..).map(|(_, impl_, origin)| (impl_, origin)));
    }
    all
}

/// Whether the trait is marked `#[rustc_deny_explicit_impl]`.
fn denies_explicit_impls(cx: &Cx<'_, '_>, trait_: hir::Trait) -> bool {
    AttrFlags::query(cx.db, hir_def::TraitId::from(trait_).into()).contains(AttrFlags::RUSTC_DENY_EXPLICIT_IMPL)
}

/// `enforce_trait_manually_implementable`: E0322 (E0328 for `Unsize`) for an impl of a trait
/// marked `#[rustc_deny_explicit_impl]`.
fn explicit_impl(cx: &Cx<'_, '_>, at: &ImplAt<'_>, trait_: hir::Trait) -> Vec<Found> {
    let db = cx.db;
    let name = trait_.name(db);
    let name = name.as_str();
    let unsize = lang_items(db, cx.krate.base()).Unsize == Some(trait_.into());
    let span = at.span(cx);
    let code = if unsize { "E0328" } else { "E0322" };
    let mut diag =
        // alloc: clone: the error's message and span
        Diag::error(Some(code), format!("explicit impls for the `{name}` trait are not permitted"), span.clone());
    // alloc: the error's label
    diag.span_label(span, format!("impl of `{name}` not allowed"));
    // alloc: the error's fact
    diag.fact(Fact::Signature(cx.key(ItemId::Trait(trait_.into()))), format!("the attributes of `{name}`"));
    diag.fact(at.coherence(cx), "the header of the impl");
    // alloc: the error
    vec![Found::new(diag, Phase::Items)]
}

#[cfg(test)]
mod tests {
    /// A trait is denied explicit impls by `#[rustc_deny_explicit_impl]`, written plainly or
    /// through `cfg_attr` (rustc expands it first), and by nothing else.
    #[test]
    fn explicit_impls_of_a_denying_trait_are_errors() {
        let checked = crate::tests::checked(
            r#"//- /main.rs crate:main
#![feature(rustc_attrs)]
#[rustc_deny_explicit_impl]
pub trait Denied {}
#[cfg_attr(all(), rustc_deny_explicit_impl)]
pub trait ByCfgAttr {}
#[cfg_attr(any(), rustc_deny_explicit_impl)]
pub trait Allowed {}
pub struct S;
impl Denied for S {}
impl ByCfgAttr for S {}
impl Allowed for S {}
fn main() {}
"#,
        );
        let errors: Vec<String> = checked.messages().into_iter().filter(|it| it.starts_with("E0322: ")).collect();
        assert_eq!(
            errors,
            [
                "E0322: explicit impls for the `Denied` trait are not permitted",
                "E0322: explicit impls for the `ByCfgAttr` trait are not permitted",
            ]
        );
    }
}
