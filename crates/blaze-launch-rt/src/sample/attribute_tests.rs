//! Attribution's tests (D100 point 4). PURE: entries are made from a model of placed functions and
//! their frames, never taken from a thread.
//!
//! The property: for entries in a function's prologue, its body and at its `ret`, the profile's
//! rows count each leaf and its edges name the true caller, whatever order the entries come in;
//! the summary accounts for every entry. The control: the link register taken as the caller
//! everywhere (`Plant::LrAlways`) names the function itself in a body, and the oracle says so.

use std::collections::BTreeMap;
use std::format;
use std::string::String;
use std::vec::Vec as StdVec;

use blaze_code::record::Symbol;
use blaze_own::{AVec, Alloc, Vec};
use blaze_sim::{Broke, Config, Tape};
use blaze_tier::sample::SampleEntry;

use super::attribute::{AttributeError, FirstCalls, Placed, Plant, Range, Summary, attribute_with, is_ret};

const NOP: u32 = 0xd503_201f;
const RET: u32 = 0xd65f_03c0;

fn symbol(n: u64) -> Symbol {
    let mut out = [0u8; 32];
    out[..8].copy_from_slice(&n.to_le_bytes());
    Symbol(out)
}

/// A return address into `range`: the word after a `bl` in it, so up to one past its end.
fn return_into(tape: &Tape, range: &Range, label: &'static str) -> u64 {
    let words = (range.high - range.low) / 4;
    range.low + 4 * (1 + tape.below(words, label))
}

/// What the model expects, beside the entries.
#[derive(Default)]
struct Expected {
    rows: BTreeMap<Symbol, u32>,
    edges: BTreeMap<(Symbol, Symbol), u32>,
    summary: Summary,
}

fn attribute_case(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let functions = tape.range(2..9, "functions");
    let mut ranges: StdVec<Range> = StdVec::new();
    for at in 0..functions {
        let low = 0x10_0000 + at * 0x1000;
        let len = 16 + 4 * tape.below(64, "length");
        ranges.push(Range { low, high: low + len, symbol: symbol(at) });
    }
    let mut given: AVec<'_, Range> = Vec::new_in(Alloc::global());
    // Given in an order the tape chooses: `from_ranges` sorts.
    let mut left = ranges.clone();
    while !left.is_empty() {
        given.push(left.remove(tape.index(left.len(), "range order")));
    }
    let placed = Placed::from_ranges(given).map_err(|error| Broke::Harness(format!("{error}")))?;
    let mut entries: StdVec<SampleEntry> = StdVec::new();
    let mut expected = Expected::default();
    for _ in 0..tape.range(1..40, "entries") {
        if tape.one_in(10, "outside placed code") {
            entries.push(SampleEntry { pc: 0x8000, lr: 0, fp: 0, frame_ret: 0, word: 0, thread: 0 });
            expected.summary.outside += 1;
            continue;
        }
        let leaf = ranges[tape.index(ranges.len(), "leaf")];
        let mut caller = ranges[tape.index(ranges.len(), "caller")];
        if caller.symbol == leaf.symbol {
            // No recursion in the model: the planted bug's wrong edge would be the right one.
            caller = ranges[(ranges.iter().position(|it| it.symbol == leaf.symbol).unwrap_or(0) + 1) % ranges.len()];
        }
        let into_caller = return_into(tape, &caller, "return into the caller");
        // The caller's caller: placed code, outside placed code, or none read.
        let grand = match tape.below(3, "grand caller") {
            0 => return_into(tape, &ranges[tape.index(ranges.len(), "grand")], "return into the grand caller"),
            1 => 0x9000,
            _ => 0,
        };
        let words = (leaf.high - leaf.low) / 4;
        let (pc, lr, frame_ret, word) = match tape.below(3, "where") {
            // The prologue's first two words: x29 is still the caller's, its record the grand
            // caller's return; the link register is the caller's return.
            0 => (leaf.low + 4 * tape.below(2, "prologue word"), into_caller, grand, NOP),
            // The body: the link register is the return of the leaf's own last call, into the
            // leaf; the frame record holds the caller's return, or the frame check refused.
            1 => {
                let pc = leaf.low + 4 * tape.range(2..words, "body word");
                let own = return_into(tape, &leaf, "the leaf's own last call");
                let frame_ret = if tape.one_in(8, "frame check refused") { 0 } else { into_caller };
                (pc, own, frame_ret, NOP)
            }
            // At a `ret`: the epilogue loaded the caller's x29; the link register is the return.
            _ => (leaf.low + 4 * tape.range(2..words, "ret word"), into_caller, grand, RET),
        };
        entries.push(SampleEntry { pc, lr, fp: 0x7000_0000, frame_ret, word, thread: 1 });
        expected.summary.attributed += 1;
        *expected.rows.entry(leaf.symbol).or_insert(0) += 1;
        let at_lr = pc - leaf.low < 8 || word == RET;
        let ret = if at_lr { lr } else { frame_ret };
        if ret == 0 {
            expected.summary.no_caller += 1;
        } else {
            expected.summary.edges += 1;
            *expected.edges.entry((caller.symbol, leaf.symbol)).or_insert(0) += 1;
        }
    }
    let (profile, summary) =
        attribute_with(&entries, &placed, Alloc::global(), plant).map_err(|error| Broke::Harness(format!("{error}")))?;
    let rows: BTreeMap<Symbol, u32> = profile.rows.iter().map(|row| (row.symbol, row.samples)).collect();
    if rows != expected.rows {
        return Err(Broke::Oracle(format!("rows {rows:?}, expected {:?}", expected.rows)));
    }
    let edges: BTreeMap<(Symbol, Symbol), u32> =
        profile.edges.iter().map(|edge| ((edge.caller, edge.callee), edge.samples)).collect();
    if edges != expected.edges {
        return Err(Broke::Oracle(format!("a wrong caller: edges {edges:?}, expected {:?}", expected.edges)));
    }
    if summary != expected.summary {
        return Err(Broke::Oracle(format!("summary {summary:?}, expected {:?}", expected.summary)));
    }
    if !profile.rows.windows(2).all(|pair| pair[0].symbol < pair[1].symbol)
        || !profile.edges.windows(2).all(|pair| (pair[0].caller, pair[0].callee) < (pair[1].caller, pair[1].callee))
    {
        return Err(Broke::Oracle(String::from("rows or edges not sorted and unique")));
    }
    // The same entries in another order give the same record.
    entries.reverse();
    let (again, _) =
        attribute_with(&entries, &placed, Alloc::global(), plant).map_err(|error| Broke::Harness(format!("{error}")))?;
    if again.rows[..] != profile.rows[..] || again.edges[..] != profile.edges[..] {
        return Err(Broke::Oracle(String::from("the entries' order changed the profile")));
    }
    Ok(())
}

