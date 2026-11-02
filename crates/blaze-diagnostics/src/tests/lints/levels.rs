//! Lint levels from attributes, the command line and `--cap-lints`, and what rustc says of the
//! lint names in them, against rustc's UI tests as they are, under compiletest's flags
//! (`-Z deduplicate-diagnostics=no` with them).

use crate::{
    Options,
    emit::Style,
    lints::{CommandLine, Level, LintOptions},
    model,
    tests::{
        Mode, checked, checked_expanding_derives, checked_with, compiletest, normalized, ui_with,
    },
};

/// The options of a test's own `//@ compile-flags`, its lint flags `flags`.
fn flags(flags: &[&str]) -> Options {
    let args: Vec<String> = flags.iter().map(|it| (*it).to_owned()).collect();
    Options { lints: CommandLine::parse(&args).unwrap(), ..Options::default() }
}

/// What rustc writes to stderr for the one-file crate `source` named `$DIR/<name>`, in edition
/// 2015, as compiletest runs it with the test's lint flags `own`.
fn ui(name: &str, own: &[&str], source: &str) -> String {
    ui_with(name, "2015", source, &flags(own))
}

#[test]
fn lint_forbid_attr() {
    let source = r##"#![forbid(deprecated)] //~ NOTE `forbid` level set here

#[allow(deprecated)]
//~^ ERROR allow(deprecated) incompatible with previous forbid [E0453]
//~^^ NOTE overruled by previous forbid
fn main() {
}
"##;
    let expected = r##"error[E0453]: allow(deprecated) incompatible with previous forbid
  --> $DIR/lint-forbid-attr.rs:3:9
   |
LL | #![forbid(deprecated)]
   |           ---------- `forbid` level set here
LL |
LL | #[allow(deprecated)]
   |         ^^^^^^^^^^ overruled by previous forbid

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0453`.
"##;
    assert_eq!(ui("lint-forbid-attr.rs", &[], source), expected);
}

#[test]
fn lint_renamed() {
    let source = r##"#[deny(bare_trait_object)]
//~^ WARN lint `bare_trait_object` has been renamed to `bare_trait_objects`
#[deny(unused)]
fn main() { let unused = (); } //~ ERROR unused
"##;
    let expected = r##"warning: lint `bare_trait_object` has been renamed to `bare_trait_objects`
  --> $DIR/lint-renamed.rs:1:8
   |
LL | #[deny(bare_trait_object)]
   |        ^^^^^^^^^^^^^^^^^ help: use the new name: `bare_trait_objects`
   |
   = note: `#[warn(renamed_and_removed_lints)]` on by default

error: unused variable: `unused`
  --> $DIR/lint-renamed.rs:4:17
   |
LL | fn main() { let unused = (); }
   |                 ^^^^^^ help: if this is intentional, prefix it with an underscore: `_unused`
   |
note: the lint level is defined here
  --> $DIR/lint-renamed.rs:3:8
   |
LL | #[deny(unused)]
   |        ^^^^^^
   = note: `#[deny(unused_variables)]` implied by `#[deny(unused)]`

error: aborting due to 1 previous error; 1 warning emitted

"##;
    assert_eq!(ui("lint-renamed.rs", &[], source), expected);
}

#[test]
fn lint_removed() {
    let source = r##"// The raw_pointer_derived lint was removed, but is now reported by
// the renamed_and_removed_lints lint, which means it's a warning by
// default, and allowed in cargo dependency builds.
// cc #30346

#[deny(raw_pointer_derive)] //~ WARN `raw_pointer_derive` has been removed
#[deny(unused_variables)]
fn main() { let unused = (); } //~ ERROR unused
"##;
    let expected = r##"warning: lint `raw_pointer_derive` has been removed: using derive with raw pointers is ok
  --> $DIR/lint-removed.rs:6:8
   |
LL | #[deny(raw_pointer_derive)]
   |        ^^^^^^^^^^^^^^^^^^
   |
   = note: `#[warn(renamed_and_removed_lints)]` on by default

error: unused variable: `unused`
  --> $DIR/lint-removed.rs:8:17
   |
LL | fn main() { let unused = (); }
   |                 ^^^^^^ help: if this is intentional, prefix it with an underscore: `_unused`
   |
note: the lint level is defined here
  --> $DIR/lint-removed.rs:7:8
   |
LL | #[deny(unused_variables)]
   |        ^^^^^^^^^^^^^^^^

error: aborting due to 1 previous error; 1 warning emitted

"##;
    assert_eq!(ui("lint-removed.rs", &[], source), expected);
}

