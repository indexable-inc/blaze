//! rustc's `CheckEnums` (`check_enums.rs`): before each transmute to an enum, a check that the
//! value is one of the enum's: its discriminant is one of the variants', or its niche is in
//! the valid range, or (of an enum with no variants) that it is never made. It is part of what
//! UB checks mean and runs at every level when the crate checks UB, after `CheckAlignment` and
//! `CheckNull`.
//!
//! rustc finds the transmutes as `Transmute` casts, which `LowerIntrinsics` makes of calls of
//! `transmute` and `transmute_unchecked`; this MIR keeps the call, and the check goes before
//! it, where rustc's goes before the statement at the end of the block.

use blaze_mem::Frame;
use hir_def::AdtId;
use rustc_abi::{Scalar, Size};
use rustc_type_ir::inherent::IntoKind;

use crate::{
    db::HirDatabase,
    layout::{TagEncoding, Variants},
    mir::{
        AssertKind, BasicBlock, BasicBlockId, BinOp, CastKind, MirBody, MirLowerError, MirSpan,
        Operand, OperandKind, ProjectionElem, Rvalue, StatementKind, SwitchTargets, Terminator,
        TerminatorKind, framed::block_id, transform::Ctx,
    },
    next_solver::{GenericArgs, Ty, TyKind},
};

use super::check_pointers::{
    Consts, Lowered, Setup, Site, append, assign, copy_of, project, site_span, sites,
    split_block, split_index, temp,
};

type Result<'db, T> = std::result::Result<T, MirLowerError<'db>>;

/// The integer type of a tag, and its size.
#[derive(Debug, Clone, Copy)]
struct Discr<'db> {
    ty: Ty<'db>,
    size: Size,
}

/// rustc's `EnumCheckType`.
enum Check<'f, 'db> {
    /// An enum with no variants made of a value of an inhabited type.
    Uninhabited,
    /// An enum with a tag and no niche: the tag is one of the variants' discriminants.
    Direct { source: Operand, discr: Discr<'db>, op_size: Size, valid: Vec<u128, &'f Frame> },
    /// An enum with a niche: the tag at `offset` is in `start..=end`, wrapping.
    WithNiche { source: Operand, discr: Discr<'db>, op_size: Size, offset: Size, start: u128, end: u128 },
}

pub(super) fn check_enums<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &mut MirBody<'db>,
    frame: &Frame,
) -> Result<'db, ()> {
    // A crate with no panic handler would not build with the panic this adds.
    if ctx.infcx.interner.lang_items().PanicImpl.is_none() {
        return Ok(());
    }
    let consts = Consts::new(ctx)?;
    let mut setup: Setup<'_> = Vec::new_in(frame);
    // The blocks there were before any check: the ones splits make come after them.
    for index in (0..body.basic_blocks.len()).rev() {
        let block = block_id(index);
        for site in sites(body, block) {
            let Some((source, ty)) = transmute_at(ctx, body, block, site) else { continue };
            let Some(check) = find(ctx, body, source, ty, frame)? else { continue };
            let span = site_span(body, block, site);
            let at = split_index(body, block, site);
            let next = split_block(body, block, at);
            match check {
                Check::Direct { op_size, .. } | Check::WithNiche { op_size, .. }
                    if op_size.bytes() == 0 =>
                {
                    // A zero-sized value is never the tag of an inhabited enum; type checking
                    // has said so.
                    body.basic_blocks[block].terminator =
                        Some(Terminator { span, kind: TerminatorKind::Goto { target: next } });
                }
                Check::Direct { source, discr, op_size, valid } => direct_check(
                    ctx, &consts, body, block, source, discr, op_size, valid, span, next,
                    &mut setup,
                )?,
                Check::Uninhabited => uninhabited_check(&consts, body, block, span, next),
                Check::WithNiche { source, discr, op_size, offset, start, end } => niche_check(
                    ctx, &consts, body, block, source, discr, op_size, offset, start, end, span,
                    next, &mut setup,
                )?,
            }
        }
    }
    Ok(())
}

