//! The bugs a control can plant in the arena (D165, rule 78: a strategy is a type parameter at
//! one seam). A [`PlantedBdd`](crate::PlantedBdd) holds its plant as its type parameter
//! `P: Planted<CondPlant>`; [`Bdd`](crate::Bdd), the arena that ships, is `PlantedBdd<'_,
//! NoPlant>`, whose every `has` is `false`, so each check compiles away. The truth-table oracle's
//! controls (tests/mod.rs) make an arena with a value of this enum.

#![blaze::sans_io]

use blaze_own::planted::Plants;

/// A bug a control plants in the arena.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CondPlant {
    /// Nothing planted: the code as it is.
    None,
    /// The unique table interns a node whose high edge is complemented as it is, instead of
    /// complementing both edges and the result: a function and its negation stop sharing nodes,
    /// and one function can get two ids.
    UniqueSkipsComplement,
    /// The `ite` cache is keyed by `f` and `g` only: an entry for `ite(f, g, h)` answers
    /// `ite(f, g, h2)`.
    IteCacheSkipsElse,
    /// `restrict(f, x, v)` takes the cofactor of `!v` at a node of `x`.
    RestrictWrongBranch,
}

impl Plants for CondPlant {}
