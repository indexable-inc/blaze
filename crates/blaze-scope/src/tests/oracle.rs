//! The cold-fill oracle (bottom.md section 6, scope lead 20, oracle 3): generated small crates
//! solved by the solver and by a slow reference written here in the plainest way, compared name
//! by name, import by import and call by call.
//!
//! The reference shares no code with the solver: it reads the test world's declarations, keeps
//! one state per import and per macro call, and repeats "resolve every import and call the
//! current facts decide" over the whole crate until nothing changes. A name is decided by the
//! wait relation of the stall rule (bottom.md section 6): unbound in a module, it waits on the
//! module's pending named imports of it, its pending globs and its calls; bound only through
//! globs, on the named imports and (outside the macro namespace) the calls; never on a glob once
//! bound. Through glob edges it is the least fixpoint of the edges' values. When nothing moves,
//! the items in sink components of the wait graph fail as not found, and the loop goes on.

use std::boxed::Box;
use std::collections::{BTreeMap, BTreeSet};
use std::string::{String, ToString};
use std::vec::Vec;
use std::{format, vec};

use blaze_id::{CrateItemKey, ItemKey};
use blaze_sim::{Broke, Config, Tape};

use super::{N, Solved, V, World, solve, solve_planted};
use crate::cells::CellPlant as Plant;
use crate::{
    CallShape, Def, DefKind, Edition, Namespace, PathKind, Rename, ScopeError, Unresolved, UseForm, Visibility,
    WrittenVis,
};

const NSS: [Namespace; 3] = [Namespace::Types, Namespace::Values, Namespace::Macros];
const ITEMS: [N; 3] = ["x", "y", "z"];
const TARGETS: [N; 3] = ["p", "q", "r"];
const MACROS: [N; 3] = ["mx", "my", "mz"];
const MODS: [N; 6] = ["m0", "m1", "m2", "m3", "m4", "m5"];

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

// ---------------------------------------------------------------------------------------------
// The generator.

/// One declaration of a module's text, in text order.
enum Ev {
    Item { name: N, ns: Namespace, vis: V },
    Use { path: &'static str, to: Option<N>, vis: V },
    Glob { path: &'static str, vis: V },
    Rules { name: N, export: bool },
    /// A call `name!()`; its expansion is empty, or holds `mod` of the child named, and (the
    /// wide distribution) an item; (the deep distribution) `body`, more of the expansion's text:
    /// imports, items, `extern crate`, and calls, which nest.
    Call { name: N, child: Option<usize>, item: Option<(N, Namespace)>, body: Vec<Ev> },
    Mod { child: usize },
    /// `extern crate e3 as name;` (the deep distribution, in a root call's expansion only).
    Extern { name: N },
}

/// The deep distribution's second dependency: crate 3, named `e3` (`pub struct y; pub mod m2 {
/// pub struct x; }`), which a macro-written `extern crate e3 as ..;` names.
const E3: N = "e3";

/// A built-in type name (R9): the deep distribution binds it to a module or writes a struct of it.
const BUILTIN: N = "str";

struct PlanMod {
    parent: Option<usize>,
    name: N,
    vis: V,
    macro_use: bool,
    evs: Vec<Ev>,
}

fn pick<T: Copy>(tape: &Tape, from: &[T], label: &'static str) -> T {
    *tape.one_of(from, label)
}

/// The path of names from the root down to `m` (`m` last), as `crate::a::b`.
fn crate_path(plan: &[PlanMod], m: usize) -> String {
    let mut names = Vec::new();
    let mut at = m;
    while let Some(p) = plan[at].parent {
        names.push(plan[at].name);
        at = p;
    }
    names.reverse();
    let mut s = String::from("crate");
    for n in names {
        s.push_str("::");
        s.push_str(n);
    }
    s
}

/// A visibility written in module `at`'s text: private, `pub`, `pub(crate)`, `pub(super)` (when
/// `at` has a parent) or `pub(in path)` naming one of `at`'s ancestors or `at` itself (the
/// Reference, "Visibility and privacy": the path "must refer to an ancestor module"). The first
/// three keep their old tape values.
fn any_vis(tape: &Tape, plan: &[PlanMod], at: usize, label: &'static str) -> V {
    let anc = ancestors(plan, at);
    match tape.below(5, label) {
        0 => V::Priv,
        1 => V::Pub,
        2 => V::Crate,
        3 if anc.len() > 1 => V::Super,
        _ => V::In(leak(crate_path(plan, anc[tape.below(anc.len() as u64, "in which") as usize]))),
    }
}

fn ancestors(plan: &[PlanMod], m: usize) -> Vec<usize> {
    let mut out = vec![m];
    let mut at = m;
    while let Some(p) = plan[at].parent {
        out.push(p);
        at = p;
    }
    out
}

/// A drawn crate: a tree of up to six modules; items in two namespaces; named imports, renames
/// and globs by `crate::`, `self::` and `super::` paths (so re-export chains and glob cycles);
/// `macro_rules!` with `#[macro_use]` modules and `#[macro_export]`; calls of them, some
/// declaring a module in their expansion; every visibility.
fn draw(tape: &Tape) -> (World, ItemKey) {
    draw_in(tape, false, true)
}

/// The seam a solver core is judged by: a drawn world (`calls`: false draws no macro call at
/// all, for a core without expansion) and the reference's outcome for every import of it, by
/// (module, index in the module's text).
pub(crate) fn reference_world(tape: &Tape, wide: bool, calls: bool) -> (World, ItemKey) {
    draw_in(tape, wide, calls)
}

/// The reference's outcome for every import of `w` (crate root `root`) it decided.
pub(crate) fn reference_imports(w: &World, root: ItemKey) -> BTreeMap<(ItemKey, u32), Imp> {
    let mut r = Ref::new(w, root);
    r.run();
    r.imports
}

/// The names a plain path's first segment is drawn from (the wide distribution): `m1` is also
/// the dependency's extern-prelude name and in the standard prelude, `m3` in the standard
/// prelude, so a local module of either name meets an outer candidate (R6, R7, R8).
const FIRSTS: [N; 4] = ["m1", "m3", "m0", "m2"];

/// `draw`, or with `wide` the wide distribution: also a dependency (crate 2, extern-prelude
/// name `m1`, `pub struct x; struct z; pub mod m2 { pub struct y; }`), a standard prelude
/// (crate 9's `pub mod prelude { pub mod m1 { pub struct y; } pub mod m3 { pub struct x; } }`),
/// imports by plain paths (a lexical first segment), and `mk!` expansions writing an item.
fn draw_in(tape: &Tape, wide: bool, calls: bool) -> (World, ItemKey) {
    draw_dist(tape, wide, calls, false)
}

/// The deep distribution: the wide one, and macros that write what real crates' macros write:
/// `use` declarations (named and glob), items, further `mk!` calls (nested two and three deep,
/// a child module's call too, so an expansion can enclose the import that reads its module),
/// a root `extern crate e3 as m0;` (adds an extern-prelude name, R8) or, in a one-module crate,
/// `as m1` (would replace the dependency's: the error), and a built-in type name `str` bound
/// to a module or written as a struct (R9). Its draws come after every wide draw of a module, so
/// the wide and no-calls tapes replay unchanged.
pub(crate) fn draw_deep(tape: &Tape) -> (World, ItemKey) {
    draw_dist(tape, true, true, true)
}

/// One import of module `i`'s text as the generator draws it, or `None` when the draw is
/// dropped (a `self::*` glob; a name the module declares or binds already).
#[allow(clippy::too_many_arguments)]
fn draw_import(
    tape: &Tape,
    plan: &[PlanMod],
    i: usize,
    n: usize,
    wide: bool,
    items: &BTreeSet<(N, u8)>,
    targets: &mut BTreeSet<N>,
    exported: &BTreeSet<N>,
) -> Option<Ev> {
    let start = match tape.below(if wide { 4 } else { 3 }, "path start") {
        0 => String::from("crate"),
        1 => String::from("self"),
        3 => String::from(pick(tape, &FIRSTS, "first segment")),
        _ => match plan[i].parent {
            Some(_) => String::from("super"),
            None => String::from("crate"),
        },
    };
    let mut path = start;
    for _ in 0..tape.below(3, "middle") {
        path.push_str("::");
        path.push_str(pick(tape, &MODS[..n], "middle seg"));
    }
    // Every visibility, `pub(super)` and `pub(in path)` too (R4: what a restricted
    // re-export lets a glob or a named import of it see).
    let vis = any_vis(tape, plan, i, "use vis");
    if tape.below(3, "glob") == 0 {
        if path == "self" {
            return None;
        }
        return Some(Ev::Glob { path: leak(path), vis });
    }
    let leaf = match tape.below(4, "leaf kind") {
        0 => pick(tape, &TARGETS, "leaf target"),
        1 => pick(tape, &MODS[..n], "leaf mod"),
        2 => pick(tape, &MACROS[..2], "leaf macro"),
        _ => pick(tape, &ITEMS, "leaf item"),
    };
    path.push_str("::");
    path.push_str(leaf);
    let to = if tape.bool("rename") { pick(tape, &TARGETS, "to") } else { leaf };
    // One binding per name: no import names what the module declares or another binds.
    let declared = ITEMS.contains(&to) && items.iter().any(|(nm, _)| *nm == to) || MODS.contains(&to) || exported.contains(&to) && i == 0;
    if declared || !targets.insert(to) {
        return None;
    }
    Some(Ev::Use { path: leak(path), to: (to != leaf).then_some(to), vis })
}

/// The deep distribution's text of one `mk!` expansion in module `i`, `depth` calls deep:
/// imports, an item, at the root an `extern crate`, and a nested `mk!` (to three deep).
#[allow(clippy::too_many_arguments)]
fn draw_body(
    tape: &Tape,
    plan: &[PlanMod],
    i: usize,
    n: usize,
    items: &mut BTreeSet<(N, u8)>,
    targets: &mut BTreeSet<N>,
    exported: &BTreeSet<N>,
    externs: &mut BTreeSet<N>,
    depth: u32,
) -> Vec<Ev> {
    let mut body = Vec::new();
    for _ in 0..tape.below(3, "mk uses") {
        // At the root no import binds a macro name: an exported macro a later module draws is a
        // root binding too (the module text's imports are filtered for it after the draw).
        if let Some(e) = draw_import(tape, plan, i, n, true, items, targets, exported) {
            let binds_macro = matches!(e, Ev::Use { path, to, .. } if MACROS.contains(&to.unwrap_or(path.rsplit("::").next().unwrap_or(""))));
            if !(i == 0 && binds_macro) {
                body.push(e);
            }
        }
    }
    if tape.bool("mk item") {
        let name = pick(tape, &ITEMS, "mk item name");
        let (ns, nsb) = if tape.bool("mk item value") { (Namespace::Values, 1) } else { (Namespace::Types, 0) };
        if !items.iter().any(|(nm, _)| *nm == name) && !targets.contains(&name) {
            items.insert((name, nsb));
            body.push(Ev::Item { name, ns, vis: V::Pub });
        }
    }
    // R8: a root `extern crate` adds `m0` (no dependency's name); in a one-module crate (no root
    // `mod m1` to clash with) it may name the dependency's `m1`: never a replacement, an error.
    if i == 0 && tape.below(4, "mk extern crate") == 0 {
        let name = if n == 1 && tape.bool("extern replaces") { "m1" } else { "m0" };
        if externs.insert(name) {
            body.push(Ev::Extern { name });
        }
    }
    if depth < 3 && tape.below(3, "nested mk") == 0 {
        let inner = draw_body(tape, plan, i, n, items, targets, exported, externs, depth + 1);
        body.push(Ev::Call { name: "mk", child: None, item: None, body: inner });
    }
    body
}

/// Writes `e` into `at` (a module or a call's expansion), recursively for a call's body.
fn emit(w: &mut World, at: ItemKey, e: &Ev, plan: &[PlanMod], keys: &mut [Option<ItemKey>]) {
    match *e {
        Ev::Item { name, ns, vis } => {
            w.item(at, name, ns, DefKind::Other, vis);
        }
        Ev::Use { path, to: Some(to), vis } => w.use_as(at, path, to, vis),
        Ev::Use { path, to: None, vis } => w.use_(at, path, vis),
        Ev::Glob { path, vis } => w.glob(at, path, vis),
        Ev::Rules { name, export } => {
            let def = w.rules_with(at, name, export);
            if export {
                let root = keys[0].expect("the root");
                w.cr(root).macro_exports.push((name, def, crate::MacroShapes::FN_LIKE));
            }
        }
        Ev::Call { name, child, item, ref body } => {
            let call = w.call(at, name, CallShape::FnLike);
            if let Some(c) = child {
                keys[c] = Some(w.module_with(call, plan[c].name, plan[c].vis, false, false));
            }
            if let Some((name, ns)) = item {
                w.item(call, name, ns, DefKind::Other, V::Pub);
            }
            for b in body.iter() {
                emit(w, call, b, plan, keys);
            }
        }
        Ev::Mod { child } => {
            keys[child] = Some(w.module_with(at, plan[child].name, plan[child].vis, plan[child].macro_use, false));
        }
        Ev::Extern { name } => w.extern_crate(at, crate::ExternTarget::Dep(E3), Rename::To(name), V::Priv),
    }
}

fn draw_dist(tape: &Tape, wide: bool, calls: bool, deep: bool) -> (World, ItemKey) {
    let n = 1 + tape.below(MODS.len() as u64, "modules") as usize;
    let mut plan: Vec<PlanMod> = Vec::new();
    for i in 0..n {
        let parent = if i == 0 { None } else { Some(tape.below(i as u64, "parent") as usize) };
        // A module's visibility is written in its parent's text: restricted relative to it.
        let vis = match parent {
            Some(p) => any_vis(tape, &plan, p, "mod vis"),
            // The root's draw is kept (unused) so tapes written before replay the same.
            None => {
                pick(tape, &[V::Priv, V::Pub, V::Crate], "mod vis");
                V::Priv
            }
        };
        plan.push(PlanMod { parent, name: MODS[i], vis, macro_use: tape.bool("macro_use"), evs: Vec::new() });
    }
    let mut exported: BTreeSet<N> = BTreeSet::new();
    let mut externs: BTreeSet<N> = BTreeSet::new();
    for i in 0..n {
        let mut evs = Vec::new();
        // Each child is declared once, in its parent's text or in a call's expansion there.
        for c in (i + 1)..n {
            if plan[c].parent == Some(i) {
                if calls && tape.below(4, "in a call") == 0 {
                    // `mk!`, defined first in the root: the module always comes to be (the
                    // tests' driver solves every module it is given).
                    evs.push(Ev::Call { name: "mk", child: Some(c), item: None, body: Vec::new() });
                } else {
                    evs.push(Ev::Mod { child: c });
                }
            }
        }
        let mut items: BTreeSet<(N, u8)> = BTreeSet::new();
        for _ in 0..tape.below(4, "items") {
            let name = pick(tape, &ITEMS, "item name");
            let (ns, nsb) = if tape.bool("value") { (Namespace::Values, 1) } else { (Namespace::Types, 0) };
            if !items.insert((name, nsb)) {
                continue;
            }
            let vis = any_vis(tape, &plan, i, "item vis");
            evs.push(Ev::Item { name, ns, vis });
        }
        let mut targets: BTreeSet<N> = BTreeSet::new();
        for _ in 0..tape.below(4, "imports") {
            if let Some(e) = draw_import(tape, &plan, i, n, wide, &items, &mut targets, &exported) {
                evs.push(e);
            }
        }
        for _ in 0..tape.below(3, "rules") {
            let name = pick(tape, &MACROS[..2], "rules name");
            let export = tape.below(3, "export") == 0 && !exported.contains(&name) && !targets.contains(&name);
            if export {
                exported.insert(name);
            }
            evs.push(Ev::Rules { name, export });
        }
        for _ in 0..if calls { tape.below(3, "calls") } else { 0 } {
            evs.push(Ev::Call { name: pick(tape, &MACROS, "call name"), child: None, item: None, body: Vec::new() });
        }
        if wide && calls && tape.bool("mk writes an item") {
            let name = pick(tape, &ITEMS, "expanded item");
            let ns = if tape.bool("expanded value") { Namespace::Values } else { Namespace::Types };
            if !items.iter().any(|(nm, _)| *nm == name) && !targets.contains(&name) {
                evs.push(Ev::Call { name: "mk", child: None, item: Some((name, ns)), body: Vec::new() });
            }
        }
        if deep {
            // The wide draw's `mk!` item is the module's too: no deep item repeats its name
            // (deep tape '4 0*8 1 0*12 1 1 0 1 0*5 1 1': two `pub type y`, a duplicate).
            for e in evs.iter() {
                if let Ev::Call { item: Some((name, ns)), .. } = e {
                    items.insert((name, if *ns == Namespace::Values { 1 } else { 0 }));
                }
            }
            // A child's own `mk!` writes more beside its `mod` (the expansion then encloses the
            // imports it writes there: R6's inner wins), and may itself be written by a `mk!`.
            for e in evs.iter_mut() {
                let Ev::Call { child: Some(_), body, .. } = e else { continue };
                if tape.bool("child mk writes more") {
                    *body = draw_body(tape, &plan, i, n, &mut items, &mut targets, &exported, &mut externs, 1);
                }
            }
            for e in evs.iter_mut() {
                if matches!(e, Ev::Call { child: Some(_), .. }) && tape.bool("nest child mk") {
                    let inner = std::mem::replace(e, Ev::Mod { child: 0 });
                    *e = Ev::Call { name: "mk", child: None, item: None, body: vec![inner] };
                }
            }
            for _ in 0..tape.below(3, "deep calls") {
                let body = draw_body(tape, &plan, i, n, &mut items, &mut targets, &exported, &mut externs, 1);
                evs.push(Ev::Call { name: "mk", child: None, item: None, body });
            }
            // R9: `str` bound to a module, or a struct of the name.
            match tape.below(3, "builtin name") {
                1 if targets.insert(BUILTIN) => {
                    let path = leak(format!("crate::{}", pick(tape, &MODS[..n], "builtin module")));
                    evs.push(Ev::Use { path, to: Some(BUILTIN), vis: any_vis(tape, &plan, i, "builtin vis") });
                }
                2 if targets.insert(BUILTIN) => evs.push(Ev::Item { name: BUILTIN, ns: Namespace::Types, vis: V::Pub }),
                _ => {}
            }
        }
        // Text order: a drawn permutation.
        let mut ordered = Vec::new();
        while !evs.is_empty() {
            let at = tape.below(evs.len() as u64, "order") as usize;
            ordered.push(evs.remove(at));
        }
        if i == 0 {
            ordered.insert(0, Ev::Rules { name: "mk", export: false });
        }
        plan[i].evs = ordered;
    }
    // Exported macros are root bindings: an import at the root that would bind one is dropped.
    let root_targets_clash = |e: &Ev| match e {
        Ev::Use { path, to, .. } => {
            let leaf = path.rsplit("::").next().unwrap_or("");
            exported.contains(&to.unwrap_or(leaf))
        }
        _ => false,
    };
    plan[0].evs.retain(|e| !root_targets_clash(e));

    let mut w = World::new();
    // Keys in an order unrelated to the tree (drawn last, so earlier tapes replay unchanged).
    w.key_mix = tape.below(64, "key order") as u16;
    let mut keys: Vec<Option<ItemKey>> = vec![None; n];
    keys[0] = Some(w.root(1, Edition::E2021));
    if wide {
        let root = keys[0].expect("the root");
        let dep = w.root(2, Edition::E2021);
        w.item(dep, "x", Namespace::Types, DefKind::Other, V::Pub);
        w.item(dep, "z", Namespace::Types, DefKind::Other, V::Priv);
        let m2 = w.module(dep, "m2", V::Pub);
        w.item(m2, "y", Namespace::Types, DefKind::Other, V::Pub);
        w.dep(root, "m1", dep);
        let std = w.root(9, Edition::E2021);
        let prelude = w.module(std, "prelude", V::Pub);
        let p1 = w.module(prelude, "m1", V::Pub);
        w.item(p1, "y", Namespace::Types, DefKind::Other, V::Pub);
        let p3 = w.module(prelude, "m3", V::Pub);
        w.item(p3, "x", Namespace::Types, DefKind::Other, V::Pub);
        w.cr(root).head.prelude = Some(prelude);
    }
    if deep {
        let root = keys[0].expect("the root");
        let e3 = w.root(3, Edition::E2021);
        w.item(e3, "y", Namespace::Types, DefKind::Other, V::Pub);
        let m2 = w.module(e3, "m2", V::Pub);
        w.item(m2, "x", Namespace::Types, DefKind::Other, V::Pub);
        w.dep(root, E3, e3);
    }
    for i in 0..n {
        let at = keys[i].expect("a parent is emitted before its children");
        for e in plan[i].evs.iter() {
            emit(&mut w, at, e, &plan, &mut keys);
        }
    }
    (w, keys[0].expect("the root"))
}

// ---------------------------------------------------------------------------------------------
// The reference.

/// A binding as the reference has it: `amb` when two globs bring different items.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct B {
    pub(crate) def: Def,
    pub(crate) vis: Visibility,
    pub(crate) amb: bool,
}


/// The reference's outcome of one import.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Imp {
    Pending,
    Named([Option<B>; 3]),
    Glob(ItemKey),
    Failed(Unresolved),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Call {
    Pending,
    Done,
    Failed(Unresolved),
}

/// A textual-scope slot of a module's text.
#[derive(Clone, Copy)]
enum Slot {
    Rules(N, ItemKey),
    Mod(ItemKey, bool),
    Call(ItemKey),
}

/// A pending item an undecided answer waits on: an import (named or glob) by its module and
/// index, or a call.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Bk {
    Imp(ItemKey, u32),
    Call(ItemKey),
}

