//! The machine's tests: a compact builder of module records ([`World`]), a driver that plays
//! the caller of the protocol ([`solve`]: answers from finished scopes, `InProgress` for a
//! module on its stack, groups on a cycle, and, given a tape, answers shuffled and given in
//! partial batches), the rule tests, and the properties.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::string::String;
use std::vec::Vec;
use std::{format, vec};

use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_own::{Alloc, Vec as OwnVec};
use blaze_sim::Tape;
use blaze_surface::SurfaceError;

use crate::resolver::Plant;
use crate::{
    Answer, CallShape, ChildModDecl, CrateRootBuilder, CrateRootHead, DeclaredItem, DeclsBuilder,
    Def, DefKind, Dep, Edition, ExternCrateDecl, ExternTarget, Group, MacroCallDecl,
    MacroRulesDecl, ModuleInfo, NameAnswer, Namespace, Path, PathKind, Read, Rename, Reply,
    Resolved, Resolver, Saw, ScopeBuilder, ScopeError, ScopeFacts, Step,
    TextualDef, UseDecl, UseForm, WrittenVis,
};

mod asked;
mod asked_digest;
mod edits;
mod attrs;
mod blocks;
mod entry;
mod exports;
mod fixtures;
mod items;
mod items_record;
mod more;
mod names_record;
mod name_rules;
mod oracle;
/// The reference solver's seam a solver core is compared through (oracle.rs).
pub(crate) use oracle::{B, Imp, reference_imports, reference_world};
mod per_crate;
mod procs;
mod props;
mod records;
mod rules;
mod tests_record;

pub(crate) type N = &'static str;

/// A scope error as the machine makes it in the tests.
pub(crate) type Error = ScopeError<N, Path<'static, N>>;

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
    fn decl(self) -> WrittenVis<Path<'static, N>> {
        match self {
            V::Priv => WrittenVis::Private,
            V::Pub => WrittenVis::Pub,
            V::Crate => WrittenVis::Crate,
            V::Super => WrittenVis::Super,
            V::In(p) => WrittenVis::In(path(p)),
        }
    }
}

