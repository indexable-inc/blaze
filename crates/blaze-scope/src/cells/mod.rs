//! The cell core of the name solver (bottom.md section 6, "The cell core (2026-11-16)"), stage 2:
//! named imports, glob imports and item-position macro calls of one crate.
//!
//! A macro call is an item like an import: its path reads the textual scope of `macro_rules!`
//! first, then cells in the macro namespace. A module's calls not expanded yet are its growth: a
//! `Globs` cell waits on them wherever an expansion could still write the name (always outside
//! the macro namespace; in it, while the globs bring nothing), so no cell is decided "not there"
//! before its module stops growing. A resolved call is expanded through the driver and
//! [`ScopeFacts::expansion`]; what it writes is added to its module as a new text fragment.
//!
//! A cell is (module, namespace, name, layer) and is decided once. An item (an import) is run
//! from the top over final reads of cells; reading an undecided cell leaves one waiter record
//! and stops the item. At a quiet point the undecided nodes are condensed by an iterative Tarjan
//! pass: a sink of cells alone is a glob cycle decided as its least fixed point, a sink with
//! items is a stall whose items fail. The worklist is two ordered sets, so the outcome and the
//! counters never depend on the order anything was pushed in.

use core::fmt;

use blaze_id::{CrateItemKey, ItemKey};
use blaze_own::{ABTreeMap, ABTreeSet, AVec, Alloc};
use blaze_surface::{DeclsBuilder, SurfaceError};

use crate::facts::ScopeFacts;
use crate::{AmbiguityKind, Def, DefKind, Edition, ExternTarget, Namespace, Origin, Path, PathKind, Rename, Unresolved, UseForm, Visibility, WrittenVis};

mod deps;
mod seam;
#[cfg(test)]
mod tests;
mod write;

pub(crate) use deps::{BuiltDeps, DepAnswer, DepNames};
pub(crate) use seam::shadow;
pub use seam::{DIFFER_SHOWN, Differ, Seam, ShadowRefused, Shadowed, Unknown, WrittenAgainst, WrittenDiffer, WrittenWhich};

pub(crate) const NAMESPACES: [Namespace; 3] = [Namespace::Types, Namespace::Values, Namespace::Macros];

/// A cell's namespace and layer as one byte of its key.
fn cell_slot(ns: Namespace, layer: Layer) -> u8 {
    (ns_at(ns) as u8) * 2 + layer as u8
}

fn ns_at(ns: Namespace) -> usize {
    match ns {
        Namespace::Types => 0,
        Namespace::Values => 1,
        Namespace::Macros => 2,
    }
}

/// Which bindings of a name a cell holds.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) enum Layer {
    /// What a path reads: the explicit entry or named import, else the globs.
    Full,
    /// Through the module's globs alone: what a named import of this very name reads.
    Globs,
}

/// A decided cell.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Value {
    NotFound,
    Found(Def, Visibility),
    /// Two different items through globs (R2); the smaller def, the first visibility.
    Ambiguous(Def, Visibility),
    /// Brought by an import whose path stage 1 does not follow (`Outcome::Outside`): unknown
    /// here, and so is every answer read from it (never taken as absent).
    Outside,
}

/// One import's or macro call's answer, as [`Cells::results`] lends it.
pub struct ItemResult<'r, N> {
    pub module: ItemKey,
    /// A macro call's key; `None` for an import.
    pub call: Option<ItemKey>,
    pub kind: PathKind,
    pub segments: &'r [N],
    pub glob: bool,
    /// The name a named import binds.
    pub binds: Option<N>,
    /// `None`: left undecided (a bug the run reports first).
    pub outcome: Option<Outcome>,
    /// Why the outcome is `Outside`, when it is.
    pub why: Option<Unfollowed>,
}

/// What an import came to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// What a named import binds in each namespace.
    Named([Option<(Def, Visibility)>; 3]),
    /// A glob's source module.
    Glob(ItemKey),
    Failed(Unresolved),
    /// A path stage 1 does not follow (a plain path missing here, `::`, `$crate`, an enum, another
    /// crate): stage 2's read modes and the extern prelude decide it.
    Outside,
    /// A macro call's path: the macro it names. Its expansion follows.
    Called(Def),
}

/// Why an import or a call is [`Outcome::Outside`]: the form this core does not follow, by name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Unfollowed {
    /// `$crate` of a crate no dependency read here names.
    DollarCrateUnknown,
    /// A crate (`::name`, `$crate`) whose root module no record read here holds.
    CrateUnread,
    /// `use ::name;`: a crate alone.
    CrateAlone,
    /// A segment before the leaf is itself unknown.
    Through,
    /// A path through an enum to its variants.
    Enum,
    /// A segment names a module no record read here holds.
    ModuleUnread,
    /// The leaf is unknown in some namespace of the module it is read in.
    Leaf,
    /// `use name;` beside a `macro_rules! name` of the module's text.
    TextualImport,
}

/// The number of [`Unfollowed`] classes.
pub const UNFOLLOWED: usize = 8;

impl Unfollowed {
    pub fn at(self) -> usize {
        self as usize
    }
}

/// Which part of a written visibility names no ancestor of its module.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BadVis {
    /// `pub(super)` or `super` in `pub(in ..)` above the root.
    AboveRoot,
    /// `pub(path)` without `in`.
    WithoutIn,
    /// `pub(in path)` starting with neither `crate`, `self` nor `super`.
    Start,
    /// The segment at this index is not the next module down toward the item.
    Segment(usize),
}

/// Why the core stopped, naming the thing.
#[derive(Debug)]
pub enum CellError {
    Facts { module: ItemKey, error: SurfaceError },
    /// `pub(in path)` naming no ancestor, `pub(path)` or `pub(super)` at the root.
    BadVisibility { module: ItemKey, why: BadVis },
    /// A cycle's least fixed point did not settle within its bound.
    NoFixpoint { module: ItemKey, cells: usize },
    /// The driver was asked to expand the call and the facts still have no expansion of it.
    NotExpanded { call: ItemKey },
    /// Reading the call's expansion failed.
    Expansion { call: ItemKey, error: SurfaceError },
    /// A bug in when cells decide: an expansion writes a name into `module` whose cell was
    /// already decided "not there" (finality, bottom.md section 6).
    DecidedTooEarly { module: ItemKey, ns: Namespace },
    /// A group of a quiet point whose every member is a call spared by the textual rule (R5).
    AllSpared { module: ItemKey },
}

impl fmt::Display for CellError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CellError::NotExpanded { call } => write!(f, "call {call:?}: expanded by the driver, but the facts have no expansion"),
            CellError::Expansion { call, error } => write!(f, "call {call:?}: reading its expansion: {error}"),
            CellError::DecidedTooEarly { module, ns } => {
                write!(f, "module {module:?}: an expansion writes a {ns:?} name whose cell was decided not found (a bug in the core)")
            }
            CellError::AllSpared { module } => write!(f, "module {module:?}: every member of a stalled group is spared by text order"),
            CellError::Facts { module, error } => write!(f, "module {module:?}: reading its facts: {error}"),
            CellError::BadVisibility { module, why } => write!(f, "module {module:?}: a visibility that names no ancestor: {why:?}"),
            CellError::NoFixpoint { module, cells } => {
                write!(f, "module {module:?}: a glob cycle of {cells} cells has no fixed point within its bound")
            }
        }
    }
}

/// One real-code run: its counters and its times by the passed clock.
#[derive(Clone, Copy, Debug)]
pub struct CellRun {
    pub stats: CellStats,
    /// Reading the facts into modules, entries and items.
    pub load_ns: u64,
    /// The solve alone.
    pub run_ns: u64,
    /// One hash probe of a cell key, measured on the run's own keys, in hundredths of a ns.
    pub probe_centi_ns: u64,
}

/// The counters that prove the shape.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct CellStats {
    pub modules: u64,
    pub names_declared: u64,
    /// Imports.
    pub items: u64,
    pub cells: u64,
    pub decisions: u64,
    pub waiters: u64,
    pub wakes: u64,
    pub evaluations: u64,
    pub max_evaluations: u64,
    pub quiet_points: u64,
    /// Undecided cells and items condensed, summed over the quiet points.
    pub quiet_nodes: u64,
    /// Groups of cells alone decided as a least fixed point (glob cycles).
    pub cycle_passes: u64,
    pub cycle_cells: u64,
    pub largest_cycle: u64,
    /// Groups holding an import decided as a least fixed point (R5).
    pub stalls: u64,
    /// Imports such a group left not found.
    pub stalled_items: u64,
    /// Macro calls met (module texts and expansions).
    pub calls: u64,
    /// Calls expanded.
    pub expansions: u64,
    /// Walks of the textual scope.
    pub textual_reads: u64,
    /// Calls a stall spared for text order (R5's refinement).
    pub spared: u64,
    /// Plain first segments that missed the module and took an outer binding (R6).
    pub outward: u64,
    /// R6 errors: an expansion not enclosing a use wrote, into the module, the name the use
    /// took from an outer scope (or the use met it there beside a different outer binding).
    pub r6_errors: u64,
    /// Cells of dependencies' modules, each decided from the record when made.
    pub dep_reads: u64,
    /// Paths into another crate whose module no dependency read here holds: left unknown.
    pub dep_absent: u64,
}

/// A bug planted on purpose, one per rule, for the tests' controls.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CellPlant {
    None,
    /// R1 broken: a glob binding shadows the module's own entries.
    GlobsShadowExplicit,
    /// R2 broken: the first glob wins over a second bringing another item.
    FirstGlobWins,
    /// R3 broken: a glob cycle is decided as not found.
    CycleNotFound,
    /// R4 broken: a glob brings in what the importer may not see.
    GlobLeaksPrivate,
    /// R5 broken: a stall fails every undecided item, not the sinks' alone.
    StallFailsAll,
    /// Calls are not growth: a cell is decided while its module's calls may still write it.
    CallsNoGrowth,
    /// A call skips the textual scope of `macro_rules!`.
    NoTextualScope,
    /// R5's refinement broken: a call waiting in text order on a call of its group is failed too.
    StallSparesNone,
    /// R6 broken: growth by expansion holds a first segment the outer scope binds.
    OutwardWaits,
    /// R6 broken: no watch, so a later definition by a non-enclosing expansion is no error.
    NoWatch,
    /// R7 broken: a glob candidate wins over a different outer one.
    GlobBeatsOuter,
    /// A stalled item whose guess is `Outside` is failed by the stall, not marked first.
    StallFailsOutside,
    /// A path into a dependency's module is unknown (`Outside`), its record unread.
    DepIsOutside,
    /// A glob of a dependency's module brings nothing.
    DepGlobEmpty,
    /// `$crate` of a dependency's macro is taken as this crate's root.
    DollarCrateIsRoot,
    /// A name brought by an `Outside` import is taken as not found.
    OutsideIsNotFound,
    /// A name a local standard prelude lacks stops there: the language prelude is not read.
    NoLanguagePrelude,
    /// R5 broken for calls: a stalled one-segment call an outer scope provides is failed.
    StallFailsOuterCall,
    /// An import through an enum is unknown in the macro namespace too.
    EnumHidesMacros,
    /// R5 broken for imports: a stalled group decides every member from its guess at once,
    /// the failed ones beside the resolved ones.
    StallDecidesAll,
    /// R6 broken by order: a definition an earlier expansion wrote beside a different outer
    /// binding wins over it (the answer then depends on which came first).
    InnerAfterWrite,
    /// An `extern crate` item binds nothing in its module (Reference `items.extern-crate`).
    ExternCrateBindsNothing,
    /// R8 broken: a macro-written root `extern crate` adds no extern-prelude name.
    R8AddsNothing,
    /// R8 broken: a macro-written root `extern crate .. as n;` replaces a dependency's `n`.
    R8MacroReplaces,
    /// A `use` of an `extern crate` item is narrowed to the item's own visibility.
    ExternReexportNarrowed,
    /// A `mod` line of a module the facts hold no text of refuses the whole crate.
    UnreadModuleRefuses,
    /// `use a::b::{self};` binds `b` in every namespace.
    SelfImportBindsAll,
    /// A 2015 crate's `use a::b;` starts lexically at its own module, as from 2018 on.
    Use2015Lexical,
    /// `use name;` does not read the textual scope of `macro_rules!`: beside a `macro_rules!
    /// name` of the module's text it is unknown (the core's answer until runner 11).
    NoTextualImport,
}

type ModId = u32;
type CellId = u32;
type ItemId = u32;
type FragId = u32;
/// An explicit entry of a module: the definition, its visibility, what wrote it (an item or
/// `mod`, an `extern crate`, a `#[macro_export]`) and the fragment it is written in.
type Entry = (Def, Visibility, Origin, FragId);

/// One entry of a text fragment the textual scope sees.
#[derive(Clone, Copy)]
enum Slot<N> {
    /// `macro_rules! name`.
    Rules(N, Def),
    /// `mod child;`, with `#[macro_use]` or not.
    Mod(ModId, bool),
    /// A macro call: its expansion is a fragment at this place.
    Call(ItemId),
}

/// A module's own text or one call's expansion: its textual-scope slots in text order.
struct Frag<'a, N> {
    module: ModId,
    /// The fragment and position of the call it is the expansion of; `None` for a module's text.
    up: Option<(FragId, u32)>,
    slots: AVec<'a, (u32, Slot<N>)>,
}

