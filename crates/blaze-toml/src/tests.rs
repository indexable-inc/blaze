//! TOML 1.0's forms read and lowered, its refusals with offsets, the bounds, the forms Cargo's
//! manifests use (this workspace's own root manifest among them), the writer's exact text, the
//! editor's exact text for the edits blaze-new makes, and two properties (write then read gives
//! the value back; an edit inserts only its own bytes and reads back as the value set) with a
//! planted-bug control.

use std::{alloc::Global, format, string::String as StdString, string::ToString, vec::Vec as StdVec};

use blaze_data::{Int, Map, Value, value};
use blaze_own::String;
use blaze_sim::{Broke, Config, Tape};

use crate::{
    Datetime, Document, DocumentOptions, Edit, EditWhat, Error, Expected, Kind, Limits, MOST_KEY_PARTS, Origin,
    Unwritable, What,
    plant::{self, Plant},
    write_document, write_inline,
};

fn parse(text: &str) -> Document<'_, Global> {
    match Document::parse_in(text, Limits::CARGO, Global) {
        Ok(doc) => doc,
        Err(error) => panic!("{text:?}: {}", error.display(text)),
    }
}

fn lower(text: &str) -> Value<Global> {
    parse(text).to_value_in(Global)
}

fn refused(text: &str) -> Error {
    refused_in(text, Limits::CARGO)
}

fn refused_in(text: &str, limits: Limits) -> Error {
    match Document::parse_in(text, limits, Global) {
        Ok(doc) => panic!("{text:?} read as {:?}", doc.to_value_in(Global)),
        Err(error) => error,
    }
}

#[test]
fn every_form_reads_and_lowers() {
    let text = r#"
# a comment
title = "TOML" # after a value
num = [0x1F, 0o17, 0b101, -17, +5, 1_000, 0, -0]
fl = [1.5, -2e3, 6.626e-34, inf, -inf, 1e1_0]
s = ['lit\n', """
multi \
   line""", '''raw
''', "\u00e9\U0001F600", """two "" quotes"""""]
dotted.a.b = true
dotted . a . c = false
"quoted key" = 1
'' = "empty key"
arr = [
  1, # one
  2,
]
inl = { x = 1, y.z = [] }

[server]
host = "x"

[server.alpha]
ip = "10.0.0.1"

[[products]]
name = "a"

[[products]]
name = "b"
sub.x = 1

[products.more]
k = 2
"#;
    let expected = value!(in Global; {
        "title": "TOML",
        "num": [31, 15, 5, -17, 5, 1000, 0, 0],
        "fl": [1.5, -2000.0, 6.626e-34, f64::INFINITY, f64::NEG_INFINITY, 1e10],
        "s": ["lit\\n", "multi line", "raw\n", "\u{e9}\u{1F600}", "two \"\" quotes\"\""],
        "dotted": {"a": {"b": true, "c": false}},
        "quoted key": 1,
        "": "empty key",
        "arr": [1, 2],
        "inl": {"x": 1, "y": {"z": []}},
        "server": {"host": "x", "alpha": {"ip": "10.0.0.1"}},
        "products": [{"name": "a"}, {"name": "b", "sub": {"x": 1}, "more": {"k": 2}}]
    });
    assert_eq!(lower(text), expected);
    let doc = parse(text);
    let keys: StdVec<&str> = doc.root().children().map(|item| item.key()).collect();
    assert_eq!(keys[..3], ["title", "num", "fl"], "the text's order");
    assert_eq!(lower("n = nan").get("n").and_then(Value::as_f64).map(f64::is_nan), Some(true));
    assert_eq!(lower("a = 9223372036854775807\nb = -9223372036854775808"), value!(in Global; {"a": i64::MAX, "b": i64::MIN}));
    assert_eq!(lower(""), value!(in Global; {}));
    assert_eq!(lower("a = 1\r\nb = 2\r\n"), value!(in Global; {"a": 1, "b": 2}), "CRLF");
    assert_eq!(lower("[a.b]\nc = 1\n[a]\nd = 2"), value!(in Global; {"a": {"b": {"c": 1}, "d": 2}}), "implicit, then defined");
}

