//! The printing of every form of type as rustc prints it in a message: each test prints the
//! type of the initializer of each `let` of a fixture, in order. Where rustc's UI tests show a
//! form, the test is one of theirs and says which; the others follow `pretty.rs`.

use std::collections::HashMap;

use span::FileId;
use syntax::view::{self as ast, AstNode as _};
use hir_expand::fixture;

use crate::{
    cx::{Cx, Files},
    print,
    tests::paths,
};

/// What `f` makes of the type of the initializer of each `let` of the fixture's first file,
/// its files named `$DIR/<path>` as rustc's UI tests name them.
fn each_let(fixture: &str, f: impl Fn(&Cx<'_, '_>, &hir::Type<'_>) -> String) -> Vec<String> {
    let fx = fixture::db(fixture);
    let files = fx.files().to_vec();
    let db = fx.into_db();
    let names: HashMap<FileId, String> = files
        .iter()
        .zip(paths(fixture))
        .map(|(file, path)| (file.file_id(&db), format!("$DIR/{path}")))
        .collect();
    hir::attach_db(&db, || {
        let krate = hir::check::workspace_crates(&db)[0];
        let frame = crate::tests::frame();
        let cx = Cx::new(&db, krate, Files::new(&names, ""), &frame);
        let source = cx.vsema.parse(files[0]);
        source
            .syntax()
            .descendants()
            .filter_map(ast::LetStmt::cast)
            .filter_map(|it| it.initializer())
            .map(|init| f(&cx, &cx.vsema.type_of_expr(init).expect("an initializer has a type").original))
            .collect()
    })
}

fn printed(fixture: &str) -> Vec<String> {
    each_let(fixture, |cx, ty| print::ty(cx, ty))
}

#[test]
fn a_closure_is_its_span_from_move_to_its_parameters() {
    // tests/ui/span/move-closure.rs: "found closure `{closure@$DIR/move-closure.rs:5:17: 5:24}`".
    let fixture = "//- /move-closure.rs crate:other:main edition:2015
// Regression test for issue #24986
// Make sure that the span of a closure marked `move` begins at the `move` keyword.

fn main() {
    let x: () = move || (); //~ ERROR mismatched types
}
";
    assert_eq!(printed(fixture), ["{closure@$DIR/move-closure.rs:5:17: 5:24}"]);
}

#[test]
fn a_closure_with_typed_parameters_ends_after_them() {
    // tests/ui/closures/closure-no-fn-1.rs:
    // "found closure `{closure@$DIR/closure-no-fn-1.rs:6:29: 6:36}`".
    let fixture = "//- /closure-no-fn-1.rs crate:other:main edition:2015
// Ensure that capturing closures are never coerced to fns
// Especially interesting as non-capturing closures can be.

fn main() {
    let mut a = 0u8;
    let foo: fn(u8) -> u8 = |v: u8| { a += v; a };
    //~^ ERROR mismatched types
}
";
    assert_eq!(printed(fixture), ["u8", "{closure@$DIR/closure-no-fn-1.rs:6:29: 6:36}"]);
}

#[test]
fn a_function_item_is_its_signature_and_its_path() {
    // tests/ui/expr/if/if-typeck.rs: "found fn item `fn() {f}`"; tests/ui/extern/
    // extern-rust-fn-type-error-10764.rs: "found fn item `extern \"C\" fn() {bar}`". An elided
    // lifetime stays elided, as written, where rustc writes `for<'a> fn(&'a u8)`.
    let fixture = r#"//- /main.rs crate:other:main edition:2021
fn f() {}
extern "C" fn bar() {}
fn id<T>(t: T) -> T { t }
fn g(_: &u8) {}
fn h<'x>(_: &'x u8) -> &'x u8 { loop {} }
extern "C" { fn printf(f: *const u8, ...) -> i32; }

fn main() {
    let a = f;
    let b = bar;
    let c = id::<u8>;
    let d = g;
    let e = h;
    let p = printf;
}
"#;
    assert_eq!(
        printed(fixture),
        [
            "fn() {f}",
            "extern \"C\" fn() {bar}",
            "fn(u8) -> u8 {id::<u8>}",
            "fn(&u8) {g}",
            "for<'x> fn(&'x u8) -> &'x u8 {h}",
            "unsafe extern \"C\" fn(*const u8, ...) -> i32 {printf}",
        ]
    );
}