/// `a::b`, `crate::a`, `self::a`, `super::super::a`, `::a`, as a path.
pub(crate) fn path(text: &'static str) -> Path<'static, N> {
    let (abs, rest) = match text.strip_prefix("::") {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let mut segments = OwnVec::from_iter_in(rest.split("::").filter(|s| !s.is_empty()), Alloc::global());
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
            drop(segments.drain(..n));
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
    pub(crate) decls: BTreeMap<ItemKey, DeclsBuilder<'static, N>>,
    pub(crate) roots: BTreeMap<CrateItemKey, CrateRootBuilder<'static, N>>,
    /// Each call's module.
    pub(crate) owner: BTreeMap<ItemKey, ItemKey>,
    pub(crate) pos: BTreeMap<ItemKey, u32>,
    /// The calls expanded so far in a solve.
    pub(crate) expanded: RefCell<BTreeSet<ItemKey>>,
    /// Each derive macro's helper attributes.
    pub(crate) helpers: BTreeMap<ItemKey, Vec<N>>,
    next: u16,
    /// XORed into each fresh key's counter (a bijection): nonzero, a key's order is unrelated
    /// to the order the test made it in, so a child module's key sorts before its parent's as
    /// often as after (the oracle's generator draws it; the real `core` crate's keys showed a
    /// solver that read ancestors in key order).
    pub(crate) key_mix: u16,
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
            helpers: BTreeMap::new(),
            next: 0,
            key_mix: 0,
        }
    }

    /// A fresh key of crate `k`: its first 8 bytes are the crate's.
    fn key(&mut self, k: u8) -> ItemKey {
        self.next += 1;
        let mut bytes = [0u8; 32];
        bytes[..8].fill(k);
        bytes[9..11].copy_from_slice(&(self.next ^ self.key_mix).to_be_bytes());
        ItemKey(ContentId(bytes))
    }

    fn module_of(&self, at: ItemKey) -> ItemKey {
        self.owner.get(&at).copied().unwrap_or(at)
    }

    fn krate_byte(at: ItemKey) -> u8 {
        at.crate_prefix()[0]
    }

    fn d(&mut self, at: ItemKey) -> &mut DeclsBuilder<'static, N> {
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
        self.decls.insert(root, DeclsBuilder::new_in(Alloc::global()));
        let head = CrateRootHead {
            krate,
            root_module: root,
            root_file: ContentId([k; 32]),
            edition,
            no_std: false,
            no_core: false,
            arbitrary_ints: false,
            prelude: None,
        };
        let mut cr = CrateRootBuilder::new_in(head, Alloc::global());
        cr.builtin_attrs.extend_from_slice(&["derive", "inline", "test"]);
        self.roots.insert(krate, cr);
        root
    }

    /// The crate-root record of the crate `root` is the root of.
    pub(crate) fn cr(&mut self, root: ItemKey) -> &mut CrateRootBuilder<'static, N> {
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
        self.decls.insert(module, DeclsBuilder::new_in(Alloc::global()));
        let pos = self.next_pos(at);
        self.d(at).mods.push(ChildModDecl { name, module, vis: vis.decl(), macro_use, pos });
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
        self.use_leaf(at, p, UseForm::Named { rename: Rename::Same, types_only: false }, vis);
    }

    pub(crate) fn use_as(&mut self, at: ItemKey, p: &'static str, to: N, vis: V) {
        self.use_leaf(at, p, UseForm::Named { rename: Rename::To(to), types_only: false }, vis);
    }

    pub(crate) fn glob(&mut self, at: ItemKey, p: &'static str, vis: V) {
        self.use_leaf(at, p, UseForm::Glob, vis);
    }

    fn use_leaf(&mut self, at: ItemKey, p: &'static str, leaf: UseForm<N>, vis: V) {
        let pos = self.next_pos(at);
        self.d(at).uses.push(UseDecl { path: path(p), leaf, vis: vis.decl(), pos });
    }

    pub(crate) fn extern_crate(&mut self, at: ItemKey, target: ExternTarget<N>, rename: Rename<N>, vis: V) {
        self.d(at).extern_crates.push(ExternCrateDecl { target, rename, vis: vis.decl() });
    }

    /// `macro_rules! name`.
    pub(crate) fn rules(&mut self, at: ItemKey, name: N) -> ItemKey {
        self.rules_with(at, name, false)
    }

    /// `macro_rules! name`, `#[macro_export]` if `export`.
    pub(crate) fn rules_with(&mut self, at: ItemKey, name: N, export: bool) -> ItemKey {
        let def = self.key(World::krate_byte(at));
        let pos = self.next_pos(at);
        self.d(at).macro_rules.push(MacroRulesDecl { name, def, pos, export, shapes: crate::MacroShapes::FN_LIKE });
        def
    }

    /// `use p as _;`.
    pub(crate) fn use_anon(&mut self, at: ItemKey, p: &'static str) {
        self.use_leaf(at, p, UseForm::Named { rename: Rename::Underscore, types_only: false }, V::Priv);
    }

    /// A macro call; its expansion is empty until the test writes into it (at its key).
    pub(crate) fn call(&mut self, at: ItemKey, p: &'static str, kind: CallShape) -> ItemKey {
        self.call_on(at, p, kind, None)
    }

    /// A derive macro `name` with `helpers`, at the root of crate `root`.
    pub(crate) fn derive_macro(&mut self, root: ItemKey, name: N, helpers: &[N]) -> ItemKey {
        let def = self.item(root, name, Namespace::Macros, DefKind::Macro(crate::MacroShapes::DERIVE), V::Pub);
        self.helpers.insert(def, helpers.to_vec());
        def
    }

    /// An attribute or a derive on item `item` of its fragment.
    pub(crate) fn call_on(&mut self, at: ItemKey, p: &'static str, kind: CallShape, item: Option<u32>) -> ItemKey {
        let call = self.key(World::krate_byte(at));
        let pos = self.next_pos(at);
        self.d(at).calls.push(MacroCallDecl { call, path: path(p), kind, pos, item });
        let module = self.module_of(at);
        self.owner.insert(call, module);
        self.decls.insert(call, DeclsBuilder::new_in(Alloc::global()));
        call
    }
}

