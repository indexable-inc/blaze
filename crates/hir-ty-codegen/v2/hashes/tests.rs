//! The items lane's hashes as the code generator asks for them, over fixtures: what an edit
//! changes and what it keeps.

#[path = "../tests/db.rs"]
mod db;

use blaze_ids::ContentId;
use blaze_items::{CodeHash, ItemId};
use hir_def::{DefWithBodyId, FunctionId, TypeAliasId, expr_store::Body as BodyStore};
use hir_ty::{TyDefId, consteval::usize_const, db::HirDatabase};
use hir_ty_mir::mir::MirSpan;
use hir_expand::fixture;

use super::ItemHashes;
use crate::v2::{Body, HashSource, Hashes, by_id::ById};
use db::TestDB;

fn hashes() -> ItemHashes {
    ItemHashes::new(Box::new(|_| None))
}

/// The database of `//- minicore: {minicore}` and `text`, and the file `text` is, to edit.
fn fixture(minicore: &str, text: &str) -> (TestDB, base_db::FileId) {
    let fx = fixture::db(&format!("//- minicore: {minicore}\n{text}"));
    let file = fx.only_file();
    let db = fx.into_db();
    let file = file.file_id(&db);
    (db, file)
}

#[track_caller]
fn function(db: &TestDB, name: &str) -> FunctionId {
    hir_def::test_items::function(db, fixture::test_crate(db), name)
}

#[track_caller]
fn alias(db: &TestDB, name: &str) -> TypeAliasId {
    hir_def::test_items::type_alias(db, fixture::test_crate(db), name)
}

#[track_caller]
fn code(db: &TestDB, name: &str) -> CodeHash {
    hashes()
        .code(db, Body::Item(function(db, name).into()))
        .unwrap_or_else(|e| panic!("the code of `{name}`: {e}"))
}

/// The content of the type alias `name` stands for.
#[track_caller]
fn ty(db: &TestDB, name: &str) -> ContentId {
    let alias = alias(db, name);
    hir_ty::attach_db(db, || {
        let ty = db.ty(TyDefId::TypeAliasId(alias)).skip_binder();
        hashes().ty(db, ty).unwrap_or_else(|e| panic!("the type `{name}`: {e}"))
    })
}

const CALLS: &str = r#"
pub struct Vec<T> { ptr: *const T, len: usize }
pub type Bytes = Vec<u8>;
pub type Words = Vec<u16>;
pub type AlsoBytes = Bytes;
pub fn callee() -> u32 { 1 }
pub fn caller() -> u32 { callee() }
"#;

#[test]
fn two_runs_of_one_source_give_one_key() {
    let (a, _) = fixture("sized", CALLS);
    let (b, _) = fixture("sized", CALLS);
    let h = hashes();
    for name in ["callee", "caller"] {
        assert_eq!(
            h.item(&a, ItemId::Function(function(&a, name))),
            h.item(&b, ItemId::Function(function(&b, name))),
        );
        assert_eq!(code(&a, name), code(&b, name), "the code of `{name}`");
    }
    for name in ["Bytes", "Words"] {
        assert_eq!(ty(&a, name), ty(&b, name), "the type `{name}`");
    }
    assert_eq!(ty(&a, "Bytes"), ty(&a, "Bytes"), "one run asked twice");
}

#[test]
fn a_hash_source_says_what_the_hashes_it_holds_say() {
    let (db, _) = fixture("sized", CALLS);
    let (items, by_id) = (HashSource::Items(hashes()), HashSource::ById(ById));
    for name in ["callee", "caller"] {
        let func = function(&db, name);
        let (item, body) = (ItemId::Function(func), Body::Item(func.into()));
        assert_eq!(items.item(&db, item), hashes().item(&db, item));
        assert_eq!(items.code(&db, body), hashes().code(&db, body));
        assert_eq!(items.declaration(&db, func), hashes().declaration(&db, func));
        assert_eq!(by_id.item(&db, item), ById.item(&db, item));
        assert_eq!(by_id.code(&db, body), ById.code(&db, body));
        assert_eq!(by_id.declaration(&db, func), ById.declaration(&db, func));
    }
}

