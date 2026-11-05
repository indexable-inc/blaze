//! Bound hashes: what code binds to by identity, which code hashes leave out.

use base_db::all_crates;
use blaze_ids::ContentId;
use hir_def::{ModuleDefId, signatures::FunctionSignature};
use hir_expand::fixture;

use super::{TestDB, edit, item, single};
use crate::{ItemId, bound_hash, code_hash};

fn bound(db: &TestDB, path: &str) -> ContentId {
    bound_hash(db, item(db, path))
        .unwrap_or_else(|err| panic!("{}", err.describe(db)))
        .unwrap_or_else(|| panic!("`{path}` has no body"))
}

fn code(db: &TestDB, path: &str) -> ContentId {
    code_hash(db, item(db, path)).unwrap().unwrap().0
}

#[test]
fn calls_of_two_functions_of_one_signature_bind_apart() {
    let (db, _) = single(
        r#"
fn a() -> u32 { 1 }
fn b() -> u32 { 2 }
fn calls_a() -> u32 { a() }
fn calls_b() -> u32 { b() }
"#,
    );
    assert_eq!(code(&db, "calls_a"), code(&db, "calls_b"), "callees by interface");
    assert_ne!(bound(&db, "calls_a"), bound(&db, "calls_b"));
}

#[test]
fn swapping_two_bodies_changes_their_bound_hashes() {
    let (mut db, file) = single(
        r#"
fn a() -> u32 { 1 }
fn b() -> u32 { 2 }
fn f() -> u32 { a() }
fn g() -> u32 { b() }
"#,
    );
    let (f, g) = (bound(&db, "f"), bound(&db, "g"));
    edit(
        &mut db,
        file,
        r#"
fn a() -> u32 { 1 }
fn b() -> u32 { 2 }
fn f() -> u32 { b() }
fn g() -> u32 { a() }
"#,
    );
    assert_eq!(bound(&db, "f"), g);
    assert_eq!(bound(&db, "g"), f);
}

/// Whether MIR names `C` or holds its evaluated bytes, the value is bound.
#[test]
fn a_constant_s_value_is_bound() {
    let (mut db, file) = single("const C: u32 = 1;\nfn f() -> u32 { C }\n");
    let before = bound(&db, "f");
    edit(&mut db, file, "const C: u32 = 2;\nfn f() -> u32 { C }\n");
    assert_ne!(bound(&db, "f"), before);
}

#[test]
fn renaming_a_callee_keeps_the_code_and_changes_what_is_bound() {
    let (mut db, file) = single("fn a() {}\nfn f() { a() }\n");
    let (code_before, bound_before) = (code(&db, "f"), bound(&db, "f"));
    edit(&mut db, file, "fn renamed() {}\nfn f() { renamed() }\n");
    assert_eq!(code(&db, "f"), code_before);
    assert_ne!(bound(&db, "f"), bound_before);
}

/// The call names `Tr::m` either way; what it resolves to moves from `S`'s impl to the
/// blanket one.
#[test]
fn a_trait_method_binds_to_the_impl_it_resolves_to() {
    let common = "trait Tr { fn m(&self) -> u32; }\nstruct S;\nfn on_s(s: &S) -> u32 { s.m() }\n";
    let (mut db, file) =
        single(&format!("{common}impl Tr for S {{ fn m(&self) -> u32 {{ 1 }} }}\n"));
    let (code_before, bound_before) = (code(&db, "on_s"), bound(&db, "on_s"));
    edit(&mut db, file, &format!("{common}impl<X> Tr for X {{ fn m(&self) -> u32 {{ 1 }} }}\n"));
    assert_eq!(code(&db, "on_s"), code_before);
    assert_ne!(bound(&db, "on_s"), bound_before);
}

#[test]
fn a_new_impl_of_an_object_s_trait_changes_the_bound_hash() {
    let text = "trait Tr { fn m(&self); }\nstruct S;\nimpl Tr for S { fn m(&self) {} }\n\
                fn f(s: &S) -> &dyn Tr { s }\n";
    // The unsizing coercion to `&dyn Tr` is `core`'s `Unsize` and `CoerceUnsized`; `m`'s
    // `&self` is dispatchable, and so `Tr` dyn-compatible, through `DispatchFromDyn`.
    let (mut db, file) =
        single(&format!("//- minicore: sized, coerce_unsized, dispatch_from_dyn\n{text}"));
    let before = bound(&db, "f");
    edit(&mut db, file, &format!("{text}struct U;\nimpl Tr for U {{ fn m(&self) {{}} }}\n"));
    assert_ne!(bound(&db, "f"), before);
}

