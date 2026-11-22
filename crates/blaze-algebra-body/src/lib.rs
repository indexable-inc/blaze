//! `blaze-algebra-body`: proof-carrying `blaze-body` MIR ↔ `blaze-algebra` `ExprGraph` compiler
//! pass (`#![no_std]`, `#![forbid(unsafe_code)]`, `#![blaze::sans_io]`).
//!
//! Realizes the compiler seam between blaze's v2 typed generic MIR ([`blaze_body::Verified`])
//! and symbolic algebra / differentiation ([`blaze_algebra::ExprGraph`]):
//! - [`lift_body`]: lifts a verified straight-line pure numeric MIR body into an [`ExprGraph`],
//!   refusing impure, branching, or cyclic control flow with a typed [`BodyAlgebraError`] naming
//!   the exact [`Block`] and [`Stmt`].
//! - [`splice_body`]: performs a CSE topological walk over an [`ExprGraph`] and emits a fresh
//!   [`blaze_body::Body`] (including math `Call` continuation blocks) verified via
//!   [`blaze_body::verify`].
//! - [`diff_body`], [`grad_bodies`], [`simplify_body`]: one-shot `Verified<Body>` → verified
//!   `Body` MIR compiler transforms (forward symbolic diff, O(|V|) reverse-mode symbolic gradient,
//!   exact rational polynomial normalization, and bounded e-graph simplification).

#![no_std]
#![forbid(unsafe_code)]
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]

#[cfg(test)]
extern crate std;

use core::alloc::Allocator;
use core::fmt;

use blaze_algebra::{
    AlgebraError, ExprGraph, ExprId as AlgExprId, Op, VarId, det_tanh, diff, grad_reverse,
    simplify, simplify_poly_expr,
};
use blaze_body::{
    BinOp, Block, BlockKind, Body, BodyBuilder, BuildError, Callee, ConstDecl, ConstKind, Local,
    LocalDecl, LocalKind, Operand, Place, Role, Span, Stmt, StmtKind, UnOp, Unwind, Value,
    Verified, VerifyError,
    body::{ExprId as MirExprId, Mutbl, PTy},
    verify,
};
use blaze_own::Vec as OwnVec;

/// Pure scalar math operations recognized across `blaze-body` `Call` terminators.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum MathOp {
    Sin,
    Cos,
    Tan,
    Exp,
    Ln,
    Sqrt,
    Tanh,
    Abs,
    Pow,
}

impl MathOp {
    /// Expected number of `f64` operands for this math operation.
    #[inline]
    pub const fn arity(self) -> usize {
        match self {
            Self::Pow => 2,
            _ => 1,
        }
    }
}

/// Maps a MIR [`Callee`] to a known pure [`MathOp`] during [`lift_body`] / [`eval_pure_body`].
pub trait MathResolver {
    fn classify(&self, callee: Callee) -> Option<MathOp>;
}

/// Maps a [`MathOp`] back to a MIR [`Callee`] during [`splice_body`].
pub trait MathEmitter {
    fn callee_for(&self, op: MathOp) -> Option<Callee>;
}

/// Resolver/emitter that recognizes no math calls (pure `+`, `-`, `*`, `/`, `-x` bodies only).
#[derive(Clone, Copy, Default, Debug)]
pub struct NoMathCalls;

impl MathResolver for NoMathCalls {
    #[inline]
    fn classify(&self, _callee: Callee) -> Option<MathOp> {
        None
    }
}

impl MathEmitter for NoMathCalls {
    #[inline]
    fn callee_for(&self, _op: MathOp) -> Option<Callee> {
        None
    }
}

/// Planted bugs for MIR-pass oracle controls (`dst-not-review`, rule 43).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum BodyPlant {
    #[default]
    None,
    /// Splices the original un-differentiated expression instead of its derivative.
    SpliceOriginalInsteadOfDeriv,
}

