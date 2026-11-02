//! rustc's `CheckAlignment` and `CheckNull` (`check_alignment.rs`, `check_null.rs` and the
//! `check_pointers.rs` they share): before each statement that reads, writes or borrows
//! through a raw pointer to a sized type, an `Assert` that the pointer is aligned for the type,
//! and one that it is not null. They are part of what UB checks mean, not optimizations: they
//! run at every level when the crate checks UB ([`MirOptions::ub_checks`]).
//!
//! rustc's MIR has had `Derefer`, `ElaborateBoxDerefs` and `LowerIntrinsics` by then and this
//! MIR has not, so the passes read it as rustc's would be:
//! - A place with several derefs is checked at its last: `Derefer` copies the pointers of the
//!   earlier ones out with `CopyForDeref`, which is not checked.
//! - A deref of a `Box<T>` is one of the `*const T` `ElaborateBoxDerefs` transmutes the box's
//!   first field to.
//! - A call of an intrinsic that `LowerIntrinsics` makes a statement ([`lowered_intrinsic`]) is
//!   checked as that statement, the last of its block: `read_via_copy` reads `*ptr`, and the
//!   call writes its destination. Other calls are terminators, which rustc does not check.
//! - A `Ref` into a raw pointer is rustc's `RawPtr`, which is not checked.
//!
//! [`MirOptions::ub_checks`]: crate::mir::MirOptions::ub_checks

use blaze_mem::Frame;
use intern::Symbol;
use rustc_type_ir::inherent::{IntoKind, Ty as _};
use smallvec::SmallVec;

use crate::{
    consteval::mir_const_alias_kind,
    intrinsic::Intrinsic,
    mir::{
        AssertKind, BasicBlock, BasicBlockId, BinOp, BorrowKind, CastKind, FieldIndex, Local,
        LocalId, MirBody, MirLowerError, MirSpan, Operand, OperandKind, Place, Projection,
        ProjectionElem, Rvalue, Statement, StatementKind, StoredPlace, Terminator,
        TerminatorKind, UnOp, framed::block_id, transform::Ctx,
    },
    next_solver::{AliasConst, Const, ConstExt, GenericArgs, IsRigid, Ty, TyKind},
};

type Result<'db, T> = std::result::Result<T, MirLowerError<'db>>;

/// Which of rustc's two pointer checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PointerCheck {
    /// `CheckAlignment`: `bool`, `i8` and `u8` are never misaligned, and a borrow of a field
    /// is checked for the field's type (`FollowProjections`).
    Alignment,
    /// `CheckNull`: a borrow is checked for the pointer's pointee type, and is never null; a
    /// read or write is, through a pointer to a type that is not zero-sized.
    Null,
}

/// How a statement uses a place, of rustc's `PlaceContext`s the checks look at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Use {
    Store,
    Copy,
    Move,
    SharedBorrow,
    MutBorrow,
}

impl Use {
    fn is_borrow(self) -> bool {
        matches!(self, Use::SharedBorrow | Use::MutBorrow)
    }
}

/// What rustc's `LowerIntrinsics` makes of a call of an intrinsic: a statement at the end of
/// the block, which these passes check as rustc does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Lowered {
    /// `dest = arg as Dst (Transmute)`.
    Transmute,
    /// `dest = <rvalue of the arguments>`: the arithmetic intrinsics, `offset`,
    /// `aggregate_raw_ptr`, `ptr_metadata`.
    Assign,
    /// `dest = copy (*ptr)`.
    ReadViaCopy,
    /// `(*ptr) = move value`, as rustc's MIR building writes it.
    WriteViaMove,
    /// `dest = <a constant or a discriminant>`.
    Destination,
    /// `Intrinsic(..)` of the arguments: `copy_nonoverlapping`, `assume`.
    Operands,
    /// `dest = &(*ptr)[index]`, or `&raw` of it.
    SliceGetUnchecked,
    /// `unreachable`: no statement.
    Nothing,
}

