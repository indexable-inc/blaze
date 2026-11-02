//! rustc's MIR optimizations, run on a [`MirPhase::Runtime`] body as rustc's
//! `run_optimization_passes` runs them on its runtime MIR (`rustc_mir_transform` at the pinned
//! nightly, `compiler/rustc_mir_transform/src/lib.rs`). The IDE and the interpreter never ask
//! for them. The code generator asks for each crate at the level rustc gives the crate's
//! `opt-level`; `BLAZE_MIR_OPT` only overrides that level.
//!
//! What they run with is a [`MirOptions`], which keys the queries:
//! - `level` is rustc's `-Zmir-opt-level`: [`MirOptLevel::Debug`] is what rustc runs at
//!   `opt-level=0` (its level 1) and [`MirOptLevel::Release`] what it runs otherwise (its
//!   level 2).
//! - `ub_checks` is rustc's `sess.ub_checks()` of the crate the body is of. With it,
//!   `CheckAlignment`, `CheckNull` and `CheckEnums` add their checks at every level,
//!   [`MirOptLevel::Off`] and coroutine bodies included, as rustc's do: they are part of what
//!   UB checks mean, not optimizations.
//! - `lifetime_markers` says the code generator gives its backend storage markers as
//!   lifetimes; `RemoveStorageMarkers` runs only without them, as rustc's runs only where it
//!   emits none (`!emit_lifetime_markers()`).
//!
//! Each pass is a port of rustc's pass of the same name; where it does less than rustc's, its
//! module says what. Passes rustc runs at a level and these do not are listed in
//! [`passes_not_ported`].
//!
//! The passes run on the generic body, as rustc's do, once per function: monomorphization
//! substitutes after. An edit to one function optimizes that one again.
//!
//! [`MirPhase::Runtime`]: super::MirPhase::Runtime

mod cfg;
mod check_enums;
mod check_pointers;
mod copy_prop;
mod dse;
mod inline;
mod inst_simplify;
mod locals;

use base_db::Db;
use blaze_mem::{Frame, Tag, frame};
use la_arena::{Arena, ArenaMap, Idx, RawIdx};

use std::cell::Cell;

use crate::{db::HirDatabase, traits::StoredParamEnvAndCrate};

use crate::next_solver::StoredGenericArgs;

use super::{
    BasicBlockId, InlinedBody, LocalId, MirBody, MirLowerError, MirOptLevel, MirOptions, MirOwner,
    MirStage, OperandKind, ProjectionElem, Rvalue, StatementKind, StoredPlace, TerminatorKind,
    budget::Pass,
    framed::{Bits, bits_in, block_id, block_index, local_index},
    transform::Ctx,
};

pub use inline::{CallerLocation, caller_location, owner_of, source_span, written_checked_at};

type Result<'db, T> = std::result::Result<T, MirLowerError<'db>>;

/// The passes of rustc's pipeline at `level` that this module does not run, in rustc's order.
/// `harness/mirparity.py` prints them next to what it measured.
pub fn passes_not_ported(level: MirOptLevel) -> &'static [&'static str] {
    match level {
        MirOptLevel::Off => &[],
        MirOptLevel::Debug => &[
            "ForceInline",
            "RemoveZsts",
            "UnreachableEnumBranching",
        ],
        MirOptLevel::Release => &[
            "ForceInline",
            "RemoveZsts",
            "UnreachableEnumBranching",
            "ReferencePropagation",
            "ScalarReplacementOfAggregates",
            "GVN",
            "SsaRangePropagation",
            "MatchBranchSimplification",
            "JumpThreading",
            "EarlyOtherwiseBranch",
            "SimplifyComparisonIntegral",
            "DestinationPropagation",
        ],
    }
}

