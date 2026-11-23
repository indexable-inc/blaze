//! [`elaborate`]: one pass, body to body. The output is the input row for row (every local,
//! place, constant, callee, origin, block and loan keeps its id; every statement is copied in
//! order), with only these changes:
//!
//! 1. A `Drop P` whose type needs no drop, or whose place is [`DropState::Uninit`], becomes
//!    `goto` its target.
//! 2. A `Drop P` that is [`DropState::Init`] stays.
//! 3. A `Drop P` that is [`DropState::Maybe`] becomes `switch copy F [0 -> target] else D`, and
//!    `D` clears `F` and drops `P` with the row's target and unwind: on the normal path and on
//!    the cleanup path alike. `F` is `P`'s drop flag: one new `bool` local per place that needs
//!    one, not one per drop.
//! 4. A `Drop P` that is [`DropState::Partial`] becomes the drops of its parts, in field order
//!    (declaration order, a nested part where its field is), each a link of a chain to the row's
//!    target, each guarded by its own flag when it is `Maybe`. In a normal block that may unwind,
//!    part k's unwind edge enters a cleanup block that drops parts k+1.. (each `unwind terminate`)
//!    and then goes where the row's unwind went (its cleanup block, or a new `resume`): the
//!    ladder blaze-build's walk/unwind_ladder.rs builds, in its order. The last part unwinds
//!    where the row did: it owes nothing more.
//! 5. A flag is written where the move dataflow's bit changes (moves.rs `transfer`), so at every
//!    row it equals "initialized on the path taken": false at the entry (true for a place in an
//!    argument); after an assignment, true for each flagged place the destination covers, after
//!    first clearing each one a `move` operand covers; on a call's return edge (a new block
//!    between the call and its target), true for each one the destination covers; before a
//!    terminator, false for each one a `move` operand covers; after `live`/`dead` of a local,
//!    false for each of its places; at any `Drop P`, false for each one `P` covers (a flag that
//!    the drop tests is cleared in its guarded block, after the test).
//!
//! A row this adds carries the span of the row it came from. Nothing is added for a body
//! without a `Maybe` part: no flag, no constant. The pass runs once a body (D119), at generic
//! arguments: a `NeedsDrop::Generic` drop stays for the builder.
#![blaze::sans_io]

use blaze_own::Vec;
use core::alloc::Allocator;
use core::cmp::Ordering;

use blaze_body::body::Mutbl;
use blaze_body::{
    BadRow, Block, BlockKind, Body, BodyBuilder, BuildError, CalleeId, Callee, Const, ConstDecl, ConstKind,
    Local, LocalDecl, LocalKind, Operand, Ops, Origin, Place, Proj, Span, Stmt, StmtKind, Subset, Table,
    Unwind, Value, Verified,
};

use crate::facts::{DropFacts, DropState, Field, NeedsDrop, PartState};

/// Why a body could not be elaborated, naming the row.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ElabError {
    /// A row of the input came out of the copy at another id: the input's tables are not in the
    /// order a builder writes them (a string constant's bytes before an earlier one's, say).
    Copy { table: Table, row: u32 },
    /// The builder refused the output: a bug in this pass, named by blaze-body's first error.
    Build(BuildError),
    /// A drop of a place whose type the body does not record (a projection other than a field:
    /// `Deref`, `DerefBox`, `Index`, `Downcast`, a slice pattern's index or rest).
    PlaceType { stmt: Stmt, place: Place },
    /// A partial drop's part with no field, or one part inside another.
    BadPart { stmt: Stmt },
    /// The entry block is the target of an edge, so the flags' entry values would be written
    /// again on that edge.
    EntryReentered { stmt: Stmt },
    /// A place row the pass made could not be read back.
    Row(BadRow),
}

/// A planted bug, for the tests' control: the checker must catch the elaboration it makes.
#[doc(hidden)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Plant {
    None,
    /// No flag is cleared where its place is moved out of.
    KeepFlagAtMove,
}

