//! A NON-survey run whose entry is bstd's hosted entry declared INSIDE a body (D104's
//! `bstd::main` expansion nests `#[unsafe(no_mangle)] pub extern "C" fn bstd_hosted_start(..)`
//! in the launcher `fn main() -> Exit`, crates/blaze-std/src/entry.rs) gets past run.rs
//! `entry_shape` as `Start::Hosted`.
//!
//! The rule (the Reference, [abi.no_mangle]): with `#[no_mangle]` "the symbol name for the
//! function ... will be the same as the item's name", the identifier as written, wherever the
//! item is declared ("Items": "Items may be declared in a block"). The tabled row's export is
//! read by program.rs `symbols_of`; before hello-entry-shape it took the symbol from the row's
//! SHOWN path, which for a fn declared in a block is no name (check.rs `item_shown` prints
//! `ItemKey(..)`), so the row's export was `ItemKey(..)`, not `bstd_hosted_start`, and the run
//! fell to the plain-main arm: hello runner 9's stop "`ItemKey(4952..)`: it takes parameters
//! (argc and argv are not passed yet)". A1's front2_main survey skips `entry_shape`, so no test
//! covered this.
//!
//! The program is `#![no_core]` (no toolchain): the run is given no host table, so a run past
//! `entry_shape` with `Start::Hosted` ends at `RunError::NoHost`, the one error that says the
//! entry was taken as the hosted one. CONTROL: with `symbols_of` reading the shown path again
//! (the old line), this test fails with "HOSTED IN A BODY: `ItemKey(..)`: it takes parameters
//! (argc and argv are not passed yet)".
//!
//! Prints: `HOSTED IN A BODY: `ItemKey(..)` is bstd's hosted entry, and this run was given no
//! host table to start it with` and
//! `test a_hosted_entry_declared_in_a_body_starts_hosted ... ok`

#![feature(allocator_api)]

use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{FrontChoice, NewFront, run_with};

const NEW: NewFront<'static> = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };

/// D104's shape without bstd: the root's `fn main` takes a parameter (as the owner's
/// `fn main(cx: &Cx, out: Out)` does), and the hosted entry, `extern "C" fn(*const _) -> i32`,
/// is declared in the body of another fn, as `bstd::main`'s launcher declares it.
const PROGRAM: &str = "#![feature(no_core, lang_items)]
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

fn launcher() -> i32 {
    #[unsafe(no_mangle)]
    pub extern \"C\" fn bstd_hosted_start(table: *const u8) -> i32 {
        crate::main(7)
    }
    0
}
";

#[test]
fn a_hosted_entry_declared_in_a_body_starts_hosted() {
    let files = [SourceFile { path: "src/main.rs", text: PROGRAM }];
    let said = match run_with(&files, "src/main.rs", None, FrontChoice::New(NEW), blaze_own::testing::heap().shared()) {
        Err(e) => format!("{e}"),
        Ok(status) => panic!("HOSTED IN A BODY: ran to status {status} with no host table; the hosted entry needs one"),
    };
    println!("HOSTED IN A BODY: {said}");
    assert!(!said.contains("it takes parameters"), "the hosted entry was taken for a plain `main`: {said}");
    assert!(said.contains("is bstd's hosted entry, and this run was given no host table"), "{said}");
}