/// What `LowerIntrinsics` makes of the call `kind`, when it calls an intrinsic it lowers.
pub(super) fn lowered_intrinsic(kind: &TerminatorKind) -> Option<Lowered> {
    let TerminatorKind::Call { intrinsic: Some(intrinsic), .. } = kind else { return None };
    lowered(*intrinsic)
}

/// What `LowerIntrinsics` makes of a call of `intrinsic`, when it lowers it.
pub(super) fn lowered(intrinsic: Intrinsic) -> Option<Lowered> {
    Some(match intrinsic {
        Intrinsic::Transmute | Intrinsic::TransmuteUnchecked => Lowered::Transmute,
        Intrinsic::WrappingAdd
        | Intrinsic::WrappingSub
        | Intrinsic::WrappingMul
        | Intrinsic::ThreeWayCompare
        | Intrinsic::UncheckedAdd
        | Intrinsic::UncheckedSub
        | Intrinsic::UncheckedMul
        | Intrinsic::UncheckedDiv
        | Intrinsic::UncheckedRem
        | Intrinsic::UncheckedShl
        | Intrinsic::UncheckedShr
        | Intrinsic::AddWithOverflow
        | Intrinsic::SubWithOverflow
        | Intrinsic::MulWithOverflow
        | Intrinsic::Offset
        | Intrinsic::AggregateRawPtr
        | Intrinsic::PtrMetadata => Lowered::Assign,
        Intrinsic::ReadViaCopy => Lowered::ReadViaCopy,
        Intrinsic::WriteViaMove => Lowered::WriteViaMove,
        Intrinsic::DiscriminantValue
        | Intrinsic::Forget
        | Intrinsic::UbChecks
        | Intrinsic::OverflowChecks => Lowered::Destination,
        Intrinsic::CopyNonoverlapping | Intrinsic::Assume => Lowered::Operands,
        Intrinsic::SliceGetUnchecked => Lowered::SliceGetUnchecked,
        Intrinsic::Unreachable => Lowered::Nothing,
        _ => return None,
    })
}

/// Where in a block a check goes: before a statement, or before the call of an intrinsic
/// `LowerIntrinsics` makes the block's last statement.
#[derive(Debug, Clone, Copy)]
pub(super) enum Site {
    Statement(usize),
    Call(Lowered),
}

/// The sites of `block` in the order rustc visits them: backwards, so that a split leaves the
/// sites still to come where they were. It doesn't borrow the body, which the splits change.
pub(super) fn sites(
    body: &MirBody<'_>,
    block: BasicBlockId,
) -> impl Iterator<Item = Site> + use<> {
    let data = &body.basic_blocks[block];
    let call = data.terminator.as_ref().and_then(|it| lowered_intrinsic(&it.kind));
    let statements = (0..data.statements.len()).rev().map(Site::Statement);
    call.map(Site::Call).into_iter().chain(statements)
}

/// The statement index a site splits its block at.
pub(super) fn split_index(body: &MirBody<'_>, block: BasicBlockId, site: Site) -> usize {
    match site {
        Site::Statement(at) => at,
        Site::Call(_) => body.basic_blocks[block].statements.len(),
    }
}

pub(super) fn site_span(body: &MirBody<'_>, block: BasicBlockId, site: Site) -> MirSpan {
    let data = &body.basic_blocks[block];
    match site {
        Site::Statement(at) => data.statements[at].span,
        Site::Call(_) => data.terminator.as_ref().map_or(MirSpan::Unknown, |it| it.span),
    }
}

/// Moves the statements of `block` from `at` and its terminator to a new block, rustc's
/// `split_block`, and returns it.
pub(super) fn split_block(body: &mut MirBody<'_>, block: BasicBlockId, at: usize) -> BasicBlockId {
    let data = &mut body.basic_blocks[block];
    let statements = data.statements.split_off(at);
    let terminator = data.terminator.take();
    let is_cleanup = data.is_cleanup;
    body.basic_blocks.alloc(BasicBlock { statements, terminator, is_cleanup })
}

