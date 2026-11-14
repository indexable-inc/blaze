//! The complexity lints over fixtures whose values are worked out by hand from each measure's
//! rules (cited at each test), each at its bound (no finding) and one under it (a finding);
//! the held values (a finding only above what an item is held at); and the property that
//! renaming locals and reordering items changes no value.

use std::{alloc::Global, path::PathBuf, sync::Arc};

use super::{Measure, fires, group_of, modules::strongly_connected, weight};
use crate::{
    baseline::items::{self, Entry, Items, Key, Loaded},
    blaze::tests::ALLOC_STD,
    conf::Conf,
    tests::lint_with,
};

/// The blaze lint tests' `core` with `drop`, for a type of the fixture that implements `Drop`.
const MINICORE: &str = "//- minicore: sized, clone, copy, derive, fn, default, from, deref, builtin_impls, dispatch_from_dyn, drop\n";
const MAIN: &str = "//- /src/lib.rs crate:main deps:std\n";

fn conf(text: &str) -> Conf {
    Conf::of_text(Some((PathBuf::from("/w/clippy.toml"), text.to_owned())), None)
        .expect("the configuration reads")
}

fn run(main: &str, lint: &str, conf_text: &str) -> String {
    let fixture = format!("{MINICORE}{ALLOC_STD}{MAIN}{}", main.trim_start_matches('\n'));
    lint_with(&fixture, &format!("-A clippy::all --warn=blaze::{lint}"), conf(conf_text))
}

/// `lint` as a diagnostic names it (`-W blaze::deep-nesting`): its words joined by `-`, as
/// the command line's flag is shown back, whatever spelling the flag was given in.
fn named(lint: &str) -> String {
    format!("blaze::{}", lint.replace('_', "-"))
}

/// Whether `lint` fires on `main` with `key` set to `under` (yes) and to `at` (no): the value
/// is `at`, exactly.
fn exactly(main: &str, lint: &str, key: &str, at: u64) -> String {
    let fired = run(main, lint, &format!("{key} = {}", at - 1));
    assert!(fired.contains(&named(lint)), "under the bound it fires:\n{fired}");
    let quiet = run(main, lint, &format!("{key} = {at}"));
    assert!(!quiet.contains(&named(lint)), "at the bound it does not:\n{quiet}");
    fired
}

/// The weight of the first function of `text`, by [`weight::weigh`] on its body.
fn weigh(text: &str) -> weight::Weight {
    use syntax::view::{self as ast, AstNode as _};
    let parse = syntax::SourceFile::parse(text, span::Edition::CURRENT);
    let f = parse.root().descendants().find_map(ast::Fn::cast).expect("a function");
    weight::weigh(f.body().expect("a body").syntax())
}

/// Campbell's own examples (2018, appendix C): `sumOfPrimes` weighs 7 (`for` +1, the inner
/// `for` +2, the `if` +3, the labelled `continue` +1) and nests 3 deep; `getWords`, one
/// `switch` of three cases, weighs 1.
#[test]
fn campbells_examples_weigh_what_the_paper_says() {
    let primes = weigh(
        r#"
fn sum_of_primes(max: u64) -> u64 {
    let mut total = 0;
    'outer: for i in 1..=max {
        for j in 2..i {
            if i % j == 0 {
                continue 'outer;
            }
        }
        total += i;
    }
    total
}
"#,
    );
    assert_eq!((primes.total, primes.depth), (7, 3));
    let words = weigh(
        r#"
fn get_words(number: u64) -> &'static str {
    match number {
        1 => "one",
        2 => "a couple",
        _ => "lots",
    }
}
"#,
    );
    assert_eq!((words.total, words.depth), (1, 1));
}

