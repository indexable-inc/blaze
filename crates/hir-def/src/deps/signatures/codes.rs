//! What each record of the part holds besides its store, in the words of
//! [`crate::hir::persist`]: one [`Sig`] per signature query, and [`Code`] for the parts of
//! signatures nothing else writes (generic params, where clauses, fields, a trait ref).

use blaze_format::FormatError;
use cfg::CfgOptions;
use hir_expand::name::Name;
use la_arena::Arena;
use smallvec::SmallVec;
use thin_vec::ThinVec;
use triomphe::Arc;

use crate::{
    FxIndexMap,
    expr_store::ExpressionStore,
    hir::{
        columns::{Checked, below, raw},
        generics::{
            ConstParamData, GenericParams, LifetimeBoundType, LifetimeParamData,
            TypeOrConstParamData, TypeParamData, TypeParamProvenance, WherePredicate,
        },
        persist::{Code, Read, Words},
    },
    signatures::{
        ConstSignature, EnumSignature, EnumVariants, FieldData, FunctionSignature, ImplSignature,
        InactiveEnumVariantCode, StaticSignature, StructSignature, TraitSignature,
        TypeAliasSignature, UnionSignature, VariantFields,
    },
    type_ref::{ConstRef, LifetimeRefId, PathId, TraitRef, TypeBound, TypeRef, TypeRefId},
};

/// One signature query, as a record holds its value.
pub(super) trait Sig {
    /// The value the record is written from.
    type Of;
    /// What the query returns.
    type Value;

    fn of(value: &Self::Value) -> &Self::Of;

    /// The store, written as a buffer of its own beside the words.
    fn store(of: &Self::Of) -> Option<&ExpressionStore>;

    fn put(of: &Self::Of, w: &mut Words);

    /// The value of `r`, with its `store`; `cfg` is the options of the item's crate.
    fn get(
        r: &mut Read<'_>,
        store: Option<ExpressionStore>,
        cfg: &CfgOptions,
    ) -> Result<Self::Value, FormatError>;
}

fn needs(store: Option<ExpressionStore>, r: &Read<'_>) -> Result<ExpressionStore, FormatError> {
    store.ok_or_else(|| r.invalid("a signature record without its store"))
}

fn put_flags<F: bitflags::Flags>(w: &mut Words, flags: &F)
where
    F::Bits: Into<u32>,
{
    w.u32(flags.bits().into());
}

fn get_flags<F: bitflags::Flags>(r: &mut Read<'_>) -> Result<F, FormatError>
where
    F::Bits: TryFrom<u32>,
{
    let word = r.u32()?;
    let bits = <F::Bits as TryFrom<u32>>::try_from(word).ok();
    bits.and_then(F::from_bits).ok_or_else(|| r.invalid("signature flags out of range"))
}

fn put_arena<T: Code>(w: &mut Words, arena: &Arena<T>) {
    w.len(arena.len());
    arena.iter().for_each(|(_, it)| it.put(w));
}

fn get_arena<T: Code>(r: &mut Read<'_>) -> Result<Arena<T>, FormatError> {
    let len = r.len()?;
    let mut arena = Arena::with_capacity(len);
    for _ in 0..len {
        arena.alloc(r.get()?);
    }
    Ok(arena)
}

/// What `params` (defaults, const params' types, where predicates) and `bounds` name in
/// `store`. A record and its store are read from buffers of their own, and the store's read
/// checks only its own columns, so a record's ids are checked here before the signature is used:
/// ids below their arenas, `for<..>` and `use<..>` runs inside the store's paths.
fn in_store(store: &ExpressionStore, params: &GenericParams, bounds: &[TypeBound]) -> Checked {
    let (types, lifetimes) = (&store.types, store.lifetimes.len());
    let ids = Ids(store);
    for (_, param) in params.type_or_consts.iter() {
        match param {
            TypeOrConstParamData::TypeParamData(it) => ids.types(it.default)?,
            TypeOrConstParamData::ConstParamData(it) => {
                ids.ty(it.ty)?;
                it.default.map_or(Ok(()), |it| ids.konst(it))?;
            }
        }
    }
    let lifetime = |id: LifetimeRefId| {
        below(raw(id), lifetimes, "a where predicate's lifetime past its arena")
    };
    for predicate in params.where_predicates.iter() {
        match *predicate {
            WherePredicate::TypeBound { lifetimes: binder, target, bound } => {
                below(raw(target), types.len(), "a where predicate's type past its arena")?;
                binder.map_or(Ok(()), |names| types.paths.check_names(names))?;
                types.check_outside_bound(&bound, lifetimes)?;
            }
            WherePredicate::Lifetime { target, bound } => {
                lifetime(target)?;
                lifetime(bound)?;
            }
        }
    }
    bounds.iter().try_for_each(|bound| types.check_outside_bound(bound, lifetimes))
}

fn check_in_store(
    store: &ExpressionStore,
    params: &GenericParams,
    bounds: &[TypeBound],
    r: &Read<'_>,
) -> Result<(), FormatError> {
    check(in_store(store, params, bounds), r)
}

fn check(checked: Checked, r: &Read<'_>) -> Result<(), FormatError> {
    checked.map_err(|what| r.invalid(what))
}

/// A record's own ids in its store, beside its generics: each below its arena, and an impl's
/// trait a path.
struct Ids<'a>(&'a ExpressionStore);