/// The value a site transmutes and the type it transmutes it to.
fn transmute_at<'a, 'db>(
    ctx: &Ctx<'_, 'db>,
    body: &'a MirBody<'db>,
    block: BasicBlockId,
    site: Site,
) -> Option<(&'a Operand, Ty<'db>)> {
    let data = &body.basic_blocks[block];
    match site {
        Site::Statement(at) => match &data.statements[at].kind {
            StatementKind::Assign(_, Rvalue::Cast(CastKind::Transmute, source, ty)) => {
                Some((source, ty.as_ref()))
            }
            _ => None,
        },
        Site::Call(Lowered::Transmute) => match &data.terminator.as_ref()?.kind {
            TerminatorKind::Call { args, destination, .. } => {
                let [source] = &args[..] else { return None };
                let ty = destination.as_ref().ty(body, &ctx.infcx, ctx.env).ty;
                Some((source, ty))
            }
            _ => None,
        },
        Site::Call(_) => None,
    }
}

/// rustc's `EnumFinder::visit_rvalue`.
fn find<'f, 'db>(
    ctx: &Ctx<'_, 'db>,
    body: &MirBody<'db>,
    source: &Operand,
    ty: Ty<'db>,
    frame: &'f Frame,
) -> Result<'db, Option<Check<'f, 'db>>> {
    let ty = ctx.normalize(ty);
    let TyKind::Adt(adt, _) = ty.kind() else { return Ok(None) };
    let AdtId::EnumId(id) = adt.def_id() else { return Ok(None) };
    let Some(enum_layout) = ctx.layout_of(ty) else { return Ok(None) };
    let source_ty = match &source.kind {
        OperandKind::Copy(place) | OperandKind::Move(place) => {
            place.as_ref().ty(body, &ctx.infcx, ctx.env).ty
        }
        OperandKind::Constant { ty, .. } => ty.as_ref(),
        OperandKind::Allocation { allocation } => allocation.as_ref().ty,
        OperandKind::Static(_) => return Ok(None),
    };
    let Some(op_layout) = ctx.layout_of(ctx.normalize(source_ty)) else { return Ok(None) };
    let source = match &source.kind {
        // clone: the place the check reads, kept in the body: a refcount
        OperandKind::Move(place) => Operand { kind: OperandKind::Copy(place.clone()), span: None },
        _ => source.clone(), // clone: the operand the check reads, kept in the body: a handle
    };
    let op_size = op_layout.size;
    Ok(match &enum_layout.variants {
        Variants::Empty if op_layout.is_uninhabited() => None,
        Variants::Empty => Some(Check::Uninhabited),
        Variants::Single { .. } => None,
        Variants::Multiple {
            tag: Scalar::Initialized { value, .. },
            tag_encoding: TagEncoding::Direct,
            ..
        } => {
            let discr = discr_of(ctx, *value)?;
            let variants = &id.enum_variants(ctx.db).variants;
            let mut valid = Vec::with_capacity_in(variants.len(), frame);
            for (variant, _) in variants.values() {
                let value = ctx.db.const_eval_discriminant(*variant).map_err(|error| {
                    // alloc: error path
                    MirLowerError::ConstEvalError("the discriminant".into(), Box::new(error))
                })?;
                // rustc's `Discr::val`: the bits of the value.
                valid.push(value as u128);
            }
            Some(Check::Direct { source, discr, op_size, valid })
        }
        Variants::Multiple {
            tag: Scalar::Initialized { value, valid_range },
            tag_encoding: TagEncoding::Niche { .. },
            tag_field,
            ..
        } => {
            let discr = discr_of(ctx, *value)?;
            let offset = enum_layout.fields.offset(tag_field.0.into_raw().into_u32() as usize);
            let (start, end) = (valid_range.start, valid_range.end);
            Some(Check::WithNiche { source, discr, op_size, offset, start, end })
        }
        _ => None,
    })
}

