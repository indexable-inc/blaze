//! Pre-type IR item generics
use std::ops;

use base_db::Db;
use hir_expand::name::Name;
use la_arena::{Arena, Idx, RawIdx};
use stdx::impl_from;

use crate::{
    AdtId, ConstParamId, GenericDefId, LifetimeParamId, TypeOrConstParamId, TypeParamId,
    expr_store::{ExpressionStore, ExpressionStoreSourceMap, path::Names},
    signatures::{
        ConstSignature, EnumSignature, FunctionSignature, ImplSignature, StaticSignature,
        StructSignature, TraitSignature, TypeAliasSignature, UnionSignature,
    },
    type_ref::{ConstRef, LifetimeRefId, TypeBound, TypeRefId},
};

pub type LocalTypeOrConstParamId = Idx<TypeOrConstParamData>;
pub type LocalLifetimeParamId = Idx<LifetimeParamData>;
pub type LocalBinderParamId = Idx<BinderParamData>;

/// A type parameter of a where predicate's `for<..>` binder (`for<F: Field<T>>`,
/// `#![feature(blaze_comptime)]`), not a generic parameter of the item: it takes no generic
/// argument and counts in none of the item's lengths. Its bounds are in the generics' store.
#[derive(Clone, PartialEq, Eq, Debug, Hash)]
pub struct BinderParamData {
    pub name: Name,
    pub bounds: Box<[TypeBound]>,
}

/// A binder parameter of `parent`'s generics.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct BinderParamId {
    pub parent: GenericDefId,
    pub local_id: LocalBinderParamId,
}

/// The binder parameters of one where predicate: a run of [`GenericParams::binder_params`].
/// The predicates one `for<..>` binder splits into share it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct BinderParams {
    pub start: u32,
    pub len: u32,
}

impl BinderParams {
    pub const EMPTY: BinderParams = BinderParams { start: 0, len: 0 };

    #[inline]
    pub fn is_empty(self) -> bool {
        self.len == 0
    }
}

/// Data about a generic type parameter (to a function, struct, impl, ...).
#[derive(Clone, PartialEq, Eq, Debug, Hash)]
pub struct TypeParamData {
    /// [`None`] only if the type ref is an [`crate::type_ref::TypeRef::ImplTrait`].
    pub name: Option<Name>,
    pub default: Option<TypeRefId>,
    pub provenance: TypeParamProvenance,
}

/// Data about a generic lifetime parameter (to a function, struct, impl, ...).
#[derive(Clone, PartialEq, Eq, Debug, Hash)]
pub struct LifetimeParamData {
    pub name: Name,
    pub bound_type: LifetimeBoundType,
}

#[derive(Clone, PartialEq, Eq, Debug, Hash)]
pub enum LifetimeBoundType {
    EarlyBound,
    LateBound,
}

impl LifetimeParamData {
    #[inline]
    pub fn is_late_bound(&self) -> bool {
        self.bound_type == LifetimeBoundType::LateBound
    }

    #[inline]
    pub fn is_early_bound(&self) -> bool {
        self.bound_type == LifetimeBoundType::EarlyBound
    }

    /// The name of the `index`th lifetime an impl's header leaves out (`&T`, `'_`), a fresh
    /// lifetime parameter of the impl as rustc's resolver `fresh_lifetime` makes it: `'_#0`,
    /// which no source can write, so only the header's own references name it.
    pub fn fresh_name(index: u32) -> Name {
        Name::new_lifetime(&format!("{FRESH_PREFIX}{index}")) // alloc: one name per fresh lifetime
    }

    /// Whether this is a lifetime an impl's header leaves out ([`Self::fresh_name`]), which
    /// rustc names `'_`.
    pub fn is_fresh(&self) -> bool {
        self.name.as_str().starts_with(FRESH_PREFIX)
    }
}

const FRESH_PREFIX: &str = "'_#";

/// Data about a generic const parameter (to a function, struct, impl, ...).
#[derive(Clone, PartialEq, Eq, Debug, Hash)]
pub struct ConstParamData {
    pub name: Name,
    pub ty: TypeRefId,
    pub default: Option<ConstRef>,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug, Hash)]
pub enum TypeParamProvenance {
    TypeParamList,
    TraitSelf,
    ArgumentImplTrait,
}

#[derive(Clone, PartialEq, Eq, Debug, Hash, blaze_query::Value)]
pub enum TypeOrConstParamData {
    TypeParamData(TypeParamData),
    ConstParamData(ConstParamData),
}

impl TypeOrConstParamData {
    pub fn name(&self) -> Option<&Name> {
        match self {
            TypeOrConstParamData::TypeParamData(it) => it.name.as_ref(),
            TypeOrConstParamData::ConstParamData(it) => Some(&it.name),
        }
    }

