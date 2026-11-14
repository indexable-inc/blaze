//! The keying properties (D39; host.md, "blaze-unitkey"), each over generated crates, each with a
//! control that the property's cause is what moves the key (rule test-the-cause).
//!
//! The invariant: a key says what a record is and moves exactly when that changes. A comment or
//! whitespace edit moves the file image's key and the manifest's and nothing else; a body edit
//! moves that and the one body key whose item's text it is in; a header edit moves that, the
//! edited item's body key and the surface key, and no other body key; an edit in another unit
//! moves no key of this one; an escaping item and a body's non-quiet macro are in its node's
//! piece; an item whose `#[cfg]` is off has no key; tokens are hashed as tokens.

use std::{alloc::Global, io::Write};

use blaze_format::Aligned;
use blaze_ids::ContentId;
use blaze_keyed::{
    kind::{Body, FileImage, Key, Namespace, Settings, Surface, ToolId, UnitManifest},
    record::StableId,
};
use cfg::CfgOptions;
use hegel::generators as gs;
use intern::Symbol;
use syntax::{Edition, Extensions};

use crate::{
    image::{self, Reading},
    rows::kind,
    unit::{self, Unit, UnitFile},
};

const TOOL: ToolId = ToolId { tool: ContentId([1; 32]), target: ContentId([4; 32]), namespace: Namespace::PUBLIC };
const READING: Reading = Reading { edition: Edition::Edition2024, extensions: Extensions::NONE };
const THIS: ContentId = ContentId([5; 32]);
const OTHER: ContentId = ContentId([6; 32]);

/// The cfg option the tests switch, and the settings that cover it (rule 21: the caller's
/// settings digest covers its cfg).
fn options(on: bool) -> CfgOptions {
    let mut options = CfgOptions::default();
    if on {
        options.insert_atom(Symbol::intern("on"));
    }
    options
}

fn settings(on: bool) -> Settings {
    Settings(ContentId::of("blaze-unitkey test settings", &[on as u8]))
}

/// Every key of a one-file unit at the crate root.
struct Run {
    file: Key<FileImage>,
    record: Vec<u8>,
    manifest: Key<UnitManifest>,
    surface: Key<Surface>,
    items: Vec<(StableId, Key<Body>), Global>,
}

impl Run {
    fn body(&self, path: &str) -> Option<Key<Body>> {
        let id = StableId::of(path.as_bytes(), kind::FN as u8);
        self.items.binary_search_by_key(&id, |(stable, _)| *stable).ok().map(|at| self.items[at].1)
    }

    fn expect_body(&self, path: &str) -> Key<Body> {
        self.body(path).unwrap_or_else(|| panic!("no body `{path}`"))
    }

    /// The paths (of `paths`) whose body key differs between `self` and `other`.
    fn moved<'p>(&self, other: &Run, paths: &[&'p str]) -> Vec<&'p str, Global> {
        let mut out = Vec::new_in(Global);
        for path in paths {
            if self.expect_body(path) != other.expect_body(path) {
                out.push(*path);
            }
        }
        out
    }
}

fn run_in(unit_id: ContentId, text: &[u8], on: bool) -> Run {
    let imaged = image::image(&TOOL, READING, text, Global).expect("the file images");
    let aligned = Aligned::copy_from(&imaged.record);
    let files = [UnitFile { key: imaged.key, image: &aligned, module: b"" }];
    let options = options(on);
    let unit = Unit { tool: TOOL, settings: settings(on), unit: unit_id, cfg: &options, files: &files, answers: &[] };
    let keys = unit::key(&unit, Global).expect("the unit keys");
    assert_eq!(keys.files.len(), 1);
    assert_eq!(keys.files[0].image, imaged.key);
    let mut items = Vec::new_in(Global);
    items.extend_from_slice(keys.file_items(0));
    Run { file: imaged.key, record: imaged.record, manifest: keys.manifest, surface: keys.surface, items }
}

fn run(text: &[u8]) -> Run {
    run_in(THIS, text, false)
}

/// A generated crate: `fns` functions `f0..`, each returning `u32`, with a literal in its body
/// and calls to some of the others in its body.
#[derive(Clone, Debug)]
struct Crate {
    lits: Vec<u8, Global>,
    /// `calls[i]`: the functions `f{i}`'s body calls.
    calls: Vec<Vec<u8, Global>, Global>,
}

