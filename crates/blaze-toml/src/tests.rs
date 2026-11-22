//! TOML 1.0's forms read and lowered, its refusals with offsets, the bounds, the forms Cargo's
//! manifests use (this workspace's own root manifest among them), the writer's exact text, the
//! editor's exact text for the edits blaze-new makes, and two properties (write then read gives
//! the value back; an edit inserts only its own bytes and reads back as the value set) with a
//! planted-bug control, its plant a [`TomlPlant`] passed to [`Document::parse_in_planted`]
//! (D165). Every cap of [`Limits`] is drawn down to its smallest legal value and must refuse
//! exactly what passes it.

use core::fmt;
use std::{format, string::String as StdString, string::ToString, vec::Vec as StdVec};

use blaze_data::{Int, Map, Value, value};
use blaze_own::{Alloc, String};
use blaze_sim::{Broken, Config, NoPlant, Planted, Point, Tape};

use crate::{
    Datetime, Document, DocumentOptions, Edit, EditWhat, Error, Expected, Kind, Limits, MOST_KEY_PARTS, Origin,
    StringForm, TomlPlant, Unwritable, What, write_document, write_dotted_key, write_inline, write_string,
    write_string_in_form,
};

/// The test's allocator (D102): the process heap's handle, never `Global`.
fn heap() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn parse(text: &str) -> Document<'_, Alloc<'static>> {
    match Document::parse_in(text, Limits::CARGO, heap()) {
        Ok(doc) => doc,
        Err(error) => panic!("{text:?}: {}", error.display(text)),
    }
}

fn lower(text: &str) -> Value<Alloc<'static>> {
    parse(text).to_value_in(heap())
}

fn refused(text: &str) -> Error {
    refused_in(text, Limits::CARGO)
}

