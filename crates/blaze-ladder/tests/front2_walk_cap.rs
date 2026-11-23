//! The walk's cap is sized by need and says what it met when it stops; a survey limited to
//! named crates files a root that does not lower as a left-out row and goes on (front2
//! reach.rs `Within`, `cap_of`, `capped_report`; program.rs `survey_within`).
//!
//! The fault this guards (tempo's build census after tree a00b66fa06): the walk stopped at a
//! fixed 65,536 fns, a cap sized from core, alloc and std, on a 1,084-crate program, and said
//! only how many fns were queued; and an `--only` survey stopped at its first root that did
//! not lower.
//!
//! It needs no toolchain: the program is `#![no_core]` (as tests/front2_survey.rs).
#![feature(allocator_api)]

use std::alloc::Global;

use blaze_ladder::crates::{Inputs, Toolchain};
use blaze_ladder::front2::program::{Front2Program, Front2Refusal};
use blaze_ladder::front2::reach::{self, Within};
use blaze_ladder::program_texts::ProgramTexts;
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::NewFront;
use blaze_scope::resolve::Names;
use blaze_surface::ItemDetail;

/// `main` calls a chain of four fns: a walk tables five fns or more.
const CHAIN: &str = "#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = \"sized\"]
trait Sized {}

fn d() -> i32 {
    0
}

fn c() -> i32 {
    d()
}

fn b() -> i32 {
    c()
}

fn a() -> i32 {
    b()
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    a()
}
";

/// `bad`'s parameter names a type no crate declares, so the head its lowering makes is
/// refused (body.rs `BodyError::Head`), and its body calls a macro no crate declares (an
/// expander finding, `BodyError::Lowered`); `good` lowers.
const ONE_BAD_ROOT: &str = "#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = \"sized\"]
trait Sized {}

fn bad(_x: Missing) -> i32 {
    nope!()
}

fn good() -> i32 {
    0
}
";

/// The program `text`'s names, then `f` over them.
fn with_names<R>(text: &str, f: impl for<'n> FnOnce(&'n Names<'n>, &[SourceFile<'_>]) -> R) -> R {
    let files = [SourceFile { path: "src/main.rs", text }];
    let tc = Toolchain::none(blaze_own::Alloc::global());
    let new = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };
    let texts = ProgramTexts::new(&files, "src/main.rs", new, &tc, blaze_build::Panic::Unwind).unwrap_or_else(|e| panic!("the program's texts: {e}"));
    let user = texts.user();
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    f(&names, &files)
}

/// Every free fn of crate 0 by its name, as tempo_build's `--only` takes them.
fn free_fns(names: &Names<'_>) -> Vec<(String, blaze_id::ItemKey)> {
    let mut out = Vec::new();
    for (key, _, place) in names.items(0) {
        let Some((text, outline)) = names.outline_at(0, place.at) else { continue };
        let item = outline.item(place.item);
        let free = item.parent.is_none_or(|p| !matches!(outline.item(p).detail, ItemDetail::Impl | ItemDetail::Trait | ItemDetail::ExternBlock { .. }));
        if matches!(item.detail, ItemDetail::Fn { .. }) && free {
            // The name's range is relative to the item's own start (rule 71).
            let at = item.range.start as usize;
            let name = item.name.map_or("", |r| &text[at + r.start as usize..at + r.end as usize]);
            out.push((String::from(name), key));
        }
    }
    out.sort();
    out
}

