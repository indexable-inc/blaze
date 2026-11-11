//! `#![blaze::early_drop]` (docs/v2/plan/design/early-drop.md): a local whose destructor only
//! frees memory ([`class::drop_class`]) is dropped after its last use, not where its scope ends.
//!
//! It runs on the built body, before the borrow checker reads it, and only *inserts*
//! `Drop(x)` terminators, each on a block edge of its own ([`frontier::edges`]): after the
//! last use of `x` and of every local that may hold a borrow of it. The drops lowering
//! scheduled where the scope ends, and on the way out of a panic, stay: they find `x` gone
//! and drop elaboration makes them nothing, so drop flags, partial moves and conditional
//! initialization are elaboration's as before, and an inserted drop can never free twice. A
//! drop put where `x` is still used or borrowed would be a borrow-check error, not undefined
//! behavior. Without the attribute the pass does not run.
//!
//! What may hold a borrow of what is found from the statements alone (the [`frontier::Tie`]s
//! [`ties`] reads off them), coarser than the borrow checker's loans and never finer, so the
//! borrow checker accepts every drop placed. A local a raw pointer was made from is pinned:
//! it keeps its scope's drop ([`pinned`]).
//!
//! The liveness is the passes' one ([`live_on_entry`], dead store elimination's and
//! `#![blaze::handles]`'s), where a drop of an early local reads nothing and a drop of any
//! other local reads it. `mir::handles` (edition-e2-handles-1109) has the same two effect
//! functions with every drop reading nothing: one pair, taking which drops read, once both
//! are on main.

pub mod class;
mod frontier;

use base_db::Db;
use blaze_mem::{Frame, Tag, frame};
use hir_def::{DefWithBodyId, signatures::FunctionSignature};
use la_arena::RawIdx;
use rustc_type_ir::{TypeVisitableExt as _, inherent::IntoKind as _};

use crate::{InferBodyId, next_solver::TyKind};

use super::{
    BasicBlock, BasicBlockId, CastKind, Idx, LocalId, MirBody, MirLowerError, OperandKind, Place,
    Rvalue, StatementKind, Terminator, TerminatorKind,
    framed::{Bits, bits_in, block_id, block_index, local_index},
    optimize::{
        Location, Mention, live_on_entry, reverse_postorder, transfer, visit_place,
        visit_statement, visit_terminator,
    },
    return_slot,
    transform::Ctx,
};

use class::{DropClass, drop_class, may_hold_raw};
use frontier::{Block, EdgeDrops, Input, Owners, Plant, Tie};

type Result<'db, T> = std::result::Result<T, MirLowerError<'db>>;

/// The mentions of one statement or terminator, reused for each.
type Mentions<'f> = Vec<(LocalId, Mention), &'f Frame>;

/// Drops each early local of `body` after its last use, when the body's crate says
/// `#![blaze::early_drop]` and the body is a function's or a closure's in one. A `const fn`
/// (whose drops the const check counts) and the body of an `async` block are left as they are.
pub(super) fn drop_early<'db>(db: &'db Db, body: &mut MirBody<'db>) -> Result<'db, ()> {
    let InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(function)) = body.owner else {
        return Ok(());
    };
    let krate = body.owner.krate(db);
    if !hir_def::nameres::crate_def_map(db, krate).early_drop()
        || body.coroutine.is_some()
        || FunctionSignature::of(db, function).is_const()
    {
        return Ok(());
    }
    let _span = tracing::info_span!(target: "blaze", "mir.early_drop").entered();
    frame::task(Tag::Mir, |frame| {
        let ctx = Ctx::new(db, body.owner, frame);
        let found = place(&ctx, body, frame);
        insert(body, &found);
    });
    Ok(())
}

/// When each local of `body` is dropped under `#![blaze::early_drop]`, by the local's index,
/// whether or not the body's crate has the attribute: what a lint or a diagnostic says of a
/// local (`blaze::unused_guard`). A raw pointer made from a local pins it in its body, which
/// this, a fact of the type, does not say.
pub fn local_classes<'db>(db: &'db Db, body: &MirBody<'db>) -> Vec<DropClass> {
    frame::task(Tag::Mir, |frame| {
        let ctx = Ctx::new(db, body.owner, frame);
        let class = |local: &super::Local| {
            drop_class(&ctx.infcx, ctx.env, ctx.normalize(local.ty.as_ref()), frame)
        };
        body.locals.iter().map(|(_, it)| class(it)).collect() // alloc: the answer, one per local
    })
}

