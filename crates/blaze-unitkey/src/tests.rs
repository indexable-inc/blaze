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
use blaze_sim::{Config, Tape};
use cfg::CfgOptions;
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
    let unit = Unit {
        tool: TOOL,
        settings: settings(on),
        unit: unit_id,
        cfg: &options,
        files: &files,
        answers: &[],
        names: &[ContentId([0; 32])],
    };
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

fn krate(tape: &Tape) -> Crate {
    let count = usize::from(tape.int_in(2..=5u8, "functions"));
    let mut lits = Vec::new_in(Global);
    let mut calls = Vec::new_in(Global);
    for _ in 0..count {
        lits.push(tape.int_in(0..=200u8, "literal"));
        let mut called = Vec::new_in(Global);
        for other in 0..count as u8 {
            if tape.bool("calls") {
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

    fn draw_at(&self, tape: &Tape) -> usize {
        usize::from(tape.int_in(0..=self.lits.len() as u8 - 1, "at"))
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
#[test]
fn a_comment_or_whitespace_edit_moves_only_the_file_and_the_manifest() {
    blaze_sim::check(Config::cases(100), |tape| {
        let krate = krate(tape);
        let at = krate.draw_at(tape);
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
    });
}

/// The invariant: a private body edit moves exactly that body's key, the file's and the
/// manifest's, never the surface's.
#[test]
fn a_private_body_edit_moves_exactly_that_body() {
    blaze_sim::check(Config::cases(100), |tape| {
        let krate = krate(tape);
        let at = krate.draw_at(tape);
        let paths = krate.paths();
        let paths = path_refs(&paths);
        let before = run(&krate.render(at, Change::None, ""));
        let after = run(&krate.render(at, Change::Body, ""));
        assert_ne!(before.file, after.file);
        assert_ne!(before.manifest, after.manifest);
        assert_eq!(before.surface, after.surface, "a body is not in the surface");
        assert_eq!(after.moved(&before, &paths), [paths[at]]);
    });
}

/// The invariant: a header edit moves the edited item's body key and the surface key, and no
/// other item's body key. The callers of the edited function keep theirs: the old claim that a
/// signature edit moves its mentioners' keys is gone (D39). What a caller read of the signature
/// is in its record's shape, so it is redone by its read, never by its key.
#[test]
fn a_header_edit_moves_its_body_and_the_surface_only() {
    blaze_sim::check(Config::cases(100), |tape| {
        let krate = krate(tape);
        let at = krate.draw_at(tape);
        let paths = krate.paths();
        let paths = path_refs(&paths);
        let before = run(&krate.render(at, Change::None, ""));
        let after = run(&krate.render(at, Change::Header, ""));
        assert_ne!(before.surface, after.surface, "a header is in the surface");
        assert_eq!(after.moved(&before, &paths), [paths[at]], "a caller's key holds nothing of its callee");
    });
}

/// The invariant: an edit in another unit moves no key of this unit: keying reads nothing of
/// another unit (`Unit` has no field for one). Under the text-closure rule it did: a body's key
/// held the closures of the names it mentions across its universe, and a surface and a manifest
/// held their dependencies' surface keys. The control: the edit moves the other unit's surface.
#[test]
fn an_edit_in_another_unit_moves_no_key_of_this_one() {
    blaze_sim::check(Config::cases(60), |tape| {
        let this = krate(tape);
        let other = krate(tape);
        let at = other.draw_at(tape);
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
    });
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
#[test]
fn each_escape_rule_with_its_control() {
    blaze_sim::check(Config::cases(100), |tape| {
        let case = &CASES[usize::from(tape.int_in(0..=CASES.len() as u8 - 1, "case"))];
        let from = usize::from(tape.int_in(0..=3u8, "from"));
        let to = (from + 1 + usize::from(tape.int_in(0..=2u8, "to"))) % WIDTHS.len();
        let filler = tape.int_in(0..=3u8, "filler");
        let before = run(&render_case(case.item, WIDTHS[from], filler));
        let after = run(&render_case(case.item, WIDTHS[to], filler));
        assert_eq!(before.surface != after.surface, case.surface_moves, "rule `{}`", case.rule);
        for index in 0..filler {
            let path = format!("filler{index}");
            assert_eq!(before.expect_body(&path), after.expect_body(&path), "rule `{}`: {path}", case.rule);
        }
    });
}

/// The invariant: an impl written in a body is in the surface, the body's statements are not.
/// Editing the impl moves the surface key; a plain statement edit in the same body moves that
/// body's key and not the surface's (the control).
#[test]
fn a_literal_impl_in_a_body_is_in_the_surface() {
    blaze_sim::check(Config::cases(60), |tape| {
        let krate = krate(tape);
        let width = usize::from(tape.int_in(0..=2u8, "width"));
        let lit = tape.int_in(0..=200u8, "lit");
        let prelude = |width: &str, lit: u8| {
            format!("struct S;\nfn host() {{ impl S {{ fn x() -> u{width} {{ 0 }} }} let _y = {lit}; }}\n")
        };
        let base = run(&krate.render(0, Change::None, &prelude(WIDTHS[width], lit)));
        let impl_edit = run(&krate.render(0, Change::None, &prelude(WIDTHS[width + 1], lit)));
        let statement_edit = run(&krate.render(0, Change::None, &prelude(WIDTHS[width], lit.wrapping_add(1))));
        assert_ne!(base.surface, impl_edit.surface, "the escaping impl is in `host`'s piece");
        assert_eq!(base.surface, statement_edit.surface, "a statement is not");
        assert_ne!(base.expect_body("host"), statement_edit.expect_body("host"));
    });
}

/// The invariant: whether a body holds a macro that is not quiet is one flag in its node's
/// piece: gaining or losing such a call moves the surface key; editing the call's arguments does
/// not (what it makes is a fact its task makes). The control: gaining a quiet call does not move
/// it. A derive in a body counts: the impls it makes are in no text.
#[test]
fn a_non_quiet_macro_in_a_body_is_one_flag() {
    blaze_sim::check(Config::cases(60), |tape| {
        let krate = krate(tape);
        let arg = tape.int_in(0..=200u8, "arg");
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
    });
}

/// The invariant: an item whose `#[cfg]` is off is no item of the unit: no body key, no piece
/// (the surface is the one of the text without it). Switching the option on gives it its key and
/// moves every key through `settings`. The control: an ungated item has a key either way.
#[test]
fn an_item_off_by_cfg_has_no_key() {
    blaze_sim::check(Config::cases(60), |tape| {
        let krate = krate(tape);
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
    });
}

// --- stable ids -----------------------------------------------------------------------------------

/// Every row's stable id in file order, of a file at the crate root.
fn ids(text: &[u8]) -> Vec<StableId, Global> {
    let imaged = image::image(&TOOL, READING, text, Global).expect("the file images");
    let aligned = Aligned::copy_from(&imaged.record);
    let opened = crate::read::Image::open(&aligned, &imaged.key).expect("the image opens");
    let mut out = Vec::new_in(Global);
    for row in 0..opened.items().len() as u32 {
        out.push(opened.stable_id(b"", row).expect("a row's id"));
    }
    out
}

/// A unit of one file at the crate root, keyed with every option of `on` set.
fn key_with(text: &[u8], on: &[&str]) -> Result<unit::UnitKeys<Global>, crate::KeyError> {
    let imaged = image::image(&TOOL, READING, text, Global).expect("the file images");
    let aligned = Aligned::copy_from(&imaged.record);
    let files = [UnitFile { key: imaged.key, image: &aligned, module: b"" }];
    let mut options = CfgOptions::default();
    for atom in on {
        options.insert_atom(Symbol::intern(atom));
    }
    let unit = Unit {
        tool: TOOL,
        settings: settings(true),
        unit: THIS,
        cfg: &options,
        files: &files,
        answers: &[],
        names: &[ContentId([0; 32])],
    };
    unit::key(&unit, Global)
}

/// What Rust lets one file hold under one (path, kind): three equal functions under three
/// `#[cfg]`s (all on here: keying decides ids before cfg, and a cfg must move none), a macro
/// called three times at item level, three equal impls, as core's `fmt/num.rs` and std's
/// platform modules have them.
const REPEATS: &str = "struct S;\n\
    #[cfg(a)] fn f() -> u32 { 1 }\n#[cfg(b)] fn f() -> u32 { 2 }\n#[cfg(c)] fn f() -> u32 { 3 }\n\
    m! { 1 }\nm! { 2 }\nm! { 3 }\n\
    #[cfg(a)] impl S { fn x() {} }\n#[cfg(b)] impl S { fn x() {} }\n#[cfg(c)] impl S { fn x() {} }\n";

/// The invariant: every row of a file has its own stable id, however many rows share one name or
/// header, so a unit of them keys. The controls, each failing by name: with no suffix, and with
/// the rule this one replaced (each row compared with its predecessor's new name, which gives
/// the third of three the first's path), keying refuses the unit by `KeyError::TwoItems` naming
/// two rows of the file.
#[test]
fn equal_siblings_have_their_own_ids() {
    use crate::image::{REPEAT_PLANT, RepeatPlant};
    let all = ids(REPEATS.as_bytes());
    let mut sorted = all.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), all.len(), "every row's id is its own");
    let keys = key_with(REPEATS.as_bytes(), &["a", "b", "c"]).expect("the unit keys");
    assert_eq!(keys.items.len(), 6, "three `f` and three `x`, each its own body");
    for plant in [RepeatPlant::NoSuffix, RepeatPlant::AgainstRenamed] {
        REPEAT_PLANT.set(plant);
        let refused = key_with(REPEATS.as_bytes(), &["a", "b", "c"]);
        REPEAT_PLANT.set(RepeatPlant::None);
        match refused {
            Err(crate::KeyError::TwoItems { first, second, path, .. }) => {
                assert_eq!((first.file, second.file), (0, 0), "{plant:?}: both in the one file");
                assert_ne!(first.row, second.row, "{plant:?}: two rows named");
                assert!(!path.is_empty(), "{plant:?}: the item's path named");
            }
            Err(other) => panic!("{plant:?}: refused by {other}, not by two items of one id"),
            Ok(_) => panic!("{plant:?}: the planted rule keyed a unit with two items of one id"),
        }
    }
}

/// Modules a macro call writes, as core's `io/io_slice.rs` (`cfg_select!` arms with
/// `#[path]`s) and libc (`cfg_if!`) write them; an inline module in the tokens; an attribute
/// whose `#[path]` belongs to no `mod`; `mod $name;`, which spells no file; a call that is off;
/// beside a plain module.
const CALL_MODS: &str = "cfg_select! {\n\
    unix => { #[path = \"io_slice/repr_iovec.rs\"] mod repr; }\n\
    _ => { #[path = \"io_slice/repr_unknown.rs\"] mod repr; }\n\
    }\n\
    cfg_if::cfg_if! {\n\
    if #[cfg(fast)] { pub(crate) mod fast; } else { mod slow { mod deep; } }\n\
    }\n\
    m! { #[path = \"wrong.rs\"] struct S; mod after; mod $name; }\n\
    #[cfg(off)] cfg_if! { if #[cfg(x)] { mod gone; } }\n\
    mod plain;\n";

/// Every candidate file of the active modules of `CALL_MODS` at `io/io_slice.rs`, sorted, each
/// once, with ` (call)` after one spelled in a call's tokens.
fn call_candidates() -> Vec<String, Global> {
    let imaged = image::image(&TOOL, READING, CALL_MODS.as_bytes(), Global).expect("the file images");
    let aligned = Aligned::copy_from(&imaged.record);
    let opened = crate::read::Image::open(&aligned, &imaged.key).expect("the image opens");
    let options = CfgOptions::default();
    let files =
        crate::mods::mod_files(&opened, b"io/io_slice.rs", crate::mods::FileKind::NonModRs, b"io::io_slice", &options, Global)
            .expect("the declarations have files");
    let mut out = Vec::new_in(Global);
    for file in files.iter() {
        for candidate in core::iter::once(&file.first).chain(file.second.as_ref()) {
            let mark = if file.in_call { " (call)" } else { "" };
            out.push(format!("{}{mark}", String::from_utf8_lossy(&candidate.path)));
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// The invariant ("Modules a macro call writes" in `crate::mods`): every file a `mod` in an
/// item-level call's tokens could name is a candidate, whatever cfg the tokens say (both
/// `repr` arms, both `cfg_if!` branches), its `#[path]` read from the attribute before it and
/// its inline modules giving their directory; a `#[path]` before another item is no module's,
/// `mod $name;` names nothing, and a call that is off names nothing. The controls, each failing
/// by name: with the tokens not read (the image of layout 3), core's `repr_iovec.rs` is
/// missing; with the `#[path]` in the tokens not read, it is missing too.
#[test]
fn modules_a_macro_call_writes_are_candidates() {
    use crate::image::{CALL_PLANT, CallPlant};
    let want = [
        "io/io_slice/after.rs (call)",
        "io/io_slice/after/mod.rs (call)",
        "io/io_slice/fast.rs (call)",
        "io/io_slice/fast/mod.rs (call)",
        "io/io_slice/plain.rs",
        "io/io_slice/plain/mod.rs",
        "io/io_slice/repr.rs (call)",
        "io/io_slice/repr/mod.rs (call)",
        "io/io_slice/repr_iovec.rs (call)",
        "io/io_slice/repr_unknown.rs (call)",
        "io/io_slice/slow/deep.rs (call)",
        "io/io_slice/slow/deep/mod.rs (call)",
    ];
    assert_eq!(call_candidates(), want, "the candidates of the modules the calls write");
    for plant in [CallPlant::Off, CallPlant::NoPath] {
        CALL_PLANT.set(plant);
        let planted = call_candidates();
        CALL_PLANT.set(CallPlant::None);
        assert!(
            !planted.iter().any(|file| file == "io/io_slice/repr_iovec.rs (call)"),
            "{plant:?}: the control still found core's `repr_iovec.rs` ({planted:?})"
        );
    }
}

/// std's `sys/pal` shape (`sys/pal/mod.rs`'s `cfg_select!`; `hermit/mod.rs:24`,
/// `wasi/mod.rs:11`, `teeos/mod.rs:10`): every platform's module is a call's candidate, and two
/// of them name unix's `time.rs` by `#[path = "../unix/time.rs"]`, so one file is three modules.
const PAL: [(&str, &str); 5] = [
    ("/u/src/lib.rs", "cfg_select! {\n unix => { mod unix; }\n target_os = \"hermit\" => { mod hermit; }\n _ => { mod wasi; }\n}\n"),
    ("/u/src/unix/mod.rs", "mod time;\n"),
    ("/u/src/hermit/mod.rs", "#[path = \"../unix/time.rs\"]\nmod time;\n"),
    ("/u/src/wasi/mod.rs", "#[path = \"../unix/time.rs\"]\nmod time;\n"),
    ("/u/src/unix/time.rs", "pub struct Instant(u64);\npub fn now() -> Instant { Instant(0) }\n"),
];

/// A source over `(path, text)` rows, for a gather of a few files.
struct Rows<'r>(&'r [(&'r str, &'r str)]);

impl crate::gather::Source for Rows<'_> {
    fn read<B: core::alloc::Allocator>(&mut self, path: &str, into: &mut Vec<u8, B>) -> Result<bool, crate::gather::SourceError> {
        into.clear();
        match self.0.iter().find(|(there, _)| *there == path) {
            Some((_, text)) => {
                into.extend_from_slice(text.as_bytes());
                Ok(true)
            }
            None => Ok(false),
        }
    }

    fn exists(&mut self, path: &str) -> Result<bool, crate::gather::SourceError> {
        Ok(self.0.iter().any(|(there, _)| *there == path))
    }
}

/// The digests of the few-file gathers: no unit of theirs reads late.
struct RowDigests;

impl crate::gather::Digests for RowDigests {
    fn var(value: Option<&[u8]>) -> ContentId {
        ContentId::of("blaze-unitkey test variable", value.unwrap_or(b""))
    }

    fn file(bytes: &[u8]) -> ContentId {
        ContentId::of("blaze-unitkey test file", bytes)
    }
}

fn no_pair(row: &(Vec<u8>, Vec<u8>)) -> (&[u8], &[u8]) {
    (&row.0, &row.1)
}

/// The unit at `/u/src/lib.rs` over `rows`, gathered, with the images it filed.
fn gather_rows(
    rows: &[(&str, &str)],
) -> (Result<crate::gather::UnitText<Global>, crate::gather::GatherError<Global>>, crate::gather::ImageList<Global>) {
    let options = CfgOptions::default();
    let env: [(Vec<u8>, Vec<u8>); 0] = [];
    let input = crate::gather::Gather {
        tool: TOOL,
        reading: READING,
        cfg: &options,
        root: b"/u/src/lib.rs",
        env: &env,
        pair: no_pair,
        member: false,
        defer: true,
    };
    let mut images = crate::gather::ImageList::new_in(Global);
    let text = crate::gather::gather::<_, _, RowDigests, _, _>(&input, &mut Rows(rows), &mut images, Global);
    (text, images)
}

/// `text` keyed, its file `rename`'s module path replaced by `module` when given; each file's
/// printed name a function of its path alone, as the command's (`program::printed_name_id`).
fn key_text(
    text: &crate::gather::UnitText<Global>,
    images: &crate::gather::ImageList<Global>,
    rename: Option<(usize, &[u8])>,
) -> Result<unit::UnitKeys<Global>, crate::KeyError> {
    use crate::gather::Images;
    let mut files = Vec::with_capacity_in(text.files.len(), Global);
    let mut names = Vec::with_capacity_in(text.files.len(), Global);
    for (at, file) in text.files.iter().enumerate() {
        let image = images.get(&file.image).expect("the gather filed every image");
        let module = match rename {
            Some((renamed, module)) if renamed == at => module,
            _ => &file.module[..],
        };
        files.push(UnitFile { key: file.image, image, module });
        names.push(ContentId::of("blaze-unitkey test printed name", &file.path));
    }
    let options = CfgOptions::default();
    let unit = Unit { tool: TOOL, settings: settings(true), unit: THIS, cfg: &options, files: &files, answers: &[], names: &names };
    unit::key(&unit, Global)
}

/// The invariant: one file named by two `#[path]`s is two modules (legal Rust), so the gather
/// gives it once per module path, every entry of one path and one image, and the unit keys,
/// though its printed names repeat. The controls, each failing by name: the same file declared
/// below itself is a cycle, refused by `gather::CIRCULAR`; and two of the entries given one
/// module path key to `KeyError::TwoItems`, so the distinct module paths are what keep them
/// apart.
#[test]
fn one_file_under_two_module_paths_gathers_and_keys() {
    let (text, images) = gather_rows(&PAL);
    let text = match text {
        Ok(text) => text,
        Err(error) => panic!("refused: {}: {} ({})", String::from_utf8_lossy(&error.path), String::from_utf8_lossy(&error.what), error.rule),
    };
    let time = b"/u/src/unix/time.rs";
    let mut modules: Vec<String, Global> = Vec::new_in(Global);
    let mut images_of = Vec::new_in(Global);
    let mut at_time = Vec::new_in(Global);
    for (at, file) in text.files.iter().enumerate() {
        if file.path[..] == time[..] {
            modules.push(String::from_utf8_lossy(&file.module).into_owned());
            images_of.push(file.image);
            at_time.push(at);
        }
    }
    modules.sort_unstable();
    assert_eq!(modules, ["hermit::time", "unix::time", "wasi::time"], "unix's `time.rs` once per module path naming it");
    assert!(images_of.windows(2).all(|pair| pair[0] == pair[1]), "one file, one image");
    assert_eq!(text.files.len(), 7, "the root, three platform modules, and `time.rs` three times");
    key_text(&text, &images, None).unwrap_or_else(|error| panic!("one file under three module paths does not key: {error}"));

    // Control 1: `time.rs` declaring itself is below itself.
    let mut cycle = PAL;
    cycle[4].1 = "#[path = \"time.rs\"]\nmod again;\n";
    match gather_rows(&cycle).0 {
        Err(error) => {
            assert_eq!(error.rule, crate::gather::CIRCULAR, "the cycle refused by its rule");
            assert_eq!(&error.path[..], &time[..], "the file below itself named");
        }
        Ok(_) => panic!("a file declaring itself was gathered"),
    }

    // Control 2: two entries under one module path are two items of one stable id.
    let first = text.files[at_time[0]].module.clone();
    match key_text(&text, &images, Some((at_time[1], &first))) {
        Err(crate::KeyError::TwoItems { first: one, second: two, .. }) => {
            assert!(at_time.contains(&(one.file as usize)) && at_time.contains(&(two.file as usize)), "both in `time.rs`");
        }
        Err(other) => panic!("refused by {other}, not by two items of one id"),
        Ok(_) => panic!("two entries under one module path keyed"),
    }
}

/// libc's shape (`src/macros.rs:63-66`, `src/lib.rs:245`): a `macro_rules!` whose body spells
/// `mod types;`, defined in one file and called in another's `cfg_if!` arm; a definition that
/// is off, whose call names nothing; a call of an unknown macro.
const MACRO_DEFS: &str = "macro_rules! prelude { () => { mod types; } }\n\
    #[cfg(off)] macro_rules! gone { () => { mod gone; } }\n";
const MACRO_CALLS: &str = "#[macro_use] mod macros;\n\
    cfg_if! { if #[cfg(x)] { mod fuchsia; prelude!(); } else { crate::prelude!(); } }\n\
    gone!();\n\
    unknown! { }\n";

/// Every candidate file the unit of `MACRO_CALLS` (`src/lib.rs`, the root) and `MACRO_DEFS`
/// (`src/macros.rs`) has through its macros' bodies, sorted, with the sites' count.
fn macro_candidates() -> (usize, Vec<String, Global>) {
    let options = CfgOptions::default();
    let imaged = [MACRO_CALLS, MACRO_DEFS].map(|text| image::image(&TOOL, READING, text.as_bytes(), Global).expect("the file images"));
    let aligned = [0usize, 1].map(|at| Aligned::copy_from(&imaged[at].record));
    let images = [0usize, 1].map(|at| crate::read::Image::open(&aligned[at], &imaged[at].key).expect("the image opens"));
    let files: [(&[u8], crate::mods::FileKind, &[u8]); 2] =
        [(b"src/lib.rs", crate::mods::FileKind::ModRs, b""), (b"src/macros.rs", crate::mods::FileKind::NonModRs, b"macros")];
    let mut done = Vec::new_in(Global);
    let sites = crate::mods::macro_sites(&images, &options, &mut done, Global).expect("the sites");
    assert!(crate::mods::macro_sites(&images, &options, &mut done, Global).expect("the sites").is_empty(), "a site twice");
    let mut out = Vec::new_in(Global);
    for site in sites.iter() {
        let (path, kind, module) = files[site.call_file as usize];
        let declared = crate::mods::macro_files(
            &images[site.def_file as usize],
            site.defined,
            &images[site.call_file as usize],
            site.called,
            path,
            kind,
            module,
            &options,
            Global,
        )
        .expect("the body's modules have files");
        for file in declared.iter() {
            assert!(file.in_call, "a module of a macro's body that is not a candidate");
            for candidate in core::iter::once(&file.first).chain(file.second.as_ref()) {
                out.push(format!("{} as {}", String::from_utf8_lossy(&candidate.path), String::from_utf8_lossy(&file.module)));
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    (sites.len(), out)
}

/// The invariant ("A macro the unit defines" in `crate::mods`): a `mod` in a `macro_rules!`
/// body is a candidate at each call of that name in the unit, the nested one in a `cfg_if!` arm
/// and the one by a path alike (two sites, one file pair), relative to the calling file; a
/// definition that is off and an unknown macro name nothing. The control, failing by name:
/// with definitions' bodies not read (the image of layout 4), libc's `types.rs` is missing.
#[test]
fn modules_a_macro_definition_writes_are_candidates() {
    use crate::image::{CALL_PLANT, CallPlant};
    let (sites, found) = macro_candidates();
    assert_eq!(sites, 2, "the calls of `prelude` ({found:?})");
    assert_eq!(found, ["src/types.rs as types", "src/types/mod.rs as types"], "the candidates of the modules the body writes");
    CALL_PLANT.set(CallPlant::NoDefinitions);
    let (_, planted) = macro_candidates();
    CALL_PLANT.set(CallPlant::None);
    assert!(
        !planted.iter().any(|file| file == "src/types.rs as types"),
        "NoDefinitions: the control still found libc's `types.rs` ({planted:?})"
    );
}

/// One generated top-level item: a function of one of two names (a body), a macro call with an
/// argument, or a unit struct of one of two names. Each is one row, so row `i` is item `i`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Piece {
    Fn { name: u8, lit: u8 },
    Call { arg: u8 },
    Struct { name: u8 },
}

impl Piece {
    fn draw(tape: &Tape) -> Piece {
        match tape.int_in(0..=2u8, "piece") {
            0 => Piece::Fn { name: tape.int_in(0..=1u8, "fn name"), lit: tape.int_in(0..=9u8, "lit") },
            1 => Piece::Call { arg: tape.int_in(0..=9u8, "arg") },
            _ => Piece::Struct { name: tape.int_in(0..=1u8, "struct name") },
        }
    }

    /// The group a piece's id is numbered in: equal (path, kind).
    fn group(self) -> (u8, u8) {
        match self {
            Piece::Fn { name, .. } => (0, name),
            Piece::Call { .. } => (1, 0),
            Piece::Struct { name } => (2, name),
        }
    }

    /// The piece with its body or argument edited: no name or header moves.
    fn edited(self) -> Piece {
        match self {
            Piece::Fn { name, lit } => Piece::Fn { name, lit: lit + 1 },
            Piece::Call { arg } => Piece::Call { arg: arg + 1 },
            Piece::Struct { name } => Piece::Struct { name },
        }
    }
}

fn render_pieces(pieces: &[Piece]) -> Vec<u8, Global> {
    let mut out = Vec::new_in(Global);
    for (at, piece) in pieces.iter().enumerate() {
        // Each under a cfg of its own, as equal siblings are in real code.
        match piece {
            Piece::Fn { name, lit } => write!(out, "#[cfg(p{at})] fn f{name}() -> u32 {{ {lit} }}\n"),
            Piece::Call { arg } => write!(out, "m! {{ {arg} }}\n"),
            Piece::Struct { name } => write!(out, "#[cfg(p{at})] struct T{name};\n"),
        }
        .expect("a Vec takes writes");
    }
    out
}

/// The invariant: an edit inside an item (a body, a call's arguments) moves no row's id, its own
/// included; inserting an item moves exactly the ids of the later members of its group
/// (equal path and kind), each by one, and no other row's. The control: when its group has a
/// later member, the insertion does move that member's id (the rule is by order, and says so).
#[test]
fn an_edit_moves_no_other_items_id() {
    blaze_sim::check(Config::cases(100), |tape| {
        let count = usize::from(tape.int_in(1..=8u8, "items"));
        let mut pieces = Vec::new_in(Global);
        for _ in 0..count {
            pieces.push(Piece::draw(tape));
        }
        let before = ids(&render_pieces(&pieces));
        assert_eq!(before.len(), count, "one row per item");

        let at = usize::from(tape.int_in(0..=count as u8 - 1, "edited"));
        let mut edited = pieces.clone();
        edited[at] = edited[at].edited();
        assert_eq!(ids(&render_pieces(&edited)), before, "an edit inside item {at} moves no id");

        let place = usize::from(tape.int_in(0..=count as u8, "inserted at"));
        let inserted = Piece::draw(tape);
        let mut grown = pieces.clone();
        grown.insert(place, inserted);
        let after = ids(&render_pieces(&grown));
        assert_eq!(after.len(), count + 1);
        for (row, piece) in pieces.iter().enumerate() {
            let now = if row < place { row } else { row + 1 };
            // The control is the `true` side: a later member of the inserted item's group moves.
            let moves = row >= place && piece.group() == inserted.group();
            assert_eq!(after[now] != before[row], moves, "row {row} ({piece:?}) after inserting {inserted:?} at {place}");
        }
    });
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
#[test]
fn tokens_are_hashed_as_tokens() {
    blaze_sim::check(Config::cases(60), |tape| {
        let (a, b, moves) = TOKENS[usize::from(tape.int_in(0..=TOKENS.len() as u8 - 1, "case"))];
        let filler = tape.int_in(0..=3u8, "filler");
        let render = |item: &str| render_case(item, "", filler);
        let before = run(&render(a));
        let after = run(&render(b));
        assert_eq!(before.expect_body("t") != after.expect_body("t"), moves, "`{a}` against `{b}`");
        assert_ne!(before.file, after.file, "the file image's key is the bytes");
    });
}