fn refused_in(text: &str, limits: Limits) -> Error {
    match Document::parse_in(text, limits, heap()) {
        Ok(doc) => panic!("{text:?} read as {:?}", doc.to_value_in(heap())),
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
    let expected = value!(in heap(); {
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
    assert_eq!(lower("a = 9223372036854775807\nb = -9223372036854775808"), value!(in heap(); {"a": i64::MAX, "b": i64::MIN}));
    assert_eq!(lower(""), value!(in heap(); {}));
    assert_eq!(lower("a = 1\r\nb = 2\r\n"), value!(in heap(); {"a": 1, "b": 2}), "CRLF");
    assert_eq!(lower("[a.b]\nc = 1\n[a]\nd = 2"), value!(in heap(); {"a": {"b": {"c": 1}, "d": 2}}), "implicit, then defined");
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
    assert_eq!(doc.to_value_in(heap()).get("lt"), Some(&value!(in heap(); "00:32:00.5")), "lowered as RFC 3339 text");
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
    assert!(Document::parse_in("a = [[1]]", limits, heap()).is_ok());
    assert_eq!(refused_in("a = [[[1]]]", limits), Error { at: 6, what: What::TooDeep { most: 2 } });
    assert_eq!(refused_in("a = [{ b = [1] }]", limits), Error { at: 11, what: What::TooDeep { most: 2 } });
    let limits = Limits { most_nodes: 2, ..Limits::CARGO };
    assert_eq!(refused_in("a = 1\nb = 2", limits), Error { at: 10, what: What::TooManyNodes { most: 2 } });
    let limits = Limits { most_bytes: 4, ..Limits::CARGO };
    assert_eq!(refused_in("a = 1", limits), Error { at: 0, what: What::TooLarge { len: 5, most: 4 } });
    let long = ["k"; MOST_KEY_PARTS + 1].join(".");
    assert_eq!(refused(&format!("{long} = 1")).what, What::KeyTooLong { most: MOST_KEY_PARTS });
    let most = ["k"; MOST_KEY_PARTS].join(".");
    assert!(Document::parse_in(&format!("{most} = 1"), Limits::CARGO, heap()).is_ok());
}

#[test]
fn a_key_twice_is_refused_in_every_table_form() {
    assert_eq!(refused("a = { b = 1, b = 2 }").what, What::Duplicate { first: 6 });
    assert_eq!(refused("a = { b.c = 1, b.c = 2 }").what, What::Duplicate { first: 8 });
    assert_eq!(refused("[[t]]\nx = 1\nx = 2").what, What::Duplicate { first: 6 });
    assert_eq!(refused("[t]\n\"x\" = 1\n'x' = 2").what, What::Duplicate { first: 4 });
    // One key per element of an array of tables: the same key in the next element is no twice.
    assert!(Document::parse_in("[[t]]\nx = 1\n[[t]]\nx = 2", Limits::CARGO, heap()).is_ok());
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
            let doc = Document::parse_in(&text, limits, heap()).unwrap();
            let value = doc.to_value_in(heap());
            let mut out = String::new_in(heap());
            write_inline(value.get("a").unwrap(), &mut out).unwrap();
            assert_eq!(out.as_str(), inner, "write(lower(read(text))) is the text");
            let mut document = String::new_in(heap());
            write_document(value.as_map().unwrap(), &mut document, DocumentOptions::CARGO).unwrap();
            let again = Document::parse_in(document.as_str(), limits, heap()).unwrap().to_map_in(heap());
            let mut out = String::new_in(heap());
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
    assert_eq!(v.get("package").and_then(|p| p.get("version")), Some(&value!(in heap(); {"workspace": true})));
    let targets = v.get("target").and_then(Value::as_map).unwrap();
    let keys: StdVec<&str> = targets.iter().map(|(k, _)| k).collect();
    assert_eq!(keys, ["cfg(unix)", "cfg(target_os = \"macos\")"]);
    assert_eq!(
        v.get("profile").and_then(|p| p.get("dev")).and_then(|d| d.get("package")).and_then(|p| p.get("*")),
        Some(&value!(in heap(); {"opt-level": 3}))
    );
    assert_eq!(v.get("bin"), Some(&value!(in heap(); [{"name": "a", "path": "src/a.rs"}])));
    let doc = parse(text);
    let edit = Edit::new_in(&doc, heap());
    assert_eq!(edit.finish().unwrap().as_str(), text, "no edits: byte for byte");
}

#[test]
fn this_workspace_manifest_reads_and_round_trips_untouched() {
    let text = include_str!("../../../Cargo.toml");
    let doc = parse(text);
    let members = doc.root().get("workspace").and_then(|w| w.get("members")).unwrap();
    assert!(members.is_array() && !members.is_empty());
    assert_eq!(Edit::new_in(&doc, heap()).finish().unwrap().as_str(), text);
}

fn edited(text: &str, edit: impl FnOnce(&Document<'_, Alloc<'static>>, &mut Edit<'_, '_, Alloc<'static>, Alloc<'static>>)) -> StdString {
    let doc = parse(text);
    let mut e = Edit::new_in(&doc, heap());
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
        e.insert(members, 1, &value!(in heap(); "crates/b")).unwrap();
        e.push(members, &value!(in heap(); "crates/d")).unwrap();
    });
    assert_eq!(
        out,
        "[workspace]\nmembers = [\n    \"crates/a\",\n    \"crates/b\",\n    \"crates/c\",\n    \"crates/d\",\n]\nresolver = \"2\"\n"
    );
    let push = |text: &str| {
        edited(text, |doc, e| {
            let members = doc.root().get("members").unwrap();
            e.push(members, &value!(in heap(); "z")).unwrap();
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
        e.set(package, "version", &value!(in heap(); "0.1.0")).unwrap();
        e.set(package, "name", &value!(in heap(); "y")).unwrap();
        let deps = root.get("dependencies").unwrap();
        e.set(deps.get("serde").unwrap(), "features", &value!(in heap(); ["derive"])).unwrap();
        e.set(deps.get("empty").unwrap(), "path", &value!(in heap(); "../e")).unwrap();
        e.set(deps, "libc", &value!(in heap(); "0.2")).unwrap();
        e.set(root, "cargo-features", &value!(in heap(); [])).unwrap();
    });
    assert_eq!(
        out,
        "cargo-features = []\n# top\n[package]\nname = \"y\" # the name\nversion = \"0.1.0\"\n\n# deps\n[dependencies]\n\
         serde = { version = \"1\", features = [\"derive\"] }\nempty = { path = \"../e\" }\nlibc = \"0.2\"\n"
    );
    assert_eq!(
        edited("[a]", |doc, e| e.set(doc.root().get("a").unwrap(), "k", &value!(in heap(); 1)).unwrap()),
        "[a]\nk = 1\n"
    );
    assert_eq!(
        edited("[[bin]]\nname = \"a\"\n[[bin]]\nname = \"b\"\n", |doc, e| {
            let second = doc.root().get("bin").unwrap().at(1).unwrap();
            e.set(second, "test", &value!(in heap(); false)).unwrap();
        }),
        "[[bin]]\nname = \"a\"\n[[bin]]\nname = \"b\"\ntest = false\n"
    );
}

#[test]
fn strings_and_keys_are_spelled_as_cargo_spells_them() {
    let spelled = |text: &str| {
        let mut out = String::new_in(heap());
        write_string(text, &mut out);
        let line = format!("k = {}", out.as_str());
        assert_eq!(lower(&line), value!(in heap(); {"k": text}), "{line:?}");
        out.as_str().to_string()
    };
    assert_eq!(spelled("plain"), "\"plain\"");
    assert_eq!(spelled("a\"b"), "'a\"b'");
    assert_eq!(spelled("C:\\x"), "'C:\\x'");
    assert_eq!(spelled("it's \"x\""), "\"\"\"it's \"x\"\"\"\"");
    assert_eq!(spelled("one\ntwo"), "\"\"\"\none\ntwo\"\"\"");
    assert_eq!(spelled("a\\b\nc"), "'''\na\\b\nc'''");
    assert_eq!(spelled("a\u{1}\t"), "\"a\\u0001\\t\"");
    assert_eq!(spelled("'''\"\"\"\n"), "\"\"\"\n'''\"\"\\\"\n\"\"\"");
    assert_eq!(spelled("\"\"\"'''\\"), "\"\\\"\\\"\\\"'''\\\\\"");
    assert_eq!(StringForm::of("a'b"), StringForm::Basic);
    let mut out = String::new_in(heap());
    assert_eq!(write_string_in_form("a'b", StringForm::Literal, &mut out), Err(StringForm::Literal));
    assert!(out.is_empty());
    write_string_in_form("a\nb", StringForm::MultiLineLiteral, &mut out).unwrap();
    assert_eq!(out.as_str(), "'''\na\nb'''");

    let key = |parts: &[&str]| {
        let mut out = String::new_in(heap());
        write_dotted_key(parts, &mut out);
        let line = format!("{} = 1", out.as_str());
        let mut v = lower(&line);
        for part in parts {
            v = v.as_map().unwrap().get(part).unwrap().clone();
        }
        assert_eq!(v, value!(in heap(); 1), "{line:?}");
        out.as_str().to_string()
    };
    assert_eq!(key(&["a-b_1"]), "a-b_1");
    assert_eq!(key(&["a b"]), "\"a b\"");
    assert_eq!(key(&["a\"b"]), "'a\"b'");
    assert_eq!(key(&["a\"'b"]), "\"a\\\"'b\"");
    assert_eq!(key(&["a\tb"]), "\"a\\tb\"");
    assert_eq!(key(&["target", "cfg(unix)", "x.y"]), "target.\"cfg(unix)\".\"x.y\"");
}

#[test]
fn remove_takes_its_own_bytes_and_keeps_every_other() {
    let removed = |text: &str, path: &[&str]| {
        edited(text, |doc, e| {
            let mut item = doc.root();
            for step in path {
                item = match step.parse::<usize>() {
                    Ok(index) => item.at(index).unwrap(),
                    Err(_) => item.get(step).unwrap(),
                };
            }
            e.remove(item).unwrap();
        })
    };
    let text = "# top\nname = \"x\" # the name\nedition = \"2024\"\n\n# deps\n[dependencies]\nserde = { version = \"1\", features = [\"derive\"] }\nlibc = \"0.2\"\n\n[dependencies.tokio]\nversion = \"1\"\n\n[dev-dependencies]\nx = 1\n";
    assert_eq!(
        removed(text, &["name"]),
        "# top\nedition = \"2024\"\n\n# deps\n[dependencies]\nserde = { version = \"1\", features = [\"derive\"] }\nlibc = \"0.2\"\n\n[dependencies.tokio]\nversion = \"1\"\n\n[dev-dependencies]\nx = 1\n"
    );
    assert_eq!(
        removed(text, &["dependencies"]),
        "# top\nname = \"x\" # the name\nedition = \"2024\"\n\n# deps\n\n\n[dev-dependencies]\nx = 1\n"
    );
    assert_eq!(
        removed(text, &["dependencies", "tokio"]),
        "# top\nname = \"x\" # the name\nedition = \"2024\"\n\n# deps\n[dependencies]\nserde = { version = \"1\", features = [\"derive\"] }\nlibc = \"0.2\"\n\n\n[dev-dependencies]\nx = 1\n"
    );
    let inline = "a = { x = 1, y = 2, z = 3 }\n";
    assert_eq!(removed(inline, &["a", "x"]), "a = { y = 2, z = 3 }\n");
    assert_eq!(removed(inline, &["a", "y"]), "a = { x = 1, z = 3 }\n");
    assert_eq!(removed(inline, &["a", "z"]), "a = { x = 1, y = 2 }\n");
    assert_eq!(removed("a = { x = 1 }\n", &["a", "x"]), "a = {}\n");
    assert_eq!(removed("a.b = 1\na.c = 2\nd = 3\n", &["a", "b"]), "a.c = 2\nd = 3\n");
    assert_eq!(removed("a.b = 1\nd = 3\na.c = 2\n", &["a"]), "d = 3\n");
    assert_eq!(removed("[a.b]\nx = 1\n[c]\n[a.d]\ny = 2\n", &["a"]), "[c]\n");
    let bins = "[[bin]]\nname = \"a\"\n[[bin]]\nname = \"b\"\n[lib]\n";
    assert_eq!(removed(bins, &["bin", "1"]), "[[bin]]\nname = \"a\"\n[lib]\n");
    assert_eq!(removed(bins, &["bin"]), "[lib]\n");

    let doc = parse("x = [1, 2]\ny = { a.b = 1 }\n");
    let mut e = Edit::new_in(&doc, heap());
    let what = |r: Result<(), crate::EditError<Alloc<'static>>>| r.unwrap_err().what;
    let root = doc.root();
    assert!(matches!(what(e.remove(root)), EditWhat::NotEditable { origin: Origin::Header }));
    assert!(matches!(what(e.remove(root.get("x").unwrap().at(0).unwrap())), EditWhat::NotEditable { origin: Origin::Value }));
    let a = root.get("y").unwrap().get("a").unwrap();
    assert!(matches!(what(e.remove(a)), EditWhat::NotEditable { origin: Origin::Dotted }));
    assert!(matches!(what(e.remove(a.get("b").unwrap())), EditWhat::NotEditable { origin: Origin::Dotted }));
}

#[test]
fn edits_with_no_one_place_are_refused() {
    let text = "a.b = 1\n[c.d]\n[[e]]\nf = [1]\n";
    let doc = parse(text);
    let root = doc.root();
    let mut e = Edit::new_in(&doc, heap());
    let what = |r: Result<(), crate::EditError<Alloc<'static>>>| r.unwrap_err().what;
    assert!(matches!(what(e.set(root.get("a").unwrap(), "x", &value!(in heap(); 1))), EditWhat::NotEditable { origin: Origin::Dotted }));
    assert!(matches!(what(e.set(root.get("c").unwrap(), "x", &value!(in heap(); 1))), EditWhat::NotEditable { origin: Origin::Implicit }));
    assert!(matches!(what(e.set(root, "a", &value!(in heap(); 1))), EditWhat::IsATable));
    assert!(matches!(what(e.push(root.get("e").unwrap(), &value!(in heap(); {}))), EditWhat::NotEditable { origin: Origin::Header }));
    let f = root.get("e").unwrap().at(0).unwrap().get("f").unwrap();
    assert!(matches!(what(e.insert(f, 2, &value!(in heap(); 2))), EditWhat::Index { index: 2, len: 1 }));
    assert!(matches!(what(e.set(f, "x", &value!(in heap(); 1))), EditWhat::NotATable { kind: Kind::Array }));
    assert!(matches!(what(e.push(f, &value!(in heap(); null))), EditWhat::Value(_)));
    let other = parse("x = 1");
    assert!(matches!(what(e.set(other.root(), "y", &value!(in heap(); 1))), EditWhat::OtherDocument));
    e.replace(f, &value!(in heap(); [2])).unwrap();
    e.replace(f, &value!(in heap(); [3])).unwrap();
    assert!(matches!(e.finish().unwrap_err().what, EditWhat::Overlap { .. }));
}

#[test]
fn the_writer_writes_cargo_layout() {
    let v = value!(in heap(); {
        "package": {"name": "x", "version": "0.1.0"},
        "dependencies": {"serde": {"version": "1", "features": ["derive"]}, "a b": "2"},
        "workspace": {"metadata": {"blaze": {"strict": true}}},
        "bin": [{"name": "a"}, {"name": "b"}]
    });
    let mut out = String::new_in(heap());
    write_document(v.as_map().unwrap(), &mut out, DocumentOptions::CARGO).unwrap();
    assert_eq!(
        out.as_str(),
        "[package]\nname = \"x\"\nversion = \"0.1.0\"\n\n[dependencies]\nserde = { version = \"1\", features = [\"derive\"] }\n\
         \"a b\" = \"2\"\n\n[workspace]\nmetadata = { blaze = { strict = true } }\n\n[[bin]]\nname = \"a\"\n\n[[bin]]\nname = \"b\"\n"
    );
    let deep = DocumentOptions { inline_from_depth: usize::MAX };
    let mut out = String::new_in(heap());
    write_document(value!(in heap(); {"a": {"b": {"c": 1}}, "t": "\u{1}\"\\\u{7f}"}).as_map().unwrap(), &mut out, deep).unwrap();
    assert_eq!(out.as_str(), "t = \"\\u0001\\\"\\\\\\u007F\"\n\n[a.b]\nc = 1\n");
    let mut out = String::new_in(heap());
    write_inline(&value!(in heap(); [1.0, -0.0, 1e300, f64::NAN, f64::NEG_INFINITY]), &mut out).unwrap();
    assert_eq!(out.as_str(), "[1.0, -0.0, 1e300, nan, -inf]");
}

#[test]
fn the_writer_refuses_by_path() {
    let mut out = String::new_in(heap());
    let error = write_document(value!(in heap(); {"a": {"b": [1, null]}}).as_map().unwrap(), &mut out, DocumentOptions::CARGO).unwrap_err();
    assert_eq!((error.path.as_str(), error.what), ("a.b[1]", Unwritable::Null));
    assert_eq!(error.to_string(), "a.b[1]: null has no TOML form");
    let mut out = String::new_in(heap());
    let error = write_inline(&value!(in heap(); u64::MAX), &mut out).unwrap_err();
    assert_eq!((error.path.as_str(), error.what), ("", Unwritable::Integer(u64::MAX)));
    let mut bytes = blaze_own::Vec::new_in(heap());
    bytes.push(1u8);
    let mut map = Map::new_in(heap());
    map.insert(String::from_str_in("x", heap()), Value::Bytes(bytes));
    let mut out = String::new_in(heap());
    assert_eq!(write_document(&map, &mut out, DocumentOptions::CARGO).unwrap_err().what, Unwritable::Bytes);
}

/// A value TOML can say, `depth` levels at most.
fn draw_value(tape: &Tape, depth: u32) -> Value<Alloc<'static>> {
    let kinds: u8 = if depth == 0 { 3 } else { 5 };
    match tape.int_in(0u8..=kinds, "kind") {
        0 => Value::Bool(tape.coin("bool")),
        1 => Value::Int(Int::from_i64(tape.int_in(i64::MIN..=i64::MAX, "i64"))),
        2 => Value::Float(tape.f64_finite("float")),
        3 => Value::Text(String::from_str_in(&tape.text_from("a\"\\'\n\u{1}é ", 0..=6, "text"), heap())),
        4 => {
            let mut list = blaze_own::Vec::new_in(heap());
            for item in tape.vec(0..=3, "list", |t| draw_value(t, depth - 1)) {
                list.push(item);
            }
            Value::List(list)
        }
        _ => draw_map(tape, depth - 1),
    }
}

fn draw_map(tape: &Tape, depth: u32) -> Value<Alloc<'static>> {
    let mut map = Map::new_in(heap());
    for (key, value) in tape.vec(0..=4, "map", |t| (t.text_from("ab-. \"é", 0..=3, "key"), draw_value(t, depth))) {
        map.insert(String::from_str_in(&key, heap()), value);
    }
    Value::Map(map)
}

