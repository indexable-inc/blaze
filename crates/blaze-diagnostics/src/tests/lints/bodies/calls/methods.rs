//! rustc's UI tests of `NoopMethodCall`, `MapUnitFn`, `NonPanicFmt` and `AsmLabels`, each compiled
//! against a `core` and a `std` of the items it uses, written as `library/core` and
//! `library/std` write them, their `#[lang]`s and `#[rustc_diagnostic_item]`s included: these
//! lints read diagnostic items minicore does not have (`Clone`, `noop_method_clone`,
//! `IteratorMap`, `std_panic_2015_macro`, ..), and the macros of edition 2015's `panic!`.

mod asm_labels;
mod map_unit_fn;
mod non_fmt_panic;
mod noop_method_call;

use super::{Ui, split_flag};
use crate::{Options, lints::CommandLine};

/// The options compiletest gives a UI test of `mode`, then the test's own lint flags.
fn compiletest(mode: crate::tests::Mode, flags: &[&str]) -> Options {
    let args: Vec<String> = flags.iter().flat_map(|flag| split_flag(flag)).map(str::to_owned).collect();
    let own = Options { lints: CommandLine::parse(&args).unwrap(), ..Options::default() };
    crate::tests::compiletest(mode, &own)
}

/// What every test's `core` starts with: `marker`'s `Sized` and `Copy`, `clone`'s `Clone` with
/// `&T`'s no-op `clone`, the primitives' impls of both, and the `derive` attribute. It is a
/// `staged_api` crate, as core is: its const stability attributes count only in one.
const CORE: &str = r#"#![no_core]
#![rustc_coherence_is_core]
#![feature(no_core, lang_items, rustc_attrs, decl_macro, fundamental, auto_traits, negative_impls, staged_api)]
#![stable(feature = "core", since = "1.6.0")]

pub mod marker {
    #[lang = "pointee_sized"]
    #[fundamental]
    #[rustc_specialization_trait]
    #[rustc_coinductive]
    pub trait PointeeSized {}

    #[lang = "meta_sized"]
    #[fundamental]
    #[rustc_specialization_trait]
    #[rustc_coinductive]
    pub trait MetaSized: PointeeSized {}

    #[lang = "sized"]
    #[rustc_diagnostic_item = "Sized"]
    #[fundamental]
    #[rustc_specialization_trait]
    #[rustc_coinductive]
    pub trait Sized: MetaSized {}

    #[rustc_diagnostic_item = "Send"]
    pub unsafe auto trait Send {}
    impl<T: PointeeSized> !Send for *const T {}
    impl<T: PointeeSized> !Send for *mut T {}
    unsafe impl<T: Sync + PointeeSized> Send for &T {}
    unsafe impl<T: Send + PointeeSized> Send for &mut T {}

    #[rustc_diagnostic_item = "Sync"]
    #[lang = "sync"]
    pub unsafe auto trait Sync {}
    impl<T: PointeeSized> !Sync for *const T {}
    impl<T: PointeeSized> !Sync for *mut T {}

    #[rustc_unsafe_specialization_marker]
    #[rustc_diagnostic_item = "Copy"]
    #[lang = "copy"]
    pub trait Copy: Clone {}

    #[rustc_builtin_macro]
    pub macro Copy($item:item) {}

    use crate::clone::Clone;

    macro_rules! impl_copy {
        ($($t:ty)*) => { $(impl Copy for $t {})* };
    }
    impl_copy! { usize u8 u16 u32 u64 u128 isize i8 i16 i32 i64 i128 f32 f64 bool char ! }
    impl<T: PointeeSized> Copy for *const T {}
    impl<T: PointeeSized> Copy for *mut T {}
    impl<T: PointeeSized> Copy for &T {}

    #[lang = "tuple_trait"]
    pub trait Tuple {}

    #[lang = "unsize"]
    pub trait Unsize<T: PointeeSized>: PointeeSized {}

    #[lang = "phantom_data"]
    #[rustc_diagnostic_item = "PhantomData"]
    pub struct PhantomData<T: PointeeSized>;
}