/// Elaborates every `Drop` row of `body` from `facts`, writing the output in `alloc`. The output
/// is meant for [`blaze_body::verify`], which the caller runs for its proof.
pub fn elaborate<F: DropFacts, A: Allocator + Copy>(
    body: Verified<'_>,
    facts: &F,
    alloc: A,
) -> Result<Body<A>, ElabError> {
    elaborate_planted(body, facts, alloc, Plant::None)
}

#[doc(hidden)]
pub fn elaborate_planted<F: DropFacts, A: Allocator + Copy>(
    body: Verified<'_>,
    facts: &F,
    alloc: A,
    plant: Plant,
) -> Result<Body<A>, ElabError> {
    let mut e = Elab {
        v: body,
        facts,
        plant,
        b: BodyBuilder::new_in(alloc),
        flags: Vec::new_in(alloc),
        chains: Vec::new_in(alloc),
        plans: Vec::with_capacity_in(body.stmts() as usize, alloc),
        steps: Vec::new_in(alloc),
        pending: Vec::new_in(alloc),
        subsets: Vec::new_in(alloc),
        next_subset: 0,
        ops: Vec::new_in(alloc),
        cases: Vec::new_in(alloc),
        order: Vec::new_in(alloc),
        blocks: Vec::new_in(alloc),
        konst: None,
    };
    e.copy_tables()?;
    e.plan()?;
    e.emit()?;
    e.b.finish().map_err(ElabError::Build)
}

/// A place with a drop flag.
#[derive(Clone, Copy)]
struct Flag {
    place: Place,
    /// The place's local.
    of: Local,
    /// The flag local's own place.
    flag: Place,
    /// `place` and each prefix of it, a range of `Elab::chains`: "`D` covers this flag" is "`D`
    /// is in the chain".
    chain: Ops,
}

/// One drop of a partial drop's chain.
#[derive(Clone, Copy)]
struct Step {
    place: Place,
    flag: Option<u32>,
}

/// What a statement row becomes.
#[derive(Clone, Copy)]
enum Plan {
    NotDrop,
    Remove,
    Keep,
    /// Guarded by this flag.
    Flag(u32),
    /// The steps `start..end`.
    Parts { start: u32, end: u32 },
}

/// A block made by the pass, filled after the input block that made it.
#[derive(Clone, Copy)]
enum Pending {
    /// A drop step (a chain's link or a ladder's rung).
    Drop { block: Block, place: Place, flag: Option<u32>, target: Block, unwind: Unwind, kind: BlockKind, span: Span },
    /// The guarded side of a flag test: clear the flag, drop.
    Guarded { block: Block, flag: u32, place: Place, target: Block, unwind: Unwind, span: Span },
    /// A call's return edge: the flags its destination covers, then the call's target.
    Return { block: Block, dest: Place, target: Block, span: Span },
    /// The end of a ladder that unwinds out of the body.
    Resume { block: Block, span: Span },
}

struct Elab<'v, 'f, F: DropFacts, A: Allocator + Copy> {
    v: Verified<'v>,
    facts: &'f F,
    plant: Plant,
    b: BodyBuilder<A>,
    flags: Vec<Flag, A>,
    chains: Vec<Place, A>,
    /// One per input statement.
    plans: Vec<Plan, A>,
    steps: Vec<Step, A>,
    pending: Vec<Pending, A>,
    /// The input's subsets, in statement order, and the next one to copy.
    subsets: Vec<Subset, A>,
    next_subset: usize,
    /// Scratch rows for one range being copied.
    ops: Vec<Operand, A>,
    cases: Vec<(u128, Block), A>,
    order: Vec<usize, A>,
    blocks: Vec<Block, A>,
    /// The flags' `false` and `true`, made when the first flag is.
    konst: Option<(Const, Const)>,
}

fn path_order(a: &[Field], b: &[Field]) -> Ordering {
    a.iter().map(|f| f.index).cmp(b.iter().map(|f| f.index))
}

impl<F: DropFacts, A: Allocator + Copy> Elab<'_, '_, F, A> {
    /// `got` must be the input's id `want` of `table`: the builder's first error if it has one.
    fn same(&self, table: Table, got: u32, want: u32) -> Result<(), ElabError> {
        if got == want {
            return Ok(());
        }
        Err(match self.b.error() {
            Some(e) => ElabError::Build(e),
            None => ElabError::Copy { table, row: want },
        })
    }

