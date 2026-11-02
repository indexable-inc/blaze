//! Rename over a module graph of two crates, against the front end: every reference the
//! front end resolves to the item is rewritten and none other, the bytes are the text edits'
//! and the VFS root theirs, and each item touched is named and keyed as the view had it.

use std::collections::BTreeMap;

use base_db::{EditionedFileId, FileId, FileText};
use blaze_api::{Edit, EditedItem, How, Target};
use blaze_items::{ItemKey, key::Resolved};
use blaze_vfs::Op;
use syntax::{Edition, NodeOrToken, SourceFile, SyntaxKind};
use hir_expand::fixture;

use super::{applied, assert_fresh, db::TestDB, roots};
use crate::{Applied, Files, apply};

const GRAPH: &str = r#"
//- /main.rs crate:main deps:lib
use lib::shapes::Circle;

fn main() {
    let c = Circle::new(1);
    let r = c.radius;
    lib::shapes::area(&c);
    let area = r;
    let _ = area;
}
//- /lib.rs crate:lib
pub mod shapes;
pub use shapes::area;
//- /shapes.rs
pub struct Circle {
    pub radius: u32,
}

impl Circle {
    pub fn new(radius: u32) -> Circle {
        Circle { radius }
    }
}

pub fn area(c: &Circle) -> u32 {
    c.radius * c.radius * 3
}

mod inner {
    fn other() -> u32 {
        super::area(&super::Circle::new(2))
    }
}
"#;

const PATHS: [&str; 3] = ["main.rs", "lib.rs", "shapes.rs"];

struct View {
    files: Vec<(FileId, String, String)>,
}

impl Files for View {
    fn file(&self, path: &str) -> Option<(FileId, &[u8])> {
        self.files.iter().find(|it| it.1 == path).map(|it| (it.0, it.2.as_bytes()))
    }

    fn path(&self, file: FileId) -> Option<&str> {
        self.files.iter().find(|it| it.0 == file).map(|it| &*it.1)
    }
}

fn view(fixture: &str, paths: &[&str]) -> (TestDB, View) {
    let fx = fixture::db(fixture);
    let files = fx.files().to_vec();
    let db = fx.into_db();
    assert_eq!(files.len(), paths.len());
    let files = files
        .iter()
        .zip(paths)
        .map(|(file, path)| {
            let id = file.file_id(&db);
            (id, (*path).to_owned(), db.file_text(id).text(&db).to_string())
        })
        .collect();
    (db, View { files })
}

fn texts(view: &View) -> BTreeMap<String, String> {
    view.files.iter().map(|(_, path, text)| (path.clone(), text.clone())).collect()
}

fn rename(file: &str, item: &str, to: &str) -> Edit {
    Edit::Rename { to: to.to_owned(), item: Target { file: file.to_owned(), item: item.to_owned() } }
}

/// `edits` made on `GRAPH`, checked against `after`, the files as text edits would leave them.
#[track_caller]
fn check(edits: &[Edit], after: &BTreeMap<String, String>) -> (TestDB, Applied) {
    let (db, view) = view(GRAPH, &PATHS);
    let made = apply(&db, &view, edits).unwrap_or_else(|error| panic!("{edits:?}: {error:?}"));
    let before = texts(&view);
    assert_eq!(&applied(&before, &made.edited.ops), after);
    let (edited, written) = roots(&before, &made.edited.ops, after);
    assert_eq!(edited, written, "the edit's root is the text edits'");
    let mut edited_files: Vec<&str> = made
        .edited
        .ops
        .iter()
        .filter_map(|op| match op {
            Op::Edit { path, .. } => Some(path.as_str()),
            _ => None,
        })
        .collect();
    edited_files.sort_unstable();
    edited_files.dedup();
    assert_eq!(made.seeds().count(), edited_files.len(), "a seed for each file edited, none with errors");
    for seed in made.seeds() {
        let (path, text) = after
            .iter()
            .find(|(_, text)| FileText::content_of(text, None) == seed.content)
            .expect("a seed is of an edited file's new text");
        assert_eq!(view.path(seed.file), Some(path.as_str()), "a seed names its own file");
        assert_eq!(seed.edition, Edition::CURRENT, "{path}");
        assert_fresh(&seed.parse, text);
    }
    // The commit, in a front end of the same files (`db` stays the view as it was, which the
    // items' keys are of): in one write, the new texts set, then each seed handed over before
    // the write's maintainers parse the files, and each file's parse is the seed's own tree.
    let (mut next, files) = self::view(GRAPH, &PATHS);
    let seeds: Vec<_> = made.seeds().collect();
    next.batch(|next| {
        for seed in &seeds {
            let path = view.path(seed.file).expect("a seed names a file of the view");
            assert_eq!(files.path(seed.file), Some(path), "the same file in the next front end");
            next.set_file_text(seed.file, &after[path]);
        }
        for seed in &seeds {
            let id = EditionedFileId::new(next, seed.file, seed.edition);
            let input = next.file_text(seed.file);
            assert_eq!(input.content(next), seed.content, "the seed is of the file's text as set");
            assert!(seed.parse.root().text() == input.text(next).as_str(), "the seed's tree is of the file's text");
            assert!(id.seed_parse(next, seed.content, &seed.parse), "the front end takes the seed");
        }
    });
    for seed in &seeds {
        let id = EditionedFileId::new(&next, seed.file, seed.edition);
        assert!(std::ptr::eq(id.parse(&next).root().file(), seed.parse.root().file()), "no parse of its own");
    }
    (db, made)
}