#[test]
fn a_callee_body_edit_keeps_its_callers_code_and_an_interface_edit_does_not() {
    let (mut db, file) = fixture("sized", CALLS);
    let (callee, caller) = (code(&db, "callee"), code(&db, "caller"));

    db.set_file_text(file, &CALLS.replace("{ 1 }", "{ 2 }"));
    assert_ne!(code(&db, "callee"), callee, "the callee's body is its code");
    assert_eq!(code(&db, "caller"), caller, "a caller takes its callee by its interface");

    db.set_file_text(file, &CALLS.replace("pub fn callee", "pub(crate) fn callee"));
    assert_ne!(code(&db, "caller"), caller, "a callee's interface is in its callers' code");
}

const LOCALS: &str = r#"
pub fn callee() -> u32 { let one = 1; one }
pub fn caller() -> u32 { callee() }
"#;

#[test]
fn renaming_a_local_changes_its_functions_code_and_not_its_callers() {
    let (mut db, file) = fixture("sized", LOCALS);
    let (callee, caller) = (code(&db, "callee"), code(&db, "caller"));

    db.set_file_text(file, &LOCALS.replace("one", "uno"));
    assert_ne!(code(&db, "callee"), callee, "debug info names the local");
    assert_eq!(code(&db, "caller"), caller, "a caller takes its callee by its interface");
}

#[test]
fn a_type_is_its_arguments_and_not_its_alias() {
    let (db, _) = fixture("sized", CALLS);
    assert_ne!(ty(&db, "Bytes"), ty(&db, "Words"));
    assert_eq!(ty(&db, "Bytes"), ty(&db, "AlsoBytes"));
}

const DROPS: &str = r#"
pub struct Inner(pub u32);
pub struct Outer { pub inner: Inner }
pub type Held = Outer;
"#;

#[test]
fn a_drop_impl_of_a_fields_type_is_in_the_type() {
    let (mut db, file) = fixture("sized, drop", DROPS);
    let held = ty(&db, "Held");

    db.set_file_text(file, &format!("{DROPS}pub fn unrelated() {{}}\n"));
    assert_eq!(ty(&db, "Held"), held, "an item the type does not reach is not in it");

    db.set_file_text(file, &format!("{DROPS}impl Drop for Inner {{\n    fn drop(&mut self) {{}}\n}}\n"));
    assert_ne!(ty(&db, "Held"), held, "drop glue of `Outer` drops an `Inner`");
}

const NAMED: &str = r#"
pub mod m {
    pub struct A(pub u32);
}
pub type Held = m::A;
"#;

/// An instance's arguments are spelled by [`Hashes::ty`], so an instance of `type_name` or
/// `type_id`, and every generic calling one with its parameters (whose share key takes their
/// identity), is another instance once a type it is called with is renamed or moved: the
/// identity holds the type's key, of which the path `type_name` prints is a function.
#[test]
fn renaming_or_moving_a_type_changes_its_identity() {
    let (mut db, file) = fixture("sized", NAMED);
    let held = ty(&db, "Held");

    db.set_file_text(file, &format!("\n\n// moved down\n{NAMED}"));
    assert_eq!(ty(&db, "Held"), held, "a move within the file keeps what the program sees");

    db.set_file_text(file, &NAMED.replace("struct A", "struct B").replace("m::A", "m::B"));
    assert_ne!(ty(&db, "Held"), held, "`type_name` prints the new name");

    db.set_file_text(file, &NAMED.replace("mod m", "mod n").replace("m::A", "n::A"));
    assert_ne!(ty(&db, "Held"), held, "`type_name` prints the new module");
}

const VALUED: &str = r#"
pub const C: u32 = 1;
pub fn reads() -> u32 { C }
"#;

/// A body reading a constant that has one value for every use is keyed by that value: a new
/// value rebuilds the reader, another way of writing the same value rebuilds nothing.
#[test]
fn a_body_reading_a_constant_is_keyed_by_its_value() {
    let (mut db, file) = fixture("sized", VALUED);
    let reads = code(&db, "reads");

    db.set_file_text(file, &VALUED.replace("= 1", "= 2 - 1"));
    assert_eq!(code(&db, "reads"), reads, "one value, however written");

    db.set_file_text(file, &VALUED.replace("= 1", "= 2"));
    assert_ne!(code(&db, "reads"), reads, "a body reading it reads its value");
}

const LINED: &str = r#"
#[rustc_builtin_macro]
macro_rules! line {() => {}}
pub fn here() -> u32 { line!() }
#[track_caller]
pub fn tracked() -> u32 { line!() }
pub fn borrowed() -> &'static u32 { &line!() }
"#;