#[test]
fn reasons_erroneous() {
    let source = r##"//@ compile-flags: -Zdeduplicate-diagnostics=yes

#![warn(absolute_paths_not_starting_with_crate, reason = 0)]
//~^ ERROR malformed lint attribute
//~| NOTE reason must be a string literal
#![warn(anonymous_parameters, reason = b"consider these, for we have condemned them")]
//~^ ERROR malformed lint attribute
//~| NOTE reason must be a string literal
#![warn(bare_trait_objects, reasons = "leaders to no sure land, guides their bearings lost")]
//~^ ERROR malformed lint attribute
//~| NOTE bad attribute argument
#![warn(unsafe_code, blerp = "or in league with robbers have reversed the signposts")]
//~^ ERROR malformed lint attribute
//~| NOTE bad attribute argument
#![warn(elided_lifetimes_in_paths, reason("disrespectful to ancestors", "irresponsible to heirs"))]
//~^ ERROR malformed lint attribute
//~| NOTE bad attribute argument
#![warn(ellipsis_inclusive_range_patterns, reason = "born barren", reason = "a freak growth")]
//~^ ERROR malformed lint attribute
//~| NOTE reason in lint attribute must come last
#![warn(keyword_idents, reason = "root in rubble", macro_use_extern_crate)]
//~^ ERROR malformed lint attribute
//~| NOTE reason in lint attribute must come last
#![warn(missing_copy_implementations, reason)]
//~^ WARN unknown lint
//~| NOTE `#[warn(unknown_lints)]` on by default

fn main() {}
"##;
    let expected = r##"error[E0452]: malformed lint attribute input
  --> $DIR/reasons-erroneous.rs:3:58
   |
LL | #![warn(absolute_paths_not_starting_with_crate, reason = 0)]
   |                                                          ^ reason must be a string literal

error[E0452]: malformed lint attribute input
  --> $DIR/reasons-erroneous.rs:6:40
   |
LL | #![warn(anonymous_parameters, reason = b"consider these, for we have condemned them")]
   |                                        ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ reason must be a string literal

error[E0452]: malformed lint attribute input
  --> $DIR/reasons-erroneous.rs:9:29
   |
LL | #![warn(bare_trait_objects, reasons = "leaders to no sure land, guides their bearings lost")]
   |                             ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ bad attribute argument

error[E0452]: malformed lint attribute input
  --> $DIR/reasons-erroneous.rs:12:22
   |
LL | #![warn(unsafe_code, blerp = "or in league with robbers have reversed the signposts")]
   |                      ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ bad attribute argument

error[E0452]: malformed lint attribute input
  --> $DIR/reasons-erroneous.rs:15:36
   |
LL | #![warn(elided_lifetimes_in_paths, reason("disrespectful to ancestors", "irresponsible to heirs"))]
   |                                    ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ bad attribute argument

error[E0452]: malformed lint attribute input
  --> $DIR/reasons-erroneous.rs:18:44
   |
LL | #![warn(ellipsis_inclusive_range_patterns, reason = "born barren", reason = "a freak growth")]
   |                                            ^^^^^^^^^^^^^^^^^^^^^^ reason in lint attribute must come last

error[E0452]: malformed lint attribute input
  --> $DIR/reasons-erroneous.rs:21:25
   |
LL | #![warn(keyword_idents, reason = "root in rubble", macro_use_extern_crate)]
   |                         ^^^^^^^^^^^^^^^^^^^^^^^^^ reason in lint attribute must come last

warning: unknown lint: `reason`
  --> $DIR/reasons-erroneous.rs:24:39
   |
LL | #![warn(missing_copy_implementations, reason)]
   |                                       ^^^^^^
   |
   = note: `#[warn(unknown_lints)]` on by default

error: aborting due to 7 previous errors; 1 warning emitted

For more information about this error, try `rustc --explain E0452`.
"##;
    let options = Options { keep_duplicates: false, ..compiletest(Mode::Other, &Options::default()) };
    let fixture = format!("//- /reasons-erroneous.rs crate:other:main edition:2015\n{source}");
    let stderr = checked_with(&fixture, "$DIR/", &options).stderr_with(Style { ui_testing: true, ..Style::default() });
    assert_eq!(normalized(&stderr), expected);
}

