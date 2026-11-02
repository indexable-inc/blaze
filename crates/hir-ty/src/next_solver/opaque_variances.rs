//! rustc's `variances_of` an opaque type (`rustc_hir_analysis::variance::variance_of_opaque`)
//! and `TyCtxt::opt_alias_variances`.
//!
//! rustc gives an `impl Trait` its own copy of each lifetime it captures, invariant, and makes
//! every lifetime of the items around it bivariant. Here an opaque's arguments are its owner's
//! (a function's, late-bound and elided lifetimes included, or a type alias's), so a lifetime is
//! invariant where rustc captures it and bivariant where rustc does not; type and const
//! parameters are invariant. What rustc captures:
//!
//! - an `impl Trait` with `use<..>`, the lifetimes it lists, and in a trait or a trait impl the
//!   trait's or impl's own too;
//! - one in a trait or a trait impl, in edition 2024, or the future of an `async fn`, all of
//!   them;
//! - a type alias's (TAIT, ATPIT), all of them;
//! - else (edition 2021 and before), the lifetimes its bounds name.
//!
//! The last is rustc's bounds walk, run on every opaque: it skips the self type of trait,
//! projection and host-effect bounds, and through an `impl Trait` nested in the bounds counts
//! only what that one captures.
//!
//! blaze has no impl-trait-in-trait projection: an `impl Trait` a trait method returns is an
//! opaque of that method, so a projection's variances are `None`, as rustc's for any other.

use base_db::Db;
use hir_def::{
    FunctionId, GenericParamId, ItemContainerId, LifetimeParamId, Lookup as _,
    expr_store::{ExpressionStore, path::GenericArg as PathGenericArg},
    hir::{
        generics::LifetimeParamData,
        type_ref::{LifetimeRef, LifetimeRefId, TypeBound, TypeRef, TypeRefId, UseArgRef},
    },
    signatures::FunctionSignature,
};
use rustc_type_ir::{
    AliasTyKind, TypeSuperVisitable, TypeVisitable, TypeVisitor, Variance,
    inherent::{GenericsOf as _, IntoKind as _},
};

use crate::{
    ElidedOutput, ImplTraitId,
    db::{HirDatabase as _, InternedOpaqueTyId},
    elided_lifetimes,
};

use super::{
    AliasTermKind, ClauseKind, DbInterner, GenericArgs, ImplTraitIdx, Region, RegionKind,
    StoredVariancesOf, Ty, TyKind, VariancesOf,
    generics::{Generics, generics},
};

/// rustc's `variances_of` the opaque type `opaque`, by the arguments it has.
pub(crate) fn variances_of_opaque<'db>(
    interner: DbInterner<'db>,
    opaque: InternedOpaqueTyId<'db>,
) -> VariancesOf<'db> {
    opaque_variances_query(interner.db, opaque).as_ref()
}

/// rustc's `TyCtxt::opt_alias_variances`: an opaque type's variances, `None` for any other
/// alias, whose arguments are related invariantly.
pub(crate) fn opt_alias_variances<'db>(
    interner: DbInterner<'db>,
    kind: AliasTermKind<'db>,
) -> Option<VariancesOf<'db>> {
    match kind {
        AliasTermKind::OpaqueTy { def_id } => Some(variances_of_opaque(interner, def_id.0)),
        AliasTermKind::ProjectionTy { .. }
        | AliasTermKind::InherentTy { .. }
        | AliasTermKind::FreeTy { .. }
        | AliasTermKind::AnonConst { .. }
        | AliasTermKind::ProjectionConst { .. }
        | AliasTermKind::FreeConst { .. }
        | AliasTermKind::InherentConstSelf { .. }
        | AliasTermKind::InherentConstImpl { .. } => None,
    }
}

