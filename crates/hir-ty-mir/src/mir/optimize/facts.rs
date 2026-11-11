//! `SpendFacts`, blaze's own (docs/v2/plan/design/facts.md): the two changes slice 1 makes to
//! a body because of what is proved of the functions it calls, each read through the one
//! query ([`facts_of`]) and each recorded in `body.inlined.relied` ([`Facts::license`]), which
//! keys the body's code (rule 21).
//!
//! - [`unwind_edges`] ([`Spend::UnwindEdge`]): a call of a function proved unable to panic
//!   loses its unwind edge. The cleanup only that edge reached is then unreachable, and
//!   `SimplifyCfg`, which runs after, removes it. A caller all of whose calls are such, and
//!   which has no panic of its own, is proved unable to panic by the same analysis, so the
//!   property goes up the call graph with no work here.
//! - [`merge_repeats`] ([`Spend::MergeRepeat`]): a call of a pure function directly after a
//!   call of the same function with the same arguments takes the first's result.
//!
//! What is never changed: a call through a `dyn` or a function pointer, of a closure, of a
//! foreign function, of a function whose ABI is not Rust's (`extern "C-unwind"` and
//! `extern "C"` keep rustc's rules), of a trait method no impl is chosen for in the generic
//! body, and of an intrinsic ([`Callee::asked`]). A `Drop` terminator's and an `Assert`'s
//! edges are not this pass's: the first needs the fact of a type's drop glue (slice 2), the
//! second is the body's own panic.
//!
//! In a binary that links no analyses (a crate below `hir-ty-pure` testing itself) nothing
//! is proved and the pass changes nothing ([`linked`]).

use hir_def::{DefWithBodyId, FunctionId, ItemContainerId, signatures::FunctionSignature};
use rustc_abi::ExternAbi;
use rustc_type_ir::inherent::IntoKind as _;

use crate::{
    InferBodyId,
    facts::{Callee, Fact, FactItem, Facts, Spend, facts_of, linked},
    mir::{
        BasicBlockId, InlinedBody, LocalId, MirBody, Operand, OperandKind, Rvalue, Statement,
        StatementKind, StoredPlace, Terminator, TerminatorKind,
        framed::{block_id, block_index},
        transform::Ctx,
    },
    next_solver::{Ty, TyKind},
    traits::ParamEnvAndCrate,
};

use super::{
    fn_can_unwind,
    inline::{fn_def, fold, resolve},
};

/// How the call that ends a block names what it calls, and the function when it is one a fact
/// may be asked of.
fn callee<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &MirBody<'db>,
    func: &Operand,
    intrinsic: bool,
) -> (Callee, Option<FunctionId>) {
    if intrinsic {
        return (Callee::Unresolved, None);
    }
    let Some((named, args)) = fn_def(body, func) else { return (Callee::Pointer, None) };
    let Some(args) = fold(ctx, args, None) else { return (Callee::Unresolved, None) };
    let env = ParamEnvAndCrate { param_env: ctx.env, krate: ctx.krate() };
    let item = match resolve(ctx, env, named, args) {
        Some((InlinedBody::Fn(item), _)) => item,
        Some((InlinedBody::Closure(_), _)) => return (Callee::Closure, None),
        // A `dyn` call, a shim, a method of a type parameter, a foreign function (it has no
        // body): nothing names the code that runs.
        None => {
            let foreign =
                matches!(named.loc(ctx.db).container, ItemContainerId::ExternBlockId(_));
            return (if foreign { Callee::Foreign } else { Callee::Unresolved }, None);
        }
    };
    if matches!(item.loc(ctx.db).container, ItemContainerId::ExternBlockId(_)) {
        return (Callee::Foreign, None);
    }
    let rust = matches!(
        FunctionSignature::of(ctx.db, item).abi,
        ExternAbi::Rust | ExternAbi::RustCall | ExternAbi::RustCold
    );
    if !rust {
        return (Callee::OtherAbi, None);
    }
    (Callee::Direct, Some(item))
}

/// The facts of the callee of the call that ends `block`, if it is one a fact is asked of.
fn callee_facts<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &MirBody<'db>,
    block: BasicBlockId,
) -> Option<&'db Facts> {
    let Some(Terminator { kind: TerminatorKind::Call { func, intrinsic, .. }, .. }) =
        &body.basic_blocks[block].terminator
    else {
        return None;
    };
    let (how, item) = callee(ctx, body, func, intrinsic.is_some());
    if !how.asked() {
        return None;
    }
    Some(facts_of(ctx.db, FactItem::Fn(item?)))
}

