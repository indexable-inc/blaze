//! rustc's UI tests of the attributes `-Z crate-attr` gives that it refuses
//! (`tests/ui/attributes/z-crate-attr/`), each in the file `<crate attribute>` of its text
//! `#![<attr>]`. `injection.rs` is not here: its attribute has a space, which a fixture's meta
//! line can't hold; `garbage.rs` and `inner-attr.rs` are syntax errors of the parser's own
//! words (`lower/parse.rs`).

use crate::tests::items::ui_fixture;

/// The `.stderr` of the binary `krate` of `source`, named `name`, given the crate attributes
/// `attrs` as `-Z crate-attr` gives them.
fn ui_attrs(name: &str, krate: &str, attrs: &[&str], source: &str) -> String {
    let attrs: String = attrs.iter().map(|it| format!(" crate-attr:{it}")).collect();
    ui_fixture(&format!("//- /{name} crate:bin:{krate} edition:2015{attrs}\n{source}"))
}

#[test]
fn an_attribute_not_closed_where_it_ends_expects_its_bracket() {
    // tests/ui/attributes/z-crate-attr/multiple.rs
    let source = "//@ compile-flags: -Zcrate-attr=feature(foo),feature(bar)\n//~? ERROR expected `]`\nfn main() {}\n";
    let stderr = r#"error: expected `]`, found `,`
  --> <crate attribute>:1:16
   |
LL | #![feature(foo),feature(bar)]
   |                ^ expected `]`

error: aborting due to 1 previous error

"#;
    assert_eq!(ui_attrs("multiple.rs", "multiple", &["feature(foo),feature(bar)"], source), stderr);
}

#[test]
fn what_follows_a_whole_attribute_is_unexpected_and_the_crate_is_checked_on() {
    // tests/ui/attributes/z-crate-attr/injection2.rs
    let source = "//@ compile-flags: -Zcrate-attr=feature(yeet_expr)]#![allow(warnings)\n//~? ERROR unexpected token\nfn foo() {} //~ ERROR `main` function not found\n";
    let stderr = r#"error: unexpected token: `#`
  --> <crate attribute>:1:23
   |
LL | #![feature(yeet_expr)]#![allow(warnings)]
   |                       ^ unexpected token after this

error[E0601]: `main` function not found in crate `injection2`
  --> $DIR/injection2.rs:3:12
   |
LL | fn foo() {}
   |            ^ consider adding a `main` function to `$DIR/injection2.rs`

error: aborting due to 2 previous errors

For more information about this error, try `rustc --explain E0601`.
"#;
    assert_eq!(
        ui_attrs("injection2.rs", "injection2", &["feature(yeet_expr)]#![allow(warnings)"], source),
        stderr
    );
}

#[test]
fn an_open_paren_mismatches_the_closing_bracket_around_the_attribute() {
    // tests/ui/attributes/z-crate-attr/unbalanced-paren.rs
    let source = "// Show diagnostics for unbalanced parens.\n//@ compile-flags: -Zcrate-attr=(\n//~? ERROR mismatched closing delimiter\nfn main() {}\n";
    let stderr = r#"error: mismatched closing delimiter: `]`
  --> <crate attribute>:1:4
   |
LL | #![(]
   |   -^^ mismatched closing delimiter
   |   ||
   |   |unclosed delimiter
   |   closing delimiter possibly meant for this

error: aborting due to 1 previous error

"#;
    assert_eq!(ui_attrs("unbalanced-paren.rs", "unbalanced_paren", &["("], source), stderr);
}

/// `multiple.rs` and `unbalanced-paren.rs` given together: both files are named
/// `<crate attribute>`, each error is in the order given and shows its own attribute.
#[test]
fn attributes_shown_by_one_name_each_show_their_own_text() {
    let source = "fn main() {}\n";
    let stderr = r#"error: mismatched closing delimiter: `]`
  --> <crate attribute>:1:4
   |
LL | #![(]
   |   -^^ mismatched closing delimiter
   |   ||
   |   |unclosed delimiter
   |   closing delimiter possibly meant for this

error: expected `]`, found `,`
  --> <crate attribute>:1:16
   |
LL | #![feature(foo),feature(bar)]
   |                ^ expected `]`

error: aborting due to 2 previous errors

"#;
    assert_eq!(ui_attrs("both.rs", "both", &["(", "feature(foo),feature(bar)"], source), stderr);
}

/// An attribute rustc applies says nothing.
#[test]
fn an_attribute_applied_says_nothing() {
    let source = "fn main() {}\n";
    assert_eq!(ui_attrs("applied.rs", "applied", &["allow(unused)"], source), "");
}
