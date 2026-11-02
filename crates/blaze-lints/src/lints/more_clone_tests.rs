//! `redundant_clone`, `clone_on_ref_ptr`, `implicit_clone`, `unnecessary_to_owned`,
//! `inefficient_to_string`, `format_collect` and `str_to_string` over clippy's UI test cases,
//! each alone on the command line (`-A clippy::all -W clippy::<lint>`), with an `alloc` and a
//! `std` of the fixtures' own: minicore has `core` only.

use crate::{
    conf::{Conf, Version},
    tests::{lint, lint_with},
};

const MINICORE: &str = "//- minicore: sized, clone, copy, deref, as_ref, iterator, iterators, fn, option, result, builtin_impls\n";

/// `alloc` with the owned types the lints ask of, at their paths, and a `std` that re-exports it.
/// minicore's `core` has `AsRef` without impls and `Result` without `unwrap`: `alloc` gives
/// `str`'s and references' `AsRef`, `std`'s prelude an `unwrap` by a trait.
const ALLOC_STD: &str = r#"
//- /alloc/lib.rs crate:alloc
pub mod borrow {
    pub trait ToOwned {
        type Owned;
        fn to_owned(&self) -> Self::Owned;
    }
    impl<T: Clone> ToOwned for T {
        type Owned = T;
        fn to_owned(&self) -> T {
            self.clone()
        }
    }
    impl ToOwned for str {
        type Owned = crate::string::String;
        fn to_owned(&self) -> crate::string::String {
            crate::string::String::new()
        }
    }
    pub enum Cow<'a, B: ?Sized + ToOwned + 'a> {
        Borrowed(&'a B),
        Owned(<B as ToOwned>::Owned),
    }
    impl<B: ?Sized + ToOwned> Cow<'_, B> {
        pub fn into_owned(self) -> <B as ToOwned>::Owned {
            loop {}
        }
    }
}
pub mod convert {
    impl AsRef<str> for str {
        fn as_ref(&self) -> &str {
            self
        }
    }
    impl<T: ?Sized + AsRef<U>, U: ?Sized> AsRef<U> for &T {
        fn as_ref(&self) -> &U {
            (**self).as_ref()
        }
    }
}
pub mod string {
    pub struct String {
        vec: crate::vec::Vec<u8>,
    }
    pub struct FromUtf8Error;
    impl String {
        pub const fn new() -> String {
            String { vec: crate::vec::Vec::new() }
        }
        pub fn from_utf8(vec: crate::vec::Vec<u8>) -> Result<String, FromUtf8Error> {
            Ok(String { vec })
        }
        pub fn as_str(&self) -> &str {
            loop {}
        }
        pub fn len(&self) -> usize {
            0
        }
    }
    impl Clone for String {
        fn clone(&self) -> String {
            String::new()
        }
    }
    impl core::ops::Deref for String {
        type Target = str;
        fn deref(&self) -> &str {
            loop {}
        }
    }
    impl AsRef<str> for String {
        fn as_ref(&self) -> &str {
            loop {}
        }
    }
    impl core::iter::FromIterator<String> for String {
        fn from_iter<I: IntoIterator<Item = String>>(_iter: I) -> String {
            String::new()
        }
    }
    pub trait ToString {
        fn to_string(&self) -> String;
    }
    impl ToString for str {
        fn to_string(&self) -> String {
            String::new()
        }
    }
    impl ToString for String {
        fn to_string(&self) -> String {
            String::new()
        }
    }
    impl<T: ?Sized + ToString> ToString for &T {
        fn to_string(&self) -> String {
            String::new()
        }
    }
}
pub mod vec {
    pub struct Vec<T> {
        ptr: *mut T,
        len: usize,
    }
    impl<T> Vec<T> {
        pub const fn new() -> Vec<T> {
            Vec { ptr: 0 as *mut T, len: 0 }
        }
    }
    impl<T: Clone> Clone for Vec<T> {
        fn clone(&self) -> Vec<T> {
            Vec::new()
        }
    }
    impl<T> core::ops::Deref for Vec<T> {
        type Target = [T];
        fn deref(&self) -> &[T] {
            loop {}
        }
    }
    pub struct IntoIter<T> {
        ptr: *mut T,
    }
    impl<T> Iterator for IntoIter<T> {
        type Item = T;
        fn next(&mut self) -> Option<T> {
            None
        }
    }
    impl<T> IntoIterator for Vec<T> {
        type Item = T;
        type IntoIter = IntoIter<T>;
        fn into_iter(self) -> IntoIter<T> {
            IntoIter { ptr: self.ptr }
        }
    }
}
pub mod slice {
    impl<T> [T] {
        #[rustc_allow_incoherent_impl]
        pub fn to_vec(&self) -> crate::vec::Vec<T>
        where
            T: Clone,
        {
            crate::vec::Vec::new()
        }
    }
}
pub mod rc {
    pub struct Rc<T: ?Sized> {
        ptr: *const T,
    }
    impl<T: ?Sized> Clone for Rc<T> {
        fn clone(&self) -> Rc<T> {
            Rc { ptr: self.ptr }
        }
    }
    pub struct Weak<T: ?Sized> {
        ptr: *const T,
    }
    impl<T: ?Sized> Clone for Weak<T> {
        fn clone(&self) -> Weak<T> {
            Weak { ptr: self.ptr }
        }
    }
}
pub mod sync {
    pub struct Arc<T: ?Sized> {
        ptr: *const T,
    }
    impl<T: ?Sized> Clone for Arc<T> {
        fn clone(&self) -> Arc<T> {
            Arc { ptr: self.ptr }
        }
    }
}
//- /std/lib.rs crate:std deps:alloc
pub use alloc::{borrow, rc, string, sync, vec};
pub mod prelude {
    pub mod v1 {
        pub use crate::{format, println, Unwrap};
        pub use alloc::borrow::ToOwned;
        pub use alloc::string::{String, ToString};
        pub use alloc::vec::Vec;
        pub use core::prelude::v1::*;
    }
    pub mod rust_2015 { pub use super::v1::*; }
    pub mod rust_2018 { pub use super::v1::*; }
    pub mod rust_2021 { pub use super::v1::*; }
    pub mod rust_2024 { pub use super::v1::*; }
}
pub trait Unwrap<T> {
    fn unwrap(self) -> T;
}
impl<T, E> Unwrap<T> for Result<T, E> {
    fn unwrap(self) -> T {
        loop {}
    }
}
#[macro_export]
macro_rules! println {
    ($($arg:tt)*) => {
        ()
    };
}
#[macro_export]
macro_rules! format {
    ($($arg:tt)*) => {
        $crate::string::String::new()
    };
}
"#;

