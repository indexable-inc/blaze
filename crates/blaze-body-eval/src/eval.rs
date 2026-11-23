//! The interpreter: a verified body's rows run as written, over blaze-consteval's memory.
//!
//! - A call pushes a [`Frame`] on an explicit stack (never host recursion), bounded by
//!   `Limits::depth`; every row run counts one step against `Limits::steps` (rule 18).
//! - Every local is one allocation of its ground type's size (`Origin::Local`). `live` makes a
//!   fresh one, `dead` releases it; a local the body never makes live (the return place, the
//!   arguments, a temporary the lowering left unmarked) is allocated at frame entry.
//! - A place is a pointer with provenance, the metadata of an unsized place, its ground type
//!   and the variant a downcast selected.
//! - Unwind edges are never followed: in a constant a panic is an error, so a `cleanup` block
//!   is never entered and `resume` is a malformed body.
//! - Types in rows are of the body's table; each is grounded at the frame's arguments
//!   (`blaze_build::ground`) into the one ground table layouts and instance keys read.

use core::cmp::Ordering;

use blaze_body::{
    AggregateKind, AssertKind, BinOp, Block, Callee, CalleeId, CastKind, Const, ConstKind, Local, Operand, Ops,
    Place, Proj, Stmt, StmtKind, UnOp, Value, Verified,
};
use blaze_build::{BuildError, ground, instance_key};
use blaze_consteval::arith::{self, Plant};
use blaze_consteval::layout::{Stop as LayoutStop, Tag};
use blaze_consteval::mem::{AllocId, Memory, Origin, PTR, Pointer, Scalar};
use blaze_consteval::{
    Answer, DataAlloc, DataRecord, DataReloc, DataTarget, FnKind, Intrinsic, Layout, LayoutCx,
    LayoutFacts, Limits, Need, NeedKind, Op, RecordKey, Refusal, eval_discriminant,
};
use blaze_own::{Alloc, Vec};
use blaze_typeck::{FloatTy, ItemKey, List, PNode, PTy, PTys, PortableTys};

use crate::error::{EvalError, Fault, Row};
use crate::world::{BodyWorld, DropGlue};

/// Evaluates the body of `item` at `args` (a list of `ground`; empty for a constant with no
/// generics) to its value, interned in blaze-build's data form: allocation 0 is the value,
/// the rest what it points to. `Answer::Need` names the body or the constant's record the
/// world lacks; the caller supplies it and runs again (records make the rerun cheap).
///
/// The body must take no arguments: it is a constant's, a static's or a promoted's.
pub fn eval_body<'w, 'a, W: BodyWorld, L: Layout>(
    world: &'w W,
    layout: &mut L,
    ground: &mut PortableTys<'_>,
    item: ItemKey,
    args: List,
    limits: Limits,
    alloc: Alloc<'a>,
) -> Result<Answer<DataRecord<Alloc<'a>>>, EvalError<'a>> {
    let mut m = Machine {
        world,
        layout,
        ground,
        mem: Memory::new_in(limits.memory, alloc),
        frames: Vec::new_in(alloc),
        consts: Vec::new_in(alloc),
        bytes_consts: Vec::new_in(alloc),
        proj_scratch: Vec::new_in(alloc),
        body_live: Vec::new_in(alloc),
        live_bits: Vec::new_in(alloc),
        inst_tys: Vec::new_in(alloc),
        cached_tys: Vec::new_in(alloc),
        limits,
        steps: 0,
        alloc,
        at: None,
    };
    match m.run(item, args) {
        Ok(rec) => Ok(Answer::Done(rec)),
        Err(Stop::Need(n)) => Ok(Answer::Need(n)),
        Err(Stop::Fault(fault)) => Err(EvalError { constant: item, at: m.at, fault }),
    }
}

/// How a step stopped the run.
enum Stop<'a> {
    Fault(Fault<'a>),
    Need(Need),
}

impl<'a, 'b: 'a> From<Refusal<'b>> for Stop<'a> {
    fn from(r: Refusal<'b>) -> Stop<'a> {
        Stop::Fault(Fault::Refused(r))
    }
}

type R<'a, T> = Result<T, Stop<'a>>;

fn malformed(what: &'static str) -> Refusal<'static> {
    Refusal::Malformed { what }
}

fn unsupported(what: &'static str) -> Refusal<'static> {
    Refusal::Unsupported { what }
}

fn layout_stop<'a>(s: LayoutStop) -> Stop<'a> {
    match s {
        LayoutStop::Refused(r) => r.into(),
        // An explicit discriminant's constant: its record first.
        LayoutStop::Need(item) => Stop::Need(Need { kind: NeedKind::Value, item, args: List::EMPTY }),
    }
}

fn build_refusal(e: BuildError) -> Refusal<'static> {
    match e {
        BuildError::Projection { trait_, name } => Refusal::Projection { trait_, name },
        _ => malformed("a body's type that does not ground at the frame's arguments"),
    }
}

/// How many of a body's own types [`ground_in`] nests through before it refuses (rule 18).
const OWN_TY_DEPTH: u32 = 256;

/// What [`ground_in`] reads: the body (its type overlay), the record's table it was built
/// against, the frame's ground arguments, and where it grounds.
struct GroundCx<'c, 'w, 't> {
    body: Verified<'w>,
    from: PTys<'w>,
    args: &'c [PTy],
    g: &'c mut PortableTys<'t>,
    scratch: Alloc<'c>,
    facts: &'c dyn LayoutFacts,
}

/// `ty`, a handle the body names (the record's table `from` below the body's overlay, the
/// overlay's from `Verified::ty_base` on), at `args` (types of `g`), as a type of `g`. A record
/// type goes to `blaze_build::ground`; an overlay node is rebuilt in `g` over its grounded
/// children.
fn ground_in(
    body: Verified<'_>,
    from: PTys<'_>,
    args: &[PTy],
    ty: PTy,
    g: &mut PortableTys<'_>,
    scratch: Alloc<'_>,
    facts: &dyn LayoutFacts,
) -> Result<PTy, Refusal<'static>> {
    let mut cx = GroundCx { body, from, args, g, scratch, facts };
    ground_own(&mut cx, ty, 0)
}

