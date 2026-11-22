//! The edge tables' own proof (test.md "End state: the search itself", piece S1b): with
//! `edge-first` on, every entry of each table is produced, every draw keeps its promises, a
//! planted leak out of `text_from`'s alphabet is caught, and two bugs only an edge value shows
//! are found within the default case count; with it off, three pinned tapes replay to the values
//! the draws gave before S1b (read from draw.rs's logic as it stands at the 15:45 snapshot).
//!
//! Run: `LANECHECK_WAIT=540 bash /Volumes/Projects/blaze-int-1112-notes/lanecheck.sh blaze-sim
//! test edge_text` with and without the feature `edge-first`.

#![expect(
    blaze::error_debug,
    reason = "a test's failure message shows the values it compared as they are"
)]

#[cfg(feature = "edge-first")]
use blaze_own::Alloc;

#[cfg(feature = "edge-first")]
use crate::search::{Broke, Config, Verdict};
use crate::tape::Tape;
#[cfg(feature = "edge-first")]
use crate::{check, control, search};

#[cfg(feature = "edge-first")]
use super::{
    BYTE_SHAPES, EDGE_CHARS, EDGE_FLOATS, FROM_SHAPES, Leak, NAME_SHAPES, RANGE_EDGES,
    TEXT_SHAPES, Width,
};

/// Draws per table in the coverage test: with the gate one in four, about a thousand edges
/// over at most 21 entries.
#[cfg(feature = "edge-first")]
const DRAWS: usize = 4_000;

/// Alphabets for `text_from` whose last character has a next scalar value outside the
/// alphabet, so the planted leak always leaves it.
#[cfg(feature = "edge-first")]
const ALPHABETS: [&str; 4] = ["a", "abc", "q0_", "xyz\u{E9}"];

/// How many entries were produced at least once.
#[cfg(feature = "edge-first")]
fn covered(seen: &[u32]) -> usize {
    seen.iter().filter(|count| **count > 0).count()
}

/// The index of `x` in the table of the width's edge floats, by bits.
#[cfg(feature = "edge-first")]
fn float_entry<W: Width, B: PartialEq>(x: W, bits: impl Fn(W) -> B) -> Option<usize> {
    EDGE_FLOATS.iter().position(|edge| bits(W::of(*edge)) == bits(x))
}

