//! The worlds a sans-IO core runs against, as reference models
//! (docs/v2/plan/design/proofs-for-sans-io.md, section 2).
//!
//! A core asks for effects and is told how each ended. What may be told is the world's to say:
//! a clock never runs back, a sync makes durable what was written before it was asked, a power
//! cut keeps a prefix of what was not. Each world here is that rule as a small, pure Rust type:
//! its state, a function per answer that gives the state after it, and a predicate that says
//! which answers the kernel may give where it has a choice. [`asks`] lays over the disk what
//! has been asked of it and not answered yet, and which answers the kernel may give to each
//! (a sync covers at least what it owes, a failed write lands what it may), and what a crash
//! does to what is in flight.
//!
//! Three things read the one text. A proof: `blaze prove` translates a world's functions as it
//! does a core's, so a core's theorem quantifies over every answer the world allows and needs
//! no model of the kernel written a second time in Lean. The simulation:
//! [`Sim`](crate::sim::Sim) is held to each world by a refinement test (`sim/world.rs`), so a
//! simulation test and a proof are about the same kernel. And a real driver: what it was
//! observed to do is an answer the world allows (objective V4).
//!
//! The worlds are written in the subset `hir-ty-lean` translates today (structs, enums, `Vec`,
//! index loops; no closures, iterators or generics), so a proof against one waits for nothing.

pub mod asks;
pub mod clock;
pub mod disk;