/// rustc's `Primitive::to_int_ty` and `size` of a tag.
fn discr_of<'db>(ctx: &Ctx<'_, 'db>, value: rustc_abi::Primitive) -> Result<'db, Discr<'db>> {
    use rustc_abi::{Integer, Primitive};
    use rustc_type_ir::{IntTy, UintTy};
    let interner = ctx.infcx.interner;
    let ty = match value {
        Primitive::Int(int, signed) => match (int, signed) {
            (Integer::I8, true) => Ty::new_int(interner, IntTy::I8),
            (Integer::I16, true) => Ty::new_int(interner, IntTy::I16),
            (Integer::I32, true) => Ty::new_int(interner, IntTy::I32),
            (Integer::I64, true) => Ty::new_int(interner, IntTy::I64),
            (Integer::I128, true) => Ty::new_int(interner, IntTy::I128),
            (Integer::I8, false) => Ty::new_uint(interner, UintTy::U8),
            (Integer::I16, false) => Ty::new_uint(interner, UintTy::U16),
            (Integer::I32, false) => Ty::new_uint(interner, UintTy::U32),
            (Integer::I64, false) => Ty::new_uint(interner, UintTy::U64),
            (Integer::I128, false) => Ty::new_uint(interner, UintTy::U128),
        },
        Primitive::Pointer(_) => Ty::new_uint(interner, UintTy::Usize),
        Primitive::Float(_) => {
            return Err(MirLowerError::NotSupported("UB checks: an enum tag that is a float".into()));
        }
    };
    let Some(size) = ctx.size_of(ty) else {
        // alloc: error path
        return Err(MirLowerError::NotSupported(format!("UB checks: no layout of the tag {ty:?}")));
    };
    Ok(Discr { ty, size: Size::from_bytes(size) })
}

fn uint_of_size<'db>(ctx: &Ctx<'_, 'db>, size: Size) -> Result<'db, Ty<'db>> {
    use rustc_type_ir::UintTy;
    let uint = match size.bytes() {
        1 => UintTy::U8,
        2 => UintTy::U16,
        4 => UintTy::U32,
        8 => UintTy::U64,
        16 => UintTy::U128,
        invalid => {
            // alloc: error path
            return Err(MirLowerError::NotSupported(format!(
                "UB checks: a value of {invalid} bytes transmuted to an enum"
            )));
        }
    };
    Ok(Ty::new_uint(ctx.infcx.interner, uint))
}

/// rustc's `insert_discr_cast_to_u128`: the tag of `source`, as a `u128`, computed by what it
/// adds to `setup`.
#[allow(clippy::too_many_arguments)]
fn discr_to_u128<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &mut MirBody<'db>,
    source: Operand,
    discr: Discr<'db>,
    op_size: Size,
    offset: Option<Size>,
    span: MirSpan,
    setup: &mut Setup<'_>,
) -> Result<'db, crate::mir::LocalId> {
    let interner = ctx.infcx.interner;
    let (cast, bits) = if discr.size.bytes() < op_size.bytes() {
        // The tag is narrower than the value: the value as `[MaybeUninit<u8>; N]`, and the
        // bytes of the tag out of it.
        let Some(maybe_uninit) = interner.lang_items().MaybeUninit else {
            return Err(MirLowerError::NotSupported(
                "UB checks: no `MaybeUninit` lang item in core".into(),
            ));
        };
        let u8_ty = Ty::new_uint(interner, rustc_type_ir::UintTy::U8);
        let byte =
            Ty::new_adt(interner, AdtId::UnionId(maybe_uninit), GenericArgs::new_from_slice(&[u8_ty.into()]));
        let array_ty = Ty::new_array(interner, byte, op_size.bytes());
        let array = temp(body, array_ty);
        setup.push(assign(array, Rvalue::Cast(CastKind::Transmute, source, array_ty.store()), span));
        let from = offset.unwrap_or(Size::ZERO).bytes();
        // `to` of this MIR's `Subslice` counts from the end.
        let to = op_size.bytes() - (from + discr.size.bytes());
        let bytes = project(
            &crate::mir::Place::from(array).store(),
            ProjectionElem::Subslice { from, to },
        );
        (CastKind::Transmute, Operand { kind: OperandKind::Copy(bytes), span: None })
    } else {
        let int_ty = uint_of_size(ctx, op_size)?;
        let int = temp(body, int_ty);
        setup.push(assign(int, Rvalue::Cast(CastKind::Transmute, source, int_ty.store()), span));
        (CastKind::IntToInt, copy_of(int))
    };
    let tag = temp(body, discr.ty);
    setup.push(assign(tag, Rvalue::Cast(cast, bits, discr.ty.store()), span));
    let u128_ty = Ty::new_uint(interner, rustc_type_ir::UintTy::U128);
    let wide = temp(body, u128_ty);
    setup.push(assign(wide, Rvalue::Cast(CastKind::IntToInt, copy_of(tag), u128_ty.store()), span));
    Ok(wide)
}

