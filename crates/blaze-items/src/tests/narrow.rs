//! Facts recorded where they are read (docs/v2/plan/lanes/items.md section 6.2): each test runs
//! a query, edits, runs it again and checks what reran. A query that reads a narrow fact reruns
//! only when that fact changed; the narrow query itself reruns and comes out equal.

use base_db::{Crate, FileId, all_crates};
use either::Either;
use hir_def::{
    AdtId, AssocItemId, DefWithBodyId, FunctionId, GenericDefId, ImplId, ModuleDefId, StructId,
    TraitId,
    expr_store::Body,
    nameres::{DefMap, crate_def_map},
    signatures::{FunctionSignature, StructSignature, TraitSignature},
};
use hir_expand::name::Name;
use hir_ty::{
    InferenceResult, ParamEnvAndCrate, check_orphan_rules,
    db::HirDatabase,
    drop::destructor,
    layout::layout_of_adt_query,
    method_resolution::{
        ImplWork, InherentImpls, SelfTyKey, TraitImpls, TraitValueItems, impl_work,
        trait_value_item,
    },
    next_solver::{DbInterner, GenericArgs, SimplifiedType, Ty},
    traits::{implements_trait_unique, implements_traits_unique, is_inherent_impl_coherent},
};
use rustc_type_ir::inherent::GenericArgs as _;
use hir_expand::fixture;

use super::db::TestDB;

/// Whether `query` is among the ingredient names in `executed`, which name a method of a
/// tracked impl by its type and method (`TraitImpls::for_crate`).
fn ran(executed: &[String], query: &str) -> bool {
    executed.iter().any(|it| it.trim_end_matches('_').ends_with(query))
}

fn single_file(text: &str) -> (TestDB, FileId) {
    let fx = fixture::db(&format!("//- /main.rs crate:main\n{text}"));
    let file = fx.only_file();
    let db = fx.into_db();
    let file = file.file_id(&db);
    (db, file)
}

fn krate(db: &TestDB) -> Crate {
    *all_crates(db).last().expect("a crate")
}

fn declarations(db: &TestDB) -> Vec<ModuleDefId> {
    hir_def::test_items::declarations(db, krate(db))
}

fn impls(db: &TestDB) -> Vec<ImplId> {
    hir_def::test_items::impls(db, krate(db))
}

