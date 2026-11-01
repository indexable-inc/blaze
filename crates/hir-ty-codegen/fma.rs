//! A fused multiply-add (`fma`, `simd_fma`) in an x86-64 function with FMA, as LLVM makes it
//! there at `-O0`: one `vfmadd213` of the operands, or, where it folds the negation of an
//! operand into it, `vfmsub213`, `vfnmadd213` or `vfnmsub213` of the operand before that
//! negation. The instruction keeps the first NaN of the product's first factor, its second
//! and the addend, in that order, quieted and with its sign: a negation it folds never flips
//! the sign of a NaN, one it leaves to `vpxor` before it does. LLVM's patterns put the
//! operands of `fma(x, y, z)` into each form (`213`, `132`, `231`, masked or not) so that `x`
//! is the first factor, except where a scalar's result goes into lane 0 of a vector and `x`
//! is lane 0 of that vector (`_mm_fmadd_sh`): the vector is the instruction's first operand
//! and `y` the first factor (the `213_Int` pattern, commuted).
//!
//! Which of those it is depends on the code around the call, by a rule of FastISel's, which
//! selects a block's instructions bottom-up one by one: an `llvm.fma` it cannot select is
//! given to SelectionDAG alone, where nothing is folded into it; any other instruction it
//! cannot select (`insertelement`, `extractelement`, `shufflevector`, a `select` of vectors,
//! an `fneg` of a vector of more than 64 bits, a store of an `f16` or of a vector of them, and
//! the terminators it has no lowering for) sends everything from the start of its block to it
//! to one SelectionDAG, where `combineFMA` folds the negations of the operands into the
//! instruction and the masked and alternating forms are matched. An instruction after the
//! fma in its block decides. A negation it folds is one of the operand's own in that block,
//! of an `extractelement` of lane 0 of a negated vector there, or of a product with a negated
//! factor (`getCheaperNegatedExpression`); an operand that might be the last two is refused.
//!
//! rustc's blocks are MIR's, a call's continuing in its block where it is the only way into
//! the next (`mergeable_succ`), and what rustc's MIR inliner put into a function is in it:
//! core is built optimized, and its inliner put a callee into a caller of the same target
//! features, `#[inline]` and cheap enough (`rustc_mir_transform::inline`), as
//! `_mm512_fmadd_ps` is in `_mm512_mask_fmadd_ps`. A callee of one fma is made here, in the
//! caller; one of more (`_mm512_fmaddsub_ps`) is called where each of its fmas is what it is
//! in the callee alone, decided there by what follows it, of operands the caller passes as
//! they are, its result going only to shuffles. A crate of the program's own is taken as the
//! dev profile builds it, where that inliner does not run.

#![forbid(unsafe_code)]

use base_db::Db;
use hir_ty::TargetFeature;
use smallvec::SmallVec;

use super::*;

/// What runs after the fma, in its block: whether FastISel can select it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Step {
    /// An instruction FastISel cannot select: the fma goes to SelectionDAG with it.
    Fails,
    /// Selected by FastISel, or by SelectionDAG on its own as a call is.
    Selected,
    /// Not known to be either.
    Unknown,
    /// The end of the block, which FastISel selects.
    End,
    /// Maybe the end of the block, maybe not, or an end FastISel may not select.
    Unclear,
}

/// Whether a local of rustc's is a value in a register or a place in memory
/// (`rustc_codegen_ssa::mir::analyze::non_ssa_locals`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Value,
    Memory,
}

/// What a type is to the instructions FastISel selects of it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Class {
    Empty,
    /// `f16` or a vector of them.
    Half,
    /// `f32`, `f64` or a vector of them.
    Float,
    Other,
}

/// Where a local gets its value, for one assigned once.
#[derive(Clone, Copy)]
enum Def {
    Param,
    Statement(BasicBlockId, usize),
    Call(BasicBlockId),
}

/// What a body says of its blocks and locals for the rule.
struct Facts<'db> {
    body: &'db MirBody<'db>,
    func: Option<FunctionId>,
    preds: HashMap<BasicBlockId, u32>,
    /// The first MIR block of the block of rustc's each is part of.
    head: HashMap<BasicBlockId, BasicBlockId>,
    defs: HashMap<LocalId, Vec<Def>>,
    /// Locals used through a projection or borrowed.
    tangled: HashSet<LocalId>,
}

fn mark_projected(facts: &mut Facts<'_>, op: &Operand) {
    if let OperandKind::Copy(place) | OperandKind::Move(place) = &op.kind
        && !place.projection.as_slice().is_empty()
    {
        facts.tangled.insert(place.local);
    }
}

/// An operand of the fma as SelectionDAG has it.
#[derive(Clone, Copy, Debug)]
enum Traced {
    /// Not negated: the local its value comes from, and the vector it is lane 0 of when it is
    /// one's `extractelement` in the block.
    Plain { root: Option<LocalId>, lane_of: Option<LocalId> },
    /// A negation in the block, of the value from `root`, with the vector that value is lane 0
    /// of as for `Plain`: `combineFMA` folds the `fneg` and leaves the `extractelement`.
    Negated { root: Option<LocalId>, lane_of: Option<LocalId> },
}

/// A wrapper of one fma that rustc's MIR inliner puts into its caller: each operand of the fma
/// is which of its parameters and whether it negates that.
struct Wrapper {
    params: [usize; 3],
    negated: [bool; 3],
}

/// The fma to make: its operands, those of the call where rustc inlined a wrapper of it, and
/// the negations the wrapper does of them.
pub(super) struct Site<'a> {
    pub(super) operands: [&'a Operand; 3],
    pub(super) negated: [bool; 3],
    pub(super) destination: &'a StoredPlace,
    pub(super) target: Option<BasicBlockId>,
}

const LIMIT: usize = 10_000;

fn callee_of(func: &Operand) -> Option<(FunctionId, bool)> {
    let ty = match &func.kind {
        OperandKind::Constant { ty, .. } => ty.as_ref(),
        OperandKind::Allocation { allocation } => allocation.as_ref().ty,
        _ => return None,
    };
    let TyKind::FnDef(def, args) = ty.kind() else { return None };
    let CallableDefId::FunctionId(func) = def.0 else { return None };
    Some((func, args.skip_binder().is_empty()))
}

/// Whether `intrinsic` is a fused multiply-add: `Some(true)` for one that always fuses,
/// `Some(false)` for one LLVM may or may not fuse, `None` for any other intrinsic.
fn fused(intrinsic: Intrinsic) -> Option<bool> {
    match intrinsic {
        Intrinsic::Fma | Intrinsic::Simd(SimdOp::Fma) => Some(true),
        Intrinsic::Fmuladd | Intrinsic::Simd(SimdOp::RelaxedFma) => Some(false),
        _ => None,
    }
}

/// What FastISel makes of a call of `intrinsic` after the fma; `None` for `simd_neg`, which
/// depends on the type it negates.
fn intrinsic_step(intrinsic: Intrinsic) -> Option<Step> {
    match intrinsic {
        Intrinsic::Simd(
            SimdOp::Insert
            | SimdOp::Extract
            | SimdOp::Shuffle
            | SimdOp::SelectBitmask
            | SimdOp::Select,
        ) => Some(Step::Fails),
        Intrinsic::Simd(SimdOp::Neg) => None,
        _ if fused(intrinsic) == Some(true) => Some(Step::Selected),
        _ => Some(Step::Unknown),
    }
}

fn plain_local(operand: &Operand) -> Option<LocalId> {
    match &operand.kind {
        OperandKind::Copy(place) | OperandKind::Move(place)
            if place.projection.as_slice().is_empty() =>
        {
            Some(place.local)
        }
        _ => None,
    }
}

