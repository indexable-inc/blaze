//! The facts: what an edit changes and what it keeps.

use blaze_ids::ContentId;
use blaze_query::QueryKey;
use hir_def::{
    AdtId, DefWithBodyId, ModuleId, TraitId, expr_store::Body, hir::Expr, nameres::crate_def_map,
};
use hir_ty::{
    db::{InternedClosure, InternedClosureId},
    next_solver::SimplifiedType as HirSimplifiedType,
};
use hir_expand::fixture;

use super::{TestDB, edit, item, single};
use crate::{
    Fact, ItemId, ItemKey, ScopeEntry, ScopeIndexKey, captures, code, const_value, const_value_of_static,
    discriminant, drop_check, hidden_types, implements, item_key, key, method, name, scope,
    signature, value, vtable,
};

#[track_caller]
fn val(db: &TestDB, fact: Fact) -> ContentId {
    value(db, &fact).unwrap_or_else(|err| panic!("{}", err.describe(db)))
}

#[track_caller]
fn adt(db: &TestDB, path: &str) -> AdtId {
    match item(db, path) {
        ItemId::Adt(it) => it,
        other => panic!("`{path}` is {other:?}"),
    }
}

#[track_caller]
fn trait_(db: &TestDB, path: &str) -> TraitId {
    match item(db, path) {
        ItemId::Trait(it) => it,
        other => panic!("`{path}` is {other:?}"),
    }
}

fn implements_of(db: &TestDB, trait_path: &str, ty: &str) -> Fact {
    implements(db, trait_(db, trait_path), &HirSimplifiedType::Adt(adt(db, ty).into()))
}

#[track_caller]
fn module(db: &TestDB, name: &str) -> ModuleId {
    crate_def_map(db, fixture::test_crate(db))
        .modules(db)
        .map(|(id, _)| id)
        .find(|id| id.name(db).is_some_and(|it| it.as_str() == name))
        .unwrap_or_else(|| panic!("no module `{name}`"))
}

fn resolves_of(db: &TestDB, scope: &str, name: &str) -> Fact {
    Fact::Resolves { scope: key::module_key(db, module(db, scope)), name: key::name_hash(name) }
}

#[test]
fn an_impl_changes_its_trait_and_type_alone() {
    let (mut db, file) = single(
        r#"
trait Trait {}
trait Other {}
struct A;
struct B;
"#,
    );
    let facts = [
        implements_of(&db, "Trait", "A"),
        implements_of(&db, "Trait", "B"),
        implements_of(&db, "Other", "A"),
    ];
    let before = facts.map(|it| val(&db, it));
    edit(
        &mut db,
        file,
        r#"
trait Trait {}
trait Other {}
struct A;
struct B;
impl Trait for A {}
"#,
    );
    let after = facts.map(|it| val(&db, it));
    assert_ne!(after[0], before[0], "an impl of Trait for A is a new answer for (Trait, A)");
    assert_eq!(after[1], before[1], "(Trait, B) did not ask about A");
    assert_eq!(after[2], before[2], "(Other, A) did not ask about Trait");
}

#[test]
fn a_blanket_impl_changes_every_type() {
    let (mut db, file) = single(
        r#"
trait Trait {}
struct A;
struct B;
"#,
    );
    let facts = [implements_of(&db, "Trait", "A"), implements_of(&db, "Trait", "B")];
    let before = facts.map(|it| val(&db, it));
    edit(
        &mut db,
        file,
        r#"
trait Trait {}
struct A;
struct B;
impl<T> Trait for T {}
"#,
    );
    let after = facts.map(|it| val(&db, it));
    assert_ne!(after[0], before[0]);
    assert_ne!(after[1], before[1]);
}

#[test]
fn no_impl_and_no_trait_are_answers() {
    let (mut db, file) = single(
        r#"
trait Trait {}
struct A;
impl Trait for A {}
"#,
    );
    let fact = implements_of(&db, "Trait", "A");
    let present = val(&db, fact);
    edit(
        &mut db,
        file,
        r#"
trait Trait {}
struct A;
"#,
    );
    let no_impl = val(&db, fact);
    assert_ne!(no_impl, present, "an impl and none differ");
    edit(
        &mut db,
        file,
        r#"
struct A;
"#,
    );
    let no_trait = val(&db, fact);
    assert_ne!(no_trait, no_impl, "a deleted trait is absent, not an empty one");
    assert_ne!(no_trait, present);
}

