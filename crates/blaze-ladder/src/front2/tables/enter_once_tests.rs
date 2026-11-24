//! D161 ("a task that lacks a fact waits on it; nothing starts over"), D197 root 2: the cost of a
//! CHAIN of needs a body meets only while it types it, in miniature. On tempo the run path's
//! check of `tempo_main_with` made 2,942 records over 256 rounds, one new hole a round, and
//! stopped at the cap (rt-loop a1850, check.rs `RoundHoles`).
//!
//! THE WORLD: unit structs `T0 ..= TN`, `impl Ti { pub fn next(self) -> T(i+1) }`, and
//! `pub fn chain() -> TN { T0.next().next() .. }`: link i+1's method is found only once link i's
//! return type is known, so each link's inherent answer and signature are needs met in order.
//!
//! WHAT "CLEAN" IS HERE: the body's TYPES checked clean (`BodyReads::typed` kept, task.rs
//! `keeps_typed`), which is what this test is about. The task's `failed` also counts a refused
//! borrow check, and over this fixture every body that holds an ADT has one: the fixture's
//! `core` (head_tests.rs `CORE`) declares no `copy` or `drop` lang item, so whether `Ti` is
//! `Copy` is unknown (borrows.rs `has_impl`: `ImplsUnknown::NoLangItem`) and the borrow check
//! refuses the body (`BorrowRefused::FactUnknown`), on both sides alike. That, not the mode,
//! was the first run's "did not check clean" (2026-11-21).
//!
//! - THE RESTART PATH (the control): a lowering store (`BodyFrame::heads` `None`) under
//!   `AsksMet::Again`. Each round meets the next link's hole, reads its stand-in (no inherent
//!   item: the receivers after it are the error type, so nothing further is asked), ends, and
//!   the loop makes that link: a round or more a link, so 300 links stop at `MOST_ROUNDS`
//!   (256) with `CheckFail::Rounds`.
//! - THE ONE ASK: frozen heads (`Freezing`) under `AsksMet::Once`. Every link is made on the
//!   spot through the store's board and the run stops at no hole (asks.rs
//!   `Store::stops_at_holes`): ONE entry, zero entries again, every link a need served.
//! - EQUAL (task.rs `asks_agree`: equal verdict bytes, the one ask's read list a subset of the
//!   restart path's) at 40 links, under the cap at up to six rounds a link; at 100 links where
//!   the restart path stays under its cap, else that path's stop at the cap is said and checked.
//!   Every head the one ask's store holds after the check is in its read list.

use core::fmt::Write as _;

use blaze_drive::key::{ABSENT, Key, Kind, Name};
use blaze_engine::{Phase, Planned, Stage, Task};
use blaze_id::ItemKey as LibKey;

use super::cost::{Cost, Gauge};
use super::head::{SeededHeads, SeededHeadsOpts};
use super::head_tests::{CORE_AT, Src, a, cells, world_of};
use super::recorder::{Recorder, RecorderOpts};
use super::seeds::{Freezing, HeadSeed};
use super::task::{BodyFrame, BodyReads, record_body_met};
use super::view::{Tables, TablesOpts};
use crate::front2::asks::{AsksMet, AsksPlant};
use crate::front2::check::MOST_ROUNDS;
use crate::front2::heads::{NeedsMet, Want};
use crate::front2::source::NamesSource;

/// A gauge that reads nothing: the meter here counts rounds and entries, not time or bytes.
struct Still;

impl Gauge for Still {
    fn now_ns(&self) -> u64 {
        0
    }

    fn bytes(&self) -> u64 {
        0
    }
}

static STILL: Still = Still;

/// Which side of D161's seam a run is on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Side {
    /// A lowering store under `AsksMet::Again`: today's restart path.
    Restart,
    /// Frozen heads under `AsksMet::Once`: the one ask, stopping at no stand-in.
    Once,
}

/// One run of `chain`'s body: the task's answer and what its meter counted.
struct Run {
    side: Side,
    links: usize,
    body: BodyReads<'static>,
    /// Check rounds entered (tables/cost.rs `Spent::rounds`).
    rounds: u64,
    /// Rounds that ended in a need, so the body was entered again (`Census::entered_again`).
    again: u64,
}

impl Run {
    /// The body's types checked clean (the module doc's "clean").
    fn types_clean(&self) -> bool {
        self.body.typed.is_some()
    }

