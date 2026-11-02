//! `static_mut_refs`: rustc's `tests/ui/lint/static-mut-refs*.rs` and the UI tests of
//! `tests/ui/statics/` that give it.

use super::{Ui, ui};
use crate::tests::{Mode, assert_reports, error, warning};

#[test]
fn static_mut_shared_parens() {
    let source = r##"//Missing paren in diagnostic msg: https://github.com/rust-lang/rust/issues/131977
//@check-pass

static mut TEST: usize = 0;

fn main() {
    let _ = unsafe { (&TEST) as *const usize };
    //~^WARN creating a shared reference to mutable static

    let _ = unsafe { (&mut TEST) as *const usize };
    //~^WARN creating a mutable reference to mutable static
}
"##;
    let expected = r##"warning: creating a shared reference to mutable static
  --> $DIR/static-mut-shared-parens.rs:7:22
   |
LL |     let _ = unsafe { (&TEST) as *const usize };
   |                      ^^^^^^^ shared reference to mutable static
   |
   = note: shared references to mutable statics are dangerous; it's undefined behavior if the static is mutated or if a mutable reference is created for it while the shared reference lives
   = note: for more information, see <https://doc.rust-lang.org/edition-guide/rust-2024/static-mut-references.html>
   = note: `#[warn(static_mut_refs)]` (part of `#[warn(rust_2024_compatibility)]`) on by default
help: use `&raw const` instead to create a raw pointer
   |
LL |     let _ = unsafe { (&raw const TEST) as *const usize };
   |                        +++++++++

warning: creating a mutable reference to mutable static
  --> $DIR/static-mut-shared-parens.rs:10:22
   |
LL |     let _ = unsafe { (&mut TEST) as *const usize };
   |                      ^^^^^^^^^^^ mutable reference to mutable static
   |
   = note: mutable references to mutable statics are dangerous; it's undefined behavior if any other pointer to the static is used or if any other reference is created for the static while the mutable reference lives
   = note: for more information, see <https://doc.rust-lang.org/edition-guide/rust-2024/static-mut-references.html>
help: use `&raw mut` instead to create a raw pointer
   |
LL |     let _ = unsafe { (&raw mut TEST) as *const usize };
   |                        +++

warning: 2 warnings emitted

"##;
    let stderr = crate::tests::ui("static-mut-shared-parens.rs", "2015", source);
    assert_eq!(stderr, expected);
}

/// A borrow in the body of a static.
#[test]
fn issue_15261() {
    let source = r##"//@ build-pass
#![allow(dead_code)]
#![allow(non_upper_case_globals)]

static mut n_mut: usize = 0;

static n: &'static usize = unsafe { &n_mut };
//~^ WARN shared reference to mutable static [static_mut_refs]

fn main() {}
"##;
    let expected = r##"warning: creating a shared reference to mutable static
  --> $DIR/issue-15261.rs:7:37
   |
LL | static n: &'static usize = unsafe { &n_mut };
   |                                     ^^^^^^ shared reference to mutable static
   |
   = note: shared references to mutable statics are dangerous; it's undefined behavior if the static is mutated or if a mutable reference is created for it while the shared reference lives
   = note: for more information, see <https://doc.rust-lang.org/edition-guide/rust-2024/static-mut-references.html>
   = note: `#[warn(static_mut_refs)]` (part of `#[warn(rust_2024_compatibility)]`) on by default
help: use `&raw const` instead to create a raw pointer
   |
LL | static n: &'static usize = unsafe { &raw const n_mut };
   |                                      +++++++++

warning: 1 warning emitted

"##;
    assert_eq!(crate::tests::ui("issue-15261.rs", "2015", source), expected);
}

/// `tests/ui/lint/static-mut-refs.rs`: borrows, `let ref`, a method call, a borrow
/// `format_args!` makes and one in a macro's expansion.
const STATIC_MUT_REFS: &str = r##"// Test `static_mut_refs` lint.

//@ revisions: e2021 e2024

//@ [e2021] edition:2021
//@ [e2021] run-pass

//@ [e2024] edition:2024

static mut FOO: (u32, u32) = (1, 2);

macro_rules! bar {
    ($x:expr) => {
        &mut ($x.0)
        //[e2021]~^ WARN creating a mutable reference to mutable static [static_mut_refs]
        //[e2024]~^^ ERROR creating a mutable reference to mutable static [static_mut_refs]
    };
}

static mut STATIC: i64 = 1;

