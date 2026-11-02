//! A unit's imports and the worklist of their fixed point: the old collector's `Imports`, over
//! owned [`ImportOp`]s so a unit's state is a plain query value carried from round to round.
//!
//! What is carried ([`Imports`]) is the imports and what each resolved to. The worklist
//! ([`ImportQueues`]) is one run's: a unit makes it in its query's frame and drops it with the
//! run. A run's fixed point ends with every queue empty and every attempt's waits stale for the
//! next run, which starts by retrying every import it has (the fresh path pushes each one, the
//! carried path wakes them all), so a new worklist resolves as the carried one did.

use std::{
    alloc::{Allocator, Global},
    cmp::Reverse,
    collections::BinaryHeap,
    iter, mem,
};

use blaze_hash::FixedState;
use blaze_mem::Frame;
use hashbrown::{HashMap, hash_map::Entry};
use hir_expand::name::Name;

use crate::{
    ModuleId,
    item_scope::GlobId,
    item_tree::ImportKind,
    nameres::facts::{FrameMap, FrameVec, map_in, ops::ImportOp},
    per_ns::PerNs,
    visibility::Visibility,
};

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(super) enum PartialResolvedImport {
    /// None of any namespaces is resolved
    Unresolved,
    /// One of namespaces is resolved
    Indeterminate(PerNs),
    /// All namespaces are resolved, OR it comes from other crate
    Resolved(PerNs),
}

impl PartialResolvedImport {
    pub(super) fn namespaces(self) -> PerNs {
        match self {
            PartialResolvedImport::Unresolved => PerNs::none(),
            PartialResolvedImport::Indeterminate(ns) | PartialResolvedImport::Resolved(ns) => ns,
        }
    }
}

/// One import of the unit and what its last attempt found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Directive {
    pub(super) op: ImportOp,
    pub(super) status: PartialResolvedImport,
}

/// Bits of a namespace mask: which of a name's namespaces a write touched, or a path read.
pub(super) const TYPES: u8 = 1;
pub(super) const VALUES: u8 = 2;
pub(super) const MACROS: u8 = 4;
pub(super) const ALL_NS: u8 = TYPES | VALUES | MACROS;

pub(super) fn written_ns(defs: &PerNs) -> u8 {
    defs.types.is_some() as u8 * TYPES
        | defs.values.is_some() as u8 * VALUES
        | defs.macros.is_some() as u8 * MACROS
}

/// A part of the state an import's resolution read that this unit can change. A read of a
/// module the unit does not hold waits on nothing: that module's scope is final for the round.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum WaitOn {
    /// A name in the scope of a module of this unit.
    Slot(ModuleId, Name),
    /// What a path's first segment finds besides module scopes: a legacy macro, the extern
    /// prelude, the `macro_use` prelude.
    Env(Name),
}

/// One import waiting on one key, linked to the key's next waiter. 16 bytes.
#[derive(Clone, Copy, Debug)]
struct WaitEdge {
    import: u32,
    /// The attempt of `import` that waits; a later attempt makes the edge stale.
    generation: u32,
    next: u32,
    ns: u8,
}

const _: () = assert!(size_of::<WaitEdge>() == 16);

pub(super) const NONE: u32 = u32::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ImportList {
    Unresolved,
    Indeterminate,
    Resolved,
}

/// Where a pass of `resolve_imports` is. A woken import whose key is at or past the phase's
/// mark runs in this phase, as the loop over every import would have reached it after the
/// write; one before the mark runs in the next pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Phase {
    /// Outside a pass: what is woken runs in the next one.
    Idle,
    /// Retrying unresolved imports, by index, from the given one on.
    Unresolved(u32),
    /// Retrying indeterminate imports, by the order they became so, from the given one on.
    Indeterminate(u32),
}

const PENDING: u8 = 1;
const DEFERRED: u8 = 2;

