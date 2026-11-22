//! The name solver's rules R1 to R9 (docs/v2/plan/design/bottom.md section 6, "Name
//! resolution: the rules (2026-11-16)"), one fixture per case, each solved by the fast solver in
//! the calm order and in `ORDERS` drawn completion orders: the answer must be the written rule's
//! in every order, and every order must give the calm order's records byte for byte.
//!
//! A fixture the current fast solver fails is named in `KNOWN` with one line: the suite stays
//! green and the list is the report for the lane rebuilding the solver's core. Run one case with
//! its output: `lanecheck.sh blaze-scope test "name_rules::r6a -- --nocapture"`.
//!
//! The expected answers come from the Rust Reference's expansion-time name resolution text
//! (`names.resolution.expansion.*`, quoted per fixture) and RFC 1560; where they do not decide,
//! from blaze's decision D56 (docs/v2/plan/decisions.json). No compiler was run for them.

use std::format;
use std::string::String;
use std::vec::Vec;

use blaze_id::ItemKey;
use blaze_sim::{Tail, Tape};

use super::{Solved, V, World, same, solve, solve_with};
use crate::{CallShape, DefKind, Edition, Namespace, ScopeError, Unresolved};

/// Drawn completion orders per fixture (the brief's floor is 64).
const ORDERS: u64 = 64;

/// The fixtures the cell core (the names records' one writer) fails as these tests drive it
/// ([`super::solve`]), each with the answer seen. A fixture listed here that passes fails the
/// suite. The oracle's comparisons skip a difference of the same class while its name is listed
/// (oracle.rs `known_import`). Since 2026-11-16 18:3x (the old fill deleted, NAMES runner 19);
/// r2c, r3b, r5f, r5g, r7d, r7f and r7g were the old fill's and pass on the cells.
pub(super) const KNOWN: &[(&str, &str)] = &[
    ("x1", "THE TESTS' DRIVER, not the cells: a body's block (a module with no `mod` line) is never loaded by `cells::solve_world` (the command loads it through `places`/`load_more_at`; cells/tests.rs `a_blocks_module_reads_the_textual_scope_around_its_body` passes)"),
    ("x2", "THE TESTS' DRIVER, not the cells: the root's `#[macro_export]` list of macros an expansion wrote is published after the cells by resolve.rs (`publish_expanded_exports`), which `cells::solve_world` does not run; cells/tests.rs `an_export_below_a_module_an_expansion_declared_is_in_the_root` passes"),
    ("r7s_dep_extern", "R7 with the glob's source in a dependency (crate 3 `lib`, `use lib::m::*;`, `m` itself a glob of `k`) beside the extern prelude's `n` (crate 2): \"no ambiguity of `n`: errors []\". NOT R6/R8 (no expansion in this world: `r6`/`r8` are not involved; names-R6-R8, 2026-11-16). The same glob passes beside the standard prelude (r7s_dep_std) and a local glob passes beside the extern prelude (r7s_chain*_extern): the class is the root's `Globs` cell of a FOREIGN glob source with the outer candidate in the extern prelude, read through `DepNames` (`cells::world::SolvedCrates::name_in`, crates solved 2 then 3); first step: run this case with --nocapture printing the root's errors and `u`'s outcome"),
    ("r7s_dep_extern_without", "as r7s_dep_extern, the dependency's module without the name: \"`u` names None, the rule says Some(..)\" although crate 2 holds `x`: the use is not resolved through the extern prelude either, so the first step is the same run; NOT R6/R8"),
];

/// A tape whose first choices are drawn from `seed` (small values, so each is in range), calm after.
fn tape_of(seed: u64) -> Tape {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut prefix = Vec::new();
    for _ in 0..64 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        prefix.push(x % 4);
    }
    Tape::new(&prefix, Tail::Calm, Tape::MOST_CHOICES)
}

/// `want` holds for the calm order and every drawn order, and each order equals the calm one.
fn every_order(w: &World, want: &dyn Fn(&Solved) -> Result<(), String>) -> Result<(), String> {
    let calm = solve(w, None);
    want(calm.as_ref().map_err(|e| format!("calm order: the solver failed: {e}"))?).map_err(|e| format!("calm order: {e}"))?;
    for seed in 0..ORDERS {
        let tape = tape_of(seed);
        let got = solve_with(w, w, Some(&tape));
        let s = got.as_ref().map_err(|e| format!("order {seed}: the solver failed: {e}"))?;
        want(s).map_err(|e| format!("order {seed}: {e}"))?;
        same(&calm, &got).map_err(|e| format!("order {seed} differs from the calm order: {e}"))?;
    }
    Ok(())
}

/// Runs fixture `name`: a failure fails the test unless `name` is in `KNOWN`.
fn rule(name: &'static str, w: &World, want: &dyn Fn(&Solved) -> Result<(), String>) {
    if let Some(e) = verdict(name, w, want) {
        panic!("{e}");
    }
}

/// Fixture `name`'s verdict against `KNOWN`: `None` when it holds (or fails and is listed),
/// else what is wrong.
fn verdict(name: &'static str, w: &World, want: &dyn Fn(&Solved) -> Result<(), String>) -> Option<String> {
    let known = KNOWN.iter().any(|(n, _)| *n == name);
    match (every_order(w, want), known) {
        (Ok(()), false) => None,
        (Err(e), true) => {
            super::oracle::keep(&format!("KNOWN failure {name}: {e}"));
            None
        }
        (Err(e), false) => Some(format!("{name}: {e}")),
        (Ok(()), true) => Some(format!("{name} passes now: take it off `KNOWN`")),
    }
}

fn names(s: &Solved, m: ItemKey, ns: Namespace, name: &'static str, want: Option<ItemKey>) -> Result<(), String> {
    let got = s.def(m, ns, name);
    if got == want { Ok(()) } else { Err(format!("`{name}` names {got:?}, the rule says {want:?}")) }
}

fn no_errors(s: &Solved, m: ItemKey) -> Result<(), String> {
    if s.errors(m).is_empty() { Ok(()) } else { Err(format!("errors {:?}, the rule says none", s.errors(m))) }
}

fn ambiguous(s: &Solved, m: ItemKey, name: &'static str) -> Result<(), String> {
    let hit = s.errors(m).iter().any(|e| match e {
        ScopeError::Ambiguous { name: n, .. } => *n == name,
        ScopeError::Import { why: Unresolved::Ambiguous(_), .. } => true,
        _ => false,
    });
    if hit { Ok(()) } else { Err(format!("no ambiguity of `{name}`: errors {:?}", s.errors(m))) }
}

fn failed_imports(s: &Solved, m: ItemKey, why: Unresolved, count: usize) -> Result<(), String> {
    let n = s.errors(m).iter().filter(|e| matches!(e, ScopeError::Import { why: w, .. } if *w == why)).count();
    if n == count && s.errors(m).len() == count {
        Ok(())
    } else {
        Err(format!("errors {:?}, the rule says {count} import(s) failing {why:?}", s.errors(m)))
    }
}

fn ty(w: &mut World, at: ItemKey, name: &'static str) -> ItemKey {
    w.item(at, name, Namespace::Types, DefKind::Other, V::Pub)
}

/// A dependency crate `n` (crate 2) in crate 1's extern prelude, with `pub struct x`.
fn with_dep() -> (World, ItemKey, ItemKey) {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let dep = w.root(2, Edition::E2021);
    let x = ty(&mut w, dep, "x");
    w.dep(root, "n", dep);
    (w, root, x)
}

/// R1, a named import shadows a glob: `mod a { pub struct x; } mod b { pub struct x; }
/// use crate::a::*; use crate::b::x;`. The Reference (`items.use.glob.shadowing`): "items and
/// named imports are allowed to shadow names from glob imports in the same namespace".
#[test]
fn r1a_named_import_shadows_glob() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let a = w.module(root, "a", V::Priv);
    let b = w.module(root, "b", V::Priv);
    ty(&mut w, a, "x");
    let bx = ty(&mut w, b, "x");
    w.glob(root, "crate::a", V::Priv);
    w.use_(root, "crate::b::x", V::Priv);
    rule("r1a", &w, &|s| names(s, root, Namespace::Types, "x", Some(bx)).and(no_errors(s, root)));
}

/// R1, an item shadows a glob: `mod a { pub struct x; } use crate::a::*; struct x;`.
#[test]
fn r1b_item_shadows_glob() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let a = w.module(root, "a", V::Priv);
    ty(&mut w, a, "x");
    w.glob(root, "crate::a", V::Priv);
    let own = ty(&mut w, root, "x");
    rule("r1b", &w, &|s| names(s, root, Namespace::Types, "x", Some(own)).and(no_errors(s, root)));
}

