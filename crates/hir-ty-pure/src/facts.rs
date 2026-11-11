//! The analyses behind `hir_ty::facts::facts_of` (docs/v2/plan/design/facts.md): every fact
//! of an item, from the one walk's summary and from the checks of what is declared.
//!
//! - A function's facts are read from the summary of its definition, in its own
//!   where-clauses ([`inferred_of`]): the kinds of effect it or anything it calls may have,
//!   no trust applied. A fact is *inferred* when no kind that would break it is there. It is
//!   *trusted* when the effect's check passes only because a callee is taken at its word
//!   (`trusted = "why"`).
//! - A type's facts are the ones declared on it that their check accepts
//!   ([`crate::program_lifetime`]).
//!
//! What an effect's check lets through and a fact must not. A check asked of a root (a lint
//! on a marked function, a build script's `main`) closes a `dyn` or function-pointer call
//! over every vtable and pointer the root's graph makes, and so counts the targets where
//! they are made. A fact is asked of one function by any caller, which may hand it an object
//! made somewhere the function's own graph never saw: so [`Rule::Indirect`] breaks every
//! fact here, as does a call only an instance decides ([`Rule::Open`]).

use base_db::Db;
use blaze_items::{ItemId, item_key};
use hir_def::{AdtId, FunctionId, ItemContainerId, signatures::FunctionSignature};
use hir_ty::facts::{Fact, FactItem, FactSet, Facts, Provider};
use hir_ty_mir::mir::dispatch::Body;

use crate::{
    effect_attrs::{NOALLOC, NONBLOCKING, NOPANIC, TERMINATES},
    effects::{EffectDef, EffectSet, Rule},
    function::{FnPurity, check_generic, fn_purity_generic, generic},
    program_lifetime,
    walk::{Def, Instance, summary},
};

#[cfg(test)]
mod model;
#[cfg(test)]
mod tests;

inventory::submit! {
    Provider { facts_of, own_tests: cfg!(test) }
}

/// Named by a crate that needs the provider linked (`hir-ty-codegen`): it does nothing, and
/// is in the object the registration above is in.
pub fn linked_here() {}

fn facts_of(db: &Db, item: FactItem) -> &Facts {
    match item {
        FactItem::Fn(func) => fn_facts(db, func),
        FactItem::Adt(adt) => adt_facts(db, adt),
    }
}

/// What no fact of one function alone survives: a call the walk can't follow, one only an
/// instance decides, and one whose target is whatever object or pointer the caller hands it.
const UNSEEN: EffectSet = EffectSet::of(&[Rule::Unchecked, Rule::Open, Rule::Indirect]);

/// A bug planted for the controls of the refinement's test ([`model`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Plant {
    /// No bug.
    Off,
    /// [`Rule::Indirect`] does not break [`Fact::NoUnwind`]: a function that calls through
    /// a `dyn` is proved unable to panic though the object's method may.
    IndirectIgnored,
}

/// The kinds of effect that break `fact` of a function, or `None` for a fact no set of kinds
/// decides (purity also scans the parameters; a type's facts are not a function's).
pub const fn breaks(fact: Fact) -> Option<EffectSet> {
    Some(match fact {
        // Code blaze never saw (`Foreign`, `ForeignUnknown`) is not proved of: it may unwind
        // through an ABI that says it can't only by being wrong, and blaze never rests code
        // on that.
        Fact::NoUnwind => NOPANIC
            .refuses
            .union(UNSEEN)
            .union(EffectSet::of(&[Rule::Foreign, Rule::ForeignUnknown])),
        Fact::NoAlloc => NOALLOC.refuses.union(UNSEEN),
        Fact::NoBlock => NONBLOCKING.refuses.union(UNSEEN),
        Fact::Returns => TERMINATES.refuses.union(UNSEEN),
        Fact::Pure | Fact::ProgramLifetime => return None,
    })
}

/// The facts a function whose summary has `kinds` is proved to have, from the kinds alone.
pub const fn inferred_of(kinds: EffectSet) -> FactSet {
    inferred_with(kinds, Plant::Off)
}

#[doc(hidden)]
pub const fn inferred_with(kinds: EffectSet, plant: Plant) -> FactSet {
    let mut facts = FactSet::EMPTY;
    let mut i = 0;
    while i < Fact::ALL.len() {
        let fact = Fact::ALL[i];
        i += 1;
        let Some(mut breaks) = breaks(fact) else { continue };
        if matches!(plant, Plant::IndirectIgnored) && matches!(fact, Fact::NoUnwind) {
            breaks = breaks.without(EffectSet::one(Rule::Indirect));
        }
        if kinds.intersect(breaks).is_empty() {
            facts = facts.with(fact);
        }
    }
    facts
}

/// The effect whose check passing through trust makes `fact` trusted.
const CHECKED: [(Fact, &EffectDef); 4] = [
    (Fact::NoUnwind, &NOPANIC),
    (Fact::NoAlloc, &NOALLOC),
    (Fact::NoBlock, &NONBLOCKING),
    (Fact::Returns, &TERMINATES),
];

/// Every fact of the function `func` as defined. A function with no body of its own to walk
/// (a foreign function, a trait's method without a default) has none.
#[blaze_query::tracked(returns(ref))]
fn fn_facts(db: &Db, func: FunctionId) -> Facts {
    let mut facts = Facts::none(item_key(db, ItemId::Function(func)).0.0);
    let foreign = matches!(func.loc(db).container, ItemContainerId::ExternBlockId(_));
    if foreign || !FunctionSignature::of(db, func).has_body() {
        return facts;
    }
    let (env, args) = generic(db, func);
    let kinds = summary(db, env, Instance { def: Def::Body(Body::Fn(func)), args }).kinds;
    facts.inferred = inferred_of(kinds);
    for (fact, effect) in CHECKED {
        if facts.inferred.has(fact) || !kinds.intersect(UNSEEN).is_empty() {
            continue;
        }
        if let Ok(proof) = check_generic(db, func, effect)
            && !proof.open
            && !proof.trusted.is_empty()
        {
            facts.trusted = facts.trusted.with(fact);
        }
    }
    if kinds.intersect(UNSEEN).is_empty()
        && let FnPurity::Pure(proof) = fn_purity_generic(db, func)
        && !proof.open
    {
        if proof.trusted.is_empty() {
            facts.inferred = facts.inferred.with(Fact::Pure);
        } else {
            facts.trusted = facts.trusted.with(Fact::Pure);
        }
    }
    facts
}

/// Every fact of the type `adt` as defined: [`Fact::ProgramLifetime`] where it is declared
/// and the type's shape is one it can hold of. Where instances are allocated and whether the
/// type is another crate's to name are the lint's (`blaze::program_lifetime`), which sees
/// the crate's bodies.
#[blaze_query::tracked(returns(ref))]
fn adt_facts(db: &Db, adt: AdtId) -> Facts {
    let mut facts = Facts::none(item_key(db, ItemId::Adt(adt)).0.0);
    if program_lifetime::declared(db, adt) && program_lifetime::shape(db, adt).is_none()
    {
        facts.declared = FactSet::one(Fact::ProgramLifetime);
    }
    facts
}
