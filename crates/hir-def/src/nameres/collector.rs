//! The core of the module-level name resolution algorithm.
//!
//! `DefCollector::collect` contains the fixed-point iteration loop which
//! resolves imports and expands macros.

use std::{
    cmp::Reverse,
    collections::{BinaryHeap, hash_map::Entry},
    iter, mem,
    ops::Range,
};

use base_db::{BuiltDependency, Crate, CrateOrigin, Db, LangCrateOrigin};
use blaze_hash::{HashMap, HashSet};
use blaze_mem::Tag;
use cfg::{CfgAtom, CfgExpr, CfgOptions};
use either::Either;
use hir_expand::{
    AttrMacroAttrIds, EditionedFileId, ErasedAstId, ExpandTo, HirFileId, InFile, MacroCallId,
    MacroCallKind, MacroDefId, MacroDefKind,
    attrs::{Attr, AttrId},
    builtin::{BuiltinDeriveExpander, find_builtin_attr, find_builtin_derive, find_builtin_macro},
    mod_path::{ModPath, PathKind},
    name::{AsName, Name},
    proc_macro::{CustomProcMacroExpander, ProcMacros},
};
use intern::{Symbol, sym};
use itertools::izip;
use la_arena::Idx;
use smallvec::SmallVec;
use span::{Edition, FileAstId, ROOT_ERASED_FILE_AST_ID, SyntaxContext};
use stdx::always;
use syntax::ast;
use triomphe::Arc;

use crate::{
    AdtId, AstId, AstIdWithPath, BuiltinDeriveImplId, BuiltinDeriveImplLoc, ConstLoc, EnumLoc,
    ExternBlockLoc, ExternCrateId, ExternCrateLoc, FunctionId, FunctionLoc, FxIndexMap, ImplLoc,
    Intern, ItemContainerId, Lookup, Macro2Id, Macro2Loc, MacroExpander, MacroId, MacroRulesId,
    MacroRulesLoc, MacroRulesLocFlags, ModuleDefId, ModuleId, ProcMacroId, ProcMacroLoc, StaticLoc,
    StructLoc, TraitLoc, TypeAliasLoc, UnionLoc, UnresolvedMacro, UseId, UseLoc, file_item_tree,
    item_scope::{
        GlobAmbiguity, GlobId, ImportId, ImportOrExternCrate, ImportOrGlob, ItemInNs,
        PerNsGlobImports, UnderscoreImport,
    },
    item_tree::{
        self, Attrs, AttrsOrCfgRef, ImportAlias, ImportKind, ItemTree, ItemTreeAstId, Macro2,
        MacroCall, MacroRules, Mod, ModItemId, ModItemKind, ModKind, StructValueNsCtor, TreeId,
        Use,
    },
    macro_call_as_call_id,
    nameres::{
        BuiltinShadowMode, DefMap, LocalDefMap, MacroSubNs, ModuleData, ModuleOrigin, ResolveMode,
        attr_resolution::{attr_macro_as_call_id, derive_macro_as_call_id},
        crate_def_map,
        diagnostics::{DefDiagnostic, PreludeError, TraitItemImport, is_glob_of_non_module},
        facts::unit::{glob_takes_over, recorded_ambiguity, widest_ambiguity},
        mod_resolution::ModDir,
        path_resolution::{PathRead, ReachedFixedPoint, ResolvePathResult},
        proc_macro::{ProcMacroDef, ProcMacroKind, parse_macro_name_and_helper_attrs},
        sub_namespace_match,
        trait_items::trait_item_per_ns,
        uses::{DeriveHelperUse, MacroCallRecord, MacroVia, NameRecords, Use as NameUse},
    },
    per_ns::{Item, Namespace, PerNs},
    tt,
    visibility::{RawVisibility, Visibility, VisibilityExplicitness},
};

const GLOB_RECURSION_LIMIT: usize = 100;
const FIXED_POINT_LIMIT: usize = 8192;

pub(super) fn collect_defs(
    db: &Db,
    def_map: DefMap,
    tree_id: TreeId,
    crate_local_def_map: Option<&LocalDefMap>,
) -> (DefMap, LocalDefMap) {
    let krate = def_map.krate.data(db);
    let cfg_options = def_map.krate.cfg_options(db);

    // populate external prelude and dependency list
    let mut deps =
        FxIndexMap::with_capacity_and_hasher(krate.dependencies.len(), Default::default());
    for dep in &krate.dependencies {
        tracing::debug!("crate dep {:?} -> {:?}", dep.name, dep.crate_id);

        deps.insert(dep.as_name(), dep);
    }

    let proc_macros = if krate.target_kind.is_proc_macro() {
        ProcMacros::get_for_crate(db, def_map.krate)
            .and_then(|proc_macros| {
                proc_macros.list(tree_id.file_id().syntax_context(db, krate.edition))
            })
            .unwrap_or_default()
    } else {
        Default::default()
    };

    let mut collector = DefCollector {
        db,
        def_map,
        local_def_map: LocalDefMap::default(),
        crate_local_def_map,
        deps,
        globs: GlobEdges::new(),
        imports: Imports::new(),
        held: Vec::new(), // alloc: empty
        path_buf: String::new(), // alloc: empty
        round_resolved: Vec::new(), // alloc: empty
        round_calls: Vec::new(), // alloc: empty
        macros_spare: Vec::new(), // alloc: empty
        unresolved_macros: Vec::new(), // alloc: empty
        expand_eager_with_unresolved_args: false,
        deferred_eager_call: false,
        mod_dirs: HashMap::default(),
        cfg_options,
        proc_macros,
        from_glob_import: Default::default(),
        skip_attrs: Default::default(),
        prev_active_attrs: Default::default(),
        unresolved_extern_crates: Default::default(),
        is_proc_macro: krate.target_kind.is_proc_macro(),
        deferred_builtin_derives: Default::default(),
        // Grown once per resolved macro path, derive helper and root `extern crate`.
        // alloc: one list per def map.
        resolved_macro_paths: Vec::new(),
        // alloc: as above.
        derive_helper_uses: Vec::new(),
        // alloc: as above.
        not_introducing: Vec::new(),
    };
    if tree_id.is_block() {
        collector.seed_with_inner(tree_id);
    } else {
        collector.seed_with_top_level();
    }
    // alloc: `DefCollector::collect`, not an iterator's
    collector.collect();
    let (mut def_map, mut local_def_map) = collector.finish();
    def_map.shrink_to_fit();
    local_def_map.shrink_to_fit();
    (def_map, local_def_map)
}

/// Computes the item tree of each of `calls`' expansions as one batch of the database's tasks
/// ([`Db::run_all`]). The results are left in the database, where the collection
/// reads them in the round's order.
fn expand_all(db: &Db, krate: Crate, calls: &[MacroCallId]) {
    if calls.len() > 1 {
        db.run_all(Tag::Expand, calls.len(), |db, at| {
            file_item_tree(db, calls[at].into(), krate);
        });
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum PartialResolvedImport {
    /// None of any namespaces is resolved
    Unresolved,
    /// One of namespaces is resolved
    Indeterminate(PerNs),
    /// All namespaces are resolved, OR it comes from other crate
    Resolved(PerNs),
}

impl PartialResolvedImport {
    fn namespaces(self) -> PerNs {
        match self {
            PartialResolvedImport::Unresolved => PerNs::none(),
            PartialResolvedImport::Indeterminate(ns) | PartialResolvedImport::Resolved(ns) => ns,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ImportSource {
    use_tree: Idx<ast::UseTree>,
    id: UseId,
    is_prelude: bool,
    kind: ImportKind,
}

#[derive(Debug, Eq, PartialEq)]
struct Import<'db> {
    path: ModPath,
    alias: Option<ImportAlias>,
    /// The `use` item's visibility in its item tree, one for all of its use trees.
    visibility: &'db RawVisibility,
    source: ImportSource,
}

impl<'db> Import<'db> {
    fn from_use(
        tree: &'db ItemTree,
        item: &'db Use,
        id: UseId,
        is_prelude: bool,
        mut cb: impl FnMut(Self),
    ) {
        let visibility = &tree[item.visibility];
        item.expand(tree, |idx, path, kind, alias| {
            cb(Self {
                path,
                alias,
                visibility,
                source: ImportSource { use_tree: idx, id, is_prelude, kind },
            });
        });
    }
}

#[derive(Debug, Eq, PartialEq)]
struct ImportDirective<'db> {
    /// The module this import directive is in.
    module_id: ModuleId,
    import: Import<'db>,
    status: PartialResolvedImport,
}

/// How `resolve_imports` picks the imports to retry. Only tests choose, to compare the two.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ResolveStrategy {
    /// The imports a write since their last attempt may have changed.
    Worklist,
    /// Every unresolved and every indeterminate import, every pass: the loop the worklist
    /// replaced, kept as the reference the worklist must agree with.
    Rounds,
}

#[cfg(test)]
thread_local! {
    /// Read when a def map's collection starts. The `TestDB` runs its batches on the calling
    /// thread, so a whole crate graph is collected with the strategy its test set.
    pub(super) static RESOLVE_STRATEGY: std::cell::Cell<ResolveStrategy> =
        const { std::cell::Cell::new(ResolveStrategy::Worklist) };
    /// Calls of `DefCollector::resolve_import` on this thread.
    pub(super) static RESOLVE_IMPORT_CALLS: std::cell::Cell<u64> =
        const { std::cell::Cell::new(0) };
}

/// Bits of a namespace mask: which of a name's namespaces a write touched, or a path read.
const TYPES: u8 = 1;
const VALUES: u8 = 2;
const MACROS: u8 = 4;
const ALL_NS: u8 = TYPES | VALUES | MACROS;

fn written_ns(defs: &PerNs) -> u8 {
    defs.types.is_some() as u8 * TYPES
        | defs.values.is_some() as u8 * VALUES
        | defs.macros.is_some() as u8 * MACROS
}

/// A part of the state an import's resolution read that collecting this def map can change.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum WaitOn {
    /// A name in the scope of a module of this def map.
    Slot(ModuleId, Name),
    /// What a path's first segment finds besides module scopes: a legacy macro, the extern
    /// prelude, the `macro_use` prelude.
    Env(Name),
}

/// One import waiting on one key, linked to the key's next waiter. 16 bytes.
#[derive(Clone, Copy)]
struct WaitEdge {
    import: u32,
    /// The attempt of `import` that waits; a later attempt makes the edge stale.
    generation: u32,
    next: u32,
    ns: u8,
}

const _: () = assert!(size_of::<WaitEdge>() == 16);

const NONE: u32 = u32::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ImportList {
    Unresolved,
    Indeterminate,
    Resolved,
}

/// Where a pass of `resolve_imports` is. A woken import whose key is at or past the phase's
/// mark runs in this phase, as the loop over every import would have reached it after the
/// write; one before the mark runs in the next pass.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Outside a pass: what is woken runs in the next one.
    Idle,
    /// Retrying unresolved imports, by index, from the given one on.
    Unresolved(u32),
    /// Retrying indeterminate imports, by the order they became so, from the given one on.
    Indeterminate(u32),
}

const PENDING: u8 = 1;
const DEFERRED: u8 = 2;

/// Every import of the def map, and the queues of the fixed point over them. An import is a
/// `u32` index into the columns; it moves between lists by its `list` entry only.
struct Imports<'db> {
    directives: Vec<ImportDirective<'db>>,
    list: Vec<ImportList>,
    /// Bumped at each attempt to resolve the import.
    generation: Vec<u32>,
    /// `PENDING` and `DEFERRED` bits.
    queued: Vec<u8>,
    /// Position in `indeterminate`, once the import was indeterminate.
    seq: Vec<u32>,
    /// The imports that were indeterminate, in the order they became so, with what each last
    /// resolved to. One resolved since stays where it is; `list` says it is done.
    indeterminate: Vec<(u32, PerNs)>,
    /// Plain imports by the module and the name they bind, in index order: head and tail of a
    /// list linked through `next_named`.
    named: HashMap<(ModuleId, Name), (u32, u32)>,
    next_named: Vec<u32>,
    phase: Phase,
    pending_unresolved: BinaryHeap<Reverse<u32>>,
    deferred_unresolved: Vec<u32>,
    pending_indeterminate: BinaryHeap<Reverse<u32>>,
    deferred_indeterminate: Vec<u32>,
    /// Head of each key's list of waiting imports in `edges`.
    waits: HashMap<WaitOn, u32>,
    edges: Vec<WaitEdge>,
    /// Head of the edges unlinked from every key, reused before `edges` grows.
    free_edge: u32,
    /// The imports, with the attempt, that read something without a key (a module's children,
    /// all its declarations): any write wakes them.
    any_waiters: Vec<(u32, u32)>,
    /// Scratch: the keys of the attempt being registered.
    keys: Vec<(WaitOn, u8)>,
    /// Scratch: whether the attempt being registered read something without a key.
    keys_any: bool,
    /// Scratch: the imports a write woke.
    woken: Vec<u32>,
    #[cfg(test)]
    rounds: bool,
}

impl<'db> Imports<'db> {
    fn new() -> Self {
        Imports {
            directives: Vec::new(),
            list: Vec::new(),
            generation: Vec::new(),
            queued: Vec::new(),
            seq: Vec::new(),
            indeterminate: Vec::new(),
            named: HashMap::default(),
            next_named: Vec::new(),
            phase: Phase::Idle,
            pending_unresolved: BinaryHeap::new(),
            deferred_unresolved: Vec::new(),
            pending_indeterminate: BinaryHeap::new(),
            deferred_indeterminate: Vec::new(),
            waits: HashMap::default(),
            edges: Vec::new(),
            free_edge: NONE,
            any_waiters: Vec::new(),
            keys: Vec::new(),
            keys_any: false,
            woken: Vec::new(),
            #[cfg(test)]
            rounds: RESOLVE_STRATEGY.get() == ResolveStrategy::Rounds,
        }
    }

    fn push(&mut self, directive: ImportDirective<'db>) {
        let at = self.directives.len() as u32;
        if directive.import.source.kind == ImportKind::Plain {
            let name = match &directive.import.alias {
                Some(ImportAlias::Alias(name)) => Some(*name),
                Some(ImportAlias::Underscore) => None,
                None => directive.import.path.segments().last().copied(),
            };
            if let Some(name) = name {
                match self.named.entry((directive.module_id, name)) {
                    Entry::Occupied(mut it) => {
                        let (_, tail) = it.get_mut();
                        self.next_named[*tail as usize] = at;
                        *tail = at;
                    }
                    Entry::Vacant(it) => {
                        it.insert((at, at));
                    }
                }
            }
        }
        self.next_named.push(NONE);
        self.directives.push(directive);
        self.list.push(ImportList::Unresolved);
        self.generation.push(0);
        self.queued.push(0);
        self.seq.push(NONE);
        self.wake(at);
    }

    /// The imports still unresolved, in order.
    fn unresolved(&self) -> impl Iterator<Item = &ImportDirective<'db>> + '_ {
        self.directives
            .iter()
            .zip(&self.list)
            .filter(|(_, list)| **list == ImportList::Unresolved)
            .map(|(directive, _)| directive)
    }

    /// The unresolved plain imports of `module` that bind `name`, in order.
    fn unresolved_named(
        &self,
        module: ModuleId,
        name: Name,
    ) -> impl Iterator<Item = &ImportDirective<'db>> + '_ {
        let mut at = self.named.get(&(module, name)).map_or(NONE, |&(head, _)| head);
        iter::from_fn(move || {
            while at != NONE {
                let this = at as usize;
                at = self.next_named[this];
                if self.list[this] == ImportList::Unresolved {
                    return Some(&self.directives[this]);
                }
            }
            None
        })
    }

    /// Queues import `at` to be retried: in this pass if its phase has not reached it yet,
    /// else in the next.
    fn wake(&mut self, at: u32) {
        #[cfg(test)]
        if self.rounds {
            return;
        }
        let i = at as usize;
        let unresolved = match self.list[i] {
            ImportList::Resolved => return,
            ImportList::Unresolved => true,
            ImportList::Indeterminate => false,
        };
        let key = if unresolved { at } else { self.seq[i] };
        let now = match (self.phase, unresolved) {
            (Phase::Idle, _) | (Phase::Unresolved(_), false) => true,
            (Phase::Unresolved(from), true) | (Phase::Indeterminate(from), false) => key >= from,
            (Phase::Indeterminate(_), true) => false,
        };
        if now {
            if self.queued[i] & PENDING == 0 {
                self.queued[i] |= PENDING;
                match unresolved {
                    true => self.pending_unresolved.push(Reverse(key)),
                    false => self.pending_indeterminate.push(Reverse(key)),
                }
            }
        } else if self.queued[i] & DEFERRED == 0 {
            self.queued[i] |= DEFERRED;
            match unresolved {
                true => self.deferred_unresolved.push(key),
                false => self.deferred_indeterminate.push(key),
            }
        }
    }

    fn begin_pass(&mut self) {
        while let Some(at) = self.deferred_unresolved.pop() {
            let i = at as usize;
            self.queued[i] &= !DEFERRED;
            if self.list[i] == ImportList::Unresolved && self.queued[i] & PENDING == 0 {
                self.queued[i] |= PENDING;
                self.pending_unresolved.push(Reverse(at));
            }
        }
        while let Some(seq) = self.deferred_indeterminate.pop() {
            let i = self.indeterminate[seq as usize].0 as usize;
            self.queued[i] &= !DEFERRED;
            if self.list[i] == ImportList::Indeterminate && self.queued[i] & PENDING == 0 {
                self.queued[i] |= PENDING;
                self.pending_indeterminate.push(Reverse(seq));
            }
        }
        self.phase = Phase::Unresolved(0);
    }

    /// The next unresolved import of this pass, by index.
    fn next_unresolved(&mut self) -> Option<u32> {
        while let Some(Reverse(at)) = self.pending_unresolved.pop() {
            let i = at as usize;
            self.queued[i] &= !PENDING;
            self.phase = Phase::Unresolved(at + 1);
            if self.list[i] == ImportList::Unresolved {
                return Some(at);
            }
        }
        None
    }

    /// The next indeterminate import of this pass, by the order they became so.
    fn next_indeterminate(&mut self) -> Option<u32> {
        while let Some(Reverse(seq)) = self.pending_indeterminate.pop() {
            let i = self.indeterminate[seq as usize].0 as usize;
            self.queued[i] &= !PENDING;
            self.phase = Phase::Indeterminate(seq + 1);
            if self.list[i] == ImportList::Indeterminate {
                return Some(seq);
            }
        }
        None
    }

