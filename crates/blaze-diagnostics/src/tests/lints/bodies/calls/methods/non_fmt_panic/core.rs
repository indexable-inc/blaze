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
    #[rustc_diagnostic_item = "Result"]
    pub enum Result<T, E> {
        #[lang = "Ok"]
        Ok(T),
        #[lang = "Err"]
        Err(E),
    }
}

pub mod any {
    use crate::marker::{PointeeSized, Sized};

    #[rustc_diagnostic_item = "Any"]
    pub trait Any: 'static {}

    impl<T: 'static + ?Sized> Any for T {}
}

pub mod hint {
    #[must_use]
    pub const fn must_use<T>(value: T) -> T {
        value
    }
}

pub mod fmt {
    use crate::marker::{PointeeSized, Sized};
    use crate::option::Option;

    pub struct Error;

    pub type Result = crate::result::Result<(), Error>;

    pub struct Formatter<'a> {
        buf: &'a (),
    }

    #[rustc_diagnostic_item = "Display"]
    pub trait Display: PointeeSized {
        fn fmt(&self, f: &mut Formatter<'_>) -> Result;
    }

    #[rustc_diagnostic_item = "Debug"]
    pub trait Debug: PointeeSized {
        fn fmt(&self, f: &mut Formatter<'_>) -> Result;
    }

    impl<T: Display + ?Sized> Display for &T {
        fn fmt(&self, f: &mut Formatter<'_>) -> Result {
            Display::fmt(&**self, f)
        }
    }

    impl<T: Debug + ?Sized> Debug for &T {
        fn fmt(&self, f: &mut Formatter<'_>) -> Result {
            Debug::fmt(&**self, f)
        }
    }

    macro_rules! impl_fmt {
        ($($t:ty)*) => {
            $(
                impl Display for $t {
                    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
                        loop {}
                    }
                }

                impl Debug for $t {
                    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
                        loop {}
                    }
                }
            )*
        };
    }
    impl_fmt! { str bool char usize u8 u16 u32 u64 u128 isize i8 i16 i32 i64 i128 f32 f64 }

    impl<T: Debug> Debug for Option<T> {
        fn fmt(&self, f: &mut Formatter<'_>) -> Result {
            loop {}
        }
    }

    pub mod rt {
        use super::{Debug, Display};

        #[lang = "format_argument"]
        pub struct Argument<'a> {
            value: &'a (),
        }

        impl Argument<'_> {
            pub const fn new_display<T: Display>(x: &T) -> Argument<'_> {
                loop {}
            }

            pub const fn new_debug<T: Debug>(x: &T) -> Argument<'_> {
                loop {}
            }
        }
    }

    #[lang = "format_arguments"]
    pub struct Arguments<'a> {
        template: &'a [u8],
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
}

#[unstable(
    feature = "panic_internals",
    reason = "internal details of the implementation of the `panic!` and related macros",
    issue = "none"
)]
pub mod panicking {
    use crate::fmt;

    #[lang = "panic_fmt"]
    #[rustc_const_stable_indirect]
    pub const fn panic_fmt(fmt: fmt::Arguments<'_>) -> ! {
        loop {}
    }

    #[rustc_const_stable_indirect]
    #[lang = "panic"]
    pub const fn panic(expr: &'static str) -> ! {
        loop {}
    }

    #[rustc_diagnostic_item = "unreachable_display"]
    pub fn unreachable_display<T: fmt::Display>(x: &T) -> ! {
        loop {}
    }

    #[rustc_diagnostic_item = "panic_str_2015"]
    #[rustc_const_stable_indirect]
    pub const fn panic_str_2015(expr: &str) -> ! {
        panic_display(&expr);
    }

    #[lang = "panic_display"]
    #[rustc_const_stable_indirect]
    pub const fn panic_display<T: fmt::Display>(x: &T) -> ! {
        loop {}
    }
}

pub mod panic {
    #[doc(hidden)]
    #[allow_internal_unstable(panic_internals, const_format_args)]
    #[rustc_diagnostic_item = "core_panic_2015_macro"]
    #[rustc_macro_transparency = "semiopaque"]
    pub macro panic_2015 {
        () => (
            $crate::panicking::panic("explicit panic")
        ),
        ($msg:literal $(,)?) => (
            $crate::panicking::panic($msg)
        ),
        // Use `panic_str_2015` instead of `panic_display::<&str>` for non_fmt_panic lint.
        ($msg:expr $(,)?) => ({
            $crate::panicking::panic_str_2015($msg);
        }),
        // Special-case the single-argument case for const_panic.
        ("{}", $arg:expr $(,)?) => ({
            $crate::panicking::panic_display(&$arg);
        }),
        ($fmt:expr, $($arg:tt)+) => ({
            // Semicolon to prevent temporaries inside the formatting machinery from
            // being considered alive in the caller after the panic_fmt call.
            $crate::panicking::panic_fmt($crate::const_format_args!($fmt, $($arg)+));
        }),
    }

