//! The properties: a scope is a function of the records, whatever order and batches the
//! answers come in; and a read list is exact under an edit (sound: a module whose reads all
//! still see what they saw has the same scope; precise: a module that never asked for the
//! edited name keeps every read), with the planted-bug control of the soundness oracle.

use std::format;
use std::vec::Vec;

use blaze_id::ItemKey;
use blaze_sim::{Broke, Config, Tape};

use super::fixtures;
use super::{N, Solved, V, World, answer, resolve_again, same, solve, solve_with};
use crate::resolver::Plant;
use crate::{Edition, Namespace, Read};

/// Rule: a result is a function of the records alone: shuffled answers, partial batches and
/// any order of starting modules give the scopes and read lists of the calm run.
#[test]
fn any_answer_order_and_batching_give_one_result() {
    let worlds = fixtures::all();
    let reference: Vec<Solved> = worlds
        .iter()
        .map(|w| match solve(w, None, Plant::None) {
            Ok(s) => s,
            Err(e) => panic!("the calm run failed: {e}"),
        })
        .collect();
    blaze_sim::check(Config::cases(300), |tape| {
        let i = tape.index(worlds.len(), "fixture");
        let got = solve(&worlds[i], Some(tape), Plant::None).map_err(Broke::Harness)?;
        for (m, want) in reference[i].by.iter() {
            match got.by.get(m) {
                Some(have) if have == want => {}
                Some(have) => {
                    return Err(Broke::Oracle(format!(
                        "fixture {i}: module {m:x} differs:\n calm: {want:?}\n drawn: {have:?}"
                    )));
                }
                None => return Err(Broke::Oracle(format!("fixture {i}: module {m:x} not resolved"))),
            }
        }
        Ok(())
    });
}

/// The edit world: `root { mod a { pub fn x; mod a_child { use super::fresh as got; } }
/// mod b { use crate::a::x; pub fn y; } mod c { use super::*; use crate::b::*; use fresh as
/// probed; } pub use a::x; }`, and with `edit`, `fn fresh` (private, or `pub`) added to one of its modules,
/// keyed last so every other key stays.
fn edit_world(edit: Option<(usize, bool)>) -> (World, Vec<ItemKey>) {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let a = w.module(root, "a", V::Priv);
    w.fun(a, "x", V::Pub);
    let a_child = w.module(a, "a_child", V::Priv);
    w.use_as(a_child, "super::fresh", "got", V::Priv);
    let b = w.module(root, "b", V::Priv);
    w.use_(b, "crate::a::x", V::Priv);
    w.fun(b, "y", V::Pub);
    let c = w.module(root, "c", V::Priv);
    w.glob(c, "super", V::Priv);
    w.glob(c, "crate::b", V::Priv);
    // A name looked up through c's globs: probes of b and of the root, by their records.
    w.use_as(c, "fresh", "probed", V::Priv);
    w.use_(root, "a::x", V::Pub);
    let modules = std::vec![root, a, a_child, b, c];
    if let Some((at, public)) = edit {
        w.fun(modules[at], "fresh", if public { V::Pub } else { V::Priv });
    }
    (w, modules)
}

const FRESH: N = "fresh";

