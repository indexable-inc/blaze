//! THE SIMULATION (rule 64): generated worlds of three crates (modules, items, `pub use` chains
//! and renames, globs and glob cycles, `macro_rules!`-like calls that write items with summaries,
//! nested calls, `pub(crate)`/`pub(self)`/`pub(super)`, the extern prelude and a standard prelude
//! module, `#![no_implicit_prelude]`, `extern crate`, `$crate` paths, and a module name equal to a
//! crate's name for 2018's uniform-path ambiguity). For random asks, the demand machine's answer
//! (driven through `driver::resolve` over a [`Source`] of the world, one need at a time) EQUALS
//! the answer of a whole-world resolver written here (the oracle: every call expanded first, every
//! cell of every module solved by the same stratified least fixpoint, globally). Several asks share
//! one machine per world, so frozen cells are reused across asks.
//!
//! Controls (rule 53): a glob that wins over an explicit binding, an expansion's items ignored,
//! the fixpoint cut after one pass: each planted, each caught.
//!
//! The measure, printed by `demand_answers_equal_the_whole_world`: outlines given against the
//! world's modules, expansions against its calls, cells made against the whole world's cells. THE
//! FLOOR: the modules on the asked path, the modules its imports and globs lead to for that one
//! name, and only the calls in those modules whose summaries name it; the whole world is every
//! module, every call and every (module, namespace, name).

use blaze_id::{CrateItemKey, ItemKey};
use blaze_ids::ContentId;
use blaze_mem::Frame;
use blaze_mem::tag::Tag;
use blaze_own::{AVec, Erased, Vec};
use blaze_sim::{Broke, Config, Tape};

use crate::driver::{DriveError, Source, resolve};
use crate::plant::{self, Plant};
use crate::resolver::Resolver;
use crate::types::{
    Ambiguity, Anchor, Answer, Ask, CallRef, CrateRoot, Def, Entry, Error, Extern, ModRef, Ns, Outline, Path, Spaces,
    Vis, Writes,
};

const MODS: [&str; 3] = ["m0", "m1", "a"];
const TYPES: [&str; 2] = ["T0", "T1"];
const VALUES: [&str; 2] = ["f0", "f1"];
const MACROS: [&str; 1] = ["k0"];
/// Every name of the world: the oracle's cells are these in every module and namespace.
const NAMES: [&str; 8] = ["m0", "m1", "a", "T0", "T1", "f0", "f1", "k0"];
/// Crate `i` is named `CRATES[i]`; crates before it are its extern prelude.
const CRATES: [&str; 3] = ["a", "b", "c"];
const SEGS: [&[&str]; 7] = [&[], &["m0"], &["m1"], &["a"], &["m0", "m1"], &["m1", "m0"], &["a", "m0"]];
const SUMMARIES: [&[&str]; 4] = [&["T0"], &["T0", "f0"], &["T1", "f1", "k0"], &["m0", "m1", "a", "T0", "T1", "f0", "f1", "k0"]];
const PRELUDE: &[&str] = &["m0"];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Mod,
    Ty,
    Val,
    Mac,
}

fn pool(kind: Kind) -> &'static [&'static str] {
    match kind {
        Kind::Mod => &MODS,
        Kind::Ty => &TYPES,
        Kind::Val => &VALUES,
        Kind::Mac => &MACROS,
    }
}

fn kind_of(name: &str) -> Kind {
    [Kind::Mod, Kind::Ty, Kind::Val, Kind::Mac].into_iter().find(|k| pool(*k).contains(&name)).expect("a name of a pool")
}

fn ns_of(kind: Kind) -> Ns {
    match kind {
        Kind::Mod | Kind::Ty => Ns::Type,
        Kind::Val => Ns::Value,
        Kind::Mac => Ns::Macro,
    }
}

fn spaces_of(kind: Kind) -> Spaces {
    match ns_of(kind) {
        Ns::Type => Spaces::TYPE,
        Ns::Value => Spaces::VALUE,
        Ns::Macro => Spaces::MACRO,
    }
}

fn id(tag: u8, a: usize, b: usize) -> ContentId {
    let mut bytes = [0u8; 32];
    bytes[0] = tag;
    bytes[1..9].copy_from_slice(&u64::try_from(a).expect("fits").to_le_bytes());
    bytes[9..17].copy_from_slice(&u64::try_from(b).expect("fits").to_le_bytes());
    ContentId(bytes)
}

fn crate_key(i: usize) -> CrateItemKey {
    CrateItemKey(id(0xC0, i, 0))
}

