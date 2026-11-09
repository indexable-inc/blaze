//! `#[blaze::bits]` structs lowered to their backing integers (docs/v2/plan/design/bits.md).
//!
//! Until this pass a field of a bits struct is a place like any other: borrow checking sees
//! its reads, writes and moves as it sees any field's, and refuses a reference to it. This
//! pass, the last of `run_passes`, leaves no place that projects into a bits struct and no
//! aggregate of one. For a field of width `w` at bit `o` of the backing `uN`:
//!
//! - a read is `(transmute::<S, uN>(s) >> o) & mask(w)`, made a value of the field's type
//!   (an integer cast, `!= 0` for a `bool`, the enum's tag for an enum, a transmute to the
//!   inner backing integer for a nested bits struct);
//! - a write is `s = transmute((transmute(s) & !(mask(w) << o)) | ((v as uN) & mask(w)) << o)`;
//! - a literal ORs its fields' bits, starting from zero, so the padding above the fields is
//!   zero;
//! - `==` of two bits structs (what a constant pattern compiles to) compares the bits the fields
//!   take.
//!
//! The code generator and the interpreter see only integer operations and transmutes between a
//! bits struct and its backing integer, of one size, so neither knows bits structs exist, and a
//! body that uses none comes out of this pass unchanged.

use blaze_mem::Frame;
use hir_def::{AdtId, VariantId};
use rustc_abi::{BackendRepr, Integer, Primitive};
use rustc_type_ir::{
    IntTy, UintTy,
    inherent::{IntoKind, Ty as _},
};

use crate::{
    layout::{BitWidth, BitsLayout, LayoutError, bits_layout, bits_struct_of, enum_variant_ids},
    mir::{
        AggregateKind, AsmOperand, BasicBlock, BinOp, CastKind, Local, LocalId, MirBody,
        MirLowerError, MirSpan, Operand, OperandKind, Place, PlaceTy, Projection, ProjectionElem,
        Rvalue, Statement, StatementKind, StoredPlace, Terminator, TerminatorKind,
        framed::block_id,
    },
    next_solver::{StoredGenericArgs, Ty, TyKind},
};

use super::{Ctx, Result};

/// Every place into a bits struct, every aggregate of one and every `==` of two, in `body`,
/// lowered to operations on the backing integer. `_frame` is every runtime pass's.
pub(super) fn lower_bits_structs<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &mut MirBody<'db>,
    _frame: &Frame,
) -> Result<'db, ()> {
    if !uses_bits(ctx, body)? {
        return Ok(());
    }
    // Blocks made for a call's write-back are appended; they hold no bits place themselves.
    let blocks = body.basic_blocks.len();
    for index in 0..blocks {
        let id = block_id(index);
        let statements = std::mem::take(&mut body.basic_blocks[id].statements);
        // alloc: the block's statements, kept in the body
        let mut out = Vec::with_capacity(statements.len());
        for statement in statements {
            lower_statement(ctx, body, &mut out, statement)?;
        }
        let terminator = body.basic_blocks[id].terminator.take();
        let terminator = match terminator {
            Some(terminator) => Some(lower_terminator(ctx, body, &mut out, terminator)?),
            None => None,
        };
        let block = &mut body.basic_blocks[id];
        block.statements = out;
        block.terminator = terminator;
    }
    Ok(())
}

