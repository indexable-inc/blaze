//! The emitter against rustc's own output: each test is a UI test of rust-lang/rust
//! ad3d0bc141a02cf446e384136d250a1f6950fed5 (`tests/ui/`, MIT or Apache-2.0), its source as it
//! is and the diagnostics rustc gives for it, written as rustc writes them with `-Z ui-testing`
//! and normalized as compiletest normalizes them, against its `.stderr`.

use crate::{
    emit::{self, Sources, Style, codes},
    model::{Applicability, Diag, Expansion, Level, MacroKind, MultiSpan, Span, SuggestionStyle},
};

/// A UI test's file, named as compiletest names it.
struct File {
    name: String,
    text: String,
}

impl File {
    fn new(name: &str, text: &str) -> File {
        File { name: format!("$DIR/{name}"), text: text.to_owned() }
    }

    fn at(&self, lo: usize, hi: usize) -> Span {
        Span { file: self.name.clone(), lo: lo as u32, hi: hi as u32, expansion: None }
    }

    fn find(&self, needle: &str) -> usize {
        self.text.find(needle).unwrap_or_else(|| panic!("no `{needle}` in {}", self.name))
    }

    /// The first `needle`.
    fn span(&self, needle: &str) -> Span {
        let lo = self.find(needle);
        self.at(lo, lo + needle.len())
    }

    /// The first `needle` in the first `context`.
    fn within(&self, context: &str, needle: &str) -> Span {
        let lo = self.find(context) + context.find(needle).unwrap();
        self.at(lo, lo + needle.len())
    }

    /// From the first `from` to the end of the first `to` after it.
    fn between(&self, from: &str, to: &str) -> Span {
        let lo = self.find(from);
        let hi = lo + self.text[lo..].find(to).unwrap() + to.len();
        self.at(lo, hi)
    }

    fn sources(files: &[&File]) -> Sources {
        Sources::of(files.iter().map(|it| (it.name.clone(), it.text.clone())))
    }
}

/// An expectation written after an opening line.
fn text(expected: &str) -> String {
    expected.strip_prefix('\n').unwrap_or(expected).to_owned()
}

/// `style` with `-Z ui-testing`, as compiletest runs rustc.
fn ui(style: Style) -> Style {
    Style { ui_testing: true, ..style }
}

/// What rustc writes to stderr for `diags`: each, then its closing ones.
fn stderr(diags: &[Diag], sources: &Sources, style: Style) -> String {
    let closing = emit::summary(diags);
    let out: String =
        diags.iter().chain(&closing).map(|it| emit::written(it, sources, ui(style)).unwrap()).collect();
    compiletest(&out, false)
}

/// What rustc writes to stderr for `diags` with `--error-format=json`.
fn json(diags: &[Diag], sources: &Sources, style: Style) -> String {
    let closing = emit::summary(diags);
    let out: String = diags
        .iter()
        .chain(&closing)
        .map(|it| emit::json(it, sources, ui(style)).unwrap())
        .filter(|it| !it.is_empty())
        .map(|it| it + "\n")
        .collect();
    compiletest(&out, true)
}

/// compiletest's `normalize_output` of what rustc wrote, JSON's `\n` first as newlines.
fn compiletest(out: &str, json: bool) -> String {
    match json {
        true => super::normalized(&out.replace("\\n", "\n")),
        false => super::normalized(out),
    }
}

