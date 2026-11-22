//! The cell core's tests: one fixture per rule with its planted bug as the control, and the
//! comparison with the solver on generated crates without macro calls, in shuffled push orders.

use std::format;
use std::string::String;
use std::vec::Vec;

use blaze_id::ItemKey;
use blaze_own::Alloc;
use blaze_sim::{Broke, Config, Tape};

use super::{CellPlant, CellStats, Cells, Outcome, Value};
use crate::tests::{B, Imp, N, V, World, reference_imports, reference_world};
use crate::{Edition, Namespace, Unresolved};

/// The core run on `w` with the tests' driver: expanding a call marks it expanded in the world,
/// whose facts then hand its expansion back. Each solve starts with nothing expanded.
fn solved(w: &World, root: ItemKey, plant: CellPlant, order: &[u32]) -> Result<Cells<'static, N>, String> {
    w.expanded.borrow_mut().clear();
    let mut c = Cells::load(w, root, Alloc::global()).map_err(|e| format!("{e}"))?;
    c.plant = plant;
    let expand = |call: ItemKey, _: crate::Def| {
        w.expanded.borrow_mut().insert(call);
    };
    c.run(w, &expand, order).map_err(|e| format!("{e}"))?;
    Ok(c)
}

fn cells_of(w: &World, root: ItemKey, plant: CellPlant) -> Cells<'static, N> {
    solved(w, root, plant, &[]).expect("the core runs")
}

fn outcome(c: &Cells<'static, N>, m: ItemKey, index: u32) -> Outcome {
    c.outcome(m, index).expect("every import is decided")
}

fn found(v: Option<Value>) -> Option<ItemKey> {
    match v {
        Some(Value::Found(d, _)) => Some(d.key),
        _ => None,
    }
}

/// R1. The Reference, `items.use.glob.shadowing`: "items and named imports are allowed to
/// shadow names from glob imports in the same namespace." The glob and the item sit in `b`, not
/// in the root its path starts from: in the root the planted bug (every read of the root's names
/// waiting on its globs first) would make the glob wait on itself and fail, hiding the plant.
#[test]
fn an_item_shadows_a_glob() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let a = w.module(root, "a", V::Pub);
    let ax = w.fun(a, "x", V::Pub);
    let b = w.module(root, "b", V::Pub);
    w.glob(b, "crate::a", V::Priv);
    let x = w.fun(b, "x", V::Priv);
    let mut ok = cells_of(&w, root, CellPlant::None);
    assert_eq!(found(ok.ask(b, Namespace::Values, "x").expect("asks")), Some(x));
    let mut bad = cells_of(&w, root, CellPlant::GlobsShadowExplicit);
    assert_eq!(found(bad.ask(b, Namespace::Values, "x").expect("asks")), Some(ax), "the control");
}

/// R2. The Reference, `items.use.glob.ambiguity`: "it is an error to use a name that is
/// imported by two glob imports where the names refer to different items" (when used).
#[test]
fn two_globs_with_different_items_are_ambiguous_where_used() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let a = w.module(root, "a", V::Pub);
    w.fun(a, "x", V::Pub);
    let b = w.module(root, "b", V::Pub);
    w.fun(b, "x", V::Pub);
    let c = w.module(root, "c", V::Pub);
    w.glob(c, "crate::a", V::Pub);
    w.glob(c, "crate::b", V::Pub);
    w.use_(root, "crate::c::x", V::Priv);
    let ok = cells_of(&w, root, CellPlant::None);
    assert!(matches!(outcome(&ok, root, 0), Outcome::Failed(Unresolved::Ambiguous(_))));
    let bad = cells_of(&w, root, CellPlant::FirstGlobWins);
    assert!(matches!(outcome(&bad, root, 0), Outcome::Named(_)), "the control");
}

/// R3. RFC 1560, "Changes to the name resolution algorithm": imports are resolved by a work
/// list "until fixed point"; globs that import each other are legal and bring what reaches the
/// cycle from outside.
#[test]
fn a_glob_cycle_brings_what_reaches_it() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let a = w.module(root, "a", V::Pub);
    let b = w.module(root, "b", V::Pub);
    let d = w.module(root, "d", V::Pub);
    w.glob(a, "crate::b", V::Pub);
    w.glob(a, "crate::d", V::Pub);
    w.glob(b, "crate::a", V::Pub);
    let x = w.fun(d, "x", V::Pub);
    w.use_(root, "crate::b::x", V::Priv);
    let ok = cells_of(&w, root, CellPlant::None);
    assert!(matches!(outcome(&ok, root, 0), Outcome::Named(bs) if bs[1].map(|b| b.0.key) == Some(x)));
    assert!(ok.stats.cycle_passes >= 1, "decided as a cycle");
    let bad = cells_of(&w, root, CellPlant::CycleNotFound);
    assert!(matches!(outcome(&bad, root, 0), Outcome::Failed(_)), "the control");
}

/// R4. The Reference, `items.use.glob`: a glob imports "all importable entities" "that are
/// visible" from the importing module; a private item stays behind.
#[test]
fn a_glob_leaves_private_items_behind() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let a = w.module(root, "a", V::Pub);
    w.fun(a, "x", V::Priv);
    let b = w.module(root, "b", V::Pub);
    w.glob(b, "crate::a", V::Pub);
    let mut ok = cells_of(&w, root, CellPlant::None);
    assert_eq!(ok.ask(b, Namespace::Values, "x").expect("asks"), Some(Value::NotFound));
    let mut bad = cells_of(&w, root, CellPlant::GlobLeaksPrivate);
    assert!(found(bad.ask(b, Namespace::Values, "x").expect("asks")).is_some(), "the control");
}

/// R5, the stall rule (bottom.md section 6): at a quiet point only the sinks of the wait graph
/// fail; what waited on them is then solved normally. RFC 1560: what is left unresolved at the
/// fixed point is an error, and "how a name resolves should be consistent under any ordering".
#[test]
fn a_stall_fails_its_sink_alone() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let a = w.module(root, "a", V::Pub);
    let b = w.module(root, "b", V::Pub);
    let c = w.module(root, "c", V::Pub);
    let d = w.module(root, "d", V::Pub);
    w.use_(a, "crate::b::q", V::Pub);
    w.use_(b, "crate::a::q", V::Pub);
    w.glob(c, "crate::a", V::Pub);
    w.glob(c, "crate::d", V::Pub);
    let q = w.fun(d, "q", V::Pub);
    w.use_(root, "crate::c::q", V::Priv);
    let ok = cells_of(&w, root, CellPlant::None);
    assert!(matches!(outcome(&ok, a, 0), Outcome::Failed(Unresolved::NotFound)));
    assert!(matches!(outcome(&ok, b, 0), Outcome::Failed(Unresolved::NotFound)));
    assert!(matches!(outcome(&ok, root, 0), Outcome::Named(bs) if bs[1].map(|b| b.0.key) == Some(q)));
    assert_eq!(ok.stats.stalls, 1);
    let bad = cells_of(&w, root, CellPlant::StallFailsAll);
    assert!(matches!(outcome(&bad, root, 0), Outcome::Failed(_)), "the control");
}

// ---------------------------------------------------------------------------------------------
// The comparison on generated crates without macro calls.

// ---------------------------------------------------------------------------------------------
// Stage 2: macro calls. The control of each growth test is `CallsNoGrowth`: a cell decided
// before its module's calls are in either answers wrong or trips the finality check.

/// The control's verdict: the planted core either fails (the finality check names a cell
/// decided not found too early) or answers otherwise than `right`.
fn caught(w: &World, root: ItemKey, plant: CellPlant, m: ItemKey, index: u32, right: Outcome) -> bool {
    match solved(w, root, plant, &[]) {
        Err(_) => true,
        Ok(c) => c.outcome(m, index) != Some(right),
    }
}

fn named_def(o: Outcome, ns: Namespace) -> Option<ItemKey> {
    match o {
        Outcome::Named(b) => b[super::ns_at(ns)].map(|(d, _)| d.key),
        _ => None,
    }
}

/// A call writes an item another module imports: the import waits for the call's expansion,
/// though a glob of the module already brings the name (an expanded item shadows a glob, R1).
#[test]
fn a_call_writes_an_item_another_module_imports() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    w.use_(root, "crate::a::x", V::Priv);
    let a = w.module(root, "a", V::Pub);
    let d = w.module(root, "d", V::Pub);
    w.fun(d, "x", V::Pub);
    // The call's macro comes by the same glob, so the glob is decided before the call resolves:
    // the moment a core without growth decides `a::x` from the glob.
    w.item(d, "mm", Namespace::Macros, crate::DefKind::Macro(crate::MacroShapes::FN_LIKE), V::Pub);
    w.glob(a, "crate::d", V::Pub);
    let call = w.call(a, "mm", crate::CallShape::FnLike);
    let x = w.fun(call, "x", V::Pub);
    let ok = cells_of(&w, root, CellPlant::None);
    let got = outcome(&ok, root, 0);
    assert_eq!(named_def(got, Namespace::Values), Some(x), "{got:?}");
    assert!(caught(&w, root, CellPlant::CallsNoGrowth, root, 0, got), "the control");
}

/// A call writes a call (two levels): the inner call resolves in the textual scope through the
/// outer expansion's place, and writes the item imported.
#[test]
fn a_call_writes_a_call() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    w.rules(root, "mk");
    w.use_(root, "crate::a::y", V::Priv);
    let a = w.module(root, "a", V::Pub);
    let outer = w.call(a, "mk", crate::CallShape::FnLike);
    let inner = w.call(outer, "mk", crate::CallShape::FnLike);
    let y = w.fun(inner, "y", V::Pub);
    let ok = cells_of(&w, root, CellPlant::None);
    assert_eq!(ok.stats.expansions, 2);
    let got = outcome(&ok, root, 0);
    assert_eq!(named_def(got, Namespace::Values), Some(y), "{got:?}");
    assert!(caught(&w, root, CellPlant::CallsNoGrowth, root, 0, got), "the control");
}

/// A call writes a module with its own imports: the module appears, and its import resolves.
#[test]
fn a_call_writes_a_module_with_imports() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    w.rules(root, "mk");
    w.use_(root, "crate::a::m::z", V::Priv);
    let a = w.module(root, "a", V::Pub);
    let k = w.module(root, "k", V::Pub);
    let z = w.fun(k, "z", V::Pub);
    let call = w.call(a, "mk", crate::CallShape::FnLike);
    let m = w.module(call, "m", V::Pub);
    w.use_(m, "crate::k::z", V::Pub);
    let ok = cells_of(&w, root, CellPlant::None);
    assert_eq!(named_def(outcome(&ok, m, 0), Namespace::Values), Some(z));
    let got = outcome(&ok, root, 0);
    assert_eq!(named_def(got, Namespace::Values), Some(z), "{got:?}");
    assert!(caught(&w, root, CellPlant::CallsNoGrowth, root, 0, got), "the control");
}

/// A call's path is imported through a glob (the path-based scope, macro namespace).
#[test]
fn a_call_named_through_a_glob() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    w.use_(root, "crate::a::x", V::Priv);
    let defs = w.module(root, "defs", V::Pub);
    let mm = w.item(defs, "mm", Namespace::Macros, crate::DefKind::Macro(crate::MacroShapes::FN_LIKE), V::Pub);
    let a = w.module(root, "a", V::Pub);
    w.glob(a, "crate::defs", V::Pub);
    let call = w.call(a, "mm", crate::CallShape::FnLike);
    let x = w.fun(call, "x", V::Pub);
    let ok = cells_of(&w, root, CellPlant::None);
    assert!(matches!(ok.call_outcome(call), Some(Outcome::Called(d)) if d.key == mm));
    let got = outcome(&ok, root, 0);
    assert_eq!(named_def(got, Namespace::Values), Some(x), "{got:?}");
    assert!(caught(&w, root, CellPlant::CallsNoGrowth, root, 0, got), "the control");
}

/// The textual scope (Reference `macros.by-example.scope`): a definition is seen after it in
/// text order and in child modules after it; `#[macro_use]` carries a child's definitions to
/// the rest of the parent; a later definition shadows; a call before any definition sees none.
#[test]
fn the_textual_scope_of_macro_rules() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let early = w.call(root, "mx", crate::CallShape::FnLike);
    let first = w.rules(root, "mx");
    let between = w.call(root, "mx", crate::CallShape::FnLike);
    let second = w.rules(root, "mx");
    let child = w.module(root, "c", V::Pub);
    let in_child = w.call(child, "mx", crate::CallShape::FnLike);
    let u = w.module_with(root, "u", V::Pub, true, false);
    let my = w.rules(u, "my");
    let after_use = w.call(root, "my", crate::CallShape::FnLike);
    let ok = cells_of(&w, root, CellPlant::None);
    let called = |c: ItemKey| match ok.call_outcome(c) {
        Some(Outcome::Called(d)) => Some(d.key),
        _ => None,
    };
    assert_eq!(called(early), None, "before any definition");
    assert_eq!(called(between), Some(first));
    assert_eq!(called(in_child), Some(second), "the latest before the child's `mod` line");
    assert_eq!(called(after_use), Some(my), "`#[macro_use]` carries `my` out");
    let bad = cells_of(&w, root, CellPlant::NoTextualScope);
    assert_eq!(bad.call_outcome(between).and_then(|o| if let Outcome::Called(d) = o { Some(d.key) } else { None }), None, "the control");
}