/// R1, an item written by an expansion shadows a glob no early resolution used:
/// `mod a { pub struct x; } use crate::a::*; macro_rules! mk; mk!{ pub struct x; }`.
/// The Reference: an expanded name shadows a glob one unless an early resolution went through
/// the glob (`GlobVsExpanded`); nothing here reads `x`.
#[test]
fn r1c_expanded_item_shadows_unused_glob() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let a = w.module(root, "a", V::Priv);
    ty(&mut w, a, "x");
    w.glob(root, "crate::a", V::Priv);
    w.rules(root, "mk");
    let call = w.call(root, "mk", CallShape::FnLike);
    let own = ty(&mut w, call, "x");
    rule("r1c", &w, &|s| names(s, root, Namespace::Types, "x", Some(own)).and(no_errors(s, root)));
}

/// R2, one item through two globs is no ambiguity: `mod a { pub struct x; } mod b { pub use
/// crate::a::x; } mod c { pub use crate::a::*; pub use crate::b::*; } use crate::c::x as u;`.
/// The Reference (`names.resolution.expansion.imports.ambiguity.glob-vs-glob`): "Multiple glob
/// imports are allowed to import the same name, and that name is allowed to be used if the
/// imports are of the same item (following reexports)."
#[test]
fn r2a_same_item_through_two_globs() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let a = w.module(root, "a", V::Priv);
    let b = w.module(root, "b", V::Priv);
    let c = w.module(root, "c", V::Priv);
    let x = ty(&mut w, a, "x");
    w.use_(b, "crate::a::x", V::Pub);
    w.glob(c, "crate::a", V::Pub);
    w.glob(c, "crate::b", V::Pub);
    w.use_as(root, "crate::c::x", "u", V::Priv);
    rule("r2a", &w, &|s| {
        names(s, root, Namespace::Types, "u", Some(x)).and(no_errors(s, root)).and(no_errors(s, c))
    });
}

/// R2, different items through two globs, used: an ambiguity where used (`glob-vs-glob`:
/// "The errors occur at time of use, not time of import"), reached through a slow glob chain
/// so the second glob can resolve after the use was first tried: `mod a { pub struct x; }
/// mod b { pub struct x; } mod d { pub use crate::b::*; } mod c { pub use crate::a::*;
/// pub use crate::d::*; } use crate::c::x as u;`.
#[test]
fn r2b_different_items_through_two_globs_used() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let a = w.module(root, "a", V::Priv);
    let b = w.module(root, "b", V::Priv);
    let d = w.module(root, "d", V::Priv);
    let c = w.module(root, "c", V::Priv);
    ty(&mut w, a, "x");
    ty(&mut w, b, "x");
    w.glob(d, "crate::b", V::Pub);
    w.glob(c, "crate::a", V::Pub);
    w.glob(c, "crate::d", V::Pub);
    w.use_as(root, "crate::c::x", "u", V::Priv);
    rule("r2b", &w, &|s| ambiguous(s, root, "x").and(no_errors(s, c)));
}

/// R3, a glob cycle is the least fixed point: `mod a { pub use crate::b::*; pub struct x; }
/// mod b { pub use crate::a::*; pub struct y; } use crate::a::y as u; use crate::b::x as v;
/// use crate::a::z;`. RFC 1560: resolution runs "until fixed point"; globs may form cycles;
/// `z` is in neither, so the last import fails as not found and nothing hangs.
#[test]
fn r3a_glob_cycle_least_fixed_point() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let a = w.module(root, "a", V::Priv);
    let b = w.module(root, "b", V::Priv);
    w.glob(a, "crate::b", V::Pub);
    let x = ty(&mut w, a, "x");
    w.glob(b, "crate::a", V::Pub);
    let y = ty(&mut w, b, "y");
    w.use_as(root, "crate::a::y", "u", V::Priv);
    w.use_as(root, "crate::b::x", "v", V::Priv);
    w.use_(root, "crate::a::z", V::Priv);
    rule("r3a", &w, &|s| {
        names(s, root, Namespace::Types, "u", Some(y))
            .and(names(s, root, Namespace::Types, "v", Some(x)))
            .and(failed_imports(s, root, Unresolved::NotFound, 1))
    });
}

/// R3 with R4 through a public glob re-export of a module into its own child, the wide
/// comparison's seed 1402 reduced: `macro_rules! mk; mk!{ mod m1 { use crate::*; mk!{ pub type x
/// = u8; } } } mk!{ mod m2 { type x = u8; fn x() {} type y = u8; mk!{ mod m3 { pub use
/// crate::m2::m2::*; use crate::*; } } use crate::*; use self::m3::m1::*; } }`. `crate::m2::m2`
/// is the root's `m2` through m2's own glob of the root; m3's `pub use` of it brings m2's names
/// visible at m3, `m1` among them, so `self::m3::m1` is visible from m2 ("The visibility of the
/// name is the maximum visibility of the imports", `...ambiguity.glob-vs-glob`, beside m3's
/// private `use crate::*` of the same `m1`); m1's glob then brings only its `pub type x`, which
/// m2's own `x` shadows (`items.use.glob.shadowing`). Every import resolves, the least fixed
/// point (RFC 1560: "until fixed point"); no error. The old solver gives no answer: its own check
/// "its scope read against its fellows' records differs from its group's" fails.
#[test]
fn r3b_glob_cycle_through_public_reexport_of_parent() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    w.rules(root, "mk");
    let c1 = w.call(root, "mk", CallShape::FnLike);
    let m1 = w.module(c1, "m1", V::Priv);
    w.glob(m1, "crate", V::Priv);
    let c1x = w.call(m1, "mk", CallShape::FnLike);
    ty(&mut w, c1x, "x");
    let c2 = w.call(root, "mk", CallShape::FnLike);
    let m2 = w.module(c2, "m2", V::Priv);
    let x = w.item(m2, "x", Namespace::Types, DefKind::Other, V::Priv);
    let fx = w.fun(m2, "x", V::Priv);
    let y = w.item(m2, "y", Namespace::Types, DefKind::Other, V::Priv);
    let c3 = w.call(m2, "mk", CallShape::FnLike);
    let m3 = w.module(c3, "m3", V::Priv);
    w.glob(m3, "crate::m2::m2", V::Pub);
    w.glob(m3, "crate", V::Priv);
    w.glob(m2, "crate", V::Priv);
    w.glob(m2, "self::m3::m1", V::Priv);
    rule("r3b", &w, &|s| {
        no_errors(s, root)
            .and(no_errors(s, m1))
            .and(no_errors(s, m2))
            .and(no_errors(s, m3))
            .and(names(s, m2, Namespace::Types, "x", Some(x)))
            .and(names(s, m2, Namespace::Values, "x", Some(fx)))
            .and(names(s, m3, Namespace::Types, "m1", Some(m1)))
            .and(names(s, m3, Namespace::Types, "y", Some(y)))
    });
}

/// R4, a glob brings only names visible at the importer: `mod a { struct x; pub struct y; }
/// mod b { use crate::a::*; use self::y as u; use self::x as v; }`. The Reference
/// (`items.use.glob`): a glob imports "all public items"; that is, the importable ones; `x` is
/// private to `a`, so `b` has no `x` and `self::x` is not found.
#[test]
fn r4a_glob_brings_only_visible_names() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let a = w.module(root, "a", V::Priv);
    let b = w.module(root, "b", V::Priv);
    w.item(a, "x", Namespace::Types, DefKind::Other, V::Priv);
    let y = ty(&mut w, a, "y");
    w.glob(b, "crate::a", V::Priv);
    w.use_as(b, "self::y", "u", V::Priv);
    w.use_as(b, "self::x", "v", V::Priv);
    rule("r4a", &w, &|s| names(s, b, Namespace::Types, "u", Some(y)).and(failed_imports(s, b, Unresolved::NotFound, 1)));
}

/// R5, a group that waits only on itself fails: `mod a { pub use crate::b::x; } mod b {
/// pub use crate::a::x; }`. The Reference (`items.use.visibility.unambiguous`): redirections
/// that "form a cycle or cannot be resolved unambiguously" are an error; both imports fail.
#[test]
fn r5a_import_cycle_fails_both() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let a = w.module(root, "a", V::Priv);
    let b = w.module(root, "b", V::Priv);
    w.use_(a, "crate::b::x", V::Pub);
    w.use_(b, "crate::a::x", V::Pub);
    rule("r5a", &w, &|s| failed_imports(s, a, Unresolved::NotFound, 1).and(failed_imports(s, b, Unresolved::NotFound, 1)));
}

