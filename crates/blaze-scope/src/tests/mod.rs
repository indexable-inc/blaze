//! The names tests: a compact builder of module records ([`World`]), [`solve`] (the cell core,
//! the names records' one writer, over a whole world, crate by crate, given a tape its items
//! pushed in a drawn order), the rule tests, and the records' tests.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::string::String;
use std::vec::Vec;
use std::{format, vec};

use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_own::{Alloc, Bump, BumpAlloc, Erased, Vec as OwnVec};
use blaze_sim::Tape;
use blaze_surface::SurfaceError;

use crate::{
    CallShape, ChildModDecl, CrateRootBuilder, CrateRootHead, DeclaredItem, DeclsBuilder,
    Def, DefKind, Dep, Edition, ExternCrateDecl, ExternTarget, MacroCallDecl,
    MacroRulesDecl, ModuleInfo, Namespace, Path, PathKind, Rename,
    Resolved, ScopeBuilder, ScopeError, ScopeFacts,
    UseDecl, UseForm, WrittenVis,
};

mod asked;
mod asked_digest;
mod attr_after_derive;
mod attr_order;
mod attr_path_from_expansion;
mod attr_value;
mod attrs;
mod call_scratch;
mod derive_where_shape;
mod blocks;
mod block_cfg;
mod crate_cfg_off;
mod macro_blocks;
mod entry;
mod failed_call;
mod failed_record;
mod implicit_main;
mod main_call;
mod include_data;
mod include_macros;
mod include_nested_mods;
mod include_roots;
mod expansions;
mod expansion_records;
mod exports;
mod fixtures;
mod global_allocator;
mod proc_macro_harness;
mod helper_in_taker_output;
mod nested_input_tokens;
mod items;
mod items_attr_text;
mod items_record;
mod items_record_once;
mod items_grain;
mod items_head_free;
mod items_owners;
mod items_buckets;
mod kept_item_origins;
mod kept_item_shared_places;
mod impl_roots;
mod more;
mod builtin_attrs_reach_cells;
mod names_attr_3;
mod names_attr_4;
mod names_record;
mod no_spelled_dollar_crate;
mod names_inplace;
mod names_cells;
mod names_cells_keys;
mod names_scratch;
mod names_macro_use;
mod names_groups;
mod names_groups_edit;
mod names_groups_lazy;
mod names_groups_lazy_deps;
mod names_groups_lazy_textual;
mod name_rules;
mod oracle;
/// The reference solver's seam a solver core is compared through (oracle.rs).
pub(crate) use oracle::{B, Imp, reference_imports, reference_world};
mod per_crate;
mod pkg_meta_env;
mod prelude_records;
mod super_in_block;
mod written_module;
mod proc_left_package;
mod procs;
mod records;
mod module_links;
mod refused;
mod refused_rows;
mod rendered_int_dot;
mod rules;
mod scope_record;
mod statement_origins;
mod tests_record;
mod waits_on;

pub(crate) type N = &'static str;

/// A thread-safe bump arena backed by a `blaze_mem::Frame`, for tests (including multi-threaded
/// tests like `per_crate::Threads`). Reclaimed in bulk when dropped.
pub(crate) struct SyncFrame(std::sync::Mutex<blaze_mem::Frame>);

impl SyncFrame {
    pub(crate) fn new() -> Erased<SyncFrame> {
        Erased::new(SyncFrame(std::sync::Mutex::new(blaze_mem::Frame::new(blaze_mem::Tag::Nameres).expect("test frame"))))
    }

    /// A `'static` bump arena for test fixtures that must return `'static` references.
    pub(crate) fn leaked() -> Bump<'static> {
        let erased: &'static Erased<SyncFrame> = std::boxed::Box::leak(std::boxed::Box::new(SyncFrame::new()));
        erased.bump()
    }
}

// SAFETY: every call locks the inner `blaze_mem::Frame` and delegates to its `Allocator` impl.
unsafe impl core::alloc::Allocator for SyncFrame {
    fn allocate(&self, layout: core::alloc::Layout) -> Result<core::ptr::NonNull<[u8]>, core::alloc::AllocError> {
        self.0.lock().expect("test frame lock").allocate(layout)
    }