impl Ids<'_> {
    fn ty(&self, id: TypeRefId) -> Checked {
        below(raw(id), self.0.types.len(), "a record's type past its store")
    }

    fn types(&self, ids: impl IntoIterator<Item = TypeRefId>) -> Checked {
        ids.into_iter().try_for_each(|id| self.ty(id))
    }

    /// The store's own read checked what each type holds, so the type of an id below the
    /// arena decodes.
    fn path(&self, id: PathId) -> Checked {
        self.ty(id.type_ref())?;
        match self.0.types.get(id.type_ref()) {
            TypeRef::Path(_) => Ok(()),
            _ => Err("a record's trait that is not a path"),
        }
    }

    fn konst(&self, it: ConstRef) -> Checked {
        below(raw(it.expr), self.0.expr_count(), "a record's constant past its store")
    }
}

impl Code for TypeParamProvenance {
    fn put(&self, w: &mut Words) {
        w.u32(match self {
            TypeParamProvenance::TypeParamList => 0,
            TypeParamProvenance::TraitSelf => 1,
            TypeParamProvenance::ArgumentImplTrait => 2,
        });
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(match r.tag(3, "a type param provenance out of range")? {
            0 => TypeParamProvenance::TypeParamList,
            1 => TypeParamProvenance::TraitSelf,
            _ => TypeParamProvenance::ArgumentImplTrait,
        })
    }
}

impl Code for LifetimeBoundType {
    fn put(&self, w: &mut Words) {
        w.u32(match self {
            LifetimeBoundType::EarlyBound => 0,
            LifetimeBoundType::LateBound => 1,
        });
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(match r.tag(2, "a lifetime bound type out of range")? {
            0 => LifetimeBoundType::EarlyBound,
            _ => LifetimeBoundType::LateBound,
        })
    }
}

impl Code for TypeOrConstParamData {
    fn put(&self, w: &mut Words) {
        match self {
            TypeOrConstParamData::TypeParamData(TypeParamData { name, default, provenance }) => {
                w.u32(0);
                w.put(name);
                w.put(default);
                w.put(provenance);
            }
            TypeOrConstParamData::ConstParamData(ConstParamData { name, ty, default }) => {
                w.u32(1);
                w.put(name);
                w.put(ty);
                w.put(default);
            }
        }
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(match r.tag(2, "a generic param kind out of range")? {
            0 => TypeOrConstParamData::TypeParamData(TypeParamData {
                name: r.get()?,
                default: r.get()?,
                provenance: r.get()?,
            }),
            _ => TypeOrConstParamData::ConstParamData(ConstParamData {
                name: r.get()?,
                ty: r.get()?,
                default: r.get()?,
            }),
        })
    }
}

impl Code for LifetimeParamData {
    fn put(&self, w: &mut Words) {
        w.put(&self.name);
        w.put(&self.bound_type);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(LifetimeParamData { name: r.get()?, bound_type: r.get()? })
    }
}

