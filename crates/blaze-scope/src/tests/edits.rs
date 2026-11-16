//! The edit-sequence property: a generated small crate, a generated sequence of edits and a
//! generated schedule; after every edit, the records resolved again from the previous ones are
//! byte-equal to a cold fill of the same text. The crate is a module tree with public, crate
//! and private items in the three namespaces, named imports (renamed or not), glob imports and
//! re-exports (which form chains and cycles at will, and shadow glob names by local ones), and
//! `macro_rules!` (exported or not); an edit adds, removes or renames an item, flips a
//! visibility, adds or removes an import, a glob, a module or a `macro_rules!`.

use std::fmt::Write as _;
use std::string::String;
use std::vec::Vec;
use std::{format, vec};

use blaze_id::{ContentId, ItemKey};
use blaze_own::{Alloc, Vec as OwnVec};
use blaze_sim::{Broke, Config, Tape};

use super::{N, Solved, V, World, crate_key, resolve_again, same, solve};
use crate::resolver::Plant;
use crate::{
    CallShape, ChildModDecl, DeclaredItem, DeclsBuilder, Def, DefKind, Edition, MacroRulesDecl, ModuleInfo, Namespace,
    Path, PathKind, Rename, UseDecl, UseForm,
};

/// The names items and imports take; `m` and `n` are module names too, so an item or an import
/// can collide with a module and a glob can bring a module in.
const NAMES: [N; 5] = ["x", "y", "z", "m", "n"];
const MODULE_NAMES: [N; 3] = ["m", "n", "k"];
const VIS: [V; 3] = [V::Priv, V::Pub, V::Crate];

#[derive(Clone, Debug)]
struct Item {
    id: u32,
    name: N,
    ns: Namespace,
    vis: V,
}

#[derive(Clone, Debug)]
struct Import {
    id: u32,
    /// The module the path names, an index into [`Spec::mods`].
    to: usize,
    /// The name it imports; a glob when none.
    leaf: Option<N>,
    rename: Option<N>,
    vis: V,
}

#[derive(Clone, Debug)]
struct Rules {
    id: u32,
    name: N,
    export: bool,
}

#[derive(Clone, Debug)]
struct Mod {
    id: u32,
    parent: Option<usize>,
    name: N,
    vis: V,
    alive: bool,
    items: Vec<Item>,
    uses: Vec<Import>,
    rules: Vec<Rules>,
}

/// A crate's text: the modules, the root first; a removed module stays, not alive, so an
/// import's path still names it.
#[derive(Clone, Debug)]
struct Spec {
    mods: Vec<Mod>,
    next: u32,
}

fn key(tag: u8, id: u32) -> ItemKey {
    let mut bytes = [0u8; 32];
    bytes[..8].fill(1);
    bytes[12] = tag;
    bytes[13..17].copy_from_slice(&id.to_be_bytes());
    ItemKey(ContentId(bytes))
}

fn vis_text(v: V) -> &'static str {
    match v {
        V::Priv => "",
        V::Pub => "pub ",
        V::Crate => "pub(crate) ",
        V::Super => "pub(super) ",
        V::In(_) => "pub(in ..) ",
    }
}

impl Spec {
    fn fresh(&mut self) -> u32 {
        self.next += 1;
        self.next
    }

    /// Whether module `i` and every ancestor are alive.
    fn live(&self, i: usize) -> bool {
        let mut at = Some(i);
        while let Some(m) = at {
            if !self.mods[m].alive {
                return false;
            }
            at = self.mods[m].parent;
        }
        true
    }

    fn live_mods(&self) -> Vec<usize> {
        (0..self.mods.len()).filter(|&i| self.live(i)).collect()
    }

    /// The names from the root down to module `i`.
    fn names_to(&self, i: usize) -> Vec<N> {
        let mut out = Vec::new();
        let mut at = i;
        while let Some(p) = self.mods[at].parent {
            out.push(self.mods[at].name);
            at = p;
        }
        out.reverse();
        out
    }