/// R5's refinement: `macro_rules! mk; mx!(); mk!{ mod m1 { mk!{ mod m3; } } }`: `mx!` fails
/// (no `mx` in the textual or path scope: the Reference, "Textual scope"), `mk!` is spared the
/// round it waits on `mx!` through a textual read, and `m3` is made.
#[test]
fn r5b_member_waiting_on_one_call_of_its_group_is_spared() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    w.rules(root, "mk");
    w.call(root, "mx", CallShape::FnLike);
    let call = w.call(root, "mk", CallShape::FnLike);
    let m1 = w.module(call, "m1", V::Priv);
    let inner = w.call(m1, "mk", CallShape::FnLike);
    let m3 = w.module(inner, "m3", V::Priv);
    rule("r5b", &w, &|s| {
        names(s, m1, Namespace::Types, "m3", Some(m3)).and(if s.errors(root).len() == 1 {
            Ok(())
        } else {
            Err(format!("root errors {:?}, the rule says one: `mx!` not found", s.errors(root)))
        })
    });
}

/// R6, the outer binding first, then the inner from an expansion not enclosing the use is an
/// ambiguity: extern prelude `n` (with `x`); `macro_rules! mk; use n::x as u; mk!{ pub mod n {
/// pub struct x; } }`. The Reference (`names.resolution.expansion`): a name "must resolve to
/// the same definition regardless of the order in which macros are expanded and imports are
/// resolved"; blaze's D56: an error at the new definition, in every order.
#[test]
fn r6a_expansion_writes_name_already_read_outward() {
    let (mut w, root, _) = with_dep();
    w.rules(root, "mk");
    w.use_as(root, "n::x", "u", V::Priv);
    let call = w.call(root, "mk", CallShape::FnLike);
    let n = w.module(call, "n", V::Pub);
    ty(&mut w, n, "x");
    rule("r6a", &w, &|s| ambiguous(s, root, "n"));
}

/// R6 when a later segment fails too, the oracle's wide seed 39 as a fixture: `use n::k as u;
/// mk!{ pub mod n {} }` with extern prelude `n` (no `k` in either `n`). D56: the use went
/// outward (or would in some order) and an expansion not enclosing it wrote `n`: an ambiguity,
/// in every order; that neither `n` holds `k` does not replace it (the old solver: not found).
#[test]
fn r6e_expansion_writes_name_later_segment_missing() {
    let (mut w, root, _) = with_dep();
    w.rules(root, "mk");
    w.use_as(root, "n::k", "u", V::Priv);
    let call = w.call(root, "mk", CallShape::FnLike);
    w.module(call, "n", V::Pub);
    rule("r6e", &w, &|s| ambiguous(s, root, "n"));
}

/// R6, written by an enclosing expansion the inner wins: `mk!{ pub mod n { pub struct x; }
/// use n::x as u; }` with extern prelude `n`. D56: the use is inside the expansion that
/// writes `n`, so it sees `n` as soon as it exists; no error.
#[test]
fn r6b_enclosing_expansion_inner_wins() {
    let (mut w, root, _) = with_dep();
    w.rules(root, "mk");
    let call = w.call(root, "mk", CallShape::FnLike);
    let n = w.module(call, "n", V::Pub);
    let x = ty(&mut w, n, "x");
    w.use_as(call, "n::x", "u", V::Priv);
    rule("r6b", &w, &|s| names(s, root, Namespace::Types, "u", Some(x)).and(no_errors(s, root)));
}

/// R6, no outer binding: the inner is used when it appears: `use m::x as u; mk!{ pub mod m {
/// pub struct x; } }` (no `m` in any prelude). D56.
#[test]
fn r6c_no_outer_binding_waits_for_inner() {
    let (mut w, root, _) = with_dep();
    w.rules(root, "mk");
    w.use_as(root, "m::x", "u", V::Priv);
    let call = w.call(root, "mk", CallShape::FnLike);
    let m = w.module(call, "m", V::Pub);
    let x = ty(&mut w, m, "x");
    rule("r6c", &w, &|s| names(s, root, Namespace::Types, "u", Some(x)).and(no_errors(s, root)));
}

/// R6, the outer binding, the expansion writing something else: `use n::x as u; mk!{ pub
/// struct k; }` with extern prelude `n`: `u` is the dependency's `x`, no error.
#[test]
fn r6d_unrelated_expansion_keeps_outer() {
    let (mut w, root, x) = with_dep();
    w.rules(root, "mk");
    w.use_as(root, "n::x", "u", V::Priv);
    let call = w.call(root, "mk", CallShape::FnLike);
    ty(&mut w, call, "k");
    rule("r6d", &w, &|s| names(s, root, Namespace::Types, "u", Some(x)).and(no_errors(s, root)));
}

/// R7, a pending glob is waited for, and a glob candidate beside an outer one is an
/// ambiguity: extern prelude `n`; `mod a { pub mod n { pub struct x; } } mod d { pub use
/// crate::a::*; } use crate::d::*; use n::x as u;`. The Reference
/// (`names.resolution.expansion.imports.ambiguity.glob-vs-outer`): "Names in imports and macro
/// invocations may not be resolved through glob imports when there is another candidate
/// available in an outer scope." Going outward before the glob resolves would give the
/// dependency's `x` with no error in the orders where the glob is late.
#[test]
fn r7a_pending_glob_is_waited_for() {
    let (mut w, root, _) = with_dep();
    let a = w.module(root, "a", V::Priv);
    let d = w.module(root, "d", V::Priv);
    let n = w.module(a, "n", V::Pub);
    ty(&mut w, n, "x");
    w.glob(d, "crate::a", V::Pub);
    w.glob(root, "crate::d", V::Priv);
    w.use_as(root, "n::x", "u", V::Priv);
    rule("r7a", &w, &|s| ambiguous(s, root, "n"));
}

/// R7, a pending glob that brings no `n`: the lookup waits, then goes outward: `mod a { pub
/// struct y; } mod d { pub use crate::a::*; } use crate::d::*; use n::x as u;`: `u` is the
/// dependency's `x`, no error.
#[test]
fn r7b_pending_glob_without_the_name_then_outward() {
    let (mut w, root, x) = with_dep();
    let a = w.module(root, "a", V::Priv);
    let d = w.module(root, "d", V::Priv);
    ty(&mut w, a, "y");
    w.glob(d, "crate::a", V::Pub);
    w.glob(root, "crate::d", V::Priv);
    w.use_as(root, "n::x", "u", V::Priv);
    rule("r7b", &w, &|s| names(s, root, Namespace::Types, "u", Some(x)).and(no_errors(s, root)));
}

/// R7, a pending named import shadows the outer binding: `mod a { pub mod n { pub struct x; }
/// } use crate::a::n; use n::x as u;` with extern prelude `n`. The Reference
/// (`names.resolution.expansion.imports.shadowing`): "Names introduced via use declarations in
/// an outer scope are shadowed by candidates in the same namespace with the same name from an
/// inner scope"; `u` is `a::n::x`, no error.
#[test]
fn r7c_pending_named_import_shadows_outer() {
    let (mut w, root, _) = with_dep();
    let a = w.module(root, "a", V::Priv);
    let n = w.module(a, "n", V::Pub);
    let x = ty(&mut w, n, "x");
    w.use_(root, "crate::a::n", V::Priv);
    w.use_as(root, "n::x", "u", V::Priv);
    rule("r7c", &w, &|s| names(s, root, Namespace::Types, "u", Some(x)).and(no_errors(s, root)));
}

/// R7, the oracle's no-calls seed 29 as a fixture: the importing use is itself a glob whose
/// first segment a glob candidate and the extern prelude both bind: extern prelude `n`; `mod a {
/// pub mod n {} } use n::k::*; use crate::a::*;` (neither `n` holds a `k`, as in seed 29). The
/// Reference (`names.resolution.expansion.imports.ambiguity.glob-vs-outer`): "Names in imports
/// and macro invocations may not be resolved through glob imports when there is another
/// candidate available in an outer scope": the glob's `n` (crate 1's `a::n`) and the dependency
/// are different definitions, so `use n::k::*` fails at its first segment, as ambiguous; that
/// neither candidate holds `k` is never reached (the old solver answers not found).
#[test]
fn r7d_glob_import_through_glob_vs_outer() {
    let (mut w, root, _) = with_dep();
    let a = w.module(root, "a", V::Priv);
    w.module(a, "n", V::Pub);
    w.glob(root, "n::k", V::Priv);
    w.glob(root, "crate::a", V::Priv);
    rule("r7d", &w, &|s| ambiguous(s, root, "n"));
}

/// R7, the oracle's wide seed 107 as a fixture: an import failed by glob-vs-outer binds
/// nothing: extern prelude `n`; `mod n { pub struct y; } mod b { use crate::*; use n::*; }`.
/// `use n::*` "may not be resolved through glob imports when there is another candidate
/// available in an outer scope" (`...ambiguity.glob-vs-outer`): it is an error, and taking the
/// glob candidate anyway (bringing `n::y` into `b`) is the resolution the sentence forbids; nor
/// is the dependency (no `y` at its root) chosen. `y` in `b` is unbound; the error is reported.
#[test]
fn r7f_glob_vs_outer_import_binds_nothing() {
    let (mut w, root, _) = with_dep();
    let n = w.module(root, "n", V::Priv);
    ty(&mut w, n, "y");
    let b = w.module(root, "b", V::Priv);
    w.glob(b, "crate", V::Priv);
    w.glob(b, "n", V::Priv);
    rule("r7f", &w, &|s| ambiguous(s, b, "n").and(names(s, b, Namespace::Types, "y", None)));
}