/// R5 over calls, the refinement: `B` reads the textual scope past the earlier call `A`, and
/// `A`'s path-based read waits on `B` (its module's growth): a sink of two calls. `A` fails
/// first; `B` is spared that round, then resolves through the glob. The crate: `mod defs { pub
/// macro my; } m::zz!(); /* A */ mod m { pub use crate::defs::*; use crate::my; my!(); /* B */ }`.
/// `A` reads `zz` in `m`, which `B` may still write; `B` is held at the stall by the pending
/// named import of its own name (`use crate::my;` waits on the root's growth, `A`: Reference
/// `names.resolution.expansion.speculation` makes only a candidate selected speculative, and a
/// named import that may still bind the name is none yet), so the weak rule takes nothing for
/// it and the round spares it. Once `A` has failed the import fails and `B` takes the glob's
/// `my`. (Until runner 15 the fixture was `mod m { pub use crate::defs::*; self::mx!(); my!(); }`:
/// there `B` is held by the text order alone, and since the stall asks each call with a weakly
/// undecided scope not holding, it takes the glob's `my` at the stall itself and no round
/// spares anything; its outcomes are asserted below as well.)
#[test]
fn a_stall_spares_the_later_call() {
    let fn_like = crate::DefKind::Macro(crate::MacroShapes::FN_LIKE);
    // The older fixture: the same two outcomes, with no call spared.
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let defs = w.module(root, "defs", V::Pub);
    let my = w.item(defs, "my", Namespace::Macros, fn_like, V::Pub);
    let m = w.module(root, "m", V::Pub);
    w.glob(m, "crate::defs", V::Pub);
    let a = w.call(m, "self::mx", crate::CallShape::FnLike);
    let b = w.call(m, "my", crate::CallShape::FnLike);
    let old = cells_of(&w, root, CellPlant::None);
    assert!(!matches!(old.call_outcome(a), Some(Outcome::Called(_))), "{:?}", old.call_outcome(a));
    assert!(matches!(old.call_outcome(b), Some(Outcome::Called(d)) if d.key == my), "{:?}", old.call_outcome(b));
    assert_eq!(old.stats.r6_errors, 0);
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let defs = w.module(root, "defs", V::Pub);
    let my = w.item(defs, "my", Namespace::Macros, fn_like, V::Pub);
    let a = w.call(root, "m::zz", crate::CallShape::FnLike);
    let m = w.module(root, "m", V::Pub);
    w.glob(m, "crate::defs", V::Pub);
    w.use_(m, "crate::my", V::Priv);
    let b = w.call(m, "my", crate::CallShape::FnLike);
    let ok = cells_of(&w, root, CellPlant::None);
    assert!(!matches!(ok.call_outcome(a), Some(Outcome::Called(_))), "{:?}", ok.call_outcome(a));
    assert!(matches!(ok.call_outcome(b), Some(Outcome::Called(d)) if d.key == my), "{:?}", ok.call_outcome(b));
    assert_eq!(ok.stats.spared, 1);
    let bad = cells_of(&w, root, CellPlant::StallSparesNone);
    assert!(!matches!(bad.call_outcome(b), Some(Outcome::Called(_))), "the control");
    // The hold itself: a stall that takes the glob's `my` past the pending `use crate::my;`
    // spares nothing (Reference `names.resolution.expansion.expansion-order-stability`: "names
    // in the fully expanded AST must resolve to the same definition regardless of the order in
    // which macros are expanded and imports are resolved": what the import binds would shadow
    // the glob's candidate, so the candidate is not selected before the import is decided).
    let past = cells_of(&w, root, CellPlant::StallPassesNamedImport);
    assert_eq!(past.stats.spared, 0, "the control StallPassesNamedImport: {:?}", past.call_outcome(b));
}

fn same_why(a: Unresolved, b: Unresolved) -> bool {
    let plain = |u: Unresolved| match u {
        Unresolved::Private => Unresolved::NotFound,
        Unresolved::Ambiguous(_) => Unresolved::Ambiguous(crate::AmbiguityKind::GlobVsGlob),
        u => u,
    };
    plain(a) == plain(b)
}

/// The core run with items first pushed in a drawn order.
fn shuffled(w: &World, root: ItemKey, tape: &Tape) -> Result<Cells<'static, N>, String> {
    let count = Cells::load(w, root, Alloc::global()).map_err(|e| format!("{e}"))?.items.len() as u32;
    let mut order: Vec<u32> = (0..count).collect();
    let mut drawn: Vec<u32> = Vec::new();
    while !order.is_empty() {
        drawn.push(order.remove(tape.below(order.len() as u64, "push order") as usize));
    }
    solved(w, root, CellPlant::None, &drawn)
}

/// Every import's outcome against the reference solver (src/tests/oracle.rs, the judge of the
/// rules R1 to R9): the item each namespace binds and its visibility, a glob's source, or why
/// it failed.
fn compare(w: &World, root: ItemKey, c: &Cells<'static, N>) -> Result<[u64; 3], String> {
    let reference = reference_imports(w, root);
    // Imports compared: bound by name, globs, failed.
    let mut seen = [0u64; 3];
    for (&m, d) in w.decls.iter() {
        for i in 0..d.uses.len() as u32 {
            let want = reference.get(&(m, i)).copied();
            let got = c.outcome(m, i);
            let ok = match (got, want) {
                (Some(Outcome::Named(a)), Some(Imp::Named(b))) => {
                    a.iter().zip(b.iter()).all(|(x, y)| *x == y.map(|y: B| (y.def, y.vis)))
                }
                (Some(Outcome::Glob(a)), Some(Imp::Glob(b))) => a == b,
                (Some(Outcome::Failed(a)), Some(Imp::Failed(b))) => same_why(a, b),
                _ => false,
            };
            if !ok {
                let u = &d.uses[i as usize];
                let parent = w.modules[&m].parent;
                return Err(format!(
                    "the reference says {want:?}, the cells {got:?}: import {i} ({:?} {:?} {:?}) of {m:x} (parent {parent:?})",
                    u.path.kind, u.path.segments, u.leaf
                ));
            }
            seen[match got {
                Some(Outcome::Named(_)) => 0,
                Some(Outcome::Glob(_)) => 1,
                _ => 2,
            }] += 1;
        }
    }
    Ok(seen)
}

fn case(tape: &Tape) -> Result<(CellStats, [u64; 3]), Broke> {
    case_in(tape, false)
}

fn case_in(tape: &Tape, calls: bool) -> Result<(CellStats, [u64; 3]), Broke> {
    // The reference's own drawing, narrow; `calls`: with `macro_rules!`, `#[macro_use]`,
    // `#[macro_export]` and calls, some writing a module (stage 2).
    let (w, root) = reference_world(tape, false, calls);
    let first = solved(&w, root, CellPlant::None, &[]).map_err(Broke::Oracle)?;
    let second = shuffled(&w, root, tape).map_err(Broke::Oracle)?;
    // The determinism proof: the same outcomes and the same counters in another push order.
    if first.stats != second.stats {
        return Err(Broke::Oracle(format!("counters differ by push order: {:?} then {:?}", first.stats, second.stats)));
    }
    for (&m, d) in w.decls.iter() {
        for i in 0..d.uses.len() as u32 {
            if first.outcome(m, i) != second.outcome(m, i) {
                return Err(Broke::Oracle(format!("import {i} of {m:x} differs by push order")));
            }
        }
    }
    let stats = first.stats;
    let seen = compare(&w, root, &first).map_err(|e| Broke::Oracle(format!("differs from the reference: {e}")))?;
    Ok((stats, seen))
}

const CASES: u32 = 2000;

#[test]
fn the_cells_equal_the_reference_without_macros() {
    let mut seen = [0u64; 3];
    blaze_sim::check(Config::cases(CASES), |tape| {
        let (_, s) = case(tape)?;
        for k in 0..3 {
            seen[k] += s[k];
        }
        Ok::<(), Broke>(())
    });
    std::println!("cells vs reference: {CASES} cases, imports equal: {} named, {} globs, {} failed", seen[0], seen[1], seen[2]);
    assert!(seen.iter().all(|&n| n > 0), "every kind of outcome was compared: {seen:?}");
}

#[test]
fn the_cells_equal_the_reference_with_calls() {
    let mut seen = [0u64; 3];
    let mut expansions = 0;
    blaze_sim::check(Config::cases(CASES), |tape| {
        let (st, s) = case_in(tape, true)?;
        expansions += st.expansions;
        for k in 0..3 {
            seen[k] += s[k];
        }
        Ok::<(), Broke>(())
    });
    std::println!(
        "cells vs reference with calls: {CASES} cases, {expansions} expansions, imports equal: {} named, {} globs, {} failed",
        seen[0],
        seen[1],
        seen[2]
    );
    assert!(seen.iter().all(|&n| n > 0) && expansions > 0, "every kind of outcome was compared: {seen:?}");
}

/// The counters on the largest drawn crate (by imports), printed for the report.
#[test]
fn the_counters_on_the_largest_case() {
    let mut largest: Option<CellStats> = None;
    blaze_sim::check(Config::cases(CASES), |tape| {
        let (s, _) = case(tape)?;
        if largest.is_none_or(|l| (s.items, s.cells) > (l.items, l.cells)) {
            largest = Some(s);
        }
        Ok::<(), Broke>(())
    });
    let s = largest.expect("a case ran");
    std::println!(
        "cells largest case: modules {} names declared {} imports {} cells {} decisions {} waiters {} wakes {} evaluations {} (mean {:.2}, max {}) quiet points {} cycle passes {} cycle cells {} largest cycle {} stalls {} stalled items {}",
        s.modules,
        s.names_declared,
        s.items,
        s.cells,
        s.decisions,
        s.waiters,
        s.wakes,
        s.evaluations,
        s.evaluations as f64 / s.items.max(1) as f64,
        s.max_evaluations,
        s.quiet_points,
        s.cycle_passes,
        s.cycle_cells,
        s.largest_cycle,
        s.stalls,
        s.stalled_items
    );
}

// ---------------------------------------------------------------------------------------------
// The outer scopes (R6-R8): a plain first segment the module misses goes outward.

/// A crate whose standard prelude is its own module `prelude` (core's `#[prelude_import]`),
/// holding `pub mod pm { pub struct y; }`; a child `c` with `use pm::y;` and, after it, a call
/// of `mk!`. Returns (world, root, c, the call, the prelude's `y`).
fn prelude_world() -> (World, ItemKey, ItemKey, ItemKey, ItemKey) {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let prelude = w.module(root, "prelude", V::Pub);
    let pm = w.module(prelude, "pm", V::Pub);
    let y = w.item(pm, "y", Namespace::Types, crate::DefKind::Other, V::Pub);
    w.cr(root).head.prelude = Some(prelude);
    w.rules(root, "mk");
    let c = w.module(root, "c", V::Pub);
    w.use_(c, "pm::y", V::Priv);
    let call = w.call(c, "mk", crate::CallShape::FnLike);
    (w, root, c, call, y)
}

/// R6: `use pm::y;` misses in `c`, which grows by expansion alone: it takes the prelude's `pm`
/// at once and leaves a watch; the call then writes `mod pm` into `c` (an expansion that does
/// not enclose the import): the R6 error, at that definition. Controls: a core whose growth
/// holds the lookup answers through the new `pm` instead; one without the watch reports nothing.
#[test]
fn r6_a_miss_goes_outward_and_a_later_definition_is_the_error() {
    let (mut w, root, c, call, y) = prelude_world();
    // The definition comes from a call the first call's expansion writes: it is evaluated after
    // the import (a call of `c`'s text expands before `c`'s imports are first tried, and the
    // import then meets the definition inside: the error in the other order).
    let nested = w.call(call, "mk", crate::CallShape::FnLike);
    let inner = w.module(nested, "pm", V::Pub);
    w.item(inner, "y", Namespace::Types, crate::DefKind::Other, V::Pub);
    let ok = cells_of(&w, root, CellPlant::None);
    let got = outcome(&ok, c, 0);
    assert_eq!(named_def(got, Namespace::Types), Some(y), "{got:?}");
    assert_eq!((ok.r6.len(), ok.stats.r6_errors), (1, 1), "the error at the expansion's `mod pm`");
    // The control: growth held the lookup, so it met the written `pm` first. Since R6 gives
    // that order the same answer (the fixture below), the control is seen in the order alone:
    // the lookup did not go outward on its first try.
    let waits = cells_of(&w, root, CellPlant::OutwardWaits);
    assert_eq!(named_def(outcome(&waits, c, 0), Namespace::Types), Some(y), "the answer does not depend on the order");
    assert!(waits.stats.outward < ok.stats.outward, "the control: {} outward, {} without the plant", waits.stats.outward, ok.stats.outward);
    let blind = cells_of(&w, root, CellPlant::NoWatch);
    assert_eq!(blind.stats.r6_errors, 0, "the control: no watch, no error");
}

/// R6 in the other order (wide seed 8 with calls): the call of `c`'s own text writes `mod pm`
/// and expands before the import is first tried, so the import meets the definition inside.
/// The answer is the one the first order gives: the prelude's `pm`, with the error at the
/// expansion's definition. Control: a core where the earlier-written definition wins answers
/// through the new `pm` (an answer that depends on the order).
#[test]
fn r6_a_definition_written_first_still_loses_to_the_outer_binding() {
    let (mut w, root, c, call, y) = prelude_world();
    let inner = w.module(call, "pm", V::Pub);
    let other = w.item(inner, "y", Namespace::Types, crate::DefKind::Other, V::Pub);
    let ok = cells_of(&w, root, CellPlant::None);
    let got = outcome(&ok, c, 0);
    assert_eq!(named_def(got, Namespace::Types), Some(y), "{got:?}");
    assert_eq!(ok.stats.r6_errors, 1, "the error at the expansion's `mod pm`");
    let bad = cells_of(&w, root, CellPlant::InnerAfterWrite);
    assert_eq!(named_def(outcome(&bad, c, 0), Namespace::Types), Some(other), "the control");
}

/// R6, D56: the same program with the expansion writing nothing named `pm`: outward, no error.
/// And D57: an expansion writing the outer definition itself (`pub use` is not drawn here: the
/// prelude's own `pm` module key) is no other candidate.
#[test]
fn r6_no_definition_no_error() {
    let (w, root, c, _, y) = prelude_world();
    let ok = cells_of(&w, root, CellPlant::None);
    assert_eq!(named_def(outcome(&ok, c, 0), Namespace::Types), Some(y));
    assert_eq!(ok.stats.r6_errors, 0);
    assert!(ok.stats.outward >= 1);
}

/// R7: a glob of `c` brings a DIFFERENT `pm` beside the prelude's: the import fails as
/// ambiguous at its first segment. D57: a glob bringing the prelude's own `pm` is one candidate.
/// Control: a core where the glob beats the outer candidate resolves through it.
#[test]
fn r7_a_glob_beside_a_different_outer_candidate_is_ambiguous() {
    let (mut w, root, c, _, y) = prelude_world();
    let g = w.module(root, "g", V::Pub);
    let other = w.module(g, "pm", V::Pub);
    w.item(other, "y", Namespace::Types, crate::DefKind::Other, V::Pub);
    w.glob(c, "crate::g", V::Priv);
    let ok = cells_of(&w, root, CellPlant::None);
    assert!(matches!(outcome(&ok, c, 0), Outcome::Failed(Unresolved::Ambiguous(_))), "{:?}", outcome(&ok, c, 0));
    let bad = cells_of(&w, root, CellPlant::GlobBeatsOuter);
    assert!(!matches!(outcome(&bad, c, 0), Outcome::Failed(Unresolved::Ambiguous(_))), "the control");
    // D57: the glob's source re-exports the prelude's own `pm`.
    let (mut w, root, c, _, _) = prelude_world();
    let g = w.module(root, "g", V::Pub);
    w.use_(g, "crate::prelude::pm", V::Pub);
    w.glob(c, "crate::g", V::Priv);
    let ok = cells_of(&w, root, CellPlant::None);
    assert_eq!(named_def(outcome(&ok, c, 0), Namespace::Types), Some(y), "{:?}", outcome(&ok, c, 0));
}