#[hegel::composite]
fn krate(tc: &hegel::TestCase) -> Crate {
    let count = usize::from(tc.draw(gs::integers::<u8>().min_value(2).max_value(5)));
    let mut lits = Vec::new_in(Global);
    let mut calls = Vec::new_in(Global);
    for _ in 0..count {
        lits.push(tc.draw(gs::integers::<u8>().max_value(200)));
        let mut called = Vec::new_in(Global);
        for other in 0..count as u8 {
            if tc.draw(gs::booleans()) {
                called.push(other);
            }
        }
        calls.push(called);
    }
    Crate { lits, calls }
}

/// How one function of a rendered crate is changed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Change {
    None,
    /// Its body's literal plus one.
    Body,
    /// Its return type `u32` -> `u64` (and its body's value cast).
    Header,
    /// A comment and extra whitespace before it and in its body, never between two
    /// punctuation tokens.
    Comment,
}

impl Crate {
    fn render(&self, at: usize, change: Change, prelude: &str) -> Vec<u8, Global> {
        let mut out = Vec::new_in(Global);
        out.extend_from_slice(prelude.as_bytes());
        for (index, lit) in self.lits.iter().enumerate() {
            let this = if index == at { change } else { Change::None };
            let lit = u32::from(*lit) + u32::from(this == Change::Body);
            let ret = if this == Change::Header { "u64" } else { "u32" };
            if this == Change::Comment {
                out.extend_from_slice(b"// a comment\n\n");
            }
            let space = if this == Change::Comment { "   " } else { " " };
            write!(out, "fn{space}f{index}() -> {ret} {{ let v = {lit}; ").expect("a Vec takes writes");
            if this == Change::Comment {
                out.extend_from_slice(b"/* inside */   \n ");
            }
            for called in &self.calls[index] {
                write!(out, "f{called}(); ").expect("a Vec takes writes");
            }
            write!(out, "v as {ret} }}\n").expect("a Vec takes writes");
        }
        out
    }

    fn paths(&self) -> Vec<String, Global> {
        let mut out = Vec::new_in(Global);
        for index in 0..self.lits.len() {
            out.push(format!("f{index}"));
        }
        out
    }

    fn draw_at(&self, tc: &hegel::TestCase) -> usize {
        usize::from(tc.draw(gs::integers::<u8>().max_value(self.lits.len() as u8 - 1)))
    }
}

fn path_refs(paths: &[String]) -> Vec<&str, Global> {
    let mut out = Vec::new_in(Global);
    out.extend(paths.iter().map(String::as_str));
    out
}

/// The invariant: a comment or whitespace edit moves the file image's key (its bytes) and the
/// manifest's (it covers the file keys) and no other key. Imaging is deterministic: one text,
/// one record's bytes. The control: the same place edited in a token moves a body key.
#[hegel::test(test_cases = 100)]
fn a_comment_or_whitespace_edit_moves_only_the_file_and_the_manifest(tc: hegel::TestCase) {
    let krate = tc.draw(krate());
    let at = krate.draw_at(&tc);
    let text = krate.render(at, Change::None, "");
    let before = run(&text);
    assert_eq!(before.record, run(&text).record, "imaging twice writes equal bytes");
    let after = run(&krate.render(at, Change::Comment, ""));
    assert_ne!(before.file, after.file, "the edit is in the file's bytes");
    assert_ne!(before.manifest, after.manifest, "the manifest covers the file keys");
    assert_eq!(before.surface, after.surface, "a comment is no token");
    assert_eq!(before.items, after.items, "no item's tokens changed");
    // The control: a token edit at the same place moves the body.
    let edited = run(&krate.render(at, Change::Body, ""));
    assert_ne!(before.items, edited.items);
}

/// The invariant: a private body edit moves exactly that body's key, the file's and the
/// manifest's, never the surface's.
#[hegel::test(test_cases = 100)]
fn a_private_body_edit_moves_exactly_that_body(tc: hegel::TestCase) {
    let krate = tc.draw(krate());
    let at = krate.draw_at(&tc);
    let paths = krate.paths();
    let paths = path_refs(&paths);
    let before = run(&krate.render(at, Change::None, ""));
    let after = run(&krate.render(at, Change::Body, ""));
    assert_ne!(before.file, after.file);
    assert_ne!(before.manifest, after.manifest);
    assert_eq!(before.surface, after.surface, "a body is not in the surface");
    assert_eq!(after.moved(&before, &paths), [paths[at]]);
}