    #[doc(hidden)]
    #[allow_internal_unstable(panic_internals, const_format_args)]
    #[rustc_diagnostic_item = "core_panic_2021_macro"]
    #[rustc_macro_transparency = "semiopaque"]
    pub macro panic_2021 {
        () => (
            $crate::panicking::panic("explicit panic")
        ),
        // Special-case the single-argument case for const_panic.
        ("{}", $arg:expr $(,)?) => ({
            $crate::panicking::panic_display(&$arg);
        }),
        ($($t:tt)+) => ({
            // Semicolon to prevent temporaries inside the formatting machinery from
            // being considered alive in the caller after the panic_fmt call.
            $crate::panicking::panic_fmt($crate::const_format_args!($($t)+));
        }),
    }

    #[doc(hidden)]
    #[allow_internal_unstable(panic_internals)]
    #[rustc_diagnostic_item = "unreachable_2015_macro"]
    #[rustc_macro_transparency = "semiopaque"]
    pub macro unreachable_2015 {
        () => (
            $crate::panicking::panic("internal error: entered unreachable code")
        ),
        // Use of `unreachable_display` for non_fmt_panic lint.
        // NOTE: the message ("internal error ...") is embedded directly in unreachable_display
        ($msg:expr $(,)?) => ({
            $crate::panicking::unreachable_display(&$msg);
        }),
        ($fmt:expr, $($arg:tt)*) => (
            $crate::panic!($crate::concat!("internal error: entered unreachable code: ", $fmt), $($arg)*)
        ),
    }

    #[doc(hidden)]
    #[allow_internal_unstable(panic_internals)]
    #[rustc_macro_transparency = "semiopaque"]
    pub macro unreachable_2021 {
        () => (
            $crate::panicking::panic("internal error: entered unreachable code")
        ),
        ($($t:tt)+) => (
            $crate::panic!("internal error: entered unreachable code: {}", $crate::format_args!($($t)+))
        ),
    }
}

#[macro_export]
#[rustc_builtin_macro(core_panic)]
#[allow_internal_unstable(edition_panic)]
#[rustc_diagnostic_item = "core_panic_macro"]
macro_rules! panic {
    // Expands to either `$crate::panic::panic_2015` or `$crate::panic::panic_2021`
    // depending on the edition of the caller.
    ($($arg:tt)*) => {
        /* compiler built-in */
    };
}

#[macro_export]
#[rustc_diagnostic_item = "debug_assert_macro"]
#[allow_internal_unstable(edition_panic)]
#[rustc_diagnostic_opaque]
macro_rules! debug_assert {
    ($($arg:tt)*) => {
        if $crate::cfg!(debug_assertions) {
            $crate::assert!($($arg)*);
        }
    };
}

#[macro_export]
#[rustc_builtin_macro(unreachable)]
#[allow_internal_unstable(edition_panic)]
#[rustc_diagnostic_item = "unreachable_macro"]
macro_rules! unreachable {
    // Expands to either `$crate::panic::unreachable_2015` or `$crate::panic::unreachable_2021`
    // depending on the edition of the caller.
    ($($arg:tt)*) => {
        /* compiler built-in */
    };
}

#[rustc_builtin_macro]
#[macro_export]
#[rustc_diagnostic_item = "assert_macro"]
#[allow_internal_unstable(core_intrinsics, panic_internals, edition_panic, generic_assert_internals)]
macro_rules! assert {
    ($cond:expr $(,)?) => {{ /* compiler built-in */ }};
    ($cond:expr, $($arg:tt)+) => {{ /* compiler built-in */ }};
}

#[rustc_builtin_macro]
#[macro_export]
#[rustc_diagnostic_item = "macro_concat"]
macro_rules! concat {
    ($($e:expr),* $(,)?) => {{ /* compiler built-in */ }};
}

#[rustc_builtin_macro]
#[macro_export]
macro_rules! cfg {
    ($($cfg:tt)*) => {
        /* compiler built-in */
    };
}

#[rustc_builtin_macro]
#[macro_export]
#[rustc_diagnostic_item = "format_args_macro"]
#[allow_internal_unsafe]
#[allow_internal_unstable(fmt_internals, fmt_arguments_from_str)]
macro_rules! format_args {
    ($fmt:expr) => {{ /* compiler built-in */ }};
    ($fmt:expr, $($args:tt)*) => {{ /* compiler built-in */ }};
}

#[rustc_builtin_macro]
#[macro_export]
#[allow_internal_unsafe]
#[allow_internal_unstable(fmt_internals, fmt_arguments_from_str)]
macro_rules! const_format_args {
    ($fmt:expr) => {{ /* compiler built-in */ }};
    ($fmt:expr, $($args:tt)*) => {{ /* compiler built-in */ }};
}

pub mod ops {
    #[lang = "not"]
    pub trait Not {
        type Output;
        fn not(self) -> Self::Output;
    }

    impl Not for bool {
        type Output = bool;
        #[inline]
        fn not(self) -> bool {
            !self
        }
    }
}
