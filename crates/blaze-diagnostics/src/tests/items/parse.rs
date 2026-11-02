//! rustc's UI tests of syntax errors (`tests/ui/lexer/`, `tests/ui/parser/`): what rustc's lexer
//! stops at, the delimiters of its token trees, and the parse errors it gives up or goes on at.

use crate::tests::ui;

#[test]
fn an_unterminated_block_comment_stops_the_compilation() {
    // tests/ui/lexer/unterminated-comment.rs
    let source = "/* //~ ERROR E0758\n";
    let stderr = r#"error[E0758]: unterminated block comment
  --> $DIR/unterminated-comment.rs:1:1
   |
LL | /*
   | ^^^^^^^^^^^^^^^^^^

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0758`.
"#;
    assert_eq!(ui("unterminated-comment.rs", "2015", source), stderr);
}

#[test]
fn an_unterminated_nested_comment_points_at_the_last_nested_one() {
    // tests/ui/lexer/unterminated-nested-comment.rs
    let source = "/* //~ ERROR E0758\n/* */\n/*\n*/\n";
    let stderr = r#"error[E0758]: unterminated block comment
  --> $DIR/unterminated-nested-comment.rs:1:1
   |
LL |   /*
   |   ^-
   |   |
   |  _unterminated block comment
   | |
LL | | /* */
LL | | /*
   | | --
   | | |
   | | ...as last nested comment starts here, maybe you want to close this instead?
LL | | */
   | |_--^
   |   |
   |   ...and last nested comment terminates here.

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0758`.
"#;
    assert_eq!(ui("unterminated-nested-comment.rs", "2015", source), stderr);
}

#[test]
fn an_unterminated_raw_string_says_how_it_ends() {
    // tests/ui/parser/raw/raw-str-unterminated.rs
    let source = r####"static s: &'static str =
    r#" string literal goes on
        and on
    //~^^ ERROR unterminated raw string
"####;
    let stderr = r####"error[E0748]: unterminated raw string
  --> $DIR/raw-str-unterminated.rs:2:5
   |
LL |     r#" string literal goes on
   |     ^ unterminated raw string
   |
   = note: this raw string should be terminated with `"#`

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0748`.
"####;
    assert_eq!(ui("raw-str-unterminated.rs", "2015", source), stderr);
}

#[test]
fn an_unterminated_raw_string_suggests_its_likeliest_end() {
    // tests/ui/parser/raw/raw-string-2.rs
    let source = r####"fn main() {
    let x = r###"here's a long string"# "# "##;
    //~^ ERROR unterminated raw string
}
"####;
    let stderr = r####"error[E0748]: unterminated raw string
  --> $DIR/raw-string-2.rs:2:13
   |
LL |     let x = r###"here's a long string"# "# "##;
   |             ^ unterminated raw string
   |
   = note: this raw string should be terminated with `"###`
help: consider terminating the string here
   |
LL |     let x = r###"here's a long string"# "# "###;
   |                                               +

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0748`.
"####;
    assert_eq!(ui("raw-string-2.rs", "2015", source), stderr);
}

#[test]
fn a_mismatched_closing_delimiter_names_what_it_left_open() {
    // tests/ui/parser/issues/issue-10636-1.rs
    let source = "struct Obj {
    //~^ NOTE: unclosed delimiter
    member: usize
)
//~^ ERROR mismatched closing delimiter
//~| NOTE mismatched closing delimiter

fn main() {}
";
    let stderr = "error: mismatched closing delimiter: `)`
  --> $DIR/issue-10636-1.rs:1:12
   |
LL | struct Obj {
   |            ^ unclosed delimiter
...
LL | )
   | ^ mismatched closing delimiter

error: aborting due to 1 previous error

";
    assert_eq!(ui("issue-10636-1.rs", "2015", source), stderr);
}

#[test]
fn an_unclosed_delimiter_points_at_the_block_indented_otherwise() {
    // tests/ui/parser/issues/issue-2354.rs
    let source = "fn foo() { //~ NOTE unclosed delimiter
  match Some(10) {
  //~^ NOTE this delimiter might not be properly closed...
      Some(y) => { panic!(); }
      None => { panic!(); }
}
//~^ NOTE ...as it matches this but it has different indentation

fn bar() {
    let mut i = 0;
    while (i < 1000) {}
}

fn main() {}
//~ ERROR this file contains an unclosed delimiter
";
    let stderr = "error: this file contains an unclosed delimiter
  --> $DIR/issue-2354.rs:15:52
   |
LL | fn foo() {
   |          - unclosed delimiter
LL |   match Some(10) {
   |                  - this delimiter might not be properly closed...
...
LL | }
   | - ...as it matches this but it has different indentation
...
LL |
   |                                                   ^

error: aborting due to 1 previous error

";
    assert_eq!(ui("issue-2354.rs", "2015", source), stderr);
}

#[test]
fn an_unclosed_delimiter_with_blocks_indented_alike_names_only_itself() {
    // tests/ui/parser/mismatched-braces/missing-close-brace-in-struct.rs
    let source = "pub(crate) struct Bar<T> {
  foo: T,

trait T {
    fn foo(&self);
}


impl T for Bar<usize> {
fn foo(&self) {}
}

fn main() {} //~ ERROR this file contains an unclosed delimiter
";
    let stderr = "error: this file contains an unclosed delimiter
  --> $DIR/missing-close-brace-in-struct.rs:13:65
   |
LL | pub(crate) struct Bar<T> {
   |                          - unclosed delimiter
...
LL | fn main() {}
   |                                                                ^

error: aborting due to 1 previous error

";
    assert_eq!(ui("missing-close-brace-in-struct.rs", "2015", source), stderr);
}

#[test]
fn a_missing_semicolon_is_added_and_parsing_goes_on() {
    // tests/ui/parser/issues/issue-3036.rs
    let source = "//@ run-rustfix

// Testing that semicolon tokens are printed correctly in errors

fn main() {
    let _x = 3 //~ ERROR: expected `;`
}
";
    let stderr = "error: expected `;`, found `}`
  --> $DIR/issue-3036.rs:6:15
   |
LL |     let _x = 3
   |               ^ help: add `;` here
LL | }
   | - unexpected token

error: aborting due to 1 previous error

";
    assert_eq!(ui("issue-3036.rs", "2015", source), stderr);
}

#[test]
fn a_global_let_mut_is_not_an_item() {
    // tests/ui/parser/suggest-static-for-global-var-mut.rs
    let source = "let mut _data = vec![1,2,3];
//~^ ERROR expected item, found keyword `let`

fn main() {
}
";
    let stderr = "error: expected item, found keyword `let`
  --> $DIR/suggest-static-for-global-var-mut.rs:1:1
   |
LL | let mut _data = vec![1,2,3];
   | ^^^ `let` cannot be used for global variables
   |
   = help: consider using `static` and a `Mutex` instead of `let mut`
   = note: for a full list of items that can appear in modules, see <https://doc.rust-lang.org/reference/items.html>

error: aborting due to 1 previous error

";
    assert_eq!(ui("suggest-static-for-global-var-mut.rs", "2015", source), stderr);
}

#[test]
fn a_global_let_suggests_static_or_const() {
    // tests/ui/parser/suggest-const-for-global-var.rs
    let source = r#"let X: i32 = 12;
//~^ ERROR expected item, found keyword `let`

fn main() {
    println!("{}", X);
}
"#;
    let stderr = "error: expected item, found keyword `let`
  --> $DIR/suggest-const-for-global-var.rs:1:1
   |
LL | let X: i32 = 12;
   | ^^^
   | |
   | `let` cannot be used for global variables
   | help: consider using `static` or `const` instead of `let`
   |
   = note: for a full list of items that can appear in modules, see <https://doc.rust-lang.org/reference/items.html>

error: aborting due to 1 previous error

";
    assert_eq!(ui("suggest-const-for-global-var.rs", "2015", source), stderr);
}

#[test]
fn a_semicolon_after_a_struct_is_removed() {
    // tests/ui/parser/issues/issue-46186.rs
    let source = "//@ run-rustfix

pub struct Struct {
    pub a: usize,
};
//~^ ERROR expected item, found `;`

fn main() {}
";
    let stderr = "error: expected item, found `;`
  --> $DIR/issue-46186.rs:5:2
   |
LL | };
   |  ^
   |
   = help: braced struct declarations are not followed by a semicolon
help: remove this semicolon
   |
LL - };
LL + }
   |

error: aborting due to 1 previous error

";
    assert_eq!(ui("issue-46186.rs", "2015", source), stderr);
}

#[test]
fn a_literal_is_not_an_item() {
    // tests/ui/parser/issues/issue-113110-non-item-at-module-root.rs
    let source = " 5 //~ ERROR expected item, found `5`\n";
    let stderr = "error: expected item, found `5`
  --> $DIR/issue-113110-non-item-at-module-root.rs:1:2
   |
LL |  5
   |  ^ expected item
   |
   = note: for a full list of items that can appear in modules, see <https://doc.rust-lang.org/reference/items.html>

error: aborting due to 1 previous error

";
    assert_eq!(ui("issue-113110-non-item-at-module-root.rs", "2015", source), stderr);
}

#[test]
fn a_reserved_keyword_where_an_item_is_expected_is_named_so() {
    // tests/ui/parser/virtual-structs.rs
    let source = "// Test diagnostics for the removed struct inheritance feature.

virtual struct SuperStruct {
//~^ ERROR expected item, found reserved keyword `virtual`
    f1: isize,
}

struct Struct : SuperStruct;

pub fn main() {}
";
    let stderr = "error: expected item, found reserved keyword `virtual`
  --> $DIR/virtual-structs.rs:3:1
   |
LL | virtual struct SuperStruct {
   | ^^^^^^^ expected item
   |
   = note: for a full list of items that can appear in modules, see <https://doc.rust-lang.org/reference/items.html>

error: aborting due to 1 previous error

";
    assert_eq!(ui("virtual-structs.rs", "2015", source), stderr);
}
