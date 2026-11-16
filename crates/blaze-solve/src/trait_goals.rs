//! Trait goals (trait_goals.rs of the reference): assembly from the param env and from impls
//! (through the fingerprint prefilter), the builtin and alias-bound hooks, and
//! `merge_trait_candidates`. (Being written: this stub fixes the names other modules use.)

use crate::term::{Def, List, Polarity, Ty};

/// A trait goal with its self type resolved and, where it was an alias, normalized
/// (`structurally_normalize_ty` in `assemble_and_evaluate_candidates`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TraitGoal {
    pub trait_: Def,
    /// The trait's arguments, the self type first (`args[0]` is `self_ty` as it was given).
    pub args: List,
    pub polarity: Polarity,
    /// `args[0]`, resolved and normalized.
    pub self_ty: Ty,
}

/// The rules of this module's candidates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Rule {
    /// The param env's clause at this index (`probe_and_match_goal_against_assumption`).
    Env(u32),
    /// The impl in `Cand::d` (`consider_impl_candidate`).
    Impl,
}