/// How the pointer a place goes through is read.
#[derive(Debug, Clone)]
enum Pointer<'db> {
    /// A raw pointer: the place before its last deref.
    Raw(StoredPlace),
    /// A `Box` of this type, before the last deref: its first field transmuted to a
    /// `*const` of it (`ElaborateBoxDerefs`).
    Box(StoredPlace, Ty<'db>),
}

struct Found<'db> {
    pointer: Pointer<'db>,
    pointee: Ty<'db>,
    used: Use,
}

/// rustc's `PointerFinder`: the pointers a statement reads, writes or borrows through, in the
/// order rustc's visitor finds them.
struct Finder<'a, 'f, 'db> {
    ctx: &'a Ctx<'f, 'db>,
    body: &'a MirBody<'db>,
    check: PointerCheck,
    found: SmallVec<[Found<'db>; 2]>,
}

impl<'db> Finder<'_, '_, 'db> {
    fn ty_of(&self, place: &StoredPlace) -> Ty<'db> {
        self.ctx.normalize(place.as_ref().ty(self.body, &self.ctx.infcx, self.ctx.env).ty)
    }

    fn place(&mut self, place: &StoredPlace, used: Use) {
        let projection = place.projection.as_slice();
        let Some(last) = projection.iter().rposition(|it| matches!(it, ProjectionElem::Deref))
        else {
            return;
        };
        let prefix = StoredPlace {
            local: place.local,
            projection: Projection::new_from_slice(&projection[..last]).store(),
        };
        let prefix_ty = self.ty_of(&prefix);
        let (pointer, mut pointee) = if let TyKind::RawPtr(pointee, _) = prefix_ty.kind() {
            (Pointer::Raw(prefix), pointee)
        } else if let Some(boxed) = prefix_ty.boxed_ty() {
            (Pointer::Box(prefix, boxed), boxed)
        } else {
            return;
        };
        if self.check == PointerCheck::Alignment && used.is_borrow() {
            pointee = self.ty_of(place);
        }
        if !self.ctx.is_sized(pointee) {
            return;
        }
        let element = match pointee.kind() {
            TyKind::Array(element, _) => element,
            _ => pointee,
        };
        let excluded = self.check == PointerCheck::Alignment
            && matches!(
                element.kind(),
                TyKind::Bool
                    | TyKind::Int(rustc_type_ir::IntTy::I8)
                    | TyKind::Uint(rustc_type_ir::UintTy::U8)
            );
        if !excluded {
            self.found.push(Found { pointer, pointee, used });
        }
    }

    fn operand(&mut self, operand: &Operand) {
        match &operand.kind {
            OperandKind::Copy(place) => self.place(place, Use::Copy),
            OperandKind::Move(place) => self.place(place, Use::Move),
            _ => (),
        }
    }

    /// rustc's `super_assign`: the place written, then what the rvalue reads.
    fn statement(&mut self, kind: &StatementKind) {
        let StatementKind::Assign(place, rvalue) = kind else { return };
        self.place(place, Use::Store);
        match rvalue {
            Rvalue::Ref(kind, borrowed) => {
                if matches!(self.ty_of(place).kind(), TyKind::RawPtr(..)) {
                    return;
                }
                match kind {
                    BorrowKind::Shared => self.place(borrowed, Use::SharedBorrow),
                    BorrowKind::Mut { .. } => self.place(borrowed, Use::MutBorrow),
                    BorrowKind::Shallow => (),
                }
            }
            Rvalue::Len(_) | Rvalue::Discriminant(_) | Rvalue::CopyForDeref(_) => (),
            _ => rvalue.for_each_operand(&mut |it| self.operand(it)),
        }
    }

    /// The statement `LowerIntrinsics` makes of the call `kind`.
    fn call(&mut self, kind: &TerminatorKind, lowered: Lowered) {
        let TerminatorKind::Call { args, destination, .. } = kind else { return };
        let deref = |pointer: &Operand| match &pointer.kind {
            OperandKind::Copy(place) | OperandKind::Move(place) => {
                Some(project(place, ProjectionElem::Deref))
            }
            _ => None,
        };
        match lowered {
            Lowered::Transmute | Lowered::Assign => {
                self.place(destination, Use::Store);
                args.iter().for_each(|it| self.operand(it));
            }
            Lowered::ReadViaCopy => {
                self.place(destination, Use::Store);
                if let Some(read) = args.first().and_then(deref) {
                    self.place(&read, Use::Copy);
                }
            }
            Lowered::WriteViaMove => {
                let [pointer, value] = &args[..] else { return };
                if let Some(written) = deref(pointer) {
                    self.place(&written, Use::Store);
                }
                self.operand(value);
            }
            Lowered::Destination => self.place(destination, Use::Store),
            Lowered::Operands => args.iter().for_each(|it| self.operand(it)),
            Lowered::SliceGetUnchecked => {
                self.place(destination, Use::Store);
                let [pointer, index] = &args[..] else { return };
                let (Some(element), OperandKind::Copy(index) | OperandKind::Move(index)) =
                    (deref(pointer), &index.kind)
                else {
                    return;
                };
                if !index.projection.as_slice().is_empty() {
                    return;
                }
                let used = match self.ty_of(destination).kind() {
                    TyKind::Ref(_, _, rustc_ast_ir::Mutability::Not) => Use::SharedBorrow,
                    TyKind::Ref(_, _, rustc_ast_ir::Mutability::Mut) => Use::MutBorrow,
                    _ => return,
                };
                self.place(&project(&element, ProjectionElem::Index(index.local)), used);
            }
            Lowered::Nothing => (),
        }
    }
}

