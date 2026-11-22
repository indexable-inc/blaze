//! The machine (`#![blaze::sans_io]` in spirit: it reads nothing but what it was handed).
//!
//! [`Resolver::ask`] opens an ask; [`Resolver::step`] solves as far as what it holds allows and
//! either answers or lists what it needs ([`Resolver::needs`]: every missing outline, expansion
//! and root it met in the solve, batched); the driver gives each ([`Resolver::give`]) and steps
//! again. A step re-solves from what it holds, so its answer is a function of the givens alone.
//!
//! THE SOLVE. Cells are made as the solve reaches them: the ask's path walk makes the cells of
//! its segments, a cell's evaluation makes the cells its imports and globs read. Rounds of the
//! stratified fixpoint (crate docs, semantics 3): each round holds fixed the single imports that
//! shadow (`before`), resets every cell not yet frozen to empty and evaluates all of them in
//! passes until a pass changes none; the imports that resolved in that last pass (`after`) are
//! the next round's shadowing set. When a round's set equals the one it started from and nothing
//! was missing, every cell is at its fixpoint: one more pass logs each cell's reads, and the
//! cells are FROZEN, kept for every later ask of the request, never evaluated again (their
//! inputs are complete: a call a frozen cell skipped could not write its name, rule 1 of the
//! soundness rule).
//!
//! RECURSION. None but one bounded step: a path walk may resolve the standard prelude's path,
//! and that inner walk refuses a first-segment anchor, so it never walks again (depth 2).
//!
//! BOUNDS (rule 18): [`MOST_MODULES`], [`MOST_CELLS`], [`MOST_PASSES`] a round, [`MOST_ROUNDS`] a
//! solve, [`MOST_DEPTH`] of nested expansion, each a typed refusal; [`MOST_NEEDS`] a batch (the
//! rest are found in the next step: a batch is a size, not a loss).

use blaze_id::CrateItemKey;
use blaze_ids::{ContentId, Hasher};
use blaze_own::{ABTreeMap, AVec, Alloc, BTreeMap, Vec};

use crate::plant::{Plant, on as planted};
use crate::types::{
    Ambiguity, Anchor, Answer, Ask, CallRef, CellKey, CrateRoot, DIGEST_DOMAIN, Def, Entry, Error, Given, Limit,
    ModRef, Need, Ns, Outline, Path, Read, Vis, Writes, hash_def,
};

pub const MOST_MODULES: usize = 1 << 16;
pub const MOST_CELLS: usize = 1 << 18;
pub const MOST_PASSES: usize = 1 << 12;
pub const MOST_ROUNDS: usize = 64;
/// rustc's default `recursion_limit`.
pub const MOST_DEPTH: usize = 128;
pub const MOST_NEEDS: usize = 256;
/// Definitions a cell keeps; past it the cell is ambiguous whatever the rest are.
const MOST_DEFS: usize = 4;

/// What [`Resolver::step`] came to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Step {
    /// [`Resolver::needs`] lists what to give before the next step.
    Needs,
    Answered(Answer),
}

/// What the request has made so far: the measure the brief asks printed against the whole crate.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Counts {
    pub asks: usize,
    pub outlines: usize,
    pub roots: usize,
    pub expansions: usize,
    pub cells: usize,
    pub rounds: usize,
    pub passes: usize,
}

/// A small sorted set of definitions, each at the widest visibility it was bound at.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Defs {
    rows: [Option<(Def, Vis)>; MOST_DEFS],
    /// More than `MOST_DEFS`, or an ambiguous import's poison: ambiguous whatever the rows are.
    over: bool,
}

impl Defs {
    const EMPTY: Defs = Defs { rows: [None; MOST_DEFS], over: false };