fn ground_own(cx: &mut GroundCx<'_, '_, '_>, ty: PTy, depth: u32) -> Result<PTy, Refusal<'static>> {
    enum Step {
        Visit(PTy, u32),
        Finish { node: PNode, list_len: usize },
    }
    let mut work: Vec<Step, Alloc<'_>> = Vec::new_in(cx.scratch);
    let mut vals: Vec<PTy, Alloc<'_>> = Vec::new_in(cx.scratch);
    work.push(Step::Visit(ty, depth));
    while let Some(step) = work.pop() {
        match step {
            Step::Visit(cur, cur_depth) => {
                let node = match cx.body.own_ty(cur) {
                    Ok(None) => {
                        let g = ground(cx.from, cur, cx.args, cx.g, cx.scratch, cx.facts).map_err(build_refusal)?;
                        vals.push(g);
                        continue;
                    }
                    Ok(Some(n)) => n,
                    Err(_) => return Err(malformed("a type handle past the body's overlay")),
                };
                if cur_depth >= OWN_TY_DEPTH {
                    return Err(unsupported("a body's own type nested past the evaluator's depth bound"));
                }
                let d = cur_depth + 1;
                match node {
                    PNode::Bool
                    | PNode::Char
                    | PNode::Int(_)
                    | PNode::Uint(_)
                    | PNode::Float(_)
                    | PNode::Str
                    | PNode::Never
                    | PNode::ExAuto(_)
                    | PNode::Foreign(_) => {
                        let interned = cx.g.intern(node).map_err(|_| malformed("a ground type the table cannot hold"))?;
                        vals.push(interned);
                    }
                    PNode::Param(i) | PNode::ConstParam(i) => {
                        let idx = usize::try_from(i).map_err(|_| malformed("a parameter past the frame's arguments"))?;
                        let a = cx.args.get(idx).copied().ok_or(malformed("a parameter past the frame's arguments"))?;
                        vals.push(a);
                    }
                    PNode::Ref(_, t) | PNode::Ptr(_, t) | PNode::Slice(t) | PNode::ConstValue { ty: t, .. } => {
                        work.push(Step::Finish { node, list_len: 0 });
                        work.push(Step::Visit(t, d));
                    }
                    PNode::Array(e, n) | PNode::Future(_, e, n) => {
                        work.push(Step::Finish { node, list_len: 0 });
                        work.push(Step::Visit(n, d));
                        work.push(Step::Visit(e, d));
                    }
                    PNode::Tuple(l)
                    | PNode::Record(_, l)
                    | PNode::Union(l)
                    | PNode::Adt(_, l)
                    | PNode::FnDef(_, l)
                    | PNode::FnPtr(l, _)
                    | PNode::Closure { args: l, .. }
                    | PNode::Coroutine { args: l, .. }
                    | PNode::Dynamic(l)
                    | PNode::ExTrait(_, l) => {
                        let src = cx.body.ty_list(&cx.from, l).map_err(|_| malformed("a list past the body's type table"))?;
                        let len = src.len();
                        work.push(Step::Finish { node, list_len: len });
                        for i in (0..len).rev() {
                            let t = src.get(i).ok_or(malformed("a list entry past its list"))?;
                            work.push(Step::Visit(t, d));
                        }
                    }
                    PNode::Pat(t, l) => {
                        let src = cx.body.ty_list(&cx.from, l).map_err(|_| malformed("a list past the body's type table"))?;
                        let len = src.len();
                        work.push(Step::Finish { node, list_len: len });
                        for i in (0..len).rev() {
                            let item = src.get(i).ok_or(malformed("a list entry past its list"))?;
                            work.push(Step::Visit(item, d));
                        }
                        work.push(Step::Visit(t, d));
                    }
                    PNode::ExProj { args, term, .. } => {
                        let src = cx.body.ty_list(&cx.from, args).map_err(|_| malformed("a list past the body's type table"))?;
                        let len = src.len();
                        work.push(Step::Finish { node, list_len: len });
                        work.push(Step::Visit(term, d));
                        for i in (0..len).rev() {
                            let item = src.get(i).ok_or(malformed("a list entry past its list"))?;
                            work.push(Step::Visit(item, d));
                        }
                    }
                    // A projection or an opaque type the body made has no record table `ground` could
                    // normalize it in: refused by name (rule 3).
                    PNode::Alias { trait_, name, .. } => return Err(Refusal::Projection { trait_, name }),
                    PNode::Opaque(..) => return Err(unsupported("an opaque type made in the body, grounded")),
                    PNode::Infer | PNode::Error => return Err(malformed("a hole or an error type in a verified body")),
                }
            }
            Step::Finish { node, list_len } => {
                let pop_list = |vals: &mut Vec<PTy, Alloc<'_>>, g: &mut PortableTys<'_>| -> Result<List, Refusal<'static>> {
                    let start = vals.len().checked_sub(list_len).ok_or(malformed("grounding stack underflow"))?;
                    let slice = vals.get(start..).ok_or(malformed("grounding stack underflow"))?;
                    let list = g.intern_list(slice).map_err(|_| malformed("a ground list the table cannot hold"))?;
                    vals.truncate(start);
                    Ok(list)
                };
                let n = match node {
                    PNode::Tuple(_) => PNode::Tuple(pop_list(&mut vals, cx.g)?),
                    PNode::Record(s, _) => PNode::Record(s, pop_list(&mut vals, cx.g)?),
                    PNode::Union(_) => PNode::Union(pop_list(&mut vals, cx.g)?),
                    PNode::Ref(m, _) => PNode::Ref(m, vals.pop().ok_or(malformed("grounding stack underflow"))?),
                    PNode::Ptr(m, _) => PNode::Ptr(m, vals.pop().ok_or(malformed("grounding stack underflow"))?),
                    PNode::Array(_, _) => {
                        let n = vals.pop().ok_or(malformed("grounding stack underflow"))?;
                        let e = vals.pop().ok_or(malformed("grounding stack underflow"))?;
                        PNode::Array(e, n)
                    }
                    PNode::Slice(_) => PNode::Slice(vals.pop().ok_or(malformed("grounding stack underflow"))?),
                    PNode::Pat(_, _) => {
                        let l = pop_list(&mut vals, cx.g)?;
                        let t = vals.pop().ok_or(malformed("grounding stack underflow"))?;
                        PNode::Pat(t, l)
                    }
                    PNode::Adt(k, _) => PNode::Adt(k, pop_list(&mut vals, cx.g)?),
                    PNode::FnDef(k, _) => PNode::FnDef(k, pop_list(&mut vals, cx.g)?),
                    PNode::FnPtr(_, h) => PNode::FnPtr(pop_list(&mut vals, cx.g)?, h),
                    PNode::Closure { owner, ordinal, .. } => PNode::Closure { owner, ordinal, args: pop_list(&mut vals, cx.g)? },
                    PNode::Coroutine { owner, ordinal, .. } => PNode::Coroutine { owner, ordinal, args: pop_list(&mut vals, cx.g)? },
                    PNode::Future(e, _, _) => {
                        let interior = vals.pop().ok_or(malformed("grounding stack underflow"))?;
                        let out = vals.pop().ok_or(malformed("grounding stack underflow"))?;
                        PNode::Future(e, out, interior)
                    }
                    PNode::ConstValue { bits, .. } => {
                        let ty = vals.pop().ok_or(malformed("grounding stack underflow"))?;
                        PNode::ConstValue { ty, bits }
                    }
                    PNode::Dynamic(_) => PNode::Dynamic(pop_list(&mut vals, cx.g)?),
                    PNode::ExTrait(k, _) => PNode::ExTrait(k, pop_list(&mut vals, cx.g)?),
                    PNode::ExProj { trait_, name, .. } => {
                        let term = vals.pop().ok_or(malformed("grounding stack underflow"))?;
                        let args = pop_list(&mut vals, cx.g)?;
                        PNode::ExProj { trait_, name, args, term }
                    }
                    _ => return Err(malformed("unexpected node on grounding stack")),
                };
                let interned = cx.g.intern(n).map_err(|_| malformed("a ground type the table cannot hold"))?;
                vals.push(interned);
            }
        }
    }
    vals.pop().ok_or(malformed("grounding stack underflow"))
}