fn local_id(index: u32) -> LocalId {
    Idx::from_raw(RawIdx::from_u32(index))
}

fn index_of(local: LocalId) -> u32 {
    local_index(local) as u32
}

/// What the types of the locals say.
struct Locals<'f> {
    /// May be dropped early, before pinning.
    candidates: Bits<'f>,
    /// Have drop glue and keep their scope's drop.
    scoped: Bits<'f>,
    /// Have a lifetime in their type: may hold a borrow.
    carrier: Bits<'f>,
    /// Have no lifetime in their type, or are a reference to a type with none: a value put in
    /// one ties no lifetime of what it came from to another.
    simple: Bits<'f>,
    /// May be or hold a raw pointer ([`may_hold_raw`]).
    raw: Bits<'f>,
}

impl<'f> Locals<'f> {
    fn new(ctx: &Ctx<'_, '_>, body: &MirBody<'_>, frame: &'f Frame) -> Locals<'f> {
        let count = body.locals.len();
        let mut locals = Locals {
            candidates: bits_in(count, frame),
            scoped: bits_in(count, frame),
            carrier: bits_in(count, frame),
            simple: bits_in(count, frame),
            raw: bits_in(count, frame),
        };
        for (local, data) in body.locals.iter() {
            let at = local_index(local);
            let ty = ctx.normalize(data.ty.as_ref());
            if ty.has_regions() {
                locals.carrier.insert(at);
            }
            let simple = match ty.kind() {
                TyKind::Ref(_, inner, _) => !inner.has_regions(),
                _ => !ty.has_regions(),
            };
            if simple {
                locals.simple.insert(at);
            }
            if may_hold_raw(&ctx.infcx, ctx.env, ty, frame) {
                locals.raw.insert(at);
            }
            match drop_class(&ctx.infcx, ctx.env, ty, frame) {
                DropClass::None => {}
                DropClass::Early => {
                    locals.candidates.insert(at);
                }
                DropClass::Scoped(_) => {
                    locals.scoped.insert(at);
                }
            }
        }
        // The return place is the caller's, and a promoted temporary lives as a constant does.
        locals.candidates.remove(local_index(return_slot()));
        for &(local, _) in &body.promoted_locals {
            locals.candidates.remove(local_index(local));
        }
        locals
    }

    /// May hold a borrow or a pointer made from one.
    fn holds(&self, local: u32) -> bool {
        self.carrier.contains(local as usize) || self.raw.contains(local as usize)
    }
}

/// The ties of a body, and what pins.
struct Ties<'f> {
    ties: Vec<Tie, &'f Frame>,
    /// The locals whose value was made an address, or given to an intrinsic or `asm!`: what
    /// each may hold a borrow of is pinned.
    exposed: Vec<u32, &'f Frame>,
    /// `(into, from)`: a candidate `into` takes a value `from` is in.
    moves: Vec<(u32, u32), &'f Frame>,
}

/// Adds the local `operand` reads, if it reads one.
fn push_operand(operands: &mut Vec<u32, &Frame>, operand: &super::Operand) {
    if let OperandKind::Copy(place) | OperandKind::Move(place) = &operand.kind {
        operands.push(index_of(place.local));
    }
}