pub mod clone {
    use crate::marker::{PointeeSized, Sized};

    #[lang = "clone"]
    #[rustc_diagnostic_item = "Clone"]
    pub trait Clone: Sized {
        fn clone(&self) -> Self;
    }

    #[rustc_builtin_macro]
    pub macro Clone($item:item) {}

    macro_rules! impl_clone {
        ($($t:ty)*) => {
            $(impl Clone for $t {
                fn clone(&self) -> Self {
                    *self
                }
            })*
        };
    }
    impl_clone! { usize u8 u16 u32 u64 u128 isize i8 i16 i32 i64 i128 f32 f64 bool char ! }

    impl<T: PointeeSized> Clone for *const T {
        fn clone(&self) -> Self {
            *self
        }
    }

    impl<T: PointeeSized> Clone for *mut T {
        fn clone(&self) -> Self {
            *self
        }
    }

    impl<T: PointeeSized> Clone for &T {
        #[rustc_diagnostic_item = "noop_method_clone"]
        fn clone(&self) -> Self {
            *self
        }
    }
}

pub mod macros {
    pub mod builtin {
        #[rustc_builtin_macro]
        pub macro derive($item:item) {}
    }
}
"#;

/// core's `ops`: `Deref` and `DerefMut` with `&T`'s no-op `deref`, `Receiver`, the `Fn` traits
/// and `CoerceUnsized`. With [`CORE_OPS_PRELUDE`] in core's prelude.
pub(crate) const CORE_OPS: &str = r#"
pub mod ops {
    mod deref {
        use crate::marker::{PointeeSized, Sized};

        #[lang = "deref"]
        #[rustc_diagnostic_item = "Deref"]
        pub trait Deref: PointeeSized {
            #[rustc_diagnostic_item = "deref_target"]
            #[lang = "deref_target"]
            type Target: ?Sized;

            #[rustc_diagnostic_item = "deref_method"]
            fn deref(&self) -> &Self::Target;
        }

        impl<T: PointeeSized> Deref for &T {
            type Target = T;

            #[rustc_diagnostic_item = "noop_method_deref"]
            fn deref(&self) -> &T {
                *self
            }
        }

        impl<T: PointeeSized> Deref for &mut T {
            type Target = T;

            fn deref(&self) -> &T {
                self
            }
        }

        #[lang = "deref_mut"]
        #[rustc_diagnostic_item = "DerefMut"]
        pub trait DerefMut: Deref + PointeeSized {
            #[rustc_diagnostic_item = "deref_mut_method"]
            fn deref_mut(&mut self) -> &mut Self::Target;
        }

        impl<T: PointeeSized> DerefMut for &mut T {
            fn deref_mut(&mut self) -> &mut T {
                self
            }
        }

        #[lang = "receiver"]
        pub trait Receiver: PointeeSized {
            #[lang = "receiver_target"]
            type Target: ?Sized;
        }

        impl<P: PointeeSized, T: PointeeSized> Receiver for P
        where
            P: Deref<Target = T>,
        {
            type Target = T;
        }
    }
    pub use self::deref::{Deref, DerefMut, Receiver};

    mod function {
        use crate::marker::{Sized, Tuple};

        #[lang = "fn"]
        #[rustc_diagnostic_item = "Fn"]
        #[fundamental]
        #[rustc_paren_sugar]
        pub trait Fn<Args: Tuple>: FnMut<Args> {
            extern "rust-call" fn call(&self, args: Args) -> Self::Output;
        }

        #[lang = "fn_mut"]
        #[rustc_diagnostic_item = "FnMut"]
        #[fundamental]
        #[rustc_paren_sugar]
        pub trait FnMut<Args: Tuple>: FnOnce<Args> {
            extern "rust-call" fn call_mut(&mut self, args: Args) -> Self::Output;
        }

        #[lang = "fn_once"]
        #[rustc_diagnostic_item = "FnOnce"]
        #[fundamental]
        #[rustc_paren_sugar]
        pub trait FnOnce<Args: Tuple> {
            #[lang = "fn_once_output"]
            type Output;