/// A function reads its `line!()` from the `Location` the link fills, so moving it keeps its
/// code. Where the line is in the code, a `#[track_caller]` function's (its `Location` is its
/// caller's) or a borrow promoted to a constant, the move is new code.
#[test]
fn moving_a_function_that_reads_its_line_keeps_its_code() {
    let (mut db, file) = fixture("panic_location", LINED);
    let names = ["here", "tracked", "borrowed"];
    let before = names.map(|it| code(&db, it));
    db.set_file_text(file, &format!("\n{LINED}"));
    let after = names.map(|it| code(&db, it));
    assert_eq!(after[0], before[0], "the line is read where the code runs");
    assert_ne!(after[1], before[1], "a `#[track_caller]` function's line is its value");
    assert_ne!(after[2], before[2], "a promoted line is its value");
}

const WRITTEN_TWICE: &str = r#"
//- /a.rs crate:a
pub fn here() -> u32 { 1 }
//- /main.rs crate:main deps:a
pub fn here() -> u32 { 1 }
"#;

/// A function written alike in two crates, as build scripts copy `cargo_env_var` between
/// packages, is two codes: a template's positions name its own crate's span tables, which a
/// link of the other crate's program does not have.
#[test]
fn a_function_written_alike_in_two_crates_is_two_codes() {
    let db = fixture::db(WRITTEN_TWICE).into_db();
    let main = fixture::test_crate(&db);
    let dep = main.data(&db).dependencies.iter().map(|it| it.crate_id).find(|it| !it.data(&db).origin.is_lang());
    let theirs = hir_def::test_items::function(&db, dep.expect("crate a"), "here");
    let ours = function(&db, "here");
    let code = |func: FunctionId| hashes().code(&db, Body::Item(func.into())).unwrap();
    assert_ne!(code(ours), code(theirs));
    let root = |func: FunctionId| {
        let owner = DefWithBodyId::FunctionId(func);
        hashes().span(&db, owner, MirSpan::ExprId(BodyStore::of(&db, owner).root_expr())).unwrap()
    };
    let (ours, theirs) = (root(ours), root(theirs));
    assert_eq!(ours.index, theirs.index, "the same span of each");
    assert_ne!(ours.anchor, theirs.anchor, "each in a table of its own crate's");
}

#[test]
fn a_constant_is_its_value() {
    let (db, _) = fixture("sized", CALLS);
    let krate = fixture::test_crate(&db);
    let h = hashes();
    let konst = |value: u128| {
        hir_ty::attach_db(&db, || {
            h.konst(&db, usize_const(&db, Some(value), krate)).unwrap_or_else(|e| panic!("{e}"))
        })
    };
    assert_eq!(konst(1), konst(1));
    assert_ne!(konst(1), konst(2));
}

const SPANNED: &str = r#"
pub fn f(x: u32) -> u32 { x + 1 }
"#;

#[test]
fn a_span_is_its_place_in_its_owner_and_formatting_keeps_it() {
    let (mut db, file) = fixture("sized", SPANNED);
    let h = hashes();
    let at = |db: &TestDB| {
        let owner = function(db, "f").into();
        let root = BodyStore::of(db, owner).root_expr();
        let start = h.span(db, owner, MirSpan::ExprId(root)).expect("a span");
        let end = h.span(db, owner, MirSpan::ExprEnd(root)).expect("a span");
        (owner, root, start, end)
    };
    let (owner, root, start, end) = at(&db);
    assert_ne!(start.anchor, end.anchor, "an expression's start and end are in two tables");
    let tables = ItemHashes::span_tables(&db, owner);
    let (_, spans) = tables.iter().find(|(anchor, _)| *anchor == start.anchor).expect("its table");
    assert_eq!(spans[start.index as usize], MirSpan::ExprId(root));

    db.set_file_text(file, "\n// a comment\npub fn f(x: u32) -> u32 {\n    x  +  1\n}\n");
    let (_, _, moved, moved_end) = at(&db);
    assert_eq!((moved, moved_end), (start, end));
}

#[test]
fn a_file_the_build_did_not_name_is_an_error() {
    let (db, file) = fixture("sized", SPANNED);
    let error = hashes().file_name(&db, file).expect_err("no name");
    assert!(error.contains(&format!("{file:?}")), "{error}");
    let named = ItemHashes::new(Box::new(|_| Some("src/lib.rs".to_owned())));
    assert_eq!(named.file_name(&db, file).as_deref(), Ok("src/lib.rs"));
}
