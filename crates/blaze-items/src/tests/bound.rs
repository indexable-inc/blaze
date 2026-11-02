//! Bound hashes: what code binds to by identity, which code hashes leave out.

use blaze_ids::ContentId;

use super::{TestDB, edit, item, single};
use crate::{bound_hash, code_hash};

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
    let (mut db, file) = single(&format!("{common}impl Tr for S {{ fn m(&self) -> u32 {{ 1 }} }}\n"));
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

#[test]
fn an_item_without_a_body_has_no_bound_hash() {
    let (db, _) = single("struct S;\n");
    assert_eq!(bound_hash(&db, item(&db, "S")).unwrap(), None);
}
