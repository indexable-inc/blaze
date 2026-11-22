//! PRINTING COSTS WHAT MOVED (job IL2, part one): a file's token places are made bytes from ONE
//! lex of the file a request (print.rs `list_observing`, parser `file_tokens`), never one lex of
//! the item's text a place (`TokenPlace::to_bytes`, 144 of 951 samples of tempo's edit run).
//!
//! Each test states what is right by the rule, not by what the code prints: a token place's
//! bytes are what `TokenPlace::to_bytes` makes of the item's text today (the definition, rule
//! 71); a finding prints at the line and column its bytes are on in the text as it is now.
//!
//! Expected lines: `lines: placed P, lexed L for N token place(s)` and `lines: the error moved
//! from <line> to <line>`.

use super::*;
use blaze_report::frame::Caps;
use blaze_report::frame::plain::View;
use bstd::fs::{Content, Durable};
use bstd::io::{Io, Limits, SimWorld};
use bstd::mem::TASK_FRAME;
use bstd::path::Path;
use bstd::test::BUDGET;
use parser::token_key::{FileTokens, TokenPlace};

const EDITION: blaze_surface::Edition = crate::fix::ITEM_GRAMMAR;
const PARSER_EDITION: parser::Edition = parser::Edition::Edition2024;

/// Two type errors in two items of one file.
const A_RS: &str = "fn first() {\n    let x: u32 = true;\n}\n\n// the second, led by a plain comment\nfn second() {\n    let y: u8 = \"no\";\n}\n";
/// `A_RS` with a comment line inside `first`, above its error: no token moved.
const A_RS_GROWN: &str = "fn first() {\n    // the error is below\n    let x: u32 = true;\n}\n\n// the second, led by a plain comment\nfn second() {\n    let y: u8 = \"no\";\n}\n";
/// One type error.
const B_RS: &str = "/// The one.\npub fn only() -> u8 {\n    'c'\n}\n";

/// A finding's token place: the bytes of `needle` in `text`, made a place of its item's text
/// as a body check makes it (engine_bodies.rs `FoundAt::Tokens`), and the item's chain.
fn place_of(text: &str, needle: &str, frame: &Frame) -> (TokenPlace, u64) {
    let at = text.find(needle).expect("the needle is in the text");
    let (item, _, chain) = crate::fix::item_of(text, at as u32, Some(EDITION), frame);
    let item_text = &text[item as usize..];
    let from = at - item as usize;
    let place = TokenPlace::from_bytes(item_text, PARSER_EDITION, from..from + needle.len()).expect("the needle is whole tokens");
    (place, chain)
}

/// Where `needle` is in `text`, as a plain view prints it: `path:line:column`, both from 1.
fn line_col(path: &str, text: &str, needle: &str) -> std::string::String {
    let at = text.find(needle).expect("the needle is in the text");
    let line = text[..at].matches('\n').count() + 1;
    let column = at - text[..at].rfind('\n').map_or(0, |newline| newline + 1) + 1;
    format!("{path}:{line}:{column}")
}

