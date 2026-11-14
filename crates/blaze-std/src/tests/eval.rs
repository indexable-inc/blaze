//! `Eval` gives a memo point's own value, whichever provider answers:
//!
//! - [`values`](a_call_gives_the_points_own_value): under `direct` and `memo`, every call of a
//!   drawn sequence gives what the plain function gives, and what the same call gave before (a
//!   model map), for two points called with the same arguments.
//!   Plants: [`Plant::KeyIgnoresLast`] (the key leaves an argument out) and
//!   [`Plant::MemoSharesPoints`] (the point is left out of what an entry is kept under).
//! - [`limits`](a_memo_stays_within_its_limit): `memo` never holds more entries or bytes than
//!   its limit, and until it has dropped or refused an entry each distinct call's body runs at
//!   most once (counted by a handler outside it, not by its own counters).
//!   Plant: [`Plant::MemoNeverEvicts`].
//! - [`provenance`](provenance_records_the_calls_made): the graph `Provenance` writes holds
//!   exactly the calls made, each under the call whose body made it, in order (a model the
//!   test builds from the points' own rule).
//!   Plant: [`Plant::NestedUnderGrandparent`].
//! - keys: equal values give equal keys, byte for byte across two computations, and values
//!   that differ give keys that differ.
//!
//! That `#[blaze::memo]` on a function that is not pure is refused is blaze-lints'
//! `a_memo_point_that_is_not_pure_is_refused` (crates/blaze-lints/src/blaze/effects_tests.rs),
//! the harness `#[blaze::pure]`'s own refusals use. A point here cannot count its own runs or
//! call itself: either would not be pure, or would read its own code identity.

use std::cell::RefCell;

use blaze_sim::{Config, Tape};

use super::{check, control, root, world};
use crate::{
    Plant,
    cx::Cx,
    eval::{
        At, Called, Eval, Graph, Handler, Key, MemoLimit, Nested, Next, Point, Provenance,
    },
    id::ContentId,
};

/// Room for every entry a plan can make: only the entry limit evicts.
const ROOMY: usize = 1 << 20;

fn mix_of(a: u64, b: u64) -> u64 {
    a.wrapping_mul(1000).wrapping_add(b.wrapping_mul(7)).wrapping_add(3)
}

/// Never [`mix_of`] of the same arguments: `999 a = 999_996 b + 8` has no solution.
fn sum_of(a: u64, b: u64) -> u64 {
    a.wrapping_add(b.wrapping_mul(1_000_003)).wrapping_add(11)
}

#[blaze::memo]
fn mix(_: &mut Nested<'_>, (a, b): (u64, u64)) -> u64 {
    mix_of(a, b)
}

#[blaze::memo]
fn sum(_: &mut Nested<'_>, (a, b): (u64, u64)) -> u64 {
    sum_of(a, b)
}

/// One call of a plan: [`sum`] or [`mix`], and its arguments.
type Asked = (bool, u64, u64);

fn asked(tape: &Tape, most: u64) -> Asked {
    (tape.coin("is sum"), tape.int_in(0..=most, "a"), tape.int_in(0..=most, "b"))
}

fn ask(eval: &Eval<'_>, cx: &Cx, (is_sum, a, b): Asked) -> u64 {
    match is_sum {
        true => eval.call(cx, sum, (a, b)),
        false => eval.call(cx, mix, (a, b)),
    }
}

fn plain((is_sum, a, b): Asked) -> u64 {
    match is_sum {
        true => sum_of(a, b),
        false => mix_of(a, b),
    }
}

fn harness(what: impl std::fmt::Display) -> String {
    format!("harness: {what}")
}

// Values.

#[derive(Debug)]
struct Values {
    seed: u64,
    memo: bool,
    entries: usize,
    calls: Vec<Asked>,
}

fn values(tape: &Tape) -> Values {
    let seed = tape.next_u64("sim seed");
    let memo = tape.coin("memo");
    let entries = tape.int_in(0..=16u64, "entries") as usize;
    // Few values, so calls repeat and differ in one argument or in the point alone.
    let calls = (0..tape.int_in(1..=32u64, "calls")).map(|_| asked(tape, 3)).collect();
    Values { seed, memo, entries, calls }
}