/// `place` with `elem` after its projections.
pub(super) fn project(place: &StoredPlace, elem: ProjectionElem<LocalId>) -> StoredPlace {
    let projection = place.projection.as_slice().iter().copied().chain([elem]);
    StoredPlace { local: place.local, projection: Projection::new_from_iter(projection).store() }
}

/// rustc's `check_pointers` with the check of `check`, when the crate checks UB.
pub(super) fn check_pointers<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &mut MirBody<'db>,
    check: PointerCheck,
    frame: &Frame,
) -> Result<'db, ()> {
    // A crate with no panic handler would not build with the panics these add.
    if ctx.infcx.interner.lang_items().PanicImpl.is_none() {
        return Ok(());
    }
    let consts = Consts::new(ctx)?;
    let mut setup: Setup<'_> = Vec::new_in(frame);
    // The blocks there were before any check: the ones splits make come after them.
    for index in (0..body.basic_blocks.len()).rev() {
        let block = block_id(index);
        for site in sites(body, block) {
            let found = {
                let mut finder = Finder { ctx, body, check, found: SmallVec::new() };
                match site {
                    Site::Statement(at) => {
                        finder.statement(&body.basic_blocks[block].statements[at].kind)
                    }
                    Site::Call(lowered) => {
                        if let Some(terminator) = &body.basic_blocks[block].terminator {
                            finder.call(&terminator.kind, lowered);
                        }
                    }
                }
                finder.found
            };
            if found.is_empty() {
                continue;
            }
            let span = site_span(body, block, site);
            let at = split_index(body, block, site);
            for found in found {
                let next = split_block(body, block, at);
                let (cond, msg) = match check {
                    PointerCheck::Alignment => {
                        alignment_check(ctx, &consts, body, &found, &mut setup, span)?
                    }
                    PointerCheck::Null => null_check(ctx, &consts, body, &found, &mut setup, span)?,
                };
                append(body, block, &mut setup);
                // The panics are `#[rustc_nounwind]`: a failed check does not unwind
                // (rustc's `UnwindAction::Unreachable`).
                body.basic_blocks[block].terminator = Some(Terminator {
                    span,
                    kind: TerminatorKind::Assert {
                        cond,
                        expected: true,
                        msg,
                        target: next,
                        cleanup: None,
                    },
                });
            }
        }
    }
    Ok(())
}