    fn add(&mut self, def: Def, vis: Vis, at: &Outline<'_>) {
        for row in self.rows.iter_mut().flatten() {
            if row.0 == def {
                row.1 = row.1.wider(vis, at);
                return;
            }
        }
        let mut carry = Some((def, vis));
        for slot in self.rows.iter_mut() {
            match (*slot, carry) {
                (None, Some(c)) => {
                    *slot = Some(c);
                    carry = None;
                    break;
                }
                (Some(x), Some(c)) if c.0 < x.0 => {
                    *slot = Some(c);
                    carry = Some(x);
                }
                _ => {}
            }
        }
        if carry.is_some() {
            self.over = true;
        }
    }

    fn len(&self) -> usize {
        self.rows.iter().flatten().count() + usize::from(self.over)
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn iter(&self) -> impl Iterator<Item = (Def, Vis)> + '_ {
        self.rows.iter().flatten().copied()
    }
}

/// A cell's value: its explicit bindings and their count, its glob bindings, and whether every
/// explicit one was written by an expansion.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Value {
    explicit: Defs,
    entries: usize,
    glob: Defs,
    only_expanded: bool,
}

impl Value {
    const EMPTY: Value = Value { explicit: Defs::EMPTY, entries: 0, glob: Defs::EMPTY, only_expanded: false };

    /// What the cell binds: the explicit ones, else the globs'.
    fn effective(&self) -> Defs {
        if planted(Plant::GlobBeatsExplicit) && !self.glob.is_empty() {
            return self.glob;
        }
        if self.entries > 0 { self.explicit } else { self.glob }
    }
}

/// A value read as an answer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Class {
    Absent,
    One(Def, Vis),
    Ambiguous(Ambiguity),
}

fn of_defs(defs: Defs) -> Class {
    if defs.over || defs.len() >= 2 {
        return Class::Ambiguous(Ambiguity::Many);
    }
    match defs.iter().next() {
        Some((def, vis)) => Class::One(def, vis),
        None => Class::Absent,
    }
}

fn classify(v: &Value, ns: Ns) -> Class {
    let glob_wins = planted(Plant::GlobBeatsExplicit) && !v.glob.is_empty();
    if !glob_wins && v.entries >= 2 {
        return Class::Ambiguous(Ambiguity::Duplicate);
    }
    if !glob_wins
        && v.entries == 1
        && ns == Ns::Macro
        && v.only_expanded
        && v.glob.iter().any(|(g, _)| v.explicit.iter().all(|(e, _)| e != g))
    {
        return Class::Ambiguous(Ambiguity::GlobVsExpanded);
    }
    of_defs(v.effective())
}

/// The digest of a cell's answer: its class and every definition it binds.
fn digest(v: &Value, ns: Ns) -> ContentId {
    let mut h = Hasher::new(DIGEST_DOMAIN);
    match classify(v, ns) {
        Class::Absent => h.update(&[0]),
        Class::One(..) => h.update(&[1]),
        Class::Ambiguous(Ambiguity::Duplicate) => h.update(&[2]),
        Class::Ambiguous(Ambiguity::Many) => h.update(&[3]),
        Class::Ambiguous(Ambiguity::GlobVsExpanded) => h.update(&[4]),
    };
    let defs = v.effective();
    for (def, vis) in defs.iter() {
        hash_def(&mut h, def, vis);
    }
    h.update(&[u8::from(defs.over)]);
    h.finish()
}

/// Whether an expansion's entry stays inside its call's summary (soundness rule 3).
fn summarised(entry: Entry<'_>, writes: Writes<'_>) -> bool {
    match entry {
        Entry::Item { name, exported: true, .. } => writes.may_export(name),
        Entry::Item { name, .. } | Entry::Use { name, .. } | Entry::ExternCrate { name, .. } => writes.may_write(name),
        Entry::Glob { .. } => matches!(writes, Writes::Any),
        Entry::Call { writes: inner, .. } => writes.covers(inner),
    }
}

fn push<T>(v: &mut AVec<'_, T>, x: T) -> Result<(), Error> {
    v.try_push(x).map_err(|r| Error::Memory(r.why))
}

fn insert<K: Ord, V>(m: &mut ABTreeMap<'_, K, V>, k: K, v: V) -> Result<(), Error> {
    m.try_insert(k, v).map(|_| ()).map_err(|r| Error::Memory(r.why))
}