struct ModSpec {
    krate: usize,
    parent: Option<usize>,
    name: &'static str,
    nip: bool,
    entries: (usize, usize),
    chain: (usize, usize),
}

struct CallSpec {
    call: CallRef,
    module: usize,
    writes: Writes<'static>,
    depth: usize,
    entries: (usize, usize),
}

struct World<'f> {
    mods: AVec<'f, ModSpec>,
    entries: AVec<'f, Entry<'static>>,
    exp: AVec<'f, Entry<'static>>,
    calls: AVec<'f, CallSpec>,
    chains: AVec<'f, ItemKey>,
    externs: AVec<'f, Extern<'static>>,
    crate_externs: [(usize, usize); 3],
    roots: [usize; 3],
    items: usize,
}

impl World<'_> {
    fn modref(&self, j: usize) -> ModRef {
        ModRef { krate: crate_key(self.mods[j].krate), key: ItemKey(id(0x30, j, 0)) }
    }

    fn draw_vis(&self, tape: &Tape, j: usize) -> Vis {
        let m = &self.mods[j];
        match tape.index(4, "vis") {
            0 => Vis::Pub,
            1 => Vis::Crate(crate_key(m.krate)),
            2 => Vis::In(self.modref(j).key),
            _ => Vis::In(self.modref(m.parent.unwrap_or(j)).key),
        }
    }

    fn draw_path(&self, tape: &Tape, j: usize, empty_ok: bool) -> Path<'static> {
        let m = &self.mods[j];
        let anchor = match tape.index(6, "anchor") {
            0 => Anchor::Crate,
            1 => Anchor::SelfMod,
            2 if m.parent.is_some() => Anchor::Super(1),
            3 if m.krate > 0 => Anchor::Extern(CRATES[tape.index(m.krate, "extern")]),
            4 => Anchor::Uniform,
            5 => Anchor::Root(self.modref(self.roots[tape.index(m.krate + 1, "$crate")])),
            _ => Anchor::SelfMod,
        };
        let mut segs = SEGS[tape.index(SEGS.len(), "segs")];
        if anchor == Anchor::Uniform && segs.is_empty() && !empty_ok {
            segs = SEGS[1];
        }
        Path { anchor, segs }
    }
}

/// A call whose expansion is drawn after the entries that hold it.
struct Job {
    call: CallRef,
    module: usize,
    writes: Writes<'static>,
    depth: usize,
}

/// One entry of module `j` into `out`; `within` restricts the names to an `Only` summary.
fn draw_entry(
    tape: &Tape,
    w: &mut World<'_>,
    j: usize,
    within: Option<&'static [&'static str]>,
    depth: usize,
    jobs: &mut AVec<'_, Job>,
    calls: &mut usize,
) -> Entry<'static> {
    let krate = w.mods[j].krate;
    let (kind, name) = match within {
        Some(names) => {
            let name = names[tape.index(names.len(), "within")];
            (kind_of(name), name)
        }
        None => {
            let kind = [Kind::Mod, Kind::Ty, Kind::Val, Kind::Mac][tape.index(4, "kind")];
            (kind, pool(kind)[tape.index(pool(kind).len(), "name")])
        }
    };
    let vis = w.draw_vis(tape, j);
    let mut what = tape.index(10, "entry");
    if what == 7 && within.is_some() {
        what = 4;
    }
    if what == 8 && (krate == 0 || kind != Kind::Mod) {
        what = 5;
    }
    if what == 9 && depth >= 2 {
        what = 6;
    }
    if what <= 3 && kind == Kind::Mod {
        what = 4;
    }
    match what {
        0..=3 => {
            w.items += 1;
            Entry::Item { name, spaces: spaces_of(kind), vis, def: Def::Item(ItemKey(id(0x17, w.items, 0))), exported: false }
        }
        4..=6 => {
            let target = pool(kind)[tape.index(pool(kind).len(), "target")];
            Entry::Use { name, path: w.draw_path(tape, j, true), target, vis }
        }
        7 => Entry::Glob { path: w.draw_path(tape, j, false), vis },
        8 => {
            let d = tape.index(krate, "extern crate");
            Entry::ExternCrate { name, krate: crate_key(d), root: w.modref(w.roots[d]).key, vis }
        }
        _ => {
            *calls += 1;
            let call = CallRef(id(0xCA, *calls, 0));
            let writes = match within {
                Some(names) => Writes::Only { names, exports: false },
                None if tape.coin("any") => Writes::Any,
                None => Writes::Only { names: SUMMARIES[tape.index(SUMMARIES.len(), "summary")], exports: false },
            };
            jobs.push(Job { call, module: j, writes, depth: depth + 1 });
            Entry::Call { call, writes }
        }
    }
}