/// `body` after rustc's optimizations with `options`. Of a coroutine body only the UB checks
/// run: what a future keeps across a yield is computed from the runtime body, and the checks'
/// temporaries live within one block.
///
/// A pass run only at [`MirOptLevel::Release`] never adds a call, constant, static, drop or
/// vtable the body does not have at [`MirOptLevel::Debug`]: those passes only remove code or
/// replace a local by another holding the same value. The one exception is `Inline`
/// (`options.inline`, only at `Release`): an inlined callee brings what its own body
/// references, which the `Debug` body reaches through the call, and lists the callee in
/// `body.inlined.callees`. The code generator's walk of what a unit references reads `Debug`
/// bodies for every level and relies on this; `tests/mir/references.rs` holds it.
pub(super) fn optimize<'db>(
    db: &'db Db,
    body: &MirBody<'db>,
    options: MirOptions,
    caller: Option<InlinedBody<'db>>,
) -> Result<'db, MirBody<'db>> {
    // clone: the copy the query keeps, which the passes change in place
    let mut body = body.clone();
    let optimizing = options.level != MirOptLevel::Off && body.coroutine.is_none();
    if !optimizing && !options.ub_checks {
        return Ok(body);
    }
    frame::task(Tag::Mir, |outer| {
        passes(&Ctx::new(db, body.owner, outer), &mut body, options, caller, optimizing)
    })?;
    Ok(body)
}

fn passes<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &mut MirBody<'db>,
    options: MirOptions,
    caller: Option<InlinedBody<'db>>,
    optimizing: bool,
) -> Result<'db, ()> {
    let release = options.level >= MirOptLevel::Release;
    // Each pass in a frame of its own, and in a debug build a check that it left a body the
    // next can read.
    macro_rules! run {
        ($pass:literal, |$frame:ident| $run:expr) => {{
            frame::task(Tag::Mir, |$frame| -> Result<'db, ()> {
                $run;
                #[cfg(debug_assertions)]
                if let Err(error) = validate(body, $frame) {
                    panic!("MIR of {:?} after {}: {error}", body.owner, $pass);
                }
                Ok(())
            })?
        }};
    }
    // rustc's runtime cleanup, which its runtime MIR has had before any of these:
    // `CleanupPostBorrowck` and `SimplifyCfg-pre-optimizations`.
    if optimizing {
        run!("CleanupPostBorrowck", |frame| cfg::cleanup_post_borrowck(body, frame));
        run!("SimplifyCfg", |frame| cfg::simplify_cfg(body, frame));
    }
    if options.ub_checks {
        use check_pointers::{PointerCheck, check_pointers};
        run!("CheckAlignment", |frame| check_pointers(ctx, body, PointerCheck::Alignment, frame)?);
        run!("CheckNull", |frame| check_pointers(ctx, body, PointerCheck::Null, frame)?);
        run!("CheckEnums", |frame| check_enums::check_enums(ctx, body, frame)?);
    }
    if !optimizing {
        return Ok(());
    }

    run!("LowerSliceLenCalls", |frame| inst_simplify::lower_slice_len_calls(ctx, body, frame));
    run!("InstSimplify", |frame| inst_simplify::inst_simplify(ctx, body, frame));
    // `options.inline` is set only at `Release` (rustc's `Inline::is_enabled`).
    if options.inline
        && let Some(caller) = caller
    {
        run!("Inline", |frame| {
            if inline::inline(ctx, body, caller, options) {
                cfg::simplify_cfg(body, frame);
            }
        });
    }
    // rustc keeps storage markers only where it gives the backend lifetimes.
    if !options.lifetime_markers {
        run!("RemoveStorageMarkers", |frame| locals::remove_storage_markers(body, frame));
    }
    run!("RemoveUnneededDrops", |frame| cfg::remove_unneeded_drops(ctx, body, frame));
    if release {
        run!("UnreachablePropagation", |frame| cfg::unreachable_propagation(body, frame));
    }
    run!("SimplifyCfg", |frame| cfg::simplify_cfg(body, frame));
    run!("InstSimplify", |frame| inst_simplify::inst_simplify(ctx, body, frame));
    run!("SimplifyConstCondition", |frame| cfg::simplify_const_condition(body, frame));
    run!("SimplifyLocals", |frame| locals::simplify_locals(body, frame));
    if release {
        run!("DeadStoreElimination", |frame| dse::dead_store_elimination(body, frame));
    }
    run!("SimplifyLocals", |frame| locals::simplify_locals(body, frame));
    run!("SingleUseConsts", |frame| inst_simplify::single_use_consts(body, frame));
    run!("SimplifyConstCondition", |frame| cfg::simplify_const_condition(body, frame));
    run!("SimplifyConstCondition", |frame| cfg::simplify_const_condition(body, frame));
    run!("RemoveNoopLandingPads", |frame| super::transform::remove_noop_landing_pads(body, frame));
    run!("SimplifyCfg", |frame| cfg::simplify_cfg(body, frame));
    run!("CopyProp", |frame| copy_prop::copy_prop(ctx, body, release, frame));
    if release {
        run!("DeadStoreElimination", |frame| dse::dead_store_elimination(body, frame));
    }
    run!("SimplifyLocals", |frame| locals::simplify_locals(body, frame));
    run!("CriticalCallEdges", |frame| cfg::critical_call_edges(body, frame));
    Ok(())
}

