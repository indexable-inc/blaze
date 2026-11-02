#![feature(allow_internal_unstable, decl_macro)]

pub use core::{any, hint, marker, option, result};
pub use core::{assert, cfg, concat, const_format_args, debug_assert, format_args, unreachable};

pub mod fmt {
    pub use core::fmt::*;

    #[must_use]
    pub fn format(args: Arguments<'_>) -> crate::string::String {
        loop {}
    }
}

pub mod string {
    use crate::fmt;

    #[lang = "String"]
    pub struct String {
        len: usize,
    }

    impl fmt::Display for String {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            loop {}
        }
    }

    impl fmt::Debug for String {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            loop {}
        }
    }
}

#[doc(hidden)]
pub mod __export {
    pub use core::format_args;
    pub use core::hint::must_use;
}

mod panicking {
    use core::any::Any;
    use core::marker::Send;

    #[lang = "begin_panic"]
    #[track_caller]
    #[rustc_do_not_const_check]
    pub const fn begin_panic<M: Any + Send>(msg: M) -> ! {
        loop {}
    }
}

#[doc(hidden)]
pub mod rt {
    pub use crate::panicking::begin_panic;
    pub use core::panicking::{panic_display, panic_fmt};
}

pub mod panic {
    use core::any::Any;
    use core::marker::Send;

    #[doc(hidden)]
    #[allow_internal_unstable(libstd_sys_internals, const_format_args, panic_internals, rt)]
    #[rustc_diagnostic_item = "std_panic_2015_macro"]
    #[rustc_macro_transparency = "semiopaque"]
    pub macro panic_2015 {
        () => ({
            $crate::rt::begin_panic("explicit panic")
        }),
        ($msg:expr $(,)?) => ({
            $crate::rt::begin_panic($msg);
        }),
        // Special-case the single-argument case for const_panic.
        ("{}", $arg:expr $(,)?) => ({
            $crate::rt::panic_display(&$arg);
        }),
        ($fmt:expr, $($arg:tt)+) => ({
            // Semicolon to prevent temporaries inside the formatting machinery from
            // being considered alive in the caller after the panic_fmt call.
            $crate::rt::panic_fmt($crate::const_format_args!($fmt, $($arg)+));
        }),
    }

    #[doc(hidden)]
    pub use core::panic::panic_2021;

    #[track_caller]
    #[rustc_diagnostic_item = "panic_any"]
    pub fn panic_any<M: 'static + Any + Send>(msg: M) -> ! {
        crate::rt::begin_panic(msg)
    }
}

#[macro_export]
#[rustc_builtin_macro(std_panic)]
#[allow_internal_unstable(edition_panic)]
#[rustc_diagnostic_item = "std_panic_macro"]
macro_rules! panic {
    // Expands to either `$crate::panic::panic_2015` or `$crate::panic::panic_2021`
    // depending on the edition of the caller.
    ($($arg:tt)*) => {
        /* compiler built-in */
    };
}

#[macro_export]
#[allow_internal_unstable(hint_must_use, liballoc_internals)]
#[rustc_diagnostic_item = "format_macro"]
#[rustc_diagnostic_opaque]
macro_rules! format {
    ($($arg:tt)*) => {
        $crate::__export::must_use({
            $crate::fmt::format($crate::__export::format_args!($($arg)*))
        })
    }
}
