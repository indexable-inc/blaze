//! rustc's UI tests of the errors of the crate-level, macro, test, `#[path]`, `#[must_use]` and
//! `#[non_exhaustive]` attribute parsers, and of `check_attr`'s of the attributes no parser takes.

use crate::tests::{assert_reports, error, ui, ui_builtins};

#[test]
fn a_windows_subsystem_is_console_or_windows() {
    // tests/ui/windows-subsystem/windows-subsystem-invalid.rs
    let source = r#"#![windows_subsystem = "wrong"]
//~^ ERROR malformed `windows_subsystem` attribute input [E0539]

fn main() {}
"#;
    let stderr = r#"error[E0539]: malformed `windows_subsystem` attribute input
  --> $DIR/windows-subsystem-invalid.rs:1:4
   |
LL | #![windows_subsystem = "wrong"]
   |    ^^^^^^^^^^^^^^^^^^^^-------
   |                        |
   |                        valid arguments are "console" or "windows"
   |
   = note: for more information, visit <https://doc.rust-lang.org/reference/runtime.html#the-windows_subsystem-attribute>
help: try changing it to one of the following valid forms of the attribute
   |
LL - #![windows_subsystem = "wrong"]
LL + #![windows_subsystem = "console"]
   |
LL - #![windows_subsystem = "wrong"]
LL + #![windows_subsystem = "windows"]
   |

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0539`.
"#;
    assert_eq!(ui("windows-subsystem-invalid.rs", "2015", source), stderr);
}

#[test]
fn a_recursion_limit_needs_a_value() {
    // tests/ui/recursion/recursion_limit/no-value.rs
    let source = r#"// Test the parse error for no value provided to recursion_limit

#![recursion_limit]
//~^ ERROR malformed `recursion_limit` attribute input

fn main() {}
"#;
    let stderr = r#"error[E0539]: malformed `recursion_limit` attribute input
  --> $DIR/no-value.rs:3:4
   |
LL | #![recursion_limit]
   |    ^^^^^^^^^^^^^^^
   |
   = note: for more information, visit <https://doc.rust-lang.org/reference/attributes/limits.html#the-recursion_limit-attribute>
help: must be of the form
   |
LL | #![recursion_limit = "N"]
   |                    +++++

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0539`.
"#;
    assert_eq!(ui("no-value.rs", "2015", source), stderr);
}

#[test]
fn a_recursion_limit_is_a_string() {
    // tests/ui/recursion/recursion_limit/invalid_digit_type.rs
    let source = r#"#![recursion_limit = 123] //~ ERROR malformed `recursion_limit` attribute

fn main() {}
"#;
    let stderr = r#"error[E0539]: malformed `recursion_limit` attribute input
  --> $DIR/invalid_digit_type.rs:1:4
   |
LL | #![recursion_limit = 123]
   |    ^^^^^^^^^^^^^^^^^^---
   |                      |
   |                      expected a string literal here
   |
   = note: for more information, visit <https://doc.rust-lang.org/reference/attributes/limits.html#the-recursion_limit-attribute>
help: must be of the form
   |
LL - #![recursion_limit = 123]
LL + #![recursion_limit = "N"]
   |

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0539`.
"#;
    assert_eq!(ui("invalid_digit_type.rs", "2015", source), stderr);
}

#[test]
fn a_crate_type_needs_a_value() {
    // tests/ui/attributes/crate-type-empty.rs
    let source = r#"// regression test for issue 11256
#![crate_type]  //~ ERROR malformed `crate_type` attribute

fn main() {
    return
}
"#;
    let stderr = r#"error[E0539]: malformed `crate_type` attribute input
  --> $DIR/crate-type-empty.rs:2:4
   |
LL | #![crate_type]
   |    ^^^^^^^^^^
   |
   = note: for more information, visit <https://doc.rust-lang.org/reference/linkage.html>
help: must be of the form
   |
LL | #![crate_type = "crate type"]
   |               ++++++++++++++

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0539`.
"#;
    assert_eq!(ui("crate-type-empty.rs", "2015", source), stderr);
}

