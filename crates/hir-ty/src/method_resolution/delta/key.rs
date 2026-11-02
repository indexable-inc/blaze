//! An impl's key, what the impl tables index it by (design §7 `ImplKey`), and its read set: what
//! computing the key reads that another write can change.
//!
//! The key is computed exactly as `TraitImpls::collect_module` and `InherentImpls::collect_module`
//! compute it: the trait of `impl_trait` and the simplified self type of the trait ref, a
//! reservation impl or one for an error type left out; for an inherent impl the simplified
//! `impl_self_ty`. Whether an impl is a trait impl is the scope's own split (`module_impls`).
//!
//! The read set is by name: every segment of every path in the impl's signature store (the self
//! type, the trait ref, the bounds), and transitively of the type aliases those paths resolve
//! to, with the aliases, traits and ADTs reached as defs. Resolving a path reads the binding of
//! each of its segments in some module (the impl's, an ancestor, the prelude, a module an
//! earlier segment named), so a binding change that can change the key changes a binding whose
//! name is among these: keyed by name alone, the read set holds every `(module, name)` the
//! resolver reads, without tracing it. A signature with a macro type or a lang-item path reads
//! what no name says; it is *wild*, recomputed on every write that changes a binding or a def.

use base_db::Db;
use hir_def::{
    BuiltinDeriveImplId, HasModule, ImplId, ModuleDefId, TraitId, TypeAliasId,
    attrs::AttrFlags,
    expr_store::{ExpressionStore, path::Path},
    hir::type_ref::TypeRef,
    lang_item::LangItems,
    resolver::{HasResolver, Resolver, TypeNs},
    signatures::{ImplSignature, TypeAliasSignature},
};
use hir_expand::name::Name;
use rustc_type_ir::fast_reject::{TreatParams, simplify_type};
use smallvec::SmallVec;

use super::head::{Head, HeadDef, head_of};
use crate::{
    db::HirDatabase, method_resolution::self_ty_has_error_constructor, next_solver::DbInterner,
};

/// What the tables index an impl by.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum ImplKey {
    /// In no table: a trait impl whose trait does not resolve, a reservation impl, a trait impl
    /// for an error type, or an inherent impl whose self type does not simplify.
    Skip,
    /// A trait impl; `head` is `None` for a blanket impl.
    Trait { trait_: TraitId, head: Option<Head> },
    Inherent(Head),
}

/// The key of `imp`, a trait impl when `is_trait_impl` (its module's scope says which).
pub(crate) fn impl_key(db: &Db, imp: ImplId, is_trait_impl: bool) -> ImplKey {
    let interner = DbInterner::new_no_crate(db);
    if !is_trait_impl {
        let self_ty = db.impl_self_ty(imp).instantiate_identity().skip_norm_wip();
        return match simplify_type(interner, self_ty, TreatParams::InstantiateWithInfer) {
            Some(self_ty) => ImplKey::Inherent(head_of(&self_ty, imp)),
            None => ImplKey::Skip,
        };
    }
    let Some(trait_ref) = db.impl_trait(imp) else { return ImplKey::Skip };
    let trait_ref = trait_ref.instantiate_identity().skip_norm_wip();
    // Reservation impls are left out of trait resolution (rust-lang/rust#64631), as the
    // tables built from `module_impls` leave them out.
    if AttrFlags::query(db, imp.into()).contains(AttrFlags::RUSTC_RESERVATION_IMPL) {
        return ImplKey::Skip;
    }
    let self_ty = trait_ref.self_ty();
    if self_ty_has_error_constructor(self_ty) {
        return ImplKey::Skip;
    }
    let head = simplify_type(interner, self_ty, TreatParams::InstantiateWithInfer)
        .map(|self_ty| head_of(&self_ty, imp));
    ImplKey::Trait { trait_: trait_ref.def_id.0, head }
}

/// The key of a built-in derive impl: the derived trait as the crate's lang items name it, and
/// the ADT. `None` when the lang item is missing, as the tables skip it.
pub(crate) fn derive_key(
    db: &Db,
    imp: BuiltinDeriveImplId,
    lang_items: &LangItems,
) -> Option<(TraitId, Head)> {
    let loc = imp.loc(db);
    let trait_ = loc.trait_.get_id(lang_items)?;
    Some((trait_, Head::Adt(HeadDef::Adt(loc.adt))))
}

/// The module a built-in derive impl is listed in: its ADT's.
pub(crate) fn derive_module(db: &Db, imp: BuiltinDeriveImplId) -> hir_def::ModuleId {
    imp.loc(db).adt.module(db)
}

/// What computing an impl's key read, as [`impl_reads`] finds it, with repeats (the read
/// indexes take each edge once). Buffers kept from one impl to the next.
#[derive(Default, Debug)]
pub(crate) struct Reads {
    pub(crate) names: Vec<Name>,
    pub(crate) defs: Vec<ModuleDefId>,
    pub(crate) wild: bool,
    aliases: Vec<TypeAliasId>,
}

impl Reads {
    pub(crate) fn clear(&mut self) {
        self.names.clear();
        self.defs.clear();
        self.wild = false;
        self.aliases.clear();
    }
}

/// The read set of `imp`'s key into `out` (cleared first).
pub(crate) fn impl_reads(db: &Db, imp: ImplId, out: &mut Reads) {
    out.clear();
    let signature = ImplSignature::of(db, imp);
    walk_store(db, &signature.store, &imp.resolver(db), out);
    // An alias reached is walked once, in its own scope; its aliases join the list.
    let mut at = 0;
    while let Some(&alias) = out.aliases.get(at) {
        at += 1;
        let signature = TypeAliasSignature::of(db, alias);
        walk_store(db, &signature.store, &alias.resolver(db), out);
    }
}

fn walk_store(db: &Db, store: &ExpressionStore, resolver: &Resolver<'_>, out: &mut Reads) {
    if !store.macro_calls().is_empty() {
        out.wild = true;
    }
    for (_, type_ref) in store.type_refs() {
        let TypeRef::Path(path) = type_ref else { continue };
        if matches!(path, Path::LangItem(..)) {
            out.wild = true;
            continue;
        }
        if let Some(mod_path) = path.mod_path() {
            out.names.extend_from_slice(mod_path.segments());
        }
        let Some((resolved, _, _)) = resolver.resolve_path_in_type_ns(db, path) else { continue };
        match resolved {
            TypeNs::TypeAliasId(alias) => {
                out.defs.push(alias.into());
                if !out.aliases.contains(&alias) {
                    out.aliases.push(alias);
                }
            }
            TypeNs::TraitId(it) => out.defs.push(it.into()),
            TypeNs::AdtId(it) | TypeNs::AdtSelfType(it) => out.defs.push(it.into()),
            TypeNs::SelfType(_)
            | TypeNs::GenericParam(_)
            | TypeNs::EnumVariantId(_)
            | TypeNs::BuiltinType(_)
            | TypeNs::ModuleId(_) => {}
        }
    }
}

/// The names of `trait_`'s associated items, each once, in a fixed order: its `AssocFn` rows
/// (design §8), the keys of the candidate slices it is in.
pub(crate) fn assoc_names(db: &Db, trait_: TraitId, out: &mut SmallVec<[Name; 8]>) {
    out.clear();
    out.extend(trait_.trait_items(db).items.iter().map(|&(name, _)| name));
    out.sort_unstable();
    out.dedup();
}
