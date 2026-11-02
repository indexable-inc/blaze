//! Vectors of 128 bits, held in `v128` locals, and the instructions of the `simd128` and
//! relaxed-simd proposals that the vector instructions of Cranelift's IR are lowered to.
//!
//! Each opcode on a vector type is the one wasm instruction that does the same where there is
//! one, whatever the vector's lanes are in the IR. Where Cranelift has an opcode wasm has no
//! instruction for (`imul` of bytes, unsigned comparisons and `min`/`max` of 64-bit lanes,
//! `iadd_pairwise`), it is a few vector instructions. Where wasm has an instruction Cranelift
//! has no opcode for, code generation calls a name of [`INSTRUCTION`] whose index is the
//! [`Instruction`], and that call is the instruction.

use std::fmt;

use cranelift_codegen::ir::{
    Opcode, Type, Value,
    condcodes::{FloatCC, IntCC},
    types,
};
use wasm_encoder::{Instruction as I, MemArg, MemArgTarget};

use super::{Class, Lower, MEMORY, mem};

/// The namespace of the names that stand for a wasm instruction rather than a function: a call
/// of one is lowered to the instruction, its arguments its operands in order. Nothing else
/// declares names in it, and the linker never sees one.
pub const INSTRUCTION: u32 = 0x7761_736d;

macro_rules! instructions {
    ($($name:ident => $instruction:expr,)*) => {
        /// A wasm instruction that Cranelift has no opcode for, as the index of a name of
        /// [`INSTRUCTION`].
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        #[repr(u32)]
        pub enum Instruction {
            $($name,)*
        }

        impl Instruction {
            const ALL: &[Instruction] = &[$(Instruction::$name,)*];

            fn of(index: u32) -> Option<Instruction> {
                Self::ALL.get(index as usize).copied()
            }

            fn encoded(self) -> I<'static> {
                match self {
                    $(Instruction::$name => $instruction,)*
                }
            }
        }
    };
}

/// What an atomic instruction says of the address it is given: its natural alignment, which
/// the format requires it to say.
const fn atomic(align: u32) -> MemArg {
    MemArg { offset: 0, align, target: MemArgTarget::Memory(MEMORY), ordering: None }
}

instructions! {
    I16x8ExtaddPairwiseI8x16S => I::I16x8ExtAddPairwiseI8x16S,
    I16x8ExtaddPairwiseI8x16U => I::I16x8ExtAddPairwiseI8x16U,
    I32x4ExtaddPairwiseI16x8S => I::I32x4ExtAddPairwiseI16x8S,
    I32x4ExtaddPairwiseI16x8U => I::I32x4ExtAddPairwiseI16x8U,
    I32x4DotI16x8S => I::I32x4DotI16x8S,
    F64x2ConvertLowI32x4S => I::F64x2ConvertLowI32x4S,
    F64x2ConvertLowI32x4U => I::F64x2ConvertLowI32x4U,
    I32x4TruncSatF64x2SZero => I::I32x4TruncSatF64x2SZero,
    I32x4TruncSatF64x2UZero => I::I32x4TruncSatF64x2UZero,
    I8x16RelaxedSwizzle => I::I8x16RelaxedSwizzle,
    I32x4RelaxedTruncF32x4S => I::I32x4RelaxedTruncF32x4S,
    I32x4RelaxedTruncF32x4U => I::I32x4RelaxedTruncF32x4U,
    I32x4RelaxedTruncF64x2SZero => I::I32x4RelaxedTruncF64x2SZero,
    I32x4RelaxedTruncF64x2UZero => I::I32x4RelaxedTruncF64x2UZero,
    F32x4RelaxedMadd => I::F32x4RelaxedMadd,
    F32x4RelaxedNmadd => I::F32x4RelaxedNmadd,
    F64x2RelaxedMadd => I::F64x2RelaxedMadd,
    F64x2RelaxedNmadd => I::F64x2RelaxedNmadd,
    I8x16RelaxedLaneselect => I::I8x16RelaxedLaneselect,
    I16x8RelaxedLaneselect => I::I16x8RelaxedLaneselect,
    I32x4RelaxedLaneselect => I::I32x4RelaxedLaneselect,
    I64x2RelaxedLaneselect => I::I64x2RelaxedLaneselect,
    F32x4RelaxedMin => I::F32x4RelaxedMin,
    F32x4RelaxedMax => I::F32x4RelaxedMax,
    F64x2RelaxedMin => I::F64x2RelaxedMin,
    F64x2RelaxedMax => I::F64x2RelaxedMax,
    I16x8RelaxedQ15mulrS => I::I16x8RelaxedQ15mulrS,
    I16x8RelaxedDotI8x16I7x16S => I::I16x8RelaxedDotI8x16I7x16S,
    I32x4RelaxedDotI8x16I7x16AddS => I::I32x4RelaxedDotI8x16I7x16AddS,
    MemorySize => I::MemorySize(MEMORY),
    MemoryGrow => I::MemoryGrow(MEMORY),
    MemoryAtomicNotify => I::MemoryAtomicNotify(atomic(2)),
    MemoryAtomicWait32 => I::MemoryAtomicWait32(atomic(2)),
    MemoryAtomicWait64 => I::MemoryAtomicWait64(atomic(3)),
}

