//! What a crate's `#[link]`s and a build's positions give the link, over fixtures.

#[path = "../tests/db.rs"]
mod db;

use blaze_codegen::{
    Arch, BuiltId, Changes, CompiledFunction, Delta, Line, NativeLib, NativeLibKind, driver::Built,
};
use blaze_format::SpanRef;
use blaze_ids::ContentId;
use blaze_items::{ItemId, item_key};
use hir_def::{DefWithBodyId, FunctionId, expr_store::Body as BodyStore};
use blaze_sim::{Config, Tape};
use hir_expand::fixture;
use hir_ty_mir::mir::MirSpan;

use super::*;
use crate::v2::{BuildTime, Databases, ItemHashes};
use db::TestDB;

/// A front end over `text`'s database, with the database's owner of what its links keep
/// ([`crate::v2::Walks`]), as a daemon's kept front end gives it.
fn frontend(text: &str) -> (&'static TestDB, HirFrontend<'static>) {
    let db: &'static TestDB = Box::leak(Box::new(fixture::db(text).into_db()));
    let walks: &'static crate::v2::Walks = Box::leak(Box::new(crate::v2::Walks::new(db)));
    let hashes = HashSource::Items(ItemHashes::new(Box::new(|_| Some("src/lib.rs".to_owned()))));
    (db, HirFrontend::new(Databases::new(db), hashes, BuildTime::default()).with_walks(walks))
}

#[track_caller]
fn function(db: &TestDB, name: &str) -> FunctionId {
    hir_def::test_items::function(db, fixture::test_crate(db), name)
}

fn lib(name: &str, kind: NativeLibKind) -> NativeLib {
    NativeLib { name: name.to_owned(), kind, verbatim: false }
}

/// A block's `wasm_import_module` names its imports' module, no library.
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
#[link(wasm_import_module = "blaze_proc_macro")]
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
        ("#[link(name = \"m\", modifiers = \"+whole-archive\")]\nextern \"C\" {}", "whole-archive"),
        ("#[link(name = \"-Wl,-z\", kind = \"link-arg\")]\nextern \"C\" {}", "kind = \"link-arg\""),
        ("#[link(name = \"m\", modifiers = \"+as-needed\")]\nextern \"C\" {}", "+as-needed"),
        ("#[link(name = \"m\", modifiers = \"-as-needed\")]\nextern \"C\" {}", "-as-needed"),
        (
            "#[link(name = \"m\", import_name_type = \"undecorated\")]\nextern \"C\" {}",
            "import_name_type",
        ),
        (
            "extern \"C\" {\n    #[link_ordinal(1)]\n    fn ordinal();\n}",
            "`#[link_ordinal(..)]` on `ordinal`",
        ),
        (
            "extern \"C\" {\n    #[link_ordinal(2)]\n    static ORDINAL: u8;\n}",
            "`#[link_ordinal(..)]` on `ORDINAL`",
        ),
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
        functions: vec![std::sync::Arc::new(compiled(
            1,
            vec![Line { at: 0, span: start }, Line { at: 1, span: declared }],
        ))],
        items: vec![item_key(db, ItemId::Function(func))],
        debug_names: vec![(InstanceKey(ContentId([1; 32])), "lib::f".to_owned())],
        ..Built::default()
    };
    let inputs = frontend.link_inputs(&built).unwrap();
    assert_eq!(inputs.base, None, "nothing was kept for this database: every table is given");
    assert!(inputs.work.whole.is_some());
    let mut anchors: Vec<ContentId> = inputs.tables.iter().map(|it| it.anchor).collect();
    assert!(anchors.is_sorted());
    anchors.sort_unstable();
    let mut want = vec![start.anchor, declared.anchor];
    want.sort_unstable();
    assert_eq!(anchors, want, "the two tables the lines name, no other");
    let whole = frontend.span_tables(&built).unwrap();
    let given: Vec<(ContentId, Vec<u8>)> =
        inputs.tables.iter().map(|it| (it.anchor, it.bytes.clone())).collect();
    assert_eq!(given, whole, "the tables made whole are the ones given");

    let table =
        |anchor: ContentId| &inputs.tables.iter().find(|it| it.anchor == anchor).unwrap().bytes;
    let body = table(start.anchor);
    let word = |bytes: &[u8], at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
    assert_eq!(&body[..8], b"BZSPANS2");
    assert_eq!(word(body, 8), 1, "one file");
    let name_len = word(body, 12) as usize;
    assert_eq!(&body[16..16 + name_len], b"src/lib.rs");
    let after = 16 + name_len;
    let count = word(body, after) as usize;
    assert_eq!(count, BodyStore::of(db, owner).exprs().count(), "a row per expression");
    let bases_at = after + 4 + 16 * count;
    assert_eq!(word(body, bases_at), 2, "the anchor's start and the owner's declaration");
    assert_eq!(body.len(), bases_at + 4 + 2 * 12);
    let row = after + 4 + 16 * start.index as usize;
    let [base, file, line, column] = std::array::from_fn(|n| word(body, row + 4 * n));
    assert_ne!(base, u32::MAX, "a span of the body's own file is counted from a base");
    assert_eq!(file, 0, "a relative span's file is its base's");
    let base_at = bases_at + 4 + 12 * base as usize;
    let [base_file, base_line, base_column] = std::array::from_fn(|n| word(body, base_at + 4 * n));
    let resolved =
        [base_file, base_line + line, if line == 0 { base_column + column } else { column }];
    // The body's block, on the first line, from its 25th column.
    assert_eq!(resolved, [0, 1, 25]);

    let declaration = table(declared.anchor);
    assert_eq!(declaration.len(), 8 + 4 + 4 + name_len + 4 + 16 + 4 + 12);

    // The same build linked again: the same snapshot, nothing made.
    let again = frontend.link_inputs(&built).unwrap();
    assert_eq!((again.serial, again.base), (inputs.serial, None));
    assert!(again.tables.is_empty() && again.removed.is_empty() && again.work.made == 0);
}

