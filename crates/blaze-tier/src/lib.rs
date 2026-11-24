//! Piece 0 of code generation on the item layer (docs/v2/plan/design/backend-next.md, "End state:
//! code generation on the item layer", D97, amended by D98, D99 and D100): the types every other
//! piece is written against, and nothing else (rule 79).
//!
//! REUSED, not copied (rule 30): blaze-code's leaf types (`record::{Symbol, Space, Target, Word,
//! Pad, SiteKind}`) and unwind rows (`unwind::{Frame, Row}`); blaze-run's keys (`InstanceKey`,
//! `CodeId`, `SlotId`) and read-list row (`parent::Read`); blaze-lir's `View` and `Op`. The offer
//! set's types are blaze-run's (offer.rs, piece O), not here.
//!
//! MOVED here: [`Names`], [`Outside`] and [`Named`] from crates/blaze-emit/src/record.rs (every
//! tier names outside references the same way; blaze-emit re-exports them), and
//! [`RoutinePlaces`] from crates/blaze-emit/src/routines.rs (the seam's routine table).
//!
//! NEW: the [`Tier`] seam ([`seam`]); the one new code record in memory ([`record`]: relocations,
//! loop map, polls, unwind rows, position map, read list: D100 point 11); the template table's
//! form and the trait it is read through ([`table`]); a target's register description
//! ([`regs`]); the sample ring's entry and the profile and startup records ([`sample`]); the two
//! record keys' inputs ([`key`]).

#![no_std]

pub mod features;
pub mod key;
pub mod names;
pub mod record;
pub mod regs;
pub mod route;
pub mod routines;
pub mod sample;
pub mod seam;
pub mod table;

pub use features::{Feature, FeatureError, Features};
pub use names::{Named, Names, Outside};
pub use route::{first_tier, optimizing_tier};
pub use routines::RoutinePlaces;
pub use record::{Emitted, InlineFrame, Live, LoopHeader, OUTERMOST, Place, Poll, Position, Reloc, RelocKind};
pub use regs::{ARM64, Arch, CallClobbers, Machine, Os, Reg, RegClass, RegDesc, RegSet};
pub use regs::x86_64::X86_64_SYSV;
pub use seam::{Tier, TierError, TierName};
pub use table::{Field, FieldKind, Form, ImmClass, Inner, Pattern, Source, Table, TableError, TableHead, Template};

#[cfg(test)]
mod tests;