    pub fn has_default(&self) -> bool {
        match self {
            TypeOrConstParamData::TypeParamData(it) => it.default.is_some(),
            TypeOrConstParamData::ConstParamData(it) => it.default.is_some(),
        }
    }

    pub fn type_param(&self) -> Option<&TypeParamData> {
        match self {
            TypeOrConstParamData::TypeParamData(it) => Some(it),
            TypeOrConstParamData::ConstParamData(_) => None,
        }
    }

    pub fn const_param(&self) -> Option<&ConstParamData> {
        match self {
            TypeOrConstParamData::TypeParamData(_) => None,
            TypeOrConstParamData::ConstParamData(it) => Some(it),
        }
    }

    pub fn is_trait_self(&self) -> bool {
        match self {
            TypeOrConstParamData::TypeParamData(it) => {
                it.provenance == TypeParamProvenance::TraitSelf
            }
            TypeOrConstParamData::ConstParamData(_) => false,
        }
    }
}

impl_from!(TypeParamData, ConstParamData for TypeOrConstParamData);

#[derive(Clone, PartialEq, Eq, Debug, Hash)]
pub enum GenericParamData {
    TypeParamData(TypeParamData),
    ConstParamData(ConstParamData),
    LifetimeParamData(LifetimeParamData),
}

impl GenericParamData {
    pub fn name(&self) -> Option<&Name> {
        match self {
            GenericParamData::TypeParamData(it) => it.name.as_ref(),
            GenericParamData::ConstParamData(it) => Some(&it.name),
            GenericParamData::LifetimeParamData(it) => Some(&it.name),
        }
    }

    pub fn type_param(&self) -> Option<&TypeParamData> {
        match self {
            GenericParamData::TypeParamData(it) => Some(it),
            _ => None,
        }
    }

    pub fn const_param(&self) -> Option<&ConstParamData> {
        match self {
            GenericParamData::ConstParamData(it) => Some(it),
            _ => None,
        }
    }

    pub fn lifetime_param(&self) -> Option<&LifetimeParamData> {
        match self {
            GenericParamData::LifetimeParamData(it) => Some(it),
            _ => None,
        }
    }
}

impl_from!(TypeParamData, ConstParamData, LifetimeParamData for GenericParamData);

#[derive(Debug, Clone, Copy)]
pub enum GenericParamDataRef<'a> {
    TypeParamData(&'a TypeParamData),
    ConstParamData(&'a ConstParamData),
    LifetimeParamData(&'a LifetimeParamData),
}

/// Data about the generic parameters of a function, struct, impl, etc.
#[derive(PartialEq, Eq, Debug, Hash, Default)]
pub struct GenericParams {
    pub(crate) type_or_consts: Arena<TypeOrConstParamData>,
    pub(crate) lifetimes: Arena<LifetimeParamData>,
    pub(crate) where_predicates: Box<[WherePredicate]>,
    pub(crate) early_bound_lifetimes_len: usize,
    /// The type parameters of the where predicates' `for<..>` binders, each predicate's a run
    /// ([`WherePredicate::TypeBound`]'s `binder`).
    pub(crate) binder_params: Arena<BinderParamData>,
}

impl ops::Index<LocalTypeOrConstParamId> for GenericParams {
    type Output = TypeOrConstParamData;
    fn index(&self, index: LocalTypeOrConstParamId) -> &TypeOrConstParamData {
        &self.type_or_consts[index]
    }
}

impl ops::Index<LocalLifetimeParamId> for GenericParams {
    type Output = LifetimeParamData;
    fn index(&self, index: LocalLifetimeParamId) -> &LifetimeParamData {
        &self.lifetimes[index]
    }
}

/// A single predicate from a where clause, i.e. `where Type: Trait`. Combined
/// where clauses like `where T: Foo + Bar` are turned into multiple of these.
/// It might still result in multiple actual predicates though, because of
/// associated type bindings like `Iterator<Item = u32>`. `lifetimes` is the predicate's
/// `for<..>` binder, a run in the store of the generics ([`ExpressionStore::names`]); `binder` is
/// its type parameters (`#![feature(blaze_comptime)]`), a run of
/// [`GenericParams::binder_params`], empty without the feature.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum WherePredicate {
    TypeBound {
        lifetimes: Option<Names>,
        binder: BinderParams,
        target: TypeRefId,
        bound: TypeBound,
    },
    Lifetime {
        target: LifetimeRefId,
        bound: LifetimeRefId,
    },
}

#[expect(
    clippy::disallowed_types,
    reason = "the process's one empty generic parameter set: callers share a static empty value"
)]
static EMPTY: std::sync::LazyLock<GenericParams> = std::sync::LazyLock::new(|| GenericParams {
    type_or_consts: Arena::default(),
    lifetimes: Arena::default(),
    where_predicates: Box::default(),
    early_bound_lifetimes_len: 0,
    binder_params: Arena::default(),
});