impl ScopeFacts for World {
    type Name = N;

    fn module(&self, module: ItemKey) -> Result<ModuleInfo<N>, SurfaceError> {
        self.modules.get(&module).copied().ok_or(SurfaceError::NoRecord { fact: "ModuleDecls", item: module })
    }

    fn decls(&self, module: ItemKey) -> Result<DeclsBuilder<'static, N>, SurfaceError> {
        let missing = SurfaceError::NoRecord { fact: "ModuleDecls", item: module };
        if self.owner.contains_key(&module) {
            return Err(missing);
        }
        self.decls.get(&module).cloned().ok_or(missing)
    }

    fn crate_root(&self, krate: CrateItemKey) -> Result<CrateRootBuilder<'static, N>, SurfaceError> {
        self.roots.get(&krate).cloned().ok_or(SurfaceError::NoCrateRecord { fact: "CrateRoot", krate })
    }

    fn expansion(&self, call: ItemKey) -> Result<Option<DeclsBuilder<'static, N>>, SurfaceError> {
        if !self.expanded.borrow().contains(&call) {
            return Ok(None);
        }
        match self.decls.get(&call) {
            Some(d) => Ok(Some(d.clone())),
            None => Err(SurfaceError::NoRecord { fact: "Expansion", item: call }),
        }
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
    Globs { edges: Vec<crate::GlobEdge>, krate: CrateItemKey, chain: Vec<ItemKey>, digest: ContentId },
    Textual(Vec<TextualDef<N>>, ContentId),
    Helpers(Vec<N>, ContentId),
    /// In progress, waiting on the module named.
    InProgress(ItemKey),
}

impl Own {
    pub(crate) fn answer(&self) -> Answer<'_, N> {
        match self {
            Own::Name(a) => Answer::Name(*a),
            Own::Globs { edges, krate, chain, digest } => Answer::Globs { edges, krate: *krate, chain, digest: *digest },
            Own::Textual(defs, digest) => Answer::Textual { defs, digest: *digest },
            Own::Helpers(names, digest) => Answer::Helpers { names, digest: *digest },
            Own::InProgress(module) => Answer::InProgress { module: *module },
        }
    }

    pub(crate) fn saw(&self) -> Option<Saw> {
        match self {
            Own::Name(a) => Some(Saw::Name(*a)),
            Own::Globs { digest: d, .. } | Own::Textual(_, d) | Own::Helpers(_, d) => Some(Saw::Digest(*d)),
            Own::InProgress(_) => None,
        }
    }
}

/// The `macro_rules!` visible at `module`'s `mod` line, from its ancestors' scopes; `Err` names
/// an ancestor not done yet.
fn entry(w: &World, done: &BTreeMap<ItemKey, Resolved<'static, N>>, module: ItemKey) -> Result<Vec<TextualDef<N>>, ItemKey> {
    let Some(parent) = w.modules[&module].parent else { return Ok(Vec::new()) };
    let Some(p) = done.get(&parent) else { return Err(parent) };
    // One level: the parent's own definitions before the `mod` line; the resolver walks up.
    let mut out = Vec::new();
    // A module whose declaring call failed is not in its parent's scope: it does not exist,
    // the driver solves it anyway, and the comparison (live modules only) says if that is wrong.
    let Some(len) = p.scope.mark(module) else { return Ok(Vec::new()) };
    out.extend_from_slice(&p.scope.textual[..len as usize]);
    Ok(out)
}

