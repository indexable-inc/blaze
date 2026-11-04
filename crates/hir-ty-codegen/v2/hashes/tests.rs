//! The items lane's hashes as the code generator asks for them, over fixtures: what an edit
//! changes and what it keeps.

#[path = "../tests/db.rs"]
mod db;

use blaze_ids::ContentId;
use blaze_items::{CodeHash, ItemId};
use hir_def::{DefWithBodyId, FunctionId, TypeAliasId, expr_store::Body as BodyStore};
use hir_expand::fixture;
use hir_ty::{TyDefId, consteval::usize_const, db::HirDatabase};
use hir_ty_mir::mir::MirSpan;

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

/// What the template of the function `name` is kept under where no debugger is told of its
/// locals, as `hashes` names files: its template code with what it reads of names, as the
/// front end folds them (`frontend::with_names`).
#[track_caller]
fn template_by(hashes: &ItemHashes, db: &TestDB, name: &str) -> CodeHash {
    let body = Body::Item(function(db, name).into());
    let code = hashes
        .template_code(db, body)
        .unwrap_or_else(|e| panic!("the template code of `{name}`: {e}"));
    let (names, whole) = hashes
        .names_read(db, body, false)
        .unwrap_or_else(|e| panic!("the names `{name}` reads: {e}"));
    assert!(whole, "untold, all `{name}` reads of names is keyed");
    let mut h = blaze_ids::Hasher::new("blaze v2 test template with names 1");
    h.id(&code.0).id(&names);
    CodeHash(h.finish())
}

/// The template code and the code of the function `name` in a fresh database of `text`.
#[track_caller]
fn both(minicore: &str, text: &str, name: &str) -> (CodeHash, CodeHash) {
    let (db, _) = fixture(minicore, text);
    (template_by(&hashes(), &db, name), code(&db, name))
}

