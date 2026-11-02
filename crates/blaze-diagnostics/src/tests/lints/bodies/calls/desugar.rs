//! rustc's UI tests of `ForLoopsOverFallibles`, `ShadowedIntoIter` and `IfLetRescope`: each
//! test's source and `.stderr` as they are in rustc's `tests/ui/` (in `desugar/ui/`), compiled
//! against a `std` of the items of std they use, written as std writes them, with their lang
//! items, diagnostic items and `#[rustc_skip_during_method_dispatch]`. Core is minicore without
//! its iterators, which std's stand for here as core's do in rustc's.

use crate::{
    Options,
    emit::Style,
    tests::{Mode, compiletest},
};

/// minicore's flags: what core has that the tests use.
const FLAGS: &str = "sized, copy, clone, drop, deref, option, result, index, slice, range, coerce_unsized, unsize, builtin_impls";

/// What std's prelude has that core's (without its iterators) does not.
const PRELUDE: &str = "\
        pub use crate::boxed::Box;
        pub use crate::iter::{IntoIterator, Iterator};
        pub use crate::vec::Vec;
        pub use crate::println;
        mod ambiguous_macros_only {
            mod vec {}
            pub use crate::*;
        }
        pub use self::ambiguous_macros_only::vec;
";

/// std's iterator traits with the `IntoIterator`s of `Option`, `Result`, arrays, slices, boxed
/// arrays and boxed slices, `[T]::iter`, `Box`, `Vec` with `vec!`, `Rc`, `ops` and `println!`.
const ITEMS: &str = r#"
pub use core::ops;

pub mod iter {
    use core::option::Option;

    #[lang = "iterator"]
    #[rustc_diagnostic_item = "Iterator"]
    pub trait Iterator {
        type Item;
        #[lang = "next"]
        fn next(&mut self) -> Option<Self::Item>;
        fn by_ref(&mut self) -> &mut Self
        where
            Self: Sized,
        {
            self
        }
    }

    impl<I: Iterator + ?Sized> Iterator for &mut I {
        type Item = I::Item;
        fn next(&mut self) -> Option<I::Item> {
            (**self).next()
        }
    }

    #[rustc_diagnostic_item = "IntoIterator"]
    #[rustc_skip_during_method_dispatch(array, boxed_slice)]
    pub trait IntoIterator {
        type Item;
        type IntoIter: Iterator<Item = Self::Item>;
        #[lang = "into_iter"]
        fn into_iter(self) -> Self::IntoIter;
    }

    impl<I: Iterator> IntoIterator for I {
        type Item = I::Item;
        type IntoIter = I;
        fn into_iter(self) -> I {
            self
        }
    }
}

pub mod option {
    use core::option::Option;
    use crate::iter::{IntoIterator, Iterator};

    pub struct IntoIter<A> {
        inner: Option<A>,
    }
    impl<A> Iterator for IntoIter<A> {
        type Item = A;
        fn next(&mut self) -> Option<A> {
            loop {}
        }
    }
    impl<T> IntoIterator for Option<T> {
        type Item = T;
        type IntoIter = IntoIter<T>;
        fn into_iter(self) -> IntoIter<T> {
            IntoIter { inner: self }
        }
    }

    pub struct Iter<'a, A> {
        inner: Option<&'a A>,
    }
    impl<'a, A> Iterator for Iter<'a, A> {
        type Item = &'a A;
        fn next(&mut self) -> Option<&'a A> {
            loop {}
        }
    }
    impl<'a, T> IntoIterator for &'a Option<T> {
        type Item = &'a T;
        type IntoIter = Iter<'a, T>;
        fn into_iter(self) -> Iter<'a, T> {
            loop {}
        }
    }

    pub struct IterMut<'a, A> {
        inner: Option<&'a mut A>,
    }
    impl<'a, A> Iterator for IterMut<'a, A> {
        type Item = &'a mut A;
        fn next(&mut self) -> Option<&'a mut A> {
            loop {}
        }
    }
    impl<'a, T> IntoIterator for &'a mut Option<T> {
        type Item = &'a mut T;
        type IntoIter = IterMut<'a, T>;
        fn into_iter(self) -> IterMut<'a, T> {
            loop {}
        }
    }
}

