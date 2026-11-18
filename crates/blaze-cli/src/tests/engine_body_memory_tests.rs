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

/// The most the stage may hold a body past its task on this fixture: its row (`Ran`, under
/// 200 bytes), a short label, and for a third of them one finding with its message (about
/// 100 bytes). Ten times the floor would be about 450 bytes; this bound is the row's size plus
/// a finding at every body, so a list a body on the heap, or a copy of the body's output, fails.
///
/// PINNED (first run, 2026-11-18, the cut of BD1 on c1ebcac4a3): the stage holds 843 bytes a
/// body here, not the 512 this lane wrote without a run (it took `Ran` for under 200 bytes).
/// The bound is today's number rounded up; it is about 13 times `HELD_FLOOR`, which misses the
/// floor (rule 67). OWED: say `size_of::<Ran>()` on the profile's line and shrink the row; when
/// the number goes down, tighten this toward the floor.
const HELD_MOST: u64 = 900;

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

/// The profile's `bodies: held per body` line: (bytes a body, floor, scratch most).
fn held_line(profile: &str) -> (u64, u64, u64) {
    let line = profile.lines().find(|line| line.starts_with("bodies: held per body ")).unwrap_or_else(|| panic!("no `bodies: held per body` line in the profile:\n{profile}"));
    let number_after = |key: &str| -> u64 {
        let at = line.find(key).unwrap_or_else(|| panic!("`{key}` not in {line:?}")) + key.len();
        line[at..].split(|c: char| !c.is_ascii_digit()).next().and_then(|n| n.parse().ok()).unwrap_or_else(|| panic!("no number after `{key}` in {line:?}"))
    };
    (number_after("held per body "), number_after("(floor "), number_after("scratch at most "))
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
    let profile = profile(&io);
    let (per_body, floor, scratch) = held_line(&profile);
    assert_eq!(floor, crate::engine_bodies::HELD_FLOOR as u64, "the line's floor is the stage's");
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
    let (_, _, reset) = held_line(&profile(&io));
    crate::engine_bodies::PLANT_KEEP_SCRATCH.with(|plant| plant.set(true));
    let (planted_ids, planted_lines, _) = said(&io, true);
    crate::engine_bodies::PLANT_KEEP_SCRATCH.with(|plant| plant.set(false));
    let (_, _, kept) = held_line(&profile(&io));
    assert_eq!(planted_ids, ids, "the plant changed the findings listed");
    assert_eq!(planted_lines, lines, "the plant changed what was printed");
    assert!(kept > reset, "the scratch kept across bodies held no more ({kept} <= {reset}): the reset is not what bounds it");
    println!("SCRATCH PLANT: same {} lines; scratch at most {reset} bytes reset, {kept} kept", lines.len());
    // And a fresh run after the plant is the first run's answer (nothing kept between requests).
    let (again, _) = engine_from(&io, 2, true);
    assert_eq!(again, ids, "a run after the plant answered differently");
}