impl Code for WherePredicate {
    fn put(&self, w: &mut Words) {
        match self {
            WherePredicate::TypeBound { lifetimes, target, bound } => {
                w.u32(0);
                w.put(lifetimes);
                w.put(target);
                w.put(bound);
            }
            WherePredicate::Lifetime { target, bound } => {
                w.u32(1);
                w.put(target);
                w.put(bound);
            }
        }
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(match r.tag(2, "a where predicate kind out of range")? {
            0 => {
                WherePredicate::TypeBound { lifetimes: r.get()?, target: r.get()?, bound: r.get()? }
            }
            _ => WherePredicate::Lifetime { target: r.get()?, bound: r.get()? },
        })
    }
}

impl Code for GenericParams {
    fn put(&self, w: &mut Words) {
        put_arena(w, &self.type_or_consts);
        put_arena(w, &self.lifetimes);
        w.put(&self.where_predicates);
        w.put(&self.early_bound_lifetimes_len);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(GenericParams {
            type_or_consts: get_arena(r)?,
            lifetimes: get_arena(r)?,
            where_predicates: r.get()?,
            early_bound_lifetimes_len: r.get()?,
        })
    }
}

impl Code for TraitRef {
    fn put(&self, w: &mut Words) {
        w.put(&self.path);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(TraitRef { path: r.get()? })
    }
}

impl Code for FieldData {
    fn put(&self, w: &mut Words) {
        let FieldData { name, type_ref, visibility, is_unsafe, default_value } = self;
        w.put(name);
        w.put(type_ref);
        w.put(visibility);
        w.bool(*is_unsafe);
        w.put(default_value);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(FieldData {
            name: r.get()?,
            type_ref: r.get()?,
            visibility: r.get()?,
            is_unsafe: r.bool()?,
            default_value: r.get()?,
        })
    }
}

/// [`StructSignature::of`].
pub(super) struct Struct;
/// [`UnionSignature::of`].
pub(super) struct Union;
/// [`EnumSignature::of`].
pub(super) struct Enum;
/// [`ConstSignature::of`].
pub(super) struct Const;
/// [`StaticSignature::of`].
pub(super) struct Static;
/// [`ImplSignature::of`].
pub(super) struct Impl;
/// [`TraitSignature::of`].
pub(super) struct Trait;
/// [`FunctionSignature::of`].
pub(super) struct Function;
/// [`TypeAliasSignature::of`].
pub(super) struct TypeAlias;
/// [`VariantFields::of`].
pub(super) struct Fields;
/// [`EnumVariants::of`].
pub(super) struct Variants;

impl Sig for Struct {
    type Of = StructSignature;
    type Value = Arc<StructSignature>;

    fn of(value: &Self::Value) -> &Self::Of {
        value
    }

    fn store(of: &Self::Of) -> Option<&ExpressionStore> {
        Some(&of.store)
    }

    fn put(of: &Self::Of, w: &mut Words) {
        w.put(&of.name);
        w.put(&of.generic_params);
        put_flags(w, &of.flags);
        w.put(&of.shape);
    }

    fn get(
        r: &mut Read<'_>,
        store: Option<ExpressionStore>,
        _: &CfgOptions,
    ) -> Result<Self::Value, FormatError> {
        let signature = StructSignature {
            name: r.get()?,
            generic_params: r.get()?,
            flags: get_flags(r)?,
            shape: r.get()?,
            store: needs(store, r)?,
        };
        check_in_store(&signature.store, &signature.generic_params, &[], r)?;
        // alloc: the `Arc` the signature query returns, once per item asked.
        Ok(Arc::new(signature))
    }
}

impl Sig for Union {
    type Of = UnionSignature;
    type Value = Arc<UnionSignature>;

    fn of(value: &Self::Value) -> &Self::Of {
        value
    }

    fn store(of: &Self::Of) -> Option<&ExpressionStore> {
        Some(&of.store)
    }

    fn put(of: &Self::Of, w: &mut Words) {
        w.put(&of.name);
        w.put(&of.generic_params);
        put_flags(w, &of.flags);
    }

    fn get(
        r: &mut Read<'_>,
        store: Option<ExpressionStore>,
        _: &CfgOptions,
    ) -> Result<Self::Value, FormatError> {
        let signature = UnionSignature {
            name: r.get()?,
            generic_params: r.get()?,
            flags: get_flags(r)?,
            store: needs(store, r)?,
        };
        check_in_store(&signature.store, &signature.generic_params, &[], r)?;
        // alloc: the `Arc` the signature query returns, once per item asked.
        Ok(Arc::new(signature))
    }
}