/// rustc's test panics in `main` with std's macro; here `main` loops, on the same line, as the
/// crate has no std.
#[test]
fn unknown_lint_suggestions() {
    let source = r##"//@ check-pass

// this tests the `unknown_lint` lint, especially the suggestions

// the suggestion only appears if a lint with the lowercase name exists
#[allow(FOO_BAR)]
//~^ WARNING unknown lint

// the suggestion appears on all-uppercase names
#[warn(DEAD_CODE)]
//~^ WARNING unknown lint
//~| HELP did you mean

// the suggestion appears also on mixed-case names
#[deny(Warnings)]
//~^ WARNING unknown lint
//~| HELP did you mean

#[deny(rust_2018_idiots)] //~ WARNING unknown lint
                           //~| HELP did you mean

fn main() {
    loop {}
}
"##;
    let expected = r##"warning: unknown lint: `FOO_BAR`
  --> $DIR/suggestions.rs:6:9
   |
LL | #[allow(FOO_BAR)]
   |         ^^^^^^^
   |
   = note: `#[warn(unknown_lints)]` on by default

warning: unknown lint: `DEAD_CODE`
  --> $DIR/suggestions.rs:10:8
   |
LL | #[warn(DEAD_CODE)]
   |        ^^^^^^^^^ help: did you mean: `dead_code`

warning: unknown lint: `Warnings`
  --> $DIR/suggestions.rs:15:8
   |
LL | #[deny(Warnings)]
   |        ^^^^^^^^ help: did you mean (notice the capitalization): `warnings`

warning: unknown lint: `rust_2018_idiots`
  --> $DIR/suggestions.rs:19:8
   |
LL | #[deny(rust_2018_idiots)]
   |        ^^^^^^^^^^^^^^^^ help: did you mean: `rust_2018_idioms`

warning: 4 warnings emitted

"##;
    assert_eq!(ui("suggestions.rs", &[], source), expected);
}

#[test]
fn crate_level_only_lint() {
    let source = r##"#![deny(uncommon_codepoints, unused_attributes)]

mod foo {
#![allow(uncommon_codepoints)]
//~^ ERROR allow(uncommon_codepoints) is ignored unless specified at crate level [unused_attributes]

#[allow(uncommon_codepoints)]
//~^ ERROR allow(uncommon_codepoints) is ignored unless specified at crate level [unused_attributes]
const BAR: f64 = 0.000001;

}

#[allow(uncommon_codepoints)]
//~^ ERROR allow(uncommon_codepoints) is ignored unless specified at crate level [unused_attributes]
fn main() {
}
"##;
    let expected = r##"error: allow(uncommon_codepoints) is ignored unless specified at crate level
  --> $DIR/crate_level_only_lint.rs:4:10
   |
LL | #![allow(uncommon_codepoints)]
   |          ^^^^^^^^^^^^^^^^^^^
   |
note: the lint level is defined here
  --> $DIR/crate_level_only_lint.rs:1:30
   |
LL | #![deny(uncommon_codepoints, unused_attributes)]
   |                              ^^^^^^^^^^^^^^^^^

error: allow(uncommon_codepoints) is ignored unless specified at crate level
  --> $DIR/crate_level_only_lint.rs:7:9
   |
LL | #[allow(uncommon_codepoints)]
   |         ^^^^^^^^^^^^^^^^^^^

error: allow(uncommon_codepoints) is ignored unless specified at crate level
  --> $DIR/crate_level_only_lint.rs:13:9
   |
LL | #[allow(uncommon_codepoints)]
   |         ^^^^^^^^^^^^^^^^^^^

error: aborting due to 3 previous errors

"##;
    assert_eq!(ui("crate_level_only_lint.rs", &[], source), expected);
}

#[test]
fn unknown_lints_in_cfg_attr() {
    let source = r##"#![deny(warnings)]

// Ensure that unknown lints inside cfg-attr's are linted for

#![cfg_attr(true, allow(nonex_lint_top_level))]
//~^ ERROR unknown lint
#![cfg_attr(true, allow(bare_trait_object))]
//~^ ERROR has been renamed

#[cfg_attr(true, allow(nonex_lint_mod))]
//~^ ERROR unknown lint
mod baz {
    #![cfg_attr(true, allow(nonex_lint_mod_inner))]
    //~^ ERROR unknown lint
}

#[cfg_attr(true, allow(nonex_lint_fn))]
//~^ ERROR unknown lint
pub fn main() {}

macro_rules! bar {
    ($($t:tt)*) => {
        $($t)*
    };
}

bar!(
    #[cfg_attr(true, allow(nonex_lint_in_macro))]
    //~^ ERROR unknown lint
    pub fn _bar() {}
);

// No warning for non-applying cfg
#[cfg_attr(false, allow(nonex_lint_fn))]
pub fn _foo() {}

// Allowing unknown lints works if inside cfg_attr
#[cfg_attr(true, allow(unknown_lints))]
mod bar_allowed {
    #[allow(nonex_lint_fn)]
    fn _foo() {}
}

// ... but not if the cfg_attr doesn't evaluate
#[cfg_attr(false, allow(unknown_lints))]
mod bar_not_allowed {
    #[allow(nonex_lint_fn)]
    //~^ ERROR unknown lint
    fn _foo() {}
}
"##;
    let expected = r##"error: unknown lint: `nonex_lint_top_level`
  --> $DIR/unknown-lints-in-cfg-attr-97094.rs:5:25
   |
LL | #![cfg_attr(true, allow(nonex_lint_top_level))]
   |                         ^^^^^^^^^^^^^^^^^^^^
   |
note: the lint level is defined here
  --> $DIR/unknown-lints-in-cfg-attr-97094.rs:1:9
   |
LL | #![deny(warnings)]
   |         ^^^^^^^^
   = note: `#[deny(unknown_lints)]` implied by `#[deny(warnings)]`

error: lint `bare_trait_object` has been renamed to `bare_trait_objects`
  --> $DIR/unknown-lints-in-cfg-attr-97094.rs:7:25
   |
LL | #![cfg_attr(true, allow(bare_trait_object))]
   |                         ^^^^^^^^^^^^^^^^^ help: use the new name: `bare_trait_objects`
   |
   = note: `#[deny(renamed_and_removed_lints)]` implied by `#[deny(warnings)]`

error: unknown lint: `nonex_lint_mod`
  --> $DIR/unknown-lints-in-cfg-attr-97094.rs:10:24
   |
LL | #[cfg_attr(true, allow(nonex_lint_mod))]
   |                        ^^^^^^^^^^^^^^

error: unknown lint: `nonex_lint_mod_inner`
  --> $DIR/unknown-lints-in-cfg-attr-97094.rs:13:29
   |
LL |     #![cfg_attr(true, allow(nonex_lint_mod_inner))]
   |                             ^^^^^^^^^^^^^^^^^^^^

error: unknown lint: `nonex_lint_fn`
  --> $DIR/unknown-lints-in-cfg-attr-97094.rs:17:24
   |
LL | #[cfg_attr(true, allow(nonex_lint_fn))]
   |                        ^^^^^^^^^^^^^

error: unknown lint: `nonex_lint_in_macro`
  --> $DIR/unknown-lints-in-cfg-attr-97094.rs:28:28
   |
LL |     #[cfg_attr(true, allow(nonex_lint_in_macro))]
   |                            ^^^^^^^^^^^^^^^^^^^

error: unknown lint: `nonex_lint_fn`
  --> $DIR/unknown-lints-in-cfg-attr-97094.rs:47:13
   |
LL |     #[allow(nonex_lint_fn)]
   |             ^^^^^^^^^^^^^

error: aborting due to 7 previous errors

"##;
    assert_eq!(ui("unknown-lints-in-cfg-attr-97094.rs", &[], source), expected);
}

