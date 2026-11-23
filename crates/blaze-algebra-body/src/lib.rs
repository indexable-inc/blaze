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
    AlgebraError, ExprGraph, ExprId as AlgExprId, Interval, Op, RewriteMode, SimplifyOpts, VarId,
    det_tanh, diff, grad_reverse, hvp, prune_branches, simplify, simplify_poly_expr,
    simplify_with,
};
use blaze_body::{
    AggregateKind, BinOp, Block, BlockKind, Body, BodyBuilder, BuildError, Callee, ConstDecl,
    ConstKind, Local, LocalDecl, LocalKind, Operand, Place, Proj, Role, Span, Stmt, StmtKind,
    UnOp, Unwind, Value, Verified, VerifyError,
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
    /// Fused multiply-add `mul_add(a, b, c) = a * b + c`.
    Fma,
}

impl MathOp {
    /// Expected number of `f64` operands for this math operation.
    #[inline]
    pub const fn arity(self) -> usize {
        match self {
            Self::Fma => 3,
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
    /// Splicing an [`Op::Select`] requires a `bool` type handle in the target type table.
    MissingBoolType,
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
            Self::MissingBoolType => {
                write!(f, "splicing Op::Select requires a bool type handle")
            }
            Self::Algebra(e) => write!(f, "algebra error: {e}"),
            Self::Build(e) => write!(f, "body build error: {e:?}"),
            Self::Verify(e) => write!(f, "emitted body failed verify: {e:?}"),
        }
    }
}

impl core::error::Error for BodyAlgebraError {}

/// Portable scalar type handles used when splicing an [`ExprGraph`] into MIR.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScalarTys {
    pub f64_ty: PTy,
    pub bool_ty: PTy,
}

/// Result of lifting a verified pure numeric [`blaze_body::Body`] into [`ExprGraph`].
pub struct LiftedBody<A: Allocator + Copy> {
    pub graph: ExprGraph<A>,
    pub vars: OwnVec<VarId, A>,
    pub result: AlgExprId,
    pub f64_ty: PTy,
    pub bool_ty: Option<PTy>,
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

const MAX_SWITCH_DEPTH: u32 = 16;
const MAX_UNROLL_STEPS: u32 = 64;
const MAX_ARRAY_ELEMS: usize = 16;

type ElemSlots = [Option<AlgExprId>; MAX_ARRAY_ELEMS];

#[derive(Clone, Copy)]
enum PlaceTarget {
    Bare(Local),
    Elem(Local, usize),
}

struct LiftState<A: Allocator + Copy> {
    env: OwnVec<Option<AlgExprId>, A>,
    cmp_env: OwnVec<Option<AlgExprId>, A>,
    int_env: OwnVec<Option<i64>, A>,
    elem_env: OwnVec<Option<ElemSlots>, A>,
    bool_ty: Option<PTy>,
}

impl<A: Allocator + Copy> LiftState<A> {
    fn new_in(nlocals: usize, alloc: A) -> Self {
        let mut env = OwnVec::with_capacity_in(nlocals, alloc);
        let mut cmp_env = OwnVec::with_capacity_in(nlocals, alloc);
        let mut int_env = OwnVec::with_capacity_in(nlocals, alloc);
        let mut elem_env = OwnVec::with_capacity_in(nlocals, alloc);
        for _ in 0..nlocals {
            env.push(None);
            cmp_env.push(None);
            int_env.push(None);
            elem_env.push(None);
        }
        Self { env, cmp_env, int_env, elem_env, bool_ty: None }
    }

    fn snapshot(&self, alloc: A) -> Self {
        let n = self.env.len();
        let mut env = OwnVec::with_capacity_in(n, alloc);
        let mut cmp_env = OwnVec::with_capacity_in(n, alloc);
        let mut int_env = OwnVec::with_capacity_in(n, alloc);
        let mut elem_env = OwnVec::with_capacity_in(n, alloc);
        for i in 0..n {
            env.push(self.env[i]);
            cmp_env.push(self.cmp_env[i]);
            int_env.push(self.int_env[i]);
            elem_env.push(self.elem_env[i]);
        }
        Self { env, cmp_env, int_env, elem_env, bool_ty: self.bool_ty }
    }

    fn set_scalar(&mut self, local: Local, expr: AlgExprId) {
        let i = local.index();
        self.env[i] = Some(expr);
        self.cmp_env[i] = None;
        self.int_env[i] = None;
    }

    fn set_int(&mut self, local: Local, val: i64) {
        let i = local.index();
        self.int_env[i] = Some(val);
        self.env[i] = None;
        self.cmp_env[i] = None;
    }

    fn set_cmp(&mut self, local: Local, cond: AlgExprId) {
        let i = local.index();
        self.cmp_env[i] = Some(cond);
        self.env[i] = None;
        self.int_env[i] = None;
    }

