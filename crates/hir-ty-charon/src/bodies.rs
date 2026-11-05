//! Bodies: blaze's runtime MIR as Charon's ULLBC, in the shapes charon-driver builds from
//! rustc's runtime MIR.
//!
//! Blocks are translated in the order charon-driver takes them: breadth first from the start
//! block, each id handed out on first reference. A statement `t = AddWithOverflow(a, b)` is, as
//! in the driver, a call of `core::intrinsics::add_with_overflow` that ends its block; the
//! `Assert` on `t.1` that follows is what Charon's `reconstruct_fallible_operations` folds back
//! into `a panic.+ b`.

use std::{alloc::Global, collections::VecDeque};

use blaze_hash::HashMap;
use charon_lib::ullbc_ast as c;
use hir_def::{FunctionId, VariantId, expr_store::ExpressionStore};
use hir_ty::{
    db::HirDatabase as _,
    mir as m,
    next_solver::{ConstKind, Ty},
};
use rustc_type_ir::inherent::{IntoKind as _, SliceLike as _};

use crate::{
    Unsupported,
    items::{Ctx, Overflowing},
    refuse,
    types::{self, Regions},
    why,
};

/// The body of `func`, `item` its path for errors. Every refusal in it is collected.
pub(crate) fn body<'db>(
    ctx: &mut Ctx<'db>,
    func: FunctionId,
    item: &str,
) -> Result<c::ExprBody, Vec<Unsupported>> {
    let mir = match ctx.db.runtime_mir_body(func.into()) {
        Ok(mir) => mir,
        // alloc: an error
        Err(err) => return Err(vec![refuse(item, format!("the body ({err:?})"), why::MIR)]),
    };
    if !mir.closures.is_empty() {
        return Err(vec![refuse(item, "a closure", why::CONTROL)]); // alloc: an error
    }
    if mir.coroutine.is_some() {
        return Err(vec![refuse(item, "an `async` block", why::CONTROL)]); // alloc: an error
    }
    let mut cx = BodyCx {
        ctx,
        item,
        mir,
        local_ids: HashMap::default(), // alloc: a body's locals
        locals: c::Locals { arg_count: mir.param_locals.len(), locals: c::IndexVec::new() },
        regions: 0,
        blocks: c::IndexMap::new(),
        block_ids: HashMap::default(), // alloc: a body's blocks
        queue: VecDeque::new(),
        errors: Vec::new(), // alloc: errors, once per body
    };
    cx.declare_locals();
    let start = cx.block(mir.start_block);
    debug_assert_eq!(start, c::BlockId::ZERO);
    while let Some(bb) = cx.queue.pop_front() {
        cx.translate_block(bb);
    }
    if !cx.errors.is_empty() {
        return Err(cx.errors);
    }
    Ok(c::ExprBody {
        span: c::Span::dummy(),
        bound_body_regions: cx.regions,
        locals: cx.locals,
        body: cx.blocks.make_contiguous(),
        comments: Vec::new(),
    })
}

struct BodyCx<'a, 'db> {
    ctx: &'a mut Ctx<'db>,
    item: &'a str,
    mir: &'db m::MirBody<'db>,
    local_ids: HashMap<m::LocalId, c::LocalId, Global>,
    locals: c::Locals,
    /// The body regions handed out: Charon's `bound_body_regions`.
    regions: usize,
    blocks: c::IndexMap<c::BlockId, c::BlockData>,
    block_ids: HashMap<m::BasicBlockId, c::BlockId, Global>,
    queue: VecDeque<m::BasicBlockId>,
    errors: Vec<Unsupported>,
}

/// A block being filled: its id, kind and statements so far.
struct Open {
    id: c::BlockId,
    kind: c::UnwindKind,
    statements: Vec<c::Statement>,
}

impl<'a, 'db> BodyCx<'a, 'db> {
    fn refuse(&self, construct: impl Into<String>, why: &'static str) -> Unsupported {
        refuse(self.item, construct, why)
    }

    fn ty(&mut self, ty: Ty<'db>) -> Result<c::Ty, Unsupported> {
        types::ty(self.ctx, self.item, ty, &mut Regions::Body(&mut self.regions))
    }