/// What today's records answer `read`; `Err` names the module that has to be done first.
pub(crate) fn answer(
    w: &World,
    done: &BTreeMap<ItemKey, Resolved<'static, N>>,
    read: Read<N>,
) -> Result<Own, ItemKey> {
    fn of<'d>(
        done: &'d BTreeMap<ItemKey, Resolved<'static, N>>,
        m: ItemKey,
    ) -> Result<&'d ScopeBuilder<'static, N>, ItemKey> {
        done.get(&m).map(|r| &r.scope).ok_or(m)
    }
    let scope = move |m: ItemKey| of(done, m);
    Ok(match read {
        Read::Name { module, ns, name } => Own::Name(NameAnswer::from(scope(module)?.get(ns, name).copied())),
        Read::Globs { module } => {
            let s = scope(module)?;
            let edges = s.globs.to_vec();
            let krate = w.modules[&module].krate;
            let mut chain = vec![module];
            while let Some(p) = w.modules[chain.last().expect("the chain starts with the module")].parent {
                chain.push(p);
            }
            let digest = blaze_surface::globs_digest(&edges, krate, &chain, Alloc::global());
            Own::Globs { edges, krate, chain, digest }
        }
        Read::TextualOut { module } => {
            let v = scope(module)?.textual.to_vec();
            let d = blaze_surface::textual_digest(&v, Alloc::global());
            Own::Textual(v, d)
        }
        Read::TextualEntry { module } => {
            let v = entry(w, done, module)?;
            let d = blaze_surface::textual_digest(&v, Alloc::global());
            Own::Textual(v, d)
        }
        Read::DeriveHelpers { derive } => {
            let v = w.helpers.get(&derive).cloned().unwrap_or_default();
            let d = digest(4, &v);
            Own::Helpers(v, d)
        }
    })
}

enum Machine {
    One(Resolver<'static, N>),
    Many(Group<'static, N>),
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
    Done(Vec<Resolved<'static, N>>),
}

fn step<F: ScopeFacts<Name = N>>(m: &mut Machine, w: &F, replies: &[Reply<'_, N>]) -> Result<Out, String> {
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
    pub(crate) by: BTreeMap<ItemKey, Resolved<'static, N>>,
    /// Each module's crate and chain of modules up to its root (itself first).
    pub(crate) krate: BTreeMap<ItemKey, CrateItemKey>,
    pub(crate) chain: BTreeMap<ItemKey, Vec<ItemKey>>,
}

impl Solved {
    pub(crate) fn scope(&self, m: ItemKey) -> &ScopeBuilder<'static, N> {
        &self.by[&m].scope
    }

    /// What `name` in `ns` names in `m`: its own binding, else through its glob edges.
    pub(crate) fn def(&self, m: ItemKey, ns: Namespace, name: N) -> Option<ItemKey> {
        self.binding(m, ns, name).map(|p| p.def.key)
    }

    /// `name` in `ns` of `m` as a reader of the records finds it (own, then through edges).
    pub(crate) fn binding(&self, m: ItemKey, ns: Namespace, name: N) -> Option<crate::Passed> {
        struct Records<'s>(&'s Solved, Namespace, N);
        impl<'s> crate::Sources<'s> for Records<'s> {
            type Stop = ItemKey;
            fn node(&mut self, source: Def) -> Result<crate::Node<'s>, ItemKey> {
                let Records(s, ns, name) = *self;
                let Some(r) = s.by.get(&source.key) else {
                    return if source.kind == DefKind::Enum { Ok(crate::Node::Value(None)) } else { Err(source.key) };
                };
                if let Some(b) = r.scope.get(ns, name) {
                    return Ok(crate::Node::Value(Some(crate::Passed { def: b.def, vis: b.vis, state: b.state })));
                }
                Ok(crate::Node::Edges { edges: &r.scope.globs, krate: s.krate[&source.key], chain: &s.chain[&source.key] })
            }
        }
        crate::through(&mut Records(self, ns, name), Def::module(m), Alloc::global()).expect("every module is solved")
    }

    pub(crate) fn errors(&self, m: ItemKey) -> &[Error] {
        &self.scope(m).errors
    }
}

fn new_frame<F: ScopeFacts<Name = N>>(w: &F, module: ItemKey, plant: Plant) -> Result<Frame, String> {
    let mut r = Resolver::new_in(module, w, Alloc::global()).map_err(|e| format!("{e}"))?;
    r.set_plant(plant);
    Ok(Frame { m: Machine::One(r), asks: Vec::new() })
}