/// Typed error returned by `blaze-algebra-body` (`errors-name-the-thing`, rule 19).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BodyAlgebraError {
    /// Body has no return local (`Local(0)`).
    MissingReturnLocal,
    /// Requested derivative argument index is out of range for the body's signature.
    ArgOutOfRange { arg: u32, args: u32 },
    /// Encountered a control-flow cycle or exceeded block traversal bound (`bounded-progress`).
    CycleOrStepLimit { block: Block, steps: u32 },
    /// Encountered a non-pure or unsupported statement/terminator at `(block, stmt)`.
    UnsupportedStmt { block: Block, stmt: Stmt },
    /// Encountered an unsupported rvalue (`Value`) at `(block, stmt)`.
    UnsupportedValue { block: Block, stmt: Stmt },
    /// Encountered a projected place (only bare scalar locals are lifted) at `(block, stmt)`.
    ProjectedPlace { block: Block, stmt: Stmt, place: Place },
    /// Encountered an unsupported constant kind (only scalar `f64` bits are lifted).
    UnsupportedConst { block: Block, stmt: Stmt },
    /// Encountered an unrecognized or wrong-arity call at `(block, stmt)`.
    UnsupportedCall { block: Block, stmt: Stmt },
    /// Read a local before it was assigned on the lifted straight-line path.
    UnassignedLocal { block: Block, stmt: Stmt, local: Local },
    /// Body returned without assigning the return place `Local(0)`.
    MissingReturnValue { block: Block },
    /// Splicing required a [`MathOp`] for which the [`MathEmitter`] supplied no [`Callee`].
    MissingMathCallee { op: MathOp },
    /// Underlying `blaze-algebra` failure.
    Algebra(AlgebraError),
    /// Underlying `blaze-body` builder failure.
    Build(BuildError),
    /// Emitted body failed [`blaze_body::verify`].
    Verify(VerifyError),
}

impl From<AlgebraError> for BodyAlgebraError {
    fn from(e: AlgebraError) -> Self {
        Self::Algebra(e)
    }
}

impl From<BuildError> for BodyAlgebraError {
    fn from(e: BuildError) -> Self {
        Self::Build(e)
    }
}

impl From<VerifyError> for BodyAlgebraError {
    fn from(e: VerifyError) -> Self {
        Self::Verify(e)
    }
}

impl fmt::Display for BodyAlgebraError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingReturnLocal => write!(f, "body has no return local _0"),
            Self::ArgOutOfRange { arg, args } => {
                write!(f, "derivative arg index {arg} out of range (body has {args} args)")
            }
            Self::CycleOrStepLimit { block, steps } => {
                write!(f, "control-flow cycle or step limit ({steps}) reached at {block}")
            }
            Self::UnsupportedStmt { block, stmt } => {
                write!(f, "unsupported non-pure statement or terminator at {block} {stmt}")
            }
            Self::UnsupportedValue { block, stmt } => {
                write!(f, "unsupported non-scalar rvalue at {block} {stmt}")
            }
            Self::ProjectedPlace { block, stmt, place } => {
                write!(f, "projected place {place} not supported at {block} {stmt}")
            }
            Self::UnsupportedConst { block, stmt } => {
                write!(f, "non-scalar constant not supported at {block} {stmt}")
            }
            Self::UnsupportedCall { block, stmt } => {
                write!(f, "unrecognized or impure call at {block} {stmt}")
            }
            Self::UnassignedLocal { block, stmt, local } => {
                write!(f, "read of unassigned local {local} at {block} {stmt}")
            }
            Self::MissingReturnValue { block } => {
                write!(f, "body returned at {block} without assigning _0")
            }
            Self::MissingMathCallee { op } => {
                write!(f, "math emitter supplied no callee for {op:?}")
            }
            Self::Algebra(e) => write!(f, "algebra error: {e}"),
            Self::Build(e) => write!(f, "body build error: {e:?}"),
            Self::Verify(e) => write!(f, "emitted body failed verify: {e:?}"),
        }
    }
}

impl core::error::Error for BodyAlgebraError {}

/// Result of lifting a verified pure numeric [`blaze_body::Body`] into [`ExprGraph`].
pub struct LiftedBody<A: Allocator + Copy> {
    pub graph: ExprGraph<A>,
    pub vars: OwnVec<VarId, A>,
    pub result: AlgExprId,
    pub f64_ty: PTy,
}