fn run_values(plan: &Values, plant: Plant) -> Result<(), String> {
    let io = world(plan.seed, plant);
    let cx = root(&io);
    let eval = match plan.memo {
        true => {
            let mem = io.need_mem().map_err(harness)?;
            Eval::memo(&mem, MemoLimit { entries: plan.entries, bytes: ROOMY }).map_err(harness)?
        }
        false => Eval::direct(),
    };
    let mut model: Vec<(Asked, u64)> = Vec::new();
    for (at, &call) in plan.calls.iter().enumerate() {
        let got = ask(&eval, &cx, call);
        match model.iter().find(|(known, _)| *known == call) {
            Some(&(_, first)) if first != got => {
                return Err(format!("call {at} {call:?} gave {got}, and {first} the first time"));
            }
            Some(_) => {}
            None => model.push((call, got)),
        }
        if got != plain(call) {
            return Err(format!(
                "call {at} {call:?} gave {got}, where the plain function gives {}",
                plain(call)
            ));
        }
    }
    Ok(())
}

/// The invariant: `Eval` gives a memo point's own value. Under `direct` and `memo`, every call
/// gives what the plain function gives and what the same call gave before, for two points
/// called with the same arguments.
#[test]
fn a_call_gives_the_points_own_value() {
    blaze_sim::check(Config::cases(200), |tape| check(values(tape), run_values));
}

#[test]
fn the_oracle_catches_a_key_that_ignores_an_argument() {
    control(Plant::KeyIgnoresLast, values, run_values);
}

#[test]
fn the_oracle_catches_two_points_sharing_entries() {
    control(Plant::MemoSharesPoints, values, run_values);
}

// Limits.

/// Counts, outside the evaluator it wraps, how often each call's body ran.
struct Count<'a> {
    inner: &'a Eval<'a>,
    runs: RefCell<Vec<(Called, u32)>>,
}

impl Handler for Count<'_> {
    fn run(&self, cx: &Cx, at: At, next: &mut Next<'_>) {
        next.through(cx, self.inner);
        if next.ran() {
            let mut runs = self.runs.borrow_mut();
            match runs.iter_mut().find(|(call, _)| *call == at.called()) {
                Some((_, count)) => *count += 1,
                None => runs.push((at.called(), 1)),
            }
        }
    }

    fn name(&self) -> &'static str {
        "the counting handler"
    }
}

#[derive(Debug)]
struct Limits {
    seed: u64,
    limit: MemoLimit,
    calls: Vec<Asked>,
}

fn limits(tape: &Tape) -> Limits {
    let seed = tape.next_u64("sim seed");
    // From an arena too small for one entry to one with room for all.
    let bytes = *tape.one_of(&[256usize, 2048, 16 << 10, ROOMY], "memo bytes");
    let limit = MemoLimit { entries: tape.int_in(0..=12u64, "entries") as usize, bytes };
    let calls = (0..tape.int_in(1..=96u64, "calls")).map(|_| asked(tape, 7)).collect();
    Limits { seed, limit, calls }
}

fn run_limits(plan: &Limits, plant: Plant) -> Result<(), String> {
    let io = world(plan.seed, plant);
    let cx = root(&io);
    let mem = io.need_mem().map_err(harness)?;
    let inner = Eval::memo(&mem, plan.limit).map_err(harness)?;
    let count = Count { inner: &inner, runs: RefCell::new(Vec::new()) };
    let eval = Eval::handler(&count);
    for (at, &call) in plan.calls.iter().enumerate() {
        let got = ask(&eval, &cx, call);
        if got != plain(call) {
            return Err(format!("call {at} {call:?} gave {got}, not {}", plain(call)));
        }
        let Some(stats) = inner.memo_stats() else {
            return Err("a remembering evaluator has no stats".to_owned());
        };
        if stats.entries > plan.limit.entries {
            return Err(format!(
                "after call {at} the memo holds {} entries, past its limit {}",
                stats.entries, plan.limit.entries
            ));
        }
        if stats.bytes > plan.limit.bytes {
            return Err(format!(
                "after call {at} the memo's arenas handed out {} bytes, past its limit {}",
                stats.bytes, plan.limit.bytes
            ));
        }
        let runs = count.runs.borrow();
        let ran: u64 = runs.iter().map(|&(_, count)| u64::from(count)).sum();
        if ran != stats.computed {
            return Err(format!(
                "after call {at} the bodies ran {ran} times and the memo says {}",
                stats.computed
            ));
        }
        // Under its limit so far: nothing dropped, nothing refused.
        if stats.evicted == 0 && stats.uncached == 0 {
            if let Some((call, count)) = runs.iter().find(|&&(_, count)| count > 1) {
                return Err(format!(
                    "after call {at} the body of {call:?} ran {count} times with the memo \
                     under its limit"
                ));
            }
        }
    }
    Ok(())
}

/// The invariant: a memo never holds more entries or bytes than its limit, and until it has
/// dropped or refused an entry each distinct call's body runs at most once.
#[test]
fn a_memo_stays_within_its_limit() {
    blaze_sim::check(Config::cases(200), |tape| check(limits(tape), run_limits));
}