    /// Starts a new attempt of import `at`: what its last one waited on no longer wakes it.
    fn attempt(&mut self, at: u32) {
        let generation = &mut self.generation[at as usize];
        *generation = generation.wrapping_add(1);
    }

    /// Makes import `at`'s current attempt wait on `self.keys` (and on any write if
    /// `self.keys_any`), and clears them.
    fn wait(&mut self, at: u32) {
        #[cfg(test)]
        if self.rounds {
            self.keys.clear();
            self.keys_any = false;
            return;
        }
        let generation = self.generation[at as usize];
        if mem::take(&mut self.keys_any) {
            self.any_waiters.push((at, generation));
        }
        for k in 0..self.keys.len() {
            let (key, ns) = self.keys[k];
            let edge = WaitEdge { import: at, generation, next: NONE, ns };
            let slot = match self.free_edge {
                NONE => {
                    self.edges.push(edge);
                    self.edges.len() as u32 - 1
                }
                free => {
                    self.free_edge = self.edges[free as usize].next;
                    self.edges[free as usize] = edge;
                    free
                }
            };
            let head = self.waits.entry(key).or_insert(NONE);
            self.edges[slot as usize].next = *head;
            *head = slot;
        }
        self.keys.clear();
    }

    /// Wakes the current waiters of `key` that read one of the namespaces `ns`, and drops them
    /// and the stale ones from its list.
    fn wake_on(&mut self, key: &WaitOn, ns: u8) {
        let Imports { waits, edges, generation, free_edge, woken, .. } = self;
        let Some(head) = waits.get_mut(key) else { return };
        let mut prev = NONE;
        let mut cur = *head;
        while cur != NONE {
            let edge = edges[cur as usize];
            let stale = generation[edge.import as usize] != edge.generation;
            let hit = !stale && edge.ns & ns != 0;
            if stale || hit {
                if hit {
                    woken.push(edge.import);
                }
                match prev {
                    NONE => *head = edge.next,
                    prev => edges[prev as usize].next = edge.next,
                }
                edges[cur as usize].next = *free_edge;
                *free_edge = cur;
            } else {
                prev = cur;
            }
            cur = edge.next;
        }
        while let Some(at) = self.woken.pop() {
            self.wake(at);
        }
    }

    /// Wakes the imports whose attempt waits on any write; stale ones are dropped.
    fn wake_any(&mut self) {
        let mut waiters = mem::take(&mut self.any_waiters);
        for (at, generation) in waiters.drain(..) {
            if self.generation[at as usize] == generation {
                self.wake(at);
            }
        }
        self.any_waiters = waiters;
    }

    /// A write of the namespaces `ns` of `name` in the scope of `module`.
    fn wake_slot(&mut self, module: ModuleId, name: Name, ns: u8) {
        if ns == 0 {
            return;
        }
        if !self.any_waiters.is_empty() {
            self.wake_any();
        }
        if !self.waits.is_empty() {
            self.wake_on(&WaitOn::Slot(module, name), ns);
        }
    }

    /// A new legacy macro, extern prelude or `macro_use` prelude entry named `name`.
    fn wake_env(&mut self, name: Name, ns: u8) {
        if !self.any_waiters.is_empty() {
            self.wake_any();
        }
        if !self.waits.is_empty() {
            self.wake_on(&WaitOn::Env(name), ns);
        }
    }

    /// A change every path may have read: the std prelude module.
    fn wake_all(&mut self) {
        for at in 0..self.directives.len() as u32 {
            self.wake(at);
        }
    }
}

/// One importer of a source, linked to the source's next importer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ImporterEdge<K, T> {
    importer: K,
    data: T,
    next: u32,
}

/// The importers of each source, each list in the order its importers came: one array of edges,
/// linked per source. An edge is appended without moving the others, so a list is read in place
/// while its importers are visited.
struct ImporterLists<K, T> {
    edges: Vec<ImporterEdge<K, T>>,
    /// Head and tail of each source's list.
    heads: HashMap<K, (u32, u32)>,
    /// The edge of each `(source, importer)`.
    by_pair: HashMap<(K, K), u32>,
}

impl<K: Copy + Eq + std::hash::Hash, T: Copy> ImporterLists<K, T> {
    fn new() -> Self {
        ImporterLists {
            edges: Vec::new(), // alloc: empty
            heads: HashMap::default(),
            by_pair: HashMap::default(),
        }
    }

    /// Adds `importer` to the importers of `source`, or, if it is one already, lets `merge`
    /// update the data it has.
    fn add(&mut self, source: K, importer: K, data: T, merge: impl FnOnce(&mut T)) {
        match self.by_pair.entry((source, importer)) {
            Entry::Occupied(it) => merge(&mut self.edges[*it.get() as usize].data),
            Entry::Vacant(it) => {
                let edge = self.edges.len() as u32;
                it.insert(edge);
                self.edges.push(ImporterEdge { importer, data, next: NONE });
                match self.heads.entry(source) {
                    Entry::Occupied(mut it) => {
                        let (_, tail) = it.get_mut();
                        self.edges[*tail as usize].next = edge;
                        *tail = edge;
                    }
                    Entry::Vacant(it) => {
                        it.insert((edge, edge));
                    }
                }
            }
        }
    }

    /// The first edge of `source`'s list, or `NONE`.
    fn first(&self, source: K) -> u32 {
        self.heads.get(&source).map_or(NONE, |&(head, _)| head)
    }

    fn edge(&self, at: u32) -> ImporterEdge<K, T> {
        self.edges[at as usize]
    }
}

/// The glob imports between modules of this def map: for each module, the modules that
/// glob-import it, with the visibility and the glob.
type GlobEdges = ImporterLists<ModuleId, (Visibility, GlobId)>;

/// A macro call a round resolved: its module, depth, container, call and the item an
/// attribute macro is on.
type ResolvedCall = (ModuleId, u32, ItemContainerId, MacroCallId, Option<AstId<ast::Item>>);

#[derive(Clone, Debug, Eq, PartialEq)]
struct MacroDirective<'db> {
    module_id: ModuleId,
    depth: u32,
    kind: MacroDirectiveKind<'db>,
    container: ItemContainerId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum MacroDirectiveKind<'db> {
    FnLike {
        ast_id: AstIdWithPath<ast::MacroCall>,
        expand_to: ExpandTo,
        ctxt: SyntaxContext,
    },
    Derive {
        ast_id: AstIdWithPath<ast::Adt>,
        derive_attr: AttrId,
        derive_pos: usize,
        ctxt: SyntaxContext,
        /// The "parent" macro it is resolved to.
        derive_macro_id: MacroCallId,
    },
    Attr {
        ast_id: AstIdWithPath<ast::Item>,
        attr_id: AttrId,
        attr: &'db Attr,
        mod_item: ModItemId,
        /* is this needed? */ tree: TreeId,
        item_tree: &'db ItemTree,
    },
}

/// A macro path of `module` that resolved to `def`: that of call `call`, or one an eager call's
/// arguments name (`None`).
#[derive(Debug)]
struct ResolvedMacroPath {
    module: ModuleId,
    path: ModPath,
    subns: MacroSubNs,
    def: MacroDefId,
    call: Option<MacroCallId>,
}

#[derive(Debug)]
struct DeferredBuiltinDerive {
    call_id: MacroCallId,
    derive: BuiltinDeriveExpander,
    module_id: ModuleId,
    depth: u32,
    container: ItemContainerId,
    derive_attr_id: AttrId,
    derive_index: u32,
    helpers_range: Range<usize>,
}

/// Walks the tree of module recursively
struct DefCollector<'db> {
    db: &'db Db,
    def_map: DefMap,
    local_def_map: LocalDefMap,
    /// Set only in case of blocks.
    crate_local_def_map: Option<&'db LocalDefMap>,
    // The dependencies of the current crate, including optional deps like `test`.
    deps: FxIndexMap<Name, &'db BuiltDependency>,
    globs: GlobEdges,
    imports: Imports<'db>,
    /// Scratch for `update`: the resolutions of each level of a glob propagation, one level's
    /// after its caller's.
    held: Vec<(Option<Name>, PerNs)>,
    /// Scratch for the paths of `mod` declarations.
    path_buf: String,
    /// Scratch for `resolve_macros`, kept across rounds.
    round_resolved: Vec<ResolvedCall>,
    round_calls: Vec<MacroCallId>,
    macros_spare: Vec<MacroDirective<'db>>,
    unresolved_macros: Vec<MacroDirective<'db>>,
    /// Whether an eager macro call whose arguments name a macro that does not resolve is expanded
    /// with that error. Until the other macros are expanded it is left for later instead, since
    /// one of them may define that macro: a `macro_rules!` an `include!` brings in.
    expand_eager_with_unresolved_args: bool,
    /// Whether the last round of `resolve_macros` left an eager call for later that way.
    deferred_eager_call: bool,
    // We'd like to avoid emitting a diagnostics avalanche when some `extern crate` doesn't
    // resolve. When we emit diagnostics for unresolved imports, we only do so if the import
    // doesn't start with an unresolved crate's name.
    unresolved_extern_crates: HashSet<Name>,
    mod_dirs: HashMap<ModuleId, ModDir>,
    cfg_options: &'db CfgOptions,
    /// List of procedural macros defined by this crate. This is read from the dynamic library
    /// built by the build system, and is the list of proc-macros we can actually expand. It is
    /// empty when proc-macro support is disabled (in which case we still do name resolution for
    /// them). The bool signals whether the proc-macro has been explicitly disabled for name-resolution.
    proc_macros: Box<[(Name, CustomProcMacroExpander, bool)]>,
    is_proc_macro: bool,
    from_glob_import: PerNsGlobImports,
    /// If we fail to resolve an attribute on a `ModItem`, we fall back to ignoring the attribute.
    /// This map is used to skip all attributes up to and including the one that failed to resolve,
    /// in order to not expand them twice.
    ///
    /// This also stores the attributes to skip when we resolve derive helpers and non-macro
    /// non-builtin attributes in general.
    // FIXME: There has to be a better way to do this
    skip_attrs: HashMap<InFile<ModItemId>, AttrId>,
    /// When we expand attributes, we need to censor all previous active attributes
    /// on the same item. Therefore, this holds all active attributes that we already
    /// expanded.
    prev_active_attrs: HashMap<AstId<ast::Item>, SmallVec<[AttrId; 1]>>,
    /// To save memory, we do not really expand builtin derives. Instead, we save them as a `BuiltinDeriveImplId`.
    ///
    /// However, we can only do that when the derive is directly above the item, and there is no attribute in between.
    /// Otherwise, all sorts of weird things can happen, like the item name resolving to something else.
    deferred_builtin_derives: HashMap<AstId<ast::Item>, Vec<DeferredBuiltinDerive>>,
    /// Each macro path that resolved, in the order it did.
    resolved_macro_paths: Vec<ResolvedMacroPath>,
    derive_helper_uses: Vec<DeriveHelperUse>,
    /// The `extern crate`s of the crate root that do not introduce their name.
    not_introducing: Vec<ExternCrateId>,
}

impl<'db> DefCollector<'db> {
    fn seed_with_top_level(&mut self) {
        let _p = tracing::info_span!("seed_with_top_level").entered();

        let file_id = self.def_map.krate.root_file_id(self.db);
        let item_tree = file_item_tree(self.db, file_id.into(), self.def_map.krate);
        let attrs = match item_tree.top_level_attrs() {
            Some(AttrsOrCfgRef::Enabled { attrs }) => attrs,
            None => Attrs::EMPTY,
            Some(AttrsOrCfgRef::CfgDisabled(_, attrs)) => attrs,
        };
        let crate_data = Arc::get_mut(&mut self.def_map.data).unwrap();

        // Process other crate-level attributes.
        for attr in &*attrs {
            let Some(attr_name) = attr.path.as_ident() else { continue };

            match () {
                () if *attr_name == sym::recursion_limit => {
                    if let Some(limit) = attr.string_value()
                        && let Ok(limit) = limit.parse()
                    {
                        crate_data.recursion_limit = Some(limit);
                    }
                }
                () if *attr_name == sym::crate_type => {
                    if attr.string_value() == Some("proc-macro") {
                        self.is_proc_macro = true;
                    }
                }
                () if *attr_name == sym::no_core => crate_data.no_core = true,
                () if *attr_name == sym::no_std => crate_data.no_std = true,
                () if *attr_name == sym::rustc_coherence_is_core => {
                    crate_data.rustc_coherence_is_core = true;
                }
                () if *attr_name == sym::feature => {
                    let features =
                        attr.parse_path_comma_token_tree(self.db).into_iter().flatten().filter_map(
                            |(feat, _, _)| match feat.segments() {
                                [name] => Some(*name.symbol()),
                                _ => None,
                            },
                        );
                    features.for_each(|feature| crate_data.unstable_features.enable(feature));
                }
                () if *attr_name == sym::register_tool => {
                    if let Some(ident) = attr.single_ident_value() {
                        crate_data.registered_tools.push(ident.sym);
                        cov_mark::hit!(register_tool);
                    }
                }
                () => (),
            }
        }
        crate_data.proc_macro = self.is_proc_macro;

        let mut prelude = Vec::with_capacity(self.deps.len());
        for (name, dep) in &self.deps {
            // Add all
            if dep.is_prelude() {
                // This is a bit confusing but the gist is that `no_core` and `no_std` remove the
                // sysroot dependence on `core` and `std` respectively. Our `CrateGraph` is eagerly
                // constructed with them in place no matter what though, since at that point we
                // don't do pre-configured attribute resolution yet.
                // So here check if we are no_core / no_std and we are trying to add the
                // corresponding dep from the sysroot

                // Depending on the crate data of a dependency seems bad for incrementality, but
                // we only do that for sysroot crates (this is why the order of the `&&` is important)
                // - which are normally standard library crate, which realistically aren't going
                // to have their crate ID invalidated, because they stay on the same root file and
                // they're dependencies of everything else, so if some collision miraculously occurs
                // we will resolve it by disambiguating the other crate.
                let skip = dep.is_sysroot()
                    && match dep.crate_id.data(self.db).origin {
                        CrateOrigin::Lang(LangCrateOrigin::Core) => crate_data.no_core,
                        CrateOrigin::Lang(LangCrateOrigin::Std) => {
                            crate_data.no_core || crate_data.no_std
                        }
                        _ => false,
                    };
                if !skip {
                    prelude.push((*name, dep.crate_id));
                }
            }
        }
        // This eagerly draws a dependency edge between the crate def maps even if the things
        // the crates are not used. The def maps are collected as one batch of the database's
        // tasks, then put in the extern prelude in the order of the dependencies.
        if prelude.len() > 1 {
            self.db.run_all(Tag::Nameres, prelude.len(), |db, at| {
                crate_def_map(db, prelude[at].1);
            });
        }
        for (name, krate) in prelude {
            self.local_def_map
                .extern_prelude
                .insert(name, (crate_def_map(self.db, krate).root_module_id(), None));
        }

        self.inject_std_extern_crate();
        self.inject_prelude();

        let items = item_tree.top_level_items();
        let items = if let Some(AttrsOrCfgRef::CfgDisabled(cfg_expr, _)) = item_tree.top_level_attrs() {
            self.def_map.diagnostics.push(DefDiagnostic::unconfigured_code(
                self.def_map.root,
                InFile::new(file_id.into(), ROOT_ERASED_FILE_AST_ID),
                // clone: only for a crate whose root is cfg'd out, into its diagnostic
                cfg_expr.clone(),
            ));
            // rustc takes the crate's items away before it injects its test harness, so a
            // test crate of `#![cfg(target_arch = "wasm32")]` still has a `main` that runs
            // no tests. The module the compiler appended to the root is all that stays.
            let root = self.db.file_text(file_id.file_id(self.db));
            let appended = root.appended_root_module(self.db);
            let Some(item) = appended.and_then(|name| {
                items.last().filter(|&&item| {
                    matches!(item_tree.index(item),
                        ModItemKind::Mod(_, module) if item_tree[module.name].as_str() == name)
                })
            }) else {
                return;
            };
            std::slice::from_ref(item)
        } else {
            items
        };

        let module_id = self.def_map.root;
        ModCollector {
            def_collector: self,
            macro_depth: 0,
            module_id,
            tree_id: TreeId::new(file_id.into(), None),
            item_tree,
            mod_dir: ModDir::root(),
        }
        .collect_in_top_module(items);
        Arc::get_mut(&mut self.def_map.data).unwrap().shrink_to_fit();
    }

    fn seed_with_inner(&mut self, tree_id: TreeId) {
        let item_tree = tree_id.item_tree(self.db, self.def_map.krate);
        let is_cfg_enabled =
            matches!(item_tree.top_level_attrs(), Some(AttrsOrCfgRef::Enabled { .. }) | None);
        if is_cfg_enabled {
            self.inject_prelude();

            let module_id = self.def_map.root;
            // A block `DefMap` is collected from scratch, but it may itself live inside a macro
            // expansion. Continue that chain's budget instead of restarting it, or a macro
            // expanding to an item that invokes the macro again never reaches the limit.
            let macro_depth = tree_id.file_id().macro_expansion_depth(self.db);
            ModCollector {
                def_collector: self,
                macro_depth,
                module_id,
                tree_id,
                item_tree,
                mod_dir: ModDir::root(),
            }
            .collect_in_top_module(item_tree.top_level_items());
        }
    }

    fn resolution_loop(&mut self) {
        let _p = tracing::info_span!("DefCollector::resolution_loop").entered();

        // main name resolution fixed-point loop.
        let mut i = 0;
        'resolve_attr: loop {
            let _p = tracing::info_span!("resolve_macros loop").entered();
            'resolve_macros: loop {
                blaze_query::db::unwind_if_cancelled(self.db);

                {
                    let _p = tracing::info_span!("resolve_imports loop").entered();

                    'resolve_imports: loop {
                        if self.resolve_imports() == ReachedFixedPoint::Yes {
                            break 'resolve_imports;
                        }
                    }
                }
                if self.resolve_macros() == ReachedFixedPoint::Yes {
                    // What is left may be eager calls waiting for a macro of their arguments: now
                    // nothing else can define it, they are expanded with the error.
                    // Only when this round left one for later, so that a crate with a macro that
                    // is missing for good does not pay a round more.
                    if self.expand_eager_with_unresolved_args || !self.deferred_eager_call {
                        break 'resolve_macros;
                    }
                    self.expand_eager_with_unresolved_args = true;
                    continue 'resolve_macros;
                }
                self.expand_eager_with_unresolved_args = false;