    /// Charon's locals: the return place, the parameters in order, then the rest in blaze's
    /// order, each named by the binding it holds.
    fn declare_locals(&mut self) {
        let db = self.ctx.db;
        let store = ExpressionStore::of(db, self.mir.owner.expression_store_owner(db));
        let mut names: HashMap<m::LocalId, &str, Global> = HashMap::default(); // alloc: a body's names
        for (binding, &local) in self.mir.binding_locals.iter() {
            names.insert(local, store[binding].name.as_str());
        }
        let ret = m::return_slot();
        let mut order = Vec::with_capacity(self.mir.locals.len()); // alloc: a body's locals
        order.push(ret);
        order.extend(self.mir.param_locals.iter().copied());
        for (local, _) in self.mir.locals.iter() {
            if local != ret && !self.mir.param_locals.contains(&local) {
                order.push(local);
            }
        }
        for local in order {
            let ty = match self.ty(self.mir.locals[local].ty.as_ref()) {
                Ok(ty) => ty,
                Err(err) => {
                    self.errors.push(err);
                    c::TyKind::Never.into_ty()
                }
            };
            let name = names.get(&local).map(|it| (*it).to_owned()); // alloc: a local's name
            let id = self.locals.locals.push_with(|index| c::Local {
                index,
                name,
                span: c::Span::dummy(),
                ty,
                drop_flag_for: None,
            });
            self.local_ids.insert(local, id);
        }
    }

    /// blaze's MIR reads a discriminant into an `i128` temporary, rustc's into one of the enum's
    /// discriminant type; Charon (`reconstruct_matches`) matches the switch's values against the
    /// variants' discriminants by type too. So the temporary `dest` takes the type of `of`'s
    /// discriminant, before any switch on it is translated (the switch ends the same block).
    fn retype_discriminant(
        &mut self,
        dest: &m::StoredPlace,
        of: &c::Place,
    ) -> Result<(), Unsupported> {
        if !dest.projection.as_slice().is_empty() {
            return Err(self.refuse("a discriminant read into a projection", why::OPERATIONS));
        }
        let enum_ty = of.ty();
        let Some(adt) = enum_ty.as_adt().and_then(|tref| self.ctx.adt_of(tref.id)) else {
            // alloc: an error
            return Err(self
                .refuse(format!("the discriminant of a value of type `{enum_ty:?}`"), why::TYPES));
        };
        let int = types::enum_discriminant_ty(self.ctx, adt);
        let local = self.local_ids[&dest.local];
        self.locals.locals[local].ty = c::TyKind::Scalar(c::ScalarTy::Integer(int)).into_ty();
        Ok(())
    }

    /// The id of the block `bb`, queued for translation on first reference.
    fn block(&mut self, bb: m::BasicBlockId) -> c::BlockId {
        if let Some(&id) = self.block_ids.get(&bb) {
            return id;
        }
        let id = self.blocks.reserve_slot();
        self.block_ids.insert(bb, id);
        self.queue.push_back(bb);
        id
    }

    /// A new block of `kind` holding only `terminator`.
    fn push_block(&mut self, kind: c::UnwindKind, terminator: c::TerminatorKind) -> c::BlockId {
        self.blocks.push(c::Terminator::new(c::Span::dummy(), terminator).into_block(kind))
    }

    /// Where a block of `kind` unwinds to: `cleanup`, or a new block that resumes unwinding
    /// (from a regular block) or aborts (from a cleanup block), as the driver makes them.
    fn unwind(&mut self, cleanup: Option<m::BasicBlockId>, kind: c::UnwindKind) -> c::BlockId {
        match cleanup {
            Some(bb) => self.block(bb),
            None => {
                let terminator = match kind {
                    c::UnwindKind::Regular => c::TerminatorKind::UnwindResume,
                    c::UnwindKind::Cleanup | c::UnwindKind::Terminate => {
                        c::TerminatorKind::UnwindTerminate
                    }
                };
                self.push_block(kind.further_unwind_kind(), terminator)
            }
        }
    }

    fn close(&mut self, open: Open, terminator: c::TerminatorKind) {
        let block = c::BlockData {
            statements: open.statements,
            terminator: c::Terminator::new(c::Span::dummy(), terminator),
            kind: open.kind,
        };
        self.blocks.set_slot(open.id, block);
    }