/// (a) Every entry of every table is produced at least once over `DRAWS` seeded draws, where
/// the sizes allow it (sizes 0..=300 allow every byte, text and name shape; the range
/// -3.0..=5.0 allows every range edge). Prints:
/// `edge tables (4000 draws each): bytes 9/9, chars 21/21, text 4/4, text_from 3/3, names 11/11, f64 16/16, f64_finite 12/12, f32 16/16, f32_finite 12/12, f64_in 6/6, f32_in 6/6`
#[cfg(feature = "edge-first")]
#[test]
fn every_table_entry_is_produced() {
    let tape = Tape::seeded(95);
    let sizes = 0..=300;
    let mut bytes = [0u32; BYTE_SHAPES.len()];
    let mut chars = [0u32; EDGE_CHARS.len()];
    let mut text = [0u32; TEXT_SHAPES.len()];
    let mut from = [0u32; FROM_SHAPES.len()];
    let mut names = [0u32; NAME_SHAPES.len()];
    let mut f64s = [0u32; EDGE_FLOATS.len()];
    let mut f64_finite = [0u32; EDGE_FLOATS.len()];
    let mut f32s = [0u32; EDGE_FLOATS.len()];
    let mut f32_finite = [0u32; EDGE_FLOATS.len()];
    let mut f64_in = [0u32; RANGE_EDGES.len()];
    let mut f32_in = [0u32; RANGE_EDGES.len()];
    for _ in 0..DRAWS {
        let mut out: Vec<u8> = Vec::new();
        if let Some(at) = super::bytes(&tape, &sizes, "bytes", &mut out) {
            bytes[at] += 1;
        }
        if let Some(c) = super::char(&tape, "char") {
            let at = EDGE_CHARS.iter().position(|e| *e == c).expect("an edge char is in the table");
            chars[at] += 1;
        }
        let mut out = String::new();
        if let Some(at) = super::text(&tape, &sizes, "text", &mut out) {
            text[at] += 1;
        }
        let mut out = String::new();
        if let Some(at) = super::text_from(&tape, "abc", 3, &sizes, "from", &mut out, Leak::None) {
            from[at] += 1;
        }
        let mut out = String::new();
        if let Some(at) = super::name(&tape, &sizes, "name", &mut out) {
            names[at] += 1;
        }
        if let Some(x) = super::float::<f64>(&tape, "f64") {
            f64s[float_entry(x, f64::to_bits).expect("an edge f64 is in the table")] += 1;
        }
        if let Some(x) = super::float_finite::<f64>(&tape, "f64_finite") {
            f64_finite[float_entry(x, f64::to_bits).expect("an edge f64 is in the table")] += 1;
        }
        if let Some(x) = super::float::<f32>(&tape, "f32") {
            f32s[float_entry(x, f32::to_bits).expect("an edge f32 is in the table")] += 1;
        }
        if let Some(x) = super::float_finite::<f32>(&tape, "f32_finite") {
            f32_finite[float_entry(x, f32::to_bits).expect("an edge f32 is in the table")] += 1;
        }
        if let Some(x) = super::float_in::<f64>(&tape, -3.0, 5.0, "f64_in") {
            let edges = [-3.0f64, 5.0, (-3.0f64).next_up(), 5.0f64.next_down(), 0.0, -0.0];
            let at = edges.iter().position(|e| e.to_bits() == x.to_bits());
            f64_in[at.expect("a range edge is one of the six")] += 1;
        }
        if let Some(x) = super::float_in::<f32>(&tape, -3.0, 5.0, "f32_in") {
            let edges = [-3.0f32, 5.0, (-3.0f32).next_up(), 5.0f32.next_down(), 0.0, -0.0];
            let at = edges.iter().position(|e| e.to_bits() == x.to_bits());
            f32_in[at.expect("a range edge is one of the six")] += 1;
        }
    }
    let finite = EDGE_FLOATS.iter().filter(|e| f64::of(**e).is_finite()).count();
    println!(
        "edge tables ({DRAWS} draws each): bytes {}/{}, chars {}/{}, text {}/{}, text_from {}/{}, \
         names {}/{}, f64 {}/{}, f64_finite {}/{finite}, f32 {}/{}, f32_finite {}/{finite}, \
         f64_in {}/{}, f32_in {}/{}",
        covered(&bytes),
        bytes.len(),
        covered(&chars),
        chars.len(),
        covered(&text),
        text.len(),
        covered(&from),
        from.len(),
        covered(&names),
        names.len(),
        covered(&f64s),
        f64s.len(),
        covered(&f64_finite),
        covered(&f32s),
        f32s.len(),
        covered(&f32_finite),
        covered(&f64_in),
        f64_in.len(),
        covered(&f32_in),
        f32_in.len(),
    );
    assert_eq!(covered(&bytes), bytes.len(), "byte shapes: {bytes:?}");
    assert_eq!(covered(&chars), chars.len(), "characters: {chars:?}");
    assert_eq!(covered(&text), text.len(), "text shapes: {text:?}");
    assert_eq!(covered(&from), from.len(), "text_from shapes: {from:?}");
    assert_eq!(covered(&names), names.len(), "name shapes: {names:?}");
    assert_eq!(covered(&f64s), f64s.len(), "f64: {f64s:?}");
    assert_eq!(covered(&f64_finite), finite, "f64_finite: {f64_finite:?}");
    assert_eq!(covered(&f32s), f32s.len(), "f32: {f32s:?}");
    assert_eq!(covered(&f32_finite), finite, "f32_finite: {f32_finite:?}");
    assert_eq!(covered(&f64_in), f64_in.len(), "f64_in: {f64_in:?}");
    assert_eq!(covered(&f32_in), f32_in.len(), "f32_in: {f32_in:?}");
}