    /// The crate as Rust-like text, for a failure's message.
    fn show(&self) -> String {
        let mut out = String::new();
        let mut stack = vec![(0usize, 0usize)];
        while let Some((i, depth)) = stack.pop() {
            let m = &self.mods[i];
            let pad = "  ".repeat(depth);
            if i != 0 {
                let _ = writeln!(out, "{pad}{}mod {} {{ // #{}", vis_text(m.vis), m.name, m.id);
            }
            let inner = "  ".repeat(depth + usize::from(i != 0));
            for it in &m.items {
                let kind = match it.ns {
                    Namespace::Types => "struct",
                    Namespace::Values => "fn",
                    Namespace::Macros => "macro",
                };
                let _ = writeln!(out, "{inner}{}{kind} {}; // #{}", vis_text(it.vis), it.name, it.id);
            }
            for u in &m.uses {
                let mut p = String::from("crate");
                for s in self.names_to(u.to) {
                    p.push_str("::");
                    p.push_str(s);
                }
                let leaf = match (u.leaf, u.rename) {
                    (None, _) => String::from("::*"),
                    (Some(l), None) => format!("::{l}"),
                    (Some(l), Some(r)) => format!("::{l} as {r}"),
                };
                let _ = writeln!(out, "{inner}{}use {p}{leaf}; // #{}", vis_text(u.vis), u.id);
            }
            for r in &m.rules {
                let export = if r.export { "#[macro_export] " } else { "" };
                let _ = writeln!(out, "{inner}{export}macro_rules! {} {{}} // #{}", r.name, r.id);
            }
            let mut kids: Vec<usize> = (0..self.mods.len()).filter(|&c| self.mods[c].parent == Some(i) && self.mods[c].alive).collect();
            kids.reverse();
            for c in kids {
                stack.push((c, depth + usize::from(i != 0)));
            }
            if i != 0 {
                let _ = writeln!(out, "{pad}}}");
            }
        }
        out
    }

    /// The crate's records.
    fn build(&self) -> World {
        let g = Alloc::global();
        let mut w = World::new();
        let root = w.root(1, Edition::E2021);
        let keys: Vec<ItemKey> = self.mods.iter().enumerate().map(|(i, m)| if i == 0 { root } else { key(1, m.id) }).collect();
        let live = self.live_mods();
        for &i in &live {
            if i == 0 {
                continue;
            }
            let m = &self.mods[i];
            let parent = keys[m.parent.expect("a module other than the root has a parent")];
            w.modules.insert(keys[i], ModuleInfo { krate: crate_key(1), parent: Some(parent), name: Some(m.name), no_implicit_prelude: false });
            w.decls.insert(keys[i], DeclsBuilder::new_in(g));
        }
        for &i in &live {
            let m = &self.mods[i];
            let mut d = DeclsBuilder::new_in(g);
            for &c in &live {
                let child = &self.mods[c];
                if child.parent == Some(i) {
                    d.mods.push(ChildModDecl { name: child.name, module: keys[c], vis: child.vis.decl(), macro_use: false, pos: child.id });
                }
            }
            for it in &m.items {
                let kind = match it.ns {
                    Namespace::Macros => DefKind::Macro(crate::MacroShapes::FN_LIKE),
                    Namespace::Types | Namespace::Values => DefKind::Other,
                };
                d.items.push(DeclaredItem { name: it.name, ns: it.ns, def: Def { key: key(2, it.id), kind }, vis: it.vis.decl() });
            }
            for u in &m.uses {
                let mut segments = OwnVec::from_iter_in(self.names_to(u.to), g);
                let leaf = match u.leaf {
                    Some(l) => {
                        segments.push(l);
                        let rename = u.rename.map_or(Rename::Same, Rename::To);
                        UseForm::Named { rename, types_only: false }
                    }
                    None => UseForm::Glob,
                };
                d.uses.push(UseDecl { path: Path { kind: PathKind::Crate, segments }, leaf, vis: u.vis.decl(), pos: u.id });
            }
            for r in &m.rules {
                d.macro_rules.push(MacroRulesDecl { name: r.name, def: key(3, r.id), pos: r.id, export: r.export, shapes: crate::MacroShapes::FN_LIKE });
            }
            w.decls.insert(keys[i], d);
        }
        w
    }

    fn draw_item(&mut self, tape: &Tape) -> (usize, Item) {
        let live = self.live_mods();
        let at = *tape.one_of(&live, "item's module");
        let ns = *tape.one_of(&[Namespace::Values, Namespace::Types, Namespace::Macros], "item's namespace");
        let item = Item { id: self.fresh(), name: *tape.one_of(&NAMES, "item's name"), ns, vis: *tape.one_of(&VIS, "item's visibility") };
        (at, item)
    }

