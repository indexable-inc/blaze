//! Name resolution of the item layer, one module at a time (docs/v2/plan/design/bottom.md,
//! "Name resolution without resolving a crate").
//!
//! A [`Resolver`] computes one module's scope (a [`ScopeBuilder`], blaze-surface's working form
//! of the `ModuleScope` record, which [`ScopeBuilder::write`] writes) from its own records, read through
//! [`ScopeFacts`] (its declarations, its place, its crate's root record, the expansions of its
//! macro calls), and from reads of other modules' scopes, which it never computes: it asks
//! for them and takes their answers. The result is the scope and the [`ReadList`] of exactly
//! the reads that determined it, negative reads included, so a record of the scope is valid
//! while each read still sees what it saw.
//!
//! # The protocol
//!
//! `Resolver::new_in(module, facts, alloc)`, then `step(facts, replies)` until `Done`:
//!
//! - [`Step::Ask`]`(reads)`: answer any of them at the next step, in any order and any
//!   batches; one left unanswered is asked again. A read of a module that is being computed
//!   and waits on this one is answered [`Answer::InProgress`].
//! - [`Step::Expand`]`(calls)`: expand each call with its macro and make
//!   [`ScopeFacts::expansion`] answer it, then step with no replies.
//! - [`Step::Cycle`]`(modules)`: the machine waits on modules that are in progress: put it
//!   and their machines in one [`Group`] (or join their group), which steps the same way and
//!   ends with one result per member.
//! - [`Step::Done`]`(resolved)`: the scope and its read list.
//!
//! A result is a function of the records alone, not of the order or batches the answers came
//! in. The rules followed are rustc's: determinacy of single and glob imports, explicit
//! bindings over globs, glob ambiguity an error only where used, visibility classes for globs,
//! `macro_rules!` textual order with `#[macro_use]` modules, the extern, `#[macro_use]` and
//! standard preludes as the outer layers, and the final pass's ambiguity kinds.
//!
//! [`RecordFacts`] reads the facts from the records an `Items` holds; the words the machine
//! speaks (namespaces, definitions, bindings, visibilities, paths) and the working forms are
//! blaze-surface's, re-exported here, and none is defined twice.
//!
//! The machine is sans-IO and allocates only in the allocator it is given. Every loop is
//! bounded ([`MAX_MODULE_DEPTH`], [`MAX_EXPANSIONS`], [`RECURSION_LIMIT`], and a pass bound
//! from the module's own work); a user-code error is a [`ScopeError`] in the scope, and the
//! machine's own failures are [`ResolveError`]s.

#![no_std]
#![feature(allocator_api, register_tool, custom_inner_attributes)]
// `#![blaze::sans_io]` in each sans-IO module; inert under rustc.
#![register_tool(blaze)]

// resolve.rs: the locks between the fill's threads (std::sync; see its module docs).
extern crate std;

mod again;
pub mod apart;
pub mod asked;
// `#[link_name = concat!(..)]`: an attribute value written as a macro call, expanded at its item.
pub mod attr_values;
pub mod blocks;
pub mod call_refusal;
pub mod expanded_blocks;
pub mod expansion_origins;
pub mod expansion_records;
pub mod nested_input;
pub mod crate_roots;
mod builtin;
// The cell core (bottom.md section 6): the names records' one writer.
pub mod cell_key;
mod cells;
pub mod crate_env;
pub mod crate_scope;
mod error;
pub mod expansions;
mod facts;
pub mod failed_call;
pub mod main_call;
pub mod macro_effect;
pub mod surface;
// D189 step 4: a body's block scopes solved as the body's own.
pub mod body_scope;
mod finish;
pub mod graph;
mod group;
pub mod include_place;
pub mod left_reads;
// The library features a toolchain crate declares for one cfg (the build-script probe's E0635).
pub mod lib_features;
pub mod included;
pub mod items_record;
// D116: names at module grain (tables of links, `Imports`, their part reads and the hop walk).
pub mod links;
mod lookup;
pub mod names;
pub mod names_cells;
pub mod names_record;
pub mod package_path;
pub mod prepares;
mod read;
pub mod refused;
pub mod resolve;
mod resolver;
mod scope;
pub mod scope_record;
pub mod task;
pub mod tests_record;
pub mod tree_record;
pub mod units;
// A path that failed behind a failed dependency: unknown, not absent (turso c1647).
pub mod waits_on;
mod work;