#[test]
fn datetimes_are_typed_and_checked() {
    let doc = parse("odt = 1979-05-27T07:32:00.999999-07:00\nz = 1979-05-27 07:32:00Z\nld = 2024-02-29\nlt = 00:32:00.5\nldt = 1979-05-27t07:32:60");
    let shown: StdVec<StdString> =
        doc.root().children().map(|item| item.as_datetime().map(Datetime::to_string).unwrap()).collect();
    assert_eq!(
        shown,
        ["1979-05-27T07:32:00.999999-07:00", "1979-05-27T07:32:00Z", "2024-02-29", "00:32:00.5", "1979-05-27T07:32:60"]
    );
    assert_eq!(doc.root().get("ld").unwrap().kind(), Kind::Datetime);
    assert_eq!(doc.to_value_in(Global).get("lt"), Some(&value!(in Global; "00:32:00.5")), "lowered as RFC 3339 text");
    assert_eq!(refused("a = 2026-02-29"), Error { at: 4, what: What::NoSuchDatetime });
    assert_eq!(refused("a = 2026-04-31"), Error { at: 4, what: What::NoSuchDatetime });
    assert_eq!(refused("a = 24:00:00"), Error { at: 4, what: What::NoSuchDatetime });
    assert_eq!(refused("a = 1979-05-27T07:32"), Error { at: 20, what: What::End { expected: Expected::DatetimePart } });
}

#[test]
fn refusals_name_the_byte_and_the_first_place() {
    let cases: &[(&str, Error)] = &[
        ("a = 1\na = 2", Error { at: 6, what: What::Duplicate { first: 0 } }),
        ("[a]\n[a]", Error { at: 5, what: What::Redefined { first: 1 } }),
        ("a.b = 1\n[a]", Error { at: 9, what: What::Redefined { first: 0 } }),
        ("[a.b]\n[a]\n[a]", Error { at: 11, what: What::Redefined { first: 1 } }),
        ("[a]\nb.c = 1\n[a.b]", Error { at: 15, what: What::Redefined { first: 4 } }),
        ("a = {}\n[a.b]", Error { at: 8, what: What::Closed { first: 0 } }),
        ("a = { b = 1 }\na.c = 2", Error { at: 14, what: What::Closed { first: 0 } }),
        ("a = [1]\n[[a]]", Error { at: 10, what: What::NotAnArrayOfTables { first: 0 } }),
        ("a = 1\na.b = 2", Error { at: 6, what: What::NotATable { first: 0 } }),
        ("a = 1\nb", Error { at: 7, what: What::End { expected: Expected::Equals } }),
        ("a = 1 b", Error { at: 6, what: What::Unexpected { found: 'b', expected: Expected::EndOfLine } }),
        ("a = { b = 1, }", Error { at: 13, what: What::Unexpected { found: '}', expected: Expected::Key } }),
        ("a = [1 2]", Error { at: 7, what: What::Unexpected { found: '2', expected: Expected::CommaOrEndArray } }),
        ("[a", Error { at: 2, what: What::End { expected: Expected::CloseHeader } }),
        ("[[a]", Error { at: 3, what: What::Unexpected { found: ']', expected: Expected::CloseArrayHeader } }),
        ("a = 01", Error { at: 4, what: What::LeadingZero }),
        ("a = 1__2", Error { at: 5, what: What::Underscore }),
        ("a = 1_", Error { at: 5, what: What::Underscore }),
        ("a = 1.", Error { at: 6, what: What::End { expected: Expected::Digit } }),
        ("a = .5", Error { at: 4, what: What::Unexpected { found: '.', expected: Expected::Value } }),
        ("a = 9223372036854775808", Error { at: 4, what: What::IntegerOutOfRange }),
        ("a = 0x8000000000000000", Error { at: 4, what: What::IntegerOutOfRange }),
        ("a = 1e400", Error { at: 4, what: What::FloatOutOfRange }),
        ("a = \"\\q\"", Error { at: 6, what: What::BadEscape { found: 'q' } }),
        ("a = \"\\uD800\"", Error { at: 5, what: What::NotAScalar { value: 0xD800 } }),
        ("a = \"x\ny\"", Error { at: 6, what: What::Control { ch: '\n' } }),
        ("a = 'x\u{7f}'", Error { at: 6, what: What::Control { ch: '\u{7f}' } }),
        ("a = \"x", Error { at: 6, what: What::End { expected: Expected::Quote } }),
        ("a = 1\r", Error { at: 5, what: What::BareCarriageReturn }),
        ("# \u{1}", Error { at: 2, what: What::Control { ch: '\u{1}' } }),
        ("a = tru", Error { at: 7, what: What::End { expected: Expected::Value } }),
        ("= 1", Error { at: 0, what: What::Unexpected { found: '=', expected: Expected::Key } }),
    ];
    for (text, error) in cases {
        assert_eq!(refused(text), *error, "{text:?}");
    }
    let error = refused("a = 1\n\n  b = 1\n  b = 2");
    assert_eq!(error.line_col("a = 1\n\n  b = 1\n  b = 2"), (4, 3));
    assert_eq!(
        error.display("a = 1\n\n  b = 1\n  b = 2").to_string(),
        "line 4, column 3: the key is already defined at byte 9"
    );
}

