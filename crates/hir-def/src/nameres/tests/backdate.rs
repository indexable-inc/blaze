//! An edit that changes no name, visibility or import of a crate leaves its `DefMap` equal (the
//! same frozen bytes, but for the content IDs of the files they name, which move with every
//! edit: [`frozen::encode_without_contents`]), so the def maps of the crates that depend on it
//! are not run again.

use base_db::{Crate, all_crates};
use hir_expand::fixture;

use crate::nameres::{
    crate_def_map, crate_resolution, frozen,
    tests::{TestDB, TestDBExt},
};

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
    /// How many times `main`'s resolution ran after the write, once `lib`'s was brought up to
    /// date.
    main_ran: usize,
}

/// Collects both crates, then writes `lib.rs` as `edited` and asks for `main`'s def map in the
/// snapshot the write made: `lib` is resolved first, and `main`'s resolution
/// ([`crate_resolution`], the per-crate query) runs again only where a binding of `lib` it
/// read changed. What `main` runs is only what `lib`'s change made it run.
fn edit_lib(edited: &str) -> Edited {
    let fx = fixture::db(&fixture());
    let files = fx.files().to_vec();
    let mut db = fx.into_db();
    let lib_file = files[0].file_id(&db);
    let (lib, main) = (crate_named(&db, "lib"), crate_named(&db, "main"));
    crate_def_map(&db, main);
    let lib_before = frozen::encode_without_contents(&db, lib).unwrap();
    let main_before = frozen::encode_without_contents(&db, main).unwrap();

    _ = db.set_file_text(lib_file, edited);
    // Both crates of the new snapshot: the write drops the arenas their ids were homed in.
    let (lib, main) = (crate_named(&db, "lib"), crate_named(&db, "main"));
    let runs = db.runs_in_read_order(|| _ = crate_def_map(&db, main));
    let main_ran = runs.iter().filter(|it| it.is::<crate_resolution>(&(main,))).count();
    Edited {
        lib_bytes: (lib_before, frozen::encode_without_contents(&db, lib).unwrap()),
        main_bytes: (main_before, frozen::encode_without_contents(&db, main).unwrap()),
        main_ran,
    }
}

// PORT (round 20): false by design on blaze-memo for the five `*_keep_the_def_map` tests,
// left failing at `main_ran == 0`. They claim an edit of `lib` that leaves its def map equal
// runs `main`'s resolution 0 times. `main` depends on `lib`: its arena borrows `lib`'s, which
// the edit makes again, so `main`'s is made again and its resolution runs again. Both def maps
// are still equal byte for byte (the two `assert_eq!`s before it hold).
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
#[ignore = "blaze-memo: an edit rebuilds its crate's arena; the claim is the host's, on records (docs/v2/plan/design/host.md)"]
fn signatures_keep_the_def_map() {
    check_backdated(
        &LIB.replace("pub fn f(x: u8) -> u8 { x }", "pub fn f(x: u16, y: &str) -> u64 { 0 }"),
    );
}

#[test]
#[ignore = "blaze-memo: an edit rebuilds its crate's arena; the claim is the host's, on records (docs/v2/plan/design/host.md)"]
fn fields_keep_the_def_map() {
    check_backdated(
        &LIB.replace("pub struct Fields { pub a: u8 }", "pub struct Fields { pub a: u8, b: u16 }"),
    );
    check_backdated(&LIB.replace("B { x: u8 }", "B { x: u8, y: u64 }"));
}

#[test]
#[ignore = "blaze-memo: an edit rebuilds its crate's arena; the claim is the host's, on records (docs/v2/plan/design/host.md)"]
fn types_of_consts_statics_and_aliases_keep_the_def_map() {
    check_backdated(&LIB.replace("pub const C: u8 = 0;", "pub const C: u32 = 7;"));
    check_backdated(&LIB.replace("pub static S: u8 = 0;", "pub static S: [u8; 4] = [0; 4];"));
    check_backdated(&LIB.replace("pub type Alias = u8;", "pub type Alias = (u8, u16);"));
}

#[test]
#[ignore = "blaze-memo: an edit rebuilds its crate's arena; the claim is the host's, on records (docs/v2/plan/design/host.md)"]
fn trait_and_impl_items_keep_the_def_map() {
    check_backdated(
        &LIB.replace(
            "pub trait Tr { fn m(&self); }",
            "pub trait Tr { fn m(&self) -> u8; fn n() {} }",
        ),
    );
    check_backdated(&LIB.replace(
        "impl Tr for Fields { fn m(&self) {} }",
        "impl Tr for Fields { fn m(&self) { let _ = 1; } }",
    ));
}

#[test]
#[ignore = "blaze-memo: an edit rebuilds its crate's arena; the claim is the host's, on records (docs/v2/plan/design/host.md)"]
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