    /// Everything a failure's message needs: the side, the meter, the body's said lines (a
    /// refusal's words included) and its verdict (its facts as one digest).
    fn diagnosis(&self) -> std::string::String {
        let mut s = std::string::String::new();
        let _ = writeln!(s, "{:?} on a chain of {}: {} rounds, {} entered again, task failed {}, types clean {}, {} needs served", self.side, self.links, self.rounds, self.again, self.body.failed, self.types_clean(), served(&self.body));
        for said in self.body.said.iter() {
            let _ = writeln!(s, "said: {:?} at {}..{}: {}", said.what, said.start, said.end, said.words.as_str());
        }
        let verdict = core::str::from_utf8(self.body.verdict_bytes()).unwrap_or("(the verdict text is not UTF-8)");
        // Every line, the facts' digest (`reads Some(..)`) included: it is one id, not the list.
        for line in verdict.lines() {
            let _ = writeln!(s, "verdict: {line}");
        }
        s
    }
}

/// The world of a chain of `links` links (the module doc).
fn chain_text(links: usize) -> &'static str {
    let mut s = std::string::String::from("#![no_std]\n");
    for i in 0..=links {
        let _ = writeln!(s, "pub struct T{i};");
    }
    for i in 0..links {
        let _ = writeln!(s, "impl T{i} {{\n    pub fn next(self) -> T{n} {{\n        T{n}\n    }}\n}}", n = i + 1);
    }
    let _ = write!(s, "pub fn chain() -> T{links} {{\n    T0");
    for _ in 0..links {
        s.push_str(".next()");
    }
    s.push_str("\n}\n");
    s.leak()
}

/// The body task of fn `name` of the world.
fn task_of(names: Src, name: &str) -> Task {
    let key = cells(names)
        .iter()
        .copied()
        .find(|w| matches!(w, Want::Fn(_)) && NamesSource::name_of(names, w.key()) == Some(name))
        .unwrap_or_else(|| panic!("the chain's world declares no fn `{name}`"))
        .key();
    let planned = Planned { name: Name::new(Kind::BodyCheck, key.0), phase: Phase::new(Stage::Bodies, 0), source: ABSENT, file: Name::new(Kind::FileItems, ABSENT) };
    Task { planned, key: Key(ABSENT) }
}