fn bare_local(
    body: Verified<'_>,
    block: Block,
    stmt: Stmt,
    place: Place,
) -> Result<Local, BodyAlgebraError> {
    let row = body.place(place);
    if row.depth != 0 || row.proj.is_some() {
        return Err(BodyAlgebraError::ProjectedPlace { block, stmt, place });
    }
    Ok(row.local)
}

fn lift_operand<A: Allocator + Copy>(
    body: Verified<'_>,
    block: Block,
    stmt: Stmt,
    op: Operand,
    env: &[Option<AlgExprId>],
    g: &mut ExprGraph<A>,
) -> Result<AlgExprId, BodyAlgebraError> {
    match op {
        Operand::Copy(p) | Operand::Move(p) => {
            let local = bare_local(body, block, stmt, p)?;
            env.get(local.index())
                .copied()
                .flatten()
                .ok_or(BodyAlgebraError::UnassignedLocal { block, stmt, local })
        }
        Operand::Const(c) => match body.konst(c).kind {
            ConstKind::Scalar { bits } => Ok(g.lit(f64::from_bits(bits as u64))),
            _ => Err(BodyAlgebraError::UnsupportedConst { block, stmt }),
        },
    }
}

/// Lift a verified pure straight-line scalar numeric `f64` [`blaze_body::Body`] into an
/// [`ExprGraph`], mapping `Local(1..=args)` to `VarId(0..args)` and `Local(0)` to `result`.
pub fn lift_body<A: Allocator + Copy, R: MathResolver>(
    body: Verified<'_>,
    math: &R,
    alloc: A,
) -> Result<LiftedBody<A>, BodyAlgebraError> {
    let nlocals = body.locals() as usize;
    if nlocals == 0 {
        return Err(BodyAlgebraError::MissingReturnLocal);
    }
    let f64_ty = body.local(Local(0)).ty;
    let nargs = body.args();

    let mut graph = ExprGraph::new_in(alloc);
    let mut vars: OwnVec<VarId, A> = OwnVec::with_capacity_in(nargs as usize, alloc);
    let mut env: OwnVec<Option<AlgExprId>, A> = OwnVec::with_capacity_in(nlocals, alloc);
    for _ in 0..nlocals {
        env.push(None);
    }

    // Bind argument locals `Local(1..=args)` as symbolic variables.
    const ARG_NAMES: [&str; 8] = ["x", "y", "z", "w", "a", "b", "c", "d"];
    for i in 0..nargs {
        let name = ARG_NAMES.get(i as usize).copied().unwrap_or("v");
        let (vid, eid) = if (i as usize) < ARG_NAMES.len() {
            graph.var(name)
        } else {
            let mut buf = blaze_own::String::new_in(alloc);
            let _ = core::fmt::Write::write_fmt(&mut buf, format_args!("arg{i}"));
            graph.var(buf.as_str())
        };
        vars.push(vid);
        env[(i + 1) as usize] = Some(eid);
    }

    // Follow straight-line blocks starting at `Block(0)` up to `body.blocks()` steps (rejecting cycles).
    let max_blocks = body.blocks();
    let mut visited: OwnVec<bool, A> = OwnVec::with_capacity_in(max_blocks as usize, alloc);
    for _ in 0..max_blocks {
        visited.push(false);
    }

    let mut cur = Block(0);
    let mut steps = 0u32;
    loop {
        if steps >= max_blocks || visited[cur.index()] {
            return Err(BodyAlgebraError::CycleOrStepLimit { block: cur, steps });
        }
        visited[cur.index()] = true;
        steps += 1;

        let brow = body.block(cur);
        for s_idx in brow.stmts.start..brow.stmts.end {
            let stmt = Stmt(s_idx);
            match body.stmt(stmt).kind {
                StmtKind::StorageLive(_) | StmtKind::StorageDead(_) => {}
                StmtKind::Assign { dest, value } => {
                    let local = bare_local(body, cur, stmt, dest)?;
                    let expr = match value {
                        Value::Use(op) => lift_operand(body, cur, stmt, op, &env, &mut graph)?,
                        Value::Unary { op: UnOp::Neg, operand } => {
                            let a = lift_operand(body, cur, stmt, operand, &env, &mut graph)?;
                            graph.neg(a)
                        }
                        Value::Binary { op, lhs, rhs } => {
                            let a = lift_operand(body, cur, stmt, lhs, &env, &mut graph)?;
                            let b = lift_operand(body, cur, stmt, rhs, &env, &mut graph)?;
                            match op {
                                BinOp::Add => graph.add(a, b),
                                BinOp::Sub => graph.sub(a, b),
                                BinOp::Mul => graph.mul(a, b),
                                BinOp::Div => graph.div(a, b),
                                _ => return Err(BodyAlgebraError::UnsupportedValue { block: cur, stmt }),
                            }
                        }
                        _ => return Err(BodyAlgebraError::UnsupportedValue { block: cur, stmt }),
                    };
                    env[local.index()] = Some(expr);
                }
                StmtKind::Goto(next) => {
                    cur = next;
                    break;
                }
                StmtKind::Call { callee, args, dest, target, .. } => {
                    let Some(math_op) = math.classify(body.callee(callee)) else {
                        return Err(BodyAlgebraError::UnsupportedCall { block: cur, stmt });
                    };
                    let (Some(dest_place), Some(next_block)) = (dest, target) else {
                        return Err(BodyAlgebraError::UnsupportedCall { block: cur, stmt });
                    };
                    if args.len() as usize != math_op.arity() {
                        return Err(BodyAlgebraError::UnsupportedCall { block: cur, stmt });
                    }
                    let mut lifted_args: [AlgExprId; 2] = [AlgExprId(0); 2];
                    for (k, op) in body.ops(args).enumerate() {
                        lifted_args[k] = lift_operand(body, cur, stmt, op, &env, &mut graph)?;
                    }
                    let res_expr = match math_op {
                        MathOp::Sin => graph.sin(lifted_args[0]),
                        MathOp::Cos => graph.cos(lifted_args[0]),
                        MathOp::Tan => graph.tan(lifted_args[0]),
                        MathOp::Exp => graph.exp(lifted_args[0]),
                        MathOp::Ln => graph.ln(lifted_args[0]),
                        MathOp::Sqrt => graph.sqrt(lifted_args[0]),
                        MathOp::Tanh => graph.tanh(lifted_args[0]),
                        MathOp::Abs => graph.abs(lifted_args[0]),
                        MathOp::Pow => graph.pow(lifted_args[0], lifted_args[1]),
                    };
                    let local = bare_local(body, cur, stmt, dest_place)?;
                    env[local.index()] = Some(res_expr);
                    cur = next_block;
                    break;
                }
                StmtKind::Return => {
                    let result = env[0].ok_or(BodyAlgebraError::MissingReturnValue { block: cur })?;
                    return Ok(LiftedBody { graph, vars, result, f64_ty });
                }
                _ => return Err(BodyAlgebraError::UnsupportedStmt { block: cur, stmt }),
            }
        }
    }
}