#[test]
fn expect_with_reason() {
    let source = r##"//@ check-pass

#![warn(unused)]

#![expect(unused_variables, reason = "<This should fail and display this reason>")]
//~^ WARNING this lint expectation is unfulfilled [unfulfilled_lint_expectations]
//~| NOTE `#[warn(unfulfilled_lint_expectations)]` on by default
//~| NOTE <This should fail and display this reason>

fn main() {}
"##;
    let expected = r##"warning: this lint expectation is unfulfilled
  --> $DIR/expect_with_reason.rs:5:11
   |
LL | #![expect(unused_variables, reason = "<This should fail and display this reason>")]
   |           ^^^^^^^^^^^^^^^^
   |
   = note: <This should fail and display this reason>
   = note: `#[warn(unfulfilled_lint_expectations)]` on by default

warning: 1 warning emitted

"##;
    assert_eq!(ui("expect_with_reason.rs", &[], source), expected);
}

#[test]
fn expect_nested_lint_levels() {
    let source = r##"// ignore-tidy-file-linelength

#![warn(unused_mut)]

#[expect(
    unused_mut,
    //~^ WARNING this lint expectation is unfulfilled [unfulfilled_lint_expectations]
    //~| NOTE `#[warn(unfulfilled_lint_expectations)]` on by default
    //~| NOTE this `expect` is overridden by a `allow` attribute before the `unused_mut` lint is triggered
    reason = "this `expect` is overridden by a `allow` attribute before the `unused_mut` lint is triggered"
)]
mod foo {
    fn bar() {
        #[allow(
            unused_mut,
            reason = "this overrides the previous `expect` lint level and allows the `unused_mut` lint here"
        )]
        let mut v = 0;
    }
}

#[expect(
    unused_mut,
    //~^ WARNING this lint expectation is unfulfilled [unfulfilled_lint_expectations]
    //~| NOTE this `expect` is overridden by a `warn` attribute before the `unused_mut` lint is triggered
    reason = "this `expect` is overridden by a `warn` attribute before the `unused_mut` lint is triggered"
)]
mod oof {
    #[warn(
        unused_mut,
        //~^ NOTE the lint level is defined here
        reason = "this overrides the previous `expect` lint level and warns about the `unused_mut` lint here"
    )]
    fn bar() {
        let mut v = 0;
        //~^ WARNING variable does not need to be mutable [unused_mut]
        //~| NOTE this overrides the previous `expect` lint level and warns about the `unused_mut` lint here
        //~| HELP remove this `mut`
    }
}

#[expect(unused_variables)]
//~^ WARNING this lint expectation is unfulfilled
#[forbid(unused_variables)]
//~^ NOTE the lint level is defined here
fn check_expect_then_forbid() {
    let this_is_my_function = 3;
    //~^ ERROR unused variable: `this_is_my_function` [unused_variables]
    //~| HELP if this is intentional, prefix it with an underscore
}