    fn translate_block(&mut self, bb: m::BasicBlockId) {
        let mir = self.mir;
        let data = &mir.basic_blocks[bb];
        let kind = if data.is_cleanup { c::UnwindKind::Cleanup } else { c::UnwindKind::Regular };
        let mut open = Open { id: self.block_ids[&bb], kind, statements: Vec::new() }; // alloc: a block's statements
        for statement in &data.statements {
            match self.statement(&statement.kind, open) {
                Ok(next) => open = next,
                Err((next, err)) => {
                    self.errors.push(err);
                    open = next;
                }
            }
        }
        let Some(terminator) = &data.terminator else {
            self.errors.push(self.refuse("a block without a terminator", why::PHASE));
            return;
        };
        match self.terminator(&terminator.kind, open.kind) {
            Ok(terminator) => self.close(open, terminator),
            Err(err) => self.errors.push(err),
        }
    }

    /// One statement into `open`; an overflowing operation closes it with a call and opens its
    /// target.
    fn statement(
        &mut self,
        statement: &m::StatementKind,
        mut open: Open,
    ) -> Result<Open, (Open, Unsupported)> {
        let span = c::Span::dummy();
        let kind = match statement {
            m::StatementKind::Assign(place, m::Rvalue::BinaryOp(op, left, right))
                if matches!(
                    op,
                    m::BinOp::AddWithOverflow
                        | m::BinOp::SubWithOverflow
                        | m::BinOp::MulWithOverflow
                ) =>
            {
                let which = match op {
                    m::BinOp::AddWithOverflow => Overflowing::Add,
                    m::BinOp::SubWithOverflow => Overflowing::Sub,
                    _ => Overflowing::Mul,
                };
                // The driver's order: the destination, then the operands.
                let parts = (|| {
                    let dest = self.place(place)?;
                    let left = self.operand(left)?;
                    let right = self.operand(right)?;
                    Ok((dest, left, right))
                })();
                let (dest, left, right) = match parts {
                    Ok(it) => it,
                    Err(err) => return Err((open, err)),
                };
                let id = self.ctx.overflowing(which);
                // alloc: a Charon type's arguments
                let generics = c::GenericArgs::new_types([left.ty().clone()].into_iter().collect()); // clone: a hash-consed handle
                let call = c::Call {
                    func: c::FnOperand::Regular(c::FnPtr::new(c::FnPtrKind::Fun(id), generics)),
                    args: vec![left, right], // alloc: a call's arguments
                    safety: c::CallSafety::Inherit,
                };
                let target = self.blocks.reserve_slot();
                let on_unwind = self.push_block(
                    open.kind.further_unwind_kind(),
                    c::TerminatorKind::UndefinedBehavior,
                );
                let kind = open.kind;
                self.close(open, c::TerminatorKind::Call { call, dest, target, on_unwind });
                return Ok(Open { id: target, kind, statements: Vec::new() }); // alloc: a block's statements
            }
            m::StatementKind::Assign(place, rvalue) => {
                let assigned = (|| {
                    let rvalue = self.rvalue(rvalue)?;
                    if let c::Rvalue::Discriminant(of) = &rvalue {
                        self.retype_discriminant(place, of)?;
                    }
                    Ok(c::StatementKind::Assign(self.place(place)?, rvalue))
                })();
                match assigned {
                    Ok(it) => it,
                    Err(err) => return Err((open, err)),
                }
            }
            // A read for the borrow checker only; runtime MIR keeps it, rustc's drops it.
            m::StatementKind::FakeRead(_) | m::StatementKind::Nop => return Ok(open),
            m::StatementKind::StorageLive(local) => {
                c::StatementKind::StorageLive(self.local_ids[local])
            }
            m::StatementKind::StorageDead(local) => {
                c::StatementKind::StorageDead(self.local_ids[local])
            }
            m::StatementKind::Deinit(_) => {
                let err = self.refuse("a `Deinit` statement", why::PHASE);
                return Err((open, err));
            }
            m::StatementKind::InlineAsm(_) => {
                let err = self.refuse("inline assembly", why::UNSAFE);
                return Err((open, err));
            }
        };
        open.statements.push(c::Statement::new(span, kind));
        Ok(open)
    }