/// One case of the edit property; `Err(Oracle)` names the module and which half broke.
fn edit_case(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let (before, modules) = edit_world(None);
    let at = tape.index(modules.len(), "edited module");
    let public = tape.coin("fresh is pub");
    let (after, _) = edit_world(Some((at, public)));
    let edited = modules[at];
    let old = solve(&before, Some(tape), plant).map_err(Broke::Harness)?;
    let new = solve(&after, Some(tape), plant).map_err(Broke::Harness)?;
    for (&m, was) in old.by.iter() {
        if m == edited {
            continue;
        }
        let holds = was.reads.holds(|read| match answer(&after, &new.by, *read) {
            Ok(own) => own.saw().unwrap_or(crate::Saw::Name(crate::NameAnswer::Absent)),
            Err(_) => crate::Saw::Name(crate::NameAnswer::Absent),
        });
        let now = &new.by[&m];
        if holds && now != was {
            return Err(Broke::Oracle(format!(
                "the read list holds but the scope changed: module {m:x} after `fn fresh` in {edited:x}"
            )));
        }
        let asked = was.reads.entries.iter().any(|e| match e.read {
            Read::Name { name, .. } => name == FRESH,
            // A record of edges holds no name: an item added moves no `Globs` read.
            Read::Globs { .. } | Read::TextualEntry { .. } | Read::TextualOut { .. } | Read::DeriveHelpers { .. } => false,
        });
        if !asked && !holds {
            return Err(Broke::Oracle(format!(
                "a read changed for a module that never asked for the name: module {m:x} after `fn fresh` in {edited:x}"
            )));
        }
    }
    Ok(())
}

/// Rule: a read list is exact: an item added to a module moves only the reads that asked for
/// its name (or for the class it is in), and a module whose reads all hold keeps its scope.
#[test]
fn an_edit_moves_only_the_reads_that_asked() {
    blaze_sim::check(Config::cases(200), |tape| edit_case(tape, Plant::None));
}

/// The control: a read list that forgets the reads that found nothing must fail the edit
/// property's soundness half (a child's `use super::fresh` saw nothing, and is not told).
#[test]
fn forgetting_negative_reads_is_caught() {
    blaze_sim::control(
        Config::cases(200),
        Plant::None,
        Plant::ForgetNegativeReads,
        "the read list holds but the scope changed",
        edit_case,
    );
}

/// The control: a read list that forgets the reads made walking a glob (`use fresh as
/// probed` in c, through c's globs of b and of the root) must fail the soundness half too.
#[test]
fn forgetting_glob_probes_is_caught() {
    blaze_sim::control(
        Config::cases(200),
        Plant::None,
        Plant::ForgetGlobProbes,
        "the read list holds but the scope changed",
        edit_case,
    );
}

/// The calm run of every fixture, both solvers held equal.
fn calm_reference() -> (Vec<World>, Vec<Solved>) {
    let worlds = fixtures::all();
    let reference = worlds
        .iter()
        .map(|w| solve(w, None, Plant::None).unwrap_or_else(|e| panic!("the calm run failed: {e}")))
        .collect();
    (worlds, reference)
}

/// One case of the worklist's properties: a fixture under drawn answer orders, solved by the
/// worklist under `plant`, gives the calm run's records; a quiet point with an item that would
/// still move (a missed wake) is the oracle's finding.
fn worklist_case(worlds: &[World], reference: &[Solved], tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let i = tape.index(worlds.len(), "fixture");
    let plant = match plant {
        Plant::Shuffle(_) => Plant::Shuffle(tape.next_u64("queue order")),
        other => other,
    };
    let got = match solve_with(&worlds[i], &worlds[i], Some(tape), plant) {
        Ok(s) => s,
        Err(e) if e.contains("missed a wake") => return Err(Broke::Oracle(format!("a wake was missed: fixture {i}: {e}"))),
        Err(e) => return Err(Broke::Harness(e)),
    };
    for (m, want) in reference[i].by.iter() {
        if got.by.get(m) != Some(want) {
            return Err(Broke::Oracle(format!("the worklist differs: fixture {i}: module {m:x}")));
        }
    }
    Ok(())
}

/// Rule: the worklist's result is one whatever order it takes its queue in: shuffled event
/// order (with shuffled answers) gives the calm run's records, byte for byte.
#[test]
fn any_event_order_gives_one_result() {
    let (worlds, reference) = calm_reference();
    blaze_sim::check(Config::cases(300), |tape| worklist_case(&worlds, &reference, tape, Plant::Shuffle(0)));
}