    fn set_elem(&mut self, local: Local, idx: usize, expr: AlgExprId) {
        let slot = self.elem_env[local.index()].get_or_insert([None; MAX_ARRAY_ELEMS]);
        slot[idx] = Some(expr);
    }
}

fn resolve_place(
    body: Verified<'_>,
    block: Block,
    stmt: Stmt,
    place: Place,
    int_env: &[Option<i64>],
) -> Result<PlaceTarget, BodyAlgebraError> {
    let row = body.place(place);
    if row.depth == 0 && row.proj.is_none() {
        return Ok(PlaceTarget::Bare(row.local));
    }
    if row.depth == 1 {
        let idx = match row.proj {
            Some(Proj::Field { index, .. }) => usize::try_from(index).ok(),
            Some(Proj::ConstIndex { offset, from_end: false, .. }) => usize::try_from(offset).ok(),
            Some(Proj::Index(idx_local)) => {
                int_env.get(idx_local.index()).copied().flatten().and_then(|v| usize::try_from(v).ok())
            }
            _ => None,
        };
        if let Some(i) = idx
            && i < MAX_ARRAY_ELEMS
        {
            return Ok(PlaceTarget::Elem(row.local, i));
        }
    }
    Err(BodyAlgebraError::ProjectedPlace { block, stmt, place })
}

fn try_int_operand(body: Verified<'_>, f64_ty: PTy, op: Operand, int_env: &[Option<i64>]) -> Option<i64> {
    match op {
        Operand::Copy(p) | Operand::Move(p) => {
            let row = body.place(p);
            if row.depth == 0 && row.proj.is_none() {
                int_env.get(row.local.index()).copied().flatten()
            } else {
                None
            }
        }
        Operand::Const(c) => {
            let decl = body.konst(c);
            if decl.ty == f64_ty {
                return None;
            }
            match decl.kind {
                ConstKind::Scalar { bits } => Some(bits as i64),
                _ => None,
            }
        }
    }
}

fn lift_scalar_operand<A: Allocator + Copy>(
    body: Verified<'_>,
    f64_ty: PTy,
    block: Block,
    stmt: Stmt,
    op: Operand,
    state: &mut LiftState<A>,
    vars: &mut OwnVec<VarId, A>,
    g: &mut ExprGraph<A>,
    alloc: A,
) -> Result<AlgExprId, BodyAlgebraError> {
    match op {
        Operand::Copy(p) | Operand::Move(p) => match resolve_place(body, block, stmt, p, &state.int_env)? {
            PlaceTarget::Bare(local) => state
                .env
                .get(local.index())
                .copied()
                .flatten()
                .ok_or(BodyAlgebraError::UnassignedLocal { block, stmt, local }),
            PlaceTarget::Elem(local, idx) => {
                if let Some(eid) = state.elem_env[local.index()].and_then(|s| s[idx]) {
                    return Ok(eid);
                }
                // Lazily bind elements of array/tuple argument locals (`Local(1..=args)`).
                if local.0 >= 1 && local.0 <= body.args() {
                    let mut buf = blaze_own::String::new_in(alloc);
                    let _ = core::fmt::Write::write_fmt(&mut buf, format_args!("a{}_{idx}", local.0 - 1));
                    let (vid, eid) = g.var(buf.as_str());
                    vars.push(vid);
                    state.set_elem(local, idx, eid);
                    return Ok(eid);
                }
                Err(BodyAlgebraError::UnassignedLocal { block, stmt, local })
            }
        },
        Operand::Const(c) => {
            let decl = body.konst(c);
            if decl.ty != f64_ty {
                return Err(BodyAlgebraError::UnsupportedConst { block, stmt });
            }
            match decl.kind {
                ConstKind::Scalar { bits } => Ok(g.lit(f64::from_bits(bits as u64))),
                _ => Err(BodyAlgebraError::UnsupportedConst { block, stmt }),
            }
        }
    }
}

fn collect_reachable_blocks<A: Allocator + Copy>(
    body: Verified<'_>,
    start: Block,
    alloc: A,
) -> OwnVec<bool, A> {
    let nb = body.blocks() as usize;
    let mut seen = OwnVec::with_capacity_in(nb, alloc);
    for _ in 0..nb {
        seen.push(false);
    }
    let mut stack = OwnVec::new_in(alloc);
    stack.push(start);
    while let Some(b) = stack.pop() {
        if seen[b.index()] {
            continue;
        }
        seen[b.index()] = true;
        let term = body.terminator(b);
        match body.stmt(term).kind {
            StmtKind::Goto(next) => stack.push(next),
            StmtKind::Call { target: Some(next), .. } => stack.push(next),
            StmtKind::Switch { cases, otherwise, .. } => {
                stack.push(otherwise);
                for (_, tgt) in body.cases(cases) {
                    stack.push(tgt);
                }
            }
            _ => {}
        }
    }
    seen
}

/// Find the earliest common post-branch join block reachable from both `bb_then` and `bb_else`,
/// or `None` if both branches terminate without rejoining (e.g. `if c { return a; } else { return b; }`).
fn find_join_block<A: Allocator + Copy>(
    body: Verified<'_>,
    bb_then: Block,
    bb_else: Block,
    alloc: A,
) -> Option<Block> {
    if bb_then == bb_else {
        return Some(bb_then);
    }
    let reach_then = collect_reachable_blocks(body, bb_then, alloc);
    let nb = body.blocks() as usize;
    let mut visited = OwnVec::with_capacity_in(nb, alloc);
    for _ in 0..nb {
        visited.push(false);
    }
    let mut queue = OwnVec::new_in(alloc);
    queue.push(bb_else);
    let mut head = 0usize;
    while head < queue.len() {
        let cur = queue[head];
        head += 1;
        if visited[cur.index()] {
            continue;
        }
        visited[cur.index()] = true;
        if reach_then[cur.index()] {
            return Some(cur);
        }
        let term = body.terminator(cur);
        match body.stmt(term).kind {
            StmtKind::Goto(next) => queue.push(next),
            StmtKind::Call { target: Some(next), .. } => queue.push(next),
            StmtKind::Switch { cases, otherwise, .. } => {
                queue.push(otherwise);
                for (_, tgt) in body.cases(cases) {
                    queue.push(tgt);
                }
            }
            _ => {}
        }
    }
    None
}

enum RegionEnd {
    Returned(AlgExprId),
    ReachedJoin,
}

fn lift_region<A: Allocator + Copy, R: MathResolver>(
    body: Verified<'_>,
    f64_ty: PTy,
    math: &R,
    graph: &mut ExprGraph<A>,
    vars: &mut OwnVec<VarId, A>,
    start: Block,
    stop_at: Option<Block>,
    state: &mut LiftState<A>,
    depth: u32,
    alloc: A,
) -> Result<RegionEnd, BodyAlgebraError> {
    let max_steps = body.blocks().saturating_mul(8).clamp(body.blocks(), MAX_UNROLL_STEPS);
    let mut cur = start;
    let mut steps = 0u32;
    loop {
        if stop_at == Some(cur) {
            return Ok(RegionEnd::ReachedJoin);
        }
        if steps >= max_steps {
            return Err(BodyAlgebraError::CycleOrStepLimit { block: cur, steps });
        }
        steps += 1;

        let brow = body.block(cur);
        for s_idx in brow.stmts.start..brow.stmts.end {
            let stmt = Stmt(s_idx);
            match body.stmt(stmt).kind {
                StmtKind::StorageLive(_) | StmtKind::StorageDead(_) => {}
                StmtKind::Assign { dest, value } => {
                    let target = resolve_place(body, cur, stmt, dest, &state.int_env)?;
                    match (target, value) {
                        (PlaceTarget::Bare(local), Value::Use(op)) => {
                            if let Some(iv) = try_int_operand(body, f64_ty, op, &state.int_env) {
                                state.set_int(local, iv);
                            } else {
                                let expr = lift_scalar_operand(
                                    body, f64_ty, cur, stmt, op, state, vars, graph, alloc,
                                )?;
                                state.set_scalar(local, expr);
                            }
                        }
                        (PlaceTarget::Elem(local, idx), Value::Use(op)) => {
                            let expr = lift_scalar_operand(
                                body, f64_ty, cur, stmt, op, state, vars, graph, alloc,
                            )?;
                            state.set_elem(local, idx, expr);
                        }
                        (PlaceTarget::Bare(local), Value::Aggregate { kind: AggregateKind::Array | AggregateKind::Tuple, operands, .. }) => {
                            if operands.len() as usize > MAX_ARRAY_ELEMS {
                                return Err(BodyAlgebraError::UnsupportedValue { block: cur, stmt });
                            }
                            let mut slots: ElemSlots = [None; MAX_ARRAY_ELEMS];
                            for (k, op) in body.ops(operands).enumerate() {
                                slots[k] = Some(lift_scalar_operand(
                                    body, f64_ty, cur, stmt, op, state, vars, graph, alloc,
                                )?);
                            }
                            state.elem_env[local.index()] = Some(slots);
                            state.env[local.index()] = None;
                            state.int_env[local.index()] = None;
                            state.cmp_env[local.index()] = None;
                        }
                        (PlaceTarget::Bare(local), Value::Unary { op: UnOp::Neg, operand }) => {
                            if let Some(iv) = try_int_operand(body, f64_ty, operand, &state.int_env) {
                                state.set_int(local, iv.wrapping_neg());
                            } else {
                                let a = lift_scalar_operand(
                                    body, f64_ty, cur, stmt, operand, state, vars, graph, alloc,
                                )?;
                                state.set_scalar(local, graph.neg(a));
                            }
                        }
                        (PlaceTarget::Bare(local), Value::Binary { op, lhs, rhs }) => {
                            if let (Some(ia), Some(ib)) = (
                                try_int_operand(body, f64_ty, lhs, &state.int_env),
                                try_int_operand(body, f64_ty, rhs, &state.int_env),
                            ) {
                                let res = match op {
                                    BinOp::Add => ia.wrapping_add(ib),
                                    BinOp::Sub => ia.wrapping_sub(ib),
                                    BinOp::Mul => ia.wrapping_mul(ib),
                                    BinOp::Lt => i64::from(ia < ib),
                                    BinOp::Le => i64::from(ia <= ib),
                                    BinOp::Gt => i64::from(ia > ib),
                                    BinOp::Ge => i64::from(ia >= ib),
                                    BinOp::Eq => i64::from(ia == ib),
                                    BinOp::Ne => i64::from(ia != ib),
                                    _ => return Err(BodyAlgebraError::UnsupportedValue { block: cur, stmt }),
                                };
                                state.set_int(local, res);
                            } else {
                                let a = lift_scalar_operand(
                                    body, f64_ty, cur, stmt, lhs, state, vars, graph, alloc,
                                )?;
                                let b = lift_scalar_operand(
                                    body, f64_ty, cur, stmt, rhs, state, vars, graph, alloc,
                                )?;
                                match op {
                                    BinOp::Add => state.set_scalar(local, graph.add(a, b)),
                                    BinOp::Sub => state.set_scalar(local, graph.sub(a, b)),
                                    BinOp::Mul => state.set_scalar(local, graph.mul(a, b)),
                                    BinOp::Div => state.set_scalar(local, graph.div(a, b)),
                                    // Comparisons produce a signed condition expression (> 0 when true).
                                    BinOp::Gt | BinOp::Ge => {
                                        state.bool_ty.get_or_insert(body.local(local).ty);
                                        state.set_cmp(local, graph.sub(a, b));
                                    }
                                    BinOp::Lt | BinOp::Le => {
                                        state.bool_ty.get_or_insert(body.local(local).ty);
                                        state.set_cmp(local, graph.sub(b, a));
                                    }
                                    _ => return Err(BodyAlgebraError::UnsupportedValue { block: cur, stmt }),
                                }
                            }
                        }
                        _ => return Err(BodyAlgebraError::UnsupportedValue { block: cur, stmt }),
                    }
                }
                StmtKind::Goto(next) => {
                    cur = next;
                    break;
                }
                StmtKind::Switch { on, cases, otherwise } => {
                    // 1. Constant-trip-count loop / static integer branch: take the known target!
                    if let Some(iv) = try_int_operand(body, f64_ty, on, &state.int_env) {
                        let discr = iv as u64 as u128;
                        let mut target = otherwise;
                        for (val, blk) in body.cases(cases) {
                            if val == discr {
                                target = blk;
                                break;
                            }
                        }
                        cur = target;
                        break;
                    }
                    // 2. Dynamic float comparison branch: lift diamond / early-return into Op::Select!
                    if depth >= MAX_SWITCH_DEPTH || cases.len() != 1 {
                        return Err(BodyAlgebraError::UnsupportedStmt { block: cur, stmt });
                    }
                    let Some((case_val, bb_then)) = body.cases(cases).next() else {
                        return Err(BodyAlgebraError::UnsupportedStmt { block: cur, stmt });
                    };
                    if case_val != 1 {
                        return Err(BodyAlgebraError::UnsupportedStmt { block: cur, stmt });
                    }
                    let cond_expr = match on {
                        Operand::Copy(p) | Operand::Move(p) => {
                            let loc = bare_local(body, cur, stmt, p)?;
                            state
                                .cmp_env
                                .get(loc.index())
                                .copied()
                                .flatten()
                                .ok_or(BodyAlgebraError::UnsupportedStmt { block: cur, stmt })?
                        }
                        _ => return Err(BodyAlgebraError::UnsupportedStmt { block: cur, stmt }),
                    };
                    let mut st_then = state.snapshot(alloc);
                    let mut st_else = state.snapshot(alloc);

                    if let Some(bb_join) = find_join_block(body, bb_then, otherwise, alloc) {
                        let r_then = lift_region(
                            body, f64_ty, math, graph, vars, bb_then, Some(bb_join), &mut st_then,
                            depth + 1, alloc,
                        )?;
                        let r_else = lift_region(
                            body, f64_ty, math, graph, vars, otherwise, Some(bb_join), &mut st_else,
                            depth + 1, alloc,
                        )?;
                        match (r_then, r_else) {
                            (RegionEnd::ReachedJoin, RegionEnd::ReachedJoin) => {
                                for i in 0..state.env.len() {
                                    if st_then.env[i] != st_else.env[i] {
                                        state.env[i] = match (st_then.env[i], st_else.env[i]) {
                                            (Some(t), Some(e)) => Some(graph.select(cond_expr, t, e)),
                                            (Some(t), None) => Some(t),
                                            (None, Some(e)) => Some(e),
                                            (None, None) => None,
                                        };
                                    }
                                }
                                cur = bb_join;
                                break;
                            }
                            _ => return Err(BodyAlgebraError::UnsupportedStmt { block: cur, stmt }),
                        }
                    } else {
                        let r_then = lift_region(
                            body, f64_ty, math, graph, vars, bb_then, None, &mut st_then,
                            depth + 1, alloc,
                        )?;
                        let r_else = lift_region(
                            body, f64_ty, math, graph, vars, otherwise, None, &mut st_else,
                            depth + 1, alloc,
                        )?;
                        match (r_then, r_else) {
                            (RegionEnd::Returned(t), RegionEnd::Returned(e)) => {
                                return Ok(RegionEnd::Returned(graph.select(cond_expr, t, e)));
                            }
                            _ => return Err(BodyAlgebraError::UnsupportedStmt { block: cur, stmt }),
                        }
                    }
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
                    let mut lifted_args: [AlgExprId; 3] = [AlgExprId(0); 3];
                    for (k, op) in body.ops(args).enumerate() {
                        lifted_args[k] = lift_scalar_operand(
                            body, f64_ty, cur, stmt, op, state, vars, graph, alloc,
                        )?;
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
                        MathOp::Fma => graph.fma(lifted_args[0], lifted_args[1], lifted_args[2]),
                    };
                    match resolve_place(body, cur, stmt, dest_place, &state.int_env)? {
                        PlaceTarget::Bare(local) => state.set_scalar(local, res_expr),
                        PlaceTarget::Elem(local, idx) => state.set_elem(local, idx, res_expr),
                    }
                    cur = next_block;
                    break;
                }
                StmtKind::Return => {
                    let result = state.env[0].ok_or(BodyAlgebraError::MissingReturnValue { block: cur })?;
                    return Ok(RegionEnd::Returned(result));
                }
                _ => return Err(BodyAlgebraError::UnsupportedStmt { block: cur, stmt }),
            }
        }
    }
}

/// Lift a verified pure numeric `f64` [`blaze_body::Body`] — including comparison `Switch`
/// diamond branches, nested `if/else` returns, constant-trip-count `for i in 0..K` loops, and
/// fixed-size `[f64; N]` / tuple element indexing — into an [`ExprGraph`].
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
    let mut state = LiftState::new_in(nlocals, alloc);