impl Sig for Enum {
    type Of = EnumSignature;
    type Value = Arc<EnumSignature>;

    fn of(value: &Self::Value) -> &Self::Of {
        value
    }

    fn store(of: &Self::Of) -> Option<&ExpressionStore> {
        Some(&of.store)
    }

    fn put(of: &Self::Of, w: &mut Words) {
        w.put(&of.name);
        w.put(&of.generic_params);
        put_flags(w, &of.flags);
    }

    fn get(
        r: &mut Read<'_>,
        store: Option<ExpressionStore>,
        _: &CfgOptions,
    ) -> Result<Self::Value, FormatError> {
        let signature = EnumSignature {
            name: r.get()?,
            generic_params: r.get()?,
            flags: get_flags(r)?,
            store: needs(store, r)?,
        };
        check_in_store(&signature.store, &signature.generic_params, &[], r)?;
        // alloc: the `Arc` the signature query returns, once per item asked.
        Ok(Arc::new(signature))
    }
}

impl Sig for Const {
    type Of = ConstSignature;
    type Value = Arc<ConstSignature>;

    fn of(value: &Self::Value) -> &Self::Of {
        value
    }

    fn store(of: &Self::Of) -> Option<&ExpressionStore> {
        Some(&of.store)
    }

    fn put(of: &Self::Of, w: &mut Words) {
        w.put(&of.name);
        w.put(&of.type_ref);
        put_flags(w, &of.flags);
    }

    fn get(
        r: &mut Read<'_>,
        store: Option<ExpressionStore>,
        _: &CfgOptions,
    ) -> Result<Self::Value, FormatError> {
        let signature = ConstSignature {
            name: r.get()?,
            type_ref: r.get()?,
            flags: get_flags(r)?,
            store: needs(store, r)?,
        };
        check(Ids(&signature.store).ty(signature.type_ref), r)?;
        // alloc: the `Arc` the signature query returns, once per item asked.
        Ok(Arc::new(signature))
    }
}

impl Sig for Static {
    type Of = StaticSignature;
    type Value = Arc<StaticSignature>;

    fn of(value: &Self::Value) -> &Self::Of {
        value
    }

    fn store(of: &Self::Of) -> Option<&ExpressionStore> {
        Some(&of.store)
    }

    fn put(of: &Self::Of, w: &mut Words) {
        w.put(&of.name);
        w.put(&of.type_ref);
        put_flags(w, &of.flags);
    }

    fn get(
        r: &mut Read<'_>,
        store: Option<ExpressionStore>,
        _: &CfgOptions,
    ) -> Result<Self::Value, FormatError> {
        let signature = StaticSignature {
            name: r.get()?,
            type_ref: r.get()?,
            flags: get_flags(r)?,
            store: needs(store, r)?,
        };
        check(Ids(&signature.store).ty(signature.type_ref), r)?;
        // alloc: the `Arc` the signature query returns, once per item asked.
        Ok(Arc::new(signature))
    }
}

impl Sig for Impl {
    type Of = ImplSignature;
    type Value = Arc<ImplSignature>;

    fn of(value: &Self::Value) -> &Self::Of {
        value
    }

    fn store(of: &Self::Of) -> Option<&ExpressionStore> {
        Some(&of.store)
    }

    fn put(of: &Self::Of, w: &mut Words) {
        w.put(&of.generic_params);
        w.put(&of.self_ty);
        w.put(&of.target_trait);
        put_flags(w, &of.flags);
    }

    fn get(
        r: &mut Read<'_>,
        store: Option<ExpressionStore>,
        _: &CfgOptions,
    ) -> Result<Self::Value, FormatError> {
        let signature = ImplSignature {
            generic_params: r.get()?,
            self_ty: r.get()?,
            target_trait: r.get()?,
            flags: get_flags(r)?,
            store: needs(store, r)?,
        };
        check_in_store(&signature.store, &signature.generic_params, &[], r)?;
        let ids = Ids(&signature.store);
        check(ids.ty(signature.self_ty), r)?;
        check(signature.target_trait.map_or(Ok(()), |it| ids.path(it.path)), r)?;
        // alloc: the `Arc` the signature query returns, once per item asked.
        Ok(Arc::new(signature))
    }
}