/// The function `name` of the crate `krate`.
fn function_of(db: &TestDB, krate: &str, name: &str) -> ItemId {
    let krate = *all_crates(db)
        .iter()
        .find(|it| {
            it.extra_data(db)
                .display_name
                .as_ref()
                .is_some_and(|it| it.crate_name().as_str() == krate)
        })
        .unwrap_or_else(|| panic!("no crate `{krate}`"));
    hir_def::test_items::declarations(db, krate)
        .into_iter()
        .find_map(|def| match def {
            ModuleDefId::FunctionId(it) if FunctionSignature::of(db, it).name.as_str() == name => {
                Some(ItemId::Function(it))
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no function `{name}`"))
}

/// selfbuild2's T1: an impl of an object's trait in a crate the item's crate does not load
/// (a sibling member, another test target, one whose types don't check) can't be what its
/// coercion picks, so it neither changes the item's bound hash nor fails it. Before, the set
/// was every impl in the graph, and one `#[derive(Debug)]` of a crate with an error type made
/// every test binary's table refuse std's `dyn Debug` coercion.
#[test]
fn an_impl_in_a_crate_the_item_does_not_load_leaves_its_bound_hash() {
    let lib = "pub trait Tr { fn m(&self); }\npub struct S;\nimpl Tr for S { fn m(&self) {} }\n\
               pub fn f(s: &S) -> &dyn Tr { s }\n";
    let fx = fixture::db(&format!(
        "//- minicore: sized, coerce_unsized, dispatch_from_dyn\n//- /lib.rs crate:lib deps:core\n{lib}\
         //- /other.rs crate:other deps:lib,core\npub struct U;\n"
    ));
    let other = fx.file(1);
    let mut db = fx.into_db();
    let other = other.file_id(&db);
    let bound_of = |db: &TestDB| {
        bound_hash(db, function_of(db, "lib", "f"))
            .unwrap_or_else(|err| panic!("{}", err.describe(db)))
            .expect("`f` has a body")
    };
    let before = bound_of(&db);
    edit(
        &mut db,
        other,
        "pub struct U;\nimpl lib::Tr for U { fn m(&self) {} }\n\
         pub struct E(Missing);\nimpl lib::Tr for E { fn m(&self) {} }\n\
         impl lib::Tr for Missing { fn m(&self) {} }\n",
    );
    assert_eq!(bound_of(&db), before);
}

/// The survey's real trigger, as the review's T2 asks: a builtin `#[derive(Debug)]` (whose
/// impl signature is its type's, through `BuiltinDerive`, not an `impl` item's) on a struct
/// with an error-typed field, in a crate `lib` does not load, neither fails nor changes the
/// bound hash of `lib`'s `dyn Debug` coercion.
#[test]
fn a_derive_on_an_error_typed_struct_in_a_crate_the_item_does_not_load_leaves_its_bound_hash() {
    let lib = "#[derive(Debug)]\npub struct S;\n\
               pub fn f(s: &S) -> &dyn core::fmt::Debug { s }\n";
    let fx = fixture::db(&format!(
        "//- minicore: sized, coerce_unsized, dispatch_from_dyn, fmt, derive\n\
         //- /lib.rs crate:lib deps:core\n{lib}\
         //- /other.rs crate:other deps:lib,core\npub struct U;\n"
    ));
    let other = fx.file(1);
    let mut db = fx.into_db();
    let other = other.file_id(&db);
    let bound_of = |db: &TestDB| {
        bound_hash(db, function_of(db, "lib", "f"))
            .unwrap_or_else(|err| panic!("{}", err.describe(db)))
            .expect("`f` has a body")
    };
    let before = bound_of(&db);
    edit(&mut db, other, "#[derive(Debug)]\npub struct B(Missing);\n");
    assert_eq!(bound_of(&db), before);
}

/// The review's P1: a crate's impls of a trait are hashed once (`own_impls_value`) for every
/// crate that loads it, not once per loader. A chain of four crates, each with an impl of
/// `c0::Tr` and a `dyn Tr` coercion, computes the own value of each crate with an impl of it
/// once (the four's; core, with none, is skipped by `trait_holders`) on a cold database, where
/// re-hashing per loader read 1 + 2 + 3 + 4 = 10 impl signatures.
#[test]
fn each_crates_impls_of_a_trait_are_hashed_once_for_all_its_loaders() {
    let mut text = String::from("//- minicore: sized, coerce_unsized, dispatch_from_dyn\n");
    for n in 0..4 {
        let deps: Vec<String> =
            (0..n).map(|it| format!("c{it}")).chain(["core".to_owned()]).collect();
        let tr = if n == 0 { "pub trait Tr { fn m(&self); }\n" } else { "" };
        let path = if n == 0 { "Tr" } else { "c0::Tr" };
        text.push_str(&format!(
            "//- /c{n}.rs crate:c{n} deps:{}\n{tr}pub struct S{n};\nimpl {path} for S{n} {{ fn m(&self) {{}} }}\n\
             pub fn f(s: &S{n}) -> &dyn {path} {{ s }}\n",
            deps.join(",")
        ));
    }
    let db = fixture::db(&text).into_db();
    let executed = db.log_executed(|| {
        for n in 0..4 {
            bound_hash(&db, function_of(&db, &format!("c{n}"), "f"))
                .unwrap_or_else(|err| panic!("{}", err.describe(&db)))
                .expect("`f` has a body");
        }
    });
    let runs = |query: &str| {
        executed.iter().filter(|it| it.trim_end_matches('_').ends_with(query)).count()
    };
    assert_eq!(runs("seen_impls_value"), 4, "{executed:#?}");
    assert_eq!(runs("own_impls_value"), 4, "{executed:#?}");
}

#[test]
fn an_item_without_a_body_has_no_bound_hash() {
    let (db, _) = single("struct S;\n");
    assert_eq!(bound_hash(&db, item(&db, "S")).unwrap(), None);
}