impl<'db> Facts<'db> {
    fn new(body: &'db MirBody<'db>) -> Self {
        let mut facts = Facts {
            body,
            func: body.owner.as_function(),
            preds: HashMap::default(),
            head: HashMap::default(),
            defs: HashMap::default(),
            tangled: HashSet::default(),
        };
        for &param in &body.param_locals {
            facts.defs.entry(param).or_default().push(Def::Param);
        }
        let operand = mark_projected;
        // The block each goes on in, where it goes there alone: a call of an intrinsic never
        // unwinds, whatever the MIR says.
        let mut goes_on: HashMap<BasicBlockId, BasicBlockId> = HashMap::default();
        for (id, block) in body.basic_blocks.iter() {
            for (at, statement) in block.statements.iter().enumerate() {
                let StatementKind::Assign(place, rvalue) = &statement.kind else { continue };
                if place.projection.as_slice().is_empty() {
                    facts.defs.entry(place.local).or_default().push(Def::Statement(id, at));
                } else {
                    facts.tangled.insert(place.local);
                }
                match rvalue {
                    Rvalue::Use(op)
                    | Rvalue::Repeat(op, _)
                    | Rvalue::Cast(_, op, _)
                    | Rvalue::UnaryOp(_, op) => operand(&mut facts, op),
                    Rvalue::BinaryOp(_, a, b) | Rvalue::CheckedBinaryOp(_, a, b) => {
                        operand(&mut facts, a);
                        operand(&mut facts, b);
                    }
                    Rvalue::Aggregate(_, ops) => ops.iter().for_each(|op| operand(&mut facts, op)),
                    Rvalue::Ref(_, place)
                    | Rvalue::Len(place)
                    | Rvalue::Discriminant(place)
                    | Rvalue::CopyForDeref(place) => {
                        facts.tangled.insert(place.local);
                    }
                    Rvalue::ThreadLocalRef(never)
                    | Rvalue::AddressOf(never)
                    | Rvalue::NullaryOp(never) => match *never {},
                }
            }
            let Some(terminator) = &block.terminator else { continue };
            match &terminator.kind {
                TerminatorKind::Goto { target } => _ = goes_on.insert(*target, id),
                TerminatorKind::Call { target: Some(target), cleanup, intrinsic, .. }
                    if cleanup.is_none() || intrinsic.is_some() =>
                {
                    goes_on.insert(*target, id);
                }
                _ => {}
            }
            match &terminator.kind {
                TerminatorKind::Call { args, destination, .. } => {
                    args.iter().for_each(|op| operand(&mut facts, op));
                    if destination.projection.as_slice().is_empty() {
                        facts.defs.entry(destination.local).or_default().push(Def::Call(id));
                    } else {
                        facts.tangled.insert(destination.local);
                    }
                }
                TerminatorKind::Drop { place, .. }
                | TerminatorKind::DropAndReplace { place, .. } => {
                    facts.tangled.insert(place.local);
                }
                _ => {}
            }
            for next in terminator.kind.successors().into_iter().chain(terminator.kind.unwind()) {
                *facts.preds.entry(next).or_default() += 1;
            }
        }
        // A block is in its predecessor's where that is its only way in and goes only there.
        for (id, _) in body.basic_blocks.iter() {
            let mut at = id;
            for _ in 0..body.basic_blocks.len() {
                match goes_on.get(&at) {
                    Some(&pred) if pred != id && facts.preds.get(&at).copied() == Some(1) => {
                        at = pred
                    }
                    _ => break,
                }
            }
            facts.head.insert(id, at);
        }
        facts
    }

    fn kind(&self, local: LocalId) -> Option<Kind> {
        if self.tangled.contains(&local) {
            return None;
        }
        match self.defs.get(&local).map_or(0, Vec::len) {
            0 => None,
            1 => Some(Kind::Value),
            _ => Some(Kind::Memory),
        }
    }

    fn single_def(&self, local: LocalId) -> Option<Def> {
        match self.defs.get(&local)?.as_slice() {
            [def] => Some(*def),
            _ => None,
        }
    }

    fn rvalue(&self, block: BasicBlockId, at: usize) -> Option<&'db Rvalue> {
        match &self.body.basic_blocks[block].statements.get(at)?.kind {
            StatementKind::Assign(_, rvalue) => Some(rvalue),
            _ => None,
        }
    }

    /// The callee, arguments and intrinsic of the call `block` ends in.
    fn call(
        &self,
        block: BasicBlockId,
    ) -> Option<(&'db Operand, &'db [Operand], Option<Intrinsic>)> {
        match &self.body.basic_blocks[block].terminator.as_ref()?.kind {
            TerminatorKind::Call { func, args, intrinsic, .. } => Some((func, args, *intrinsic)),
            _ => None,
        }
    }

    /// The local a value is copied from, through copies, to where it was made.
    fn root(&self, mut local: LocalId) -> LocalId {
        for _ in 0..LIMIT {
            let Some(Def::Statement(block, at)) = self.single_def(local) else { break };
            let Some(Rvalue::Use(op)) = self.rvalue(block, at) else { break };
            let Some(from) = plain_local(op) else { break };
            local = from;
        }
        local
    }
}

/// One function's body being walked, or one rustc's inliner put into it.
struct Frame {
    /// The facts of the body: the walk's own, or (`Some`) the inlined body's of that index in
    /// the walk's list of them, which grows and shrinks with the frames.
    facts: Option<u32>,
    block: BasicBlockId,
    statement: usize,
    /// For an inlined body: what its return place is in its caller, and where the caller
    /// goes on.
    returns: Option<(Option<Kind>, BasicBlockId)>,
}

/// The facts of `frame`'s body, of a walk whose own are `outer` and inlined ones `inlined`.
fn facts_of<'f, 'db>(
    outer: &'f Facts<'db>,
    inlined: &'f [Facts<'db>],
    frame: &Frame,
) -> &'f Facts<'db> {
    match frame.facts {
        None => outer,
        Some(at) => &inlined[at as usize],
    }
}

/// Keeps `why` as the reason what follows is not known, unless one is kept: the first is the
/// one given.
fn note(unknown: &mut Option<String>, why: impl FnOnce() -> String) {
    if unknown.is_none() {
        *unknown = Some(why());
    }
}