impl Sig for Trait {
    type Of = TraitSignature;
    type Value = Arc<TraitSignature>;

    fn of(value: &Self::Value) -> &Self::Of {
        value
    }

    fn store(of: &Self::Of) -> Option<&ExpressionStore> {
        Some(&of.store)
    }

    fn put(of: &Self::Of, w: &mut Words) {
        w.put(&of.name);
        w.put(&of.generic_params);
        put_flags(w, &of.flags);
    }

    fn get(
        r: &mut Read<'_>,
        store: Option<ExpressionStore>,
        _: &CfgOptions,
    ) -> Result<Self::Value, FormatError> {
        let signature = TraitSignature {
            name: r.get()?,
            generic_params: r.get()?,
            flags: get_flags(r)?,
            store: needs(store, r)?,
        };
        check_in_store(&signature.store, &signature.generic_params, &[], r)?;
        // alloc: the `Arc` the signature query returns, once per item asked.
        Ok(Arc::new(signature))
    }
}

impl Sig for Function {
    type Of = FunctionSignature;
    type Value = Arc<FunctionSignature>;

    fn of(value: &Self::Value) -> &Self::Of {
        value
    }

    fn store(of: &Self::Of) -> Option<&ExpressionStore> {
        Some(&of.store)
    }

    fn put(of: &Self::Of, w: &mut Words) {
        w.put(&of.name);
        w.put(&of.generic_params);
        w.put(&of.params);
        w.put(&of.ret_type);
        w.put(&of.abi);
        put_flags(w, &of.flags);
        w.put(&of.splatted);
    }

    fn get(
        r: &mut Read<'_>,
        store: Option<ExpressionStore>,
        _: &CfgOptions,
    ) -> Result<Self::Value, FormatError> {
        let signature = FunctionSignature {
            name: r.get()?,
            generic_params: r.get()?,
            params: r.get()?,
            ret_type: r.get()?,
            abi: r.get()?,
            flags: get_flags(r)?,
            splatted: r.get()?,
            store: needs(store, r)?,
        };
        check_in_store(&signature.store, &signature.generic_params, &[], r)?;
        let types = signature.params.iter().copied().chain(signature.ret_type);
        check(Ids(&signature.store).types(types), r)?;
        // alloc: the `Arc` the signature query returns, once per item asked.
        Ok(Arc::new(signature))
    }
}

impl Sig for TypeAlias {
    type Of = TypeAliasSignature;
    type Value = Arc<TypeAliasSignature>;

    fn of(value: &Self::Value) -> &Self::Of {
        value
    }

    fn store(of: &Self::Of) -> Option<&ExpressionStore> {
        Some(&of.store)
    }

    fn put(of: &Self::Of, w: &mut Words) {
        w.put(&of.name);
        w.put(&of.generic_params);
        w.put(&of.bounds);
        w.put(&of.ty);
        put_flags(w, &of.flags);
    }

    fn get(
        r: &mut Read<'_>,
        store: Option<ExpressionStore>,
        _: &CfgOptions,
    ) -> Result<Self::Value, FormatError> {
        let signature = TypeAliasSignature {
            name: r.get()?,
            generic_params: r.get()?,
            bounds: r.get()?,
            ty: r.get()?,
            flags: get_flags(r)?,
            store: needs(store, r)?,
        };
        check_in_store(&signature.store, &signature.generic_params, &signature.bounds, r)?;
        check(Ids(&signature.store).types(signature.ty), r)?;
        // alloc: the `Arc` the signature query returns, once per item asked.
        Ok(Arc::new(signature))
    }
}

impl Sig for Fields {
    type Of = VariantFields;
    type Value = Arc<VariantFields>;

    fn of(value: &Self::Value) -> &Self::Of {
        value
    }

    fn store(of: &Self::Of) -> Option<&ExpressionStore> {
        Some(&of.store)
    }

    fn put(of: &Self::Of, w: &mut Words) {
        put_arena(w, of.fields());
        w.put(&of.shape);
    }

