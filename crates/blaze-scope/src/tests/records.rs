//! The machine's inputs and outputs as records: a scope it resolves writes to a `ModuleScope`
//! record and decodes back to the same working form, and the machine reading its facts from
//! the world's records (through [`RecordFacts`]) resolves what it resolves from the working
//! forms the tests wrote.

use std::alloc::Global;
use std::boxed::Box;
use std::format;
use std::string::String;
use std::vec::Vec;

use blaze_id::{CrateItemKey, ItemKey};
use blaze_surface::testing::{Fact, MemSurface};
use blaze_surface::{Items, NotBuilt, SurfaceError};

use super::{World, fixtures, solve, solve_with};
use crate::resolver::Plant;
use crate::{RecordFacts, ResolveError, ScopeBuilder};

/// Every record of `w`'s modules and crates: one surface per crate.
fn surfaces(w: &World) -> Result<Vec<(CrateItemKey, MemSurface<Global>)>, String> {
    let mut out: Vec<(CrateItemKey, MemSurface<Global>)> = Vec::new();
    for (&krate, cr) in w.roots.iter() {
        let mut mem = MemSurface::new_in(krate, Global);
        let bytes = cr.write(Global).map_err(|e| format!("the CrateRoot of {krate:x}: {e}"))?;
        mem.insert(Fact::CrateRoot, &bytes).map_err(|e| format!("the CrateRoot of {krate:x}: {e}"))?;
        out.push((krate, mem));
    }
    for (&module, info) in w.modules.iter() {
        let bytes = w.decls[&module]
            .write(module, info, Global)
            .map_err(|e| format!("the ModuleDecls of {module:x}: {e}"))?;
        let Some((_, mem)) = out.iter_mut().find(|(krate, _)| module.is_of(*krate)) else {
            return Err(format!("module {module:x} is of no crate of the world"));
        };
        mem.insert(Fact::ModuleDecls, &bytes).map_err(|e| format!("the ModuleDecls of {module:x}: {e}"))?;
    }
    Ok(out)
}

/// Rule: a resolved scope survives its record: write, read, `from` gives the scope the machine
/// made, for every module of every fixture.
#[test]
fn every_resolved_scope_round_trips_through_its_record() {
    for (i, w) in fixtures::all().iter().enumerate() {
        let solved = solve(w, None, Plant::None).unwrap_or_else(|e| panic!("fixture {i}: {e}"));
        let mut by_crate = surfaces(w).unwrap_or_else(|e| panic!("fixture {i}: {e}"));
        for (&module, resolved) in solved.by.iter() {
            let bytes = resolved.scope.write(Global).unwrap_or_else(|e| panic!("fixture {i}: {module:x}: {e}"));
            let Some((_, mem)) = by_crate.iter_mut().find(|(krate, _)| module.is_of(*krate)) else {
                panic!("fixture {i}: module {module:x} is of no crate");
            };
            mem.insert(Fact::ModuleScope, &bytes).unwrap_or_else(|e| panic!("fixture {i}: {module:x}: {e}"));
            let view = mem.scope(module).unwrap_or_else(|e| panic!("fixture {i}: {module:x}: {e}"));
            let back = ScopeBuilder::from(&view, Global);
            assert!(resolved.scope == back, "fixture {i}: module {module:x}:\n made: {:?}\n read: {back:?}", resolved.scope);
            // The same scope written again is the same bytes.
            let again = back.write(Global).unwrap_or_else(|e| panic!("fixture {i}: {module:x}: {e}"));
            assert!(again[..] == bytes[..], "fixture {i}: module {module:x}: a second write differs");
        }
    }
}

/// Rule: the facts read from records are the facts the tests wrote. Each one-crate fixture
/// resolves to the same scopes and read lists through [`RecordFacts`] as through the working
/// forms; one with a macro call stops on the expansion record not built, never on a guess.
#[test]
fn record_facts_resolve_as_the_working_forms_do() {
    let mut through_records = 0usize;
    for (i, w) in fixtures::all().iter().enumerate() {
        if w.roots.len() != 1 {
            continue;
        }
        let mut by_crate = surfaces(w).unwrap_or_else(|e| panic!("fixture {i}: {e}"));
        let (_, mem) = by_crate.pop().unwrap_or_else(|| panic!("fixture {i}: one crate"));
        // `RecordFacts` names are the records' text, borrowed for the surface's life: the test
        // keeps the surface for the whole run.
        let mem: &'static MemSurface<Global> = Box::leak(Box::new(mem));
        let facts = RecordFacts::new_in(mem, Global);
        let want = solve(w, None, Plant::None).unwrap_or_else(|e| panic!("fixture {i}: {e}"));
        match solve_with(w, &facts, None, Plant::None) {
            Ok(got) => {
                assert!(got.by == want.by, "fixture {i}: the records resolve otherwise");
                through_records += 1;
            }
            Err(e) => {
                // Only a resolved macro call may stop it, on its expansion (the driver shows
                // the typed error as text).
                let refused = |(&call, &module): (&ItemKey, &ItemKey)| {
                    let error = SurfaceError::NotBuilt(NotBuilt { method: "expansion", item: call });
                    e == format!("{}", ResolveError::Surface { module, error })
                };
                assert!(w.owner.iter().any(refused), "fixture {i}: refused otherwise: {e}");
            }
        }
    }
    // The one-crate fixtures with no macro call (named, shadow, two globs, chain, super glob,
    // private, restricted, failed import) at least.
    assert!(through_records >= 8, "only {through_records} fixtures went through records");
}

/// The control: a record that lost a fact resolves otherwise, so the equality above can fail.
#[test]
fn a_record_missing_a_declaration_is_seen() {
    let (w, k) = fixtures::named();
    let mut by_crate = surfaces(&w).unwrap_or_else(|e| panic!("{e}"));
    let (_, mut mem) = by_crate.pop().unwrap_or_else(|| panic!("one crate"));
    // The root's record written without its `use a::foo as bar;`.
    let mut cut = w.decls[&k.root].clone();
    cut.uses.truncate(1);
    let bytes = cut.write(k.root, &w.modules[&k.root], Global).unwrap_or_else(|e| panic!("{e}"));
    mem.insert(Fact::ModuleDecls, &bytes).unwrap_or_else(|e| panic!("{e}"));
    let mem: &'static MemSurface<Global> = Box::leak(Box::new(mem));
    let facts = RecordFacts::new_in(mem, Global);
    let want = solve(&w, None, Plant::None).unwrap_or_else(|e| panic!("{e}"));
    let got = solve_with(&w, &facts, None, Plant::None).unwrap_or_else(|e| panic!("{e}"));
    assert!(got.by != want.by, "a record without `bar` resolved as the whole module");
    let bar = |by: &std::collections::BTreeMap<ItemKey, crate::Resolved<super::N, Global>>| {
        by[&k.root].scope.get(crate::Namespace::Values, "bar").map(|b| b.def.key)
    };
    assert_eq!(bar(&want.by), Some(k.foo));
    assert_eq!(bar(&got.by), None);
}
