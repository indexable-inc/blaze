use blaze_ids::ContentId;

use super::{BASES, Base, Bases, Found, HELD, Key, diff, found, normalized_into, places};
use crate::{Diagnostic, Level, Span};

fn span(file: &str, line: u32, column: u32) -> Span {
    Span {
        file: file.to_owned(),
        line,
        label: None,
        column,
        primary: true,
        byte_end: 0,
        end_line: line,
        byte_start: 0,
        end_column: column + 1,
        replacement: None,
        applicability: None,
    }
}

fn diagnostic(level: Level, message: &str, at: Span) -> Diagnostic {
    Diagnostic {
        code: Some("E0308".to_owned()),
        unit: None,
        facts: Vec::new(),
        level,
        spans: vec![at],
        message: message.to_owned(),
        children: Vec::new(),
        rendered: message.to_owned(),
        explanation: None,
    }
}

fn normalized(text: &str) -> String {
    let mut out = String::new();
    normalized_into(text, &mut out);
    out
}

fn key(client: &str) -> Key {
    Key { checkout: "/w".to_owned(), scope: ContentId([1; 32]), client: client.to_owned() }
}

fn one(message: &str, line: u32, text: &str) -> Found {
    found(diagnostic(Level::Warning, message, span("src/lib.rs", line, 4)), "src/lib.rs", Some(text))
}

/// The references the Python core made `#` are made `#`, and nothing else is: a number that is
/// no line's, `:1` alone, `line` inside a word, `lines` with no number.
#[test]
fn line_references_are_normalized_as_the_python_core_did() {
    assert_eq!(normalized("first defined at src/a.rs:12:5 here"), "first defined at src/a.rs# here");
    assert_eq!(normalized("see line 12"), "see #");
    assert_eq!(normalized("lines 3-5 and lines 3 to 5."), "# and #.");
    assert_eq!(normalized("lines 3 -x"), "# -x");
    assert_eq!(normalized("expected 2 arguments, found 3"), "expected 2 arguments, found 3");
    assert_eq!(normalized("a:1 b:1:2x outline 4 lines"), "a:1 b:1:2x outline 4 lines");
    assert_eq!(normalized("水 line 7"), "水 #");
}

/// The invariant: an edit that only moves a diagnostic (its line, and line numbers in its
/// message) keeps its id; a change of its message, its line's text or its level does not.
/// Control: the line's text changed gives another id.
#[test]
fn an_id_is_kept_across_moved_lines_and_changed_by_what_it_says() {
    let at_3 = one("unused variable `x`, see line 3", 3, "    let x = 1;");
    let at_9 = one("unused variable `x`, see line 9", 9, "  let x = 1;  ");
    assert_eq!(at_3.id, at_9.id);
    assert_ne!(one("unused variable `y`", 3, "    let x = 1;").id, at_3.id);
    assert_ne!(one("unused variable `x`, see line 3", 3, "    let x = 2;").id, at_3.id);
    let error = found(
        diagnostic(Level::Error, "unused variable `x`, see line 3", span("src/lib.rs", 3, 4)),
        "src/lib.rs",
        Some("    let x = 1;"),
    );
    assert_ne!(error.id, at_3.id);
}

/// The invariant: the diff counts ids as multisets: the same warning twice is two, one fixed
/// of two is one fixed and one unchanged.
#[test]
fn the_diff_counts_each_id_as_many_times_as_it_is_said() {
    let a = one("a", 1, "a();");
    let b = one("b", 2, "b();");
    let base = vec![a.clone(), a.clone(), b.clone()];
    let (new, unchanged, fixed) = diff(&base, vec![a.clone(), one("c", 3, "c();")]);
    assert_eq!(new.iter().map(|it| it.message.as_str()).collect::<Vec<_>>(), ["c"]);
    assert_eq!(unchanged, [a.clone()]);
    assert_eq!(fixed, [a, b]);
}

/// The invariant: a token names one base of one key; a token of another client, or one past
/// the bound, is unknown and everything is new. Control: the held token diffs.
#[test]
fn a_token_names_one_base_and_past_the_bound_is_unknown() {
    let mut bases = Bases::default();
    let first = bases.ended(key("a"), None, vec![one("a", 1, "a();")], ContentId([2; 32]), None);
    assert_eq!((first.base, first.new.len()), (Base::First, 1));
    let token = first.token;
    let held = bases.ended(key("a"), token, vec![one("a", 1, "a();")], ContentId([2; 32]), None);
    assert_eq!((held.base, held.new.len(), held.unchanged.len()), (Base::Held, 0, 1));
    assert_eq!(held.token, token, "the same diagnostics are the same base");
    let other = bases.ended(key("b"), token, vec![one("a", 1, "a();")], ContentId([2; 32]), None);
    assert_eq!((other.base, other.new.len()), (Base::Unknown, 1));
    for client in 0..BASES {
        bases.ended(key(&format!("c{client}")), None, Vec::new(), ContentId([2; 32]), None);
    }
    assert_eq!(bases.held().0, BASES);
    assert_eq!(bases.base(&key("a"), token), Base::Unknown, "the oldest dropped first");
}

/// The invariant: the bases hold at most HELD diagnostics but the one just kept; the oldest
/// go first.
#[test]
fn the_bases_hold_a_bounded_number_of_diagnostics() {
    let mut bases = Bases::default();
    let many = |n: u32| (0..n).map(|it| one(&format!("w{it}"), it + 1, "x();")).collect();
    bases.ended(key("a"), None, many(10), ContentId([2; 32]), None);
    bases.ended(key("b"), None, many(HELD as u32 - 5), ContentId([2; 32]), None);
    assert_eq!(bases.held(), (1, HELD - 5), "a, the oldest, is dropped for b");
    bases.ended(key("c"), None, many(HELD as u32 + 1), ContentId([2; 32]), None);
    assert_eq!(bases.held(), (1, HELD + 1), "the one just kept is kept whatever its size");
}

/// A span's file is found under the head when absolute, else from the unit's manifest
/// directory up; one outside the checkout has no place in it.
#[test]
fn a_spans_file_is_looked_for_where_rustc_ran() {
    assert_eq!(places("/w/src/lib.rs", "/w", None), ["src/lib.rs"]);
    assert_eq!(places("/home/x/.cargo/registry/a.rs", "/w", None), Vec::<String>::new());
    assert_eq!(places("src/lib.rs", "/w", Some("crates/a")), [
        "crates/a/src/lib.rs",
        "crates/src/lib.rs",
        "src/lib.rs"
    ]);
    assert_eq!(places("src/lib.rs", "/w", None), ["src/lib.rs"]);
}