/// The types and constants every check of a body uses, made once.
pub(super) struct Consts<'db> {
    pub(super) usize_ty: Ty<'db>,
    pub(super) bool_ty: Ty<'db>,
    pub(super) u128_ty: Ty<'db>,
    unit_ptr: Ty<'db>,
    word: usize,
}

impl<'db> Consts<'db> {
    pub(super) fn new(ctx: &Ctx<'_, 'db>) -> Result<'db, Consts<'db>> {
        let interner = ctx.infcx.interner;
        let usize_ty = Ty::new_uint(interner, rustc_type_ir::UintTy::Usize);
        let Some(word) = ctx.size_of(usize_ty) else {
            return Err(MirLowerError::NotSupported(
                // alloc: error path
                "UB checks: the target has no layout of `usize`".to_owned(),
            ));
        };
        Ok(Consts {
            usize_ty,
            bool_ty: ctx.bool_ty(),
            u128_ty: Ty::new_uint(interner, rustc_type_ir::UintTy::U128),
            unit_ptr: Ty::new_imm_ptr(interner, Ty::new_unit(interner)),
            word: word as usize,
        })
    }

    fn usize(&self, value: u64) -> Operand {
        Operand::from_bytes(value.to_le_bytes()[..self.word].into(), self.usize_ty)
    }

    pub(super) fn u128(&self, value: u128) -> Operand {
        Operand::from_bytes(value.to_le_bytes().into(), self.u128_ty)
    }

    pub(super) fn bool(&self, value: bool) -> Operand {
        // alloc: the constant's bytes, kept in the body
        Operand::from_bytes(Box::new([u8::from(value)]), self.bool_ty)
    }
}

/// The statements a check computes its condition with, before they go into their block.
pub(super) type Setup<'f> = Vec<Statement, &'f Frame>;

/// `setup`'s statements moved to the end of `block`'s, which grow to exactly their new length.
pub(super) fn append(body: &mut MirBody<'_>, block: BasicBlockId, setup: &mut Setup<'_>) {
    let statements = &mut body.basic_blocks[block].statements;
    statements.reserve_exact(setup.len());
    statements.extend(setup.drain(..));
}

pub(super) fn temp<'db>(body: &mut MirBody<'db>, ty: Ty<'db>) -> LocalId {
    body.locals.alloc(Local { ty: ty.store() })
}

pub(super) fn assign(local: LocalId, rvalue: Rvalue, span: MirSpan) -> Statement {
    Statement { kind: StatementKind::Assign(Place::from(local).store(), rvalue), span }
}

pub(super) fn copy_of(local: LocalId) -> Operand {
    Operand { kind: OperandKind::Copy(Place::from(local).store()), span: None }
}

/// `<ty as SizedTypeProperties>::SIZE` or `ALIGN`, the lang item `item` of core, unevaluated:
/// what rustc's `Operand::unevaluated_constant` makes of the `SizeOf` or `AlignOf` lang item.
fn type_property<'db>(
    ctx: &Ctx<'_, 'db>,
    consts: &Consts<'db>,
    item: Option<hir_def::ConstId>,
    name: &str,
    ty: Ty<'db>,
) -> Result<'db, Operand> {
    let Some(item) = item else {
        // alloc: error path
        return Err(MirLowerError::NotSupported(format!(
            "UB checks: no `{name}` lang item in core"
        )));
    };
    let args = GenericArgs::new_from_slice(&[ty.into()]);
    let interner = ctx.infcx.interner;
    let kind = mir_const_alias_kind(ctx.db, item);
    let konst = Const::new_alias(interner, IsRigid::No, AliasConst::new(interner, kind, args));
    Ok(Operand {
        kind: OperandKind::Constant { konst: konst.store(), ty: consts.usize_ty.store() },
        span: None,
    })
}