/// One entry of a loaded module: as written (depth 0) or by an expansion (its depth).
#[derive(Clone, Copy)]
struct Placed<'w> {
    entry: Entry<'w>,
    depth: usize,
    expanded: bool,
}

struct Module<'a, 'w> {
    outline: &'w Outline<'w>,
    entries: AVec<'a, Placed<'w>>,
}

#[derive(Clone, Copy)]
struct CellRow<'w> {
    module: usize,
    ns: Ns,
    name: &'w str,
    value: Value,
    frozen: bool,
    /// Its range of the read log, from the logging pass that froze it.
    reads: (usize, usize),
}

/// A single import that shadows: which module, which of its entries, which namespace.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Shadow {
    module: usize,
    entry: usize,
    ns: Ns,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LogRead {
    Cell(usize),
    Call(CallRef),
    Root(CrateItemKey),
}

/// Where a module path walk ended.
#[derive(Clone, Copy)]
enum Walked {
    At(usize),
    Fail(Answer),
}

/// The machine: see the module doc.
pub struct Resolver<'a, 'w> {
    alloc: Alloc<'a>,
    modules: AVec<'a, Module<'a, 'w>>,
    module_at: ABTreeMap<'a, ModRef, usize>,
    roots: ABTreeMap<'a, CrateItemKey, &'w CrateRoot<'w>>,
    cells: AVec<'a, CellRow<'w>>,
    cell_at: ABTreeMap<'a, (usize, Ns, &'w str), usize>,
    before: ABTreeMap<'a, Shadow, ()>,
    after: ABTreeMap<'a, Shadow, ()>,
    needs: AVec<'a, Need>,
    log: AVec<'a, LogRead>,
    log_from: usize,
    logging: bool,
    open: Option<Ask<'w>>,
    counts: Counts,
}

impl<'a, 'w> Resolver<'a, 'w> {
    pub fn new(alloc: Alloc<'a>) -> Resolver<'a, 'w> {
        Resolver {
            alloc,
            modules: Vec::new_in(alloc),
            module_at: BTreeMap::new_in(alloc),
            roots: BTreeMap::new_in(alloc),
            cells: Vec::new_in(alloc),
            cell_at: BTreeMap::new_in(alloc),
            before: BTreeMap::new_in(alloc),
            after: BTreeMap::new_in(alloc),
            needs: Vec::new_in(alloc),
            log: Vec::new_in(alloc),
            log_from: 0,
            logging: false,
            open: None,
            counts: Counts::default(),
        }
    }

    pub fn counts(&self) -> Counts {
        Counts { cells: self.cells.len(), ..self.counts }
    }

    /// What the last [`Step::Needs`] needs, each once.
    pub fn needs(&self) -> &[Need] {
        self.needs.as_slice()
    }

    pub fn ask(&mut self, ask: Ask<'w>) -> Result<(), Error> {
        if self.open.is_some() {
            return Err(Error::Busy);
        }
        self.open = Some(ask);
        self.counts.asks += 1;
        Ok(())
    }

