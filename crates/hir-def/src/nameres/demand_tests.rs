use base_db::{Crate, all_crates};
use hir_expand::fixture;

use crate::{
    AdtId, AssocItemId, AttrDefId, DefWithBodyId, GenericDefId, ImplId, Lookup, ModuleDefId,
    VariantId,
    attrs::AttrFlags,
    hir::generics::GenericParams,
    lang_item::crate_lang_items,
    nameres::{
        assoc::{ImplItems, TraitItems},
        crate_def_map,
        demand::{
            body_blocks, bodies_declaring_roots, crate_body_owners, every_body,
            macros_declaring_roots, may_declare_roots,
        },
        frozen::{Dec, Enc},
    },
    resolver::HasResolver,
    signatures::{
        ConstSignature, EnumSignature, EnumVariants, FunctionSignature, ImplSignature,
        StaticSignature, StructSignature, TraitSignature, TypeAliasSignature, UnionSignature,
        VariantFields,
    },
    test_db::{TestDB, TestDBExt},
};

/// Items with bodies that declare items, in every place a block can be: nested blocks, a
/// macro's argument, a macro's expansion, a discriminant, a provided method, an impl's method,
/// named and unnamed consts, a static, and a function declared in a body.
const BLOCKS: &str = r#"
//- /lib.rs crate:lib
#![feature(lang_items)]
#[lang = "sized"]
pub trait Sized {}
macro_rules! wrap { ($e:expr) => { $e } }
macro_rules! make { () => { pub fn made() { struct M; impl M {} } } }
make!();
pub fn one() { { struct A; impl A {} { struct B; impl B {} } } }
pub fn two() { struct C; impl C {} }
pub fn three() { wrap!({ struct I; impl I {} }) }
pub enum E { V = { struct D; impl D {} 1 }, W }
pub trait T { fn provided() { struct P; impl P {} } }
pub struct S([u8; 2]);
pub union Un { a: u8 }
pub type Alias = S;
impl S { pub fn method() { struct Q; impl Q {} } }
pub const K: () = { struct KK; impl KK {} };
const _: () = { struct U; impl U {} };
pub static ST: () = { struct W; impl W {} };
pub fn nested() { fn inner() { struct N; impl N {} } }
pub fn plain() -> u8 { 1 }
"#;

fn crate_named(db: &TestDB, name: &str) -> Crate {
    *all_crates(db)
        .iter()
        .find(|it| {
            it.extra_data(db).display_name.as_ref().is_some_and(|it| it.to_string() == name)
        })
        .unwrap()
}

/// The queries of `executed` that lower a body or read what one lowered to.
fn lowering(executed: &[String]) -> Vec<&String> {
    const WORDS: &[&str] = &["body", "block_def_map", "exprscopes", "expr_scopes", "expressionstore"];
    let lowers = |it: &&String| {
        let it = it.to_lowercase();
        WORDS.iter().any(|word| it.contains(word))
    };
    executed.iter().filter(lowers).collect()
}

fn bodies_lowered(executed: &[String]) -> usize {
    executed.iter().filter(|it| it.contains("Body::of")).count()
}

fn owner_name(db: &TestDB, owner: DefWithBodyId) -> String {
    match owner {
        DefWithBodyId::FunctionId(it) => FunctionSignature::of(db, it).name.as_str().to_owned(),
        DefWithBodyId::ConstId(it) => {
            ConstSignature::of(db, it).name.as_ref().map_or("_".to_owned(), |it| it.as_str().to_owned())
        }
        DefWithBodyId::StaticId(it) => StaticSignature::of(db, it).name.as_str().to_owned(),
        DefWithBodyId::VariantId(it) => it.lookup(db).name.as_str().to_owned(),
    }
}

/// The item with a body named `name`: one of the crate's modules, or `inner`, which is declared
/// in the body of `nested` (lowered to find it).
fn owner_named(db: &TestDB, krate: Crate, name: &str) -> DefWithBodyId {
    let named = |owners: Vec<DefWithBodyId>, name: &str| {
        owners.into_iter().find(|&it| owner_name(db, it) == name)
    };
    if name == "inner" {
        let nested = named(crate_body_owners(db, krate), "nested").unwrap();
        return named(body_blocks(db, nested).owners, "inner").unwrap();
    }
    named(crate_body_owners(db, krate), name).unwrap_or_else(|| panic!("no body owner {name}"))
}

