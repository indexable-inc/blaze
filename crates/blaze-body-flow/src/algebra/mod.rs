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

/// Optional resolver supplying verified [`blaze_body::Body`]s for pure user-defined helper
/// functions called from a lifted MIR body, enabling bounded interprocedural inlining directly
/// into the shared [`ExprGraph`].
pub trait CalleeBodyResolver {
    /// Return a verified MIR body for `callee` if it is a pure helper eligible for inlining.
    fn resolve_body(&self, callee: Callee) -> Option<Verified<'_>>;
}

/// Default empty helper resolver (no interprocedural inlining).
#[derive(Clone, Copy, Default, Debug)]
pub struct NoHelperBodies;

impl CalleeBodyResolver for NoHelperBodies {
    #[inline]
    fn resolve_body(&self, _callee: Callee) -> Option<Verified<'_>> {
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
    /// Interprocedural helper inlining exceeded [`MAX_INLINE_DEPTH`] at `(block, stmt)`.
    InlineDepthLimit { block: Block, stmt: Stmt, depth: u32 },
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
            Self::InlineDepthLimit { block, stmt, depth } => {
                write!(f, "interprocedural helper inline depth limit ({depth}) reached at {block} {stmt}")
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

fn set_local<T>(
    env: &mut [Option<T>],
    local: Local,
    val: Option<T>,
    block: Block,
    stmt: Stmt,
) -> Result<(), BodyAlgebraError> {
    let slot =
        env.get_mut(local.index()).ok_or(BodyAlgebraError::UnassignedLocal { block, stmt, local })?;
    *slot = val;
    Ok(())
}

const MAX_SWITCH_DEPTH: u32 = 16;
const MAX_UNROLL_STEPS: u32 = 64;
const MAX_ARRAY_ELEMS: usize = 16;
const MAX_INLINE_DEPTH: u32 = 8;

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
        Self { env, cmp_env, int_env, elem_env }
    }

    fn snapshot(&self, alloc: A) -> Self {
        let mut env = OwnVec::with_capacity_in(self.env.len(), alloc);
        env.extend(self.env.iter().copied());
        let mut cmp_env = OwnVec::with_capacity_in(self.cmp_env.len(), alloc);
        cmp_env.extend(self.cmp_env.iter().copied());
        let mut int_env = OwnVec::with_capacity_in(self.int_env.len(), alloc);
        int_env.extend(self.int_env.iter().copied());
        let mut elem_env = OwnVec::with_capacity_in(self.elem_env.len(), alloc);
        elem_env.extend(self.elem_env.iter().copied());
        Self { env, cmp_env, int_env, elem_env }
    }

    fn set_scalar(
        &mut self,
        local: Local,
        expr: AlgExprId,
        block: Block,
        stmt: Stmt,
    ) -> Result<(), BodyAlgebraError> {
        set_local(&mut self.env, local, Some(expr), block, stmt)?;
        set_local(&mut self.cmp_env, local, None, block, stmt)?;
        set_local(&mut self.int_env, local, None, block, stmt)?;
        Ok(())
    }

    fn set_int(
        &mut self,
        local: Local,
        val: i64,
        block: Block,
        stmt: Stmt,
    ) -> Result<(), BodyAlgebraError> {
        set_local(&mut self.int_env, local, Some(val), block, stmt)?;
        set_local(&mut self.env, local, None, block, stmt)?;
        set_local(&mut self.cmp_env, local, None, block, stmt)?;
        Ok(())
    }

    fn set_cmp(
        &mut self,
        local: Local,
        cond: AlgExprId,
        block: Block,
        stmt: Stmt,
    ) -> Result<(), BodyAlgebraError> {
        set_local(&mut self.cmp_env, local, Some(cond), block, stmt)?;
        set_local(&mut self.env, local, None, block, stmt)?;
        set_local(&mut self.int_env, local, None, block, stmt)?;
        Ok(())
    }

    fn set_elem(
        &mut self,
        local: Local,
        idx: usize,
        expr: AlgExprId,
        block: Block,
        stmt: Stmt,
    ) -> Result<(), BodyAlgebraError> {
        let slots = self
            .elem_env
            .get_mut(local.index())
            .ok_or(BodyAlgebraError::UnassignedLocal { block, stmt, local })?
            .get_or_insert([None; MAX_ARRAY_ELEMS]);
        let elem =
            slots.get_mut(idx).ok_or(BodyAlgebraError::UnsupportedValue { block, stmt })?;
        *elem = Some(expr);
        Ok(())
    }

    fn set_elems(
        &mut self,
        local: Local,
        slots: ElemSlots,
        block: Block,
        stmt: Stmt,
    ) -> Result<(), BodyAlgebraError> {
        set_local(&mut self.elem_env, local, Some(slots), block, stmt)?;
        set_local(&mut self.env, local, None, block, stmt)?;
        set_local(&mut self.int_env, local, None, block, stmt)?;
        set_local(&mut self.cmp_env, local, None, block, stmt)?;
        Ok(())
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
                ConstKind::Scalar { bits } => u64::try_from(bits).ok().map(u64::cast_signed),
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
                if let Some(eid) = state
                    .elem_env
                    .get(local.index())
                    .copied()
                    .flatten()
                    .and_then(|s| s.get(idx).copied().flatten())
                {
                    return Ok(eid);
                }
                // Lazily bind elements of array/tuple argument locals (`Local(1..=args)`).
                if local.0 >= 1 && local.0 <= body.args() {
                    let mut buf = blaze_own::String::new_in(alloc);
                    let _ = core::fmt::Write::write_fmt(
                        &mut buf,
                        format_args!("a{}_{idx}", local.0.saturating_sub(1)),
                    );
                    let (vid, eid) = g.var(buf.as_str());
                    vars.push(vid);
                    state.set_elem(local, idx, eid, block, stmt)?;
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
                ConstKind::Scalar { bits } => match u64::try_from(bits) {
                    Ok(b) => Ok(g.lit(f64::from_bits(b))),
                    Err(_) => Err(BodyAlgebraError::UnsupportedConst { block, stmt }),
                },
                _ => Err(BodyAlgebraError::UnsupportedConst { block, stmt }),
            }
        }
    }
}

fn new_bool_vec<A: Allocator + Copy>(len: usize, alloc: A) -> OwnVec<bool, A> {
    let mut out = OwnVec::with_capacity_in(len, alloc);
    for _ in 0..len {
        out.push(false);
    }
    out
}