/// The UI test's `normalize-stderr: "(\\u001b\[[0-9;]+m)+" -> "[ANSI]"`.
fn ansi(json: &str) -> String {
    const ESCAPE: &str = "\\u001b[";
    let mut out = String::with_capacity(json.len());
    let mut rest = json;
    while let Some(at) = rest.find(ESCAPE) {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let mut codes = false;
        while let Some(after) = rest.strip_prefix(ESCAPE) {
            let digits = after.find(|c: char| !(c.is_ascii_digit() || c == ';')).unwrap_or(after.len());
            if digits == 0 || !after[digits..].starts_with('m') {
                break;
            }
            rest = &after[digits + 1..];
            codes = true;
        }
        match codes {
            true => out.push_str("[ANSI]"),
            false => {
                out.push_str(ESCAPE);
                rest = &rest[ESCAPE.len()..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// An explanation as it is in a `.stderr` of JSON: a JSON string with its `\n` as newlines.
fn explained(code: &str) -> String {
    let explanation = codes::explanation(code).unwrap();
    serde_json::to_string(explanation).unwrap().replace("\\n", "\n")
}

// tests/ui/error-codes/E0412.rs
#[test]
fn one_span_with_a_label() {
    let file = File::new("E0412.rs", "impl Something {} //~ ERROR E0425\n\nfn main() {\n}\n");
    let at = file.span("Something");
    let mut diag = Diag::error(Some("E0425"), "cannot find type `Something` in this scope", at.clone());
    diag.span_label(at, "not found in this scope");
    let expected = text(
        r#"
error[E0425]: cannot find type `Something` in this scope
  --> $DIR/E0412.rs:1:6
   |
LL | impl Something {}
   |      ^^^^^^^^^ not found in this scope

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0425`.
"#,
    );
    assert_eq!(stderr(&[diag], &File::sources(&[&file]), Style::default()), expected);
}

// tests/ui/error-codes/E0408.rs
#[test]
fn primary_and_secondary_labels_on_one_line() {
    let file = File::new(
        "E0408.rs",
        r#"fn main() {
    let x = Some(0);

    match x {
        Some(y) | None => {} //~  ERROR variable `y` is not bound in all patterns
        _ => ()
    }
}
"#,
    );
    let none = file.within("Some(y) | None", "None");
    let mut diag = Diag::error(Some("E0408"), "variable `y` is not bound in all patterns", none.clone());
    diag.span_label(none, "pattern doesn't bind `y`");
    diag.span_label(file.within("Some(y) | None", "y"), "variable not in all patterns");
    let expected = text(
        r#"
error[E0408]: variable `y` is not bound in all patterns
  --> $DIR/E0408.rs:5:19
   |
LL |         Some(y) | None => {}
   |              -    ^^^^ pattern doesn't bind `y`
   |              |
   |              variable not in all patterns

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0408`.
"#,
    );
    assert_eq!(stderr(&[diag], &File::sources(&[&file]), Style::default()), expected);
}

// tests/ui/error-codes/E0428.rs
#[test]
fn primary_and_secondary_labels_on_several_lines() {
    let file = File::new(
        "E0428.rs",
        r#"//@ dont-require-annotations: NOTE

struct Bar; //~ NOTE previous definition of the type `Bar` here
struct Bar; //~ ERROR E0428

fn main () {
}
"#,
    );
    let first = file.find("struct Bar;");
    let second = first + 1 + file.text[first + 1..].find("struct Bar;").unwrap();
    let redefined = file.at(second, second + "struct Bar;".len());
    let mut diag =
        Diag::error(Some("E0428"), "the name `Bar` is defined multiple times", redefined.clone());
    diag.span_label(file.at(first, first + "struct Bar;".len()), "previous definition of the type `Bar` here");
    diag.span_label(redefined, "`Bar` redefined here");
    diag.note("`Bar` must be defined only once in the type namespace of this module");
    let expected = text(
        r#"
error[E0428]: the name `Bar` is defined multiple times
  --> $DIR/E0428.rs:4:1
   |
LL | struct Bar;
   | ----------- previous definition of the type `Bar` here
LL | struct Bar;
   | ^^^^^^^^^^^ `Bar` redefined here
   |
   = note: `Bar` must be defined only once in the type namespace of this module

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0428`.
"#,
    );
    assert_eq!(stderr(&[diag], &File::sources(&[&file]), Style::default()), expected);
}

// tests/ui/mismatched_types/numeric-literal-cast.rs: labels under one line and under a callee
// and its parameter in a note, and a verbose suggestion.
#[test]
fn labels_a_note_with_spans_and_a_verbose_suggestion() {
    let file = File::new(
        "numeric-literal-cast.rs",
        r#"fn foo(_: u16) {}
fn foo1(_: f64) {}
fn foo2(_: i32) {}

fn main() {
    foo(1u8);
//~^ ERROR mismatched types
    foo1(2f32);
//~^ ERROR mismatched types
    foo2(3i16);
//~^ ERROR mismatched types
}
"#,
    );
    let mismatch = |callee: &str, arg: &str, found: &str, expected: &str, param: &str| {
        let call = format!("    {callee}({arg});");
        let at = file.within(&call, arg);
        let mut diag = Diag::error(Some("E0308"), "mismatched types", at.clone());
        diag.span_label(at, format!("expected `{expected}`, found `{found}`"));
        diag.span_label(file.within(&call, callee), "arguments to this function are incorrect");
        let def = format!("fn {callee}({param})");
        let mut note = MultiSpan::of(file.within(&def, callee));
        note.labels.push((file.within(&def, param), String::new()));
        diag.span_note(note, "function defined here");
        let suffix = file.within(&call, &arg[1..]);
        diag.multipart_suggestion(
            format!("change the type of the numeric literal from `{found}` to `{expected}`"),
            vec![(suffix, expected.to_owned())],
            Applicability::MachineApplicable,
        );
        diag
    };
    let diags = [
        mismatch("foo", "1u8", "u8", "u16", "_: u16"),
        mismatch("foo1", "2f32", "f32", "f64", "_: f64"),
        mismatch("foo2", "3i16", "i16", "i32", "_: i32"),
    ];
    let expected = text(
        r#"
error[E0308]: mismatched types
  --> $DIR/numeric-literal-cast.rs:6:9
   |
LL |     foo(1u8);
   |     --- ^^^ expected `u16`, found `u8`
   |     |
   |     arguments to this function are incorrect
   |
note: function defined here
  --> $DIR/numeric-literal-cast.rs:1:4
   |
LL | fn foo(_: u16) {}
   |    ^^^ ------
help: change the type of the numeric literal from `u8` to `u16`
   |
LL -     foo(1u8);
LL +     foo(1u16);
   |

error[E0308]: mismatched types
  --> $DIR/numeric-literal-cast.rs:8:10
   |
LL |     foo1(2f32);
   |     ---- ^^^^ expected `f64`, found `f32`
   |     |
   |     arguments to this function are incorrect
   |
note: function defined here
  --> $DIR/numeric-literal-cast.rs:2:4
   |
LL | fn foo1(_: f64) {}
   |    ^^^^ ------
help: change the type of the numeric literal from `f32` to `f64`
   |
LL -     foo1(2f32);
LL +     foo1(2f64);
   |

error[E0308]: mismatched types
  --> $DIR/numeric-literal-cast.rs:10:10
   |
LL |     foo2(3i16);
   |     ---- ^^^^ expected `i32`, found `i16`
   |     |
   |     arguments to this function are incorrect
   |
note: function defined here
  --> $DIR/numeric-literal-cast.rs:3:4
   |
LL | fn foo2(_: i32) {}
   |    ^^^^ ------
help: change the type of the numeric literal from `i16` to `i32`
   |
LL -     foo2(3i16);
LL +     foo2(3i32);
   |

error: aborting due to 3 previous errors

For more information about this error, try `rustc --explain E0308`.
"#,
    );
    assert_eq!(stderr(&diags, &File::sources(&[&file]), Style::default()), expected);
}

// tests/ui/return/tail-expr-if-as-return.rs
#[test]
fn a_multi_line_span() {
    let file = File::new(
        "tail-expr-if-as-return.rs",
        r#"fn main() {
    if true {
        "" //~ ERROR mismatched types [E0308]
    }
}
"#,
    );
    let at = file.span(r#""""#);
    let mut diag = Diag::error(Some("E0308"), "mismatched types", at.clone());
    diag.span_label(at, "expected `()`, found `&str`");
    diag.span_label(
        file.between("if true", "    }"),
        "`if` expressions without `else` arms expect their inner expression to be `()`",
    );
    let expected = text(
        r#"
error[E0308]: mismatched types
  --> $DIR/tail-expr-if-as-return.rs:3:9
   |
LL | /     if true {
LL | |         ""
   | |         ^^ expected `()`, found `&str`
LL | |     }
   | |_____- `if` expressions without `else` arms expect their inner expression to be `()`

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0308`.
"#,
    );
    assert_eq!(stderr(&[diag], &File::sources(&[&file]), Style::default()), expected);
}

// tests/ui/annotate-snippet/multiple-files.rs, with its auxiliary/other_file.rs
#[test]
fn labels_in_two_files() {
    let main = File::new(
        "multiple-files.rs",
        r#"//@ aux-build:other_file.rs
//@ compile-flags: --error-format human

extern crate other_file;

fn main() {
    other_file::WithPrivateMethod.private_method();
}
"#,
    );
    let other = File::new(
        "auxiliary/other_file.rs",
        r#"pub struct WithPrivateMethod;

impl WithPrivateMethod {
    /// Private to get an error involving two files
    fn private_method(&self) {}
}
"#,
    );
    let at = main.within(".private_method()", "private_method");
    let mut diag = Diag::error(Some("E0624"), "method `private_method` is private", at.clone());
    diag.span_label(at, "private method");
    diag.span_label(other.span("fn private_method(&self)"), "private method defined here");
    let expected = text(
        r#"
error[E0624]: method `private_method` is private
  --> $DIR/multiple-files.rs:7:35
   |
LL |     other_file::WithPrivateMethod.private_method();
   |                                   ^^^^^^^^^^^^^^ private method
   |
  ::: $DIR/auxiliary/other_file.rs:5:5
   |
LL |     fn private_method(&self) {}
   |     ------------------------ private method defined here

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0624`.
"#,
    );
    assert_eq!(stderr(&[diag], &File::sources(&[&main, &other]), Style::default()), expected);
}

// tests/ui/match/postfix-match/pf-match-exhaustiveness.rs: its note points into core, whose
// source rustc under compiletest doesn't have, so the note is where it is and its label a note.
#[test]
fn labels_in_a_file_whose_source_is_not_shown() {
    let main = File::new(
        "pf-match-exhaustiveness.rs",
        r#"#![feature(postfix_match)]

fn main() {
    Some(1).match { //~ ERROR non-exhaustive patterns
        None => {},
    }
}
"#,
    );
    let option = File {
        name: "$SRC_DIR/core/src/option.rs".to_owned(),
        text: "pub enum Option<T> {\n    /// No value.\n    None,\n    /// Some value of type `T`.\n    Some(T),\n}\n"
            .to_owned(),
    };
    let at = main.span("Some(1)");
    let mut diag = Diag::error(Some("E0004"), "non-exhaustive patterns: `Some(_)` not covered", at.clone());
    diag.span_label(at, "pattern `Some(_)` not covered");
    let mut defined = MultiSpan::of(option.span("pub enum Option<T>"));
    defined.labels.push((option.span("Some(T)"), "not covered".to_owned()));
    diag.span_note(defined, "`Option<i32>` defined here");
    diag.note("the matched value is of type `Option<i32>`");
    diag.suggestion_verbose(
        main.span("None => {}").shrink_to_hi(),
        "ensure that all possible cases are being handled by adding a match arm with a wildcard pattern or an \
         explicit pattern as shown",
        ",\n        Some(_) => todo!()",
        Applicability::HasPlaceholders,
    );
    let mut sources = File::sources(&[&main]);
    sources.add_unshown(&option.name, option.text.clone());
    let expected = text(
        r#"
error[E0004]: non-exhaustive patterns: `Some(_)` not covered
  --> $DIR/pf-match-exhaustiveness.rs:4:5
   |
LL |     Some(1).match {
   |     ^^^^^^^ pattern `Some(_)` not covered
   |
note: `Option<i32>` defined here
  --> $SRC_DIR/core/src/option.rs:LL:COL
  ::: $SRC_DIR/core/src/option.rs:LL:COL
   |
   = note: not covered
   = note: the matched value is of type `Option<i32>`
help: ensure that all possible cases are being handled by adding a match arm with a wildcard pattern or an explicit pattern as shown
   |
LL ~         None => {},
LL ~         Some(_) => todo!(),
   |

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0004`.
"#,
    );
    assert_eq!(stderr(&[diag], &sources, Style::default()), expected);
}

#[test]
fn a_file_whose_source_is_not_shown_is_placed_but_neither_quoted_nor_spliced() {
    let main = File::new("main.rs", "fn main() { None::<u8>; }\n");
    let option = File {
        name: "$SRC_DIR/core/src/option.rs".to_owned(),
        text: "pub enum Option<T> {\n    None,\n}\n".to_owned(),
    };
    let mut sources = File::sources(&[&main]);
    sources.add_unshown(&option.name, option.text.clone());
    assert_eq!(sources.position(&option.name, 25), Some((2, 5)));

    let at = main.span("None");
    let mut diag = Diag::error(None, "x", at.clone());
    diag.span_label(at, "here");
    diag.span_label(option.span("None"), "defined here");
    let unspliced = emit::written(&diag, &sources, ui(Style::default())).unwrap();
    diag.suggestion_verbose(
        option.span("None"),
        "a suggestion into a file not shown",
        "Some",
        Applicability::MachineApplicable,
    );
    assert_eq!(emit::written(&diag, &sources, ui(Style::default())).unwrap(), unspliced);
    assert!(unspliced.contains(" $SRC_DIR/core/src/option.rs:2:4\n"), "{unspliced}");
    assert!(!unspliced.contains("LL |     None,"), "{unspliced}");
    assert!(unspliced.ends_with("   = note: defined here\n\n"), "{unspliced}");

    let line = emit::json(&diag, &sources, Style::default()).unwrap();
    let json: serde_json::Value = serde_json::from_str(&line).unwrap();
    let defined = &json["spans"][1];
    assert_eq!((&defined["line_start"], &defined["column_start"]), (&serde_json::json!(2), &serde_json::json!(5)));
    assert_eq!(defined["text"], serde_json::json!([]));
}

// tests/ui/rust-2024/unsafe-extern-blocks/extern-items-unsafe.rs, edition 2024
#[test]
fn notes_without_spans() {
    let file = File::new(
        "extern-items-unsafe.rs",
        r#"//@ revisions: edition2021 edition2024
//@[edition2021] edition:2021
//@[edition2024] edition:2024

unsafe extern "C" {
    static TEST1: i32;
    fn test1(i: i32);
}

fn test2() {
    test1(TEST1);
    //~^ ERROR: call to unsafe function `test1` is unsafe
    //~| ERROR: use of extern static is unsafe
}

fn test3() {
    unsafe {
        test1(TEST1);
    }
}

fn main() {}
"#,
    );
    let call = file.span("test1(TEST1)");
    let mut first = Diag::error(
        Some("E0133"),
        "call to unsafe function `test1` is unsafe and requires unsafe block",
        call.clone(),
    );
    first.span_label(call, "call to unsafe function");
    first.note("consult the function's documentation for information on how to avoid undefined behavior");
    let static_ = file.within("test1(TEST1)", "TEST1");
    let mut second = Diag::error(
        Some("E0133"),
        "use of extern static is unsafe and requires unsafe block",
        static_.clone(),
    );
    second.span_label(static_, "use of extern static");
    second.note("extern statics are not controlled by the Rust type system: invalid data, aliasing violations or data races will cause undefined behavior");
    let expected = text(
        r#"
error[E0133]: call to unsafe function `test1` is unsafe and requires unsafe block
  --> $DIR/extern-items-unsafe.rs:11:5
   |
LL |     test1(TEST1);
   |     ^^^^^^^^^^^^ call to unsafe function
   |
   = note: consult the function's documentation for information on how to avoid undefined behavior

error[E0133]: use of extern static is unsafe and requires unsafe block
  --> $DIR/extern-items-unsafe.rs:11:11
   |
LL |     test1(TEST1);
   |           ^^^^^ use of extern static
   |
   = note: extern statics are not controlled by the Rust type system: invalid data, aliasing violations or data races will cause undefined behavior

error: aborting due to 2 previous errors

For more information about this error, try `rustc --explain E0133`.
"#,
    );
    assert_eq!(stderr(&[first, second], &File::sources(&[&file]), Style::default()), expected);
}

// tests/ui/macros/macro-path-prelude-fail-4.rs: a help with a span, then one of two lines
// without, which goes under it.
#[test]
fn helps_with_and_without_a_span() {
    let file = File::new(
        "macro-path-prelude-fail-4.rs",
        "#[derive(inline)] //~ ERROR expected derive macro, found built-in attribute `inline`\nstruct S;\n\nfn main() {}\n",
    );
    let at = file.span("inline");
    let mut diag = Diag::error(None, "expected derive macro, found built-in attribute `inline`", at.clone());
    diag.span_label(at.clone(), "not a derive macro");
    diag.span_help(at, "remove from the surrounding `derive()`");
    diag.help("add as non-Derive macro\n`#[inline]`");
    let expected = text(
        r#"
error: expected derive macro, found built-in attribute `inline`
  --> $DIR/macro-path-prelude-fail-4.rs:1:10
   |
LL | #[derive(inline)]
   |          ^^^^^^ not a derive macro
   |
help: remove from the surrounding `derive()`
  --> $DIR/macro-path-prelude-fail-4.rs:1:10
   |
LL | #[derive(inline)]
   |          ^^^^^^
   = help: add as non-Derive macro
           `#[inline]`

error: aborting due to 1 previous error

"#,
    );
    assert_eq!(stderr(&[diag], &File::sources(&[&file]), Style::default()), expected);
}

// tests/ui/generics/issue-59508.rs: two primary spans in a secondary one, and a suggestion
// short enough to be its label.
#[test]
fn an_inline_suggestion() {
    let file = File::new(
        "issue-59508.rs",
        r#"//@ run-rustfix

#![allow(dead_code)]

// This test checks that generic parameter re-ordering diagnostic suggestions contain bounds.

struct A;

impl A {
    pub fn do_things<T, 'a, 'b: 'a>() {
    //~^ ERROR lifetime parameters must be declared prior to type and const parameters
        println!("panic");
    }
}

fn main() {}
"#,
    );
    let params = "<T, 'a, 'b: 'a>";
    let mut diag = Diag::error(
        None,
        "lifetime parameters must be declared prior to type and const parameters",
        file.within(params, "'a"),
    );
    diag.primary(file.within(params, "'b"));
    diag.suggestion(
        file.span(params),
        "reorder the parameters: lifetimes, then consts and types",
        "<'a, 'b: 'a, T>",
        Applicability::MachineApplicable,
    );
    let expected = text(
        r#"
error: lifetime parameters must be declared prior to type and const parameters
  --> $DIR/issue-59508.rs:10:25
   |
LL |     pub fn do_things<T, 'a, 'b: 'a>() {
   |                     ----^^--^^----- help: reorder the parameters: lifetimes, then consts and types: `<'a, 'b: 'a, T>`

error: aborting due to 1 previous error

"#,
    );
    assert_eq!(stderr(&[diag], &File::sources(&[&file]), Style::default()), expected);
}

// tests/ui/did_you_mean/issue-56028-there-is-an-enum-variant.rs, its first error: several
// ways to substitute, four shown.
#[test]
fn several_substitutions_and_other_candidates() {
    let file = File::new(
        "issue-56028-there-is-an-enum-variant.rs",
        r#"//@ edition:2015
enum PutDown { Set }
enum AffixHeart { Set }
enum CauseToBe { Set }
enum Determine { Set }
enum TableDishesAction { Set }
enum Solidify { Set }
enum UnorderedCollection { Set }

fn setup() -> Set { Set }
//~^ ERROR cannot find type `Set` in this scope
//~| ERROR cannot find value `Set` in this scope

fn main() {
    setup();
}
"#,
    );
    let at = file.within("-> Set", "Set");
    let mut diag = Diag::error(Some("E0425"), "cannot find type `Set` in this scope", at.clone());
    diag.span_label(at.clone(), "not found in this scope");
    let enums = [
        "AffixHeart",
        "CauseToBe",
        "Determine",
        "PutDown",
        "Solidify",
        "TableDishesAction",
        "UnorderedCollection",
    ];
    diag.suggestions(
        at,
        "there is an enum variant `AffixHeart::Set` and 7 others; try using the variant's enum",
        enums.iter().map(|it| (*it).to_owned()),
        Applicability::MaybeIncorrect,
        SuggestionStyle::ShowCode,
    );
    let expected = text(
        r#"
error[E0425]: cannot find type `Set` in this scope
  --> $DIR/issue-56028-there-is-an-enum-variant.rs:10:15
   |
LL | fn setup() -> Set { Set }
   |               ^^^ not found in this scope
   |
help: there is an enum variant `AffixHeart::Set` and 7 others; try using the variant's enum
   |
LL - fn setup() -> Set { Set }
LL + fn setup() -> AffixHeart { Set }
   |
LL - fn setup() -> Set { Set }
LL + fn setup() -> CauseToBe { Set }
   |
LL - fn setup() -> Set { Set }
LL + fn setup() -> Determine { Set }
   |
LL - fn setup() -> Set { Set }
LL + fn setup() -> PutDown { Set }
   |
   = and 3 other candidates

"#,
    );
    let written = emit::written(&diag, &File::sources(&[&file]), ui(Style::default())).unwrap();
    assert_eq!(compiletest(&written, false), expected);
}

// tests/ui/lint/issue-87274-paren-parent.rs
#[test]
fn a_lint_warning_with_its_on_by_default_note() {
    let file = File::new(
        "issue-87274-paren-parent.rs",
        r#"//@ edition:2015
//@ check-pass
// Tests that we properly lint at 'paren' expressions

fn foo() -> Result<(), String>  {
    (try!(Ok::<u8, String>(1))); //~ WARN use of deprecated macro `try`
    Ok(())
}

fn main() {}
"#,
    );
    let mut diag = Diag::lint(
        "deprecated",
        "use of deprecated macro `try`: use the `?` operator instead",
        file.within("(try!", "try"),
    );
    diag.sub(
        crate::model::Sublevel::Note,
        "`#[warn(deprecated)]` on by default",
        MultiSpan::default(),
        true,
    );
    let expected = text(
        r#"
warning: use of deprecated macro `try`: use the `?` operator instead
  --> $DIR/issue-87274-paren-parent.rs:6:6
   |
LL |     (try!(Ok::<u8, String>(1)));
   |      ^^^
   |
   = note: `#[warn(deprecated)]` on by default

warning: 1 warning emitted

"#,
    );
    assert_eq!(stderr(&[diag], &File::sources(&[&file]), Style::default()), expected);
}

const MACRO: &str = r#"macro_rules! g {
    ($inp:ident) => (
        { $inp $nonexistent }
        //~^ ERROR expected one of `!`, `.`, `::`, `;`, `?`, `{`, `}`, or an operator, found `$`
    );
}

fn main() {
    let foo = 0;
    g!(foo);
}
"#;

/// tests/ui/macros/issue-6596-2.rs's error: in the expansion of `g!`, which rustc defines.
fn in_a_macro(file: &File) -> Diag {
    let mut at = file.span("$nonexistent");
    at.expansion = Some(Box::new(Expansion {
        call_site: file.span("g!(foo)"),
        kind: MacroKind::Bang,
        name: "g".to_owned(),
        def_site: Some(file.between("macro_rules! g", "    );\n}")),
        opaque: false,
    }));
    let mut diag = Diag::error(
        None,
        "expected one of `!`, `.`, `::`, `;`, `?`, `{`, `}`, or an operator, found `$`",
        at.clone(),
    );
    diag.span_label(at, "expected one of 8 possible tokens");
    diag
}

#[test]
fn a_macro_backtrace() {
    let file = File::new("issue-6596-2.rs", MACRO);
    let expected = text(
        r#"
error: expected one of `!`, `.`, `::`, `;`, `?`, `{`, `}`, or an operator, found `$`
  --> $DIR/issue-6596-2.rs:3:16
   |
LL |         { $inp $nonexistent }
   |                ^^^^^^^^^^^^ expected one of 8 possible tokens
...
LL |     g!(foo);
   |     ------- in this macro invocation
   |
   = note: this error originates in the macro `g` (in Nightly builds, run with -Z macro-backtrace for more info)

error: aborting due to 1 previous error

"#,
    );
    let sources = File::sources(&[&file]);
    assert_eq!(stderr(&[in_a_macro(&file)], &sources, Style::default()), expected);
    // The note is written once when the diagnostic has it already, as `check` gives it.
    let mut noted = in_a_macro(&file);
    noted.originates_in();
    assert_eq!(stderr(&[noted], &sources, Style::default()), expected);
}

#[test]
fn a_macro_expansion_in_json() {
    let file = File::new("issue-6596-2.rs", MACRO);
    let mut noted = in_a_macro(&file);
    noted.originates_in();
    let line = emit::json(&noted, &File::sources(&[&file]), Style::default()).unwrap();
    let value: serde_json::Value = serde_json::from_str(&line).unwrap();
    // The note rustc's text emitter adds is not one of its children.
    assert_eq!(value["children"], serde_json::json!([]));
    let expansion = &value["spans"][0]["expansion"];
    assert_eq!(expansion["macro_decl_name"], "g!");
    assert_eq!(expansion["span"]["line_start"], 10);
    assert_eq!(expansion["span"]["column_start"], 5);
    assert_eq!(expansion["span"]["column_end"], 12);
    assert_eq!(expansion["span"]["expansion"], serde_json::Value::Null);
    let def = &expansion["def_site_span"];
    assert_eq!(def["text"][0]["text"], "macro_rules! g {");
    assert_eq!(def["column_start"], 1);
    assert_eq!(def["column_end"], 15);
    assert!(value["rendered"].as_str().unwrap().contains("= note: this error originates in the macro `g`"));
}

// tests/ui/diagnostic-flags/error-format-short.rs
#[test]
fn short() {
    let file = File::new(
        "error-format-short.rs",
        r#"//! Check that compile errors are formatted in the "short" style
//! when `--error-format=short` is used.

//@ compile-flags: --error-format=short

fn foo(_: u32) {}

fn main() {
    foo("Bonjour".to_owned());
    let x = 0u32;
    x.salut();
}
"#,
    );
    let arg = file.span(r#""Bonjour".to_owned()"#);
    let mut mismatch = Diag::error(Some("E0308"), "mismatched types", arg.clone());
    mismatch.span_label(arg, "expected `u32`, found `String`");
    mismatch.span_label(file.within("    foo(", "foo"), "arguments to this function are incorrect");
    let mut def = MultiSpan::of(file.within("fn foo(", "foo"));
    def.labels.push((file.span("_: u32"), String::new()));
    mismatch.span_note(def, "function defined here");
    let salut = file.span("salut");
    let mut method = Diag::error(
        Some("E0599"),
        "no method named `salut` found for type `u32` in the current scope",
        salut.clone(),
    );
    method.span_label(salut, "method not found in `u32`");
    let expected = text(
        r#"
$DIR/error-format-short.rs:9:9: error[E0308]: mismatched types: expected `u32`, found `String`
$DIR/error-format-short.rs:11:7: error[E0599]: no method named `salut` found for type `u32` in the current scope: method not found in `u32`
error: aborting due to 2 previous errors
"#,
    );
    let short = Style { short: true, ..Style::default() };
    assert_eq!(stderr(&[mismatch, method], &File::sources(&[&file]), short), expected);
}

// tests/ui/json/multibyte-rendered-ansi-issue-157148.rs: a lint's JSON with a note and a
// suggestion, rendered in colour.
#[test]
fn colour_in_json() {
    let file = File::new(
        "multibyte-rendered-ansi-issue-157148.rs",
        r#"// Regression test for <https://github.com/rust-lang/rust/issues/157148>.
// JSON rendered diagnostics should not ICE when spans involve non-ASCII source text.

//@ edition: 2021
//@ check-pass
//@ compile-flags: --error-format=json --json=diagnostic-rendered-ansi
//@ normalize-stderr: "(\\u001b\[[0-9;]+m)+" -> "[ANSI]"

#![warn(unused_mut)]

fn main() {
    let mut x = 0; // 这是一段中文注释，用于在被下划线标注的行中加入多字节字符
    //~^ WARNING variable does not need to be mutable
    //~| HELP remove this `mut`
    let _ = x;
}
"#,
    );
    assert_eq!(file.text.len(), 567);
    let mut diag = Diag::lint("unused_mut", "variable does not need to be mutable", file.span("mut x"));
    diag.span_note(file.within("#![warn(unused_mut)]", "unused_mut"), "the lint level is defined here");
    diag.suggestion_with_style(
        vec![(file.span("mut "), String::new())],
        "remove this `mut`",
        Applicability::MachineApplicable,
        SuggestionStyle::HideCodeInline,
    );
    let expected = text(
        r##"
{"$message_type":"diagnostic","message":"variable does not need to be mutable","code":{"code":"unused_mut","explanation":null},"level":"warning","spans":[{"file_name":"$DIR/multibyte-rendered-ansi-issue-157148.rs","byte_start":365,"byte_end":370,"line_start":12,"line_end":12,"column_start":9,"column_end":14,"is_primary":true,"text":[{"text":"    let mut x = 0; // 这是一段中文注释，用于在被下划线标注的行中加入多字节字符","highlight_start":9,"highlight_end":14}],"label":null,"suggested_replacement":null,"suggestion_applicability":null,"expansion":null}],"children":[{"message":"the lint level is defined here","code":null,"level":"note","spans":[{"file_name":"$DIR/multibyte-rendered-ansi-issue-157148.rs","byte_start":331,"byte_end":341,"line_start":9,"line_end":9,"column_start":9,"column_end":19,"is_primary":true,"text":[{"text":"#![warn(unused_mut)]","highlight_start":9,"highlight_end":19}],"label":null,"suggested_replacement":null,"suggestion_applicability":null,"expansion":null}],"children":[],"rendered":null},{"message":"remove this `mut`","code":null,"level":"help","spans":[{"file_name":"$DIR/multibyte-rendered-ansi-issue-157148.rs","byte_start":365,"byte_end":369,"line_start":12,"line_end":12,"column_start":9,"column_end":13,"is_primary":true,"text":[{"text":"    let mut x = 0; // 这是一段中文注释，用于在被下划线标注的行中加入多字节字符","highlight_start":9,"highlight_end":13}],"label":null,"suggested_replacement":"","suggestion_applicability":"MachineApplicable","expansion":null}],"children":[],"rendered":null}],"rendered":"[ANSI]warning[ANSI]: variable does not need to be mutable[ANSI]
  [ANSI]--> [ANSI]$DIR/multibyte-rendered-ansi-issue-157148.rs:12:9
   [ANSI]|[ANSI]
[ANSI]LL[ANSI] [ANSI]|[ANSI]     let mut x = 0; // 这是一段中文注释，用于在被下划线标注的行中加入多字节字符
   [ANSI]|[ANSI]         [ANSI]----[ANSI]^[ANSI]
   [ANSI]|[ANSI]         [ANSI]|[ANSI]
   [ANSI]|[ANSI]         [ANSI]help: remove this `mut`[ANSI]
   [ANSI]|[ANSI]
[ANSI]note[ANSI]: the lint level is defined here
  [ANSI]--> [ANSI]$DIR/multibyte-rendered-ansi-issue-157148.rs:9:9
   [ANSI]|[ANSI]
[ANSI]LL[ANSI] [ANSI]|[ANSI] #![warn(unused_mut)]
   [ANSI]|[ANSI]         [ANSI]^^^^^^^^^^[ANSI]

"}
{"$message_type":"diagnostic","message":"1 warning emitted","code":null,"level":"warning","spans":[],"children":[],"rendered":"[ANSI]warning[ANSI]: 1 warning emitted[ANSI]

"}
"##,
    );
    let ansi_json = Style { ansi: true, ..Style::default() };
    let out = json(&[diag], &File::sources(&[&file]), ansi_json);
    assert_eq!(ansi(&out), expected);
}

/// The escape codes themselves, as rustc's `Renderer::styled()` gives them: `anstyle`'s bold
/// (`1`) then colour (bright red `91`, yellow `33`, bright blue `94`, bright green `92`), each
/// run ended with a reset (`0`).
#[test]
fn colour_codes_are_rustcs() {
    let file = File::new("E0412.rs", "impl Something {} //~ ERROR E0425\n\nfn main() {\n}\n");
    let at = file.span("Something");
    let mut diag = Diag::error(Some("E0425"), "cannot find type `Something` in this scope", at.clone());
    diag.span_label(at.clone(), "not found in this scope");
    diag.span_note(at, "here");
    let style = ui(Style { ansi: true, ..Style::default() });
    let rendered = emit::written(&diag, &File::sources(&[&file]), style).unwrap();
    let lines: Vec<&str> = rendered.lines().collect();
    assert_eq!(
        lines[0],
        "\x1b[1m\x1b[91merror[E0425]\x1b[0m\x1b[1m: cannot find type `Something` in this scope\x1b[0m"
    );
    assert_eq!(lines[1], "  \x1b[1m\x1b[94m--> \x1b[0m$DIR/E0412.rs:1:6");
    assert!(lines.iter().any(|it| *it == "\x1b[1m\x1b[92mnote\x1b[0m: here"), "{rendered:?}");
    let warned =
        emit::written(&Diag::new(Level::Warning, None, "1 warning emitted"), &File::sources(&[]), style).unwrap();
    assert_eq!(warned, "\x1b[1m\x1b[33mwarning\x1b[0m\x1b[1m: 1 warning emitted\x1b[0m\n\n");
    let failure = Diag::new(Level::FailureNote, None, "For more information about this error, try `rustc --explain E0425`.");
    let written = emit::written(&failure, &File::sources(&[]), style).unwrap();
    assert_eq!(written, "\x1b[1mFor more information about this error, try `rustc --explain E0425`.\x1b[0m\n");
}

// tests/ui/lint/future-incompat-json-test.rs, its diagnostic: a lint's JSON with an inline
// suggestion, rendered.
#[test]
fn json_of_a_lint_with_an_inline_suggestion() {
    let file = File::new(
        "future-incompat-json-test.rs",
        r#"//@ compile-flags: -Zfuture-incompat-test --json=future-incompat --error-format=json
//@ check-pass

// The `-Zfuture-incompat-test flag causes any normal warning to be included
// in the future-incompatible report. The stderr output here should mention
// the future incompatible report (as extracted by compiletest).

fn main() {
    let x = 1;
}
"#,
    );
    assert_eq!(file.text.len(), 349);
    let x = file.within("let x", "x");
    let mut diag = Diag::lint("unused_variables", "unused variable: `x`", x.clone());
    diag.suggestion(
        x,
        "if this is intentional, prefix it with an underscore",
        "_x",
        Applicability::MachineApplicable,
    );
    let expected = text(
        r##"
{"$message_type":"diagnostic","message":"unused variable: `x`","code":{"code":"unused_variables","explanation":null},"level":"warning","spans":[{"file_name":"$DIR/future-incompat-json-test.rs","byte_start":340,"byte_end":341,"line_start":9,"line_end":9,"column_start":9,"column_end":10,"is_primary":true,"text":[{"text":"    let x = 1;","highlight_start":9,"highlight_end":10}],"label":null,"suggested_replacement":null,"suggestion_applicability":null,"expansion":null}],"children":[{"message":"if this is intentional, prefix it with an underscore","code":null,"level":"help","spans":[{"file_name":"$DIR/future-incompat-json-test.rs","byte_start":340,"byte_end":341,"line_start":9,"line_end":9,"column_start":9,"column_end":10,"is_primary":true,"text":[{"text":"    let x = 1;","highlight_start":9,"highlight_end":10}],"label":null,"suggested_replacement":"_x","suggestion_applicability":"MachineApplicable","expansion":null}],"children":[],"rendered":null}],"rendered":"warning: unused variable: `x`
  --> $DIR/future-incompat-json-test.rs:9:9
   |
LL |     let x = 1;
   |         ^ help: if this is intentional, prefix it with an underscore: `_x`

"}"##,
    );
    let line = emit::json(&diag, &File::sources(&[&file]), ui(Style::default())).unwrap();
    assert_eq!(compiletest(&line, true), expected);
}

// tests/ui/json/json-short.rs: a span at the end of the file, short, with the explanation.
#[test]
fn json_short_at_the_end_of_a_file() {
    let file = File::new(
        "json-short.rs",
        "//~v ERROR `main` function not found in crate `json_short`\n//@ compile-flags: --json=diagnostic-short --error-format=json\n",
    );
    assert_eq!(file.text.len(), 122);
    let end = file.at(122, 122);
    let mut diag = Diag::error(Some("E0601"), "`main` function not found in crate `json_short`", end.clone());
    diag.span_label(end, "consider adding a `main` function to `$DIR/json-short.rs`");
    let expected = text(
        r#"
{"$message_type":"diagnostic","message":"`main` function not found in crate `json_short`","code":{"code":"E0601","explanation":EXPLANATION},"level":"error","spans":[{"file_name":"$DIR/json-short.rs","byte_start":122,"byte_end":122,"line_start":2,"line_end":2,"column_start":64,"column_end":64,"is_primary":true,"text":[{"text":"//@ compile-flags: --json=diagnostic-short --error-format=json","highlight_start":64,"highlight_end":64}],"label":"consider adding a `main` function to `$DIR/json-short.rs`","suggested_replacement":null,"suggestion_applicability":null,"expansion":null}],"children":[],"rendered":"$DIR/json-short.rs:2:64: error[E0601]: `main` function not found in crate `json_short`: consider adding a `main` function to `$DIR/json-short.rs`
"}
{"$message_type":"diagnostic","message":"aborting due to 1 previous error","code":null,"level":"error","spans":[],"children":[],"rendered":"error: aborting due to 1 previous error
"}
"#,
    )
    .replace("EXPLANATION", &explained("E0601"));
    let short = Style { short: true, ..Style::default() };
    assert_eq!(json(&[diag], &File::sources(&[&file]), short), expected);
}

// tests/ui/json/json-bom-plus-crlf.rs: byte offsets of the file as read, lines and columns of
// it without its BOM and with `\r\n` as `\n`.
#[test]
fn json_of_a_file_with_a_bom_and_crlf() {
    let lines = [
        "// (This line has BOM so it's ignored by compiletest for directives)",
        "//",
        "//@ compile-flags: --json=diagnostic-short --error-format=json",
        "//@ reference: input.byte-order-mark",
        "//@ reference: input.crlf",
        "// ignore-tidy-file-cr",
        "",
        "// For easier verifying, the byte offsets in this file should match those",
        "// in the json_bom_plus_crlf_multifile_aux.rs - given the actual fn is",
        "// identical (just with a different, but equally sized name), the easiest way",
        "// to do this is to ensure the two files are of equal size on disk.",
        "",
        "// N.B., this file needs CRLF line endings. The .gitattributes file in",
        "// this directory should enforce it.",
        "",
        "fn main() {",
        "",
        "    let s : String = 1;  // Error in the middle of line.",
        "    //~^ ERROR mismatched types",
        "",
        "    let s : String = 1",
        "    ;  // Error before the newline.",
        "    //~^^ ERROR mismatched types",
        "",
        "    let s : String =",
        "1;  // Error after the newline.",
        "    //~^ ERROR mismatched types",
        "",
        "    let s : String = (",
        "    );  // Error spanning the newline.",
        "    //~^^ ERROR mismatched types",
        "}",
    ];
    let text_of_file: String = format!("\u{feff}{}\r\n", lines.join("\r\n"));
    let file = File::new("json-bom-plus-crlf.rs", &text_of_file);
    assert_eq!(file.text.len(), 1037);
    let mismatch = |at: Span, string: Span, found: &str, suggest: bool| {
        let mut diag = Diag::error(Some("E0308"), "mismatched types", at.clone());
        diag.span_label(at.clone(), format!("expected `String`, found {found}"));
        diag.span_label(string, "expected due to this");
        if suggest {
            diag.suggestion_verbose(
                at.shrink_to_hi(),
                "try using a conversion method",
                ".to_string()",
                Applicability::MaybeIncorrect,
            );
        }
        diag
    };
    let string_at = |lo: usize| {
        let lo = lo + file.text[lo..].find("String").unwrap();
        file.at(lo, lo + "String".len())
    };
    let first = file.find("    let s : String = 1;");
    let second = file.find("    let s : String = 1\r\n");
    let third = file.find("    let s : String =\r\n");
    let fourth = file.find("    let s : String = (");
    let one = |from: usize| {
        let lo = from + file.text[from..].find('1').unwrap();
        file.at(lo, lo + 1)
    };
    let paren = file.find("(\r\n    )");
    let diags = [
        mismatch(one(first), string_at(first), "integer", true),
        mismatch(one(second), string_at(second), "integer", true),
        mismatch(one(third), string_at(third), "integer", true),
        mismatch(file.at(paren, paren + "(\r\n    )".len()), string_at(fourth), "`()`", false),
    ];
    let expected = text(
        r#"
{"$message_type":"diagnostic","message":"mismatched types","code":{"code":"E0308","explanation":EXPLANATION},"level":"error","spans":[{"file_name":"$DIR/json-bom-plus-crlf.rs","byte_start":677,"byte_end":678,"line_start":18,"line_end":18,"column_start":22,"column_end":23,"is_primary":true,"text":[{"text":"    let s : String = 1;  // Error in the middle of line.","highlight_start":22,"highlight_end":23}],"label":"expected `String`, found integer","suggested_replacement":null,"suggestion_applicability":null,"expansion":null},{"file_name":"$DIR/json-bom-plus-crlf.rs","byte_start":668,"byte_end":674,"line_start":18,"line_end":18,"column_start":13,"column_end":19,"is_primary":false,"text":[{"text":"    let s : String = 1;  // Error in the middle of line.","highlight_start":13,"highlight_end":19}],"label":"expected due to this","suggested_replacement":null,"suggestion_applicability":null,"expansion":null}],"children":[{"message":"try using a conversion method","code":null,"level":"help","spans":[{"file_name":"$DIR/json-bom-plus-crlf.rs","byte_start":678,"byte_end":678,"line_start":18,"line_end":18,"column_start":23,"column_end":23,"is_primary":true,"text":[{"text":"    let s : String = 1;  // Error in the middle of line.","highlight_start":23,"highlight_end":23}],"label":null,"suggested_replacement":".to_string()","suggestion_applicability":"MaybeIncorrect","expansion":null}],"children":[],"rendered":null}],"rendered":"$DIR/json-bom-plus-crlf.rs:18:22: error[E0308]: mismatched types: expected `String`, found integer
"}
{"$message_type":"diagnostic","message":"mismatched types","code":{"code":"E0308","explanation":EXPLANATION},"level":"error","spans":[{"file_name":"$DIR/json-bom-plus-crlf.rs","byte_start":770,"byte_end":771,"line_start":21,"line_end":21,"column_start":22,"column_end":23,"is_primary":true,"text":[{"text":"    let s : String = 1","highlight_start":22,"highlight_end":23}],"label":"expected `String`, found integer","suggested_replacement":null,"suggestion_applicability":null,"expansion":null},{"file_name":"$DIR/json-bom-plus-crlf.rs","byte_start":761,"byte_end":767,"line_start":21,"line_end":21,"column_start":13,"column_end":19,"is_primary":false,"text":[{"text":"    let s : String = 1","highlight_start":13,"highlight_end":19}],"label":"expected due to this","suggested_replacement":null,"suggestion_applicability":null,"expansion":null}],"children":[{"message":"try using a conversion method","code":null,"level":"help","spans":[{"file_name":"$DIR/json-bom-plus-crlf.rs","byte_start":771,"byte_end":771,"line_start":21,"line_end":21,"column_start":23,"column_end":23,"is_primary":true,"text":[{"text":"    let s : String = 1","highlight_start":23,"highlight_end":23}],"label":null,"suggested_replacement":".to_string()","suggestion_applicability":"MaybeIncorrect","expansion":null}],"children":[],"rendered":null}],"rendered":"$DIR/json-bom-plus-crlf.rs:21:22: error[E0308]: mismatched types: expected `String`, found integer
"}
{"$message_type":"diagnostic","message":"mismatched types","code":{"code":"E0308","explanation":EXPLANATION},"level":"error","spans":[{"file_name":"$DIR/json-bom-plus-crlf.rs","byte_start":868,"byte_end":869,"line_start":26,"line_end":26,"column_start":1,"column_end":2,"is_primary":true,"text":[{"text":"1;  // Error after the newline.","highlight_start":1,"highlight_end":2}],"label":"expected `String`, found integer","suggested_replacement":null,"suggestion_applicability":null,"expansion":null},{"file_name":"$DIR/json-bom-plus-crlf.rs","byte_start":858,"byte_end":864,"line_start":25,"line_end":25,"column_start":13,"column_end":19,"is_primary":false,"text":[{"text":"    let s : String =","highlight_start":13,"highlight_end":19}],"label":"expected due to this","suggested_replacement":null,"suggestion_applicability":null,"expansion":null}],"children":[{"message":"try using a conversion method","code":null,"level":"help","spans":[{"file_name":"$DIR/json-bom-plus-crlf.rs","byte_start":869,"byte_end":869,"line_start":26,"line_end":26,"column_start":2,"column_end":2,"is_primary":true,"text":[{"text":"1;  // Error after the newline.","highlight_start":2,"highlight_end":2}],"label":null,"suggested_replacement":".to_string()","suggestion_applicability":"MaybeIncorrect","expansion":null}],"children":[],"rendered":null}],"rendered":"$DIR/json-bom-plus-crlf.rs:26:1: error[E0308]: mismatched types: expected `String`, found integer
"}
{"$message_type":"diagnostic","message":"mismatched types","code":{"code":"E0308","explanation":EXPLANATION},"level":"error","spans":[{"file_name":"$DIR/json-bom-plus-crlf.rs","byte_start":957,"byte_end":965,"line_start":29,"line_end":30,"column_start":22,"column_end":6,"is_primary":true,"text":[{"text":"    let s : String = (","highlight_start":22,"highlight_end":23},{"text":"    );  // Error spanning the newline.","highlight_start":1,"highlight_end":6}],"label":"expected `String`, found `()`","suggested_replacement":null,"suggestion_applicability":null,"expansion":null},{"file_name":"$DIR/json-bom-plus-crlf.rs","byte_start":948,"byte_end":954,"line_start":29,"line_end":29,"column_start":13,"column_end":19,"is_primary":false,"text":[{"text":"    let s : String = (","highlight_start":13,"highlight_end":19}],"label":"expected due to this","suggested_replacement":null,"suggestion_applicability":null,"expansion":null}],"children":[],"rendered":"$DIR/json-bom-plus-crlf.rs:29:22: error[E0308]: mismatched types: expected `String`, found `()`
"}
{"$message_type":"diagnostic","message":"aborting due to 4 previous errors","code":null,"level":"error","spans":[],"children":[],"rendered":"error: aborting due to 4 previous errors
"}
"#,
    )
    .replace("EXPLANATION", &explained("E0308"));
    let short = Style { short: true, ..Style::default() };
    assert_eq!(json(&diags, &File::sources(&[&file]), short), expected);
}

/// A child with no span has none in JSON, and a failure note's line, from rustc's `json.rs`.
#[test]
fn json_of_children_without_spans_and_of_a_failure_note() {
    let file = File::new("lib.rs", "fn f() {}\n");
    let mut diag = Diag::error(Some("E0308"), "mismatched types", file.at(3, 4));
    diag.note("a note\nof two lines");
    let sources = File::sources(&[&file]);
    let line = emit::json(&diag, &sources, Style::default()).unwrap();
    let written = emit::written(&diag, &sources, Style::default()).unwrap();
    let value: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(
        value["children"],
        serde_json::json!([{
            "message": "a note\nof two lines",
            "code": null,
            "level": "note",
            "spans": [],
            "children": [],
            "rendered": null,
        }])
    );
    assert_eq!(value["code"]["explanation"], codes::explanation("E0308").unwrap());
    let keys: Vec<&str> = value.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys.len(), 7);
    assert!(line.starts_with(r#"{"$message_type":"diagnostic","message":"mismatched types","code":{"code":"E0308","explanation":"#));
    assert_eq!(value["rendered"], written.as_str());
    assert!(line.ends_with(&format!(r#""rendered":{}}}"#, serde_json::to_string(&written).unwrap())));

    let failure = &emit::summary(&[diag])[1];
    assert_eq!(
        emit::json(failure, &File::sources(&[]), Style::default()).unwrap(),
        r#"{"$message_type":"diagnostic","message":"For more information about this error, try `rustc --explain E0308`.","code":null,"level":"failure-note","spans":[],"children":[],"rendered":"For more information about this error, try `rustc --explain E0308`.\n"}"#
    );
    let short = Style { short: true, ..Style::default() };
    assert_eq!(emit::json(failure, &File::sources(&[]), short).unwrap(), "");
    assert_eq!(emit::written(failure, &File::sources(&[]), short).unwrap(), "");
    // Only the notes naming codes to explain are left out of short; another failure note is
    // written as one.
    let other = Diag::new(Level::FailureNote, None, "query stack during panic:");
    assert_eq!(emit::written(&other, &File::sources(&[]), short).unwrap(), "query stack during panic:\n");
}

/// A suggestion to remove a whole line removes its newline too, in JSON.
#[test]
fn json_of_removing_a_line_takes_its_newline() {
    let file = File::new("lib.rs", "use a;\r\nuse b;\r\n");
    let line = file.span("use a;");
    let mut diag = Diag::lint("unused_imports", "unused import: `a`", file.within("use a;", "a"));
    diag.suggestion_with_style(
        vec![(line, String::new())],
        "remove the unused import",
        Applicability::MachineApplicable,
        SuggestionStyle::HideCodeInline,
    );
    let out = emit::json(&diag, &File::sources(&[&file]), Style::default()).unwrap();
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    let removed = &value["children"][0]["spans"][0];
    assert_eq!(removed["byte_start"], 0);
    assert_eq!(removed["byte_end"], 8);
    assert_eq!(removed["line_end"], 2);
    assert_eq!(removed["column_end"], 1);
    assert_eq!(removed["text"].as_array().unwrap().len(), 2);
}

fn error(code: Option<&str>) -> Diag {
    Diag::error(code, "e", Span { file: "lib.rs".to_owned(), lo: 0, hi: 1, expansion: None })
}

fn warning() -> Diag {
    Diag::lint("unused_variables", "w", Span { file: "lib.rs".to_owned(), lo: 0, hi: 1, expansion: None })
}

fn closing(diags: &[Diag]) -> Vec<(Level, String)> {
    emit::summary(diags).into_iter().map(|it| (it.level, it.message)).collect()
}

#[test]
fn the_closing_lines_word_it_as_rustc() {
    assert_eq!(closing(&[]), []);
    assert_eq!(
        closing(&[error(Some("E0308"))]),
        [
            (Level::Error, "aborting due to 1 previous error".to_owned()),
            (
                Level::FailureNote,
                "For more information about this error, try `rustc --explain E0308`.".to_owned()
            ),
        ]
    );
    assert_eq!(
        closing(&[error(None), error(None), error(None)]),
        [(Level::Error, "aborting due to 3 previous errors".to_owned())]
    );
    assert_eq!(
        closing(&[error(None), warning(), warning()]),
        [(Level::Error, "aborting due to 1 previous error; 2 warnings emitted".to_owned())]
    );
    assert_eq!(closing(&[warning()]), [(Level::Warning, "1 warning emitted".to_owned())]);
    assert_eq!(
        closing(&[warning(), warning()]),
        [(Level::Warning, "2 warnings emitted".to_owned())]
    );
    // A note on its own is neither.
    assert_eq!(closing(&[Diag::new(Level::Note, None, "n")]), []);
}

#[test]
fn the_closing_lines_name_the_codes_with_explanations() {
    assert_eq!(
        closing(&[error(Some("E0425")), error(Some("E0308")), error(Some("E0308"))]),
        [
            (Level::Error, "aborting due to 3 previous errors".to_owned()),
            (
                Level::FailureNote,
                "Some errors have detailed explanations: E0308, E0425.".to_owned()
            ),
            (
                Level::FailureNote,
                "For more information about an error, try `rustc --explain E0308`.".to_owned()
            ),
        ]
    );
    // A code without an explanation is not named, nor a lint's name.
    assert_eq!(
        closing(&[error(Some("E9999")), warning()]),
        [(Level::Error, "aborting due to 1 previous error; 1 warning emitted".to_owned())]
    );
    assert_eq!(
        closing(&[error(Some("E9999")), error(Some("E0599"))])[1].1,
        "For more information about this error, try `rustc --explain E0599`."
    );
    let ten = [
        "E0599", "E0308", "E0425", "E0277", "E0061", "E0106", "E0382", "E0499", "E0502", "E0433",
    ];
    let diags: Vec<Diag> = ten.iter().map(|it| error(Some(it))).collect();
    assert_eq!(
        closing(&diags)[1].1,
        "Some errors have detailed explanations: E0061, E0106, E0277, E0308, E0382, E0425, E0433, E0499, E0502..."
    );
}

#[test]
fn the_closing_lines_are_written_as_rustc_writes_them() {
    let diags = [error(Some("E0308"))];
    let closing = emit::summary(&diags);
    let sources = File::sources(&[]);
    let written: Vec<String> =
        closing.iter().map(|it| emit::written(it, &sources, Style::default()).unwrap()).collect();
    assert_eq!(
        written,
        [
            "error: aborting due to 1 previous error\n\n",
            "For more information about this error, try `rustc --explain E0308`.\n"
        ]
    );
    let short = Style { short: true, ..Style::default() };
    let written: Vec<String> =
        closing.iter().map(|it| emit::written(it, &sources, short).unwrap()).collect();
    assert_eq!(written, ["error: aborting due to 1 previous error\n", ""]);
}

#[test]
fn rendered_is_written_without_its_empty_line() {
    let file = File::new("lib.rs", "fn f() {}\n");
    let diag = Diag::error(Some("E0308"), "mismatched types", file.at(3, 4));
    let sources = File::sources(&[&file]);
    let rendered = emit::rendered(&diag, &sources, Style::default()).unwrap();
    assert_eq!(rendered, "error[E0308]: mismatched types\n --> $DIR/lib.rs:1:4\n  |\n1 | fn f() {}\n  |    ^");
    assert_eq!(emit::written(&diag, &sources, Style::default()).unwrap(), format!("{rendered}\n\n"));
}

#[test]
fn ansi_runs_are_normalized_as_the_ui_test_says() {
    assert_eq!(ansi(r"\u001b[1m\u001b[33mw\u001b[0m: x"), "[ANSI]w[ANSI]: x");
}

#[test]
fn a_span_of_a_file_not_read_is_an_error_of_every_writer_naming_the_file() {
    let file = File::new("out/gen.rs", "let unused = 1;\n");
    let diag = Diag::error(None, "unused variable: `unused`", file.span("unused"));
    let mut sources = Sources::default();
    sources.add(&file.name, None);
    let unread = emit::Unplaced::Unread { file: file.name.clone(), byte: 4 };
    assert_eq!(emit::written(&diag, &sources, Style::default()), Err(unread.clone()));
    assert_eq!(emit::rendered(&diag, &sources, Style::default()), Err(unread.clone()));
    assert_eq!(emit::json(&diag, &sources, Style::default()), Err(unread.clone()));
    assert_eq!(emit::json_rendered(&diag, &sources, String::new()), Err(unread));
    assert!(emit::written(&diag, &File::sources(&[&file]), Style::default()).is_ok());
}
