//! Each edit kind against its text-edit equivalent, with no front end: the same bytes and VFS
//! root, a tree equal to a fresh parse, and the items a whole-file diff finds changed.

use std::collections::{BTreeMap, BTreeSet};

use blaze_api::{Edit, How, Target};
use blaze_vfs::Op;
use syntax::{Edition, SourceFile, view::Node};

use super::{assert_fresh, roots};
use crate::{
    edit::{self, Touch},
    file::Working,
    path,
};

const BASE: &str = r#"use std::fmt;

/// A point.
#[derive(Debug)]
pub struct P {
    pub x: u32,
    y: u32,
}

struct Q { a: u8, b: u8 }

struct U;

struct T(u8);

struct E {}

enum K {
    A,
    B { z: u8 },
}

trait Tr {
    fn need(&self);
}

impl P {
    pub fn new() -> P {
        P { x: 0, y: 0 }
    }
}

mod m {
    pub fn inner() {}
}

mod empty {}

fn last() {}
"#;

const FILE: &str = "src/lib.rs";

/// `BASE` with its one `from` replaced by `to`: the text edit an agent would make.
#[track_caller]
fn swap(text: &str, from: &str, to: &str) -> String {
    assert_eq!(text.matches(from).count(), 1, "`{from}` is not once in the file");
    text.replacen(from, to, 1)
}

fn target(item: &str) -> Target {
    Target { file: FILE.to_owned(), item: item.to_owned() }
}

/// What a whole-file diff finds changed: each top item's own text (its member items cut out)
/// compared by path. A module, impl or trait whose own text differs, and the file, changed in
/// scope; any other item changed; one on one side only was added or removed.
fn diff(before: &str, after: &str) -> BTreeSet<(String, &'static str)> {
    fn own(node: Node<'_>) -> String {
        let mut text = node.text().to_owned();
        let start = usize::from(node.text_range().start());
        let mut members: Vec<Node<'_>> = path::members(node).filter(|it| path::is_top(*it)).collect();
        members.sort_unstable_by_key(|it| std::cmp::Reverse(it.text_range().start()));
        for member in members {
            let range = member.text_range();
            text.replace_range(usize::from(range.start()) - start..usize::from(range.end()) - start, "\u{0}");
        }
        text
    }
    fn walk(node: Node<'_>, out: &mut BTreeMap<String, (bool, String)>) {
        for member in path::members(node).filter(|it| path::is_top(*it)) {
            let container = matches!(
                member.kind(),
                syntax::SyntaxKind::MODULE | syntax::SyntaxKind::IMPL | syntax::SyntaxKind::TRAIT
            );
            out.insert(path::of(member), (container, own(member)));
            walk(member, out);
        }
    }
    let tops = |text: &str| {
        let parse = SourceFile::parse(text, Edition::CURRENT);
        let mut out = BTreeMap::new();
        out.insert(String::new(), (true, own(parse.root())));
        walk(parse.root(), &mut out);
        out
    };
    let (old, new) = (tops(before), tops(after));
    let mut found = BTreeSet::new();
    for (path, (container, text)) in &old {
        match new.get(path) {
            None => {
                found.insert((path.clone(), "removed"));
            }
            Some((_, now)) if now != text => {
                found.insert((path.clone(), if *container { "scope" } else { "changed" }));
            }
            Some(_) => {}
        }
    }
    for path in new.keys().filter(|it| !old.contains_key(*it)) {
        found.insert((path.clone(), "added"));
    }
    found
}

fn class(how: How) -> &'static str {
    match how {
        How::Body | How::Signature | How::Fields | How::Whole | How::Renamed => "changed",
        How::Added => "added",
        How::Removed => "removed",
        How::Scope => "scope",
    }
}

/// Makes `edits` on `BASE` and checks them against the text `after`.
#[track_caller]
fn check(edits: &[Edit], after: &str) -> Vec<(String, How)> {
    let mut file = Working::parsed(FILE, Edition::CURRENT, BASE);
    let mut touches: Vec<Touch> = Vec::new();
    for edit in edits {
        edit::apply(&mut file, 0, edit, &mut touches).unwrap_or_else(|error| panic!("{edit:?}: {error}"));
    }
    assert_eq!(file.bytes(), after);
    assert_fresh(&file.parse, after);
    let ops: Vec<Op> = file.ops().collect();
    let one = |text: &str| BTreeMap::from([(FILE.to_owned(), text.to_owned())]);
    let (edited, written) = roots(&one(BASE), &ops, &one(after));
    assert_eq!(edited, written, "the edit's root is the text edit's");
    let mine: BTreeSet<(String, &'static str)> =
        touches.iter().map(|it| (edit::path_at(&file, &it.at), class(it.how))).collect();
    assert_eq!(mine, diff(BASE, after), "the items touched are those a whole-file diff finds");
    let mut named: Vec<(String, How)> = touches.iter().map(|it| (edit::path_at(&file, &it.at), it.how)).collect();
    named.sort_unstable_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)));
    named.dedup();
    named
}