/// Where a call is written.
#[derive(Clone, Copy)]
struct CallAt {
    key: ItemKey,
    frag: FragId,
    pos: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Node {
    Cell(CellId),
    Item(ItemId),
}

struct Module<'a, N> {
    key: ItemKey,
    /// Its name in its parent (none at the root).
    name: Option<N>,
    parent: Option<ModId>,
    /// Itself, then its ancestors up to the root.
    chain: AVec<'a, ItemKey>,
    explicit: ABTreeMap<'a, (Namespace, N), Entry>,
    /// The named imports binding each name, by index.
    named: ABTreeMap<'a, N, AVec<'a, ItemId>>,
    globs_pending: u32,
    /// Its glob imports, by index.
    glob_items: AVec<'a, ItemId>,
    /// Resolved glob edges by use index: the source and the glob's visibility.
    edges: ABTreeMap<'a, u32, (ModId, Visibility)>,
    /// Its `Globs` cells, in creation order.
    glob_cells: AVec<'a, CellId>,
    /// Its macro calls (its text's and its expansions'), by item id.
    calls: AVec<'a, ItemId>,
    /// Its calls whose expansion is not in yet: its growth.
    growth: u32,
    /// Its own text.
    text: FragId,
    /// Where its `mod` line is: a fragment of its parent and a position there.
    mod_line: Option<(FragId, u32)>,
    /// Its imports read so far: the next one's index.
    uses: u32,
    /// A module of a dependency: each of its cells is decided when made, from the dependency's
    /// record ([`DepNames`]); it has no text, items or growth here.
    foreign: bool,
    /// A module a `mod` line names whose text the facts do not hold (its file was not read, or
    /// an attribute macro on the `mod` item was not expanded): the name is bound in its parent
    /// (Reference `items.mod`), and every name inside it is unknown (`Value::Outside`), never
    /// "not there". Listed in [`Cells::unread`].
    unread: bool,
}

struct Cell<'a, N> {
    module: ModId,
    ns: Namespace,
    name: N,
    layer: Layer,
    value: Option<Value>,
    waiters: AVec<'a, Node>,
    waiting_on: Option<Node>,
}

struct Item<'a, N> {
    module: ModId,
    index: u32,
    kind: PathKind,
    segments: AVec<'a, N>,
    glob: bool,
    /// The name a named import binds (`None` for a glob or `as _`).
    binds: Option<N>,
    vis: Visibility,
    outcome: Option<Outcome>,
    waiters: AVec<'a, Node>,
    /// The undecided nodes the last evaluation stopped on.
    waits: AVec<'a, Node>,
    evaluations: u64,
    /// A macro call: where it is written. `None` for an import.
    call: Option<CallAt>,
    /// A call that has added what it writes (its expansion, or nothing once it failed).
    grown: bool,
    /// A call's expansion, once read.
    expansion: Option<FragId>,
    /// The fragment it is written in (its module's text or an expansion).
    at: FragId,
    /// Its place in that fragment's text: what the textual scope before it is cut at.
    pos: u32,
    /// An import whose path goes through an enum: `Outside` in types and values only (a
    /// variant is never a macro).
    of_enum: bool,
    /// `use a::b::{self};`: it binds in the type namespace alone.
    types_only: bool,
    /// Why its outcome is `Outside`, when it is.
    why: Option<Unfollowed>,
}

/// What a module's own view leaves out of its growth (calls not expanded yet).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Skip {
    /// Nothing: the cell's own value waits on every call.
    No,
    /// The asking call, which never waits on itself.
    Call(ItemId),
    /// All of it: R6's inner scope, which growth by expansion alone does not hold.
    Growth,
}

/// Where a module's own view of a name came from (R6, R7).
#[derive(Clone, Copy, PartialEq, Eq)]
enum From {
    /// An item, `mod` or named import of the module (an item an expansion wrote is `Wrote`).
    Own,
    /// An item or `mod` the expansion at this fragment wrote.
    Wrote(FragId),
    /// A glob import.
    Glob,
}

/// The outer scopes a plain first segment reaches (R6, R8), read at load.
struct Outer<'a, N> {
    /// The extern prelude, sorted by name: each name to a dependency's root module (R8: final).
    externs: AVec<'a, (N, ItemKey)>,
    /// The names macro-written root `extern crate` items added (R8), asked after `externs`: a
    /// dependency's name is never replaced.
    added: ABTreeMap<'a, N, ItemKey>,
    /// The standard prelude's module when it is one of this crate's (core's `#[prelude_import]`).
    local: Option<ModId>,
    /// A standard prelude of another crate: its own items and modules by name.
    foreign: ABTreeMap<'a, (Namespace, N), Def>,
    /// That prelude also imports: a name its own items miss is not known here.
    foreign_open: bool,
    /// The `#[macro_use]` prelude of other crates' exported macros is not empty: a macro name
    /// that misses before it is not known here.
    macro_use: bool,
}

pub(crate) struct Cells<'a, N> {
    alloc: Alloc<'a>,
    krate: CrateItemKey,
    root: ModId,
    modules: AVec<'a, Module<'a, N>>,
    by_key: ABTreeMap<'a, ItemKey, ModId>,
    cells: AVec<'a, Cell<'a, N>>,
    /// Lookup only, never iterated: a hash map keeps the order-free outcome.
    cell_ids: blaze_hash::HashMap<(ModId, u8, N), CellId, Alloc<'a>>,
    items: AVec<'a, Item<'a, N>>,
    frags: AVec<'a, Frag<'a, N>>,
    cell_queue: ABTreeSet<'a, CellId>,
    item_queue: ABTreeSet<'a, ItemId>,
    /// Calls resolved and not expanded yet.
    expand_queue: ABTreeSet<'a, ItemId>,
    /// Inputs a group's fixed point met outside the group (a node reads the second): edges the
    /// quiet point adds to the wait graph, so the group is not taken for a sink again.
    extra: ABTreeSet<'a, (Node, Node)>,
    outer: Outer<'a, N>,
    /// R6's watches: a use whose plain first segment took an outer binding while its module could
    /// still grow, by module and name.
    watches: ABTreeMap<'a, (ModId, Namespace, N), AVec<'a, (ItemId, Def)>>,
    /// Names an expansion wrote into a module as items or `mod`s: the fragment that wrote each.
    written: ABTreeMap<'a, (ModId, Namespace, N), FragId>,
    /// The uses an R6 error is reported for.
    pub(crate) r6: ABTreeSet<'a, ItemId>,
    pub(crate) stats: CellStats,
    pub(crate) plant: CellPlant,
    /// The dependencies' decided names; `None`: every path into another crate is unknown.
    deps: Option<&'a dyn DepNames<N>>,
    /// The crate's edition: what `::name` starts from.
    edition: Edition,
    /// The `#[macro_use]` prelude's crates' root modules, in order (their exported macros).
    macro_use: AVec<'a, ItemKey>,
    /// Every dependency by name, sorted (the root record's): what an `extern crate` names.
    dep_roots: AVec<'a, (N, ItemKey)>,
    /// The `extern crate` items naming no dependency: the module and the name written.
    pub(crate) bad_extern: AVec<'a, (ItemKey, N)>,
    /// R8's errors: a macro-written root `extern crate .. as n;` whose `n` a dependency holds.
    pub(crate) r8: AVec<'a, N>,
    /// The modules a `mod` line names whose text the facts do not hold, in the order met.
    pub(crate) unread: AVec<'a, ItemKey>,
    /// R6's watches for the extern prelude: a use whose first segment took a later outer scope's
    /// binding while the root's calls could still add an extern-prelude name, by name.
    extern_watches: ABTreeMap<'a, N, AVec<'a, (ItemId, Def)>>,
}

/// A read of a cell by an item: decided, or the item now waits on it.
enum Read {
    Ready(Value),
    Wait(CellId),
}

enum Step<T> {
    Done(T),
    Wait(Node),
}

/// The current values of a group's members while its least fixed point is computed: `None` is
/// the bottom (a cell not found, an import that brings nothing yet).
/// A member's new guess.
enum GuessStep {
    Cell(Option<Value>),
    Item(Option<Outcome>),
}

struct Guess<'a> {
    cells: ABTreeMap<'a, CellId, Option<Value>>,
    items: ABTreeMap<'a, ItemId, Option<Outcome>>,
}

impl<'a, N: crate::Name> Cells<'a, N> {
    /// Reads the crate's module tree under `root` from `facts`: modules, explicit entries and
    /// imports. Modules are numbered in key order, so nothing depends on the order they are met.
    pub(crate) fn load<F: ScopeFacts<Name = N>>(facts: &F, root: ItemKey, alloc: Alloc<'a>) -> Result<Cells<'a, N>, CellError> {
        Cells::load_with(facts, root, None, alloc)
    }

    /// [`Cells::load`] with the dependencies' decided names: a path into one of their modules
    /// reads it (R8's extern prelude, `$crate`, `::name`, the `#[macro_use]` prelude, globs).
    pub(crate) fn load_with<F: ScopeFacts<Name = N>>(
        facts: &F,
        root: ItemKey,
        deps: Option<&'a dyn DepNames<N>>,
        alloc: Alloc<'a>,
    ) -> Result<Cells<'a, N>, CellError> {
        Cells::load_planted(facts, root, deps, CellPlant::None, alloc)
    }