#[test]
fn bounds_are_parameters_and_refused() {
    let limits = Limits { most_depth: 2, ..Limits::CARGO };
    assert!(Document::parse_in("a = [[1]]", limits, Global).is_ok());
    assert_eq!(refused_in("a = [[[1]]]", limits), Error { at: 6, what: What::TooDeep { most: 2 } });
    assert_eq!(refused_in("a = [{ b = [1] }]", limits), Error { at: 11, what: What::TooDeep { most: 2 } });
    let limits = Limits { most_nodes: 2, ..Limits::CARGO };
    assert_eq!(refused_in("a = 1\nb = 2", limits), Error { at: 10, what: What::TooManyNodes { most: 2 } });
    let limits = Limits { most_bytes: 4, ..Limits::CARGO };
    assert_eq!(refused_in("a = 1", limits), Error { at: 0, what: What::TooLarge { len: 5, most: 4 } });
    let long = ["k"; MOST_KEY_PARTS + 1].join(".");
    assert_eq!(refused(&format!("{long} = 1")).what, What::KeyTooLong { most: MOST_KEY_PARTS });
    let most = ["k"; MOST_KEY_PARTS].join(".");
    assert!(Document::parse_in(&format!("{most} = 1"), Limits::CARGO, Global).is_ok());
}

#[test]
fn a_key_twice_is_refused_in_every_table_form() {
    assert_eq!(refused("a = { b = 1, b = 2 }").what, What::Duplicate { first: 6 });
    assert_eq!(refused("a = { b.c = 1, b.c = 2 }").what, What::Duplicate { first: 8 });
    assert_eq!(refused("[[t]]\nx = 1\nx = 2").what, What::Duplicate { first: 6 });
    assert_eq!(refused("[t]\n\"x\" = 1\n'x' = 2").what, What::Duplicate { first: 4 });
    // One key per element of an array of tables: the same key in the next element is no twice.
    assert!(Document::parse_in("[[t]]\nx = 1\n[[t]]\nx = 2", Limits::CARGO, Global).is_ok());
}

/// `body` on a thread with a 64 KiB stack, where a recursion on depth overflows long before a
/// million levels.
fn small_stack(body: impl FnOnce() + Send + 'static) {
    let thread = std::thread::Builder::new().stack_size(64 << 10).spawn(body).unwrap();
    thread.join().unwrap();
}

/// `n` levels, arrays and inline tables (key `k`) by turns, around a `1`, as the inline writer
/// lays them out.
fn nested_toml(n: usize) -> StdString {
    let mut text = StdString::new();
    for i in 0..n {
        text.push_str(if i % 2 == 0 { "[" } else { "{ k = " });
    }
    text.push('1');
    for i in (0..n).rev() {
        text.push_str(if i % 2 == 0 { "]" } else { " }" });
    }
    text
}

