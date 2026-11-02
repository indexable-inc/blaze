//! rustc's checks as rustc's MIR building makes them (`build/expr/as_rvalue.rs`,
//! `as_place.rs`): an `Assert` terminator before each operation that can panic, with an unwind
//! edge of its own into the scopes' unwind drop tree, and the operation after it one that does
//! not panic.
//! - `a + b`, `a - b`, `a * b`: `t = AddWithOverflow(a, b); assert(!t.1); x = t.0`. These
//!   are optional overflow checks, which the code generator skips where overflow is not checked.
//! - `a / b`, `a % b`: `assert(b != 0)`, and for signed integers `assert(!(b == -1 && a == MIN))`.
//! - `a << b`, `a >> b`: `assert(b as unsigned < BITS)`.
//! - `-a` of a signed integer: `assert(a != MIN)`.
//! - `base[i]`: `len = Len(base); assert(i < len)`, at the index expression.
//!
//! Lowering keeps the operation in one statement (`CheckedBinaryOp`) and records where it
//! unwinds (`MirBody::panic_unwinds`); after this pass no statement panics and every binary
//! operation is a `BinaryOp`.

use arrayvec::ArrayVec;
use blaze_mem::Frame;
use rustc_type_ir::inherent::{IntoKind, Ty as _};

use crate::mir::{
    AssertKind, BasicBlock, BasicBlockId, BinOp, CastKind, FieldIndex, Local, LocalId, MirBody,
    MirSpan, Operand, OperandKind, Place, Projection, ProjectionElem, Rvalue, Statement,
    StatementKind, StoredPlace, Terminator, TerminatorKind, UnOp,
    framed::{self, block_id, local_index},
};
use crate::next_solver::{ConstExt, Ty, TyKind};

use super::Ctx;

/// A check: what computes the condition, the condition, what it must be, and the panic.
struct Check {
    /// At most the length, the comparison and the bound.
    setup: ArrayVec<Statement, 3>,
    cond: Operand,
    expected: bool,
    msg: AssertKind,
    span: MirSpan,
}

/// The checks one statement or terminator makes, reused across them.
type Checks<'f> = Vec<Check, &'f Frame>;

/// Where each local starts to live, by its index: see [`index_spans`].
type IndexSpans<'f> = Vec<Option<MirSpan>, &'f Frame>;

pub(super) fn add_asserts<'db>(ctx: &Ctx<'_, 'db>, body: &mut MirBody<'db>, frame: &Frame) {
    // rustc builds overflow checks into the MIR of a crate that checks overflow, of a function
    // that inherits the check from the crate it is compiled in, and of every constant and
    // static; the standard library is built without.
    let constant = !matches!(
        body.owner,
        crate::InferBodyId::DefWithBodyId(hir_def::DefWithBodyId::FunctionId(_))
    );
    let checks_overflow = constant || body.inherits_overflow_checks || {
        let flag = cfg::CfgAtom::Flag(intern::sym::overflow_checks);
        !ctx.krate.data(ctx.db).origin.is_lang() && ctx.krate.cfg_options(ctx.db).check_atom(&flag)
    };
    let mut unwinds = framed::map_in(frame);
    unwinds.reserve(body.panic_unwinds.len());
    unwinds
        .extend(body.panic_unwinds.drain(..).map(|(block, at, unwind)| ((block, at), unwind)));
    let index_spans = index_spans(body, frame);
    let mut pending: Vec<Statement, &Frame> = Vec::new_in(frame);
    let mut checks: Checks<'_> = Vec::new_in(frame);
    // The blocks `assert` adds go after these, and are already checked.
    let blocks = body.basic_blocks.len();
    for index in 0..blocks {
        let id = block_id(index);
        pending.clear();
        pending.extend(body.basic_blocks[id].statements.drain(..));
        let terminator = body.basic_blocks[id].terminator.take();
        let is_cleanup = body.basic_blocks[id].is_cleanup;
        let count = pending.len();
        let mut current = id;
        for (at, statement) in pending.drain(..).enumerate() {
            let unwind = unwinds.get(&(id, at)).copied();
            let rewritten =
                rewrite(ctx, body, statement, &index_spans, checks_overflow, &mut checks);
            for check in checks.drain(..) {
                current = assert(body, current, check, unwind, is_cleanup, count - at);
            }
            body.basic_blocks[current].statements.push(rewritten);
        }
        if let Some(terminator) = terminator {
            let unwind = terminator.kind.unwind();
            terminator.kind.for_each_operand(&mut |operand| {
                if let OperandKind::Copy(place) | OperandKind::Move(place) = &operand.kind {
                    bounds_checks(ctx, body, place, terminator.span, &index_spans, &mut checks);
                }
            });
            if let TerminatorKind::Call { destination: place, .. }
            | TerminatorKind::Drop { place, .. } = &terminator.kind
            {
                bounds_checks(ctx, body, place, terminator.span, &index_spans, &mut checks);
            }
            for check in checks.drain(..) {
                current = assert(body, current, check, unwind, is_cleanup, 0);
            }
            body.basic_blocks[current].terminator = Some(terminator);
        }
    }
}