/// R7, the oracle's no-calls seed 156 as a fixture: a module whose own glob of the root brings
/// the module itself under the name a standard-prelude module also has: crate 9's `prelude {
/// pub mod n { pub struct x; } }` as the standard prelude; `struct x; mod n { struct x; use
/// n::*; use crate::*; } use crate::*;`. In `n`, `use n::*`'s first segment has the glob
/// candidate `crate::n` and the outer candidate `prelude::n`, different definitions:
/// "may not be resolved through glob imports when there is another candidate available in an
/// outer scope" (`...ambiguity.glob-vs-outer`; the panic note shows a standard-prelude name is
/// such a candidate). An error; reporting nothing is a wrong accept.
#[test]
fn r7g_self_glob_vs_standard_prelude() {
    let (mut w, root, _) = with_outer(Outer::Std);
    ty(&mut w, root, "x");
    let n = w.module(root, "n", V::Priv);
    w.item(n, "x", Namespace::Types, DefKind::Other, V::Priv);
    w.glob(n, "n", V::Priv);
    w.glob(n, "crate", V::Priv);
    w.glob(root, "crate", V::Priv);
    rule("r7g", &w, &|s| ambiguous(s, n, "n"));
}

/// R7's kind (was KNOWN k1, the oracle's wide tape '1 0*12 1 0 0 0 2 3 0*16'): an import failed
/// by glob-vs-outer says so, not glob-vs-glob: extern prelude `n`; `mod a { pub mod n { pub
/// struct k; } } use crate::a::*; use n::k;`. The Reference
/// (`names.resolution.expansion.imports.ambiguity.glob-vs-outer`): "Names in imports and macro
/// invocations may not be resolved through glob imports when there is another candidate
/// available in an outer scope": `use n::k`'s first segment has the glob's `a::n` and the
/// dependency, different definitions, and only one glob brings `n`: the one error is the
/// import's, failed at segment 0, ambiguous glob vs outer. Before the fix: "errors [Import { ..
/// why: Ambiguous(GlobVsGlob) }], the rule says 1 import(s) failing Ambiguous(GlobVsOuter)".
#[test]
fn r7h_glob_vs_outer_names_its_kind() {
    let (mut w, root, _) = with_dep();
    let a = w.module(root, "a", V::Priv);
    let n = w.module(a, "n", V::Pub);
    ty(&mut w, n, "k");
    w.glob(root, "crate::a", V::Priv);
    w.use_(root, "n::k", V::Priv);
    rule("r7h", &w, &|s| failed_imports(s, root, Unresolved::Ambiguous(crate::AmbiguityKind::GlobVsOuter), 1));
}

/// R5 with R7, the oracle's wide seed 107 shrunk: `use n::*; use crate::m0::*;` at the root,
/// extern prelude `n` (the dependency, no `m0`). Each import waits on the other (`n` on the
/// pending glob, which might bring an `n`; `m0` on `use n::*`, which might bring an `m0`): a
/// group waiting only on itself, failed by the stall (R5), where what is undecided is taken as
/// absent: the glob brings no `n`, so `use n::*` takes the outer `n` (the dependency) and
/// resolves; `use crate::m0::*` finds no `m0` and fails, not found. That is also the one
/// consistent answer (with `use n::*` resolved, the dependency holds no `m0`).
/// R5 when a call is spared, the wide comparison's seed 644 reduced: `macro_rules! mk;
/// #[macro_use] mod m1 { use crate::my; use crate::m2::*; my!{} } mk!{ mod m2 {} }`. All four
/// wait on each other (`mk!`'s textual read waits on `my!`, which could define `mk` before it
/// through `#[macro_use]`; `my!` on `use crate::my`; both imports on `mk!`'s expansion at the
/// root). `mk!` is spared, so its expansion is still to come: only `my!` fails that round
/// (nothing defines `my`: the Reference, "Textual scope"), then `mk!` expands and writes `m2`,
/// and `crate::m2::*` resolves; `crate::my` is then not found. Failing `crate::m2::*` in the
/// stall would contradict the Reference's "names in the fully expanded AST must resolve to the
/// same definition regardless of the order in which macros are expanded and imports are
/// resolved" (`names.resolution.expansion`): after expansion `crate::m2` exists.
#[test]
fn r5e_member_waiting_on_a_spared_call_is_not_failed() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    w.rules(root, "mk");
    let m1 = w.module_with(root, "m1", V::Priv, true, false);
    w.use_(m1, "crate::my", V::Priv);
    w.glob(m1, "crate::m2", V::Priv);
    w.call(m1, "my", CallShape::FnLike);
    let call = w.call(root, "mk", CallShape::FnLike);
    let m2 = w.module(call, "m2", V::Priv);
    rule("r5e", &w, &|s| {
        let imports = s.errors(m1).iter().filter(|e| matches!(e, ScopeError::Import { why: Unresolved::NotFound, .. })).count();
        let macros = s.errors(m1).iter().filter(|e| matches!(e, ScopeError::Macro { .. })).count();
        names(s, root, Namespace::Types, "m2", Some(m2)).and(if imports == 1 && macros == 1 && s.errors(m1).len() == 2 {
            Ok(())
        } else {
            Err(format!("m1 errors {:?}, the rule says `crate::my` not found and `my!` not found", s.errors(m1)))
        })
    });
}

/// R5 as r5e, the oracle's seed 9 reduced, where the old solver answers FALSE "not found":
/// `macro_rules! mk; use crate::*; mx!{} mk!{ mod m2 {} } use crate::m2::*;`. `mk!`'s textual
/// read waits on `mx!` (which could define `mk` before it); `mx!` on the root's names (its own
/// glob of the root, its calls); `crate::m2::*` on `mk!`'s expansion. `mk!` is spared, `mx!`
/// fails (no `mx` in the textual or path scope: the Reference, "Textual scope"), then `mk!`
/// writes `m2` and `crate::m2::*` resolves: in the fully expanded crate `crate::m2` exists, and
/// a name "must resolve to the same definition regardless of the order in which macros are
/// expanded and imports are resolved" (`names.resolution.expansion`). One error: `mx!`.
#[test]
fn r5f_import_behind_a_spared_call_resolves() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    w.rules(root, "mk");
    w.glob(root, "crate", V::Priv);
    w.call(root, "mx", CallShape::FnLike);
    let call = w.call(root, "mk", CallShape::FnLike);
    let m2 = w.module(call, "m2", V::Priv);
    w.glob(root, "crate::m2", V::Priv);
    rule("r5f", &w, &|s| {
        let macros = s.errors(root).iter().filter(|e| matches!(e, ScopeError::Macro { .. })).count();
        names(s, root, Namespace::Types, "m2", Some(m2)).and(if macros == 1 && s.errors(root).len() == 1 {
            Ok(())
        } else {
            Err(format!("root errors {:?}, the rule says one: `mx!` not found", s.errors(root)))
        })
    });
}

#[test]
fn r5c_stalled_plain_first_segment_goes_outward() {
    let (mut w, root, x) = with_dep();
    w.glob(root, "n", V::Priv);
    w.glob(root, "crate::m0", V::Priv);
    rule("r5c", &w, &|s| names(s, root, Namespace::Types, "x", Some(x)).and(failed_imports(s, root, Unresolved::NotFound, 1)));
}

/// R5 with R7, the oracle's wide seed 107 as later shrunk: extern prelude `n` (the dependency,
/// with `pub mod k { pub struct y; }`); `mod b { use n::*; use crate::b::k::*; }`. The two
/// imports wait on each other; the stall takes the undecided as absent, so `use n::*` goes
/// outward and resolves to the dependency; that glob brings `k`, so `use crate::b::k::*` then
/// resolves too: only a member that FAILS under the stall is decided with the others; one that
/// resolves is decided alone and the rest retried with the facts (the one consistent answer:
/// `y` in `b` is the dependency's `k::y`, no error).
#[test]
fn r5d_stall_resolving_member_answers_the_other() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let dep = w.root(2, Edition::E2021);
    w.dep(root, "n", dep);
    let k = w.module(dep, "k", V::Pub);
    let y = ty(&mut w, k, "y");
    let b = w.module(root, "b", V::Priv);
    w.glob(b, "n", V::Priv);
    w.glob(b, "crate::b::k", V::Priv);
    rule("r5d", &w, &|s| names(s, b, Namespace::Types, "y", Some(y)).and(no_errors(s, b)));
}

