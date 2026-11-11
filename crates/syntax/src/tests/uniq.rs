//! `&uniq` and `ref uniq` under `#![blaze::uniq]` (docs/v2/plan/design/edition.md, change 10;
//! the grammar is `parser`'s `grammar/uniq.rs`): each position, a reference pattern's
//! included, parses to the tree of its `mut` spelling, a `uniq` that the lookahead keeps a name parses as without the gate, and without
//! the gate `&uniq T` is the syntax error it always was.

use parser::Edition;

use crate::{Extensions, Parse, SourceFile, SyntaxKind};

/// One unique reference in each position: a field's and a parameter's type, with and without
/// a lifetime and nested, a receiver with and without a lifetime, a borrow of a place, of an
/// index, of a parenthesized and of a dereferenced expression, and `ref` bindings in a `let`,
/// an `if let` and a closure's parameter.
const POSITIONS: &str = r#"
struct S<'a> { r: &'a uniq u32 }
fn ty(r: &uniq u32, s: &'static uniq [u8], t: &uniq &uniq u32, f: &uniq dyn FnMut(), g: &uniq (u8, u8)) {}
impl S<'_> {
    fn by(&uniq self) {}
    fn named<'a>(&'a uniq self) -> &'a uniq u32 { &uniq *self.r }
}
fn expr(mut x: u32, mut v: [u32; 1]) {
    let a = &uniq x;
    let b = &uniq v[0];
    let c = &uniq (x);
    let d = &uniq &uniq x;
    let ref uniq e = x;
    if let Some(ref uniq f) = Some(x) {}
    let g = |ref uniq h: u32| ();
}
"#;

/// Each `uniq` here is a name, read as without the gate: a parameter, a variable and a binding
/// named `uniq`, a borrow of it before `;`, `,`, `)`, `]`, `{`, `.`, `::`, `as` and `==`, a
/// macro `uniq!`, and a type `uniq` before `,`, `<` and `::`.
const NAMES: &str = r#"
fn names(uniq: u32, p: &uniq, q: &uniq<u8>, r: &uniq::X, s: &uniq!()) {
    let uniq = 1;
    let a = &uniq;
    let b = (&uniq, &uniq.0, &uniq.f(), &uniq::C, &uniq::<u8>::C, &uniq as &u32, &uniq == &1);
    let c = &uniq!();
    for x in &uniq {}
    let ref uniq = 2;
    let d = [&uniq];
    let e = &uniq { x: 1 };
}
"#;

/// A unique reference pattern wherever a pattern is read (a parameter's, a `let`'s, a
/// `let else`'s, a match arm's alternatives, a closure's parameter, a `for`'s, an `if let`'s)
/// and over each kind of pattern: a binding, a tuple, a slice, a struct, a tuple struct,
/// another reference pattern, a `ref uniq` binding, `_`, literals and a parenthesized path.
const PATTERNS: &str = r#"
fn pats(&uniq a: &uniq u32, (&uniq b, _): (&uniq u32, u8)) {
    let &uniq c = r;
    let &uniq (d, e) = pair;
    let &uniq [f, ..] = slice;
    let &uniq S { g } = s;
    let &uniq Some(h) = o else { return };
    let &uniq &uniq i = rr;
    let &uniq ref uniq j = r;
    let &uniq _ = r;
    let &uniq 0 = r else { return };
    let &uniq -1 = r else { return };
    let &uniq (<T>::C) = r else { return };
    match r { &uniq 1 | &uniq 2 => {} &uniq k => {} }
    let l = |&uniq m: &uniq u32| ();
    for &uniq n in v {}
    if let Some(&uniq o) = p {}
}
"#;

/// Each `uniq` here is the name a shared reference pattern binds or starts with, read as
/// without the gate: before `:`, `=`, `,`, `)`, `]`, `}`, `@`, `|`, `if`, `=>` and `in`, and
/// before the `::`, `!` and `.` that go on with a path, a macro call and a range.
const PATTERN_NAMES: &str = r#"
fn names(&uniq: &u32) {
    let &uniq = r;
    let (&uniq, &uniq) = pair;
    let [&uniq] = arr;
    let S { f: &uniq } = s;
    let &uniq @ _ = r;
    let &uniq: &u32 = r;
    match r { &uniq | &uniq => {} &uniq if true => {} &uniq::C => {} &uniq::<u8>::C => {} }
    match r { &uniq!() => {} &uniq..=3 => {} }
    for &uniq in v {}
    let k = |&uniq| ();
}
"#;