impl<'db> FunctionCx<'_, 'db> {
    /// Whether the function has FMA or FMA4: by its crate, or by its own `#[target_feature]`
    /// and what that implies, as AVX-512 does FMA.
    pub(super) fn x86_fused(&self) -> bool {
        self.cg.target.arch == Arch::X86_64
            && (self.enables(TargetFeature::FMA) || self.enables(TargetFeature::FMA4))
    }

    /// Whether the function's crate or the function has `feature`.
    fn enables(&self, feature: TargetFeature) -> bool {
        hir_ty::crate_target_features(self.cg.db, self.env.krate).contains(feature)
            || self.function_enables(feature)
    }

    /// The fma `intrinsic` of the intrinsic call this block ends in, into `dst`.
    pub(super) fn x86_fma(
        &mut self,
        intrinsic: Intrinsic,
        args: &[Operand],
        dst: Value,
        dst_ty: Ty<'db>,
    ) -> Result<()> {
        let [a, b, c] = args else { unsupported!("`{intrinsic}` with {} arguments", args.len()) };
        if fused(intrinsic) == Some(false) {
            unsupported!(
                "`{intrinsic}` in an x86-64 function with FMA, which LLVM may or may not fuse \
                 there"
            );
        }
        let body = self.body.expect("MIR body");
        let Some(block) = self.block else { unsupported!("`{intrinsic}` outside a block") };
        let Some(TerminatorKind::Call { destination, target, .. }) =
            body.basic_blocks[block].terminator.as_ref().map(|it| &it.kind)
        else {
            unsupported!("`{intrinsic}` not called by its block's terminator");
        };
        let site = Site { operands: [a, b, c], negated: [false; 3], destination, target: *target };
        self.lower_fma(intrinsic, &site, dst, dst_ty)
    }

    /// Makes the fma of a call of `func` here when rustc's MIR inliner puts the body of `func`,
    /// a wrapper of one, into this function: true if it did so.
    pub(super) fn x86_inlined_fma(
        &mut self,
        func: FunctionId,
        args: &[Operand],
        destination: &StoredPlace,
        target: Option<BasicBlockId>,
        dst: Value,
        dst_ty: Ty<'db>,
    ) -> Result<bool> {
        let db = self.cg.db;
        let caller = self.body.and_then(|it| it.owner.as_function());
        if !self.x86_fused() {
            return Ok(false);
        }
        match hir_ty::intrinsic::intrinsic_of(db, func) {
            Ok(None) => {}
            Ok(Some(_)) => return Ok(false),
            Err(e) => unsupported!("{e}"),
        }
        if !caller.is_some_and(|it| it.krate(db).data(db).origin.is_lang()) {
            return Ok(false);
        }
        if !self.has_fma(func)? {
            return Ok(false);
        }
        let name = FunctionSignature::of(db, func).name.as_str();
        match self.rustc_inlines(caller, func)? {
            Some(false) => return Ok(false),
            None => unsupported!(
                "a call of `{name}`, whose fused multiply-add rustc's MIR inliner may or may not \
                 have put into this function, where LLVM fuses it by its context"
            ),
            Some(true) => {}
        }
        let Some(wrapper) = self.wrapper(func)? else {
            // As it is in `func` alone, so the call of what `func` makes alone is that.
            if let Err(why) = self.decided_inside(func, args) {
                unsupported!(
                    "a call of `{name}`, which rustc's MIR inliner puts into this function, \
                     where LLVM fuses its fused multiply-add by the context: {why}"
                );
            }
            return Ok(false);
        };
        let [Some(x), Some(y), Some(z)] = wrapper.params.map(|at| args.get(at)) else {
            unsupported!("a call of `{name}` with fewer arguments than its fma takes");
        };
        let site = Site {
            operands: [x, y, z],
            negated: wrapper.negated,
            destination,
            target,
        };
        self.lower_fma(Intrinsic::Simd(SimdOp::Fma), &site, dst, dst_ty)?;
        Ok(true)
    }

    fn lower_fma(
        &mut self,
        intrinsic: Intrinsic,
        site: &Site<'_>,
        dst: Value,
        dst_ty: Ty<'db>,
    ) -> Result<()> {
        if self.enables(TargetFeature::FMA4) {
            unsupported!("`{intrinsic}` in an x86-64 function with FMA4");
        }
        let layout = self.layout(dst_ty)?;
        let bytes = layout.size.bytes() as u32;
        let (lane, count, scalar) = match layout.backend_repr {
            BackendRepr::Scalar(scalar) => (self.scalar(scalar)?.ty, 1, true),
            BackendRepr::SimdVector { element, count } => {
                (self.scalar(element)?.ty, count.as_u64(), false)
            }
            _ => unsupported!("`{intrinsic}` of {dst_ty:?}"),
        };
        if !lane.is_float() || lane == types::F128 {
            unsupported!("`{intrinsic}` of {lane} in an x86-64 function with FMA");
        }
        if lane == types::F16 && !self.enables(TargetFeature::AVX512FP16) {
            unsupported!(
                "`{intrinsic}` of f16 in an x86-64 function with FMA and without AVX512-FP16, \
                 which LLVM fuses by its context"
            );
        }
        let vl =
            lane == types::F16 && !scalar && bytes < 64 && !self.enables(TargetFeature::AVX512VL);
        if !scalar && !matches!(bytes, 16 | 32 | 64)
            || bytes == 64 && !self.enables(TargetFeature::AVX512F)
            || vl
        {
            unsupported!("`{intrinsic}` of {dst_ty:?} in an x86-64 function with FMA");
        }
        let body = self.body.expect("MIR body");
        let facts = Facts::new(body);
        let block = self.block.expect("the fma's block");
        let fold = self.folds(&facts, site, block, false).map_err(|why| {
            // alloc: the error's message
            CodegenError::Unsupported(format!(
                "`{intrinsic}` in an x86-64 function with FMA, where whether LLVM folds into it \
                 the negations of its operands is not known: {why}"
            ))
        })?;
        // The negations folded, of the operands as given: those are negated values.
        let mut given = [false; 3];
        let mut swapped = false;
        if fold {
            let mut traced = [Traced::Plain { root: None, lane_of: None }; 3];
            for (i, operand) in site.operands.iter().enumerate() {
                traced[i] = self.trace(&facts, operand, block).map_err(|why| {
                    // alloc: the error's message
                    CodegenError::Unsupported(format!(
                        "`{intrinsic}` in an x86-64 function with FMA, whose operand {why}, \
                         which LLVM may fold into it"
                    ))
                })?;
                if let Traced::Negated { .. } = traced[i] {
                    if site.negated[i] {
                        unsupported!("`{intrinsic}` of a negation of a negation");
                    }
                    given[i] = true;
                }
            }
            swapped = self.fma_consumers(&facts, site, &traced, scalar).map_err(|why| {
                // alloc: the error's message
                CodegenError::Unsupported(format!(
                    "`{intrinsic}` in an x86-64 function with FMA, whose result {why}"
                ))
            })?;
        }
        let mut values = [dst; 3];
        for (i, operand) in site.operands.iter().enumerate() {
            let (addr, _) = self.operand(operand)?;
            // A folded negation of the given value reads the value before it; one the wrapper
            // does that is not folded is done first.
            let flip = if fold { given[i] } else { site.negated[i] };
            values[i] = if flip { self.negated_copy(addr, lane, count) } else { addr };
        }
        let negated = |i: usize| fold && (given[i] || site.negated[i]);
        let op = match (negated(0) != negated(1), negated(2)) {
            (false, false) => "vfmadd",
            (true, false) => "vfnmadd",
            (false, true) => "vfmsub",
            (true, true) => "vfnmsub",
        };
        let suffix = match (lane, scalar) {
            (types::F16, true) => "sh",
            (types::F32, true) => "ss",
            (types::F64, true) => "sd",
            (types::F16, false) => "ph",
            (types::F32, false) => "ps",
            _ => "pd",
        };
        let reg = match bytes {
            64 => "zmm",
            32 => "ymm",
            _ => "xmm",
        };
        // `213` is `op2 * op1 + op3`, the NaN of `op2` first: the first factor there.
        let (first, second) = if swapped { (2, 1) } else { (1, 2) };
        // alloc: the stub's text, the output
        let text = format!(
            "vmovaps {reg}0, {reg}{second}\n{op}213{suffix} {reg}0, {reg}{first}, {reg}3"
        );
        let lowered = avx512::Lowered {
            text,
            // alloc: `avx512::Lowered`'s loads, a `Vec` of its own
            loads: vec![avx512::Load::Vector(bytes); 3],
            out: avx512::Out::Vector(bytes),
        };
        self.call_assembled(&lowered, &values, dst);
        Ok(())
    }

    /// A copy of the `count` lanes of `lane` at `addr` with their signs flipped, as `fneg`.
    fn negated_copy(&mut self, addr: Value, lane: Type, count: u64) -> Value {
        let bytes = lane.bytes();
        let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            bytes * count as u32,
            6,
        ));
        let copy = self.builder.ins().stack_addr(ptr(), slot, 0);
        let int = Type::int(lane.bits() as u16).expect("a float's width");
        for i in 0..count as i32 {
            let offset = i * bytes as i32;
            let bits = self.builder.ins().load(int, MEM, addr, offset);
            let sign = self.builder.ins().iconst(int, 1i64.wrapping_shl(lane.bits() - 1));
            let flipped = self.builder.ins().bxor(bits, sign);
            self.builder.ins().store(MEM, flipped, copy, offset);
        }
        copy
    }

    /// Whether an instruction after the fma at `site`, in `block`, makes FastISel give it to
    /// SelectionDAG with the code before it; why not known if that is not. In a body rustc
    /// `inlined`, what follows its `return` is not known.
    fn folds(
        &mut self,
        facts: &Facts<'db>,
        site: &Site<'_>,
        block: BasicBlockId,
        inlined: bool,
    ) -> std::result::Result<bool, String> {
        let body = facts.body;
        let outer = if inlined {
            None
        } else if matches!(self.ret_mode, PassMode::Indirect) {
            Some(Kind::Memory)
        } else {
            facts.kind(return_slot())
        };
        let mut unknown: Option<String> = None;
        // The result is written where the call puts it.
        let destination = site.destination;
        if !destination.projection.as_slice().is_empty() {
            // alloc: the error's message, made once
            note(&mut unknown, || "its result goes into part of a local".to_owned());
        } else {
            let kind = if destination.local == return_slot() {
                outer
            } else {
                facts.kind(destination.local)
            };
            let ty = body.locals[destination.local].ty.as_ref();
            match self.store(kind, ty) {
                Step::Fails => return Ok(true),
                Step::Selected => {}
                // alloc: the error's message, made once
                _ => note(&mut unknown, || "its result is stored as is not known".to_owned()),
            }
        }
        // alloc: the error's message
        let Some(target) = site.target else { return Err("it does not return".to_owned()) };
        let mut bodies: SmallVec<[Facts<'db>; 2]> = SmallVec::new();
        let mut frames: SmallVec<[Frame; 4]> = SmallVec::new();
        frames.push(Frame { facts: None, block: target, statement: 0, returns: None });
        if facts.preds.get(&target).copied() != Some(1)
            || facts.head.get(&target) != facts.head.get(&block)
        {
            return match unknown {
                None => Ok(false),
                Some(why) => Err(why),
            };
        }
        for _ in 0..LIMIT {
            let frame = frames.last_mut().expect("a frame");
            let here = facts_of(facts, &bodies, frame);
            let frame_body: &'db MirBody<'db> = here.body;
            let data = &frame_body.basic_blocks[frame.block];
            if let Some(statement) = data.statements.get(frame.statement) {
                frame.statement += 1;
                let step = self.statement_step(here, frame.returns, outer, statement);
                match step {
                    Step::Fails => return Ok(true),
                    Step::Selected => continue,
                    Step::Unknown => {
                        // alloc: the error's message, made once
                        note(&mut unknown, || format!("then {:?}", statement.kind));
                        continue;
                    }
                    Step::End => return unknown.map_or(Ok(false), Err),
                    // alloc: the error's message
                    Step::Unclear => return Err(format!("then {:?}", statement.kind)),
                }
            }
            let Some(terminator) = &data.terminator else {
                return Err("a block without end".into());
            };
            let kind = &terminator.kind;
            let goes_on = |facts: &Facts<'db>, next: BasicBlockId| {
                facts.preds.get(&next).copied() == Some(1)
            };
            match kind {
                TerminatorKind::Goto { target } => {
                    let target = *target;
                    let frame = frames.last_mut().expect("a frame");
                    if !goes_on(facts_of(facts, &bodies, frame), target) {
                        return unknown.map_or(Ok(false), Err);
                    }
                    frame.block = target;
                    frame.statement = 0;
                }
                TerminatorKind::SwitchInt { targets, .. } if targets.iter().count() == 1 => {
                    return unknown.map_or(Ok(false), Err);
                }
                TerminatorKind::Assert { .. } => return unknown.map_or(Ok(false), Err),
                TerminatorKind::Return => {
                    let frame = frames.pop().expect("a frame");
                    match frame.returns {
                        Some((_, target)) => {
                            if let Some(at) = frame.facts {
                                bodies.truncate(at as usize);
                            }
                            let caller = frames.last_mut().expect("the caller");
                            if !goes_on(facts_of(facts, &bodies, caller), target) {
                                return unknown.map_or(Ok(false), Err);
                            }
                            caller.block = target;
                            caller.statement = 0;
                        }
                        None if inlined => {
                            // alloc: the error's message
                            return Err("then the end of the body, inlined".to_owned());
                        }
                        None => {
                            return match self.ret_step(facts_of(facts, &bodies, &frame), outer) {
                                Step::End => unknown.map_or(Ok(false), Err),
                                // alloc: the error's message
                                _ => Err("it returns a value FastISel may not".to_owned()),
                            };
                        }
                    }
                }
                TerminatorKind::Call {
                    func,
                    args,
                    destination,
                    target,
                    cleanup,
                    intrinsic,
                    ..
                } => {
                    let Some((callee, monomorphic)) = callee_of(func) else {
                        // alloc: the error's message
                        return Err("then a call of a pointer".to_owned());
                    };
                    let db = self.cg.db;
                    let frame = frames.last().expect("a frame");
                    let here = facts_of(facts, &bodies, frame);
                    let returned = if destination.projection.as_slice().is_empty() {
                        let kind = match (destination.local == return_slot(), frame.returns) {
                            (true, Some((kind, _))) => kind,
                            (true, None) => outer,
                            (false, _) => here.kind(destination.local),
                        };
                        let ty = here.body.locals[destination.local].ty.as_ref();
                        self.store(kind, ty)
                    } else {
                        Step::Unknown
                    };
                    let Some(target) = *target else { return unknown.map_or(Ok(false), Err) };
                    if let Some(intrinsic) = *intrinsic {
                        let step = match intrinsic_step(intrinsic) {
                            Some(step) => step,
                            None => {
                                let ty = here.body.locals[destination.local].ty.as_ref();
                                let wide = self.layout(ty).is_ok_and(|it| {
                                    matches!(it.backend_repr, BackendRepr::SimdVector { .. })
                                        && it.size.bytes() > 8
                                });
                                if wide && self.class(ty) != Class::Other {
                                    Step::Fails
                                } else {
                                    Step::Unknown
                                }
                            }
                        };
                        match (step, returned) {
                            (Step::Fails, _) | (_, Step::Fails) => return Ok(true),
                            (Step::Selected, Step::Selected) => {}
                            // alloc: the error's message, made once
                            _ => note(&mut unknown, || format!("then `{intrinsic}`")),
                        }
                    } else {
                        let name = FunctionSignature::of(db, callee).name.as_str();
                        let inlined = if monomorphic {
                            self.rustc_inlines(here.func, callee)
                                .map_err(|e| format!("{e:?}"))? // alloc: the error's message
                        } else {
                            None
                        };
                        match inlined {
                            Some(true) => {
                                let Ok((callee_body, _)) = root_body(db, callee) else {
                                    // alloc: the error's message
                                    return Err(format!("then `{name}`, whose body is not known"));
                                };
                                let kind = if destination.projection.as_slice().is_empty() {
                                    here.kind(destination.local)
                                } else {
                                    None
                                };
                                let at = bodies.len() as u32;
                                bodies.push(Facts::new(callee_body));
                                frames.push(Frame {
                                    block: callee_body.start_block,
                                    facts: Some(at),
                                    statement: 0,
                                    returns: Some((kind, target)),
                                });
                                continue;
                            }
                            Some(false) | None => {
                                if cleanup.is_some() {
                                    // alloc: the error's message
                                    return Err(format!("then `{name}`, which may unwind"));
                                }
                                let halves = args.iter().any(|arg| {
                                    let ty = match &arg.kind {
                                        OperandKind::Copy(place) | OperandKind::Move(place) => {
                                            here.body.locals[place.local].ty.as_ref()
                                        }
                                        OperandKind::Constant { ty, .. } => ty.as_ref(),
                                        OperandKind::Allocation { allocation } => {
                                            allocation.as_ref().ty
                                        }
                                        _ => return true,
                                    };
                                    matches!(arg.kind, OperandKind::Copy(_) | OperandKind::Move(_))
                                        && self.class(ty) == Class::Half
                                });
                                if returned == Step::Fails {
                                    return Ok(true);
                                }
                                if inlined.is_none() || halves || returned != Step::Selected {
                                    // alloc: the error's message, made once
                                    note(&mut unknown, || format!("then `{name}`"));
                                }
                            }
                        }
                    }
                    let frame = frames.last_mut().expect("a frame");
                    if !goes_on(facts_of(facts, &bodies, frame), target) {
                        return unknown.map_or(Ok(false), Err);
                    }
                    frame.block = target;
                    frame.statement = 0;
                }
                // alloc: the error's message
                kind => return Err(format!("then {kind:?}")),
            }
        }
        // alloc: the error's message
        Err("a body too long to follow".to_owned())
    }

    /// What FastISel makes of writing a value of `ty` to a local of `kind`.
    fn store(&self, kind: Option<Kind>, ty: Ty<'db>) -> Step {
        match (kind, self.class(ty)) {
            (_, Class::Empty) | (Some(Kind::Value), _) => Step::Selected,
            // `X86FastEmitStore` has no store of an `f16` or a vector of them.
            (Some(Kind::Memory), Class::Half) => Step::Fails,
            (Some(Kind::Memory), Class::Float) => Step::Selected,
            _ => Step::Unknown,
        }
    }

    /// What FastISel makes of reading `op` in `facts`' body.
    fn read(&self, facts: &Facts<'db>, op: &Operand) -> Step {
        match &op.kind {
            OperandKind::Constant { ty, .. } if self.class(ty.as_ref()) == Class::Empty => {
                Step::Selected
            }
            OperandKind::Allocation { allocation }
                if self.class(allocation.as_ref().ty) == Class::Empty =>
            {
                Step::Selected
            }
            OperandKind::Copy(place) | OperandKind::Move(place)
                if place.projection.as_slice().is_empty() =>
            {
                let local = place.local;
                let class = self.class(facts.body.locals[local].ty.as_ref());
                // A parameter may be passed by reference, and read from there.
                let param = facts.body.param_locals.contains(&local);
                match (facts.kind(local), class) {
                    (_, Class::Empty) => Step::Selected,
                    (Some(Kind::Value), _) if !param || class != Class::Half => Step::Selected,
                    (Some(Kind::Memory), Class::Float) => Step::Selected,
                    _ => Step::Unknown,
                }
            }
            _ => Step::Unknown,
        }
    }

    /// What FastISel makes of `statement`, of a body of `facts` whose return place is in its
    /// caller as `returns` says ([`Frame::returns`]).
    fn statement_step(
        &self,
        facts: &Facts<'db>,
        returns: Option<(Option<Kind>, BasicBlockId)>,
        outer: Option<Kind>,
        statement: &hir_ty_mir::mir::Statement,
    ) -> Step {
        match &statement.kind {
            StatementKind::StorageLive(_)
            | StatementKind::StorageDead(_)
            | StatementKind::FakeRead(_)
            | StatementKind::Nop => Step::Selected,
            StatementKind::Assign(place, rvalue) if place.projection.as_slice().is_empty() => {
                let read = match rvalue {
                    Rvalue::Use(op) => self.read(facts, op),
                    Rvalue::UnaryOp(UnOp::Neg, op) => {
                        let ty = facts.body.locals[place.local].ty.as_ref();
                        let scalar = self
                            .layout(ty)
                            .is_ok_and(|it| matches!(it.backend_repr, BackendRepr::Scalar(_)));
                        // A scalar `fneg` of `f32` or `f64` is an integer `xor`.
                        if scalar && self.class(ty) == Class::Float {
                            self.read(facts, op)
                        } else {
                            Step::Unknown
                        }
                    }
                    Rvalue::Aggregate(_, ops) if ops.is_empty() => Step::Selected,
                    _ => Step::Unknown,
                };
                let kind = match (place.local == return_slot(), returns) {
                    (true, Some((kind, _))) => kind,
                    (true, None) => outer,
                    (false, _) => facts.kind(place.local),
                };
                let store = self.store(kind, facts.body.locals[place.local].ty.as_ref());
                match (read, store) {
                    (_, Step::Fails) => Step::Fails,
                    (Step::Selected, Step::Selected) => Step::Selected,
                    _ => Step::Unknown,
                }
            }
            _ => Step::Unknown,
        }
    }

    /// What FastISel makes of the function's `ret`.
    fn ret_step(&self, facts: &Facts<'db>, outer: Option<Kind>) -> Step {
        let ty = facts.body.locals[return_slot()].ty.as_ref();
        let class = self.class(ty);
        match &self.ret_mode {
            PassMode::Ignore | PassMode::Indirect => Step::End,
            PassMode::Direct(_) | PassMode::Cast(_) => {
                let Ok(layout) = self.layout(self.normalize(ty)) else { return Step::Unclear };
                let vector = matches!(layout.backend_repr, BackendRepr::SimdVector { .. })
                    && layout.size.bytes() == 16;
                let float = matches!(layout.backend_repr, BackendRepr::Scalar(_));
                match (outer, class) {
                    (Some(Kind::Value), Class::Float | Class::Half) if vector => Step::End,
                    (Some(Kind::Value), Class::Float) if float => Step::End,
                    (Some(Kind::Memory), Class::Float) if vector || float => Step::End,
                    _ => Step::Unclear,
                }
            }
            _ => Step::Unclear,
        }
    }

    fn class(&self, ty: Ty<'db>) -> Class {
        let Ok(layout) = self.layout(self.normalize(ty)) else { return Class::Other };
        if layout.size.bytes() == 0 {
            return Class::Empty;
        }
        let element = match layout.backend_repr {
            BackendRepr::Scalar(scalar) => scalar,
            BackendRepr::SimdVector { element, .. } => element,
            _ => return Class::Other,
        };
        match element.primitive() {
            Primitive::Float(rustc_abi::Float::F16) => Class::Half,
            Primitive::Float(rustc_abi::Float::F32 | rustc_abi::Float::F64) => Class::Float,
            _ => Class::Other,
        }
    }

    /// What SelectionDAG has as operand `op` of the fma at `site`, which is in it with the code
    /// before it in its block; why not known if that is not.
    fn trace(
        &mut self,
        facts: &Facts<'db>,
        op: &Operand,
        block: BasicBlockId,
    ) -> std::result::Result<Traced, String> {
        let head = facts.head.get(&block).copied();
        // alloc: the error's message
        let mut local = plain_local(op).ok_or_else(|| format!("is {:?}", op.kind))?;
        let mut negated = false;
        for _ in 0..LIMIT {
            let plain = |local: LocalId, lane_of| {
                let root = Some(facts.root(local));
                if negated {
                    Traced::Negated { root, lane_of }
                } else {
                    Traced::Plain { root, lane_of }
                }
            };
            if facts.body.param_locals.contains(&local) && facts.single_def(local).is_some() {
                return Ok(plain(local, None));
            }
            match facts.kind(local) {
                Some(Kind::Value) => {}
                Some(Kind::Memory) => {
                    let negations = facts.defs[&local].iter().any(|def| match *def {
                        Def::Statement(b, at) => {
                            matches!(facts.rvalue(b, at), Some(Rvalue::UnaryOp(UnOp::Neg, _)))
                        }
                        Def::Call(b) => facts.call(b).is_some_and(|(_, _, intrinsic)| {
                            intrinsic == Some(Intrinsic::Simd(SimdOp::Neg))
                        }),
                        Def::Param => false,
                    });
                    if negations {
                        // alloc: the error's message
                        return Err("is a negation kept in memory".to_owned());
                    }
                    return Ok(plain(local, None));
                }
                // alloc: the error's message
                None => return Err("is a local used by its parts or borrowed".to_owned()),
            }
            let def = facts.single_def(local).expect("a local of one definition");
            let at = match def {
                Def::Statement(b, _) | Def::Call(b) => b,
                Def::Param => return Ok(plain(local, None)),
            };
            if facts.head.get(&at).copied() != head {
                return Ok(plain(local, None));
            }
            let negate = |negated: &mut bool| {
                if *negated {
                    // alloc: the error's message
                    return Err("is a negation of a negation".to_owned());
                }
                *negated = true;
                Ok(())
            };
            match def {
                Def::Statement(b, i) => match facts.rvalue(b, i) {
                    Some(Rvalue::Use(op)) => {
                        // alloc: the error's message
                        local = plain_local(op).ok_or_else(|| format!("is {:?}", op.kind))?;
                    }
                    Some(Rvalue::UnaryOp(UnOp::Neg, op)) => {
                        negate(&mut negated)?;
                        // alloc: the error's message
                        local = plain_local(op).ok_or_else(|| format!("is {:?}", op.kind))?;
                    }
                    // alloc: the error's message
                    rvalue => return Err(format!("is {rvalue:?}")),
                },
                Def::Call(b) => {
                    let Some((func, args, intrinsic)) = facts.call(b) else {
                        return Err("is a call".into());
                    };
                    let Some((callee, monomorphic)) = callee_of(func) else {
                        // alloc: the error's message
                        return Err("is the result of a call of a pointer".to_owned());
                    };
                    let Some(intrinsic) = intrinsic else {
                        let db = self.cg.db;
                        let name = FunctionSignature::of(db, callee).name.as_str();
                        let inlined = if monomorphic {
                            // alloc: the error's message
                            self.rustc_inlines(facts.func, callee).map_err(|e| format!("{e:?}"))?
                        } else {
                            None
                        };
                        return match inlined {
                            Some(false) => Ok(plain(local, None)),
                            // alloc: the error's message
                            _ => Err(format!("is the result of `{name}`, which rustc may inline")),
                        };
                    };
                    match (intrinsic, args) {
                        (Intrinsic::Simd(SimdOp::Neg), [op]) => {
                            negate(&mut negated)?;
                            // alloc: the error's message
                            local = plain_local(op).ok_or_else(|| format!("is {:?}", op.kind))?;
                        }
                        (Intrinsic::Simd(SimdOp::Extract), [vector, index]) => {
                            let index =
                                if facts.func == self.body.and_then(|it| it.owner.as_function()) {
                                    // alloc: the error's message
                                    self.immediate(index).map_err(|e| format!("{e:?}"))?
                                } else {
                                    None
                                };
                            let vector = plain_local(vector)
                                // alloc: the error's message
                                .ok_or_else(|| "is a lane of a vector not in a local".to_owned())?;
                            let lane_of = Some(facts.root(vector));
                            if index != Some(0) {
                                // alloc: the error's message
                                return Err("is a lane of a vector".to_owned());
                            }
                            // `combineFMA` looks through the `extractelement` of lane 0.
                            let mut inner = vector;
                            loop {
                                match facts.single_def(inner) {
                                    Some(Def::Statement(b, i)) => match facts.rvalue(b, i) {
                                        Some(Rvalue::Use(op)) if plain_local(op).is_some() => {
                                            inner = plain_local(op).expect("a local");
                                        }
                                        Some(Rvalue::UnaryOp(..)) => {
                                            // alloc: the error's message
                                            return Err("is a lane of a negated vector".to_owned());
                                        }
                                        _ => break,
                                    },
                                    Some(Def::Call(b)) => {
                                        let neg = facts.call(b).is_some_and(|(_, _, intrinsic)| {
                                            intrinsic == Some(Intrinsic::Simd(SimdOp::Neg))
                                        });
                                        if neg {
                                            // alloc: the error's message
                                            return Err("is a lane of a negated vector".to_owned());
                                        }
                                        break;
                                    }
                                    _ => break,
                                }
                            }
                            return Ok(plain(local, lane_of));
                        }
                        // alloc: the error's message
                        _ => return Err(format!("is the result of `{intrinsic}`")),
                    }
                }
                Def::Param => unreachable!(),
            }
        }
        // alloc: the error's message
        Err("is too far to follow".to_owned())
    }

    /// Whether the instruction LLVM matches takes `y` as the first factor of the fma at
    /// `site`, by what its result goes to in its block; why not known if that is not.
    fn fma_consumers(
        &mut self,
        facts: &Facts<'db>,
        site: &Site<'_>,
        traced: &[Traced; 3],
        scalar: bool,
    ) -> std::result::Result<bool, String> {
        let block = self.block.expect("the fma's block");
        let head = facts.head.get(&block).copied();
        let root = |t: &Traced| match *t {
            Traced::Plain { root, .. } | Traced::Negated { root, .. } => root,
        };
        if !site.destination.projection.as_slice().is_empty() {
            // alloc: the error's message
            return Err("goes into part of a local".to_owned());
        }
        // The locals the result is copied to, and what reads them in the block.
        let mut held: SmallVec<[LocalId; 8]> = SmallVec::from_slice(&[site.destination.local]);
        let mut uses: SmallVec<[_; 4]> = SmallVec::new();
        for (id, data) in facts.body.basic_blocks.iter() {
            if facts.head.get(&id).copied() != head {
                continue;
            }
            for statement in &data.statements {
                let StatementKind::Assign(place, rvalue) = &statement.kind else { continue };
                let reads = |op: &Operand, held: &[LocalId]| {
                    plain_local(op).is_some_and(|it| held.contains(&it))
                };
                match rvalue {
                    Rvalue::Use(op) if reads(op, &held[..]) => {
                        if place.projection.as_slice().is_empty() {
                            held.push(place.local);
                        } else {
                            // alloc: the error's message
                            return Err("is copied into part of a local".to_owned());
                        }
                    }
                    rvalue => {
                        let mut any = false;
                        let mut check = |op: &Operand| any |= reads(op, &held[..]);
                        match rvalue {
                            Rvalue::Use(op)
                            | Rvalue::Repeat(op, _)
                            | Rvalue::Cast(_, op, _)
                            | Rvalue::UnaryOp(_, op) => check(op),
                            Rvalue::BinaryOp(_, a, b) | Rvalue::CheckedBinaryOp(_, a, b) => {
                                check(a);
                                check(b);
                            }
                            Rvalue::Aggregate(_, ops) => ops.iter().for_each(check),
                            _ => {}
                        }
                        if any {
                            // alloc: the error's message
                            return Err(format!("is read by {rvalue:?}"));
                        }
                    }
                }
            }
            if let Some(TerminatorKind::Call { func, args, intrinsic, .. }) =
                data.terminator.as_ref().map(|it| &it.kind)
            {
                let positions: SmallVec<[usize; 4]> = SmallVec::from_iter(
                    (0..args.len())
                        .filter(|&i| plain_local(&args[i]).is_some_and(|it| held.contains(&it))),
                );
                if !positions.is_empty() {
                    uses.push((func, args, *intrinsic, positions));
                }
            }
        }
        let mut swapped = false;
        for (func, args, intrinsic, positions) in uses {
            let Some((callee, _)) = callee_of(func) else {
                return Err("is passed to a pointer".into());
            };
            let Some(intrinsic) = intrinsic else {
                let db = self.cg.db;
                // alloc: the error's message
                if self.rustc_inlines(facts.func, callee).map_err(|e| format!("{e:?}"))?
                    != Some(false)
                {
                    let name = FunctionSignature::of(db, callee).name.as_str();
                    // alloc: the error's message
                    return Err(format!("is passed to `{name}`, which rustc may inline"));
                }
                continue;
            };
            match (intrinsic, positions.as_slice()) {
                (Intrinsic::Simd(SimdOp::SelectBitmask | SimdOp::Select), [1]) => {
                    // A passthru of `y` would be matched by the commuted masked form.
                    let passthru = plain_local(&args[2]).map(|it| facts.root(it));
                    if passthru.is_some()
                        && passthru == root(&traced[1])
                        && passthru != root(&traced[0])
                    {
                        // alloc: the error's message
                        return Err("is merged into its second factor".to_owned());
                    }
                }
                (Intrinsic::Simd(SimdOp::Shuffle), _) if !scalar => {}
                (Intrinsic::Simd(SimdOp::Insert), [2]) if scalar => {
                    // alloc: the error's message
                    let index = self.immediate(&args[1]).map_err(|e| format!("{e:?}"))?;
                    let vector = plain_local(&args[0]).map(|it| facts.root(it));
                    let lane = |t: &Traced| match *t {
                        Traced::Plain { lane_of, .. } | Traced::Negated { lane_of, .. } => lane_of,
                    };
                    let of = [0, 1, 2].map(|i| vector.is_some() && lane(&traced[i]) == vector);
                    if index == Some(0) && of.contains(&true) {
                        // `213_Int` and `231_Int` take the vector as the first operand.
                        match of {
                            [true, false, false] => swapped = true,
                            [false, true, false] | [false, false, true] => {}
                            _ => {
                                return Err("goes into a vector more than one operand is of".into());
                            }
                        }
                    }
                }
                // alloc: the error's message
                _ => return Err(format!("is read by `{intrinsic}`")),
            }
        }
        Ok(swapped)
    }

    /// Why an fma of `func`, which rustc's MIR inliner puts into this function where it is
    /// called with `args`, may not be what LLVM makes of it in `func` alone. It is that where
    /// what follows it in `func` decides whether it folds negations, and where it does, its
    /// operands are `func`'s own or ones this function passes as they are, and its result goes
    /// only to shuffles in `func`: what this function has around the call is then not in it.
    fn decided_inside(
        &mut self,
        func: FunctionId,
        args: &[Operand],
    ) -> std::result::Result<(), String> {
        let db = self.cg.db;
        let Ok((body, _)) = root_body(db, func) else { return Err("its body is not known".into()) };
        let facts = Facts::new(body);
        let here = self.block.expect("the call's block");
        let caller = Facts::new(self.body.expect("MIR body"));
        for (id, data) in body.basic_blocks.iter() {
            let Some(TerminatorKind::Call {
                func: callee,
                args: operands,
                destination,
                target,
                intrinsic,
                ..
            }) = data.terminator.as_ref().map(|it| &it.kind)
            else {
                continue;
            };
            let Some((callee, _)) = callee_of(callee) else {
                return Err("it calls a pointer".into());
            };
            let Some(intrinsic) = *intrinsic else {
                let inlined =
                    // alloc: the error's message
                    self.rustc_inlines(Some(func), callee).map_err(|e| format!("{e:?}"))?;
                // alloc: the error's message
                if inlined != Some(false) && self.has_fma(callee).map_err(|e| format!("{e:?}"))? {
                    let name = FunctionSignature::of(db, callee).name.as_str();
                    // alloc: the error's message
                    return Err(format!("its call of `{name}` may have one"));
                }
                continue;
            };
            match fused(intrinsic) {
                Some(true) => {}
                // alloc: the error's message
                Some(false) => return Err(format!("it has a `{intrinsic}`")),
                None => continue,
            }
            let [x, y, z] = &operands[..] else {
                // alloc: the error's message
                return Err(format!("its `{intrinsic}` is odd"));
            };
            let site =
                Site { operands: [x, y, z], negated: [false; 3], destination, target: *target };
            if !self.folds(&facts, &site, id, true)? {
                continue;
            }
            for op in [x, y, z] {
                let (Traced::Plain { root, lane_of } | Traced::Negated { root, lane_of }) =
                    self.trace(&facts, op, id)?;
                if lane_of.is_some() {
                    return Err("an operand is a lane of a vector".into());
                }
                let Some(at) =
                    root.and_then(|root| body.param_locals.iter().position(|&it| it == root))
                else {
                    continue;
                };
                let arg = args.get(at).ok_or("it is passed fewer arguments")?;
                match self.trace(&caller, arg, here)? {
                    Traced::Plain { lane_of: None, .. } => {}
                    _ => return Err("an operand is passed a negation or a lane".into()),
                }
            }
            if !destination.projection.as_slice().is_empty()
                || facts.tangled.contains(&destination.local)
            {
                return Err("its result goes into part of a local".into());
            }
            let mut held: SmallVec<[LocalId; 8]> = SmallVec::from_slice(&[destination.local]);
            let mut grew = true;
            while grew {
                grew = false;
                for (_, data) in body.basic_blocks.iter() {
                    for statement in &data.statements {
                        let StatementKind::Assign(place, Rvalue::Use(op)) = &statement.kind else {
                            continue;
                        };
                        if plain_local(op).is_some_and(|it| held.contains(&it))
                            && !held.contains(&place.local)
                        {
                            if !place.projection.as_slice().is_empty() {
                                return Err("its result is copied into part of a local".into());
                            }
                            held.push(place.local);
                            grew = true;
                        }
                    }
                }
            }
            if held.contains(&return_slot()) || held.iter().any(|it| facts.tangled.contains(it)) {
                return Err("its result is returned or borrowed".into());
            }
            for (_, data) in body.basic_blocks.iter() {
                for statement in &data.statements {
                    let StatementKind::Assign(_, rvalue) = &statement.kind else { continue };
                    let reads = |op: &Operand| plain_local(op).is_some_and(|it| held.contains(&it));
                    let read = match rvalue {
                        Rvalue::Use(_) => false,
                        Rvalue::Repeat(op, _) | Rvalue::Cast(_, op, _) | Rvalue::UnaryOp(_, op) => {
                            reads(op)
                        }
                        Rvalue::BinaryOp(_, a, b) | Rvalue::CheckedBinaryOp(_, a, b) => {
                            reads(a) || reads(b)
                        }
                        Rvalue::Aggregate(_, ops) => ops.iter().any(reads),
                        _ => false,
                    };
                    if read {
                        // alloc: the error's message
                        return Err(format!("its result is read by {rvalue:?}"));
                    }
                }
                let Some(TerminatorKind::Call { args: inputs, intrinsic: used_by, .. }) =
                    data.terminator.as_ref().map(|it| &it.kind)
                else {
                    continue;
                };
                if !inputs.iter().any(|it| plain_local(it).is_some_and(|it| held.contains(&it))) {
                    continue;
                }
                let shuffle = *used_by == Some(Intrinsic::Simd(SimdOp::Shuffle));
                if !shuffle || intrinsic != Intrinsic::Simd(SimdOp::Fma) {
                    return Err("its result goes to more than a shuffle".into());
                }
            }
        }
        Ok(())
    }

    /// Whether rustc's MIR inliner put `callee` into `caller` in the build of `caller`'s crate:
    /// `None` where that is not known.
    fn rustc_inlines(
        &mut self,
        caller: Option<FunctionId>,
        callee: FunctionId,
    ) -> Result<Option<bool>> {
        rustc_inlines_at(self.cg.db, caller, callee, 0)
    }

    /// Whether `func`'s body has an fma, or a call rustc may inline that has one.
    fn has_fma(&mut self, func: FunctionId) -> Result<bool> {
        Ok(reaches_fma(self.cg.db, func))
    }

    /// `func`'s body as a wrapper of one `simd_fma` of its parameters, some negated.
    fn wrapper(&mut self, func: FunctionId) -> Result<Option<Wrapper>> {
        let db = self.cg.db;
        let Ok((body, _)) = root_body(db, func) else { return Ok(None) };
        let mut of: HashMap<LocalId, (usize, bool)> = HashMap::default();
        for (i, &param) in body.param_locals.iter().enumerate() {
            of.insert(param, (i, false));
        }
        let mut result: SmallVec<[LocalId; 4]> = SmallVec::new();
        let mut found = None;
        let mut block = body.start_block;
        let mut returns = false;
        let local_of = |op: &Operand| plain_local(op);
        for _ in 0..body.basic_blocks.len() + 1 {
            let data = &body.basic_blocks[block];
            for statement in &data.statements {
                match &statement.kind {
                    StatementKind::StorageLive(_)
                    | StatementKind::StorageDead(_)
                    | StatementKind::FakeRead(_)
                    | StatementKind::Nop => {}
                    StatementKind::Assign(place, rvalue)
                        if place.projection.as_slice().is_empty() =>
                    {
                        match rvalue {
                            Rvalue::Use(op) => match local_of(op) {
                                Some(from) if result.contains(&from) => result.push(place.local),
                                Some(from) if of.contains_key(&from) => {
                                    let it = of[&from];
                                    of.insert(place.local, it);
                                }
                                _ => return Ok(None),
                            },
                            Rvalue::UnaryOp(UnOp::Neg, op) => match local_of(op)
                                .and_then(|it| of.get(&it))
                            {
                                Some(&(param, false)) => _ = of.insert(place.local, (param, true)),
                                _ => return Ok(None),
                            },
                            _ => return Ok(None),
                        }
                    }
                    _ => return Ok(None),
                }
            }
            match data.terminator.as_ref().map(|it| &it.kind) {
                Some(TerminatorKind::Return) => {
                    returns = true;
                    break;
                }
                Some(TerminatorKind::Goto { target }) => block = *target,
                Some(TerminatorKind::Call {
                    args,
                    destination,
                    target: Some(target),
                    intrinsic,
                    ..
                }) => {
                    let Some(intrinsic) = *intrinsic else { return Ok(None) };
                    if !destination.projection.as_slice().is_empty() {
                        return Ok(None);
                    }
                    match (intrinsic, &args[..]) {
                        (Intrinsic::Simd(SimdOp::Neg), [op]) => match local_of(op)
                            .and_then(|it| of.get(&it))
                        {
                            Some(&(param, false)) => {
                                _ = of.insert(destination.local, (param, true))
                            }
                            _ => return Ok(None),
                        },
                        (Intrinsic::Simd(SimdOp::Fma), [x, y, z]) if found.is_none() => {
                            let mut params = [0; 3];
                            let mut negated = [false; 3];
                            for (i, op) in [x, y, z].into_iter().enumerate() {
                                let Some(&(param, neg)) = local_of(op).and_then(|it| of.get(&it))
                                else {
                                    return Ok(None);
                                };
                                (params[i], negated[i]) = (param, neg);
                            }
                            found = Some(Wrapper { params, negated });
                            result.push(destination.local);
                        }
                        _ => return Ok(None),
                    }
                    block = *target;
                }
                _ => return Ok(None),
            }
        }
        Ok(found.filter(|_| returns && result.contains(&return_slot())))
    }
}