/// Every import of the unit and what its attempts found: what a component carries from round to
/// round (`A = Global`), or a block's, in its frame. An import is a `u32` index into the
/// columns; it moves between lists by its `list` entry only.
#[derive(Clone, Debug)]
pub(super) struct Imports<A: Allocator + Clone = Global> {
    pub(super) directives: Vec<Directive, A>,
    pub(super) list: Vec<ImportList, A>,
    /// Position in `indeterminate`, once the import was indeterminate.
    pub(super) seq: Vec<u32, A>,
    /// The imports that were indeterminate, in the order they became so, with what each last
    /// resolved to. One resolved since stays where it is; `list` says it is done.
    pub(super) indeterminate: Vec<(u32, PerNs), A>,
    /// Plain imports by the module and the name they bind, in index order: head and tail of a
    /// list linked through `next_named`.
    named: HashMap<(ModuleId, Name), (u32, u32), FixedState, A>,
    next_named: Vec<u32, A>,
}

/// One run's worklist over a unit's imports: made in the run's frame for the imports the unit
/// starts with, grown with each it pushes, dropped when the run ends. Besides the queues it
/// holds what keeps them consistent: each import's attempt count (what makes a wait stale), its
/// queued bits, the phase, and the edges' free list.
pub(super) struct ImportQueues<'f> {
    /// Bumped at each attempt to resolve the import.
    generation: FrameVec<'f, u32>,
    /// `PENDING` and `DEFERRED` bits.
    queued: FrameVec<'f, u8>,
    pub(super) phase: Phase,
    pending_unresolved: BinaryHeap<Reverse<u32>, &'f Frame>,
    deferred_unresolved: FrameVec<'f, u32>,
    pending_indeterminate: BinaryHeap<Reverse<u32>, &'f Frame>,
    deferred_indeterminate: FrameVec<'f, u32>,
    /// Head of each key's list of waiting imports in `edges`.
    waits: FrameMap<'f, WaitOn, u32>,
    edges: FrameVec<'f, WaitEdge>,
    /// Head of the edges unlinked from every key, reused before `edges` grows.
    free_edge: u32,
    /// The imports, with the attempt, that read something without a key (a module's children,
    /// all its declarations): any write wakes them.
    any_waiters: FrameVec<'f, (u32, u32)>,
    /// Scratch: the keys of the attempt being registered.
    pub(super) keys: FrameVec<'f, (WaitOn, u8)>,
    /// Scratch: whether the attempt being registered read something without a key.
    pub(super) keys_any: bool,
    /// Scratch: the imports a write woke.
    woken: FrameVec<'f, u32>,
}

impl<A: Allocator + Clone> Imports<A> {
    pub(super) fn new_in(alloc: A) -> Self {
        // clone: the allocator, a `&Frame` or `Global`: no allocation.
        let a = || alloc.clone();
        Imports {
            directives: Vec::new_in(a()),
            list: Vec::new_in(a()),
            seq: Vec::new_in(a()),
            indeterminate: Vec::new_in(a()),
            named: HashMap::with_hasher_in(FixedState, a()),
            next_named: Vec::new_in(a()),
        }
    }

    pub(super) fn len(&self) -> u32 {
        self.directives.len() as u32
    }