/// The invariant: a header edit moves the edited item's body key and the surface key, and no
/// other item's body key. The callers of the edited function keep theirs: the old claim that a
/// signature edit moves its mentioners' keys is gone (D39). What a caller read of the signature
/// is in its record's shape, so it is redone by its read, never by its key.
#[hegel::test(test_cases = 100)]
fn a_header_edit_moves_its_body_and_the_surface_only(tc: hegel::TestCase) {
    let krate = tc.draw(krate());
    let at = krate.draw_at(&tc);
    let paths = krate.paths();
    let paths = path_refs(&paths);
    let before = run(&krate.render(at, Change::None, ""));
    let after = run(&krate.render(at, Change::Header, ""));
    assert_ne!(before.surface, after.surface, "a header is in the surface");
    assert_eq!(after.moved(&before, &paths), [paths[at]], "a caller's key holds nothing of its callee");
}

/// The invariant: an edit in another unit moves no key of this unit: keying reads nothing of
/// another unit (`Unit` has no field for one). Under the text-closure rule it did: a body's key
/// held the closures of the names it mentions across its universe, and a surface and a manifest
/// held their dependencies' surface keys. The control: the edit moves the other unit's surface.
#[hegel::test(test_cases = 60)]
fn an_edit_in_another_unit_moves_no_key_of_this_one(tc: hegel::TestCase) {
    let this = tc.draw(krate());
    let other = tc.draw(krate());
    let at = other.draw_at(&tc);
    let text = this.render(0, Change::None, "");
    let before = run_in(THIS, &text, false);
    let other_before = run_in(OTHER, &other.render(at, Change::None, ""), false);
    let other_after = run_in(OTHER, &other.render(at, Change::Header, ""), false);
    let after = run_in(THIS, &text, false);
    assert_ne!(other_before.surface, other_after.surface, "the control: the other unit's surface moved");
    assert_eq!(before.manifest, after.manifest);
    assert_eq!(before.surface, after.surface);
    assert_eq!(before.items, after.items);
    // And two units of equal text key apart: the unit is in every key.
    let twin = run_in(OTHER, &text, false);
    assert_ne!(before.surface, twin.surface);
    assert_ne!(before.manifest, twin.manifest);
    assert!(before.items.iter().zip(&twin.items).all(|(a, b)| a.0 == b.0 && a.1 != b.1));
}

/// An escape rule's case: an item `g` with `{w}` where the edited width goes, and whether that
/// edit moves the surface. Every rule has its control.
struct Case {
    rule: &'static str,
    item: &'static str,
    surface_moves: bool,
}

const CASES: [Case; 17] = [
    Case { rule: "escaping impl", item: "struct S; fn g() { impl S { fn x() -> u{w} { 0 } } }", surface_moves: true },
    Case { rule: "control: nested fn", item: "fn g() { fn x() -> u{w} { 0 } }", surface_moves: false },
    Case {
        rule: "escaping impl in a const body",
        item: "struct S; const _: () = { impl S { fn x() -> u{w} { 0 } } };",
        surface_moves: true,
    },
    Case {
        rule: "escaping #[macro_export]",
        item: "fn g() { #[macro_export] macro_rules! m { () => { {w} }; } }",
        surface_moves: true,
    },
    Case { rule: "control: local macro_rules", item: "fn g() { macro_rules! m { () => { {w} }; } }", surface_moves: false },
    Case { rule: "escaping #[no_mangle]", item: "fn g() { #[no_mangle] fn x() -> u{w} { 0 } }", surface_moves: true },
    Case {
        rule: "escaping #[export_name]",
        item: "fn g() { #[export_name = \"x\"] fn x() -> u{w} { 0 } }",
        surface_moves: true,
    },
    Case { rule: "escaping #[used]", item: "fn g() { #[used] static X: u{w} = 0; }", surface_moves: true },
    Case {
        rule: "escaping #[link_section]",
        item: "fn g() { #[link_section = \".x\"] static X: u{w} = 0; }",
        surface_moves: true,
    },
    Case {
        rule: "escaping #[link] block",
        item: "fn g() { #[link(name = \"x\")] unsafe extern \"C\" { fn x() -> u{w}; } }",
        surface_moves: true,
    },
    Case {
        rule: "control: a plain extern block",
        item: "fn g() { unsafe extern \"C\" { fn x() -> u{w}; } }",
        surface_moves: false,
    },
    // An attribute macro reads its whole item, so its header cuts nothing.
    Case { rule: "attribute macro", item: "#[my_attr] fn g() -> u32 { {w} }", surface_moves: true },
    Case { rule: "control: inert attribute", item: "#[inline] fn g() -> u32 { {w} }", surface_moves: false },
    // No body is interface by its header any more: what it shows others is a fact its task makes.
    Case { rule: "const fn body", item: "const fn g() -> u32 { {w} }", surface_moves: false },
    Case { rule: "async fn body", item: "async fn g() -> u32 { {w} }", surface_moves: false },
    Case { rule: "impl Trait return body", item: "fn g() -> impl Sized { {w}u32 }", surface_moves: false },
    // A header's own expression is no body: it stays in the header.
    Case { rule: "array length in a header", item: "fn g(_: [u8; {w}]) {}", surface_moves: true },
];

