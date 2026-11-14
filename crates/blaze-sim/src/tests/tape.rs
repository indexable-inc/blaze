#![expect(
    blaze::error_debug,
    reason = "a test's failure message shows the values it compared as they are"
)]

use std::collections::BTreeSet;
use std::panic::{AssertUnwindSafe, catch_unwind};

use blaze_rng::SplitMix64;

use crate::tape::{Choice, Int, Note, Plant, Record, Span, Tail, Tape};
use crate::{Broke, Config};

/// One call of a method `Tape` and `SplitMix64` share, with its arguments.
#[derive(Clone, Copy, Debug)]
enum Call {
    NextU64,
    NextU32,
    Below(u64),
    Index(usize),
    Range(u64, u64),
    OneIn(u64),
    Chance(f64),
    Coin,
    Unit,
    Pick(usize),
    Fill(usize),
    Shuffle(usize),
}

/// What a call gave, comparable across the two.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Out {
    Number(u64),
    Flag(bool),
    /// An `f64`'s bits.
    Unit(u64),
    Picked(Option<usize>),
    Bytes(Vec<u8>),
    Order(Vec<usize>),
}

/// The probabilities `chance` is called with: the two that draw nothing, beyond them, and some
/// between.
const CHANCES: [f64; 7] = [0.0, 1.0, 0.25, 0.5, 0.999, -1.0, 2.0];

/// A bound for `below`, `index`, `range` and `one_in`: 0 and 1 often, small, or any.
fn bound(tape: &Tape) -> u64 {
    match tape.below(4, "bound kind") {
        0 => 0,
        1 => 1,
        2 => tape.below(10, "small bound"),
        _ => tape.next_u64("any bound"),
    }
}

fn size(tape: &Tape) -> usize {
    tape.index(20, "size")
}

fn call(tape: &Tape) -> Call {
    match tape.below(12, "call") {
        0 => Call::NextU64,
        1 => Call::NextU32,
        2 => Call::Below(bound(tape)),
        3 => Call::Index(usize::try_from(bound(tape)).expect(
            "a bound is any `u64`, which is a `usize` on the 64-bit targets blaze's tests run on",
        )),
        4 => {
            let start = bound(tape);
            Call::Range(start, bound(tape))
        }
        5 => Call::OneIn(bound(tape)),
        6 => Call::Chance(*tape.pick(&CHANCES, "p").expect("CHANCES is not empty")),
        7 => Call::Coin,
        8 => Call::Unit,
        9 => Call::Pick(size(tape)),
        10 => Call::Fill(size(tape)),
        _ => Call::Shuffle(size(tape)),
    }
}

fn on_tape(call: Call, tape: &Tape) -> Out {
    match call {
        Call::NextU64 => Out::Number(tape.next_u64("next_u64")),
        Call::NextU32 => Out::Number(u64::from(tape.next_u32("next_u32"))),
        Call::Below(n) => Out::Number(tape.below(n, "below")),
        #[expect(blaze::as_cast, reason = "an index below a `usize` length, widened to compare")]
        Call::Index(len) => Out::Number(tape.index(len, "index") as u64),
        Call::Range(start, end) => Out::Number(tape.range(start..end, "range")),
        Call::OneIn(n) => Out::Flag(tape.one_in(n, "one_in")),
        Call::Chance(p) => Out::Flag(tape.chance(p, "chance")),
        Call::Coin => Out::Flag(tape.coin("coin")),
        Call::Unit => Out::Unit(tape.unit_f64("unit_f64").to_bits()),
        Call::Pick(len) => {
            let items: Vec<usize> = (0..len).collect();
            Out::Picked(tape.pick(&items, "pick").copied())
        }
        Call::Fill(len) => {
            let mut out = vec![0u8; len];
            tape.fill(&mut out, "fill");
            Out::Bytes(out)
        }
        Call::Shuffle(len) => {
            let mut items: Vec<usize> = (0..len).collect();
            tape.shuffle(&mut items, "shuffle");
            Out::Order(items)
        }
    }
}