#[test]
fn deep_nesting_reads_lowers_writes_and_drops_in_a_small_stack() {
    for (n, limits) in [(64, Limits::CARGO), (1_000_000, Limits { most_depth: 1_000_000, ..Limits::CARGO })] {
        small_stack(move || {
            let inner = nested_toml(n);
            let text = format!("a = {inner}\n");
            let doc = Document::parse_in(&text, limits, Global).unwrap();
            let value = doc.to_value_in(Global);
            let mut out = String::new_in(Global);
            write_inline(value.get("a").unwrap(), &mut out).unwrap();
            assert_eq!(out.as_str(), inner, "write(lower(read(text))) is the text");
            let mut document = String::new_in(Global);
            write_document(value.as_map().unwrap(), &mut document, DocumentOptions::CARGO).unwrap();
            let again = Document::parse_in(document.as_str(), limits, Global).unwrap().to_map_in(Global);
            let mut out = String::new_in(Global);
            write_inline(again.get("a").unwrap(), &mut out).unwrap();
            assert_eq!(out.as_str(), inner, "the document written reads back the same");
            let past = format!("a = {}\n", nested_toml(n + 1));
            assert_eq!(refused_in(&past, limits).what, What::TooDeep { most: n });
            drop(value);
            drop(again);
            drop(doc);
        });
    }
}

#[test]
fn spans_are_the_text_as_written() {
    let text = "[package]\nname = \"x\" # c\n\"a b\".c = 'y'\n[dependencies]\nserde = { version = \"1\" }\n";
    let doc = parse(text);
    let package = doc.root().get("package").unwrap();
    assert_eq!(&text[package.span()], "[package]");
    assert_eq!(package.origin(), Origin::Header);
    let name = package.get("name").unwrap();
    assert_eq!((&text[name.key_span()], &text[name.span()]), ("name", "\"x\""));
    assert_eq!(name.line_col(), (2, 8));
    let ab = package.get("a b").unwrap();
    assert_eq!((&text[ab.key_span()], ab.origin()), ("\"a b\"", Origin::Dotted));
    assert_eq!(ab.get("c").unwrap().as_str(), Some("y"));
    let serde = doc.root().get("dependencies").unwrap().get("serde").unwrap();
    assert_eq!((&text[serde.span()], serde.origin()), ("{ version = \"1\" }", Origin::Inline));
    assert_eq!(serde.parent().map(|p| p.key()), Some("dependencies"));
}

#[test]
fn cargo_manifest_forms_read() {
    let text = r#"
[package]
name = "x"
version.workspace = true
edition = { workspace = true }

[target.'cfg(unix)'.dependencies]
libc = "0.2"

[target."cfg(target_os = \"macos\")".dependencies]
core-foundation = { version = "0.10", default-features = false, features = ["x"] }

[patch.'crates-io']
foo = { path = "../foo" }

[workspace.metadata.blaze]
strict = true

[profile.dev.package."*"]
opt-level = 3

[[bin]]
name = "a"
path = "src/a.rs"
"#;
    let v = lower(text);
    assert_eq!(v.get("package").and_then(|p| p.get("version")), Some(&value!(in Global; {"workspace": true})));
    let targets = v.get("target").and_then(Value::as_map).unwrap();
    let keys: StdVec<&str> = targets.iter().map(|(k, _)| k).collect();
    assert_eq!(keys, ["cfg(unix)", "cfg(target_os = \"macos\")"]);
    assert_eq!(
        v.get("profile").and_then(|p| p.get("dev")).and_then(|d| d.get("package")).and_then(|p| p.get("*")),
        Some(&value!(in Global; {"opt-level": 3}))
    );
    assert_eq!(v.get("bin"), Some(&value!(in Global; [{"name": "a", "path": "src/a.rs"}])));
    let doc = parse(text);
    let edit = Edit::new_in(&doc, Global);
    assert_eq!(edit.finish().unwrap().as_str(), text, "no edits: byte for byte");
}

#[test]
fn this_workspace_manifest_reads_and_round_trips_untouched() {
    let text = include_str!("../../../Cargo.toml");
    let doc = parse(text);
    let members = doc.root().get("workspace").and_then(|w| w.get("members")).unwrap();
    assert!(members.is_array() && !members.is_empty());
    assert_eq!(Edit::new_in(&doc, Global).finish().unwrap().as_str(), text);
}