/// A finding of the promises test, as the oracle's.
#[cfg(feature = "edge-first")]
fn broke(why: std::fmt::Arguments<'_>) -> Broke {
    Broke::Oracle(std::fmt::format(why))
}

/// The invariant: with edge-first enabled, every `blaze-sim` generator respects its size, range, UTF-8, and alphabet bounds without leaking characters outside `text_from`'s alphabet.
/// (b) Over drawn sizes, alphabets and ranges, the draws keep their promises with the edges on:
/// `bytes`, `text`, `name` and `text_from` give a size in `sizes` (counted in characters for
/// text); `text_from` only its alphabet's characters; `char` no surrogate; `f64_finite` and
/// `f32_finite` no NaN and no infinity; `f64_in` and `f32_in` a finite value inside the range.
/// Prints nothing of its own; passes as `test edge_text::tests::the_draws_keep_their_promises ... ok`.
#[cfg(feature = "edge-first")]
#[test]
fn the_draws_keep_their_promises() {
    check(Config::cases(2_000), |tape| -> Result<(), Broke> {
        let least = tape.int_in::<usize>(0..=300, "least");
        let most = least + tape.int_in::<usize>(0..=300, "room");
        let sizes = least..=most;
        let bytes = tape.bytes(sizes.clone(), "bytes");
        if !sizes.contains(&bytes.len()) {
            return Err(broke(format_args!("bytes: {} bytes outside {sizes:?}", bytes.len())));
        }
        let text = tape.text(sizes.clone(), "text");
        let name = tape.name(sizes.clone(), "name", Alloc::global());
        for (what, count) in [("text", text.chars().count()), ("name", name.chars().count())] {
            if !sizes.contains(&count) {
                return Err(broke(format_args!("{what}: {count} characters outside {sizes:?}")));
            }
        }
        let alphabet = *tape.one_of(&ALPHABETS, "alphabet");
        let from = tape.text_from(alphabet, sizes.clone(), "text_from");
        if !sizes.contains(&from.chars().count()) {
            return Err(broke(format_args!("text_from: {from:?} has a size outside {sizes:?}")));
        }
        if let Some(c) = from.chars().find(|c| !alphabet.contains(*c)) {
            return Err(broke(format_args!("text_from: {c:?} is outside its alphabet {alphabet:?}")));
        }
        let c = tape.char("char");
        if (0xD800..=0xDFFF).contains(&u32::from(c)) {
            return Err(broke(format_args!("char: {c:?} is a surrogate")));
        }
        let x = tape.f64_finite("f64_finite");
        let y = tape.f32_finite("f32_finite");
        if !x.is_finite() || !y.is_finite() {
            return Err(broke(format_args!("finite draws gave {x:?} and {y:?}")));
        }
        let (a, b) = (tape.f64_finite("lo"), tape.f64_finite("hi"));
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
        let x = tape.f64_in(lo..=hi, "f64_in");
        if !x.is_finite() || x < lo || x > hi {
            return Err(broke(format_args!("f64_in: {x:?} outside {lo:?}..={hi:?}")));
        }
        let (a, b) = (tape.f32_finite("lo"), tape.f32_finite("hi"));
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
        let y = tape.f32_in(lo..=hi, "f32_in");
        if !y.is_finite() || y < lo || y > hi {
            return Err(broke(format_args!("f32_in: {y:?} outside {lo:?}..={hi:?}")));
        }
        Ok(())
    });
}