/// The list `l` the body names, each entry grounded, interned in `g`.
fn ground_own_list(cx: &mut GroundCx<'_, '_, '_>, l: List, depth: u32) -> Result<List, Refusal<'static>> {
    let src = cx.body.ty_list(&cx.from, l).map_err(|_| malformed("a list past the body's type table"))?;
    let mut out: Vec<PTy, Alloc<'_>> = Vec::with_capacity_in(src.len(), cx.scratch);
    for i in 0..src.len() {
        let t = src.get(i).ok_or(malformed("a list entry past its list"))?;
        out.push(ground_own(cx, t, depth)?);
    }
    cx.g.intern_list(&out).map_err(|_| malformed("a ground list the table cannot hold"))
}

/// Where a call returns to: the caller's destination, and the block after the call.
#[derive(Clone, Copy)]
struct Ret {
    dest: Option<Pointer>,
    target: Option<Block>,
}

/// One activation.
struct Frame<'w, 'a> {
    item: ItemKey,
    body: Verified<'w>,
    types: PTys<'w>,
    /// The ground arguments, a copy of the list `ground` holds.
    args: Vec<PTy, Alloc<'a>>,
    /// Slice `(start, len)` in `Machine::cached_tys` holding each local's ground type.
    tys: (u32, u32),
    /// Each local's allocation while it is live.
    locals: Vec<Option<AllocId>, Alloc<'a>>,
    /// The row run next.
    at: Stmt,
    ret: Ret,
}

/// A place: where it is, its metadata if unsized, its ground type, the variant a downcast
/// selected (0 otherwise).
#[derive(Clone, Copy)]
struct Pl {
    ptr: Pointer,
    meta: Option<u64>,
    ty: PTy,
    variant: u32,
}

/// An operand's value: in memory (a place's bytes, or a temporary made for a constant), or
/// a scalar constant's bits.
#[derive(Clone, Copy)]
enum Src {
    At(Pointer),
    Temp(AllocId),
    Imm(u128),
}

#[derive(Clone, Copy)]
struct Opnd {
    src: Src,
    ty: PTy,
}

struct Machine<'w, 'g, 't, 'a, W: BodyWorld, L: Layout> {
    world: &'w W,
    layout: &'g mut L,
    ground: &'g mut PortableTys<'t>,
    mem: Memory<Alloc<'a>>,
    frames: Vec<Frame<'w, 'a>, Alloc<'a>>,
    /// Constants' records loaded into memory this run, by item and ground arguments: a
    /// constant read in a loop is loaded once (rule 77).
    consts: Vec<(ItemKey, List, AllocId), Alloc<'a>>,
    /// Byte-string / string literal data allocations cached by `(item, const_id)` so loops
    /// never leak `Origin::Data` allocations.
    bytes_consts: Vec<(ItemKey, Const, AllocId), Alloc<'a>>,
    /// Reused projection scratch stack for `place()` (zero per-call heap allocations).
    proj_scratch: Vec<Proj, Alloc<'a>>,
    /// Cached per-body `StorageLive` scan: `(item, start, len)` indexing `live_bits`.
    body_live: Vec<(ItemKey, u32, u32), Alloc<'a>>,
    live_bits: Vec<bool, Alloc<'a>>,
    /// Cached per-instance grounded local types: `(item, args, start, len)` indexing `cached_tys`.
    inst_tys: Vec<(ItemKey, List, u32, u32), Alloc<'a>>,
    cached_tys: Vec<PTy, Alloc<'a>>,
    limits: Limits,
    steps: u64,
    alloc: Alloc<'a>,
    /// The row running: what an error names.
    at: Option<Row>,
}

impl<'w, 'g, 't, 'a, W: BodyWorld, L: Layout> Machine<'w, 'g, 't, 'a, W, L> {
    fn run(&mut self, item: ItemKey, args: List) -> R<'a, DataRecord<Alloc<'a>>> {
        let root = self.frame(item, args, Ret { dest: None, target: None })?;
        if root.body.args() != 0 {
            return Err(malformed("a constant's body that takes arguments").into());
        }
        self.frames.push(root);
        loop {
            self.tick()?;
            let f = self.top()?;
            let (body, item, at) = (f.body, f.item, f.at);
            let row = body.stmt(at);
            self.at = Some(Row { body: item, stmt: at, span: row.span });
            if let Some(done) = self.step(row.kind)? {
                return Ok(done);
            }
        }
    }

    fn tick(&mut self) -> R<'a, ()> {
        self.steps += 1;
        if self.steps > self.limits.steps {
            return Err(Refusal::StepLimit { limit: self.limits.steps }.into());
        }
        Ok(())
    }

    fn top(&self) -> R<'a, &Frame<'w, 'a>> {
        self.frames.last().ok_or(Stop::from(malformed("a row run with no frame")))
    }

    fn top_mut(&mut self) -> R<'a, &mut Frame<'w, 'a>> {
        self.frames.last_mut().ok_or(Stop::from(malformed("a row run with no frame")))
    }

    // ---- Rows ----

    /// Runs one row; `Some` when the root frame returned.
    fn step(&mut self, kind: StmtKind) -> R<'a, Option<DataRecord<Alloc<'a>>>> {
        match kind {
            StmtKind::Assign { dest, value } => {
                let d = self.place(dest)?;
                self.value(d, value)?;
                self.next()?;
            }
            StmtKind::StorageLive(l) => {
                // A `live` of a live local starts fresh storage (MIR's StorageLive).
                self.storage_dead(l)?;
                self.storage_live(l)?;
                self.next()?;
            }
            StmtKind::StorageDead(l) => {
                self.storage_dead(l)?;
                self.next()?;
            }
            StmtKind::Activate(_) | StmtKind::FakeRead(_) => self.next()?,
            // The fields were written by their own assignments; only the tag is written here.
            StmtKind::SetDiscriminant { place, variant } => {
                let d = self.place(place)?;
                self.set_tag(d.ptr, d.ty, variant)?;
                self.next()?;
            }
            StmtKind::Deinit(place) => {
                let d = self.place(place)?;
                if d.meta.is_some() {
                    return Err(unsupported("`deinit` of an unsized place").into());
                }
                let size = self.size(d.ty)?;
                self.mem.deinit(d.ptr, size)?;
                self.next()?;
            }
            StmtKind::Goto(b) => self.jump(b)?,
            StmtKind::Switch { on, cases, otherwise } => {
                let v = self.int(on)?;
                let body = self.top()?.body;
                let to = body.cases(cases).find(|&(c, _)| c == v).map_or(otherwise, |(_, b)| b);
                self.jump(to)?;
            }
            StmtKind::Call { callee, args, dest, target, unwind: _ } => self.call(callee, args, dest, target)?,
            StmtKind::Drop { place, target, unwind: _ } => self.drop_place(place, target)?,
            StmtKind::Assert { cond, expected, kind, msg, target, unwind: _ } => {
                if (self.int(cond)? != 0) != expected {
                    let stop = self.assert_fault(kind, msg)?;
                    return Err(stop);
                }
                self.jump(target)?;
            }
            StmtKind::Return => return self.ret(),
            // Undefined behaviour reached (the Reference, "Behavior considered undefined").
            StmtKind::Unreachable => return Err(Refusal::Unreachable.into()),
            StmtKind::Resume => return Err(malformed("`resume` reached: the evaluator follows no unwind edge").into()),
            StmtKind::Terminate => return Err(Stop::Fault(Fault::Abort)),
            StmtKind::Suspend { .. } => return Err(unsupported("a suspension (a coroutine's) in a constant").into()),
            StmtKind::InlineAsm { .. } => return Err(unsupported("inline assembly in a constant").into()),
        }
        Ok(None)
    }

    fn next(&mut self) -> R<'a, ()> {
        let f = self.top_mut()?;
        f.at = Stmt(f.at.0 + 1);
        Ok(())
    }

    fn jump(&mut self, b: Block) -> R<'a, ()> {
        let f = self.top_mut()?;
        f.at = Stmt(f.body.block(b).stmts.start);
        Ok(())
    }

    fn jump_opt(&mut self, target: Option<Block>) -> R<'a, ()> {
        match target {
            Some(b) => self.jump(b),
            None => Err(malformed("a call that returns with no target block").into()),
        }
    }

    fn assert_fault(&mut self, kind: AssertKind, msg: Ops) -> R<'a, Stop<'a>> {
        let refusal = match kind {
            AssertKind::Overflow(op) => Refusal::Overflow { op: op_of(op)? },
            AssertKind::OverflowNeg => Refusal::Overflow { op: Op::Neg },
            AssertKind::DivisionByZero => Refusal::DivByZero,
            AssertKind::RemainderByZero => Refusal::RemByZero,
            AssertKind::Bounds => {
                // The message's operands: the length, then the index (MIR's `BoundsCheck`).
                let body = self.top()?.body;
                let mut ops = body.ops(msg);
                let (Some(len), Some(index)) = (ops.next(), ops.next()) else {
                    return Err(malformed("a bounds assert without its length and index").into());
                };
                let (len, index) = (self.int(len)? as u64, self.int(index)? as u64);
                Refusal::IndexOutOfBounds { index, len }
            }
            AssertKind::ResumedAfterReturn | AssertKind::ResumedAfterPanic => {
                unsupported("a coroutine resumed after it finished, in a constant")
            }
        };
        Ok(Stop::Fault(Fault::Assert { kind, refusal }))
    }

    // ---- Frames ----

    /// A frame for `item` at `args` (a list of the ground table), its locals allocated but
    /// for those the body makes live itself.
    fn frame(&mut self, item: ItemKey, args: List, ret: Ret) -> R<'a, Frame<'w, 'a>> {
        if self.frames.len() >= self.limits.depth as usize {
            return Err(Refusal::CallDepth { limit: self.limits.depth }.into());
        }
        let b = match self.world.verified(item) {
            Ok(Some(b)) => b,
            Ok(None) => return Err(Refusal::NotConstFn { item }.into()),
            Err(_) => return Err(Stop::Need(Need { kind: NeedKind::Body, item, args })),
        };
        let mut argv: Vec<PTy, Alloc<'a>> = Vec::new_in(self.alloc);
        argv.extend_from_slice(self.ground.view().list(args).map_err(|_| malformed("a call's arguments past the ground table"))?);
        let n = b.body.locals() as usize;
        // Scan StorageLive statements once per body (avoids O(stmts) scan on every recursive call).
        let (live_start, live_len) = match self.body_live.iter().find(|&&(k, _, _)| k == item) {
            Some(&(_, s, l)) => (s as usize, l as usize),
            None => {
                let s = self.live_bits.len();
                self.live_bits.resize(s + n, false);
                for st in 0..b.body.stmts() {
                    if let StmtKind::StorageLive(l) = b.body.stmt(Stmt(st)).kind
                        && let Some(bit) = self.live_bits.get_mut(s + l.0 as usize)
                    {
                        *bit = true;
                    }
                }
                self.body_live.push((item, s as u32, n as u32));
                (s, n)
            }
        };
        // Ground local types once per `(item, args)` instance.
        let (tys_start, tys_len) = match self.inst_tys.iter().find(|&&(k, a, _, _)| k == item && a == args) {
            Some(&(_, _, s, l)) => (s, l),
            None => {
                let s = self.cached_tys.len() as u32;
                for l in 0..n {
                    let decl = b.body.local(Local(l as u32));
                    let gty = ground_in(b.body, b.types, &argv, decl.ty, &mut *self.ground, self.alloc, self.world)?;
                    self.cached_tys.push(gty);
                }
                self.inst_tys.push((item, args, s, n as u32));
                (s, n as u32)
            }
        };
        let mut locals: Vec<Option<AllocId>, Alloc<'a>> = Vec::with_capacity_in(n, self.alloc);
        for l in 0..n {
            let needs_live = l < live_len && self.live_bits.get(live_start + l).copied().unwrap_or(false);
            if l < live_len && !needs_live {
                let Some(&ty) = self.cached_tys.get(tys_start as usize + l) else {
                    return Err(malformed("a local past the frame").into());
                };
                locals.push(Some(self.allocate_local(ty)?));
            } else {
                locals.push(None);
            }
        }
        Ok(Frame { item, body: b.body, types: b.types, args: argv, tys: (tys_start, tys_len), locals, at: b.body.entry(), ret })
    }

    fn local_ty(&self, f: &Frame<'w, 'a>, i: usize) -> R<'a, PTy> {
        if i >= f.tys.1 as usize {
            return Err(malformed("a local past the frame").into());
        }
        self.cached_tys
            .get(f.tys.0 as usize + i)
            .copied()
            .ok_or_else(|| malformed("a local past the frame").into())
    }

    fn allocate_local(&mut self, ty: PTy) -> R<'a, AllocId> {
        let (size, align) = self.size_align(ty)?;
        Ok(self.mem.allocate(size, align, true, Origin::Local)?)
    }

    fn storage_live(&mut self, l: Local) -> R<'a, ()> {
        let f = self.top()?;
        let ty = self.local_ty(f, l.0 as usize)?;
        let id = self.allocate_local(ty)?;
        let slot = self
            .top_mut()?
            .locals
            .get_mut(l.0 as usize)
            .ok_or(Stop::from(malformed("a local past the frame")))?;
        *slot = Some(id);
        Ok(())
    }

    fn storage_dead(&mut self, l: Local) -> R<'a, ()> {
        let slot = self.top_mut()?.locals.get_mut(l.0 as usize).ok_or(Stop::from(malformed("a local past the frame")))?;
        if let Some(id) = slot.take() {
            self.mem.kill(id);
        }
        Ok(())
    }

    fn call(&mut self, callee: CalleeId, args: Ops, dest: Option<Place>, target: Option<Block>) -> R<'a, ()> {
        let body = self.top()?.body;
        let (key, gargs) = match body.callee(callee) {
            Callee::FnItem { key, args } | Callee::Method { key, args } | Callee::Intrinsic { key, args } => {
                (key, self.ground_list(args)?)
            }
            Callee::Value(op) => {
                let o = self.operand(op)?;
                self.release(o);
                match self.node(o.ty)? {
                    // A fn item's value is zero-sized: its type names the callee.
                    PNode::FnDef(k, l) => (k, l),
                    _ => return Err(unsupported("a call through a fn pointer in a constant").into()),
                }
            }
        };
        // The same two questions the old evaluator asks (machine/code.rs `call`).
        let (item, iargs) = self.resolve(key, gargs)?;
        let d = match dest {
            Some(p) => Some(self.place(p)?),
            None => None,
        };
        match self.world.fn_kind(item) {
            FnKind::Const => {
                let f = self.frame(item, iargs, Ret { dest: d.map(|d| d.ptr), target })?;
                let mut k = 1usize;
                for op in body.ops(args) {
                    let id = f.locals.get(k).copied().flatten().ok_or(Stop::from(malformed("a call with more arguments than its callee takes")))?;
                    self.operand_into(op, Pointer { alloc: id, offset: 0 })?;
                    k += 1;
                }
                if k != f.body.args() as usize + 1 {
                    return Err(malformed("a call with fewer arguments than its callee takes").into());
                }
                self.frames.push(f);
                Ok(())
            }
            FnKind::NotConst => Err(Refusal::NotConstFn { item }.into()),
            FnKind::UnknownIntrinsic => Err(Refusal::UnknownIntrinsic { item }.into()),
            FnKind::Missing(fact) => Err(Refusal::FnFact { item, fact }.into()),
            FnKind::Ctor { variant, .. } => {
                let d = d.ok_or(Stop::from(malformed("a constructor call with no destination")))?;
                self.fields(d.ptr, d.ty, variant, body.ops(args))?;
                self.set_tag(d.ptr, d.ty, variant)?;
                self.jump_opt(target)
            }
            // The panic's message is not formatted yet (see the report: NOT DONE).
            FnKind::Panic => Err(Refusal::Panic { parts: Vec::new_in(self.alloc) }.into()),
            FnKind::Intrinsic(i) => {
                self.intrinsic(i, iargs, args, d)?;
                self.jump_opt(target)
            }
        }
    }

    fn resolve(&mut self, item: ItemKey, args: List) -> R<'a, (ItemKey, List)> {
        self.world
            .resolve(item, args, &mut *self.ground, self.alloc)
            .ok_or(Stop::from(unsupported("a trait method no impl answers at the frame's arguments")))
    }

    fn drop_place(&mut self, place: Place, target: Block) -> R<'a, ()> {
        let pl = self.place(place)?;
        match self.world.drop_glue(pl.ty, &mut *self.ground, self.alloc)? {
            DropGlue::Nothing => self.jump(target),
            DropGlue::Call { item, args } => {
                if pl.meta.is_some() {
                    return Err(unsupported("a drop of an unsized place in a constant").into());
                }
                let f = self.frame(item, args, Ret { dest: None, target: Some(target) })?;
                let one = f.locals.get(1).copied().flatten().ok_or(Stop::from(malformed("drop glue that takes no argument")))?;
                self.mem.write(Pointer { alloc: one, offset: 0 }, PTR, Scalar::Ptr(pl.ptr))?;
                self.frames.push(f);
                Ok(())
            }
        }
    }

    /// `return`: the callee's `_0` to the caller's destination, its locals released; the
    /// root's `_0` interned as the value.
    fn ret(&mut self) -> R<'a, Option<DataRecord<Alloc<'a>>>> {
        let f = self.frames.pop().ok_or(Stop::from(malformed("a return with no frame")))?;
        let r0 = f.locals.first().copied().flatten().ok_or(Stop::from(Refusal::Dangling))?;
        if self.frames.is_empty() {
            return Ok(Some(self.intern(r0)?));
        }
        if let Some(dst) = f.ret.dest {
            let ty0 = self.local_ty(&f, 0)?;
            let size = self.size(ty0)?;
            if size != 0 {
                self.mem.copy(Pointer { alloc: r0, offset: 0 }, dst, size)?;
            }
        }
        for id in f.locals.iter().flatten() {
            self.mem.kill(*id);
        }
        self.jump_opt(f.ret.target)?;
        Ok(None)
    }

    // ---- Places ----

    fn place(&mut self, p: Place) -> R<'a, Pl> {
        let f = self.top()?;
        let row = f.body.place(p);
        if row.parent.is_none() {
            let i = row.local.0 as usize;
            let ty = self.local_ty(f, i)?;
            // A local read outside its storage is undefined behaviour.
            let id = f.locals.get(i).copied().flatten().ok_or(Stop::from(Refusal::Dangling))?;
            return Ok(Pl { ptr: Pointer { alloc: id, offset: 0 }, meta: None, ty, variant: 0 });
        }
        let body = f.body;
        self.proj_scratch.clear();
        let mut cur = row;
        while let (Some(parent), Some(proj)) = (cur.parent, cur.proj) {
            self.proj_scratch.push(proj);
            cur = body.place(parent);
        }
        let f = self.top()?;
        let i = cur.local.0 as usize;
        let ty = self.local_ty(f, i)?;
        let id = f.locals.get(i).copied().flatten().ok_or(Stop::from(Refusal::Dangling))?;
        let mut pl = Pl { ptr: Pointer { alloc: id, offset: 0 }, meta: None, ty, variant: 0 };
        while let Some(proj) = self.proj_scratch.pop() {
            pl = self.project(pl, proj)?;
        }
        Ok(pl)
    }

    fn project(&mut self, pl: Pl, proj: Proj) -> R<'a, Pl> {
        match proj {
            Proj::Field { index, ty } => {
                let fty = self.ground_ty(ty)?;
                let off = self.field_offset(pl.ty, pl.variant, index)?;
                // The unsized tail field keeps the place's metadata.
                let meta = if self.is_unsized(fty)? { pl.meta } else { None };
                Ok(Pl { ptr: pl.ptr.at(off), meta, ty: fty, variant: 0 })
            }
            Proj::Deref => match self.node(pl.ty)? {
                PNode::Ref(_, t) | PNode::Ptr(_, t) => self.deref(pl.ptr, t),
                _ => Err(malformed("a deref of a place that is no reference or pointer").into()),
            },
            Proj::DerefBox => {
                // `Box<T, A>`: its pointer is its first word.
                let PNode::Adt(_, args) = self.node(pl.ty)? else {
                    return Err(malformed("a box deref of a place that is no box").into());
                };
                let t = *self.ground.view().list(args).map_err(|_| malformed("a box's arguments"))?.first().ok_or(Stop::from(malformed("a box with no pointee")))?;
                self.deref(pl.ptr, t)
            }
            Proj::Index(l) => {
                let f = self.top()?;
                let id = f.locals.get(l.0 as usize).copied().flatten().ok_or(Stop::from(Refusal::Dangling))?;
                let i = self.uint(self.mem.read(Pointer { alloc: id, offset: 0 }, PTR)?)? as u64;
                self.index(pl, i)
            }
            Proj::ConstIndex { offset, min_length: _, from_end } => {
                let (_, len) = self.elem(pl)?;
                let i = if from_end { len - offset as u64 } else { offset as u64 };
                self.index(pl, i)
            }
            Proj::Subslice { from, to, from_end } => {
                let (elem, len) = self.elem(pl)?;
                let (from, to) = (from as u64, to as u64);
                let n = if from_end { len - to - from } else { to - from };
                let stride = self.size(elem)?;
                let ptr = pl.ptr.at(from * stride);
                match self.node(pl.ty)? {
                    PNode::Array(..) => {
                        let count = self.ground.usize_value(n).map_err(|_| malformed("a subslice's length"))?;
                        let ty = self.ground.intern(PNode::Array(elem, count)).map_err(|_| malformed("a subslice's type"))?;
                        Ok(Pl { ptr, meta: None, ty, variant: 0 })
                    }
                    _ => Ok(Pl { ptr, meta: Some(n), ty: pl.ty, variant: 0 }),
                }
            }
            Proj::Downcast(v) => Ok(Pl { variant: v, ..pl }),
        }
    }

    fn deref(&mut self, at: Pointer, pointee: PTy) -> R<'a, Pl> {
        let ptr = match self.mem.read(at, PTR)? {
            Scalar::Ptr(p) => p,
            Scalar::Int(_) => return Err(Refusal::IntAsPointer.into()),
        };
        let meta = if self.is_unsized(pointee)? { Some(self.uint(self.mem.read(at.at(PTR), PTR)?)? as u64) } else { None };
        Ok(Pl { ptr, meta, ty: pointee, variant: 0 })
    }

    /// The element type and length of an array or slice place.
    fn elem(&mut self, pl: Pl) -> R<'a, (PTy, u64)> {
        match self.node(pl.ty)? {
            PNode::Array(e, n) => Ok((e, self.ground.view().value_u64(n).ok_or(Stop::from(malformed("an array length that is no value")))?)),
            PNode::Slice(e) => Ok((e, pl.meta.ok_or(Stop::from(malformed("a slice place with no length")))?)),
            _ => Err(malformed("an index of a place that is no array or slice").into()),
        }
    }

    /// Element `i`; the bounds were checked by the body's `assert` (and memory refuses an
    /// access past the allocation regardless).
    fn index(&mut self, pl: Pl, i: u64) -> R<'a, Pl> {
        let (e, _) = self.elem(pl)?;
        let stride = self.size(e)?;
        Ok(Pl { ptr: pl.ptr.at(i * stride), meta: None, ty: e, variant: 0 })
    }

    fn len_of(&mut self, pl: Pl) -> R<'a, u64> {
        match self.node(pl.ty)? {
            PNode::Str => pl.meta.ok_or(Stop::from(malformed("a str place with no length"))),
            _ => Ok(self.elem(pl)?.1),
        }
    }

    // ---- Operands ----

    fn operand(&mut self, op: Operand) -> R<'a, Opnd> {
        match op {
            Operand::Copy(p) | Operand::Move(p) => {
                let pl = self.place(p)?;
                Ok(Opnd { src: Src::At(pl.ptr), ty: pl.ty })
            }
            Operand::Const(c) => self.konst(c),
        }
    }

    fn konst(&mut self, c: Const) -> R<'a, Opnd> {
        let body = self.top()?.body;
        let decl = body.konst(c);
        let ty = self.ground_ty(decl.ty)?;
        let src = match decl.kind {
            ConstKind::Scalar { bits } => Src::Imm(bits),
            // A fn item is zero-sized.
            ConstKind::FnItem { .. } => Src::Imm(0),
            ConstKind::Param(n) => {
                let a = *self.top()?.args.get(n as usize).ok_or(Stop::from(malformed("a const parameter past the frame's arguments")))?;
                match self.node(a)? {
                    PNode::ConstValue { bits, .. } => Src::Imm(bits),
                    _ => return Err(malformed("a const parameter whose argument is no value").into()),
                }
            }
            ConstKind::Bytes { .. } => {
                let item = self.top()?.item;
                let bytes = body.bytes(c).ok_or(Stop::from(malformed("a bytes constant with no bytes")))?;
                let data = match self.bytes_consts.iter().find(|&&(it, kc, _)| it == item && kc == c) {
                    Some(&(_, _, id)) => id,
                    None => {
                        let id = self.mem.allocate(bytes.len() as u64, 0, false, Origin::Data)?;
                        self.mem.fill(id, bytes, &[])?;
                        self.bytes_consts.push((item, c, id));
                        id
                    }
                };
                match self.node(ty)? {
                    PNode::Ref(_, t) | PNode::Ptr(_, t) => {
                        let unsized_ = self.is_unsized(t)?;
                        let tmp = self.mem.allocate(if unsized_ { 2 * PTR } else { PTR }, 3, true, Origin::Data)?;
                        let at = Pointer { alloc: tmp, offset: 0 };
                        self.mem.write(at, PTR, Scalar::Ptr(Pointer { alloc: data, offset: 0 }))?;
                        if unsized_ {
                            self.mem.write(at.at(PTR), PTR, Scalar::Int(bytes.len() as u128))?;
                        }
                        Src::Temp(tmp)
                    }
                    // An array by value.
                    _ => Src::At(Pointer { alloc: data, offset: 0 }),
                }
            }
            ConstKind::Item { key, args } => {
                let args = self.ground_list(args)?;
                Src::At(Pointer { alloc: self.load_const(key, args)?, offset: 0 })
            }
            ConstKind::Promoted(_) => return Err(unsupported("a promoted constant (its body is not asked of the world yet)").into()),
            ConstKind::Anon(_) => return Err(unsupported("an inline `const` block (its body is not asked of the world yet)").into()),
            ConstKind::StaticAddr(_) => return Err(unsupported("a static's address in a constant").into()),
        };
        Ok(Opnd { src, ty })
    }

    /// The value of constant `key` at `args`, from its record, in memory: one load per run.
    fn load_const(&mut self, key: ItemKey, args: List) -> R<'a, AllocId> {
        if let Some(&(_, _, id)) = self.consts.iter().find(|&&(k, a, _)| k == key && a == args) {
            return Ok(id);
        }
        let code = self.world.code_id(key);
        let inst = instance_key(code, args, self.ground.view(), self.alloc).map_err(build_refusal)?;
        let Ok(rec) = self.world.record(RecordKey::of(inst)) else {
            return Err(Stop::Need(Need { kind: NeedKind::Value, item: key, args }));
        };
        let mut ids: Vec<AllocId, Alloc<'a>> = Vec::new_in(self.alloc);
        for a in rec.data.allocs.iter() {
            ids.push(self.mem.allocate(a.bytes.len() as u64, a.align, false, Origin::Data)?);
        }
        for (k, a) in rec.data.allocs.iter().enumerate() {
            let mut relocs: Vec<(u64, AllocId), Alloc<'a>> = Vec::new_in(self.alloc);
            for r in a.relocs.iter() {
                let to = match r.target {
                    DataTarget::Local(n) => *ids.get(n as usize).ok_or(Stop::from(malformed("a record's relocation past its allocations")))?,
                    DataTarget::Fn(i) => self.mem.allocate(0, 0, false, Origin::Fn(i))?,
                    DataTarget::Static(_) => return Err(unsupported("a constant's value that points to a static").into()),
                    DataTarget::Vtable(_) => return Err(unsupported("a constant's value that holds a vtable").into()),
                };
                relocs.push((r.offset as u64, to));
            }
            let id = *ids.get(k).ok_or(Stop::from(malformed("a record's allocation past its ids")))?;
            self.mem.fill(id, &a.bytes, &relocs)?;
        }
        let id = *ids.first().ok_or(Stop::from(malformed("a record with no allocation")))?;
        self.consts.push((key, args, id));
        Ok(id)
    }

    fn release(&mut self, o: Opnd) {
        if let Src::Temp(id) = o.src {
            self.mem.kill(id);
        }
    }

    fn check_scalar_valid(&self, ty: PTy, s: Scalar) -> R<'a, ()> {
        match (self.node(ty)?, s) {
            (PNode::Bool, Scalar::Int(v)) if v > 1 => Err(Refusal::InvalidBool { value: v }.into()),
            (PNode::Bool, Scalar::Ptr(_)) => Err(Refusal::PointerAsInt.into()),
            (PNode::Char, Scalar::Int(v)) if u32::try_from(v).ok().and_then(char::from_u32).is_none() => {
                Err(Refusal::InvalidChar { value: v }.into())
            }
            (PNode::Char, Scalar::Ptr(_)) => Err(Refusal::PointerAsInt.into()),
            _ => Ok(()),
        }
    }

    fn read(&mut self, o: Opnd) -> R<'a, Scalar> {
        let size = self.size(o.ty)?;
        let s = match o.src {
            Src::At(p) => self.mem.read(p, size)?,
            Src::Temp(id) => self.mem.read(Pointer { alloc: id, offset: 0 }, size)?,
            Src::Imm(b) => Scalar::Int(b),
        };
        self.check_scalar_valid(o.ty, s)?;
        Ok(s)
    }

    fn copy_out(&mut self, o: Opnd, dst: Pointer, size: u64) -> R<'a, ()> {
        if size == 0 {
            return Ok(());
        }
        match o.src {
            Src::At(p) => self.mem.copy(p, dst, size)?,
            Src::Temp(id) => self.mem.copy(Pointer { alloc: id, offset: 0 }, dst, size)?,
            Src::Imm(b) => self.mem.write(dst, size, Scalar::Int(b))?,
        }
        Ok(())
    }

    fn operand_into(&mut self, op: Operand, dst: Pointer) -> R<'a, ()> {
        let o = self.operand(op)?;
        let res = (|| -> R<'a, ()> {
            if matches!(self.node(o.ty)?, PNode::Bool | PNode::Char) {
                let s = self.read(o)?;
                let size = self.size(o.ty)?;
                self.mem.write(dst, size, s)?;
                return Ok(());
            }
            let size = self.size(o.ty)?;
            self.copy_out(o, dst, size)
        })();
        self.release(o);
        res
    }

    fn uint(&self, s: Scalar) -> R<'a, u128> {
        match s {
            Scalar::Int(v) => Ok(v),
            Scalar::Ptr(_) => Err(Refusal::PointerAsInt.into()),
        }
    }

    /// An operand's bits as an integer.
    fn int(&mut self, op: Operand) -> R<'a, u128> {
        let o = self.operand(op)?;
        let s = self.read(o)?;
        self.release(o);
        self.uint(s)
    }

    /// `v` written at `d` in `d`'s size.
    fn put(&mut self, d: Pl, v: u128) -> R<'a, ()> {
        let size = self.size(d.ty)?;
        if size != 0 {
            self.mem.write(d.ptr, size, Scalar::Int(arith::mask(v, (size * 8) as u32)))?;
        }
        Ok(())
    }

    // ---- Values ----

    fn value(&mut self, d: Pl, v: Value) -> R<'a, ()> {
        match v {
            Value::Use(op) => self.operand_into(op, d.ptr),
            Value::Borrow { place, .. } | Value::RawAddr { place, .. } => {
                let p = self.place(place)?;
                self.mem.write(d.ptr, PTR, Scalar::Ptr(p.ptr))?;
                if let Some(m) = p.meta {
                    self.mem.write(d.ptr.at(PTR), PTR, Scalar::Int(m as u128))?;
                }
                Ok(())
            }
            Value::Binary { op, lhs, rhs } => {
                let (s, _) = self.binary(op, lhs, rhs, false)?;
                let size = self.size(d.ty)?;
                self.mem.write(d.ptr, size, s)?;
                Ok(())
            }
            Value::CheckedBinary { op, lhs, rhs } => {
                let (s, overflowed) = self.binary(op, lhs, rhs, true)?;
                self.write_checked(d, s, overflowed)
            }
            Value::Unary { op, operand } => self.unary(d, op, operand),
            Value::Cast { kind, operand, ty: _ } => self.cast(d, kind, operand),
            Value::Aggregate { kind, ty: _, operands } => {
                let body = self.top()?.body;
                match kind {
                    AggregateKind::Tuple => self.fields(d.ptr, d.ty, 0, body.ops(operands)),
                    AggregateKind::Adt(variant) => {
                        self.fields(d.ptr, d.ty, variant, body.ops(operands))?;
                        self.set_tag(d.ptr, d.ty, variant)
                    }
                    AggregateKind::Array => {
                        let (e, _) = self.elem(d)?;
                        let stride = self.size(e)?;
                        for (k, op) in body.ops(operands).enumerate() {
                            self.operand_into(op, d.ptr.at(k as u64 * stride))?;
                        }
                        Ok(())
                    }
                    AggregateKind::Closure => Err(unsupported("a closure built in a constant").into()),
                    AggregateKind::Coroutine => Err(unsupported("a coroutine built in a constant").into()),
                }
            }
            Value::Repeat { operand, count } => {
                let n = {
                    let o = self.konst(count)?;
                    let s = self.read(o)?;
                    self.release(o);
                    self.uint(s)? as u64
                };
                let (e, _) = self.elem(d)?;
                let stride = self.size(e)?;
                let o = self.operand(operand)?;
                let res = (|| -> R<'a, ()> {
                    // A long repeat is bounded as a loop is.
                    self.steps = self.steps.saturating_add(n);
                    if self.steps > self.limits.steps {
                        return Err(Refusal::StepLimit { limit: self.limits.steps }.into());
                    }
                    if n > 0 && stride != 0 {
                        let size = self.size(o.ty)?;
                        if matches!(self.node(o.ty)?, PNode::Bool | PNode::Char) {
                            let s = self.read(o)?;
                            for k in 0..n {
                                self.mem.write(d.ptr.at(k * stride), size, s)?;
                            }
                        } else {
                            for k in 0..n {
                                self.copy_out(o, d.ptr.at(k * stride), size)?;
                            }
                        }
                    }
                    Ok(())
                })();
                self.release(o);
                res
            }
            Value::Len(p) => {
                let pl = self.place(p)?;
                let n = self.len_of(pl)?;
                self.put(d, n as u128)
            }
            Value::Discriminant(p) => {
                let pl = self.place(p)?;
                let v = self.discriminant(pl)?;
                self.put(d, v as u128)
            }
            Value::SizeOf(t) => {
                let t = self.ground_ty(t)?;
                let (size, _) = self.size_align(t)?;
                self.put(d, size as u128)
            }
            Value::AlignOf(t) => {
                let t = self.ground_ty(t)?;
                let (_, align) = self.size_align(t)?;
                self.put(d, 1u128 << align)
            }
            Value::ThreadLocalAddr(_) => Err(unsupported("a thread-local's address in a constant").into()),
        }
    }

    /// A `(value, overflowed)` pair written at `d`, a two-field tuple.
    fn write_checked(&mut self, d: Pl, s: Scalar, overflowed: bool) -> R<'a, ()> {
        let PNode::Tuple(parts) = self.node(d.ty)? else {
            return Err(malformed("a checked operation into a place that is no pair").into());
        };
        let t0 = *self.ground.view().list(parts).map_err(|_| malformed("a pair's fields"))?.first().ok_or(Stop::from(malformed("a pair with no fields")))?;
        let (off0, off1) = (self.field_offset(d.ty, 0, 0)?, self.field_offset(d.ty, 0, 1)?);
        let size0 = self.size(t0)?;
        self.mem.write(d.ptr.at(off0), size0, s)?;
        self.mem.write(d.ptr.at(off1), 1, Scalar::Int(overflowed as u128))?;
        Ok(())
    }

    fn fields(&mut self, at: Pointer, ty: PTy, variant: u32, ops: impl Iterator<Item = Operand>) -> R<'a, ()> {
        for (k, op) in ops.enumerate() {
            let off = self.field_offset(ty, variant, k as u32)?;
            self.operand_into(op, at.at(off))?;
        }
        Ok(())
    }

    fn set_tag(&mut self, at: Pointer, ty: PTy, variant: u32) -> R<'a, ()> {
        let mut cx = LayoutCx { ground: &mut *self.ground, facts: self.world, scratch: self.alloc };
        if let Some(w) = self.layout.tag_write(ty, variant, &mut cx).map_err(layout_stop)? {
            self.mem.write(at.at(w.offset), w.size, Scalar::Int(w.value))?;
        }
        Ok(())
    }

    fn discriminant(&mut self, pl: Pl) -> R<'a, i128> {
        let PNode::Adt(adt, _) = self.node(pl.ty)? else {
            return Err(malformed("a discriminant of a place that is no enum").into());
        };
        let tag: Tag = {
            let mut cx = LayoutCx { ground: &mut *self.ground, facts: self.world, scratch: self.alloc };
            self.layout.tag(pl.ty, &mut cx).map_err(layout_stop)?
        };
        let bits = match tag.at {
            None => 0,
            Some((off, size)) => self.uint(self.mem.read(pl.ptr.at(off), size)?)?,
        };
        let variants = self.world.adt(adt).ok_or(Stop::from(Refusal::Missing { what: "an enum's definition", item: adt }))?.variants.len();
        let mut discrs: Vec<i128, Alloc<'a>> = Vec::new_in(self.alloc);
        for v in 0..variants as u32 {
            match eval_discriminant(self.world, adt, v, self.alloc) {
                Ok(Answer::Done(d)) => discrs.push(d),
                Ok(Answer::Need(n)) => return Err(Stop::Need(n)),
                Err(e) => return Err(e.refusal.into()),
            }
        }
        let v = tag.variant(bits, &discrs)?;
        discrs.get(v as usize).copied().ok_or(Stop::from(Refusal::InvalidTag { bits }))
    }

    fn unary(&mut self, d: Pl, op: UnOp, operand: Operand) -> R<'a, ()> {
        let o = self.operand(operand)?;
        let n = self.node(o.ty)?;
        if op == UnOp::PtrMetadata {
            // A pointer's metadata: its second word if the pointee is unsized, else `()`.
            let (PNode::Ref(_, t) | PNode::Ptr(_, t)) = n else {
                self.release(o);
                return Err(malformed("metadata of a value that is no pointer").into());
            };
            let res = if self.is_unsized(t)? {
                let p = match o.src {
                    Src::At(p) => p,
                    Src::Temp(id) => Pointer { alloc: id, offset: 0 },
                    Src::Imm(_) => {
                        self.release(o);
                        return Err(malformed("metadata of a pointer constant").into());
                    }
                };
                self.mem.copy(p.at(PTR), d.ptr, PTR).map_err(Stop::from)
            } else {
                Ok(())
            };
            self.release(o);
            return res;
        }
        let s = self.read(o)?;
        self.release(o);
        let v = self.uint(s)?;
        let r = match (op, n) {
            (UnOp::Not, PNode::Bool) => v ^ 1,
            (UnOp::Neg, PNode::Float(f)) => v ^ (1u128 << (float_bits(f) - 1)),
            (_, n) => {
                let (bits, _) = arith::int_info(n).ok_or(Stop::from(malformed("an operator on a type that is no integer")))?;
                // Neg wraps: the body checks `MIN` with an `overflow_neg` assert first.
                if op == UnOp::Not { arith::mask(!v, bits) } else { arith::mask(0u128.wrapping_sub(v), bits) }
            }
        };
        self.put(d, r)
    }

    fn cast(&mut self, d: Pl, kind: CastKind, operand: Operand) -> R<'a, ()> {
        let o = self.operand(operand)?;
        let (from, to) = (self.node(o.ty)?, self.node(d.ty)?);
        match kind {
            CastKind::IntToInt | CastKind::IntToFloat | CastKind::FloatToInt | CastKind::FloatToFloat => {
                let v = self.read(o)?;
                let v = self.uint(v)?;
                let r = arith::num_cast(from, to, v)?;
                self.put(d, r)?;
            }
            // Same bytes; a wide pointer cast to a thin one keeps its first word.
            CastKind::PtrToPtr
            | CastKind::MutToConstPointer
            | CastKind::UnsafeFnPointer
            | CastKind::FnPtrToPtr
            | CastKind::ArrayToPointer
            | CastKind::FromExposedAddr => {
                let size = self.size(d.ty)?;
                self.copy_out(o, d.ptr, size)?;
            }
            CastKind::ExposeAddr => {
                let v = self.read(o)?;
                let v = self.uint(v)?;
                self.put(d, v)?;
            }
            CastKind::Transmute => {
                let (fs, ts) = (self.size(o.ty)?, self.size(d.ty)?);
                if fs != ts {
                    self.release(o);
                    return Err(Refusal::TransmuteSize { from: fs, to: ts }.into());
                }
                self.copy_out(o, d.ptr, ts)?;
                if matches!(to, PNode::Bool | PNode::Char) {
                    let s = self.mem.read(d.ptr, ts)?;
                    if let Err(e) = self.check_scalar_valid(d.ty, s) {
                        self.release(o);
                        return Err(e);
                    }
                }
            }
            CastKind::Unsize => {
                let (PNode::Ref(_, s) | PNode::Ptr(_, s), PNode::Ref(_, t) | PNode::Ptr(_, t)) = (from, to) else {
                    return Err(unsupported("an unsizing of a value that is no reference or pointer, in a constant").into());
                };
                let (PNode::Array(_, n), PNode::Slice(_)) = (self.node(s)?, self.node(t)?) else {
                    return Err(unsupported("an unsizing to a trait object (a vtable) in a constant").into());
                };
                let len = self.ground.view().value_u64(n).ok_or(Stop::from(malformed("an array length that is no value")))?;
                self.copy_out(o, d.ptr, PTR)?;
                self.mem.write(d.ptr.at(PTR), PTR, Scalar::Int(len as u128))?;
            }
            CastKind::ReifyFnPointer => {
                let PNode::FnDef(k, l) = from else {
                    return Err(malformed("a fn pointer made of a value that is no fn item").into());
                };
                let (k, l) = self.resolve(k, l)?;
                let inst = instance_key(self.world.code_id(k), l, self.ground.view(), self.alloc).map_err(build_refusal)?;
                let f = self.mem.allocate(0, 0, false, Origin::Fn(inst))?;
                self.mem.write(d.ptr, PTR, Scalar::Ptr(Pointer { alloc: f, offset: 0 }))?;
            }
            CastKind::ClosureFnPointer => return Err(unsupported("a closure cast to a fn pointer in a constant").into()),
        }
        self.release(o);
        Ok(())
    }

    /// `lhs op rhs`: the result and whether it overflowed. A wrapping operator's result is
    /// the wrapped value; `checked` makes a shift past the width an overflow, not a refusal.
    fn binary(&mut self, op: BinOp, lhs: Operand, rhs: Operand, checked: bool) -> R<'a, (Scalar, bool)> {
        let (a, b) = (self.operand(lhs)?, self.operand(rhs)?);
        let (x, y) = (self.read(a)?, self.read(b)?);
        self.release(a);
        self.release(b);
        let (na, nb) = (self.node(a.ty)?, self.node(b.ty)?);
        match (x, y) {
            (Scalar::Int(x), Scalar::Int(y)) => self.arith(op, na, nb, x, y, checked),
            (Scalar::Ptr(p), Scalar::Int(n)) if op == BinOp::Offset => {
                let (PNode::Ref(_, t) | PNode::Ptr(_, t)) = na else {
                    return Err(malformed("an offset of a value that is no pointer").into());
                };
                let size = self.size(t)?;
                Ok((Scalar::Ptr(p.at((n as u64).wrapping_mul(size))), false))
            }
            (Scalar::Ptr(p), Scalar::Ptr(q)) => {
                // Two pointers compare only within one allocation in a constant.
                if p.alloc != q.alloc {
                    return Err(Refusal::PointerCompare.into());
                }
                let o = p.offset.cmp(&q.offset);
                let holds = holds(op, Some(o)).ok_or(Stop::from(Refusal::PointerAsInt))?;
                Ok((Scalar::Int(holds as u128), false))
            }
            _ => Err(Refusal::PointerAsInt.into()),
        }
    }

    fn arith(&mut self, op: BinOp, na: PNode, nb: PNode, x: u128, y: u128, checked: bool) -> R<'a, (Scalar, bool)> {
        if let PNode::Float(f) = na {
            if let Some(h) = holds(op, arith::float_cmp(f, x, y)) {
                return Ok((Scalar::Int(h as u128), false));
            }
            if op == BinOp::Cmp {
                return Err(malformed("a three-way compare of floats").into());
            }
            return Ok((Scalar::Int(arith::float_binop(typeck_op(op)?, f, x, y)?), false));
        }
        let (bits, signed) = arith::int_info(na).ok_or(Stop::from(malformed("arithmetic on a type that is no integer")))?;
        let rhs = arith::int_info(nb).ok_or(Stop::from(malformed("a right operand that is no integer")))?;
        let ord = if signed { arith::sext(x, bits).cmp(&arith::sext(y, bits)) } else { x.cmp(&y) };
        if let Some(h) = holds(op, Some(ord)) {
            return Ok((Scalar::Int(h as u128), false));
        }
        match op {
            // `Ordering` as `i8`; `put` masks it to the destination.
            BinOp::Cmp => Ok((Scalar::Int(match ord { Ordering::Less => 0xff, Ordering::Equal => 0, Ordering::Greater => 1 }), false)),
            BinOp::Add | BinOp::Sub | BinOp::Mul => match arith::int_binop(typeck_op(op)?, x, y, bits, signed, rhs, Plant::None) {
                Ok(v) => Ok((Scalar::Int(v), false)),
                Err(Refusal::Overflow { .. }) => {
                    let w = match op {
                        BinOp::Add => x.wrapping_add(y),
                        BinOp::Sub => x.wrapping_sub(y),
                        _ => x.wrapping_mul(y),
                    };
                    Ok((Scalar::Int(arith::mask(w, bits)), true))
                }
                Err(e) => Err(e.into()),
            },
            BinOp::Shl | BinOp::Shr => {
                let amount = if rhs.1 { arith::sext(y, rhs.0) } else { y as i128 };
                let far = amount < 0 || amount >= bits as i128;
                if far && !checked {
                    return Err(Refusal::ShiftTooFar { amount, bits }.into());
                }
                // A checked shift past the width shifts by the amount masked (`overflowing_shl`).
                let y = if far { y & (bits as u128 - 1) } else { y };
                Ok((Scalar::Int(arith::int_binop(typeck_op(op)?, x, y, bits, signed, rhs, Plant::None)?), far))
            }
            BinOp::Div | BinOp::Rem | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor => {
                Ok((Scalar::Int(arith::int_binop(typeck_op(op)?, x, y, bits, signed, rhs, Plant::None)?), false))
            }
            _ => Err(malformed("an operator with no integer meaning").into()),
        }
    }

    /// The intrinsics answered from the rows alone; the rest wait for the lift of
    /// `Machine::intrinsic` (see the report).
    fn intrinsic(&mut self, i: Intrinsic, iargs: List, args: Ops, d: Option<Pl>) -> R<'a, ()> {
        let body = self.top()?.body;
        let mut ops = body.ops(args);
        let op0 = ops.next();
        let op1 = ops.next();
        let need_dest = |d: Option<Pl>| d.ok_or(Stop::from(malformed("an intrinsic call with no destination")));
        let two = || match (op0, op1) {
            (Some(a), Some(b)) => Ok((a, b)),
            _ => Err(Stop::from(malformed("a two-operand intrinsic without two operands"))),
        };
        match i {
            Intrinsic::SizeOf | Intrinsic::AlignOf => {
                let t = *self.ground.view().list(iargs).map_err(|_| malformed("an intrinsic's arguments"))?.first().ok_or(Stop::from(malformed("`size_of` with no type")))?;
                let (size, align) = self.size_align(t)?;
                let v = if i == Intrinsic::SizeOf { size as u128 } else { 1u128 << align };
                self.put(need_dest(d)?, v)
            }
            // `true` in a constant, as the old evaluator answers.
            Intrinsic::UbChecks => self.put(need_dest(d)?, 1),
            Intrinsic::Likely | Intrinsic::Unlikely | Intrinsic::BlackBox => {
                let a = op0.ok_or(Stop::from(malformed("a one-operand intrinsic with none")))?;
                self.operand_into(a, need_dest(d)?.ptr)
            }
            Intrinsic::Assume => {
                let a = op0.ok_or(Stop::from(malformed("`assume` with no argument")))?;
                if self.int(a)? == 0 {
                    return Err(Refusal::Unreachable.into());
                }
                Ok(())
            }
            Intrinsic::ColdPath => Ok(()),
            Intrinsic::Unreachable => Err(Refusal::Unreachable.into()),
            Intrinsic::AbortImmediate => Err(Stop::Fault(Fault::Abort)),
            Intrinsic::WrappingAdd | Intrinsic::WrappingSub | Intrinsic::WrappingMul => {
                let (a, b) = two()?;
                let op = match i {
                    Intrinsic::WrappingAdd => BinOp::Add,
                    Intrinsic::WrappingSub => BinOp::Sub,
                    _ => BinOp::Mul,
                };
                let (s, _) = self.binary(op, a, b, true)?;
                let d = need_dest(d)?;
                let size = self.size(d.ty)?;
                self.mem.write(d.ptr, size, s)?;
                Ok(())
            }
            Intrinsic::AddWithOverflow | Intrinsic::SubWithOverflow | Intrinsic::MulWithOverflow => {
                let (a, b) = two()?;
                let op = match i {
                    Intrinsic::AddWithOverflow => BinOp::Add,
                    Intrinsic::SubWithOverflow => BinOp::Sub,
                    _ => BinOp::Mul,
                };
                let (s, o) = self.binary(op, a, b, true)?;
                self.write_checked(need_dest(d)?, s, o)
            }
            _ => Err(unsupported("an intrinsic not lifted from blaze-consteval's `Machine::intrinsic` yet").into()),
        }
    }

    // ---- The value ----

    /// The root's `_0` and every allocation it reaches, as a data record: allocation 0 is the
    /// value. A pointer into a local's storage (other than the value's own) dangles once the
    /// constant is evaluated, and is refused.
    fn intern(&mut self, root: AllocId) -> R<'a, DataRecord<Alloc<'a>>> {
        let mut order: Vec<AllocId, Alloc<'a>> = Vec::new_in(self.alloc);
        order.push(root);
        // blaze-build's data form holds the standard vector (data.rs), so the record is built in
        // it directly, in the same allocator: no copy from blaze-own's.
        let mut allocs: alloc::vec::Vec<DataAlloc<Alloc<'a>>, Alloc<'a>> = alloc::vec::Vec::new_in(self.alloc);
        let mut k = 0;
        while let Some(&id) = order.get(k) {
            k += 1;
            let align = self.mem.get(id)?.align_log2;
            let mut bytes: alloc::vec::Vec<u8, Alloc<'a>> = alloc::vec::Vec::new_in(self.alloc);
            bytes.extend_from_slice(self.mem.bytes(id)?);
            self.mem.zero_unwritten(id, &mut bytes);
            let mut relocs: alloc::vec::Vec<DataReloc, Alloc<'a>> = alloc::vec::Vec::new_in(self.alloc);
            for &(offset, to) in self.mem.relocs(id)? {
                let target = match self.mem.get(to)?.origin {
                    Origin::Static(s) => DataTarget::Static(s),
                    Origin::Fn(i) => DataTarget::Fn(i),
                    Origin::Local if to != root => return Err(Refusal::Dangling.into()),
                    _ => {
                        let n = match order.iter().position(|&x| x == to) {
                            Some(n) => n,
                            None => {
                                order.push(to);
                                order.len() - 1
                            }
                        };
                        DataTarget::Local(n as u32)
                    }
                };
                relocs.push(DataReloc { offset: offset as u32, target });
            }
            allocs.push(DataAlloc { bytes, align, mutable: false, relocs });
        }
        Ok(DataRecord { allocs })
    }

    // ---- Types and layouts ----

    fn ground_ty(&mut self, ty: PTy) -> R<'a, PTy> {
        let f = self.frames.last().ok_or(Stop::from(malformed("a type grounded with no frame")))?;
        Ok(ground_in(f.body, f.types, &f.args, ty, &mut *self.ground, self.alloc, self.world)?)
    }

    /// A list the frame's body names (its record's or its overlay's), grounded and interned in
    /// the ground table.
    fn ground_list(&mut self, l: List) -> R<'a, List> {
        let f = self.frames.last().ok_or(Stop::from(malformed("a list grounded with no frame")))?;
        let mut cx = GroundCx { body: f.body, from: f.types, args: &f.args, g: &mut *self.ground, scratch: self.alloc, facts: self.world };
        Ok(ground_own_list(&mut cx, l, 0)?)
    }

    fn node(&self, ty: PTy) -> R<'a, PNode> {
        Ok(self.ground.view().node(ty).map_err(|_| malformed("a type past the ground table"))?)
    }

    fn is_unsized(&mut self, ty: PTy) -> R<'a, bool> {
        Ok(blaze_consteval::layout::is_unsized(self.world, &mut *self.ground, ty, self.alloc)?)
    }

    fn size_align(&mut self, ty: PTy) -> R<'a, (u64, u8)> {
        let mut cx = LayoutCx { ground: &mut *self.ground, facts: self.world, scratch: self.alloc };
        self.layout.size_align(ty, &mut cx).map_err(layout_stop)
    }

    fn size(&mut self, ty: PTy) -> R<'a, u64> {
        Ok(self.size_align(ty)?.0)
    }

    fn field_offset(&mut self, ty: PTy, variant: u32, field: u32) -> R<'a, u64> {
        let mut cx = LayoutCx { ground: &mut *self.ground, facts: self.world, scratch: self.alloc };
        self.layout.field_offset(ty, variant, field, &mut cx).map_err(layout_stop)
    }
}

