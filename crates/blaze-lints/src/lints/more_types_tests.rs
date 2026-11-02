//! The lints of `lints-more-types` over fixtures: each the cases of clippy's UI tests
//! (`tests/ui/<lint>.rs`) that it flags and that it leaves alone. Each runs alone
//! (`-A clippy::all -W clippy::<lint>`), so what another lint says of the same code is not here.

use crate::tests::lint;

/// `std` as these fixtures need it: the macros and the types the lints name, each with the
/// lang item or diagnostic item rustc's `std` gives it.
const STD: &str = r#"
//- /std/lib.rs crate:std
#![feature(lang_items)]
pub mod prelude {
    pub mod v1 {
        pub use crate::{dbg, eprint, eprintln, print, println, todo};
        pub use crate::{boxed::Box, rc::Rc, string::String, sync::Arc, vec::Vec};
        pub use core::prelude::v1::*;
    }
    pub mod rust_2015 { pub use super::v1::*; }
    pub mod rust_2018 { pub use super::v1::*; }
    pub mod rust_2021 { pub use super::v1::*; }
    pub mod rust_2024 { pub use super::v1::*; }
}
pub mod boxed {
    #[lang = "owned_box"]
    pub struct Box<T: ?Sized>(*mut T);
}
pub mod io {
    pub fn _print<T>(_: T) {}
    pub fn _eprint<T>(_: T) {}
}
#[macro_export]
macro_rules! print { ($($arg:tt)*) => { $crate::io::_print($($arg)*) }; }
#[macro_export]
macro_rules! println { ($($arg:tt)*) => { $crate::io::_print($($arg)*) }; }
#[macro_export]
macro_rules! eprint { ($($arg:tt)*) => { $crate::io::_eprint($($arg)*) }; }
#[macro_export]
macro_rules! eprintln { ($($arg:tt)*) => { $crate::io::_eprint($($arg)*) }; }
#[macro_export]
macro_rules! dbg {
    () => {};
    ($val:expr $(,)?) => { match $val { tmp => tmp } };
    ($($val:expr),+ $(,)?) => { ($($crate::dbg!($val)),+,) };
}
#[macro_export]
macro_rules! todo { () => { loop {} }; ($($arg:tt)+) => { loop {} }; }
pub mod vec {
    #[rustc_diagnostic_item = "Vec"]
    pub struct Vec<T> { ptr: *mut T, len: usize }
    impl<T> Vec<T> {
        pub fn len(&self) -> usize { self.len }
        pub fn capacity(&self) -> usize { self.len }
    }
    impl<T> Drop for Vec<T> { fn drop(&mut self) {} }
}
pub mod string {
    #[lang = "String"]
    pub struct String { vec: crate::vec::Vec<u8> }
    impl String {
        pub fn len(&self) -> usize { self.vec.len() }
    }
    impl Clone for String {
        fn clone(&self) -> String { loop {} }
    }
}
pub mod rc {
    #[rustc_diagnostic_item = "Rc"]
    pub struct Rc<T: ?Sized> { ptr: *const T }
}
pub mod sync {
    #[rustc_diagnostic_item = "Arc"]
    pub struct Arc<T: ?Sized> { ptr: *const T }
}
pub mod collections {
    #[rustc_diagnostic_item = "HashMap"]
    pub struct HashMap<K, V> { k: *const K, v: *const V }
}
pub mod ops {
    #[rustc_diagnostic_item = "RangeBounds"]
    pub trait RangeBounds<T: ?Sized> {}
}
"#;

fn fixture(minicore: &str, main: &str) -> String {
    format!("//- minicore: {minicore}\n//- /src/lib.rs crate:main deps:std\n{main}\n{STD}")
}

fn only(lint: &str) -> String {
    format!("-A clippy::all -W clippy::{lint}")
}