fn on_rng(call: Call, rng: &mut SplitMix64) -> Out {
    match call {
        Call::NextU64 => Out::Number(rng.next_u64()),
        Call::NextU32 => Out::Number(u64::from(rng.next_u32())),
        Call::Below(n) => Out::Number(rng.below(n)),
        #[expect(blaze::as_cast, reason = "an index below a `usize` length, widened to compare")]
        Call::Index(len) => Out::Number(rng.index(len) as u64),
        Call::Range(start, end) => Out::Number(rng.range(start..end)),
        Call::OneIn(n) => Out::Flag(rng.one_in(n)),
        Call::Chance(p) => Out::Flag(rng.chance(p)),
        Call::Coin => Out::Flag(rng.coin()),
        Call::Unit => Out::Unit(rng.unit_f64().to_bits()),
        Call::Pick(len) => {
            let items: Vec<usize> = (0..len).collect();
            Out::Picked(rng.pick(&items).copied())
        }
        Call::Fill(len) => {
            let mut out = vec![0u8; len];
            rng.fill(&mut out);
            Out::Bytes(out)
        }
        Call::Shuffle(len) => {
            let mut items: Vec<usize> = (0..len).collect();
            rng.shuffle(&mut items);
            Out::Order(items)
        }
    }
}

/// A recorded tape on the seed's stream, with `plant` planted.
fn recorded(seed: u64, plant: Plant) -> Tape {
    let tail = Tail::Seeded(SplitMix64::new(seed));
    match plant {
        Plant::None => Tape::new(&[], tail, Tape::MOST_CHOICES),
        plant @ (Plant::OneInDrawsAtZero
        | Plant::ShuffleFromFront
        | Plant::ReplayFlipsFlags
        | Plant::ShuffleSwapsWithValue) => Tape::planted(&[], tail, Tape::MOST_CHOICES, plant),
    }
}

fn refines(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let seed = tape.next_u64("seed");
    let calls = tape.vec(0..=40, "calls", call);
    let subjects =
        [("Tape::seeded", Tape::seeded(seed)), ("a recorded Tape::new", recorded(seed, plant))];
    for (name, subject) in &subjects {
        let mut rng = SplitMix64::new(seed);
        for (at, &call) in calls.iter().enumerate() {
            let (got, want) = (on_tape(call, subject), on_rng(call, &mut rng));
            if got != want {
                return Err(Broke::Oracle(format!(
                    "{name} of seed {seed}, call {at} ({call:?}): the tape gave {got:?}, SplitMix64 {want:?}"
                )));
            }
        }
        let (got, want) = (subject.next_u64("after"), rng.next_u64());
        if got != want {
            return Err(Broke::Oracle(format!(
                "{name} of seed {seed}, after {} calls: the tape gave {got}, SplitMix64 {want}",
                calls.len()
            )));
        }
    }
    Ok(())
}

/// The refinement: a seeded tape, recorded or not, gives what `SplitMix64` of its seed gives on
/// any sequence of calls, and leaves the stream where `SplitMix64` leaves it.
#[test]
fn a_seeded_tape_is_splitmix64() {
    crate::check(Config::cases(200), |tape| refines(tape, Plant::None));
}

#[test]
fn the_oracle_catches_a_one_in_that_draws_at_zero() {
    crate::control(
        Config::cases(200),
        Plant::None,
        Plant::OneInDrawsAtZero,
        "the tape gave",
        refines,
    );
}

#[test]
fn the_oracle_catches_a_shuffle_that_walks_from_the_front() {
    crate::control(
        Config::cases(200),
        Plant::None,
        Plant::ShuffleFromFront,
        "the tape gave",
        refines,
    );
}