    fn draw_use(&mut self, tape: &Tape) -> (usize, Import) {
        let live = self.live_mods();
        let at = *tape.one_of(&live, "import's module");
        // Any module, a removed one too: its path then names nothing.
        let to = tape.index(self.mods.len(), "import's target");
        let leaf = if tape.coin("a glob") { None } else { Some(*tape.one_of(&NAMES, "imported name")) };
        let rename = if leaf.is_some() && tape.coin("renamed") { Some(*tape.one_of(&NAMES, "rename")) } else { None };
        let u = Import { id: self.fresh(), to, leaf, rename, vis: *tape.one_of(&VIS, "import's visibility") };
        (at, u)
    }

    fn draw_mod(&mut self, tape: &Tape) -> usize {
        let live = self.live_mods();
        let parent = *tape.one_of(&live, "module's parent");
        let id = self.fresh();
        let m = Mod {
            id,
            parent: Some(parent),
            name: *tape.one_of(&MODULE_NAMES, "module's name"),
            vis: *tape.one_of(&VIS, "module's visibility"),
            alive: true,
            items: Vec::new(),
            uses: Vec::new(),
            rules: Vec::new(),
        };
        self.mods.push(m);
        self.mods.len() - 1
    }

    fn generate(tape: &Tape) -> Spec {
        let root = Mod { id: 0, parent: None, name: "crate", vis: V::Pub, alive: true, items: Vec::new(), uses: Vec::new(), rules: Vec::new() };
        let mut s = Spec { mods: vec![root], next: 0 };
        for _ in 0..tape.below(5, "modules") {
            s.draw_mod(tape);
        }
        for _ in 0..tape.below(7, "items") {
            let (at, it) = s.draw_item(tape);
            s.mods[at].items.push(it);
        }
        for _ in 0..tape.below(7, "imports") {
            let (at, u) = s.draw_use(tape);
            s.mods[at].uses.push(u);
        }
        for _ in 0..tape.below(3, "macro_rules") {
            let live = s.live_mods();
            let at = *tape.one_of(&live, "macro_rules' module");
            let r = Rules { id: s.fresh(), name: *tape.one_of(&NAMES, "macro_rules' name"), export: tape.coin("exported") };
            s.mods[at].rules.push(r);
        }
        s
    }

    /// One drawn edit, said in words; `None` when there was nothing to edit of the drawn kind.
    fn edit(&mut self, tape: &Tape) -> Option<String> {
        let live = self.live_mods();
        let pick = |what: &[(usize, usize)], label: &'static str| -> Option<(usize, usize)> {
            if what.is_empty() { None } else { Some(what[tape.index(what.len(), label)]) }
        };
        let items: Vec<(usize, usize)> = live.iter().flat_map(|&m| (0..self.mods[m].items.len()).map(move |i| (m, i))).collect();
        let uses: Vec<(usize, usize)> = live.iter().flat_map(|&m| (0..self.mods[m].uses.len()).map(move |i| (m, i))).collect();
        let rules: Vec<(usize, usize)> = live.iter().flat_map(|&m| (0..self.mods[m].rules.len()).map(move |i| (m, i))).collect();
        match tape.weighted(&[3, 2, 2, 1, 1, 3, 2, 1, 1, 1, 1], "edit kind") {
            0 => {
                let (at, it) = self.draw_item(tape);
                let said = format!("add item #{} {} in module #{}", it.id, it.name, self.mods[at].id);
                self.mods[at].items.push(it);
                Some(said)
            }
            1 => {
                let (m, i) = pick(&items, "item removed")?;
                let it = self.mods[m].items.remove(i);
                Some(format!("remove item #{}", it.id))
            }
            2 => {
                let (m, i) = pick(&items, "item renamed")?;
                let name = *tape.one_of(&NAMES, "new name");
                let it = &mut self.mods[m].items[i];
                it.name = name;
                Some(format!("rename item #{} to {name}", it.id))
            }
            3 => {
                let (m, i) = pick(&items, "item's visibility flipped")?;
                let v = *tape.one_of(&VIS, "new visibility");
                let it = &mut self.mods[m].items[i];
                it.vis = v;
                Some(format!("item #{} becomes {}", it.id, vis_text(v)))
            }
            4 => {
                let (m, i) = pick(&uses, "import's visibility flipped")?;
                let v = *tape.one_of(&VIS, "new visibility");
                let u = &mut self.mods[m].uses[i];
                u.vis = v;
                Some(format!("import #{} becomes {}", u.id, vis_text(v)))
            }
            5 => {
                let (at, u) = self.draw_use(tape);
                let said = format!("add import #{} in module #{}", u.id, self.mods[at].id);
                self.mods[at].uses.push(u);
                Some(said)
            }
            6 => {
                let (m, i) = pick(&uses, "import removed")?;
                let u = self.mods[m].uses.remove(i);
                Some(format!("remove import #{}", u.id))
            }
            7 => {
                let i = self.draw_mod(tape);
                Some(format!("add module #{} {}", self.mods[i].id, self.mods[i].name))
            }
            8 => {
                let removable: Vec<usize> = live.iter().copied().filter(|&m| m != 0).collect();
                if removable.is_empty() {
                    return None;
                }
                let m = removable[tape.index(removable.len(), "module removed")];
                self.mods[m].alive = false;
                Some(format!("remove module #{}", self.mods[m].id))
            }
            9 => {
                let at = *tape.one_of(&live, "macro_rules' module");
                let r = Rules { id: self.fresh(), name: *tape.one_of(&NAMES, "macro_rules' name"), export: tape.coin("exported") };
                let said = format!("add macro_rules! #{} {}", r.id, r.name);
                self.mods[at].rules.push(r);
                Some(said)
            }
            _ => {
                let (m, i) = pick(&rules, "macro_rules removed")?;
                let r = self.mods[m].rules.remove(i);
                Some(format!("remove macro_rules! #{}", r.id))
            }
        }
    }
}