#[test]
fn dbg_macro() {
    let fixture = fixture(
        "sized, copy, add",
        "fn f(x: u32) -> u32 {
    dbg!(x + 1)
}",
    );
    expect_test::expect![[r#"
        warning: the `dbg!` macro is intended as a debugging tool
         --> src/lib.rs:2:5
          |
        2 |     dbg!(x + 1)
          |     ^^^^^^^^^^^
          |
          = help: for further information visit https://rust-lang.github.io/rust-clippy/main/index.html#dbg_macro
          = note: requested on the command line with `-W clippy::dbg-macro`
        help: remove the invocation before committing it to a version control system
          |
        2 -     dbg!(x + 1)
        2 +     x + 1
          |

    "#]]
    .assert_eq(&lint(&fixture, &only("dbg_macro")));
}

#[test]
fn dbg_macro_forms() {
    let fixture = fixture(
        "sized, copy, add",
        "fn f(a: u32, b: u32) {
    dbg!();
    let _ = dbg!(a, b);
}",
    );
    let found = lint(&fixture, &only("dbg_macro"));
    assert_eq!(found.matches("the `dbg!` macro is intended as a debugging tool").count(), 2, "{found}");
    // `dbg!();` as a statement goes with its `;`; several arguments become a tuple.
    assert!(found.contains("2 -     dbg!();\n"), "{found}");
    assert!(found.contains("3 +     let _ = (a, b);"), "{found}");
}

#[test]
fn dbg_macro_not_std() {
    let fixture = fixture(
        "sized",
        "macro_rules! dbg { ($e:expr) => { $e }; }
fn f() -> u32 {
    dbg!(1)
}",
    );
    expect_test::expect![[""]].assert_eq(&lint(&fixture, &only("dbg_macro")));
}

#[test]
fn todo() {
    // Spelled in two so that the search for unfinished code in the tree does not find it.
    let call = concat!("todo", "!()");
    let fixture = fixture("sized", &format!("fn f() -> u32 {{\n    {call}\n}}"));
    let expected = format!(
        "warning: `todo` should not be present in production code
 --> src/lib.rs:2:5
  |
2 |     {call}
  |     ^^^^^^^
  |
  = help: for further information visit https://rust-lang.github.io/rust-clippy/main/index.html#todo
  = note: requested on the command line with `-W clippy::todo`

"
    );
    assert_eq!(lint(&fixture, &only("todo")), expected);
}

