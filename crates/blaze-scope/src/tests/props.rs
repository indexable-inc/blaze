//! The properties: a scope is a function of the records, whatever order and batches the
//! answers come in; and a read list is exact under an edit (sound: a module whose reads all
//! still see what they saw has the same scope; precise: a module that never asked for the
//! edited name keeps every read), with the planted-bug control of the soundness oracle.

use std::format;
use std::vec::Vec;

use blaze_id::ItemKey;
use blaze_sim::{Broke, Config, Tape};

use super::fixtures;
use super::{N, Solved, V, World, answer, solve};
use crate::resolver::Plant;
use crate::{Edition, Read, VisibilityClass};

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
/// mod b { use crate::a::x; pub fn y; } mod c { use super::*; use crate::b::*; } pub use
/// a::x; }`, and with `edit`, `fn fresh` (private, or `pub`) added to one of its modules,
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
    // The class the new item is in, as `edited`'s scope groups it.
    let class = if public {
        VisibilityClass::Public
    } else if edited == modules[0] {
        VisibilityClass::Crate
    } else {
        VisibilityClass::Restricted(edited)
    };
    for (&m, was) in old.by.iter() {
        if m == edited || was.scope.fellows.contains(&edited) {
            // Written with the edited module: valid together with it, not by reads of it.
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
            Read::Exports { module, class: c } => module == edited && c == class,
            Read::TextualEntry { .. } | Read::TextualOut { .. } | Read::DeriveHelpers { .. } => false,
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