    fn terminator(
        &mut self,
        terminator: &m::TerminatorKind,
        kind: c::UnwindKind,
    ) -> Result<c::TerminatorKind, Unsupported> {
        Ok(match terminator {
            m::TerminatorKind::Goto { target }
            | m::TerminatorKind::FalseEdge { real_target: target, .. }
            | m::TerminatorKind::FalseUnwind { real_target: target, .. } => {
                c::TerminatorKind::Goto { target: self.block(*target) }
            }
            m::TerminatorKind::SwitchInt { discr, targets } => self.switch(discr, targets)?,
            m::TerminatorKind::UnwindResume => c::TerminatorKind::UnwindResume,
            m::TerminatorKind::Abort => c::TerminatorKind::UnwindTerminate,
            m::TerminatorKind::Return => c::TerminatorKind::Return,
            m::TerminatorKind::Unreachable => c::TerminatorKind::UndefinedBehavior,
            m::TerminatorKind::Drop { place, target, unwind: _ } => {
                // The whole local without drop glue means no part of it has any.
                let ty = self.mir.locals[place.local].ty.as_ref();
                if !types::trivially_droppable(self.ctx, ty) {
                    // alloc: an error
                    return Err(
                        self.refuse(format!("a drop of a value of type `{ty:?}`"), why::DROPS)
                    );
                }
                c::TerminatorKind::Goto { target: self.block(*target) }
            }
            m::TerminatorKind::DropAndReplace { .. } => {
                return Err(self.refuse("a `DropAndReplace` terminator", why::PHASE));
            }
            m::TerminatorKind::Call {
                func, args, destination, target, cleanup, intrinsic, ..
            } => {
                if intrinsic.is_some() {
                    return Err(self.refuse("a call of an intrinsic", why::OPERATIONS));
                }
                // The driver's order: the destination, the unwind edge, the callee, the
                // arguments, the target.
                let dest = self.place(destination)?;
                let on_unwind = self.unwind(*cleanup, kind);
                let func = self.callee(func)?;
                let mut operands = Vec::with_capacity(args.len()); // alloc: a call's arguments
                for arg in args.iter() {
                    operands.push(self.operand(arg)?);
                }
                let target = match target {
                    Some(bb) => self.block(*bb),
                    None => self.push_block(kind, c::TerminatorKind::UndefinedBehavior),
                };
                let call = c::Call { func, args: operands, safety: c::CallSafety::Inherit };
                c::TerminatorKind::Call { call, dest, target, on_unwind }
            }
            m::TerminatorKind::TailCall { .. } => {
                return Err(self.refuse("a tail call (`become`)", why::CONTROL));
            }
            m::TerminatorKind::Assert { cond, expected, msg, target, cleanup } => {
                let on_unwind = self.unwind(*cleanup, kind);
                let check_kind = self.assert_kind(msg)?;
                let cond = self.operand(cond)?;
                let assert = c::Assert { cond, expected: *expected, check_kind: Some(check_kind) };
                c::TerminatorKind::Assert { assert, target: self.block(*target), on_unwind }
            }
            m::TerminatorKind::Yield { .. } | m::TerminatorKind::CoroutineDrop => {
                return Err(self.refuse("a coroutine", why::CONTROL));
            }
            m::TerminatorKind::ComptimeFor { .. } | m::TerminatorKind::ComptimeNext { .. } => {
                return Err(self.refuse("a `for const` loop", why::CONTROL));
            }
        })
    }