/// Rule: the parked lists are exact enough: waking only what is parked on a changed key gives
/// what waking every pending item on every change gives.
#[test]
fn waking_the_parked_gives_waking_everything() {
    let (worlds, reference) = calm_reference();
    blaze_sim::check(Config::cases(300), |tape| worklist_case(&worlds, &reference, tape, Plant::WakeAll));
}

/// The control: a worklist that drops the events on names leaves an item parked that would
/// move, and the quiet-point check catches it.
#[test]
fn a_dropped_wake_is_caught() {
    let (worlds, reference) = calm_reference();
    blaze_sim::control(
        Config::cases(300),
        Plant::None,
        Plant::DropWake,
        "a wake was missed",
        |tape, plant| worklist_case(&worlds, &reference, tape, plant),
    );
}

/// An edit of the cycle world: `fn fresh` (private or `pub`), or `pub use .. as fresh`, added
/// to one of its modules.
#[derive(Clone, Copy)]
enum CycleEdit {
    Fn { at: usize, public: bool },
    Use { at: usize },
}

/// The cycle world: `root { mod a { pub use crate::b::*; pub use crate::b::y as by; pub fn x;
/// mod a_child { use super::fresh as got; } } mod b { pub use crate::a::*; pub use crate::a::x
/// as ax; pub fn y; } mod c { use crate::a::y as cy; use crate::b::*; use fresh as probed; } }`:
/// `a` and `b` read each other, so a driver solves them as a group (with more modules, as its
/// stack has them), and `c` reads names through both.
fn cycle_world(edit: Option<CycleEdit>) -> (World, Vec<ItemKey>) {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let a = w.module(root, "a", V::Pub);
    w.glob(a, "crate::b", V::Pub);
    w.use_as(a, "crate::b::y", "by", V::Pub);
    w.fun(a, "x", V::Pub);
    let a_child = w.module(a, "a_child", V::Priv);
    w.use_as(a_child, "super::fresh", "got", V::Priv);
    let b = w.module(root, "b", V::Pub);
    w.glob(b, "crate::a", V::Pub);
    w.use_as(b, "crate::a::x", "ax", V::Pub);
    w.fun(b, "y", V::Pub);
    let c = w.module(root, "c", V::Priv);
    w.use_as(c, "crate::a::y", "cy", V::Priv);
    w.glob(c, "crate::b", V::Priv);
    w.use_as(c, "fresh", "probed", V::Priv);
    let modules = std::vec![root, a, a_child, b, c];
    match edit {
        None => {}
        Some(CycleEdit::Fn { at, public }) => {
            w.fun(modules[at], "fresh", if public { V::Pub } else { V::Priv });
        }
        Some(CycleEdit::Use { at }) => {
            let from = if modules[at] == a { "crate::b::y" } else { "crate::a::x" };
            w.use_as(modules[at], from, "fresh", V::Pub);
        }
    }
    (w, modules)
}

/// One case of the schedule property: the cycle world solved, an edit drawn, then resolved
/// again under two drawn schedules (each its own orders and batches): both
/// give the records and read lists of a cold fill of the edited world, byte for byte.
fn schedule_case(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let (before, modules) = cycle_world(None);
    let at = tape.index(modules.len(), "edited module");
    let edit = if tape.coin("a use, not a fn") {
        CycleEdit::Use { at }
    } else {
        CycleEdit::Fn { at, public: tape.coin("fresh is pub") }
    };
    let (after, _) = cycle_world(Some(edit));
    let old = solve(&before, Some(tape), plant).map_err(Broke::Harness)?;
    let cold = solve(&after, Some(tape), plant);
    let one = resolve_again(&before, &after, &old, &[modules[at]], Some(tape), plant).map(|a| a.solved);
    let two = resolve_again(&before, &after, &old, &[modules[at]], Some(tape), plant).map(|a| a.solved);
    same(&one, &cold).map_err(|e| Broke::Oracle(format!("one schedule differs from the cold fill: {e}")))?;
    same(&two, &cold).map_err(|e| Broke::Oracle(format!("another schedule differs from the cold fill: {e}")))?;
    Ok(())
}