/// Whether `body` has a place in a bits struct, a literal of one or an `==` of two: what the
/// rewrite below would change, found without changing or allocating anything, so a body that
/// uses no bits struct is left as it is.
fn uses_bits<'db>(ctx: &Ctx<'_, 'db>, body: &MirBody<'db>) -> Result<'db, bool> {
    let place = |place: &StoredPlace| -> Result<'db, bool> {
        Ok(access(ctx, body, place.as_ref())?.is_some())
    };
    let operand = |operand: &Operand| -> Result<'db, bool> {
        match &operand.kind {
            OperandKind::Copy(it) | OperandKind::Move(it) => place(it),
            _ => Ok(false),
        }
    };
    let operands = |list: &[Operand]| -> Result<'db, bool> {
        for it in list {
            if operand(it)? {
                return Ok(true);
            }
        }
        Ok(false)
    };
    let eq = |l: &Operand| bits_struct_of(ctx.db, ctx.normalize(ctx.operand_ty(body, l))).is_some();
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            let found = match &statement.kind {
                StatementKind::Assign(target, rvalue) => {
                    place(target)?
                        || match rvalue {
                            Rvalue::Use(it) | Rvalue::Repeat(it, _) | Rvalue::Cast(_, it, _) => {
                                operand(it)?
                            }
                            Rvalue::UnaryOp(_, it) => operand(it)?,
                            Rvalue::BinaryOp(op, l, r) | Rvalue::CheckedBinaryOp(op, l, r) => {
                                operand(l)?
                                    || operand(r)?
                                    || (matches!(op, BinOp::Eq | BinOp::Ne) && eq(l))
                            }
                            Rvalue::Ref(_, it)
                            | Rvalue::Discriminant(it)
                            | Rvalue::Len(it)
                            | Rvalue::CopyForDeref(it) => place(it)?,
                            Rvalue::Aggregate(kind, operands) => {
                                let mut found = matches!(
                                    kind,
                                    AggregateKind::Adt(VariantId::StructId(it), _)
                                        if crate::layout::is_bits_struct(ctx.db, *it)
                                );
                                for it in operands.iter() {
                                    found |= operand(it)?;
                                }
                                found
                            }
                            Rvalue::ThreadLocalRef(never)
                            | Rvalue::AddressOf(never)
                            | Rvalue::NullaryOp(never) => match *never {},
                        }
                }
                StatementKind::FakeRead(it) | StatementKind::Deinit(it) => place(it)?,
                StatementKind::InlineAsm(asm) => {
                    let mut found = false;
                    for it in asm.operands.iter() {
                        if let AsmOperand::Reg { input, output, .. } = it {
                            found |= input.as_ref().map(operand).transpose()?.unwrap_or(false)
                                || output.as_ref().map(place).transpose()?.unwrap_or(false);
                        }
                    }
                    found
                }
                StatementKind::StorageLive(_)
                | StatementKind::StorageDead(_)
                | StatementKind::Nop => false,
            };
            if found {
                return Ok(true);
            }
        }
        let Some(terminator) = &block.terminator else { continue };
        let found = match &terminator.kind {
            TerminatorKind::SwitchInt { discr: it, .. }
            | TerminatorKind::Assert { cond: it, .. } => operand(it)?,
            TerminatorKind::TailCall { func, args, .. } => operand(func)? || operands(&args[..])?,
            TerminatorKind::Call { func, args, destination, .. } => {
                operand(func)? || place(destination)? || operands(&args[..])?
            }
            TerminatorKind::Yield { value, resume_arg, .. } => {
                operand(value)? || place(resume_arg)?
            }
            TerminatorKind::Drop { place: it, .. } => place(it)?,
            TerminatorKind::DropAndReplace { place: it, value, .. } => {
                place(it)? || operand(value)?
            }
            TerminatorKind::Goto { .. }
            | TerminatorKind::UnwindResume
            | TerminatorKind::Abort
            | TerminatorKind::Return
            | TerminatorKind::Unreachable
            | TerminatorKind::CoroutineDrop
            | TerminatorKind::FalseEdge { .. }
            | TerminatorKind::FalseUnwind { .. }
            | TerminatorKind::ComptimeFor { .. }
            | TerminatorKind::ComptimeNext { .. } => false,
        };
        if found {
            return Ok(true);
        }
    }
    Ok(false)
}

/// A place in a bits struct: the bits of the backing integer of `carrier` it is.
struct Access<'db> {
    /// The outermost bits struct the place is in: a place with an address.
    carrier: Place<'db>,
    carrier_ty: Ty<'db>,
    backing: Integer,
    /// Where the place's bits start in the backing integer: the sum of the offsets of the
    /// fields on the way, for a field of a bits struct in a bits struct.
    offset: u32,
    width: BitWidth,
    /// The type of the place, normalized.
    ty: Ty<'db>,
}

/// What a bits field holds, as its reads and writes need it.
enum Leaf<'db> {
    Bool,
    Int,
    /// An enum of one variant: its value is that variant, and it takes no bits it reads.
    SingleEnum {
        variant: VariantId,
        args: StoredGenericArgs,
    },
    /// An enum of more variants, its tag a `tag`.
    Enum {
        tag: Ty<'db>,
    },
    /// A bits struct, its backing integer `backing`.
    Nested {
        backing: Integer,
    },
}

/// The bits of a bits struct `place` is, if it is in one. A place that goes on past such a
/// field (`s.f.0`, `*s.f`) is refused: no field of a bits struct has fields or is a pointer,
/// but for a bits struct, whose fields the walk follows.
fn access<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &MirBody<'db>,
    place: Place<'db>,
) -> Result<'db, Option<Access<'db>>> {
    let projection = place.projection.as_slice();
    if !projection.iter().any(|it| matches!(it, ProjectionElem::Field(_))) {
        return Ok(None);
    }
    let mut ty = PlaceTy::from_ty(body.locals[place.local].ty.as_ref());
    for (at, elem) in projection.iter().enumerate() {
        if let ProjectionElem::Field(field) = elem
            && ty.variant_id.is_none()
            && let Some(strukt) = bits_struct_of(ctx.db, ctx.normalize(ty.ty))
        {
            let layout = layout_of(ctx, strukt)?;
            let first = bits_field(layout, strukt, *field)?;
            let (mut offset, mut width, mut leaf) = (first.offset, first.width, first.ty.as_ref());
            let mut end = at + 1;
            while let Some(ProjectionElem::Field(next)) = projection.get(end)
                && let Some(inner) = bits_struct_of(ctx.db, leaf)
            {
                let field = bits_field(layout_of(ctx, inner)?, inner, *next)?;
                offset += field.offset;
                width = field.width;
                leaf = field.ty.as_ref();
                end += 1;
            }
            if end != projection.len() {
                // alloc: error path
                return Err(MirLowerError::ImplementationError(format!(
                    "the place {place:?} goes on past field {} of `#[blaze::bits]` struct \
                     {strukt:?}, which holds a `{leaf:?}`: a field of a bits struct has no place \
                     in it",
                    field.0
                )));
            }
            return Ok(Some(Access {
                carrier: Place {
                    local: place.local,
                    projection: Projection::new_from_slice(&projection[..at]),
                },
                carrier_ty: ty.ty,
                backing: layout.backing,
                offset,
                width,
                ty: leaf,
            }));
        }
        if at + 1 < projection.len() {
            ty = ty.multi_projection_ty(&ctx.infcx, ctx.env, std::slice::from_ref(elem));
        }
    }
    Ok(None)
}