/// The literals an edit swaps between: valid as a value and as an integer width (`u8`/`u16`...).
const WIDTHS: [&str; 4] = ["8", "16", "32", "64"];

fn render_case(item: &str, width: &str, filler: u8) -> Vec<u8, Global> {
    let mut out = Vec::new_in(Global);
    out.extend_from_slice(item.replace("{w}", width).as_bytes());
    out.push(b'\n');
    for index in 0..filler {
        write!(out, "fn filler{index}() -> u32 {{ {index} }}\n").expect("a Vec takes writes");
    }
    out
}

/// The invariant: an item written literally in a body that escapes it joins its node's piece,
/// so editing it moves the surface key; a nested item that does not escape is the body's own
/// text. Each rule with its control; no filler's body key moves.
#[hegel::test(test_cases = 100)]
fn each_escape_rule_with_its_control(tc: hegel::TestCase) {
    let case = &CASES[usize::from(tc.draw(gs::integers::<u8>().max_value(CASES.len() as u8 - 1)))];
    let from = usize::from(tc.draw(gs::integers::<u8>().max_value(3)));
    let to = (from + 1 + usize::from(tc.draw(gs::integers::<u8>().max_value(2)))) % WIDTHS.len();
    let filler = tc.draw(gs::integers::<u8>().max_value(3));
    let before = run(&render_case(case.item, WIDTHS[from], filler));
    let after = run(&render_case(case.item, WIDTHS[to], filler));
    assert_eq!(before.surface != after.surface, case.surface_moves, "rule `{}`", case.rule);
    for index in 0..filler {
        let path = format!("filler{index}");
        assert_eq!(before.expect_body(&path), after.expect_body(&path), "rule `{}`: {path}", case.rule);
    }
}

/// The invariant: an impl written in a body is in the surface, the body's statements are not.
/// Editing the impl moves the surface key; a plain statement edit in the same body moves that
/// body's key and not the surface's (the control).
#[hegel::test(test_cases = 60)]
fn a_literal_impl_in_a_body_is_in_the_surface(tc: hegel::TestCase) {
    let krate = tc.draw(krate());
    let width = usize::from(tc.draw(gs::integers::<u8>().max_value(2)));
    let lit = tc.draw(gs::integers::<u8>().max_value(200));
    let prelude = |width: &str, lit: u8| {
        format!("struct S;\nfn host() {{ impl S {{ fn x() -> u{width} {{ 0 }} }} let _y = {lit}; }}\n")
    };
    let base = run(&krate.render(0, Change::None, &prelude(WIDTHS[width], lit)));
    let impl_edit = run(&krate.render(0, Change::None, &prelude(WIDTHS[width + 1], lit)));
    let statement_edit = run(&krate.render(0, Change::None, &prelude(WIDTHS[width], lit.wrapping_add(1))));
    assert_ne!(base.surface, impl_edit.surface, "the escaping impl is in `host`'s piece");
    assert_eq!(base.surface, statement_edit.surface, "a statement is not");
    assert_ne!(base.expect_body("host"), statement_edit.expect_body("host"));
}

