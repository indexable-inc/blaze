//! The seam in the names library's per-crate entry (`resolve_one`), feature `cells` only: after
//! the old core's fill, the cell core solves the SAME crate from the same facts (the crate's
//! files, the modules and fragments its expansions wrote as the fill made them, the
//! dependencies' records through [`BuiltDeps`]), timed, and its answers are compared with the
//! old core's scopes. The records stay the old core's: this is a measurement, not the switch
//! (the cell core does not drive expansion or load macro-written files yet). One line per crate
//! on stderr, `cells seam: ..`; deleted with the old core.

use core::cell::Cell;
use std::time::{Duration, Instant};

use blaze_id::{CrateItemKey, ItemKey};
use blaze_hash::HashMap;
use blaze_own::{AVec, Alloc};
use blaze_surface::{CrateRootBuilder, DeclsBuilder, ExpandedDecls, ModuleInfo, SurfaceError};

use super::{BuiltDeps, Cells, Outcome, Value};
use crate::facts::ScopeFacts;
use crate::resolve::Built;
use crate::scope::Resolved;
use crate::{BindingState, Namespace};

/// The fill's facts, with a call the fill did not expand (one the old core resolves otherwise
/// or not at all) counted and named, and read as adding nothing: a difference of the
/// comparison, never a stop of the measurement.
struct Facts<'f, 'a, F> {
    inner: &'f F,
    not_expanded: Cell<u64>,
    first: Cell<Option<ItemKey>>,
    alloc: Alloc<'a>,
}

impl<'a, F: ScopeFacts<Name = &'a str>> ScopeFacts for Facts<'_, 'a, F> {
    type Name = &'a str;

    fn module(&self, module: ItemKey) -> Result<ModuleInfo<&'a str>, SurfaceError> {
        self.inner.module(module)
    }

    fn decls(&self, module: ItemKey) -> Result<DeclsBuilder<'_, &'a str>, SurfaceError> {
        self.inner.decls(module)
    }

    fn crate_root(&self, krate: CrateItemKey) -> Result<CrateRootBuilder<'_, &'a str>, SurfaceError> {
        self.inner.crate_root(krate)
    }

    fn expansion(&self, call: ItemKey) -> Result<Option<ExpandedDecls<'_, &'a str>>, SurfaceError> {
        match self.inner.expansion(call)? {
            Some(d) => Ok(Some(d)),
            None => {
                self.not_expanded.set(self.not_expanded.get() + 1);
                if self.first.get().is_none_or(|f| call < f) {
                    self.first.set(Some(call));
                }
                Ok(Some(DeclsBuilder::new_in(self.alloc)))
            }
        }
    }
}

/// Solves `krate` with the cell core over `facts` and compares with the old core's `done`; the
/// old fill took `old`.
pub(crate) fn shadow<'a, F: ScopeFacts<Name = &'a str>>(
    facts: &F,
    built: &Built<'_, 'a>,
    krate: &str,
    root: ItemKey,
    done: &HashMap<ItemKey, Resolved<'a, &'a str>, Alloc<'a>>,
    old: Duration,
    alloc: Alloc<'a>,
) {
    let deps = BuiltDeps { built, alloc };
    let facts = Facts { inner: facts, not_expanded: Cell::new(0), first: Cell::new(None), alloc };
    let start = Instant::now();
    let mut c = match Cells::load_with(&facts, root, Some(&deps), alloc) {
        Ok(c) => c,
        Err(e) => {
            std::eprintln!("cells seam: {krate}: load refused: {e}");
            return;
        }
    };
    let loaded = start.elapsed();
    if let Err(e) = c.run(&facts, &|_, _| {}, &[]) {
        std::eprintln!("cells seam: {krate}: run refused: {e}");
        return;
    }
    let solved = start.elapsed();
    // The answers: imports and calls by outcome.
    let (mut failed, mut outside, mut undecided) = (0u64, 0u64, 0u64);
    for r in c.results() {
        match r.outcome {
            Some(Outcome::Failed(_)) => failed += 1,
            Some(Outcome::Outside) => outside += 1,
            None => undecided += 1,
            Some(_) => {}
        }
    }
    // Each binding of the old core's scopes, asked of the cells (a later reader), in key order.
    let mut keys: AVec<'a, ItemKey> = AVec::with_capacity_in(done.len(), alloc);
    keys.extend(done.keys().copied());
    keys.sort_unstable();
    let (mut equal, mut differ, mut unknown, mut old_errors) = (0u64, 0u64, 0u64, 0u64);
    let mut shown = 0;
    for &m in keys.iter() {
        let Some(resolved) = done.get(&m) else { continue };
        let scope = &resolved.scope;
        old_errors += scope.errors.len() as u64;
        for e in scope.bindings.iter() {
            // `None`: the cell core could not decide it (a refusal or an undecided cell).
            let got = c.ask(m, e.ns, e.name).ok().flatten();
            let same = match (got, e.binding.state) {
                (Some(Value::Found(d, _)), BindingState::Determined) => d == e.binding.def,
                (Some(Value::Ambiguous(d, _)), BindingState::Ambiguous(_)) => d == e.binding.def,
                (Some(Value::Outside), _) => {
                    unknown += 1;
                    continue;
                }
                _ => false,
            };
            if same {
                equal += 1;
                continue;
            }
            differ += 1;
            if shown < 12 {
                shown += 1;
                let ns = match e.ns {
                    Namespace::Types => "type",
                    Namespace::Values => "value",
                    Namespace::Macros => "macro",
                };
                std::eprintln!("cells seam: {krate}: differ {m:x} {ns} {}: old {:?} {:?}, cells {got:?}", e.name, e.binding.def, e.binding.state);
            }
        }
    }
    std::eprintln!(
        "cells seam: {krate}: old fill {:.2} ms, cells load {:.2} ms + solve {:.2} ms; bindings equal {equal}, differ {differ}, cells unknown {unknown}; \
         old errors {old_errors}, cells failed {failed}, outside {outside}, undecided {undecided}; dep reads {}, dep absent {}; \
         calls the fill did not expand {} (first {:?}); cells {}, stalls {}",
        old.as_secs_f64() * 1e3,
        loaded.as_secs_f64() * 1e3,
        (solved - loaded).as_secs_f64() * 1e3,
        c.stats.dep_reads,
        c.stats.dep_absent,
        facts.not_expanded.get(),
        facts.first.get(),
        c.stats.cells,
        c.stats.stalls,
    );
}