#[test]
fn a_crate_type_is_not_a_list() {
    // tests/ui/attributes/crate-type-delimited.rs
    let source = r#"// regression test for issue 16974
#![crate_type(lib)]  //~ ERROR malformed `crate_type` attribute input

fn my_lib_fn() {}

fn main() {}
"#;
    let stderr = r#"error[E0539]: malformed `crate_type` attribute input
  --> $DIR/crate-type-delimited.rs:2:4
   |
LL | #![crate_type(lib)]
   |    ^^^^^^^^^^^^^^^
   |
   = note: for more information, visit <https://doc.rust-lang.org/reference/linkage.html>
help: must be of the form
   |
LL - #![crate_type(lib)]
LL + #![crate_type = "crate type"]
   |

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0539`.
"#;
    assert_eq!(ui("crate-type-delimited.rs", "2015", source), stderr);
}

#[test]
fn a_crate_name_read_before_expansion_is_a_literal() {
    // tests/ui/attributes/crate-name-macro-call.rs
    let source = r#"// issue: rust-lang/rust#122001
// Ensure we reject macro calls inside `#![crate_name]` as their result wouldn't get honored anyway.

#![crate_name = concat!("my", "crate")] //~ ERROR attribute value must be a literal

fn main() {}
"#;
    let stderr = r#"error: attribute value must be a literal
  --> $DIR/crate-name-macro-call.rs:4:17
   |
LL | #![crate_name = concat!("my", "crate")]
   |                 ^^^^^^^^^^^^^^^^^^^^^^

error: aborting due to 1 previous error

"#;
    assert_eq!(ui("crate-name-macro-call.rs", "2015", source), stderr);
}

#[test]
fn a_crate_type_read_before_expansion_is_a_literal() {
    // tests/ui/attributes/crate-type-macro-call.rs
    let source = r#"#![crate_type = foo!()]
//~^ ERROR attribute value must be a literal

macro_rules! foo {
    () => {"rlib"};
}

fn main() {}
"#;
    let stderr = r#"error: attribute value must be a literal
  --> $DIR/crate-type-macro-call.rs:1:17
   |
LL | #![crate_type = foo!()]
   |                 ^^^^^^

error: aborting due to 1 previous error

"#;
    assert_eq!(ui("crate-type-macro-call.rs", "2015", source), stderr);
}

#[test]
fn the_attributes_of_a_macro_call_are_read_before_their_values_expand() {
    // tests/ui/attributes/key-value-expansion-on-mac.rs
    let source = r#"#![feature(rustc_attrs)]

#[rustc_dummy = stringify!(a)] // OK
macro_rules! bar {
    () => {};
}

// FIXME?: `bar` here expands before `stringify` has a chance to expand.
// `#[rustc_dummy = ...]` is validated and dropped during expansion of `bar`,
// the "attribute value must be a literal" error comes from the validation.
#[rustc_dummy = stringify!(b)] //~ ERROR attribute value must be a literal
bar!();

fn main() {}
"#;
    let stderr = r#"error: attribute value must be a literal
  --> $DIR/key-value-expansion-on-mac.rs:11:17
   |
LL | #[rustc_dummy = stringify!(b)]
   |                 ^^^^^^^^^^^^^

error: aborting due to 1 previous error

"#;
    assert_eq!(ui("key-value-expansion-on-mac.rs", "2015", source), stderr);
}

#[test]
fn rustc_cannot_be_registered_as_a_tool() {
    // tests/ui/tool-attributes/predefined-tool.rs
    let source = r#"#![feature(register_tool)]
// Registering predefined tool is okay.
#![register_tool(clippy)]
#![register_attribute_tool(miri)]
#![register_lint_tool(rustfmt)]
#![register_tool(diagnostic)]
#![register_attribute_tool(rust_analyzer)]
// Registering "rustc" is an error.
#![register_tool(rustc)] //~ ERROR reserved

fn main() {}
"#;
    let stderr = r#"error: tool `rustc` is reserved and cannot be registered
  --> $DIR/predefined-tool.rs:9:18
   |
LL | #![register_tool(rustc)]
   |                  ^^^^^

error: aborting due to 1 previous error

"#;
    assert_eq!(ui("predefined-tool.rs", "2015", source), stderr);
}

