//! Cranelift's instructions on vectors as LLVM IR: each one by what Cranelift defines it to do
//! in every lane (`cranelift-codegen-meta`'s `shared/instructions.rs`), written with LLVM's
//! generic vector operations, which LLVM selects as the one instruction where the target has
//! it (`uaddlp`, `sqxtn`, `urhadd`, `sqrdmulh`, `tbl`, `packsswb`, `pavgb`).
//!
//! The front end makes these for `core::arch` intrinsics that a vector instruction is (NEON's
//! `llvm.aarch64.neon.*` in `neon.rs`, SSE's in `x86.rs`) and for the generic `simd_*`
//! intrinsics (`wasm_simd.rs`), on every target; the stencils take the same IR. So LLVM lowers
//! every vector instruction the stencils do, and one it has no case for is an error naming
//! its opcode, never another path.

use cranelift_codegen::ir::{Inst, InstructionData, Opcode, Type};
use inkwell::{
    types::{BasicTypeEnum, IntType, VectorType},
    values::{BasicMetadataValueEnum, BasicValue, BasicValueEnum, IntValue, VectorValue},
};
use smallvec::SmallVec;

use super::{Lowered, Lowering, Operand, Unhandled};

/// How a vector of lanes twice as wide is brought into the lanes of the result.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Narrow {
    /// `snarrow`: signed lanes, saturated to the signed range of the half.
    Signed,
    /// `unarrow`: signed lanes, saturated to the unsigned range of the half.
    SignedToUnsigned,
    /// `uunarrow`: unsigned lanes, saturated to the unsigned range of the half.
    Unsigned,
}

