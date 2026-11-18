//! WHAT THE BODIES STAGE HOLDS A BODY (lane BD1, 2026-11-18). On tempo (about 700 crates) the
//! Bodies stage grew the process by 20 GB: what a body's check made outlived its task. Now a
//! body's findings and words are made in a scratch frame reset as each body ends, the run hands
//! on only its rows and their findings (engine_bodies.rs `RunOut`), and the stage holds a body
//! only its row, label, findings and note words (`BodiesStage::held`), said on the profile's
//! `bodies:` line with its floor (rule 67: `HELD_FLOOR`, a task name, a record key and a status).
//!
//! The Rust rule the planted errors rest on (the Reference, "Function body", "Type
//! coercions"): a body's tail is coerced to its return type, and no coercion turns `u8` into
//! `u16`: `fn f(x: u8) -> u16 { x }` is one E0308.

use super::engine_check_tests::{engine_from, profile, said};
use super::*;

/// Bodies in the fixture, and every how-many-th one holding one type error.
const BODIES: usize = 300;
const EVERY: usize = 3;

/// On a 64-bit host the row is 96 bytes. The labels f0 through f299 total 1090 bytes.
/// Every third body has a 40-byte finding and a 32-byte message (E0308, separator,
/// "found `u8`, expected `u16`"). There are no notes. The integer mean is
/// 96 + (1090 + 100 * (40 + 32)) / 300 = 123 bytes per body.
///
/// PINNED (first run, 2026-11-18, the cut of BD1 on c1ebcac4a3): the stage holds 843 bytes a
/// body here, not the 512 this lane wrote without a run (it took `Ran` for under 200 bytes).
/// That measurement was about 13 times `HELD_FLOOR` and the bound was raised to 900.
const HELD_MOST: u64 = 123;

/// A lib of [`BODIES`] fns, every [`EVERY`]th one returning a `u8` as a `u16`.
fn many_bodies(io: &Io) {
    use core::fmt::Write as _;
    write(io, "Cargo.toml", MANIFEST);
    // The text in a frame of the test's own (no global heap in a line of this lane).
    let frame = bstd::mem::Frame::with_reserve(1 << 20, bstd::mem::Tag::Other).expect("the fixture's frame");
    let mut lib = blaze_own::String::new_in(&frame);
    for i in 0..BODIES {
        let ret = if i % EVERY == 0 { "u16" } else { "u8" };
        writeln!(lib, "pub fn f{i}(x: u8) -> {ret} {{ x }}").expect("a string takes text");
    }
    write(io, "src/lib.rs", lib.as_str());
}

struct HeldLine {
    per_body: u64,
    floor: u64,
    row: u64,
    label: u64,
    findings: u64,
    words: u64,
    scratch: u64,
}

/// Read the numbers the stage actually printed, including the independently rounded means.
fn held_line(profile: &str) -> HeldLine {
    let line = profile.lines().find(|line| line.starts_with("bodies: held per body ")).unwrap_or_else(|| panic!("no `bodies: held per body` line in the profile:\n{profile}"));
    let number_after = |key: &str| -> u64 {
        let at = line.find(key).unwrap_or_else(|| panic!("`{key}` not in {line:?}")) + key.len();
        line[at..].split(|c: char| !c.is_ascii_digit()).next().and_then(|n| n.parse().ok()).unwrap_or_else(|| panic!("no number after `{key}` in {line:?}"))
    };
    HeldLine {
        per_body: number_after("held per body "),
        floor: number_after("(floor "),
        row: number_after("row "),
        label: number_after("label "),
        findings: number_after("findings "),
        words: number_after("words "),
        scratch: number_after("scratch at most "),
    }
}

