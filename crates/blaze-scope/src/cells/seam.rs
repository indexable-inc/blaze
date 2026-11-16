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

/// What turns the shadow on, given by the caller that turns the `cells` feature on (through
/// [`crate::task::Par::cells_seam`]): the clock the two cores are timed by, and where each
/// crate's report goes. A library never prints: the caller does, from the typed report.
pub trait Seam: Sync {
    /// The time since a start of the caller's choosing.
    fn now(&self) -> Duration;

    /// One crate's shadow: what the cell core answered against the old core, or why it refused.
    fn report(&self, krate: &str, shadowed: Result<Shadowed<'_>, ShadowRefused>);
}

/// The most differing bindings (and unknown items, and differing written entries) a report
/// names; the rest are only counted.
pub const DIFFER_SHOWN: usize = 48;

/// One binding the two cores answer differently: the old core's binding, and the def the cell
/// core found (`None`: not found, or not decided).
#[derive(Clone, Copy, Debug)]
pub struct Differ<'a> {
    pub module: ItemKey,
    pub ns: Namespace,
    pub name: &'a str,
    pub old: blaze_surface::Binding,
    pub cells: Option<blaze_surface::Def>,
}

/// One import or call the cell core left unknown, with the form it does not follow.
#[derive(Debug)]
pub struct Unknown<'a> {
    pub module: ItemKey,
    /// A macro call's key; `None` for an import.
    pub call: Option<ItemKey>,
    pub kind: crate::PathKind,
    pub segments: AVec<'a, &'a str>,
    pub glob: bool,
    pub why: super::Unfollowed,
}

/// Which side of a written entry differs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WrittenWhich {
    OnlyOld,
    OnlyCells,
    Def,
    Vis,
    Origin,
    Expansion,
    State,
}

/// One entry of a module's written binding list the two cores write differently.
#[derive(Clone, Copy, Debug)]
pub struct WrittenDiffer<'a> {
    pub module: ItemKey,
    pub ns: Namespace,
    pub name: &'a str,
    pub which: WrittenWhich,
    pub old: Option<blaze_surface::Binding>,
    pub cells: Option<blaze_surface::Binding>,
}

/// One crate's shadow run: times, and the bindings by agreement (bottom.md section 6).
#[derive(Debug)]
pub struct Shadowed<'a> {
    /// Calls the fill expanded that the cell core did not resolve, and the first of them.
    pub lost: u64,
    pub lost_first: AVec<'a, Lost<'a>>,
    /// Unknown imports and unknown calls by [`super::Unfollowed`] class.
    pub unknown_imports: [u64; super::UNFOLLOWED],
    pub unknown_calls: [u64; super::UNFOLLOWED],
    /// The first [`DIFFER_SHOWN`] unknown items, in item order.
    pub unknown_first: AVec<'a, Unknown<'a>>,
    /// The first [`DIFFER_SHOWN`] differing written entries, in module key order.
    pub written_first: AVec<'a, WrittenDiffer<'a>>,
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
    /// The uses the cell core reports a time-travel or glob-vs-outer error at (R6, and the
    /// validation of what stalled calls took).
    pub r6_errors: u64,
    /// What stalled calls took while their module's globs were weakly undecided, validated
    /// when the run ended.
    pub speculated: u64,
    /// The first [`DIFFER_SHOWN`] differing bindings, in module key order.
    pub differ_first: AVec<'a, Differ<'a>>,
    /// The cell core's own written lists ([`super::write`]) against the old core's scopes.
    pub written: WrittenAgainst,
}

/// The scope lists the cell core writes (bindings and glob edges of each module) against the
/// old core's, by module and then by entry: what stands between the shadow and the switch.
#[derive(Clone, Copy, Default, Debug)]
pub struct WrittenAgainst {
    /// Modules of the old core's scopes the cell core has no module for (a block's module).
    pub modules_absent: u64,
    /// Modules a `mod` line names whose text the facts do not hold: bound, their inside unknown.
    pub modules_unread: u64,
    /// Modules no `mod` line names (blocks' modules), loaded after the first run.
    pub modules_later: u64,
    /// Modules with an import or glob the cell core holds as outside what it follows.
    pub modules_unknown: u64,
    pub bindings_equal: u64,
    pub bindings_differ: u64,
    pub globs_equal: u64,
    pub globs_differ: u64,
    /// In the modules whose bindings differ, entry by entry: a name only the old core binds,
    /// a name only the cell core binds, then one field of a name both bind (the first that
    /// differs, in this order).
    pub only_old: u64,
    pub only_cells: u64,
    pub def: u64,
    pub vis: u64,
    pub origin: u64,
    pub expansion: u64,
    pub state: u64,
    /// Root entries of `#[macro_export]` macros written by expansions, which the old path
    /// publishes into the root scope after its fill: held by the cells, compared with nothing.
    pub exports_published_later: u64,
}