    // Bind scalar `f64` argument locals eagerly; aggregate (`[f64; N]` / tuple) args bind lazily per element.
    const ARG_NAMES: [&str; 8] = ["x", "y", "z", "w", "a", "b", "c", "d"];
    for i in 0..nargs {
        let local = Local(i + 1);
        if body.local(local).ty == f64_ty {
            let name = ARG_NAMES.get(i as usize).copied().unwrap_or("v");
            let (vid, eid) = if (i as usize) < ARG_NAMES.len() {
                graph.var(name)
            } else {
                let mut buf = blaze_own::String::new_in(alloc);
                let _ = core::fmt::Write::write_fmt(&mut buf, format_args!("arg{i}"));
                graph.var(buf.as_str())
            };
            vars.push(vid);
            state.env[local.index()] = Some(eid);
        }
    }

    match lift_region(
        body, f64_ty, math, &mut graph, &mut vars, Block(0), None, &mut state, 0, alloc,
    )? {
        RegionEnd::Returned(result) => {
            Ok(LiftedBody { graph, vars, result, f64_ty, bool_ty: state.bool_ty })
        }
        RegionEnd::ReachedJoin => Err(BodyAlgebraError::MissingReturnValue { block: Block(0) }),
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
        Op::Select(c, t, e) | Op::Fma(c, t, e) => {
            collect_postorder(g, c, visited, order)?;
            collect_postorder(g, t, visited, order)?;
            collect_postorder(g, e, visited, order)?;
        }
    }
    order.push(id);
    Ok(())
}