/// The pointer `found` goes through as a `*const ()`, then its address (`ptr.addr()`).
fn address<'db>(
    ctx: &Ctx<'_, 'db>,
    consts: &Consts<'db>,
    body: &mut MirBody<'db>,
    found: &Found<'db>,
    setup: &mut Setup<'_>,
    span: MirSpan,
) -> LocalId {
    let pointer = match &found.pointer {
        // clone: the pointer's place, kept in the body: a refcount
        Pointer::Raw(place) => Operand { kind: OperandKind::Copy(place.clone()), span: None },
        Pointer::Box(place, boxed) => {
            let raw_ty = Ty::new_imm_ptr(ctx.infcx.interner, *boxed);
            let raw = temp(body, raw_ty);
            let field = project(place, ProjectionElem::Field(FieldIndex(0)));
            let field = Operand { kind: OperandKind::Copy(field), span: None };
            setup.push(assign(raw, Rvalue::Cast(CastKind::Transmute, field, raw_ty.store()), span));
            copy_of(raw)
        }
    };
    let thin = temp(body, consts.unit_ptr);
    let cast = Rvalue::Cast(CastKind::PtrToPtr, pointer, consts.unit_ptr.store());
    setup.push(assign(thin, cast, span));
    let addr = temp(body, consts.usize_ty);
    let cast = Rvalue::Cast(CastKind::Transmute, copy_of(thin), consts.usize_ty.store());
    setup.push(assign(addr, cast, span));
    addr
}

/// rustc's `insert_alignment_check`.
fn alignment_check<'db>(
    ctx: &Ctx<'_, 'db>,
    consts: &Consts<'db>,
    body: &mut MirBody<'db>,
    found: &Found<'db>,
    setup: &mut Setup<'_>,
    span: MirSpan,
) -> Result<'db, (Operand, AssertKind)> {
    let addr = address(ctx, consts, body, found, setup, span);
    let align_of = ctx.infcx.interner.lang_items().AlignOf;
    let alignment = type_property(ctx, consts, align_of, "mem_align_const", found.pointee)?;
    let mask = temp(body, consts.usize_ty);
    // clone: an interned constant's handle, read twice in the body
    let minus_one = Rvalue::BinaryOp(BinOp::Sub, alignment.clone(), consts.usize(1));
    setup.push(assign(mask, minus_one, span));
    if let Some(max) = max_reliable_alignment(ctx) {
        let limit = Rvalue::BinaryOp(BinOp::BitAnd, copy_of(mask), consts.usize(max - 1));
        setup.push(assign(mask, limit, span));
    }
    let bits = temp(body, consts.usize_ty);
    setup.push(assign(bits, Rvalue::BinaryOp(BinOp::BitAnd, copy_of(addr), copy_of(mask)), span));
    let is_ok = temp(body, consts.bool_ty);
    setup.push(assign(is_ok, Rvalue::BinaryOp(BinOp::Eq, copy_of(bits), consts.usize(0)), span));
    let msg = AssertKind::MisalignedPointerDereference { required: alignment, found: copy_of(addr) };
    Ok((copy_of(is_ok), msg))
}

/// rustc's `Target::max_reliable_alignment`: MSVC on 32-bit x86 aligns the stack to 4 bytes,
/// so a larger alignment is not one a pointer can be relied on to have.
fn max_reliable_alignment(ctx: &Ctx<'_, '_>) -> Option<u64> {
    let cfg = ctx.krate().cfg_options(ctx.db);
    let is = |key: &str, value: &str| {
        cfg.check_atom(&cfg::CfgAtom::KeyValue {
            key: Symbol::intern(key),
            value: Symbol::intern(value),
        })
    };
    let like_windows = is("target_os", "windows") || is("target_os", "uefi");
    (like_windows && is("target_arch", "x86")).then_some(4)
}

