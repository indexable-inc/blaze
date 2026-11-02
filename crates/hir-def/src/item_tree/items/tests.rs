use std::{
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};

use base_db::EditionedFileId;
use blaze_ids::rng::SplitMix64;
use syntax::view::HasModuleItem;
use hir_expand::fixture;

use crate::{
    item_tree::{ItemTree, lower_whole, tests::assert_assembled_is_whole},
    test_db::TestDB,
};

/// Real files with nested and outline modules, `use` trees, macros, `extern` blocks, `impl`s
/// and attributes: the sources of the item tree, of name resolution and of the syntax tree.
fn corpus() -> Vec<(String, String)> {
    let root = test_utils::project_root().into_std_path_buf();
    let mut files = Vec::new();
    for dir in
        ["crates/hir-def/src/item_tree", "crates/hir-def/src/nameres", "crates/syntax/src/ast"]
    {
        collect_rs(&root.join(dir), &mut files);
    }
    files.sort();
    assert!(files.len() > 20, "the sources of the item tree, name resolution and the syntax tree");
    (files.into_iter())
        .map(|path| {
            let text = std::fs::read_to_string(&path).expect("a source file");
            (path.display().to_string(), text)
        })
        .collect()
}

fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("a directory of the tree") {
        let path = entry.expect("a directory entry").path();
        if path.is_dir() {
            collect_rs(&path, out);
        } else if path.extension().is_some_and(|it| it == "rs") {
            out.push(path);
        }
    }
}

/// A database of one file, the crate's root, holding `text`.
fn db_of(text: &str) -> (TestDB, EditionedFileId) {
    let fx = fixture::db("");
    let file = fx.only_file();
    let mut db = fx.into_db();
    db.set_file_text(file.file_id(&db), text);
    (db, file)
}

fn assembled(db: &TestDB, file: EditionedFileId) -> Arc<ItemTree> {
    super::file_item_tree(db, file, fixture::test_crate(db))
}

#[test]
fn real_files_assemble_to_their_whole_trees() {
    let fx = fixture::db("");
    let file = fx.only_file();
    let mut db = fx.into_db();
    for (path, text) in &corpus() {
        db.set_file_text(file.file_id(&db), text);
        assert_assembled_is_whole(&db, file, path);
    }
}

#[rustfmt::skip]
const INSERTS: &[&str] = &[
    "", " ", "\n", "{", "}", "(", ")", ";", ",", "x", "::", "#[a]", "#![a]", "#[cfg(false)] ",
    "#![cfg(false)]\n", "/// d\n", "//! d\n", "pub ", "pub(crate) ", "pub(in crate::m) ",
    "fn f() {}", "fn g() { #![inline] 1 }", "mod m {}", "mod m;", "mod n { pub fn i() {} }",
    "struct S(pub u8, u16);", "struct R { a: u8 }", "enum E { A }", "union U { a: u8 }",
    "use a::{b, c as d, e::*};", "use self::x as _;", "extern crate core as c;",
    "impl S { fn g() {} }", "impl T for S {}", "trait T { fn t(); }", "type A = u8;",
    "static X: u8 = 0;", "const _: () = ();", "m!();", "m! { x }", "macro_rules! m { () => {} }",
    "macro m() {}", "extern \"C\" { fn e(); static E: u8; m!(); }", "let x = 1;",
];

/// An edit of `text`: a range of it and what replaces it, from [`INSERTS`] or `text` itself.
fn random_edit(rng: &mut SplitMix64, text: &str) -> (Range<usize>, String) {
    let boundary = |mut at: usize| {
        while !text.is_char_boundary(at) {
            at -= 1;
        }
        at
    };
    let start = boundary(rng.index(text.len() + 1));
    let end = boundary((start + rng.index(12)).min(text.len())).max(start);
    let insert = match rng.below(3) {
        0 => {
            let from = boundary(rng.index(text.len() + 1));
            let to = boundary((from + rng.index(48)).min(text.len())).max(from);
            text[from..to].to_owned()
        }
        _ => INSERTS[rng.index(INSERTS.len())].to_owned(),
    };
    (start..end, insert)
}

