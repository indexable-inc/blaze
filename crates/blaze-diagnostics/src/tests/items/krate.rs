//! rustc's UI tests of a binary's entry point (`tests/ui/error-codes/E0601.rs`,
//! `tests/ui/missing/`, `tests/ui/entry-point/`, `tests/ui/extern/`,
//! `tests/ui/conditional-compilation/`).

use crate::tests::{assert_reports, error, items::ui_bin};

#[test]
fn a_binary_without_main_points_at_the_end_of_its_root_file() {
    // tests/ui/error-codes/E0601.rs
    let source = "//~ ERROR `main` function not found\n";
    let stderr = r#"error[E0601]: `main` function not found in crate `E0601`
  --> $DIR/E0601.rs:1:37
   |
LL |
   |                                    ^ consider adding a `main` function to `$DIR/E0601.rs`

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0601`.
"#;
    assert_eq!(ui_bin("E0601.rs", "E0601", "2015", source), stderr);
}

#[test]
fn the_crates_span_ends_with_its_last_item() {
    // tests/ui/missing/missing-main.rs
    let source = "fn mian() { } //~ ERROR `main` function not found in crate `missing_main`\n";
    let stderr = r#"error[E0601]: `main` function not found in crate `missing_main`
  --> $DIR/missing-main.rs:1:14
   |
LL | fn mian() { }
   |              ^ consider adding a `main` function to `$DIR/missing-main.rs`

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0601`.
"#;
    assert_eq!(ui_bin("missing-main.rs", "missing_main", "2015", source), stderr);
}

#[test]
fn a_main_in_a_module_is_noted() {
    // tests/ui/entry-point/main-in-submodule.rs
    let source = r#"mod m {
    // An inferred main entry point
    // must appear at the top of the crate
    fn main() { }
} //~ ERROR `main` function not found
"#;
    let stderr = r#"error[E0601]: `main` function not found in crate `main_in_submodule`
  --> $DIR/main-in-submodule.rs:5:2
   |
LL | }
   |  ^ the main function must be defined at the crate level (in `$DIR/main-in-submodule.rs`)
   |
note: here is a function named `main`
  --> $DIR/main-in-submodule.rs:4:5
   |
LL |     fn main() { }
   |     ^^^^^^^^^
   = note: you have one or more functions named `main` not defined at the crate level
   = help: consider moving the `main` function definitions

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0601`.
"#;
    assert_eq!(ui_bin("main-in-submodule.rs", "main_in_submodule", "2015", source), stderr);
}

#[test]
fn a_const_imported_as_main_is_labelled() {
    // tests/ui/entry-point/imported_main_const_forbidden.rs
    let source = r#"pub mod foo {
    pub const BAR: usize = 42;
}

use foo::BAR as main; //~ ERROR `main` function not found in crate
"#;
    let stderr = r#"error[E0601]: `main` function not found in crate `imported_main_const_forbidden`
  --> $DIR/imported_main_const_forbidden.rs:5:22
   |
LL | use foo::BAR as main;
   |     ---------------- ^ consider adding a `main` function to `$DIR/imported_main_const_forbidden.rs`
   |     |
   |     non-function item at `crate::main` is found

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0601`.
"#;
    assert_eq!(ui_bin("imported_main_const_forbidden.rs", "imported_main_const_forbidden", "2015", source), stderr);
}

#[test]
fn a_crate_configured_out_has_no_main() {
    // tests/ui/conditional-compilation/cfg-in-crate-1.rs
    let source = "#![cfg(false)] //~ ERROR `main` function not found in crate `cfg_in_crate_1`\n";
    // A crate configured out drops the fixture's `no_core` too: it takes `std`'s prelude.
    let fixture = format!(
        "//- /std.rs crate:std edition:2015 library\npub mod prelude {{ pub mod rust_2015 {{}} }}\n\
         //- /cfg-in-crate-1.rs crate:bin:cfg_in_crate_1 edition:2015 deps:std\n{source}"
    );
    assert_reports(
        &super::ui_fixture(&fixture),
        &[
            error(1, &["`cfg_in_crate_1`", "`main`"]),
        ],
    );
}

#[test]
fn main_cannot_be_in_an_extern_block() {
    // tests/ui/extern/extern-main-issue-86110.rs
    let source = r#"// missing and missing2 exist to make sure that the error only happens on a `main` declaration
extern "C" {
    fn missing();
    fn main();
    //~^ ERROR the `main` function cannot be declared in an `extern` block
    fn missing2();
}
"#;
    let stderr = r#"error: the `main` function cannot be declared in an `extern` block
  --> $DIR/extern-main-issue-86110.rs:4:5
   |
LL |     fn main();
   |     ^^^^^^^^^^

error: aborting due to 1 previous error

"#;
    assert_eq!(ui_bin("extern-main-issue-86110.rs", "extern_main_issue_86110", "2015", source), stderr);
}