/// Splice any [`ExprId`](AlgExprId) from [`ExprGraph`] back into a fresh, CSE-shared, verified
/// [`blaze_body::Body`]. Every shared DAG node is computed once into a temporary local; math
/// operations emit real [`StmtKind::Call`] terminators with continuation blocks, and
/// [`Op::Select`] emits real [`BinOp::Gt`] + [`StmtKind::Switch`] diamond CFGs.
pub fn splice_body<A: Allocator + Copy, E: MathEmitter>(
    g: &ExprGraph<A>,
    root: AlgExprId,
    vars: &[VarId],
    tys: ScalarTys,
    math: &E,
    alloc: A,
) -> Result<Body<A>, BodyAlgebraError> {
    splice_body_impl(g, &[root], vars, tys.f64_ty, Some(tys.bool_ty), None, math, alloc)
}

/// Splice multiple [`ExprId`](AlgExprId) roots (e.g. `[f, ∂f/∂x, ∂f/∂y]`) into a **single**
/// verified [`blaze_body::Body`] returning `[f64; K]` via `Value::Aggregate { kind: Array, .. }`,
/// sharing every CSE temporary local across all outputs!
pub fn splice_multi_body<A: Allocator + Copy, E: MathEmitter>(
    g: &ExprGraph<A>,
    roots: &[AlgExprId],
    vars: &[VarId],
    tys: ScalarTys,
    array_ty: PTy,
    math: &E,
    alloc: A,
) -> Result<Body<A>, BodyAlgebraError> {
    splice_body_impl(g, roots, vars, tys.f64_ty, Some(tys.bool_ty), Some(array_ty), math, alloc)
}