fn replays(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let seed = tape.next_u64("seed");
    let calls = tape.vec(0..=40, "calls", call);
    let first = Tape::new(&[], Tail::Seeded(SplitMix64::new(seed)), Tape::MOST_CHOICES);
    let firsts: Vec<Out> = calls.iter().map(|&call| on_tape(call, &first)).collect();
    let mut was = Record::new();
    first.finish(&mut was);
    let values: Vec<u64> = was.choices.iter().map(|choice| choice.value).collect();
    let again = match plant {
        Plant::None => Tape::replay(&values),
        plant @ (Plant::OneInDrawsAtZero
        | Plant::ShuffleFromFront
        | Plant::ReplayFlipsFlags
        | Plant::ShuffleSwapsWithValue) => {
            Tape::planted(&values, Tail::Calm, Tape::MOST_CHOICES, plant)
        }
    };
    let agains: Vec<Out> = calls.iter().map(|&call| on_tape(call, &again)).collect();
    let mut is = Record::new();
    again.finish(&mut is);
    if let Some(at) = (0..calls.len()).find(|&at| firsts[at] != agains[at]) {
        return Err(Broke::Oracle(format!(
            "seed {seed}, call {at} ({:?}): the replay gave {:?}, the case {:?}",
            calls[at], agains[at], firsts[at]
        )));
    }
    if is.choices != was.choices {
        return Err(Broke::Oracle(format!(
            "seed {seed}: the replay recorded {:?}, the case {:?}",
            is.choices, was.choices
        )));
    }
    if is.clamped != 0 || is.unused != 0 {
        return Err(Broke::Oracle(format!(
            "seed {seed}: the replay lowered {} values and left {} unused",
            is.clamped, is.unused
        )));
    }
    Ok(())
}

/// The invariant: a recorded tape's values, replayed, give the same results and an equal list
/// of choices, with nothing lowered and nothing left over.
#[test]
fn a_recorded_tape_replays_to_itself() {
    crate::check(Config::cases(200), |tape| replays(tape, Plant::None));
}

#[test]
fn the_oracle_catches_a_replay_that_flips_flags() {
    crate::control(Config::cases(200), Plant::None, Plant::ReplayFlipsFlags, "the replay", replays);
}

/// A draw `Tape::calm` is asked for beyond the shared ones, with its arguments.
#[derive(Clone, Copy, Debug)]
enum Quiet {
    Call(Call),
    More(u64),
    List(usize, usize),
    IntU8,
    IntU64,
    IntUsize,
    IntI8,
    IntI64,
    IntIsize,
    IntInU64(u64, u64),
    IntInI32(i32, i32),
    IntInI8(i8, i8),
}

fn quiet(tape: &Tape) -> Quiet {
    match tape.below(12, "method") {
        0 => Quiet::Call(call(tape)),
        1 => Quiet::More(bound(tape)),
        2 => {
            let min = tape.index(6, "min");
            Quiet::List(min, min + tape.index(6, "more"))
        }
        3 => Quiet::IntU8,
        4 => Quiet::IntU64,
        5 => Quiet::IntUsize,
        6 => Quiet::IntI8,
        7 => Quiet::IntI64,
        8 => Quiet::IntIsize,
        9 => {
            let (a, b) = (tape.int::<u64>("a"), tape.int::<u64>("b"));
            Quiet::IntInU64(a.min(b), a.max(b))
        }
        10 => {
            let (a, b) = (tape.int::<i32>("a"), tape.int::<i32>("b"));
            Quiet::IntInI32(a.min(b), a.max(b))
        }
        _ => {
            let (a, b) = (tape.int::<i8>("a"), tape.int::<i8>("b"));
            Quiet::IntInI8(a.min(b), a.max(b))
        }
    }
}

/// What a call gives on the calm tape.
fn calm_out(call: Call) -> Out {
    match call {
        Call::NextU64 | Call::NextU32 | Call::Below(_) | Call::Index(_) => Out::Number(0),
        Call::Range(start, _) => Out::Number(start),
        Call::OneIn(_) | Call::Coin => Out::Flag(false),
        Call::Chance(p) => Out::Flag(p >= 1.0),
        Call::Unit => Out::Unit(0.0f64.to_bits()),
        Call::Pick(len) => Out::Picked(if len == 0 { None } else { Some(0) }),
        Call::Fill(len) => Out::Bytes(vec![0; len]),
        Call::Shuffle(len) => Out::Order((0..len).collect()),
    }
}

