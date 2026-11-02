//! The interner's answers that port a rustc rule rather than a fixed value, one test each.

use base_db::{Db, all_crates};
use hir_def::{
    AssocItemId, FunctionId, ImplId, ModuleDefId, ModuleId, TraitId, TypeAliasId,
    item_scope::ItemScope,
    nameres::crate_def_map,
    signatures::{FunctionSignature, TraitSignature},
};
use hir_expand::name::Name;
use intern::Symbol;
use la_arena::RawIdx;
use rustc_type_ir::Interner as _;
use hir_expand::fixture;

use crate::{
    ImplTraitId,
    db::{HirDatabase as _, InternedOpaqueTyId},
    next_solver::{
        AnyImplId, CallableIdWrapper, DbInterner, ImplOrTraitAssocTermId, ImplTraitIdx,
        SolverDefId, TermId,
    },
};

fn modules(db: &Db) -> Vec<ModuleId> {
    all_crates(db)
        .iter()
        .flat_map(|&krate| crate_def_map(db, krate).modules(db).map(|(id, _)| id).collect::<Vec<_>>())
        .collect()
}

fn scope(db: &Db, module: ModuleId) -> &ItemScope {
    module.def_map(db).module(db, module).scope(db)
}

fn function(db: &Db, name: &str) -> FunctionId {
    modules(db)
        .into_iter()
        .flat_map(|module| scope(db, module).declarations().collect::<Vec<_>>())
        .find_map(|def| match def {
            ModuleDefId::FunctionId(it) if FunctionSignature::of(db, it).name.as_str() == name => {
                Some(it)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no function `{name}` in the fixture"))
}

fn trait_(db: &Db, name: &str) -> TraitId {
    modules(db)
        .into_iter()
        .flat_map(|module| scope(db, module).declarations().collect::<Vec<_>>())
        .find_map(|def| match def {
            ModuleDefId::TraitId(it) if TraitSignature::of(db, it).name.as_str() == name => {
                Some(it)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no trait `{name}` in the fixture"))
}

fn assoc(db: &Db, trait_: TraitId, name: &str) -> AssocItemId {
    trait_
        .trait_items(db)
        .assoc_item_by_name(&Name::new_symbol_root(Symbol::intern(name)))
        .unwrap_or_else(|| panic!("no item `{name}` in the trait"))
}

fn assoc_type(db: &Db, trait_: TraitId, name: &str) -> TypeAliasId {
    match assoc(db, trait_, name) {
        AssocItemId::TypeAliasId(it) => it,
        it => panic!("`{name}` is {it:?}, not a type"),
    }
}

/// The impl of the trait named `trait_name`; each test's fixture has one.
fn impl_of(db: &Db, trait_name: &str) -> ImplId {
    let wanted = trait_(db, trait_name);
    modules(db)
        .into_iter()
        .flat_map(|module| scope(db, module).impls().collect::<Vec<_>>())
        .find(|&impl_| {
            db.impl_trait(impl_).is_some_and(|it| it.skip_binder().def_id.0 == wanted)
        })
        .unwrap_or_else(|| panic!("no impl of `{trait_name}` in the fixture"))
}

fn first_rpit(db: &Db, func: FunctionId) -> SolverDefId<'_> {
    let idx = ImplTraitIdx::from_raw(RawIdx::from_u32(0));
    InternedOpaqueTyId::new(db, ImplTraitId::ReturnTypeImplTrait(func, idx)).into()
}

#[test]
fn a_trait_item_has_a_definition_only_when_it_writes_one() {
    let db = fixture::db(
        r#"
#![feature(associated_type_defaults)]
trait Tr {
    type Bare;
    type Defaulted = u8;
    const BARE: u32;
    const DEFAULTED: u32 = 1;
}
"#,
    )
    .into_db();
    crate::next_solver::attach_db(&db, || {
        let interner = DbInterner::new_with(&db, fixture::test_crate(&db));
        let tr = trait_(&db, "Tr");
        for (name, expected) in
            [("Bare", false), ("Defaulted", true), ("BARE", false), ("DEFAULTED", true)]
        {
            let def = match assoc(&db, tr, name) {
                AssocItemId::TypeAliasId(it) => ImplOrTraitAssocTermId(TermId::TypeAliasId(it)),
                AssocItemId::ConstId(it) => ImplOrTraitAssocTermId(TermId::ConstId(it)),
                it => panic!("`{name}` is {it:?}"),
            };
            assert_eq!(interner.has_item_definition(def), expected, "`{name}`");
        }
    });
}

#[test]
fn an_impl_self_is_guaranteed_unsized_when_its_struct_tail_is_dyn_a_slice_or_str() {
    let db = fixture::db(
        r#"
//- minicore: sized
trait Tr {}
trait OnStr {}
trait OnSlice {}
trait OnDyn {}
trait OnTail {}
trait OnTuple {}
trait OnAlias {}
trait OnU8 {}
trait OnParam {}
struct Wrap<T: ?Sized>(u8, T);
type Bytes = [u8];
impl OnStr for str {}
impl OnSlice for [u8] {}
impl OnDyn for dyn Tr {}
impl OnTail for Wrap<Wrap<str>> {}
impl OnTuple for (u8, [u16]) {}
impl OnAlias for Wrap<Bytes> {}
impl OnU8 for Wrap<u8> {}
impl<T: ?Sized> OnParam for Wrap<T> {}
"#,
    )
    .into_db();
    crate::next_solver::attach_db(&db, || {
        let interner = DbInterner::new_with(&db, fixture::test_crate(&db));
        for (trait_name, expected) in [
            ("OnStr", true),
            ("OnSlice", true),
            ("OnDyn", true),
            ("OnTail", true),
            ("OnTuple", true),
            ("OnAlias", true),
            ("OnU8", false),
            ("OnParam", false),
        ] {
            let impl_ = AnyImplId::ImplId(impl_of(&db, trait_name));
            assert_eq!(
                interner.impl_self_is_guaranteed_unsized(impl_),
                expected,
                "the impl of `{trait_name}`"
            );
        }
    });
}

#[test]
fn a_function_has_target_features_when_it_enables_one() {
    let db = fixture::db(
        r#"
#[target_feature(enable = "neon")]
fn enables() {}
fn plain() {}
"#,
    )
    .into_db();
    crate::next_solver::attach_db(&db, || {
        let interner = DbInterner::new_with(&db, fixture::test_crate(&db));
        for (name, expected) in [("enables", true), ("plain", false)] {
            let def = CallableIdWrapper(function(&db, name).into());
            assert_eq!(interner.has_target_features(def), expected, "`{name}`");
        }
    });
}

const CONST_ALIASES: &str = r#"
//- minicore: sized, copy
#![feature(const_trait_impl)]
const trait Plain { type Item; }
trait NotConst { type Item; }
const trait Other {}
const trait Bounded { type Item: [const] Other; }
const fn const_rpit() -> impl Copy { 0u8 }
fn plain_rpit() -> impl Copy { 0u8 }
"#;

#[test]
fn an_alias_is_conditionally_const_as_its_trait_or_function_is() {
    let db = fixture::db(CONST_ALIASES).into_db();
    crate::next_solver::attach_db(&db, || {
        let interner = DbInterner::new_with(&db, fixture::test_crate(&db));
        let item = |trait_name| assoc_type(&db, trait_(&db, trait_name), "Item").into();
        assert!(interner.alias_has_const_conditions(item("Plain")));
        assert!(!interner.alias_has_const_conditions(item("NotConst")));
        assert!(interner.alias_has_const_conditions(first_rpit(&db, function(&db, "const_rpit"))));
        assert!(!interner.alias_has_const_conditions(first_rpit(&db, function(&db, "plain_rpit"))));
    });
}

#[test]
fn an_alias_that_writes_no_const_bound_implies_none() {
    let db = fixture::db(CONST_ALIASES).into_db();
    crate::next_solver::attach_db(&db, || {
        let interner = DbInterner::new_with(&db, fixture::test_crate(&db));
        let plain = assoc_type(&db, trait_(&db, "Plain"), "Item").into();
        let rpit = first_rpit(&db, function(&db, "const_rpit"));
        for def in [plain, rpit] {
            let bounds = interner.explicit_implied_const_bounds(def).skip_binder();
            assert_eq!(bounds.into_iter().count(), 0, "{def:?}");
        }
    });
}

#[test]
fn an_alias_implies_the_const_bounds_it_writes() {
    let db = fixture::db(CONST_ALIASES).into_db();
    crate::next_solver::attach_db(&db, || {
        let interner = DbInterner::new_with(&db, fixture::test_crate(&db));
        let bounded = assoc_type(&db, trait_(&db, "Bounded"), "Item").into();
        let traits: Vec<TraitId> = interner
            .explicit_implied_const_bounds(bounded)
            .skip_binder()
            .into_iter()
            .map(|it| it.skip_binder().def_id.0)
            .collect();
        assert_eq!(traits, [trait_(&db, "Other")]);
    });
}

#[test]
fn a_type_alias_impl_trait_is_never_conditionally_const() {
    let db = fixture::db(
        r#"
#![feature(type_alias_impl_trait)]
type Opaque = impl Sized;
"#,
    )
    .into_db();
    crate::next_solver::attach_db(&db, || {
        let interner = DbInterner::new_with(&db, fixture::test_crate(&db));
        let alias = modules(&db)
            .into_iter()
            .flat_map(|module| scope(&db, module).declarations().collect::<Vec<_>>())
            .find_map(|def| match def {
                ModuleDefId::TypeAliasId(it) => Some(it),
                _ => None,
            })
            .expect("the fixture's alias");
        let idx = ImplTraitIdx::from_raw(RawIdx::from_u32(0));
        let tait = InternedOpaqueTyId::new(&db, ImplTraitId::TypeAliasImplTrait(alias, idx));
        assert!(!interner.alias_has_const_conditions(tait.into()));
    });
}

const TWO_CRATES: &str = r#"
//- /up.rs crate:up
pub trait Up {}
pub struct Foreign;
//- /main.rs crate:main deps:up
pub trait Mine {}
fn here() {}
"#;

fn foreign_struct(db: &Db) -> hir_def::AdtId {
    modules(db)
        .into_iter()
        .flat_map(|module| scope(db, module).declarations().collect::<Vec<_>>())
        .find_map(|def| match def {
            ModuleDefId::AdtId(it) => Some(it),
            _ => None,
        })
        .expect("the fixture's `Foreign`")
}

#[test]
fn an_item_is_local_only_to_the_crate_that_defines_it() {
    use hir_def::HasModule;
    use rustc_type_ir::inherent::DefId as _;

    let db = fixture::db(TWO_CRATES).into_db();
    crate::next_solver::attach_db(&db, || {
        let (up, mine) = (trait_(&db, "Up"), trait_(&db, "Mine"));
        let main = DbInterner::new_with(&db, function(&db, "here").krate(&db));
        let upstream = DbInterner::new_with(&db, up.krate(&db));
        let (up, mine) = (SolverDefId::from(up), SolverDefId::from(mine));
        assert!(!up.is_local(main));
        assert!(mine.is_local(main));
        assert!(up.is_local(upstream));
        assert!(!mine.is_local(upstream));
    });
}

#[test]
fn coherence_leaves_a_goal_only_an_upstream_crate_could_implement_ambiguous() {
    use hir_def::HasModule;

    use crate::next_solver::{
        GenericArgs, ParamEnv, Ty,
        infer::{DbInternerInferExt, select::EvaluationResult},
    };
    use rustc_type_ir::TypingMode;

    let db = fixture::db(TWO_CRATES).into_db();
    crate::next_solver::attach_db(&db, || {
        let up = trait_(&db, "Up");
        let foreign = foreign_struct(&db);
        for (krate, expected) in [
            (function(&db, "here").krate(&db), EvaluationResult::EvaluatedToAmbig),
            (up.krate(&db), EvaluationResult::EvaluatedToErr),
        ] {
            let interner = DbInterner::new_with(&db, krate);
            let infcx = interner.infer_ctxt().build(TypingMode::Coherence);
            let ty = Ty::new_adt(interner, foreign, GenericArgs::empty());
            let answer = infcx.type_implements_trait(up, [ty], ParamEnv::empty());
            assert_eq!(answer, expected, "`Foreign: Up` asked by {krate:?}");
        }
    });
}