/// Rule (deterministic output): a record says nothing of the schedule it was made in. The same
/// edit resolved again under two schedules gives byte-equal records and read lists, equal to
/// a cold fill; inside a cycle too, where which modules are solved together is the schedule's.
#[test]
fn an_edit_under_any_schedule_gives_the_cold_records() {
    blaze_sim::check(Config::cases(200), |tape| schedule_case(tape, Plant::None));
}

/// `root { mod c { pub fn x; } mod p { pub use crate::<from>::x; } mod q { pub use
/// crate::p::x; } }`: with `from` `c`, `p` and `q` bind `c::x`; with `q`, they import each
/// other's `x` and nothing binds it.
fn circular_world(from: &'static str) -> (World, [ItemKey; 4]) {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let c = w.module(root, "c", V::Pub);
    w.fun(c, "x", V::Pub);
    let p = w.module(root, "p", V::Pub);
    w.use_(p, from, V::Pub);
    let q = w.module(root, "q", V::Pub);
    w.use_(q, "crate::p::x", V::Pub);
    (w, [root, c, p, q])
}

/// Rule: resolving again justifies nothing by itself. An edit that makes two imports read each
/// other leaves both unresolved, as a cold fill does, though each one's old record still sees
/// the target the other's now would give (the read lists hold by value; the binding they
/// describe has no ground left).
#[test]
fn an_edit_that_closes_an_import_cycle_gives_the_cold_records() {
    let (before, _) = circular_world("crate::c::x");
    let (after, [_, _, p, q]) = circular_world("crate::q::x");
    let old = solve(&before, None, Plant::None).unwrap_or_else(|e| panic!("the first fill failed: {e}"));
    let cold = solve(&after, None, Plant::None);
    assert!(cold.as_ref().is_ok_and(|s| s.def(q, Namespace::Values, "x").is_none()), "the cold fill binds q::x");
    let again = resolve_again(&before, &after, &old, &[p], None, Plant::None);
    if let Err(e) = same(&again.map(|a| a.solved), &cold) {
        panic!("resolved again, the records differ from the cold fill: {e}");
    }
    // The control: keeping every record whose reads hold keeps `q`'s, justified only by `p`'s
    // old import, and both bind `c::x`.
    let kept = resolve_again(&before, &after, &old, &[p], None, Plant::KeepSuspects).map(|a| a.solved);
    assert!(same(&kept, &cold).is_err(), "the control: keeping the suspects gave the cold records");
}

/// Rule: the widening is precise: a `pub use` of a new name added inside the cycle reruns only
/// its module, every kept record reading nothing it touches.
#[test]
fn a_new_name_in_a_cycle_reruns_its_module_alone() {
    let (before, modules) = cycle_world(None);
    let a = modules[1];
    let (after, _) = cycle_world(Some(CycleEdit::Use { at: 1 }));
    let old = solve(&before, None, Plant::None).unwrap_or_else(|e| panic!("the first fill failed: {e}"));
    let again = resolve_again(&before, &after, &old, &[a], None, Plant::None).unwrap_or_else(|e| panic!("{e}"));
    let cold = solve(&after, None, Plant::None);
    if let Err(e) = same(&Ok(again.solved), &cold) {
        panic!("resolved again, the records differ from the cold fill: {e}");
    }
    assert!(again.widened.is_empty(), "kept records were solved again as suspects: {:?}", again.widened);
}

/// The control: a group's members that leave their reads of each other out of their read
/// lists (what the group read live, valid together) are not rerun when an edit reaches them
/// through a fellow, and the schedule property catches it.
#[test]
fn fellow_reads_left_out_are_caught() {
    blaze_sim::control(
        Config::cases(200),
        Plant::None,
        Plant::FellowReadsLeftOut,
        "differs from the cold fill",
        schedule_case,
    );
}