/// `None` when `quiet` answers calm on `subject`, else what it gave.
fn not_calm(quiet: Quiet, subject: &Tape) -> Option<String> {
    let calm = match quiet {
        Quiet::Call(
            call @ (Call::NextU64
            | Call::NextU32
            | Call::Below(..)
            | Call::Index(..)
            | Call::Range(..)
            | Call::OneIn(..)
            | Call::Chance(..)
            | Call::Coin
            | Call::Unit
            | Call::Pick(..)
            | Call::Fill(..)
            | Call::Shuffle(..)),
        ) => {
            let got = on_tape(call, subject);
            return (got != calm_out(call)).then(|| format!("{got:?}"));
        }
        Quiet::More(k) => !subject.more(k, "more"),
        Quiet::List(min, max) => {
            let list = subject.vec(min..=max, "list", |t| t.below(5, "element"));
            return (list.len() != min || list.iter().any(|&x| x != 0))
                .then(|| format!("{list:?}"));
        }
        Quiet::IntU8 => subject.int::<u8>("int") == 0,
        Quiet::IntU64 => subject.int::<u64>("int") == 0,
        Quiet::IntUsize => subject.int::<usize>("int") == 0,
        Quiet::IntI8 => subject.int::<i8>("int") == 0,
        Quiet::IntI64 => subject.int::<i64>("int") == 0,
        Quiet::IntIsize => subject.int::<isize>("int") == 0,
        Quiet::IntInU64(lo, hi) => subject.int_in(lo..=hi, "int_in") == lo,
        Quiet::IntInI32(lo, hi) => subject.int_in(lo..=hi, "int_in") == lo,
        Quiet::IntInI8(lo, hi) => subject.int_in(lo..=hi, "int_in") == lo,
    };
    (!calm).then(|| "not its calm value".to_owned())
}

fn is_calm(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let subject = match plant {
        Plant::None => Tape::calm(),
        plant @ (Plant::OneInDrawsAtZero
        | Plant::ShuffleFromFront
        | Plant::ReplayFlipsFlags
        | Plant::ShuffleSwapsWithValue) => {
            Tape::planted(&[], Tail::Calm, Tape::MOST_CHOICES, plant)
        }
    };
    let steps = tape.vec(1..=30, "steps", quiet);
    for (at, &step) in steps.iter().enumerate() {
        if let Some(got) = not_calm(step, &subject) {
            return Err(Broke::Oracle(format!("step {at}, {step:?}: the calm tape gave {got}")));
        }
    }
    Ok(())
}

/// The invariant: on the calm tape every method gives its calm answer: 0, the start, false, the
/// first item, zeros, the order left alone, a list of the least size, an integer of 0.
#[test]
fn the_calm_tape_is_calm_everywhere() {
    crate::check(Config::cases(200), |tape| is_calm(tape, Plant::None));
}

#[test]
fn the_oracle_catches_a_shuffle_that_swaps_with_its_value() {
    crate::control(
        Config::cases(200),
        Plant::None,
        Plant::ShuffleSwapsWithValue,
        "the calm tape gave",
        is_calm,
    );
}

fn finished(tape: &Tape) -> Record {
    let mut record = Record::new();
    tape.finish(&mut record);
    record
}