/// R8: a dependency's extern-prelude name is an outer candidate; a module of the crate of that
/// name shadows it (the module is the inner scope). Through the dependency the next segment is
/// in a crate this core does not see: the import is unknown (`Outside`), never not found; and an
/// import of a name such an import brings is unknown too (the last runner's fix (2)). Control:
/// a core taking the Outside binding as absent fails the second import as not found.
#[test]
fn r8_extern_prelude_and_an_outside_binding_is_unknown() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let dep = w.root(2, Edition::E2021);
    w.item(dep, "x", Namespace::Types, crate::DefKind::Other, V::Pub);
    w.dep(root, "e", dep);
    w.use_(root, "e::x", V::Pub);
    let b = w.module(root, "b", V::Pub);
    w.use_(b, "crate::x", V::Priv);
    w.use_(b, "e", V::Priv);
    let ok = cells_of(&w, root, CellPlant::None);
    assert_eq!(outcome(&ok, root, 0), Outcome::Outside);
    assert_eq!(outcome(&ok, b, 0), Outcome::Outside, "a name an Outside import brings is unknown");
    assert_eq!(named_def(outcome(&ok, b, 1), Namespace::Types), Some(dep), "`use e;` binds the dependency's root");
    let bad = cells_of(&w, root, CellPlant::OutsideIsNotFound);
    assert!(matches!(outcome(&bad, b, 0), Outcome::Failed(Unresolved::NotFound)), "the control: {:?}", outcome(&bad, b, 0));
    // A module of the name shadows the extern prelude.
    let e = w.module(root, "e", V::Pub);
    w.item(e, "x", Namespace::Types, crate::DefKind::Other, V::Pub);
    let ok = cells_of(&w, root, CellPlant::None);
    assert!(matches!(outcome(&ok, root, 0), Outcome::Named(_)), "{:?}", outcome(&ok, root, 0));
}

/// The last runner's fix (1): a stalled call whose guess is `Outside` (the `#[macro_use]`
/// prelude of another crate may hold its name) is decided Outside first, alone, never failed by
/// the stall. The sink: call `mx!` in `m` waits on `m`'s glob `self::k::*`, whose `k` waits on
/// `m`'s growth: the call. Control: a stall that fails it.
#[test]
fn a_stalled_outside_call_is_marked_before_the_stall() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let other = w.root(5, Edition::E2021);
    w.cr(root).macro_use.push(other);
    let m = w.module(root, "m", V::Pub);
    w.glob(m, "self::k", V::Priv);
    let call = w.call(m, "mx", crate::CallShape::FnLike);
    let ok = cells_of(&w, root, CellPlant::None);
    assert_eq!(ok.call_outcome(call), Some(Outcome::Outside));
    let bad = cells_of(&w, root, CellPlant::StallFailsOutside);
    assert_ne!(bad.call_outcome(call), Some(Outcome::Outside), "the control");
}

/// R5 (2026-11-16) for a call: in a stalled member the undecided inner scope is absent, so a
/// one-segment call goes outward and resolves to the standard prelude's macro: core's
/// `core_arch` `#[derive(Clone)]` in a module whose glob waits on that module's own calls. The
/// sink: `Cl!` in `m` waits on `m`'s glob `self::k::*`, whose `k` waits on `m`'s growth: the
/// call. Control: a stall that fails the call.
#[test]
fn a_stalled_call_an_outer_scope_provides_resolves() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let prelude = w.module(root, "prelude", V::Pub);
    let cl = w.item(prelude, "Cl", Namespace::Macros, crate::DefKind::Macro(crate::MacroShapes::DERIVE), V::Pub);
    w.cr(root).head.prelude = Some(prelude);
    let m = w.module(root, "m", V::Pub);
    w.glob(m, "self::k", V::Priv);
    // A derive, as written in core (runner 16: it was a function-like call of a derive, which
    // the sub-namespace rule now refuses).
    let call = w.call(m, "Cl", crate::CallShape::Derive);
    let ok = cells_of(&w, root, CellPlant::None);
    assert!(matches!(ok.call_outcome(call), Some(Outcome::Called(d)) if d.key == cl), "{:?}", ok.call_outcome(call));
    assert!(matches!(outcome(&ok, m, 0), Outcome::Failed(Unresolved::NotFound)), "{:?}", outcome(&ok, m, 0));
    let bad = cells_of(&w, root, CellPlant::StallFailsOuterCall);
    assert!(!matches!(bad.call_outcome(call), Some(Outcome::Called(_))), "the control: {:?}", bad.call_outcome(call));
}

/// A glob of an enum (core::cmp's `use self::Ordering::*;`) brings variants, which this core
/// does not read: unknown in types and values, but never a macro, so the module's
/// `#[derive(PartialEq)]` still goes outward to the prelude's. Control: a core taking the enum
/// glob as unknown in every namespace leaves the call `Outside`.
#[test]
fn an_enum_glob_brings_no_macro() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let prelude = w.module(root, "prelude", V::Pub);
    let cl = w.item(prelude, "Cl", Namespace::Macros, crate::DefKind::Macro(crate::MacroShapes::DERIVE), V::Pub);
    w.cr(root).head.prelude = Some(prelude);
    let m = w.module(root, "m", V::Pub);
    w.item(m, "E", Namespace::Types, crate::DefKind::Enum, V::Pub);
    w.glob(m, "self::E", V::Priv);
    // A derive, as the doc says (runner 16: it was a function-like call of a derive).
    let call = w.call(m, "Cl", crate::CallShape::Derive);
    let ok = cells_of(&w, root, CellPlant::None);
    assert_eq!(outcome(&ok, m, 0), Outcome::Outside, "the enum glob itself stays unknown");
    assert!(matches!(ok.call_outcome(call), Some(Outcome::Called(d)) if d.key == cl), "{:?}", ok.call_outcome(call));
    let bad = cells_of(&w, root, CellPlant::EnumHidesMacros);
    assert_eq!(bad.call_outcome(call), Some(Outcome::Outside), "the control");
}

/// The language prelude after a local standard prelude: core's `core::primitive` has
/// `pub use bool;`, a built-in type by its one-segment name (Reference, "Use declarations",
/// "Preludes"). Control: a core that stops at the standard prelude fails it as not found.
#[test]
fn a_use_of_a_builtin_type_after_a_local_prelude() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let prelude = w.module(root, "prelude", V::Pub);
    w.cr(root).head.prelude = Some(prelude);
    let primitive = w.module(root, "primitive", V::Pub);
    w.use_(primitive, "bool", V::Pub);
    let want = crate::builtin_type("bool").map(|d| d.key);
    let ok = cells_of(&w, root, CellPlant::None);
    assert_eq!(named_def(outcome(&ok, primitive, 0), Namespace::Types), want, "{:?}", outcome(&ok, primitive, 0));
    let bad = cells_of(&w, root, CellPlant::NoLanguagePrelude);
    assert!(matches!(outcome(&bad, primitive, 0), Outcome::Failed(Unresolved::NotFound)), "the control: {:?}", outcome(&bad, primitive, 0));
}

/// Every import's outcome against the reference on the WIDE distribution (an extern prelude, a
/// foreign standard prelude, plain first segments): `Outside` answers (a crate this core does
/// not see) and imports with an R6 error (the program is an error; the reference keeps its own
/// order's answer) are counted apart, the rest must be equal.
fn compare_wide(w: &World, root: ItemKey, c: &Cells<'static, N>) -> Result<[u64; 3], String> {
    let reference = reference_imports(w, root);
    // equal, outside, R6 errors
    let mut seen = [0u64; 3];
    for (&m, d) in w.decls.iter() {
        for i in 0..d.uses.len() as u32 {
            let got = c.outcome(m, i);
            if got == Some(Outcome::Outside) {
                seen[1] += 1;
                continue;
            }
            if c.import_has_r6(m, i) {
                seen[2] += 1;
                continue;
            }
            let want = reference.get(&(m, i)).copied();
            let ok = match (got, want) {
                (Some(Outcome::Named(a)), Some(Imp::Named(b))) => a.iter().zip(b.iter()).all(|(x, y)| *x == y.map(|y: B| (y.def, y.vis))),
                (Some(Outcome::Glob(a)), Some(Imp::Glob(b))) => a == b,
                (Some(Outcome::Failed(a)), Some(Imp::Failed(b))) => same_why(a, b),
                _ => false,
            };
            if !ok {
                let u = &d.uses[i as usize];
                return Err(format!("the reference says {want:?}, the cells {got:?}: import {i} ({:?} {:?} {:?}) of {m:x}", u.path.kind, u.path.segments, u.leaf));
            }
            seen[0] += 1;
        }
    }
    Ok(seen)
}

fn case_wide(tape: &Tape, calls: bool) -> Result<[u64; 3], Broke> {
    let (w, root) = reference_world(tape, true, calls);
    let first = solved(&w, root, CellPlant::None, &[]).map_err(Broke::Oracle)?;
    let second = shuffled(&w, root, tape).map_err(Broke::Oracle)?;
    if first.stats != second.stats {
        return Err(Broke::Oracle(format!("counters differ by push order: {:?} then {:?}", first.stats, second.stats)));
    }
    compare_wide(&w, root, &first).map_err(|e| {
        let text = format!("differs from the reference: {e}\nthe program:\n{}", program(&w));
        // A failing case in full, for a runner whose test driver cuts long lines.
        if let Some(path) = std::env::var_os("CELLS_DUMP") {
            std::fs::write(path, &text).expect("the dump file is writable");
        }
        Broke::Oracle(text)
    })
}

/// The drawn program, for a failing case: each module and expansion with its declarations.
fn program(w: &World) -> String {
    let mut out = String::new();
    for (k, info) in w.modules.iter() {
        out.push_str(&format!("module {k:x}: {info:?}\n"));
    }
    for (k, root) in w.roots.iter() {
        out.push_str(&format!("crate {k:?}: {root:?}\n"));
    }
    for (k, d) in w.decls.iter() {
        out.push_str(&format!("text of {k:x}: {d:?}\n"));
    }
    out
}

fn wide(calls: bool) {
    let mut seen = [0u64; 3];
    blaze_sim::check(Config::cases(CASES), |tape| {
        let s = case_wide(tape, calls)?;
        for k in 0..3 {
            seen[k] += s[k];
        }
        Ok::<(), Broke>(())
    });
    std::println!("cells vs reference, wide, calls {calls}: {CASES} cases, imports equal {}, outside {}, R6 errors {}", seen[0], seen[1], seen[2]);
    assert!(seen[0] > 0, "imports were compared: {seen:?}");
}

#[test]
fn the_cells_equal_the_reference_wide() {
    wide(false);
}

#[test]
fn the_cells_equal_the_reference_wide_with_calls() {
    wide(true);
}

/// A dependency solved first by this core, read through [`super::DepNames`] (the ready queue's
/// order: a dependency is decided before its dependents).
struct SolvedDep {
    cells: core::cell::RefCell<Cells<'static, N>>,
    krate: blaze_id::CrateItemKey,
    root: ItemKey,
}

impl super::DepNames<N> for SolvedDep {
    fn has_module(&self, module: ItemKey) -> bool {
        self.cells.borrow().by_key.get(&module).is_some_and(|&m| !self.cells.borrow().modules[m as usize].foreign)
    }

    fn name_in(&self, module: ItemKey, ns: Namespace, name: N) -> super::DepAnswer {
        match self.cells.borrow_mut().ask(module, ns, name) {
            Ok(Some(Value::Found(d, v))) => super::DepAnswer::Found(d, v),
            Ok(Some(Value::Ambiguous(d, v))) => super::DepAnswer::Ambiguous(d, v),
            Ok(Some(Value::NotFound)) => super::DepAnswer::NotFound,
            Ok(Some(Value::Outside) | None) | Err(_) => super::DepAnswer::Unknown,
        }
    }

    fn crate_root(&self, krate: blaze_id::CrateItemKey) -> Option<ItemKey> {
        (krate == self.krate).then_some(self.root)
    }

    fn has_enum(&self, _key: ItemKey) -> bool {
        false
    }
}

/// A crate's enums as the tests write them: each variant's name, key and whether it is a
/// record variant (`V { .. }`: no value).
struct TestEnums(std::collections::BTreeMap<ItemKey, Vec<(N, ItemKey, bool)>>);

impl super::EnumNames<N> for TestEnums {
    fn holds(&self, of: ItemKey) -> bool {
        self.0.contains_key(&of)
    }

    fn variant(&self, of: ItemKey, ns: Namespace, name: N) -> super::DepAnswer {
        let Some(variants) = self.0.get(&of) else { return super::DepAnswer::Unknown };
        match variants.iter().find(|v| v.0 == name && (ns == Namespace::Types || (ns == Namespace::Values && !v.2))) {
            Some(v) => super::DepAnswer::Found(crate::Def { key: v.1, kind: crate::DefKind::Other }, crate::Visibility::Public),
            None => super::DepAnswer::NotFound,
        }
    }
}

