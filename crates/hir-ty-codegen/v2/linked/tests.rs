//! What a crate's `#[link]`s and a build's positions give the link, over fixtures.

#[path = "../tests/db.rs"]
mod db;

use blaze_codegen::{Arch, CompiledFunction, Line, NativeLib, NativeLibKind, driver::Built};
use blaze_format::SpanRef;
use blaze_ids::ContentId;
use blaze_items::{ItemId, item_key};
use hir_def::{DefWithBodyId, FunctionId, expr_store::Body as BodyStore};
use hir_ty_mir::mir::MirSpan;
use hir_expand::fixture;

use super::*;
use crate::v2::{BuildTime, Databases, ItemHashes};
use db::TestDB;

fn frontend(text: &str) -> (&'static TestDB, HirFrontend<'static>) {
    let db: &'static TestDB = Box::leak(Box::new(fixture::db(text).into_db()));
    let hashes = HashSource::Items(ItemHashes::new(Box::new(|_| Some("src/lib.rs".to_owned()))));
    (db, HirFrontend::new(Databases::new(db), hashes, std::sync::Arc::new(BuildTime(Default::default()))))
}

#[track_caller]
fn function(db: &TestDB, name: &str) -> FunctionId {
    hir_def::test_items::function(db, fixture::test_crate(db), name)
}

fn lib(name: &str, kind: NativeLibKind) -> NativeLib {
    NativeLib { name: name.to_owned(), kind, verbatim: false }
}

#[test]
fn a_crates_libraries_are_its_links_in_the_order_written() {
    let (db, frontend) = frontend(
        r#"
#[link(name = "z")]
extern "C" {}
#[link(name = "ssl", kind = "static", modifiers = "+whole-archive,-bundle")]
#[link(name = "Foundation", kind = "framework")]
extern "C" {}
#[link(name = "left_out", cfg(any()))]
#[link(name = "exact.so", kind = "dylib", modifiers = "+verbatim")]
extern "C" {}
"#,
    );
    let libs = frontend.linked(fixture::test_crate(&db)).unwrap();
    assert_eq!(
        libs,
        [
            lib("z", NativeLibKind::Dylib),
            lib("ssl", NativeLibKind::Static { whole: true, bundle: false }),
            lib("Foundation", NativeLibKind::Framework),
            NativeLib { name: "exact.so".to_owned(), kind: NativeLibKind::Dylib, verbatim: true },
        ]
    );
}

#[test]
fn a_link_blaze_cannot_make_is_an_error_naming_it() {
    for (text, named) in [
        ("#[link(name = \"kernel32\", kind = \"raw-dylib\")]\nextern \"C\" {}", "raw-dylib"),
        ("#[link(wasm_import_module = \"env\")]\nextern \"C\" {}", "wasm_import_module"),
        ("#[link(name = \"m\", modifiers = \"+whole-archive\")]\nextern \"C\" {}", "whole-archive"),
        ("#[link(name = \"-Wl,-z\", kind = \"link-arg\")]\nextern \"C\" {}", "kind = \"link-arg\""),
        ("#[link(name = \"m\", modifiers = \"+as-needed\")]\nextern \"C\" {}", "+as-needed"),
        ("#[link(name = \"m\", modifiers = \"-as-needed\")]\nextern \"C\" {}", "-as-needed"),
        ("#[link(name = \"m\", import_name_type = \"undecorated\")]\nextern \"C\" {}", "import_name_type"),
        ("extern \"C\" {\n    #[link_ordinal(1)]\n    fn ordinal();\n}", "`#[link_ordinal(..)]` on `ordinal`"),
        ("extern \"C\" {\n    #[link_ordinal(2)]\n    static ORDINAL: u8;\n}", "`#[link_ordinal(..)]` on `ORDINAL`"),
    ] {
        let (db, frontend) = frontend(text);
        let error = frontend.linked(fixture::test_crate(&db)).unwrap_err();
        assert!(error.what.contains(named), "{text}: {}", error.what);
    }
}

fn compiled(key: u8, lines: Vec<Line>) -> CompiledFunction {
    CompiledFunction {
        key: InstanceKey(ContentId([key; 32])),
        arch: Arch::X86_64,
        align: 16,
        code: vec![0xc3],
        relocs: Default::default(),
        data: Vec::new(),
        inlined: Vec::new(),
        side: None,
        unwind: None,
        lines,
        coverage: None,
        profile: None,
    }
}

const SPANNED: &str = "pub fn f(x: u32) -> u32 {\n    x + 1\n}\n";