fn main() {}
"##;
    let expected = r##"warning: variable does not need to be mutable
  --> $DIR/expect_nested_lint_levels.rs:35:13
   |
LL |         let mut v = 0;
   |             ----^
   |             |
   |             help: remove this `mut`
   |
   = note: this overrides the previous `expect` lint level and warns about the `unused_mut` lint here
note: the lint level is defined here
  --> $DIR/expect_nested_lint_levels.rs:30:9
   |
LL |         unused_mut,
   |         ^^^^^^^^^^

error: unused variable: `this_is_my_function`
  --> $DIR/expect_nested_lint_levels.rs:47:9
   |
LL |     let this_is_my_function = 3;
   |         ^^^^^^^^^^^^^^^^^^^ help: if this is intentional, prefix it with an underscore: `_this_is_my_function`
   |
note: the lint level is defined here
  --> $DIR/expect_nested_lint_levels.rs:44:10
   |
LL | #[forbid(unused_variables)]
   |          ^^^^^^^^^^^^^^^^

warning: this lint expectation is unfulfilled
  --> $DIR/expect_nested_lint_levels.rs:6:5
   |
LL |     unused_mut,
   |     ^^^^^^^^^^
   |
   = note: this `expect` is overridden by a `allow` attribute before the `unused_mut` lint is triggered
   = note: `#[warn(unfulfilled_lint_expectations)]` on by default

warning: this lint expectation is unfulfilled
  --> $DIR/expect_nested_lint_levels.rs:23:5
   |
LL |     unused_mut,
   |     ^^^^^^^^^^
   |
   = note: this `expect` is overridden by a `warn` attribute before the `unused_mut` lint is triggered

warning: this lint expectation is unfulfilled
  --> $DIR/expect_nested_lint_levels.rs:42:10
   |
LL | #[expect(unused_variables)]
   |          ^^^^^^^^^^^^^^^^

error: aborting due to 1 previous error; 4 warnings emitted

"##;
    assert_eq!(ui("expect_nested_lint_levels.rs", &[], source), expected);
}

#[test]
fn warn_unknown_unstable_lint_inline() {
    let source = r##"//@ check-pass

#![warn(unknown_lints)]
#![allow(test_unstable_lint)]
//~^ WARNING unknown lint: `test_unstable_lint`

fn main() {}
"##;
    let expected = r##"warning: unknown lint: `test_unstable_lint`
  --> $DIR/warn-unknown-unstable-lint-inline.rs:4:10
   |
LL | #![allow(test_unstable_lint)]
   |          ^^^^^^^^^^^^^^^^^^
   |
   = note: the `test_unstable_lint` lint is unstable
   = help: add `#![feature(test_unstable_lint)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date
note: the lint level is defined here
  --> $DIR/warn-unknown-unstable-lint-inline.rs:3:9
   |
LL | #![warn(unknown_lints)]
   |         ^^^^^^^^^^^^^

warning: 1 warning emitted

"##;
    assert_eq!(ui("warn-unknown-unstable-lint-inline.rs", &[], source), expected);
}

#[test]
fn register_tool_lint() {
    let source = r##"#![crate_type = "lib"]
#![feature(register_tool)]
#![register_tool(xyz)]
#![warn(xyz::my_lint)] // this should not error
#![warn(abc::my_lint)]
//~^ ERROR unknown tool name `abc` found in scoped lint
//~| HELP add `#![register_tool(abc)]`
//~| ERROR unknown tool name `abc`
//~| HELP add `#![register_tool(abc)]`
"##;
    let expected = r##"error[E0710]: unknown tool name `abc` found in scoped lint: `abc::my_lint`
  --> $DIR/register-tool-lint.rs:5:9
   |
LL | #![warn(abc::my_lint)]
   |         ^^^
   |
   = help: add `#![register_tool(abc)]` to the crate root

error[E0710]: unknown tool name `abc` found in scoped lint: `abc::my_lint`
  --> $DIR/register-tool-lint.rs:5:9
   |
LL | #![warn(abc::my_lint)]
   |         ^^^
   |
   = help: add `#![register_tool(abc)]` to the crate root
   = note: duplicate diagnostic emitted due to `-Z deduplicate-diagnostics=no`

error: aborting due to 2 previous errors

For more information about this error, try `rustc --explain E0710`.
"##;
    assert_eq!(ui("register-tool-lint.rs", &[], source), expected);
}

