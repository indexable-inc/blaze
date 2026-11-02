//! Constants of the closed shape, evaluated over their HIR instead of their MIR: the shape and
//! why the two give the same value are in `docs/v2/plan/lanes/frontend/fast-const.md`.
//!
//! [`value`] is the one entry the evaluation queries call. It classifies the constant once:
//! one of the shape is compiled into a [`Program`], its tree in postorder, and run; any other
//! goes to [`by_mir`], the one path for it. Each check of the program is the assert MIR
//! building puts in a constant (`hir-ty-mir`'s `transform/asserts.rs`) and fails with the error
//! the MIR evaluator gives for it, and each value is the bytes the MIR evaluator writes.

mod ops;
#[cfg(test)]
mod tests;

use arrayvec::ArrayVec;
use base_db::Db;
use hir_def::{
    CallableDefId, ConstId, DefWithBodyId, HasModule, ItemContainerId,
    attrs::AttrFlags,
    expr_store::ExpressionStore,
    hir::{BinaryOp, Expr, ExprId, Literal, LogicOp, UnaryOp},
    resolver::ValueNs,
};
use hir_expand::Lookup;
use rustc_type_ir::{TypeVisitableExt as _, UintTy, inherent::IntoKind};

pub(crate) use self::ops::{Kind, Panic};
use crate::{
    InferBodyId, InferenceResult, MemoryMap, ParamEnvAndCrate,
    consteval::ConstEvalError,
    db::HirDatabase,
    intrinsic::{Intrinsic, intrinsic_of},
    mir::{IsSigned, MirEvalError, MirLowerError, pad16},
    next_solver::{Allocation, AllocationData, GenericArgs, Ty, TyKind, ensure_sufficient_stack},
};

/// The most nodes a constant of the shape has. Each is at most one op of its program.
const MAX_NODES: usize = 256;

/// The value of the constant `owner` for `subst`, evaluated in `trait_env` (its own
/// environment when `None`).
pub(crate) fn value<'db>(
    db: &'db Db,
    owner: InferBodyId<'db>,
    subst: GenericArgs<'db>,
    trait_env: Option<ParamEnvAndCrate<'db>>,
) -> Result<Allocation<'db>, ConstEvalError<'db>> {
    // As rustc's `eval_to_allocation` of a body whose type check is tainted by errors: no value,
    // so a type using the constant is an error, not a mismatch with whatever it would run to.
    if InferenceResult::of(db, owner).has_errors {
        return Err(ConstEvalError::MirLowerError(MirLowerError::HasErrors));
    }
    match by_hir(db, owner, trait_env) {
        Some(value) => value,
        None => by_mir(db, owner, subst, trait_env),
    }
}

/// The value of `owner` when it is of the shape, `None` when it is not. A constant of the
/// shape uses no generic parameter, so its value is the same for every `subst`.
pub fn by_hir<'db>(
    db: &'db Db,
    owner: InferBodyId<'db>,
    trait_env: Option<ParamEnvAndCrate<'db>>,
) -> Option<Result<Allocation<'db>, ConstEvalError<'db>>> {
    let env = trait_env.unwrap_or_else(|| ParamEnvAndCrate {
        param_env: db.trait_environment(owner.generic_def(db)),
        krate: owner.krate(db),
    });
    let program = ensure_sufficient_stack(|| Program::of(db, owner, env))?;
    Some(program.run(db, env))
}

/// The value of `owner` by lowering it to MIR and running that.
pub fn by_mir<'db>(
    db: &'db Db,
    owner: InferBodyId<'db>,
    subst: GenericArgs<'db>,
    trait_env: Option<ParamEnvAndCrate<'db>>,
) -> Result<Allocation<'db>, ConstEvalError<'db>> {
    let body = db.monomorphized_runtime_mir_body(
        owner,
        subst.store(),
        // The crate the constant is evaluated for, not the one it is in: the impls its
        // generic arguments select can be in crates its own doesn't depend on.
        ParamEnvAndCrate {
            param_env: db.trait_environment(owner.generic_def(db)),
            krate: trait_env.map_or_else(|| owner.krate(db), |env| env.krate),
        }
        .store(),
    )?;
    Ok(db.interpret_mir(body, trait_env)?)
}

