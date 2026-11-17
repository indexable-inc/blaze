//! The plain views' goldens (D70): the exact bytes of the agent and JSON views of a syntax
//! error, an unresolved import, and the type checker's unknown name with its fix, and of their
//! closing lines.

use blaze_own::{AString};

use super::plain::{Plain, PlainClosing, View, agent, agent_closing, json, json_closing, view};
use super::{Counts, Coverage, Edit};
use crate::Severity;

const OWNERS: &str = "fn add(a: u32, b: u32) -> u32 { a + c }\n";
const SYNTAX: &str = "fn ok() {}\n\nfn f() { let x = ; }\n";
const IMPORT: &str = "mod a;\nuse crate::nope;\n";

fn at(text: &str, needle: &str) -> u32 {
    text.find(needle).expect("the needle is in the text") as u32
}

fn owners() -> Plain<'static> {
    let c = at(OWNERS, "c }");
    let b = at(OWNERS, "b:");
    Plain {
        id: "e7k2q",
        severity: Severity::Error,
        code: "E0425",
        path: "src/main.rs",
        text: Some(OWNERS.as_bytes()),
        start: c,
        end: c + 1,
        message: "cannot find value `c` in this scope",
        fix: Some((Edit { start: c, end: c + 1, insert: &OWNERS[b as usize..b as usize + 1] }, "`b` is in scope, is a `u32`, and is never used")),
    }
}

fn syntax() -> Plain<'static> {
    let semi = at(SYNTAX, "; }");
    Plain {
        id: "4hx9m",
        severity: Severity::Error,
        code: "syntax",
        path: "src/lib.rs",
        text: Some(SYNTAX.as_bytes()),
        start: semi,
        end: semi + 1,
        message: "expected expression",
        fix: None,
    }
}

fn import() -> Plain<'static> {
    let nope = at(IMPORT, "nope");
    Plain {
        id: "s3ta8",
        severity: Severity::Error,
        code: "unresolved",
        path: "src/lib.rs",
        text: Some(IMPORT.as_bytes()),
        start: nope,
        end: nope + 4,
        message: "no `nope` in `crate`",
        fix: None,
    }
}

fn agent_of(plain: &Plain<'_>) -> String {
    let mut out = AString::new_in(blaze_own::testing::heap().alloc());
    agent(&mut out, plain).expect("the fix is in its text");
    out.as_str().to_string()
}

fn json_of(plain: &Plain<'_>) -> String {
    let mut out = AString::new_in(blaze_own::testing::heap().alloc());
    json(&mut out, plain).expect("the fix is in its text");
    out.as_str().to_string()
}

fn closing_of(errors: u32, fixes: &[&str], coverage: Option<Coverage<'_>>, as_json: bool) -> String {
    let mut out = AString::new_in(blaze_own::testing::heap().alloc());
    let closing = PlainClosing { counts: Counts { errors, ..Counts::default() }, fixes, coverage, command: "blaze check" };
    match as_json {
        false => agent_closing(&mut out, &closing),
        true => json_closing(&mut out, &closing),
    }
    out.as_str().to_string()
}

/// D70's text, byte for byte.
#[test]
fn the_agent_view_of_the_owners_case_is_d70s_text() {
    let got = agent_of(&owners()) + &closing_of(1, &["e7k2q"], None, false);
    assert_eq!(
        got,
        "e7k2q  error[E0425]  src/main.rs:1:37  cannot find value `c` in this scope\n       fix: replace `c` with `b` (1:37-1:38): `b` is in scope, is a `u32`, and is never used\n1 error, 1 fix: blaze check --fix e7k2q\n"
    );
}

#[test]
fn the_agent_view_of_a_finding_without_a_fix_is_one_line() {
    assert_eq!(agent_of(&syntax()), "4hx9m  error[syntax]  src/lib.rs:3:18  expected expression\n");
    assert_eq!(agent_of(&import()), "s3ta8  error[unresolved]  src/lib.rs:2:12  no `nope` in `crate`\n");
}

