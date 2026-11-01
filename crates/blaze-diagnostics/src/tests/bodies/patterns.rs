//! rustc's UI tests of the errors of patterns type checking gives (`rustc_hir_typeck/src/pat.rs`):
//! slice and array patterns, range patterns, tuple struct patterns' field counts, `&` patterns of
//! trait objects, union patterns, `#[non_exhaustive]` structs and the binding modes of RFC 3627.
//! A test whose file has other kinds' errors too keeps only this kind's diagnostics; a file that
//! needs what minicore lacks (`String`, `Vec`, `Box`) has it added after its last line, which no
//! diagnostic shows.

use crate::{
    Options,
    emit::{Style, written},
    model::Diag,
    tests::{Mode, assert_reports, checked_with, compiletest, error, normalized, ui},
};

/// [`ui`] of a fixture of several files or crates, or with minicore: each file named
/// `$DIR/<path>`, run as compiletest runs a test that is not `//@ run-pass` (none here is, in
/// the revisions taken).
fn ui_fixture(fixture: &str) -> String {
    let stderr = checked_with(fixture, "$DIR/", &compiletest(Mode::Other, &Options::default()))
        .stderr_with(ui_testing());
    normalized(&stderr)
}

/// [`ui_fixture`] with only the diagnostics `keep` takes, without the closing ones, ending at the
/// last one's text: the empty line rustc writes after it comes before the ones left out.
fn ui_only(fixture: &str, keep: impl Fn(&Diag) -> bool) -> String {
    let checked = checked_with(fixture, "$DIR/", &compiletest(Mode::Other, &Options::default()));
    let sources = checked.sources();
    let mut stderr: String = checked
        .diags
        .iter()
        .filter(|it| keep(it))
        .map(|it| written(it, &sources, ui_testing()).unwrap())
        .collect();
    if stderr.ends_with("\n\n") {
        stderr.pop();
    }
    normalized(&stderr)
}

fn ui_testing() -> Style {
    Style { ui_testing: true, ..Style::default() }
}

fn code(code: &str) -> impl Fn(&Diag) -> bool + '_ {
    move |it| it.code.as_ref().is_some_and(|it| it.as_str() == code)
}

fn message<'a>(messages: &'a [&'a str]) -> impl Fn(&Diag) -> bool + 'a {
    move |it| messages.contains(&it.message.as_str())
}

/// `Vec` and `Box` as alloc's, for a file that adds `extern crate alloc;` and its `use`.
const ALLOC: &str = "//- /alloc.rs crate:alloc
pub mod vec {
    pub struct Vec<T>(*mut T);
    impl<T> core::ops::Deref for Vec<T> {
        type Target = [T];
        fn deref(&self) -> &[T] {
            loop {}
        }
    }
}
pub mod boxed {
    pub struct Box<T: ?Sized>(*mut T);
    impl<T: ?Sized> core::ops::Deref for Box<T> {
        type Target = T;
        fn deref(&self) -> &T {
            loop {}
        }
    }
}
";

#[test]
fn an_array_of_a_generic_length_cannot_be_matched() {
    // tests/ui/error-codes/E0730.rs
    let source = r#"fn is_123<const N: usize>(x: [u32; N]) -> bool {
    match x {
        [1, 2, ..] => true, //~ ERROR cannot pattern-match on an array without a fixed length
        _ => false
    }
}

fn main() {}
"#;
    let stderr = "error[E0730]: cannot pattern-match on an array without a fixed length
  --> $DIR/E0730.rs:3:9
   |
LL |         [1, 2, ..] => true,
   |         ^^^^^^^^^^

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0730`.
";
    assert_eq!(ui("E0730.rs", "2015", source), stderr);
}

#[test]
fn a_slice_pattern_of_a_float_is_refused() {
    // tests/ui/error-codes/E0529.rs
    let source = "fn main() {
    let r: f32 = 1.0;
    match r {
        [a, b] => {
        //~^ ERROR E0529
        }
    }
}
";
    let stderr = "error[E0529]: expected an array or slice, found `f32`
  --> $DIR/E0529.rs:4:9
   |
LL |         [a, b] => {
   |         ^^^^^^ pattern cannot match with input type `f32`

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0529`.
";
    assert_eq!(ui("E0529.rs", "2015", source), stderr);
}

#[test]
fn a_slice_pattern_of_unit_is_refused() {
    // tests/ui/match/match-vec-mismatch-2.rs
    let source = "fn main() {
    match () {
        [()] => { }
        //~^ ERROR expected an array or slice, found `()`
    }
}
";
    let stderr = "error[E0529]: expected an array or slice, found `()`
  --> $DIR/match-vec-mismatch-2.rs:3:9
   |
