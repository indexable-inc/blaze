//! BODY CHECKING'S MEMORY IS BOUNDED BY THE LARGEST BODY, NOT BY HOW MANY BODIES A WORKER
//! CHECKED (rule 54; the class: "a per-item allocation into a longer-lived frame").
//!
//! The leak this guards: tempo (16,600 bodies, `engine-check --workers 16`, 2026-11-17) reached
//! 23.4 GB resident and ten workers panicked "frames: its region of 1073741824 reserved bytes is
//! used up" in front2 source.rs `Records::kept`: every path a body resolved had its segments
//! copied into the records' frame (to meet a lifetime `RecordNames` never needed: it answers
//! `World<N>` for every name type), and `NamesSource::variants` copied each enum's variants
//! there per call. That frame is a stretch's (engine_bodies.rs `run_stretch`), never reset under
//! its records, so it grew with every body checked over it. Now a walk reads the caller's
//! segments for the call alone and the variants are the record's rows read in place.
//!
//! A child of main.rs's tests: engine_check_tests.rs lends `engine_from` and `profile`.
//!
//! NOT RUN by the writer (a write-only lane, uncompiled).


use super::engine_check_tests::{engine_from, profile};
use super::*;

/// How many small fns crate `a` holds: each resolves three paths and matches on an enum.
const FNS: usize = 400;

/// The bodies past their stretch's first the guard reads at least (the brief's "last 300").
const LATER_AT_LEAST: u64 = 300;

/// What one later body may leave in its stretch's frame, summed over all of them: nothing. A
/// body of `a` has no local items, so its check keeps nothing past itself; a stretch's one-time
/// costs (its records' decoding, a head frozen on its first read) are its first body's.
const LATER_GREW_FLOOR: u64 = 0;

/// `l`'s one body: a local item, whose rows its lowering keeps in the stretch's frame
/// (front2 locals.rs `Locals::keep`): the control that the meter reads the frame.
const LOCAL: &str = "pub fn local() -> u32 {\n    struct L(u32);\n    L(1).0\n}\n";

/// Two members: `a`, an enum, a helper and [`FNS`] fns that each match on the enum (so
/// `NamesSource::variants` is asked) and name the helper by `crate::` and `self::` paths (so
/// `NamesSource::walk` is asked); and `l`, [`LOCAL`] alone.
fn workspace(io: &Io) {
    write(io, "Cargo.toml", "[workspace]\nmembers = [\"a\", \"l\"]\nresolver = \"2\"\n");
    write(io, "a/Cargo.toml", "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(io, "l/Cargo.toml", "[package]\nname = \"l\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(io, "l/src/lib.rs", LOCAL);
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let mut text = blaze_own::String::new_in(cx.frame());
    let _ = write!(text, "pub enum E {{\n    A,\n    B(u32),\n}}\n\npub fn helper(x: u32) -> u32 {{\n    x\n}}\n");
    for i in 0..FNS {
        let _ = write!(text, "\npub fn f{i}(e: E) -> u32 {{\n    match e {{\n        E::A => crate::helper({i}),\n        E::B(n) => self::helper(n),\n    }}\n}}\n");
    }
    write(io, "a/src/lib.rs", text.as_str());
}

/// The number after `label` on `line` (its first occurrence).
fn number_after(line: &str, label: &str) -> u64 {
    let Some(at) = line.find(label) else { panic!("`{label}` is on the line: {line}") };
    let rest = line[at + label.len()..].trim_start();
    let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    rest[..end].parse().unwrap_or_else(|_| panic!("a number after `{label}`: {line}"))
}

/// THE STRETCH FRAMES DO NOT GROW BY THE BODIES CHECKED: on one worker, every body of `a` past
/// its stretch's first leaves 0 bytes in the stretch's frame ([`LATER_GREW_FLOOR`]), over at
/// least [`LATER_AT_LEAST`] of them. CONTROL: `l`'s body keeps its local item's rows in its
/// frame, so the most one body grew a frame by is above 0: the meter reads the frame.
///
/// Before (the tree with `Records::kept` and the copying `variants`): every later body grew its
/// frame by its paths' segments and its enum's variants, so the sum grew linearly with the
/// bodies; the number the coordinator's run of this test printed on that tree: NOT YET RUN.
#[test]
fn engine_body_frames_do_not_grow_by_the_bodies_checked() {
    let io = Io::simulated(SimWorld::new(41, Limits::USUAL));
    workspace(&io);
    let (_, checked) = engine_from(&io, 1, true);
    let text = profile(&io);
    let Some(line) = text.lines().find(|line| line.starts_with("bodies: the stretch frames grew ")) else {
        panic!("the stretch frames' line is in the profile:\n{text}")
    };
    println!("{line}");
    let bodies = number_after(line, "bytes over");
    let most = number_after(line, "most by one body");
    let Some(at) = line.find("past each stretch's first body") else { panic!("the later bodies' part is on the line: {line}") };
    let past = &line[at..];
    let later_grew = number_after(past, "past each stretch's first body");
    let later = number_after(past, "bytes over");
    assert!(bodies >= FNS as u64, "every fn of `a` was checked in a stretch ({bodies} of {FNS}): {line}\n{}", super::engine_check_tests::entries(&checked));
    assert!(later >= LATER_AT_LEAST, "at least {LATER_AT_LEAST} bodies past their stretch's first: {line}");
    assert!(later_grew <= LATER_GREW_FLOOR, "the bodies past their stretch's first left {later_grew} bytes in its frame over {later} bodies (at most {LATER_GREW_FLOOR}): a question copies into the stretch's frame per body: {line}");
    assert!(most > 0, "CONTROL: `l`'s local item's rows are kept in its stretch's frame, so some body grew a frame: {line}");
}