fn splice_body_impl<A: Allocator + Copy, E: MathEmitter>(
    g: &ExprGraph<A>,
    roots: &[AlgExprId],
    vars: &[VarId],
    f64_ty: PTy,
    bool_ty: Option<PTy>,
    array_ty: Option<PTy>,
    math: &E,
    alloc: A,
) -> Result<Body<A>, BodyAlgebraError> {
    let n = g.len();
    for &root in roots {
        if (root.0 as usize) >= n {
            return Err(BodyAlgebraError::Algebra(AlgebraError::InvalidExpr {
                expr: root,
                node_count: n,
            }));
        }
    }

    let mut visited: OwnVec<bool, A> = OwnVec::with_capacity_in(n, alloc);
    for _ in 0..n {
        visited.push(false);
    }
    let mut order: OwnVec<AlgExprId, A> = OwnVec::new_in(alloc);
    for &root in roots {
        collect_postorder(g, root, &mut visited, &mut order)?;
    }

    let mut b = BodyBuilder::new_in(alloc);
    let ret_ty = array_ty.unwrap_or(f64_ty);
    let ret_local = b.new_local(LocalDecl { ty: ret_ty, kind: LocalKind::Return, mutbl: Mutbl::Mut });
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
    let mut alloc_temp_ty = |b: &mut BodyBuilder<A>, ty: PTy, mutbl: Mutbl| -> Place {
        let l = b.new_local(LocalDecl {
            ty,
            kind: LocalKind::Temp(MirExprId(next_temp_id), Role::Value),
            mutbl,
        });
        next_temp_id += 1;
        b.local_place(l)
    };

    let emit_math_call = |b: &mut BodyBuilder<A>,
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

    let single_root = if array_ty.is_none() { roots.first().copied() } else { None };

    for &id in order.iter() {
        let is_single_root = single_root == Some(id);
        let op = g.node(id)?;
        match op {
            Op::Const(bits) => {
                let c = b.new_const(ConstDecl {
                    kind: ConstKind::Scalar { bits: u128::from(bits) },
                    ty: f64_ty,
                });
                let operand = Operand::Const(c);
                node_ops[id.0 as usize] = Some(operand);
                if is_single_root {
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
                if is_single_root {
                    b.assign(ret_place, Value::Use(operand), span);
                }
            }
            Op::Neg(a) => {
                let oa = node_ops[a.0 as usize].expect("postorder visited child");
                let dest = if is_single_root { ret_place } else { alloc_temp_ty(&mut b, f64_ty, Mutbl::Not) };
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
                let dest = if is_single_root { ret_place } else { alloc_temp_ty(&mut b, f64_ty, Mutbl::Not) };
                b.assign(dest, Value::Binary { op: bin, lhs: oa, rhs: ob }, span);
                node_ops[id.0 as usize] = Some(Operand::Copy(dest));
            }
            Op::Pow(a, b_id) => {
                let oa = node_ops[a.0 as usize].expect("postorder visited base");
                let ob = node_ops[b_id.0 as usize].expect("postorder visited exp");
                let dest = if is_single_root { ret_place } else { alloc_temp_ty(&mut b, f64_ty, Mutbl::Not) };
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
                let dest = if is_single_root { ret_place } else { alloc_temp_ty(&mut b, f64_ty, Mutbl::Not) };
                emit_math_call(&mut b, mop, &[oa], dest, &mut cur_block)?;
                node_ops[id.0 as usize] = Some(Operand::Copy(dest));
            }
            Op::Select(c_id, t_id, e_id) => {
                let bty = bool_ty.ok_or(BodyAlgebraError::MissingBoolType)?;
                let oc = node_ops[c_id.0 as usize].expect("postorder visited cond");
                let ot = node_ops[t_id.0 as usize].expect("postorder visited then");
                let oe = node_ops[e_id.0 as usize].expect("postorder visited else");
                let zero_c = b.new_const(ConstDecl {
                    kind: ConstKind::Scalar { bits: u128::from(0.0_f64.to_bits()) },
                    ty: f64_ty,
                });
                let cmp_place = alloc_temp_ty(&mut b, bty, Mutbl::Not);
                let dest =
                    if is_single_root { ret_place } else { alloc_temp_ty(&mut b, f64_ty, Mutbl::Mut) };
                b.assign(
                    cmp_place,
                    Value::Binary { op: BinOp::Gt, lhs: oc, rhs: Operand::Const(zero_c) },
                    span,
                );
                let bb_then = b.new_block(BlockKind::Normal);
                let bb_else = b.new_block(BlockKind::Normal);
                let bb_join = b.new_block(BlockKind::Normal);
                let cases = b.cases(&[(1u128, bb_then)]);
                b.terminate(
                    StmtKind::Switch { on: Operand::Copy(cmp_place), cases, otherwise: bb_else },
                    span,
                );
                b.start(bb_then);
                b.assign(dest, Value::Use(ot), span);
                b.terminate(StmtKind::Goto(bb_join), span);
                b.start(bb_else);
                b.assign(dest, Value::Use(oe), span);
                b.terminate(StmtKind::Goto(bb_join), span);
                b.start(bb_join);
                cur_block = bb_join;
                node_ops[id.0 as usize] = Some(Operand::Copy(dest));
            }
            Op::Fma(a, b_id, c_id) => {
                let oa = node_ops[a.0 as usize].expect("postorder visited fma a");
                let ob = node_ops[b_id.0 as usize].expect("postorder visited fma b");
                let oc = node_ops[c_id.0 as usize].expect("postorder visited fma c");
                let dest = if is_single_root { ret_place } else { alloc_temp_ty(&mut b, f64_ty, Mutbl::Not) };
                emit_math_call(&mut b, MathOp::Fma, &[oa, ob, oc], dest, &mut cur_block)?;
                node_ops[id.0 as usize] = Some(Operand::Copy(dest));
            }
        }
    }

    if let Some(arr_ty) = array_ty {
        let mut elem_ops: OwnVec<Operand, A> = OwnVec::with_capacity_in(roots.len(), alloc);
        for &r in roots {
            elem_ops.push(node_ops[r.0 as usize].expect("postorder visited root"));
        }
        let ops = b.operands(elem_ops.as_slice());
        b.assign(
            ret_place,
            Value::Aggregate { kind: AggregateKind::Array, ty: arr_ty, operands: ops },
            span,
        );
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
    splice_body_impl(&lifted.graph, &[target], &lifted.vars, lifted.f64_ty, lifted.bool_ty, None, emit, alloc)
}

/// Unified MIR compiler pass selector for the MIR pipeline (`blaze-body-lower` → `blaze-algebra-body` → `blaze-body-build`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BodyTransform {
    /// Simplify the MIR body under [`SimplifyOpts`] (e.g. `IeeeExact`, `FastMath`, `CpuFma`).
    Simplify(SimplifyOpts),
    /// Differentiate the MIR body with respect to argument `arg`.
    Diff { arg: u32 },
    /// Compile `[f, ∂f/∂x_0, ..., ∂f/∂x_{N-1}]` into one fused CSE MIR body returning `array_ty`.
    ValueAndGrad { array_ty: PTy },
    /// Compile the O(|V|) Hessian-Vector Product `H(f)(x) · v` into one `2*N`-arg CSE MIR body.
    Hvp { array_ty: PTy },
}

/// Single compiler pass entry point running a [`BodyTransform`] over a verified [`blaze_body::Body`].
pub fn run_body_transform<A: Allocator + Copy, R: MathResolver, E: MathEmitter>(
    body: Verified<'_>,
    transform: BodyTransform,
    bounds: Option<&[Interval]>,
    resolve: &R,
    emit: &E,
    alloc: A,
) -> Result<Body<A>, BodyAlgebraError> {
    match transform {
        BodyTransform::Simplify(opts) => simplify_body_with(body, opts, bounds, resolve, emit, alloc),
        BodyTransform::Diff { arg } => diff_body(body, arg, resolve, emit, alloc),
        BodyTransform::ValueAndGrad { array_ty } => {
            value_and_grad_body(body, array_ty, resolve, emit, alloc)
        }
        BodyTransform::Hvp { array_ty } => hvp_body(body, array_ty, resolve, emit, alloc),
    }
}

/// End-to-end MIR → MIR algebraic simplification pass (exact polynomial cancellation + e-graph):
/// `Verified<Body>` → [`lift_body`] → `simplify_poly_expr` + `simplify` → [`splice_body`].
pub fn simplify_body<A: Allocator + Copy, R: MathResolver, E: MathEmitter>(
    body: Verified<'_>,
    resolve: &R,
    emit: &E,
    alloc: A,
) -> Result<Body<A>, BodyAlgebraError> {
    simplify_body_with(body, SimplifyOpts::default(), None, resolve, emit, alloc)
}

/// Configurable MIR → MIR algebraic simplification pass supporting [`RewriteMode::IeeeExact`] vs
/// [`RewriteMode::FastMath`], target [`CostModel`](blaze_algebra::CostModel) (including FMA
/// fusion), and optional sound [`Interval`] dead-branch pruning before e-graph saturation.
pub fn simplify_body_with<A: Allocator + Copy, R: MathResolver, E: MathEmitter>(
    body: Verified<'_>,
    opts: SimplifyOpts,
    bounds: Option<&[Interval]>,
    resolve: &R,
    emit: &E,
    alloc: A,
) -> Result<Body<A>, BodyAlgebraError> {
    let mut lifted = lift_body(body, resolve, alloc)?;
    let mut s = lifted.result;
    if let Some(var_bounds) = bounds {
        s = prune_branches(&mut lifted.graph, s, var_bounds)?;
    }
    if opts.mode == RewriteMode::FastMath {
        s = simplify_poly_expr(&mut lifted.graph, s)?;
    }
    let (s, _) = simplify_with(&mut lifted.graph, s, opts)?;
    splice_body_impl(&lifted.graph, &[s], &lifted.vars, lifted.f64_ty, lifted.bool_ty, None, emit, alloc)
}

/// End-to-end MIR → MIR O(|V|) Hessian-Vector Product pass: compiles a verified scalar function
/// `f(x_0, ..., x_{N-1}) -> f64` into a **single** verified `2*N`-argument [`Body`]
/// `hvp(x_0, ..., x_{N-1}, v_0, ..., v_{N-1}) -> [f64; N]` computing `H(f)(x) · v` via forward
/// tangent + reverse adjoint sweeps with one shared CSE MIR schedule!
pub fn hvp_body<A: Allocator + Copy, R: MathResolver, E: MathEmitter>(
    body: Verified<'_>,
    array_ty: PTy,
    resolve: &R,
    emit: &E,
    alloc: A,
) -> Result<Body<A>, BodyAlgebraError> {
    let mut lifted = lift_body(body, resolve, alloc)?;
    let n = lifted.vars.len();
    let mut dir_vars = OwnVec::with_capacity_in(n, alloc);
    let mut dir_exprs = OwnVec::with_capacity_in(n, alloc);
    let mut all_vars = OwnVec::with_capacity_in(2 * n, alloc);
    for &v in lifted.vars.iter() {
        all_vars.push(v);
    }
    for i in 0..n {
        let mut buf = blaze_own::String::new_in(alloc);
        let _ = core::fmt::Write::write_fmt(&mut buf, format_args!("v{i}"));
        let (vid, eid) = lifted.graph.var(buf.as_str());
        dir_vars.push(vid);
        dir_exprs.push(eid);
        all_vars.push(vid);
    }
    let hv = hvp(&mut lifted.graph, lifted.result, &lifted.vars, &dir_exprs)?;
    let mut roots = OwnVec::with_capacity_in(hv.len(), alloc);
    for &h in hv.iter() {
        let h = simplify_poly_expr(&mut lifted.graph, h)?;
        let h = simplify(&mut lifted.graph, h)?;
        roots.push(h);
    }
    splice_body_impl(
        &lifted.graph,
        roots.as_slice(),
        all_vars.as_slice(),
        lifted.f64_ty,
        lifted.bool_ty,
        Some(array_ty),
        emit,
        alloc,
    )
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
        let mir = splice_body_impl(
            &lifted.graph,
            &[d],
            &lifted.vars,
            lifted.f64_ty,
            lifted.bool_ty,
            None,
            emit,
            alloc,
        )?;
        out.push(mir);
    }
    Ok(out)
}