/// (c) The alphabet oracle catches a planted `text_from` that leaks the scalar value after the
/// alphabet's last character (in the edge shape of first and last characters), and passes the
/// clean one. Passes as `test edge_text::tests::the_oracle_catches_a_text_from_leaking_past_its_alphabet ... ok`
/// (a miss panics `the oracle missed PastLast in 500 cases`).
#[cfg(feature = "edge-first")]
#[test]
fn the_oracle_catches_a_text_from_leaking_past_its_alphabet() {
    control(Config::cases(500), Leak::None, Leak::PastLast, "outside its alphabet", |tape, leak| {
        let alphabet = *tape.one_of(&ALPHABETS, "alphabet");
        for _ in 0..4 {
            let mut text = String::new();
            tape.text_from_into(alphabet, 0..=12, "text_from", leak, &mut text);
            if let Some(c) = text.chars().find(|c| !alphabet.contains(*c)) {
                return Err(broke(format_args!(
                    "{c:?} in {text:?} is outside its alphabet {alphabet:?}"
                )));
            }
        }
        Ok(())
    });
}

/// The shrunk tape of a failed search, replayed through `draw`.
#[cfg(feature = "edge-first")]
fn least<T>(verdict: &Verdict, draw: impl Fn(&Tape) -> T) -> (u32, u32, T) {
    let Verdict::Failed(failed) = verdict else {
        panic!("the search did not find the bug in the default case count: {verdict:?}")
    };
    let mut values = Vec::new();
    failed.least.values_into(&mut values);
    (failed.case, failed.cases, draw(&Tape::replay(&values)))
}

/// Whether `bytes` is at least 9 bytes of valid UTF-8 holding a character past ASCII, then one
/// byte that breaks it: what a bug in a decoder's resynchronisation needs.
#[cfg(feature = "edge-first")]
fn utf8_then_break(bytes: &[u8]) -> bool {
    let Some((_, head)) = bytes.split_last() else { return false };
    head.len() >= 8
        && core::str::from_utf8(head).is_ok_and(|text| !text.is_ascii())
        && core::str::from_utf8(bytes).is_err()
}

/// (d) A search with `Config::DEFAULT` (256 cases) finds a bug that needs a text holding U+2028,
/// and one that needs a byte string of valid UTF-8 followed by one breaking byte. Plain draws
/// give U+2028 about once in 250,000 characters, and such bytes almost never. Prints, for
/// example (the case numbers and values are the run's own):
/// `U+2028 found at case 3 of 256, shrunk to "\u{2028}"`
/// `UTF-8 then a breaking byte found at case 9 of 256, shrunk to [0, 0, 0, 0, 0, 0, 0, 194, 128, 128]`
#[cfg(feature = "edge-first")]
#[test]
fn the_search_finds_bugs_only_edges_show() {
    let line_separator = |tape: &Tape| tape.text(0..=32, "text");
    let verdict = search(Config::DEFAULT, |tape| -> Result<(), Broke> {
        let text = line_separator(tape);
        match text.contains('\u{2028}') {
            true => Err(broke(format_args!("{text:?} holds U+2028"))),
            false => Ok(()),
        }
    });
    let (case, cases, text) = least(&verdict, line_separator);
    println!("U+2028 found at case {case} of {cases}, shrunk to {text:?}");
    assert!(text.contains('\u{2028}'), "the shrunk text {text:?} lost U+2028");

    let four = |tape: &Tape| -> Vec<Vec<u8>> {
        (0..4).map(|_| tape.bytes(0..=64, "bytes")).collect()
    };
    let verdict = search(Config::DEFAULT, |tape| -> Result<(), Broke> {
        match four(tape).into_iter().find(|bytes| utf8_then_break(bytes)) {
            Some(bytes) => Err(broke(format_args!("{bytes:?} is UTF-8 then a breaking byte"))),
            None => Ok(()),
        }
    });
    let (case, cases, drawn) = least(&verdict, four);
    let found = drawn.into_iter().find(|bytes| utf8_then_break(bytes));
    println!("UTF-8 then a breaking byte found at case {case} of {cases}, shrunk to {found:?}");
    assert!(found.is_some(), "the shrunk tape lost the breaking bytes");
}