/// Both paths' values of `owner` when it is of the shape, the HIR one first: for the
/// differential tests, which require them identical.
pub fn both<'db>(
    db: &'db Db,
    owner: InferBodyId<'db>,
    subst: GenericArgs<'db>,
    trait_env: Option<ParamEnvAndCrate<'db>>,
) -> Option<[Result<Allocation<'db>, ConstEvalError<'db>>; 2]> {
    let by_hir = by_hir(db, owner, trait_env)?;
    Some([by_hir, by_mir(db, owner, subst, trait_env)])
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Op {
    /// A value known when the program is made: a literal, a size or an alignment.
    Push(u128),
    /// The value of a `const` item with no generic parameters.
    Const(ConstId),
    Unary(UnaryOp, Kind),
    /// The operation and the kinds of its left and right sides.
    Binary(BinaryOp, Kind, Kind),
    /// `as` from the first kind to the second.
    Cast(Kind, Kind),
    /// The jump of `&&` (`on: false`) or `||` (`on: true`), after its left side: when that is
    /// `on`, it is the value and the `skip` ops of the right side are not run.
    Short { on: bool, skip: u16 },
}

/// A constant of the shape, compiled.
struct Program<'db> {
    ops: ArrayVec<Op, MAX_NODES>,
    ty: Ty<'db>,
    kind: Kind,
}

impl<'db> Program<'db> {
    fn of(db: &'db Db, owner: InferBodyId<'db>, env: ParamEnvAndCrate<'db>) -> Option<Self> {
        match owner {
            InferBodyId::DefWithBodyId(DefWithBodyId::ConstId(_) | DefWithBodyId::VariantId(_)) => {
            }
            InferBodyId::AnonConstId(konst) if !konst.is_inline_const(db) => {}
            _ => return None,
        }
        // The MIR evaluator's budget and pointer width are those of the constant's own crate;
        // a program of the shape takes far less than this budget.
        let krate = owner.krate(db);
        let budget = krate.workspace_data(db).const_eval;
        if budget.steps < 4096 || budget.memory < 1 << 20 {
            return None;
        }
        let infer = InferenceResult::of(db, owner);
        if infer.has_errors || infer.has_type_mismatches() || !infer.diagnostics().is_empty() {
            return None;
        }
        let (store, root) = owner.store_and_root_expr(db);
        if !store.macro_calls().is_empty() {
            return None;
        }
        let pointer_bytes = db.target_data_layout(krate).ok()?.pointer_size().bytes();
        let mut compiler =
            Compiler { db, infer, store, env, pointer_bytes, nodes: 0, ops: ArrayVec::new() };
        let kind = compiler.expr(root)?;
        Some(Program { ops: compiler.ops, ty: infer.expr_ty(root), kind })
    }

    fn run(
        &self,
        db: &'db Db,
        env: ParamEnvAndCrate<'db>,
    ) -> Result<Allocation<'db>, ConstEvalError<'db>> {
        let v = match exec(&self.ops, |konst| read_const(db, konst, env)) {
            Ok(v) => v,
            // The assert's panic unwinds out of the constant's frame, which is no function's, so
            // the trace is empty.
            Err(Stop::Panic(panic)) => {
                // alloc: the error is the MIR evaluator's, owned, and made only on failure.
                let panic = MirEvalError::Panic(panic.message().to_owned());
                return Err(in_constant(panic));
            }
            Err(Stop::Const(error)) => return Err(error),
        };
        // alloc: the value's bytes, interned with its type as the MIR evaluator's are.
        let memory = Box::from(&v.to_le_bytes()[..self.kind.bytes()]);
        Ok(Allocation::new(AllocationData { ty: self.ty, memory, memory_map: MemoryMap::Empty }))
    }
}