fn collect_reachable_blocks<A: Allocator + Copy>(
    body: Verified<'_>,
    start: Block,
    alloc: A,
) -> OwnVec<bool, A> {
    let nb = usize::try_from(body.blocks()).unwrap_or(0);
    let mut seen = new_bool_vec(nb, alloc);
    let mut stack = OwnVec::new_in(alloc);
    stack.push(start);
    while let Some(b) = stack.pop() {
        let Some(slot) = seen.get_mut(b.index()) else {
            continue;
        };
        if *slot {
            continue;
        }
        *slot = true;
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
    let nb = usize::try_from(body.blocks()).unwrap_or(0);
    let mut visited = new_bool_vec(nb, alloc);
    let mut queue = OwnVec::new_in(alloc);
    queue.push(bb_else);
    let mut head = 0usize;
    while let Some(&cur) = queue.get(head) {
        head += 1;
        let Some(vis_slot) = visited.get_mut(cur.index()) else {
            continue;
        };
        if *vis_slot {
            continue;
        }
        *vis_slot = true;
        if reach_then.get(cur.index()).copied().unwrap_or(false) {
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

struct RegionState<'b, A: Allocator + Copy> {
    body: Verified<'b>,
    f64_ty: PTy,
    max_steps: u32,
    cur: Block,
    stop_at: Option<Block>,
    state: LiftState<A>,
    steps: u32,
    depth: u32,
    inline_depth: u32,
}

enum SwitchStage<A: Allocator + Copy> {
    Then {
        bb_else: Block,
        state_else: LiftState<A>,
    },
    Else {
        then_end: RegionEnd,
        state_then: LiftState<A>,
    },
}

struct SwitchFrame<'b, A: Allocator + Copy> {
    parent: RegionState<'b, A>,
    switch_block: Block,
    switch_stmt: Stmt,
    cond_expr: AlgExprId,
    join: Option<Block>,
    stage: SwitchStage<A>,
}

enum LiftFrame<'b, A: Allocator + Copy> {
    Switch(SwitchFrame<'b, A>),
    InlineCall {
        parent: RegionState<'b, A>,
        call_block: Block,
        call_stmt: Stmt,
        dest_place: Place,
        next_block: Block,
    },
}

fn lift_region<'b, A: Allocator + Copy, R: MathResolver, H: CalleeBodyResolver>(
    body: Verified<'b>,
    f64_ty: PTy,
    math: &R,
    helpers: &'b H,
    graph: &mut ExprGraph<A>,
    vars: &mut OwnVec<VarId, A>,
    start: Block,
    state: LiftState<A>,
    bool_ty: &mut Option<PTy>,
    alloc: A,
) -> Result<RegionEnd, BodyAlgebraError> {
    let max_steps = body.blocks().saturating_mul(8).clamp(body.blocks(), MAX_UNROLL_STEPS);

    let mut active = RegionState {
        body,
        f64_ty,
        max_steps,
        cur: start,
        stop_at: None,
        state,
        steps: 0,
        depth: 0,
        inline_depth: 0,
    };
    let mut frames: OwnVec<LiftFrame<'b, A>, A> = OwnVec::new_in(alloc);

    loop {
        let finished: Option<RegionEnd> = if active.stop_at == Some(active.cur) {
            Some(RegionEnd::ReachedJoin)
        } else {
            let body = active.body;
            let f64_ty = active.f64_ty;
            let cur = active.cur;
            if active.steps >= active.max_steps {
                return Err(BodyAlgebraError::CycleOrStepLimit { block: cur, steps: active.steps });
            }
            active.steps += 1;

            let mut block_end = None;
            let brow = body.block(cur);
            for s_idx in brow.stmts.start..brow.stmts.end {
                let stmt = Stmt(s_idx);
                match body.stmt(stmt).kind {
                    StmtKind::StorageLive(_) | StmtKind::StorageDead(_) | StmtKind::FakeRead(_) => {
                    }
                    StmtKind::Assign { dest, value } => {
                        let target = resolve_place(body, cur, stmt, dest, &active.state.int_env)?;
                        match (target, value) {
                            (PlaceTarget::Bare(local), Value::Use(op)) => {
                                let mut handled = false;
                                if let Operand::Copy(p) | Operand::Move(p) = op {
                                    let row = body.place(p);
                                    if row.depth == 0 && row.proj.is_none() {
                                        if let Some(cond) =
                                            active.state.cmp_env.get(row.local.index()).copied().flatten()
                                        {
                                            bool_ty.get_or_insert(body.local(local).ty);
                                            active.state.set_cmp(local, cond, cur, stmt)?;
                                            handled = true;
                                        } else if let Some(slots) = active
                                            .state
                                            .elem_env
                                            .get(row.local.index())
                                            .copied()
                                            .flatten()
                                        {
                                            active.state.set_elems(local, slots, cur, stmt)?;
                                            handled = true;
                                        }
                                    }
                                }
                                if !handled {
                                    if let Some(iv) =
                                        try_int_operand(body, f64_ty, op, &active.state.int_env)
                                    {
                                        active.state.set_int(local, iv, cur, stmt)?;
                                    } else {
                                        let expr = lift_scalar_operand(
                                            body,
                                            f64_ty,
                                            cur,
                                            stmt,
                                            op,
                                            &mut active.state,
                                            vars,
                                            graph,
                                            alloc,
                                        )?;
                                        active.state.set_scalar(local, expr, cur, stmt)?;
                                    }
                                }
                            }
                            (PlaceTarget::Elem(local, idx), Value::Use(op)) => {
                                let expr = lift_scalar_operand(
                                    body,
                                    f64_ty,
                                    cur,
                                    stmt,
                                    op,
                                    &mut active.state,
                                    vars,
                                    graph,
                                    alloc,
                                )?;
                                active.state.set_elem(local, idx, expr, cur, stmt)?;
                            }
                            (
                                PlaceTarget::Bare(local),
                                Value::Aggregate {
                                    kind: AggregateKind::Array | AggregateKind::Tuple | AggregateKind::Adt(0),
                                    operands,
                                    ..
                                },
                            ) => {
                                if usize::try_from(operands.len())
                                    .map_or(true, |len| len > MAX_ARRAY_ELEMS)
                                {
                                    return Err(BodyAlgebraError::UnsupportedValue {
                                        block: cur,
                                        stmt,
                                    });
                                }
                                let mut slots: ElemSlots = [None; MAX_ARRAY_ELEMS];
                                for (slot, op) in slots.iter_mut().zip(body.ops(operands)) {
                                    *slot = Some(lift_scalar_operand(
                                        body,
                                        f64_ty,
                                        cur,
                                        stmt,
                                        op,
                                        &mut active.state,
                                        vars,
                                        graph,
                                        alloc,
                                    )?);
                                }
                                active.state.set_elems(local, slots, cur, stmt)?;
                            }
                            (PlaceTarget::Bare(local), Value::Unary { op: UnOp::Neg, operand }) => {
                                if let Some(iv) =
                                    try_int_operand(body, f64_ty, operand, &active.state.int_env)
                                {
                                    active.state.set_int(local, iv.wrapping_neg(), cur, stmt)?;
                                } else {
                                    let a = lift_scalar_operand(
                                        body,
                                        f64_ty,
                                        cur,
                                        stmt,
                                        operand,
                                        &mut active.state,
                                        vars,
                                        graph,
                                        alloc,
                                    )?;
                                    active.state.set_scalar(local, graph.neg(a), cur, stmt)?;
                                }
                            }
                            (PlaceTarget::Bare(local), Value::Unary { op: UnOp::Not, operand }) => {
                                if let Some(iv) =
                                    try_int_operand(body, f64_ty, operand, &active.state.int_env)
                                {
                                    active.state.set_int(local, i64::from(iv == 0), cur, stmt)?;
                                } else if let Operand::Copy(p) | Operand::Move(p) = operand {
                                    let loc = bare_local(body, cur, stmt, p)?;
                                    let cond = active
                                        .state
                                        .cmp_env
                                        .get(loc.index())
                                        .copied()
                                        .flatten()
                                        .ok_or(BodyAlgebraError::UnsupportedValue {
                                            block: cur,
                                            stmt,
                                        })?;
                                    bool_ty.get_or_insert(body.local(local).ty);
                                    active.state.set_cmp(local, graph.neg(cond), cur, stmt)?;
                                } else {
                                    return Err(BodyAlgebraError::UnsupportedValue {
                                        block: cur,
                                        stmt,
                                    });
                                }
                            }
                            (PlaceTarget::Bare(local), Value::Binary { op, lhs, rhs }) => {
                                if let (Some(ia), Some(ib)) = (
                                    try_int_operand(body, f64_ty, lhs, &active.state.int_env),
                                    try_int_operand(body, f64_ty, rhs, &active.state.int_env),
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
                                        _ => {
                                            return Err(BodyAlgebraError::UnsupportedValue {
                                                block: cur,
                                                stmt,
                                            });
                                        }
                                    };
                                    active.state.set_int(local, res, cur, stmt)?;
                                } else {
                                    let a = lift_scalar_operand(
                                        body,
                                        f64_ty,
                                        cur,
                                        stmt,
                                        lhs,
                                        &mut active.state,
                                        vars,
                                        graph,
                                        alloc,
                                    )?;
                                    let b = lift_scalar_operand(
                                        body,
                                        f64_ty,
                                        cur,
                                        stmt,
                                        rhs,
                                        &mut active.state,
                                        vars,
                                        graph,
                                        alloc,
                                    )?;
                                    match op {
                                        BinOp::Add => {
                                            active.state.set_scalar(
                                                local,
                                                graph.add(a, b),
                                                cur,
                                                stmt,
                                            )?;
                                        }
                                        BinOp::Sub => {
                                            active.state.set_scalar(
                                                local,
                                                graph.sub(a, b),
                                                cur,
                                                stmt,
                                            )?;
                                        }
                                        BinOp::Mul => {
                                            active.state.set_scalar(
                                                local,
                                                graph.mul(a, b),
                                                cur,
                                                stmt,
                                            )?;
                                        }
                                        BinOp::Div => {
                                            active.state.set_scalar(
                                                local,
                                                graph.div(a, b),
                                                cur,
                                                stmt,
                                            )?;
                                        }
                                        // Comparisons produce a signed condition expression (> 0 when true).
                                        BinOp::Gt | BinOp::Ge => {
                                            bool_ty.get_or_insert(body.local(local).ty);
                                            active.state.set_cmp(
                                                local,
                                                graph.sub(a, b),
                                                cur,
                                                stmt,
                                            )?;
                                        }
                                        BinOp::Lt | BinOp::Le => {
                                            bool_ty.get_or_insert(body.local(local).ty);
                                            active.state.set_cmp(
                                                local,
                                                graph.sub(b, a),
                                                cur,
                                                stmt,
                                            )?;
                                        }
                                        _ => {
                                            return Err(BodyAlgebraError::UnsupportedValue {
                                                block: cur,
                                                stmt,
                                            });
                                        }
                                    }
                                }
                            }
                            _ => {
                                return Err(BodyAlgebraError::UnsupportedValue { block: cur, stmt });
                            }
                        }
                    }
                    StmtKind::Goto(next) => {
                        active.cur = next;
                        break;
                    }
                    StmtKind::Switch { on, cases, otherwise } => {
                        // 1. Constant-trip-count loop / static integer branch: take the known target!
                        if let Some(iv) = try_int_operand(body, f64_ty, on, &active.state.int_env) {
                            let discr = u128::from(iv.cast_unsigned());
                            let mut target = otherwise;
                            for (val, blk) in body.cases(cases) {
                                if val == discr {
                                    target = blk;
                                    break;
                                }
                            }
                            active.cur = target;
                            break;
                        }
                        // 2. Dynamic float comparison branch: lift diamond / early-return into Op::Select!
                        // Supports both `[(1, then)] else else_bb` (splicer) and `[(0, else_bb)] else then` (blaze-body-lower).
                        if active.depth >= MAX_SWITCH_DEPTH || cases.len() != 1 {
                            return Err(BodyAlgebraError::UnsupportedStmt { block: cur, stmt });
                        }
                        let Some((case_val, case_blk)) = body.cases(cases).next() else {
                            return Err(BodyAlgebraError::UnsupportedStmt { block: cur, stmt });
                        };
                        let (bb_then, bb_else) = match case_val {
                            1 => (case_blk, otherwise),
                            0 => (otherwise, case_blk),
                            _ => {
                                return Err(BodyAlgebraError::UnsupportedStmt { block: cur, stmt });
                            }
                        };
                        let cond_expr = match on {
                            Operand::Copy(p) | Operand::Move(p) => {
                                let loc = bare_local(body, cur, stmt, p)?;
                                active
                                    .state
                                    .cmp_env
                                    .get(loc.index())
                                    .copied()
                                    .flatten()
                                    .ok_or(BodyAlgebraError::UnsupportedStmt { block: cur, stmt })?
                            }
                            _ => {
                                return Err(BodyAlgebraError::UnsupportedStmt { block: cur, stmt });
                            }
                        };
                        let state_then = active.state.snapshot(alloc);
                        let state_else = active.state.snapshot(alloc);
                        let join = find_join_block(body, bb_then, bb_else, alloc);

                        let then_state = RegionState {
                            body: active.body,
                            f64_ty: active.f64_ty,
                            max_steps: active.max_steps,
                            cur: bb_then,
                            stop_at: join,
                            state: state_then,
                            steps: 0,
                            depth: active.depth + 1,
                            inline_depth: active.inline_depth,
                        };
                        let parent = core::mem::replace(&mut active, then_state);
                        frames.push(LiftFrame::Switch(SwitchFrame {
                            parent,
                            switch_block: cur,
                            switch_stmt: stmt,
                            cond_expr,
                            join,
                            stage: SwitchStage::Then { bb_else, state_else },
                        }));
                        break;
                    }
                    StmtKind::Call { callee, args, dest, target, .. } => {
                        let (Some(dest_place), Some(next_block)) = (dest, target) else {
                            return Err(BodyAlgebraError::UnsupportedCall { block: cur, stmt });
                        };
                        let callee_val = body.callee(callee);
                        if let Some(math_op) = math.classify(callee_val) {
                            if usize::try_from(args.len()) != Ok(math_op.arity()) {
                                return Err(BodyAlgebraError::UnsupportedCall { block: cur, stmt });
                            }
                            let mut lifted_args: [AlgExprId; 3] = [AlgExprId(0); 3];
                            for (slot, op) in lifted_args.iter_mut().zip(body.ops(args)) {
                                *slot = lift_scalar_operand(
                                    body,
                                    f64_ty,
                                    cur,
                                    stmt,
                                    op,
                                    &mut active.state,
                                    vars,
                                    graph,
                                    alloc,
                                )?;
                            }
                            let [a0, a1, a2] = lifted_args;
                            let res_expr = match math_op {
                                MathOp::Sin => graph.sin(a0),
                                MathOp::Cos => graph.cos(a0),
                                MathOp::Tan => graph.tan(a0),
                                MathOp::Exp => graph.exp(a0),
                                MathOp::Ln => graph.ln(a0),
                                MathOp::Sqrt => graph.sqrt(a0),
                                MathOp::Tanh => graph.tanh(a0),
                                MathOp::Abs => graph.abs(a0),
                                MathOp::Pow => graph.pow(a0, a1),
                                MathOp::Fma => graph.fma(a0, a1, a2),
                            };
                            match resolve_place(body, cur, stmt, dest_place, &active.state.int_env)?
                            {
                                PlaceTarget::Bare(local) => {
                                    active.state.set_scalar(local, res_expr, cur, stmt)?;
                                }
                                PlaceTarget::Elem(local, idx) => {
                                    active.state.set_elem(local, idx, res_expr, cur, stmt)?;
                                }
                            }
                            active.cur = next_block;
                            break;
                        } else if let Some(callee_body) = helpers.resolve_body(callee_val) {
                            if active.inline_depth >= MAX_INLINE_DEPTH {
                                return Err(BodyAlgebraError::InlineDepthLimit {
                                    block: cur,
                                    stmt,
                                    depth: active.inline_depth,
                                });
                            }
                            let cnlocals = usize::try_from(callee_body.locals()).unwrap_or(0);
                            if cnlocals == 0 || callee_body.args() != args.len() {
                                return Err(BodyAlgebraError::UnsupportedCall { block: cur, stmt });
                            }
                            let callee_f64_ty = callee_body.local(Local(0)).ty;
                            let mut callee_state = LiftState::new_in(cnlocals, alloc);
                            for (k, op) in body.ops(args).enumerate() {
                                let param_idx = u32::try_from(k)
                                    .ok()
                                    .and_then(|v| v.checked_add(1))
                                    .ok_or(BodyAlgebraError::UnsupportedCall {
                                        block: cur,
                                        stmt,
                                    })?;
                                let param_local = Local(param_idx);
                                let mut bound = false;
                                if let Operand::Copy(p) | Operand::Move(p) = op {
                                    let prow = body.place(p);
                                    if prow.depth == 0
                                        && prow.proj.is_none()
                                        && let Some(slots) = active
                                            .state
                                            .elem_env
                                            .get(prow.local.index())
                                            .copied()
                                            .flatten()
                                    {
                                        callee_state.set_elems(param_local, slots, cur, stmt)?;
                                        bound = true;
                                    }
                                }
                                if !bound {
                                    if let Some(iv) =
                                        try_int_operand(body, f64_ty, op, &active.state.int_env)
                                    {
                                        callee_state.set_int(param_local, iv, cur, stmt)?;
                                    } else {
                                        let expr = lift_scalar_operand(
                                            body,
                                            f64_ty,
                                            cur,
                                            stmt,
                                            op,
                                            &mut active.state,
                                            vars,
                                            graph,
                                            alloc,
                                        )?;
                                        callee_state.set_scalar(param_local, expr, cur, stmt)?;
                                    }
                                }
                            }
                            let callee_max_steps = callee_body
                                .blocks()
                                .saturating_mul(8)
                                .clamp(callee_body.blocks(), MAX_UNROLL_STEPS);
                            let callee_region = RegionState {
                                body: callee_body,
                                f64_ty: callee_f64_ty,
                                max_steps: callee_max_steps,
                                cur: Block(0),
                                stop_at: None,
                                state: callee_state,
                                steps: 0,
                                depth: active.depth,
                                inline_depth: active.inline_depth + 1,
                            };
                            let parent = core::mem::replace(&mut active, callee_region);
                            frames.push(LiftFrame::InlineCall {
                                parent,
                                call_block: cur,
                                call_stmt: stmt,
                                dest_place,
                                next_block,
                            });
                            break;
                        } else {
                            return Err(BodyAlgebraError::UnsupportedCall { block: cur, stmt });
                        }
                    }
                    StmtKind::Return => {
                        let result = active
                            .state
                            .env
                            .first()
                            .copied()
                            .flatten()
                            .ok_or(BodyAlgebraError::MissingReturnValue { block: cur })?;
                        block_end = Some(RegionEnd::Returned(result));
                        break;
                    }
                    _ => return Err(BodyAlgebraError::UnsupportedStmt { block: cur, stmt }),
                }
            }
            block_end
        };

        if let Some(mut finished_end) = finished {
            let mut finished_state = active.state;
            loop {
                let Some(frame) = frames.pop() else {
                    return Ok(finished_end);
                };
                match frame {
                    LiftFrame::Switch(frame) => match frame.stage {
                        SwitchStage::Then { bb_else, state_else } => {
                            let else_state = RegionState {
                                body: frame.parent.body,
                                f64_ty: frame.parent.f64_ty,
                                max_steps: frame.parent.max_steps,
                                cur: bb_else,
                                stop_at: frame.join,
                                state: state_else,
                                steps: 0,
                                depth: frame.parent.depth + 1,
                                inline_depth: frame.parent.inline_depth,
                            };
                            frames.push(LiftFrame::Switch(SwitchFrame {
                                parent: frame.parent,
                                switch_block: frame.switch_block,
                                switch_stmt: frame.switch_stmt,
                                cond_expr: frame.cond_expr,
                                join: frame.join,
                                stage: SwitchStage::Else {
                                    then_end: finished_end,
                                    state_then: finished_state,
                                },
                            }));
                            active = else_state;
                            break;
                        }
                        SwitchStage::Else { then_end, state_then } => {
                            let state_else = finished_state;
                            let else_end = finished_end;
                            if let Some(bb_join) = frame.join {
                                match (then_end, else_end) {
                                    (RegionEnd::ReachedJoin, RegionEnd::ReachedJoin) => {
                                        let mut parent = frame.parent;
                                        for (dst, (&t_val, &e_val)) in parent
                                            .state
                                            .env
                                            .iter_mut()
                                            .zip(state_then.env.iter().zip(state_else.env.iter()))
                                        {
                                            if t_val != e_val {
                                                *dst = match (t_val, e_val) {
                                                    (Some(t), Some(e)) => {
                                                        Some(graph.select(frame.cond_expr, t, e))
                                                    }
                                                    (Some(t), None) => Some(t),
                                                    (None, Some(e)) => Some(e),
                                                    (None, None) => None,
                                                };
                                            }
                                        }
                                        for (dst, (&t_arr, &e_arr)) in parent
                                            .state
                                            .elem_env
                                            .iter_mut()
                                            .zip(
                                                state_then
                                                    .elem_env
                                                    .iter()
                                                    .zip(state_else.elem_env.iter()),
                                            )
                                        {
                                            if t_arr != e_arr {
                                                *dst = match (t_arr, e_arr) {
                                                    (Some(ts), Some(es)) => {
                                                        let mut merged = [None; MAX_ARRAY_ELEMS];
                                                        for (m, (&tv, &ev)) in merged
                                                            .iter_mut()
                                                            .zip(ts.iter().zip(es.iter()))
                                                        {
                                                            *m = match (tv, ev) {
                                                                (Some(t), Some(e)) if t != e => {
                                                                    Some(graph.select(
                                                                        frame.cond_expr,
                                                                        t,
                                                                        e,
                                                                    ))
                                                                }
                                                                (Some(t), _) => Some(t),
                                                                (None, Some(e)) => Some(e),
                                                                (None, None) => None,
                                                            };
                                                        }
                                                        Some(merged)
                                                    }
                                                    (Some(ts), None) => Some(ts),
                                                    (None, Some(es)) => Some(es),
                                                    (None, None) => None,
                                                };
                                            }
                                        }
                                        parent.cur = bb_join;
                                        active = parent;
                                        break;
                                    }
                                    _ => {
                                        return Err(BodyAlgebraError::UnsupportedStmt {
                                            block: frame.switch_block,
                                            stmt: frame.switch_stmt,
                                        });
                                    }
                                }
                            } else {
                                match (then_end, else_end) {
                                    (RegionEnd::Returned(t), RegionEnd::Returned(e)) => {
                                        finished_end = RegionEnd::Returned(graph.select(
                                            frame.cond_expr,
                                            t,
                                            e,
                                        ));
                                        finished_state = frame.parent.state;
                                    }
                                    _ => {
                                        return Err(BodyAlgebraError::UnsupportedStmt {
                                            block: frame.switch_block,
                                            stmt: frame.switch_stmt,
                                        });
                                    }
                                }
                            }
                        }
                    },
                    LiftFrame::InlineCall {
                        mut parent,
                        call_block,
                        call_stmt,
                        dest_place,
                        next_block,
                    } => match finished_end {
                        RegionEnd::Returned(res_expr) => {
                            match resolve_place(
                                parent.body,
                                call_block,
                                call_stmt,
                                dest_place,
                                &parent.state.int_env,
                            )? {
                                PlaceTarget::Bare(local) => {
                                    parent.state.set_scalar(
                                        local,
                                        res_expr,
                                        call_block,
                                        call_stmt,
                                    )?;
                                }
                                PlaceTarget::Elem(local, idx) => {
                                    parent.state.set_elem(
                                        local,
                                        idx,
                                        res_expr,
                                        call_block,
                                        call_stmt,
                                    )?;
                                }
                            }
                            parent.cur = next_block;
                            active = parent;
                            break;
                        }
                        RegionEnd::ReachedJoin => {
                            return Err(BodyAlgebraError::MissingReturnValue { block: Block(0) });
                        }
                    },
                }
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
    lift_body_with_helpers(body, math, &NoHelperBodies, alloc)
}

/// Lift a verified pure numeric [`blaze_body::Body`] while inlining pure helper callees resolved
/// through `helpers` (up to [`MAX_INLINE_DEPTH`]) directly into the shared [`ExprGraph`].
pub fn lift_body_with_helpers<A: Allocator + Copy, R: MathResolver, H: CalleeBodyResolver>(
    body: Verified<'_>,
    math: &R,
    helpers: &H,
    alloc: A,
) -> Result<LiftedBody<A>, BodyAlgebraError> {
    let nlocals = usize::try_from(body.locals()).unwrap_or(0);
    if nlocals == 0 {
        return Err(BodyAlgebraError::MissingReturnLocal);
    }
    let f64_ty = body.local(Local(0)).ty;
    let nargs = body.args();
    let nargs_usize = usize::try_from(nargs)
        .map_err(|_| BodyAlgebraError::ArgOutOfRange { arg: 0, args: nargs })?;

    let mut graph = ExprGraph::new_in(alloc);
    let mut vars: OwnVec<VarId, A> = OwnVec::with_capacity_in(nargs_usize, alloc);
    let mut state = LiftState::new_in(nlocals, alloc);

    // Bind scalar `f64` argument locals eagerly; aggregate (`[f64; N]` / tuple) args bind lazily per element.
    const ARG_NAMES: [&str; 8] = ["x", "y", "z", "w", "a", "b", "c", "d"];
    for i in 0..nargs {
        let local = Local(i + 1);
        if body.local(local).ty == f64_ty {
            let (vid, eid) = if let Some(name) =
                usize::try_from(i).ok().and_then(|idx| ARG_NAMES.get(idx)).copied()
            {
                graph.var(name)
            } else {
                let mut buf = blaze_own::String::new_in(alloc);
                let _ = core::fmt::Write::write_fmt(&mut buf, format_args!("arg{i}"));
                graph.var(buf.as_str())
            };
            vars.push(vid);
            state.set_scalar(local, eid, Block(0), Stmt(0))?;
        }
    }

    let mut bool_ty = None;
    match lift_region(
        body,
        f64_ty,
        math,
        helpers,
        &mut graph,
        &mut vars,
        Block(0),
        state,
        &mut bool_ty,
        alloc,
    )? {
        RegionEnd::Returned(result) => {
            Ok(LiftedBody { graph, vars, result, f64_ty, bool_ty })
        }
        RegionEnd::ReachedJoin => Err(BodyAlgebraError::MissingReturnValue { block: Block(0) }),
    }
}

fn collect_postorder<A: Allocator + Copy>(
    g: &ExprGraph<A>,
    root: AlgExprId,
    visited: &mut OwnVec<bool, A>,
    order: &mut OwnVec<AlgExprId, A>,
    alloc: A,
) -> Result<(), AlgebraError> {
    let n = g.len();
    let mut stack: OwnVec<(AlgExprId, bool), A> = OwnVec::new_in(alloc);
    stack.push((root, false));
    while let Some((id, expanded)) = stack.pop() {
        if expanded {
            order.push(id);
            continue;
        }
        let idx = usize::try_from(id.0)
            .map_err(|_| AlgebraError::InvalidExpr { expr: id, node_count: n })?;
        let slot = visited
            .get_mut(idx)
            .ok_or(AlgebraError::InvalidExpr { expr: id, node_count: n })?;
        if *slot {
            continue;
        }
        *slot = true;
        stack.push((id, true));
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
            | Op::Abs(a) => {
                stack.push((a, false));
            }
            Op::Add(a, b) | Op::Sub(a, b) | Op::Mul(a, b) | Op::Div(a, b) | Op::Pow(a, b) => {
                stack.push((b, false));
                stack.push((a, false));
            }
            Op::Select(c, t, e) | Op::Fma(c, t, e) => {
                stack.push((e, false));
                stack.push((t, false));
                stack.push((c, false));
            }
        }
    }
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

fn get_node_op(
    node_ops: &[Option<Operand>],
    id: AlgExprId,
    node_count: usize,
) -> Result<Operand, BodyAlgebraError> {
    usize::try_from(id.0)
        .ok()
        .and_then(|idx| node_ops.get(idx).copied().flatten())
        .ok_or(BodyAlgebraError::Algebra(AlgebraError::InvalidExpr { expr: id, node_count }))
}

fn set_node_op(
    node_ops: &mut [Option<Operand>],
    id: AlgExprId,
    op: Operand,
    node_count: usize,
) -> Result<(), BodyAlgebraError> {
    let slot = usize::try_from(id.0)
        .ok()
        .and_then(|idx| node_ops.get_mut(idx))
        .ok_or(BodyAlgebraError::Algebra(AlgebraError::InvalidExpr { expr: id, node_count }))?;
    *slot = Some(op);
    Ok(())
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
        let Ok(r_idx) = usize::try_from(root.0) else {
            return Err(BodyAlgebraError::Algebra(AlgebraError::InvalidExpr {
                expr: root,
                node_count: n,
            }));
        };
        if r_idx >= n {
            return Err(BodyAlgebraError::Algebra(AlgebraError::InvalidExpr {
                expr: root,
                node_count: n,
            }));
        }
    }

    let mut visited = new_bool_vec(n, alloc);
    let mut order: OwnVec<AlgExprId, A> = OwnVec::new_in(alloc);
    for &root in roots {
        collect_postorder(g, root, &mut visited, &mut order, alloc)?;
    }

    let mut b = BodyBuilder::new_in(alloc);
    let ret_ty = array_ty.unwrap_or(f64_ty);
    let ret_local = b.new_local(LocalDecl { ty: ret_ty, kind: LocalKind::Return, mutbl: Mutbl::Mut });
    let ret_place = b.local_place(ret_local);

    let mut var_places: OwnVec<Place, A> = OwnVec::with_capacity_in(vars.len(), alloc);
    for (idx, _) in vars.iter().enumerate() {
        let arg_idx = u32::try_from(idx)
            .map_err(|_| BodyAlgebraError::ArgOutOfRange { arg: u32::MAX, args: u32::MAX })?;
        let l = b.new_local(LocalDecl {
            ty: f64_ty,
            kind: LocalKind::Arg(arg_idx),
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
                set_node_op(&mut node_ops, id, operand, n)?;
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
                let &place = var_places.get(pos).ok_or(BodyAlgebraError::Algebra(
                    AlgebraError::UnknownVar { var: vid, env_len: vars.len() },
                ))?;
                let operand = Operand::Copy(place);
                set_node_op(&mut node_ops, id, operand, n)?;
                if is_single_root {
                    b.assign(ret_place, Value::Use(operand), span);
                }
            }
            Op::Neg(a) => {
                let oa = get_node_op(&node_ops, a, n)?;
                let dest = if is_single_root { ret_place } else { alloc_temp_ty(&mut b, f64_ty, Mutbl::Not) };
                b.assign(dest, Value::Unary { op: UnOp::Neg, operand: oa }, span);
                set_node_op(&mut node_ops, id, Operand::Copy(dest), n)?;
            }
            Op::Add(a, b_id) | Op::Sub(a, b_id) | Op::Mul(a, b_id) | Op::Div(a, b_id) => {
                let oa = get_node_op(&node_ops, a, n)?;
                let ob = get_node_op(&node_ops, b_id, n)?;
                let bin = match op {
                    Op::Add(_, _) => BinOp::Add,
                    Op::Sub(_, _) => BinOp::Sub,
                    Op::Mul(_, _) => BinOp::Mul,
                    Op::Div(_, _) => BinOp::Div,
                    Op::Const(_)
                    | Op::Var(_)
                    | Op::Neg(_)
                    | Op::Pow(_, _)
                    | Op::Sin(_)
                    | Op::Cos(_)
                    | Op::Tan(_)
                    | Op::Exp(_)
                    | Op::Ln(_)
                    | Op::Sqrt(_)
                    | Op::Tanh(_)
                    | Op::Abs(_)
                    | Op::Select(_, _, _)
                    | Op::Fma(_, _, _) => {
                        return Err(BodyAlgebraError::Algebra(AlgebraError::InvalidExpr {
                            expr: id,
                            node_count: n,
                        }));
                    }
                };
                let dest = if is_single_root { ret_place } else { alloc_temp_ty(&mut b, f64_ty, Mutbl::Not) };
                b.assign(dest, Value::Binary { op: bin, lhs: oa, rhs: ob }, span);
                set_node_op(&mut node_ops, id, Operand::Copy(dest), n)?;
            }
            Op::Pow(a, b_id) => {
                let oa = get_node_op(&node_ops, a, n)?;
                let ob = get_node_op(&node_ops, b_id, n)?;
                let dest = if is_single_root { ret_place } else { alloc_temp_ty(&mut b, f64_ty, Mutbl::Not) };
                emit_math_call(&mut b, MathOp::Pow, &[oa, ob], dest, &mut cur_block)?;
                set_node_op(&mut node_ops, id, Operand::Copy(dest), n)?;
            }
            Op::Sin(a)
            | Op::Cos(a)
            | Op::Tan(a)
            | Op::Exp(a)
            | Op::Ln(a)
            | Op::Sqrt(a)
            | Op::Tanh(a)
            | Op::Abs(a) => {
                let oa = get_node_op(&node_ops, a, n)?;
                let mop = match op {
                    Op::Sin(_) => MathOp::Sin,
                    Op::Cos(_) => MathOp::Cos,
                    Op::Tan(_) => MathOp::Tan,
                    Op::Exp(_) => MathOp::Exp,
                    Op::Ln(_) => MathOp::Ln,
                    Op::Sqrt(_) => MathOp::Sqrt,
                    Op::Tanh(_) => MathOp::Tanh,
                    Op::Abs(_) => MathOp::Abs,
                    Op::Const(_)
                    | Op::Var(_)
                    | Op::Neg(_)
                    | Op::Add(_, _)
                    | Op::Sub(_, _)
                    | Op::Mul(_, _)
                    | Op::Div(_, _)
                    | Op::Pow(_, _)
                    | Op::Select(_, _, _)
                    | Op::Fma(_, _, _) => {
                        return Err(BodyAlgebraError::Algebra(AlgebraError::InvalidExpr {
                            expr: id,
                            node_count: n,
                        }));
                    }
                };
                let dest = if is_single_root { ret_place } else { alloc_temp_ty(&mut b, f64_ty, Mutbl::Not) };
                emit_math_call(&mut b, mop, &[oa], dest, &mut cur_block)?;
                set_node_op(&mut node_ops, id, Operand::Copy(dest), n)?;
            }
            Op::Select(c_id, t_id, e_id) => {
                let bty = bool_ty.ok_or(BodyAlgebraError::MissingBoolType)?;
                let oc = get_node_op(&node_ops, c_id, n)?;
                let ot = get_node_op(&node_ops, t_id, n)?;
                let oe = get_node_op(&node_ops, e_id, n)?;
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
                set_node_op(&mut node_ops, id, Operand::Copy(dest), n)?;
            }
            Op::Fma(a, b_id, c_id) => {
                let oa = get_node_op(&node_ops, a, n)?;
                let ob = get_node_op(&node_ops, b_id, n)?;
                let oc = get_node_op(&node_ops, c_id, n)?;
                let dest = if is_single_root { ret_place } else { alloc_temp_ty(&mut b, f64_ty, Mutbl::Not) };
                emit_math_call(&mut b, MathOp::Fma, &[oa, ob, oc], dest, &mut cur_block)?;
                set_node_op(&mut node_ops, id, Operand::Copy(dest), n)?;
            }
        }
    }

    if let Some(arr_ty) = array_ty {
        let mut elem_ops: OwnVec<Operand, A> = OwnVec::with_capacity_in(roots.len(), alloc);
        for &r in roots {
            elem_ops.push(get_node_op(&node_ops, r, n)?);
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
    let wrt = usize::try_from(arg_index)
        .ok()
        .and_then(|idx| lifted.vars.get(idx).copied())
        .ok_or(BodyAlgebraError::ArgOutOfRange { arg: arg_index, args: nargs })?;
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
    /// Differentiate with respect to argument `arg` and optimize the derivative under [`SimplifyOpts`]
    /// (e.g. `CostModel::CpuFma` fusing multiply-add into hardware `Op::Fma`).
    DiffWith { arg: u32, opts: SimplifyOpts },
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
    run_body_transform_with_helpers(body, transform, bounds, resolve, &NoHelperBodies, emit, alloc)
}

/// Compiler pass entry point running a [`BodyTransform`] with interprocedural pure helper inlining
/// (`CalleeBodyResolver`).
pub fn run_body_transform_with_helpers<
    A: Allocator + Copy,
    R: MathResolver,
    H: CalleeBodyResolver,
    E: MathEmitter,
>(
    body: Verified<'_>,
    transform: BodyTransform,
    bounds: Option<&[Interval]>,
    resolve: &R,
    helpers: &H,
    emit: &E,
    alloc: A,
) -> Result<Body<A>, BodyAlgebraError> {
    let mut lifted = lift_body_with_helpers(body, resolve, helpers, alloc)?;
    let mut expr = lifted.result;
    if let Some(var_bounds) = bounds {
        expr = prune_branches(&mut lifted.graph, expr, var_bounds)?;
    }
    match transform {
        BodyTransform::Simplify(opts) => {
            if opts.mode == RewriteMode::FastMath {
                expr = simplify_poly_expr(&mut lifted.graph, expr)?;
            }
            let (s, _) = simplify_with(&mut lifted.graph, expr, opts)?;
            splice_body_impl(&lifted.graph, &[s], &lifted.vars, lifted.f64_ty, lifted.bool_ty, None, emit, alloc)
        }
        BodyTransform::Diff { arg } => {
            let args_count = u32::try_from(lifted.vars.len()).unwrap_or(u32::MAX);
            let wrt = usize::try_from(arg)
                .ok()
                .and_then(|idx| lifted.vars.get(idx).copied())
                .ok_or(BodyAlgebraError::ArgOutOfRange { arg, args: args_count })?;
            let d = diff(&mut lifted.graph, expr, wrt)?;
            let d = simplify_poly_expr(&mut lifted.graph, d)?;
            let d = simplify(&mut lifted.graph, d)?;
            splice_body_impl(
                &lifted.graph,
                &[d],
                &lifted.vars,
                lifted.f64_ty,
                lifted.bool_ty,
                None,
                emit,
                alloc,
            )
        }
        BodyTransform::DiffWith { arg, opts } => {
            let args_count = u32::try_from(lifted.vars.len()).unwrap_or(u32::MAX);
            let wrt = usize::try_from(arg)
                .ok()
                .and_then(|idx| lifted.vars.get(idx).copied())
                .ok_or(BodyAlgebraError::ArgOutOfRange { arg, args: args_count })?;
            let d = diff(&mut lifted.graph, expr, wrt)?;
            let d = if opts.mode == RewriteMode::FastMath {
                simplify_poly_expr(&mut lifted.graph, d)?
            } else {
                d
            };
            let (d, _) = simplify_with(&mut lifted.graph, d, opts)?;
            splice_body_impl(
                &lifted.graph,
                &[d],
                &lifted.vars,
                lifted.f64_ty,
                lifted.bool_ty,
                None,
                emit,
                alloc,
            )
        }
        BodyTransform::ValueAndGrad { array_ty } => {
            let grads = grad_reverse(&mut lifted.graph, expr, &lifted.vars)?;
            let mut roots = OwnVec::with_capacity_in(1 + grads.len(), alloc);
            let primal = simplify_poly_expr(&mut lifted.graph, expr)?;
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
        BodyTransform::Hvp { array_ty } => {
            let n = lifted.vars.len();
            let mut dir_exprs = OwnVec::with_capacity_in(n, alloc);
            let mut all_vars = OwnVec::with_capacity_in(2 * n, alloc);
            for &v in lifted.vars.iter() {
                all_vars.push(v);
            }
            for i in 0..n {
                let mut buf = blaze_own::String::new_in(alloc);
                let _ = core::fmt::Write::write_fmt(&mut buf, format_args!("v{i}"));
                let (vid, eid) = lifted.graph.var(buf.as_str());
                dir_exprs.push(eid);
                all_vars.push(vid);
            }
            let hv = hvp(&mut lifted.graph, expr, &lifted.vars, &dir_exprs)?;
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
    let nlocals = usize::try_from(body.locals()).unwrap_or(0);
    if nlocals == 0 {
        return Err(BodyAlgebraError::MissingReturnLocal);
    }
    let nargs_u32 = body.args();
    let nargs = usize::try_from(nargs_u32)
        .map_err(|_| BodyAlgebraError::ArgOutOfRange { arg: 0, args: nargs_u32 })?;
    if args_in.len() < nargs {
        return Err(BodyAlgebraError::ArgOutOfRange {
            arg: u32::try_from(args_in.len()).unwrap_or(u32::MAX),
            args: nargs_u32,
        });
    }

    let mut env: OwnVec<Option<MirVal<A>>, A> = OwnVec::with_capacity_in(nlocals, alloc);
    for _ in 0..nlocals {
        env.push(None);
    }
    for (slot, &v) in env.iter_mut().skip(1).zip(args_in.iter().take(nargs)) {
        *slot = Some(MirVal::Scalar(v));
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
                ConstKind::Scalar { bits } => match u64::try_from(bits) {
                    Ok(b) => Ok(f64::from_bits(b)),
                    Err(_) => Err(BodyAlgebraError::UnsupportedConst { block, stmt }),
                },
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
    let max_blocks_usize = usize::try_from(max_blocks)
        .map_err(|_| BodyAlgebraError::CycleOrStepLimit { block: Block(0), steps: 0 })?;
    let mut visited = new_bool_vec(max_blocks_usize, alloc);

    let mut cur = Block(0);
    let mut steps = 0u32;
    loop {
        let Some(vis_slot) = visited.get_mut(cur.index()) else {
            return Err(BodyAlgebraError::CycleOrStepLimit { block: cur, steps });
        };
        if steps >= max_blocks || *vis_slot {
            return Err(BodyAlgebraError::CycleOrStepLimit { block: cur, steps });
        }
        *vis_slot = true;
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
                            let cap = usize::try_from(operands.len())
                                .map_err(|_| BodyAlgebraError::UnsupportedValue { block: cur, stmt })?;
                            let mut elems = OwnVec::with_capacity_in(cap, alloc);
                            for op in body.ops(operands) {
                                elems.push(read_f64(cur, stmt, op, &env)?);
                            }
                            MirVal::Array(elems)
                        }
                        _ => return Err(BodyAlgebraError::UnsupportedValue { block: cur, stmt }),
                    };
                    set_local(&mut env, local, Some(val), cur, stmt)?;
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
                    if usize::try_from(args.len()) != Ok(math_op.arity()) {
                        return Err(BodyAlgebraError::UnsupportedCall { block: cur, stmt });
                    }
                    let mut vals = [0.0_f64; 3];
                    for (slot, op) in vals.iter_mut().zip(body.ops(args)) {
                        *slot = read_f64(cur, stmt, op, &env)?;
                    }
                    let [v0, v1, v2] = vals;
                    let res = match math_op {
                        MathOp::Sin => blaze_math::sin(v0),
                        MathOp::Cos => blaze_math::cos(v0),
                        MathOp::Tan => blaze_math::tan(v0),
                        MathOp::Exp => blaze_math::exp(v0),
                        MathOp::Ln => blaze_math::ln(v0),
                        MathOp::Sqrt => blaze_math::sqrt(v0),
                        MathOp::Tanh => det_tanh(v0),
                        MathOp::Abs => blaze_math::abs(v0),
                        MathOp::Pow => blaze_math::pow(v0, v1),
                        MathOp::Fma => blaze_math::mul_add(v0, v1, v2),
                    };
                    let local = bare_local(body, cur, stmt, dest_place)?;
                    set_local(&mut env, local, Some(MirVal::Scalar(res)), cur, stmt)?;
                    cur = next_block;
                    break;
                }
                StmtKind::Return => {
                    return env
                        .first_mut()
                        .and_then(Option::take)
                        .ok_or(BodyAlgebraError::MissingReturnValue { block: cur });
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