#[test]
fn the_tables_the_code_names_are_given_with_their_positions() {
    let (db, frontend) = frontend(SPANNED);
    let func = function(db, "f");
    let owner = DefWithBodyId::FunctionId(func);
    let root = BodyStore::of(db, owner).root_expr();
    let hashes = &frontend.hashes;
    let start = hashes.span(db, owner, MirSpan::ExprId(root)).unwrap();
    let declared = hashes.declaration(db, func).unwrap();
    let built = Built {
        functions: vec![compiled(1, vec![Line { at: 0, span: start }, Line { at: 1, span: declared }])],
        items: vec![item_key(db, ItemId::Function(func))],
        debug_names: vec![(InstanceKey(ContentId([1; 32])), "lib::f".to_owned())],
        ..Built::default()
    };
    let inputs = frontend.link_inputs(&built).unwrap();
    assert_eq!(inputs.debug_names, built.debug_names);
    let mut anchors: Vec<ContentId> = inputs.spans.iter().map(|it| it.0).collect();
    assert!(anchors.is_sorted());
    anchors.sort_unstable();
    let mut want = vec![start.anchor, declared.anchor];
    want.sort_unstable();
    assert_eq!(anchors, want, "the two tables the lines name, no other");

    let table = |anchor: ContentId| &inputs.spans.iter().find(|it| it.0 == anchor).unwrap().1;
    let body = table(start.anchor);
    assert_eq!(&body[..8], b"BZSPANS1");
    assert_eq!(u32::from_le_bytes(body[8..12].try_into().unwrap()), 1, "one file");
    let name_len = u32::from_le_bytes(body[12..16].try_into().unwrap()) as usize;
    assert_eq!(&body[16..16 + name_len], b"src/lib.rs");
    let after = 16 + name_len;
    let count = u32::from_le_bytes(body[after..after + 4].try_into().unwrap()) as usize;
    assert_eq!(count, BodyStore::of(db, owner).exprs().count(), "a row per expression");
    assert_eq!(body.len(), after + 4 + 12 * count);
    let row = after + 4 + 12 * start.index as usize;
    let words: Vec<u32> =
        body[row..row + 12].chunks(4).map(|it| u32::from_le_bytes(it.try_into().unwrap())).collect();
    // The body's block, on the first line, from its 25th column.
    assert_eq!(words, [0, 1, 25]);

    let declaration = table(declared.anchor);
    assert_eq!(declaration.len(), 8 + 4 + 4 + name_len + 4 + 12);
}

#[test]
fn a_position_of_no_table_the_build_has_is_an_error() {
    let (db, frontend) = frontend(SPANNED);
    let func = function(db, "f");
    let stray = SpanRef { anchor: ContentId([9; 32]), index: 0 };
    let unknown = ItemKey(ContentId([7; 32]));
    let built = Built {
        functions: vec![compiled(1, vec![Line { at: 0, span: stray }])],
        items: vec![item_key(db, ItemId::Function(func)), unknown],
        debug_names: vec![(InstanceKey(ContentId([1; 32])), "lib::f".to_owned())],
        ..Built::default()
    };
    let error = frontend.link_inputs(&built).unwrap_err();
    assert!(error.what.contains(&format!("{:?}", stray.anchor)), "{}", error.what);
    assert!(error.what.contains("a line row of the function"), "says what names it: {}", error.what);
    assert!(error.what.contains("`lib::f`"), "names the function: {}", error.what);
    assert!(
        error.what.contains(&format!("1 of the build's 2 items resolve to no item of the database, among them {unknown:?}")),
        "names an item it could not look in: {}",
        error.what
    );
}

#[test]
fn a_reformat_changes_a_table_and_not_its_anchor() {
    let fx = fixture::db(SPANNED);
    let file = fx.only_file();
    let db = fx.into_db();
    let mut db = db;
    let file = file.file_id(&db);
    let owner = |db: &TestDB| DefWithBodyId::FunctionId(function(db, "f"));
    let before = positions(&db, owner(&db)).clone().unwrap();
    db.set_file_text(file, "\n\npub fn f(x: u32) -> u32 {\n        x + 1\n}\n");
    let after = positions(&db, owner(&db)).clone().unwrap();
    assert_eq!(before.starts, after.starts, "the same spans");
    assert_ne!(before.rows, after.rows, "each two lines on");
    assert!(before.rows.iter().zip(&after.rows).all(|(was, now)| now.1 == was.1 + 2));
}