/// A lookup's answer: bound, absent, or undecided with the pending items it waits on.
type Look = Result<Option<B>, Vec<Bk>>;

struct Ref<'w> {
    w: &'w World,
    root: ItemKey,
    krate: CrateItemKey,
    imports: BTreeMap<(ItemKey, u32), Imp>,
    /// What a pending named import has found so far, per namespace: a binding found in one
    /// namespace is a fact while the others are undecided.
    partial: BTreeMap<(ItemKey, u32), [Option<B>; 3]>,
    calls: BTreeMap<ItemKey, Call>,
    /// The members of a stalled group left undecided in a round that spared a call (R5): what
    /// they read was still to be written by that call's expansion (r5e, r5f).
    behind_spared: BTreeSet<Bk>,
    /// The imports a stall resolved through a later segment's undecided lookup taken as absent
    /// (R5; KNOWN r5g).
    middle_stalled: BTreeSet<Bk>,
    /// A rule broken on purpose, to show the comparison catches it (the testing skill's
    /// planted-bug control); `None` in every real run.
    bug: Option<Bug>,
}

/// One planted bug per rule of bottom.md section 6 that the reference states.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Bug {
    /// R1: a named import loses to a glob (`own` skips resolved named imports).
    R1NamedLosesToGlob,
    /// R2: two globs bringing different items are not ambiguous.
    R2GlobsNeverClash,
    /// R2: a name bound through a decided glob stops waiting on the module's pending globs, so a
    /// pending glob that would bring another item is never met (the reference's own bug until
    /// the deep tape of r2e: the answer hung on the order of work).
    R2BoundStopsWaiting,
    /// R3: one pass over the glob edges instead of the least fixed point.
    R3OnePass,
    /// R4: a glob brings names the importer may not see.
    R4GlobIgnoresPrivacy,
    /// R6: a lexical first segment waits for the module's macro calls instead of going out.
    R6WaitsForExpansion,
    /// R7: a glob candidate beats an outer one instead of being ambiguous.
    R7GlobBeatsOuter,
    /// R8: the standard prelude is asked before the extern prelude.
    R8StdBeforeExtern,
    /// R5: at a quiet point the self-waiting groups are not failed: the run stops undecided.
    R5NoStall,
    /// R5: the textual refinement spares every call of a sink, not only one whose textual read
    /// waits on a call of that sink; a sink of calls alone is then never failed.
    R5SparesAll,
    /// R5: a stalled try fails a later path segment whose lookup is undecided, instead of taking
    /// the undecided as absent (the reference's own bug until the cell core lane's seed 1983).
    R5StallFailsMiddle,
    /// Step 1: an import an expansion writes is not the module's (only its own text's are).
    ExpansionImportsIgnored,
    /// Step 2: a call an expansion writes is never walked: what it writes never stands.
    NestedNotWalked,
    /// Step 3, R6: enclosure is ignored: a definition written by the expansion that encloses
    /// the use is an error too.
    R6IgnoresEnclosure,
    /// R8: a macro-written root `extern crate` replaces a dependency's extern-prelude name.
    R8MacroReplaces,
    /// A `pub use` of an `extern crate` item's binding is narrowed to the item's visibility (the
    /// reference's own answer until the deep tape `5 0 0 0 0 0 0 0 1 1 ..`).
    ExternReexportNarrowed,
    /// R6: a definition standing in the module's own text does not stop the finding (the
    /// reference's own bug until the deep tape `1 0 0 0 0 0 1 1 ..`).
    R6IgnoresStanding,
    /// R9: a one-segment type path whose lexical binding is a module names the module.
    R9ModuleIsTheType,
}

fn ns_at(ns: Namespace) -> usize {
    match ns {
        Namespace::Types => 0,
        Namespace::Values => 1,
        Namespace::Macros => 2,
    }
}

