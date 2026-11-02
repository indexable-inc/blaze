//! A body reads only what it names: an edit re-infers the bodies whose text or whose named
//! items it changes, and no other. Each test checks every body once, makes one kind of edit and
//! checks them all again, counting what ran.

use hir_expand::EditionedFileId;
use hir_ty::{InferBodyId, InferenceResult};
use hir_expand::fixture;

use super::{TestDB, count, function};

const FIXTURE: &str = r#"
//- minicore: sized, copy, clone, derive, default
//- /main.rs crate:main
mod a;
mod b;
pub struct Shared(pub u32);
impl Shared {
    pub fn get(&self) -> u32 {
        self.0
    }
}
pub struct Other;
fn main() {
    let _ = a::reader();
}
//- /a.rs
use crate::Shared;
pub fn reader() -> u32 {
    let s = Shared(1);
    s.get()
}
//- /b.rs
pub fn edited() -> u32 {
    let x = 2;
    x
}
pub fn sibling() -> u32 {
    let y = 3;
    y
}
"#;

/// The module-level functions of [`FIXTURE`]: every body it has but `Shared::get`'s.
const BODIES: [&str; 4] = ["main", "reader", "edited", "sibling"];

fn fixture() -> (TestDB, Vec<EditionedFileId>) {
    let fx = fixture::db(FIXTURE);
    let files = fx.files().to_vec();
    let db = fx.into_db();
    check_all(&db);
    (db, files)
}

/// Infers and borrow-checks every body of [`BODIES`], and what ran doing it.
fn check_all(db: &TestDB) -> Vec<String> {
    crate::attach_db(db, || {
        db.log_executed(|| {
            for name in BODIES {
                let f = function(db, name);
                InferenceResult::of(db, InferBodyId::DefWithBodyId(f));
                hir_ty_borrowck::borrowck(db, f);
            }
        })
    })
}

/// Replaces `from` with `to` in the one file of `files` whose text has `from`.
fn edit(db: &mut TestDB, files: &[EditionedFileId], from: &str, to: &str) {
    let mut found = files.iter().map(|it| it.file_id(db)).filter(|&it| {
        let text = db.file_text(it).text(db);
        text.contains(from)
    });
    let file = found.next().unwrap_or_else(|| panic!("no file has `{from}`"));
    assert!(found.next().is_none(), "more than one file has `{from}`");
    let text = db.file_text(file).text(db).replacen(from, to, 1);
    db.set_file_text(file, &text);
}

/// After `from` becomes `to`, the number of bodies inferred and borrow-checked again.
fn rerun_after(from: &str, to: &str) -> (usize, usize, Vec<String>) {
    let (mut db, files) = fixture();
    edit(&mut db, &files, from, to);
    let executed = check_all(&db);
    (count(&executed, "infer_body_query"), count(&executed, "borrowck"), executed)
}

#[test]
fn a_warm_check_of_every_body_allocates_nothing() {
    let (db, _) = fixture();
    crate::attach_db(&db, || {
        let bodies = BODIES.map(|name| function(&db, name));
        blaze_alloc_count::assert_at_most(0, "a warm check of every body", || {
            for f in bodies {
                InferenceResult::of(&db, InferBodyId::DefWithBodyId(f));
                hir_ty_borrowck::borrowck(&db, f);
            }
        });
    });
}

#[test]
fn a_body_edit_reinfers_only_that_body() {
    let (infer, borrowck, executed) = rerun_after("let x = 2;", "let x = 25;");
    assert_eq!(infer, 1, "only `edited`: {executed:#?}");
    assert_eq!(borrowck, 1, "{executed:#?}");
}

#[test]
fn a_body_edit_that_types_the_same_reinfers_only_that_body() {
    let (infer, _, executed) = rerun_after("let y = 3;\n    y", "let y = 3;\n    let z = y;\n    z");
    assert_eq!(infer, 1, "only `sibling`: {executed:#?}");
}

#[test]
fn an_unrelated_impl_reinfers_no_body() {
    let (infer, borrowck, executed) = rerun_after(
        "pub struct Other;",
        "pub struct Other;\nimpl Other {\n    pub fn get(&self) -> u32 {\n        7\n    }\n}",
    );
    assert_eq!(infer, 0, "`reader` probes `get` on `Shared`, not `Other`: {executed:#?}");
    assert_eq!(borrowck, 0, "{executed:#?}");
}