#[test]
fn the_oracle_catches_a_memo_that_never_evicts() {
    control(Plant::MemoNeverEvicts, limits, run_limits);
}

// Provenance.

/// Whether the point at `row` calls its input `col`, by the plan's `shape`: rows 0 to 3 are
/// [`mid`]'s calls of [`leaf`], row 4 is [`top`]'s calls of [`mid`].
fn wants(shape: u64, row: u64, col: u64) -> bool {
    (shape >> (row * 4 + col)) & 1 == 1
}

fn leaf_of(x: u64) -> u64 {
    x.wrapping_mul(3).wrapping_add(1)
}

fn mid_of(shape: u64, row: u64) -> u64 {
    (0..4).filter(|&col| wants(shape, row, col)).fold(row, |total, col| total.wrapping_add(leaf_of(col)))
}

fn top_of(shape: u64) -> u64 {
    (0..4).filter(|&row| wants(shape, 4, row)).fold(0, |total, row| total.wrapping_add(mid_of(shape, row)))
}

#[blaze::memo]
fn leaf(_: &mut Nested<'_>, (x,): (u64,)) -> u64 {
    leaf_of(x)
}

#[blaze::memo]
fn mid(points: &mut Nested<'_>, (shape, row): (u64, u64)) -> u64 {
    let mut total = row;
    for col in 0..4 {
        if wants(shape, row, col) {
            total = total.wrapping_add(points.call(leaf, (col,)));
        }
    }
    total
}

#[blaze::memo]
fn top(points: &mut Nested<'_>, (shape,): (u64,)) -> u64 {
    let mut total = 0u64;
    for row in 0..4 {
        if wants(shape, 4, row) {
            total = total.wrapping_add(points.call(mid, (shape, row)));
        }
    }
    total
}

#[derive(Debug)]
struct Traced {
    seed: u64,
    /// Whether the evaluator under the provenance remembers.
    memo: bool,
    /// The shape of each call of [`top`], in order; few shapes, so calls repeat.
    shapes: Vec<u64>,
}

fn traced(tape: &Tape) -> Traced {
    let seed = tape.next_u64("sim seed");
    let memo = tape.coin("memo");
    let pool: Vec<u64> = (0..3).map(|_| tape.int_in(0..=(1u64 << 20) - 1, "shape")).collect();
    let shapes = (0..tape.int_in(1..=8u64, "calls")).map(|_| *tape.one_of(&pool, "which shape")).collect();
    Traced { seed, memo, shapes }
}

/// The node `call` of `model`, added with no inputs when it is new.
fn node(model: &mut Vec<(Called, Vec<Called>)>, call: Called) -> &mut Vec<Called> {
    let at = match model.iter().position(|(known, _)| *known == call) {
        Some(at) => at,
        None => {
            model.push((call, Vec::new()));
            model.len() - 1
        }
    };
    &mut model[at].1
}

/// `made` as an input of `parent` in `model`, once.
fn edge(model: &mut Vec<(Called, Vec<Called>)>, parent: Called, made: Called) {
    node(model, made);
    let inputs = node(model, parent);
    if !inputs.contains(&made) {
        inputs.push(made);
    }
}

fn run_traced(plan: &Traced, plant: Plant) -> Result<(), String> {
    let io = world(plan.seed, plant);
    let cx = root(&io);
    let mem = io.need_mem().map_err(harness)?;
    let arena = mem.arena("test provenance", ROOMY).map_err(harness)?;
    let graph = Graph::new_in(&arena);
    let inner = match plan.memo {
        true => Eval::memo(&mem, MemoLimit { entries: 64, bytes: ROOMY }).map_err(harness)?,
        false => Eval::direct(),
    };
    let provenance = Provenance::new(&inner, &graph);
    let eval = Eval::handler(&provenance);

    let top_at = |shape: u64| Called { point: Point::of(&top), key: Key::of(&(shape,)) };
    let mid_at = |shape: u64, row: u64| Called { point: Point::of(&mid), key: Key::of(&(shape, row)) };
    let leaf_at = |col: u64| Called { point: Point::of(&leaf), key: Key::of(&(col,)) };
    let mut model: Vec<(Called, Vec<Called>)> = Vec::new();
    let mut roots: Vec<Called> = Vec::new();
    for (at, &shape) in plan.shapes.iter().enumerate() {
        let got = eval.call(&cx, top, (shape,));
        if got != top_of(shape) {
            return Err(format!("call {at} of top({shape:#x}) gave {got}, not {}", top_of(shape)));
        }
        // What the points' own rule says was called from what.
        node(&mut model, top_at(shape));
        if !roots.contains(&top_at(shape)) {
            roots.push(top_at(shape));
        }
        for row in (0..4).filter(|&row| wants(shape, 4, row)) {
            edge(&mut model, top_at(shape), mid_at(shape, row));
            for col in (0..4).filter(|&col| wants(shape, row, col)) {
                edge(&mut model, mid_at(shape, row), leaf_at(col));
            }
        }
    }
    if graph.len() != model.len() {
        return Err(format!("the graph holds {} calls, and {} were made", graph.len(), model.len()));
    }
    let recorded = graph.roots(|roots| roots.to_vec()).map_err(|e| e.to_string())?;
    if recorded != roots {
        return Err(format!("the graph's roots are {recorded:?}, and the program called {roots:?}"));
    }
    for (call, inputs) in &model {
        let recorded = graph
            .inputs_of(call.point, &call.key, |inputs| inputs.to_vec())
            .map_err(|e| e.to_string())?;
        if recorded.as_ref() != Some(inputs) {
            return Err(format!(
                "the graph says {call:?} called {recorded:?}, and its body called {inputs:?}"
            ));
        }
    }
    Ok(())
}

