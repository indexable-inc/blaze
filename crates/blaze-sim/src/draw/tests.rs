//! The draws' own proof: calm everywhere, replay, floats that keep their promises, and each
//! draw's plain behaviour. Each invariant's check also runs on a planted wrong draw defined
//! here, since the draws have no state to plant a bug in.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use blaze_rng::SplitMix64;

use crate::{Config, Record, Tail, Tape, Verdict, check, search};

/// A draw run on a calm tape, named by what it should give, and whether it gave that.
type Calm = (&'static str, fn(&Tape) -> bool);

/// Every draw of this module on the calm tape.
const CALM: &[Calm] = &[
    ("bool is false", |t: &Tape| !t.bool("bool")),
    ("option is None", |t: &Tape| t.option("option", |t| t.int::<u32>("some")).is_none()),
    ("one_of is the first", |t: &Tape| *t.one_of(&[7, 8, 9], "one_of") == 7),
    ("array is zeros", |t: &Tape| t.array::<u32, 4>("array", |t| t.int("element")) == [0; 4]),
    ("bytes from 0 is empty", |t: &Tape| t.bytes(0..=8, "bytes").is_empty()),
    ("bytes from 3 is three zeros", |t: &Tape| t.bytes(3..=8, "bytes") == [0, 0, 0]),
    ("char is a space", |t: &Tape| t.char("char") == ' '),
    ("text from 0 is empty", |t: &Tape| t.text(0..=8, "text").is_empty()),
    ("text from 2 is two spaces", |t: &Tape| t.text(2..=8, "text") == "  "),
    ("text_from from 0 is empty", |t: &Tape| t.text_from("qrs", 0..=3, "text_from").is_empty()),
    ("text_from from 3 is the first letter thrice", |t: &Tape| t.text_from("qrs", 3..=5, "text_from") == "qqq"),
    ("f64 is 0.0", |t: &Tape| t.f64("f64").to_bits() == 0.0f64.to_bits()),
    ("f64_finite is 0.0", |t: &Tape| t.f64_finite("f64_finite").to_bits() == 0.0f64.to_bits()),
    ("f64_in is its start", |t: &Tape| t.f64_in(-2.5..=7.0, "f64_in").to_bits() == (-2.5f64).to_bits()),
    ("f64_in of one value is it", |t: &Tape| t.f64_in(1.0..=1.0, "f64_in") == 1.0),
    ("f32 is 0.0", |t: &Tape| t.f32("f32").to_bits() == 0.0f32.to_bits()),
    ("f32_finite is 0.0", |t: &Tape| t.f32_finite("f32_finite").to_bits() == 0.0f32.to_bits()),
    ("f32_in is its start", |t: &Tape| t.f32_in(-2.5..=7.0, "f32_in").to_bits() == (-2.5f32).to_bits()),
    ("u128 is 0", |t: &Tape| t.u128("u128") == 0),
    ("i128 is 0", |t: &Tape| t.i128("i128") == 0),
    ("weighted is the first with a weight", |t: &Tape| t.weighted(&[0, 0, 4, 1], "weighted") == 2),
    ("weighted of one is it", |t: &Tape| t.weighted(&[9], "weighted") == 0),
    ("btree_map from 0 is empty", |t: &Tape| {
        t.btree_map(0..=5, "map", |t| t.int::<u8>("key"), |t| t.int::<u8>("value")).is_empty()
    }),
    ("btree_map from 1 is 0 to 0", |t: &Tape| {
        t.btree_map(1..=5, "map", |t| t.int::<u8>("key"), |t| t.int::<u8>("value")) == BTreeMap::from([(0, 0)])
    }),
    ("any u64 is 0", |t: &Tape| t.any::<u64>("any") == 0),
];

/// The names of the draws that did not give their simplest value, each on a fresh calm tape.
fn not_calm(draws: &[Calm]) -> Vec<&'static str> {
    draws.iter().filter(|(_, calm)| !calm(&Tape::calm())).map(|(name, _)| *name).collect()
}

/// The invariant: on the calm tape every draw here gives its simplest value: `bool` false,
/// `option` `None`, `one_of` the first item, `array` zeros, `bytes` the least size of zeros,
/// `char` a space, `text` the least size of spaces, `text_from` the least size of the alphabet's
/// first letter, every float 0.0 or its range's start, `u128` and `i128` 0, `weighted` the first
/// alternative with a weight, `btree_map` the least size (keys and values calm).
#[test]
fn every_draw_is_calm_on_the_calm_tape() {
    assert_eq!(not_calm(CALM), Vec::<&str>::new());
}

