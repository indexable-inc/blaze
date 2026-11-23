//! One body's record (flow.md section 6): its findings, and the proof the code builder takes.
#![blaze::sans_io]

use blaze_own::Vec;
use core::alloc::Allocator;

use crate::flow::{Bound, Flow};
use crate::loans::{BorrowFinding, LoanParts, TierOne, facts, loan_parts, tier_one, tier_two};
use crate::moves::{MoveFinding, check_moves_planted};

/// A planted bug for the simulations' controls; `None` everywhere but in a control.
#[doc(hidden)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Plant {
    None,
    /// The move dataflows join by intersection where they must unite.
    MovesIntersect,
    /// Tier two's second contains rule runs against the subset: loans flow from `O2` to `O1`.
    TierTwoReversed,
    /// Tier one does not close contained loans over the subset graph.
    TierOneUnclosed,
    /// The signature's relations are ignored: no placeholder is known to outlive another.
    KnownIgnored,
    /// A relation the signature does not state is taken as known: every pair of placeholders.
    AllKnown,
    /// A statement's operands are all checked against its entry state, not each after the
    /// moves of the ones before it (moves.rs `check_moves_planted`): `(p, p)` is clean.
    OperandsAtEntry,
}

/// The proof that a body passed the move and borrow checks: only [`check`] makes one, and the
/// code builder takes it (rule 74).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Checked {
    /// Which tier accepted: 1 or 2.
    tier: u8,
}

impl Checked {
    pub fn tier(self) -> u8 {
        self.tier
    }
}

/// One body's record: findings sorted by point, and the proof when there are none.
pub struct Record<A: Allocator> {
    pub moves: Vec<MoveFinding, A>,
    pub borrows: Vec<BorrowFinding, A>,
    /// The borrow, the conflict and the later use of each `Invalidated` finding in `borrows`,
    /// in its order (loans.rs [`loan_parts`]).
    pub loan_parts: Vec<LoanParts, A>,
    pub proof: Option<Checked>,
}

/// The move check, then tier one, then tier two only if tier one cannot accept.
pub fn check<A: Allocator + Copy>(flow: &Flow<A>, alloc: A) -> Result<Record<A>, Bound> {
    check_planted(flow, Plant::None, alloc)
}

/// Carry the caller's function identity in a refusal. This can be the complete
/// borrowed spelling, or the item key a driver resolves when printing its diagnostic.
pub fn check_named<N, A: Allocator + Copy>(
    function: N, flow: &Flow<A>, alloc: A,
) -> Result<Record<A>, Bound<N>> {
    check(flow, alloc).map_err(|bound| bound.in_function(function))
}

#[doc(hidden)]
pub fn check_planted<A: Allocator + Copy>(
    flow: &Flow<A>,
    plant: Plant,
    alloc: A,
) -> Result<Record<A>, Bound> {
    let moves = check_moves_planted(flow, plant, alloc)?;
    let facts = facts(flow, alloc)?;
    let (borrows, tier) = match tier_one(flow, &facts, plant, alloc)? {
        TierOne::Accepted => (Vec::new_in(alloc), 1),
        TierOne::Refused => (tier_two(flow, &facts, plant, alloc)?, 2),
    };
    let loan_parts = loan_parts(flow, &facts, &borrows, alloc);
    let proof = (moves.is_empty() && borrows.is_empty()).then_some(Checked { tier });
    Ok(Record { moves, borrows, loan_parts, proof })
}