    pub fn give(&mut self, given: Given<'w>) -> Result<(), Error> {
        match given {
            Given::Outline(o) => {
                if self.module_at.contains_key(&o.module) {
                    return Err(Error::Twice(Need::Outline(o.module)));
                }
                if self.modules.len() >= MOST_MODULES {
                    return Err(Error::Bound { limit: Limit::Modules, most: MOST_MODULES });
                }
                let mut entries = Vec::new_in(self.alloc);
                entries.try_reserve(o.entries.len())?;
                for entry in o.entries {
                    push(&mut entries, Placed { entry: *entry, depth: 0, expanded: false })?;
                }
                let at = self.modules.len();
                push(&mut self.modules, Module { outline: o, entries })?;
                insert(&mut self.module_at, o.module, at)?;
                self.counts.outlines += 1;
            }
            Given::Root(root) => {
                if self.roots.contains_key(&root.krate) {
                    return Err(Error::Twice(Need::Root(root.krate)));
                }
                insert(&mut self.roots, root.krate, root)?;
                self.counts.roots += 1;
            }
            Given::Expansion { module, call, entries } => {
                let unasked = Error::Unasked { module, call };
                let m = *self.module_at.get(&module).ok_or(unasked)?;
                let md = self.modules.get_mut(m).ok_or(Error::Internal("a module index without its module"))?;
                let place = md
                    .entries
                    .iter()
                    .position(|p| !p.expanded && matches!(p.entry, Entry::Call { call: c, .. } if c == call))
                    .ok_or(unasked)?;
                let placed = *md.entries.get(place).ok_or(unasked)?;
                let Entry::Call { writes, .. } = placed.entry else { return Err(unasked) };
                let depth = placed.depth + 1;
                if depth > MOST_DEPTH {
                    return Err(Error::Bound { limit: Limit::Depth, most: MOST_DEPTH });
                }
                if entries.iter().any(|e| !summarised(*e, writes)) {
                    return Err(Error::SummaryBroken { module, call });
                }
                if let Some(slot) = md.entries.get_mut(place) {
                    slot.expanded = true;
                }
                md.entries.try_reserve(entries.len())?;
                for entry in entries {
                    push(&mut md.entries, Placed { entry: *entry, depth, expanded: false })?;
                }
                self.counts.expansions += 1;
            }
        }
        Ok(())
    }

    /// Solves the open ask as far as what is held allows.
    pub fn step(&mut self) -> Result<Step, Error> {
        let ask = self.open.ok_or(Error::NoAsk)?;
        self.needs.clear();
        let Some(from) = self.module_of(ask.from)? else { return Ok(Step::Needs) };
        self.before = BTreeMap::new_in(self.alloc);
        for _ in 0..MOST_ROUNDS {
            self.counts.rounds += 1;
            for row in self.cells.iter_mut() {
                if !row.frozen {
                    row.value = Value::EMPTY;
                }
            }
            let mut settled = false;
            for _ in 0..MOST_PASSES {
                self.counts.passes += 1;
                self.after = BTreeMap::new_in(self.alloc);
                // The ask's own walk makes the cells of its path.
                self.answer(from, ask)?;
                let mut changed = false;
                let mut c = 0;
                while let Some(row) = self.cells.get(c).copied() {
                    if !row.frozen {
                        let value = self.eval(c)?;
                        if value != row.value {
                            if let Some(r) = self.cells.get_mut(c) {
                                r.value = value;
                            }
                            changed = true;
                        }
                    }
                    c += 1;
                }
                if !changed || planted(Plant::CutEarly) {
                    settled = true;
                    break;
                }
            }
            if !settled {
                return Err(Error::Bound { limit: Limit::Passes, most: MOST_PASSES });
            }
            if !self.needs.is_empty() {
                return Ok(Step::Needs);
            }
            if let Some(lost) = self.before.keys().find(|k| !self.after.contains_key(*k)).copied() {
                let module = self.outline(lost.module)?.module;
                return Err(Error::Unstable { module, ns: lost.ns });
            }
            if self.after.len() == self.before.len() {
                return self.settle(from, ask);
            }
            core::mem::swap(&mut self.before, &mut self.after);
        }
        Err(Error::Bound { limit: Limit::Rounds, most: MOST_ROUNDS })
    }

    /// The fixpoint is reached: log each new cell's reads, freeze it, answer.
    fn settle(&mut self, from: usize, ask: Ask<'w>) -> Result<Step, Error> {
        self.logging = true;
        let mut c = 0;
        while let Some(row) = self.cells.get(c).copied() {
            if !row.frozen {
                self.log_from = self.log.len();
                let value = match self.eval(c) {
                    Ok(value) => value,
                    Err(why) => {
                        self.logging = false;
                        return Err(why);
                    }
                };
                if value != row.value {
                    self.logging = false;
                    return Err(Error::Internal("a cell moved in the pass after its fixpoint"));
                }
                let end = self.log.len();
                if let Some(r) = self.cells.get_mut(c) {
                    r.reads = (self.log_from, end);
                }
            }
            c += 1;
        }
        self.logging = false;
        for row in self.cells.iter_mut() {
            row.frozen = true;
        }
        let answer = self.answer(from, ask)?;
        self.open = None;
        Ok(Step::Answered(answer))
    }