/// rustc's `insert_null_check`.
fn null_check<'db>(
    ctx: &Ctx<'_, 'db>,
    consts: &Consts<'db>,
    body: &mut MirBody<'db>,
    found: &Found<'db>,
    setup: &mut Setup<'_>,
    span: MirSpan,
) -> Result<'db, (Operand, AssertKind)> {
    let addr = address(ctx, consts, body, found, setup, span);
    // A borrow of null is UB also of a zero-sized type; a read or write is not.
    let (checked, msg) = if found.used.is_borrow() {
        (consts.bool(true), AssertKind::NullReferenceConstructed)
    } else {
        let size_of = ctx.infcx.interner.lang_items().SizeOf;
        let size = type_property(ctx, consts, size_of, "mem_size_const", found.pointee)?;
        let checked = temp(body, consts.bool_ty);
        setup.push(assign(checked, Rvalue::BinaryOp(BinOp::Ne, size, consts.usize(0)), span));
        (copy_of(checked), AssertKind::NullPointerDereference)
    };
    let is_null = temp(body, consts.bool_ty);
    setup.push(assign(is_null, Rvalue::BinaryOp(BinOp::Eq, copy_of(addr), consts.usize(0)), span));
    let fails = temp(body, consts.bool_ty);
    setup.push(assign(fails, Rvalue::BinaryOp(BinOp::BitAnd, copy_of(is_null), checked), span));
    let is_ok = temp(body, consts.bool_ty);
    setup.push(assign(is_ok, Rvalue::UnaryOp(UnOp::Not, copy_of(fails)), span));
    Ok((copy_of(is_ok), msg))
}

#[cfg(test)]
mod tests {
    use super::{Lowered, lowered};
    use crate::intrinsic::{AtomicOp, AtomicOrders, Intrinsic, SimdOp};

    #[test]
    fn what_lower_intrinsics_makes_of_each_intrinsic() {
        assert_eq!(lowered(Intrinsic::Transmute), Some(Lowered::Transmute));
        assert_eq!(lowered(Intrinsic::TransmuteUnchecked), Some(Lowered::Transmute));
        assert_eq!(lowered(Intrinsic::UncheckedShr), Some(Lowered::Assign));
        assert_eq!(lowered(Intrinsic::PtrMetadata), Some(Lowered::Assign));
        assert_eq!(lowered(Intrinsic::ReadViaCopy), Some(Lowered::ReadViaCopy));
        assert_eq!(lowered(Intrinsic::WriteViaMove), Some(Lowered::WriteViaMove));
        assert_eq!(lowered(Intrinsic::OverflowChecks), Some(Lowered::Destination));
        assert_eq!(lowered(Intrinsic::Forget), Some(Lowered::Destination));
        assert_eq!(lowered(Intrinsic::Assume), Some(Lowered::Operands));
        assert_eq!(lowered(Intrinsic::SliceGetUnchecked), Some(Lowered::SliceGetUnchecked));
        assert_eq!(lowered(Intrinsic::Unreachable), Some(Lowered::Nothing));
        // Calls `LowerIntrinsics` leaves terminators.
        for kept in [
            Intrinsic::Copy,
            Intrinsic::SizeOf,
            Intrinsic::VolatileLoad,
            Intrinsic::Atomic(AtomicOp::Load, AtomicOrders::Generic),
            Intrinsic::Simd(SimdOp::Add),
        ] {
            assert_eq!(lowered(kept), None, "{kept}");
        }
    }

    #[test]
    fn exactly_the_lowered_intrinsics_are_lowered() {
        let count = (0..Intrinsic::COUNT as u16)
            .filter_map(Intrinsic::from_index)
            .filter(|&it| lowered(it).is_some())
            .count();
        assert_eq!(count, 29);
    }
}
