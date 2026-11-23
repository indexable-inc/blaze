//! [`Resolver`]: one module's name resolution as a sans-IO state machine, and the loop that
//! runs one machine or the members of a group.
//!
//! The machine holds what the module declares (its own text and each expansion as a
//! *fragment*), the state of each `use`, glob and macro call, and the answers it was given.
//! A step runs passes until nothing moves (each pass plans every update from the state as it
//! is, then applies them all), and then returns what it needs: reads, expansions, or the
//! modules of a cycle. When nothing moves while items are pending, the stall rule fails the
//! sinks of the wait graph (bottom.md section 6), and at last re-resolves everything once
//! against the final state: that pass checks each early resolution with rustc's ambiguity kinds and is what the
//! read list is made of.
//!
//! Order independence. An item still pending is *blocked* while what it waits on is an answer
//! not given yet, and a lookup that meets a blocked item waits too, where it would otherwise
//! take the item as undetermined and go on to an outer scope (rustc's weak undeterminacy).
//! So no decision is made on a guess that one more answer would change: a step's result is a
//! function of the records, not of the order or the batches the answers came in. Blocked
//! flags are set conservatively after every move and lowered to their least fixpoint only
//! when no answer is outstanding (`relax`), which is when "undetermined" means what rustc
//! means by it.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use core::cmp::Ordering;

use blaze_hash::HashMap;
use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_own::{AVec, Alloc, Vec};
use blaze_surface::{
    AmbiguityKind, BindingState, CallShape, DeclsBuilder, Def, DefKind, Dep, Edition, ExternTarget,
    GlobEdge, MacroShapes, Namespace, Origin, Path, PathKind, Rename, TextualDef, Unresolved,
    UseForm, UseSite, Visibility, WrittenVis,
};

use crate::error::{GlobBug, ResolveError};
use crate::facts::{Name, ScopeFacts};
use crate::finish::Finish;
use crate::read::{Answer, ExpandRequest, NameAnswer, Read, Reply, Step};
use crate::scope::{ReadEntry, Resolved, Saw};
use blaze_surface::Asker;
use crate::work::{Key, Work};

/// The most parents a module may have (rule 18); no real crate comes near it.
pub const MAX_MODULE_DEPTH: usize = 128;

/// rustc's default `recursion_limit`: a macro call written at this expansion depth is an error.
pub const RECURSION_LIMIT: u16 = 128;

/// The most macro expansions one module may take in (rule 18).
pub const MAX_EXPANSIONS: usize = 1 << 16;

/// A place in a module's text: a fragment and a slot of it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct At {
    pub(crate) frag: u32,
    pub(crate) slot: u32,
}

/// One ordered entry of a fragment: what textual order matters for.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Slot<N> {
    /// A `macro_rules!`; `export` for a `#[macro_export]` one, whose textual binding is public.
    Rules { name: N, def: ItemKey, export: bool, shapes: MacroShapes },
    Call(u32),
    Mod { child: ItemKey, macro_use: bool },
    Use(u32),
    Glob(u32),
}

/// A non-glob declaration: an item, a `mod`, an `extern crate`, a macro export.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct DefEntry {
    pub(crate) def: Def,
    pub(crate) vis: Visibility,
    pub(crate) origin: Origin,
    pub(crate) expansion: u16,
    pub(crate) frag: u32,
    /// Its presence condition: a `blaze-cond` decision-diagram id, 0 for always. Reserved:
    /// cfg is evaluated under one configuration today, so every entry is 0.
    pub(crate) cond: u32,
}

/// The module's own text, or one call's expansion.
pub(crate) struct Fragment<'a, N> {
    pub(crate) expansion: u16,
    /// The call it is the expansion of, for a lookup that runs off the fragment's start.
    pub(crate) parent: Option<At>,
    pub(crate) slots: AVec<'a, Slot<N>>,
    /// Its non-glob declarations, in the order written.
    pub(crate) items: AVec<'a, (Namespace, N, DefEntry)>,
    /// Its `extern crate`s of no dependency.
    pub(crate) bad_extern: AVec<'a, N>,
    /// Its `pub(in path)`s whose path names no ancestor.
    pub(crate) bad_vis: AVec<'a, Path<'a, N>>,
    /// Its declarations' `pub(in path)`s whose path is not the names of an ancestor as written
    /// (`pub(in crate::alias)`): private while the module resolves, and resolved as paths in
    /// the final pass, which writes the binding with the module the path names.
    pub(crate) deferred_vis: AVec<'a, (Path<'a, N>, Namespace, N, Def)>,
    /// Its `#[macro_export]` `macro_rules!`, in an expansion only.
    pub(crate) exports: AVec<'a, TextualDef<N>>,
    /// It or a fragment expanded inside it holds a `macro_rules!` or a `#[macro_use]` module:
    /// set when such a fragment is added, never cleared (fragments are only added).
    pub(crate) textual: bool,
    /// Calls not expanded in it and in the fragments expanded inside it (kept by
    /// `add_fragment` and `set_call_state`). With `textual` false and this zero, a walk of the
    /// textual scope can find nothing in it and wait on nothing in it: it is passed over
    /// (`textual_back`), so a module of N expanded calls costs a lookup N steps, not the sum
    /// of every expansion's slots.
    pub(crate) open: u32,
}

/// Which scope a hit was found in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Layer {
    Textual,
    Module,
    ExternPrelude,
    MacroUsePrelude,
    StdPrelude,
    /// The builtin types, last of the type namespace.
    Builtin,
    /// Another module's scope, through a path.
    Path,
}

/// A binding a lookup found.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Hit {
    pub(crate) def: Def,
    pub(crate) vis: Visibility,
    pub(crate) origin: Origin,
    pub(crate) expansion: u16,
    /// The fragment of this machine it was written in; `None` for a binding of another
    /// module or a prelude (which rustc's expansion order puts at the root).
    pub(crate) frag: Option<u32>,
    pub(crate) state: BindingState,
    pub(crate) layer: Layer,
}

impl Hit {
    /// The visibility a single `use` declared `import` gives this binding: no wider than the
    /// binding itself (E0364/E0365), except a binding of an `extern crate` item, which a `use`
    /// re-exports at the `use`'s own visibility: every root's injected `extern crate core;` is
    /// private, yet `pub use core as core_;` (the `uint` crate) is a public re-export (rustc's
    /// `Resolver::import`, `pub_use_of_private_extern_crate_hack`: a lint at most).
    pub(crate) fn through_import(&self, import: Visibility, asker: Asker<'_>) -> Visibility {
        if self.origin == Origin::ExternCrate { import } else { blaze_surface::SeenFrom::narrower(import, self.vis, asker) }
    }

