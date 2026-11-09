//! The pass: a bounds check whose index a proved `ensures` bounds is removed, over a body at
//! [`MirPhase::Runtime`](hir_ty::mir::MirPhase) or later, where each check is an `Assert`
//! (`hir-ty-mir`'s `transform/asserts.rs`): `lt = Lt(copy index, len); assert(move lt) -> next`.

use base_db::Db;
use blaze_items::{ItemId, item_key};
use blaze_mem::{Frame, Tag, frame};
use hir_def::{CallableDefId, FunctionId, HasModule as _};
use hir_ty::{
    InferBodyId,
    consteval::try_const_usize,
    mir::{
        AsmOperand, AssertKind, BasicBlockId, BinOp, CastKind, LocalId, MirBody, Operand,
        OperandKind, Rvalue, StatementKind, StoredPlace, TerminatorKind,
    },
    next_solver::{GenericArgs, TyKind},
};
use hir_ty_verus::{Clause, proved};
use rustc_type_ir::inherent::IntoKind as _;

use crate::{
    Entry, Fact, How, Use,
    clause::{self, ReturnBound},
};

/// How many copies and casts may stand between a call's destination and an index.
const MAX_STEPS: u32 = 8;

/// Removes each bounds check of `body` that a proved contract makes always pass, and gives the
/// facts it used, one per check removed, in block order. A check is kept wherever the fact is
/// not proved, not of the one form read, or not shown to be about the very value the check
/// tests; keeping a check is always correct.
pub fn use_proved_contracts<'db>(db: &'db Db, body: &mut MirBody<'db>) -> Vec<Fact> {
    let owner = owner_path(db, body.owner);
    let _span =
        tracing::info_span!(target: "blaze", "contracts.use_proved", owner = %owner).entered();
    let mut facts = Vec::new(); // alloc: the facts outlive the pass's frame, kept with the body
    frame::task(Tag::Mir, |frame| {
        let defs = Defs::of(body, frame);
        let mut callees: Vec<(FunctionId, Option<Usable>), &Frame> = Vec::new_in(frame);
        let mut removed: Vec<Removal, &Frame> = Vec::new_in(frame);
        for (block, data) in body.basic_blocks.iter() {
            let Some(terminator) = &data.terminator else { continue };
            let TerminatorKind::Assert {
                cond,
                expected: true,
                msg: AssertKind::BoundsCheck { len, index },
                target,
                ..
            } = &terminator.kind
            else {
                continue;
            };
            let (Some(cond), Some(index)) = (whole_local(cond), whole_local(index)) else {
                continue;
            };
            // An array's length is a constant; a slice's is read from the place.
            let OperandKind::Constant { konst, .. } = &len.kind else { continue };
            let Some(length) = try_const_usize(db, konst.as_ref()) else { continue };
            // The check tests `index < len`, computed here from these very operands.
            if !defs.single(cond) || !compares(&data.statements, cond, index, len) {
                continue;
            }
            let Some(call) = defs.returned(body, index) else { continue };
            let Some((callee, args)) = direct_callee(body, call) else { continue };
            let at = match callees.iter().position(|(it, _)| *it == callee) {
                Some(at) => at,
                None => {
                    callees.push((callee, usable(db, callee, args)));
                    callees.len() - 1
                }
            };
            let Some(usable) = &callees[at].1 else { continue };
            if usable.bound.below > length {
                continue;
            }
            let fact = Fact {
                callee: item_key(db, ItemId::Function(callee)),
                clause: usable.bound.clause,
                how: usable.how,
                entry: usable.entry,
                used: Use::BoundsCheckRemoved,
            };
            removed.push(Removal { block, target: *target, callee, fact });
        }
        for removal in removed.iter() {
            let Removal { block, target, callee, fact } = *removal;
            let usable =
                callees.iter().find(|(it, _)| *it == callee).and_then(|(_, it)| it.as_ref());
            let text = usable.map_or("", |it| it.bound.text.as_str());
            let _span = tracing::info_span!(
                target: "blaze",
                "contracts.fact",
                callee = %blaze_items::key::path(db, ItemId::Function(callee)),
                clause = %format_args!("ensures {text}"),
                how = %fact.how,
                entry = %fact.entry,
                r#use = %fact.used,
            )
            .entered();
            // The condition's computation stays, read by nothing: a later pass removes it.
            if let Some(terminator) = body.basic_blocks[block].terminator.as_mut() {
                terminator.kind = TerminatorKind::Goto { target };
            }
            facts.push(fact);
        }
    });
    facts
}

/// A check to remove, with the fact it rests on.
#[derive(Clone, Copy)]
struct Removal {
    block: BasicBlockId,
    target: BasicBlockId,
    callee: FunctionId,
    fact: Fact,
}