#[test]
fn a_registered_tool_is_an_identifier() {
    // tests/ui/tool-attributes/nested-disallowed.rs
    let source = r#"#![feature(register_tool)]
#![register_tool(foo::bar)] //~ ERROR malformed `register_tool` attribute input

fn main() {}
"#;
    let stderr = r#"error[E0565]: malformed `register_tool` attribute input
  --> $DIR/nested-disallowed.rs:2:4
   |
LL | #![register_tool(foo::bar)]
   |    ^^^^^^^^^^^^^^--------^
   |                  |
   |                  expected a valid identifier here
   |
help: must be of the form
   |
LL - #![register_tool(foo::bar)]
LL + #![register_tool(tool1, tool2, ...)]
   |

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0565`.
"#;
    assert_eq!(ui("nested-disallowed.rs", "2015", source), stderr);
}

#[test]
fn a_registered_tool_is_not_a_literal() {
    // tests/ui/tool-attributes/invalid-tool.rs
    let source = r#"#![feature(register_tool)]

#![register_tool(1)]
//~^ ERROR malformed `register_tool` attribute input

fn main() {}
"#;
    let stderr = r#"error[E0565]: malformed `register_tool` attribute input
  --> $DIR/invalid-tool.rs:3:4
   |
LL | #![register_tool(1)]
   |    ^^^^^^^^^^^^^^-^
   |                  |
   |                  expected a valid identifier here
   |
help: must be of the form
   |
LL - #![register_tool(1)]
LL + #![register_tool(tool1, tool2, ...)]
   |

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0565`.
"#;
    assert_eq!(ui("invalid-tool.rs", "2015", source), stderr);
}

#[test]
fn a_feature_list_names_features() {
    // Our own: tests/ui/feature-gates/gated-bad-feature.rs without the features
    // `checks::features` says are unknown or removed.
    let source = r#"#![feature(foo(bar), foo = "baz")]
#![feature]
#![feature = "foo"]

fn main() {}
"#;
    let stderr = r#"error[E0565]: malformed `feature` attribute input
  --> $DIR/gated-bad-feature.rs:1:4
   |
LL | #![feature(foo(bar), foo = "baz")]
   |    ^^^^^^^^^^^-----^^^^^^^^^^^^^^
   |               |
   |               didn't expect any arguments here
   |
help: must be of the form
   |
LL - #![feature(foo(bar), foo = "baz")]
LL + #![feature(feature1, feature2, ...)]
   |

error[E0565]: malformed `feature` attribute input
  --> $DIR/gated-bad-feature.rs:1:4
   |
LL | #![feature(foo(bar), foo = "baz")]
   |    ^^^^^^^^^^^^^^^^^^^^^^-------^
   |                          |
   |                          didn't expect any arguments here
   |
help: must be of the form
   |
LL - #![feature(foo(bar), foo = "baz")]
LL + #![feature(feature1, feature2, ...)]
   |

error[E0539]: malformed `feature` attribute input
  --> $DIR/gated-bad-feature.rs:2:4
   |
LL | #![feature]
   |    ^^^^^^^ expected this to be a list
   |
help: must be of the form
   |
LL | #![feature(feature1, feature2, ...)]
   |           +++++++++++++++++++++++++

error[E0539]: malformed `feature` attribute input
  --> $DIR/gated-bad-feature.rs:3:4
   |
LL | #![feature = "foo"]
   |    ^^^^^^^^-------
   |            |
   |            expected this to be a list
   |
help: must be of the form
   |
LL - #![feature = "foo"]
LL + #![feature(feature1, feature2, ...)]
   |

error: aborting due to 4 previous errors

Some errors have detailed explanations: E0539, E0565.
For more information about an error, try `rustc --explain E0539`.
"#;
    assert_eq!(ui("gated-bad-feature.rs", "2015", source), stderr);
}