#[test]
fn an_item_changes_its_name_in_its_module_alone() {
    let (mut db, file) = single(
        r#"
mod m { pub fn o() {} }
mod other {}
"#,
    );
    let facts =
        [resolves_of(&db, "m", "n"), resolves_of(&db, "m", "o"), resolves_of(&db, "other", "n")];
    let before = facts.map(|it| val(&db, it));
    edit(
        &mut db,
        file,
        r#"
mod m { pub fn o() {} pub fn n() {} }
mod other {}
"#,
    );
    let after = facts.map(|it| val(&db, it));
    assert_ne!(after[0], before[0], "`n` is now something in `m`");
    assert_eq!(after[1], before[1], "`o` in `m` is what it was");
    assert_eq!(after[2], before[2], "`n` in `other` is still nothing");
}

#[test]
fn an_item_shadowing_a_glob_changes_the_name() {
    let (mut db, file) = single(
        r#"
mod a { pub fn x() {} }
mod m { pub use crate::a::*; }
"#,
    );
    let fact = resolves_of(&db, "m", "x");
    let before = val(&db, fact);
    edit(
        &mut db,
        file,
        r#"
mod a { pub fn x() {} }
mod m { pub use crate::a::*; pub fn x() {} }
"#,
    );
    assert_ne!(val(&db, fact), before, "the explicit `x` shadows the glob's");
}

#[test]
fn a_rename_changes_the_name_alone() {
    let (mut db, file) = single(
        r#"
fn f(x: u32) -> u32 { x + 1 }
"#,
    );
    let old = item(&db, "f");
    let ItemId::Function(func) = old else { panic!("`f` is {old:?}") };
    let (old_name, old_signature, old_code) =
        (val(&db, name(&db, old)), val(&db, signature(&db, func)), val(&db, code(&db, old)));
    let old_key = item_key(&db, old);
    edit(
        &mut db,
        file,
        r#"
fn g(x: u32) -> u32 { x + 1 }
"#,
    );
    let new = item(&db, "g");
    let ItemId::Function(func) = new else { panic!("`g` is {new:?}") };
    assert_ne!(item_key(&db, new), old_key);
    assert_ne!(val(&db, name(&db, new)), old_name, "the name is observable");
    assert_eq!(val(&db, signature(&db, func)), old_signature);
    assert_eq!(val(&db, code(&db, new)), old_code);
    assert_ne!(val(&db, Fact::Name(old_key)), old_name, "the old key names nothing");
}

#[test]
fn a_const_with_the_same_value_keeps_it() {
    let (mut db, file) = single(
        r#"
const C: u32 = 1 + 1;
"#,
    );
    let ItemId::Const(konst) = item(&db, "C") else { panic!("`C` is no const") };
    let fact = const_value(&db, konst);
    let before = val(&db, fact);
    edit(
        &mut db,
        file,
        r#"
const C: u32 = 2;
"#,
    );
    assert_eq!(val(&db, fact), before, "the expression changed, the value did not");
    edit(
        &mut db,
        file,
        r#"
const C: u32 = 3;
"#,
    );
    assert_ne!(val(&db, fact), before);
}

fn closure<'db>(db: &'db TestDB, path: &str) -> InternedClosureId<'db> {
    let ItemId::Function(func) = item(db, path) else { panic!("`{path}` is no fn") };
    let owner = DefWithBodyId::from(func);
    let found: Vec<_> = Body::of(db, owner)
        .exprs()
        .filter_map(|(expr, it)| match it {
            Expr::Closure { closure_kind, .. } => Some(InternedClosureId::new(
                db,
                InternedClosure { owner: owner.into(), expr, kind: closure_kind },
            )),
            _ => None,
        })
        .collect();
    match found[..] {
        [one] => one,
        _ => panic!("{} closures in `{path}`", found.len()),
    }
}

