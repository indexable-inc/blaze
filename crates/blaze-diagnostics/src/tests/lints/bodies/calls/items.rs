//! rustc's UI tests of `DropForgetUseless`, `InvalidFromUtf8` and `InvalidAtomicOrdering`:
//! each test's source and `.stderr` as they are in `tests/ui/lint/` (in `items/ui/`), compiled
//! against a `std` of the items of std it uses, written as std writes them, with their
//! diagnostic items and lang items.

use crate::{
    Options,
    emit::Style,
    tests::{Mode, compiletest},
};

/// A `std` prelude: core's, with `extra` (`pub use` lines) for what std's prelude has that
/// core's does not, which shadow core's of the same name.
fn prelude(extra: &str) -> String {
    let mut out = String::from("pub mod prelude {\n");
    for edition in ["rust_2015", "rust_2018", "rust_2021", "rust_2024"] {
        out.push_str(&format!("    pub mod {edition} {{\n        pub use core::prelude::{edition}::*;\n{extra}    }}\n"));
    }
    out.push_str("}\n");
    out
}

/// What rustc writes compiling the UI test `name` (2015 edition, as the tests say none; none is
/// `run-pass` and none has `//@ compile-flags`), core being minicore with `flags`, std the
/// prelude of [`prelude`] with `extra` and `std_items`, and the library features of std's
/// `library` ([`crate::tests::library_features`]).
fn ui_std(name: &str, (flags, library): (&str, &[&str]), extra: &str, std_items: &str, source: &str) -> String {
    let (library, deps) = match library {
        [] => (String::new(), String::new()),
        _ => (crate::tests::library_features(library), format!(",{}", crate::tests::LIBRARY_FEATURES)),
    };
    let fixture = format!(
        "//- minicore: {flags}\n//- /std.rs crate:std\n#![feature(lang_items, rustc_attrs)]\n{}{std_items}\n{library}\
         //- /{name} crate:other:main edition:2015 deps:std{deps}\n{source}",
        prelude(extra)
    );
    let stderr = crate::tests::checked_with(&fixture, "$DIR/", &compiletest(Mode::Other, &Options::default()))
        .stderr_with(Style { ui_testing: true, ..Style::default() });
    crate::tests::normalized(&stderr)
}

macro_rules! ui {
    ($test:ident, $group:ident) => {
        #[test]
        fn $test() {
            let name = concat!(stringify!($test), ".rs");
            let source = include_str!(concat!("items/ui/", stringify!($test), ".rs"));
            let expected = include_str!(concat!("items/ui/", stringify!($test), ".stderr"));
            assert_eq!(ui_std(name, ($group::FLAGS, $group::LIBRARY), $group::PRELUDE, $group::ITEMS, source), expected);
        }
    };
    ($test:ident, $file:literal, $group:ident) => {
        #[test]
        fn $test() {
            let source = include_str!(concat!("items/ui/", $file, ".rs"));
            let expected = include_str!(concat!("items/ui/", $file, ".stderr"));
            assert_eq!(ui_std(concat!($file, ".rs"), ($group::FLAGS, $group::LIBRARY), $group::PRELUDE, $group::ITEMS, source), expected);
        }
    };
}

/// std's `drop`, `forget`, `ManuallyDrop`, `Result` (with `map`, `map_err` and `?`), `String`,
/// `Vec`, `vec!` and `println!`, for the tests of `DropForgetUseless`. Core is minicore without
/// its `Result`, which std's stands for here as core's does in rustc's.
mod mem {
    pub(super) const FLAGS: &str = "sized, copy, clone, derive, eq, drop, fn, try";

    pub(super) const LIBRARY: &[&str] = &[];

    pub(super) const PRELUDE: &str = "\
        pub use crate::mem::drop;
        pub use crate::result::Result::{self, Err, Ok};
        pub use crate::string::String;
        pub use crate::vec::Vec;
        pub use crate::{println, vec};
";

    pub(super) const ITEMS: &str = r#"
pub mod mem {
    #[rustc_diagnostic_item = "mem_forget"]
    pub const fn forget<T>(t: T) {
        let _ = ManuallyDrop::new(t);
    }

    #[rustc_diagnostic_item = "mem_drop"]
    pub fn drop<T>(_x: T) {}

    #[lang = "manually_drop"]
    #[repr(transparent)]
    pub struct ManuallyDrop<T: ?Sized> {
        value: T,
    }

    impl<T> ManuallyDrop<T> {
        pub const fn new(value: T) -> ManuallyDrop<T> {
            ManuallyDrop { value }
        }