/// After each of a run of edits, the tree a database assembles from the items it kept and the
/// items it lowered again is the tree a fresh database lowers the text to whole.
#[test]
fn random_edits_assemble_to_a_fresh_whole_tree() {
    let mut rng = SplitMix64::new(0x2545_f491_4f6c_dd1d);
    let fx = fixture::db("");
    let file = fx.only_file();
    let mut db = fx.into_db();
    for (path, text) in corpus().iter().step_by(2) {
        let mut text = text.clone();
        db.set_file_text(file.file_id(&db), &text);
        assembled(&db, file);
        for _ in 0..6 {
            let (range, insert) = random_edit(&mut rng, &text);
            text.replace_range(range.clone(), &insert);
            db.set_file_text(file.file_id(&db), &text);
            let (fresh, fresh_file) = db_of(&text);
            let edited = assembled(&db, file);
            let whole = lower_whole(&fresh, fresh_file.into(), fixture::test_crate(&fresh));
            assert_eq!(*edited, whole, "{path} after replacing {range:?} with {insert:?}");
            assert_eq!(
                blaze_format::to_bytes(&*edited),
                blaze_format::to_bytes(&whole),
                "{path} after replacing {range:?} with {insert:?}"
            );
        }
    }
}

const ITEMS: &str = r#"
use std::fmt;
pub struct S(pub u8);
pub fn f(s: S) -> u8 { s.0 }
impl S {
    fn g(&self) -> u8 { 1 }
    fn h(&self) {}
}
pub trait T { fn t(&self) { let x = 1; } }
const C: u8 = { 1 + 1 };
mod m { pub fn inner() {} }
"#;

/// How many top-level items making the file's tree lowers once it holds `text`, whole or
/// alone. Before it, an edit that changes no item (a newline at the end) has the file lowered
/// whole and keyed, as a file's first edit is, so what is counted is an edit of a keyed file.
/// Counted from before the write: the write's name resolution maintainer asks for the tree.
fn items_lowered_by(db: &mut TestDB, file: EditionedFileId, text: &str) -> usize {
    let krate = fixture::test_crate(db);
    let file_id = file.file_id(&*db);
    crate::file_item_tree(&*db, file.into(), krate);
    let before = file.parse(&*db).root().text().to_string();
    db.set_file_text(file_id, &format!("{before}\n"));
    crate::file_item_tree(&*db, file.into(), krate);
    super::LOWERED.set(0);
    db.set_file_text(file_id, text);
    crate::file_item_tree(&*db, file.into(), krate);
    let lowered = super::LOWERED.get();
    assert_assembled_is_whole(db, file, text);
    lowered
}

#[test]
fn an_edit_inside_bodies_lowers_no_item() {
    let (mut db, file) = db_of(ITEMS);
    let mut text = ITEMS.to_owned();
    for (from, to) in [
        ("{ s.0 }", "{ let y = s.0 + 1; y * 2 }"),
        ("{ 1 }", "{\n        let z = [1, 2];\n        z.len() as u8\n    }"),
        ("fn h(&self) {}", "fn h(&self) { loop {} }"),
        // The attributes of an `impl`'s `fn` are the `fn`'s, not the `impl`'s.
        ("{ loop {} }", "{ #![inline] loop {} }"),
        ("let x = 1;", "let x = 2; x"),
        ("{ 1 + 1 }", "{ 2 }"),
    ] {
        text = text.replace(from, to);
        assert_eq!(items_lowered_by(&mut db, file, &text), 0, "after {to}");
    }
}

/// An edit that changes no item gives back the kept tree itself: the query database sees the same value.
#[test]
fn an_edit_inside_a_body_gives_the_kept_tree() {
    let (mut db, file) = db_of(ITEMS);
    let krate = fixture::test_crate(&db);
    items_lowered_by(&mut db, file, &ITEMS.replace("{ s.0 }", "{ s.0 + 1 }"));
    let kept = super::file_item_tree(&db, file, krate);
    let again = super::file_item_tree(&db, file, krate);
    assert!(Arc::ptr_eq(&kept, &again));
}

#[test]
fn an_edit_of_one_item_lowers_it_alone() {
    for (from, to) in [
        ("pub fn f(", "pub fn renamed("),
        ("pub struct S(pub u8)", "pub struct S(u8)"),
        ("impl S {", "impl fmt::Debug for S {"),
        ("const C", "pub const C"),
        ("pub trait T", "struct New;\npub trait T"),
        ("pub fn inner", "fn inner"),
        ("pub fn f(s: S) -> u8 { s.0 }", "pub fn f(s: S) -> u8 {\n    #![allow(x)]\n    s.0\n}"),
    ] {
        let (mut db, file) = db_of(ITEMS);
        let edited = ITEMS.replace(from, to);
        assert_eq!(items_lowered_by(&mut db, file, &edited), 1, "after {to}");
    }
}

