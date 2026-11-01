//! What the IR builder's lowerings of intrinsics, `llvm.*` functions, operators and float
//! conversions ask for besides their operands, decided as they decide it but without building
//! IR: compiler-builtins' and the C library's functions, the routines Cranelift calls where
//! the baseline x86-64 it compiles for has no instruction, and the functions of assembly the
//! lowerings call, by their text.
//!
//! The lowerings that build IR are mirrored here on the walk's own types: `FunctionCx::operand_ty`
//! of a projected place builds the place, so `immediate`, `splat_immediate` and `half_lanes` are
//! too. Where a lowering decides by the code around the call (the fused multiply-adds of
//! `fma.rs`), every form it may take is listed.

use hir_ty::db::HirDatabase;
use std::{
    borrow::Cow,
    fmt::{self, Write as _},
};

use blaze_hash::HashSet;
use cranelift_codegen::ir::{self, types};
use hir_def::{
    AdtId, FunctionId, HasModule, VariantId,
    layout::{BackendRepr, Primitive, Variants},
    signatures::FunctionSignature,
};
use hir_ty::{
    TargetFeature,
    consteval::try_const_usize,
    intrinsic::{AtomicOp, FloatWidth, Intrinsic, SimdOp},
    layout::Layout,
    next_solver::{ConstKind, GenericArgs, Ty, TyKind},
};
use hir_ty_mir::mir::{
    AggregateKind, BinOp, CastKind, MirBody, Operand, OperandKind, Rvalue, UnOp,
};
use rustc_abi::Size;
use rustc_type_ir::inherent::{GenericArgs as _, IntoKind, SliceLike as _};
use smallvec::SmallVec;

use super::{Tys, Walk, told};
use crate::{
    AbiScalar, Arch, CodegenError, VariableArguments, asm, avx512, f16_f128, neon, sve, x86,
};

/// Whether `wasm_simd_intrinsic` may make wasm's vector instructions of the generic intrinsic
/// `op` (its `Op::of`), which ask for nothing but their operands.
const fn wasm_simd(op: SimdOp) -> bool {
    use SimdOp as S;
    matches!(
        op,
        S::Add
            | S::Sub
            | S::Mul
            | S::Div
            | S::And
            | S::Or
            | S::Xor
            | S::Eq
            | S::Ne
            | S::Lt
            | S::Le
            | S::Gt
            | S::Ge
            | S::SaturatingAdd
            | S::SaturatingSub
            | S::Shl
            | S::Shr
            | S::Neg
            | S::Fabs
            | S::Fsqrt
            | S::Floor
            | S::Ceil
            | S::Trunc
            | S::RoundTiesEven
            | S::Ctpop
            | S::Select
            | S::Splat
            | S::Shuffle
            | S::Cast
            | S::As
            | S::Bitmask
            | S::ReduceAddUnordered
            | S::ReduceMax
            | S::ReduceMin
            | S::ReduceAnd
            | S::ReduceOr
            | S::ReduceXor
            | S::ReduceAny
            | S::ReduceAll
    )
}

/// An operation of a float Cranelift's x86-64 backend calls the C library for: the baseline it
/// compiles for has neither SSE4.1's rounding nor FMA.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Routine {
    Floor,
    Ceil,
    Trunc,
    Nearest,
    Fma,
}

fn routine(op: Routine, ty: ir::Type) -> ir::LibCall {
    use ir::LibCall as L;
    match (op, ty == types::F32) {
        (Routine::Floor, true) => L::FloorF32,
        (Routine::Floor, false) => L::FloorF64,
        (Routine::Ceil, true) => L::CeilF32,
        (Routine::Ceil, false) => L::CeilF64,
        (Routine::Trunc, true) => L::TruncF32,
        (Routine::Trunc, false) => L::TruncF64,
        (Routine::Nearest, true) => L::NearestF32,
        (Routine::Nearest, false) => L::NearestF64,
        (Routine::Fma, true) => L::FmaF32,
        (Routine::Fma, false) => L::FmaF64,
    }
}

/// The error the IR builder's `unsupported!` makes of `what`.
fn unsupported(what: impl Into<String>) -> String {
    told(CodegenError::Unsupported(what.into()))
}

/// compiler-builtins' quotient or remainder of two 128-bit integers (`FunctionCx::divide`).
fn divide_builtin(div: bool, signed: bool) -> &'static str {
    match (div, signed) {
        (true, true) => "__divti3",
        (true, false) => "__udivti3",
        (false, true) => "__modti3",
        (false, false) => "__umodti3",
    }
}

/// compiler-builtins' conversion between an `f32` or `f64` and a 128-bit integer
/// (`FunctionCx::float_cast`), `None` for a cast it has none for.
fn wide_int_float(from: AbiScalar, to: AbiScalar) -> Option<&'static str> {
    Some(match (from.ty, from.signed, to.ty, to.signed) {
        (types::F32, _, _, true) => "__fixsfti",
        (types::F32, _, _, false) => "__fixunssfti",
        (types::F64, _, _, true) => "__fixdfti",
        (types::F64, _, _, false) => "__fixunsdfti",
        (_, true, types::F32, _) => "__floattisf",
        (_, false, types::F32, _) => "__floatuntisf",
        (_, true, types::F64, _) => "__floattidf",
        (_, false, types::F64, _) => "__floatuntidf",
        _ => return None,
    })
}

/// compiler-builtins' `f128` of an integer of `bits` (`int_to_quad`).
fn quad_of_int(bits: u32, signed: bool) -> &'static str {
    match (bits, signed) {
        (..=32, true) => "__floatsitf",
        (..=32, false) => "__floatunsitf",
        (64, true) => "__floatditf",
        (64, false) => "__floatunditf",
        (_, true) => "__floattitf",
        (_, false) => "__floatuntitf",
    }
}

/// compiler-builtins' integer of `bits` of an `f128` (`quad_to_int`).
fn int_of_quad(bits: u32, signed: bool) -> &'static str {
    match (bits, signed) {
        (..=32, true) => "__fixtfsi",
        (..=32, false) => "__fixunstfsi",
        (64, true) => "__fixtfdi",
        (64, false) => "__fixunstfdi",
        (_, true) => "__fixtfti",
        (_, false) => "__fixunstfti",
    }
}

/// `text` with each `{imm}` in it made `value`, as `str::replace` makes it.
fn put_immediate(text: &str, value: impl fmt::Display) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for (i, piece) in text.split("{imm}").enumerate() {
        if i > 0 {
            _ = write!(out, "{value}");
        }
        out.push_str(piece);
    }
    out
}

/// The letter `asm::fcvt` names a float of `ty` by.
fn letter(ty: ir::Type) -> char {
    match ty {
        types::F16 => 'h',
        types::F32 => 's',
        _ => 'd',
    }
}

/// Whether `neon_lanes` lowers `llvm.aarch64.neon.{name}` of `arity` operands lane by lane.
fn neon_pairwise(name: &str, arity: usize) -> bool {
    let mut parts = name.split('.');
    let op = parts.next().unwrap_or_default();
    let first = name.split('.').nth(1);
    let Some((lanes, ty)) = parts.last().and_then(|it| {
        let (lanes, bits) = it.strip_prefix('v')?.split_once('i')?;
        Some((lanes.parse::<i32>().ok()?, ir::Type::int(bits.parse().ok()?)?))
    }) else {
        return false;
    };
    let width = ty.bytes() as i32;
    match (op, arity) {
        ("addp", 2) if lanes * width == 16 && ty.bits() <= 32 => true,
        ("umaxp" | "smaxp" | "uminp" | "sminp" | "addp", 2) => true,
        ("sqdmulh", 2) => ty.bits() <= 32,
        ("uaddlp" | "saddlp", 1) => ty.bits() <= 32,
        ("uaddlv" | "saddlv" | "uaddv" | "saddv", 1) => first
            .and_then(|it| ir::Type::int(it.strip_prefix('i')?.parse().ok()?))
            .is_some(),
        ("sqdmull", 2) => ty.bits() >= 32,
        ("tbl1", 2) | ("tbx1", 3) => ty == types::I8,
        _ => false,
    }
}

/// The instructions `lower_fma` may make of an fma of `bytes` with the instruction suffix
/// `suffix`: each folded negation (`vfnmadd`, `vfmsub`, `vfnmsub`), and either factor first.
fn fma_texts(bytes: u32, suffix: &str) -> Vec<String> {
    let reg = match bytes {
        64 => "zmm",
        32 => "ymm",
        _ => "xmm",
    };
    let mut texts = Vec::with_capacity(8);
    for op in ["vfmadd", "vfnmadd", "vfmsub", "vfnmsub"] {
        for (first, second) in [(1, 2), (2, 1)] {
            // alloc: the instruction's text, which `avx512_function` is handed
            texts.push(format!(
                "vmovaps {reg}0, {reg}{second}\n{op}213{suffix} {reg}0, {reg}{first}, {reg}3"
            ));
        }
    }
    texts
}

