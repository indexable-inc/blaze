//! The seam: what drop elaboration asks of the move check and of the type table. The pass
//! depends on neither (it names no blaze-flow or blaze-build type); a driver implements
//! [`DropFacts`] over the check's move dataflow (blaze-flow moves.rs `inits`) and the type
//! table's drop answer.
//!
//! How blaze-flow's answer maps onto it, at a `Drop P` row `s` (its `init`/`uninit` bits are the
//! maybe-initialized and maybe-uninitialized dataflows at `s`):
//!
//! - `s` not reached, or `P`'s init bit clear: [`DropState::Uninit`] (moves.rs `Nothing`).
//! - a strict sub-place of `P` maybe uninitialized: [`DropState::Partial`] (moves.rs `Open`, and
//!   ALSO the `Flag` case when a part is moved too: `drop_kind` tests `P`'s own bits first, so
//!   "moved whole on one path, a field moved on another" is `Flag` there, which would drop the
//!   moved field on the second path).
//! - both of `P`'s bits set: [`DropState::Maybe`] (moves.rs `Flag`).
//! - otherwise: [`DropState::Init`] (moves.rs `Static`).
//!
//! The parts of a `Partial` answer are the fields of `P`'s type (the type table's, which moves.rs
//! does not read): a field with a part moved below it is opened into its own fields; any other
//! field takes its state from its own place row's bits, or, when the field is no row of the
//! flow (nothing named it), from its nearest ancestor row's; a field not initialized on any path
//! is left out.
#![blaze::sans_io]

use blaze_body::Stmt;
use blaze_body::body::PTy;

/// One field step of a part's path: `.index`, whose type is `ty`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Field {
    pub index: u32,
    pub ty: PTy,
}

/// Whether a part left to drop is initialized on every path or on some.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum PartState {
    Init,
    Maybe,
}

/// A part of a partly moved place that is still to be dropped: the field path from the dropped
/// place (never empty), and its state.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Part<'f> {
    pub path: &'f [Field],
    pub state: PartState,
}

/// What the move check says of the place a `Drop` row names, at that row.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum DropState<'f> {
    /// Initialized on every path: the drop stays.
    Init,
    /// Moved out of or never initialized on every path: the drop goes.
    Uninit,
    /// Initialized on some paths: the drop is guarded by the place's drop flag.
    Maybe,
    /// Some parts moved out: the parts listed are dropped one by one, in field order (the pass
    /// orders them; the answer may list them in any order). Parts initialized on no path are
    /// not listed.
    Partial(&'f [Part<'f>]),
}

/// Whether a type's values need drop.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum NeedsDrop {
    Yes,
    No,
    /// It depends on the instance's type arguments: the drop row stays, and the builder decides
    /// after substitution.
    Generic,
}

/// The move check's and the type table's answers the pass reads.
pub trait DropFacts {
    /// The state of the place `drop` (a `Drop` row of the input body) names, at that row.
    fn at(&self, drop: Stmt) -> DropState<'_>;
    /// Whether values of `ty` (a type of the checked record) need drop.
    fn needs_drop(&self, ty: PTy) -> NeedsDrop;
    /// The checked record's `bool`: the type of every drop flag.
    fn bool_ty(&self) -> PTy;
}