fn main() {
    static mut X: i32 = 1;

    static mut Y: i32 = 1;

    struct TheStruct {
        pub value: i32,
    }
    struct MyStruct {
        pub value: i32,
        pub s: TheStruct,
    }

    static mut A: MyStruct = MyStruct { value: 1, s: TheStruct { value: 2 } };

    unsafe {
        let _y = &X;
        //[e2021]~^ WARN shared reference to mutable static [static_mut_refs]
        //[e2024]~^^ ERROR shared reference to mutable static [static_mut_refs]

        let _y = &mut X;
        //[e2021]~^ WARN mutable reference to mutable static [static_mut_refs]
        //[e2024]~^^ ERROR mutable reference to mutable static [static_mut_refs]

        let _z = &raw mut X;

        let _p = &raw const X;

        let ref _a = X;
        //[e2021]~^ WARN shared reference to mutable static [static_mut_refs]
        //[e2024]~^^ ERROR shared reference to mutable static [static_mut_refs]

        let (_b, _c) = (&X, &Y);
        //[e2021]~^ WARN shared reference to mutable static [static_mut_refs]
        //[e2024]~^^ ERROR shared reference to mutable static [static_mut_refs]
        //[e2021]~^^^ WARN shared reference to mutable static [static_mut_refs]
        //[e2024]~^^^^ ERROR shared reference to mutable static [static_mut_refs]

        foo(&X);
        //[e2021]~^ WARN shared reference to mutable static [static_mut_refs]
        //[e2024]~^^ ERROR shared reference to mutable static [static_mut_refs]

        static mut Z: &[i32; 3] = &[0, 1, 2];

        let _ = Z.len();
        //[e2021]~^ WARN creating a shared reference to mutable static [static_mut_refs]
        //[e2024]~^^ ERROR creating a shared reference to mutable static [static_mut_refs]

        let _ = Z[0];

        let _ = format!("{:?}", Z);
        //[e2021]~^ WARN creating a shared reference to mutable static [static_mut_refs]
        //[e2024]~^^ ERROR creating a shared reference to mutable static [static_mut_refs]

        let _v = &A.value;
        //[e2021]~^ WARN creating a shared reference to mutable static [static_mut_refs]
        //[e2024]~^^ ERROR creating a shared reference to mutable static [static_mut_refs]

        let _s = &A.s.value;
        //[e2021]~^ WARN creating a shared reference to mutable static [static_mut_refs]
        //[e2024]~^^ ERROR creating a shared reference to mutable static [static_mut_refs]

        let ref _v = A.value;
        //[e2021]~^ WARN creating a shared reference to mutable static [static_mut_refs]
        //[e2024]~^^ ERROR creating a shared reference to mutable static [static_mut_refs]

        let _x = bar!(FOO);

        STATIC += 1;
    }
}

fn foo<'a>(_x: &'a i32) {}
"##;

/// What of std `static-mut-refs.rs` uses: `format!`, as alloc writes it.
const FORMAT_STD: &str = r##"
pub mod fmt {
    pub use core::fmt::*;

    #[must_use]
    #[inline]
    pub fn format(args: Arguments<'_>) -> crate::string::String {
        loop {}
    }
}

pub mod string {
    #[rustc_diagnostic_item = "String"]
    pub struct String {
        vec: (),
    }
}

#[doc(hidden)]
pub mod __export {
    pub use core::format_args;

    #[must_use]
    #[inline]
    pub const fn must_use<T>(arg: T) -> T {
        arg
    }
}

#[macro_export]
#[allow_internal_unstable(hint_must_use, liballoc_internals)]
#[rustc_diagnostic_item = "format_macro"]
macro_rules! format {
    ($($arg:tt)*) => {
        $crate::__export::must_use({
            let res = $crate::fmt::format($crate::__export::format_args!($($arg)*));
            res
        })
    }
}
"##;

const STATIC_MUT_REFS_FLAGS: &str = "add, builtin_impls, coerce_unsized, fmt, index, slice, unsize";

#[test]
fn static_mut_refs_e2021() {
    let stderr = ui(Ui {
        name: "static-mut-refs.rs",
        edition: "2021",
        mode: Mode::RunPass,
        core: STATIC_MUT_REFS_FLAGS,
        std: Some(("pub use crate::format;", FORMAT_STD)),
        source: STATIC_MUT_REFS,
        ..Ui::default()
    });
    assert_reports(
        &stderr,
        &[
            warning(38, &[]),
            warning(42, &[]),
            warning(50, &[]),
            warning(54, &[]),
            warning(54, &[]),
            warning(60, &[]),
            warning(66, &[]),
            warning(72, &[]),
            warning(76, &[]),
            warning(80, &[]),
            warning(84, &[]),
            warning(14, &[]),
        ],
    );
}

/// The e2024 revision: the e2021 revision's references, each an error, `static_mut_refs` being
/// `deny` in edition 2024. Only the e2021 revision is `run-pass`.
#[test]
fn static_mut_refs_e2024() {
    let stderr = ui(Ui {
        name: "static-mut-refs.rs",
        edition: "2024",
        core: STATIC_MUT_REFS_FLAGS,
        std: Some(("pub use crate::format;", FORMAT_STD)),
        source: STATIC_MUT_REFS,
        ..Ui::default()
    });
    assert_reports(
        &stderr,
        &[
            error(38, &[]),
            error(42, &[]),
            error(50, &[]),
            error(54, &[]),
            error(54, &[]),
            error(60, &[]),
            error(66, &[]),
            error(72, &[]),
            error(76, &[]),
            error(80, &[]),
            error(84, &[]),
            error(14, &[]),
        ],
    );
}