    /// A `SwitchInt` as the driver makes it: one branch per distinct target, the values typed
    /// as the scrutinee, and the otherwise target as the fallback (registered first on `bool`).
    fn switch(
        &mut self,
        discr: &m::Operand,
        targets: &m::SwitchTargets,
    ) -> Result<c::TerminatorKind, Unsupported> {
        let scrutinee = self.operand(discr)?;
        let scrutinee_ty = scrutinee.ty().clone(); // clone: a hash-consed handle
        let c::TyKind::Scalar(scalar) = scrutinee_ty.kind() else {
            // alloc: an error
            return Err(self.refuse(
                format!("a switch on a value of type `{scrutinee_ty:?}`"),
                why::OPERATIONS,
            ));
        };
        let scalar = *scalar;
        let mut branches: c::IndexVec<c::BranchId, c::BlockId> = c::IndexVec::new(); // alloc: a switch's branches
        let mut branch_of = |this: &mut Self, bb: m::BasicBlockId| -> c::BranchId {
            let block = this.block(bb);
            match branches.iter().position(|it| *it == block) {
                Some(index) => c::BranchId::from_usize(index),
                None => branches.push(block),
            }
        };
        if matches!(scalar, c::ScalarTy::Bool) {
            branch_of(self, targets.otherwise());
        }
        let mut values = Vec::new(); // alloc: a switch's values
        for (value, bb) in targets.iter() {
            let branch = branch_of(self, bb);
            let Some(kind) = c::ConstantExprKind::from_bits(&scalar, value) else {
                // alloc: an error
                return Err(self
                    .refuse(format!("the switch value {value} of `{scalar:?}`"), why::OPERATIONS));
            };
            // clone: a hash-consed handle
            values.push((c::ConstantExpr::new(kind, scrutinee_ty.clone()), branch));
        }
        let fallback = Some(branch_of(self, targets.otherwise()));
        Ok(c::TerminatorKind::Switch {
            data: c::SwitchData {
                scrutinee: c::SwitchScrutinee::Value(scrutinee),
                branches: values,
                fallback,
            },
            branches,
        })
    }

    /// A direct call's callee: a non-generic function, with a fresh body region for each of its
    /// late-bound lifetimes.
    fn callee(&mut self, func: &m::Operand) -> Result<c::FnOperand, Unsupported> {
        let Some((callee, args)) = func.fn_def() else {
            return Err(self.refuse("a call of a function pointer or closure", why::CONTROL));
        };
        let hir_def::CallableDefId::FunctionId(callee) = callee else {
            return Err(self.refuse("a call of a struct or variant constructor", why::OPERATIONS));
        };
        if !args.is_empty() {
            return Err(self.refuse("a call with generic arguments", why::GENERICS));
        }
        let id = self.ctx.callee(self.item, callee)?;
        let mut regions = c::IndexVec::new(); // alloc: a call's region arguments
        for _ in 0..self.ctx.region_count(id) {
            regions.push(c::Region::Body(c::RegionId::from_usize(self.regions)));
            self.regions += 1;
        }
        let generics = c::GenericArgs::new_lifetimes(regions);
        Ok(c::FnOperand::Regular(c::FnPtr::new(c::FnPtrKind::Fun(id), generics)))
    }

    fn assert_kind(&mut self, msg: &m::AssertKind) -> Result<c::BuiltinAssertKind, Unsupported> {
        Ok(match msg {
            m::AssertKind::BoundsCheck { len, index } => c::BuiltinAssertKind::BoundsCheck {
                len: self.operand(len)?,
                index: self.operand(index)?,
            },
            m::AssertKind::Overflow(op, left, right) => c::BuiltinAssertKind::Overflow(
                self.bin_op(op)?,
                self.operand(left)?,
                self.operand(right)?,
            ),
            m::AssertKind::OverflowNeg(it) => c::BuiltinAssertKind::OverflowNeg(self.operand(it)?),
            m::AssertKind::DivisionByZero(it) => {
                c::BuiltinAssertKind::DivisionByZero(self.operand(it)?)
            }
            m::AssertKind::RemainderByZero(it) => {
                c::BuiltinAssertKind::RemainderByZero(self.operand(it)?)
            }
            m::AssertKind::MisalignedPointerDereference { .. }
            | m::AssertKind::NullPointerDereference
            | m::AssertKind::NullReferenceConstructed => {
                return Err(self.refuse("a check of a raw pointer", why::UNSAFE));
            }
            m::AssertKind::InvalidEnumConstruction(it) => {
                c::BuiltinAssertKind::InvalidEnumConstruction(self.operand(it)?)
            }
        })
    }

