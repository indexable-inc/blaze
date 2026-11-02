//! 128-bit vectors for the generic `simd_*` intrinsics, on every target, and WebAssembly's
//! `simd128` and relaxed-simd features.
//!
//! A SIMD value lives in memory as everywhere else. A vector operation on lanes that fill whole
//! vectors of 16 bytes loads each 16 bytes as a Cranelift vector, does the one operation on it,
//! and stores it, where the target has the instruction ([`Vectors`]): NEON on AArch64, SSE2 on
//! x86-64, and on wasm where the function may use `simd128` (its crate is built with the
//! feature, or the function enables it with `#[target_feature]`), which [`super::wasm`] lowers
//! to a `v128.load`, the instruction of the operation, and a `v128.store`. So do the
//! `llvm.wasm.*` intrinsics `core::arch::wasm32` declares, and the vector `llvm.*` ones it
//! declares for wasm. What has no vector instruction here (a shift by a vector of amounts that
//! is not one amount, a lane cast wasm has no instruction for, on x86-64 whatever needs a
//! feature past SSE2) is done a lane at a time, as on a wasm function without the feature,
//! where no vector type is made at all. The two give the same bits: each operation here is
//! exact on integers, and on floats the one IEEE operation LLVM selects for it.
//!
//! An instruction Cranelift has no opcode for is a call of its name in
//! [`super::wasm::INSTRUCTION`]. A relaxed instruction is emitted as itself, so it behaves as
//! the engine that runs it chooses, the way rustc's does.

#![forbid(unsafe_code)]

use hir_ty::db::HirDatabase;
use cranelift_codegen::ir::{
    self, AbiParam, Endianness, InstBuilder, MemFlagsData, Type, Value,
    condcodes::{FloatCC, IntCC},
    types,
};
use cranelift_frontend::FunctionBuilder;
use hir_def::{
    CallableDefId,
    layout::{BackendRepr, Primitive},
};
use hir_ty::{
    TargetFeature, TargetFeatures, crate_target_features,
    intrinsic::{Intrinsic, SimdOp},
    mir::{LocalId, Operand, OperandKind, Rvalue, StatementKind, TerminatorKind},
    next_solver::{ConstKind, Ty, TyKind},
};
use rustc_type_ir::inherent::IntoKind;
use smallvec::SmallVec;

use super::wasm::{self, Instruction};
use super::{AbiScalar, Arch, CodegenError, FunctionCx, MEM, Result, return_slot};

/// The vector instructions a function's `simd_*` intrinsics may be given in place of a lane
/// at a time.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Vectors {
    /// wasm's `simd128`.
    Wasm,
    /// AArch64's NEON, which every AArch64 target has.
    Neon,
    /// x86-64's SSE2 alone: Cranelift's ISA is made with no later feature
    /// ([`Target::isa`](super::Target)), and lowers a vector operation of one (`pshufb` of a
    /// `shuffle` or a byte `splat`, `roundps`, `pminud`, `pcmpgtq`) to a libcall there.
    Sse2,
}

/// A generic `simd_*` intrinsic this file lowers, named once: 1 byte, a `match` per use.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
enum Op {
    Add,
    Sub,
    Mul,
    Div,
    And,
    Or,
    Xor,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    SatAdd,
    SatSub,
    Shl,
    Shr,
    Neg,
    Fabs,
    Sqrt,
    Floor,
    Ceil,
    Trunc,
    RoundEven,
    Ctpop,
    Select,
    Splat,
    Shuffle,
    Cast,
    Bitmask,
    ReduceAdd,
    ReduceMax,
    ReduceMin,
    ReduceAnd,
    ReduceOr,
    ReduceXor,
    ReduceAny,
    ReduceAll,
}

const _: () = assert!(size_of::<Op>() == 1);

impl Op {
    /// The op of the `simd_*` intrinsic `op`, if this file lowers it: a jump table on a byte.
    fn of(op: SimdOp) -> Option<Op> {
        Some(match op {
            SimdOp::Add => Op::Add,
            SimdOp::Sub => Op::Sub,
            SimdOp::Mul => Op::Mul,
            SimdOp::Div => Op::Div,
            SimdOp::And => Op::And,
            SimdOp::Or => Op::Or,
            SimdOp::Xor => Op::Xor,
            SimdOp::Eq => Op::Eq,
            SimdOp::Ne => Op::Ne,
            SimdOp::Lt => Op::Lt,
            SimdOp::Le => Op::Le,
            SimdOp::Gt => Op::Gt,
            SimdOp::Ge => Op::Ge,
            SimdOp::SaturatingAdd => Op::SatAdd,
            SimdOp::SaturatingSub => Op::SatSub,
            SimdOp::Shl => Op::Shl,
            SimdOp::Shr => Op::Shr,
            SimdOp::Neg => Op::Neg,
            SimdOp::Fabs => Op::Fabs,
            SimdOp::Fsqrt => Op::Sqrt,
            SimdOp::Floor => Op::Floor,
            SimdOp::Ceil => Op::Ceil,
            SimdOp::Trunc => Op::Trunc,
            SimdOp::RoundTiesEven => Op::RoundEven,
            SimdOp::Ctpop => Op::Ctpop,
            SimdOp::Select => Op::Select,
            SimdOp::Splat => Op::Splat,
            SimdOp::Shuffle => Op::Shuffle,
            SimdOp::Cast | SimdOp::As => Op::Cast,
            SimdOp::Bitmask => Op::Bitmask,
            SimdOp::ReduceAddUnordered => Op::ReduceAdd,
            SimdOp::ReduceMax => Op::ReduceMax,
            SimdOp::ReduceMin => Op::ReduceMin,
            SimdOp::ReduceAnd => Op::ReduceAnd,
            SimdOp::ReduceOr => Op::ReduceOr,
            SimdOp::ReduceXor => Op::ReduceXor,
            SimdOp::ReduceAny => Op::ReduceAny,
            SimdOp::ReduceAll => Op::ReduceAll,
            SimdOp::ArithOffset
            | SimdOp::Bitreverse
            | SimdOp::Bswap
            | SimdOp::CarrylessMul
            | SimdOp::CastPtr
            | SimdOp::Ctlz
            | SimdOp::Cttz
            | SimdOp::ExposeProvenance
            | SimdOp::Extract
            | SimdOp::ExtractDyn
            | SimdOp::Fcos
            | SimdOp::Fexp
            | SimdOp::Fexp2
            | SimdOp::Flog
            | SimdOp::Flog10
            | SimdOp::Flog2
            | SimdOp::Fma
            | SimdOp::Fsin
            | SimdOp::FunnelShl
            | SimdOp::FunnelShr
            | SimdOp::Gather
            | SimdOp::Insert
            | SimdOp::InsertDyn
            | SimdOp::MaskedLoad
            | SimdOp::MaskedStore
            | SimdOp::MaximumNumberNsz
            | SimdOp::MinimumNumberNsz
            | SimdOp::ReduceAddOrdered
            | SimdOp::ReduceMulOrdered
            | SimdOp::ReduceMulUnordered
            | SimdOp::RelaxedFma
            | SimdOp::Rem
            | SimdOp::Round
            | SimdOp::Scatter
            | SimdOp::SelectBitmask
            | SimdOp::WithExposedProvenance => return None,
        })
    }

    fn is_binary(self) -> bool {
        (self as u8) <= (Op::Shr as u8)
    }

    fn is_unary(self) -> bool {
        (Op::Neg as u8..=Op::Ctpop as u8).contains(&(self as u8))
    }

    fn is_reduce(self) -> bool {
        (self as u8) >= (Op::ReduceAdd as u8)
    }
}