/// Resolves every module of `w` as a caller would: demand-driven, a module asked for pushed
/// on a stack, `InProgress` for one on it, a group of the frames of a cycle. With a tape, the
/// modules start in a drawn order and each step gets a drawn, shuffled part of its answers.
pub(crate) fn solve(w: &World, tape: Option<&Tape>, plant: Plant) -> Result<Solved, String> {
    solve_with(w, w, tape, plant)
}

/// Whether two runs gave one result: the same scopes (as record bytes) and read lists, or the
/// same error.
pub(crate) fn same(a: &Result<Solved, String>, b: &Result<Solved, String>) -> Result<(), String> {
    match (a, b) {
        (Ok(a), Ok(b)) => {
            let keys_a: Vec<ItemKey> = a.by.keys().copied().collect();
            let keys_b: Vec<ItemKey> = b.by.keys().copied().collect();
            if keys_a != keys_b {
                return Err(format!("the solvers finished other modules: {keys_a:x?} and {keys_b:x?}"));
            }
            for (m, ra) in a.by.iter() {
                let rb = &b.by[m];
                let bytes = |r: &Resolved<'static, N>| r.scope.write(Alloc::global()).map(|b| b.to_vec());
                let (wa, wb) = (bytes(ra), bytes(rb));
                if wa.is_err() != wb.is_err() || wa.as_ref().ok() != wb.as_ref().ok() || ra.scope != rb.scope {
                    return Err(format!("the runs differ on the scope of {m:x}: {}", scope_diff(&ra.scope, &rb.scope)));
                }
                if ra.reads != rb.reads {
                    let only = |x: &Resolved<'static, N>, y: &Resolved<'static, N>| -> Vec<String> {
                        x.reads.entries.iter().filter(|e| !y.reads.entries.contains(e)).take(3).map(|e| format!("{e:?}")).collect()
                    };
                    return Err(format!(
                        "the runs differ on the reads of {m:x}:\n only first: {:?}\n only second: {:?}",
                        only(ra, rb),
                        only(rb, ra)
                    ));
                }
            }
            Ok(())
        }
        (Err(x), Err(y)) if x == y => Ok(()),
        (x, y) => Err(format!(
            "the solvers differ: passes {}, worklist {}",
            x.as_ref().map_or_else(|e| e.clone(), |_| String::from("done")),
            y.as_ref().map_or_else(|e| e.clone(), |_| String::from("done"))
        )),
    }
}

/// The first parts of two scopes that differ, each on its own line.
fn scope_diff(a: &ScopeBuilder<'static, N>, b: &ScopeBuilder<'static, N>) -> String {
    let mut out = String::new();
    let only = |x: &ScopeBuilder<'static, N>, y: &ScopeBuilder<'static, N>| -> Vec<String> {
        let mut v: Vec<String> =
            x.bindings.iter().filter(|e| !y.bindings.contains(e)).map(|e| format!("{} {:?}: {:?}", e.name, e.ns, e.binding)).collect();
        v.extend(x.errors.iter().filter(|e| !y.errors.contains(e)).map(|e| format!("error {e:?}")));
        if x.globs[..] != y.globs[..] {
            v.push(format!("globs {:?}", x.globs));
        }
        if x.textual[..] != y.textual[..] || x.marks[..] != y.marks[..] {
            v.push(format!("textual {:?} marks {:?}", x.textual, x.marks));
        }
        v
    };
    for (label, x, y) in [("first", a, b), ("second", b, a)] {
        out.push_str(&format!("\n only {label}:"));
        for line in only(x, y).iter().take(4) {
            out.push_str("\n   ");
            out.push_str(line);
        }
    }
    if out.matches("\n   ").count() == 0 {
        out.push_str(&format!("\n (in order or another field) first: {a:?}"));
    }
    out
}

