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
use hegel::generators as gs;
use hir_expand::fixture;
use hir_ty_mir::mir::MirSpan;

use super::*;
use crate::v2::{BuildTime, Databases, ItemHashes};
use db::TestDB;

fn frontend(text: &str) -> (&'static TestDB, HirFrontend<'static>) {
    let db: &'static TestDB = Box::leak(Box::new(fixture::db(text).into_db()));
    let hashes = HashSource::Items(ItemHashes::new(Box::new(|_| Some("src/lib.rs".to_owned()))));
    (db, HirFrontend::new(Databases::new(db), hashes, BuildTime::default()))
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
        functions: vec![compiled(
            1,
            vec![Line { at: 0, span: start }, Line { at: 1, span: declared }],
        )],
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
    assert_eq!(&body[..8], b"BZSPANS1");
    assert_eq!(u32::from_le_bytes(body[8..12].try_into().unwrap()), 1, "one file");
    let name_len = u32::from_le_bytes(body[12..16].try_into().unwrap()) as usize;
    assert_eq!(&body[16..16 + name_len], b"src/lib.rs");
    let after = 16 + name_len;
    let count = u32::from_le_bytes(body[after..after + 4].try_into().unwrap()) as usize;
    assert_eq!(count, BodyStore::of(db, owner).exprs().count(), "a row per expression");
    assert_eq!(body.len(), after + 4 + 12 * count);
    let row = after + 4 + 12 * start.index as usize;
    let words: Vec<u32> = body[row..row + 12]
        .chunks(4)
        .map(|it| u32::from_le_bytes(it.try_into().unwrap()))
        .collect();
    // The body's block, on the first line, from its 25th column.
    assert_eq!(words, [0, 1, 25]);

    let declaration = table(declared.anchor);
    assert_eq!(declaration.len(), 8 + 4 + 4 + name_len + 4 + 12);

    // The same build linked again: the same snapshot, nothing made.
    let again = frontend.link_inputs(&built).unwrap();
    assert_eq!((again.serial, again.base), (inputs.serial, None));
    assert!(again.tables.is_empty() && again.removed.is_empty() && again.work.made == 0);
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
    let before = positions(&db, owner(&db)).clone().unwrap();
    db.set_file_text(file, "\n\npub fn f(x: u32) -> u32 {\n        x + 1\n}\n");
    let after = positions(&db, owner(&db)).clone().unwrap();
    assert_eq!(before.starts, after.starts, "the same spans");
    assert_ne!(before.rows, after.rows, "each two lines on");
    assert!(before.rows.iter().zip(&after.rows).all(|(was, now)| now.1 == was.1 + 2));
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
        let mut found = None;
        let executed = db.log_executed(|| found = Some(positions(&db, owner).clone().unwrap()));
        let found = found.expect("the positions");
        assert!(executed.iter().any(|it| it == "positions"), "the log sees queries: {executed:#?}");
        for per_span in
            ["source_place", "row_place", "source_location_in_file", "row_location_in_file"]
        {
            assert!(
                !executed.iter().any(|it| it == per_span),
                "a memo of {per_span}: {executed:#?}"
            );
        }

        let table = |t: usize| &found.rows[found.starts[t] as usize..found.starts[t + 1] as usize];
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
        Program { pad: 0, bodies: vec![(0, true, false); count] }
    }

    fn text(&self) -> String {
        let mut text = "\n".repeat(self.pad);
        for (at, (edits, _, _)) in self.bodies.iter().enumerate() {
            let body = "x + 1".to_owned() + &" + 1".repeat(*edits as usize);
            text += &format!("pub fn f{at}(x: u32) -> u32 {{\n    {body}\n}}\n");
        }
        text
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
            functions.push(compiled);
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
/// each link's changes are not the tables made whole from the build. `plant` skips asking
/// the database for the positions of the bodies kept.
fn kept_is_whole(count: usize, edits: &[Edit], plant: bool) -> Result<InputsWork, String> {
    let mut program = Program::new(count);
    let fx = fixture::db(&program.text());
    let file = fx.only_file();
    let mut db = fx.into_db();
    let file = file.file_id(&db);
    let hashes = || HashSource::Items(ItemHashes::new(Box::new(|_| Some("src/lib.rs".to_owned()))));
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
        let built = program.built(&db, &frontend.hashes, before.as_ref());
        let given = frontend.with(|db| inputs(db, &frontend.hashes, &built, plant))?;
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

#[hegel::composite]
fn edit(tc: &hegel::TestCase) -> Edit {
    let at = tc.draw(gs::integers::<usize>().max_value(3));
    match tc.draw(gs::integers::<u8>().max_value(5)) {
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
#[hegel::test]
fn the_link_inputs_kept_are_the_tables_made_whole(tc: hegel::TestCase) {
    let edits: Vec<Edit> = tc.draw(gs::vecs(edit()).max_size(8));
    if let Err(why) = kept_is_whole(4, &edits, false) {
        panic!("{why}");
    }
}

/// A control of [`the_link_inputs_kept_are_the_tables_made_whole`]: with a link that does
/// not ask the database for the positions of the bodies it kept, a reformat leaves every
/// table on its old lines, and the oracle says at which step.
#[test]
fn the_oracle_catches_a_moved_body_not_encoded_again() {
    let edits = [Edit::Reformat];
    kept_is_whole(4, &edits, false).unwrap();
    let why = kept_is_whole(4, &edits, true).expect_err("the oracle missed tables left stale");
    assert!(why.contains("after step 1"), "{why}");
}

/// A function dropped takes its tables with it, and one back brings them: the anchors
/// gone are said, so a link holds no table the code does not name.
#[test]
fn a_function_gone_takes_its_tables() {
    kept_is_whole(4, &[Edit::Drop(2), Edit::Patterns(0), Edit::Add(2), Edit::Patterns(0)], false)
        .unwrap();
}

/// The counter: after one body edit, the link inputs resolve no item, read the one function
/// whose key is new and encode that body's tables, among 100 functions as among 2,000 (a
/// fixture of 10,000 bodies takes minutes to lower). What still follows the program here is
/// `verified`: every body of this fixture is the workspace's, so each is asked of the
/// database again; a library's are not (`Kept::durable`), which no fixture crate is.
#[test]
fn a_body_edit_among_many_makes_its_own_tables() {
    let mut costs = Vec::new();
    for count in [100, 2_000] {
        let work = kept_is_whole(count, &[Edit::Body(7)], false).unwrap();
        assert_eq!(work.whole, None, "{count} functions: the link before was kept");
        assert_eq!((work.items, work.scanned), (0, 0), "{count} functions: no item is new");
        assert_eq!(work.functions, 1, "{count} functions: one function has a new key");
        assert!(work.made <= 2 && work.bytes <= 1024, "{count} functions: {work:?}");
        assert_eq!(work.verified, 2 * count, "each body and declaration of the workspace");
        costs.push((work.made, work.bytes, work.functions));
    }
    assert_eq!(costs[0], costs[1], "the same work among 100 functions as among 2,000");
}