    fn allocate_zeroed(&self, layout: core::alloc::Layout) -> Result<core::ptr::NonNull<[u8]>, core::alloc::AllocError> {
        self.0.lock().expect("test frame lock").allocate_zeroed(layout)
    }

    unsafe fn deallocate(&self, ptr: core::ptr::NonNull<u8>, layout: core::alloc::Layout) {
        // SAFETY: caller's contract passed through to the inner `Frame`.
        unsafe { self.0.lock().expect("test frame lock").deallocate(ptr, layout) }
    }

    unsafe fn grow(&self, ptr: core::ptr::NonNull<u8>, old: core::alloc::Layout, new: core::alloc::Layout) -> Result<core::ptr::NonNull<[u8]>, core::alloc::AllocError> {
        // SAFETY: caller's contract passed through to the inner `Frame`.
        unsafe { self.0.lock().expect("test frame lock").grow(ptr, old, new) }
    }

    unsafe fn grow_zeroed(&self, ptr: core::ptr::NonNull<u8>, old: core::alloc::Layout, new: core::alloc::Layout) -> Result<core::ptr::NonNull<[u8]>, core::alloc::AllocError> {
        // SAFETY: caller's contract passed through to the inner `Frame`.
        unsafe { self.0.lock().expect("test frame lock").grow_zeroed(ptr, old, new) }
    }

    unsafe fn shrink(&self, ptr: core::ptr::NonNull<u8>, old: core::alloc::Layout, new: core::alloc::Layout) -> Result<core::ptr::NonNull<[u8]>, core::alloc::AllocError> {
        // SAFETY: caller's contract passed through to the inner `Frame`.
        unsafe { self.0.lock().expect("test frame lock").shrink(ptr, old, new) }
    }
}

// SAFETY: every allocation comes from the inner `blaze_mem::Frame`, which implements `BumpAlloc`
// and reclaims all allocations in bulk on drop.
unsafe impl BumpAlloc for SyncFrame {}

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
        // Built-in attributes only: `derive` and `test` are the prelude's attribute macros
        // (blaze_surface::builtin_attrs), never in the built-in attribute scope.
        cr.builtin_attrs.extend_from_slice(&["inline"]);
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

    /// `use p::*;`: `p` is the source module's path, without the `*` (a `UseDecl`'s path, as
    /// the lowering writes it); a `*` segment would be looked up as a name and find nothing.
    pub(crate) fn glob(&mut self, at: ItemKey, p: &'static str, vis: V) {
        assert!(!p.split("::").any(|s| s == "*"), "the glob path `{p}` spells its `*`: name the source module alone");
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
        self.d(at).calls.push(MacroCallDecl { call, path: path(p), kind, pos, item, role: blaze_surface::CallRole::Call });
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

    fn derive_helpers(&self, derive: ItemKey, out: &mut blaze_own::AVec<'_, N>) -> Result<(), SurfaceError> {
        out.extend(self.helpers.get(&derive).into_iter().flatten().copied());
        Ok(())
    }
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
                Ok(crate::Node::Edges { edges: blaze_surface::Globs::of(&r.scope.globs), krate: s.krate[&source.key], chain: &s.chain[&source.key] })
            }
        }
        crate::through(&mut Records(self, ns, name), Def::module(m), Alloc::global()).expect("every module is solved")
    }

    pub(crate) fn errors(&self, m: ItemKey) -> &[Error] {
        &self.scope(m).errors
    }
}



/// Every module of `w` as the cell core (the names records' one writer) writes it, crate by
/// crate in dependency order. With a tape, each crate's items are pushed in a drawn order.
pub(crate) fn solve(w: &World, tape: Option<&Tape>) -> Result<Solved, String> {
    solve_with(w, w, tape)
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

/// [`solve`], the cell core reading its facts from `facts` (the world's records, say).
pub(crate) fn solve_with<F: ScopeFacts<Name = N>>(w: &World, facts: &F, tape: Option<&Tape>) -> Result<Solved, String> {
    let done = crate::cells::solve_world(w, facts, tape, crate::cells::CellPlant::None)?;
    Ok(w.solved(done))
}

/// [`solve`] with a bug planted in the cell core (a control).
pub(crate) fn solve_planted(w: &World, plant: crate::cells::CellPlant) -> Result<Solved, String> {
    let done = crate::cells::solve_world(w, w, None, plant)?;
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


