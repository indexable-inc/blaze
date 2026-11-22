//! The soft-float routines a tier's code may call (port27/tempo-run/INTRINSICS.md 3.5, P5b), as
//! part of the seam's input: where each one is among the caller's function references. Moved
//! whole from crates/blaze-emit/src/routines.rs, which re-exports it (rule 30: one home), so
//! that every tier is handed the routines the same way, through [`crate::Names::routines`]
//! (D197 root 6, piece (a)).
//!
//! WHICH routines a function calls is an output of emitting it: only blaze-lir's legalize maps
//! an `f128` (or `f16`) operation to its compiler_builtins routine, and a tier runs legalize. So
//! the caller hands ONE table covering every routine its program's compiler_builtins exports
//! (blaze-ladder front2/builtins_shim.rs); a tier adds a reference row for each bound routine to
//! its copy of the function and legalizes; a routine the function calls is then a function
//! target of the record like any callee. A caller with no table answers
//! [`RoutinePlaces::NONE`]: an `f128` operation is then refused by name.

use blaze_lir::legalize::SoftFloat;

/// How many routines there are: [`SoftFloat::ALL`]'s length.
const COUNT: usize = SoftFloat::ALL.len();

/// No place: the routine is not bound.
const UNBOUND: u32 = u32::MAX;

/// Where each [`SoftFloat`] routine is among the caller's function references (the place an
/// `Outside::Func(place)` names through the caller's [`crate::Names`]); an unbound routine has
/// none. Indexed by `SoftFloat as usize` (the order of [`SoftFloat::ALL`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RoutinePlaces {
    places: [u32; COUNT],
}

impl RoutinePlaces {
    /// No routine bound: every `f128` operation is refused by name.
    pub const NONE: RoutinePlaces = RoutinePlaces { places: [UNBOUND; COUNT] };

    /// Routine `r` is function reference `place` of the caller's names. `place` is not
    /// `u32::MAX` (no names table has that many references).
    pub fn bind(&mut self, r: SoftFloat, place: u32) {
        self.places[r as usize] = place;
    }

    /// Where routine `r` is, if bound.
    pub fn place(&self, r: SoftFloat) -> Option<u32> {
        match self.places[r as usize] {
            UNBOUND => None,
            place => Some(place),
        }
    }
}