#[blaze_query::tracked(returns(ref), mem_tag = Infer)]
fn opaque_variances_query<'db>(db: &'db Db, opaque: InternedOpaqueTyId<'db>) -> StoredVariancesOf {
    let interner = DbInterner::new_no_crate(db);
    let params = generics(interner, opaque.into());
    let count = params.count();
    if count == 0 {
        return VariancesOf::empty().store();
    }
    let root = opaque.loc(db);
    blaze_mem::frame::current(|frame| {
        let variances = frame.alloc_slice_fill_with(count, |_| Variance::Invariant);
        for (variance, (param, _)) in variances.iter_mut().zip(params.iter()) {
            if let GenericParamId::LifetimeParamId(_) = param {
                *variance = Variance::Bivariant;
            }
        }
        match root {
            ImplTraitId::ReturnTypeImplTrait(function, index) => {
                capture_return_position(db, function, index, &params, variances)
            }
            ImplTraitId::TypeAliasImplTrait(..) => variances.fill(Variance::Invariant),
        }
        if variances.iter().any(|&variance| variance == Variance::Bivariant) {
            let mut collector = Collector { interner, root, variances: &mut *variances };
            for &clause in root.all_bounds(db).skip_binder() {
                collector.visit_bound(clause);
            }
        }
        VariancesOf::new_from_slice(variances).store()
    })
}

/// Makes the lifetimes of the `index`th `impl Trait` in `function`'s return type that rustc
/// captures without reading its bounds invariant.
fn capture_return_position(
    db: &Db,
    function: FunctionId,
    index: ImplTraitIdx,
    params: &Generics<'_>,
    variances: &mut [Variance],
) {
    let loc = function.lookup(db);
    let in_trait = match loc.container {
        ItemContainerId::TraitId(_) => true,
        ItemContainerId::ImplId(it) => db.impl_trait(it).is_some(),
        ItemContainerId::ModuleId(_) | ItemContainerId::ExternBlockId(_) => false,
    };
    let signature = FunctionSignature::of(db, function);
    let index = index.into_raw().into_u32();
    let mut nth = index as usize;
    let listed = signature
        .ret_type
        .and_then(|ret| nth_impl_trait(&signature.store, ret, &mut nth))
        .and_then(|bounds| {
            bounds.iter().find_map(|bound| match bound {
                TypeBound::Use(args) => Some(signature.store.use_args(*args)),
                _ => None,
            })
        });
    let Some(listed) = listed else {
        // An `async fn`'s future is its return type's first `impl Trait`.
        let future = index == 0
            && signature.is_async()
            && signature
                .ret_type
                .is_some_and(|ret| matches!(signature.store.type_ref(ret), TypeRef::ImplTrait(_)));
        if in_trait || future || loc.id.file_id.edition(db).at_least_2024() {
            variances.fill(Variance::Invariant);
        }
        return;
    };
    let elided = elided_lifetimes(db, function);
    let declared = variances.len() - elided.len() as usize;
    // `use<'_>` lists the lifetime the elision rules give the return type.
    let output = listed
        .iter()
        .any(|arg| {
            matches!(arg, UseArgRef::Lifetime(it)
                if matches!(signature.store[*it], LifetimeRef::Placeholder))
        })
        .then_some(elided.output)
        .flatten();
    for (at, (param, data)) in params.iter().enumerate() {
        let GenericParamId::LifetimeParamId(param) = param else { continue };
        let captured = match data {
            Some(data) => {
                (in_trait && param.parent != function.into())
                    || matches!(output, Some(ElidedOutput::Written(it))
                        if names(&signature.store[LifetimeRefId::from_raw(
                            la_arena::RawIdx::from_u32(it),
                        )], param, data))
                    || listed.iter().any(|arg| {
                        matches!(arg, UseArgRef::Lifetime(it)
                            if names(&signature.store[*it], param, data))
                    })
            }
            None => output == Some(ElidedOutput::Elided((at - declared) as u32)),
        };
        if captured {
            variances[at] = Variance::Invariant;
        }
    }
}

fn names(lifetime: &LifetimeRef, param: LifetimeParamId, data: &LifetimeParamData) -> bool {
    match lifetime {
        LifetimeRef::Named(name) => *name == data.name,
        LifetimeRef::Param(it) => *it == param,
        LifetimeRef::Static | LifetimeRef::Placeholder | LifetimeRef::Error => false,
    }
}