/// B1's boolean sequences: `a && b && c` is one, `a && b || c` two, `a && !(b && c)` two (the
/// `!` starts a new expression); an `else if` and an `else` add 1 each with no nesting (B3), a
/// closure nests what is in it (+1 for the `if` inside, at nesting 1: 2), a `let .. else` is an
/// `if`.
#[test]
fn sequences_chains_closures_and_let_else() {
    let w = |body: &str| weigh(&format!("fn f(a: bool, b: bool, c: bool, x: Option<u8>) {{ {body} }}")).total;
    assert_eq!(w("let _ = a && b && c;"), 1);
    assert_eq!(w("let _ = a && b || c;"), 2);
    assert_eq!(w("let _ = a && !(b && c);"), 2);
    assert_eq!(w("if a {} else if b {} else {}"), 3);
    assert_eq!(w("let g = || { if a {} }; g();"), 2);
    assert_eq!(w("let Some(_y) = x else { return };"), 1);
    // `?`, `return` and an unlabelled `break` add nothing.
    assert_eq!(w("loop { break; }"), 1);
}

/// The way down is the smallest part that brings the total to the bound, else the largest.
#[test]
fn the_way_down_is_the_least_that_is_enough() {
    let r = |at: u32| syntax::TextRange::new(at.into(), (at + 1).into());
    // Total 20, bound 15: 5 is enough and smallest of those that are; 2 is not enough.
    assert_eq!(weight::way_down(20, 15, &[(r(0), 2), (r(1), 9), (r(2), 5)]), Some(2));
    // Nothing is enough alone: the largest.
    assert_eq!(weight::way_down(40, 15, &[(r(0), 2), (r(1), 9)]), Some(1));
    // A part that is the whole is no way down.
    assert_eq!(weight::way_down(9, 5, &[(r(0), 9)]), None);
}

#[test]
fn a_value_at_its_bound_or_at_what_it_is_held_at_is_no_finding() {
    assert!(!fires(15, 15, None));
    assert!(fires(16, 15, None));
    assert!(!fires(40, 15, Some(40)));
    assert!(fires(41, 15, Some(40)));
    assert!(!fires(12, 15, Some(40)));
}

#[test]
fn every_measure_has_its_own_key_and_name() {
    for (at, a) in Measure::ALL.iter().enumerate() {
        for b in &Measure::ALL[at + 1..] {
            assert_ne!(a.key(), b.key());
            assert_ne!(a.name(), b.name());
        }
    }
}

#[test]
fn fields_that_share_a_prefix_are_a_group() {
    let names: Vec<String, Global> = {
        let mut it = Vec::new_in(Global);
        it.extend(["cache_hits", "cache_misses", "cache_bytes", "name", "io_reads"].map(str::to_owned));
        it
    };
    assert_eq!(group_of(&names), Some(("cache", 3)));
}

/// Tarjan's components: 0 <-> 1 is one, 2 -> 3 two of one each.
#[test]
fn components_are_the_cycles() {
    let edges = |list: &[u32]| {
        let mut it = Vec::new_in(Global);
        it.extend_from_slice(list);
        it
    };
    let graph = [edges(&[1]), edges(&[0, 2]), edges(&[3]), edges(&[])];
    let groups = strongly_connected(&graph);
    assert_eq!(groups.iter().map(|it| it.as_slice()).collect::<Vec<_>>(), [&[0, 1][..], &[2], &[3]]);
}

const PRIMES: &str = r#"
pub fn sum_primes(max: u64) -> u64 {
    let mut total = 0;
    let mut i = 1;
    'outer: while i < max {
        i += 1;
        let mut j = 2;
        while j < i {
            if i % j == 0 {
                continue 'outer;
            }
            j += 1;
        }
        total += i;
    }
    total
}
"#;

/// `sum_primes` is `sumOfPrimes` with `while`: 7, as Campbell's paper works it out.
#[test]
fn function_weight_fires_over_its_bound_and_not_at_it() {
    let out = exactly(PRIMES, "function_weight", "function-weight-threshold", 7);
    assert!(out.contains("`sum_primes` weighs 7 by cognitive complexity, over the bound of 6"), "{out}");
    assert!(out.contains("item baseline line: function_weight\tmain::sum_primes\t"), "{out}");
    // The whole outer loop carries all 7, so it is no way down; of the inner loop (6) and its
    // `if` (4), both enough, the smaller.
    assert!(
        out.contains("move the if at lines 8-10 of the function into its own function: it carries 4 of the 7 (57%)"),
        "{out}"
    );
}

