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
use blaze_build::vtable::{VtableKey, VtableRef, vtable_key};
use blaze_build::{BuildError, FnTrait, Instance, InstanceKey, closure_method_key_args, ground, instance_key};
use blaze_consteval::arith::{self, Plant};
use blaze_consteval::layout::{Stop as LayoutStop, Tag};
use blaze_consteval::mem::{AllocId, Memory, Origin, PTR, Pointer, Scalar};
use blaze_consteval::{
    Answer, ConstFn, ConstRecord, ConstVtable, DataAlloc, DataRecord, DataReloc, DataTarget, FnKind,
    FnTraitMethod, Intrinsic, Layout, LayoutCx, LayoutFacts, Limits, Need, NeedKind, Op, RecordKey, Refusal,
    eval_discriminant,
};
use blaze_own::{Alloc, Vec};
use blaze_typeck::{AdtKind, ExprId, FloatTy, ItemKey, List, Mutbl, PNode, PTy, PTys, PortableTys};

use crate::error::{EvalError, Fault, Row};
use crate::world::{Bodied, BodyWorld, DropGlue};

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
        promoted: Vec::new_in(alloc),
        anons: Vec::new_in(alloc),
        statics: Vec::new_in(alloc),
        fns: Vec::new_in(alloc),
        vtables: Vec::new_in(alloc),
        type_names: Vec::new_in(alloc),
        type_ids: Vec::new_in(alloc),
        bytes_consts: Vec::new_in(alloc),
        proj_scratch: Vec::new_in(alloc),
        body_live: Vec::new_in(alloc),
        live_bits: Vec::new_in(alloc),
        inst_tys: Vec::new_in(alloc),
        cached_tys: Vec::new_in(alloc),
        min_align: Vec::new_in(alloc),
        limits,
        steps: 0,
        alloc,
        at: None,
    };
    match m.run(item, args) {
        Ok(rec) => Ok(Answer::Done(rec)),
        Err(Stop::Need(n)) => Ok(Answer::Need(n)),
        Err(Stop::Fault(fault)) => Err(EvalError { constant: item, at: m.at, fault }),
        Err(Stop::SubFrame) => Err(EvalError {
            constant: item,
            at: m.at,
            fault: Fault::Refused(malformed("unhandled sub-body frame transition")),
        }),
    }
}

/// How a step stopped the run.
pub(crate) enum Stop<'a> {
    Fault(Fault<'a>),
    Need(Need),
    /// A sub-body (`Promoted` / `Anon`) was pushed onto the frame stack; resume the main loop
    /// without advancing the caller's row so it retries once the sub-body caches its value.
    SubFrame,
}

impl<'a, 'b: 'a> From<Refusal<'b>> for Stop<'a> {
    fn from(r: Refusal<'b>) -> Stop<'a> {
        Stop::Fault(Fault::Refused(r))
    }
}

pub(crate) type R<'a, T> = Result<T, Stop<'a>>;