#[track_caller]
fn fails(edits: &[Edit]) -> String {
    let mut file = Working::parsed(FILE, Edition::CURRENT, BASE);
    let mut touches = Vec::new();
    for edit in edits {
        if let Err(error) = edit::apply(&mut file, 0, edit, &mut touches) {
            return error;
        }
    }
    panic!("{edits:?} made no error")
}

fn named(path: &str, how: How) -> (String, How) {
    (path.to_owned(), how)
}

#[test]
fn replace_body() {
    let edit = Edit::ReplaceBody { body: "{\n        P { x: 1, y: 2 }\n    }".to_owned(), item: target("<P>::new") };
    let after = swap(BASE, "P { x: 0, y: 0 }", "P { x: 1, y: 2 }");
    assert_eq!(check(&[edit], &after), [named("<P>::new", How::Body)]);
}

#[test]
fn replace_body_of_a_fn_with_none_gives_it_one() {
    let edit = Edit::ReplaceBody { body: "{}".to_owned(), item: target("Tr::need") };
    let after = swap(BASE, "fn need(&self);", "fn need(&self) {}");
    assert_eq!(check(&[edit], &after), [named("Tr::need", How::Whole)]);
}

#[test]
fn replace_signature() {
    let edit = Edit::ReplaceSignature { item: target("last"), signature: "pub(crate) fn last(x: u8)".to_owned() };
    let after = swap(BASE, "fn last() {}", "pub(crate) fn last(x: u8) {}");
    assert_eq!(check(&[edit], &after), [named("last", How::Signature)]);
    let edit =
        Edit::ReplaceSignature { item: target("<P>::new"), signature: "pub fn new(x: u32) -> P".to_owned() };
    let after = swap(BASE, "pub fn new() -> P", "pub fn new(x: u32) -> P");
    assert_eq!(check(&[edit], &after), [named("<P>::new", How::Signature)]);
}

#[test]
fn replace_item_and_field() {
    let edit = Edit::ReplaceItem { item: target("last"), text: "fn last() -> u8 { 0 }".to_owned() };
    let after = swap(BASE, "fn last() {}", "fn last() -> u8 { 0 }");
    assert_eq!(check(&[edit], &after), [named("last", How::Whole)]);
    let edit = Edit::ReplaceItem { item: target("P::y"), text: "y: u64".to_owned() };
    let after = swap(BASE, "y: u32", "y: u64");
    assert_eq!(check(&[edit], &after), [named("P", How::Fields)]);
}

#[test]
fn add_field_in_each_list_style() {
    let cases = [
        ("P", "pub z: u8", swap(BASE, "    y: u32,\n}", "    y: u32,\n    pub z: u8,\n}")),
        ("Q", "c: u8", swap(BASE, "b: u8 }", "b: u8, c: u8 }")),
        ("U", "v: i32", swap(BASE, "struct U;", "struct U { v: i32 }")),
        ("T", "u16", swap(BASE, "struct T(u8);", "struct T(u8, u16);")),
        ("E", "e: bool", swap(BASE, "struct E {}", "struct E { e: bool }")),
        ("K::A", "u8", swap(BASE, "    A,\n", "    A(u8),\n")),
    ];
    for (strukt, field, after) in cases {
        let top = strukt.split("::").next().unwrap();
        let edit = Edit::AddField { field: field.to_owned(), strukt: target(strukt) };
        assert_eq!(check(&[edit], &after), [named(top, How::Fields)], "{strukt}");
    }
}

#[test]
fn remove_field_keeps_the_list_style() {
    let cases = [
        ("P", "x", swap(BASE, "    pub x: u32,\n    y: u32,", "    y: u32,")),
        ("P", "y", swap(BASE, "    pub x: u32,\n    y: u32,\n}", "    pub x: u32,\n}")),
        ("Q", "b", swap(BASE, "{ a: u8, b: u8 }", "{ a: u8 }")),
        ("T", "0", swap(BASE, "struct T(u8);", "struct T();")),
    ];
    for (strukt, field, after) in cases {
        let edit = Edit::RemoveField { field: field.to_owned(), strukt: target(strukt) };
        assert_eq!(check(&[edit], &after), [named(strukt, How::Fields)], "{strukt}::{field}");
    }
}