#[test]
fn deep_nesting_is_the_control_flow_depth() {
    let out = exactly(PRIMES, "deep_nesting", "nesting-threshold", 3);
    assert!(out.contains("`sum_primes` nests control flow 3 deep"), "{out}");
}

/// Lines of code from `fn` on: the 16 lines of `sum_primes`, none blank.
#[test]
fn long_function_counts_lines_of_code() {
    let out = exactly(PRIMES, "long_function", "function-lines-threshold", 16);
    assert!(out.contains("`sum_primes` is 16 lines of code"), "{out}");
}

#[test]
fn many_params_does_not_count_self() {
    let main = r#"
pub struct S;
impl S {
    pub fn f(&self, a: u8, b: u8, c: u8) -> u8 { a + b + c }
}
"#;
    let out = exactly(main, "many_params", "params-threshold", 3);
    assert!(out.contains("`S::f` takes 3 parameters"), "{out}");
}

/// NPATH (Nejmeh 1988): three `if`s in sequence, each with no `else`, are 2 * 2 * 2 = 8 paths;
/// a fourth inside the first adds one path to its branch: (2 + 1) * 2 * 2 = 12.
#[test]
fn path_count_multiplies_sequences_and_adds_branches() {
    let main = r#"
pub fn f(a: bool, b: bool, c: bool) -> u8 {
    let mut n = 0;
    if a { n = 1; }
    if b { n = 2; }
    if c { n = 3; }
    n
}
pub fn g(a: bool, b: bool, c: bool, d: bool) -> u8 {
    let mut n = 0;
    if a { if d { n = 4; } }
    if b { n = 2; }
    if c { n = 3; }
    n
}
"#;
    let out = run(main, "path_count", "path-count-threshold = 7");
    assert!(out.contains("`f` has 8 paths through it"), "{out}");
    assert!(out.contains("`g` has 12 paths through it"), "{out}");
    let out = run(main, "path_count", "path-count-threshold = 12");
    assert!(!out.contains(&named("path_count")), "{out}");
}

/// Three `mut` bindings live across the call of `g`; `d` is not `mut`, `e` is dead by then.
#[test]
fn live_state_counts_mutable_places_across_one_call() {
    let main = r#"
fn g() {}
pub fn f(d: u8) -> u8 {
    let mut a = 1;
    let mut b = 2;
    let mut c = 3;
    let mut e = 4;
    e = e;
    let _ = e;
    g();
    a = a + d;
    b = b + a;
    c = c + b;
    c
}
"#;
    let out = exactly(main, "live_state", "live-state-threshold", 3);
    assert!(out.contains("`f` holds 3 mutable places live across one call (`a`, `b`, `c`)"), "{out}");
}

/// Three `return`s while a `Guard`, whose drop is its own, is held; with no such value the
/// same exits are not counted.
#[test]
fn exits_count_only_while_a_drop_finishes_something() {
    let main = r#"
pub struct Guard;
impl Drop for Guard {
    fn drop(&mut self) {}
}
pub fn held(a: bool, b: bool, c: bool) -> u8 {
    let _guard = Guard;
    if a { return 1; }
    if b { return 2; }
    if c { return 3; }
    0
}
pub fn free(a: bool, b: bool, c: bool) -> u8 {
    if a { return 1; }
    if b { return 2; }
    if c { return 3; }
    0
}
"#;
    let out = exactly(main, "exits", "exits-threshold", 3);
    assert!(out.contains("`held` has 3 early exits while it holds `_guard`, a `Guard`"), "{out}");
    assert!(!out.contains("`free`"), "{out}");
}