LL |         [()] => { }
   |         ^^^^ pattern cannot match with input type `()`

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0529`.
";
    assert_eq!(ui("match-vec-mismatch-2.rs", "2015", source), stderr);
}

#[test]
fn a_slice_pattern_of_an_option_of_a_vec_suggests_as_deref_or_slicing() {
    // tests/ui/typeck/issue-91328.rs
    let fixture = format!(
        "//- minicore: option, result, deref, add
//- /issue-91328.rs crate:other:main edition:2015 deps:alloc
// Regression test for issue #91328.

//@ run-rustfix

#![allow(dead_code)]

fn foo(r: Result<Vec<i32>, i32>) -> i32 {{
    match r {{
    //~^ HELP: consider using `as_deref` here
        Ok([a, b]) => a + b,
        //~^ ERROR: expected an array or slice
        //~| NOTE: pattern cannot match with input type
        _ => 42,
    }}
}}

fn bar(o: Option<Vec<i32>>) -> i32 {{
    match o {{
    //~^ HELP: consider using `as_deref` here
        Some([a, b]) => a + b,
        //~^ ERROR: expected an array or slice
        //~| NOTE: pattern cannot match with input type
        _ => 42,
    }}
}}

fn baz(v: Vec<i32>) -> i32 {{
    match v {{
    //~^ HELP: consider slicing here
        [a, b] => a + b,
        //~^ ERROR: expected an array or slice
        //~| NOTE: pattern cannot match with input type
        _ => 42,
    }}
}}

fn qux(a: &Option<Box<[i32; 2]>>) -> i32 {{
    match a {{
    //~^ HELP: consider using `as_deref` here
        Some([a, b]) => a + b,
        //~^ ERROR: expected an array or slice
        //~| NOTE: pattern cannot match with input type
        _ => 42,
    }}
}}

fn main() {{}}
extern crate alloc;
use alloc::{{boxed::Box, vec::Vec}};
{ALLOC}"
    );
    let stderr = "error[E0529]: expected an array or slice, found `Vec<i32>`
  --> $DIR/issue-91328.rs:10:12
   |
LL |         Ok([a, b]) => a + b,
   |            ^^^^^^ pattern cannot match with input type `Vec<i32>`
   |
help: consider using `as_deref` here
   |
LL |     match r.as_deref() {
   |            +++++++++++

error[E0529]: expected an array or slice, found `Vec<i32>`
  --> $DIR/issue-91328.rs:20:14
   |
LL |         Some([a, b]) => a + b,
   |              ^^^^^^ pattern cannot match with input type `Vec<i32>`
   |
help: consider using `as_deref` here
   |
LL |     match o.as_deref() {
   |            +++++++++++

error[E0529]: expected an array or slice, found `Vec<i32>`
  --> $DIR/issue-91328.rs:30:9
   |
LL |         [a, b] => a + b,
   |         ^^^^^^ pattern cannot match with input type `Vec<i32>`
   |
help: consider slicing here
   |
LL |     match v[..] {
   |            ++++

error[E0529]: expected an array or slice, found `Box<[i32; 2]>`
  --> $DIR/issue-91328.rs:40:14
   |
LL |         Some([a, b]) => a + b,
   |              ^^^^^^ pattern cannot match with input type `Box<[i32; 2]>`
   |
help: consider using `as_deref` here
   |
LL |     match a.as_deref() {
   |            +++++++++++

error: aborting due to 4 previous errors

For more information about this error, try `rustc --explain E0529`.
";
    assert_eq!(ui_fixture(&fixture), stderr);
}

#[test]
fn slicing_is_suggested_only_for_the_whole_pattern() {
    // tests/ui/suggestions/suppress-consider-slicing-issue-120605.rs
    let fixture = format!(
        r#"//- minicore: deref
//- /suppress-consider-slicing-issue-120605.rs crate:other:main edition:2015 deps:alloc
pub struct Struct {{
    a: Vec<Struct>,
}}

impl Struct {{
    pub fn test(&self) {{
        if let [Struct {{ a: [] }}] = &self.a {{
            //~^ ERROR expected an array or slice
            //~| ERROR expected an array or slice
            println!("matches!")
        }}

        if let [Struct {{ a: [] }}] = &self.a[..] {{
            //~^ ERROR expected an array or slice
            println!("matches!")
        }}
    }}
}}

fn main() {{}}
extern crate alloc;
use alloc::vec::Vec;
{ALLOC}"#
    );
    let stderr = "error[E0529]: expected an array or slice, found `Vec<Struct>`
  --> $DIR/suppress-consider-slicing-issue-120605.rs:7:16
   |
LL |         if let [Struct { a: [] }] = &self.a {
   |                ^^^^^^^^^^^^^^^^^^ pattern cannot match with input type `Vec<Struct>`
   |
help: consider slicing here
   |
LL |         if let [Struct { a: [] }] = &self.a[..] {
   |                                            ++++

error[E0529]: expected an array or slice, found `Vec<Struct>`
  --> $DIR/suppress-consider-slicing-issue-120605.rs:7:29
   |
LL |         if let [Struct { a: [] }] = &self.a {
   |                             ^^ pattern cannot match with input type `Vec<Struct>`

error[E0529]: expected an array or slice, found `Vec<Struct>`
  --> $DIR/suppress-consider-slicing-issue-120605.rs:13:29
   |
LL |         if let [Struct { a: [] }] = &self.a[..] {
   |                             ^^ pattern cannot match with input type `Vec<Struct>`
";
    assert_eq!(ui_only(&fixture, code("E0529")), stderr);
}

#[test]
fn a_range_of_strings_labels_both_ends() {
    // tests/ui/error-codes/E0029.rs
    let source = r#"fn main() {
    let s = "hoho";

    match s {
        "hello" ..= "world" => {}
        //~^ ERROR only `char` and numeric types are allowed in range patterns
        _ => {}
    }
}
"#;
    let stderr = "error[E0029]: only `char` and numeric types are allowed in range patterns
  --> $DIR/E0029.rs:5:9
   |
LL |         \"hello\" ..= \"world\" => {}
   |         -------^^^^^-------
   |         |           |
   |         |           this is of type `&'static str` but it should be `char` or numeric
   |         this is of type `&'static str` but it should be `char` or numeric

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0029`.
";
    assert_eq!(ui("E0029.rs", "2015", source), stderr);
}

#[test]
fn a_half_open_range_of_a_string_points_at_its_end() {
    // tests/ui/half-open-range-patterns/half-open-range-pats-bad-types.rs
    let source = r#"fn main() {
    let "a".. = "a"; //~ ERROR only `char` and numeric types are allowed in range patterns
    let .."a" = "a"; //~ ERROR only `char` and numeric types are allowed in range patterns
    let ..="a" = "a"; //~ ERROR only `char` and numeric types are allowed in range patterns
}
"#;
    let stderr = "error[E0029]: only `char` and numeric types are allowed in range patterns
  --> $DIR/half-open-range-pats-bad-types.rs:2:9
   |
LL |     let \"a\".. = \"a\";
   |         ^^^ this is of type `&'static str` but it should be `char` or numeric