impl<'w> Ref<'w> {
    fn new(w: &'w World, root: ItemKey) -> Ref<'w> {
        Ref { w, root, krate: w.modules[&root].krate, imports: BTreeMap::new(), partial: BTreeMap::new(), calls: BTreeMap::new(), behind_spared: BTreeSet::new(), middle_stalled: BTreeSet::new(), bug: None }
    }

    /// The outer candidate of a lexical first segment `s0` (R6, R8): the extern prelude, then
    /// the standard prelude's own modules and type items (it holds no imports here).
    fn outer(&self, s0: N) -> Option<B> {
        let cr = &self.w.roots[&self.krate];
        let dep = cr.extern_prelude.iter().find(|(n, _)| *n == s0).map(|&(_, k)| k);
        let ext = if self.bug == Some(Bug::R8MacroReplaces) { self.extern_added(s0).or(dep) } else { dep.or_else(|| self.extern_added(s0)) };
        let ext = ext.map(|k| B { def: Def::module(k), vis: Visibility::Public, amb: false });
        let std = cr.head.prelude.and_then(|p| {
            let d = &self.w.decls[&p];
            let m = d.mods.iter().find(|c| c.name == s0).map(|c| Def::module(c.module));
            let it = d.items.iter().find(|it| it.ns == Namespace::Types && it.name == s0).map(|it| it.def);
            m.or(it).map(|def| B { def, vis: Visibility::Public, amb: false })
        });
        if self.bug == Some(Bug::R8StdBeforeExtern) { std.or(ext) } else { ext.or(std) }
    }

    /// R9: the one-segment path `name` in TYPE position at `m` (`fn f(_: &str)`): its lexical
    /// binding, unless that is a module (a module is never a type), then a type of the preludes,
    /// then the built-in type of the name; a struct of the name shadows the built-in. `None`
    /// while undecided or ambiguous. As a path prefix (`str::x`) the module stands: that is
    /// `lookup`, which the import comparison reads.
    fn type_path(&self, m: ItemKey, name: N) -> Option<Def> {
        let inner = self.lookup(m, Namespace::Types, name, None, m).ok()?;
        if let Some(b) = inner {
            if b.amb {
                return None;
            }
            if b.def.kind != DefKind::Module || self.bug == Some(Bug::R9ModuleIsTheType) {
                return Some(b.def);
            }
        }
        self.outer(name).filter(|o| o.def.kind != DefKind::Module).map(|o| o.def).or_else(|| crate::builtin_type(name))
    }

    /// The root of the dependency an `extern crate` names.
    fn extern_target(&self, t: crate::ExternTarget<N>) -> Option<ItemKey> {
        match t {
            crate::ExternTarget::Dep(d) => self.w.roots[&self.krate].deps.iter().find(|x| x.name == d).map(|x| x.root),
            crate::ExternTarget::SelfCrate => Some(self.root),
        }
    }

    /// R8: the extern-prelude name `s0` an `extern crate .. as s0;` written in the crate root's
    /// text or an expanded call there adds (`names.preludes.extern`). It never replaces a
    /// dependency's name: that one stays and the `extern crate` is an error (D56; planted
    /// `R8MacroReplaces`).
    /// The name an `extern crate` item binds in its module: `as name`, else the crate's own
    /// name; `as _` and a bare `extern crate self;` bind none.
    fn extern_binds(target: crate::ExternTarget<N>, rename: Rename<N>) -> Option<N> {
        match (rename, target) {
            (Rename::To(n), _) => Some(n),
            (Rename::Same, crate::ExternTarget::Dep(d)) => Some(d),
            (Rename::Same, crate::ExternTarget::SelfCrate) | (Rename::Underscore, _) => None,
        }
    }

    /// Whether `name` in the type namespace of `m` is bound by an `extern crate` item of crate
    /// `def`.
    fn extern_item(&self, m: ItemKey, name: N, def: Def) -> bool {
        self.frags(m).iter().any(|&f| {
            self.w.decls[&f].extern_crates.iter().any(|ec| Ref::extern_binds(ec.target, ec.rename) == Some(name) && self.extern_target(ec.target) == Some(def.key))
        })
    }

    fn extern_added(&self, s0: N) -> Option<ItemKey> {
        for f in self.frags(self.root) {
            for ec in self.w.decls[&f].extern_crates.iter() {
                if ec.rename == Rename::To(s0) {
                    return self.extern_target(ec.target);
                }
            }
        }
        None
    }

    /// The `extern crate`s at the root that name a dependency's extern-prelude name: R8's error.
    fn extern_replacing(&self) -> Vec<N> {
        let cr = &self.w.roots[&self.krate];
        let mut out = Vec::new();
        for f in self.frags(self.root) {
            for ec in self.w.decls[&f].extern_crates.iter() {
                if let Rename::To(n) = ec.rename
                    && cr.extern_prelude.iter().any(|(d, _)| *d == n)
                {
                    out.push(n);
                }
            }
        }
        out
    }

    /// A plain path's first segment `s0` at `m`, asked by import `me` (R6, R7): `Err` while it
    /// waits; else the binding or why it failed.
    /// `stall`: the import is failed in a sink of the wait graph (R5), so what is undecided in
    /// the inner scope is taken as absent and the outer candidate, if any, is used.
    fn first_plain(&self, m: ItemKey, s0: N, me: Option<Bk>, stall: bool) -> Result<Result<B, Unresolved>, Vec<Bk>> {
        let outer = self.outer(s0);
        // An item, a child module or a resolved named import shadows the outer candidate; a
        // pending named import of `s0` is waited for (R7, `...imports.shadowing`).
        let (own, named) = self.own(m, Namespace::Types, s0, me, m);
        if let Some(b) = own {
            return Ok(Ok(b));
        }
        if !named.is_empty() && !stall {
            return Err(named);
        }
        let calls = if stall { Vec::new() } else { self.calls_of(m, me) };
        // Globs: a pending one (or a source still open) is waited for (R7).
        // Stalled, the undecided is taken as absent: the decided globs' value stands (bound
        // through one, the lookup still waits on pending globs, R2), else it goes outward.
        let inner = match self.lookup_in(m, Namespace::Types, s0, me, m, false) {
            Err(_) if stall => Ok(self.lookup_parts(m, Namespace::Types, s0, me, m, false).0),
            r => r,
        };
        match inner {
            Err(wait) => Err(wait),
            Ok(Some(b)) => match outer {
                // A glob candidate beside a DIFFERENT outer one: `...ambiguity.glob-vs-outer`
                // ("another candidate"). The same definition both ways is one candidate (D57:
                // `...ambiguity.glob-vs-glob`'s "the same item (following reexports)" across
                // scopes; either order resolves to the same definition).
                Some(o) if o.def != b.def && self.bug != Some(Bug::R7GlobBeatsOuter) => {
                    Ok(Err(Unresolved::Ambiguous(crate::AmbiguityKind::GlobVsOuter)))
                }
                _ if !calls.is_empty() => Err(calls),
                _ => Ok(Ok(b)),
            },
            Ok(None) => match outer {
                // Growth by expansion only does not hold the lookup (R6, D56).
                Some(_) if self.bug == Some(Bug::R6WaitsForExpansion) && !calls.is_empty() => Err(calls),
                Some(o) => Ok(Ok(o)),
                None if !calls.is_empty() => Err(calls),
                // No candidate yet, and the extern prelude can still grow: a root call may
                // write `extern crate .. as s0;` (R6 for the extern prelude, R8).
                None if !stall && !self.calls_of(self.root, me).is_empty() => Err(self.calls_of(self.root, me)),
                None => Ok(Err(Unresolved::NotFound)),
            },
        }
    }

    /// R6's error: import `i` of fragment `f` of `m` whose plain first segment `s0` went
    /// outward, and an expansion standing in `m` that does NOT enclose `f` wrote `s0` into `m`
    /// after: a `mod`, a type item, an `extern crate`, or an IMPORT (named, or a glob bringing
    /// `s0`: an import an expansion writes is the module's growth until the call expands). The
    /// rule: an ambiguity at the new definition, in every order; written by an enclosing
    /// expansion, it existed before the use did, and the inner wins (D56). A candidate that is
    /// the outer definition itself is no other candidate (D57). (module, fragment, index, s0).
    fn r6_findings(&self) -> Vec<(ItemKey, ItemKey, u32, N)> {
        let mut out = Vec::new();
        for m in self.live() {
            let frags = self.frags(m);
            for (f, i, u) in self.uses_of(m) {
                if u.path.kind != PathKind::Plain {
                    continue;
                }
                let s0 = u.path.segments[0];
                // Whatever the import's outcome in this run's order: in another order it went
                // outward first (wide seed 196: resolved through the expansion's `m1` here,
                // failing as private; outward in the solver's order, resolving).
                let Some(outer) = self.outer(s0) else { continue };
                let other = |b: Option<B>| b.is_some_and(|b| b.def != outer.def);
                // R6's precondition (bottom.md section 6): "A path's first segment `n` that
                // misses in the inner scope goes outward". A `mod`, a type item or an `extern
                // crate` of the module's own text, or of an expansion enclosing the use, stood
                // in the inner scope before the use did: the lookup never missed, in no order,
                // so nothing an expansion writes later is R6's error (the deep tape `1 0 0 0 0
                // 0 1 1 ..`: `mod m1 {}` beside the extern prelude's `m1`, and `mk! { use
                // crate::*; }` bringing that same `m1`; the Reference,
                // `names.resolution.expansion.imports.shadowing`: an outer name is "shadowed by
                // candidates in the same namespace with the same name from an inner scope").
                // An `extern crate` binds in both forms (Reference `items.extern-crate`: "bound
                // into the declaring scope as the identifier provided").
                let stood = frags.iter().filter(|&&g| g == m || self.encloses(g, f)).any(|&g| {
                    let e = &self.w.decls[&g];
                    e.mods.iter().any(|d| d.name == s0)
                        || e.items.iter().any(|it| it.ns == Namespace::Types && it.name == s0)
                        || e.extern_crates.iter().any(|ec| ec.rename == Rename::To(s0) || (ec.rename == Rename::Same && ec.target == crate::ExternTarget::Dep(s0)))
                });
                if stood && self.bug != Some(Bug::R6IgnoresStanding) {
                    continue;
                }
                let by_expansion = frags.iter().filter(|&&g| g != m && !self.encloses(g, f)).any(|&g| {
                    let e = &self.w.decls[&g];
                    let defined = e.mods.iter().any(|d| d.name == s0)
                        || e.items.iter().any(|it| it.ns == Namespace::Types && it.name == s0)
                        || e.extern_crates.iter().any(|ec| ec.rename == Rename::To(s0));
                    let imported = e.uses.iter().enumerate().any(|(j, v)| match (v.leaf, self.imports.get(&(g, j as u32))) {
                        (UseForm::Named { rename, .. }, Some(Imp::Named(bs))) => {
                            let to = match rename {
                                Rename::To(t) => t,
                                _ => *v.path.segments.last().expect("a leaf"),
                            };
                            to == s0 && other(bs[0])
                        }
                        (UseForm::Glob, Some(&Imp::Glob(src))) => {
                            other(self.lookup_parts(src, Namespace::Types, s0, None, m, true).0.filter(|b| self.visible(b.vis, m)))
                        }
                        _ => false,
                    });
                    defined || imported
                });
                if by_expansion {
                    out.push((m, f, i, s0));
                }
            }
        }
        out
    }

    fn parent(&self, m: ItemKey) -> Option<ItemKey> {
        self.w.modules[&m].parent
    }

    /// The module a fragment (a module's own text, or a call's expansion) stands in.
    fn module_of(&self, f: ItemKey) -> ItemKey {
        self.w.owner.get(&f).copied().unwrap_or(f)
    }

    /// The fragments of `m`'s text that exist: its own text, then the expansion of every call
    /// written in one of them that has expanded (a call an expansion writes stands in `m` too).
    /// An import, item or call of one is `m`'s from the moment its call expanded.
    fn frags(&self, m: ItemKey) -> Vec<ItemKey> {
        let mut out = vec![m];
        let mut at = 0;
        while at < out.len() {
            let f = out[at];
            for c in self.w.decls[&f].calls.iter() {
                if self.calls.get(&c.call) == Some(&Call::Done) && (f == m || self.bug != Some(Bug::NestedNotWalked)) {
                    out.push(c.call);
                }
            }
            at += 1;
        }
        out
    }

    /// The fragment call `c` is written in.
    fn written_in(&self, c: ItemKey) -> ItemKey {
        self.w.decls.iter().find(|(_, d)| d.calls.iter().any(|x| x.call == c)).map(|(&f, _)| f).expect("a call is written somewhere")
    }

    /// Whether the expansion of call `e` encloses fragment `f`: `f` is `e` or was written,
    /// at any depth, by `e`'s expansion (R6's line).
    fn encloses(&self, e: ItemKey, f: ItemKey) -> bool {
        let mut at = f;
        loop {
            if at == e {
                return self.bug != Some(Bug::R6IgnoresEnclosure);
            }
            if !self.w.owner.contains_key(&at) {
                return false;
            }
            at = self.written_in(at);
        }
    }

    /// Where `mod m` is written: the fragment of its parent and the position in it.
    fn mod_site(&self, m: ItemKey) -> (ItemKey, u32) {
        let p = self.parent(m).expect("not the root");
        for f in self.frags(p) {
            if let Some(c) = self.w.decls[&f].mods.iter().find(|c| c.module == m) {
                return (f, c.pos);
            }
        }
        unreachable!("a live module is declared in a live fragment of its parent")
    }

    fn chain(&self, m: ItemKey) -> Vec<ItemKey> {
        let mut out = vec![m];
        while let Some(p) = self.parent(*out.last().expect("not empty")) {
            out.push(p);
        }
        out
    }

    /// The modules that exist: the root, every `mod` of a live module's text, and every `mod`
    /// of the expansion of a call of a live module that resolved.
    fn live(&self) -> Vec<ItemKey> {
        let mut out = vec![self.root];
        let mut at = 0;
        while at < out.len() {
            let m = out[at];
            out.extend(self.children(m).into_iter().map(|c| c.1));
            at += 1;
        }
        out
    }

    /// The module `crate::a::b` of a `pub(in ..)`, by child names alone.
    fn in_path(&self, segments: &[N]) -> Option<ItemKey> {
        let mut at = self.root;
        for s in segments {
            at = self.children(at).into_iter().find(|(n, _, _)| n == s)?.1;
        }
        Some(at)
    }

    /// Every `mod` declared in `m`, in its text or in a resolved call's expansion: name, key,
    /// written visibility.
    fn children(&self, m: ItemKey) -> Vec<(N, ItemKey, WrittenVis<crate::Path<'static, N>>)> {
        let mut out = Vec::new();
        for f in self.frags(m) {
            out.extend(self.w.decls[&f].mods.iter().map(|c| (c.name, c.module, c.vis.clone())));
        }
        out
    }

    /// A written visibility as a value; restricted to the root is the whole crate.
    fn vis_of(&self, m: ItemKey, v: &WrittenVis<crate::Path<'static, N>>) -> Visibility {
        match self.vis_written(m, v) {
            Visibility::Restricted(a) if a == self.root => Visibility::Crate(self.krate),
            other => other,
        }
    }

    fn vis_written(&self, m: ItemKey, v: &WrittenVis<crate::Path<'static, N>>) -> Visibility {
        match v {
            WrittenVis::Private | WrittenVis::SelfOnly => Visibility::Restricted(m),
            WrittenVis::WithoutIn(_) => unreachable!("the generator writes no `pub(path)`"),
            WrittenVis::Pub => Visibility::Public,
            WrittenVis::Crate => Visibility::Crate(self.krate),
            WrittenVis::Super => Visibility::Restricted(self.parent(m).expect("the generator writes pub(super) below the root")),
            WrittenVis::In(p) => {
                // The Reference, `vis.scoped.edition2018`: "The 2015 edition may also use paths
                // starting with `::` or modules from the crate root."
                let from_root = matches!(p.kind, PathKind::Plain | PathKind::Abs) && self.w.roots[&self.krate].head.edition == crate::Edition::E2015;
                assert!(p.kind == PathKind::Crate || from_root, "the generator writes pub(in crate::..)");
                Visibility::Restricted(self.in_path(&p.segments).expect("pub(in ..) names an ancestor"))
            }
        }
    }

    fn visible(&self, v: Visibility, from: ItemKey) -> bool {
        match v {
            Visibility::Public | Visibility::Crate(_) => true,
            Visibility::Restricted(a) => self.chain(from).contains(&a),
        }
    }

    /// How narrow a visibility is seen from `from`: larger is narrower; one `from` cannot see
    /// is narrowest.
    fn rank(&self, v: Visibility, from: ItemKey) -> usize {
        match v {
            Visibility::Public => 0,
            Visibility::Crate(_) => 1,
            Visibility::Restricted(a) => {
                let chain = self.chain(from);
                match chain.iter().position(|&m| m == a) {
                    Some(i) => 2 + chain.len() - i,
                    None => usize::MAX,
                }
            }
        }
    }

    fn narrower(&self, a: Visibility, b: Visibility, from: ItemKey) -> Visibility {
        if self.rank(b, from) > self.rank(a, from) { b } else { a }
    }

    fn wider(&self, a: Visibility, b: Visibility, from: ItemKey) -> Visibility {
        if self.rank(b, from) < self.rank(a, from) { b } else { a }
    }

    fn pending_call(&self, c: ItemKey) -> bool {
        self.calls.get(&c).copied().unwrap_or(Call::Pending) == Call::Pending
    }

    /// `name` in `ns` bound by `m` itself: an item, a child module, an exported macro, a
    /// resolved named import. Else `None` and the pending named imports of `m` that can bind
    /// it and that `from` may see. `skip` is the asker, which never sees itself wherever the
    /// walk meets it.
    fn own(&self, m: ItemKey, ns: Namespace, name: N, skip: Option<Bk>, from: ItemKey) -> (Option<B>, Vec<Bk>) {
        // Items of `m`'s text and of every expanded call's text standing in it (`mk!`), and its
        // `extern crate`s: each binds its crate's root in the type namespace
        // (`items.extern-crate`).
        let frags = self.frags(m);
        for &f in frags.iter() {
            for it in self.w.decls[&f].items.iter() {
                if it.ns == ns && it.name == name {
                    return (Some(B { def: it.def, vis: self.vis_of(m, &it.vis), amb: false }), Vec::new());
                }
            }
            if ns != Namespace::Types {
                continue;
            }
            for ec in self.w.decls[&f].extern_crates.iter() {
                // The Reference, `items.extern-crate`: "The external crate is then bound into
                // the declaring scope as the identifier provided in the extern crate
                // declaration": the renamed form and the plain `extern crate x;` both.
                if let (Some(n), Some(k)) = (Ref::extern_binds(ec.target, ec.rename), self.extern_target(ec.target))
                    && n == name
                {
                    return (Some(B { def: Def::module(k), vis: self.vis_of(m, &ec.vis), amb: false }), Vec::new());
                }
            }
        }
        if ns == Namespace::Types {
            for (n, k, v) in self.children(m) {
                if n == name {
                    return (Some(B { def: Def::module(k), vis: self.vis_of(m, &v), amb: false }), Vec::new());
                }
            }
        }
        if ns == Namespace::Macros && m == self.root {
            for &(n, def, shapes) in self.w.roots[&self.krate].macro_exports.iter() {
                if n == name {
                    return (Some(B { def: Def { key: def, kind: DefKind::Macro(shapes) }, vis: Visibility::Public, amb: false }), Vec::new());
                }
            }
        }
        let mut pending = Vec::new();
        for (f, i, u) in self.uses_of(m) {
            let UseForm::Named { rename, .. } = u.leaf else { continue };
            let leaf = *u.path.segments.last().expect("a named import has a leaf");
            let to = match rename {
                Rename::Same => leaf,
                Rename::To(t) => t,
                Rename::Underscore => continue,
            };
            if to != name || Some(Bk::Imp(f, i)) == skip {
                continue;
            }
            match self.imports.get(&(f, i)).copied().unwrap_or(Imp::Pending) {
                Imp::Pending => {
                    if let Some(b) = self.partial.get(&(f, i)).and_then(|p| p[ns_at(ns)]) {
                        return (Some(b), Vec::new());
                    }
                    if self.visible(self.vis_of(m, &u.vis), from) {
                        pending.push(Bk::Imp(f, i));
                    }
                }
                Imp::Named(bs) => {
                    if let Some(b) = bs[ns_at(ns)]
                        && self.bug != Some(Bug::R1NamedLosesToGlob)
                    {
                        return (Some(b), Vec::new());
                    }
                }
                Imp::Failed(_) | Imp::Glob(_) => {}
            }
        }
        (None, pending)
    }

    /// The calls of `m` not expanded yet, but `skip`.
    fn calls_of(&self, m: ItemKey, skip: Option<Bk>) -> Vec<Bk> {
        let mut out = Vec::new();
        for f in self.frags(m) {
            let d = &self.w.decls[&f];
            out.extend(d.calls.iter().filter(|c| Some(Bk::Call(c.call)) != skip && self.pending_call(c.call)).map(|c| Bk::Call(c.call)));
        }
        out
    }

    /// Every `use` standing in `m`: (its fragment, its index there, the declaration). An import
    /// an expansion wrote is `m`'s from the moment the call expanded (planted
    /// `ExpansionImportsIgnored`: only `m`'s own text).
    fn uses_of(&self, m: ItemKey) -> Vec<(ItemKey, u32, &'w crate::UseDecl<N, crate::Path<'static, N>>)> {
        let w: &'w World = self.w;
        let mut out = Vec::new();
        for f in self.frags(m) {
            if f != m && self.bug == Some(Bug::ExpansionImportsIgnored) {
                continue;
            }
            out.extend(w.decls[&f].uses.iter().enumerate().map(|(i, u)| (f, i as u32, u)));
        }
        out
    }

    /// The decided glob edges of `m`, and its pending globs that `from` may see.
    fn globs(&self, m: ItemKey, skip: Option<Bk>, from: ItemKey) -> (Vec<(ItemKey, Visibility)>, Vec<Bk>) {
        let mut out = Vec::new();
        let mut pending = Vec::new();
        for (f, i, u) in self.uses_of(m) {
            if u.leaf != UseForm::Glob || Some(Bk::Imp(f, i)) == skip {
                continue;
            }
            match self.imports.get(&(f, i)).copied().unwrap_or(Imp::Pending) {
                Imp::Pending => {
                    if self.visible(self.vis_of(m, &u.vis), from) {
                        pending.push(Bk::Imp(f, i));
                    }
                }
                Imp::Glob(s) => out.push((s, self.vis_of(m, &u.vis))),
                Imp::Named(_) | Imp::Failed(_) => {}
            }
        }
        (out, pending)
    }

    /// `name` in `ns` as `m` has it, asked by an item of `from` (`skip`): its own binding, else
    /// the least fixpoint over its globs. The wait relation (bottom.md section 6, the stall
    /// rule): unbound, it waits on `m`'s pending named imports of `name`, its pending globs
    /// (and what the walk meets undecided) and its calls; bound only through globs, on the
    /// named imports, the pending globs and what the walk met undecided (R2: one may bring
    /// another item), and, outside the macro namespace, the calls.
    fn lookup(&self, m: ItemKey, ns: Namespace, name: N, skip: Option<Bk>, from: ItemKey) -> Look {
        self.lookup_in(m, ns, name, skip, from, true)
    }

    /// `lookup`; `own_calls` false leaves out `m`'s own pending calls (R6 weighs them apart).
    fn lookup_in(&self, m: ItemKey, ns: Namespace, name: N, skip: Option<Bk>, from: ItemKey, own_calls: bool) -> Look {
        let (v, wait) = self.lookup_parts(m, ns, name, skip, from, own_calls);
        if wait.is_empty() { Ok(v) } else { Err(wait) }
    }

    /// `lookup` in a stalled try (R5): what is undecided is taken as absent, so the answer is
    /// the value the current facts give (a pending import or call writes nothing).
    fn lookup_absent(&self, m: ItemKey, ns: Namespace, name: N, skip: Option<Bk>, from: ItemKey) -> Option<B> {
        if self.bug == Some(Bug::R5StallFailsMiddle) {
            return self.lookup(m, ns, name, skip, from).ok().flatten();
        }
        self.lookup_parts(m, ns, name, skip, from, true).0
    }


    /// `lookup_in`'s value from the current facts and what it waits on.
    fn lookup_parts(&self, m: ItemKey, ns: Namespace, name: N, skip: Option<Bk>, from: ItemKey, own_calls: bool) -> (Option<B>, Vec<Bk>) {
        let (bound, named) = self.own(m, ns, name, skip, from);
        if let Some(b) = bound {
            return (Some(b), Vec::new());
        }
        let calls = if own_calls { self.calls_of(m, skip) } else { Vec::new() };
        // Every module the globs reach, and its own value; the walk stops at an own binding.
        // A source with something pending of its own that may bind the name is an open node:
        // no value and no edges.
        let (edges, mut open) = self.globs(m, skip, from);
        let mut todo: Vec<ItemKey> = edges.iter().map(|e| e.0).collect();
        let mut nodes: Vec<(ItemKey, Option<B>, Vec<(ItemKey, Visibility)>)> = vec![(m, None, edges)];
        while let Some(s) = todo.pop() {
            if nodes.iter().any(|(k, _, _)| *k == s) {
                continue;
            }
            let (b, named_s) = self.own(s, ns, name, skip, m);
            if let Some(b) = b {
                nodes.push((s, Some(b), Vec::new()));
                continue;
            }
            let calls_s = if ns == Namespace::Macros { Vec::new() } else { self.calls_of(s, skip) };
            if !named_s.is_empty() || !calls_s.is_empty() {
                open.extend(named_s);
                open.extend(calls_s);
                nodes.push((s, None, Vec::new()));
                continue;
            }
            let (e, p) = self.globs(s, skip, m);
            open.extend(p);
            todo.extend(e.iter().map(|e| e.0));
            nodes.push((s, None, e));
        }
        let fixed: Vec<bool> = nodes.iter().map(|n| n.1.is_some()).collect();
        let mut vals: Vec<Option<B>> = nodes.iter().map(|n| n.1).collect();
        loop {
            let mut changed = false;
            for i in 0..nodes.len() {
                if fixed[i] {
                    continue;
                }
                let importer = nodes[i].0;
                let mut acc: Option<B> = None;
                for &(src, evis) in nodes[i].2.iter() {
                    let j = nodes.iter().position(|n| n.0 == src).expect("every source was walked");
                    let Some(b) = vals[j] else { continue };
                    if !self.visible(b.vis, importer) && self.bug != Some(Bug::R4GlobIgnoresPrivacy) {
                        continue;
                    }
                    let b = B { vis: self.narrower(evis, b.vis, importer), ..b };
                    let clash = self.bug != Some(Bug::R2GlobsNeverClash);
                    acc = Some(match acc {
                        None => b,
                        Some(a) if a.def == b.def => B { def: a.def, vis: self.wider(a.vis, b.vis, importer), amb: a.amb || b.amb },
                        Some(a) => B { def: if a.def < b.def { a.def } else { b.def }, vis: a.vis, amb: clash },
                    });
                }
                if acc != vals[i] {
                    vals[i] = acc;
                    changed = true;
                }
            }
            if !changed || self.bug == Some(Bug::R3OnePass) {
                break;
            }
        }
        let mut wait = named;
        match vals[0] {
            Some(b) => {
                if ns != Namespace::Macros {
                    wait.extend(calls);
                }
                // R2: a pending glob, or a source the walk met undecided, may still bring
                // another item of the name: the use is then ambiguous. Bound or not, the lookup
                // waits for them, so its answer does not hang on which was decided first (r2e:
                // `self::m2` took the root's `m2` before `crate::m0::*` brought crate 3's). Not
                // in the macro namespace: a call waiting on a glob whose path waits on the call
                // is a sink R5 fails, though the call has a binding (oldest seed 364: `mx!`
                // found through a glob, failed as not found); a macro path's R2 wait is open.
                if ns != Namespace::Macros && self.bug != Some(Bug::R2BoundStopsWaiting) {
                    wait.extend(open);
                }
                (Some(b), wait)
            }
            None => {
                wait.extend(calls);
                wait.extend(open);
                (None, wait)
            }
        }
    }

    /// One try at import `i` of `m`: `Err` with what it waits on while undecided. `stall`: it
    /// is failed in a sink of the wait graph, so what is undecided is taken as absent.
    /// `Err` also carries what the leaf found so far in each namespace.
    fn try_import(&self, f: ItemKey, i: u32, stall: bool) -> Result<Imp, (Vec<Bk>, [Option<B>; 3])> {
        let u = &self.w.decls[&f].uses[i as usize];
        let m = self.module_of(f);
        let mut cur = match u.path.kind {
            PathKind::Crate => self.root,
            PathKind::SelfMod => m,
            PathKind::Super(n) => {
                let mut at = m;
                for _ in 0..n {
                    match self.parent(at) {
                        Some(p) => at = p,
                        None => return Ok(Imp::Failed(Unresolved::NoParent)),
                    }
                }
                at
            }
            // A lexical first segment (the wide distribution): taken below, as segment 0.
            PathKind::Plain => self.root,
            // The Reference, `paths.qualifiers.global-root.edition2018`: "In the 2015 Edition,
            // identifiers resolve from the "crate root"" and "Beginning with the 2018 Edition,
            // paths starting with `::` resolve from crates in the extern prelude. That is, they
            // must be followed by the name of a crate."
            PathKind::Abs if self.w.roots[&self.krate].head.edition == crate::Edition::E2015 => self.root,
            PathKind::Abs => {
                let s0 = u.path.segments[0];
                let dep = self.w.roots[&self.krate].extern_prelude.iter().find(|(n, _)| *n == s0).map(|&(_, k)| k);
                let Some(k) = dep.or_else(|| self.extern_added(s0)) else { return Ok(Imp::Failed(Unresolved::NotFound)) };
                if u.path.segments.len() == 1 && u.leaf != UseForm::Glob {
                    // `use ::name;`: the crate itself, in the type namespace.
                    return Ok(Imp::Named([Some(B { def: Def::module(k), vis: self.vis_of(m, &u.vis), amb: false }), None, None]));
                }
                k
            }
            PathKind::DollarCrate(_) => unreachable!("the generator writes crate, self, super, plain and `::` paths"),
        };
        let me = Some(Bk::Imp(f, i));
        let segs = &u.path.segments[..];
        let steps = if u.leaf == UseForm::Glob { segs.len() } else { segs.len() - 1 };
        let mut first = 0;
        if u.path.kind == PathKind::Abs && self.w.roots[&self.krate].head.edition != crate::Edition::E2015 {
            first = 1; // segment 0 named the crate `cur` is the root of
        }
        // The Reference, `items.use.path.edition2018`: "In the 2015 edition, `use` paths are
        // relative to the crate root." and "The 2015 edition does not allow use declarations to
        // reference the extern prelude.": a 2015 plain path is walked from `self.root` (set
        // above), every segment by `lookup`, with no lexical first segment and no outer scope.
        let lexical = u.path.kind == PathKind::Plain && self.w.roots[&self.krate].head.edition != crate::Edition::E2015;
        if lexical && steps == 0 {
            return self.try_one_segment(f, i, stall);
        }
        if lexical {
            assert!(steps >= 1, "the generator writes a plain path with a module segment before its leaf");
            let b = match self.first_plain(m, segs[0], me, stall) {
                Err(wait) => return Err((wait, [None; 3])),
                Ok(Err(why)) => return Ok(Imp::Failed(why)),
                Ok(Ok(b)) => b,
            };
            if !self.visible(b.vis, m) {
                return Ok(Imp::Failed(Unresolved::Private));
            }
            if b.amb {
                return Ok(Imp::Failed(Unresolved::Ambiguous(crate::AmbiguityKind::GlobVsGlob)));
            }
            if b.def.kind != DefKind::Module {
                return Ok(Imp::Failed(Unresolved::NotAModule));
            }
            cur = b.def.key;
            first = 1;
        }
        for &s in &segs[first..steps] {
            // Stalled (R5), the undecided is taken as absent: the current facts' value (the
            // cell core lane's seed 1983: `use crate::m2::*` beside a pending `mx!` resolves
            // through `crate::m1::*`'s `m2`; failing it was the reference's bug).
            let look = match self.lookup(cur, Namespace::Types, s, me, m) {
                Err(_) if stall => Ok(self.lookup_absent(cur, Namespace::Types, s, me, m)),
                r => r,
            };
            match look {
                Err(wait) => return Err((wait, [None; 3])),
                Ok(None) => return Ok(Imp::Failed(Unresolved::NotFound)),
                Ok(Some(b)) => {
                    if !self.visible(b.vis, m) {
                        return Ok(Imp::Failed(Unresolved::Private));
                    }
                    if b.amb {
                        return Ok(Imp::Failed(Unresolved::Ambiguous(crate::AmbiguityKind::GlobVsGlob)));
                    }
                    if b.def.kind != DefKind::Module {
                        return Ok(Imp::Failed(Unresolved::NotAModule));
                    }
                    cur = b.def.key;
                }
            }
        }
        if u.leaf == UseForm::Glob {
            return Ok(Imp::Glob(cur));
        }
        let leaf = segs[segs.len() - 1];
        let ivis = self.vis_of(m, &u.vis);
        let mut out = [None; 3];
        let (mut found, mut hidden, mut amb) = (false, false, false);
        let mut waits: Vec<Bk> = Vec::new();
        for ns in NSS {
            let look = match self.lookup(cur, ns, leaf, me, m) {
                Err(_) if stall => Ok(self.lookup_absent(cur, ns, leaf, me, m)),
                r => r,
            };
            match look {
                Err(wait) => waits.extend(wait),
                Ok(None) => {}
                Ok(Some(b)) if !self.visible(b.vis, m) => hidden = true,
                Ok(Some(b)) if b.amb => amb = true,
                Ok(Some(b)) => {
                    found = true;
                    // A `pub use` of an `extern crate` item's binding is a public re-export:
                    // the Reference does not state the case; rustc decides it
                    // (rustc_resolve imports.rs, `pub_use_of_private_extern_crate_hack`: "if let
                    // ImportKind::ExternCrate { .. } = decl_import.kind && import.vis.is_public()",
                    // then the import's own visibility with the lint
                    // PUB_USE_OF_PRIVATE_EXTERN_CRATE, capped in a dependency: `pub use core as
                    // core_;` over the private injected `extern crate core;`). Only a fully
                    // public `use`: any narrower one keeps the binding's visibility.
                    let reexported = ns == Namespace::Types && ivis == Visibility::Public && self.w.decls.contains_key(&cur) && self.extern_item(cur, leaf, b.def);
                    let vis = if reexported && self.bug != Some(Bug::ExternReexportNarrowed) { ivis } else { self.narrower(ivis, b.vis, m) };
                    out[ns_at(ns)] = Some(B { def: b.def, vis, amb: false });
                }
            }
        }
        Ok(if amb {
            // An ambiguous namespace fails the import at once, whatever the others wait on.
            Imp::Failed(Unresolved::Ambiguous(crate::AmbiguityKind::GlobVsGlob))
        } else if !waits.is_empty() {
            return Err((waits, out));
        } else if found {
            Imp::Named(out)
        } else if hidden {
            Imp::Failed(Unresolved::Private)
        } else {
            Imp::Failed(Unresolved::NotFound)
        })
    }

    /// `use name;` from 2018 on (import `i` of fragment `f`): the name in each namespace. In the
    /// macro namespace the textual scope at the import is read first: the Reference,
    /// `macro.decl.scope.path.reexport`: "Macros can be re-exported to give them path-based scope
    /// from a module other than the crate root." (`macro_rules! m { .. } pub(crate) use m;`,
    /// "from within `m`'s textual scope"), with the macro's own visibility,
    /// `macro.decl.scope.path-based.visibility`: "Macros have an implicit visibility of
    /// `pub(crate)`. `#[macro_export]` changes the implicit visibility to `pub`." Else the
    /// module's own scope, then for a type the outer candidate (the extern prelude, R8). This
    /// states no R6 finding for the leaf: the generator writes no such import.
    fn try_one_segment(&self, f: ItemKey, i: u32, stall: bool) -> Result<Imp, (Vec<Bk>, [Option<B>; 3])> {
        let u = &self.w.decls[&f].uses[i as usize];
        let m = self.module_of(f);
        let me = Some(Bk::Imp(f, i));
        let leaf = u.path.segments[0];
        let ivis = self.vis_of(m, &u.vis);
        let mut out = [None; 3];
        let (mut found, mut hidden, mut amb) = (false, false, false);
        let mut waits: Vec<Bk> = Vec::new();
        for ns in NSS {
            let textual = if ns == Namespace::Macros { self.textual(f, u.pos, leaf) } else { Ok(None) };
            let look = match textual {
                Ok(Some(b)) => {
                    let exported = self.w.decls.values().any(|d| d.macro_rules.iter().any(|r| r.def == b.def.key && r.export));
                    Ok(Some(B { vis: if exported { Visibility::Public } else { b.vis }, ..b }))
                }
                Ok(None) => self.lookup(m, ns, leaf, me, m),
                Err(wait) => Err(wait),
            };
            let look = match look {
                Err(_) if stall => Ok(self.lookup_absent(m, ns, leaf, me, m)),
                r => r,
            };
            let look = match look {
                Ok(None) if ns == Namespace::Types => Ok(self.outer(leaf)),
                r => r,
            };
            match look {
                Err(wait) => waits.extend(wait),
                Ok(None) => {}
                Ok(Some(b)) if !self.visible(b.vis, m) => hidden = true,
                Ok(Some(b)) if b.amb => amb = true,
                Ok(Some(b)) => {
                    found = true;
                    out[ns_at(ns)] = Some(B { def: b.def, vis: self.narrower(ivis, b.vis, m), amb: false });
                }
            }
        }
        Ok(if amb {
            Imp::Failed(Unresolved::Ambiguous(crate::AmbiguityKind::GlobVsGlob))
        } else if !waits.is_empty() {
            return Err((waits, out));
        } else if found {
            Imp::Named(out)
        } else if hidden {
            Imp::Failed(Unresolved::Private)
        } else {
            Imp::Failed(Unresolved::NotFound)
        })
    }

    /// The slots of `m`'s text that the textual scope sees, in text order.
    fn slots(&self, m: ItemKey) -> Vec<(u32, Slot)> {
        let d = &self.w.decls[&m];
        let mut out: Vec<(u32, Slot)> = Vec::new();
        out.extend(d.macro_rules.iter().map(|r| (r.pos, Slot::Rules(r.name, r.def))));
        out.extend(d.mods.iter().map(|c| (c.pos, Slot::Mod(c.module, c.macro_use))));
        out.extend(d.calls.iter().map(|c| (c.pos, Slot::Call(c.call))));
        out.sort_by_key(|s| s.0);
        out
    }

    /// `name!` by the textual scope at `pos` of fragment `m` (a module's text or a call's
    /// expansion): searched backward, into `#[macro_use]` children, then out of an expansion at
    /// its call and up at each `mod` line; a call still pending on the way may still define it.
    fn textual(&self, m: ItemKey, pos: u32, name: N) -> Look {
        let mut cur = m;
        let mut end = pos;
        loop {
            let mut stack: Vec<Vec<(u32, Slot)>> = vec![self.slots(cur).into_iter().filter(|s| s.0 < end).collect()];
            while let Some(top) = stack.last_mut() {
                let Some((_, slot)) = top.pop() else {
                    stack.pop();
                    continue;
                };
                match slot {
                    Slot::Rules(n, def) if n == name => {
                        return Ok(Some(B { def: Def { key: def, kind: DefKind::Macro(crate::MacroShapes::FN_LIKE) }, vis: Visibility::Crate(self.krate), amb: false }));
                    }
                    Slot::Rules(..) | Slot::Mod(_, false) => {}
                    Slot::Mod(child, true) => stack.push(self.slots(child)),
                    Slot::Call(c) => {
                        if self.pending_call(c) {
                            return Err(vec![Bk::Call(c)]);
                        }
                    }
                }
            }
            if self.w.owner.contains_key(&cur) {
                let at = self.written_in(cur);
                end = self.w.decls[&at].calls.iter().find(|c| c.call == cur).expect("written there").pos;
                cur = at;
                continue;
            }
            if self.parent(cur).is_none() {
                return Ok(None);
            }
            (cur, end) = self.mod_site(cur);
        }
    }

    /// One try at a call: the textual scope, then the module's macro namespace. `Err` with what
    /// it waits on while undecided.
    fn try_call(&self, f: ItemKey, call: ItemKey) -> Result<Call, Vec<Bk>> {
        let c = self.w.decls[&f].calls.iter().find(|c| c.call == call).expect("a call of f");
        let m = self.module_of(f);
        let name = c.path.segments[0];
        if self.textual(f, c.pos, name)?.is_some() {
            return Ok(Call::Done);
        }
        Ok(match self.lookup(m, Namespace::Macros, name, Some(Bk::Call(call)), m)? {
            None => Call::Failed(Unresolved::NotFound),
            Some(b) if !self.visible(b.vis, m) => Call::Failed(Unresolved::Private),
            Some(b) if b.amb => Call::Failed(Unresolved::Ambiguous(crate::AmbiguityKind::GlobVsGlob)),
            Some(_) => Call::Done,
        })
    }

    /// Repeat until nothing changes: every pending import and call of every live module tried
    /// against the current facts, and every decided import tried again (one that now reads an
    /// ambiguous binding fails as ambiguous; nothing else of a decided import changes). Then,
    /// if anything is still pending, the sinks of the wait graph fail as not found, and again.
    fn run(&mut self) {
        loop {
            let mut changed = false;
            let mut pending: Vec<(Bk, Vec<Bk>)> = Vec::new();
            // Every fragment that exists (a module's text, an expanded call's text), named `m`
            // below: imports and calls are keyed by the fragment they are written in.
            let frags: Vec<ItemKey> = self.live().into_iter().flat_map(|m| self.frags(m)).collect();
            for m in frags {
                let d = &self.w.decls[&m];
                for i in 0..d.uses.len() as u32 {
                    let was = self.imports.get(&(m, i)).copied().unwrap_or(Imp::Pending);
                    match (was, self.try_import(m, i, false)) {
                        (Imp::Failed(_), _) => {}
                        (Imp::Pending, Ok(r)) => {
                            self.imports.insert((m, i), r);
                            changed = true;
                        }
                        (Imp::Pending, Err((wait, found))) => {
                            // Only grows: a namespace once found stays found.
                            let old = self.partial.get(&(m, i)).copied().unwrap_or([None; 3]);
                            let new = [old[0].or(found[0]), old[1].or(found[1]), old[2].or(found[2])];
                            if new != old {
                                self.partial.insert((m, i), new);
                                changed = true;
                            }
                            pending.push((Bk::Imp(m, i), wait));
                        }
                        (Imp::Named(_) | Imp::Glob(_), Ok(Imp::Failed(Unresolved::Ambiguous(k)))) => {
                            self.imports.insert((m, i), Imp::Failed(Unresolved::Ambiguous(k)));
                            changed = true;
                        }
                        (Imp::Named(_) | Imp::Glob(_), _) => {}
                    }
                }
                let calls: Vec<ItemKey> = d.calls.iter().map(|c| c.call).collect();
                for call in calls {
                    if !self.pending_call(call) {
                        continue;
                    }
                    match self.try_call(m, call) {
                        Ok(r) => {
                            self.calls.insert(call, r);
                            changed = true;
                        }
                        Err(wait) => pending.push((Bk::Call(call), wait)),
                    }
                }
            }
            if changed {
                continue;
            }
            if pending.is_empty() || self.bug == Some(Bug::R5NoStall) {
                return;
            }
            // The sinks of the condensation, plainly: `v` is in one when everything it reaches
            // reaches it back.
            let reach = |from: Bk| -> BTreeSet<Bk> {
                let mut seen = BTreeSet::from([from]);
                let mut todo = vec![from];
                while let Some(v) = todo.pop() {
                    let Some((_, wait)) = pending.iter().find(|p| p.0 == v) else { continue };
                    for &u in wait.iter() {
                        if seen.insert(u) {
                            todo.push(u);
                        }
                    }
                }
                seen
            };
            let reaches: Vec<BTreeSet<Bk>> = pending.iter().map(|p| reach(p.0)).collect();
            let mut sinks = Vec::new();
            for (k, p) in pending.iter().enumerate() {
                let back = reaches[k].iter().all(|u| match pending.iter().position(|q| q.0 == *u) {
                    Some(j) => reaches[j].contains(&p.0),
                    None => false,
                });
                if back {
                    sinks.push(p.0);
                }
            }
            assert!(!sinks.is_empty(), "a finite graph has a sink component");
            // A call whose textual read waits on a call of its sink is spared this round: that
            // call fails first and defines nothing (the Reference's textual scope).
            let textual_wait = |v: Bk| match v {
                Bk::Call(_) if self.bug == Some(Bug::R5SparesAll) => true,
                Bk::Call(call) => {
                    let m = self.written_in(call);
                    let c = self.w.decls[&m].calls.iter().find(|c| c.call == call).expect("a call of m");
                    matches!(self.textual(m, c.pos, c.path.segments[0]), Err(w) if w.iter().all(|u| sinks.contains(u)))
                }
                Bk::Imp(..) => false,
            };
            let spared: Vec<Bk> = sinks.iter().copied().filter(|&v| textual_wait(v)).collect();
            let mut sinks: Vec<Bk> = sinks.iter().copied().filter(|&v| !textual_wait(v)).collect();
            if sinks.is_empty() && self.bug == Some(Bug::R5SparesAll) {
                return;
            }
            // A spared call still expands, so "nothing more comes" is false for every member
            // that waits on it: this round fails only the calls a spared call's textual read
            // waits on (they define nothing first); the rest are tried again with the facts
            // (wide seed 644: `use crate::m2::*` beside a spared `mk!` writing `mod m2`; r5e).
            if !spared.is_empty() && self.bug != Some(Bug::R5SparesAll) {
                let mut first: BTreeSet<Bk> = BTreeSet::new();
                for &v in spared.iter() {
                    let Bk::Call(call) = v else { continue };
                    let m = self.written_in(call);
                    let c = self.w.decls[&m].calls.iter().find(|c| c.call == call).expect("a call of m");
                    if let Err(w) = self.textual(m, c.pos, c.path.segments[0]) {
                        first.extend(w.into_iter().filter(|u| matches!(u, Bk::Call(_)) && sinks.contains(u)));
                    }
                }
                self.behind_spared.extend(sinks.iter().copied().filter(|v| !first.contains(v)));
                sinks.retain(|v| first.contains(v));
            }
            assert!(!sinks.is_empty(), "text order leaves every sink a member that reads no call of it");
            let failed: Vec<(Bk, Option<Imp>)> = sinks
                .iter()
                .map(|&v| match v {
                    Bk::Imp(m, i) => (v, Some(self.try_import(m, i, true).expect("a stalled try decides"))),
                    Bk::Call(_) => (v, None),
                })
                .collect();
            // Taken as absent, an undecided inner scope may send a plain first segment outward,
            // and the import RESOLVES: what it brings may answer another member, so only those
            // are decided this round, and the rest are tried again with the facts (the stall's
            // "nothing more comes" holds only for what fails). Without one, all fail.
            // r5g (name_rules.rs `KNOWN`): which of these resolve only because a later segment's
            // undecided lookup is taken as absent (the reference failed them so until the cell
            // core lane's seed 1983; so does the old solver).
            let was = self.bug;
            self.bug = Some(Bug::R5StallFailsMiddle);
            for (v, r) in failed.iter() {
                if let (Bk::Imp(f, i), Some(Imp::Named(_) | Imp::Glob(_))) = (*v, r)
                    && matches!(self.try_import(f, i, true), Ok(Imp::Failed(_)))
                {
                    self.middle_stalled.insert(*v);
                }
            }
            self.bug = was;
            let resolved = failed.iter().any(|(_, r)| matches!(r, Some(Imp::Named(_) | Imp::Glob(_))));
            let failed: Vec<(Bk, Option<Imp>)> = if resolved {
                failed.into_iter().filter(|(_, r)| matches!(r, Some(Imp::Named(_) | Imp::Glob(_)))).collect()
            } else {
                failed
            };
            for (v, imp) in failed {
                match (v, imp) {
                    (Bk::Imp(m, i), Some(r)) => {
                        self.imports.insert((m, i), r);
                    }
                    (Bk::Call(c), _) => {
                        self.calls.insert(c, Call::Failed(Unresolved::NotFound));
                    }
                    (Bk::Imp(..), None) => unreachable!("an import's try is kept"),
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The comparison.

/// The reference's answer a fixture checks besides the solver's (name_rules.rs): what `name` in
/// `ns` names at `m`, and the number of R6 errors and R8 errors it finds.
pub(super) fn reference_says(w: &World, root: ItemKey, m: ItemKey, ns: Namespace, name: N) -> (Option<ItemKey>, usize, usize) {
    let mut r = Ref::new(w, root);
    r.run();
    let def = r.lookup(m, ns, name, None, m).ok().flatten().filter(|b| !b.amb).map(|b| b.def.key);
    (def, r.r6_findings().len(), r.extern_replacing().len())
}

/// R9 for a fixture: `name` in type position at `m`, by the reference and by the solver.
pub(super) fn type_position(w: &World, root: ItemKey, s: &Solved, m: ItemKey, name: N) -> (Option<Def>, Option<Def>) {
    let mut r = Ref::new(w, root);
    r.run();
    (r.type_path(m, name), solver_type_path(w, s, m, name))
}

/// The solver's finished records as the names library reads them (R9's query on its side).
struct Records<'s> {
    w: &'s World,
    s: &'s Solved,
}

impl crate::names::World<N> for Records<'_> {
    type Scope<'w>
        = &'w crate::ScopeBuilder<'w, N>
    where
        Self: 'w;
    type Root<'w>
        = &'w crate::CrateRootBuilder<'w, N>
    where
        Self: 'w;
    fn scope(&self, module: ItemKey) -> Option<&crate::ScopeBuilder<'_, N>> {
        self.s.by.get(&module).map(|r| &r.scope)
    }
    fn chain(&self, module: ItemKey) -> Option<&[ItemKey]> {
        self.s.chain.get(&module).map(|c| &c[..])
    }
    fn root_of(&self, key: ItemKey) -> Option<&crate::CrateRootBuilder<'_, N>> {
        self.w.roots.values().find(|r| r.head.root_module.crate_prefix() == key.crate_prefix())
    }
    fn root(&self, krate: CrateItemKey) -> Option<&crate::CrateRootBuilder<'_, N>> {
        self.w.roots.get(&krate)
    }
    fn variant(&self, _owner: ItemKey, _name: N) -> Option<Option<crate::names::VariantHit>> {
        None
    }
    fn no_implicit_prelude(&self, module: ItemKey) -> bool {
        self.s.chain.get(&module).is_some_and(|c| c.iter().any(|m| self.w.modules[m].no_implicit_prelude))
    }
    fn anonymous(&self, module: ItemKey) -> bool {
        self.w.modules.get(&module).is_some_and(|info| info.name.is_some_and(|n| crate::blocks::is_block_module(n.as_ref())))
    }
}

/// R9 on the solver's side: the one-segment path `name` in the type namespace at `m`, read
/// through the names library over the solver's records; `None` when it names nothing whole.
fn solver_type_path(w: &World, s: &Solved, m: ItemKey, name: N) -> Option<Def> {
    let segments = [name];
    let written = crate::names::Written { kind: PathKind::Plain, segments: &segments[..] };
    crate::names::path(&Records { w, s }, m, Namespace::Types, written, blaze_own::Alloc::global()).ok().and_then(|r| r.whole(1))
}

/// A binding as both sides are compared: an ambiguous one by that alone.
fn norm(b: Option<B>) -> Option<(Option<Def>, Option<Visibility>)> {
    b.map(|b| if b.amb { (None, None) } else { (Some(b.def), Some(b.vis)) })
}

/// Two reasons a path failed, as the oracle holds them equal: the solver says `NotFound` where
/// the only binding is one the asker may not name through a glob (rustc's `check_usable`), the
/// reference `Private`; both leave the path unresolved, the difference is the message.
fn same_why(a: Unresolved, b: Unresolved) -> bool {
    let plain = |u: Unresolved| if u == Unresolved::Private { Unresolved::NotFound } else { u };
    plain(a) == plain(b)
}

fn compare(w: &World, root: ItemKey, s: &Solved) -> Result<(), String> {
    compare_with(w, root, s, None)
}

/// An early-resolution ambiguity of `name` reported in `m`'s errors (R6, R7).
fn early_ambiguity(s: &Solved, m: ItemKey, name: N) -> bool {
    s.errors(m).iter().any(|e| match e {
        ScopeError::Ambiguous { name: n, .. } | ScopeError::Inconsistent { name: n, .. } => *n == name,
        _ => false,
    })
}

fn compare_with(w: &World, root: ItemKey, s: &Solved, bug: Option<Bug>) -> Result<(), String> {
    compare_known(w, root, s, bug).map(|_| ())
}

/// A difference of one import that a fixture in name_rules.rs `KNOWN` pins on the solver: that
/// fixture's name, while it is listed there (taken off `KNOWN`, the class fails again).
/// r7d: the rule fails the import at its first segment, glob-vs-outer; the solver answers not
/// found further on. r7f: the solver reports the glob-vs-outer ambiguity early and still
/// resolves the import through the glob candidate (the rule: the import fails, binds nothing).
fn known_import(want: Imp, got: Option<Unresolved>, early: bool) -> Option<&'static str> {
    let name = match (want, got) {
        (Imp::Failed(Unresolved::Ambiguous(crate::AmbiguityKind::GlobVsOuter)), Some(b)) if same_why(Unresolved::NotFound, b) => "r7d",
        (Imp::Failed(Unresolved::Ambiguous(crate::AmbiguityKind::GlobVsOuter)), None) if early => "r7f",
        // A wrong accept: the import kept and no ambiguity reported anywhere.
        (Imp::Failed(Unresolved::Ambiguous(crate::AmbiguityKind::GlobVsOuter)), None) => "r7g",
        // A wrong accept of R2: two globs bring different items, one through an `extern crate`
        // item (deep tape in name_rules.rs `KNOWN` r2c).
        (Imp::Failed(Unresolved::Ambiguous(crate::AmbiguityKind::GlobVsGlob)), None) => "r2c",
        // The same false NotFound upstream, the ambiguous name used at a middle segment
        // (`self::m2::m0::*`): the solver answers NotFound at `m0` (deep tape, r16-deep2.txt).
        (Imp::Failed(Unresolved::Ambiguous(crate::AmbiguityKind::GlobVsGlob)), Some(b)) if same_why(Unresolved::NotFound, b) => "r2c",
        _ => return None,
    };
    super::name_rules::KNOWN.iter().any(|(n, _)| *n == name).then_some(name)
}

