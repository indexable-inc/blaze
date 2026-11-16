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

#[cfg(test)]
extern crate std;

mod builtin;
mod error;
mod facts;
mod finish;
mod group;
mod lookup;
mod read;
mod resolver;
mod scope;

#[cfg(test)]
mod tests;

pub use builtin::{BUILTIN_TYPES, builtin_type, is_builtin_type};
pub use error::ResolveError;
pub use facts::{Name, RecordFacts, ScopeFacts};
pub use group::Group;
pub use read::{Answer, ExpandRequest, NameAnswer, Read, Reply, Step};
pub use resolver::{MAX_EXPANSIONS, MAX_MODULE_DEPTH, RECURSION_LIMIT, Resolver};
pub use scope::{ReadEntry, ReadList, Resolved, Saw};

// The item layer's words and working forms, from their one home.
pub use blaze_surface::{
    AmbiguityKind, AnonImport, Binding, BindingState, CallShape, ChildModDecl, CrateRootBuilder,
    CrateRootHead, DeclaredItem, DeclsBuilder, Def, DefKind, Dep, Edition, ExpandedDecls,
    ExportEntry, ExternCrateDecl, ExternTarget, MacroCallDecl, MacroRulesDecl, ModuleInfo,
    Namespace, Origin, Path, PathKind, Rename, ScopeBuilder, ScopeEntry, ScopeError, Site,
    TextualDef, TextualMark, Unresolved, UseDecl, UseForm, UseSite, Visibility, VisibilityClass,
    WrittenVis,
};