#[test]
fn a_test_runner_is_a_path_and_rustc_abi_a_word() {
    // Our own: the `test_runner` and `rustc_abi` lines of tests/ui/attributes/args-checked.rs,
    // whose other lines are other parsers'.
    let source = r#"#![feature(rustc_attrs)]
#![feature(custom_test_frameworks)]
#![allow(unused_attributes)]

#![test_runner(x = 5)]
#![test_runner(x(x,y,z))]

#[rustc_abi(debug = 5)]
#[rustc_abi(debug(x, y, z))]
fn main() {}
"#;
    let stderr = r#"error[E0565]: malformed `test_runner` attribute input
  --> $DIR/args-checked.rs:5:4
   |
LL | #![test_runner(x = 5)]
   |    ^^^^^^^^^^^^-----^
   |                |
   |                didn't expect a literal here
   |
help: must be of the form
   |
LL - #![test_runner(x = 5)]
LL + #![test_runner(path)]
   |

error[E0565]: malformed `test_runner` attribute input
  --> $DIR/args-checked.rs:6:4
   |
LL | #![test_runner(x(x,y,z))]
   |    ^^^^^^^^^^^^--------^
   |                |
   |                didn't expect a literal here
   |
help: must be of the form
   |
LL - #![test_runner(x(x,y,z))]
LL + #![test_runner(path)]
   |

error[E0539]: malformed `rustc_abi` attribute input
  --> $DIR/args-checked.rs:8:3
   |
LL | #[rustc_abi(debug = 5)]
   |   ^^^^^^^^^-----------
   |            |
   |            valid arguments are `assert_eq` or `debug`
   |
help: try changing it to one of the following valid forms of the attribute
   |
LL - #[rustc_abi(debug = 5)]
LL + #[rustc_abi(assert_eq)]
   |
LL - #[rustc_abi(debug = 5)]
LL + #[rustc_abi(debug)]
   |

error[E0539]: malformed `rustc_abi` attribute input
  --> $DIR/args-checked.rs:9:3
   |
LL | #[rustc_abi(debug(x, y, z))]
   |   ^^^^^^^^^----------------
   |            |
   |            valid arguments are `assert_eq` or `debug`
   |
help: try changing it to one of the following valid forms of the attribute
   |
LL - #[rustc_abi(debug(x, y, z))]
LL + #[rustc_abi(assert_eq)]
   |
LL - #[rustc_abi(debug(x, y, z))]
LL + #[rustc_abi(debug)]
   |

error: aborting due to 4 previous errors

Some errors have detailed explanations: E0539, E0565.
For more information about an error, try `rustc --explain E0539`.
"#;
    assert_eq!(ui("args-checked.rs", "2015", source), stderr);
}