/// [`Spend::UnwindEdge`] over `body`: says how many edges went.
pub(super) fn unwind_edges<'db>(ctx: &Ctx<'_, 'db>, body: &mut MirBody<'db>) -> u32 {
    if linked().is_none() || body.coroutine.is_some() {
        return 0;
    }
    // A function whose ABI ends unwinding keeps rustc's edges as they are.
    if let InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(own)) = body.owner
        && !fn_can_unwind(ctx.db, own)
    {
        return 0;
    }
    let mut gone = 0;
    for at in 0..body.basic_blocks.len() {
        let block = block_id(at);
        let has_edge = matches!(
            &body.basic_blocks[block].terminator,
            Some(Terminator { kind: TerminatorKind::Call { cleanup: Some(_), .. }, .. })
        );
        if !has_edge {
            continue;
        }
        let Some(facts) = callee_facts(ctx, body, block) else { continue };
        let Some(relied) = facts.license(Spend::UnwindEdge) else { continue };
        if let Some(Terminator { kind: TerminatorKind::Call { cleanup, .. }, .. }) =
            &mut body.basic_blocks[block].terminator
        {
            *cleanup = None;
        }
        for digest in relied {
            body.inlined.rely(digest);
        }
        gone += 1;
    }
    // The body's own: the code generator marks a function `nounwind` where its body relied
    // on the function's own fact, so the mark is in the key that covers it.
    if let InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(own)) = body.owner {
        let facts = facts_of(ctx.db, FactItem::Fn(own));
        if facts.spendable(Fact::NoUnwind) {
            body.inlined.rely(facts.relied(Fact::NoUnwind));
        }
    }
    if gone != 0 {
        tracing::debug!(target: "blaze", gone, "unwind edges removed on facts");
    }
    gone
}

/// Whether the body's code may be marked as never unwinding: its optimized body relied on
/// its own [`Fact::NoUnwind`] ([`unwind_edges`]). The one question the code generator asks
/// for `nounwind`; it reads the body, so whatever keys the body's code keys the mark.
pub fn relied_on_own_no_unwind(db: &base_db::Db, body: &MirBody<'_>) -> bool {
    let InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(own)) = body.owner else {
        return false;
    };
    if linked().is_none() || body.inlined.relied.is_empty() {
        return false;
    }
    let digest = facts_of(db, FactItem::Fn(own)).relied(Fact::NoUnwind);
    body.inlined.relied.contains(&digest)
}

/// A value copied bit for bit with nothing behind it: a second copy of it is the same value
/// whatever ran between, and holds nothing a call could change or free.
fn plain(ty: Ty<'_>) -> bool {
    matches!(
        ty.kind(),
        TyKind::Bool | TyKind::Char | TyKind::Int(_) | TyKind::Uint(_) | TyKind::Float(_)
    )
}

/// The local an operand reads whole, or `None` for a constant.
enum Arg<'a> {
    Local(LocalId),
    Constant(&'a Operand),
}

fn arg<'a>(body: &MirBody<'_>, operand: &'a Operand) -> Option<Arg<'a>> {
    match &operand.kind {
        OperandKind::Copy(place) | OperandKind::Move(place)
            if place.projection.is_empty() && plain(body.locals[place.local].ty.as_ref()) =>
        {
            Some(Arg::Local(place.local))
        }
        OperandKind::Constant { ty, .. } if plain(ty.as_ref()) => Some(Arg::Constant(operand)),
        _ => None,
    }
}

fn same_args(body: &MirBody<'_>, first: &[Operand], second: &[Operand], result: LocalId) -> bool {
    first.len() == second.len()
        && first.iter().zip(second).all(|(a, b)| match (arg(body, a), arg(body, b)) {
            // The first call's result is written between the two reads of it.
            (Some(Arg::Local(a)), Some(Arg::Local(b))) => a == b && a != result,
            (Some(Arg::Constant(a)), Some(Arg::Constant(b))) => a.kind == b.kind,
            _ => false,
        })
}

fn whole_plain(body: &MirBody<'_>, place: &StoredPlace) -> Option<LocalId> {
    (place.projection.is_empty() && plain(body.locals[place.local].ty.as_ref()))
        .then_some(place.local)
}

