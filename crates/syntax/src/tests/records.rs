//! Records under `#![blaze::records]` (docs/v2/plan/design/records.md; the grammar is in
//! `parser`'s `grammar/types.rs`, `expressions.rs` and `patterns.rs`): each form parses with no
//! error and prints back as written, every near miss of a record value is the block it is
//! without the extension, a program that writes no record parses the same with it, and without
//! it each form is the syntax error it always was.

use parser::Edition;

use crate::{Extensions, Parse, SourceFile, SyntaxError, SyntaxKind, TextRange, TextSize};

/// Every form: a record type in an alias, a field, a tuple field, a parameter (with and
/// without defaults), a return type, a `let`, a generic argument, a fn pointer and a nested
/// record; a record value with named and shorthand fields, a trailing comma, a base
/// and the rest taken from defaults; a record pattern in a `let`, a parameter, a closure, a
/// match arm, a tuple and nested, with and without `..`.
const FORMS: &str = r#"
type Point = { x: i32, y: i32 };
type Nested = { at: { x: i32, y: i32 }, name: &'static str };
type Empty = {};
struct Holder { p: { x: i32, y: i32 }, list: Vec<{ x: i32, y: i32 }> }
struct Tuple({ x: i32 }, ({ y: i32 }));
type Pointer = fn({ x: i32 }) -> { y: i32 };
fn open(path: &str, opts: { create: bool = false, mode: u32 = 0o644 }) -> { fd: i32 } {
    { fd: 3 }
}
fn takes({ x, y: other }: { x: i32, y: i32 }) -> i32 { x + other }
fn values(x: i32, old: { x: i32, y: i32 }) {
    let a = { x: 1, y: 2 };
    let b = { x, y: 2 };
    let c = { x, };
    let d: { x: i32, y: i32 } = { y: 2, x: 1, };
    let e = { x: 5, ..old };
    let f = open("p", { create: true, .. });
    let g = { at: { x: 1, y: 2 }, name: "n" };
    let h = { x: 1, y: 2 }.x;
    let i = [{ x: 1 }, { x: 2 }];
    let j = ({ x: 1 }, 2);
    let k = if x > 0 { { x: 1 } } else { { x: 2 } };
}
fn patterns(p: { x: i32, y: i32 }, q: Option<{ x: i32, y: i32 }>) {
    let { x, y } = p;
    let { x: a, .. } = p;
    let ({ x: b, y: _ }, { y: c, .. }) = (p, p);
    let sum = |{ x, y }: { x: i32, y: i32 }| x + y;
    match q {
        Some({ x: 0, y }) => y,
        Some({ x, .. }) if x > 0 => x,
        { .. } => 0,
    };
    if let Some({ x, y: 1 }) = q {}
}
"#;

/// A record value's near misses, each the block it is in Rust: one shorthand name, nothing, a
/// range from nothing, the full range, a path, a labeled loop, statements, a field access, an
/// assignment, a range from a name, a call, a macro call and a struct literal.
const BLOCKS: [&str; 13] = [
    "{ a }",
    "{}",
    "{ ..a }",
    "{ .. }",
    "{ a::b }",
    "{ 'l: loop { break 'l } }",
    "{ a; b }",
    "{ a.b }",
    "{ a = 1; }",
    "{ a ..b }",
    "{ a(b) }",
    "{ a!(b: c) }",
    "{ S { a: 1 } }",
];

/// A program that writes no record, with a `{` in every place Rust has one that the extension
/// reads: struct literals and patterns, blocks as values and as arguments, const blocks as
/// generic arguments and in bindings, match arms and bodies, closures, a labeled block, and
/// braces the grammar requires.
const NO_RECORDS: &str = r#"
struct S<const N: usize> { a: u32, b: [u8; N] }
type A = S<{ 1 + 2 }>;
type B = S<{}>;
type C = S<{ N }>;
trait T { const K: usize; }
fn g<X: T<K = { 1 }>>() {}
fn f(a: u32, b: u32) -> u32 where u32: Copy {
    let s = S::<0> { a, b: [] };
    let S { a: x, .. } = s;
    let t = { a };
    let u = { a; b };
    let v = { ..a };
    let w = { a::max(a, b) };
    let l = 'l: { break 'l a };
    let c = |a: u32| { a };
    let d = || -> u32 { a };
    let m = match a { 0 => { b }, _ if a > b => { a } _ => b };
    unsafe { a };
    const { 1 };
    f({ a }, { b })
}
"#;