/// An option that is `Some` on the calm choice: the plant for the calm check.
fn option_some_when_calm(t: &Tape) -> Option<u32> {
    match t.below(4, "option") {
        3 => None,
        _ => Some(t.int("some")),
    }
}

#[test]
fn the_oracle_catches_an_option_that_is_some_when_calm() {
    let planted: &[Calm] = &[("option is None", |t: &Tape| option_some_when_calm(t).is_none())];
    assert_eq!(not_calm(planted), vec!["option is None"]);
}

/// A mix of every draw here, as numbers (floats by their bits), so two runs compare exactly.
fn mix(t: &Tape) -> Vec<u64> {
    let mut out = Vec::new();
    out.push(u64::from(t.bool("bool")));
    out.push(t.option("option", |t| t.int::<u32>("some")).map_or(u64::MAX, u64::from));
    out.push(*t.one_of(&[3u64, 5, 7, 11], "one_of"));
    out.extend(t.array::<u16, 3>("array", |t| t.int("element")).map(u64::from));
    out.extend(t.bytes(0..=12, "bytes").into_iter().map(u64::from));
    out.push(u64::from(t.char("char")));
    out.extend(t.text(0..=6, "text").chars().map(u64::from));
    out.extend(t.text_from("abc", 1..=4, "text_from").chars().map(u64::from));
    out.push(t.f64("f64").to_bits());
    out.push(t.f64_finite("f64_finite").to_bits());
    out.push(t.f64_in(-1.0..=1e6, "f64_in").to_bits());
    out.push(u64::from(t.f32("f32").to_bits()));
    out.push(u64::from(t.f32_finite("f32_finite").to_bits()));
    out.push(u64::from(t.f32_in(0.5..=2.0, "f32_in").to_bits()));
    let unsigned = t.u128("u128");
    out.extend([(unsigned >> 64) as u64, unsigned as u64]);
    let signed = t.i128("i128") as u128;
    out.extend([(signed >> 64) as u64, signed as u64]);
    out.push(t.weighted(&[3, 0, 1, 7], "weighted") as u64);
    let map = t.btree_map(0..=5, "map", |t| t.int::<u16>("key"), |t| t.int::<i8>("value"));
    for (key, value) in map {
        out.extend([u64::from(key), value as u64]);
    }
    let any = t.any::<(u8, Option<Vec<i16>>, String, [bool; 2], f32)>("any");
    out.extend(format!("{any:?}").bytes().map(u64::from));
    out
}

/// Runs `mix` on a recorded seeded tape, then on the replay of what it recorded: an error says
/// what differed.
fn replays(seed: u64, mix: fn(&Tape) -> Vec<u64>) -> Result<(), String> {
    let seeded = Tape::new(&[], Tail::Seeded(SplitMix64::new(seed)), Tape::MOST_CHOICES);
    let first = mix(&seeded);
    let mut recorded = Record::new();
    seeded.finish(&mut recorded);
    let mut values = Vec::new();
    recorded.values_into(&mut values);

    let replay = Tape::replay(&values);
    let again = mix(&replay);
    let mut replayed = Record::new();
    replay.finish(&mut replayed);

    if first != again {
        return Err(format!("seed {seed}: drew {first:?}, replayed {again:?}"));
    }
    if replayed.clamped != 0 || replayed.unused != 0 {
        return Err(format!("seed {seed}: the replay clamped {} and left {} unused", replayed.clamped, replayed.unused));
    }
    if replayed.choices != recorded.choices {
        return Err(format!("seed {seed}: the replay recorded other choices"));
    }
    Ok(())
}

/// The invariant: every draw replays: a mix of them on a recorded seeded tape, run again on
/// `Tape::replay` of its record, gives equal results (floats by bits) and the same choices, with
/// nothing clamped and nothing unused.
#[test]
fn every_draw_replays() {
    for seed in 0..200 {
        replays(seed, mix).unwrap();
    }
}

/// A count outside any tape, which the planted mix reads.
static OUTSIDE: AtomicU64 = AtomicU64::new(0);

/// `mix` plus a value read from outside the tape: the plant for the replay check.
fn mix_reading_outside(t: &Tape) -> Vec<u64> {
    let mut out = mix(t);
    out.push(OUTSIDE.fetch_add(1, Ordering::Relaxed));
    out
}