fn layout_of<'db>(ctx: &Ctx<'_, 'db>, strukt: hir_def::StructId) -> Result<'db, &'db BitsLayout> {
    bits_layout(ctx.db, strukt)
        // alloc: error path
        .map_err(|err| MirLowerError::LayoutError(LayoutError::Bits(Box::new(err.clone()))))
}

fn bits_field<'db, 'l>(
    layout: &'l BitsLayout,
    strukt: hir_def::StructId,
    field: crate::mir::FieldIndex,
) -> Result<'db, &'l crate::layout::BitsField> {
    layout.field(field.to_local_field_id()).ok_or_else(|| {
        // alloc: error path
        MirLowerError::ImplementationError(format!(
            "`#[blaze::bits]` struct {strukt:?} has no field {}",
            field.0
        ))
    })
}

/// What `ty`, the type of a bits field, reads and writes as.
fn leaf<'db>(ctx: &Ctx<'_, 'db>, ty: Ty<'db>) -> Result<'db, Leaf<'db>> {
    Ok(match ty.kind() {
        TyKind::Bool => Leaf::Bool,
        TyKind::Int(_) | TyKind::Uint(_) => Leaf::Int,
        TyKind::Adt(def, args) => match def.def_id() {
            AdtId::StructId(strukt) if bits_struct_of(ctx.db, ty).is_some() => {
                Leaf::Nested { backing: layout_of(ctx, strukt)?.backing }
            }
            AdtId::EnumId(id) => {
                let variants = enum_variant_ids(ctx.db, id);
                if let [only] = &*variants {
                    Leaf::SingleEnum { variant: (*only).into(), args: args.store() }
                } else {
                    Leaf::Enum { tag: enum_tag(ctx, ty)? }
                }
            }
            _ => return Err(not_packable(ctx, ty)),
        },
        _ => return Err(not_packable(ctx, ty)),
    })
}

/// The errors here name their types with the database attached: a type's `Debug` names it
/// only through it, and the lowering may run on a worker with none.
fn not_packable<'db>(ctx: &Ctx<'_, 'db>, ty: Ty<'db>) -> MirLowerError<'db> {
    // alloc: error path
    MirLowerError::ImplementationError(blaze_query::db::attach(ctx.db, || {
        format!(
            "a field of a `#[blaze::bits]` struct has type `{ty:?}`, which `bits_layout` refuses"
        )
    }))
}

/// The integer type of the tag of the fieldless enum `ty`, which its values are transmutes of.
fn enum_tag<'db>(ctx: &Ctx<'_, 'db>, ty: Ty<'db>) -> Result<'db, Ty<'db>> {
    let layout = ctx.layout_of(ty).ok_or_else(|| {
        // alloc: error path
        MirLowerError::ImplementationError(blaze_query::db::attach(ctx.db, || {
            format!("the enum `{ty:?}` in a `#[blaze::bits]` struct has no layout")
        }))
    })?;
    match layout.backend_repr {
        BackendRepr::Scalar(scalar) => match scalar.primitive() {
            Primitive::Int(integer, signed) => Ok(int_ty(ctx, integer, signed)),
            _ => Err(not_packable(ctx, ty)),
        },
        // alloc: error path
        _ => Err(MirLowerError::ImplementationError(blaze_query::db::attach(ctx.db, || {
            format!("the enum `{ty:?}` in a `#[blaze::bits]` struct is not laid out as its tag")
        }))),
    }
}

fn int_ty<'db>(ctx: &Ctx<'_, 'db>, integer: Integer, signed: bool) -> Ty<'db> {
    let interner = ctx.infcx.interner;
    if signed {
        Ty::new_int(
            interner,
            match integer {
                Integer::I8 => IntTy::I8,
                Integer::I16 => IntTy::I16,
                Integer::I32 => IntTy::I32,
                Integer::I64 => IntTy::I64,
                Integer::I128 => IntTy::I128,
            },
        )
    } else {
        Ty::new_uint(
            interner,
            match integer {
                Integer::I8 => UintTy::U8,
                Integer::I16 => UintTy::U16,
                Integer::I32 => UintTy::U32,
                Integer::I64 => UintTy::U64,
                Integer::I128 => UintTy::U128,
            },
        )
    }
}