/// One of the optimizations alone in `frame`, or one of the runtime passes through
/// [`super::transform::run_one`].
pub(super) fn run_one<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &mut MirBody<'db>,
    pass: Pass,
    frame: &Frame,
) -> Result<'db, ()> {
    use check_pointers::PointerCheck;
    match pass {
        Pass::CleanupPostBorrowck => cfg::cleanup_post_borrowck(body, frame),
        Pass::SimplifyCfg => cfg::simplify_cfg(body, frame),
        Pass::RemoveStorageMarkers => locals::remove_storage_markers(body, frame),
        Pass::RemoveUnneededDrops => cfg::remove_unneeded_drops(ctx, body, frame),
        Pass::UnreachablePropagation => cfg::unreachable_propagation(body, frame),
        Pass::InstSimplify => inst_simplify::inst_simplify(ctx, body, frame),
        Pass::LowerSliceLenCalls => inst_simplify::lower_slice_len_calls(ctx, body, frame),
        Pass::SimplifyConstCondition => cfg::simplify_const_condition(body, frame),
        Pass::SimplifyLocals => locals::simplify_locals(body, frame),
        Pass::SingleUseConsts => inst_simplify::single_use_consts(body, frame),
        Pass::DeadStoreElimination => dse::dead_store_elimination(body, frame),
        Pass::CopyProp => copy_prop::copy_prop(ctx, body, true, frame),
        Pass::CriticalCallEdges => cfg::critical_call_edges(body, frame),
        Pass::CheckAlignment => {
            check_pointers::check_pointers(ctx, body, PointerCheck::Alignment, frame)?
        }
        Pass::CheckNull => check_pointers::check_pointers(ctx, body, PointerCheck::Null, frame)?,
        Pass::CheckEnums => check_enums::check_enums(ctx, body, frame)?,
        other => return super::transform::run_one(ctx, body, other, frame),
    }
    Ok(())
}

thread_local! {
    /// The level the interpreter runs functions and closures at on this thread, which is
    /// [`MirOptLevel::Off`] unless a test of these passes sets it.
    static INTERPRETER_LEVEL: Cell<MirOptLevel> = const { Cell::new(MirOptLevel::Off) };
}

#[doc(hidden)]
pub fn set_interpreter_mir_opt_level(level: MirOptLevel) {
    INTERPRETER_LEVEL.with(|it| it.set(level));
}

fn interpreter_level() -> MirOptLevel {
    INTERPRETER_LEVEL.with(Cell::get)
}