/// The invariant: a database's link inputs are kept by its owner ([`crate::v2::Walks`]) and by
/// nothing else, so they leave with it. The control is the front end given no owner: were
/// anything in the process (a static, as until 2026-11-13) keeping them, its second link
/// would find them and make nothing.
#[test]
fn link_inputs_are_kept_by_the_databases_owner_alone() {
    let (db, frontend) = frontend(SPANNED);
    let func = function(db, "f");
    let owner = DefWithBodyId::FunctionId(func);
    let root = BodyStore::of(db, owner).root_expr();
    let start = frontend.hashes.span(db, owner, MirSpan::ExprId(root)).unwrap();
    let built = Built {
        functions: vec![std::sync::Arc::new(compiled(1, vec![Line { at: 0, span: start }]))],
        items: vec![item_key(db, ItemId::Function(func))],
        debug_names: vec![(InstanceKey(ContentId([1; 32])), "lib::f".to_owned())],
        ..Built::default()
    };
    let first = frontend.link_inputs(&built).unwrap();
    assert!(first.work.whole.is_some());
    let again = frontend.link_inputs(&built).unwrap();
    assert!(again.work.whole.is_none() && again.work.made == 0, "the owner kept the first link's");

    // A front end with no owner, over the same database: nothing it links is kept anywhere.
    let hashes = HashSource::Items(ItemHashes::new(Box::new(|_| Some("src/lib.rs".to_owned()))));
    let unowned = HirFrontend::new(Databases::new(db), hashes, BuildTime::default());
    for link in 0..2 {
        let inputs = unowned.link_inputs(&built).unwrap();
        assert_eq!(
            inputs.work.whole,
            Some(NOTHING_KEPT),
            "link {link} of a front end with no owner found link inputs kept"
        );
    }

    // Let go by the owner: the next link makes every table again.
    let walks = frontend.walks.expect("given by `frontend`");
    assert_eq!(walks.links().let_go(), 1, "one program's link inputs were kept");
    assert_eq!(frontend.link_inputs(&built).unwrap().work.whole, Some(NOTHING_KEPT));
}

