//! A CELL THAT DOES NOT FREEZE KEEPS ITS OWN FAILURE (head.rs `SeededHeads::fail`, lane HF1).
//!
//! THE CAUSE (tempo, `blaze check`, 2026-11-18: the Heads stage 577 s over 20,183 head tasks, 98.9%
//! of its busy samples under blaze-cli engine_heads_once.rs `alone`): a shared source answered a
//! failure by the order it was asked in. The ask on which a freeze failed was handed the error of
//! the cell that failed first (the top of its stack), every later ask `Again`, so a failed head's
//! words and reads were facts of the order, and every failed head was frozen again from nothing.
//!
//! THE WORLD: `C` and `Other` fail by themselves (a field of a type no crate declares); `B` holds a
//! `C`, `A` a `B` (depth 1 and 2); `D` holds a `C`; `Both` holds an `A` and an `Other` (two failed
//! heads); `takes` takes a `Both`; `Fine` and `ok` freeze.
//!
//! THE ORACLE, for every order the tape shuffles the cells into (`blaze_sim::check`), each cell
//! asked twice (a head task through its own owner, as engine_heads_once.rs `one_head`): each cell's
//! line (its first answer, its answer asked again, its kept failure with the heads it read, and its
//! own names reads) equals its line when the cells are asked in key order. Then the shape: `C` and
//! `Other` fail by themselves, every other failed cell names its DIRECT need.
//! THE CONTROL (rule 23): the planted `FailPlant::FirstFailedNamed` (the rule before: the first
//! asker told the first failed cell's error, every cell naming that cell) breaks the oracle.

use core::fmt::Write as _;

use blaze_own::{AVec, Alloc};
use blaze_sim::{Broke, Config, Tape, check as sim_check, control};

use super::head::{CellFailure, FailPlant, WANT_BYTES, head_task, want_bytes};
use super::head::{SeededHeads, SeededHeadsOpts};
use super::head_tests::{CORE_AT, Src, a, cells, world_of};
use super::owned_reads::{OwnedRecorder, OwnedRecorderOpts, Owning, ReadOwners};
use super::view::{Tables, TablesOpts};
use crate::front2::heads::Want;
use crate::front2::source::NamesSource;

const WORLD: &str = "#![no_std]
pub struct C {
    pub x: Missing,
}
pub struct B {
    pub c: C,
}
pub struct A {
    pub b: B,
}
pub struct D {
    pub c: C,
}
pub struct Other {
    pub y: AlsoMissing,
}
pub struct Both {
    pub a: A,
    pub o: Other,
}
pub fn takes(b: Both) -> u32 {
    0
}
pub struct Fine {
    pub v: u32,
}
pub fn ok(f: Fine) -> u32 {
    0
}
";

/// The crates whose lang rows name the lang items (the fixture's `core`).
const LANG: &[usize] = &[CORE_AT];

/// What each failed cell's kept failure must be: `own`, or `needs <cell>` (either of the names).
const SHAPE: &[(&str, &[&str])] = &[("C", &["own"]), ("Other", &["own"]), ("B", &["needs C"]), ("A", &["needs B"]), ("D", &["needs C"]), ("Both", &["needs A", "needs Other"]), ("takes", &["needs Both"])];

/// One text of the test's own heap.
type Text = blaze_own::String<Alloc<'static>>;

fn tool() -> blaze_drive::key::Tool {
    blaze_drive::key::Tool(blaze_id::ContentId::of("blaze-ladder failed once test", b"head"))
}

/// Cell `w` by its item's name.
fn label(names: Src, w: Want) -> &'static str {
    NamesSource::name_of(names, w.key()).unwrap_or("?")
}

/// One order's run: each cell's line, by its bytes; and the cells kept frozen and failed.
struct Run {
    lines: AVec<'static, ([u8; WANT_BYTES], &'static str, Text)>,
    frozen: usize,
    failed: usize,
}

/// `order`'s cells, each asked twice through one source and one owned recorder, under `plant`.
fn asked_in(names: Src, sized: Option<blaze_id::ItemKey>, frame: &'static blaze_mem::Frame, order: &[Want], plant: FailPlant) -> Result<Run, Broke> {
    let g = a();
    let rec = OwnedRecorder::new(OwnedRecorderOpts { alloc: g, owning: Owning::Each });
    let view = Tables::new(TablesOpts { records: names, impls: None, lang_crates: LANG, reads: &rec });
    let mut source = SeededHeads::new(SeededHeadsOpts { names: view, sized, pointers: crate::front2::receiver::Pointers::NOT_LOOKED_UP, frame, scratch: g, alloc: g, consts: None, known: None, owners: Some(&rec) });
    source.plant_failures(plant);
    // (cell, first answer, answer asked again)
    let mut answers: AVec<'static, (Want, Text, Text)> = AVec::new_in(g);
    for pass in 0..2 {
        for (at, w) in order.iter().copied().enumerate() {
            rec.enter(w);
            let made = head_task(view, &mut source, w, tool(), g);
            rec.leave();
            let mut said = Text::new_in(g);
            match &made.failed {
                Some(e) => {
                    let _ = write!(said, "failed: {e}");
                }
                None => {
                    let _ = write!(said, "froze {:?}", made.out);
                }
            }
            match pass {
                0 => answers.push((w, said, Text::new_in(g))),
                _ => answers[at].2 = said,
            }
        }
    }
    if let Some(why) = rec.broken() {
        return Err(Broke::Harness(blaze_own::text::format(g, format_args!("the owned recorder refused: {why}")).as_str().into()));
    }
    let mut lines: AVec<'static, ([u8; WANT_BYTES], &'static str, Text)> = AVec::new_in(g);
    for (w, first, again) in answers.iter() {
        let mut line = Text::new_in(g);
        let _ = write!(line, "first {first}; again {again}; failure ");
        match source.failure(*w) {
            None => {
                let _ = write!(line, "none");
            }
            Some((failure, heads)) => {
                match failure {
                    CellFailure::Own(e) => {
                        let _ = write!(line, "own ({e})");
                    }
                    CellFailure::Needs(need) => {
                        let _ = write!(line, "needs {}", label(names, need));
                    }
                    CellFailure::Refused { want, why } => {
                        let _ = write!(line, "refused {}: {}", label(names, want), why.said());
                    }
                }
                let _ = write!(line, "; heads read [");
                for h in heads.iter() {
                    let _ = write!(line, " {}", label(names, *h));
                }
                let _ = write!(line, " ]");
            }
        }
        let _ = write!(line, "; reads [");
        let read = rec.reads_of(*w, &mut |one| {
            let _ = write!(line, " {one:?}");
        });
        if let Err(why) = read {
            return Err(Broke::Harness(blaze_own::text::format(g, format_args!("the reads of {}: {why}", label(names, *w))).as_str().into()));
        }
        let _ = write!(line, " ]");
        lines.push((want_bytes(*w), label(names, *w), line));
    }
    lines.sort_unstable_by_key(|(at, _, _)| *at);
    Ok(Run { lines, frozen: source.kept_len(), failed: source.failed_len() })
}