/// `root`'s crate solved with its enums' variants held.
fn with_enums(w: &World, root: ItemKey, enums: &'static TestEnums, plant: CellPlant) -> Cells<'static, N> {
    w.expanded.borrow_mut().clear();
    let mut c = Cells::load_planted(w, root, None, plant, Alloc::global()).expect("the core loads");
    c.enums = Some(enums);
    let expand = |call: ItemKey, _: crate::Def| {
        w.expanded.borrow_mut().insert(call);
    };
    c.run(w, &expand, &[]).expect("the core runs");
    c
}

/// Enum paths (566 unknown imports on tempo, c10-r11c: `use Enum::Variant`, `use
/// self::Ordering::*` in `core::cmp`, which left every lexical read of `core::cmp` unknown: "lost
/// call Plain ["PartialEq"] -> Some(Outside)"). The Reference, `items.use.restrictions.variant`:
/// "`use MyEnum::MyVariant; //~ OK`"; `names.namespaces`: the type namespace holds "Struct,
/// union, enum, enum variant declarations", the value namespace "Enum variant constructors";
/// `vis.default`: "Enum variants in a pub enum are also public by default" (a variant is as
/// visible as its enum). The crate: `mod m { pub(crate) enum E { A, R { .. } } }`, at the root
/// `use m::E::A;` (types and values), `use m::E::R;` (the type alone: a record variant has no
/// constructor), `use m::E::Q;` (not found, never unknown), `mod g { pub use crate::m::E::*; }`
/// (the glob brings `A` no wider than `E` is read there: `pub(crate)`), and a derive call `Cl`
/// in `g` that the standard prelude answers (the enum glob holds no lexical read up).
/// Controls: `EnumIsOutside` leaves the imports and the call's module unknown (runner 11's
/// state), `EnumGlobNotClamped` leaves `g::A` public.
#[test]
fn a_path_through_an_enum_reads_its_variants() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let prelude = w.module(root, "prelude", V::Pub);
    let cl = w.item(prelude, "Cl", Namespace::Macros, crate::DefKind::Macro(crate::MacroShapes::DERIVE), V::Pub);
    w.cr(root).head.prelude = Some(prelude);
    let m = w.module(root, "m", V::Pub);
    let e = w.item(m, "E", Namespace::Types, crate::DefKind::Enum, V::Crate);
    let (a, r) = (w.fun(prelude, "variant_a", V::Priv), w.fun(prelude, "variant_r", V::Priv));
    let mut held = std::collections::BTreeMap::new();
    held.insert(e, std::vec![("A", a, false), ("R", r, true)]);
    let enums: &'static TestEnums = std::boxed::Box::leak(std::boxed::Box::new(TestEnums(held)));
    w.use_(root, "m::E::A", V::Priv);
    w.use_(root, "m::E::R", V::Priv);
    w.use_(root, "m::E::Q", V::Priv);
    let g = w.module(root, "g", V::Pub);
    w.glob(g, "crate::m::E", V::Pub);
    let call = w.call(g, "Cl", crate::CallShape::Derive);
    let mut ok = with_enums(&w, root, enums, CellPlant::None);
    assert_eq!(named_def(outcome(&ok, root, 0), Namespace::Types), Some(a), "{:?}", outcome(&ok, root, 0));
    assert_eq!(named_def(outcome(&ok, root, 0), Namespace::Values), Some(a), "a unit variant is a value");
    assert_eq!(named_def(outcome(&ok, root, 1), Namespace::Types), Some(r));
    assert_eq!(named_def(outcome(&ok, root, 1), Namespace::Values), None, "a record variant has no constructor");
    assert!(matches!(outcome(&ok, root, 2), Outcome::Failed(Unresolved::NotFound)), "{:?}", outcome(&ok, root, 2));
    assert_eq!(outcome(&ok, g, 0), Outcome::Glob(e), "the glob's source is the enum");
    let krate = crate::Visibility::Crate(crate::tests::crate_key(1));
    assert_eq!(ok.ask(g, Namespace::Values, "A").expect("asks"), Some(Value::Found(crate::Def { key: a, kind: crate::DefKind::Other }, krate)));
    assert!(matches!(ok.call_outcome(call), Some(Outcome::Called(d)) if d.key == cl), "{:?}", ok.call_outcome(call));
    // The edge as the record's list holds it: the enum under its own kind (c10-r12-zc2 wrote
    // it as a module: "globs_differ 21"), narrowed to the enum's visibility.
    let written = ok.written(g, Alloc::global()).expect("`g` is written");
    assert_eq!(written.globs.len(), 1);
    assert_eq!((written.globs[0].source, written.globs[0].vis), (crate::Def { key: e, kind: crate::DefKind::Enum }, krate));
    let mut bad = with_enums(&w, root, enums, CellPlant::EnumIsOutside);
    assert_eq!(outcome(&bad, root, 0), Outcome::Outside, "the control EnumIsOutside");
    assert_eq!(bad.ask(g, Namespace::Values, "A").expect("asks"), Some(Value::Outside), "the control EnumIsOutside, the glob");
    let mut bad = with_enums(&w, root, enums, CellPlant::EnumGlobNotClamped);
    assert!(matches!(bad.ask(g, Namespace::Values, "A").expect("asks"), Some(Value::Found(_, crate::Visibility::Public))), "the control EnumGlobNotClamped");
}

/// `root`'s crate solved with `dep`'s (crate 2) read as decided.
fn with_dep(w: &World, root: ItemKey, dep: ItemKey, plant: CellPlant) -> Cells<'static, N> {
    let d = cells_of(w, dep, CellPlant::None);
    let reader: &'static SolvedDep = std::boxed::Box::leak(std::boxed::Box::new(SolvedDep { cells: core::cell::RefCell::new(d), krate: crate::tests::crate_key(2), root: dep }));
    w.expanded.borrow_mut().clear();
    let mut c = Cells::load_planted(w, root, Some(reader), plant, Alloc::global()).expect("the core loads");
    let expand = |call: ItemKey, _: crate::Def| {
        w.expanded.borrow_mut().insert(call);
    };
    c.run(w, &expand, &[]).expect("the core runs");
    c
}

/// Step 1 of runner 9: a path into a dependency is an answer. Dependency `e` (crate 2): `pub mod
/// m { pub struct S; struct P; pub use crate::inner::*; }`, `mod inner { pub struct T; }`, and
/// `#[macro_export] macro_rules! mac`. The crate: `use e::m::S;` (S), `use e::m::P;` (private),
/// `use e::m::T;` (through the dependency's glob), `use e::m::Q;` (not found: never unknown),
/// `use ::e::m::S as S2;` (`::` reads the extern prelude), in `b`: `use e::m::*;` then
/// `use self::T as T2;` (the glob of a dependency's module brings its exported names), and
/// `use $crate::m::S as S3;` written as by a macro of `e`. Controls: `DepIsOutside` leaves S
/// unknown, `DepGlobEmpty` fails `T2`, `DollarCrateIsRoot` fails `S3`.
#[test]
fn a_path_into_a_dependency_reads_its_decided_names() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let dep = w.root(2, Edition::E2021);
    let m = w.module(dep, "m", V::Pub);
    let s = w.item(m, "S", Namespace::Types, crate::DefKind::Other, V::Pub);
    w.item(m, "P", Namespace::Types, crate::DefKind::Other, V::Priv);
    let inner = w.module(dep, "inner", V::Priv);
    let t = w.item(inner, "T", Namespace::Types, crate::DefKind::Other, V::Pub);
    w.glob(m, "crate::inner", V::Pub);
    w.dep(root, "e", dep);
    w.use_(root, "e::m::S", V::Pub);
    w.use_(root, "e::m::P", V::Pub);
    w.use_(root, "e::m::T", V::Pub);
    w.use_(root, "e::m::Q", V::Pub);
    w.use_as(root, "::e::m::S", "S2", V::Pub);
    let b = w.module(root, "b", V::Pub);
    w.glob(b, "e::m", V::Pub);
    w.use_as(b, "self::T", "T2", V::Pub);
    w.use_as(b, "x::m::S", "S3", V::Pub);
    let u = w.decls.get_mut(&b).and_then(|d| d.uses.last_mut()).expect("the use just written");
    u.path.kind = crate::PathKind::DollarCrate(crate::tests::crate_key(2));
    u.path.segments.remove(0);
    let ok = with_dep(&w, root, dep, CellPlant::None);
    assert_eq!(named_def(outcome(&ok, root, 0), Namespace::Types), Some(s));
    assert_eq!(outcome(&ok, root, 1), Outcome::Failed(Unresolved::Private));
    assert_eq!(named_def(outcome(&ok, root, 2), Namespace::Types), Some(t), "through the dependency's own glob: {:?}", outcome(&ok, root, 2));
    assert!(matches!(outcome(&ok, root, 3), Outcome::Failed(Unresolved::NotFound)), "{:?}", outcome(&ok, root, 3));
    assert_eq!(named_def(outcome(&ok, root, 4), Namespace::Types), Some(s), "`::e` is the extern prelude's");
    assert_eq!(named_def(outcome(&ok, b, 1), Namespace::Types), Some(t), "the dependency's glob layer");
    assert_eq!(named_def(outcome(&ok, b, 2), Namespace::Types), Some(s), "`$crate` of the dependency");
    assert!(ok.stats.dep_reads > 0 && ok.stats.dep_absent == 0, "{:?}", ok.stats);
    let bad = with_dep(&w, root, dep, CellPlant::DepIsOutside);
    assert_eq!(outcome(&bad, root, 0), Outcome::Outside, "the control DepIsOutside");
    let bad = with_dep(&w, root, dep, CellPlant::DepGlobEmpty);
    assert!(matches!(outcome(&bad, b, 1), Outcome::Failed(Unresolved::NotFound)), "the control DepGlobEmpty: {:?}", outcome(&bad, b, 1));
    let bad = with_dep(&w, root, dep, CellPlant::DollarCrateIsRoot);
    assert!(matches!(outcome(&bad, b, 2), Outcome::Failed(_)), "the control DollarCrateIsRoot: {:?}", outcome(&bad, b, 2));
}

/// The `#[macro_use]` prelude read from the dependency's root: a call `mac!()` of the crate
/// resolves to `e`'s exported `mac`; a call `nope!()` is not found (no longer unknown). Control:
/// `DepIsOutside` leaves `mac!` unknown.
#[test]
fn the_macro_use_prelude_reads_the_dependencys_exports() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let dep = w.root(2, Edition::E2021);
    let mac = w.rules_with(dep, "mac", true);
    w.cr(dep).macro_exports.push(("mac", mac, crate::MacroShapes::FN_LIKE));
    w.dep(root, "e", dep);
    w.cr(root).macro_use.push(dep);
    let call = w.call(root, "mac", crate::CallShape::FnLike);
    let nope = w.call(root, "nope", crate::CallShape::FnLike);
    let ok = with_dep(&w, root, dep, CellPlant::None);
    assert!(matches!(ok.call_outcome(call), Some(Outcome::Called(d)) if d.key == mac), "{:?}", ok.call_outcome(call));
    assert!(matches!(ok.call_outcome(nope), Some(Outcome::Failed(Unresolved::NotFound))), "{:?}", ok.call_outcome(nope));
    let bad = with_dep(&w, root, dep, CellPlant::DepIsOutside);
    assert_eq!(bad.call_outcome(call), Some(Outcome::Outside), "the control DepIsOutside");
}

/// An `extern crate` item binds its name in its module. The Reference, `items.extern-crate`:
/// "The external crate is then bound into the declaring scope as the identifier provided in the
/// extern crate declaration." Dependency `e` (crate 2): `pub struct x;`. The crate: `mod b {
/// extern crate e as k; use self::k::x as u; pub use self::k as r; extern crate nothere; }` and
/// `mod c { extern crate e; use self::e::x as v; }`. `u` and `v` are the dependency's `x`; `r` is
/// a public re-export of the private item (rustc's `Resolver::import`: a `use` of an `extern
/// crate` binding has the `use`'s own visibility, a lint at most); `nothere` is named as an
/// `extern crate` of no dependency. Controls: `ExternCrateBindsNothing` fails `u` as not found;
/// `ExternReexportNarrowed` leaves `r` private to `b`.
/// Prints: "test cells::tests::an_extern_crate_item_binds_its_name_in_its_module ... ok".
#[test]
fn an_extern_crate_item_binds_its_name_in_its_module() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let dep = w.root(2, Edition::E2021);
    let x = w.item(dep, "x", Namespace::Types, crate::DefKind::Other, V::Pub);
    w.dep(root, "e", dep);
    let b = w.module(root, "b", V::Pub);
    w.extern_crate(b, crate::ExternTarget::Dep("e"), crate::Rename::To("k"), V::Priv);
    w.use_as(b, "self::k::x", "u", V::Priv);
    w.use_as(b, "self::k", "r", V::Pub);
    w.extern_crate(b, crate::ExternTarget::Dep("nothere"), crate::Rename::Same, V::Priv);
    let c = w.module(root, "c", V::Pub);
    w.extern_crate(c, crate::ExternTarget::Dep("e"), crate::Rename::Same, V::Priv);
    w.use_as(c, "self::e::x", "v", V::Priv);
    let ok = with_dep(&w, root, dep, CellPlant::None);
    assert_eq!(named_def(outcome(&ok, b, 0), Namespace::Types), Some(x), "`u`: {:?}", outcome(&ok, b, 0));
    assert_eq!(named_def(outcome(&ok, c, 0), Namespace::Types), Some(x), "`v`: {:?}", outcome(&ok, c, 0));
    let r = |c: &Cells<'static, N>| match outcome(c, b, 1) {
        Outcome::Named(n) => n[super::ns_at(Namespace::Types)],
        other => panic!("`r` is not bound: {other:?}"),
    };
    assert_eq!(r(&ok), Some((crate::Def::module(dep), crate::Visibility::Public)), "`pub use self::k as r;`");
    assert_eq!(&ok.bad_extern[..], &[(b, "nothere")][..]);
    let bad = with_dep(&w, root, dep, CellPlant::ExternCrateBindsNothing);
    assert!(matches!(outcome(&bad, b, 0), Outcome::Failed(Unresolved::NotFound)), "the control ExternCrateBindsNothing: {:?}", outcome(&bad, b, 0));
    let bad = with_dep(&w, root, dep, CellPlant::ExternReexportNarrowed);
    assert_ne!(r(&bad).map(|b| b.1), Some(crate::Visibility::Public), "the control ExternReexportNarrowed");
}

/// R8 (bottom.md section 6): "a macro-written root `extern crate X as n;` may add a name
/// (`names.preludes.extern`), seen from every module, never replace one (an error, whether or
/// not a path reads it; D56)". The Reference, `names.preludes.extern`: "External crates imported
/// with `extern crate` in the root module or provided to the compiler (as with the `--extern`
/// flag) are added to the extern prelude." Dependency `n` (crate 2): `pub struct x;`; crate 3 a
/// dependency named `other`. The crate: `macro_rules! mk; mk!{ extern crate n as k; extern
/// crate other as n; }` and `mod b { use k::x as u; use n::x as t; }`. `u` is crate 2's `x`
/// through the added name `k`, read from a module that is not the root; `t` stays crate 2's `x`
/// and the second `extern crate` is R8's error. Controls: `R8AddsNothing` fails `u` as not
/// found; `R8MacroReplaces` takes `n` as crate 3, where `t` is no longer crate 2's `x`.
/// Prints: "test cells::tests::r8_a_macro_written_root_extern_crate_adds_a_name_and_never_replaces_one ... ok".
#[test]
fn r8_a_macro_written_root_extern_crate_adds_a_name_and_never_replaces_one() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let dep = w.root(2, Edition::E2021);
    let x = w.item(dep, "x", Namespace::Types, crate::DefKind::Other, V::Pub);
    let other = w.root(3, Edition::E2021);
    w.dep(root, "n", dep);
    w.dep(root, "other", other);
    w.rules(root, "mk");
    let call = w.call(root, "mk", crate::CallShape::FnLike);
    w.extern_crate(call, crate::ExternTarget::Dep("n"), crate::Rename::To("k"), V::Priv);
    w.extern_crate(call, crate::ExternTarget::Dep("other"), crate::Rename::To("n"), V::Priv);
    let b = w.module(root, "b", V::Pub);
    w.use_as(b, "k::x", "u", V::Priv);
    w.use_as(b, "n::x", "t", V::Priv);
    let ok = with_dep(&w, root, dep, CellPlant::None);
    assert_eq!(named_def(outcome(&ok, b, 0), Namespace::Types), Some(x), "`u`: {:?}", outcome(&ok, b, 0));
    assert_eq!(named_def(outcome(&ok, b, 1), Namespace::Types), Some(x), "`t`: {:?}", outcome(&ok, b, 1));
    let replaced: Vec<_> = ok.r8.iter().map(|r| r.1.name).collect();
    assert_eq!(&replaced[..], &["n"][..], "R8's error names the replaced name");
    let bad = with_dep(&w, root, dep, CellPlant::R8AddsNothing);
    assert!(matches!(outcome(&bad, b, 0), Outcome::Failed(Unresolved::NotFound)), "the control R8AddsNothing: {:?}", outcome(&bad, b, 0));
    let bad = with_dep(&w, root, dep, CellPlant::R8MacroReplaces);
    assert_ne!(named_def(outcome(&bad, b, 1), Namespace::Types), Some(x), "the control R8MacroReplaces: {:?}", outcome(&bad, b, 1));
}