    /// Every table but the statements, in the order the input's rows are: each id the same.
    fn copy_tables(&mut self) -> Result<(), ElabError> {
        let v = self.v;
        let mut next_local = 0u32;
        for i in 0..v.places() {
            let row = v.place(Place(i));
            let got = match (row.parent, row.proj) {
                (None, _) => {
                    self.same(Table::Locals, row.local.0, next_local)?;
                    next_local += 1;
                    let l = self.b.new_local(v.local(row.local));
                    self.b.local_place(l)
                }
                (Some(up), Some(proj)) => self.b.project_marked(up, proj, row.of_drop_type),
                (Some(_), None) => return Err(ElabError::Copy { table: Table::Places, row: i }),
            };
            self.same(Table::Places, got.0, i)?;
        }
        self.same(Table::Locals, next_local, v.locals())?;
        let mut data = 0u64;
        for i in 0..v.consts() {
            let d = v.konst(Const(i));
            let got = match d.kind {
                ConstKind::Bytes { start, len } if u64::from(start) + u64::from(len) <= data => self.b.new_const(d),
                ConstKind::Bytes { start, len } if u64::from(start) == data => {
                    data += u64::from(len);
                    let bytes = v.bytes(Const(i)).ok_or(ElabError::Copy { table: Table::Consts, row: i })?;
                    self.b.new_bytes(bytes, d.ty)
                }
                ConstKind::Bytes { .. } => return Err(ElabError::Copy { table: Table::Data, row: i }),
                _ => self.b.new_const(d),
            };
            self.same(Table::Consts, got.0, i)?;
        }
        for i in 0..v.body().callees() {
            let got = self.b.callee(v.callee(CalleeId(i)));
            self.same(Table::Callees, got.0, i)?;
        }
        for o in 0..v.origins() {
            let got = self.b.origin(v.placeholder(Origin(o)));
            self.same(Table::Origins, got.0, o)?;
        }
        for (from, into) in v.known() {
            self.b.known(from, into);
        }
        for (l, o) in v.local_origins() {
            self.b.mentions(l, o);
        }
        for i in 0..v.blocks() {
            let got = self.b.new_block(v.block(Block(i)).kind);
            self.same(Table::Blocks, got.0, i)?;
        }
        for s in v.subsets() {
            self.subsets.push(s);
        }
        Ok(())
    }

    /// The type of a dropped place, from the body: a local's or a field's.
    fn place_ty(&self, stmt: Stmt, place: Place) -> Result<blaze_body::body::PTy, ElabError> {
        let row = self.v.place(place);
        match row.proj {
            None => Ok(self.v.local(row.local).ty),
            Some(Proj::Field { ty, .. }) => Ok(ty),
            Some(_) => Err(ElabError::PlaceType { stmt, place }),
        }
    }

    /// A plan for each statement: the facts read once, the parts' places and the flags made.
    fn plan(&mut self) -> Result<(), ElabError> {
        let v = self.v;
        for s in 0..v.stmts() {
            let plan = match v.stmt(Stmt(s)).kind {
                StmtKind::Drop { place, .. } => self.plan_drop(Stmt(s), place)?,
                _ => Plan::NotDrop,
            };
            self.plans.push(plan);
        }
        if !self.flags.is_empty() {
            let ty = self.facts.bool_ty();
            let off = self.b.new_const(ConstDecl { kind: ConstKind::Scalar { bits: 0 }, ty });
            let on = self.b.new_const(ConstDecl { kind: ConstKind::Scalar { bits: 1 }, ty });
            self.konst = Some((off, on));
        }
        Ok(())
    }

