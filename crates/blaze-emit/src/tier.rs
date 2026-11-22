//! The Reference tier behind piece 0's seam (WAVE-BACKEND.md piece A; backend-next.md D97 point
//! 1): today's cell emitter ([`emit_record`]) with its `bzcode01` record carried into the one new
//! form by `blaze_tier::record::from_v1` (D100 point 11), so every tier hands the harness, the
//! placer and the run path the same [`Emitted`]. It is the oracle tier: the tier equality harness
//! (tier_tests.rs) runs it and every other tier against LIR's evaluator.

use blaze_lir::Verified;
use blaze_own::Alloc;
use blaze_tier::{Arch, Emitted, Machine, Names, Tier, TierError, TierName};

use crate::{EmitError, Isa, Os, RecordEmitError, emit_record};

/// The Reference tier: every value in its stack cell, arm64 only.
pub struct Reference;

impl Tier for Reference {
    const NAME: TierName = TierName::Reference;

    fn emit<'a>(&self, func: Verified<'_>, names: &dyn Names, machine: Machine, alloc: Alloc<'a>) -> Result<Emitted<'a>, TierError> {
        if machine.arch != Arch::Arm64 {
            return Err(TierError::Machine { tier: Self::NAME, machine });
        }
        let os = match machine.os {
            blaze_tier::Os::MacOs => Os::MacOs,
            blaze_tier::Os::Linux => Os::Linux,
        };
        // Feature bits other than LSE permit code this tier never emits: base code runs on any
        // machine that has them.
        let isa = Isa { lse: machine.features & Machine::LSE != 0, os };
        // The routine table is the seam's input (`Names::routines`, D197 root 6 piece (a)): an
        // `f128` operation calls its routine where the caller binds it and is refused naming it
        // where not. Which routines the function calls is in the record's function targets, so
        // the seam's form needs no `Called` of its own.
        // Its source-position slots, part alignments and trap rows go into the form's own
        // columns (`bzcode08`), whole.
        let (record, _called) = emit_record(func, names, &names.routines(), isa, alloc).map_err(refused)?;
        blaze_tier::record::from_v1(record, alloc).map_err(|error| TierError::Record { tier: Self::NAME, error })
    }
}

/// The cell emitter's refusal as the seam's: the instruction where the emitter names one, and its
/// static reason. A name the caller lacks stays [`TierError::NoName`].
fn refused(error: RecordEmitError) -> TierError {
    const TIER: TierName = TierName::Reference;
    match error {
        RecordEmitError::Emit(EmitError::Unsupported { inst, op: _, why }) => {
            TierError::Refused { tier: TIER, inst: Some(inst), what: why }
        }
        RecordEmitError::Emit(EmitError::NoVia { place: _ }) => {
            TierError::Refused { tier: TIER, inst: None, what: "a function reference with no call kind" }
        }
        RecordEmitError::Emit(EmitError::TooLarge { what }) => TierError::TooLarge { tier: TIER, what },
        RecordEmitError::Emit(EmitError::Signature { why }) => TierError::Refused { tier: TIER, inst: None, what: why },
        RecordEmitError::Emit(EmitError::Data { row: _, why }) => TierError::Refused { tier: TIER, inst: None, what: why },
        RecordEmitError::Emit(EmitError::Record { why }) => TierError::Refused { tier: TIER, inst: None, what: why },
        RecordEmitError::Emit(EmitError::Legalize(why)) | RecordEmitError::Legalize(why) => legalize_refused(TIER, why),
        // The seam's refusal holds a static reason only: the line and token stay in the
        // emitter's own error (`EmitError::Asm`'s `Display`).
        RecordEmitError::Emit(EmitError::Asm { inst, line: _, why, token: _ }) => {
            TierError::Refused { tier: TIER, inst: Some(inst), what: why }
        }
        RecordEmitError::NoSymbol { of } => TierError::NoName { tier: TIER, of },
        RecordEmitError::CName { of: _ } => {
            TierError::Refused { tier: TIER, inst: None, what: "a C name given for a reference other than a function or a static" }
        }
        RecordEmitError::CAddend { of: _, addend: _ } => {
            TierError::Refused { tier: TIER, inst: None, what: "data holding a C name's address plus an addend" }
        }
        // The seam hands no trap sources (`emit_record`'s are none), so none is past the end.
        RecordEmitError::TrapOrdinal { ordinal: _, unreachables: _ } => {
            TierError::Refused { tier: TIER, inst: None, what: "a trap source past the function's `unreachable` instructions" }
        }
    }
}

/// Legalize's refusal as the seam's, for the tier `TIER` that ran it (Reference always; Fast for
/// a function holding an `f128`).
pub(crate) fn legalize_refused(tier: TierName, why: blaze_lir::legalize::LegalizeError) -> TierError {
    match why {
        blaze_lir::legalize::LegalizeError::Refused { inst, op: _, what } => TierError::Refused { tier, inst: Some(inst as u32), what },
        blaze_lir::legalize::LegalizeError::NoRoutine(_) => {
            TierError::Refused { tier, inst: None, what: "an `f128` operation whose compiler_builtins routine is not bound (INTRINSICS.md P5b)" }
        }
        blaze_lir::legalize::LegalizeError::Scalable => TierError::Refused { tier, inst: None, what: "a scalable vector" },
        blaze_lir::legalize::LegalizeError::Full { table } => TierError::TooLarge { tier, what: table },
        blaze_lir::legalize::LegalizeError::Verify(_) => {
            TierError::Refused { tier, inst: None, what: "legalize's output does not verify (a bug of blaze-lir's legalize)" }
        }
    }
}
