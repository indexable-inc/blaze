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
use blaze_surface::{CallRole, DeclsBuilder, SurfaceError};

use crate::facts::ScopeFacts;
use crate::{AmbiguityKind, CallShape, Def, DefKind, Edition, ExternTarget, Namespace, Origin, PathKind, Rename, Unresolved, UseForm, Visibility};

// D116: the old scopes against the records at module grain, name by name.
mod agree;
// D189 step 4: one body's blocks solved over the crate's finished names.
mod body;
mod decide;
mod deps;
mod early;
// E4's incremental half: an edit re-solves the groups whose inputs moved, from the records.
mod edit;
// E4: what the cold solve writes, one record per group of modules (engine.md "Tables").
mod groups;
// E13: a crate's groups solved on demand from the modules asked, never the whole crate.
mod lazy;
// E13 across crates: the root crates solved whole, every dependency's groups on demand.
mod lazy_deps;
// D116: a module's tables of links and its `Imports`, from what the core loaded and decided.
mod made;
mod switch;
#[cfg(test)]
mod tests;
mod vis;
#[cfg(test)]
mod world;
mod write;

use early::EarlyAmbiguity;

#[cfg(test)]
pub(crate) use world::{links_agree_world, solve_world};

pub use agree::{Agreed, Answer, AnswersDiffer, Asked, Disagreement};
pub(crate) use body::{BodyTree, FinishedScopes, LoggedEnums, solve_body_in};
pub(crate) use made::Tables;
pub use made::TablesError;
pub(crate) use switch::solve_and_write;
pub use decide::{DecideCheck, DecideError, Difference};
pub use groups::{ABSENT as GROUP_ABSENT, GroupRead, GroupSeen, GroupShape, GroupsError, NameGroup, NodeAt, OwnError as GroupOwnError, RecordError as GroupRecordError, write_groups};
use groups::OwnError;
pub use lazy_deps::{LazyDep, MOST_ROUNDS as MOST_WANT_ROUNDS, OnDemand, OnDemandError, Refused as OnDemandRefused, Want, names_on_demand};
pub use lazy::{DemandError, Demanded, DemandedOpts, MOST_DEPTH as MOST_DEMAND_DEPTH, MOST_SOLVES as MOST_DEMAND_SOLVES, NoRecordOf, demand};
pub use edit::{Edit, EditError, Edited, GroupRecords, GroupRef, GroupSolver, MemberRefusal, Members, OneGroup, OneGroupRefusal, Solved, most_entries, re_solve};
#[cfg(test)]
pub(crate) use edit::{EditPlant, re_solve_planted};
pub use groups::{globs_cell, own_digest, textual_cell};
pub use switch::Unwritten;

pub(crate) use deps::{BuiltDeps, DepAnswer, DepNames, EnumNames};

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
pub enum Layer {
    /// What a path reads: the explicit entry or named import, else the globs.
    Full,
    /// Through the module's globs alone: what a named import of this very name reads.
    Globs,
}

/// A decided cell.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Value {
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
    /// A one-segment attribute naming a built-in attribute (`#[inline]`) that no nearer scope
    /// binds: inert, no macro and no expansion (Reference `attributes.builtin`, "Built-in
    /// attributes index": "built-in attributes"; rustc's built-in attribute scope).
    Inert,
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
    /// The segment at this index is not the next module down toward the item: on a `use` or
    /// `extern crate`, whose `pub(in path)` the core does not read through names (vis.rs).
    Segment(usize),
}

/// Why the core stopped, naming the thing.
#[derive(Clone, Debug)]
pub enum CellError {
    Facts { module: ItemKey, error: SurfaceError },
    /// A `use` or `extern crate` whose `pub(in path)` leaves the module chain (vis.rs): refused
    /// by name. A visibility naming no ancestor is its item's scope error, never this.
    BadVisibility { module: ItemKey, why: BadVis },
    /// A cycle's least fixed point did not settle within its bound.
    NoFixpoint { module: ItemKey, cells: usize },
    /// The driver was asked to expand the call and the facts still have no expansion of it.
    NotExpanded { call: ItemKey },
    /// Reading the call's expansion failed.
    Expansion { call: ItemKey, error: SurfaceError },
    /// Reading the helper attributes of derive `derive`, called by `call`, failed.
    Helpers { call: ItemKey, derive: ItemKey, error: SurfaceError },
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
            CellError::Helpers { call, derive, error } => write!(f, "call {call:?}: reading the helper attributes of derive {derive:?}: {error}"),
            CellError::DecidedTooEarly { module, ns } => {
                write!(f, "module {module:?}: an expansion writes a {ns:?} name whose cell was decided not found (a bug in the core)")
            }
            CellError::AllSpared { module } => write!(f, "module {module:?}: every member of a stalled group is spared by text order"),            CellError::Facts { module, error } => write!(f, "module {module:?}: reading its facts: {error}"),
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
    /// Tool attributes (`#[rustfmt::skip]`) met: inert, no call made.
    pub tool_attrs: u64,
    /// Calls written at expansion depth `RECURSION_LIMIT` or deeper: errors, never expanded.
    pub too_deep: u64,
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
    /// A `use` that makes a binding more visible than it is writes no `WiderReexport`.
    NoWiderReexport,
    /// The `#[macro_export]` macros an expansion wrote are listed in the order their
    /// expansions were read, not in the module's text order.
    ExportsByExpansionTime,
    /// A tool attribute (`#[rustfmt::skip]`) is a call like any other: its path fails.
    ToolAttrIsACall,
    /// No expansion depth limit: a self-calling macro expands until the load's bound refuses.
    NoRecursionLimit,
    /// `use crate as name;` names nothing (the walk reads no leaf): `name` is not bound.
    CrateRootImportFails,
    /// An enum is no source of names: a path through one stays unknown.
    EnumIsOutside,
    /// A glob of an enum is as visible as the glob alone, not narrowed by the enum's binding.
    EnumGlobNotClamped,
    /// A macro name a glob brings waits for the module's other calls to expand (as a type or a
    /// value does), so two calls named through a glob in one module stall and fail.
    MacroGlobWaits,
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
    /// A block's lexical `use super::*;` is read as a glob import at a first segment: a name
    /// the module around the block binds by a named import is ambiguous with a different
    /// outer one (the `#[macro_use]` prelude's `format` beside `use dep::x as format;`).
    LexicalGlobIsAGlob,
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
    /// `use ::name;` (a crate alone) is unknown (the core's answer until runner 11).
    CrateAloneUnknown,
    /// A block's module has no place in the text around its body: its textual scope ends at
    /// its own text (the core's answer until runner 13: zerocopy's `impl_for_transmute_from`).
    BlockHasNoPlace,
    /// The first segment of a call's path (`a::m!`) found through a glob waits for the module's
    /// other calls (the core's answer until runner 13: icu's `#[derive(yoke::Yokeable,
    /// zerofrom::ZeroFrom)]` under `use icu_provider::prelude::*;` stalled and failed).
    CallPrefixGlobWaits,
    /// A stalled call `a::m!` whose first segment only an outer scope holds fails with the
    /// stall (the core's answer until runner 14: alloy_primitives' `paste::paste!` in a module
    /// whose glob cycle waits on that very call).
    StallFailsOuterPrefixCall,
    /// A stalled one-segment call whose macro a glob source of its module holds decided fails
    /// with the stall (the core's answer until runner 14: objc2_core_foundation's
    /// `cf_objc2_type!` in `generated`, which globs its children and the root, which glob it).
    StallFailsGlobCall,
    /// What a stalled call took while its module's globs were weakly undecided is never read
    /// again: a glob that later brings another candidate of the name is no error.
    SpeculationUnchecked,
    /// At a stall a pending named import of the call's own name does not hold its lookup: the
    /// globs' candidate or an outer one is taken past it.
    StallPassesNamedImport,
    /// A call takes the first candidate of its name whatever its sub-namespace ([`same_sub`]),
    /// and a macro of the wrong shape is called (the core's answer until runner 16).
    SubNamespaceIgnored,
    /// D116's walk (decide.rs) keeps a node's value at once though it read a node still open on
    /// the walk's stack, instead of answering `Absent` until their group closes: a glob cycle
    /// keeps the bottom it saw.
    DecideCachesOpenCycle,
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
    /// `macro_rules! name`, and whether it is `#[macro_export]`.
    Rules(N, Def, bool),
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
    /// How it is written (`m!(..)`, `#[m]`, `#[derive(m)]`): which macros it may name.
    shape: CallShape,
}

/// Whether macro `def` is in the sub-namespace a call of `shape` reads (Reference
/// `names.resolution.expansion.macros.intro`: "Macros are split into two sub-namespaces, one
/// for function-like macros, and the other for attributes and derives. Resolution candidates
/// from the incorrect sub-namespace are ignored."). Not a macro: never.
/// What the built-in attribute scope binds a built-in attribute's name to (rustc's
/// `Res::NonMacroAttr(Builtin)`), read only by the attribute call that names it, which is then
/// [`Outcome::Inert`]: never bound in a module, never written to a record. Its first eight
/// bytes (`builtin ` with a space) are no builtin type's (`builtin\0`) and, but by a 2^-64
/// chance, no item's (an item key starts with its crate's hash).
const BUILTIN_ATTR: Def = Def {
    key: ItemKey(blaze_id::ContentId(*b"builtin attribute\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0")),
    kind: DefKind::Other,
};

/// What a derived item's keep call ([`CallRole::Keep`]) is called with when no attribute written
/// after its derives took the item ([`Cells::item_fate`], attr-bang-kind-2): no macro; the
/// driver lowers the call's input, the item, as its expansion (resolve.rs `keep_item`). Never
/// bound in a module, never written to a record. Its bytes are `BUILTIN_ATTR`'s neither and, but
/// by a 2^-64 chance, no item's (an item key starts with its crate's hash).
pub(crate) const KEEP_ITEM: Def = Def {
    key: ItemKey(blaze_id::ContentId(*b"kept item after its derives\0\0\0\0\0")),
    kind: DefKind::Other,
};

/// What a proc-macro crate's harness call ([`CallRole::ProcMacroHarness`], D175) is called
/// with: no macro; the driver lowers the call's input, the `_DECLS` static blaze-scope
/// task/lower.rs `Lower::proc_macro_harness` wrote, as its expansion, declaring no name
/// (resolve.rs `keep_item`, [`crate::resolve`]'s `KeepAs::Harness`). Never bound in a module,
/// never written to a record; its bytes are no other sentinel's and, but by a 2^-64 chance, no
/// item's.
pub(crate) const PROC_MACRO_HARNESS: Def = Def {
    key: ItemKey(blaze_id::ContentId(*b"proc-macro harness _DECLS\0\0\0\0\0\0\0")),
    kind: DefKind::Other,
};

/// What the calls written after a derive on an item decided for one attribute call on it
/// ([`Cells::item_fate`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Fate {
    /// Nothing took the item before the call: it resolves by its path.
    Open,
    /// An attribute macro took the item: the call is that macro's input, inert.
    Taken,
    /// The item's keep call, and nothing took the item: the item is kept.
    Kept,
    /// Under a group's guess, a call before it of the same group is not decided: no answer
    /// that could expand ([`Cells::item_fate`]).
    Undecided,
}

/// The [`CallRole::AfterDerive`] and [`CallRole::AfterDeriveBeforeDerive`] calls of one item of
/// one fragment, in text order, the latter also listed alone, and its [`CallRole::Keep`] call.
struct AfterDeriveCalls<'a> {
    takers: AVec<'a, ItemId>,
    before_derive: AVec<'a, ItemId>,
    keep: Option<ItemId>,
}

fn same_sub(def: Def, shape: CallShape) -> bool {
    let DefKind::Macro(s) = def.kind else { return false };
    match shape {
        CallShape::FnLike => s.accepts(CallShape::FnLike),
        CallShape::Attr | CallShape::Derive => s.accepts(CallShape::Attr) || s.accepts(CallShape::Derive),
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Node {
    Cell(CellId),
    Item(ItemId),
}

/// A module's textual sequence as its group's record holds it (a module outside a one-group
/// solve's members): what a `macro_rules!` walk out of a member, or into a `#[macro_use]`
/// child, reads in place of its text.
pub(crate) struct RecTextual<'a, N> {
    /// Its `macro_rules!` in text order, its `#[macro_use]` children's flattened in at their
    /// lines (`ModuleScope`'s `textual`).
    pub(crate) defs: AVec<'a, blaze_surface::TextualDef<N>>,
    /// Where each child's `mod` line falls in `defs`, in text order (`ModuleScope`'s `marks`).
    pub(crate) marks: AVec<'a, blaze_surface::TextualMark>,
    /// Its textual cell's digest in the record's `defines` (groups.rs `textual_cell`).
    pub(crate) digest: blaze_id::ContentId,
    /// Its children with `#[macro_use]`, in text order (its `ModuleDecls`: the module tree,
    /// read by every group). `None`: a child its marks name is not in its `mod` lines (an
    /// expansion wrote it), so which children a walk enters is not known here.
    pub(crate) macro_use: Option<AVec<'a, ItemKey>>,
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
    /// A block's module (no `mod` line): where its body is written, as the lowering files it
    /// (the fragment's owner, the last ordered entry's position before the body). Found, and
    /// kept in `mod_line`, when the textual scope first goes out of it ([`Cells::line_of`]).
    block_at: Option<(ItemKey, u32)>,
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
    /// Where its names come from: its text, or (a one-group solve's module outside the group)
    /// its group's record.
    answered: Answered,
    /// Its `mod` lines as its declarations hold them: each child, and whether the line is
    /// `#[macro_use]` (the module tree; what a `macro_rules!` walk out of a child enters).
    /// `None` when the facts hold no declarations of it.
    lines: Option<AVec<'a, (ItemKey, bool)>>,
}

/// Where a module of the crate is answered from (edit.rs, the one-group solve).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Answered {
    /// Its text, read here.
    Text,
    /// Outside a one-group solve's members, not read yet: filled from its group's record when
    /// first read ([`Cells::fill_pending`]), then `Record` or `NoRecord`. A module no member
    /// reads costs nothing.
    Pending,
    /// Its group's record: its own bindings and glob edges were filled from its scope there
    /// (no items, no growth); the digest of its globs cell, as that record defines it.
    Record(blaze_id::ContentId),
    /// Outside the group and in no record (a module just added): a read of it is refused
    /// ([`edit::MemberRefusal::NoRecord`]).
    NoRecord,
}

/// Where a walk of a recorded textual sequence ([`Cells::textual_recorded`]) stops.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Cut {
    /// The whole sequence (a `#[macro_use]` child walked into).
    Whole,
    /// Before this child's `mod` line (its mark).
    Child(ItemKey),
}

/// A module outside a one-group solve's members as its group's record holds it ([`Outside`]).
pub(crate) struct Filled<'f, N> {
    pub(crate) bindings: AVec<'f, blaze_surface::ScopeEntry<N>>,
    pub(crate) globs: AVec<'f, blaze_surface::GlobEdge>,
    /// Its scope holds `#[macro_export]` macros an expansion wrote.
    pub(crate) macro_exports: bool,
    /// Its textual cell's digest, when its record defines one; then its sequence and marks.
    pub(crate) textual: Option<blaze_id::ContentId>,
    pub(crate) defs: AVec<'f, blaze_surface::TextualDef<N>>,
    pub(crate) marks: AVec<'f, blaze_surface::TextualMark>,
}