#[test]
fn should_panic_takes_only_an_expected_message() {
    // Our own: tests/ui/test-attrs/test-should-panic-attr.rs without `--test`, its `#[test]`s
    // and their bodies, which need std's `panic!`; the parser's errors are the same.
    let source = r#"#[should_panic(expected)]
fn test2() {}

#[should_panic(expect)]
fn test3() {}

#[should_panic(expected(foo, bar))]
fn test4() {}

#[should_panic(expected = "foo", bar)]
fn test5() {}

fn main() {}
"#;
    let stderr = r#"error[E0539]: malformed `should_panic` attribute input
  --> $DIR/test-should-panic-attr.rs:1:3
   |
LL | #[should_panic(expected)]
   |   ^^^^^^^^^^^^^--------^
   |                |
   |                expected this to be of the form `expected = "..."`
   |
   = note: for more information, visit <https://doc.rust-lang.org/reference/attributes/testing.html#the-should_panic-attribute>
help: try changing it to one of the following valid forms of the attribute
   |
LL - #[should_panic(expected)]
LL + #[should_panic]
   |
LL - #[should_panic(expected)]
LL + #[should_panic = "reason"]
   |
LL | #[should_panic(expected = "reason")]
   |                         ++++++++++

error[E0539]: malformed `should_panic` attribute input
  --> $DIR/test-should-panic-attr.rs:4:3
   |
LL | #[should_panic(expect)]
   |   ^^^^^^^^^^^^^------^
   |                |
   |                expected this to be of the form `expected = "..."`
   |
   = note: for more information, visit <https://doc.rust-lang.org/reference/attributes/testing.html#the-should_panic-attribute>
help: try changing it to one of the following valid forms of the attribute
   |
LL - #[should_panic(expect)]
LL + #[should_panic]
   |
LL - #[should_panic(expect)]
LL + #[should_panic = "reason"]
   |
LL | #[should_panic(expected = "reason")]
   |                      +++++++++++++

error[E0539]: malformed `should_panic` attribute input
  --> $DIR/test-should-panic-attr.rs:7:3
   |
LL | #[should_panic(expected(foo, bar))]
   |   ^^^^^^^^^^^^^------------------^
   |                |
   |                expected this to be of the form `expected = "..."`
   |
   = note: for more information, visit <https://doc.rust-lang.org/reference/attributes/testing.html#the-should_panic-attribute>
help: try changing it to one of the following valid forms of the attribute
   |
LL - #[should_panic(expected(foo, bar))]
LL + #[should_panic]
   |
LL - #[should_panic(expected(foo, bar))]
LL + #[should_panic = "reason"]
   |
LL - #[should_panic(expected(foo, bar))]
LL + #[should_panic(expected = "reason")]
   |

error[E0805]: malformed `should_panic` attribute input
  --> $DIR/test-should-panic-attr.rs:10:3
   |
LL | #[should_panic(expected = "foo", bar)]
   |   ^^^^^^^^^^^^-----------------------
   |               |
   |               expected a single argument here
   |
   = note: for more information, visit <https://doc.rust-lang.org/reference/attributes/testing.html#the-should_panic-attribute>
help: try changing it to one of the following valid forms of the attribute
   |
LL - #[should_panic(expected = "foo", bar)]
LL + #[should_panic]
   |
LL - #[should_panic(expected = "foo", bar)]
LL + #[should_panic = "reason"]
   |
LL - #[should_panic(expected = "foo", bar)]
LL + #[should_panic(expected = "reason")]
   |

error: aborting due to 4 previous errors

Some errors have detailed explanations: E0539, E0805.
For more information about an error, try `rustc --explain E0539`.
"#;
    assert_eq!(ui("test-should-panic-attr.rs", "2015", source), stderr);
}

#[test]
fn must_use_is_not_a_list() {
    // tests/ui/attributes/malformed-must_use.rs
    let source = r#"#[must_use()] //~ ERROR malformed
struct Test;

fn main() {}
"#;
    let stderr = r#"error[E0539]: malformed `must_use` attribute input
  --> $DIR/malformed-must_use.rs:1:3
   |
LL | #[must_use()]
   |   ^^^^^^^^--
   |           |
   |           didn't expect a list here
   |
   = note: for more information, visit <https://doc.rust-lang.org/reference/attributes/diagnostics.html#the-must_use-attribute>
help: try changing it to one of the following valid forms of the attribute
   |
LL - #[must_use()]
LL + #[must_use]
   |
LL - #[must_use()]
LL + #[must_use = "reason"]
   |

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0539`.
"#;
    assert_eq!(ui("malformed-must_use.rs", "2015", source), stderr);
}

#[test]
fn non_exhaustive_takes_no_arguments() {
    // tests/ui/rfcs/rfc-2008-non-exhaustive/invalid-attribute.rs
    let source = r#"#[non_exhaustive(anything)]
//~^ ERROR malformed `non_exhaustive` attribute
struct Foo;

#[non_exhaustive]
//~^ ERROR attribute cannot be used on
trait Bar { }

#[non_exhaustive]
//~^ ERROR attribute cannot be used on
union Baz {
    f1: u16,
    f2: u16
}

fn main() { }
"#;
    let stderr = r#"error[E0565]: malformed `non_exhaustive` attribute input
  --> $DIR/invalid-attribute.rs:1:3
   |
LL | #[non_exhaustive(anything)]
   |   ^^^^^^^^^^^^^^----------
   |                 |
   |                 didn't expect any arguments here
   |
help: must be of the form
   |
LL - #[non_exhaustive(anything)]
LL + #[non_exhaustive]
   |

error: the `non_exhaustive` attribute cannot be used on traits
  --> $DIR/invalid-attribute.rs:5:3
   |
LL | #[non_exhaustive]
   |   ^^^^^^^^^^^^^^
   |
   = help: the `non_exhaustive` attribute can be applied to data types and enum variants

error: the `non_exhaustive` attribute cannot be used on unions
  --> $DIR/invalid-attribute.rs:9:3
   |
LL | #[non_exhaustive]
   |   ^^^^^^^^^^^^^^
   |
   = help: the `non_exhaustive` attribute can be applied to enum variants, enums, and structs

error: aborting due to 3 previous errors

For more information about this error, try `rustc --explain E0565`.
"#;
    assert_eq!(ui("invalid-attribute.rs", "2015", source), stderr);
}

#[test]
fn the_path_of_an_inline_module_is_a_string() {
    // Our own: tests/ui/modules/path-invalid-form.rs on an inline module, whose `#[path]`
    // expansion reads as well, where the test's `mod foo;` has no file.
    let source = r#"#[path = 123]
mod foo {}

fn main() {}
"#;
    let stderr = r#"error: malformed `path` attribute input
  --> $DIR/path-invalid-form.rs:1:1
   |
LL | #[path = 123]
   | ^^^^^^^^^^^^^
   |
   = note: for more information, visit <https://doc.rust-lang.org/reference/items/modules.html#the-path-attribute>
help: must be of the form
   |
LL - #[path = 123]
LL + #[path = "file"]
   |

error: aborting due to 1 previous error

"#;
    assert_eq!(ui("path-invalid-form.rs", "2015", source), stderr);
}

