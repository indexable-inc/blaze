//! rustc's borrow checker: its errors, from rustc's UI tests (`tests/ui/borrowck/`,
//! `tests/ui/error-codes/`), and `unused_mut`. Most errors are checked by code, where and
//! message; `E0499.rs` by rustc's whole `.stderr`, with the labels hir-ty-borrowck gives.

use crate::tests::{assert_reports, checked, error, items::ui_core, ui};

/// The errors of a crate of `main.rs` and `minicore` (`flags`, if any), each as
/// `E0499 4:17 message`: its code, the line and column of its primary span in `source`, and
/// its message; the lints and the closing lines left out.
fn errors(minicore: Option<&str>, source: &str) -> Vec<String> {
    let header = match minicore {
        Some(flags) => format!("//- minicore: {flags}\n//- /main.rs crate:other:main\n"),
        None => "//- /main.rs crate:other:main\n".to_owned(),
    };
    let checked = checked(&format!("{header}{source}"));
    checked
        .diags
        .iter()
        .filter(|it| it.is_error() && !it.is_lint())
        .filter_map(|diag| {
            let span = diag.span.primary.first()?;
            let before = &source[..span.lo as usize];
            let line = before.matches('\n').count() + 1;
            let column = before[before.rfind('\n').map_or(0, |it| it + 1)..].chars().count() + 1;
            let code = diag.code.as_ref().map_or("", |it| it.as_str());
            Some(format!("{code} {line}:{column} {}", diag.message))
        })
        .collect()
}

#[test]
fn two_mutable_borrows_at_once() {
    // tests/ui/error-codes/E0499.rs
    let source = r#"fn main() {
    let mut i = 0;
    let mut x = &mut i;
    let mut a = &mut i; //~ ERROR E0499
    a.use_mut();
    x.use_mut();
}

trait Fake { fn use_mut(&mut self) { } fn use_ref(&self) { }  }
impl<T> Fake for T { }
"#;
    assert_eq!(
        errors(None, source),
        ["E0499 4:17 cannot borrow `i` as mutable more than once at a time"]
    );
    // tests/ui/error-codes/E0499.stderr
    let stderr = r#"error[E0499]: cannot borrow `i` as mutable more than once at a time
  --> $DIR/E0499.rs:4:17
   |
LL |     let mut x = &mut i;
   |                 ------ first mutable borrow occurs here
LL |     let mut a = &mut i;
   |                 ^^^^^^ second mutable borrow occurs here
