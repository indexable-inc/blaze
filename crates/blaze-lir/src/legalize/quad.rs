//! `f128` to routine calls (INTRINSICS.md 3.5). Each routine is the pinned compiler_builtins'
//! own (library/compiler-builtins/compiler-builtins/src/float/{add,sub,mul,div,extend,trunc,
//! conv,cmp}.rs of nightly-2026-10-27), `extern "C"`, by its symbol.
//!
//! Comparisons follow libgcc's contract as compiler_builtins states it (float/cmp.rs, "Conversions
//! to match GCC intrinsics"): "`unord`: nonzero if either NaN, 0 otherwise"; "`eq`, `ne`: 0 if
//! a == b and both YaN, nonzero otherwise"; "`ge`, `gt`, `lt`, `le`: return an int result that
//! provides the same comparison to 0 if both YaN and the comparison matches", unordered giving
//! 1 from `lt`/`le`/`eq` (`to_default_cmp_result`) and -1 from `gt`/`ge`
//! (`to_gt_ge_cmp_result`); on aarch64 the result is `i32` (`CmpResult = i32`, "Aarch64 uses
//! `int` rather than a pointer-sized value").

use super::{LegalizeError, Out};
use crate::{
    Op, Ty,
    flags::{FloatCond, IntCond},
    ty::Scalar,
};

/// A compiler_builtins routine for `f128` (and the `f16` conversions through it).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum SoftFloat {
    AddTf3,
    SubTf3,
    MulTf3,
    DivTf3,
    ExtendHfTf2,
    TruncTfHf2,
    ExtendSfTf2,
    ExtendDfTf2,
    TruncTfSf2,
    TruncTfDf2,
    FixTfDi,
    FixTfTi,
    FixUnsTfDi,
    FixUnsTfTi,
    FloatDiTf,
    FloatTiTf,
    FloatUnDiTf,
    FloatUnTiTf,
    EqTf2,
    LtTf2,
    LeTf2,
    GtTf2,
    GeTf2,
    UnordTf2,
    /// `frem` of `f128`: libm's `fmodf128`, which compiler_builtins exports under its
    /// `#[no_mangle]` name as it does the tf routines (src/math/mod.rs, `full_availability`,
    /// `#[cfg(f128_enabled)] libm_intrinsics! { .. fn fmodf128(x: f128, y: f128) -> f128; .. }`,
    /// through the same `intrinsics!` macro). Exact, as C's `fmod` is: the result has the
    /// dividend's sign, which Rust's `%` of floats is (LIR's `frem`).
    FmodF128,
}

impl SoftFloat {
    pub const ALL: [SoftFloat; 25] = [
        SoftFloat::AddTf3,
        SoftFloat::SubTf3,
        SoftFloat::MulTf3,
        SoftFloat::DivTf3,
        SoftFloat::ExtendHfTf2,
        SoftFloat::TruncTfHf2,
        SoftFloat::ExtendSfTf2,
        SoftFloat::ExtendDfTf2,
        SoftFloat::TruncTfSf2,
        SoftFloat::TruncTfDf2,
        SoftFloat::FixTfDi,
        SoftFloat::FixTfTi,
        SoftFloat::FixUnsTfDi,
        SoftFloat::FixUnsTfTi,
        SoftFloat::FloatDiTf,
        SoftFloat::FloatTiTf,
        SoftFloat::FloatUnDiTf,
        SoftFloat::FloatUnTiTf,
        SoftFloat::EqTf2,
        SoftFloat::LtTf2,
        SoftFloat::LeTf2,
        SoftFloat::GtTf2,
        SoftFloat::GeTf2,
        SoftFloat::UnordTf2,
        SoftFloat::FmodF128,
    ];

