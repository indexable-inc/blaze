//! What the generator draws: every op of the stencil set ([`Op::all`]) at every type its
//! [`Op::info`] has stencils of, less what can't be compared and what Cranelift 0.136.2 does
//! not lower for the instruction set. The exceptions per instruction set are
//! `cranelift-fuzzgen`'s `valid_for_target` (the list its maintainers keep of lowerings that are
//! missing or known wrong), checked against `isa/{x64,aarch64}/lower.isle`.

use cranelift_codegen::ir::condcodes::FloatCC;

use crate::shape::{Isa, Op, Ty};

/// The types values are made of: the IR builder's, less `f16` and `f128`, whose arithmetic it
/// lowers to calls and whose stencils are only moves.
pub(super) const TYS: [Ty; 13] = [
    Ty::I8,
    Ty::I16,
    Ty::I32,
    Ty::I64,
    Ty::I128,
    Ty::F32,
    Ty::F64,
    Ty::I8x16,
    Ty::I16x8,
    Ty::I32x4,
    Ty::I64x2,
    Ty::F32x4,
    Ty::F64x2,
];

pub(super) const INTS: [Ty; 4] = [Ty::I8, Ty::I16, Ty::I32, Ty::I64];
pub(super) const INTS_WIDE: [Ty; 5] = [Ty::I8, Ty::I16, Ty::I32, Ty::I64, Ty::I128];

/// The index of `ty` in [`TYS`].
pub(super) fn index(ty: Ty) -> usize {
    TYS.iter().position(|it| *it == ty).unwrap_or_else(|| panic!("{} is not a type the generator makes", ty.name()))
}

/// One op the generator draws, with the types it draws it at.
pub(super) struct Candidate {
    pub(super) op: Op,
    pub(super) tys: Vec<Ty>,
}

/// Every op drawn for `isa`, in [`Op::all`]'s order.
pub(super) fn candidates(isa: Isa) -> Vec<Candidate> {
    Op::all()
        .filter(|op| op.on(isa) && drawn(*op))
        .filter_map(|op| {
            let tys: Vec<Ty> = op
                .info()
                .tys
                .iter()
                .filter(|ty| TYS.contains(ty) && allowed(isa, op, *ty))
                .filter(|ty| !has_sources(op) || !sources(isa, op, *ty).is_empty())
                .collect();
            (!tys.is_empty()).then_some(Candidate { op, tys })
        })
        .collect()
}

/// Whether the generator draws `op` as an instruction of its own. Not drawn:
///
/// - `Copy` (block arguments are), `Jump`, `Brif` and `BrIcmp` (the control flow the generator
///   lays out is), `StackAddr` (every memory access makes one);
/// - the carry-in adds and borrow-in subtracts, which neither x64's nor AArch64's lowering in
///   Cranelift 0.136.2 has a rule for, so no IR Cranelift compiles has them;
/// - `fcvt_to_uint` and `fcvt_to_sint`, which trap on NaN and out-of-range values: the
///   generator converts with `_sat` only;
/// - the extending vector loads (`uload8x8` and the rest), `blendv` and the `x86_*` ops, which
///   `cranelift-interpreter` does not implement, so nothing would say what they mean;
/// - `load_confined` and `store_confined`, the fusion of an access with its address
///   arithmetic: the generator draws the plain access and the arithmetic, but not yet the one
///   address pattern the patcher fuses (`stencil.rs` runs every confined shape).
fn drawn(op: Op) -> bool {
    !matches!(
        op,
        Op::Copy
            | Op::Jump
            | Op::Brif
            | Op::BrIcmp
            | Op::StackAddr
            | Op::UaddOverflowCin
            | Op::SaddOverflowCin
            | Op::UsubOverflowBin
            | Op::SsubOverflowBin
            | Op::FcvtToUint
            | Op::FcvtToSint
            | Op::Uload8x8
            | Op::Sload8x8
            | Op::Uload16x4
            | Op::Sload16x4
            | Op::Uload32x2
            | Op::Sload32x2
            | Op::Blendv
            | Op::X86Pshufb
            | Op::X86Pmulhrsw
            | Op::X86Pmaddubsw
            | Op::X86Cvtt2dq
            | Op::LoadConfined
            | Op::StoreConfined
    )
}

/// Whether `op` at `ty` is drawn on `isa`: what both Cranelift lowers there and the
/// interpreter runs.
fn allowed(isa: Isa, op: Op, ty: Ty) -> bool {
    let x64 = isa == Isa::X86_64;
    let vector = ty.is_vector();
    let float = matches!(ty, Ty::F32 | Ty::F64);
    match op {
        // The interpreter widens a lane to the type twice its width, which a vector isn't.
        Op::Umulhi | Op::Smulhi => !vector,
        // fuzzgen leaves the vector forms out on every target.
        Op::Fmin | Op::Fmax => !vector,
        // x64 has `pavgb`/`pavgw` only.
        Op::AvgRound => matches!(ty, Ty::I8x16 | Ty::I16x8),
        // An `i128` select becomes a min/max rule neither backend has.
        Op::Select => ty != Ty::I128,
        Op::Bitselect => !float && !(ty == Ty::I128 && !x64),
        Op::Band | Op::Bor | Op::Bxor | Op::Bnot => x64 || !float,
        Op::Bitcast => ty != Ty::I128,
        Op::AtomicCas => ty != Ty::I128,
        Op::Cls => !x64,
        Op::UaddSat | Op::SaddSat | Op::UsubSat | Op::SsubSat => !x64 || matches!(ty, Ty::I8x16 | Ty::I16x8),
        Op::Fcopysign | Op::Popcnt => !(x64 && vector),
        Op::Smin | Op::Umin | Op::Smax | Op::Umax | Op::Icmp => !(x64 && ty == Ty::I64x2),
        Op::Uunarrow | Op::IaddPairwise => !x64,
        // From `i64x2` operands.
        Op::Snarrow | Op::Unarrow | Op::SqmulRoundSat => !(x64 && ty == Ty::I32x4),
        Op::VhighBits => x64 || !matches!(ty, Ty::F32x4 | Ty::F64x2),
        Op::FcvtToUintSat | Op::FcvtToSintSat => !(x64 && matches!(ty, Ty::I8 | Ty::I16 | Ty::I64x2)),
        Op::FcvtFromUint | Op::FcvtFromSint => !(x64 && ty == Ty::F64x2),
        // x64's `bitcast_gpr_to_xmm`, which lowers it from a GPR, has no 8-bit rule.
        Op::ScalarToVector => !(x64 && ty == Ty::I8x16),
        _ => true,
    }
}