#[test]
fn lint_renamed_cmdline() {
    let source = r##"//@ compile-flags:-D bare_trait_object
//@ dont-require-annotations: NOTE

#[deny(unused)]
fn main() { let unused = (); } //~ ERROR unused variable: `unused`

//~? WARN lint `bare_trait_object` has been renamed to `bare_trait_objects`
//~? WARN lint `bare_trait_object` has been renamed to `bare_trait_objects`
//~? WARN lint `bare_trait_object` has been renamed to `bare_trait_objects`
//~? NOTE requested on the command line with `-D bare_trait_object`
//~? NOTE `#[warn(renamed_and_removed_lints)]` on by default
"##;
    let expected = r##"warning: lint `bare_trait_object` has been renamed to `bare_trait_objects`
   |
   = help: use the new name `bare_trait_objects`
   = note: requested on the command line with `-D bare_trait_object`
   = note: `#[warn(renamed_and_removed_lints)]` on by default

warning: lint `bare_trait_object` has been renamed to `bare_trait_objects`
   |
   = help: use the new name `bare_trait_objects`
   = note: requested on the command line with `-D bare_trait_object`
   = note: duplicate diagnostic emitted due to `-Z deduplicate-diagnostics=no`

warning: lint `bare_trait_object` has been renamed to `bare_trait_objects`
   |
   = help: use the new name `bare_trait_objects`
   = note: requested on the command line with `-D bare_trait_object`
   = note: duplicate diagnostic emitted due to `-Z deduplicate-diagnostics=no`

error: unused variable: `unused`
  --> $DIR/lint-renamed-cmdline.rs:5:17
   |
LL | fn main() { let unused = (); }
   |                 ^^^^^^ help: if this is intentional, prefix it with an underscore: `_unused`
   |
note: the lint level is defined here
  --> $DIR/lint-renamed-cmdline.rs:4:8
   |
LL | #[deny(unused)]
   |        ^^^^^^
   = note: `#[deny(unused_variables)]` implied by `#[deny(unused)]`

error: aborting due to 1 previous error; 3 warnings emitted

"##;
    assert_eq!(ui("lint-renamed-cmdline.rs", &["-D", "bare_trait_object"], source), expected);
}

#[test]
fn lint_removed_cmdline_deny() {
    let source = r##"// The raw_pointer_derived lint warns about its removal
// cc #30346

//@ compile-flags:-D renamed-and-removed-lints -D raw_pointer_derive
//@ dont-require-annotations: NOTE

#![warn(unused)]

#[deny(warnings)]
fn main() { let unused = (); } //~ ERROR unused variable: `unused`

//~? ERROR lint `raw_pointer_derive` has been removed: using derive with raw pointers is ok
//~? ERROR lint `raw_pointer_derive` has been removed: using derive with raw pointers is ok
//~? ERROR lint `raw_pointer_derive` has been removed: using derive with raw pointers is ok
//~? NOTE requested on the command line with `-D raw_pointer_derive`
//~? NOTE requested on the command line with `-D renamed-and-removed-lints`
"##;
    let expected = r##"error: lint `raw_pointer_derive` has been removed: using derive with raw pointers is ok
   |
   = note: requested on the command line with `-D raw_pointer_derive`
   = note: requested on the command line with `-D renamed-and-removed-lints`

error: lint `raw_pointer_derive` has been removed: using derive with raw pointers is ok
   |
   = note: requested on the command line with `-D raw_pointer_derive`
   = note: duplicate diagnostic emitted due to `-Z deduplicate-diagnostics=no`

error: lint `raw_pointer_derive` has been removed: using derive with raw pointers is ok
   |
   = note: requested on the command line with `-D raw_pointer_derive`
   = note: duplicate diagnostic emitted due to `-Z deduplicate-diagnostics=no`

error: unused variable: `unused`
  --> $DIR/lint-removed-cmdline-deny.rs:10:17
   |
LL | fn main() { let unused = (); }
   |                 ^^^^^^ help: if this is intentional, prefix it with an underscore: `_unused`
   |
note: the lint level is defined here
  --> $DIR/lint-removed-cmdline-deny.rs:9:8
   |
LL | #[deny(warnings)]
   |        ^^^^^^^^
   = note: `#[deny(unused_variables)]` implied by `#[deny(warnings)]`

error: aborting due to 4 previous errors

"##;
    let flags = ["-D", "renamed-and-removed-lints", "-D", "raw_pointer_derive"];
    assert_eq!(ui("lint-removed-cmdline-deny.rs", &flags, source), expected);
}