error[E0029]: only `char` and numeric types are allowed in range patterns
  --> $DIR/half-open-range-pats-bad-types.rs:3:11
   |
LL |     let ..\"a\" = \"a\";
   |           ^^^ this is of type `&'static str` but it should be `char` or numeric

error[E0029]: only `char` and numeric types are allowed in range patterns
  --> $DIR/half-open-range-pats-bad-types.rs:4:12
   |
LL |     let ..=\"a\" = \"a\";
   |            ^^^ this is of type `&'static str` but it should be `char` or numeric

error: aborting due to 3 previous errors

For more information about this error, try `rustc --explain E0029`.
";
    assert_eq!(ui("half-open-range-pats-bad-types.rs", "2015", source), stderr);
}

#[test]
fn a_range_with_one_bad_end_names_the_type_of_the_other() {
    // tests/ui/match/match-range-fail.rs
    let source = r#"fn main() {
    match "wow" {
        "bar" ..= "foo" => { }
    };
    //~^^ ERROR only `char` and numeric types are allowed in range

    match "wow" {
        10 ..= "what" => ()
    };
    //~^^ ERROR only `char` and numeric types are allowed in range

    match "wow" {
        true ..= "what" => {}
    };
    //~^^ ERROR only `char` and numeric types are allowed in range

    match 5 {
        'c' ..= 100 => { }
        _ => { }
    };
    //~^^^ ERROR mismatched types
}
"#;
    let stderr = "error[E0029]: only `char` and numeric types are allowed in range patterns
  --> $DIR/match-range-fail.rs:3:9
   |
LL |         \"bar\" ..= \"foo\" => { }
   |         -----^^^^^-----
   |         |         |
   |         |         this is of type `&'static str` but it should be `char` or numeric
   |         this is of type `&'static str` but it should be `char` or numeric

error[E0029]: only `char` and numeric types are allowed in range patterns
  --> $DIR/match-range-fail.rs:8:16
   |
LL |         10 ..= \"what\" => ()
   |         --     ^^^^^^ this is of type `&'static str` but it should be `char` or numeric
   |         |
   |         this is of type `{integer}`

error[E0029]: only `char` and numeric types are allowed in range patterns
  --> $DIR/match-range-fail.rs:13:9
   |
LL |         true ..= \"what\" => {}
   |         ----^^^^^------
   |         |        |
   |         |        this is of type `&'static str` but it should be `char` or numeric
   |         this is of type `bool` but it should be `char` or numeric
";
    let fixture = format!("//- /match-range-fail.rs crate:other:main edition:2015\n{source}");
    assert_eq!(ui_only(&fixture, code("E0029")), stderr);
}

#[test]
fn an_array_pattern_of_too_few_elements_is_refused() {
    // tests/ui/pattern/array-length-mismatch-13482.rs
    let fixture = "//- minicore: option
//- /array-length-mismatch-13482.rs crate:other:main edition:2015
//! Regression test for https://github.com/rust-lang/rust/issues/13482

fn main() {
  let x = [1,2];
  let y = match x {
    [] => None, //~ ERROR pattern requires 0 elements but array has 2
    [a,_] => Some(a)
  };
}
";
    let stderr = "error[E0527]: pattern requires 0 elements but array has 2
  --> $DIR/array-length-mismatch-13482.rs:6:5
   |
LL |     [] => None,
   |     ^^ expected 2 elements

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0527`.
";
    assert_eq!(ui_fixture(fixture), stderr);
}

#[test]
fn an_array_destructuring_assignment_of_the_wrong_length_is_refused() {
    // tests/ui/destructuring-assignment/slice_destructure_fail.rs
    let fixture = "//- /slice_destructure_fail.rs crate:other:main edition:2015
fn main() {
    let (mut a, mut b);
    [a, .., b, ..] = [0, 1]; //~ ERROR `..` can only be used once per slice pattern
    [a, a, b] = [1, 2];
    //~^ ERROR pattern requires 3 elements but array has 2
    [_] = [1, 2];
    //~^ ERROR pattern requires 1 element but array has 2
}
";
    let stderr = "error[E0527]: pattern requires 3 elements but array has 2
  --> $DIR/slice_destructure_fail.rs:4:5
   |
LL |     [a, a, b] = [1, 2];
   |     ^^^^^^^^^ expected 2 elements

error[E0527]: pattern requires 1 element but array has 2
  --> $DIR/slice_destructure_fail.rs:6:5
   |
LL |     [_] = [1, 2];
   |     ^^^ expected 2 elements
";
    assert_eq!(ui_only(fixture, code("E0527")), stderr);
}

#[test]
fn an_array_pattern_with_a_rest_of_too_many_elements_is_refused() {
    // tests/ui/error-codes/E0528.rs
    let source = "fn main() {
    let r = &[1, 2];
    match r {
        &[a, b, c, rest @ ..] => {
        //~^ ERROR E0528
        }
    }
}
";
    let stderr = "error[E0528]: pattern requires at least 3 elements but array has 2
  --> $DIR/E0528.rs:4:10
   |
LL |         &[a, b, c, rest @ ..] => {
   |          ^^^^^^^^^^^^^^^^^^^^ pattern cannot match array of 2 elements

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0528`.
";
    assert_eq!(ui("E0528.rs", "2015", source), stderr);
}