impl<'ctx> Lowering<'ctx, '_> {
    /// `inst` if it is one of Cranelift's instructions on vectors that the scalar lowering has
    /// no case for: true when it is lowered here, false to leave it to [`Lowering::inst`].
    pub(super) fn vector(&mut self, inst: Inst) -> Lowered<bool> {
        let dfg = &self.func.dfg;
        let data = dfg.insts[inst];
        let opcode = data.opcode();
        let value = match data {
            InstructionData::UnaryConst { constant_handle, .. } => {
                let bytes = dfg.constants.get(constant_handle).as_slice();
                let byte = self.context.i8_type();
                let lanes = bytes
                    .iter()
                    .map(|it| byte.const_int(u64::from(*it), false))
                    .collect::<SmallVec<[IntValue<'ctx>; 16]>>();
                let ty = self.ty(self.result(inst))?;
                let bytes = VectorType::const_vector(lanes.as_slice());
                self.builder.build_bit_cast(bytes, ty, "")?.into_vector_value()
            }
            InstructionData::Unary { arg, .. } => match opcode {
                Opcode::ScalarToVector => {
                    let ty = self.vector_ty(self.result(inst))?;
                    let zero = self.context.i32_type().const_zero();
                    self.builder.build_insert_element(
                        ty.const_zero(),
                        self.plain(arg)?,
                        zero,
                        "",
                    )?
                }
                _ => {
                    let Operand::Vector(x) = self.operand(arg) else { return Ok(false) };
                    match self.vector_unary(inst, opcode, x)? {
                        Some(it) => it,
                        None => return Ok(false),
                    }
                }
            },
            InstructionData::Binary { args: [l, r], .. } => {
                let (Operand::Vector(x), Operand::Vector(y)) = (self.operand(l), self.operand(r))
                else {
                    return Ok(false);
                };
                match self.vector_binary(inst, opcode, x, y)? {
                    Some(it) => it,
                    None => return Ok(false),
                }
            }
            InstructionData::Ternary { opcode: Opcode::Bitselect, args: [c, x, y] } => {
                let ints = self.vector_ty(dfg.value_type(x).as_int())?;
                let (Operand::Vector(c), Operand::Vector(x), Operand::Vector(y)) =
                    (self.operand(c), self.operand(x), self.operand(y))
                else {
                    return Ok(false);
                };
                self.bitselect(ints, c, x, y)?
            }
            // A lane that holds is all ones, as in Cranelift: the result's lanes are integers
            // of the compared lanes' width.
            InstructionData::FloatCompare { args: [l, r], cond, .. } => {
                let (Operand::Vector(x), Operand::Vector(y)) = (self.operand(l), self.operand(r))
                else {
                    return Ok(false);
                };
                let holds =
                    self.builder.build_float_compare(Self::float_predicate(cond), x, y, "")?;
                let ty = self.vector_ty(self.result(inst))?;
                self.builder.build_int_s_extend(holds, ty, "")?
            }
            // `uload8x8` and the like: 8 bytes of lanes half as wide, each extended.
            InstructionData::Load { arg, offset, flags, .. } => {
                let signed = match opcode {
                    Opcode::Uload8x8 | Opcode::Uload16x4 | Opcode::Uload32x2 => false,
                    Opcode::Sload8x8 | Opcode::Sload16x4 | Opcode::Sload32x2 => true,
                    _ => return Ok(false),
                };
                let result = self.result(inst);
                let narrow = Self::halved(result)?;
                let address = self.address(arg, offset.into())?;
                let loaded = self.builder.build_load(self.ty(narrow)?, address, "")?;
                Self::set_access_alignment(
                    loaded.as_instruction_value().expect("a load"),
                    dfg.mem_flags[flags].aligned(),
                )?;
                let ty = self.vector_ty(result)?;
                self.extend(loaded.into_vector_value(), ty, signed)?
            }
            _ => return Ok(false),
        };
        self.set(inst, Operand::Vector(value));
        Ok(true)
    }

    fn vector_unary(
        &self,
        inst: Inst,
        opcode: Opcode,
        x: VectorValue<'ctx>,
    ) -> Lowered<Option<VectorValue<'ctx>>> {
        let b = &self.builder;
        let lanes = x.get_type().get_size();
        let each = |name: &str| self.each(name, &[x.into()]);
        let no = self.context.bool_type().const_zero();
        Ok(Some(match opcode {
            // Wrapping: the absolute value of the least integer is itself.
            Opcode::Iabs => self.each("llvm.abs", &[x.into(), no.into()])?,
            Opcode::Popcnt => each("llvm.ctpop")?,
            // A lane of zero has as many zero bits as it has bits.
            Opcode::Clz => self.each("llvm.ctlz", &[x.into(), no.into()])?,
            Opcode::Ctz => self.each("llvm.cttz", &[x.into(), no.into()])?,
            Opcode::Fneg => b.build_float_neg(x, "")?,
            Opcode::Fabs => each("llvm.fabs")?,
            Opcode::Sqrt => each("llvm.sqrt")?,
            Opcode::Ceil => each("llvm.ceil")?,
            Opcode::Floor => each("llvm.floor")?,
            Opcode::Trunc => each("llvm.trunc")?,
            // Ties go to the even neighbour.
            Opcode::Nearest => each("llvm.roundeven")?,
            Opcode::SwidenLow | Opcode::SwidenHigh | Opcode::UwidenLow | Opcode::UwidenHigh => {
                let high = matches!(opcode, Opcode::SwidenHigh | Opcode::UwidenHigh);
                let signed = matches!(opcode, Opcode::SwidenLow | Opcode::SwidenHigh);
                let half =
                    self.lanes(x, (0..lanes / 2).map(|it| it + u32::from(high) * lanes / 2))?;
                self.extend(half, self.vector_ty(self.result(inst))?, signed)?
            }
            Opcode::FcvtFromSint => {
                b.build_signed_int_to_float(x, self.vector_ty(self.result(inst))?, "")?
            }
            Opcode::FcvtFromUint => {
                b.build_unsigned_int_to_float(x, self.vector_ty(self.result(inst))?, "")?
            }
            // A NaN is zero, and what is past the range is its nearest end.
            Opcode::FcvtToSintSat | Opcode::FcvtToUintSat => {
                let name = match opcode {
                    Opcode::FcvtToSintSat => "llvm.fptosi.sat",
                    _ => "llvm.fptoui.sat",
                };
                let ty = self.vector_ty(self.result(inst))?;
                self.intrinsic(name, &[ty.into(), x.get_type().into()], &[x.into()])?
                    .into_vector_value()
            }
            Opcode::Fpromote | Opcode::Fdemote => {
                b.build_float_cast(x, self.vector_ty(self.result(inst))?, "")?
            }
            // The low two lanes, each made a `f64`.
            Opcode::FvpromoteLow => {
                let low = self.lanes(x, 0..2)?;
                b.build_float_ext(low, self.vector_ty(self.result(inst))?, "")?
            }
            // Each lane rounded to the nearest `f32`, ties to even, and two lanes of zero
            // above them.
            Opcode::Fvdemote => {
                let f32s = self.context.f32_type().vec_type(lanes);
                let narrowed = b.build_float_trunc(x, f32s, "")?;
                let mask = self.mask(0..2 * lanes);
                b.build_shuffle_vector(narrowed, f32s.const_zero(), mask, "")?
            }
            _ => return Ok(None),
        }))
    }

    fn vector_binary(
        &self,
        inst: Inst,
        opcode: Opcode,
        x: VectorValue<'ctx>,
        y: VectorValue<'ctx>,
    ) -> Lowered<Option<VectorValue<'ctx>>> {
        let b = &self.builder;
        let each = |name: &str| self.each(name, &[x.into(), y.into()]);
        Ok(Some(match opcode {
            Opcode::Fadd => b.build_float_add(x, y, "")?,
            Opcode::Fsub => b.build_float_sub(x, y, "")?,
            Opcode::Fmul => b.build_float_mul(x, y, "")?,
            Opcode::Fdiv => b.build_float_div(x, y, "")?,
            // A NaN in either is a NaN, and -0 is less than +0.
            Opcode::Fmin => each("llvm.minimum")?,
            Opcode::Fmax => each("llvm.maximum")?,
            Opcode::Fcopysign => each("llvm.copysign")?,
            // `(x + y + 1) / 2` of unsigned lanes, the sum taken without losing its carry.
            Opcode::AvgRound => {
                let (ty, wide) = self.widened(x)?;
                let (x, y) =
                    (b.build_int_z_extend(x, wide, "")?, b.build_int_z_extend(y, wide, "")?);
                let one = self.splat_int(wide, 1);
                let sum = b.build_int_add(b.build_int_add(x, y, "")?, one, "")?;
                b.build_int_truncate(b.build_right_shift(sum, one, false, "")?, ty, "")?
            }
            // `(x * y + (1 << (Q - 1))) >> Q` of signed fixed-point lanes of `Q + 1` bits,
            // saturated: only the least times itself is past the range.
            Opcode::SqmulRoundSat => {
                let (ty, wide) = self.widened(x)?;
                let bits = u64::from(ty.get_element_type().into_int_type().get_bit_width());
                let (x, y) =
                    (b.build_int_s_extend(x, wide, "")?, b.build_int_s_extend(y, wide, "")?);
                let product = b.build_int_add(
                    b.build_int_mul(x, y, "")?,
                    self.splat_int(wide, 1 << (bits - 2)),
                    "",
                )?;
                let shifted =
                    b.build_right_shift(product, self.splat_int(wide, bits - 1), true, "")?;
                let (most, least) = ((1u64 << (bits - 1)) - 1, u64::MAX << (bits - 1));
                let clamped =
                    self.each("llvm.smin", &[shifted.into(), self.splat_int(wide, most).into()])?;
                let clamped =
                    self.each("llvm.smax", &[clamped.into(), self.splat_int(wide, least).into()])?;
                b.build_int_truncate(clamped, ty, "")?
            }
            Opcode::Snarrow => self.narrow(inst, x, y, Narrow::Signed)?,
            Opcode::Unarrow => self.narrow(inst, x, y, Narrow::SignedToUnsigned)?,
            Opcode::Uunarrow => self.narrow(inst, x, y, Narrow::Unsigned)?,
            Opcode::Swizzle => self.swizzle(x, y)?,
            // The high half of each lane's product.
            Opcode::Umulhi | Opcode::Smulhi => {
                let signed = opcode == Opcode::Smulhi;
                let (ty, wide) = self.widened(x)?;
                let bits = u64::from(ty.get_element_type().into_int_type().get_bit_width());
                let x = b.build_int_cast_sign_flag(x, wide, signed, "")?;
                let y = b.build_int_cast_sign_flag(y, wide, signed, "")?;
                let product = b.build_int_mul(x, y, "")?;
                b.build_int_truncate(
                    b.build_right_shift(product, self.splat_int(wide, bits), false, "")?,
                    ty,
                    "",
                )?
            }
            _ => return Ok(None),
        }))
    }

    /// `snarrow`, `unarrow` and `uunarrow`: the lanes of `x` then those of `y`, each brought
    /// into the range of a lane half as wide and then cut to it.
    fn narrow(
        &self,
        inst: Inst,
        x: VectorValue<'ctx>,
        y: VectorValue<'ctx>,
        how: Narrow,
    ) -> Lowered<VectorValue<'ctx>> {
        let b = &self.builder;
        let lanes = x.get_type().get_size();
        let both = b.build_shuffle_vector(x, y, self.mask(0..2 * lanes), "")?;
        let ty = both.get_type();
        let half = self.vector_ty(self.result(inst))?;
        let bits = half.get_element_type().into_int_type().get_bit_width();
        let unsigned_most = (1u64 << bits) - 1;
        let clamp = |name: &str, value: VectorValue<'ctx>, bound: u64| {
            self.each(name, &[value.into(), self.splat_int(ty, bound).into()])
        };
        let clamped = match how {
            Narrow::Signed => {
                let most = clamp("llvm.smin", both, (1u64 << (bits - 1)) - 1)?;
                clamp("llvm.smax", most, u64::MAX << (bits - 1))?
            }
            Narrow::SignedToUnsigned => {
                let least = clamp("llvm.smax", both, 0)?;
                clamp("llvm.smin", least, unsigned_most)?
            }
            Narrow::Unsigned => clamp("llvm.umin", both, unsigned_most)?,
        };
        Ok(b.build_int_truncate(clamped, half, "")?)
    }

    /// `swizzle`: each byte the byte of `x` its index in `indices` names, or zero for an index
    /// past the 16 bytes. That is AArch64's `tbl` of one register; elsewhere each byte is
    /// picked by its index and the bytes past the end cleared, which LLVM selects as the
    /// target's byte shuffle where it has one.
    fn swizzle(
        &self,
        x: VectorValue<'ctx>,
        indices: VectorValue<'ctx>,
    ) -> Lowered<VectorValue<'ctx>> {
        let b = &self.builder;
        let ty = x.get_type();
        if self.target.arch == crate::target::Arch::Aarch64 {
            let args = [x.into(), indices.into()];
            return Ok(self
                .intrinsic("llvm.aarch64.neon.tbl1", &[ty.into()], &args)?
                .into_vector_value());
        }
        let lanes = ty.get_size();
        let lane = self.context.i32_type();
        let within = b.build_and(indices, self.splat_int(ty, u64::from(lanes) - 1), "")?;
        let mut picked = ty.get_poison();
        for at in 0..lanes {
            let at = lane.const_int(u64::from(at), false);
            let index = b.build_extract_element(within, at, "")?.into_int_value();
            let byte = b.build_extract_element(x, index, "")?;
            picked = b.build_insert_element(picked, byte, at, "")?;
        }
        let inside = b.build_int_compare(
            inkwell::IntPredicate::ULT,
            indices,
            self.splat_int(ty, u64::from(lanes)),
            "",
        )?;
        Ok(b.build_select(inside, picked, ty.const_zero(), "")?.into_vector_value())
    }

    /// `bitselect` of vectors: each bit of `x` where `c`'s is set, else of `y`. A vector of
    /// floats is taken as `ints`, the integers of its bits.
    fn bitselect(
        &self,
        ints: VectorType<'ctx>,
        c: VectorValue<'ctx>,
        x: VectorValue<'ctx>,
        y: VectorValue<'ctx>,
    ) -> Lowered<VectorValue<'ctx>> {
        let b = &self.builder;
        let ty = x.get_type();
        let bits = |it: VectorValue<'ctx>| -> Lowered<VectorValue<'ctx>> {
            Ok(b.build_bit_cast(it, ints, "")?.into_vector_value())
        };
        let (c, x, y) = (bits(c)?, bits(x)?, bits(y)?);
        let then = b.build_and(x, c, "")?;
        let otherwise = b.build_and(y, b.build_not(c, "")?, "")?;
        let selected = b.build_or(then, otherwise, "")?;
        Ok(b.build_bit_cast(selected, ty, "")?.into_vector_value())
    }

    /// The type of `inst`'s first result.
    fn result(&self, inst: Inst) -> Type {
        self.func.dfg.value_type(self.func.dfg.first_result(inst))
    }

    fn vector_ty(&self, ty: Type) -> Lowered<VectorType<'ctx>> {
        match self.ty(ty)? {
            BasicTypeEnum::VectorType(it) => Ok(it),
            _ => Err(Unhandled::Type(ty)),
        }
    }

    /// The vector of `ty`'s lanes, each half as wide: what a widening load reads.
    fn halved(ty: Type) -> Lowered<Type> {
        ty.lane_type()
            .half_width()
            .and_then(|lane| lane.by(ty.lane_count()))
            .ok_or(Unhandled::Type(ty))
    }

    /// `x`'s type, and the vector of as many lanes twice as wide.
    fn widened(&self, x: VectorValue<'ctx>) -> Lowered<(VectorType<'ctx>, VectorType<'ctx>)> {
        let ty = x.get_type();
        let BasicTypeEnum::IntType(lane) = ty.get_element_type() else {
            return Err(Unhandled::Shape("a vector of floats where Cranelift has integers"));
        };
        let wide: IntType<'ctx> = Self::int_of_width(self.context, lane.get_bit_width() * 2)?;
        Ok((ty, wide.vec_type(ty.get_size())))
    }

    fn extend(
        &self,
        x: VectorValue<'ctx>,
        to: VectorType<'ctx>,
        signed: bool,
    ) -> Lowered<VectorValue<'ctx>> {
        Ok(match signed {
            true => self.builder.build_int_s_extend(x, to, "")?,
            false => self.builder.build_int_z_extend(x, to, "")?,
        })
    }

    /// The lanes of `x` that `lanes` names, in that order.
    fn lanes(
        &self,
        x: VectorValue<'ctx>,
        lanes: impl Iterator<Item = u32>,
    ) -> Lowered<VectorValue<'ctx>> {
        let poison = x.get_type().get_poison();
        Ok(self.builder.build_shuffle_vector(x, poison, self.mask(lanes), "")?)
    }

    /// A `shufflevector` mask of `lanes`.
    fn mask(&self, lanes: impl Iterator<Item = u32>) -> VectorValue<'ctx> {
        let index = self.context.i32_type();
        let lanes = lanes
            .map(|it| index.const_int(u64::from(it), false))
            .collect::<SmallVec<[IntValue<'ctx>; 16]>>();
        VectorType::const_vector(lanes.as_slice())
    }

    /// `value` in every lane of `ty`, a vector of integers, as a constant.
    fn splat_int(&self, ty: VectorType<'ctx>, value: u64) -> VectorValue<'ctx> {
        let lane = ty.get_element_type().into_int_type().const_int(value, false);
        let lanes = std::iter::repeat_n(lane, ty.get_size() as usize)
            .collect::<SmallVec<[IntValue<'ctx>; 16]>>();
        VectorType::const_vector(lanes.as_slice())
    }

    /// LLVM's intrinsic `name` of the type of the first of `args`, a vector, lane by lane.
    fn each(
        &self,
        name: &str,
        args: &[BasicMetadataValueEnum<'ctx>],
    ) -> Lowered<VectorValue<'ctx>> {
        let BasicMetadataValueEnum::VectorValue(first) = args[0] else {
            return Err(Unhandled::Shape("a vector intrinsic of no vector"));
        };
        let value: BasicValueEnum<'ctx> = self.intrinsic(name, &[first.get_type().into()], args)?;
        Ok(value.into_vector_value())
    }
}