/// Reads the ties off the statements and terminators of `body`.
fn ties<'f>(body: &MirBody<'_>, locals: &Locals<'_>, frame: &'f Frame) -> Ties<'f> {
    let mut found =
        Ties { ties: Vec::new_in(frame), exposed: Vec::new_in(frame), moves: Vec::new_in(frame) };
    let mut operands: Vec<u32, &Frame> = Vec::new_in(frame);
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            match &statement.kind {
                StatementKind::Assign(place, Rvalue::Ref(_, of)) => {
                    let (into, of) = (index_of(place.local), index_of(of.local));
                    found.ties.push(Tie::Borrow { into, of });
                    if locals.carrier.contains(of as usize) {
                        found.ties.push(Tie::Referent { of: into, is: of });
                    }
                }
                StatementKind::Assign(place, rvalue) => {
                    operands.clear();
                    match rvalue {
                        Rvalue::CopyForDeref(from) => operands.push(index_of(from.local)),
                        _ => rvalue.for_each_operand(&mut |it| push_operand(&mut operands, it)),
                    }
                    found.flow(locals, index_of(place.local), &operands);
                    if let Rvalue::Cast(
                        CastKind::PointerExposeAddress | CastKind::Transmute,
                        ..,
                    ) = rvalue
                    {
                        found.exposed.extend_from_slice(&operands);
                    }
                }
                StatementKind::InlineAsm(_) => {
                    operands.clear();
                    statement.kind.for_each_operand(&mut |it| push_operand(&mut operands, it));
                    found.exposed.extend_from_slice(&operands);
                }
                _ => {}
            }
        }
        let Some(terminator) = &block.terminator else { continue };
        operands.clear();
        terminator.kind.for_each_operand(&mut |it| push_operand(&mut operands, it));
        match &terminator.kind {
            TerminatorKind::Call { destination, intrinsic, .. } => {
                found.flow(locals, index_of(destination.local), &operands);
                if intrinsic.is_some() {
                    found.exposed.extend_from_slice(&operands);
                }
            }
            TerminatorKind::DropAndReplace { place, .. } => {
                found.flow(locals, index_of(place.local), &operands);
            }
            _ => {}
        }
    }
    found
}

impl Ties<'_> {
    /// `into` is given a value made of `operands`: it may hold what each that can hold a
    /// borrow does, and two such operands of one statement may come to hold what the other
    /// does.
    fn flow(&mut self, locals: &Locals<'_>, into: u32, operands: &[u32]) {
        let mut last: Option<u32> = None;
        for &from in operands {
            if from == into {
                continue;
            }
            if locals.candidates.contains(into as usize) {
                self.moves.push((into, from));
            }
            if !locals.holds(from) {
                continue;
            }
            if locals.holds(into) {
                self.ties.push(match locals.simple.contains(into as usize) {
                    true => Tie::Flow { into, from },
                    false => Tie::Meet(into, from),
                });
            }
            if let Some(last) = last
                && last != from
            {
                self.ties.push(Tie::Meet(last, from));
            }
            last = Some(from);
        }
    }
}

/// The candidates that keep their scope's drop because a raw pointer may point into them:
/// each a local that may be or hold a raw pointer, or one of `exposed`, may hold a borrow of,
/// and each candidate that takes the value of a pinned one.
fn pinned<'f>(
    locals: &Locals<'_>,
    found: &Ties<'_>,
    owners: &Owners<'_>,
    count: usize,
    frame: &'f Frame,
) -> Bits<'f> {
    let mut pinned: Bits<'f> = bits_in(count, frame);
    for holder in locals.raw.iter() {
        owners.held_by(holder, |it| {
            pinned.insert(it);
        });
    }
    for &holder in found.exposed.iter() {
        owners.held_by(holder as usize, |it| {
            pinned.insert(it);
        });
    }
    // Each round but the last pins a local.
    for _ in 0..=count {
        let mut changed = false;
        for &(into, from) in found.moves.iter() {
            if pinned.contains(from as usize) {
                changed |= pinned.insert(into as usize);
            }
        }
        if !changed {
            break;
        }
    }
    pinned
}

/// Liveness across a statement, backwards: what it writes whole, then what it reads.
fn statement_effect(
    live: &mut Bits<'_>,
    statement: &StatementKind,
    at: Location,
    mentions: &mut Mentions<'_>,
) {
    mentions.clear();
    visit_statement(statement, at, &mut |local, mention, _| mentions.push((local, mention)));
    for &(local, mention) in mentions.iter() {
        if mention == Mention::Assigned {
            transfer(live, local, mention);
        }
    }
    for &(local, mention) in mentions.iter() {
        if mention != Mention::Assigned {
            transfer(live, local, mention);
        }
    }
}

