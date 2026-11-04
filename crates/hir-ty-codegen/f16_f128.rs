//! `f16` and `f128`, which Cranelift carries (loads, stores, calls, `select`) but has no
//! arithmetic for on the targets LLVM has none for either. Each operation is lowered the way
//! LLVM lowers it for rustc, so that the bits come out the same:
//!
//! - an `f16` is widened to `f32` (to `f64` for `fma`), worked on there and narrowed back, as
//!   LLVM does without the half-precision instructions: by AArch64's `fcvt`, and on x86-64 by
//!   compiler-builtins' `__extendhfsf2`, `__truncsfhf2` and `__truncdfhf2`; its sign bit is
//!   flipped or cleared in place. In an AArch64 function with the half-precision
//!   instructions (`fp16`) its arithmetic, square root, rounding and `fma` are those
//!   instructions, as LLVM selects them there, and in an x86-64 function with AVX512-FP16 its
//!   arithmetic is `vaddsh` and the like;
//! - an `f128` is handed to compiler-builtins (`__addtf3`, `__lttf2`, `__fixtfsi`, ...) and
//!   to C's functions of it: `long double`'s on AArch64 (`fmodl`, `sqrtl`, ...), `_Float128`'s
//!   on x86-64 (`fmodf128`, `sqrtf128`, ...), the calls LLVM makes.
//!
//! Only the targets this was compared with rustc on take them ([`soft_floats`]); others
//! refuse them, as before.

#![forbid(unsafe_code)]

use hir_ty::TargetFeature;

use super::*;

/// Whether `ty` is a float this module lowers.
pub(super) fn soft(ty: Type) -> bool {
    ty == types::F16 || ty == types::F128
}

/// Whether the target's `f16` and `f128` are lowered here: AArch64 and x86-64 with the
/// System V convention, where every lowering below was checked against rustc's, and Apple's
/// AArch64. Apple's convention passes both as AAPCS64 does, `f16` in an `h` and `f128` in a
/// `q` register, which Cranelift's `apple_aarch64` does under `enable_llvm_abi_extensions`;
/// LLVM's calls for them are AArch64's, `long double`'s `fmodl` among them though Apple's
/// `long double` is an `f64`, which is why std's `f128` math is off there.
pub(super) fn soft_floats(target: &Target) -> bool {
    match (target.arch, target.call_conv) {
        (Arch::Aarch64, isa::CallConv::SystemV | isa::CallConv::AppleAarch64) => true,
        (Arch::X86_64, isa::CallConv::SystemV) => true,
        // wasm32's LLVM passes an `f16` as the bits of an `i32` and an `f128` as an `i128`
        // (`wasm::Class`), and works on both through compiler-builtins, as on x86-64 without
        // F16C; its C `long double` is the `f128`, so C's functions of it are the `l` ones.
        (Arch::Wasm32 | Arch::Wasm64, _) => true,
        _ => false,
    }
}

/// The bits of the `f128` that is exactly `-magnitude` or `magnitude`, which has at most 113
/// significant bits.
fn quad_bits(negative: bool, magnitude: u128) -> u128 {
    if magnitude == 0 {
        return u128::from(negative) << 127;
    }
    let exponent = 127 - magnitude.leading_zeros();
    let significand = match exponent {
        ..=112 => magnitude << (112 - exponent),
        _ => magnitude >> (exponent - 112),
    };
    (u128::from(negative) << 127)
        | (u128::from(exponent + 16383) << 112)
        | (significand & ((1 << 112) - 1))
}

/// The largest integer of at most 113 significant bits that is not above `max`: the largest
/// `f128` an integer type holds.
fn quad_floor(max: u128) -> u128 {
    let width = 128 - max.leading_zeros();
    if width <= 113 { max } else { max & !((1 << (width - 113)) - 1) }
}

const HALF_SIGN: i64 = 0x8000;
const QUAD_SIGN: u128 = 1 << 127;