pub mod result {
    use core::option::Option;
    use core::result::Result;
    use crate::iter::{IntoIterator, Iterator};

    pub struct IntoIter<T> {
        inner: Option<T>,
    }
    impl<T> Iterator for IntoIter<T> {
        type Item = T;
        fn next(&mut self) -> Option<T> {
            loop {}
        }
    }
    impl<T, E> IntoIterator for Result<T, E> {
        type Item = T;
        type IntoIter = IntoIter<T>;
        fn into_iter(self) -> IntoIter<T> {
            loop {}
        }
    }

    pub struct Iter<'a, T> {
        inner: Option<&'a T>,
    }
    impl<'a, T> Iterator for Iter<'a, T> {
        type Item = &'a T;
        fn next(&mut self) -> Option<&'a T> {
            loop {}
        }
    }
    impl<'a, T, E> IntoIterator for &'a Result<T, E> {
        type Item = &'a T;
        type IntoIter = Iter<'a, T>;
        fn into_iter(self) -> Iter<'a, T> {
            loop {}
        }
    }

    pub struct IterMut<'a, T> {
        inner: Option<&'a mut T>,
    }
    impl<'a, T> Iterator for IterMut<'a, T> {
        type Item = &'a mut T;
        fn next(&mut self) -> Option<&'a mut T> {
            loop {}
        }
    }
    impl<'a, T, E> IntoIterator for &'a mut Result<T, E> {
        type Item = &'a mut T;
        type IntoIter = IterMut<'a, T>;
        fn into_iter(self) -> IterMut<'a, T> {
            loop {}
        }
    }
}

pub mod slice {
    use core::option::Option;
    use crate::iter::{IntoIterator, Iterator};

    pub struct Iter<'a, T> {
        slice: &'a [T],
    }
    impl<'a, T> Iterator for Iter<'a, T> {
        type Item = &'a T;
        fn next(&mut self) -> Option<&'a T> {
            loop {}
        }
    }
    pub struct IterMut<'a, T> {
        slice: &'a mut [T],
    }
    impl<'a, T> Iterator for IterMut<'a, T> {
        type Item = &'a mut T;
        fn next(&mut self) -> Option<&'a mut T> {
            loop {}
        }
    }

    impl<T> [T] {
        #[rustc_allow_incoherent_impl]
        pub fn iter(&self) -> Iter<'_, T> {
            Iter { slice: self }
        }
    }

    impl<'a, T> IntoIterator for &'a [T] {
        type Item = &'a T;
        type IntoIter = Iter<'a, T>;
        fn into_iter(self) -> Iter<'a, T> {
            self.iter()
        }
    }
    impl<'a, T> IntoIterator for &'a mut [T] {
        type Item = &'a mut T;
        type IntoIter = IterMut<'a, T>;
        fn into_iter(self) -> IterMut<'a, T> {
            IterMut { slice: self }
        }
    }
}

pub mod array {
    use core::option::Option;
    use crate::iter::{IntoIterator, Iterator};
    use crate::slice::{Iter, IterMut};

    pub struct IntoIter<T, const N: usize> {
        data: [T; N],
    }
    impl<T, const N: usize> Iterator for IntoIter<T, N> {
        type Item = T;
        fn next(&mut self) -> Option<T> {
            loop {}
        }
    }
    impl<T, const N: usize> IntoIterator for [T; N] {
        type Item = T;
        type IntoIter = IntoIter<T, N>;
        fn into_iter(self) -> IntoIter<T, N> {
            IntoIter { data: self }
        }
    }
    impl<'a, T, const N: usize> IntoIterator for &'a [T; N] {
        type Item = &'a T;
        type IntoIter = Iter<'a, T>;
        fn into_iter(self) -> Iter<'a, T> {
            self.iter()
        }
    }
    impl<'a, T, const N: usize> IntoIterator for &'a mut [T; N] {
        type Item = &'a mut T;
        type IntoIter = IterMut<'a, T>;
        fn into_iter(self) -> IterMut<'a, T> {
            loop {}
        }
    }
}

pub mod boxed {
    use core::clone::Clone;
    use core::marker::Unsize;
    use core::ops::{CoerceUnsized, Deref};
    use core::option::Option;
    use crate::iter::{IntoIterator, Iterator};
    use crate::slice::Iter;