    /// A module named by a path's start (`crate`, `self`, `super`, `$crate`).
    pub(crate) fn module(key: ItemKey) -> Hit {
        Hit {
            def: Def::module(key),
            vis: Visibility::Public,
            origin: Origin::Declared,
            expansion: 0,
            frag: None,
            state: BindingState::Determined,
            layer: Layer::Path,
        }
    }
}

impl DefEntry {
    pub(crate) fn hit(&self) -> Hit {
        Hit {
            def: self.def,
            vis: self.vis,
            origin: self.origin,
            expansion: self.expansion,
            frag: Some(self.frag),
            state: BindingState::Determined,
            layer: Layer::Module,
        }
    }
}

/// A lookup's outcome.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Res {
    Found(Hit),
    /// Determined: no binding.
    None,
    /// Not determined. `weak`: only a glob or a macro expansion can still define it, so an
    /// early lexical lookup goes on to outer scopes (rustc's `Weak::Yes`).
    Undet { weak: bool },
    /// An answer not given yet decides it.
    Wait,
}

/// One namespace of a named import.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum NsRes {
    Pending,
    Absent,
    /// Found, and the importing module may not name it.
    Private(Hit),
    Found(Hit),
}

pub(crate) struct Import<'a, N> {
    pub(crate) site: UseSite,
    pub(crate) path: Path<'a, N>,
    /// What it binds; `None` for `as _`.
    pub(crate) bound: Option<N>,
    pub(crate) types_only: bool,
    pub(crate) vis: Visibility,
    pub(crate) expansion: u16,
    pub(crate) at: At,
    pub(crate) ns: [NsRes; 3],
    pub(crate) failed: Option<(u32, Unresolved)>,
    /// What its first segment resolved to, early.
    pub(crate) first: Option<Def>,
    pub(crate) blocked: bool,
    /// Decided reading a binding through globs: tried again when what it read changes, and
    /// failed as ambiguous if a later glob made that binding ambiguous (the stall rule).
    pub(crate) via_glob: bool,
    /// Its presence condition, as [`DefEntry::cond`]: 0 today.
    pub(crate) cond: u32,
}

impl<N> Import<'_, N> {
    /// Whether it imports namespace `ns`.
    pub(crate) fn imports(&self, ns: Namespace) -> bool {
        !self.types_only || ns == Namespace::Types
    }

    pub(crate) fn done(&self) -> bool {
        self.failed.is_some()
            || Namespace::ALL
                .iter()
                .all(|&ns| !self.imports(ns) || self.ns[ns.index()] != NsRes::Pending)
    }
}

/// A glob's source.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Source {
    Pending,
    /// A module or an enum; an enum's variants are as visible as the enum (`clamp`).
    Resolved { def: Def, clamp: Option<Visibility> },
    /// `use self::*`: imports nothing new.
    Itself,
    Failed(u32, Unresolved),
}

pub(crate) struct Glob<'a, N> {
    pub(crate) site: UseSite,
    pub(crate) path: Path<'a, N>,
    pub(crate) vis: Visibility,
    pub(crate) expansion: u16,
    pub(crate) at: At,
    pub(crate) source: Source,
    pub(crate) first: Option<Def>,
    pub(crate) blocked: bool,
    /// As [`Import::via_glob`].
    pub(crate) via_glob: bool,
    /// Its presence condition, as [`DefEntry::cond`]: 0 today.
    pub(crate) cond: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CallState {
    Pending,
    /// Its path is resolved and its expansion asked for.
    Resolved(Def),
    Expanded { frag: u32, def: Def },
    /// A tool attribute: nothing to expand.
    Inert,
    Failed(u32, Unresolved),
    /// Written at the recursion limit.
    TooDeep,
}

impl CallState {
    /// Whether the call may still add names.
    pub(crate) fn unexpanded(self) -> bool {
        matches!(self, CallState::Pending | CallState::Resolved(_))
    }
}

pub(crate) struct Call<'a, N> {
    pub(crate) key: ItemKey,
    pub(crate) path: Path<'a, N>,
    pub(crate) kind: CallShape,
    pub(crate) at: At,
    /// The item it is on, in its fragment (an attribute or a derive).
    pub(crate) item: Option<u32>,
    pub(crate) state: CallState,
    pub(crate) first: Option<Def>,
    pub(crate) blocked: bool,
}

/// An answer as kept.
pub(crate) enum Cached<'a, N> {
    Name(NameAnswer),
    Globs {
        edges: AVec<'a, GlobEdge>,
        krate: CrateItemKey,
        chain: AVec<'a, ItemKey>,
        digest: ContentId,
    },
    Textual { defs: AVec<'a, TextualDef<N>>, digest: ContentId },
    Helpers { names: AVec<'a, N>, digest: ContentId },
    /// Answered in progress, waiting on the module named.
    InProgress(ItemKey),
}

/// What a pass is for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Scan {
    /// Resolve what can be resolved.
    Early,
    /// A quiet point: each pending item is evaluated again for the pending items it waits on
    /// (its [`Blame`]s), the edges of the stall rule's wait graph.
    Stall,
    /// The last pass: every layer is visited for ambiguity, and the reads used are kept.
    Final,
}

/// An ambiguity an early resolution meets in the final pass.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Amb<N> {
    pub(crate) ns: Namespace,
    pub(crate) name: N,
    pub(crate) kind: AmbiguityKind,
    pub(crate) first: Def,
    pub(crate) second: Def,
}

/// What an undecided answer waits on, by the stall rule's wait relation (bottom.md section 6):
/// a pending named import, a pending glob, a call not expanded (in the textual scope), or every
/// call of a module not expanded yet but the asking one.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) enum Blame {
    Import(ItemKey, u32),
    Glob(ItemKey, u32),
    Call(ItemKey, u32),
    Calls(ItemKey, Option<u32>),
}

/// Where a pass's reads go: those it needs and has no answer to, those answered `InProgress`,
/// and in the final pass those it used with what they saw.
pub(crate) struct Sink<'a, N> {
    pub(crate) scan: Scan,
    pub(crate) need: AVec<'a, Read<N>>,
    pub(crate) waits: AVec<'a, Read<N>>,
    pub(crate) used: AVec<'a, ReadEntry<N>>,
    pub(crate) amb: Option<Amb<N>>,
    /// The member state and the answers the item being planned read (early passes only),
    /// fellows' facts met walking globs among them.
    pub(crate) touched: AVec<'a, Key<N>>,
    /// A lookup through globs broke an invariant (the first one): the pass stops with a typed
    /// error naming it.
    pub(crate) overrun: Option<GlobBug>,
    /// The earliest glob of the module being finished that left a name undetermined.
    pub(crate) undet_glob: Option<(ItemKey, At, u32)>,
    /// What the item being evaluated waits on ([`Scan::Stall`] only): a lookup that decides
    /// drops what it recorded.
    pub(crate) blame: AVec<'a, Blame>,
    /// The item being evaluated read a binding found through globs.
    pub(crate) via_glob: bool,
    /// The work this sink saw.
    pub(crate) stats: FillStats,
    /// The planted bug of the read-list control: a read that found nothing is not kept.
    #[cfg(test)]
    pub(crate) forget_negative: bool,
    /// The planted bug of the glob-probe control: a read made walking a glob is not kept.
    #[cfg(test)]
    pub(crate) forget_probes: bool,
    /// A glob of the module is being walked.
    #[cfg(test)]
    pub(crate) probing: bool,
}