    /// A binary operation as the driver maps rustc's runtime MIR: the arithmetic wraps (its
    /// checks are the `Assert`s before it), and division and remainder are UB on zero.
    fn bin_op(&self, op: &m::BinOp) -> Result<c::BinOp, Unsupported> {
        use c::OverflowMode::{UB, Wrap};
        Ok(match op {
            m::BinOp::Add => c::BinOp::Add(Wrap),
            m::BinOp::Sub => c::BinOp::Sub(Wrap),
            m::BinOp::Mul => c::BinOp::Mul(Wrap),
            m::BinOp::Div => c::BinOp::Div(UB),
            m::BinOp::Rem => c::BinOp::Rem(UB),
            m::BinOp::Shl => c::BinOp::Shl(Wrap),
            m::BinOp::Shr => c::BinOp::Shr(Wrap),
            m::BinOp::BitXor => c::BinOp::BitXor,
            m::BinOp::BitAnd => c::BinOp::BitAnd,
            m::BinOp::BitOr => c::BinOp::BitOr,
            m::BinOp::Eq => c::BinOp::Eq,
            m::BinOp::Lt => c::BinOp::Lt,
            m::BinOp::Le => c::BinOp::Le,
            m::BinOp::Ne => c::BinOp::Ne,
            m::BinOp::Ge => c::BinOp::Ge,
            m::BinOp::Gt => c::BinOp::Gt,
            m::BinOp::Offset => return Err(self.refuse("pointer `offset`", why::UNSAFE)),
            m::BinOp::AddWithOverflow | m::BinOp::SubWithOverflow | m::BinOp::MulWithOverflow => {
                // alloc: an error
                return Err(self.refuse(format!("`{op}` outside an assignment"), why::PHASE));
            }
        })
    }

    fn rvalue(&mut self, rvalue: &m::Rvalue) -> Result<c::Rvalue, Unsupported> {
        Ok(match rvalue {
            m::Rvalue::Use(op) => c::Rvalue::Use(self.operand(op)?, c::WithRetag::Yes),
            m::Rvalue::CopyForDeref(place) => {
                c::Rvalue::Use(c::Operand::Copy(self.place(place)?), c::WithRetag::No)
            }
            m::Rvalue::Ref(kind, place) => c::Rvalue::Ref {
                place: self.place(place)?,
                kind: match kind {
                    m::BorrowKind::Shared => c::BorrowKind::Shared,
                    m::BorrowKind::Shallow => c::BorrowKind::Shallow,
                    m::BorrowKind::Mut { kind: m::MutBorrowKind::Default } => c::BorrowKind::Mut,
                    m::BorrowKind::Mut { kind: m::MutBorrowKind::TwoPhasedBorrow } => {
                        c::BorrowKind::TwoPhaseMut
                    }
                    m::BorrowKind::Mut { kind: m::MutBorrowKind::ClosureCapture } => {
                        c::BorrowKind::UniqueImmutable
                    }
                },
                // What the driver leaves for `insert_ptr_metadata` to fill.
                ptr_metadata: c::Operand::Const(c::ConstantExpr::new(
                    c::ConstantExprKind::Opaque("Missing metadata".to_owned()), // alloc: a marker
                    c::Ty::mk_unit(),
                )),
            },
            m::Rvalue::BinaryOp(op, left, right) => {
                c::Rvalue::BinaryOp(self.bin_op(op)?, self.operand(left)?, self.operand(right)?)
            }
            m::Rvalue::UnaryOp(op, it) => {
                let op = match op {
                    m::UnOp::Not => c::UnOp::Not,
                    m::UnOp::Neg => c::UnOp::Neg(c::OverflowMode::Wrap),
                };
                c::Rvalue::UnaryOp(op, self.operand(it)?)
            }
            m::Rvalue::Cast(m::CastKind::IntToInt, it, ty) => {
                let target = self.ty(ty.as_ref())?;
                let c::TyKind::Scalar(scalar) = target.kind() else {
                    // alloc: an error
                    return Err(self.refuse(format!("a cast to `{target:?}`"), why::OPERATIONS));
                };
                c::Rvalue::UnaryOp(c::UnOp::Cast(c::CastKind::Scalar(*scalar)), self.operand(it)?)
            }
            m::Rvalue::Cast(kind, ..) => {
                // alloc: an error
                return Err(self.refuse(format!("a `{kind:?}` cast"), why::OPERATIONS));
            }
            m::Rvalue::Discriminant(place) => c::Rvalue::Discriminant(self.place(place)?),
            m::Rvalue::Aggregate(kind, operands) => {
                let kind = match kind {
                    m::AggregateKind::Tuple(ty) => {
                        let ty = self.ty(ty.as_ref())?;
                        let Some(tref) = ty.as_adt() else {
                            // alloc: an error
                            return Err(
                                self.refuse(format!("a tuple of type `{ty:?}`"), why::PHASE)
                            );
                        };
                        c::AggregateKind::Adt(tref.clone(), None, None) // clone: a type reference
                    }
                    m::AggregateKind::Adt(variant, args) => {
                        if !args.as_ref().is_empty() {
                            return Err(
                                self.refuse("a generic struct or enum value", why::GENERICS)
                            );
                        }
                        self.adt_aggregate(*variant)?
                    }
                    m::AggregateKind::Array(_) => return Err(self.refuse("an array", why::TYPES)),
                    m::AggregateKind::Union(..) => return Err(self.refuse("a union", why::TYPES)),
                    m::AggregateKind::Closure(_) => {
                        return Err(self.refuse("a closure", why::CONTROL));
                    }
                };
                let mut out = Vec::with_capacity(operands.len()); // alloc: an aggregate's fields
                for operand in operands.iter() {
                    out.push(self.operand(operand)?);
                }
                c::Rvalue::Aggregate(kind, out)
            }
            m::Rvalue::Repeat(..) => return Err(self.refuse("an array `[x; N]`", why::TYPES)),
            m::Rvalue::Len(_) => return Err(self.refuse("a slice or array length", why::TYPES)),
            m::Rvalue::CheckedBinaryOp(..) => {
                return Err(self.refuse("a `CheckedBinaryOp`", why::PHASE));
            }
            m::Rvalue::ThreadLocalRef(never)
            | m::Rvalue::AddressOf(never)
            | m::Rvalue::NullaryOp(never) => match *never {},
        })
    }