/// A `mod c;` whose text the facts do not hold (its file was not read, or an attribute macro on
/// the item was not expanded: std, tokio and reqwest on tempo's graph). The Reference,
/// `items.mod`: a module item "introduces a new, named module into the tree of modules making
/// up a crate": the name is bound in its parent whatever its inside is. `mod c;` (no text),
/// `use self::c as d;` binds the module; `use self::c::x as u;` is unknown (`Outside`), never
/// "not found"; the module is listed as unread. Control `UnreadModuleRefuses`: the load refuses
/// the whole crate, as it did before.
/// Prints: "test cells::tests::a_mod_line_without_a_text_is_bound_and_unknown_inside ... ok".
#[test]
fn a_mod_line_without_a_text_is_bound_and_unknown_inside() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let c = w.module(root, "c", V::Pub);
    w.decls.remove(&c);
    w.use_as(root, "self::c", "d", V::Priv);
    w.use_as(root, "self::c::x", "u", V::Priv);
    let ok = cells_of(&w, root, CellPlant::None);
    assert_eq!(named_def(outcome(&ok, root, 0), Namespace::Types), Some(c), "`d`: {:?}", outcome(&ok, root, 0));
    assert_eq!(outcome(&ok, root, 1), Outcome::Outside, "`u` is unknown");
    assert_eq!(&ok.unread[..], &[c][..]);
    let written = ok.written(root, Alloc::global()).expect("the root is a module");
    assert_eq!(written.unknown, 1, "the root holds one import that is unknown");
    assert!(ok.written(c, Alloc::global()).is_none(), "an unread module has no lists");
    let refused = Cells::load_planted(&w, root, None, CellPlant::UnreadModuleRefuses, Alloc::global());
    assert!(matches!(refused, Err(super::CellError::Facts { module, .. }) if module == c), "the control UnreadModuleRefuses");
}

/// `use self::b::k::{self};` binds `k` in the type namespace alone. The Reference,
/// `items.use.self`: "`self` may be used as the last segment in a brace syntax ... to import
/// the parent entity under its own name. This only creates a binding in the type namespace."
/// `mod b { pub mod k {} pub fn k() {} }` and `use self::b::k::{self} as u;`: `u` is the module,
/// no function. Control `SelfImportBindsAll`: `u` is the function too.
/// Prints: "test cells::tests::a_self_import_binds_the_type_namespace_alone ... ok".
#[test]
fn a_self_import_binds_the_type_namespace_alone() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let b = w.module(root, "b", V::Pub);
    let k = w.module(b, "k", V::Pub);
    w.fun(b, "k", V::Pub);
    w.use_as(root, "self::b::k", "u", V::Priv);
    let last = w.decls.get_mut(&root).expect("the root's text").uses.last_mut().expect("the import");
    last.leaf = crate::UseForm::Named { rename: crate::Rename::To("u"), types_only: true };
    let ok = cells_of(&w, root, CellPlant::None);
    assert_eq!(named_def(outcome(&ok, root, 0), Namespace::Types), Some(k));
    assert_eq!(named_def(outcome(&ok, root, 0), Namespace::Values), None, "no value is bound");
    let bad = Cells::load_planted(&w, root, None, CellPlant::SelfImportBindsAll, Alloc::global());
    let mut bad = bad.unwrap_or_else(|e| panic!("the core loads: {e}"));
    bad.run(&w, &|_, _| {}, &[]).unwrap_or_else(|e| panic!("the core runs: {e}"));
    assert!(named_def(outcome(&bad, root, 0), Namespace::Values).is_some(), "the control SelfImportBindsAll");
}

/// A module no `mod` line names (a block's module, made beside its body's module) is loaded
/// when the caller tells of it, solved by a second run, and written in the record's forms:
/// bindings sorted by namespace then name, each with its origin. `pub struct S;` at the root;
/// the block's module (parent: the root) holds `struct Q;` and `use super::S as T;`. Before it
/// is told of, the core has no lists for it (the control: the shadow counted its bindings as
/// different); after, `Q` is declared and `T` is an import of `S`.
/// Prints: "test cells::tests::a_module_no_mod_line_names_is_loaded_later_and_written ... ok".
#[test]
fn a_module_no_mod_line_names_is_loaded_later_and_written() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let s = w.item(root, "S", Namespace::Types, crate::DefKind::Other, V::Pub);
    let blk = w.module(root, "blk", V::Priv);
    w.decls.get_mut(&root).expect("the root's text").mods.retain(|c| c.module != blk);
    let q = w.item(blk, "Q", Namespace::Types, crate::DefKind::Other, V::Priv);
    w.use_as(blk, "super::S", "T", V::Priv);
    let mut c = cells_of(&w, root, CellPlant::None);
    assert!(c.written(blk, Alloc::global()).is_none(), "the control: not told of, no lists");
    assert_eq!(c.load_more(&w, &[blk]).map_err(|e| format!("{e}")), Ok(1));
    c.run(&w, &|_, _| {}, &[]).unwrap_or_else(|e| panic!("the core runs: {e}"));
    let written = c.written(blk, Alloc::global()).expect("told of, it is a module");
    assert_eq!(written.unknown, 0);
    let got: Vec<(N, ItemKey, crate::Origin)> = written.bindings.iter().map(|e| (e.name, e.binding.def.key, e.binding.origin)).collect();
    assert_eq!(got, [("Q", q, crate::Origin::Declared), ("T", s, crate::Origin::Import)]);
    assert!(written.globs.is_empty());
}

/// async_compression, icu_locale_core, wincode and core on tempo's graph (c10-r11a: 104 imports
/// "unknown import TextualImport", and behind them the calls through the re-export, unknown,
/// whose expansions' items the cells then lacked: "differ" with "cells None"). The Reference,
/// `macro.decl.scope.path.reexport`: "Macros can be re-exported to give them path-based scope
/// from a module other than the crate root." (its example: `macro_rules! m { () => {}; }
/// pub(crate) use m;`, "Reexport with path-based scope from within `m`'s textual scope");
/// `macro.decl.scope.path-based.visibility`: "Macros have an implicit visibility of
/// `pub(crate)`. `#[macro_export]` changes the implicit visibility to `pub`." The crate: `mod
/// generic { pub(crate) use late; macro_rules! impl_decoder; pub(crate) use impl_decoder;
/// macro_rules! late; #[macro_export] macro_rules! ex; pub use ex as ex2; }` and `mod b {
/// crate::generic::impl_decoder! {} }` writing `pub struct Decoder;`. `impl_decoder` is bound in
/// `generic`'s macro namespace, crate-wide; `late` is not found (the import stands before the
/// definition: outside its textual scope); `ex2` is public; the call through the re-export
/// expands and `Decoder` stands in `b`. The reference solver agrees on every import. Control
/// `NoTextualImport`: the import is unknown and `Decoder` is not there.
/// Prints: "test cells::tests::a_use_of_a_macro_rules_name_reads_the_textual_scope ... ok".
#[test]
fn a_use_of_a_macro_rules_name_reads_the_textual_scope() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let generic = w.module(root, "generic", V::Pub);
    w.use_(generic, "late", V::Crate);
    let dec = w.rules(generic, "impl_decoder");
    w.use_(generic, "impl_decoder", V::Crate);
    w.rules(generic, "late");
    let ex = w.rules_with(generic, "ex", true);
    w.use_as(generic, "ex", "ex2", V::Pub);
    let b = w.module(root, "b", V::Pub);
    let call = w.call(b, "crate::generic::impl_decoder", crate::CallShape::FnLike);
    let made = w.item(call, "Decoder", Namespace::Types, crate::DefKind::Other, V::Pub);
    let macros = |c: &Cells<'static, N>, index: u32| match outcome(c, generic, index) {
        Outcome::Named(n) => n[super::ns_at(Namespace::Macros)].map(|(d, v)| (d.key, v)),
        other => panic!("import {index} is not bound: {other:?}"),
    };
    let mut ok = cells_of(&w, root, CellPlant::None);
    assert!(matches!(outcome(&ok, generic, 0), Outcome::Failed(Unresolved::NotFound)), "`use late;`: {:?}", outcome(&ok, generic, 0));
    assert_eq!(macros(&ok, 1), Some((dec, crate::Visibility::Crate(crate::tests::crate_key(1)))), "`pub(crate) use impl_decoder;`");
    assert_eq!(macros(&ok, 2), Some((ex, crate::Visibility::Public)), "`pub use ex as ex2;`");
    assert_eq!(found(ok.ask(b, Namespace::Types, "Decoder").expect("asks")), Some(made), "the call through the re-export expands");
    compare(&w, root, &ok).expect("the reference solver agrees");
    let mut bad = cells_of(&w, root, CellPlant::NoTextualImport);
    assert!(matches!(outcome(&bad, generic, 1), Outcome::Outside), "the control: {:?}", outcome(&bad, generic, 1));
    assert_ne!(found(bad.ask(b, Namespace::Types, "Decoder").expect("asks")), Some(made), "the control, `Decoder`");
}

/// tempo_contracts on tempo's graph (c10-r11a: 897 of its imports "unknown import CrateAlone Abs
/// ["alloy_sol_types"]": `use ::alloy_sol_types as alloy_sol_types;` in each `sol!` expansion;
/// 933 on the whole graph). The Reference, `paths.qualifiers.global-root.edition2018`:
/// "Beginning with the 2018 Edition, paths starting with `::` resolve from crates in the extern
/// prelude. That is, they must be followed by the name of a crate." Dependency `e` (crate 2):
/// `pub struct x;`. The crate: `mod b { use ::e as k; use self::k::x as u; pub use ::e; use
/// ::nothere as z; }`. `k` is the crate `e` itself, private to `b`; `u` is its `x`; `pub use
/// ::e;` is public; `::nothere` is not found. The reference solver agrees on the three `::`
/// imports. Control `CrateAloneUnknown`: `k` and with it `u` are unknown.
/// Prints: "test cells::tests::a_use_of_a_crate_alone_binds_the_crate ... ok".
#[test]
fn a_use_of_a_crate_alone_binds_the_crate() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let dep = w.root(2, Edition::E2021);
    let x = w.item(dep, "x", Namespace::Types, crate::DefKind::Other, V::Pub);
    w.dep(root, "e", dep);
    let b = w.module(root, "b", V::Pub);
    w.use_as(b, "::e", "k", V::Priv);
    w.use_as(b, "self::k::x", "u", V::Priv);
    w.use_(b, "::e", V::Pub);
    w.use_as(b, "::nothere", "z", V::Priv);
    let types = |c: &Cells<'static, N>, index: u32| match outcome(c, b, index) {
        Outcome::Named(n) => n[super::ns_at(Namespace::Types)],
        other => panic!("import {index} is not bound: {other:?}"),
    };
    let ok = with_dep(&w, root, dep, CellPlant::None);
    assert_eq!(types(&ok, 0), Some((crate::Def::module(dep), crate::Visibility::Restricted(b))), "`use ::e as k;`");
    assert_eq!(named_def(outcome(&ok, b, 1), Namespace::Types), Some(x), "`u`: {:?}", outcome(&ok, b, 1));
    assert_eq!(types(&ok, 2), Some((crate::Def::module(dep), crate::Visibility::Public)), "`pub use ::e;`");
    assert!(matches!(outcome(&ok, b, 3), Outcome::Failed(Unresolved::NotFound)), "`::nothere`: {:?}", outcome(&ok, b, 3));
    let reference = reference_imports(&w, root);
    for index in [0, 2] {
        match reference.get(&(b, index)) {
            Some(Imp::Named(n)) => assert_eq!(n[0].map(|y: B| (y.def, y.vis)), types(&ok, index), "the reference, import {index}"),
            other => panic!("the reference, import {index}: {other:?}"),
        }
    }
    assert!(matches!(reference.get(&(b, 3)), Some(Imp::Failed(Unresolved::NotFound))), "the reference, `::nothere`: {:?}", reference.get(&(b, 3)));
    let bad = with_dep(&w, root, dep, CellPlant::CrateAloneUnknown);
    assert!(matches!(outcome(&bad, b, 0), Outcome::Outside), "the control, `k`: {:?}", outcome(&bad, b, 0));
    assert!(matches!(outcome(&bad, b, 1), Outcome::Outside), "the control, `u`: {:?}", outcome(&bad, b, 1));
}

/// `use crate as name;` (serde's `pub use crate as serde;` in its `private` module, which every
/// serde derive's `use serde::private::serde as _serde;` reads; found when the cell core became
/// the writer: tests::blocks::blocks_read_a_crate_path_given_to_serde_s_derive "`_serde`:
/// segment 0: not found"). The Reference, `items.use.path`: a `use` path's prefix `crate`
/// names the crate root; a path of `crate` alone names that module, and with `as name` binds
/// it in the type namespace like any imported module. The crate: `mod private { pub use crate
/// as serde; }`, `use private::serde::x as y;`, `pub struct x;`. `serde` is the root, public;
/// `y` is `x`. Control `CrateRootImportFails`: `serde` fails, and with it `y`.
/// Prints: "test cells::tests::a_use_of_crate_alone_binds_the_root ... ok".
#[test]
fn a_use_of_crate_alone_binds_the_root() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let x = w.item(root, "x", Namespace::Types, crate::DefKind::Other, V::Pub);
    let private = w.module(root, "private", V::Pub);
    w.use_as(private, "crate", "serde", V::Pub);
    w.use_as(root, "private::serde::x", "y", V::Priv);
    let ok = cells_of(&w, root, CellPlant::None);
    match outcome(&ok, private, 0) {
        Outcome::Named(n) => assert_eq!(n[super::ns_at(Namespace::Types)], Some((crate::Def::module(root), crate::Visibility::Public)), "`pub use crate as serde;`"),
        other => panic!("`serde` is not bound: {other:?}"),
    }
    assert_eq!(named_def(outcome(&ok, root, 0), Namespace::Types), Some(x), "`y`: {:?}", outcome(&ok, root, 0));
    let bad = cells_of(&w, root, CellPlant::CrateRootImportFails);
    assert!(matches!(outcome(&bad, private, 0), Outcome::Failed(_)), "the control, `serde`: {:?}", outcome(&bad, private, 0));
    assert!(matches!(outcome(&bad, root, 0), Outcome::Failed(_)), "the control, `y`: {:?}", outcome(&bad, root, 0));
}