/// Ends `block` in an `Assert` for `check`, and returns the block it goes on to, made with room
/// for the `after` statements that follow the check.
fn assert(
    body: &mut MirBody<'_>,
    block: BasicBlockId,
    check: Check,
    unwind: Option<BasicBlockId>,
    is_cleanup: bool,
    after: usize,
) -> BasicBlockId {
    body.basic_blocks[block].statements.extend(check.setup);
    let next = body.basic_blocks.alloc(BasicBlock {
        statements: Vec::with_capacity(after),
        terminator: None,
        is_cleanup,
    });
    body.basic_blocks[block].terminator = Some(Terminator {
        span: check.span,
        kind: TerminatorKind::Assert {
            cond: check.cond,
            expected: check.expected,
            msg: check.msg,
            target: next,
            cleanup: if is_cleanup { None } else { unwind },
        },
    });
    next
}

/// The statement as it is after its checks, which go to `checks` in the order rustc makes
/// them: the places its operands read, the operation, then the place it writes.
fn rewrite<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &mut MirBody<'db>,
    statement: Statement,
    index_spans: &IndexSpans<'_>,
    checks_overflow: bool,
    checks: &mut Checks<'_>,
) -> Statement {
    let span = statement.span;
    match statement.kind {
        StatementKind::Assign(place, rvalue) => {
            match &rvalue {
                Rvalue::Ref(_, read)
                | Rvalue::Len(read)
                | Rvalue::Discriminant(read)
                | Rvalue::CopyForDeref(read) => {
                    bounds_checks(ctx, body, read, span, index_spans, checks)
                }
                _ => rvalue.for_each_operand(&mut |operand| {
                    if let OperandKind::Copy(read) | OperandKind::Move(read) = &operand.kind {
                        bounds_checks(ctx, body, read, span, index_spans, checks);
                    }
                }),
            }
            let rvalue = match rvalue {
                Rvalue::CheckedBinaryOp(op, a, b) => {
                    let overflow = matches!(
                        op,
                        BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Shl | BinOp::Shr
                    );
                    match overflow && !checks_overflow {
                        true => Rvalue::BinaryOp(op, a, b),
                        false => binary_op(ctx, body, op, a, b, span, checks),
                    }
                }
                Rvalue::UnaryOp(UnOp::Neg, a) => {
                    let ty = operand_ty(ctx, body, &a);
                    if let (TyKind::Int(_), true) = (ty.kind(), checks_overflow) {
                        let min = int_const(ctx, ty, Bound::Min);
                        let is_min = eq(ctx, body, copy(&a), min, span);
                        checks.push(Check {
                            cond: move_of(is_min.0),
                            setup: setup_of([is_min.1]),
                            expected: false,
                            msg: AssertKind::OverflowNeg(copy(&a)),
                            span,
                        });
                    }
                    Rvalue::UnaryOp(UnOp::Neg, a)
                }
                rvalue => rvalue,
            };
            bounds_checks(ctx, body, &place, span, index_spans, checks);
            Statement { kind: StatementKind::Assign(place, rvalue), span }
        }
        // `let _ = items[at]` checks the index all the same.
        StatementKind::FakeRead(read) => {
            bounds_checks(ctx, body, &read, span, index_spans, checks);
            Statement { kind: StatementKind::FakeRead(read), span }
        }
        kind => Statement { kind, span },
    }
}