                i += 1;
                if i > FIXED_POINT_LIMIT {
                    panic!(
                        "name resolution of {:?} still made progress after {FIXED_POINT_LIMIT} rounds",
                        self.def_map.krate
                    );
                }
            }

            if self.reseed_with_unresolved_attribute() == ReachedFixedPoint::Yes {
                break 'resolve_attr;
            }
            // The reseed brought in directives (an eager call, an `include!` that defines the
            // macro its arguments name): they get the same wait as those of the first round.
            self.expand_eager_with_unresolved_args = false;
        }
    }

    fn collect(&mut self) {
        let _p = tracing::info_span!("DefCollector::collect").entered();

        self.resolution_loop();

        // show unresolved imports in completion, etc
        for at in 0..self.imports.directives.len() as u32 {
            if self.imports.list[at as usize] == ImportList::Unresolved {
                self.record_resolved_import(at);
            }
        }

        if self.is_proc_macro {
            // A crate exporting procedural macros is not allowed to export anything else.
            //
            // Additionally, while the proc macro entry points must be `pub`, they are not publicly
            // exported in type/value namespace. This function reduces the visibility of all items
            // in the crate root that aren't proc macros.
            let module_id = self.def_map.root_module_id();
            let root = &mut self.def_map.modules.building()[module_id];
            root.scope_mut().censor_non_proc_macros(self.def_map.krate);
        }
    }

    /// When the fixed-point loop reaches a stable state, we might still have
    /// some unresolved attributes left over. This takes one of them, and feeds
    /// the item it's applied to back into name resolution.
    ///
    /// This effectively ignores the fact that the macro is there and just treats the items as
    /// normal code.
    ///
    /// This improves UX for unresolved attributes, and replicates the
    /// behavior before we supported proc. attribute macros.
    fn reseed_with_unresolved_attribute(&mut self) -> ReachedFixedPoint {
        cov_mark::hit!(unresolved_attribute_fallback);

        let unresolved_attr =
            self.unresolved_macros.iter().enumerate().find_map(|(idx, directive)| match &directive
                .kind
            {
                MacroDirectiveKind::Attr { ast_id, mod_item, attr_id, attr, tree, item_tree } => {
                    self.def_map.diagnostics.push(DefDiagnostic::unresolved_macro_call(
                        directive.module_id,
                        MacroCallKind::Attr {
                            ast_id: ast_id.ast_id,
                            attr_args: None,
                            censored_attr_ids: AttrMacroAttrIds::from_one(*attr_id),
                        },
                        ModPath::clone(&attr.path),
                    ));

                    self.skip_attrs.insert(ast_id.ast_id.with_value(*mod_item), *attr_id);

                    Some((idx, directive, *mod_item, *tree, *item_tree))
                }
                _ => None,
            });

        match unresolved_attr {
            Some((
                pos,
                &MacroDirective { module_id, depth, container, .. },
                mod_item,
                tree_id,
                item_tree,
            )) => {
                let mod_dir = self.mod_dirs[&module_id];
                ModCollector {
                    def_collector: self,
                    macro_depth: depth,
                    module_id,
                    tree_id,
                    item_tree,
                    mod_dir,
                }
                .collect(&[mod_item], container);

                self.unresolved_macros.swap_remove(pos);
                // Continue name resolution with the new data.
                ReachedFixedPoint::No
            }
            None => ReachedFixedPoint::Yes,
        }
    }

    fn inject_prelude(&mut self) {
        // See compiler/rustc_builtin_macros/src/standard_library_imports.rs

        if self.def_map.data.no_core {
            // libcore does not get a prelude.
            return;
        }

        let krate = match self.def_map.data.no_std {
            true => Name::new_symbol_root(sym::core),
            false => Name::new_symbol_root(sym::std),
        };
        if !self.local_def_map().extern_prelude().any(|(name, _)| *name == krate) {
            let root = self.def_map.root;
            let diagnostic = DefDiagnostic::prelude(root, krate, PreludeError::CrateNotFound);
            self.def_map.diagnostics.push(diagnostic);
            return;
        }

        let edition = match self.def_map.data.edition {
            Edition::Edition2015 => Name::new_symbol_root(sym::rust_2015),
            Edition::Edition2018 => Name::new_symbol_root(sym::rust_2018),
            Edition::Edition2021 => Name::new_symbol_root(sym::rust_2021),
            Edition::Edition2024 => Name::new_symbol_root(sym::rust_2024),
        };

        let path_kind = match self.def_map.data.edition {
            Edition::Edition2015 => PathKind::Plain,
            _ => PathKind::Abs,
        };
        let path = ModPath::from_segments(
            path_kind,
            [krate, Name::new_symbol_root(sym::prelude), edition],
        );

        let (per_ns, _) = self.def_map.resolve_path(
            self.crate_local_def_map.unwrap_or(&self.local_def_map),
            self.db,
            self.def_map.root_module_id(),
            &path,
            BuiltinShadowMode::Other,
            None,
        );

        match per_ns.types {
            Some(Item { def: ModuleDefId::ModuleId(m), import, .. }) => {
                self.def_map.prelude = Some((m, import.and_then(ImportOrExternCrate::use_)));
                self.imports.wake_all();
            }
            _ => {
                let root = self.def_map.root;
                let diagnostic = DefDiagnostic::prelude(root, krate, PreludeError::NoPreludeModule);
                self.def_map.diagnostics.push(diagnostic);
            }
        }
    }

    /// Defines the `extern crate std;` (`core` under `no_std`) that rustc inserts as the first
    /// item of the crate root, as a private name in the root's type namespace.
    ///
    /// See compiler/rustc_builtin_macros/src/standard_library_imports.rs: rustc gives that item
    /// a call-site name in edition 2015 and a def-site one from 2018 on. So in 2015 paths that
    /// look the name up in the root module itself, such as `crate::std::...`, `super::std::...`
    /// or a macro's `$crate::std::...` (lewton 0.10's own `try!`), find it, and in later editions
    /// they do not (rustc: "could not find `std` in the crate root"). Plain paths reach the crate
    /// through the extern prelude in every edition, which `seed_with_top_level` fills; a
    /// dependency cargo passes `noprelude` (build-std's `--extern noprelude:std`) is put there by
    /// this item, as by any `extern crate` of the root, and the std prelude is injected after it.
    /// `core` is in the extern prelude even when it is not the item's (rustc's `Resolver::new`
    /// adds `core` unless `no_core`, then `std` unless `no_std`), so a 2018 crate's `use core::..`
    /// finds build-std's `noprelude:core`.
    fn inject_std_extern_crate(&mut self) {
        if self.def_map.data.no_core {
            return;
        }
        let core = Name::new_symbol_root(sym::core);
        if !self.local_def_map().extern_prelude.contains_key(&core)
            && let Some(dep) = self.deps.get(&core)
        {
            let krate_root = crate_def_map(self.db, dep.crate_id).root_module_id();
            self.local_def_map.extern_prelude.insert(core, (krate_root, None));
        }
        let name = match self.def_map.data.no_std {
            true => Name::new_symbol_root(sym::core),
            false => Name::new_symbol_root(sym::std),
        };
        let krate_root = match self.local_def_map().extern_prelude.get(&name) {
            Some(&(krate_root, _)) => krate_root,
            // Without the crate rustc stops with "can't find crate"; there is no item to point
            // at then, and `inject_prelude` reports it.
            None => {
                let Some(dep) = self.deps.get(&name) else { return };
                let krate_root = crate_def_map(self.db, dep.crate_id).root_module_id();
                self.local_def_map.extern_prelude.insert(name, (krate_root, None));
                krate_root
            }
        };
        if self.def_map.data.edition != Edition::Edition2015 {
            return;
        }
        let root = self.def_map.root;
        let vis = Visibility::Module(root, VisibilityExplicitness::Implicit);
        self.update(root, &[(Some(name), PerNs::types(krate_root.into(), vis, None))], vis, None);
    }

    fn local_def_map(&mut self) -> &LocalDefMap {
        self.crate_local_def_map.unwrap_or(&self.local_def_map)
    }

    /// Adds a definition of procedural macro `name` to the root module.
    ///
    /// # Notes on procedural macro resolution
    ///
    /// Procedural macro functionality is provided by the build system: It has to build the proc
    /// macro and pass the resulting dynamic library to rust-analyzer.
    ///
    /// When procedural macro support is enabled, the list of proc macros exported by a crate is
    /// known before we resolve names in the crate. This list is stored in `self.proc_macros` and is
    /// derived from the dynamic library.
    ///
    /// However, we *also* would like to be able to at least *resolve* macros on our own, without
    /// help by the build system. So, when the macro isn't found in `self.proc_macros`, we instead
    /// use a dummy expander that always errors. This comes with the drawback of macros potentially
    /// going out of sync with what the build system sees (since we resolve using VFS state, but
    /// Cargo builds only on-disk files). We could and probably should add diagnostics for that.
    fn export_proc_macro(&mut self, def: ProcMacroDef, ast_id: AstId<ast::Fn>, fn_id: FunctionId) {
        let kind = def.kind.to_basedb_kind();
        let (expander, kind) = match self.proc_macros.iter().find(|(n, _, _)| n == &def.name) {
            Some(&(_, _, true)) => (CustomProcMacroExpander::disabled(), kind),
            Some(&(_, expander, false)) => (expander, kind),
            None => (CustomProcMacroExpander::missing_expander(), kind),
        };

        let proc_macro_id = ProcMacroLoc {
            container: self.def_map.root_module_id(),
            id: ast_id,
            expander,
            kind,
            edition: self.def_map.data.edition,
        }
        .intern(self.db);
        self.define_proc_macro(def.name, proc_macro_id);
        let crate_data = Arc::get_mut(&mut self.def_map.data).unwrap();
        if let ProcMacroKind::Derive { helpers } = def.kind {
            crate_data.exported_derives.insert(proc_macro_id.into(), helpers);
        }
        crate_data.fn_proc_macro_mapping.insert(fn_id, proc_macro_id);
        crate_data.fn_proc_macro_mapping_back.insert(proc_macro_id, fn_id);
    }

    /// Define a macro with `macro_rules`.
    ///
    /// It will define the macro in legacy textual scope, and if it has `#[macro_export]`,
    /// then it is also defined in the root module scope.
    /// You can `use` or invoke it by `crate::macro_name` anywhere, before or after the definition.
    ///
    /// It is surprising that the macro will never be in the current module scope.
    /// These code fails with "unresolved import/macro",
    /// ```rust,compile_fail
    /// mod m { macro_rules! foo { () => {} } }
    /// use m::foo as bar;
    /// ```
    ///
    /// ```rust,compile_fail
    /// macro_rules! foo { () => {} }
    /// self::foo!();
    /// crate::foo!();
    /// ```
    ///
    /// Well, this code compiles, because the plain path `foo` in `use` is searched
    /// in the legacy textual scope only.
    /// ```rust
    /// macro_rules! foo { () => {} }
    /// use foo as bar;
    /// ```
    fn define_macro_rules(
        &mut self,
        module_id: ModuleId,
        name: Name,
        macro_: MacroRulesId,
        export: bool,
    ) {
        // Textual scoping
        self.define_legacy_macro(module_id, name, macro_.into());

        // Module scoping
        // In Rust, `#[macro_export]` macros are unconditionally visible at the
        // crate root, even if the parent modules is **not** visible.
        if export {
            let module_id = self.def_map.root;
            self.def_map.modules.building()[module_id].scope_mut().declare(macro_.into());
            self.update(
                module_id,
                &[(Some(name), PerNs::macros(macro_.into(), Visibility::Public, None))],
                Visibility::Public,
                None,
            );
        }
    }

    /// Define a legacy textual scoped macro in module
    ///
    /// We use a map `legacy_macros` to store all legacy textual scoped macros visible per module.
    /// It will clone all macros from parent legacy scope, whose definition is prior to
    /// the definition of current module.
    /// And also, `macro_use` on a module will import all legacy macros visible inside to
    /// current legacy scope, with possible shadowing.
    fn define_legacy_macro(&mut self, module_id: ModuleId, name: Name, mac: MacroId) {
        // Always shadowing
        self.def_map.modules.building()[module_id].scope_mut().define_legacy_macro(name, mac);
        self.imports.wake_env(name, MACROS);
    }

    /// Define a macro 2.0 macro
    ///
    /// The scoped of macro 2.0 macro is equal to normal function
    fn define_macro_def(
        &mut self,
        module_id: ModuleId,
        name: Name,
        macro_: Macro2Id,
        vis: &RawVisibility,
    ) {
        let vis = self
            .def_map
            .resolve_visibility(
                self.crate_local_def_map.unwrap_or(&self.local_def_map),
                self.db,
                module_id,
                vis,
                false,
            )
            .unwrap_or(Visibility::Public);
        self.def_map.modules.building()[module_id].scope_mut().declare(macro_.into());
        self.update(
            module_id,
            &[(Some(name), PerNs::macros(macro_.into(), Visibility::Public, None))],
            vis,
            None,
        );
    }

    /// Define a proc macro
    ///
    /// A proc macro is similar to normal macro scope, but it would not visible in legacy textual scoped.
    /// And unconditionally exported.
    fn define_proc_macro(&mut self, name: Name, macro_: ProcMacroId) {
        let module_id = self.def_map.root;
        self.def_map.modules.building()[module_id].scope_mut().declare(macro_.into());
        self.update(
            module_id,
            &[(Some(name), PerNs::macros(macro_.into(), Visibility::Public, None))],
            Visibility::Public,
            None,
        );
    }

    /// Import exported macros from another crate. `names`, if `Some(_)`, specifies the name of
    /// macros to be imported. Otherwise this method imports all exported macros.
    ///
    /// Exported macros are just all macros in the root module scope.
    /// Note that it contains not only all `#[macro_export]` macros, but also all aliases
    /// created by `use` in the root module, ignoring the visibility of `use`.
    fn import_macros_from_extern_crate(
        &mut self,
        krate: Crate,
        names: Option<Vec<Name>>,
        extern_crate: Option<ExternCrateId>,
    ) {
        let def_map = crate_def_map(self.db, krate);
        // `#[macro_use]` brings macros into macro_use prelude. Yes, even non-`macro_rules!`
        // macros.
        let root_scope = def_map.module(self.db, def_map.root).scope(self.db);
        match names {
            Some(names) => {
                for name in names {
                    // FIXME: Report diagnostic on 404.
                    if let Some(def) = root_scope.get(&name).take_macros() {
                        self.def_map.macro_use_prelude.insert(name, (def, extern_crate));
                        self.imports.wake_env(name, MACROS);
                    }
                }
            }
            None => {
                for (name, it) in root_scope.macros() {
                    let name: Name = *name;
                    self.def_map.macro_use_prelude.insert(name, (it.def, extern_crate));
                    self.imports.wake_env(name, MACROS);
                }
            }
        }
    }

    /// One pass over the imports: every unresolved one in order, then every indeterminate one
    /// in the order they became so, as the loop over all of them was. Only those a write since
    /// their last attempt woke are retried: each other one would resolve as it did, recording
    /// nothing. So the pass records what that loop records, in its order.
    fn resolve_imports(&mut self) -> ReachedFixedPoint {
        #[cfg(test)]
        if self.imports.rounds {
            return self.resolve_imports_in_rounds();
        }
        let mut res = ReachedFixedPoint::Yes;
        self.imports.begin_pass();
        while let Some(at) = self.imports.next_unresolved() {
            if self.retry_unresolved(at) {
                res = ReachedFixedPoint::No;
            }
        }
        // Macro expansion can expose a new binding or change the visibility of an existing
        // one without adding a namespace. Revisit partial imports until their bindings settle.
        self.imports.phase = Phase::Indeterminate(0);
        while let Some(seq) = self.imports.next_indeterminate() {
            if self.retry_indeterminate(seq) {
                res = ReachedFixedPoint::No;
            }
        }
        self.imports.phase = Phase::Idle;
        res
    }

    /// The loop the worklist replaced: every unresolved and every indeterminate import, each
    /// pass.
    #[cfg(test)]
    fn resolve_imports_in_rounds(&mut self) -> ReachedFixedPoint {
        let mut res = ReachedFixedPoint::Yes;
        for at in 0..self.imports.directives.len() as u32 {
            if self.imports.list[at as usize] == ImportList::Unresolved && self.retry_unresolved(at)
            {
                res = ReachedFixedPoint::No;
            }
        }
        let mut seq = 0;
        while seq < self.imports.indeterminate.len() as u32 {
            let at = self.imports.indeterminate[seq as usize].0;
            if self.imports.list[at as usize] == ImportList::Indeterminate
                && self.retry_indeterminate(seq)
            {
                res = ReachedFixedPoint::No;
            }
            seq += 1;
        }
        res
    }

    /// Retries unresolved import `at`; whether it recorded anything.
    fn retry_unresolved(&mut self, at: u32) -> bool {
        let i = at as usize;
        self.imports.attempt(at);
        let status = self.resolve_import(at);
        self.imports.directives[i].status = status;
        match status {
            PartialResolvedImport::Unresolved => {
                self.wait_on_path(at);
                false
            }
            PartialResolvedImport::Indeterminate(resolved) => {
                self.imports.seq[i] = self.imports.indeterminate.len() as u32;
                self.imports.indeterminate.push((at, resolved));
                self.imports.list[i] = ImportList::Indeterminate;
                // Before recording: what it records may change what it resolves to.
                self.wait_on_path(at);
                self.record_resolved_import(at);
                true
            }
            PartialResolvedImport::Resolved(_) => {
                self.imports.list[i] = ImportList::Resolved;
                self.record_resolved_import(at);
                true
            }
        }
    }

    /// Retries the indeterminate import at `seq`; whether it recorded anything.
    fn retry_indeterminate(&mut self, seq: u32) -> bool {
        let (at, settled) = self.imports.indeterminate[seq as usize];
        let i = at as usize;
        self.imports.attempt(at);
        let status = self.resolve_import(at);
        self.imports.directives[i].status = status;
        match status {
            PartialResolvedImport::Indeterminate(import) if import != settled => {
                self.imports.indeterminate[seq as usize].1 = import;
                self.wait_on_path(at);
                self.record_resolved_import(at);
                true
            }
            PartialResolvedImport::Resolved(_) => {
                self.imports.list[i] = ImportList::Resolved;
                self.record_resolved_import(at);
                true
            }
            _ => {
                self.wait_on_path(at);
                false
            }
        }
    }

    /// Makes import `at` wait on what its attempt just read.
    fn wait_on_path(&mut self, at: u32) {
        self.imports.wait(at);
    }

    /// Resolves import `at`, leaving in `imports.keys` (and `imports.keys_any`) what the lookup
    /// read that collecting this def map can still change, for `wait_on_path`.
    fn resolve_import(&mut self, at: u32) -> PartialResolvedImport {
        #[cfg(test)]
        RESOLVE_IMPORT_CALLS.with(|it| it.set(it.get() + 1));
        let db = self.db;
        let def_map = &self.def_map;
        let Imports { directives, keys, keys_any, .. } = &mut self.imports;
        keys.clear();
        *keys_any = false;
        let directive = &directives[at as usize];
        let path = &directive.import.path;
        let last = path.segments().last().copied();
        let in_crate = def_map.block.is_none();
        let local = |m: ModuleId| m.krate(db) == def_map.krate && m.block(db) == def_map.block_id();
        // The def maps of other crates and of the blocks around this one are collected, so what
        // the lookup reads there waits on nothing. A name other than the last is read for the
        // module or type it names.
        let mut reads = |read: PathRead| match read {
            PathRead::Slot(module, name) if local(module) => {
                let ns = if Some(name) == last { ALL_NS } else { TYPES };
                keys.push((WaitOn::Slot(module, name), ns));
            }
            PathRead::LegacyMacro(module, name) if local(module) => {
                keys.push((WaitOn::Env(name), MACROS));
            }
            PathRead::ExternPrelude(name) if in_crate => keys.push((WaitOn::Env(name), TYPES)),
            PathRead::MacroUsePrelude(name) => keys.push((WaitOn::Env(name), MACROS)),
            // A new std prelude module wakes every import.
            PathRead::Prelude => {}
            PathRead::Unscoped => *keys_any = true,
            PathRead::Slot(..) | PathRead::LegacyMacro(..) | PathRead::ExternPrelude(_) => {}
        };
        let ResolvePathResult { resolved_def, segment_index, reached_fixedpoint, prefix_info } =
            def_map.resolve_path_fp_recording(
                self.crate_local_def_map.unwrap_or(&self.local_def_map),
                db,
                ResolveMode::Import,
                directive.module_id,
                path,
                BuiltinShadowMode::Module,
                None, // An import may resolve to any kind of macro.
                &mut reads,
            );

        if reached_fixedpoint == ReachedFixedPoint::No
            || resolved_def.is_none()
            || segment_index.is_some()
        {
            return PartialResolvedImport::Unresolved;
        }

        if prefix_info.differing_crate {
            return PartialResolvedImport::Resolved(
                resolved_def.filter_visibility(|v| matches!(v, Visibility::Public)),
            );
        }

        // Check whether all namespaces are resolved.
        if resolved_def.is_full() {
            PartialResolvedImport::Resolved(resolved_def)
        } else {
            PartialResolvedImport::Indeterminate(resolved_def)
        }
    }

    fn record_resolved_import(&mut self, at: u32) {
        let directive = &self.imports.directives[at as usize];
        let module_id = directive.module_id;
        let mut def = directive.status.namespaces();
        let raw_vis = directive.import.visibility;
        let source = directive.import.source;
        let alias = directive.import.alias.as_ref();
        let path_kind = directive.import.path.kind;
        let path_len = directive.import.path.segments().len();
        let last_segment = directive.import.path.segments().last().copied();
        let vis = self
            .def_map
            .resolve_visibility(
                self.crate_local_def_map.unwrap_or(&self.local_def_map),
                self.db,
                module_id,
                raw_vis,
                false,
            )
            .unwrap_or(Visibility::Public);

        match source {
            ImportSource {
                kind: kind @ (ImportKind::Plain | ImportKind::TypeOnly),
                id,
                use_tree,
                ..
            } => {
                let name = match alias {
                    Some(ImportAlias::Alias(name)) => Some(*name),
                    Some(ImportAlias::Underscore) => None,
                    None => match last_segment {
                        Some(last_segment) => Some(last_segment),
                        None => {
                            cov_mark::hit!(bogus_paths);
                            return;
                        }
                    },
                };

                if kind == ImportKind::TypeOnly {
                    def.values = None;
                    def.macros = None;
                }
                let imp = ImportOrExternCrate::Import(ImportId { use_: id, idx: use_tree });

                // `extern crate crate_name` things can be re-exported as `pub use crate_name`.
                // But they cannot be re-exported as `pub use self::crate_name`, `pub use crate::crate_name`
                // or `pub use ::crate_name`.
                //
                // This has been historically allowed, but may be not allowed in future
                // https://github.com/rust-lang/rust/issues/127909
                if let Some(def) = def.types.as_mut() {
                    let is_extern_crate_reimport_without_prefix = || {
                        let Some(ImportOrExternCrate::ExternCrate(_)) = def.import else {
                            return false;
                        };
                        if kind == ImportKind::Glob {
                            return false;
                        }
                        matches!(path_kind, PathKind::Plain | PathKind::SELF) && path_len < 2
                    };
                    if is_extern_crate_reimport_without_prefix() {
                        def.vis = vis;
                    }
                }

                if name.is_none() {
                    self.def_map.modules.building()[module_id].scope_mut().push_underscore_import(
                        UnderscoreImport {
                            import: ImportId { use_: id, idx: use_tree },
                            def: def.types.map(|it| it.def),
                            vis,
                        },
                    );
                }
                self.update(module_id, &[(name, def)], vis, Some(imp));
            }
            ImportSource { kind: ImportKind::Glob, id, is_prelude, use_tree, .. } => {
                let glob = GlobId { use_: id, idx: use_tree };
                match def.take_types() {
                    Some(ModuleDefId::ModuleId(m)) => {
                        if is_prelude {
                            // Note: This dodgily overrides the injected prelude. The rustc
                            // implementation seems to work the same though.
                            cov_mark::hit!(std_prelude);
                            self.def_map.prelude = Some((m, Some(id)));
                            self.imports.wake_all();
                        } else if m.krate(self.db) != self.def_map.krate {
                            cov_mark::hit!(glob_across_crates);
                            // glob import from other crate => we can just import everything once
                            let scope = &m.def_map(self.db).module(self.db, m).scope(self.db);

                            // Module scoped macros is included
                            let start = self.held.len();
                            self.held.extend(
                                scope
                                    .resolutions()
                                    // only keep visible names...
                                    .map(|(n, res)| {
                                        (
                                            n,
                                            res.filter_visibility(|v| {
                                                v.is_visible_from_other_crate()
                                            }),
                                        )
                                    })
                                    .filter(|(_, res)| !res.is_none()),
                            );
                            self.update_held(
                                module_id,
                                start,
                                vis,
                                Some(ImportOrExternCrate::Glob(glob)),
                            );
                        } else {
                            // glob import from same crate => we do an initial
                            // import, and then need to propagate any further
                            // additions
                            let scope = if m.block(self.db) == self.def_map.block_id() {
                                self.def_map.module(self.db, m).scope(self.db)
                            } else {
                                m.def_map(self.db).module(self.db, m).scope(self.db)
                            };

                            // Module scoped macros is included
                            let start = self.held.len();
                            self.held.extend(
                                scope
                                    .resolutions()
                                    // only keep visible names...
                                    .map(|(n, res)| {
                                        (
                                            n,
                                            res.filter_visibility(|v| {
                                                v.is_visible_from_def_map(
                                                    self.db,
                                                    &self.def_map,
                                                    module_id,
                                                )
                                            }),
                                        )
                                    })
                                    .filter(|(_, res)| !res.is_none()),
                            );
                            self.update_held(
                                module_id,
                                start,
                                vis,
                                Some(ImportOrExternCrate::Glob(glob)),
                            );
                            // record the glob import in case we add further items
                            let (db, def_map) = (self.db, &self.def_map);
                            // Two globs of one module share an edge. The one its names are bound
                            // through (`glob_takes_over`) takes it over, so what the source gains
                            // later is credited as when it was pulled.
                            self.globs.add(m, module_id, (vis, glob), |(old_vis, old_glob)| {
                                if glob_takes_over(db, def_map, glob, vis, Some(*old_glob), *old_vis) {
                                    *old_glob = glob;
                                }
                                if let Some(new_vis) = old_vis.max(db, vis, def_map) {
                                    *old_vis = new_vis;
                                }
                            });
                        }
                    }
                    Some(ModuleDefId::AdtId(AdtId::EnumId(e))) => {
                        cov_mark::hit!(glob_enum);
                        // glob import from enum => just import all the variants
                        let start = self.held.len();
                        self.held.extend(e.enum_variants(self.db).variants.iter().map(
                            |(name, &(variant, shape))| {
                                // We don't need to reduce the visibility, since enum variants and their fields do not have visibility on their own.
                                let res = if shape.has_value_ns_ctor() {
                                    PerNs::both(variant.into(), variant.into(), vis, None)
                                } else {
                                    PerNs::types(variant.into(), vis, None)
                                };
                                (Some(*name), res)
                            },
                        ));
                        self.update_held(
                            module_id,
                            start,
                            vis,
                            Some(ImportOrExternCrate::Glob(glob)),
                        );
                    }
                    Some(ModuleDefId::TraitId(it)) => {
                        // glob import from trait => its associated items, as rustc's traits are
                        // modules to `use`. None where it cannot list them, which the end of
                        // collection reports.
                        let start = self.held.len();
                        let held = &mut self.held;
                        let listed = self.def_map.for_each_trait_item_for_import(
                            self.db,
                            it,
                            false,
                            |name, item| held.push((Some(name), trait_item_per_ns(item, vis))),
                        );
                        if listed.is_err() {
                            self.held.truncate(start);
                        }
                        self.update_held(
                            module_id,
                            start,
                            vis,
                            Some(ImportOrExternCrate::Glob(glob)),
                        );
                    }
                    Some(d) => {
                        tracing::debug!("glob import from non-module/enum {:?}", d);
                    }
                    None => {
                        tracing::debug!("glob import didn't resolve as type");
                    }
                }
            }
        }
    }

    fn update(
        &mut self,
        // The module for which `resolutions` have been resolve
        module_id: ModuleId,
        resolutions: &[(Option<Name>, PerNs)],
        // Visibility this import will have
        vis: Visibility,
        import: Option<ImportOrExternCrate>,
    ) {
        let start = self.held.len();
        self.held.extend_from_slice(resolutions);
        self.update_held(module_id, start, vis, import);
    }

    /// `update` with the resolutions at `start..` of `held`, which it takes off.
    fn update_held(
        &mut self,
        module_id: ModuleId,
        start: usize,
        vis: Visibility,
        import: Option<ImportOrExternCrate>,
    ) {
        let end = self.held.len();
        self.update_recursive(module_id, start..end, vis, import, 0);
        self.held.truncate(start);
    }

    fn update_recursive(
        &mut self,
        // The module for which `resolutions` have been resolved.
        module_id: ModuleId,
        // Where in `held` they are.
        resolutions: Range<usize>,
        // All resolutions are imported with this visibility; the visibilities in
        // the `PerNs` values are ignored and overwritten
        vis: Visibility,
        import: Option<ImportOrExternCrate>,
        depth: usize,
    ) {
        if depth > GLOB_RECURSION_LIMIT {
            // prevent stack overflows (but this shouldn't be possible)
            panic!("infinite recursion in glob imports!");
        }
        let mut changed = false;

        for at in resolutions.start..resolutions.end {
            let (name, res) = self.held[at];
            match name {
                Some(name) => {
                    changed |=
                        self.push_res_and_update_glob_vis(module_id, &name, res, vis, import);
                }
                None => {
                    let tr = match res.take_types_full() {
                        Some(Item { def: ModuleDefId::TraitId(tr), vis: _, import: _ }) => tr,
                        Some(other) => {
                            tracing::debug!("non-trait `_` import of {:?}", other);
                            continue;
                        }
                        None => continue,
                    };
                    let old_vis = self.def_map.modules.building()[module_id]
                        .scope(self.db)
                        .unnamed_trait_vis(tr);
                    let should_update = match old_vis {
                        None => true,
                        Some(old_vis) => {
                            let max_vis = old_vis.max(self.db, vis, &self.def_map).unwrap_or_else(|| {
                                panic!("`Tr as _` imports with unrelated visibilities {old_vis:?} and {vis:?} (trait {tr:?})");
                            });

                            if max_vis == old_vis {
                                false
                            } else {
                                cov_mark::hit!(upgrade_underscore_visibility);
                                true
                            }
                        }
                    };

                    if should_update {
                        changed = true;
                        self.def_map.modules.building()[module_id]
                            .scope_mut()
                            .push_unnamed_trait_through(tr, vis, import);
                    }
                }
            }
        }

        if !changed {
            return;
        }
        let first_edge = self.globs.first(module_id);
        if first_edge == NONE {
            return;
        }
        // A name this module already had in one namespace kept its own item there, so only
        // what the module now holds reaches the modules that glob-import it. Those go after
        // this level's, and are taken off when its importers are done.
        let held = self.held.len();
        let scope = self.def_map.module(self.db, module_id).scope(self.db);
        for at in resolutions {
            let (name, res) = self.held[at];
            let res = match name {
                None => res,
                Some(name) => {
                    let now = scope.get(&name);
                    PerNs {
                        types: res
                            .types
                            .filter(|it| now.types.is_some_and(|now| now.def == it.def)),
                        values: res
                            .values
                            .filter(|it| now.values.is_some_and(|now| now.def == it.def)),
                        macros: res
                            .macros
                            .filter(|it| now.macros.is_some_and(|now| now.def == it.def)),
                    }
                }
            };
            // A glob carries no unresolved name, as when it is first pulled: else an importer
            // would hold one only when it shares a run with the import that failed.
            if res.is_none() {
                continue;
            }
            self.held.push((name, res));
        }
        let end = self.held.len();

        // Propagating adds no glob edge, so the list read as it goes is the list as it was.
        let mut edge = first_edge;
        while edge != NONE {
            let ImporterEdge { importer, data: (glob_import_vis, glob), next } =
                self.globs.edge(edge);
            edge = next;
            if !vis.is_visible_from_def_map(self.db, &self.def_map, importer) {
                continue;
            }
            let vis = glob_import_vis.min(self.db, vis, &self.def_map).unwrap_or(glob_import_vis);
            // Each name is held at the narrower of the import's visibility and its own, so the
            // importer takes only the names whose own visibility it sees, as a first glob does.
            let from = self.held.len();
            for at in held..end {
                let (name, res) = self.held[at];
                let res = res.filter_visibility(|it| it.is_visible_from_def_map(self.db, &self.def_map, importer));
                if !res.is_none() {
                    self.held.push((name, res));
                }
            }
            let visible = from..self.held.len();
            self.update_recursive(importer, visible, vis, Some(ImportOrExternCrate::Glob(glob)), depth + 1);
            self.held.truncate(from);
        }
        self.held.truncate(held);
    }

    /// Records that `glob` gives `name` of `module_id` the item `other` where another glob
    /// gave it a different one: rustc's ambiguity binding, an error where a path uses it.
    fn push_glob_ambiguity(
        &mut self,
        module_id: ModuleId,
        name: &Name,
        other: ItemInNs,
        glob: GlobId,
        vis: Visibility,
    ) {
        let earlier = recorded_ambiguity(self.def_map.modules.building()[module_id].scope(self.db), name, other, glob);
        let vis = widest_ambiguity(self.db, &self.def_map, earlier, vis);
        let it = GlobAmbiguity { name: *name, other, glob, vis };
        self.def_map.modules.building()[module_id].scope_mut().push_glob_ambiguity(it);
    }

    fn push_res_and_update_glob_vis(
        &mut self,
        module_id: ModuleId,
        name: &Name,
        mut defs: PerNs,
        vis: Visibility,
        def_import_type: Option<ImportOrExternCrate>,
    ) -> bool {
        let glob = matches!(def_import_type, Some(ImportOrExternCrate::Glob(_)));
        let (db, def_map) = (self.db, &self.def_map);
        let narrowed = |it: Visibility| {
            let it = it.min(db, vis, def_map).unwrap_or(vis);
            if glob { it.root_as_pub_crate(db, def_map) } else { it }
        };
        if let Some(def) = defs.types.as_mut() {
            def.vis = narrowed(def.vis);
        }
        if let Some(def) = defs.values.as_mut() {
            def.vis = narrowed(def.vis);
        }
        if let Some(def) = defs.macros.as_mut() {
            def.vis = narrowed(def.vis);
        }
        // Whatever the scope does with them: an import that read the slot is retried, and
        // finds the same answer if nothing it reads changed.
        self.imports.wake_slot(module_id, *name, written_ns(&defs));

        // A name is unresolved only where nothing resolved it, never where every namespace was
        // handled below.
        let unresolved = defs.is_none();
        let mut changed = false;

        if let Some(ImportOrExternCrate::Glob(glob)) = def_import_type {
            let prev_defs = self.def_map.modules.building()[module_id].scope(self.db).get(name);

            // Multiple globs may import the same item and they may override visibility from
            // previously resolved globs. Handle overrides here and leave the rest to
            // `ItemScope::push_res_with_import()`.
            if let Some(def) = defs.types
                && let Some(prev_def) = prev_defs.types
            {
                if def.def == prev_def.def
                    && self.from_glob_import.contains_type(module_id, *name)
                    && glob_takes_over(
                        self.db,
                        &self.def_map,
                        glob,
                        def.vis,
                        prev_def.import.and_then(ImportOrExternCrate::glob),
                        prev_def.vis,
                    )
                {
                    changed = true;
                    // This import is being handled here, don't pass it down to
                    // `ItemScope::push_res_with_import()`.
                    defs.types = None;
                    self.def_map.modules.building()[module_id]
                        .scope_mut()
                        .rebind_glob_types(name, def, glob);
                }
                // When the source module's definition changed (e.g., due to an explicit import
                // shadowing a glob), propagate the new definition to modules that glob-import from it.
                // We check that the previous definition came from the same glob import to avoid
                // incorrectly overwriting definitions from different glob sources.
                //
                // Note this is not a perfect fix, but it makes
                // https://github.com/rust-lang/rust-analyzer/issues/19224 work for now until we
                // implement a proper glob graph
                else if def.def != prev_def.def && prev_def.import == def_import_type {
                    changed = true;
                    defs.types = None;
                    self.def_map.modules.building()[module_id]
                        .scope_mut()
                        .update_def_types(name, def.def, def.vis);
                } else if def.def != prev_def.def
                    && let Some(ImportOrExternCrate::Glob(_)) = prev_def.import
                {
                    let other = ItemInNs::Types(def.def);
                    self.push_glob_ambiguity(module_id, name, other, glob, def.vis);
                }
            }

            if let Some(def) = defs.values
                && let Some(prev_def) = prev_defs.values
            {
                if def.def == prev_def.def
                    && self.from_glob_import.contains_value(module_id, *name)
                    && glob_takes_over(
                        self.db,
                        &self.def_map,
                        glob,
                        def.vis,
                        prev_def.import.map(ImportOrExternCrate::from).and_then(ImportOrExternCrate::glob),
                        prev_def.vis,
                    )
                {
                    changed = true;
                    defs.values = None;
                    self.def_map.modules.building()[module_id]
                        .scope_mut()
                        .rebind_glob_values(name, def, glob);
                } else if def.def != prev_def.def
                    && prev_def.import.map(ImportOrExternCrate::from) == def_import_type
                {
                    changed = true;
                    defs.values = None;
                    self.def_map.modules.building()[module_id]
                        .scope_mut()
                        .update_def_values(name, def.def, def.vis);
                } else if def.def != prev_def.def
                    && let Some(ImportOrGlob::Glob(_)) = prev_def.import
                {
                    let other = ItemInNs::Values(def.def);
                    self.push_glob_ambiguity(module_id, name, other, glob, def.vis);
                }
            }

            if let Some(def) = defs.macros
                && let Some(prev_def) = prev_defs.macros
            {
                if def.def == prev_def.def
                    && self.from_glob_import.contains_macro(module_id, *name)
                    && glob_takes_over(
                        self.db,
                        &self.def_map,
                        glob,
                        def.vis,
                        prev_def.import.and_then(ImportOrExternCrate::glob),
                        prev_def.vis,
                    )
                {
                    changed = true;
                    defs.macros = None;
                    self.def_map.modules.building()[module_id]
                        .scope_mut()
                        .rebind_glob_macros(name, def, glob);
                } else if def.def != prev_def.def && prev_def.import == def_import_type {
                    changed = true;
                    defs.macros = None;
                    self.def_map.modules.building()[module_id]
                        .scope_mut()
                        .update_def_macros(name, def.def, def.vis);
                } else if def.def != prev_def.def
                    && let Some(ImportOrExternCrate::Glob(_)) = prev_def.import
                {
                    let other = ItemInNs::Macros(def.def);
                    self.push_glob_ambiguity(module_id, name, other, glob, def.vis);
                }
            }
        }

        if unresolved || !defs.is_none() {
            changed |= self.def_map.modules.building()[module_id].scope_mut().push_res_with_import(
                &mut self.from_glob_import,
                (module_id, *name),
                defs,
                def_import_type,
            );
        }

        changed
    }

    fn resolve_macros(&mut self) -> ReachedFixedPoint {
        // The round's directives; those its resolutions add collect in the spare list.
        let mut macros =
            mem::replace(&mut self.unresolved_macros, mem::take(&mut self.macros_spare));
        let mut resolved = mem::take(&mut self.round_resolved);
        let push_resolved = |resolved: &mut Vec<ResolvedCall>,
                             directive: &MacroDirective<'_>,
                             call_id| {
            let attr_macro_item = match &directive.kind {
                MacroDirectiveKind::Attr { ast_id, .. } => Some(ast_id.ast_id),
                MacroDirectiveKind::FnLike { .. } | MacroDirectiveKind::Derive { .. } => None,
            };
            resolved.push((
                directive.module_id,
                directive.depth,
                directive.container,
                call_id,
                attr_macro_item,
            ));
        };

        #[derive(PartialEq, Eq)]
        enum Resolved {
            Yes,
            No,
        }

        // alloc: none unless an eager call's input holds macro calls; one per round
        let mut eager_callback_buffer = vec![];
        // alloc: none unless an eager call's input holds macro calls; reused per call
        let mut eager_calls = Vec::new();
        let mut res = ReachedFixedPoint::Yes;
        let expand_eager_with_unresolved_args = self.expand_eager_with_unresolved_args;
        let deferred_eager_call = std::cell::Cell::new(false);
        // Retain unresolved macros after this round of resolution.
        let mut retain = |directive: &MacroDirective<'db>| {
            let subns = match &directive.kind {
                MacroDirectiveKind::FnLike { .. } => MacroSubNs::Bang,
                MacroDirectiveKind::Attr { .. } | MacroDirectiveKind::Derive { .. } => {
                    MacroSubNs::Attr
                }
            };
            let resolver = |def_map: &DefMap, path: &_| {
                let resolved_res = def_map.resolve_path_fp_with_macro(
                    self.crate_local_def_map.unwrap_or(&self.local_def_map),
                    self.db,
                    ResolveMode::Other,
                    directive.module_id,
                    path,
                    BuiltinShadowMode::Module,
                    Some(subns),
                );
                resolved_res.resolved_def.take_macros().map(|it| (it, it.definition(self.db)))
            };
            let resolver_def_id = |path: &ModPath| {
                resolver(&self.def_map, path).map(|(_, it)| it).or_else(|| {
                    // A `macro_rules!` only the textual scope has, named where a call that
                    // resolves late (an eager one, as `include!` from the prelude) names it:
                    // rquickjs-sys's `include!(concat!("bindings/", bindings_env!("TARGET"),
                    // ".rs"))`, after an `include!` from `OUT_DIR` that defines `bindings_env!`.
                    let name = path.as_ident()?;
                    let def_map = &self.def_map;
                    def_map
                        .legacy_macro_in_textual_scope(self.db, directive.module_id, name)
                        .filter(|&id| sub_namespace_match(self.db, id, Some(subns)))
                        .map(|it| it.definition(self.db))
                })
            };

            match &directive.kind {
                MacroDirectiveKind::FnLike { ast_id, expand_to, ctxt: call_site } => {
                    let unresolved = std::cell::Cell::new(false);
                    // alloc: the paths an eager call's arguments name, kept once it resolves.
                    let named = std::cell::RefCell::new(Vec::new());
                    let buffered = eager_callback_buffer.len();
                    let call_id = macro_call_as_call_id(
                        self.db,
                        ast_id.ast_id,
                        &ast_id.path,
                        *call_site,
                        *expand_to,
                        self.def_map.krate,
                        directive.depth,
                        self.def_map.recursion_limit(),
                        |path| {
                            let def = resolver_def_id(path);
                            unresolved.set(unresolved.get() || def.is_none());
                            if let Some(def) = def
                                && path != &ast_id.path
                            {
                                // clone: kept until the def map's records are made.
                                named.borrow_mut().push((path.clone(), def));
                            }
                            def
                        },
                        &mut eager_calls,
                    );
                    let module_id = directive.module_id;
                    eager_callback_buffer.extend(
                        eager_calls.drain(..).map(|(ptr, call_id)| (module_id, ptr, call_id)),
                    );
                    if call_id.is_ok() && unresolved.get() && !expand_eager_with_unresolved_args {
                        // A macro its arguments name may be defined by one not expanded yet.
                        eager_callback_buffer.truncate(buffered);
                        deferred_eager_call.set(true);
                        return Resolved::No;
                    }
                    if let Ok(call_id) = call_id {
                        // FIXME: Expansion error
                        if let Some(call_id) = call_id.value {
                            self.def_map.modules.building()[directive.module_id]
                                .scope_mut()
                                .add_macro_invoc(ast_id.ast_id, call_id);
                            let def = call_id.loc(self.db).def;
                            self.resolved_macro_paths.push(ResolvedMacroPath {
                                module: directive.module_id,
                                // clone: kept until the def map's records are made.
                                path: ast_id.path.clone(),
                                subns: MacroSubNs::Bang,
                                def,
                                call: Some(call_id),
                            });
                            let module = directive.module_id;
                            self.resolved_macro_paths.extend(named.into_inner().into_iter().map(
                                |(path, def)| ResolvedMacroPath {
                                    module,
                                    path,
                                    subns: MacroSubNs::Bang,
                                    def,
                                    call: None,
                                },
                            ));

                            push_resolved(&mut resolved, directive, call_id);

                            res = ReachedFixedPoint::No;
                            return Resolved::Yes;
                        }
                    }
                }
                MacroDirectiveKind::Derive {
                    ast_id,
                    derive_attr,
                    derive_pos,
                    ctxt: call_site,
                    derive_macro_id,
                } => {
                    // FIXME: This code is almost duplicate below.
                    let id = derive_macro_as_call_id(
                        self.db,
                        ast_id,
                        *derive_attr,
                        *derive_pos as u32,
                        *call_site,
                        self.def_map.krate,
                        |path| resolver(&self.def_map, path),
                        *derive_macro_id,
                        directive.depth,
                    );

                    if let Ok((macro_id, def_id, call_id)) = id {
                        self.resolved_macro_paths.push(ResolvedMacroPath {
                            module: directive.module_id,
                            // clone: kept until the def map's records are made.
                            path: ast_id.path.clone(),
                            subns: MacroSubNs::Attr,
                            def: def_id,
                            call: Some(call_id),
                        });
                        self.def_map.modules.building()[directive.module_id]
                            .scope_mut()
                            .set_derive_macro_invoc(
                                ast_id.ast_id,
                                Either::Left(call_id),
                                *derive_attr,
                                *derive_pos,
                            );
                        // Record its helper attributes.
                        if def_id.krate != self.def_map.krate {
                            let def_map = crate_def_map(self.db, def_id.krate);
                            if let Some(helpers) = def_map.data.exported_derives.get(&macro_id) {
                                self.def_map
                                    .derive_helpers_in_scope
                                    .entry(ast_id.ast_id.map(|it| it.upcast()))
                                    .or_default()
                                    .extend(izip!(
                                        helpers.iter().cloned(),
                                        iter::repeat(macro_id),
                                        iter::repeat(Either::Left(call_id)),
                                    ));
                            }
                        }

                        push_resolved(&mut resolved, directive, call_id);

                        res = ReachedFixedPoint::No;
                        return Resolved::Yes;
                    }
                }
                MacroDirectiveKind::Attr {
                    ast_id: file_ast_id,
                    mod_item,
                    attr_id,
                    attr,
                    tree,
                    item_tree,
                } => {
                    let &AstIdWithPath { ast_id, ref path } = file_ast_id;
                    let file_id = ast_id.file_id;

                    let mut recollect_without = |collector: &mut Self| {
                        // Remove the original directive since we resolved it.
                        let mod_dir = collector.mod_dirs[&directive.module_id];
                        collector.skip_attrs.insert(InFile::new(file_id, *mod_item), *attr_id);

                        ModCollector {
                            def_collector: collector,
                            macro_depth: directive.depth,
                            module_id: directive.module_id,
                            tree_id: *tree,
                            item_tree,
                            mod_dir,
                        }
                        .collect(&[*mod_item], directive.container);
                        res = ReachedFixedPoint::No;
                        Resolved::Yes
                    };

                    if let Some(ident) = path.as_ident()
                        && let Some(helpers) = self.def_map.derive_helpers_in_scope.get(&ast_id)
                        && let Some(&(_, derive, _)) = helpers.iter().find(|(it, ..)| it == ident)
                    {
                        cov_mark::hit!(resolved_derive_helper);
                        self.derive_helper_uses.push(DeriveHelperUse {
                            item: ast_id,
                            attr: *attr_id,
                            derive,
                            before_derive: false,
                        });
                        // Resolved to derive helper. Collect the item's attributes again,
                        // starting after the derive helper.
                        return recollect_without(self);
                    }

                    // rustc's `DeriveHelpersCompat`: a helper of a derive the item's later
                    // `#[derive]`s name, which rustc accepts with the `legacy_derive_helpers`
                    // lint.
                    if let Some(ident) = path.as_ident()
                        && let Some(AttrsOrCfgRef::Enabled { attrs }) = item_tree.attrs(*mod_item)
                    {
                        let helper_of = |derive_path: &ModPath| {
                            let (macro_id, def) = resolver(&self.def_map, derive_path)?;
                            if def.krate == self.def_map.krate {
                                return None;
                            }
                            let exported = &crate_def_map(self.db, def.krate).data.exported_derives;
                            exported
                                .get(&macro_id)
                                .is_some_and(|helpers| helpers.contains(ident))
                                .then_some(macro_id)
                        };
                        let derive = attrs
                            .iter_after(Some(*attr_id))
                            .filter(|(_, later)| {
                                later.path.as_ident().is_some_and(|it| *it == sym::derive)
                            })
                            .filter_map(|(_, later)| later.parse_path_comma_token_tree(self.db))
                            .flatten()
                            .find_map(|(derive_path, ..)| helper_of(&derive_path));
                        if let Some(derive) = derive {
                            self.derive_helper_uses.push(DeriveHelperUse {
                                item: ast_id,
                                attr: *attr_id,
                                derive,
                                before_derive: true,
                            });
                            return recollect_without(self);
                        }
                    }

                    let def = match resolver_def_id(path) {
                        Some(def) if def.is_attribute() => def,
                        _ => return Resolved::No,
                    };

                    // Skip #[test]/#[bench]/#[test_case] expansion, which would merely result in more memory usage
                    // due to duplicating functions into macro expansions, but only if `cfg(test)` is active,
                    // otherwise they are expanded to nothing and this can impact e.g. diagnostics (due to things
                    // being cfg'ed out).
                    // Ideally we will just expand them to nothing here. But we are only collecting macro calls,
                    // not expanding them, so we have no way to do that.
                    if matches!(
                        def.kind,
                        MacroDefKind::BuiltInAttr(_, expander)
                        if expander.is_test() || expander.is_bench() || expander.is_test_case()
                    ) {
                        let test_is_active = self.cfg_options.check_atom(&CfgAtom::Flag(sym::test));
                        if test_is_active {
                            self.resolved_macro_paths.push(ResolvedMacroPath {
                                module: directive.module_id,
                                // clone: kept until the def map's records are made.
                                path: path.clone(),
                                subns: MacroSubNs::Attr,
                                def,
                                call: None,
                            });
                            return recollect_without(self);
                        }
                    }

                    let mut call_id = || {
                        let active_attrs = self.prev_active_attrs.entry(ast_id).or_default();
                        active_attrs.push(*attr_id);

                        attr_macro_as_call_id(
                            self.db,
                            file_ast_id,
                            attr,
                            AttrMacroAttrIds::from_many(active_attrs),
                            self.def_map.krate,
                            def,
                            directive.depth,
                        )
                    };
                    if matches!(def,
                        MacroDefId { kind: MacroDefKind::BuiltInAttr(_, exp), .. }
                        if exp.is_derive()
                    ) {
                        // Resolved to `#[derive]`, we don't actually expand this attribute like
                        // normal (as that would just be an identity expansion with extra output)
                        // Instead we treat derive attributes special and apply them separately.

                        let ast_adt_id: FileAstId<ast::Adt> = match item_tree.index(*mod_item) {
                            ModItemKind::Struct(ast_id, _) => ast_id.upcast(),
                            ModItemKind::Union(ast_id, _) => ast_id.upcast(),
                            ModItemKind::Enum(ast_id, _) => ast_id.upcast(),
                            _ => {
                                let diag = DefDiagnostic::invalid_derive_target(
                                    directive.module_id,
                                    ast_id,
                                    *attr_id,
                                );
                                self.def_map.diagnostics.push(diag);
                                return recollect_without(self);
                            }
                        };

                        let ast_id = ast_id.with_value(ast_adt_id);

                        let mut derive_call_ids = SmallVec::new();
                        match attr.parse_path_comma_token_tree(self.db) {
                            Some(derive_macros) => {
                                let call_id = call_id();
                                self.resolved_macro_paths.push(ResolvedMacroPath {
                                    module: directive.module_id,
                                    // clone: kept until the def map's records are made.
                                    path: path.clone(),
                                    subns: MacroSubNs::Attr,
                                    def,
                                    call: Some(call_id),
                                });
                                for (idx, (path, call_site, _)) in derive_macros.enumerate() {
                                    let ast_id =
                                        AstIdWithPath::new(file_id, ast_id.value, path);

                                    derive_call_ids.push(None);

                                    // Try to resolve the derive immediately. If we succeed, we can also use the fast path
                                    // for builtin derives. If not, we cannot use it, as it can cause the ADT to become
                                    // interned while the derive is still unresolved, which will cause it to get forgotten.
                                    let id = derive_macro_as_call_id(
                                        self.db,
                                        &ast_id,
                                        *attr_id,
                                        idx as u32,
                                        call_site.ctx,
                                        self.def_map.krate,
                                        |path| resolver(&self.def_map, path),
                                        call_id,
                                        directive.depth,
                                    );

                                    let ast_id_without_path = ast_id.ast_id;
                                    let directive = MacroDirective {
                                        module_id: directive.module_id,
                                        depth: directive.depth + 1,
                                        kind: MacroDirectiveKind::Derive {
                                            ast_id,
                                            derive_attr: *attr_id,
                                            derive_pos: idx,
                                            ctxt: call_site.ctx,
                                            derive_macro_id: call_id,
                                        },
                                        container: directive.container,
                                    };

                                    if let Ok((macro_id, def_id, call_id)) = id {
                                        if let MacroDirectiveKind::Derive { ast_id, .. } =
                                            &directive.kind
                                        {
                                            self.resolved_macro_paths.push(ResolvedMacroPath {
                                                module: directive.module_id,
                                                // Kept until the def map's records are made.
                                                // clone: one per resolved derive.
                                                path: ast_id.path.clone(),
                                                subns: MacroSubNs::Attr,
                                                def: def_id,
                                                call: Some(call_id),
                                            });
                                        }
                                        let (mut helpers_start, mut helpers_end) = (0, 0);
                                        // Record its helper attributes.
                                        if def_id.krate != self.def_map.krate {
                                            let def_map = crate_def_map(self.db, def_id.krate);
                                            if let Some(helpers) =
                                                def_map.data.exported_derives.get(&macro_id)
                                            {
                                                let derive_helpers = self
                                                    .def_map
                                                    .derive_helpers_in_scope
                                                    .entry(
                                                        ast_id_without_path.map(|it| it.upcast()),
                                                    )
                                                    .or_default();
                                                helpers_start = derive_helpers.len();
                                                derive_helpers.extend(izip!(
                                                    helpers.iter().cloned(),
                                                    iter::repeat(macro_id),
                                                    iter::repeat(Either::Left(call_id)),
                                                ));
                                                helpers_end = derive_helpers.len();
                                            }
                                        }

                                        if super::enable_builtin_derive_fast_path(self.db)
                                            && let MacroDefKind::BuiltInDerive(_, builtin_derive) =
                                                def_id.kind
                                            && crate::builtin_derive::has_builtin_derive_impl(
                                                builtin_derive,
                                            )
                                        {
                                            self.deferred_builtin_derives
                                                .entry(ast_id_without_path.upcast())
                                                .or_default()
                                                .push(DeferredBuiltinDerive {
                                                    call_id,
                                                    derive: builtin_derive,
                                                    module_id: directive.module_id,
                                                    container: directive.container,
                                                    depth: directive.depth,
                                                    derive_attr_id: *attr_id,
                                                    derive_index: idx as u32,
                                                    helpers_range: helpers_start..helpers_end,
                                                });
                                        } else {
                                            push_resolved(&mut resolved, &directive, call_id);
                                            *derive_call_ids.last_mut().unwrap() =
                                                Some(Either::Left(call_id));
                                        }
                                    } else {
                                        self.unresolved_macros.push(directive);
                                    }
                                }

                                // We treat the #[derive] macro as an attribute call, but we do not resolve it for nameres collection.
                                // This is just a trick to be able to resolve the input to derives
                                // as proper paths in `Semantics`.
                                // Check the comment in [`builtin_attr_macro`].
                                self.def_map.modules.building()[directive.module_id]
                                    .scope_mut()
                                    .init_derive_attribute(
                                        ast_id,
                                        *attr_id,
                                        call_id,
                                        derive_call_ids,
                                    );
                            }
                            None => {
                                let diag = DefDiagnostic::malformed_derive(
                                    directive.module_id,
                                    ast_id,
                                    *attr_id,
                                );
                                self.def_map.diagnostics.push(diag);
                            }
                        }

                        return recollect_without(self);
                    }

                    if let MacroDefKind::ProcMacro(_, exp, _) = def.kind {
                        // If there's no expander for the proc macro (e.g.
                        // because proc macros are disabled, or building the
                        // proc macro crate failed), report this and skip
                        // expansion like we would if it was disabled
                        if let Some(err) = exp.as_expand_error(self.db, def.krate) {
                            self.def_map.diagnostics.push(DefDiagnostic::macro_error(
                                directive.module_id,
                                ast_id,
                                ModPath::clone(path),
                                err,
                            ));
                            return recollect_without(self);
                        }
                    }

                    // Clear deferred derives for this item, unfortunately we cannot use them due to the attribute.
                    if let Some(deferred_derives) = self.deferred_builtin_derives.remove(&ast_id) {
                        resolved.extend(deferred_derives.into_iter().map(|derive| {
                            (
                                derive.module_id,
                                derive.depth,
                                derive.container,
                                derive.call_id,
                                Some(ast_id),
                            )
                        }));
                    }

                    let call_id = call_id();
                    self.resolved_macro_paths.push(ResolvedMacroPath {
                        module: directive.module_id,
                        // clone: kept until the def map's records are made.
                        path: path.clone(),
                        subns: MacroSubNs::Attr,
                        def,
                        call: Some(call_id),
                    });
                    self.def_map.modules.building()[directive.module_id]
                        .scope_mut()
                        .add_attr_macro_invoc(ast_id, call_id);

                    push_resolved(&mut resolved, directive, call_id);
                    res = ReachedFixedPoint::No;
                    return Resolved::Yes;
                }
            }

            Resolved::No
        };
        macros.retain(|it| retain(it) == Resolved::No);
        self.deferred_eager_call = deferred_eager_call.get();
        // Attribute resolution can add unresolved macro invocations, so concatenate the lists.
        macros.append(&mut self.unresolved_macros);
        self.macros_spare = mem::replace(&mut self.unresolved_macros, macros);

        for (module_id, ptr, call_id) in eager_callback_buffer {
            self.def_map.modules.building()[module_id]
                .scope_mut()
                .add_macro_invoc(ptr.map(|(_, it)| it), call_id);
        }

        self.expand_round(&resolved);
        for (module_id, depth, container, macro_call_id, attr_macro_item) in resolved.drain(..) {
            self.collect_macro_expansion(
                module_id,
                macro_call_id,
                depth,
                container,
                attr_macro_item,
            );
        }
        self.round_resolved = resolved;

        res
    }

    /// Expands the macros a round resolved as one batch of plane tasks, before
    /// `collect_macro_expansion` collects them one at a time in the round's order. An expansion
    /// is a query of its call alone, so which task runs it when changes no result, and
    /// collecting in the round's order keeps the def map the same whatever the scheduling.
    ///
    /// A proc macro that keeps a session sees its calls in the order rustc makes them, so its
    /// calls are left to the collection, in order.
    fn expand_round(&mut self, resolved: &[ResolvedCall]) {
        let db = self.db;
        let limit = self.def_map.recursion_limit();
        let in_session = |call: MacroCallId| {
            let def = call.loc(db).def;
            matches!(
                def.kind,
                MacroDefKind::ProcMacro(_, exp, _) if exp.requires_session(db, def.krate)
            )
        };
        let mut calls = mem::take(&mut self.round_calls);
        calls.extend(
            resolved
                .iter()
                .filter(|&&(_, depth, ..)| depth <= limit)
                .map(|&(_, _, _, call, _)| call)
                .filter(|&call| !in_session(call)),
        );
        expand_all(db, self.def_map.krate, &calls);
        calls.clear();
        self.round_calls = calls;
    }

    fn collect_macro_expansion(
        &mut self,
        module_id: ModuleId,
        macro_call_id: MacroCallId,
        depth: u32,
        container: ItemContainerId,
        attr_macro_item: Option<AstId<ast::Item>>,
    ) {
        if depth > self.def_map.recursion_limit() {
            cov_mark::hit!(macro_expansion_overflow);
            let diag = DefDiagnostic::recursion_overflow(self.db, module_id, macro_call_id);
            self.def_map.diagnostics.push(diag);
            return;
        }
        let def = macro_call_id.loc(self.db).def;
        if let MacroDefKind::ProcMacro(_, exp, _) = def.kind
            && exp.requires_session(self.db, def.krate)
        {
            // Equal expansion output still has state changes to replay next revision.
            blaze_query::db::report_untracked_read(self.db);
            self.def_map.session_macros = true;
        }
        let file_id = macro_call_id.into();

        let item_tree = file_item_tree(self.db, file_id, self.def_map.krate);

        // Derive helpers that are in scope for an item are also in scope for attribute macro expansions
        // of that item (but not derive or fn like macros).
        // FIXME: This is a hack. The proper way to do this is by having a chain of derive helpers scope,
        // where the next scope in the chain is the parent hygiene context of the span. Unfortunately
        // it's difficult to implement with our current name resolution and hygiene system.
        // This hack is also incorrect since it ignores item in blocks. But the main reason to bring derive
        // helpers into scope in this case is to help with:
        // ```
        // #[derive(DeriveWithHelper)]
        // #[helper]
        // #[attr_macro]
        // struct Foo;
        // ```
        // Where `attr_macro`'s input will include `#[helper]` but not the derive, and it will likely therefore
        // also include it in its output. Therefore I hope not supporting blocks is fine at least for now.
        if let Some(attr_macro_item) = attr_macro_item
            && let Some(derive_helpers) = self.def_map.derive_helpers_in_scope.get(&attr_macro_item)
        {
            // The map is written below while the list is read.
            // clone: only for an attribute macro's expansion on an item with derive helpers
            let derive_helpers = derive_helpers.clone();
            for item in item_tree.top_level_items() {
                self.def_map
                    .derive_helpers_in_scope
                    .entry(InFile::new(file_id, item.ast_id(item_tree)))
                    .or_default()
                    .extend(derive_helpers.iter().cloned());
            }
        }

        let mod_dir = if macro_call_id.is_include_macro(self.db) {
            ModDir::root()
        } else {
            self.mod_dirs[&module_id]
        };

        ModCollector {
            def_collector: &mut *self,
            macro_depth: depth,
            tree_id: TreeId::new(file_id, None),
            module_id,
            item_tree,
            mod_dir,
        }
        .collect(item_tree.top_level_items(), container);
    }

    fn finish(mut self) -> (DefMap, LocalDefMap) {
        // Emit diagnostics for all remaining unexpanded macros.
        let _p = tracing::info_span!("DefCollector::finish").entered();

        for directive in &self.unresolved_macros {
            match &directive.kind {
                MacroDirectiveKind::FnLike { ast_id, expand_to, ctxt: call_site } => {
                    // FIXME: we shouldn't need to re-resolve the macro here just to get the unresolved error!
                    let macro_call_as_call_id = macro_call_as_call_id(
                        self.db,
                        ast_id.ast_id,
                        &ast_id.path,
                        *call_site,
                        *expand_to,
                        self.def_map.krate,
                        directive.depth,
                        self.def_map.recursion_limit(),
                        |path| {
                            let resolved_res = self.def_map.resolve_path_fp_with_macro(
                                self.crate_local_def_map.unwrap_or(&self.local_def_map),
                                self.db,
                                ResolveMode::Other,
                                directive.module_id,
                                path,
                                BuiltinShadowMode::Module,
                                Some(MacroSubNs::Bang),
                            );
                            resolved_res.resolved_def.take_macros().map(|it| it.definition(self.db))
                        },
                        // alloc: none unless an eager input holds macro calls; only the error is kept
                        &mut Vec::new(),
                    );
                    if let Err(UnresolvedMacro { path }) = macro_call_as_call_id {
                        self.def_map.diagnostics.push(DefDiagnostic::unresolved_macro_call(
                            directive.module_id,
                            MacroCallKind::FnLike {
                                ast_id: ast_id.ast_id,
                                expand_to: *expand_to,
                                eager: None,
                            },
                            path,
                        ));
                    }
                }
                MacroDirectiveKind::Derive {
                    ast_id,
                    derive_attr,
                    derive_pos,
                    derive_macro_id,
                    ..
                } => {
                    self.def_map.diagnostics.push(DefDiagnostic::unresolved_macro_call(
                        directive.module_id,
                        MacroCallKind::Derive {
                            ast_id: ast_id.ast_id,
                            derive_attr_index: *derive_attr,
                            derive_index: *derive_pos as u32,
                            derive_macro_id: *derive_macro_id,
                        },
                        ModPath::clone(&ast_id.path),
                    ));
                }
                // These are diagnosed by `reseed_with_unresolved_attribute`, as that function consumes them
                MacroDirectiveKind::Attr { .. } => {}
            }
        }

        // Emit diagnostics for all remaining unresolved imports.
        for import in self.imports.unresolved() {
            let &ImportDirective {
                module_id,
                import:
                    Import {
                        ref path,
                        source: ImportSource { use_tree, id, is_prelude: _, kind: _ },
                        ..
                    },
                ..
            } = import;
            if matches!(
                (path.segments().first(), &path.kind),
                (Some(krate), PathKind::Plain | PathKind::Abs) if self.unresolved_extern_crates.contains(krate)
            ) {
                continue;
            }
            let item_tree_id = id.lookup(self.db).id;
            self.def_map.diagnostics.push(DefDiagnostic::unresolved_import(
                module_id,
                item_tree_id,
                use_tree,
            ));
        }

        // Then what rustc reports of imports of a trait's associated items.
        for directive in &self.imports.directives {
            let ImportDirective { module_id, import: Import { source, .. }, status } = directive;
            let ImportSource { use_tree, id, kind, .. } = *source;
            let (db, def_map) = (self.db, &mut self.def_map);
            // alloc: none, at most one report per namespace (two), inline
            let mut reports = SmallVec::<[TraitItemImport; 2]>::new();
            def_map.trait_item_import_reports(db, kind, status.namespaces(), |it| reports.push(it));
            if is_glob_of_non_module(kind, status.namespaces()) {
                let item_tree_id = id.lookup(db).id;
                def_map.diagnostics.push(DefDiagnostic::unresolved_import(
                    *module_id,
                    item_tree_id,
                    use_tree,
                ));
            }
            for why in reports {
                let item_tree_id = id.lookup(db).id;
                def_map.diagnostics.push(DefDiagnostic::trait_item_import(
                    *module_id,
                    item_tree_id,
                    use_tree,
                    why,
                ));
            }
        }

        always!(
            self.deferred_builtin_derives.is_empty(),
            "self.deferred_builtin_derives={:#?}",
            self.deferred_builtin_derives,
        );

        self.def_map.names = self.name_records();

        (self.def_map, self.local_def_map)
    }

    /// Resolves each import and macro path again on the def map as collected, as rustc's
    /// `finalize_imports` and `finalize_macro_resolutions` do, into what they went through.
    fn name_records(&mut self) -> NameRecords {
        let db = self.db;
        let def_map = &self.def_map;
        let local_def_map = self.crate_local_def_map.unwrap_or(&self.local_def_map);
        let q = (local_def_map, db, ResolveMode::Import);
        // alloc: a record of the def map, made once.
        let mut uses = Vec::new();
        // alloc: reused for each import.
        let mut found = Vec::new();
        // alloc: a record of the def map, made once.
        let mut unresolved = Vec::new();
        // alloc: a record of the def map, made once.
        let mut glob_targets = Vec::new();
        for (directive, list) in self.imports.directives.iter().zip(&self.imports.list) {
            let ImportDirective { module_id, ref import, status } = *directive;
            let ImportSource { use_tree, id, kind, .. } = import.source;
            let own = match kind {
                ImportKind::Glob => ImportOrExternCrate::Glob(GlobId { use_: id, idx: use_tree }),
                ImportKind::Plain | ImportKind::TypeOnly => {
                    ImportOrExternCrate::Import(ImportId { use_: id, idx: use_tree })
                }
            };
            if *list == ImportList::Unresolved {
                unresolved.push(own);
                continue;
            }
            let ns = match kind {
                ImportKind::Plain => None,
                ImportKind::Glob | ImportKind::TypeOnly => Some(Namespace::Types),
            };
            let shadow = BuiltinShadowMode::Module;
            def_map.path_uses(q, module_id, &import.path, shadow, None, ns, true, &mut found);
            uses.extend(found.drain(..).filter(|it: &NameUse| it.import != own));
            if kind == ImportKind::Glob
                && !import.source.is_prelude
                && let Some(ModuleDefId::ModuleId(target)) = status.namespaces().take_types()
                && target.krate(db) == def_map.krate
                && let ImportOrExternCrate::Glob(glob) = own
            {
                glob_targets.push((glob, target));
            }
        }
        // alloc: a record of the def map, made once.
        let mut macro_calls = Vec::with_capacity(self.resolved_macro_paths.len());
        for it in &self.resolved_macro_paths {
            let ResolvedMacroPath { module, ref path, subns, def, call } = *it;
            let Some((macro_id, via)) =
                def_map.macro_via(local_def_map, db, module, path, subns, def)
            else {
                continue;
            };
            if let Some(call) = call {
                macro_calls.push(MacroCallRecord { call, def: macro_id, via });
            }
            if via != MacroVia::Textual {
                let ns = Namespace::Macros;
                let subns = Some(subns);
                def_map.path_uses_outside_imports(
                    local_def_map,
                    db,
                    module,
                    path,
                    ns,
                    subns,
                    true,
                    &mut uses,
                );
            }
        }
        let mut seen = HashSet::default();
        uses.retain(|&it| seen.insert(it));
        NameRecords {
            uses: uses.into_boxed_slice(),
            unresolved: unresolved.into_boxed_slice(),
            glob_targets: glob_targets.into_boxed_slice(),
            macro_calls: macro_calls.into_boxed_slice(),
            derive_helpers: mem::take(&mut self.derive_helper_uses).into_boxed_slice(),
            not_introducing: mem::take(&mut self.not_introducing).into_boxed_slice(),
        }
    }
}