/// The invariant: the graph `Provenance` writes holds exactly the calls made, each under the
/// call whose body made it, in order.
#[test]
fn provenance_records_the_calls_made() {
    blaze_sim::check(Config::cases(200), |tape| check(traced(tape), run_traced));
}

#[test]
fn the_oracle_catches_calls_put_under_the_grandparent() {
    control(Plant::NestedUnderGrandparent, traced, run_traced);
}

// Handlers.

/// Offers a value of another type first, which is refused, then runs the body.
struct Wrong;

impl Handler for Wrong {
    fn run(&self, _: &Cx, at: At, next: &mut Next<'_>) {
        let refused = next.supply(&"not a u64").expect_err("a `&str` is no `u64`");
        assert_eq!(refused.point, at.point);
        assert!(next.value().is_none(), "a refused value is not the call's");
        next.compute();
        assert!(next.ran());
        assert_eq!(next.value().and_then(|it| it.downcast_ref::<u64>()), Some(&mix_of(2, 5)));
    }
}

#[test]
fn a_handler_cannot_supply_a_value_of_another_type() {
    let io = world(0, Plant::None);
    let cx = root(&io);
    assert_eq!(Eval::handler(&Wrong).call(&cx, mix, (2, 5)), mix_of(2, 5));
}

// Keys.

#[test]
fn equal_values_give_equal_keys_byte_for_byte() {
    let text = String::from("layout");
    let bytes = vec![1u8, 2, 3];
    let once = Key::of(&(7u32, text.as_str(), bytes.as_slice(), Some('x'), true));
    // The same values at other addresses.
    let (text, bytes) = (text.clone(), bytes.clone());
    let again = Key::of(&(7u32, text.as_str(), bytes.as_slice(), Some('x'), true));
    assert_eq!(once.id().0, again.id().0);
    // A reference is what it refers to.
    assert_eq!(Key::of(&(&7u32, &&text.as_str())).id().0, Key::of(&(7u32, text.as_str())).id().0);
    // A byte slice is written as one run, which is its elements one by one.
    let one_by_one = Key::of(&[Some(1u8), Some(2), Some(3)][..]);
    assert_ne!(Key::of(bytes.as_slice()), one_by_one);
    assert_eq!(Key::of(&ContentId([9; 32])), Key::of(&[9u8; 32]));
}

#[test]
fn values_that_differ_give_keys_that_differ() {
    let keys = [
        // One type, values that differ.
        Key::of(&(1u16, 2u16)),
        Key::of(&(2u16, 1u16)),
        Key::of(&(1u16, 3u16)),
        // Parts of any length never run together.
        Key::of(&("ab", "c")),
        Key::of(&("a", "bc")),
        Key::of(&("abc", "")),
        Key::of(&(&[1u8, 2][..], &[3u8][..])),
        Key::of(&(&[1u8][..], &[2u8, 3][..])),
        // An option's none is not its some of nothing.
        Key::of(&(None::<u8>, 0u8)),
        Key::of(&(Some(0u8),)),
        Key::of(&(Some(0u8), 0u8)),
        Key::of(&(false, 'a')),
        Key::of(&(true, 'a')),
        Key::of(&(-1i64,)),
        Key::of(&(1i64,)),
        // A `usize` is written as a `u64`, at one width on every target.
        Key::of(&(7usize,)),
        Key::of(&()),
    ];
    for (at, key) in keys.iter().enumerate() {
        for (other_at, other) in keys.iter().enumerate().skip(at + 1) {
            assert_ne!(key, other, "values {at} and {other_at} share a key");
        }
    }
}