fn named(items: &[EditedItem]) -> Vec<(&str, &str, How)> {
    items.iter().map(|it| (&*it.file, &*it.path, it.how)).collect()
}

/// The path blaze-items gives the item a key resolves to, its crate's name left out.
fn path_of_key(db: &TestDB, item: &EditedItem) -> Option<String> {
    let key = item.key?;
    let full = match blaze_items::resolve(db, ItemKey(key))? {
        Resolved::Item(id) => blaze_items::key::path(db, id),
        Resolved::Module(_) => return Some("<module>".to_owned()),
        _ => return None,
    };
    Some(full.split_once("::").map_or(full.clone(), |(_, rest)| rest.to_owned()))
}

#[track_caller]
fn swap(files: &mut BTreeMap<String, String>, path: &str, from: &str, to: &str) {
    let text = files.get_mut(path).unwrap();
    assert_eq!(text.matches(from).count(), 1, "`{from}` is not once in {path}");
    *text = text.replacen(from, to, 1);
}

#[test]
fn renaming_a_fn_rewrites_its_uses_in_every_crate_and_none_of_a_local_of_its_name() {
    let (_, view) = view(GRAPH, &PATHS);
    let mut after = texts(&view);
    swap(&mut after, "shapes.rs", "pub fn area(", "pub fn surface(");
    swap(&mut after, "shapes.rs", "super::area(", "super::surface(");
    swap(&mut after, "lib.rs", "pub use shapes::area;", "pub use shapes::surface;");
    swap(&mut after, "main.rs", "lib::shapes::area(&c)", "lib::shapes::surface(&c)");
    let (db, made) = check(&[rename("shapes.rs", "area", "surface")], &after);
    assert_eq!(
        named(&made.edited.items),
        [
            ("lib.rs", "", How::Scope),
            ("main.rs", "main", How::Body),
            ("shapes.rs", "", How::Scope),
            ("shapes.rs", "area", How::Renamed),
            ("shapes.rs", "inner::other", How::Body),
        ]
    );
    let renamed = made.edited.items.iter().find(|it| it.how == How::Renamed).unwrap();
    assert_eq!(path_of_key(&db, renamed).as_deref(), Some("shapes::area"));
    let body = made.edited.items.iter().find(|it| it.path == "inner::other").unwrap();
    assert_eq!(path_of_key(&db, body).as_deref(), Some("shapes::inner::other"));
}

#[test]
fn renaming_a_field_writes_out_its_shorthands() {
    let (_, view) = view(GRAPH, &PATHS);
    let mut after = texts(&view);
    swap(&mut after, "shapes.rs", "pub radius: u32", "pub r: u32");
    swap(&mut after, "shapes.rs", "Circle { radius }", "Circle { r: radius }");
    swap(&mut after, "shapes.rs", "c.radius * c.radius", "c.r * c.r");
    swap(&mut after, "main.rs", "c.radius;", "c.r;");
    let (_, made) = check(&[rename("shapes.rs", "Circle::radius", "r")], &after);
    assert_eq!(
        named(&made.edited.items),
        [
            ("main.rs", "main", How::Body),
            ("shapes.rs", "<Circle>::new", How::Body),
            ("shapes.rs", "Circle", How::Fields),
            ("shapes.rs", "area", How::Body),
        ]
    );
}

#[test]
fn renaming_a_module_rewrites_its_paths_and_moves_its_file() {
    let (_, view) = view(GRAPH, &PATHS);
    let mut after = texts(&view);
    swap(&mut after, "lib.rs", "pub mod shapes;", "pub mod geometry;");
    swap(&mut after, "lib.rs", "pub use shapes::area;", "pub use geometry::area;");
    swap(&mut after, "main.rs", "use lib::shapes::Circle;", "use lib::geometry::Circle;");
    swap(&mut after, "main.rs", "lib::shapes::area(&c)", "lib::geometry::area(&c)");
    let moved = after.remove("shapes.rs").unwrap();
    after.insert("geometry.rs".to_owned(), moved);
    let (_, made) = check(&[rename("lib.rs", "shapes", "geometry")], &after);
    assert_eq!(
        named(&made.edited.items),
        [
            ("lib.rs", "", How::Scope),
            ("lib.rs", "shapes", How::Renamed),
            ("main.rs", "", How::Scope),
            ("main.rs", "main", How::Body),
        ]
    );
}

