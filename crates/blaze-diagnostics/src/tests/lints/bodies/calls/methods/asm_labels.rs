//! rustc's UI tests of `AsmLabels` (`named_asm_labels`, `binary_asm_labels`), from
//! `tests/ui/asm` (MIT or Apache-2.0), kept byte for byte in `asm_labels/`, each built for the
//! target its `//@ only-..` names (`target_arch`, which these lints read).

use super::super::Ui;
use super::{Library, ui_library};

/// What the tests use of `core` (`library/core/src/arch.rs`, `library/core/src/macros/mod.rs`).
const CORE: &str = r#"pub mod arch {
    #[rustc_builtin_macro]
    pub macro asm("assembly template", $(operands,)* $(options($(option),*))?) {
        /* compiler built-in */
    }

    #[rustc_builtin_macro]
    pub macro naked_asm("assembly template", $(operands,)* $(options($(option),*))?) {
        /* compiler built-in */
    }

    #[rustc_builtin_macro]
    pub macro global_asm("assembly template", $(operands,)* $(options($(option),*))?) {
        /* compiler built-in */
    }
}

#[rustc_builtin_macro]
#[macro_export]
#[rustc_diagnostic_item = "include_str_macro"]
macro_rules! include_str {
    ($file:expr $(,)?) => {{ /* compiler built-in */ }};
}
"#;

/// What the tests use of `std` (`library/std/src/lib.rs`).
const STD: &str = r#"pub mod arch {
    pub use core::arch::*;
}
"#;

/// The asm macros of `tests/auxiliary/minicore.rs`, for a test with `//@ add-minicore`.
const MINICORE: &str = r#"#![feature(no_core, rustc_attrs, decl_macro)]
#![no_std]
#![no_core]

#[rustc_builtin_macro]
pub macro asm("assembly template", $(operands,)* $(options($(option),*))?) {
    /* compiler built-in */
}
#[rustc_builtin_macro]
pub macro naked_asm("assembly template", $(operands,)* $(options($(option),*))?) {
    /* compiler built-in */
}
#[rustc_builtin_macro]
pub macro global_asm("assembly template", $(operands,)* $(options($(option),*))?) {
    /* compiler built-in */
}
"#;

fn library() -> Library<'static> {
    Library { core: CORE, core_prelude: "        pub use crate::include_str;", std: STD, ..Library::default() }
}

#[test]
fn named_labels_are_denied_but_in_naked_functions_that_are_not_generic() {
    let source = format!(
        "{}//- /named-asm-labels.s\n{}",
        include_str!("asm_labels/named-asm-labels.rs"),
        include_str!("asm_labels/named-asm-labels.s"),
    );
    let ui = Ui {
        name: "named-asm-labels.rs",
        meta: "cfg:target_arch=x86_64",
        source: &source,
        ..Ui::default()
    };
    assert_eq!(ui_library(ui, &library()), include_str!("asm_labels/named-asm-labels.stderr"));
}

#[test]
fn binary_labels_are_denied_on_x86() {
    let ui = Ui {
        name: "binary_asm_labels.rs",
        meta: "cfg:target_arch=x86_64",
        source: include_str!("asm_labels/binary_asm_labels.rs"),
        ..Ui::default()
    };
    assert_eq!(ui_library(ui, &library()), include_str!("asm_labels/binary_asm_labels.stderr"));
}

#[test]
fn binary_labels_are_allowed_off_x86() {
    let ui = Ui {
        name: "binary_asm_labels_allowed.rs",
        meta: "cfg:target_arch=aarch64",
        source: include_str!("asm_labels/binary_asm_labels_allowed.rs"),
        ..Ui::default()
    };
    assert_eq!(ui_library(ui, &library()), "");
}

/// The `.stderr` without its first warning, of `-C target-feature=+hvx-length128b`, which is
/// not a lint and which the harness has no flag for.
#[test]
fn hexagon_register_pairs_are_not_labels() {
    let ui = Ui {
        name: "hexagon-register-pairs.rs",
        meta: "cfg:target_arch=hexagon",
        source: include_str!("asm_labels/hexagon-register-pairs.rs"),
        ..Ui::default()
    };
    let library = Library { aux: &[("minicore", "2015", MINICORE)], ..library() };
    let stderr = include_str!("asm_labels/hexagon-register-pairs.stderr");
    let (warning, rest) = stderr.split_once("\n\n").unwrap();
    assert!(warning.starts_with("warning: unstable feature specified for `-Ctarget-feature`"));
    let expected = rest.replace(
        "error: aborting due to 2 previous errors; 1 warning emitted",
        "error: aborting due to 2 previous errors",
    );
    assert_eq!(ui_library(ui, &library), expected);
}
