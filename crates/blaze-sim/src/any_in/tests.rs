//! `AnyIn` takes the choices `Any` takes: one tape gives one value either way, so a drawn
//! parameter of `#[bstd::test]` shrinks and replays as `blaze_sim::check`'s values do.

use std::alloc::Global;

use blaze_rng::SplitMix64;

use crate::any_in::AnyIn;
use crate::draw::Any;
use crate::tape::{Record, Tail, Tape};

/// Over 300 seeded tapes, a `Vec<Vec<u32>>`, an `Option<(u8, bool)>` and a `[i16; 3]` drawn
/// with `AnyIn<Global>` equal the ones drawn with `Any`, and the two records' choices are equal.
/// Prints `any_in: 300 tapes, equal values and choices`.
#[test]
fn any_in_takes_the_choices_any_takes() {
    let tape = Tape::new(&[], Tail::Calm, Tape::MOST_CHOICES);
    let (mut by_any, mut by_any_in) = (Record::new(), Record::new());
    for seed in 0..300u64 {
        tape.reset(&[], Tail::Seeded(SplitMix64::new(seed)));
        let a: (Vec<Vec<u32>>, Option<(u8, bool)>, [i16; 3]) = Any::any(&tape, "x");
        tape.finish(&mut by_any);
        tape.reset(&[], Tail::Seeded(SplitMix64::new(seed)));
        let b: (Vec<Vec<u32, Global>, Global>, Option<(u8, bool)>, [i16; 3]) =
            AnyIn::any_in(&tape, "x", Global);
        tape.finish(&mut by_any_in);
        assert_eq!(a, b, "seed {seed}: the values differ");
        let values = |record: &Record| record.choices.iter().map(|choice| choice.value).collect::<Vec<u64>>();
        assert_eq!(values(&by_any), values(&by_any_in), "seed {seed}: the choices differ");
    }
    println!("any_in: 300 tapes, equal values and choices");
}

/// A text drawn in an allocator holds what its choices say: replayed from the record of a first
/// draw, it is the same text, and the calm tape gives the empty text. Prints
/// `any_in: text replays from its record`.
#[test]
fn a_text_replays_from_its_record() {
    let tape = Tape::new(&[], Tail::Calm, Tape::MOST_CHOICES);
    let mut record = Record::new();
    let mut values = Vec::new();
    for seed in 0..100u64 {
        tape.reset(&[], Tail::Seeded(SplitMix64::new(seed)));
        let first: blaze_own::String<Global> = AnyIn::any_in(&tape, "s", Global);
        tape.finish(&mut record);
        record.values_into(&mut values);
        tape.reset(&values, Tail::Calm);
        let again: blaze_own::String<Global> = AnyIn::any_in(&tape, "s", Global);
        assert_eq!(first.as_str(), again.as_str(), "seed {seed}");
    }
    tape.reset(&[], Tail::Calm);
    let calm: blaze_own::String<Global> = AnyIn::any_in(&tape, "s", Global);
    assert_eq!(calm.as_str(), "");
    println!("any_in: text replays from its record");
}