#[test]
fn a_capture_edit_changes_captures() {
    // #906: a closure stayed stale after its captures changed.
    let (mut db, file) = single(
        r#"
fn f() -> u32 {
    let a = 1u8;
    let b = 2u32;
    let c = || a as u32;
    c() + b
}
"#,
    );
    let fact = captures(&db, closure(&db, "f"));
    let before = val(&db, fact);
    edit(
        &mut db,
        file,
        r#"
fn f() -> u32 {
    let a = 1u8;
    let b = 2u32;
    let c = || b;
    c() + a as u32
}
"#,
    );
    assert_ne!(val(&db, fact), before, "the closure captures `b` now, not `a`");
}

#[test]
fn a_method_changes_its_name_alone() {
    let (mut db, file) = single(
        r#"
struct A;
impl A { fn n(&self) {} }
"#,
    );
    let ty = HirSimplifiedType::Adt(adt(&db, "A").into());
    let m = key::name_hash("m");
    let n = key::name_hash("n");
    let facts = [
        Fact::Method { self_ty: key::simplified(&db, &ty), name: m },
        Fact::Method { self_ty: key::simplified(&db, &ty), name: n },
    ];
    let before = facts.map(|it| val(&db, it));
    edit(
        &mut db,
        file,
        r#"
struct A;
impl A { fn n(&self) {} }
impl A { fn m(&self) {} }
"#,
    );
    let after = facts.map(|it| val(&db, it));
    assert_ne!(after[0], before[0], "`m` is a method of A now");
    assert_eq!(after[1], before[1], "`n` is what it was");
}

#[test]
fn a_method_constructor_hashes_like_the_fact() {
    let (db, _) = single(
        r#"
struct A;
impl A { fn n(&self) {} }
"#,
    );
    let ty = HirSimplifiedType::Adt(adt(&db, "A").into());
    let n = hir_expand::name::Name::new_symbol_root(intern::Symbol::intern("n"));
    assert_eq!(
        method(&db, &ty, &n),
        Fact::Method { self_ty: key::simplified(&db, &ty), name: key::name_hash("n") }
    );
}

#[test]
fn a_drop_impl_changes_drop_check() {
    let (mut db, file) = single(
        r#"
#![feature(lang_items)]
#[lang = "drop"]
pub trait Drop { fn drop(&mut self); }
struct S;
"#,
    );
    let fact = drop_check(&db, adt(&db, "S"));
    let before = val(&db, fact);
    edit(
        &mut db,
        file,
        r#"
#![feature(lang_items)]
#[lang = "drop"]
pub trait Drop { fn drop(&mut self); }
struct S;
impl Drop for S { fn drop(&mut self) {} }
"#,
    );
    assert_ne!(val(&db, fact), before);
}

#[test]
fn reordering_trait_methods_changes_the_vtable() {
    let (mut db, file) = single(
        r#"
trait T { fn a(&self); fn b(&self); }
struct S;
impl T for S { fn a(&self) {} fn b(&self) {} }
"#,
    );
    let fact = vtable(&db, trait_(&db, "T"), &HirSimplifiedType::Adt(adt(&db, "S").into()));
    let before = val(&db, fact);
    edit(
        &mut db,
        file,
        r#"
trait T { fn b(&self); fn a(&self); }
struct S;
impl T for S { fn a(&self) {} fn b(&self) {} }
"#,
    );
    assert_ne!(val(&db, fact), before, "the slots are in declaration order");
}

#[test]
fn a_type_and_a_const_of_one_name_are_one_run() {
    let (db, _) = single("trait T { type X; const X: u8; fn f(); }");
    let t = item_key(&db, ItemId::Trait(trait_(&db, "T")));
    let at = |name: &str| {
        let key = ScopeIndexKey { scope: t, name: key::name_hash(name) };
        hir_ty::attach_db(&db, || scope(&db, key)).unwrap()
    };
    let ScopeEntry::Assoc(items) = at("X") else { panic!("`X` is not an associated item") };
    assert_eq!(items.len(), 2);
    assert!(items.is_sorted_by_key(|&(key, _)| key));
    assert!(matches!(at("f"), ScopeEntry::Assoc(items) if items.len() == 1));
    assert_eq!(at("g"), ScopeEntry::Nothing);
}

const HIDDEN: &str = r#"
pub trait Tr {}
pub struct A;
pub struct B;
impl Tr for A {}
impl Tr for B {}
pub fn f() -> impl Tr { A }
"#;