impl Instruction {
    /// The name code generation calls for it.
    pub fn index(self) -> u32 {
        self as u32
    }
}

/// The lanes of a vector type, by how many bits each is and whether they are floats.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Lanes {
    I8,
    I16,
    I32,
    I64,
    F32,
    F64,
}

impl Lanes {
    fn of(ty: Type) -> Result<Lanes, String> {
        Ok(match ty.lane_type() {
            types::I8 if ty.lane_count() == 16 => Lanes::I8,
            types::I16 if ty.lane_count() == 8 => Lanes::I16,
            types::I32 if ty.lane_count() == 4 => Lanes::I32,
            types::I64 if ty.lane_count() == 2 => Lanes::I64,
            types::F32 if ty.lane_count() == 4 => Lanes::F32,
            types::F64 if ty.lane_count() == 2 => Lanes::F64,
            // alloc: the error's message
            _ => return Err(format!("wasm: vector of {ty}")),
        })
    }

    fn bytes(self) -> u8 {
        match self {
            Lanes::I8 => 1,
            Lanes::I16 => 2,
            Lanes::I32 | Lanes::F32 => 4,
            Lanes::I64 | Lanes::F64 => 8,
        }
    }

    /// The instruction of these lanes among those given for each, in the order of the
    /// variants.
    fn pick(
        self,
        mut of: [Option<I<'static>>; 6],
        what: impl fmt::Display,
    ) -> Result<I<'static>, String> {
        let at = match self {
            Lanes::I8 => 0,
            Lanes::I16 => 1,
            Lanes::I32 => 2,
            Lanes::I64 => 3,
            Lanes::F32 => 4,
            Lanes::F64 => 5,
        };
        // alloc: the error's message, built only on the error
        of[at].take().ok_or_else(|| format!("wasm: {what} of {}", self.name()))
    }

    fn name(self) -> &'static str {
        match self {
            Lanes::I8 => "i8x16",
            Lanes::I16 => "i16x8",
            Lanes::I32 => "i32x4",
            Lanes::I64 => "i64x2",
            Lanes::F32 => "f32x4",
            Lanes::F64 => "f64x2",
        }
    }
}

/// The bytes `i8x16.shuffle` picks for each lane of `bytes` bytes to be lane `2 i + odd` of
/// the two vectors one after the other: the even or the odd lanes of both.
fn pairs(bytes: u8, odd: bool) -> [u8; 16] {
    let mut picked = [0; 16];
    for (at, byte) in picked.iter_mut().enumerate() {
        let (lane, within) = (at as u8 / bytes, at as u8 % bytes);
        *byte = (2 * lane + u8::from(odd)) * bytes + within;
    }
    picked
}