    /// Its `#[no_mangle]` symbol in compiler_builtins.
    pub const fn symbol(self) -> &'static str {
        match self {
            SoftFloat::AddTf3 => "__addtf3",
            SoftFloat::SubTf3 => "__subtf3",
            SoftFloat::MulTf3 => "__multf3",
            SoftFloat::DivTf3 => "__divtf3",
            SoftFloat::ExtendHfTf2 => "__extendhftf2",
            SoftFloat::TruncTfHf2 => "__trunctfhf2",
            SoftFloat::ExtendSfTf2 => "__extendsftf2",
            SoftFloat::ExtendDfTf2 => "__extenddftf2",
            SoftFloat::TruncTfSf2 => "__trunctfsf2",
            SoftFloat::TruncTfDf2 => "__trunctfdf2",
            SoftFloat::FixTfDi => "__fixtfdi",
            SoftFloat::FixTfTi => "__fixtfti",
            SoftFloat::FixUnsTfDi => "__fixunstfdi",
            SoftFloat::FixUnsTfTi => "__fixunstfti",
            SoftFloat::FloatDiTf => "__floatditf",
            SoftFloat::FloatTiTf => "__floattitf",
            SoftFloat::FloatUnDiTf => "__floatunditf",
            SoftFloat::FloatUnTiTf => "__floatuntitf",
            SoftFloat::EqTf2 => "__eqtf2",
            SoftFloat::LtTf2 => "__lttf2",
            SoftFloat::LeTf2 => "__letf2",
            SoftFloat::GtTf2 => "__gttf2",
            SoftFloat::GeTf2 => "__getf2",
            SoftFloat::UnordTf2 => "__unordtf2",
            SoftFloat::FmodF128 => "fmodf128",
        }
    }

    /// Its bit in [`super::Legalized::routines`].
    pub const fn bit(self) -> u32 {
        1 << self as u32
    }

    /// Its C signature as compiler_builtins declares it: parameters, then the one result.
    /// Unsigned integers are LIR integers of their width.
    pub const fn signature(self) -> (&'static [Ty], Ty) {
        const Q: Ty = Ty::F128;
        match self {
            SoftFloat::AddTf3 | SoftFloat::SubTf3 | SoftFloat::MulTf3 | SoftFloat::DivTf3 | SoftFloat::FmodF128 => (&[Q, Q], Q),
            SoftFloat::ExtendHfTf2 => (&[Ty::F16], Q),
            SoftFloat::ExtendSfTf2 => (&[Ty::F32], Q),
            SoftFloat::ExtendDfTf2 => (&[Ty::F64], Q),
            SoftFloat::TruncTfHf2 => (&[Q], Ty::F16),
            SoftFloat::TruncTfSf2 => (&[Q], Ty::F32),
            SoftFloat::TruncTfDf2 => (&[Q], Ty::F64),
            SoftFloat::FixTfDi | SoftFloat::FixUnsTfDi => (&[Q], Ty::I64),
            SoftFloat::FixTfTi | SoftFloat::FixUnsTfTi => (&[Q], Ty::I128),
            SoftFloat::FloatDiTf | SoftFloat::FloatUnDiTf => (&[Ty::I64], Q),
            SoftFloat::FloatTiTf | SoftFloat::FloatUnTiTf => (&[Ty::I128], Q),
            SoftFloat::EqTf2
            | SoftFloat::LtTf2
            | SoftFloat::LeTf2
            | SoftFloat::GtTf2
            | SoftFloat::GeTf2
            | SoftFloat::UnordTf2 => (&[Q, Q], Ty::I32),
        }
    }
}

const _: () = assert!(SoftFloat::ALL.len() <= 32, "Legalized::routines holds a bit per routine");

/// The sign bit of an `f128` (IEEE 754 binary128: 1 sign, 15 exponent, 112 fraction bits).
const SIGN: u128 = 1 << 127;
/// binary128's exponent bias.
const BIAS: u128 = 16383;

/// The bits of `2^k` as an `f128`.
const fn power_of_two(k: u32) -> u128 {
    (BIAS + k as u128) << 112
}