fn world<'f>(tape: &Tape, alloc: blaze_own::Alloc<'f>) -> World<'f> {
    let mut w = World {
        mods: Vec::new_in(alloc),
        entries: Vec::new_in(alloc),
        exp: Vec::new_in(alloc),
        calls: Vec::new_in(alloc),
        chains: Vec::new_in(alloc),
        externs: Vec::new_in(alloc),
        crate_externs: [(0, 0); 3],
        roots: [0; 3],
        items: 0,
    };
    for i in 0..3 {
        let root = w.mods.len();
        w.roots[i] = root;
        w.mods.push(ModSpec { krate: i, parent: None, name: "", nip: tape.one_in(10, "no_implicit_prelude"), entries: (0, 0), chain: (0, 0) });
        for _ in 0..tape.index(5, "modules") {
            let parent = root + tape.index(w.mods.len() - root, "parent");
            let name = MODS[tape.index(MODS.len(), "module name")];
            if w.mods.iter().any(|m| m.parent == Some(parent) && m.name == name) {
                continue;
            }
            let nip = w.mods[parent].nip || tape.one_in(10, "no_implicit_prelude");
            w.mods.push(ModSpec { krate: i, parent: Some(parent), name, nip, entries: (0, 0), chain: (0, 0) });
        }
        let start = w.externs.len();
        for d in 0..i {
            let root = w.modref(w.roots[d]).key;
            w.externs.push(Extern { name: CRATES[d], krate: crate_key(d), root });
        }
        w.crate_externs[i] = (start, w.externs.len());
    }
    for j in 0..w.mods.len() {
        let mut up = [0usize; 8];
        let mut n = 0;
        let mut at = w.mods[j].parent;
        while let Some(p) = at {
            up[n] = p;
            n += 1;
            at = w.mods[p].parent;
        }
        let start = w.chains.len();
        for k in (0..n).rev() {
            let key = w.modref(up[k]).key;
            w.chains.push(key);
        }
        w.mods[j].chain = (start, w.chains.len());
    }
    let mut jobs: AVec<'_, Job> = Vec::new_in(alloc);
    let mut calls = 0;
    for j in 0..w.mods.len() {
        let start = w.entries.len();
        for k in 0..w.mods.len() {
            if w.mods[k].parent == Some(j) {
                let entry = Entry::Item { name: w.mods[k].name, spaces: Spaces::TYPE, vis: w.draw_vis(tape, j), def: Def::Module(w.modref(k)), exported: false };
                w.entries.push(entry);
            }
        }
        for _ in 0..tape.index(7, "entries") {
            let entry = draw_entry(tape, &mut w, j, None, 0, &mut jobs, &mut calls);
            w.entries.push(entry);
        }
        w.mods[j].entries = (start, w.entries.len());
    }
    let mut next = 0;
    while let Some(job) = jobs.get(next) {
        let (call, module, writes, depth) = (job.call, job.module, job.writes, job.depth);
        next += 1;
        let within = match writes {
            Writes::Any => None,
            Writes::Only { names, .. } => Some(names),
        };
        let mut written: AVec<'_, Entry<'static>> = Vec::new_in(alloc);
        for _ in 0..1 + tape.index(3, "written") {
            let entry = draw_entry(tape, &mut w, module, within, depth, &mut jobs, &mut calls);
            written.push(entry);
        }
        let start = w.exp.len();
        w.exp.extend_from_slice(written.as_slice());
        w.calls.push(CallSpec { call, module, writes, depth, entries: (start, w.exp.len()) });
    }
    w
}

/// The world as the machine reads it.
struct Built<'w> {
    outlines: AVec<'w, Outline<'w>>,
    roots: AVec<'w, CrateRoot<'w>>,
}

fn built<'w>(w: &'w World<'_>, alloc: blaze_own::Alloc<'w>) -> Built<'w> {
    let mut outlines = Vec::new_in(alloc);
    for (j, m) in w.mods.iter().enumerate() {
        outlines.push(Outline {
            module: w.modref(j),
            digest: id(0xD1, j, 0),
            chain: &w.chains[m.chain.0..m.chain.1],
            no_implicit_prelude: m.nip,
            entries: &w.entries[m.entries.0..m.entries.1],
        });
    }
    let mut roots = Vec::new_in(alloc);
    for i in 0..3 {
        let (s, e) = w.crate_externs[i];
        let prelude = (i > 0).then_some(Path { anchor: Anchor::Extern("a"), segs: PRELUDE });
        roots.push(CrateRoot { krate: crate_key(i), digest: id(0xD2, i, 0), extern_prelude: &w.externs[s..e], prelude, export_modules: &[] });
    }
    Built { outlines, roots }
}