/// Liveness across a terminator, backwards. A drop of a whole early local reads nothing: the
/// value is gone either way, and that is what lets it go sooner. A drop of any other local
/// reads it, which keeps a guard, and what it borrows, to the end of its scope.
fn terminator_effect(
    live: &mut Bits<'_>,
    kind: &TerminatorKind,
    at: Location,
    early: &Bits<'_>,
    mentions: &mut Mentions<'_>,
) {
    match kind {
        TerminatorKind::Drop { place, .. }
            if place.projection.as_slice().is_empty()
                && early.contains(local_index(place.local)) => {}
        TerminatorKind::DropAndReplace { place, value, .. }
            if place.projection.as_slice().is_empty()
                && early.contains(local_index(place.local)) =>
        {
            live.remove(local_index(place.local));
            super::optimize::visit_operand(value, at, &mut |local, mention, _| {
                transfer(live, local, mention)
            });
        }
        _ => {
            mentions.clear();
            visit_terminator(kind, at, &mut |local, mention, _| mentions.push((local, mention)));
            for &(local, mention) in mentions.iter() {
                if mention == Mention::CallReturned {
                    transfer(live, local, mention);
                }
            }
            for &(local, mention) in mentions.iter() {
                if mention != Mention::CallReturned {
                    transfer(live, local, mention);
                }
            }
        }
    }
}

/// The blocks an edge from a block ending in `kind` may carry a drop to: where control goes on
/// when nothing unwinds. Not the edges of a `yield` or of a `for const`, which are not edges
/// of the code that runs.
fn drop_targets(kind: &TerminatorKind, out: &mut Vec<u32, &Frame>) {
    match kind {
        TerminatorKind::Goto { target }
        | TerminatorKind::Drop { target, .. }
        | TerminatorKind::DropAndReplace { target, .. }
        | TerminatorKind::Assert { target, .. }
        | TerminatorKind::FalseEdge { real_target: target, .. }
        | TerminatorKind::FalseUnwind { real_target: target, .. }
        | TerminatorKind::Call { target: Some(target), .. } => {
            out.push(block_index(*target) as u32)
        }
        TerminatorKind::SwitchInt { targets, .. } => {
            out.extend(targets.all_targets().iter().map(|&it| block_index(it) as u32))
        }
        _ => {}
    }
}

/// Makes each way from a block ending in `kind` to `to` that [`drop_targets`] lists go to
/// `through` instead.
fn retarget(kind: &mut TerminatorKind, to: BasicBlockId, through: BasicBlockId) {
    match kind {
        TerminatorKind::Goto { target }
        | TerminatorKind::Drop { target, .. }
        | TerminatorKind::DropAndReplace { target, .. }
        | TerminatorKind::Assert { target, .. }
        | TerminatorKind::FalseEdge { real_target: target, .. }
        | TerminatorKind::FalseUnwind { real_target: target, .. }
        | TerminatorKind::Call { target: Some(target), .. } => {
            if *target == to {
                *target = through;
            }
        }
        TerminatorKind::SwitchInt { targets, .. } => {
            for target in targets.targets.iter_mut() {
                if *target == to {
                    *target = through;
                }
            }
        }
        _ => {}
    }
}

/// The locals whose scope ends where `start` starts: those dropped whole by the chain of
/// blocks from it that hold nothing but storage statements and the assignment of a constant
/// (the `()` a block leaves) and end in such a drop. Nothing in the chain reads a local, so a
/// drop before it frees no borrow sooner.
fn scope_ends_at(body: &MirBody<'_>, start: BasicBlockId, out: &mut Bits<'_>) {
    let mut at = start;
    // A chain visits each block once.
    for _ in 0..body.basic_blocks.len() {
        let block = &body.basic_blocks[at];
        let only_storage = block.statements.iter().all(|it| match &it.kind {
            StatementKind::StorageDead(_) | StatementKind::StorageLive(_) | StatementKind::Nop => {
                true
            }
            StatementKind::Assign(place, Rvalue::Use(value)) => {
                place.projection.as_slice().is_empty()
                    && matches!(value.kind, OperandKind::Constant { .. })
            }
            _ => false,
        });
        if !only_storage {
            return;
        }
        match &block.terminator {
            Some(Terminator { kind: TerminatorKind::Drop { place, target, .. }, .. })
                if place.projection.as_slice().is_empty() =>
            {
                out.insert(local_index(place.local));
                at = *target;
            }
            _ => return,
        }
    }
}