    fn adt_aggregate(&mut self, variant: VariantId) -> Result<c::AggregateKind, Unsupported> {
        let db = self.ctx.db;
        let (adt, index) = match variant {
            VariantId::StructId(it) => (hir_def::AdtId::StructId(it), None),
            VariantId::EnumVariantId(it) => (
                hir_def::AdtId::EnumId(it.loc(db).parent),
                Some(c::VariantId::from_usize(it.index(db))),
            ),
            VariantId::UnionId(_) => return Err(self.refuse("a union", why::TYPES)),
        };
        let Some(&id) = self.ctx.adts.get(&adt) else {
            let name = crate::names::adt_path(db, adt).display;
            // alloc: an error
            return Err(
                self.refuse(format!("a value of `{name}`, which is not translated"), why::TYPES)
            );
        };
        Ok(c::AggregateKind::Adt(
            c::TypeDeclRef::new(id, c::GenericArgs::empty(), None),
            index,
            None,
        ))
    }

    fn operand(&mut self, operand: &m::Operand) -> Result<c::Operand, Unsupported> {
        Ok(match &operand.kind {
            m::OperandKind::Copy(place) => c::Operand::Copy(self.place(place)?),
            m::OperandKind::Move(place) => c::Operand::Move(self.place(place)?),
            m::OperandKind::Constant { konst, ty } => {
                let ty = ty.as_ref();
                if operand.fn_def().is_some() {
                    return Err(self.refuse("a function used as a value", why::CONTROL));
                }
                let cty = self.ty(ty)?;
                if cty == c::Ty::mk_unit() {
                    return Ok(c::Operand::Const(c::ConstantExpr::mk_unit()));
                }
                let bits = match konst.as_ref().kind() {
                    ConstKind::Value(value) => {
                        value.try_to_leaf().map(|leaf| leaf.to_bits_unchecked())
                    }
                    _ => None,
                };
                let Some(bits) = bits else {
                    // alloc: an error
                    return Err(
                        self.refuse(format!("a constant of type `{ty:?}`"), why::OPERATIONS)
                    );
                };
                c::Operand::Const(self.scalar_const(cty, bits)?)
            }
            m::OperandKind::Allocation { allocation } => {
                if operand.fn_def().is_some() {
                    return Err(self.refuse("a function used as a value", why::CONTROL));
                }
                let allocation = allocation.as_ref();
                let cty = self.ty(allocation.ty)?;
                if cty == c::Ty::mk_unit() {
                    return Ok(c::Operand::Const(c::ConstantExpr::mk_unit()));
                }
                let memory: &[u8] = &allocation.memory;
                if memory.len() > 16 || !matches!(cty.kind(), c::TyKind::Scalar(_)) {
                    // alloc: an error
                    return Err(
                        self.refuse(format!("a constant of type `{cty:?}`"), why::OPERATIONS)
                    );
                }
                let mut bytes = [0u8; 16];
                if self.ctx.little_endian {
                    bytes[..memory.len()].copy_from_slice(memory);
                } else {
                    for (index, byte) in memory.iter().rev().enumerate() {
                        bytes[index] = *byte;
                    }
                }
                c::Operand::Const(self.scalar_const(cty, u128::from_le_bytes(bytes))?)
            }
            m::OperandKind::Static(_) => return Err(self.refuse("a static", why::UNSAFE)),
        })
    }