fn impls_in(db: &TestDB, owner: DefWithBodyId) -> Vec<ImplId> {
    let blocks = body_blocks(db, owner).blocks;
    let modules = blocks.iter().flat_map(|&(_, def_map)| def_map.modules(db));
    modules.flat_map(|(_, module)| module.scope(db).impls().collect::<Vec<_>>()).collect()
}

fn ask_assoc(db: &TestDB, item: AssocItemId) {
    match item {
        AssocItemId::FunctionId(it) => {
            _ = FunctionSignature::of(db, it);
            _ = AttrFlags::query(db, AttrDefId::FunctionId(it));
            _ = it.resolver(db);
            _ = GenericParams::of(db, GenericDefId::FunctionId(it));
        }
        AssocItemId::ConstId(it) => {
            _ = ConstSignature::of(db, it);
            _ = AttrFlags::query(db, AttrDefId::ConstId(it));
            _ = it.resolver(db);
        }
        AssocItemId::TypeAliasId(it) => {
            _ = TypeAliasSignature::of(db, it);
            _ = AttrFlags::query(db, AttrDefId::TypeAliasId(it));
            _ = it.resolver(db);
        }
    }
}

fn ask_def(db: &TestDB, def: ModuleDefId) {
    match def {
        ModuleDefId::FunctionId(it) => ask_assoc(db, AssocItemId::FunctionId(it)),
        ModuleDefId::ConstId(it) => ask_assoc(db, AssocItemId::ConstId(it)),
        ModuleDefId::TypeAliasId(it) => ask_assoc(db, AssocItemId::TypeAliasId(it)),
        ModuleDefId::StaticId(it) => {
            _ = StaticSignature::of(db, it);
            _ = AttrFlags::query(db, AttrDefId::StaticId(it));
            _ = it.resolver(db);
        }
        ModuleDefId::AdtId(adt) => {
            _ = AttrFlags::query(db, AttrDefId::AdtId(adt));
            _ = adt.resolver(db);
            _ = GenericParams::of(db, GenericDefId::AdtId(adt));
            match adt {
                AdtId::StructId(it) => {
                    _ = StructSignature::of(db, it);
                    _ = VariantFields::of(db, VariantId::StructId(it));
                }
                AdtId::UnionId(it) => {
                    _ = UnionSignature::of(db, it);
                    _ = VariantFields::of(db, VariantId::UnionId(it));
                }
                AdtId::EnumId(it) => {
                    _ = EnumSignature::of(db, it);
                    let variants = &EnumVariants::of(db, it).0;
                    _ = variants.is_payload_free(db);
                    for (variant, _, _) in variants.with_explicit_discriminant() {
                        _ = VariantFields::of(db, VariantId::EnumVariantId(variant));
                        _ = AttrFlags::query(db, AttrDefId::EnumVariantId(variant));
                        _ = variant.resolver(db);
                    }
                }
            }
        }
        ModuleDefId::TraitId(it) => {
            _ = TraitSignature::of(db, it);
            _ = AttrFlags::query(db, AttrDefId::TraitId(it));
            _ = it.resolver(db);
            for &(_, item) in TraitItems::query(db, it).items.iter() {
                ask_assoc(db, item);
            }
        }
        ModuleDefId::MacroId(it) => _ = AttrFlags::query(db, AttrDefId::MacroId(it)),
        _ => {}
    }
}

/// Every signature of `krate`, the attributes and resolver of every item, and its lang items.
fn ask_signatures(db: &TestDB, krate: Crate) {
    let def_map = crate_def_map(db, krate);
    for (module, data) in def_map.modules(db) {
        _ = AttrFlags::query(db, AttrDefId::ModuleId(module));
        _ = module.resolver(db);
        let scope = data.scope(db);
        for def in scope.declarations() {
            ask_def(db, def);
        }
        for imp in scope.impls() {
            _ = ImplSignature::of(db, imp);
            _ = AttrFlags::query(db, AttrDefId::ImplId(imp));
            _ = imp.resolver(db);
            for &(_, item) in ImplItems::of(db, imp).0.items.iter() {
                ask_assoc(db, item);
            }
        }
    }
    _ = crate_lang_items(db, krate);
}