/// The records a one-group solve answers its modules outside the members from (switch.rs),
/// read when a member first reads such a module ([`Answered::Pending`]).
pub(crate) trait Outside<N> {
    /// `key`'s scope from its group's record into `into`, and the digest of its globs cell
    /// there; `None` when no record holds it.
    fn fill(&self, key: ItemKey, into: &mut Filled<'_, N>) -> Result<Option<blaze_id::ContentId>, edit::MemberRefusal>;

    /// The chain of `key`, a module of this crate the outside holds (itself first, the root
    /// last): how a module that no text read here names, and that no record filled yet
    /// declares, is reached ([`Cells::reach_outside`]: each ancestor filled from the root
    /// down, whose record declares the next). `None` (the default, a one-group solve's group
    /// records): such a module is reached only through its parent's record, as before.
    fn chain(&self, key: ItemKey) -> Option<&[ItemKey]> {
        let _ = key;
        None
    }
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
    /// A path whose last module-like segment is an enum: the visibility the enum was read
    /// with there, which its glob is narrowed by (a variant is as visible as its enum).
    clamp: Option<Visibility>,
    /// The segment the last evaluation's walk was at: where a failure is reported (the scope
    /// record's `segment` of an `Import` or `Macro` error).
    seg: u32,
    /// A call written at expansion depth [`crate::RECURSION_LIMIT`] or deeper: never resolved
    /// or expanded, a `RecursionLimit` error of its scope (rustc's default `recursion_limit`).
    deep: bool,
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
    /// All of it, and the module's glob imports not resolved yet: the weak view
    /// ([`Cells::weak_view`]), read at a stall under a guess of its own.
    Weak,
}

/// How far [`Cells::make_modules_in`] makes modules from the ones it is given.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Extent {
    /// Their subtrees by their `mod` lines: a crate solve's whole tree.
    Subtrees,
    /// The given modules alone, each with its `mod` lines: a body solve's chain
    /// ([`Cells::load_body`]).
    Listed,
}

/// What holds the lookup of a plain first segment in its module ([`Cells::first_segment`]).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Hold {
    /// Whatever is undecided there: the answer every order of evaluation gives.
    Undecided,
    /// A pending named import of the name alone: a stalled call's lookup, asked at a quiet point
    /// (the one state every order reaches). A scope only weakly undecided (it binds nothing now;
    /// only an expansion or a glob import not resolved yet could add the name) does not hold it:
    /// the candidate its globs bring now is taken, else the lookup goes on outward, and what was
    /// taken is validated when the run ends (Reference `names.resolution.expansion.speculation`).
    NamedImport,
}