#[test]
fn constructors_and_methods_are_value_paths() {
    let fixture = "//- /main.rs crate:other:main edition:2021
struct P(u8);
enum E { V(u8) }
struct S;
impl S { fn m(&self) {} }
trait Tr { fn t(&self); }
impl Tr for S { fn t(&self) {} }

fn main() {
    let a = P;
    let b = E::V;
    let c = S::m;
    let d = <S as Tr>::t;
}
";
    assert_eq!(
        printed(fixture),
        ["fn(u8) -> P {P}", "fn(u8) -> E {E::V}", "fn(&S) {S::m}", "fn(&S) {<S as Tr>::t}"]
    );
}

#[test]
fn a_function_pointer_names_its_anonymous_lifetimes() {
    let fixture = "//- /main.rs crate:other:main edition:2021
fn g(_: &u8) {}
fn s(_: &'static (dyn Tr + 'static)) {}
trait Tr {}

fn main() {
    let a = g as fn(&u8);
    let b = g as fn(&'static u8);
    let c = s;
}
";
    assert_eq!(
        printed(fixture),
        ["for<'a> fn(&'a u8)", "fn(&'static u8)", "fn(&'static (dyn Tr + 'static)) {s}"]
    );
}

#[test]
fn builtin_types_print_as_written() {
    let fixture = r#"//- /main.rs crate:other:main edition:2021
fn main() {
    let a = ();
    let b = (1u8,);
    let c = (1u8, 'c', true);
    let d = [0u16; 4];
    let e = &0u8 as *const u8;
    let f = &mut 0i64 as *mut i64;
    let g = "a";
    let h = 1.5f32;
    let n = loop {};
}
"#;
    assert_eq!(
        printed(fixture),
        ["()", "(u8,)", "(u8, char, bool)", "[u16; 4]", "*const u8", "*mut i64", "&'static str", "f32", "!"]
    );
}

#[test]
fn paths_are_trimmed_to_a_name_no_other_item_has() {
    let fixture = "//- /main.rs crate:other:main edition:2021
mod a { pub struct S; }
mod b { pub struct S; }
mod c { pub mod d { pub struct T; } }

fn main() {
    let x = a::S;
    let y = b::S;
    let z = c::d::T;
}
";
    assert_eq!(printed(fixture), ["a::S", "b::S", "T"]);
}

#[test]
fn arguments_equal_to_their_defaults_are_left_out() {
    let fixture = "//- /main.rs crate:other:main edition:2021
struct Foo<T, U = u8>(T, U);
struct A<const N: usize>;

fn main() {
    let x = Foo(1i32, 2u8);
    let y = Foo(1i32, 2u16);
    let z = A::<3>;
}
";
    assert_eq!(printed(fixture), ["Foo<i32>", "Foo<i32, u16>", "A<3>"]);
}

#[test]
fn impl_trait_is_its_bounds_with_fn_sugar() {
    let fixture = "//- minicore: sized, fn
//- /main.rs crate:other:main edition:2021
trait Tr {}
trait Gen<T> {}
struct S;
impl Tr for S {}
impl Gen<u8> for S {}
fn o() -> impl Tr { S }
fn two() -> impl Tr + Gen<u8> { S }
fn c() -> impl Fn(u8) -> u8 { |x| x }
fn u() -> impl FnOnce() { || () }

fn main() {
    let a = o();
    let b = two();
    let x = c();
    let y = u();
}
";
    assert_eq!(printed(fixture), ["impl Tr", "impl Tr + Gen<u8>", "impl Fn(u8) -> u8", "impl FnOnce()"]);
}

#[test]
fn sort_strings_name_the_kind_of_type() {
    // rustc's `Ty::sort_string`, as the E0308 notes and labels print it.
    let fixture = "//- /main.rs crate:other:main edition:2021
struct P(u8);
fn f() {}

fn main() {
    let a = || ();
    let b = f;
    let c = P;
    let d = f as fn();
    let e = 1u8;
}
";
    assert_eq!(
        each_let(fixture, |cx, ty| print::sort_string(cx, ty)),
        ["closure", "fn item", "struct constructor", "fn pointer", "`u8`"]
    );
    assert_eq!(
        each_let(fixture, |cx, ty| print::prefix_string(cx, ty)),
        ["closure", "fn item", "struct constructor", "fn pointer", "type"]
    );
}