        pub const fn into_inner(slot: ManuallyDrop<T>) -> T {
            loop {}
        }
    }

    impl<T: ?Sized> ManuallyDrop<T> {
        pub unsafe fn drop(slot: &mut ManuallyDrop<T>) {}
    }
}

pub mod result {
    use core::convert::Infallible;
    use core::ops::{ControlFlow, FromResidual, Try};

    pub enum Result<T, E> {
        #[lang = "Ok"]
        Ok(T),
        #[lang = "Err"]
        Err(E),
    }

    use self::Result::{Err, Ok};

    impl<T, E> Result<T, E> {
        pub fn map<U, F: FnOnce(T) -> U>(self, op: F) -> Result<U, E> {
            match self {
                Ok(t) => Ok(op(t)),
                Err(e) => Err(e),
            }
        }

        pub fn map_err<F, O: FnOnce(E) -> F>(self, op: O) -> Result<T, F> {
            match self {
                Ok(t) => Ok(t),
                Err(e) => Err(op(e)),
            }
        }
    }

    impl<T, E> Try for Result<T, E> {
        type Output = T;
        type Residual = Result<Infallible, E>;

        fn from_output(output: T) -> Self {
            Ok(output)
        }

        fn branch(self) -> ControlFlow<Result<Infallible, E>, T> {
            match self {
                Ok(v) => ControlFlow::Continue(v),
                Err(e) => ControlFlow::Break(Err(e)),
            }
        }
    }

    impl<T, E> FromResidual<Result<Infallible, E>> for Result<T, E> {
        fn from_residual(residual: Result<Infallible, E>) -> Self {
            match residual {
                Err(e) => Err(e),
                Ok(_) => loop {},
            }
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
            loop {}
        }

        pub fn from_array<const N: usize>(elements: [T; N]) -> Vec<T> {
            loop {}
        }
    }

    impl<T: Clone> Clone for Vec<T> {
        fn clone(&self) -> Vec<T> {
            loop {}
        }
    }
}

pub mod string {
    pub struct String {
        vec: crate::vec::Vec<u8>,
    }

    impl String {
        pub const fn new() -> String {
            loop {}
        }
    }
}

#[macro_export]
macro_rules! vec {
    ($($x:expr),* $(,)?) => {
        $crate::vec::Vec::from_array([$($x),*])
    };
}

#[macro_export]
macro_rules! println {
    ($($arg:tt)*) => {{}};
}
"#;
}

/// std's `str::from_utf8` and its forms, free and inherent, and `concat_bytes!`, for the
/// tests of `InvalidFromUtf8`.
mod utf8 {
    pub(super) const FLAGS: &str = "sized, copy, coerce_unsized, result";

    pub(super) const LIBRARY: &[&str] = &["concat_bytes"];

    pub(super) const PRELUDE: &str = "\
        pub use crate::concat_bytes;
";

    pub(super) const ITEMS: &str = r#"
pub mod str {
    use core::result::Result;

    pub struct Utf8Error {
        valid_up_to: usize,
    }

    #[rustc_diagnostic_item = "str_from_utf8"]
    pub const fn from_utf8(v: &[u8]) -> Result<&str, Utf8Error> {
        loop {}
    }

    #[rustc_diagnostic_item = "str_from_utf8_mut"]
    pub const fn from_utf8_mut(v: &mut [u8]) -> Result<&mut str, Utf8Error> {
        loop {}
    }

    #[rustc_diagnostic_item = "str_from_utf8_unchecked"]
    pub const unsafe fn from_utf8_unchecked(v: &[u8]) -> &str {
        loop {}
    }

    #[rustc_diagnostic_item = "str_from_utf8_unchecked_mut"]
    pub const unsafe fn from_utf8_unchecked_mut(v: &mut [u8]) -> &mut str {
        loop {}
    }

    impl str {
        #[rustc_allow_incoherent_impl]
        #[rustc_diagnostic_item = "str_inherent_from_utf8"]
        pub const fn from_utf8(v: &[u8]) -> Result<&str, Utf8Error> {
            loop {}
        }

        #[rustc_allow_incoherent_impl]
        #[rustc_diagnostic_item = "str_inherent_from_utf8_mut"]
        pub const fn from_utf8_mut(v: &mut [u8]) -> Result<&mut str, Utf8Error> {
            loop {}
        }

        #[rustc_allow_incoherent_impl]
        #[rustc_diagnostic_item = "str_inherent_from_utf8_unchecked"]
        pub const unsafe fn from_utf8_unchecked(v: &[u8]) -> &str {
            loop {}
        }