            extern "rust-call" fn call_once(self, args: Args) -> Self::Output;
        }

        impl<A: Tuple, F: ?Sized> Fn<A> for &F
        where
            F: Fn<A>,
        {
            extern "rust-call" fn call(&self, args: A) -> F::Output {
                (**self).call(args)
            }
        }

        impl<A: Tuple, F: ?Sized> FnMut<A> for &F
        where
            F: Fn<A>,
        {
            extern "rust-call" fn call_mut(&mut self, args: A) -> F::Output {
                (**self).call(args)
            }
        }

        impl<A: Tuple, F: ?Sized> FnOnce<A> for &F
        where
            F: Fn<A>,
        {
            type Output = F::Output;

            extern "rust-call" fn call_once(self, args: A) -> F::Output {
                (*self).call(args)
            }
        }

        impl<A: Tuple, F: ?Sized> FnMut<A> for &mut F
        where
            F: FnMut<A>,
        {
            extern "rust-call" fn call_mut(&mut self, args: A) -> F::Output {
                (*self).call_mut(args)
            }
        }

        impl<A: Tuple, F: ?Sized> FnOnce<A> for &mut F
        where
            F: FnMut<A>,
        {
            type Output = F::Output;

            extern "rust-call" fn call_once(self, args: A) -> F::Output {
                (*self).call_mut(args)
            }
        }
    }
    pub use self::function::{Fn, FnMut, FnOnce};

    mod unsize {
        use crate::marker::{PointeeSized, Unsize};

        #[lang = "coerce_unsized"]
        pub trait CoerceUnsized<T: PointeeSized> {}

        impl<'a, T: PointeeSized + Unsize<U>, U: PointeeSized> CoerceUnsized<&'a mut U> for &'a mut T {}
        impl<'a, 'b: 'a, T: PointeeSized + Unsize<U>, U: PointeeSized> CoerceUnsized<&'a U> for &'b mut T {}
        impl<'a, T: PointeeSized + Unsize<U>, U: PointeeSized> CoerceUnsized<*mut U> for &'a mut T {}
        impl<'a, T: PointeeSized + Unsize<U>, U: PointeeSized> CoerceUnsized<*const U> for &'a mut T {}
        impl<'a, 'b: 'a, T: PointeeSized + Unsize<U>, U: PointeeSized> CoerceUnsized<&'a U> for &'b T {}
        impl<'a, T: PointeeSized + Unsize<U>, U: PointeeSized> CoerceUnsized<*const U> for &'a T {}
        impl<T: PointeeSized + Unsize<U>, U: PointeeSized> CoerceUnsized<*mut U> for *mut T {}
        impl<T: PointeeSized + Unsize<U>, U: PointeeSized> CoerceUnsized<*const U> for *mut T {}
        impl<T: PointeeSized + Unsize<U>, U: PointeeSized> CoerceUnsized<*const U> for *const T {}
    }
    pub use self::unsize::CoerceUnsized;
}
"#;

/// What core's prelude has of [`CORE_OPS`].
pub(crate) const CORE_OPS_PRELUDE: &str = "        pub use crate::ops::{Fn, FnMut, FnOnce};\n";

/// core's `borrow`: `Borrow` and `BorrowMut`, with `T`'s no-op `borrow`.
pub(crate) const CORE_BORROW: &str = r#"
pub mod borrow {
    use crate::marker::Sized;

    #[rustc_diagnostic_item = "Borrow"]
    pub trait Borrow<Borrowed: ?Sized> {
        fn borrow(&self) -> &Borrowed;
    }

    #[rustc_diagnostic_item = "BorrowMut"]
    pub trait BorrowMut<Borrowed: ?Sized>: Borrow<Borrowed> {
        fn borrow_mut(&mut self) -> &mut Borrowed;
    }

    impl<T: ?Sized> Borrow<T> for T {
        #[rustc_diagnostic_item = "noop_method_borrow"]
        fn borrow(&self) -> &T {
            self
        }
    }