fn harness<B: fmt::Display>(error: B) -> Broken<TripBroken> {
    Broken::Harness(error.to_string())
}

/// The round trip's and the edits' invariants: each names the texts, as owned strings.
#[derive(Clone, PartialEq, Eq, Debug)]
enum TripBroken {
    /// The text a value was written as did not read back.
    Unread { value: StdString, text: StdString, error: StdString },
    /// The text read back as another value.
    Differs { value: StdString, text: StdString, back: StdString },
    /// Removing a root key gave text that reads as another map than the one without it.
    RemoveDiffers { gone: StdString, text: StdString, out: StdString, got: StdString },
    /// Removing a root key gave text that does not read.
    RemoveUnread { gone: StdString, text: StdString, out: StdString, error: StdString },
    /// Setting a new root key changed bytes other than its own.
    SetOtherBytes { key: StdString, text: StdString, out: StdString },
    /// Setting a new root key gave text that reads as another map than the one with it.
    SetDiffers { key: StdString, out: StdString, got: StdString },
    /// Setting a new root key gave text that does not read.
    SetUnread { key: StdString, out: StdString, error: StdString },
}

impl fmt::Display for TripBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TripBroken::Unread { value, text, error } => write!(f, "{value} wrote {text:?}, which reads as {error}"),
            TripBroken::Differs { value, text, back } => {
                write!(f, "the round trip of {value} through {text:?} gave {back}")
            }
            TripBroken::RemoveDiffers { gone, text, out, got } => {
                write!(f, "removing {gone:?} from {text:?} gave {out:?}, read as {got}")
            }
            TripBroken::RemoveUnread { gone, text, out, error } => {
                write!(f, "removing {gone:?} from {text:?} gave {out:?}: {error}")
            }
            TripBroken::SetOtherBytes { key, text, out } => {
                write!(f, "setting {key:?} in {text:?} changed other bytes: {out:?}")
            }
            TripBroken::SetDiffers { key, out, got } => write!(f, "setting {key:?} gave {out:?}, read as {got}"),
            TripBroken::SetUnread { key, out, error } => write!(f, "setting {key:?} gave {out:?}: {error}"),
        }
    }
}