/// The modules whose text an edit changed, or that it added.
fn edited(before: &World, after: &World) -> Vec<ItemKey> {
    after.decls.iter().filter(|(m, d)| before.decls.get(*m) != Some(*d)).map(|(m, _)| *m).collect()
}

/// One case: a crate, then up to four edits, each resolved again from the last records under a
/// drawn schedule, and held byte-equal to a cold fill of the edited crate.
fn sequence_case(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let mut spec = Spec::generate(tape);
    let mut world = spec.build();
    let mut old: Solved = solve(&world, Some(tape), plant)
        .map_err(|e| Broke::Harness(format!("{e}\nthe crate: {}", spec.show().replace('\n', " "))))?;
    let mut said: Vec<String> = Vec::new();
    for _ in 0..1 + tape.below(4, "edits") {
        let mut next = spec.clone();
        let Some(what) = next.edit(tape) else { continue };
        said.push(what);
        let after = next.build();
        let changed = edited(&world, &after);
        let cold = solve(&after, Some(tape), plant);
        let again = resolve_again(&world, &after, &old, &changed, Some(tape), plant).map(|a| a.solved);
        if let Err(e) = same(&again, &cold) {
            return Err(Broke::Oracle(format!(
                "after the edits [{}] the records differ from the cold fill\nthe crate after: {}\n{e}",
                said.join("; "),
                next.show().replace('\n', " ")
            )));
        }
        old = match again {
            Ok(s) => s,
            Err(e) => {
                return Err(Broke::Harness(format!(
                    "after the edits [{}]: {e}\nthe crate after: {}",
                    said.join("; "),
                    next.show().replace('\n', " ")
                )));
            }
        };
        world = after;
        spec = next;
    }
    Ok(())
}

/// `root { fn x × dups; use crate::x × dups; use crate::m::y; mod m { use crate::*; } }`: `y`
/// could only come around the cycle root → `m`'s glob → root. By the Reference ("Use
/// declarations"), a glob imports the names its source has, and the names are the least
/// fixpoint of the imports: nothing declares `y`, so nothing has it, the import of it is not
/// found, and `m`'s glob, which brings what root has, is no error. Found by the edit property
/// (seed 44).
fn glob_cycle_world(dups: usize) -> World {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    for _ in 0..dups {
        w.fun(root, "x", V::Pub);
    }
    for _ in 0..dups {
        w.use_(root, "crate::x", V::Pub);
    }
    w.use_(root, "crate::m::y", V::Pub);
    let m = w.module(root, "m", V::Pub);
    w.glob(m, "crate", V::Pub);
    w
}

/// The case as the property drew it (seed 44), every item private.
fn seed_44(fns: &[u32]) -> World {
    let item = |id| Item { id, name: "x", ns: Namespace::Values, vis: V::Priv };
    let import = |id, to, leaf| Import { id, to, leaf, rename: None, vis: V::Priv };
    let root = Mod {
        id: 0,
        parent: None,
        name: "crate",
        vis: V::Pub,
        alive: true,
        items: fns.iter().map(|&i| item(i)).collect(),
        uses: vec![import(7, 0, Some("x")), import(9, 0, Some("x")), import(10, 1, Some("y"))],
        rules: Vec::new(),
    };
    let m = Mod { id: 1, parent: Some(0), name: "m", vis: V::Priv, alive: true, items: Vec::new(), uses: vec![import(8, 0, None)], rules: Vec::new() };
    Spec { mods: vec![root, m], next: 11 }.build()
}