/// The low `bits` bits set.
fn mask(bits: u32) -> u128 {
    if bits >= 128 { u128::MAX } else { (1u128 << bits) - 1 }
}

/// Builds the statements of one block's rewrite, each at `span`.
struct Emit<'o> {
    out: &'o mut Vec<Statement>,
    span: MirSpan,
}

impl Emit<'_> {
    fn assign<'db>(&mut self, body: &mut MirBody<'db>, ty: Ty<'db>, value: Rvalue) -> LocalId {
        let temp = body.locals.alloc(Local { ty: ty.store() });
        self.assign_to(Place::from(temp).store(), value);
        temp
    }

    fn assign_to(&mut self, place: StoredPlace, value: Rvalue) {
        self.out.push(Statement { kind: StatementKind::Assign(place, value), span: self.span });
    }
}

fn moved(local: LocalId) -> Operand {
    Operand { kind: OperandKind::Move(Place::from(local).store()), span: None }
}

/// `value` as a constant of the integer `integer`, truncated to it.
fn konst<'db>(ctx: &Ctx<'_, 'db>, integer: Integer, signed: bool, value: u128) -> Operand {
    let bytes = integer.size().bytes() as usize;
    Operand::from_bytes(value.to_le_bytes()[..bytes].into(), int_ty(ctx, integer, signed))
}

/// `op(value, constant)` in the backing integer.
fn with_const<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &mut MirBody<'db>,
    emit: &mut Emit<'_>,
    op: BinOp,
    value: LocalId,
    (integer, signed): (Integer, bool),
    constant: u128,
) -> LocalId {
    let ty = int_ty(ctx, integer, signed);
    emit.assign(body, ty, Rvalue::BinaryOp(op, moved(value), konst(ctx, integer, signed, constant)))
}

/// The backing integer of the bits struct at `carrier`, as a `uN`.
fn backing_of<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &mut MirBody<'db>,
    emit: &mut Emit<'_>,
    carrier: Place<'db>,
    backing: Integer,
) -> LocalId {
    let uint = int_ty(ctx, backing, false);
    let carrier = Operand { kind: OperandKind::Copy(carrier.store()), span: None };
    emit.assign(body, uint, Rvalue::Cast(CastKind::Transmute, carrier, uint.store()))
}

/// The value of the bits field `access`, in a new temporary of its type.
fn read<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &mut MirBody<'db>,
    emit: &mut Emit<'_>,
    access: &Access<'db>,
) -> Result<'db, LocalId> {
    let unsigned = (access.backing, false);
    let n = access.backing.size().bits() as u32;
    let leaf = leaf(ctx, access.ty)?;
    if let Leaf::SingleEnum { variant, args } = leaf {
        // Its one value, whatever the bits say.
        let value = Rvalue::Aggregate(AggregateKind::Adt(variant, args), Box::default());
        return Ok(emit.assign(body, access.ty, value));
    }
    let mut bits = backing_of(ctx, body, emit, access.carrier, access.backing);
    if access.offset != 0 {
        bits = with_const(ctx, body, emit, BinOp::Shr, bits, unsigned, u128::from(access.offset));
    }
    let w = access.width.bits;
    let cast = |emit: &mut Emit<'_>, body: &mut MirBody<'db>, from: LocalId, kind, to: Ty<'db>| {
        emit.assign(body, to, Rvalue::Cast(kind, moved(from), to.store()))
    };
    Ok(match leaf {
        Leaf::SingleEnum { .. } => unreachable!("read above"),
        Leaf::Int => {
            // A standard integer is as wide as its field, so the cast's truncation keeps all
            // of it. An `iN`/`uN` is narrower than the integer that stores it: its bits are
            // cut out (and sign-extended) here, so the cast only ever sees a value in range.
            let narrower = matches!(
                access.ty.kind(),
                TyKind::Int(IntTy::Bits(_)) | TyKind::Uint(UintTy::Bits(_))
            );
            if narrower {
                bits = narrow(ctx, body, emit, bits, access.backing, access.width);
            }
            cast(emit, body, bits, CastKind::IntToInt, access.ty)
        }
        Leaf::Bool => {
            let bit = with_const(ctx, body, emit, BinOp::BitAnd, bits, unsigned, 1);
            let value =
                Rvalue::BinaryOp(BinOp::Ne, moved(bit), konst(ctx, access.backing, false, 0));
            emit.assign(body, access.ty, value)
        }
        Leaf::Enum { tag } => {
            bits = narrow(ctx, body, emit, bits, access.backing, access.width);
            let tag_value = cast(emit, body, bits, CastKind::IntToInt, tag);
            cast(emit, body, tag_value, CastKind::Transmute, access.ty)
        }
        Leaf::Nested { backing } => {
            if w < n {
                bits = with_const(ctx, body, emit, BinOp::BitAnd, bits, unsigned, mask(w));
            }
            let inner = cast(emit, body, bits, CastKind::IntToInt, int_ty(ctx, backing, false));
            cast(emit, body, inner, CastKind::Transmute, access.ty)
        }
    })
}