#[test]
fn a_new_item_reinfers_no_body() {
    let (infer, borrowck, executed) = rerun_after(
        "pub fn sibling()",
        "pub struct Fresh;\npub enum Kind {\n    A,\n}\npub fn fresh() {}\npub fn sibling()",
    );
    assert_eq!(infer, 0, "{executed:#?}");
    assert_eq!(borrowck, 0, "{executed:#?}");
}

#[test]
fn a_derived_item_reinfers_no_body() {
    let (infer, borrowck, executed) = rerun_after(
        "pub fn sibling()",
        "#[derive(Clone, Copy, Default)]\npub struct Fresh;\npub fn sibling()",
    );
    assert_eq!(infer, 0, "{executed:#?}");
    assert_eq!(borrowck, 0, "{executed:#?}");
}

#[test]
fn a_use_reinfers_no_body() {
    let (infer, borrowck, executed) =
        rerun_after("pub fn edited()", "use crate::Other;\npub fn edited()");
    assert_eq!(infer, 0, "no body of `b` names `Other`: {executed:#?}");
    assert_eq!(borrowck, 0, "{executed:#?}");
}

#[test]
fn a_use_in_another_module_reinfers_no_body() {
    let (infer, _, executed) =
        rerun_after("use crate::Shared;", "use crate::Shared;\nuse crate::b::sibling;");
    assert_eq!(infer, 0, "`reader` does not name `sibling`: {executed:#?}");
}

#[test]
fn an_unresolved_use_reinfers_no_body() {
    let (infer, _, executed) =
        rerun_after("pub fn edited()", "use crate::Missing;\npub fn edited()");
    assert_eq!(infer, 0, "the unresolved import is a def map diagnostic only: {executed:#?}");
}

#[test]
fn a_trait_use_reinfers_no_body_of_another_module() {
    let (mut db, files) = fixture();
    edit(
        &mut db,
        &files,
        "pub fn edited()",
        "pub trait Local {\n    fn local(&self) {}\n}\nuse core::clone::Clone as _;\npub fn edited()",
    );
    let outside_b = crate::attach_db(&db, || {
        db.log_executed(|| {
            for name in ["main", "reader"] {
                InferenceResult::of(&db, InferBodyId::DefWithBodyId(function(&db, name)));
            }
        })
    });
    assert_eq!(count(&outside_b, "infer_body_query"), 0, "{outside_b:#?}");
    let inside_b = check_all(&db);
    assert!(count(&inside_b, "infer_body_query") <= 2, "at most `b`'s two bodies: {inside_b:#?}");
}

#[test]
fn a_changed_signature_reinfers_its_callers() {
    let (mut db, files) = fixture();
    edit(
        &mut db,
        &files,
        "pub fn get(&self) -> u32 {\n        self.0",
        "pub fn get(&self) -> u64 {\n        self.0 as u64",
    );
    let executed = check_all(&db);
    assert_eq!(count(&executed, "infer_body_query"), 1, "`reader`, which calls `get`: {executed:#?}");
    crate::attach_db(&db, || {
        let reader = function(&db, "reader");
        let result = InferenceResult::of(&db, InferBodyId::DefWithBodyId(reader));
        assert!(result.has_type_mismatches(), "`reader` returns the `u64` of `get` as `u32`");
    });
}

#[test]
fn a_removed_impl_reinfers_its_reader() {
    let (infer, _, executed) = rerun_after(
        "impl Shared {\n    pub fn get(&self) -> u32 {\n        self.0\n    }\n}",
        "",
    );
    assert_eq!(infer, 1, "`reader`, whose `get` is gone: {executed:#?}");
}

#[test]
fn an_edit_of_another_crate_reinfers_no_body() {
    let fx = fixture::db(
        r#"
//- minicore: sized
//- /dep.rs crate:dep library
pub fn used() -> u32 {
    1
}
pub fn unused() -> u32 {
    2
}
//- /main.rs crate:main deps:dep
fn main() {
    let _ = dep::used();
}
"#,
    );
    let files = fx.files().to_vec();
    let mut db = fx.into_db();
    let main = crate::attach_db(&db, || {
        let main = function(&db, "main");
        InferenceResult::of(&db, InferBodyId::DefWithBodyId(main));
        main
    });
    edit(&mut db, &files, "pub fn unused() -> u32 {\n    2", "pub fn unused() -> u32 {\n    let z = 2;\n    z");
    let executed = crate::attach_db(&db, || {
        db.log_executed(|| {
            InferenceResult::of(&db, InferBodyId::DefWithBodyId(main));
        })
    });
    assert_eq!(count(&executed, "infer_body_query"), 0, "{executed:#?}");
}