#[test]
fn cli_unknown_force_warn() {
    let source = r##"// Checks that rustc correctly errors when passed an invalid lint with
// `--force-warn`. This is a regression test for issue #86958.

//@ check-pass
//@ compile-flags: --force-warn foo-qux
//@ dont-require-annotations: NOTE

fn main() {}

//~? WARN unknown lint: `foo_qux`
//~? WARN unknown lint: `foo_qux`
//~? WARN unknown lint: `foo_qux`
//~? NOTE requested on the command line with `--force-warn foo_qux`
//~? NOTE `#[warn(unknown_lints)]` on by default
"##;
    let expected = r##"warning[E0602]: unknown lint: `foo_qux`
   |
   = note: requested on the command line with `--force-warn foo_qux`
   = note: `#[warn(unknown_lints)]` on by default

warning[E0602]: unknown lint: `foo_qux`
   |
   = note: requested on the command line with `--force-warn foo_qux`
   = note: duplicate diagnostic emitted due to `-Z deduplicate-diagnostics=no`

warning[E0602]: unknown lint: `foo_qux`
   |
   = note: requested on the command line with `--force-warn foo_qux`
   = note: duplicate diagnostic emitted due to `-Z deduplicate-diagnostics=no`

warning: 3 warnings emitted

For more information about this error, try `rustc --explain E0602`.
"##;
    assert_eq!(ui("cli-unknown-force-warn.rs", &["--force-warn", "foo-qux"], source), expected);
}

#[test]
fn command_line_register_unknown_lint_tool() {
    let source = r##"//@ compile-flags: -A unknown_tool::foo

fn main() {}

//~? ERROR unknown lint tool: `unknown_tool`
//~? ERROR unknown lint tool: `unknown_tool`
//~? ERROR unknown lint tool: `unknown_tool`
"##;
    let expected = r##"error[E0602]: unknown lint tool: `unknown_tool`
   |
   = note: requested on the command line with `-A unknown_tool::foo`

error[E0602]: unknown lint tool: `unknown_tool`
   |
   = note: requested on the command line with `-A unknown_tool::foo`
   = note: duplicate diagnostic emitted due to `-Z deduplicate-diagnostics=no`

error[E0602]: unknown lint tool: `unknown_tool`
   |
   = note: requested on the command line with `-A unknown_tool::foo`
   = note: duplicate diagnostic emitted due to `-Z deduplicate-diagnostics=no`

error: aborting due to 3 previous errors

For more information about this error, try `rustc --explain E0602`.
"##;
    let stderr = ui("command-line-register-unknown-lint-tool.rs", &["-A", "unknown_tool::foo"], source);
    assert_eq!(stderr, expected);
}

/// The levels of the lints of `fixture` checked with the lint flags `args`.
fn lint_levels(fixture: &str, args: &[&str]) -> Vec<(String, model::Level)> {
    let args: Vec<String> = args.iter().map(|it| (*it).to_owned()).collect();
    let options = Options { lints: CommandLine::parse(&args).unwrap(), ..Options::default() };
    checked_with(fixture, "", &options)
        .diags
        .iter()
        .filter_map(|it| it.lint_name().map(|name| (name.to_owned(), it.level)))
        .collect()
}

#[test]
fn cap_lints_lowers_a_denied_lint_and_drops_an_allowed_one() {
    let fixture = "//- /main.rs crate:other:main\n#[deny(unused)]\nfn main() { let unused = (); }\n";
    let unused = |level| vec![("unused_variables".to_owned(), level)];
    assert_eq!(lint_levels(fixture, &[]), unused(model::Level::Error));
    assert_eq!(lint_levels(fixture, &["--cap-lints", "warn"]), unused(model::Level::Warning));
    assert_eq!(lint_levels(fixture, &["--cap-lints=allow"]), vec![]);
}

/// `--force-warn` holds under `--cap-lints allow`, as rustc's `reveal_actual_level_spec` has it.
#[test]
fn force_warn_is_not_capped() {
    let fixture = "//- /main.rs crate:other:main\n#[allow(unused)]\nfn main() { let unused = (); }\n";
    let levels = lint_levels(fixture, &["--force-warn", "unused-variables", "--cap-lints", "allow"]);
    assert_eq!(levels, vec![("unused_variables".to_owned(), model::Level::Warning)]);
}

/// A lint inside another crate's macro is not reported, as rustc's `in_external_macro` has
/// it; the same inside the crate's own macro is.
#[test]
fn a_lint_in_another_crates_macro_is_not_reported() {
    let fixture = r#"
//- /dep.rs crate:dep
#[macro_export]
macro_rules! in_dep {
    () => { pub fn from_dep() { let unused = (); } };
}
//- /main.rs crate:other:main deps:dep
macro_rules! here {
    () => { fn from_here() { let unused = (); } };
}
dep::in_dep!();
here!();
fn main() { from_dep(); from_here(); }
"#;
    let checked = checked(fixture);
    let unused: Vec<_> = checked.diags.iter().filter(|it| it.lint_name() == Some("unused_variables")).collect();
    assert_eq!(unused.len(), 1, "{:?}", checked.messages());
    assert_eq!(unused[0].span.primary[0].file, "main.rs", "{:?}", checked.messages());
}

