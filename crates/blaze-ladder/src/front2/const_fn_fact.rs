//! What a call of a fn item does in a constant (`ConstWorld::fn_kind`), read from the facts
//! front2 holds. A fact it needs and does not find is the evaluator's own typed kind,
//! `FnKind::Missing(Fact)`, which the evaluator refuses as `Refusal::FnFact` naming the callee
//! and the fact: a PENDING refusal (blaze-consteval machine/late.rs `Wait::FnFact`), so code
//! holding it is compiled again once the fact is made (rule 3: no fallback; rule 19: the error
//! names the thing). "Not a `const fn`" is said ONLY of a callee whose head was read and has no
//! `const` (the Reference, "Constant evaluation", "Const functions": a call in a constant
//! context must be of a const fn, a tuple struct's or variant's constructor, or a const
//! intrinsic).
//!
//! The kinds a call in a constant can reach, each from its own fact:
//! - a constructor (tuple struct and tuple variant constructors are const): the store's ctor
//!   row; with no row yet, a tuple STRUCT's ctor by its key's origin (a `MemberKind::Ctor`
//!   member of the struct, variant 0) and an enum's tuple VARIANT by the names library
//!   (`variant_of`): its ADT's row is made later by the layout's own need (consts.rs
//!   `adt_row`), never guessed;
//! - a free fn of any crate (core's `const fn`s included): its head record;
//! - a fn declared in a body (one a macro call wrote included): its local head fact, never from
//!   the names library, which keys no such item;
//! - an impl's fn (`SpecialFields::new()`, `Vec::new()`: protobuf 3.7.2's generated
//!   `default_instance` statics call only inherent `const fn`s of impls, of its own crate and
//!   of alloc): its signature row (`Store::fns`), its own want (`Want::MemberFn`), made on use:
//!   not made yet is [`Fact::Signature`], which [`answer`] makes;
//! - a trait's fn: never reaches here with its row made (`ConstWorld::resolve` answers `None`
//!   for it); without its row, [`Fact::TraitMembers`], made the same way;
//! - a closure: not a fn item (a call of one in a constant is the evaluator's own `Unsupported`
//!   refusal, blaze-consteval machine/code.rs), so never asked here.

use std::alloc::Global;
use std::boxed::Box;
use std::format;
use std::vec::Vec;

use blaze_consteval::{Fact, FnKind};
use blaze_surface::ItemDetail;
use blaze_typeck::ItemKey;

use super::check::Records;
use super::consts::ConstFail;
use super::heads::{Store, Want};
use super::index::Index;
use super::item_shown::{ItemShown, head_error_shown};
use super::keys::{Keys, MemberKind, Origin, checker_of};
use super::source::NamesSource;

/// What a call of `item` does in a constant, from front2's facts; a missing fact is
/// `FnKind::Missing` naming it. A made head carries the qualifiers needed here.
pub fn fn_fact<'n, S: NamesSource<'n>>(names: S, keys: &Keys, index: &Index<'n>, store: &Store<'n>, item: ItemKey) -> FnKind {
    if let Some(c) = store.ctors.get(&item) {
        let variant = keys.library_in(names, item).and_then(|lib| names.variant_of(lib)).map_or(0, |(_, i)| i);
        return FnKind::Ctor { adt: c.adt, variant };
    }
    if let Some(Origin::Member { container, kind, .. }) = keys.origin(item) {
        return member_fact(names, keys, index, store, item, *container, *kind);
    }
    // An item declared in a body (core's `const fn precondition_check`, which
    // `assert_unsafe_precondition!` writes in a block of `Layout::from_size_align_unchecked`;
    // the `const fn gcd` of `<[T]>::align_to_offsets`): `Keys::local` files its key as an
    // `Origin::Item`, so `library_in` gives its library key, but the names library keys no such
    // item. Its head is read from the enclosing body's local facts (locals.rs `Locals::head`).
    if let Some(f) = store.fns.get(&item) {
        return f.const_call(names, index);
    }
    if let Some(local) = store.locals.head(item) {
        return match local.kind {
            super::locals::LocalKind::Fn => FnKind::Missing(Fact::Signature),
            _ => FnKind::Missing(Fact::NotAFn),
        };
    }
    match keys.library_in(names, item) {
        Some(lib) => match names.variant_of(lib) {
            // An enum's tuple variant whose ADT row is not made: a ctor by the names library.
            Some((enum_, variant)) => FnKind::Ctor { adt: checker_of(enum_), variant },
            None => FnKind::Missing(Fact::Signature),
        },
        // A fn declared in a body: its row, made when its enclosing body was lowered.
        None => match store.fns.get(&item) {
            Some(f) => f.const_call(names, index),
            None => FnKind::Missing(Fact::Origin),
        },
    }
}

