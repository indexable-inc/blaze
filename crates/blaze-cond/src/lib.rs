//! Presence conditions (docs/v2/plan/design/conditions.md; bottom.md 11, "One check covers every
//! configuration"): the boolean function of a request's cfg variables under which an entry of a
//! record exists, as a reduced ordered binary decision diagram, hash-consed, so equal functions
//! are equal ids.
//!
//! Why not a fork per value (crates/blaze-drive/src/condition.rs): a module's scope record
//! depends on every switch that gates one of its items, 21 in tokio 1.43's `lib.rs` and 48 in
//! `std/src/lib.rs`, so a record per assignment is 2^21 records or a refusal. Here a record is
//! never forked: each entry carries a [`Cond`], the record stays linear in its entries, and a
//! task forks only on distinct outcomes (TypeChef, Kästner et al., OOPSLA 2011; SuperC,
//! Gazzillo and Grimm, PLDI 2012, whose presence conditions are BDDs too).
//!
//! | piece | what |
//! |---|---|
//! | [`Space`], [`SpaceBuilder`] | the request's variables in one stable order (by name), each with a [`VarKey`] from its name; the configurations it allows ([`Space::valid_in`]): one of the targets, and the features closed under Cargo's implications |
//! | [`Bdd`], [`Cond`] | the arena (rows of `var`, `low`, `high`; a unique table; a direct-mapped `ite` cache; a node cap) and an id into it, complement in the low bit |
//! | [`Bdd::ite`] and the rest | `and`, `or`, `not`, `implies`, `restrict`, `exists`, `support`, `sat_count`, `any_sat`, with explicit stacks, no recursion |
//! | [`isop`] | an irredundant sum of products (Minato-Morreale), printed in Rust's cfg syntax |
//! | [`portable`] | a condition as rows of (variable key, low, high) in one canonical order; import refuses malformed rows by name |
//! | [`cfg`] | Rust's cfg predicate syntax, parsed into a postfix [`cfg::Expr`] and lowered to a [`Cond`] |
//!
//! # Two choices that differ from the brief, and why
//!
//! **Targets are not one many-valued variable.** A target's atoms (`unix`, `target_os =
//! "linux"`, `target_pointer_width = "64"`, ...) are booleans of their own, and the space's
//! `valid` is the disjunction, over the request's targets, of each target's full cube over them.
//! A binary code for "which of N targets" (ceil(log2 N) bits) is smaller, but its codes are the
//! request's numbering of its targets: a stored condition over them means another thing in a
//! request with one more target (rule 25), and a record whose item is `#[cfg(unix)]` would hold
//! "targets 0 or 2" where it read `unix`. Over atoms, a stored condition says what the code says,
//! its [`support`](Bdd::support) is its read list in Rust's own terms, and it stays valid when a
//! target is added. The cost is more variables (one per distinct atom of the targets, about 40
//! for a dozen targets) and a `valid` of at most targets x atoms nodes, built once per arena.
//!
//! **The printed form is in cfg syntax and may use `valid` as don't-care.** [`isop::Cover`]
//! takes a lower and an upper bound: `isop(f, f)` is exact, `isop(f & valid, f | !valid)` is the
//! shortest true statement about the configurations that exist, which is what a finding says.
//!
//! # The floor (rule 67), and the worst case
//!
//! One `ite` step that misses the cache is up to three node reads (one row each: a node is a
//! 12-byte row, not three columns, since every read wants all three fields), one cache probe and
//! one unique-table probe (a slot holds the node's hash beside its number, so a probe reads the
//! node only when the hashes agree). Inside L1 that is about 3 ns. Once the arena outgrows the
//! last-level cache the cache probe and the unique-table probe are each a DRAM miss at random
//! addresses that do not depend on each other, so the floor is about one DRAM latency a step,
//! 80 to 100 ns on Apple silicon. Measured (`examples/cost.rs`, 32 variables, the arena at 6.2
//! M nodes and 228 MB): 93 ns a step, so 0.9 to 1.2 times that floor; as three columns with a
//! bare-number unique table it was 108. Real cfg conditions stay in L2 (the roots of tokio and
//! rustix: 7k and 38k nodes, `valid` included), where lowering is dominated by finding the
//! atom's variable by name, not by steps. Memory: 12 bytes a node, 16 in the unique table at
//! its worst load (half full), 8 more in the traversal columns once a traversal ran; the cache
//! is fixed at 16 bytes x 2^`cache_bits`.
//!
//! The variable order is fixed by name and never reordered (determinism over size): two builders
//! of one key make the same nodes in the same order. A function's diagram can be exponential in
//! its variables under a bad order (the hidden weighted bit function is under every order);
//! cfg predicates are short formulas over few atoms and stay linear in their size. Past the
//! arena's node cap an operation refuses with [`CondError::NodeCap`] (rule 18).
//!
//! # SIMD
//!
//! None: every operation is a walk over nodes whose next address is the last load's value, so
//! it is latency-bound, and no step has independent lanes to fill. The truth tables of the
//! tests are words, and stay scalar.

#![no_std]
#![feature(allocator_ext)]
// blaze's lints (`blaze::sans_io` on the modules) name their tool; inert under rustc.
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![deny(blaze::std_use)]
#![deny(blaze::implicit_allocator)]
#![deny(blaze::process_static)]
#![forbid(unsafe_code)]

// Tests compare against truth tables on std's vectors.
#[cfg(test)]
extern crate std;

mod bdd;
pub mod cfg;
mod error;
pub mod isop;
mod plant;
pub mod portable;
mod space;

#[cfg(test)]
mod tests;

pub use bdd::{Bdd, Cond, Limits, MOST_NODES, MOST_VARS, PlantedBdd, Stats, Var, Witness};
pub use plant::CondPlant;
pub use error::{CondError, ImportError, ParseError, SpaceError};
pub use space::{Atom, Space, SpaceBuilder, VarKey, VarKind};