/// What stopped a program: a check that failed, or the error of a constant it read.
#[derive(Clone, PartialEq, Eq, Debug)]
enum Stop<E> {
    Panic(Panic),
    Const(E),
}

/// Runs `ops`, a program of the shape, reading each constant with `read`, and gives its value.
fn exec<E>(
    ops: &[Op],
    mut read: impl FnMut(ConstId) -> Result<u128, E>,
) -> Result<u128, Stop<E>> {
    let mut stack = ArrayVec::<u128, MAX_NODES>::new();
    let pop = |stack: &mut ArrayVec<u128, MAX_NODES>| {
        stack.pop().expect("a program pops only what it pushed")
    };
    let mut at = 0;
    while let Some(&op) = ops.get(at) {
        match op {
            Op::Push(v) => stack.push(v),
            Op::Const(konst) => stack.push(read(konst).map_err(Stop::Const)?),
            Op::Unary(op, k) => {
                let a = pop(&mut stack);
                stack.push(ops::unary(op, k, a).map_err(Stop::Panic)?);
            }
            Op::Binary(op, lk, rk) => {
                let b = pop(&mut stack);
                let a = pop(&mut stack);
                stack.push(ops::binary(op, lk, rk, a, b).map_err(Stop::Panic)?);
            }
            Op::Cast(from, to) => {
                let a = pop(&mut stack);
                stack.push(ops::cast(from, to, a));
            }
            Op::Short { on, skip } => {
                if (stack.last() == Some(&1)) == on {
                    at += usize::from(skip);
                } else {
                    pop(&mut stack);
                }
            }
        }
        at += 1;
    }
    let v = pop(&mut stack);
    debug_assert!(stack.is_empty());
    Ok(v)
}

/// The value of `konst`, as the MIR evaluator reads a constant it meets: from its query, its
/// error wrapped as the evaluator wraps it.
fn read_const<'db>(
    db: &'db Db,
    konst: ConstId,
    env: ParamEnvAndCrate<'db>,
) -> Result<u128, ConstEvalError<'db>> {
    match db.const_eval(konst, GenericArgs::empty(), Some(env)) {
        Ok(value) => Ok(u128::from_le_bytes(pad16(&value.memory, IsSigned::No))),
        Err(error) => {
            let name = crate::db::GeneralConstId::ConstId(konst).name(db);
            // alloc: the error is the MIR evaluator's, owned, and made only on failure.
            Err(in_constant(MirEvalError::ConstEvalError(name, Box::new(error))))
        }
    }
}