fn gated(text: &str) -> Parse<SourceFile> {
    SourceFile::parse_shared_with(text, Edition::CURRENT, Extensions::UNIQ)
}

fn ungated(text: &str) -> Parse<SourceFile> {
    SourceFile::parse_shared_with(text, Edition::CURRENT, Extensions::NONE)
}

/// The kind of every node and token of the parse, in order: its shape, without the text.
fn kinds(parse: &Parse<SourceFile>) -> Vec<SyntaxKind> {
    parse.syntax_node().descendants_with_tokens().map(|it| it.kind()).collect()
}

/// How many `mut` tokens of the parse are spelled `uniq`.
fn spelled_uniq(parse: &Parse<SourceFile>) -> usize {
    parse
        .syntax_node()
        .descendants_with_tokens()
        .filter_map(|it| it.into_token())
        .filter(|it| it.kind() == SyntaxKind::MUT_KW && it.text() == "uniq")
        .count()
}

#[test]
fn every_position_parses_as_its_mut_spelling() {
    let uniq = gated(POSITIONS);
    assert!(uniq.errors().is_empty(), "{}", uniq.debug_dump());
    assert_eq!(uniq.extensions(), Extensions::UNIQ);
    assert_eq!(spelled_uniq(&uniq), POSITIONS.matches("uniq").count(), "{}", uniq.debug_dump());
    let spelled_mut = POSITIONS.replace("uniq", "mut");
    for mut_ in [ungated(&spelled_mut), gated(&spelled_mut)] {
        assert!(mut_.errors().is_empty(), "{}", mut_.debug_dump());
        assert_eq!(spelled_uniq(&mut_), 0);
        assert_eq!(kinds(&uniq), kinds(&mut_), "{}\n{}", uniq.debug_dump(), mut_.debug_dump());
    }
}

#[test]
fn a_uniq_the_lookahead_keeps_a_name_parses_as_without_the_gate() {
    for names in [NAMES, PATTERN_NAMES] {
        let with = gated(names);
        let without = ungated(names);
        assert_eq!(spelled_uniq(&with), 0, "{}", with.debug_dump());
        assert_eq!(
            kinds(&with),
            kinds(&without),
            "{}\n{}",
            with.debug_dump(),
            without.debug_dump()
        );
        assert_eq!(with.errors(), without.errors());
    }
}

/// `&uniq pat` is the reference pattern `&mut pat`: the same tree, a `REF_PAT` whose `mut`
/// token is spelled `uniq`, with no error, in every position and over every kind of pattern.
#[test]
fn a_reference_pattern_parses_as_its_mut_spelling() {
    let uniq = gated(PATTERNS);
    assert!(uniq.errors().is_empty(), "{}", uniq.debug_dump());
    assert_eq!(spelled_uniq(&uniq), PATTERNS.matches("uniq").count(), "{}", uniq.debug_dump());
    let spelled_mut = PATTERNS.replace("uniq", "mut");
    for mut_ in [ungated(&spelled_mut), gated(&spelled_mut)] {
        assert!(mut_.errors().is_empty(), "{}", mut_.debug_dump());
        assert_eq!(spelled_uniq(&mut_), 0);
        assert_eq!(kinds(&uniq), kinds(&mut_), "{}\n{}", uniq.debug_dump(), mut_.debug_dump());
    }
    assert!(kinds(&uniq).contains(&SyntaxKind::REF_PAT), "{}", uniq.debug_dump());
}