/// [`solve`], the machines reading their facts from `facts` (the world's records, say) and
/// the driver answering reads from `w`'s finished scopes as before.
pub(crate) fn solve_with<F: ScopeFacts<Name = N>>(
    w: &World,
    facts: &F,
    tape: Option<&Tape>,
    plant: Plant,
) -> Result<Solved, String> {
    w.expanded.borrow_mut().clear();
    let order: Vec<ItemKey> = w.modules.keys().copied().collect();
    let done = drive_by::<F>(w, facts, tape, plant, BTreeMap::new(), order)?;
    Ok(w.solved(done))
}

impl World {
    /// `done`, with each module's crate and chain.
    fn solved(&self, done: BTreeMap<ItemKey, Resolved<'static, N>>) -> Solved {
        let krate = self.modules.iter().map(|(&m, i)| (m, i.krate)).collect();
        let chain = self
            .modules
            .keys()
            .map(|&m| {
                let mut c = vec![m];
                while let Some(p) = self.modules[c.last().expect("the chain starts with the module")].parent {
                    c.push(p);
                }
                (m, c)
            })
            .collect();
        Solved { by: done, krate, chain }
    }
}

/// Resolves again after an edit of `before` into `w`, as an incremental driver does: the
/// `edited` modules from their text, every other module answered by its record in `old`;
/// widened by [`crate::Again::suspects`] to the kept records a rerun's derivation may go
/// around; then each module whose read list no longer holds, again, until every one holds.
/// With a tape, every round draws its own order and batches, so two calls are two schedules.
/// With `plant` [`Plant::KeepSuspects`], the widening is left out.
pub(crate) fn resolve_again(
    before: &World,
    w: &World,
    old: &Solved,
    edited: &[ItemKey],
    tape: Option<&Tape>,
    plant: Plant,
) -> Result<Again, String> {
    let g = Alloc::global();
    let mut done: BTreeMap<ItemKey, Resolved<'static, N>> =
        old.by.iter().filter(|(m, _)| w.modules.contains_key(*m)).map(|(m, r)| (*m, r.clone())).collect();
    let parent_before = |m: ItemKey| before.modules.get(&m).and_then(|i| i.parent);
    let parent = |m: ItemKey| w.modules.get(&m).and_then(|i| i.parent);
    let again = crate::Again::new_in(old.by.iter().map(|(m, r)| (*m, &r.reads)), parent_before, g);
    let mut touched: BTreeMap<ItemKey, crate::Touched<'static, N>> = BTreeMap::new();
    // A module new in the edit has no record yet; one that read a module the edit removed has
    // nothing to answer that read; one that NAMES a removed module (a glob edge to it, a
    // binding of it) is no longer true, and a walk through it would read the removed module
    // (seed 654: a kept root's glob edge to a removed `m::m`, walked by the edited `m`).
    let removed = |d: Def| d.kind == DefKind::Module && !w.modules.contains_key(&d.key);
    let gone = |r: &Resolved<'static, N>| {
        r.reads.entries.iter().any(|e| match e.read {
            Read::TextualEntry { .. } | Read::DeriveHelpers { .. } => false,
            Read::Name { module, .. } | Read::Globs { module } | Read::TextualOut { module } => !w.modules.contains_key(&module),
        }) || r.scope.globs.iter().any(|e| removed(e.source))
            || r.scope.bindings.iter().any(|b| removed(b.binding.def))
    };
    let mut dirty: Vec<ItemKey> = w
        .modules
        .keys()
        .copied()
        .filter(|m| edited.contains(m) || done.get(m).is_none_or(|r| gone(r)))
        .collect();
    for &m in dirty.iter() {
        let t = match (before.decls.get(&m), w.decls.get(&m)) {
            (Some(a), Some(b)) => crate::Touched::of_text(a, b, g),
            _ => crate::Touched::wild(g),
        };
        touched.insert(m, t);
    }
    let mut fresh: BTreeSet<ItemKey> = BTreeSet::new();
    let mut widened: BTreeSet<ItemKey> = BTreeSet::new();
    let mut rounds = 0u32;
    while !dirty.is_empty() {
        rounds += 1;
        if rounds > 1_000 {
            return Err(String::from("resolving again took 1000 rounds"));
        }
        // One wave: the dirty modules from their text, widened until no kept record they read
        // reaches back to what they derive anew.
        loop {
            rounds += 1;
            if rounds > 1_000 {
                return Err(String::from("resolving again took 1000 rounds"));
            }
            for m in dirty.iter() {
                done.remove(m);
            }
            done = drive_by::<World>(w, w, tape, plant, done, dirty.clone())?;
            fresh.extend(dirty.iter().copied());
            for m in dirty.iter() {
                if let (Some(o), Some(n)) = (old.by.get(m), done.get(m)) {
                    touched.get_mut(m).expect("every dirty module is touched").add_changed(&o.scope, &n.scope);
                }
            }
            if plant == Plant::KeepSuspects {
                break;
            }
            let rerun: Vec<(ItemKey, &crate::ReadList<'static, N>, &crate::Touched<'static, N>)> =
                dirty.iter().map(|m| (*m, &done[m].reads, &touched[m])).collect();
            let suspects = again.suspects(&rerun, |m| !fresh.contains(&m) && done.contains_key(&m), parent);
            if suspects.is_empty() {
                break;
            }
            for &s in suspects.iter() {
                widened.insert(s);
                touched.insert(s, crate::Touched::wild(g));
                dirty.push(s);
            }
        }
        dirty = done
            .iter()
            .filter(|(_, r)| {
                !r.reads.holds(|read| match answer(w, &done, *read) {
                    Ok(own) => own.saw().unwrap_or(Saw::Name(NameAnswer::Absent)),
                    Err(_) => Saw::Name(NameAnswer::Absent),
                })
            })
            .map(|(m, _)| *m)
            .collect();
        for &m in dirty.iter() {
            touched.insert(m, crate::Touched::wild(g));
        }
    }
    Ok(Again { solved: w.solved(done), widened })
}