#[test]
fn an_import_found_only_around_its_glob_cycle_is_one_error() {
    let worlds = [glob_cycle_world(0), glob_cycle_world(1), glob_cycle_world(6), seed_44(&[2, 3, 4, 5, 6, 11]), seed_44(&[2]), seed_44(&[])];
    let calm: Vec<Solved> = worlds.iter().map(|w| solve(w, None, Plant::None).unwrap_or_else(|e| panic!("{e}"))).collect();
    for (i, (w, s)) in worlds.iter().zip(calm.iter()).enumerate() {
        let root = w.modules.iter().find(|(_, info)| info.parent.is_none()).map(|(&k, _)| k).expect("a root");
        let m = w.modules.iter().find(|(_, info)| info.parent == Some(root)).map(|(&k, _)| k).expect("module m");
        let not_found = s.errors(root).iter().filter(|e| {
            matches!(e, crate::ScopeError::Import { segment: 1, why: crate::Unresolved::NotFound, .. })
        });
        assert_eq!(not_found.count(), 1, "world {i}: root's `use crate::m::y` is one NotFound: {:?}", s.errors(root));
        for ns in Namespace::ALL {
            assert_eq!(s.def(m, ns, "y"), None, "world {i}: m has no `y`");
            assert_eq!(s.def(root, ns, "y"), None, "world {i}: root has no `y`");
        }
        assert!(s.errors(m).is_empty(), "world {i}: m's glob is no error: {:?}", s.errors(m));
    }
    blaze_sim::check(Config::cases(300), |tape| {
        let i = tape.index(worlds.len(), "world");
        let got = solve(&worlds[i], Some(tape), Plant::None);
        let want = Ok(Solved { by: calm[i].by.clone(), krate: calm[i].krate.clone(), chain: calm[i].chain.clone() });
        same(&got, &want).map_err(|e| Broke::Oracle(format!("world {i}: a schedule differs from the calm run: {e}")))
    });
}

/// `root { fn x × fns; use crate::m::z; use crate::m as y; mod m { use crate::y as z; } }`,
/// every item private. `m`'s `z` names `m` itself through root's `y`: it exists, private to
/// `m`, so root's import of it is private (the Reference, "Visibility and privacy": a private
/// item is visible only in its module and its descendants), not missing. Found by the edit
/// property (seed 44, after the Undetermined removal).
fn private_through_cycle(fns: usize) -> World {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    for _ in 0..fns {
        w.fun(root, "x", V::Priv);
    }
    w.use_(root, "crate::m::z", V::Priv);
    w.use_as(root, "crate::m", "y", V::Priv);
    let m = w.module(root, "m", V::Priv);
    w.use_as(m, "crate::y", "z", V::Priv);
    w
}

#[test]
fn a_private_import_reached_around_a_cycle_is_private() {
    for fns in [0, 1, 6] {
        let w = private_through_cycle(fns);
        let root = w.modules.iter().find(|(_, info)| info.parent.is_none()).map(|(&k, _)| k).expect("a root");
        let m = w.modules.iter().find(|(_, info)| info.parent == Some(root)).map(|(&k, _)| k).expect("module m");
        let s = solve(&w, None, Plant::None).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(s.def(m, Namespace::Types, "z"), Some(m), "fns {fns}: m's z is m");
        let private = s.errors(root).iter().filter(|e| {
            matches!(e, crate::ScopeError::Import { segment: 1, why: crate::Unresolved::Private, .. })
        });
        assert_eq!(private.count(), 1, "fns {fns}: root's `use crate::m::z` is one Private: {:?}", s.errors(root));
    }
}

fn probe_world(xs: usize, uses: usize, rules: usize, y: bool) -> World {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    for _ in 0..xs {
        w.fun(root, "x", V::Priv);
    }
    for _ in 0..uses {
        w.use_(root, "crate::x", V::Priv);
    }
    w.glob(root, "crate::n", V::Priv);
    w.glob(root, "crate::m", V::Priv);
    for _ in 0..rules {
        w.rules(root, "x");
    }
    let m = w.module(root, "m", V::Priv);
    w.fun(m, "z", V::Pub);
    let n = w.module(root, "n", V::Priv);
    w.use_as(n, "crate::z", "z", V::Priv);
    if y {
        w.fun(n, "y", V::Priv);
    }
    w
}