    #[lang = "owned_box"]
    pub struct Box<T: ?Sized>(*mut T);

    impl<T> Box<T> {
        #[rustc_diagnostic_item = "box_new"]
        pub fn new(x: T) -> Box<T> {
            loop {}
        }
    }
    impl<T: ?Sized> Deref for Box<T> {
        type Target = T;
        fn deref(&self) -> &T {
            loop {}
        }
    }
    impl<T: ?Sized + Unsize<U>, U: ?Sized> CoerceUnsized<Box<U>> for Box<T> {}
    impl<T> Clone for Box<[T]> {
        fn clone(&self) -> Self {
            loop {}
        }
    }

    impl<I> IntoIterator for Box<[I]> {
        type Item = I;
        type IntoIter = crate::vec::IntoIter<I>;
        fn into_iter(self) -> crate::vec::IntoIter<I> {
            loop {}
        }
    }
    impl<'a, I> IntoIterator for &'a Box<[I]> {
        type Item = &'a I;
        type IntoIter = Iter<'a, I>;
        fn into_iter(self) -> Iter<'a, I> {
            self.iter()
        }
    }

    pub struct BoxedArrayIntoIter<T, const N: usize> {
        data: Box<[T; N]>,
    }
    impl<T, const N: usize> Iterator for BoxedArrayIntoIter<T, N> {
        type Item = T;
        fn next(&mut self) -> Option<T> {
            loop {}
        }
    }
    impl<T, const N: usize> IntoIterator for Box<[T; N]> {
        type Item = T;
        type IntoIter = BoxedArrayIntoIter<T, N>;
        fn into_iter(self) -> BoxedArrayIntoIter<T, N> {
            BoxedArrayIntoIter { data: self }
        }
    }
}

pub mod vec {
    use core::clone::Clone;
    use core::option::Option;
    use crate::boxed::Box;
    use crate::iter::Iterator;

    pub struct Vec<T> {
        ptr: *mut T,
        len: usize,
    }
    impl<T> Vec<T> {
        pub fn into_boxed_slice(self) -> Box<[T]> {
            loop {}
        }
    }

    #[doc(hidden)]
    pub fn from_elem<T: Clone>(elem: T, n: usize) -> Vec<T> {
        loop {}
    }
    #[doc(hidden)]
    pub fn from_array<T, const N: usize>(array: [T; N]) -> Vec<T> {
        loop {}
    }

    pub struct IntoIter<T> {
        ptr: *const T,
    }
    impl<T> Iterator for IntoIter<T> {
        type Item = T;
        fn next(&mut self) -> Option<T> {
            loop {}
        }
    }
}

pub mod rc {
    use core::ops::Deref;

    pub struct Rc<T: ?Sized> {
        ptr: *const T,
    }
    impl<T> Rc<T> {
        pub fn new(value: T) -> Rc<T> {
            loop {}
        }
    }
    impl<T: ?Sized> Deref for Rc<T> {
        type Target = T;
        fn deref(&self) -> &T {
            loop {}
        }
    }
}

#[macro_export]
macro_rules! vec {
    ($elem:expr; $n:expr) => {
        $crate::vec::from_elem($elem, $n)
    };
    ($($x:expr),+ $(,)?) => {
        $crate::vec::from_array([$($x),+])
    };
}

#[macro_export]
macro_rules! println {
    ($($arg:tt)*) => {{}};
}
"#;

/// What rustc writes compiling the UI test `name` in `edition` as compiletest does for `mode`
/// (none of these tests has lint flags), core being minicore with [`FLAGS`] and std the
/// prelude with [`PRELUDE`] and [`ITEMS`].
fn ui(mode: Mode, name: &str, edition: &str, source: &str) -> String {
    let mut fixture = format!("//- minicore: {FLAGS}\n//- /std.rs crate:std\n");
    fixture.push_str("#![feature(lang_items, rustc_attrs)]\npub mod prelude {\n");
    for edition in ["rust_2015", "rust_2018", "rust_2021", "rust_2024"] {
        fixture.push_str(&format!(
            "    pub mod {edition} {{\n        pub use core::prelude::{edition}::*;\n{PRELUDE}    }}\n"
        ));
    }
    fixture.push_str(&format!("}}\n{ITEMS}\n//- /{name} crate:other:main edition:{edition} deps:std\n{source}"));
    let stderr = crate::tests::checked_with(&fixture, "$DIR/", &compiletest(mode, &Options::default()))
        .stderr_with(Style { ui_testing: true, ..Style::default() });
    crate::tests::normalized(&stderr)
}