fn rustc_inlines_at(
    db: &Db,
    caller: Option<FunctionId>,
    callee: FunctionId,
    depth: u32,
) -> Result<Option<bool>> {
    let Some(caller) = caller else { return Ok(Some(false)) };
    if !caller.krate(db).data(db).origin.is_lang() {
        return Ok(Some(false));
    }
    let flags = AttrFlags::query(db, callee.into());
    if flags.contains(AttrFlags::INLINE_NEVER) {
        return Ok(Some(false));
    }
    let (mine, theirs) =
        (AttrFlags::target_feature_list(db, caller), AttrFlags::target_feature_list(db, callee));
    if mine != theirs {
        let (enabled, enabled_there) = (
            hir_ty::TargetFeatures::from_fn(db, caller),
            hir_ty::TargetFeatures::from_fn(db, callee),
        );
        let differ = mine.iter().any(|it| !enabled_there.enables_name(it.as_str()))
            || theirs.iter().any(|it| !enabled.enables_name(it.as_str()));
        return Ok(if differ { Some(false) } else { None });
    }
    // Not generic, so only what is `#[inline]` is inlined across crates and within one.
    if !flags.intersects(AttrFlags::IS_INLINE | AttrFlags::RUSTC_FORCE_INLINE) {
        return Ok(None);
    }
    let Ok((body, _)) = root_body(db, callee) else { return Ok(None) };
    let Ok((caller_body, _)) = root_body(db, caller) else { return Ok(None) };
    // A caller that is `#[inline]` and only forwards has the lowest threshold, 30: one of
    // two calls is not one.
    let calls = |body: &MirBody<'_>| {
        body.basic_blocks
            .iter()
            .filter(|(_, it)| {
                matches!(it.terminator.as_ref().map(|t| &t.kind), Some(TerminatorKind::Call { .. }))
            })
            .count()
    };
    let threshold = if AttrFlags::query(db, caller.into()).contains(AttrFlags::IS_INLINE)
        && calls(caller_body) < 2
    {
        30
    } else {
        100
    };
    let cost = inline_cost(db, callee, body, depth)?;
    Ok(cost.is_some_and(|it| it <= threshold).then_some(true))
}