#[cfg(test)]
mod tests;

pub use again::{Again, Touched};
pub use builtin::{BUILTIN_TYPES, Builtin, PREFIX as BUILTIN_PREFIX, builtin_of, builtin_type, builtin_type_in, is_builtin_type};
pub use error::{GlobBug, ResolveError};
pub use facts::{Name, RecordFacts, ScopeFacts};

pub use cells::{
    DecideCheck, DecideError, Difference as DecideDifference, Layer as CellLayer, Value as CellValue, GroupRecords, GroupRef, MemberRefusal as GroupMemberRefusal, Members as GroupMembers, OneGroup as OneGroupSolver, OneGroupRefusal, Solved as GroupsSolved, Edit as GroupsEdit, EditError as GroupsEditError, Edited as GroupsEdited, GROUP_ABSENT, GroupOwnError, GroupRead, GroupRecordError, GroupSeen, GroupShape, GroupSolver,
    GroupsError, ItemResult as CellResult, NameGroup, NodeAt as GroupNodeAt, Outcome as CellOutcome, globs_cell as group_globs_cell, most_entries as group_most_entries,
    own_digest as group_own_digest, re_solve as re_solve_groups, DemandError as GroupDemandError, Demanded as GroupsDemanded, DemandedOpts as GroupsDemandedOpts, MOST_DEMAND_DEPTH, MOST_DEMAND_SOLVES, NoRecordOf as GroupNoRecordOf, demand as demand_groups, names_on_demand, OnDemand as NamesOnDemand, OnDemandError as NamesOnDemandError, OnDemandRefused as NamesOnDemandRefused, LazyDep, Want as NamesWant, MOST_WANT_ROUNDS, textual_cell as group_textual_cell, write_groups,
};

/// The cell core (bottom.md section 6) on one crate's module tree under `root`, for measuring
/// it on real trees: `expand(call, def)` makes the call's expansion, which `facts.expansion`
/// then hands back (the same hook and facts the old core's fill uses); `visit` is lent every
/// import's and call's answer at the end.
pub fn cells_count<'a, F: ScopeFacts>(
    facts: &F,
    root: blaze_id::ItemKey,
    alloc: blaze_own::Alloc<'a>,
    now: &dyn Fn() -> u64,
    expand: &dyn Fn(blaze_id::ItemKey, Def),
    visit: &mut dyn FnMut(CellResult<'_, F::Name>),
) -> Result<cells::CellRun, cells::CellError> {
    let t0 = now();
    let mut c = cells::Cells::load(facts, root, alloc)?;
    let t1 = now();
    c.run(facts, expand, &[])?;
    let t2 = now();
    for r in c.results() {
        visit(r);
    }
    Ok(cells::CellRun { stats: c.stats, load_ns: t1 - t0, run_ns: t2 - t1, probe_centi_ns: c.probe_cost(now) })
}
pub use group::Group;
pub use read::{Answer, ExpandRequest, NameAnswer, Read, Reply, Step};
pub use resolver::{FillStats, MAX_EXPANSIONS, MAX_MODULE_DEPTH, RECURSION_LIMIT, Resolver};
pub use scope::{ReadEntry, ReadList, Resolved, Saw};

// The item layer's words and working forms, from their one home.
pub use blaze_surface::{
    AmbiguityKind, AnonImport, Binding, BindingState, CallShape, ChildModDecl, CrateRootBuilder,
    CrateRootHead, DeclaredItem, DeclsBuilder, Def, DefKind, Dep, Edition, ExpandedDecls,
    ExternCrateDecl, ExternTarget, GlobEdge, MacroCallDecl, MacroRulesDecl, MacroShapes, ModuleInfo,
    Namespace, Origin, Path, PathKind, Rename, ScopeBuilder, ScopeEntry, ScopeError, Site,
    TextualDef, TextualMark, Unresolved, UseDecl, UseForm, UseSite, Visibility, VisibilityClass,
    WrittenVis, globs_digest, Asker, Node, Passed, SeenFrom, Sources, Through, through,
};