#[test]
fn the_oracle_catches_a_draw_that_reads_outside_its_tape() {
    let caught = replays(7, mix_reading_outside).unwrap_err();
    assert!(caught.contains("replayed"), "{caught}");
}

/// Whether `x` is finite and, given a range, inside it.
fn finite_and_in(what: &str, x: f64, range: Option<(f64, f64)>) -> Result<(), String> {
    if !x.is_finite() {
        return Err(format!("{what} gave {x:?}, which is not finite"));
    }
    match range {
        Some((lo, hi)) if !(lo <= x && x <= hi) => Err(format!("{what} gave {x:?}, outside {lo:?}..={hi:?}")),
        _ => Ok(()),
    }
}

/// 2,000 draws of `draw` from one seed stay in `lo..=hi` and give both ends.
fn reaches_both_ends(lo: f64, hi: f64, draw: impl Fn(&Tape) -> f64) -> Result<(), String> {
    let tape = Tape::seeded(5);
    let (mut low, mut high) = (false, false);
    for _ in 0..2_000 {
        let x = draw(&tape);
        finite_and_in("the range draw", x, Some((lo, hi)))?;
        low |= x == lo;
        high |= x == hi;
    }
    match (low, high) {
        (true, true) => Ok(()),
        _ => Err(format!("in 2,000 draws over {lo:?}..={hi:?}: start reached {low}, end reached {high}")),
    }
}

/// The invariant: `f64_finite` and `f32_finite` are finite, and `f64_in` and `f32_in` stay in
/// their range (any finite one) and reach both ends within 2,000 seeded draws.
#[test]
fn floats_stay_finite_and_in_range() {
    check(Config::cases(500), |t| -> Result<(), String> {
        finite_and_in("f64_finite", t.f64_finite("x"), None)?;
        finite_and_in("f32_finite", f64::from(t.f32_finite("x")), None)?;
        let (a, b) = (t.f64_finite("a"), t.f64_finite("b"));
        let (lo, hi) = (a.min(b), a.max(b));
        finite_and_in("f64_in", t.f64_in(lo..=hi, "in"), Some((lo, hi)))?;
        let (a, b) = (t.f32_finite("a"), t.f32_finite("b"));
        let (lo, hi) = (a.min(b), a.max(b));
        finite_and_in("f32_in", f64::from(t.f32_in(lo..=hi, "in")), Some((f64::from(lo), f64::from(hi))))?;
        Ok(())
    });
    reaches_both_ends(-1.5, 3.25, |t| t.f64_in(-1.5..=3.25, "in")).unwrap();
    reaches_both_ends(-1.5, 3.25, |t| f64::from(t.f32_in(-1.5..=3.25, "in"))).unwrap();
    reaches_both_ends(f64::MIN, f64::MAX, |t| t.f64_in(f64::MIN..=f64::MAX, "in")).unwrap();
}

/// A finite draw that lets a NaN through once in twenty: the plant for the finite check.
fn finite_letting_nan_through(t: &Tape) -> f64 {
    match t.one_in(20, "nan") {
        true => f64::NAN,
        false => t.f64_finite("x"),
    }
}

#[test]
fn the_oracle_catches_a_finite_draw_that_lets_nan_through() {
    let verdict = search(Config::cases(500).shrink_runs(0), |t| -> Result<(), String> {
        finite_and_in("planted", finite_letting_nan_through(t), None)
    });
    assert!(matches!(verdict, Verdict::Failed(_)), "{verdict:?}");
}

#[test]
fn the_oracle_catches_a_range_draw_that_never_reaches_its_end() {
    let caught = reaches_both_ends(-1.5, 3.25, |t| -1.5 + t.unit_f64("unit") * 4.75).unwrap_err();
    assert!(caught.contains("end reached false"), "{caught}");
}

/// Which of the values a float test wants to see one has been.
#[derive(Default, Debug)]
struct Seen {
    nan: bool,
    infinity: bool,
    neg_infinity: bool,
    zero: bool,
    neg_zero: bool,
    subnormal: bool,
}

impl Seen {
    fn all(&self) -> bool {
        self.nan && self.infinity && self.neg_infinity && self.zero && self.neg_zero && self.subnormal
    }
}