impl GenericParams {
    /// The index of the self param in the generic of the non-parent definition.
    pub const SELF_PARAM_ID_IN_SELF: la_arena::Idx<TypeOrConstParamData> =
        LocalTypeOrConstParamId::from_raw(RawIdx::from_u32(0));

    #[inline]
    #[expect(
        clippy::disallowed_types,
        reason = "the process's one empty generic parameter set: callers share a static empty value"
    )]
    pub fn empty() -> &'static GenericParams {
        std::sync::LazyLock::force(&EMPTY)
    }

    pub fn of(db: &Db, def: GenericDefId) -> &GenericParams {
        Self::with_store(db, def).0
    }

    pub fn with_store(db: &Db, def: GenericDefId) -> (&GenericParams, &ExpressionStore) {
        match def {
            GenericDefId::AdtId(AdtId::EnumId(id)) => {
                let sig = EnumSignature::of(db, id);
                (&sig.generic_params, &sig.store)
            }
            GenericDefId::AdtId(AdtId::StructId(id)) => {
                let sig = StructSignature::of(db, id);
                (&sig.generic_params, &sig.store)
            }
            GenericDefId::AdtId(AdtId::UnionId(id)) => {
                let sig = UnionSignature::of(db, id);
                (&sig.generic_params, &sig.store)
            }
            GenericDefId::ConstId(id) => {
                let sig = ConstSignature::of(db, id);
                (&EMPTY, &sig.store)
            }
            GenericDefId::FunctionId(id) => {
                let sig = FunctionSignature::of(db, id);
                (&sig.generic_params, &sig.store)
            }
            GenericDefId::ImplId(id) => {
                let sig = ImplSignature::of(db, id);
                (&sig.generic_params, &sig.store)
            }
            GenericDefId::StaticId(id) => {
                let sig = StaticSignature::of(db, id);
                (&EMPTY, &sig.store)
            }
            GenericDefId::TraitId(id) => {
                let sig = TraitSignature::of(db, id);
                (&sig.generic_params, &sig.store)
            }
            GenericDefId::TypeAliasId(id) => {
                let sig = TypeAliasSignature::of(db, id);
                (&sig.generic_params, &sig.store)
            }
        }
    }

    pub fn with_source_map(
        db: &Db,
        def: GenericDefId,
    ) -> (&GenericParams, &ExpressionStore, &ExpressionStoreSourceMap) {
        match def {
            GenericDefId::AdtId(AdtId::EnumId(id)) => {
                let (sig, sm) = EnumSignature::with_source_map(db, id);
                (&sig.generic_params, &sig.store, sm)
            }
            GenericDefId::AdtId(AdtId::StructId(id)) => {
                let (sig, sm) = StructSignature::with_source_map(db, id);
                (&sig.generic_params, &sig.store, sm)
            }
            GenericDefId::AdtId(AdtId::UnionId(id)) => {
                let (sig, sm) = UnionSignature::with_source_map(db, id);
                (&sig.generic_params, &sig.store, sm)
            }
            GenericDefId::ConstId(id) => {
                let (sig, sm) = ConstSignature::with_source_map(db, id);
                (&EMPTY, &sig.store, sm)
            }
            GenericDefId::FunctionId(id) => {
                let (sig, sm) = FunctionSignature::with_source_map(db, id);
                (&sig.generic_params, &sig.store, sm)
            }
            GenericDefId::ImplId(id) => {
                let (sig, sm) = ImplSignature::with_source_map(db, id);
                (&sig.generic_params, &sig.store, sm)
            }
            GenericDefId::StaticId(id) => {
                let (sig, sm) = StaticSignature::with_source_map(db, id);
                (&EMPTY, &sig.store, sm)
            }
            GenericDefId::TraitId(id) => {
                let (sig, sm) = TraitSignature::with_source_map(db, id);
                (&sig.generic_params, &sig.store, sm)
            }
            GenericDefId::TypeAliasId(id) => {
                let (sig, sm) = TypeAliasSignature::with_source_map(db, id);
                (&sig.generic_params, &sig.store, sm)
            }
        }
    }

    /// Number of Generic parameters (type_or_consts + lifetimes)
    #[inline]
    pub fn len(&self) -> usize {
        self.type_or_consts.len() + self.lifetimes.len()
    }

    #[inline]
    pub fn len_no_late(&self) -> usize {
        self.type_or_consts.len() + self.early_bound_lifetimes_len
    }

    #[inline]
    pub fn len_lifetimes(&self) -> usize {
        self.lifetimes.len()
    }

    #[inline]
    pub fn len_early_bound_lifetimes(&self) -> usize {
        self.early_bound_lifetimes_len
    }

    #[inline]
    pub fn len_late_bound_lifetimes(&self) -> usize {
        self.len_lifetimes() - self.len_early_bound_lifetimes()
    }

    #[inline]
    pub fn len_type_or_consts(&self) -> usize {
        self.type_or_consts.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[inline]
    pub fn has_no_predicates(&self) -> bool {
        self.where_predicates.is_empty()
    }

    #[inline]
    pub fn where_predicates(&self) -> &[WherePredicate] {
        &self.where_predicates
    }

    /// The binder parameters of `run` (a where predicate's `binder`), in the order written.
    pub fn binder_params(
        &self,
        run: BinderParams,
    ) -> impl ExactSizeIterator<Item = (LocalBinderParamId, &BinderParamData)> {
        (run.start..run.start + run.len).map(|raw| {
            let id = LocalBinderParamId::from_raw(RawIdx::from_u32(raw));
            (id, &self.binder_params[id])
        })
    }

    /// Every binder parameter of every where predicate.
    pub fn iter_binder_params(
        &self,
    ) -> impl Iterator<Item = (LocalBinderParamId, &BinderParamData)> {
        self.binder_params.iter()
    }

    /// Iterator of type_or_consts field
    #[inline]
    pub fn iter_type_or_consts(
        &self,
    ) -> impl DoubleEndedIterator<Item = (LocalTypeOrConstParamId, &TypeOrConstParamData)> {
        self.type_or_consts.iter()
    }

    /// Iterator of lifetimes field
    #[inline]
    pub fn iter_lt(
        &self,
    ) -> impl DoubleEndedIterator<Item = (LocalLifetimeParamId, &LifetimeParamData)> {
        self.lifetimes.iter()
    }

    #[inline]
    pub fn iter_early_bound_lt(
        &self,
    ) -> impl DoubleEndedIterator<Item = (LocalLifetimeParamId, &LifetimeParamData)> {
        self.lifetimes.iter().filter(|(_, p)| p.bound_type == LifetimeBoundType::EarlyBound)
    }

    #[inline]
    pub fn iter_late_bound_lt(
        &self,
    ) -> impl DoubleEndedIterator<Item = (LocalLifetimeParamId, &LifetimeParamData)> {
        self.lifetimes.iter().filter(|(_, p)| p.bound_type == LifetimeBoundType::LateBound)
    }

    pub fn find_type_by_name(&self, name: &Name, parent: GenericDefId) -> Option<TypeParamId> {
        self.type_or_consts.iter().find_map(|(id, p)| {
            if p.name().as_ref() == Some(&name) && p.type_param().is_some() {
                Some(TypeParamId::from_unchecked(TypeOrConstParamId { local_id: id, parent }))
            } else {
                None
            }
        })
    }

    pub fn find_const_by_name(&self, name: &Name, parent: GenericDefId) -> Option<ConstParamId> {
        self.type_or_consts.iter().find_map(|(id, p)| {
            if p.name().as_ref() == Some(&name) && p.const_param().is_some() {
                Some(ConstParamId::from_unchecked(TypeOrConstParamId { local_id: id, parent }))
            } else {
                None
            }
        })
    }

    #[inline]
    pub fn trait_self_param(&self) -> Option<LocalTypeOrConstParamId> {
        if self.type_or_consts.is_empty() {
            return None;
        }
        matches!(
            self.type_or_consts[Self::SELF_PARAM_ID_IN_SELF],
            TypeOrConstParamData::TypeParamData(TypeParamData {
                provenance: TypeParamProvenance::TraitSelf,
                ..
            })
        )
        .then(|| Self::SELF_PARAM_ID_IN_SELF)
    }

    pub fn find_lifetime_by_name(
        &self,
        name: &Name,
        parent: GenericDefId,
    ) -> Option<LifetimeParamId> {
        self.lifetimes.iter().find_map(|(id, p)| {
            if &p.name == name { Some(LifetimeParamId { local_id: id, parent }) } else { None }
        })
    }

    pub fn lifetime_param_idx(
        &self,
        lifetime_param_id: &LocalLifetimeParamId,
    ) -> Option<(usize, bool)> {
        let mut late_bound_idx = 0;
        self.iter_lt().enumerate().find_map(|(idx, (param_id, param_data))| {
            let idx = if param_data.is_late_bound() {
                let prev = late_bound_idx;
                late_bound_idx += 1;
                prev
            } else {
                idx - late_bound_idx
            };

            (param_id == *lifetime_param_id).then(|| (idx, param_data.is_late_bound()))
        })
    }
}