    /// Adds `op`, unresolved, and queues it in `queues`.
    pub(super) fn push(&mut self, queues: &mut ImportQueues<'_>, op: ImportOp) {
        let at = self.directives.len() as u32;
        if op.source.kind == ImportKind::Plain
            && let Some(name) = op.bound_name()
        {
            match self.named.entry((op.module, name)) {
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
        self.next_named.push(NONE);
        self.directives.push(Directive { op, status: PartialResolvedImport::Unresolved });
        self.list.push(ImportList::Unresolved);
        self.seq.push(NONE);
        queues.generation.push(0);
        queues.queued.push(0);
        queues.wake(self, at);
    }

    /// The imports still unresolved, in order.
    pub(super) fn unresolved(&self) -> impl Iterator<Item = &Directive> + '_ {
        self.directives
            .iter()
            .zip(&self.list)
            .filter(|(_, list)| **list == ImportList::Unresolved)
            .map(|(directive, _)| directive)
    }

    /// The unresolved plain imports of `module` that bind `name`, in order.
    pub(super) fn unresolved_named(
        &self,
        module: ModuleId,
        name: Name,
    ) -> impl Iterator<Item = &Directive> + '_ {
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
}

impl<'f> ImportQueues<'f> {
    /// An empty worklist over the `len` imports a unit starts with, none queued.
    pub(super) fn new_in(frame: &'f Frame, len: u32) -> Self {
        let mut generation = Vec::with_capacity_in(len as usize, frame);
        generation.resize(len as usize, 0);
        let mut queued = Vec::with_capacity_in(len as usize, frame);
        queued.resize(len as usize, 0);
        ImportQueues {
            generation,
            queued,
            phase: Phase::Idle,
            pending_unresolved: BinaryHeap::new_in(frame),
            deferred_unresolved: Vec::new_in(frame),
            pending_indeterminate: BinaryHeap::new_in(frame),
            deferred_indeterminate: Vec::new_in(frame),
            waits: map_in(frame),
            edges: Vec::new_in(frame),
            free_edge: NONE,
            any_waiters: Vec::new_in(frame),
            keys: Vec::new_in(frame),
            keys_any: false,
            woken: Vec::new_in(frame),
        }
    }