#[test]
fn a_tuple_variant_pattern_of_the_wrong_field_count_says_how_to_fix_it() {
    // tests/ui/error-codes/E0023.rs
    let fixture = "//- /E0023.rs crate:other:main edition:2015
enum Fruit {
    Apple(String, String),
    Pear(u32),
    Orange((String, String)),
    Banana(()),
}

fn main() {
    let x = Fruit::Apple(String::new(), String::new());
    match x {
        Fruit::Apple(a) => {}, //~ ERROR E0023
        Fruit::Apple(a, b, c) => {}, //~ ERROR E0023
        Fruit::Pear(1, 2) => {}, //~ ERROR E0023
        Fruit::Orange(a, b) => {}, //~ ERROR E0023
        Fruit::Banana() => {}, //~ ERROR E0023
    }
}
struct String;
impl String {
    fn new() -> String {
        String
    }
}
";
    assert_reports(
        &ui_only(fixture, code("E0023")),
        &[
            error(11, &[]),
            error(12, &[]),
            error(13, &[]),
            error(14, &[]),
            error(15, &[]),
        ],
    );
}

#[test]
fn a_tuple_variant_pattern_of_wildcards_suggests_a_rest() {
    // tests/ui/match/match-pattern-field-mismatch.rs
    let source = "fn main() {
    enum Color {
        Rgb(usize, usize, usize),
        Cmyk(usize, usize, usize, usize),
        NoColor,
    }

    fn foo(c: Color) {
        match c {
          Color::Rgb(_, _) => { }
          //~^ ERROR this pattern has 2 fields, but the corresponding tuple variant has 3
          Color::Cmyk(_, _, _, _) => { }
          Color::NoColor => { }
        }
    }
}
";
    let stderr = "error[E0023]: this pattern has 2 fields, but the corresponding tuple variant has 3 fields
  --> $DIR/match-pattern-field-mismatch.rs:10:22
   |
LL |         Rgb(usize, usize, usize),
   |             -----  -----  ----- tuple variant has 3 fields
...
LL |           Color::Rgb(_, _) => { }
   |                      ^  ^ expected 3 fields, found 2
   |
help: use `_` to explicitly ignore each field
   |
LL |           Color::Rgb(_, _, _) => { }
   |                          +++
help: use `..` to ignore all fields
   |