/// tempo_contracts and alloy_provider (c10-r17a: "scope errors old 0 cells 2; .. Macro { ..
/// segments: [\"rustfmt\", \"skip\"] }, segment: 0, why: NotFound }"). The Reference,
/// `attributes.tool`: "Tool attributes .. allow external tools .. The tool's name must be a
/// tool known to the compiler", `rustfmt` and `clippy` among them. The crate: `#[rustfmt::skip]
/// fn f() {}` with `rustfmt` in the root record's tools. The written scope holds no error; the
/// control `ToolAttrIsACall` makes the attribute a call whose path fails at `rustfmt`.
#[test]
fn a_tool_attribute_is_inert() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    w.cr(root).tools.push("rustfmt");
    w.call_on(root, "rustfmt::skip", crate::CallShape::Attr, Some(0));
    assert_eq!(written_errors(&w, root, CellPlant::None), "[]", "a tool attribute is no call");
    let bad = written_errors(&w, root, CellPlant::ToolAttrIsACall);
    assert!(bad.contains("rustfmt") && bad.contains("NotFound"), "the control: {bad}");
}

/// The written scope of `c` planted with `plant` at the load, its errors as text.
fn written_errors(w: &World, root: ItemKey, plant: CellPlant) -> String {
    w.expanded.borrow_mut().clear();
    let mut c = Cells::load_planted(w, root, None, plant, Alloc::global()).expect("the load");
    let expand = |call: ItemKey, _: crate::Def| {
        w.expanded.borrow_mut().insert(call);
    };
    c.run(w, &expand, &[]).expect("the core runs");
    let ix = c.write_index(Alloc::global());
    let (scope, unknown) = c.scope(&ix, root, Alloc::global()).expect("the root is written");
    assert_eq!(unknown, 0, "{:?}", scope.errors);
    format!("{:?}", scope.errors)
}

/// evm2 and reth_engine_tree (c10-r17a: "scope errors old 9 cells 2; error old
/// Some(RecursionLimit { .. }) cells None"). rustc's default `recursion_limit` is 128 (Reference
/// `attributes.limits.recursion_limit`: "the maximum depth for potentially infinitely-recursive
/// compile-time operations like macro expansion"). The crate: `macro_rules! m {}` and a chain of
/// 131 calls `m!()`, each written in the expansion before it. The call at depth 128 is the
/// scope's one `RecursionLimit` error and is not expanded; the control `NoRecursionLimit`
/// expands the whole chain and writes no error.
#[test]
fn a_call_at_the_recursion_limit_is_an_error() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    w.rules(root, "m");
    let mut at = root;
    let mut chain = Vec::new();
    for _ in 0..131 {
        at = w.call(at, "m", crate::CallShape::FnLike);
        chain.push(at);
    }
    // A root call after the chain: its textual read walks down every expansion before it to the
    // deep call, which must count as grown (c10-r17c hung there in a quiet-point loop).
    w.call(root, "m", crate::CallShape::FnLike);
    let deep = format!("RecursionLimit {{ call: {:?} }}", chain[128]);
    assert_eq!(written_errors(&w, root, CellPlant::None), format!("[{deep}]"), "the call at depth 128");
    assert_eq!(written_errors(&w, root, CellPlant::NoRecursionLimit), "[]", "the control");
}

/// mach2 0.5.0 on tempo's graph (edition 2015; c10-g3: "differ 153, cells failed 153, stalls 1").
/// The Reference, `items.use.path.edition2018`: "In the 2015 edition, `use` paths are relative to
/// the crate root." and "The 2015 edition does not allow use declarations to reference the extern
/// prelude."; `vis.scoped.edition2018`: "The 2015 edition may also use paths starting with `::`
/// or modules from the crate root." The crate (2015), mach2's shape: `mod core_ { pub mod mem {}
/// }` (the injected `extern crate core;`), `use core_::mem;`, `pub mod kern_return { pub struct
/// kern_return_t; }`, `pub mod task_info { mod kern_return {} use mem; use
/// kern_return::kern_return_t; pub(in task_info) struct S; use e::x as u; }` with a dependency
/// `e` (`pub struct x;`) that no `extern crate` names. `use mem;` is the root's `mem` (read
/// lexically it waits on itself: the stall); `kern_return` is the root's module, not the empty
/// one beside the import; `S` is visible in `task_info`; `u` is not found (no extern prelude).
/// The reference solver agrees on every import. Control `Use2015Lexical`: `use mem;` and the
/// `kern_return` import fail, `u` resolves, and `pub(in task_info)` is `task_info`'s
/// `RestrictedNotAncestor` error (names-G1: never a refused load).
/// Prints: "test cells::tests::a_2015_use_path_starts_at_the_crate_root ... ok".
#[test]
fn a_2015_use_path_starts_at_the_crate_root() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2015);
    let dep = w.root(2, Edition::E2021);
    w.item(dep, "x", Namespace::Types, crate::DefKind::Other, V::Pub);
    w.dep(root, "e", dep);
    let core_ = w.module(root, "core_", V::Priv);
    let mem = w.module(core_, "mem", V::Pub);
    w.use_(root, "core_::mem", V::Priv);
    let kr = w.module(root, "kern_return", V::Pub);
    let t = w.item(kr, "kern_return_t", Namespace::Types, crate::DefKind::Other, V::Pub);
    let ti = w.module(root, "task_info", V::Pub);
    w.module(ti, "kern_return", V::Priv);
    w.use_(ti, "mem", V::Priv);
    w.use_(ti, "kern_return::kern_return_t", V::Priv);
    w.use_as(ti, "e::x", "u", V::Priv);
    let ok = with_dep(&w, root, dep, CellPlant::None);
    assert_eq!(named_def(outcome(&ok, ti, 0), Namespace::Types), Some(mem), "`use mem;`: {:?}", outcome(&ok, ti, 0));
    assert_eq!(named_def(outcome(&ok, ti, 1), Namespace::Types), Some(t), "`use kern_return::kern_return_t;`: {:?}", outcome(&ok, ti, 1));
    assert!(matches!(outcome(&ok, ti, 2), Outcome::Failed(Unresolved::NotFound)), "`use e::x as u;`: {:?}", outcome(&ok, ti, 2));
    assert_eq!(ok.stats.stalls, 0, "no import waits on itself");
    compare(&w, root, &ok).expect("the reference solver agrees");
    let bad = with_dep(&w, root, dep, CellPlant::Use2015Lexical);
    assert!(matches!(outcome(&bad, ti, 0), Outcome::Failed(_)), "the control, `use mem;`: {:?}", outcome(&bad, ti, 0));
    assert!(matches!(outcome(&bad, ti, 1), Outcome::Failed(_)), "the control, `kern_return`: {:?}", outcome(&bad, ti, 1));
    assert!(matches!(outcome(&bad, ti, 2), Outcome::Named(_)), "the control, `u`: {:?}", outcome(&bad, ti, 2));
    // `pub(in task_info)`: an ancestor named from the crate root.
    w.item(ti, "S", Namespace::Types, crate::DefKind::Other, V::In("task_info"));
    let mut vis = cells_of(&w, root, CellPlant::None);
    assert!(matches!(vis.ask(ti, Namespace::Types, "S").expect("asks"), Some(Value::Found(_, crate::Visibility::Restricted(k))) if k == ti));
    // The control: a 2015 path from no module names no ancestor, the item's own error in
    // `task_info`'s scope (names-G1: an error at the item, never a refused load).
    let alloc = blaze_own::testing::heap().alloc();
    let good = with_dep(&w, root, dep, CellPlant::None);
    let gix = good.write_index(alloc);
    let (gscope, _) = good.scope(&gix, ti, alloc).expect("`task_info` is written");
    assert!(!gscope.errors.iter().any(|e| matches!(e, blaze_surface::ScopeError::RestrictedNotAncestor { .. })), "`pub(in task_info)`: {:?}", gscope.errors);
    let bad = with_dep(&w, root, dep, CellPlant::Use2015Lexical);
    let ix = bad.write_index(alloc);
    let (scope, _) = bad.scope(&ix, ti, alloc).expect("`task_info` is written");
    assert!(
        scope.errors.iter().any(|e| matches!(e, blaze_surface::ScopeError::RestrictedNotAncestor { .. })),
        "the control, `pub(in task_info)`: {:?}",
        scope.errors
    );
}

/// Two derives of one item named through a glob (zerocopy's `byteorder`: `use super::*;` and
/// `derive(KnownLayout, Immutable, FromBytes, IntoBytes, Unaligned)`; c10-r11c: "lost call Plain
/// ["IntoBytes"] -> Some(Failed(NotFound))", zerocopy "stalls 90"). The Reference,
/// `names.resolution.expansion.speculation`: "All name resolution candidates selected during
/// macro expansion are considered speculative. Once the crate has been fully expanded, all
/// speculative import resolutions are validated to ensure that macro expansion did not introduce
/// any new ambiguities."; `names.resolution.expansion.unresolved-invocations`: "Every macro
/// invocation resolves to a valid definition that exists in the final AST or in an external
/// crate." So each call takes the glob's candidate at once, without waiting for the module's
/// other calls, and a later write of the name by an expansion is the error. The crate: `mod defs
/// { pub macro D1; pub macro D2; pub macro mk; }`, `mod a { use crate::defs::*; #[derive(D1, D2)]
/// struct S; }`; in `b` the same with `crate::defs::mk!()` whose expansion writes a macro `D1`
/// into `b` (the time-travel error at `b`'s `D1` call, whose answer stays the glob's `D1` in
/// either order). Controls: `MacroGlobWaits` stalls `a` and fails a derive of it, and reports
/// no error in `b` (it takes the written `D1`).
#[test]
fn two_calls_named_through_a_glob_do_not_wait_on_each_other() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let defs = w.module(root, "defs", V::Pub);
    let derive = crate::DefKind::Macro(crate::MacroShapes::DERIVE);
    let d1 = w.item(defs, "D1", Namespace::Macros, derive, V::Pub);
    let d2 = w.item(defs, "D2", Namespace::Macros, derive, V::Pub);
    w.item(defs, "mk", Namespace::Macros, crate::DefKind::Macro(crate::MacroShapes::FN_LIKE), V::Pub);
    let a = w.module(root, "a", V::Pub);
    w.glob(a, "crate::defs", V::Priv);
    let c1 = w.call(a, "D1", crate::CallShape::Derive);
    let c2 = w.call(a, "D2", crate::CallShape::Derive);
    let b = w.module(root, "b", V::Pub);
    w.glob(b, "crate::defs", V::Priv);
    let b1 = w.call(b, "D1", crate::CallShape::Derive);
    let mk = w.call(b, "crate::defs::mk", crate::CallShape::FnLike);
    w.item(mk, "D1", Namespace::Macros, derive, V::Pub);
    let ok = cells_of(&w, root, CellPlant::None);
    assert!(matches!(ok.call_outcome(c1), Some(Outcome::Called(d)) if d.key == d1), "{:?}", ok.call_outcome(c1));
    assert!(matches!(ok.call_outcome(c2), Some(Outcome::Called(d)) if d.key == d2), "{:?}", ok.call_outcome(c2));
    assert!(matches!(ok.call_outcome(b1), Some(Outcome::Called(d)) if d.key == d1), "{:?}", ok.call_outcome(b1));
    assert_eq!(ok.stats.r6_errors, 1, "the later write of `D1` into `b` is the error");
    let bad = cells_of(&w, root, CellPlant::MacroGlobWaits);
    // The two wait on each other, the group stalls and the stall fails a derive: the plant
    // turns the one rule off (`first_segment`'s `speculative`), mid-run and at the stall alike.
    // (Runner 14's separate stall pick did not know the plant, and this control had shrunk to
    // the stall count; since the stall asks `first_segment` itself it fails a derive again.)
    let failed = [c1, c2].iter().filter(|c| matches!(bad.call_outcome(**c), Some(Outcome::Failed(Unresolved::NotFound)))).count();
    assert!(failed >= 1 && bad.stats.stalls > ok.stats.stalls, "the control MacroGlobWaits: {:?} {:?}, stalls {}", bad.call_outcome(c1), bad.call_outcome(c2), bad.stats.stalls);
    assert!(matches!(bad.call_outcome(b1), Some(Outcome::Called(d)) if d.key != d1) && bad.stats.r6_errors == 0, "the control MacroGlobWaits in `b`: {:?}", bad.call_outcome(b1));
}

/// icu_normalizer and icu_properties on tempo's graph (c10-r13c: "lost call Plain ["yoke",
/// "Yokeable"] -> Some(Failed(NotFound))" and `["zerofrom", "ZeroFrom"]`, 16 in all:
/// `use icu_provider::prelude::*;` brings `yoke` and `zerofrom`, and each struct is
/// `#[derive(.., yoke::Yokeable, zerofrom::ZeroFrom)]`). The Reference,
/// `names.resolution.expansion.speculation`: "All name resolution candidates selected during
/// macro expansion are considered speculative. Once the crate has been fully expanded, all
/// speculative import resolutions are validated to ensure that macro expansion did not introduce
/// any new ambiguities." The first segment of a call's path is such a candidate, in the type
/// namespace: it is taken from the glob at once, with a watch, as the macro's own name is. The
/// crate: `mod defs { pub mod yoke { pub macro D1; pub macro D2; } }`, `mod a { use
/// crate::defs::*; #[derive(yoke::D1, yoke::D2)] struct S; }`. Both derives resolve. Control
/// `CallPrefixGlobWaits`: each waits on the other's growth, the group stalls and a derive fails.
/// Prints: "test cells::tests::a_calls_first_segment_through_a_glob_does_not_wait ... ok".
#[test]
fn a_calls_first_segment_through_a_glob_does_not_wait() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let defs = w.module(root, "defs", V::Pub);
    let yoke = w.module(defs, "yoke", V::Pub);
    let derive = crate::DefKind::Macro(crate::MacroShapes::DERIVE);
    let d1 = w.item(yoke, "D1", Namespace::Macros, derive, V::Pub);
    let d2 = w.item(yoke, "D2", Namespace::Macros, derive, V::Pub);
    let a = w.module(root, "a", V::Pub);
    w.glob(a, "crate::defs", V::Priv);
    let c1 = w.call(a, "yoke::D1", crate::CallShape::Derive);
    let c2 = w.call(a, "yoke::D2", crate::CallShape::Derive);
    let ok = cells_of(&w, root, CellPlant::None);
    assert!(matches!(ok.call_outcome(c1), Some(Outcome::Called(d)) if d.key == d1), "{:?}", ok.call_outcome(c1));
    assert!(matches!(ok.call_outcome(c2), Some(Outcome::Called(d)) if d.key == d2), "{:?}", ok.call_outcome(c2));
    assert_eq!(ok.stats.r6_errors, 0);
    let bad = cells_of(&w, root, CellPlant::CallPrefixGlobWaits);
    let failed = [c1, c2].iter().filter(|c| matches!(bad.call_outcome(**c), Some(Outcome::Failed(Unresolved::NotFound)))).count();
    assert!(failed >= 1 && bad.stats.stalls > ok.stats.stalls, "the control CallPrefixGlobWaits: {:?} {:?}, stalls {}", bad.call_outcome(c1), bad.call_outcome(c2), bad.stats.stalls);
}