LL |     a.use_mut();
LL |     x.use_mut();
   |     - first borrow later used here

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0499`.
"#;
    assert_eq!(ui("E0499.rs", "2015", source), stderr);
}

#[test]
fn a_mutable_borrow_while_borrowed_as_immutable() {
    // tests/ui/error-codes/E0502.rs
    let source = r#"fn bar(x: &mut i32) {}
fn foo(a: &mut i32) {
    let ref y = a;
    bar(a); //~ ERROR E0502
    y.use_ref();
}

fn main() {
}

trait Fake { fn use_mut(&mut self) { } fn use_ref(&self) { }  }
impl<T> Fake for T { }
"#;
    assert_eq!(
        errors(None, source),
        ["E0502 4:5 cannot borrow `*a` as mutable because it is also borrowed as immutable"]
    );
}

#[test]
fn a_move_out_of_a_borrowed_value() {
    // tests/ui/error-codes/E0505.rs
    let source = r#"struct Value {}

fn eat(val: Value) {}

fn main() {
    let x = Value{};
    {
        let _ref_to_val: &Value = &x;
        eat(x); //~ ERROR E0505
        _ref_to_val.use_ref();
    }
}

trait Fake { fn use_mut(&mut self) { } fn use_ref(&self) { }  }
impl<T> Fake for T { }
"#;
    assert_eq!(errors(None, source), ["E0505 9:13 cannot move out of `x` because it is borrowed"]);
}

#[test]
fn a_mutable_borrow_of_an_immutable_binding() {
    // tests/ui/error-codes/E0596.rs
    let source = r#"fn main() {
    let x = 1;
    let y = &mut x; //~ ERROR [E0596]
}
"#;
    assert_eq!(
        errors(None, source),
        ["E0596 3:13 cannot borrow `x` as mutable, as it is not declared as mutable"]
    );
}

#[test]
fn a_borrow_that_outlives_its_value() {
    // tests/ui/error-codes/E0597.rs
    let source = r#"struct Foo<'a> {
    x: Option<&'a u32>,
}

fn main() {
    let mut x = Foo { x: None };
    let y = 0;
    x.x = Some(&y);
    //~^ ERROR `y` does not live long enough [E0597]
}

impl<'a> Drop for Foo<'a> { fn drop(&mut self) { } }
"#;
    assert_eq!(errors(Some("option, drop"), source), ["E0597 8:16 `y` does not live long enough"]);
}

/// rustc's `promote_consts` promotes an array of associated constants and const calls in a
/// const or static initializer: type inference resolves `A::ITEM`, `<A as HasItem>::ITEM` and
/// `Self::ITEM`, which value-name lookup does not, so none is a temporary dropped (E0716).
#[test]
fn associated_constants_promote_in_const_and_static_initializers() {
    let source = r#"struct Item(u8);
const fn item(value: u8) -> Item { Item(value) }
trait HasItem {
    const ITEM: Item;
    const ITEMS: &'static [Item] = &[Self::ITEM, item(2)];
}
struct A;
impl HasItem for A { const ITEM: Item = item(1); }
impl A { const INHERENT: Item = item(3); }
const QUALIFIED: &[Item] = &[<A as HasItem>::ITEM, item(2)];
const TRAIT: &[Item] = &[A::ITEM, item(2)];
const INHERENT: &[Item] = &[A::INHERENT, item(2)];
static STATIC: &[Item] = &[A::ITEM, item(2)];
fn main() {}
"#;
    assert_eq!(errors(Some("sized, copy, unsize, coerce_unsized"), source), [] as [&str; 0]);
}

/// The same array in a function body is not promoted: its const call runs at run time, so
/// returning a borrow of it is E0515.
#[test]
fn associated_constants_do_not_promote_a_runtime_const_call() {
    let source = r#"struct Item(u8);
const fn item(value: u8) -> Item { Item(value) }
trait HasItem { const ITEM: Item; }
struct A;
impl HasItem for A { const ITEM: Item = item(1); }
fn runtime() -> &'static [Item] {
    &[A::ITEM, item(2)]
}
fn main() {}
"#;
    assert_eq!(
        errors(Some("sized, copy, unsize, coerce_unsized"), source),
        ["E0515 7:5 returns a value referencing data owned by the current function"]
    );
}

#[test]
fn an_immutable_pattern_binding_assigned_again() {
    // tests/ui/borrowck/borrowck-match-binding-is-assignment.rs
    let source = r#"// Test that immutable pattern bindings cannot be reassigned.

enum E {
    Foo(isize)
}

struct S {
    bar: isize,
}

pub fn main() {
    match 1 {
        x => {
            x += 1; //~ ERROR [E0384]
        }
    }

    match E::Foo(1) {
        E::Foo(x) => {
            x += 1; //~ ERROR [E0384]
        }
    }

    match (S { bar: 1 }) {
        S { bar: x } => {
            x += 1; //~ ERROR [E0384]
        }
    }

    match (1,) {
        (x,) => {
            x += 1; //~ ERROR [E0384]
        }
    }

    match [1,2,3] {
        [x,_,_] => {
            x += 1; //~ ERROR [E0384]
        }
    }
}
"#;
    let message = "cannot assign twice to immutable variable `x`";
    assert_eq!(
        errors(None, source),
        [14, 20, 26, 32, 38].map(|line| format!("E0384 {line}:13 {message}"))
    );
}

#[test]
fn a_use_of_a_mutably_borrowed_value() {
    // tests/ui/borrowck/borrowck-match-already-borrowed.rs
    let source = r#"enum Foo {
    A(i32),
    B
}

fn match_enum() {
    let mut foo = Foo::B;
    let p = &mut foo;
    let _ = match foo { //~ ERROR [E0503]
        Foo::B => 1,
        _ => 2,
        Foo::A(x) => x //~ ERROR [E0503]
    };
    drop(p);
}


fn main() {
    let mut x = 1;
    let r = &mut x;
    let _ = match x {
        x => x + 1, //~ ERROR [E0503]
        y => y + 2, //~ ERROR [E0503]
    };
    drop(r);
}
"#;
    let errors = errors(Some("drop"), source);
    let expected = [("9:19", "`foo`"), ("12:16", "`foo.0`"), ("22:9", "`x`"), ("23:9", "`x`")];
    assert_eq!(errors.len(), expected.len(), "{errors:?}");
    for (error, (at, place)) in errors.iter().zip(expected) {
        // The bindings copy an `i32`: a use, not a move.
        assert!(
            error.contains(&format!(" {at} "))
                && error.contains(place)
                && error.contains("cannot use"),
            "{at} {place}: {errors:?}"
        );
    }
}

#[test]
fn a_mut_binding_never_mutated() {
    // tests/ui/nll/issue-61424.rs, with `x = 1u32; let _ = x;` for its `String::new()` and
    // `dbg!(x)`, which need std: the same lines and columns.
    let source = r#"//@ run-rustfix

#![deny(unused_mut)]

fn main() {
    let mut x; //~ ERROR: variable does not need to be mutable
    x = 1u32;
    let _ = x;
}
"#;
    let stderr = r#"error: variable does not need to be mutable
  --> $DIR/issue-61424.rs:6:9
   |
LL |     let mut x;
   |         ----^
   |         |
   |         help: remove this `mut`
   |
note: the lint level is defined here
  --> $DIR/issue-61424.rs:3:9
   |
LL | #![deny(unused_mut)]
   |         ^^^^^^^^^^

error: aborting due to 1 previous error

"#;
    assert_eq!(ui("issue-61424.rs", "2015", source), stderr);
}

#[test]
fn a_mut_closure_argument_never_mutated() {
    // tests/ui/nll/unused-mut-issue-50343.rs, its closure called directly rather than through
    // `vec![..].iter().map(..)`, which needs std; minicore's `Fn` traits for the call.
    let source = r#"//@ run-rustfix

#![deny(unused_mut)]
#![allow(unused_variables)] // for rustfix

fn main() {
    let f = |(mut x, _y): (i32, i32)| ();
    f((42, 22));
}
"#;
    assert_reports(
        &ui_core("unused-mut-issue-50343.rs", "2015", "fn", source),
        &[error(7, &["`mut`"])],
    );
}

#[test]
fn a_mut_binding_mutated_is_not_linted() {
    let source = r#"#![deny(unused_mut)]

fn main() {
    let mut x = 1u32;
    x += 1;
    let _ = x;
}
"#;
    assert_eq!(ui("mutated.rs", "2015", source), "");
}

#[test]
fn a_reference_to_a_bits_field_names_the_struct_and_the_field() {
    // blaze's own: a field of a `#[blaze::bits]` struct sits at a bit offset and has no
    // address. A reference to the whole struct and a copy of the field are fine.
    let source = r#"#[blaze::bits]