/// The checks of `a op b`, and the operation after them.
fn binary_op<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &mut MirBody<'db>,
    op: BinOp,
    a: Operand,
    b: Operand,
    span: MirSpan,
    checks: &mut Checks<'_>,
) -> Rvalue {
    let ty = operand_ty(ctx, body, &a);
    let signed = matches!(ty.kind(), TyKind::Int(_));
    if !matches!(ty.kind(), TyKind::Int(_) | TyKind::Uint(_)) {
        return Rvalue::BinaryOp(op, a, b);
    }
    match op {
        BinOp::Add | BinOp::Sub | BinOp::Mul => {
            let with = match op {
                BinOp::Add => BinOp::AddWithOverflow,
                BinOp::Sub => BinOp::SubWithOverflow,
                _ => BinOp::MulWithOverflow,
            };
            let pair = Ty::new_tup_from_iter(ctx.infcx.interner, [ty, ctx.bool_ty()].into_iter());
            let result = temp(body, pair);
            let setup = Statement {
                kind: StatementKind::Assign(
                    Place::from(result).store(),
                    Rvalue::BinaryOp(with, copy(&a), copy(&b)),
                ),
                span,
            };
            let field = |index| {
                Place::from(result).project(ProjectionElem::Field(FieldIndex(index))).store()
            };
            // The panic takes the operands as they are, moved out of their temporaries, where
            // rustc's checks of the other operations read them: what a future keeps of them
            // is gone once the check is done.
            checks.push(Check {
                setup: setup_of([setup]),
                cond: Operand { kind: OperandKind::Move(field(1)), span: None },
                expected: false,
                msg: AssertKind::Overflow(op, a, b),
                span,
            });
            Rvalue::Use(Operand { kind: OperandKind::Move(field(0)), span: None })
        }
        BinOp::Div | BinOp::Rem => {
            let zero = int_const(ctx, ty, Bound::Zero);
            let is_zero = eq(ctx, body, copy(&b), zero, span);
            checks.push(Check {
                setup: setup_of([is_zero.1]),
                cond: move_of(is_zero.0),
                expected: false,
                msg: match op {
                    BinOp::Div => AssertKind::DivisionByZero(copy(&a)),
                    _ => AssertKind::RemainderByZero(copy(&a)),
                },
                span,
            });
            if signed {
                let minus_one = int_const(ctx, ty, Bound::MinusOne);
                let min = int_const(ctx, ty, Bound::Min);
                let is_minus_one = eq(ctx, body, copy(&b), minus_one, span);
                let is_min = eq(ctx, body, copy(&a), min, span);
                let overflow = temp(body, ctx.bool_ty());
                let both = Statement {
                    kind: StatementKind::Assign(
                        Place::from(overflow).store(),
                        Rvalue::BinaryOp(BinOp::BitAnd, move_of(is_minus_one.0), move_of(is_min.0)),
                    ),
                    span,
                };
                checks.push(Check {
                    setup: setup_of([is_minus_one.1, is_min.1, both]),
                    cond: move_of(overflow),
                    expected: false,
                    // clone: a `BinOp` tag, kept in the panic as in the operation
                    // clone: a `BinOp` tag, kept in the panic as in the operation
                msg: AssertKind::Overflow(op.clone(), copy(&a), copy(&b)),
                    span,
                });
            }
            Rvalue::BinaryOp(op, a, b)
        }
        BinOp::Shl | BinOp::Shr => {
            let rhs_ty = operand_ty(ctx, body, &b);
            let mut setup = ArrayVec::new();
            let (unsigned, unsigned_ty) = match rhs_ty.kind() {
                TyKind::Int(int) => {
                    let uint_ty = Ty::new_uint(ctx.infcx.interner, unsigned_of(int));
                    let cast = temp(body, uint_ty);
                    setup.push(Statement {
                        kind: StatementKind::Assign(
                            Place::from(cast).store(),
                            Rvalue::Cast(CastKind::IntToInt, copy(&b), uint_ty.store()),
                        ),
                        span,
                    });
                    (move_of(cast), uint_ty)
                }
                _ => (copy(&b), rhs_ty),
            };
            let bits = ty.primitive_size(ctx.infcx.interner).bits();
            let bits = int_value(ctx, unsigned_ty, u128::from(bits));
            let inbounds = temp(body, ctx.bool_ty());
            setup.push(Statement {
                kind: StatementKind::Assign(
                    Place::from(inbounds).store(),
                    Rvalue::BinaryOp(BinOp::Lt, unsigned, bits),
                ),
                span,
            });
            checks.push(Check {
                setup,
                cond: move_of(inbounds),
                expected: true,
                // clone: a `BinOp` tag, kept in the panic as in the operation
                msg: AssertKind::Overflow(op.clone(), copy(&a), copy(&b)),
                span,
            });
            Rvalue::BinaryOp(op, a, b)
        }
        op => Rvalue::BinaryOp(op, a, b),
    }
}