    impl<T: ?Sized> BorrowMut<T> for T {
        fn borrow_mut(&mut self) -> &mut T {
            self
        }
    }

    impl<T: ?Sized> Borrow<T> for &T {
        fn borrow(&self) -> &T {
            self
        }
    }

    impl<T: ?Sized> Borrow<T> for &mut T {
        fn borrow(&self) -> &T {
            self
        }
    }

    impl<T: ?Sized> BorrowMut<T> for &mut T {
        fn borrow_mut(&mut self) -> &mut T {
            self
        }
    }
}
"#;

/// core's `option` and `result`: `Option` and `Result` with their lang items. With
/// [`CORE_OPTION_RESULT_PRELUDE`] in core's prelude.
pub(crate) const CORE_OPTION_RESULT: &str = r#"
pub mod option {
    #[lang = "Option"]
    #[rustc_diagnostic_item = "Option"]
    pub enum Option<T> {
        #[lang = "None"]
        None,
        #[lang = "Some"]
        Some(T),
    }
}

pub mod result {
    #[lang = "Result"]
    #[rustc_diagnostic_item = "Result"]
    pub enum Result<T, E> {
        #[lang = "Ok"]
        Ok(T),
        #[lang = "Err"]
        Err(E),
    }
}
"#;

/// What core's prelude has of [`CORE_OPTION_RESULT`].
pub(crate) const CORE_OPTION_RESULT_PRELUDE: &str = "        pub use crate::option::Option::{self, None, Some};\n\
     \x20       pub use crate::result::Result::{self, Err, Ok};\n";

/// core's `cmp`: `PartialEq`, `Eq`, `PartialOrd` and `Ord`, implemented for the integers. With
/// [`CORE_CMP_PRELUDE`] in core's prelude.
pub(crate) const CORE_CMP: &str = r#"
pub mod cmp {
    use crate::marker::Sized;

    #[lang = "eq"]
    #[rustc_diagnostic_item = "PartialEq"]
    pub trait PartialEq<Rhs: ?Sized = Self> {
        fn eq(&self, other: &Rhs) -> bool;
    }

    #[rustc_diagnostic_item = "Eq"]
    pub trait Eq: PartialEq<Self> {}

    #[lang = "partial_ord"]
    #[rustc_diagnostic_item = "PartialOrd"]
    pub trait PartialOrd<Rhs: ?Sized = Self>: PartialEq<Rhs> {
        fn lt(&self, other: &Rhs) -> bool;
    }

    #[rustc_diagnostic_item = "Ord"]
    pub trait Ord: Eq + PartialOrd<Self> {}

    macro_rules! impl_cmp {
        ($($t:ty)*) => {
            $(
                impl PartialEq for $t {
                    fn eq(&self, other: &$t) -> bool {
                        loop {}
                    }
                }
                impl Eq for $t {}
                impl PartialOrd for $t {
                    fn lt(&self, other: &$t) -> bool {
                        loop {}
                    }
                }
                impl Ord for $t {}
            )*
        };
    }
    impl_cmp! { usize u8 u16 u32 u64 u128 isize i8 i16 i32 i64 i128 bool char }
}
"#;

/// What core's prelude has of [`CORE_CMP`].
pub(crate) const CORE_CMP_PRELUDE: &str = "        pub use crate::cmp::{Eq, Ord, PartialEq, PartialOrd};\n";

/// core's `iter` (`Iterator` with `map` and `collect`, `Map`, `IntoIterator`, `FromIterator`)
/// and `slice` (`[T]::iter`, `[T]::iter_mut`, `[T]::sort`, their iterators), on
/// [`CORE_OPS`] and [`CORE_OPTION_RESULT`] and [`CORE_CMP`]. With [`CORE_ITER_PRELUDE`] in
/// core's prelude.
pub(crate) const CORE_ITER: &str = r#"
pub mod iter {
    use crate::marker::Sized;
    use crate::ops::FnMut;
    use crate::option::Option;