/// `bits`, a `backing` integer with its field shifted down to bit 0, cut to the field's
/// `width`: masked, or for a signed field sign-extended over the bits above it (shifted to the
/// backing integer's top, and an arithmetic shift brings the sign back down), as a
/// backing-wide integer of the field's signedness.
fn narrow<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &mut MirBody<'db>,
    emit: &mut Emit<'_>,
    bits: LocalId,
    backing: Integer,
    width: BitWidth,
) -> LocalId {
    let n = backing.size().bits() as u32;
    let w = width.bits;
    if w >= n {
        return bits;
    }
    let unsigned = (backing, false);
    if !width.signed {
        return with_const(ctx, body, emit, BinOp::BitAnd, bits, unsigned, mask(w));
    }
    let up = with_const(ctx, body, emit, BinOp::Shl, bits, unsigned, u128::from(n - w));
    let signed = int_ty(ctx, backing, true);
    let up = emit.assign(body, signed, Rvalue::Cast(CastKind::IntToInt, moved(up), signed.store()));
    with_const(ctx, body, emit, BinOp::Shr, up, (backing, true), u128::from(n - w))
}

/// The bits `value`, of the field type `ty` and `width`, takes in a `backing` integer, not
/// yet shifted to its field: `None` for a field of no bits.
fn bits_of<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &mut MirBody<'db>,
    emit: &mut Emit<'_>,
    (ty, width): (Ty<'db>, BitWidth),
    backing: Integer,
    value: Operand,
) -> Result<'db, Option<LocalId>> {
    let unsigned = (backing, false);
    let uint = int_ty(ctx, backing, false);
    let n = backing.size().bits() as u32;
    let w = width.bits;
    let as_uint = |emit: &mut Emit<'_>, body: &mut MirBody<'db>, value: Operand| {
        emit.assign(body, uint, Rvalue::Cast(CastKind::IntToInt, value, uint.store()))
    };
    let bits = match leaf(ctx, ty)? {
        Leaf::SingleEnum { .. } if w == 0 => return Ok(None),
        // `bool as uN` is 0 or 1; an unsigned integer is zero-extended and no wider than its
        // field.
        Leaf::Bool => return Ok(Some(as_uint(emit, body, value))),
        Leaf::Int if !width.signed => return Ok(Some(as_uint(emit, body, value))),
        // Sign-extended: cut to the field.
        Leaf::Int => as_uint(emit, body, value),
        Leaf::SingleEnum { .. } | Leaf::Enum { .. } => {
            // The discriminant, through a place, as `Discriminant` reads one.
            let held = emit.assign(body, ty, Rvalue::Use(value));
            let discr_ty = ty.discriminant_ty(ctx.infcx.interner);
            let discr =
                emit.assign(body, discr_ty, Rvalue::Discriminant(Place::from(held).store()));
            as_uint(emit, body, moved(discr))
        }
        Leaf::Nested { backing: inner } => {
            let inner_ty = int_ty(ctx, inner, false);
            let raw = emit.assign(
                body,
                inner_ty,
                Rvalue::Cast(CastKind::Transmute, value, inner_ty.store()),
            );
            // An inner padding bit (set by a transmute into it) must not spill into the next
            // field: the mask below cuts it.
            as_uint(emit, body, moved(raw))
        }
    };
    Ok(Some(if w < n {
        with_const(ctx, body, emit, BinOp::BitAnd, bits, unsigned, mask(w))
    } else {
        bits
    }))
}

/// `access = value`: the backing integer with the field's bits replaced.
fn write<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &mut MirBody<'db>,
    emit: &mut Emit<'_>,
    access: &Access<'db>,
    value: Operand,
) -> Result<'db, ()> {
    let unsigned = (access.backing, false);
    let n = access.backing.size().bits() as u32;
    let Some(mut bits) =
        bits_of(ctx, body, emit, (access.ty, access.width), access.backing, value)?
    else {
        return Ok(());
    };
    let w = access.width.bits;
    let uint = int_ty(ctx, access.backing, false);
    if w < n {
        let old = backing_of(ctx, body, emit, access.carrier, access.backing);
        let kept = !(mask(w) << access.offset);
        let old = with_const(ctx, body, emit, BinOp::BitAnd, old, unsigned, kept);
        if access.offset != 0 {
            bits =
                with_const(ctx, body, emit, BinOp::Shl, bits, unsigned, u128::from(access.offset));
        }
        bits = emit.assign(body, uint, Rvalue::BinaryOp(BinOp::BitOr, moved(old), moved(bits)));
    }
    let new = Rvalue::Cast(CastKind::Transmute, moved(bits), access.carrier_ty.store());
    emit.assign_to(access.carrier.store(), new);
    Ok(())
}