/// What a check did not run is said in the agent view too, on its own line: an agent never
/// reads a pass that is not one.
#[test]
fn the_agent_closing_names_what_was_not_checked() {
    let coverage = Coverage { ran: &["manifests", "names"], not: &["types", "borrows"] };
    assert_eq!(closing_of(0, &[], Some(coverage), false), "incomplete: NOT checked: types, borrows\n0 errors: blaze check\n");
    assert_eq!(closing_of(2, &["s3ta8"], Some(coverage), false), "incomplete: NOT checked: types, borrows\n2 errors, 1 fix: blaze check --fix s3ta8\n");
    assert_eq!(closing_of(3, &["s3ta8", "e7k2q"], None, false), "3 errors, 2 fixes: blaze check --fix all\n");
}

#[test]
fn the_json_view_holds_the_same_fields_in_the_stated_order() {
    assert_eq!(
        json_of(&owners()),
        "{\"id\":\"e7k2q\",\"severity\":\"error\",\"code\":\"E0425\",\"path\":\"src/main.rs\",\"line\":1,\"col\":37,\"end_line\":1,\"end_col\":38,\"message\":\"cannot find value `c` in this scope\",\"fix\":{\"start_line\":1,\"start_col\":37,\"end_line\":1,\"end_col\":38,\"replace\":\"c\",\"with\":\"b\",\"reason\":\"`b` is in scope, is a `u32`, and is never used\"}}\n"
    );
    assert_eq!(
        json_of(&syntax()),
        "{\"id\":\"4hx9m\",\"severity\":\"error\",\"code\":\"syntax\",\"path\":\"src/lib.rs\",\"line\":3,\"col\":18,\"end_line\":3,\"end_col\":19,\"message\":\"expected expression\",\"fix\":null}\n"
    );
    assert_eq!(
        json_of(&import()),
        "{\"id\":\"s3ta8\",\"severity\":\"error\",\"code\":\"unresolved\",\"path\":\"src/lib.rs\",\"line\":2,\"col\":12,\"end_line\":2,\"end_col\":16,\"message\":\"no `nope` in `crate`\",\"fix\":null}\n"
    );
    let coverage = Coverage { ran: &["names"], not: &["types", "borrows"] };
    assert_eq!(
        closing_of(1, &["e7k2q"], Some(coverage), true),
        "{\"errors\":1,\"warnings\":0,\"fixes\":1,\"command\":\"blaze check --fix e7k2q\",\"not_checked\":[\"types\",\"borrows\"]}\n"
    );
}

/// A line's whole text deleted is said without its newline; a quote in a message is escaped.
#[test]
fn a_deleted_line_and_an_escaped_quote() {
    let text = "[dependencies]\nbstd = { path = \"x\" }\n";
    let start = at(text, "bstd");
    let plain = Plain {
        id: "m2c4v",
        severity: Severity::Error,
        code: "toolchain-crate-named",
        path: "Cargo.toml",
        text: Some(text.as_bytes()),
        start,
        end: text.len() as u32,
        message: "names \"bstd\"",
        fix: Some((Edit { start, end: text.len() as u32, insert: "" }, "the crate is `std` with no line")),
    };
    assert_eq!(
        agent_of(&plain),
        "m2c4v  error[toolchain-crate-named]  Cargo.toml:2:1  names \"bstd\"\n       fix: delete `bstd = { path = \"x\" }` (2:1-3:1): the crate is `std` with no line\n"
    );
    assert!(json_of(&plain).contains("\"message\":\"names \\\"bstd\\\"\""), "{}", json_of(&plain));
    assert!(json_of(&plain).contains("\"replace\":\"bstd = { path = \\\"x\\\" }\\n\""), "{}", json_of(&plain));
}

/// One test of the output, one home: a terminal gets the drawing, anything else the agent view,
/// unless a view is asked.
#[test]
fn the_default_view_follows_the_terminal() {
    assert_eq!(view(None, true), View::Person);
    assert_eq!(view(None, false), View::Agent);
    assert_eq!(view(Some(View::Json), true), View::Json);
    assert_eq!(View::parse("agent"), Some(View::Agent));
    assert_eq!(View::parse("eyes"), None);
}