struct Flags { valid: bool, level: u8 }

fn main() {
    let mut f = Flags { valid: true, level: 3 };
    let r = &f.level;
    let p = &raw const f.valid;
    let whole = &f;
    let copied = { f.level };
}
"#;
    assert_eq!(
        errors(None, source),
        [
            "E0793 6:13 reference to field `level` of `#[blaze::bits]` struct `Flags`: a \
             bit-packed field has no address; copy it out with `{ f.level }` or borrow the \
             whole struct",
            "E0793 7:13 reference to field `valid` of `#[blaze::bits]` struct `Flags`: a \
             bit-packed field has no address; copy it out with `{ f.valid }` or borrow the \
             whole struct",
        ]
    );
}

/// `#![feature(blaze_comptime)]` and `library/core/src/reflect.rs` reduced to what inference,
/// lowering and borrowck read: the head of each `for const` test's `main.rs`.
const REFLECT: &str = r#"#![feature(blaze_comptime)]
#[lang = "field"]
pub unsafe trait FieldOf: Copy {
    #[lang = "field_base"]
    type Base: ?Sized;
    #[lang = "field_type"]
    type Type: ?Sized;
}
#[lang = "blaze_field"]
pub unsafe trait Field<B: ?Sized>: FieldOf {}
#[lang = "blaze_fields"]
pub struct Fields<T: ?Sized, P = ()>(*const T, P);
pub fn fields_of<T: ?Sized>() -> Fields<T> { loop {} }
"#;