/// Whether `op` takes an operand of a type other than its own ([`sources`]).
pub(super) fn has_sources(op: Op) -> bool {
    matches!(
        op,
        Op::Select
            | Op::Bmask
            | Op::Bitcast
            | Op::Ireduce
            | Op::Uextend
            | Op::Sextend
            | Op::FcvtToUintSat
            | Op::FcvtToSintSat
            | Op::FcvtFromUint
            | Op::FcvtFromSint
            | Op::ScalarToVector
            | Op::VhighBits
    )
}

/// For the ops of [`has_sources`] at `ty`: the types of the operand that differs (the
/// condition of `select`, the source of a conversion) or, for `vhigh_bits`, of the result.
pub(super) fn sources(isa: Isa, op: Op, ty: Ty) -> Vec<Ty> {
    let ints = |set: &[Ty]| set.to_vec();
    match op {
        Op::Select => ints(&INTS),
        Op::Bmask => ints(&INTS_WIDE),
        Op::Bitcast => TYS.into_iter().filter(|it| it.bytes() == ty.bytes() && *it != ty && *it != Ty::I128).collect(),
        Op::Ireduce => INTS_WIDE.into_iter().filter(|it| it.bytes() > ty.bytes()).collect(),
        Op::Uextend | Op::Sextend => INTS.into_iter().filter(|it| it.bytes() < ty.bytes()).collect(),
        Op::FcvtToUintSat | Op::FcvtToSintSat => match ty {
            Ty::I32x4 => vec![Ty::F32x4],
            Ty::I64x2 => vec![Ty::F64x2],
            _ if ty.is_vector() => Vec::new(),
            _ => vec![Ty::F32, Ty::F64],
        },
        Op::FcvtFromUint | Op::FcvtFromSint => match ty {
            Ty::F32x4 => vec![Ty::I32x4],
            Ty::F64x2 if isa == Isa::Aarch64 => vec![Ty::I64x2],
            Ty::F32 | Ty::F64 => ints(&INTS),
            _ => Vec::new(),
        },
        Op::ScalarToVector => vec![ty.lane()],
        Op::VhighBits => INTS.into_iter().filter(|it| u32::from(it.bytes()) * 8 >= u32::from(ty.lanes())).collect(),
        _ => Vec::new(),
    }
}

/// The float conditions `isa` lowers for a compare of scalars or of vectors.
pub(super) fn float_ccs(isa: Isa, vector: bool) -> Vec<FloatCC> {
    FloatCC::all()
        .iter()
        .copied()
        .filter(|cc| match isa {
            Isa::X86_64 => !(vector && matches!(cc, FloatCC::UnorderedOrEqual | FloatCC::OrderedNotEqual)),
            Isa::Aarch64 => {
                !matches!(cc, FloatCC::OrderedNotEqual | FloatCC::UnorderedOrEqual)
                    && !(vector
                        && matches!(
                            cc,
                            FloatCC::UnorderedOrLessThan
                                | FloatCC::UnorderedOrLessThanOrEqual
                                | FloatCC::UnorderedOrGreaterThan
                                | FloatCC::UnorderedOrGreaterThanOrEqual
                        ))
            }
        })
        .collect()
}

/// The amount types a shift or rotate of `ty` is drawn with: any integer for a scalar, and
/// for a vector any but `i128`, which neither backend lowers.
pub(super) fn amounts(ty: Ty) -> &'static [Ty] {
    if ty.is_vector() { &INTS } else { &INTS_WIDE }
}

#[test]
fn every_candidate_is_of_the_stencil_set() {
    for isa in Isa::ALL {
        let candidates = candidates(isa);
        assert!(candidates.len() > 80, "{isa:?} draws only {} ops", candidates.len());
        for candidate in &candidates {
            let info = candidate.op.info();
            for ty in &candidate.tys {
                assert!(info.tys.contains(*ty) && candidate.op.on(isa), "{} {}", info.name, ty.name());
                if has_sources(candidate.op) {
                    assert!(!sources(isa, candidate.op, *ty).is_empty(), "{} {}", info.name, ty.name());
                }
            }
        }
    }
}

#[test]
fn the_type_set_covers_what_every_drawn_op_needs() {
    for ty in TYS {
        assert_eq!(TYS[index(ty)], ty);
    }
    for isa in Isa::ALL {
        for candidate in candidates(isa) {
            for ty in &candidate.tys {
                for source in sources(isa, candidate.op, *ty) {
                    assert!(TYS.contains(&source), "{} from {}", candidate.op.info().name, source.name());
                }
            }
        }
        assert!(!float_ccs(isa, true).is_empty() && !float_ccs(isa, false).is_empty());
    }
}
