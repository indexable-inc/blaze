//! The suggestion lints over a small `std`, each alone on the command line: where each fires,
//! where it does not, and `could_be_sans_io`'s fix.

use base_db::FileId;
use hir_expand::fixture;
use syntax::{TextRange, TextSize};

use crate::{
    check::{Setup, check},
    conf::Conf,
    level::CommandLine,
    tests::{lint, main_crate},
};

const MINICORE: &str = "//- minicore: sized, copy, clone, option, result, fn, drop, default, iterator, slice, index, range, deref, eq, ord, fmt, panic, assert, builtin_impls, str\n";

/// A `std` with core's prelude (without it `Drop` and the rest of the prelude resolve to
/// nothing in `main`) and the one ambient effect the fixtures reach: the wall clock.
const STD: &str = r#"
//- /std/lib.rs crate:std
pub mod prelude {
    pub mod rust_2015 { pub use core::prelude::rust_2015::*; }
    pub mod rust_2018 { pub use core::prelude::rust_2018::*; }
    pub mod rust_2021 { pub use core::prelude::rust_2021::*; }
    pub mod rust_2024 { pub use core::prelude::rust_2024::*; }
}
pub mod time {
    pub struct Instant { at: u64 }
    impl Instant {
        pub fn now() -> Instant { Instant { at: 0 } }
    }
}
"#;

/// The header of the crate linted when a fixture does not start with its own.
const MAIN: &str = "//- /src/lib.rs crate:main deps:std cfg:test\n";

fn fixture_of(main: &str) -> String {
    let main = main.trim_start_matches('\n');
    let header = if main.starts_with("//- ") { "" } else { MAIN };
    format!("{MINICORE}{}{header}{main}", STD.trim_start_matches('\n'))
}

fn flags(lint_name: &str) -> String {
    format!("-A clippy::all --warn=blaze::{lint_name}")
}

fn found(lint_name: &str, main: &str) -> String {
    lint(&fixture_of(main), &flags(lint_name))
}

/// The last file of `main` with the first suggestion of each finding applied, after checking
/// that one was offered and that the file it makes is offered nothing more.
fn offered(lint_name: &str, main: &str) -> String {
    let (before, parts) = suggested(lint_name, &fixture_of(main));
    assert!(!parts.is_empty(), "`{lint_name}` offers a fix of:\n{before}");
    let mut after = before.clone();
    for (range, text) in parts.iter().rev() {
        after.replace_range(std::ops::Range::<usize>::from(*range), text);
    }
    assert_ne!(after, before, "the suggestions of `{lint_name}` changed the file");
    let fixture = fixture_of(main);
    let at = fixture.rfind(&before).expect("the last file's text is the fixture's end");
    let (_, more) = suggested(lint_name, &format!("{}{after}", &fixture[..at]));
    assert!(more.is_empty(), "the file the suggestion made has nothing left to offer:\n{after}");
    after
}

fn suggested(lint_name: &str, fixture: &str) -> (String, Vec<(TextRange, String)>) {
    let fx = fixture::db(fixture);
    let file = *fx.files().last().expect("the last file");
    let db = fx.into_db();
    let krate = blaze_query::db::attach(&db, || main_crate(&db));
    let file = file.file_id(&db);
    // `check` names every module file of the crate; only the last file's suggestions are kept.
    let last = "src/last.rs";
    let name_of = |it: FileId| Some(if it == file { last.to_owned() } else { format!("src/other{it:?}.rs") });
    let flags: Vec<String> = flags(lint_name).split_whitespace().map(str::to_owned).collect();
    let setup = Setup { command_line: CommandLine::parse(&flags).expect("the flags read"), conf: Conf::default() };
    let text = db.file_text(file).text(&db).to_string();
    let found = blaze_query::db::attach(&db, || check(&db, krate, &setup, &name_of).expect("the file is named"));
    let mut parts: Vec<(TextRange, String)> = found
        .iter()
        .filter_map(|it| it.diagnostic.suggestions.first()?.substitutions.first())
        .flatten()
        .filter(|(span, _)| span.file == last)
        .map(|(span, text)| (TextRange::new(TextSize::new(span.lo), TextSize::new(span.hi)), text.clone()))
        .collect();
    parts.sort_by_key(|(range, _)| (range.start(), range.end()));
    parts.dedup();
    (text, parts)
}

#[test]
fn could_be_narrower_finds_a_result_whose_literals_fit_fewer_bits() {
    let out = found(
        "could_be_narrower",
        r#"
fn level(x: u8) -> u64 {
    match x {
        0 => 1,
        1 => 20,
        _ => 300,
    }
}
fn sign(x: i32) -> i64 {
    if x < 0 {
        return -1;
    }
    if x == 0 { 0 } else { 1 }
}
pub fn uses() -> u64 { level(1) + sign(2) as u64 }
"#,
    );
    assert!(out.contains("`fn level` returns `u64`, but every value it returns is in 1..=300: `NonZero<u16>`"), "{out}");
    assert!(out.contains("`fn sign` returns `i64`, but every value it returns is in -1..=1: `i8`"), "{out}");
}