/// What a stalled call's lookup took while a scope was weakly undecided ([`Hold::NamedImport`]):
/// kept only when the call is decided by it.
#[derive(Clone, Copy)]
enum Taken<N> {
    /// R6's watch: a later write of the name into the module by an expansion that does not
    /// enclose the call is the error, of the kind given (the definition taken was an outer
    /// scope's or a glob's).
    Watch(ModId, Namespace, N, Def, AmbiguityKind),
    /// The module's `Globs` cell of the name, read again when the run ends: the definition
    /// taken, and whether the globs brought it (else an outer scope did, and the globs had none).
    Globs(CellId, Def, bool),
    /// The error is known already (a definition an expansion wrote beside another candidate).
    Error(EarlyAmbiguity<N>),
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
    /// Reusable traversal stack for [`Cells::textual`], avoiding per-call bump allocations.
    textual_stack: AVec<'a, (FragId, usize)>,
    cell_queue: ABTreeSet<'a, CellId>,
    item_queue: ABTreeSet<'a, ItemId>,
    /// Calls resolved and not expanded yet.
    expand_queue: ABTreeSet<'a, ItemId>,
    /// Inputs a group's fixed point met outside the group (a node reads the second): edges the
    /// quiet point adds to the wait graph, so the group is not taken for a sink again.
    extra: ABTreeSet<'a, (Node, Node)>,
    outer: Outer<'a, N>,
    /// The crate's tool namespaces (`rustfmt`, `clippy`, `blaze`, the root's `register_tool`s),
    /// from its root record: an attribute whose path starts with one is inert.
    tools: AVec<'a, N>,
    /// The toolchain's built-in attribute names (`inline`, `test`, ...), from the root record:
    /// the built-in attribute scope of a one-segment attribute call ([`Outcome::Inert`]).
    builtin_attrs: AVec<'a, N>,
    /// The root's `#![blaze::arbitrary_ints]`: `u24`, `i80` are builtin types in this crate's
    /// language prelude (docs/v2/plan/design/arbitrary-ints.md, "The opt-in").
    arbitrary_ints: bool,
    /// The item each call is written on (`MacroCallDecl::item`), by call: a derive's helper
    /// scope is its item's ([`Cells::derive_helper`]).
    call_on: ABTreeMap<'a, ItemId, u32>,
    /// The proc-macro harness calls ([`CallRole::ProcMacroHarness`]): each answered
    /// [`PROC_MACRO_HARNESS`] at once, its path never read.
    harness_calls: ABTreeSet<'a, ItemId>,
    /// The derive calls on each item of each fragment, in text order.
    derives_on: ABTreeMap<'a, (FragId, u32), AVec<'a, ItemId>>,
    /// The calls written after a derive on each item of each fragment ([`CallRole`]): what
    /// [`Cells::item_fate`] reads.
    after_derive_on: ABTreeMap<'a, (FragId, u32), AfterDeriveCalls<'a>>,
    /// The last place of a one-segment attribute call on each item of each fragment: a derive
    /// before it has its helpers read when it expands ([`Cells::expand_call`]).
    attr_after: ABTreeMap<'a, (FragId, u32), u32>,
    /// Each derive's helper attributes, sorted, by the derive's key.
    helpers: ABTreeMap<'a, ItemKey, AVec<'a, N>>,
    /// R6's watches: a use whose plain first segment took an outer binding while its module could
    /// still grow, by module and name.
    /// Each with the definition taken and the ambiguity a later write makes of it.
    watches: ABTreeMap<'a, (ModId, Namespace, N), AVec<'a, (ItemId, Def, AmbiguityKind)>>,
    /// Names an expansion wrote into a module as items or `mod`s: the fragment that wrote each.
    written: ABTreeMap<'a, (ModId, Namespace, N), FragId>,
    /// The uses and calls an R6 error is reported for, each with its error (the least, when
    /// one item meets two: `EarlyAmbiguity`'s order), written by `scope` at the item.
    pub(crate) r6: ABTreeMap<'a, ItemId, EarlyAmbiguity<N>>,
    /// GlobVsExpanded's candidates: a use whose plain first segment took a name an expansion
    /// wrote into its module, by the use and namespace: the name, the expanded definition and
    /// the module's `Globs` cell of the name, read when the run ends ([`Cells::validate_expanded`]).
    expanded_uses: ABTreeMap<'a, (ItemId, Namespace), (N, Def, CellId)>,
    /// GlobVsExpanded's errors, by the use and namespace: the name, the expanded definition and
    /// the different one the module's globs bring.
    pub(crate) glob_vs_expanded: ABTreeMap<'a, (ItemId, Namespace), (N, Def, Def)>,
    pub(crate) stats: CellStats,
    pub(crate) plant: CellPlant,
    /// Items are evaluated from the last id first (and so their calls expanded in that order):
    /// a second schedule the tests run, so that what the core writes is shown not to depend on
    /// the order its work happened to finish in (rule 25). Production runs forward.
    pub(crate) backward: bool,
    /// The dependencies' decided names; `None`: every path into another crate is unknown.
    deps: Option<&'a dyn DepNames<N>>,
    /// This crate's enums' variants; `None`: every path through one of them is unknown.
    pub(crate) enums: Option<&'a dyn EnumNames<N>>,
    /// The sources of names that are enums ([`Cells::enum_at`]): a glob edge of one is written
    /// with the enum's own definition kind.
    enum_mods: ABTreeSet<'a, ModId>,
    /// The crate's edition: what `::name` starts from.
    edition: Edition,
    /// The `#[macro_use]` prelude's crates' root modules, in order (their exported macros).
    macro_use: AVec<'a, ItemKey>,
    /// Every dependency by name, sorted (the root record's): what an `extern crate` names.
    dep_roots: AVec<'a, (N, ItemKey)>,
    /// The `extern crate` items naming no dependency: the module and the name written.
    pub(crate) bad_extern: AVec<'a, (ItemKey, N)>,
    /// Visibilities naming no ancestor wherever names resolve: each its item's error (vis.rs).
    bad_vis: AVec<'a, vis::BadVisAt<'a, N>>,
    /// Explicit entries whose `pub(in path)` is read through the cells, by entry (vis.rs).
    vis_paths: ABTreeMap<'a, (ModId, Namespace, N), vis::VisPath<'a, N>>,
    /// R8's errors: a macro-written root `extern crate .. as n;` whose `n` a dependency holds,
    /// each with the fragment (an expansion at the root) it is written in.
    pub(crate) r8: AVec<'a, (FragId, EarlyAmbiguity<N>)>,
    /// The modules a `mod` line names whose text the facts do not hold, in the order met.
    pub(crate) unread: AVec<'a, ItemKey>,
    /// R6's watches for the extern prelude: a use whose first segment took a later outer scope's
    /// binding while the root's calls could still add an extern-prelude name, by name.
    extern_watches: ABTreeMap<'a, N, AVec<'a, (ItemId, Def)>>,
    /// Every macro call by its key: a block's body written by an expansion is found by it.
    call_ids: ABTreeMap<'a, ItemKey, ItemId>,
    /// What stalled calls took while their module's globs were weakly undecided
    /// ([`Hold::NamedImport`]): the call, its module's `Globs` cell of the name, the definition
    /// taken and whether the globs brought it; validated when the run ends.
    speculated: AVec<'a, (ItemId, CellId, Def, bool)>,
    /// What the stalled call being asked has taken so far ([`Taken`]).
    taken: AVec<'a, Taken<N>>,
    /// A weak view whose fixed point did not settle: the stall's error.
    weak_error: Option<CellError>,
    /// The modules of the crate's walked tree are `0..walked`: those an expansion or a block
    /// made come after (their `#[macro_export]` macros are published to the root by the
    /// record writer, not read from the root record).
    walked: ModId,
    /// D116's read log (decide.rs): while a decision runs, every cell it reads, in order.
    decide_log: Option<AVec<'a, CellId>>,
    /// The groups' textual log (groups.rs): while armed, every fragment a textual walk reads.
    textual_log: Option<AVec<'a, FragId>>,
    /// The groups' textual log of the modules outside a one-group solve's members whose
    /// textual sequence a walk read from their records ([`Cells::textual_recorded`]).
    textual_rec_log: Option<AVec<'a, ModId>>,
    /// Each module outside a one-group solve's members, filled from its record: its textual
    /// sequence there ([`Cells::fill_pending`]).
    recorded_textual: ABTreeMap<'a, ModId, RecTextual<'a, N>>,
    /// A one-group solve's records of the modules outside its members ([`Answered::Pending`]).
    outside: Option<&'a dyn Outside<N>>,
    /// A one-group solve met what it cannot answer from the members' texts and the records
    /// (the first met): the caller solves the crate whole (edit.rs).
    pub(crate) member_refusal: Option<edit::MemberRefusal>,
    /// A body solve's (body.rs) outermost block modules, each with the length of its body's
    /// module's textual sequence at the body's place: a `macro_rules!` walk out of the block
    /// reads that module's finished sequence cut there ([`Cells::textual`]). Empty otherwise.
    body_cuts: ABTreeMap<'a, ItemKey, u32>,
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
        Cells::load_reading(facts, root, deps, plant, &|_| true, alloc)
    }

    /// [`Cells::load_with`] reading the text of the modules `read` holds alone (a one-group
    /// solve's members, edit.rs): every other module of the tree is made with its parent and
    /// chain and no text, for the caller to fill from its record. The root's outer scopes (the
    /// extern and `#[macro_use]` preludes, the edition, the standard prelude) are read always.
    pub(crate) fn load_reading<F: ScopeFacts<Name = N>>(
        facts: &F,
        root: ItemKey,
        deps: Option<&'a dyn DepNames<N>>,
        plant: CellPlant,
        read: &dyn Fn(ItemKey) -> bool,
        alloc: Alloc<'a>,
    ) -> Result<Cells<'a, N>, CellError> {
        let mut cells = Cells::empty(facts, root, deps, plant, alloc)?;
        let made = cells.make_modules(facts, &[root])?;
        cells.walked = cells.modules.len() as ModId;
        cells.root = cells.by_key[&root];
        for &m in made.iter() {
            if read(cells.modules[m as usize].key) {
                cells.read_text(facts, m)?;
            } else if m == cells.root {
                cells.read_outer(facts, m)?;
            }
        }
        Ok(cells)
    }

    /// A body solve's load (cells/body.rs `solve_body_in`): the crate's root, the chain of
    /// modules `prelude` names (the crate's own standard prelude and its ancestors, empty when
    /// the prelude is a dependency's: [`Cells::read_outer`] reads a prelude of this crate as one
    /// of its modules), the root's outer scopes, and the modules `chain` names (the body's
    /// module's ancestors from the root, `BodyTree::chain`), each with its `mod` lines and no
    /// text, and the `#[macro_use]`
    /// children of each module of `chain`, to the end of that closure (a `macro_rules!` walk
    /// out of the body enters them, [`Cells::textual_recorded`]). No subtree is walked: every
    /// other module of the crate is made when a lookup first reaches it
    /// ([`Cells::reach_outside`] through the finished scopes' chains, or
    /// [`Cells::fill_pending`] as a child its parent's scope declares), so a body's load is its
    /// chain and what it reads, never its crate's whole tree (rule 77: the tree is the crate's
    /// records, made once by the crate's solve).
    pub(crate) fn load_body<F: ScopeFacts<Name = N>>(
        facts: &F,
        root: ItemKey,
        chain: &[ItemKey],
        prelude: &[ItemKey],
        deps: Option<&'a dyn DepNames<N>>,
        alloc: Alloc<'a>,
    ) -> Result<Cells<'a, N>, CellError> {
        let mut cells = Cells::empty(facts, root, deps, CellPlant::None, alloc)?;
        cells.make_modules_in(facts, &[root], Extent::Listed)?;
        let r = cells.by_key[&root];
        cells.root = r;
        cells.make_modules_in(facts, prelude, Extent::Listed)?;
        cells.read_outer(facts, r)?;
        let mut made = cells.make_modules_in(facts, chain, Extent::Listed)?;
        made.push(r);
        // The `#[macro_use]` closure from the chain (the root's own children included): each
        // round makes the children not made yet of the last round's modules; a module is made
        // once, so it ends within the crate's tree.
        while !made.is_empty() {
            let mut kids: AVec<'a, ItemKey> = AVec::new_in(alloc);
            for &m in made.iter() {
                let lines = cells.modules[m as usize].lines.iter().flatten();
                kids.extend(lines.filter(|l| l.1 && !cells.by_key.contains_key(&l.0)).map(|l| l.0));
            }
            made = cells.make_modules_in(facts, &kids, Extent::Listed)?;
        }
        cells.walked = cells.modules.len() as ModId;
        Ok(cells)
    }

    /// The cells of the crate under `root` with no module made yet: the crate's dependencies by
    /// name, its tools and built-in attributes read from its root record.
    fn empty<F: ScopeFacts<Name = N>>(facts: &F, root: ItemKey, deps: Option<&'a dyn DepNames<N>>, plant: CellPlant, alloc: Alloc<'a>) -> Result<Cells<'a, N>, CellError> {
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
            textual_stack: AVec::new_in(alloc),
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
            tools: AVec::new_in(alloc),
            builtin_attrs: AVec::new_in(alloc),
            arbitrary_ints: false,
            call_on: ABTreeMap::new_in(alloc),
            harness_calls: ABTreeSet::new_in(alloc),
            derives_on: ABTreeMap::new_in(alloc),
            after_derive_on: ABTreeMap::new_in(alloc),
            attr_after: ABTreeMap::new_in(alloc),
            helpers: ABTreeMap::new_in(alloc),
            watches: ABTreeMap::new_in(alloc),
            written: ABTreeMap::new_in(alloc),
            r6: ABTreeMap::new_in(alloc),
            expanded_uses: ABTreeMap::new_in(alloc),
            glob_vs_expanded: ABTreeMap::new_in(alloc),
            stats: CellStats::default(),
            plant,
            backward: false,
            deps,
            enums: None,
            enum_mods: ABTreeSet::new_in(alloc),
            edition: Edition::E2021,
            macro_use: AVec::new_in(alloc),
            dep_roots: AVec::new_in(alloc),
            bad_extern: AVec::new_in(alloc),
            bad_vis: AVec::new_in(alloc),
            vis_paths: ABTreeMap::new_in(alloc),
            r8: AVec::new_in(alloc),
            unread: AVec::new_in(alloc),
            extern_watches: ABTreeMap::new_in(alloc),
            call_ids: ABTreeMap::new_in(alloc),
            speculated: AVec::new_in(alloc),
            taken: AVec::new_in(alloc),
            weak_error: None,
            walked: 0,
            decide_log: None,
            textual_log: None,
            textual_rec_log: None,
            recorded_textual: ABTreeMap::new_in(alloc),
            outside: None,
            member_refusal: None,
            body_cuts: ABTreeMap::new_in(alloc),
        };
        // The dependencies by name before any text is read: an `extern crate` of any module
        // names one.
        {
            let r = facts.crate_root(info.krate).map_err(|error| CellError::Facts { module: root, error })?;
            cells.dep_roots.extend(r.deps.iter().map(|d| (d.name, d.root)));
            cells.tools.extend(r.tools.iter().copied());
            cells.builtin_attrs.extend(r.builtin_attrs.iter().copied());
            cells.arbitrary_ints = r.head.arbitrary_ints;
        }
        Ok(cells)
    }

    /// Makes the modules of the subtrees under `tops` that are not made yet (the `mod` lines of
    /// their texts; those an expansion writes come with it), numbered in key order after the
    /// modules already made, with parents and chains. Their texts are read by the caller.
    fn make_modules<F: ScopeFacts<Name = N>>(&mut self, facts: &F, tops: &[ItemKey]) -> Result<AVec<'a, ModId>, CellError> {
        self.make_modules_in(facts, tops, Extent::Subtrees)
    }

    /// [`Cells::make_modules`] to the extent `extent`: the subtrees under `tops`, or the modules
    /// `tops` alone (each with its `mod` lines, its children not made).
    fn make_modules_in<F: ScopeFacts<Name = N>>(&mut self, facts: &F, tops: &[ItemKey], extent: Extent) -> Result<AVec<'a, ModId>, CellError> {
        let alloc = self.alloc;
        let mut keys: ABTreeSet<'a, ItemKey> = ABTreeSet::new_in(alloc);
        let mut todo: AVec<'a, ItemKey> = AVec::from_slice_in(tops, alloc);
        let mut unread: ABTreeSet<'a, ItemKey> = ABTreeSet::new_in(alloc);
        let mut lines: ABTreeMap<'a, ItemKey, AVec<'a, (ItemKey, bool)>> = ABTreeMap::new_in(alloc);
        while let Some(m) = todo.pop() {
            if self.by_key.contains_key(&m) || !keys.insert(m) {
                continue;
            }
            // The module tree record's lines: no text of it is lowered for them.
            let mut own: AVec<'a, (ItemKey, bool)> = AVec::new_in(alloc);
            if facts.mod_lines(m, &mut own) {
                if extent == Extent::Subtrees {
                    todo.extend(own.iter().map(|l| l.0));
                }
                lines.insert(m, own);
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
            own.reserve(d.mods.len());
            for c in d.mods.iter() {
                if extent == Extent::Subtrees {
                    todo.push(c.module);
                }
                own.push((c.module, c.macro_use));
            }
            lines.insert(m, own);
        }
        let mut made: AVec<'a, ModId> = AVec::new_in(alloc);
        for &k in keys.iter() {
            let id = self.modules.len() as ModId;
            self.by_key.insert(k, id);
            self.modules.push(Module {
                // An unread module has none: nothing of its text is known.
                lines: lines.remove(&k),
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
                block_at: None,
                uses: 0,
                foreign: false,
                unread: unread.contains(&k),
                answered: Answered::Text,
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

    /// Makes module `key`, named `name` in `parent`, with no text: a child its parent's record
    /// declares that no `mod` line of a text read names (a module an expansion of a module
    /// outside a one-group solve's members wrote, which no call here makes). It is filled from
    /// its own record when first read ([`Answered::Pending`]).
    fn make_recorded_child(&mut self, parent: ModId, key: ItemKey, name: N) -> ModId {
        let alloc = self.alloc;
        let id = self.modules.len() as ModId;
        let mut chain: AVec<'a, ItemKey> = AVec::with_capacity_in(self.modules[parent as usize].chain.len() + 1, alloc);
        chain.push(key);
        chain.extend(self.modules[parent as usize].chain.iter().copied());
        self.by_key.insert(key, id);
        self.modules.push(Module {
            key,
            name: Some(name),
            parent: Some(parent),
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
            block_at: None,
            uses: 0,
            foreign: false,
            unread: false,
            answered: Answered::Pending,
            lines: None,
        });
        self.stats.modules += 1;
        id
    }

    /// Marks each of the modules `ms` with no text read (outside a one-group solve's members)
    /// to be filled from its group's record when first read ([`Cells::fill_pending`]).
    pub(crate) fn pend(&mut self, ms: impl Iterator<Item = ModId>) {
        for m in ms {
            let md = &mut self.modules[m as usize];
            if !md.foreign && !md.unread && md.text == FragId::MAX && md.answered == Answered::Text {
                md.answered = Answered::Pending;
            }
        }
    }

    /// Fills each pending `#[macro_use]` child of a module whose text is read: its sequence is
    /// flattened into its parent's when the parent's scope is written (write.rs).
    pub(crate) fn fill_macro_use_children(&mut self) {
        for m in 0..self.modules.len() {
            if self.modules[m].text == FragId::MAX {
                continue;
            }
            let kids = self.modules[m].lines.iter().flatten().filter(|l| l.1).filter_map(|l| self.by_key.get(&l.0).copied());
            let kids: AVec<'a, ModId> = AVec::from_iter_in(kids, self.alloc);
            for c in kids {
                self.fill_pending(c);
            }
        }
    }

    /// Fills module `m` from its group's record if it waits for its first read
    /// ([`Answered::Pending`]): its own bindings as explicit entries (no fragment), its glob
    /// edges, no items and no growth; its cells then follow the core's own rules
    /// ([`Cells::cell_value`]). Its textual sequence is kept for a `macro_rules!` walk
    /// ([`RecTextual`]), with its `#[macro_use]` children from its `mod` lines. Its glob sources
    /// are filled with it (a glob brings every name of its source, which the scopes' writing
    /// enumerates), and each child its record declares that no text read here names (a module
    /// an expansion outside wrote) is made, pending. A record that does not answer refuses the
    /// solve ([`Cells::member_refusal`], rule 3). Each module leaves `Pending` once, so the walk
    /// ends within the crate's modules.
    pub(crate) fn fill_pending(&mut self, m: ModId) {
        let Some(outside) = self.outside else { return };
        if self.modules[m as usize].answered != Answered::Pending {
            return;
        }
        let alloc = self.alloc;
        let mut todo: AVec<'a, ModId> = AVec::new_in(alloc);
        todo.push(m);
        while let Some(m) = todo.pop() {
            if self.modules[m as usize].answered != Answered::Pending {
                continue;
            }
            let module = self.modules[m as usize].key;
            let mut f = Filled {
                bindings: AVec::new_in(alloc),
                globs: AVec::new_in(alloc),
                macro_exports: false,
                textual: None,
                defs: AVec::new_in(alloc),
                marks: AVec::new_in(alloc),
            };
            let globs = match outside.fill(module, &mut f) {
                Ok(Some(globs)) => globs,
                Ok(None) => {
                    self.modules[m as usize].answered = Answered::NoRecord;
                    continue;
                }
                Err(refusal) => {
                    self.modules[m as usize].answered = Answered::NoRecord;
                    self.member_refusal.get_or_insert(refusal);
                    continue;
                }
            };
            self.modules[m as usize].answered = Answered::Record(globs);
            if f.macro_exports {
                self.member_refusal.get_or_insert(edit::MemberRefusal::MacroExports { module });
                continue;
            }
            for e in f.bindings.iter() {
                if !matches!(e.binding.state, crate::BindingState::Determined) {
                    self.member_refusal.get_or_insert(edit::MemberRefusal::Ambiguous { module });
                    break;
                }
                self.modules[m as usize].explicit.insert((e.ns, e.name), (e.binding.def, e.binding.vis, e.binding.origin, FragId::MAX));
            }
            for (i, g) in f.globs.iter().enumerate() {
                let src = match g.source.kind {
                    crate::DefKind::Enum => self.enum_at(g.source.key),
                    _ => self.module_at(g.source.key),
                };
                let Some(src) = src else {
                    self.member_refusal.get_or_insert(edit::MemberRefusal::GlobSource { module, source: g.source.key });
                    break;
                };
                self.modules[m as usize].edges.insert(i as u32, (src, g.vis));
                todo.push(src);
            }
            if let Some(digest) = f.textual {
                // Its `#[macro_use]` children by its `mod` lines; unknown when a child its marks
                // name is in none of them (an expansion wrote that `mod`), or it has no lines.
                let lines = self.modules[m as usize].lines.as_ref();
                let macro_use = lines.filter(|lines| f.marks.iter().all(|k| lines.iter().any(|l| l.0 == k.child))).map(|lines| {
                    let mut out = AVec::new_in(alloc);
                    out.extend(lines.iter().filter(|l| l.1).map(|l| l.0));
                    out
                });
                self.recorded_textual.insert(m, RecTextual { defs: f.defs, marks: f.marks, digest, macro_use });
            }
            for e in f.bindings.iter() {
                let d = e.binding.def;
                // A child the outside places ([`Outside::chain`]: a body solve's finished
                // scopes) is made when a lookup reaches it ([`Cells::reach_outside`]): a body
                // that reads the root makes none of the root's other children.
                if e.binding.origin == crate::Origin::Declared
                    && d.kind == crate::DefKind::Module
                    && !self.by_key.contains_key(&d.key)
                    && outside.chain(d.key).is_none()
                {
                    self.make_recorded_child(m, d.key, e.name);
                }
            }
        }
    }

    /// Loads the modules under `tops` not made yet, with their texts: the modules no `mod` line
    /// names (a block's module, made beside its body's module), which the caller is told of as
    /// the old fill is (`FillEnv::made`). Their imports and calls are queued: the caller runs
    /// again. How many were made.
    pub(crate) fn load_more<F: ScopeFacts<Name = N>>(&mut self, facts: &F, tops: &[ItemKey]) -> Result<usize, CellError> {
        self.load_more_at(facts, tops, &|_| None)
    }

    /// [`Cells::load_more`] with each block's place: `place(module)` is where the body whose
    /// block `module` is was written, as the lowering files it (`BodySrc::place`: the fragment's
    /// owner, a module's key for its own text or a call's key for its expansion, and the last
    /// ordered entry's position before the body there). The module's textual scope goes on
    /// outward from that place (Reference `macro.decl.scope.textual`: "When `macro_rules!` is
    /// used to define a macro, the macro enters the scope after the definition (note that it
    /// can still be used recursively, since names are looked up from the invocation site), up
    /// until its surrounding scope, typically a module, is closed."): a body's block is inside
    /// that scope. `None`: a module with no text around it (its textual scope is its own).
    pub(crate) fn load_more_at<F: ScopeFacts<Name = N>>(
        &mut self,
        facts: &F,
        tops: &[ItemKey],
        place: &dyn Fn(ItemKey) -> Option<(ItemKey, u32)>,
    ) -> Result<usize, CellError> {
        let (made, _) = self.load_more_reading(facts, tops, place, &|_| true)?;
        Ok(made)
    }

    /// [`Cells::load_more_at`] reading the text of the modules `read` holds alone (a one-group
    /// solve's members, edit.rs): how many were made, and those made with no text, for the
    /// caller to fill from their records (`switch.rs`, `record_module`).
    pub(crate) fn load_more_reading<F: ScopeFacts<Name = N>>(
        &mut self,
        facts: &F,
        tops: &[ItemKey],
        place: &dyn Fn(ItemKey) -> Option<(ItemKey, u32)>,
        read: &dyn Fn(ItemKey) -> bool,
    ) -> Result<(usize, AVec<'a, ModId>), CellError> {
        let made = self.make_modules(facts, tops)?;
        let mut outside: AVec<'a, ModId> = AVec::new_in(self.alloc);
        for &m in made.iter() {
            if read(self.modules[m as usize].key) {
                self.read_text(facts, m)?;
            } else {
                outside.push(m);
            }
        }
        // The place is kept as written and found when first read ([`Cells::line_of`]): the
        // body may be written by an expansion of a call of this same batch, read only later.
        for &m in made.iter() {
            if self.plant != CellPlant::BlockHasNoPlace {
                self.modules[m as usize].block_at = place(self.modules[m as usize].key);
            }
        }
        Ok((made.len(), outside))
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
            self.read_outer(facts, m)?;
        }
        let f = self.read_fragment(m, &d, None)?;
        self.modules[m as usize].text = f;
        Ok(())
    }

    /// The root `m`'s outer scopes (R6, R8) from its crate's root record: read whether or not
    /// the root's text is (a one-group solve whose group the root is not in).
    fn read_outer<F: ScopeFacts<Name = N>>(&mut self, facts: &F, m: ModId) -> Result<(), CellError> {
        let key = self.modules[m as usize].key;
        {
            let r = facts.crate_root(self.krate).map_err(|error| CellError::Facts { module: key, error })?;
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
        Ok(())
    }

    /// An explicit entry of `m`; the first of a name stays. A cell of it decided not found is a
    /// bug in when cells decide (finality). A second item of one name (E0428) is not reported
    /// yet: the injected `extern crate` comes last and must yield silently (it is def-site
    /// hygienic: thiserror's `extern crate std as core;` under `#![no_std]`), which the decl
    /// does not mark yet.
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
        for (k, it) in d.items.iter().enumerate() {
            self.add_declared(m, it.ns, it.name, it.def, &it.vis, fid, (vis::ORD_ITEM, k as u32))?;
            self.stats.names_declared += 1;
            if up.is_some() {
                self.wrote(m, it.ns, it.name, it.def, fid);
            }
        }
        for (k, c) in d.mods.iter().enumerate() {
            self.add_declared(m, Namespace::Types, c.name, Def::module(c.module), &c.vis, fid, (vis::ORD_MOD, k as u32))?;
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
        for (k, ec) in d.extern_crates.iter().enumerate() {
            let vis = self.import_vis(m, &ec.vis, vis::VisAt { module: m, frag: fid, ord: (vis::ORD_EXTERN_CRATE, k as u32) })?;
            self.extern_crate(m, ec.target, ec.rename, vis, fid, up.is_some())?;
        }
        for r in d.macro_rules.iter() {
            let def = Def { key: r.def, kind: DefKind::Macro(r.shapes) };
            slots.push((r.pos, Slot::Rules(r.name, def, r.export)));
            if r.export {
                // Also a crate-root name in the macro namespace; the root record already holds
                // those of module texts, so the same def adds nothing.
                let root = self.root;
                self.add_explicit(root, Namespace::Macros, r.name, (def, Visibility::Public, Origin::MacroExport, fid))?;
            }
        }
        // The fragment's expansion depth: a call written at rustc's default `recursion_limit`
        // or deeper is an error, not a call (the old core's `CallState::TooDeep`).
        let depth = match up {
            Some((u, _)) => self.depth(u).saturating_add(1),
            None => 0,
        };
        let deep = depth >= crate::RECURSION_LIMIT && self.plant != CellPlant::NoRecursionLimit;
        for c in d.calls.iter() {
            // A tool attribute (Reference `attributes.tool`: "The tool attributes are ..
            // `rustfmt` and `clippy`", and `register_tool`'s): inert, no call. The first segment
            // is a tool module only in an attribute's plain path of two segments or more.
            if c.kind == CallShape::Attr
                && c.path.kind == PathKind::Plain
                && c.path.segments.len() >= 2
                && self.tools.contains(&c.path.segments[0])
                && self.plant != CellPlant::ToolAttrIsACall
            {
                self.stats.tool_attrs += 1;
                continue;
            }
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
                call: Some(CallAt { key: c.call, frag: fid, pos: c.pos, shape: c.kind }),
                // A call past the recursion limit adds nothing: grown from the start, so a read
                // after it in its fragment (the textual scope) never waits on it.
                grown: deep,
                expansion: None,
                at: fid,
                pos: c.pos,
                of_enum: false,
                types_only: false,
                why: None,
                clamp: None,
                seg: 0,
                deep,
            });
            self.call_ids.insert(c.call, id);
            if c.role == CallRole::ProcMacroHarness {
                self.harness_calls.insert(id);
            }
            if let Some(on) = c.item {
                self.call_on.insert(id, on);
                if c.role != CallRole::Call {
                    let calls = self
                        .after_derive_on
                        .entry((fid, on))
                        .or_insert_with(|| AfterDeriveCalls { takers: AVec::new_in(alloc), before_derive: AVec::new_in(alloc), keep: None });
                    match c.role {
                        CallRole::AfterDerive => calls.takers.push(id),
                        CallRole::AfterDeriveBeforeDerive => {
                            calls.takers.push(id);
                            calls.before_derive.push(id);
                        }
                        CallRole::Keep => calls.keep = Some(id),
                        // Never on an item (its `item` is `None`): registered below.
                        CallRole::Call | CallRole::ProcMacroHarness => {}
                    }
                }
                match c.kind {
                    CallShape::Derive => self.derives_on.entry((fid, on)).or_insert_with(|| AVec::new_in(alloc)).push(id),
                    CallShape::Attr if c.path.kind == PathKind::Plain && c.path.segments.len() == 1 => {
                        let last = self.attr_after.entry((fid, on)).or_insert(c.pos);
                        *last = (*last).max(c.pos);
                    }
                    CallShape::Attr | CallShape::FnLike => {}
                }
            }
            slots.push((c.pos, Slot::Call(id)));
            if deep {
                // Never resolved, never waited on: its scope's `RecursionLimit` error.
                self.stats.too_deep += 1;
                continue;
            }
            let module = &mut self.modules[m as usize];
            module.calls.push(id);
            module.growth += 1;
            self.item_queue.insert(id);
            self.stats.calls += 1;
        }
        // Positions are unique in a fragment: no sort order is left open.
        slots.sort_unstable_by_key(|s| s.0);
        self.frags.push(Frag { module: m, up, slots });
        for (k, u) in d.uses.iter().enumerate() {
            let vis = self.import_vis(m, &u.vis, vis::VisAt { module: m, frag: fid, ord: (vis::ORD_USE, k as u32) })?;
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
                clamp: None,
                seg: 0,
                deep: false,
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
        if let Ok(k) = self.outer.externs.binary_search_by(|e| e.0.cmp(&name)) {
            if matches!(rename, Rename::To(_)) {
                let first = Def::module(self.outer.externs[k].1);
                self.r8.push((fid, EarlyAmbiguity { ns: Namespace::Types, name, kind: AmbiguityKind::MoreExpandedVsOuter, first, second: Some(def) }));
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
                    let e = EarlyAmbiguity { ns: Namespace::Types, name, kind: AmbiguityKind::MoreExpandedVsOuter, first: od, second: Some(def) };
                    self.r6_error(u, e);
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
        // Read now, before `grown` wakes its waiters (such an attribute among them,
        // [`Cells::derive_helper`]); only where an attribute follows, so a derive no attribute
        // follows never asks (a built-in derive's helpers are refused by the facts).
        if at.shape == CallShape::Derive
            && !self.helpers.contains_key(&def.key)
            && let Some(&on) = self.call_on.get(&i)
            && self.attr_after.get(&(at.frag, on)).is_some_and(|&last| last > at.pos)
        {
            let mut names: AVec<'a, N> = AVec::new_in(self.alloc);
            facts.derive_helpers(def.key, &mut names).map_err(|error| CellError::Helpers { call: at.key, derive: def.key, error })?;
            names.sort_unstable();
            names.dedup();
            self.helpers.insert(def.key, names);
        }
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

    // A written visibility's walk, its errors and `pub(in path)` through an import: vis.rs.

    /// Whether `m` is a block's module ([`crate::blocks::is_block_module`]): an anonymous
    /// scope, passed over by a written `self` and `super`.
    fn anonymous(&self, m: ModId) -> bool {
        self.modules[m as usize].name.is_some_and(|n| crate::blocks::is_block_module(n.as_ref()))
    }

    /// The module a written `self` names from `m`: `m`, or for a block's module the nearest
    /// module around it that is no block's (the Reference, `paths.qualifiers.mod-self`; a block
    /// is an anonymous scope). A block's module is made under its body's module, which is
    /// loaded first, so its parent is always held and the walk ends at a module that is no
    /// block's.
    fn normal(&self, m: ModId) -> ModId {
        let mut at = m;
        while self.anonymous(at) {
            let Some(p) = self.modules[at as usize].parent else { break };
            at = p;
        }
        at
    }

    /// The module `super` written `n` times names from `m` (`n == 0`: `self`): [`Cells::normal`]
    /// of `m`, then `n` times its parent's [`Cells::normal`] (the Reference,
    /// `paths.qualifiers.super`). `None`: past the root.
    fn up(&self, m: ModId, n: u8) -> Option<ModId> {
        let mut at = self.normal(m);
        for _ in 0..n {
            at = self.normal(self.modules[at as usize].parent?);
        }
        Some(at)
    }

    /// Whether import `i` is the `use super::*;` a block's module opens with
    /// ([`crate::expanded_blocks::render`] writes it as the first import of the module's own
    /// text): the block's lexical scope. It reaches the module's own parent (the block around
    /// it, or the body's module), where a written `super` passes over blocks ([`Cells::up`]). A
    /// `use super::*;` the user wrote in the block comes after it and keeps Rust's meaning.
    fn lexical_glob(&self, i: ItemId) -> bool {
        let it = &self.items[i as usize];
        it.call.is_none()
            && it.glob
            && it.index == 0
            && it.kind == PathKind::Super(1)
            && it.segments.is_empty()
            && it.at == self.modules[it.module as usize].text
            && self.anonymous(it.module)
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
        if let Some(m) = self.reach_outside(key) {
            return Some(m);
        }
        let found = self.foreign(key);
        if found.is_none() {
            self.stats.dep_absent += 1;
        }
        found
    }

    /// Module `key` of this crate when the outside places it ([`Outside::chain`]) and it is
    /// not made yet: a body solve's glob of a module an expansion wrote outside the body
    /// (serde_core's every `use crate::lib::*;`, `lib` written by `crate_root!()`; libc's
    /// `use crate::prelude::*;` and the root's `pub use unix::*;` under `cfg_if!`), whose
    /// source no `mod` line of a text read here names. Each ancestor below the deepest one
    /// made here is filled from its finished scope, root side first ([`Cells::fill_pending`]),
    /// which makes the next one as the child its scope declares; the ancestors' scopes are
    /// then reads of the solve, as for any path through them. `None`: no outside places it,
    /// or a filled ancestor does not declare the next (the caller then refuses the module by
    /// name, rule 3). Bounded by the chain's length; each module leaves `Pending` once.
    fn reach_outside(&mut self, key: ItemKey) -> Option<ModId> {
        if key.crate_prefix() != self.krate.prefix() {
            return None;
        }
        let outside = self.outside?;
        let chain = outside.chain(key)?;
        // The deepest ancestor made here (the root is always made: `load_reading`'s first).
        let made = chain.iter().position(|k| self.by_key.contains_key(k))?;
        for i in (1..=made).rev() {
            let parent = self.by_key[&chain[i]];
            let child = chain[i - 1];
            self.fill_pending(parent);
            if self.by_key.contains_key(&child) {
                continue;
            }
            // A parent filled already, or being filled now (its bindings read, its globs being
            // followed: libc's root reaching `unix` from its own `pub use unix::*;`), makes the
            // children its record declares only after its globs: the one this reach needs is
            // made here, by the name the parent's record declares it under.
            if !matches!(self.modules[parent as usize].answered, Answered::Record(_)) {
                return None;
            }
            let name = self.modules[parent as usize].explicit.iter().find(|(_, e)| e.0.key == child && e.0.kind == DefKind::Module && e.2 == Origin::Declared).map(|(&(_, n), _)| n)?;
            self.make_recorded_child(parent, child, name);
        }
        self.by_key.get(&key).copied()
    }

    fn foreign(&mut self, key: ItemKey) -> Option<ModId> {
        if let Some(&m) = self.by_key.get(&key) {
            return Some(m);
        }
        let deps = self.deps?;
        if self.plant == CellPlant::DepIsOutside || !deps.has_module(key) {
            return None;
        }
        Some(self.decided_module(key))
    }

    /// The enum `key` as a source of names (its variants), when its variants are held: this
    /// crate's by [`EnumNames`], a dependency's by [`DepNames`]. Reference `items.use.restrictions.variant`
    /// has `use MyEnum::MyVariant;` as the accepted form, and `paths` reads a path's prefix
    /// through an enum to its variants as through a module to its items.
    fn enum_at(&mut self, key: ItemKey) -> Option<ModId> {
        if self.plant == CellPlant::EnumIsOutside {
            return None;
        }
        if let Some(&m) = self.by_key.get(&key) {
            return Some(m);
        }
        let held = if key.crate_prefix() == self.krate.prefix() {
            self.enums.is_some_and(|e| e.holds(key))
        } else {
            self.plant != CellPlant::DepIsOutside && self.deps.is_some_and(|d| d.has_enum(key))
        };
        if !held {
            return None;
        }
        let id = self.decided_module(key);
        self.enum_mods.insert(id);
        Some(id)
    }

    /// A source of names whose cells are decided when made (a dependency's module, an enum):
    /// no text, items or growth here.
    fn decided_module(&mut self, key: ItemKey) -> ModId {
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
            block_at: None,
            uses: 0,
            foreign: true,
            unread: false,
            answered: Answered::Text,
            lines: None,
        });
        id
    }

    fn cell(&mut self, m: ModId, ns: Namespace, name: N, layer: Layer) -> CellId {
        let id = self.make_cell(m, ns, name, layer);
        if let Some(log) = self.decide_log.as_mut() {
            log.push(id);
        }
        id
    }

    fn make_cell(&mut self, m: ModId, ns: Namespace, name: N, layer: Layer) -> CellId {
        let key = (m, cell_slot(ns, layer), name);
        if let Some(&c) = self.cell_ids.get(&key) {
            return c;
        }
        // A module outside a one-group solve's members is filled when its first cell is made.
        self.fill_pending(m);
        let id = self.cells.len() as CellId;
        if self.modules[m as usize].answered == Answered::NoRecord && self.member_refusal.is_none() {
            // A one-group solve read a module it holds neither the text nor a record of.
            self.member_refusal = Some(edit::MemberRefusal::NoRecord { module: self.modules[m as usize].key });
        }
        if self.modules[m as usize].unread || self.modules[m as usize].answered == Answered::NoRecord {
            // Its text was never read: unknown, decided when made.
            self.cells.push(Cell { module: m, ns, name, layer, value: Some(Value::Outside), waiters: AVec::new_in(self.alloc), waiting_on: None });
            self.cell_ids.insert(key, id);
            self.stats.cells += 1;
            self.stats.decisions += 1;
            return id;
        }
        if self.modules[m as usize].foreign {
            // Decided already (in the dependency, or an enum's variants): read, never waited
            // on. A reader gone since the module was made is unknown, never "not there".
            let of = self.modules[m as usize].key;
            let answer = if of.crate_prefix() == self.krate.prefix() {
                self.enums.map(|e| e.variant(of, ns, name))
            } else {
                self.stats.dep_reads += 1;
                self.deps.map(|d| d.name_in(of, ns, name))
            };
            let value = answer.map_or(Value::Outside, DepAnswer::value);
            self.cells.push(Cell { module: m, ns, name, layer, value: Some(value), waiters: AVec::new_in(self.alloc), waiting_on: None });
            self.cell_ids.insert(key, id);
            self.stats.cells += 1;
            self.stats.decisions += 1;
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
                match self.own_value(m, ns, name, guess) {
                    Step::Done(Some(v)) => return Step::Done(v),
                    Step::Done(None) => {}
                    Step::Wait(on) => return Step::Wait(on),
                }
                let g = self.cell(m, ns, name, Layer::Globs);
                match self.current(g, guess) {
                    Some(v) => Step::Done(v),
                    None => Step::Wait(Node::Cell(g)),
                }
            }
            Layer::Globs => self.globs_value(c, guess, skip),
        }
    }

    /// The half of `m`'s `Full` cell of `name` its own text decides: an item or `mod` it
    /// declares, else what its named imports of the name bind; `None` when neither binds it (the
    /// cell is then its `Globs` layer). A function of the module alone and the cells a
    /// `pub(in path)` reads: the binding cell a group record defines (groups.rs).
    pub(super) fn own_value(&mut self, m: ModId, ns: Namespace, name: N, guess: Option<&Guess<'a>>) -> Step<Option<Value>> {
                if let Some(&(def, vis, ..)) = self.modules[m as usize].explicit.get(&(ns, name)) {
                    // A `pub(in path)` read through the cells waits on them (vis.rs).
                    return match self.entry_vis(m, ns, name, vis, guess) {
                        Step::Done(Some(vis)) => Step::Done(Some(Value::Found(def, vis))),
                        Step::Done(None) => Step::Done(Some(Value::Outside)),
                        Step::Wait(on) => Step::Wait(on),
                    };
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
                    return Step::Done(Some(Value::Found(def, vis)));
                }
                Step::Done(outside.then_some(Value::Outside))
    }

    /// `c`'s value, a `Globs` cell: what `m`'s glob imports bring (see [`Cells::cell_value`]).
    fn globs_value(&mut self, c: CellId, guess: Option<&Guess<'a>>, skip: Skip) -> Step<Value> {
        let Cell { module: m, ns, name, .. } = self.cells[c as usize];
        {
            {
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
                                            let vis = match it.clamp {
                                                Some(c) if self.plant != CellPlant::EnumGlobNotClamped => self.narrower(it.vis, c, m),
                                                _ => it.vis,
                                            };
                                            edges.push((s, vis));
                                        }
                                    }
                                    Some(Some(_)) => {}
                                    // The weak view: a glob not resolved yet brings nothing now.
                                    Some(None) if skip == Skip::Weak => {}
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
                // A glob of the module whose path stage 1 does not follow brings what is unknown:
                // decided so, or (a member of the group) guessed so, as its edge is read above.
                let mut outside = self.modules[m as usize].glob_items.iter().any(|&g| {
                    let o = self.items[g as usize].outcome.or_else(|| guess.and_then(|gs| gs.items.get(&g).copied().flatten()));
                    o == Some(Outcome::Outside) && !(ns == Namespace::Macros && self.items[g as usize].of_enum)
                });
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
                        if skip == Skip::Call(j) || matches!(skip, Skip::Growth | Skip::Weak) || self.items[j as usize].grown || guess.is_some_and(|g| g.items.contains_key(&j)) {
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
        match self.try_item(i, None, Hold::Undecided) {
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

    /// Item `i`'s answer from its reads; `hold`: what holds a plain first segment's lookup.
    fn try_item(&mut self, i: ItemId, guess: Option<&Guess<'a>>, hold: Hold) -> Step<Outcome> {
        // A proc-macro crate's harness: no path to read, its expansion its own input.
        if self.harness_calls.contains(&i) {
            return Step::Done(Outcome::Called(PROC_MACRO_HARNESS));
        }
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
        // An attribute on an item whose attributes after a derive may take it is first answered
        // by them (rustc's `take_first_attr`, attr-bang-kind-2): once one took the item, every
        // later call on it is that macro's input (inert, never resolved); the keep call is called
        // when none did, its expansion the item itself ([`KEEP_ITEM`]).
        if let Some(at) = self.items[i as usize].call
            && at.shape == CallShape::Attr
        {
            match self.item_fate(i, at, guess) {
                Step::Done(Fate::Open) => {}
                Step::Done(Fate::Taken) => return Step::Done(Outcome::Inert),
                Step::Done(Fate::Kept) => return Step::Done(Outcome::Called(KEEP_ITEM)),
                // Only under a group's guess: an answer no stall picks (`stall_with_calls`
                // picks `Called`, `Inert` and `Outside`), so the call waits for the real one.
                Step::Done(Fate::Undecided) => return Step::Done(Outcome::Failed(Unresolved::NotFound)),
                Step::Wait(n) => {
                    self.items[i as usize].waits.push(n);
                    return Step::Wait(n);
                }
            }
        }
        // A derive's helper (the Reference, "Procedural Macros", "Derive macro helper
        // attributes"): a one-segment attribute after a derive on its item, named in that
        // derive's helpers, is inert. The innermost scope of an attribute (rustc's
        // `Scope::DeriveHelpers`), so read before the textual scope.
        if let Some(at) = self.items[i as usize].call
            && at.shape == CallShape::Attr
            && kind == PathKind::Plain
            && self.items[i as usize].segments.len() == 1
        {
            let name = self.items[i as usize].segments[0];
            match self.derive_helper(i, at, name, guess) {
                Step::Done(true) => return Step::Done(Outcome::Inert),
                Step::Done(false) => {}
                Step::Wait(n) => {
                    self.items[i as usize].waits.push(n);
                    return Step::Wait(n);
                }
            }
        }
        if let Some(at) = self.items[i as usize].call
            && kind == PathKind::Plain
            && self.items[i as usize].segments.len() == 1
            && self.plant != CellPlant::NoTextualScope
        {
            // A one-segment call looks in the textual scope first (Reference `macros.by-example.scope`).
            let name = self.items[i as usize].segments[0];
            match self.textual(at.frag, at.pos, name, Some(at.shape), guess) {
                // An attribute `macro_rules!` named like a built-in attribute: the textual scope
                // is nearer than the built-in attribute scope, and the two are ambiguous.
                Step::Done(Some(_)) if self.builtin_attr(at, name) => {
                    return Step::Done(Outcome::Failed(Unresolved::Ambiguous(AmbiguityKind::BuiltinAttr)));
                }
                Step::Done(Some(def)) => return Step::Done(self.called(i, at, def)),
                Step::Done(None) => {}
                Step::Wait(n) => {
                    self.items[i as usize].waits.push(n);
                    return Step::Wait(n);
                }
            }
        }
        self.items[i as usize].clamp = None;
        self.items[i as usize].seg = 0;
        // The first segment the walk reads (`::dep` reads its crate from the extern prelude).
        let mut first = 0;
        let mut cur = match kind {
            PathKind::Crate => self.root,
            // A block's lexical glob reaches the scope around the block ([`Cells::lexical_glob`]).
            PathKind::Super(1) if self.lexical_glob(i) => match self.modules[m as usize].parent {
                Some(p) => p,
                None => return Step::Done(Outcome::Failed(Unresolved::NoParent)),
            },
            // A written `self` or `super` passes over blocks' modules ([`Cells::up`]).
            PathKind::SelfMod => self.normal(m),
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
                let Some(&s) = self.items[i as usize].segments.first() else {
                    return Step::Done(Outcome::Failed(Unresolved::NotFound));
                };
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
                if len < 2 && !glob {
                    // `use ::name;` / `use ::name as n;`: the crate itself, bound in the type
                    // namespace at the import's own visibility (Reference
                    // `paths.qualifiers.global-root.intro`: "Paths starting with `::` are
                    // considered to be global paths where the segments of the path start being
                    // resolved from a place which differs based on edition"; from 2018 on "they
                    // must be followed by the name of a crate"; `names.preludes.extern`: the
                    // extern prelude's names are the crates themselves).
                    if self.items[i as usize].call.is_some() || self.plant == CellPlant::CrateAloneUnknown {
                        return self.unfollowed(i, Unfollowed::CrateAlone);
                    }
                    return Step::Done(Outcome::Named([Some((Def::module(named), ivis)), None, None]));
                }
                first = 1;
                match self.module_at(named) {
                    Some(r) => r,
                    None => return self.unfollowed(i, Unfollowed::CrateUnread),
                }
            }
        };
        let len = self.items[i as usize].segments.len();
        // `use crate as name;` (and `$crate` alone): the path names the crate root itself, a
        // module, bound in the type namespace at the import's visibility (serde's `pub use
        // crate as serde;`, read by its derives' `use serde::private::serde as _serde;`).
        if len == 0 && !glob && self.items[i as usize].call.is_none() && matches!(kind, PathKind::Crate | PathKind::DollarCrate(_)) && self.plant != CellPlant::CrateRootImportFails {
            return Step::Done(Outcome::Named([Some((Def::module(self.modules[cur as usize].key), ivis)), None, None]));
        }
        let steps = if glob { len } else { len.saturating_sub(1) };
        for k in first..steps {
            let s = self.items[i as usize].segments[k];
            self.items[i as usize].seg = k as u32;
            // The kind of an ambiguity `first_segment` answers (`GlobVsOuter` where a glob's
            // candidate meets a different outer one); every other ambiguity here is two globs'.
            let mut amb_kind = AmbiguityKind::GlobVsGlob;
            let v = if kind == PathKind::Plain && k == 0 {
                match self.first_segment(m, Namespace::Types, s, i, guess, hold, &mut amb_kind) {
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
                Value::Ambiguous(..) => return Step::Done(Outcome::Failed(Unresolved::Ambiguous(amb_kind))),
                Value::Found(def, vis) if def.kind == DefKind::Enum && self.enum_at(def.key).is_some() => {
                    // Through an enum whose variants are held: its variants are its names.
                    if let Some(e) = self.enum_at(def.key) {
                        cur = e;
                        self.items[i as usize].clamp = Some(vis);
                    }
                }
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
        self.items[i as usize].seg = len.saturating_sub(1) as u32;
        let Some(&leaf) = self.items[i as usize].segments.last() else {
            return Step::Done(Outcome::Failed(Unresolved::NotFound));
        };
        if let Some(at) = self.items[i as usize].call {
            // The path-based scope of a call: the leaf in the macro namespace. In its own module
            // the call reads its own view, which does not wait on its own growth.
            let mut amb_kind = AmbiguityKind::GlobVsGlob;
            let v = if kind == PathKind::Plain && len == 1 {
                // A one-segment call the textual scope missed: lexically, R6.
                self.first_segment(m, Namespace::Macros, leaf, i, guess, hold, &mut amb_kind)
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
            // A built-in attribute's name (`outer_of`'s built-in attribute scope): the built-in
            // attribute, inert, when no nearer scope binds an attribute macro of the name; else
            // the nearer one and the built-in are ambiguous (E0659, rustc's
            // `AmbiguityKind::BuiltinAttr`: "built-in attribute vs any other name").
            if kind == PathKind::Plain
                && len == 1
                && self.builtin_attr(at, leaf)
                && let Step::Done(found) = &v
            {
                match *found {
                    Value::Found(d, _) if d == BUILTIN_ATTR => return Step::Done(Outcome::Inert),
                    Value::Found(..) | Value::Ambiguous(..) => {
                        return Step::Done(Outcome::Failed(Unresolved::Ambiguous(AmbiguityKind::BuiltinAttr)));
                    }
                    Value::NotFound | Value::Outside => {}
                }
            }
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
                Step::Done(Value::Ambiguous(..)) => Step::Done(Outcome::Failed(Unresolved::Ambiguous(amb_kind))),
                Step::Done(Value::Found(def, _)) => Step::Done(self.called(i, at, def)),
            };
        }
        // An import never justifies itself: of its own name in its own module it reads the globs.
        let layer = if cur == m && binds == Some(leaf) { Layer::Globs } else { Layer::Full };
        let mut out = [None; 3];
        let (mut found, mut hidden, mut amb, mut wait, mut outside) = (false, false, false, None, false);
        let mut amb_kind = AmbiguityKind::GlobVsGlob;
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
                    match self.textual(frag, pos, leaf, None, guess) {
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
                        self.fill_pending(self.root);
                        let exported = self.modules[self.root as usize].explicit.get(&(Namespace::Macros, leaf)).is_some_and(|e| e.0 == def && e.2 == Origin::MacroExport);
                        Read::Ready(Value::Found(def, if exported { Visibility::Public } else { Visibility::Crate(self.krate) }))
                    }
                    // Else the leaf is a first segment (R6-R8), in each namespace.
                    None => match self.first_segment(m, ns, leaf, i, guess, hold, &mut amb_kind) {
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
                    // Only a fully public `use` (rustc's hack tests `import.vis.is_public()`):
                    // a narrower one keeps the binding's own visibility.
                    let of_extern = ns == Namespace::Types
                        && ivis == Visibility::Public
                        && self.plant != CellPlant::ExternReexportNarrowed
                        && self.modules[through as usize].explicit.get(&(ns, leaf)).is_some_and(|e| e.0 == def && e.2 == Origin::ExternCrate);
                    out[ns_at(ns)] = Some((def, if of_extern { ivis } else { self.narrower(ivis, vis, m) }));
                }
            }
        }
        if amb {
            // An ambiguous namespace fails the import at once (R2, R7 by its kind).
            return Step::Done(Outcome::Failed(Unresolved::Ambiguous(amb_kind)));
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
        let whole = matches!(skip, Skip::Growth | Skip::Weak);
        if !whole
            && let Some(v) = self.current(c, guess)
        {
            return Step::Done((v, From::Own));
        }
        if let Some(&(def, vis, ..)) = self.modules[m as usize].explicit.get(&(ns, name)) {
            let from = self.written.get(&(m, ns, name)).map_or(From::Own, |&f| From::Wrote(f));
            let value = match self.entry_vis(m, ns, name, vis, guess) {
                Step::Done(Some(vis)) => Value::Found(def, vis),
                Step::Done(None) => Value::Outside,
                Step::Wait(on) => return Step::Wait(on),
            };
            return Step::Done((value, from));
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
                (None, None) if skip == Skip::Weak && self.plant == CellPlant::StallPassesNamedImport => None,
                // A named import that may still bind the name holds every view of it, the
                // weak one too: what it binds shadows the globs' candidate and any outer one,
                // so nothing is selected before it is decided.
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
        if !whole
            && let Some(v) = self.current(g, guess)
        {
            return Step::Done((v, From::Glob));
        }
        match self.cell_value(g, guess, skip) {
            Step::Done(v) => Step::Done((v, From::Glob)),
            Step::Wait(n) => Step::Wait(n),
        }
    }

    /// Module `m`'s own view of `name` in `ns` for the stalled call `me`, with every scope that
    /// is only weakly undecided taken as it is now ([`Hold::NamedImport`]): the undecided cells
    /// of `name` its globs reach through resolved glob imports are decided together, as their
    /// least fixed point, with no call's growth and no unresolved glob import adding anything.
    /// A pending named import of `name` in `m` or in a module those globs reach holds the view
    /// (`Step::Wait`): it may bind the name, and nothing is taken before it is decided.
    fn weak_view(&mut self, m: ModId, ns: Namespace, name: N, me: ItemId) -> Step<(Value, From)> {
        match self.weak_guess(m, ns, name, me) {
            Step::Done(view) => self.own_view(m, ns, name, me, Skip::Weak, Some(&view)),
            Step::Wait(n) => Step::Wait(n),
        }
    }

    /// What `m`'s globs bring of `name` in `ns` now, by the weak view's rule.
    fn weak_globs(&mut self, m: ModId, ns: Namespace, name: N, me: ItemId) -> Step<Value> {
        match self.weak_guess(m, ns, name, me) {
            Step::Done(view) => {
                let g = self.cell(m, ns, name, Layer::Globs);
                self.cell_value(g, Some(&view), Skip::Weak)
            }
            Step::Wait(n) => Step::Wait(n),
        }
    }

    /// The weak view's cells: every undecided cell of `name` in `ns` that `m`'s `Globs` cell
    /// reaches through resolved glob imports, each with its value at their least fixed point.
    fn weak_guess(&mut self, m: ModId, ns: Namespace, name: N, me: ItemId) -> Step<Guess<'a>> {
        let alloc = self.alloc;
        let mut view = Guess { cells: ABTreeMap::new_in(alloc), items: ABTreeMap::new_in(alloc) };
        let mut todo: AVec<'a, CellId> = AVec::new_in(alloc);
        todo.push(self.cell(m, ns, name, Layer::Globs));
        let mut srcs: AVec<'a, ModId> = AVec::new_in(alloc);
        while let Some(c) = todo.pop() {
            if self.cells[c as usize].value.is_some() || view.cells.insert(c, None).is_some() {
                continue;
            }
            let Cell { module, layer, .. } = self.cells[c as usize];
            match layer {
                Layer::Full => todo.push(self.cell(module, ns, name, Layer::Globs)),
                Layer::Globs => {
                    srcs.clear();
                    srcs.extend(self.modules[module as usize].edges.values().map(|x| x.0));
                    for &src in srcs.iter() {
                        todo.push(self.cell(src, ns, name, Layer::Full));
                    }
                }
            }
        }
        // Each value only rises (as in `fixpoint`), so the rounds are bounded.
        let mut members: AVec<'a, CellId> = AVec::new_in(alloc);
        members.extend(view.cells.keys().copied());
        let bound = 4 * members.len() + 4;
        let mut rounds = 0;
        loop {
            let mut changed = false;
            for &c in members.iter() {
                let v = match self.cell_value(c, Some(&view), Skip::Weak) {
                    Step::Done(v) => Some(v).filter(|v| *v != Value::NotFound),
                    Step::Wait(on) => return Step::Wait(on),
                };
                if view.cells.insert(c, v) != Some(v) {
                    changed = true;
                }
            }
            if !changed {
                break;
            }
            rounds += 1;
            if rounds > bound {
                self.weak_error = Some(CellError::NoFixpoint { module: self.modules[m as usize].key, cells: members.len() });
                return Step::Wait(Node::Item(me));
            }
        }
        Step::Done(view)
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
    ///
    /// `sub`: a call's shape; a scope's candidate of the wrong sub-namespace is passed over
    /// ([`same_sub`]) and the walk goes on to the next scope.
    fn outer_of(&mut self, ns: Namespace, name: N, sub: Option<CallShape>, guess: Option<&Guess<'a>>) -> Step<Option<Value>> {
        let wrong = |v: Value| matches!(v, Value::Found(d, _) if sub.is_some_and(|s| !same_sub(d, s)));
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
                    Read::Ready(v @ (Value::Found(_, Visibility::Public) | Value::Ambiguous(_, Visibility::Public))) if !wrong(v) => return Step::Done(Some(v)),
                    Read::Ready(Value::Outside) => unknown = true,
                    Read::Ready(_) => {}
                    Read::Wait(c) => return Step::Wait(Node::Cell(c)),
                }
            }
            if unknown {
                return Step::Done(Some(Value::Outside));
            }
        }
        if ns == Namespace::Macros
            && sub == Some(CallShape::Attr)
            && self.builtin_attrs.contains(&name)
        {
            // The built-in attribute scope, after the `#[macro_use]` prelude and before the
            // standard prelude (rustc's `Scope::BuiltinAttrs`): an attribute call's name that is
            // a built-in attribute is bound to it here. An import passes no `sub`: never bound.
            return Step::Done(Some(Value::Found(BUILTIN_ATTR, Visibility::Public)));
        }
        if let Some(p) = self.outer.local {
            // A name the standard prelude lacks goes on to the language prelude.
            return match self.read(p, ns, name, Layer::Full, guess) {
                Read::Wait(c) => Step::Wait(Node::Cell(c)),
                Read::Ready(Value::NotFound) if self.plant != CellPlant::NoLanguagePrelude => Step::Done(self.builtin(ns, name)),
                Read::Ready(Value::NotFound) => Step::Done(None),
                // The language prelude after it holds no macro (`builtin`).
                Read::Ready(v) if wrong(v) => Step::Done(None),
                Read::Ready(v) => Step::Done(Some(v)),
            };
        }
        if let Some(&def) = self.outer.foreign.get(&(ns, name))
            && !wrong(Value::Found(def, Visibility::Public))
        {
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
            // Under `#![blaze::arbitrary_ints]` an integer of any width too (`u24`, `i80`).
            Namespace::Types => crate::builtin_type_in(name.as_ref(), self.arbitrary_ints).map(|d| Value::Found(d, Visibility::Public)),
            Namespace::Macros | Namespace::Values => None,
        }
    }

    /// Call `at` is an attribute whose one-segment name `name` is a built-in attribute's.
    fn builtin_attr(&self, at: CallAt, name: N) -> bool {
        at.shape == CallShape::Attr && self.builtin_attrs.contains(&name)
    }

    /// Whether attribute call `i` (one segment, `name`, written at `at`) names a helper of a
    /// derive written before it on the same item of the same fragment: in scope on the item the
    /// derive is applied to, from the derive on (the Reference, "Derive macro helper
    /// attributes"). A derive not grown yet is waited for (its helpers are read as it expands,
    /// [`Cells::expand_call`], before it grows); under a stall's guess its guessed outcome is
    /// taken, and a derive whose helpers are not read yet then gives none. Reads only the
    /// derives of the call's own item (`derives_on`), never the module's calls; then, when the
    /// fragment is the output of an attribute call on an item, the derives written before that
    /// call on that item, and so up the fragments: rustc's `Scope::DeriveHelpers` walks the
    /// parent expansions, so a helper the taking macro wrote back (derive_where's
    /// `#[derive_where(..)]` under its `derive_where_visited`, reth-codecs' `#[reth_codecs(..)]`
    /// under `add_arbitrary_tests`) stays inert in its output.
    fn derive_helper(&self, i: ItemId, at: CallAt, name: N, guess: Option<&Guess<'a>>) -> Step<bool> {
        let Some(&on) = self.call_on.get(&i) else { return Step::Done(false) };
        let (mut frag, mut on, mut before) = (at.frag, on, at.pos);
        loop {
            for &d in self.derives_on.get(&(frag, on)).map_or(&[][..], |ds| &ds[..]) {
                let it = &self.items[d as usize];
                if it.pos >= before {
                    continue;
                }
                let outcome = if it.grown {
                    it.outcome
                } else {
                    match guess.and_then(|g| g.items.get(&d)) {
                        Some(&o) => o,
                        None => return Step::Wait(Node::Item(d)),
                    }
                };
                if let Some(Outcome::Called(def)) = outcome
                    && self.helpers.get(&def.key).is_some_and(|h| h.binary_search(&name).is_ok())
                {
                    return Step::Done(true);
                }
            }
            let Some((up, pos)) = self.frags[frag as usize].up else { return Step::Done(false) };
            let slots = &self.frags[up as usize].slots;
            let Ok(k) = slots.binary_search_by_key(&pos, |s| s.0) else { return Step::Done(false) };
            let Slot::Call(call) = slots[k].1 else { return Step::Done(false) };
            if self.items[call as usize].call.is_none_or(|c| c.shape != CallShape::Attr) {
                return Step::Done(false);
            }
            let Some(&up_on) = self.call_on.get(&call) else { return Step::Done(false) };
            (frag, on, before) = (up, up_on, pos);
        }
    }

    /// A plain path's first segment `name` (or a one-segment path's leaf) in `ns` at `m`, asked
    /// by item `me`: R6 (the module first; growth by expansion alone does not hold the lookup,
    /// the outer binding is taken and a watch left), R7 (a pending import is waited for; a glob
    /// candidate beside a different outer one is an ambiguity, the same definition is one, D57),
    /// R8 (an extern-prelude name is final).
    ///
    /// `hold`: what holds the lookup in `m` ([`Hold`]). A stalled call asks with
    /// [`Hold::NamedImport`]: `m`'s view is the weak one ([`Cells::weak_view`]), a candidate its
    /// globs bring now is taken at once, a miss goes outward, and what was taken is left in
    /// `self.taken` for the stall to keep (Reference `names.resolution.expansion.speculation`:
    /// "All name resolution candidates selected during macro expansion are considered
    /// speculative. Once the crate has been fully expanded, all speculative import resolutions
    /// are validated to ensure that macro expansion did not introduce any new ambiguities.").
    ///
    /// `amb_kind`: set to `GlobVsOuter` when the answer is ambiguous because a glob's candidate
    /// meets a different outer one (`...ambiguity.glob-vs-outer`); left as it is otherwise.
    #[expect(clippy::too_many_arguments, reason = "the lookup's inputs, and the ambiguity kind it answers beside its value")]
    fn first_segment(&mut self, mut m: ModId, ns: Namespace, name: N, me: ItemId, guess: Option<&Guess<'a>>, hold: Hold, amb_kind: &mut AmbiguityKind) -> Step<Value> {
        let weak = hold == Hold::NamedImport;
        let (inner, from) = loop {
            let view = if weak { self.weak_view(m, ns, name, me) } else { self.own_view(m, ns, name, me, Skip::Growth, guess) };
            let (inner, from) = match view {
                Step::Done(x) => x,
                Step::Wait(n) => return Step::Wait(n),
            };
            // A block's module whose only glob is its lexical one ([`Cells::lexical_glob`]) and that
            // binds `name` itself nowhere: its candidate is the scope around the block's, not a
            // glob's. The Reference, `names.scopes.block` / "Statements", "Item declarations": a
            // block's names are its own item declarations, and a name it lacks is looked up in the
            // scope around it, lexically; that walk is no glob import, so the glob-vs-outer rule
            // (`names.resolution.expansion.imports.ambiguity.glob-vs-outer`) is the enclosing
            // module's to apply, against its own kind of binding. (`use dep::emits_impl as format;
            // fn g() { format!(); }`: the module's named import shadows the `#[macro_use]`
            // prelude's `format`; taken as a glob's candidate it was GlobVsOuter, the call failed
            // and its impl was never written.) A block with a glob of its own keeps the glob
            // reading: its candidates and the outer ones are then really a glob's and the outer's.
            if from == From::Glob
                && self.anonymous(m)
                && self.plant != CellPlant::LexicalGlobIsAGlob
                && self.modules[m as usize].glob_items.iter().all(|&g| self.lexical_glob(g))
                && let Some(p) = self.modules[m as usize].parent
            {
                m = p;
                continue;
            }
            break (inner, from);
        };
        if inner == Value::Outside {
            return Step::Done(Value::Outside);
        }
        // A call's name: a candidate of the wrong sub-namespace is ignored, here and outward.
        let sub = match self.items[me as usize].call {
            Some(at) if ns == Namespace::Macros && self.plant != CellPlant::SubNamespaceIgnored => Some(at.shape),
            _ => None,
        };
        let inner = match inner {
            Value::Found(d, _) if sub.is_some_and(|s| !same_sub(d, s)) => Value::NotFound,
            v => v,
        };
        let outer = match self.outer_of(ns, name, sub, guess) {
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
                    // `...ambiguity.glob-vs-outer`: the import fails at its first segment, and
                    // its error names this kind (not two globs').
                    *amb_kind = AmbiguityKind::GlobVsOuter;
                    return Step::Done(Value::Ambiguous(d, v));
                }
                // Every candidate a macro call's path selects is speculative (the rule quoted
                // below): its macro name, and the first segment of `a::m!` in the type namespace.
                let speculative = match ns {
                    Namespace::Macros => self.plant != CellPlant::MacroGlobWaits,
                    Namespace::Types | Namespace::Values => self.items[me as usize].call.is_some() && self.plant != CellPlant::CallPrefixGlobWaits,
                };
                if let From::Wrote(f) = from
                    && speculative
                    && !self.encloses(f, self.items[me as usize].at)
                {
                    // The speculation rule in the other order: the expansion's definition came
                    // first, beside a different candidate of the module's globs, and does not
                    // enclose this use. The answer is the one every order gives (Reference
                    // `names.resolution.expansion.expansion-order-stability`): the glob's
                    // candidate, with the error (`...macros.ambiguity.more-expanded-vs-outer`).
                    let g = self.cell(m, ns, name, Layer::Globs);
                    let globbed = match self.current(g, guess) {
                        Some(v) => Step::Done(v),
                        None if weak => self.weak_globs(m, ns, name, me),
                        None => self.cell_value(g, guess, Skip::Growth),
                    };
                    let globbed = match globbed {
                        Step::Done(v) => v,
                        Step::Wait(n) => return Step::Wait(n),
                    };
                    if let Value::Found(gd, _) | Value::Ambiguous(gd, _) = globbed
                        && gd != d
                    {
                        let e = EarlyAmbiguity { ns, name, kind: AmbiguityKind::GlobVsExpanded, first: gd, second: Some(d) };
                        if weak {
                            self.taken.push(Taken::Error(e));
                        } else if guess.is_none() {
                            self.r6_error(me, e);
                        }
                        return Step::Done(globbed);
                    }
                }
                if let From::Wrote(_) = from
                    && !weak
                    && guess.is_none()
                {
                    // GlobVsExpanded (rustc's "forbid expanded shadowing to avoid time travel"):
                    // the name an expansion wrote into `m` shadows a glob candidate this use
                    // would have taken before the expansion (Reference
                    // `names.resolution.expansion.expansion-order-stability`: "names in the
                    // fully expanded AST must resolve to the same definition regardless of the
                    // order in which macros are expanded and imports are resolved"). The
                    // module's globs are read when the run ends (`validate_expanded`), never
                    // waited on here: a glob of `m` may itself wait on this use.
                    let g = self.cell(m, ns, name, Layer::Globs);
                    self.expanded_uses.insert((me, ns), (name, d, g));
                }
                if let From::Wrote(f) = from
                    && let Some(o) = outer
                    && let Some(od) = outer_def
                    && od != d
                    && !self.encloses(f, self.items[me as usize].at)
                {
                    // R6 in the other order: the inner definition came first, from an expansion
                    // that does not enclose the use, beside a different outer binding. The
                    // answer is the one every order gives: the outer binding, with the error at
                    // the expansion's definition (wide seed 8 with calls: the reference's).
                    let e = EarlyAmbiguity { ns, name, kind: AmbiguityKind::MoreExpandedVsOuter, first: od, second: Some(d) };
                    if weak {
                        self.taken.push(Taken::Error(e));
                    } else if guess.is_none() {
                        self.r6_error(me, e);
                    }
                    if self.plant != CellPlant::InnerAfterWrite {
                        return Step::Done(o);
                    }
                }
                if from == From::Glob && weak {
                    // At a stall the candidate the globs bring now is selected (the rule quoted
                    // below), whatever those globs still wait on: a later write of the name
                    // into `m` is the watch's error, another answer of the globs the
                    // validation's when the run ends.
                    if !speculative || self.plant == CellPlant::StallFailsGlobCall {
                        return Step::Wait(Node::Item(me));
                    }
                    let g = self.cell(m, ns, name, Layer::Globs);
                    self.taken.push(Taken::Watch(m, ns, name, d, AmbiguityKind::GlobVsExpanded));
                    self.taken.push(Taken::Globs(g, d, true));
                } else if from == From::Glob
                    && let Some(j) = self.growing(m, me, guess)
                {
                    if !speculative {
                        // A glob candidate may still be shadowed by what a call writes.
                        return Step::Wait(Node::Item(j));
                    }
                    // In the macro namespace the glob's candidate is selected now (Reference
                    // `names.resolution.expansion.speculation`: "All name resolution candidates
                    // selected during macro expansion are considered speculative. Once the
                    // crate has been fully expanded, all speculative import resolutions are
                    // validated to ensure that macro expansion did not introduce any new
                    // ambiguities."): the module's other calls are not waited for (two derives
                    // of one item named through a glob would wait on each other and fail,
                    // against `...unresolved-invocations`: "Every macro invocation resolves to
                    // a valid definition"); a later write of the name by an expansion that
                    // does not enclose this use is the error, by the watch.
                    if guess.is_none() {
                        self.watches.entry((m, ns, name)).or_insert_with(|| AVec::new_in(self.alloc)).push((me, d, AmbiguityKind::GlobVsExpanded));
                    }
                }
                Step::Done(inner)
            }
            Value::Outside => Step::Done(Value::Outside),
            Value::NotFound => match outer {
                Some(o) if weak => {
                    // The scope binds nothing now and is at most weakly undecided: the lookup
                    // goes on outward and takes the first candidate, speculatively.
                    let refused = match (o, ns) {
                        (Value::Outside, _) => CellPlant::StallFailsOutside,
                        (_, Namespace::Macros) => CellPlant::StallFailsOuterCall,
                        (_, Namespace::Types | Namespace::Values) => CellPlant::StallFailsOuterPrefixCall,
                    };
                    if self.plant == refused {
                        return Step::Wait(Node::Item(me));
                    }
                    if let Some(od) = outer_def {
                        let g = self.cell(m, ns, name, Layer::Globs);
                        self.taken.push(Taken::Watch(m, ns, name, od, AmbiguityKind::MoreExpandedVsOuter));
                        self.taken.push(Taken::Globs(g, od, false));
                    }
                    Step::Done(o)
                }
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
                            self.watches.entry((m, ns, name)).or_insert_with(|| AVec::new_in(self.alloc)).push((me, od, AmbiguityKind::MoreExpandedVsOuter));
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

    /// R6's error `e` at item `me` (a use or a call). A second error at one item keeps the
    /// least, so what is written does not depend on which the work met first (rule 25).
    fn r6_error(&mut self, me: ItemId, e: EarlyAmbiguity<N>) {
        let mut new = false;
        let had = self.r6.entry(me).or_insert_with(|| {
            new = true;
            e
        });
        if e < *had {
            *had = e;
        }
        if new {
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
        let ws: AVec<'a, (ItemId, Def, AmbiguityKind)> = AVec::from_slice_in(ws, self.alloc);
        for &(u, od, kind) in ws.iter() {
            // The outer definition itself is no other candidate (D57).
            if od != def && !self.encloses(f, self.items[u as usize].at) {
                self.r6_error(u, EarlyAmbiguity { ns, name, kind, first: od, second: Some(def) });
            }
        }
    }

    /// What the calls written after a derive on attribute call `i`'s item (`i` written at `at`)
    /// decided for it (rustc's `take_first_attr`, attr-bang-kind-2): `Taken` when one of them is
    /// called with a macro (only those written before `i` when `i` is one of them); else `Kept`
    /// for the item's keep call, `Open` for any other. A call past the recursion limit (grown
    /// with no outcome) takes nothing. One not decided is waited for; under a group's guess
    /// (d116-decide-4's `fixpoint` and `stall_with_calls`) one of the group is never read from
    /// the guess: the answer is `Undecided`, which no stall picks, so no call on the item (the
    /// keep call, a later taker) is decided before the takers before it really are. Reads only
    /// the calls of the call's own item.
    fn item_fate(&self, i: ItemId, at: CallAt, guess: Option<&Guess<'a>>) -> Step<Fate> {
        let Some(&on) = self.call_on.get(&i) else { return Step::Done(Fate::Open) };
        let Some(calls) = self.after_derive_on.get(&(at.frag, on)) else { return Step::Done(Fate::Open) };
        for &t in calls.takers.iter() {
            if t == i {
                break;
            }
            let it = &self.items[t as usize];
            let outcome = if it.outcome.is_some() || it.grown {
                it.outcome
            } else if guess.is_some_and(|g| g.items.contains_key(&t)) {
                return Step::Done(Fate::Undecided);
            } else {
                return Step::Wait(Node::Item(t));
            };
            if let Some(Outcome::Called(def)) = outcome
                && def != KEEP_ITEM
            {
                return Step::Done(Fate::Taken);
            }
        }
        Step::Done(if calls.keep == Some(i) { Fate::Kept } else { Fate::Open })
    }

    /// Whether call `i` (written at `at`) is a [`CallRole::AfterDerive`] call of its item.
    fn is_taker(&self, i: ItemId, at: CallAt) -> bool {
        let Some(&on) = self.call_on.get(&i) else { return false };
        self.after_derive_on.get(&(at.frag, on)).is_some_and(|calls| calls.takers.contains(&i))
    }

    /// Whether call `i` (written at `at`) is a [`CallRole::AfterDeriveBeforeDerive`] call: a
    /// `#[derive]` is written after it on its item.
    fn derive_follows(&self, i: ItemId, at: CallAt) -> bool {
        let Some(&on) = self.call_on.get(&i) else { return false };
        self.after_derive_on.get(&(at.frag, on)).is_some_and(|calls| calls.before_derive.contains(&i))
    }

    /// A call written at `at` that selected `def`: called when `def` takes the call's shape;
    /// else refused by name. Its sub-namespace's candidates alone were looked at
    /// ([`same_sub`]), so what is refused here is the other shape of that sub-namespace (an
    /// attribute macro named by `#[derive(..)]`, a derive by `#[..]`) or, for a path of two or
    /// more segments, any macro the path's module holds under that name. Call `i` in a derive
    /// helper's place ([`Cells::after_derive`]) that is no helper and finds an attribute macro:
    /// on the item itself (an after-derive call, [`Cells::is_taker`]) it is called and takes
    /// the item, as rustc's `take_first_attr` expands it after the derives, unless a `#[derive]`
    /// is written after it ([`Cells::derive_follows`]): that derive, sent on the item as
    /// written, would run again on the macro's output, so the call is refused
    /// `DeriveAfterAttrMacro` and the item kept; on a variant or a field (names-attr-4's helper
    /// checks, which have no input) it is refused `WrongMacroKind`, as rustc refuses it
    /// ("expected non-macro attribute, found attribute macro").
    fn called(&self, i: ItemId, at: CallAt, def: Def) -> Outcome {
        match def.kind {
            DefKind::Macro(s) if s.accepts(at.shape) || self.plant == CellPlant::SubNamespaceIgnored => {
                if !self.after_derive(i, at) {
                    Outcome::Called(def)
                } else if !self.is_taker(i, at) {
                    Outcome::Failed(Unresolved::WrongMacroKind)
                } else if self.derive_follows(i, at) {
                    Outcome::Failed(Unresolved::DeriveAfterAttrMacro)
                } else {
                    Outcome::Called(def)
                }
            }
            _ => Outcome::Failed(Unresolved::WrongMacroKind),
        }
    }

    /// Whether attribute call `i`, written at `at`, follows a derive written on its item in its
    /// fragment: a derive helper's place (the Reference, "Derive macro helper attributes").
    /// Reads only the derives of the call's own item (`derives_on`).
    fn after_derive(&self, i: ItemId, at: CallAt) -> bool {
        if at.shape != CallShape::Attr {
            return false;
        }
        let Some(&on) = self.call_on.get(&i) else { return false };
        self.derives_on.get(&(at.frag, on)).is_some_and(|derives| derives.iter().any(|&d| self.items[d as usize].pos < at.pos))
    }

    /// The text of `m` defines a `macro_rules!` named `name`.
    fn has_rules(&self, m: ModId, name: N) -> bool {
        self.frags[self.modules[m as usize].text as usize].slots.iter().any(|s| matches!(s.1, Slot::Rules(n, ..) if n == name))
    }

    /// `name!` by the textual scope just before `pos` of fragment `frag` (Reference
    /// `macros.by-example.scope`): backward through the text, the latest definition first, into
    /// `#[macro_use]` modules and expansions met on the way, then out to the enclosing text at
    /// each call and `mod` line. A call met that has not grown yet may still define the name:
    /// the walk waits on it (under a guess a call of the group adds nothing). `sub`: a call's
    /// shape, whose wrong sub-namespace's definitions are passed over ([`same_sub`]); `None`
    /// for an import, which takes any.
    fn textual(&mut self, frag: FragId, pos: u32, name: N, sub: Option<CallShape>, guess: Option<&Guess<'a>>) -> Step<Option<Def>> {
        self.stats.textual_reads += 1;
        let (mut f, mut end) = (frag, pos);
        self.textual_stack.clear();
        loop {
            let below = self.frags[f as usize].slots.partition_point(|s| s.0 < end);
            self.textual_stack.push((f, below));
            // Every fragment the walk reads, its slots before the cut or all of them (one it
            // finds empty there is read too): groups.rs's textual cells.
            if let Some(log) = self.textual_log.as_mut() {
                log.push(f);
            }
            while let Some(top) = self.textual_stack.last_mut() {
                if top.1 == 0 {
                    self.textual_stack.pop();
                    continue;
                }
                top.1 -= 1;
                let (top_f, top_i) = *top;
                let (_, slot) = self.frags[top_f as usize].slots[top_i];
                match slot {
                    Slot::Rules(n, def, _) if n == name && (self.plant == CellPlant::SubNamespaceIgnored || sub.is_none_or(|s| same_sub(def, s))) => {
                        return Step::Done(Some(def));
                    }
                    Slot::Rules(..) | Slot::Mod(_, false) => {}
                    Slot::Mod(child, true) => {
                        let t = self.modules[child as usize].text;
                        if t == FragId::MAX {
                            // A `#[macro_use]` child outside the group: its textual sequence
                            // from its record; one not held is refused (rule 3: never skipped).
                            match self.textual_recorded(child, Cut::Whole, name, sub) {
                                Ok(Some(def)) => return Step::Done(Some(def)),
                                Ok(None) => {}
                                Err(module) => {
                                    let refusal = self.textual_refusal(module);
                                    self.member_refusal.get_or_insert(refusal);
                                }
                            }
                            continue;
                        }
                        self.textual_stack.push((t, self.frags[t as usize].slots.len()));
                        if let Some(log) = self.textual_log.as_mut() {
                            log.push(t);
                        }
                    }
                    Slot::Call(j) => {
                        let it = &self.items[j as usize];
                        if it.grown {
                            if let Some(x) = it.expansion {
                                self.textual_stack.push((x, self.frags[x as usize].slots.len()));
                                if let Some(log) = self.textual_log.as_mut() {
                                    log.push(x);
                                }
                            }
                        } else if !guess.is_some_and(|g| g.items.contains_key(&j)) {
                            return Step::Wait(Node::Item(j));
                        }
                    }
                }
            }
            let (up, module) = (self.frags[f as usize].up, self.frags[f as usize].module);
            let out = match up {
                Some(u) => Some(u),
                None => {
                    // Out through each parent outside a one-group solve's members, its
                    // sequence before this `mod` line read from its record, up to the first
                    // parent whose text is read here (or the root). The chain bounds it.
                    let mut at = module;
                    for _ in 0..self.modules[module as usize].chain.len() {
                        let Some(p) = self.modules[at as usize].parent else { break };
                        self.fill_pending(p);
                        // A body solve's outermost block (body.rs): out of the body, the scope is
                        // the crate's FINISHED textual scope at the body's place, an input fixed
                        // outside (D189): its module's flattened sequence before the place, then
                        // each ancestor's before its child's mark ([`Cells::textual_finished`]).
                        if let Some(&len) = self.body_cuts.get(&self.modules[at as usize].key)
                            && matches!(self.modules[p as usize].answered, Answered::Record(_))
                        {
                            match self.textual_finished(p, len, name, sub) {
                                Ok(found) => return Step::Done(found),
                                Err(module) => {
                                    let refusal = self.textual_refusal(module);
                                    self.member_refusal.get_or_insert(refusal);
                                    return Step::Done(None);
                                }
                            }
                        }
                        let md = &self.modules[at as usize];
                        if md.mod_line.is_some() || md.block_at.is_some() || !matches!(self.modules[p as usize].answered, Answered::Record(_)) {
                            break;
                        }
                        let child = md.key;
                        match self.textual_recorded(p, Cut::Child(child), name, sub) {
                            Ok(Some(def)) => return Step::Done(Some(def)),
                            Ok(None) => at = p,
                            Err(module) => {
                                let refusal = self.textual_refusal(module);
                                self.member_refusal.get_or_insert(refusal);
                                return Step::Done(None);
                            }
                        }
                    }
                    match self.line_of(at, guess) {
                        Step::Done(l) => l,
                        Step::Wait(n) => return Step::Wait(n),
                    }
                }
            };
            match out {
                Some((p, at)) => {
                    f = p;
                    end = at;
                }
                None => return Step::Done(None),
            }
        }
    }

    /// `name!` by the textual sequence of module `p` as its record holds it (a module outside a
    /// one-group solve's members): before a child's `mod` line ([`Cut::Child`]) or in the whole
    /// of it ([`Cut::Whole`], a
    /// `#[macro_use]` child walked into), the latest definition first. The walk over text
    /// ([`Cells::textual`]) would read `p`'s fragments and each `#[macro_use]` child's met on
    /// the way back to the definition found, or all of them: those are logged
    /// (`textual_rec_log`), each from its own record, so the group's reads are the whole
    /// solve's. `Err(module)`: a module the walk reads whose sequence is not held here.
    fn textual_recorded(&mut self, p: ModId, cut: Cut, name: N, sub: Option<CallShape>) -> Result<Option<Def>, ItemKey> {
        let alloc = self.alloc;
        let key = self.modules[p as usize].key;
        self.fill_pending(p);
        let Some(r) = self.recorded_textual.get(&p) else { return Err(key) };
        let mut found: Option<(usize, Def)> = None;
        let end = match cut {
            Cut::Whole => r.defs.len(),
            Cut::Child(c) => match r.marks.iter().find(|k| k.child == c) {
                Some(k) => k.len as usize,
                None => return Err(key),
            },
        };
        for (i, d) in r.defs.iter().enumerate().take(end) {
            let def = Def { key: d.def, kind: DefKind::Macro(d.shapes) };
            if d.name == name && (self.plant == CellPlant::SubNamespaceIgnored || sub.is_none_or(|s| same_sub(def, s))) {
                found = Some((i, def));
            }
        }
        // The modules the walk reads: `p` (before `cut`), then each `#[macro_use]` child it
        // meets going back to the definition, with where that definition is inside it. Each
        // module once (the tree), so the list ends within the crate's modules (rule 18).
        let mut todo: AVec<'a, (ModId, Cut, Option<usize>)> = AVec::new_in(alloc);
        todo.push((p, cut, found.map(|(i, _)| i)));
        let mut steps = 0usize;
        while let Some((t, cut, at)) = todo.pop() {
            steps += 1;
            let tkey = self.modules[t as usize].key;
            if steps > self.modules.len() {
                return Err(tkey);
            }
            self.fill_pending(t);
            // Its `#[macro_use]` children, whose sequences are flattened into its own.
            let kids = self.recorded_textual.get(&t).and_then(|r| r.macro_use.as_ref()).map(|m| AVec::from_iter_in(m.iter().filter_map(|k| self.by_key.get(k).copied()), alloc));
            for &c in kids.iter().flatten() {
                self.fill_pending(c);
            }
            let Some(r) = self.recorded_textual.get(&t) else { return Err(tkey) };
            let Some(macro_use) = r.macro_use.as_ref() else { return Err(tkey) };
            let mut met: AVec<'a, (ModId, Option<usize>)> = AVec::new_in(alloc);
            for k in r.marks.iter() {
                match cut {
                    Cut::Child(c) if k.child == c => break,
                    Cut::Whole | Cut::Child(_) => {}
                }
                if !macro_use.contains(&k.child) {
                    continue;
                }
                let Some(&c) = self.by_key.get(&k.child) else { return Err(k.child) };
                let Some(flat) = self.recorded_textual.get(&c).map(|r| r.defs.len()) else { return Err(k.child) };
                let (from, to) = (k.len as usize, k.len as usize + flat);
                // Met going back from the end (or the cut) to the definition at `i`: the
                // child's sequence ends after it, and the definition is inside it or before.
                match at {
                    None => met.push((c, None)),
                    Some(i) if to > i => met.push((c, (i >= from).then(|| i - from))),
                    Some(_) => {}
                }
            }
            if let Some(log) = self.textual_rec_log.as_mut() {
                log.push(t);
            }
            todo.extend(met.iter().map(|&(c, at)| (c, Cut::Whole, at)));
        }
        Ok(found.map(|(_, def)| def))
    }

    /// `name!` out of a body (a body solve, D189): the crate's finished textual scope at the
    /// body's place, which the crate solve fixed before the body is solved. Module `p` (the
    /// body's module) is read in its finished sequence before `len` definitions (the body's
    /// cut, `HeldBody::cut`); then each module up the tree in its finished sequence before its
    /// child's mark, to the root (Reference `macro.decl.scope.textual`: a `macro_rules!` is in
    /// scope after its definition, into child modules, and out of a `#[macro_use]` module).
    /// Each sequence is the scope's own `textual` as write.rs flattened it (its `#[macro_use]`
    /// children's and its expansions' definitions in place, every `mod` line's mark, a
    /// macro-written `mod` too), so the walk never needs a module's `mod` lines: the case
    /// [`Cells::textual_recorded`] refuses for a parent whose `mod` an expansion wrote
    /// (`cfg_if! { mod soft; }`), which refused every body of such a crate that calls a name no
    /// nearer sequence defines (`vec!`, `format!`: a one-segment call asks the textual scope
    /// first). Every module read is filled through the solve's [`Outside`], so the body's read
    /// list holds its scope (crate::body_scope `reads_digest`). `Err(module)`: a module the walk
    /// needs whose finished sequence is not held, or that marks no place for the child it came
    /// up from (rule 3: refused, never taken as the end of the scope).
    fn textual_finished(&mut self, p: ModId, len: u32, name: N, sub: Option<CallShape>) -> Result<Option<Def>, ItemKey> {
        let plant = self.plant;
        let mut at = p;
        let mut child: Option<ItemKey> = None;
        // The tree bounds the walk (rule 18): one step per module of `p`'s chain.
        for _ in 0..=self.modules[p as usize].chain.len() {
            self.fill_pending(at);
            let key = self.modules[at as usize].key;
            let Some(r) = self.recorded_textual.get(&at) else { return Err(key) };
            let end = match child {
                None => len as usize,
                Some(c) => match r.marks.iter().find(|k| k.child == c) {
                    Some(k) => k.len as usize,
                    // A module with no `macro_rules!` at all shows nothing before any line.
                    None if r.defs.is_empty() => 0,
                    None => return Err(key),
                },
            };
            if end > r.defs.len() {
                return Err(key);
            }
            let hit = r.defs[..end].iter().rev().find(|d| {
                let def = Def { key: d.def, kind: DefKind::Macro(d.shapes) };
                d.name == name && (plant == CellPlant::SubNamespaceIgnored || sub.is_none_or(|s| same_sub(def, s)))
            });
            if let Some(d) = hit {
                return Ok(Some(Def { key: d.def, kind: DefKind::Macro(d.shapes) }));
            }
            let Some(parent) = self.modules[at as usize].parent else { return Ok(None) };
            child = Some(key);
            at = parent;
        }
        Err(self.modules[p as usize].key)
    }

    /// The refusal of a textual walk that needs the sequence of `module`, outside a one-group
    /// solve's members, and was not given it: `NoRecord` when no record holds the module (the
    /// lazy driver demands its group and asks again, lazy.rs; the edit driver solves whole),
    /// `Textual` when its record holds it but not the sequence the walk needs.
    fn textual_refusal(&mut self, module: ItemKey) -> edit::MemberRefusal {
        let Some(&m) = self.by_key.get(&module) else { return edit::MemberRefusal::Textual { module } };
        self.fill_pending(m);
        match self.modules[m as usize].answered {
            Answered::NoRecord => edit::MemberRefusal::NoRecord { module },
            _ => edit::MemberRefusal::Textual { module },
        }
    }

    /// Where module `m`'s own text stands in the text around it: its `mod` line, or, for a
    /// block's module, the place of its body (every entry at or before the body's place is
    /// before the block). A body an expansion writes is found once its call has grown: until
    /// then the read waits on that call. `None`: the root, or a block whose body no fragment
    /// held here holds (its call did not resolve here): nothing outside is in its scope.
    fn line_of(&mut self, m: ModId, guess: Option<&Guess<'a>>) -> Step<Option<(FragId, u32)>> {
        if let Some(l) = self.modules[m as usize].mod_line {
            return Step::Done(Some(l));
        }
        if let Some(p) = self.modules[m as usize].parent
            && self.modules[m as usize].block_at.is_none()
            && self.modules[p as usize].text == FragId::MAX
        {
            // The walk goes out into a parent whose text is not read here and whose record did
            // not answer its sequence before this line ([`Cells::textual`] read it from its
            // record where one holds it): a parent with no record is demanded (`NoRecord`,
            // lazy.rs), one whose record does not give it refused (rule 3: never taken as the
            // end of the scope).
            let module = self.modules[p as usize].key;
            let refusal = self.textual_refusal(module);
            self.member_refusal.get_or_insert(refusal);
            return Step::Done(None);
        }
        let Some((owner, pos)) = self.modules[m as usize].block_at else { return Step::Done(None) };
        if let Some(&o) = self.by_key.get(&owner) {
            self.fill_pending(o);
        }
        let frag = match self.by_key.get(&owner) {
            // A block of a body in the text of a module outside a one-group solve's members:
            // its record marks no body's place (rule 3: refused, never the end of the scope).
            Some(&o) if matches!(self.modules[o as usize].answered, Answered::Record(_)) => {
                let module = self.modules[o as usize].key;
                self.member_refusal.get_or_insert(edit::MemberRefusal::Textual { module });
                return Step::Done(None);
            }
            Some(&o) => Some(self.modules[o as usize].text).filter(|&t| t != FragId::MAX),
            None => match self.call_ids.get(&owner) {
                Some(&j) if self.items[j as usize].grown => self.items[j as usize].expansion,
                Some(&j) if !guess.is_some_and(|g| g.items.contains_key(&j)) => return Step::Wait(Node::Item(j)),
                _ => None,
            },
        };
        if let Some(f) = frag {
            self.modules[m as usize].mod_line = Some((f, pos + 1));
        }
        Step::Done(frag.map(|f| (f, pos + 1)))
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
                // A glob of an enum brings its variants as visible as the enum was read there
                // (Reference `vis.default`: "Enum variants in a pub enum are also public by
                // default": a variant is as visible as its enum, never wider).
                let vis = match self.items[i as usize].clamp {
                    Some(c) if self.plant != CellPlant::EnumGlobNotClamped => self.narrower(self.items[i as usize].vis, c, m),
                    _ => self.items[i as usize].vis,
                };
                self.modules[m as usize].edges.insert(index, (s, vis));
                // The scopes' writing enumerates every name of a glob's source.
                self.fill_pending(s);
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
            let next = if self.backward { self.item_queue.pop_last() } else { self.item_queue.pop_first() };
            if let Some(i) = next {
                // A call past the recursion limit is never evaluated (its error is written).
                if !self.items[i as usize].deep {
                    self.eval(i);
                }
                continue;
            }
            if !self.quiet_point()? {
                self.validate_speculated();
                self.validate_expanded();
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
        self.import_at(frag, index).is_some_and(|i| self.r6.contains_key(&i))
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
                        // An explicit entry's `pub(in path)` read through the cells (vis.rs).
                        if let Some(on) = self.vis_wait(m, ns, name) {
                            out.push(on);
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
            if self.items[i as usize].outcome.is_none() && !self.items[i as usize].deep {
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

    /// One evaluation of `n` under `guess` (a group's member, or a node on D116's walk): the
    /// value it takes as a guess (not found as the bottom), or the undecided node it read.
    fn member_step(&mut self, n: Node, guess: &Guess<'a>) -> Step<GuessStep> {
        match n {
            Node::Cell(c) => match self.cell_value(c, Some(guess), Skip::No) {
                Step::Done(v) => Step::Done(GuessStep::Cell(Some(v).filter(|v| *v != Value::NotFound))),
                Step::Wait(on) => Step::Wait(on),
            },
            Node::Item(i) => match self.try_item(i, Some(guess), Hold::Undecided) {
                Step::Done(o) => Step::Done(GuessStep::Item(Some(o))),
                Step::Wait(on) => Step::Wait(on),
            },
        }
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
                let step = self.member_step(n, &guess);
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
        self.conclude(members, &guess)
    }

    /// A group at its least fixed point `guess` is decided: a stall with calls, or every member
    /// (shared by the quiet point and D116's walk). R5's "only the members that resolved are
    /// decided that round" needs no round here: `guess` is the group's least fixed point, so
    /// what the resolved members bring is already read by the rest (d116-decide-4: without the
    /// round, 0 differences on std's 12 crates and tempo's 506).
    fn conclude(&mut self, members: &[Node], guess: &Guess<'a>) -> Result<(), CellError> {
        let has_call = members.iter().any(|&n| matches!(n, Node::Item(i) if self.items[i as usize].call.is_some()));
        if has_call {
            return self.stall_with_calls(members, guess);
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

    /// Reference `names.resolution.expansion.speculation`: "Once the crate has been fully
    /// expanded, all speculative import resolutions are validated to ensure that macro
    /// expansion did not introduce any new ambiguities." What each stalled call took while its
    /// module's globs were weakly undecided is read again in the module's `Globs` cell: a
    /// candidate the globs brought must be their answer still; beside an outer scope's the
    /// globs bring nothing, or that same definition (D57). Anything else is the error at the
    /// call (`...ambiguity.glob-vs-outer`).
    fn validate_speculated(&mut self) {
        if self.plant == CellPlant::SpeculationUnchecked {
            return;
        }
        for k in 0..self.speculated.len() {
            let (i, g, def, globbed) = self.speculated[k];
            match self.cells[g as usize].value {
                Some(Value::Found(d, _)) if d == def => {}
                Some(Value::NotFound) if !globbed => {}
                // Unknown here, or not decided: no other answer is shown.
                Some(Value::Outside) | None => {}
                Some(v @ (Value::Found(..) | Value::Ambiguous(..) | Value::NotFound)) => {
                    // The globs' answer now: another definition beside the one taken, or none
                    // (the candidate the stall took is gone, no second definition to name).
                    let second = match v {
                        Value::Found(d, _) | Value::Ambiguous(d, _) => Some(d),
                        Value::NotFound | Value::Outside => None,
                    };
                    let c = &self.cells[g as usize];
                    let e = EarlyAmbiguity { ns: c.ns, name: c.name, kind: AmbiguityKind::GlobVsOuter, first: def, second };
                    self.r6_error(i, e);
                }
            }
        }
    }

    /// GlobVsExpanded, when the run ends: each use whose plain first segment took a name an
    /// expansion wrote into its module ([`Cells::first_segment`] records it) is the error where
    /// the module's globs, decided now, bring a different definition of the name (the Reference
    /// `names.resolution.expansion.expansion-order-stability`, quoted there). Globs bringing
    /// nothing or the same definition are no other candidate (D57); a `Globs` cell unknown here
    /// (`Outside`) or not decided shows nothing. Idempotent: `ask` runs the loop's end again.
    fn validate_expanded(&mut self) {
        for (&key, &(name, def, g)) in self.expanded_uses.iter() {
            match self.cells[g as usize].value {
                Some(Value::Found(gd, _) | Value::Ambiguous(gd, _)) if gd != def => {
                    self.glob_vs_expanded.insert(key, (name, def, gd));
                }
                Some(Value::Found(..) | Value::Ambiguous(..) | Value::NotFound | Value::Outside) | None => {}
            }
        }
    }

    /// A stalled call's answer with a weakly undecided scope not holding its lookup
    /// ([`Hold::NamedImport`]): `calls`, the stalled group's calls, add nothing (a call of the
    /// group before it in the text is weak too); every other read is of a decided cell. What
    /// the lookup took is left in `self.taken`. The call's own wait list is put back.
    fn stalled_call(&mut self, i: ItemId, calls: &Guess<'a>) -> Step<Outcome> {
        self.taken.clear();
        let waits = core::mem::replace(&mut self.items[i as usize].waits, AVec::new_in(self.alloc));
        let step = self.try_item(i, Some(calls), Hold::NamedImport);
        self.items[i as usize].waits = waits;
        step
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
        // The group's calls, adding nothing: what a stalled call is asked under.
        let mut calls = Guess { cells: ABTreeMap::new_in(alloc), items: ABTreeMap::new_in(alloc) };
        for &n in members {
            if let Node::Item(i) = n
                && self.items[i as usize].call.is_some()
            {
                calls.items.insert(i, None);
            }
        }
        // What the picked calls took: kept with them (`taken` is one call's at a time).
        let mut kept: AVec<'a, (ItemId, Taken<N>)> = AVec::new_in(alloc);
        let queued = self.cell_queue.len();
        for &n in members {
            let Node::Item(i) = n else { continue };
            if self.items[i as usize].call.is_none() {
                // An import: R5 as in `fixpoint`, its guess when the guess resolves it.
                if let Some(&Some(o @ (Outcome::Named(_) | Outcome::Glob(_) | Outcome::Outside))) = guess.items.get(&i) {
                    picked.push((i, o));
                }
                continue;
            }
            // A call: the one rule of `first_segment` under `Hold::NamedImport`. A scope that is
            // only weakly undecided does not hold its lookup (core's `core_arch`: a
            // `#[derive(Clone)]` in a module whose globs wait on its own derives takes the
            // standard prelude's; `cfg_select!` there is unknown, never failed), and nothing
            // else is guessed: every later segment is read from a decided cell.
            if let Step::Done(o @ (Outcome::Called(_) | Outcome::Inert | Outcome::Outside)) = self.stalled_call(i, &calls) {
                picked.push((i, o));
                for k in 0..self.taken.len() {
                    kept.push((i, self.taken[k]));
                }
            }
        }
        self.taken.clear();
        if let Some(e) = self.weak_error.take() {
            return Err(e);
        }
        if !picked.is_empty() {
            self.stats.stalls += 1;
            for &(i, t) in kept.iter() {
                match t {
                    Taken::Watch(m, ns, name, def, kind) => self.watches.entry((m, ns, name)).or_insert_with(|| AVec::new_in(alloc)).push((i, def, kind)),
                    Taken::Globs(g, def, globbed) => self.speculated.push((i, g, def, globbed)),
                    Taken::Error(e) => self.r6_error(i, e),
                }
            }
            for &(i, o) in picked.iter() {
                self.settle(i, o);
            }
            return Ok(());
        }
        if self.cell_queue.len() != queued {
            // A stalled call's lookup made a cell no evaluation had made yet: it is tried
            // first, and the group is asked again (a cell is made once, so this ends).
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
                if let Step::Wait(w) = self.textual(at.frag, at.pos, name, Some(at.shape), None)
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