LL -           Color::Rgb(_, _) => { }
LL +           Color::Rgb(..) => { }
   |

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0023`.
";
    assert_eq!(ui("match-pattern-field-mismatch.rs", "2015", source), stderr);
}

#[test]
fn an_empty_tuple_struct_pattern_of_another_type_suggests_no_parentheses() {
    // tests/ui/pattern/issue-67037-pat-tup-scrut-ty-diff-less-fields.rs
    let fixture = "//- /issue-67037-pat-tup-scrut-ty-diff-less-fields.rs crate:other:main edition:2015
// Regression test for #67037.
//
// In type checking patterns, E0023 occurs when the tuple pattern and the expected
// tuple pattern have different number of fields. For example, as below, `P()`,
// the tuple struct pattern, has 0 fields, but requires 1 field.
//
// In emitting E0023, we try to see if this is a case of e.g., `Some(a, b, c)` but where
// the scrutinee was of type `Some((a, b, c))`, and suggest that parentheses be added.
//
// However, we did not account for the expected type being different than the tuple pattern type.
// This caused an issue when the tuple pattern type (`P<T>`) was generic.
// Specifically, we tried deriving the 0th field's type using the `substs` of the expected type.
// When attempting to substitute `T`, there was no such substitution, so \"out of range\" occurred.

struct U {} // 0 type parameters offered
struct P<T>(T); // 1 type parameter wanted

fn main() {
    let P() = U {}; //~ ERROR mismatched types
    //~^ ERROR this pattern has 0 fields, but the corresponding tuple struct has 1 field
}
";
    let stderr = "error[E0023]: this pattern has 0 fields, but the corresponding tuple struct has 1 field
  --> $DIR/issue-67037-pat-tup-scrut-ty-diff-less-fields.rs:19:9
   |
LL | struct P<T>(T); // 1 type parameter wanted
   |             - tuple struct has 1 field
...
LL |     let P() = U {};
   |         ^^^ expected 1 field, found 0
   |
help: use `_` to explicitly ignore each field
   |
LL |     let P(_) = U {};
   |           +
help: use `..` to ignore all fields
   |
LL |     let P(..) = U {};
   |           ++
";
    assert_eq!(ui_only(fixture, code("E0023")), stderr);
}

#[test]
fn a_tuple_struct_pattern_counts_a_binding_of_a_rest_as_a_field() {
    // tests/ui/pattern/issue-72574-2.rs
    let fixture = "//- /issue-72574-2.rs crate:other:main edition:2015
struct Binder(i32, i32, i32);

fn main() {
    let x = Binder(1, 2, 3);
    match x {
        Binder(_a, _x @ ..) => {}
        _ => {}
    }
}
//~^^^^ ERROR `_x @` is not allowed in a tuple struct
//~| ERROR: `..` patterns are not allowed here
//~| ERROR: this pattern has 2 fields, but the corresponding tuple struct has 3 fields
";
    let stderr = "error[E0023]: this pattern has 2 fields, but the corresponding tuple struct has 3 fields
  --> $DIR/issue-72574-2.rs:6:16
   |
LL | struct Binder(i32, i32, i32);
   |               ---  ---  --- tuple struct has 3 fields
...
LL |         Binder(_a, _x @ ..) => {}
   |                ^^  ^^^^^^^ expected 3 fields, found 2
   |
help: use `_` to explicitly ignore each field
   |
LL |         Binder(_a, _x @ .., _) => {}
   |                           +++
";
    assert_eq!(ui_only(fixture, code("E0023")), stderr);
}

#[test]
fn a_tuple_struct_destructuring_assignment_of_the_wrong_field_count_is_refused() {
    // tests/ui/destructuring-assignment/tuple_struct_destructure_fail.rs
    let fixture = "//- /tuple_struct_destructure_fail.rs crate:other:main edition:2015
struct TupleStruct<S, T>(S, T);

enum Enum<S, T> {
    SingleVariant(S, T)
}

type Alias<S> = Enum<S, isize>;

trait Test {
    fn test() -> TupleStruct<isize, isize> {
        TupleStruct(0, 0)
    }
}

impl Test for Alias<isize> {}

fn test() -> TupleStruct<isize, isize> {
    TupleStruct(0, 0)
}

fn main() {
    let (mut a, mut b);
    TupleStruct(a, .., b, ..) = TupleStruct(0, 1);
    //~^ ERROR `..` can only be used once per tuple struct or variant pattern
    Enum::SingleVariant(a, .., b, ..) = Enum::SingleVariant(0, 1);
    //~^ ERROR `..` can only be used once per tuple struct or variant pattern

    TupleStruct(a, a, b) = TupleStruct(1, 2);
    //~^ ERROR this pattern has 3 fields, but the corresponding tuple struct has 2 fields
    TupleStruct(_) = TupleStruct(1, 2);
    //~^ ERROR this pattern has 1 field, but the corresponding tuple struct has 2 fields
    Enum::SingleVariant(a, a, b) = Enum::SingleVariant(1, 2);
    //~^ ERROR this pattern has 3 fields, but the corresponding tuple variant has 2 fields
    Enum::SingleVariant(_) = Enum::SingleVariant(1, 2);
    //~^ ERROR this pattern has 1 field, but the corresponding tuple variant has 2 fields

    // Check if `test` is recognized as not a tuple struct but a function call:
    test() = TupleStruct(0, 0);
    //~^ ERROR invalid left-hand side of assignment
    (test)() = TupleStruct(0, 0);
    //~^ ERROR invalid left-hand side of assignment
    <Alias::<isize> as Test>::test() = TupleStruct(0, 0);
    //~^ ERROR invalid left-hand side of assignment
}
";
    let stderr = "error[E0023]: this pattern has 3 fields, but the corresponding tuple struct has 2 fields
  --> $DIR/tuple_struct_destructure_fail.rs:28:17
   |
LL | struct TupleStruct<S, T>(S, T);
   |                          -  - tuple struct has 2 fields
...
LL |     TupleStruct(a, a, b) = TupleStruct(1, 2);
   |                 ^  ^  ^ expected 2 fields, found 3

error[E0023]: this pattern has 1 field, but the corresponding tuple struct has 2 fields
  --> $DIR/tuple_struct_destructure_fail.rs:30:17
   |
LL | struct TupleStruct<S, T>(S, T);
   |                          -  - tuple struct has 2 fields
...
LL |     TupleStruct(_) = TupleStruct(1, 2);
   |                 ^ expected 2 fields, found 1
   |
help: use `_` to explicitly ignore each field
   |
LL |     TupleStruct(_, _) = TupleStruct(1, 2);
   |                  +++
help: use `..` to ignore all fields
   |
LL -     TupleStruct(_) = TupleStruct(1, 2);
LL +     TupleStruct(..) = TupleStruct(1, 2);
   |

error[E0023]: this pattern has 3 fields, but the corresponding tuple variant has 2 fields
  --> $DIR/tuple_struct_destructure_fail.rs:32:25
   |
LL |     SingleVariant(S, T)
   |                   -  - tuple variant has 2 fields
...
LL |     Enum::SingleVariant(a, a, b) = Enum::SingleVariant(1, 2);
   |                         ^  ^  ^ expected 2 fields, found 3

error[E0023]: this pattern has 1 field, but the corresponding tuple variant has 2 fields
  --> $DIR/tuple_struct_destructure_fail.rs:34:25
   |
LL |     SingleVariant(S, T)
   |                   -  - tuple variant has 2 fields
...
LL |     Enum::SingleVariant(_) = Enum::SingleVariant(1, 2);
   |                         ^ expected 2 fields, found 1
   |
help: use `_` to explicitly ignore each field
   |
LL |     Enum::SingleVariant(_, _) = Enum::SingleVariant(1, 2);
   |                          +++
help: use `..` to ignore all fields
   |
LL -     Enum::SingleVariant(_) = Enum::SingleVariant(1, 2);
LL +     Enum::SingleVariant(..) = Enum::SingleVariant(1, 2);
   |
";
    assert_eq!(ui_only(fixture, code("E0023")), stderr);
}

#[test]
fn a_reference_pattern_of_a_trait_object_cannot_dereference_it() {
    // tests/ui/error-codes/E0033.rs
    // `dispatch_from_dyn` makes `&self` dispatchable, so `SomeTrait` is dyn-compatible.
    let fixture = "//- minicore: coerce_unsized, dispatch_from_dyn
//- /E0033.rs crate:other:main edition:2015
trait SomeTrait {
    fn foo(&self);
}
struct S;
impl SomeTrait for S {
    fn foo(&self) {}
}
fn main() {
    let trait_obj: &dyn SomeTrait = &S;

    let &invalid = trait_obj;
    //~^ ERROR E0033
}
";
    assert_reports(&ui_fixture(fixture), &[error(11, &["`&dyn SomeTrait`"])]);
}

#[test]
fn a_union_pattern_names_exactly_one_field_and_no_rest() {
    // tests/ui/union/union-fields-2.rs
    let fixture = "//- /union-fields-2.rs crate:other:main edition:2015
union U {
    a: u8,
    b: u16,
}

fn main() {
    let u = U {}; //~ ERROR union expressions should have exactly one field
    let u = U { a: 0 }; // OK
    let u = U { a: 0, b: 1 }; //~ ERROR union expressions should have exactly one field
    let u = U { a: 0, b: 1, c: 2 }; //~ ERROR union expressions should have exactly one field
                                    //~^ ERROR union `U` has no field named `c`
    let u = U { ..u }; //~ ERROR union expressions should have exactly one field
                       //~^ ERROR functional record update syntax requires a struct

    let U {} = u; //~ ERROR union patterns should have exactly one field
    let U { a } = u; // OK
    let U { a, b } = u; //~ ERROR union patterns should have exactly one field
    let U { a, b, c } = u; //~ ERROR union patterns should have exactly one field
                           //~^ ERROR union `U` does not have a field named `c`
    let U { .. } = u; //~ ERROR union patterns should have exactly one field
                      //~^ ERROR `..` cannot be used in union patterns
    let U { a, .. } = u; //~ ERROR `..` cannot be used in union patterns
}
";
    let stderr = "error: union patterns should have exactly one field
  --> $DIR/union-fields-2.rs:15:9
   |
LL |     let U {} = u;
   |         ^^^^

error: union patterns should have exactly one field
  --> $DIR/union-fields-2.rs:17:9
   |
LL |     let U { a, b } = u;
   |         ^^^^^^^^^^

error: union patterns should have exactly one field
  --> $DIR/union-fields-2.rs:18:9
   |
LL |     let U { a, b, c } = u;
   |         ^^^^^^^^^^^^^

error: union patterns should have exactly one field
  --> $DIR/union-fields-2.rs:20:9
   |
LL |     let U { .. } = u;
   |         ^^^^^^^^

error: `..` cannot be used in union patterns
  --> $DIR/union-fields-2.rs:20:9
   |
LL |     let U { .. } = u;
   |         ^^^^^^^^

error: `..` cannot be used in union patterns
  --> $DIR/union-fields-2.rs:22:9
   |
LL |     let U { a, .. } = u;
   |         ^^^^^^^^^^^
";
    let union = message(&[
        "union patterns should have exactly one field",
        "`..` cannot be used in union patterns",
    ]);
    assert_eq!(ui_only(fixture, union), stderr);
}

#[test]
fn a_struct_pattern_of_another_crates_non_exhaustive_struct_needs_a_rest() {
    // tests/ui/rfcs/rfc-2008-non-exhaustive/struct.rs, with auxiliary/structs.rs
    let fixture = "//- minicore: default, derive, fmt
//- /struct.rs crate:other:main edition:2015 deps:structs
//@ aux-build:structs.rs
extern crate structs;

use structs::{NormalStruct, UnitStruct, TupleStruct, FunctionalRecord};

fn main() {
    let fr = FunctionalRecord {
    //~^ ERROR cannot create non-exhaustive struct
        first_field: 1920,
        second_field: 1080,
        ..FunctionalRecord::default()
    };

    let ns = NormalStruct { first_field: 640, second_field: 480 };
    //~^ ERROR cannot create non-exhaustive struct

    let NormalStruct { first_field, second_field } = ns;
    //~^ ERROR `..` required with struct marked as non-exhaustive

    let ts = TupleStruct(640, 480);
    //~^ ERROR cannot initialize a tuple struct which contains private fields [E0423]

    let ts_explicit = structs::TupleStruct(640, 480);
    //~^ ERROR tuple struct constructor `TupleStruct` is private [E0603]

    let TupleStruct { 0: first_field, 1: second_field } = ts;
    //~^ ERROR `..` required with struct marked as non-exhaustive

    let us = UnitStruct;
    //~^ ERROR cannot find value `UnitStruct` in this scope [E0423]

    let us_explicit = structs::UnitStruct;
    //~^ ERROR unit struct `UnitStruct` is private [E0603]

    let UnitStruct { } = us;
    //~^ ERROR `..` required with struct marked as non-exhaustive
}

// Everything below this is expected to compile successfully.

// We only test matching here as we cannot create non-exhaustive
// structs from another crate. i.e. they'll never pass in run-pass tests.
fn match_structs(ns: NormalStruct, ts: TupleStruct, us: UnitStruct) {
    let NormalStruct { first_field, second_field, .. } = ns;

    let TupleStruct { 0: first, 1: second, .. } = ts;

    let UnitStruct { .. } = us;
}
//- /auxiliary/structs.rs crate:structs edition:2015
#[derive(Default)]
#[non_exhaustive]
pub struct NormalStruct {
    pub first_field: u16,
    pub second_field: u16,
}

#[non_exhaustive]
pub struct UnitStruct;

#[non_exhaustive]
pub struct TupleStruct(pub u16, pub u16);

#[derive(Debug)]
#[non_exhaustive]
pub struct FunctionalRecord {
    pub first_field: u16,
    pub second_field: u16,
    pub third_field: bool,
}

impl Default for FunctionalRecord {
    fn default() -> FunctionalRecord {
        FunctionalRecord { first_field: 640, second_field: 480, third_field: false }
    }
}

#[derive(Default)]
#[non_exhaustive]
pub struct NestedStruct {
    pub foo: u16,
    pub bar: NormalStruct,
}

#[derive(Default)]
#[non_exhaustive]
pub struct MixedVisFields {
    pub a: u16,
    pub b: bool,
    pub(crate) foo: bool,
}
";
    let stderr = "error[E0638]: `..` required with struct marked as non-exhaustive
  --> $DIR/struct.rs:17:9
   |
LL |     let NormalStruct { first_field, second_field } = ns;
   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
help: add `..` at the end of the field list to ignore all other fields
   |
LL |     let NormalStruct { first_field, second_field , .. } = ns;
   |                                                  ++++

error[E0638]: `..` required with struct marked as non-exhaustive
  --> $DIR/struct.rs:26:9
   |
LL |     let TupleStruct { 0: first_field, 1: second_field } = ts;
   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
help: add `..` at the end of the field list to ignore all other fields
   |
LL |     let TupleStruct { 0: first_field, 1: second_field , .. } = ts;
   |                                                       ++++

error[E0638]: `..` required with struct marked as non-exhaustive
  --> $DIR/struct.rs:35:9
   |
LL |     let UnitStruct { } = us;
   |         ^^^^^^^^^^^^^^
   |
help: add `..` at the end of the field list to ignore all other fields
   |
LL |     let UnitStruct { .. } = us;
   |                      ++
";
    assert_eq!(ui_only(fixture, code("E0638")), stderr);
}

/// tests/ui/pattern/rfc-3627-match-ergonomics-2024/experimental/mut-ref-mut.rs
const MUT_REF_MUT: &str = "//@ revisions: stable2021 classic2021 structural2021 classic2024 structural2024
//@[stable2021] edition: 2021
//@[classic2021] edition: 2021
//@[structural2021] edition: 2021
//@[classic2024] edition: 2024
//@[structural2024] edition: 2024
//@[stable2021] run-pass
//@[classic2021] run-pass
//@[structural2021] run-pass
//@ dont-require-annotations: NOTE

//! Test diagnostics for binding with `mut` when the default binding mode is by-ref.
#![allow(incomplete_features, unused_assignments, unused_variables)]
#![cfg_attr(any(classic2021, classic2024), feature(ref_pat_eat_one_layer_2024))]
#![cfg_attr(any(structural2021, structural2024), feature(ref_pat_eat_one_layer_2024_structural))]

pub fn main() {
    struct Foo(u8);

    let Foo(mut a) = &Foo(0);
    //[classic2024,structural2024]~^ ERROR: binding cannot be both mutable and by-reference
    #[cfg(any(stable2021, classic2021, structural2021))] { a = 42 }
    #[cfg(any(classic2024, structural2024))] { a = &42 }

    let Foo(mut a) = &mut Foo(0);
    //[classic2024,structural2024]~^ ERROR: binding cannot be both mutable and by-reference
    #[cfg(any(stable2021, classic2021, structural2021))] { a = 42 }
    #[cfg(any(classic2024, structural2024))] { a = &mut 42 }

    let [&mut mut x] = &[&mut 0];
    //[classic2024]~^ ERROR: mismatched types
    //[classic2024]~| NOTE cannot match inherited `&` with `&mut` pattern
    //[structural2024]~^^^ ERROR binding cannot be both mutable and by-reference
    #[cfg(any(stable2021, classic2021, structural2021))] { x = 0 }
}
";

#[test]
fn a_mutable_binding_by_reference_is_behind_the_mut_ref_feature() {
    let fixture =
        format!("//- /mut-ref-mut.rs crate:other:main edition:2024 cfg:structural2024\n{MUT_REF_MUT}");
    let note = "   |
   = note: see issue #123076 <https://github.com/rust-lang/rust/issues/123076> for more information
   = help: add `#![feature(mut_ref)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date