/// The bounds check of each runtime index in `place`, outermost first.
fn bounds_checks<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &mut MirBody<'db>,
    place: &StoredPlace,
    span: MirSpan,
    index_spans: &IndexSpans<'_>,
    checks: &mut Checks<'_>,
) {
    let projection = place.projection.as_slice();
    for (at, elem) in projection.iter().enumerate() {
        let ProjectionElem::Index(index) = elem else { continue };
        let base = StoredPlace {
            local: place.local,
            projection: Projection::new_from_slice(&projection[..at]).store(),
        };
        let usize_ty = Ty::new_uint(ctx.infcx.interner, rustc_type_ir::UintTy::Usize);
        let lt = temp(body, ctx.bool_ty());
        let index = Operand { kind: OperandKind::Copy(Place::from(*index).store()), span: None };
        // An array's length is its constant, as rustc has it (monomorphization fills in a
        // generic one); a slice's is read from the place.
        let base_ty = ctx.normalize(base.as_ref().ty(body, &ctx.infcx, ctx.env).ty);
        let mut setup = ArrayVec::new();
        let len = match base_ty.kind() {
            TyKind::Array(_, len) => Operand {
                kind: OperandKind::Constant { konst: len.store(), ty: usize_ty.store() },
                span: None,
            },
            _ => {
                let len = temp(body, usize_ty);
                setup.push(Statement {
                    kind: StatementKind::Assign(Place::from(len).store(), Rvalue::Len(base)),
                    span,
                });
                copy_of(len)
            }
        };
        setup.push(Statement {
            kind: StatementKind::Assign(
                Place::from(lt).store(),
                // clone: the index and length the kept body reads twice (interned handles)
                Rvalue::BinaryOp(BinOp::Lt, index.clone(), len.clone()),
            ),
            span,
        });
        checks.push(Check {
            setup,
            cond: move_of(lt),
            expected: true,
            msg: AssertKind::BoundsCheck { len, index },
            // rustc reports it at the indexing expression, where the index's temporary starts.
            span: match elem {
                ProjectionElem::Index(index) => {
                    index_spans.get(local_index(*index)).copied().flatten().unwrap_or(span)
                }
                _ => span,
            },
        });
    }
}

/// Where each local starts to live: a local that holds the index of `base[index]` starts at
/// that expression.
/// The last marker of a local wins, as the map this was had it.
fn index_spans<'f>(body: &MirBody<'_>, frame: &'f Frame) -> IndexSpans<'f> {
    let mut spans = Vec::with_capacity_in(body.locals.len(), frame);
    spans.resize(body.locals.len(), None);
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            if let StatementKind::StorageLive(local) = statement.kind {
                spans[local_index(local)] = Some(statement.span);
            }
        }
    }
    spans
}

enum Bound {
    Zero,
    MinusOne,
    Min,
}

fn int_const<'db>(ctx: &Ctx<'_, 'db>, ty: Ty<'db>, bound: Bound) -> Operand {
    let bytes = ty.primitive_size(ctx.infcx.interner).bytes() as u32;
    let value = match bound {
        Bound::Zero => 0,
        Bound::MinusOne => u128::MAX,
        Bound::Min => 1u128 << (bytes * 8 - 1),
    };
    int_value(ctx, ty, value)
}

/// The integer `value` of the integer type `ty`, truncated to its width.
fn int_value<'db>(ctx: &Ctx<'_, 'db>, ty: Ty<'db>, value: u128) -> Operand {
    let bytes = ty.primitive_size(ctx.infcx.interner).bytes() as usize;
    Operand::from_bytes(value.to_le_bytes()[..bytes].into(), ty)
}

fn unsigned_of(int: rustc_type_ir::IntTy) -> rustc_type_ir::UintTy {
    use rustc_type_ir::{IntTy, UintTy};
    match int {
        IntTy::Isize => UintTy::Usize,
        IntTy::I8 => UintTy::U8,
        IntTy::I16 => UintTy::U16,
        IntTy::I32 => UintTy::U32,
        IntTy::I64 => UintTy::U64,
        IntTy::I128 => UintTy::U128,
    }
}

/// `t = a == b` into a new temporary.
fn eq<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &mut MirBody<'db>,
    a: Operand,
    b: Operand,
    span: MirSpan,
) -> (LocalId, Statement) {
    let t = temp(body, ctx.bool_ty());
    let statement = Statement {
        kind: StatementKind::Assign(Place::from(t).store(), Rvalue::BinaryOp(BinOp::Eq, a, b)),
        span,
    };
    (t, statement)
}

/// A check's setup, of at most three statements.
fn setup_of<const N: usize>(statements: [Statement; N]) -> ArrayVec<Statement, 3> {
    let mut setup = ArrayVec::new();
    for statement in statements {
        setup.push(statement);
    }
    setup
}

fn temp<'db>(body: &mut MirBody<'db>, ty: Ty<'db>) -> LocalId {
    body.locals.alloc(Local { ty: ty.store() })
}

fn move_of(local: LocalId) -> Operand {
    Operand { kind: OperandKind::Move(Place::from(local).store()), span: None }
}

fn copy_of(local: LocalId) -> Operand {
    Operand { kind: OperandKind::Copy(Place::from(local).store()), span: None }
}

/// The operand read again: an integer is `Copy`.
fn copy(operand: &Operand) -> Operand {
    match &operand.kind {
        // clone: the place the kept body reads again (interned handles)
        OperandKind::Move(place) => Operand { kind: OperandKind::Copy(place.clone()), span: None },
        // clone: the operand the kept body reads again (interned handles)
        _ => operand.clone(),
    }
}

fn operand_ty<'db>(ctx: &Ctx<'_, 'db>, body: &MirBody<'db>, operand: &Operand) -> Ty<'db> {
    ctx.normalize(ctx.operand_ty(body, operand))
}