/// Whether `vectors` has the instruction for `op` on lanes of `lane` (of integers `signed`),
/// one Cranelift lowers with no call.
fn admits(vectors: Vectors, op: Op, lane: Type, signed: bool) -> bool {
    let float = lane.is_float();
    let bits = lane.bits();
    let lowers = match op {
        Op::Div => float,
        Op::And | Op::Or | Op::Xor | Op::Shl | Op::Shr => !float,
        Op::SatAdd | Op::SatSub => !float && (bits <= 16 || vectors == Vectors::Neon),
        Op::Ctpop => lane == types::I8,
        Op::Neg => true,
        Op::Fabs | Op::Sqrt | Op::Floor | Op::Ceil | Op::Trunc | Op::RoundEven => float,
        Op::Bitmask => !float,
        _ if op.is_reduce() => !float,
        _ => true,
    };
    lowers
        && match vectors {
            Vectors::Wasm => !op.is_reduce(),
            // NEON multiplies lanes of at most 32 bits; LLVM does 64 a lane at a time too.
            Vectors::Neon => op != Op::Mul || float || bits <= 32,
            Vectors::Sse2 => match op {
                Op::Add | Op::Sub | Op::Div | Op::And | Op::Or | Op::Xor | Op::SatAdd => true,
                Op::SatSub | Op::Neg | Op::Fabs | Op::Sqrt | Op::Select | Op::Splat => true,
                Op::Bitmask => true,
                // `pmullw`, and `pmuludq` twice for 32 bits.
                Op::Mul => float || matches!(bits, 16 | 32),
                // `pcmpeqq` and `pcmpgtq` are SSE4's, and SSE2 compares only signed integers.
                Op::Eq | Op::Ne => float || bits <= 32,
                Op::Lt | Op::Gt => float || (signed && bits <= 32),
                Op::Le | Op::Ge => float,
                // No shift of bytes, nor an arithmetic one of 64 bits, before AVX-512.
                Op::Shl => bits >= 16,
                Op::Shr => bits >= 16 && !(signed && bits == 64),
                _ => false,
            },
        }
}

/// What a lane of `bits` bits, zero-extended to 64, is multiplied by to repeat it over 64 bits.
fn repeat_factor(bits: u32) -> Option<i64> {
    match bits {
        8 => Some(0x0101_0101_0101_0101),
        16 => Some(0x0001_0001_0001_0001),
        32 => Some(0x0000_0001_0000_0001),
        _ => None,
    }
}

/// The `shuffle` of a vector with itself that moves each byte down by `by`, the ones below
/// coming round from the top: NEON's `ext` of the vector with itself.
fn rotation(by: u8) -> [u8; 16] {
    std::array::from_fn(|at| at as u8 + by)
}

/// The lanes of a SIMD type whose bytes are a whole number of 16-byte vectors, or 8 bytes.
#[derive(Clone, Copy)]
struct Lanes {
    lane: AbiScalar,
    count: u64,
}

impl Lanes {
    fn bytes(self) -> u64 {
        u64::from(self.lane.ty.bytes()) * self.count
    }

    /// The Cranelift type of 16 bytes of these lanes.
    fn vector(self) -> Type {
        self.lane.ty.by(16 / self.lane.ty.bytes()).expect("a lane of at most 16 bytes")
    }

    /// How many vectors of 16 bytes they are.
    fn chunks(self) -> Option<i32> {
        let bytes = self.bytes();
        (bytes % 16 == 0 && bytes != 0).then(|| (bytes / 16) as i32)
    }
}

/// How a vector of another type is read as these bytes: as they are in memory.
const LITTLE: MemFlagsData = MemFlagsData::new().with_endianness(Endianness::Little);