/// The content of the type alias `name` stands for.
#[track_caller]
fn ty(db: &TestDB, name: &str) -> ContentId {
    let alias = alias(db, name);
    blaze_query::db::attach(db, || {
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

/// Debug info names a local by its binding and reads its name at link (`v2::side`), so the
/// name is in no code.
#[test]
fn renaming_a_local_keeps_its_functions_code_and_its_callers() {
    let (mut db, file) = fixture("sized", LOCALS);
    let (callee, caller) = (code(&db, "callee"), code(&db, "caller"));

    db.set_file_text(file, &LOCALS.replace("one", "uno"));
    assert_eq!(code(&db, "callee"), callee, "a local is its binding, not its name");
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

    db.set_file_text(
        file,
        &format!("{DROPS}impl Drop for Inner {{\n    fn drop(&mut self) {{}}\n}}\n"),
    );
    assert_ne!(ty(&db, "Held"), held, "drop glue of `Outer` drops an `Inner`");
}

const NAMED: &str = r#"
pub mod m {
    pub struct A(pub u32);
}
pub type Held = m::A;
"#;

/// An instance's arguments are spelled by [`Hashes::ty`], so an instance of `type_name` or
/// `type_id`, and every generic calling one with its parameters (whose compile identity takes their
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
    let dep = main
        .data(&db)
        .dependencies
        .iter()
        .map(|it| it.crate_id)
        .find(|it| !it.data(&db).origin.is_lang());
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
        blaze_query::db::attach(&db, || {
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

const PLACED: &str = r#"
pub mod m {
    pub fn callee(x: u32) -> u32 { x + 1 }
}
pub fn caller(x: u32) -> u32 { let one = m::callee(x); one * 2 }
"#;

/// Unison's rule (docs/v2/plan/design/name-free-identity.md): a function's template is kept
/// under what it does, never what it or its callees are called or where they are written. Its
/// code, which its instance key and the link name it by, keeps the key of the store it is
/// written in, so two items written alike stay two functions, each with its own positions.
#[test]
fn renaming_or_moving_a_function_keeps_its_template_code() {
    let (template, code) = both("sized", PLACED, "caller");
    let same = |text: &str, name: &str, why: &str| {
        let (made, owned) = both("sized", text, name);
        assert_eq!(made, template, "{why}: its template is what it does");
        owned
    };
    let renamed = PLACED.replace("fn caller", "fn renamed");
    assert_ne!(same(&renamed, "renamed", "renamed"), code, "its instance is its store's");
    let moved = PLACED
        .replace("pub fn caller(x: u32) -> u32 { let one = m::callee(x); one * 2 }\n", "")
        .replace(
            "pub mod m {\n",
            "pub mod m {\n    pub fn caller(x: u32) -> u32 { let one = callee(x); one * 2 }\n",
        );
    assert_ne!(
        same(&moved, "caller", "moved into the callee's module"),
        code,
        "its instance is its store's"
    );
    let callee_renamed = PLACED.replace("callee", "next");
    assert_eq!(
        same(&callee_renamed, "caller", "its callee renamed"),
        code,
        "a callee is its interface"
    );
    let local_renamed = PLACED.replace("one", "uno");
    assert_eq!(same(&local_renamed, "caller", "a local renamed"), code, "a local is its binding");
}

const OBSERVED: &str = r#"
#![feature(intrinsics)]
mod intrinsics {
    #[rustc_intrinsic]
    pub const fn type_name<T: ?Sized>() -> &'static str;
}
pub mod m { pub struct A; }
pub fn reads() -> &'static str { intrinsics::type_name::<m::A>() }
pub fn plain() -> u32 { 1 }
"#;

/// A name the code reads is in its template's key, and only in that code's.
#[test]
fn a_template_that_reads_a_name_is_keyed_by_it() {
    let (reads, plain) = (both("sized", OBSERVED, "reads").0, both("sized", OBSERVED, "plain").0);
    let renamed = OBSERVED.replace("struct A", "struct B").replace("m::A", "m::B");
    assert_ne!(both("sized", &renamed, "reads").0, reads, "`type_name` prints the new name");
    assert_eq!(both("sized", &renamed, "plain").0, plain, "what reads no name keeps its template");
    let moved = OBSERVED.replace("pub mod m", "pub mod n").replace("m::A", "n::A");
    assert_ne!(both("sized", &moved, "reads").0, reads, "`type_name` prints the new module");
    assert_eq!(both("sized", &moved, "plain").0, plain, "what reads no name keeps its template");
}

/// A function reads its `line!()` from the `Location` its instance's anchors fill, so moving it
/// to another line or module keeps its template. Where the line is a value of the code, a
/// `#[track_caller]` function's or a promoted borrow's, a move is another template.
#[test]
fn a_template_that_reads_its_line_is_keyed_by_it() {
    let names = ["here", "tracked", "borrowed"];
    let before = names.map(|it| both("panic_location", LINED, it));
    let down = format!("\n{LINED}");
    let after = names.map(|it| both("panic_location", &down, it).0);
    assert_eq!(after[0], before[0].0, "the line is read where the code runs");
    assert_ne!(after[1], before[1].0, "a `#[track_caller]` function's line is its value");
    assert_ne!(after[2], before[2].0, "a promoted line is its value");
    let moved = LINED.replace(
        "pub fn here() -> u32 { line!() }",
        "pub mod m { pub fn here() -> u32 { line!() } }",
    );
    let (template, code) = both("panic_location", &moved, "here");
    assert_eq!(template, before[0].0, "moved into a module on its own line");
    assert_ne!(code, before[0].1, "its instance is its store's");
}

const FILED: &str = r#"
#[rustc_builtin_macro]
macro_rules! file {() => {}}
pub fn named() -> &'static str { file!() }
pub fn plain() -> u32 { 1 }
"#;

/// `file!()` bakes the file's name into the code's data: the name the build gives the file is
/// in the template's key of the code that reads it, and in no other's.
#[test]
fn a_template_that_reads_its_file_is_keyed_by_its_name() {
    let (db, _) = fixture("sized", FILED);
    let lib = ItemHashes::new(Box::new(|_| Some("src/lib.rs".to_owned())));
    let other = ItemHashes::new(Box::new(|_| Some("src/other.rs".to_owned())));
    assert_ne!(
        template_by(&lib, &db, "named"),
        template_by(&other, &db, "named"),
        "`file!()` says the new name"
    );
    assert_eq!(
        template_by(&lib, &db, "plain"),
        template_by(&other, &db, "plain"),
        "what reads no file keeps its template"
    );
}

const TOLD: &str = r#"
pub struct A(pub u32);
pub fn holds() -> u32 { let held = A(1); held.0 }
pub trait Tr { type Out; fn out(&self) -> Self::Out; }
pub fn projects<T: Tr>(it: T) -> u32 { let out = it.out(); let _ = out; 1 }
"#;

/// What the function `name` of a fresh database of `text` reads of names, where a debugger is
/// told of its locals when `told`; `None` when that is not all of it.
#[track_caller]
fn told_template(text: &str, name: &str, told: bool) -> Option<ContentId> {
    let (db, _) = fixture("sized", text);
    let (names, whole) = hashes()
        .names_read(&db, Body::Item(function(&db, name).into()), told)
        .unwrap_or_else(|e| panic!("the names `{name}` reads: {e}"));
    whole.then_some(names)
}

/// Where a debugger is told of a function's locals, their types' identities are in what it
/// reads of names, which its code and its template code both hold (`side::Told::ty` holds
/// them): renaming the type a local is of is another key there, and the same one where none
/// is told. Which binding is of which type counts too. A told local whose type is a
/// projection normalizes to what neither the code nor the arguments key, so its body has no
/// template code of its own and is kept under its code.
#[test]
fn a_told_locals_type_is_in_its_template_key() {
    let renamed = TOLD.replace("struct A", "struct B").replace("A(1)", "B(1)");
    let (told, quiet) = (told_template(TOLD, "holds", true), told_template(TOLD, "holds", false));
    assert!(
        told.is_some() && quiet.is_some(),
        "a local of a type with no alias is told by its identity"
    );
    assert_ne!(told_template(&renamed, "holds", true), told, "the debugger names the new type");
    assert_eq!(told_template(&renamed, "holds", false), quiet, "untold, the name is in no key");
    let swapped = TOLD.replace(
        "pub fn holds() -> u32 { let held = A(1); held.0 }",
        "pub fn holds() -> u32 { let held = A(1); let n = 1u32; held.0 + n }",
    );
    let reordered = TOLD.replace(
        "pub fn holds() -> u32 { let held = A(1); held.0 }",
        "pub fn holds() -> u32 { let n = 1u32; let held = A(1); held.0 + n }",
    );
    assert_ne!(
        told_template(&swapped, "holds", true),
        told_template(&reordered, "holds", true),
        "each binding by its own type"
    );
    assert_eq!(told_template(TOLD, "projects", true), None, "a told projection keeps its code");
    assert!(told_template(TOLD, "projects", false).is_some(), "untold, the projection is no input");
}