impl<'a, N: Name> Sink<'a, N> {
    pub(crate) fn new_in(alloc: Alloc<'a>, scan: Scan) -> Sink<'a, N> {
        Sink {
            scan,
            need: Vec::new_in(alloc),
            waits: Vec::new_in(alloc),
            used: Vec::new_in(alloc),
            amb: None,
            touched: Vec::new_in(alloc),
            overrun: None,
            undet_glob: None,
            blame: Vec::new_in(alloc),
            via_glob: false,
            stats: FillStats::default(),
            #[cfg(test)]
            forget_negative: false,
            #[cfg(test)]
            forget_probes: false,
            #[cfg(test)]
            probing: false,
        }
    }

    /// Records a read of member state, or of an answer, for the item being planned.
    pub(crate) fn touch(&mut self, key: Key<N>) {
        if self.scan == Scan::Early {
            self.touched.push(key);
        }
    }

    /// Records what the answer being made waits on, in a [`Scan::Stall`] evaluation.
    pub(crate) fn blame(&mut self, b: Blame) {
        if self.scan == Scan::Stall {
            self.blame.push(b);
        }
    }

    /// Starts the record of one item's plan.
    pub(crate) fn begin(&mut self) -> Mark {
        Mark { touched: self.touched.len(), need: self.need.len(), waits: self.waits.len() }
    }

    /// The typed error of a lookup through globs that broke an invariant, if one did.
    pub(crate) fn check(&self, module: ItemKey) -> Result<(), ResolveError> {
        match self.overrun {
            Some(why) => Err(ResolveError::GlobLookup { module, why }),
            None => Ok(()),
        }
    }

    /// Stops the pass on `why`, keeping the first invariant that broke.
    pub(crate) fn broke(&mut self, why: GlobBug) {
        if self.overrun.is_none() {
            self.overrun = Some(why);
        }
    }
}

/// Where one item's record starts in a sink.
#[derive(Clone, Copy)]
pub(crate) struct Mark {
    pub(crate) touched: usize,
    pub(crate) need: usize,
    pub(crate) waits: usize,
}

/// The machines a lookup reads live: the members of the group being solved, sorted by module
/// (a machine alone is a group of one).
pub(crate) struct Env<'e, 'a, N> {
    pub(crate) members: &'e [Resolver<'a, N>],
}

impl<N> Clone for Env<'_, '_, N> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<N> Copy for Env<'_, '_, N> {}

impl<'e, 'a, N> Env<'e, 'a, N> {
    pub(crate) fn fellow(&self, module: ItemKey) -> Option<&'e Resolver<'a, N>> {
        self.index(module).map(|at| &self.members[at])
    }

    pub(crate) fn index(&self, module: ItemKey) -> Option<usize> {
        self.members.binary_search_by(|m| m.module.cmp(&module)).ok()
    }
}

/// One lookup's context.
#[derive(Clone, Copy)]
pub(crate) struct Query<'q> {
    pub(crate) asker: Asker<'q>,
    /// rustc's restricted shadowing: an early lexical lookup takes a glob binding at once.
    pub(crate) restricted: bool,
    /// The import or glob being resolved, which may not justify itself (rustc's
    /// `ignore_import`).
    pub(crate) ignore: Option<(ItemKey, crate::lookup::Asking)>,
}

/// A pending item, for its blocked flag.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub(crate) enum Item {
    Import(u32),
    Glob(u32),
    Call(u32),
}

/// What resolving an import found, before it is merged into its state.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ImportOutcome {
    pub(crate) ns: [NsRes; 3],
    pub(crate) failed: Option<(u32, Unresolved)>,
    pub(crate) first: Option<Def>,
    pub(crate) wait: bool,
}

/// What a step hands its caller, kept so the step can lend it.
pub(crate) struct Outbox<'a, N> {
    pub(crate) asks: AVec<'a, Read<N>>,
    pub(crate) expand: AVec<'a, ExpandRequest>,
    pub(crate) cycle: AVec<'a, ItemKey>,
    /// The work of every step so far.
    pub(crate) stats: FillStats,
    /// The worklist's state between steps, made at the first.
    pub(crate) work: Option<Work<'a, N>>,
}

impl<'a, N> Outbox<'a, N> {
    pub(crate) fn new_in(alloc: Alloc<'a>) -> Outbox<'a, N> {
        Outbox {
            asks: Vec::new_in(alloc),
            expand: Vec::new_in(alloc),
            cycle: Vec::new_in(alloc),
            stats: FillStats::default(),
            work: None,
        }
    }
}

/// How much work a machine or group did: counts, for a profile to say where a fill's time
/// went without a clock in a sans-IO core.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct FillStats {
    /// Early and forced passes over every member.
    pub passes: u64,
    /// Rounds of `relax`, each a plan of every member.
    pub relax_rounds: u64,
    /// Pending items evaluated.
    pub plans: u64,
    /// `lookup_module` calls.
    pub lookups: u64,
    /// Final passes (one per member).
    pub finals: u64,
    /// Changes to member state or answers the worklist saw.
    pub events: u64,
    /// Items those changes woke.
    pub wakes: u64,
    /// Walk steps into a member of the group (`member_node`).
    pub member_nodes: u64,
    /// Bytes of glob edges those steps borrowed (before 2026-11-16 each step leaked a copy).
    pub member_edge_bytes: u64,
    /// Evaluations of a decided item that read through globs (its answer checked again).
    pub agains: u64,
    /// Decided via-glob items a relax round passed over (each was re-solved per round before).
    pub agains_spared: u64,
    /// Keys the evaluations read (before duplicates are dropped), and the parks they made.
    pub touches: u64,
    pub parks: u64,
    /// Items a relax round queued (every pending item of every member, per round).
    pub relax_queued: u64,
    /// Stall passes, the undecided items their wait graphs held, and those graphs' edges.
    pub stalls: u64,
    pub stall_items: u64,
    pub stall_edges: u64,
    /// Of `plans`, those of imports and of globs (the rest are calls).
    pub plan_imports: u64,
    pub plan_globs: u64,
}