/// The line and column (1-based) of the `nth` (from 0) `needle` in `source`, as [`errors`]
/// writes a primary span's start.
fn at(source: &str, needle: &str, nth: usize) -> String {
    let (offset, _) = source.match_indices(needle).nth(nth).expect("the needle is in the source");
    let before = &source[..offset];
    let line = before.matches('\n').count() + 1;
    let column = before[before.rfind('\n').map_or(0, |it| it + 1)..].chars().count() + 1;
    format!("{line}:{column}")
}

#[test]
fn two_mutable_borrows_of_one_reflected_field_conflict() {
    // One iteration of a `for const` body names one field: `&mut x.[f]` twice in it is two
    // mutable borrows of the same place, named as the source writes it.
    let source = format!(
        "{REFLECT}\
pub trait Touch {{ fn touch(&mut self); }}
fn twice<T>(x: &mut T) where for<F: Field<T>> F::Type: Touch {{
    for const f in fields_of::<T>() {{
        let a = &mut x.[f];
        let b = &mut x.[f];
        a.touch();
        b.touch();
    }}
}}
fn main() {{}}
"
    );
    assert_eq!(
        errors(Some("sized, copy"), &source),
        [format!(
            "E0499 {} cannot borrow `x.[f]` as mutable more than once at a time",
            at(&source, "&mut x.[f]", 1)
        )]
    );
}

#[test]
fn a_move_out_of_a_reflected_field_is_refused_naming_the_place() {
    // Behind a reference, as a named field's: E0507 naming `x.[f]`. Out of an owned base, a
    // partial move per field is not built yet (drop elaboration runs on the generic body,
    // which cannot say which field an open drop of `y` skips): E0507 that says so.
    let source = format!(
        "{REFLECT}\
fn borrowed<T>(x: &T) where for<F: Field<T>> F::Type: Sized {{
    for const f in fields_of::<T>() {{
        let v = x.[f];
    }}
}}
fn owned<T>(y: T) where for<F: Field<T>> F::Type: Sized {{
    for const f in fields_of::<T>() {{
        let v = y.[f];
    }}
}}
fn main() {{}}
"
    );
    assert_eq!(
        errors(Some("sized, copy"), &source),
        [
            format!(
                "E0507 {} cannot move out of `x.[f]` which is behind a shared reference",
                at(&source, "x.[f]", 0)
            ),
            format!(
                "E0507 {} cannot move out of the reflected field `y.[f]`: moving a field out of \
                 its base in a `for const` loop is not supported yet \
                 (`#![feature(blaze_comptime)]`); borrow it or copy it",
                at(&source, "y.[f]", 0)
            ),
        ]
    );
}

#[test]
fn a_for_const_body_holds_its_binders_bounds_in_borrowck_and_lints() {
    // `T` is generic: the loop's `F::Type` is `Show + Copy` only by the field binder. Copying
    // `t.[f]` out of a `&T` twice and calling `show` passes borrowck with no error and no
    // unsatisfied bound; a lint that asks `Copy` of the type gets inference's answer (yes), so
    // `dropping_copy_types` fires, which it cannot without the loop's environment.
    let source = format!(
        "#![warn(dropping_copy_types)]
{REFLECT}\
#[rustc_diagnostic_item = \"mem_drop\"]
pub fn drop<X>(_x: X) {{}}
pub trait Show {{ fn show(&self); }}
fn each<T>(t: &T) where for<F: Field<T>> F::Type: Show + Copy {{
    for const f in fields_of::<T>() {{
        let v = t.[f];
        let w = v;
        v.show();
        w.show();
        drop(t.[f]);
    }}
}}
fn main() {{}}
"
    );
    let checked = checked(&format!(
        "//- minicore: sized, copy, clone\n//- /main.rs crate:other:main\n{source}"
    ));
    let messages: Vec<&str> = checked.diags.iter().map(|it| it.message.as_str()).collect();
    let refused: Vec<&str> = checked
        .diags
        .iter()
        .filter(|it| {
            it.is_error()
                || it.message.contains("not satisfied")
                || it.message.contains("does not implement")
                || it.message.contains("moved")
        })
        .map(|it| it.message.as_str())
        .collect();
    assert_eq!(refused, Vec::<&str>::new(), "every diagnostic: {messages:?}");
    assert!(
        messages.contains(
            &"calls to `std::mem::drop` with a value that implements `Copy` does nothing"
        ),
        "`dropping_copy_types` sees `F::Type: Copy`: {messages:?}"
    );
}