/// The `Assert` a failed check ends in: `panic_invalid_enum_construction(value)`, which does
/// not unwind (`#[rustc_nounwind]`).
fn assert(cond: Operand, value: Operand, span: MirSpan, next: BasicBlockId) -> Terminator {
    Terminator {
        span,
        kind: TerminatorKind::Assert {
            cond,
            expected: true,
            msg: AssertKind::InvalidEnumConstruction(value),
            target: next,
            cleanup: None,
        },
    }
}

/// rustc's `insert_direct_enum_check`.
#[allow(clippy::too_many_arguments)]
fn direct_check<'db>(
    ctx: &Ctx<'_, 'db>,
    consts: &Consts<'db>,
    body: &mut MirBody<'db>,
    block: BasicBlockId,
    source: Operand,
    discr: Discr<'db>,
    op_size: Size,
    valid: Vec<u128, &Frame>,
    span: MirSpan,
    next: BasicBlockId,
    setup: &mut Setup<'_>,
) -> Result<'db, ()> {
    let is_cleanup = body.basic_blocks[block].is_cleanup;
    let invalid = body.basic_blocks.alloc(BasicBlock {
        statements: Vec::new(), // alloc: none, a kept block with no statements
        terminator: None,
        is_cleanup,
    });
    let wide = discr_to_u128(ctx, body, source, discr, op_size, None, span, setup)?;
    let masked = temp(body, consts.u128_ty);
    let mask = consts.u128(discr.size.unsigned_int_max());
    setup.push(assign(masked, Rvalue::BinaryOp(BinOp::BitAnd, copy_of(wide), mask), span));
    append(body, block, setup);
    let targets = valid.into_iter().map(|value| (discr.size.truncate(value), next));
    body.basic_blocks[block].terminator = Some(Terminator {
        span,
        kind: TerminatorKind::SwitchInt {
            discr: copy_of(masked),
            targets: SwitchTargets::new(targets, invalid),
        },
    });
    body.basic_blocks[invalid].terminator =
        Some(assert(consts.bool(false), copy_of(masked), span, next));
    Ok(())
}

/// rustc's `insert_uninhabited_enum_check`.
fn uninhabited_check<'db>(
    consts: &Consts<'db>,
    body: &mut MirBody<'db>,
    block: BasicBlockId,
    span: MirSpan,
    next: BasicBlockId,
) {
    let is_ok = temp(body, consts.bool_ty);
    let zero = consts.u128(0);
    let data = &mut body.basic_blocks[block];
    data.statements.push(assign(is_ok, Rvalue::Use(consts.bool(false)), span));
    data.terminator = Some(assert(copy_of(is_ok), zero, span, next));
}

/// rustc's `insert_niche_check`.
#[allow(clippy::too_many_arguments)]
fn niche_check<'db>(
    ctx: &Ctx<'_, 'db>,
    consts: &Consts<'db>,
    body: &mut MirBody<'db>,
    block: BasicBlockId,
    source: Operand,
    discr: Discr<'db>,
    op_size: Size,
    offset: Size,
    start: u128,
    end: u128,
    span: MirSpan,
    next: BasicBlockId,
    setup: &mut Setup<'_>,
) -> Result<'db, ()> {
    let wide = discr_to_u128(ctx, body, source, discr, op_size, Some(offset), span, setup)?;
    let diff = temp(body, consts.u128_ty);
    let is_ok = temp(body, consts.bool_ty);
    let below = Rvalue::BinaryOp(BinOp::Sub, copy_of(wide), consts.u128(start));
    // A `WrappingRange`: what is past `start` by at most `end - start`, wrapping.
    let within = Rvalue::BinaryOp(BinOp::Le, copy_of(diff), consts.u128(end.wrapping_sub(start)));
    setup.push(assign(diff, below, span));
    setup.push(assign(is_ok, within, span));
    append(body, block, setup);
    body.basic_blocks[block].terminator =
        Some(assert(copy_of(is_ok), copy_of(wide), span, next));
    Ok(())
}