impl FillStats {
    /// Adds `other` in.
    pub fn add(&mut self, other: &FillStats) {
        self.touches += other.touches;
        self.parks += other.parks;
        self.relax_queued += other.relax_queued;
        self.stalls += other.stalls;
        self.stall_items += other.stall_items;
        self.stall_edges += other.stall_edges;
        self.plan_imports += other.plan_imports;
        self.plan_globs += other.plan_globs;
        self.member_nodes += other.member_nodes;
        self.member_edge_bytes += other.member_edge_bytes;
        self.agains += other.agains;
        self.agains_spared += other.agains_spared;
        self.events += other.events;
        self.wakes += other.wakes;
        self.passes += other.passes;
        self.relax_rounds += other.relax_rounds;
        self.plans += other.plans;
        self.lookups += other.lookups;
        self.finals += other.finals;
    }
}

/// What [`run`] stopped on.
pub(crate) enum Ran<'a, N> {
    Ask,
    Expand,
    Cycle,
    Done(AVec<'a, Resolved<'a, N>>),
}

/// The planted bugs the tests' controls switch on.
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plant {
    None,
    /// The read list leaves out the reads that found nothing.
    ForgetNegativeReads,
    /// The read list leaves out the reads made walking a glob of the module.
    ForgetGlobProbes,
    /// The worklist drops every event on a name: an item parked on it is never woken.
    DropWake,
    /// The worklist wakes every pending item on every event: the reference its parked lists
    /// must equal.
    WakeAll,
    /// The worklist takes its queue in an order drawn from this seed.
    Shuffle(u64),
    /// A group's members keep the read lists of the group's final pass, which read their
    /// fellows live: no read of a fellow is in them.
    FellowReadsLeftOut,
    /// Read by the tests' driver: resolving again keeps every record whose reads hold, the
    /// ones a rerun's derivation goes around too.
    KeepSuspects,
    /// A name read through a glob from another module's record passes whatever its visibility:
    /// a glob brings in private items too (the cold-fill oracle's control, `tests/oracle.rs`).
    GlobLeaksPrivate,
}

/// One module's name resolution (the protocol is in the crate's documentation).
pub struct Resolver<'a, N> {
    pub(crate) module: ItemKey,
    pub(crate) krate: CrateItemKey,
    pub(crate) root: ItemKey,
    pub(crate) edition: Edition,
    /// The module, its parent, ..., the crate root.
    pub(crate) chain: AVec<'a, ItemKey>,
    /// The name of each of `chain` in its parent.
    pub(crate) chain_names: AVec<'a, Option<N>>,
    /// No `#[no_implicit_prelude]` on the module or above it.
    pub(crate) implicit_prelude: bool,
    pub(crate) extern_prelude: AVec<'a, (N, ItemKey)>,
    pub(crate) deps: AVec<'a, Dep<N>>,
    pub(crate) macro_use: AVec<'a, ItemKey>,
    pub(crate) prelude: Option<ItemKey>,
    /// The crate root's `#![blaze::arbitrary_ints]`: `u24`, `i80` are builtin types here.
    pub(crate) arbitrary_ints: bool,
    pub(crate) tools: AVec<'a, N>,
    /// Sorted.
    pub(crate) builtin_attrs: AVec<'a, N>,
    pub(crate) frags: AVec<'a, Fragment<'a, N>>,
    /// The non-glob declarations by namespace and name, the first in the module's tree kept.
    pub(crate) defs: HashMap<(Namespace, N), DefEntry, Alloc<'a>>,
    pub(crate) imports: AVec<'a, Import<'a, N>>,
    /// The imports by the name they bind.
    pub(crate) named: HashMap<N, AVec<'a, u32>, Alloc<'a>>,
    pub(crate) globs: AVec<'a, Glob<'a, N>>,
    /// The module's glob edges as `glob_edges` computes them from `globs`, kept current by
    /// [`Resolver::set_glob_source`] (the one writer of a glob's source once it is pushed), so a
    /// walk borrows them instead of computing and keeping a copy per lookup.
    edges: AVec<'a, GlobEdge>,
    pub(crate) calls: AVec<'a, Call<'a, N>>,
    /// Calls that may still add names (pending or asked to be expanded).
    pub(crate) unexpanded: u32,
    /// Pending calls whose blocked flag is set.
    pub(crate) blocked_calls: u32,
    pub(crate) expansions: usize,
    pub(crate) answers: HashMap<Read<N>, Cached<'a, N>, Alloc<'a>>,
    pub(crate) out: Outbox<'a, N>,
    pub(crate) finished: bool,
    pub(crate) alloc: Alloc<'a>,
    #[cfg(test)]
    pub(crate) plant: Plant,
}

fn count(n: usize, module: ItemKey) -> Result<u32, ResolveError> {
    u32::try_from(n).map_err(|_| ResolveError::TooManyExpansions { module })
}