    #[rustc_diagnostic_item = "Iterator"]
    #[lang = "iterator"]
    #[must_use = "iterators are lazy and do nothing unless consumed"]
    pub trait Iterator {
        #[rustc_diagnostic_item = "IteratorItem"]
        type Item;

        #[lang = "next"]
        fn next(&mut self) -> Option<Self::Item>;

        #[rustc_diagnostic_item = "IteratorMap"]
        fn map<B, F>(self, f: F) -> Map<Self, F>
        where
            Self: Sized,
            F: FnMut(Self::Item) -> B,
        {
            Map { iter: self, f }
        }

        fn for_each<F>(self, f: F)
        where
            Self: Sized,
            F: FnMut(Self::Item),
        {
            loop {}
        }

        #[rustc_diagnostic_item = "iterator_collect_fn"]
        fn collect<B: FromIterator<Self::Item>>(self) -> B
        where
            Self: Sized,
        {
            FromIterator::from_iter(self)
        }
    }

    #[must_use = "iterators are lazy and do nothing unless consumed"]
    pub struct Map<I, F> {
        iter: I,
        f: F,
    }

    impl<B, I: Iterator, F> Iterator for Map<I, F>
    where
        F: FnMut(I::Item) -> B,
    {
        type Item = B;

        fn next(&mut self) -> Option<B> {
            loop {}
        }
    }

    #[rustc_diagnostic_item = "IntoIterator"]
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

    #[rustc_diagnostic_item = "FromIterator"]
    pub trait FromIterator<A>: Sized {
        #[rustc_diagnostic_item = "from_iter_fn"]
        fn from_iter<T: IntoIterator<Item = A>>(iter: T) -> Self;
    }
}

pub mod slice {
    use crate::marker::PhantomData;
    use crate::option::Option;

    #[lang = "slice"]
    impl<T> [T] {
        #[lang = "slice_len_fn"]
        pub const fn len(&self) -> usize {
            loop {}
        }

        #[rustc_diagnostic_item = "slice_iter"]
        pub fn iter(&self) -> Iter<'_, T> {
            loop {}
        }

        pub fn iter_mut(&mut self) -> IterMut<'_, T> {
            loop {}
        }

        pub fn sort(&mut self)
        where
            T: crate::cmp::Ord,
        {
            loop {}
        }
    }

    pub struct Iter<'a, T: 'a> {
        ptr: *const T,
        end: *const T,
        _marker: PhantomData<&'a T>,
    }

    impl<'a, T> crate::iter::Iterator for Iter<'a, T> {
        type Item = &'a T;

        fn next(&mut self) -> Option<&'a T> {
            loop {}
        }
    }

    pub struct IterMut<'a, T: 'a> {
        ptr: *mut T,
        end: *mut T,
        _marker: PhantomData<&'a mut T>,
    }

    impl<'a, T> crate::iter::Iterator for IterMut<'a, T> {
        type Item = &'a mut T;

        fn next(&mut self) -> Option<&'a mut T> {
            loop {}
        }
    }
}
"#;

/// What core's prelude has of [`CORE_ITER`].
pub(crate) const CORE_ITER_PRELUDE: &str =
    "        pub use crate::iter::{FromIterator, IntoIterator, Iterator};\n";

/// core's `fmt`: the formatting traits, `Formatter`, `Arguments` and `rt::Argument` with the
/// lang items `format_args!` lowers to, and the `format_args!` macros, on
/// [`CORE_OPTION_RESULT`]. With [`CORE_FMT_PRELUDE`] in core's prelude.
pub(crate) const CORE_FMT: &str = r#"
pub mod fmt {
    use crate::marker::PointeeSized;

    pub struct Error;

    pub type Result = crate::result::Result<(), Error>;