/// What one request prints of `findings` (path, the text its place was made in, needle) over
/// the files `now` (path, text as it is today), in the agent's view: its stdout and its tally.
fn printed(now: &[(&str, &str)], findings: &[(&str, &str, &str)]) -> (std::vec::Vec<u8>, Tally) {
    let io = Io::simulated(SimWorld::new(23, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).expect("request context");
    let root = io.need_fs().expect("filesystem").root();
    let out = io.need_out().expect("stdout");
    let frame = cx.frame();
    let alloc = blaze_own::testing::heap().shared();
    for (path, text) in now {
        root.publish(&cx, &Path::new(path).unwrap(), Content::bytes(text.as_bytes()), Durable::No).unwrap();
    }
    let mut lines = Lines::new_in(alloc);
    for (n, (path, made_in, needle)) in findings.iter().enumerate() {
        let (at, chain) = place_of(made_in, needle, frame);
        let name = Name::new(Kind::BodyCheck, ContentId([n as u8 + 1; 32]));
        lines.placed_tokens(name, crate::work::SYNTAX, at, path.as_bytes(), chain, EDITION, b"expected an expression");
    }
    let mut shown = print::Shown::new(Caps::LINES, View::Agent, "/ws", frame);
    let tally = print_all(&cx, &root, lines.blocks(), &mut shown, alloc, frame, &out).unwrap();
    (io.script(|host| host.out.clone()).expect("simulated stdout"), tally)
}

/// ONE LEX A FILE: three token places in two files are made bytes from two lexes, one a file,
/// and each prints at the line and column its bytes are on (the oracle, from the text alone).
/// CONTROL (rule 23): with the table made again for each place (`with_a_lex_a_place`, the old
/// cost), the count is one a place, three, and the bytes printed are the same.
#[test]
fn a_file_is_lexed_once_however_many_places_it_holds() {
    let files = [("a.rs", A_RS), ("b.rs", B_RS)];
    let findings = [("a.rs", A_RS, "true"), ("a.rs", A_RS, "\"no\""), ("b.rs", B_RS, "'c'")];
    let (bytes, tally) = printed(&files, &findings);
    let said = core::str::from_utf8(&bytes).unwrap();
    for (path, text, needle) in findings {
        let at = line_col(path, text, needle);
        assert!(said.contains(&at), "IL2: the place of {needle:?} prints at {at}: {said}");
    }
    let placing = tally.tail.placing;
    assert_eq!(placing.placed, 2, "IL2: two files placed: {placing:?}");
    assert_eq!(placing.lexed, 2, "IL2: one lex a file, not one a place: {placing:?}");
    assert!(placing.lexed <= placing.placed, "IL2: L <= R: {placing:?}");
    println!("lines: placed {}, lexed {} for {} token place(s)", placing.placed, placing.lexed, findings.len());
    let (again, control) = print::with_a_lex_a_place(|| printed(&files, &findings));
    assert_eq!(control.tail.placing.lexed, 3, "IL2 CONTROL: a lex a place counts one a place: {:?}", control.tail.placing);
    assert_eq!(again, bytes, "IL2 CONTROL: the table and a lex a place print the same bytes");
    let profile = tally.tail.line(tally.listed, &Frame::with_reserve(1 << 16, bstd::mem::Tag::Other).unwrap()).as_str().to_owned();
    assert!(profile.contains("lines: files rendered 2 (drawn 2), replayed 0, lexed 2"), "IL2: the profile's counters: {profile}");
}

/// A STORED PLACE PRINTS WHERE ITS TOKENS ARE TODAY: the place made in `A_RS`, printed over
/// `A_RS_GROWN` (a comment line above the error inside `first`), prints one line down, and the
/// bytes printed equal those of a request whose places were made in today's text (the oracle: a
/// fresh request). CONTROL: the line printed moved; the old line is not printed.
#[test]
fn a_stored_place_prints_at_its_line_in_todays_text() {
    let stored = [("a.rs", A_RS, "true"), ("b.rs", B_RS, "'c'")];
    let fresh = [("a.rs", A_RS_GROWN, "true"), ("b.rs", B_RS, "'c'")];
    let now = [("a.rs", A_RS_GROWN), ("b.rs", B_RS)];
    let (edited, tally) = printed(&now, &stored);
    let (oracle, _) = printed(&now, &fresh);
    assert_eq!(edited, oracle, "IL2: the stored places print as a fresh request's");
    let said = core::str::from_utf8(&edited).unwrap();
    let (before, after) = (line_col("a.rs", A_RS, "true"), line_col("a.rs", A_RS_GROWN, "true"));
    assert_ne!(before, after, "IL2 CONTROL: the comment moved the error's line");
    assert!(said.contains(&after), "IL2: the error prints at its line today, {after}: {said}");
    assert!(!said.contains(&before), "IL2 CONTROL: the old line {before} is not printed: {said}");
    assert!(tally.tail.placing.lexed <= tally.tail.placing.placed, "IL2: L <= R: {:?}", tally.tail.placing);
    println!("lines: the error moved from {before} to {after}");
}

/// THE TABLE IS THE DEFINITION: for every item of a file (comment-led, doc-led, attributed,
/// nested, members), every token place of it (one to four tokens, and inside a token) is the
/// same range, or the same refusal, from the file's one table (`TokenPlace::to_bytes_in`) as
/// from a lex of the item's text (`TokenPlace::to_bytes`). CONTROL: a table of another text
/// (one byte added at the file's start) is caught by the same comparison.
#[test]
fn every_place_from_the_table_is_the_items_own_lex() {
    const TEXT: &str = "//! inner doc\n#![allow(dead_code)]\n\n// a plain comment led item\n/// doc\n#[inline]\npub fn one(x: u32) -> u32 { /* block */ x + 1 }\n\nstruct S<'a> { a: u8, b: &'a str }\n\nimpl S<'_> {\n    // a member's comment\n    fn m(&self) -> u8 { self.a >> 1 }\n}\n\nmod inner {\n    pub const C: &str = r#\"raw \"str\"\"#;\n    /* before */ pub fn deep() -> i8 { -1 }\n}\n";
    let frame = Frame::with_reserve(1 << 24, bstd::mem::Tag::Other).unwrap();
    let erased = blaze_own::Erased::new(&frame);
    let outline = blaze_file_items::outline(TEXT, EDITION, erased.handle());
    let table = |text: &str| {
        let (mut keys, mut comments) = (std::vec::Vec::new(), std::vec::Vec::new());
        parser::token_key::file_tokens(text, PARSER_EDITION, |start, len| keys.push((start, len)), |start, len| comments.push((start, len))).unwrap();
        (keys, comments)
    };
    let (keys, comments) = table(TEXT);
    let tokens = FileTokens { keys: &keys, comments: &comments };
    let (shifted_keys, shifted_comments) = table(&format!(" {TEXT}"));
    let shifted = FileTokens { keys: &shifted_keys, comments: &shifted_comments };
    let (mut compared, mut caught) = (0usize, 0usize);
    assert!(outline.items().len() >= 6, "the text's items: {}", outline.items().len());
    for item in outline.items() {
        let start = item.range.start;
        let text = &TEXT[start as usize..];
        let count = match (TokenPlace { first: u32::MAX, last: u32::MAX, inside: None }).to_bytes(text, PARSER_EDITION) {
            Err(parser::token_key::PlaceError::PastEnd { tokens, .. }) => tokens,
            other => panic!("a place past every token is refused as past the end: {other:?}"),
        };
        let own = tokens.item(start).expect("an item starts at a token of its file's lex");
        let other = shifted.item(start);
        for first in 0..count.min(40) {
            for last in first..(first + 4).min(count + 1) {
                for inside in [None, Some((0, 1))] {
                    let place = TokenPlace { first, last, inside };
                    let want = place.to_bytes(text, PARSER_EDITION);
                    assert_eq!(place.to_bytes_in(own, text), want, "IL2: {place:?} of the item at {start}");
                    compared += 1;
                    if other.is_some_and(|other| place.to_bytes_in(other, text) != want) {
                        caught += 1;
                    }
                }
            }
        }
    }
    assert!(compared > 500, "IL2: places compared: {compared}");
    assert!(caught > 0, "IL2 CONTROL: a table of another text gives other ranges");
}

// ---- One body's rows at its units' views, printed (D191, rule 81: `print_with`'s fold) ----

/// The lib and its test build, as a VIEW entry names them, in rank.
const LIB_AND_TESTS: [ViewUnit<'static>; 2] = [ViewUnit { test: false, name: "wide" }, ViewUnit { test: true, name: "wide" }];
/// The lib, its test build and a bin that `mod`s the lib's file.
const LIB_TESTS_AND_BIN: [ViewUnit<'static>; 3] = [ViewUnit { test: false, name: "wide" }, ViewUnit { test: true, name: "wide" }, ViewUnit { test: false, name: "tool" }];

/// What one request prints of body `B_RS`'s rows: `rows` are (place among `units`, whether the
/// row holds the error at `'c'`), each a row of the one body with its VIEW entry when `units`
/// has two or more (`viewed`); `fold`: the command's printer, else the control's
/// (`print_all_each_row`). Its stdout and tally.
fn printed_rows(units: &[ViewUnit<'_>], rows: &[(u32, bool)], viewed: bool, fold: bool) -> (std::string::String, Tally) {
    let io = Io::simulated(SimWorld::new(23, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).expect("request context");
    let root = io.need_fs().expect("filesystem").root();
    let out = io.need_out().expect("stdout");
    let frame = cx.frame();
    let alloc = blaze_own::testing::heap().shared();
    root.publish(&cx, &Path::new("b.rs").unwrap(), Content::bytes(B_RS.as_bytes()), Durable::No).unwrap();
    let base = Name::new(Kind::BodyCheck, ContentId([1; 32]));
    let mut lines = Lines::new_in(alloc);
    for (at, holds) in rows.iter().copied() {
        let name = if at == 0 { base } else { Name::new(Kind::BodyCheck, ContentId([at as u8 + 1; 32])) };
        if !holds {
            continue;
        }
        if viewed {
            lines.view(name, base, at, units);
        }
        let (place, chain) = place_of(B_RS, "'c'", frame);
        lines.placed_tokens(name, crate::work::SYNTAX, place, b"b.rs", chain, EDITION, b"expected an expression");
    }
    let mut shown = print::Shown::new(Caps::LINES, View::Agent, "/ws", frame);
    let tally = match fold {
        true => print_all(&cx, &root, lines.blocks(), &mut shown, alloc, frame, &out).unwrap(),
        false => print_all_each_row(&cx, &root, lines.blocks(), &mut shown, alloc, frame, &out).unwrap(),
    };
    let bytes = io.script(|host| host.out.clone()).expect("simulated stdout");
    (std::string::String::from_utf8(bytes).expect("UTF-8"), tally)
}

/// (b) ONE LINE FOR ONE FINDING OF A BODY'S ROWS: the error both the lib's row and its test
/// build's row hold prints once, counted once, naming no view (every row holds it). CONTROL:
/// with the fold off at its one seam (`EachRow`), the same blocks print it twice.
#[test]
fn a_finding_every_row_holds_prints_once() {
    let (said, tally) = printed_rows(&LIB_AND_TESTS, &[(0, true), (1, true)], true, true);
    println!("folded:\n{said}");
    assert_eq!(tally.listed, 1, "one finding listed: {said}");
    assert_eq!(said.matches("expected an expression").count(), 1, "printed once: {said}");
    assert!(!said.contains("(in ") && !said.contains("(not in "), "no view named: every row holds it: {said}");
    let (each, control) = printed_rows(&LIB_AND_TESTS, &[(0, true), (1, true)], true, false);
    println!("each row:\n{each}");
    assert_eq!(control.listed, 2, "CONTROL: each row's finding listed: {each}");
    assert_eq!(each.matches("expected an expression").count(), 2, "CONTROL: printed twice: {each}");
}

/// (a) A FINDING ONLY SOME ROWS HOLD NAMES THEIR VIEW after its message: only the test build's
/// row: "(in the test build of `wide`)"; only the base row, with a test build: "(not in the test
/// build)"; only a bin's row: "(in `tool`)"; the base and the test build, not the bin: "(not in
/// `tool`)". CONTROL: the same rows with no VIEW entry (a body one unit reaches) name nothing.
#[test]
fn a_finding_only_some_rows_hold_names_their_view() {
    let cases: [(&[ViewUnit<'_>], &[(u32, bool)], &str); 4] = [
        (&LIB_AND_TESTS, &[(0, false), (1, true)], "expected an expression (in the test build of `wide`)"),
        (&LIB_AND_TESTS, &[(0, true), (1, false)], "expected an expression (not in the test build)"),
        (&LIB_TESTS_AND_BIN, &[(0, false), (1, false), (2, true)], "expected an expression (in `tool`)"),
        (&LIB_TESTS_AND_BIN, &[(0, true), (1, true), (2, false)], "expected an expression (not in `tool`)"),
    ];
    for (units, rows, words) in cases {
        let (said, tally) = printed_rows(units, rows, true, true);
        println!("{rows:?}:\n{said}");
        assert_eq!(tally.listed, 1, "one finding: {said}");
        assert!(said.contains(words), "{rows:?} says {words:?}: {said}");
        let (plain, _) = printed_rows(units, rows, false, true);
        assert!(!plain.contains("(in ") && !plain.contains("(not in "), "CONTROL: no VIEW entry, no view named: {plain}");
    }
}

/// (c) NO SHARED BODY, NOTHING MOVES: rows with no VIEW entry print the same bytes folded or not
/// (the fold touches only a body's rows at its views), and a base row's name is its body's.
#[test]
fn a_body_one_unit_reaches_prints_as_before() {
    let (folded, _) = printed_rows(&LIB_AND_TESTS, &[(0, true)], false, true);
    let (each, _) = printed_rows(&LIB_AND_TESTS, &[(0, true)], false, false);
    assert_eq!(folded, each, "no view: the same bytes");
    let name = Name::new(Kind::BodyCheck, ContentId([1; 32]));
    assert_eq!(blaze_drive::view::at_view(name, blaze_drive::view::UnitView::Base), name, "the base row is the body's name");
}

/// The record round trip: a block opened by a VIEW entry is read back whole, and its forms
/// before D191 are refused by name (rule 26).
#[test]
fn a_view_entry_round_trips_and_older_forms_are_refused() {
    let alloc = blaze_own::testing::heap().shared();
    let base = Name::new(Kind::BodyCheck, ContentId([1; 32]));
    let row = Name::new(Kind::BodyCheck, ContentId([2; 32]));
    let mut lines = Lines::new_in(alloc);
    lines.view(row, base, 1, &LIB_AND_TESTS);
    lines.note(row, Class::Head, b"a head");
    let (_, bytes) = lines.record();
    let last = decode(&bytes, alloc).expect("the record reads back");
    let block = last.block(row).expect("the row's block");
    let mut heads: std::vec::Vec<std::string::String> = std::vec::Vec::new();
    head_notes(block, &mut |words| heads.push(words.to_owned())).expect("its entries read");
    assert_eq!(heads, ["a head"], "the VIEW entry is read past, the note kept");
    let mut older = bytes.to_vec();
    older[..4].copy_from_slice(b"BEF7");
    assert!(decode(&older, alloc).is_err(), "BEF7 is refused");
}