#[test]
fn could_be_narrower_leaves_one_value_a_computed_value_an_api_and_an_index() {
    let out = found(
        "could_be_narrower",
        r#"
fn one() -> u64 { 7 }
fn computed(x: u64) -> u64 { if x > 3 { x } else { 0 } }
pub fn exported(x: u8) -> u64 { if x > 3 { 1 } else { 2 } }
fn index(x: u8) -> usize { if x > 3 { 70000 } else { 2 } }
fn already(x: u8) -> u8 { if x > 3 { 1 } else { 2 } }
pub fn uses() -> u64 { one() + computed(1) + index(1) as u64 + already(1) as u64 }
"#,
    );
    assert!(!out.contains("could_be_narrower"), "{out}");
}

#[test]
fn could_be_sans_io_finds_the_highest_module_that_reaches_no_effect_and_marks_it() {
    let main = r#"
//- /src/lib.rs crate:main deps:std cfg:test
mod driver;
mod machine;
//- /src/driver.rs
pub fn run() -> u64 { std::time::Instant::now(); super::machine::step(1) }
//- /src/machine.rs
//! The state machine.
mod inner {
    pub fn double(x: u64) -> u64 { x * 2 }
}
pub fn step(x: u64) -> u64 { inner::double(x) + 1 }
"#;
    let out = found("could_be_sans_io", main);
    assert!(out.contains("machine` reaches no ambient effect in its 2 functions"), "{out}");
    assert!(!out.contains("driver`"), "{out}");
    assert!(!out.contains("inner`"), "{out}");
    let after = offered("could_be_sans_io", main);
    assert!(after.starts_with("//! The state machine.\n#![blaze::sans_io]\n"), "{after}");
}

#[test]
fn could_be_sans_io_leaves_a_module_already_in_a_core_and_one_without_functions() {
    let out = found(
        "could_be_sans_io",
        r#"
//- /src/lib.rs crate:main deps:std cfg:test
mod driver;
mod marked;
mod types;
//- /src/driver.rs
pub fn run() { std::time::Instant::now(); }
//- /src/marked.rs
#![blaze::sans_io]
pub fn step(x: u64) -> u64 { x + 1 }
//- /src/types.rs
pub struct Pair { pub a: u64, pub b: u64 }
"#,
    );
    assert!(!out.contains("could_be_sans_io"), "{out}");
}

#[test]
fn could_be_linear_finds_a_drop_that_does_the_finishing_methods_work() {
    let out = found(
        "could_be_linear",
        r#"
pub struct Writer { pending: u32 }
impl Writer {
    fn flush_pending(&mut self) { self.pending = 0; }
    pub fn finish(mut self) { self.flush_pending(); }
}
impl Drop for Writer {
    fn drop(&mut self) { self.flush_pending(); }
}
"#,
    );
    assert!(out.contains("`Drop::drop` of `Writer` calls `flush_pending`, as `Writer::finish` does"), "{out}");
    assert!(out.contains("#[blaze::linear]"), "{out}");
}

#[test]
fn could_be_linear_leaves_a_drop_that_only_frees_and_a_type_with_no_finishing_method() {
    let out = found(
        "could_be_linear",
        r#"
pub struct Plain { n: u32 }
impl Plain {
    pub fn finish(self) -> u32 { self.n }
}
impl Drop for Plain {
    fn drop(&mut self) {}
}
pub struct Unfinished { n: u32 }
impl Unfinished {
    fn reset(&mut self) { self.n = 0; }
    pub fn get(&self) -> u32 { self.n }
}
impl Drop for Unfinished {
    fn drop(&mut self) { self.reset(); }
}
"#,
    );
    assert!(!out.contains("could_be_linear"), "{out}");
}

#[test]
fn could_have_invariant_finds_a_condition_every_constructor_asserts() {
    let out = found(
        "could_have_invariant",
        r#"
pub struct Span { lo: u32, hi: u32 }
impl Span {
    pub fn new(lo: u32, hi: u32) -> Span {
        assert!(lo <= hi);
        Span { lo, hi }
    }
    pub fn set(&mut self, lo: u32, hi: u32) {
        assert!(lo<=hi, "a span ends after it starts");
        self.lo = lo;
        self.hi = hi;
    }
    pub fn len(&self) -> u32 { self.hi - self.lo }
}
"#,
    );
    assert!(out.contains("`new`, `set` each start with `assert!(lo <= hi)`"), "{out}");
    assert!(out.contains("#[blaze::invariant(..)]"), "{out}");
}

#[test]
fn could_have_invariant_leaves_a_public_field_and_a_condition_asserted_once() {
    let out = found(
        "could_have_invariant",
        r#"
pub struct Open { pub lo: u32, hi: u32 }
impl Open {
    pub fn new(lo: u32, hi: u32) -> Open { assert!(lo <= hi); Open { lo, hi } }
    pub fn set(&mut self, lo: u32, hi: u32) { assert!(lo <= hi); self.lo = lo; self.hi = hi; }
}
pub struct Once { lo: u32, hi: u32 }
impl Once {
    pub fn new(lo: u32, hi: u32) -> Once { assert!(lo <= hi); Once { lo, hi } }
    pub fn set(&mut self, lo: u32) { assert!(lo > 0); self.lo = lo; }
}
"#,
    );
    assert!(!out.contains("could_have_invariant"), "{out}");
}