    /// The record of a frozen cell: its key, its answer's digest, and its reads into `reads`.
    pub fn record(&self, module: ModRef, ns: Ns, name: &'w str, reads: &mut AVec<'_, Read>) -> Result<(CellKey, ContentId), Error> {
        let not = Error::NotResolved { module, ns };
        let m = *self.module_at.get(&module).ok_or(not)?;
        let c = *self.cell_at.get(&(m, ns, name)).ok_or(not)?;
        let row = self.cells.get(c).ok_or(not)?;
        if !row.frozen {
            return Err(not);
        }
        let (start, end) = row.reads;
        for read in self.log.get(start..end).ok_or(Error::Internal("a cell's reads past the log"))? {
            let read = match *read {
                LogRead::Cell(x) => {
                    let xr = self.cells.get(x).ok_or(Error::Internal("a read of a cell not made"))?;
                    Read::Cell { key: CellKey::of(self.outline(xr.module)?, xr.ns, xr.name), answer: digest(&xr.value, xr.ns) }
                }
                LogRead::Call(call) => Read::Call(call),
                LogRead::Root(krate) => {
                    let root = self.roots.get(&krate).ok_or(Error::Internal("a read of a root not given"))?;
                    Read::Root { krate, digest: root.digest }
                }
            };
            push(reads, read)?;
        }
        Ok((CellKey::of(self.outline(m)?, ns, name), digest(&row.value, ns)))
    }

    fn outline(&self, m: usize) -> Result<&'w Outline<'w>, Error> {
        self.modules.get(m).map(|x| x.outline).ok_or(Error::Internal("a module index without its module"))
    }