/// The bounds of the `index`th `impl Trait` written in `ty`, counted in the order lowering
/// numbers them: each before those in its bounds, `Option<impl Iterator<Item = impl Sized>>`
/// has the `Iterator` one first.
fn nth_impl_trait<'s>(
    store: &'s ExpressionStore,
    ty: TypeRefId,
    index: &mut usize,
) -> Option<&'s [TypeBound]> {
    let in_path = |path: hir_def::expr_store::path::Path<'s>, index: &mut usize| {
        for args in path.generic_args().into_iter().flatten().flatten() {
            for arg in args.args {
                if let PathGenericArg::Type(it) = arg
                    && let Some(found) = nth_impl_trait(store, *it, index)
                {
                    return Some(found);
                }
            }
            for binding in &args.bindings {
                if let Some(it) = binding.type_ref
                    && let Some(found) = nth_impl_trait(store, it, index)
                {
                    return Some(found);
                }
            }
        }
        None
    };
    let in_bounds = |bounds: &'s [TypeBound], index: &mut usize| {
        bounds.iter().find_map(|bound| match bound {
            TypeBound::Path(path, _) | TypeBound::ForLifetime(_, path) => {
                in_path(store.path(*path), index)
            }
            _ => None,
        })
    };
    match store.type_ref(ty) {
        TypeRef::ImplTrait(bounds) => {
            if *index == 0 {
                return Some(bounds);
            }
            *index -= 1;
            in_bounds(bounds, index)
        }
        TypeRef::DynTrait(bounds) => in_bounds(bounds, index),
        TypeRef::Path(path) => in_path(path, index),
        TypeRef::Reference(it) => nth_impl_trait(store, it.ty, index),
        TypeRef::RawPtr(it, _) | TypeRef::Slice(it) | TypeRef::PatternType(it, _) => {
            nth_impl_trait(store, it, index)
        }
        TypeRef::Array(it) => nth_impl_trait(store, it.ty, index),
        TypeRef::Tuple(tys) => tys.iter().find_map(|&it| nth_impl_trait(store, it, index)),
        _ => None,
    }
}

/// rustc's `OpaqueTypeLifetimeCollector`: makes each lifetime parameter it meets invariant.
struct Collector<'v, 'db> {
    interner: DbInterner<'db>,
    root: ImplTraitId,
    variances: &'v mut [Variance],
}

impl<'db> Collector<'_, 'db> {
    fn visit_bound(&mut self, clause: super::Clause<'db>) {
        match clause.kind().skip_binder() {
            ClauseKind::Trait(it) => self.visit_after_self(it.trait_ref.args),
            ClauseKind::HostEffect(it) => self.visit_after_self(it.trait_ref.args),
            ClauseKind::Projection(it) => {
                self.visit_after_self(it.projection_term.args);
                it.term.visit_with(self);
            }
            ClauseKind::TypeOutlives(it) => it.1.visit_with(self),
            ClauseKind::RegionOutlives(_)
            | ClauseKind::ConstArgHasType(..)
            | ClauseKind::WellFormed(_)
            | ClauseKind::ConstEvaluatable(_)
            | ClauseKind::UnstableFeature(_) => clause.visit_with(self),
        }
    }

    fn visit_after_self(&mut self, args: GenericArgs<'db>) {
        for arg in args.iter().skip(1) {
            arg.visit_with(self);
        }
    }

    /// An `impl Trait` nested in the root's bounds is lowered after it, under the same owner; a
    /// type alias naming itself in its bounds lowers to an error, so no other comes after it.
    fn visit_opaque(&mut self, opaque: InternedOpaqueTyId<'db>, args: GenericArgs<'db>) {
        let nested = match (opaque.loc(self.interner.db), self.root) {
            (
                ImplTraitId::ReturnTypeImplTrait(owner, index),
                ImplTraitId::ReturnTypeImplTrait(root, root_index),
            ) => owner == root && index.into_raw().into_u32() > root_index.into_raw().into_u32(),
            (
                ImplTraitId::TypeAliasImplTrait(owner, index),
                ImplTraitId::TypeAliasImplTrait(root, root_index),
            ) => owner == root && index.into_raw().into_u32() > root_index.into_raw().into_u32(),
            _ => false,
        };
        if !nested {
            args.visit_with(self);
            return;
        }
        let variances = variances_of_opaque(self.interner, opaque);
        debug_assert_eq!(args.len(), variances.len(), "the arguments of {opaque:?}");
        for (arg, variance) in args.iter().zip(variances.iter()) {
            if variance != Variance::Bivariant {
                arg.visit_with(self);
            }
        }
    }
}