/// What of std the interior mutability tests use: `std::sync::Mutex`, as std writes it, its
/// lock and poisoning reduced to what the tests reach.
const MUTEX_STD: &str = r##"
pub mod sync {
    use core::cell::UnsafeCell;
    use core::fmt;

    #[rustc_diagnostic_item = "Mutex"]
    pub struct Mutex<T: ?Sized> {
        inner: (),
        poison: bool,
        data: UnsafeCell<T>,
    }

    impl<T> Mutex<T> {
        #[inline]
        pub const fn new(t: T) -> Mutex<T> {
            Mutex { inner: (), poison: false, data: UnsafeCell::new(t) }
        }
    }

    impl<T: ?Sized> Mutex<T> {
        pub fn lock(&self) -> LockResult<MutexGuard<'_, T>> {
            loop {}
        }
    }

    #[must_use = "if unused the Mutex will immediately unlock"]
    #[rustc_diagnostic_item = "MutexGuard"]
    pub struct MutexGuard<'a, T: ?Sized + 'a> {
        lock: &'a Mutex<T>,
        poison: bool,
    }

    pub struct PoisonError<T> {
        data: T,
    }

    impl<T> fmt::Debug for PoisonError<T> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            loop {}
        }
    }

    pub type LockResult<T> = Result<T, PoisonError<T>>;
}
"##;

const MUTEX_FLAGS: &str = "cell, fmt, result";

#[test]
fn static_mut_refs_interior_mutability() {
    let source = r##"//@ edition:2024
//@ run-rustfix

#![allow(unused_unsafe)]

use std::sync::Mutex;

static mut STDINOUT_MUTEX: Mutex<bool> = Mutex::new(false);

fn main() {
    let _lock = unsafe { STDINOUT_MUTEX.lock().unwrap() };
    //~^ ERROR creating a shared reference to mutable static [static_mut_refs]
}
"##;
    let expected = r##"error: creating a shared reference to mutable static
  --> $DIR/static-mut-refs-interior-mutability.rs:11:26
   |
LL |     let _lock = unsafe { STDINOUT_MUTEX.lock().unwrap() };
   |                          ^^^^^^^^^^^^^^^^^^^^^ shared reference to mutable static
   |
   = note: shared references to mutable statics are dangerous; it's undefined behavior if the static is mutated or if a mutable reference is created for it while the shared reference lives
   = note: for more information, see <https://doc.rust-lang.org/edition-guide/rust-2024/static-mut-references.html>
   = note: `#[deny(static_mut_refs)]` (part of `#[deny(rust_2024_compatibility)]`) on by default
help: this type already provides "interior mutability", so its binding doesn't need to be declared as mutable when borrowed with a shared reference
   |
LL - static mut STDINOUT_MUTEX: Mutex<bool> = Mutex::new(false);
LL + static STDINOUT_MUTEX: Mutex<bool> = Mutex::new(false);
   |

error: aborting due to 1 previous error

"##;
    let stderr = ui(Ui {
        name: "static-mut-refs-interior-mutability.rs",
        edition: "2024",
        core: MUTEX_FLAGS,
        std: Some(("", MUTEX_STD)),
        source,
        ..Ui::default()
    });
    assert_eq!(stderr, expected);
}

/// A static written by a macro: no suggestion to drop its `mut`.
#[test]
fn static_mut_refs_interior_mutability_no_sugg() {
    let source = r##"//@ edition:2024
// Ensure that we don't make structured suggestions for interior mutability note
// when span is not available.

use std::sync::Mutex;

macro_rules! declare_mutex {
    () => {
        static mut MACRO_MUTEX: Mutex<bool> = Mutex::new(false);
    };
}

declare_mutex!();

fn main() {
    let _lock = unsafe { MACRO_MUTEX.lock().unwrap() };
    //~^ ERROR creating a shared reference to mutable static [static_mut_refs]
}
"##;
    let expected = r##"error: creating a shared reference to mutable static
  --> $DIR/static-mut-refs-interior-mutability-no-sugg.rs:16:26
   |
LL |     let _lock = unsafe { MACRO_MUTEX.lock().unwrap() };
   |                          ^^^^^^^^^^^^^^^^^^ shared reference to mutable static
   |
   = note: shared references to mutable statics are dangerous; it's undefined behavior if the static is mutated or if a mutable reference is created for it while the shared reference lives
   = note: for more information, see <https://doc.rust-lang.org/edition-guide/rust-2024/static-mut-references.html>
   = note: `#[deny(static_mut_refs)]` (part of `#[deny(rust_2024_compatibility)]`) on by default

error: aborting due to 1 previous error

"##;
    let stderr = ui(Ui {
        name: "static-mut-refs-interior-mutability-no-sugg.rs",
        edition: "2024",
        core: MUTEX_FLAGS,
        std: Some(("", MUTEX_STD)),
        source,
        ..Ui::default()
    });
    assert_eq!(stderr, expected);
}