#[test]
fn f64_reaches_nan_the_infinities_both_zeros_and_a_subnormal() {
    let tape = Tape::seeded(11);
    let mut seen = Seen::default();
    for _ in 0..5_000 {
        let x = tape.f64("x");
        seen.nan |= x.is_nan();
        seen.infinity |= x == f64::INFINITY;
        seen.neg_infinity |= x == f64::NEG_INFINITY;
        seen.zero |= x.to_bits() == 0.0f64.to_bits();
        seen.neg_zero |= x.to_bits() == (-0.0f64).to_bits();
        seen.subnormal |= x.is_subnormal();
    }
    assert!(seen.all(), "{seen:?}");
}

#[test]
fn f32_reaches_nan_the_infinities_both_zeros_and_a_subnormal() {
    let tape = Tape::seeded(11);
    let mut seen = Seen::default();
    for _ in 0..5_000 {
        let x = tape.f32("x");
        seen.nan |= x.is_nan();
        seen.infinity |= x == f32::INFINITY;
        seen.neg_infinity |= x == f32::NEG_INFINITY;
        seen.zero |= x.to_bits() == 0.0f32.to_bits();
        seen.neg_zero |= x.to_bits() == (-0.0f32).to_bits();
        seen.subnormal |= x.is_subnormal();
    }
    assert!(seen.all(), "{seen:?}");
}

#[test]
#[should_panic(expected = "`range`: f64_in over 1.0..=0.0: the range is empty")]
fn f64_in_refuses_an_empty_range() {
    Tape::calm().f64_in(1.0..=0.0, "range");
}

#[test]
#[should_panic(expected = "`range`: f64_in over 0.0..=inf: an end is NaN or infinite")]
fn f64_in_refuses_an_infinite_end() {
    Tape::calm().f64_in(0.0..=f64::INFINITY, "range");
}

#[test]
#[should_panic(expected = "`range`: f32_in over NaN..=1.0: an end is NaN or infinite")]
fn f32_in_refuses_a_nan_end() {
    Tape::calm().f32_in(f32::NAN..=1.0, "range");
}

#[test]
fn char_never_gives_a_surrogate_and_reaches_past_ascii() {
    let tape = Tape::seeded(3);
    let (mut basic, mut astral) = (false, false);
    for _ in 0..5_000 {
        let code = u32::from(tape.char("c"));
        assert!(!(0xD800..=0xDFFF).contains(&code), "a surrogate {code:#x}");
        basic |= (0x80..0x1_0000).contains(&code);
        astral |= code >= 0x1_0000;
    }
    assert!(basic && astral, "past ASCII in the basic plane {basic}, past it {astral}");
    // The first code point past the surrogates, and the last scalar value, from their counts.
    assert_eq!(Tape::replay(&[5, 0xD800]).char("c"), '\u{E000}');
    assert_eq!(Tape::replay(&[7, u64::MAX]).char("c"), '\u{10FFFF}');
}

#[test]
fn text_from_uses_only_its_alphabet() {
    let tape = Tape::seeded(9);
    for _ in 0..2_000 {
        let text = tape.text_from("αb-", 0..=8, "t");
        assert!(text.chars().count() <= 8, "{text:?}");
        assert!(text.chars().all(|c| "αb-".contains(c)), "{text:?}");
    }
}

#[test]
#[should_panic(expected = "`letters`: text_from with an empty alphabet")]
fn text_from_refuses_an_empty_alphabet() {
    Tape::calm().text_from("", 0..=3, "letters");
}

#[test]
fn one_of_gives_an_item_and_reaches_each() {
    let tape = Tape::seeded(4);
    let mut seen = [false; 3];
    for _ in 0..500 {
        let at = *tape.one_of(&[0usize, 1, 2], "o");
        seen[at] = true;
    }
    assert_eq!(seen, [true; 3]);
}

#[test]
#[should_panic(expected = "one of nothing (`o`)")]
fn one_of_refuses_an_empty_slice() {
    Tape::calm().one_of::<u8>(&[], "o");
}

#[test]
fn weighted_never_returns_a_zero_weight_and_reaches_each_other() {
    let tape = Tape::seeded(2);
    let weights = [0, 3, 0, 1, 5];
    let mut seen = [false; 5];
    for _ in 0..2_000 {
        let at = tape.weighted(&weights, "w");
        assert_ne!(weights[at], 0, "alternative {at} has weight 0");
        seen[at] = true;
    }
    assert_eq!(seen, [false, true, false, true, true]);
}

#[test]
#[should_panic(expected = "`w`: weighted with no weights")]
fn weighted_refuses_no_weights() {
    Tape::calm().weighted(&[], "w");
}