/// End-to-end MIR → MIR fused value-and-gradient pass: compiles a verified scalar function
/// `f(x_0, ..., x_{N-1}) -> f64` into a **single** verified [`Body`] returning `[f64; 1 + N]`
/// (`[f, ∂f/∂x_0, ..., ∂f/∂x_{N-1}]`) where the primal value and every reverse-mode partial share
/// one topological CSE MIR schedule via [`splice_multi_body`].
pub fn value_and_grad_body<A: Allocator + Copy, R: MathResolver, E: MathEmitter>(
    body: Verified<'_>,
    array_ty: PTy,
    resolve: &R,
    emit: &E,
    alloc: A,
) -> Result<Body<A>, BodyAlgebraError> {
    let mut lifted = lift_body(body, resolve, alloc)?;
    let grads = grad_reverse(&mut lifted.graph, lifted.result, &lifted.vars)?;
    let mut roots = OwnVec::with_capacity_in(1 + grads.len(), alloc);
    let primal = simplify_poly_expr(&mut lifted.graph, lifted.result)?;
    let primal = simplify(&mut lifted.graph, primal)?;
    roots.push(primal);
    for &d in grads.iter() {
        let d = simplify_poly_expr(&mut lifted.graph, d)?;
        let d = simplify(&mut lifted.graph, d)?;
        roots.push(d);
    }
    splice_body_impl(
        &lifted.graph,
        roots.as_slice(),
        &lifted.vars,
        lifted.f64_ty,
        lifted.bool_ty,
        Some(array_ty),
        emit,
        alloc,
    )
}