fn collect_postorder<A: Allocator + Copy>(
    g: &ExprGraph<A>,
    id: AlgExprId,
    visited: &mut OwnVec<bool, A>,
    order: &mut OwnVec<AlgExprId, A>,
) -> Result<(), AlgebraError> {
    let idx = id.0 as usize;
    if visited[idx] {
        return Ok(());
    }
    visited[idx] = true;
    match g.node(id)? {
        Op::Const(_) | Op::Var(_) => {}
        Op::Neg(a)
        | Op::Sin(a)
        | Op::Cos(a)
        | Op::Tan(a)
        | Op::Exp(a)
        | Op::Ln(a)
        | Op::Sqrt(a)
        | Op::Tanh(a)
        | Op::Abs(a) => collect_postorder(g, a, visited, order)?,
        Op::Add(a, b) | Op::Sub(a, b) | Op::Mul(a, b) | Op::Div(a, b) | Op::Pow(a, b) => {
            collect_postorder(g, a, visited, order)?;
            collect_postorder(g, b, visited, order)?;
        }
    }
    order.push(id);
    Ok(())
}

/// Splice any [`ExprId`](AlgExprId) from [`ExprGraph`] back into a fresh, CSE-shared, verified
/// [`blaze_body::Body`]. Every shared DAG node is computed once into a temporary local; math
/// operations emit real [`StmtKind::Call`] terminators with continuation blocks.
pub fn splice_body<A: Allocator + Copy, E: MathEmitter>(
    g: &ExprGraph<A>,
    root: AlgExprId,
    vars: &[VarId],
    f64_ty: PTy,
    math: &E,
    alloc: A,
) -> Result<Body<A>, BodyAlgebraError> {
    let n = g.len();
    if (root.0 as usize) >= n {
        return Err(BodyAlgebraError::Algebra(AlgebraError::InvalidExpr {
            expr: root,
            node_count: n,
        }));
    }

    let mut visited: OwnVec<bool, A> = OwnVec::with_capacity_in(n, alloc);
    for _ in 0..n {
        visited.push(false);
    }
    let mut order: OwnVec<AlgExprId, A> = OwnVec::new_in(alloc);
    collect_postorder(g, root, &mut visited, &mut order)?;

    let mut b = BodyBuilder::new_in(alloc);
    let ret_local = b.new_local(LocalDecl { ty: f64_ty, kind: LocalKind::Return, mutbl: Mutbl::Mut });
    let ret_place = b.local_place(ret_local);

    let mut var_places: OwnVec<Place, A> = OwnVec::with_capacity_in(vars.len(), alloc);
    for (idx, _) in vars.iter().enumerate() {
        let l = b.new_local(LocalDecl {
            ty: f64_ty,
            kind: LocalKind::Arg(idx as u32),
            mutbl: Mutbl::Not,
        });
        var_places.push(b.local_place(l));
    }

    // Map each `ExprId` to the MIR `Operand` holding its CSE value.
    let mut node_ops: OwnVec<Option<Operand>, A> = OwnVec::with_capacity_in(n, alloc);
    for _ in 0..n {
        node_ops.push(None);
    }

    let mut cur_block = b.new_block(BlockKind::Normal);
    b.start(cur_block);
    let span = Span { lo: 0, hi: 0 };

    let mut next_temp_id = 0u32;
    let mut alloc_temp = |b: &mut BodyBuilder<A>| -> Place {
        let l = b.new_local(LocalDecl {
            ty: f64_ty,
            kind: LocalKind::Temp(MirExprId(next_temp_id), Role::Value),
            mutbl: Mutbl::Not,
        });
        next_temp_id += 1;
        b.local_place(l)
    };

    let mut emit_math_call = |b: &mut BodyBuilder<A>,
                              op: MathOp,
                              call_args: &[Operand],
                              dest: Place,
                              cur_block: &mut Block|
     -> Result<(), BodyAlgebraError> {
        let callee = math.callee_for(op).ok_or(BodyAlgebraError::MissingMathCallee { op })?;
        let cid = b.callee(callee);
        let ops = b.operands(call_args);
        let next = b.new_block(BlockKind::Normal);
        b.terminate(
            StmtKind::Call {
                callee: cid,
                args: ops,
                dest: Some(dest),
                target: Some(next),
                unwind: Unwind::Continue,
            },
            span,
        );
        b.start(next);
        *cur_block = next;
        Ok(())
    };

    for &id in order.iter() {
        let is_root = id == root;
        let op = g.node(id)?;
        match op {
            Op::Const(bits) => {
                let c = b.new_const(ConstDecl {
                    kind: ConstKind::Scalar { bits: u128::from(bits) },
                    ty: f64_ty,
                });
                let operand = Operand::Const(c);
                node_ops[id.0 as usize] = Some(operand);
                if is_root {
                    b.assign(ret_place, Value::Use(operand), span);
                }
            }
            Op::Var(vid) => {
                let pos = vars
                    .iter()
                    .position(|&v| v == vid)
                    .ok_or(BodyAlgebraError::Algebra(AlgebraError::UnknownVar {
                        var: vid,
                        env_len: vars.len(),
                    }))?;
                let operand = Operand::Copy(var_places[pos]);
                node_ops[id.0 as usize] = Some(operand);
                if is_root {
                    b.assign(ret_place, Value::Use(operand), span);
                }
            }
            Op::Neg(a) => {
                let oa = node_ops[a.0 as usize].expect("postorder visited child");
                let dest = if is_root { ret_place } else { alloc_temp(&mut b) };
                b.assign(dest, Value::Unary { op: UnOp::Neg, operand: oa }, span);
                node_ops[id.0 as usize] = Some(Operand::Copy(dest));
            }
            Op::Add(a, b_id) | Op::Sub(a, b_id) | Op::Mul(a, b_id) | Op::Div(a, b_id) => {
                let oa = node_ops[a.0 as usize].expect("postorder visited lhs");
                let ob = node_ops[b_id.0 as usize].expect("postorder visited rhs");
                let bin = match op {
                    Op::Add(_, _) => BinOp::Add,
                    Op::Sub(_, _) => BinOp::Sub,
                    Op::Mul(_, _) => BinOp::Mul,
                    Op::Div(_, _) => BinOp::Div,
                    _ => unreachable!(),
                };
                let dest = if is_root { ret_place } else { alloc_temp(&mut b) };
                b.assign(dest, Value::Binary { op: bin, lhs: oa, rhs: ob }, span);
                node_ops[id.0 as usize] = Some(Operand::Copy(dest));
            }
            Op::Pow(a, b_id) => {
                let oa = node_ops[a.0 as usize].expect("postorder visited base");
                let ob = node_ops[b_id.0 as usize].expect("postorder visited exp");
                let dest = if is_root { ret_place } else { alloc_temp(&mut b) };
                emit_math_call(&mut b, MathOp::Pow, &[oa, ob], dest, &mut cur_block)?;
                node_ops[id.0 as usize] = Some(Operand::Copy(dest));
            }
            Op::Sin(a)
            | Op::Cos(a)
            | Op::Tan(a)
            | Op::Exp(a)
            | Op::Ln(a)
            | Op::Sqrt(a)
            | Op::Tanh(a)
            | Op::Abs(a) => {
                let oa = node_ops[a.0 as usize].expect("postorder visited arg");
                let mop = match op {
                    Op::Sin(_) => MathOp::Sin,
                    Op::Cos(_) => MathOp::Cos,
                    Op::Tan(_) => MathOp::Tan,
                    Op::Exp(_) => MathOp::Exp,
                    Op::Ln(_) => MathOp::Ln,
                    Op::Sqrt(_) => MathOp::Sqrt,
                    Op::Tanh(_) => MathOp::Tanh,
                    Op::Abs(_) => MathOp::Abs,
                    _ => unreachable!(),
                };
                let dest = if is_root { ret_place } else { alloc_temp(&mut b) };
                emit_math_call(&mut b, mop, &[oa], dest, &mut cur_block)?;
                node_ops[id.0 as usize] = Some(Operand::Copy(dest));
            }
        }
    }

    b.terminate(StmtKind::Return, span);
    let body = b.finish()?;
    // Proof check (rule 74 `verify-once`): guarantee every spliced body passes `blaze_body::verify`!
    let _ = verify(&body)?;
    Ok(body)
}