/// The pinned tapes, each with the values the draws give from it. Read from draw.rs at the
/// 15:45 snapshot: `bytes` is `vec` (a `more` flag, 1 to go on, then `below(256)` per byte);
/// `char` kind 5 is `below(BASIC_COUNT)` past the surrogates (0xD800 names U+E000); `text_from`
/// is `vec` of `index(3)`; a float is a kind of `below(5)` then its own draws, kind 4 one of the
/// twelve specials (8 is infinity, 11 the least subnormal), kind 3 one raw draw; `f64_in`'s kind
/// 1 is its end.
const PINNED_BYTES: [u64; 5] = [1, 7, 1, 255, 0];
const PINNED_TEXT: [u64; 7] = [5, 0xD800, 1, 2, 1, 0, 0];
const PINNED_FLOATS: [u64; 7] = [4, 8, 1, 4, 11, 3, 0x4049_0FDB];

/// The draws of the three pinned tapes, in order.
fn pinned(bytes: &Tape, text: &Tape, floats: &Tape) -> (Vec<u8>, char, String, [u64; 4]) {
    let drawn_bytes = bytes.bytes(0..=8, "bytes");
    let c = text.char("char");
    let from = text.text_from("xyz", 0..=5, "text_from");
    let f = [
        floats.f64("f64").to_bits(),
        floats.f64_in(-1.0..=1.0, "f64_in").to_bits(),
        u64::from(floats.f32("f32").to_bits()),
        u64::from(floats.f32("f32").to_bits()),
    ];
    (drawn_bytes, c, from, f)
}

/// What the pinned tapes give: `[7, 255]`, U+E000, "zx", and infinity, 1.0, the least `f32`
/// subnormal and the `f32` of bits 0x4049_0FDB (pi).
fn pinned_expected() -> (Vec<u8>, char, String, [u64; 4]) {
    (
        vec![7, 255],
        '\u{E000}',
        String::from("zx"),
        [f64::INFINITY.to_bits(), 1.0f64.to_bits(), 1, 0x4049_0FDB],
    )
}

/// (e) With `edge-first` off, the three pinned tapes replay to the values the draws gave before
/// S1b. Prints `pinned tapes, edge-first off: [7, 255] '\u{e000}' "zx" [9218868437227405312, 4607182418800017408, 1, 1078530011]`
#[cfg(not(feature = "edge-first"))]
#[test]
fn pinned_tapes_replay_as_before_with_the_feature_off() {
    let got = pinned(
        &Tape::replay(&PINNED_BYTES),
        &Tape::replay(&PINNED_TEXT),
        &Tape::replay(&PINNED_FLOATS),
    );
    println!("pinned tapes, edge-first off: {:?} {:?} {:?} {:?}", got.0, got.1, got.2, got.3);
    assert_eq!(got, pinned_expected());
}

/// (e, the feature on) The pinned char and float tapes with a calm gate (0) before each gated
/// draw give the same values: the calm gate is the draw as it was. The lists (`bytes`,
/// `text_from`) also go through S2's shaped `vec`, whose choices come first; their replay is
/// S2's to pin. Prints
/// `pinned tapes, edge-first on, gates calm: '\u{e000}' [9218868437227405312, 4607182418800017408, 1, 1078530011]`
#[cfg(feature = "edge-first")]
#[test]
fn pinned_tapes_with_calm_gates_replay_as_before() {
    let c = Tape::replay(&[0, PINNED_TEXT[0], PINNED_TEXT[1]]).char("char");
    let floats = Tape::replay(&[
        0,
        PINNED_FLOATS[0],
        PINNED_FLOATS[1],
        0,
        PINNED_FLOATS[2],
        0,
        PINNED_FLOATS[3],
        PINNED_FLOATS[4],
        0,
        PINNED_FLOATS[5],
        PINNED_FLOATS[6],
    ]);
    let f = [
        floats.f64("f64").to_bits(),
        floats.f64_in(-1.0..=1.0, "f64_in").to_bits(),
        u64::from(floats.f32("f32").to_bits()),
        u64::from(floats.f32("f32").to_bits()),
    ];
    println!("pinned tapes, edge-first on, gates calm: {c:?} {f:?}");
    let expected = pinned_expected();
    assert_eq!((c, f), (expected.1, expected.3));
}