impl Out<'_, '_, '_> {
    /// `op` touching `f128` as routine calls and integer operations; `None` for an operation
    /// that only moves an `f128` (emitted as it is).
    pub(super) fn quad(
        &mut self,
        inst: usize,
        op: Op,
        cond: u32,
        ty: Ty,
        src: Ty,
        args: &[u32],
    ) -> Result<Option<u32>, LegalizeError> {
        let value = match op {
            Op::Fadd => self.routine(inst, SoftFloat::AddTf3, args)?,
            Op::Fsub => self.routine(inst, SoftFloat::SubTf3, args)?,
            Op::Fmul => self.routine(inst, SoftFloat::MulTf3, args)?,
            Op::Fdiv => self.routine(inst, SoftFloat::DivTf3, args)?,
            // "fused or not as the consumer finds faster" (op.rs, `Fmuladd`): not fused.
            Op::Fmuladd => {
                let product = self.routine(inst, SoftFloat::MulTf3, &args[..2])?;
                self.routine(inst, SoftFloat::AddTf3, &[product, args[2]])?
            }
            Op::Fneg | Op::Fabs | Op::Fcopysign => self.quad_sign(op, args),
            Op::Fcmp => self.compare(inst, cond, args)?,
            Op::Fpext => {
                let r = match src.elem() {
                    Scalar::F16 => SoftFloat::ExtendHfTf2,
                    Scalar::F32 => SoftFloat::ExtendSfTf2,
                    Scalar::F64 => SoftFloat::ExtendDfTf2,
                    _ => return Err(self.refuse(inst, "an `fpext` to `f128` from a type that is not a narrower float")),
                };
                self.routine(inst, r, args)?
            }
            Op::Fptrunc => {
                let r = match ty.elem() {
                    Scalar::F16 => SoftFloat::TruncTfHf2,
                    Scalar::F32 => SoftFloat::TruncTfSf2,
                    Scalar::F64 => SoftFloat::TruncTfDf2,
                    _ => return Err(self.refuse(inst, "an `fptrunc` of `f128` to a type that is not a narrower float")),
                };
                self.routine(inst, r, args)?
            }
            Op::FptoSi | Op::FptoUi => self.to_int(inst, op == Op::FptoSi, ty, args[0])?,
            Op::SitoFp | Op::UitoFp => self.from_int(inst, op == Op::SitoFp, src, args[0])?,
            Op::Frem => self.routine(inst, SoftFloat::FmodF128, args)?,
            Op::Sqrt => return Err(self.refuse(inst, "an `f128` square root (no compiler_builtins routine; libm's `sqrtf128` is not bound)")),
            Op::Fma => return Err(self.refuse(inst, "an `f128` fused multiply-add (no compiler_builtins routine; libm's `fmaf128` is not bound)")),
            Op::Floor | Op::Ceil | Op::Ftrunc | Op::Nearest | Op::Fround => {
                return Err(self.refuse(inst, "an `f128` rounding to an integer (no compiler_builtins routine; libm's is not bound)"));
            }
            Op::FminNum | Op::FmaxNum | Op::Fminimum | Op::Fmaximum => {
                return Err(self.refuse(inst, "an `f128` minimum or maximum (no compiler_builtins routine; libm's is not bound)"));
            }
            _ => return Ok(None),
        };
        Ok(Some(value))
    }

    /// `fneg`, `fabs` and `fcopysign` of `f128` on its bits: they change the sign bit only
    /// (IEEE 754-2019 5.5.1, "Sign bit operations": "only the sign bit" is affected, NaN
    /// payloads kept), which an integer operation on the bits does exactly.
    fn quad_sign(&mut self, op: Op, args: &[u32]) -> u32 {
        let a = self.emit(Op::Bitcast, 0, Ty::I128, 0, &[args[0]]);
        let bits = match op {
            Op::Fneg => {
                let sign = self.iconst(Ty::I128, SIGN);
                self.emit(Op::Xor, 0, Ty::I128, 0, &[a, sign])
            }
            Op::Fabs => {
                let magnitude = self.iconst(Ty::I128, !SIGN);
                self.emit(Op::And, 0, Ty::I128, 0, &[a, magnitude])
            }
            _ => {
                let b = self.emit(Op::Bitcast, 0, Ty::I128, 0, &[args[1]]);
                let magnitude = self.iconst(Ty::I128, !SIGN);
                let sign = self.iconst(Ty::I128, SIGN);
                let kept = self.emit(Op::And, 0, Ty::I128, 0, &[a, magnitude]);
                let taken = self.emit(Op::And, 0, Ty::I128, 0, &[b, sign]);
                self.emit(Op::Or, 0, Ty::I128, 0, &[kept, taken])
            }
        };
        self.emit(Op::Bitcast, 0, Ty::F128, 0, &[bits])
    }

    /// `fcmp cond a, b` of `f128`: the libgcc routines' results against 0 (the module).
    fn compare(&mut self, inst: usize, cond: u32, args: &[u32]) -> Result<u32, LegalizeError> {
        let Some(cond) = FloatCond::from_word(cond) else {
            return Err(self.refuse(inst, "a float comparison of no known condition"));
        };
        let against = |out: &mut Self, r: SoftFloat, int: IntCond| -> Result<u32, LegalizeError> {
            let result = out.routine(inst, r, args)?;
            let zero = out.iconst(Ty::I32, 0);
            Ok(out.icmp(int, result, zero))
        };
        Ok(match cond {
            FloatCond::False => self.iconst(Ty::I1, 0),
            FloatCond::True => self.iconst(Ty::I1, 1),
            FloatCond::Oeq => against(self, SoftFloat::EqTf2, IntCond::Eq)?,
            FloatCond::Une => against(self, SoftFloat::EqTf2, IntCond::Ne)?,
            FloatCond::Olt => against(self, SoftFloat::LtTf2, IntCond::Slt)?,
            FloatCond::Ole => against(self, SoftFloat::LeTf2, IntCond::Sle)?,
            FloatCond::Ogt => against(self, SoftFloat::GtTf2, IntCond::Sgt)?,
            FloatCond::Oge => against(self, SoftFloat::GeTf2, IntCond::Sge)?,
            // Unordered or less: `ge` is -1 for both.
            FloatCond::Ult => against(self, SoftFloat::GeTf2, IntCond::Slt)?,
            // Unordered, less or equal: `gt` is -1 or 0 for those.
            FloatCond::Ule => against(self, SoftFloat::GtTf2, IntCond::Sle)?,
            // Unordered or greater: `le` is 1 for both.
            FloatCond::Ugt => against(self, SoftFloat::LeTf2, IntCond::Sgt)?,
            // Unordered, greater or equal: `lt` is 1 or 0 for those.
            FloatCond::Uge => against(self, SoftFloat::LtTf2, IntCond::Sge)?,
            FloatCond::Uno => against(self, SoftFloat::UnordTf2, IntCond::Ne)?,
            FloatCond::Ord => against(self, SoftFloat::UnordTf2, IntCond::Eq)?,
            FloatCond::One => {
                let ordered = against(self, SoftFloat::UnordTf2, IntCond::Eq)?;
                let unequal = against(self, SoftFloat::EqTf2, IntCond::Ne)?;
                self.emit(Op::And, 0, Ty::I1, 0, &[ordered, unequal])
            }
            FloatCond::Ueq => {
                let unordered = against(self, SoftFloat::UnordTf2, IntCond::Ne)?;
                let equal = against(self, SoftFloat::EqTf2, IntCond::Eq)?;
                self.emit(Op::Or, 0, Ty::I1, 0, &[unordered, equal])
            }
        })
    }

    /// `fptosi`/`fptoui` of `f128` to `ty`: Rust's `as` (the Reference, expressions/
    /// operator-expr.html, "Numeric cast": "Casting from a float to an integer will round the
    /// float towards zero; NaN will return 0; Values larger than the maximum integer value,
    /// including INFINITY, will saturate to the maximum value of the integer type; Values
    /// smaller than the minimum integer value, including NEG_INFINITY, will saturate to the
    /// minimum value of the integer type"), which LIR's `fpto*i` is (op.rs: "Saturates (Rust's
    /// `as`) ... NaN is 0"). The routine's result is clamped by comparisons, so its own
    /// behaviour out of range is never relied on.
    fn to_int(&mut self, inst: usize, signed: bool, ty: Ty, x: u32) -> Result<u32, LegalizeError> {
        let n = ty.bits(self.ptr_bits());
        if !ty.is_scalar() || !ty.is_int() || !matches!(n, 8 | 16 | 32 | 64 | 128) {
            return Err(self.refuse(inst, "a conversion of `f128` to an integer that is not 8, 16, 32, 64 or 128 bits"));
        }
        let (fix, wide) = match (signed, n) {
            (true, 128) => (SoftFloat::FixTfTi, Ty::I128),
            (true, _) => (SoftFloat::FixTfDi, Ty::I64),
            (false, 128) => (SoftFloat::FixUnsTfTi, Ty::I128),
            (false, _) => (SoftFloat::FixUnsTfDi, Ty::I64),
        };
        let mut r = self.routine(inst, fix, &[x])?;
        if wide != ty {
            r = self.emit(Op::Trunc, 0, ty, 0, &[r]);
        }
        let mask = match n {
            128 => u128::MAX,
            n => (1u128 << n) - 1,
        };
        let (low, high, min, max) = match signed {
            // Below -2^(n-1) gives MIN, at or above 2^(n-1) gives MAX.
            true => (SIGN | power_of_two(n - 1), power_of_two(n - 1), 1u128 << (n - 1), (1u128 << (n - 1)) - 1),
            // Below 0 gives 0 (a value in (-1, 0) truncates to 0 too), at or above 2^n MAX.
            false => (0, power_of_two(n), 0, mask),
        };
        let low = self.fconst(Ty::F128, low);
        let high = self.fconst(Ty::F128, high);
        let below = {
            let result = self.routine(inst, SoftFloat::LtTf2, &[x, low])?;
            let zero = self.iconst(Ty::I32, 0);
            self.icmp(IntCond::Slt, result, zero)
        };
        let above = {
            let result = self.routine(inst, SoftFloat::GeTf2, &[x, high])?;
            let zero = self.iconst(Ty::I32, 0);
            self.icmp(IntCond::Sge, result, zero)
        };
        let nan = {
            let result = self.routine(inst, SoftFloat::UnordTf2, &[x, x])?;
            let zero = self.iconst(Ty::I32, 0);
            self.icmp(IntCond::Ne, result, zero)
        };
        let min = self.iconst(ty, min);
        let max = self.iconst(ty, max);
        let zero = self.iconst(ty, 0);
        let r = self.emit(Op::Select, 0, ty, 0, &[below, min, r]);
        let r = self.emit(Op::Select, 0, ty, 0, &[above, max, r]);
        Ok(self.emit(Op::Select, 0, ty, 0, &[nan, zero, r]))
    }

    /// `sitofp`/`uitofp` to `f128` from an integer of type `src`: through the 64-bit routine
    /// after a sign or zero extension (exact), or the 128-bit one. Every integer of up to 113
    /// bits is exact in `f128`; the routines round a wider one once.
    fn from_int(&mut self, inst: usize, signed: bool, src: Ty, a: u32) -> Result<u32, LegalizeError> {
        let n = src.bits(self.ptr_bits());
        if !src.is_scalar() || !src.is_int() || n == 0 || n > 128 {
            return Err(self.refuse(inst, "a conversion to `f128` from a type that is not a scalar integer"));
        }
        let (r, wide) = match (signed, n) {
            (true, 65..) => (SoftFloat::FloatTiTf, Ty::I128),
            (true, _) => (SoftFloat::FloatDiTf, Ty::I64),
            (false, 65..) => (SoftFloat::FloatUnTiTf, Ty::I128),
            (false, _) => (SoftFloat::FloatUnDiTf, Ty::I64),
        };
        let mut a = a;
        if src != wide {
            let ext = match signed {
                true => Op::Sext,
                false => Op::Zext,
            };
            a = self.emit(ext, 0, wide, 0, &[a]);
        }
        self.routine(inst, r, &[a])
    }
}