impl<'cx, 'db> Walk<'_, 'cx, 'db> {
    fn ty_layout(&self, ty: Ty<'db>) -> Result<&'cx Layout, String> {
        self.fx.layout(ty).map_err(told)
    }

    fn ty_scalar(&self, ty: Ty<'db>) -> Result<AbiScalar, String> {
        self.fx.scalar_of(ty).map_err(told)
    }

    fn simd_lanes_of(&self, ty: Ty<'db>) -> Result<(AbiScalar, u64), String> {
        self.fx.simd_lanes(ty).map_err(told)
    }

    fn on_x86(&self) -> bool {
        self.fx.cg.target.arch == Arch::X86_64
    }

    /// `check_bfloat_blocks`, and what `receive_bfloat` rounds an `f16b` parameter by on entry.
    pub(super) fn bfloat(&mut self, body: &'db MirBody<'db>) -> Result<(), String> {
        self.fx.check_bfloat_blocks(body).map_err(told)?;
        if !self.on_x86() {
            return Ok(());
        }
        for &local in &body.param_locals {
            // A parameter without a layout is refused where it is compiled, for that.
            let Ok(layout) = self.fx.layout(body.locals[local].ty.as_ref()) else { continue };
            if let BackendRepr::Scalar(scalar) = layout.backend_repr
                && scalar.primitive() == Primitive::Float(rustc_abi::Float::F16B)
            {
                return self.symbol("__truncsfbf2");
            }
        }
        Ok(())
    }

    /// `FunctionCx::neon_fused`: when the body is one of core's NEON fused multiply-adds, the
    /// function of its instruction, which is then the whole body.
    pub(super) fn neon_fused(&mut self) -> Result<bool, String> {
        let Some((name, lowered)) = self.fx.neon_fused().map_err(told)? else { return Ok(false) };
        let body = self.fx.body.expect("MIR body");
        let arg_tys: Tys<'db> =
            // alloc: into `Tys`, inline up to eight parameters
            body.param_locals.iter().map(|&local| body.locals[local].ty.as_ref()).collect();
        let dst_ty = body.locals[crate::returned_local(body)].ty.as_ref();
        let text = self.neon_text(&name, &lowered, &lowered.text, &arg_tys, dst_ty)?;
        self.assembly(text)?;
        Ok(true)
    }

    /// `binary_op` and `overflowing_op`: a constant pattern's comparison part by part, a soft
    /// float's operator, `%` of floats, and the 128-bit products and quotients Cranelift has
    /// no instruction for.
    pub(super) fn binary_op(
        &mut self,
        op: &BinOp,
        lhs: &Operand,
        rhs: &Operand,
    ) -> Result<(), String> {
        let given = op;
        let (op, overflowing) = match op {
            BinOp::AddWithOverflow => (&BinOp::Add, true),
            BinOp::SubWithOverflow => (&BinOp::Sub, true),
            BinOp::MulWithOverflow => (&BinOp::Mul, true),
            op => (op, false),
        };
        let (mut ty, mut r_ty) = (self.operand_ty(lhs), self.operand_ty(rhs));
        while let TyKind::Ref(_, inner, _) = ty.kind() {
            ty = inner;
        }
        while let TyKind::Ref(_, inner, _) = r_ty.kind() {
            r_ty = inner;
        }
        if let BinOp::Eq | BinOp::Ne = op
            && matches!(
                ty.kind(),
                TyKind::Adt(..)
                    | TyKind::Tuple(_)
                    | TyKind::Array(..)
                    | TyKind::Slice(_)
                    | TyKind::Str
            )
        {
            return self.equal_parts(ty, &mut HashSet::default());
        }
        if matches!(ty.kind(), TyKind::RawPtr(..))
            && matches!(self.ty_layout(ty)?.backend_repr, BackendRepr::ScalarPair { .. })
        {
            return match op {
                BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => Ok(()),
                // alloc: the error's message
                _ => Err(unsupported(format!("{op:?} on wide pointers"))),
            };
        }
        let scalar = self.ty_scalar(ty)?;
        self.ty_scalar(r_ty)?;
        let float = f16_f128::soft(scalar.ty) || scalar.ty.is_float();
        if f16_f128::soft(scalar.ty) {
            self.soft_binary(op, scalar.ty)?;
        } else if scalar.ty.is_float() {
            self.float_binary(op, scalar.ty)?;
        }
        if float {
            if overflowing {
                // alloc: the error's message
                return Err(unsupported(format!("{given:?} of floats")));
            }
            return Ok(());
        }
        match op {
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => Ok(()),
            BinOp::Mul if scalar.ty == types::I128 => self.wide_mul(scalar.signed),
            BinOp::Div | BinOp::Rem => self.int_divide(scalar, matches!(op, BinOp::Div)),
            BinOp::Offset => Err(unsupported("pointer offset")),
            _ => Ok(()),
        }
    }

    /// `UnaryOp`: `!` and `-` are instructions, and `-` of a soft float (`soft_neg`) a flip of
    /// its sign bit.
    pub(super) fn unary_op(&mut self, op: &UnOp, operand: &Operand) -> Result<(), String> {
        let ty = self.operand_ty(operand);
        match op {
            UnOp::Not | UnOp::Neg => self.ty_scalar(ty).map(drop),
        }
    }

    /// `float_cast` of the scalars of `from` and `to`.
    pub(super) fn float_cast(
        &mut self,
        _kind: &CastKind,
        from: Ty<'db>,
        to: Ty<'db>,
    ) -> Result<(), String> {
        let (from, to) = (self.ty_scalar(from)?, self.ty_scalar(to)?);
        self.scalar_cast(from, to)
    }

    /// `FunctionCx::intrinsic`: true where it lowers the call (`Inline` or `Diverges`), false
    /// where the function's body is called (`Unknown`).
    pub(super) fn intrinsic(
        &mut self,
        intrinsic: Intrinsic,
        generic_args: GenericArgs<'db>,
        args: &[Operand],
        dst_ty: Ty<'db>,
    ) -> Result<bool, String> {
        if self.scalable_intrinsic(intrinsic, generic_args, args, dst_ty)? {
            return Ok(true);
        }
        // `wasm_simd_intrinsic` makes wasm's instructions of what it can, which ask for
        // nothing; the rest is lowered as on any target.
        let wasm = matches!(intrinsic, Intrinsic::Simd(op) if wasm_simd(op))
            && self.fx.cg.target.is_wasm()
            && self.fx.simd128();
        match self.lowered_intrinsic(intrinsic, generic_args, args, dst_ty) {
            Ok(inline) => Ok(inline || wasm),
            Err(_) if wasm => {
                self.operands(args)?;
                Ok(true)
            }
            Err(why) => Err(why),
        }
    }

    /// `scalable_intrinsic`: the stubs of SVE's selects and conversions.
    fn scalable_intrinsic(
        &mut self,
        intrinsic: Intrinsic,
        generic_args: GenericArgs<'db>,
        args: &[Operand],
        dst_ty: Ty<'db>,
    ) -> Result<bool, String> {
        use Intrinsic as I;
        let ours = matches!(
            intrinsic,
            I::SveTupleCreate2
                | I::SveTupleCreate3
                | I::SveTupleCreate4
                | I::SveTupleGet
                | I::SveTupleSet
                | I::Simd(SimdOp::Select | SimdOp::Cast)
        );
        if !ours {
            return Ok(false);
        }
        let Some(to) = self.fx.scalable(dst_ty).map_err(told)? else { return Ok(false) };
        let mut shapes = Vec::with_capacity(args.len());
        for arg in args {
            let ty = self.operand(arg)?;
            shapes.push(self.fx.scalable(ty).map_err(told)?);
        }
        match (intrinsic, &shapes[..]) {
            (I::SveTupleCreate2 | I::SveTupleCreate3 | I::SveTupleCreate4, _) => {
                if shapes.len() as u64 != to.vectors
                    || shapes.iter().any(|it| it.is_none_or(|it| it.vectors != 1 || it.predicate()))
                {
                    // alloc: the error's message
                    return Err(unsupported(format!("`{intrinsic}` of {dst_ty:?}")));
                }
            }
            (I::SveTupleGet | I::SveTupleSet, _) => {
                let Some(index) = generic_args.iter().nth(2).and_then(|it| it.konst()) else {
                    // alloc: the error's message
                    return Err(unsupported(format!("`{intrinsic}` without its index")));
                };
                let ConstKind::Value(index) = index.kind() else {
                    // alloc: the error's message
                    return Err(unsupported(format!("`{intrinsic}` with an index not evaluated")));
                };
                let index = index.value.inner().to_leaf().to_bits(Size::from_bytes(4)) as i32;
                let fits = match (intrinsic, &shapes[..]) {
                    (I::SveTupleGet, [Some(of)]) => {
                        (0..of.vectors as i32).contains(&index) && to.vectors == 1
                    }
                    (I::SveTupleSet, [Some(of), Some(one)]) => {
                        (0..of.vectors as i32).contains(&index) && one.vectors == 1 && *of == to
                    }
                    _ => false,
                };
                if !fits {
                    // alloc: the error's message
                    return Err(unsupported(format!("`{intrinsic}` of {dst_ty:?} at {index}")));
                }
            }
            (I::Simd(SimdOp::Select), [Some(mask), Some(value), Some(_)]) => {
                let text = sve::select(*mask, *value).map_err(unsupported)?;
                self.assembly(text)?;
            }
            (I::Simd(SimdOp::Cast), [Some(from)]) => {
                if let Some(text) = sve::cast(*from, to).map_err(unsupported)? {
                    self.assembly(text)?;
                }
            }
            // alloc: the error's message
            _ => return Err(unsupported(format!("intrinsic `{intrinsic}` on scalable vectors"))),
        }
        Ok(true)
    }

    /// The match of `FunctionCx::intrinsic`, arm by arm.
    fn lowered_intrinsic(
        &mut self,
        intrinsic: Intrinsic,
        generic_args: GenericArgs<'db>,
        args: &[Operand],
        dst_ty: Ty<'db>,
    ) -> Result<bool, String> {
        use AtomicOp as A;
        use FloatWidth as W;
        use Intrinsic as I;
        use SimdOp as S;
        let unread = matches!(
            intrinsic,
            I::Assume
                | I::ColdPath
                | I::Forget
                | I::AssertInhabited
                | I::AssertZeroValid
                | I::AssertMemUninitializedValid
                | I::IsValStaticallyKnown
                | I::Unreachable
                | I::AbortImmediate
                | I::Breakpoint
        );
        let arg_tys = match unread {
            // alloc: into `Tys`, inline up to eight operands
            true => args.iter().map(|it| self.operand_ty(it)).collect(),
            false => self.operands(args)?,
        };
        let soft = f16_f128::soft;
        match (intrinsic, args) {
            (
                I::WrappingAdd
                | I::UncheckedAdd
                | I::WrappingSub
                | I::UncheckedSub
                | I::WrappingMul
                | I::UncheckedMul
                | I::UncheckedDiv
                | I::ExactDiv
                | I::UncheckedRem
                | I::UncheckedShl
                | I::UncheckedShr
                | I::RotateLeft
                | I::RotateRight
                | I::SaturatingAdd
                | I::SaturatingSub
                | I::ThreeWayCompare,
                [_, _],
            ) => {
                let scalar = self.ty_scalar(arg_tys[0])?;
                match intrinsic {
                    I::UncheckedDiv | I::ExactDiv => self.int_divide(scalar, true)?,
                    I::UncheckedRem => self.int_divide(scalar, false)?,
                    _ => (),
                }
            }
            (I::AddWithOverflow | I::SubWithOverflow | I::MulWithOverflow, [_, _]) => {
                let scalar = self.ty_scalar(arg_tys[0])?;
                if intrinsic == I::MulWithOverflow && scalar.ty == types::I128 {
                    self.wide_mul(scalar.signed)?;
                }
            }
            (I::TypeName, []) => {
                let ty = generic_args.type_at(0);
                if hir_ty_mir::mir::type_name::rustc_type_name(self.db(), ty).is_none() {
                    // alloc: the error's message
                    return Err(unsupported(format!("`type_name` of `{ty:?}`")));
                }
            }
            (I::VaArg, [_]) => match self.fx.cg.target.variable_arguments() {
                Some(
                    VariableArguments::Buffer
                    | VariableArguments::Homed
                    | VariableArguments::Stack
                    | VariableArguments::Registers,
                ) => (),
                _ => return Err(unsupported("intrinsic `va_arg` for this target")),
            },
            (I::FloatToIntUnchecked, [_]) => {
                let (from, to) = (self.ty_scalar(arg_tys[0])?, self.ty_scalar(dst_ty)?);
                self.scalar_cast(from, to)?;
            }
            (I::Atomic(A::Load, _), [_]) => self.wide_atomic(dst_ty)?,
            (I::Atomic(A::Store, _), [_, _]) => self.wide_atomic(arg_tys[1])?,
            (
                I::Atomic(
                    A::Xchg
                    | A::Xadd
                    | A::Xsub
                    | A::And
                    | A::Nand
                    | A::Or
                    | A::Xor
                    | A::Max
                    | A::Min
                    | A::Umax
                    | A::Umin,
                    _,
                ),
                [_, _],
            ) => self.wide_atomic(dst_ty)?,
            (I::Atomic(A::Cxchg | A::CxchgWeak, _), [_, _, _]) => self.wide_atomic(arg_tys[1])?,
            (
                I::Assume
                | I::ColdPath
                | I::Forget
                | I::AssertInhabited
                | I::AssertZeroValid
                | I::AssertMemUninitializedValid
                | I::IsValStaticallyKnown,
                _,
            ) => (),
            // A trap.
            (I::Unreachable | I::AbortImmediate | I::Breakpoint, _) => (),
            (I::RawEq | I::CompareBytes, [_, _, ..]) => {
                if intrinsic == I::RawEq {
                    let size = self.ty_layout(generic_args.type_at(0))?.size.bytes();
                    // `copies::equal_bytes` compares as much inline.
                    if size <= 2 * u64::from(crate::ptr().bytes()) {
                        return Ok(true);
                    }
                }
                self.symbol("memcmp")?;
            }
            (I::Simd(S::Fma | S::RelaxedFma), _) if self.fx.x86_fused() => {
                self.x86_fma(intrinsic, args, dst_ty)?
            }
            (
                I::Simd(
                    op @ (S::Add
                    | S::Sub
                    | S::Mul
                    | S::Div
                    | S::Eq
                    | S::Ge
                    | S::Gt
                    | S::Le
                    | S::Lt
                    | S::Neg
                    | S::Fabs
                    | S::Fsqrt
                    | S::Floor
                    | S::Ceil
                    | S::Fma
                    | S::Cast),
                ),
                _,
            ) if self.half_lanes(&arg_tys, dst_ty)? => {
                self.half_vector(op, args, &arg_tys, dst_ty)?
            }
            (
                I::Simd(
                    op @ (S::And
                    | S::Or
                    | S::Xor
                    | S::Add
                    | S::Sub
                    | S::Mul
                    | S::Shl
                    | S::Shr
                    | S::Eq
                    | S::Ne
                    | S::Lt
                    | S::Le
                    | S::Gt
                    | S::Ge
                    | S::Div
                    | S::SaturatingAdd
                    | S::SaturatingSub),
                ),
                [_, _],
            ) => {
                let (lane, _) = self.simd_lanes_of(arg_tys[0])?;
                self.simd_lanes_of(dst_ty)?;
                let by_lanes = matches!(
                    op,
                    S::Eq
                        | S::Ne
                        | S::Lt
                        | S::Le
                        | S::Gt
                        | S::Ge
                        | S::Add
                        | S::Sub
                        | S::Mul
                        | S::Div
                );
                if lane.ty.is_float() && !by_lanes {
                    // alloc: the error's message
                    return Err(unsupported(format!("`{intrinsic}` of floats")));
                }
                if !lane.ty.is_float() && op == S::Div {
                    return Err(unsupported("`simd_div` of integers"));
                }
            }
            (I::Simd(S::Fma | S::RelaxedFma), [_, _, _]) => {
                let (lane, count) = self.simd_lanes_of(dst_ty)?;
                if !lane.ty.is_float() {
                    // alloc: the error's message
                    return Err(unsupported(format!("`{intrinsic}` of integers")));
                }
                let swapped = self.fx.cg.target.arch == Arch::Aarch64 && count > 1;
                let bytes = count * u64::from(lane.ty.bytes());
                let chunk = match bytes {
                    8 => 8,
                    _ if bytes % 16 == 0 => 16,
                    _ => 0,
                };
                let vectors = swapped && matches!(lane.ty, types::F32 | types::F64) && chunk != 0;
                let unfused = intrinsic == I::Simd(S::RelaxedFma) && self.on_x86();
                if !vectors && !unfused {
                    self.cranelift_routine(Routine::Fma, lane.ty)?;
                }
            }
            (I::Simd(S::Cast | S::As), [_]) => {
                let (from, _) = self.simd_lanes_of(arg_tys[0])?;
                let (to, _) = self.simd_lanes_of(dst_ty)?;
                if from.ty.is_float() || to.ty.is_float() {
                    self.scalar_cast(from, to)?;
                }
            }
            (I::Simd(S::Bitmask), [_]) => {
                let (lane, count) = self.simd_lanes_of(arg_tys[0])?;
                let BackendRepr::Scalar(_) = self.ty_layout(dst_ty)?.backend_repr else {
                    // alloc: the error's message
                    return Err(unsupported(format!("simd_bitmask into {dst_ty:?}")));
                };
                self.ty_scalar(dst_ty)?;
                if count > 64 || lane.ty.bits() > 64 {
                    // alloc: the error's message
                    return Err(unsupported(format!("simd_bitmask of {count} lanes")));
                }
            }
            (
                I::Simd(S::ReduceAnd | S::ReduceOr | S::ReduceXor | S::ReduceMax | S::ReduceMin),
                [_],
            ) => {
                let (lane, _) = self.simd_lanes_of(arg_tys[0])?;
                if lane.ty.is_float() {
                    // alloc: the error's message
                    return Err(unsupported(format!("`{intrinsic}` of floats")));
                }
            }
            (I::Simd(op @ (S::Trunc | S::Round | S::RoundTiesEven)), [_])
                if self.fx.cg.target.arch == Arch::Aarch64 =>
            {
                let ty = arg_tys[0];
                let lanes = match self.ty_layout(ty)?.backend_repr {
                    BackendRepr::SimdVector { element, count } => match element.primitive() {
                        Primitive::Float(float) => {
                            neon::Lanes::floats(float.size().bits() as u32, count.as_u64() as u32)
                        }
                        // alloc: the error's message
                        _ => return Err(unsupported(format!("`{intrinsic}` of integers"))),
                    },
                    // alloc: the error's message
                    _ => return Err(unsupported(format!("vector operation on {ty:?}"))),
                };
                let instruction = match op {
                    S::Trunc => "frintz",
                    S::Round => "frinta",
                    _ => "frintx",
                };
                let Some(lowered) = neon::rounding(instruction, lanes) else {
                    // alloc: the error's message
                    return Err(unsupported(format!("`{intrinsic}` of {ty:?}")));
                };
                self.neon_assembly(&intrinsic, &lowered, args, dst_ty)?;
            }
            (
                I::Simd(
                    op @ (S::Ctpop
                    | S::Ctlz
                    | S::Cttz
                    | S::Bswap
                    | S::Bitreverse
                    | S::Neg
                    | S::Fabs
                    | S::Fsqrt
                    | S::Floor
                    | S::Ceil),
                ),
                [_],
            ) => {
                let (lane, _) = self.simd_lanes_of(arg_tys[0])?;
                let float = lane.ty.is_float();
                match op {
                    S::Ctpop | S::Ctlz | S::Cttz | S::Bswap | S::Bitreverse if float => {
                        // alloc: the error's message
                        return Err(unsupported(format!("`{intrinsic}` of floats")));
                    }
                    S::Fabs | S::Fsqrt | S::Floor | S::Ceil if !float => {
                        // alloc: the error's message
                        return Err(unsupported(format!("`{intrinsic}` of integers")));
                    }
                    S::Fsqrt | S::Floor | S::Ceil
                        if lane.ty != types::F32 && lane.ty != types::F64 =>
                    {
                        // alloc: the error's message
                        return Err(unsupported(format!("`{intrinsic}` of {:?} lanes", lane.ty)));
                    }
                    S::Floor => self.cranelift_routine(Routine::Floor, lane.ty)?,
                    S::Ceil => self.cranelift_routine(Routine::Ceil, lane.ty)?,
                    _ => (),
                }
            }
            (I::Simd(S::FunnelShl | S::FunnelShr), [_, _, _]) => {
                let (lane, _) = self.simd_lanes_of(arg_tys[0])?;
                if lane.ty.is_float() {
                    // alloc: the error's message
                    return Err(unsupported(format!("`{intrinsic}` of floats")));
                }
            }
            // The ordered ones start from their second operand.
            (I::Simd(S::ReduceAddOrdered | S::ReduceMulOrdered), [_, _])
            | (I::Simd(S::ReduceAddUnordered | S::ReduceMulUnordered), [_]) => (),
            (
                I::Fabs | I::Sqrt | I::Floor | I::Ceil | I::Trunc | I::RoundTiesEven | I::Round,
                [_],
            ) => {
                let scalar = self.ty_scalar(arg_tys[0])?;
                if !scalar.ty.is_float() {
                    // alloc: the error's message
                    return Err(unsupported(format!(
                        "intrinsic `{intrinsic}` on {:?}",
                        scalar.ty
                    )));
                }
                if soft(scalar.ty) {
                    self.soft_unary(intrinsic, scalar.ty)?;
                    return Ok(true);
                }
                match intrinsic {
                    I::Fabs | I::Sqrt => (),
                    I::Floor => self.cranelift_routine(Routine::Floor, scalar.ty)?,
                    I::Ceil => self.cranelift_routine(Routine::Ceil, scalar.ty)?,
                    I::Trunc => self.cranelift_routine(Routine::Trunc, scalar.ty)?,
                    I::RoundTiesEven => self.cranelift_routine(Routine::Nearest, scalar.ty)?,
                    _ => self.symbol(if scalar.ty == types::F32 { "roundf" } else { "round" })?,
                }
            }
            (I::Powi, [_, _]) => {
                let scalar = self.ty_scalar(arg_tys[0])?;
                self.ty_scalar(arg_tys[1])?;
                if soft(scalar.ty) {
                    self.soft_powi(scalar.ty)?;
                } else {
                    self.symbol(if scalar.ty == types::F32 { "__powisf2" } else { "__powidf2" })?;
                }
            }
            (
                I::CopySign(_)
                | I::Powf
                | I::FaddFast
                | I::FsubFast
                | I::FmulFast
                | I::FdivFast
                | I::FremFast
                | I::FaddAlgebraic
                | I::FsubAlgebraic
                | I::FmulAlgebraic
                | I::FdivAlgebraic
                | I::FremAlgebraic,
                [_, _],
            ) => {
                let scalar = self.ty_scalar(arg_tys[0])?;
                self.ty_scalar(arg_tys[1])?;
                if !scalar.ty.is_float() {
                    // alloc: the error's message
                    return Err(unsupported(format!(
                        "intrinsic `{intrinsic}` on {:?}",
                        scalar.ty
                    )));
                }
                if soft(scalar.ty) {
                    let op = match intrinsic {
                        I::CopySign(W::F16 | W::F128) => None,
                        I::FaddFast | I::FaddAlgebraic => Some(BinOp::Add),
                        I::FsubFast | I::FsubAlgebraic => Some(BinOp::Sub),
                        I::FmulFast | I::FmulAlgebraic => Some(BinOp::Mul),
                        I::FdivFast | I::FdivAlgebraic => Some(BinOp::Div),
                        I::FremFast | I::FremAlgebraic => Some(BinOp::Rem),
                        _ => {
                            // alloc: the error's message
                            let why = format!("intrinsic `{intrinsic}` on {:?}", scalar.ty);
                            return Err(unsupported(why));
                        }
                    };
                    // `soft_copysign` moves bits.
                    if let Some(op) = op {
                        self.soft_binary(&op, scalar.ty)?;
                    }
                    return Ok(true);
                }
                let single = scalar.ty == types::F32;
                match intrinsic {
                    I::Powf => self.symbol(if single { "powf" } else { "pow" })?,
                    I::FremFast | I::FremAlgebraic => {
                        if !single && scalar.ty != types::F64 {
                            // alloc: the error's message
                            let why = format!("intrinsic `{intrinsic}` on {:?}", scalar.ty);
                            return Err(unsupported(why));
                        }
                        self.float_binary(&BinOp::Rem, scalar.ty)?;
                    }
                    _ => (),
                }
            }
            (I::Fma | I::Fmuladd, [_, _, _])
                if self.fx.x86_fused() && self.ty_scalar(dst_ty)?.ty != types::F128 =>
            {
                self.x86_fma(intrinsic, args, dst_ty)?
            }
            (I::Fma | I::Fmuladd, [_, _, _]) => {
                let scalar = self.ty_scalar(arg_tys[0])?;
                if soft(scalar.ty) {
                    self.soft_fma(scalar.ty)?;
                } else if intrinsic != I::Fmuladd {
                    self.symbol(if scalar.ty == types::F32 { "fmaf" } else { "fma" })?;
                }
            }
            (I::CatchUnwind, [_, _, _]) => {
                let outcome = self.ty_layout(dst_ty)?.size.bytes();
                if ir::Type::int_with_byte_size(outcome as u16).is_none() {
                    // alloc: the error's message
                    return Err(unsupported(format!("`catch_unwind` giving {outcome} bytes")));
                }
                // The calls of the closures go through the pointers MIR passes, whose
                // functions their reification listed; the landing pad is `outside`'s. With
                // SEH what was caught is released.
                if !self.fx.cg.target.exception_in_register() {
                    self.symbol(crate::unwind::seh::RELEASE)?;
                }
            }
            // Instructions of their operands, constants and layouts, and data.
            (
                I::Ctpop
                | I::Ctlz
                | I::CtlzNonzero
                | I::Cttz
                | I::CttzNonzero
                | I::Bswap
                | I::Bitreverse,
                [_],
            )
            | (I::SizeOf | I::AlignOf, [])
            | (I::NeedsDrop | I::UbChecks | I::OverflowChecks | I::CallerLocation, [])
            | (I::ReturnAddress, [])
            | (I::Transmute | I::TransmuteUnchecked | I::BlackBox, [_])
            | (I::Atomic(A::Fence | A::SingleThreadFence, _), [])
            | (I::ReadViaCopy | I::VolatileLoad | I::UnalignedVolatileLoad, [_])
            | (I::WriteViaMove | I::VolatileStore | I::UnalignedVolatileStore, [_, _])
            | (I::WriteBoxViaMove, [_, _])
            | (I::Offset | I::ArithOffset, [_, _])
            | (I::PtrMask, [_, _])
            | (I::PtrOffsetFrom | I::PtrOffsetFromUnsigned, [_, _])
            | (I::Copy | I::CopyNonoverlapping | I::WriteBytes, [_, _, _])
            | (I::Simd(S::Select | S::SelectBitmask), [_, _, _])
            | (I::Simd(S::Extract), [_, _])
            | (I::Simd(S::Insert), [_, _, _])
            | (I::Simd(S::Shuffle), [_, _, _])
            | (I::Simd(S::Splat), [_])
            | (I::Simd(S::ReduceAll | S::ReduceAny), [_])
            | (I::Simd(S::MaskedLoad | S::MaskedStore), [_, _, _])
            | (I::DiscriminantValue, [_])
            | (I::PtrMetadata, [_])
            | (I::AggregateRawPtr, [_, _])
            | (I::SliceGetUnchecked, [_, _])
            | (I::VtableSize | I::VtableAlign, [_])
            | (I::SizeOfVal | I::AlignOfVal, [_]) => (),
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// `FunctionCx::llvm_intrinsic`: a `#[link_name = "llvm.*"]` function, `name` without
    /// `llvm.`.
    pub(super) fn llvm_intrinsic(
        &mut self,
        name: &str,
        args: &[Operand],
        dst_ty: Ty<'db>,
    ) -> Result<(), String> {
        let arg_tys = self.operands(args)?;
        match self.llvm_lowered(name, args, &arg_tys, dst_ty) {
            // `wasm_llvm_intrinsic` makes wasm's instructions of what it knows, which ask
            // for nothing.
            Err(_) if self.fx.cg.target.is_wasm() => Ok(()),
            done => done,
        }
    }

    fn llvm_lowered(
        &mut self,
        name: &str,
        args: &[Operand],
        arg_tys: &[Ty<'db>],
        dst_ty: Ty<'db>,
    ) -> Result<(), String> {
        let arch = self.fx.cg.target.arch;
        if let Some(neon) = name.strip_prefix("aarch64.neon.")
            && neon_pairwise(neon, args.len())
        {
            return Ok(());
        }
        if name.starts_with("aarch64.sve.")
            || name.starts_with("vector.reverse.nxv")
            || name.starts_with("vector.insert.nxv")
        {
            return self.sve_intrinsic(name, args, arg_tys, dst_ty);
        }
        match (name, args) {
            (
                "x86.sse.cmp.ps" | "x86.sse2.cmp.pd" | "x86.avx.cmp.ps.256" | "x86.avx.cmp.pd.256"
                | "x86.sse.cmp.ss" | "x86.sse2.cmp.sd",
                [_, _, predicate],
            ) => {
                if self.const_immediate(predicate)?.is_none() {
                    // alloc: the error's message
                    let why = format!("`llvm.{name}` with a predicate that is not a constant");
                    return Err(unsupported(why));
                }
            }
            (
                "x86.sse41.round.ps"
                | "x86.avx.round.ps.256"
                | "x86.sse41.round.pd"
                | "x86.avx.round.pd.256",
                [_, mode],
            ) => {
                let Some(mode) = self.const_immediate(mode)? else {
                    // alloc: the error's message
                    let why = format!("LLVM intrinsic `{name}` with a mode that is not a constant");
                    return Err(unsupported(why));
                };
                let double = name.ends_with("pd") || name.contains(".pd.");
                let lane = if double { types::F64 } else { types::F32 };
                let rounding = match if mode & 4 != 0 { 0 } else { mode & 3 } {
                    0 => Routine::Nearest,
                    1 => Routine::Floor,
                    2 => Routine::Ceil,
                    _ => Routine::Trunc,
                };
                self.cranelift_routine(rounding, lane)?;
            }
            (
                "x86.sha1msg1" | "x86.sha1msg2" | "x86.sha1nexte" | "x86.sha256msg1"
                | "x86.sha256msg2",
                [_, _],
            ) => {
                let instruction = match name {
                    "x86.sha1msg1" => "sha1msg1 xmm0, xmm1",
                    "x86.sha1msg2" => "sha1msg2 xmm0, xmm1",
                    "x86.sha1nexte" => "sha1nexte xmm0, xmm1",
                    "x86.sha256msg1" => "sha256msg1 xmm0, xmm1",
                    _ => "sha256msg2 xmm0, xmm1",
                };
                self.assembly(asm::vector_instruction(instruction, "xmm0"))?;
            }
            ("x86.sha256rnds2", [_, _, _]) => {
                let rounds = "movdqa xmm3, xmm0\nmovdqa xmm0, xmm2\nsha256rnds2 xmm3, xmm1";
                self.assembly(asm::vector_instruction(rounds, "xmm3"))?;
            }
            (
                "aarch64.crypto.sha256su0"
                | "aarch64.crypto.sha512su0"
                | "aarch64.crypto.sha1su1"
                | "aarch64.crypto.aese"
                | "aarch64.crypto.aesd",
                [_, _],
            )
            | ("aarch64.crypto.aesmc" | "aarch64.crypto.aesimc", [_])
            | (
                "aarch64.crypto.sha256h"
                | "aarch64.crypto.sha256h2"
                | "aarch64.crypto.sha256su1"
                | "aarch64.crypto.sha512h"
                | "aarch64.crypto.sha512h2"
                | "aarch64.crypto.sha512su1"
                | "aarch64.crypto.sha1su0",
                [_, _, _],
            ) => self.assembly(crate::neon_crypto(name))?,
            ("x86.sha1rnds4", [_, _, _]) => self.assembly(asm::sha1rnds4())?,
            ("x86.xgetbv", [_]) => self.assembly(asm::xgetbv())?,
            (
                "aarch64.crc32b" | "aarch64.crc32h" | "aarch64.crc32w" | "aarch64.crc32x"
                | "aarch64.crc32cb" | "aarch64.crc32ch" | "aarch64.crc32cw" | "aarch64.crc32cx",
                [_, _],
            ) => self.assembly(asm::crc32(&name["aarch64.".len()..]))?,
            (
                "x86.sse42.crc32.32.8"
                | "x86.sse42.crc32.32.16"
                | "x86.sse42.crc32.32.32"
                | "x86.sse42.crc32.64.64",
                [_, _],
            ) => {
                let data = self.ty_scalar(arg_tys[1])?;
                self.assembly(asm::x86_crc32(data.ty.bits()))?;
            }
            ("aarch64.dmb" | "aarch64.dsb" | "aarch64.hint", [option]) => {
                let Some(option) = self.const_immediate(option)? else {
                    // alloc: the error's message
                    let why = format!("`llvm.{name}` of an option that is not a constant");
                    return Err(unsupported(why));
                };
                let (word, most, at) = match name {
                    "aarch64.dmb" => (0xd503_30bf_u32, 15, 8),
                    "aarch64.dsb" => (0xd503_309f, 15, 8),
                    _ => (0xd503_201f, 127, 5),
                };
                if option > most {
                    // alloc: the error's message
                    return Err(unsupported(format!("`llvm.{name}` of option {option}")));
                }
                self.assembly(asm::instruction_word(word | (option as u32) << at))?;
            }
            ("aarch64.rndr" | "aarch64.rndrrs", []) => {
                let layout = self.ty_layout(dst_ty)?;
                if layout.size.bytes() != 16 || layout.fields.offset(1).bytes() != 8 {
                    // alloc: the error's message
                    return Err(unsupported(format!("`llvm.{name}` returning {dst_ty:?}")));
                }
                self.assembly(asm::random(name == "aarch64.rndrrs"))?;
            }
            ("x86.sse2.pause", []) => self.assembly(asm::pause())?,
            ("aarch64.isb", [_]) => self.assembly(asm::isb())?,
            ("x86.vcvtps2ph.128" | "x86.vcvtps2ph.256", [_, _]) => {
                let register = if name.ends_with("256") { "ymm0" } else { "xmm0" };
                self.assembly(asm::vcvtps2ph(register))?;
            }
            (_, [_, _, _]) if name.starts_with("x86.avx512.vpermi2var.") => {
                if !matches!(name.split('.').nth(3), Some("qi" | "hi" | "d" | "ps" | "q" | "pd")) {
                    // alloc: the error's message
                    return Err(unsupported(format!("LLVM intrinsic `{name}`")));
                }
            }
            // Instructions of their operands.
            ("x86.sse3.ldu.dq" | "x86.avx.ldu.dq.256", [_])
            | ("x86.ssse3.pshuf.b.128" | "x86.avx2.pshuf.b" | "x86.avx512.pshuf.b.512", [_, _])
            | ("x86.avx2.permd" | "x86.avx2.permps", [_, _])
            | ("x86.sse2.psad.bw" | "x86.avx2.psad.bw" | "x86.avx512.psad.bw.512", [_, _])
            | ("x86.sse2.pmadd.wd" | "x86.avx2.pmadd.wd" | "x86.avx512.pmaddw.d.512", [_, _])
            | (
                "x86.ssse3.pmadd.ub.sw.128" | "x86.avx2.pmadd.ub.sw" | "x86.avx512.pmaddubs.w.512",
                [_, _],
            )
            | (
                "x86.sse.min.ps" | "x86.sse.max.ps" | "x86.avx.min.ps.256" | "x86.avx.max.ps.256"
                | "x86.sse2.min.pd" | "x86.sse2.max.pd" | "x86.avx.min.pd.256"
                | "x86.avx.max.pd.256",
                [_, _],
            )
            | ("x86.sse2.cvttps2dq" | "x86.avx.cvtt.ps2dq.256", [_])
            | ("x86.sse2.psrl.w" | "x86.avx2.psrl.w", [_, _])
            | ("x86.sse2.packsswb.128" | "x86.avx2.packsswb" | "x86.avx512.packsswb.512", [_, _])
            | ("x86.sse2.packuswb.128" | "x86.avx2.packuswb" | "x86.avx512.packuswb.512", [_, _])
            | ("x86.sse2.packssdw.128" | "x86.avx2.packssdw" | "x86.avx512.packssdw.512", [_, _])
            | ("x86.sse41.packusdw" | "x86.avx2.packusdw" | "x86.avx512.packusdw.512", [_, _])
            | ("x86.pclmulqdq" | "x86.pclmulqdq.256" | "x86.pclmulqdq.512", [_, _, _])
            | ("x86.sse41.phminposuw", [_])
            | ("x86.avx.vzeroupper", [])
            | ("prefetch.p0", [_, _, _, _]) => (),
            (_, [_, _]) if name.starts_with("x86.avx512.permvar.") => (),
            (_, [_, _]) if crate::x86_lanewise(name).is_some() => (),
            (_, [_, _, _, _]) if name.starts_with("x86.avx512.pternlog.") => (),
            (_, [_, _, _])
                if name.starts_with("x86.avx512.vpmadd52l.uq.")
                    || name.starts_with("x86.avx512.vpmadd52h.uq.") => {}
            _ if arch == Arch::Aarch64
                && let Some(lowered) = neon::lower(name) =>
            {
                self.neon_assembly(name, &lowered, args, dst_ty)?
            }
            _ if arch == Arch::X86_64
                && [
                    "x86.avx512.",
                    "x86.avx512fp16.",
                    "x86.avx512bf16.",
                    "x86.avx2.vpdp",
                    "x86.vcvtne",
                    "x86.vbcstne",
                ]
                .iter()
                .any(|it| name.starts_with(it))
                && self.avx512_assembly(name, args, arg_tys, dst_ty)? => {}
            _ if arch == Arch::X86_64
                && let Some(lowered) = x86::lower(name) =>
            {
                self.x86_assembly(name, &lowered, args, arg_tys, dst_ty)?
            }
            // alloc: the error's message
            _ => return Err(unsupported(format!("LLVM intrinsic `{name}`"))),
        }
        Ok(())
    }

    /// What `x86_inlined_fma` makes of a call of `func` in a function of core with FMA: the
    /// fma of the wrapper rustc's MIR inliner puts into it, in any of its forms. Whether `func`
    /// is such a wrapper is `fma.rs`'s to say, so every call it could be is taken as one.
    pub(super) fn inlined_fma(&mut self, func: FunctionId, dst_ty: Ty<'db>) -> Result<(), String> {
        let db = self.db();
        let caller = self.fx.body.and_then(|it| it.owner.as_function());
        if !self.fx.x86_fused()
            || FunctionSignature::is_intrinsic(db, func)
            || !caller.is_some_and(|it| it.krate(db).data(db).origin.is_lang())
        {
            return Ok(());
        }
        match self.fma_shape(Intrinsic::Simd(SimdOp::Fma), dst_ty) {
            Ok((bytes, suffix)) => self.fused_forms(bytes, suffix),
            // `lower_fma` refuses it, if it is made at all.
            Err(_) => Ok(()),
        }
    }

    /// `x86_fma`: the fma of the intrinsic call in a function with FMA.
    fn x86_fma(
        &mut self,
        name: Intrinsic,
        args: &[Operand],
        dst_ty: Ty<'db>,
    ) -> Result<(), String> {
        if args.len() != 3 {
            // alloc: the error's message
            return Err(unsupported(format!("`{name}` with {} arguments", args.len())));
        }
        if matches!(name, Intrinsic::Simd(SimdOp::RelaxedFma) | Intrinsic::Fmuladd) {
            // alloc: the error's message
            return Err(unsupported(format!(
                "`{name}` in an x86-64 function with FMA, which LLVM may or may not fuse there"
            )));
        }
        let (bytes, suffix) = self.fma_shape(name, dst_ty)?;
        self.fused_forms(bytes, suffix)
    }

    /// What `lower_fma` refuses of an fma giving `dst_ty`, and else its bytes and suffix.
    fn fma_shape(
        &self,
        name: Intrinsic,
        dst_ty: Ty<'db>,
    ) -> Result<(u32, &'static str), String> {
        if self.enables_feature(TargetFeature::FMA4) {
            // alloc: the error's message
            return Err(unsupported(format!("`{name}` in an x86-64 function with FMA4")));
        }
        let layout = self.ty_layout(dst_ty)?;
        let bytes = layout.size.bytes() as u32;
        let (lane, scalar) = match layout.backend_repr {
            BackendRepr::Scalar(scalar) => (self.fx.scalar(scalar).map_err(told)?.ty, true),
            BackendRepr::SimdVector { element, .. } => {
                (self.fx.scalar(element).map_err(told)?.ty, false)
            }
            // alloc: the error's message
            _ => return Err(unsupported(format!("`{name}` of {dst_ty:?}"))),
        };
        if !lane.is_float() || lane == types::F128 {
            // alloc: the error's message
            return Err(unsupported(format!("`{name}` of {lane} in an x86-64 function with FMA")));
        }
        if lane == types::F16 && !self.enables_feature(TargetFeature::AVX512FP16) {
            // alloc: the error's message
            return Err(unsupported(format!(
                "`{name}` of f16 in an x86-64 function with FMA and without AVX512-FP16, \
                 which LLVM fuses by its context"
            )));
        }
        let vl = lane == types::F16
            && !scalar
            && bytes < 64
            && !self.enables_feature(TargetFeature::AVX512VL);
        if !scalar && !matches!(bytes, 16 | 32 | 64)
            || bytes == 64 && !self.enables_feature(TargetFeature::AVX512F)
            || vl
        {
            // alloc: the error's message
            let why = format!("`{name}` of {dst_ty:?} in an x86-64 function with FMA");
            return Err(unsupported(why));
        }
        let suffix = match (lane, scalar) {
            (types::F16, true) => "sh",
            (types::F32, true) => "ss",
            (types::F64, true) => "sd",
            (types::F16, false) => "ph",
            (types::F32, false) => "ps",
            _ => "pd",
        };
        Ok((bytes, suffix))
    }

    /// The functions of each form `lower_fma` may give an fma of `bytes` and `suffix`, as
    /// `call_assembled` makes them.
    fn fused_forms(&mut self, bytes: u32, suffix: &str) -> Result<(), String> {
        let loads = [avx512::Load::Vector(bytes); 3];
        for text in fma_texts(bytes, suffix) {
            self.assembly(asm::avx512_function(&loads, &text, avx512::Out::Vector(bytes)))?;
        }
        Ok(())
    }

    /// `fma.rs`'s `enables`: whether the function's crate or the function has `feature`.
    fn enables_feature(&self, feature: TargetFeature) -> bool {
        hir_ty::crate_target_features(self.db(), self.fx.env.krate).contains(feature)
            || self.fx.function_enables(feature)
    }

    /// `FunctionCx::half_lanes` of the operands' types.
    fn half_lanes(&self, arg_tys: &[Ty<'db>], dst_ty: Ty<'db>) -> Result<bool, String> {
        for ty in arg_tys.first().copied().into_iter().chain([dst_ty]) {
            if let BackendRepr::SimdVector { element, .. } = self.ty_layout(ty)?.backend_repr
                && let Primitive::Float(rustc_abi::Float::F16) = element.primitive()
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// `FunctionCx::half_vector` and `x86_half_vector`: the instruction of a generic intrinsic
    /// on vectors of `f16`.
    fn half_vector(
        &mut self,
        op: SimdOp,
        args: &[Operand],
        arg_tys: &[Ty<'db>],
        dst_ty: Ty<'db>,
    ) -> Result<(), String> {
        let name = Intrinsic::Simd(op);
        let Some(&ty) = arg_tys.first() else {
            // alloc: the error's message
            return Err(unsupported(format!("`{name}` without operands")));
        };
        let (a, a_signed) = self.fx.neon_lanes_of(ty).map_err(told)?;
        let (r, r_signed) = self.fx.neon_lanes_of(dst_ty).map_err(told)?;
        let widths = (r.float && a.float).then_some((r.bits, a.bits));
        if self.on_x86() {
            // `vpxor` of the sign of each lane.
            if op == SimdOp::Neg
                && self.fx.function_enables(TargetFeature::AVX512FP16)
                && a.float
                && a.bits == 16
            {
                if args.len() != 1 {
                    // alloc: the error's message
                    return Err(unsupported(format!("`simd_neg` with {} arguments", args.len())));
                }
                return Ok(());
            }
            let lowered = avx512::half_vector(op, a.count * a.bits / 8, r.count * r.bits / 8, |it| {
                self.fx.function_enables(it)
            });
            let Some(lowered) = lowered.filter(|it| it.loads.len() == args.len()) else {
                // alloc: the error's message
                return Err(unsupported(format!(
                    "`{name}` of lanes of f16 on x86-64 without an instruction for it here"
                )));
            };
            return self.assembly(asm::avx512_function(&lowered.loads, &lowered.text, lowered.out));
        }
        let single_and_half = op == SimdOp::Cast && matches!(widths, Some((16, 32) | (32, 16)));
        if self.fx.cg.target.arch != Arch::Aarch64
            || !(single_and_half || self.fx.half_precision())
        {
            return Err(unsupported(
                "vector operation on lanes of f16 without the half-precision instructions",
            ));
        }
        let signed = if a.float { r_signed } else { a_signed };
        let Some(lowered) = neon::half_vector(op, r, a, signed) else {
            // alloc: the error's message
            return Err(unsupported(format!("`{name}` of lanes of f16 into {dst_ty:?}")));
        };
        self.neon_assembly(&name, &lowered, args, dst_ty)
    }

    /// `neon_assembly`: the function of `lowered`'s instructions, its immediate put in. `name`
    /// is the intrinsic's, for errors.
    fn neon_assembly(
        &mut self,
        name: &(impl fmt::Display + ?Sized),
        lowered: &neon::Lowered,
        args: &[Operand],
        dst_ty: Ty<'db>,
    ) -> Result<(), String> {
        // alloc: inline up to eight operands
        let mut args: SmallVec<[&Operand; 8]> = args.iter().collect();
        let mut with_immediate = None;
        if let Some(immediate) = &lowered.immediate {
            if immediate.arg >= args.len() {
                // alloc: the error's message
                return Err(unsupported(format!("`{name}` with {} arguments", args.len())));
            }
            let arg = args.remove(immediate.arg);
            let value = match immediate.splat {
                Some(lane) => self.splat_immediate(arg, lane)?,
                None => self.const_immediate(arg)?,
            };
            let range = &immediate.range;
            let Some(value) =
                value.and_then(|it| u32::try_from(it).ok()).filter(|it| range.contains(it))
            else {
                // alloc: the error's message
                return Err(unsupported(format!(
                    "`{name}` with an immediate that is not a constant of {}..={}",
                    range.start(),
                    range.end()
                )));
            };
            with_immediate = Some(put_immediate(&lowered.text, value));
        }
        // alloc: into `Tys`, inline up to eight operands
        let arg_tys: Tys<'db> = args.iter().map(|arg| self.operand_ty(arg)).collect();
        let text = with_immediate.as_deref().unwrap_or(lowered.text.as_str());
        let text = self.neon_text(name, lowered, text, &arg_tys, dst_ty)?;
        self.assembly(text)
    }

    /// The text `neon_call` assembles for `lowered`, whose instructions are `text`, on operands
    /// of `arg_tys`.
    fn neon_text(
        &self,
        name: &(impl fmt::Display + ?Sized),
        lowered: &neon::Lowered,
        text: &str,
        arg_tys: &[Ty<'db>],
        dst_ty: Ty<'db>,
    ) -> Result<String, String> {
        if arg_tys.len() != lowered.args.len() {
            // alloc: the error's message
            return Err(unsupported(format!("`{name}` with {} arguments", arg_tys.len())));
        }
        for (&ty, expected) in arg_tys.iter().zip(&lowered.args) {
            let pointer = matches!(ty.kind(), TyKind::RawPtr(..));
            let fits = match expected {
                None => pointer,
                Some(bytes) => !pointer && self.ty_layout(ty)?.size.bytes() == u64::from(*bytes),
            };
            if !fits {
                // alloc: the error's message
                return Err(unsupported(format!(
                    "`{name}` of {ty:?}, which is not what its name says"
                )));
            }
        }
        let layout = self.ty_layout(dst_ty)?;
        let size: u64 = lowered.results.iter().map(|&it| u64::from(it)).sum();
        let results: SmallVec<[(u32, u32); 4]> = match lowered.results.as_slice() {
            [] if layout.is_zst() => SmallVec::new(),
            &[bytes] if layout.size.bytes() == u64::from(bytes) => {
                SmallVec::from_slice(&[(0, bytes)])
            }
            many if layout.size.bytes() == size
                && layout.fields.count() == many.len()
                && (0..many.len())
                    .all(|i| layout.fields.offset(i).bytes() == i as u64 * u64::from(many[0])) =>
            {
                // alloc: inline up to four results
                many.iter().enumerate().map(|(i, &bytes)| (i as u32 * bytes, bytes)).collect()
            }
            _ => {
                // alloc: the error's message
                return Err(unsupported(format!(
                    "`{name}` returning {dst_ty:?}, which is not what its name says"
                )));
            }
        };
        let directives = asm::extensions(Arch::Aarch64, lowered.features.iter().copied());
        Ok(asm::neon_function(&directives, &lowered.args, text, &results))
    }

    /// `FunctionCx::immediate`, of the walk's types.
    fn const_immediate(&mut self, operand: &Operand) -> Result<Option<u128>, String> {
        if let Some((bits, _)) = self.fx.literal_bits(operand) {
            return Ok(Some(bits));
        }
        let ty = self.operand_ty(operand);
        if !matches!(ty.kind(), TyKind::Int(_) | TyKind::Uint(_)) {
            return Ok(None);
        }
        let bits = self.ty_layout(ty)?.size.bits();
        let mask = u128::MAX >> (128 - bits);
        match &operand.kind {
            OperandKind::Constant { konst, .. } => {
                let ConstKind::Alias(_, named) = konst.as_ref().kind() else {
                    return Ok(None);
                };
                let allocation = self.recorded(|fx| fx.eval_const(named))?;
                let Some(memory) = allocation.memory.get(..bits as usize / 8) else {
                    return Ok(None);
                };
                let mut bytes = [0; 16];
                bytes[..memory.len()].copy_from_slice(memory);
                Ok(Some(u128::from_le_bytes(bytes)))
            }
            OperandKind::Copy(_) | OperandKind::Move(_) => {
                let Some(Rvalue::Use(from) | Rvalue::Cast(CastKind::IntToInt, from, _)) =
                    self.fx.assigned_once(operand)
                else {
                    return Ok(None);
                };
                let from_ty = self.operand_ty(from);
                let Some(value) = self.const_immediate(from)? else { return Ok(None) };
                let unused = 128 - self.ty_layout(from_ty)?.size.bits() as u32;
                let value = match from_ty.kind() {
                    TyKind::Int(_) => ((value << unused) as i128 >> unused) as u128,
                    _ => value,
                };
                Ok(Some(value & mask))
            }
            _ => Ok(None),
        }
    }

    /// `FunctionCx::splat_immediate`, of the walk's types.
    fn splat_immediate(&mut self, operand: &Operand, lane: u32) -> Result<Option<u128>, String> {
        let (blob, interned);
        let memory: &[u8] = match &operand.kind {
            OperandKind::Allocation { allocation } => {
                interned = allocation.as_ref();
                &interned.memory[..]
            }
            OperandKind::Constant { konst, ty } => match konst.as_ref().kind() {
                ConstKind::Value(value) => {
                    blob = self.recorded(|fx| fx.valtree_blob(ty.as_ref(), value.value))?.bytes;
                    &blob[..]
                }
                ConstKind::Alias(_, named) => {
                    interned = self.recorded(|fx| fx.eval_const(named))?;
                    &interned.memory[..]
                }
                _ => return Ok(None),
            },
            OperandKind::Copy(_) | OperandKind::Move(_) => {
                return match self.fx.assigned_once(operand) {
                    Some(Rvalue::Use(from)) => self.splat_immediate(from, lane),
                    Some(Rvalue::Aggregate(AggregateKind::Adt(..), fields)) if fields.len() == 1 => {
                        self.splat_immediate(&fields[0], lane)
                    }
                    Some(Rvalue::Repeat(from, _)) => {
                        let ty = self.operand_ty(from);
                        if self.ty_layout(ty)?.size.bytes() != u64::from(lane) {
                            return Ok(None);
                        }
                        self.const_immediate(from)
                    }
                    _ => Ok(None),
                };
            }
            _ => return Ok(None),
        };
        let ty = self.operand_ty(operand);
        let size = self.ty_layout(ty)?.size.bytes_usize();
        let Some(memory) = memory.get(..size) else { return Ok(None) };
        let mut lanes = memory.chunks_exact(lane as usize);
        let Some(first) = lanes.next() else { return Ok(None) };
        if !lanes.remainder().is_empty() || lanes.any(|it| it != first) {
            return Ok(None);
        }
        let mut bytes = [0; 16];
        bytes[..first.len()].copy_from_slice(first);
        Ok(Some(u128::from_le_bytes(bytes)))
    }

    /// `avx512_assembly`: false where `avx512::lower` knows no instruction for `name`.
    fn avx512_assembly(
        &mut self,
        name: &str,
        args: &[Operand],
        arg_tys: &[Ty<'db>],
        dst_ty: Ty<'db>,
    ) -> Result<bool, String> {
        let mut given = Vec::with_capacity(args.len());
        for (arg, &ty) in args.iter().zip(arg_tys) {
            given.push(avx512::Given {
                bytes: self.ty_layout(ty)?.size.bytes() as u32,
                pointer: matches!(ty.kind(), TyKind::RawPtr(..)),
                constant: self.const_immediate(arg)?,
            });
        }
        let layout = self.ty_layout(dst_ty)?;
        let func = self.fx.body.and_then(|it| it.owner.as_function());
        let features = func.map_or(hir_ty::TargetFeatureSet::EMPTY, |func| {
            hir_ty::TargetFeatures::from_fn_no_implications(self.db(), func).known()
        });
        let lowered = match avx512::lower(name, &given, layout.size.bytes() as u32, features) {
            None => return Ok(false),
            Some(lowered) => lowered.map_err(unsupported)?,
        };
        if let avx512::Out::Masks(_, at) = lowered.out
            && (layout.fields.count() != 2 || layout.fields.offset(1).bytes() != u64::from(at))
        {
            // alloc: the error's message
            return Err(unsupported(format!("`llvm.{name}` returning {dst_ty:?}")));
        }
        self.assembly(asm::avx512_function(&lowered.loads, &lowered.text, lowered.out))?;
        Ok(true)
    }

    /// `x86_assembly`: the function of `lowered`'s instruction, its immediate put in.
    fn x86_assembly(
        &mut self,
        name: &str,
        lowered: &x86::Lowered,
        args: &[Operand],
        arg_tys: &[Ty<'db>],
        dst_ty: Ty<'db>,
    ) -> Result<(), String> {
        if args.len() != lowered.args.len() {
            // alloc: the error's message
            return Err(unsupported(format!("`{name}` with {} arguments", args.len())));
        }
        let mut text = Cow::Borrowed(lowered.text.as_str());
        let mut reads = Vec::with_capacity(args.len());
        let mut size = 0;
        for ((arg, &ty), register) in args.iter().zip(arg_tys).zip(&lowered.args) {
            let Some(register) = register else {
                let Some(immediate) = self.const_immediate(arg)?.filter(|it| *it <= 0xff) else {
                    // alloc: the error's message
                    let why = format!("`{name}` with an immediate that is not a constant byte");
                    return Err(unsupported(why));
                };
                text = Cow::Owned(put_immediate(&text, immediate));
                continue;
            };
            let bytes = asm::x86_register_bytes(register).expect("a register of x86");
            if self.ty_layout(ty)?.size.bytes() != u64::from(bytes) {
                // alloc: the error's message
                return Err(unsupported(format!(
                    "`{name}` of {ty:?}, which is not what its name says"
                )));
            }
            reads.push((*register, size));
            size += bytes;
        }
        let layout = self.ty_layout(dst_ty)?;
        let register_bytes = |it: &str| u64::from(asm::x86_register_bytes(it).expect("x86's"));
        let writes: SmallVec<[(&str, u32); 4]> = match lowered.results.as_slice() {
            [] if layout.is_zst() => SmallVec::new(),
            &[register] if layout.size.bytes() == register_bytes(register) => {
                SmallVec::from_slice(&[(register, 0)])
            }
            many => {
                let fields: Tys<'db> = match dst_ty.kind() {
                    // alloc: into `Tys`, inline up to eight fields
                    TyKind::Tuple(tys) => tys.iter().collect(),
                    TyKind::Adt(adt_def, subst) if let AdtId::StructId(id) = adt_def.def_id() => {
                        let field_types = self.db().field_types(id.into());
                        field_types
                            .iter()
                            .map(|(_, it)| {
                                let ty =
                                    it.ty().instantiate(self.fx.interner, subst).skip_norm_wip();
                                self.fx.normalize(ty)
                            })
                            // alloc: into `Tys`, inline up to eight fields
                            .collect()
                    }
                    _ => Tys::new(),
                };
                let mut writes = SmallVec::with_capacity(many.len());
                for (i, (&register, &field)) in many.iter().zip(&fields).enumerate() {
                    if self.ty_layout(field)?.size.bytes() != register_bytes(register) {
                        break;
                    }
                    writes.push((register, layout.fields.offset(i).bytes() as u32));
                }
                if fields.len() != many.len() || writes.len() != many.len() {
                    // alloc: the error's message
                    return Err(unsupported(format!(
                        "`{name}` returning {dst_ty:?}, which is not what its name says"
                    )));
                }
                writes
            }
        };
        let text =
            asm::x86_function(&text, &reads, &writes, &lowered.clobbers).map_err(unsupported)?;
        self.assembly(text)
    }

    /// `sve_intrinsic`: the stub of an SVE intrinsic, by the kinds of its operands.
    fn sve_intrinsic(
        &mut self,
        name: &str,
        args: &[Operand],
        arg_tys: &[Ty<'db>],
        dst_ty: Ty<'db>,
    ) -> Result<(), String> {
        let mut kinds = Vec::with_capacity(args.len());
        for (arg, &ty) in args.iter().zip(arg_tys) {
            if let Some(shape) = self.fx.scalable(ty).map_err(told)? {
                kinds.push(sve::Arg::Vector(shape));
                continue;
            }
            let layout = self.ty_layout(ty)?;
            if let Some(shape) = sve::Shape::fixed(&layout) {
                kinds.push(sve::Arg::Fixed(shape));
                continue;
            }
            let BackendRepr::Scalar(_) = layout.backend_repr else {
                // alloc: the error's message
                return Err(unsupported(format!(
                    "LLVM intrinsic `{name}` with an operand of {ty:?}"
                )));
            };
            let constant = self.sve_constant(arg, ty)?;
            let scalar = self.ty_scalar(ty)?;
            let bits = u64::from(scalar.ty.bits());
            kinds.push(match scalar.ty.is_float() {
                true => sve::Arg::Float(bits),
                false => sve::Arg::Int(bits, constant),
            });
        }
        let ret = if let Some(shape) = self.fx.scalable(dst_ty).map_err(told)? {
            sve::Ret::Vector(shape)
        } else if self.ty_layout(dst_ty)?.is_zst() {
            sve::Ret::Unit
        } else {
            let scalar = self.ty_scalar(dst_ty)?;
            let bits = u64::from(scalar.ty.bits());
            if scalar.ty.is_float() { sve::Ret::Float(bits) } else { sve::Ret::Int(bits) }
        };
        let text = sve::stub(name, &kinds, ret).map_err(unsupported)?;
        self.assembly(text)
    }

    /// `sve_constant`, of the walk's types.
    fn sve_constant(&mut self, operand: &Operand, ty: Ty<'db>) -> Result<Option<i128>, String> {
        let layout = self.ty_layout(ty)?;
        let BackendRepr::Scalar(scalar) = layout.backend_repr else { return Ok(None) };
        let size = layout.size;
        let signed = matches!(scalar.primitive(), Primitive::Int(_, true));
        let bits = match ty.kind() {
            TyKind::Int(_) | TyKind::Uint(_) => self.const_immediate(operand)?,
            TyKind::Adt(..) => {
                let (blob, interned);
                let bytes: Option<&[u8]> = match &operand.kind {
                    OperandKind::Constant { konst, .. } => match konst.as_ref().kind() {
                        ConstKind::Value(value) => {
                            let made = self.recorded(|fx| fx.valtree_blob(ty, value.value))?;
                            match made.relocs.is_empty() && made.fn_relocs.is_empty() {
                                true => {
                                    blob = made.bytes;
                                    Some(&blob[..])
                                }
                                false => None,
                            }
                        }
                        ConstKind::Alias(_, named) => {
                            interned = self.recorded(|fx| fx.eval_const(named))?;
                            Some(&interned.memory[..])
                        }
                        _ => None,
                    },
                    OperandKind::Allocation { allocation } => {
                        interned = allocation.as_ref();
                        Some(&interned.memory[..])
                    }
                    // A const generic argument is moved to a local of its own first.
                    OperandKind::Copy(place) | OperandKind::Move(place)
                        if place.projection.as_slice().is_empty() =>
                    {
                        let Some(Rvalue::Use(from)) = self.fx.assigned_once(operand) else {
                            return Ok(None);
                        };
                        if self.operand_ty(from) != ty {
                            return Ok(None);
                        }
                        return self.sve_constant(from, ty);
                    }
                    _ => None,
                };
                bytes.filter(|it| it.len() >= size.bytes_usize()).map(|it| {
                    let mut all = [0; 16];
                    all[..size.bytes_usize()].copy_from_slice(&it[..size.bytes_usize()]);
                    u128::from_le_bytes(all)
                })
            }
            _ => None,
        };
        Ok(bits.map(|bits| {
            let shift = 128 - size.bits();
            if signed { ((bits << shift) as i128) >> shift } else { bits as i128 }
        }))
    }

    /// The routine Cranelift calls for `op` of a float of `ty` on x86-64.
    fn cranelift_routine(&mut self, op: Routine, ty: ir::Type) -> Result<(), String> {
        if !self.on_x86() {
            return Ok(());
        }
        self.libcall(routine(op, ty))
    }

    /// The compare-and-swap of compiler-builtins an atomic of `ty` is made of, where it is
    /// (`wide_atomics`).
    fn wide_atomic(&mut self, ty: Ty<'db>) -> Result<(), String> {
        let scalar = self.ty_scalar(ty)?;
        if self.fx.wide_atomics(scalar.ty).map_err(told)? {
            self.symbol("__aarch64_cas16_acq_rel")?;
        }
        Ok(())
    }

    /// `divide` of two integers of `scalar`.
    fn int_divide(&mut self, scalar: AbiScalar, div: bool) -> Result<(), String> {
        if scalar.ty != types::I128 {
            return Ok(());
        }
        self.symbol(divide_builtin(div, scalar.signed))
    }

    /// `wide_mul_overflow`.
    fn wide_mul(&mut self, signed: bool) -> Result<(), String> {
        self.symbol(if signed { "__rust_i128_mulo" } else { "__rust_u128_mulo" })
    }

    /// `branch_unless_equal` of two values of `ty`: `memcmp` of bytes, a soft float's `==`.
    /// `seen` are the types already looked at, which ask for the same wherever they are.
    fn equal_parts(&mut self, ty: Ty<'db>, seen: &mut HashSet<Ty<'db>>) -> Result<(), String> {
        let db = self.db();
        let ty = self.fx.normalize(ty);
        if !seen.insert(ty) {
            return Ok(());
        }
        match ty.kind() {
            TyKind::Ref(_, inner, _) | TyKind::Pat(inner, _) => self.equal_parts(inner, seen),
            TyKind::Str => self.symbol("memcmp"),
            TyKind::Slice(element) | TyKind::Array(element, _) => {
                if let TyKind::Array(_, len) = ty.kind()
                    && try_const_usize(db, len).is_none()
                {
                    return Err(unsupported("array with unevaluated length"));
                }
                match element.kind() {
                    TyKind::Int(_) | TyKind::Uint(_) | TyKind::Bool | TyKind::Char => {
                        self.symbol("memcmp")
                    }
                    _ => self.equal_parts(element, seen),
                }
            }
            TyKind::Tuple(tys) => {
                for field in tys.iter() {
                    self.equal_parts(field, seen)?;
                }
                Ok(())
            }
            TyKind::Adt(adt_def, subst) => {
                let layout = self.ty_layout(ty)?;
                let variants: SmallVec<[VariantId; 8]> = match adt_def.def_id() {
                    AdtId::UnionId(_) => return Err(unsupported("comparison of a union")),
                    AdtId::StructId(id) => SmallVec::from_slice(&[id.into()]),
                    AdtId::EnumId(id) => {
                        let all = id.enum_variants(db).variants.values().map(|&(it, _)| it);
                        match &layout.variants {
                            Variants::Empty => SmallVec::new(),
                            Variants::Single { index } => all
                                .filter(|variant| variant.index(db) == index.0)
                                .take(1)
                                .map(VariantId::from)
                                // alloc: one variant, inline
                                .collect(),
                            // Only the variants with fields that compare in parts are
                            // compared field by field.
                            Variants::Multiple { .. } => {
                                let mut stack = Vec::with_capacity(8);
                                all.filter(|&variant| {
                                    stack.clear();
                                    stack.push(ty);
                                    db.field_types(variant.into()).iter().next().is_some()
                                        && self.fx.fields_compare_in_parts(
                                            variant.into(),
                                            subst,
                                            &mut stack,
                                        )
                                })
                                .map(VariantId::from)
                                // alloc: inline up to eight variants
                                .collect()
                            }
                        }
                    }
                };
                for variant in variants {
                    for (_, field) in db.field_types(variant).iter() {
                        let field = field.ty().instantiate(self.fx.interner, subst).skip_norm_wip();
                        self.equal_parts(field, seen)?;
                    }
                }
                Ok(())
            }
            TyKind::RawPtr(pointee, _) if self.fx.is_unsized(pointee) => Ok(()),
            _ => {
                let scalar = self.ty_scalar(ty)?;
                if f16_f128::soft(scalar.ty) {
                    self.soft_binary(&BinOp::Eq, scalar.ty)?;
                }
                Ok(())
            }
        }
    }

    /// `float_cast` of `from` to `to`.
    fn scalar_cast(&mut self, from: AbiScalar, to: AbiScalar) -> Result<(), String> {
        if f16_f128::soft(from.ty) || f16_f128::soft(to.ty) {
            return self.soft_cast(from, to);
        }
        if from.ty == types::I128 || to.ty == types::I128 {
            let Some(name) = wide_int_float(from, to) else {
                // alloc: the error's message
                return Err(unsupported(format!("cast from {} to {}", from.ty, to.ty)));
            };
            return self.symbol(name);
        }
        Ok(())
    }

    /// `soft_cast`.
    fn soft_cast(&mut self, from: AbiScalar, to: AbiScalar) -> Result<(), String> {
        let single = AbiScalar { ty: types::F32, signed: true };
        if from.ty == to.ty {
            return Ok(());
        }
        if self.on_x86() && (from.ty == types::F16 || to.ty == types::F16) {
            self.x86_soft_half()?;
        }
        match (from.ty, to.ty) {
            (types::F16, types::F32 | types::F64) => self.widen_half(to.ty),
            (types::F32 | types::F64, types::F16) => self.narrow_to_half(from.ty),
            (types::F16, types::F128) => self.symbol("__extendhftf2"),
            (types::F32, types::F128) => self.symbol("__extendsftf2"),
            (types::F64, types::F128) => self.symbol("__extenddftf2"),
            (types::F128, types::F16) => self.symbol("__trunctfhf2"),
            (types::F128, types::F32) => self.symbol("__trunctfsf2"),
            (types::F128, types::F64) => self.symbol("__trunctfdf2"),
            (types::F16, _) => {
                self.widen_half(types::F32)?;
                self.scalar_cast(single, to)
            }
            (types::I128, types::F16) if self.on_x86() => {
                self.symbol(if from.signed { "__floattihf" } else { "__floatuntihf" })
            }
            (_, types::F16) => {
                self.scalar_cast(from, single)?;
                self.narrow_to_half(types::F32)
            }
            // `quad_to_int`: the conversion, checked against the bounds by comparisons.
            (types::F128, _) => {
                self.soft_binary(&BinOp::Ge, types::F128)?;
                self.symbol(int_of_quad(to.ty.bits(), to.signed))?;
                self.soft_binary(&BinOp::Gt, types::F128)?;
                if to.signed {
                    self.soft_binary(&BinOp::Eq, types::F128)?;
                }
                Ok(())
            }
            (_, types::F128) => self.symbol(quad_of_int(from.ty.bits(), from.signed)),
            // alloc: the error's message
            _ => Err(unsupported(format!("cast from {} to {}", from.ty, to.ty))),
        }
    }

    /// `float_binary_op` of two `f32`s or `f64`s.
    fn float_binary(&mut self, op: &BinOp, ty: ir::Type) -> Result<(), String> {
        match op {
            BinOp::Eq
            | BinOp::Ne
            | BinOp::Lt
            | BinOp::Le
            | BinOp::Gt
            | BinOp::Ge
            | BinOp::Add
            | BinOp::Sub
            | BinOp::Mul
            | BinOp::Div => Ok(()),
            BinOp::Rem => self.symbol(if ty == types::F32 { "fmodf" } else { "fmod" }),
            // alloc: the error's message
            _ => Err(unsupported(format!("float operator {op:?}"))),
        }
    }

    /// `soft_binary_op` of two `f16`s or two `f128`s.
    fn soft_binary(&mut self, op: &BinOp, ty: ir::Type) -> Result<(), String> {
        if ty == types::F16 {
            let instructions = match op {
                BinOp::Add => Some(("fadd h0, h0, h1", "vaddsh xmm0, xmm0, xmm1")),
                BinOp::Sub => Some(("fsub h0, h0, h1", "vsubsh xmm0, xmm0, xmm1")),
                BinOp::Mul => Some(("fmul h0, h0, h1", "vmulsh xmm0, xmm0, xmm1")),
                BinOp::Div => Some(("fdiv h0, h0, h1", "vdivsh xmm0, xmm0, xmm1")),
                _ => None,
            };
            if let Some((aarch64, _)) = instructions.filter(|_| self.fx.half_precision()) {
                return self.half_instruction(aarch64);
            }
            if let Some((_, x86)) = instructions.filter(|_| self.x86_half_precision()) {
                return self.half_instruction(x86);
            }
            self.widen_half(types::F32)?;
            self.float_binary(op, types::F32)?;
            return match op {
                BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => Ok(()),
                _ => self.narrow_to_half(types::F32),
            };
        }
        let name = match op {
            BinOp::Add => "__addtf3",
            BinOp::Sub => "__subtf3",
            BinOp::Mul => "__multf3",
            BinOp::Div => "__divtf3",
            BinOp::Rem => return self.quad_libcall(("fmodf128", "fmodl")),
            BinOp::Eq => "__eqtf2",
            BinOp::Ne => "__netf2",
            BinOp::Lt => "__lttf2",
            BinOp::Le => "__letf2",
            BinOp::Gt => "__gttf2",
            BinOp::Ge => "__getf2",
            // alloc: the error's message
            _ => return Err(unsupported(format!("float operator {op:?}"))),
        };
        self.symbol(name)
    }

    /// `soft_unary`: `op`, one of the float intrinsics of one operand, of an `f16` or an
    /// `f128`.
    fn soft_unary(&mut self, op: Intrinsic, ty: ir::Type) -> Result<(), String> {
        use Intrinsic as I;
        if op == I::Fabs {
            return Ok(());
        }
        if ty == types::F16 && self.fx.half_precision() {
            let instruction = match op {
                I::Sqrt => "fsqrt h0, h0",
                I::Floor => "frintm h0, h0",
                I::Ceil => "frintp h0, h0",
                I::Trunc => "frintz h0, h0",
                I::RoundTiesEven => "frintx h0, h0",
                _ => "frinta h0, h0",
            };
            return self.half_instruction(instruction);
        }
        if ty == types::F16 {
            self.widen_half(types::F32)?;
            let x86 = self.on_x86();
            match op {
                I::Sqrt => (),
                I::Floor if x86 => self.symbol("floorf")?,
                I::Ceil if x86 => self.symbol("ceilf")?,
                I::Trunc if x86 => self.symbol("truncf")?,
                I::RoundTiesEven if x86 => self.symbol("rintf")?,
                // AArch64's instructions.
                I::Floor | I::Ceil | I::Trunc | I::RoundTiesEven => (),
                _ => self.symbol("roundf")?,
            }
            return self.narrow_to_half(types::F32);
        }
        let libm = match op {
            I::Sqrt => ("sqrtf128", "sqrtl"),
            I::Floor => ("floorf128", "floorl"),
            I::Ceil => ("ceilf128", "ceill"),
            I::Trunc => ("truncf128", "truncl"),
            I::RoundTiesEven => ("rintf128", "rintl"),
            _ => ("roundf128", "roundl"),
        };
        self.quad_libcall(libm)
    }

    /// `soft_fma`.
    fn soft_fma(&mut self, ty: ir::Type) -> Result<(), String> {
        if ty == types::F128 {
            return self.quad_libcall(("fmaf128", "fmal"));
        }
        if self.fx.half_precision() {
            return self.half_instruction("fmadd h0, h0, h1, h2");
        }
        self.widen_half(types::F64)?;
        self.cranelift_routine(Routine::Fma, types::F64)?;
        self.narrow_to_half(types::F64)
    }

    /// `soft_powi`.
    fn soft_powi(&mut self, ty: ir::Type) -> Result<(), String> {
        if ty == types::F128 {
            return self.symbol("__powitf2");
        }
        self.widen_half(types::F32)?;
        self.symbol("__powisf2")?;
        self.narrow_to_half(types::F32)
    }

    /// `widen_half` of an `f16` to `to`.
    fn widen_half(&mut self, to: ir::Type) -> Result<(), String> {
        if !self.on_x86() {
            return self.assembly(asm::fcvt(letter(to), 'h'));
        }
        self.x86_soft_half()?;
        self.symbol("__extendhfsf2")
    }

    /// `narrow_to_half` of an `f32` or `f64` of `from`.
    fn narrow_to_half(&mut self, from: ir::Type) -> Result<(), String> {
        if !self.on_x86() {
            return self.assembly(asm::fcvt('h', letter(from)));
        }
        self.x86_soft_half()?;
        self.symbol(if from == types::F32 { "__truncsfhf2" } else { "__truncdfhf2" })
    }

    /// `half_instruction`: the function of a half-precision instruction.
    fn half_instruction(&mut self, instruction: &str) -> Result<(), String> {
        let text = match self.on_x86() {
            true => asm::x86_half(instruction),
            false => asm::half(instruction),
        };
        self.assembly(text)
    }

    /// `quad_libcall`: C's function of `f128`, by its `f128` name on x86-64 and its `long
    /// double` name elsewhere.
    fn quad_libcall(&mut self, (f128, long): (&str, &str)) -> Result<(), String> {
        self.symbol(if self.on_x86() { f128 } else { long })
    }

    fn x86_soft_half(&self) -> Result<(), String> {
        if self.fx.function_enables(TargetFeature::F16C) {
            return Err(unsupported(
                "f16 in a function with F16C, which LLVM converts by its instructions",
            ));
        }
        Ok(())
    }

    fn x86_half_precision(&self) -> bool {
        self.on_x86() && self.fx.function_enables(TargetFeature::AVX512FP16)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn scalar(ty: ir::Type, signed: bool) -> AbiScalar {
        AbiScalar { ty, signed }
    }

    #[test]
    fn pairwise_neon_is_lowered_lane_by_lane() {
        assert!(neon_pairwise("umaxp.v16i8", 2));
        assert!(neon_pairwise("addp.v8i8", 2));
        assert!(neon_pairwise("addp.v2i64", 2));
        assert!(neon_pairwise("uaddlv.i32.v16i8", 1));
        assert!(!neon_pairwise("uaddlv.v16i8", 1));
        assert!(neon_pairwise("tbl1.v16i8", 2));
        assert!(!neon_pairwise("tbl1.v16i8", 3));
        assert!(neon_pairwise("tbx1.v8i8", 3));
        assert!(!neon_pairwise("tbx1.v8i8", 2));
        assert!(!neon_pairwise("tbl1.v8i16", 2));
        assert!(!neon_pairwise("sqdmulh.v2i64", 2));
        assert!(neon_pairwise("sqdmull.v2i64", 2));
        assert!(!neon_pairwise("sqdmull.v4i16", 2));
        // Float lanes and unknown operations go to `neon::lower`.
        assert!(!neon_pairwise("fmaxp.v4f32", 2));
        assert!(!neon_pairwise("sqshlu.v8i8", 2));
    }

    #[test]
    fn every_fold_of_an_fma_is_listed() {
        let texts = fma_texts(32, "ps");
        assert_eq!(texts.len(), 8);
        assert!(texts.contains(&"vmovaps ymm0, ymm2\nvfmadd213ps ymm0, ymm1, ymm3".to_owned()));
        assert!(texts.contains(&"vmovaps ymm0, ymm1\nvfnmsub213ps ymm0, ymm2, ymm3".to_owned()));
        let texts = fma_texts(4, "ss");
        assert!(texts.contains(&"vmovaps xmm0, xmm2\nvfmsub213ss xmm0, xmm1, xmm3".to_owned()));
        let texts = fma_texts(64, "pd");
        assert!(texts.iter().all(|it| it.starts_with("vmovaps zmm0, zmm")));
    }

    #[test]
    fn wide_integers_convert_by_compiler_builtins() {
        let (int, uint) = (scalar(types::I128, true), scalar(types::I128, false));
        let (single, double) = (scalar(types::F32, true), scalar(types::F64, true));
        assert_eq!(wide_int_float(single, int), Some("__fixsfti"));
        assert_eq!(wide_int_float(double, uint), Some("__fixunsdfti"));
        assert_eq!(wide_int_float(int, double), Some("__floattidf"));
        assert_eq!(wide_int_float(uint, single), Some("__floatuntisf"));
        assert_eq!(wide_int_float(int, uint), None);
        assert_eq!(divide_builtin(true, true), "__divti3");
        assert_eq!(divide_builtin(false, false), "__umodti3");
    }

    #[test]
    fn quads_convert_by_width() {
        assert_eq!(quad_of_int(8, true), "__floatsitf");
        assert_eq!(quad_of_int(64, false), "__floatunditf");
        assert_eq!(quad_of_int(128, true), "__floattitf");
        assert_eq!(int_of_quad(16, false), "__fixunstfsi");
        assert_eq!(int_of_quad(64, true), "__fixtfdi");
        assert_eq!(int_of_quad(128, false), "__fixunstfti");
    }

    /// The names `wasm_simd.rs`'s `Op::of` admits, the 31 the walk took by name and the
    /// reductions it missed.
    #[test]
    fn wasm_takes_the_generic_intrinsics_wasm_simd_lowers() {
        const BY_NAME: &[&str] = &[
            "simd_add",
            "simd_sub",
            "simd_mul",
            "simd_div",
            "simd_and",
            "simd_or",
            "simd_xor",
            "simd_eq",
            "simd_ne",
            "simd_lt",
            "simd_le",
            "simd_gt",
            "simd_ge",
            "simd_saturating_add",
            "simd_saturating_sub",
            "simd_shl",
            "simd_shr",
            "simd_neg",
            "simd_fabs",
            "simd_fsqrt",
            "simd_floor",
            "simd_ceil",
            "simd_trunc",
            "simd_round_ties_even",
            "simd_ctpop",
            "simd_select",
            "simd_splat",
            "simd_shuffle",
            "simd_cast",
            "simd_as",
            "simd_bitmask",
            "simd_reduce_add_unordered",
            "simd_reduce_max",
            "simd_reduce_min",
            "simd_reduce_and",
            "simd_reduce_or",
            "simd_reduce_xor",
            "simd_reduce_any",
            "simd_reduce_all",
        ];
        let mut taken = 0;
        for index in 0..Intrinsic::COUNT as u16 {
            let intrinsic = Intrinsic::from_index(index).expect("an intrinsic");
            let by_name = BY_NAME.contains(&intrinsic.to_string().as_str());
            let by_enum = matches!(intrinsic, Intrinsic::Simd(op) if wasm_simd(op));
            assert_eq!(by_enum, by_name, "{intrinsic}");
            taken += usize::from(by_enum);
        }
        assert_eq!(taken, BY_NAME.len());
    }

    #[test]
    fn routines_are_by_width() {
        assert_eq!(routine(Routine::Floor, types::F32), ir::LibCall::FloorF32);
        assert_eq!(routine(Routine::Nearest, types::F64), ir::LibCall::NearestF64);
        assert_eq!(routine(Routine::Fma, types::F64), ir::LibCall::FmaF64);
        assert_eq!(letter(types::F16), 'h');
        assert_eq!(letter(types::F64), 'd');
    }
}