impl<'db> FunctionCx<'_, 'db> {
    /// Whether the function is compiled with the wasm feature `feature`: by its crate, or by
    /// its own `#[target_feature(enable = ...)]`.
    fn wasm_feature(&self, feature: TargetFeature) -> bool {
        if !self.cg.target.is_wasm() {
            return false;
        }
        if crate_target_features(self.cg.db, self.env.krate).contains(feature) {
            return true;
        }
        self.body.and_then(|body| body.owner.as_function()).is_some_and(|func| {
            TargetFeatures::from_fn_no_implications(self.cg.db, func).enables(feature)
        })
    }

    /// Whether the function may use wasm's `v128`: `relaxed-simd` implies `simd128`, as rustc
    /// has it.
    pub(super) fn simd128(&self) -> bool {
        self.wasm_feature(TargetFeature::SIMD128) || self.wasm_feature(TargetFeature::RELAXED_SIMD)
    }

    /// Whether code generation may make a vector type: always but on wasm without `simd128`.
    pub(super) fn makes_vectors(&self) -> bool {
        !self.cg.target.is_wasm() || self.simd128()
    }

    /// The vector instructions this function's `simd_*` intrinsics may be given.
    fn vectors(&self) -> Option<Vectors> {
        match self.cg.target.arch {
            Arch::Wasm32 => self.simd128().then_some(Vectors::Wasm),
            Arch::Aarch64 => Some(Vectors::Neon),
            Arch::X86_64 => Some(Vectors::Sse2),
        }
    }

    /// The lanes of a vector of integers, `f32`s or `f64`s; `None` for others (halves, which
    /// are done a lane at a time as `f32`s, and pointers).
    fn simd_shape(&self, ty: Ty<'db>) -> Result<Option<Lanes>> {
        let BackendRepr::SimdVector { element, .. } = self.layout(ty)?.backend_repr else {
            return Ok(None);
        };
        let (Primitive::Int(..) | Primitive::Float(rustc_abi::Float::F32 | rustc_abi::Float::F64)) =
            element.primitive()
        else {
            return Ok(None);
        };
        let (lane, count) = self.simd_lanes(ty)?;
        if lane.ty.bits() > 64 || u64::from(lane.ty.bytes()) * count > 64 * 16 {
            return Ok(None);
        }
        Ok(Some(Lanes { lane, count }))
    }

    fn vbitcast(&mut self, ty: Type, value: Value) -> Value {
        if self.builder.func.dfg.value_type(value) == ty {
            return value;
        }
        self.builder.ins().bitcast(ty, LITTLE, value)
    }

    /// Loads the 8 bytes at `addr + offset` as the low half of a vector of type `ty`.
    fn load_half(&mut self, ty: Type, addr: Value, offset: i32) -> Value {
        let low = self.builder.ins().load(types::I64, MEM, addr, offset);
        let vector = self.builder.ins().scalar_to_vector(types::I64X2, low);
        self.vbitcast(ty, vector)
    }

    /// Stores the low 8 bytes of a vector at `addr + offset`.
    fn store_half(&mut self, value: Value, addr: Value, offset: i32) {
        let value = self.vbitcast(types::I64X2, value);
        let low = self.builder.ins().extractlane(value, 0);
        self.builder.ins().store(MEM, low, addr, offset);
    }

    /// Calls the wasm instruction `instruction`: its operands are `args`, and it leaves a value
    /// of `ret` when there is one.
    fn wasm_instruction(
        &mut self,
        instruction: Instruction,
        args: &[Value],
        ret: Option<Type>,
    ) -> Option<Value> {
        let mut sig = self.cg.make_signature();
        for &arg in args {
            sig.params.push(AbiParam::new(self.builder.func.dfg.value_type(arg)));
        }
        sig.returns.extend(ret.map(AbiParam::new));
        let signature = self.builder.import_signature(sig);
        let name = self.builder.func.declare_imported_user_function(ir::UserExternalName {
            namespace: wasm::INSTRUCTION,
            index: instruction.index(),
        });
        let callee = self.builder.import_function(ir::ExtFuncData {
            name: ir::ExternalName::user(name),
            signature,
            colocated: true,
            patchable: false,
        });
        let call = self.builder.ins().call(callee, args);
        self.builder.inst_results(call).first().copied()
    }

    /// The bytes of a constant operand, as `simd_shuffle` is given its indices.
    fn constant_bytes(&mut self, operand: &Operand) -> Result<Option<Vec<u8>>> {
        Ok(match &operand.kind {
            OperandKind::Constant { konst, ty } => match konst.as_ref().kind() {
                ConstKind::Value(value) => {
                    let blob = self.valtree_blob(ty.as_ref(), value.value)?;
                    (blob.relocs.is_empty() && blob.fn_relocs.is_empty()).then_some(blob.bytes)
                }
                // alloc: once per `simd_shuffle`, its indices out of a const evaluated here
                ConstKind::Alias(_, named) => Some(self.eval_const(named)?.memory.to_vec()),
                _ => None,
            },
            // alloc: once per `simd_shuffle`, its indices
            OperandKind::Allocation { allocation } => Some(allocation.as_ref().memory.to_vec()),
            OperandKind::Copy(place) | OperandKind::Move(place)
                if place.projection.as_slice().is_empty() =>
            {
                let Some(from) = self.only_assignment(place.local) else { return Ok(None) };
                self.constant_bytes(from)?
            }
            _ => None,
        })
    }

    /// What a local is set to, when one statement of the body sets all of it and nothing else
    /// does: a const generic argument is moved to a local of its own first.
    fn only_assignment(&self, local: LocalId) -> Option<&'db Operand> {
        let body = self.body?;
        let mut assigned = body.basic_blocks.iter().flat_map(|(_, block)| {
            block.statements.iter().filter_map(move |it| match &it.kind {
                StatementKind::Assign(to, rvalue) if to.local == local => Some((to, rvalue)),
                _ => None,
            })
        });
        let (Some((to, Rvalue::Use(from))), None) = (assigned.next(), assigned.next()) else {
            return None;
        };
        to.projection.as_slice().is_empty().then_some(from)
    }

    /// Whether `operand` is a local that only one call sets, of `simd_splat` or of a function
    /// that only returns what `simd_splat` makes (`core::arch`'s `Simd::splat`): all its lanes
    /// one value, what `core::arch::wasm32` shifts each lane by.
    fn splatted(&mut self, operand: &Operand) -> Result<bool> {
        let (OperandKind::Copy(place) | OperandKind::Move(place)) = &operand.kind else {
            return Ok(false);
        };
        let Some(body) = self.body else { return Ok(false) };
        if !place.projection.as_slice().is_empty() {
            return Ok(false);
        }
        let (mut calls, mut last) = (0u32, None);
        for (_, block) in body.basic_blocks.iter() {
            let assigns = block.statements.iter().any(
                |it| matches!(&it.kind, StatementKind::Assign(to, _) if to.local == place.local),
            );
            if assigns {
                return Ok(false);
            }
            if let Some(TerminatorKind::Call { func, destination, intrinsic, .. }) =
                block.terminator.as_ref().map(|it| &it.kind)
                && destination.local == place.local
            {
                if !destination.projection.as_slice().is_empty() {
                    return Ok(false);
                }
                calls += 1;
                last = Some((self.operand_ty(func)?, *intrinsic));
            }
        }
        let (1, Some((callee, intrinsic))) = (calls, last) else { return Ok(false) };
        if intrinsic == Some(Intrinsic::Simd(SimdOp::Splat)) {
            return Ok(true);
        }
        let TyKind::FnDef(def, _) = callee.kind() else { return Ok(false) };
        let CallableDefId::FunctionId(callee) = def.0 else { return Ok(false) };
        Ok(self.only_splats(callee))
    }

    /// Whether the body of `func` makes its result with one call, of `simd_splat`, and does
    /// nothing else that is a call.
    fn only_splats(&self, func: hir_def::FunctionId) -> bool {
        let Ok(body) = self.cg.db.mir_body(func.into()) else { return false };
        let mut calls = body.basic_blocks.iter().filter_map(|(_, block)| {
            match block.terminator.as_ref().map(|it| &it.kind) {
                Some(TerminatorKind::Call { destination, intrinsic, .. }) => {
                    Some((*intrinsic, destination))
                }
                _ => None,
            }
        });
        let (Some((intrinsic, destination)), None) = (calls.next(), calls.next()) else {
            return false;
        };
        if destination.local != return_slot() || !destination.projection.as_slice().is_empty() {
            return false;
        }
        intrinsic == Some(Intrinsic::Simd(SimdOp::Splat))
    }

    /// A `simd_*` intrinsic as vector instructions, when the target has them ([`Vectors`])
    /// and there is one for it. False when it is to be done a lane at a time.
    pub(super) fn wasm_simd_intrinsic(
        &mut self,
        intrinsic: Intrinsic,
        args: &[Operand],
        dst: Value,
        dst_ty: Ty<'db>,
    ) -> Result<bool> {
        let Intrinsic::Simd(op) = intrinsic else { return Ok(false) };
        let Some(op) = Op::of(op) else { return Ok(false) };
        let Some(vectors) = self.vectors() else { return Ok(false) };
        match (op, args) {
            (_, [l, r]) if op.is_binary() => self.wasm_simd_binary(vectors, op, l, r, dst, dst_ty),
            (_, [arg]) if op.is_unary() => self.wasm_simd_unary(vectors, op, arg, dst, dst_ty),
            (_, [arg]) if op.is_reduce() => self.simd_reduce(vectors, op, arg, dst, dst_ty),
            (Op::Select, [mask, on_true, on_false]) => {
                let mask_ty = self.operand_ty(mask)?;
                let (Some(mask_lanes), Some(lanes)) =
                    (self.simd_shape(mask_ty)?, self.simd_shape(dst_ty)?)
                else {
                    return Ok(false);
                };
                // A lane of a mask is all zeros or all ones, so the bits of the mask pick.
                let Some(chunks) = lanes.chunks() else { return Ok(false) };
                if mask_lanes.lane.ty.bits() != lanes.lane.ty.bits()
                    || !admits(vectors, op, lanes.lane.ty, lanes.lane.signed)
                {
                    return Ok(false);
                }
                let (mask, _) = self.operand(mask)?;
                let (on_true, _) = self.operand(on_true)?;
                let (on_false, _) = self.operand(on_false)?;
                let vector = lanes.vector();
                for chunk in 0..chunks {
                    let at = chunk * 16;
                    let [m, a, b] = [mask, on_true, on_false]
                        .map(|it| self.builder.ins().load(vector, MEM, it, at));
                    let value = self.builder.ins().bitselect(m, a, b);
                    self.builder.ins().store(MEM, value, dst, at);
                }
                Ok(true)
            }
            (Op::Splat, [arg]) => {
                let Some(lanes) = self.simd_shape(dst_ty)? else { return Ok(false) };
                let Some(chunks) = lanes.chunks() else { return Ok(false) };
                let (src, _) = self.operand(arg)?;
                let value = self.builder.ins().load(lanes.lane.ty, MEM, src, 0);
                if vectors == Vectors::Sse2 {
                    // The lane repeated over 64 bits and stored as that: a vector `splat` of
                    // bytes is `pshufb`, SSSE3's.
                    let pattern = self.repeated(value, lanes.lane.ty);
                    for at in (0..chunks * 16).step_by(8) {
                        self.builder.ins().store(MEM, pattern, dst, at);
                    }
                    return Ok(true);
                }
                let vector = self.builder.ins().splat(lanes.vector(), value);
                for chunk in 0..chunks {
                    self.builder.ins().store(MEM, vector, dst, chunk * 16);
                }
                Ok(true)
            }
            (Op::Shuffle, [first, second, indices]) if vectors != Vectors::Sse2 => {
                self.wasm_simd_shuffle(first, second, indices, dst, dst_ty)
            }
            (Op::Cast, [arg]) if vectors != Vectors::Sse2 => {
                self.wasm_simd_cast(vectors, arg, dst, dst_ty)
            }
            (Op::Bitmask, [arg]) => {
                let ty = self.operand_ty(arg)?;
                let Some(lanes) = self.simd_shape(ty)? else { return Ok(false) };
                let Some(chunks) = lanes.chunks() else { return Ok(false) };
                let layout = self.layout(dst_ty)?;
                let BackendRepr::Scalar(_) = layout.backend_repr else {
                    return Ok(false);
                };
                if lanes.count > 64 || !admits(vectors, op, lanes.lane.ty, lanes.lane.signed) {
                    return Ok(false);
                }
                let to = self.scalar_of(dst_ty)?;
                let (src, _) = self.operand(arg)?;
                let vector = lanes.vector();
                // Bit `i` of the result is the top bit of lane `i`.
                let mut bits = self.builder.ins().iconst(types::I64, 0);
                for chunk in 0..chunks {
                    let value = self.builder.ins().load(vector, MEM, src, chunk * 16);
                    let top = self.builder.ins().vhigh_bits(types::I64, value);
                    let shift = i64::from(chunk) * i64::from(vector.lane_count());
                    let top = self.builder.ins().ishl_imm_u(top, shift);
                    bits = self.builder.ins().bor(bits, top);
                }
                let from = AbiScalar { ty: types::I64, signed: false };
                let value = self.int_cast(bits, from, to);
                self.builder.ins().store(MEM, value, dst, 0);
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn wasm_simd_binary(
        &mut self,
        vectors: Vectors,
        op: Op,
        l: &Operand,
        r: &Operand,
        dst: Value,
        dst_ty: Ty<'db>,
    ) -> Result<bool> {
        let ty = self.operand_ty(l)?;
        let (Some(lanes), Some(out)) = (self.simd_shape(ty)?, self.simd_shape(dst_ty)?) else {
            return Ok(false);
        };
        let Some(chunks) = lanes.chunks() else { return Ok(false) };
        if out.count != lanes.count || out.lane.ty.bits() != lanes.lane.ty.bits() {
            return Ok(false);
        }
        let (float, signed) = (lanes.lane.ty.is_float(), lanes.lane.signed);
        let shift = matches!(op, Op::Shl | Op::Shr);
        if !admits(vectors, op, lanes.lane.ty, signed) || (shift && !self.splatted(r)?) {
            return Ok(false);
        }
        let (a, _) = self.operand(l)?;
        let (b, _) = self.operand(r)?;
        let vector = lanes.vector();
        // A shift moves each lane by the one amount all of them have.
        let amount = shift.then(|| self.builder.ins().load(lanes.lane.ty, MEM, b, 0));
        let int_cc = match (op, signed) {
            (Op::Ne, _) => IntCC::NotEqual,
            (Op::Lt, true) => IntCC::SignedLessThan,
            (Op::Lt, false) => IntCC::UnsignedLessThan,
            (Op::Le, true) => IntCC::SignedLessThanOrEqual,
            (Op::Le, false) => IntCC::UnsignedLessThanOrEqual,
            (Op::Gt, true) => IntCC::SignedGreaterThan,
            (Op::Gt, false) => IntCC::UnsignedGreaterThan,
            (Op::Ge, true) => IntCC::SignedGreaterThanOrEqual,
            (Op::Ge, false) => IntCC::UnsignedGreaterThanOrEqual,
            _ => IntCC::Equal,
        };
        let float_cc = match op {
            Op::Ne => FloatCC::NotEqual,
            Op::Lt => FloatCC::LessThan,
            Op::Le => FloatCC::LessThanOrEqual,
            Op::Gt => FloatCC::GreaterThan,
            Op::Ge => FloatCC::GreaterThanOrEqual,
            _ => FloatCC::Equal,
        };
        for chunk in 0..chunks {
            let at = chunk * 16;
            let x = self.builder.ins().load(vector, MEM, a, at);
            let value = match (op, amount) {
                (Op::Shl, Some(amount)) => self.builder.ins().ishl(x, amount),
                (Op::Shr, Some(amount)) if signed => self.builder.ins().sshr(x, amount),
                (Op::Shr, Some(amount)) => self.builder.ins().ushr(x, amount),
                _ => {
                    let y = self.builder.ins().load(vector, MEM, b, at);
                    let ins = self.builder.ins();
                    match op {
                        Op::Add if float => ins.fadd(x, y),
                        Op::Add => ins.iadd(x, y),
                        Op::Sub if float => ins.fsub(x, y),
                        Op::Sub => ins.isub(x, y),
                        Op::Mul if float => ins.fmul(x, y),
                        Op::Mul => ins.imul(x, y),
                        Op::Div => ins.fdiv(x, y),
                        Op::And => ins.band(x, y),
                        Op::Or => ins.bor(x, y),
                        Op::Xor => ins.bxor(x, y),
                        Op::SatAdd if signed => ins.sadd_sat(x, y),
                        Op::SatAdd => ins.uadd_sat(x, y),
                        Op::SatSub if signed => ins.ssub_sat(x, y),
                        Op::SatSub => ins.usub_sat(x, y),
                        // A comparison: all ones in the lanes where it holds.
                        _ if float => ins.fcmp(float_cc, x, y),
                        _ => ins.icmp(int_cc, x, y),
                    }
                }
            };
            self.builder.ins().store(MEM, value, dst, at);
        }
        Ok(true)
    }

    fn wasm_simd_unary(
        &mut self,
        vectors: Vectors,
        op: Op,
        arg: &Operand,
        dst: Value,
        dst_ty: Ty<'db>,
    ) -> Result<bool> {
        let ty = self.operand_ty(arg)?;
        let (Some(lanes), Some(out)) = (self.simd_shape(ty)?, self.simd_shape(dst_ty)?) else {
            return Ok(false);
        };
        let Some(chunks) = lanes.chunks() else { return Ok(false) };
        let float = lanes.lane.ty.is_float();
        if !admits(vectors, op, lanes.lane.ty, lanes.lane.signed)
            || out.lane.ty != lanes.lane.ty
            || out.count != lanes.count
        {
            return Ok(false);
        }
        let (src, _) = self.operand(arg)?;
        let vector = lanes.vector();
        for chunk in 0..chunks {
            let at = chunk * 16;
            let x = self.builder.ins().load(vector, MEM, src, at);
            let ins = self.builder.ins();
            let value = match op {
                Op::Neg if float => ins.fneg(x),
                Op::Neg => ins.ineg(x),
                Op::Fabs => ins.fabs(x),
                Op::Sqrt => ins.sqrt(x),
                Op::Floor => ins.floor(x),
                Op::Ceil => ins.ceil(x),
                Op::Trunc => ins.trunc(x),
                Op::RoundEven => ins.nearest(x),
                _ => ins.popcnt(x),
            };
            self.builder.ins().store(MEM, value, dst, at);
        }
        Ok(true)
    }

    /// `value`, a lane of `lane`, repeated over 64 bits.
    fn repeated(&mut self, value: Value, lane: Type) -> Value {
        let bits = match lane {
            types::F32 => self.builder.ins().bitcast(types::I32, MemFlagsData::new(), value),
            types::F64 => self.builder.ins().bitcast(types::I64, MemFlagsData::new(), value),
            _ => value,
        };
        let Some(factor) = repeat_factor(lane.bits()) else { return bits };
        let wide = self.builder.ins().uextend(types::I64, bits);
        self.builder.ins().imul_imm(wide, factor)
    }

    /// `simd_reduce_*` of integer lanes on NEON: the vectors of the operand combined into one,
    /// then halved until one lane is left, each time combining it with itself moved down by
    /// half its live bytes (an `ext`), so lane 0 holds the whole reduction. `simd_reduce_any`
    /// and `_all` of a mask are whether any or all lanes are set.
    fn simd_reduce(
        &mut self,
        vectors: Vectors,
        op: Op,
        arg: &Operand,
        dst: Value,
        dst_ty: Ty<'db>,
    ) -> Result<bool> {
        let ty = self.operand_ty(arg)?;
        let Some(lanes) = self.simd_shape(ty)? else { return Ok(false) };
        let Some(chunks) = lanes.chunks() else { return Ok(false) };
        if !admits(vectors, op, lanes.lane.ty, lanes.lane.signed) {
            return Ok(false);
        }
        let truth = matches!(op, Op::ReduceAny | Op::ReduceAll);
        let out = u64::from(if truth { 1 } else { lanes.lane.ty.bytes() });
        if self.layout(dst_ty)?.size.bytes() != out {
            return Ok(false);
        }
        let signed = lanes.lane.signed;
        let combine = |b: &mut FunctionBuilder<'_>, x: Value, y: Value| {
            let ins = b.ins();
            match op {
                Op::ReduceAdd => ins.iadd(x, y),
                Op::ReduceMax if signed => ins.smax(x, y),
                Op::ReduceMax => ins.umax(x, y),
                Op::ReduceMin if signed => ins.smin(x, y),
                Op::ReduceMin => ins.umin(x, y),
                Op::ReduceAnd | Op::ReduceAll => ins.band(x, y),
                Op::ReduceOr | Op::ReduceAny => ins.bor(x, y),
                _ => ins.bxor(x, y),
            }
        };
        let (src, _) = self.operand(arg)?;
        let vector = lanes.vector();
        let mut value = self.builder.ins().load(vector, MEM, src, 0);
        for chunk in 1..chunks {
            let next = self.builder.ins().load(vector, MEM, src, chunk * 16);
            value = combine(&mut self.builder, value, next);
        }
        let result = match op {
            Op::ReduceAny => self.builder.ins().vany_true(value),
            Op::ReduceAll => self.builder.ins().vall_true(value),
            _ => {
                let lane = lanes.lane.ty.bytes() as u8;
                let mut by = 8;
                while by >= lane {
                    // alloc: the shuffle's mask, which the function's IR owns
                    let mask = self.builder.func.dfg.immediates.push(rotation(by).to_vec().into());
                    let bytes = self.vbitcast(types::I8X16, value);
                    let moved = self.builder.ins().shuffle(bytes, bytes, mask);
                    let moved = self.vbitcast(vector, moved);
                    value = combine(&mut self.builder, value, moved);
                    by /= 2;
                }
                self.builder.ins().extractlane(value, 0)
            }
        };
        self.builder.ins().store(MEM, result, dst, 0);
        Ok(true)
    }

    /// `simd_shuffle` with constant indices: each 16 bytes of the result one `i8x16.shuffle`
    /// of at most two vectors of the operands.
    fn wasm_simd_shuffle(
        &mut self,
        first: &Operand,
        second: &Operand,
        indices: &Operand,
        dst: Value,
        dst_ty: Ty<'db>,
    ) -> Result<bool> {
        let ty = self.operand_ty(first)?;
        let (Some(lanes), Some(out)) = (self.simd_shape(ty)?, self.simd_shape(dst_ty)?) else {
            return Ok(false);
        };
        if lanes.lane.ty.bytes() != out.lane.ty.bytes() {
            return Ok(false);
        }
        let Some(bytes) = self.constant_bytes(indices)? else { return Ok(false) };
        let count = out.count as usize;
        if bytes.len() < count * 4 {
            return Ok(false);
        }
        let indices = &bytes[..count * 4];
        let index = |at: usize| {
            u64::from(u32::from_le_bytes(indices[at * 4..at * 4 + 4].try_into().expect("four bytes")))
        };
        // The operands in pieces a vector each: 16 bytes, or 8 that are the low half of one.
        let piece = match lanes.bytes() {
            8 => 8,
            bytes if bytes % 16 == 0 => 16,
            _ => return Ok(false),
        };
        let per_operand = lanes.bytes() / piece;
        let (out_bytes, out_piece) = match out.bytes() {
            8 => (8, 8),
            bytes if bytes % 16 == 0 => (bytes, 16),
            _ => return Ok(false),
        };
        let lane = u64::from(lanes.lane.ty.bytes());
        if (0..count).any(|at| index(at) >= 2 * lanes.count) {
            return Ok(false);
        }
        // For each piece of the result, the pieces of the operands it takes bytes of and the
        // byte of those each of its bytes is.
        let mut plans: SmallVec<[(u64, SmallVec<[u64; 4]>, [u8; 16]); 4]> = SmallVec::new();
        for start in (0..out_bytes).step_by(out_piece as usize) {
            let mut sources: SmallVec<[u64; 4]> = SmallVec::new();
            let mut mask = [0u8; 16];
            for (at, byte) in mask.iter_mut().enumerate().take(out_piece as usize) {
                let at = start + at as u64;
                let from = index((at / lane) as usize) * lane + at % lane;
                let source = from / piece;
                let slot = match sources.iter().position(|&it| it == source) {
                    Some(slot) => slot,
                    None => {
                        sources.push(source);
                        sources.len() - 1
                    }
                };
                if slot > 1 {
                    return Ok(false);
                }
                *byte = (slot * 16) as u8 + (from % piece) as u8;
            }
            plans.push((start, sources, mask));
        }
        let (first, _) = self.operand(first)?;
        let (second, _) = self.operand(second)?;
        for (start, sources, mask) in plans {
            let mut loaded: SmallVec<[Value; 4]> = SmallVec::new();
            for &source in &sources {
                let operand = if source < per_operand { first } else { second };
                let at = ((source % per_operand) * piece) as i32;
                loaded.push(if piece == 16 {
                    self.builder.ins().load(types::I8X16, MEM, operand, at)
                } else {
                    self.load_half(types::I8X16, operand, at)
                });
            }
            let (a, b) = (loaded[0], *loaded.get(1).unwrap_or(&loaded[0]));
            // alloc: the shuffle's mask, which the function's IR owns
            let mask = self.builder.func.dfg.immediates.push(mask.to_vec().into());
            let value = self.builder.ins().shuffle(a, b, mask);
            if out_piece == 16 {
                self.builder.ins().store(MEM, value, dst, start as i32);
            } else {
                self.store_half(value, dst, start as i32);
            }
        }
        Ok(true)
    }

    /// `simd_cast` and `simd_as` that one or two wasm instructions do for each vector.
    fn wasm_simd_cast(
        &mut self,
        vectors: Vectors,
        arg: &Operand,
        dst: Value,
        dst_ty: Ty<'db>,
    ) -> Result<bool> {
        let ty = self.operand_ty(arg)?;
        let (Some(from), Some(to)) = (self.simd_shape(ty)?, self.simd_shape(dst_ty)?) else {
            return Ok(false);
        };
        if from.count != to.count {
            return Ok(false);
        }
        let wasm = vectors == Vectors::Wasm;
        let (f, t) = (from.lane, to.lane);
        let (src, _) = self.operand(arg)?;
        let (from_bytes, to_bytes) = (from.bytes(), to.bytes());
        match (f.ty.is_float(), t.ty.is_float()) {
            // The same bits, or a sign that is read another way.
            (false, false) if f.ty == t.ty => {
                let size = self.layout(dst_ty)?.size;
                self.copy(dst, src, size);
            }
            // Twice as wide: each lane extended by the sign it has.
            (false, false) if t.ty.bits() == 2 * f.ty.bits() && from_bytes == 8 => {
                let ins = self.builder.ins();
                let value = match (f.ty, f.signed) {
                    (types::I8, true) => ins.sload8x8(MEM, src, 0),
                    (types::I8, false) => ins.uload8x8(MEM, src, 0),
                    (types::I16, true) => ins.sload16x4(MEM, src, 0),
                    (types::I16, false) => ins.uload16x4(MEM, src, 0),
                    (_, true) => ins.sload32x2(MEM, src, 0),
                    (_, false) => ins.uload32x2(MEM, src, 0),
                };
                self.builder.ins().store(MEM, value, dst, 0);
            }
            (false, false) if t.ty.bits() == 2 * f.ty.bits() && from_bytes % 16 == 0 => {
                let vector = from.vector();
                for chunk in 0..(from_bytes / 16) as i32 {
                    let x = self.builder.ins().load(vector, MEM, src, chunk * 16);
                    let (low, high) = if f.signed {
                        (self.builder.ins().swiden_low(x), self.builder.ins().swiden_high(x))
                    } else {
                        (self.builder.ins().uwiden_low(x), self.builder.ins().uwiden_high(x))
                    };
                    self.builder.ins().store(MEM, low, dst, chunk * 32);
                    self.builder.ins().store(MEM, high, dst, chunk * 32 + 16);
                }
            }
            // Half as wide: the low half of each lane.
            (false, false)
                if 2 * t.ty.bits() == f.ty.bits() && (to_bytes == 8 || to_bytes % 16 == 0) =>
            {
                let (fb, tb) = (f.ty.bytes() as u8, t.ty.bytes() as u8);
                let mut mask = [0u8; 16];
                for (at, byte) in mask.iter_mut().enumerate() {
                    let at = at as u8;
                    *byte = (at / tb) * fb + at % tb;
                }
                // alloc: the shuffle's mask, which the function's IR owns
                let mask = self.builder.func.dfg.immediates.push(mask.to_vec().into());
                if to_bytes == 8 {
                    let x = self.builder.ins().load(types::I8X16, MEM, src, 0);
                    let value = self.builder.ins().shuffle(x, x, mask);
                    self.store_half(value, dst, 0);
                } else {
                    for chunk in 0..(to_bytes / 16) as i32 {
                        let x = self.builder.ins().load(types::I8X16, MEM, src, chunk * 32);
                        let y = self.builder.ins().load(types::I8X16, MEM, src, chunk * 32 + 16);
                        let value = self.builder.ins().shuffle(x, y, mask);
                        self.builder.ins().store(MEM, value, dst, chunk * 16);
                    }
                }
            }
            (false, true) if (f.ty, t.ty) == (types::I32, types::F32) && from_bytes % 16 == 0 => {
                for chunk in 0..(from_bytes / 16) as i32 {
                    let x = self.builder.ins().load(types::I32X4, MEM, src, chunk * 16);
                    let ins = self.builder.ins();
                    let value = if f.signed {
                        ins.fcvt_from_sint(types::F32X4, x)
                    } else {
                        ins.fcvt_from_uint(types::F32X4, x)
                    };
                    self.builder.ins().store(MEM, value, dst, chunk * 16);
                }
            }
            // Each 8 bytes of the operand the low half of a vector, widened to a whole one.
            (false, true)
                if wasm && (f.ty, t.ty) == (types::I32, types::F64) && from_bytes % 8 == 0 =>
            {
                let instruction = if f.signed {
                    Instruction::F64x2ConvertLowI32x4S
                } else {
                    Instruction::F64x2ConvertLowI32x4U
                };
                for half in 0..(from_bytes / 8) as i32 {
                    let x = self.load_half(types::I32X4, src, half * 8);
                    let value = self.wasm_instruction(instruction, &[x], Some(types::F64X2));
                    self.builder.ins().store(MEM, value.expect("a result"), dst, half * 16);
                }
            }
            // Saturated, as `simd_as` is and `simd_cast` may be.
            (true, false) if (f.ty, t.ty) == (types::F32, types::I32) && from_bytes % 16 == 0 => {
                for chunk in 0..(from_bytes / 16) as i32 {
                    let x = self.builder.ins().load(types::F32X4, MEM, src, chunk * 16);
                    let ins = self.builder.ins();
                    let value = if t.signed {
                        ins.fcvt_to_sint_sat(types::I32X4, x)
                    } else {
                        ins.fcvt_to_uint_sat(types::I32X4, x)
                    };
                    self.builder.ins().store(MEM, value, dst, chunk * 16);
                }
            }
            // Each 16 bytes of the operand narrowed to 8 of the result.
            (true, false)
                if wasm && (f.ty, t.ty) == (types::F64, types::I32) && from_bytes % 16 == 0 =>
            {
                let instruction = if t.signed {
                    Instruction::I32x4TruncSatF64x2SZero
                } else {
                    Instruction::I32x4TruncSatF64x2UZero
                };
                for chunk in 0..(from_bytes / 16) as i32 {
                    let x = self.builder.ins().load(types::F64X2, MEM, src, chunk * 16);
                    let value = self.wasm_instruction(instruction, &[x], Some(types::I32X4));
                    self.store_half(value.expect("a result"), dst, chunk * 8);
                }
            }
            (true, true) if (f.ty, t.ty) == (types::F32, types::F64) && from_bytes % 8 == 0 => {
                for half in 0..(from_bytes / 8) as i32 {
                    let x = self.load_half(types::F32X4, src, half * 8);
                    let value = self.builder.ins().fvpromote_low(x);
                    self.builder.ins().store(MEM, value, dst, half * 16);
                }
            }
            (true, true) if (f.ty, t.ty) == (types::F64, types::F32) && from_bytes % 16 == 0 => {
                for chunk in 0..(from_bytes / 16) as i32 {
                    let x = self.builder.ins().load(types::F64X2, MEM, src, chunk * 16);
                    let value = self.builder.ins().fvdemote(x);
                    self.store_half(value, dst, chunk * 8);
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// An intrinsic `core::arch::wasm32` declares as `llvm.{name}`, as the wasm instruction it
    /// stands for. False for one that is not wasm's.
    pub(super) fn wasm_llvm_intrinsic(
        &mut self,
        name: &str,
        args: &[Operand],
        dst: Value,
    ) -> Result<bool> {
        if !self.cg.target.is_wasm() {
            return Ok(false);
        }
        let (scalars, instruction) = match name {
            "wasm.memory.size.i32" => (1, Instruction::MemorySize),
            "wasm.memory.grow.i32" => (1, Instruction::MemoryGrow),
            "wasm.memory.atomic.wait32" => (0, Instruction::MemoryAtomicWait32),
            "wasm.memory.atomic.wait64" => (0, Instruction::MemoryAtomicWait64),
            "wasm.memory.atomic.notify" => (0, Instruction::MemoryAtomicNotify),
            _ => return self.wasm_llvm_vector(name, args, dst),
        };
        // The memory instructions name the first memory, the only one there is.
        let (memory, args) = args.split_at(scalars);
        if let [memory] = memory
            && self.immediate(memory)? != Some(0)
        {
            unsupported!("`llvm.{name}` of a memory other than the first");
        }
        let operands =
            args.iter().map(|it| Ok(self.load_scalar(it)?.0)).collect::<Result<Vec<_>>>()?;
        let value = self.wasm_instruction(instruction, &operands, Some(types::I32));
        self.builder.ins().store(MEM, value.expect("a result"), dst, 0);
        Ok(true)
    }

    fn wasm_llvm_vector(&mut self, name: &str, args: &[Operand], dst: Value) -> Result<bool> {
        let Some((operands, result, how)) = vector_intrinsic(name) else { return Ok(false) };
        if operands.len() != args.len() {
            return Ok(false);
        }
        let mut values = Vec::with_capacity(args.len());
        for (arg, &ty) in args.iter().zip(operands) {
            let (addr, _) = self.operand(arg)?;
            values.push(self.builder.ins().load(ty, MEM, addr, 0));
        }
        let value = match how {
            How::Opcode(emit) => emit(&mut self.builder, &values),
            How::Instruction(instruction) => {
                self.wasm_instruction(instruction, &values, Some(result)).expect("a result")
            }
        };
        self.builder.ins().store(MEM, value, dst, 0);
        Ok(true)
    }
}

/// How a vector intrinsic is computed from its operands.
enum How {
    Opcode(fn(&mut FunctionBuilder<'_>, &[Value]) -> Value),
    Instruction(Instruction),
}

/// What the operands of the vector intrinsic `llvm.{name}` of `core::arch::wasm32` are, what its
/// result is, and how it is computed.
fn vector_intrinsic(name: &str) -> Option<(&'static [Type], Type, How)> {
    use Instruction as W;
    use types::{F32X4, F64X2, I8X16, I16X8, I32, I32X4, I64X2};
    let (stem, suffix) = name.rsplit_once('.')?;
    let lanes = match suffix {
        "v16i8" => I8X16,
        "v8i16" => I16X8,
        "v4i32" => I32X4,
        "v2i64" => I64X2,
        "v4f32" => F32X4,
        "v2f64" => F64X2,
        _ => return untyped_vector_intrinsic(name),
    };
    let one: &'static [Type] = match lanes {
        I8X16 => &[I8X16],
        I16X8 => &[I16X8],
        I32X4 => &[I32X4],
        I64X2 => &[I64X2],
        F32X4 => &[F32X4],
        _ => &[F64X2],
    };
    let two: &'static [Type] = match lanes {
        I8X16 => &[I8X16; 2],
        I16X8 => &[I16X8; 2],
        F32X4 => &[F32X4; 2],
        F64X2 => &[F64X2; 2],
        _ => &[],
    };
    let three: &'static [Type] = match lanes {
        I8X16 => &[I8X16; 3],
        I16X8 => &[I16X8; 3],
        I32X4 => &[I32X4; 3],
        I64X2 => &[I64X2; 3],
        F32X4 => &[F32X4; 3],
        _ => &[F64X2; 3],
    };
    let float = lanes.lane_type().is_float();
    let f32 = lanes == F32X4;
    Some(match stem {
        "wasm.anytrue" if !float => (
            one,
            I32,
            How::Opcode(|b, v| {
                let any = b.ins().vany_true(v[0]);
                b.ins().uextend(I32, any)
            }),
        ),
        "wasm.alltrue" if !float => (
            one,
            I32,
            How::Opcode(|b, v| {
                let all = b.ins().vall_true(v[0]);
                b.ins().uextend(I32, all)
            }),
        ),
        "wasm.bitmask" if !float => (one, I32, How::Opcode(|b, v| b.ins().vhigh_bits(I32, v[0]))),
        "wasm.bitselect" if lanes == I8X16 => {
            (three, lanes, How::Opcode(|b, v| b.ins().bitselect(v[2], v[0], v[1])))
        }
        "wasm.avgr.unsigned" if matches!(lanes, I8X16 | I16X8) => {
            (two, lanes, How::Opcode(|b, v| b.ins().avg_round(v[0], v[1])))
        }
        // Named by the result: each lane the sum of two of the narrower lanes of the operand.
        "wasm.extadd.pairwise.signed" | "wasm.extadd.pairwise.unsigned"
            if matches!(lanes, I16X8 | I32X4) =>
        {
            let signed = stem.ends_with(".signed");
            let (operand, instruction): (&'static [Type], _) = match (lanes, signed) {
                (I16X8, true) => (&[I8X16], W::I16x8ExtaddPairwiseI8x16S),
                (I16X8, false) => (&[I8X16], W::I16x8ExtaddPairwiseI8x16U),
                (_, true) => (&[I16X8], W::I32x4ExtaddPairwiseI16x8S),
                (_, false) => (&[I16X8], W::I32x4ExtaddPairwiseI16x8U),
            };
            (operand, lanes, How::Instruction(instruction))
        }
        "nearbyint" if float => (one, lanes, How::Opcode(|b, v| b.ins().nearest(v[0]))),
        // `fmin` and `fmax` are wasm's: a NaN if either is one, and -0 below +0.
        "minimum" if float => (two, lanes, How::Opcode(|b, v| b.ins().fmin(v[0], v[1]))),
        "maximum" if float => (two, lanes, How::Opcode(|b, v| b.ins().fmax(v[0], v[1]))),
        "wasm.relaxed.madd" if float => (
            three,
            lanes,
            How::Instruction(if f32 { W::F32x4RelaxedMadd } else { W::F64x2RelaxedMadd }),
        ),
        "wasm.relaxed.nmadd" if float => (
            three,
            lanes,
            How::Instruction(if f32 { W::F32x4RelaxedNmadd } else { W::F64x2RelaxedNmadd }),
        ),
        "wasm.relaxed.min" if float => (
            two,
            lanes,
            How::Instruction(if f32 { W::F32x4RelaxedMin } else { W::F64x2RelaxedMin }),
        ),
        "wasm.relaxed.max" if float => (
            two,
            lanes,
            How::Instruction(if f32 { W::F32x4RelaxedMax } else { W::F64x2RelaxedMax }),
        ),
        "wasm.relaxed.laneselect" if !float => {
            let instruction = match lanes {
                I8X16 => W::I8x16RelaxedLaneselect,
                I16X8 => W::I16x8RelaxedLaneselect,
                I32X4 => W::I32x4RelaxedLaneselect,
                _ => W::I64x2RelaxedLaneselect,
            };
            (three, lanes, How::Instruction(instruction))
        }
        _ => return None,
    })
}

/// The vector intrinsics whose names say nothing of their lanes.
fn untyped_vector_intrinsic(name: &str) -> Option<(&'static [Type], Type, How)> {
    use Instruction as W;
    use types::{F32X4, F64X2, I8X16, I16X8, I32X4};
    Some(match name {
        "wasm.swizzle" => (&[I8X16; 2], I8X16, How::Opcode(|b, v| b.ins().swizzle(v[0], v[1]))),
        "wasm.q15mulr.sat.signed" => {
            (&[I16X8; 2], I16X8, How::Opcode(|b, v| b.ins().sqmul_round_sat(v[0], v[1])))
        }
        "wasm.dot" => (&[I16X8; 2], I32X4, How::Instruction(W::I32x4DotI16x8S)),
        "wasm.relaxed.swizzle" => (&[I8X16; 2], I8X16, How::Instruction(W::I8x16RelaxedSwizzle)),
        "wasm.relaxed.trunc.signed" => {
            (&[F32X4], I32X4, How::Instruction(W::I32x4RelaxedTruncF32x4S))
        }
        "wasm.relaxed.trunc.unsigned" => {
            (&[F32X4], I32X4, How::Instruction(W::I32x4RelaxedTruncF32x4U))
        }
        "wasm.relaxed.trunc.signed.zero" => {
            (&[F64X2], I32X4, How::Instruction(W::I32x4RelaxedTruncF64x2SZero))
        }
        "wasm.relaxed.trunc.unsigned.zero" => {
            (&[F64X2], I32X4, How::Instruction(W::I32x4RelaxedTruncF64x2UZero))
        }
        "wasm.relaxed.q15mulr.signed" => {
            (&[I16X8; 2], I16X8, How::Instruction(W::I16x8RelaxedQ15mulrS))
        }
        "wasm.relaxed.dot.i8x16.i7x16.signed" => {
            (&[I8X16; 2], I16X8, How::Instruction(W::I16x8RelaxedDotI8x16I7x16S))
        }
        "wasm.relaxed.dot.i8x16.i7x16.add.signed" => {
            (&[I8X16, I8X16, I32X4], I32X4, How::Instruction(W::I32x4RelaxedDotI8x16I7x16AddS))
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use blaze_ids::rng::SplitMix64;

    use super::*;

    #[test]
    fn every_vector_intrinsic_of_core_arch() {
        for name in [
            "minimum.v4f32",
            "maximum.v2f64",
            "nearbyint.v4f32",
            "wasm.bitmask.v16i8",
            "wasm.alltrue.v2i64",
            "wasm.anytrue.v16i8",
            "wasm.bitselect.v16i8",
            "wasm.avgr.unsigned.v8i16",
            "wasm.extadd.pairwise.signed.v8i16",
            "wasm.extadd.pairwise.unsigned.v4i32",
            "wasm.q15mulr.sat.signed",
            "wasm.dot",
            "wasm.swizzle",
            "wasm.relaxed.swizzle",
            "wasm.relaxed.madd.v4f32",
            "wasm.relaxed.nmadd.v2f64",
            "wasm.relaxed.min.v4f32",
            "wasm.relaxed.max.v2f64",
            "wasm.relaxed.laneselect.v2i64",
            "wasm.relaxed.trunc.signed",
            "wasm.relaxed.trunc.unsigned.zero",
            "wasm.relaxed.q15mulr.signed",
            "wasm.relaxed.dot.i8x16.i7x16.signed",
            "wasm.relaxed.dot.i8x16.i7x16.add.signed",
        ] {
            assert!(vector_intrinsic(name).is_some(), "{name}");
        }
    }

    /// The names the string table admitted before the intrinsics were an enum, and their ops.
    const NAMES: [(&str, Op); 39] = [
        ("simd_add", Op::Add),
        ("simd_sub", Op::Sub),
        ("simd_mul", Op::Mul),
        ("simd_div", Op::Div),
        ("simd_and", Op::And),
        ("simd_or", Op::Or),
        ("simd_xor", Op::Xor),
        ("simd_eq", Op::Eq),
        ("simd_ne", Op::Ne),
        ("simd_lt", Op::Lt),
        ("simd_le", Op::Le),
        ("simd_gt", Op::Gt),
        ("simd_ge", Op::Ge),
        ("simd_saturating_add", Op::SatAdd),
        ("simd_saturating_sub", Op::SatSub),
        ("simd_shl", Op::Shl),
        ("simd_shr", Op::Shr),
        ("simd_neg", Op::Neg),
        ("simd_fabs", Op::Fabs),
        ("simd_fsqrt", Op::Sqrt),
        ("simd_floor", Op::Floor),
        ("simd_ceil", Op::Ceil),
        ("simd_trunc", Op::Trunc),
        ("simd_round_ties_even", Op::RoundEven),
        ("simd_ctpop", Op::Ctpop),
        ("simd_select", Op::Select),
        ("simd_splat", Op::Splat),
        ("simd_shuffle", Op::Shuffle),
        ("simd_cast", Op::Cast),
        ("simd_as", Op::Cast),
        ("simd_bitmask", Op::Bitmask),
        ("simd_reduce_add_unordered", Op::ReduceAdd),
        ("simd_reduce_max", Op::ReduceMax),
        ("simd_reduce_min", Op::ReduceMin),
        ("simd_reduce_and", Op::ReduceAnd),
        ("simd_reduce_or", Op::ReduceOr),
        ("simd_reduce_xor", Op::ReduceXor),
        ("simd_reduce_any", Op::ReduceAny),
        ("simd_reduce_all", Op::ReduceAll),
    ];

    /// Every `simd_*` intrinsic of the pinned nightly.
    fn simd_ops() -> impl Iterator<Item = SimdOp> {
        (0..Intrinsic::COUNT as u16).filter_map(|index| match Intrinsic::from_index(index) {
            Some(Intrinsic::Simd(op)) => Some(op),
            _ => None,
        })
    }

    /// Each name the string table admitted is the `SimdOp` that is still admitted as the same
    /// op, and no other `SimdOp` is admitted.
    #[test]
    fn admits_the_names_it_did() {
        for (name, op) in NAMES {
            let intrinsic = Intrinsic::from_symbol(&intern::Symbol::intern(name));
            let Some(Intrinsic::Simd(simd)) = intrinsic else { panic!("{name}: {intrinsic:?}") };
            assert_eq!(Op::of(simd), Some(op), "{name}");
        }
        let mut admitted = 0;
        for simd in simd_ops() {
            let name = Intrinsic::Simd(simd).to_string();
            let before = NAMES.iter().find(|(it, _)| *it == name).map(|&(_, op)| op);
            assert_eq!(Op::of(simd), before, "{name}");
            admitted += usize::from(before.is_some());
        }
        assert_eq!(admitted, NAMES.len());
        assert_eq!(Op::of(SimdOp::ReduceAddOrdered), None);
        assert_eq!(Op::of(SimdOp::Cast), Op::of(SimdOp::As));
        assert_eq!(Intrinsic::from_symbol(&intern::Symbol::intern("simd_fmin")), None);
    }

    /// Each op is one `SimdOp` but `Cast`, which `simd_cast` and `simd_as` both are, the ops'
    /// kinds do not overlap, and the arity a kind is matched with is the intrinsic's.
    #[test]
    fn every_op_is_named_once() {
        let mut seen = Vec::new();
        for simd in simd_ops() {
            let Some(op) = Op::of(simd) else { continue };
            let kinds = [op.is_binary(), op.is_unary(), op.is_reduce()];
            assert!(kinds.iter().filter(|it| **it).count() <= 1, "{simd:?}");
            if !matches!(simd, SimdOp::Cast | SimdOp::As) {
                assert!(!seen.contains(&op), "{simd:?}");
            }
            seen.push(op);
        }
        assert_eq!(seen.len(), NAMES.len());
        assert!(Op::Shr.is_binary() && !Op::Neg.is_binary());
        assert!(Op::Ctpop.is_unary() && !Op::Select.is_unary());
        assert!(Op::ReduceAdd.is_reduce() && !Op::Bitmask.is_reduce());
    }

    /// What memchr's and aho-corasick's (Teddy's) loops use on AArch64 (`vceqq_u8`,
    /// `vandq_u8`, `vshrq_n_u8`, `vshrn_n_u16`, `vmaxvq_u8`) and x86-64 (`_mm_cmpeq_epi8`,
    /// `_mm_movemask_epi8` as `simd_lt` of zero and `simd_bitmask`, `_mm_set1_epi8`) is
    /// vector code.
    #[test]
    fn admits_what_memchr_and_teddy_use() {
        use types::{I8, I16, I32};
        for (op, lane, signed) in [
            (Op::Eq, I8, false),
            (Op::And, I8, false),
            (Op::Or, I8, false),
            (Op::Shr, I8, false),
            (Op::Shr, I16, false),
            (Op::ReduceMax, I8, false),
            (Op::Bitmask, I8, true),
            (Op::Splat, I8, false),
            (Op::Shuffle, I8, false),
            (Op::Cast, I16, false),
            (Op::Select, I8, false),
        ] {
            assert!(admits(Vectors::Neon, op, lane, signed), "{op:?} {lane}");
        }
        for (op, lane, signed) in [
            (Op::Eq, I8, false),
            (Op::Lt, I8, true),
            (Op::Bitmask, I8, true),
            (Op::And, I8, false),
            (Op::Or, I8, false),
            (Op::Splat, I8, false),
            (Op::Shr, I16, false),
            (Op::Add, I32, true),
        ] {
            assert!(admits(Vectors::Sse2, op, lane, signed), "{op:?} {lane}");
        }
    }

    /// On x86-64 an operation whose instruction is past SSE2 is done a lane at a time, never a
    /// libcall of Cranelift's.
    #[test]
    fn sse2_refuses_later_features() {
        use types::{F32, F64, I8, I16, I32, I64};
        for (op, lane, signed) in [
            (Op::Floor, F32, true),
            (Op::Ceil, F64, true),
            (Op::Trunc, F32, true),
            (Op::RoundEven, F64, true),
            (Op::Ctpop, I8, false),
            (Op::Shuffle, I8, false),
            (Op::Cast, I16, false),
            (Op::Lt, I8, false),
            (Op::Gt, I32, false),
            (Op::Gt, I64, true),
            (Op::Eq, I64, false),
            (Op::Le, I16, true),
            (Op::Ge, I32, false),
            (Op::Shl, I8, false),
            (Op::Shr, I64, true),
            (Op::Mul, I8, false),
            (Op::Mul, I64, false),
            (Op::SatAdd, I32, true),
            (Op::ReduceMax, I8, false),
            (Op::ReduceAny, I8, false),
        ] {
            assert!(!admits(Vectors::Sse2, op, lane, signed), "{op:?} {lane}");
        }
        assert!(admits(Vectors::Sse2, Op::Shr, I64, false));
        assert!(admits(Vectors::Sse2, Op::Le, F64, true));
        assert!(admits(Vectors::Sse2, Op::Mul, I16, true));
    }

    /// wasm keeps what it was given before every target was: no reduction, and the limits
    /// wasm's instructions have.
    #[test]
    fn wasm_admits_as_before() {
        use types::{F32, I8, I16, I32, I64};
        assert!(!admits(Vectors::Wasm, Op::Div, I32, true));
        assert!(admits(Vectors::Wasm, Op::Div, F32, true));
        assert!(!admits(Vectors::Wasm, Op::SatAdd, I32, true));
        assert!(admits(Vectors::Wasm, Op::SatAdd, I16, true));
        assert!(!admits(Vectors::Wasm, Op::Ctpop, I16, false));
        assert!(!admits(Vectors::Wasm, Op::And, F32, true));
        assert!(!admits(Vectors::Wasm, Op::Bitmask, F32, true));
        assert!(admits(Vectors::Wasm, Op::Mul, I64, true));
        assert!(admits(Vectors::Wasm, Op::Floor, F32, true));
        assert!(!admits(Vectors::Wasm, Op::ReduceMax, I8, false));
        assert!(admits(Vectors::Neon, Op::SatAdd, I32, true));
        assert!(!admits(Vectors::Neon, Op::Mul, I64, true));
        assert!(!admits(Vectors::Neon, Op::ReduceAdd, F32, true));
    }

    /// The factor repeats a lane as storing it in every lane of 8 bytes would.
    #[test]
    fn repeat_factor_repeats_the_lane() {
        let mut rng = SplitMix64::new(0x5eed);
        for _ in 0..1000 {
            for bits in [8u32, 16, 32] {
                let lane = rng.next_u64() & ((1u64 << bits) - 1);
                let factor = repeat_factor(bits).expect("a factor") as u64;
                let mut lanes = 0u64;
                for at in (0..64).step_by(bits as usize) {
                    lanes |= lane << at;
                }
                assert_eq!(lane.wrapping_mul(factor), lanes, "{bits} {lane:#x}");
            }
        }
        assert_eq!(repeat_factor(64), None);
    }

    /// [`FunctionCx::simd_reduce`]'s rotations, on a model of the vector: after moving down by
    /// 8, 4, ... bytes down to a lane and combining each time, lane 0 is the fold of every
    /// lane, for each lane width and combination.
    #[test]
    fn rotations_reduce_every_lane() {
        let mut rng = SplitMix64::new(0xb1a2e);
        for _ in 0..500 {
            let mut bytes = [0u8; 16];
            rng.fill(&mut bytes);
            for lane in [1u8, 2, 4, 8] {
                let read = |bytes: &[u8; 16], i: usize| {
                    let mut word = [0u8; 8];
                    word[..lane as usize]
                        .copy_from_slice(&bytes[i * lane as usize..][..lane as usize]);
                    u64::from_le_bytes(word)
                };
                let mask = if lane == 8 { u64::MAX } else { (1u64 << (lane * 8)) - 1 };
                let ops: [fn(u64, u64, u64) -> u64; 4] = [
                    |x, y, _| x.max(y),
                    |x, y, _| x.min(y),
                    |x, y, mask| x.wrapping_add(y) & mask,
                    |x, y, _| x ^ y,
                ];
                for combine in ops {
                    let count = 16 / lane as usize;
                    let first = read(&bytes, 0);
                    let expected =
                        (1..count).fold(first, |acc, i| combine(acc, read(&bytes, i), mask));
                    let mut value = bytes;
                    let mut by = 8;
                    while by >= lane {
                        let order = rotation(by);
                        let moved: [u8; 16] =
                            std::array::from_fn(|at| value[order[at] as usize % 16]);
                        let mut out = [0u8; 16];
                        for i in 0..count {
                            let lane_value = combine(read(&value, i), read(&moved, i), mask);
                            out[i * lane as usize..][..lane as usize]
                                .copy_from_slice(&lane_value.to_le_bytes()[..lane as usize]);
                        }
                        value = out;
                        by /= 2;
                    }
                    assert_eq!(read(&value, 0), expected, "lanes of {lane}");
                }
            }
        }
    }

    /// Every index of a rotation is one of the two operands' 32 bytes, and consecutive, which
    /// is the pattern Cranelift's AArch64 backend lowers to `ext`.
    #[test]
    fn rotations_are_ext() {
        for by in [1u8, 2, 4, 8] {
            let order = rotation(by);
            assert_eq!(order[0], by);
            assert!(order.windows(2).all(|it| it[1] == it[0] + 1));
            assert!(order.iter().all(|it| *it < 32));
        }
    }
}