/// Rule: an import is resolved as if it were not there itself (rustc's `ignore_import`), also
/// when the walk comes back to its module through another module's kept record. Seed 385:
/// `n`'s `use crate::z as z` reaches root's glob of `n` and must not wait on itself; it names
/// `m::z` through root's glob of `m`, after an edit of `n` as in a cold fill.
#[test]
fn an_import_reaching_itself_through_a_kept_record_ignores_itself() {
    for (xs, uses, rules) in [(3, 3, 2), (0, 0, 0), (0, 1, 0), (1, 1, 0)] {
        let before = probe_world(xs, uses, rules, false);
        let after = probe_world(xs, uses, rules, true);
        let old = solve(&before, None, Plant::None).unwrap_or_else(|e| panic!("{e}"));
        let cold = solve(&after, None, Plant::None);
        let again = resolve_again(&before, &after, &old, &edited(&before, &after), None, Plant::None);
        let again = again.map(|a| a.solved);
        same(&again, &cold).unwrap_or_else(|e| panic!("xs {xs} uses {uses} rules {rules}: {e}"));
        let s = again.unwrap_or_else(|e| panic!("{e}"));
        let root = after.modules.iter().find(|(_, info)| info.parent.is_none()).map(|(&k, _)| k).expect("a root");
        let n = after.modules.iter().filter(|(_, info)| info.parent == Some(root)).map(|(&k, _)| k).max().expect("n");
        assert!(s.def(n, Namespace::Values, "z").is_some(), "n's z names m::z");
        assert!(s.errors(n).is_empty(), "{:?}", s.errors(n));
    }
}

/// `root { use crate::m::m::*; mod m { use crate::m::m::x; mod m { use crate::*; } } }`, every
/// item private (seed 518). By the Reference ("Use declarations", "Visibility and privacy"):
/// `m::m` is private to `m`, so root may not name it and its glob of it is an error; `m` may,
/// and what `m::m`'s glob brings from root holds no `x` (nothing declares one), so `m`'s import
/// of `x` is not found. `m::m` finishes first (its glob's path needs no scope), and its record's
/// edge leads a walk from `m` back into root while root is still being solved.
fn seed_518() -> World {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    w.glob(root, "crate::m::m", V::Priv);
    let m = w.module(root, "m", V::Priv);
    w.use_(m, "crate::m::m::x", V::Priv);
    let mm = w.module(m, "m", V::Priv);
    w.glob(mm, "crate", V::Priv);
    w
}

#[test]
fn a_walk_through_a_finished_record_back_into_its_group_is_answered() {
    let w = seed_518();
    let s = solve(&w, None, Plant::None).unwrap_or_else(|e| panic!("{e}"));
    let root = w.modules.iter().find(|(_, info)| info.parent.is_none()).map(|(&k, _)| k).expect("a root");
    let m = w.modules.iter().find(|(_, info)| info.parent == Some(root)).map(|(&k, _)| k).expect("module m");
    let not_found =
        s.errors(m).iter().filter(|e| matches!(e, crate::ScopeError::Import { why: crate::Unresolved::NotFound, .. }));
    assert_eq!(not_found.count(), 1, "m's `use crate::m::m::x` is one NotFound: {:?}", s.errors(m));
}

/// Cases of the edit property and of each of its controls.
const CASES: u32 = 2_000;

/// Rule: any sequence of edits resolved again gives, after every edit, the records a cold fill
/// of the edited text gives, byte for byte, whatever the schedule and the solver.
#[test]
fn any_edit_sequence_gives_the_cold_records() {
    blaze_sim::check(Config::cases(CASES), |tape| sequence_case(tape, Plant::None));
}

/// The control: members' reads of each other left out of their read lists are caught.
#[test]
fn edit_sequence_catches_fellow_reads_left_out() {
    blaze_sim::control(Config::cases(CASES), Plant::None, Plant::FellowReadsLeftOut, "differ from the cold fill", sequence_case);
}

/// The control: keeping the records a rerun's derivation goes around is caught.
#[test]
fn edit_sequence_catches_kept_suspects() {
    blaze_sim::control(Config::cases(CASES), Plant::None, Plant::KeepSuspects, "differ from the cold fill", sequence_case);
}