    fn scalar_const(&self, ty: c::Ty, bits: u128) -> Result<c::ConstantExpr, Unsupported> {
        let kind = match ty.kind() {
            c::TyKind::Scalar(scalar) => c::ConstantExprKind::from_bits(scalar, bits),
            _ => None,
        };
        match kind {
            Some(kind) => Ok(c::ConstantExpr::new(kind, ty)),
            // alloc: an error
            None => {
                Err(self
                    .refuse(format!("the constant {bits:#x} of type `{ty:?}`"), why::OPERATIONS))
            }
        }
    }

    /// A place, its type at each projection as Charon computes it: a field of a tuple is its
    /// type argument, a field of a struct or enum its declaration's, and a downcast is folded
    /// into the field after it (`Field(Some(variant), field)`).
    fn place(&mut self, place: &m::StoredPlace) -> Result<c::Place, Unsupported> {
        let local = self.local_ids[&place.local];
        let mut out = c::Place::new(local, self.locals.locals[local].ty.clone()); // clone: a hash-consed handle
        let elems = place.projection.as_slice();
        let mut index = 0;
        while index < elems.len() {
            let (elem, field) = match elems[index] {
                m::ProjectionElem::Deref => (c::ProjectionElem::Deref, None),
                m::ProjectionElem::Field(field) => (
                    c::ProjectionElem::Field(None, c::FieldId::from_usize(field.0 as usize)),
                    Some(field.0 as usize),
                ),
                m::ProjectionElem::Downcast(VariantId::EnumVariantId(variant)) => {
                    let Some(m::ProjectionElem::Field(field)) = elems.get(index + 1) else {
                        return Err(
                            self.refuse("a downcast not followed by a field", why::OPERATIONS)
                        );
                    };
                    index += 1;
                    let variant = c::VariantId::from_usize(variant.index(self.ctx.db));
                    (
                        c::ProjectionElem::Field(
                            Some(variant),
                            c::FieldId::from_usize(field.0 as usize),
                        ),
                        None,
                    )
                }
                m::ProjectionElem::Downcast(_) => {
                    return Err(self.refuse("a downcast to a non-enum variant", why::PHASE));
                }
                m::ProjectionElem::Index(_)
                | m::ProjectionElem::ConstantIndex { .. }
                | m::ProjectionElem::Subslice { .. } => {
                    return Err(self.refuse("an index into a slice or array", why::TYPES));
                }
                m::ProjectionElem::ReflectedField(_) => {
                    return Err(self.refuse("a reflected field (`for const`)", why::GENERICS));
                }
            };
            index += 1;
            let tuple_field = match (out.ty().as_adt(), field) {
                (Some(tref), Some(field)) if tref.builtin == Some(c::BuiltinAdt::Tuple) => {
                    tref.generics.types.get(c::TypeVarId::from_usize(field)).cloned() // clone: a hash-consed handle
                }
                _ => None,
            };
            let ty = match tuple_field {
                Some(ty) => Some(ty),
                None => elem.project_type(&self.ctx.translated, out.ty()),
            };
            let Some(ty) = ty else {
                // alloc: an error
                return Err(self
                    .refuse(format!("the projection `{elem:?}` of `{:?}`", out.ty()), why::PHASE));
            };
            out = out.project(elem, ty);
        }
        Ok(out)
    }
}