impl<'db> FunctionCx<'_, 'db> {
    /// Calls the function of `fcvt` from `from` to `to`.
    fn fcvt(&mut self, value: Value, from: Type, to: Type) -> Value {
        let letter = |ty: Type| match ty {
            types::F16 => 'h',
            types::F32 => 's',
            _ => 'd',
        };
        let mut sig = self.cg.make_signature();
        sig.params.push(AbiParam::new(from));
        sig.returns.push(AbiParam::new(to));
        let callee = self.cg.assembly(asm::fcvt(letter(to), letter(from)), &sig);
        let callee = self.cg.declare_func_in_func(callee, self.builder.func);
        let call = self.builder.ins().call(callee, &[value]);
        self.builder.inst_results(call)[0]
    }

    /// Whether the function being compiled has AArch64's half-precision instructions, which
    /// LLVM then works on an `f16` with: in its own register, rounded once, a signalling NaN
    /// of either operand taking precedence over a quiet one.
    pub(super) fn half_precision(&self) -> bool {
        self.cg.target.arch == Arch::Aarch64 && self.function_enables(TargetFeature::FP16)
    }

    /// Whether the function being compiled enables the target feature `feature`, itself or by
    /// one that implies it.
    pub(super) fn function_enables(&self, feature: TargetFeature) -> bool {
        self.body
            .and_then(|it| it.owner.as_function())
            .is_some_and(|func| hir_ty::TargetFeatures::from_fn(self.cg.db, func).enables(feature))
    }

    /// The half-precision `instruction` of `args`, passed from `h0` up (`xmm0` up on x86-64),
    /// its result in `h0` (`xmm0`).
    fn half_instruction(&mut self, instruction: &str, args: &[Value]) -> Value {
        let mut sig = self.cg.make_signature();
        sig.params.extend(args.iter().map(|_| AbiParam::new(types::F16)));
        sig.returns.push(AbiParam::new(types::F16));
        let text = if self.x86() { asm::x86_half(instruction) } else { asm::half(instruction) };
        let callee = self.cg.assembly(text, &sig);
        let callee = self.cg.declare_func_in_func(callee, self.builder.func);
        let call = self.builder.ins().call(callee, args);
        self.builder.inst_results(call)[0]
    }

    fn x86(&self) -> bool {
        self.cg.target.arch == Arch::X86_64
    }

    /// Whether an `f16` is converted by compiler-builtins (`__extendhfsf2`, `__truncsfhf2`,
    /// `__truncdfhf2`), as LLVM does on x86-64 and wasm32, not by AArch64's `fcvt`.
    /// `v2::refs` lists the same.
    pub(super) fn half_by_builtins(&self) -> bool {
        matches!(self.cg.target.arch, Arch::X86_64 | Arch::Wasm32 | Arch::Wasm64)
    }

    /// Whether the function being compiled is x86-64's with AVX512-FP16, whose instructions
    /// LLVM works on an `f16` with there.
    fn x86_half_precision(&self) -> bool {
        self.x86() && self.function_enables(TargetFeature::AVX512FP16)
    }

    /// Refuses an `f16` converted in a function with F16C (which AVX-512 implies): x86-64's
    /// LLVM converts it there by `vcvtph2ps` and `vcvtps2ph`, and works on it by AVX512-FP16's
    /// instructions where it has those, not by compiler-builtins.
    fn x86_soft_half(&self) -> Result<()> {
        if self.function_enables(TargetFeature::F16C) {
            return Err(CodegenError::Unsupported(
                "f16 in a function with F16C, which LLVM converts by its instructions".into(),
            ));
        }
        Ok(())
    }

    /// An `f16` as the `f32` or `f64` of the same value, which is exact. x86-64's LLVM goes
    /// to `f64` through `f32`.
    fn widen_half(&mut self, value: Value, to: Type) -> Result<Value> {
        if !self.half_by_builtins() {
            return Ok(self.fcvt(value, types::F16, to));
        }
        if self.x86() {
            self.x86_soft_half()?;
        }
        let single = self.builtin("__extendhfsf2", &[types::F16], types::F32, &[value])?;
        Ok(match to {
            types::F32 => single,
            _ => self.builder.ins().fpromote(to, single),
        })
    }

    /// An `f32` or `f64` rounded to the nearest `f16`, ties to even.
    fn narrow_to_half(&mut self, value: Value) -> Result<Value> {
        let from = self.builder.func.dfg.value_type(value);
        if !self.half_by_builtins() {
            return Ok(self.fcvt(value, from, types::F16));
        }
        if self.x86() {
            self.x86_soft_half()?;
        }
        let name = if from == types::F32 { "__truncsfhf2" } else { "__truncdfhf2" };
        self.builtin(name, &[from], types::F16, &[value])
    }

    /// The C function of `f128`, named with C's suffix of it: `[fmodf128, fmodl]` is `fmodl`
    /// where `long double` is it (AArch64), `fmodf128` on x86-64, whose `long double` is x87's.
    fn quad_libcall(&mut self, [f128, l]: [&str; 2], args: &[Value]) -> Result<Value> {
        let name = if self.x86() { f128 } else { l };
        self.float_libcall(name, types::F128, args)
    }

    fn bits_of(&mut self, value: Value) -> Value {
        let ty = self.builder.func.dfg.value_type(value);
        let int = if ty == types::F16 { types::I16 } else { types::I128 };
        self.builder.ins().bitcast(int, MemFlagsData::new(), value)
    }

    fn from_bits(&mut self, bits: Value, ty: Type) -> Value {
        self.builder.ins().bitcast(ty, MemFlagsData::new(), bits)
    }

    fn quad_const(&mut self, bits: u128) -> Value {
        let bits = self.iconst(types::I128, bits);
        self.from_bits(bits, types::F128)
    }

    /// `-value`, which flips the sign bit and nothing else, a NaN's included.
    pub(super) fn soft_neg(&mut self, value: Value) -> Value {
        let ty = self.builder.func.dfg.value_type(value);
        let bits = self.bits_of(value);
        let flipped = match ty {
            types::F16 => self.builder.ins().bxor_imm_u(bits, HALF_SIGN),
            _ => {
                let sign = self.iconst(types::I128, QUAD_SIGN);
                self.builder.ins().bxor(bits, sign)
            }
        };
        self.from_bits(flipped, ty)
    }

    /// `value` with its sign bit cleared.
    fn soft_abs(&mut self, value: Value) -> Value {
        let ty = self.builder.func.dfg.value_type(value);
        let bits = self.bits_of(value);
        let cleared = match ty {
            types::F16 => self.builder.ins().band_imm_u(bits, HALF_SIGN - 1),
            _ => {
                let rest = self.iconst(types::I128, !QUAD_SIGN);
                self.builder.ins().band(bits, rest)
            }
        };
        self.from_bits(cleared, ty)
    }

    /// `copysign`: the bits of `l` but the sign bit of `r`, a NaN's payload kept as Rust has
    /// it. (Optimizing LLVM does an `f16`'s in `f32`, which quiets a signalling NaN; rustc at
    /// `opt-level=0` moves the bits.)
    pub(super) fn soft_copysign(&mut self, l: Value, r: Value) -> Value {
        let ty = self.builder.func.dfg.value_type(l);
        let int = if ty == types::F16 { types::I16 } else { types::I128 };
        let sign = if ty == types::F16 { HALF_SIGN as u128 } else { QUAD_SIGN };
        let (l, r) = (self.bits_of(l), self.bits_of(r));
        let rest = self.iconst(int, !sign);
        let sign = self.iconst(int, sign);
        let l = self.builder.ins().band(l, rest);
        let r = self.builder.ins().band(r, sign);
        let bits = self.builder.ins().bor(l, r);
        self.from_bits(bits, ty)
    }

    /// An operator of two `f16`s or two `f128`s.
    pub(super) fn soft_binary_op(&mut self, op: &BinOp, l: Value, r: Value) -> Result<Value> {
        if self.builder.func.dfg.value_type(l) == types::F16 {
            // AArch64's, then AVX512-FP16's, operands in order: the first one's NaN where both
            // are.
            let instruction = match op {
                BinOp::Add => Some(["fadd h0, h0, h1", "vaddsh xmm0, xmm0, xmm1"]),
                BinOp::Sub => Some(["fsub h0, h0, h1", "vsubsh xmm0, xmm0, xmm1"]),
                BinOp::Mul => Some(["fmul h0, h0, h1", "vmulsh xmm0, xmm0, xmm1"]),
                BinOp::Div => Some(["fdiv h0, h0, h1", "vdivsh xmm0, xmm0, xmm1"]),
                _ => None,
            };
            if let Some([aarch64, _]) = instruction.filter(|_| self.half_precision()) {
                return Ok(self.half_instruction(aarch64, &[l, r]));
            }
            if let Some([_, x86]) = instruction.filter(|_| self.x86_half_precision()) {
                return Ok(self.half_instruction(x86, &[l, r]));
            }
            let (l, r) = (self.widen_half(l, types::F32)?, self.widen_half(r, types::F32)?);
            let value = self.float_binary_op(op, l, r)?;
            return match op {
                BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => Ok(value),
                _ => self.narrow_to_half(value),
            };
        }
        let (name, cc) = match op {
            BinOp::Add => ("__addtf3", None),
            BinOp::Sub => ("__subtf3", None),
            BinOp::Mul => ("__multf3", None),
            BinOp::Div => ("__divtf3", None),
            BinOp::Rem => return self.quad_libcall(["fmodf128", "fmodl"], &[l, r]),
            // Each comparison of compiler-builtins answers so that NaN makes it false.
            BinOp::Eq => ("__eqtf2", Some(IntCC::Equal)),
            BinOp::Ne => ("__netf2", Some(IntCC::NotEqual)),
            BinOp::Lt => ("__lttf2", Some(IntCC::SignedLessThan)),
            BinOp::Le => ("__letf2", Some(IntCC::SignedLessThanOrEqual)),
            BinOp::Gt => ("__gttf2", Some(IntCC::SignedGreaterThan)),
            BinOp::Ge => ("__getf2", Some(IntCC::SignedGreaterThanOrEqual)),
            // alloc: the error's message
            _ => return Err(CodegenError::Unsupported(format!("float operator {op:?}"))),
        };
        let quads = [types::F128; 2];
        Ok(match cc {
            None => self.builtin(name, &quads, types::F128, &[l, r])?,
            Some(cc) => {
                // compiler-builtins answers in a word on x86-64, as libgcc does, of which LLVM
                // reads the low 32 bits.
                let answer = match self.x86() {
                    true => {
                        let word = self.builtin(name, &quads, types::I64, &[l, r])?;
                        self.builder.ins().ireduce(types::I32, word)
                    }
                    false => self.builtin(name, &quads, types::I32, &[l, r])?,
                };
                self.builder.ins().icmp_imm_s(cc, answer, 0)
            }
        })
    }

    /// A comparison of two `f128`s by compiler-builtins.
    fn quad_compare(&mut self, op: &BinOp, l: Value, r: Value) -> Result<Value> {
        self.soft_binary_op(op, l, r)
    }

    /// `as` where one side is an `f16` or an `f128`.
    pub(super) fn soft_cast(
        &mut self,
        value: Value,
        from: AbiScalar,
        to: AbiScalar,
    ) -> Result<Value> {
        let single = AbiScalar { ty: types::F32, signed: true };
        if from.ty == to.ty {
            return Ok(value);
        }
        if self.x86() && (from.ty == types::F16 || to.ty == types::F16) {
            self.x86_soft_half()?;
        }
        Ok(match (from.ty, to.ty) {
            (types::F16, types::F32 | types::F64) => self.widen_half(value, to.ty)?,
            (types::F32 | types::F64, types::F16) => self.narrow_to_half(value)?,
            (types::F16, types::F128) => {
                self.builtin("__extendhftf2", &[types::F16], types::F128, &[value])?
            }
            (types::F32, types::F128) => {
                self.builtin("__extendsftf2", &[types::F32], types::F128, &[value])?
            }
            (types::F64, types::F128) => {
                self.builtin("__extenddftf2", &[types::F64], types::F128, &[value])?
            }
            (types::F128, types::F16) => {
                self.builtin("__trunctfhf2", &[types::F128], types::F16, &[value])?
            }
            (types::F128, types::F32) => {
                self.builtin("__trunctfsf2", &[types::F128], types::F32, &[value])?
            }
            (types::F128, types::F64) => {
                self.builtin("__trunctfdf2", &[types::F128], types::F64, &[value])?
            }
            // An `f16` goes to an integer as the `f32` it is.
            (types::F16, _) => {
                let wide = self.widen_half(value, types::F32)?;
                self.float_cast(wide, single, to)?
            }
            // x86-64's and wasm32's LLVM, which soft-promote an `f16`, round a 128-bit
            // integer to it once, by compiler-builtins.
            (types::I128, types::F16) if self.half_by_builtins() => {
                let name = if from.signed { "__floattihf" } else { "__floatuntihf" };
                self.builtin(name, &[types::I128], types::F16, &[value])?
            }
            // Any other integer goes to `f16` through `f32`, rounded twice where it does not
            // fit `f32`, as LLVM converts it.
            (_, types::F16) => {
                let wide = self.float_cast(value, from, single)?;
                self.narrow_to_half(wide)?
            }
            (types::F128, _) => self.quad_to_int(value, to)?,
            (_, types::F128) => self.int_to_quad(value, from)?,
            _ => {
                // alloc: the error's message
                let why = format!("cast from {} to {}", from.ty, to.ty);
                return Err(CodegenError::Unsupported(why));
            }
        })
    }

    /// An integer as the `f128` of its value, which is exact up to 64 bits.
    fn int_to_quad(&mut self, value: Value, from: AbiScalar) -> Result<Value> {
        let (name, ty) = match (from.ty.bits(), from.signed) {
            (..=32, true) => ("__floatsitf", types::I32),
            (..=32, false) => ("__floatunsitf", types::I32),
            (64, true) => ("__floatditf", types::I64),
            (64, false) => ("__floatunditf", types::I64),
            (_, true) => ("__floattitf", types::I128),
            (_, false) => ("__floatuntitf", types::I128),
        };
        let value = match from.ty.bits() < ty.bits() {
            true if from.signed => self.builder.ins().sextend(ty, value),
            true => self.builder.ins().uextend(ty, value),
            false => value,
        };
        self.builtin(name, &[ty], types::F128, &[value])
    }

    /// An `f128` as an integer, saturating and with NaN as zero as `as` has it. LLVM checks the
    /// bounds itself around compiler-builtins' conversion, which leaves them undefined.
    fn quad_to_int(&mut self, value: Value, to: AbiScalar) -> Result<Value> {
        let bits = to.ty.bits();
        let (name, ty) = match (bits, to.signed) {
            (..=32, true) => ("__fixtfsi", types::I32),
            (..=32, false) => ("__fixunstfsi", types::I32),
            (64, true) => ("__fixtfdi", types::I64),
            (64, false) => ("__fixunstfdi", types::I64),
            (_, true) => ("__fixtfti", types::I128),
            (_, false) => ("__fixunstfti", types::I128),
        };
        let (min, max): (u128, u128) = match to.signed {
            true => (1 << (bits - 1), (1 << (bits - 1)) - 1),
            false => (0, u128::MAX >> (128 - bits)),
        };
        let low = self.quad_const(quad_bits(to.signed, min));
        let high = self.quad_const(quad_bits(false, quad_floor(max)));
        let above_low = self.quad_compare(&BinOp::Ge, value, low)?;
        let raw = self.builtin(name, &[types::F128], ty, &[value])?;
        let raw = if ty.bits() > bits { self.builder.ins().ireduce(to.ty, raw) } else { raw };
        let min = if to.signed { min.wrapping_neg() } else { 0 };
        let (min, max) = (self.iconst(to.ty, min), self.iconst(to.ty, max));
        let value_or_min = self.builder.ins().select(above_low, raw, min);
        let above_high = self.quad_compare(&BinOp::Gt, value, high)?;
        let saturated = self.builder.ins().select(above_high, max, value_or_min);
        if !to.signed {
            // NaN is not above zero, so it is zero already.
            return Ok(saturated);
        }
        let ordered = self.quad_compare(&BinOp::Eq, value, value)?;
        let zero = self.iconst(to.ty, 0);
        Ok(self.builder.ins().select(ordered, saturated, zero))
    }

    /// `fabs`, `sqrt`, `floor`, `ceil`, `trunc`, `round_ties_even` or `round` of an `f16` or an
    /// `f128`.
    pub(super) fn soft_unary(&mut self, intrinsic: Intrinsic, value: Value) -> Result<Value> {
        let ty = self.builder.func.dfg.value_type(value);
        if intrinsic == Intrinsic::Fabs {
            return Ok(self.soft_abs(value));
        }
        if ty == types::F16 && self.half_precision() {
            let instruction = match intrinsic {
                Intrinsic::Sqrt => "fsqrt h0, h0",
                Intrinsic::Floor => "frintm h0, h0",
                Intrinsic::Ceil => "frintp h0, h0",
                Intrinsic::Trunc => "frintz h0, h0",
                // `rint`, in the mode a Rust program runs in, to even.
                Intrinsic::RoundTiesEven => "frintx h0, h0",
                _ => "frinta h0, h0",
            };
            return Ok(self.half_instruction(instruction, &[value]));
        }
        if ty == types::F16 {
            let wide = self.widen_half(value, types::F32)?;
            let value = match intrinsic {
                Intrinsic::Sqrt => self.builder.ins().sqrt(wide),
                // x86-64 without SSE4.1 has no instruction for them: LLVM calls libm.
                Intrinsic::Floor if self.x86() => {
                    self.float_libcall("floorf", types::F32, &[wide])?
                }
                Intrinsic::Ceil if self.x86() => {
                    self.float_libcall("ceilf", types::F32, &[wide])?
                }
                Intrinsic::Trunc if self.x86() => {
                    self.float_libcall("truncf", types::F32, &[wide])?
                }
                Intrinsic::RoundTiesEven if self.x86() => {
                    self.float_libcall("rintf", types::F32, &[wide])?
                }
                Intrinsic::Floor => self.builder.ins().floor(wide),
                Intrinsic::Ceil => self.builder.ins().ceil(wide),
                Intrinsic::Trunc => self.builder.ins().trunc(wide),
                Intrinsic::RoundTiesEven => self.builder.ins().nearest(wide),
                _ => self.float_libcall("roundf", types::F32, &[wide])?,
            };
            return self.narrow_to_half(value);
        }
        let libm = match intrinsic {
            Intrinsic::Sqrt => ["sqrtf128", "sqrtl"],
            Intrinsic::Floor => ["floorf128", "floorl"],
            Intrinsic::Ceil => ["ceilf128", "ceill"],
            Intrinsic::Trunc => ["truncf128", "truncl"],
            // In the rounding mode a Rust program runs in, ties to even.
            Intrinsic::RoundTiesEven => ["rintf128", "rintl"],
            _ => ["roundf128", "roundl"],
        };
        self.quad_libcall(libm, &[value])
    }

    /// `fma`, which LLVM does for an `f16` in `f64`, where the product is exact.
    pub(super) fn soft_fma(&mut self, a: Value, b: Value, c: Value) -> Result<Value> {
        if self.builder.func.dfg.value_type(a) == types::F128 {
            return self.quad_libcall(["fmaf128", "fmal"], &[a, b, c]);
        }
        if self.half_precision() {
            return Ok(self.half_instruction("fmadd h0, h0, h1, h2", &[a, b, c]));
        }
        let mut wide = [a, b, c];
        for value in &mut wide {
            *value = self.widen_half(*value, types::F64)?;
        }
        let [a, b, c] = wide;
        let value = self.builder.ins().fma(a, b, c);
        self.narrow_to_half(value)
    }

    /// `powi`: compiler-builtins multiplies, an `f16` as an `f32`.
    pub(super) fn soft_powi(&mut self, base: Value, power: Value) -> Result<Value> {
        if self.builder.func.dfg.value_type(base) == types::F128 {
            let params = [types::F128, types::I32];
            return self.builtin("__powitf2", &params, types::F128, &[base, power]);
        }
        let wide = self.widen_half(base, types::F32)?;
        let params = [types::F32, types::I32];
        let value = self.builtin("__powisf2", &params, types::F32, &[wide, power])?;
        self.narrow_to_half(value)
    }

    /// Rounds the `f16b` parameter at `addr` as x86-64's LLVM does on entry: it receives a
    /// `bfloat` as the `f32` of its bits and narrows that by compiler-builtins'
    /// `__truncsfbf2`, which keeps every value but quiets a signalling NaN.
    pub(super) fn receive_bfloat(&mut self, addr: Value) -> Result<()> {
        let value = self.builder.ins().load(types::F16, MEM, addr, 0);
        let bits = self.bits_of(value);
        let wide = self.builder.ins().uextend(types::I32, bits);
        let wide = self.builder.ins().ishl_imm(wide, 16);
        let single = self.builder.ins().bitcast(types::F32, MemFlagsData::new(), wide);
        let rounded = self.builtin("__truncsfbf2", &[types::F32], types::F16, &[single])?;
        self.builder.ins().store(MEM, rounded, addr, 0);
        Ok(())
    }

    /// Refuses an x86-64 body in which LLVM would round an `f16b` where this compilation
    /// cannot tell. Between its blocks LLVM holds a `bfloat` value as the `f32` it widens to,
    /// so one kept in a register is narrowed by `__truncsfbf2` (quieting a signalling NaN)
    /// wherever it is read in another block than the one that made it; in memory, or read in
    /// its own block, its bits only move. The parameters are rounded on entry
    /// (`receive_bfloat`); every other local holding an `f16b` must be mentioned in one block
    /// only, counting a block as part of the one that alone jumps to it or calls with it as
    /// the return and nothing to unwind to, which rustc emits as one.
    pub(super) fn check_bfloat_blocks(&self, body: &MirBody<'db>) -> Result<()> {
        if !self.x86() {
            return Ok(());
        }
        let bfloat = |scalar: &rustc_abi::Scalar| {
            matches!(scalar.primitive(), Primitive::Float(rustc_abi::Float::F16B))
        };
        let mut held = HashSet::new_in(std::alloc::Global);
        for (local, data) in body.locals.iter() {
            // A local without a layout is refused where it is compiled, for that.
            let Ok(layout) = self.layout(data.ty.as_ref()) else {
                continue;
            };
            let (carries, alone) = match &layout.backend_repr {
                BackendRepr::Scalar(it) => (bfloat(it), true),
                BackendRepr::ScalarPair { a, b, .. } => (bfloat(a) || bfloat(b), false),
                BackendRepr::SimdVector { element, .. } => (bfloat(element), false),
                _ => (false, false),
            };
            if !carries {
                continue;
            }
            if !body.param_locals.contains(&local) {
                held.insert(local);
            } else if !alone {
                return Err(CodegenError::Unsupported(
                    "f16b in a parameter of two values or lanes on x86-64".into(),
                ));
            }
        }
        if held.is_empty() {
            return Ok(());
        }
        let mut predecessors: HashMap<BasicBlockId, usize, std::alloc::Global> =
            HashMap::new_in(std::alloc::Global);
        for (_, block) in body.basic_blocks.iter() {
            if let Some(terminator) = &block.terminator {
                for to in terminator.kind.successors().into_iter().chain(terminator.kind.unwind()) {
                    *predecessors.entry(to).or_default() += 1;
                }
            }
        }
        let mut follows: HashMap<BasicBlockId, BasicBlockId, std::alloc::Global> =
            HashMap::new_in(std::alloc::Global);
        for (id, block) in body.basic_blocks.iter() {
            let next = match block.terminator.as_ref().map(|it| &it.kind) {
                Some(TerminatorKind::Goto { target })
                | Some(TerminatorKind::Call { target: Some(target), cleanup: None, .. }) => *target,
                _ => continue,
            };
            if next != id && predecessors.get(&next) == Some(&1) {
                follows.insert(next, id);
            }
        }
        let first = |mut id: BasicBlockId| {
            for _ in 0..follows.len() {
                match follows.get(&id) {
                    Some(&before) => id = before,
                    None => break,
                }
            }
            id
        };
        let mut made_in: HashMap<LocalId, BasicBlockId, std::alloc::Global> =
            HashMap::new_in(std::alloc::Global);
        // alloc: only an x86-64 body with an `f16b` local; one list reused across its blocks
        let mut mentioned = Vec::new();
        for (id, block) in body.basic_blocks.iter() {
            mentioned.clear();
            for statement in &block.statements {
                MirBody::visit_statement_places(&statement.kind, &mut |it| {
                    mentioned.push(it.local)
                });
            }
            if let Some(terminator) = &block.terminator {
                MirBody::visit_terminator_places(&terminator.kind, &mut |it| {
                    mentioned.push(it.local)
                });
                if let TerminatorKind::Return = terminator.kind {
                    mentioned.push(returned_local(body));
                }
            }
            let run = first(id);
            for &local in mentioned.iter().filter(|it| held.contains(*it)) {
                if *made_in.entry(local).or_insert(run) != run {
                    return Err(CodegenError::Unsupported(
                        "f16b held across blocks on x86-64, which LLVM rounds there by \
                         __truncsfbf2 where rustc's blocks are"
                            .into(),
                    ));
                }
            }
        }
        Ok(())
    }

    /// The `f16` or `f128` of all zero bits.
    pub(super) fn soft_zero(&mut self, ty: Type) -> Value {
        let int = if ty == types::F16 { types::I16 } else { types::I128 };
        let bits = self.iconst(int, 0);
        self.from_bits(bits, ty)
    }
}

#[cfg(test)]
mod tests {
    use super::{quad_bits, quad_floor};

    #[test]
    fn quad_bounds() {
        assert_eq!(quad_bits(false, 1), 0x3fff << 112);
        assert_eq!(quad_bits(true, 1 << 31), (0xc01e << 112));
        assert_eq!(quad_bits(false, 3), (0x4000 << 112) | (1 << 111));
        assert_eq!(quad_floor(u128::MAX), u128::MAX - ((1 << 15) - 1));
        assert_eq!(quad_floor(u64::MAX.into()), u64::MAX.into());
        // 2^128 - 2^15: exponent 127, all 112 fraction bits set.
        assert_eq!(quad_bits(false, quad_floor(u128::MAX)), (0x407e << 112) | ((1 << 112) - 1));
    }
}