#[test]
fn a_prefix_value_above_most_is_lowered_and_counted() {
    let tape = Tape::new(&[7, 1, 300, u64::MAX, 0, 0], Tail::Calm, Tape::MOST_CHOICES);
    assert_eq!(tape.below(3, "below"), 2);
    assert!(tape.coin("coin"));
    assert!(tape.one_in(4, "one_in"));
    assert_eq!(tape.next_u32("next_u32"), u32::MAX);
    assert_eq!(tape.next_u64("zero"), 0);
    assert_eq!(tape.next_u64("zero"), 0);
    let record = finished(&tape);
    assert_eq!(record.clamped, 3);
    assert_eq!(record.unused, 0);
    assert_eq!(record.choices[0], Choice { value: 2, most: 2, label: "below" });
    let mut values = vec![9];
    record.values_into(&mut values);
    assert_eq!(values, [2, 1, 1, u64::from(u32::MAX)], "trailing zeros are cut");
}

#[test]
fn an_unused_prefix_is_counted() {
    let tape = Tape::new(&[1, 2, 3], Tail::Calm, Tape::MOST_CHOICES);
    assert_eq!(tape.below(10, "one"), 1);
    let record = finished(&tape);
    assert_eq!((record.unused, record.clamped), (2, 0));
}

#[test]
fn a_draw_past_the_bound_overruns() {
    let tape = Tape::new(&[], Tail::Calm, 2);
    tape.coin("first");
    tape.coin("second");
    let caught = catch_unwind(AssertUnwindSafe(|| tape.coin("third")));
    let payload = caught.expect_err("the third draw is past the bound");
    let message = payload.downcast_ref::<String>().expect("the overrun's message is formatted");
    assert_eq!(message, "a case drew more than 2 choices (the last: `third`)");
    let record = finished(&tape);
    assert!(record.overran);
    assert_eq!(record.choices.len(), 2);
}

#[test]
fn a_seeded_tape_records_nothing() {
    let tape = Tape::seeded(5);
    tape.span("span", |t| t.next_u64("draw"));
    tape.note(format_args!("a note"));
    tape.reached("a point");
    let record = finished(&tape);
    assert_eq!(record, Record::new());
}

#[test]
fn spans_nest_and_finish_closes_one_left_open() {
    let tape = Tape::calm();
    tape.span("outer", |t| {
        t.coin("a");
        t.span("inner", |t| t.coin("b"));
        t.coin("c");
    });
    let left = catch_unwind(AssertUnwindSafe(|| {
        tape.span("open", |t| {
            t.coin("d");
            panic!("left open");
        });
    }));
    assert!(left.is_err());
    tape.coin("e");
    let record = finished(&tape);
    assert_eq!(
        record.spans,
        [
            Span { label: "outer", start: 0, end: 3 },
            Span { label: "inner", start: 1, end: 2 },
            Span { label: "open", start: 3, end: 5 },
        ]
    );
}

#[test]
fn a_vecs_elements_are_spans_and_its_length_stays_in_sizes() {
    let mut lengths = BTreeSet::new();
    for seed in 0..64 {
        let tape = Tape::new(&[], Tail::Seeded(SplitMix64::new(seed)), Tape::MOST_CHOICES);
        let list = tape.vec(2..=5, "list", |t| t.below(10, "element"));
        assert!((2..=5).contains(&list.len()), "seed {seed}: {} elements", list.len());
        lengths.insert(list.len());
        let record = finished(&tape);
        assert_eq!(record.spans.len(), list.len(), "seed {seed}");
        for span in &record.spans {
            assert_eq!(span.label, "list");
            assert_eq!(span.range().len(), 1, "seed {seed}");
            assert_eq!(record.choices[span.range()][0].label, "element");
        }
    }
    assert!(lengths.contains(&2) && lengths.contains(&5), "lengths {lengths:?}");
    let calm = Tape::calm();
    assert_eq!(calm.vec(2..=5, "list", |t| t.below(10, "element")), [0, 0]);
}

#[test]
fn int_u8_reaches_its_ends_and_its_small_values() {
    let tape = Tape::seeded(1);
    let mut seen = [false; 256];
    for _ in 0..2_000 {
        seen[usize::from(tape.int::<u8>("byte"))] = true;
    }
    assert!(seen[0] && seen[255], "the ends");
    assert!(seen[..8].iter().all(|&seen| seen), "every value below 8: {:?}", &seen[..8]);
}