/// At least what rustc's `CostChecker` counts of `func`'s optimized MIR, from `body`, its
/// MIR here; `None` where that is not known. The optimizations take statements out and
/// put none in but for what the inliner puts in, counted here as the body of each callee
/// it may inline with what passes its arguments, and for SROA's splitting of an aggregate
/// whose fields are used, which a body without projections has none of. A callee that is
/// `#[rustc_nounwind]` has its unwinding made unreachable (`AbortUnwindingCalls`), and the
/// cleanup only that reaches is gone.
fn inline_cost<'db>(
    db: &'db Db,
    func: FunctionId,
    body: &MirBody<'db>,
    depth: u32,
) -> Result<Option<usize>> {
    if depth > 4 {
        return Ok(None);
    }
    let operand_projected = |op: &Operand| match &op.kind {
        OperandKind::Copy(place) | OperandKind::Move(place) => {
            !place.projection.as_slice().is_empty()
        }
        _ => false,
    };
    let mut reached = HashSet::default();
    let mut work: SmallVec<[BasicBlockId; 16]> = SmallVec::from_slice(&[body.start_block]);
    while let Some(id) = work.pop() {
        if !reached.insert(id) {
            continue;
        }
        let Some(terminator) = &body.basic_blocks[id].terminator else { return Ok(None) };
        work.extend(terminator.kind.successors());
        if let TerminatorKind::Call { func: callee, .. } = &terminator.kind
            && callee_of(callee).is_some_and(|(f, _)| AttrFlags::is_rustc_nounwind(db, f))
        {
            continue;
        }
        work.extend(terminator.kind.unwind());
    }
    let mut cost = 0;
    for (id, data) in body.basic_blocks.iter() {
        if !reached.contains(&id) {
            continue;
        }
        for statement in &data.statements {
            match &statement.kind {
                StatementKind::Assign(place, rvalue) => {
                    let reads_projected = match rvalue {
                        Rvalue::Use(op) | Rvalue::Repeat(op, _) | Rvalue::Cast(_, op, _) => {
                            operand_projected(op)
                        }
                        Rvalue::UnaryOp(_, op) => operand_projected(op),
                        Rvalue::BinaryOp(_, a, b) | Rvalue::CheckedBinaryOp(_, a, b) => {
                            operand_projected(a) || operand_projected(b)
                        }
                        Rvalue::Aggregate(_, ops) => ops.iter().any(operand_projected),
                        _ => true,
                    };
                    if !place.projection.as_slice().is_empty() || reads_projected {
                        return Ok(None);
                    }
                    cost += 5;
                }
                StatementKind::StorageLive(_)
                | StatementKind::StorageDead(_)
                | StatementKind::FakeRead(_)
                | StatementKind::Nop => {}
                _ => return Ok(None),
            }
        }
        let kind = &data.terminator.as_ref().expect("a reached block's end").kind;
        cost += match kind {
            TerminatorKind::Call { func: callee, args, cleanup, intrinsic, .. } => {
                if args.iter().any(operand_projected) {
                    return Ok(None);
                }
                let Some((callee, _)) = callee_of(callee) else { return Ok(None) };
                let unwinds = cleanup.is_some() && !AttrFlags::is_rustc_nounwind(db, callee);
                let pad = if unwinds { 50 } else { 0 };
                if intrinsic.is_some() {
                    5 + pad
                } else if rustc_inlines_at(db, Some(func), callee, depth + 1)? == Some(false) {
                    25 + pad
                } else {
                    let Ok((inner, _)) = root_body(db, callee) else { return Ok(None) };
                    let Some(inner) = inline_cost(db, callee, inner, depth + 1)? else {
                        return Ok(None);
                    };
                    (25 + pad).max(inner + 5 * (args.len() + 1))
                }
            }
            TerminatorKind::Goto { .. } | TerminatorKind::Return | TerminatorKind::Unreachable => 0,
            TerminatorKind::SwitchInt { .. } => 5,
            TerminatorKind::Assert { .. } | TerminatorKind::Drop { .. } => 75,
            TerminatorKind::UnwindResume => 45,
            _ => return Ok(None),
        };
    }
    Ok(Some(cost))
}