#[test]
fn add_item_in_the_file_a_module_an_impl_and_an_empty_module() {
    let edit = Edit::AddItem { file: FILE.to_owned(), into: None, text: "fn added() {}".to_owned() };
    let after = swap(BASE, "fn last() {}\n", "fn last() {}\n\nfn added() {}\n");
    assert_eq!(check(&[edit], &after), [named("", How::Scope), named("added", How::Added)]);

    let edit =
        Edit::AddItem { file: FILE.to_owned(), into: Some("m".to_owned()), text: "fn more() {\n    1\n}".to_owned() };
    let after = swap(BASE, "    pub fn inner() {}\n", "    pub fn inner() {}\n\n    fn more() {\n        1\n    }\n");
    assert_eq!(check(&[edit], &after), [named("m", How::Scope), named("m::more", How::Added)]);

    let edit = Edit::AddItem { file: FILE.to_owned(), into: Some("<P>".to_owned()), text: "fn two() {}".to_owned() };
    let after = swap(BASE, "        P { x: 0, y: 0 }\n    }\n", "        P { x: 0, y: 0 }\n    }\n\n    fn two() {}\n");
    assert_eq!(check(&[edit], &after), [named("<P>", How::Scope), named("<P>::two", How::Added)]);

    let edit = Edit::AddItem { file: FILE.to_owned(), into: Some("empty".to_owned()), text: "fn e() {}".to_owned() };
    let after = swap(BASE, "mod empty {}", "mod empty {\n    fn e() {}\n}");
    assert_eq!(check(&[edit], &after), [named("empty", How::Scope), named("empty::e", How::Added)]);
}

#[test]
fn remove_item_and_variant() {
    let edit = Edit::RemoveItem { item: target("last") };
    let after = swap(BASE, "mod empty {}\n\nfn last() {}\n", "mod empty {}\n");
    assert_eq!(check(&[edit], &after), [named("", How::Scope), named("last", How::Removed)]);
    let edit = Edit::RemoveItem { item: target("K::B") };
    let after = swap(BASE, "    A,\n    B { z: u8 },\n", "    A,\n");
    assert_eq!(check(&[edit], &after), [named("K", How::Fields)]);
}

#[test]
fn add_use_after_the_uses_or_before_the_first_item() {
    let edit = Edit::AddUse { file: FILE.to_owned(), into: None, path: "std::io".to_owned() };
    let after = swap(BASE, "use std::fmt;\n", "use std::fmt;\nuse std::io;\n");
    assert_eq!(check(&[edit], &after), [named("", How::Scope)]);
    let edit = Edit::AddUse { file: FILE.to_owned(), into: Some("m".to_owned()), path: "super::P".to_owned() };
    let after = swap(BASE, "mod m {\n    pub fn inner", "mod m {\n    use super::P;\n\n    pub fn inner");
    assert_eq!(check(&[edit], &after), [named("m", How::Scope)]);
}

#[test]
fn edits_in_a_row_name_items_as_the_file_was() {
    let edits = [
        Edit::AddField { field: "pub z: u8".to_owned(), strukt: target("P") },
        Edit::ReplaceBody { body: "{\n        P { x: 1, y: 2, z: 3 }\n    }".to_owned(), item: target("<P>::new") },
        Edit::RemoveItem { item: target("last") },
    ];
    let after = swap(BASE, "    y: u32,\n}", "    y: u32,\n    pub z: u8,\n}");
    let after = swap(&after, "P { x: 0, y: 0 }", "P { x: 1, y: 2, z: 3 }");
    let after = swap(&after, "mod empty {}\n\nfn last() {}\n", "mod empty {}\n");
    assert_eq!(
        check(&edits, &after),
        [
            named("", How::Scope),
            named("<P>::new", How::Body),
            named("P", How::Fields),
            named("last", How::Removed),
        ]
    );
}

#[test]
fn an_edit_names_what_it_cant_do() {
    let error = fails(&[Edit::ReplaceBody { body: "{}".to_owned(), item: target("P::x") }]);
    assert!(error.contains("is a field, and the edit takes a fn"), "{error}");
    let error = fails(&[Edit::ReplaceBody { body: "1".to_owned(), item: target("last") }]);
    assert!(error.contains("no block"), "{error}");
    let error = fails(&[Edit::AddUse { file: FILE.to_owned(), into: None, path: "std::fmt".to_owned() }]);
    assert!(error.contains("is there already"), "{error}");
    let error = fails(&[Edit::AddField { field: "u8".to_owned(), strukt: target("P") }]);
    assert!(error.contains("is a tuple field"), "{error}");
    let error = fails(&[
        Edit::AddItem { file: FILE.to_owned(), into: None, text: "fn added() {}".to_owned() },
        Edit::ReplaceBody { body: "{ 1 }".to_owned(), item: target("added") },
    ]);
    assert!(error.contains("was added by an earlier edit"), "{error}");
    let error = fails(&[Edit::AddUse { file: FILE.to_owned(), into: Some("<P>".to_owned()), path: "x".to_owned() }]);
    assert!(error.contains("a use goes in a module"), "{error}");
}