enum MirVal<A: Allocator + Copy> {
    Scalar(f64),
    Bool(bool),
    Array(OwnVec<f64, A>),
}

fn eval_pure_impl<A: Allocator + Copy, R: MathResolver>(
    body: Verified<'_>,
    args_in: &[f64],
    math: &R,
    alloc: A,
) -> Result<MirVal<A>, BodyAlgebraError> {
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

    let mut env: OwnVec<Option<MirVal<A>>, A> = OwnVec::with_capacity_in(nlocals, alloc);
    for _ in 0..nlocals {
        env.push(None);
    }
    for (i, &v) in args_in.iter().take(nargs).enumerate() {
        env[i + 1] = Some(MirVal::Scalar(v));
    }

    let read_f64 = |block: Block,
                    stmt: Stmt,
                    op: Operand,
                    env: &[Option<MirVal<A>>]|
     -> Result<f64, BodyAlgebraError> {
        match op {
            Operand::Copy(p) | Operand::Move(p) => {
                let local = bare_local(body, block, stmt, p)?;
                match env.get(local.index()).and_then(Option::as_ref) {
                    Some(MirVal::Scalar(v)) => Ok(*v),
                    Some(_) => Err(BodyAlgebraError::UnsupportedValue { block, stmt }),
                    None => Err(BodyAlgebraError::UnassignedLocal { block, stmt, local }),
                }
            }
            Operand::Const(c) => match body.konst(c).kind {
                ConstKind::Scalar { bits } => Ok(f64::from_bits(bits as u64)),
                _ => Err(BodyAlgebraError::UnsupportedConst { block, stmt }),
            },
        }
    };

    let read_switch_val = |block: Block,
                           stmt: Stmt,
                           op: Operand,
                           env: &[Option<MirVal<A>>]|
     -> Result<u128, BodyAlgebraError> {
        match op {
            Operand::Copy(p) | Operand::Move(p) => {
                let local = bare_local(body, block, stmt, p)?;
                match env.get(local.index()).and_then(Option::as_ref) {
                    Some(MirVal::Bool(b)) => Ok(u128::from(*b)),
                    Some(_) => Err(BodyAlgebraError::UnsupportedValue { block, stmt }),
                    None => Err(BodyAlgebraError::UnassignedLocal { block, stmt, local }),
                }
            }
            Operand::Const(c) => match body.konst(c).kind {
                ConstKind::Scalar { bits } => Ok(bits),
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
                        Value::Use(op) => MirVal::Scalar(read_f64(cur, stmt, op, &env)?),
                        Value::Unary { op: UnOp::Neg, operand } => {
                            MirVal::Scalar(-read_f64(cur, stmt, operand, &env)?)
                        }
                        Value::Binary { op, lhs, rhs } => {
                            let a = read_f64(cur, stmt, lhs, &env)?;
                            let b = read_f64(cur, stmt, rhs, &env)?;
                            match op {
                                BinOp::Add => MirVal::Scalar(a + b),
                                BinOp::Sub => MirVal::Scalar(a - b),
                                BinOp::Mul => MirVal::Scalar(a * b),
                                BinOp::Div => MirVal::Scalar(a / b),
                                BinOp::Gt => MirVal::Bool(a > b),
                                BinOp::Lt => MirVal::Bool(a < b),
                                BinOp::Ge => MirVal::Bool(a >= b),
                                BinOp::Le => MirVal::Bool(a <= b),
                                BinOp::Eq => MirVal::Bool(a == b),
                                BinOp::Ne => MirVal::Bool(a != b),
                                _ => {
                                    return Err(BodyAlgebraError::UnsupportedValue {
                                        block: cur,
                                        stmt,
                                    });
                                }
                            }
                        }
                        Value::Aggregate { kind: AggregateKind::Array, operands, .. } => {
                            let mut elems = OwnVec::with_capacity_in(operands.len() as usize, alloc);
                            for op in body.ops(operands) {
                                elems.push(read_f64(cur, stmt, op, &env)?);
                            }
                            MirVal::Array(elems)
                        }
                        _ => return Err(BodyAlgebraError::UnsupportedValue { block: cur, stmt }),
                    };
                    env[local.index()] = Some(val);
                }
                StmtKind::Goto(next) => {
                    cur = next;
                    break;
                }
                StmtKind::Switch { on, cases, otherwise } => {
                    let discr = read_switch_val(cur, stmt, on, &env)?;
                    let mut target = otherwise;
                    for (val, blk) in body.cases(cases) {
                        if val == discr {
                            target = blk;
                            break;
                        }
                    }
                    cur = target;
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
                    let mut vals = [0.0_f64; 3];
                    for (k, op) in body.ops(args).enumerate() {
                        vals[k] = read_f64(cur, stmt, op, &env)?;
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
                        MathOp::Fma => blaze_math::mul_add(vals[0], vals[1], vals[2]),
                    };
                    let local = bare_local(body, cur, stmt, dest_place)?;
                    env[local.index()] = Some(MirVal::Scalar(res));
                    cur = next_block;
                    break;
                }
                StmtKind::Return => {
                    return env[0].take().ok_or(BodyAlgebraError::MissingReturnValue { block: cur });
                }
                _ => return Err(BodyAlgebraError::UnsupportedStmt { block: cur, stmt }),
            }
        }
    }
}