fn on(text: &str) -> Parse<SourceFile> {
    SourceFile::parse_shared_with(text, Edition::CURRENT, Extensions::RECORDS)
}

fn off(text: &str) -> Parse<SourceFile> {
    SourceFile::parse_shared_with(text, Edition::CURRENT, Extensions::NONE)
}

fn tree(parse: &Parse<SourceFile>) -> String {
    format!("{:#?}", parse.syntax_node())
}

fn count(parse: &Parse<SourceFile>, kind: SyntaxKind) -> usize {
    parse.syntax_node().descendants().filter(|it| it.kind() == kind).count()
}

/// How many nodes of `kind` (a struct literal's or a struct pattern's) have no path: the
/// record values or the record patterns of the parse.
fn pathless(parse: &Parse<SourceFile>, kind: SyntaxKind) -> usize {
    parse
        .syntax_node()
        .descendants()
        .filter(|it| it.kind() == kind && it.children().all(|part| part.kind() != SyntaxKind::PATH))
        .count()
}

#[test]
fn every_form_parses_and_prints_back() {
    let parse = on(FORMS);
    assert_eq!(parse.errors(), Vec::<SyntaxError>::new(), "{}", tree(&parse));
    assert_eq!(parse.extensions(), Extensions::RECORDS);
    assert_eq!(parse.syntax_node().to_string(), FORMS);
    // Written above: 18 record types, 15 record values, 10 record patterns.
    assert_eq!(count(&parse, SyntaxKind::RECORD_TYPE), 18, "{}", tree(&parse));
    assert_eq!(pathless(&parse, SyntaxKind::RECORD_EXPR), 15, "{}", tree(&parse));
    assert_eq!(pathless(&parse, SyntaxKind::RECORD_PAT), 10, "{}", tree(&parse));
    // A parameter's defaults are the fields' own nodes, as a struct's default field values.
    assert_eq!(count(&parse, SyntaxKind::CONST_ARG), 2, "{}", tree(&parse));
}

#[test]
fn an_outline_parses_its_lazy_bodies_under_the_extension() {
    let whole = on(FORMS);
    let outline = SourceFile::parse_outline_with(FORMS, Edition::CURRENT, Extensions::RECORDS);
    assert_eq!(outline.errors_with_bodies(), Vec::<SyntaxError>::new());
    assert_eq!(tree(&outline), tree(&whole));
}

#[test]
fn each_near_miss_is_the_block_it_is_without_the_extension() {
    for block in BLOCKS {
        let text = format!("fn f() {{ let x = {block}; g({block}); {block} }}\n");
        let (with, without) = (on(&text), off(&text));
        assert_eq!(tree(&with), tree(&without), "{block}");
        assert_eq!(with.errors(), without.errors(), "{block}");
        assert_eq!(without.errors(), Vec::<SyntaxError>::new(), "{block}");
        assert_eq!(pathless(&with, SyntaxKind::RECORD_EXPR), 0, "{block}");
    }
}