#[test]
#[should_panic(expected = "`w`: weighted with every one of 2 weights 0")]
fn weighted_refuses_all_zero_weights() {
    Tape::calm().weighted(&[0, 0], "w");
}

#[test]
fn btree_map_has_a_size_in_its_sizes() {
    let tape = Tape::seeded(6);
    for _ in 0..500 {
        let map = tape.btree_map(1..=6, "m", |t| t.int::<u16>("k"), |t| t.bool("v"));
        assert!((1..=6).contains(&map.len()), "{} entries", map.len());
    }
}

#[test]
#[should_panic(expected = "`m`: btree_map drew a key already in the map 64 times in a row")]
fn btree_map_refuses_when_keys_cannot_be_distinct() {
    Tape::seeded(1).btree_map(2..=2, "m", |_| 0u8, |t| t.bool("v"));
}

#[test]
fn u128_passes_u64_and_i128_goes_negative() {
    let tape = Tape::seeded(8);
    let (mut wide, mut negative) = (false, false);
    for _ in 0..1_000 {
        wide |= tape.u128("u") > u128::from(u64::MAX);
        negative |= tape.i128("i") < 0;
    }
    assert!(wide && negative, "above u64::MAX {wide}, negative {negative}");
}

#[test]
fn array_draws_each_element_in_a_span() {
    let tape = Tape::calm();
    let array = tape.array::<u32, 5>("element", |t| t.int("x"));
    assert_eq!(array.len(), 5);
    let mut record = Record::new();
    tape.finish(&mut record);
    assert_eq!(record.spans.iter().filter(|span| span.label == "element").count(), 5);
}

#[test]
fn option_is_some_three_times_in_four_and_its_draws_are_a_span() {
    let tape = Tape::seeded(10);
    let some = (0..4_000).filter(|_| tape.option("o", |t| t.int::<u8>("x")).is_some()).count();
    assert!((2_800..=3_200).contains(&some), "{some} of 4,000 were Some");
    let recorded = Tape::replay(&[1]);
    assert_eq!(recorded.option("o", |t| t.int::<u8>("x")), Some(0));
    let mut record = Record::new();
    recorded.finish(&mut record);
    assert_eq!(record.spans.iter().filter(|span| span.label == "o").count(), 1);
}

#[test]
fn bytes_draw_each_byte_in_a_span() {
    let tape = Tape::calm();
    assert_eq!(tape.bytes(3..=3, "byte"), vec![0, 0, 0]);
    let mut record = Record::new();
    tape.finish(&mut record);
    assert_eq!(record.spans.iter().filter(|span| span.label == "byte").count(), 3);
}

#[test]
fn any_compiles_and_is_calm_for_each_type() {
    let calm = Tape::calm;
    assert!(!calm().any::<bool>("x"));
    assert_eq!(calm().any::<char>("x"), ' ');
    assert_eq!(calm().any::<u8>("x"), 0);
    assert_eq!(calm().any::<u16>("x"), 0);
    assert_eq!(calm().any::<u32>("x"), 0);
    assert_eq!(calm().any::<u64>("x"), 0);
    assert_eq!(calm().any::<usize>("x"), 0);
    assert_eq!(calm().any::<u128>("x"), 0);
    assert_eq!(calm().any::<i8>("x"), 0);
    assert_eq!(calm().any::<i16>("x"), 0);
    assert_eq!(calm().any::<i32>("x"), 0);
    assert_eq!(calm().any::<i64>("x"), 0);
    assert_eq!(calm().any::<isize>("x"), 0);
    assert_eq!(calm().any::<i128>("x"), 0);
    assert_eq!(calm().any::<f32>("x").to_bits(), 0);
    assert_eq!(calm().any::<f64>("x").to_bits(), 0);
    calm().any::<()>("x");
    assert_eq!(calm().any::<(u8,)>("x"), (0,));
    assert_eq!(calm().any::<(u8, bool, char, i64)>("x"), (0, false, ' ', 0));
    assert_eq!(calm().any::<(u8, u16, u32, u64, i8, bool)>("x"), (0, 0, 0, 0, 0, false));
    assert_eq!(calm().any::<[u16; 3]>("x"), [0; 3]);
    assert_eq!(calm().any::<Option<Vec<u8>>>("x"), None);
    assert_eq!(calm().any::<Vec<i32>>("x"), Vec::<i32>::new());
    assert_eq!(calm().any::<String>("x"), "");
    assert_eq!(calm().any::<Box<u8>>("x"), Box::new(0));
}