#[test]
fn signatures_lower_no_body() {
    let db = fixture::db(BLOCKS).into_db();
    let krate = crate_named(&db, "lib");
    let executed = db.executed_in_read_order(|| {
        ask_signatures(&db, krate);
        for owner in crate_body_owners(&db, krate) {
            _ = may_declare_roots(&db, owner);
        }
        _ = macros_declaring_roots(&db, krate);
    });
    assert!(executed.iter().any(|it| it.contains("crate_local_def_map")), "{executed:#?}");
    assert_eq!(lowering(&executed), Vec::<&String>::new());
}

#[test]
fn payload_free_as_before() {
    const ENUMS: &str = r#"
//- /lib.rs crate:lib
pub enum Unit { A, B }
pub enum UnitDiscriminant { A = 1, B }
pub enum EmptyTuple { A(), B }
pub enum EmptyTupleDiscriminant { A() = 1, B }
pub enum EmptyStruct { A {}, B }
pub enum EmptyStructDiscriminant { A {} = 2 }
pub enum Fields { A(u8) }
pub enum FieldCfgdOut { A(#[cfg(any())] u8), B }
pub enum VariantCfgdOut { #[cfg(any())] A(u8), B }
pub enum DiscriminantCfgdOut { #[cfg(any())] A() = 1, B }
pub enum Twice { A() = 1, A }
"#;
    let db = fixture::db(ENUMS).into_db();
    let krate = crate_named(&db, "lib");
    let def_map = crate_def_map(&db, krate);
    let enums: Vec<_> = def_map
        .modules(&db)
        .flat_map(|(_, module)| module.scope(&db).declarations().collect::<Vec<_>>())
        .filter_map(|it| match it {
            ModuleDefId::AdtId(AdtId::EnumId(it)) => Some(it),
            _ => None,
        })
        .collect();
    let mut found = Vec::new();
    let executed = db.executed_in_read_order(|| {
        for &it in &enums {
            let name = EnumSignature::of(&db, it).name.as_str().to_owned();
            let variants = &EnumVariants::of(&db, it).0;
            let explicit: Vec<bool> =
                variants.with_explicit_discriminant().map(|(_, _, it)| it).collect();
            found.push((name, variants.is_payload_free(&db), explicit));
        }
    });
    assert_eq!(lowering(&executed), Vec::<&String>::new());
    let expected = [
        ("Unit", true, vec![false, false]),
        ("UnitDiscriminant", true, vec![true, false]),
        ("EmptyTuple", true, vec![false, false]),
        ("EmptyTupleDiscriminant", false, vec![true, false]),
        ("EmptyStruct", true, vec![false, false]),
        ("EmptyStructDiscriminant", false, vec![true]),
        ("Fields", false, vec![false]),
        ("FieldCfgdOut", true, vec![false, false]),
        ("VariantCfgdOut", true, vec![false]),
        ("DiscriminantCfgdOut", true, vec![false]),
        // The name keeps its first place and the last variant's discriminant.
        ("Twice", true, vec![false]),
    ];
    let expected: Vec<_> =
        expected.into_iter().map(|(name, free, explicit)| (name.to_owned(), free, explicit)).collect();
    assert_eq!(found, expected);
    for &it in &enums {
        let variants = &EnumVariants::of(&db, it).0;
        for (variant, _, explicit) in variants.with_explicit_discriminant() {
            assert_eq!(variants.has_explicit_discriminant(variant), explicit);
        }
    }
}

/// The owners of [`BLOCKS`] whose bodies hold an impl, and how many bodies reading a block
/// module of theirs back lowers: theirs, and for `inner` that of `nested` too.
const OWNERS: &[(&str, usize)] = &[
    ("made", 1),
    ("one", 1),
    ("two", 1),
    ("three", 1),
    ("V", 1),
    ("provided", 1),
    ("method", 1),
    ("K", 1),
    ("_", 1),
    ("ST", 1),
    ("inner", 2),
];

#[test]
fn frozen_blocks_round_trip_lowering_only_their_owner() {
    for &(name, read_lowers) in OWNERS {
        let db = fixture::db(BLOCKS).into_db();
        let krate = crate_named(&db, "lib");
        let owner = owner_named(&db, krate, name);
        let impls = impls_in(&db, owner);
        assert!(!impls.is_empty(), "{name}");
        for imp in impls {
            let mut bytes = Vec::new();
            let executed = db.executed_in_read_order(|| {
                let mut enc = Enc::new(&db, krate);
                enc.impl_id(imp).unwrap_or_else(|why| panic!("{name}: {why}"));
                bytes = enc.finish().unwrap();
            });
            assert_eq!(lowering(&executed), Vec::<&String>::new(), "{name}");

            let mut dec = Dec::new_checked(&db, krate, &bytes).unwrap();
            assert_eq!(dec.impl_id(), Ok(imp), "{name}");
            assert_eq!(dec.finish(), Ok(()), "{name}");

            let fresh = fixture::db(BLOCKS).into_db();
            let fresh_krate = crate_named(&fresh, "lib");
            _ = crate_def_map(&fresh, fresh_krate);
            let mut read = None;
            let executed = fresh.executed_in_read_order(|| {
                let mut dec = Dec::new_checked(&fresh, fresh_krate, &bytes).unwrap();
                read = Some(dec.impl_id().unwrap_or_else(|why| panic!("{name}: {why}")));
            });
            assert_eq!(bodies_lowered(&executed), read_lowers, "{name}: {executed:#?}");
            let fresh_owner = owner_named(&fresh, fresh_krate, name);
            assert!(impls_in(&fresh, fresh_owner).contains(&read.unwrap()), "{name}");
        }
    }
}

#[test]
fn the_blocks_of_a_body_lower_only_it() {
    let db = fixture::db(BLOCKS).into_db();
    let krate = crate_named(&db, "lib");
    _ = crate_def_map(&db, krate);
    let nested = owner_named(&db, krate, "nested");
    let mut owners = Vec::new();
    let executed = db.executed_in_read_order(|| {
        let blocks = body_blocks(&db, nested);
        assert_eq!(blocks.blocks.len(), 1);
        owners = blocks.owners;
    });
    assert_eq!(bodies_lowered(&executed), 1, "{executed:#?}");
    let names: Vec<_> = owners.iter().map(|&it| owner_name(&db, it)).collect();
    assert_eq!(names, ["inner"]);

    let one = owner_named(&db, krate, "one");
    let executed = db.executed_in_read_order(|| assert_eq!(body_blocks(&db, one).blocks.len(), 2));
    assert_eq!(bodies_lowered(&executed), 1, "{executed:#?}");
}

#[test]
fn every_body_reaches_the_bodies_in_bodies() {
    let db = fixture::db(BLOCKS).into_db();
    let krate = crate_named(&db, "lib");
    let mut names: Vec<_> = every_body(&db, krate).into_iter().map(|it| owner_name(&db, it)).collect();
    names.sort();
    let mut expected: Vec<_> = OWNERS.iter().map(|&(it, _)| it.to_owned()).collect();
    expected.extend(["nested", "plain"].map(str::to_owned));
    expected.sort();
    assert_eq!(names, expected);
}

#[test]
fn only_bodies_that_may_declare_roots_are_lowered() {
    const ROOTS: &str = r#"
//- /dep.rs crate:dep
#[macro_export]
macro_rules! submit { () => { $crate::__keep!(); } }
#[macro_export]
macro_rules! __keep { () => { #[used] static KEPT: u8 = 0; } }
#[macro_export]
macro_rules! harmless { () => { 1 } }
//- /lib.rs crate:lib deps:dep
pub fn plain() { let _x = dep::harmless!(); }
pub fn exports() { #[no_mangle] extern "C" fn exported() {} }
pub fn submits() { dep::submit!(); }
pub fn local_macro() {
    macro_rules! keep { () => { #[used] static S: u8 = 0; } }
    fn inner() { keep!(); }
}
"#;
    let db = fixture::db(ROOTS).into_db();
    let krate = crate_named(&db, "lib");
    let by_name = |name: &str| {
        crate_body_owners(&db, krate).into_iter().find(|&it| owner_name(&db, it) == name).unwrap()
    };
    let executed = db.executed_in_read_order(|| {
        assert!(!may_declare_roots(&db, by_name("plain")));
        assert!(may_declare_roots(&db, by_name("exports")));
        assert!(may_declare_roots(&db, by_name("submits")));
        assert!(may_declare_roots(&db, by_name("local_macro")));
    });
    assert_eq!(lowering(&executed), Vec::<&String>::new());
    let declaring = macros_declaring_roots(&db, krate);
    let declaring: Vec<_> = declaring.iter().map(|it| it.as_str()).collect();
    assert_eq!(declaring, ["__keep", "submit"]);

    let mut names: Vec<_> =
        bodies_declaring_roots(&db, krate).into_iter().map(|it| owner_name(&db, it)).collect();
    names.sort();
    assert_eq!(names, ["exports", "inner", "local_macro", "submits"]);
}