/// A test of `$file` in `$edition`, expecting `$stderr` (a file of `desugar/ui/`, or nothing).
macro_rules! ui {
    ($test:ident, $mode:ident, $file:literal, $edition:literal, $stderr:literal) => {
        #[test]
        fn $test() {
            let source = include_str!(concat!("desugar/ui/", $file, ".rs"));
            let expected = include_str!(concat!("desugar/ui/", $stderr));
            assert_eq!(ui(Mode::$mode, concat!($file, ".rs"), $edition, source), expected);
        }
    };
    ($test:ident, $mode:ident, $file:literal, $edition:literal) => {
        #[test]
        fn $test() {
            let source = include_str!(concat!("desugar/ui/", $file, ".rs"));
            assert_eq!(ui(Mode::$mode, concat!($file, ".rs"), $edition, source), "");
        }
    };
}

// `tests/ui/lint/`.
ui!(for_loop_over_fallibles, Other, "for_loop_over_fallibles", "2015", "for_loop_over_fallibles.stderr");

// `tests/ui/iterators/`; `into-iter-on-arrays-lint` runs in each edition of `2015..2021`.
ui!(into_iter_on_arrays_2018, Other, "into-iter-on-arrays-2018", "2018", "into-iter-on-arrays-2018.stderr");
ui!(into_iter_on_arrays_2021, Other, "into-iter-on-arrays-2021", "2021");
ui!(into_iter_on_arrays_lint_2015, RunPass, "into-iter-on-arrays-lint", "2015", "into-iter-on-arrays-lint.stderr");
ui!(into_iter_on_arrays_lint_2018, RunPass, "into-iter-on-arrays-lint", "2018", "into-iter-on-arrays-lint.stderr");
ui!(
    into_iter_on_boxed_slices_2021,
    Other,
    "into-iter-on-boxed-slices-2021",
    "2018",
    "into-iter-on-boxed-slices-2021.stderr"
);
ui!(into_iter_on_boxed_slices_2024, Other, "into-iter-on-boxed-slices-2024", "2024");
ui!(
    into_iter_on_boxed_slices_lint,
    RunPass,
    "into-iter-on-boxed-slices-lint",
    "2015",
    "into-iter-on-boxed-slices-lint.stderr"
);

// `tests/ui/rust-2021/`: its warning is typeck's, under `array_into_iter`
// (`rustc_hir_typeck/src/method/prelude_edition_lints.rs`), for a call that resolves to
// another trait's `into_iter`; `ShadowedIntoIter` sees `MyIntoIter::into_iter` and is silent.
#[test]
fn array_into_iter_ambiguous() {
    let source = include_str!("desugar/ui/array-into-iter-ambiguous.rs");
    let expected = include_str!("desugar/ui/array-into-iter-ambiguous.stderr");
    assert_eq!(ui(Mode::Other, "array-into-iter-ambiguous.rs", "2018", source), expected);
}

// `tests/ui/drop/`; `lint-if-let-rescope-gated` has the revisions `edition2021` and
// `edition2024`.
ui!(lint_if_let_rescope, Other, "lint-if-let-rescope", "2015", "lint-if-let-rescope.stderr");
ui!(
    lint_if_let_rescope_gated_edition2021,
    Other,
    "lint-if-let-rescope-gated",
    "2021",
    "lint-if-let-rescope-gated.edition2021.stderr"
);
ui!(lint_if_let_rescope_gated_edition2024, Other, "lint-if-let-rescope-gated", "2024");
ui!(lint_if_let_rescope_with_macro, Other, "lint-if-let-rescope-with-macro", "2021", "lint-if-let-rescope-with-macro.stderr");
ui!(lint_if_let_rescope_false_positives, Other, "lint-if-let-rescope-false-positives", "2021");