    fn plan_drop(&mut self, stmt: Stmt, place: Place) -> Result<Plan, ElabError> {
        let facts = self.facts;
        if facts.needs_drop(self.place_ty(stmt, place)?) == NeedsDrop::No {
            return Ok(Plan::Remove);
        }
        let parts = match facts.at(stmt) {
            DropState::Uninit => return Ok(Plan::Remove),
            DropState::Init => return Ok(Plan::Keep),
            DropState::Maybe => return Ok(Plan::Flag(self.flag_for(place)?)),
            DropState::Partial(parts) => parts,
        };
        self.order.clear();
        for (i, part) in parts.iter().enumerate() {
            if part.path.is_empty() {
                return Err(ElabError::BadPart { stmt });
            }
            self.order.push(i);
        }
        self.order.sort_unstable_by(|&a, &b| {
            let pa = parts.get(a).map_or(&[][..], |p| p.path);
            let pb = parts.get(b).map_or(&[][..], |p| p.path);
            path_order(pa, pb)
        });
        for w in self.order.windows(2) {
            let [i, j] = w else { continue };
            let (Some(a), Some(b)) = (parts.get(*i).map(|p| p.path), parts.get(*j).map(|p| p.path)) else {
                return Err(ElabError::BadPart { stmt });
            };
            if let Some(prefix) = b.get(..a.len())
                && path_order(a, prefix) == Ordering::Equal
            {
                return Err(ElabError::BadPart { stmt });
            }
        }
        let start = self.steps.len() as u32;
        for k in 0..self.order.len() {
            let Some(&ord) = self.order.get(k) else { continue };
            let Some(&part) = parts.get(ord) else { return Err(ElabError::BadPart { stmt }) };
            let mut p = place;
            for f in part.path {
                p = self.b.project(p, Proj::Field { index: f.index, ty: f.ty });
            }
            let Some(last) = part.path.last() else { return Err(ElabError::BadPart { stmt }) };
            if facts.needs_drop(last.ty) == NeedsDrop::No {
                continue;
            }
            let flag = match part.state {
                PartState::Init => None,
                PartState::Maybe => Some(self.flag_for(p)?),
            };
            self.steps.push(Step { place: p, flag });
        }
        let end = self.steps.len() as u32;
        Ok(if start == end { Plan::Remove } else { Plan::Parts { start, end } })
    }

    /// The drop flag of `place`, made the first time it is asked for.
    fn flag_for(&mut self, place: Place) -> Result<u32, ElabError> {
        if let Some(i) = self.flags.iter().position(|f| f.place == place) {
            return Ok(i as u32);
        }
        let view = self.b.body().view();
        let of = view.place(place).map_err(ElabError::Row)?.local;
        let start = self.chains.len() as u32;
        let mut p = place;
        loop {
            self.chains.push(p);
            match self.b.body().view().place(p).map_err(ElabError::Row)?.parent {
                Some(up) => p = up,
                None => break,
            }
        }
        let end = self.chains.len() as u32;
        let decl = LocalDecl { ty: self.facts.bool_ty(), kind: LocalKind::DropFlag(of), mutbl: Mutbl::Mut };
        let local = self.b.new_local(decl);
        let flag = self.b.local_place(local);
        self.flags.push(Flag { place, of, flag, chain: Ops { start, end } });
        Ok((self.flags.len() - 1) as u32)
    }

    /// Whether `d` is `place` of flag `i` or a prefix of it.
    fn covers(&self, d: Place, i: usize) -> bool {
        let Some(flag) = self.flags.get(i) else { return false };
        self.chains.get(flag.chain.range()).is_some_and(|c| c.contains(&d))
    }

    /// Writes flag `i`. Only reached with a flag `i`, and `plan` made the constants with the
    /// first flag, so `konst` is set here.
    fn set(&mut self, i: usize, on: bool, span: Span) {
        let Some((off, on_c)) = self.konst else { return };
        let Some(flag) = self.flags.get(i) else { return };
        let c = if on { on_c } else { off };
        self.b.assign(flag.flag, Value::Use(Operand::Const(c)), span);
    }