/// The crate `main`, written as `main`, with the fixtures' `core`, `alloc` and `std`.
fn fixture(main: &str) -> String {
    let main = main.trim_start_matches('\n');
    format!("\n{MINICORE}//- /src/lib.rs crate:main deps:std,alloc\n{main}{ALLOC_STD}")
}

/// `lint` alone on the command line.
fn only(lint: &str) -> String {
    format!("-A clippy::all -W clippy::{lint}")
}

#[test]
fn redundant_clone() {
    let main = r#"
fn dropped(s: String) -> String {
    s.clone()
}
fn used_after(s: String) -> (String, String) {
    let t = s.clone();
    (t, s)
}
fn borrowed_before(s: String) -> (usize, String) {
    let n = s.len();
    let t = s.clone();
    (n, t)
}
fn kept_borrow(s: String) -> String {
    let r = s.as_str();
    let t = s.clone();
    let _ = r;
    t
}
fn in_loop(s: String) {
    loop {
        let _t = s.clone();
    }
}
fn printed(s: String) -> String {
    println!("{}", s);
    s.to_string()
}
fn named_in_macro_after(s: String) -> String {
    let t = s.to_owned();
    let _ = format!("{s}");
    t
}
fn copy(n: u32) -> u32 {
    n.clone()
}
"#;
    expect_test::expect![[r#"
        warning: redundant clone
         --> src/lib.rs:2:6
          |
        2 |     s.clone()
          |      ^^^^^^^^ help: remove this
          |
        note: this value is dropped without further use
         --> src/lib.rs:2:5
          |
        2 |     s.clone()
          |     ^
          = help: for further information visit https://rust-lang.github.io/rust-clippy/main/index.html#redundant_clone
          = note: requested on the command line with `-W clippy::redundant-clone`

        warning: redundant clone
          --> src/lib.rs:10:14
           |
        10 |     let t = s.clone();
           |              ^^^^^^^^ help: remove this
           |
        note: this value is dropped without further use
          --> src/lib.rs:10:13
           |
        10 |     let t = s.clone();
           |             ^
           = help: for further information visit https://rust-lang.github.io/rust-clippy/main/index.html#redundant_clone

        warning: redundant clone
          --> src/lib.rs:26:6
           |
        26 |     s.to_string()
           |      ^^^^^^^^^^^^ help: remove this
           |
        note: this value is dropped without further use
          --> src/lib.rs:26:5
           |
        26 |     s.to_string()
           |     ^
           = help: for further information visit https://rust-lang.github.io/rust-clippy/main/index.html#redundant_clone

    "#]]
    .assert_eq(&lint(&fixture(main), &only("redundant_clone")));
}

#[test]
fn clone_on_ref_ptr() {
    let main = r#"
use std::rc::{Rc, Weak};
use std::sync::Arc;
fn f(rc: Rc<i32>, arc: Arc<u8>, weak: Weak<i32>) {
    let _ = rc.clone();
    let _ = arc.clone();
    let _ = weak.clone();
    let _ = Rc::clone(&rc);
}
"#;
    expect_test::expect![[r#"
        warning: using `.clone()` on a ref-counted pointer
         --> src/lib.rs:4:13
          |
        4 |     let _ = rc.clone();
          |             ^^^^^^^^^^ help: try: `Rc::<i32>::clone(&rc)`
          |
          = help: for further information visit https://rust-lang.github.io/rust-clippy/main/index.html#clone_on_ref_ptr
          = note: requested on the command line with `-W clippy::clone-on-ref-ptr`

        warning: using `.clone()` on a ref-counted pointer
         --> src/lib.rs:5:13
          |
        5 |     let _ = arc.clone();
          |             ^^^^^^^^^^^ help: try: `Arc::<u8>::clone(&arc)`
          |
          = help: for further information visit https://rust-lang.github.io/rust-clippy/main/index.html#clone_on_ref_ptr

        warning: using `.clone()` on a ref-counted pointer
         --> src/lib.rs:6:13
          |
        6 |     let _ = weak.clone();
          |             ^^^^^^^^^^^^ help: try: `Weak::<i32>::clone(&weak)`
          |
          = help: for further information visit https://rust-lang.github.io/rust-clippy/main/index.html#clone_on_ref_ptr

    "#]]
    .assert_eq(&lint(&fixture(main), &only("clone_on_ref_ptr")));
}

#[test]
fn implicit_clone() {
    let main = r#"
fn f(s: String, v: Vec<u8>, r: &String) {
    let _ = s.to_owned();
    let _ = v.to_vec();
    let _ = r.to_owned();
    let _ = "a".to_owned();
}
"#;
    expect_test::expect![[r#"
        warning: implicitly cloning a `String` by calling `to_owned` on its dereferenced type
         --> src/lib.rs:2:13
          |
        2 |     let _ = s.to_owned();
          |             ^^^^^^^^^^^^ help: consider using: `s.clone()`
          |
          = help: for further information visit https://rust-lang.github.io/rust-clippy/main/index.html#implicit_clone
          = note: requested on the command line with `-W clippy::implicit-clone`

        warning: implicitly cloning a `Vec` by calling `to_vec` on its dereferenced type
         --> src/lib.rs:3:13
          |
        3 |     let _ = v.to_vec();
          |             ^^^^^^^^^^ help: consider using: `v.clone()`
          |
          = help: for further information visit https://rust-lang.github.io/rust-clippy/main/index.html#implicit_clone

    "#]]
    .assert_eq(&lint(&fixture(main), &only("implicit_clone")));
}

#[test]
fn unnecessary_to_owned() {
    let main = r#"
fn takes_str(_: &str) {}
fn takes_slice(_: &[u8]) {}
fn takes_ref<T: AsRef<str>>(_: T) {}
fn f(s: &str, owned: String, v: &[u8], bytes: Vec<u8>) {
    takes_str(&s.to_string());
    takes_slice(&v.to_vec());
    takes_str(&owned.to_string());
    takes_ref(s.to_string());
    let _ = bytes.to_vec().into_iter();
    let _ = String::from_utf8(v.to_vec()).unwrap();
    let _: &str = &String::from_utf8(v.to_vec()).unwrap();
}
fn g(s: &str) {
    let _ = s.to_string().split(',');
}
"#;
    expect_test::expect![[r#"
        warning: unnecessary use of `to_string`
         --> src/lib.rs:5:15
          |
        5 |     takes_str(&s.to_string());
          |               ^^^^^^^^^^^^^^ help: use: `s`
          |
          = help: for further information visit https://rust-lang.github.io/rust-clippy/main/index.html#unnecessary_to_owned
          = note: requested on the command line with `-W clippy::unnecessary-to-owned`

        warning: unnecessary use of `to_vec`
         --> src/lib.rs:6:17
          |
        6 |     takes_slice(&v.to_vec());
          |                 ^^^^^^^^^^^ help: use: `v`
          |
          = help: for further information visit https://rust-lang.github.io/rust-clippy/main/index.html#unnecessary_to_owned

        warning: unnecessary use of `to_string`
         --> src/lib.rs:8:15
          |
        8 |     takes_ref(s.to_string());
          |               ^^^^^^^^^^^^^ help: use: `s`
          |
          = help: for further information visit https://rust-lang.github.io/rust-clippy/main/index.html#unnecessary_to_owned

        warning: unnecessary use of `to_vec`
         --> src/lib.rs:9:13
          |
        9 |     let _ = bytes.to_vec().into_iter();
          |             ^^^^^^^^^^^^^^^^^^^^^^^^^^ help: use: `bytes.iter().copied()`
          |
          = help: for further information visit https://rust-lang.github.io/rust-clippy/main/index.html#unnecessary_to_owned

        warning: allocating a new `String` only to create a temporary `&str` from it
          --> src/lib.rs:11:20
           |
        11 |     let _: &str = &String::from_utf8(v.to_vec()).unwrap();
           |                    ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
           |
           = help: for further information visit https://rust-lang.github.io/rust-clippy/main/index.html#unnecessary_to_owned
        help: convert from `&[u8]` to `&str` directly
           |
        11 -     let _: &str = &String::from_utf8(v.to_vec()).unwrap();
        11 +     let _: &str = core::str::from_utf8(v).unwrap();
           |

        warning: unnecessary use of `to_string`
          --> src/lib.rs:14:13
           |
        14 |     let _ = s.to_string().split(',');
           |             ^^^^^^^^^^^^^^^^^^^^^^^^ help: use: `s.split(',')`
           |
           = help: for further information visit https://rust-lang.github.io/rust-clippy/main/index.html#unnecessary_to_owned

    "#]]
    .assert_eq(&lint(&fixture(main), &only("unnecessary_to_owned")));
}

#[test]
fn inefficient_to_string() {
    let main = r#"
fn f(rstr: &&str, rstring: &String, s: &str) {
    let _ = rstr.to_string();
    let _ = rstring.to_string();
    let _ = s.to_string();
}
"#;
    let before_specialization = Conf { msrv: Some(Version(1, 81, 0)), ..Conf::default() };
    expect_test::expect![[r#"
        warning: calling `to_string` on `&&str`
         --> src/lib.rs:2:13
          |
        2 |     let _ = rstr.to_string();
          |             ^^^^^^^^^^^^^^^^ help: try dereferencing the receiver: `(*rstr).to_string()`
          |
          = help: `&str` implements `ToString` through a slower blanket impl, but `str` has a fast specialization of `ToString`
          = help: for further information visit https://rust-lang.github.io/rust-clippy/main/index.html#inefficient_to_string
          = note: requested on the command line with `-W clippy::inefficient-to-string`

    "#]]
    .assert_eq(&lint_with(
        &fixture(main),
        &only("inefficient_to_string"),
        before_specialization,
    ));
    // From Rust 1.82 `ToString` of a reference reaches the specialization: no version set keeps
    // to the newest.
    expect_test::expect![[""]].assert_eq(&lint(&fixture(main), &only("inefficient_to_string")));
}

#[test]
fn format_collect() {
    let main = r#"
fn f(v: Vec<u8>) -> String {
    v.into_iter().map(|b| format!("{b:02X}")).collect()
}
"#;
    expect_test::expect![[r#"
        warning: use of `format` to build up a string from an iterator
         --> src/lib.rs:2:5
          |
        2 |     v.into_iter().map(|b| format!("{b:02X}")).collect()
          |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
          |
        help: call `fold` instead
         --> src/lib.rs:2:19
          |
        2 |     v.into_iter().map(|b| format!("{b:02X}")).collect()
          |                   ^^^
        note: ... and use the `write!` macro here
         --> src/lib.rs:2:27
          |
        2 |     v.into_iter().map(|b| format!("{b:02X}")).collect()
          |                           ^^^^^^^^^^^^^^^^^^
          = help: for further information visit https://rust-lang.github.io/rust-clippy/main/index.html#format_collect
          = note: requested on the command line with `-W clippy::format-collect`

    "#]]
    .assert_eq(&lint(&fixture(main), &only("format_collect")));
}

#[test]
fn str_to_string() {
    let main = r#"
fn f(s: &str, t: String) {
    let _ = s.to_string();
    let _ = "x".to_string();
    let _ = t.to_string();
}
"#;
    expect_test::expect![[r#"
        warning: `to_string()` called on a `&str`
         --> src/lib.rs:2:13
          |
        2 |     let _ = s.to_string();
          |             ^^^^^^^^^^^^^ help: try: `s.to_owned()`
          |
          = help: for further information visit https://rust-lang.github.io/rust-clippy/main/index.html#str_to_string
          = note: requested on the command line with `-W clippy::str-to-string`

        warning: `to_string()` called on a `&str`
         --> src/lib.rs:3:13
          |
        3 |     let _ = "x".to_string();
          |             ^^^^^^^^^^^^^^^ help: try: `"x".to_owned()`
          |
          = help: for further information visit https://rust-lang.github.io/rust-clippy/main/index.html#str_to_string

    "#]]
    .assert_eq(&lint(&fixture(main), &only("str_to_string")));
}