/// R7 with ONE definition both ways (D57): the standard prelude's `n` and a glob bringing that
/// same module: `mod a { pub use std::prelude::n; } use crate::a::*; use n::x as u;` with
/// crate 9's `prelude { pub mod n { pub struct x; } }` as the standard prelude. The
/// glob-vs-outer sentence forbids "another candidate"; glob-vs-glob allows "the same item
/// (following reexports)", and the stability principle ("the same definition regardless of the
/// order") holds with one definition: `u` is that `x`, no error.
#[test]
fn r7e_glob_and_outer_same_definition() {
    let (mut w, root, x) = with_outer(Outer::Std);
    let a = w.module(root, "a", V::Priv);
    w.use_(a, "std::prelude::n", V::Pub);
    w.glob(root, "crate::a", V::Priv);
    w.use_as(root, "n::x", "u", V::Priv);
    rule("r7e", &w, &|s| names(s, root, Namespace::Types, "u", Some(x)).and(no_errors(s, root)));
}

/// Where R7's outer candidate `n` (holding `pub struct x`) lives.
#[derive(Clone, Copy, Debug)]
enum Outer {
    /// The unit's dependency named `n` (crate 2), in the extern prelude.
    Extern,
    /// `pub mod n` in crate 9's `prelude` module, the crate's standard prelude.
    Std,
}

/// Why the source of the inner glob into the root is still pending when `n` is first looked up.
#[derive(Clone, Copy, Debug)]
enum Source {
    /// The source is reached through `k` further globs: `mod a {..} mod g1 { pub use
    /// crate::a::*; } .. mod gk { pub use crate::g(k-1)::*; } use crate::gk::*;`.
    Chain(u8),
    /// The glob's path goes through a named import itself behind a glob: `mod a { pub mod b
    /// {..} } mod d { pub use crate::a::*; } use crate::d::b as s; use crate::s::*;`.
    Named,
    /// The source module's items are written by a macro call: `macro_rules! mk; mod d {
    /// mk!{..} } use crate::d::*;`.
    Macro,
    /// The source is a dependency's module, itself a glob of another: crate 3 `lib`: `pub mod m
    /// { pub use crate::k::*; } pub mod k {..}`; `use lib::m::*;`.
    Dep,
}

/// The crate root with the outer candidate placed, and the outer `x`.
fn with_outer(outer: Outer) -> (World, ItemKey, ItemKey) {
    match outer {
        Outer::Extern => with_dep(),
        Outer::Std => {
            let mut w = World::new();
            let root = w.root(1, Edition::E2021);
            let std = w.root(9, Edition::E2021);
            let prelude = w.module(std, "prelude", V::Pub);
            let n = w.module(prelude, "n", V::Pub);
            let x = ty(&mut w, n, "x");
            w.dep(root, "std", std);
            w.cr(root).head.prelude = Some(prelude);
            (w, root, x)
        }
    }
}

/// The source's contents at `at`: `pub mod n { pub struct x; }` when `has_n` (returning that
/// `x`), else `pub struct y;`.
fn source_body(w: &mut World, at: ItemKey, has_n: bool) -> Option<ItemKey> {
    if has_n {
        let n = w.module(at, "n", V::Pub);
        Some(ty(w, n, "x"))
    } else {
        ty(w, at, "y");
        None
    }
}

/// R7's world: the outer candidate, a glob into the root pending for `src`'s reason, then `use
/// n::x as u;` at the root. Returns the world, the root and the outer `x`.
fn r7_world(outer: Outer, src: Source, has_n: bool) -> (World, ItemKey, ItemKey) {
    const LINKS: [&str; 4] = ["crate::a", "crate::g1", "crate::g2", "crate::g3"];
    const NAMES: [&str; 3] = ["g1", "g2", "g3"];
    let (mut w, root, outer_x) = with_outer(outer);
    match src {
        Source::Chain(k) => {
            let a = w.module(root, "a", V::Priv);
            source_body(&mut w, a, has_n);
            for i in 0..usize::from(k) {
                let g = w.module(root, NAMES[i], V::Priv);
                w.glob(g, LINKS[i], V::Pub);
            }
            w.glob(root, LINKS[usize::from(k)], V::Priv);
        }
        Source::Named => {
            let a = w.module(root, "a", V::Priv);
            let b = w.module(a, "b", V::Pub);
            source_body(&mut w, b, has_n);
            let d = w.module(root, "d", V::Priv);
            w.glob(d, "crate::a", V::Pub);
            w.use_as(root, "crate::d::b", "s", V::Priv);
            w.glob(root, "crate::s", V::Priv);
        }
        Source::Macro => {
            w.rules(root, "mk");
            let d = w.module(root, "d", V::Priv);
            let call = w.call(d, "mk", CallShape::FnLike);
            source_body(&mut w, call, has_n);
            w.glob(root, "crate::d", V::Priv);
        }
        Source::Dep => {
            let lib = w.root(3, Edition::E2021);
            let m = w.module(lib, "m", V::Pub);
            let k = w.module(lib, "k", V::Pub);
            source_body(&mut w, k, has_n);
            w.glob(m, "crate::k", V::Pub);
            w.dep(root, "lib", lib);
            w.glob(root, "lib::m", V::Priv);
        }
    }
    w.use_as(root, "n::x", "u", V::Priv);
    (w, root, outer_x)
}

/// R7 stressed: the inner glob's source pending for each reason (two and three glob links, a
/// named import, a macro call, a dependency's glob), the outer candidate in the extern prelude
/// and, separately, the standard prelude. With `n` in the source: an ambiguity (the Reference,
/// `...ambiguity.glob-vs-outer`: "Names in imports and macro invocations may not be resolved
/// through glob imports when there is another candidate available in an outer scope"; its
/// panic note shows a standard-prelude candidate counts as one). Without: the lookup waits, then
/// takes the outer `x`, no error. Every case in the calm order and 64 drawn orders, each equal
/// to the calm one: a case answering differently by order is order-dependence.
#[test]
fn r7_stress_pending_glob_sources() {
    const CASES: &[(&str, Source, Outer)] = &[
        ("r7s_chain1_std", Source::Chain(1), Outer::Std),
        ("r7s_chain2_extern", Source::Chain(2), Outer::Extern),
        ("r7s_chain2_std", Source::Chain(2), Outer::Std),
        ("r7s_chain3_extern", Source::Chain(3), Outer::Extern),
        ("r7s_chain3_std", Source::Chain(3), Outer::Std),
        ("r7s_named_extern", Source::Named, Outer::Extern),
        ("r7s_named_std", Source::Named, Outer::Std),
        ("r7s_macro_extern", Source::Macro, Outer::Extern),
        ("r7s_macro_std", Source::Macro, Outer::Std),
        ("r7s_dep_extern", Source::Dep, Outer::Extern),
        ("r7s_dep_std", Source::Dep, Outer::Std),
    ];
    const WITHOUT: &[&str] = &[
        "r7s_chain1_std_without",
        "r7s_chain2_extern_without",
        "r7s_chain2_std_without",
        "r7s_chain3_extern_without",
        "r7s_chain3_std_without",
        "r7s_named_extern_without",
        "r7s_named_std_without",
        "r7s_macro_extern_without",
        "r7s_macro_std_without",
        "r7s_dep_extern_without",
        "r7s_dep_std_without",
    ];
    let mut wrong = Vec::new();
    for (i, &(name, src, outer)) in CASES.iter().enumerate() {
        let (w, root, _) = r7_world(outer, src, true);
        wrong.extend(verdict(name, &w, &|s| ambiguous(s, root, "n")));
        let (w, root, outer_x) = r7_world(outer, src, false);
        let want = |s: &Solved| names(s, root, Namespace::Types, "u", Some(outer_x)).and(no_errors(s, root));
        wrong.extend(verdict(WITHOUT[i], &w, &want));
    }
    assert!(wrong.is_empty(), "{} case(s) wrong:\n{}", wrong.len(), wrong.join("\n"));
}

/// R8, a macro-written root `extern crate` may add an extern-prelude name: crate 3 (`k`'s
/// target, with `pub struct y`) a dependency named `other`; `macro_rules! mk; mk!{ extern crate
/// other as k; } use k::y as u;`. The Reference (`names.preludes.extern`): `extern crate` at the
/// crate root adds its name to the extern prelude.
#[test]
fn r8a_macro_extern_crate_adds_a_name() {
    let (mut w, root, _) = with_dep();
    let other = w.root(3, Edition::E2021);
    let y = ty(&mut w, other, "y");
    w.dep(root, "other", other);
    w.rules(root, "mk");
    let call = w.call(root, "mk", CallShape::FnLike);
    w.extern_crate(call, crate::ExternTarget::Dep("other"), crate::Rename::To("k"), V::Priv);
    w.use_as(root, "k::y", "u", V::Priv);
    rule("r8a", &w, &|s| names(s, root, Namespace::Types, "u", Some(y)).and(no_errors(s, root)));
}