/// End-to-end MIR → MIR symbolic differentiation pass:
/// `Verified<Body>` → [`lift_body`] → `diff` → `simplify_poly_expr` + `simplify` → [`splice_body`].
pub fn diff_body<A: Allocator + Copy, R: MathResolver, E: MathEmitter>(
    body: Verified<'_>,
    arg_index: u32,
    resolve: &R,
    emit: &E,
    alloc: A,
) -> Result<Body<A>, BodyAlgebraError> {
    diff_body_with_plant(body, arg_index, resolve, emit, BodyPlant::None, alloc)
}

/// End-to-end MIR differentiation with [`BodyPlant`] for planted-bug oracle controls.
pub fn diff_body_with_plant<A: Allocator + Copy, R: MathResolver, E: MathEmitter>(
    body: Verified<'_>,
    arg_index: u32,
    resolve: &R,
    emit: &E,
    plant: BodyPlant,
    alloc: A,
) -> Result<Body<A>, BodyAlgebraError> {
    let nargs = body.args();
    if arg_index >= nargs {
        return Err(BodyAlgebraError::ArgOutOfRange { arg: arg_index, args: nargs });
    }
    let mut lifted = lift_body(body, resolve, alloc)?;
    let wrt = lifted.vars[arg_index as usize];
    let target = if plant == BodyPlant::SpliceOriginalInsteadOfDeriv {
        lifted.result
    } else {
        let d = diff(&mut lifted.graph, lifted.result, wrt)?;
        let d = simplify_poly_expr(&mut lifted.graph, d)?;
        simplify(&mut lifted.graph, d)?
    };
    splice_body(&lifted.graph, target, &lifted.vars, lifted.f64_ty, emit, alloc)
}