/// The world as a [`Source`]: what the machine asks, found by key.
struct Sim<'w> {
    world: &'w World<'w>,
    built: &'w Built<'w>,
}

#[derive(Debug)]
enum Missing {
    Module(ModRef),
    Call(CallRef),
    Crate(CrateItemKey),
}

impl<'w> Source<'w> for Sim<'w> {
    type Error = Missing;

    fn outline(&mut self, module: ModRef) -> Result<&'w Outline<'w>, Missing> {
        self.built.outlines.iter().find(|o| o.module == module).ok_or(Missing::Module(module))
    }

    fn expansion(&mut self, _module: ModRef, call: CallRef) -> Result<&'w [Entry<'w>], Missing> {
        let world = self.world;
        world.calls.iter().find(|c| c.call == call).map(|c| &world.exp[c.entries.0..c.entries.1]).ok_or(Missing::Call(call))
    }

    fn root(&mut self, krate: CrateItemKey) -> Result<&'w CrateRoot<'w>, Missing> {
        self.built.roots.iter().find(|r| r.krate == krate).ok_or(Missing::Crate(krate))
    }
}

// ---- THE ORACLE: the whole world, every call expanded, every cell solved. ----

const MOST: usize = 4;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct OSet {
    rows: [Option<(Def, Vis)>; MOST],
    over: bool,
}

impl OSet {
    const EMPTY: OSet = OSet { rows: [None; MOST], over: false };

    fn add(&mut self, def: Def, vis: Vis, at: &Outline<'_>) {
        if let Some(row) = self.rows.iter_mut().flatten().find(|r| r.0 == def) {
            row.1 = row.1.wider(vis, at);
            return;
        }
        let mut all: [Option<(Def, Vis)>; MOST + 1] = [None; MOST + 1];
        let mut n = 0;
        for r in self.rows.iter().flatten() {
            all[n] = Some(*r);
            n += 1;
        }
        all[n] = Some((def, vis));
        n += 1;
        all[..n].sort_unstable_by_key(|r| r.map(|r| r.0));
        if n > MOST {
            self.over = true;
            n = MOST;
        }
        self.rows = [None; MOST];
        self.rows[..n].copy_from_slice(&all[..n]);
    }

    fn len(&self) -> usize {
        self.rows.iter().flatten().count() + usize::from(self.over)
    }

    fn iter(&self) -> impl Iterator<Item = (Def, Vis)> + '_ {
        self.rows.iter().flatten().copied()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct OVal {
    explicit: OSet,
    entries: usize,
    glob: OSet,
    only_expanded: bool,
}

impl OVal {
    const EMPTY: OVal = OVal { explicit: OSet::EMPTY, entries: 0, glob: OSet::EMPTY, only_expanded: false };