        #[rustc_allow_incoherent_impl]
        #[rustc_diagnostic_item = "str_inherent_from_utf8_unchecked_mut"]
        pub const unsafe fn from_utf8_unchecked_mut(v: &mut [u8]) -> &mut str {
            loop {}
        }
    }
}

#[rustc_builtin_macro]
#[macro_export]
macro_rules! concat_bytes {
    ($($e:literal),+ $(,)?) => {{ /* compiler built-in */ }};
}
"#;
}

/// std's atomics, their `Ordering` and fences, for the tests of `InvalidAtomicOrdering`.
mod atomic {
    pub(super) const FLAGS: &str = "sized, copy, fn, option, result, add, builtin_impls";

    pub(super) const LIBRARY: &[&str] = &[];

    pub(super) const PRELUDE: &str = "";

    pub(super) const ITEMS: &str = r#"
pub mod sync {
    pub mod atomic {
        use core::option::Option;
        use core::result::Result;

        #[rustc_diagnostic_item = "Atomic"]
        #[repr(C)]
        pub struct Atomic<T> {
            v: T,
        }

        pub type AtomicBool = Atomic<bool>;

        pub type AtomicPtr<T> = Atomic<*mut T>;

        #[rustc_diagnostic_item = "Ordering"]
        #[non_exhaustive]
        pub enum Ordering {
            Relaxed,
            Release,
            Acquire,
            AcqRel,
            SeqCst,
        }

        impl AtomicBool {
            pub const fn new(v: bool) -> AtomicBool {
                loop {}
            }

            pub fn load(&self, order: Ordering) -> bool {
                loop {}
            }

            pub fn store(&self, val: bool, order: Ordering) {
                loop {}
            }
        }

        impl<T> AtomicPtr<T> {
            pub const fn new(p: *mut T) -> AtomicPtr<T> {
                loop {}
            }

            pub fn load(&self, order: Ordering) -> *mut T {
                loop {}
            }

            pub fn store(&self, ptr: *mut T, order: Ordering) {
                loop {}
            }

            pub fn compare_exchange(
                &self,
                current: *mut T,
                new: *mut T,
                success: Ordering,
                failure: Ordering,
            ) -> Result<*mut T, *mut T> {
                loop {}
            }

            pub fn compare_exchange_weak(
                &self,
                current: *mut T,
                new: *mut T,
                success: Ordering,
                failure: Ordering,
            ) -> Result<*mut T, *mut T> {
                loop {}
            }
        }

        macro_rules! atomic_int {
            ($($atomic_type:ident $int_type:ident)*) => {$(
                pub type $atomic_type = Atomic<$int_type>;

                impl $atomic_type {
                    pub const fn new(v: $int_type) -> Self {
                        loop {}
                    }

                    pub fn load(&self, order: Ordering) -> $int_type {
                        loop {}
                    }

                    pub fn store(&self, val: $int_type, order: Ordering) {
                        loop {}
                    }

                    pub fn compare_exchange(
                        &self,
                        current: $int_type,
                        new: $int_type,
                        success: Ordering,
                        failure: Ordering,
                    ) -> Result<$int_type, $int_type> {
                        loop {}
                    }

                    pub fn compare_exchange_weak(
                        &self,
                        current: $int_type,
                        new: $int_type,
                        success: Ordering,
                        failure: Ordering,
                    ) -> Result<$int_type, $int_type> {
                        loop {}
                    }

                    pub fn fetch_update<F>(
                        &self,
                        set_order: Ordering,
                        fetch_order: Ordering,
                        f: F,
                    ) -> Result<$int_type, $int_type>
                    where
                        F: FnMut($int_type) -> Option<$int_type>,
                    {
                        loop {}
                    }

                    pub fn try_update(
                        &self,
                        set_order: Ordering,
                        fetch_order: Ordering,
                        f: impl FnMut($int_type) -> Option<$int_type>,
                    ) -> Result<$int_type, $int_type> {
                        loop {}
                    }

                    pub fn update(
                        &self,
                        set_order: Ordering,
                        fetch_order: Ordering,
                        f: impl FnMut($int_type) -> $int_type,
                    ) -> $int_type {
                        loop {}
                    }
                }
            )*};
        }

        atomic_int! {
            AtomicI8 i8
            AtomicU8 u8
            AtomicI16 i16
            AtomicU16 u16
            AtomicI32 i32
            AtomicU32 u32
            AtomicI64 i64
            AtomicU64 u64
            AtomicIsize isize
            AtomicUsize usize
        }