/// One case: the cells shuffled by the tape, against the same cells in key order.
fn case(tape: &Tape, plant: FailPlant) -> Result<(), Broke> {
    let (names, sized, frame) = world_of(WORLD);
    let reference = cells(names);
    let mut order: AVec<'static, Want> = AVec::new_in(a());
    order.extend_from_slice(&reference);
    tape.shuffle(&mut order[..], "the order the cells are asked in");
    let keyed = asked_in(names, sized, frame, &reference, plant)?;
    let shuffled = asked_in(names, sized, frame, &order, plant)?;
    let g = a();
    for ((_, name, want), (_, _, got)) in keyed.lines.iter().zip(shuffled.lines.iter()) {
        if want != got {
            let mut asked = Text::new_in(g);
            for w in order.iter() {
                let _ = write!(asked, " {}", label(names, *w));
            }
            return Err(Broke::Oracle(
                blaze_own::text::format(g, format_args!("cell `{name}`'s failure depends on the order: asked in key order `{want}`, asked in the order [{asked} ] `{got}`")).as_str().into(),
            ));
        }
    }
    if keyed.failed != shuffled.failed || keyed.frozen != shuffled.frozen {
        return Err(Broke::Oracle(
            blaze_own::text::format(g, format_args!("the cells kept depend on the order: frozen {} and failed {} in key order, {} and {} shuffled", keyed.frozen, keyed.failed, shuffled.frozen, shuffled.failed)).as_str().into(),
        ));
    }
    // The shape: who fails by itself, and the direct need each other failed cell names.
    for (name, may) in SHAPE.iter() {
        let Some((_, _, line)) = keyed.lines.iter().find(|(_, n, _)| n == name) else {
            return Err(Broke::Harness(blaze_own::text::format(g, format_args!("the world has no cell `{name}`")).as_str().into()));
        };
        let failure = line.as_str().split("; failure ").nth(1).unwrap_or("");
        if !may.iter().any(|one| failure.starts_with(one)) {
            return Err(Broke::Oracle(blaze_own::text::format(g, format_args!("cell `{name}` keeps the failure `{failure}`, not one of {may:?}")).as_str().into()));
        }
        // Every ask of a failed cell, the first too, is answered alike.
        let (first, again) = (line.as_str().split("; again ").next().unwrap_or(""), line.as_str().split("; again ").nth(1).and_then(|rest| rest.split("; failure ").next()).unwrap_or(""));
        if first.strip_prefix("first ") != Some(again) {
            return Err(Broke::Oracle(blaze_own::text::format(g, format_args!("cell `{name}` was answered `{first}` first and `{again}` asked again")).as_str().into()));
        }
    }
    for name in ["Fine", "ok"] {
        if !keyed.lines.iter().any(|(_, n, line)| *n == name && line.as_str().starts_with("first froze")) {
            return Err(Broke::Oracle(blaze_own::text::format(g, format_args!("cell `{name}` did not freeze")).as_str().into()));
        }
    }
    Ok(())
}

/// Expected line: `failed once: 48 orders of 9 cells, 7 failed and N frozen once each; every
/// cell's answers, failure and reads equal its key order's` (N: the app's two and core's).
#[test]
fn a_failed_cell_keeps_its_own_failure_in_every_order() {
    let (names, sized, frame) = world_of(WORLD);
    let app = cells(names);
    let once = asked_in(names, sized, frame, &app, FailPlant::Rule).unwrap_or_else(|e| panic!("the key order: {e}"));
    // The floor (rule 67): each failed cell kept failed once, though every cell is asked twice.
    assert_eq!(once.failed, SHAPE.len(), "{} failures kept for the {} cells the world fails (`Missing` resolved, or one failed twice?)", once.failed, SHAPE.len());
    sim_check(Config::cases(48), |tape| case(tape, FailPlant::Rule));
    std::println!(
        "failed once: 48 orders of {} cells, {} failed and {} frozen once each; every cell's answers, failure and reads equal its key order's",
        app.len(),
        once.failed,
        once.frozen
    );
}

/// CONTROL (rule 23): the rule before lane HF1, planted, is caught by the same oracle.
#[test]
fn the_order_oracle_catches_the_first_failed_cell_named() {
    control(Config::cases(96), FailPlant::Rule, FailPlant::FirstFailedNamed, "depends on the order", case);
}