impl<'db> TypeVisitor<DbInterner<'db>> for Collector<'_, 'db> {
    type Result = ();

    fn visit_region(&mut self, region: Region<'db>) {
        if let RegionKind::ReEarlyParam(param) = region.kind() {
            self.variances[param.index as usize] = Variance::Invariant;
        }
    }

    fn visit_ty(&mut self, ty: Ty<'db>) {
        if let TyKind::Alias(_, alias) = ty.kind()
            && let AliasTyKind::Opaque { def_id } = alias.kind
        {
            self.visit_opaque(def_id.0, alias.args);
        } else {
            ty.super_visit_with(self);
        }
    }
}

#[cfg(test)]
mod tests {
    use base_db::{Crate, Db};
    use hir_def::{FunctionId, GenericParamId, ModuleDefId, nameres::crate_def_map};
    use hir_expand::name::Name;
    use rustc_type_ir::Variance;
    use hir_expand::fixture;

    use super::{opt_alias_variances, variances_of_opaque};
    use crate::{
        ImplTraitId,
        db::InternedOpaqueTyId,
        next_solver::{
            AliasTermKind, DbInterner, ImplTraitIdx, OpaqueTyIdWrapper, TraitAssocTyId,
            generics::generics,
        },
    };

    fn fixture(text: &str) -> (Db, Crate) {
        let db = fixture::db(text).into_db();
        let krate = fixture::test_crate(&db);
        (db, krate)
    }

    fn item(db: &Db, krate: Crate, name: &str) -> ModuleDefId {
        let def_map = crate_def_map(db, krate);
        let scope = def_map.module(db, def_map.root_module_id()).scope(db);
        scope
            .types()
            .map(|(it, item)| (it, item.def))
            .chain(scope.values().map(|(it, item)| (it, item.def)))
            .find(|(it, _)| it.as_str() == name)
            .map(|(_, def)| def)
            .unwrap_or_else(|| panic!("the fixture defines `{name}`"))
    }

    fn function(db: &Db, krate: Crate, name: &str) -> FunctionId {
        match item(db, krate, name) {
            ModuleDefId::FunctionId(it) => it,
            other => panic!("`{name}` is {other:?}"),
        }
    }

    fn opaque<'db>(db: &'db Db, function: FunctionId, index: u32) -> InternedOpaqueTyId<'db> {
        let index = ImplTraitIdx::from_raw(la_arena::RawIdx::from_u32(index));
        InternedOpaqueTyId::new(db, ImplTraitId::ReturnTypeImplTrait(function, index))
    }

    /// Each parameter of `opaque`, a lifetime by its name and any other as `None`, with its
    /// variance.
    fn variances(
        db: &Db,
        krate: Crate,
        opaque: InternedOpaqueTyId<'_>,
    ) -> Vec<(Option<String>, Variance)> {
        let interner = DbInterner::new_with(db, krate);
        let params = generics(interner, opaque.into());
        let variances = variances_of_opaque(interner, opaque);
        params
            .iter()
            .zip(variances.iter())
            .map(|((param, data), variance)| {
                let name = match param {
                    GenericParamId::LifetimeParamId(_) => Some(
                        data.map_or_else(|| "'_".to_owned(), |it| it.name.as_str().to_owned()),
                    ),
                    _ => None,
                };
                (name, variance)
            })
            .collect()
    }

    fn lifetime(of: &[(Option<String>, Variance)], name: &str) -> Variance {
        of.iter()
            .find(|(it, _)| it.as_deref() == Some(name))
            .map(|&(_, variance)| variance)
            .unwrap_or_else(|| panic!("no lifetime `{name}` in {of:?}"))
    }

    fn assert_types_invariant(of: &[(Option<String>, Variance)]) {
        for (name, variance) in of {
            if name.is_none() {
                assert_eq!(*variance, Variance::Invariant, "a type parameter in {of:?}");
            }
        }
    }

    const EDITION_2021: &str = r#"