/// The body the interpreter runs of `owner`: the runtime body, or that body optimized at the
/// level [`set_interpreter_mir_opt_level`] set.
pub(super) fn interpreted_body<'db>(
    db: &'db Db,
    owner: MirOwner<'db>,
    subst: StoredGenericArgs,
    env: StoredParamEnvAndCrate,
) -> Result<'db, &'db MirBody<'db>> {
    let stage = match interpreter_level() {
        MirOptLevel::Off => MirStage::Runtime,
        level => MirStage::Optimized(MirOptions::at(level)),
    };
    db.monomorphized_mir(owner, stage, subst, env)
}

/// What every pass leaves: each local and block a body names is one it has, and each block
/// ends in a terminator.
#[cfg(debug_assertions)]
fn validate(body: &MirBody<'_>, frame: &Frame) -> std::result::Result<(), String> {
    let locals = body.locals.len();
    let blocks = body.basic_blocks.len();
    let mut error = None;
    visit_body(body, |local, _, at| {
        if local_index(local) >= locals && error.is_none() {
            // alloc: only when a pass broke the body, which panics
            error = Some(format!(
                "local _{} of {locals} in bb{}",
                local_index(local),
                block_index(at.block)
            ));
        }
    });
    if let Some(error) = error {
        return Err(error);
    }
    let named = body.param_locals.iter().chain(body.binding_locals.values());
    let named = named.chain(body.promoted_locals.iter().map(|it| &it.0));
    if let Some(local) = named.into_iter().find(|it| local_index(**it) >= locals) {
        // alloc: error path
        return Err(format!(
            "parameter, binding or promoted local _{} of {locals}",
            local_index(*local)
        ));
    }
    if block_index(body.start_block) >= blocks {
        // alloc: error path
        return Err("start block".to_owned());
    }
    let mut reachable: Bits<'_> = bits_in(blocks, frame);
    for block in reverse_postorder(body, frame) {
        reachable.insert(block_index(block));
    }
    for (id, block) in body.basic_blocks.iter() {
        let Some(terminator) = &block.terminator else {
            if reachable.contains(block_index(id)) {
                // alloc: error path
                return Err(format!("bb{} has no terminator", block_index(id)));
            }
            continue;
        };
        if let Some(target) =
            terminator.kind.all_successors().find(|it| block_index(*it) >= blocks)
        {
            // alloc: error path
            return Err(format!(
                "bb{} goes to bb{} of {blocks}",
                block_index(id),
                block_index(target)
            ));
        }
    }
    Ok(())
}

/// Every block a terminator can go to, unwinding and coroutine drops included, as rustc's
/// `Terminator::successors_mut`.
fn successors_mut(kind: &mut TerminatorKind, mut f: impl FnMut(&mut BasicBlockId)) {
    match kind {
        TerminatorKind::Goto { target } => f(target),
        TerminatorKind::SwitchInt { targets, .. } => targets.targets.iter_mut().for_each(f),
        TerminatorKind::Drop { target, unwind, .. }
        | TerminatorKind::DropAndReplace { target, unwind, .. }
        | TerminatorKind::FalseUnwind { real_target: target, unwind } => {
            f(target);
            unwind.iter_mut().for_each(f);
        }
        TerminatorKind::Call { target, cleanup, .. } => {
            target.iter_mut().for_each(&mut f);
            cleanup.iter_mut().for_each(f);
        }
        TerminatorKind::Assert { target, cleanup, .. } => {
            f(target);
            cleanup.iter_mut().for_each(f);
        }
        TerminatorKind::Yield { resume, drop, .. } => {
            f(resume);
            drop.iter_mut().for_each(f);
        }
        TerminatorKind::FalseEdge { real_target, imaginary_target } => {
            f(real_target);
            f(imaginary_target);
        }
        TerminatorKind::UnwindResume
        | TerminatorKind::Abort
        | TerminatorKind::Return
        | TerminatorKind::Unreachable
        | TerminatorKind::TailCall { .. }
        | TerminatorKind::CoroutineDrop => (),
    }
}

