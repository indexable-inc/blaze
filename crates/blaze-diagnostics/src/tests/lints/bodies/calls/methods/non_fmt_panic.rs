//! rustc's UI tests of `NonPanicFmt` (`non_fmt_panics`): `tests/ui/macros/non-fmt-panic.rs` with
//! its aux crate `tests/ui/macros/auxiliary/fancy-panic.rs`, and the edition 2018 revision of
//! `tests/ui/consts/const-eval/const_panic_stability.rs` (MIT or Apache-2.0), their sources and
//! `.stderr`s as they are (in `non_fmt_panic/`). Both are edition 2015 or 2018 tests: the lint
//! is of the macros of those editions.

use super::{Library, ui_library};
use crate::tests::{assert_reports, lints::bodies::calls::Ui, warning};

/// What core's prelude has of `non_fmt_panic/core.rs`: `Option`, and the macros
/// `library/core/src/prelude/v1.rs` re-exports that the tests call.
const CORE_PRELUDE: &str = "        pub use crate::option::Option::{self, None, Some};\n\
     \x20       pub use crate::{assert, cfg, concat, const_format_args, debug_assert, format_args, unreachable};\n";

/// What std's prelude has of `non_fmt_panic/std.rs`, as `library/std/src/prelude/v1.rs` writes
/// it: `format!`, `String`, and `panic!` without the module `panic`.
const STD_PRELUDE: &str = "        pub use crate::format;\n\
     \x20       pub use crate::string::String;\n\
     \x20       mod ambiguous_macros_only {\n\
     \x20           mod panic {}\n\
     \x20           pub use crate::*;\n\
     \x20       }\n\
     \x20       pub use self::ambiguous_macros_only::panic;\n";

fn library<'a>(aux: &'a [(&'a str, &'a str, &'a str)]) -> Library<'a> {
    Library {
        core: include_str!("non_fmt_panic/core.rs"),
        core_prelude: CORE_PRELUDE,
        std: include_str!("non_fmt_panic/std.rs"),
        std_prelude: STD_PRELUDE,
        aux,
    }
}

/// Run with `//@ edition:2015..2021`; its `.stderr` is the same for 2015 and 2018.
#[test]
fn non_fmt_panic() {
    let aux = [("fancy_panic", "2015", include_str!("non_fmt_panic/fancy-panic.rs"))];
    let ui = Ui { name: "non-fmt-panic.rs", source: include_str!("non_fmt_panic/non-fmt-panic.rs"), ..Ui::default() };
    assert_reports(
        &ui_library(ui, &library(&aux)),
        &[
            warning(20, &[]),
            warning(21, &[]),
            warning(22, &[]),
            warning(23, &[]),
            warning(24, &[]),
            warning(26, &[]),
            warning(28, &[]),
            warning(30, &[]),
            warning(32, &[]),
            warning(33, &[]),
            warning(34, &[]),
            warning(35, &[]),
            warning(36, &[]),
            warning(37, &[]),
            warning(38, &[]),
            warning(39, &[]),
            warning(40, &[]),
            warning(41, &[]),
            warning(43, &[]),
            warning(53, &[]),
            warning(54, &[]),
            warning(56, &[]),
            warning(57, &[]),
            warning(58, &[]),
            warning(59, &[]),
            warning(61, &[]),
            warning(62, &[]),
            warning(79, &[]),
            warning(80, &[]),
            warning(84, &[]),
            warning(85, &[]),
            warning(89, &[]),
            warning(90, &[]),
            warning(94, &[]),
            warning(95, &[]),
        ],
    );
}

/// The same, as edition 2018 builds it.
#[test]
fn non_fmt_panic_2018() {
    let aux = [("fancy_panic", "2018", include_str!("non_fmt_panic/fancy-panic.rs"))];
    let ui = Ui {
        name: "non-fmt-panic.rs",
        edition: "2018",
        source: include_str!("non_fmt_panic/non-fmt-panic.rs"),
        ..Ui::default()
    };
    assert_reports(
        &ui_library(ui, &library(&aux)),
        &[
            warning(20, &[]),
            warning(21, &[]),
            warning(22, &[]),
            warning(23, &[]),
            warning(24, &[]),
            warning(26, &[]),
            warning(28, &[]),
            warning(30, &[]),
            warning(32, &[]),
            warning(33, &[]),
            warning(34, &[]),
            warning(35, &[]),
            warning(36, &[]),
            warning(37, &[]),
            warning(38, &[]),
            warning(39, &[]),
            warning(40, &[]),
            warning(41, &[]),
            warning(43, &[]),
            warning(53, &[]),
            warning(54, &[]),
            warning(56, &[]),
            warning(57, &[]),
            warning(58, &[]),
            warning(59, &[]),
            warning(61, &[]),
            warning(62, &[]),
            warning(79, &[]),
            warning(80, &[]),
            warning(84, &[]),
            warning(85, &[]),
            warning(89, &[]),
            warning(90, &[]),
            warning(94, &[]),
            warning(95, &[]),
        ],
    );
}

/// The revision `e2018`, a `check-pass` test: `panic!({ "foo" })` in a `const fn`.
#[test]
fn const_panic_stability_e2018() {
    let ui = Ui {
        name: "const_panic_stability.rs",
        edition: "2018",
        source: include_str!("non_fmt_panic/const_panic_stability.rs"),
        ..Ui::default()
    };
    assert_reports(&ui_library(ui, &library(&[])), &[warning(14, &[])]);
}