/// Walks a single module, populating defs, imports and macros
struct ModCollector<'a, 'db> {
    def_collector: &'a mut DefCollector<'db>,
    macro_depth: u32,
    module_id: ModuleId,
    tree_id: TreeId,
    item_tree: &'db ItemTree,
    mod_dir: ModDir,
}

impl<'db> ModCollector<'_, 'db> {
    fn collect_in_top_module(&mut self, items: &[ModItemId]) {
        self.collect(items, self.module_id.into())
    }

    fn collect(&mut self, items: &[ModItemId], container: ItemContainerId) {
        let is_crate_root = self.module_id == self.def_collector.def_map.root
            && self.def_collector.def_map.block.is_none();

        // Note: don't assert that inserted value is fresh: it's simply not true
        // for macros.
        self.def_collector.mod_dirs.insert(self.module_id, self.mod_dir);

        let db = self.def_collector.db;
        let module_id = self.module_id;
        let consider_deferred_derives =
            |file_id: HirFileId,
             deferred_derives: &mut HashMap<_, Vec<DeferredBuiltinDerive>>,
             ast_id: FileAstId<ast::Adt>,
             id: AdtId,
             def_map: &mut DefMap| {
                let ast_id = InFile::new(file_id, ast_id.upcast());
                let Some(deferred_derives) = deferred_derives.remove(&ast_id.upcast()) else {
                    return;
                };
                let module = &mut def_map.modules.building()[module_id];
                for deferred_derive in deferred_derives {
                    crate::builtin_derive::with_derive_traits(deferred_derive.derive, |trait_| {
                        let impl_id = BuiltinDeriveImplId::new(
                            db,
                            BuiltinDeriveImplLoc {
                                adt: id,
                                trait_,
                                derive_attr_id: deferred_derive.derive_attr_id,
                                derive_index: deferred_derive.derive_index,
                            },
                        );
                        module.scope_mut().define_builtin_derive_impl(impl_id);
                        module.scope_mut().set_derive_macro_invoc(
                            ast_id,
                            Either::Right(impl_id),
                            deferred_derive.derive_attr_id,
                            deferred_derive.derive_index as usize,
                        );
                        // Change its helper attributes to the new id.
                        if let Some(derive_helpers) =
                            def_map.derive_helpers_in_scope.get_mut(&ast_id.map(|it| it.upcast()))
                        {
                            for (_, _, call_id) in
                                &mut derive_helpers[deferred_derive.helpers_range.start
                                    ..deferred_derive.helpers_range.end]
                            {
                                *call_id = Either::Right(impl_id);
                            }
                        }
                    });
                }
            };
        let update_def =
            |def_collector: &mut DefCollector<'_>, id, name: &Name, vis, value_ns_ctor_vis| {
                def_collector.def_map.modules.building()[module_id].scope_mut().declare(id);
                def_collector.update(
                    module_id,
                    &[(Some(*name), PerNs::from_def(id, vis, value_ns_ctor_vis, None))],
                    vis,
                    None,
                )
            };
        let resolve_vis = |def_map: &DefMap, local_def_map: &LocalDefMap, visibility| {
            def_map
                .resolve_visibility(local_def_map, db, module_id, visibility, false)
                .unwrap_or(Visibility::Public)
        };

        let item_tree = self.item_tree;

        let mut process_mod_item = |item_id: ModItemId| {
            let attrs = match item_tree.attrs(item_id) {
                Some(AttrsOrCfgRef::Enabled { attrs }) => attrs,
                None => Attrs::EMPTY,
                Some(AttrsOrCfgRef::CfgDisabled(cfg, _)) => {
                    let ast_id = item_id.ast_id(self.item_tree).erase();
                    self.emit_unconfigured_diagnostic(InFile::new(self.file_id(), ast_id), cfg);
                    return;
                }
            };

            if let Err(()) = self.resolve_attributes(attrs, item_id, container) {
                // Do not process the item. It has at least one non-builtin attribute, so the
                // fixed-point algorithm is required to resolve the rest of them.
                return;
            }

            let def_map = &mut self.def_collector.def_map;
            let local_def_map =
                self.def_collector.crate_local_def_map.unwrap_or(&self.def_collector.local_def_map);

            match item_tree.index(item_id) {
                ModItemKind::Mod(ast_id, module) => self.collect_module(ast_id, module, attrs),
                ModItemKind::Use(ast_id, use_) => {
                    let file_id = self.tree_id.file_id();
                    let id = UseLoc { container: module_id, id: InFile::new(file_id, ast_id) }
                        .intern(db);
                    def_map.modules.building()[self.module_id].scope_mut().define_use_decl(id);
                    let is_prelude = attrs.by_key(sym::prelude_import).exists();
                    Import::from_use(item_tree, use_, id, is_prelude, |import| {
                        self.def_collector.imports.push(ImportDirective {
                            module_id: self.module_id,
                            import,
                            status: PartialResolvedImport::Unresolved,
                        });
                    })
                }
                ModItemKind::ExternCrate(ast_id, item) => {
                    let item_tree::ExternCrate { name, visibility, alias } = item;
                    let name = &item_tree[*name];
                    let alias = item_tree.import_alias(*alias);

                    let id = ExternCrateLoc {
                        container: module_id,
                        id: InFile::new(self.tree_id.file_id(), ast_id),
                    }
                    .intern(db);
                    def_map.modules.building()[self.module_id]
                        .scope_mut()
                        .define_extern_crate_decl(id);

                    let is_self = *name == sym::self_;
                    let resolved = if is_self {
                        cov_mark::hit!(extern_crate_self_as);
                        Some(def_map.crate_root(db))
                    } else {
                        self.def_collector
                            .deps
                            .get(name)
                            .map(|dep| crate_def_map(db, dep.crate_id).root_module_id())
                    };

                    let name = match &alias {
                        Some(ImportAlias::Alias(name)) => Some(name),
                        Some(ImportAlias::Underscore) => None,
                        None => Some(name),
                    };

                    if let Some(resolved) = resolved {
                        let vis = resolve_vis(def_map, local_def_map, &self.item_tree[*visibility]);

                        if is_crate_root {
                            // extern crates in the crate root are special-cased to insert entries into the extern prelude: rust-lang/rust#54658
                            if let Some(name) = name {
                                // rustc's `introduced_by_item`: renamed, or naming a crate the
                                // dependencies do not put in the extern prelude.
                                let from_deps = matches!(
                                    self.def_collector.local_def_map.extern_prelude.get(name),
                                    Some((_, None))
                                );
                                if alias.is_none() && from_deps {
                                    self.def_collector.not_introducing.push(id);
                                }
                                self.def_collector
                                    .local_def_map
                                    .extern_prelude
                                    .insert(*name, (resolved, Some(id)));
                                self.def_collector.imports.wake_env(*name, TYPES);
                            }
                            // they also allow `#[macro_use]`
                            if !is_self {
                                self.process_macro_use_extern_crate(
                                    id,
                                    attrs.by_key(sym::macro_use).attrs(),
                                    resolved.krate(self.def_collector.db),
                                );
                            }
                        }

                        self.def_collector.update(
                            module_id,
                            &[(
                                name.cloned(),
                                PerNs::types(
                                    resolved.into(),
                                    vis,
                                    Some(ImportOrExternCrate::ExternCrate(id)),
                                ),
                            )],
                            vis,
                            Some(ImportOrExternCrate::ExternCrate(id)),
                        );
                    } else {
                        if let Some(name) = name {
                            self.def_collector.unresolved_extern_crates.insert(*name);
                        }
                        self.def_collector.def_map.diagnostics.push(
                            DefDiagnostic::unresolved_extern_crate(
                                module_id,
                                InFile::new(self.file_id(), ast_id),
                            ),
                        );
                    }
                }
                ModItemKind::ExternBlock(ast_id, block) => {
                    let extern_block_id = ExternBlockLoc {
                        container: module_id,
                        id: InFile::new(self.file_id(), ast_id),
                    }
                    .intern(db);
                    self.def_collector.def_map.modules.building()[self.module_id]
                        .scope_mut()
                        .define_extern_block(extern_block_id);
                    self.collect(item_tree.items(block.children), ItemContainerId::ExternBlockId(extern_block_id))
                }
                ModItemKind::MacroCall(ast_id, mac) => {
                    self.collect_macro_call(ast_id, mac, container)
                }
                ModItemKind::MacroRules(ast_id, mac) => {
                    self.collect_macro_rules(attrs, ast_id, mac, module_id)
                }
                ModItemKind::Macro2(ast_id, mac) => {
                    self.collect_macro_def(attrs, ast_id, mac, module_id)
                }
                ModItemKind::Impl(ast_id, imp) => {
                    let impl_id =
                        ImplLoc { container: module_id, id: InFile::new(self.file_id(), ast_id) }
                            .intern(db);
                    let is_reservation = attrs.by_key(sym::rustc_reservation_impl).exists();
                    self.def_collector.def_map.modules.building()[self.module_id]
                        .scope_mut()
                        .define_impl(impl_id, imp.is_trait_impl, is_reservation)
                }
                ModItemKind::Function(ast_id, it) => {
                    let fn_id =
                        FunctionLoc { container, id: InFile::new(self.tree_id.file_id(), ast_id) }
                            .intern(db);

                    let vis = resolve_vis(def_map, local_def_map, &self.item_tree[it.visibility]);

                    update_def(self.def_collector, fn_id.into(), &item_tree[it.name], vis, None);

                    if self.def_collector.def_map.block.is_none()
                        && self.def_collector.is_proc_macro
                        && self.module_id == self.def_collector.def_map.root
                        && let Some(proc_macro) = attrs.parse_proc_macro_decl(&item_tree[it.name])
                    {
                        // A proc macro is implemented as a function, which is a macro to other
                        // crates. In its own it is a function still, which can be called and
                        // which rustc's list of the crate's macros names from the root;
                        // `censor_non_proc_macros` keeps it to the crate.
                        self.def_collector.export_proc_macro(
                            proc_macro,
                            InFile::new(self.file_id(), ast_id),
                            fn_id,
                        );
                    }
                }
                ModItemKind::Struct(ast_id, it) => {
                    let vis = resolve_vis(def_map, local_def_map, &self.item_tree[it.visibility]);
                    let value_ns_ctor_vis = match item_tree.value_ns_ctor(it) {
                        StructValueNsCtor::NoValueNsCtor => None,
                        StructValueNsCtor::ValueNsCtorWithVis(vis) => {
                            Some(resolve_vis(def_map, local_def_map, &self.item_tree[*vis]))
                        }
                        StructValueNsCtor::ValueNsCtorWithMinVis(multiple) => {
                            let mut each = multiple.iter().map(|&vis| {
                                resolve_vis(def_map, local_def_map, &self.item_tree[vis])
                            });
                            let first = each.next();
                            each.fold(first, |a: Option<Visibility>, b| a?.min(db, b, def_map))
                        }
                    };
                    let interned = StructLoc {
                        container: module_id,
                        id: InFile::new(self.tree_id.file_id(), ast_id),
                    }
                    .intern(db);
                    consider_deferred_derives(
                        self.tree_id.file_id(),
                        &mut self.def_collector.deferred_builtin_derives,
                        ast_id.upcast(),
                        interned.into(),
                        def_map,
                    );
                    update_def(
                        self.def_collector,
                        interned.into(),
                        &item_tree[it.name],
                        vis,
                        value_ns_ctor_vis,
                    );
                }
                ModItemKind::Union(ast_id, it) => {
                    let vis = resolve_vis(def_map, local_def_map, &self.item_tree[it.visibility]);
                    let interned = UnionLoc {
                        container: module_id,
                        id: InFile::new(self.tree_id.file_id(), ast_id),
                    }
                    .intern(db);
                    consider_deferred_derives(
                        self.tree_id.file_id(),
                        &mut self.def_collector.deferred_builtin_derives,
                        ast_id.upcast(),
                        interned.into(),
                        def_map,
                    );
                    update_def(self.def_collector, interned.into(), &item_tree[it.name], vis, None);
                }
                ModItemKind::Enum(ast_id, it) => {
                    let enum_ = EnumLoc {
                        container: module_id,
                        id: InFile::new(self.tree_id.file_id(), ast_id),
                    }
                    .intern(db);

                    consider_deferred_derives(
                        self.tree_id.file_id(),
                        &mut self.def_collector.deferred_builtin_derives,
                        ast_id.upcast(),
                        enum_.into(),
                        def_map,
                    );
                    let vis = resolve_vis(def_map, local_def_map, &self.item_tree[it.visibility]);
                    update_def(self.def_collector, enum_.into(), &item_tree[it.name], vis, None);
                }
                ModItemKind::Const(ast_id, it) => {
                    let const_id =
                        ConstLoc { container, id: InFile::new(self.tree_id.file_id(), ast_id) }
                            .intern(db);

                    match item_tree.get_name(it.name) {
                        Some(name) => {
                            let vis =
                                resolve_vis(def_map, local_def_map, &self.item_tree[it.visibility]);
                            update_def(self.def_collector, const_id.into(), name, vis, None);
                        }
                        None => {
                            // const _: T = ...;
                            self.def_collector.def_map.modules.building()[self.module_id]
                                .scope_mut()
                                .define_unnamed_const(const_id);
                        }
                    }
                }
                ModItemKind::Static(ast_id, it) => {
                    let vis = resolve_vis(def_map, local_def_map, &self.item_tree[it.visibility]);
                    update_def(
                        self.def_collector,
                        StaticLoc { container, id: InFile::new(self.file_id(), ast_id) }
                            .intern(db)
                            .into(),
                        &item_tree[it.name],
                        vis,
                        None,
                    );
                }
                ModItemKind::Trait(ast_id, it) => {
                    let vis = resolve_vis(def_map, local_def_map, &self.item_tree[it.visibility]);
                    update_def(
                        self.def_collector,
                        TraitLoc { container: module_id, id: InFile::new(self.file_id(), ast_id) }
                            .intern(db)
                            .into(),
                        &item_tree[it.name],
                        vis,
                        None,
                    );
                }
                ModItemKind::TypeAlias(ast_id, it) => {
                    let vis = resolve_vis(def_map, local_def_map, &self.item_tree[it.visibility]);
                    update_def(
                        self.def_collector,
                        TypeAliasLoc { container, id: InFile::new(self.file_id(), ast_id) }
                            .intern(db)
                            .into(),
                        &item_tree[it.name],
                        vis,
                        None,
                    );
                }
            }
        };

        // extern crates should be processed eagerly instead of deferred to resolving.
        // `#[macro_use] extern crate` is hoisted to imports macros before collecting
        // any other items.
        if is_crate_root {
            items
                .iter()
                .filter(move |&&it| matches!(item_tree.index(it), ModItemKind::ExternCrate(..)))
                .copied()
                .for_each(&mut process_mod_item);
            items
                .iter()
                .filter(move |&&it| !matches!(item_tree.index(it), ModItemKind::ExternCrate(..)))
                .copied()
                .for_each(process_mod_item);
        } else {
            items.iter().copied().for_each(process_mod_item);
        }
    }

    fn process_macro_use_extern_crate<'a>(
        &mut self,
        extern_crate_id: ExternCrateId,
        macro_use_attrs: impl Iterator<Item = &'a Attr>,
        target_crate: Crate,
    ) {
        cov_mark::hit!(macro_rules_from_other_crates_are_visible_with_macro_use);
        // alloc: only for `#[macro_use(..)]` on an `extern crate`; the import takes the list
        let mut single_imports = Vec::new();
        for attr in macro_use_attrs {
            let Some(paths) = attr.parse_path_comma_token_tree(self.def_collector.db) else {
                // `#[macro_use]` (without any paths) found, forget collected names and just import
                // all visible macros.
                self.def_collector.import_macros_from_extern_crate(
                    target_crate,
                    None,
                    Some(extern_crate_id),
                );
                return;
            };
            for (path, _, _) in paths {
                if let Some(name) = path.as_ident() {
                    single_imports.push(*name);
                }
            }
        }

        self.def_collector.import_macros_from_extern_crate(
            target_crate,
            Some(single_imports),
            Some(extern_crate_id),
        );
    }

    fn collect_module(
        &mut self,
        module_ast_id: ItemTreeAstId<Mod>,
        module: &Mod,
        attrs: Attrs<'_>,
    ) {
        let item_tree = self.item_tree;
        let name = &item_tree[module.name];
        let path_attr = attrs.by_key(sym::path).string_value_unescape();
        let is_macro_use = attrs.by_key(sym::macro_use).exists();
        match item_tree.mod_kind(module) {
            // inline module, just recurse
            ModKind::Inline { items } => {
                let Some(module_id) = self.push_child_module(
                    *name,
                    module_ast_id,
                    None,
                    &self.item_tree[module.visibility],
                ) else {
                    return;
                };

                let Some(mod_dir) = self.mod_dir.descend_into_definition(
                    name,
                    path_attr.as_deref(),
                    &mut self.def_collector.path_buf,
                ) else {
                    return;
                };
                ModCollector {
                    def_collector: &mut *self.def_collector,
                    macro_depth: self.macro_depth,
                    module_id,
                    tree_id: self.tree_id,
                    item_tree: self.item_tree,
                    mod_dir,
                }
                .collect_in_top_module(items);
                if is_macro_use {
                    self.import_all_legacy_macros(module_id);
                }
            }
            // out of line module, resolve, parse and recurse
            ModKind::Outline => {
                let file_id = self.file_id();
                let ast_id = AstId::new(file_id, module_ast_id);
                let db = self.def_collector.db;
                let krate = self.def_collector.def_map.krate;
                match self.mod_dir.resolve_declaration(
                    db,
                    krate,
                    file_id,
                    name,
                    path_attr.as_deref(),
                    &mut self.def_collector.path_buf,
                ) {
                    Ok((file_id, is_mod_rs, mod_dir)) => {
                        let item_tree =
                            file_item_tree(db, file_id.into(), self.def_collector.def_map.krate);
                        let top_level_attrs = item_tree.top_level_attrs();
                        match top_level_attrs {
                            Some(AttrsOrCfgRef::CfgDisabled(cfg, _)) => {
                                self.emit_unconfigured_diagnostic(
                                    InFile::new(self.file_id(), module_ast_id.erase()),
                                    cfg,
                                );
                            }
                            Some(AttrsOrCfgRef::Enabled { .. }) | None => {
                                let Some(module_id) = self.push_child_module(
                                    *name,
                                    ast_id.value,
                                    Some((file_id, is_mod_rs)),
                                    &self.item_tree[module.visibility],
                                ) else {
                                    return;
                                };
                                ModCollector {
                                    def_collector: self.def_collector,
                                    macro_depth: self.macro_depth,
                                    module_id,
                                    tree_id: TreeId::new(file_id.into(), None),
                                    item_tree,
                                    mod_dir,
                                }
                                .collect_in_top_module(item_tree.top_level_items());
                                let is_macro_use = is_macro_use
                                    || matches!(top_level_attrs, Some(AttrsOrCfgRef::Enabled { attrs }) if attrs.by_key(sym::macro_use).exists());
                                if is_macro_use {
                                    self.import_all_legacy_macros(module_id);
                                }
                            }
                        }
                    }
                    Err(candidates) => {
                        self.push_child_module(
                            *name,
                            ast_id.value,
                            None,
                            &self.item_tree[module.visibility],
                        );
                        self.def_collector.def_map.diagnostics.push(
                            DefDiagnostic::unresolved_module(self.module_id, ast_id, candidates),
                        );
                    }
                };
            }
        }
    }

    fn push_child_module(
        &mut self,
        name: Name,
        declaration: FileAstId<ast::Module>,
        definition: Option<(EditionedFileId, bool)>,
        visibility: &crate::visibility::RawVisibility,
    ) -> Option<ModuleId> {
        // A module's id is its parent and name, so a second `mod` of a name (E0428) declares
        // nothing: the first keeps the name, as in rustc's resolver (`facts::collect` too).
        let db = self.def_collector.db;
        if self.def_collector.def_map.module(db, self.module_id).children(db).contains_key(&name) {
            return None;
        }
        let vis = self
            .def_collector
            .def_map
            .resolve_visibility(
                self.def_collector.crate_local_def_map.unwrap_or(&self.def_collector.local_def_map),
                self.def_collector.db,
                self.module_id,
                visibility,
                false,
            )
            .unwrap_or(Visibility::Public);
        let origin = match definition {
            None => {
                ModuleOrigin::Inline { definition: declaration, definition_tree_id: self.tree_id }
            }
            Some((definition, is_mod_rs)) => ModuleOrigin::File {
                declaration,
                definition,
                is_mod_rs,
                declaration_tree_id: self.tree_id,
            },
        };

        let module = super::facts::queries::child_module(
            self.def_collector.db,
            self.def_collector.def_map.krate,
            self.def_collector.def_map.block_id(),
            self.module_id,
            name,
        );
        let def_map = &mut self.def_collector.def_map;
        let modules = def_map.modules.building();
        let mut data = ModuleData::new(origin, vis, Some(self.module_id));
        for (name, macs) in modules[self.module_id].scope(self.def_collector.db).legacy_macros() {
            for &mac in macs {
                data.scope_mut().define_legacy_macro(*name, mac);
            }
        }
        let replaced = modules[self.module_id].children_mut().insert(name, module);
        modules.insert(module, data);
        if replaced.is_some() {
            // A path can read whether a module is its parent's `__blaze_test_harness`.
            self.def_collector.imports.wake_all();
        }

        let def = ModuleDefId::from(module);

        def_map.modules.building()[self.module_id].scope_mut().declare(def);
        self.def_collector.update(
            self.module_id,
            &[(Some(name), PerNs::from_def(def, vis, None, None))],
            vis,
            None,
        );
        Some(module)
    }

    /// Resolves attributes on an item.
    ///
    /// Returns `Err` when some attributes could not be resolved to builtins and have been
    /// registered as unresolved.
    ///
    /// If `ignore_up_to` is `Some`, attributes preceding and including that attribute will be
    /// assumed to be resolved already.
    fn resolve_attributes(
        &mut self,
        attrs: Attrs<'db>,
        mod_item: ModItemId,
        container: ItemContainerId,
    ) -> Result<(), ()> {
        let skip_attrs = &self.def_collector.skip_attrs;
        let ignore_up_to = match skip_attrs.is_empty() {
            true => None,
            false => skip_attrs.get(&InFile::new(self.file_id(), mod_item)).copied(),
        };
        for (attr_id, attr) in attrs.iter_after(ignore_up_to) {
            if self.def_collector.def_map.is_builtin_or_registered_attr(&attr.path) {
                continue;
            }
            tracing::debug!(
                "non-builtin attribute {}",
                attr.path.display(self.def_collector.db, Edition::LATEST)
            );

            let ast_id = AstIdWithPath::new(
                self.file_id(),
                mod_item.ast_id(self.item_tree),
                ModPath::clone(&attr.path),
            );
            self.def_collector.unresolved_macros.push(MacroDirective {
                module_id: self.module_id,
                depth: self.macro_depth + 1,
                kind: MacroDirectiveKind::Attr {
                    ast_id,
                    attr_id,
                    attr,
                    mod_item,
                    tree: self.tree_id,
                    item_tree: self.item_tree,
                },
                container,
            });

            return Err(());
        }

        Ok(())
    }

    fn collect_macro_rules(
        &mut self,
        attrs: Attrs<'_>,
        ast_id: ItemTreeAstId<MacroRules>,
        mac: &MacroRules,
        module: ModuleId,
    ) {
        let krate = self.def_collector.def_map.krate;
        let f_ast_id = InFile::new(self.file_id(), ast_id.upcast());

        let export_attr = || attrs.by_key(sym::macro_export);

        let is_export = export_attr().exists();
        let local_inner = if is_export {
            export_attr().tt_values().flat_map(|it| it.iter()).any(|it| match it {
                tt::TtElement::Leaf(tt::Leaf::Ident(ident)) => ident.sym == sym::local_inner_macros,
                _ => false,
            })
        } else {
            false
        };

        // Case 1: builtin macros
        let expander = if attrs.by_key(sym::rustc_builtin_macro).exists() {
            // `#[rustc_builtin_macro = "builtin_name"]` overrides the `macro_rules!` name.
            let name = match attrs.by_key(sym::rustc_builtin_macro).string_value_with_span() {
                Some((it, span)) => &Name::new_symbol(Symbol::intern(it), span.ctx),
                None => {
                    let explicit_name =
                        attrs.by_key(sym::rustc_builtin_macro).tt_values().next().and_then(|tt| {
                            match tt.token_trees().iter().next() {
                                Some(tt::TtElement::Leaf(tt::Leaf::Ident(name))) => Some(name),
                                _ => None,
                            }
                        });
                    match explicit_name {
                        Some(ident) => &ident.as_name(),
                        None => &self.item_tree[mac.name],
                    }
                }
            };
            match find_builtin_macro(name) {
                Some(Either::Left(it)) => MacroExpander::BuiltIn(it),
                Some(Either::Right(it)) => MacroExpander::BuiltInEager(it),
                None => {
                    self.def_collector
                        .def_map
                        .diagnostics
                        .push(DefDiagnostic::unimplemented_builtin_macro(self.module_id, f_ast_id));
                    MacroExpander::UnimplementedBuiltIn
                }
            }
        } else {
            // Case 2: normal `macro_rules!` macro
            let id = InFile::new(self.file_id(), ast_id);
            let decl_expander = id.upcast().decl_macro_expander(self.def_collector.db, krate);
            let styles = decl_expander.mac.rule_styles();
            MacroExpander::Declarative { styles }
        };
        let allow_internal_unsafe = attrs.by_key(sym::allow_internal_unsafe).exists();

        let mut flags = MacroRulesLocFlags::empty();
        flags.set(MacroRulesLocFlags::LOCAL_INNER, local_inner);
        flags.set(MacroRulesLocFlags::ALLOW_INTERNAL_UNSAFE, allow_internal_unsafe);
        let edition_panic = attrs
            .by_key(sym::allow_internal_unstable)
            .tt_values()
            .flat_map(|it| it.iter())
            .any(|it| matches!(it, tt::TtElement::Leaf(tt::Leaf::Ident(ident)) if ident.sym == sym::edition_panic));
        flags.set(MacroRulesLocFlags::EDITION_PANIC, edition_panic);

        let macro_id = MacroRulesLoc {
            container: module,
            id: InFile::new(self.file_id(), ast_id),
            flags,
            expander,
            edition: self.def_collector.def_map.data.edition,
        }
        .intern(self.def_collector.db);
        self.def_collector.define_macro_rules(
            self.module_id,
            self.item_tree[mac.name],
            macro_id,
            is_export,
        );
    }

    fn collect_macro_def(
        &mut self,
        attrs: Attrs<'_>,
        ast_id: ItemTreeAstId<Macro2>,
        mac: &Macro2,
        module: ModuleId,
    ) {
        let krate = self.def_collector.def_map.krate;
        let item_tree = self.item_tree;
        let mac_name = &item_tree[mac.name];
        let f_ast_id = InFile::new(self.file_id(), ast_id.upcast());

        // Case 1: builtin macros
        let mut helpers_opt = None;
        let expander = if attrs.by_key(sym::rustc_builtin_macro).exists() {
            if let Some(expander) = find_builtin_macro(mac_name) {
                match expander {
                    Either::Left(it) => MacroExpander::BuiltIn(it),
                    Either::Right(it) => MacroExpander::BuiltInEager(it),
                }
            } else if let Some(expander) = find_builtin_derive(mac_name) {
                if let Some(attr) = attrs.by_key(sym::rustc_builtin_macro).tt_values().next() {
                    // NOTE: The item *may* have both `#[rustc_builtin_macro]` and `#[proc_macro_derive]`,
                    // in which case rustc ignores the helper attributes from the latter, but it
                    // "doesn't make sense in practice" (see rust-lang/rust#87027).
                    if let Some((name, helpers)) = parse_macro_name_and_helper_attrs(attr) {
                        // NOTE: rustc overrides the name if the macro name if it's different from the
                        // macro name, but we assume it isn't as there's no such case yet. FIXME if
                        // the following assertion fails.
                        stdx::always!(
                            name == *mac_name,
                            "built-in macro {} has #[rustc_builtin_macro] which declares different name {}",
                            mac_name.display(self.def_collector.db, Edition::LATEST),
                            name.display(self.def_collector.db, Edition::LATEST),
                        );
                        helpers_opt = Some(helpers);
                    }
                }
                MacroExpander::BuiltInDerive(expander)
            } else if let Some(expander) = find_builtin_attr(mac_name) {
                MacroExpander::BuiltInAttr(expander)
            } else {
                self.def_collector
                    .def_map
                    .diagnostics
                    .push(DefDiagnostic::unimplemented_builtin_macro(self.module_id, f_ast_id));
                MacroExpander::UnimplementedBuiltIn
            }
        } else {
            // Case 2: normal `macro`
            let id = InFile::new(self.file_id(), ast_id);
            let decl_expander = id.upcast().decl_macro_expander(self.def_collector.db, krate);
            let styles = decl_expander.mac.rule_styles();
            MacroExpander::Declarative { styles }
        };
        let allow_internal_unsafe = attrs.by_key(sym::allow_internal_unsafe).exists();

        let macro_id = Macro2Loc {
            container: module,
            id: InFile::new(self.file_id(), ast_id),
            expander,
            allow_internal_unsafe,
            edition: self.def_collector.def_map.data.edition,
        }
        .intern(self.def_collector.db);
        self.def_collector.define_macro_def(
            self.module_id,
            *mac_name,
            macro_id,
            &self.item_tree[mac.visibility],
        );
        if let Some(helpers) = helpers_opt
            && self.def_collector.def_map.block.is_none()
        {
            Arc::get_mut(&mut self.def_collector.def_map.data)
                .unwrap()
                .exported_derives
                .insert(macro_id.into(), helpers);
        }
    }

    fn collect_macro_call(
        &mut self,
        ast_id: FileAstId<ast::MacroCall>,
        macro_call: &MacroCall,
        container: ItemContainerId,
    ) {
        let &MacroCall { path, expand_to, ctxt } = macro_call;
        let path = &self.item_tree[path];
        let ast_id = AstIdWithPath::new(self.file_id(), ast_id, ModPath::clone(path));
        let db = self.def_collector.db;

        // FIXME: Immediately expanding in "Case 1" is insufficient since "Case 2" may also define
        // new legacy macros that create textual scopes. We need a way to resolve names in textual
        // scopes without eager expansion.

        // alloc: none unless the call is eager and its input holds macro calls
        let mut eager_calls = Vec::new();
        // alloc: the paths an eager call's arguments name, kept once it resolves.
        let named = std::cell::RefCell::new(Vec::new());
        // Case 1: try to resolve macro calls with single-segment name, or with a path to a macro
        // of another crate, and expand them, so that the `macro_rules!` they define are in the
        // textual scope of what follows, child modules included.
        if let Ok(res) = macro_call_as_call_id(
            db,
            ast_id.ast_id,
            &ast_id.path,
            ctxt,
            expand_to,
            self.def_collector.def_map.krate,
            self.macro_depth + 1,
            self.def_collector.def_map.recursion_limit(),
            |path| {
                let def = self.case_one_macro_def(path);
                if let Some(def) = def
                    && path != &ast_id.path
                {
                    // clone: kept until the def map's records are made.
                    named.borrow_mut().push((path.clone(), def));
                }
                def
            },
            &mut eager_calls,
        ) {
            for (ptr, call_id) in eager_calls {
                self.def_collector.def_map.modules.building()[self.module_id]
                    .scope_mut()
                    .add_macro_invoc(ptr.map(|(_, it)| it), call_id);
            }
            // FIXME: if there were errors, this might've been in the eager expansion from an
            // unresolved macro, so we need to push this into late macro resolution. see fixme above
            if res.err.is_none() {
                // Legacy macros need to be expanded immediately, so that any macros they produce
                // are in scope.
                if let Some(call_id) = res.value {
                    let module = self.module_id;
                    let paths = &mut self.def_collector.resolved_macro_paths;
                    paths.push(ResolvedMacroPath {
                        module,
                        // clone: kept until the def map's records are made.
                        path: ast_id.path.clone(),
                        subns: MacroSubNs::Bang,
                        def: call_id.loc(db).def,
                        call: Some(call_id),
                    });
                    paths.extend(named.into_inner().into_iter().map(|(path, def)| {
                        ResolvedMacroPath { module, path, subns: MacroSubNs::Bang, def, call: None }
                    }));
                    self.def_collector.def_map.modules.building()[self.module_id]
                        .scope_mut()
                        .add_macro_invoc(ast_id.ast_id, call_id);
                    self.def_collector.collect_macro_expansion(
                        self.module_id,
                        call_id,
                        self.macro_depth + 1,
                        container,
                        None,
                    );
                }

                return;
            }
        }

        // Case 2: resolve in module scope, expand during name resolution.
        self.def_collector.unresolved_macros.push(MacroDirective {
            module_id: self.module_id,
            depth: self.macro_depth + 1,
            kind: MacroDirectiveKind::FnLike { ast_id, expand_to, ctxt },
            container,
        });
    }

    /// What Case 1 of [`Self::collect_macro_call`] resolves `path` to: the textual scope, the
    /// module's scope, the `#[macro_use]` prelude, a name a `use` of another crate imports, or
    /// the std prelude.
    fn case_one_macro_def(&self, path: &ModPath) -> Option<MacroDefId> {
        let db = self.def_collector.db;
        {
                let def_map = &self.def_collector.def_map;
                // Where the path goes on in another crate, that crate is collected in full, so
                // what it names there it will still name once this crate is.
                let of_another_crate = |path: &ModPath| {
                    let local_def_map = self
                        .def_collector
                        .crate_local_def_map
                        .unwrap_or(&self.def_collector.local_def_map);
                    let resolved = def_map.resolve_path_fp_with_macro(
                        local_def_map,
                        db,
                        ResolveMode::Other,
                        self.module_id,
                        path,
                        BuiltinShadowMode::Module,
                        Some(MacroSubNs::Bang),
                    );
                    let whole =
                        resolved.segment_index.is_none() && resolved.prefix_info.differing_crate;
                    resolved
                        .resolved_def
                        .take_macros()
                        .filter(|&id| whole && sub_namespace_match(db, id, Some(MacroSubNs::Bang)))
                        .map(|it| it.definition(db))
                };
                let Some(name) = path.as_ident() else { return of_another_crate(path) };
                // A name a `use` of this module imports from another crate (`use cfg_if::cfg_if;`)
                // is not in its scope until imports are resolved, after this call would be:
                // lol_html's `cfg_if!` defines its `trace!` for the modules that follow.
                let imported = || {
                    self.def_collector
                        .imports
                        .unresolved_named(self.module_id, *name)
                        .find_map(|it| of_another_crate(&it.import.path))
                };
                // The std prelude is another crate's, collected in full. Its `include!` read at
                // item level puts the `macro_rules!` of the file in the textual scope of what
                // follows: syn 1's `#[macro_use] mod token` has `Token!` from
                // `include!("await.rs")`, and its `mod op` names it with nothing imported.
                let prelude = || {
                    // A block's def map is collected after the def maps around it, so what the
                    // name is there (the std prelude, or an item of the module holding the
                    // block) it stays. clang-sys's `load_manually` has a `mod build` in its
                    // body whose `include!`d `macro_rules!` its `mod dynamic` uses.
                    if def_map.block.is_some() {
                        let local_def_map = self
                            .def_collector
                            .crate_local_def_map
                            .unwrap_or(&self.def_collector.local_def_map);
                        return def_map
                            .resolve_path_fp_with_macro(
                                local_def_map,
                                db,
                                ResolveMode::Other,
                                self.module_id,
                                path,
                                BuiltinShadowMode::Module,
                                Some(MacroSubNs::Bang),
                            )
                            .resolved_def
                            .take_macros()
                            .filter(|&id| sub_namespace_match(db, id, Some(MacroSubNs::Bang)))
                            .map(|it| it.definition(db));
                    }
                    let (prelude, _) = def_map.prelude?;
                    if prelude.krate(db) == def_map.krate {
                        return None;
                    }
                    def_map
                        .resolve_in_prelude(db, name)
                        .take_macros()
                        .filter(|&id| sub_namespace_match(db, id, Some(MacroSubNs::Bang)))
                        .map(|it| it.definition(db))
                };
                def_map
                    .legacy_macro_in_textual_scope(db, self.module_id, name)
                    .or_else(|| {
                        def_map
                            .module(db, self.module_id)
                            .scope(self.def_collector.db)
                            .get(name)
                            .take_macros()
                    })
                    .or_else(|| Some(def_map.macro_use_prelude.get(name).copied()?.0))
                    .filter(|&id| sub_namespace_match(db, id, Some(MacroSubNs::Bang)))
                    .map(|it| it.definition(self.def_collector.db))
                    .or_else(imported)
                    .or_else(prelude)
        }
    }

    fn import_all_legacy_macros(&mut self, module_id: ModuleId) {
        let [Some(source), Some(target)] = self
            .def_collector
            .def_map
            .modules
            .building()
            .get_disjoint_mut([&module_id, &self.module_id])
        else {
            return;
        };

        for (name, macs) in source.scope(self.def_collector.db).legacy_macros() {
            if let Some(&mac) = macs.last() {
                let name: Name = *name;
                target.scope_mut().define_legacy_macro(name, mac);
                self.def_collector.imports.wake_env(name, MACROS);
            }
        }
    }

    fn emit_unconfigured_diagnostic(&mut self, ast_id: ErasedAstId, cfg: &CfgExpr) {
        self.def_collector.def_map.diagnostics.push(DefDiagnostic::unconfigured_code(
            self.module_id,
            ast_id,
            // clone: only for cfg'd-out code, into its diagnostic
            cfg.clone(),
        ));
    }

    #[inline]
    fn file_id(&self) -> HirFileId {
        self.tree_id.file_id()
    }
}

