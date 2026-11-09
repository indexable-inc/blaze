//! `BLAZE_BORROWCK=polonius-flow` against the cases of fed1eb428d:docs/research/borrowck.md section 2.5:
//! what the alpha rejects and datalog Polonius accepts is accepted, what all of them reject stays
//! rejected. Each upstream test is written here without `std` (a `Box` is a `&'static mut`, a
//! `Vec` a slice, a `Cell` an invariant `fn` pointer, `String` a `u32`), keeping the borrows and
//! the control flow that make the verdict; the upstream files themselves are run by
//! `harness/v2/frontend/borrowck_flow.py`.

use std::time::Duration;

use hir_def::DefWithBodyId;
use hir_expand::fixture;

use crate::{BorrowckResult, Mode, borrowck_in_mode, budget, timings};

/// Rejected by the alpha, accepted by legacy Polonius.
const LEGACY_ACCEPTS: &str = r#"
//- minicore: sized, copy, clone, fn, index, slice, add, option, result
fn as_mut<T>(it: &mut Option<T>) -> Option<&mut T> {
    match it {
        Some(x) => Some(x),
        None => None,
    }
}
fn unwrap<T>(it: Option<T>) -> T {
    match it {
        Some(x) => x,
        None => loop {},
    }
}

// tests/ui/nll/polonius/iterating-updating-cursor-issue-57165.rs
struct X { next: Option<&'static mut X> }
fn conditional(mut p: &mut Option<&'static mut X>) {
    while let Some(now) = p {
        if true {
            p = &mut now.next;
        }
    }
}
fn conditional_with_indirection(mut p: &mut Option<&'static mut X>) {
    while let Some(now) = p {
        if true {
            p = &mut unwrap(as_mut(&mut *p)).next;
        }
    }
}

// tests/ui/nll/polonius/iterating-updating-cursor-issue-63908.rs
struct Node { value: u32, next: Option<&'static mut Node> }
fn remove_last_node_iterative(mut node_ref: &mut Option<&'static mut Node>) {
    loop {
        let next_ref = &mut unwrap(as_mut(&mut *node_ref)).next;
        if next_ref.is_some() {
            node_ref = next_ref;
        } else {
            break;
        }
    }
    *node_ref = None;
}

// tests/ui/nll/polonius/iterating-updating-cursor-issue-108704.rs
struct Tree { name: u32, children: &'static mut [Tree] }
fn merge_tree_ko(root: &mut Tree, path: &[u32]) {
    let mut elements = &mut *root.children;
    let mut i = 0;
    while i < path.len() {
        let mut idx = 0;
        while idx < elements.len() {
            let el = &mut elements[idx];
            if el.name == path[i] {
                elements = &mut *el.children;
                break;
            }
            idx += 1;
        }
        i += 1;
    }
}

// tests/ui/nll/issue-46589.rs
struct Foo;
impl Foo {
    fn get_self(&mut self) -> Option<&mut Self> {
        Some(self)
    }
    fn new_self(&mut self) -> &mut Self {
        self
    }
    fn trigger_bug(&mut self) {
        let other = &mut (&mut *self);
        *other = match (*other).get_self() {
            Some(s) => s,
            None => (*other).new_self(),
        };
        let c = other;
    }
}

// tests/ui/nll/polonius/flow-sensitive-invariance.rs
struct Invariant<'l>(fn(&'l ()) -> &'l ());
fn create_invariant<'l>() -> Invariant<'l> {
    loop {}
}
fn use_it<'a, 'b>(choice: bool) -> Result<Invariant<'a>, Invariant<'b>> {
    let returned_value = create_invariant();
    if choice { Ok(returned_value) } else { Err(returned_value) }
}
"#;

const ACCEPTED: &[&str] = &[
    "conditional",
    "conditional_with_indirection",
    "remove_last_node_iterative",
    "merge_tree_ko",
    "trigger_bug",
    "use_it",
];

/// Rejected by the alpha and by legacy Polonius: what neither accepts is never accepted.
const ALL_REJECT: &str = r#"
//- minicore: sized, copy, clone, option, drop
fn as_mut<T>(it: &mut Option<T>) -> Option<&mut T> {
    match it {
        Some(x) => Some(x),
        None => None,
    }
}

// tests/ui/nll/polonius/polonius-smoke-test.rs
pub fn return_ref_to_local() -> &'static i32 {
    let x = 0;
    &x
}
pub fn use_while_mut() {
    let mut x = 0;
    let y = &mut x;
    let z = x;
    let w = y;
}
pub fn use_while_mut_fr(x: &mut i32) -> &mut i32 {
    let y = &mut *x;
    let z = x;
    y
}
fn foo<'a, 'b>(p: &'b &'a mut usize) -> &'b usize {
    p
}
fn well_formed_function_inputs() {
    let s = &mut 1;
    let r = &mut *s;
    let tmp = foo(&r);
    s;
    tmp;
}

// tests/ui/nll/polonius/subset-relations.rs
fn missing_subset<'a, 'b>(x: &'a u32, y: &'b u32) -> &'a u32 {
    y
}

// tests/ui/nll/polonius/lending-iterator-sanity-checks.rs
trait LendingIterator {
    type Item<'a>
    where
        Self: 'a;
    fn next(&mut self) -> Option<Self::Item<'_>>;
}
fn use_live<T: LendingIterator>(t: &mut T) -> Option<(T::Item<'_>, T::Item<'_>)> {
    let Some(i) = t.next() else { return None };
    let Some(j) = t.next() else { return None };
    Some((i, j))
}
fn drop_live<T: LendingIterator>(t: &mut T) {
    let i = t.next();
    let j = t.next();
}