fn edited(text: &str, edit: impl FnOnce(&Document<'_, Global>, &mut Edit<'_, '_, Global, Global>)) -> StdString {
    let doc = parse(text);
    let mut e = Edit::new_in(&doc, Global);
    edit(&doc, &mut e);
    let out = e.finish().unwrap().as_str().to_string();
    let _ = parse(&out);
    out
}

#[test]
fn members_are_added_in_every_layout() {
    let text = "[workspace]\nmembers = [\n    \"crates/a\",\n    \"crates/c\",\n]\nresolver = \"2\"\n";
    let out = edited(text, |doc, e| {
        let members = doc.root().get("workspace").unwrap().get("members").unwrap();
        e.insert(members, 1, &value!(in Global; "crates/b")).unwrap();
        e.push(members, &value!(in Global; "crates/d")).unwrap();
    });
    assert_eq!(
        out,
        "[workspace]\nmembers = [\n    \"crates/a\",\n    \"crates/b\",\n    \"crates/c\",\n    \"crates/d\",\n]\nresolver = \"2\"\n"
    );
    let push = |text: &str| {
        edited(text, |doc, e| {
            let members = doc.root().get("members").unwrap();
            e.push(members, &value!(in Global; "z")).unwrap();
        })
    };
    assert_eq!(push("members = [\"a\", \"b\"] # kept\n"), "members = [\"a\", \"b\", \"z\"] # kept\n");
    assert_eq!(push("members = [\"a\",]"), "members = [\"a\", \"z\",]");
    assert_eq!(push("members = []"), "members = [\"z\"]");
    assert_eq!(push("members = [\n  \"a\"\n]"), "members = [\n  \"a\",\n  \"z\"\n]");
    assert_eq!(push("members = [\n  \"a\", # one\n]"), "members = [\n  \"a\",\n  \"z\", # one\n]");
}

#[test]
fn set_keeps_every_other_byte() {
    let text = "# top\n[package]\nname = \"x\" # the name\n\n# deps\n[dependencies]\nserde = { version = \"1\" }\nempty = {}";
    let out = edited(text, |doc, e| {
        let root = doc.root();
        let package = root.get("package").unwrap();
        e.set(package, "version", &value!(in Global; "0.1.0")).unwrap();
        e.set(package, "name", &value!(in Global; "y")).unwrap();
        let deps = root.get("dependencies").unwrap();
        e.set(deps.get("serde").unwrap(), "features", &value!(in Global; ["derive"])).unwrap();
        e.set(deps.get("empty").unwrap(), "path", &value!(in Global; "../e")).unwrap();
        e.set(deps, "libc", &value!(in Global; "0.2")).unwrap();
        e.set(root, "cargo-features", &value!(in Global; [])).unwrap();
    });
    assert_eq!(
        out,
        "cargo-features = []\n# top\n[package]\nname = \"y\" # the name\nversion = \"0.1.0\"\n\n# deps\n[dependencies]\n\
         serde = { version = \"1\", features = [\"derive\"] }\nempty = { path = \"../e\" }\nlibc = \"0.2\"\n"
    );
    assert_eq!(
        edited("[a]", |doc, e| e.set(doc.root().get("a").unwrap(), "k", &value!(in Global; 1)).unwrap()),
        "[a]\nk = 1\n"
    );
    assert_eq!(
        edited("[[bin]]\nname = \"a\"\n[[bin]]\nname = \"b\"\n", |doc, e| {
            let second = doc.root().get("bin").unwrap().at(1).unwrap();
            e.set(second, "test", &value!(in Global; false)).unwrap();
        }),
        "[[bin]]\nname = \"a\"\n[[bin]]\nname = \"b\"\ntest = false\n"
    );
}

#[test]
fn edits_with_no_one_place_are_refused() {
    let text = "a.b = 1\n[c.d]\n[[e]]\nf = [1]\n";
    let doc = parse(text);
    let root = doc.root();
    let mut e = Edit::new_in(&doc, Global);
    let what = |r: Result<(), crate::EditError<Global>>| r.unwrap_err().what;
    assert!(matches!(what(e.set(root.get("a").unwrap(), "x", &value!(in Global; 1))), EditWhat::NotEditable { origin: Origin::Dotted }));
    assert!(matches!(what(e.set(root.get("c").unwrap(), "x", &value!(in Global; 1))), EditWhat::NotEditable { origin: Origin::Implicit }));
    assert!(matches!(what(e.set(root, "a", &value!(in Global; 1))), EditWhat::IsATable));
    assert!(matches!(what(e.push(root.get("e").unwrap(), &value!(in Global; {}))), EditWhat::NotEditable { origin: Origin::Header }));
    let f = root.get("e").unwrap().at(0).unwrap().get("f").unwrap();
    assert!(matches!(what(e.insert(f, 2, &value!(in Global; 2))), EditWhat::Index { index: 2, len: 1 }));
    assert!(matches!(what(e.set(f, "x", &value!(in Global; 1))), EditWhat::NotATable { kind: Kind::Array }));
    assert!(matches!(what(e.push(f, &value!(in Global; null))), EditWhat::Value(_)));
    let other = parse("x = 1");
    assert!(matches!(what(e.set(other.root(), "y", &value!(in Global; 1))), EditWhat::OtherDocument));
    e.replace(f, &value!(in Global; [2])).unwrap();
    e.replace(f, &value!(in Global; [3])).unwrap();
    assert!(matches!(e.finish().unwrap_err().what, EditWhat::Overlap { .. }));
}

#[test]
fn the_writer_writes_cargo_layout() {
    let v = value!(in Global; {
        "package": {"name": "x", "version": "0.1.0"},
        "dependencies": {"serde": {"version": "1", "features": ["derive"]}, "a b": "2"},
        "workspace": {"metadata": {"blaze": {"strict": true}}},
        "bin": [{"name": "a"}, {"name": "b"}]
    });
    let mut out = String::new_in(Global);
    write_document(v.as_map().unwrap(), &mut out, DocumentOptions::CARGO).unwrap();
    assert_eq!(
        out.as_str(),
        "[package]\nname = \"x\"\nversion = \"0.1.0\"\n\n[dependencies]\nserde = { version = \"1\", features = [\"derive\"] }\n\
         \"a b\" = \"2\"\n\n[workspace]\nmetadata = { blaze = { strict = true } }\n\n[[bin]]\nname = \"a\"\n\n[[bin]]\nname = \"b\"\n"
    );
    let deep = DocumentOptions { inline_from_depth: usize::MAX };
    let mut out = String::new_in(Global);
    write_document(value!(in Global; {"a": {"b": {"c": 1}}, "t": "\u{1}\"\\\u{7f}"}).as_map().unwrap(), &mut out, deep).unwrap();
    assert_eq!(out.as_str(), "t = \"\\u0001\\\"\\\\\\u007F\"\n\n[a.b]\nc = 1\n");
    let mut out = String::new_in(Global);
    write_inline(&value!(in Global; [1.0, -0.0, 1e300, f64::NAN, f64::NEG_INFINITY]), &mut out).unwrap();
    assert_eq!(out.as_str(), "[1.0, -0.0, 1e300, nan, -inf]");
}

#[test]
fn the_writer_refuses_by_path() {
    let mut out = String::new_in(Global);
    let error = write_document(value!(in Global; {"a": {"b": [1, null]}}).as_map().unwrap(), &mut out, DocumentOptions::CARGO).unwrap_err();
    assert_eq!((error.path.as_str(), error.what), ("a.b[1]", Unwritable::Null));
    assert_eq!(error.to_string(), "a.b[1]: null has no TOML form");
    let mut out = String::new_in(Global);
    let error = write_inline(&value!(in Global; u64::MAX), &mut out).unwrap_err();
    assert_eq!((error.path.as_str(), error.what), ("", Unwritable::Integer(u64::MAX)));
    let mut bytes = blaze_own::Vec::new_in(Global);
    bytes.push(1u8);
    let mut map = Map::new_in(Global);
    map.insert(String::from_str_in("x", Global), Value::Bytes(bytes));
    let mut out = String::new_in(Global);
    assert_eq!(write_document(&map, &mut out, DocumentOptions::CARGO).unwrap_err().what, Unwritable::Bytes);
}

/// A value TOML can say, `depth` levels at most.
fn draw_value(tape: &Tape, depth: u32) -> Value<Global> {
    let kinds: u8 = if depth == 0 { 3 } else { 5 };
    match tape.int_in(0u8..=kinds, "kind") {
        0 => Value::Bool(tape.coin("bool")),
        1 => Value::Int(Int::from_i64(tape.int_in(i64::MIN..=i64::MAX, "i64"))),
        2 => Value::Float(tape.f64_finite("float")),
        3 => Value::Text(String::from_str_in(&tape.text_from("a\"\\'\n\u{1}é ", 0..=6, "text"), Global)),
        4 => {
            let mut list = blaze_own::Vec::new_in(Global);
            for item in tape.vec(0..=3, "list", |t| draw_value(t, depth - 1)) {
                list.push(item);
            }
            Value::List(list)
        }
        _ => draw_map(tape, depth - 1),
    }
}

fn draw_map(tape: &Tape, depth: u32) -> Value<Global> {
    let mut map = Map::new_in(Global);
    for (key, value) in tape.vec(0..=4, "map", |t| (t.text_from("ab-. \"é", 0..=3, "key"), draw_value(t, depth))) {
        map.insert(String::from_str_in(&key, Global), value);
    }
    Value::Map(map)
}

fn harness<E: core::fmt::Display>(error: E) -> Broke {
    Broke::Harness(error.to_string())
}

/// read(write(v)) == v for documents; and setting a new key at the root inserts only its own
/// bytes and reads back as the value with that key.
fn round_trip(tape: &Tape) -> Result<(), Broke> {
    let v = draw_map(tape, 3);
    let Value::Map(map) = &v else { unreachable!("draw_map draws a map") };
    let inline_from_depth = tape.int_in(1usize..=4, "inline_from_depth");
    let mut text = String::new_in(Global);
    write_document(map, &mut text, DocumentOptions { inline_from_depth }).map_err(harness)?;
    let text = text.as_str();
    let doc = Document::parse_in(text, Limits::CARGO, Global)
        .map_err(|e| Broke::Oracle(format!("{v:?} wrote {text:?}, which reads as {}", e.display(text))))?;
    let back = doc.to_value_in(Global);
    if back != v {
        return Err(Broke::Oracle(format!("the round trip of {v:?} through {text:?} gave {back:?}")));
    }
    let key = format!("new{}", tape.text_from("xy", 0..=2, "new key"));
    if map.contains_key(&key) {
        return Ok(());
    }
    let new = draw_value(tape, 1);
    let mut edit = Edit::new_in(&doc, Global);
    edit.set(doc.root(), &key, &new).map_err(harness)?;
    let out = edit.finish().map_err(harness)?;
    let out = out.as_str();
    let prefix = text.bytes().zip(out.bytes()).take_while(|(a, b)| a == b).count();
    let suffix = text.bytes().rev().zip(out.bytes().rev()).take_while(|(a, b)| a == b).count();
    if prefix + suffix < text.len() {
        return Err(Broke::Oracle(format!("setting {key:?} in {text:?} changed other bytes: {out:?}")));
    }
    let mut expected = map.clone();
    expected.insert(String::from_str_in(&key, Global), new);
    match Document::parse_in(out, Limits::CARGO, Global) {
        Ok(doc) if doc.to_value_in(Global) == Value::Map(expected) => Ok(()),
        Ok(doc) => Err(Broke::Oracle(format!("setting {key:?} gave {out:?}, read as {:?}", doc.to_value_in(Global)))),
        Err(e) => Err(Broke::Oracle(format!("setting {key:?} gave {out:?}: {}", e.display(out)))),
    }
}

#[test]
fn the_round_trip_and_the_edit_hold() {
    blaze_sim::check(Config::cases(400), round_trip);
}

#[test]
fn the_oracle_catches_a_short_body_end() {
    let run = |tape: &Tape, p: Plant| plant::with(p, || round_trip(tape));
    blaze_sim::control(Config::cases(400), Plant::Clean, Plant::BodyEndShort, "setting", run);
}