/// Prints `test sample::attribute_tests::the_caller_comes_from_the_frame_record_except_at_entry_and_ret ... ok`.
#[test]
fn the_caller_comes_from_the_frame_record_except_at_entry_and_ret() {
    blaze_sim::check(Config::cases(1024), |tape: &Tape| attribute_case(tape, Plant::None));
}

/// The control (rule 53): the link register as the caller everywhere names the leaf itself in a
/// body. Prints `test sample::attribute_tests::the_attribution_control_catches_lr_as_the_caller ... ok`
/// and, on standard error, "plant LrAlways: caught at case k of 1024".
#[test]
fn the_attribution_control_catches_lr_as_the_caller() {
    blaze_sim::control(Config::cases(1024), Plant::None, Plant::LrAlways, "a wrong caller", attribute_case);
}

/// Overlapping or empty ranges are refused by name, never attributed to either.
/// Prints `test sample::attribute_tests::overlapping_ranges_are_refused ... ok`.
#[test]
fn overlapping_ranges_are_refused() {
    let mut ranges: AVec<'_, Range> = Vec::new_in(Alloc::global());
    ranges.push(Range { low: 0x2000, high: 0x2100, symbol: symbol(2) });
    ranges.push(Range { low: 0x1000, high: 0x2004, symbol: symbol(1) });
    let refused = Placed::from_ranges(ranges).err();
    assert_eq!(refused, Some(AttributeError::Overlap { first: symbol(1), second: symbol(2), at: 0x2000 }));
    let mut empty: AVec<'_, Range> = Vec::new_in(Alloc::global());
    empty.push(Range { low: 0x1000, high: 0x1000, symbol: symbol(3) });
    assert!(matches!(Placed::from_ranges(empty), Err(AttributeError::EmptyRange { .. })));
}

/// `ret`, `ret x3`, `retaa` and `retab` are returns; a `nop` and a `br x30` are not.
/// Prints `test sample::attribute_tests::the_ret_words ... ok`.
#[test]
fn the_ret_words() {
    assert!(is_ret(0xd65f_03c0));
    assert!(is_ret(0xd65f_0060));
    assert!(is_ret(0xd65f_0bff));
    assert!(is_ret(0xd65f_0fff));
    assert!(!is_ret(NOP));
    assert!(!is_ret(0xd61f_03c0));
}

/// The startup record keeps first calls in order, once each, and nothing after the close.
/// Prints `test sample::attribute_tests::startup_is_first_calls_in_order ... ok`.
#[test]
fn startup_is_first_calls_in_order() {
    let mut calls = FirstCalls::new_in(Alloc::global());
    for n in [5, 3, 5, 9, 3, 1] {
        calls.called(symbol(n));
    }
    calls.close();
    calls.called(symbol(7));
    let startup = calls.record();
    assert_eq!(startup.order[..], [symbol(5), symbol(3), symbol(9), symbol(1)]);
}