/// The invariant: whether a body holds a macro that is not quiet is one flag in its node's
/// piece: gaining or losing such a call moves the surface key; editing the call's arguments does
/// not (what it makes is a fact its task makes). The control: gaining a quiet call does not move
/// it. A derive in a body counts: the impls it makes are in no text.
#[hegel::test(test_cases = 60)]
fn a_non_quiet_macro_in_a_body_is_one_flag(tc: hegel::TestCase) {
    let krate = tc.draw(krate());
    let arg = tc.draw(gs::integers::<u8>().max_value(200));
    let caller = |call: &str| format!("fn caller() {{ let _a = 1; {call} }}\n");
    let render = |call: &str| run(&krate.render(0, Change::None, &caller(call)));
    let none = render("");
    let mine = render(&format!("mine!({arg});"));
    let mine_edited = render(&format!("mine!({});", u32::from(arg) + 1));
    let quiet = render("assert!(true);");
    let derived = render("#[derive(Clone)] struct L;");
    let plain = render("struct L;");
    assert_ne!(none.surface, mine.surface, "gaining a non-quiet call sets the flag");
    assert_eq!(mine.surface, mine_edited.surface, "its arguments are the body's own text");
    assert_ne!(mine.expect_body("caller"), mine_edited.expect_body("caller"));
    assert_eq!(none.surface, quiet.surface, "the control: a quiet call sets nothing");
    assert_ne!(plain.surface, derived.surface, "a derive in a body sets the flag");
    assert_eq!(none.surface, plain.surface, "the control: a plain nested struct sets nothing");
}

/// The invariant: an item whose `#[cfg]` is off is no item of the unit: no body key, no piece
/// (the surface is the one of the text without it). Switching the option on gives it its key and
/// moves every key through `settings`. The control: an ungated item has a key either way.
#[hegel::test(test_cases = 60)]
fn an_item_off_by_cfg_has_no_key(tc: hegel::TestCase) {
    let krate = tc.draw(krate());
    let paths = krate.paths();
    let paths = path_refs(&paths);
    let gated = "#[cfg(on)] fn gated() -> u32 { 1 }\n";
    let off = run_in(THIS, &krate.render(0, Change::None, gated), false);
    let without = run_in(THIS, &krate.render(0, Change::None, ""), false);
    let on = run_in(THIS, &krate.render(0, Change::None, gated), true);
    assert_eq!(off.body("gated"), None, "an item that is off has no body key");
    assert_eq!(off.surface, without.surface, "nor a piece");
    assert_eq!(off.items, without.items);
    assert!(on.body("gated").is_some(), "on, it has its key");
    assert_ne!(off.surface, on.surface);
    assert_ne!(off.manifest, on.manifest);
    assert_eq!(on.moved(&off, &paths), paths, "settings cover the cfg, so every body key moves");
    assert!(off.body(paths[0]).is_some() && on.body(paths[0]).is_some(), "the control");
}

/// One token-hashing case: two texts of `fn t`, and whether `t`'s key differs.
const TOKENS: [(&str, &str, bool); 8] = [
    ("fn t() { a b }", "fn t() { ab }", true),
    ("fn t() { x - -y }", "fn t() { x --y }", true),
    ("/// one\nfn t() {}", "/// two\nfn t() {}", true),
    ("fn t() { \"a b\" }", "fn t() { \"a  b\" }", true),
    ("fn t() { r\"a\" }", "fn t() { \"a\" }", true),
    // The controls: a plain comment and whitespace between tokens that are not both
    // punctuation are no token.
    ("fn t() { x /* one */ }", "fn t() { x /* two */ }", false),
    ("fn t() { x // one\n}", "fn t() { x // two\n}", false),
    ("fn t() { a + b }", "fn t() {a+b}", false),
];

/// The invariant: tokens are hashed as tokens: (kind, joint punctuation, length-prefixed text).
/// `a b` and `ab`, `x - -y` and `x --y`, a doc comment's text and a string literal's text each
/// move the item's key; a plain comment and whitespace between tokens do not.
#[hegel::test(test_cases = 60)]
fn tokens_are_hashed_as_tokens(tc: hegel::TestCase) {
    let (a, b, moves) = TOKENS[usize::from(tc.draw(gs::integers::<u8>().max_value(TOKENS.len() as u8 - 1)))];
    let filler = tc.draw(gs::integers::<u8>().max_value(3));
    let render = |item: &str| render_case(item, "", filler);
    let before = run(&render(a));
    let after = run(&render(b));
    assert_eq!(before.expect_body("t") != after.expect_body("t"), moves, "`{a}` against `{b}`");
    assert_ne!(before.file, after.file, "the file image's key is the bytes");
}