/// End-to-end MIR → MIR algebraic simplification pass (exact polynomial cancellation + e-graph):
/// `Verified<Body>` → [`lift_body`] → `simplify_poly_expr` + `simplify` → [`splice_body`].
pub fn simplify_body<A: Allocator + Copy, R: MathResolver, E: MathEmitter>(
    body: Verified<'_>,
    resolve: &R,
    emit: &E,
    alloc: A,
) -> Result<Body<A>, BodyAlgebraError> {
    let mut lifted = lift_body(body, resolve, alloc)?;
    let s = simplify_poly_expr(&mut lifted.graph, lifted.result)?;
    let s = simplify(&mut lifted.graph, s)?;
    splice_body(&lifted.graph, s, &lifted.vars, lifted.f64_ty, emit, alloc)
}

/// End-to-end MIR → MIR full symbolic gradient pass using single-sweep O(|V|) reverse-mode
/// adjoints ([`grad_reverse`]), returning one verified [`Body`] per argument.
pub fn grad_bodies<A: Allocator + Copy, R: MathResolver, E: MathEmitter>(
    body: Verified<'_>,
    resolve: &R,
    emit: &E,
    alloc: A,
) -> Result<OwnVec<Body<A>, A>, BodyAlgebraError> {
    let mut lifted = lift_body(body, resolve, alloc)?;
    let grads = grad_reverse(&mut lifted.graph, lifted.result, &lifted.vars)?;
    let mut out = OwnVec::with_capacity_in(grads.len(), alloc);
    for &d in grads.iter() {
        let d = simplify_poly_expr(&mut lifted.graph, d)?;
        let d = simplify(&mut lifted.graph, d)?;
        let mir = splice_body(&lifted.graph, d, &lifted.vars, lifted.f64_ty, emit, alloc)?;
        out.push(mir);
    }
    Ok(out)
}