blaze_sim::plants! {
    /// The bugs the round trip's control plants.
    enum TripPlant breaks TripBroken {
        /// The reader as it is.
        None,
        /// A section's body end one byte short: a key set at the end of the root lands inside
        /// its last line (the old control's "setting" findings, now by variant).
        BodyEndShort => TripBroken::SetUnread { .. } | TripBroken::SetDiffers { .. } | TripBroken::SetOtherBytes { .. },
    }
}

impl TripPlant {
    fn planted(self) -> TomlPlant {
        match self {
            TripPlant::None => TomlPlant::None,
            TripPlant::BodyEndShort => TomlPlant::BodyEndShort,
        }
    }
}

/// read(write(v)) == v for documents; and setting a new key at the root inserts only its own
/// bytes and reads back as the value with that key. Every read is planted with `plant`.
fn round_trip(tape: &Tape, plant: impl Planted<TomlPlant>) -> Result<(), Broken<TripBroken>> {
    let v = draw_map(tape, 3);
    let Value::Map(map) = &v else { unreachable!("draw_map draws a map") };
    let inline_from_depth = tape.int_in(1usize..=4, "inline_from_depth");
    let mut text = String::new_in(heap());
    write_document(map, &mut text, DocumentOptions { inline_from_depth }).map_err(harness)?;
    let text = text.as_str();
    let doc = Document::parse_in_planted(text, Limits::CARGO, heap(), plant).map_err(|e| {
        Broken::Oracle(TripBroken::Unread {
            value: format!("{v:?}"),
            text: text.to_string(),
            error: e.display(text).to_string(),
        })
    })?;
    let back = doc.to_value_in(heap());
    if back != v {
        return Err(Broken::Oracle(TripBroken::Differs {
            value: format!("{v:?}"),
            text: text.to_string(),
            back: format!("{back:?}"),
        }));
    }
    // Removing a root key takes its bytes alone and reads back as the map without it.
    if !map.is_empty() {
        let (gone, _) = map.entry_at(tape.int_in(0usize..=map.len() - 1, "removed")).unwrap();
        let mut edit = Edit::new_in(&doc, heap());
        edit.remove(doc.root().get(gone).unwrap()).map_err(harness)?;
        let out = edit.finish().map_err(harness)?;
        let out = out.as_str();
        let mut expected = map.clone();
        expected.remove(gone);
        match Document::parse_in_planted(out, Limits::CARGO, heap(), plant) {
            Ok(doc) if doc.to_value_in(heap()) == Value::Map(expected) => {}
            Ok(doc) => {
                return Err(Broken::Oracle(TripBroken::RemoveDiffers {
                    gone: gone.to_string(),
                    text: text.to_string(),
                    out: out.to_string(),
                    got:format!("{:?}", doc.to_value_in(heap())),
                }));
            }
            Err(e) => {
                return Err(Broken::Oracle(TripBroken::RemoveUnread {
                    gone: gone.to_string(),
                    text: text.to_string(),
                    out: out.to_string(),
                    error:e.display(out).to_string(),
                }));
            }
        }
    }
    let key = format!("new{}", tape.text_from("xy", 0..=2, "new key"));
    if map.contains_key(&key) {
        return Ok(());
    }
    let new = draw_value(tape, 1);
    let mut edit = Edit::new_in(&doc, heap());
    edit.set(doc.root(), &key, &new).map_err(harness)?;
    let out = edit.finish().map_err(harness)?;
    let out = out.as_str();
    let prefix = text.bytes().zip(out.bytes()).take_while(|(a, b)| a == b).count();
    let suffix = text.bytes().rev().zip(out.bytes().rev()).take_while(|(a, b)| a == b).count();
    if prefix + suffix < text.len() {
        return Err(Broken::Oracle(TripBroken::SetOtherBytes {
            key: key.clone(),
            text: text.to_string(),
            out: out.to_string(),
        }));
    }
    let mut expected = map.clone();
    expected.insert(String::from_str_in(&key, heap()), new);
    match Document::parse_in_planted(out, Limits::CARGO, heap(), plant) {
        Ok(doc) if doc.to_value_in(heap()) == Value::Map(expected) => Ok(()),
        Ok(doc) => Err(Broken::Oracle(TripBroken::SetDiffers {
            key,
            out: out.to_string(),
            got: format!("{:?}", doc.to_value_in(heap())),
        })),
        Err(e) => {
            Err(Broken::Oracle(TripBroken::SetUnread { key, out: out.to_string(), error: e.display(out).to_string() }))
        }
    }
}