/// THE STAGE HOLDS A BODY ITS ROW AND FINDINGS ALONE: on [`BODIES`] bodies with a finding in
/// every third, the profile says what the stage holds a body past its task, at most
/// [`HELD_MOST`] bytes, and its floor. Prints
/// `BODIES HELD: <B> bytes a body (floor <F>, <B/F>x) over 300 bodies, 100 findings; scratch at most <S> bytes`.
#[test]
fn the_bodies_stage_holds_a_body_its_row_and_findings_alone() {
    let io = Io::simulated(SimWorld::new(41, Limits::USUAL));
    many_bodies(&io);
    let (_, lines, _) = said(&io, true);
    let errors = lines.iter().filter(|line| line.contains("error[")).count();
    assert_eq!(errors, BODIES.div_ceil(EVERY), "every planted E0308 printed:\n{}", lines.join("\n"));
    for i in (0..BODIES).step_by(EVERY) {
        let path = format!("src/lib.rs:{}:", i + 1);
        assert!(lines.iter().any(|line| line.contains(&path) && line.contains("error[types]") && line.contains("found `u8`, expected `u16`")),
            "missing the tail type mismatch for f{i} at {path}");
    }
    let profile = profile(&io);
    let HeldLine { per_body, floor, row, label, findings, words, scratch } = held_line(&profile);
    assert_eq!(floor, crate::engine_bodies::HELD_FLOOR as u64, "the line's floor is the stage's");
    assert_eq!(row, core::mem::size_of::<crate::engine_bodies::Ran<'_>>() as u64);
    assert_eq!(label, 1090 / BODIES as u64, "the labels are f0 through f299");
    assert!(findings > 0, "the stage must account for the planted findings");
    assert_eq!(words, 0, "the type mismatches have findings and every body was checked");
    // Only label and findings have a fractional part here. Their two discarded fractions
    // can contribute at most one byte to the independently rounded total.
    let parts = row + label + findings + words;
    assert!((parts..=parts + 1).contains(&per_body), "parts and total disagree:\n{profile}");
    assert!(per_body >= floor, "a body held under its floor ({per_body} < {floor}): the count misses its row");
    assert!(per_body <= HELD_MOST, "the stage holds {per_body} bytes a body, over {HELD_MOST}:\n{profile}");
    println!("BODIES HELD: {per_body} bytes a body (floor {floor}, {:.1}x) over {BODIES} bodies, {errors} findings; scratch at most {scratch} bytes", per_body as f64 / floor as f64);
}

/// THE SCRATCH RESET CHANGES NO ANSWER (rule 23): with the per-body reset planted off
/// (`PLANT_KEEP_SCRATCH`: a run's scratch keeps every body's findings), the same findings and
/// lines print, and the most a body's scratch held grows (the cause removed, its effect on
/// memory alone). Prints `SCRATCH PLANT: same <N> lines; scratch at most <S> bytes reset, <P> kept`.
#[test]
fn the_per_body_scratch_reset_changes_no_answer() {
    let io = Io::simulated(SimWorld::new(43, Limits::USUAL));
    many_bodies(&io);
    let (ids, lines, _) = said(&io, true);
    let reset = held_line(&profile(&io)).scratch;
    crate::engine_bodies::PLANT_KEEP_SCRATCH.with(|plant| plant.set(true));
    let (planted_ids, planted_lines, _) = said(&io, true);
    crate::engine_bodies::PLANT_KEEP_SCRATCH.with(|plant| plant.set(false));
    let kept = held_line(&profile(&io)).scratch;
    assert_eq!(planted_ids, ids, "the plant changed the findings listed");
    assert_eq!(planted_lines, lines, "the plant changed what was printed");
    assert!(kept > reset, "the scratch kept across bodies held no more ({kept} <= {reset}): the reset is not what bounds it");
    println!("SCRATCH PLANT: same {} lines; scratch at most {reset} bytes reset, {kept} kept", lines.len());
    // And a fresh run after the plant is the first run's answer (nothing kept between requests).
    let (again, _) = engine_from(&io, 2, true);
    assert_eq!(again, ids, "a run after the plant answered differently");
}