/// `error` raised in a constant's own frame, which is no function's: the MIR evaluator gives
/// it with an empty trace.
fn in_constant(error: MirEvalError<'_>) -> ConstEvalError<'_> {
    // alloc: the MIR evaluator's error, made only on failure; an empty `Vec` allocates nothing.
    ConstEvalError::MirEvalError(MirEvalError::InFunction(Box::new(error), Vec::new()))
}

struct Compiler<'a, 'db> {
    db: &'db Db,
    infer: &'db InferenceResult<'db>,
    store: &'a ExpressionStore,
    env: ParamEnvAndCrate<'db>,
    pointer_bytes: u64,
    nodes: usize,
    ops: ArrayVec<Op, MAX_NODES>,
}

impl<'db> Compiler<'_, 'db> {
    /// Compiles `id` and gives its kind, or `None` when it is not of the shape.
    fn expr(&mut self, id: ExprId) -> Option<Kind> {
        self.nodes += 1;
        if self.nodes > MAX_NODES || self.infer.expr_adjustment(id).is_some_and(|it| !it.is_empty())
        {
            return None;
        }
        let ty = self.infer.expr_ty(id);
        let kind = self.kind_of(ty)?;
        match self.store.expr(id) {
            Expr::Literal(literal) => self.literal(literal, kind, false)?,
            Expr::Path(_) => {
                if self.infer.assoc_resolutions_for_expr(id).is_some()
                    || self.infer.variant_resolution_for_expr(id).is_some()
                {
                    return None;
                }
                let Some(ValueNs::ConstId(konst)) = self.infer.value_resolution(id.into()) else {
                    return None;
                };
                if !matches!(konst.lookup(self.db).container, ItemContainerId::ModuleId(_))
                    || !crate::generics::generics(self.db, konst.into()).has_no_params()
                {
                    return None;
                }
                self.push(Op::Const(konst))?;
            }
            Expr::UnaryOp { expr, op: UnaryOp::Neg } => {
                if !kind.is_signed() {
                    return None;
                }
                // `-128i8` is that number, not the negation of one `i8` does not have.
                if let Expr::Literal(literal @ Literal::Int(..)) = self.store.expr(expr) {
                    self.nodes += 1;
                    if self.infer.expr_adjustment(expr).is_some_and(|it| !it.is_empty())
                        || self.infer.expr_ty(expr) != ty
                    {
                        return None;
                    }
                    self.literal(literal, kind, true)?;
                } else {
                    self.operand(expr, kind)?;
                    self.push(Op::Unary(UnaryOp::Neg, kind))?;
                }
            }
            Expr::UnaryOp { expr, op: UnaryOp::Not } => {
                if kind == Kind::Char {
                    return None;
                }
                self.operand(expr, kind)?;
                self.push(Op::Unary(UnaryOp::Not, kind))?;
            }
            Expr::BinaryOp { lhs, rhs, op: Some(op) } => self.binary(ty, kind, lhs, rhs, op)?,
            Expr::Cast { expr, .. } => {
                let from = self.expr(expr)?;
                if self.infer.coercion_casts.contains(&expr) {
                    // A cast the type checker made a coercion is a move of the bytes.
                    if self.infer.expr_ty(expr) != ty {
                        return None;
                    }
                } else {
                    let int_to_int = kind.is_int() || (kind == Kind::Char && from == Kind::U8);
                    if !int_to_int {
                        return None;
                    }
                    self.push(Op::Cast(from, kind))?;
                }
            }
            Expr::Call { callee, args: [] } => self.layout_fact(ty, callee)?,
            Expr::Block { statements, tail: Some(tail), label: None, .. }
                if statements.is_empty() =>
            {
                if self.expr(tail)? != kind {
                    return None;
                }
            }
            _ => return None,
        }
        Some(kind)
    }

    /// Compiles `id`, an operand that must be of `kind`.
    fn operand(&mut self, id: ExprId, kind: Kind) -> Option<()> {
        (self.expr(id)? == kind).then_some(())
    }

    fn binary(
        &mut self,
        ty: Ty<'db>,
        kind: Kind,
        lhs: ExprId,
        rhs: ExprId,
        op: BinaryOp,
    ) -> Option<()> {
        let (lhs_ty, rhs_ty) = (self.infer.expr_ty(lhs), self.infer.expr_ty(rhs));
        match op {
            BinaryOp::LogicOp(logic) => {
                if kind != Kind::Bool {
                    return None;
                }
                self.operand(lhs, Kind::Bool)?;
                let jump = self.ops.len();
                self.push(Op::Short { on: logic == LogicOp::Or, skip: 0 })?;
                self.operand(rhs, Kind::Bool)?;
                let skip = u16::try_from(self.ops.len() - jump - 1).ok()?;
                self.ops[jump] = Op::Short { on: logic == LogicOp::Or, skip };
            }
            BinaryOp::ArithOp(arith) => {
                use hir_def::hir::ArithOp::*;
                let lk = self.expr(lhs)?;
                let rk = self.expr(rhs)?;
                let fits = match arith {
                    // MIR building takes a shift of two integers as built in whatever their
                    // types; anything else of two different types is a trait's method.
                    Shl | Shr => kind.is_int() && rk.is_int() && lhs_ty == ty,
                    Add | Sub | Mul | Div | Rem => kind.is_int() && lhs_ty == ty && rhs_ty == ty,
                    BitAnd | BitOr | BitXor => {
                        kind != Kind::Char && lhs_ty == ty && rhs_ty == ty
                    }
                };
                if !fits {
                    return None;
                }
                self.push(Op::Binary(op, lk, rk))?;
            }
            BinaryOp::CmpOp(_) => {
                if kind != Kind::Bool || lhs_ty != rhs_ty {
                    return None;
                }
                let lk = self.expr(lhs)?;
                let rk = self.expr(rhs)?;
                self.push(Op::Binary(op, lk, rk))?;
            }
            BinaryOp::Assignment { .. } => return None,
        }
        Some(())
    }

    /// A value of `kind` written as `literal`, negated when `negated`, as MIR building writes
    /// it: its low bytes.
    fn literal(&mut self, literal: &Literal, kind: Kind, negated: bool) -> Option<()> {
        let bits = match *literal {
            Literal::Int(v, _) if kind.is_int() => {
                if negated { v.wrapping_neg() as u128 } else { v as u128 }
            }
            Literal::Uint(v, _) if kind.is_int() && !negated => v,
            Literal::Bool(b) if kind == Kind::Bool => u128::from(b),
            Literal::Char(c) if kind == Kind::Char => u128::from(u32::from(c)),
            _ => return None,
        };
        self.push(Op::Push(kind.wrap(bits)))
    }

    /// `size_of::<T>()` or `align_of::<T>()`, of type `ty`, for a `T` with no parameter whose
    /// layout is sized: the intrinsic, or `core::mem`'s function, which is it.
    fn layout_fact(&mut self, ty: Ty<'db>, callee: ExprId) -> Option<()> {
        self.nodes += 1;
        if !matches!(ty.kind(), TyKind::Uint(UintTy::Usize))
            || self.infer.expr_adjustment(callee).is_some_and(|it| !it.is_empty())
        {
            return None;
        }
        let TyKind::FnDef(def, args) = self.infer.expr_ty(callee).kind() else { return None };
        let CallableDefId::FunctionId(func) = def.0 else { return None };
        let is_size = match intrinsic_of(self.db, func) {
            Ok(Some(Intrinsic::SizeOf)) => true,
            Ok(Some(Intrinsic::AlignOf)) => false,
            Ok(_) => match AttrFlags::diagnostic_item(self.db, func.into())?.as_str() {
                "mem_size_of" => true,
                "mem_align_of" => false,
                _ => return None,
            },
            Err(_) => return None,
        };
        let [arg] = args.skip_binder().as_slice() else { return None };
        let of = arg.ty()?;
        if of.has_param() || of.has_infer() || of.has_aliases() || of.references_error() {
            return None;
        }
        let layout = self.db.layout_of_ty(of.store(), self.env.store()).ok()?;
        if layout.is_unsized() {
            return None;
        }
        let fact = if is_size { layout.size.bytes() } else { layout.align.bytes() };
        self.push(Op::Push(u128::from(fact)))
    }

    fn kind_of(&self, ty: Ty<'db>) -> Option<Kind> {
        let bytes = |bits: Option<u64>| bits.map_or(self.pointer_bytes, |it| it / 8);
        match ty.kind() {
            TyKind::Bool => Some(Kind::Bool),
            TyKind::Char => Some(Kind::Char),
            TyKind::Int(int) => Kind::int(bytes(int.bit_width()), true),
            TyKind::Uint(uint) => Kind::int(bytes(uint.bit_width()), false),
            _ => None,
        }
    }

    fn push(&mut self, op: Op) -> Option<()> {
        self.ops.try_push(op).ok()
    }
}