        #[rustc_diagnostic_item = "fence"]
        pub fn fence(order: Ordering) {
            loop {}
        }

        #[rustc_diagnostic_item = "compiler_fence"]
        pub fn compiler_fence(order: Ordering) {
            loop {}
        }
    }
}
"#;
}

ui!(dropping_references, mem);
ui!(dropping_copy_types, mem);
ui!(forgetting_references, mem);
ui!(forgetting_copy_types, mem);
ui!(undropped_manually_drops, mem);
ui!(invalid_from_utf8, utf8);
ui!(atomic_ordering_bool, "lint-invalid-atomic-ordering-bool", atomic);
ui!(atomic_ordering_exchange, "lint-invalid-atomic-ordering-exchange", atomic);
ui!(atomic_ordering_exchange_weak, "lint-invalid-atomic-ordering-exchange-weak", atomic);
ui!(atomic_ordering_fence, "lint-invalid-atomic-ordering-fence", atomic);
ui!(atomic_ordering_int, "lint-invalid-atomic-ordering-int", atomic);
ui!(atomic_ordering_ptr, "lint-invalid-atomic-ordering-ptr", atomic);
ui!(atomic_ordering_uint, "lint-invalid-atomic-ordering-uint", atomic);
ui!(atomic_ordering_update, "lint-invalid-atomic-ordering-update", atomic);

/// A method named as an atomic's but of a trait is not linted: rustc's
/// `lint-invalid-atomic-ordering-false-positive.rs`, which passes with nothing to say.
#[test]
fn atomic_ordering_false_positive() {
    let name = "lint-invalid-atomic-ordering-false-positive.rs";
    let source = include_str!("items/ui/lint-invalid-atomic-ordering-false-positive.rs");
    assert_eq!(ui_std(name, (atomic::FLAGS, atomic::LIBRARY), atomic::PRELUDE, atomic::ITEMS, source), "");
}

/// A `drop` of a value with side effects that is a whole arm of a `match` is kept, and one
/// given an argument with none is linted.
#[test]
fn a_drop_that_is_an_arm_with_side_effects_is_not_linted() {
    let source = "\
#![warn(dropping_copy_types)]
fn f() -> u8 { 1 }
fn g(x: u8) {
    match x {
        0 => drop(f()),
        1 => drop(x),
        _ => (),
    }
}
fn main() {}
";
    let expected = "\
warning: calls to `std::mem::drop` with a value that implements `Copy` does nothing
  --> $DIR/arm.rs:6:14
   |
LL |         1 => drop(x),
   |              ^^^^^-^
   |                   |
   |                   argument has type `u8`
   |
   = note: use `let _ = ...` to ignore the expression or result
note: the lint level is defined here
  --> $DIR/arm.rs:1:9
   |
LL | #![warn(dropping_copy_types)]
   |         ^^^^^^^^^^^^^^^^^^^

warning: 1 warning emitted

";
    assert_eq!(ui_std("arm.rs", (mem::FLAGS, mem::LIBRARY), mem::PRELUDE, mem::ITEMS, source), expected);
}

/// The bytes are read through an immutable `let` and a `const`, not a `let mut`.
#[test]
fn from_utf8_reads_through_lets_and_consts_but_not_mutable_ones() {
    let source = "\
#![deny(invalid_from_utf8)]
const BYTES: &[u8; 2] = &[0xC0, 0x80];
fn main() {
    let a = BYTES;
    let _ = std::str::from_utf8(a);
    let mut b = [0xC0, 0x80];
    let _ = std::str::from_utf8(&mut b);
}
";
    let expected = "\
error: calls to `std::str::from_utf8` with an invalid literal always return an error
  --> $DIR/lets.rs:5:13
   |
LL | const BYTES: &[u8; 2] = &[0xC0, 0x80];
   |                          ------------ the literal was valid UTF-8 up to the 0 bytes
...
LL |     let _ = std::str::from_utf8(a);
   |             ^^^^^^^^^^^^^^^^^^^^^^
   |
note: the lint level is defined here
  --> $DIR/lets.rs:1:9
   |
LL | #![deny(invalid_from_utf8)]
   |         ^^^^^^^^^^^^^^^^^

error: aborting due to 1 previous error

";
    assert_eq!(ui_std("lets.rs", (utf8::FLAGS, &[]), utf8::PRELUDE, utf8::ITEMS, source), expected);
}