/// rustc's `fn_can_unwind` where panics unwind: whether a panic may unwind out of `func`
/// rather than abort.
pub(crate) fn fn_can_unwind(db: &Db, func: hir_def::FunctionId) -> bool {
    !hir_def::attrs::AttrFlags::query(db, func.into())
        .contains(hir_def::attrs::AttrFlags::RUSTC_NOUNWIND)
        && abi_can_unwind(hir_def::signatures::FunctionSignature::of(db, func).abi)
}

/// Whether a function of `abi` may unwind: every Rust ABI, and the others' `-unwind` forms.
pub(crate) fn abi_can_unwind(abi: rustc_abi::ExternAbi) -> bool {
    use rustc_abi::ExternAbi;
    match abi {
        ExternAbi::Rust
        | ExternAbi::RustCall
        | ExternAbi::RustCold
        | ExternAbi::RustInvalid
        | ExternAbi::RustPreserveNone
        | ExternAbi::RustTail => true,
        ExternAbi::C { unwind }
        | ExternAbi::System { unwind }
        | ExternAbi::Aapcs { unwind }
        | ExternAbi::Cdecl { unwind }
        | ExternAbi::Stdcall { unwind }
        | ExternAbi::Fastcall { unwind }
        | ExternAbi::Thiscall { unwind }
        | ExternAbi::Vectorcall { unwind }
        | ExternAbi::SysV64 { unwind }
        | ExternAbi::Win64 { unwind } => unwind,
        _ => false,
    }
}

fn local_id(index: usize) -> LocalId {
    Idx::from_raw(RawIdx::from_u32(index as u32))
}

/// The blocks reachable from the start, in reverse postorder, unwinding included.
fn reverse_postorder<'f>(body: &MirBody<'_>, frame: &'f Frame) -> Vec<BasicBlockId, &'f Frame> {
    let count = body.basic_blocks.len();
    let mut visited: Bits<'_> = bits_in(count, frame);
    let mut post = Vec::with_capacity_in(count, frame);
    // Each block and how many of its successors it has gone to; a block is on it at most once.
    let mut stack: Vec<(BasicBlockId, usize), &Frame> = Vec::with_capacity_in(count, frame);
    let start = body.start_block;
    visited.insert(block_index(start));
    stack.push((start, 0));
    while let Some((block, at)) = stack.last_mut() {
        let next = body.basic_blocks[*block]
            .terminator
            .as_ref()
            .and_then(|it| it.kind.all_successors().nth(*at));
        if let Some(target) = next {
            *at += 1;
            if visited.insert(block_index(target)) {
                stack.push((target, 0));
            }
        } else {
            post.push(*block);
            stack.pop();
        }
    }
    post.reverse();
    post
}

/// Immediate dominators of the reachable blocks (Cooper, Harvey and Kennedy).
struct Dominators<'f> {
    /// By block index: the immediate dominator, `None` for a block that is not reachable.
    idom: Vec<Option<usize>, &'f Frame>,
}

