//! rustc's `Ty::inhabited_predicate`: a `#[non_exhaustive]` enum of another crate is inhabited,
//! a private field hides an uninhabited type unless visibility is ignored.

use base_db::{Crate, Db, all_crates};
use hir_def::{AdtId, EnumVariantId, ModuleDefId, ModuleId, nameres::crate_def_map};
use hir_expand::name::Name;
use hir_expand::fixture;

use crate::{
    TyDefId,
    db::HirDatabase,
    inhabitedness::{
        is_enum_variant_privately_uninhabited, is_enum_variant_uninhabited_from,
        is_ty_uninhabited_from,
    },
    next_solver::{
        DbInterner, GenericArgs, ParamEnv, Ty, TypingMode,
        infer::{DbInternerInferExt, InferCtxt},
    },
};

const FIXTURE: &str = r#"
//- /dep.rs crate:dep
#[non_exhaustive]
pub enum ForeignNonExhaustive {}
pub enum ForeignVoid {}
#[non_exhaustive]
pub struct ForeignStruct(pub ForeignVoid);
//- /main.rs crate:main deps:dep
#[non_exhaustive]
pub enum LocalNonExhaustive {}
pub enum Void {}
mod m {
    pub struct Private(super::Void);
}
pub enum HasPrivate {
    Private(m::Private),
}
"#;

fn crate_named(db: &Db, name: &str) -> Crate {
    *all_crates(db)
        .iter()
        .find(|it| it.extra_data(db).display_name.as_ref().is_some_and(|it| it.to_string() == name))
        .unwrap()
}

/// What `name` names in the types of `module`.
fn named(db: &Db, module: ModuleId, name: &str) -> Option<ModuleDefId> {
    module.def_map(db).module(db, module).scope(db).get(&Name::new_root(name)).take_types()
}

/// The ADT `name` at `root`, or `dep::name` at the root of the crate `dep`, which is in the
/// extern prelude, not in `root`'s scope.
fn adt(db: &Db, root: ModuleId, name: &str) -> AdtId {
    let (module, name) = match name.strip_prefix("dep::") {
        Some(rest) => (crate_def_map(db, crate_named(db, "dep")).root_module_id(), rest),
        None => (root, name),
    };
    match named(db, module, name) {
        Some(ModuleDefId::AdtId(it)) => it,
        other => panic!("{name} is {other:?}"),
    }
}

fn ty<'db>(db: &'db Db, adt: AdtId) -> Ty<'db> {
    db.ty(TyDefId::AdtId(adt)).instantiate_identity().skip_norm_wip()
}

#[test]
fn only_a_foreign_non_exhaustive_enum_is_taken_as_inhabited() {
    let db = fixture::db(FIXTURE).into_db();
    let krate = crate_named(&db, "main");
    let root = crate_def_map(&db, krate).root_module_id();
    let interner = DbInterner::new_with(&db, krate);
    let infcx: InferCtxt<'_> = interner.infer_ctxt().build(TypingMode::non_body_analysis());
    let env = ParamEnv::empty();
    let uninhabited = |name| is_ty_uninhabited_from(&infcx, ty(&db, adt(&db, root, name)), root, env);
    assert!(!uninhabited("dep::ForeignNonExhaustive"));
    assert!(uninhabited("dep::ForeignVoid"));
    assert!(uninhabited("dep::ForeignStruct"), "only an enum's applies");
    assert!(uninhabited("LocalNonExhaustive"));
    assert!(uninhabited("Void"));

    let AdtId::EnumId(has_private) = adt(&db, root, "HasPrivate") else { panic!("an enum") };
    let variant: EnumVariantId = has_private.enum_variants(&db).variants.values().next().unwrap().0;
    let args = GenericArgs::empty();
    assert!(!is_enum_variant_uninhabited_from(&infcx, variant, args, root, env));
    assert!(is_enum_variant_privately_uninhabited(&infcx, variant, args, krate, env));
}