#[test]
fn a_lint_attribute_is_a_list() {
    // Our own: the first line of tests/ui/lint/lint-malformed.rs, whose second is the lint
    // levels' (E0452).
    let source = r#"#![deny = "foo"] //~ ERROR malformed `deny` attribute input
fn main() { }
"#;
    let stderr = r#"error: malformed `deny` attribute input
  --> $DIR/lint-malformed.rs:1:1
   |
LL | #![deny = "foo"]
   | ^^^^^^^^^^^^^^^^
   |
   = note: for more information, visit <https://doc.rust-lang.org/reference/attributes/diagnostics.html#lint-check-attributes>
help: the following are the possible correct uses
   |
LL - #![deny = "foo"]
LL + #![deny(lint1)]
   |
LL - #![deny = "foo"]
LL + #![deny(lint1, lint2, ...)]
   |
LL - #![deny = "foo"]
LL + #![deny(lint1, lint2, lint3, reason = "...")]
   |

error: aborting due to 1 previous error

"#;
    assert_eq!(ui("lint-malformed.rs", "2015", source), stderr);
}

#[test]
fn a_lint_list_of_name_values_is_a_meta_item_list() {
    // tests/ui/error-codes/E0452.rs: the list reads as meta items, so `check_attr` says
    // nothing; the errors are the lint levels'.
    let source = r#"#![allow(foo = "")] //~ ERROR E0452
                    //~| ERROR E0452
                    //~| ERROR E0452
                    //~| ERROR E0452
                    //~| ERROR E0452
                    //~| ERROR E0452
fn main() {
}
"#;
    assert_reports(
        &ui("E0452.rs", "2015", source),
        &[
            error(1, &[]),
            error(1, &[]),
            error(1, &[]),
            error(1, &[]),
            error(1, &[]),
            error(1, &[]),
        ],
    );
}