/// `compare_with`, also returning the `KNOWN` classes it met and skipped (`known_import`). An
/// import of a known class is not compared, and neither are the case's bindings, which follow
/// from it.
fn compare_known(w: &World, root: ItemKey, s: &Solved, bug: Option<Bug>) -> Result<Vec<&'static str>, String> {
    let mut skipped: Vec<&'static str> = Vec::new();
    let mut r = Ref::new(w, root);
    r.bug = bug;
    r.run();
    // R6: each import that went outward before an expansion wrote its first segment is an
    // ambiguity, whatever the order.
    let r6_all = r.r6_findings();
    for &(m, f, i, s0) in r6_all.iter() {
        let site = crate::UseSite { expansion: (f != m).then_some(f), index: i };
        let on_import = s.errors(m).iter().any(|e| matches!(e, ScopeError::Import { site: t, why: Unresolved::Ambiguous(_), .. } if *t == site));
        // r6e (name_rules.rs `KNOWN`): the solver fails the import further on instead.
        let later = s.errors(m).iter().any(|e| matches!(e, ScopeError::Import { site: t, why, .. } if *t == site && same_why(Unresolved::NotFound, *why)));
        if !on_import && !early_ambiguity(s, m, s0) && later && super::name_rules::KNOWN.iter().any(|(n, _)| *n == "r6e") {
            skipped.push("r6e");
            continue;
        }
        if !on_import && !early_ambiguity(s, m, s0) {
            return Err(format!("import {i} of {m:x}: R6 says `{s0}` is ambiguous (outer, then written by an expansion), the solver reports nothing: {:?}", s.errors(m)));
        }
    }
    let live: BTreeSet<ItemKey> = r.live().into_iter().collect();
    let solved: BTreeSet<ItemKey> = s.by.keys().copied().collect();
    // The test driver solves every module of the world, one a failed call would have declared
    // too: only the live ones are compared.
    if !live.is_subset(&solved) {
        return Err(format!("modules: the reference has {} live, the solver finished {} of them", live.len(), live.intersection(&solved).count()));
    }
    let mut names: Vec<N> = Vec::new();
    names.extend_from_slice(&ITEMS);
    names.extend_from_slice(&TARGETS);
    names.extend_from_slice(&MACROS);
    names.extend_from_slice(&MODS);
    // R8: a macro-written root `extern crate` naming a dependency's extern-prelude name is an
    // error (D56); the program is rejected and its recovery not compared.
    for n in r.extern_replacing() {
        let named = format!("{:?}", s.errors(root)).contains(&format!("\"{n}\""));
        // r8d (name_rules.rs `KNOWN`): the old solver reports the replacement only where a path
        // reads the name.
        if !named && super::name_rules::KNOWN.iter().any(|(k, _)| *k == "r8d") {
            skipped.push("r8d");
        } else if !named {
            return Err(format!("R8: `extern crate .. as {n};` would replace a dependency's name: the solver reports nothing naming it: {:?}", s.errors(root)));
        }
        skipped.push("R8 error, recovery not compared");
    }
    // Every import standing in a live module, in its own text or in an expanded call's text
    // (an expansion's import is the module's, `UseSite { expansion: Some(call) }`).
    for &m in live.iter() {
        let errors = s.errors(m);
        let uses: Vec<(ItemKey, usize, &crate::UseDecl<N, crate::Path<'static, N>>)> =
            r.frags(m).into_iter().flat_map(|f| w.decls[&f].uses.iter().enumerate().map(move |(i, u)| (f, i, u))).collect();
        for (f, i, u) in uses {
            let site = crate::UseSite { expansion: (f != m).then_some(f), index: i as u32 };
            let got = errors.iter().find_map(|e| match e {
                ScopeError::Import { site: s, why, .. } if *s == site => Some(*why),
                _ => None,
            });
            let want = r.imports.get(&(f, i as u32)).copied().unwrap_or(Imp::Pending);
            let first = u.path.segments.first().copied().unwrap_or("");
            let r6 = r6_all.contains(&(m, f, i as u32, first));
            let ok = match (want, got) {
                // The solver reports the import's plain first segment ambiguous (R7's
                // glob-vs-outer, reported early: r7f) where the reference resolved it: the
                // reference broke R7 (the planted `R7GlobBeatsOuter` is caught here).
                (Imp::Named(_) | Imp::Glob(_), None) if u.path.kind == PathKind::Plain && !r6 && early_ambiguity(s, m, first) => false,
                (Imp::Named(_) | Imp::Glob(_), None) => true,
                // R6's ambiguity may stand on the import itself.
                (Imp::Named(_) | Imp::Glob(_), Some(Unresolved::Ambiguous(_))) => r6,
                // R6's error checked above: the import's own outcome follows the order.
                (_, None | Some(Unresolved::Ambiguous(_))) if r6 => true,
                // R7's glob-vs-outer fails the import: reported only early, with the import
                // kept, is r7f (`known_import`).
                (Imp::Failed(a), Some(b)) => same_why(a, b),
                // R5: the reference's run decides every import (a stall fails it); an import it
                // left undecided is its own broken rule, whatever the solver says.
                _ => false,
            };
            if !ok && let Some(k) = known_import(want, got, early_ambiguity(s, m, first)) {
                skipped.push(k);
                continue;
            }
            // r5f (name_rules.rs `KNOWN`): a FALSE "not found" from the old solver on an import
            // the reference left for the round after a spared call expanded (R5).
            if !ok
                && matches!((want, got), (Imp::Named(_) | Imp::Glob(_), Some(Unresolved::NotFound)))
                && r.behind_spared.contains(&Bk::Imp(f, i as u32))
                && super::name_rules::KNOWN.iter().any(|(n, _)| *n == "r5f")
            {
                skipped.push("r5f");
                continue;
            }
            // r5g (name_rules.rs `KNOWN`): a FALSE "not found" from the old solver on an import
            // a stall resolves through a later segment taken with the undecided as absent (R5).
            if !ok
                && matches!((want, got), (Imp::Named(_) | Imp::Glob(_), Some(Unresolved::NotFound)))
                && r.middle_stalled.contains(&Bk::Imp(f, i as u32))
                && super::name_rules::KNOWN.iter().any(|(n, _)| *n == "r5g")
            {
                skipped.push("r5g");
                continue;
            }
            // After R6's error (checked above, on the import whose first segment it is) another
            // import that reads through that one follows the order too: the program is rejected
            // and the recovery is not compared (wide seed 225: root `use m1::*` with `mk!`
            // writing `mod m1` beside the extern prelude's `m1`; `use crate::m3::*` in a child
            // resolves through the inner `m1` in the reference's order, is not found through the
            // dependency's in the solver's).
            if !ok && !r6 && !r6_all.is_empty() {
                skipped.push("R6 error, recovery not compared");
                continue;
            }
            if !ok && !r.extern_replacing().is_empty() {
                skipped.push("R8 error, recovery not compared");
                continue;
            }
            if !ok {
                return Err(format!(
                    "the solver {got:?}, the reference {want:?}: import {i} of fragment {f:x} (`{:?}` {:?}) of {m:x}; the reference's calls {:?}, expanded {}",
                    u.path.segments,
                    u.leaf,
                    r.calls.values().collect::<Vec<_>>(),
                    w.expanded.borrow().len()
                ));
            }
        }
    }
    // After R6's error the rule fixes the error, not what the import binds (D56 puts the error at
    // the new definition; the solver may fail the import or keep it, and either side's binding
    // then follows its completion order): the program is rejected, the recovery not compared
    // (wide seed 110: `use m1::*` with `mk!` writing `mod m1` at the root).
    if !r.r6_findings().is_empty() {
        skipped.push("R6 error, recovery not compared");
    }
    // The calls and bindings follow from the imports: after a known import, not compared.
    if !skipped.is_empty() {
        return Ok(skipped);
    }
    for &m in live.iter() {
        let errors = s.errors(m);
        // R9 in type position, on both sides: `str` alone, as a type (`fn f(_: &str)`).
        if let Some(want) = r.type_path(m, BUILTIN) {
            let got = solver_type_path(w, s, m, BUILTIN);
            if Some(want) != got {
                return Err(format!("R9: `{BUILTIN}` in type position in {m:x}: the reference {want:?}, the solver {got:?}"));
            }
        }
        let calls: Vec<&crate::MacroCallDecl<crate::Path<'static, N>>> = r.frags(m).into_iter().flat_map(|f| w.decls[&f].calls.iter()).collect();
        for c in calls {
            let got = errors.iter().find_map(|e| match e {
                ScopeError::Macro { call, why, .. } if *call == c.call => Some(*why),
                _ => None,
            });
            let expanded = w.expanded.borrow().contains(&c.call);
            let want = r.calls.get(&c.call).copied().unwrap_or(Call::Pending);
            let ok = match want {
                Call::Done => expanded && got.is_none(),
                Call::Failed(a) => !expanded && got.is_some_and(|b| same_why(a, b)),
                Call::Pending => false,
            };
            if !ok {
                return Err(format!(
                    "call `{}!` of {m:x}: the reference says {want:?}, the solver expanded {expanded}, error {got:?}",
                    c.path.segments[0]
                ));
            }
        }
        for ns in NSS {
            for &name in names.iter() {
                let want = r.lookup(m, ns, name, None, m).ok().flatten();
                let got = s.binding(m, ns, name).map(|p| B {
                    def: p.def,
                    vis: p.vis,
                    amb: p.state != crate::BindingState::Determined,
                });
                if norm(want) != norm(got) {
                    return Err(format!(
                        "`{name}` in {ns:?}: the reference {:?}, the solver {:?}, of {m:x}",
                        want.map(|b| (b.def.key.crate_prefix()[0], b.def.kind, b.vis, b.amb)),
                        got.map(|b| (b.def.key.crate_prefix()[0], b.def.kind, b.vis, b.amb))
                    ));
                }
            }
        }
    }
    Ok(skipped)
}