/// zerocopy on tempo's graph (c10-r13a, `--roots zerocopy`: "lost call Plain
/// ["impl_for_transmute_from"] -> Some(Failed(NotFound))", 66 of them, each in a different
/// module: the macro's own expansion is `const _: () = { .. impl_for_transmute_from!(@..); .. }`,
/// and a body's block that holds an impl is a module of its own with no `mod` line). The
/// Reference, `macro.decl.scope.textual`: "Textual scope is based largely on the order that
/// things appear in source files, and works similarly to the scope of local variables declared
/// with `let` except it also applies at the module level. When `macro_rules!` is used to define
/// a macro, the macro enters the scope after the definition (note that it can still be used
/// recursively, since names are looked up from the invocation site), up until its surrounding
/// scope, typically a module, is closed." So a block reads what is in scope where its body is
/// written. The crate: `macro_rules! far; macro_rules! mk; mod b { macro_rules! before;
/// const _: () = { far!(); before!(); after!(); }; macro_rules! after; } mod e { mk!(); }`,
/// `mk!()` writing `macro_rules! inner; const _: () = { inner!(); far!(); };`. `far`, `before`
/// and `inner` resolve (the body's place is in `b`'s text after `before`, and in the expansion
/// after `inner`); `after` is defined after the body and is not found. Control
/// `BlockHasNoPlace`: every one of them is not found.
/// Prints: "test cells::tests::a_blocks_module_reads_the_textual_scope_around_its_body ... ok".
#[test]
fn a_blocks_module_reads_the_textual_scope_around_its_body() {
    let fn_like = crate::CallShape::FnLike;
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let far = w.rules(root, "far");
    w.rules(root, "mk");
    let b = w.module(root, "b", V::Pub);
    let before = w.rules(b, "before");
    w.rules(b, "after");
    let blk = w.module(b, "blk", V::Priv);
    w.decls.get_mut(&b).expect("b's text").mods.retain(|c| c.module != blk);
    let (c_far, c_before, c_after) = (w.call(blk, "far", fn_like), w.call(blk, "before", fn_like), w.call(blk, "after", fn_like));
    let e = w.module(root, "e", V::Pub);
    let mk = w.call(e, "mk", fn_like);
    let inner = w.rules(mk, "inner");
    let blk2 = w.module(e, "blk2", V::Priv);
    w.decls.get_mut(&e).expect("e's text").mods.retain(|c| c.module != blk2);
    let (c_inner, c_far2) = (w.call(blk2, "inner", fn_like), w.call(blk2, "far", fn_like));
    // `before` is `b`'s first ordered entry, `inner` the expansion's first: each body follows it.
    let place = |m: ItemKey| if m == blk { Some((b, 1)) } else if m == blk2 { Some((mk, 1)) } else { None };
    let run = |plant: CellPlant| {
        let mut c = cells_of(&w, root, plant);
        assert_eq!(c.load_more_at(&w, &[blk, blk2], &place).map_err(|e| format!("{e}")), Ok(2));
        let expand = |call: ItemKey, _: crate::Def| {
            w.expanded.borrow_mut().insert(call);
        };
        c.run(&w, &expand, &[]).unwrap_or_else(|e| panic!("the core runs: {e}"));
        c
    };
    let ok = run(CellPlant::None);
    for (call, def, what) in [(c_far, far, "far"), (c_before, before, "before"), (c_inner, inner, "inner"), (c_far2, far, "far, from the expansion's block")] {
        assert!(matches!(ok.call_outcome(call), Some(Outcome::Called(d)) if d.key == def), "`{what}!`: {:?}", ok.call_outcome(call));
    }
    assert!(matches!(ok.call_outcome(c_after), Some(Outcome::Failed(Unresolved::NotFound))), "`after!`, defined after the body: {:?}", ok.call_outcome(c_after));
    let bad = run(CellPlant::BlockHasNoPlace);
    for (call, what) in [(c_far, "far"), (c_before, "before"), (c_inner, "inner"), (c_far2, "far, from the expansion's block")] {
        assert!(matches!(bad.call_outcome(call), Some(Outcome::Failed(Unresolved::NotFound))), "the control, `{what}!`: {:?}", bad.call_outcome(call));
    }
}

/// alloy_primitives on tempo's graph (c10-r13d: "lost call Plain ["paste", "paste"] ->
/// Some(Failed(NotFound))", src/map/fixed.rs:30: `mod map { pub use fixed::*; mod fixed { use
/// super::*; paste::paste! { .. } } }`, the eight `pub type ..Map` aliases it writes absent).
/// The call reads `paste` in the type namespace of `fixed`; `fixed`'s glob reads `map`, whose
/// glob reads `fixed`, whose own cell waits on its growth: that very call. The Reference,
/// `names.resolution.expansion.unresolved-invocations`: "Every macro invocation resolves to a
/// valid definition that exists in the final AST or in an external crate.", and
/// `names.resolution.expansion.speculation`: "All name resolution candidates selected during
/// macro expansion are considered speculative. Once the crate has been fully expanded, all
/// speculative import resolutions are validated to ensure that macro expansion did not introduce
/// any new ambiguities." So in the stalled group the module's own view of `paste` is absent,
/// the first segment goes outward to the extern prelude's crate (`names.preludes.extern`), the
/// rest of the path is read from the dependency's decided names, and the call resolves; what it
/// writes is then read by the glob (`T` through `map`). Control `StallFailsOuterPrefixCall`:
/// the call fails as not found and `T` is absent.
/// Prints: "test cells::tests::a_stalled_call_whose_first_segment_is_an_outer_crate_resolves ... ok".
#[test]
fn a_stalled_call_whose_first_segment_is_an_outer_crate_resolves() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let dep = w.root(2, Edition::E2021);
    let paste = w.item(dep, "paste", Namespace::Macros, crate::DefKind::Macro(crate::MacroShapes::FN_LIKE), V::Pub);
    w.dep(root, "paste", dep);
    let map = w.module(root, "map", V::Pub);
    let fixed = w.module(map, "fixed", V::Priv);
    w.glob(map, "self::fixed", V::Pub);
    w.glob(fixed, "super", V::Priv);
    let call = w.call(fixed, "paste::paste", crate::CallShape::FnLike);
    let t = w.item(call, "T", Namespace::Types, crate::DefKind::Other, V::Pub);
    let mut ok = with_dep(&w, root, dep, CellPlant::None);
    assert!(matches!(ok.call_outcome(call), Some(Outcome::Called(d)) if d.key == paste), "{:?}", ok.call_outcome(call));
    assert_eq!(found(ok.ask(map, Namespace::Types, "T").expect("asks")), Some(t), "what the call writes, through `map`'s glob");
    assert_eq!(ok.stats.r6_errors, 0);
    let mut bad = with_dep(&w, root, dep, CellPlant::StallFailsOuterPrefixCall);
    assert!(matches!(bad.call_outcome(call), Some(Outcome::Failed(Unresolved::NotFound))), "the control: {:?}", bad.call_outcome(call));
    assert_eq!(found(bad.ask(map, Namespace::Types, "T").expect("asks")), None, "the control: nothing was written");
}

/// objc2_core_foundation and objc2_open_directory on tempo's graph (c10-r14b: "lost call Plain
/// ["cf_objc2_type"] -> Some(Failed(NotFound))", 4 in `generated` and 5 with 5 `cf_type` in
/// open_directory's `CFOpenDirectory`: `use objc2::__framework_prelude::*; use crate::*;` in a
/// module that also globs its children, while the root globs it: `cf_type!(..);
/// cf_objc2_type!(..);` one after the other). Each call's macro is a glob candidate from the
/// dependency; the module's `Globs` cell waits on the root's, which waits on the module's own
/// growth: its calls; each later call's textual read waits on the call before it, and the
/// stall failed that earlier call first. The Reference, `names.resolution.expansion.speculation`:
/// "All name resolution candidates selected during macro expansion are considered speculative.
/// Once the crate has been fully expanded, all speculative import resolutions are validated to
/// ensure that macro expansion did not introduce any new ambiguities.", and
/// `...unresolved-invocations`: "Every macro invocation resolves to a valid definition that
/// exists in the final AST or in an external crate." So the candidate a glob source holds
/// decided is selected at the stall, and validated when the crate is expanded. The crate:
/// dependency `e` with `pub macro m1; pub macro m2;`, `pub use self::g::*;` at the root, `mod g
/// { use e::*; use crate::*; m1!(); m2!(); }`, `m1!()` writing `struct T`. Both calls resolve
/// and `T` reaches the root. Control `StallFailsGlobCall`: `m1!` fails as not found.
/// Prints: "test cells::tests::a_stalled_call_a_decided_glob_source_names_resolves ... ok".
#[test]
fn a_stalled_call_a_decided_glob_source_names_resolves() {
    let fn_like = crate::DefKind::Macro(crate::MacroShapes::FN_LIKE);
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let dep = w.root(2, Edition::E2021);
    let m1 = w.item(dep, "m1", Namespace::Macros, fn_like, V::Pub);
    let m2 = w.item(dep, "m2", Namespace::Macros, fn_like, V::Pub);
    w.dep(root, "e", dep);
    let g = w.module(root, "g", V::Pub);
    w.glob(root, "self::g", V::Pub);
    w.glob(g, "e", V::Priv);
    w.glob(g, "crate", V::Priv);
    let c1 = w.call(g, "m1", crate::CallShape::FnLike);
    let c2 = w.call(g, "m2", crate::CallShape::FnLike);
    let t = w.item(c1, "T", Namespace::Types, crate::DefKind::Other, V::Pub);
    let mut ok = with_dep(&w, root, dep, CellPlant::None);
    assert!(matches!(ok.call_outcome(c1), Some(Outcome::Called(d)) if d.key == m1), "{:?}", ok.call_outcome(c1));
    assert!(matches!(ok.call_outcome(c2), Some(Outcome::Called(d)) if d.key == m2), "{:?}", ok.call_outcome(c2));
    assert_eq!(found(ok.ask(root, Namespace::Types, "T").expect("asks")), Some(t), "what `m1!` writes, through the root's glob");
    assert_eq!(ok.stats.r6_errors, 0);
    let bad = with_dep(&w, root, dep, CellPlant::StallFailsGlobCall);
    assert!(matches!(bad.call_outcome(c1), Some(Outcome::Failed(Unresolved::NotFound))), "the control: {:?}", bad.call_outcome(c1));
}

/// Seven of tempo's crates (c10-r14c: "'only_old': 0, 'only_cells': 21": serde_core's
/// `__private_serialize` in `private::doc`, whose `mod private;` is written by `crate_root!()`;
/// nix's `sockopt_impl` under `feature! { pub mod socket; }`; tokio's `count!` under
/// `cfg_macros! { mod select; }`; four commonware crates): the root's written scope holds the
/// macro here and lacks it in the old core. The Reference, `macro.decl.scope.macro_export.intro`:
/// "The `macro_export` attribute exports the macro from the crate and makes it available in the
/// root of the crate for path-based resolution.", and `macro.decl.scope.path-based.visibility`:
/// "`#[macro_export]` changes the implicit visibility to `pub`." So this core is right, and the
/// old one is the fault (tests/name_rules.rs `x2`). The crate: `macro_rules! mk; mk!{ mod p {
/// pub mod d { #[macro_export] macro_rules! exp; } } }`: `exp` is a public macro of the root,
/// written with the origin `MacroExport`.
/// Prints: "test cells::tests::an_export_below_a_module_an_expansion_declared_is_in_the_root ... ok".
#[test]
fn an_export_below_a_module_an_expansion_declared_is_in_the_root() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    w.rules(root, "mk");
    let call = w.call(root, "mk", crate::CallShape::FnLike);
    let p = w.module(call, "p", V::Priv);
    let d = w.module(p, "d", V::Pub);
    let exp = w.rules_with(d, "exp", true);
    let mut c = cells_of(&w, root, CellPlant::None);
    let got = c.ask(root, Namespace::Macros, "exp").expect("asks");
    assert!(matches!(got, Some(Value::Found(def, crate::Visibility::Public)) if def.key == exp), "{got:?}");
}

/// The validation of what a stalled call took (runner 15: the stall asks `first_segment` with a
/// weakly undecided scope not holding the lookup, so an outer candidate is taken while the
/// module's globs may still bring the name). The Reference,
/// `names.resolution.expansion.speculation`: "All name resolution candidates selected during
/// macro expansion are considered speculative. Once the crate has been fully expanded, all
/// speculative import resolutions are validated to ensure that macro expansion did not introduce
/// any new ambiguities.", and `names.resolution.expansion.imports.ambiguity.glob-vs-outer`:
/// "Names in imports and macro invocations may not be resolved through glob imports when there
/// is another candidate available in an outer scope." The crate: the standard prelude holds a
/// macro `Cl`; `mod m { use self::k::*; Cl!(); }`, and `Cl!()` writes `pub mod k { pub macro Cl; }`
/// into `m`. The glob cannot resolve before the call expands (the sink of
/// `a_stalled_call_an_outer_scope_provides_resolves`), the call takes the prelude's `Cl`, and
/// its own expansion then brings another `Cl` through the glob: the error at the call, whose
/// answer stays the prelude's. Beside it `mod n { use self::k::*; Cl!(); }` whose `Cl!()` writes
/// `pub mod k { pub macro other; }`: no error. Control `SpeculationUnchecked`: no error at all.
/// Prints: "test cells::tests::what_a_stalled_call_took_is_validated_when_the_run_ends ... ok".
#[test]
fn what_a_stalled_call_took_is_validated_when_the_run_ends() {
    let fn_like = crate::DefKind::Macro(crate::MacroShapes::FN_LIKE);
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let prelude = w.module(root, "prelude", V::Pub);
    let cl = w.item(prelude, "Cl", Namespace::Macros, fn_like, V::Pub);
    w.cr(root).head.prelude = Some(prelude);
    let m = w.module(root, "m", V::Pub);
    w.glob(m, "self::k", V::Priv);
    let call = w.call(m, "Cl", crate::CallShape::FnLike);
    let k = w.module(call, "k", V::Pub);
    let inner = w.item(k, "Cl", Namespace::Macros, fn_like, V::Pub);
    let n = w.module(root, "n", V::Pub);
    w.glob(n, "self::k", V::Priv);
    let quiet = w.call(n, "Cl", crate::CallShape::FnLike);
    let k2 = w.module(quiet, "k", V::Pub);
    w.item(k2, "other", Namespace::Macros, fn_like, V::Pub);
    let mut ok = cells_of(&w, root, CellPlant::None);
    assert!(matches!(ok.call_outcome(call), Some(Outcome::Called(d)) if d.key == cl), "{:?}", ok.call_outcome(call));
    assert!(matches!(ok.call_outcome(quiet), Some(Outcome::Called(d)) if d.key == cl), "{:?}", ok.call_outcome(quiet));
    assert_eq!(found(ok.ask(m, Namespace::Macros, "Cl").expect("asks")), Some(inner), "the glob brings the written `Cl` in the end");
    assert_eq!(ok.stats.r6_errors, 1, "the error at `m`'s call alone");
    let bad = cells_of(&w, root, CellPlant::SpeculationUnchecked);
    assert!(matches!(bad.call_outcome(call), Some(Outcome::Called(d)) if d.key == cl), "{:?}", bad.call_outcome(call));
    assert_eq!(bad.stats.r6_errors, 0, "the control SpeculationUnchecked");
}