/// Removing an item lowers none, and an item moved by another's removal keeps its rows.
#[test]
fn removing_an_item_lowers_none() {
    let (mut db, file) = db_of(ITEMS);
    let edited = ITEMS.replace("pub struct S(pub u8);\n", "");
    assert_eq!(items_lowered_by(&mut db, file, &edited), 0);
}

#[test]
fn a_big_item_keeps_its_tree_across_an_edit_elsewhere() {
    let rules: String = (0..300).map(|i| format!("    (a{i}) => {{ {i} }};\n")).collect();
    let text = format!("macro_rules! big {{\n{rules}}}\nfn f() {{}}\nstruct S;\n");
    assert!(text.len() > 4096 + 32);
    let (mut db, file) = db_of(&text);
    crate::file_item_tree(&db, file.into(), fixture::test_crate(&db));
    assert_eq!(items_lowered_by(&mut db, file, &text.replace("struct S;", "struct T;")), 1);
    let renamed = text.replace("struct S;", "struct T;").replace("(a7)", "(b7)");
    assert_eq!(items_lowered_by(&mut db, file, &renamed), 1);
}

/// How many top-level items' syntax making the file's tree hashes once it holds `text`, and how
/// many items' ast ids and anchors it reads from the maps, the file keyed at the text it holds
/// before. Counted from before the write, as [`items_lowered_by`] is.
fn items_keyed_by(db: &mut TestDB, file: EditionedFileId, text: &str) -> (usize, usize) {
    let krate = fixture::test_crate(db);
    let file_id = file.file_id(&*db);
    let before = file.parse(&*db).root().text().to_string();
    for warm in [format!("{before}\n"), before] {
        db.set_file_text(file_id, &warm);
        crate::file_item_tree(&*db, file.into(), krate);
    }
    super::HASHED.set(0);
    super::READ.set(0);
    db.set_file_text(file_id, text);
    crate::file_item_tree(&*db, file.into(), krate);
    let counts = (super::HASHED.get(), super::READ.get());
    assert_assembled_is_whole(db, file, text);
    counts
}

fn items_hashed_by(db: &mut TestDB, file: EditionedFileId, text: &str) -> usize {
    items_keyed_by(db, file, text).0
}

#[test]
fn an_edit_hashes_only_the_items_it_touched() {
    let items: String =
        (0..40).map(|i| format!("pub fn f{i}(a: u8) -> u8 {{ a + {i} }}\n")).collect();
    let (mut db, file) = db_of(&items);
    // The item edited, and the two before it, past which the parser may have looked.
    let edited = items.replace("{ a + 20 }", "{ a * 20 }");
    assert_eq!(items_hashed_by(&mut db, file, &edited), 3);
    // The item added at the end, and the two before it.
    let appended = format!("{edited}struct S;\n");
    assert_eq!(items_hashed_by(&mut db, file, &appended), 3);
    // With the first item gone, the second begins where the text was the first's.
    let removed = appended.replacen("pub fn f0(a: u8) -> u8 { a + 0 }\n", "", 1);
    assert_eq!(items_hashed_by(&mut db, file, &removed), 1);
}

/// A body edit changes no entry of the ast id map or anchor of the span map outside the item
/// it is in, so the ids and anchors read from the maps are those of the items whose text it
/// touched (the ones hashed), and every other item's are its last keys'.
#[test]
fn an_edit_reads_ids_only_for_the_items_it_touched() {
    let items: String =
        (0..40).map(|i| format!("pub fn f{i}(a: u8) -> u8 {{ a + {i} }}\n")).collect();
    let (mut db, file) = db_of(&items);
    let edited = items.replace("{ a + 20 }", "{ a * 20 }");
    assert_eq!(items_keyed_by(&mut db, file, &edited), (3, 3));
    let longer = edited.replace("{ a + 30 }", "{ let b = a; b + 30 }");
    assert_eq!(items_keyed_by(&mut db, file, &longer), (3, 3));
    // An item added at the start moves every other, and their ids and anchors stay theirs.
    let added = format!("struct S;\n{longer}");
    assert_eq!(items_keyed_by(&mut db, file, &added), (1, 1));
}