fn cold_case(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let (w, root) = draw(tape);
    let s = solve_planted(&w, plant).map_err(|e| Broke::Oracle(format!("the solver failed: {e}")))?;
    compare(&w, root, &s).map_err(|e| Broke::Oracle(format!("differs from the reference: {e}")))
}

/// The failed imports and calls the reference gives, and the comparison with the solver.
fn failures(w: &World, root: ItemKey) -> (Vec<(ItemKey, u32, Unresolved)>, Vec<(ItemKey, Unresolved)>) {
    let s = solve(w, None).expect("solves");
    compare(w, root, &s).expect("equals the reference");
    let mut r = Ref::new(w, root);
    r.run();
    let imports = r.imports.iter().filter_map(|(&(m, i), imp)| match imp {
        Imp::Failed(why) => Some((m, i, *why)),
        Imp::Pending | Imp::Named(_) | Imp::Glob(_) => None,
    });
    let calls = r.calls.iter().filter_map(|(&c, call)| match call {
        Call::Failed(why) => Some((c, *why)),
        Call::Pending | Call::Done => None,
    });
    (imports.collect(), calls.collect())
}

/// `root { macro_rules! mk; mx!(); mk!{ mod m1; } } m1 { mk!{ mod m3; } }`: `mx!` fails (no
/// `macro_rules! mx` before it in the textual scope, and no `mx` in the root's macro
/// namespace), and `m1`'s `mk!` resolves (`macro_rules! mk` stands before it in the textual
/// scope, the Reference's "Textual scope"): a failed call adds nothing, so it does not hold
/// another.
#[test]
fn a_failed_call_before_a_declaring_call_keeps_the_grandchild() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    w.rules(root, "mk");
    w.call(root, "mx", CallShape::FnLike);
    let c = w.call(root, "mk", CallShape::FnLike);
    let m1 = w.module(c, "m1", V::Priv);
    let c2 = w.call(m1, "mk", CallShape::FnLike);
    w.module(c2, "m3", V::Priv);
    let (imports, calls) = failures(&w, root);
    assert!(imports.is_empty(), "{imports:?}");
    assert_eq!(calls.len(), 1, "only `mx!` fails: {calls:?}");
}