/// Calls by kind (runner 16). The Reference, `names.resolution.expansion.macros.intro`: "Macros
/// are split into two sub-namespaces, one for function-like macros, and the other for
/// attributes and derives. Resolution candidates from the incorrect sub-namespace are
/// ignored." The crate: a standard prelude holding the derive `Cl`; `mod m { macro_rules! Cl;
/// pub macro at /* an attribute macro */; Cl!(); #[derive(Cl)] struct S; #[derive(at)] struct
/// T; }`, and in `mod p { pub macro Cl /* an attribute macro */; #[derive(Cl)] struct U; }`.
/// `Cl!()` names the `macro_rules!` (textual); `#[derive(Cl)]` in `m` passes over it (wrong
/// sub-namespace) to the prelude's derive; `#[derive(at)]` selects the attribute macro of its
/// own sub-namespace and is refused as the wrong kind; in `p` the module's attribute `Cl` is of
/// the derive's sub-namespace, so it is selected (not passed over to the prelude) and refused.
/// Control `SubNamespaceIgnored`: `#[derive(Cl)]` in `m` calls the `macro_rules!`, and
/// `#[derive(at)]` calls the attribute macro.
/// Prints: "test cells::tests::a_call_reads_only_its_own_sub_namespace ... ok".
#[test]
fn a_call_reads_only_its_own_sub_namespace() {
    use crate::{CallShape, DefKind, MacroShapes};
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let prelude = w.module(root, "prelude", V::Pub);
    let cl = w.item(prelude, "Cl", Namespace::Macros, DefKind::Macro(MacroShapes::DERIVE), V::Pub);
    w.cr(root).head.prelude = Some(prelude);
    let m = w.module(root, "m", V::Pub);
    let rules = w.rules(m, "Cl");
    let at = w.item(m, "at", Namespace::Macros, DefKind::Macro(MacroShapes::ATTR), V::Pub);
    let bang = w.call(m, "Cl", CallShape::FnLike);
    let derive = w.call(m, "Cl", CallShape::Derive);
    let wrong = w.call(m, "at", CallShape::Derive);
    let p = w.module(root, "p", V::Pub);
    w.item(p, "Cl", Namespace::Macros, DefKind::Macro(MacroShapes::ATTR), V::Pub);
    let near = w.call(p, "Cl", CallShape::Derive);
    let ok = cells_of(&w, root, CellPlant::None);
    assert!(matches!(ok.call_outcome(bang), Some(Outcome::Called(d)) if d.key == rules), "`Cl!()`: {:?}", ok.call_outcome(bang));
    assert!(matches!(ok.call_outcome(derive), Some(Outcome::Called(d)) if d.key == cl), "`#[derive(Cl)]`: {:?}", ok.call_outcome(derive));
    assert_eq!(ok.call_outcome(wrong), Some(Outcome::Failed(Unresolved::WrongMacroKind)), "`#[derive(at)]`");
    assert_eq!(ok.call_outcome(near), Some(Outcome::Failed(Unresolved::WrongMacroKind)), "`#[derive(Cl)]` in `p`");
    let bad = cells_of(&w, root, CellPlant::SubNamespaceIgnored);
    assert!(matches!(bad.call_outcome(derive), Some(Outcome::Called(d)) if d.key == rules), "the control: {:?}", bad.call_outcome(derive));
    assert!(matches!(bad.call_outcome(wrong), Some(Outcome::Called(d)) if d.key == at), "the control: {:?}", bad.call_outcome(wrong));
}

/// The written scope of module `m` of the crate `root` is the root of, the core planted with
/// `plant` and run forward or `backward` (items evaluated, and so calls expanded, from the last
/// id first).
fn written_scope(w: &World, root: ItemKey, m: ItemKey, plant: CellPlant, backward: bool) -> blaze_surface::ScopeBuilder<'static, N> {
    w.expanded.borrow_mut().clear();
    let mut c = Cells::load_planted(w, root, None, plant, Alloc::global()).expect("the load");
    c.backward = backward;
    let expand = |call: ItemKey, _: crate::Def| {
        w.expanded.borrow_mut().insert(call);
    };
    c.run(w, &expand, &[]).expect("the core runs");
    let ix = c.write_index(Alloc::global());
    let (scope, unknown) = c.scope(&ix, m, Alloc::global()).expect("the module is written");
    assert_eq!(unknown, 0, "{:?}", scope.errors);
    scope
}

/// The names of the `WiderReexport` errors among `errors`, in their order.
fn wider_names(errors: &[blaze_surface::ScopeError<N, blaze_surface::Path<'static, N>>]) -> Vec<N> {
    errors
        .iter()
        .filter_map(|e| match e {
            blaze_surface::ScopeError::WiderReexport { name, .. } => Some(*name),
            _ => None,
        })
        .collect()
}

/// lz4_flex (c10-r17c: two `WiderReexport` the old core wrote and the cells did not). The
/// Reference, `items.use.visibility.intro`: "A public use declaration can therefore redirect
/// some public name to a different target definition"; the error index, E0364: "Private items
/// cannot be publicly re-exported", E0365: "Private modules cannot be publicly re-exported"
/// (its example: `mod foo { .. } pub use foo as foo2;`). The crate: `mod foo {}`, `pub use foo
/// as foo2;`, `pub mod a { pub(crate) struct s; pub struct p; pub use self::s as t;
/// pub(crate) use self::s as u; pub use self::p as q; }`. `foo2` (root) and `t` (`a`) are the
/// errors, in the types namespace, and each name is still bound; `u` and `q` are not errors.
/// The control `NoWiderReexport` writes none.
/// Prints: "test cells::tests::a_use_wider_than_its_target_is_an_error ... ok".
#[test]
fn a_use_wider_than_its_target_is_an_error() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let foo = w.module(root, "foo", V::Priv);
    w.use_as(root, "foo", "foo2", V::Pub);
    let a = w.module(root, "a", V::Pub);
    let s = w.item(a, "s", Namespace::Types, crate::DefKind::Other, V::Crate);
    let p = w.item(a, "p", Namespace::Types, crate::DefKind::Other, V::Pub);
    w.use_as(a, "self::s", "t", V::Pub);
    w.use_as(a, "self::s", "u", V::Crate);
    w.use_as(a, "self::p", "q", V::Pub);
    let r = written_scope(&w, root, root, CellPlant::None, false);
    assert_eq!(wider_names(&r.errors), ["foo"], "the root: {:?}", r.errors);
    assert!(r.errors.iter().all(|e| matches!(e, blaze_surface::ScopeError::WiderReexport { ns: Namespace::Types, def, .. } if def.key == foo)), "{:?}", r.errors);
    assert!(r.bindings.iter().any(|e| e.name == "foo2" && e.binding.def.key == foo), "`foo2` is bound: {:?}", r.bindings);
    let sa = written_scope(&w, root, a, CellPlant::None, false);
    assert_eq!(wider_names(&sa.errors), ["s"], "`a`: {:?}", sa.errors);
    assert_eq!(sa.errors.len(), 1, "`a`: {:?}", sa.errors);
    for (name, def) in [("t", s), ("u", s), ("q", p)] {
        assert!(sa.bindings.iter().any(|e| e.name == name && e.binding.def.key == def), "`{name}` is bound: {:?}", sa.bindings);
    }
    assert!(written_scope(&w, root, root, CellPlant::NoWiderReexport, false).errors.is_empty(), "the control, the root");
    assert!(written_scope(&w, root, a, CellPlant::NoWiderReexport, false).errors.is_empty(), "the control, `a`");
}

/// futures_util (c10-r17c: the root's `macro_exports` in another order than the old core's).
/// Its `select_mod.rs`: `document_select_macro! { #[macro_export] macro_rules! select { .. }
/// #[macro_export] macro_rules! select_biased { .. } }`, whose second arm expands to
/// `document_select_macro!($select); .. $select_biased`: `select` is written by the nested call,
/// one expansion later than `select_biased`, but before it in the text. Rule 25: a record's
/// bytes do not depend on the order work happened to finish in; the list is the walk's tree
/// order (the module's text, every call's expansion at the call), a function of the text alone.
/// The crate: `pub mod x { pub macro m }` (named by path, so no call waits on an earlier one as a
/// textual name would), `x::m!()` (A, expanding to `x::m!()` (B, expanding to `#[macro_export]
/// macro_rules! s`) then `#[macro_export] macro_rules! sb`), `x::m!()` (C1: `.. p`), `x::m!()`
/// (C2: `.. q`). Run forward (A, C1, C2 expand, then B) and backward (C2, C1, A, then B), the root's
/// whole written scope is equal, its `macro_exports` [s, sb, p, q]. The control
/// `ExportsByExpansionTime` lists them by expansion: forward [sb, p, q, s], backward [q, p, sb,
/// s], the two schedules differ.
/// Prints: "test cells::tests::macro_exports_are_in_text_order_whatever_the_expansion_order ... ok".
#[test]
fn macro_exports_are_in_text_order_whatever_the_expansion_order() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let x = w.module(root, "x", V::Pub);
    w.item(x, "m", Namespace::Macros, crate::DefKind::Macro(crate::MacroShapes::FN_LIKE), V::Pub);
    let a = w.call(root, "x::m", crate::CallShape::FnLike);
    let b = w.call(a, "x::m", crate::CallShape::FnLike);
    w.rules_with(b, "s", true);
    w.rules_with(a, "sb", true);
    let c1 = w.call(root, "x::m", crate::CallShape::FnLike);
    w.rules_with(c1, "p", true);
    let c2 = w.call(root, "x::m", crate::CallShape::FnLike);
    w.rules_with(c2, "q", true);
    let names = |s: &blaze_surface::ScopeBuilder<'static, N>| s.macro_exports.iter().map(|d| d.name).collect::<Vec<N>>();
    let fwd = written_scope(&w, root, root, CellPlant::None, false);
    let bwd = written_scope(&w, root, root, CellPlant::None, true);
    assert_eq!(names(&fwd), ["s", "sb", "p", "q"], "forward");
    assert_eq!(format!("{fwd:?}"), format!("{bwd:?}"), "the whole scope, forward and backward");
    let pf = written_scope(&w, root, root, CellPlant::ExportsByExpansionTime, false);
    let pb = written_scope(&w, root, root, CellPlant::ExportsByExpansionTime, true);
    assert_ne!(names(&pf), names(&pb), "the control: the schedules differ: {:?} {:?}", names(&pf), names(&pb));
}

/// A call in a block reads the scope around the block, not a glob. The Reference, "Macros By
/// Example", `macro.decl.scope` (a one-segment call: the textual scope first, then the
/// path-based scope) and `names.preludes`: "Prelude names are not part of any module or block
/// ... Names in the preludes can be shadowed by declarations in a module": `use e::other as
/// format;` in the module shadows the `#[macro_use]` prelude's `format` for every block of the
/// module's bodies; a block reaches its module lexically, through no glob import, so the
/// glob-vs-outer ambiguity does not arise. The world (the D189 surface test's, reduced:
/// `use dep::emits_impl as format; fn g() { format!(); }`): dependency `e` exports `format`
/// and `other`; the crate's root has `use e::other as format;` and the block's module
/// `__blaze_block_0` (its lexical `use super::*;` first, as expanded_blocks.rs renders it)
/// calls `format!`. The call is `other`. Twin: without the import the same call is the
/// prelude's `format`. Control (rule 23): `LexicalGlobIsAGlob` reads the lexical glob as a glob
/// and fails the call as ambiguous (the cause of the missing impl).
/// Prints: "test cells::tests::a_blocks_call_reads_its_modules_named_import_before_the_prelude ... ok".
#[test]
fn a_blocks_call_reads_its_modules_named_import_before_the_prelude() {
    let world = |renamed: bool| {
        let mut w = World::new();
        let root = w.root(1, Edition::E2021);
        let dep = w.root(2, Edition::E2021);
        let format = w.rules_with(dep, "format", true);
        let other = w.rules_with(dep, "other", true);
        w.cr(dep).macro_exports.push(("format", format, crate::MacroShapes::FN_LIKE));
        w.cr(dep).macro_exports.push(("other", other, crate::MacroShapes::FN_LIKE));
        w.dep(root, "e", dep);
        w.cr(root).macro_use.push(dep);
        if renamed {
            w.use_as(root, "e::other", "format", V::Priv);
        }
        let block = w.module(root, "__blaze_block_0", V::Priv);
        w.glob(block, "super", V::Priv);
        let call = w.call(block, "format", crate::CallShape::FnLike);
        (w, root, dep, call, format, other)
    };
    let called = |c: &Cells<'static, N>, call: ItemKey| match c.call_outcome(call) {
        Some(Outcome::Called(d)) => Some(d.key),
        _ => None,
    };
    let (w, root, dep, call, _, other) = world(true);
    let ok = with_dep(&w, root, dep, CellPlant::None);
    assert_eq!(called(&ok, call), Some(other), "the module's `use e::other as format;`: {:?}", ok.call_outcome(call));
    let bad = with_dep(&w, root, dep, CellPlant::LexicalGlobIsAGlob);
    assert!(matches!(bad.call_outcome(call), Some(Outcome::Failed(Unresolved::Ambiguous(_)))), "the control: {:?}", bad.call_outcome(call));
    let (w, root, dep, call, format, _) = world(false);
    let twin = with_dep(&w, root, dep, CellPlant::None);
    assert_eq!(called(&twin, call), Some(format), "the twin: the prelude's `format`: {:?}", twin.call_outcome(call));
}
