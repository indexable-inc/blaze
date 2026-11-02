//! Coherence, specialization, opaque types and `Freeze` over fixtures: each answers as it
//! did before speed-types-probe-coherence's pass
//! (docs/v2/plan/lanes/items/speed-types-probe-coherence.md), and skips what it now skips.

use base_db::{Crate, all_crates};
use hir_def::{
    AdtId, DefWithBodyId, FunctionId, ImplId, ModuleDefId, TypeAliasId,
    signatures::{
        EnumSignature, FunctionSignature, StructSignature, TypeAliasSignature, UnionSignature,
    },
};
use hir_ty::{
    InferenceResult, check_orphan_rules,
    drop::is_freeze,
    impl_unconstrained_params,
    next_solver::{
        DbInterner, GenericArgs, ParamEnv, Ty, TyKind, TypingMode, infer::DbInternerInferExt,
    },
    tait_hidden_types,
};
use rustc_type_ir::{
    UintTy,
    inherent::{GenericArgs as _, IntoKind as _},
};
use hir_expand::fixture;

use super::db::TestDB;

/// How many times `query` is among the ingredient names in `executed`.
fn runs(executed: &[String], query: &str) -> usize {
    executed.iter().filter(|it| it.trim_end_matches('_').ends_with(query)).count()
}

fn crate_named(db: &TestDB, name: &str) -> Crate {
    *all_crates(db)
        .iter()
        .find(|it| {
            let display = &it.extra_data(db).display_name;
            display.as_ref().is_some_and(|it| it.crate_name().as_str() == name)
        })
        .unwrap_or_else(|| panic!("no crate `{name}`"))
}

fn declarations(db: &TestDB, krate: Crate) -> Vec<ModuleDefId> {
    hir_def::test_items::declarations(db, krate)
}

/// The impls of `krate`'s modules, in the order they are written.
fn impls(db: &TestDB, krate: Crate) -> Vec<ImplId> {
    hir_def::test_items::impls(db, krate)
}