/// E0320 of a local whose type nests `Option` past the recursion limit around a projection
/// the drop check cannot see through (a chain of 140 such `let`s, cut to the
/// first refusal): rustc's message with its type abbreviated by `short_string`, its note of
/// what overflowed, and the body, the limit and a raised limit to try.
#[test]
fn drop_check_overflow_names_the_body_the_type_and_the_limit() {
    let mut source = String::from(
        "pub trait Tr { type A; }\npub fn wrap<T>(x: T) -> Option<T> { Some(x) }\n\
         pub fn caller<T: Tr>(x: T::A) {\n    let a0 = x;\n",
    );
    for i in 1..=130 {
        source += &format!("    let a{i} = wrap(a{});\n", i - 1);
    }
    source += "    let _ = a130;\n}\nfn main() {}\n";
    let stderr = ui_core("param140.rs", "2024", "sized, option", &source);
    let first = stderr.split("\n\n").find(|it| it.contains("E0320")).expect(&stderr);
    assert_eq!(
        first,
        r#"error[E0320]: overflow while adding drop-check rules for `Option<Option<Option<Option<Option<Option<Option<Option<_>>>>>>>>` in `caller`, past the recursion limit of 128
  --> $DIR/param140.rs:134:9
   |
LL |     let a130 = wrap(a129);
   |         ^^^^
   |
   = note: overflowed on `Option<<T as Tr>::A>`
   = help: consider increasing the recursion limit by adding a `#![recursion_limit = "256"]` attribute to your crate (`main`)"#
    );
}

#[test]
fn a_by_value_reference_self_does_not_choose_the_output_lifetime() {
    // A receiver `self` whose `Self` is `&u8` is not `&self`: lifetime elision gives the
    // returned `&u8` the input's lifetime, not the receiver's, in an impl's method and a
    // trait's default one alike, so returning `input` past the receiver's local is fine.
    let source = r#"trait Pick {
    fn pick(self, input: &u8) -> &u8;
}
impl Pick for &u8 {
    fn pick(self, input: &u8) -> &u8 { input }
}
fn caller(input: &u8) -> &u8 {
    let receiver = 9;
    Pick::pick(&receiver, input)
}
trait DefaultPick: Sized {
    fn pick(self, input: &u8) -> &u8 { input }
}
impl DefaultPick for &u16 {}
fn default_caller(input: &u8) -> &u8 {
    let receiver = 9;
    DefaultPick::pick(&receiver, input)
}
fn main() {}
"#;
    assert_eq!(errors(Some("sized, copy"), source), Vec::<String>::new());
}

#[test]
fn a_by_value_reference_self_cannot_escape_as_the_input_lifetime() {
    // The other side of the same rule: the return type has `input`'s lifetime, which the
    // receiver `&u8` need not outlive.
    let source = r#"trait Pick {
    fn pick(self, input: &u8) -> &u8;
}
impl Pick for &u8 {
    fn pick(self, _input: &u8) -> &u8 {
        self
    }
}
fn main() {}
"#;
    assert_eq!(
        errors(Some("sized"), source),
        [format!(" {} lifetime may not live long enough", at(source, "self\n", 0))]
    );
}

#[test]
fn a_borrowed_reference_self_still_chooses_the_output_lifetime() {
    // `&self` with `Self = &u8` is a reference to `Self`: the return type has the receiver's
    // lifetime, so returning `input` is refused, and a caller may keep what `keep` returns
    // past the input's local.
    let source = r#"trait Pick {
    fn pick(&self, input: &u8) -> &u8;
}
impl Pick for &u8 {
    fn pick(&self, input: &u8) -> &u8 {
        input
    }
}
trait Keep {
    fn keep(&self, _input: &u8) -> &Self { self }
}
impl Keep for &u8 {}
fn caller<'a>(receiver: &'a &'a u8) -> &'a &'a u8 {
    let input = 9;
    Keep::keep(receiver, &input)
}
fn main() {}
"#;
    assert_eq!(
        errors(Some("sized"), source),
        [format!(" {} lifetime may not live long enough", at(source, "input\n", 0))]
    );
}
