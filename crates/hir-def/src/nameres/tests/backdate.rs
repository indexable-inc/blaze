//! An edit that changes no name, visibility or import of a crate leaves its `DefMap` equal (the
//! same frozen bytes), so the def maps of the crates that depend on it are not run again.

use base_db::{Crate, all_crates};
use hir_expand::fixture;

use crate::nameres::{crate_def_map, frozen, tests::TestDB};

const LIB: &str = r#"
pub struct Fields { pub a: u8 }
pub struct Tuple(pub u8);
pub enum E { A, B { x: u8 } }
pub fn f(x: u8) -> u8 { x }
pub const C: u8 = 0;
pub static S: u8 = 0;
pub type Alias = u8;
pub trait Tr { fn m(&self); }
impl Tr for Fields { fn m(&self) {} }
pub mod inner { pub fn g() {} }
"#;

const MAIN: &str = r#"
use lib::*;
use lib::inner::g;
fn main() { f(C); g(); }
"#;

fn fixture() -> String {
    format!("//- /lib.rs crate:lib\n{LIB}\n//- /main.rs crate:main deps:lib\n{MAIN}")
}

fn crate_named(db: &TestDB, name: &str) -> Crate {
    *all_crates(db)
        .iter()
        .find(|it| it.extra_data(db).display_name.as_ref().is_some_and(|it| it.to_string() == name))
        .unwrap()
}

struct Edited {
    lib_bytes: (Vec<u8>, Vec<u8>),
    main_bytes: (Vec<u8>, Vec<u8>),
    /// Whether the write resolved groups of `main` again once `lib`'s were brought up to date.
    main_ran: usize,
}

/// Collects both crates, then writes `lib.rs` as `edited`. The write ends in the nameres
/// maintainer, which resolves `lib` first, then the groups of `main` that read a binding of
/// `lib` the write changed: what `main` runs is only what `lib`'s change made it run.
fn edit_lib(edited: &str) -> Edited {
    let fx = fixture::db(&fixture());
    let files = fx.files().to_vec();
    let mut db = fx.into_db();
    let lib = crate_named(&db, "lib");
    let main = crate_named(&db, "main");
    crate_def_map(&db, main);
    let lib_before = frozen::encode(&db, lib).unwrap();
    let main_before = frozen::encode(&db, main).unwrap();

    db.keep_maintained(true);
    let mut maintained = Vec::new();
    db.take_maintained(&mut maintained);
    maintained.clear();
    db.set_file_text(files[0].file_id(&db), edited);
    db.take_maintained(&mut maintained);
    db.keep_maintained(false);
    let main_ran = maintained
        .iter()
        .filter(|it| it.maintainer == "nameres" && it.krate == main && it.work.region > 0)
        .count();
    Edited {
        lib_bytes: (lib_before, frozen::encode(&db, lib).unwrap()),
        main_bytes: (main_before, frozen::encode(&db, main).unwrap()),
        main_ran,
    }
}

#[track_caller]
fn check_backdated(edited: &str) {
    assert_ne!(edited, LIB);
    let it = edit_lib(edited);
    assert_eq!(it.lib_bytes.0, it.lib_bytes.1, "lib's def map changed for:\n{edited}");
    assert_eq!(it.main_bytes.0, it.main_bytes.1, "main's def map changed for:\n{edited}");
    assert_eq!(it.main_ran, 0, "main's def map ran again for:\n{edited}");
}

#[track_caller]
fn check_rerun(edited: &str) {
    let it = edit_lib(edited);
    assert_ne!(it.lib_bytes.0, it.lib_bytes.1, "lib's def map stayed equal for:\n{edited}");
    assert_eq!(it.main_ran, 1, "main's def map did not run again for:\n{edited}");
}

#[test]
fn signatures_keep_the_def_map() {
    check_backdated(&LIB.replace(
        "pub fn f(x: u8) -> u8 { x }",
        "pub fn f(x: u16, y: &str) -> u64 { 0 }",
    ));
}

#[test]
fn fields_keep_the_def_map() {
    check_backdated(&LIB.replace(
        "pub struct Fields { pub a: u8 }",
        "pub struct Fields { pub a: u8, b: u16 }",
    ));
    check_backdated(&LIB.replace("B { x: u8 }", "B { x: u8, y: u64 }"));
}

#[test]
fn types_of_consts_statics_and_aliases_keep_the_def_map() {
    check_backdated(&LIB.replace("pub const C: u8 = 0;", "pub const C: u32 = 7;"));
    check_backdated(&LIB.replace("pub static S: u8 = 0;", "pub static S: [u8; 4] = [0; 4];"));
    check_backdated(&LIB.replace("pub type Alias = u8;", "pub type Alias = (u8, u16);"));
}

#[test]
fn trait_and_impl_items_keep_the_def_map() {
    check_backdated(&LIB.replace(
        "pub trait Tr { fn m(&self); }",
        "pub trait Tr { fn m(&self) -> u8; fn n() {} }",
    ));
    check_backdated(&LIB.replace(
        "impl Tr for Fields { fn m(&self) {} }",
        "impl Tr for Fields { fn m(&self) { let _ = 1; } }",
    ));
}

#[test]
fn bodies_keep_the_def_map() {
    check_backdated(&LIB.replace(
        "pub mod inner { pub fn g() {} }",
        "pub mod inner { pub fn g() { struct Local; } }",
    ));
}

/// The controls: a renamed item, a narrowed visibility and a new import each change what
/// `main` sees, so its def map runs again.
#[test]
fn names_visibilities_and_imports_rerun_dependents() {
    check_rerun(&LIB.replace("pub fn f(x: u8)", "pub fn renamed(x: u8)"));
    check_rerun(&LIB.replace("pub const C", "pub(crate) const C"));
    check_rerun(&format!("{LIB}\npub use inner::g as h;\n"));
}