fn function_named(db: &TestDB, krate: Crate, name: &str) -> FunctionId {
    declarations(db, krate)
        .into_iter()
        .find_map(|def| match def {
            ModuleDefId::FunctionId(it) if FunctionSignature::of(db, it).name.as_str() == name => {
                Some(it)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no function `{name}`"))
}

fn type_alias_named(db: &TestDB, krate: Crate, name: &str) -> TypeAliasId {
    declarations(db, krate)
        .into_iter()
        .find_map(|def| match def {
            ModuleDefId::TypeAliasId(it)
                if TypeAliasSignature::of(db, it).name.as_str() == name =>
            {
                Some(it)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no type alias `{name}`"))
}

fn adt_named(db: &TestDB, krate: Crate, name: &str) -> AdtId {
    declarations(db, krate)
        .into_iter()
        .find_map(|def| match def {
            ModuleDefId::AdtId(adt) => {
                let adt_name = match adt {
                    AdtId::StructId(it) => &StructSignature::of(db, it).name,
                    AdtId::EnumId(it) => &EnumSignature::of(db, it).name,
                    AdtId::UnionId(it) => &UnionSignature::of(db, it).name,
                };
                (adt_name.as_str() == name).then_some(adt)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no type `{name}`"))
}

fn has_type_mismatches(db: &TestDB, function: FunctionId) -> bool {
    hir_ty::attach_db(db, || {
        InferenceResult::of(db, DefWithBodyId::from(function)).has_type_mismatches()
    })
}

const UNCONSTRAINED: &str = r#"
pub trait It { type Item; }
pub trait Tr {}
pub struct W<T>(T);
pub struct S;
impl<T, U> Tr for W<T> where T: It<Item = U> {}
impl<T, U, V> Tr for (T,) where U: It<Item = V>, T: It<Item = U> {}
impl<T, U> Tr for S {}
impl<T: It<Item = U>, U> W<T> {}
impl<T, U> It for W<T> where W<T>: It<Item = U> { type Item = (); }
impl<const N: usize> Tr for [u8; 1] {}
impl<'a, T> Tr for &'a [T] {}
"#;

/// A projection constrains its value once what it names is, over as many rounds as a chain
/// takes (`V` after `U`, written first); one of the impl's own trait ref never does. The
/// unconstrained parameters come in declaration order, const ones too, lifetimes never.
#[test]
fn unconstrained_params_are_as_before_in_declaration_order() {
    let fx = fixture::db(&format!("//- /main.rs crate:main\n{UNCONSTRAINED}"));
    let _ = fx.only_file();
    let db = fx.into_db();
    let main = crate_named(&db, "main");
    let found: Vec<Vec<u32>> = hir_ty::attach_db(&db, || {
        impls(&db, main)
            .into_iter()
            .map(|impl_| {
                impl_unconstrained_params(&db, impl_)
                    .into_iter()
                    .map(|param| u32::from(param.local_id.into_raw()))
                    .collect()
            })
            .collect()
    });
    let expected: [&[u32]; 7] = [&[], &[], &[0, 1], &[], &[1], &[0], &[]];
    assert_eq!(found, expected.map(<[u32]>::to_vec));
}

const SPECIALIZATION: &str = r#"//- /dep.rs crate:dep
#![feature(specialization)]
pub trait Tr { type A; }
impl<T> Tr for T { default type A = u8; }
//- /main.rs crate:main deps:dep
#![feature(specialization)]
use dep::Tr;
pub struct S;
pub struct O;
impl Tr for S {}
impl Tr for O { type A = u16; }
impl Tr for u32 { type A = u32; }
pub fn good(x: <S as Tr>::A) -> u8 { x }
pub fn bad(x: <S as Tr>::A) -> u16 { x }
"#;

/// `impl Tr for S` leaves `A` out and takes it from the blanket impl of another crate, as
/// before. The impls for `O` and `u32` define `A` too, but their headers cannot unify with
/// `S`'s, so no `specializes_query` is made for them: at most the pair `S`, blanket and its
/// reverse, where every impl that defines `A` (four queries) was asked before.
#[test]
fn a_specializing_impl_asks_only_the_impls_its_header_unifies_with() {
    let fx = fixture::db(SPECIALIZATION);
    let _ = fx.files().to_vec();
    let db = fx.into_db();
    let main = crate_named(&db, "main");
    let good = function_named(&db, main, "good");
    let bad = function_named(&db, main, "bad");
    let executed = db.log_executed(|| {
        assert!(!has_type_mismatches(&db, good), "`<S as Tr>::A` is `u8`");
        assert!(has_type_mismatches(&db, bad), "`<S as Tr>::A` is no `u16`");
    });
    assert!(runs(&executed, "specializes_query") <= 2, "{executed:#?}");
}

/// `define_opaque` is core's built-in attribute macro, declared here as core declares it: the
/// fixture has no core, and an unresolved `#[define_opaque]` names no defining function.
const TAITS: &str = r#"
#![feature(type_alias_impl_trait, rustc_attrs, decl_macro)]
#[rustc_builtin_macro]
pub macro define_opaque($($tt:tt)*) {}
pub trait Tr {}
impl Tr for u32 {}
pub type Plain = u32;
pub type Opaque = impl Tr;
#[define_opaque(Plain)]
pub fn plain() -> Plain { 0 }
#[define_opaque(Opaque)]
pub fn opaque() -> Opaque { 0u32 }
"#;

/// An alias with no `impl Trait` has no hidden types, as before, and its defining function
/// is no longer inferred to find that out.
#[test]
fn an_alias_without_impl_trait_infers_no_body() {
    let fx = fixture::db(&format!("//- /main.rs crate:main\n{TAITS}"));
    let _ = fx.only_file();
    let db = fx.into_db();
    let plain = type_alias_named(&db, crate_named(&db, "main"), "Plain");
    let executed = db.log_executed(|| {
        hir_ty::attach_db(&db, || assert_eq!(tait_hidden_types(&db, plain).values().count(), 0))
    });
    assert_eq!(runs(&executed, "infer_body_query"), 0, "{executed:#?}");
}

#[test]
fn an_alias_with_impl_trait_takes_its_hidden_type_from_its_defining_body() {
    let fx = fixture::db(&format!("//- /main.rs crate:main\n{TAITS}"));
    let _ = fx.only_file();
    let db = fx.into_db();
    let opaque = type_alias_named(&db, crate_named(&db, "main"), "Opaque");
    let executed = db.log_executed(|| {
        hir_ty::attach_db(&db, || {
            let hidden: Vec<_> = tait_hidden_types(&db, opaque)
                .values()
                .map(|it| it.get().skip_binder())
                .collect();
            assert_eq!(hidden.len(), 1, "{hidden:?}");
            assert!(matches!(hidden[0].kind(), TyKind::Uint(UintTy::U32)), "{hidden:?}");
        })
    });
    assert!(runs(&executed, "infer_body_query") >= 1, "{executed:#?}");
}

const ORPHAN: &str = r#"//- /dep.rs crate:dep
pub trait Foreign<T> {}
pub trait Proj { type Out; }
//- /main.rs crate:main deps:dep
pub struct Local;
impl dep::Proj for Local { type Out = Local; }
impl dep::Foreign<<Local as dep::Proj>::Out> for u32 {}
impl dep::Foreign<Local> for u8 {}
impl dep::Foreign<u8> for u16 {}
"#;

/// A header with an alias is normalized before the orphan rules look at it (the projection
/// is `Local`), as before; one without is looked at as written, with no parameter
/// environment made for it.
#[test]
fn orphan_rules_normalize_only_a_header_with_an_alias() {
    let fx = fixture::db(ORPHAN);
    let _ = fx.files().to_vec();
    let db = fx.into_db();
    let impls = impls(&db, crate_named(&db, "main"));
    let answers: Vec<bool> =
        hir_ty::attach_db(&db, || impls.iter().map(|&it| check_orphan_rules(&db, it)).collect());
    assert_eq!(answers, [true, true, true, false]);

    let fx = fixture::db(ORPHAN);
    let _ = fx.files().to_vec();
    let db = fx.into_db();
    let impls = self::impls(&db, crate_named(&db, "main"));
    let plain = db.log_executed(|| hir_ty::attach_db(&db, || assert!(check_orphan_rules(&db, impls[2]))));
    assert_eq!(runs(&plain, "trait_environment_query"), 0, "{plain:#?}");
    let alias = db.log_executed(|| hir_ty::attach_db(&db, || assert!(check_orphan_rules(&db, impls[1]))));
    assert!(runs(&alias, "trait_environment_query") >= 1, "{alias:#?}");
}

const FREEZE: &str = r#"
#![feature(lang_items)]
#[lang = "unsafe_cell"]
pub struct UnsafeCell<T>(T);
pub struct Cell<T>(UnsafeCell<T>);
pub enum Plain { A(u8), B { x: u32 }, C }
pub enum Shared { A(u8), B(Cell<u8>) }
pub struct Wrap(Plain, Shared);
pub union U { a: u8 }
pub struct Empty;
pub enum Never {}
"#;

/// `Freeze` walks every variant's fields in place: an enum is `Freeze` when each variant is.
#[test]
fn is_freeze_walks_every_variant() {
    let fx = fixture::db(&format!("//- /main.rs crate:main\n{FREEZE}"));
    let _ = fx.only_file();
    let db = fx.into_db();
    let main = crate_named(&db, "main");
    let names = ["UnsafeCell", "Cell", "Plain", "Shared", "Wrap", "U", "Empty", "Never"];
    let adts = names.map(|name| adt_named(&db, main, name));
    let answers = hir_ty::attach_db(&db, || {
        let interner = DbInterner::new_with(&db, main);
        let infcx = interner.infer_ctxt().build(TypingMode::non_body_analysis());
        adts.map(|adt| {
            let args = GenericArgs::identity_for_item(interner, adt.into());
            is_freeze(&infcx, Ty::new_adt(interner, adt, args), ParamEnv::empty())
        })
    });
    assert_eq!(answers, [false, false, true, false, false, false, true, true]);
}
