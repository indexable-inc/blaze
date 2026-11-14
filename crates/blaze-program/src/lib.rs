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
//! **The row rule** (one definition; every path derives from it). Each function has a tier: a
//! dependency's is the top tier; a workspace function's is the fast tier unless it was tiered
//! up and is unchanged since. The *rows* of a program are the least set of nodes that holds the
//! roots and is closed under the edges of the code at those tiers: a function's calls that its
//! code does not take in (an inline edge is not an edge), its statics, and a static's fields
//! (what takes a function's address). A node has a row exactly when it is in that set, never
//! because it had one before. A build's makes are the set's keys the store does not hold.
//!
//! - A whole build ([`walk::Fresh`]) walks the set from the roots.
//! - A delta keeps every row whose node it did not touch and that is still in the set. It walks
//!   the *stale* nodes (edited, revisited, tiered up, changed statics, roots not yet placed)
//!   only when they are in the set. A stale node is walked for its own sake
//!   ([`walk::Seed::Walk`]) when it is a root, or when an unstale root reaches it through
//!   unstale rows only ([`program::Program::seeds`]): then it is in the set whatever the walk
//!   does. Every other stale node ([`walk::Seed::IfReached`]) is walked when the walk reaches it:
//!   through a walked node's code, or through the kept calls of a row the walk passed over as
//!   held ([`walk::Seed::Through`]). A stale node the walk never reaches is not in the set; its
//!   row is retired ([`walk::Ended::unreached`] makes [`program::Program::place`] run the mark).
//! - [`program::Program::tier_ups`] picks which rows to tier up (a policy); a tier-up is a stale
//!   node like any other. A node new to the set in a delta that only tiers up is made at the
//!   top tier ([`program::Delta::new_at`]), so a tier-up leaves no row at the fast tier.
//!   [`program::Program::top_tier`] is the rows held at the top tier: a whole build seeded
//!   with it as `Delta::tier_up` (new nodes at the fast tier) has the same tiers, so the same
//!   rows.
//! - A tier changes no behaviour, so a test's key is over the tier-free plan keys of what it
//!   can run ([`reach`]); a tier-up never runs a test again.
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
