//! Things related to generics in the next-trait-solver.

use base_db::Db;
use hir_def::{
    FunctionId, GenericDefId, GenericParamId, TypeParamId,
    hir::generics::{GenericParamDataRef, LifetimeParamData},
};


use super::{Ctor, DbInterner, EarlyParamRegion, SolverDefId};

pub(crate) fn generics<'db>(interner: DbInterner<'db>, def: SolverDefId<'db>) -> Generics<'db> {
    let db = interner.db;
    let (def, consider_late_bound) = match (def.try_into(), def) {
        (Ok(def), _) => (def, false),
        (_, SolverDefId::InternedOpaqueTyId(id)) => match id.loc(db) {
            crate::ImplTraitId::ReturnTypeImplTrait(function_id, _) => {
                let mut generics = Generics::from_generic_def(db, function_id.into(), true);
                let declared = hir_def::hir::generics::GenericParams::of(db, function_id.into())
                    .len_lifetimes() as u32;
                generics.elided = Some((
                    function_id,
                    declared,
                    crate::lower::elided_lifetimes(db, function_id).len(),
                ));
                return generics;
            }
            crate::ImplTraitId::TypeAliasImplTrait(type_alias_id, _) => {
                (type_alias_id.into(), true)
            }
        },
        (_, SolverDefId::BuiltinDeriveImplId(id)) => {
            return crate::builtin_derive::generics_of(interner, id);
        }
        (_, SolverDefId::EnumVariantId(id) | SolverDefId::Ctor(Ctor::Enum(id))) => {
            (id.loc(db).parent.into(), false)
        }
        (_, SolverDefId::Ctor(Ctor::Struct(id))) => (id.into(), false),
        (_, SolverDefId::AnonConstId(id)) => {
            let loc = id.loc(db);
            let generic_def = loc.owner.generic_def(db);
            return if loc.allow_using_generic_params {
                Generics::from_generic_def(db, generic_def, false)
            } else {
                #[expect(
                    deprecated,
                    reason = "`Generics` only exposes an iterator over `GenericParamId`, \
                        so you cannot exploit the erroneous `crate::generics::Generics`"
                )]
                Generics {
                    generics: crate::generics::Generics::empty(generic_def),
                    additional_param: None,
                    consider_late_bound: false,
                    elided: None,
                }
            };
        }
        _ => panic!("No generics for {def:?}"),
    };

    Generics::from_generic_def(db, def, consider_late_bound)
}

#[derive(Debug)]
pub struct Generics<'db> {
    generics: crate::generics::Generics<'db>,
    /// This is used for builtin derives, specifically `CoercePointee`.
    additional_param: Option<GenericParamId>,
    consider_late_bound: bool,
    /// Of the `impl Trait` a function returns, how many lifetimes the function's parameters
    /// leave out, which it has as parameters after the function's own, and how many lifetimes
    /// the function declares, past which their ids are.
    elided: Option<(FunctionId, u32, u32)>,
}

impl<'db> Generics<'db> {
    pub(crate) fn from_generic_def(
        db: &'db Db,
        def: GenericDefId,
        consider_late_bound: bool,
    ) -> Generics<'db> {
        Generics {
            generics: crate::generics::generics(db, def),
            additional_param: None,
            consider_late_bound,
            elided: None,
        }
    }

    pub(crate) fn from_generic_def_plus_one(
        db: &'db Db,
        def: GenericDefId,
        additional_param: TypeParamId,
        consider_late_bound: bool,
    ) -> Generics<'db> {
        Generics {
            generics: crate::generics::generics(db, def),
            additional_param: Some(additional_param.into()),
            consider_late_bound,
            elided: None,
        }
    }

    /// A lifetime parameter for a lifetime a function's parameters leave out has no data.
    pub(super) fn iter(
        &self,
    ) -> impl Iterator<Item = (GenericParamId, Option<&LifetimeParamData>)> {
        let elided = self.elided.into_iter().flat_map(|(function, declared, len)| {
            (declared..declared + len).map(move |index| {
                let param = hir_def::LifetimeParamId {
                    parent: function.into(),
                    local_id: la_arena::Idx::from_raw(la_arena::RawIdx::from_u32(index)),
                };
                (param.into(), None)
            })
        });
        self.generics
            .iter(self.consider_late_bound)
            .map(|(id, data)| {
                if let GenericParamDataRef::LifetimeParamData(lt_param) = data {
                    (id, Some(lt_param))
                } else {
                    (id, None)
                }
            })
            .chain(self.additional_param.map(|param| (param, None)))
            .chain(elided)
    }

    /// The parameters the item declares itself, without its parent's.
    pub(super) fn iter_own(&self) -> impl Iterator<Item = GenericParamId> {
        self.generics.iter_self().map(|(id, _)| id)
    }
}

impl<'db> rustc_type_ir::inherent::GenericsOf<DbInterner<'db>> for Generics<'db> {
    fn count(&self) -> usize {
        self.generics.len(self.consider_late_bound)
            + usize::from(self.additional_param.is_some())
            + self.elided.map_or(0, |(_, _, len)| len as usize)
    }

    /// rustc's `region_param(ebr).def_id`. A lifetime parameter has no `SolverDefId` of its
    /// own, so this is the item declaring it, the id `BoundRegionKind::Named` holds too.
    fn param_region_def_id(
        self,
        _interner: DbInterner<'db>,
        ebr: EarlyParamRegion,
    ) -> SolverDefId<'db> {
        match self.iter().nth(ebr.index as usize) {
            Some((GenericParamId::LifetimeParamId(param), _)) if param == ebr.id => {
                param.parent.into()
            }
            param => panic!("expected lifetime parameter {ebr:?} at its index, found {param:?}"),
        }
    }
}