impl<'f> Dominators<'f> {
    fn new(body: &MirBody<'_>, frame: &'f Frame) -> Dominators<'f> {
        let rpo = reverse_postorder(body, frame);
        let count = body.basic_blocks.len();
        let mut order = Vec::with_capacity_in(count, frame);
        order.resize(count, usize::MAX);
        for (at, &block) in rpo.iter().enumerate() {
            order[block_index(block)] = at;
        }
        // The predecessors of block `b` are `preds[starts[b]..starts[b + 1]]`, in reverse
        // postorder and then successor order.
        let mut starts = Vec::with_capacity_in(count + 1, frame);
        starts.resize(count + 1, 0usize);
        for &block in &rpo {
            if let Some(terminator) = &body.basic_blocks[block].terminator {
                for target in terminator.kind.all_successors() {
                    starts[block_index(target) + 1] += 1;
                }
            }
        }
        for at in 0..count {
            starts[at + 1] += starts[at];
        }
        let mut next = Vec::with_capacity_in(count, frame);
        next.extend_from_slice(&starts[..count]);
        let mut preds = Vec::with_capacity_in(starts[count], frame);
        preds.resize(starts[count], 0usize);
        for &block in &rpo {
            if let Some(terminator) = &body.basic_blocks[block].terminator {
                for target in terminator.kind.all_successors() {
                    let slot = &mut next[block_index(target)];
                    preds[*slot] = block_index(block);
                    *slot += 1;
                }
            }
        }
        let start = block_index(body.start_block);
        let mut idom = Vec::with_capacity_in(count, frame);
        idom.resize(count, None);
        idom[start] = Some(start);
        let intersect = |idom: &[Option<usize>], mut a: usize, mut b: usize| {
            while a != b {
                while order[a] > order[b] {
                    a = idom[a].expect("processed");
                }
                while order[b] > order[a] {
                    b = idom[b].expect("processed");
                }
            }
            a
        };
        let mut changed = true;
        while changed {
            changed = false;
            for &block in rpo.iter().skip(1) {
                let block = block_index(block);
                let mut new = None;
                for &pred in &preds[starts[block]..starts[block + 1]] {
                    if idom[pred].is_none() {
                        continue;
                    }
                    new = Some(match new {
                        None => pred,
                        Some(it) => intersect(&idom, pred, it),
                    });
                }
                if new.is_some() && idom[block] != new {
                    idom[block] = new;
                    changed = true;
                }
            }
        }
        Dominators { idom }
    }

    /// Whether `a` dominates `b`; a block dominates itself.
    fn dominates(&self, a: BasicBlockId, b: BasicBlockId) -> bool {
        let (a, mut b) = (block_index(a), block_index(b));
        if self.idom[a].is_none() || self.idom[b].is_none() {
            return false;
        }
        loop {
            if a == b {
                return true;
            }
            let up = self.idom[b].expect("reachable");
            // Only the start block is its own dominator.
            if up == b {
                return false;
            }
            b = up;
        }
    }
}

/// How a statement or terminator mentions a local, as rustc's `PlaceContext` says it for the
/// distinctions these passes make.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mention {
    /// The local as a whole is the left-hand side of an assignment.
    Assigned,
    /// A part of the local, not behind a pointer, is the left-hand side of an assignment.
    AssignedPart,
    /// The local as a whole is where a call returns to.
    CallReturned,
    /// Read: copied, moved, inspected, or the pointer a place goes through.
    Read,
    /// Shared borrow of the local or a part of it.
    SharedBorrow,
    /// Anything else that may change it: a `&mut` of it, a drop of it, a part of it a call
    /// returns to, an `asm!` output.
    Mutated,
    /// `StorageLive` or `StorageDead`.
    Storage,
}

/// Where in a body: a block and a statement in it, the terminator at `statements.len()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Location {
    block: BasicBlockId,
    index: usize,
}

fn visit_place(
    place: &StoredPlace,
    outer: Mention,
    at: Location,
    f: &mut impl FnMut(LocalId, Mention, Location),
) {
    let projection = place.projection.as_slice();
    for elem in projection {
        if let ProjectionElem::Index(local) = elem {
            f(*local, Mention::Read, at);
        }
    }
    let base = match projection.first() {
        None => outer,
        Some(ProjectionElem::Deref) => Mention::Read,
        Some(_) => match outer {
            Mention::Assigned => Mention::AssignedPart,
            Mention::CallReturned => Mention::Mutated,
            other => other,
        },
    };
    f(place.local, base, at);
}

fn visit_operand(
    operand: &super::Operand,
    at: Location,
    f: &mut impl FnMut(LocalId, Mention, Location),
) {
    if let OperandKind::Copy(place) | OperandKind::Move(place) = &operand.kind {
        visit_place(place, Mention::Read, at, f);
    }
}