fn struct_named(db: &TestDB, name: &str) -> StructId {
    declarations(db)
        .into_iter()
        .find_map(|def| match def {
            ModuleDefId::AdtId(AdtId::StructId(it))
                if StructSignature::of(db, it).name.as_str() == name =>
            {
                Some(it)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no struct `{name}`"))
}

fn trait_named(db: &TestDB, name: &str) -> TraitId {
    declarations(db)
        .into_iter()
        .find_map(|def| match def {
            ModuleDefId::TraitId(it) if TraitSignature::of(db, it).name.as_str() == name => {
                Some(it)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no trait `{name}`"))
}

fn function_named(db: &TestDB, name: &str) -> FunctionId {
    declarations(db)
        .into_iter()
        .find_map(|def| match def {
            ModuleDefId::FunctionId(it) if FunctionSignature::of(db, it).name.as_str() == name => {
                Some(it)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no function `{name}`"))
}

/// The function `name` of the first impl that has one.
fn impl_function_named(db: &TestDB, name: &str) -> FunctionId {
    impls(db)
        .into_iter()
        .find_map(|impl_| {
            impl_.impl_items(db).items.iter().find_map(|(item_name, item)| match *item {
                AssocItemId::FunctionId(it) if item_name.as_str() == name => Some(it),
                _ => None,
            })
        })
        .unwrap_or_else(|| panic!("no impl function `{name}`"))
}

/// The impl of the trait `trait_`, or the inherent impl with `None`.
fn impl_of(db: &TestDB, trait_: Option<TraitId>) -> ImplId {
    hir_ty::attach_db(db, || {
        impls(db)
            .into_iter()
            .find(|&it| db.impl_trait(it).map(|it| it.skip_binder().def_id.0) == trait_)
            .expect("an impl")
    })
}

fn infer(db: &TestDB, function: FunctionId) {
    hir_ty::attach_db(db, || {
        let _ = InferenceResult::of(db, DefWithBodyId::from(function));
    })
}

const DROP: &str = r#"
#![feature(lang_items)]
#[lang = "drop"]
pub trait Drop {
    fn drop(&mut self);
}
pub struct A;
pub struct B;
"#;

fn destructor_of(db: &TestDB, adt: AdtId) -> Option<ImplId> {
    hir_ty::attach_db(db, || destructor(db, adt))
}

#[test]
fn a_drop_impl_for_another_type_leaves_destructor() {
    let (mut db, file) = single_file(DROP);
    let a = AdtId::from(struct_named(&db, "A"));
    let cold = db.log_executed(|| assert_eq!(destructor_of(&db, a), None));
    assert!(ran(&cold, "destructor"), "{cold:#?}");

    db.set_file_text(file, &format!("{DROP}impl Drop for B {{\n    fn drop(&mut self) {{}}\n}}\n"));
    let warm = db.log_executed(|| assert_eq!(destructor_of(&db, a), None));
    assert!(ran(&warm, "::in_crate_for_trait_and_self_ty"), "{warm:#?}");
    assert!(!ran(&warm, "destructor"), "{warm:#?}");
}

#[test]
fn a_drop_impl_for_the_type_reruns_destructor() {
    let (mut db, file) = single_file(DROP);
    let a = AdtId::from(struct_named(&db, "A"));
    assert_eq!(destructor_of(&db, a), None);

    db.set_file_text(file, &format!("{DROP}impl Drop for A {{\n    fn drop(&mut self) {{}}\n}}\n"));
    let warm = db.log_executed(|| assert!(destructor_of(&db, a).is_some()));
    assert!(ran(&warm, "destructor"), "{warm:#?}");
}

const CALLER: &str = "mod a;\nfn caller(x: a::A) {\n    x.m();\n}\n";
const IMPL: &str = "pub struct A;\nimpl A {\n    pub fn m(&self) {}\n}\n";

/// A caller of `A::m` in one file and `impl A` in another, so an edit of the impl moves
/// nothing in the caller's file.
fn method_db() -> (TestDB, FileId, FunctionId) {
    let fx = fixture::db(&format!("//- /main.rs crate:main\n{CALLER}//- /a.rs\n{IMPL}"));
    let files = fx.files().to_vec();
    let db = fx.into_db();
    let impl_file = files[1].file_id(&db);
    let caller = function_named(&db, "caller");
    (db, impl_file, caller)
}

#[test]
fn a_method_of_another_name_leaves_the_lookup() {
    let (mut db, impl_file, caller) = method_db();
    let cold = db.log_executed(|| infer(&db, caller));
    assert!(ran(&cold, "infer_body_query"), "{cold:#?}");
    assert!(ran(&cold, "impl_value_item"), "{cold:#?}");

    db.set_file_text(
        impl_file,
        "pub struct A;\nimpl A {\n    pub fn m(&self) {}\n    pub fn n(&self) {}\n}\n",
    );
    let warm = db.log_executed(|| infer(&db, caller));
    assert!(ran(&warm, "impl_value_item"), "{warm:#?}");
    assert!(!ran(&warm, "infer_body_query"), "{warm:#?}");
}

#[test]
fn removing_the_method_reruns_the_lookup() {
    let (mut db, impl_file, caller) = method_db();
    infer(&db, caller);

    db.set_file_text(impl_file, "pub struct A;\nimpl A {\n    pub fn k(&self) {}\n}\n");
    let warm = db.log_executed(|| infer(&db, caller));
    assert!(ran(&warm, "infer_body_query"), "{warm:#?}");
}

const SPECIALIZING: &str = "#![feature(specialization)]
pub trait Tr {
    fn f(&self) {}
}
pub trait Other {}
pub struct S;
impl<T> Tr for T {}
impl Tr for S {}
";

fn impls_of_trait(db: &TestDB, trait_: TraitId) -> usize {
    hir_ty::attach_db(db, || TraitImpls::in_crate_and_deps_for_trait(db, krate(db), trait_).len())
}

#[test]
fn an_impl_of_another_trait_leaves_the_impls_of_a_trait() {
    let (mut db, file) = single_file(SPECIALIZING);
    let tr = trait_named(&db, "Tr");
    let cold = db.log_executed(|| assert_eq!(impls_of_trait(&db, tr), 2));
    assert!(ran(&cold, "::in_crate_and_deps_for_trait"), "{cold:#?}");

    db.set_file_text(file, &format!("{SPECIALIZING}impl Other for S {{}}\n"));
    let warm = db.log_executed(|| assert_eq!(impls_of_trait(&db, tr), 2));
    assert!(!ran(&warm, "::for_crate_and_trait"), "{warm:#?}");
    assert!(!ran(&warm, "::in_crate_for_trait"), "{warm:#?}");
    assert!(!ran(&warm, "::in_crate_and_deps_for_trait"), "{warm:#?}");
}

#[test]
fn an_impl_of_the_trait_reruns_the_impls_of_the_trait() {
    let (mut db, file) = single_file(SPECIALIZING);
    let tr = trait_named(&db, "Tr");
    assert_eq!(impls_of_trait(&db, tr), 2);

    db.set_file_text(file, &format!("{SPECIALIZING}impl Tr for u32 {{}}\n"));
    let warm = db.log_executed(|| assert_eq!(impls_of_trait(&db, tr), 3));
    assert!(ran(&warm, "::in_crate_and_deps_for_trait"), "{warm:#?}");
}

const COHERENCE: &str = "pub trait Tr {}\npub struct S;\nimpl Tr for S {}\nimpl S {}\n";

/// Whether the trait impl of `Tr` satisfies the orphan rules and the inherent impl is
/// coherent.
fn coherence(db: &TestDB) -> (bool, bool) {
    let tr = trait_named(db, "Tr");
    let trait_impl = impl_of(db, Some(tr));
    let inherent = impl_of(db, None);
    hir_ty::attach_db(db, || {
        let def_map = crate_def_map(db, krate(db));
        (check_orphan_rules(db, trait_impl), is_inherent_impl_coherent(db, def_map, inherent))
    })
}

#[test]
fn an_unrelated_item_leaves_coherence() {
    let (mut db, file) = single_file(COHERENCE);
    let cold = db.log_executed(|| assert_eq!(coherence(&db), (true, true)));
    assert!(ran(&cold, "orphan_rules_hold"), "{cold:#?}");
    assert!(ran(&cold, "inherent_impl_coherent"), "{cold:#?}");

    db.set_file_text(file, &format!("{COHERENCE}pub fn unrelated() {{}}\n"));
    let warm = db.log_executed(|| assert_eq!(coherence(&db), (true, true)));
    assert!(!ran(&warm, "orphan_rules_hold"), "{warm:#?}");
    assert!(!ran(&warm, "inherent_impl_coherent"), "{warm:#?}");
}

#[test]
fn changing_the_impls_reruns_coherence() {
    let (mut db, file) = single_file(COHERENCE);
    assert_eq!(coherence(&db), (true, true));

    db.set_file_text(file, "pub trait Tr {}\npub struct S;\nimpl Tr for u32 {}\nimpl u32 {}\n");
    let warm = db.log_executed(|| assert_eq!(coherence(&db), (true, false)));
    assert!(ran(&warm, "orphan_rules_hold"), "{warm:#?}");
    assert!(ran(&warm, "inherent_impl_coherent"), "{warm:#?}");
}

const LAYOUT: &str = "pub struct S {\n    a: u32,\n    b: u8,\n}\n";

fn size_of_struct(db: &TestDB, name: &str) -> u64 {
    let adt = AdtId::from(struct_named(db, name));
    hir_ty::attach_db(db, || {
        let interner = DbInterner::new_no_crate(db);
        let args = GenericArgs::identity_for_item(interner, adt.into());
        let env = ParamEnvAndCrate {
            param_env: db.trait_environment(GenericDefId::AdtId(adt)),
            krate: krate(db),
        };
        layout_of_adt_query(db, adt, args.store(), env.store()).expect("a layout").size.bytes()
    })
}

#[test]
fn renaming_a_field_leaves_the_layout() {
    let (mut db, file) = single_file(LAYOUT);
    let cold = db.log_executed(|| assert_eq!(size_of_struct(&db, "S"), 8));
    assert!(ran(&cold, "layout_of_adt_query"), "{cold:#?}");

    db.set_file_text(file, "pub struct S {\n    c: u32,\n    b: u8,\n}\n");
    let warm = db.log_executed(|| assert_eq!(size_of_struct(&db, "S"), 8));
    assert!(ran(&warm, "variant_field_ids"), "{warm:#?}");
    assert!(!ran(&warm, "layout_of_adt_query"), "{warm:#?}");
}

#[test]
fn changing_a_field_type_reruns_the_layout() {
    let (mut db, file) = single_file(LAYOUT);
    assert_eq!(size_of_struct(&db, "S"), 8);

    db.set_file_text(file, "pub struct S {\n    a: u32,\n    b: u64,\n}\n");
    let warm = db.log_executed(|| assert_eq!(size_of_struct(&db, "S"), 16));
    assert!(ran(&warm, "layout_of_adt_query"), "{warm:#?}");
}

const LAYOUT_IN_ENVS: &str = "pub struct S {
    a: u32,
    b: u8,
}
pub trait A {}
pub trait B {}
pub fn g<T: A>(_: T) {}
pub fn h<U: B>(_: U) {}
";

/// The size of the struct `name` asked under the environment of `function`.
fn size_under(db: &TestDB, name: &str, function: &str) -> u64 {
    let adt = AdtId::from(struct_named(db, name));
    let function = function_named(db, function);
    hir_ty::attach_db(db, || {
        let interner = DbInterner::new_no_crate(db);
        let ty = Ty::new_adt(interner, adt, GenericArgs::identity_for_item(interner, adt.into()));
        let env = ParamEnvAndCrate {
            param_env: db.trait_environment(GenericDefId::FunctionId(function)),
            krate: krate(db),
        };
        db.layout_of_ty(ty.store(), env.store()).expect("a layout").get().size.bytes()
    })
}

#[test]
fn a_type_without_parameters_is_laid_out_once_for_every_environment() {
    let (db, _) = single_file(LAYOUT_IN_ENVS);
    let cold = db.log_executed(|| assert_eq!(size_under(&db, "S", "g"), 8));
    assert!(ran(&cold, "layout_of_adt_query"), "{cold:#?}");

    let warm = db.log_executed(|| assert_eq!(size_under(&db, "S", "h"), 8));
    assert!(ran(&warm, "layout_of_ty_query"), "{warm:#?}");
    assert!(!ran(&warm, "layout_of_adt_query"), "{warm:#?}");
}

const WARM: &str = "pub trait Tr {}
pub struct S {
    a: u32,
    b: u8,
}
impl Tr for S {}
impl S {}
pub fn g<T: Copy>(_: T) {}
pub fn r() -> impl Tr {
    S { a: 0, b: 0 }
}
";

/// What a body's inference and a fact's value ask of items once warm, each a memo hit that
/// allocates nothing: an ADT's layout under its own environment and a type's under a
/// function's, the ADT's destructor, both impls' coherence and a function's hidden types.
#[test]
fn warm_layouts_destructors_coherence_and_hidden_types_allocate_nothing() {
    assert!(blaze_alloc_count::installed());
    let (db, _) = single_file(WARM);
    let adt = AdtId::from(struct_named(&db, "S"));
    let g = function_named(&db, "g");
    let r = function_named(&db, "r");
    let tr = trait_named(&db, "Tr");
    let (trait_impl, inherent) = (impl_of(&db, Some(tr)), impl_of(&db, None));
    let krate = krate(&db);
    hir_ty::attach_db(&db, || {
        let db = &db;
        let interner = DbInterner::new_no_crate(db);
        let args = GenericArgs::identity_for_item(interner, adt.into());
        let ty = Ty::new_adt(interner, adt, args);
        let own = ParamEnvAndCrate {
            param_env: db.trait_environment(GenericDefId::AdtId(adt)),
            krate,
        };
        let under_g = ParamEnvAndCrate {
            param_env: db.trait_environment(GenericDefId::FunctionId(g)),
            krate,
        };
        let def_map = crate_def_map(db, krate);
        let ask = || {
            let of_adt = layout_of_adt_query(db, adt, args.store(), own.store()).expect("a layout");
            let of_ty = db.layout_of_ty(ty.store(), under_g.store()).expect("a layout");
            std::hint::black_box(hir_ty::rpit_hidden_types(db, r));
            (
                of_adt.size.bytes(),
                of_ty.get().size.bytes(),
                destructor(db, adt),
                check_orphan_rules(db, trait_impl),
                is_inherent_impl_coherent(db, def_map, inherent),
            )
        };
        let warm = ask();
        assert_eq!(warm, (8, 8, None, true, true));
        let again = blaze_alloc_count::assert_at_most(
            0,
            "warm layouts, a destructor, coherence and hidden types",
            || ask(),
        );
        assert_eq!(again, warm);
    });
}

const ATPIT: &str = "pub trait Tr {
    fn m(&self) -> u32;
}
pub struct A;
impl Tr for A {
    fn m(&self) -> u32 {
        1
    }
";

#[test]
fn a_constant_added_to_the_impl_leaves_its_bodies() {
    let (mut db, file) = single_file(&format!("{ATPIT}}}\n"));
    let m = impl_function_named(&db, "m");
    let cold = db.log_executed(|| infer(&db, m));
    assert!(ran(&cold, "infer_body_query"), "{cold:#?}");
    assert!(ran(&cold, "impl_type_aliases"), "{cold:#?}");

    db.set_file_text(file, &format!("{ATPIT}    const C: u32 = 0;\n}}\n"));
    let m = impl_function_named(&db, "m");
    let warm = db.log_executed(|| infer(&db, m));
    assert!(ran(&warm, "impl_type_aliases"), "{warm:#?}");
    assert!(!ran(&warm, "infer_body_query"), "{warm:#?}");
}

#[test]
fn an_associated_type_added_to_the_impl_reruns_its_bodies() {
    let (mut db, file) = single_file(&format!("{ATPIT}}}\n"));
    let m = impl_function_named(&db, "m");
    infer(&db, m);

    db.set_file_text(file, &format!("{ATPIT}    type X = u32;\n}}\n"));
    let m = impl_function_named(&db, "m");
    let warm = db.log_executed(|| infer(&db, m));
    assert!(ran(&warm, "infer_body_query"), "{warm:#?}");
}

const TABLES: &str = r#"
pub trait T {}
pub trait U {}
pub struct A<X>(X);
pub struct B;
impl A<u8> { pub fn a1() {} }
impl B { pub fn b() {} }
impl A<u16> { pub fn a2() {} }
impl T for A<u8> {}
impl T for B {}
impl T for A<u16> {}
impl<X> T for [X] {}
impl<X> U for X {}
"#;

/// The impl tables, as columns, give each self type its impls in the order they were found,
/// a trait's blanket impls apart, and nothing for a type with none.
#[test]
fn impl_tables_keep_the_order_impls_are_found_in() {
    let (db, _) = single_file(TABLES);
    let k = krate(&db);
    let a = SimplifiedType::Adt(AdtId::from(struct_named(&db, "A")).into());
    let b = SimplifiedType::Adt(AdtId::from(struct_named(&db, "B")).into());
    let (t, u) = (trait_named(&db, "T"), trait_named(&db, "U"));
    hir_ty::attach_db(&db, || {
        let trait_of = |it: ImplId| db.impl_trait(it).map(|it| it.skip_binder().def_id.0);
        let all = impls(&db);
        let of = |trait_: Option<TraitId>| {
            all.iter().copied().filter(|&it| trait_of(it) == trait_).collect::<Vec<_>>()
        };
        let (inherent, of_t, of_u) = (of(None), of(Some(t)), of(Some(u)));

        let inherent_impls = InherentImpls::for_crate(&db, k);
        assert_eq!(inherent_impls.for_self_ty(&a), [inherent[0], inherent[2]]);
        assert_eq!(inherent_impls.for_self_ty(&b), [inherent[1]]);
        assert!(inherent_impls.for_self_ty(&SimplifiedType::Str).is_empty());

        let trait_impls = &**TraitImpls::for_crate(&db, k);
        assert_eq!(trait_impls.for_trait_and_self_ty(t, &a).0, [of_t[0], of_t[2]]);
        assert_eq!(trait_impls.for_trait_and_self_ty(t, &b).0, [of_t[1]]);
        assert_eq!(trait_impls.for_trait_and_self_ty(t, &SimplifiedType::Slice).0, [of_t[3]]);
        assert!(trait_impls.for_trait_and_self_ty(u, &a).0.is_empty());
        assert!(trait_impls.blanket_impls(t).is_empty());
        assert_eq!(trait_impls.blanket_impls(u), of_u);
        assert!(trait_impls.has_impls_for_trait_and_self_ty(u, &SimplifiedType::Str));
        assert!(!trait_impls.has_impls_for_trait_and_self_ty(t, &SimplifiedType::Str));

        let mut found = Vec::new();
        trait_impls.for_trait(t, |it| {
            if let Either::Left(it) = it {
                found.extend_from_slice(it);
            }
        });
        found.sort();
        let mut expected = of_t.clone();
        expected.sort();
        assert_eq!(found, expected);
    });
}

const MODULES: &str = "mod a;\nmod b;\npub trait T {}\npub trait U {}\n";
const MODULE_A: &str = "pub struct A;\nimpl A {}\nimpl crate::T for A {}\n";
const MODULE_B: &str = "pub struct B;\nimpl B {}\nimpl crate::T for B {}\nimpl crate::U for B {}\n";

/// A crate of two modules with impls, and the files of `a` and `b`.
fn modules_db() -> (TestDB, FileId, FileId) {
    let fx = fixture::db(&format!(
        "//- /main.rs crate:main\n{MODULES}//- /a.rs\n{MODULE_A}//- /b.rs\n{MODULE_B}"
    ));
    let files = fx.files().to_vec();
    let db = fx.into_db();
    let (a, b) = (files[1].file_id(&db), files[2].file_id(&db));
    (db, a, b)
}

/// The buckets built module by module hold what the crate's tables hold, in the same order.
#[test]
fn per_module_buckets_are_the_crate_tables() {
    let (db, _, _) = modules_db();
    let k = krate(&db);
    let a = SimplifiedType::Adt(AdtId::from(struct_named(&db, "A")).into());
    let b = SimplifiedType::Adt(AdtId::from(struct_named(&db, "B")).into());
    let (t, u) = (trait_named(&db, "T"), trait_named(&db, "U"));
    hir_ty::attach_db(&db, || {
        let whole = &**TraitImpls::for_crate(&db, k);
        let inherent = InherentImpls::for_crate(&db, k);
        for trait_ in [t, u] {
            let bucket = TraitImpls::for_crate_and_trait(&db, k, trait_);
            for self_ty in [a, b, SimplifiedType::Str] {
                let expected = whole.for_trait_and_self_ty(trait_, &self_ty);
                assert_eq!(bucket.for_trait_and_self_ty(trait_, &self_ty), expected);
                let narrow =
                    TraitImpls::in_crate_for_trait_and_self_ty(&db, k, trait_, SelfTyKey(self_ty));
                assert_eq!(narrow, expected.0);
            }
            assert_eq!(bucket.blanket_impls(trait_), whole.blanket_impls(trait_));
        }
        for self_ty in [a, b, SimplifiedType::Str] {
            let narrow = InherentImpls::in_crate_for_self_ty(&db, k, SelfTyKey(self_ty));
            assert_eq!(narrow, inherent.for_self_ty(&self_ty));
        }
        assert_eq!(whole.for_trait_and_self_ty(t, &a).0.len(), 1);
        assert_eq!(whole.for_trait_and_self_ty(u, &b).0.len(), 1);
        assert_eq!(inherent.for_self_ty(&b).len(), 1);
    });
}

/// An impl of `U` added to `a` reruns `U`'s bucket and what reads it; `T`'s bucket, the
/// inherent ones and every module's body tables are left unexecuted: which modules' bodies hold
/// impls is the impls maintainer's, read off the slices.
#[test]
fn an_impl_in_one_module_reruns_only_its_trait_bucket() {
    let (mut db, a_file, _) = modules_db();
    let k = krate(&db);
    let (t, u) = (trait_named(&db, "T"), trait_named(&db, "U"));
    let b = SimplifiedType::Adt(AdtId::from(struct_named(&db, "B")).into());
    let read = |db: &TestDB| {
        hir_ty::attach_db(db, || {
            (
                TraitImpls::for_crate_and_trait(db, k, t).blanket_impls(t).len(),
                TraitImpls::in_crate_for_trait(db, k, u).len(),
                InherentImpls::in_crate_for_self_ty(db, k, SelfTyKey(b)).len(),
            )
        })
    };
    assert_eq!(read(&db), (0, 1, 1));

    db.set_file_text(a_file, &format!("{MODULE_A}impl crate::U for A {{}}\n"));
    let warm = db.log_executed(|| assert_eq!(read(&db), (0, 2, 1)));
    for query in ["trait_bodies", "inherent_bodies", "body_part"] {
        assert!(!ran(&warm, query), "{query} ran: {warm:#?}");
    }
    assert!(ran(&warm, "::in_crate_for_trait"), "{warm:#?}");
    let buckets = warm.iter().filter(|it| it.contains("::for_crate_and_trait")).count();
    assert_eq!(buckets, 1, "only `U`'s bucket reruns: {warm:#?}");
    assert!(!ran(&warm, "::in_crate_for_self_ty"), "{warm:#?}");
}

/// Adding a function to `a` leaves every bucket and both crate tables unexecuted.
#[test]
fn an_item_that_is_no_impl_reruns_no_bucket() {
    let (mut db, a_file, _) = modules_db();
    let k = krate(&db);
    let t = trait_named(&db, "T");
    let a = SimplifiedType::Adt(AdtId::from(struct_named(&db, "A")).into());
    let read = |db: &TestDB| {
        hir_ty::attach_db(db, || {
            _ = TraitImpls::for_crate(db, k);
            _ = InherentImpls::for_crate(db, k);
            (
                TraitImpls::in_crate_for_trait_and_self_ty(db, k, t, SelfTyKey(a)).len(),
                InherentImpls::in_crate_for_self_ty(db, k, SelfTyKey(a)).len(),
            )
        })
    };
    assert_eq!(read(&db), (1, 1));

    db.set_file_text(a_file, &format!("{MODULE_A}pub fn added() {{}}\n"));
    let warm = db.log_executed(|| assert_eq!(read(&db), (1, 1)));
    for query in [
        "::for_crate",
        "::for_crate_and_trait",
        "trait_bodies",
        "inherent_bodies",
        "body_part",
        "::in_crate_for_trait_and_self_ty",
        "::in_crate_for_self_ty",
    ] {
        assert!(!ran(&warm, query), "{query} reran: {warm:#?}");
    }
}

const AUTO: &str = r#"
#![feature(auto_traits, negative_impls)]
pub trait T {}
pub auto trait Au {}
pub struct A;
pub struct B;
pub struct C<X>(X);
impl T for A {}
impl !Au for B {}
impl<X: T> T for C<X> {}
"#;

/// The checks of `implements_traits_unique` share an inference context, one probe each: the
/// answers are those of one `implements_trait_unique` at a time, a trait asked twice included.
#[test]
fn implements_traits_unique_answers_as_one_at_a_time() {
    let (db, _) = single_file(AUTO);
    let traits = [trait_named(&db, "T"), trait_named(&db, "Au"), trait_named(&db, "T")];
    for name in ["A", "B", "C"] {
        let s = struct_named(&db, name);
        hir_ty::attach_db(&db, || {
            let ty = db.ty(s.into()).instantiate_identity().skip_norm_wip();
            let env = ParamEnvAndCrate {
                param_env: db.trait_environment(GenericDefId::AdtId(s.into())),
                krate: krate(&db),
            };
            let one_at_a_time: Vec<bool> =
                traits.iter().map(|&it| implements_trait_unique(ty, &db, env, it)).collect();
            let mut together = Vec::new();
            implements_traits_unique(ty, &db, env, &traits, |_, holds| together.push(holds));
            assert_eq!(together, one_at_a_time, "{name}");
            match name {
                "A" => assert_eq!(one_at_a_time, [true, true, true]),
                "B" => assert_eq!(one_at_a_time, [false, false, false]),
                _ => {}
            }
        });
    }
}

const DEP_BODIES: &str = r#"
//- proc_macros: mirror
//- /main.rs crate:main deps:dep
//- /dep.rs crate:dep library
macro_rules! quiet { ($($t:tt)*) => {}; }
macro_rules! mirror { ($($t:tt)*) => {}; }
pub fn plain() { let _ = 1; }
pub fn calls_inert() { quiet!(x); }
pub fn allowed() { #[allow(unused)] let x = 1; }
pub fn calls_proc_macro() { mirror!(struct S;); }
pub fn derives() { #[derive(Clone)] struct X; }
"#;

fn crate_named(db: &TestDB, name: &str) -> Crate {
    *all_crates(db)
        .iter()
        .find(|it| {
            let display = &it.extra_data(db).display_name;
            display.as_ref().is_some_and(|it| it.crate_name().as_str() == name)
        })
        .unwrap_or_else(|| panic!("no crate `{name}`"))
}

/// The functions of `krate` whose bodies are lowered to find impls in them.
fn holders(db: &TestDB, krate: Crate) -> Vec<String> {
    hir_ty::attach_db(db, || {
        impl_work(db, krate)
            .into_iter()
            .filter_map(|it| match it {
                ImplWork::Holder(DefWithBodyId::FunctionId(it)) => {
                    Some(FunctionSignature::of(db, it).name.as_str().to_owned())
                }
                _ => None,
            })
            .collect()
    })
}

/// A dependency's function holds an impl rustc sees when a procedural macro is called in it
/// (`mirror!`, though a `macro_rules!` of the name writes none) or an item in it has a
/// `#[derive]`, with no `impl` in its text. A call of a macro proven to write none, or an
/// inert attribute, leaves the body unlowered.
#[test]
fn a_dependency_body_with_a_proc_macro_call_or_a_derive_holds_impls() {
    let fx = fixture::db(DEP_BODIES);
    let _ = fx.files().to_vec();
    let db = fx.into_db();
    let held = holders(&db, crate_named(&db, "dep"));
    for holder in ["calls_proc_macro", "derives"] {
        assert!(held.iter().any(|it| it == holder), "{holder}: {held:?}");
    }
    for inert in ["plain", "calls_inert", "allowed"] {
        assert!(!held.iter().any(|it| it == inert), "{inert}: {held:?}");
    }
}

const LOCAL_BODIES: &str = r#"
//- /main.rs crate:main
pub trait Tr {}
macro_rules! quiet { ($($t:tt)*) => {}; }
macro_rules! writes { ($t:ident) => { impl Tr for $t {} }; }
fn plain() { let _ = 1; }
fn calls_inert() { quiet!(x); }
fn allowed() { #[allow(unused)] let x = 1; }
fn holds() { struct A; impl Tr for A {} }
fn calls_writer() { struct B; writes!(B); }
fn derives() { #[derive(Clone)] struct C; }
fn calls_unknown() { unknown!(struct D;); }
fn bracketed() -> [u8; { 4 }] { struct E; impl Tr for E {} [0; 4] }
pub struct Wrap<const N: usize>;
fn const_argument() -> Wrap<{ 4 }> { struct F; impl Tr for F {} Wrap }
"#;

/// A workspace crate's bodies are looked into as a dependency's are: a body whose text has
/// an `impl`, a call of a macro that may write one or an attribute that is not inert holds
/// impls, and the rest are not lowered for them. A block in the signature (`[u8; { 4 }]`,
/// `Wrap<{ 4 }>`) is not taken for the body. Every impl in a body is still found.
#[test]
fn a_local_body_is_lowered_for_impls_only_when_it_may_hold_one() {
    let fx = fixture::db(LOCAL_BODIES);
    let _ = fx.files().to_vec();
    let db = fx.into_db();
    let main = crate_named(&db, "main");
    let held = holders(&db, main);
    let holding =
        ["holds", "calls_writer", "derives", "calls_unknown", "bracketed", "const_argument"];
    for holder in holding {
        assert!(held.iter().any(|it| it == holder), "{holder}: {held:?}");
    }
    for inert in ["plain", "calls_inert", "allowed"] {
        assert!(!held.iter().any(|it| it == inert), "{inert}: {held:?}");
    }
    let tr = crate_def_map(&db, main)
        .modules(&db)
        .flat_map(|(_, module)| module.scope(&db).declarations().collect::<Vec<_>>())
        .find_map(|def| match def {
            ModuleDefId::TraitId(it) => Some(it),
            _ => None,
        })
        .expect("trait `Tr`");
    let found = hir_ty::attach_db(&db, || TraitImpls::in_crate_for_trait(&db, main, tr).len());
    // `A`, `B` (through `writes!`), `E` and `F`.
    assert_eq!(found, 4);
}

const TRAIT_ITEMS: &str = r#"
//- /main.rs crate:main deps:dep,other
mod inner;
pub trait Shared { fn a(&self); fn b(&self) {} const C: u8; type T; }
pub trait Twice { fn a(&self); fn x(&self); fn a(&self, _: u8); }
macro_rules! make { ($n:ident) => { pub trait $n { fn a(&self); fn made(&self); } }; }
make!(Made);
fn blocky() {
    trait Local { fn a(&self); fn local(&self); }
    { trait Nested { fn b(&self); const C: u8; } }
}
//- /inner.rs
pub trait InModule { fn b(&self); fn c(&self); }
//- /dep.rs crate:dep library
pub trait Shared { fn a(&self); fn z(&self); }
pub trait Other { const A: u8; }
//- /other.rs crate:other library
pub trait Empty {}
pub trait OnlyTypes { type a; }
"#;

/// The traits of `def_map`'s modules and of the blocks in its functions' bodies.
fn traits_in(db: &TestDB, def_map: &DefMap, traits: &mut Vec<TraitId>) {
    for (_, module) in def_map.modules(db) {
        for def in module.scope(db).declarations() {
            match def {
                ModuleDefId::TraitId(it) => traits.push(it),
                ModuleDefId::FunctionId(it) => {
                    for (_, block) in Body::of(db, DefWithBodyId::from(it)).blocks(db) {
                        traits_in(db, block, traits);
                    }
                }
                _ => {}
            }
        }
    }
}

/// The per-crate name index gives each trait, in crates and in blocks, the item
/// `trait_value_item` gives it alone, for every name its items have and one they don't, with
/// one index asked of every trait in turn as a probe asks.
#[test]
fn the_trait_name_index_answers_as_each_trait_alone() {
    let fx = fixture::db(TRAIT_ITEMS);
    let _ = fx.files().to_vec();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let mut traits = Vec::new();
        for name in ["main", "dep", "other"] {
            traits_in(&db, crate_def_map(&db, crate_named(&db, name)), &mut traits);
        }
        assert_eq!(traits.len(), 10, "{traits:?}");
        let mut names: Vec<Name> = traits
            .iter()
            .flat_map(|it| it.trait_items(&db).items.iter().map(|&(name, _)| name))
            .collect();
        names.push(Name::new_root("missing"));
        names.sort();
        names.dedup();
        for name in names {
            blaze_mem::frame::task(blaze_mem::Tag::Infer, |frame| {
                let mut index = TraitValueItems::new_in(frame);
                for &trait_ in traits.iter().chain(traits.iter().rev()) {
                    let alone = trait_value_item(&db, trait_, name);
                    assert_eq!(index.get(&db, trait_, name), alone, "{trait_:?} {name:?}");
                }
            });
        }
    });
}

const TRAIT_CALLER: &str = "mod a;\nuse a::Tr;\nfn caller(x: a::A) {\n    x.m();\n}\n";
const TRAIT_DEF: &str =
    "pub struct A;\npub trait Tr {\n    fn m(&self);\n}\nimpl Tr for A {\n    fn m(&self) {}\n}\n";

fn trait_method_db() -> (TestDB, FileId, FunctionId) {
    let fx = fixture::db(&format!(
        "//- /main.rs crate:main\n{TRAIT_CALLER}//- /a.rs\n{TRAIT_DEF}"
    ));
    let files = fx.files().to_vec();
    let db = fx.into_db();
    let trait_file = files[1].file_id(&db);
    let caller = function_named(&db, "caller");
    (db, trait_file, caller)
}

#[test]
fn a_trait_method_of_another_name_leaves_the_lookup() {
    let (mut db, trait_file, caller) = trait_method_db();
    let cold = db.log_executed(|| infer(&db, caller));
    assert!(ran(&cold, "in_crate_trait_value_items"), "{cold:#?}");

    let added = TRAIT_DEF.replace("fn m(&self);", "fn m(&self);\n    fn n(&self) {}");
    db.set_file_text(trait_file, &added);
    let warm = db.log_executed(|| infer(&db, caller));
    assert!(!ran(&warm, "in_crate_trait_value_items"), "{warm:#?}");
    assert!(!ran(&warm, "infer_body_query"), "{warm:#?}");
}

#[test]
fn removing_the_trait_method_reruns_the_lookup() {
    let (mut db, trait_file, caller) = trait_method_db();
    infer(&db, caller);

    db.set_file_text(trait_file, &TRAIT_DEF.replace("fn m(&self);", "fn k(&self);"));
    let warm = db.log_executed(|| infer(&db, caller));
    assert!(ran(&warm, "infer_body_query"), "{warm:#?}");
}

const SPECIALIZED_CHAIN: &str = r#"//- /dep.rs crate:dep
#![feature(specialization)]
pub trait Tr { type A; }
impl<T> Tr for T { default type A = u16; }
pub struct Other<T>(T);
impl<T> Tr for Other<T> { default type A = u32; }
//- /main.rs crate:main deps:dep
#![feature(specialization)]
use dep::Tr;
pub struct W<T>(T);
impl<T> Tr for W<T> { default type A = u8; }
impl Tr for W<u8> {}
pub fn closest(x: <W<u8> as Tr>::A) -> u8 { x }
pub fn blanket(x: <W<u8> as Tr>::A) -> u16 { x }
"#;

/// The impl a specializing impl takes an item from is among the blanket impls and the impls
/// for its own simplified self type: `impl Tr for W<u8>` takes `A` from `impl<T> Tr for W<T>`,
/// the closest, not from the blanket impl of another crate it also specializes.
#[test]
fn a_specializing_impl_takes_the_item_of_its_closest_ancestor_of_its_self_type() {
    let fx = fixture::db(SPECIALIZED_CHAIN);
    let _ = fx.files().to_vec();
    let db = fx.into_db();
    let main = crate_named(&db, "main");
    let mismatches = |name: &str| {
        let function = crate_def_map(&db, main)
            .modules(&db)
            .flat_map(|(_, module)| module.scope(&db).declarations().collect::<Vec<_>>())
            .find_map(|def| match def {
                ModuleDefId::FunctionId(it)
                    if FunctionSignature::of(&db, it).name.as_str() == name =>
                {
                    Some(it)
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("no function `{name}`"));
        hir_ty::attach_db(&db, || {
            InferenceResult::of(&db, DefWithBodyId::from(function)).has_type_mismatches()
        })
    };
    assert!(!mismatches("closest"), "`<W<u8> as Tr>::A` is `u8`");
    assert!(mismatches("blanket"), "`<W<u8> as Tr>::A` is no `u16`");
}