/// A second `fn f` before another takes the first id of its name, so the later one's id, in
/// the text the edit left unchanged, is renumbered: that item's ids are read from the maps
/// (and it is lowered again) although its text is as it was, and only it of those kept.
#[test]
fn a_renumbered_id_reads_the_item_holding_it() {
    let gs: String = (0..6).map(|i| format!("fn g{i}() {{}}\n")).collect();
    let hs: String = (0..4).map(|i| format!("fn h{i}() {{}}\n")).collect();
    let text = format!("{gs}fn f() {{}}\n{hs}");
    let (mut db, file) = db_of(&text);
    let edited = text.replacen("fn g4()", "fn f() {}\nfn g4()", 1);
    let (hashed, read) = items_keyed_by(&mut db, file, &edited);
    assert!(hashed < 12, "the items kept are not hashed");
    assert_eq!(read, hashed + 1, "the renumbered `fn f` is read, and no other item kept");
}

#[test]
fn common_ends_are_the_longest_that_do_not_overlap() {
    let long = "x".repeat(5000);
    let mut cases: Vec<(String, String)> = [
        ("abc", "abc"),
        ("abc", "abXc"),
        ("abc", "c"),
        ("", "fn f() {}"),
        ("fn f() {}", ""),
        ("aaaa", "aaaaaa"),
        ("é", "è"),
    ]
    .iter()
    .map(|&(a, b)| (a.to_owned(), b.to_owned()))
    .collect();
    cases.push((format!("{long}a{long}"), format!("{long}bb{long}")));
    cases.push((format!("{long}{long}"), format!("{long}y{long}")));
    for (old, new) in &cases {
        let (a, b) = (old.as_bytes(), new.as_bytes());
        let prefix = a.iter().zip(b).take_while(|(x, y)| x == y).count();
        let most = a.len().min(b.len()) - prefix;
        let suffix = (a.iter().rev().zip(b.iter().rev()).take_while(|(x, y)| x == y).count())
            .min(most);
        assert_eq!(super::common_ends(a, b), (prefix, suffix), "{old:?} to {new:?}");
    }
}

/// A tree asked again is a memo hit of its query, and a file whose items all kept their keys
/// is its kept tree: neither allocates once the worker's scratch is grown.
#[test]
fn trees_asked_again_allocate_nothing() {
    let (db, file) = db_of(ITEMS);
    let krate = fixture::test_crate(&db);
    crate::file_item_tree(&db, file.into(), krate);
    blaze_alloc_count::assert_at_most(0, "a memo hit of `file_item_tree`", || {
        std::hint::black_box(crate::file_item_tree(&db, file.into(), krate));
    });
    // A first lowering, a keyed one, then the kept tree twice, the scratch grown by the first.
    for _ in 0..4 {
        super::file_item_tree(&db, file, krate);
    }
    let kept = blaze_alloc_count::assert_at_most(0, "a file whose items kept their keys", || {
        super::file_item_tree(&db, file, krate)
    });
    assert_eq!(*kept, lower_whole(&db, file.into(), krate));
}

/// Assembling a file's tree allocates once, its block, however many items the file has.
#[test]
fn assembly_allocates_once_a_tree() {
    let allocations = |items: usize| {
        let text: String = (0..items)
            .map(|i| format!("pub struct S{i}(pub u8);\nfn f{i}() {{}}\nuse a::b{i};\n"))
            .collect();
        let (db, file) = db_of(&text);
        let krate = fixture::test_crate(&db);
        let top = super::top_attrs(&db, file, krate);
        let alone: Vec<ItemTree> = (file.parse(&db).view().items())
            .map(|item| super::item_alone(&db, file, krate, item))
            .collect();
        let marks: Vec<[super::Marks; 2]> =
            alone.iter().map(|it| [super::Marks::default(), super::Marks::ends(it)]).collect();
        let parts: Vec<super::Rows<'_>> = (alone.iter().zip(&marks))
            .map(|(tree, marks)| super::Rows { tree, marks })
            .collect();
        let mut made = Vec::with_capacity(items * 3 + 1);
        let (tree, used) =
            blaze_alloc_count::measure(|| super::assemble(&top, &parts, &mut made));
        assert_eq!(tree, lower_whole(&db, file.into(), krate));
        assert_eq!(made.len(), items * 3 + 1, "a mark per item, then the end");
        used.allocations
    };
    assert_eq!((allocations(4), allocations(64)), (1, 1));
}