fn stays_in_and_reaches_both_ends<T: Int + PartialOrd + core::fmt::Debug>(
    range: core::ops::RangeInclusive<T>,
) {
    let tape = Tape::seeded(7);
    let (lo, hi) = (*range.start(), *range.end());
    let (mut low, mut high) = (false, false);
    for _ in 0..20_000 {
        let value = tape.int_in(range.clone(), "in");
        assert!(lo <= value && value <= hi, "{value:?} is outside {lo:?}..={hi:?}");
        low |= value == lo;
        high |= value == hi;
    }
    assert!(low && high, "{lo:?}..={hi:?}: reached the start {low}, the end {high}");
}

#[test]
fn int_in_stays_in_its_range_and_reaches_both_ends() {
    stays_in_and_reaches_both_ends(10u16..=20);
    stays_in_and_reaches_both_ends(-3i8..=3);
    stays_in_and_reaches_both_ends(0u8..=255);
    stays_in_and_reaches_both_ends(i8::MIN..=i8::MAX);
    stays_in_and_reaches_both_ends(-1_000i32..=1_000_000);
    stays_in_and_reaches_both_ends(5usize..=1 << 40);
    stays_in_and_reaches_both_ends(isize::MIN..=0);
    stays_in_and_reaches_both_ends(0u64..=u64::MAX);
    stays_in_and_reaches_both_ends(i64::MIN..=i64::MAX);
}

#[test]
fn reached_keeps_each_point_once_in_order() {
    let tape = Tape::calm();
    tape.reached("b");
    tape.coin("draw");
    tape.note(format_args!("after {} draw", 1));
    tape.reached("a");
    tape.reached("b");
    let record = finished(&tape);
    assert_eq!(record.reached, ["b", "a"]);
    assert_eq!(record.notes, [Note { at: 1, text: "after 1 draw".to_owned() }]);
}

#[test]
fn discard_sets_discarded() {
    let tape = Tape::calm();
    tape.assume(true);
    assert!(!finished(&tape).discarded);
    let caught = catch_unwind(AssertUnwindSafe(|| tape.assume(false)));
    let payload = caught.expect_err("assume(false) discards");
    assert_eq!(payload.downcast_ref::<&str>(), Some(&"discarded"));
    assert!(finished(&tape).discarded);
}

#[test]
fn reset_clears_the_case_and_keeps_the_new_tails_stream() {
    let tape = Tape::new(&[300], Tail::Calm, 2);
    tape.below(3, "clamped");
    tape.note(format_args!("a note"));
    tape.reached("a point");
    let over = catch_unwind(AssertUnwindSafe(|| {
        tape.span("open", |t| {
            t.coin("last");
            t.coin("over");
        });
    }));
    assert!(over.is_err());
    let discarded = catch_unwind(AssertUnwindSafe(|| {
        tape.discard();
    }));
    assert!(discarded.is_err());
    let before = finished(&tape);
    assert!(before.overran && before.discarded && before.clamped == 1);
    assert!(!before.notes.is_empty() && !before.reached.is_empty() && !before.spans.is_empty());

    tape.reset(&[], Tail::Seeded(SplitMix64::new(9)));
    let mut rng = SplitMix64::new(9);
    let (a, b) = (rng.next_u64(), rng.next_u64());
    assert_eq!(tape.next_u64("a"), a);
    assert_eq!(tape.next_u64("b"), b);
    let mut record = before;
    tape.finish(&mut record);
    assert_eq!(
        record,
        Record {
            choices: vec![
                Choice { value: a, most: u64::MAX, label: "a" },
                Choice { value: b, most: u64::MAX, label: "b" },
            ],
            spans: vec![],
            notes: vec![],
            reached: vec![],
            discarded: false,
            overran: false,
            clamped: 0,
            unused: 0,
        }
    );
}
