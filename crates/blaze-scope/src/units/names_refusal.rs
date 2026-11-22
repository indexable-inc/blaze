//! A refused crate's names are refused, never resolved as if it had no dependencies (rule 3).
//!
//! [`super::macro_side::add_macro_side`] keeps a member proc macro in the list with no
//! dependencies when the toolchain shelf has no `wasm32-wasip1` side, and records why in
//! [`UnitCrates::refused`]. Resolved like that, every path the proc macro writes into `syn`,
//! `quote` or `proc_macro2` is NotFound at segment 0 (tempo's `tempo_precompiles_macros`: 94
//! heads), an answer about a crate that was never made. [`UnitCrates::checked_inputs`] is the
//! one way a whole-list caller gets the inputs: it gives the refusal of the first refused
//! crate, by index, naming it and what fixes it (rule 19), and no inputs.
//!
//! A per-crate caller (the command line's `CrateScope` task) asks [`UnitCrates::refusal`]
//! for the crate it is about to resolve and fails that crate alone (rule 20).
//!
//! Sans-IO, like its parent: nothing here reads a file.

#![blaze::sans_io]

use blaze_own::AVec;

use super::{RefusedCrate, UnitCrates};
use crate::resolve::CrateInput;

impl<'a> UnitCrates<'a> {
    /// The inputs of one `resolve_crates` call, by index, when no crate of the list is
    /// refused; else the first refusal (the list is sorted by index, so the same list gives the
    /// same refusal on any machine). [`Self::inputs`] gives a refused crate's input too, with
    /// no dependencies: only a caller that refuses that crate itself, by [`Self::refusal`],
    /// takes it.
    pub fn checked_inputs(&self) -> Result<AVec<'a, CrateInput<'_>>, RefusedCrate<'a>> {
        match self.refused.first() {
            Some(refused) => Err(refused.clone()),
            None => Ok(self.inputs()),
        }
    }
}