    /// Queues import `at` of `imports` to be retried: in this pass if its phase has not reached
    /// it yet, else in the next.
    pub(super) fn wake<A: Allocator + Clone>(&mut self, imports: &Imports<A>, at: u32) {
        let i = at as usize;
        let unresolved = match imports.list[i] {
            ImportList::Resolved => return,
            ImportList::Unresolved => true,
            ImportList::Indeterminate => false,
        };
        let key = if unresolved { at } else { imports.seq[i] };
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

    pub(super) fn begin_pass<A: Allocator + Clone>(&mut self, imports: &Imports<A>) {
        while let Some(at) = self.deferred_unresolved.pop() {
            let i = at as usize;
            self.queued[i] &= !DEFERRED;
            if imports.list[i] == ImportList::Unresolved && self.queued[i] & PENDING == 0 {
                self.queued[i] |= PENDING;
                self.pending_unresolved.push(Reverse(at));
            }
        }
        while let Some(seq) = self.deferred_indeterminate.pop() {
            let i = imports.indeterminate[seq as usize].0 as usize;
            self.queued[i] &= !DEFERRED;
            if imports.list[i] == ImportList::Indeterminate && self.queued[i] & PENDING == 0 {
                self.queued[i] |= PENDING;
                self.pending_indeterminate.push(Reverse(seq));
            }
        }
        self.phase = Phase::Unresolved(0);
    }

    /// The next unresolved import of this pass, by index.
    pub(super) fn next_unresolved<A: Allocator + Clone>(&mut self, imports: &Imports<A>) -> Option<u32> {
        while let Some(Reverse(at)) = self.pending_unresolved.pop() {
            let i = at as usize;
            self.queued[i] &= !PENDING;
            self.phase = Phase::Unresolved(at + 1);
            if imports.list[i] == ImportList::Unresolved {
                return Some(at);
            }
        }
        None
    }

    /// The next indeterminate import of this pass, by the order they became so.
    pub(super) fn next_indeterminate<A: Allocator + Clone>(&mut self, imports: &Imports<A>) -> Option<u32> {
        while let Some(Reverse(seq)) = self.pending_indeterminate.pop() {
            let i = imports.indeterminate[seq as usize].0 as usize;
            self.queued[i] &= !PENDING;
            self.phase = Phase::Indeterminate(seq + 1);
            if imports.list[i] == ImportList::Indeterminate {
                return Some(seq);
            }
        }
        None
    }

    /// Starts a new attempt of import `at`: what its last one waited on no longer wakes it.
    pub(super) fn attempt(&mut self, at: u32) {
        let generation = &mut self.generation[at as usize];
        *generation = generation.wrapping_add(1);
    }

    /// Makes import `at`'s current attempt wait on `self.keys` (and on any write if
    /// `self.keys_any`), and clears them.
    pub(super) fn wait(&mut self, at: u32) {
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
    fn wake_on<A: Allocator + Clone>(&mut self, imports: &Imports<A>, key: &WaitOn, ns: u8) {
        let ImportQueues { waits, edges, generation, free_edge, woken, .. } = self;
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
            self.wake(imports, at);
        }
    }

    /// Wakes the imports whose attempt waits on any write, and drops every waiter: a woken one
    /// waits again at its next attempt. Waking queues an import and adds no waiter.
    fn wake_any<A: Allocator + Clone>(&mut self, imports: &Imports<A>) {
        for k in 0..self.any_waiters.len() {
            let (at, generation) = self.any_waiters[k];
            if self.generation[at as usize] == generation {
                self.wake(imports, at);
            }
        }
        self.any_waiters.clear();
    }

    /// A write of the namespaces `ns` of `name` in the scope of `module`.
    pub(super) fn wake_slot<A: Allocator + Clone>(
        &mut self,
        imports: &Imports<A>,
        module: ModuleId,
        name: Name,
        ns: u8,
    ) {
        if ns == 0 {
            return;
        }
        if !self.any_waiters.is_empty() {
            self.wake_any(imports);
        }
        if !self.waits.is_empty() {
            self.wake_on(imports, &WaitOn::Slot(module, name), ns);
        }
    }

    /// A new legacy macro, extern prelude or `macro_use` prelude entry named `name`.
    pub(super) fn wake_env<A: Allocator + Clone>(&mut self, imports: &Imports<A>, name: Name, ns: u8) {
        if !self.any_waiters.is_empty() {
            self.wake_any(imports);
        }
        if !self.waits.is_empty() {
            self.wake_on(imports, &WaitOn::Env(name), ns);
        }
    }

    /// A change every path may have read: the std prelude module, a module's children, or,
    /// when a unit carries on into a new round, any module of another component.
    pub(super) fn wake_all<A: Allocator + Clone>(&mut self, imports: &Imports<A>) {
        for at in 0..imports.len() {
            self.wake(imports, at);
        }
    }
}

/// One importer of a source, linked to the source's next importer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ImporterEdge<K, T> {
    pub(super) importer: K,
    pub(super) data: T,
    pub(super) next: u32,
}

/// The importers of each source, each list in the order its importers came: one array of edges,
/// linked per source. An edge is appended without moving the others, so a list is read in place
/// while its importers are visited.
#[derive(Clone, Debug)]
pub(super) struct ImporterLists<K, T, A: Allocator + Clone = Global> {
    edges: Vec<ImporterEdge<K, T>, A>,
    /// Head and tail of each source's list.
    heads: HashMap<K, (u32, u32), FixedState, A>,
    /// The edge of each `(source, importer)`.
    by_pair: HashMap<(K, K), u32, FixedState, A>,
}

impl<K, T, A: Allocator + Clone> ImporterLists<K, T, A> {
    pub(super) fn new_in(alloc: A) -> Self {
        // clone: the allocator, a `&Frame` or `Global`: no allocation.
        let a = || alloc.clone();
        ImporterLists {
            edges: Vec::new_in(a()),
            heads: HashMap::with_hasher_in(FixedState, a()),
            by_pair: HashMap::with_hasher_in(FixedState, a()),
        }
    }
}

impl<K: Copy + Eq + std::hash::Hash, T: Copy, A: Allocator + Clone> ImporterLists<K, T, A> {
    /// Adds `importer` to the importers of `source`, or, if it is one already, lets `merge`
    /// update the data it has.
    pub(super) fn add(&mut self, source: K, importer: K, data: T, merge: impl FnOnce(&mut T)) {
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
    pub(super) fn first(&self, source: K) -> u32 {
        self.heads.get(&source).map_or(NONE, |&(head, _)| head)
    }

    pub(super) fn edge(&self, at: u32) -> ImporterEdge<K, T> {
        self.edges[at as usize]
    }
}

/// The glob imports into the unit's modules: for each source module, the modules that
/// glob-import it, with the visibility and the glob. A source may be a module of another
/// component, whose scope the next round pulls again.
pub(super) type GlobEdges<A = Global> = ImporterLists<ModuleId, (Visibility, GlobId), A>;