    #[rustc_diagnostic_item = "Formatter"]
    pub struct Formatter<'a> {
        buf: &'a mut (dyn Write + 'a),
    }

    pub trait Write {
        fn write_str(&mut self, s: &str) -> Result;
    }

    #[rustc_diagnostic_item = "Debug"]
    pub trait Debug: PointeeSized {
        fn fmt(&self, f: &mut Formatter<'_>) -> Result;
    }

    #[rustc_diagnostic_item = "Display"]
    pub trait Display: PointeeSized {
        fn fmt(&self, f: &mut Formatter<'_>) -> Result;
    }

    #[rustc_diagnostic_item = "Pointer"]
    pub trait Pointer: PointeeSized {
        fn fmt(&self, f: &mut Formatter<'_>) -> Result;
    }

    #[rustc_diagnostic_item = "LowerHex"]
    pub trait LowerHex: PointeeSized {
        fn fmt(&self, f: &mut Formatter<'_>) -> Result;
    }

    impl<T: PointeeSized + Debug> Debug for &T {
        fn fmt(&self, f: &mut Formatter<'_>) -> Result {
            Debug::fmt(&**self, f)
        }
    }

    impl<T: PointeeSized + Display> Display for &T {
        fn fmt(&self, f: &mut Formatter<'_>) -> Result {
            Display::fmt(&**self, f)
        }
    }

    impl<T: PointeeSized> Pointer for &T {
        fn fmt(&self, f: &mut Formatter<'_>) -> Result {
            loop {}
        }
    }

    impl<T: PointeeSized> Pointer for *const T {
        fn fmt(&self, f: &mut Formatter<'_>) -> Result {
            loop {}
        }
    }

    macro_rules! impl_fmt {
        ($($t:ty)*) => {
            $(
                impl Debug for $t {
                    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
                        loop {}
                    }
                }
                impl Display for $t {
                    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
                        loop {}
                    }
                }
            )*
        };
    }
    impl_fmt! { usize u8 u16 u32 u64 u128 isize i8 i16 i32 i64 i128 f32 f64 bool char str }

    macro_rules! impl_lower_hex {
        ($($t:ty)*) => {
            $(
                impl LowerHex for $t {
                    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
                        loop {}
                    }
                }
            )*
        };
    }
    impl_lower_hex! { usize u8 u16 u32 u64 u128 isize i8 i16 i32 i64 i128 }

    impl<T: Debug> Debug for [T] {
        fn fmt(&self, f: &mut Formatter<'_>) -> Result {
            loop {}
        }
    }

    impl<T: Debug> Debug for crate::option::Option<T> {
        fn fmt(&self, f: &mut Formatter<'_>) -> Result {
            loop {}
        }
    }

    pub mod rt {
        use super::{Debug, Display, Formatter, LowerHex, Pointer, Result};

        #[lang = "format_argument"]
        pub struct Argument<'a> {
            value: &'a (),
            formatter: fn(&(), &mut Formatter<'_>) -> Result,
        }

        impl<'a> Argument<'a> {
            fn new<'b, T>(x: &'b T, f: fn(&T, &mut Formatter<'_>) -> Result) -> Argument<'b> {
                loop {}
            }

            pub fn new_display<T: Display>(x: &T) -> Argument<'_> {
                Self::new(x, Display::fmt)
            }

            pub fn new_debug<T: Debug>(x: &T) -> Argument<'_> {
                Self::new(x, Debug::fmt)
            }

            pub fn new_pointer<T: Pointer>(x: &T) -> Argument<'_> {
                Self::new(x, Pointer::fmt)
            }

            pub fn new_lower_hex<T: LowerHex>(x: &T) -> Argument<'_> {
                Self::new(x, LowerHex::fmt)
            }

            pub const fn from_usize(x: &usize) -> Argument<'_> {
                loop {}
            }
        }

        #[lang = "format_alignment"]
        pub enum Alignment {
            Left,
            Right,
            Center,
            Unknown,
        }

        #[lang = "format_count"]
        pub enum Count {
            Is(u16),
            Param(usize),
            Implied,
        }

        #[lang = "format_placeholder"]
        pub struct Placeholder {
            pub position: usize,
            pub flags: u32,
            pub precision: Count,
            pub width: Count,
        }

        impl Placeholder {
            pub const fn new(position: usize, flags: u32, precision: Count, width: Count) -> Self {
                Self { position, flags, precision, width }
            }
        }

        #[lang = "format_unsafe_arg"]
        pub struct UnsafeArg {
            _private: (),
        }

        impl UnsafeArg {
            pub unsafe fn new() -> Self {
                Self { _private: () }
            }
        }
    }

    #[lang = "format_arguments"]
    #[rustc_diagnostic_item = "Arguments"]
    pub struct Arguments<'a> {
        template: &'a [u8],
        args: &'a [rt::Argument<'a>],
    }

    impl<'a> Arguments<'a> {
        pub unsafe fn new<const N: usize, const M: usize>(
            template: &'a [u8; N],
            args: &'a [rt::Argument<'a>; M],
        ) -> Arguments<'a> {
            loop {}
        }

        pub const fn from_str(s: &'static str) -> Arguments<'a> {
            loop {}
        }
    }

    impl Display for Arguments<'_> {
        fn fmt(&self, f: &mut Formatter<'_>) -> Result {
            loop {}
        }
    }
}