#[test]
fn the_round_trip_and_the_edit_hold() {
    blaze_sim::check(Config::cases(400), |tape| round_trip(tape, NoPlant));
}

#[test]
fn every_round_trip_plant_is_caught_by_its_invariant() {
    blaze_sim::controls(Config::cases(400), |tape, plant: TripPlant| round_trip(tape, plant.planted()));
}

// ---- the caps, drawn (D165) ----

impl blaze_sim::Limits for Limits {
    const SHIPPED: Limits = Limits::SHIPPED;

    fn draw(tape: &Tape) -> Limits {
        let shipped = Limits::SHIPPED;
        Limits {
            most_bytes: tape.limit(shipped.most_bytes, 0, "most bytes of text"),
            most_depth: tape.limit(shipped.most_depth, 0, "most arrays and inline tables open"),
            most_nodes: tape.limit(shipped.most_nodes, 1, "most nodes"),
            most_key_parts: tape.limit(shipped.most_key_parts, 1, "most parts of a dotted key"),
            most_float_len: tape.limit(shipped.most_float_len, 1, "most characters of a float"),
        }
    }
}

blaze_sim::points! {
    /// Where the caps' simulation has to get: each cap refusing, and a text read whole under a
    /// lowered cap.
    enum CapPoint {
        /// `most_bytes` refused the text.
        Bytes,
        /// `most_depth` refused an array or an inline table.
        Depth,
        /// `most_nodes` refused a node.
        Nodes,
        /// `most_key_parts` refused a dotted key.
        KeyParts,
        /// `most_float_len` refused a float.
        FloatLen,
        /// The text read whole under a lowered cap, as it reads under the shipped ones.
        ReadBack,
    }
}