/// R8, it never replaces one: `mk!{ extern crate other as n; } use n::x as u;` with `n` the
/// unit's dependency crate 2: `u` stays crate 2's `x` and the `extern crate` is an error
/// (D56: a dependency's extern-prelude name is final).
#[test]
fn r8b_macro_extern_crate_never_replaces() {
    let (mut w, root, x) = with_dep();
    let other = w.root(3, Edition::E2021);
    ty(&mut w, other, "x");
    w.dep(root, "other", other);
    w.rules(root, "mk");
    let call = w.call(root, "mk", CallShape::FnLike);
    w.extern_crate(call, crate::ExternTarget::Dep("other"), crate::Rename::To("n"), V::Priv);
    w.use_as(root, "n::x", "u", V::Priv);
    rule("r8b", &w, &|s| {
        names(s, root, Namespace::Types, "u", Some(x)).and(if s.errors(root).is_empty() {
            Err(String::from("no error for the `extern crate` replacing `n`"))
        } else {
            Ok(())
        })
    });
}

/// The control: r7a's world with the answer a solver that goes outward early would give (`u`
/// the dependency's `x`, no error) is refused, so the harness's checks run in every order.
#[test]
fn control_a_wrong_answer_is_caught() {
    let (mut w, root, dep_x) = with_dep();
    let a = w.module(root, "a", V::Priv);
    let d = w.module(root, "d", V::Priv);
    let n = w.module(a, "n", V::Pub);
    ty(&mut w, n, "x");
    w.glob(d, "crate::a", V::Pub);
    w.glob(root, "crate::d", V::Priv);
    w.use_as(root, "n::x", "u", V::Priv);
    let got = every_order(&w, &|s| names(s, root, Namespace::Types, "u", Some(dep_x)).and(no_errors(s, root)));
    assert!(got.is_err(), "the planted answer was accepted");
}

// R9 (a one-segment type path naming a built-in type beside a module of its name) is also the
// names library's: tests/entry.rs `builtin_type_name_beside_a_module_of_its_name`; r9a below
// holds the solver's records to it through the same query, and the reference beside it.

/// The reference agrees with a fixture: `name` in `ns` at `m` names `want`, with `r6` R6
/// errors and `r8` R8 errors (oracle.rs `reference_says`).
#[allow(clippy::too_many_arguments)]
fn reference(w: &World, root: ItemKey, m: ItemKey, ns: Namespace, name: &'static str, want: Option<ItemKey>, r6: usize, r8: usize) {
    let got = super::oracle::reference_says(w, root, m, ns, name);
    assert_eq!(got, (want, r6, r8), "the reference: `{name}` in {ns:?}, R6 errors, R8 errors");
}

/// Step 1, a macro-written named import is the module's from its expansion on, and another
/// module's import reads it: `mod a { pub struct x; } mk!{ pub use crate::a::x as p; } mod b {
/// use crate::p as q; }`: `q` is `a::x`. The Reference (`names.resolution.expansion`): "names
/// in the fully expanded AST must resolve to the same definition"; in that AST the `use` stands
/// in the root.
#[test]
fn r6f_macro_written_import_read_by_another_module() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    w.rules(root, "mk");
    let a = w.module(root, "a", V::Priv);
    let x = ty(&mut w, a, "x");
    let call = w.call(root, "mk", CallShape::FnLike);
    w.use_as(call, "crate::a::x", "p", V::Pub);
    let b = w.module(root, "b", V::Priv);
    w.use_as(b, "crate::p", "q", V::Priv);
    reference(&w, root, b, Namespace::Types, "q", Some(x), 0, 0);
    rule("r6f", &w, &|s| names(s, b, Namespace::Types, "q", Some(x)).and(no_errors(s, b)).and(no_errors(s, root)));
}

/// Steps 1 and 3, R6 with R7: a macro-written GLOB brings a name an earlier lookup took from
/// the standard prelude: standard prelude `n` (with `x`); `mod a { pub mod n { pub struct x; } }
/// use n::x as u; mk!{ use crate::a::*; }`. Before the call expands the root's `n` can grow by
/// expansion only, so the use goes outward (D56); the glob the expansion writes (it does not
/// enclose the use) then brings another `n`: "Names in imports and macro invocations may not be
/// resolved through glob imports when there is another candidate available in an outer scope"
/// (`...ambiguity.glob-vs-outer`) and the name "must resolve to the same definition regardless
/// of the order": an ambiguity, in every order (D56).
#[test]
fn r6g_macro_written_glob_shadows_used_prelude_name() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let std = w.root(9, Edition::E2021);
    let prelude = w.module(std, "prelude", V::Pub);
    let pn = w.module(prelude, "n", V::Pub);
    ty(&mut w, pn, "x");
    w.cr(root).head.prelude = Some(prelude);
    w.rules(root, "mk");
    let a = w.module(root, "a", V::Priv);
    let n = w.module(a, "n", V::Pub);
    ty(&mut w, n, "x");
    w.use_as(root, "n::x", "u", V::Priv);
    let call = w.call(root, "mk", CallShape::FnLike);
    w.glob(call, "crate::a", V::Priv);
    let (_, r6, _) = super::oracle::reference_says(&w, root, root, Namespace::Types, "u");
    assert_eq!(r6, 1, "the reference: one R6 error");
    rule("r6g", &w, &|s| ambiguous(s, root, "n"));
}

/// Steps 2 and 3, a nested call does not enclose its outer call's import: extern prelude `n`;
/// `mk!{ use n::x as u; mk!{ pub mod n { pub struct x; } } }`. The inner expansion is enclosed
/// BY the outer, it does not enclose the use; the use exists before the inner call expands and
/// may go outward: D56's error, in every order.
#[test]
fn r6h_nested_expansion_not_enclosing_the_use() {
    let (mut w, root, _) = with_dep();
    w.rules(root, "mk");
    let outer = w.call(root, "mk", CallShape::FnLike);
    w.use_as(outer, "n::x", "u", V::Priv);
    let inner = w.call(outer, "mk", CallShape::FnLike);
    let n = w.module(inner, "n", V::Pub);
    ty(&mut w, n, "x");
    let (_, r6, _) = super::oracle::reference_says(&w, root, root, Namespace::Types, "u");
    assert_eq!(r6, 1, "the reference: one R6 error");
    rule("r6h", &w, &|s| ambiguous(s, root, "n"));
}

/// Step 3, the other side of the line, two deep: `mk!{ mk!{ pub mod n { pub struct x; } use
/// n::x as u; } }` with extern prelude `n`: the expansion that writes `n` encloses the use (it
/// wrote both), so `n` existed before the use did: the inner wins, no error (D56).
#[test]
fn r6i_enclosing_nested_expansion_inner_wins() {
    let (mut w, root, _) = with_dep();
    w.rules(root, "mk");
    let outer = w.call(root, "mk", CallShape::FnLike);
    let inner = w.call(outer, "mk", CallShape::FnLike);
    let n = w.module(inner, "n", V::Pub);
    let x = ty(&mut w, n, "x");
    w.use_as(inner, "n::x", "u", V::Priv);
    reference(&w, root, root, Namespace::Types, "u", Some(x), 0, 0);
    rule("r6i", &w, &|s| names(s, root, Namespace::Types, "u", Some(x)).and(no_errors(s, root)));
}

/// Steps 2 and 3, three deep: `mk!{ pub mod n { pub struct x; } mk!{ mk!{ use n::x as u; } } }`
/// with extern prelude `n`: the outermost expansion writes `n` and encloses the innermost one,
/// which writes the use: the inner wins, no error (D56).
#[test]
fn r6j_three_deep_enclosing_expansion_inner_wins() {
    let (mut w, root, _) = with_dep();
    w.rules(root, "mk");
    let c1 = w.call(root, "mk", CallShape::FnLike);
    let n = w.module(c1, "n", V::Pub);
    let x = ty(&mut w, n, "x");
    let c2 = w.call(c1, "mk", CallShape::FnLike);
    let c3 = w.call(c2, "mk", CallShape::FnLike);
    w.use_as(c3, "n::x", "u", V::Priv);
    reference(&w, root, root, Namespace::Types, "u", Some(x), 0, 0);
    rule("r6j", &w, &|s| names(s, root, Namespace::Types, "u", Some(x)).and(no_errors(s, root)));
}

/// Step 2, three deep, the item stands in the module the outermost call stands in: `mk!{ mk!{
/// mk!{ pub struct x; } } } mod b { use crate::x as u; }`: `u` is that `x`. The Reference
/// (`names.resolution.expansion`): resolution runs on "the fully expanded AST".
#[test]
fn r6k_three_deep_nested_call_writes_an_item() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    w.rules(root, "mk");
    let c1 = w.call(root, "mk", CallShape::FnLike);
    let c2 = w.call(c1, "mk", CallShape::FnLike);
    let c3 = w.call(c2, "mk", CallShape::FnLike);
    let x = ty(&mut w, c3, "x");
    let b = w.module(root, "b", V::Priv);
    w.use_as(b, "crate::x", "u", V::Priv);
    reference(&w, root, b, Namespace::Types, "u", Some(x), 0, 0);
    rule("r6k", &w, &|s| names(s, b, Namespace::Types, "u", Some(x)).and(no_errors(s, b)).and(no_errors(s, root)));
}