/// `Mutex::lock` blocks and locks, `File::sync_all` does IO, blocks and syncs (hir-ty-pure's
/// table, `effects.rs`): four kinds of the six.
#[test]
fn effect_breadth_is_the_kinds_its_calls_bring() {
    let main = r#"
pub fn f(m: &std::sync::Mutex, file: &std::fs::File) -> u64 {
    let _ = m.lock();
    file.sync_all()
}
"#;
    let std = r#"
//- minicore: sized, copy, clone, fn, option, drop
//- /std/lib.rs crate:std
pub mod prelude {
    pub mod v1 { pub use core::prelude::v1::*; }
    pub mod rust_2015 { pub use super::v1::*; }
    pub mod rust_2018 { pub use super::v1::*; }
    pub mod rust_2021 { pub use super::v1::*; }
    pub mod rust_2024 { pub use super::v1::*; }
}
pub mod sync {
    pub struct Mutex(u64);
    impl Mutex {
        pub fn lock(&self) -> u64 { self.0 }
    }
}
pub mod fs {
    pub struct File(u64);
    impl File {
        pub fn sync_all(&self) -> u64 { self.0 }
    }
}
"#;
    let lint = |bound: u64| {
        let fixture = format!("{std}{MAIN}{}", main.trim_start_matches('\n'));
        lint_with(
            &fixture,
            "-A clippy::all --warn=blaze::effect_breadth",
            conf(&format!("effect-breadth-threshold = {bound}")),
        )
    };
    let out = lint(3);
    assert!(out.contains("`f` does 4 kinds of effect through its own calls"), "{out}");
    assert!(out.contains("locking through `Mutex::lock`"), "{out}");
    assert!(out.contains("syncing to disk through `File::sync_all`"), "{out}");
    let out = lint(4);
    assert!(!out.contains(&named("effect_breadth")), "{out}");
}

#[test]
fn fan_out_counts_distinct_callees() {
    let main = r#"
fn a() {}
fn b() {}
fn c() {}
pub fn f() {
    a();
    b();
    b();
    c();
}
"#;
    let out = exactly(main, "fan_out", "fan-out-threshold", 3);
    assert!(out.contains("`f` calls 3 distinct functions"), "{out}");
}

#[test]
fn wide_type_counts_fields_and_variants_with_data() {
    let main = r#"
pub struct Wide {
    pub cache_hits: u8,
    pub cache_misses: u8,
    pub cache_bytes: u8,
}
pub enum Data {
    A(u8),
    B,
    C,
}
pub enum Names {
    A,
    B,
    C,
    D,
}
"#;
    let out = run(main, "wide_type", "struct-fields-threshold = 2\nenum-variants-threshold = 2");
    assert!(out.contains("`struct Wide` has 3 fields"), "{out}");
    assert!(out.contains("the 3 fields named `cache_..` travel together"), "{out}");
    assert!(out.contains("`enum Data` has 3 variants"), "{out}");
    assert!(!out.contains("`enum Names`"), "a fieldless enum is a list of names:\n{out}");
    let out = run(main, "wide_type", "struct-fields-threshold = 3\nenum-variants-threshold = 3");
    assert!(!out.contains(&named("wide_type")), "{out}");
}

/// The 4 lines of code of `src/lib.rs`, its `#[cfg(test)]` module left out.
#[test]
fn long_file_leaves_tests_out() {
    let main = r#"
pub fn a() {}

pub fn b() {}
// a comment
pub fn c() {}
pub fn d() {}
#[cfg(test)]
mod tests {
    fn t() {}
}
"#;
    let out = exactly(main, "long_file", "file-lines-threshold", 4);
    assert!(out.contains("this file (`main`) is 4 lines of code"), "{out}");
}

/// `a` uses `b`'s `B` and `b` uses `a::inner`'s `A` (lifted to `a`): one cycle of two; `c`
/// uses `a`, and nothing uses `c`, so it is not in it.
#[test]
fn module_cycle_lists_the_edges_and_the_cut() {
    let main = r#"
pub mod a {
    pub mod inner { pub struct A; }
    pub use crate::b::B;
}
pub mod b {
    pub struct B;
    pub use crate::a::inner::A;
}
pub mod c {
    pub use crate::a::inner::A;
}
"#;
    let out = run(main, "module_cycle", "");
    assert!(out.contains("2 modules of this crate import from each other: `main::a` -> `main::b` (B); `main::b` -> `main::a` (A)"), "{out}");
    assert!(out.contains("cut `main::a` -> `main::b`"), "{out}");
    assert!(!out.contains("main::c"), "{out}");
}