/// What a builtin derive writes is another crate's macro's: derive(Debug)'s `&mut Formatter`
/// and the bodies of the other derives give no lint, while the same elided path written by
/// hand does (blaze-hash's `FixedState` warned twice in the self-build).
#[test]
fn the_lints_of_what_a_builtin_derive_writes_are_not_reported() {
    let fixture = r#"//- minicore: clone, copy, default, eq, fmt, derive
//- /lib.rs crate:other
#![warn(elided_lifetimes_in_paths)]
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct FixedState;
pub fn by_hand(_: &mut core::fmt::Formatter) {}
"#;
    let checked = checked_expanding_derives(fixture, &Options::default());
    let lib = fixture.split("//- /lib.rs crate:other\n").nth(1).unwrap();
    let line = lib.find("pub fn by_hand").unwrap() as u32..lib.len() as u32;
    let lints: Vec<_> = checked
        .diags
        .iter()
        .filter_map(|it| Some((it.lint_name()?, it.span.primary[0].lo)))
        .collect();
    assert!(
        matches!(lints[..], [("elided_lifetimes_in_paths", lo)] if line.contains(&lo)),
        "{lints:?}: {:?}",
        checked.messages()
    );
}

/// As blaze-rng's `SplitMix64` under `blaze check`'s lints: derives expanded, the lint warned on the
/// command line. Each expansion names the derive it is of, so what it writes is external.
#[test]
fn the_lints_of_what_a_derive_of_a_documented_item_writes_are_not_reported() {
    // `ord` has the integers' `PartialEq`, `dispatch_from_dyn` makes `Debug` dyn-compatible:
    // what derive(PartialEq) and derive(Debug) of a field need, as with the real core.
    let fixture = r#"//- minicore: clone, copy, eq, ord, fmt, derive, dispatch_from_dyn
//- /lib.rs crate:other edition:2024
/// One stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SplitMix64(u64);
"#;
    let checked = checked_expanding_derives(fixture, &flags(&["-W", "elided-lifetimes-in-paths"]));
    let lints: Vec<_> = checked.diags.iter().filter_map(|it| it.lint_name()).collect();
    assert!(lints.is_empty(), "{lints:?}: {:?}", checked.messages());
    assert!(!checked.diags.iter().any(|it| it.is_error()), "{}", checked.stderr());
}

/// `blaze` is a tool every crate has, with the front end's `blaze::const_impure` and
/// blaze-lints' lints and group: naming them is quiet, an `#[expect]` of blaze-lints' is left
/// to it, and any other `blaze::` name is an unknown lint of a known tool.
#[test]
fn blaze_tool_lints() {
    let fixture = "//- /main.rs crate:other:main\n#![deny(blaze::const_impure)]\n#![allow(blaze::const_purity)]\n#![warn(blaze::hot)]\n#![expect(blaze::wall_clock)]\nfn main() {}\n";
    let messages = checked(fixture).messages();
    let unknown: Vec<_> = messages.iter().filter(|it| it.contains("unknown lint") || it.contains("unfulfilled")).collect();
    assert_eq!(unknown, ["unknown_lints: unknown lint: `blaze::const_purity`"], "{messages:?}");
}

#[test]
fn command_line_flags_in_order() {
    let args: Vec<String> = [
        "-A", "unused", "-Wdead-code", "--deny=warnings", "--forbid", "unsafe_code", "--force-warn", "clippy::all",
        "-C", "opt-level=3", "--edition", "2021",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    let expected = CommandLine {
        lints: LintOptions {
            opts: vec![
                ("unused".to_owned(), Level::Allow),
                ("dead_code".to_owned(), Level::Warn),
                ("warnings".to_owned(), Level::Deny),
                ("unsafe_code".to_owned(), Level::Forbid),
                ("clippy::all".to_owned(), Level::ForceWarn),
            ],
            cap: None,
        },
        codegen: true,
        ..CommandLine::default()
    };
    assert_eq!(CommandLine::parse(&args), Ok(expected));
}

#[test]
fn command_line_errors() {
    let parse = |args: &[&str]| {
        let args: Vec<String> = args.iter().map(|it| (*it).to_owned()).collect();
        CommandLine::parse(&args)
    };
    assert_eq!(parse(&["--cap-lints", "warn"]).map(|it| it.lints.cap), Ok(Some(Level::Warn)));
    assert_eq!(
        parse(&["--cap-lints", "warn", "--cap-lints=allow"]),
        Err("Option 'cap-lints' given more than once".to_owned())
    );
    assert_eq!(parse(&["--cap-lints", "loud"]), Err("unknown lint level: `loud`".to_owned()));
    assert_eq!(parse(&["-D"]), Err("Argument to option 'D' missing".to_owned()));
    assert_eq!(parse(&["--allow"]), Err("Argument to option 'allow' missing".to_owned()));
    assert_eq!(
        parse(&["--cfg", "unix", "-Awarnings", "--cfg=feature=\"std\"", "--cfgx"]).map(|it| it.cfg),
        Ok(vec!["unix".to_owned(), "feature=\"std\"".to_owned()])
    );
    assert_eq!(parse(&["--cfg"]), Err("Argument to option 'cfg' missing".to_owned()));
}