/// Fault 3: `root { macro_rules! mk; #[macro_use] mod m1; mk!{ mod m2; } } m1 { mx!(); }
/// m2 { mk!{ mod m3; } }`. The root's `mk!` reads the textual scope back through the
/// `#[macro_use]` child `m1`, so it waits on `m1`'s `mx!`; `mx!` waits on nothing (no
/// `macro_rules! mx` stands before it, and `m1` binds no `mx`), so it fails, and then `mk!`
/// finds `macro_rules! mk` and `m2`'s `mk!` resolves too.
#[test]
fn a_failed_call_in_a_macro_use_child_keeps_the_grandchild() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    w.rules(root, "mk");
    let m1 = w.module_with(root, "m1", V::Priv, true, false);
    let c = w.call(root, "mk", CallShape::FnLike);
    let m2 = w.module(c, "m2", V::Priv);
    let mx = w.call(m1, "mx", CallShape::FnLike);
    let c2 = w.call(m2, "mk", CallShape::FnLike);
    w.module(c2, "m3", V::Priv);
    let (imports, calls) = failures(&w, root);
    assert!(imports.is_empty(), "{imports:?}");
    assert_eq!(calls, vec![(mx, Unresolved::NotFound)]);
}

/// Fault 2: `mod m1 { struct x; } mod m2 { use crate::m2::m0::*; use crate::m2::m1::*;
/// use crate::*; }`. `crate::m2::m1` finds `m1` through `crate::*`; a glob binding is not held
/// by another glob ("items and named imports are allowed to shadow names from glob imports",
/// the Reference's `items.use.glob.shadowing`: a glob never shadows a glob). Then
/// `crate::m2::m0` reads `m2`, `m1` and the root, none of which binds `m0`: one failure.
#[test]
fn a_failed_sibling_glob_frees_the_other() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let m1 = w.module(root, "m1", V::Priv);
    w.item(m1, "x", Namespace::Types, DefKind::Other, V::Priv);
    let m2 = w.module(root, "m2", V::Priv);
    w.glob(m2, "crate::m2::m0", V::Priv);
    w.glob(m2, "crate::m2::m1", V::Priv);
    w.glob(m2, "crate", V::Priv);
    let (imports, calls) = failures(&w, root);
    assert_eq!(imports, vec![(m2, 0, Unresolved::NotFound)]);
    assert!(calls.is_empty(), "{calls:?}");
    let _ = m1;
}

