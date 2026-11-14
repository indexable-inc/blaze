//! The correct-by-construction lints: per lint, a case where it fires and a near miss where it
//! does not; for `proof_forgotten`, its fix made and the made file linted clean.

use crate::tests::{lint, lint_both_builds, offered_in};

const MINICORE: &str = "//- minicore: sized, copy, clone, drop, option, result, fn, panic, index, slice, builtin_impls, allocator_api, range, iterator, non_zero, reach\n";

/// `alloc` with a `Vec` whose index checks its length and panics, as the real one does, and a
/// `std` that re-exports it.
const TOOLCHAIN: &str = r#"
//- /alloc.rs crate:alloc
pub mod vec {
    pub struct Vec<T> {
        ptr: *mut T,
        len: usize,
    }
    impl<T> Vec<T> {
        pub fn len(&self) -> usize {
            self.len
        }
        pub fn push(&mut self, _value: T) {
            self.len = self.len + 1;
        }
    }
    impl<T> core::ops::Index<usize> for Vec<T> {
        type Output = T;
        fn index(&self, index: usize) -> &T {
            if index >= self.len {
                core::panicking::panic("index out of bounds");
            }
            unsafe { &*self.ptr }
        }
    }
}
//- /std.rs crate:std deps:alloc
pub mod prelude {
    pub mod rust_2015 { pub use core::prelude::rust_2015::*; }
    pub mod rust_2018 { pub use core::prelude::rust_2018::*; }
    pub mod rust_2021 { pub use core::prelude::rust_2021::*; }
    pub mod rust_2024 { pub use core::prelude::rust_2024::*; }
}
pub mod vec { pub use alloc::vec::Vec; }
"#;

fn header() -> String {
    format!("{MINICORE}{TOOLCHAIN}//- /src/lib.rs crate:main deps:alloc,std\n")
}

fn flags(name: &str) -> String {
    format!("-A clippy::all --warn=blaze::{name}")
}

/// The output of linting `main` with only `blaze::name` on.
fn linted(name: &str, main: &str) -> String {
    lint(&format!("{}{}", header(), main.trim_start_matches('\n')), &flags(name))
}

/// The lint fires on each of `fires`' findings (`count`) and on none of `near`'s.
fn fires_and_not(name: &str, said: &str, fires: &str, count: usize, near: &str) {
    let out = linted(name, fires);
    assert_eq!(out.matches(said).count(), count, "{out}");
    assert!(out.contains("carry it in a type:"), "{out}");
    let out = linted(name, near);
    assert_eq!(out.matches(said).count(), 0, "{out}");
}

#[test]
fn proof_forgotten_finds_an_unwrap_its_check_proved() {
    fires_and_not(
        "proof_forgotten",
        "blaze proves never happens",
        r#"
pub fn first(x: Option<u32>) -> u32 {
    if x.is_some() { x.unwrap() + 1 } else { 0 }
}
pub fn early(x: Option<u32>) -> u32 {
    if x.is_none() {
        return 0;
    }
    x.unwrap()
}
pub fn at(v: &[u32], i: usize) -> u32 {
    if i < v.len() { v[i] } else { 0 }
}
"#,
        3,
        r#"
pub fn unchecked(x: Option<u32>, y: Option<u32>) -> u32 {
    if y.is_some() { x.unwrap() } else { 0 }
}
pub fn known() -> u32 {
    Some(3).unwrap()
}
"#,
    );
}

#[test]
fn proof_forgotten_binds_what_the_check_proved() {
    let after = offered_in(
        &header(),
        "pub fn first(x: Option<u32>) -> u32 {\n    if x.is_some() { x.unwrap() + 1 } else { 0 }\n}\n",
        &flags("proof_forgotten"),
    );
    assert!(after.contains("if let Some(x) = x { x + 1 }"), "{after}");
}

#[test]
fn unit_mismatch_finds_two_units_joined() {
    fires_and_not(
        "unit_mismatch",
        "as if they were one unit",
        r#"
pub fn late(elapsed_ms: u64, limit_secs: u64) -> bool {
    elapsed_ms > limit_secs
}
pub fn total(head_bytes: u64, body_kib: u64) -> u64 {
    head_bytes + body_kib
}
"#,
        2,
        r#"
pub fn late(elapsed_ms: u64, limit_ms: u64, count: u64) -> bool {
    elapsed_ms > limit_ms && count > elapsed_ms
}
"#,
    );
}

#[test]
fn primitive_meaning_finds_a_unit_or_an_id_in_a_name() {
    fires_and_not(
        "primitive_meaning",
        "that only the name says",
        r#"
pub fn wait(timeout_ms: u64) -> u64 { timeout_ms }
pub struct Edge { pub node_id: u32 }
"#,
        2,
        r#"
pub fn wait(count: u64) -> u64 { count }
pub struct Edge { pub weight: u32, pub id_text: u32 }
"#,
    );
}

#[test]
fn sentinel_value_finds_a_value_standing_in_for_none() {
    fires_and_not(
        "sentinel_value",
        "standing in for \"none\"",
        r#"
pub fn found(pos: i64) -> bool { pos != -1 }
pub fn is_root(node_id: u32) -> bool { node_id == 0 }
"#,
        2,
        r#"
pub fn none(count: u32) -> bool { count == 0 }
pub fn small(pos: i64) -> bool { pos < 1 }
"#,
    );
}