";
    let stderr = format!(
        "error[E0658]: binding cannot be both mutable and by-reference
  --> $DIR/mut-ref-mut.rs:20:13
   |
LL |     let Foo(mut a) = &Foo(0);
   |             ^^^^
{note}
error[E0658]: binding cannot be both mutable and by-reference
  --> $DIR/mut-ref-mut.rs:25:13
   |
LL |     let Foo(mut a) = &mut Foo(0);
   |             ^^^^
{note}
error[E0658]: binding cannot be both mutable and by-reference
  --> $DIR/mut-ref-mut.rs:30:15
   |
LL |     let [&mut mut x] = &[&mut 0];
   |               ^^^^
{note}
error: aborting due to 3 previous errors

For more information about this error, try `rustc --explain E0658`.
"
    );
    assert_eq!(ui_fixture(&fixture), stderr);
}

/// tests/ui/pattern/rfc-3627-match-ergonomics-2024/experimental/ref-mut-inside-shared-ref-pat.rs
const REF_MUT_INSIDE_SHARED_REF_PAT: &str = "//@ revisions: stable2021 classic2021 structural2021 classic2024 structural2024
//@[stable2021] edition: 2021
//@[classic2021] edition: 2021
//@[structural2021] edition: 2021
//@[classic2024] edition: 2024
//@[structural2024] edition: 2024
//@[classic2024] run-rustfix
//@[structural2024] run-rustfix
//! Tests for `&` patterns matched against `&mut` reference types where the inner pattern attempts
//! to bind by mutable reference.
#![allow(incomplete_features)]
#![cfg_attr(any(classic2021, classic2024), feature(ref_pat_eat_one_layer_2024))]
#![cfg_attr(any(structural2021, structural2024), feature(ref_pat_eat_one_layer_2024_structural))]