    /// Clears each flag `p` covers, but the ones `keep` names and the ones the steps test.
    fn clear_under(&mut self, p: Place, steps: Option<(u32, u32)>, keep: Option<u32>, span: Span) {
        for i in 0..self.flags.len() {
            if !self.covers(p, i) || keep == Some(i as u32) {
                continue;
            }
            if let Some((a, b)) = steps
                && self.steps.get(a as usize..b as usize).is_some_and(|s| s.iter().any(|s| s.flag == Some(i as u32)))
            {
                continue;
            }
            self.set(i, false, span);
        }
    }

    fn set_under(&mut self, d: Place, span: Span) {
        for i in 0..self.flags.len() {
            if self.covers(d, i) {
                self.set(i, true, span);
            }
        }
    }

    fn any_under(&self, d: Place) -> bool {
        (0..self.flags.len()).any(|i| self.covers(d, i))
    }

    fn moved(&mut self, o: Operand, span: Span) {
        if self.plant == Plant::KeepFlagAtMove {
            return;
        }
        if let Operand::Move(p) = o {
            self.clear_under(p, None, None, span);
        }
    }

    fn moved_in_value(&mut self, value: Value, span: Span) {
        let v = self.v;
        match value {
            Value::Use(o) | Value::Unary { operand: o, .. } | Value::Cast { operand: o, .. } => self.moved(o, span),
            Value::Binary { lhs, rhs, .. } | Value::CheckedBinary { lhs, rhs, .. } => {
                self.moved(lhs, span);
                self.moved(rhs, span);
            }
            Value::Aggregate { operands, .. } => {
                for o in v.ops(operands) {
                    self.moved(o, span);
                }
            }
            Value::Repeat { operand, .. } => self.moved(operand, span),
            Value::Borrow { .. }
            | Value::RawAddr { .. }
            | Value::Len(_)
            | Value::Discriminant(_)
            | Value::SizeOf(_)
            | Value::AlignOf(_)
            | Value::ThreadLocalAddr(_) => {}
        }
    }

    fn moved_in_terminator(&mut self, kind: StmtKind, span: Span) {
        let v = self.v;
        match kind {
            StmtKind::Call { callee, args, .. } => {
                if let Callee::Value(o) = v.callee(callee) {
                    self.moved(o, span);
                }
                for o in v.ops(args) {
                    self.moved(o, span);
                }
            }
            StmtKind::Switch { on, .. } => self.moved(on, span),
            StmtKind::Assert { cond, msg, .. } => {
                self.moved(cond, span);
                for o in v.ops(msg) {
                    self.moved(o, span);
                }
            }
            StmtKind::InlineAsm { operands, .. } => {
                for o in v.ops(operands) {
                    self.moved(o, span);
                }
            }
            _ => {}
        }
    }

    /// An input range of operands, written again for the output.
    fn reops(&mut self, o: Ops) -> Ops {
        self.ops.clear();
        for x in self.v.ops(o) {
            self.ops.push(x);
        }
        self.b.operands(&self.ops)
    }

    fn recases(&mut self, o: Ops) -> Ops {
        self.cases.clear();
        for x in self.v.cases(o) {
            self.cases.push(x);
        }
        self.b.cases(&self.cases)
    }

    fn subsets_at(&mut self, s: Stmt) {
        while let Some(sub) = self.subsets.get(self.next_subset).copied() {
            if sub.at != s {
                break;
            }
            self.b.subset(sub.from, sub.into);
            self.next_subset += 1;
        }
    }

    fn emit(&mut self) -> Result<(), ElabError> {
        let v = self.v;
        if !self.flags.is_empty() {
            for b in 0..v.blocks() {
                let t = v.terminator(Block(b));
                let mut back = false;
                v.for_each_edge(&v.stmt(t).kind, |to, _| back |= to == Block(0));
                if back {
                    return Err(ElabError::EntryReentered { stmt: t });
                }
            }
        }
        for bi in 0..v.blocks() {
            let block = Block(bi);
            let row = v.block(block);
            self.b.start(block);
            if bi == 0 {
                for i in 0..self.flags.len() {
                    let Some(flag) = self.flags.get(i) else { continue };
                    let l = flag.of.0;
                    self.set(i, l >= 1 && l <= v.args(), Span::default());
                }
            }
            for s in row.stmts.start..row.stmts.end - 1 {
                self.statement(Stmt(s))?;
            }
            self.terminator(Stmt(row.stmts.end - 1), row.kind)?;
            let mut i = 0;
            while let Some(p) = self.pending.get(i).copied() {
                self.fill(p);
                i += 1;
            }
            self.pending.clear();
        }
        Ok(())
    }