fn visit_rvalue(rvalue: &Rvalue, at: Location, f: &mut impl FnMut(LocalId, Mention, Location)) {
    match rvalue {
        Rvalue::Ref(kind, place) => {
            let mention = match kind {
                super::BorrowKind::Shared | super::BorrowKind::Shallow => Mention::SharedBorrow,
                super::BorrowKind::Mut { .. } => Mention::Mutated,
            };
            visit_place(place, mention, at, f);
        }
        Rvalue::Len(place) | Rvalue::Discriminant(place) | Rvalue::CopyForDeref(place) => {
            visit_place(place, Mention::Read, at, f)
        }
        _ => rvalue.for_each_operand(&mut |it| visit_operand(it, at, f)),
    }
}

/// Calls `f` with every mention of a local in the statement at `at`.
fn visit_statement(
    statement: &StatementKind,
    at: Location,
    f: &mut impl FnMut(LocalId, Mention, Location),
) {
    match statement {
        StatementKind::Assign(place, rvalue) => {
            visit_rvalue(rvalue, at, f);
            visit_place(place, Mention::Assigned, at, f);
        }
        StatementKind::FakeRead(place) => visit_place(place, Mention::Read, at, f),
        StatementKind::Deinit(place) => visit_place(place, Mention::Mutated, at, f),
        StatementKind::StorageLive(local) | StatementKind::StorageDead(local) => {
            f(*local, Mention::Storage, at)
        }
        StatementKind::InlineAsm(asm) => {
            for operand in &asm.operands {
                match operand {
                    super::AsmOperand::Reg { input, output, .. } => {
                        if let Some(input) = input {
                            visit_operand(input, at, f);
                        }
                        if let Some(output) = output {
                            visit_place(output, Mention::Mutated, at, f);
                        }
                    }
                    super::AsmOperand::Const(it) | super::AsmOperand::Sym(it) => {
                        visit_operand(it, at, f)
                    }
                    super::AsmOperand::Label => (),
                }
            }
            if let Some(taken) = &asm.taken {
                visit_place(taken, Mention::Mutated, at, f);
            }
        }
        StatementKind::Nop => (),
    }
}

fn visit_terminator(
    terminator: &TerminatorKind,
    at: Location,
    f: &mut impl FnMut(LocalId, Mention, Location),
) {
    match terminator {
        TerminatorKind::Drop { place, .. } => visit_place(place, Mention::Mutated, at, f),
        TerminatorKind::DropAndReplace { place, value, .. } => {
            visit_operand(value, at, f);
            visit_place(place, Mention::Mutated, at, f);
        }
        TerminatorKind::Call { func, args, destination, .. } => {
            visit_operand(func, at, f);
            for arg in args.iter() {
                visit_operand(arg, at, f);
            }
            visit_place(destination, Mention::CallReturned, at, f);
        }
        TerminatorKind::Yield { value, resume_arg, .. } => {
            visit_operand(value, at, f);
            visit_place(resume_arg, Mention::Mutated, at, f);
        }
        _ => terminator.for_each_operand(&mut |it| visit_operand(it, at, f)),
    }
}

/// Calls `f` with every mention of a local in the body.
fn visit_body(body: &MirBody<'_>, mut f: impl FnMut(LocalId, Mention, Location)) {
    for (block, data) in body.basic_blocks.iter() {
        for (index, statement) in data.statements.iter().enumerate() {
            visit_statement(&statement.kind, Location { block, index }, &mut f);
        }
        if let Some(terminator) = &data.terminator {
            let at = Location { block, index: data.statements.len() };
            visit_terminator(&terminator.kind, at, &mut f);
        }
    }
}