/// Per block, the locals that have storage where it ends: those with no `StorageLive` in the
/// body (parameters, the return place) always, the others between their `StorageLive` and
/// their `StorageDead` on some path.
fn storage_out<'f>(
    body: &MirBody<'_>,
    order: &[BasicBlockId],
    frame: &'f Frame,
) -> Vec<Bits<'f>, &'f Frame> {
    let locals = body.locals.len();
    let blocks = body.basic_blocks.len();
    let mut always: Bits<'f> = bits_in(locals, frame);
    always.insert_range(0..locals);
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            if let StatementKind::StorageLive(local) = statement.kind {
                always.remove(local_index(local));
            }
        }
    }
    let mut entry: Vec<Bits<'f>, &Frame> = Vec::with_capacity_in(blocks, frame);
    entry.extend((0..blocks).map(|_| bits_in(locals, frame)));
    let mut exit: Vec<Bits<'f>, &Frame> = Vec::with_capacity_in(blocks, frame);
    exit.extend((0..blocks).map(|_| bits_in(locals, frame)));
    let mut now: Bits<'f> = bits_in(locals, frame);
    // A forward bit problem over reverse postorder settles in at most the blocks' count of
    // rounds, and three more.
    for _ in 0..blocks + 3 {
        let mut changed = false;
        for &bb in order {
            let block = &body.basic_blocks[bb];
            now.copy_from(&entry[block_index(bb)]);
            now.union_with(&always);
            for statement in &block.statements {
                match statement.kind {
                    StatementKind::StorageLive(local) => {
                        now.insert(local_index(local));
                    }
                    StatementKind::StorageDead(local) => {
                        now.remove(local_index(local));
                    }
                    _ => {}
                }
            }
            exit[block_index(bb)].copy_from(&now);
            if let Some(terminator) = &block.terminator {
                for target in terminator.kind.all_successors() {
                    changed |= entry[block_index(target)].union_with(&now);
                }
            }
        }
        if !changed {
            break;
        }
    }
    exit
}

