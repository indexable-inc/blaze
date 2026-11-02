//! Rules of `impl Interner for DbInterner` and of its `GenericsOf` that read items, so only a
//! database can exercise them.

use hir_def::{
    AdtId, AssocItemId, DefWithBodyId, FunctionId, GenericDefId, ImplId, LifetimeParamId,
    ModuleDefId, TypeAliasId,
    expr_store::Body,
    hir::generics::GenericParams,
    item_scope::ItemScope,
    signatures::{
        ConstSignature, EnumSignature, FunctionSignature, StaticSignature, StructSignature,
        TraitSignature,
    },
};
use intern::sym;
use rustc_type_ir::{
    Interner as _,
    inherent::{GenericsOf as _, IntoKind as _},
};
use hir_expand::fixture;

use crate::{
    InferenceResult,
    db::AnonConstId,
    display::{DisplayTarget, HirDisplay},
    next_solver::{
        AliasTermKind, BoundRegionKind, DbInterner, EarlyParamRegion, GenericArg, GenericArgs,
        InherentAssocTyId, SolverDefId, Symbol, TraitAssocTyId, Ty, TyKind,
    },
    test_db::{TestDB, TestDBExt as _},
};

/// The item of the module scope declared as `name`.
#[track_caller]
fn declared(db: &TestDB, scope: &ItemScope, name: &str) -> ModuleDefId {
    scope
        .declarations()
        .find(|&decl| match decl {
            ModuleDefId::AdtId(AdtId::StructId(it)) => StructSignature::of(db, it).name.as_str() == name,
            ModuleDefId::AdtId(AdtId::EnumId(it)) => EnumSignature::of(db, it).name.as_str() == name,
            ModuleDefId::FunctionId(it) => FunctionSignature::of(db, it).name.as_str() == name,
            ModuleDefId::ConstId(it) => {
                ConstSignature::of(db, it).name.as_ref().is_some_and(|it| it.as_str() == name)
            }
            ModuleDefId::StaticId(it) => StaticSignature::of(db, it).name.as_str() == name,
            ModuleDefId::TraitId(it) => TraitSignature::of(db, it).name.as_str() == name,
            _ => false,
        })
        .unwrap_or_else(|| panic!("no item `{name}` in the fixture"))
}

/// The impl of the module scope with an item named `item`.
#[track_caller]
fn impl_with(db: &TestDB, scope: &ItemScope, item: &str) -> ImplId {
    scope
        .impls()
        .find(|impl_| impl_.impl_items(db).items.iter().any(|(name, _)| name.as_str() == item))
        .unwrap_or_else(|| panic!("no impl with an item `{item}` in the fixture"))
}