    fn placed(&self, m: usize, i: usize) -> Option<Placed<'w>> {
        self.modules.get(m).and_then(|x| x.entries.get(i)).copied()
    }

    fn need(&mut self, need: Need) -> Result<(), Error> {
        if self.needs.len() < MOST_NEEDS && !self.needs.contains(&need) {
            push(&mut self.needs, need)?;
        }
        Ok(())
    }

    fn note(&mut self, read: LogRead) -> Result<(), Error> {
        if self.logging && !self.log.get(self.log_from..).is_some_and(|s| s.contains(&read)) {
            push(&mut self.log, read)?;
        }
        Ok(())
    }

    fn module_of(&mut self, m: ModRef) -> Result<Option<usize>, Error> {
        if let Some(at) = self.module_at.get(&m) {
            return Ok(Some(*at));
        }
        self.need(Need::Outline(m))?;
        Ok(None)
    }

    fn root_of(&mut self, krate: CrateItemKey) -> Result<Option<&'w CrateRoot<'w>>, Error> {
        if let Some(root) = self.roots.get(&krate).copied() {
            self.note(LogRead::Root(krate))?;
            return Ok(Some(root));
        }
        self.need(Need::Root(krate))?;
        Ok(None)
    }

    fn cell(&mut self, module: usize, ns: Ns, name: &'w str) -> Result<usize, Error> {
        if let Some(c) = self.cell_at.get(&(module, ns, name)) {
            return Ok(*c);
        }
        if self.cells.len() >= MOST_CELLS {
            return Err(Error::Bound { limit: Limit::Cells, most: MOST_CELLS });
        }
        let c = self.cells.len();
        push(&mut self.cells, CellRow { module, ns, name, value: Value::EMPTY, frozen: false, reads: (0, 0) })?;
        insert(&mut self.cell_at, (module, ns, name), c)?;
        Ok(c)
    }

    fn read(&mut self, c: usize) -> Result<Value, Error> {
        self.note(LogRead::Cell(c))?;
        self.cells.get(c).map(|r| r.value).ok_or(Error::Internal("a cell index without its cell"))
    }

    /// The module a single definition leads to, as segment `at` of a path walked from `from`.
    fn descend(&mut self, from: &Outline<'_>, class: Class, at: usize) -> Result<Result<usize, Answer>, Error> {
        Ok(match class {
            Class::Absent => Err(Answer::Absent { at }),
            Class::Ambiguous(why) => Err(Answer::Ambiguous { at, why }),
            Class::One(def, vis) if !vis.visible_from(from) => Err(Answer::Private { at, def }),
            Class::One(def @ Def::Item(_), _) => Err(Answer::NotAModule { at, def }),
            Class::One(Def::Module(m), _) => match self.module_of(m)? {
                Some(i) => Ok(i),
                None => Err(Answer::Absent { at }),
            },
        })
    }

    /// Walks a module path from module `from`. `first` false refuses a first-segment anchor (the
    /// prelude's path), which bounds the one recursion (module doc).
    fn walk(&mut self, from: usize, path: Path<'w>, first: bool) -> Result<Walked, Error> {
        let o = self.outline(from)?;
        let krate = o.module.krate;
        let blocked = Walked::Fail(Answer::Absent { at: 0 });
        let (mut at, rest) = match path.anchor {
            Anchor::SelfMod | Anchor::Super(0) => (from, path.segs),
            Anchor::Crate => match self.module_of(ModRef { krate, key: o.root() })? {
                Some(m) => (m, path.segs),
                None => return Ok(blocked),
            },
            Anchor::Super(n) => {
                let Some(key) = o.chain.len().checked_sub(n).and_then(|i| o.chain.get(i)).copied() else {
                    return Ok(Walked::Fail(Answer::NoParent));
                };
                match self.module_of(ModRef { krate, key })? {
                    Some(m) => (m, path.segs),
                    None => return Ok(blocked),
                }
            }
            Anchor::Root(m) => match self.module_of(m)? {
                Some(m) => (m, path.segs),
                None => return Ok(blocked),
            },
            Anchor::Extern(name) => {
                let Some(root) = self.root_of(krate)? else { return Ok(blocked) };
                let Some(e) = root.extern_prelude.iter().find(|e| e.name == name) else {
                    return Ok(Walked::Fail(Answer::NoCrate));
                };
                match self.module_of(ModRef { krate: e.krate, key: e.root })? {
                    Some(m) => (m, path.segs),
                    None => return Ok(blocked),
                }
            }
            Anchor::Uniform | Anchor::Lexical => {
                let Some((&name, rest)) = path.segs.split_first() else { return Ok(blocked) };
                if !first {
                    return Ok(blocked);
                }
                let class = self.first(from, name, Ns::Type, path.anchor == Anchor::Uniform)?;
                match self.descend(o, class, 0)? {
                    Ok(m) => (m, rest),
                    Err(fail) => return Ok(Walked::Fail(fail)),
                }
            }
        };
        let offset = path.segs.len().saturating_sub(rest.len());
        for (i, &seg) in rest.iter().enumerate() {
            let c = self.cell(at, Ns::Type, seg)?;
            let v = self.read(c)?;
            match self.descend(o, classify(&v, Ns::Type), offset + i)? {
                Ok(m) => at = m,
                Err(fail) => return Ok(Walked::Fail(fail)),
            }
        }
        Ok(Walked::At(at))
    }

    /// A first segment `name` in module `from`: the module's binding, the extern prelude (in a
    /// `use` path, `uniform`, the two ambiguous when they differ), the standard prelude.
    fn first(&mut self, from: usize, name: &'w str, ns: Ns, uniform: bool) -> Result<Class, Error> {
        let o = self.outline(from)?;
        let c = self.cell(from, ns, name)?;
        let v = self.read(c)?;
        let local = classify(&v, ns);
        // A lexical name the module binds shadows both preludes: nothing more is read.
        if o.no_implicit_prelude || (!uniform && local != Class::Absent) {
            return Ok(local);
        }
        let Some(root) = self.root_of(o.module.krate)? else { return Ok(local) };
        if ns == Ns::Type
            && let Some(e) = root.extern_prelude.iter().find(|e| e.name == name)
        {
            let ext = Def::Module(ModRef { krate: e.krate, key: e.root });
            return Ok(match local {
                Class::Absent => Class::One(ext, Vis::Pub),
                Class::One(def, _) if uniform && def != ext => Class::Ambiguous(Ambiguity::Many),
                other => other,
            });
        }
        if local != Class::Absent {
            return Ok(local);
        }
        let Some(prelude) = root.prelude else { return Ok(local) };
        let Some(rm) = self.module_of(ModRef { krate: o.module.krate, key: o.root() })? else { return Ok(local) };
        match self.walk(rm, prelude, false)? {
            Walked::At(p) => {
                let c = self.cell(p, ns, name)?;
                let v = self.read(c)?;
                Ok(match classify(&v, ns) {
                    Class::One(_, vis) if !vis.visible_from(o) => Class::Absent,
                    other => other,
                })
            }
            Walked::Fail(_) => Ok(local),
        }
    }

    /// What `use path::target as _` in module `m` binds in `ns`, at the import's visibility.
    fn import(&mut self, m: usize, path: Path<'w>, target: &'w str, ns: Ns, vis: Vis) -> Result<Defs, Error> {
        let o = self.outline(m)?;
        let class = if matches!(path.anchor, Anchor::Uniform | Anchor::Lexical) && path.segs.is_empty() {
            self.first(m, target, ns, path.anchor == Anchor::Uniform)?
        } else {
            match self.walk(m, path, true)? {
                Walked::At(t) => {
                    let c = self.cell(t, ns, target)?;
                    let v = self.read(c)?;
                    classify(&v, ns)
                }
                Walked::Fail(_) => Class::Absent,
            }
        };
        let mut out = Defs::EMPTY;
        match class {
            Class::Absent => {}
            Class::One(def, dv) => {
                if dv.visible_from(o) {
                    out.add(def, vis.narrower(dv, o), o);
                }
            }
            Class::Ambiguous(_) => out.over = true,
        }
        Ok(out)
    }

    /// The answer to `ask` from what the cells hold now (exact once they are frozen).
    fn answer(&mut self, from: usize, ask: Ask<'w>) -> Result<Answer, Error> {
        let o = self.outline(from)?;
        let n = ask.path.segs.len();
        let class = if matches!(ask.path.anchor, Anchor::Uniform | Anchor::Lexical) && ask.path.segs.is_empty() {
            self.first(from, ask.name, ask.ns, ask.path.anchor == Anchor::Uniform)?
        } else {
            match self.walk(from, ask.path, true)? {
                Walked::Fail(answer) => return Ok(answer),
                Walked::At(t) => {
                    let c = self.cell(t, ask.ns, ask.name)?;
                    let v = self.read(c)?;
                    classify(&v, ask.ns)
                }
            }
        };
        Ok(match class {
            Class::Absent => Answer::Absent { at: n },
            Class::Ambiguous(why) => Answer::Ambiguous { at: n, why },
            Class::One(def, vis) if vis.visible_from(o) => Answer::Found { def, vis },
            Class::One(def, _) => Answer::Private { at: n, def },
        })
    }

    /// One cell's value from what the other cells hold now (module doc).
    fn eval(&mut self, c: usize) -> Result<Value, Error> {
        let row = *self.cells.get(c).ok_or(Error::Internal("a cell index without its cell"))?;
        let (m, ns, name) = (row.module, row.ns, row.name);
        let o = self.outline(m)?;
        let krate = o.module.krate;
        let is_root = o.chain.is_empty();
        // KNOWN for the name (soundness rule 1): every call that may write it, expanded.
        let mut known = true;
        let mut i = 0;
        while let Some(p) = self.placed(m, i) {
            i += 1;
            if let Entry::Call { call, writes } = p.entry {
                self.note(LogRead::Call(call))?;
                if !p.expanded && writes.may_write(name) {
                    self.need(Need::Expand { module: o.module, call })?;
                    known = false;
                }
            }
        }
        // The root's macro namespace: every exporting module known for it (rule 4).
        let mut exports: Option<&'w CrateRoot<'w>> = None;
        if ns == Ns::Macro && is_root {
            match self.root_of(krate)? {
                None => known = false,
                Some(root) => {
                    exports = Some(root);
                    for key in root.export_modules {
                        let module = ModRef { krate, key: *key };
                        let Some(x) = self.module_of(module)? else {
                            known = false;
                            continue;
                        };
                        let mut j = 0;
                        while let Some(p) = self.placed(x, j) {
                            j += 1;
                            if let Entry::Call { call, writes } = p.entry {
                                self.note(LogRead::Call(call))?;
                                if !p.expanded && writes.may_export(name) {
                                    self.need(Need::Expand { module, call })?;
                                    known = false;
                                }
                            }
                        }
                    }
                }
            }
        }
        if !known {
            return Ok(Value::EMPTY);
        }
        let ignore_expanded = planted(Plant::IgnoreExpanded);
        let mut v = Value::EMPTY;
        let mut plain = false;
        let mut i = 0;
        while let Some(p) = self.placed(m, i) {
            let at = i;
            i += 1;
            if ignore_expanded && p.depth > 0 {
                continue;
            }
            match p.entry {
                Entry::Item { name: n, spaces, vis, def, exported: false } if n == name && spaces.has(ns) => {
                    v.entries += 1;
                    v.explicit.add(def, vis, o);
                    plain |= p.depth == 0;
                }
                Entry::ExternCrate { name: n, krate: k, root, vis } if n == name && ns == Ns::Type => {
                    v.entries += 1;
                    v.explicit.add(Def::Module(ModRef { krate: k, key: root }), vis, o);
                    plain |= p.depth == 0;
                }
                Entry::Use { name: n, path, target, vis } if n == name => {
                    let got = self.import(m, path, target, ns, vis)?;
                    let key = Shadow { module: m, entry: at, ns };
                    if !got.is_empty() {
                        insert(&mut self.after, key, ())?;
                    }
                    if self.before.contains_key(&key) {
                        v.entries += 1;
                        for (def, dv) in got.iter() {
                            v.explicit.add(def, dv, o);
                        }
                        v.explicit.over |= got.over;
                        plain |= p.depth == 0;
                    }
                }
                _ => {}
            }
        }
        if let Some(root) = exports {
            for key in root.export_modules {
                let Some(x) = self.module_at.get(&ModRef { krate, key: *key }).copied() else { continue };
                let mut j = 0;
                while let Some(p) = self.placed(x, j) {
                    j += 1;
                    if ignore_expanded && p.depth > 0 {
                        continue;
                    }
                    if let Entry::Item { name: n, spaces, def, exported: true, .. } = p.entry
                        && n == name
                        && spaces.macros
                    {
                        v.entries += 1;
                        v.explicit.add(def, Vis::Pub, o);
                        plain |= p.depth == 0;
                    }
                }
            }
        }
        v.only_expanded = v.entries > 0 && !plain;
        // GLOBS, only where nothing explicit binds the name (and, in the macro namespace, beside
        // an expansion's binding, to see the ambiguity).
        if v.entries == 0 || (ns == Ns::Macro && v.only_expanded) || planted(Plant::GlobBeatsExplicit) {
            let mut i = 0;
            while let Some(p) = self.placed(m, i) {
                i += 1;
                if ignore_expanded && p.depth > 0 {
                    continue;
                }
                let Entry::Glob { path, vis } = p.entry else { continue };
                let Walked::At(t) = self.walk(m, path, true)? else { continue };
                let tc = self.cell(t, ns, name)?;
                let source = self.read(tc)?.effective();
                for (def, dv) in source.iter() {
                    if dv.visible_from(o) {
                        v.glob.add(def, vis.narrower(dv, o), o);
                    }
                }
                v.glob.over |= source.over;
            }
        }
        Ok(v)
    }
}