pub fn main() {
    if let Some(&Some(ref mut x)) = &mut Some(Some(0)) {
        //[stable2021]~^ ERROR: mismatched types
        //[classic2021,structural2021,classic2024,structural2024]~^^ ERROR: cannot borrow as mutable inside an `&` pattern
        let _: &mut u8 = x;
    }

    if let &Some(Some(ref mut x)) = &mut Some(Some(0)) {
        //[stable2021]~^ ERROR: mismatched types
        //[classic2021,structural2021,classic2024,structural2024]~^^ ERROR: cannot borrow as mutable inside an `&` pattern
        let _: &mut u8 = x;
    }

    macro_rules! pat {
        ($var:ident) => { ref mut $var };
    }
    let &pat!(x) = &mut 0;
    //[stable2021]~^ ERROR: mismatched types
    //[classic2021,structural2021,classic2024,structural2024]~^^ ERROR: cannot borrow as mutable inside an `&` pattern
    let _: &mut u8 = x;

    let &(ref mut a, ref mut b) = &mut (true, false);
    //[stable2021]~^ ERROR: mismatched types
    //[classic2021,structural2021,classic2024,structural2024]~^^ ERROR: cannot borrow as mutable inside an `&` pattern
    //[classic2021,structural2021,classic2024,structural2024]~| ERROR: cannot borrow as mutable inside an `&` pattern
    let _: &mut bool = a;
    let _: &mut bool = b;

    let &[x] = &mut &mut [0];
    //[stable2021]~^ ERROR: mismatched types
    //[classic2021,classic2024]~^^ ERROR: cannot borrow as mutable inside an `&` pattern
    let _: &u32 = x;
}
";

