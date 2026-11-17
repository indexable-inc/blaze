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
//! - a free fn of any crate (core's `const fn`s included): its head, read from its text;
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

use blaze_consteval::{Fact, FnKind, Intrinsic};
use blaze_id::ItemKey as LibKey;
use blaze_lower::head;
use blaze_own::Alloc;
use blaze_surface::ItemDetail;
use blaze_typeck::ItemKey;

use super::check::Records;
use super::consts::{ConstFail, is_panic_lang, written_name};
use super::heads::{CrateCfg, Store, Want};
use super::index::Index;
use super::item_shown::{ItemShown, head_error_shown};
use super::keys::{Keys, MemberKind, Origin, checker_of};
use super::source::NamesSource;

/// What a call of `item` does in a constant, from front2's facts; a missing fact is
/// `FnKind::Missing` naming it. `alloc` holds the callee's lowered head while it is read.
pub fn fn_fact<'n, S: NamesSource<'n>>(names: S, keys: &Keys, index: &Index<'n>, store: &Store<'n>, item: ItemKey, alloc: Alloc<'_>) -> FnKind {
    if let Some(c) = store.ctors.get(&item) {
        let variant = keys.library_in(names, item).and_then(|lib| names.variant_of(lib)).map_or(0, |(_, i)| i);
        return FnKind::Ctor { adt: c.adt, variant };
    }
    if let Some(Origin::Member { container, kind, .. }) = keys.origin(item) {
        return member_fact(names, keys, index, store, item, *container, *kind, alloc);
    }
    match keys.library_in(names, item) {
        Some(lib) => match names.variant_of(lib) {
            // An enum's tuple variant whose ADT row is not made: a ctor by the names library.
            Some((enum_, variant)) => FnKind::Ctor { adt: checker_of(enum_), variant },
            None => match names.text_of(lib) {
                Some(text) => head_fact(names, index, lib, text.text, alloc),
                None => FnKind::Missing(Fact::Text),
            },
        },
        // A fn declared in a body: its row, made when its enclosing body was lowered.
        None => match store.fns.get(&item) {
            Some(f) => head_fact(names, index, f.lib, f.text, alloc),
            None => FnKind::Missing(Fact::Origin),
        },
    }
}

/// A member's kind: a struct's ctor, or a fn of an impl or a trait by its signature row.
#[expect(clippy::too_many_arguments, reason = "the records fn_fact reads, passed on whole")]
fn member_fact<'n, S: NamesSource<'n>>(
    names: S,
    keys: &Keys,
    index: &Index<'n>,
    store: &Store<'n>,
    item: ItemKey,
    container: ItemKey,
    kind: MemberKind,
    alloc: Alloc<'_>,
) -> FnKind {
    match kind {
        // A tuple struct's ctor (heads.rs files it as the struct's `Ctor` member) whose row is
        // not made: the struct's one variant.
        MemberKind::Ctor => return FnKind::Ctor { adt: container, variant: 0 },
        MemberKind::Fn => {}
        MemberKind::Const | MemberKind::Type | MemberKind::Anon => return FnKind::Missing(Fact::NotAFn),
    }
    if let Some(f) = store.fns.get(&item) {
        return head_fact(names, index, f.lib, f.text, alloc);
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

/// A fn's kind from its head as written (`text`, of library item `lib`): a panic entry of
/// core, an intrinsic, `const`, or not const.
fn head_fact<'n, S: NamesSource<'n>>(names: S, index: &Index<'n>, lib: LibKey, text: &str, alloc: Alloc<'_>) -> FnKind {
    if index.lang_name_of(lib).is_some_and(is_panic_lang) {
        return FnKind::Panic;
    }
    let Some(krate) = names.crate_of(lib) else { return FnKind::Missing(Fact::Crate) };
    let (Some(atoms), Some(edition)) = (names.cfg_atoms(krate), names.edition_of(krate)) else { return FnKind::Missing(Fact::Crate) };
    let Ok(sig) = blaze_lower::lower_signature(text, edition, &mut CrateCfg(atoms), alloc) else { return FnKind::Missing(Fact::Head) };
    let v = sig.view();
    let intrinsic = v.rows(v.list(head::ATTRS)).ok().is_some_and(|rows| {
        rows.iter().any(|&n| v.attr(n).ok().and_then(|a| v.text_at(a.path).ok()).is_some_and(|p| p == "rustc_intrinsic"))
    });
    if intrinsic {
        let Some(name) = written_name(names, lib) else { return FnKind::Missing(Fact::IntrinsicName) };
        // An unknown name is its own kind: the evaluator refuses a call of it as
        // `Refusal::UnknownIntrinsic`, which compiled code keeps like any final refusal.
        return match Intrinsic::named(name) {
            Ok(w) => FnKind::Intrinsic(w),
            Err(_) => FnKind::UnknownIntrinsic,
        };
    }
    // The head was read: its `const` (or its absence) is the answer.
    if v.head[head::FLAGS] & blaze_lower::machine::sig::CONST != 0 { FnKind::Const } else { FnKind::NotConst }
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