/// A read of a bits field in `operand` taken out to a temporary before the statement.
fn lower_operand<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &mut MirBody<'db>,
    emit: &mut Emit<'_>,
    operand: &mut Operand,
) -> Result<'db, ()> {
    let (OperandKind::Copy(place) | OperandKind::Move(place)) = &operand.kind else {
        return Ok(());
    };
    let Some(access) = access(ctx, body, place.as_ref())? else { return Ok(()) };
    let value = read(ctx, body, emit, &access)?;
    operand.kind = OperandKind::Move(Place::from(value).store());
    Ok(())
}

/// What becomes of a statement whose value is `rvalue`.
enum Lowered {
    Kept,
    /// A fake borrow of a bits field: only a fake read reads it, and neither is code.
    Removed,
}

fn lower_rvalue<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &mut MirBody<'db>,
    emit: &mut Emit<'_>,
    rvalue: &mut Rvalue,
) -> Result<'db, Lowered> {
    match rvalue {
        Rvalue::Use(operand) | Rvalue::Repeat(operand, _) | Rvalue::Cast(_, operand, _) => {
            lower_operand(ctx, body, emit, operand)?
        }
        Rvalue::UnaryOp(_, operand) => lower_operand(ctx, body, emit, operand)?,
        Rvalue::BinaryOp(op, l, r) | Rvalue::CheckedBinaryOp(op, l, r) => {
            lower_operand(ctx, body, emit, l)?;
            lower_operand(ctx, body, emit, r)?;
            if matches!(op, BinOp::Eq | BinOp::Ne) {
                lower_eq(ctx, body, emit, l, r)?;
            }
        }
        Rvalue::Ref(kind, place) => {
            if let Some(access) = access(ctx, body, place.as_ref())? {
                if *kind == crate::mir::BorrowKind::Shallow {
                    return Ok(Lowered::Removed);
                }
                // Borrow checking refuses every other reference to a bits field (E0793).
                // alloc: error path
                return Err(MirLowerError::ImplementationError(format!(
                    "a {kind:?} reference to a field of `#[blaze::bits]` struct `{:?}` reached \
                     code generation: borrow checking refuses it",
                    access.carrier_ty
                )));
            }
        }
        Rvalue::Discriminant(place) => {
            if let Some(access) = access(ctx, body, place.as_ref())? {
                let value = read(ctx, body, emit, &access)?;
                *place = Place::from(value).store();
            }
        }
        Rvalue::Len(place) | Rvalue::CopyForDeref(place) => {
            if access(ctx, body, place.as_ref())?.is_some() {
                // alloc: error path
                return Err(MirLowerError::ImplementationError(format!(
                    "{place:?}, a field of a `#[blaze::bits]` struct, is used as a slice or a \
                     pointer"
                )));
            }
        }
        Rvalue::Aggregate(kind, operands) => {
            for operand in operands.iter_mut() {
                lower_operand(ctx, body, emit, operand)?;
            }
            if let AggregateKind::Adt(VariantId::StructId(strukt), args) = kind
                && crate::layout::is_bits_struct(ctx.db, *strukt)
            {
                let ty = Ty::new_adt(ctx.infcx.interner, AdtId::StructId(*strukt), args.as_ref());
                let strukt = *strukt;
                *rvalue = pack(ctx, body, emit, (strukt, ty), std::mem::take(operands))?;
            }
        }
        Rvalue::ThreadLocalRef(never) | Rvalue::AddressOf(never) | Rvalue::NullaryOp(never) => {
            match *never {}
        }
    }
    Ok(Lowered::Kept)
}

/// A literal of the bits struct `strukt` of the fields' values `operands`, in field order: its
/// backing integer, as the struct.
fn pack<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &mut MirBody<'db>,
    emit: &mut Emit<'_>,
    (strukt, ty): (hir_def::StructId, Ty<'db>),
    operands: Box<[Operand]>,
) -> Result<'db, Rvalue> {
    let layout = layout_of(ctx, strukt)?;
    let unsigned = (layout.backing, false);
    let uint = int_ty(ctx, layout.backing, false);
    if operands.len() != layout.fields.len() {
        // alloc: error path
        return Err(MirLowerError::ImplementationError(format!(
            "a literal of `#[blaze::bits]` struct {strukt:?} has {} values for {} fields",
            operands.len(),
            layout.fields.len()
        )));
    }
    let mut packed: Option<LocalId> = None;
    for (field, value) in layout.fields.iter().zip(operands.into_vec()) {
        let Some(mut bits) =
            bits_of(ctx, body, emit, (field.ty.as_ref(), field.width), layout.backing, value)?
        else {
            continue;
        };
        if field.offset != 0 {
            bits =
                with_const(ctx, body, emit, BinOp::Shl, bits, unsigned, u128::from(field.offset));
        }
        packed = Some(match packed {
            None => bits,
            Some(sum) => {
                emit.assign(body, uint, Rvalue::BinaryOp(BinOp::BitOr, moved(sum), moved(bits)))
            }
        });
    }
    let packed = match packed {
        Some(packed) => moved(packed),
        None => konst(ctx, layout.backing, false, 0),
    };
    Ok(Rvalue::Cast(CastKind::Transmute, packed, ty.store()))
}