/// A member's kind: a struct's ctor, or a fn of an impl or a trait by its signature row.
fn member_fact<'n, S: NamesSource<'n>>(
    names: S,
    keys: &Keys,
    index: &Index<'n>,
    store: &Store<'n>,
    item: ItemKey,
    container: ItemKey,
    kind: MemberKind,
) -> FnKind {
    match kind {
        // A tuple struct's ctor (heads.rs files it as the struct's `Ctor` member) whose row is
        // not made: the struct's one variant.
        MemberKind::Ctor => return FnKind::Ctor { adt: container, variant: 0 },
        MemberKind::Fn => {}
        MemberKind::Const | MemberKind::Type | MemberKind::Anon => return FnKind::Missing(Fact::NotAFn),
    }
    if let Some(f) = store.fns.get(&item) {
        return f.const_call(names, index);
    }
    if store.fn_gaps.contains_key(&item) {
        return FnKind::Missing(Fact::SignatureRefused);
    }
    if super::heads::member_fn_want(names, keys, item).is_some() {
        return FnKind::Missing(Fact::Signature);
    }
    match keys.library_in(names, container) {
        Some(trait_) if matches!(names.detail_of(trait_), Some(ItemDetail::Trait)) => FnKind::Missing(Fact::TraitMembers),
        _ => FnKind::Missing(Fact::Container),
    }
}

/// The evaluator refused a call of `callee` as `Refusal::FnFact { fact }` (the fact rides on
/// the refusal: nothing is read again here but what makes it). A fact a record supplies (an
/// impl's fn's signature, a trait's members) is made, and `Ok(())` says to evaluate the need
/// again: the code holding the refusal is pending, so the evaluator compiles it again
/// (blaze-consteval machine/late.rs). `made` holds the callees made in this evaluation: each
/// is made at most once (rule 18), so a fact still missing after its record is made is named
/// ([`ConstFail::FnFact`] with `made`), never made again. Any other fact is the refusal.
pub(super) fn answer<'n, S: NamesSource<'n>>(r: &mut Records<'_, 'n, S>, made: &mut Vec<ItemKey, Global>, callee: ItemKey, fact: Fact) -> Result<(), ConstFail> {
    let want = match fact {
        Fact::Signature => super::heads::member_fn_want(r.names, &*r.keys, callee),
        Fact::TraitMembers => match r.keys.origin(callee) {
            Some(Origin::Member { container, .. }) => r.keys.library_in(r.names, *container).map(Want::Members),
            _ => None,
        },
        _ => None,
    };
    let Some(want) = want else {
        // The store's own words for a signature it refused (its `fn_gaps` row).
        let why = match fact {
            Fact::SignatureRefused => r.store.fn_gaps.get(&callee).map(|w| format!("{w:?}").into_boxed_str()),
            _ => None,
        };
        return Err(ConstFail::FnFact { callee, shown: shown(r, callee), fact, made: None, why });
    };
    if made.contains(&callee) {
        let why = Some(Box::from("it was made once in this evaluation already and the fact is still missing"));
        return Err(ConstFail::FnFact { callee, shown: shown(r, callee), fact, made: Some(want), why });
    }
    made.push(callee);
    match r.store.ensure(r.names, &mut *r.keys, want) {
        Ok(()) => Ok(()),
        Err(e) => {
            let why = Some(head_error_shown(r.names, &e).into_boxed_str());
            Err(ConstFail::FnFact { callee, shown: shown(r, callee), fact, made: Some(want), why })
        }
    }
}

/// `callee` as a refusal names it: its whole path and key (item_shown.rs).
pub(super) fn shown<'n, S: NamesSource<'n>>(r: &Records<'_, 'n, S>, callee: ItemKey) -> Box<str> {
    ItemShown::of(r.names, &*r.keys, callee).boxed()
}
