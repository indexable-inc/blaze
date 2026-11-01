//! `TypeId`s as rustc computes them.
//!
//! A `TypeId` is rustc's stable hash of the type, and rustc's code compares it with ours, so
//! this follows `TyCtxt::type_id_hash` of the pinned toolchain input for input. `None` is a
//! type whose hash takes something this does not reproduce, never a guess.

#![forbid(unsafe_code)]

use std::hash::Hasher;

use base_db::{Db, Crate};
use hir_def::{
    AdtId, CallableDefId, DefWithBodyId, EnumVariantId, FunctionId, HasModule, ImplId,
    ItemContainerId, Lookup, ModuleIdLt, TraitId, TypeAliasId, VariantId,
    attrs::AttrFlags,
    item_tree::FieldsShape,
    signatures::{
        EnumSignature, FunctionSignature, StructSignature, TraitFlags, TraitSignature,
        TypeAliasSignature, UnionSignature,
    },
    src::HasSource,
    visibility::{Visibility, visibility_from_ast_view},
};
use rustc_abi::{ExternAbi, Integer, IntegerType, ReprFlags};
use rustc_ast_ir::Mutability;
use rustc_stable_hash::{FromStableHash, SipHasher128Hash, StableSipHasher128};
use rustc_type_ir::{
    BoundVar, BoundVarIndexKind, ExistentialPredicate, FloatTy, GenericArgKind, IntTy, RegionKind,
    UintTy,
    inherent::{GenericArgs as _, IntoKind, Safety as _, Term as _},
};
use syntax::view::HasVisibility;

use smallvec::SmallVec;

use super::SymbolResolver;
use hir_ty::{
    InferBodyId,
    consteval::try_const_usize,
    db::{HirDatabase, InternedClosure},
    next_solver::{Const, ConstKind, GenericArgs, Region, TermId, Ty, TyKind, default_types},
};

/// rustc's `Fingerprint`, which is also the shape of a `DefPathHash`: the crate, then the item.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Fingerprint(pub(super) u64, pub(super) u64);

impl FromStableHash for Fingerprint {
    type Hash = SipHasher128Hash;

    fn from(SipHasher128Hash([a, b]): Self::Hash) -> Self {
        Fingerprint(a, b)
    }
}

/// rustc's `DefPathData`, as far as types are named through it.
#[derive(Clone, Copy)]
enum DefPathData<'a> {
    Impl,
    TypeNs(&'a str),
    ValueNs(&'a str),
    Closure,
    Ctor,
    AnonConst,
}

impl Fingerprint {
    fn crate_root(stable_crate_id: u64) -> Self {
        Fingerprint(stable_crate_id, stable_crate_id)
    }

    /// `DefKey::compute_stable_hash`, for a child no sibling shares its name with.
    fn child(self, data: DefPathData<'_>) -> Self {
        self.child_at(data, 0)
    }

    /// `DefKey::compute_stable_hash`, for the child that `earlier` siblings of the same name
    /// come before.
    fn child_at(self, data: DefPathData<'_>, earlier: u32) -> Self {
        let mut hasher = StableSipHasher128::new();
        hasher.write_u64(self.1);
        // The variant's index in rustc's `DefPathData`, where `TestBinderConstraints` comes
        // between `Closure` (9) and `Ctor`.
        let (discriminant, name) = match data {
            DefPathData::Impl => (1, None),
            DefPathData::TypeNs(name) => (5, Some(name)),
            DefPathData::ValueNs(name) => (6, Some(name)),
            DefPathData::Closure => (9, None),
            DefPathData::Ctor => (11, None),
            DefPathData::AnonConst => (12, None),
        };
        hasher.write_isize(discriminant);
        if let Some(name) = name {
            hasher.write(name.as_bytes());
            hasher.write_u8(0xff);
        }
        hasher.write_u32(earlier);
        let Fingerprint(local, _) = hasher.finish();
        Fingerprint(self.0, local)
    }

    fn hash(self, hasher: &mut StableSipHasher128) {
        hasher.write_u64(self.0);
        hasher.write_u64(self.1);
    }
}

/// rustc's `VariantDiscr`.
enum Discriminant {
    /// The constant the variant is assigned.
    Explicit(Fingerprint),
    /// How far past the last assigned variant, or the first, this one is.
    Relative(u32),
}
