//! The seam in the names library's per-crate entry (`resolve_one`), feature `cells` only: after
//! the old core's fill, the cell core solves the SAME crate from the same facts (the crate's
//! files, the modules and fragments its expansions wrote as the fill made them, the
//! dependencies' records through [`BuiltDeps`]), timed, and its answers are compared with the
//! old core's scopes. The records stay the old core's: this is a measurement, not the switch
//! (the cell core does not drive expansion or load macro-written files yet). One line per crate
//! on stderr, `cells seam: ..`; deleted with the old core.

use core::cell::Cell;
use core::time::Duration;

use blaze_id::{CrateItemKey, ItemKey};
use blaze_hash::HashMap;
use blaze_own::{AVec, Alloc, FixedVec};
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

/// What turns the shadow on, given by the caller that turns the `cells` feature on (through
/// [`crate::task::Par::cells_seam`]): the clock the two cores are timed by, and where each
/// crate's report goes. A library never prints: the caller does, from the typed report.
pub trait Seam: Sync {
    /// The time since a start of the caller's choosing.
    fn now(&self) -> Duration;

    /// One crate's shadow: what the cell core answered against the old core, or why it refused.
    fn report(&self, krate: &str, shadowed: Result<Shadowed, ShadowRefused>);
}

/// The most differing bindings a report names; the rest are only counted.
pub const DIFFER_SHOWN: usize = 12;

/// One binding the two cores answer differently: the old core's binding, and the def the cell
/// core found (`None`: not found, or not decided).
#[derive(Clone, Copy, Debug)]
pub struct Differ {
    pub module: ItemKey,
    pub ns: Namespace,
    pub old: blaze_surface::Binding,
    pub cells: Option<blaze_surface::Def>,
}

/// One crate's shadow run: times, and the bindings by agreement (bottom.md section 6).
#[derive(Debug)]
pub struct Shadowed {
    pub old_fill: Duration,
    pub cells_load: Duration,
    pub cells_solve: Duration,
    pub equal: u64,
    pub differ: u64,
    /// Bindings the cell core holds as outside what it follows: neither equal nor different.
    pub unknown: u64,
    pub old_errors: u64,
    pub failed: u64,
    pub outside: u64,
    pub undecided: u64,
    pub dep_reads: u64,
    pub dep_absent: u64,
    pub not_expanded: u64,
    pub first_not_expanded: Option<ItemKey>,
    pub cells: u64,
    pub stalls: u64,
    /// The first [`DIFFER_SHOWN`] differing bindings, in module key order.
    pub differ_first: FixedVec<Differ, DIFFER_SHOWN>,
}

/// The cell core refused a crate: at its load or its run.
#[derive(Debug)]
pub enum ShadowRefused {
    Load(super::CellError),
    Run(super::CellError),
}

/// Solves a crate with the cell core over `facts` and compares with the old core's `done`; the
/// old fill took `old`, and the cell core is timed by `seam`'s clock.
pub(crate) fn shadow<'a, F: ScopeFacts<Name = &'a str>>(
    facts: &F,
    built: &Built<'_, 'a>,
    root: ItemKey,
    done: &HashMap<ItemKey, Resolved<'a, &'a str>, Alloc<'a>>,
    old: Duration,
    seam: &dyn Seam,
    alloc: Alloc<'a>,
) -> Result<Shadowed, ShadowRefused> {
    let deps = BuiltDeps { built, alloc };
    let facts = Facts { inner: facts, not_expanded: Cell::new(0), first: Cell::new(None), alloc };
    let start = seam.now();
    let mut c = Cells::load_with(&facts, root, Some(&deps), alloc).map_err(ShadowRefused::Load)?;
    let loaded = seam.now().saturating_sub(start);
    c.run(&facts, &|_, _| {}, &[]).map_err(ShadowRefused::Run)?;
    let solved = seam.now().saturating_sub(start);
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
    let mut differ_first = FixedVec::new();
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
            let cells = match got {
                Some(Value::Found(d, _) | Value::Ambiguous(d, _)) => Some(d),
                _ => None,
            };
            // Full past the first DIFFER_SHOWN: the rest are counted in `differ` only.
            _ = differ_first.push(Differ { module: m, ns: e.ns, old: e.binding, cells });
        }
    }
    Ok(Shadowed {
        old_fill: old,
        cells_load: loaded,
        cells_solve: solved.saturating_sub(loaded),
        equal,
        differ,
        unknown,
        old_errors,
        failed,
        outside,
        undecided,
        dep_reads: c.stats.dep_reads as u64,
        dep_absent: c.stats.dep_absent as u64,
        not_expanded: facts.not_expanded.get() as u64,
        first_not_expanded: facts.first.get(),
        cells: c.stats.cells as u64,
        stalls: c.stats.stalls as u64,
        differ_first,
    })
}
