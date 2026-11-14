//! Code as records and programs as manifests (target decisions 1, 5, 6 and 8;
//! /Volumes/Projects/blaze-int-1112-notes/target-decisions-1113.md).
//!
//! - [`record::InstanceCode`]: one function instance's code, position-independent, every
//!   address a relocation by stable symbol identity, panic locations and unwind and line rows
//!   item-relative. Found by its [`key::InstanceKey`], which is computed before any lowering
//!   from the body's key, the type arguments' content keys, the settings and tier, and the keys
//!   of the bodies it takes in ([`inline`]: the two-step rule).
//! - [`walk::Walk`]: the monomorphization walk as a streaming sans-IO state machine: roots
//!   in, records asked for, one instance out at a time; it holds the frontier and the seen set,
//!   never a crate's bodies. Checking is demand-driven through it.
//! - [`program::Program`]: a program as a manifest (an ordered table of what is where, itself a
//!   record) and the machine that turns a demand, an edit's delta or a tier-up into a
//!   [`change::PlacementChange`] of bytes proportional to the edit. One driver applies it now:
//!   the file writer ([`change::apply`]). Nothing is relinked.
//! - [`reach`] and [`record::TestRecord`]: tests are programs; a result is keyed by what the
//!   test reaches (calls and statics' values), its outside reads and its arguments.
//!
//! Nothing here does IO, reads a clock, or reacts to a file change: a build happens when asked,
//! and every state machine is driven by its caller.
//!
//! Records are the core's (`blaze_keyed`): code is its `Instance`, a test result its `TestRun`;
//! [`keyed`] is the one adapter between this crate's key rules and layouts and the core's kinds,
//! and [`codec`] lays out the payloads inside them.

#![feature(allocator_api, btreemap_alloc)]
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]

pub mod change;
pub mod codec;
pub mod inline;
pub mod key;
pub mod keyed;
pub mod program;
pub mod reach;
pub mod record;
pub mod walk;

#[cfg(test)]
mod tests;

/// A bug planted on purpose, so a test shows its oracle catches it. `None` in every real use.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum Plant {
    #[default]
    None,
    /// An instance key leaves its inlinees out.
    InlineeLeftOut,
    /// The walk's seen set forgets every other node it finished.
    SeenDrops,
    /// A function or static that moved keeps its referrers pointing at its old place (the
    /// file-only layout's counterpart of a slot not repointed).
    SiteNotRepatched,
    /// A test's reach leaves statics out.
    ReachWithoutStatics,
}