/// The errors of `ref-mut-inside-shared-ref-pat.rs` all its revisions but `stable2021` give,
/// before the one of `&[x]` only the classic ones give.
const REF_MUT_INSIDE_SHARED_REF_PAT_ERRORS: &str = "error[E0596]: cannot borrow as mutable inside an `&` pattern
  --> $DIR/ref-mut-inside-shared-ref-pat.rs:16:31
   |
LL |     if let Some(&Some(ref mut x)) = &mut Some(Some(0)) {
   |                 -             ^
   |                 |
   |                 help: replace this `&` with `&mut`: `&mut`

error[E0596]: cannot borrow as mutable inside an `&` pattern
  --> $DIR/ref-mut-inside-shared-ref-pat.rs:22:31
   |
LL |     if let &Some(Some(ref mut x)) = &mut Some(Some(0)) {
   |            -                  ^
   |            |
   |            help: replace this `&` with `&mut`: `&mut`

error[E0596]: cannot borrow as mutable inside an `&` pattern
  --> $DIR/ref-mut-inside-shared-ref-pat.rs:31:15
   |
LL |     let &pat!(x) = &mut 0;
   |         -     ^
   |         |
   |         help: replace this `&` with `&mut`: `&mut`

error[E0596]: cannot borrow as mutable inside an `&` pattern
  --> $DIR/ref-mut-inside-shared-ref-pat.rs:36:19
   |
LL |     let &(ref mut a, ref mut b) = &mut (true, false);
   |         -         ^
   |         |
   |         help: replace this `&` with `&mut`: `&mut`

error[E0596]: cannot borrow as mutable inside an `&` pattern
  --> $DIR/ref-mut-inside-shared-ref-pat.rs:36:30
   |
LL |     let &(ref mut a, ref mut b) = &mut (true, false);
   |         -                    ^
   |         |
   |         help: replace this `&` with `&mut`: `&mut`

";

#[test]
fn a_mutable_borrow_inside_a_shared_reference_pattern_suggests_mut() {
    let fixture = format!(
        "//- minicore: option
//- /ref-mut-inside-shared-ref-pat.rs crate:other:main edition:2024 cfg:classic2024
{REF_MUT_INSIDE_SHARED_REF_PAT}"
    );
    let stderr = format!(
        "{REF_MUT_INSIDE_SHARED_REF_PAT_ERRORS}error[E0596]: cannot borrow as mutable inside an `&` pattern
  --> $DIR/ref-mut-inside-shared-ref-pat.rs:43:11
   |
LL |     let &[x] = &mut &mut [0];
   |         - ^
   |         |
   |         help: replace this `&` with `&mut`: `&mut`

error: aborting due to 6 previous errors

For more information about this error, try `rustc --explain E0596`.
"
    );
    assert_eq!(ui_fixture(&fixture), stderr);
}

#[test]
fn a_mutable_borrow_inside_a_shared_reference_pattern_is_refused_in_2021_too() {
    let fixture = format!(
        "//- minicore: option
//- /ref-mut-inside-shared-ref-pat.rs crate:other:main edition:2021 cfg:structural2021
{REF_MUT_INSIDE_SHARED_REF_PAT}"
    );
    let stderr = format!(
        "{REF_MUT_INSIDE_SHARED_REF_PAT_ERRORS}error: aborting due to 5 previous errors

For more information about this error, try `rustc --explain E0596`.
"
    );
    assert_eq!(ui_fixture(&fixture), stderr);
}