#[test]
fn state_in_flags_finds_states_kept_as_bools() {
    fires_and_not(
        "state_in_flags",
        "keeps its state in",
        "pub struct Conn { started: bool, closed: bool, port: u16 }\n",
        1,
        "pub struct Opts { verbose: bool, color: bool }\npub struct One { closed: bool }\n",
    );
}

#[test]
fn bool_pair_state_finds_unnamed_states() {
    fires_and_not(
        "bool_pair_state",
        "states with no names",
        r#"
pub fn pick(a: (bool, bool)) -> u32 { if a.0 { 1 } else { 0 } }
pub struct Answer { pub ok: Option<bool> }
pub fn both(a: bool, b: bool) -> u32 {
    match (a, b) {
        (true, true) => 3,
        (true, false) => 2,
        (false, true) => 1,
        (false, false) => 0,
    }
}
"#,
        3,
        r#"
pub fn pick(a: bool, n: (bool, u32)) -> u32 { if a && n.0 { 1 } else { n.1 } }
pub struct Answer { pub ok: Option<u32> }
"#,
    );
}

#[test]
fn out_parameter_finds_a_second_return_value() {
    fires_and_not(
        "out_parameter",
        "is only written through",
        r#"
pub fn parse(s: u32, out: &mut u32) -> bool {
    if s == 0 {
        return false;
    }
    *out = s;
    true
}
"#,
        1,
        r#"
pub fn bump(n: &mut u32) {
    *n = *n + 1;
}
pub fn parse(s: u32, out: &mut u32) -> u32 {
    *out = s;
    s
}
"#,
    );
}

#[test]
fn parallel_collections_finds_two_vecs_kept_in_step() {
    fires_and_not(
        "parallel_collections",
        "kept the same length by convention",
        r#"
use std::vec::Vec;
pub struct People { names: Vec<u32>, ages: Vec<u32> }
impl People {
    pub fn add(&mut self, n: u32, a: u32) {
        self.names.push(n);
        self.ages.push(a);
    }
}
pub struct Table { keys: Vec<u32>, values: Vec<u32> }
impl Table {
    pub fn get(&self, i: usize) -> u32 {
        self.keys[i] + self.values[i]
    }
}
"#,
        2,
        r#"
use std::vec::Vec;
pub struct People { names: Vec<u32>, ages: Vec<u32> }
impl People {
    pub fn add(&mut self, n: u32) {
        self.names.push(n);
    }
    pub fn get(&self, i: usize, j: usize) -> u32 {
        self.names[i] + self.ages[j]
    }
}
"#,
    );
}

/// The invariant: a library's `--test` build judges its production code as its plain build
/// does, so an `#[expect]` on a function that silences a finding on a statement inside it is
/// fulfilled in both builds, and one on a function the lint does not fire in is unfulfilled in
/// both. The `--test` build once took all of a library for test code and ran no body lint of
/// this module, and said the expectation its plain build had met unfulfilled.
#[test]
fn an_expected_parallel_collections_is_fulfilled_in_the_test_build_too() {
    let flags = flags("parallel_collections");
    let built = |main: &str| lint_both_builds(&format!("{}{}", header(), main.trim_start_matches('\n')), &flags);
    let fires = r#"
use std::vec::Vec;
pub struct People { names: Vec<u32>, ages: Vec<u32> }
impl People {
    pub fn add(&mut self, n: u32, a: u32) {
        self.names.push(n);
        self.ages.push(a);
    }
}
"#;
    let expected = r#"
use std::vec::Vec;
pub struct People { names: Vec<u32>, ages: Vec<u32> }
impl People {
    #[expect(
        blaze::parallel_collections,
        reason = "the two are read apart"
    )]
    pub fn add(&mut self, n: u32, a: u32) {
        self.names.push(n);
        self.ages.push(a);
    }
}
"#;
    let missed = r#"
use std::vec::Vec;
pub struct People { names: Vec<u32>, ages: Vec<u32> }
impl People {
    #[expect(
        blaze::parallel_collections,
        reason = "the two are read apart"
    )]
    pub fn add(&mut self, n: u32) {
        self.names.push(n);
    }
}
"#;
    for out in built(fires) {
        assert_eq!(out.matches("kept the same length by convention").count(), 1, "{out}");
    }
    for out in built(expected) {
        assert!(!out.contains("kept the same length by convention"), "{out}");
        assert!(!out.contains("lint expectation is unfulfilled"), "the expectation is met:\n{out}");
    }
    for out in built(missed) {
        assert_eq!(out.matches("lint expectation is unfulfilled").count(), 1, "{out}");
    }
}

#[test]
fn untyped_index_finds_an_integer_used_as_one_collections_index() {
    fires_and_not(
        "untyped_index",
        "which collection it indexes is a convention",
        r#"
use std::vec::Vec;
pub struct Cursor { items: Vec<u32>, pub current: usize }
impl Cursor {
    pub fn get(&self) -> u32 { self.items[self.current] }
}
"#,
        1,
        r#"
use std::vec::Vec;
pub struct Counter { items: Vec<u32>, pub count: usize }
impl Counter {
    pub fn next(&self) -> usize { self.count + 1 }
}
"#,
    );
}

#[test]
fn validated_but_raw_finds_a_check_thrown_away() {
    fires_and_not(
        "validated_but_raw",
        "returned raw as",
        r#"
pub fn port(p: u16) -> Option<u16> {
    if p == 0 {
        return None;
    }
    Some(p)
}
"#,
        1,
        r#"
pub fn next_port(p: u16) -> Option<u16> {
    if p == 0 {
        return None;
    }
    Some(p + 1)
}
pub fn same(p: u16) -> u16 { p }
"#,
    );
}