/// What a callee's contract lets its callers use: the bound on its return, why its `requires`
/// held, and the proof.
struct Usable {
    bound: ReturnBound,
    entry: Entry,
    how: How,
}

/// What a call of `callee` returns is bounded by a proved `ensures`, if it is. The clause is
/// read first, so a callee with nothing usable never reaches the solver.
fn usable(db: &Db, callee: FunctionId, args: GenericArgs<'_>) -> Option<Usable> {
    let contract = clause::contract(db, callee);
    let bound = contract.bound?;
    let entry = match contract.has_requires {
        false => Entry::NoRequires,
        true if callee.krate(db).cfg_options(db).contract_checks() => Entry::RequiresChecked,
        // A proved `ensures` assumes the `requires`, which nothing checks here; using the
        // `requires` at the call site needs `Clause::Requires`, which lane verus does not answer
        // yet.
        true => return None,
    };
    let key = {
        let _span = tracing::info_span!(
            target: "blaze",
            "contracts.prove",
            callee = %blaze_items::key::path(db, ItemId::Function(callee)),
        )
        .entered();
        proved(db, callee, args.store(), Clause::Ensures)?
    };
    Some(Usable { bound, entry, how: How::Verus(key) })
}

/// The function a call at the end of `block` calls directly, with its (empty) generic
/// arguments, if it is a non-generic function.
fn direct_callee<'db>(
    body: &MirBody<'db>,
    block: BasicBlockId,
) -> Option<(FunctionId, GenericArgs<'db>)> {
    let TerminatorKind::Call { func, .. } = &body.basic_blocks[block].terminator.as_ref()?.kind
    else {
        return None;
    };
    let ty = match &func.kind {
        OperandKind::Constant { ty, .. } => ty.as_ref(),
        OperandKind::Copy(place) | OperandKind::Move(place) if place.projection.is_empty() => {
            body.locals[place.local].ty.as_ref()
        }
        _ => return None,
    };
    let TyKind::FnDef(def, args) = ty.kind() else { return None };
    let CallableDefId::FunctionId(callee) = def.0 else { return None };
    let args = args.skip_binder();
    args.is_empty().then_some((callee, args))
}

/// Whether `statements` assign `cond` the comparison `index < len`.
fn compares(
    statements: &[hir_ty::mir::Statement],
    cond: LocalId,
    index: LocalId,
    len: &Operand,
) -> bool {
    statements.iter().any(|statement| match &statement.kind {
        StatementKind::Assign(place, Rvalue::BinaryOp(BinOp::Lt, a, b)) => {
            place.local == cond
                && place.projection.is_empty()
                && whole_local(a) == Some(index)
                && b.kind == len.kind
        }
        _ => false,
    })
}

/// The local `operand` reads as a whole.
fn whole_local(operand: &Operand) -> Option<LocalId> {
    match &operand.kind {
        OperandKind::Copy(place) | OperandKind::Move(place) if place.projection.is_empty() => {
            Some(place.local)
        }
        _ => None,
    }
}

fn is_unsigned(body: &MirBody<'_>, local: LocalId) -> bool {
    matches!(body.locals[local].ty.as_ref().kind(), TyKind::Uint(_))
}

/// The one assignment a local has as a whole, by what it assigns.
#[derive(Clone, Copy, Default)]
enum Def {
    #[default]
    None,
    /// A call returns to it, at the end of this block.
    Call(BasicBlockId),
    /// A copy or move of a local.
    Copy(LocalId),
    /// An integer cast of a local.
    Cast(LocalId),
    /// Anything else.
    Other,
}

#[derive(Clone, Copy, Default)]
struct Slot {
    /// How many times it is assigned as a whole, a value the body starts with counted once.
    defs: u32,
    /// The last of them.
    def: Def,
    /// Borrowed, assigned in part, dropped, deinitialized, an `asm!` output or a coroutine's
    /// resume argument: something other than its assignments may change it.
    escaped: bool,
}

/// Each local's assignments, over the whole body.
struct Defs<'f> {
    slots: &'f mut [Slot],
}