    /// [`Cells::load_with`] with a planted bug the load itself reads (a control of the tests).
    pub(crate) fn load_planted<F: ScopeFacts<Name = N>>(
        facts: &F,
        root: ItemKey,
        deps: Option<&'a dyn DepNames<N>>,
        plant: CellPlant,
        alloc: Alloc<'a>,
    ) -> Result<Cells<'a, N>, CellError> {
        let info = facts.module(root).map_err(|error| CellError::Facts { module: root, error })?;
        let mut cells = Cells {
            alloc,
            krate: info.krate,
            root: 0,
            modules: AVec::new_in(alloc),
            by_key: ABTreeMap::new_in(alloc),
            cells: AVec::new_in(alloc),
            cell_ids: blaze_hash::HashMap::new_in(alloc),
            items: AVec::new_in(alloc),
            frags: AVec::new_in(alloc),
            cell_queue: ABTreeSet::new_in(alloc),
            item_queue: ABTreeSet::new_in(alloc),
            expand_queue: ABTreeSet::new_in(alloc),
            extra: ABTreeSet::new_in(alloc),
            outer: Outer {
                externs: AVec::new_in(alloc),
                added: ABTreeMap::new_in(alloc),
                local: None,
                foreign: ABTreeMap::new_in(alloc),
                foreign_open: false,
                macro_use: false,
            },
            watches: ABTreeMap::new_in(alloc),
            written: ABTreeMap::new_in(alloc),
            r6: ABTreeSet::new_in(alloc),
            stats: CellStats::default(),
            plant,
            deps,
            edition: Edition::E2021,
            macro_use: AVec::new_in(alloc),
            dep_roots: AVec::new_in(alloc),
            bad_extern: AVec::new_in(alloc),
            r8: AVec::new_in(alloc),
            unread: AVec::new_in(alloc),
            extern_watches: ABTreeMap::new_in(alloc),
        };
        // The dependencies by name before any text is read: an `extern crate` of any module
        // names one.
        {
            let r = facts.crate_root(info.krate).map_err(|error| CellError::Facts { module: root, error })?;
            cells.dep_roots.extend(r.deps.iter().map(|d| (d.name, d.root)));
        }
        let made = cells.make_modules(facts, &[root])?;
        cells.root = cells.by_key[&root];
        for &m in made.iter() {
            cells.read_text(facts, m)?;
        }
        Ok(cells)
    }

    /// Makes the modules of the subtrees under `tops` that are not made yet (the `mod` lines of
    /// their texts; those an expansion writes come with it), numbered in key order after the
    /// modules already made, with parents and chains. Their texts are read by the caller.
    fn make_modules<F: ScopeFacts<Name = N>>(&mut self, facts: &F, tops: &[ItemKey]) -> Result<AVec<'a, ModId>, CellError> {
        let alloc = self.alloc;
        let mut keys: ABTreeSet<'a, ItemKey> = ABTreeSet::new_in(alloc);
        let mut todo: AVec<'a, ItemKey> = AVec::from_slice_in(tops, alloc);
        let mut unread: ABTreeSet<'a, ItemKey> = ABTreeSet::new_in(alloc);
        while let Some(m) = todo.pop() {
            if self.by_key.contains_key(&m) || !keys.insert(m) {
                continue;
            }
            let d = match facts.decls(m) {
                Ok(d) => d,
                // A `mod` line of a module the facts hold no text of: bound, its inside unknown.
                Err(SurfaceError::NoRecord { .. })
                    if self.plant != CellPlant::UnreadModuleRefuses && !(self.modules.is_empty() && tops.first() == Some(&m)) =>
                {
                    unread.insert(m);
                    self.unread.push(m);
                    continue;
                }
                Err(error) => return Err(CellError::Facts { module: m, error }),
            };
            for c in d.mods.iter() {
                todo.push(c.module);
            }
        }
        let mut made: AVec<'a, ModId> = AVec::new_in(alloc);
        for &k in keys.iter() {
            let id = self.modules.len() as ModId;
            self.by_key.insert(k, id);
            self.modules.push(Module {
                key: k,
                name: None,
                parent: None,
                chain: AVec::new_in(alloc),
                explicit: ABTreeMap::new_in(alloc),
                named: ABTreeMap::new_in(alloc),
                globs_pending: 0,
                glob_items: AVec::new_in(alloc),
                edges: ABTreeMap::new_in(alloc),
                glob_cells: AVec::new_in(alloc),
                calls: AVec::new_in(alloc),
                growth: 0,
                text: FragId::MAX,
                mod_line: None,
                uses: 0,
                foreign: false,
                unread: unread.contains(&k),
            });
            if !unread.contains(&k) {
                made.push(id);
            }
        }
        for &m in made.iter() {
            let key = self.modules[m as usize].key;
            let info = facts.module(key).map_err(|error| CellError::Facts { module: key, error })?;
            self.modules[m as usize].parent = info.parent.and_then(|p| self.by_key.get(&p).copied());
            // Its name in its parent: `pub(in path)` walks the chain by name, reading no entries.
            self.modules[m as usize].name = info.name;
        }
        for &k in unread.iter() {
            let m = self.by_key[&k];
            self.modules[m as usize].chain.push(k);
        }
        // Chains only after every parent is set: modules are numbered in key order, not tree order.
        for &m in made.iter() {
            let mut at = Some(m);
            while let Some(a) = at {
                let k = self.modules[a as usize].key;
                self.modules[m as usize].chain.push(k);
                at = self.modules[a as usize].parent;
            }
        }
        self.stats.modules += made.len() as u64;
        Ok(made)
    }

    /// Loads the modules under `tops` not made yet, with their texts: the modules no `mod` line
    /// names (a block's module, made beside its body's module), which the caller is told of as
    /// the old fill is (`FillEnv::made`). Their imports and calls are queued: the caller runs
    /// again. How many were made.
    pub(crate) fn load_more<F: ScopeFacts<Name = N>>(&mut self, facts: &F, tops: &[ItemKey]) -> Result<usize, CellError> {
        let made = self.make_modules(facts, tops)?;
        for &m in made.iter() {
            self.read_text(facts, m)?;
        }
        Ok(made.len())
    }

    /// Reads module `m`'s own text.
    fn read_text<F: ScopeFacts<Name = N>>(&mut self, facts: &F, m: ModId) -> Result<(), CellError> {
        let key = self.modules[m as usize].key;
        let d = facts.decls(key).map_err(|error| CellError::Facts { module: key, error })?;
        if m == self.root {
            // `#[macro_export]` macros are public names of the crate root in the macro namespace
            // (Reference `macros.by-example.export`).
            let r = facts.crate_root(self.krate).map_err(|error| CellError::Facts { module: key, error })?;
            for &(name, def, shapes) in r.macro_exports.iter() {
                // The root's text fragment is the next one made (`read_fragment` below).
                let text = self.frags.len() as FragId;
                let entry = (Def { key: def, kind: DefKind::Macro(shapes) }, Visibility::Public, Origin::MacroExport, text);
                self.add_explicit(m, Namespace::Macros, name, entry)?;
                self.stats.names_declared += 1;
            }
            // The outer scopes (R6, R8): sorted already by the record's rule.
            self.outer.externs.extend_from_slice(&r.extern_prelude);
            self.outer.macro_use = !r.macro_use.is_empty();
            self.macro_use.extend_from_slice(&r.macro_use);
            self.edition = r.head.edition;
            if let Some(p) = r.head.prelude {
                // A standard prelude of a dependency is read like any module of it.
                let prelude = match self.by_key.get(&p) {
                    Some(&local) => Some(local),
                    None => self.foreign(p),
                };
                match prelude {
                    Some(m) => self.outer.local = Some(m),
                    None => {
                        let pd = facts.decls(p).map_err(|error| CellError::Facts { module: p, error })?;
                        for it in pd.items.iter() {
                            self.outer.foreign.entry((it.ns, it.name)).or_insert(it.def);
                        }
                        for c in pd.mods.iter() {
                            self.outer.foreign.entry((Namespace::Types, c.name)).or_insert(Def::module(c.module));
                        }
                        self.outer.foreign_open = !pd.uses.is_empty() || !pd.calls.is_empty();
                    }
                }
            }
        }
        let f = self.read_fragment(m, &d, None)?;
        self.modules[m as usize].text = f;
        Ok(())
    }

    /// An explicit entry of `m`; the first of a name stays. A cell of it decided not found is a
    /// bug in when cells decide (finality).
    fn add_explicit(&mut self, m: ModId, ns: Namespace, name: N, entry: Entry) -> Result<(), CellError> {
        self.check_final(m, ns, name)?;
        self.modules[m as usize].explicit.entry((ns, name)).or_insert(entry);
        Ok(())
    }

    fn check_final(&self, m: ModId, ns: Namespace, name: N) -> Result<(), CellError> {
        if let Some(&c) = self.cell_ids.get(&(m, cell_slot(ns, Layer::Full), name))
            && self.cells[c as usize].value == Some(Value::NotFound)
        {
            return Err(CellError::DecidedTooEarly { module: self.modules[m as usize].key, ns });
        }
        Ok(())
    }

    /// Adds one fragment of module `m` (its text, or the expansion of the call at `up`): its
    /// entries, imports, `macro_rules!` and calls. Its child modules are made already.
    fn read_fragment(&mut self, m: ModId, d: &DeclsBuilder<'_, N>, up: Option<(FragId, u32)>) -> Result<FragId, CellError> {
        let alloc = self.alloc;
        let fid = self.frags.len() as FragId;
        let mut slots: AVec<'a, (u32, Slot<N>)> = AVec::new_in(alloc);
        for it in d.items.iter() {
            let vis = self.vis_of(m, &it.vis)?;
            self.add_explicit(m, it.ns, it.name, (it.def, vis, Origin::Declared, fid))?;
            self.stats.names_declared += 1;
            if up.is_some() {
                self.wrote(m, it.ns, it.name, it.def, fid);
            }
        }
        for c in d.mods.iter() {
            let vis = self.vis_of(m, &c.vis)?;
            self.add_explicit(m, Namespace::Types, c.name, (Def::module(c.module), vis, Origin::Declared, fid))?;
            self.stats.names_declared += 1;
            if up.is_some() {
                self.wrote(m, Namespace::Types, c.name, Def::module(c.module), fid);
            }
            let Some(&child) = self.by_key.get(&c.module) else {
                return Err(CellError::Facts { module: c.module, error: SurfaceError::NoRecord { fact: "ModuleDecls", item: c.module } });
            };
            self.modules[child as usize].mod_line = Some((fid, c.pos));
            slots.push((c.pos, Slot::Mod(child, c.macro_use)));
        }
        for ec in d.extern_crates.iter() {
            let vis = self.vis_of(m, &ec.vis)?;
            self.extern_crate(m, ec.target, ec.rename, vis, fid, up.is_some())?;
        }
        for r in d.macro_rules.iter() {
            let def = Def { key: r.def, kind: DefKind::Macro(r.shapes) };
            slots.push((r.pos, Slot::Rules(r.name, def)));
            if r.export {
                // Also a crate-root name in the macro namespace; the root record already holds
                // those of module texts, so the same def adds nothing.
                let root = self.root;
                self.add_explicit(root, Namespace::Macros, r.name, (def, Visibility::Public, Origin::MacroExport, fid))?;
            }
        }
        for c in d.calls.iter() {
            let id = self.items.len() as ItemId;
            self.items.push(Item {
                module: m,
                index: u32::MAX,
                kind: c.path.kind,
                segments: AVec::from_slice_in(&c.path.segments, alloc),
                glob: false,
                binds: None,
                vis: Visibility::Crate(self.krate),
                outcome: None,
                waiters: AVec::new_in(alloc),
                waits: AVec::new_in(alloc),
                evaluations: 0,
                call: Some(CallAt { key: c.call, frag: fid, pos: c.pos }),
                grown: false,
                expansion: None,
                at: fid,
                pos: c.pos,
                of_enum: false,
                types_only: false,
                why: None,
            });
            let module = &mut self.modules[m as usize];
            module.calls.push(id);
            module.growth += 1;
            slots.push((c.pos, Slot::Call(id)));
            self.item_queue.insert(id);
            self.stats.calls += 1;
        }
        // Positions are unique in a fragment: no sort order is left open.
        slots.sort_unstable_by_key(|s| s.0);
        self.frags.push(Frag { module: m, up, slots });
        for u in d.uses.iter() {
            let vis = self.vis_of(m, &u.vis)?;
            let (glob, binds) = match u.leaf {
                UseForm::Glob => (true, None),
                UseForm::Named { rename, .. } => {
                    let leaf = u.path.segments.last().copied();
                    let binds = match rename {
                        Rename::Same => leaf,
                        Rename::To(to) => Some(to),
                        Rename::Underscore => None,
                    };
                    (false, binds)
                }
            };
            if let Some(n) = binds {
                for ns in NAMESPACES {
                    self.check_final(m, ns, n)?;
                }
            }
            // Imports are numbered through the module's text, then its expansions in the order
            // they are read: the glob edges are ordered by it.
            let index = self.modules[m as usize].uses;
            self.modules[m as usize].uses += 1;
            let id = self.items.len() as ItemId;
            self.item_queue.insert(id);
            self.items.push(Item {
                module: m,
                index,
                kind: u.path.kind,
                segments: AVec::from_slice_in(&u.path.segments, self.alloc),
                glob,
                binds,
                vis,
                outcome: None,
                waiters: AVec::new_in(self.alloc),
                waits: AVec::new_in(self.alloc),
                evaluations: 0,
                call: None,
                grown: false,
                expansion: None,
                at: fid,
                pos: u.pos,
                of_enum: false,
                types_only: false,
                why: None,
            });
            // `use a::b::{self};` binds `b` in the type namespace alone (Reference
            // `items.use.self`: "only the type namespace").
            if matches!(u.leaf, UseForm::Named { types_only: true, .. }) && self.plant != CellPlant::SelfImportBindsAll {
                self.items[id as usize].types_only = true;
            }
            let module = &mut self.modules[m as usize];
            if glob {
                module.globs_pending += 1;
                module.glob_items.push(id);
            } else if let Some(n) = binds {
                module.named.entry(n).or_insert_with(|| AVec::new_in(alloc)).push(id);
            }
            self.stats.items += 1;
        }
        Ok(fid)
    }

    /// One `extern crate` item of `m` (Reference `items.extern-crate`: "The external crate is
    /// then bound into the declaring scope as the identifier provided in the extern crate
    /// declaration"; `as _` binds nothing; `extern crate self` binds only with `as name`).
    /// `fid`: the fragment it is written in, `expanded`: an expansion's. One a macro wrote at the crate root also
    /// adds its name to the extern prelude (R8, Reference `names.preludes.extern`), never in
    /// place of a dependency's: `.. as n;` beside a dependency's `n` is R8's error (rustc's
    /// "macro-expanded `extern crate` items cannot shadow names passed with `--extern`"). The
    /// root text's own are in the root record's extern prelude already.
    fn extern_crate(&mut self, m: ModId, target: ExternTarget<N>, rename: Rename<N>, vis: Visibility, fid: FragId, expanded: bool) -> Result<(), CellError> {
        let (root, written) = match target {
            ExternTarget::Dep(name) => (self.dep_roots.binary_search_by(|d| d.0.cmp(&name)).ok().map(|k| self.dep_roots[k].1), Some(name)),
            ExternTarget::SelfCrate => (Some(self.modules[self.root as usize].key), None),
        };
        let bound = match rename {
            Rename::Same => written,
            Rename::To(name) => Some(name),
            Rename::Underscore => None,
        };
        let Some(root) = root else {
            if let Some(name) = written {
                self.bad_extern.push((self.modules[m as usize].key, name));
            }
            return Ok(());
        };
        let Some(name) = bound else { return Ok(()) };
        if self.plant == CellPlant::ExternCrateBindsNothing {
            return Ok(());
        }
        let def = Def::module(root);
        self.add_explicit(m, Namespace::Types, name, (def, vis, Origin::ExternCrate, fid))?;
        self.stats.names_declared += 1;
        if !expanded {
            return Ok(());
        }
        let f = fid;
        self.wrote(m, Namespace::Types, name, def, f);
        if m != self.root || self.plant == CellPlant::R8AddsNothing {
            return Ok(());
        }
        if self.outer.externs.binary_search_by(|e| e.0.cmp(&name)).is_ok() {
            if matches!(rename, Rename::To(_)) {
                self.r8.push(name);
                if self.plant == CellPlant::R8MacroReplaces {
                    self.outer.added.insert(name, root);
                }
            }
            return Ok(());
        }
        if self.outer.added.contains_key(&name) {
            return Ok(());
        }
        self.outer.added.insert(name, root);
        // A use that went past the extern prelude for this name while the root could still add it.
        if let Some(ws) = self.extern_watches.get(&name) {
            let ws: AVec<'a, (ItemId, Def)> = AVec::from_slice_in(ws, self.alloc);
            for &(u, od) in ws.iter() {
                if od != def && !self.encloses(f, self.items[u as usize].at) {
                    self.r6_error(u);
                }
            }
        }
        Ok(())
    }

    /// Expands call `i`, resolved: the driver expands it, the facts hand back what it writes,
    /// which is added to its module as a fragment at the call's place; then its growth ends.
    fn expand_call<F: ScopeFacts<Name = N>>(&mut self, facts: &F, expand: &dyn Fn(ItemKey, Def), i: ItemId) -> Result<(), CellError> {
        let (m, at, def) = match (self.items[i as usize].call, self.items[i as usize].outcome) {
            (Some(at), Some(Outcome::Called(def))) => (self.items[i as usize].module, at, def),
            _ => return Ok(()),
        };
        expand(at.key, def);
        let d = match facts.expansion(at.key) {
            Ok(Some(d)) => d,
            Ok(None) => return Err(CellError::NotExpanded { call: at.key }),
            Err(error) => return Err(CellError::Expansion { call: at.key, error }),
        };
        let mut tops: AVec<'a, ItemKey> = AVec::new_in(self.alloc);
        for c in d.mods.iter() {
            tops.push(c.module);
        }
        let made = self.make_modules(facts, &tops)?;
        let f = self.read_fragment(m, &d, Some((at.frag, at.pos)))?;
        for &n in made.iter() {
            self.read_text(facts, n)?;
        }
        self.items[i as usize].expansion = Some(f);
        self.stats.expansions += 1;
        self.grown(i);
        Ok(())
    }

    /// Call `i` has added all it writes: its module's growth falls by one, its waiters wake.
    fn grown(&mut self, i: ItemId) {
        let it = &mut self.items[i as usize];
        it.grown = true;
        let m = it.module;
        let waiters = core::mem::replace(&mut it.waiters, AVec::new_in(self.alloc));
        self.modules[m as usize].growth -= 1;
        for &w in waiters.iter() {
            self.wake(w);
        }
    }

    // -----------------------------------------------------------------------------------------
    // Visibility (R4).

    fn vis_of(&self, m: ModId, v: &WrittenVis<Path<'_, N>>) -> Result<Visibility, CellError> {
        let module = &self.modules[m as usize];
        let bad = |why| CellError::BadVisibility { module: module.key, why };
        let at = match v {
            WrittenVis::Pub => return Ok(Visibility::Public),
            WrittenVis::Crate => return Ok(Visibility::Crate(self.krate)),
            WrittenVis::Private | WrittenVis::SelfOnly => m,
            WrittenVis::Super => module.parent.ok_or(bad(BadVis::AboveRoot))?,
            WrittenVis::WithoutIn(_) => return Err(bad(BadVis::WithoutIn)),
            WrittenVis::In(p) => {
                let mut at = match p.kind {
                    PathKind::Crate => self.root,
                    PathKind::SelfMod => m,
                    PathKind::Super(n) => self.up(m, n).ok_or(bad(BadVis::AboveRoot))?,
                    // Reference `vis.scoped.edition2018`: "The 2015 edition may also use paths
                    // starting with `::` or modules from the crate root."
                    PathKind::Plain | PathKind::Abs if self.edition == Edition::E2015 && self.plant != CellPlant::Use2015Lexical => self.root,
                    PathKind::Plain | PathKind::Abs | PathKind::DollarCrate(_) => return Err(bad(BadVis::Start)),
                };
                // The target must be an ancestor (Reference `vis.scoped.in`): each segment names
                // the next module down the asker's own chain, so the walk reads no entries.
                let start = self.modules[at as usize].key;
                let mut i = module.chain.iter().position(|&k| k == start).ok_or(bad(BadVis::Segment(0)))?;
                for (j, s) in p.segments.iter().enumerate() {
                    let next = i.checked_sub(1).ok_or(bad(BadVis::Segment(j)))?;
                    let down = *self.by_key.get(&module.chain[next]).ok_or(bad(BadVis::Segment(j)))?;
                    if self.modules[down as usize].name != Some(*s) {
                        return Err(bad(BadVis::Segment(j)));
                    }
                    at = down;
                    i = next;
                }
                at
            }
        };
        Ok(if at == self.root { Visibility::Crate(self.krate) } else { Visibility::Restricted(self.modules[at as usize].key) })
    }

    fn up(&self, m: ModId, n: u8) -> Option<ModId> {
        let mut at = m;
        for _ in 0..n {
            at = self.modules[at as usize].parent?;
        }
        Some(at)
    }

    fn visible(&self, v: Visibility, from: ModId) -> bool {
        match v {
            Visibility::Public => true,
            Visibility::Crate(k) => k == self.krate,
            Visibility::Restricted(a) => self.modules[from as usize].chain.contains(&a),
        }
    }

    /// Larger is narrower seen from `from`; one `from` cannot see is narrowest.
    fn rank(&self, v: Visibility, from: ModId) -> usize {
        match v {
            Visibility::Public => 0,
            Visibility::Crate(_) => 1,
            Visibility::Restricted(a) => {
                let chain = &self.modules[from as usize].chain;
                match chain.iter().position(|&k| k == a) {
                    Some(i) => 2 + chain.len() - i,
                    None => usize::MAX,
                }
            }
        }
    }

    fn narrower(&self, a: Visibility, b: Visibility, from: ModId) -> Visibility {
        if self.rank(b, from) > self.rank(a, from) { b } else { a }
    }

    fn wider(&self, a: Visibility, b: Visibility, from: ModId) -> Visibility {
        if self.rank(b, from) < self.rank(a, from) { b } else { a }
    }

    // -----------------------------------------------------------------------------------------
    // Cells.

    /// The module `key` of this crate, or of a dependency (made here once, its cells decided from
    /// the record); `None` when no dependency read here holds it (its record absent): unknown.
    fn module_at(&mut self, key: ItemKey) -> Option<ModId> {
        if let Some(&m) = self.by_key.get(&key) {
            return Some(m);
        }
        let found = self.foreign(key);
        if found.is_none() {
            self.stats.dep_absent += 1;
        }
        found
    }

    fn foreign(&mut self, key: ItemKey) -> Option<ModId> {
        if let Some(&m) = self.by_key.get(&key) {
            return Some(m);
        }
        let deps = self.deps?;
        if self.plant == CellPlant::DepIsOutside || !deps.has_module(key) {
            return None;
        }
        let id = self.modules.len() as ModId;
        let alloc = self.alloc;
        self.by_key.insert(key, id);
        let mut chain = AVec::new_in(alloc);
        chain.push(key);
        self.modules.push(Module {
            key,
            name: None,
            parent: None,
            chain,
            explicit: ABTreeMap::new_in(alloc),
            named: ABTreeMap::new_in(alloc),
            globs_pending: 0,
            glob_items: AVec::new_in(alloc),
            edges: ABTreeMap::new_in(alloc),
            glob_cells: AVec::new_in(alloc),
            calls: AVec::new_in(alloc),
            growth: 0,
            text: FragId::MAX,
            mod_line: None,
            uses: 0,
            foreign: true,
            unread: false,
        });
        Some(id)
    }

    fn cell(&mut self, m: ModId, ns: Namespace, name: N, layer: Layer) -> CellId {
        let key = (m, cell_slot(ns, layer), name);
        if let Some(&c) = self.cell_ids.get(&key) {
            return c;
        }
        let id = self.cells.len() as CellId;
        if self.modules[m as usize].unread {
            // Its text was never read: unknown, decided when made.
            self.cells.push(Cell { module: m, ns, name, layer, value: Some(Value::Outside), waiters: AVec::new_in(self.alloc), waiting_on: None });
            self.cell_ids.insert(key, id);
            self.stats.cells += 1;
            self.stats.decisions += 1;
            return id;
        }
        if self.modules[m as usize].foreign
            && let Some(deps) = self.deps
        {
            // Decided already, in the dependency: read, never waited on.
            let value = deps.name_in(self.modules[m as usize].key, ns, name).value();
            self.cells.push(Cell { module: m, ns, name, layer, value: Some(value), waiters: AVec::new_in(self.alloc), waiting_on: None });
            self.cell_ids.insert(key, id);
            self.stats.cells += 1;
            self.stats.decisions += 1;
            self.stats.dep_reads += 1;
            return id;
        }
        self.cells.push(Cell { module: m, ns, name, layer, value: None, waiters: AVec::new_in(self.alloc), waiting_on: None });
        self.cell_ids.insert(key, id);
        if layer == Layer::Globs {
            self.modules[m as usize].glob_cells.push(id);
        }
        self.cell_queue.insert(id);
        self.stats.cells += 1;
        id
    }

    fn decide(&mut self, c: CellId, v: Value) {
        let cell = &mut self.cells[c as usize];
        debug_assert!(cell.value.is_none(), "a cell is decided once");
        cell.value = Some(v);
        cell.waiting_on = None;
        self.stats.decisions += 1;
        let waiters = core::mem::replace(&mut cell.waiters, AVec::new_in(self.alloc));
        for w in waiters.iter() {
            self.wake(*w);
        }
    }

    fn wake(&mut self, n: Node) {
        self.stats.wakes += 1;
        match n {
            Node::Cell(c) => {
                self.cell_queue.insert(c);
            }
            Node::Item(i) => {
                self.item_queue.insert(i);
            }
        }
    }

    /// `c` waits on `on`: one waiter record, unless it already waits there.
    fn cell_waits(&mut self, c: CellId, on: Node) {
        // A Globs cell with globs pending is woken by its module, not by a waiter record.
        if self.cells[c as usize].waiting_on == Some(on) || on == Node::Cell(c) {
            return;
        }
        self.cells[c as usize].waiting_on = Some(on);
        self.stats.waiters += 1;
        match on {
            Node::Cell(d) => self.cells[d as usize].waiters.push(Node::Cell(c)),
            Node::Item(i) => self.items[i as usize].waiters.push(Node::Cell(c)),
        }
    }

    /// One try at deciding `c` from its inputs.
    fn step_cell(&mut self, c: CellId) {
        if self.cells[c as usize].value.is_some() {
            return;
        }
        match self.cell_value(c, None, Skip::No) {
            Step::Done(v) => self.decide(c, v),
            Step::Wait(on) => self.cell_waits(c, on),
        }
    }

    /// `c`'s value from its inputs; `guess` gives the current value of the cells of a cycle.
    /// `skip`: a call reading its own module's `Globs` layer, whose own growth it does not wait
    /// on (the asker never waits on itself); the value is then that call's view, never kept.
    fn cell_value(&mut self, c: CellId, guess: Option<&Guess<'a>>, skip: Skip) -> Step<Value> {
        let Cell { module: m, ns, name, layer, .. } = self.cells[c as usize];
        match layer {
            Layer::Full => {
                if self.plant == CellPlant::GlobsShadowExplicit {
                    let g = self.cell(m, ns, name, Layer::Globs);
                    match self.current(g, guess) {
                        Some(v @ (Value::Found(..) | Value::Ambiguous(..) | Value::Outside)) => return Step::Done(v),
                        Some(Value::NotFound) => {}
                        None => return Step::Wait(Node::Cell(g)),
                    }
                }
                if let Some(&(def, vis, ..)) = self.modules[m as usize].explicit.get(&(ns, name)) {
                    return Step::Done(Value::Found(def, vis));
                }
                let mut found = None;
                let mut outside = false;
                if let Some(named) = self.modules[m as usize].named.get(&name) {
                    for &i in named.iter() {
                        let o = match (self.items[i as usize].outcome, guess.and_then(|g| g.items.get(&i))) {
                            (Some(o), _) => Some(o),
                            // A member of the group: its guess, absent while at the bottom.
                            (None, Some(&o)) => o,
                            (None, None) => return Step::Wait(Node::Item(i)),
                        };
                        match o {
                            None => {}
                            Some(Outcome::Named(b)) => {
                                if found.is_none() {
                                    found = b[ns_at(ns)];
                                }
                            }
                            Some(Outcome::Outside) if ns == Namespace::Macros && self.items[i as usize].of_enum => {}
                            Some(Outcome::Outside) => outside = self.plant != CellPlant::OutsideIsNotFound,
                            Some(_) => {}
                        }
                    }
                }
                if let Some((def, vis)) = found {
                    return Step::Done(Value::Found(def, vis));
                }
                if outside {
                    return Step::Done(Value::Outside);
                }
                let g = self.cell(m, ns, name, Layer::Globs);
                match self.current(g, guess) {
                    Some(v) => Step::Done(v),
                    None => Step::Wait(Node::Cell(g)),
                }
            }
            Layer::Globs => {
                if self.modules[m as usize].globs_pending > 0 && guess.is_none() {
                    if skip != Skip::No {
                        // A call's view waits on one pending glob, which wakes it.
                        for &g in self.modules[m as usize].glob_items.iter() {
                            if self.items[g as usize].outcome.is_none() {
                                return Step::Wait(Node::Item(g));
                            }
                        }
                    }
                    // Woken again when a glob of the module is decided.
                    return Step::Wait(Node::Cell(c));
                }
                // The glob edges by use index: the resolved ones, and under a guess the group's
                // pending globs as guessed (one at the bottom brings nothing yet).
                let mut edges: AVec<'a, (ModId, Visibility)> = AVec::new_in(self.alloc);
                {
                    let module = &self.modules[m as usize];
                    let (mut r, mut p) = (module.edges.iter().peekable(), module.glob_items.iter().peekable());
                    loop {
                        let next_r = r.peek().map(|e| *e.0);
                        let next_p = p.peek().map(|i| self.items[**i as usize].index);
                        match (next_r, next_p) {
                            (None, None) => break,
                            (Some(k), q) if q.is_none_or(|q| k < q) => {
                                if let Some((_, &x)) = r.next() {
                                    edges.push(x);
                                }
                            }
                            _ => {
                                let Some(&i) = p.next() else { break };
                                let it = &self.items[i as usize];
                                if it.outcome.is_some() {
                                    continue; // its edge, if any, is in `edges`
                                }
                                match guess.map(|g| g.items.get(&i)) {
                                    Some(Some(Some(Outcome::Glob(src)))) => {
                                        if let Some(&s) = self.by_key.get(src) {
                                            edges.push((s, it.vis));
                                        }
                                    }
                                    Some(Some(_)) => {}
                                    // A pending glob outside the group: an input not decided.
                                    Some(None) => return Step::Wait(Node::Item(i)),
                                    None => {}
                                }
                            }
                        }
                    }
                }
                // Every source cell is made first, so the quiet point sees each input of this
                // cell (one left unmade would hide a source from a cycle's fixed point).
                let mut waits_on = None;
                for &(src, _) in edges.iter() {
                    let s = self.cell(src, ns, name, Layer::Full);
                    if waits_on.is_none() && self.current(s, guess).is_none() {
                        waits_on = Some(s);
                    }
                }
                if let Some(s) = waits_on {
                    return Step::Wait(Node::Cell(s));
                }
                let mut acc: Option<Value> = None;
                // A glob of the module whose path stage 1 does not follow brings what is unknown.
                let mut outside = self.modules[m as usize]
                    .glob_items
                    .iter()
                    .any(|&g| self.items[g as usize].outcome == Some(Outcome::Outside) && !(ns == Namespace::Macros && self.items[g as usize].of_enum));
                for &(src, evis) in edges.iter() {
                    let s = self.cell(src, ns, name, Layer::Full);
                    let v = self.current(s, guess).unwrap_or(Value::NotFound);
                    let (def, vis, amb) = match v {
                        Value::NotFound => continue,
                        Value::Outside => {
                            outside = true;
                            continue;
                        }
                        Value::Found(d, v) => (d, v, false),
                        Value::Ambiguous(d, v) => (d, v, true),
                    };
                    if !self.visible(vis, m) && self.plant != CellPlant::GlobLeaksPrivate {
                        continue;
                    }
                    let vis = self.narrower(evis, vis, m);
                    if self.plant == CellPlant::FirstGlobWins && acc.is_some() {
                        continue;
                    }
                    acc = Some(match acc {
                        None | Some(Value::NotFound | Value::Outside) => if amb { Value::Ambiguous(def, vis) } else { Value::Found(def, vis) },
                        Some(Value::Found(d, v)) if d == def => {
                            let w = self.wider(v, vis, m);
                            if amb { Value::Ambiguous(d, w) } else { Value::Found(d, w) }
                        }
                        Some(Value::Ambiguous(d, v)) if d == def => Value::Ambiguous(d, self.wider(v, vis, m)),
                        Some(Value::Found(d, v) | Value::Ambiguous(d, v)) => Value::Ambiguous(if d < def { d } else { def }, v),
                    });
                }
                // Growth: a call of the module not expanded yet may still write the name, always
                // outside the macro namespace, and in it while the globs bring nothing (the
                // reference's wait relation). Under a guess a call of the group adds nothing.
                let brought = matches!(acc, Some(Value::Found(..) | Value::Ambiguous(..)));
                if self.plant != CellPlant::CallsNoGrowth && !(ns == Namespace::Macros && brought) {
                    for k in 0..self.modules[m as usize].calls.len() {
                        let j = self.modules[m as usize].calls[k];
                        if skip == Skip::Call(j) || skip == Skip::Growth || self.items[j as usize].grown || guess.is_some_and(|g| g.items.contains_key(&j)) {
                            continue;
                        }
                        return Step::Wait(Node::Item(j));
                    }
                }
                Step::Done(acc.unwrap_or(if outside { Value::Outside } else { Value::NotFound }))
            }
        }
    }

    /// A cell's value: decided, or a member's guess (the bottom is not found); `None` while it is
    /// undecided and outside the group.
    fn current(&self, c: CellId, guess: Option<&Guess<'a>>) -> Option<Value> {
        match self.cells[c as usize].value {
            Some(v) => Some(v),
            None => guess.and_then(|g| g.cells.get(&c)).map(|v| v.unwrap_or(Value::NotFound)),
        }
    }

    // -----------------------------------------------------------------------------------------
    // Items.

    /// A final read of a cell by item `i`.
    /// Under a guess (a group's fixed point) nothing is decided: the read takes the guess.
    fn read(&mut self, m: ModId, ns: Namespace, name: N, layer: Layer, guess: Option<&Guess<'a>>) -> Read {
        let c = self.cell(m, ns, name, layer);
        if guess.is_none() {
            // A new cell is tried at once, so a read of a decidable cell does not wait.
            self.drain_cells();
        }
        match self.current(c, guess) {
            Some(v) => Read::Ready(v),
            None => Read::Wait(c),
        }
    }

    /// Runs item `i` from the top.
    fn eval(&mut self, i: ItemId) {
        if self.items[i as usize].outcome.is_some() {
            return;
        }
        self.items[i as usize].evaluations += 1;
        self.stats.evaluations += 1;
        let ev = self.items[i as usize].evaluations;
        if ev > self.stats.max_evaluations {
            self.stats.max_evaluations = ev;
        }
        self.items[i as usize].waits.clear();
        match self.try_item(i, None) {
            Step::Done(o) => self.settle(i, o),
            Step::Wait(_) => {
                let waits = core::mem::replace(&mut self.items[i as usize].waits, AVec::new_in(self.alloc));
                for w in waits.iter() {
                    self.stats.waiters += 1;
                    match *w {
                        Node::Cell(c) => self.cells[c as usize].waiters.push(Node::Item(i)),
                        Node::Item(j) => self.items[j as usize].waiters.push(Node::Item(i)),
                    }
                }
                self.items[i as usize].waits = waits;
            }
        }
    }

    fn try_item(&mut self, i: ItemId, guess: Option<&Guess<'a>>) -> Step<Outcome> {
        let (m, kind, glob, binds, ivis) = {
            let it = &self.items[i as usize];
            (it.module, it.kind, it.glob, it.binds, it.vis)
        };
        // Reference `items.use.path.edition2018`: "In the 2015 edition, `use` paths are relative
        // to the crate root." and "The 2015 edition does not allow use declarations to reference
        // the extern prelude." So a 2015 import's plain path is `crate::path`: no lexical first
        // segment, no outer scope. A macro call's path stays lexical in every edition.
        let kind = if kind == PathKind::Plain && self.edition == Edition::E2015 && self.items[i as usize].call.is_none() && self.plant != CellPlant::Use2015Lexical {
            PathKind::Crate
        } else {
            kind
        };
        if let Some(at) = self.items[i as usize].call
            && kind == PathKind::Plain
            && self.items[i as usize].segments.len() == 1
            && self.plant != CellPlant::NoTextualScope
        {
            // A one-segment call looks in the textual scope first (Reference `macros.by-example.scope`).
            let name = self.items[i as usize].segments[0];
            match self.textual(at.frag, at.pos, name, guess) {
                Step::Done(Some(def)) => return Step::Done(Outcome::Called(def)),
                Step::Done(None) => {}
                Step::Wait(n) => {
                    self.items[i as usize].waits.push(n);
                    return Step::Wait(n);
                }
            }
        }
        // The first segment the walk reads (`::dep` reads its crate from the extern prelude).
        let mut first = 0;
        let mut cur = match kind {
            PathKind::Crate => self.root,
            PathKind::SelfMod => m,
            PathKind::Super(n) => match self.up(m, n) {
                Some(p) => p,
                None => return Step::Done(Outcome::Failed(Unresolved::NoParent)),
            },
            PathKind::Plain => m,
            // `$crate` names the crate the macro was defined in (Reference `macro.decl.hygiene`).
            PathKind::DollarCrate(k) if k == self.krate || self.plant == CellPlant::DollarCrateIsRoot => self.root,
            PathKind::DollarCrate(k) => match self.deps.and_then(|d| d.crate_root(k)) {
                Some(r) => match self.module_at(r) {
                    Some(r) => r,
                    None => return self.unfollowed(i, Unfollowed::CrateUnread),
                },
                None => {
                    self.stats.dep_absent += 1;
                    return self.unfollowed(i, Unfollowed::DollarCrateUnknown);
                }
            },
            // `::name` from 2018 on: `name` is a crate of the extern prelude (Reference
            // `paths.qualifiers.global-root`); in 2015 the crate root.
            PathKind::Abs if self.edition == Edition::E2015 => self.root,
            PathKind::Abs => {
                let len = self.items[i as usize].segments.len();
                if len < 2 && !glob {
                    return self.unfollowed(i, Unfollowed::CrateAlone);
                }
                let s = self.items[i as usize].segments[0];
                let named = match self.outer.externs.binary_search_by(|e| e.0.cmp(&s)) {
                    Ok(k) => Some(self.outer.externs[k].1),
                    Err(_) => self.outer.added.get(&s).copied(),
                };
                let Some(named) = named else {
                    // A root call may still write `extern crate .. as s;` (R8).
                    if let Some(j) = self.growing(self.root, i, guess) {
                        self.items[i as usize].waits.push(Node::Item(j));
                        return Step::Wait(Node::Item(j));
                    }
                    return Step::Done(Outcome::Failed(Unresolved::NotFound));
                };
                first = 1;
                match self.module_at(named) {
                    Some(r) => r,
                    None => return self.unfollowed(i, Unfollowed::CrateUnread),
                }
            }
        };
        let len = self.items[i as usize].segments.len();
        let steps = if glob { len } else { len.saturating_sub(1) };
        for k in first..steps {
            let s = self.items[i as usize].segments[k];
            let v = if kind == PathKind::Plain && k == 0 {
                match self.first_segment(m, Namespace::Types, s, i, guess) {
                    Step::Done(v) => v,
                    Step::Wait(n) => {
                        self.items[i as usize].waits.push(n);
                        return Step::Wait(n);
                    }
                }
            } else {
                match self.read(cur, Namespace::Types, s, Layer::Full, guess) {
                    Read::Ready(v) => v,
                    Read::Wait(c) => {
                        self.items[i as usize].waits.push(Node::Cell(c));
                        return Step::Wait(Node::Cell(c));
                    }
                }
            };
            match v {
                Value::Outside => return self.unfollowed(i, Unfollowed::Through),
                Value::NotFound => return Step::Done(Outcome::Failed(Unresolved::NotFound)),
                Value::Found(_, vis) | Value::Ambiguous(_, vis) if !self.visible(vis, m) => {
                    return Step::Done(Outcome::Failed(Unresolved::Private));
                }
                Value::Ambiguous(..) => return Step::Done(Outcome::Failed(Unresolved::Ambiguous(AmbiguityKind::GlobVsGlob))),
                Value::Found(def, _) if def.kind != DefKind::Module => {
                    return Step::Done(if def.kind == DefKind::Enum {
                        // Through an enum: its variants, unread here, are types and values,
                        // never macros: the import is unknown in those two namespaces alone.
                        self.items[i as usize].of_enum = self.plant != CellPlant::EnumHidesMacros;
                        self.items[i as usize].why = Some(Unfollowed::Enum);
                        Outcome::Outside
                    } else {
                        Outcome::Failed(Unresolved::NotAModule)
                    });
                }
                Value::Found(def, _) => match self.module_at(def.key) {
                    Some(next) => cur = next,
                    None => return self.unfollowed(i, Unfollowed::ModuleUnread),
                },
            }
        }
        if glob {
            return Step::Done(Outcome::Glob(self.modules[cur as usize].key));
        }
        let Some(&leaf) = self.items[i as usize].segments.last() else {
            return Step::Done(Outcome::Failed(Unresolved::NotFound));
        };
        if self.items[i as usize].call.is_some() {
            // The path-based scope of a call: the leaf in the macro namespace. In its own module
            // the call reads its own view, which does not wait on its own growth.
            let v = if kind == PathKind::Plain && len == 1 {
                // A one-segment call the textual scope missed: lexically, R6.
                self.first_segment(m, Namespace::Macros, leaf, i, guess)
            } else if cur == m {
                match self.own_view(m, Namespace::Macros, leaf, i, Skip::Call(i), guess) {
                    Step::Done((v, _)) => Step::Done(v),
                    Step::Wait(n) => Step::Wait(n),
                }
            } else {
                match self.read(cur, Namespace::Macros, leaf, Layer::Full, guess) {
                    Read::Ready(v) => Step::Done(v),
                    Read::Wait(c) => Step::Wait(Node::Cell(c)),
                }
            };
            return match v {
                Step::Wait(n) => {
                    self.items[i as usize].waits.push(n);
                    Step::Wait(n)
                }
                Step::Done(Value::Outside) => self.unfollowed(i, Unfollowed::Leaf),
                Step::Done(Value::NotFound) => Step::Done(Outcome::Failed(Unresolved::NotFound)),
                Step::Done(Value::Found(_, vis) | Value::Ambiguous(_, vis)) if !self.visible(vis, m) => {
                    Step::Done(Outcome::Failed(Unresolved::Private))
                }
                Step::Done(Value::Ambiguous(..)) => Step::Done(Outcome::Failed(Unresolved::Ambiguous(AmbiguityKind::GlobVsGlob))),
                Step::Done(Value::Found(def, _)) => Step::Done(Outcome::Called(def)),
            };
        }
        // An import never justifies itself: of its own name in its own module it reads the globs.
        let layer = if cur == m && binds == Some(leaf) { Layer::Globs } else { Layer::Full };
        let mut out = [None; 3];
        let (mut found, mut hidden, mut amb, mut wait, mut outside) = (false, false, false, None, false);
        let lexical = kind == PathKind::Plain && len == 1;
        let mut textual_open = false;
        let types_only = self.items[i as usize].types_only;
        for ns in NAMESPACES {
            if types_only && ns != Namespace::Types {
                continue;
            }
            let r = if lexical {
                // `use name;` in the macro namespace reads the textual scope at the import
                // first (Reference `macro.decl.scope.path.reexport`: "Macros can be re-exported
                // to give them path-based scope from a module other than the crate root.",
                // its example `macro_rules! m { .. } pub(crate) use m;` "from within `m`'s
                // textual scope"). The binding's own visibility is the macro's (Reference
                // `macro.decl.scope.path-based.visibility`: "Macros have an implicit visibility
                // of `pub(crate)`. `#[macro_export]` changes the implicit visibility to `pub`.").
                let tex = if ns == Namespace::Macros && self.plant != CellPlant::NoTextualImport {
                    let (frag, pos) = (self.items[i as usize].at, self.items[i as usize].pos);
                    match self.textual(frag, pos, leaf, guess) {
                        Step::Done(d) => d,
                        Step::Wait(n) => {
                            self.items[i as usize].waits.push(n);
                            wait = wait.or(Some(n));
                            continue;
                        }
                    }
                } else {
                    None
                };
                if self.plant == CellPlant::NoTextualImport && ns == Namespace::Macros && self.has_rules(m, leaf) {
                    textual_open = true;
                }
                match tex {
                    Some(def) => {
                        let exported = self.modules[self.root as usize].explicit.get(&(Namespace::Macros, leaf)).is_some_and(|e| e.0 == def && e.2 == Origin::MacroExport);
                        Read::Ready(Value::Found(def, if exported { Visibility::Public } else { Visibility::Crate(self.krate) }))
                    }
                    // Else the leaf is a first segment (R6-R8), in each namespace.
                    None => match self.first_segment(m, ns, leaf, i, guess) {
                        Step::Done(v) => Read::Ready(v),
                        Step::Wait(n) => {
                            self.items[i as usize].waits.push(n);
                            wait = wait.or(Some(n));
                            continue;
                        }
                    },
                }
            } else {
                self.read(cur, ns, leaf, layer, guess)
            };
            match r {
                Read::Wait(c) => {
                    self.items[i as usize].waits.push(Node::Cell(c));
                    wait = wait.or(Some(Node::Cell(c)));
                }
                Read::Ready(Value::NotFound) => {}
                Read::Ready(Value::Outside) => outside = true,
                Read::Ready(Value::Found(_, vis) | Value::Ambiguous(_, vis)) if !self.visible(vis, m) => hidden = true,
                Read::Ready(Value::Ambiguous(..)) => amb = true,
                Read::Ready(Value::Found(def, vis)) => {
                    found = true;
                    // A binding of an `extern crate` item is re-exported at the `use`'s own
                    // visibility (rustc's `Resolver::import`: `pub use core as core_;` over the
                    // private injected `extern crate core;` is a public re-export, a lint at most).
                    let through = if lexical { m } else { cur };
                    let of_extern = ns == Namespace::Types
                        && self.plant != CellPlant::ExternReexportNarrowed
                        && self.modules[through as usize].explicit.get(&(ns, leaf)).is_some_and(|e| e.0 == def && e.2 == Origin::ExternCrate);
                    out[ns_at(ns)] = Some((def, if of_extern { ivis } else { self.narrower(ivis, vis, m) }));
                }
            }
        }
        if amb {
            // An ambiguous namespace fails the import at once (R2).
            return Step::Done(Outcome::Failed(Unresolved::Ambiguous(AmbiguityKind::GlobVsGlob)));
        }
        if let Some(w) = wait {
            return Step::Wait(w);
        }
        // A namespace this core cannot answer leaves the whole import unknown.
        if outside || textual_open {
            return self.unfollowed(i, if outside { Unfollowed::Leaf } else { Unfollowed::TextualImport });
        }
        Step::Done(if found {
            Outcome::Named(out)
        } else if hidden {
            Outcome::Failed(Unresolved::Private)
        } else {
            Outcome::Failed(Unresolved::NotFound)
        })
    }

    /// Item `i` is unknown here, and why, by name.
    fn unfollowed(&mut self, i: ItemId, why: Unfollowed) -> Step<Outcome> {
        self.items[i as usize].why = Some(why);
        Step::Done(Outcome::Outside)
    }

    /// `name!` in the macro namespace of `m`, read by `m`'s own call `me`: the cell when decided,
    /// else the cell's rule with `me`'s own growth left out (not kept: it is `me`'s view alone).
    /// Module `m`'s own view of `name` in `ns` for item `me`, leaving `skip` out of its growth:
    /// an item or `mod`, else a named import (a pending one is waited for, R7), else the globs.
    /// With the view, where it came from: `Some(fragment)` for an item an expansion wrote,
    /// `Own` for the rest of `m`'s own bindings, `Glob` through a glob.
    fn own_view(&mut self, m: ModId, ns: Namespace, name: N, me: ItemId, skip: Skip, guess: Option<&Guess<'a>>) -> Step<(Value, From)> {
        let c = self.cell(m, ns, name, Layer::Full);
        if skip != Skip::Growth
            && let Some(v) = self.current(c, guess)
        {
            return Step::Done((v, From::Own));
        }
        if let Some(&(def, vis, ..)) = self.modules[m as usize].explicit.get(&(ns, name)) {
            let from = self.written.get(&(m, ns, name)).map_or(From::Own, |&f| From::Wrote(f));
            return Step::Done((Value::Found(def, vis), from));
        }
        let (mut found, mut outside) = (None, false);
        let named_count = self.modules[m as usize].named.get(&name).map_or(0, |v| v.len());
        for k in 0..named_count {
            let j = self.modules[m as usize].named[&name][k];
            if j == me {
                continue; // an import never justifies itself
            }
            let o = match (self.items[j as usize].outcome, guess.and_then(|g| g.items.get(&j))) {
                (Some(o), _) => Some(o),
                (None, Some(&o)) => o,
                (None, None) => return Step::Wait(Node::Item(j)),
            };
            match o {
                Some(Outcome::Named(b)) if found.is_none() => found = b[ns_at(ns)],
                Some(Outcome::Outside) => outside = true,
                _ => {}
            }
        }
        if let Some((def, vis)) = found {
            return Step::Done((Value::Found(def, vis), From::Own));
        }
        if outside {
            return Step::Done((Value::Outside, From::Own));
        }
        let g = self.cell(m, ns, name, Layer::Globs);
        if skip != Skip::Growth
            && let Some(v) = self.current(g, guess)
        {
            return Step::Done((v, From::Glob));
        }
        match self.cell_value(g, guess, skip) {
            Step::Done(v) => Step::Done((v, From::Glob)),
            Step::Wait(n) => Step::Wait(n),
        }
    }

    /// The first call of `m` not grown yet that may still write (not `me`, not one of a group
    /// under a guess, where a call adds nothing).
    fn growing(&self, m: ModId, me: ItemId, guess: Option<&Guess<'a>>) -> Option<ItemId> {
        self.modules[m as usize].calls.iter().copied().find(|&j| j != me && !self.items[j as usize].grown && !guess.is_some_and(|g| g.items.contains_key(&j)))
    }

    /// The outer candidate of `name` in `ns` (R6, R8): the extern prelude (types), the
    /// `#[macro_use]` prelude (macros), the standard prelude. `Some(Value::Outside)` when a scope
    /// of another crate this run cannot see may hold it. Built-in types are a type position's
    /// (R9), not a path's first segment.
    fn outer_of(&mut self, ns: Namespace, name: N, guess: Option<&Guess<'a>>) -> Step<Option<Value>> {
        if ns == Namespace::Types
            && self.plant == CellPlant::R8MacroReplaces
            && let Some(&k) = self.outer.added.get(&name)
        {
            return Step::Done(Some(Value::Found(Def::module(k), Visibility::Public)));
        }
        if ns == Namespace::Types
            && let Ok(k) = self.outer.externs.binary_search_by(|e| e.0.cmp(&name))
        {
            return Step::Done(Some(Value::Found(Def::module(self.outer.externs[k].1), Visibility::Public)));
        }
        if ns == Namespace::Types
            && let Some(&k) = self.outer.added.get(&name)
        {
            return Step::Done(Some(Value::Found(Def::module(k), Visibility::Public)));
        }
        if ns == Namespace::Macros && self.outer.macro_use {
            // The `#[macro_use]` prelude: the macros each such crate's root exports, the first
            // crate's that has the name (Reference `names.preludes.macro_use`).
            let mut unknown = false;
            for k in 0..self.macro_use.len() {
                let Some(r) = self.module_at(self.macro_use[k]) else {
                    unknown = true;
                    continue;
                };
                match self.read(r, ns, name, Layer::Full, guess) {
                    Read::Ready(v @ (Value::Found(_, Visibility::Public) | Value::Ambiguous(_, Visibility::Public))) => return Step::Done(Some(v)),
                    Read::Ready(Value::Outside) => unknown = true,
                    Read::Ready(_) => {}
                    Read::Wait(c) => return Step::Wait(Node::Cell(c)),
                }
            }
            if unknown {
                return Step::Done(Some(Value::Outside));
            }
        }
        if let Some(p) = self.outer.local {
            // A name the standard prelude lacks goes on to the language prelude.
            return match self.read(p, ns, name, Layer::Full, guess) {
                Read::Wait(c) => Step::Wait(Node::Cell(c)),
                Read::Ready(Value::NotFound) if self.plant != CellPlant::NoLanguagePrelude => Step::Done(self.builtin(ns, name)),
                Read::Ready(Value::NotFound) => Step::Done(None),
                Read::Ready(v) => Step::Done(Some(v)),
            };
        }
        if let Some(&def) = self.outer.foreign.get(&(ns, name)) {
            return Step::Done(Some(Value::Found(def, Visibility::Public)));
        }
        if self.outer.foreign_open {
            return Step::Done(Some(Value::Outside));
        }
        Step::Done(self.builtin(ns, name))
    }

    /// The last scope, the language prelude: a built-in type (core's `pub use bool;` in
    /// `core::primitive`). The language's built-in bang macros are not in it: they are items of
    /// `core::macros::builtin`, reached through the standard prelude (Reference, "Preludes").
    fn builtin(&self, ns: Namespace, name: N) -> Option<Value> {
        match ns {
            Namespace::Types => crate::builtin_type(name.as_ref()).map(|d| Value::Found(d, Visibility::Public)),
            Namespace::Macros | Namespace::Values => None,
        }
    }

    /// A plain path's first segment `name` (or a one-segment path's leaf) in `ns` at `m`, asked
    /// by item `me`: R6 (the module first; growth by expansion alone does not hold the lookup,
    /// the outer binding is taken and a watch left), R7 (a pending import is waited for; a glob
    /// candidate beside a different outer one is an ambiguity, the same definition is one, D57),
    /// R8 (an extern-prelude name is final).
    fn first_segment(&mut self, m: ModId, ns: Namespace, name: N, me: ItemId, guess: Option<&Guess<'a>>) -> Step<Value> {
        let (inner, from) = match self.own_view(m, ns, name, me, Skip::Growth, guess) {
            Step::Done(x) => x,
            Step::Wait(n) => return Step::Wait(n),
        };
        if inner == Value::Outside {
            return Step::Done(Value::Outside);
        }
        let outer = match self.outer_of(ns, name, guess) {
            Step::Done(o) => o,
            Step::Wait(n) => return Step::Wait(n),
        };
        let outer_def = match outer {
            Some(Value::Found(d, _) | Value::Ambiguous(d, _)) => Some(d),
            _ => None,
        };
        match inner {
            Value::Found(d, v) | Value::Ambiguous(d, v) => {
                if from == From::Glob
                    && let Some(od) = outer_def
                    && od != d
                    && self.plant != CellPlant::GlobBeatsOuter
                {
                    // `...ambiguity.glob-vs-outer`: the import fails at its first segment.
                    return Step::Done(Value::Ambiguous(d, v));
                }
                if let From::Wrote(f) = from
                    && let Some(o) = outer
                    && outer_def.is_some_and(|od| od != d)
                    && !self.encloses(f, self.items[me as usize].at)
                {
                    // R6 in the other order: the inner definition came first, from an expansion
                    // that does not enclose the use, beside a different outer binding. The
                    // answer is the one every order gives: the outer binding, with the error at
                    // the expansion's definition (wide seed 8 with calls: the reference's).
                    if guess.is_none() {
                        self.r6_error(me);
                    }
                    if self.plant != CellPlant::InnerAfterWrite {
                        return Step::Done(o);
                    }
                }
                if from == From::Glob
                    && let Some(j) = self.growing(m, me, guess)
                {
                    // A glob candidate may still be shadowed by what a call writes.
                    return Step::Wait(Node::Item(j));
                }
                Step::Done(inner)
            }
            Value::Outside => Step::Done(Value::Outside),
            Value::NotFound => match outer {
                Some(o) => {
                    if self.plant == CellPlant::OutwardWaits
                        && let Some(j) = self.growing(m, me, guess)
                    {
                        return Step::Wait(Node::Item(j));
                    }
                    if guess.is_none() {
                        self.stats.outward += 1;
                        if let Some(od) = outer_def
                            && self.growing(m, me, None).is_some()
                        {
                            self.watches.entry((m, ns, name)).or_insert_with(|| AVec::new_in(self.alloc)).push((me, od));
                        }
                        // The binding of a scope after the extern prelude, while a root call
                        // may still write `extern crate .. as name;` (R6 for the extern prelude).
                        if let Some(od) = outer_def
                            && ns == Namespace::Types
                            && self.outer.externs.binary_search_by(|e| e.0.cmp(&name)).is_err()
                            && !self.outer.added.contains_key(&name)
                            && self.growing(self.root, me, None).is_some()
                        {
                            self.extern_watches.entry(name).or_insert_with(|| AVec::new_in(self.alloc)).push((me, od));
                        }
                    }
                    Step::Done(o)
                }
                None => {
                    if let Some(j) = self.growing(m, me, guess) {
                        return Step::Wait(Node::Item(j));
                    }
                    // The extern prelude may still grow by a root call's `extern crate .. as`.
                    if ns == Namespace::Types
                        && let Some(j) = self.growing(self.root, me, guess)
                    {
                        return Step::Wait(Node::Item(j));
                    }
                    Step::Done(Value::NotFound)
                }
            },
        }
    }

    /// Fragment `e` encloses fragment `f`: `f` is `e` or written by a call inside `e`.
    fn encloses(&self, e: FragId, f: FragId) -> bool {
        let mut at = Some(f);
        while let Some(x) = at {
            if x == e {
                return true;
            }
            at = self.frags[x as usize].up.map(|u| u.0);
        }
        false
    }

    fn r6_error(&mut self, me: ItemId) {
        if self.r6.insert(me) {
            self.stats.r6_errors += 1;
        }
    }

    /// Fragment `f`, an expansion, wrote `name` into `m` in `ns`: a use that took an outer
    /// binding of it, and that `f` does not enclose, is R6's error.
    fn wrote(&mut self, m: ModId, ns: Namespace, name: N, def: Def, f: FragId) {
        self.written.entry((m, ns, name)).or_insert(f);
        if self.plant == CellPlant::NoWatch {
            return;
        }
        let Some(ws) = self.watches.get(&(m, ns, name)) else { return };
        let ws: AVec<'a, (ItemId, Def)> = AVec::from_slice_in(ws, self.alloc);
        for &(u, od) in ws.iter() {
            // The outer definition itself is no other candidate (D57).
            if od != def && !self.encloses(f, self.items[u as usize].at) {
                self.r6_error(u);
            }
        }
    }

    /// The text of `m` defines a `macro_rules!` named `name`.
    fn has_rules(&self, m: ModId, name: N) -> bool {
        self.frags[self.modules[m as usize].text as usize].slots.iter().any(|s| matches!(s.1, Slot::Rules(n, _) if n == name))
    }

    /// `name!` by the textual scope just before `pos` of fragment `frag` (Reference
    /// `macros.by-example.scope`): backward through the text, the latest definition first, into
    /// `#[macro_use]` modules and expansions met on the way, then out to the enclosing text at
    /// each call and `mod` line. A call met that has not grown yet may still define the name:
    /// the walk waits on it (under a guess a call of the group adds nothing).
    fn textual(&mut self, frag: FragId, pos: u32, name: N, guess: Option<&Guess<'a>>) -> Step<Option<Def>> {
        self.stats.textual_reads += 1;
        let (mut f, mut end) = (frag, pos);
        let mut stack: AVec<'a, (FragId, usize)> = AVec::new_in(self.alloc);
        loop {
            let below = self.frags[f as usize].slots.partition_point(|s| s.0 < end);
            stack.push((f, below));
            while let Some(top) = stack.last_mut() {
                if top.1 == 0 {
                    stack.pop();
                    continue;
                }
                top.1 -= 1;
                let (_, slot) = self.frags[top.0 as usize].slots[top.1];
                match slot {
                    Slot::Rules(n, def) if n == name => return Step::Done(Some(def)),
                    Slot::Rules(..) | Slot::Mod(_, false) => {}
                    Slot::Mod(child, true) => {
                        let t = self.modules[child as usize].text;
                        stack.push((t, self.frags[t as usize].slots.len()));
                    }
                    Slot::Call(j) => {
                        let it = &self.items[j as usize];
                        if it.grown {
                            if let Some(x) = it.expansion {
                                stack.push((x, self.frags[x as usize].slots.len()));
                            }
                        } else if !guess.is_some_and(|g| g.items.contains_key(&j)) {
                            return Step::Wait(Node::Item(j));
                        }
                    }
                }
            }
            let fr = &self.frags[f as usize];
            match fr.up.or(self.modules[fr.module as usize].mod_line) {
                Some((p, at)) => {
                    f = p;
                    end = at;
                }
                None => return Step::Done(None),
            }
        }
    }

    /// Item `i` is decided: its growth ends.
    fn settle(&mut self, i: ItemId, o: Outcome) {
        if self.items[i as usize].call.is_some() {
            self.items[i as usize].outcome = Some(o);
            self.items[i as usize].waits.clear();
            match o {
                // Its expansion is read next; its waiters wake once it has grown.
                Outcome::Called(_) => {
                    self.expand_queue.insert(i);
                }
                _ => self.grown(i),
            }
            return;
        }
        let it = &mut self.items[i as usize];
        it.outcome = Some(o);
        it.waits.clear();
        let (m, index, glob) = (it.module, it.index, it.glob);
        let waiters = core::mem::replace(&mut it.waiters, AVec::new_in(self.alloc));
        for &w in waiters.iter() {
            self.wake(w);
        }
        if glob {
            let module = &mut self.modules[m as usize];
            module.globs_pending -= 1;
            if let Outcome::Glob(src) = o
                && let Some(&s) = self.by_key.get(&src)
                && !(self.modules[s as usize].foreign && self.plant == CellPlant::DepGlobEmpty)
            {
                let vis = self.items[i as usize].vis;
                self.modules[m as usize].edges.insert(index, (s, vis));
            }
            if self.modules[m as usize].globs_pending == 0 {
                for k in 0..self.modules[m as usize].glob_cells.len() {
                    let c = self.modules[m as usize].glob_cells[k];
                    self.wake(Node::Cell(c));
                }
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // The loop.

    fn drain_cells(&mut self) {
        while let Some(c) = self.cell_queue.pop_first() {
            self.step_cell(c);
        }
    }

    /// Runs every import of the crate to its decision. `order` is the order items are first
    /// pushed in: it changes nothing, which the tests check.
    /// The floor's unit, measured on this run's own cell keys: every key probed in a hash map of
    /// them, in a scattered order, `ROUNDS` times. Hundredths of a nanosecond per probe; `now` is
    /// the passed clock in nanoseconds.
    pub(crate) fn probe_cost(&self, now: &dyn Fn() -> u64) -> u64 {
        const ROUNDS: usize = 50;
        let mut map: blaze_hash::HashMap<(ModId, u8, N), CellId, Alloc<'a>> = blaze_hash::HashMap::new_in(self.alloc);
        let mut keys: AVec<'a, (ModId, u8, N)> = AVec::new_in(self.alloc);
        for (id, c) in self.cells.iter().enumerate() {
            let k = (c.module, cell_slot(c.ns, c.layer), c.name);
            map.insert(k, id as CellId);
            keys.push(k);
        }
        let n = keys.len();
        if n == 0 {
            return 0;
        }
        // A step coprime with any count below 2^32 that is not a multiple of it.
        let step = if n % 7_919 == 0 { 1 } else { 7_919 };
        let mut sum = 0u64;
        let at = now();
        for r in 0..ROUNDS {
            for j in 0..n {
                let k = &keys[(j * step + r) % n];
                sum = sum.wrapping_add(u64::from(map.get(k).copied().unwrap_or(0)));
            }
        }
        let spent = now() - at;
        core::hint::black_box(sum);
        spent * 100 / (ROUNDS * n) as u64
    }

    /// Runs every import and call of the crate to its decision; `expand` is the driver's hook that
    /// expands a resolved call, whose expansion `facts` then hands back.
    pub(crate) fn run<F: ScopeFacts<Name = N>>(&mut self, facts: &F, expand: &dyn Fn(ItemKey, Def), order: &[u32]) -> Result<(), CellError> {
        for &i in order {
            self.item_queue.insert(i);
        }
        for i in 0..self.items.len() as ItemId {
            self.item_queue.insert(i);
        }
        self.turn(&mut |c: &mut Cells<'a, N>, i| c.expand_call(facts, expand, i))
    }

    /// The loop: cells first, then expansions, then items; at a quiet point the sinks.
    fn turn(&mut self, expand: &mut dyn FnMut(&mut Cells<'a, N>, ItemId) -> Result<(), CellError>) -> Result<(), CellError> {
        loop {
            self.drain_cells();
            if let Some(i) = self.expand_queue.pop_first() {
                expand(self, i)?;
                continue;
            }
            if let Some(i) = self.item_queue.pop_first() {
                self.eval(i);
                continue;
            }
            if !self.quiet_point()? {
                return Ok(());
            }
        }
    }

    /// Asks for a cell after the run (a later reader): decided by the same rules. Every call is
    /// decided by then, so none is left to expand.
    pub(crate) fn ask(&mut self, module: ItemKey, ns: Namespace, name: N) -> Result<Option<Value>, CellError> {
        let Some(&m) = self.by_key.get(&module) else { return Ok(None) };
        let c = self.cell(m, ns, name, Layer::Full);
        self.turn(&mut |c: &mut Cells<'a, N>, i| {
            Err(CellError::NotExpanded { call: c.items[i as usize].call.map_or(c.modules[c.root as usize].key, |at| at.key) })
        })?;
        Ok(self.cells[c as usize].value)
    }

    /// Import `index` of fragment `frag`: a module's own text (by the module's key) or a call's
    /// expansion (by the call's key), numbered in that fragment's text, as the seam keys them.
    pub(crate) fn outcome(&self, frag: ItemKey, index: u32) -> Option<Outcome> {
        self.import_at(frag, index).and_then(|i| self.items[i as usize].outcome)
    }

    /// The item of import `index` of fragment `frag` (keyed as in `outcome`).
    fn import_at(&self, frag: ItemKey, index: u32) -> Option<ItemId> {
        let f = match self.by_key.get(&frag) {
            Some(&m) => self.modules[m as usize].text,
            None => self.items.iter().find(|it| it.call.is_some_and(|at| at.key == frag))?.expansion?,
        };
        let found = self.items.iter().enumerate().filter(|(_, it)| it.at == f && it.call.is_none()).nth(index as usize);
        found.map(|(i, _)| i as ItemId)
    }

    /// An R6 error is reported for import `index` of fragment `frag` (keyed as in `outcome`).
    pub(crate) fn import_has_r6(&self, frag: ItemKey, index: u32) -> bool {
        self.import_at(frag, index).is_some_and(|i| self.r6.contains(&i))
    }

    /// A call's outcome, by its key.
    pub(crate) fn call_outcome(&self, call: ItemKey) -> Option<Outcome> {
        self.items.iter().find(|it| it.call.is_some_and(|at| at.key == call)).and_then(|it| it.outcome)
    }

    /// Every import's and macro call's answer, in item order: what a caller comparing two
    /// cores on one crate reads.
    pub(crate) fn results(&self) -> impl Iterator<Item = ItemResult<'_, N>> {
        self.items.iter().map(|it| ItemResult {
            module: self.modules[it.module as usize].key,
            call: it.call.map(|at| at.key),
            kind: it.kind,
            segments: &it.segments,
            glob: it.glob,
            binds: it.binds,
            outcome: it.outcome,
            why: it.why,
        })
    }

    // -----------------------------------------------------------------------------------------
    // Quiet points: cycles (R3) and stalls (R5).

    /// The undecided nodes `n` reads, in a stated order.
    /// `n`'s undecided-or-not inputs in the wait graph. A `Globs` cell's growth (every call of
    /// its module not grown yet) is not listed: `growth` is set to its module instead, whose
    /// one hub node stands for those calls (one edge per cell, not one per cell and call).
    /// `extra` edges are added by the caller.
    fn inputs(&mut self, n: Node, out: &mut AVec<'a, Node>, srcs: &mut AVec<'a, ModId>, growth: &mut Option<ModId>) {
        out.clear();
        *growth = None;
        match n {
            Node::Item(i) => out.extend_from_slice(&self.items[i as usize].waits),
            Node::Cell(c) => {
                let Cell { module: m, ns, name, layer, .. } = self.cells[c as usize];
                match layer {
                    Layer::Full => {
                        if let Some(named) = self.modules[m as usize].named.get(&name) {
                            for &i in named.iter() {
                                if self.items[i as usize].outcome.is_none() {
                                    out.push(Node::Item(i));
                                }
                            }
                        }
                        if out.is_empty() && !self.modules[m as usize].explicit.contains_key(&(ns, name)) {
                            let g = self.cell(m, ns, name, Layer::Globs);
                            if self.cells[g as usize].value.is_none() {
                                out.push(Node::Cell(g));
                            }
                        }
                    }
                    Layer::Globs => {
                        for &i in self.modules[m as usize].glob_items.iter() {
                            if self.items[i as usize].outcome.is_none() {
                                out.push(Node::Item(i));
                            }
                        }
                        // Growth: the module's calls not grown yet, through its hub.
                        if self.plant != CellPlant::CallsNoGrowth {
                            *growth = Some(m);
                        }
                        srcs.clear();
                        srcs.extend(self.modules[m as usize].edges.values().map(|x| x.0));
                        for &src in srcs.iter() {
                            let s = self.cell(src, ns, name, Layer::Full);
                            if self.cells[s as usize].value.is_none() {
                                out.push(Node::Cell(s));
                            }
                        }
                    }
                }
            }
        }
    }

    /// Condenses the undecided nodes; decides the sinks of cells alone (glob cycles), else
    /// fails the items of the sinks (stalls). `false` when nothing is undecided.
    fn quiet_point(&mut self) -> Result<bool, CellError> {
        let alloc = self.alloc;
        let mut nodes: AVec<'a, Node> = AVec::new_in(alloc);
        for c in 0..self.cells.len() as CellId {
            if self.cells[c as usize].value.is_none() {
                nodes.push(Node::Cell(c));
            }
        }
        for i in 0..self.items.len() as ItemId {
            if self.items[i as usize].outcome.is_none() {
                nodes.push(Node::Item(i));
            }
        }
        if nodes.is_empty() {
            return Ok(false);
        }
        self.stats.quiet_points += 1;
        self.stats.quiet_nodes += nodes.len() as u64;
        // Each undecided node's index, dense by node (cells, then items): one load per edge,
        // where the ordered map it replaces was a tree walk (core: 324k nodes over 331 quiet
        // points).
        let cells_len = self.cells.len();
        let mut dense: AVec<'a, u32> = AVec::new_in(alloc);
        dense.resize(cells_len + self.items.len(), u32::MAX);
        for (k, &n) in nodes.iter().enumerate() {
            match n {
                Node::Cell(c) => dense[c as usize] = k as u32,
                Node::Item(i) => dense[cells_len + i as usize] = k as u32,
            }
        }
        let index_of = |n: &Node| -> Option<u32> {
            let at = match *n {
                Node::Cell(c) => c as usize,
                Node::Item(i) => cells_len + i as usize,
            };
            dense.get(at).copied().filter(|&k| k != u32::MAX)
        };
        // Adjacency, by node index, flat (`off[k]..off[k + 1]` in `to`). A module's growth is one
        // hub node after the real ones, made when first read, with an edge to each call of the
        // module not grown and not decided: a hub only joins cells to calls, so every real
        // node's group and every group's closedness are those of the direct edges. A hub with
        // no such call gets no edge in (it would be a node with nothing to wait on).
        const NO_HUB: u32 = u32::MAX;
        const EMPTY_HUB: u32 = u32::MAX - 1;
        let real = nodes.len();
        let mut hub_of: AVec<'a, u32> = AVec::new_in(alloc);
        hub_of.resize(self.modules.len(), NO_HUB);
        let mut hub_rows: AVec<'a, u32> = AVec::new_in(alloc);
        let mut hub_off: AVec<'a, u32> = AVec::new_in(alloc);
        hub_off.push(0);
        let mut off: AVec<'a, u32> = AVec::new_in(alloc);
        let mut to: AVec<'a, u32> = AVec::new_in(alloc);
        off.push(0);
        let mut buf: AVec<'a, Node> = AVec::new_in(alloc);
        let mut srcs: AVec<'a, ModId> = AVec::new_in(alloc);
        let mut growth = None;
        // The fixed points' outside edges, in node order like `nodes`: one ordered walk.
        let extra = core::mem::replace(&mut self.extra, ABTreeSet::new_in(alloc));
        let mut more = extra.iter().peekable();
        for k in 0..real {
            let n = nodes[k];
            self.inputs(n, &mut buf, &mut srcs, &mut growth);
            while let Some(&&(from, x)) = more.peek() {
                if from > n {
                    break;
                }
                if from == n {
                    buf.push(x);
                }
                more.next();
            }
            for x in buf.iter() {
                if let Some(j) = index_of(x) {
                    to.push(j);
                }
            }
            if let Some(m) = growth {
                if hub_of[m as usize] == NO_HUB {
                    let start = hub_rows.len();
                    for &j in self.modules[m as usize].calls.iter() {
                        if !self.items[j as usize].grown
                            && let Some(x) = index_of(&Node::Item(j))
                        {
                            hub_rows.push(x);
                        }
                    }
                    if hub_rows.len() == start {
                        hub_of[m as usize] = EMPTY_HUB;
                    } else {
                        hub_of[m as usize] = (real + hub_off.len() - 1) as u32;
                        hub_off.push(hub_rows.len() as u32);
                    }
                }
                if hub_of[m as usize] != EMPTY_HUB {
                    to.push(hub_of[m as usize]);
                }
            }
            off.push(to.len() as u32);
        }
        drop(more);
        self.extra = extra;
        for h in 0..hub_off.len() - 1 {
            to.extend_from_slice(&hub_rows[hub_off[h] as usize..hub_off[h + 1] as usize]);
            off.push(to.len() as u32);
        }
        // Reading the inputs made a cell no evaluation had made yet: it is tried first, and the
        // graph is drawn again with it (a cell is made once, so this ends).
        if !self.cell_queue.is_empty() {
            return Ok(true);
        }
        let comp = tarjan(&off, &to, alloc);
        let count = comp.iter().copied().max().map_or(0, |c| c as usize + 1);
        let mut sink: AVec<'a, bool> = AVec::new_in(alloc);
        sink.resize(count, true);
        for k in 0..off.len() - 1 {
            for &j in to[off[k] as usize..off[k + 1] as usize].iter() {
                if comp[k] != comp[j as usize] {
                    sink[comp[k] as usize] = false;
                }
            }
        }
        let mut has_item: AVec<'a, bool> = AVec::new_in(alloc);
        has_item.resize(count, false);
        for (k, n) in nodes.iter().enumerate() {
            if let Node::Item(_) = n {
                has_item[comp[k] as usize] = true;
            }
        }
        if self.plant == CellPlant::StallFailsAll && (0..count).any(|s| sink[s] && has_item[s]) {
            // The planted R5 bug: a stall fails every undecided item.
            self.stats.stalls += 1;
            for n in nodes.iter() {
                if let Node::Item(i) = *n {
                    self.stats.stalled_items += 1;
                    self.settle(i, Outcome::Failed(Unresolved::NotFound));
                }
            }
            return Ok(true);
        }
        // Every sink waits on itself alone: nothing outside it can change it any more, so it is
        // decided as its least fixed point (R3); a member that reaches nothing from outside the
        // group is not found (R5, the stall rule). Sinks share no edge, so their order is moot.
        for s in 0..count {
            if !sink[s] {
                continue;
            }
            let mut members: AVec<'a, Node> = AVec::new_in(alloc);
            for (k, &n) in nodes.iter().enumerate() {
                if comp[k] as usize == s {
                    members.push(n);
                }
            }
            if members.is_empty() {
                continue;
            }
            self.fixpoint(&members)?;
        }
        Ok(true)
    }

    /// Decides a sink of cells together as the least fixed point of their candidates (R3).
    fn fixpoint(&mut self, members: &[Node]) -> Result<(), CellError> {
        let mut guess = Guess { cells: ABTreeMap::new_in(self.alloc), items: ABTreeMap::new_in(self.alloc) };
        for &n in members {
            match n {
                Node::Cell(c) => {
                    guess.cells.insert(c, None);
                }
                Node::Item(i) => {
                    guess.items.insert(i, None);
                }
            }
        }
        // Each value only rises (nothing, found with a wider visibility, ambiguous), so the
        // rounds are bounded by a few per member.
        let bound = 4 * members.len() + 4;
        let mut rounds = 0;
        loop {
            let mut changed = false;
            for &n in members {
                let step = match n {
                    Node::Cell(c) => match self.cell_value(c, Some(&guess), Skip::No) {
                        Step::Done(v) => Step::Done(GuessStep::Cell(Some(v).filter(|v| *v != Value::NotFound))),
                        Step::Wait(on) => Step::Wait(on),
                    },
                    Node::Item(i) => match self.try_item(i, Some(&guess)) {
                        Step::Done(o) => Step::Done(GuessStep::Item(Some(o))),
                        Step::Wait(on) => Step::Wait(on),
                    },
                };
                let on = match step {
                    Step::Wait(on) => on,
                    Step::Done(GuessStep::Cell(v)) => {
                        if let Node::Cell(c) = n
                            && guess.cells.insert(c, v) != Some(v)
                        {
                            changed = true;
                        }
                        continue;
                    }
                    Step::Done(GuessStep::Item(o)) => {
                        if let Node::Item(i) = n
                            && guess.items.insert(i, o) != Some(o)
                        {
                            changed = true;
                        }
                        continue;
                    }
                };
                // The group read a node outside it that is not decided: it was no sink. The edge
                // is kept so the next quiet point sees it, and the reader is woken by it.
                self.extra.insert((n, on));
                match (on, n) {
                    (Node::Cell(d), _) => {
                        self.stats.waiters += 1;
                        self.cells[d as usize].waiters.push(n);
                    }
                    (Node::Item(j), _) => {
                        self.stats.waiters += 1;
                        self.items[j as usize].waiters.push(n);
                    }
                }
                return Ok(());
            }
            if !changed {
                break;
            }
            rounds += 1;
            if rounds > bound {
                let module = match members[0] {
                    Node::Cell(c) => self.modules[self.cells[c as usize].module as usize].key,
                    Node::Item(i) => self.modules[self.items[i as usize].module as usize].key,
                };
                return Err(CellError::NoFixpoint { module, cells: members.len() });
            }
        }
        let has_call = members.iter().any(|&n| matches!(n, Node::Item(i) if self.items[i as usize].call.is_some()));
        if has_call {
            return self.stall_with_calls(members, &guess);
        }
        // R5: in a stalled group what is undecided is taken as absent, and an import may RESOLVE
        // so; then only the members that resolved are decided this round, and the rest (cells
        // too: their guess read the failed members as absent) are tried again with the facts,
        // woken by what the decided imports bring. Each such round decides one import at least,
        // so the rounds are bounded by the group's imports.
        if self.plant != CellPlant::StallDecidesAll {
            let alloc = self.alloc;
            let mut picked: AVec<'a, (ItemId, Outcome)> = AVec::new_in(alloc);
            let mut failed = false;
            for (&i, &o) in guess.items.iter() {
                match o {
                    Some(o @ (Outcome::Named(_) | Outcome::Glob(_) | Outcome::Outside)) => picked.push((i, o)),
                    _ => failed = true,
                }
            }
            if failed && !picked.is_empty() {
                self.stats.stalls += 1;
                for &(i, o) in picked.iter() {
                    self.settle(i, o);
                }
                return Ok(());
            }
        }
        // Decided: a group holding an import is a stall (R5), one of cells alone a glob cycle.
        if guess.items.is_empty() {
            self.stats.cycle_passes += 1;
            self.stats.cycle_cells += members.len() as u64;
            self.stats.largest_cycle = self.stats.largest_cycle.max(members.len() as u64);
        } else {
            self.stats.stalls += 1;
        }
        for &n in members {
            match n {
                Node::Cell(c) => {
                    let v = guess.cells.get(&c).copied().flatten().unwrap_or(Value::NotFound);
                    let v = if self.plant == CellPlant::CycleNotFound { Value::NotFound } else { v };
                    self.decide(c, v);
                }
                Node::Item(_) => {}
            }
        }
        for &n in members {
            if let Node::Item(i) = n {
                let o = guess.items.get(&i).copied().flatten().unwrap_or(Outcome::Failed(Unresolved::NotFound));
                if o == Outcome::Failed(Unresolved::NotFound) {
                    self.stats.stalled_items += 1;
                }
                self.settle(i, o);
            }
        }
        Ok(())
    }

    /// R5 over a sink holding a macro call, its least fixed point `guess` computed with the
    /// group's calls adding nothing. Imports the guess resolves are decided first, alone (what
    /// they bring may answer the rest); else the stall: every call fails as not found (none is
    /// decided by a guess) but one whose textual read waits on a call of the group (spared this
    /// round: the earlier call fails first), every import takes its guess,
    /// and the cells are decided only when no call was spared (a spared call may still write).
    fn stall_with_calls(&mut self, members: &[Node], guess: &Guess<'a>) -> Result<(), CellError> {
        let alloc = self.alloc;
        // Calls are never decided from a guess: R5 fails them (the reference's sinks), and one
        // resolving under the guess would expand before an earlier call failed.
        let mut picked: AVec<'a, (ItemId, Outcome)> = AVec::new_in(alloc);
        for &n in members {
            let Node::Item(i) = n else { continue };
            match guess.items.get(&i) {
                Some(&Some(o @ (Outcome::Named(_) | Outcome::Glob(_)))) if self.items[i as usize].call.is_none() => picked.push((i, o)),
                // R6: a plain first segment the inner scope misses while that scope can grow by
                // expansion alone takes the outer binding, whatever the group's calls write
                // later (a later write is an ambiguity at it, never another answer). Stage 1
                // follows no outer scope: the import or call is `Outside`, decided now, alone,
                // and never failed by the stall (core's `cfg_select!`, `global_asm!`).
                Some(&Some(Outcome::Outside)) if self.plant != CellPlant::StallFailsOutside => picked.push((i, Outcome::Outside)),
                // R5 (2026-11-16): in a stalled member the undecided inner scope is absent, so a
                // one-segment call goes outward and RESOLVES to what an outer scope holds (the
                // standard prelude's `Clone` at a `#[derive(Clone)]` in a module whose globs wait
                // on its own derives: core's `core_arch`). Only an outer answer is taken: one
                // from a guessed inner value could still be shadowed or made ambiguous.
                Some(&Some(Outcome::Called(def))) if self.plant != CellPlant::StallFailsOuterCall => {
                    let it = &self.items[i as usize];
                    if it.kind == PathKind::Plain && it.segments.len() == 1 {
                        let (m, name) = (it.module, it.segments[0]);
                        if let Step::Done(Some(Value::Found(d, _))) = self.outer_of(Namespace::Macros, name, Some(guess))
                            && d == def
                        {
                            picked.push((i, Outcome::Called(def)));
                            // R6's watch: a later write of `name` into `m` by an expansion that
                            // does not enclose the call is the error.
                            self.watches.entry((m, Namespace::Macros, name)).or_insert_with(|| AVec::new_in(alloc)).push((i, def));
                        }
                    }
                }
                _ => {}
            }
        }
        if !picked.is_empty() {
            self.stats.stalls += 1;
            for &(i, o) in picked.iter() {
                self.settle(i, o);
            }
            return Ok(());
        }
        let mut spared: AVec<'a, ItemId> = AVec::new_in(alloc);
        let mut blocking: AVec<'a, ItemId> = AVec::new_in(alloc);
        if self.plant != CellPlant::StallSparesNone {
            for &n in members {
                let Node::Item(i) = n else { continue };
                let Some(at) = self.items[i as usize].call else { continue };
                if self.items[i as usize].kind != PathKind::Plain || self.items[i as usize].segments.len() != 1 {
                    continue;
                }
                let name = self.items[i as usize].segments[0];
                if let Step::Wait(w) = self.textual(at.frag, at.pos, name, None)
                    && members.contains(&w)
                {
                    spared.push(i);
                    if let Node::Item(j) = w {
                        blocking.push(j);
                    }
                }
            }
        }
        // R5 (2026-11-16): a round that spares a call fails only the calls the spared calls'
        // textual reads wait on; every other member is tried again with the facts.
        if !spared.is_empty() {
            blocking.retain(|j| !spared.contains(j));
            blocking.sort_unstable();
            blocking.dedup();
            if !blocking.is_empty() {
                self.stats.stalls += 1;
                self.stats.spared += spared.len() as u64;
                for &j in blocking.iter() {
                    self.stats.stalled_items += 1;
                    self.settle(j, Outcome::Failed(Unresolved::NotFound));
                }
                return Ok(());
            }
        }
        let failing = members.iter().filter(|n| matches!(n, Node::Item(i) if !spared.contains(i))).count();
        if failing == 0 {
            let module = match members[0] {
                Node::Cell(c) => self.modules[self.cells[c as usize].module as usize].key,
                Node::Item(i) => self.modules[self.items[i as usize].module as usize].key,
            };
            return Err(CellError::AllSpared { module });
        }
        self.stats.stalls += 1;
        self.stats.spared += spared.len() as u64;
        if spared.is_empty() {
            for &n in members {
                if let Node::Cell(c) = n {
                    let v = guess.cells.get(&c).copied().flatten().unwrap_or(Value::NotFound);
                    self.decide(c, v);
                }
            }
        }
        for &n in members {
            let Node::Item(i) = n else { continue };
            if spared.contains(&i) {
                continue;
            }
            let o = if self.items[i as usize].call.is_some() {
                Outcome::Failed(Unresolved::NotFound)
            } else {
                guess.items.get(&i).copied().flatten().unwrap_or(Outcome::Failed(Unresolved::NotFound))
            };
            if o == Outcome::Failed(Unresolved::NotFound) {
                self.stats.stalled_items += 1;
            }
            self.settle(i, o);
        }
        Ok(())
    }
}

/// Strongly connected components of `adj`, iteratively (Tarjan): each node's component number.
fn tarjan<'a>(off: &[u32], to: &[u32], alloc: Alloc<'a>) -> AVec<'a, u32> {
    const NONE: u32 = u32::MAX;
    let n = off.len() - 1;
    let row = |v: usize| &to[off[v] as usize..off[v + 1] as usize];
    let mut index: AVec<'a, u32> = AVec::new_in(alloc);
    index.resize(n, NONE);
    let mut low: AVec<'a, u32> = AVec::new_in(alloc);
    low.resize(n, 0);
    let mut on_stack: AVec<'a, bool> = AVec::new_in(alloc);
    on_stack.resize(n, false);
    let mut comp: AVec<'a, u32> = AVec::new_in(alloc);
    comp.resize(n, NONE);
    let mut stack: AVec<'a, u32> = AVec::new_in(alloc);
    // (node, next edge to look at)
    let mut calls: AVec<'a, (u32, u32)> = AVec::new_in(alloc);
    let (mut next, mut comps) = (0u32, 0u32);
    for start in 0..n as u32 {
        if index[start as usize] != NONE {
            continue;
        }
        calls.push((start, 0));
        index[start as usize] = next;
        low[start as usize] = next;
        next += 1;
        stack.push(start);
        on_stack[start as usize] = true;
        while let Some(&(v, e)) = calls.last() {
            let vi = v as usize;
            if (e as usize) < row(vi).len() {
                let last = calls.len() - 1;
                calls[last].1 += 1;
                let w = row(vi)[e as usize];
                let wi = w as usize;
                if index[wi] == NONE {
                    index[wi] = next;
                    low[wi] = next;
                    next += 1;
                    stack.push(w);
                    on_stack[wi] = true;
                    calls.push((w, 0));
                } else if on_stack[wi] {
                    low[vi] = low[vi].min(index[wi]);
                }
                continue;
            }
            calls.pop();
            if let Some(&(p, _)) = calls.last() {
                low[p as usize] = low[p as usize].min(low[vi]);
            }
            if low[vi] == index[vi] {
                while let Some(w) = stack.pop() {
                    on_stack[w as usize] = false;
                    comp[w as usize] = comps;
                    if w == v {
                        break;
                    }
                }
                comps += 1;
            }
        }
    }
    comp
}