//- minicore: sized, iterator
//- /main.rs edition:2021
fn f<'a, T>(x: &'a T) -> impl Sized + 'a { x }
fn unnamed<'a, 'b, T>(x: &'a T, _: &'b u8) -> impl Sized + 'a { x }
fn nested<'a, 'b>(x: &'a u8, _: &'b u8) -> impl Iterator<Item = impl Sized + 'a> {
    core::iter::once(x)
}
fn listed<'a, 'b>(x: &'a u8, _: &'b u8) -> impl Sized + use<'a> { x }
trait Tr { type A; }
"#;

    #[test]
    fn a_lifetime_the_bounds_name_is_invariant() {
        let (db, krate) = fixture(EDITION_2021);
        let of = variances(&db, krate, opaque(&db, function(&db, krate, "f"), 0));
        assert_eq!(lifetime(&of, "'a"), Variance::Invariant);
        assert_types_invariant(&of);
        assert!(of.iter().any(|(name, _)| name.is_none()), "`T` is a parameter: {of:?}");
    }

    #[test]
    fn a_lifetime_the_bounds_do_not_name_is_bivariant() {
        let (db, krate) = fixture(EDITION_2021);
        let of = variances(&db, krate, opaque(&db, function(&db, krate, "unnamed"), 0));
        assert_eq!(lifetime(&of, "'a"), Variance::Invariant);
        assert_eq!(lifetime(&of, "'b"), Variance::Bivariant);
        assert_types_invariant(&of);
    }

    #[test]
    fn a_nested_impl_trait_makes_what_it_captures_invariant() {
        let (db, krate) = fixture(EDITION_2021);
        let nested = function(&db, krate, "nested");
        let outer = variances(&db, krate, opaque(&db, nested, 0));
        assert_eq!(lifetime(&outer, "'a"), Variance::Invariant);
        assert_eq!(lifetime(&outer, "'b"), Variance::Bivariant);
        let inner = variances(&db, krate, opaque(&db, nested, 1));
        assert_eq!(lifetime(&inner, "'a"), Variance::Invariant);
        assert_eq!(lifetime(&inner, "'b"), Variance::Bivariant);
    }

    #[test]
    fn use_captures_what_it_lists() {
        let (db, krate) = fixture(EDITION_2021);
        let of = variances(&db, krate, opaque(&db, function(&db, krate, "listed"), 0));
        assert_eq!(lifetime(&of, "'a"), Variance::Invariant);
        assert_eq!(lifetime(&of, "'b"), Variance::Bivariant);
    }

    #[test]
    fn edition_2024_captures_every_lifetime() {
        let (db, krate) = fixture(
            r#"
//- minicore: sized
//- /main.rs edition:2024
fn all<'a, 'b>(x: &'a u8, _: &'b u8) -> impl Sized { x }
fn none<'a, 'b>(_: &'a u8, _: &'b u8) -> impl Sized + use<> {}
"#,
        );
        let all = variances(&db, krate, opaque(&db, function(&db, krate, "all"), 0));
        assert_eq!(lifetime(&all, "'a"), Variance::Invariant);
        assert_eq!(lifetime(&all, "'b"), Variance::Invariant);
        let none = variances(&db, krate, opaque(&db, function(&db, krate, "none"), 0));
        assert_eq!(lifetime(&none, "'a"), Variance::Bivariant);
        assert_eq!(lifetime(&none, "'b"), Variance::Bivariant);
    }

    #[test]
    fn opt_alias_variances_is_some_only_for_an_opaque() {
        let (db, krate) = fixture(EDITION_2021);
        let interner = DbInterner::new_with(&db, krate);
        let f = opaque(&db, function(&db, krate, "f"), 0);
        let of_opaque =
            opt_alias_variances(interner, AliasTermKind::OpaqueTy { def_id: OpaqueTyIdWrapper(f) });
        assert_eq!(of_opaque, Some(variances_of_opaque(interner, f)));

        let ModuleDefId::TraitId(tr) = item(&db, krate, "Tr") else { panic!("`Tr` is a trait") };
        let assoc = tr
            .trait_items(&db)
            .associated_type_by_name(&Name::new_root("A"))
            .expect("`Tr` has `A`");
        let of_projection = opt_alias_variances(
            interner,
            AliasTermKind::ProjectionTy { def_id: TraitAssocTyId(assoc) },
        );
        assert_eq!(of_projection, None);
    }
}