/// `l == r` (or `!=`) of two bits structs, as a constant pattern compiles to: the bits their
/// fields take compared, the padding above them left out.
fn lower_eq<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &mut MirBody<'db>,
    emit: &mut Emit<'_>,
    l: &mut Operand,
    r: &mut Operand,
) -> Result<'db, ()> {
    let ty = ctx.operand_ty(body, l);
    let Some(strukt) = bits_struct_of(ctx.db, ctx.normalize(ty)) else { return Ok(()) };
    let layout = layout_of(ctx, strukt)?;
    let unsigned = (layout.backing, false);
    let uint = int_ty(ctx, layout.backing, false);
    let n = layout.backing_bits();
    for side in [l, r] {
        let value = std::mem::replace(side, konst(ctx, layout.backing, false, 0));
        let mut bits =
            emit.assign(body, uint, Rvalue::Cast(CastKind::Transmute, value, uint.store()));
        if layout.used < n {
            bits = with_const(ctx, body, emit, BinOp::BitAnd, bits, unsigned, mask(layout.used));
        }
        *side = moved(bits);
    }
    Ok(())
}

fn lower_statement<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &mut MirBody<'db>,
    out: &mut Vec<Statement>,
    statement: Statement,
) -> Result<'db, ()> {
    let Statement { kind, span } = statement;
    let mut emit = Emit { out, span };
    let kind = match kind {
        StatementKind::Assign(place, mut rvalue) => {
            if let Lowered::Removed = lower_rvalue(ctx, body, &mut emit, &mut rvalue)? {
                return Ok(());
            }
            match access(ctx, body, place.as_ref())? {
                // The value first, then the read-modify-write of the backing integer.
                Some(access) => {
                    let value = emit.assign(body, access.ty, rvalue);
                    write(ctx, body, &mut emit, &access, moved(value))?;
                }
                None => emit.assign_to(place, rvalue),
            }
            return Ok(());
        }
        other => other,
    };
    match &kind {
        StatementKind::Assign(..) => unreachable!("lowered above"),
        // A bits field is no place of its own to read fake or to mark uninitialized: a write to
        // it rewrites its bits whole.
        StatementKind::FakeRead(place) | StatementKind::Deinit(place) => {
            if access(ctx, body, place.as_ref())?.is_some() {
                return Ok(());
            }
        }
        StatementKind::InlineAsm(asm) => {
            for operand in asm.operands.iter() {
                let places = match operand {
                    AsmOperand::Reg { input, output, .. } => {
                        let input = input.as_ref().and_then(|it| match &it.kind {
                            OperandKind::Copy(place) | OperandKind::Move(place) => Some(place),
                            _ => None,
                        });
                        [input, output.as_ref()]
                    }
                    AsmOperand::Const(_) | AsmOperand::Label | AsmOperand::Sym(_) => [None, None],
                };
                for place in places.into_iter().flatten() {
                    if access(ctx, body, place.as_ref())?.is_some() {
                        // alloc: error path
                        return Err(MirLowerError::NotSupported(format!(
                            "a field of a `#[blaze::bits]` struct ({place:?}) as an `asm!` \
                             operand: copy it to a local first"
                        )));
                    }
                }
            }
        }
        StatementKind::StorageLive(_) | StatementKind::StorageDead(_) | StatementKind::Nop => {}
    }
    emit.out.push(Statement { kind, span });
    Ok(())
}

