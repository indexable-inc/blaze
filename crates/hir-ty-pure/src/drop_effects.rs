//! Whether a `Drop` impl only frees memory (docs/v2/plan/design/early-drop.md, section 1): what
//! `#[blaze::drop(early)]` on a type promises, checked here from the effect walk's summary of
//! the impl's `drop` in its own where-clauses.
//!
//! `hir_ty_mir::mir::early_drop` does not ask this while it builds a body: the walk reads
//! built MIR, so asking it there for a `Drop` impl of a crate under `#![blaze::early_drop]`
//! is a query cycle. The pass reads the attribute; [`check_drop_attr`] is what holds the
//! attribute to the impl, for `blaze check` to report per type.

use base_db::{CrateOrigin, Db};
use hir_def::{AdtId, AssocItemId, AttrDefId, FunctionId, HasModule, attrs::AttrFlags};
use hir_expand::name::Name;
use hir_ty::{
    ParamEnvAndCrate,
    db::HirDatabase,
    drop::{DropGlue, destructor, has_drop_glue},
    next_solver::{DbInterner, GenericArgs, TypingMode, infer::DbInternerInferExt},
};
use hir_ty_mir::mir::dispatch::Body;
use intern::sym;
use rustc_type_ir::inherent::GenericArgs as _;

use crate::{
    effects::{EffectSet, Rule},
    walk::{Def, Instance, fn_item_path, summary},
};

/// The kinds a destructor that only frees memory may have: the global allocator, and a call
/// only an instance decides (the drop of a type argument, which each use's type answers).
pub const FREES_ONLY: EffectSet = EffectSet::of(&[Rule::Alloc, Rule::Open]);

/// What the toolchain's destructors may have beside: a panic entry point, which there is an
/// `unsafe` precondition check that aborts on what is already undefined.
pub const FREES_ONLY_TOOLCHAIN: EffectSet =
    EffectSet::of(&[Rule::Alloc, Rule::Open, Rule::Panic, Rule::Overflow]);

/// The `drop` of `adt`'s `Drop` impl, if it has one.
pub fn drop_fn(db: &Db, adt: AdtId) -> Option<FunctionId> {
    let imp = destructor(db, adt)?;
    let drop = Name::new_symbol_root(sym::drop);
    imp.impl_items(db).items.iter().find_map(|(name, item)| match item {
        AssocItemId::FunctionId(it) if *name == drop => Some(*it),
        _ => None,
    })
}

/// Every kind of effect `adt`'s `Drop::drop` may have, in the impl's own where-clauses, its
/// callees' included; `None` for a type with no `Drop` impl.
pub fn drop_effects(db: &Db, adt: AdtId) -> Option<EffectSet> {
    let func = drop_fn(db, adt)?;
    let krate = func.krate(db);
    let param_env = db.trait_environment(func.into());
    let interner = DbInterner::new_with(db, krate);
    let args = GenericArgs::identity_for_item(interner, func.into()).store();
    let env = ParamEnvAndCrate { param_env, krate }.store();
    Some(summary(db, env, Instance { def: Def::Body(Body::Fn(func)), args }).kinds)
}

/// The first kind of `kinds` a destructor that only frees memory may not have, in [`Rule`]
/// order; `toolchain`: the impl is the toolchain's.
pub fn not_frees_only(kinds: EffectSet, toolchain: bool) -> Option<Rule> {
    let allowed = if toolchain { FREES_ONLY_TOOLCHAIN } else { FREES_ONLY };
    kinds.without(allowed).iter().next()
}

/// Whether `adt`'s `Drop` impl only frees memory: `Err` names the first kind that says not.
/// A type with no `Drop` impl has nothing to refuse.
pub fn frees_only(db: &Db, adt: AdtId) -> Result<(), Rule> {
    let Some(kinds) = drop_effects(db, adt) else { return Ok(()) };
    let toolchain = matches!(adt.krate(db).data(db).origin, CrateOrigin::Lang(_));
    match not_frees_only(kinds, toolchain) {
        Some(rule) => Err(rule),
        None => Ok(()),
    }
}

/// What is wrong with `#[blaze::drop(..)]` on `adt`, naming the type and the rule: both words
/// at once; `early` on a type with no `Drop` impl; `early` without `trusted` on an impl that
/// does more than free memory; `scoped` on a type that drops nothing. `Ok` without the
/// attribute.
pub fn check_drop_attr(db: &Db, adt: AdtId) -> Result<(), String> {
    let flags = AttrFlags::query(db, AttrDefId::AdtId(adt));
    let early = flags.contains(AttrFlags::DROP_EARLY);
    let scoped = flags.contains(AttrFlags::DROP_SCOPED);
    if !early && !scoped {
        return Ok(());
    }
    let name = hir_ty_mir::mir::early_drop::class::adt_name(db, adt);
    // alloc: an error's text, on the error path
    if early && scoped {
        return Err(format!(
            "`{name}` is both `#[blaze::drop(early)]` and `#[blaze::drop(scoped)]`: a value is \
             dropped after its last use or where its scope ends, not both"
        ));
    }
    if scoped {
        let krate = adt.krate(db);
        let interner = DbInterner::new_with(db, krate);
        let infcx = interner.infer_ctxt().build(TypingMode::non_body_analysis());
        let env = db.trait_environment(adt.into());
        let ty = db.ty(hir_ty::TyDefId::AdtId(adt)).instantiate_identity().skip_norm_wip();
        return match has_drop_glue(&infcx, ty, env) {
            DropGlue::None => Err(format!(
                "`{name}` is `#[blaze::drop(scoped)]` but dropping it runs nothing: there is no \
                 drop to keep at the end of a scope"
            )),
            _ => Ok(()),
        };
    }
    let Some(func) = drop_fn(db, adt) else {
        return Err(format!(
            "`{name}` is `#[blaze::drop(early)]` but has no `Drop` impl: a type without one is \
             dropped early when its fields are, with no attribute"
        ));
    };
    if flags.contains(AttrFlags::DROP_TRUSTED) {
        return Ok(());
    }
    match frees_only(db, adt) {
        Ok(()) => Ok(()),
        Err(rule) => Err(format!(
            "`{name}` is `#[blaze::drop(early)]`, but `{}` {}: a destructor dropped early only \
             frees memory; keep the scope's drop, or assert it with `#[blaze::drop(early, \
             trusted = \"why\")]`",
            fn_item_path(db, func),
            rule.said()
        )),
    }
}