/// A `(` or `[` after `&uniq` in a pattern starts the pattern referred to under the gate, the
/// crate's opt-in to that reading: without it `&uniq (a, b)` is a shared reference pattern
/// over a tuple struct `uniq`, and `&uniq [c]` a syntax error.
#[test]
fn under_the_gate_a_pattern_follows_uniq() {
    let text = "fn f() { let &uniq (a, b) = p; let &uniq [c] = q; }";
    let with = gated(text);
    assert!(with.errors().is_empty(), "{}", with.debug_dump());
    assert_eq!(spelled_uniq(&with), 2, "{}", with.debug_dump());
    assert!(kinds(&with).contains(&SyntaxKind::TUPLE_PAT), "{}", with.debug_dump());
    assert!(!kinds(&with).contains(&SyntaxKind::TUPLE_STRUCT_PAT), "{}", with.debug_dump());
    let without = ungated(text);
    assert_eq!(spelled_uniq(&without), 0);
    assert!(kinds(&without).contains(&SyntaxKind::TUPLE_STRUCT_PAT), "{}", without.debug_dump());
    assert!(!without.errors().is_empty(), "`&uniq [c]` is no pattern without the gate");
}

/// A token that starts an operand and could go on with a name `uniq` (`(`, `[`, `-`, `*`)
/// starts the operand under the gate: the crate's opt-in to that reading. Without it, the
/// same text is a call, an index, a subtraction and a multiplication of the variable.
#[test]
fn under_the_gate_an_operand_follows_uniq() {
    let text = "fn f() { let _ = (&uniq (x), &uniq [0], &uniq - 1, &uniq * x); }";
    let with = gated(text);
    assert!(with.errors().is_empty(), "{}", with.debug_dump());
    assert_eq!(spelled_uniq(&with), 4, "{}", with.debug_dump());
    let without = ungated(text);
    assert_eq!(spelled_uniq(&without), 0);
    let without = kinds(&without);
    for kind in [SyntaxKind::CALL_EXPR, SyntaxKind::INDEX_EXPR, SyntaxKind::BIN_EXPR] {
        assert!(without.contains(&kind), "{kind:?} without the gate");
    }
    assert!(!kinds(&with).contains(&SyntaxKind::CALL_EXPR), "{}", with.debug_dump());
}

/// Without the gate the parse is rustc's: `&uniq u32` is a reference to a type `uniq` and
/// then a syntax error, whatever attributes the text says (the gate is the crate's, read
/// before the parse, never by it).
#[test]
fn without_the_gate_uniq_is_a_syntax_error_as_before() {
    for text in [
        "fn f(r: &uniq u32) {}",
        "#![blaze::uniq]\nfn f(r: &uniq u32) {}",
        "fn f() { let _ = &uniq x; }",
        "fn f(&uniq self) {}",
        "fn f() { let ref uniq x = 1; }",
        "fn f() { let &uniq x = r; }",
        "fn f(&uniq x: &mut u32) {}",
    ] {
        let parse = ungated(text);
        assert!(!parse.errors().is_empty(), "{text}: {}", parse.debug_dump());
        assert_eq!(spelled_uniq(&parse), 0, "{text}");
        assert_eq!(parse.debug_dump(), SourceFile::parse(text, Edition::CURRENT).debug_dump());
    }
}

/// An outline under the gate parses its lazy bodies under it, to the full parse's tree, and a
/// reparse of a gated parse stays gated.
#[test]
fn outlines_and_reparses_keep_the_gate() {
    let outline = SourceFile::parse_outline_with(POSITIONS, Edition::CURRENT, Extensions::UNIQ);
    assert_eq!(outline.extensions(), Extensions::UNIQ);
    assert!(outline.errors_with_bodies().is_empty(), "{}", outline.debug_dump());
    assert_eq!(kinds(&outline), kinds(&gated(POSITIONS)));
    assert_eq!(spelled_uniq(&outline), POSITIONS.matches("uniq").count());

    let before = "fn f(r: &uniq u32) { let _ = &uniq x; }\n";
    let after = "fn f(r: &uniq u32) { let _ = &uniq y; }\n";
    let reparsed = gated(before).reparse_to(after, Edition::CURRENT);
    assert_eq!(reparsed.extensions(), Extensions::UNIQ);
    assert!(reparsed.errors().is_empty(), "{}", reparsed.debug_dump());
    assert_eq!(kinds(&reparsed), kinds(&gated(after)));
    assert_eq!(spelled_uniq(&reparsed), 2);
}
