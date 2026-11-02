//! The impls of each module, for the impl buckets (docs/v2/plan/design/frontend.md items 2 and
//! 3). A module's list is equal (backdated) unless an impl is added to or removed from that
//! module, so a bucket built from the lists of the modules that have impls in it reruns only
//! when one of those modules' impls change, never for an edit elsewhere in the crate.
//!
//! The list is read from the module's final scope, so it holds what collection put there:
//! impls written in the module's files and in its macro expansions, after `cfg`, and the impls
//! the builtin-derive fast path records instead of expanding. Impls in the blocks of bodies are
//! their block modules' own lists: a block's def map is its own query, and the crate's buckets
//! reach it through the bodies the module holds (hir-ty's `impl_holders`).

use base_db::Db;

use crate::{BuiltinDeriveImplId, ImplId, ModuleId};

/// The impls one module declares, in declaration order; equal (backdated) unless an impl is
/// added to or removed from the module.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ModuleImpls {
    pub trait_impls: Box<[ImplId]>,
    pub inherent_impls: Box<[ImplId]>,
    pub builtin_derive_impls: Box<[BuiltinDeriveImplId]>,
}

/// Reads the module's final `ModuleScope` only (through the public `DefMap` API). Trait and
/// inherent impls are told apart as hir-ty's `TraitImpls` and `InherentImpls` tell them, by the
/// scope's own split (`ItemScope::trait_impls`, `ItemScope::inherent_impls`); a
/// `#[rustc_reservation_impl]` is a trait impl here, left to the trait tables to skip as they do.
#[blaze_query::tracked(returns(ref))]
pub fn module_impls(db: &Db, module: ModuleId) -> ModuleImpls {
    let scope = module.def_map(db).module(db, module).scope(db);
    let all = scope.impls().len();
    let traits = scope.trait_impls().count();
    let mut trait_impls = Vec::with_capacity(traits);
    trait_impls.extend(scope.trait_impls());
    let mut inherent_impls = Vec::with_capacity(all - traits);
    inherent_impls.extend(scope.inherent_impls());
    let mut builtin_derive_impls = Vec::with_capacity(scope.builtin_derive_impls().len());
    builtin_derive_impls.extend(scope.builtin_derive_impls());
    ModuleImpls {
        trait_impls: trait_impls.into_boxed_slice(),
        inherent_impls: inherent_impls.into_boxed_slice(),
        builtin_derive_impls: builtin_derive_impls.into_boxed_slice(),
    }
}