/// A generated crate as Rust source, for reading a disagreement: `mk!`
/// pastes its input, the other macros expand to nothing, a type item is a `type` alias and a
/// value item a `fn`. Written with a stack of jobs, not by recursion.
fn rust_of(w: &World, root: ItemKey) -> String {
    enum Job {
        Body(ItemKey),
        Line(String),
    }
    let vis = |v: &WrittenVis<crate::Path<'static, N>>| -> String {
        match v {
            WrittenVis::Private => String::new(),
            WrittenVis::SelfOnly => "pub(self) ".to_string(),
            WrittenVis::Pub => "pub ".to_string(),
            WrittenVis::Crate => "pub(crate) ".to_string(),
            WrittenVis::Super => "pub(super) ".to_string(),
            WrittenVis::In(p) => format!("pub(in crate{}) ", p.segments.iter().map(|s| format!("::{s}")).collect::<String>()),
            WrittenVis::WithoutIn(_) => unreachable!("the generator writes no `pub(path)`"),
        }
    };
    let mut out = String::from("#![allow(unused, non_camel_case_types, unused_macros)]\nmacro_rules! mk { ($($t:tt)*) => { $($t)* } }\n");
    let cr = &w.roots[&w.modules[&root].krate];
    if !cr.extern_prelude.is_empty() || cr.head.prelude.is_some() {
        out.push_str(&format!(
            "// extern prelude {:?} (the wide distribution's dependency: `pub struct x; struct z; pub mod m2 {{ pub struct y; }}`); standard prelude `m1 {{ pub struct y; }}`, `m3 {{ pub struct x; }}`: {}\n",
            cr.extern_prelude.iter().map(|e| e.0).collect::<Vec<_>>(),
            cr.head.prelude.is_some()
        ));
    }
    let mut jobs = vec![Job::Line("fn main() {}\n".to_string()), Job::Body(root)];
    while let Some(job) = jobs.pop() {
        let at = match job {
            Job::Line(s) => {
                out.push_str(&s);
                continue;
            }
            Job::Body(at) => at,
        };
        let Some(d) = w.decls.get(&at) else { continue };
        let mut evs: Vec<(u32, Vec<Job>)> = Vec::new();
        for it in d.items.iter() {
            let s = match it.ns {
                Namespace::Types => format!("{}type {} = u8;\n", vis(&it.vis), it.name),
                _ => format!("{}fn {}() {{}}\n", vis(&it.vis), it.name),
            };
            evs.push((0, vec![Job::Line(s)]));
        }
        for ec in d.extern_crates.iter() {
            let (crate::ExternTarget::Dep(t), Rename::To(n)) = (ec.target, ec.rename) else { continue };
            evs.push((0, vec![Job::Line(format!("{}extern crate {t} as {n};\n", vis(&ec.vis)))]));
        }
        for c in d.mods.iter() {
            let attr = if c.macro_use { "#[macro_use] " } else { "" };
            let head = format!("{attr}{}mod {} {{\n", vis(&c.vis), c.name);
            evs.push((c.pos, vec![Job::Line(head), Job::Body(c.module), Job::Line("}\n".to_string())]));
        }
        for u in d.uses.iter() {
            let mut p = match u.path.kind {
                PathKind::Crate => "crate".to_string(),
                PathKind::SelfMod => "self".to_string(),
                PathKind::Super(n) => vec!["super"; n as usize].join("::"),
                // A lexical first segment: the segments alone (the `::` before it is cut below).
                PathKind::Plain => String::new(),
                _ => unreachable!("the generator writes crate, self, super and plain paths"),
            };
            let plain = u.path.kind == PathKind::Plain;
            let segs = &u.path.segments[..];
            let s = match u.leaf {
                UseForm::Glob => {
                    for s in segs {
                        p.push_str(&format!("::{s}"));
                    }
                    let p = if plain { &p[2..] } else { &p[..] };
                    format!("{}use {p}::*;\n", vis(&u.vis))
                }
                UseForm::Named { rename, .. } => {
                    for s in segs {
                        p.push_str(&format!("::{s}"));
                    }
                    let to = match rename {
                        Rename::Same => String::new(),
                        Rename::To(t) => format!(" as {t}"),
                        Rename::Underscore => " as _".to_string(),
                    };
                    let p = if plain { &p[2..] } else { &p[..] };
                    format!("{}use {p}{to};\n", vis(&u.vis))
                }
            };
            evs.push((u.pos, vec![Job::Line(s)]));
        }
        for r in d.macro_rules.iter() {
            if r.name == "mk" {
                continue;
            }
            let attr = if r.export { "#[macro_export] " } else { "" };
            evs.push((r.pos, vec![Job::Line(format!("{attr}macro_rules! {} {{ () => {{}} }}\n", r.name))]));
        }
        for c in d.calls.iter() {
            let name = c.path.segments.last().copied().unwrap_or("?");
            let job = if w.decls.contains_key(&c.call) {
                vec![Job::Line(format!("{name}! {{\n")), Job::Body(c.call), Job::Line("}\n".to_string())]
            } else {
                vec![Job::Line(format!("{name}!();\n"))]
            };
            evs.push((c.pos, job));
        }
        evs.sort_by_key(|e| e.0);
        for (_, js) in evs.into_iter().rev() {
            jobs.extend(js.into_iter().rev());
        }
    }
    out
}