/// What a drawn text holds, measured against each cap.
#[derive(Clone, Copy, Debug)]
struct Measures {
    bytes: usize,
    depth: usize,
    nodes: usize,
    key_parts: usize,
    float_len: usize,
}

/// A text of key/value lines: dotted keys of drawn parts, each value arrays and inline tables
/// nested around a scalar (an integer, a string or a float of drawn digits), with what it
/// holds measured as it is written; the nodes are the shipped read's count.
fn draw_text(tape: &Tape) -> (StdString, Measures) {
    let mut text = StdString::new();
    let mut m = Measures { bytes: 0, depth: 0, nodes: 0, key_parts: 0, float_len: 0 };
    let lines = tape.int_in(1usize..=4, "lines");
    for line in 0..lines {
        let parts = tape.int_in(1usize..=3, "key parts");
        m.key_parts = m.key_parts.max(parts);
        text.push_str(&format!("k{line}"));
        for part in 1..parts {
            text.push_str(&format!(".p{part}"));
        }
        text.push_str(" = ");
        let depth = tape.int_in(0usize..=3, "depth");
        m.depth = m.depth.max(depth);
        let opens: StdVec<bool> = (0..depth).map(|_| tape.coin("array")).collect();
        for &array in &opens {
            text.push_str(if array { "[" } else { "{ x = " });
        }
        match tape.below(3, "scalar") {
            0 => text.push('1'),
            1 => text.push_str("\"s\""),
            _ => {
                let float = format!(
                    "{}.{}e{}",
                    tape.text_from("123456789", 1..=3, "whole"),
                    tape.text_from("0123456789", 1..=3, "fraction"),
                    tape.text_from("0123456789", 1..=2, "exponent")
                );
                m.float_len = m.float_len.max(float.len());
                text.push_str(&float);
            }
        }
        for &array in opens.iter().rev() {
            text.push_str(if array { "]" } else { " }" });
        }
        text.push('\n');
    }
    m.bytes = text.len();
    (text, m)
}