#[cfg(test)]
mod tests {
    use hir_expand::fixture;

    use crate::test_db::TestDB;

    use super::*;

    fn do_resolve(not_ra_fixture: &str) {
        let fx = fixture::db(not_ra_fixture);
        let _ = fx.only_file();
        let db = fx.into_db();
        let krate = fixture::test_crate(&db);

        crate_def_map(&db, krate);
    }

    #[test]
    fn test_macro_expand_will_stop_1() {
        do_resolve(
            r#"
macro_rules! foo {
    ($($ty:ty)*) => { foo!($($ty)*); }
}
foo!(KABOOM);
"#,
        );
        do_resolve(
            r#"
macro_rules! foo {
    ($($ty:ty)*) => { foo!(() $($ty)*); }
}
foo!(KABOOM);
"#,
        );
    }

    #[ignore]
    #[test]
    fn test_macro_expand_will_stop_2() {
        // FIXME: this test does succeed, but takes quite a while: 90 seconds in
        // the release mode. That's why the argument is not an ra_fixture --
        // otherwise injection highlighting gets stuck.
        //
        // We need to find a way to fail this faster!
        do_resolve(
            r#"
macro_rules! foo {
    ($($ty:ty)*) => { foo!($($ty)* $($ty)*); }
}
foo!(KABOOM);
"#,
        );
    }

    #[test]
    fn crate_attrs() {
        let fixture = r#"
//- /lib.rs crate:foo crate-attr:recursion_limit="4" crate-attr:no_core crate-attr:no_std crate-attr:feature(register_tool)
        "#;
        let fx = fixture::db(fixture);
        let _ = fx.only_file();
        let db = fx.into_db();
        let def_map = crate_def_map(&db, fixture::test_crate(&db));
        assert_eq!(def_map.recursion_limit(), 4);
        assert!(def_map.is_no_core());
        assert!(def_map.is_no_std());
        assert!(def_map.features().is_enabled(&sym::register_tool));
    }
}