pub(crate) fn malformed(what: &'static str) -> Refusal<'static> {
    Refusal::Malformed { what }
}

pub(crate) fn unsupported(what: &'static str) -> Refusal<'static> {
    Refusal::Unsupported { what }
}

pub(crate) fn layout_stop<'a>(s: LayoutStop) -> Stop<'a> {
    match s {
        LayoutStop::Refused(r) => r.into(),
        // An explicit discriminant's constant: its record first.
        LayoutStop::Need(item) => Stop::Need(Need { kind: NeedKind::Value, item, args: List::EMPTY }),
    }
}

pub(crate) fn build_refusal(e: BuildError) -> Refusal<'static> {
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

/// Identity of a verified body for caching `StorageLive` scans and grounded local types.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BodyId {
    Item(ItemKey),
    Promoted(ItemKey, ExprId),
    Anon(ItemKey, ExprId),
}

/// What a frame returns to when it hits `StmtKind::Return`.
#[derive(Clone, Copy)]
enum Ret {
    Call { dest: Option<Pointer>, target: Option<Block> },
    Promoted { owner: ItemKey, borrow: ExprId, args: List },
    Anon { owner: ItemKey, block: ExprId, args: List },
}

/// One activation.
pub(crate) struct Frame<'w, 'a> {
    pub(crate) item: ItemKey,
    pub(crate) body: Verified<'w>,
    pub(crate) types: PTys<'w>,
    /// The ground arguments list handle in `ground`.
    pub(crate) args_list: List,
    /// The ground arguments, a copy of the list `ground` holds.
    pub(crate) args: Vec<PTy, Alloc<'a>>,
    /// Slice `(start, len)` in `Machine::cached_tys` holding each local's ground type.
    pub(crate) tys: (u32, u32),
    /// Each local's allocation while it is live.
    pub(crate) locals: Vec<Option<AllocId>, Alloc<'a>>,
    /// The row run next.
    pub(crate) at: Stmt,
    ret: Ret,
}

/// A place: where it is, its metadata if unsized (slice length or vtable pointer), its ground
/// type, the variant a downcast selected (0 otherwise).
#[derive(Clone, Copy)]
pub(crate) struct Pl {
    pub(crate) ptr: Pointer,
    pub(crate) meta: Option<Scalar>,
    pub(crate) ty: PTy,
    pub(crate) variant: u32,
}

/// An operand's value: in memory (a place's bytes, or a temporary made for a constant), or
/// a scalar constant's bits.
#[derive(Clone, Copy)]
pub(crate) enum Src {
    At(Pointer),
    Temp(AllocId),
    Imm(u128),
}

#[derive(Clone, Copy)]
pub(crate) struct Opnd {
    pub(crate) src: Src,
    pub(crate) ty: PTy,
}

#[derive(Clone, Copy)]
pub(crate) struct StaticSlot {
    pub(crate) id: AllocId,
    pub(crate) loaded: bool,
    pub(crate) mut_static: bool,
}

#[derive(Clone, Copy)]
pub(crate) enum TypeIdTarget {
    Ty(PTy),
    Field { base: PTy, variant: u32, field: u32 },
}

pub(crate) struct Machine<'w, 'g, 't, 'a, W: BodyWorld, L: Layout> {
    pub(crate) world: &'w W,
    pub(crate) layout: &'g mut L,
    pub(crate) ground: &'g mut PortableTys<'t>,
    pub(crate) mem: Memory<Alloc<'a>>,
    pub(crate) frames: Vec<Frame<'w, 'a>, Alloc<'a>>,
    /// Constants' records loaded into memory this run, by item and ground arguments: a
    /// constant read in a loop is loaded once (rule 77).
    pub(crate) consts: Vec<(ItemKey, List, AllocId), Alloc<'a>>,
    /// Promoted borrows cached by `(owner, borrow, args)`: `(data_alloc, body_ret_ty)`.
    /// `body_ret_ty` is `Some(t)` when produced by a promoted body so `konst` knows whether `_0`
    /// already holds the reference or the pointee value; `None` for a `promoted_record`.
    pub(crate) promoted: Vec<(ItemKey, ExprId, List, AllocId, Option<PTy>), Alloc<'a>>,
    /// Inline `const { .. }` blocks cached by `(owner, block, args)`.
    pub(crate) anons: Vec<(ItemKey, ExprId, List, AllocId), Alloc<'a>>,
    /// Static allocations cached by item, tracking load status and `static mut`.
    pub(crate) statics: Vec<(ItemKey, StaticSlot), Alloc<'a>>,
    /// Deduplicated fn pointer allocations and their known `ConstFn` targets.
    pub(crate) fns: Vec<(InstanceKey, Option<ConstFn>, AllocId), Alloc<'a>>,
    /// Deduplicated vtable allocations and their known `ConstVtable` metadata.
    pub(crate) vtables: Vec<(VtableKey, Option<ConstVtable>, AllocId), Alloc<'a>>,
    /// Cached `type_name::<T>()` data allocations.
    pub(crate) type_names: Vec<(PTy, AllocId, u64), Alloc<'a>>,
    /// Recorded `TypeId` and `FieldRepresentingType` hashes for reflection intrinsics.
    pub(crate) type_ids: Vec<(u128, TypeIdTarget), Alloc<'a>>,
    /// Byte-string / string literal data allocations cached by `(item, const_id)` so loops
    /// never leak `Origin::Data` allocations.
    pub(crate) bytes_consts: Vec<(ItemKey, Const, AllocId), Alloc<'a>>,
    /// Reused projection scratch stack for `place()` (zero per-call heap allocations).
    pub(crate) proj_scratch: Vec<Proj, Alloc<'a>>,
    /// Cached per-body `StorageLive` scan: `(body_id, start, len)` indexing `live_bits`.
    body_live: Vec<(BodyId, u32, u32), Alloc<'a>>,
    live_bits: Vec<bool, Alloc<'a>>,
    /// Cached per-instance grounded local types: `(body_id, args, start, len)` indexing `cached_tys`.
    inst_tys: Vec<(BodyId, List, u32, u32), Alloc<'a>>,
    cached_tys: Vec<PTy, Alloc<'a>>,
    /// Inherited minimum log2 alignments for allocations written whole from a higher-aligned
    /// base place (e.g. promoted `&ALIGNED_BYTES.bytes` inheriting the base struct's alignment).
    min_align: Vec<(AllocId, u8), Alloc<'a>>,
    pub(crate) limits: Limits,
    pub(crate) steps: u64,
    pub(crate) alloc: Alloc<'a>,
    /// The row running: what an error names.
    pub(crate) at: Option<Row>,
}

impl<'w, 'g, 't, 'a, W: BodyWorld, L: Layout> Machine<'w, 'g, 't, 'a, W, L> {
    fn run(&mut self, item: ItemKey, args: List) -> R<'a, DataRecord<Alloc<'a>>> {
        let root = self.frame(item, args, Ret::Call { dest: None, target: None })?;
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
            match self.step(row.kind) {
                Ok(Some(done)) => return Ok(done),
                Ok(None) | Err(Stop::SubFrame) => {}
                Err(e) => return Err(e),
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

    pub(crate) fn top(&self) -> R<'a, &Frame<'w, 'a>> {
        self.frames.last().ok_or(Stop::from(malformed("a row run with no frame")))
    }

    fn top_mut(&mut self) -> R<'a, &mut Frame<'w, 'a>> {
        self.frames.last_mut().ok_or(Stop::from(malformed("a row run with no frame")))
    }

    // ---- Checked memory helpers (static load & mut_static guards) ----

    pub(crate) fn is_mut_static_alloc(&self, id: AllocId) -> bool {
        self.statics.iter().any(|(_, s)| s.id == id && s.mut_static)
    }

    pub(crate) fn ensure_loaded(&self, id: AllocId) -> R<'a, ()> {
        if let Some(&(item, slot)) = self.statics.iter().find(|(_, s)| s.id == id) {
            if slot.mut_static {
                return Err(Refusal::MutableStatic { item }.into());
            }
            if !slot.loaded {
                return Err(Stop::Need(Need { kind: NeedKind::Value, item, args: List::EMPTY }));
            }
        }
        Ok(())
    }

    pub(crate) fn alloc_len(&self, id: AllocId) -> R<'a, u64> {
        self.ensure_loaded(id)?;
        let bytes = self.mem.bytes(id)?;
        u64::try_from(bytes.len()).map_err(|_| malformed("allocation length past u64").into())
    }

    pub(crate) fn mem_read(&self, p: Pointer, size: u64) -> R<'a, Scalar> {
        self.ensure_loaded(p.alloc)?;
        Ok(self.mem.read(p, size)?)
    }

    pub(crate) fn mem_read_bytes(&self, p: Pointer, size: u64) -> R<'a, &[u8]> {
        self.ensure_loaded(p.alloc)?;
        Ok(self.mem.read_bytes(p, size)?)
    }

    pub(crate) fn mem_write(&mut self, p: Pointer, size: u64, v: Scalar) -> R<'a, ()> {
        self.ensure_loaded(p.alloc)?;
        Ok(self.mem.write(p, size, v)?)
    }

    /// Effective log2 alignment of `id`: its own allocation alignment, or a higher alignment
    /// inherited from a base-struct field copy (e.g. promoted `&ALIGNED_BYTES.bytes`).
    pub(crate) fn effective_align_log2(&self, id: AllocId) -> R<'a, u8> {
        let base = self.mem.get(id)?.align_log2;
        let extra = self.min_align.iter().find(|&&(a, _)| a == id).map_or(0, |&(_, al)| al);
        Ok(base.max(extra))
    }

    /// Guaranteed log2 byte alignment of `ptr` within its allocation: the allocation's effective
    /// alignment when `ptr.offset == 0`, otherwise capped by the lowest set bit of `ptr.offset`.
    pub(crate) fn place_align_log2(&self, ptr: Pointer) -> R<'a, u8> {
        let src = self.effective_align_log2(ptr.alloc)?;
        Ok(if ptr.offset == 0 {
            src
        } else {
            src.min(u8::try_from(ptr.offset.trailing_zeros()).unwrap_or(u8::MAX))
        })
    }

    /// Preserves `src`'s base-place alignment on `dst` when a whole allocation is overwritten
    /// at offset 0.
    fn inherit_align(&mut self, src: Pointer, dst: Pointer, size: u64) -> R<'a, ()> {
        if dst.offset != 0 || size != self.alloc_len(dst.alloc)? {
            return Ok(());
        }
        let align = self.place_align_log2(src)?;
        let base = self.mem.get(dst.alloc)?.align_log2;
        if align <= base {
            return Ok(());
        }
        if let Some((_, cur)) = self.min_align.iter_mut().find(|(a, _)| *a == dst.alloc) {
            *cur = (*cur).max(align);
        } else {
            self.min_align.push((dst.alloc, align));
        }
        Ok(())
    }

    pub(crate) fn mem_copy(&mut self, src: Pointer, dst: Pointer, size: u64) -> R<'a, ()> {
        self.ensure_loaded(src.alloc)?;
        self.ensure_loaded(dst.alloc)?;
        self.mem.copy(src, dst, size)?;
        self.inherit_align(src, dst, size)
    }

    pub(crate) fn mem_deinit(&mut self, p: Pointer, size: u64) -> R<'a, ()> {
        self.ensure_loaded(p.alloc)?;
        Ok(self.mem.deinit(p, size)?)
    }

    pub(crate) fn mem_memset(&mut self, p: Pointer, size: u64, byte: u8) -> R<'a, ()> {
        self.ensure_loaded(p.alloc)?;
        Ok(self.mem.memset(p, size, byte)?)
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
                self.mem_deinit(d.ptr, size)?;
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
            StmtKind::TailCall { callee, args } => return self.tail_call(callee, args),
            // Undefined behaviour reached (the Reference, "Behavior considered undefined").
            StmtKind::Unreachable => return Err(Refusal::Unreachable.into()),
            StmtKind::Resume => return Err(malformed("`resume` reached: the evaluator follows no unwind edge").into()),
            StmtKind::Terminate => return Err(Stop::Fault(Fault::Abort)),
            StmtKind::Suspend { .. } | StmtKind::Cancelled => {
                return Err(unsupported("a suspension (a coroutine's) in a constant").into());
            }
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
                let len = u64::try_from(self.int(len)?).map_err(|_| malformed("a bounds assert length past u64"))?;
                let index = u64::try_from(self.int(index)?).map_err(|_| malformed("a bounds assert index past u64"))?;
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
        let b = match self.world.verified(item) {
            Ok(Some(b)) => b,
            Ok(None) => return Err(Refusal::NotConstFn { item }.into()),
            Err(_) => return Err(Stop::Need(Need { kind: NeedKind::Body, item, args })),
        };
        self.frame_from_bodied(BodyId::Item(item), b, args, ret)
    }

    fn frame_from_bodied(&mut self, body_id: BodyId, b: Bodied<'w>, args: List, ret: Ret) -> R<'a, Frame<'w, 'a>> {
        if self.frames.len() >= usize::try_from(self.limits.depth).unwrap_or(usize::MAX) {
            return Err(Refusal::CallDepth { limit: self.limits.depth }.into());
        }
        let mut argv: Vec<PTy, Alloc<'a>> = Vec::new_in(self.alloc);
        argv.extend_from_slice(self.ground.view().list(args).map_err(|_| malformed("a call's arguments past the ground table"))?);
        let n = usize::try_from(b.body.locals()).map_err(|_| malformed("local count overflow"))?;
        // Scan StorageLive statements once per body (avoids O(stmts) scan on every recursive call).
        let (live_start, live_len) = match self.body_live.iter().find(|&&(k, _, _)| k == body_id) {
            Some(&(_, s, l)) => (usize::try_from(s).unwrap_or(0), usize::try_from(l).unwrap_or(0)),
            None => {
                let s = self.live_bits.len();
                self.live_bits.resize(s + n, false);
                for st in 0..b.body.stmts() {
                    if let StmtKind::StorageLive(l) = b.body.stmt(Stmt(st)).kind
                        && let Ok(li) = usize::try_from(l.0)
                        && let Some(bit) = self.live_bits.get_mut(s + li)
                    {
                        *bit = true;
                    }
                }
                let s32 = u32::try_from(s).map_err(|_| malformed("live_bits overflow"))?;
                let n32 = u32::try_from(n).map_err(|_| malformed("locals overflow"))?;
                self.body_live.push((body_id, s32, n32));
                (s, n)
            }
        };
        // Ground local types once per `(body_id, args)` instance.
        let (tys_start, tys_len) = match self.inst_tys.iter().find(|&&(k, a, _, _)| k == body_id && a == args) {
            Some(&(_, _, s, l)) => (s, l),
            None => {
                let s = u32::try_from(self.cached_tys.len()).map_err(|_| malformed("cached_tys overflow"))?;
                for l in 0..b.body.locals() {
                    let decl = b.body.local(Local(l));
                    let gty = ground_in(b.body, b.types, &argv, decl.ty, &mut *self.ground, self.alloc, self.world)?;
                    self.cached_tys.push(gty);
                }
                let n32 = b.body.locals();
                self.inst_tys.push((body_id, args, s, n32));
                (s, n32)
            }
        };
        let tys_start_usize = usize::try_from(tys_start).unwrap_or(0);
        let mut locals: Vec<Option<AllocId>, Alloc<'a>> = Vec::with_capacity_in(n, self.alloc);
        for l in 0..n {
            let needs_live = l < live_len && self.live_bits.get(live_start + l).copied().unwrap_or(false);
            if l < live_len && !needs_live {
                let Some(&ty) = self.cached_tys.get(tys_start_usize + l) else {
                    return Err(malformed("a local past the frame").into());
                };
                match self.allocate_local(ty) {
                    Ok(id) => locals.push(Some(id)),
                    Err(e) => {
                        for lid in locals.iter().flatten() {
                            self.mem.kill(*lid);
                        }
                        return Err(e);
                    }
                }
            } else {
                locals.push(None);
            }
        }
        Ok(Frame {
            item: b.item,
            body: b.body,
            types: b.types,
            args_list: args,
            args: argv,
            tys: (tys_start, tys_len),
            locals,
            at: b.body.entry(),
            ret,
        })
    }

    fn local_ty(&self, f: &Frame<'w, 'a>, i: usize) -> R<'a, PTy> {
        if i >= usize::try_from(f.tys.1).unwrap_or(0) {
            return Err(malformed("a local past the frame").into());
        }
        let base = usize::try_from(f.tys.0).unwrap_or(0);
        self.cached_tys
            .get(base + i)
            .copied()
            .ok_or_else(|| malformed("a local past the frame").into())
    }

    fn allocate_local(&mut self, ty: PTy) -> R<'a, AllocId> {
        let (size, align) = self.size_align(ty)?;
        Ok(self.mem.allocate(size, align, true, Origin::Local)?)
    }

    fn storage_live(&mut self, l: Local) -> R<'a, ()> {
        let idx = usize::try_from(l.0).map_err(|_| malformed("a local past the frame"))?;
        let f = self.top()?;
        let ty = self.local_ty(f, idx)?;
        let id = self.allocate_local(ty)?;
        let slot = self
            .top_mut()?
            .locals
            .get_mut(idx)
            .ok_or(Stop::from(malformed("a local past the frame")))?;
        *slot = Some(id);
        Ok(())
    }

    fn storage_dead(&mut self, l: Local) -> R<'a, ()> {
        let idx = usize::try_from(l.0).map_err(|_| malformed("a local past the frame"))?;
        let slot = self.top_mut()?.locals.get_mut(idx).ok_or(Stop::from(malformed("a local past the frame")))?;
        if let Some(id) = slot.take() {
            self.mem.kill(id);
        }
        Ok(())
    }

    fn callee_of_value(&mut self, op: Operand) -> R<'a, (ItemKey, List)> {
        let o = self.operand(op)?;
        let res = match self.node(o.ty)? {
            // A fn item's value is zero-sized: its type names the callee.
            PNode::FnDef(k, l) => Ok((k, l)),
            PNode::FnPtr(..) => match self.read(o)? {
                Scalar::Ptr(p) => {
                    if p.offset != 0 {
                        Err(unsupported("a call through an offset fn pointer in a constant").into())
                    } else if let Origin::Fn(inst) = self.mem.get(p.alloc)?.origin {
                        match self.fns.iter().find(|&&(k, _, _)| k == inst).and_then(|&(_, cf, _)| cf) {
                            Some(cf) => Ok((cf.instance.item, cf.instance.args)),
                            None => Err(unsupported("a call through a fn pointer whose instance is not known").into()),
                        }
                    } else {
                        Err(unsupported("a call through a pointer that is not a fn pointer in a constant").into())
                    }
                }
                Scalar::Int(_) => Err(Refusal::IntAsPointer.into()),
            },
            _ => Err(unsupported("a call through a fn pointer in a constant").into()),
        };
        self.release(o);
        res
    }

    fn call(&mut self, callee: CalleeId, args: Ops, dest: Option<Place>, target: Option<Block>) -> R<'a, ()> {
        let body = self.top()?.body;
        let (key, gargs) = match body.callee(callee) {
            Callee::FnItem { key, args } | Callee::Method { key, args } | Callee::Intrinsic { key, args } => {
                (key, self.ground_list(args)?)
            }
            Callee::Value(op) => self.callee_of_value(op)?,
        };
        // The same two questions the old evaluator asks (machine/code.rs `call`).
        let (item, iargs) = self.resolve(key, gargs)?;
        let d = match dest {
            Some(p) => Some(self.place(p)?),
            None => None,
        };
        match self.world.fn_kind(item) {
            FnKind::Const => {
                let f = self.frame(item, iargs, Ret::Call { dest: d.map(|d| d.ptr), target })?;
                let mut k = 1usize;
                for op in body.ops(args) {
                    let Some(id) = f.locals.get(k).copied().flatten() else {
                        for lid in f.locals.iter().flatten() {
                            self.mem.kill(*lid);
                        }
                        return Err(malformed("a call with more arguments than its callee takes").into());
                    };
                    if let Err(e) = self.operand_into(op, Pointer { alloc: id, offset: 0 }) {
                        for lid in f.locals.iter().flatten() {
                            self.mem.kill(*lid);
                        }
                        return Err(e);
                    }
                    k += 1;
                }
                if k != usize::try_from(f.body.args()).unwrap_or(0) + 1 {
                    for lid in f.locals.iter().flatten() {
                        self.mem.kill(*lid);
                    }
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
            FnKind::Intrinsic(Intrinsic::ConstEvalSelect) => self.call_const_eval_select(args, d, target),
            FnKind::Intrinsic(i) => {
                self.intrinsic(i, iargs, args, d)?;
                self.jump_opt(target)
            }
        }
    }

    fn tail_call(&mut self, callee: CalleeId, args: Ops) -> R<'a, Option<DataRecord<Alloc<'a>>>> {
        let body = self.top()?.body;
        let ret = self.top()?.ret;
        let (key, gargs) = match body.callee(callee) {
            Callee::FnItem { key, args } | Callee::Method { key, args } | Callee::Intrinsic { key, args } => {
                (key, self.ground_list(args)?)
            }
            Callee::Value(op) => self.callee_of_value(op)?,
        };
        let (item, iargs) = self.resolve(key, gargs)?;
        match self.world.fn_kind(item) {
            FnKind::Const => {
                let f = self.frame(item, iargs, ret)?;
                let mut k = 1usize;
                for op in body.ops(args) {
                    let Some(id) = f.locals.get(k).copied().flatten() else {
                        for lid in f.locals.iter().flatten() {
                            self.mem.kill(*lid);
                        }
                        return Err(malformed("a tail call with more arguments than its callee takes").into());
                    };
                    if let Err(e) = self.operand_into(op, Pointer { alloc: id, offset: 0 }) {
                        for lid in f.locals.iter().flatten() {
                            self.mem.kill(*lid);
                        }
                        return Err(e);
                    }
                    k += 1;
                }
                if k != usize::try_from(f.body.args()).unwrap_or(0) + 1 {
                    for lid in f.locals.iter().flatten() {
                        self.mem.kill(*lid);
                    }
                    return Err(malformed("a tail call with fewer arguments than its callee takes").into());
                }
                if let Some(old) = self.frames.pop() {
                    for lid in old.locals.iter().flatten() {
                        self.mem.kill(*lid);
                    }
                }
                self.frames.push(f);
                Ok(None)
            }
            FnKind::NotConst => Err(Refusal::NotConstFn { item }.into()),
            FnKind::UnknownIntrinsic => Err(Refusal::UnknownIntrinsic { item }.into()),
            FnKind::Missing(fact) => Err(Refusal::FnFact { item, fact }.into()),
            FnKind::Ctor { variant, .. } => {
                let d = self.place(body.local_place(Local(0)))?;
                self.fields(d.ptr, d.ty, variant, body.ops(args))?;
                self.set_tag(d.ptr, d.ty, variant)?;
                self.ret()
            }
            FnKind::Panic => Err(Refusal::Panic { parts: Vec::new_in(self.alloc) }.into()),
            FnKind::Intrinsic(Intrinsic::ConstEvalSelect) => {
                Err(unsupported("`become const_eval_select` in a constant").into())
            }
            FnKind::Intrinsic(i) => {
                let d = self.place(body.local_place(Local(0)))?;
                self.intrinsic(i, iargs, args, Some(d))?;
                self.ret()
            }
        }
    }

    fn call_const_eval_select(&mut self, args: Ops, d: Option<Pl>, target: Option<Block>) -> R<'a, ()> {
        let body = self.top()?.body;
        let mut ops = body.ops(args);
        let (Some(tuple_op), Some(const_fn_op)) = (ops.next(), ops.next()) else {
            return Err(malformed("`const_eval_select` arguments").into());
        };
        let (key, gargs) = self.callee_of_value(const_fn_op)?;
        let (item, iargs) = self.resolve(key, gargs)?;
        let tuple = self.operand(tuple_op)?;
        let res = (|| -> R<'a, ()> {
            let PNode::Tuple(parts) = self.node(tuple.ty)? else {
                return Err(malformed("`const_eval_select` arguments that are not a tuple").into());
            };
            let n = usize::try_from(parts.len).map_err(|_| malformed("tuple length overflow"))?;
            match self.world.fn_kind(item) {
                FnKind::Const => {
                    let f = self.frame(item, iargs, Ret::Call { dest: d.map(|d| d.ptr), target })?;
                    if n != usize::try_from(f.body.args()).unwrap_or(0) {
                        for lid in f.locals.iter().flatten() {
                            self.mem.kill(*lid);
                        }
                        return Err(malformed("`const_eval_select` argument count mismatch").into());
                    }
                    let base_ptr = match tuple.src {
                        Src::At(p) => Some(p),
                        Src::Temp(id) => Some(Pointer { alloc: id, offset: 0 }),
                        Src::Imm(_) => None,
                    };
                    for idx in 0..n {
                        let Some(id) = f.locals.get(idx + 1).copied().flatten() else {
                            for lid in f.locals.iter().flatten() {
                                self.mem.kill(*lid);
                            }
                            return Err(malformed("`const_eval_select` argument slot missing").into());
                        };
                        let fi = u32::try_from(idx).map_err(|_| malformed("tuple index overflow"))?;
                        let fty = self.field_ty(tuple.ty, 0, fi)?;
                        let fsize = self.size(fty)?;
                        if fsize != 0 {
                            let off = self.field_offset(tuple.ty, 0, fi)?;
                            let Some(p) = base_ptr else {
                                for lid in f.locals.iter().flatten() {
                                    self.mem.kill(*lid);
                                }
                                return Err(malformed("`const_eval_select` non-empty tuple as immediate").into());
                            };
                            if let Err(e) = self.mem_copy(p.at(off), Pointer { alloc: id, offset: 0 }, fsize) {
                                for lid in f.locals.iter().flatten() {
                                    self.mem.kill(*lid);
                                }
                                return Err(e);
                            }
                        }
                    }
                    self.frames.push(f);
                    Ok(())
                }
                FnKind::NotConst => Err(Refusal::NotConstFn { item }.into()),
                FnKind::UnknownIntrinsic => Err(Refusal::UnknownIntrinsic { item }.into()),
                FnKind::Missing(fact) => Err(Refusal::FnFact { item, fact }.into()),
                FnKind::Panic => Err(Refusal::Panic { parts: Vec::new_in(self.alloc) }.into()),
                _ => Err(unsupported("`const_eval_select` targeting a constructor or intrinsic").into()),
            }
        })();
        self.release(tuple);
        res
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
                let f = self.frame(item, args, Ret::Call { dest: None, target: Some(target) })?;
                let one = f.locals.get(1).copied().flatten().ok_or(Stop::from(malformed("drop glue that takes no argument")))?;
                self.mem_write(Pointer { alloc: one, offset: 0 }, PTR, Scalar::Ptr(pl.ptr))?;
                self.frames.push(f);
                Ok(())
            }
        }
    }

    /// Freezes a sub-body's `_0` allocation into an immutable `Origin::Data` allocation, zeroing
    /// unwritten padding and verifying that no pointer dangles into a dead local of `f`.
    fn freeze_sub_result(&mut self, r0: AllocId) -> R<'a, AllocId> {
        let align = self.effective_align_log2(r0)?;
        let mut bytes: Vec<u8, Alloc<'a>> = Vec::new_in(self.alloc);
        bytes.extend_from_slice(self.mem.bytes(r0)?);
        self.mem.zero_unwritten(r0, &mut bytes);
        let mut relocs: Vec<(u64, AllocId), Alloc<'a>> = Vec::new_in(self.alloc);
        for &(off, to) in self.mem.relocs(r0)? {
            if matches!(self.mem.get(to)?.origin, Origin::Local) {
                return Err(Refusal::Dangling.into());
            }
            relocs.push((off, to));
        }
        let len = u64::try_from(bytes.len()).map_err(|_| malformed("allocation length overflow"))?;
        let data_id = self.mem.allocate(len, align, false, Origin::Data)?;
        self.mem.fill(data_id, &bytes, &relocs)?;
        Ok(data_id)
    }

    /// `return`: the callee's `_0` to the caller's destination, its locals released; the
    /// root's `_0` interned as the value.
    fn ret(&mut self) -> R<'a, Option<DataRecord<Alloc<'a>>>> {
        let f = self.frames.pop().ok_or(Stop::from(malformed("a return with no frame")))?;
        let r0 = f.locals.first().copied().flatten().ok_or(Stop::from(Refusal::Dangling))?;
        if self.frames.is_empty() {
            let ty0 = self.local_ty(&f, 0)?;
            let is_static = self.world.is_static(f.item);
            return Ok(Some(self.intern(r0, ty0, is_static)?));
        }
        match f.ret {
            Ret::Call { dest, target } => {
                if let Some(dst) = dest {
                    let ty0 = self.local_ty(&f, 0)?;
                    let size = self.size(ty0)?;
                    if size != 0 {
                        self.mem_copy(Pointer { alloc: r0, offset: 0 }, dst, size)?;
                    }
                }
                for id in f.locals.iter().flatten() {
                    self.mem.kill(*id);
                }
                self.jump_opt(target)?;
            }
            Ret::Promoted { owner, borrow, args } => {
                let ty0 = self.local_ty(&f, 0)?;
                let data_id = self.freeze_sub_result(r0)?;
                for id in f.locals.iter().flatten() {
                    self.mem.kill(*id);
                }
                self.promoted.push((owner, borrow, args, data_id, Some(ty0)));
            }
            Ret::Anon { owner, block, args } => {
                let data_id = self.freeze_sub_result(r0)?;
                for id in f.locals.iter().flatten() {
                    self.mem.kill(*id);
                }
                self.anons.push((owner, block, args, data_id));
            }
        }
        Ok(None)
    }

    // ---- Places ----

    fn place(&mut self, p: Place) -> R<'a, Pl> {
        let f = self.top()?;
        let row = f.body.place(p);
        if row.parent.is_none() {
            let i = usize::try_from(row.local.0).map_err(|_| malformed("a local past the frame"))?;
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
        let i = usize::try_from(cur.local.0).map_err(|_| malformed("a local past the frame"))?;
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
                let off = match self.node(pl.ty)? {
                    PNode::Closure { .. } => {
                        let env = self.closure_env_ty(pl.ty)?;
                        self.field_offset(env, 0, index)?
                    }
                    _ => self.field_offset(pl.ty, pl.variant, index)?,
                };
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
                let idx = usize::try_from(l.0).map_err(|_| malformed("a local past the frame"))?;
                let f = self.top()?;
                let id = f.locals.get(idx).copied().flatten().ok_or(Stop::from(Refusal::Dangling))?;
                let raw = self.uint(self.mem_read(Pointer { alloc: id, offset: 0 }, PTR)?)?;
                let i = u64::try_from(raw).map_err(|_| malformed("an index past u64"))?;
                self.index(pl, i)
            }
            Proj::ConstIndex { offset, min_length, from_end } => {
                let (_, len) = self.elem(pl)?;
                let off = u64::from(offset);
                let min_len = u64::from(min_length);
                if len < min_len {
                    return Err(Refusal::IndexOutOfBounds { index: off, len }.into());
                }
                let i = if from_end {
                    if offset == 0 {
                        return Err(Refusal::IndexOutOfBounds { index: len, len }.into());
                    }
                    len.checked_sub(off).ok_or(Stop::from(Refusal::IndexOutOfBounds { index: off, len }))?
                } else {
                    off
                };
                self.index(pl, i)
            }
            Proj::Subslice { from, to, from_end } => {
                let (elem, len) = self.elem(pl)?;
                let (from, to) = (u64::from(from), u64::from(to));
                let n = if from_end {
                    let trim = from.checked_add(to).ok_or(Stop::from(malformed("a subslice trim overflow")))?;
                    len.checked_sub(trim).ok_or(Stop::from(Refusal::IndexOutOfBounds { index: trim, len }))?
                } else {
                    if to > len {
                        return Err(Refusal::IndexOutOfBounds { index: to, len }.into());
                    }
                    to.checked_sub(from).ok_or(Stop::from(malformed("a subslice with `to < from`")))?
                };
                let stride = self.size(elem)?;
                let byte_off = from.checked_mul(stride).ok_or(Stop::from(Refusal::OutOfBounds { offset: u64::MAX, size: 0, len }))?;
                let ptr = pl.ptr.at(byte_off);
                match self.node(pl.ty)? {
                    PNode::Array(..) => {
                        let count = self.ground.usize_value(n).map_err(|_| malformed("a subslice's length"))?;
                        let ty = self.ground.intern(PNode::Array(elem, count)).map_err(|_| malformed("a subslice's type"))?;
                        Ok(Pl { ptr, meta: None, ty, variant: 0 })
                    }
                    _ => Ok(Pl { ptr, meta: Some(Scalar::Int(u128::from(n))), ty: pl.ty, variant: 0 }),
                }
            }
            Proj::Downcast(v) => Ok(Pl { variant: v, ..pl }),
        }
    }

    pub(crate) fn deref(&mut self, at: Pointer, pointee: PTy) -> R<'a, Pl> {
        let ptr = match self.mem_read(at, PTR)? {
            Scalar::Ptr(p) => p,
            Scalar::Int(_) => return Err(Refusal::IntAsPointer.into()),
        };
        let meta = if self.is_unsized(pointee)? { Some(self.mem_read(at.at(PTR), PTR)?) } else { None };
        Ok(Pl { ptr, meta, ty: pointee, variant: 0 })
    }

    /// The element type and length of an array or slice place.
    fn elem(&mut self, pl: Pl) -> R<'a, (PTy, u64)> {
        match self.node(pl.ty)? {
            PNode::Array(e, n) => Ok((e, self.ground.view().value_u64(n).ok_or(Stop::from(malformed("an array length that is no value")))?)),
            PNode::Slice(e) => {
                let m = pl.meta.ok_or(Stop::from(malformed("a slice place with no length")))?;
                let raw = self.uint(m)?;
                let len = u64::try_from(raw).map_err(|_| malformed("a slice length past u64"))?;
                Ok((e, len))
            }
            _ => Err(malformed("an index of a place that is no array or slice").into()),
        }
    }

    /// Element `i` of an array or slice place.
    fn index(&mut self, pl: Pl, i: u64) -> R<'a, Pl> {
        let (e, len) = self.elem(pl)?;
        if i >= len {
            return Err(Refusal::IndexOutOfBounds { index: i, len }.into());
        }
        let stride = self.size(e)?;
        let byte_off = i.checked_mul(stride).ok_or(Stop::from(Refusal::OutOfBounds { offset: u64::MAX, size: 0, len }))?;
        Ok(Pl { ptr: pl.ptr.at(byte_off), meta: None, ty: e, variant: 0 })
    }

    fn len_of(&mut self, pl: Pl) -> R<'a, u64> {
        match self.node(pl.ty)? {
            PNode::Str => {
                let m = pl.meta.ok_or(Stop::from(malformed("a str place with no length")))?;
                let raw = self.uint(m)?;
                u64::try_from(raw).map_err(|_| malformed("a str length past u64").into())
            }
            _ => Ok(self.elem(pl)?.1),
        }
    }

    // ---- Operands ----

    pub(crate) fn operand(&mut self, op: Operand) -> R<'a, Opnd> {
        match op {
            Operand::Copy(p) | Operand::Move(p) => {
                let pl = self.place(p)?;
                Ok(Opnd { src: Src::At(pl.ptr), ty: pl.ty })
            }
            Operand::Const(c) => self.konst(c),
        }
    }

    /// Wraps a data allocation in a pointer/fat-pointer temporary when `ty` is a ref/ptr to it,
    /// or returns `Src::At(data)` directly when `ty` matches the allocation's own value.
    fn ptr_or_data(&mut self, data: AllocId, ty: PTy, body_ret_ty: Option<PTy>) -> R<'a, Src> {
        if body_ret_ty == Some(ty) {
            return Ok(Src::At(Pointer { alloc: data, offset: 0 }));
        }
        match self.node(ty)? {
            PNode::Ref(_, t) | PNode::Ptr(_, t) => {
                let unsized_ = self.is_unsized(t)?;
                let tmp = self.mem.allocate(if unsized_ { 2 * PTR } else { PTR }, 3, true, Origin::Data)?;
                let at = Pointer { alloc: tmp, offset: 0 };
                self.mem.write(at, PTR, Scalar::Ptr(Pointer { alloc: data, offset: 0 }))?;
                if unsized_ {
                    let byte_len = self.alloc_len(data)?;
                    let meta_len = match self.node(t)? {
                        PNode::Str => byte_len,
                        PNode::Slice(elem) => {
                            let esize = self.size(elem)?;
                            if esize == 0 {
                                0
                            } else {
                                byte_len / esize
                            }
                        }
                        _ => {
                            self.mem.kill(tmp);
                            return Err(unsupported("a promoted unsized borrow that is neither a slice nor `str`").into());
                        }
                    };
                    self.mem.write(at.at(PTR), PTR, Scalar::Int(u128::from(meta_len)))?;
                }
                Ok(Src::Temp(tmp))
            }
            _ => Ok(Src::At(Pointer { alloc: data, offset: 0 })),
        }
    }

    fn konst(&mut self, c: Const) -> R<'a, Opnd> {
        let f = self.top()?;
        let body = f.body;
        let owner = f.item;
        let owner_args = f.args_list;
        let decl = body.konst(c);
        let ty = self.ground_ty(decl.ty)?;
        let src = match decl.kind {
            ConstKind::Scalar { bits } => Src::Imm(bits),
            // A fn item is zero-sized.
            ConstKind::FnItem { .. } => Src::Imm(0),
            ConstKind::Param(n) => {
                let idx = usize::try_from(n).map_err(|_| malformed("a const parameter past the frame's arguments"))?;
                let a = *self.top()?.args.get(idx).ok_or(Stop::from(malformed("a const parameter past the frame's arguments")))?;
                match self.node(a)? {
                    PNode::ConstValue { bits, .. } => Src::Imm(bits),
                    _ => return Err(malformed("a const parameter whose argument is no value").into()),
                }
            }
            ConstKind::Bytes { .. } => {
                let bytes = body.bytes(c).ok_or(Stop::from(malformed("a bytes constant with no bytes")))?;
                let data = match self.bytes_consts.iter().find(|&&(it, kc, _)| it == owner && kc == c) {
                    Some(&(_, _, id)) => id,
                    None => {
                        let len = u64::try_from(bytes.len()).map_err(|_| malformed("bytes constant length overflow"))?;
                        let id = self.mem.allocate(len, 0, false, Origin::Data)?;
                        self.mem.fill(id, bytes, &[])?;
                        self.bytes_consts.push((owner, c, id));
                        id
                    }
                };
                self.ptr_or_data(data, ty, None)?
            }
            ConstKind::Item { key, args } => {
                let args = self.ground_list(args)?;
                Src::At(Pointer { alloc: self.load_const(key, args)?, offset: 0 })
            }
            ConstKind::Promoted(id) => {
                let borrow = ExprId(id);
                if let Some(&(_, _, _, data, ret_ty)) =
                    self.promoted.iter().find(|&&(o, b, a, _, _)| o == owner && b == borrow && a == owner_args)
                {
                    self.ptr_or_data(data, ty, ret_ty)?
                } else {
                    match self.world.promoted(owner, borrow) {
                        Ok(Some(b)) => {
                            let sub = self.frame_from_bodied(
                                BodyId::Promoted(owner, borrow),
                                b,
                                owner_args,
                                Ret::Promoted { owner, borrow, args: owner_args },
                            )?;
                            if sub.body.args() != 0 {
                                for lid in sub.locals.iter().flatten() {
                                    self.mem.kill(*lid);
                                }
                                return Err(malformed("a promoted constant's body that takes arguments").into());
                            }
                            self.frames.push(sub);
                            return Err(Stop::SubFrame);
                        }
                        Err(_) => {
                            return Err(Stop::Need(Need { kind: NeedKind::Body, item: owner, args: owner_args }));
                        }
                        Ok(None) => {
                            let args_slice = self.ground.view().list(owner_args).map_err(|_| malformed("frame arguments past ground table"))?;
                            let mut argv: Vec<PTy, Alloc<'a>> = Vec::with_capacity_in(args_slice.len(), self.alloc);
                            argv.extend_from_slice(args_slice);
                            match self.world.promoted_record(owner, borrow, &argv) {
                                Ok(Some(rec)) => {
                                    let data = self.decode_record_into(rec, None)?;
                                    self.promoted.push((owner, borrow, owner_args, data, None));
                                    self.ptr_or_data(data, ty, None)?
                                }
                                Err(_) => {
                                    return Err(Stop::Need(Need { kind: NeedKind::Value, item: owner, args: owner_args }));
                                }
                                Ok(None) => {
                                    return Err(unsupported("a promoted constant whose world provides neither body nor record").into());
                                }
                            }
                        }
                    }
                }
            }
            ConstKind::Anon(block) => {
                if let Some(&(_, _, _, data)) =
                    self.anons.iter().find(|&&(o, b, a, _)| o == owner && b == block && a == owner_args)
                {
                    Src::At(Pointer { alloc: data, offset: 0 })
                } else {
                    match self.world.anon(owner, block) {
                        Ok(Some(b)) => {
                            let sub = self.frame_from_bodied(
                                BodyId::Anon(owner, block),
                                b,
                                owner_args,
                                Ret::Anon { owner, block, args: owner_args },
                            )?;
                            if sub.body.args() != 0 {
                                for lid in sub.locals.iter().flatten() {
                                    self.mem.kill(*lid);
                                }
                                return Err(malformed("an inline `const` block's body that takes arguments").into());
                            }
                            self.frames.push(sub);
                            return Err(Stop::SubFrame);
                        }
                        Err(_) => {
                            return Err(Stop::Need(Need { kind: NeedKind::Body, item: owner, args: owner_args }));
                        }
                        Ok(None) => {
                            let args_slice = self.ground.view().list(owner_args).map_err(|_| malformed("frame arguments past ground table"))?;
                            let mut argv: Vec<PTy, Alloc<'a>> = Vec::with_capacity_in(args_slice.len(), self.alloc);
                            argv.extend_from_slice(args_slice);
                            match self.world.anon_record(owner, block, &argv) {
                                Ok(Some(rec)) => {
                                    let data = self.decode_record_into(rec, None)?;
                                    self.anons.push((owner, block, owner_args, data));
                                    Src::At(Pointer { alloc: data, offset: 0 })
                                }
                                Err(_) => {
                                    return Err(Stop::Need(Need { kind: NeedKind::Value, item: owner, args: owner_args }));
                                }
                                Ok(None) => {
                                    return Err(unsupported("an inline `const` block whose world provides neither body nor record").into());
                                }
                            }
                        }
                    }
                }
            }
            ConstKind::StaticAddr(item) => {
                let is_mut = matches!(self.node(ty)?, PNode::Ptr(Mutbl::Mut, _));
                let id = self.static_alloc(item, is_mut)?;
                let tmp = self.mem.allocate(PTR, 3, true, Origin::Data)?;
                self.mem.write(Pointer { alloc: tmp, offset: 0 }, PTR, Scalar::Ptr(Pointer { alloc: id, offset: 0 }))?;
                Src::Temp(tmp)
            }
        };
        Ok(Opnd { src, ty })
    }

    pub(crate) fn fn_alloc(&mut self, inst: InstanceKey, named: Option<ConstFn>) -> R<'a, AllocId> {
        if let Some((_, existing, id)) = self.fns.iter_mut().find(|(k, _, _)| *k == inst) {
            if existing.is_none() && named.is_some() {
                *existing = named;
            }
            return Ok(*id);
        }
        let id = self.mem.allocate(0, 0, false, Origin::Fn(inst))?;
        self.fns.push((inst, named, id));
        Ok(id)
    }

    pub(crate) fn vtable_alloc(&mut self, key: VtableKey, named: Option<ConstVtable>) -> R<'a, AllocId> {
        if let Some((_, existing, id)) = self.vtables.iter_mut().find(|(k, _, _)| *k == key) {
            if existing.is_none() && named.is_some() {
                *existing = named;
            }
            return Ok(*id);
        }
        let id = self.mem.allocate(0, 0, false, Origin::Vtable(key))?;
        self.vtables.push((key, named, id));
        Ok(id)
    }

    fn static_alloc(&mut self, item: ItemKey, mutable: bool) -> R<'a, AllocId> {
        let is_mut = mutable || self.world.is_mut_static(item);
        if let Some((_, slot)) = self.statics.iter_mut().find(|(k, _)| *k == item) {
            if is_mut {
                slot.mut_static = true;
            }
            return Ok(slot.id);
        }
        if is_mut {
            let id = self.mem.allocate(0, 0, false, Origin::Static(item))?;
            self.statics.push((item, StaticSlot { id, loaded: false, mut_static: true }));
            return Ok(id);
        }
        let code = self.world.code_id(item);
        let inst = instance_key(code, List::EMPTY, self.ground.view(), self.alloc).map_err(build_refusal)?;
        match self.world.record(RecordKey::of(inst)) {
            Ok(rec) => {
                let first = rec.data.allocs.first().ok_or(Stop::from(malformed("a record with no allocation")))?;
                let len = u64::try_from(first.bytes.len()).map_err(|_| malformed("static record length overflow"))?;
                let id = self.mem.allocate(len, first.align, false, Origin::Static(item))?;
                self.statics.push((item, StaticSlot { id, loaded: true, mut_static: false }));
                self.decode_record_into(rec, Some(id))?;
                Ok(id)
            }
            Err(_) => {
                let id = self.mem.allocate(0, 0, false, Origin::Static(item))?;
                self.statics.push((item, StaticSlot { id, loaded: false, mut_static: false }));
                Ok(id)
            }
        }
    }

    /// Iteratively decodes `rec` (and any static records reachable from its relocations whose
    /// records are already present in `world`) into `self.mem` with zero host recursion.
    fn decode_record_into(&mut self, rec: &'w ConstRecord<Alloc<'_>>, root: Option<AllocId>) -> R<'a, AllocId> {
        let first_id = match root {
            Some(id) => id,
            None => {
                let first = rec.data.allocs.first().ok_or(Stop::from(malformed("a record with no allocation")))?;
                let len = u64::try_from(first.bytes.len()).map_err(|_| malformed("record allocation length overflow"))?;
                self.mem.allocate(len, first.align, false, Origin::Data)?
            }
        };
        let mut work: Vec<(AllocId, &'w ConstRecord<Alloc<'_>>), Alloc<'a>> = Vec::new_in(self.alloc);
        work.push((first_id, rec));
        while let Some((root_id, cur)) = work.pop() {
            let mut ids: Vec<AllocId, Alloc<'a>> = Vec::with_capacity_in(cur.data.allocs.len(), self.alloc);
            for (i, a) in cur.data.allocs.iter().enumerate() {
                if i == 0 {
                    ids.push(root_id);
                } else {
                    let len = u64::try_from(a.bytes.len()).map_err(|_| malformed("record allocation length overflow"))?;
                    ids.push(self.mem.allocate(len, a.align, false, Origin::Data)?);
                }
            }
            let mut relocs: Vec<(u64, AllocId), Alloc<'a>> = Vec::new_in(self.alloc);
            for (k, a) in cur.data.allocs.iter().enumerate() {
                relocs.clear();
                for r in a.relocs.iter() {
                    let to = match r.target {
                        DataTarget::Local(n) => {
                            let idx = usize::try_from(n).map_err(|_| malformed("a record's relocation past its allocations"))?;
                            *ids.get(idx).ok_or(Stop::from(malformed("a record's relocation past its allocations")))?
                        }
                        DataTarget::Fn(i) => self.fn_alloc(i, cur.fn_named(i))?,
                        DataTarget::Vtable(v) => self.vtable_alloc(v, cur.vtable_named(v))?,
                        DataTarget::Static(s) => {
                            if let Some(&(_, slot)) = self.statics.iter().find(|&&(k, _)| k == s) {
                                slot.id
                            } else if self.world.is_mut_static(s) {
                                let sid = self.mem.allocate(0, 0, false, Origin::Static(s))?;
                                self.statics.push((s, StaticSlot { id: sid, loaded: false, mut_static: true }));
                                sid
                            } else {
                                let code = self.world.code_id(s);
                                let inst = instance_key(code, List::EMPTY, self.ground.view(), self.alloc).map_err(build_refusal)?;
                                match self.world.record(RecordKey::of(inst)) {
                                    Ok(srec) => {
                                        let sfirst = srec.data.allocs.first().ok_or(Stop::from(malformed("a static record with no allocation")))?;
                                        let slen = u64::try_from(sfirst.bytes.len()).map_err(|_| malformed("static record length overflow"))?;
                                        let sid = self.mem.allocate(slen, sfirst.align, false, Origin::Static(s))?;
                                        self.statics.push((s, StaticSlot { id: sid, loaded: true, mut_static: false }));
                                        work.push((sid, srec));
                                        sid
                                    }
                                    Err(_) => {
                                        let sid = self.mem.allocate(0, 0, false, Origin::Static(s))?;
                                        self.statics.push((s, StaticSlot { id: sid, loaded: false, mut_static: false }));
                                        sid
                                    }
                                }
                            }
                        }
                    };
                    relocs.push((u64::from(r.offset), to));
                }
                let id = *ids.get(k).ok_or(Stop::from(malformed("a record's allocation past its ids")))?;
                self.mem.fill(id, &a.bytes, &relocs)?;
            }
        }
        Ok(first_id)
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
        let id = self.decode_record_into(rec, None)?;
        self.consts.push((key, args, id));
        Ok(id)
    }

    pub(crate) fn release(&mut self, o: Opnd) {
        if let Src::Temp(id) = o.src {
            self.mem.kill(id);
        }
    }

    pub(crate) fn check_scalar_valid(&self, ty: PTy, s: Scalar) -> R<'a, ()> {
        match (self.node(ty)?, s) {
            (PNode::Bool, Scalar::Int(v)) if v > 1 => Err(Refusal::InvalidBool { value: v }.into()),
            (PNode::Bool, Scalar::Ptr(_)) => Err(Refusal::PointerAsInt.into()),
            (PNode::Char, Scalar::Int(v)) if u32::try_from(v).ok().and_then(char::from_u32).is_none() => {
                Err(Refusal::InvalidChar { value: v }.into())
            }
            (PNode::Char, Scalar::Ptr(_)) => Err(Refusal::PointerAsInt.into()),
            (PNode::Int(_) | PNode::Uint(_) | PNode::Float(_), Scalar::Ptr(_)) => Err(Refusal::PointerAsInt.into()),
            _ => Ok(()),
        }
    }

    pub(crate) fn read(&mut self, o: Opnd) -> R<'a, Scalar> {
        let size = self.size(o.ty)?;
        let s = match o.src {
            Src::At(p) => self.mem_read(p, size)?,
            Src::Temp(id) => self.mem_read(Pointer { alloc: id, offset: 0 }, size)?,
            Src::Imm(b) => Scalar::Int(b),
        };
        self.check_scalar_valid(o.ty, s)?;
        Ok(s)
    }

    pub(crate) fn copy_out(&mut self, o: Opnd, dst: Pointer, size: u64) -> R<'a, ()> {
        if size == 0 {
            return Ok(());
        }
        match o.src {
            Src::At(p) => self.mem_copy(p, dst, size)?,
            Src::Temp(id) => self.mem_copy(Pointer { alloc: id, offset: 0 }, dst, size)?,
            Src::Imm(b) => self.mem_write(dst, size, Scalar::Int(b))?,
        }
        Ok(())
    }

    pub(crate) fn operand_into(&mut self, op: Operand, dst: Pointer) -> R<'a, ()> {
        let o = self.operand(op)?;
        let res = (|| -> R<'a, ()> {
            if matches!(
                self.node(o.ty)?,
                PNode::Bool | PNode::Char | PNode::Int(_) | PNode::Uint(_) | PNode::Float(_)
            ) {
                let s = self.read(o)?;
                let size = self.size(o.ty)?;
                self.mem_write(dst, size, s)?;
                if let Src::At(src) = o.src {
                    self.inherit_align(src, dst, size)?;
                }
                return Ok(());
            }
            let size = self.size(o.ty)?;
            self.copy_out(o, dst, size)
        })();
        self.release(o);
        res
    }

    pub(crate) fn uint(&self, s: Scalar) -> R<'a, u128> {
        match s {
            Scalar::Int(v) => Ok(v),
            Scalar::Ptr(_) => Err(Refusal::PointerAsInt.into()),
        }
    }

    /// An operand's bits as an integer.
    pub(crate) fn int(&mut self, op: Operand) -> R<'a, u128> {
        let o = self.operand(op)?;
        let s = self.read(o);
        self.release(o);
        self.uint(s?)
    }

    /// `v` written at `d` in `d`'s size.
    pub(crate) fn put(&mut self, d: Pl, v: u128) -> R<'a, ()> {
        let size = self.size(d.ty)?;
        if size != 0 {
            let bits = u32::try_from(size.saturating_mul(8)).unwrap_or(128);
            self.mem_write(d.ptr, size, Scalar::Int(arith::mask(v, bits)))?;
        }
        Ok(())
    }

    // ---- Values ----

    fn value(&mut self, d: Pl, v: Value) -> R<'a, ()> {
        match v {
            Value::Use(op) => self.operand_into(op, d.ptr),
            Value::Borrow { place, .. } | Value::RawAddr { place, .. } => {
                let p = self.place(place)?;
                self.mem_write(d.ptr, PTR, Scalar::Ptr(p.ptr))?;
                if let Some(m) = p.meta {
                    self.mem_write(d.ptr.at(PTR), PTR, m)?;
                }
                Ok(())
            }
            Value::Binary { op, lhs, rhs } => {
                let (s, _) = self.binary(op, lhs, rhs, false)?;
                let size = self.size(d.ty)?;
                self.mem_write(d.ptr, size, s)?;
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
                        if let PNode::Adt(adt_key, _) = self.node(d.ty)?
                            && self.world.adt(adt_key).is_some_and(|a| a.kind == AdtKind::Union)
                        {
                            return self.write_union(d, variant, body.ops(operands));
                        }
                        self.fields(d.ptr, d.ty, variant, body.ops(operands))?;
                        self.set_tag(d.ptr, d.ty, variant)
                    }
                    AggregateKind::Array => {
                        let (e, _) = self.elem(d)?;
                        let stride = self.size(e)?;
                        for (k, op) in body.ops(operands).enumerate() {
                            let idx = u64::try_from(k).map_err(|_| malformed("array operand index overflow"))?;
                            let off = idx.checked_mul(stride).ok_or(Stop::from(malformed("array offset overflow")))?;
                            self.operand_into(op, d.ptr.at(off))?;
                        }
                        Ok(())
                    }
                    AggregateKind::Closure => {
                        let env = self.closure_env_ty(d.ty)?;
                        self.fields(d.ptr, env, 0, body.ops(operands))
                    }
                    AggregateKind::Coroutine => Err(unsupported("a coroutine built in a constant").into()),
                }
            }
            Value::Repeat { operand, count } => {
                let n = {
                    let o = self.konst(count)?;
                    let s = self.read(o);
                    self.release(o);
                    u64::try_from(self.uint(s?)?).map_err(|_| malformed("repeat count past u64"))?
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
                                self.mem_write(d.ptr.at(k * stride), size, s)?;
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
                self.put(d, u128::from(n))
            }
            Value::Discriminant(p) => {
                let pl = self.place(p)?;
                let v = self.discriminant(pl)?;
                self.put(d, u128::from_le_bytes(v.to_le_bytes()))
            }
            Value::SizeOf(t) => {
                let t = self.ground_ty(t)?;
                let (size, _) = self.size_align(t)?;
                self.put(d, u128::from(size))
            }
            Value::AlignOf(t) => {
                let t = self.ground_ty(t)?;
                let (_, align) = self.size_align(t)?;
                self.put(d, 1u128 << align)
            }
            Value::ThreadLocalAddr(_) => Err(unsupported("a thread-local's address in a constant").into()),
        }
    }

    fn write_union(&mut self, d: Pl, field_idx: u32, mut ops: impl Iterator<Item = Operand>) -> R<'a, ()> {
        let Some(op) = ops.next() else {
            return Err(malformed("a union aggregate without its active field operand").into());
        };
        if ops.next().is_some() {
            return Err(malformed("a union aggregate with more than one operand").into());
        }
        let off = self.field_offset(d.ty, 0, field_idx)?;
        let union_size = self.size(d.ty)?;
        let o = self.operand(op)?;
        let res = (|| -> R<'a, ()> {
            let op_size = self.size(o.ty)?;
            if matches!(self.node(o.ty)?, PNode::Bool | PNode::Char) {
                let s = self.read(o)?;
                self.mem_deinit(d.ptr, union_size)?;
                self.mem_write(d.ptr.at(off), op_size, s)?;
                return Ok(());
            }
            let aliases = matches!(o.src, Src::At(p) if p.alloc == d.ptr.alloc);
            if aliases && op_size != 0 {
                let (_, op_align) = self.size_align(o.ty)?;
                let tmp = self.mem.allocate(op_size, op_align, true, Origin::Data)?;
                let copy_res = (|| -> R<'a, ()> {
                    self.copy_out(o, Pointer { alloc: tmp, offset: 0 }, op_size)?;
                    self.mem_deinit(d.ptr, union_size)?;
                    self.mem_copy(Pointer { alloc: tmp, offset: 0 }, d.ptr.at(off), op_size)
                })();
                self.mem.kill(tmp);
                return copy_res;
            }
            self.mem_deinit(d.ptr, union_size)?;
            self.copy_out(o, d.ptr.at(off), op_size)
        })();
        self.release(o);
        res
    }

    /// A `(value, overflowed)` pair written at `d`, a two-field tuple.
    pub(crate) fn write_checked(&mut self, d: Pl, s: Scalar, overflowed: bool) -> R<'a, ()> {
        let PNode::Tuple(parts) = self.node(d.ty)? else {
            return Err(malformed("a checked operation into a place that is no pair").into());
        };
        let t0 = *self.ground.view().list(parts).map_err(|_| malformed("a pair's fields"))?.first().ok_or(Stop::from(malformed("a pair with no fields")))?;
        let (off0, off1) = (self.field_offset(d.ty, 0, 0)?, self.field_offset(d.ty, 0, 1)?);
        let size0 = self.size(t0)?;
        self.mem_write(d.ptr.at(off0), size0, s)?;
        self.mem_write(d.ptr.at(off1), 1, Scalar::Int(u128::from(overflowed)))?;
        Ok(())
    }

    fn fields(&mut self, at: Pointer, ty: PTy, variant: u32, ops: impl Iterator<Item = Operand>) -> R<'a, ()> {
        for (k, op) in ops.enumerate() {
            let fi = u32::try_from(k).map_err(|_| malformed("field index overflow"))?;
            let off = self.field_offset(ty, variant, fi)?;
            self.operand_into(op, at.at(off))?;
        }
        Ok(())
    }

    pub(crate) fn set_tag(&mut self, at: Pointer, ty: PTy, variant: u32) -> R<'a, ()> {
        let mut cx = LayoutCx { ground: &mut *self.ground, facts: self.world, scratch: self.alloc };
        if let Some(w) = self.layout.tag_write(ty, variant, &mut cx).map_err(layout_stop)? {
            self.mem_write(at.at(w.offset), w.size, Scalar::Int(w.value))?;
        }
        Ok(())
    }

    pub(crate) fn discriminant(&mut self, pl: Pl) -> R<'a, i128> {
        let mut discrs: Vec<i128, Alloc<'a>> = Vec::new_in(self.alloc);
        match self.node(pl.ty)? {
            PNode::Adt(adt, _) => {
                let variants = self
                    .world
                    .adt(adt)
                    .ok_or(Stop::from(Refusal::Missing { what: "an enum's definition", item: adt }))?
                    .variants
                    .len();
                let count = u32::try_from(variants).map_err(|_| malformed("variant count overflow"))?;
                for v in 0..count {
                    match eval_discriminant(self.world, adt, v, self.alloc) {
                        Ok(Answer::Done(d)) => discrs.push(d),
                        Ok(Answer::Need(n)) => return Err(Stop::Need(n)),
                        Err(e) => return Err(e.refusal.into()),
                    }
                }
            }
            PNode::Union(ul) => {
                for v in 0..ul.len {
                    discrs.push(i128::from(v));
                }
            }
            _ => return Err(malformed("a discriminant of a place that is no enum").into()),
        }
        let tag: Tag = {
            let mut cx = LayoutCx { ground: &mut *self.ground, facts: self.world, scratch: self.alloc };
            self.layout.tag(pl.ty, &mut cx).map_err(layout_stop)?
        };
        let v = match tag.at {
            None => {
                self.ensure_loaded(pl.ptr.alloc)?;
                tag.variant(0, &discrs)?
            }
            Some((off, size)) => match self.mem_read(pl.ptr.at(off), size)? {
                Scalar::Int(bits) => tag.variant(bits, &discrs)?,
                Scalar::Ptr(p) => {
                    let in_bounds = if self.is_mut_static_alloc(p.alloc) {
                        p.offset == 0
                    } else {
                        p.offset <= self.alloc_len(p.alloc)?
                    };
                    if in_bounds && let Some(v) = tag.ptr_variant() {
                        v
                    } else {
                        return Err(Refusal::PointerAsInt.into());
                    }
                }
            },
        };
        let idx = usize::try_from(v).map_err(|_| malformed("variant index overflow"))?;
        discrs.get(idx).copied().ok_or(Stop::from(Refusal::InvalidTag { bits: u128::from(v) }))
    }

    fn unary(&mut self, d: Pl, op: UnOp, operand: Operand) -> R<'a, ()> {
        let o = self.operand(operand)?;
        let n = self.node(o.ty)?;
        if op == UnOp::PtrMetadata {
            let res = self.ptr_metadata_into(d, o);
            self.release(o);
            return res;
        }
        let s = self.read(o);
        self.release(o);
        let v = self.uint(s?)?;
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

    pub(crate) fn ptr_metadata_into(&mut self, d: Pl, o: Opnd) -> R<'a, ()> {
        let (PNode::Ref(_, t) | PNode::Ptr(_, t)) = self.node(o.ty)? else {
            return Err(malformed("metadata of a value that is no pointer").into());
        };
        if self.is_unsized(t)? {
            let p = match o.src {
                Src::At(p) => p,
                Src::Temp(id) => Pointer { alloc: id, offset: 0 },
                Src::Imm(_) => return Err(malformed("metadata of a pointer constant").into()),
            };
            self.mem_copy(p.at(PTR), d.ptr, PTR)
        } else {
            Ok(())
        }
    }

    pub(crate) fn transmute_into(&mut self, d: Pl, o: Opnd) -> R<'a, ()> {
        let (fs, ts) = (self.size(o.ty)?, self.size(d.ty)?);
        if fs != ts {
            return Err(Refusal::TransmuteSize { from: fs, to: ts }.into());
        }
        self.copy_out(o, d.ptr, ts)?;
        if matches!(self.node(d.ty)?, PNode::Bool | PNode::Char | PNode::Int(_) | PNode::Uint(_) | PNode::Float(_)) {
            let s = self.mem_read(d.ptr, ts)?;
            self.check_scalar_valid(d.ty, s)?;
        }
        Ok(())
    }

    fn principal_of(&self, t: PTy) -> R<'a, Option<(ItemKey, List)>> {
        let view = self.ground.view();
        let PNode::Dynamic(l) = view.node(t).map_err(|_| malformed("a type past the ground table"))? else {
            return Err(malformed("a trait object's principal asked of another type").into());
        };
        let preds = view.list(l).map_err(|_| malformed("a trait object's predicates past the ground table"))?;
        let Some(&first) = preds.first() else {
            return Err(unsupported("a trait object with no predicates").into());
        };
        Ok(match view.node(first).map_err(|_| malformed("a type past the ground table"))? {
            PNode::ExTrait(k, args) => Some((k, args)),
            _ => None,
        })
    }

    fn vtable_key_of(&mut self, concrete: PTy, principal: ItemKey, args: List) -> R<'a, VtableKey> {
        let mut args_buf: Vec<PTy, Alloc<'a>> = Vec::new_in(self.alloc);
        args_buf.extend_from_slice(
            self.ground
                .view()
                .list(args)
                .map_err(|_| malformed("a trait object's arguments past the ground table"))?,
        );
        let shape = self.world.code_id(principal);
        vtable_key(concrete, principal, &args_buf, shape, &mut *self.ground, self.alloc)
            .map_err(|_| malformed("a vtable's types past the ground table").into())
    }

    fn unsize_tails(&mut self, mut f: PTy, mut g: PTy) -> R<'a, (PTy, PTy)> {
        const UNSIZE_DEPTH: u32 = 16;
        for _ in 0..UNSIZE_DEPTH {
            if f == g {
                return Ok((f, g));
            }
            let nfields = match (self.node(f)?, self.node(g)?) {
                (PNode::Adt(k, _), PNode::Adt(k2, _)) if k == k2 => {
                    let adt = self
                        .world
                        .adt(k)
                        .ok_or(Stop::from(Refusal::Missing { what: "ADT definition", item: k }))?;
                    if adt.kind != AdtKind::Struct || adt.variants.len() != 1 {
                        return Err(unsupported("an unsizing of a pointee that is an enum or a union").into());
                    }
                    let Some(v) = adt.variants.first() else {
                        return Err(unsupported("an unsizing of a pointee struct without fields").into());
                    };
                    if v.fields.len == 0 {
                        return Err(unsupported("an unsizing of a pointee struct without fields").into());
                    }
                    v.fields.len
                }
                _ => return Ok((f, g)),
            };
            let last = nfields - 1;
            f = self.field_ty(f, 0, last)?;
            g = self.field_ty(g, 0, last)?;
        }
        Err(unsupported("an unsizing whose pointee passes through more struct tails than the bound").into())
    }

    fn unsize_into(&mut self, d: Pl, o: Opnd) -> R<'a, ()> {
        const UNSIZE_DEPTH: u32 = 16;
        let mut src = o.src;
        let mut dst = d.ptr;
        let mut ty = o.ty;
        let mut next = d.ty;
        for _ in 0..UNSIZE_DEPTH {
            if ty == next {
                let sz = self.size(ty)?;
                return self.copy_out(Opnd { src, ty }, dst, sz);
            }
            match (self.node(ty)?, self.node(next)?) {
                (PNode::Ref(_, f) | PNode::Ptr(_, f), PNode::Ref(_, g) | PNode::Ptr(_, g)) => {
                    let (from, to) = self.unsize_tails(f, g)?;
                    match (self.node(from)?, self.node(to)?) {
                        (PNode::Array(_, n), PNode::Slice(_)) => {
                            let len = self
                                .ground
                                .view()
                                .value_u64(n)
                                .ok_or(Stop::from(malformed("an array length that is no value")))?;
                            self.copy_out(Opnd { src, ty }, dst, PTR)?;
                            self.mem_write(dst.at(PTR), PTR, Scalar::Int(u128::from(len)))?;
                            return Ok(());
                        }
                        (PNode::Dynamic(_), PNode::Dynamic(_)) => {
                            let Some((principal, args)) = self.principal_of(to)? else {
                                return Err(unsupported("an unsizing to a trait object of auto traits alone (no principal trait)").into());
                            };
                            let sp = match src {
                                Src::At(p) => p,
                                Src::Temp(id) => Pointer { alloc: id, offset: 0 },
                                Src::Imm(_) => return Err(malformed("a trait-object upcast of an immediate").into()),
                            };
                            let data_word = self.mem_read(sp, PTR)?;
                            let meta_word = self.mem_read(sp.at(PTR), PTR)?;
                            let Scalar::Ptr(vp) = meta_word else {
                                return Err(malformed("a trait-object upcast whose metadata is not a vtable pointer").into());
                            };
                            let Origin::Vtable(old_key) = self.mem.get(vp.alloc)?.origin else {
                                return Err(malformed("a trait-object upcast whose metadata does not point to a vtable").into());
                            };
                            let Some(old) = self.vtables.iter().find(|&&(k, _, _)| k == old_key).and_then(|&(_, v, _)| v) else {
                                return Err(malformed("a trait-object upcast whose vtable was not registered").into());
                            };
                            let new_vt_ptr = if old.vtable.principal == principal && old.vtable.args == args {
                                vp
                            } else {
                                let concrete = old.vtable.concrete;
                                let key = self.vtable_key_of(concrete, principal, args)?;
                                let named = ConstVtable { key, vtable: VtableRef { concrete, principal, args } };
                                let vt_id = self.vtable_alloc(key, Some(named))?;
                                Pointer { alloc: vt_id, offset: 0 }
                            };
                            self.mem_write(dst, PTR, data_word)?;
                            self.mem_write(dst.at(PTR), PTR, Scalar::Ptr(new_vt_ptr))?;
                            return Ok(());
                        }
                        (_, PNode::Dynamic(_)) => {
                            let Some((principal, args)) = self.principal_of(to)? else {
                                return Err(unsupported("an unsizing to a trait object of auto traits alone (no principal trait)").into());
                            };
                            let key = self.vtable_key_of(from, principal, args)?;
                            let named = ConstVtable { key, vtable: VtableRef { concrete: from, principal, args } };
                            let vt_id = self.vtable_alloc(key, Some(named))?;
                            self.copy_out(Opnd { src, ty }, dst, PTR)?;
                            self.mem_write(dst.at(PTR), PTR, Scalar::Ptr(Pointer { alloc: vt_id, offset: 0 }))?;
                            return Ok(());
                        }
                        _ if from == to => {
                            let sz = self.size(next)?;
                            return self.copy_out(Opnd { src, ty }, dst, sz);
                        }
                        _ => return Err(unsupported("an unsizing coercion other than to a slice or a trait object").into()),
                    }
                }
                (PNode::Pat(bs, ps), PNode::Pat(bt, pt)) => {
                    let view = self.ground.view();
                    let lps = view.list(ps).map_err(|_| malformed("a pattern type's ranges past the ground table"))?;
                    let lpt = view.list(pt).map_err(|_| malformed("a pattern type's ranges past the ground table"))?;
                    if lps != lpt {
                        return Err(unsupported("an unsizing between two pattern types of different patterns").into());
                    }
                    ty = bs;
                    next = bt;
                }
                (PNode::Adt(k, _), PNode::Adt(k2, _)) if k == k2 => {
                    let adt = self
                        .world
                        .adt(k)
                        .ok_or(Stop::from(Refusal::Missing { what: "ADT definition", item: k }))?;
                    if adt.kind != AdtKind::Struct || adt.variants.len() != 1 {
                        return Err(unsupported("an unsizing coercion through an enum or a union").into());
                    }
                    let Some(v) = adt.variants.first() else {
                        return Err(unsupported("an unsizing coercion through an empty struct").into());
                    };
                    let nfields = v.fields.len;
                    let sp = match src {
                        Src::At(p) => p,
                        Src::Temp(id) => Pointer { alloc: id, offset: 0 },
                        Src::Imm(_) => return Err(malformed("a struct unsizing of an immediate").into()),
                    };
                    let mut next_step: Option<(u64, u64, PTy, PTy)> = None;
                    for fi in 0..nfields {
                        let fty_s = self.field_ty(ty, 0, fi)?;
                        let fty_t = self.field_ty(next, 0, fi)?;
                        if fty_s == fty_t {
                            let fsz = self.size(fty_s)?;
                            if fsz != 0 {
                                let soff = self.field_offset(ty, 0, fi)?;
                                let doff = self.field_offset(next, 0, fi)?;
                                self.mem_copy(sp.at(soff), dst.at(doff), fsz)?;
                            }
                        } else if self.size(fty_s)? == 0 && self.size(fty_t)? == 0 {
                            continue;
                        } else {
                            if next_step.is_some() {
                                return Err(unsupported("an unsizing coercion through a struct where more than one field differs").into());
                            }
                            let soff = self.field_offset(ty, 0, fi)?;
                            let doff = self.field_offset(next, 0, fi)?;
                            next_step = Some((soff, doff, fty_s, fty_t));
                        }
                    }
                    let Some((soff, doff, fty_s, fty_t)) = next_step else {
                        return Err(unsupported("an unsizing coercion through a struct where no field holding bytes differs").into());
                    };
                    src = Src::At(sp.at(soff));
                    dst = dst.at(doff);
                    ty = fty_s;
                    next = fty_t;
                }
                _ => return Err(unsupported("an unsizing coercion of a value that is not a pointer or a struct holding one").into()),
            }
        }
        Err(unsupported("an unsizing coercion through more nested structs than the bound").into())
    }

    fn cast(&mut self, d: Pl, kind: CastKind, operand: Operand) -> R<'a, ()> {
        let o = self.operand(operand)?;
        let res = (|| -> R<'a, ()> {
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
                CastKind::Transmute => self.transmute_into(d, o)?,
                CastKind::Unsize => self.unsize_into(d, o)?,
                CastKind::ReifyFnPointer => {
                    let PNode::FnDef(k, l) = from else {
                        return Err(malformed("a fn pointer made of a value that is no fn item").into());
                    };
                    let (k, l) = self.resolve(k, l)?;
                    let inst = instance_key(self.world.code_id(k), l, self.ground.view(), self.alloc).map_err(build_refusal)?;
                    let named = ConstFn { key: inst, instance: Instance { item: k, args: l } };
                    let f = self.fn_alloc(inst, Some(named))?;
                    self.mem_write(d.ptr, PTR, Scalar::Ptr(Pointer { alloc: f, offset: 0 }))?;
                }
                CastKind::ClosureFnPointer => {
                    let PNode::Closure { owner, .. } = from else {
                        return Err(malformed("a closure's fn pointer made of a value that is not a closure").into());
                    };
                    if self.size(o.ty)? != 0 {
                        return Err(unsupported("a closure with captures coerced to a fn pointer").into());
                    }
                    let PNode::FnPtr(sig, _) = to else {
                        return Err(malformed("a closure coerced to a type that is not a fn pointer").into());
                    };
                    let call_once = match self.world.fn_trait_method(FnTrait::FnOnce) {
                        FnTraitMethod::Found(item) => item,
                        FnTraitMethod::Absent => {
                            return Err(unsupported(
                                "a closure as a fn pointer in a world that names no `FnOnce::call_once` (ConstWorld::fn_trait_method)",
                            )
                            .into());
                        }
                        FnTraitMethod::Pending { trait_ } => {
                            return Err(Refusal::Missing {
                                what: "the trait members for `FnOnce::call_once` (ConstWorld::fn_trait_method)",
                                item: trait_,
                            }
                            .into());
                        }
                    };
                    let mut inputs: Vec<PTy, Alloc<'a>> = Vec::new_in(self.alloc);
                    let all = self.ground.view().list(sig).map_err(|_| malformed("a list past the ground table"))?;
                    let (_, ins) = all.split_last().ok_or(Stop::from(malformed("a fn pointer type without its output")))?;
                    inputs.extend_from_slice(ins);
                    let list = self.ground.intern_list(&inputs).map_err(|_| malformed("the ground table is full"))?;
                    let tuple = self.ground.intern(PNode::Tuple(list)).map_err(|_| malformed("the ground table is full"))?;
                    let own = self.ground.intern_list(&[o.ty, tuple]).map_err(|_| malformed("the ground table is full"))?;
                    let method = Instance { item: call_once, args: own };
                    let key_args = closure_method_key_args(method, &mut *self.ground, self.alloc)
                        .map_err(|_| malformed("a closure's `call_once` instance past the ground table"))?;
                    let inst = instance_key(self.world.code_id(owner), key_args, self.ground.view(), self.alloc).map_err(build_refusal)?;
                    let named = ConstFn { key: inst, instance: method };
                    let f = self.fn_alloc(inst, Some(named))?;
                    self.mem_write(d.ptr, PTR, Scalar::Ptr(Pointer { alloc: f, offset: 0 }))?;
                }
            }
            Ok(())
        })();
        self.release(o);
        res
    }

    /// `lhs op rhs`: the result and whether it overflowed. A wrapping operator's result is
    /// the wrapped value; `checked` makes a shift past the width an overflow, not a refusal.
    pub(crate) fn binary(&mut self, op: BinOp, lhs: Operand, rhs: Operand, checked: bool) -> R<'a, (Scalar, bool)> {
        let a = self.operand(lhs)?;
        let b = match self.operand(rhs) {
            Ok(b) => b,
            Err(e) => {
                self.release(a);
                return Err(e);
            }
        };
        let x = match self.read(a) {
            Ok(x) => x,
            Err(e) => {
                self.release(a);
                self.release(b);
                return Err(e);
            }
        };
        let y = self.read(b);
        self.release(a);
        self.release(b);
        let y = y?;
        let (na, nb) = (self.node(a.ty)?, self.node(b.ty)?);
        match (x, y) {
            (Scalar::Int(x), Scalar::Int(y)) => self.arith(op, na, nb, x, y, checked),
            (Scalar::Ptr(p), Scalar::Int(n)) if op == BinOp::Offset => {
                let (PNode::Ref(_, t) | PNode::Ptr(_, t)) = na else {
                    return Err(malformed("an offset of a value that is no pointer").into());
                };
                let size = self.size(t)?;
                let n64 = u64::from_le_bytes({
                    let bytes = n.to_le_bytes();
                    [bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7]]
                });
                Ok((Scalar::Ptr(p.at(n64.wrapping_mul(size))), false))
            }
            (Scalar::Ptr(p), Scalar::Ptr(q)) => {
                // Two pointers compare only within one allocation in a constant.
                if p.alloc != q.alloc {
                    return Err(Refusal::PointerCompare.into());
                }
                let o = p.offset.cmp(&q.offset);
                let holds = holds(op, Some(o)).ok_or(Stop::from(Refusal::PointerAsInt))?;
                Ok((Scalar::Int(u128::from(holds)), false))
            }
            _ => Err(Refusal::PointerAsInt.into()),
        }
    }

    fn arith(&mut self, op: BinOp, na: PNode, nb: PNode, x: u128, y: u128, checked: bool) -> R<'a, (Scalar, bool)> {
        if let PNode::Float(f) = na {
            if let Some(h) = holds(op, arith::float_cmp(f, x, y)) {
                return Ok((Scalar::Int(u128::from(h)), false));
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
            return Ok((Scalar::Int(u128::from(h)), false));
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
                let amount = if rhs.1 { arith::sext(y, rhs.0) } else { i128::try_from(y).unwrap_or(i128::MAX) };
                let far = amount < 0 || amount >= i128::from(bits);
                if far && !checked {
                    return Err(Refusal::ShiftTooFar { amount, bits }.into());
                }
                // A checked shift past the width shifts by the amount masked (`overflowing_shl`).
                let y = if far { y & (u128::from(bits) - 1) } else { y };
                Ok((Scalar::Int(arith::int_binop(typeck_op(op)?, x, y, bits, signed, rhs, Plant::None)?), far))
            }
            BinOp::Div | BinOp::Rem | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor => {
                Ok((Scalar::Int(arith::int_binop(typeck_op(op)?, x, y, bits, signed, rhs, Plant::None)?), false))
            }
            _ => Err(malformed("an operator with no integer meaning").into()),
        }
    }

    // ---- The value ----

    /// The root's `_0` and every allocation it reaches, as a data record: allocation 0 is the
    /// value. A pointer into a dead allocation (such as a scoped local whose storage ended or a
    /// callee's frame) dangles once the constant is evaluated, and is refused; a live unscoped
    /// local of the root constant body (such as a promoted borrow inside a constant initializer)
    /// is interned alongside the root value.
    fn intern(&mut self, root: AllocId, root_ty: PTy, is_static: bool) -> R<'a, DataRecord<Alloc<'a>>> {
        let mut order: Vec<AllocId, Alloc<'a>> = Vec::new_in(self.alloc);
        order.push(root);
        // blaze-build's data form holds the standard vector (data.rs), so the record is built in
        // it directly, in the same allocator: no copy from blaze-own's.
        let mut allocs: alloc::vec::Vec<DataAlloc<Alloc<'a>>, Alloc<'a>> = alloc::vec::Vec::new_in(self.alloc);
        let mut k = 0;
        while let Some(&id) = order.get(k) {
            k += 1;
            let align = self.effective_align_log2(id)?;
            let mut bytes: alloc::vec::Vec<u8, Alloc<'a>> = alloc::vec::Vec::new_in(self.alloc);
            bytes.extend_from_slice(self.mem.bytes(id)?);
            self.mem.zero_unwritten(id, &mut bytes);
            let mut relocs: alloc::vec::Vec<DataReloc, Alloc<'a>> = alloc::vec::Vec::new_in(self.alloc);
            for &(offset, to) in self.mem.relocs(id)? {
                let target_alloc = self.mem.get(to)?;
                let target = match target_alloc.origin {
                    Origin::Static(s) => DataTarget::Static(s),
                    Origin::Fn(i) => DataTarget::Fn(i),
                    Origin::Vtable(v) => DataTarget::Vtable(v),
                    Origin::Data | Origin::Local => {
                        let _ = self.mem.bytes(to)?;
                        let n = match order.iter().position(|&x| x == to) {
                            Some(n) => n,
                            None => {
                                order.push(to);
                                order.len() - 1
                            }
                        };
                        DataTarget::Local(u32::try_from(n).map_err(|_| malformed("allocation index overflow"))?)
                    }
                };
                let off32 = u32::try_from(offset).map_err(|_| unsupported("a pointer past 4 GiB in a constant"))?;
                relocs.push(DataReloc { offset: off32, target });
            }
            allocs.push(DataAlloc { bytes, align, mutable: false, relocs });
        }
        if is_static {
            let written = self.written_in_place(&allocs, root_ty)?;
            for (a, &w) in allocs.iter_mut().zip(written.iter()) {
                a.mutable = w;
            }
        }
        Ok(DataRecord { allocs })
    }

    /// Which allocations of a static's record, of type `t`, are written in place (matching
    /// `blaze-consteval`'s `Machine::written_in_place`): non-`Freeze` statics (`UnsafeCell`),
    /// pointees behind `&mut`/`*mut`, pointees whose type is not `Freeze` (`&AtomicU8`), and
    /// allocations reached only through untyped/unsized indirections (trait objects, unsized
    /// structs, closures).
    fn written_in_place(&mut self, allocs: &[DataAlloc<Alloc<'a>>], t: PTy) -> R<'a, Vec<bool, Alloc<'a>>> {
        /// The most parts one walk visits (rule 18).
        const MOST_PARTS: usize = 1 << 20;
        let n = allocs.len();
        let mut written: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(n, self.alloc);
        let mut typed: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(n, self.alloc);
        for _ in 0..n {
            written.push(false);
            typed.push(false);
        }
        if n == 0 {
            return Ok(written);
        }
        let root_mut = self.interior_mut(t)?;
        if let Some(slot) = written.get_mut(0) {
            *slot = root_mut;
        }
        if let Some(slot) = typed.get_mut(0) {
            *slot = true;
        }
        let mut todo: Vec<(u32, u64, PTy), Alloc<'a>> = Vec::new_in(self.alloc);
        todo.push((0, 0, t));
        let mut parts = 0usize;
        let too_many = || Stop::from(unsupported("a static whose value has more than 2^20 parts holding pointers to walk"));
        while let Some((i, base, ty)) = todo.pop() {
            parts += 1;
            if parts > MOST_PARTS || todo.len() > MOST_PARTS {
                return Err(too_many());
            }
            let iu = usize::try_from(i).map_err(|_| malformed("a relocation to an allocation past the record"))?;
            let a = allocs.get(iu).ok_or(Stop::from(malformed("a relocation to an allocation past the record")))?;
            let end = base.saturating_add(self.size_align(ty)?.0);
            if !a.relocs.iter().any(|r| (base..end).contains(&u64::from(r.offset))) {
                continue;
            }
            match self.node(ty)? {
                PNode::Ref(m, p) | PNode::Ptr(m, p) => {
                    let Some(r) = a.relocs.iter().find(|r| u64::from(r.offset) == base) else { continue };
                    let DataTarget::Local(j) = r.target else { continue };
                    let ju = usize::try_from(j).map_err(|_| malformed("a relocation to an allocation past the record"))?;
                    let target = allocs.get(ju).ok_or(Stop::from(malformed("a relocation to an allocation past the record")))?;
                    let addend = data_word(&a.bytes, base)?;
                    if let Some(slot) = typed.get_mut(ju) {
                        *slot = true;
                    }
                    let mut mark = m == Mutbl::Mut;
                    match self.node(p)? {
                        PNode::Str => {}
                        PNode::Slice(e) => {
                            mark |= self.interior_mut(e)?;
                            let len = data_word(&a.bytes, base + PTR)?;
                            let es = self.size_align(e)?.0;
                            let to = addend.saturating_add(len.saturating_mul(es));
                            if es != 0 && target.relocs.iter().any(|r| (addend..to).contains(&u64::from(r.offset))) {
                                if len > u64::try_from(MOST_PARTS).unwrap_or(u64::MAX) {
                                    return Err(too_many());
                                }
                                for k in 0..len {
                                    todo.push((j, addend + k * es, e));
                                }
                            }
                        }
                        PNode::Dynamic(_) => mark = true,
                        _ if self.is_unsized(p)? => mark = true,
                        _ => {
                            mark |= self.interior_mut(p)?;
                            todo.push((j, addend, p));
                        }
                    }
                    if let Some(slot) = written.get_mut(ju) {
                        *slot |= mark;
                    }
                }
                PNode::Tuple(l) | PNode::Record(_, l) => {
                    for f in 0..l.len {
                        let off = self.field_offset(ty, 0, f)?;
                        let ft = self.field_ty(ty, 0, f)?;
                        todo.push((i, base + off, ft));
                    }
                }
                PNode::Union(l) => {
                    let variants = l.len;
                    let tag = self.tag_of(ty)?;
                    let live = match tag.at {
                        None if variants == 0 => 0..0,
                        None => {
                            let index = tag.variant(0, &[])?;
                            index..index + 1
                        }
                        Some(_) => 0..variants,
                    };
                    for v in live {
                        let off = self.field_offset(ty, v, 0)?;
                        let ft = self.field_ty(ty, v, 0)?;
                        todo.push((i, base + off, ft));
                    }
                }
                PNode::Array(e, _) => {
                    let es = self.size_align(e)?.0;
                    if es != 0 {
                        let count = (end - base) / es;
                        if count > u64::try_from(MOST_PARTS).unwrap_or(u64::MAX) {
                            return Err(too_many());
                        }
                        for k in 0..count {
                            todo.push((i, base + k * es, e));
                        }
                    }
                }
                PNode::Adt(key, _) => {
                    let adt = self
                        .world
                        .adt(key)
                        .ok_or(Stop::from(Refusal::Missing { what: "ADT definition", item: key }))?;
                    let variants = u32::try_from(adt.variants.len()).map_err(|_| malformed("an ADT variant count past u32"))?;
                    let tag = self.tag_of(ty)?;
                    let live = match tag.at {
                        None => {
                            let index = tag.variant(0, &[])?;
                            index..index + 1
                        }
                        Some(_) => 0..variants,
                    };
                    for v in live {
                        let v_idx = usize::try_from(v).map_err(|_| malformed("a variant past its ADT"))?;
                        let v_fields = self
                            .world
                            .adt(key)
                            .and_then(|a| a.variants.get(v_idx))
                            .map(|var| var.fields.len)
                            .ok_or(Stop::from(malformed("a variant past its ADT")))?;
                        for f in 0..v_fields {
                            let off = self.field_offset(ty, v, f)?;
                            let ft = self.field_ty(ty, v, f)?;
                            todo.push((i, base + off, ft));
                        }
                    }
                }
                PNode::Pat(e, _) => todo.push((i, base, e)),
                // A closure's environment, a coroutine: any allocation they point to stays untyped
                // and is marked writable below.
                _ => {}
            }
        }
        for (w, &t) in written.iter_mut().zip(typed.iter()).skip(1) {
            *w |= !t;
        }
        Ok(written)
    }

    fn interior_mut(&mut self, ty: PTy) -> R<'a, bool> {
        let mut cx = LayoutCx { ground: &mut *self.ground, facts: self.world, scratch: self.alloc };
        self.layout.interior_mut(ty, &mut cx).map_err(layout_stop)
    }

    fn tag_of(&mut self, ty: PTy) -> R<'a, Tag> {
        let mut cx = LayoutCx { ground: &mut *self.ground, facts: self.world, scratch: self.alloc };
        self.layout.tag(ty, &mut cx).map_err(layout_stop)
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

    pub(crate) fn closure_env_ty(&mut self, ty: PTy) -> R<'a, PTy> {
        let PNode::Closure { owner, ordinal, args } = self.node(ty)? else {
            return Err(malformed("a closure's environment asked of another type").into());
        };
        let c = self
            .world
            .closure(owner, ordinal)
            .ok_or(Stop::from(unsupported("a closure whose facts the world did not answer")))?;
        let mut argv: Vec<PTy, Alloc<'a>> = Vec::new_in(self.alloc);
        argv.extend_from_slice(
            self.ground
                .view()
                .list(args)
                .map_err(|_| malformed("a closure's arguments past the ground table"))?,
        );
        Ok(ground(c.types, c.env, &argv, &mut *self.ground, self.alloc, self.world).map_err(build_refusal)?)
    }

    pub(crate) fn field_ty(&mut self, ty: PTy, variant: u32, field: u32) -> R<'a, PTy> {
        match self.node(ty)? {
            PNode::Tuple(l) | PNode::Record(_, l) => {
                let idx = usize::try_from(field).map_err(|_| malformed("a tuple field past its tuple"))?;
                self.ground
                    .view()
                    .list(l)
                    .ok()
                    .and_then(|s| s.get(idx).copied())
                    .ok_or(Stop::from(malformed("a tuple field past its tuple")))
            }
            PNode::Union(l) => {
                if field != 0 {
                    return Err(malformed("a union variant field other than 0").into());
                }
                let idx = usize::try_from(variant).map_err(|_| malformed("a union variant past its union"))?;
                self.ground
                    .view()
                    .list(l)
                    .ok()
                    .and_then(|s| s.get(idx).copied())
                    .ok_or(Stop::from(malformed("a union variant past its union")))
            }
            PNode::Adt(key, args) => {
                let adt = self
                    .world
                    .adt(key)
                    .ok_or(Stop::from(Refusal::Missing { what: "ADT definition", item: key }))?;
                let v_idx = usize::try_from(variant).map_err(|_| malformed("a variant past its ADT"))?;
                let v = adt.variants.get(v_idx).ok_or(Stop::from(malformed("a variant past its ADT")))?;
                if field >= v.fields.len {
                    return Err(malformed("a field past its variant").into());
                }
                let f_idx = usize::try_from(v.fields.start + field).map_err(|_| malformed("a field past its ADT"))?;
                let f = adt.fields.get(f_idx).ok_or(Stop::from(malformed("a field past its ADT")))?;
                let mut argv: Vec<PTy, Alloc<'a>> = Vec::new_in(self.alloc);
                argv.extend_from_slice(
                    self.ground
                        .view()
                        .list(args)
                        .map_err(|_| malformed("an ADT's arguments past the ground table"))?,
                );
                Ok(ground(adt.types, f.ty, &argv, &mut *self.ground, self.alloc, self.world).map_err(build_refusal)?)
            }
            PNode::Closure { .. } => {
                let env = self.closure_env_ty(ty)?;
                let PNode::Tuple(l) = self.node(env)? else {
                    return Err(malformed("a closure's environment that is not a tuple").into());
                };
                let idx = usize::try_from(field).map_err(|_| malformed("a closure capture past its environment"))?;
                self.ground
                    .view()
                    .list(l)
                    .ok()
                    .and_then(|s| s.get(idx).copied())
                    .ok_or(Stop::from(malformed("a closure capture past its environment")))
            }
            _ => Err(malformed("a field type asked of a type without fields").into()),
        }
    }

    pub(crate) fn node(&self, ty: PTy) -> R<'a, PNode> {
        Ok(self.ground.view().node(ty).map_err(|_| malformed("a type past the ground table"))?)
    }

    pub(crate) fn is_unsized(&mut self, ty: PTy) -> R<'a, bool> {
        Ok(blaze_consteval::layout::is_unsized(self.world, &mut *self.ground, ty, self.alloc)?)
    }

    pub(crate) fn size_align(&mut self, ty: PTy) -> R<'a, (u64, u8)> {
        let mut cx = LayoutCx { ground: &mut *self.ground, facts: self.world, scratch: self.alloc };
        self.layout.size_align(ty, &mut cx).map_err(layout_stop)
    }

    pub(crate) fn size(&mut self, ty: PTy) -> R<'a, u64> {
        Ok(self.size_align(ty)?.0)
    }

    pub(crate) fn field_offset(&mut self, ty: PTy, variant: u32, field: u32) -> R<'a, u64> {
        let mut cx = LayoutCx { ground: &mut *self.ground, facts: self.world, scratch: self.alloc };
        self.layout.field_offset(ty, variant, field, &mut cx).map_err(layout_stop)
    }
}

/// The little-endian word at `at` of `bytes` (a relocation's addend, a slice's length).
fn data_word<'a>(bytes: &[u8], at: u64) -> R<'a, u64> {
    let at = usize::try_from(at).map_err(|_| malformed("a word past its allocation"))?;
    let end = at.checked_add(8).ok_or(Stop::from(malformed("a word past its allocation")))?;
    let w = bytes.get(at..end).ok_or(Stop::from(malformed("a word past its allocation")))?;
    let arr: [u8; 8] = w.try_into().map_err(|_| malformed("a word past its allocation"))?;
    Ok(u64::from_le_bytes(arr))
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
