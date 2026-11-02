//! `bad_asm_style`, against rustc's `tests/ui/asm/inline-syntax.rs` (MIT or Apache-2.0), kept
//! byte for byte with its `x86_64` revision's `.stderr` in `asm/`.

use super::ui_fixture;
use crate::{Options, tests::Mode};

/// The asm macros of `tests/auxiliary/minicore.rs`, for a test with `//@ add-minicore`.
const MINICORE: &str = r#"//- /minicore.rs crate:minicore library
#[rustc_builtin_macro]
pub macro asm("assembly template", $(operands,)* $(options($(option),*))?) {
    /* compiler built-in */
}
#[rustc_builtin_macro]
pub macro global_asm("assembly template", $(operands,)* $(options($(option),*))?) {
    /* compiler built-in */
}
#[rustc_builtin_macro]
pub macro naked_asm("assembly template", $(operands,)* $(options($(option),*))?) {
    /* compiler built-in */
}
"#;

#[test]
fn syntax_directives_are_bad_style_on_x86() {
    let source = include_str!("asm/inline-syntax.rs");
    let fixture = format!(
        "{MINICORE}//- /inline-syntax.rs crate:other:main edition:2015 deps:minicore cfg:target_arch=x86_64\n{source}"
    );
    let expected = include_str!("asm/inline-syntax.x86_64.stderr");
    assert_eq!(ui_fixture(Mode::Other, &fixture, &Options::default()), expected);
}