/// The caps' invariants.
#[derive(Clone, PartialEq, Eq, Debug)]
enum CapBroken {
    /// The text did not read under the shipped caps.
    ShippedRefused { text: StdString, error: Error },
    /// A refusal no cap warrants: a cap's refusal of what does not pass it, or another error.
    Unwarranted { text: StdString, error: Error },
    /// Read whole, but as another value than under the shipped caps.
    Differs { text: StdString },
    /// Read whole though the measure `cap` passed its cap.
    Unenforced { text: StdString, cap: &'static str, value: usize, most: usize },
}

impl fmt::Display for CapBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CapBroken::ShippedRefused { text, error } => {
                write!(f, "{text:?} did not read under the shipped caps: {}", error.display(text))
            }
            CapBroken::Unwarranted { text, error } => {
                write!(f, "a refusal of {text:?} no cap warrants: {}", error.display(text))
            }
            CapBroken::Differs { text } => write!(f, "{text:?} read under lowered caps as another value"),
            CapBroken::Unenforced { text, cap, value, most } => {
                write!(f, "{text:?} read whole though its {cap} is {value}, past the cap of {most}")
            }
        }
    }
}

/// The point a refusal reaches when the cap it names was passed by `m`.
fn warranted(m: Measures, what: What) -> Option<CapPoint> {
    match what {
        What::TooLarge { len, most } if len == m.bytes && len > most => Some(CapPoint::Bytes),
        What::TooDeep { most } if m.depth > most => Some(CapPoint::Depth),
        What::TooManyNodes { most } if m.nodes > most => Some(CapPoint::Nodes),
        What::KeyTooLong { most } if m.key_parts > most => Some(CapPoint::KeyParts),
        What::NumberTooLong { most } if m.float_len > most => Some(CapPoint::FloatLen),
        _ => None,
    }
}

/// The first measure of `m` past its cap in `limits`, if any.
fn passed(m: Measures, limits: Limits) -> Option<(&'static str, usize, usize)> {
    [
        ("text length", m.bytes, limits.most_bytes),
        ("depth", m.depth, limits.most_depth),
        ("node count", m.nodes, limits.most_nodes),
        ("key parts", m.key_parts, limits.most_key_parts),
        ("float length", m.float_len, limits.most_float_len),
    ]
    .into_iter()
    .find(|&(_, value, most)| value > most)
}

/// One case: a text, one cap of the five drawn down (the rest shipped, so a case is at one
/// cap's boundary), and the text read: a refusal must be warranted by the cap it names, a read
/// must be whole, within every cap, and the value the shipped caps read.
fn within_caps(tape: &Tape) -> Result<(), Broken<CapBroken>> {
    let (text, mut m) = draw_text(tape);
    let shipped = match Document::parse_in(&text, Limits::SHIPPED, heap()) {
        Ok(doc) => doc,
        Err(error) => return Err(Broken::Oracle(CapBroken::ShippedRefused { text: text.clone(), error })),
    };
    m.nodes = shipped.nodes.len();
    let want = shipped.to_value_in(heap());
    let drawn: Limits = tape.limits();
    let mut limits = Limits::SHIPPED;
    match tape.below(5, "lowered cap") {
        0 => limits.most_bytes = drawn.most_bytes,
        1 => limits.most_depth = drawn.most_depth,
        2 => limits.most_nodes = drawn.most_nodes,
        3 => limits.most_key_parts = drawn.most_key_parts,
        _ => limits.most_float_len = drawn.most_float_len,
    }
    match Document::parse_in(&text, limits, heap()) {
        Err(error) => match warranted(m, error.what) {
            Some(point) => {
                tape.reach(point);
                Ok(())
            }
            None => Err(Broken::Oracle(CapBroken::Unwarranted { text: text.clone(), error })),
        },
        Ok(doc) => {
            if let Some((cap, value, most)) = passed(m, limits) {
                return Err(Broken::Oracle(CapBroken::Unenforced { text: text.clone(), cap, value, most }));
            }
            if doc.to_value_in(heap()) != want {
                return Err(Broken::Oracle(CapBroken::Differs { text: text.clone() }));
            }
            if limits != Limits::SHIPPED {
                tape.reach(CapPoint::ReadBack);
            }
            Ok(())
        }
    }
}

#[test]
fn every_cap_refuses_exactly_what_passes_it() {
    blaze_sim::check(Config::cases(512).must_reach_points(<CapPoint as Point>::IDS), within_caps);
}

/// The text the errors and datetimes said through `write!` before they were `Show`: the
/// oracle the conversion is held to, byte for byte.
fn said_before(what: What) -> StdString {
    match what {
        What::TooLarge { len, most } => format!("the text is {len} bytes, past the limit of {most}"),
        What::TooDeep { most } => format!("more than {most} arrays and inline tables are open"),
        What::Unexpected { found, expected } => format!("{found:?} where {} was wanted", expected_text(expected)),
        What::Control { ch } => format!("control character {ch:?} is not allowed here"),
        What::BadEscape { found } => format!("`\\{found}` is no escape"),
        What::NotAScalar { value } => format!("U+{value:04X} is not a Unicode scalar value"),
        What::Closed { first } => format!("the inline table or array at byte {first} is closed once written"),
        other => panic!("no oracle for {other:?}"),
    }
}