/// Deterministic sans-IO scalar `f64` evaluator over any verified acyclic pure numeric
/// [`blaze_body::Body`] (including `Switch` diamond CFGs from `Op::Select`), using `blaze_math`
/// for transcendental ops.
pub fn eval_pure_body<A: Allocator + Copy, R: MathResolver>(
    body: Verified<'_>,
    args_in: &[f64],
    math: &R,
    alloc: A,
) -> Result<f64, BodyAlgebraError> {
    match eval_pure_impl(body, args_in, math, alloc)? {
        MirVal::Scalar(v) => Ok(v),
        _ => Err(BodyAlgebraError::MissingReturnValue { block: Block(0) }),
    }
}

/// Deterministic sans-IO multi-output evaluator for a verified [`blaze_body::Body`] produced by
/// [`splice_multi_body`] or [`value_and_grad_body`] returning `[f64; K]`.
pub fn eval_pure_multi_body<A: Allocator + Copy, R: MathResolver>(
    body: Verified<'_>,
    args_in: &[f64],
    math: &R,
    alloc: A,
) -> Result<OwnVec<f64, A>, BodyAlgebraError> {
    match eval_pure_impl(body, args_in, math, alloc)? {
        MirVal::Array(vs) => Ok(vs),
        MirVal::Scalar(v) => {
            let mut out = OwnVec::with_capacity_in(1, alloc);
            out.push(v);
            Ok(out)
        }
        MirVal::Bool(_) => Err(BodyAlgebraError::MissingReturnValue { block: Block(0) }),
    }
}

#[cfg(test)]
mod tests;