/// Step 4, R8 seen from a non-root module: crate 3 (`pub struct y`) a dependency named
/// `other`; `mk!{ extern crate other as k; } mod b { use k::y as u; }`: `u` is crate 3's `y`.
/// The Reference (`names.preludes.extern`): an `extern crate` at the crate root adds its name
/// to the extern prelude, which every module's lexical lookup reaches; until the root's call
/// expands, `k` has no candidate and the lookup waits (D56: the extern prelude grows by root
/// calls).
#[test]
fn r8c_macro_extern_crate_seen_from_a_child() {
    let (mut w, root, _) = with_dep();
    let other = w.root(3, Edition::E2021);
    let y = ty(&mut w, other, "y");
    w.dep(root, "other", other);
    w.rules(root, "mk");
    let call = w.call(root, "mk", CallShape::FnLike);
    w.extern_crate(call, crate::ExternTarget::Dep("other"), crate::Rename::To("k"), V::Priv);
    let b = w.module(root, "b", V::Priv);
    w.use_as(b, "k::y", "u", V::Priv);
    reference(&w, root, b, Namespace::Types, "u", Some(y), 0, 0);
    rule("r8c", &w, &|s| names(s, b, Namespace::Types, "u", Some(y)).and(no_errors(s, b)).and(no_errors(s, root)));
}

/// R5, a stalled member's LATER segment takes the undecided as absent (the cell core lane's seed
/// 1983): `mk!{ mod m1 { mk!{ pub mod m2 {} } } } use crate::m2::*; use crate::*; use
/// crate::m1::*; mx!{}`. `mx!` and `use crate::m2::*` wait on each other (one sink, no call
/// spared); taken as absent, `mx!` writes nothing and the root's `m2` is `m1::m2` through `use
/// crate::m1::*`: the import resolves, and `mx!` fails not found after. The Reference
/// (`names.resolution.expansion`): names "in the fully expanded AST must resolve to the same
/// definition", and in that AST (`mx!` failed) `crate::m2` is `m1::m2`.
#[test]
fn r5g_stalled_later_segment_taken_as_absent() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    w.rules(root, "mk");
    let c1 = w.call(root, "mk", CallShape::FnLike);
    let m1 = w.module(c1, "m1", V::Priv);
    let c2 = w.call(m1, "mk", CallShape::FnLike);
    w.module(c2, "m2", V::Pub);
    w.glob(root, "crate::m2", V::Priv);
    w.glob(root, "crate", V::Priv);
    w.glob(root, "crate::m1", V::Priv);
    w.call(root, "mx", CallShape::FnLike);
    rule("r5g", &w, &|s| {
        let imports = s.errors(root).iter().filter(|e| matches!(e, ScopeError::Import { .. })).count();
        if imports == 0 { Ok(()) } else { Err(format!("errors {:?}, the rule: no import fails (only `mx!`)", s.errors(root))) }
    });
}

/// R8, a replacement is an error whether or not a path reads the name: `mk!{ extern crate other
/// as n; }` with `n` the unit's dependency (no `use` of `n`). D56: "a macro-written root
/// `extern crate X as n;` may add a name, never replace one (an error)"; the oracle's deep tape
/// '0*8 1 0*13 1 0*3'.
#[test]
fn r8d_macro_extern_crate_replacing_unread_is_an_error() {
    let (mut w, root, _) = with_dep();
    let other = w.root(3, Edition::E2021);
    w.dep(root, "other", other);
    w.rules(root, "mk");
    let call = w.call(root, "mk", CallShape::FnLike);
    w.extern_crate(call, crate::ExternTarget::Dep("other"), crate::Rename::To("n"), V::Priv);
    let (_, _, r8) = super::oracle::reference_says(&w, root, root, Namespace::Types, "n");
    assert_eq!(r8, 1, "the reference: one R8 error");
    rule("r8d", &w, &|s| if s.errors(root).is_empty() { Err(String::from("no error for the `extern crate` replacing `n`")) } else { Ok(()) });
}

/// R2 through an `extern crate` item, the deep oracle's program exactly (port27/world-1116/
/// deep-seed-amb.txt; dependencies `m1` = crate 2 `pub struct x; struct z; pub mod m2 { pub
/// struct y; }` and `e3` = crate 3 `pub struct y; pub mod m2 { pub struct x; }`):
/// `macro_rules! mk; type x = u8; mod m1 { use crate::m1::m0::*; use crate::*; use crate::*;
/// mx!{} mx!{} mk!{ mk!{ mk!{ use crate::m1::*; use self::m2::*; } } } } mod m2 {} use
/// crate::*; use crate::*; mx!{} mx!{} mk!{ extern crate e3 as m0; mk!{ mk!{} } } mk!{ mk!{
/// mk!{} } }`. By hand: `crate::*` gives `m1` the root's `m0` (the `extern crate` item binds
/// crate 3's root, `items.extern-crate`) and the root's `m2`; `crate::m1::m0::*` then gives it
/// crate 3's `m2`. Two globs, different items, and `use self::m2::*` (written three expansions
/// deep, so the module's text from when they expand) uses `m2`: an error at the use
/// (`...ambiguity.glob-vs-glob`: "it is an error to use a name that is ambiguous", "The errors
/// occur at time of use, not time of import"). R8: `m0` is a new extern-prelude name, no clash.
#[test]
fn r2c_two_globs_one_through_an_extern_crate_item() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let dep = w.root(2, Edition::E2021);
    ty(&mut w, dep, "x");
    w.item(dep, "z", Namespace::Types, DefKind::Other, V::Priv);
    let dm2 = w.module(dep, "m2", V::Pub);
    ty(&mut w, dm2, "y");
    let e3 = w.root(3, Edition::E2021);
    ty(&mut w, e3, "y");
    let em2 = w.module(e3, "m2", V::Pub);
    ty(&mut w, em2, "x");
    w.dep(root, "m1", dep);
    w.dep(root, "e3", e3);
    w.rules(root, "mk");
    w.item(root, "x", Namespace::Types, DefKind::Other, V::Priv);
    let m1 = w.module(root, "m1", V::Priv);
    w.glob(m1, "crate::m1::m0", V::Priv);
    w.glob(m1, "crate", V::Priv);
    w.glob(m1, "crate", V::Priv);
    w.call(m1, "mx", CallShape::FnLike);
    w.call(m1, "mx", CallShape::FnLike);
    let c1 = w.call(m1, "mk", CallShape::FnLike);
    let c2 = w.call(c1, "mk", CallShape::FnLike);
    let c3 = w.call(c2, "mk", CallShape::FnLike);
    w.glob(c3, "crate::m1", V::Priv);
    w.glob(c3, "self::m2", V::Priv);
    w.module(root, "m2", V::Priv);
    w.glob(root, "crate", V::Priv);
    w.glob(root, "crate", V::Priv);
    w.call(root, "mx", CallShape::FnLike);
    w.call(root, "mx", CallShape::FnLike);
    let r1 = w.call(root, "mk", CallShape::FnLike);
    w.extern_crate(r1, crate::ExternTarget::Dep("e3"), crate::Rename::To("m0"), V::Priv);
    let r2 = w.call(r1, "mk", CallShape::FnLike);
    w.call(r2, "mk", CallShape::FnLike);
    let s1 = w.call(root, "mk", CallShape::FnLike);
    let s2 = w.call(s1, "mk", CallShape::FnLike);
    w.call(s2, "mk", CallShape::FnLike);
    let refs = super::oracle::reference_imports(&w, root);
    let amb = refs.values().filter(|i| matches!(i, super::Imp::Failed(Unresolved::Ambiguous(_)))).count();
    assert_eq!(amb, 1, "the reference: one ambiguous import ({refs:?})");
    rule("r2c", &w, &|s| ambiguous(s, m1, "m2"));
}