    fn get(
        r: &mut Read<'_>,
        store: Option<ExpressionStore>,
        _: &CfgOptions,
    ) -> Result<Self::Value, FormatError> {
        let fields = get_arena(r)?;
        let shape = r.get()?;
        let value = VariantFields::from_parts(fields, needs(store, r)?, shape);
        let ids = Ids(&value.store);
        for field in value.fields().values() {
            check(ids.ty(field.type_ref), r)?;
            check(field.default_value.map_or(Ok(()), |it| ids.konst(it)), r)?;
        }
        // alloc: the `Arc` the signature query returns, once per item asked.
        Ok(Arc::new(value))
    }
}

impl Sig for Variants {
    type Of = (EnumVariants, ThinVec<InactiveEnumVariantCode>);
    type Value = (EnumVariants, ThinVec<InactiveEnumVariantCode>);

    fn of(value: &Self::Value) -> &Self::Of {
        value
    }

    fn store(_: &Self::Of) -> Option<&ExpressionStore> {
        None
    }

    fn put((variants, inactive): &Self::Of, w: &mut Words) {
        w.len(variants.variants.len());
        let explicit = variants.with_explicit_discriminant().map(|(_, _, explicit)| explicit);
        for ((name, (id, shape)), explicit) in variants.variants.iter().zip(explicit) {
            w.put(name);
            w.put(id);
            w.put(shape);
            w.bool(explicit);
        }
        w.len(inactive.len());
        for code in inactive {
            w.put(&code.cfg);
            w.put(&code.ast_id);
        }
    }

    fn get(
        r: &mut Read<'_>,
        store: Option<ExpressionStore>,
        cfg: &CfgOptions,
    ) -> Result<Self::Value, FormatError> {
        if store.is_some() {
            return Err(r.invalid("enum variants with a store"));
        }
        let len = r.len()?;
        let mut variants = FxIndexMap::with_capacity_and_hasher(len, Default::default());
        let mut explicit = SmallVec::<[bool; 32]>::with_capacity(len);
        for _ in 0..len {
            let name: Name = r.get()?;
            let value = (r.get()?, r.get()?);
            explicit.push(r.bool()?);
            if variants.insert(name, value).is_some() {
                return Err(r.invalid("an enum variant named twice"));
            }
        }
        let len = r.len()?;
        let mut inactive = ThinVec::with_capacity(len);
        for _ in 0..len {
            let (cfg_expr, ast_id) = (r.get()?, r.get()?);
            // clone: each inactive variant holds the crate's options, as lowering gives it.
            inactive.push(InactiveEnumVariantCode { cfg: cfg_expr, opts: cfg.clone(), ast_id });
        }
        Ok((EnumVariants::from_parts(variants, &explicit), inactive))
    }
}

#[cfg(test)]
mod tests {
    use hir_expand::fixture;

    use super::*;
    use crate::{
        ModuleDefId,
        expr_store::path::{Names, UseArgs},
        hir::columns::idx,
        nameres::crate_def_map,
        test_db::TestDBExt,
    };