/// A call the fill expanded that the cell core did not resolve to a macro: what it wrote is
/// missing from the cells' answers.
#[derive(Debug)]
pub struct Lost<'a> {
    pub module: ItemKey,
    pub call: ItemKey,
    pub kind: crate::PathKind,
    pub segments: AVec<'a, &'a str>,
    pub outcome: Option<Outcome>,
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
    enums: &dyn super::EnumNames<&'a str>,
    // Where each block's body is written ([`Cells::load_more_at`]).
    places: &dyn Fn(ItemKey) -> Option<(ItemKey, u32)>,
    root: ItemKey,
    done: &HashMap<ItemKey, Resolved<'a, &'a str>, Alloc<'a>>,
    old: Duration,
    seam: &dyn Seam,
    alloc: Alloc<'a>,
) -> Result<Shadowed<'a>, ShadowRefused> {
    let deps = BuiltDeps { built, alloc };
    let facts = Facts { inner: facts, not_expanded: Cell::new(0), first: Cell::new(None), alloc };
    let start = seam.now();
    let mut c = Cells::load_with(&facts, root, Some(&deps), alloc).map_err(ShadowRefused::Load)?;
    c.enums = Some(enums);
    let loaded = seam.now().saturating_sub(start);
    c.run(&facts, &|_, _| {}, &[]).map_err(ShadowRefused::Run)?;
    // The modules no `mod` line names (a block's module): the fill has made them all by now,
    // and nothing outside one reads into it, so they are solved after the rest.
    let mut extra: AVec<'a, ItemKey> = AVec::new_in(alloc);
    extra.extend(done.keys().copied().filter(|k| !c.by_key.contains_key(k)));
    extra.sort_unstable();
    let blocks = c.load_more_at(&facts, &extra, places).map_err(ShadowRefused::Load)?;
    if blocks > 0 {
        c.run(&facts, &|_, _| {}, &[]).map_err(ShadowRefused::Run)?;
    }
    let solved = seam.now().saturating_sub(start);
    // The answers: imports and calls by outcome.
    let (mut failed, mut outside, mut undecided) = (0u64, 0u64, 0u64);
    let (mut unknown_imports, mut unknown_calls) = ([0u64; super::UNFOLLOWED], [0u64; super::UNFOLLOWED]);
    let mut unknown_first: AVec<'a, Unknown<'a>> = AVec::new_in(alloc);
    let (mut lost, mut lost_first): (u64, AVec<'a, Lost<'a>>) = (0, AVec::new_in(alloc));
    for r in c.results() {
        if let Some(call) = r.call
            && !matches!(r.outcome, Some(Outcome::Called(_)))
            && matches!(facts.inner.expansion(call), Ok(Some(_)))
        {
            lost += 1;
            if lost_first.len() < DIFFER_SHOWN {
                let mut segments = AVec::with_capacity_in(r.segments.len(), alloc);
                segments.extend_from_slice(r.segments);
                lost_first.push(Lost { module: r.module, call, kind: r.kind, segments, outcome: r.outcome });
            }
        }
        match r.outcome {
            Some(Outcome::Failed(_)) => failed += 1,
            Some(Outcome::Outside) => {
                outside += 1;
                // Set wherever the outcome is: `Through` stands for one a stall decided.
                let why = r.why.unwrap_or(super::Unfollowed::Through);
                if r.call.is_some() {
                    unknown_calls[why.at()] += 1;
                } else {
                    unknown_imports[why.at()] += 1;
                }
                if unknown_first.len() < DIFFER_SHOWN {
                    let mut segments = AVec::with_capacity_in(r.segments.len(), alloc);
                    segments.extend_from_slice(r.segments);
                    unknown_first.push(Unknown { module: r.module, call: r.call, kind: r.kind, segments, glob: r.glob, why });
                }
            }
            None => undecided += 1,
            Some(_) => {}
        }
    }
    // Each binding of the old core's scopes, asked of the cells (a later reader), in key order.
    let mut keys: AVec<'a, ItemKey> = AVec::with_capacity_in(done.len(), alloc);
    keys.extend(done.keys().copied());
    keys.sort_unstable();
    let (mut equal, mut differ, mut unknown, mut old_errors) = (0u64, 0u64, 0u64, 0u64);
    let mut differ_first: AVec<'a, Differ<'a>> = AVec::new_in(alloc);
    let mut written_first: AVec<'a, WrittenDiffer<'a>> = AVec::new_in(alloc);
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
            // Past the first DIFFER_SHOWN the rest are counted in `differ` only.
            if differ_first.len() < DIFFER_SHOWN {
                differ_first.push(Differ { module: m, ns: e.ns, name: e.name, old: e.binding, cells });
            }
        }
    }
    // The lists the cell core would hand the record writer, module by module.
    let mut written = WrittenAgainst { modules_unread: c.unread.len() as u64, modules_later: blocks as u64, ..WrittenAgainst::default() };
    for &m in keys.iter() {
        let Some(resolved) = done.get(&m) else { continue };
        let Some(w) = c.written(m, alloc) else {
            written.modules_absent += 1;
            continue;
        };
        if w.unknown > 0 {
            written.modules_unknown += 1;
            continue;
        }
        if w.globs[..] == resolved.scope.globs[..] {
            written.globs_equal += 1;
        } else {
            written.globs_differ += 1;
        }
        // A `#[macro_export]` macro an expansion wrote is a root binding the old path adds after
        // its fill (`publish_expanded_exports`, after this comparison): the cells hold it now.
        let mut held: AVec<'a, blaze_surface::ScopeEntry<&'a str>> = AVec::with_capacity_in(w.bindings.len(), alloc);
        for e in w.bindings.iter() {
            if e.binding.origin == crate::Origin::MacroExport && e.binding.expansion > 0 && resolved.scope.get(e.ns, e.name).is_none() {
                written.exports_published_later += 1;
            } else {
                held.push(*e);
            }
        }
        let (ours, theirs) = (&held[..], &resolved.scope.bindings[..]);
        if ours == theirs {
            written.bindings_equal += 1;
            continue;
        }
        written.bindings_differ += 1;
        let (mut i, mut j) = (0, 0);
        while i < ours.len() || j < theirs.len() {
            let order = match (ours.get(i), theirs.get(j)) {
                (Some(a), Some(b)) => (a.ns, a.name).cmp(&(b.ns, b.name)),
                (Some(_), None) => core::cmp::Ordering::Less,
                (None, _) => core::cmp::Ordering::Greater,
            };
            let shown = match order {
                core::cmp::Ordering::Less => {
                    written.only_cells += 1;
                    i += 1;
                    Some((ours[i - 1].ns, ours[i - 1].name, WrittenWhich::OnlyCells, None, Some(ours[i - 1].binding)))
                }
                core::cmp::Ordering::Greater => {
                    written.only_old += 1;
                    j += 1;
                    Some((theirs[j - 1].ns, theirs[j - 1].name, WrittenWhich::OnlyOld, Some(theirs[j - 1].binding), None))
                }
                core::cmp::Ordering::Equal => {
                    let (a, b) = (ours[i].binding, theirs[j].binding);
                    let which = if a.def != b.def {
                        written.def += 1;
                        Some(WrittenWhich::Def)
                    } else if a.vis != b.vis {
                        written.vis += 1;
                        Some(WrittenWhich::Vis)
                    } else if a.origin != b.origin {
                        written.origin += 1;
                        Some(WrittenWhich::Origin)
                    } else if a.expansion != b.expansion {
                        written.expansion += 1;
                        Some(WrittenWhich::Expansion)
                    } else if a.state != b.state {
                        written.state += 1;
                        Some(WrittenWhich::State)
                    } else {
                        None
                    };
                    i += 1;
                    j += 1;
                    which.map(|w| (ours[i - 1].ns, ours[i - 1].name, w, Some(b), Some(a)))
                }
            };
            if let Some((ns, name, which, old, cells)) = shown
                && written_first.len() < DIFFER_SHOWN
            {
                written_first.push(WrittenDiffer { module: m, ns, name, which, old, cells });
            }
        }
    }
    Ok(Shadowed {
        lost,
        lost_first,
        unknown_imports,
        unknown_calls,
        unknown_first,
        written_first,
        written,
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
        r6_errors: c.stats.r6_errors as u64,
        speculated: c.speculated.len() as u64,
        differ_first,
    })
}
