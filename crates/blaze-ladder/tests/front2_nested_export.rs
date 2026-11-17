//! An exported fn declared INSIDE a body is the process's entry (front2/nested_exports.rs,
//! program.rs `exported_entry` and `make_from`).
//!
//! The Reference, "Items": "Items may be declared in a block"; "The `no_mangle` attribute"
//! ([abi.no_mangle]): the symbol "will be the same as the item's name", wherever the item is
//! written. D104's `#[bstd::main]` expansion nests its exported `bstd_hosted_start` in the
//! process's `fn main` body; these programs are `#![no_core]` and need no toolchain, with the
//! C entry `main` in place of the hosted one (the same rule's second arm).

#![feature(allocator_api)]

use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{FrontChoice, NewFront, run_with};

const NEW: NewFront<'static> = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };

const HEAD: &str = "#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = \"sized\"]
trait Sized {}

#[lang = \"copy\"]
trait Copy {}

impl Copy for i32 {}

fn main(code: i32) -> i32 {
    code
}
";

/// The C entry two blocks deep: in the body of `level`, itself declared in the body of `outer`.
const DEEP: &str = "
fn outer() -> i32 {
    fn level() -> i32 {
        #[unsafe(no_mangle)]
        pub extern \"C\" fn main() -> i32 {
            crate::main(42)
        }
        0
    }
    level()
}
";

/// Two C entries, each in the body of its own fn.
const TWO: &str = "
fn first() -> i32 {
    #[unsafe(no_mangle)]
    pub extern \"C\" fn main() -> i32 {
        crate::main(1)
    }
    0
}

fn second() -> i32 {
    #[export_name = \"main\"]
    pub extern \"C\" fn start() -> i32 {
        crate::main(2)
    }
    0
}
";

/// The exported `main` declared in a block two levels down is the entry, keyed by its own
/// local item key, its enclosing bodies lowered first so its head is kept: the run ends with
/// its status, 42. Without the nested scan the root's `fn main(code)` is the entry and the run
/// is refused with "`main`: it takes parameters" (the line this test fails with).
///
/// Prints: `NESTED ENTRY: status 42` and
/// `test an_exported_fn_declared_in_a_body_is_the_entry ... ok`
#[test]
fn an_exported_fn_declared_in_a_body_is_the_entry() {
    let text = [HEAD, DEEP].concat();
    let files = [SourceFile { path: "src/main.rs", text: &text }];
    let status = run_with(&files, "src/main.rs", None, FrontChoice::New(NEW), blaze_own::testing::heap().shared()).unwrap_or_else(|e| panic!("NESTED ENTRY: refused: {e}"));
    println!("NESTED ENTRY: status {status}");
    assert_eq!(status, 42);
}

/// Two fns declared in blocks exported under one entry symbol (one by `no_mangle`, one by
/// `export_name = "main"`): the typed refusal `TwoEntries` naming both by their whole paths.
/// Without the nested scan neither is a candidate and the run is refused for the root's
/// `main` taking parameters instead.
///
/// Prints: `TWO NESTED: front2: `first::main` (declared in a block) and `second::start`
/// (declared in a block) are both exported as `main`, the process's entry` (the module
/// prefix, when the names library gives one, before each path) and
/// `test two_nested_exports_of_one_entry_are_refused_naming_both ... ok`
#[test]
fn two_nested_exports_of_one_entry_are_refused_naming_both() {
    let text = [HEAD, TWO].concat();
    let files = [SourceFile { path: "src/main.rs", text: &text }];
    let refused = match run_with(&files, "src/main.rs", None, FrontChoice::New(NEW), blaze_own::testing::heap().shared()) {
        Err(e) => format!("{e}"),
        Ok(status) => panic!("TWO NESTED: ran to status {status}; two exports of `main` must be refused"),
    };
    println!("TWO NESTED: {refused}");
    assert!(refused.contains("both exported as `main`"), "{refused}");
    assert!(refused.contains("first::main`"), "{refused}");
    assert!(refused.contains("second::start`"), "{refused}");
}