    fn statement(&mut self, s: Stmt) -> Result<(), ElabError> {
        let row = self.v.stmt(s);
        let span = row.span;
        match row.kind {
            StmtKind::Assign { dest, value: Value::Borrow { kind, place, loan } } => {
                let origin = self.v.loan(loan).origin;
                let got = self.b.borrow(dest, kind, place, origin, span);
                self.same(Table::Loans, got.0, loan.0)?;
            }
            StmtKind::Assign { dest, value: Value::Aggregate { kind, ty, operands } } => {
                let operands = self.reops(operands);
                self.b.assign(dest, Value::Aggregate { kind, ty, operands }, span);
            }
            other => {
                self.b.push(other, span);
            }
        }
        self.subsets_at(s);
        match row.kind {
            StmtKind::Assign { dest, value } => {
                self.moved_in_value(value, span);
                self.set_under(dest, span);
            }
            StmtKind::StorageLive(l) | StmtKind::StorageDead(l) => {
                for i in 0..self.flags.len() {
                    if self.flags.get(i).is_some_and(|f| f.of == l) {
                        self.set(i, false, span);
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn terminator(&mut self, s: Stmt, kind: BlockKind) -> Result<(), ElabError> {
        let v = self.v;
        let row = v.stmt(s);
        let span = row.span;
        self.moved_in_terminator(row.kind, span);
        match row.kind {
            StmtKind::Drop { place, target, unwind } => self.drop(s, place, target, unwind, kind, span)?,
            StmtKind::Call { callee, args, dest, target, unwind } => {
                let args = self.reops(args);
                let target = match (dest, target) {
                    (Some(d), Some(t)) if self.any_under(d) => {
                        let r = self.b.new_block(v.block(t).kind);
                        self.pending.push(Pending::Return { block: r, dest: d, target: t, span });
                        Some(r)
                    }
                    _ => target,
                };
                self.b.terminate(StmtKind::Call { callee, args, dest, target, unwind }, span);
            }
            StmtKind::Switch { on, cases, otherwise } => {
                let cases = self.recases(cases);
                self.b.terminate(StmtKind::Switch { on, cases, otherwise }, span);
            }
            StmtKind::Assert { cond, expected, kind: k, msg, target, unwind } => {
                let msg = self.reops(msg);
                self.b.terminate(StmtKind::Assert { cond, expected, kind: k, msg, target, unwind }, span);
            }
            StmtKind::InlineAsm { asm, operands, target, unwind } => {
                let operands = self.reops(operands);
                self.b.terminate(StmtKind::InlineAsm { asm, operands, target, unwind }, span);
            }
            other => {
                self.b.terminate(other, span);
            }
        }
        self.subsets_at(s);
        Ok(())
    }

    fn drop(&mut self, s: Stmt, place: Place, target: Block, unwind: Unwind, kind: BlockKind, span: Span) -> Result<(), ElabError> {
        match self.plans.get(s.index()).copied() {
            None | Some(Plan::NotDrop) => return Err(ElabError::Copy { table: Table::Stmts, row: s.0 }),
            Some(Plan::Remove) => {
                self.clear_under(place, None, None, span);
                self.b.terminate(StmtKind::Goto(target), span);
            }
            Some(Plan::Keep) => {
                self.clear_under(place, None, None, span);
                self.b.terminate(StmtKind::Drop { place, target, unwind }, span);
            }
            Some(Plan::Flag(f)) => {
                self.clear_under(place, None, Some(f), span);
                self.drop_one(place, Some(f), target, unwind, kind, span);
            }
            Some(Plan::Parts { start, end }) => {
                self.clear_under(place, Some((start, end)), None, span);
                self.chain(start, end, target, unwind, kind, span);
            }
        }
        Ok(())
    }

    /// Ends the open block with one drop of `place`, behind `flag` when it has one.
    fn drop_one(&mut self, place: Place, flag: Option<u32>, target: Block, unwind: Unwind, kind: BlockKind, span: Span) {
        let Some(f) = flag else {
            self.b.terminate(StmtKind::Drop { place, target, unwind }, span);
            return;
        };
        let Some(flag_row) = self.flags.get(f as usize) else { return };
        let flag_place = flag_row.flag;
        let guarded = self.b.new_block(kind);
        let cases = self.b.cases(&[(0, target)]);
        let on = Operand::Copy(flag_place);
        self.b.terminate(StmtKind::Switch { on, cases, otherwise: guarded }, span);
        self.pending.push(Pending::Guarded { block: guarded, flag: f, place, target, unwind, span });
    }

    /// The drops of steps `start..end` from the open block, then `target`; under a ladder when
    /// a normal block may unwind (rule 4 of the module doc).
    fn chain(&mut self, start: u32, end: u32, target: Block, unwind: Unwind, kind: BlockKind, span: Span) {
        let n = (end - start) as usize;
        let ladder = kind == BlockKind::Normal && n >= 2 && matches!(unwind, Unwind::Continue | Unwind::Cleanup(_));
        // links[k - 1]: where step k (0-based) is dropped on the normal path, for k >= 1.
        self.blocks.clear();
        for _ in 1..n {
            let b = self.b.new_block(kind);
            self.blocks.push(b);
        }
        // rungs[k]: the cleanup block step k unwinds into, which drops steps k+1.. .
        let rungs = self.blocks.len();
        let mut bottom = target;
        if ladder {
            for _ in 1..n {
                let b = self.b.new_block(BlockKind::Cleanup);
                self.blocks.push(b);
            }
            bottom = match unwind {
                Unwind::Cleanup(c) => c,
                _ => self.b.new_block(BlockKind::Cleanup),
            };
        }
        for k in 0..n {
            let Some(&step) = self.steps.get(start as usize + k) else { continue };
            let to = if k + 1 < n { self.blocks.get(k).copied().unwrap_or(target) } else { target };
            let uw = if ladder && k + 1 < n {
                self.blocks.get(rungs + k).copied().map_or(unwind, Unwind::Cleanup)
            } else {
                unwind
            };
            if k == 0 {
                self.drop_one(step.place, step.flag, to, uw, kind, span);
            } else if let Some(&block) = self.blocks.get(k - 1) {
                self.pending.push(Pending::Drop { block, place: step.place, flag: step.flag, target: to, unwind: uw, kind, span });
            }
        }
        if ladder {
            for k in 0..n.saturating_sub(1) {
                let Some(&step) = self.steps.get(start as usize + k + 1) else { continue };
                let Some(&block) = self.blocks.get(rungs + k) else { continue };
                let to = if k + 1 < n - 1 { self.blocks.get(rungs + k + 1).copied().unwrap_or(bottom) } else { bottom };
                self.pending.push(Pending::Drop {
                    block,
                    place: step.place,
                    flag: step.flag,
                    target: to,
                    unwind: Unwind::Terminate,
                    kind: BlockKind::Cleanup,
                    span,
                });
            }
            if unwind == Unwind::Continue {
                self.pending.push(Pending::Resume { block: bottom, span });
            }
        }
    }

    fn fill(&mut self, p: Pending) {
        match p {
            Pending::Drop { block, place, flag, target, unwind, kind, span } => {
                self.b.start(block);
                self.drop_one(place, flag, target, unwind, kind, span);
            }
            Pending::Guarded { block, flag, place, target, unwind, span } => {
                self.b.start(block);
                self.set(flag as usize, false, span);
                self.b.terminate(StmtKind::Drop { place, target, unwind }, span);
            }
            Pending::Return { block, dest, target, span } => {
                self.b.start(block);
                self.set_under(dest, span);
                self.b.terminate(StmtKind::Goto(target), span);
            }
            Pending::Resume { block, span } => {
                self.b.start(block);
                self.b.terminate(StmtKind::Resume, span);
            }
        }
    }
}