#[test]
fn a_hidden_type_is_its_own_fact_and_not_the_signature() {
    let (mut db, file) = single(HIDDEN);
    let ItemId::Function(func) = item(&db, "f") else { panic!("`f` is no fn") };
    let facts = [signature(&db, func), hidden_types(&db, ItemId::Function(func))];
    let before = facts.map(|it| val(&db, it));
    edit(&mut db, file, &HIDDEN.replace("{ A }", "{ let _x = 0u8; A }"));
    assert_eq!(facts.map(|it| val(&db, it)), before, "the same hidden type");
    edit(&mut db, file, &HIDDEN.replace("{ A }", "{ B }"));
    let after = facts.map(|it| val(&db, it));
    assert_eq!(after[0], before[0], "the signature leaves the hidden type out");
    assert_ne!(after[1], before[1], "the hidden type is `B` now");
}

const LEAKS_OF_IMPLS: &str = r#"
pub trait Tr { const C: u8; }
pub struct A;
impl Tr for A { const C: u8 = 1; }
impl A { pub const fn m() -> u8 { 1 } }
"#;

#[test]
fn what_an_impls_items_leak_keeps_its_impl_facts() {
    let (mut db, file) = single(LEAKS_OF_IMPLS);
    let ty = HirSimplifiedType::Adt(adt(&db, "A").into());
    let m = hir_expand::name::Name::new_symbol_root(intern::Symbol::intern("m"));
    let facts = [implements_of(&db, "Tr", "A"), method(&db, &ty, &m)];
    let before = facts.map(|it| val(&db, it));
    edit(&mut db, file, &LEAKS_OF_IMPLS.replace("= 1;", "= 2;").replace("{ 1 }", "{ 2 }"));
    assert_eq!(facts.map(|it| val(&db, it)), before, "a const's and a const fn's bodies");
    edit(&mut db, file, &LEAKS_OF_IMPLS.replace("-> u8 { 1 }", "-> u16 { 1 }"));
    let after = facts.map(|it| val(&db, it));
    assert_eq!(after[0], before[0], "`Tr for A` is what it was");
    assert_ne!(after[1], before[1], "`m` returns another type");
}

const VALUES: &str = r#"
pub static S: u32 = 1 + 1;
pub enum E { A = 1 + 1, B }
"#;

#[test]
fn a_statics_and_a_discriminants_value_is_a_const_value() {
    let (mut db, file) = single(VALUES);
    let ItemId::Static(it) = item(&db, "S") else { panic!("`S` is no static") };
    let AdtId::EnumId(e) = adt(&db, "E") else { panic!("`E` is no enum") };
    let variants: Vec<_> = e.enum_variants(&db).variants.values().map(|&(it, _)| it).collect();
    let facts = [
        const_value_of_static(&db, it),
        discriminant(&db, variants[0]),
        discriminant(&db, variants[1]),
    ];
    let before = facts.map(|it| val(&db, it));
    assert_ne!(before[1], before[2], "`A` is 2, `B` is 3");
    edit(&mut db, file, &VALUES.replace("1 + 1", "2"));
    assert_eq!(facts.map(|it| val(&db, it)), before, "the same values");
    edit(&mut db, file, &VALUES.replace("1 + 1", "5"));
    let after = facts.map(|it| val(&db, it));
    for at in 0..3 {
        assert_ne!(after[at], before[at], "{:?}", facts[at]);
    }
}

#[test]
fn batched_name_hashes_are_the_single_ones() {
    let long = "x".repeat(1025);
    let names = ["", "a", "self", "r#type", "über", long.as_str()];
    let single: Vec<_> = names.iter().map(|it| key::name_hash(it)).collect();
    assert_eq!(crate::index::name_hashes(names.iter().copied()), single);
}

fn key_of(n: u8) -> ItemKey {
    ItemKey(ContentId([n; 32]))
}

#[test]
fn facts_of_one_item_have_distinct_keys() {
    let facts = [Fact::Signature(key_of(1)), Fact::Layout(key_of(1)), Fact::Code(key_of(1))];
    let keys: std::collections::BTreeSet<QueryKey> = facts.iter().map(Fact::key).collect();
    assert_eq!(keys.len(), facts.len());
}

#[test]
fn a_field_fact_names_its_field() {
    let a = Fact::Field { ty: key_of(1), field: 0 }.key();
    let b = Fact::Field { ty: key_of(1), field: 1 }.key();
    assert_ne!(a, b);
}