#[test]
fn a_position_of_no_table_the_build_has_is_an_error() {
    let (db, frontend) = frontend(SPANNED);
    let func = function(db, "f");
    let stray = SpanRef { anchor: ContentId([9; 32]), index: 0 };
    let unknown = ItemKey(ContentId([7; 32]));
    let built = Built {
        functions: vec![std::sync::Arc::new(compiled(1, vec![Line { at: 0, span: stray }]))],
        items: vec![item_key(db, ItemId::Function(func)), unknown],
        debug_names: vec![(InstanceKey(ContentId([1; 32])), "lib::f".to_owned())],
        ..Built::default()
    };
    let error = frontend.link_inputs(&built).unwrap_err();
    assert!(error.what.contains(&format!("{:?}", stray.anchor)), "{}", error.what);
    assert!(
        error.what.contains("a line row of the function"),
        "says what names it: {}",
        error.what
    );
    assert!(error.what.contains("`lib::f`"), "names the function: {}", error.what);
    assert!(
        error.what.contains(&format!(
            "1 of the build's 2 items resolve to no item of the database, among them {unknown:?}"
        )),
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
    let before = all_rows(&db, owner(&db));
    db.set_file_text(file, "\n\npub fn f(x: u32) -> u32 {\n        x + 1\n}\n");
    let after = all_rows(&db, owner(&db));
    assert_eq!(before.len(), after.len(), "the same spans");
    assert_ne!(before, after, "each two lines on");
    assert!(before.iter().zip(&after).all(|(was, now)| now.1 == was.1 + 2));
}

/// Every table's rows of `owner`, in table order ([`table_rows`]).
fn all_rows(db: &TestDB, owner: DefWithBodyId) -> Vec<Row> {
    (0..TABLES).flat_map(|table| table_rows(db, owner, table).unwrap()).collect()
}

const SHIFTED: &str = "pub fn above() -> u32 {\n    1\n}\npub fn f(x: u32) -> u32 {\n    let y = x + 1;\n    y * 2\n}\n";

/// The invariant: a line added above a body changes where its positions are counted from and
/// nothing else of them. Its places, the memo that reads its syntax, are kept and not run
/// again; its bases, two rows, are; each row the link is given is the one found from where
/// the body is in its file now, `N` lines on. The control: an edit of the body itself places
/// it again.
#[test]
fn a_line_added_above_a_body_places_none_of_it_again() {
    for n in [1usize, 3, 40] {
        let fx = fixture::db(SHIFTED);
        let file = fx.only_file();
        let mut db = fx.into_db();
        let file = file.file_id(&db);
        let owner = |db: &TestDB| DefWithBodyId::FunctionId(function(db, "f"));
        let before_rows = all_rows(&db, owner(&db));
        let before_places = span_places(&db, owner(&db)).clone();
        let cells_of = |db: &TestDB| -> Vec<(Vec<Cell>, Vec<Row>)> {
            (0..TABLES).map(|table| table_cells(db, owner(db), table).unwrap()).collect()
        };
        let before_cells = cells_of(&db);

        let shifted = format!("{}{SHIFTED}", "\n".repeat(n));
        db.set_file_text(file, &shifted);
        blaze_query::db::attach(&db, || {
            let owner = owner(&db);
            let mut after_rows = Vec::new();
            let executed = db.log_executed(|| after_rows = all_rows(&db, owner));
            assert!(executed.iter().any(|it| it == "span_bases"), "the bases move: {executed:#?}");
            assert!(
                !executed.iter().any(|it| it == "span_places"),
                "{n} lines above the body placed it again: {executed:#?}"
            );
            assert_eq!(*span_places(&db, owner), before_places, "its places are as they were");
            // What the link is given: each table's cells as they were, its bases `n` on.
            for (table, ((cells, bases), (was_cells, was_bases))) in
                cells_of(&db).into_iter().zip(&before_cells).enumerate()
            {
                assert_eq!(&cells, was_cells, "table {table}'s cells after {n} lines");
                let moved: Vec<Row> = (was_bases.iter())
                    .map(|&(file, line, column)| (file, line + n as u32, column))
                    .collect();
                assert_eq!(bases, moved, "table {table}'s bases after {n} lines");
            }
            assert_eq!(after_rows.len(), before_rows.len());
            for (was, now) in before_rows.iter().zip(&after_rows) {
                assert_eq!((now.0, now.1, now.2), (was.0, was.1 + n as u32, was.2), "{n} lines on");
            }
            // What the program prints: each row is the absolute one found from the whole file.
            let store = BodyStore::of(&db, owner);
            let starts = span_places(&db, owner).starts;
            let rows = &after_rows[starts[code::EXPRS] as usize..starts[code::EXPRS + 1] as usize];
            for (span, row) in store.expr_ids().map(MirSpan::ExprId).zip(rows) {
                assert_eq!(
                    Some(*row),
                    hir_ty_mir::mir::location::source_location_in_file(&db, owner, span),
                    "{span:?} after {n} lines"
                );
            }
        });

        // The control: the body's own edit places it again.
        db.set_file_text(file, &shifted.replace("x + 1", "x + 10"));
        blaze_query::db::attach(&db, || {
            let owner = owner(&db);
            let executed = db.log_executed(|| _ = all_rows(&db, owner));
            assert!(
                executed.iter().any(|it| it == "span_places"),
                "an edit of the body: {executed:#?}"
            );
        });
    }
}

const LINE_SHIFTED: &str = "#[rustc_builtin_macro]\nmacro_rules! line {() => {}}\npub fn above() -> u32 {\n    1\n}\npub fn f() -> u32 {\n    line!()\n}\n";

/// The line, from 1, of the first `line!()` in `text`.
fn line_of_call(text: &str) -> u32 {
    text[..text.find("line!()").expect("the call")].matches('\n').count() as u32 + 1
}

/// The invariant: a body that reads `line!()` keeps, below `n` lines added above it, its HIR
/// body (the call is the placeholder, not its line), its item key and its places; and the line
/// it reads is still the one it is written on: the row its `Location` is filled from (the cell
/// as it was, the base `n` on) and the value a constant takes of it
/// (`hir_ty::body_store::placed_literal`) are both the absolute line in the shifted file. The
/// control: that line is `n` more than before, so a body that kept a line would be caught.
#[test]
fn a_body_reading_its_line_keeps_its_hir_and_key_below_an_inserted_line() {
    use hir_def::{
        ExpressionStoreOwnerId,
        hir::{Expr, Literal},
    };
    for n in [1usize, 5, 60] {
        let fx = fixture::db(LINE_SHIFTED);
        let file = fx.only_file();
        let mut db = fx.into_db();
        let file = file.file_id(&db);
        let func = |db: &TestDB| function(db, "f");
        // What of the body a shift must keep, and where its `line!()` is read and placed.
        let seen = |db: &TestDB| {
            blaze_query::db::attach(db, || {
                let owner = DefWithBodyId::FunctionId(func(db));
                let store = BodyStore::of(db, owner);
                let (expr, literal) = (store.exprs())
                    .find_map(|(id, it)| match it {
                        Expr::Literal(it @ Literal::Uint(..)) => Some((id, it.clone())), // clone: a test's comparison
                        _ => None,
                    })
                    .expect("the literal `line!()` expands to");
                let placed = hir_ty::body_store::placed_literal(
                    db,
                    ExpressionStoreOwnerId::Body(owner),
                    expr,
                )
                .expect("the call is in its file's text");
                let row = hir_ty_mir::mir::location::source_location_in_file(
                    db,
                    owner,
                    MirSpan::ExprId(expr),
                )
                .expect("the literal has a place");
                let starts = span_places(db, owner).starts;
                let rows = all_rows(db, owner);
                // The expressions' table is in `expr_ids` order, as the test above reads it.
                let index = store.expr_ids().position(|it| it == expr).expect("the literal");
                let linked = rows[starts[code::EXPRS] as usize + index];
                (
                    format!("{store:?}"),
                    item_key(db, ItemId::Function(func(db))),
                    span_places(db, owner).clone(), // clone: a test's comparison
                    literal,
                    placed,
                    row,
                    linked,
                )
            })
        };
        let (body, key, places, literal, placed, row, linked) = seen(&db);
        let written = line_of_call(LINE_SHIFTED);
        assert_eq!(
            literal,
            Literal::Uint(u128::from(hir_expand::builtin::PLACE_PLACEHOLDER), Some(hir_def::builtin_type::BuiltinUint::U32)),
            "`line!()` is the placeholder in the body"
        );
        assert_eq!(placed, Some(Literal::Uint(u128::from(written), Some(hir_def::builtin_type::BuiltinUint::U32))));
        assert_eq!((row.1, linked.1), (written, written), "the row a `Location` is filled from");

        let shifted =
            LINE_SHIFTED.replacen("pub fn above", &format!("{}pub fn above", "\n".repeat(n)), 1);
        db.set_file_text(file, &shifted);
        let (body2, key2, places2, literal2, placed2, row2, linked2) = seen(&db);
        let now = line_of_call(&shifted);
        assert_eq!(now, written + n as u32, "the control: the call moved {n} lines");
        assert_eq!(body2, body, "the HIR body after {n} lines");
        assert_eq!(key2, key, "the item key after {n} lines");
        assert_eq!(places2, places, "the places after {n} lines");
        assert_eq!(literal2, literal, "the placeholder after {n} lines");
        assert_eq!(
            placed2,
            Some(Literal::Uint(u128::from(now), Some(hir_def::builtin_type::BuiltinUint::U32))),
            "a constant's value after {n} lines"
        );
        assert_eq!((row2.1, linked2.1), (now, now), "the printed line after {n} lines");
    }
}

/// A body's positions are found by plain functions over its memos: each span's is the one
/// found from where the body is in its file, and placing them makes no memo per span.
#[test]
fn positions_are_placed_without_a_memo_per_span() {
    let fx = fixture::db(SPANNED);
    let _ = fx.only_file();
    let db = fx.into_db();
    blaze_query::db::attach(&db, || {
        let owner = DefWithBodyId::FunctionId(function(&db, "f"));
        let mut rows = Vec::new();
        let executed = db.log_executed(|| rows = all_rows(&db, owner));
        assert!(
            executed.iter().any(|it| it == "span_places"),
            "the log sees queries: {executed:#?}"
        );
        for per_span in
            ["source_place", "row_place", "source_location_in_file", "row_location_in_file"]
        {
            assert!(
                !executed.iter().any(|it| it == per_span),
                "a memo of {per_span}: {executed:#?}"
            );
        }

        let starts = span_places(&db, owner).starts;
        let table = |t: usize| &rows[starts[t] as usize..starts[t + 1] as usize];
        let store = BodyStore::of(&db, owner);
        let in_file =
            |span: MirSpan| hir_ty_mir::mir::location::source_location_in_file(&db, owner, span);
        let exprs: Vec<MirSpan> = store.expr_ids().map(MirSpan::ExprId).collect();
        let pats: Vec<MirSpan> = store.pat_ids().map(MirSpan::PatId).collect();
        for (spans, rows) in [(exprs, table(code::EXPRS)), (pats, table(code::PATS))] {
            assert_eq!(spans.len(), rows.len());
            for (span, row) in spans.into_iter().zip(rows) {
                assert_eq!(Some(*row), in_file(span), "{span:?}");
            }
        }
        let root = store.expr_ids().position(|it| it == store.root_expr()).expect("the root");
        // The body's block, on the first line, from its 25th column.
        assert_eq!((table(code::EXPRS)[root].1, table(code::EXPRS)[root].2), (1, 25));
    });
}

/// A program of [`Program::COUNT`] functions in one file, as a sequence of edits leaves it.
#[derive(Clone, Debug)]
struct Program {
    /// Blank lines before the first function: every position below moves with them.
    pad: usize,
    /// Per function: how many times its body was edited (each edit adds an expression, and
    /// gives it another key), whether the build has it, and whether its code names its
    /// patterns' table too (which changes what it is under the same key).
    bodies: Vec<(u8, bool, bool)>,
    /// How many of the functions, the first, are in the crate's root file, which the edits
    /// write; the others are in the file of a module `rest`, which no edit writes.
    near: usize,
}

/// One step of a program's life.
#[derive(Clone, Copy, Debug)]
enum Edit {
    /// A blank line more at the top: positions move, no code changes.
    Reformat,
    Body(usize),
    Drop(usize),
    Add(usize),
    /// The function names one table more or one fewer under its key.
    Patterns(usize),
    /// A build of the program as it is.
    Again,
}

impl Program {
    fn new(count: usize) -> Program {
        Program { pad: 0, bodies: vec![(0, true, false); count], near: count }
    }

    /// `count` functions, the first `near` in the root file and the others in another.
    fn split(count: usize, near: usize) -> Program {
        Program { near, ..Program::new(count) }
    }

    fn functions(&self, of: std::ops::Range<usize>) -> String {
        let mut text = String::new();
        for at in of {
            let body = "x + 1".to_owned() + &" + 1".repeat(self.bodies[at].0 as usize);
            text += &format!("pub fn f{at}(x: u32) -> u32 {{\n    {body}\n}}\n");
        }
        text
    }

    /// The root file's text: the file the edits write.
    fn text(&self) -> String {
        let mut text = "\n".repeat(self.pad) + &self.functions(0..self.near);
        if self.near < self.bodies.len() {
            text += "pub mod rest;\n";
        }
        text
    }

    /// The fixture of the program: its root file, and the other file where it has one.
    fn fixture(&self) -> String {
        match self.near < self.bodies.len() {
            true => {
                let rest = self.functions(self.near..self.bodies.len());
                format!("//- /lib.rs\n{}//- /rest.rs\n{rest}", self.text())
            }
            false => self.text(),
        }
    }

    fn apply(&mut self, edit: Edit) {
        match edit {
            Edit::Reformat => self.pad += 1,
            Edit::Body(at) => self.bodies[at].0 += 1,
            Edit::Drop(at) => self.bodies[at].1 = false,
            Edit::Add(at) => self.bodies[at].1 = true,
            Edit::Patterns(at) => self.bodies[at].2 ^= true,
            Edit::Again => {}
        }
    }

    fn key(&self, at: usize) -> InstanceKey {
        let mut key = [0; 32];
        key[..8].copy_from_slice(&(at as u64 + 1).to_be_bytes());
        key[8] = self.bodies[at].0;
        InstanceKey(ContentId(key))
    }

    /// What the build says of each function it has, by key: its key and whether it names
    /// its patterns' table.
    fn entries(&self) -> Vec<(InstanceKey, bool)> {
        let mut entries: Vec<(InstanceKey, bool)> = (0..self.bodies.len())
            .filter(|at| self.bodies[*at].1)
            .map(|at| (self.key(at), self.bodies[at].2))
            .collect();
        entries.sort_unstable();
        entries
    }

    /// The build of the program over `db`, as a change of the build `before` left
    /// (its id and entries).
    fn built(
        &self,
        db: &TestDB,
        hashes: &HashSource,
        before: Option<&(BuiltId, Vec<(InstanceKey, bool)>)>,
    ) -> Built {
        let mut functions = Vec::new();
        let mut items = Vec::new();
        for at in (0..self.bodies.len()).filter(|at| self.bodies[*at].1) {
            let func = function(db, &format!("f{at}"));
            let owner = DefWithBodyId::FunctionId(func);
            let store = BodyStore::of(db, owner);
            let mut lines = vec![
                Line { at: 0, span: hashes.span(db, owner, MirSpan::ExprId(store.root_expr())).unwrap() },
                Line { at: 1, span: hashes.declaration(db, func).unwrap() },
            ];
            if self.bodies[at].2 {
                let pat = store.pat_ids().next().expect("the parameter's pattern");
                lines.push(Line { at: 2, span: hashes.span(db, owner, MirSpan::PatId(pat)).unwrap() });
            }
            let mut compiled = compiled(0, lines);
            compiled.key = self.key(at);
            functions.push(std::sync::Arc::new(compiled));
            items.push(item_key(db, ItemId::Function(func)));
        }
        functions.sort_unstable_by_key(|it| it.key);
        items.sort_unstable();
        let entries = self.entries();
        let mut bytes = Vec::new();
        for (key, patterns) in &entries {
            bytes.extend_from_slice(&key.0.0);
            bytes.push(u8::from(*patterns));
        }
        let id = BuiltId(ContentId::of("linked test built", &bytes));
        let delta = match before {
            None => Delta::default(),
            Some((base, was)) => {
                let mut functions = Changes::default();
                for (key, patterns) in &entries {
                    match was.iter().find(|it| it.0 == *key) {
                        None => functions.added.push(*key),
                        Some(it) if it.1 != *patterns => functions.changed.push(*key),
                        Some(_) => {}
                    }
                }
                for (key, _) in was {
                    if !entries.iter().any(|it| it.0 == *key) {
                        functions.removed.push(*key);
                    }
                }
                Delta { base: Some(*base), functions, ..Delta::default() }
            }
        };
        Built { functions, items, id, delta, ..Built::default() }
    }
}

/// `edits` applied to a program, each followed by a build and its link inputs; what the last
/// link's inputs cost. `Err` names the first step after which the tables kept by applying
/// each link's changes are not the tables made whole from the build. `plant` is a control's
/// bug ([`Plant`]).
fn kept_is_whole(count: usize, edits: &[Edit], plant: Plant) -> Result<InputsWork, String> {
    kept_is_whole_of(Program::new(count), edits, plant)
}

/// [`kept_is_whole`] of `program`.
fn kept_is_whole_of(mut program: Program, edits: &[Edit], plant: Plant) -> Result<InputsWork, String> {
    let fx = fixture::db(&program.fixture());
    let file = fx.file(0);
    let mut db = fx.into_db();
    let file = file.file_id(&db);
    let hashes = || HashSource::Items(ItemHashes::new(Box::new(|_| Some("src/lib.rs".to_owned()))));
    // What the database's links keep, owned beside it for every step, as a daemon's kept
    // front end owns it.
    let links = Links::new();
    let mut before: Option<(BuiltId, Vec<(InstanceKey, bool)>)> = None;
    // The tables a link that applies each change holds, and the snapshot they are.
    let mut held: std::collections::BTreeMap<ContentId, Vec<u8>> = Default::default();
    let mut serial = 0;
    let mut work = InputsWork::default();
    let steps = std::iter::once(Edit::Again).chain(edits.iter().copied());
    for (step, edit) in steps.enumerate() {
        program.apply(edit);
        db.set_file_text(file, &program.text());
        let frontend = HirFrontend::new(Databases::new(&db), hashes(), BuildTime::default());
        // A build walks the engine's changed set before its link (its body check does), so
        // each walk is of one build's writes: without it the first link's walk is of
        // everything written since the database was made, a wide level that names every row.
        blaze_query::db::plumbing::walk_changed_set(&db);
        let built = program.built(&db, &frontend.hashes, before.as_ref());
        let given = frontend.with(|db| inputs(db, &frontend.hashes, &built, plant, &links))?;
        if given.serial != serial {
            if given.base != Some(serial) {
                if given.base.is_some() {
                    return Err(format!("step {step} ({edit:?}): a change of a snapshot never given"));
                }
                held.clear();
            }
            for anchor in &given.removed {
                held.remove(anchor);
            }
            for table in &given.tables {
                assert_eq!(table.id, ContentId::of(TABLE_DOMAIN, &table.bytes));
                held.insert(table.anchor, table.bytes.clone());
            }
            serial = given.serial;
        } else if !given.tables.is_empty() || !given.removed.is_empty() {
            return Err(format!("step {step} ({edit:?}): one snapshot given two ways"));
        }
        let whole = frontend.span_tables(&built).map_err(|it| it.what)?;
        let kept: Vec<(ContentId, Vec<u8>)> = held.iter().map(|(k, v)| (*k, v.clone())).collect();
        if kept != whole {
            let differ = whole.iter().filter(|it| held.get(&it.0) != Some(&it.1)).count();
            return Err(format!(
                "after step {step} ({edit:?}): {} tables kept, {} made whole, {differ} of them other than kept",
                kept.len(),
                whole.len()
            ));
        }
        work = given.work;
        before = Some((built.id, program.entries()));
    }
    Ok(work)
}

fn edit(tape: &Tape) -> Edit {
    let at = tape.int_in(0..=3usize, "at");
    match tape.int_in(0..=5u8, "edit") {
        0 => Edit::Reformat,
        1 => Edit::Body(at),
        2 => Edit::Drop(at),
        3 => Edit::Add(at),
        4 => Edit::Patterns(at),
        _ => Edit::Again,
    }
}

/// The refinement: for any program and sequence of edits (a reformat, a body edited, a
/// function gone or back, a function that names another table), the span tables a link holds
/// by applying each build's link inputs to those of the link before are the tables made
/// whole from the build, with nothing kept.
#[test]
fn the_link_inputs_kept_are_the_tables_made_whole() {
    blaze_sim::check(Config::DEFAULT, |tape| {
        let edits: Vec<Edit> = tape.vec(0..=8, "edits", edit);
        if let Err(why) = kept_is_whole(4, &edits, Plant::None) {
            panic!("{why}");
        }
    })
}

/// A control of [`the_link_inputs_kept_are_the_tables_made_whole`]: with a link that does
/// not ask the engine which bodies' positions changed, a reformat leaves every table on its
/// old lines, and the oracle says at which step.
#[test]
fn the_oracle_catches_a_moved_body_not_encoded_again() {
    let edits = [Edit::Reformat];
    kept_is_whole(4, &edits, Plant::None).unwrap();
    let why = kept_is_whole(4, &edits, Plant::Unasked).expect_err("the oracle missed tables left stale");
    assert!(why.contains("after step 1"), "{why}");
}

/// A function dropped takes its tables with it, and one back brings them: the anchors
/// gone are said, so a link holds no table the code does not name.
#[test]
fn a_function_gone_takes_its_tables() {
    let edits = [Edit::Drop(2), Edit::Patterns(0), Edit::Add(2), Edit::Patterns(0)];
    kept_is_whole(4, &edits, Plant::None).unwrap();
}

/// The functions in the file the counter edits ([`Program::split`]): few enough that the
/// engine's walk of the edit verifies each row the file's text reaches. A file whose rows
/// are a wide level of that walk (64 rows) is named whole, its unchanged bodies with it, and
/// the link then encodes that file's kept tables again and finds them the same: the file's
/// cost, never the program's.
const NEAR: usize = 8;

/// What one body edit among `count` functions costs the link inputs, as the counter holds
/// it: tables encoded, their bytes, functions read, and owners whose kept tables were encoded
/// again. The edited function's file holds [`NEAR`] of the functions.
fn body_edit_cost(count: usize, plant: Plant) -> (usize, usize, usize, usize) {
    let work = kept_is_whole_of(Program::split(count, NEAR), &[Edit::Body(7)], plant).unwrap();
    assert_eq!(work.whole, None, "{count} functions: the link before was kept");
    assert_eq!((work.items, work.scanned), (0, 0), "{count} functions: no item is new");
    assert_eq!(work.functions, 1, "{count} functions: one function has a new key");
    (work.made, work.bytes, work.functions, work.verified)
}

/// The counter: after one body edit, the link inputs resolve no item, read the one function
/// whose key is new, encode that body's tables, and encode again the kept tables of the
/// owners the engine names as moved, among 100 functions as among 2,000 (a fixture of 10,000
/// bodies takes minutes to lower). Every body of this fixture is the workspace's and watched;
/// none is asked of the database for being watched, and none of another file than the
/// edit's is looked at.
#[test]
fn a_body_edit_among_many_makes_its_own_tables() {
    let costs = [100, 2_000].map(|count| body_edit_cost(count, Plant::None));
    let (made, bytes, _, verified) = costs[0];
    assert!(made <= 4 && bytes <= 2048 && verified <= 2, "100 functions: {:?}", costs[0]);
    assert_eq!(costs[0], costs[1], "the same work among 100 functions as among 2,000");
}

/// The control of [`a_body_edit_among_many_makes_its_own_tables`]: a link that takes every
/// watched owner as moved (what it did while it asked a stamp of each) gives the right
/// tables, and the counter says its work follows the program.
#[test]
fn the_oracle_catches_a_link_that_asks_of_every_body() {
    let costs = [100, 2_000].map(|count| body_edit_cost(count, Plant::Sweeps));
    assert_ne!(costs[0], costs[1], "a sweep of every watched owner was counted as the edit's");
    assert!(costs[1].3 >= 2_000, "2,000 functions: {:?}", costs[1]);
}