#[track_caller]
fn impl_fn(db: &TestDB, impl_: ImplId, name: &str) -> FunctionId {
    impl_
        .impl_items(db)
        .items
        .iter()
        .find_map(|(item_name, item)| match *item {
            AssocItemId::FunctionId(it) if item_name.as_str() == name => Some(it),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no function `{name}` in the impl"))
}

#[track_caller]
fn impl_type(db: &TestDB, impl_: ImplId, name: &str) -> TypeAliasId {
    impl_
        .impl_items(db)
        .items
        .iter()
        .find_map(|(item_name, item)| match *item {
            AssocItemId::TypeAliasId(it) if item_name.as_str() == name => Some(it),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no type `{name}` in the impl"))
}

/// The one lifetime parameter `def` declares, which the fixture makes early-bound.
#[track_caller]
fn only_lifetime(db: &TestDB, def: GenericDefId) -> LifetimeParamId {
    let mut lifetimes = GenericParams::of(db, def).iter_lt();
    let (local_id, data) = lifetimes.next().expect("a lifetime parameter");
    assert!(data.is_early_bound(), "`{}` should be early-bound", data.name.as_str());
    assert!(lifetimes.next().is_none(), "one lifetime parameter");
    LifetimeParamId { parent: def, local_id }
}

const REGION_PARAMS: &str = r#"
struct S<'a>(&'a u8);
fn f<'a, T: 'a>() {}
impl<'a> S<'a> {
    fn g<'b>() where 'a: 'b {}
}
"#;

#[test]
fn param_region_def_id_is_the_declaring_item() {
    let fx = fixture::db(REGION_PARAMS);
    let file_id = fx.only_file();
    let db = fx.into_db();

    crate::attach_db(&db, || {
        let module_id = db.module_for_file(file_id.file_id(&db));
        let def_map = module_id.def_map(&db);
        let scope = &def_map.module(&db, module_id).scope(&db);
        let interner = DbInterner::new_with(&db, module_id.krate(&db));

        let ModuleDefId::FunctionId(f) = declared(&db, scope, "f") else {
            panic!("`f` should be a function");
        };
        let impl_ = impl_with(&db, scope, "g");
        let g = impl_fn(&db, impl_, "g");

        let f_a = EarlyParamRegion { id: only_lifetime(&db, f.into()), index: 0 };
        assert_eq!(
            interner.generics_of(f.into()).param_region_def_id(interner, f_a),
            SolverDefId::FunctionId(f),
        );

        let impl_a = EarlyParamRegion { id: only_lifetime(&db, impl_.into()), index: 0 };
        assert_eq!(
            interner.generics_of(impl_.into()).param_region_def_id(interner, impl_a),
            SolverDefId::ImplId(impl_),
        );
        // `g`'s generics are the impl's `'a` and then its own `'b`.
        assert_eq!(
            interner.generics_of(g.into()).param_region_def_id(interner, impl_a),
            SolverDefId::ImplId(impl_),
        );
        let g_b = EarlyParamRegion { id: only_lifetime(&db, g.into()), index: 1 };
        assert_eq!(
            interner.generics_of(g.into()).param_region_def_id(interner, g_b),
            SolverDefId::FunctionId(g),
        );
    });
}

#[test]
#[should_panic(expected = "expected lifetime parameter")]
fn param_region_def_id_at_a_type_parameter_panics() {
    let fx = fixture::db(REGION_PARAMS);
    let file_id = fx.only_file();
    let db = fx.into_db();

    crate::attach_db(&db, || {
        let module_id = db.module_for_file(file_id.file_id(&db));
        let def_map = module_id.def_map(&db);
        let scope = &def_map.module(&db, module_id).scope(&db);
        let interner = DbInterner::new_with(&db, module_id.krate(&db));

        let ModuleDefId::FunctionId(f) = declared(&db, scope, "f") else {
            panic!("`f` should be a function");
        };
        // Index 1 of `f<'a, T>` is `T`.
        let at_t = EarlyParamRegion { id: only_lifetime(&db, f.into()), index: 1 };
        interner.generics_of(f.into()).param_region_def_id(interner, at_t);
    });
}

#[test]
fn item_name_is_the_declared_name_or_anonymous() {
    let fx = fixture::db(
        r#"
struct S;
fn f() {
    let c = || ();
}
const C: u8 = 0;
trait Tr {}
enum E { V }
impl S {
    fn m() {}
}
struct N<const K: u32>;
fn h(_: N<{ 1 + 1 }>) {}
"#,
    );
    let file_id = fx.only_file();
    let db = fx.into_db();

    crate::attach_db(&db, || {
        let module_id = db.module_for_file(file_id.file_id(&db));
        let def_map = module_id.def_map(&db);
        let scope = &def_map.module(&db, module_id).scope(&db);
        let interner = DbInterner::new_with(&db, module_id.krate(&db));

        let ModuleDefId::AdtId(s) = declared(&db, scope, "S") else {
            panic!("`S` should be a struct");
        };
        let ModuleDefId::FunctionId(f) = declared(&db, scope, "f") else {
            panic!("`f` should be a function");
        };
        let ModuleDefId::ConstId(c) = declared(&db, scope, "C") else {
            panic!("`C` should be a const");
        };
        let ModuleDefId::TraitId(tr) = declared(&db, scope, "Tr") else {
            panic!("`Tr` should be a trait");
        };
        let ModuleDefId::AdtId(AdtId::EnumId(e)) = declared(&db, scope, "E") else {
            panic!("`E` should be an enum");
        };
        let variant = e.enum_variants(&db).variants.values().next().expect("a variant").0;

        let named: [(SolverDefId<'_>, &str); 5] =
            [(s.into(), "S"), (f.into(), "f"), (c.into(), "C"), (tr.into(), "Tr"), (variant.into(), "V")];
        for (def, name) in named {
            let item_name = interner.item_name(def);
            assert_eq!(item_name.0.as_str(), name, "the name of {def:?}");
            assert_eq!(BoundRegionKind::Named(def).get_name(interner), Some(item_name));
        }

        let body = Body::of(&db, f.into());
        let (binding, _) = body
            .bindings()
            .find(|(_, binding)| binding.name.as_str() == "c")
            .expect("a binding `c` in `f`");
        let infer = InferenceResult::of(&db, DefWithBodyId::from(f));
        let TyKind::Closure(closure, _) = infer.type_of_binding[binding].as_ref().kind() else {
            panic!("`c` should be a closure");
        };
        let ModuleDefId::FunctionId(h) = declared(&db, scope, "h") else {
            panic!("`h` should be a function");
        };
        let anon_const = AnonConstId::all_from_signature(&db, h.into())
            .next()
            .expect("the anonymous const `{ 1 + 1 }` in `h`'s signature");

        let anonymous: [SolverDefId<'_>; 3] =
            [impl_with(&db, scope, "m").into(), closure.into(), anon_const.into()];
        for def in anonymous {
            assert_eq!(interner.item_name(def), Symbol(sym::tick_underscore), "the name of {def:?}");
            assert_eq!(BoundRegionKind::Named(def).get_name(interner), None);
        }
    });
}

fn compatible<'db>(
    interner: DbInterner<'db>,
    kind: AliasTermKind<'db>,
    args: &[GenericArg<'db>],
) -> bool {
    interner.check_alias_term_args_compatible(kind, GenericArgs::new_from_slice(args))
}

#[test]
fn alias_term_args_compatible_by_form() {
    let fx = fixture::db(
        r#"
struct S;
impl S {
    type Assoc<U> = U;
}
trait Tr<T> {
    type Proj<'x>;
}
"#,
    );
    let file_id = fx.only_file();
    let db = fx.into_db();

    crate::attach_db(&db, || {
        let module_id = db.module_for_file(file_id.file_id(&db));
        let def_map = module_id.def_map(&db);
        let scope = &def_map.module(&db, module_id).scope(&db);
        let interner = DbInterner::new_with(&db, module_id.krate(&db));

        let ModuleDefId::AdtId(s) = declared(&db, scope, "S") else {
            panic!("`S` should be a struct");
        };
        let ModuleDefId::TraitId(tr) = declared(&db, scope, "Tr") else {
            panic!("`Tr` should be a trait");
        };
        let proj = tr
            .trait_items(&db)
            .items
            .iter()
            .find_map(|(name, item)| match *item {
                AssocItemId::TypeAliasId(it) if name.as_str() == "Proj" => Some(it),
                _ => None,
            })
            .expect("a type `Proj` in `Tr`");
        let assoc = impl_type(&db, impl_with(&db, scope, "Assoc"), "Assoc");

        let s: GenericArg<'_> = Ty::new_adt(interner, s, GenericArgs::empty()).into();
        let u8: GenericArg<'_> = interner.default_types().types.u8.into();
        let statik: GenericArg<'_> = interner.default_types().regions.statik.into();

        // `S::Assoc<u8>` in "Self form": the self type, then the item's own parameters.
        let inherent = AliasTermKind::InherentTy { def_id: InherentAssocTyId(assoc) };
        assert!(compatible(interner, inherent, &[s, u8]));
        assert!(!compatible(interner, inherent, &[u8]));
        assert!(!compatible(interner, inherent, &[s]));
        assert!(!compatible(interner, inherent, &[s, u8, u8]));
        assert!(!compatible(interner, inherent, &[s, statik]));

        // `<S as Tr<u8>>::Proj<'static>`: the trait's `Self` and `T`, then the item's `'x`.
        let projection = AliasTermKind::ProjectionTy { def_id: TraitAssocTyId(proj) };
        assert!(compatible(interner, projection, &[s, u8, statik]));
        assert!(!compatible(interner, projection, &[s, u8]));
        assert!(!compatible(interner, projection, &[s, statik]));
        assert!(!compatible(interner, projection, &[s, u8, u8]));
        assert!(!compatible(interner, projection, &[s, u8, statik, statik]));
    });
}

#[test]
fn type_of_impls_consts_statics_and_anon_consts() {
    let fx = fixture::db(
        r#"
struct W<T>(T);
impl<T> W<T> {
    fn new() {}
}
const C: u32 = 0;
static ST: [u8; 4] = [0; 4];
struct N<const K: u32>;
fn h(_: N<{ 1 + 1 }>) {}
"#,
    );
    let file_id = fx.only_file();
    let db = fx.into_db();

    crate::attach_db(&db, || {
        let module_id = db.module_for_file(file_id.file_id(&db));
        let def_map = module_id.def_map(&db);
        let scope = &def_map.module(&db, module_id).scope(&db);
        let krate = module_id.krate(&db);
        let interner = DbInterner::new_with(&db, krate);
        let display_target = DisplayTarget::from_crate(&db, krate);

        let ModuleDefId::FunctionId(h) = declared(&db, scope, "h") else {
            panic!("`h` should be a function");
        };
        let anon_const = AnonConstId::all_from_signature(&db, h.into())
            .next()
            .expect("the anonymous const `{ 1 + 1 }` in `h`'s signature");
        let ModuleDefId::ConstId(c) = declared(&db, scope, "C") else {
            panic!("`C` should be a const");
        };
        let ModuleDefId::StaticId(st) = declared(&db, scope, "ST") else {
            panic!("`ST` should be a static");
        };

        let cases: [(SolverDefId<'_>, &str); 4] = [
            (impl_with(&db, scope, "new").into(), "W<T>"),
            (c.into(), "u32"),
            (st.into(), "[u8; 4]"),
            // The type of `N`'s parameter `K`, not the `usize` of an array length.
            (anon_const.into(), "u32"),
        ];
        for (def, expected) in cases {
            let ty = interner.type_of(def).instantiate_identity().skip_norm_wip();
            assert_eq!(
                // alloc: the printed type is what the test compares, once per case.
                ty.display_test(&db, display_target).to_string(),
                expected,
                "the type of {def:?}"
            );
        }
    });
}

#[test]
fn an_item_bound_only_an_upstream_crate_could_meet_is_impossible_only_there() {
    let fx = fixture::db(
        r#"
//- /dep.rs crate:dep
pub struct Foreign;
pub trait ForeignTrait {}
pub trait Tr<T> {
    fn m(&self) where T: ForeignTrait {}
}
pub struct S;
impl Tr<Foreign> for S {}
//- /main.rs crate:main deps:dep
"#,
    );
    let file_ids = fx.files().to_vec();
    let db = fx.into_db();

    crate::attach_db(&db, || {
        let dep_module = db.module_for_file(file_ids[0].file_id(&db));
        let main = db.module_for_file(file_ids[1].file_id(&db)).krate(&db);
        let def_map = dep_module.def_map(&db);
        let scope = &def_map.module(&db, dep_module).scope(&db);
        let ModuleDefId::TraitId(tr) = declared(&db, scope, "Tr") else {
            panic!("`Tr` should be a trait");
        };
        let m = tr
            .trait_items(&db)
            .items
            .iter()
            .find_map(|(name, item)| match *item {
                AssocItemId::FunctionId(it) if name.as_str() == "m" => Some(it),
                _ => None,
            })
            .expect("`Tr::m`");
        let impl_ = scope.impls().next().expect("`impl Tr<Foreign> for S`");

        // `dep` knows every impl of `ForeignTrait`, so `Foreign: ForeignTrait` fails there.
        assert!(crate::traits::is_impossible_associated_item(
            &db,
            dep_module.krate(&db),
            impl_,
            m.into(),
        ));
        // `main` can't: `dep` may add the impl, so the bound is ambiguous, not impossible.
        assert!(!crate::traits::is_impossible_associated_item(&db, main, impl_, m.into()));
    });
}