/// Every lane of 64 bits with only its top bit set: what an unsigned comparison of them flips
/// to compare them signed.
const TOP_BITS: i128 = i128::from_le_bytes([0, 0, 0, 0, 0, 0, 0, 0x80, 0, 0, 0, 0, 0, 0, 0, 0x80]);

/// The sign bit of each lane of an `f32x4`.
const SIGNS_OF_F32: i128 =
    i128::from_le_bytes([0, 0, 0, 0x80, 0, 0, 0, 0x80, 0, 0, 0, 0x80, 0, 0, 0, 0x80]);

impl Lower<'_> {
    fn lanes(&self, value: Value) -> Result<Lanes, String> {
        Lanes::of(self.ty(value))
    }

    /// Lowers a call of a name of [`INSTRUCTION`].
    pub(super) fn instruction(
        &mut self,
        index: u32,
        args: &[Value],
        results: &[Value],
    ) -> Result<(), String> {
        let instruction =
            // alloc: the error's message, built only on the error
            Instruction::of(index).ok_or_else(|| format!("wasm: no instruction {index}"))?;
        for &arg in args {
            self.get(arg)?;
        }
        self.i(instruction.encoded());
        match results {
            [] => Ok(()),
            &[result] => self.set(result),
            // alloc: the error's message
            _ => Err(format!("wasm: {instruction:?} has one result")),
        }
    }

    /// A load of 64 bits whose lanes are extended to twice their width.
    pub(super) fn load_extend(
        &mut self,
        opcode: Opcode,
        result: Value,
        base: Value,
        offset: i64,
    ) -> Result<(), String> {
        let offset = self.pointer(base, offset)?;
        let at = mem(offset);
        self.i(match opcode {
            Opcode::Uload8x8 => I::V128Load8x8U(at),
            Opcode::Sload8x8 => I::V128Load8x8S(at),
            Opcode::Uload16x4 => I::V128Load16x4U(at),
            Opcode::Sload16x4 => I::V128Load16x4S(at),
            Opcode::Uload32x2 => I::V128Load32x2U(at),
            _ => I::V128Load32x2S(at),
        });
        self.set(result)
    }

    pub(super) fn extract_lane(
        &mut self,
        result: Value,
        arg: Value,
        lane: u8,
    ) -> Result<(), String> {
        let lanes = self.lanes(arg)?;
        self.get(arg)?;
        // A narrow integer is held zero-extended.
        self.i(match lanes {
            Lanes::I8 => I::I8x16ExtractLaneU(lane),
            Lanes::I16 => I::I16x8ExtractLaneU(lane),
            Lanes::I32 => I::I32x4ExtractLane(lane),
            Lanes::I64 => I::I64x2ExtractLane(lane),
            Lanes::F32 => I::F32x4ExtractLane(lane),
            Lanes::F64 => I::F64x2ExtractLane(lane),
        });
        self.set(result)
    }

    pub(super) fn insert_lane(
        &mut self,
        result: Value,
        vector: Value,
        value: Value,
        lane: u8,
    ) -> Result<(), String> {
        let lanes = self.lanes(vector)?;
        self.get(vector)?;
        self.get(value)?;
        self.replace_lane(lanes, lane);
        self.set(result)
    }

    fn replace_lane(&mut self, lanes: Lanes, lane: u8) {
        self.i(match lanes {
            Lanes::I8 => I::I8x16ReplaceLane(lane),
            Lanes::I16 => I::I16x8ReplaceLane(lane),
            Lanes::I32 => I::I32x4ReplaceLane(lane),
            Lanes::I64 => I::I64x2ReplaceLane(lane),
            Lanes::F32 => I::F32x4ReplaceLane(lane),
            Lanes::F64 => I::F64x2ReplaceLane(lane),
        });
    }

    /// An instruction with a vector or a scalar operand and a vector or a scalar result, one of
    /// them a vector.
    pub(super) fn unary_vector(
        &mut self,
        opcode: Opcode,
        result: Value,
        arg: Value,
    ) -> Result<(), String> {
        let to = self.ty(result);
        if opcode == Opcode::Splat {
            let lanes = Lanes::of(to)?;
            self.get(arg)?;
            self.i(match lanes {
                Lanes::I8 => I::I8x16Splat,
                Lanes::I16 => I::I16x8Splat,
                Lanes::I32 => I::I32x4Splat,
                Lanes::I64 => I::I64x2Splat,
                Lanes::F32 => I::F32x4Splat,
                Lanes::F64 => I::F64x2Splat,
            });
            return self.set(result);
        }
        if opcode == Opcode::ScalarToVector {
            self.i(I::V128Const(0));
            self.get(arg)?;
            self.replace_lane(Lanes::of(to)?, 0);
            return self.set(result);
        }
        let from = self.lanes(arg)?;
        let into = || Lanes::of(to);
        let what = opcode;
        self.get(arg)?;
        let instruction = match opcode {
            Opcode::Bnot => I::V128Not,
            Opcode::Ineg => from.pick(
                [
                    Some(I::I8x16Neg),
                    Some(I::I16x8Neg),
                    Some(I::I32x4Neg),
                    Some(I::I64x2Neg),
                    None,
                    None,
                ],
                &what,
            )?,
            Opcode::Iabs => from.pick(
                [
                    Some(I::I8x16Abs),
                    Some(I::I16x8Abs),
                    Some(I::I32x4Abs),
                    Some(I::I64x2Abs),
                    None,
                    None,
                ],
                &what,
            )?,
            Opcode::Popcnt => {
                from.pick([Some(I::I8x16Popcnt), None, None, None, None, None], &what)?
            }
            Opcode::Fneg
            | Opcode::Fabs
            | Opcode::Sqrt
            | Opcode::Ceil
            | Opcode::Floor
            | Opcode::Trunc
            | Opcode::Nearest => {
                let (single, double) = match opcode {
                    Opcode::Fneg => (I::F32x4Neg, I::F64x2Neg),
                    Opcode::Fabs => (I::F32x4Abs, I::F64x2Abs),
                    Opcode::Sqrt => (I::F32x4Sqrt, I::F64x2Sqrt),
                    Opcode::Ceil => (I::F32x4Ceil, I::F64x2Ceil),
                    Opcode::Floor => (I::F32x4Floor, I::F64x2Floor),
                    Opcode::Trunc => (I::F32x4Trunc, I::F64x2Trunc),
                    _ => (I::F32x4Nearest, I::F64x2Nearest),
                };
                from.pick([None, None, None, None, Some(single), Some(double)], &what)?
            }
            Opcode::SwidenLow | Opcode::SwidenHigh | Opcode::UwidenLow | Opcode::UwidenHigh => {
                let of = match opcode {
                    Opcode::SwidenLow => {
                        [I::I16x8ExtendLowI8x16S, I::I32x4ExtendLowI16x8S, I::I64x2ExtendLowI32x4S]
                    }
                    Opcode::SwidenHigh => [
                        I::I16x8ExtendHighI8x16S,
                        I::I32x4ExtendHighI16x8S,
                        I::I64x2ExtendHighI32x4S,
                    ],
                    Opcode::UwidenLow => {
                        [I::I16x8ExtendLowI8x16U, I::I32x4ExtendLowI16x8U, I::I64x2ExtendLowI32x4U]
                    }
                    _ => [
                        I::I16x8ExtendHighI8x16U,
                        I::I32x4ExtendHighI16x8U,
                        I::I64x2ExtendHighI32x4U,
                    ],
                };
                let [a, b, c] = of.map(Some);
                from.pick([a, b, c, None, None, None], &what)?
            }
            Opcode::FcvtFromSint | Opcode::FcvtFromUint
                if (from, into()?) == (Lanes::I32, Lanes::F32) =>
            {
                if opcode == Opcode::FcvtFromSint {
                    I::F32x4ConvertI32x4S
                } else {
                    I::F32x4ConvertI32x4U
                }
            }
            Opcode::FcvtToSintSat | Opcode::FcvtToUintSat
                if (from, into()?) == (Lanes::F32, Lanes::I32) =>
            {
                if opcode == Opcode::FcvtToSintSat {
                    I::I32x4TruncSatF32x4S
                } else {
                    I::I32x4TruncSatF32x4U
                }
            }
            Opcode::FvpromoteLow if from == Lanes::F32 => I::F64x2PromoteLowF32x4,
            Opcode::Fvdemote if from == Lanes::F64 => I::F32x4DemoteF64x2Zero,
            Opcode::VanyTrue => I::V128AnyTrue,
            Opcode::VallTrue => from.pick(
                [
                    Some(I::I8x16AllTrue),
                    Some(I::I16x8AllTrue),
                    Some(I::I32x4AllTrue),
                    Some(I::I64x2AllTrue),
                    Some(I::I32x4AllTrue),
                    Some(I::I64x2AllTrue),
                ],
                &what,
            )?,
            Opcode::VhighBits => {
                let bitmask = from.pick(
                    [
                        Some(I::I8x16Bitmask),
                        Some(I::I16x8Bitmask),
                        Some(I::I32x4Bitmask),
                        Some(I::I64x2Bitmask),
                        Some(I::I32x4Bitmask),
                        Some(I::I64x2Bitmask),
                    ],
                    &what,
                )?;
                self.i(bitmask);
                match self.class(result)? {
                    Class::Long => self.i(I::I64ExtendI32U),
                    class @ Class::Word(_) => self.mask_for(class),
                    // alloc: the error's message
                    class => return Err(format!("wasm: vhigh_bits to {class:?}")),
                }
                return self.set(result);
            }
            // alloc: the error's message
            _ => return Err(format!("wasm: {opcode} of {} to {to}", from.name())),
        };
        self.i(instruction);
        self.set(result)
    }

    /// An instruction whose first operand is a vector.
    pub(super) fn binary_vector(
        &mut self,
        opcode: Opcode,
        result: Value,
        a: Value,
        b: Value,
    ) -> Result<(), String> {
        let lanes = self.lanes(a)?;
        let what = opcode;
        let ints = |i8: Option<I<'static>>, i16, i32, i64| [i8, i16, i32, i64, None, None];
        let floats = |f32, f64| [None, None, None, None, Some(f32), Some(f64)];
        match opcode {
            Opcode::Ishl | Opcode::Ushr | Opcode::Sshr => {
                // The amount is a scalar, taken modulo the width of a lane as Cranelift does.
                self.get(a)?;
                self.amount(b, false)?;
                let of = match opcode {
                    Opcode::Ishl => ints(
                        Some(I::I8x16Shl),
                        Some(I::I16x8Shl),
                        Some(I::I32x4Shl),
                        Some(I::I64x2Shl),
                    ),
                    Opcode::Ushr => ints(
                        Some(I::I8x16ShrU),
                        Some(I::I16x8ShrU),
                        Some(I::I32x4ShrU),
                        Some(I::I64x2ShrU),
                    ),
                    _ => ints(
                        Some(I::I8x16ShrS),
                        Some(I::I16x8ShrS),
                        Some(I::I32x4ShrS),
                        Some(I::I64x2ShrS),
                    ),
                };
                self.i(lanes.pick(of, &what)?);
                return self.set(result);
            }
            Opcode::Imul if lanes == Lanes::I8 => {
                // The low byte of each product of the bytes widened, both halves.
                self.get(a)?;
                self.get(b)?;
                self.i(I::I16x8ExtMulLowI8x16U);
                self.get(a)?;
                self.get(b)?;
                self.i(I::I16x8ExtMulHighI8x16U);
                self.i(I::I8x16Shuffle(pairs(1, false)));
                return self.set(result);
            }
            Opcode::Umin | Opcode::Umax | Opcode::Smin | Opcode::Smax if lanes == Lanes::I64 => {
                let cond = match opcode {
                    Opcode::Umin => IntCC::UnsignedLessThan,
                    Opcode::Umax => IntCC::UnsignedGreaterThan,
                    Opcode::Smin => IntCC::SignedLessThan,
                    _ => IntCC::SignedGreaterThan,
                };
                self.get(a)?;
                self.get(b)?;
                self.wide_lanes_compare(cond, a, b)?;
                self.i(I::V128Bitselect);
                return self.set(result);
            }
            Opcode::IaddPairwise => {
                // The even lanes of both added to the odd ones.
                let bytes = lanes.bytes();
                for odd in [false, true] {
                    self.get(a)?;
                    self.get(b)?;
                    self.i(I::I8x16Shuffle(pairs(bytes, odd)));
                }
                let add = ints(
                    Some(I::I8x16Add),
                    Some(I::I16x8Add),
                    Some(I::I32x4Add),
                    Some(I::I64x2Add),
                );
                self.i(lanes.pick(add, &what)?);
                return self.set(result);
            }
            Opcode::Fcopysign => {
                let sign = match lanes {
                    Lanes::F32 => SIGNS_OF_F32,
                    Lanes::F64 => TOP_BITS,
                    // alloc: the error's message
                    _ => return Err(format!("wasm: {what} of {}", lanes.name())),
                };
                self.get(b)?;
                self.get(a)?;
                self.i(I::V128Const(sign));
                self.i(I::V128Bitselect);
                return self.set(result);
            }
            _ => {}
        }
        let instruction = match opcode {
            Opcode::Band => I::V128And,
            Opcode::Bor => I::V128Or,
            Opcode::Bxor => I::V128Xor,
            Opcode::Iadd => lanes.pick(
                ints(Some(I::I8x16Add), Some(I::I16x8Add), Some(I::I32x4Add), Some(I::I64x2Add)),
                &what,
            )?,
            Opcode::Isub => lanes.pick(
                ints(Some(I::I8x16Sub), Some(I::I16x8Sub), Some(I::I32x4Sub), Some(I::I64x2Sub)),
                &what,
            )?,
            Opcode::Imul => lanes
                .pick(ints(None, Some(I::I16x8Mul), Some(I::I32x4Mul), Some(I::I64x2Mul)), &what)?,
            Opcode::SaddSat => {
                lanes.pick(ints(Some(I::I8x16AddSatS), Some(I::I16x8AddSatS), None, None), &what)?
            }
            Opcode::UaddSat => {
                lanes.pick(ints(Some(I::I8x16AddSatU), Some(I::I16x8AddSatU), None, None), &what)?
            }
            Opcode::SsubSat => {
                lanes.pick(ints(Some(I::I8x16SubSatS), Some(I::I16x8SubSatS), None, None), &what)?
            }
            Opcode::UsubSat => {
                lanes.pick(ints(Some(I::I8x16SubSatU), Some(I::I16x8SubSatU), None, None), &what)?
            }
            Opcode::Smin => lanes.pick(
                ints(Some(I::I8x16MinS), Some(I::I16x8MinS), Some(I::I32x4MinS), None),
                &what,
            )?,
            Opcode::Smax => lanes.pick(
                ints(Some(I::I8x16MaxS), Some(I::I16x8MaxS), Some(I::I32x4MaxS), None),
                &what,
            )?,
            Opcode::Umin => lanes.pick(
                ints(Some(I::I8x16MinU), Some(I::I16x8MinU), Some(I::I32x4MinU), None),
                &what,
            )?,
            Opcode::Umax => lanes.pick(
                ints(Some(I::I8x16MaxU), Some(I::I16x8MaxU), Some(I::I32x4MaxU), None),
                &what,
            )?,
            Opcode::AvgRound => {
                lanes.pick(ints(Some(I::I8x16AvgrU), Some(I::I16x8AvgrU), None, None), &what)?
            }
            Opcode::SqmulRoundSat => {
                lanes.pick(ints(None, Some(I::I16x8Q15MulrSatS), None, None), &what)?
            }
            Opcode::Snarrow => lanes.pick(
                ints(None, Some(I::I8x16NarrowI16x8S), Some(I::I16x8NarrowI32x4S), None),
                &what,
            )?,
            // The operands are signed and the result is unsigned, as wasm's `narrow_u`.
            Opcode::Unarrow => lanes.pick(
                ints(None, Some(I::I8x16NarrowI16x8U), Some(I::I16x8NarrowI32x4U), None),
                &what,
            )?,
            Opcode::Swizzle => lanes.pick(ints(Some(I::I8x16Swizzle), None, None, None), &what)?,
            Opcode::Fadd => lanes.pick(floats(I::F32x4Add, I::F64x2Add), &what)?,
            Opcode::Fsub => lanes.pick(floats(I::F32x4Sub, I::F64x2Sub), &what)?,
            Opcode::Fmul => lanes.pick(floats(I::F32x4Mul, I::F64x2Mul), &what)?,
            Opcode::Fdiv => lanes.pick(floats(I::F32x4Div, I::F64x2Div), &what)?,
            // Both pass a NaN on and order -0 below +0, as Cranelift's `fmin` and `fmax` do.
            Opcode::Fmin => lanes.pick(floats(I::F32x4Min, I::F64x2Min), &what)?,
            Opcode::Fmax => lanes.pick(floats(I::F32x4Max, I::F64x2Max), &what)?,
            // alloc: the error's message
            _ => return Err(format!("wasm: {what} of {}", lanes.name())),
        };
        self.get(a)?;
        self.get(b)?;
        self.i(instruction);
        self.set(result)
    }

    /// Pushes the comparison of two vectors of 64-bit integers: all ones in the lanes where it
    /// holds. Wasm compares them signed only; flipping the top bit of both orders them unsigned.
    fn wide_lanes_compare(&mut self, cond: IntCC, a: Value, b: Value) -> Result<(), String> {
        let (signed, flip) = match cond {
            IntCC::Equal => (I::I64x2Eq, false),
            IntCC::NotEqual => (I::I64x2Ne, false),
            IntCC::SignedLessThan => (I::I64x2LtS, false),
            IntCC::SignedGreaterThan => (I::I64x2GtS, false),
            IntCC::SignedLessThanOrEqual => (I::I64x2LeS, false),
            IntCC::SignedGreaterThanOrEqual => (I::I64x2GeS, false),
            IntCC::UnsignedLessThan => (I::I64x2LtS, true),
            IntCC::UnsignedGreaterThan => (I::I64x2GtS, true),
            IntCC::UnsignedLessThanOrEqual => (I::I64x2LeS, true),
            IntCC::UnsignedGreaterThanOrEqual => (I::I64x2GeS, true),
        };
        for value in [a, b] {
            self.get(value)?;
            if flip {
                self.i(I::V128Const(TOP_BITS));
                self.i(I::V128Xor);
            }
        }
        self.i(signed);
        Ok(())
    }

    pub(super) fn vector_compare(
        &mut self,
        result: Value,
        cond: IntCC,
        a: Value,
        b: Value,
    ) -> Result<(), String> {
        let lanes = self.lanes(a)?;
        if lanes == Lanes::I64 {
            self.wide_lanes_compare(cond, a, b)?;
            return self.set(result);
        }
        // The instructions of `i8x16`, `i16x8` and `i32x4`.
        let of = |i8: I<'static>, i16, i32| [Some(i8), Some(i16), Some(i32), None, None, None];
        let instructions = match cond {
            IntCC::Equal => of(I::I8x16Eq, I::I16x8Eq, I::I32x4Eq),
            IntCC::NotEqual => of(I::I8x16Ne, I::I16x8Ne, I::I32x4Ne),
            IntCC::SignedLessThan => of(I::I8x16LtS, I::I16x8LtS, I::I32x4LtS),
            IntCC::SignedGreaterThan => of(I::I8x16GtS, I::I16x8GtS, I::I32x4GtS),
            IntCC::SignedLessThanOrEqual => of(I::I8x16LeS, I::I16x8LeS, I::I32x4LeS),
            IntCC::SignedGreaterThanOrEqual => of(I::I8x16GeS, I::I16x8GeS, I::I32x4GeS),
            IntCC::UnsignedLessThan => of(I::I8x16LtU, I::I16x8LtU, I::I32x4LtU),
            IntCC::UnsignedGreaterThan => of(I::I8x16GtU, I::I16x8GtU, I::I32x4GtU),
            IntCC::UnsignedLessThanOrEqual => of(I::I8x16LeU, I::I16x8LeU, I::I32x4LeU),
            IntCC::UnsignedGreaterThanOrEqual => of(I::I8x16GeU, I::I16x8GeU, I::I32x4GeU),
        };
        let instruction = lanes.pick(instructions, format_args!("icmp {cond}"))?;
        self.get(a)?;
        self.get(b)?;
        self.i(instruction);
        self.set(result)
    }

    pub(super) fn vector_float_compare(
        &mut self,
        result: Value,
        cond: FloatCC,
        a: Value,
        b: Value,
    ) -> Result<(), String> {
        let single = match self.lanes(a)? {
            Lanes::F32 => true,
            Lanes::F64 => false,
            // alloc: the error's message
            lanes => return Err(format!("wasm: fcmp of {}", lanes.name())),
        };
        let pick = |f32: I<'static>, f64: I<'static>| if single { f32 } else { f64 };
        let eq = pick(I::F32x4Eq, I::F64x2Eq);
        let lt = pick(I::F32x4Lt, I::F64x2Lt);
        let gt = pick(I::F32x4Gt, I::F64x2Gt);
        let le = pick(I::F32x4Le, I::F64x2Le);
        let ge = pick(I::F32x4Ge, I::F64x2Ge);
        // Each is one comparison or the opposite of one, as for scalars: an unordered pair
        // fails every comparison but `ne`.
        let (first, negate) = match cond {
            FloatCC::Equal => (eq, false),
            FloatCC::NotEqual => (pick(I::F32x4Ne, I::F64x2Ne), false),
            FloatCC::LessThan => (lt, false),
            FloatCC::LessThanOrEqual => (le, false),
            FloatCC::GreaterThan => (gt, false),
            FloatCC::GreaterThanOrEqual => (ge, false),
            FloatCC::UnorderedOrLessThan => (ge, true),
            FloatCC::UnorderedOrLessThanOrEqual => (gt, true),
            FloatCC::UnorderedOrGreaterThan => (le, true),
            FloatCC::UnorderedOrGreaterThanOrEqual => (lt, true),
            FloatCC::Ordered | FloatCC::Unordered => {
                for value in [a, b] {
                    self.get(value)?;
                    self.get(value)?;
                    self.i(pick(I::F32x4Eq, I::F64x2Eq));
                }
                self.i(I::V128And);
                if cond == FloatCC::Unordered {
                    self.i(I::V128Not);
                }
                return self.set(result);
            }
            FloatCC::OrderedNotEqual | FloatCC::UnorderedOrEqual => {
                for instruction in [lt, gt] {
                    self.get(a)?;
                    self.get(b)?;
                    self.i(instruction);
                }
                self.i(I::V128Or);
                if cond == FloatCC::UnorderedOrEqual {
                    self.i(I::V128Not);
                }
                return self.set(result);
            }
        };
        self.get(a)?;
        self.get(b)?;
        self.i(first);
        if negate {
            self.i(I::V128Not);
        }
        self.set(result)
    }
}

#[cfg(test)]
mod tests {
    use super::pairs;

    #[test]
    fn pairs_pick_the_even_and_the_odd_lanes_of_both() {
        assert_eq!(pairs(1, false), [0, 2, 4, 6, 8, 10, 12, 14, 16, 18, 20, 22, 24, 26, 28, 30]);
        assert_eq!(pairs(2, true), [2, 3, 6, 7, 10, 11, 14, 15, 18, 19, 22, 23, 26, 27, 30, 31]);
        assert_eq!(pairs(4, false), [0, 1, 2, 3, 8, 9, 10, 11, 16, 17, 18, 19, 24, 25, 26, 27]);
    }
}