/// One chain's world (head_tests.rs `world_of`), shared by both sides' runs.
type World = (Src, Option<LibKey>, &'static blaze_mem::Frame);

/// The world of a chain of `links`.
fn chain(links: usize) -> World {
    world_of(chain_text(links))
}

/// `chain`'s body of a chain of `links` (its world `world`), checked on `side`, metered.
fn run(world: World, links: usize, side: Side) -> Run {
    let (names, sized, frame) = world;
    let cost: &'static Cost<'static> = blaze_own::Box::leak_static(blaze_own::Box::new_in(Cost::new(&STILL), a()));
    let heads: Option<&'static dyn HeadSeed<'static>> = match side {
        Side::Restart => None,
        Side::Once => {
            let opts = SeededHeadsOpts { names, sized, pointers: crate::front2::receiver::Pointers::NOT_LOOKED_UP, frame, scratch: a(), alloc: a(), consts: None, known: None, owners: None };
            let freezing: &'static Freezing<'static, 'static, Src> = blaze_own::Box::leak_static(blaze_own::Box::new_in(Freezing::new(names, SeededHeads::new(opts)), a()));
            Some(freezing as &'static dyn HeadSeed<'static>)
        }
    };
    let met = match side {
        Side::Restart => AsksMet::Again,
        Side::Once => AsksMet::Once(AsksPlant::Clean),
    };
    let body_frame = BodyFrame { frame, alloc: a(), plant_drop: None, cost: Some(cost), needs: NeedsMet::First, heads, sources: None, goals: None, prepared: None };
    let recorder = Recorder::new(RecorderOpts { alloc: a() });
    let tables = Tables::new(TablesOpts { records: names, impls: None, lang_crates: &[CORE_AT], reads: &recorder });
    let body = record_body_met(tables, &task_of(names, "chain"), &body_frame, a(), met).unwrap_or_else(|e| panic!("{side:?} on a chain of {links}: the chain's body task refused: {e}"));
    let spent = cost.spent();
    Run { side, links, body, rounds: spent.rounds, again: spent.census.entered_again }
}

/// The needs the one ask served (made) in the body's store, every fact.
fn served(body: &BodyReads<'_>) -> u64 {
    body.asked_counts().made.iter().sum()
}

/// The one ask's run: types clean, one entry, no entry again, at least a need served a link,
/// and every head its store holds is a read of the body.
fn assert_entered_once(run: &Run) {
    let body = &run.body;
    assert!(run.types_clean(), "ONE ASK: the chain's types did not check clean:\n{}", run.diagnosis());
    assert_eq!(run.rounds, 1, "ONE ASK: entered {} times (one entry is the design):\n{}", run.rounds, run.diagnosis());
    assert_eq!(run.again, 0, "ONE ASK: entered again:\n{}", run.diagnosis());
    assert!(served(body) >= run.links as u64, "ONE ASK: fewer needs served than links (each link's inherent answer is one):\n{}", run.diagnosis());
    let reads = body.reads.as_slice();
    let unread: std::vec::Vec<_> = body.held_heads().iter().filter(|(read, _)| reads.binary_search(read).is_err()).collect();
    assert!(unread.is_empty(), "ONE ASK: heads made that the body's final typing never read: {unread:?}\n{}", run.diagnosis());
}

/// The restart path's run and the one ask's agree (task.rs `asks_agree`), both types clean.
fn assert_agree(old: &Run, new: &Run) {
    assert!(old.types_clean(), "RESTART: the chain's types did not check clean:\n{}", old.diagnosis());
    assert!(
        old.body.agrees_with_once(&new.body),
        "the two sides disagree (task.rs `asks_agree`: equal findings, verdict bytes and typed record; the one ask's reads a subset), by field:\n{}{}\n{}",
        old.body.once_differs(&new.body),
        old.diagnosis(),
        new.diagnosis()
    );
}

/// The restart path stopped at its round cap: types not clean, every round the cap allows
/// entered, each but the last ending in a need.
fn assert_at_cap(old: &Run) {
    assert!(!old.types_clean(), "CONTROL: the restart path checked the chain clean under a cap of {MOST_ROUNDS} rounds:\n{}", old.diagnosis());
    assert!(old.rounds >= u64::from(MOST_ROUNDS), "CONTROL: the restart path stopped before its cap of {MOST_ROUNDS}:\n{}", old.diagnosis());
    assert!(old.again >= u64::from(MOST_ROUNDS) - 1, "CONTROL: too few rounds ended in a need:\n{}", old.diagnosis());
}

#[test]
fn a_chain_of_forty_needs_checks_alike_on_both_sides_and_the_one_ask_enters_once() {
    let world = chain(40);
    let (old, new) = (run(world, 40, Side::Restart), run(world, 40, Side::Once));
    assert_entered_once(&new);
    assert_agree(&old, &new);
    assert!(old.rounds >= 40, "CONTROL: the restart path met 40 links in fewer rounds; a round a link is what the one ask removes:\n{}", old.diagnosis());
    std::println!("chain of 40: restart {} rounds, {} reads; one ask {} entry, {} needs served, {} reads", old.rounds, old.body.reads.len(), new.rounds, served(&new.body), new.body.reads.len());
}

#[test]
fn a_chain_of_a_hundred_needs_is_one_entry_and_alike_where_the_restart_path_finishes() {
    let world = chain(100);
    let (old, new) = (run(world, 100, Side::Restart), run(world, 100, Side::Once));
    assert_entered_once(&new);
    // At more than 2.56 rounds a link the restart path cannot finish 100 links under its cap:
    // then it must have stopped AT the cap (said), else the two must agree.
    if old.rounds >= u64::from(MOST_ROUNDS) && !old.types_clean() {
        assert_at_cap(&old);
        std::println!("chain of 100: the restart path stopped at its cap after {} rounds; one ask {} entry, {} needs served", old.rounds, new.rounds, served(&new.body));
    } else {
        assert_agree(&old, &new);
        std::println!("chain of 100: restart {} rounds, {} reads; one ask {} entry, {} needs served, {} reads", old.rounds, old.body.reads.len(), new.rounds, served(&new.body), new.body.reads.len());
    }
}

#[test]
fn a_chain_of_three_hundred_needs_is_one_entry_where_the_restart_path_stops_at_its_cap() {
    let world = chain(300);
    let new = run(world, 300, Side::Once);
    assert_entered_once(&new);
    // CONTROL (rule 23): the restart path on the same body meets a link a round or more and
    // stops at the cap, tempo's `tempo_main_with` in miniature.
    let old = run(world, 300, Side::Restart);
    assert_at_cap(&old);
    std::println!("chain of 300: one ask {} entry, {} needs served; restart stopped after {} rounds", new.rounds, served(&new.body), old.rounds);
}