/// Whether `op`, a comparison, holds for `ord` (`None`: unordered, a NaN); `None` for an
/// operator that is no comparison.
fn holds(op: BinOp, ord: Option<Ordering>) -> Option<bool> {
    Some(match op {
        BinOp::Eq => ord == Some(Ordering::Equal),
        BinOp::Ne => ord != Some(Ordering::Equal),
        BinOp::Lt => ord == Some(Ordering::Less),
        BinOp::Le => matches!(ord, Some(Ordering::Less | Ordering::Equal)),
        BinOp::Gt => ord == Some(Ordering::Greater),
        BinOp::Ge => matches!(ord, Some(Ordering::Greater | Ordering::Equal)),
        _ => return None,
    })
}

fn float_bits(f: FloatTy) -> u32 {
    match f {
        FloatTy::F16 => 16,
        FloatTy::F32 => 32,
        FloatTy::F64 => 64,
        FloatTy::F128 => 128,
    }
}

/// The checker's operator for the body's: the one the arithmetic rules take.
fn typeck_op(op: BinOp) -> Result<blaze_typeck::BinOp, Refusal<'static>> {
    use blaze_typeck::BinOp as T;
    Ok(match op {
        BinOp::Add => T::Add,
        BinOp::Sub => T::Sub,
        BinOp::Mul => T::Mul,
        BinOp::Div => T::Div,
        BinOp::Rem => T::Rem,
        BinOp::BitAnd => T::BitAnd,
        BinOp::BitOr => T::BitOr,
        BinOp::BitXor => T::BitXor,
        BinOp::Shl => T::Shl,
        BinOp::Shr => T::Shr,
        BinOp::Eq => T::Eq,
        BinOp::Ne => T::Ne,
        BinOp::Lt => T::Lt,
        BinOp::Le => T::Le,
        BinOp::Gt => T::Gt,
        BinOp::Ge => T::Ge,
        BinOp::Cmp | BinOp::Offset => return Err(malformed("an operator the arithmetic rules do not take")),
    })
}

/// The operator an overflow assert names, as the old evaluator's refusal names it.
fn op_of(op: BinOp) -> Result<Op, Refusal<'static>> {
    Ok(match op {
        BinOp::Add => Op::Add,
        BinOp::Sub => Op::Sub,
        BinOp::Mul => Op::Mul,
        BinOp::Div => Op::Div,
        BinOp::Rem => Op::Rem,
        BinOp::Shl => Op::Shl,
        BinOp::Shr => Op::Shr,
        _ => return Err(malformed("an overflow assert of an operator that cannot overflow")),
    })
}