mod fmt_macros {
    #[macro_export]
    #[rustc_builtin_macro]
    macro_rules! format_args {
        ($fmt:expr) => {{ /* compiler built-in */ }};
        ($fmt:expr, $($args:tt)*) => {{ /* compiler built-in */ }};
    }

    #[macro_export]
    #[rustc_builtin_macro]
    macro_rules! const_format_args {
        ($fmt:expr) => {{ /* compiler built-in */ }};
        ($fmt:expr, $($args:tt)*) => {{ /* compiler built-in */ }};
    }

    #[macro_export]
    #[rustc_builtin_macro]
    macro_rules! format_args_nl {
        ($fmt:expr) => {{ /* compiler built-in */ }};
        ($fmt:expr, $($args:tt)*) => {{ /* compiler built-in */ }};
    }

    #[macro_export]
    #[rustc_builtin_macro]
    macro_rules! concat {
        ($($e:expr),* $(,)?) => {{ /* compiler built-in */ }};
    }
}
"#;

/// What core's prelude has of [`CORE_FMT`].
pub(crate) const CORE_FMT_PRELUDE: &str =
    "        pub use crate::{concat, const_format_args, format_args, format_args_nl};\n";

/// std's `vec`: `Vec` with `Clone`, `Deref` and `DerefMut` to its slice and `FromIterator`, and
/// `vec!`, on [`CORE_OPS`] and [`CORE_ITER`]. With [`STD_VEC_PRELUDE`] in std's prelude.
pub(crate) const STD_VEC: &str = r#"
pub mod vec {
    use core::marker::PhantomData;
    use core::ops::{Deref, DerefMut};

    #[rustc_diagnostic_item = "Vec"]
    pub struct Vec<T> {
        ptr: *mut T,
        len: usize,
        _marker: PhantomData<T>,
    }

    impl<T> Vec<T> {
        #[doc(hidden)]
        pub fn from_array<const N: usize>(elements: [T; N]) -> Vec<T> {
            loop {}
        }
    }

    impl<T: Clone> Clone for Vec<T> {
        fn clone(&self) -> Vec<T> {
            loop {}
        }
    }

    impl<T> Deref for Vec<T> {
        type Target = [T];

        fn deref(&self) -> &[T] {
            loop {}
        }
    }

    impl<T> DerefMut for Vec<T> {
        fn deref_mut(&mut self) -> &mut [T] {
            loop {}
        }
    }

    impl<T> core::iter::FromIterator<T> for Vec<T> {
        fn from_iter<I: core::iter::IntoIterator<Item = T>>(iter: I) -> Vec<T> {
            loop {}
        }
    }
}

#[macro_export]
#[rustc_diagnostic_item = "vec_macro"]
macro_rules! vec {
    ($($x:expr),* $(,)?) => {
        $crate::vec::Vec::from_array([$($x),*])
    };
}
"#;