/// Fault 1: `mod m1 {} mod m2 { use crate::m3::*; pub use crate::*; use self::m0::*; }
/// mod m3 { pub use crate::m2::m1::*; }`. `m1` in `m2` is bound through `pub use crate::*`, and
/// a glob binding never waits on another glob (`items.use.glob.shadowing`: only items and named
/// imports shadow a glob), so `m3`'s glob resolves; `self::m0::*` reads `m2`, `m3` and the
/// root, none binds `m0`: one failure.
#[test]
fn a_found_glob_binding_does_not_wait_on_a_pending_glob() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let m1 = w.module(root, "m1", V::Priv);
    let m2 = w.module(root, "m2", V::Priv);
    let m3 = w.module(root, "m3", V::Priv);
    w.glob(m2, "crate::m3", V::Priv);
    w.glob(m2, "crate", V::Pub);
    w.glob(m2, "self::m0", V::Priv);
    w.glob(m3, "crate::m2::m1", V::Pub);
    let (imports, calls) = failures(&w, root);
    assert_eq!(imports, vec![(m2, 2, Unresolved::NotFound)]);
    assert!(calls.is_empty(), "{calls:?}");
    let _ = (m1, m3);
}

/// The rule-4 guard: `mod m1 {} mod m2 { use crate::m3::*; pub use crate::*; }
/// mod m3 { pub use crate::m2::m1::*; }` is a program rustc accepts: `m3`'s glob finds `m1`
/// through `m2`'s `pub use crate::*` while `m2`'s other glob waits on it. No error.
#[test]
fn a_glob_cycle_through_a_found_binding_resolves() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    w.module(root, "m1", V::Priv);
    let m2 = w.module(root, "m2", V::Priv);
    let m3 = w.module(root, "m3", V::Priv);
    w.glob(m2, "crate::m3", V::Priv);
    w.glob(m2, "crate", V::Pub);
    w.glob(m3, "crate::m2::m1", V::Pub);
    let (imports, calls) = failures(&w, root);
    assert!(imports.is_empty() && calls.is_empty(), "{imports:?} {calls:?}");
}

/// `mod m1 { pub struct x; } mod m2 { pub struct x; } mod m3 { pub use crate::m1::*;
/// pub use crate::m2::*; }`, and `used` adds `use crate::m3::x;` at the root. Two globs bring
/// different items under `x`: an ambiguity, an error only where the name is used (the
/// Reference, `items.use.glob.shadowing`), whatever order the globs resolve in.
fn two_globs(used: bool) -> (Vec<(ItemKey, u32, Unresolved)>, ItemKey) {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let m1 = w.module(root, "m1", V::Priv);
    let m2 = w.module(root, "m2", V::Priv);
    let m3 = w.module(root, "m3", V::Priv);
    w.item(m1, "x", Namespace::Types, DefKind::Other, V::Pub);
    w.item(m2, "x", Namespace::Types, DefKind::Other, V::Pub);
    w.glob(m3, "crate::m1", V::Pub);
    w.glob(m3, "crate::m2", V::Pub);
    if used {
        w.use_(root, "crate::m3::x", V::Priv);
    }
    let (imports, calls) = failures(&w, root);
    assert!(calls.is_empty(), "{calls:?}");
    (imports, root)
}

#[test]
fn two_globs_bringing_different_items_are_an_error_where_used() {
    let (imports, root) = two_globs(true);
    assert_eq!(imports, vec![(root, 0, Unresolved::Ambiguous(crate::AmbiguityKind::GlobVsGlob))]);
}

#[test]
fn two_globs_bringing_different_items_are_no_error_unused() {
    let (imports, _) = two_globs(false);
    assert!(imports.is_empty(), "{imports:?}");
}

/// Replays one tape of the oracle with its world, the reference's state and the comparison
/// printed: `ORACLE_TAPE='1 0*8 2' cargo test replay_one_tape` (does nothing without it).
#[test]
fn replay_one_tape() {
    let Ok(t) = std::env::var("ORACLE_TAPE") else { return };
    let values: Vec<u64> = t
        .split(' ')
        .flat_map(|p| match p.split_once('*') {
            Some((v, n)) => vec![v.parse().unwrap(); n.parse().unwrap()],
            None => vec![p.parse().unwrap()],
        })
        .collect();
    let tape = Tape::replay(&values);
    // `ORACLE_DIST=wide` or `nocalls` replays a tape of the wide or the no-calls distribution.
    let (w, root) = match std::env::var("ORACLE_DIST").as_deref() {
        Ok("wide") => draw_in(&tape, true, true),
        Ok("deep") => draw_deep(&tape),
        Ok("nocalls") => draw_in(&tape, true, false),
        _ => draw(&tape),
    };
    let mut report = String::new();
    for (m, d) in w.decls.iter() {
        report.push_str(&format!("decls {m:x}: mods {:?}\n uses {:?}\n items {:?}\n rules {:?}\n calls {:?}\n", d.mods, d.uses, d.items, d.macro_rules, d.calls));
    }
    report.push_str(&format!("rust:\n{}", rust_of(&w, root)));
    let mut r = Ref::new(&w, root);
    r.run();
    report.push_str(&format!("ref imports {:?}\nref calls {:?}\n", r.imports, r.calls));
    let s = solve(&w, None);
    report.push_str(&format!("compare: {:?}\n", s.map(|s| compare(&w, root, &s))));
    // `ORACLE_OUT=<file>` keeps the whole report (a checker may cut the test's output).
    if let Ok(out) = std::env::var("ORACLE_OUT") {
        std::fs::write(&out, &report).expect("ORACLE_OUT is writable");
    }
    std::println!("{report}");
    panic!("debug output above");
}

/// The cases every test run takes.
const CASES: u32 = 2000;
/// The long run, by hand.
const FULL_CASES: u32 = 20000;

#[test]
fn a_cold_fill_equals_the_plain_reference() {
    blaze_sim::check(Config::cases(CASES), |tape| cold_case(tape, Plant::None));
}

#[test]
#[ignore = "long: ten times the cases, run by hand with --include-ignored"]
fn a_cold_fill_equals_the_plain_reference_in_full() {
    blaze_sim::check(Config::cases(FULL_CASES), |tape| cold_case(tape, Plant::None));
}

/// The plant's failure, shrunk, for quoting: run by hand with `--include-ignored`; it fails by
/// design and prints the least tape (`ORACLE_TAPE` with `replay_one_tape` prints it as Rust).
#[test]
#[ignore = "fails by design: prints the planted bug's shrunk tape"]
fn the_planted_leak_shrunk() {
    blaze_sim::check(Config::cases(CASES), |tape| cold_case(tape, Plant::GlobLeaksPrivate));
}

/// A case of the wide distribution, the reference carrying `bug`.
fn wide_case(tape: &Tape, bug: Option<Bug>) -> Result<(), Broke> {
    dist_case(tape, true, bug)
}

/// A case of the no-calls distribution, the reference carrying `bug`.
fn nocalls_case(tape: &Tape, bug: Option<Bug>) -> Result<(), Broke> {
    dist_case(tape, false, bug)
}

/// A case of the deep distribution (`draw_deep`), the reference carrying `bug`.
fn deep_case(tape: &Tape, bug: Option<Bug>) -> Result<(), Broke> {
    world_case(draw_deep(tape), bug)
}

fn dist_case(tape: &Tape, calls: bool, bug: Option<Bug>) -> Result<(), Broke> {
    world_case(draw_in(tape, true, calls), bug)
}

fn world_case((w, root): (World, ItemKey), bug: Option<Bug>) -> Result<(), Broke> {
    let s = match solve(&w, None) {
        Ok(s) => s,
        // r3b (`known_run`): no answer to compare a planted reference against.
        Err(e) if e.contains(R3B) && super::name_rules::KNOWN.iter().any(|(n, _)| *n == "r3b") => return Ok(()),
        Err(e) => return Err(Broke::Oracle(format!("the solver failed: {e}"))),
    };
    compare_known(&w, root, &s, bug).map(|_| ()).map_err(|e| Broke::Oracle(format!("differs from the reference: {e}")))
}

/// The wide distribution (a dependency, the extern and standard preludes, lexical first
/// segments, `mk!` writing items): the solver equals the reference. A disagreement the written
/// rule pins on the solver goes into name_rules.rs `KNOWN`.
#[test]
#[ignore = "GAP G8 (cells-core.md runner 19), NOT ATTRIBUTED: in a drawn world with dependencies, an import is left unknown (ORACLE_TAPE='1 0*12 1 0 0 0 1 3 1 0*10'): the cells or the tests' dependency reader; cells/tests.rs `the_cells_equal_the_reference_wide*` pass"]
fn a_wide_cold_fill_equals_the_plain_reference() {
    known_run("wide", true);
}

/// The deep distribution (macros writing imports, items, `extern crate` and further calls,
/// nested three deep; `str` beside a module): the solver equals the reference.
#[test]
#[ignore = "CELLS GAP G6 (cells-core.md runner 19): on a deep world the cells leave an import or call unknown (ORACLE_TAPE='4 0*17 1 0*18 1 3 0*33')"]
fn a_deep_cold_fill_equals_the_plain_reference() {
    known_run("deep", true);
}

/// The old solver's failure of class r3b, as its error says it.
const R3B: &str = "its scope read against its fellows' records differs from its group's";

/// The 2,000 cases of the wide (`calls`) or the no-calls distribution, counting the cases each
/// `KNOWN` class (`known_import`) skipped; any other difference fails.
fn known_run(label: &str, calls: bool) {
    let mut seen: BTreeMap<&'static str, u32> = BTreeMap::new();
    // A search, not `check`: a failure prints its text and its least tape as `ORACLE_TAPE`
    // takes it, for `replay_one_tape` (with `ORACLE_DIST={label}`).
    let verdict = blaze_sim::search(Config::cases(CASES), |tape| {
        let (w, root) = if label == "deep" { draw_deep(tape) } else { reference_world(tape, true, calls) };
        let s = match solve(&w, None) {
            Ok(s) => s,
            // r3b (name_rules.rs `KNOWN`): the solver's own group check fails, no answer.
            Err(e) if e.contains(R3B) && super::name_rules::KNOWN.iter().any(|(n, _)| *n == "r3b") => {
                *seen.entry("r3b").or_default() += 1;
                return Ok(());
            }
            Err(e) => return Err(Broke::Oracle(format!("the solver failed: {e}"))),
        };
        let k = compare_known(&w, root, &s, None).map_err(|e| Broke::Oracle(format!("differs from the reference: {e}")))?;
        for c in k.into_iter().collect::<BTreeSet<_>>() {
            *seen.entry(c).or_default() += 1;
        }
        Ok::<(), Broke>(())
    });
    if let blaze_sim::Verdict::Failed(f) = verdict {
        let tape: Vec<String> = f.least.choices.iter().map(|c| c.value.to_string()).collect();
        // With `ORACLE_OUT`, the whole tape is kept in the file (a checker cuts a long line).
        if std::env::var("ORACLE_OUT").is_ok() {
            keep(&std::format!("{label}: FAILED\nORACLE_TAPE='{}'", tape.join(" ")));
        }
        panic!("{label}: {}\nORACLE_TAPE='{}'", f.failure.text, tape.join(" "));
    }
    keep(&std::format!("{label}: {CASES} cases; cases with a KNOWN difference, by class: {seen:?}"));
}

/// Prints a proof line and, with `ORACLE_OUT=<file>`, appends it there (a checker cuts a passing
/// test's output; the summaries, KNOWN verdicts and planted catches are quoted from the file).
pub(super) fn keep(line: &str) {
    std::println!("{line}");
    if let Ok(out) = std::env::var("ORACLE_OUT") {
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&out).expect("ORACLE_OUT is writable");
        std::writeln!(f, "{line}").expect("ORACLE_OUT is writable");
    }
}

/// The no-calls distribution a core without expansion is judged by (`reference_world`).
#[test]
#[ignore = "GAP G8 (cells-core.md runner 19), NOT ATTRIBUTED: as the wide comparison (ORACLE_TAPE='2 0*8 1 0 0 0 2 0*17 1 0 0 0 1 3 1 0*7')"]
fn a_cold_fill_without_calls_equals_the_plain_reference() {
    known_run("no calls", false);
}

/// Every planted reference bug (one per rule it states) is caught by the comparison.
#[test]
fn every_planted_reference_bug_is_caught() {
    for bug in [
        Bug::R1NamedLosesToGlob,
        Bug::R2GlobsNeverClash,
        Bug::R2BoundStopsWaiting,
        Bug::R3OnePass,
        Bug::R4GlobIgnoresPrivacy,
        Bug::R6WaitsForExpansion,
        Bug::R7GlobBeatsOuter,
        Bug::R8StdBeforeExtern,
        Bug::R5NoStall,
        Bug::R5SparesAll,
        Bug::R5StallFailsMiddle,
        Bug::ExpansionImportsIgnored,
        Bug::NestedNotWalked,
        Bug::R6IgnoresEnclosure,
        Bug::R8MacroReplaces,
        Bug::R9ModuleIsTheType,
        Bug::R6IgnoresStanding,
        Bug::ExternReexportNarrowed,
    ] {
        // The clean runs are the two comparisons above (both green). A planted bug is caught
        // where the old solver is right: R7's glob-vs-outer is mostly a KNOWN class of the old
        // solver (r7d, r7f, r7g), so the no-calls distribution is searched too. The first catch
        // is printed, for quoting which difference caught which bug.
        let cases: [(&str, fn(&Tape, Option<Bug>) -> Result<(), Broke>); 3] = [("wide", wide_case), ("no calls", nocalls_case), ("deep", deep_case)];
        let caught = cases.iter().find_map(|&(label, case)| match blaze_sim::search(Config::cases(CASES).shrink_runs(0), |tape| case(tape, Some(bug))) {
            blaze_sim::Verdict::Failed(f) if f.failure.text.contains("differs from the reference") => {
                Some(std::format!("{label}: {}", f.failure.text.lines().next().unwrap_or("")))
            }
            _ => None,
        });
        match caught {
            Some(line) => keep(&std::format!("planted {bug:?} caught by the {line}")),
            None => panic!("the comparison missed the planted {bug:?} in {CASES} cases of either distribution"),
        }
    }
}

#[test]
fn a_glob_that_brings_in_private_items_is_caught() {
    blaze_sim::control(Config::cases(CASES), Plant::None, Plant::GlobLeaksPrivate, "differs from the reference", cold_case);
}