fn lower_terminator<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &mut MirBody<'db>,
    out: &mut Vec<Statement>,
    mut terminator: Terminator,
) -> Result<'db, Terminator> {
    let mut emit = Emit { out, span: terminator.span };
    let emit = &mut emit;
    match &mut terminator.kind {
        TerminatorKind::SwitchInt { discr, .. } => lower_operand(ctx, body, emit, discr)?,
        TerminatorKind::Assert { cond, .. } => lower_operand(ctx, body, emit, cond)?,
        TerminatorKind::TailCall { func, args, .. } => {
            lower_operand(ctx, body, emit, func)?;
            for arg in args.iter_mut() {
                lower_operand(ctx, body, emit, arg)?;
            }
        }
        TerminatorKind::Call { func, args, destination, target, .. } => {
            lower_operand(ctx, body, emit, func)?;
            for arg in args.iter_mut() {
                lower_operand(ctx, body, emit, arg)?;
            }
            if let Some(access) = access(ctx, body, destination.as_ref())? {
                let span = emit.span;
                *destination = written_after(ctx, body, span, &access, target)?;
            }
        }
        TerminatorKind::Yield { value, resume_arg, resume, .. } => {
            lower_operand(ctx, body, emit, value)?;
            if let Some(access) = access(ctx, body, resume_arg.as_ref())? {
                let span = emit.span;
                let mut target = Some(*resume);
                *resume_arg = written_after(ctx, body, span, &access, &mut target)?;
                *resume = target.expect("kept");
            }
        }
        TerminatorKind::Drop { place, target, .. } => {
            if let Some(access) = access(ctx, body, place.as_ref())? {
                if ctx.needs_drop(access.ty) {
                    // `bits_layout` refuses a field type that needs dropping.
                    // alloc: error path
                    return Err(MirLowerError::ImplementationError(format!(
                        "{place:?}, a field of a `#[blaze::bits]` struct, is dropped"
                    )));
                }
                // Nothing to drop.
                terminator.kind = TerminatorKind::Goto { target: *target };
            }
        }
        TerminatorKind::DropAndReplace { place, value, target, .. } => {
            lower_operand(ctx, body, emit, value)?;
            if let Some(access) = access(ctx, body, place.as_ref())? {
                if ctx.needs_drop(access.ty) {
                    // alloc: error path
                    return Err(MirLowerError::ImplementationError(format!(
                        "{place:?}, a field of a `#[blaze::bits]` struct, is dropped"
                    )));
                }
                let value = std::mem::replace(value, konst(ctx, Integer::I8, false, 0));
                write(ctx, body, emit, &access, value)?;
                terminator.kind = TerminatorKind::Goto { target: *target };
            }
        }
        TerminatorKind::Goto { .. }
        | TerminatorKind::UnwindResume
        | TerminatorKind::Abort
        | TerminatorKind::Return
        | TerminatorKind::Unreachable
        | TerminatorKind::CoroutineDrop
        | TerminatorKind::FalseEdge { .. }
        | TerminatorKind::FalseUnwind { .. }
        | TerminatorKind::ComptimeFor { .. }
        | TerminatorKind::ComptimeNext { .. } => {}
    }
    Ok(terminator)
}

/// A temporary for a call or a resume to write to in place of the bits field `access`, and,
/// when `target` returns, a block before it that writes the temporary to the field.
fn written_after<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &mut MirBody<'db>,
    span: MirSpan,
    access: &Access<'db>,
    target: &mut Option<crate::mir::BasicBlockId>,
) -> Result<'db, StoredPlace> {
    let temp = body.locals.alloc(Local { ty: access.ty.store() });
    if let Some(next) = *target {
        // alloc: the new block's statements, kept in the body
        let mut statements = Vec::new();
        let mut emit = Emit { out: &mut statements, span };
        write(ctx, body, &mut emit, access, moved(temp))?;
        let block = body.basic_blocks.alloc(BasicBlock {
            statements,
            terminator: Some(Terminator { span, kind: TerminatorKind::Goto { target: next } }),
            is_cleanup: body.basic_blocks[next].is_cleanup,
        });
        *target = Some(block);
    }
    Ok(Place::from(temp).store())
}

#[cfg(test)]
mod tests {
    use blaze_mem::{Tag, frame};
    use hir_expand::fixture;

    use crate::{db::HirDatabase, test_db::TestDBExt as _};

    /// A program that names no `#[blaze::bits]` struct gives the pass nothing to do, so it
    /// returns before touching the body: runtime MIR, and so the code made from it, is byte for
    /// byte what it was before the extension. The fixture has every shape the pass rewrites in a
    /// bits struct (field reads and writes, a literal and a `..` update, a reference to a field,
    /// `==` of two structs, a pattern) on a plain and a packed struct, whose layouts stay theirs.
    #[test]
    fn a_program_without_bits_structs_is_left_as_it_was() {
        let fx = fixture::db(
            r#"
//- minicore: copy, clone, derive, eq, add
#[derive(Clone, Copy, PartialEq)]
struct P { a: u8, b: bool }
#[repr(packed)]
#[derive(Clone, Copy)]
struct Q { a: u8, b: u32 }
fn f(mut p: P, q: Q) -> bool {
    p.a += 1;
    let r = &p.b;
    let P { a, .. } = p;
    let s = P { a: 3, ..p };
    let b = { q.b };
    *r && s == p && a == 0 && b == 1
}
"#,
        );
        let file_ids = fx.files().to_vec();
        let db = fx.into_db();
        blaze_query::db::attach(&db, || {
            let file_id = *file_ids.last().unwrap();
            let module_id = db.module_for_file(file_id.file_id(&db));
            let def_map = module_id.def_map(&db);
            let scope = &def_map.module(&db, module_id).scope(&db);
            let mut checked = 0;
            for def in scope.declarations() {
                match def {
                    hir_def::ModuleDefId::FunctionId(func) => {
                        let body = db.mir_body(func.into()).expect("`f` lowers");
                        frame::task(Tag::Mir, |frame| {
                            let ctx = super::super::Ctx::new(&db, body.owner, frame);
                            assert!(!super::uses_bits(&ctx, body).expect("`f` is scanned"));
                        });
                        checked += 1;
                    }
                    hir_def::ModuleDefId::AdtId(hir_def::AdtId::StructId(id)) => {
                        assert!(!crate::layout::is_bits_struct(&db, id));
                    }
                    _ => {}
                }
            }
            assert_eq!(checked, 1);
        })
    }
}