impl<'f> Defs<'f> {
    fn of(body: &MirBody<'_>, frame: &'f Frame) -> Defs<'f> {
        let slots = frame.alloc_slice_fill_with(body.locals.len(), |_| Slot::default());
        let mut defs = Defs { slots };
        // Values the body starts with, which no statement assigns: its parameters, an `async`
        // body's captures, and the constants the code generator gives promoted locals.
        let given = body.param_locals.iter().copied();
        let given = given.chain(body.upvars_in_place.iter().map(|it| it.0));
        let given = given.chain(body.promoted_locals.iter().map(|it| it.0));
        for local in given.chain(body.promoted_constants.iter().copied()) {
            defs.assign(local, Def::Other);
        }
        for (block, data) in body.basic_blocks.iter() {
            for statement in &data.statements {
                defs.statement(&statement.kind);
            }
            if let Some(terminator) = &data.terminator {
                defs.terminator(block, &terminator.kind);
            }
        }
        defs
    }

    fn slot(&mut self, local: LocalId) -> &mut Slot {
        &mut self.slots[local.into_raw().into_u32() as usize]
    }

    fn assign(&mut self, local: LocalId, def: Def) {
        let slot = self.slot(local);
        slot.defs = slot.defs.saturating_add(1);
        slot.def = def;
    }

    fn escape(&mut self, place: &StoredPlace) {
        self.slot(place.local).escaped = true;
    }

    fn write(&mut self, place: &StoredPlace, def: Def) {
        match place.projection.is_empty() {
            true => self.assign(place.local, def),
            false => self.escape(place),
        }
    }

    // Reads change nothing: an index local in a projection, an operand, a `Len`.

    fn statement(&mut self, statement: &StatementKind) {
        match statement {
            StatementKind::Assign(place, rvalue) => {
                if let Rvalue::Ref(_, borrowed) = rvalue {
                    self.escape(borrowed);
                }
                let def = match rvalue {
                    Rvalue::Use(operand) => whole_local(operand).map_or(Def::Other, Def::Copy),
                    Rvalue::Cast(CastKind::IntToInt, operand, _) => {
                        whole_local(operand).map_or(Def::Other, Def::Cast)
                    }
                    _ => Def::Other,
                };
                self.write(place, def);
            }
            StatementKind::Deinit(place) => self.escape(place),
            StatementKind::InlineAsm(asm) => {
                for operand in &asm.operands {
                    if let AsmOperand::Reg { output: Some(output), .. } = operand {
                        self.escape(output);
                    }
                }
                if let Some(taken) = &asm.taken {
                    self.escape(taken);
                }
            }
            StatementKind::FakeRead(_)
            | StatementKind::StorageLive(_)
            | StatementKind::StorageDead(_)
            | StatementKind::Nop => {}
        }
    }

    fn terminator(&mut self, block: BasicBlockId, terminator: &TerminatorKind) {
        match terminator {
            TerminatorKind::Call { destination, .. } => self.write(destination, Def::Call(block)),
            TerminatorKind::Drop { place, .. } | TerminatorKind::DropAndReplace { place, .. } => {
                self.escape(place)
            }
            TerminatorKind::Yield { resume_arg, .. } => self.escape(resume_arg),
            TerminatorKind::Goto { .. }
            | TerminatorKind::SwitchInt { .. }
            | TerminatorKind::UnwindResume
            | TerminatorKind::Abort
            | TerminatorKind::Return
            | TerminatorKind::Unreachable
            | TerminatorKind::TailCall { .. }
            | TerminatorKind::Assert { .. }
            | TerminatorKind::CoroutineDrop
            | TerminatorKind::FalseEdge { .. }
            | TerminatorKind::FalseUnwind { .. }
            | TerminatorKind::ComptimeFor { .. }
            | TerminatorKind::ComptimeNext { .. } => {}
        }
    }

    /// Assigned once, by a statement or call of the body or as a value it starts with, and
    /// changed by nothing else: every read of it reads that one value.
    fn single(&self, local: LocalId) -> bool {
        let slot = &self.slots[local.into_raw().into_u32() as usize];
        slot.defs == 1 && !slot.escaped
    }

    /// The block whose call returned the value `local` holds, through copies and unsigned
    /// integer casts, each local of the chain unsigned and [`Defs::single`]: an unsigned cast
    /// never makes a value larger, so a bound on what the call returned bounds `local`.
    fn returned(&self, body: &MirBody<'_>, mut local: LocalId) -> Option<BasicBlockId> {
        for _ in 0..MAX_STEPS {
            if !is_unsigned(body, local) || !self.single(local) {
                return None;
            }
            match self.slots[local.into_raw().into_u32() as usize].def {
                Def::Call(block) => return Some(block),
                Def::Copy(from) | Def::Cast(from) => local = from,
                Def::None | Def::Other => return None,
            }
        }
        None
    }
}

/// The body's owner as a span names it: an item by its path, never an interned id (rule 25).
fn owner_path(db: &Db, owner: InferBodyId<'_>) -> String {
    match owner {
        InferBodyId::DefWithBodyId(def) => blaze_items::key::path(db, ItemId::of_body(db, def)),
        InferBodyId::AnonConstId(_) => "an anonymous constant".to_owned(), // alloc: a span's name
    }
}