// tests/ui/nll/polonius/assignment-to-differing-field.rs
struct List { value: u32, next: Option<&'static mut List> }
struct Out<'a> { slot: Option<&'a mut u32> }
fn keep<'a>(out: &mut Out<'a>, it: &'a mut u32) {}
fn assignment_to_field_projection<'a>(mut list: (&'a mut List, &'a mut List), out: &mut Out<'a>) {
    loop {
        keep(out, &mut (list.0).value);
        if let Some(n) = as_mut(&mut (list.0).next) {
            list.1 = &mut **n;
        } else {
            return;
        }
    }
}

// tests/ui/nll/get_default.rs
struct Map {}
impl Map {
    fn get(&self) -> Option<&u32> { None }
    fn set(&mut self, v: u32) {}
}
fn err(map: &mut Map) -> &u32 {
    loop {
        match map.get() {
            Some(v) => {
                map.set(0);
                return v;
            }
            None => {
                map.set(0);
            }
        }
    }
}
"#;

const REJECTED: &[&str] = &[
    "return_ref_to_local",
    "use_while_mut",
    "use_while_mut_fr",
    "well_formed_function_inputs",
    "missing_subset",
    "use_live",
    "drop_live",
    "assignment_to_field_projection",
    "err",
];

fn errors(result: &BorrowckResult) -> Vec<&'static str> {
    // `#[blaze::linear]`'s errors are the same in every mode.
    result
        .diagnostics
        .iter()
        .filter(|it| it.is_error() && it.code != hir_ty_linear::LinearDiagnostic::CODE)
        .map(|it| it.code)
        .collect()
}

/// Each function of `fixture` named in `names`, checked in the alpha and in `polonius-flow`.
fn checked(fixture: &str, names: &[&str]) -> Vec<(String, BorrowckResult, BorrowckResult)> {
    let fx = fixture::db(fixture);
    let _ = fx.only_file();
    let db = fx.into_db();
    blaze_query::db::attach(&db, || {
        let functions = budget::functions(&db);
        names
            .iter()
            .map(|&name| {
                let def = functions
                    .iter()
                    .find(|(it, _)| it == name)
                    .map(|&(_, def)| def)
                    .unwrap_or_else(|| panic!("no function `{name}` in the fixture"));
                let alpha = borrowck_in_mode(&db, def, Mode::Polonius);
                let flow = borrowck_in_mode(&db, def, Mode::PoloniusFlow);
                for result in [&alpha, &flow] {
                    assert_eq!(result.skipped, None, "`{name}`");
                    assert_eq!(result.not_checked, None, "`{name}`");
                }
                (name.to_owned(), alpha, flow)
            })
            .collect()
    })
}

#[test]
fn polonius_flow_accepts_what_legacy_polonius_accepts_and_the_alpha_rejects() {
    for (name, alpha, flow) in checked(LEGACY_ACCEPTS, ACCEPTED) {
        assert!(!errors(&alpha).is_empty(), "`{name}`: the alpha accepts it, so it tests nothing");
        assert_eq!(errors(&flow), Vec::<&str>::new(), "`{name}` in polonius-flow: {flow:#?}");
    }
}

#[test]
fn polonius_flow_rejects_what_every_polonius_rejects() {
    for (name, alpha, flow) in checked(ALL_REJECT, REJECTED) {
        assert!(!errors(&alpha).is_empty(), "`{name}`: the alpha accepts it");
        assert!(!errors(&flow).is_empty(), "`{name}`: polonius-flow accepts it: {flow:#?}");
    }
}

/// The benchmark: the time `polonius-flow` adds per body, from [`timings::measure`] (the
/// `borrowck.flow` span measures the same in a build). A body the alpha accepts costs nothing
/// more; one it rejects costs a second check, of which `Phase::Flow` is the datalog.
///
///     cargo test -p hir-ty-borrowck flow_cost_per_body -- --ignored --nocapture
#[test]
#[ignore = "a benchmark: prints the cost per body"]
fn flow_cost_per_body() {
    const RUNS: u32 = 50;
    for (fixture, names) in [(LEGACY_ACCEPTS, ACCEPTED), (ALL_REJECT, REJECTED)] {
        let fx = fixture::db(fixture);
        let _ = fx.only_file();
        let db = fx.into_db();
        blaze_query::db::attach(&db, || {
            let functions = budget::functions(&db);
            for &name in names {
                let def: DefWithBodyId =
                    functions.iter().find(|(it, _)| it == name).map(|&(_, def)| def).unwrap();
                // Warm the queries a check asks.
                let _ = borrowck_in_mode(&db, def, Mode::PoloniusFlow);
                let time = |mode| {
                    let mut total = Duration::ZERO;
                    let mut flow = Duration::ZERO;
                    for _ in 0..RUNS {
                        let started = std::time::Instant::now();
                        let (_, timings) = timings::measure(|| borrowck_in_mode(&db, def, mode));
                        total += started.elapsed();
                        flow += timings.get(timings::Phase::Flow);
                    }
                    (total / RUNS, flow / RUNS)
                };
                let (alpha, _) = time(Mode::Polonius);
                let (both, datalog) = time(Mode::PoloniusFlow);
                println!(
                    "{name:32} alpha {alpha:>10?}  polonius-flow {both:>10?}  of which datalog {datalog:>10?}"
                );
            }
        });
    }
}