/// The block whose call `second`'s call repeats, with the local its result is in: `second`
/// is reached only from that block's call returning, runs nothing between but storage
/// markers, and calls the same function (the same instance: the `FnDef` types are equal) with
/// the same plain arguments into a plain local.
fn repeated(
    body: &MirBody<'_>,
    predecessors: &[(u32, BasicBlockId)],
    second: BasicBlockId,
) -> Option<(BasicBlockId, LocalId)> {
    let (count, first) = predecessors[block_index(second)];
    if count != 1 || first == second {
        return None;
    }
    let (a, b) = (&body.basic_blocks[first], &body.basic_blocks[second]);
    if a.is_cleanup || b.is_cleanup {
        return None;
    }
    let Some(Terminator {
        kind:
            TerminatorKind::Call {
                func: first_func,
                args: first_args,
                destination: first_into,
                target: Some(returns_to),
                intrinsic: None,
                ..
            },
        ..
    }) = &a.terminator
    else {
        return None;
    };
    let Some(Terminator {
        kind:
            TerminatorKind::Call {
                func: second_func,
                args: second_args,
                destination: second_into,
                target: Some(_),
                intrinsic: None,
                ..
            },
        ..
    }) = &b.terminator
    else {
        return None;
    };
    if *returns_to != second {
        return None;
    }
    let (OperandKind::Constant { ty: first_ty, .. }, OperandKind::Constant { ty: second_ty, .. }) =
        (&first_func.kind, &second_func.kind)
    else {
        return None;
    };
    if first_ty != second_ty {
        return None;
    }
    let result = whole_plain(body, first_into)?;
    whole_plain(body, second_into)?;
    if !same_args(body, first_args, second_args, result) {
        return None;
    }
    let only_markers = b.statements.iter().all(|it| match &it.kind {
        StatementKind::StorageLive(local) | StatementKind::StorageDead(local) => *local != result,
        StatementKind::Nop => true,
        _ => false,
    });
    only_markers.then_some((first, result))
}

/// How many blocks go to each block when nothing unwinds, and one of them.
fn predecessors(body: &MirBody<'_>) -> Vec<(u32, BasicBlockId)> {
    let mut found = vec![(0u32, block_id(0)); body.basic_blocks.len()]; // alloc: one pair a block, for the pass
    for (from, block) in body.basic_blocks.iter() {
        let Some(terminator) = &block.terminator else { continue };
        for to in terminator.kind.successors().chain(terminator.kind.unwind()) {
            let entry = &mut found[block_index(to)];
            entry.0 += 1;
            entry.1 = from;
        }
    }
    // The entry block is entered from outside.
    if let Some(entry) = found.get_mut(block_index(body.start_block)) {
        entry.0 += 1;
    }
    found
}

/// [`Spend::MergeRepeat`] over `body`: says how many calls went.
pub(super) fn merge_repeats<'db>(ctx: &Ctx<'_, 'db>, body: &mut MirBody<'db>) -> u32 {
    if linked().is_none() || body.coroutine.is_some() {
        return 0;
    }
    let predecessors = predecessors(body);
    let mut gone = 0;
    for at in 0..body.basic_blocks.len() {
        let second = block_id(at);
        let Some((first, result)) = repeated(body, &predecessors, second) else { continue };
        // The first call's block still ends in the call: a merged call is a `Goto`, which
        // `repeated` does not match, so a third call in a row merges with the first's result
        // through the second's copy on the next run, never with a call that is gone.
        let Some(facts) = callee_facts(ctx, body, first) else { continue };
        let Some(relied) = facts.license(Spend::MergeRepeat) else { continue };
        let block = &mut body.basic_blocks[second];
        let Some(terminator) = &mut block.terminator else { continue };
        let TerminatorKind::Call { destination, target: Some(target), .. } = &terminator.kind
        else {
            continue;
        };
        let (into, target) = (destination.clone(), *target); // clone: a place's refcount
        let from = StoredPlace { local: result, projection: into.projection.clone() }; // clone: the empty projection
        block.statements.push(Statement {
            kind: StatementKind::Assign(
                into,
                Rvalue::Use(Operand { kind: OperandKind::Copy(from), span: None }),
            ),
            span: terminator.span,
        });
        terminator.kind = TerminatorKind::Goto { target };
        for digest in relied {
            body.inlined.rely(digest);
        }
        gone += 1;
    }
    if gone != 0 {
        tracing::debug!(target: "blaze", gone, "repeated pure calls merged on facts");
    }
    gone
}