#[test]
fn a_lint_attribute_list_is_in_parentheses() {
    // tests/ui/malformed/malformed-meta-delim.rs
    let source = r#"fn main() {}

#[allow { foo_lint } ]
//~^ ERROR wrong meta list delimiters
//~| HELP the delimiters should be `(` and `)`
fn delim_brace() {}

#[allow [ foo_lint ] ]
//~^ ERROR wrong meta list delimiters
//~| HELP the delimiters should be `(` and `)`
fn delim_bracket() {}
"#;
    let stderr = r#"error: wrong meta list delimiters
  --> $DIR/malformed-meta-delim.rs:3:9
   |
LL | #[allow { foo_lint } ]
   |         ^^^^^^^^^^^^
   |
help: the delimiters should be `(` and `)`
   |
LL - #[allow { foo_lint } ]
LL + #[allow ( foo_lint ) ]
   |

error: wrong meta list delimiters
  --> $DIR/malformed-meta-delim.rs:8:9
   |
LL | #[allow [ foo_lint ] ]
   |         ^^^^^^^^^^^^
   |
help: the delimiters should be `(` and `)`
   |
LL - #[allow [ foo_lint ] ]
LL + #[allow ( foo_lint ) ]
   |

error: aborting due to 2 previous errors

"#;
    assert_eq!(ui("malformed-meta-delim.rs", "2015", source), stderr);
}

#[test]
fn a_lint_attribute_list_is_of_meta_items() {
    // Our own: rustc's parser of a meta list after a path, where tests/ui/parser/attribute/
    // attr-incomplete.rs gives it in a `cfg` (which `checks::cfg` parses).
    let source = r#"#[allow(dead_code -)]
fn f() {}

fn main() {}
"#;
    let stderr = r#"error: expected one of `(`, `,`, `::`, or `=`, found `-`
  --> $DIR/allow-meta.rs:1:19
   |
LL | #[allow(dead_code -)]
   |                   ^ expected one of `(`, `,`, `::`, or `=`

error: aborting due to 1 previous error

"#;
    assert_eq!(ui("allow-meta.rs", "2015", source), stderr);
}

#[test]
fn the_value_of_a_key_value_attribute_is_an_unsuffixed_literal() {
    // Our own: `check_attr` of a tool attribute, where tests/ui/malformed/
    // malformed-interpolated.rs gives these in a builtin attribute.
    let source = r#"#[rustfmt::skip = 1 + 1]
fn f() {}

#[rustfmt::skip = 1u8]
fn g() {}

fn main() {}
"#;
    let stderr = r#"error: attribute value must be a literal
  --> $DIR/tool-value.rs:1:19
   |
LL | #[rustfmt::skip = 1 + 1]
   |                   ^^^^^

error: suffixed literals are not allowed in attributes
  --> $DIR/tool-value.rs:4:19
   |
LL | #[rustfmt::skip = 1u8]
   |                   ^^^
   |
   = help: instead of using a suffixed literal (`1u8`, `1.0f32`, etc.), use an unsuffixed version (`1`, `1.0`, etc.)

error: aborting due to 2 previous errors

"#;
    assert_eq!(ui("tool-value.rs", "2015", source), stderr);
}

#[test]
fn cfg_accessible_takes_one_path() {
    // tests/ui/conditional-compilation/cfg_accessible-input-validation.rs
    let source = r#"#![feature(cfg_accessible)]

#[cfg_accessible] //~ ERROR malformed `cfg_accessible` attribute input
struct S1;

#[cfg_accessible = "value"] //~ ERROR malformed `cfg_accessible` attribute input
struct S2;

#[cfg_accessible()] //~ ERROR `cfg_accessible` path is not specified
struct S3;

#[cfg_accessible(std, core)] //~ ERROR multiple `cfg_accessible` paths are specified
struct S4;

#[cfg_accessible("std")] //~ ERROR `cfg_accessible` path cannot be a literal
struct S5;

#[cfg_accessible(std = "value")] //~ ERROR `cfg_accessible` path cannot accept arguments
struct S6;

#[cfg_accessible(std(value))] //~ ERROR `cfg_accessible` path cannot accept arguments
struct S7;

fn main() {}
"#;
    assert_reports(&ui_builtins("cfg_accessible-input-validation.rs", "2015", source), &[error(3, &["`cfg_accessible`"]), error(6, &["`cfg_accessible`"]), error(9, &["`cfg_accessible`"]), error(12, &["`cfg_accessible`"]), error(15, &["`cfg_accessible`"]), error(18, &["`cfg_accessible`"]), error(21, &["`cfg_accessible`"])]);
}