/// A held item fires only above its held value, found by path when its tokens changed.
#[test]
fn a_held_item_fires_only_above_what_it_is_held_at() {
    let held = |value: u64| {
        let entry = Entry {
            key: Key { measure: "function_weight".to_owned(), path: "main::sum_primes".to_owned(), fingerprint: 1 },
            value,
        };
        let loaded = Loaded {
            path: PathBuf::from("/w/crates/blaze-lints/baselines/items.tsv"),
            id: [value as u8; 32],
            items: Arc::new(Items::of([entry])),
        };
        let mut it = conf("function-weight-threshold = 0");
        it.items = Some(loaded);
        let fixture = format!("{MINICORE}{ALLOC_STD}{MAIN}{}", PRIMES.trim_start_matches('\n'));
        lint_with(&fixture, "-A clippy::all --warn=blaze::function_weight", it)
    };
    let out = held(7);
    assert!(!out.contains(&named("function_weight")), "held at 7, it is 7:\n{out}");
    let out = held(6);
    assert!(out.contains("over the bound of 0, and over the 6 it is held at"), "{out}");
}

/// Every complexity lint warned, every bound 0: each item's held line, as `(measure, path,
/// value)`, sorted.
fn values(main: &str) -> Vec<(String, String, u64)> {
    let lints: String = super::LINTS.iter().map(|it| format!(" --warn=blaze::{}", it.name())).collect();
    let bounds: String = Measure::ALL.iter().map(|it| format!("{} = 0\n", it.key())).collect();
    let fixture = format!("{MINICORE}{ALLOC_STD}{MAIN}{main}");
    let out = lint_with(&fixture, &format!("-A clippy::all{lints}"), conf(&bounds));
    let mut found: Vec<(String, String, u64)> = out
        .lines()
        .filter_map(|line| line.find(items::NOTE).and_then(|at| items::from_note(&line[at..])))
        .map(|it| (it.key.measure, it.key.path, it.value))
        .collect();
    found.sort();
    found.dedup();
    found
}

/// The invariant: renaming a function's locals and reordering a file's items changes no
/// measured value of any item (each measure reads structure, not names or order).
#[hegel::test(test_cases = 8)]
fn renaming_locals_and_reordering_items_changes_no_value(tc: hegel::TestCase) {
    use hegel::generators as gs;
    let names = tc.draw(gs::permutations(vec!["x", "y", "z", "w", "v"]));
    let order = tc.draw(gs::permutations(vec![0usize, 1, 2]));
    let item = |at: usize, [p, q, r, s, t]: [&str; 5]| match at {
        0 => format!(
            "pub fn first(max: u64) -> u64 {{\n    let mut {p} = 0;\n    let mut {q} = 1;\n    while {q} < max {{\n        if {q} % 2 == 0 && {p} < 9 || max == 3 {{ {p} += {q}; }}\n        {q} += 1;\n    }}\n    {p}\n}}\n"
        ),
        1 => format!(
            "fn helper() -> u64 {{ 1 }}\npub fn second({r}: bool, {s}: bool) -> u64 {{\n    let mut {t} = helper();\n    if {r} {{ {t} = 2; }} else if {s} {{ {t} = 3; }}\n    match {t} {{ 1 => helper(), _ => {t} }}\n}}\n"
        ),
        _ => "pub struct Third {\n    pub a: u8,\n    pub b: u8,\n}\n".to_owned(),
    };
    let plain = ["x", "y", "z", "w", "v"];
    let renamed = [names[0], names[1], names[2], names[3], names[4]];
    let base: String = (0..3).map(|at| item(at, plain)).collect();
    let other: String = order.iter().map(|&at| item(at, renamed)).collect();
    let before = values(&base);
    assert!(!before.is_empty(), "the measures find something to compare");
    assert_eq!(values(&other), before);
}