#[test]
fn renaming_an_associated_fn_and_then_editing_names_the_view_as_it_was() {
    let (_, view) = view(GRAPH, &PATHS);
    let mut after = texts(&view);
    swap(&mut after, "shapes.rs", "pub fn new(", "pub fn make(");
    swap(&mut after, "shapes.rs", "super::Circle::new(2)", "super::Circle::make(2)");
    swap(&mut after, "main.rs", "Circle::new(1)", "Circle::make(1)");
    swap(&mut after, "shapes.rs", "c.radius * c.radius * 3", "c.radius * 4");
    let edits = [
        rename("shapes.rs", "<Circle>::new", "make"),
        Edit::ReplaceBody {
            body: "{\n    c.radius * 4\n}".to_owned(),
            item: Target { file: "shapes.rs".to_owned(), item: "area".to_owned() },
        },
    ];
    let (_, made) = check(&edits, &after);
    assert_eq!(
        named(&made.edited.items),
        [
            ("main.rs", "main", How::Body),
            ("shapes.rs", "<Circle>", How::Scope),
            ("shapes.rs", "<Circle>::new", How::Renamed),
            ("shapes.rs", "area", How::Body),
            ("shapes.rs", "inner::other", How::Body),
        ]
    );
}

#[test]
fn a_rename_stops_at_what_it_cant_place_or_do() {
    let (db, files) = view(GRAPH, &PATHS);
    let error = apply(&db, &files, &[rename("shapes.rs", "area", "fn")]).err().expect("the edits are refused");
    assert!(error.what.contains("is no identifier"), "{error:?}");
    let edit = Edit::AddItem { file: "lib.rs".to_owned(), into: None, text: "fn x() {}".to_owned() };
    let error = apply(&db, &files, &[edit, rename("shapes.rs", "area", "surface")]).err().expect("the edits are refused");
    assert!(error.what.contains("renames come first"), "{error:?}");

    let fixture = r#"
//- /lib.rs crate:lib
macro_rules! twice { () => { area() + area() } }
pub fn area() -> u32 { 1 }
pub fn both() -> u32 { twice!() }
"#;
    let (db, files) = view(fixture, &["lib.rs"]);
    let error = apply(&db, &files, &[rename("lib.rs", "area", "surface")]).err().expect("the edits are refused");
    assert!(error.what.contains("lib.rs:1: in tokens no macro expanded"), "{error:?}");

    let fixture = r#"
//- /lib.rs crate:lib
trait Shape { fn area(&self) -> u32; }
struct S;
impl Shape for S { fn area(&self) -> u32 { 1 } }
fn total(s: &S) -> u32 { s.area() }
"#;
    let (db, files) = view(fixture, &["lib.rs"]);
    let error = apply(&db, &files, &[rename("lib.rs", "<S as Shape>::area", "size")]).err().expect("the edits are refused");
    assert!(error.what.contains("rename it in the trait"), "{error:?}");
    let made = apply(&db, &files, &[rename("lib.rs", "Shape::area", "size")]).unwrap();
    let mut after = texts(&files);
    let text = after.get_mut("lib.rs").unwrap();
    *text = text.replace("area", "size");
    assert_eq!(applied(&texts(&files), &made.edited.ops), after);
}

/// The identifier tokens of the name, in order, as a walk of every token of a full parse
/// finds them.
fn walked(text: &str, name: &str) -> Vec<u32> {
    let parse = SourceFile::parse(text, Edition::CURRENT);
    parse
        .root()
        .descendants_with_tokens()
        .filter_map(|it| match it {
            NodeOrToken::Token(token) => Some(token),
            NodeOrToken::Node(_) => None,
        })
        .filter(|it| it.kind() == SyntaxKind::IDENT && it.text().strip_prefix("r#").unwrap_or(it.text()) == name)
        .map(|it| u32::from(it.text_range().start()))
        .collect()
}

#[test]
fn the_names_matches_reach_every_identifier_of_it_and_nothing_else() {
    let text = "fn area() {} // area\nfn r#area() { areas(); \"area\"; x.area; let _ = area_of; }\n\
                mod m { fn f() { m::area() } }\n";
    let finder = memchr::memmem::Finder::new(b"area");
    for parse in [SourceFile::parse(text, Edition::CURRENT), SourceFile::parse_outline(text, Edition::CURRENT)] {
        let root = parse.root();
        let found: Vec<u32> = finder
            .find_iter(text.as_bytes())
            .filter_map(|at| crate::rename::ident_at(root, at, 4))
            .map(|it| u32::from(it.text_range().start()))
            .collect();
        assert_eq!(found, walked(text, "area"));
        assert_eq!(found.len(), 4, "the fn, the raw fn, the field and the path; no comment, string or longer name");
    }
}