/// What std's prelude has of [`STD_VEC`], as `library/std/src/prelude/v1.rs` writes it: `Vec`,
/// and `vec!` without the module `vec`.
pub(crate) const STD_VEC_PRELUDE: &str = "        pub use crate::vec::Vec;\n\
     \x20       mod ambiguous_macros_only {\n\
     \x20           mod vec {}\n\
     \x20           pub use crate::*;\n\
     \x20       }\n\
     \x20       pub use self::ambiguous_macros_only::vec;\n";

/// The crates a UI test is compiled against.
#[derive(Default)]
pub(crate) struct Library<'a> {
    /// The modules of `core` the test uses beyond [`CORE`]'s, as core writes them.
    pub(crate) core: &'a str,
    /// What core's prelude has beyond `Sized`, `Send`, `Sync`, `Copy`, `Clone` and `derive`:
    /// `pub use crate::..;` lines.
    pub(crate) core_prelude: &'a str,
    /// The modules of `std` the test uses, as std writes them; `core` is its dependency.
    /// [`ui_library`] gives it `#![feature(lang_items, rustc_attrs)]`, so its own `#![feature]`
    /// line, if any, names only the others.
    pub(crate) std: &'a str,
    /// What std's prelude has beyond core's: `pub use crate::..;` lines.
    pub(crate) std_prelude: &'a str,
    /// The crates of the test's `//@ aux-build`s, `(crate name, edition, source)`, each
    /// depending on `std` and `core`; the test's crate depends on each.
    pub(crate) aux: &'a [(&'a str, &'a str, &'a str)],
}

/// The prelude module of each edition: `uses` and, in std's, all of core's first.
fn prelude(of_core: bool, uses: &str) -> String {
    let mut out = String::from("pub mod prelude {\n");
    for edition in ["rust_2015", "rust_2018", "rust_2021", "rust_2024"] {
        out.push_str(&format!("    pub mod {edition} {{\n"));
        if of_core {
            out.push_str(
                "        pub use crate::marker::{Copy, Send, Sized, Sync};\n\
                 \x20       pub use crate::clone::Clone;\n\
                 \x20       pub use crate::macros::builtin::derive;\n",
            );
        } else {
            out.push_str(&format!("        pub use core::prelude::{edition}::*;\n"));
        }
        out.push_str(uses);
        out.push_str("\n    }\n");
    }
    out.push_str("}\n");
    out
}

/// What rustc writes to stderr compiling the UI test `ui` against `library`: its `.stderr`.
/// `ui`'s own `core` and `std` are minicore's, which these tests do not use. std starts with
/// the features its `#[lang]`s and `#[rustc_diagnostic_item]`s need, without which hir-def
/// collects none of them.
pub(crate) fn ui_library(ui: Ui<'_>, library: &Library<'_>) -> String {
    assert!(ui.core.is_empty() && ui.std.is_none(), "the library is `library`'s");
    let mut fixture = format!(
        "//- /core.rs crate:core edition:2024\n{CORE}{}\n{}\
         //- /std.rs crate:std edition:2024 deps:core\n#![feature(lang_items, rustc_attrs)]\n{}\n{}",
        library.core,
        prelude(true, library.core_prelude),
        library.std,
        prelude(false, library.std_prelude),
    );
    let mut deps = String::from("std,core");
    for (name, edition, source) in library.aux {
        fixture.push_str(&format!("//- /{name}.rs crate:{name} edition:{edition} deps:std,core\n{source}"));
        if !source.ends_with('\n') {
            fixture.push('\n');
        }
        deps.push(',');
        deps.push_str(name);
    }
    let meta = if ui.meta.is_empty() { String::new() } else { format!(" {}", ui.meta) };
    fixture.push_str(&format!("//- /{} crate:other:main edition:{} deps:{deps}{meta}\n{}", ui.name, ui.edition, ui.source));
    let stderr = crate::tests::checked_with(&fixture, "$DIR/", &compiletest(ui.mode, ui.flags))
        .stderr_with(crate::emit::Style { ui_testing: true, ..crate::emit::Style::default() });
    crate::tests::normalized(&stderr)
}
