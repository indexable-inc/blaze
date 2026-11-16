//! The machine's tests: a compact builder of module records ([`World`]), a driver that plays
//! the caller of the protocol ([`solve`]: answers from finished scopes, `InProgress` for a
//! module on its stack, groups on a cycle, and, given a tape, answers shuffled and given in
//! partial batches), the rule tests, and the properties.

use std::alloc::Global;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::string::String;
use std::vec::Vec;
use std::{format, vec};

use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_sim::Tape;

use crate::resolver::Plant;
use crate::{
    Answer, CrateRoot, DeclaredItem, Def, DefKind, Dep, Edition, ExternCrateDecl, ExternTarget,
    Group, MacroCallDecl, MacroKind, MacroRulesDecl, ModDecl, ModuleDecls, ModuleInfo, ModuleScope,
    NameAnswer, Namespace, Path, PathKind, Read, Rename, Reply, Resolved, Resolver, Saw, ScopeError,
    ScopeFacts, Step, TextualDef, UseDecl, UseLeaf, VisDecl,
};

mod fixtures;
mod props;
mod rules;

pub(crate) type N = &'static str;

/// A visibility as a test writes it.
#[derive(Clone, Copy, Debug)]
pub(crate) enum V {
    Priv,
    Pub,
    Crate,
    Super,
    In(&'static str),
}

impl V {
    fn decl(self) -> VisDecl<N, Global> {
        match self {
            V::Priv => VisDecl::Private,
            V::Pub => VisDecl::Public,
            V::Crate => VisDecl::Crate,
            V::Super => VisDecl::Super,
            V::In(p) => VisDecl::In(path(p)),
        }
    }
}

/// `a::b`, `crate::a`, `self::a`, `super::super::a`, `::a`, as a path.
pub(crate) fn path(text: &'static str) -> Path<N, Global> {
    let (abs, rest) = match text.strip_prefix("::") {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let mut segments: Vec<N> = rest.split("::").filter(|s| !s.is_empty()).collect();
    if abs {
        return Path { kind: PathKind::Abs, segments };
    }
    let kind = match segments.first().copied() {
        Some("crate") => {
            segments.remove(0);
            PathKind::Crate
        }
        Some("self") => {
            segments.remove(0);
            PathKind::SelfMod
        }
        Some("super") => {
            let n = segments.iter().take_while(|s| **s == "super").count();
            segments.drain(..n);
            PathKind::Super(u8::try_from(n).expect("a test path of few supers"))
        }
        Some(_) | None => PathKind::Plain,
    };
    Path { kind, segments }
}

/// The records of a few crates, written by the tests.
pub(crate) struct World {
    pub(crate) modules: BTreeMap<ItemKey, ModuleInfo<N>>,
    /// The module text of each module, and the expansion of each call.
    pub(crate) decls: BTreeMap<ItemKey, ModuleDecls<N, Global>>,
    pub(crate) roots: BTreeMap<CrateItemKey, CrateRoot<N, Global>>,
    /// Each call's module.
    pub(crate) owner: BTreeMap<ItemKey, ItemKey>,
    pub(crate) pos: BTreeMap<ItemKey, u32>,
    /// The calls expanded so far in a solve.
    pub(crate) expanded: RefCell<BTreeSet<ItemKey>>,
    next: u16,
}

pub(crate) fn crate_key(k: u8) -> CrateItemKey {
    CrateItemKey(ContentId([k; 32]))
}

impl World {
    pub(crate) fn new() -> World {
        World {
            modules: BTreeMap::new(),
            decls: BTreeMap::new(),
            roots: BTreeMap::new(),
            owner: BTreeMap::new(),
            pos: BTreeMap::new(),
            expanded: RefCell::new(BTreeSet::new()),
            next: 0,
        }
    }

    /// A fresh key of crate `k`: its first 8 bytes are the crate's.
    fn key(&mut self, k: u8) -> ItemKey {
        self.next += 1;
        let mut bytes = [0u8; 32];
        bytes[..8].fill(k);
        bytes[9..11].copy_from_slice(&self.next.to_be_bytes());
        ItemKey(ContentId(bytes))
    }

    fn module_of(&self, at: ItemKey) -> ItemKey {
        self.owner.get(&at).copied().unwrap_or(at)
    }

    fn krate_byte(at: ItemKey) -> u8 {
        at.crate_prefix()[0]
    }

    fn d(&mut self, at: ItemKey) -> &mut ModuleDecls<N, Global> {
        self.decls.get_mut(&at).expect("a test writes into a module or a call it made")
    }

    fn next_pos(&mut self, at: ItemKey) -> u32 {
        let p = self.pos.entry(at).or_insert(0);
        *p += 1;
        *p
    }

    /// The root module of a new crate `k`.
    pub(crate) fn root(&mut self, k: u8, edition: Edition) -> ItemKey {
        let root = self.key(k);
        let krate = crate_key(k);
        self.modules.insert(root, ModuleInfo { krate, parent: None, name: None, no_implicit_prelude: false });
        self.decls.insert(root, ModuleDecls::new_in(Global));
        self.roots.insert(
            krate,
            CrateRoot {
                krate,
                root,
                edition,
                no_std: false,
                no_core: false,
                extern_prelude: Vec::new(),
                deps: Vec::new(),
                macro_use: Vec::new(),
                prelude: None,
                macro_exports: Vec::new(),
                tools: Vec::new(),
            },
        );
        root
    }

    /// The crate-root record of the crate `root` is the root of.
    pub(crate) fn cr(&mut self, root: ItemKey) -> &mut CrateRoot<N, Global> {
        let krate = self.modules[&root].krate;
        self.roots.get_mut(&krate).expect("a crate made by `root`")
    }

    /// `dep_root`'s crate as a dependency of `root`'s, named `name`, in its extern prelude.
    pub(crate) fn dep(&mut self, root: ItemKey, name: N, dep_root: ItemKey) {
        let krate = self.modules[&dep_root].krate;
        let cr = self.cr(root);
        cr.extern_prelude.push((name, dep_root));
        cr.deps.push(Dep { name, krate, root: dep_root });
    }

    /// `mod name;` written at `at` (a module, or a call whose expansion it is in).
    pub(crate) fn module(&mut self, at: ItemKey, name: N, vis: V) -> ItemKey {
        self.module_with(at, name, vis, false, false)
    }

    pub(crate) fn module_with(
        &mut self,
        at: ItemKey,
        name: N,
        vis: V,
        macro_use: bool,
        no_implicit_prelude: bool,
    ) -> ItemKey {
        let parent = self.module_of(at);
        let krate = self.modules[&parent].krate;
        let module = self.key(World::krate_byte(at));
        self.modules.insert(module, ModuleInfo { krate, parent: Some(parent), name: Some(name), no_implicit_prelude });
        self.decls.insert(module, ModuleDecls::new_in(Global));
        let pos = self.next_pos(at);
        self.d(at).mods.push(ModDecl { name, module, vis: vis.decl(), macro_use, pos });
        module
    }

    pub(crate) fn item(&mut self, at: ItemKey, name: N, ns: Namespace, kind: DefKind, vis: V) -> ItemKey {
        let key = self.key(World::krate_byte(at));
        self.d(at).items.push(DeclaredItem { name, ns, def: Def { key, kind }, vis: vis.decl() });
        key
    }

    /// A `fn`.
    pub(crate) fn fun(&mut self, at: ItemKey, name: N, vis: V) -> ItemKey {
        self.item(at, name, Namespace::Values, DefKind::Other, vis)
    }

    pub(crate) fn use_(&mut self, at: ItemKey, p: &'static str, vis: V) {
        self.use_leaf(at, p, UseLeaf::Named { rename: Rename::Same, types_only: false }, vis);
    }

    pub(crate) fn use_as(&mut self, at: ItemKey, p: &'static str, to: N, vis: V) {
        self.use_leaf(at, p, UseLeaf::Named { rename: Rename::To(to), types_only: false }, vis);
    }

    pub(crate) fn glob(&mut self, at: ItemKey, p: &'static str, vis: V) {
        self.use_leaf(at, p, UseLeaf::Glob, vis);
    }

    fn use_leaf(&mut self, at: ItemKey, p: &'static str, leaf: UseLeaf<N>, vis: V) {
        let pos = self.next_pos(at);
        self.d(at).uses.push(UseDecl { path: path(p), leaf, vis: vis.decl(), pos });
    }

    pub(crate) fn extern_crate(&mut self, at: ItemKey, target: ExternTarget<N>, rename: Rename<N>, vis: V) {
        self.d(at).extern_crates.push(ExternCrateDecl { target, rename, vis: vis.decl() });
    }

    /// `macro_rules! name`.
    pub(crate) fn rules(&mut self, at: ItemKey, name: N) -> ItemKey {
        let def = self.key(World::krate_byte(at));
        let pos = self.next_pos(at);
        self.d(at).macro_rules.push(MacroRulesDecl { name, def, pos });
        def
    }

    /// A macro call; its expansion is empty until the test writes into it (at its key).
    pub(crate) fn call(&mut self, at: ItemKey, p: &'static str, kind: MacroKind) -> ItemKey {
        let call = self.key(World::krate_byte(at));
        let pos = self.next_pos(at);
        self.d(at).calls.push(MacroCallDecl { call, path: path(p), kind, pos });
        let module = self.module_of(at);
        self.owner.insert(call, module);
        self.decls.insert(call, ModuleDecls::new_in(Global));
        call
    }
}

impl ScopeFacts for World {
    type Name = N;
    type Alloc = Global;

    fn module(&self, module: ItemKey) -> Option<ModuleInfo<N>> {
        self.modules.get(&module).copied()
    }

    fn decls(&self, module: ItemKey) -> Option<&ModuleDecls<N, Global>> {
        if self.owner.contains_key(&module) {
            return None;
        }
        self.decls.get(&module)
    }

    fn crate_root(&self, krate: CrateItemKey) -> Option<&CrateRoot<N, Global>> {
        self.roots.get(&krate)
    }

    fn expansion(&self, call: ItemKey) -> Option<&ModuleDecls<N, Global>> {
        if !self.expanded.borrow().contains(&call) {
            return None;
        }
        self.decls.get(&call)
    }
}

/// A digest of an answer's content, as a record writer would give.
fn digest<T: Hash>(tag: u8, items: &[T]) -> ContentId {
    let mut h = DefaultHasher::new();
    tag.hash(&mut h);
    items.hash(&mut h);
    let mut bytes = [0u8; 32];
    bytes[..8].copy_from_slice(&h.finish().to_le_bytes());
    ContentId(bytes)
}

/// An answer as the driver holds it.
pub(crate) enum Own {
    Name(NameAnswer),
    Exports(Vec<crate::ExportEntry<N>>, ContentId),
    Textual(Vec<TextualDef<N>>, ContentId),
    InProgress,
}

impl Own {
    fn answer(&self) -> Answer<'_, N> {
        match self {
            Own::Name(a) => Answer::Name(*a),
            Own::Exports(entries, digest) => Answer::Exports { entries, digest: *digest },
            Own::Textual(defs, digest) => Answer::Textual { defs, digest: *digest },
            Own::InProgress => Answer::InProgress,
        }
    }

    pub(crate) fn saw(&self) -> Option<Saw> {
        match self {
            Own::Name(a) => Some(Saw::Name(*a)),
            Own::Exports(_, d) | Own::Textual(_, d) => Some(Saw::Digest(*d)),
            Own::InProgress => None,
        }
    }
}

/// The `macro_rules!` visible at `module`'s `mod` line, from its ancestors' scopes; `Err` names
/// an ancestor not done yet.
fn entry(w: &World, done: &BTreeMap<ItemKey, Resolved<N, Global>>, module: ItemKey) -> Result<Vec<TextualDef<N>>, ItemKey> {
    let Some(parent) = w.modules[&module].parent else { return Ok(Vec::new()) };
    let Some(p) = done.get(&parent) else { return Err(parent) };
    let mut out = entry(w, done, parent)?;
    let len = p.scope.mark(module).expect("a child's mod line is marked in its parent's scope");
    out.extend_from_slice(&p.scope.textual[..len as usize]);
    Ok(out)
}

/// What today's records answer `read`; `Err` names the module that has to be done first.
pub(crate) fn answer(
    w: &World,
    done: &BTreeMap<ItemKey, Resolved<N, Global>>,
    read: Read<N>,
) -> Result<Own, ItemKey> {
    fn of(done: &BTreeMap<ItemKey, Resolved<N, Global>>, m: ItemKey) -> Result<&ModuleScope<N, Global>, ItemKey> {
        done.get(&m).map(|r| &r.scope).ok_or(m)
    }
    let scope = move |m: ItemKey| of(done, m);
    Ok(match read {
        Read::Name { module, ns, name } => Own::Name(scope(module)?.answer(ns, name)),
        Read::Exports { module, class } => {
            let v: Vec<_> = scope(module)?.exports(class).collect();
            let d = digest(1, &v);
            Own::Exports(v, d)
        }
        Read::TextualOut { module } => {
            let v = scope(module)?.textual.to_vec();
            let d = digest(2, &v);
            Own::Textual(v, d)
        }
        Read::TextualEntry { module } => {
            let v = entry(w, done, module)?;
            let d = digest(3, &v);
            Own::Textual(v, d)
        }
    })
}

enum Machine {
    One(Resolver<N, Global>),
    Many(Group<N, Global>),
}

impl Machine {
    fn holds(&self, module: ItemKey) -> bool {
        match self {
            Machine::One(r) => r.module() == module,
            Machine::Many(g) => g.modules().any(|m| m == module),
        }
    }
}

struct Frame {
    m: Machine,
    asks: Vec<Read<N>>,
}

enum Out {
    Ask(Vec<Read<N>>),
    Expand(Vec<ItemKey>),
    Cycle(Vec<ItemKey>),
    Done(Vec<Resolved<N, Global>>),
}

fn step(m: &mut Machine, w: &World, replies: &[Reply<'_, N>]) -> Result<Out, String> {
    let shown = |e: crate::ResolveError| format!("{e}");
    Ok(match m {
        Machine::One(r) => match r.step(w, replies).map_err(shown)? {
            Step::Ask(x) => Out::Ask(x.to_vec()),
            Step::Expand(x) => Out::Expand(x.iter().map(|e| e.call).collect()),
            Step::Cycle(x) => Out::Cycle(x.to_vec()),
            Step::Done(d) => Out::Done(vec![d]),
        },
        Machine::Many(g) => match g.step(w, replies).map_err(shown)? {
            Step::Ask(x) => Out::Ask(x.to_vec()),
            Step::Expand(x) => Out::Expand(x.iter().map(|e| e.call).collect()),
            Step::Cycle(x) => Out::Cycle(x.to_vec()),
            Step::Done(d) => Out::Done(d.into_iter().collect()),
        },
    })
}

/// Every module of a world, finished.
pub(crate) struct Solved {
    pub(crate) by: BTreeMap<ItemKey, Resolved<N, Global>>,
}

impl Solved {
    pub(crate) fn scope(&self, m: ItemKey) -> &ModuleScope<N, Global> {
        &self.by[&m].scope
    }

    pub(crate) fn def(&self, m: ItemKey, ns: Namespace, name: N) -> Option<ItemKey> {
        self.scope(m).get(ns, name).map(|b| b.def.key)
    }

    pub(crate) fn errors(&self, m: ItemKey) -> &[ScopeError<N, Global>] {
        &self.scope(m).errors
    }
}

fn new_frame(w: &World, module: ItemKey, plant: Plant) -> Result<Frame, String> {
    let mut r = Resolver::new_in(module, w, Global).map_err(|e| format!("{e}"))?;
    r.set_plant(plant);
    Ok(Frame { m: Machine::One(r), asks: Vec::new() })
}

/// Resolves every module of `w` as a caller would: demand-driven, a module asked for pushed
/// on a stack, `InProgress` for one on it, a group of the frames of a cycle. With a tape, the
/// modules start in a drawn order and each step gets a drawn, shuffled part of its answers.
pub(crate) fn solve(w: &World, tape: Option<&Tape>, plant: Plant) -> Result<Solved, String> {
    w.expanded.borrow_mut().clear();
    let mut order: Vec<ItemKey> = w.modules.keys().copied().collect();
    if let Some(t) = tape {
        t.shuffle(&mut order, "module order");
    }
    let mut done: BTreeMap<ItemKey, Resolved<N, Global>> = BTreeMap::new();
    let mut stack: Vec<Frame> = Vec::new();
    let mut next = 0usize;
    let mut steps = 0u32;
    loop {
        if stack.is_empty() {
            while next < order.len() && done.contains_key(&order[next]) {
                next += 1;
            }
            if next == order.len() {
                break;
            }
            stack.push(new_frame(w, order[next], plant)?);
        }
        steps += 1;
        if steps > 100_000 {
            return Err(String::from("the driver took 100000 steps"));
        }
        let top = stack.len() - 1;
        let asks = core::mem::take(&mut stack[top].asks);
        let mut owned: Vec<(Read<N>, Own)> = Vec::new();
        let mut first_needed = None;
        for &read in asks.iter() {
            match answer(w, &done, read) {
                Ok(own) => owned.push((read, own)),
                Err(owner) => {
                    if stack.iter().any(|f| f.m.holds(owner)) {
                        owned.push((read, Own::InProgress));
                    } else if first_needed.is_none() {
                        first_needed = Some(owner);
                    }
                }
            }
        }
        if let Some(t) = tape {
            t.shuffle(&mut owned, "answer order");
            if owned.len() > 1 {
                let keep = 1 + t.index(owned.len(), "answer batch");
                owned.truncate(keep);
            }
        }
        if owned.is_empty() {
            if let Some(owner) = first_needed {
                stack[top].asks = asks;
                stack.push(new_frame(w, owner, plant)?);
                continue;
            }
        }
        let replies: Vec<Reply<'_, N>> =
            owned.iter().map(|(read, own)| Reply { read: *read, answer: own.answer() }).collect();
        match step(&mut stack[top].m, w, &replies)? {
            Out::Ask(reads) => stack[top].asks = reads,
            Out::Expand(calls) => {
                let mut expanded = w.expanded.borrow_mut();
                for call in calls {
                    expanded.insert(call);
                }
            }
            Out::Cycle(modules) => {
                let mut low = top;
                for m in modules {
                    let at = stack
                        .iter()
                        .position(|f| f.m.holds(m))
                        .ok_or_else(|| format!("a cycle names {m:x}, which is not in progress"))?;
                    low = low.min(at);
                }
                let mut members = Vec::new();
                for f in stack.drain(low..) {
                    match f.m {
                        Machine::One(r) => members.push(r),
                        Machine::Many(g) => members.extend(g.into_members()),
                    }
                }
                let g = Group::new_in(members, Global).map_err(|e| format!("{e}"))?;
                stack.push(Frame { m: Machine::Many(g), asks: Vec::new() });
            }
            Out::Done(all) => {
                stack.pop();
                for r in all {
                    done.insert(r.scope.module, r);
                }
            }
        }
    }
    Ok(Solved { by: done })
}
