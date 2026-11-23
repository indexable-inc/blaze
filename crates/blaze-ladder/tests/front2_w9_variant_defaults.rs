//! A variant's path written short of its enum's defaulted parameters takes their defaults
//! (the Reference, "Paths in expressions"; "Generic parameters", defaults), as the enum's own
//! segment does: alloy's `enum FilterChanges<T = Transaction, L = Log>` is written
//! `FilterChanges::<Transaction<E>>::Logs(logs)` in tempo's zones rpc. A parameter with no
//! default still has to be written: E0107.

use blaze_ladder::crates::{Inputs, Toolchain};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::program_texts::ProgramTexts;
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::NewFront;

const PRELUDE: &str = include_str!("../../blaze-cli/tests/ladder/front2/x0-clo-04-generic-fn/src/prelude.rs");
const HEADER: &str = r#"#![feature(no_core, lang_items, intrinsics, macro_derive, rustc_attrs, unboxed_closures)]
#![no_core]
#![no_main]
mod prelude;
use prelude::*;
"#;

fn check(source: &str, cases: &[(&str, &[&str])]) {
    let source = [HEADER, source].concat();
    let files = [
        SourceFile { path: "src/main.rs", text: &source },
        SourceFile { path: "src/prelude.rs", text: PRELUDE },
    ];
    let alloc = blaze_own::testing::heap().alloc();
    let tc = Toolchain::none(alloc);
    let new = NewFront { package: "variant_defaults", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::Off };
    let texts = ProgramTexts::new(&files, "src/main.rs", new, &tc, blaze_build::Panic::Unwind).expect("fixture texts");
    let user = texts.user();
    let inputs = Inputs::new(&user, &tc).expect("fixture crates");
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("fixture names");
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("fixture frame");
    let mut world = ProjectWorld::new(&names, &frame, &files, alloc).expect("front2");
    let bodies = world.bodies(alloc);
    for &(name, expected) in cases {
        let body = bodies.iter().find_map(|&(body, _)| match body {
            BodyRef::Free(key) if names.item_text(key)?.name == Some(name) => Some(body),
            _ => None,
        }).unwrap_or_else(|| panic!("missing fixture function `{name}`"));
        let verdict = world.check(body, alloc);
        assert!(verdict.reads.is_some() && verdict.gaps.is_empty() && verdict.refused.is_none() && verdict.undecided.is_empty(), "{name}: {verdict:?}");
        let codes: Vec<_> = verdict.diagnostics.iter().map(|d| d.code).collect();
        assert_eq!(codes.as_slice(), expected, "{name}: {verdict:?}");
    }
}

#[test]
fn a_variant_path_takes_its_enums_defaults() {
    const SOURCE: &str = r#"
struct Tx;
struct Log;
enum Changes<T = Tx, L = Log> { Empty, Logs(L), Txs(T) }
fn logs() -> Changes<u8> { Changes::<u8>::Logs(Log) }
fn empty() -> Changes<u8> { Changes::<u8>::Empty }
fn on_variant() -> Changes<u8> { Changes::Txs::<u8>(1) }
fn all_written() -> Changes<u8, u16> { Changes::<u8, u16>::Logs(2) }
enum Pair<T, U> { Left(T), Right(U) }
// Control: `U` has no default, so the enum's segment written with one argument is E0107.
fn short() -> Pair<u8, u8> { Pair::<u8>::Left(1) }
"#;
    check(SOURCE, &[("logs", &[]), ("empty", &[]), ("on_variant", &[]), ("all_written", &[]), ("short", &["E0107"])]);
    println!("logs: clean, empty: clean, on_variant: clean, all_written: clean, short: E0107");
}