/// The early drops of `body`, by edge.
fn place<'f>(
    ctx: &Ctx<'_, '_>,
    body: &MirBody<'_>,
    frame: &'f Frame,
) -> Vec<EdgeDrops<'f>, &'f Frame> {
    let count = body.locals.len();
    let locals = Locals::new(ctx, body, frame);
    if locals.candidates.is_empty() {
        return Vec::new_in(frame);
    }
    let found = ties(body, &locals, frame);
    let owners =
        Owners::new(frame, count, &locals.candidates, &locals.scoped, &found.ties, Plant::None);
    let pinned = pinned(&locals, &found, &owners, count, frame);
    let mut early: Bits<'f> = bits_in(count, frame);
    early.copy_from(&locals.candidates);
    early.subtract(&pinned);
    if early.is_empty() {
        return Vec::new_in(frame);
    }

    let order = reverse_postorder(body, frame);
    let live = {
        let mut statement_mentions: Mentions<'_> = Vec::new_in(frame);
        let mut terminator_mentions: Mentions<'_> = Vec::new_in(frame);
        let early = &early;
        live_on_entry(
            body,
            &order,
            frame,
            |live, statement, at| statement_effect(live, statement, at, &mut statement_mentions),
            |live, kind, at| terminator_effect(live, kind, at, early, &mut terminator_mentions),
        )
    };
    let storage = storage_out(body, &order, frame);
    let mut reached: Bits<'f> = bits_in(body.basic_blocks.len(), frame);
    for &bb in order.iter() {
        reached.insert(block_index(bb));
    }
    for &bb in &body.unreached {
        reached.remove(block_index(bb));
    }

    // Where each local's scope ends among the scopes' ends.
    let mut rank: Vec<u32, &Frame> = Vec::with_capacity_in(count, frame);
    rank.resize(count, u32::MAX);
    for (at, &local) in body.scope_ends.iter().enumerate() {
        let slot = &mut rank[local_index(local)];
        *slot = (*slot).min(at as u32);
    }

    let mut blocks: Vec<Block<'f>, &Frame> = Vec::with_capacity_in(body.basic_blocks.len(), frame);
    let mut mentions: Mentions<'_> = Vec::new_in(frame);
    for ((id, block), (live_in, storage_out)) in
        body.basic_blocks.iter().zip(live.into_iter().zip(storage))
    {
        let mut facts = Block {
            live_in,
            touched: bits_in(count, frame),
            moved_last: bits_in(count, frame),
            storage_out,
            ends: bits_in(count, frame),
            successors: Vec::new_in(frame),
            closed: block.is_cleanup || !reached.contains(block_index(id)),
        };
        scope_ends_at(body, id, &mut facts.ends);
        let touch = |facts: &mut Block<'_>, mentions: &Mentions<'_>| {
            for &(local, mention) in mentions.iter() {
                if mention != Mention::Storage {
                    facts.touched.insert(local_index(local));
                    facts.moved_last.remove(local_index(local));
                }
            }
        };
        let moved = |facts: &mut Block<'_>, operand: &super::Operand| {
            if let OperandKind::Move(place) = &operand.kind
                && place.projection.as_slice().is_empty()
            {
                facts.moved_last.insert(local_index(place.local));
            }
        };
        for (index, statement) in block.statements.iter().enumerate() {
            let at = Location { block: id, index };
            mentions.clear();
            visit_statement(&statement.kind, at, &mut |local, mention, _| {
                mentions.push((local, mention))
            });
            touch(&mut facts, &mentions);
            statement.kind.for_each_operand(&mut |it| moved(&mut facts, it));
            if let StatementKind::Assign(place, _) = &statement.kind
                && place.projection.as_slice().is_empty()
            {
                facts.moved_last.remove(local_index(place.local));
            }
        }
        if let Some(terminator) = &block.terminator {
            let at = Location { block: id, index: block.statements.len() };
            mentions.clear();
            match &terminator.kind {
                // The drop of a whole local touches nothing: it is what is being placed.
                TerminatorKind::Drop { place, .. } if place.projection.as_slice().is_empty() => {}
                TerminatorKind::Drop { place, .. } => {
                    visit_place(place, Mention::Mutated, at, &mut |local, mention, _| {
                        mentions.push((local, mention))
                    });
                }
                kind => visit_terminator(kind, at, &mut |local, mention, _| {
                    mentions.push((local, mention))
                }),
            }
            touch(&mut facts, &mentions);
            terminator.kind.for_each_operand(&mut |it| moved(&mut facts, it));
            if let TerminatorKind::Call { destination: place, .. }
            | TerminatorKind::DropAndReplace { place, .. } = &terminator.kind
                && place.projection.as_slice().is_empty()
            {
                facts.moved_last.remove(local_index(place.local));
            }
            drop_targets(&terminator.kind, &mut facts.successors);
        }
        blocks.push(facts);
    }
    let input =
        Input { locals: count, early: &early, scoped: &locals.scoped, rank: &rank, blocks: &blocks };
    frontier::edges(frame, &input, &owners, Plant::None)
}

/// Puts the drops of each edge on it: a block per drop, the first to run entered from the
/// edge's source, the last going on to its target. A drop that unwinds goes where the
/// terminator of the source does: what is live there is live here, less what that terminator
/// just made.
fn insert(body: &mut MirBody<'_>, found: &[EdgeDrops<'_>]) {
    for edge in found {
        let from = block_id(edge.from as usize);
        let to = block_id(edge.to as usize);
        let Some(terminator) = &body.basic_blocks[from].terminator else { continue };
        let (span, unwind) = (terminator.span, terminator.kind.unwind());
        let mut target = to;
        for &local in edge.locals.iter().rev() {
            let place = Place::from(local_id(local)).store();
            target = body.basic_blocks.alloc(BasicBlock {
                statements: Vec::new(), // alloc: empty
                terminator: Some(Terminator {
                    span,
                    kind: TerminatorKind::Drop { place, target, unwind },
                }),
                is_cleanup: false,
            });
        }
        if let Some(terminator) = &mut body.basic_blocks[from].terminator {
            retarget(&mut terminator.kind, to, target);
        }
    }
}