/// A walk whose cap is set to 2 stops with the cap's message, and the message carries what it
/// met: the crates the fns it tabled are of, the keys tabled twice, and the fns still queued
/// by path. CONTROL: the same walk under the cap by need ends, and that cap ([`reach::cap_of`]
/// of the names' items) is above the fns it tabled.
///
/// Prints: `CAPPED: front2: `..` reaches more than 2 fns, the cap its caller set ..` then the
/// by-crate block.
#[test]
fn a_walk_over_a_low_cap_stops_and_says_what_it_met() {
    with_names(CHAIN, |names, files| {
        let roots: Vec<blaze_id::ItemKey> = free_fns(names).into_iter().filter(|(n, _)| n == "main").map(|(_, k)| k).collect();
        assert_eq!(roots.len(), 1, "the program has one `main`");
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the survey's frame");
        let mut left_out = Vec::new_in(Global);
        let low = Within { crates: None, most_fns: Some(2) };
        let stopped = Front2Program::survey_within(names, &frame, files, &roots, low, &mut left_out, None, blaze_own::Alloc::global());
        let Err(e) = stopped else { panic!("a walk of five fns stops at a cap of 2") };
        let said = e.to_string();
        println!("CAPPED: {said}");
        let Front2Refusal::Capped { cap, items, forced, queued, .. } = &e else { panic!("the cap's refusal: {said}") };
        assert_eq!((*cap, *forced), (2, true), "the caller's cap");
        assert!(*queued >= 1, "fns were still queued: {said}");
        assert!(*items > 0, "the names hold items: {said}");
        assert!(said.contains("reaches more than 2 fns, the cap its caller set"), "the message names the cap met: {said}");
        assert!(said.contains("REACHED BY CRATE ("), "the by-crate block: {said}");
        assert!(said.contains("TABLED MORE THAN ONCE: "), "the repeats block: {said}");
        assert!(said.contains("STILL QUEUED ("), "the queued block: {said}");
        assert!(said.lines().count() <= 3 + 15 + 1 + 10 + 1 + 20, "the message is bounded: {said}");
        // CONTROL: the cap by need lets the same walk end.
        let mut left_out = Vec::new_in(Global);
        let program = Front2Program::survey_within(names, &frame, files, &roots, Within::ALL, &mut left_out, None, blaze_own::Alloc::global())
            .unwrap_or_else(|e| panic!("control: the walk under the cap by need ends: {e}"));
        let tabled: usize = program.reached_by_crate().iter().map(|(_, n)| n).sum();
        let need = reach::cap_of(reach::items_held(names));
        println!("CONTROL: {tabled} fns tabled; the cap by need {need}");
        // `main` is the walk's start (no row of its census); `a` to `d` are.
        assert!(tabled >= 4, "control: the chain's four callees are tabled: {tabled}");
        assert!(need > tabled, "control: the cap by need is above the walk ({need} > {tabled})");
    });
}

/// A survey limited to crate 0 from both free fns: `bad`, which does not lower, is a left-out
/// row of class `lower: root`; the survey goes on and tables `good`. CONTROL: the same roots
/// through `survey_from` (no `Within`) stop at `bad`, as before.
///
/// Prints: `ROOT LEFT OUT: ..bad: lower: root` and the control's stop.
#[test]
fn a_root_that_does_not_lower_is_left_out_under_within() {
    with_names(ONE_BAD_ROOT, |names, files| {
        let fns = free_fns(names);
        let roots: Vec<blaze_id::ItemKey> = fns.iter().map(|(_, k)| *k).collect();
        assert_eq!(fns.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(), ["bad", "good"], "the fixture's two roots");
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the survey's frame");
        let mut left_out = Vec::new_in(Global);
        let crates = [0usize];
        let within = Within { crates: Some(&crates), most_fns: None };
        let program = Front2Program::survey_within(names, &frame, files, &roots, within, &mut left_out, None, blaze_own::Alloc::global())
            .unwrap_or_else(|e| panic!("a root that does not lower does not stop the survey: {e}"));
        for (f, row) in left_out.iter() {
            println!("ROOT LEFT OUT: {f}: {}", row.class);
        }
        let bad: Vec<_> = left_out.iter().filter(|(f, _)| f.ends_with("bad")).collect();
        let rows: Vec<String> = left_out.iter().map(|(f, row)| format!("{f}: {} | {:?}", row.class, row.refusal)).collect();
        assert_eq!(bad.len(), 1, "`bad` is one left-out row; the rows: {rows:?}");
        assert_eq!(&*bad[0].1.class, "lower: root", "its class");
        assert!(matches!(bad[0].1.refusal, Front2Refusal::Lower(_)), "it keeps the lowering's typed refusal: {:?}", bad[0].1.refusal);
        // `good` is a start of the walk, so a row of the program's table (no census row: it
        // calls nothing); `bad` is none.
        let fns = blaze_ladder::seam::FrontEnd::fn_count(&program);
        assert_eq!(fns, 1, "the table holds `good` alone: {fns} rows");
        // CONTROL: without `Within`, the root's refusal stops the survey (a suite's rows).
        let mut left_out = Vec::new_in(Global);
        let control = Front2Program::survey_from(names, &frame, files, &roots, &mut left_out, None, blaze_own::Alloc::global());
        match control {
            Err(Front2Refusal::Lower(_)) => println!("CONTROL: survey_from stops at `bad`"),
            Err(e) => panic!("control: survey_from stops at `bad`'s lowering, not: {e}"),
            Ok(_) => panic!("control: survey_from stops at `bad`'s lowering"),
        }
    });
}
