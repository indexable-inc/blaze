//! A block-scoped fn is an item. Its return expression must have its declared type even
//! when nothing calls it. The enclosing body's task owns its verdict and dependencies.

use blaze_drive::key::{ABSENT, Key, Kind, Name};
use blaze_engine::{CellId, Made, Phase, Planned, SVec, Stage, Task};
use blaze_id::ContentId;

use super::head_tests::{CORE_AT, Src, a, cells, world_of};
use super::recorder::{Recorder, RecorderOpts};
use super::task::{BodyFrame, BodyOut, Coverage, Make, MakeRefused, SaidWhat, check_body, check_body_in};
use super::view::{Tables, TablesOpts};
use crate::front2::heads::{NeedsMet, Want};
use crate::front2::project::{BodyRef, ProjectWorld};
use crate::front2::source::NamesSource;

const CLEAN: &str = "#![no_std]\npub fn outer() { fn inner() -> u16 { 1u16 } }";
const WRONG: &str = "#![no_std]\npub fn outer() { fn inner() -> u16 { 1u8 } }";
const PLAIN: &str = "#![no_std]\npub fn outer() -> u16 { 1u8 }";
const NESTED: &str = "#![no_std]
pub fn value() -> u8 { 1u8 }
pub fn outer() {
    fn left() { fn inner() -> u16 { value() } }
    fn right() { fn inner() -> u16 { 1u8 } }
}";

struct Unchanged;

impl<'a> Make<'a> for Unchanged {
    fn make(&self, _: &CellId, _: &mut SVec<'a, Made<'a>>) -> Result<ContentId, MakeRefused> {
        Ok(ABSENT)
    }
}

fn function(records: Src, name: &str) -> Want {
    cells(records).iter().copied().find(|w| matches!(w, Want::Fn(_)) && NamesSource::name_of(records, w.key()) == Some(name)).unwrap_or_else(|| panic!("missing fixture fn {name}"))
}

fn run(records: Src, frame: &'static blaze_mem::Frame, coverage: Coverage) -> BodyOut<'static> {
    let key = function(records, "outer").key();
    let planned = Planned { name: Name::new(Kind::BodyCheck, key.0), phase: Phase::new(Stage::Bodies, 0), source: ABSENT, file: Name::new(Kind::FileItems, ABSENT) };
    let task = Task { planned, key: Key(ABSENT) };
    let recorder = Recorder::new(RecorderOpts { alloc: a() });
    let tables = Tables::new(TablesOpts { records, impls: None, lang_crates: &[CORE_AT], reads: &recorder });
    let body = BodyFrame { frame, alloc: a(), plant_drop: None, cost: None, needs: NeedsMet::All, heads: None, sources: None, goals: None, prepared: None };
    let shared = blaze_own::testing::heap().shared();
    match coverage {
        Coverage::WithLocals => check_body(tables, &task, &body, &Unchanged, shared),
        Coverage::RootOnly => check_body_in(tables, &task, &body, &Unchanged, shared, Coverage::RootOnly),
    }
    .unwrap_or_else(|e| panic!("checking outer: {e}"))
}

fn type_errors(out: &BodyOut<'_>, paths: &[Option<&str>]) {
    assert!(!out.done.failed, "checking stopped: {:?}", out.said);
    assert_eq!(out.done.findings as usize, paths.len(), "{:?}", out.said);
    assert_eq!(out.said.len(), paths.len(), "{:?}", out.said);
    for (line, path) in out.said.iter().zip(paths) {
        assert!(matches!(line.what, SaidWhat::Type { code: "E0308" }), "{line:?}");
        assert_eq!(line.within.as_ref().map(|p| p.as_str()), *path, "{line:?}");
    }
}

#[test]
fn a_local_return_type_changes_the_enclosing_verdict() {
    let (good, _, good_frame) = world_of(CLEAN);
    let (bad, _, bad_frame) = world_of(WRONG);
    let clean = run(good, good_frame, Coverage::WithLocals);
    let wrong = run(bad, bad_frame, Coverage::WithLocals);
    type_errors(&clean, &[]);
    type_errors(&wrong, &[Some("inner")]);
    let old = run(bad, bad_frame, Coverage::RootOnly);
    type_errors(&old, &[]);
    assert!(wrong.bytes.starts_with(&old.bytes) && wrong.bytes.len() > old.bytes.len());
    assert!(wrong.bytes[old.bytes.len()..].windows(5).any(|w| w == b"E0308"));
    assert_ne!(wrong.done.out, clean.done.out);
    std::println!("outer clean, outer::inner E0308, root-only control missed the error");
}

#[test]
fn a_root_without_local_fns_keeps_its_bytes() {
    let (records, _, frame) = world_of(PLAIN);
    let mut world = ProjectWorld::from_records(records, frame, &[CORE_AT], a()).expect("outer's world");
    let (verdict, borrows) = world.check_borrows(BodyRef::Free(function(records, "outer").key()), a());
    // The stored single-body format and hash domain, independent of the task's encoder.
    let old = std::format!("diagnostics {:?}\nfindings {:?}\ngaps {:?}\nrefused {:?}\nborrows {:?}\nreads {:?}\n", verdict.diagnostics, verdict.findings, verdict.gaps, verdict.refused, borrows, verdict.reads);
    let now = run(records, frame, Coverage::WithLocals);
    type_errors(&now, &[None]);
    assert_eq!(&now.bytes[..], old.as_bytes());
    assert_eq!(now.done.out, ContentId::of("blaze-ladder front2 body task out 1", old.as_bytes()));
    std::println!("outer E0308, identical root verdict bytes and digest");
}

#[test]
fn nested_fns_contribute_their_findings_and_reads() {
    let (records, _, frame) = world_of(NESTED);
    let value = super::head::cell_id(function(records, "value"));
    let out = run(records, frame, Coverage::WithLocals);
    type_errors(&out, &[Some("left::inner"), Some("right::inner")]);
    assert!(out.done.reads.iter().any(|(id, _)| *id == value));
    let old = run(records, frame, Coverage::RootOnly);
    type_errors(&old, &[]);
    assert!(!old.done.reads.iter().any(|(id, _)| *id == value));
    std::println!("outer::left::inner E0308, outer::right::inner E0308, read value's head");
}

#[test]
fn local_errors_follow_their_declaring_fn_in_either_order() {
    blaze_sim::check(blaze_sim::Config::cases(12), |tape| {
        let mut fns = ["left", "right"];
        tape.shuffle(&mut fns, "declaration order");
        let mut app = std::string::String::from("#![no_std]\npub fn outer() {");
        let mut expected = std::vec::Vec::new();
        for name in fns {
            let literal = if tape.below(2, "literal type") == 0 {
                "1u16"
            } else {
                expected.push(Some(name));
                "1u8"
            };
            use core::fmt::Write as _;
            write!(app, "fn {name}() -> u16 {{ {literal} }}").unwrap();
        }
        app.push('}');
        let (records, _, frame) = world_of(app.leak());
        type_errors(&run(records, frame, Coverage::WithLocals), &expected);
    });
    std::println!("one E0308 per local u8 return, in declaration order");
}
