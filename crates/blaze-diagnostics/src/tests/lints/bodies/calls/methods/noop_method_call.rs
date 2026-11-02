//! rustc's UI tests of `NoopMethodCall`: `tests/ui/lint/noop-method-call.rs` and
//! `tests/ui/lint/suspicious-double-ref-op.rs` (MIT or Apache-2.0), their sources and `.stderr`s
//! as they are (in `noop_method_call/`).

use super::{
    CORE_BORROW, CORE_CMP, CORE_CMP_PRELUDE, CORE_FMT, CORE_FMT_PRELUDE, CORE_ITER, CORE_ITER_PRELUDE, CORE_OPS,
    CORE_OPS_PRELUDE, CORE_OPTION_RESULT, CORE_OPTION_RESULT_PRELUDE, Library, STD_VEC, STD_VEC_PRELUDE, ui_library,
};
use crate::tests::lints::bodies::calls::Ui;

#[test]
fn noop_method_call() {
    let core = format!("{CORE_OPS}{CORE_BORROW}");
    let library = Library { core: &core, core_prelude: CORE_OPS_PRELUDE, std: "pub use core::{borrow, ops};\n", ..Library::default() };
    let ui = Ui { name: "noop-method-call.rs", source: include_str!("noop_method_call/noop-method-call.rs"), ..Ui::default() };
    assert_eq!(ui_library(ui, &library), include_str!("noop_method_call/noop-method-call.stderr"));
}

/// std's `println!`, `io`'s `Write` and `_print`, `fs::File` with `Write for &File`, and
/// `sync::LazyLock`, after [`STD_VEC`].
const DOUBLE_REF_STD: &str = r#"#![feature(allow_internal_unstable)]

pub use core::{borrow, fmt, iter, ops, option, result, slice};
pub use core::{concat, format_args, format_args_nl};

#[macro_export]
#[rustc_diagnostic_item = "println_macro"]
#[allow_internal_unstable(print_internals, format_args_nl)]
macro_rules! println {
    ($($arg:tt)*) => {{
        $crate::io::_print($crate::format_args_nl!($($arg)*));
    }};
}

pub mod io {
    pub struct Error {
        repr: (),
    }

    pub type Result<T> = core::result::Result<T, Error>;

    #[rustc_diagnostic_item = "IoWrite"]
    pub trait Write {
        fn write(&mut self, buf: &[u8]) -> Result<usize>;
    }

    pub fn _print(args: core::fmt::Arguments<'_>) {}
}

pub mod fs {
    pub struct File {
        inner: (),
    }

    impl crate::io::Write for File {
        fn write(&mut self, buf: &[u8]) -> crate::io::Result<usize> {
            loop {}
        }
    }

    impl crate::io::Write for &File {
        fn write(&mut self, buf: &[u8]) -> crate::io::Result<usize> {
            loop {}
        }
    }
}

pub mod sync {
    use core::marker::PhantomData;
    use core::ops::{Deref, FnOnce};

    #[rustc_diagnostic_item = "LazyLock"]
    pub struct LazyLock<T, F = fn() -> T> {
        init: F,
        data: PhantomData<T>,
    }

    impl<T, F: FnOnce() -> T> LazyLock<T, F> {
        pub const fn new(f: F) -> LazyLock<T, F> {
            LazyLock { init: f, data: PhantomData }
        }
    }

    impl<T, F: FnOnce() -> T> Deref for LazyLock<T, F> {
        type Target = T;

        fn deref(&self) -> &T {
            loop {}
        }
    }
}
"#;

#[test]
fn suspicious_double_ref_op() {
    let core = format!("{CORE_OPS}{CORE_BORROW}{CORE_OPTION_RESULT}{CORE_CMP}{CORE_ITER}{CORE_FMT}");
    let core_prelude =
        format!("{CORE_OPS_PRELUDE}{CORE_OPTION_RESULT_PRELUDE}{CORE_CMP_PRELUDE}{CORE_ITER_PRELUDE}{CORE_FMT_PRELUDE}");
    let std = format!("{DOUBLE_REF_STD}{STD_VEC}");
    let std_prelude = format!("{STD_VEC_PRELUDE}        pub use crate::println;\n");
    let library = Library {
        core: &core,
        core_prelude: &core_prelude,
        std: &std,
        std_prelude: &std_prelude,
        ..Library::default()
    };
    let ui = Ui {
        name: "suspicious-double-ref-op.rs",
        source: include_str!("noop_method_call/suspicious-double-ref-op.rs"),
        ..Ui::default()
    };
    assert_eq!(ui_library(ui, &library), include_str!("noop_method_call/suspicious-double-ref-op.stderr"));
}