/// What [`resolve_again`] gives: every record, and the modules it solved again.
pub(crate) struct Again {
    pub(crate) solved: Solved,
    /// The kept records solved again because a rerun's derivation may go around them.
    pub(crate) widened: BTreeSet<ItemKey>,
}

/// Drives the machines of `order`'s modules (a drawn order with a tape) to done, every module
/// in `done` answered by its record there; `done` with them.
fn drive_by<F: ScopeFacts<Name = N>>(
    w: &World,
    facts: &F,
    tape: Option<&Tape>,
    plant: Plant,
    done: BTreeMap<ItemKey, Resolved<'static, N>>,
    order: Vec<ItemKey>,
) -> Result<BTreeMap<ItemKey, Resolved<'static, N>>, String> {
    let mut order = order;
    if let Some(t) = tape {
        t.shuffle(&mut order, "module order");
    }
    let mut done = done;
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
            stack.push(new_frame(facts, order[next], plant)?);
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
                        owned.push((read, Own::InProgress(owner)));
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
                if !w.modules.contains_key(&owner) {
                    let who: Vec<String> = stack
                        .iter()
                        .map(|f| match &f.m {
                            Machine::One(r) => format!("{:x}", r.module),
                            Machine::Many(g) => format!("{:x?}", g.modules().collect::<Vec<_>>()),
                        })
                        .collect();
                    // A kept record that names a removed module is dirty (`gone`), so no walk
                    // reaches one: a read of it is the driver's bug, named with its reader.
                    return Err(format!("a read of the removed module {owner:x}: {asks:x?}, by the machines {who:?}"));
                }
                stack[top].asks = asks;
                stack.push(new_frame(facts, owner, plant)?);
                continue;
            }
        }
        let replies: Vec<Reply<'_, N>> =
            owned.iter().map(|(read, own)| Reply { read: *read, answer: own.answer() }).collect();
        match step::<F>(&mut stack[top].m, facts, &replies)? {
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
                let g = Group::new_in(OwnVec::from_iter_in(members, Alloc::global()), Alloc::global())
                    .map_err(|e| format!("{e}"))?;
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
    Ok(done)
}