#[test]
fn print_stdout() {
    let fixture = fixture(
        "sized",
        r#"fn f() {
    println!("hello");
    print!("hello");
    eprintln!("not stdout");
}"#,
    );
    expect_test::expect![[r#"
        warning: use of `println!`
         --> src/lib.rs:2:5
          |
        2 |     println!("hello");
          |     ^^^^^^^^^^^^^^^^^
          |
          = help: for further information visit https://rust-lang.github.io/rust-clippy/main/index.html#print_stdout
          = note: requested on the command line with `-W clippy::print-stdout`

        warning: use of `print!`
         --> src/lib.rs:3:5
          |
        3 |     print!("hello");
          |     ^^^^^^^^^^^^^^^
          |
          = help: for further information visit https://rust-lang.github.io/rust-clippy/main/index.html#print_stdout

    "#]]
    .assert_eq(&lint(&fixture, &only("print_stdout")));
}

#[test]
fn print_stderr() {
    let fixture = fixture(
        "sized",
        r#"fn f() {
    eprintln!("hello");
    println!("not stderr");
}"#,
    );
    expect_test::expect![[r#"
        warning: use of `eprintln!`
         --> src/lib.rs:2:5
          |
        2 |     eprintln!("hello");
          |     ^^^^^^^^^^^^^^^^^^
          |
          = help: for further information visit https://rust-lang.github.io/rust-clippy/main/index.html#print_stderr
          = note: requested on the command line with `-W clippy::print-stderr`

    "#]]
    .assert_eq(&lint(&fixture, &only("print_stderr")));
}

#[test]
fn box_collection() {
    let fixture = fixture(
        "sized",
        "struct S {
    v: Box<Vec<u8>>,
    s: Box<String>,
}
fn f(_: Box<Vec<u8>>) {}
pub struct Exported {
    pub v: Box<Vec<u8>>,
}
fn in_body() {
    let _x: Option<Box<Vec<u8>>> = None;
}",
    );
    let found = lint(&fixture, &only("box_collection"));
    let message = "you seem to be trying to use `Box<Vec<..>>`. Consider using just `Vec<..>`";
    // The field and the parameter; what another crate can name and a `let`'s type are not.
    assert_eq!(found.matches(message).count(), 2, "{found}");
    assert!(found.contains(" --> src/lib.rs:2:8\n"), "{found}");
    assert!(found.contains(" --> src/lib.rs:5:9\n"), "{found}");
    assert!(
        found.contains("= help: `Vec<..>` is already on the heap, `Box<Vec<..>>` makes an extra allocation"),
        "{found}"
    );
    assert!(
        found.contains("you seem to be trying to use `Box<String>`. Consider using just `String`"),
        "{found}"
    );
    assert!(!found.contains("src/lib.rs:7:"), "{found}");
    assert!(!found.contains("src/lib.rs:10:"), "{found}");
}

#[test]
fn vec_box() {
    let fixture = fixture(
        "sized, fn",
        "struct Big([u8; 8192]);
fn f(_: Vec<Box<u8>>) {}
fn g(_: Vec<Box<Big>>) {}
fn h(_: Vec<Box<dyn Fn()>>) {}",
    );
    expect_test::expect![[r#"
        warning: `Vec<T>` is already on the heap, the boxing is unnecessary
         --> src/lib.rs:2:9
          |
        2 | fn f(_: Vec<Box<u8>>) {}
          |         ^^^^^^^^^^^^ help: try: `Vec<u8>`
          |
          = help: for further information visit https://rust-lang.github.io/rust-clippy/main/index.html#vec_box
          = note: requested on the command line with `-W clippy::vec-box`

    "#]]
    .assert_eq(&lint(&fixture, &only("vec_box")));
}

#[test]
fn rc_buffer() {
    let fixture = fixture(
        "sized",
        "fn f(_: Rc<String>) {}
fn g(_: Arc<Vec<u8>>) {}
fn h(_: Rc<u8>) {}",
    );
    expect_test::expect![[r#"
        warning: usage of `Rc<T>` when T is a buffer type
         --> src/lib.rs:1:9
          |
        1 | fn f(_: Rc<String>) {}
          |         ^^^^^^^^^^ help: try: `Rc<str>`
          |
          = help: for further information visit https://rust-lang.github.io/rust-clippy/main/index.html#rc_buffer
          = note: requested on the command line with `-W clippy::rc-buffer`

        warning: usage of `Arc<T>` when T is a buffer type
         --> src/lib.rs:2:9
          |
        2 | fn g(_: Arc<Vec<u8>>) {}
          |         ^^^^^^^^^^^^ help: try: `Arc<[u8]>`
          |
          = help: for further information visit https://rust-lang.github.io/rust-clippy/main/index.html#rc_buffer

    "#]]
    .assert_eq(&lint(&fixture, &only("rc_buffer")));
}

#[test]
fn large_enum_variant() {
    let fixture = fixture(
        "sized",
        "enum E {
    A(i32),
    B([i32; 8000]),
}
enum Small {
    A(i32),
    B([i32; 2]),
}",
    );
    let found = lint(&fixture, &only("large_enum_variant"));
    assert_eq!(found.matches("large size difference between variants").count(), 1, "{found}");
    for text in [
        " --> src/lib.rs:1:1\n",
        "the entire enum is at least 32004 bytes",
        "the largest variant contains at least 32000 bytes",
        "the second-largest variant contains at least 4 bytes",
        "consider boxing the large fields or introducing indirection in some other way to reduce the total size of the enum",
        "Box<[i32; 8000]>",
    ] {
        assert!(found.contains(text), "{text}: {found}");
    }
}

#[test]
fn large_enum_variant_copy() {
    let fixture = fixture(
        "sized, copy, clone",
        "enum E {
    A(i32),
    B([i32; 8000]),
}
impl Clone for E {
    fn clone(&self) -> E { *self }
}
impl Copy for E {}",
    );
    let found = lint(&fixture, &only("large_enum_variant"));
    assert!(found.contains("boxing a variant would require the type no longer be `Copy`"), "{found}");
    assert!(!found.contains("consider boxing"), "{found}");
}

#[test]
fn large_types_passed_by_value() {
    let fixture = fixture(
        "sized, copy, clone",
        "struct Large([u8; 2048]);
impl Clone for Large {
    fn clone(&self) -> Large { *self }
}
impl Copy for Large {}
fn f(a: Large) {}
fn g(mut a: Large) {}
fn h(a: [u8; 128]) {}
fn i(a: [u8; 512]) {}
extern \"C\" fn j(a: Large) {}
pub fn exported(a: Large) {}",
    );
    let found = lint(&fixture, &only("large_types_passed_by_value"));
    assert_eq!(found.matches("is passed by value, but might be more efficient").count(), 2, "{found}");
    assert!(found.contains(
        "warning: this argument (2048 byte) is passed by value, but might be more efficient if passed by reference (limit: 256 byte)\n --> src/lib.rs:6:9\n"
    ), "{found}");
    assert!(found.contains("help: consider passing by reference instead: `&Large`"), "{found}");
    assert!(found.contains("this argument (512 byte)"), "{found}");
    assert!(found.contains("help: consider passing by reference instead: `&[u8; 512]`"), "{found}");
}

#[test]
fn needless_pass_by_value() {
    let fixture = fixture(
        "sized, copy, clone, fn",
        "fn f(v: Vec<u8>) -> usize {
    v.len()
}
fn consumed(v: Vec<u8>) -> Vec<u8> {
    v
}
fn underscore(_v: Vec<u8>) {}
fn mutable(mut v: Vec<u8>) -> usize {
    v.len()
}
fn copied(x: u32) -> u32 {
    x
}
fn callable<F: Fn()>(f: F) {
    f()
}",
    );
    expect_test::expect![[r#"
        warning: this argument is passed by value, but not consumed in the function body
         --> src/lib.rs:1:9
          |
        1 | fn f(v: Vec<u8>) -> usize {
          |         ^^^^^^^ help: consider changing the type to: `&[u8]`
          |
          = help: for further information visit https://rust-lang.github.io/rust-clippy/main/index.html#needless_pass_by_value
          = note: requested on the command line with `-W clippy::needless-pass-by-value`

    "#]]
    .assert_eq(&lint(&fixture, &only("needless_pass_by_value")));
}

#[test]
fn needless_pass_by_value_local_type() {
    let fixture = fixture(
        "sized, copy, clone, fn",
        "struct S {
    x: u32,
}
fn f(s: S) -> u32 {
    s.x
}",
    );
    let found = lint(&fixture, &only("needless_pass_by_value"));
    assert!(found.contains(
        "warning: this argument is passed by value, but not consumed in the function body\n --> src/lib.rs:4:9\n"
    ), "{found}");
    assert!(found.contains("or consider marking this type as `Copy`"), "{found}");
    assert!(found.contains("consider taking a reference instead"), "{found}");
    assert!(found.contains("4 | fn f(s: &S) -> u32 {\n  |         +\n"), "{found}");
}

#[test]
fn needless_pass_by_value_clones() {
    let fixture = fixture(
        "sized, copy, clone, fn",
        "fn f(s: String) -> usize {
    let t = s.clone();
    t.len()
}
fn g(v: Vec<u8>) -> usize {
    v.capacity()
}",
    );
    let found = lint(&fixture, &only("needless_pass_by_value"));
    assert!(found.contains("consider changing the type to"), "{found}");
    assert!(found.contains("&str"), "{found}");
    assert!(found.contains("change `s.clone()` to"), "{found}");
    assert!(found.contains("s.to_string()"), "{found}");
    // `capacity` is not a slice's: the type is not changed, a reference is suggested.
    assert!(found.contains(" --> src/lib.rs:5:9\n"), "{found}");
    assert!(found.contains("consider taking a reference instead"), "{found}");
}

#[test]
fn undocumented_unsafe_blocks() {
    let fixture = fixture(
        "sized",
        "fn f() {
    unsafe {}
}
fn g() {
    // SAFETY: nothing is done
    unsafe {}
}
fn h() {
    // SAFETY: nothing is done
    let _x = unsafe { 1 };
}
fn i() {
    /* SAFETY: a block comment */
    unsafe {}
}
fn j() {
    // SAFETY: too far
    let _y = 1;
    unsafe {}
}",
    );
    expect_test::expect![[r#"
        warning: unsafe block missing a safety comment
         --> src/lib.rs:2:5
          |
        2 |     unsafe {}
          |     ^^^^^^^^^
          |
          = help: consider adding a safety comment on the preceding line
          = help: for further information visit https://rust-lang.github.io/rust-clippy/main/index.html#undocumented_unsafe_blocks
          = note: requested on the command line with `-W clippy::undocumented-unsafe-blocks`

        warning: unsafe block missing a safety comment
          --> src/lib.rs:19:5
           |
        19 |     unsafe {}
           |     ^^^^^^^^^
           |
           = help: consider adding a safety comment on the preceding line
           = help: for further information visit https://rust-lang.github.io/rust-clippy/main/index.html#undocumented_unsafe_blocks

    "#]]
    .assert_eq(&lint(&fixture, &only("undocumented_unsafe_blocks")));
}

#[test]
fn undocumented_unsafe_blocks_multiline() {
    let fixture = fixture(
        "sized",
        "fn f() -> u32 {
    unsafe {
        1
    }
}",
    );
    let found = lint(&fixture, &only("undocumented_unsafe_blocks"));
    // The span stops at the first line's end.
    assert!(found.contains("2 |     unsafe {\n  |     ^^^^^^^^\n"), "{found}");
}

#[test]
fn undocumented_unsafe_impl() {
    let fixture = fixture(
        "sized",
        "unsafe trait Tr {}
struct A;
unsafe impl Tr for A {}
struct B;
// SAFETY: B holds nothing
unsafe impl Tr for B {}
mod m {
    unsafe impl super::Tr for super::C {}
}
struct C;",
    );
    let found = lint(&fixture, &only("undocumented_unsafe_blocks"));
    assert_eq!(found.matches("unsafe impl missing a safety comment").count(), 2, "{found}");
    assert!(found.contains(" --> src/lib.rs:3:1\n"), "{found}");
    let whole = format!("3 | unsafe impl Tr for A {{}}\n  | {}\n", "^".repeat(23));
    assert!(found.contains(&whole), "{found}");
    assert!(found.contains(" --> src/lib.rs:8:5\n"), "{found}");
    assert!(!found.contains("src/lib.rs:6:"), "{found}");
}

#[test]
fn multiple_unsafe_ops_per_block() {
    let fixture = fixture(
        "sized, copy, add",
        "unsafe fn a() {}
static mut X: u32 = 0;
fn f() {
    unsafe {
        a();
        X = 1;
    }
}
fn g() {
    unsafe { a() }
}
fn h(p: *const u32) -> u32 {
    unsafe { *p + *p }
}",
    );
    let found = lint(&fixture, &only("multiple_unsafe_ops_per_block"));
    assert_eq!(
        found.matches("this `unsafe` block contains 2 unsafe operations, expected only one").count(),
        2,
        "{found}"
    );
    for text in [
        " --> src/lib.rs:4:5\n",
        "unsafe function call occurs here",
        "modification of a mutable static occurs here",
        " --> src/lib.rs:13:5\n",
        "raw pointer dereference occurs here",
    ] {
        assert!(found.contains(text), "{text}: {found}");
    }
    assert!(!found.contains("src/lib.rs:10:5\n"), "{found}");
}