/// `place` with each local of it, the index locals too, replaced by what `map` says.
fn map_place(place: &mut StoredPlace, map: &mut impl FnMut(LocalId) -> LocalId) {
    place.local = map(place.local);
    if place.projection.as_slice().iter().any(|it| matches!(it, ProjectionElem::Index(_))) {
        let elems = place.projection.as_slice().iter().map(|it| match *it {
            ProjectionElem::Index(local) => ProjectionElem::Index(map(local)),
            other => other,
        });
        place.projection = super::Projection::new_from_iter(elems).store();
    }
}

/// Every local of the body replaced by what `map` says, in its statements and terminators and
/// in what the body says of its locals. `None` drops the local from those lists; a place
/// must not name it.
fn renumber_locals(
    body: &mut MirBody<'_>,
    new_locals: Arena<super::Local>,
    map: &[Option<LocalId>],
) {
    let mut lookup = |local: LocalId| map[local_index(local)].expect("a removed local is used");
    body.walk_places(|place| map_place(place, &mut lookup));
    for (_, block) in body.basic_blocks.iter_mut() {
        for statement in &mut block.statements {
            if let StatementKind::StorageLive(local) | StatementKind::StorageDead(local) =
                &mut statement.kind
            {
                *local = lookup(*local);
            }
        }
    }
    let get = |local: LocalId| map.get(local_index(local)).copied().flatten();
    // Each list keeps its order and only gets shorter: a kept local renumbered, a removed one
    // gone.
    let renumber = |local: &mut LocalId| match get(*local) {
        Some(new) => {
            *local = new;
            true
        }
        None => false,
    };
    body.locals = new_locals;
    if body.binding_locals.values().all(|&it| get(it).is_some()) {
        for (_, local) in body.binding_locals.iter_mut() {
            *local = get(*local).expect("checked above");
        }
    } else {
        // `ArenaMap::remove` leaves a trailing `None`, which its equality and hash see, so the
        // map is made again at the length its largest kept binding gives it.
        let last = body.binding_locals.iter().rev().find(|&(_, &local)| get(local).is_some());
        let len = last.map_or(0, |(binding, _)| binding.into_raw().into_u32() as usize + 1);
        let mut bindings = ArenaMap::with_capacity(len);
        for (binding, &local) in body.binding_locals.iter() {
            if let Some(local) = get(local) {
                bindings.insert(binding, local);
            }
        }
        body.binding_locals = bindings;
    }
    body.param_locals.retain_mut(renumber);
    body.promoted_locals.retain_mut(|(local, _)| renumber(local));
    body.promoted_constants.retain_mut(renumber);
    body.scope_ends.retain_mut(renumber);
    body.upvars_in_place.retain_mut(|(local, _)| renumber(local));
    body.inlined.bindings.retain_mut(|it| renumber(&mut it.local));
    body.inlined.promoted.retain_mut(|it| renumber(&mut it.local));
    body.upvar_locals.retain(|_, locals| {
        locals.retain_mut(|(local, _)| renumber(local));
        !locals.is_empty()
    });
}

/// The block arena's targets and the body's lists of blocks renumbered by `map`, which says
/// where each old block went. The caller has already put the kept blocks in
/// `body.basic_blocks`.
fn renumber_blocks(body: &mut MirBody<'_>, map: &[Option<usize>]) {
    let moved = |block: BasicBlockId| map[block_index(block)].map(block_id);
    for (_, block) in body.basic_blocks.iter_mut() {
        if let Some(terminator) = &mut block.terminator {
            successors_mut(&mut terminator.kind, |it| {
                *it = moved(*it).expect("a removed block is a target")
            });
        }
    }
    body.start_block = moved(body.start_block).expect("start is kept");
    body.unreached.retain_mut(|block| match moved(*block) {
        Some(new) => {
            *block = new;
            true
        }
        None => false,
    });
    body.panic_unwinds.retain_mut(|(block, _, to)| match (moved(*block), moved(*to)) {
        (Some(new_block), Some(new_to)) => {
            (*block, *to) = (new_block, new_to);
            true
        }
        _ => false,
    });
}