    #[test]
    fn a_where_predicate_or_bound_past_its_store_is_refused() {
        let db = fixture::db(
            "trait Tr<'a> {}\nfn f<'x, T>() where for<'a> T: Tr<'a>, T: 'x, 'x: 'x {}",
        )
        .into_db();
        let krate = db.fetch_test_crate();
        let def_map = crate_def_map(&db, krate);
        let mut function = None;
        for (_, module) in def_map.modules(&db) {
            for decl in module.scope(&db).declarations() {
                if let ModuleDefId::FunctionId(it) = decl {
                    function = Some(it);
                }
            }
        }
        let (signature, _) = FunctionSignature::with_source_map(&db, function.expect("`f`"));
        let (store, params) = (&signature.store, &signature.generic_params);
        assert_eq!(in_store(store, params, &[]), Ok(()));

        let (binder, target, bound) = (params.where_predicates.iter())
            .find_map(|it| match *it {
                WherePredicate::TypeBound { lifetimes: Some(names), target, bound } => {
                    Some((names, target, bound))
                }
                _ => None,
            })
            .expect("the `for<'a>` predicate");
        let one = |predicate: WherePredicate| GenericParams {
            where_predicates: Box::new([predicate]),
            ..GenericParams::default()
        };
        let none = GenericParams::default();
        let ok = WherePredicate::TypeBound { lifetimes: Some(binder), target, bound };
        assert_eq!(in_store(store, &one(ok), &[]), Ok(()));

        let names = store.types.paths.names.len() as u32;
        let past = Names { start: names, len: 1 };
        let past = WherePredicate::TypeBound { lifetimes: Some(past), target, bound };
        assert!(in_store(store, &one(past), &[]).is_err(), "a `for<..>` run past the names");

        let types = store.types.len() as u32;
        let past = WherePredicate::TypeBound { lifetimes: None, target: idx(types), bound };
        assert!(in_store(store, &one(past), &[]).is_err(), "a target past the types");

        let lifetimes = store.lifetimes.len() as u32;
        let past = WherePredicate::Lifetime { target: idx(lifetimes), bound: idx(0) };
        assert!(in_store(store, &one(past), &[]).is_err(), "a lifetime past its arena");

        let past = TypeBound::Lifetime(idx(lifetimes));
        assert!(in_store(store, &none, &[past]).is_err(), "a bound's lifetime past its arena");

        let uses = store.types.paths.use_args.len() as u32;
        let past = TypeBound::Use(UseArgs { start: 0, len: uses + 1 });
        assert!(in_store(store, &none, &[past]).is_err(), "a `use<..>` run past its table");
    }

    #[test]
    fn splatted_counts_the_parameters_cfg_leaves() {
        let db = fixture::db(
            "fn f(#[cfg(any())] x: u8, a: u8, #[rustc_splat] b: (u8,)) {}\nfn g(a: u8) {}\n\
             unsafe extern \"C\" { fn v(#[rustc_splat] a: (u8,), ...); }",
        )
        .into_db();
        let krate = db.fetch_test_crate();
        let mut found = Vec::new();
        for (_, module) in crate_def_map(&db, krate).modules(&db) {
            for decl in module.scope(&db).declarations() {
                if let ModuleDefId::FunctionId(it) = decl {
                    let signature = FunctionSignature::of(&db, it);
                    found.push((signature.name.as_str().to_owned(), signature.splatted));
                }
            }
        }
        found.sort();
        let expected = [("f".to_owned(), Some(1)), ("g".to_owned(), None), ("v".to_owned(), None)];
        assert_eq!(found, expected);
    }

    #[test]
    fn a_record_id_past_its_store_is_refused() {
        let db = fixture::db("fn f<const C: usize = 1>(_: (), _: u8) {}").into_db();
        let krate = db.fetch_test_crate();
        let mut function = None;
        for (_, module) in crate_def_map(&db, krate).modules(&db) {
            for decl in module.scope(&db).declarations() {
                if let ModuleDefId::FunctionId(it) = decl {
                    function = Some(it);
                }
            }
        }
        let (signature, _) = FunctionSignature::with_source_map(&db, function.expect("`f`"));
        let store = &signature.store;
        assert_eq!(in_store(store, &signature.generic_params, &[]), Ok(()));
        let ids = Ids(store);
        assert_eq!(ids.types(signature.params.iter().copied()), Ok(()));

        let types = store.types.len() as u32;
        assert!(ids.ty(idx(types)).is_err(), "a type past its arena");
        let path = PathId::from_type_ref_unchecked;
        let (mut paths, mut others) = (0, 0);
        for (id, ty) in store.types.iter() {
            match ty {
                TypeRef::Path(_) => {
                    assert_eq!(ids.path(path(id)), Ok(()));
                    paths += 1;
                }
                _ => {
                    assert!(ids.path(path(id)).is_err(), "{ty:?} as a trait");
                    others += 1;
                }
            }
        }
        assert!(paths > 0 && others > 0, "the fixture has a path type and a tuple type");
        assert!(ids.path(path(idx(types))).is_err(), "a trait past the types");

        let exprs = store.expr_count() as u32;
        assert!(exprs > 0, "the const param's default is an expression");
        assert_eq!(ids.konst(ConstRef { expr: idx(0) }), Ok(()));
        assert!(ids.konst(ConstRef { expr: idx(exprs) }).is_err(), "a constant past its store");
    }
}