    fn effective(&self) -> OSet {
        if self.entries > 0 { self.explicit } else { self.glob }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum OClass {
    Absent,
    One(Def, Vis),
    Amb(Ambiguity),
}

fn oclass_of(set: OSet) -> OClass {
    match (set.over, set.len(), set.iter().next()) {
        (false, 1, Some((d, v))) => OClass::One(d, v),
        (false, 0, _) => OClass::Absent,
        _ => OClass::Amb(Ambiguity::Many),
    }
}

fn oclassify(v: &OVal, ns: Ns) -> OClass {
    if v.entries >= 2 {
        return OClass::Amb(Ambiguity::Duplicate);
    }
    if v.entries == 1 && ns == Ns::Macro && v.only_expanded && v.glob.iter().any(|(g, _)| v.explicit.iter().all(|(e, _)| e != g)) {
        return OClass::Amb(Ambiguity::GlobVsExpanded);
    }
    oclass_of(v.effective())
}

fn ns_index(ns: Ns) -> usize {
    match ns {
        Ns::Type => 0,
        Ns::Value => 1,
        Ns::Macro => 2,
    }
}

struct Oracle<'w> {
    built: &'w Built<'w>,
    /// Every entry of every module, expansions included: (module, entry, depth).
    ents: AVec<'w, (usize, Entry<'w>, usize)>,
    vals: AVec<'w, OVal>,
    before: AVec<'w, bool>,
    after: AVec<'w, bool>,
    unstable: bool,
}

impl<'w> Oracle<'w> {
    fn new(w: &'w World<'_>, built: &'w Built<'w>, alloc: blaze_own::Alloc<'w>) -> Oracle<'w> {
        let mut ents = Vec::new_in(alloc);
        for (j, m) in w.mods.iter().enumerate() {
            for e in &w.entries[m.entries.0..m.entries.1] {
                ents.push((j, *e, 0));
            }
        }
        for c in w.calls.iter() {
            for e in &w.exp[c.entries.0..c.entries.1] {
                ents.push((c.module, *e, c.depth));
            }
        }
        let cells = w.mods.len() * 3 * NAMES.len();
        let mut vals = Vec::new_in(alloc);
        let mut before = Vec::new_in(alloc);
        let mut after = Vec::new_in(alloc);
        for _ in 0..cells {
            vals.push(OVal::EMPTY);
        }
        for _ in 0..ents.len() * 3 {
            before.push(false);
            after.push(false);
        }
        let mut o = Oracle { built, ents, vals, before, after, unstable: false };
        o.solve();
        o
    }

    fn cells(&self) -> usize {
        self.vals.len()
    }

    fn cell(&self, j: usize, ns: Ns, name: &str) -> usize {
        let n = NAMES.iter().position(|x| *x == name).expect("a world name");
        (j * 3 + ns_index(ns)) * NAMES.len() + n
    }

    fn module(&self, m: ModRef) -> Option<usize> {
        self.built.outlines.iter().position(|o| o.module == m)
    }

    fn root(&self, krate: CrateItemKey) -> &'w CrateRoot<'w> {
        self.built.roots.iter().find(|r| r.krate == krate).expect("a world crate")
    }

    fn solve(&mut self) {
        for slot in self.before.iter_mut() {
            *slot = false;
        }
        for _ in 0..64 {
            for v in self.vals.iter_mut() {
                *v = OVal::EMPTY;
            }
            let mut settled = false;
            for _ in 0..4096 {
                for slot in self.after.iter_mut() {
                    *slot = false;
                }
                let mut changed = false;
                for c in 0..self.vals.len() {
                    let v = self.eval(c);
                    if v != self.vals[c] {
                        self.vals[c] = v;
                        changed = true;
                    }
                }
                if !changed {
                    settled = true;
                    break;
                }
            }
            if !settled || self.before.iter().zip(self.after.iter()).any(|(b, a)| *b && !*a) {
                self.unstable = true;
                return;
            }
            if self.before.as_slice() == self.after.as_slice() {
                return;
            }
            for (b, a) in self.before.iter_mut().zip(self.after.iter()) {
                *b = *a;
            }
        }
        self.unstable = true;
    }

    fn eval(&mut self, c: usize) -> OVal {
        let per_module = 3 * NAMES.len();
        let j = c / per_module;
        let ns = Ns::ALL[(c % per_module) / NAMES.len()];
        let name = NAMES[c % NAMES.len()];
        // Through the `&'w` copy, so `o` does not hold `self` while `after` is written.
        let built = self.built;
        let o = &built.outlines[j];
        let mut v = OVal::EMPTY;
        let mut plain = false;
        for k in 0..self.ents.len() {
            let (m, entry, depth) = self.ents[k];
            if m != j {
                continue;
            }
            match entry {
                Entry::Item { name: n, spaces, vis, def, exported: false } if n == name && spaces.has(ns) => {
                    v.entries += 1;
                    v.explicit.add(def, vis, o);
                    plain |= depth == 0;
                }
                Entry::ExternCrate { name: n, krate, root, vis } if n == name && ns == Ns::Type => {
                    v.entries += 1;
                    v.explicit.add(Def::Module(ModRef { krate, key: root }), vis, o);
                    plain |= depth == 0;
                }
                Entry::Use { name: n, path, target, vis } if n == name => {
                    let got = self.import(j, path, target, ns, vis);
                    let s = k * 3 + ns_index(ns);
                    if got.len() > 0 {
                        self.after[s] = true;
                    }
                    if self.before[s] {
                        v.entries += 1;
                        for (d, dv) in got.iter() {
                            v.explicit.add(d, dv, o);
                        }
                        v.explicit.over |= got.over;
                        plain |= depth == 0;
                    }
                }
                _ => {}
            }
        }
        v.only_expanded = v.entries > 0 && !plain;
        if v.entries == 0 || (ns == Ns::Macro && v.only_expanded) {
            for k in 0..self.ents.len() {
                let (m, entry, _) = self.ents[k];
                if m != j {
                    continue;
                }
                let Entry::Glob { path, vis } = entry else { continue };
                let Ok(t) = self.walk(j, path, true) else { continue };
                let source = self.vals[self.cell(t, ns, name)].effective();
                for (d, dv) in source.iter() {
                    if dv.visible_from(o) {
                        v.glob.add(d, vis.narrower(dv, o), o);
                    }
                }
                v.glob.over |= source.over;
            }
        }
        v
    }

    fn descend(&self, from: &Outline<'_>, class: OClass, at: usize) -> Result<usize, Answer> {
        match class {
            OClass::Absent => Err(Answer::Absent { at }),
            OClass::Amb(why) => Err(Answer::Ambiguous { at, why }),
            OClass::One(def, vis) if !vis.visible_from(from) => Err(Answer::Private { at, def }),
            OClass::One(def @ Def::Item(_), _) => Err(Answer::NotAModule { at, def }),
            OClass::One(Def::Module(m), _) => self.module(m).ok_or(Answer::Absent { at }),
        }
    }

    fn walk(&self, from: usize, path: Path<'_>, first: bool) -> Result<usize, Answer> {
        let o = &self.built.outlines[from];
        let krate = o.module.krate;
        let missing = Answer::Absent { at: 0 };
        let (mut at, rest) = match path.anchor {
            Anchor::SelfMod | Anchor::Super(0) => (from, path.segs),
            Anchor::Crate => (self.module(ModRef { krate, key: o.root() }).ok_or(missing)?, path.segs),
            Anchor::Super(n) => {
                let key = o.chain.len().checked_sub(n).and_then(|i| o.chain.get(i)).copied().ok_or(Answer::NoParent)?;
                (self.module(ModRef { krate, key }).ok_or(missing)?, path.segs)
            }
            Anchor::Root(m) => (self.module(m).ok_or(missing)?, path.segs),
            Anchor::Extern(name) => {
                let e = self.root(krate).extern_prelude.iter().find(|e| e.name == name).ok_or(Answer::NoCrate)?;
                (self.module(ModRef { krate: e.krate, key: e.root }).ok_or(missing)?, path.segs)
            }
            Anchor::Uniform | Anchor::Lexical => {
                let (&name, rest) = path.segs.split_first().ok_or(missing)?;
                if !first {
                    return Err(missing);
                }
                let class = self.first(from, name, Ns::Type, path.anchor == Anchor::Uniform);
                (self.descend(o, class, 0)?, rest)
            }
        };
        let offset = path.segs.len() - rest.len();
        for (i, &seg) in rest.iter().enumerate() {
            let v = self.vals[self.cell(at, Ns::Type, seg)];
            at = self.descend(o, oclassify(&v, Ns::Type), offset + i)?;
        }
        Ok(at)
    }

    fn first(&self, from: usize, name: &str, ns: Ns, uniform: bool) -> OClass {
        let o = &self.built.outlines[from];
        let local = oclassify(&self.vals[self.cell(from, ns, name)], ns);
        if o.no_implicit_prelude || (!uniform && local != OClass::Absent) {
            return local;
        }
        let root = self.root(o.module.krate);
        if ns == Ns::Type
            && let Some(e) = root.extern_prelude.iter().find(|e| e.name == name)
        {
            let ext = Def::Module(ModRef { krate: e.krate, key: e.root });
            return match local {
                OClass::Absent => OClass::One(ext, Vis::Pub),
                OClass::One(d, _) if uniform && d != ext => OClass::Amb(Ambiguity::Many),
                other => other,
            };
        }
        if local != OClass::Absent {
            return local;
        }
        let Some(prelude) = root.prelude else { return local };
        let Some(rm) = self.module(ModRef { krate: o.module.krate, key: o.root() }) else { return local };
        match self.walk(rm, prelude, false) {
            Ok(p) => match oclassify(&self.vals[self.cell(p, ns, name)], ns) {
                OClass::One(_, vis) if !vis.visible_from(o) => OClass::Absent,
                other => other,
            },
            Err(_) => local,
        }
    }

    fn import(&self, j: usize, path: Path<'_>, target: &str, ns: Ns, vis: Vis) -> OSet {
        let o = &self.built.outlines[j];
        let class = if matches!(path.anchor, Anchor::Uniform | Anchor::Lexical) && path.segs.is_empty() {
            self.first(j, target, ns, path.anchor == Anchor::Uniform)
        } else {
            match self.walk(j, path, true) {
                Ok(t) => oclassify(&self.vals[self.cell(t, ns, target)], ns),
                Err(_) => OClass::Absent,
            }
        };
        let mut out = OSet::EMPTY;
        match class {
            OClass::Absent => {}
            OClass::One(d, dv) => {
                if dv.visible_from(o) {
                    out.add(d, vis.narrower(dv, o), o);
                }
            }
            OClass::Amb(_) => out.over = true,
        }
        out
    }

    fn answer(&self, ask: Ask<'_>) -> Answer {
        let from = self.module(ask.from).expect("an asking module of the world");
        let o = &self.built.outlines[from];
        let n = ask.path.segs.len();
        let class = if matches!(ask.path.anchor, Anchor::Uniform | Anchor::Lexical) && ask.path.segs.is_empty() {
            self.first(from, ask.name, ask.ns, ask.path.anchor == Anchor::Uniform)
        } else {
            match self.walk(from, ask.path, true) {
                Err(answer) => return answer,
                Ok(t) => oclassify(&self.vals[self.cell(t, ask.ns, ask.name)], ask.ns),
            }
        };
        match class {
            OClass::Absent => Answer::Absent { at: n },
            OClass::Amb(why) => Answer::Ambiguous { at: n, why },
            OClass::One(def, vis) if vis.visible_from(o) => Answer::Found { def, vis },
            OClass::One(def, _) => Answer::Private { at: n, def },
        }
    }
}

// ---- THE CASE ----

/// What the cases made, summed for the printed measure.
#[derive(Default)]
struct Tally {
    worlds: usize,
    asks: usize,
    found: usize,
    modules: usize,
    calls: usize,
    whole_cells: usize,
    outlines: usize,
    expansions: usize,
    cells: usize,
    unstable: usize,
}

fn draw_ask(tape: &Tape, w: &World<'_>) -> Ask<'static> {
    let j = tape.index(w.mods.len(), "asking module");
    let m = &w.mods[j];
    let anchor = match tape.index(6, "ask anchor") {
        0 => Anchor::Crate,
        1 => Anchor::SelfMod,
        2 if m.parent.is_some() => Anchor::Super(1),
        3 if m.krate > 0 => Anchor::Extern(CRATES[tape.index(m.krate, "ask extern")]),
        4 => Anchor::Uniform,
        _ => Anchor::Lexical,
    };
    let segs = SEGS[tape.index(SEGS.len(), "ask segs")];
    let kind = [Kind::Mod, Kind::Ty, Kind::Val, Kind::Mac][tape.index(4, "ask kind")];
    let name = pool(kind)[tape.index(pool(kind).len(), "ask name")];
    Ask { from: w.modref(j), path: Path { anchor, segs }, name, ns: ns_of(kind) }
}

fn case(tape: &Tape, tally: &mut Tally) -> Result<(), Broke> {
    let frame = Frame::new(Tag::Nameres).map_err(|why| Broke::Harness(std::format!("the case's frame: {why:?}")))?;
    let erased = Erased::new(&frame);
    let alloc = erased.handle();
    let w = world(tape, alloc);
    let b = built(&w, alloc);
    let oracle = Oracle::new(&w, &b, alloc);
    if oracle.unstable {
        tally.unstable += 1;
        return Ok(());
    }
    let mut machine = Resolver::new(alloc);
    let mut source = Sim { world: &w, built: &b };
    tally.worlds += 1;
    for _ in 0..1 + tape.index(4, "asks") {
        let ask = draw_ask(tape, &w);
        let want = oracle.answer(ask);
        let got = match resolve(&mut machine, &mut source, ask, alloc) {
            Ok(got) => got,
            Err(DriveError::Machine(Error::Unstable { .. })) => {
                return Err(Broke::Oracle(std::format!("the demand machine found {ask:?} unstable, the whole world {want:?}")));
            }
            Err(why) => return Err(Broke::Oracle(std::format!("the demand machine refused {ask:?}: {why:?}"))),
        };
        if got != want {
            return Err(Broke::Oracle(std::format!("the demand answer differs for {ask:?}: demand {got:?}, whole world {want:?}")));
        }
        tally.asks += 1;
        tally.found += usize::from(matches!(got, Answer::Found { .. }));
    }
    let counts = machine.counts();
    tally.modules += w.mods.len();
    tally.calls += w.calls.len();
    tally.whole_cells += oracle.cells();
    tally.outlines += counts.outlines;
    tally.expansions += counts.expansions;
    tally.cells += counts.cells;
    Ok(())
}

fn ratio(part: usize, whole: usize) -> f64 {
    let (p, w) = (u32::try_from(part).map_or(f64::MAX, f64::from), u32::try_from(whole.max(1)).map_or(f64::MAX, f64::from));
    p / w
}

/// The refinement: for random generated module worlds and asks, demand-driven name resolution equals whole-world resolution.
#[test]
fn demand_answers_equal_the_whole_world() {
    let mut tally = Tally::default();
    blaze_sim::check(Config::cases(400), |tape| case(tape, &mut tally));
    assert!(tally.asks > 0, "the simulation asked nothing");
    assert!(tally.found > 0, "no ask found a binding: the worlds are too sparse to test anything");
    std::println!(
        "names on demand (simulated): {} asks over {} worlds ({} found, {} worlds the whole world found unstable): outlines {} of {} modules ({:.3}), expansions {} of {} calls ({:.3}), cells {} of {} whole-world cells ({:.3})",
        tally.asks,
        tally.worlds,
        tally.found,
        tally.unstable,
        tally.outlines,
        tally.modules,
        ratio(tally.outlines, tally.modules),
        tally.expansions,
        tally.calls,
        ratio(tally.expansions, tally.calls),
        tally.cells,
        tally.whole_cells,
        ratio(tally.cells, tally.whole_cells),
    );
}

fn planted(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    plant::with(plant, || case(tape, &mut Tally::default()))
}

#[test]
fn the_oracle_catches_a_glob_that_beats_an_explicit_binding() {
    blaze_sim::control(Config::cases(400), Plant::Clean, Plant::GlobBeatsExplicit, "demand", planted);
}

#[test]
fn the_oracle_catches_an_expansion_ignored() {
    blaze_sim::control(Config::cases(400), Plant::Clean, Plant::IgnoreExpanded, "demand", planted);
}

#[test]
fn the_oracle_catches_a_fixpoint_cut_early() {
    blaze_sim::control(Config::cases(400), Plant::Clean, Plant::CutEarly, "demand", planted);
}

/// The soundness rule's cause removed (rule 23): one module, a glob of a crate root holding `T0`,
/// and a call summarised to write only `f0`. Asking `T0` expands nothing; asking `f0` expands
/// the call, which writes `f0`, and the answer is the expansion's item.
#[test]
fn a_call_that_cannot_write_the_name_is_not_expanded() {
    let frame = Frame::new(Tag::Nameres).expect("the test's frame");
    let erased = Erased::new(&frame);
    let alloc = erased.handle();
    let krate = crate_key(0);
    let root = ModRef { krate, key: ItemKey(id(0x30, 0, 0)) };
    let call = CallRef(id(0xCA, 1, 0));
    let t0 = Def::Item(ItemKey(id(0x17, 1, 0)));
    let f0 = Def::Item(ItemKey(id(0x17, 2, 0)));
    let entries = [
        Entry::Item { name: "T0", spaces: Spaces::TYPE, vis: Vis::Pub, def: t0, exported: false },
        Entry::Call { call, writes: Writes::Only { names: &["f0"], exports: false } },
    ];
    let written = [Entry::Item { name: "f0", spaces: Spaces::VALUE, vis: Vis::Pub, def: f0, exported: false }];
    let outline = Outline { module: root, digest: id(0xD1, 0, 0), chain: &[], no_implicit_prelude: true, entries: &entries };
    let mut machine = Resolver::new(alloc);
    machine.give(crate::types::Given::Outline(&outline)).expect("the outline");
    machine.ask(Ask { from: root, path: Path { anchor: Anchor::Crate, segs: &[] }, name: "T0", ns: Ns::Type }).expect("an ask");
    let step = machine.step().expect("a step");
    assert_eq!(step, crate::resolver::Step::Answered(Answer::Found { def: t0, vis: Vis::Pub }), "T0 is known without the call");
    assert_eq!(machine.counts().expansions, 0, "the call that cannot write T0 was not expanded");
    machine.ask(Ask { from: root, path: Path { anchor: Anchor::Crate, segs: &[] }, name: "f0", ns: Ns::Value }).expect("an ask");
    assert_eq!(machine.step().expect("a step"), crate::resolver::Step::Needs);
    assert_eq!(machine.needs(), &[crate::types::Need::Expand { module: root, call }][..], "f0 needs the one call");
    machine.give(crate::types::Given::Expansion { module: root, call, entries: &written }).expect("the expansion");
    assert_eq!(machine.step().expect("a step"), crate::resolver::Step::Answered(Answer::Found { def: f0, vis: Vis::Pub }));
    assert_eq!(machine.counts().expansions, 1);
    // An expansion that writes outside its summary is refused (soundness rule 3).
    let mut other = Resolver::new(alloc);
    other.give(crate::types::Given::Outline(&outline)).expect("the outline");
    let lying = [Entry::Item { name: "T0", spaces: Spaces::TYPE, vis: Vis::Pub, def: f0, exported: false }];
    assert_eq!(
        other.give(crate::types::Given::Expansion { module: root, call, entries: &lying }),
        Err(Error::SummaryBroken { module: root, call })
    );
    std::println!("names on demand: T0 with 0 expansions, f0 with 1, a summary broken refused");
}
