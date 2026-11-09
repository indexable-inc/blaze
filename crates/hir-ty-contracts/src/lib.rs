//! The optimizer's use of proved contracts (docs/v2/plan/design/contracts.md, part 3): a fact a
//! native `ensures` states is used only once a proof stands behind it, and each use is a
//! [`Fact`] the caller's code identity covers (rule 21).
//!
//! What it uses today, and nothing else ([`use_proved_contracts`]): where a body does
//! `r = f(..)`, `f` a direct call of a non-generic function whose `ensures` lane verus proves
//! ([`hir_ty_verus::proved`], `Proved` only, never `Unknown`), and that `ensures` bounds the
//! return by a literal (`r < K`, `r <= K`), the bounds check of an index into an array of
//! constant length `len >= K` (`K > len` for `<=`) whose index is `r`, or a copy or unsigned
//! integer cast of it, is removed.
//!
//! **Soundness.** A proved `ensures` holds when `f` returns, given that its `requires` held at
//! entry. So the fact is used only when `f` has no `requires`, or `f`'s crate checks contracts
//! at run time (the atom `cfg::CONTRACT_CHECKS` in its cfg: a broken `requires` stops the program
//! before `f` returns). Unsigned values only: an unsigned cast never makes a value
//! larger, so `r < K` gives `r as usize < K`. The index must be the value `f` returned: every
//! local from the call's destination to the index is assigned once in the whole body, never
//! borrowed, never written in part, never an argument or a capture, so each read of it reads
//! the one assignment, which a run reaches only by `f` returning. The condition the check
//! tests must be `index < len`, computed in its own block from those very operands.
//!
//! **Not built.** A `requires` discharged at a call site (the callee's unchecked entry) needs
//! [`hir_ty_verus::Clause::Requires`], which answers `None` until lane verus lowers call sites
//! (its order step 1). Generic callees wait for lane verus's step 5. Facts proved by blaze's own
//! passes (range facts at every `Return`) are the next kind of [`How`].

#![forbid(unsafe_code)]

mod clause;
mod pass;
#[cfg(test)]
mod tests;

use std::fmt;

use blaze_ids::{ContentId, Hasher};
use blaze_items::ItemKey;
use hir_ty_verus::ProofKey;

pub use pass::use_proved_contracts;

/// The domain of a [`Fact`]'s content id.
pub const FACT_DOMAIN: &str = "blaze v2 contract fact 1";
/// The domain of the facts one body used, in the order it used them ([`facts_id`]).
pub const FACTS_DOMAIN: &str = "blaze v2 contract facts 1";
/// The domain of a [`ClauseHash`].
pub const CLAUSE_DOMAIN: &str = "blaze v2 contract clause 1";

/// The clause item a fact was read from: its keyword, which clause group of the function it is
/// in and which of the group's expressions, and the expression printed from HIR (no positions,
/// so an edit elsewhere in the file keeps it).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ClauseHash(pub ContentId);

impl ClauseHash {
    pub fn of(keyword: &str, group: u32, item: u32, text: &str) -> ClauseHash {
        let mut h = Hasher::new(CLAUSE_DOMAIN);
        h.part(keyword.as_bytes())
            .update(&group.to_le_bytes())
            .update(&item.to_le_bytes())
            .part(text.as_bytes());
        ClauseHash(h.finish())
    }
}

/// What a fact's proof is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum How {
    /// Lane verus proved the callee: the proof's key, which covers the vendored Verus, the Z3
    /// version, the resource limit, the seed and the callee's VIR (its clauses, body and types).
    Verus(ProofKey),
}

impl fmt::Display for How {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            How::Verus(key) => write!(f, "verus {:x}", key.0),
        }
    }
}

/// Why the callee's `requires` held when it returned: what makes its proved `ensures` true.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Entry {
    /// The callee has no `requires`.
    NoRequires,
    /// The callee's crate checks contracts at run time: a broken `requires` stops the program
    /// before the callee returns.
    RequiresChecked,
}

impl fmt::Display for Entry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Entry::NoRequires => "no_requires",
            Entry::RequiresChecked => "requires_checked",
        })
    }
}

/// What the optimizer did with a fact.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Use {
    /// An index's bounds check is gone.
    BoundsCheckRemoved,
}

impl fmt::Display for Use {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Use::BoundsCheckRemoved => "bounds_check_removed",
        })
    }
}

/// One use of a proved contract: the code the optimizer made depends on it, so the caller's
/// code identity covers it (rule 21). A change to the callee's clause, its body (through the
/// proof's key), whether it has a `requires` or whether its crate checks contracts changes the
/// fact, and with it every caller that used it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Fact {
    /// The callee, by item key. It is non-generic: a generic callee's fact will also need its
    /// arguments' content identity.
    pub callee: ItemKey,
    /// The clause item the fact was read from.
    pub clause: ClauseHash,
    /// Its proof.
    pub how: How,
    /// Why the callee's `requires` held.
    pub entry: Entry,
    /// What was done with it.
    pub used: Use,
}

impl Fact {
    /// The fact's content id: every field, each tagged.
    pub fn id(&self) -> ContentId {
        let mut h = Hasher::new(FACT_DOMAIN);
        h.id(&self.callee.0).id(&self.clause.0);
        match self.how {
            How::Verus(key) => h.update(&[0]).id(&key.0),
        };
        h.update(&[match self.entry {
            Entry::NoRequires => 0,
            Entry::RequiresChecked => 1,
        }]);
        h.update(&[match self.used {
            Use::BoundsCheckRemoved => 0,
        }]);
        h.finish()
    }
}

/// The facts one body used, in the order [`use_proved_contracts`] gives them (the body's block
/// order, which the source fixes): what the body's code identity folds in.
pub fn facts_id(facts: &[Fact]) -> ContentId {
    let mut h = Hasher::new(FACTS_DOMAIN);
    h.update(&(facts.len() as u64).to_le_bytes());
    for fact in facts {
        h.id(&fact.id());
    }
    h.finish()
}