/// Deterministic sans-IO scalar `f64` evaluator over any verified straight-line pure numeric
/// [`blaze_body::Body`], using `blaze_math` for transcendental ops.
pub fn eval_pure_body<A: Allocator + Copy, R: MathResolver>(
    body: Verified<'_>,
    args_in: &[f64],
    math: &R,
    alloc: A,
) -> Result<f64, BodyAlgebraError> {
    let nlocals = body.locals() as usize;
    if nlocals == 0 {
        return Err(BodyAlgebraError::MissingReturnLocal);
    }
    let nargs = body.args() as usize;
    if args_in.len() < nargs {
        return Err(BodyAlgebraError::ArgOutOfRange {
            arg: args_in.len() as u32,
            args: nargs as u32,
        });
    }

    let mut env: OwnVec<Option<f64>, A> = OwnVec::with_capacity_in(nlocals, alloc);
    for _ in 0..nlocals {
        env.push(None);
    }
    for (i, &v) in args_in.iter().take(nargs).enumerate() {
        env[i + 1] = Some(v);
    }

    let read_op = |block: Block, stmt: Stmt, op: Operand, env: &[Option<f64>]| -> Result<f64, BodyAlgebraError> {
        match op {
            Operand::Copy(p) | Operand::Move(p) => {
                let local = bare_local(body, block, stmt, p)?;
                env.get(local.index())
                    .copied()
                    .flatten()
                    .ok_or(BodyAlgebraError::UnassignedLocal { block, stmt, local })
            }
            Operand::Const(c) => match body.konst(c).kind {
                ConstKind::Scalar { bits } => Ok(f64::from_bits(bits as u64)),
                _ => Err(BodyAlgebraError::UnsupportedConst { block, stmt }),
            },
        }
    };

    let max_blocks = body.blocks();
    let mut visited: OwnVec<bool, A> = OwnVec::with_capacity_in(max_blocks as usize, alloc);
    for _ in 0..max_blocks {
        visited.push(false);
    }

    let mut cur = Block(0);
    let mut steps = 0u32;
    loop {
        if steps >= max_blocks || visited[cur.index()] {
            return Err(BodyAlgebraError::CycleOrStepLimit { block: cur, steps });
        }
        visited[cur.index()] = true;
        steps += 1;

        let brow = body.block(cur);
        for s_idx in brow.stmts.start..brow.stmts.end {
            let stmt = Stmt(s_idx);
            match body.stmt(stmt).kind {
                StmtKind::StorageLive(_) | StmtKind::StorageDead(_) => {}
                StmtKind::Assign { dest, value } => {
                    let local = bare_local(body, cur, stmt, dest)?;
                    let val = match value {
                        Value::Use(op) => read_op(cur, stmt, op, &env)?,
                        Value::Unary { op: UnOp::Neg, operand } => -read_op(cur, stmt, operand, &env)?,
                        Value::Binary { op, lhs, rhs } => {
                            let a = read_op(cur, stmt, lhs, &env)?;
                            let b = read_op(cur, stmt, rhs, &env)?;
                            match op {
                                BinOp::Add => a + b,
                                BinOp::Sub => a - b,
                                BinOp::Mul => a * b,
                                BinOp::Div => a / b,
                                _ => return Err(BodyAlgebraError::UnsupportedValue { block: cur, stmt }),
                            }
                        }
                        _ => return Err(BodyAlgebraError::UnsupportedValue { block: cur, stmt }),
                    };
                    env[local.index()] = Some(val);
                }
                StmtKind::Goto(next) => {
                    cur = next;
                    break;
                }
                StmtKind::Call { callee, args, dest, target, .. } => {
                    let Some(math_op) = math.classify(body.callee(callee)) else {
                        return Err(BodyAlgebraError::UnsupportedCall { block: cur, stmt });
                    };
                    let (Some(dest_place), Some(next_block)) = (dest, target) else {
                        return Err(BodyAlgebraError::UnsupportedCall { block: cur, stmt });
                    };
                    if args.len() as usize != math_op.arity() {
                        return Err(BodyAlgebraError::UnsupportedCall { block: cur, stmt });
                    }
                    let mut vals = [0.0_f64; 2];
                    for (k, op) in body.ops(args).enumerate() {
                        vals[k] = read_op(cur, stmt, op, &env)?;
                    }
                    let res = match math_op {
                        MathOp::Sin => blaze_math::sin(vals[0]),
                        MathOp::Cos => blaze_math::cos(vals[0]),
                        MathOp::Tan => blaze_math::tan(vals[0]),
                        MathOp::Exp => blaze_math::exp(vals[0]),
                        MathOp::Ln => blaze_math::ln(vals[0]),
                        MathOp::Sqrt => blaze_math::sqrt(vals[0]),
                        MathOp::Tanh => det_tanh(vals[0]),
                        MathOp::Abs => blaze_math::abs(vals[0]),
                        MathOp::Pow => blaze_math::pow(vals[0], vals[1]),
                    };
                    let local = bare_local(body, cur, stmt, dest_place)?;
                    env[local.index()] = Some(res);
                    cur = next_block;
                    break;
                }
                StmtKind::Return => {
                    return env[0].ok_or(BodyAlgebraError::MissingReturnValue { block: cur });
                }
                _ => return Err(BodyAlgebraError::UnsupportedStmt { block: cur, stmt }),
            }
        }
    }
}

#[cfg(test)]
mod tests;