fn expected_text(expected: Expected) -> &'static str {
    match expected {
        Expected::Key => "a key",
        Expected::Quote => "the closing quote",
        _ => "`,` or `]`",
    }
}

/// The `Show` conversion's invariant.
#[derive(Clone, PartialEq, Eq, Debug)]
enum ShowBroken {
    /// The `Show` form says `got` where `write!` said `want`.
    Differs { got: StdString, want: StdString },
}

impl fmt::Display for ShowBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ShowBroken::Differs { got, want } => write!(f, "the Show form says {got:?}; write! said {want:?}"),
        }
    }
}

fn shows_as_before(tape: &Tape) -> Result<(), Broken<ShowBroken>> {
    let chars = ['a', '\'', '"', '\n', '\u{1}', '\u{7f}', 'é'];
    let pick = |tape: &Tape| *tape.one_of(&chars, "char");
    let expected = *tape.one_of(&[Expected::Key, Expected::Quote, Expected::CommaOrEndArray], "expected");
    let n = usize::try_from(tape.next_u64("n")).unwrap_or(0);
    let what = match tape.below(7, "what") {
        0 => What::TooLarge { len: n, most: n / 3 },
        1 => What::TooDeep { most: n },
        2 => What::Unexpected { found: pick(tape), expected },
        3 => What::Control { ch: pick(tape) },
        4 => What::BadEscape { found: pick(tape) },
        5 => What::NotAScalar { value: tape.next_u32("scalar") >> tape.below(32, "shift") },
        _ => What::Closed { first: n },
    };
    let error = Error { at: n, what };
    let datetime = Datetime {
        date: tape.coin("date").then(|| crate::doc::Date { year: 1979, month: 5, day: 7 }),
        time: tape.coin("time").then(|| crate::doc::Time { hour: 7, minute: 3, second: 0, nanosecond: tape.next_u32("ns") % 1_000_000_000 }),
        offset: match tape.below(3, "offset") {
            0 => None,
            1 => Some(crate::doc::Offset::Z),
            _ => Some(crate::doc::Offset::Minutes(i16::try_from(tape.below(1440, "minutes")).unwrap_or(0) - 720)),
        },
    };
    let datetime_before = {
        let mut s = StdString::new();
        if let Some(d) = datetime.date {
            s.push_str(&format!("{:04}-{:02}-{:02}", d.year, d.month, d.day));
            if datetime.time.is_some() {
                s.push('T');
            }
        }
        if let Some(t) = datetime.time {
            s.push_str(&format!("{:02}:{:02}:{:02}", t.hour, t.minute, t.second));
            if t.nanosecond > 0 {
                let fraction = format!("{:09}", t.nanosecond);
                s.push('.');
                s.push_str(fraction.trim_end_matches('0'));
            }
        }
        match datetime.offset {
            None => {}
            Some(crate::doc::Offset::Z) => s.push('Z'),
            Some(crate::doc::Offset::Minutes(m)) => {
                let sign = if m < 0 { '-' } else { '+' };
                let m = m.unsigned_abs();
                s.push_str(&format!("{sign}{:02}:{:02}", m / 60, m % 60));
            }
        }
        s
    };
    for (got, want) in [
        (what.to_string(), said_before(what)),
        (error.to_string(), format!("{} at byte {n}", said_before(what))),
        (datetime.to_string(), datetime_before),
    ] {
        if got != want {
            return Err(Broken::Oracle(ShowBroken::Differs { got, want }));
        }
    }
    Ok(())
}

#[test]
fn errors_and_datetimes_show_as_write_said() {
    blaze_sim::check(Config::cases(500), shows_as_before);
}

/// Every allocation and growth a manifest's parse makes (its node columns, its string arena,
/// its datetimes), refused one at a time, comes back as the parse's own `What::Memory` error
/// with nothing left live: never a panic. tempo's `blaze check` (2026-11-17) died in this parse
/// when the allocator it was handed was full; the caller's job must fail, by the manifest's
/// name, and the process go on (rule 20). Prints nothing when it passes; a wrong parse prints
/// the run that broke it, e.g. "with allocation or growth #3 refused the run still succeeded:
/// the refusal was swallowed", or the panic "a refused allocation read as another error: ...".
#[test]
fn every_refused_allocation_is_the_documents_memory_error() {
    let text = "[package]\nname = \"big\"\nversion = \"0.1.0\"\nauthors = [\"a\\tb\", 'c', '''d''']\nwhen = 1979-05-27T07:32:00Z\n\n[dependencies]\nserde = { version = \"1\", features = [\"derive\"] }\n\n[[bin]]\nname = \"x\\u00e9\"\n";
    let events = blaze_own::testing::every_failure(&heap(), |alloc| match Document::parse_in(text, Limits::CARGO, alloc) {
        Ok(doc) => Ok(doc.nodes.len()),
        Err(Error { what: What::Memory(_), .. }) => Err("the document's memory error"),
        Err(other) => panic!("a refused allocation read as another error: {}", other.display(text)),
    });
    match events {
        Ok(events) => assert!(events >= 3, "the parse asked for {events} allocations: its columns, arena and datetimes are three at least"),
        Err(broke) => panic!("{broke}"),
    }
}