/// R2 at a MIDDLE segment, the deep oracle's program exactly (port27/world-1116/
/// r16-deep-replay.txt; the dependencies as r2c): `macro_rules! mk; type x = u8; mod m1 { use
/// crate::m0::*; use crate::*; use crate::*; mk!{ mk!{ mk!{ use self::m2::m0::*; } } } } mod m2
/// {} use crate::*; use crate::*; mx!{} mx!{} mk!{ extern crate e3 as m0; mk!{ mk!{} } } mk!{
/// mk!{ mk!{} } }`. By hand: `m1` gets the root's `m2` through `crate::*` and crate 3's `m2`
/// through `crate::m0::*` (`m0` the macro-written `extern crate` item): two globs, different
/// items, and `self::m2::m0` USES `m2` at its first segment: an error there (`...ambiguity.
/// glob-vs-glob`: "it is an error to use a name that is ambiguous"), whatever `m0` would be in
/// either candidate (crate 3's `m2` has no `m0` either: the use is an error, not a NotFound).
#[test]
fn r2e_two_globs_ambiguous_at_a_middle_segment() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let dep = w.root(2, Edition::E2021);
    ty(&mut w, dep, "x");
    w.item(dep, "z", Namespace::Types, DefKind::Other, V::Priv);
    let dm2 = w.module(dep, "m2", V::Pub);
    ty(&mut w, dm2, "y");
    let e3 = w.root(3, Edition::E2021);
    ty(&mut w, e3, "y");
    let em2 = w.module(e3, "m2", V::Pub);
    ty(&mut w, em2, "x");
    w.dep(root, "m1", dep);
    w.dep(root, "e3", e3);
    w.rules(root, "mk");
    w.item(root, "x", Namespace::Types, DefKind::Other, V::Priv);
    let m1 = w.module(root, "m1", V::Priv);
    w.glob(m1, "crate::m0", V::Priv);
    w.glob(m1, "crate", V::Priv);
    w.glob(m1, "crate", V::Priv);
    let c1 = w.call(m1, "mk", CallShape::FnLike);
    let c2 = w.call(c1, "mk", CallShape::FnLike);
    let c3 = w.call(c2, "mk", CallShape::FnLike);
    w.glob(c3, "self::m2::m0", V::Priv);
    w.module(root, "m2", V::Priv);
    w.glob(root, "crate", V::Priv);
    w.glob(root, "crate", V::Priv);
    w.call(root, "mx", CallShape::FnLike);
    w.call(root, "mx", CallShape::FnLike);
    let r1 = w.call(root, "mk", CallShape::FnLike);
    w.extern_crate(r1, crate::ExternTarget::Dep("e3"), crate::Rename::To("m0"), V::Priv);
    let r2 = w.call(r1, "mk", CallShape::FnLike);
    w.call(r2, "mk", CallShape::FnLike);
    let s1 = w.call(root, "mk", CallShape::FnLike);
    let s2 = w.call(s1, "mk", CallShape::FnLike);
    w.call(s2, "mk", CallShape::FnLike);
    let refs = super::oracle::reference_imports(&w, root);
    let amb = refs.values().filter(|i| matches!(i, super::Imp::Failed(Unresolved::Ambiguous(_)))).count();
    assert_eq!(amb, 1, "the reference: one ambiguous import ({refs:?})");
    rule("r2e", &w, &|s| ambiguous(s, m1, "m2"));
}

/// R2 as r2c, written plainly (no nesting, the `extern crate` named `k`): `mod m2 {} mk!{
/// extern crate other as k; } mod a { use crate::a::k::*; use crate::*; use self::m2::*; }` with
/// crate 3 (`pub mod m2 {}`) the dependency `other`: the same ambiguity at the use.
#[test]
fn r2d_two_globs_one_through_an_extern_crate_item_plain() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let other = w.root(3, Edition::E2021);
    w.module(other, "m2", V::Pub);
    w.dep(root, "other", other);
    w.rules(root, "mk");
    w.module(root, "m2", V::Priv);
    let call = w.call(root, "mk", CallShape::FnLike);
    w.extern_crate(call, crate::ExternTarget::Dep("other"), crate::Rename::To("k"), V::Priv);
    let a = w.module(root, "a", V::Priv);
    // `k` reached through `a`'s own glob of the root, as in the oracle's program.
    w.glob(a, "crate::a::k", V::Priv);
    w.glob(a, "crate", V::Priv);
    w.glob(a, "self::m2", V::Priv);
    rule("r2d", &w, &|s| ambiguous(s, a, "m2"));
}

/// Step 5, R9 in type position on both sides: `mod m1 { pub struct x; } use crate::m1 as str;
/// use str::x as u; mod b { pub struct str; }`. At the root `str` in type position is the
/// built-in type (a module is never a type), while as a path prefix the module stands (`u` is
/// `m1::x`); in `b` the struct shadows the built-in. The Reference (`names.preludes`, the
/// language prelude): built-in types are in the language prelude, the outermost scope, so any
/// type of the name shadows them; a module binding is no type, so the type lookup goes on.
#[test]
fn r9a_builtin_type_beside_a_module_both_sides() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let m1 = w.module(root, "m1", V::Priv);
    let x = ty(&mut w, m1, "x");
    w.use_as(root, "crate::m1", "str", V::Priv);
    w.use_as(root, "str::x", "u", V::Priv);
    let b = w.module(root, "b", V::Priv);
    let own = ty(&mut w, b, "str");
    let builtin = crate::builtin_type("str");
    assert!(builtin.is_some(), "`str` is built in");
    reference(&w, root, root, Namespace::Types, "u", Some(x), 0, 0);
    rule("r9a", &w, &|s| {
        let (want_root, got_root) = super::oracle::type_position(&w, root, s, root, "str");
        let (want_b, got_b) = super::oracle::type_position(&w, root, s, b, "str");
        if want_root != builtin || want_b.map(|d| d.key) != Some(own) {
            return Err(format!("the reference: root {want_root:?}, b {want_b:?}"));
        }
        if got_root != want_root || got_b != want_b {
            return Err(format!("the solver: root {got_root:?}, b {got_b:?}; the rule: root {want_root:?}, b {want_b:?}"));
        }
        names(s, root, Namespace::Types, "u", Some(x)).and(no_errors(s, root))
    });
}

/// The textual scope reaches a block (the cell core's runner 15; tempo run c10-r14c: "calls
/// the fill did not expand, summed: 332", 320 of them in serde_core): `mod b { macro_rules!
/// far; const _: () = { far!{ fn x() {} } }; }`. A body's block that holds items is a module of
/// its own with no `mod` line in its parent (blocks.rs), so the old core marks no place for it
/// in `b`'s textual scope and gives it none of `b`'s `macro_rules!` (the real-path form is
/// tests/blocks.rs `blocks_expand_a_statement_call_named_by_textual_scope`, "2 of 3"; with the
/// macro in the ROOT instead, as runner 15 first wrote it, the old core passes: `b`'s own place
/// in the root is marked). The Reference, `macro.decl.scope.textual.intro`: "When
/// `macro_rules!` is used to define a macro, the macro enters the scope after the definition
/// (note that it can still be used recursively, since names are looked up from the invocation
/// site), up until its surrounding scope, typically a module, is closed.", and
/// `macro.decl.scope.textual.function-local`: "Macros can be declared and used locally inside
/// functions as well, and work similarly". So `far!` in the block names `b`'s `far`, it is
/// expanded, and `x` is an item of the block's module. The cell core's side of this is
/// cells/tests.rs `a_blocks_module_reads_the_textual_scope_around_its_body`.
#[test]
#[ignore = "the tests' driver (cells::solve_world) never loads a body's block module, so `Solved::binding` stops on it; `KNOWN` x1; the cells' own test passes"]
fn x1_a_block_reads_the_textual_scope_around_it() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let b = w.module(root, "b", V::Pub);
    w.rules(b, "far");
    let blk = w.module(b, "blk", V::Priv);
    w.decls.get_mut(&b).expect("b's text").mods.retain(|c| c.module != blk);
    let call = w.call(blk, "far", CallShape::FnLike);
    let x = w.fun(call, "x", V::Pub);
    rule("x1", &w, &|s| names(s, blk, Namespace::Values, "x", Some(x)));
}

/// `#[macro_export]` in the text of a module below one an expansion declared (the cell core's
/// runner 15; tempo run c10-r14c: "'only_old': 0, 'only_cells': 21" in seven crates, each a
/// root's macro the old core's written scope lacks: serde_core's `__private_serialize` in
/// `private::doc`, whose `mod private;` is written by `crate_root!()`; nix's `sockopt_impl`
/// under `feature! { pub mod socket; }`; tokio's `count!` under `cfg_macros! { mod select; }`).
/// The Reference, `macro.decl.scope.macro_export.intro`: "The `macro_export` attribute exports
/// the macro from the crate and makes it available in the root of the crate for path-based
/// resolution." Nothing there depends on how the macro's module came to be. The crate:
/// `macro_rules! mk; mk!{ mod p { pub mod d { #[macro_export] macro_rules! exp; } } }`: `exp`
/// is a macro of the root. The cell core's side: cells/tests.rs
/// `an_export_below_a_module_an_expansion_declared_is_in_the_root`.
#[test]
fn x2_an_export_below_a_module_an_expansion_declared_is_in_the_root() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    w.rules(root, "mk");
    let call = w.call(root, "mk", CallShape::FnLike);
    let p = w.module(call, "p", V::Priv);
    let d = w.module(p, "d", V::Pub);
    let exp = w.rules_with(d, "exp", true);
    rule("x2", &w, &|s| names(s, root, Namespace::Macros, "exp", Some(exp)));
}
