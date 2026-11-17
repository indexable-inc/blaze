//! What the two code record keys are made of (D97 point 6, D100 points 8, 12 and 14). The keys'
//! digests are computed by the pieces that write the records; this is the one list of inputs.
//! A record's READ LIST is not in its key: a record is valid by its key or by its read list
//! against today's facts.

use blaze_run::CodeId;

use crate::{Machine, TierName};

/// The inputs of a Fast (or Reference) record's key.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FastKeyIn {
    /// The instance's share identity (polymorph.md; D100 point 12).
    pub share: [u8; 32],
    /// The body's code identity.
    pub body: CodeId,
    /// The digest of the facts of the types the body was built with (layouts).
    pub type_facts: [u8; 32],
    pub machine: Machine,
    /// The target CPU by name, never `native` (D100 point 14).
    pub cpu: &'static str,
    pub tier: TierName,
    /// The producer's code identity (D87).
    pub producer: [u8; 32],
    /// The template table's digest (`TableHead::digest`); zero for the Reference tier.
    pub table: [u8; 32],
}

/// The inputs of an Optimized record's key: a fast key's, and the pipeline, LLVM and the offer
/// set.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct OptimizedKeyIn {
    pub fast: FastKeyIn,
    /// The named custom pipeline (D100 point 7 (a)).
    pub pipeline: &'static str,
    /// LLVM's version (major, minor, patch).
    pub llvm: (u32, u32, u32),
    /// The offer set's digest in its total order (D100 point 8).
    pub offer: [u8; 32],
    /// Whether the offer set came from an observed profile: such a record stays in the private
    /// layer and is never shared (D100 point 8).
    pub observed: bool,
}