#[test]
fn a_brace_the_grammar_requires_stays_a_block() {
    // Each body starts as a record value would; where a block is required it is one, with
    // the error Rust gives it.
    for text in [
        "fn f() { a: 1 }\n",
        "fn f() { if c { a: 1 } }\n",
        "fn f() { loop { a, b } }\n",
        "fn f() { unsafe { a: 1 } }\n",
        "fn f() { let c = || -> u8 { a: 1 }; }\n",
    ] {
        let (with, without) = (on(text), off(text));
        assert_eq!(tree(&with), tree(&without), "{text}");
        assert!(!with.errors().is_empty(), "{text}");
        assert_eq!(pathless(&with, SyntaxKind::RECORD_EXPR), 0, "{text}");
    }
}

#[test]
fn a_program_without_records_parses_the_same_under_the_extension() {
    let without = SourceFile::parse_shared(NO_RECORDS, Edition::CURRENT);
    assert_eq!(without.errors(), Vec::<SyntaxError>::new(), "{}", tree(&without));
    for outline in [false, true] {
        let with = match outline {
            false => on(NO_RECORDS),
            true => SourceFile::parse_outline_with(NO_RECORDS, Edition::CURRENT, Extensions::RECORDS),
        };
        assert_eq!(tree(&with), tree(&without));
        assert_eq!(with.errors_with_bodies(), without.errors_with_bodies());
        assert_eq!(count(&with, SyntaxKind::RECORD_TYPE), 0);
    }
}

#[test]
fn without_the_extension_each_form_is_the_error_it_was() {
    let none = off(FORMS);
    let today = SourceFile::parse_shared(FORMS, Edition::CURRENT);
    assert_eq!(none, today, "no extension is today's parse");
    assert_eq!(count(&none, SyntaxKind::RECORD_TYPE), 0);
    assert_eq!(pathless(&none, SyntaxKind::RECORD_EXPR), 0);
    assert_eq!(pathless(&none, SyntaxKind::RECORD_PAT), 0);
    // One form at a time, so that none hides behind another's recovery.
    for form in [
        "type P = { x: i32 };\n",
        "fn f(o: { create: bool = false }) {}\n",
        "fn f() { let a = { x: 1, y: 2 }; }\n",
        "fn f() { let a = { x, y }; }\n",
        "fn f() { let { x, y } = p; }\n",
        "fn f() { match p { { x, .. } => x } }\n",
        "fn f() { g(p, { create: true, .. }); }\n",
    ] {
        let none = off(form);
        assert_eq!(none, SourceFile::parse_shared(form, Edition::CURRENT), "{form}");
        assert!(!none.errors().is_empty(), "{form}");
        assert_eq!(on(form).errors(), Vec::<SyntaxError>::new(), "{form}");
    }
}

#[test]
fn an_edit_that_makes_a_block_a_record_is_the_whole_parse() {
    // `,` inserted after `a`: the block `{ a }` is the record `{ a, }`.
    let before = "fn f(a: u8) { let x = { a }; }\n";
    let after = "fn f(a: u8) { let x = { a, }; }\n";
    let at = TextSize::from(before.find(" };").unwrap() as u32);
    let edited = on(before).reparse_onto(TextRange::empty(at), after, Edition::CURRENT);
    assert_eq!(edited.extensions(), Extensions::RECORDS);
    assert_eq!(tree(&edited), tree(&on(after)));
    assert_eq!(pathless(&edited, SyntaxKind::RECORD_EXPR), 1);
    // And back: the `,` deleted.
    let comma = TextSize::from(after.find(", }").unwrap() as u32);
    let back = on(after).reparse_onto(TextRange::at(comma, TextSize::of(',')), before, Edition::CURRENT);
    assert_eq!(tree(&back), tree(&on(before)));
    assert_eq!(pathless(&back, SyntaxKind::RECORD_EXPR), 0);
    // An edit past the tokens that decide leaves a record value a record value.
    let longer = "fn f(a: u8) { let x = { a, b: 1 }; }\n";
    let end = TextSize::from(after.find(" };").unwrap() as u32);
    let grown = on(after).reparse_onto(TextRange::empty(end), longer, Edition::CURRENT);
    assert_eq!(tree(&grown), tree(&on(longer)));
}