impl<'a, N: Name> Resolver<'a, N> {
    /// A machine for `module`, made from its records in `facts`, allocating in `alloc`.
    pub fn new_in<F: ScopeFacts<Name = N>>(
        module: ItemKey,
        facts: &F,
        alloc: Alloc<'a>,
    ) -> Result<Resolver<'a, N>, ResolveError> {
        let surface = |error| ResolveError::Surface { module, error };
        let info = facts.module(module).map_err(surface)?;
        let mut chain = Vec::new_in(alloc);
        let mut chain_names = Vec::new_in(alloc);
        chain.push(module);
        chain_names.push(info.name);
        let mut implicit_prelude = !info.no_implicit_prelude;
        let mut parent = info.parent;
        while let Some(p) = parent {
            if chain.len() >= MAX_MODULE_DEPTH {
                return Err(ResolveError::ModuleTooDeep { module });
            }
            let up = facts.module(p).map_err(surface)?;
            chain.push(p);
            chain_names.push(up.name);
            implicit_prelude &= !up.no_implicit_prelude;
            parent = up.parent;
        }
        let root = chain[chain.len() - 1];
        let krate = info.krate;
        let cr = facts.crate_root(krate).map_err(surface)?;
        let mut extern_prelude = Vec::from_slice_in(&cr.extern_prelude, alloc);
        // By name, then key: a total order, so a working form made by hand (which may hold a
        // name twice, where a record cannot) still gives one result.
        extern_prelude.sort_unstable();
        let deps = Vec::from_slice_in(&cr.deps, alloc);
        let macro_use = Vec::from_slice_in(&cr.macro_use, alloc);
        let tools = Vec::from_slice_in(&cr.tools, alloc);
        let mut builtin_attrs = Vec::from_slice_in(&cr.builtin_attrs, alloc);
        builtin_attrs.sort_unstable();
        let mut r = Resolver {
            module,
            krate,
            root,
            edition: cr.head.edition,
            chain,
            chain_names,
            implicit_prelude,
            extern_prelude,
            deps,
            macro_use,
            prelude: cr.head.prelude,
            arbitrary_ints: cr.head.arbitrary_ints,
            tools,
            builtin_attrs,
            frags: Vec::new_in(alloc),
            defs: HashMap::new_in(alloc),
            imports: Vec::new_in(alloc),
            named: HashMap::new_in(alloc),
            globs: Vec::new_in(alloc),
            edges: Vec::new_in(alloc),
            calls: Vec::new_in(alloc),
            unexpanded: 0,
            blocked_calls: 0,
            expansions: 0,
            answers: HashMap::new_in(alloc),
            out: Outbox::new_in(alloc),
            finished: false,
            alloc,
            #[cfg(test)]
            plant: Plant::None,
        };
        let decls = facts.decls(module).map_err(surface)?;
        r.add_fragment(&decls, None, 0, None)?;
        if module == root {
            // `#[macro_export]` macros are bindings of the crate root (rustc's `MacroExport`).
            for &(name, def, shapes) in cr.macro_exports.iter() {
                let entry = DefEntry {
                    def: Def { key: def, kind: DefKind::Macro(shapes) },
                    vis: Visibility::Public,
                    origin: Origin::MacroExport,
                    expansion: 0,
                    frag: 0,
                    cond: 0,
                };
                r.frags[0].items.push((Namespace::Macros, name, entry));
                r.insert_def(Namespace::Macros, name, entry);
            }
        }
        Ok(r)
    }

    /// The module this machine resolves.
    pub fn module(&self) -> ItemKey {
        self.module
    }

    pub(crate) fn asker(&self) -> Asker<'_> {
        Asker { krate: self.krate, chain: &self.chain }
    }

    /// A visibility restricted to `module`, in its one form.
    pub(crate) fn restricted(&self, module: ItemKey) -> Visibility {
        if module == self.root { Visibility::Crate(self.krate) } else { Visibility::Restricted(module) }
    }

    /// A written visibility resolved against the module's own chain; a `pub(in path)` naming
    /// no ancestor, and a `pub(path)` written without `in` (which rustc refuses, E0704), go to
    /// `bad` and are private. `pub(self)` is private.
    fn resolve_vis(
        &self,
        vis: &WrittenVis<Path<'_, N>>,
        bad: &mut AVec<'a, Path<'a, N>>,
    ) -> Visibility {
        match vis {
            WrittenVis::Private | WrittenVis::SelfOnly => self.restricted(self.module),
            WrittenVis::WithoutIn(path) => {
                bad.push(path.copy_in(self.alloc));
                self.restricted(self.module)
            }
            WrittenVis::Pub => Visibility::Public,
            WrittenVis::Crate => Visibility::Crate(self.krate),
            WrittenVis::Super => match self.chain.get(1) {
                Some(&parent) => self.restricted(parent),
                None => {
                    bad.push(Path {
                        kind: PathKind::Super(1),
                        segments: Vec::new_in(self.alloc),
                    });
                    self.restricted(self.module)
                }
            },
            WrittenVis::In(path) => {
                let top = self.chain.len() - 1;
                let start = match path.kind {
                    PathKind::Crate => Some(top),
                    PathKind::SelfMod => Some(0),
                    PathKind::Super(n) => {
                        let n = usize::from(n);
                        if n <= top { Some(n) } else { None }
                    }
                    PathKind::Plain if self.edition == Edition::E2015 => Some(top),
                    PathKind::Plain | PathKind::Abs | PathKind::DollarCrate(_) => None,
                };
                let mut at = start;
                for seg in path.segments.iter() {
                    at = match at {
                        Some(i) if i > 0 && self.chain_names[i - 1] == Some(*seg) => Some(i - 1),
                        Some(_) | None => None,
                    };
                }
                match at {
                    Some(i) => self.restricted(self.chain[i]),
                    None => {
                        bad.push(path.copy_in(self.alloc));
                        self.restricted(self.module)
                    }
                }
            }
        }
    }

    /// A declaration's visibility. A `pub(in path)` from `crate`, `self` or `super` whose
    /// names are not an ancestor's as written may go through an alias, so it is deferred to
    /// the final pass (private until then) rather than refused.
    fn resolve_decl_vis(
        &self,
        vis: &WrittenVis<Path<'_, N>>,
        ns: Namespace,
        name: N,
        def: Def,
        bad: &mut AVec<'a, Path<'a, N>>,
        deferred: &mut AVec<'a, (Path<'a, N>, Namespace, N, Def)>,
    ) -> Visibility {
        let before = bad.len();
        let resolved = self.resolve_vis(vis, bad);
        if let WrittenVis::In(path) = vis {
            let as_path = match path.kind {
                PathKind::Crate | PathKind::SelfMod | PathKind::Super(_) => true,
                PathKind::Plain => self.edition == Edition::E2015,
                PathKind::Abs | PathKind::DollarCrate(_) => false,
            };
            if as_path && bad.len() > before {
                if let Some(p) = bad.pop() {
                    deferred.push((p, ns, name, def));
                }
            }
        }
        resolved
    }

    /// Adds one fragment (the module's text or a call's expansion), returning its index.
    pub(crate) fn add_fragment(
        &mut self,
        decls: &DeclsBuilder<'_, N>,
        call: Option<ItemKey>,
        expansion: u16,
        parent: Option<At>,
    ) -> Result<u32, ResolveError> {
        let module = self.module;
        let fid = count(self.frags.len(), module)?;
        let a = self.alloc;
        let mut items = Vec::new_in(a);
        let mut bad_extern = Vec::new_in(a);
        let mut bad_vis = Vec::new_in(a);
        let mut deferred_vis = Vec::new_in(a);
        let mut exports = Vec::new_in(a);
        let mut ordered: AVec<'a, (u32, Slot<N>)> = Vec::new_in(a);
        let entry = |def: Def, vis: Visibility, origin: Origin| DefEntry {
            def,
            vis,
            origin,
            expansion,
            frag: fid,
            cond: 0,
        };

        for it in decls.items.iter() {
            let vis = self.resolve_decl_vis(&it.vis, it.ns, it.name, it.def, &mut bad_vis, &mut deferred_vis);
            items.push((it.ns, it.name, entry(it.def, vis, Origin::Declared)));
        }
        for m in decls.mods.iter() {
            let def = Def::module(m.module);
            let vis = self.resolve_decl_vis(&m.vis, Namespace::Types, m.name, def, &mut bad_vis, &mut deferred_vis);
            items.push((Namespace::Types, m.name, entry(def, vis, Origin::Declared)));
            ordered.push((m.pos, Slot::Mod { child: m.module, macro_use: m.macro_use }));
        }
        for ec in decls.extern_crates.iter() {
            let vis = self.resolve_vis(&ec.vis, &mut bad_vis);
            let (target, written) = match ec.target {
                ExternTarget::Dep(name) => {
                    (self.deps.iter().find(|d| d.name == name).map(|d| d.root), Some(name))
                }
                ExternTarget::SelfCrate => (Some(self.root), None),
            };
            let bound = match ec.rename {
                Rename::Same => written,
                Rename::To(name) => Some(name),
                Rename::Underscore => None,
            };
            match (target, bound) {
                (Some(root), Some(name)) => {
                    items.push((Namespace::Types, name, entry(Def::module(root), vis, Origin::ExternCrate)));
                }
                (Some(_), None) => {}
                (None, _) => {
                    if let Some(name) = written {
                        bad_extern.push(name);
                    }
                }
            }
        }
        for (index, u) in decls.uses.iter().enumerate() {
            let site = UseSite { expansion: call, index: count(index, module)? };
            let vis = self.resolve_vis(&u.vis, &mut bad_vis);
            let path = u.path.copy_in(a);
            let placeholder = At { frag: fid, slot: 0 };
            match u.leaf {
                UseForm::Glob => {
                    let g = count(self.globs.len(), module)?;
                    self.globs.push(Glob {
                        site,
                        path,
                        vis,
                        expansion,
                        at: placeholder,
                        source: Source::Pending,
                        first: None,
                        blocked: true,
                        via_glob: false,
                        cond: 0,
                    });
                    ordered.push((u.pos, Slot::Glob(g)));
                }
                UseForm::Named { rename, types_only } => {
                    let bound = match rename {
                        Rename::Same => path.segments.last().copied(),
                        Rename::To(name) => Some(name),
                        Rename::Underscore => None,
                    };
                    let i = count(self.imports.len(), module)?;
                    if let Some(name) = bound {
                        self.named.entry(name).or_insert_with(|| Vec::new_in(a)).push(i);
                    }
                    self.imports.push(Import {
                        site,
                        path,
                        bound,
                        types_only,
                        vis,
                        expansion,
                        at: placeholder,
                        ns: [NsRes::Pending; 3],
                        failed: None,
                        first: None,
                        blocked: true,
                        via_glob: false,
                        cond: 0,
                    });
                    ordered.push((u.pos, Slot::Use(i)));
                }
            }
        }
        for mr in decls.macro_rules.iter() {
            ordered.push((mr.pos, Slot::Rules { name: mr.name, def: mr.def, export: mr.export, shapes: mr.shapes }));
            if mr.export && expansion > 0 {
                exports.push(TextualDef { name: mr.name, def: mr.def, expansion, shapes: mr.shapes });
            }
        }
        for c in decls.calls.iter() {
            let i = count(self.calls.len(), module)?;
            let state = if expansion >= RECURSION_LIMIT {
                CallState::TooDeep
            } else {
                self.unexpanded += 1;
                self.blocked_calls += 1;
                CallState::Pending
            };
            self.calls.push(Call {
                key: c.call,
                path: c.path.copy_in(a),
                kind: c.kind,
                at: At { frag: fid, slot: 0 },
                item: c.item,
                state,
                first: None,
                blocked: true,
            });
            ordered.push((c.pos, Slot::Call(i)));
        }
        ordered.sort_unstable_by_key(|&(pos, _)| pos);
        let mut slots = Vec::with_capacity_in(ordered.len(), a);
        for (s, &(_, slot)) in ordered.iter().enumerate() {
            let at = At { frag: fid, slot: count(s, module)? };
            match slot {
                Slot::Use(i) => self.imports[i as usize].at = at,
                Slot::Glob(i) => self.globs[i as usize].at = at,
                Slot::Call(i) => self.calls[i as usize].at = at,
                Slot::Rules { .. } | Slot::Mod { .. } => {}
            }
            slots.push(slot);
        }
        let textual = slots.iter().any(|s| matches!(s, Slot::Rules { .. } | Slot::Mod { macro_use: true, .. }));
        let open = slots
            .iter()
            .filter(|s| matches!(s, Slot::Call(c) if self.calls[*c as usize].state.unexpanded()))
            .count();
        let open = count(open, module)?;
        self.frags.push(Fragment { expansion, parent, slots, items, bad_extern, bad_vis, deferred_vis, exports, textual, open });
        let mut up = parent;
        while let Some(p) = up {
            let f = &mut self.frags[p.frag as usize];
            f.open += open;
            f.textual |= textual;
            up = f.parent;
        }
        for k in 0..self.frags[fid as usize].items.len() {
            let (ns, name, e) = self.frags[fid as usize].items[k];
            self.insert_def(ns, name, e);
        }
        Ok(fid)
    }

    /// Keeps `e` as the binding of `(ns, name)` unless one written before it in the module's
    /// tree is kept (the later one is a `Duplicate` error of the final pass).
    fn insert_def(&mut self, ns: Namespace, name: N, e: DefEntry) {
        let replace = match self.defs.get(&(ns, name)).copied() {
            Some(old) => self.frag_before(e.frag, old.frag),
            None => true,
        };
        if replace {
            self.defs.insert((ns, name), e);
        }
    }

    /// The slots of the calls a fragment is inside, from the module's text down.
    fn frag_path(&self, frag: u32) -> AVec<'a, u32> {
        let mut path = Vec::new_in(self.alloc);
        let mut cur = frag;
        while let Some(p) = self.frags[cur as usize].parent {
            path.push(p.slot);
            cur = p.frag;
        }
        path.reverse();
        path
    }

    /// Whether fragment `a` is before `b` in the module's tree (an enclosing fragment first).
    fn frag_before(&self, a: u32, b: u32) -> bool {
        a != b && self.frag_path(a).as_slice() < self.frag_path(b).as_slice()
    }

    /// The order of two places in the module's tree: what "written first" means once
    /// expansions are in, whatever order they were taken in.
    pub(crate) fn tree_cmp(&self, a: At, b: At) -> Ordering {
        let mut pa = self.frag_path(a.frag);
        pa.push(a.slot);
        let mut pb = self.frag_path(b.frag);
        pb.push(b.slot);
        pa.as_slice().cmp(pb.as_slice())
    }

    /// Whether fragment `a` is `b` or inside `b`.
    pub(crate) fn is_desc(&self, a: u32, b: u32) -> bool {
        let mut cur = a;
        loop {
            if cur == b {
                return true;
            }
            match self.frags[cur as usize].parent {
                Some(p) => cur = p.frag,
                None => return false,
            }
        }
    }

    /// Keeps the answers of `replies`. An `InProgress` never replaces a final answer.
    pub(crate) fn absorb(&mut self, replies: &[Reply<'_, N>]) -> Result<(), ResolveError> {
        for reply in replies {
            let kept = match (reply.read, reply.answer) {
                (_, Answer::InProgress { module }) => Cached::InProgress(module),
                (Read::Name { .. }, Answer::Name(answer)) => Cached::Name(answer),
                (Read::Globs { .. }, Answer::Globs { edges, krate, chain, digest }) => {
                    let mut e = Vec::with_capacity_in(edges.len(), self.alloc);
                    e.extend_from_slice(edges);
                    let mut c = Vec::with_capacity_in(chain.len(), self.alloc);
                    c.extend_from_slice(chain);
                    Cached::Globs { edges: e, krate, chain: c, digest }
                }
                (
                    Read::TextualEntry { .. } | Read::TextualOut { .. },
                    Answer::Textual { defs, digest },
                ) => {
                    let mut v = Vec::with_capacity_in(defs.len(), self.alloc);
                    v.extend_from_slice(defs);
                    Cached::Textual { defs: v, digest }
                }
                (Read::DeriveHelpers { .. }, Answer::Helpers { names, digest }) => {
                    let mut v = Vec::with_capacity_in(names.len(), self.alloc);
                    v.extend_from_slice(names);
                    v.sort_unstable();
                    Cached::Helpers { names: v, digest }
                }
                (
                    Read::Name { .. }
                    | Read::Globs { .. }
                    | Read::TextualEntry { .. }
                    | Read::TextualOut { .. }
                    | Read::DeriveHelpers { .. },
                    Answer::Name(_) | Answer::Globs { .. } | Answer::Textual { .. } | Answer::Helpers { .. },
                ) => {
                    return Err(ResolveError::WrongAnswer {
                        module: self.module,
                        read_of: reply.read.module(),
                    });
                }
            };
            let keep_old = matches!(kept, Cached::InProgress(_))
                && matches!(self.answers.get(&reply.read), Some(c) if !matches!(c, Cached::InProgress(_)));
            if !keep_old {
                self.answers.insert(reply.read, kept);
            }
        }
        Ok(())
    }

    /// The answer to `read`: a missing one is needed, one in progress is waited on, any other
    /// is used (and kept with what it saw, in the final pass).
    pub(crate) fn cached(&self, read: Read<N>, sink: &mut Sink<'a, N>) -> Option<&Cached<'a, N>> {
        let Some(c) = self.answers.get(&read) else {
            sink.need.push(read);
            sink.touch(Key::Read(read));
            return None;
        };
        let saw = match c {
            Cached::Name(answer) => Saw::Name(*answer),
            Cached::Globs { digest, .. } | Cached::Textual { digest, .. } | Cached::Helpers { digest, .. } => {
                Saw::Digest(*digest)
            }
            Cached::InProgress(_) => {
                sink.waits.push(read);
                sink.touch(Key::Read(read));
                return None;
            }
        };
        if sink.scan == Scan::Final {
            #[cfg(test)]
            let keep = !(sink.forget_negative && saw == Saw::Name(NameAnswer::Absent))
                && !(sink.forget_probes && sink.probing);
            #[cfg(not(test))]
            let keep = true;
            if keep {
                sink.used.push(ReadEntry { read, saw });
            }
        }
        Some(c)
    }

    /// Sets each pending item's flag to whether it is in `waiting`; whether a flag changed.
    pub(crate) fn set_flags(&mut self, waiting: &[Item], upto: [usize; 3]) -> bool {
        let wait = |item: Item, pending: bool| pending && waiting.binary_search(&item).is_ok();
        self.flags_by(upto, |item, pending| wait(item, pending))
    }

    /// Sets each item's blocked flag below `upto` to `to(item, pending)`; whether a flag
    /// changed.
    fn flags_by(&mut self, upto: [usize; 3], to: impl Fn(Item, bool) -> bool) -> bool {
        let mut changed = false;
        for (i, imp) in self.imports.iter_mut().enumerate().take(upto[0]) {
            let new = to(Item::Import(i as u32), !imp.done());
            if imp.blocked != new {
                imp.blocked = new;
                changed = true;
            }
        }
        for (i, g) in self.globs.iter_mut().enumerate().take(upto[1]) {
            let new = to(Item::Glob(i as u32), matches!(g.source, Source::Pending));
            if g.blocked != new {
                g.blocked = new;
                changed = true;
            }
        }
        for i in 0..upto[2] {
            let new = to(Item::Call(i as u32), self.calls[i].state == CallState::Pending);
            changed |= self.set_call_blocked(i, new);
        }
        changed
    }

    /// Sets glob `g`'s source and recomputes the module's edges: a glob's source is written at
    /// most twice (decided, then perhaps failed as ambiguous), so the edges cost
    /// O(globs) per write and every walk through this module borrows them for free.
    pub(crate) fn set_glob_source(&mut self, g: usize, source: Source) {
        self.globs[g].source = source;
        self.edges = self.glob_edges();
    }

    /// The module's glob edges, current (see [`Resolver::set_glob_source`]).
    pub(crate) fn edges(&self) -> &[GlobEdge] {
        &self.edges
    }

    /// Sets call `c`'s state, keeping the counts of calls not expanded and of pending calls
    /// blocked (what [`Resolver::calls_state`] reads in constant time).
    pub(crate) fn set_call_state(&mut self, c: usize, state: CallState) {
        let call = &mut self.calls[c];
        let old = call.state;
        if old.unexpanded() != state.unexpanded() {
            // The fragment holding the call and every fragment it sits inside.
            let mut up = Some(call.at);
            while let Some(p) = up {
                let f = &mut self.frags[p.frag as usize];
                if state.unexpanded() {
                    f.open += 1;
                } else {
                    f.open -= 1;
                }
                up = f.parent;
            }
        }
        let call = &mut self.calls[c];
        if old.unexpanded() && !state.unexpanded() {
            self.unexpanded -= 1;
        }
        if old == CallState::Pending && state != CallState::Pending && call.blocked {
            self.blocked_calls -= 1;
        }
        call.state = state;
    }

    /// Sets call `c`'s blocked flag, keeping the count of pending calls blocked; whether it
    /// changed.
    pub(crate) fn set_call_blocked(&mut self, c: usize, to: bool) -> bool {
        let call = &mut self.calls[c];
        if call.blocked == to {
            return false;
        }
        call.blocked = to;
        if call.state == CallState::Pending {
            if to {
                self.blocked_calls += 1;
            } else {
                self.blocked_calls -= 1;
            }
        }
        true
    }

    /// Every pending item blocked: after a move, nothing is known not to wait.
    pub(crate) fn block_all(&mut self) {
        let upto = self.upto();
        self.flags_by(upto, |_, pending| pending);
    }

    pub(crate) fn upto(&self) -> [usize; 3] {
        [self.imports.len(), self.globs.len(), self.calls.len()]
    }

    /// Fails `item`, a member of a sink of the stall rule's wait graph (bottom.md section 6):
    /// what it still waits on is taken as absent, so an import keeps the namespaces it found,
    /// and is not found (or private) if none. Whether it was pending.
    pub(crate) fn fail_stalled(&mut self, item: Item) -> bool {
        match item {
            Item::Import(i) => {
                let imp = &mut self.imports[i as usize];
                if imp.done() {
                    return false;
                }
                let mut any = false;
                let mut private = false;
                for ns in Namespace::ALL {
                    match imp.ns[ns.index()] {
                        NsRes::Pending if imp.imports(ns) => imp.ns[ns.index()] = NsRes::Absent,
                        NsRes::Found(_) => any = true,
                        NsRes::Private(_) => private = true,
                        NsRes::Pending | NsRes::Absent => {}
                    }
                }
                if !any {
                    let last = u32::try_from(imp.path.segments.len().saturating_sub(1)).unwrap_or(u32::MAX);
                    imp.failed = Some((last, if private { Unresolved::Private } else { Unresolved::NotFound }));
                }
                true
            }
            Item::Glob(g) => {
                let glob = &mut self.globs[g as usize];
                if !matches!(glob.source, Source::Pending) {
                    return false;
                }
                let last = u32::try_from(glob.path.segments.len().saturating_sub(1)).unwrap_or(u32::MAX);
                self.set_glob_source(g as usize, Source::Failed(last, Unresolved::NotFound));
                true
            }
            Item::Call(c) => {
                if self.calls[c as usize].state != CallState::Pending {
                    return false;
                }
                let last = u32::try_from(self.calls[c as usize].path.segments.len().saturating_sub(1)).unwrap_or(u32::MAX);
                self.set_call_state(c as usize, CallState::Failed(last, Unresolved::NotFound));
                true
            }
        }
    }

    /// The most passes that can each move something: every import namespace, glob and call
    /// moves forward only (rule 18).
    pub(crate) fn pass_bound(&self) -> u32 {
        let work = 4 * self.imports.len() + 2 * self.globs.len() + 3 * self.calls.len() + 4;
        u32::try_from(work).unwrap_or(u32::MAX)
    }

    /// One step of the protocol: takes `replies`, runs until it needs something, and says what.
    pub fn step<F: ScopeFacts<Name = N>>(
        &mut self,
        facts: &F,
        replies: &[Reply<'_, N>],
    ) -> Result<Step<'_, N, Resolved<'a, N>>, ResolveError> {
        if self.finished {
            return Err(ResolveError::Finished { module: self.module });
        }
        self.absorb(replies)?;
        let alloc = self.alloc;
        let mut out = core::mem::replace(&mut self.out, Outbox::new_in(alloc));
        let ran = crate::work::run(core::slice::from_mut(self), facts, replies, &mut out, alloc);
        self.out = out;
        match ran? {
            Ran::Ask => Ok(Step::Ask(&self.out.asks)),
            Ran::Expand => Ok(Step::Expand(&self.out.expand)),
            Ran::Cycle => Ok(Step::Cycle(&self.out.cycle)),
            Ran::Done(mut all) => {
                all.pop().map(Step::Done).ok_or(ResolveError::BadGroup { module: None })
            }
        }
    }

    /// The work of every step so far.
    pub fn stats(&self) -> FillStats {
        self.out.stats
    }
}

/// Fills `out.cycle` with the modules the reads of `waits` were answered in progress on, those
/// that are not members; whether there is one. The module is the one the answerer named
/// (`Answer::InProgress { module }`), never one guessed from the read: a `TextualEntry` waits
/// on whichever ancestor is pending, which only the answerer knows.
pub(crate) fn cycle_of<N: Name>(
    ms: &[Resolver<'_, N>],
    waits: &[Read<N>],
    out: &mut Outbox<'_, N>,
) -> bool {
    out.cycle.clear();
    for read in waits {
        let on = ms.iter().find_map(|r| match r.answers.get(read) {
            Some(Cached::InProgress(m)) => Some(*m),
            Some(_) | None => None,
        });
        if let Some(m) = on {
            if ms.binary_search_by(|r| r.module.cmp(&m)).is_err() {
                out.cycle.push(m);
            }
        }
    }
    out.cycle.sort_unstable();
    out.cycle.dedup();
    !out.cycle.is_empty()
}

/// The final pass over every member, once nothing can move: their results, or the reads it
/// still needs, or the cycle it waits on.
pub(crate) fn finish<'a, N: Name, F: ScopeFacts<Name = N>>(
    ms: &mut [Resolver<'a, N>],
    facts: &F,
    out: &mut Outbox<'a, N>,
    alloc: Alloc<'a>,
) -> Result<Ran<'a, N>, ResolveError> {
    {
        let mut done = Vec::with_capacity_in(ms.len(), alloc);
        let mut asks = Vec::new_in(alloc);
        let mut waits = Vec::new_in(alloc);
        
        {
            let env = Env { members: &*ms };
            for m in ms.iter() {
                match m.finalize(facts, env, &mut out.stats)? {
                    Finish::Need { asks: a, waits: w } => {
                        asks.extend_from_slice(&a);
                        waits.extend_from_slice(&w);
                    }
                    Finish::Done(resolved) => done.push(resolved),
                }
            }
        }
        if !asks.is_empty() {
            out.asks.clear();
            out.asks.extend_from_slice(&asks);
            out.asks.sort_unstable();
            out.asks.dedup();
            return Ok(Ran::Ask);
        }
        if cycle_of(ms, &waits, out) {
            return Ok(Ran::Cycle);
        }
        if done.len() != ms.len() {
            // A final pass waited on a member: a member's records are read live, never
            // waited on, so this is the machine's own bug, stopped on rather than hidden.
            let module = ms.iter().map(|m| m.module).find(|m| !done.iter().any(|d| d.scope.module == *m));
            return Err(ResolveError::MemberUnfinished { module: module.unwrap_or(ms[0].module) });
        }
        #[cfg(test)]
        let by_records = ms[0].plant != Plant::FellowReadsLeftOut;
        #[cfg(not(test))]
        let by_records = true;
        if ms.len() > 1 && by_records {
            // A member's read list reads its fellows by their records, as any module's does.
            match crate::finish::settle_reads(ms, done, facts, &mut out.stats, alloc)? {
                crate::finish::Settled::Done(all) => done = all,
                crate::finish::Settled::Need { asks, waits } => {
                    if !asks.is_empty() {
                        out.asks.clear();
                        out.asks.extend_from_slice(&asks);
                        return Ok(Ran::Ask);
                    }
                    if cycle_of(ms, &waits, out) {
                        return Ok(Ran::Cycle);
                    }
                    return Err(ResolveError::MemberUnfinished { module: ms[0].module });
                }
            }
        }
        for m in ms.iter_mut() {
            m.finished = true;
        }
        Ok(Ran::Done(done))
    }
}