/// Whether `func`'s body has an fma, or a call rustc may inline that has one, at any depth: a
/// call of no fma that rustc inlines however deep puts none into its caller. A cycle of calls
/// starts as having none and is iterated to its fixpoint.
#[blaze_query::tracked(returns(copy), cycle_initial = |_, _, _| false)]
fn reaches_fma(db: &Db, func: FunctionId) -> bool {
    let Ok((body, _)) = root_body(db, func) else { return false };
    for (_, data) in body.basic_blocks.iter() {
        let Some(TerminatorKind::Call { func: callee, intrinsic, .. }) =
            data.terminator.as_ref().map(|it| &it.kind)
        else {
            continue;
        };
        let Some((callee, _)) = callee_of(callee) else { continue };
        if let Some(intrinsic) = *intrinsic {
            if fused(intrinsic).is_some() {
                return true;
            }
        } else if !rustc_inlines_at(db, Some(func), callee, 0).is_ok_and(|it| it == Some(false))
            && reaches_fma(db, callee)
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use hir_ty::intrinsic::AtomicOrders;

    use super::*;

    #[test]
    fn fma_intrinsics_go_as_their_names_did() {
        let of = |name: &str| Intrinsic::from_symbol(&intern::Symbol::intern(name));
        // `fma`/`fmaf32`/`fmaf64` and `simd_fma` fused and were selected after an fma;
        // `fmuladd*` and `simd_relaxed_fma` were refused, and unknown after an fma.
        for (name, intrinsic, fuses, step) in [
            ("fma", Intrinsic::Fma, Some(true), Step::Selected),
            ("simd_fma", Intrinsic::Simd(SimdOp::Fma), Some(true), Step::Selected),
            ("fmuladd", Intrinsic::Fmuladd, Some(false), Step::Unknown),
            ("simd_relaxed_fma", Intrinsic::Simd(SimdOp::RelaxedFma), Some(false), Step::Unknown),
        ] {
            assert_eq!(of(name), Some(intrinsic), "{name}");
            assert_eq!(intrinsic.to_string(), name);
            assert_eq!(fused(intrinsic), fuses, "{name}");
            assert_eq!(intrinsic_step(intrinsic), Some(step), "{name}");
        }
        for gone in ["fmaf32", "fmaf64", "fmuladdf32", "fmuladdf64"] {
            assert_eq!(of(gone), None, "{gone}");
        }
    }

    #[test]
    fn what_follows_an_fma() {
        for op in [SimdOp::Insert, SimdOp::Extract, SimdOp::Shuffle, SimdOp::SelectBitmask] {
            assert_eq!(intrinsic_step(Intrinsic::Simd(op)), Some(Step::Fails), "{op:?}");
        }
        assert_eq!(intrinsic_step(Intrinsic::Simd(SimdOp::Select)), Some(Step::Fails));
        assert_eq!(intrinsic_step(Intrinsic::Simd(SimdOp::Neg)), None);
        for other in [
            Intrinsic::Simd(SimdOp::Add),
            Intrinsic::Simd(SimdOp::InsertDyn),
            Intrinsic::Simd(SimdOp::ExtractDyn),
            Intrinsic::Sqrt,
            Intrinsic::Transmute,
            Intrinsic::Atomic(AtomicOp::Load, AtomicOrders::Generic),
        ] {
            assert_eq!(intrinsic_step(other), Some(Step::Unknown), "{other}");
            assert_eq!(fused(other), None, "{other}");
        }
    }
}
