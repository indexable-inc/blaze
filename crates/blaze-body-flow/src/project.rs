//! [`project`]: a verified body to the flow the borrow check takes today, through
//! [`blaze_flow::FlowBuilder`], with [`Projected`]'s map both ways.
//!
//! Each body row's flow form. What a flow does not hold is abstracted as blaze-flow-lower
//! abstracts it (its `bin_done`, `aggregate`, `RawDone`, `call_done`, `FakeUse`, `BranchOn`):
//!
//! ```text
//! body row                                   flow statement(s)
//! d = use O                                  d = O'                 (a constant is `const`)
//! d = borrow K P L 'o                        d = &K P' [L in 'o]    (the loan row's origin)
//! d = raw const|mut P                        d = (copy P')
//! d = len P                                  d = (copy P')
//! d = op(A, B), d = checked op(A, B)         d = (A', B')
//! d = unop(A), d = cast K(A) as T            d = (A')
//! d = repeat(A, cN)                          d = (A')
//! d = tuple|array|adt|closure|coroutine ()   d = const              (no operand: a constant)
//! d = tuple|array|adt|closure|coroutine (..) d = (..')
//! d = size T | align T | tls cN              d = const
//! d = discr P; switch copy|move d [..]       switch P' -> ..        (one flow row for both)
//! live _N, dead _N                           live _N, dead _N
//! activate LN                                activate LN'
//! fakeread P                                 P' = copy P'           (blaze-flow-lower's guard use)
//! deinit P                                   k = move P'            (the value ends: a move out)
//! setdiscr P variant N                       P' = const             (a write to the place)
//! goto B                                     goto B'
//! switch copy|move P [v -> B ..] else E      switch P' -> B' .. E'  (cases in value order, then E)
//! switch const cN [..] else E                k = const; switch k -> ..   (k: the scratch local)
//! call F(args) -> P then B unwind U          P' = call(F?, args'); goto B'   (F: a value callee's operand)
//! call F(args) unwind U  (diverges)          k = call(F?, args'); unreachable
//! drop P then B unwind U                     drop P'; goto B'
//! assert copy|move P == x, .. then B         switch P' -> B'        (a read of the condition)
//! assert const cN == x, .. then B            goto B'
//! suspend resume R cancel C                  suspend -> R', cancel C'
//! return                                     return
//! unreachable, terminate                     unreachable
//! asm N(..) ..                               ProjectError::InlineAsm
//! a cleanup block (all its rows)             left out: `Projected::left_out`
//! ```
//!
//! Unwind edges are not projected: a flow has none ("the rebuilt path aborts on panic"), so
//! every cleanup block, entered only by an unwind edge (`verify`), is unreachable and is left
//! out whole with its rows, its loans and its subsets. Places: `.N:T` is `Field(N)`, `[_N]` is
//! `Index` (the flow's index names no local, so the read of the index local is not an access),
//! `#N` is `Downcast(N)`; a constant index or a subslice is [`ProjectError::SlicePlace`].
//! Origins, placeholders, `mentions` and `known` carry over id for id; a body local `_N` is the
//! flow's `_N`, and the one flow local past them is the scratch local `k`.
#![blaze::sans_io]

use blaze_own::Vec;
use core::alloc::Allocator;
use core::fmt;

use blaze_body as bd;
use blaze_body::{BlockKind, Callee, NONE, Operand, Proj, StmtKind, Value, Verified};
use blaze_flow as fl;
use blaze_flow::{Bound, DropKind, Flow, FlowBuilder, FlowError};

/// A body row with no flow form, or a body the flow cannot hold: named.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProjectError {
    /// An `asm!`: its operands' places and borrows have no flow form (blaze-flow-lower refuses
    /// it too, `Form::InlineAsm`).
    InlineAsm { stmt: bd::Stmt },
    /// A place through a slice pattern's constant index or subslice: the flow's `Index` overlaps
    /// every index and refuses a move out, where Rust allows a move out of an array's constant
    /// index (blaze-flow-lower refuses these patterns, `Form::SlicePatternBinding`).
    SlicePlace { place: bd::Place },
    /// A discriminant read that is not the operand of the switch on the next row: a flow reads a
    /// discriminant only as a switch on the place.
    LoneDiscriminant { stmt: bd::Stmt },
    /// Local `local`, in `1..=args`, is not the argument at its index: a flow takes `1..=args`
    /// as its arguments, in order.
    ArgOrder { local: bd::Local },
    /// A borrow whose loan row is not this borrow's (another place, kind or statement).
    LoanRow { stmt: bd::Stmt, loan: bd::Loan },
    /// An activation of a loan no projected borrow issues (one of a left-out cleanup block).
    LoanLeftOut { stmt: bd::Stmt, loan: bd::Loan },
    /// A subset out of statement order (the body's promise), at this statement.
    SubsetOrder { at: bd::Stmt },
    /// A normal edge into a cleanup block, which is left out (verify refuses one).
    EdgeIntoCleanup { stmt: bd::Stmt, block: bd::Block },
    /// The flow builder refused what was projected.
    Flow(FlowError),
}

impl fmt::Display for ProjectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            ProjectError::InlineAsm { stmt } => write!(f, "{stmt}: an `asm!` has no flow form"),
            ProjectError::SlicePlace { place } => {
                write!(f, "{place}: a constant index or subslice of a slice pattern has no flow form")
            }
            ProjectError::LoneDiscriminant { stmt } => {
                write!(f, "{stmt}: a discriminant read that the next row does not switch on")
            }
            ProjectError::ArgOrder { local } => write!(f, "{local}: not the argument at its index"),
            ProjectError::LoanRow { stmt, loan } => write!(f, "{stmt}: loan {loan}'s row is not this borrow's"),
            ProjectError::LoanLeftOut { stmt, loan } => {
                write!(f, "{stmt}: activates {loan}, issued only in a cleanup block")
            }
            ProjectError::SubsetOrder { at } => write!(f, "a subset at {at} is out of statement order"),
            ProjectError::EdgeIntoCleanup { stmt, block } => {
                write!(f, "{stmt}: a normal edge into cleanup block {block}")
            }
            ProjectError::Flow(e) => write!(f, "the flow builder refused the projection: {e:?}"),
        }
    }
}

/// A body's flow and the map both ways between their ids.
pub struct Projected<A: Allocator + Copy> {
    pub flow: Flow<A>,
    /// Per body statement: its flow statement (the one a finding there names), or [`NONE`].
    flow_stmt: Vec<u32, A>,
    /// Per flow statement: the body row it was made for.
    body_stmt: Vec<u32, A>,
    flow_block: Vec<u32, A>,
    body_block: Vec<u32, A>,
    flow_place: Vec<u32, A>,
    /// Per flow place: the first body place projected to it, or [`NONE`] (the scratch local's).
    body_place: Vec<u32, A>,
    flow_loan: Vec<u32, A>,
    body_loan: Vec<u32, A>,
    /// The body's locals: flow local `l` below this is body local `l`.
    locals: u32,
    scratch: Option<fl::Local>,
    left_out: Vec<bd::Block, A>,
    /// Each projected body `Drop`: its row, its flow statement and the flow place it drops.
    drops: Vec<(bd::Stmt, fl::Stmt, fl::Place), A>,
}

fn at(v: &[u32], i: usize) -> Option<u32> {
    match v.get(i) {
        Some(&x) if x != NONE => Some(x),
        _ => None,
    }
}

impl<A: Allocator + Copy> Projected<A> {
    /// The flow statement body row `s` became (a finding at it names this one); `None` for a
    /// row of a left-out cleanup block.
    pub fn flow_stmt(&self, s: bd::Stmt) -> Option<fl::Stmt> {
        at(&self.flow_stmt, s.index()).map(fl::Stmt)
    }

    /// The body row flow statement `s` was made for (a `goto` after a call, the call's row).
    pub fn body_stmt(&self, s: fl::Stmt) -> Option<bd::Stmt> {
        at(&self.body_stmt, s.index()).map(bd::Stmt)
    }

    pub fn flow_block(&self, b: bd::Block) -> Option<fl::Block> {
        at(&self.flow_block, b.index()).map(fl::Block)
    }

    pub fn body_block(&self, b: fl::Block) -> Option<bd::Block> {
        at(&self.body_block, b.index()).map(bd::Block)
    }

    pub fn flow_place(&self, p: bd::Place) -> Option<fl::Place> {
        at(&self.flow_place, p.index()).map(fl::Place)
    }

    /// The first body place projected to flow place `p` (two body indices `[_1]`, `[_2]` of one
    /// parent are one flow place); `None` for the scratch local's.
    pub fn body_place(&self, p: fl::Place) -> Option<bd::Place> {
        at(&self.body_place, p.index()).map(bd::Place)
    }

    pub fn flow_local(&self, l: bd::Local) -> fl::Local {
        fl::Local(l.0)
    }

    /// The body local of flow local `l`; `None` for the scratch local.
    pub fn body_local(&self, l: fl::Local) -> Option<bd::Local> {
        (l.0 < self.locals).then_some(bd::Local(l.0))
    }

    pub fn flow_loan(&self, l: bd::Loan) -> Option<fl::Loan> {
        at(&self.flow_loan, l.index()).map(fl::Loan)
    }

    pub fn body_loan(&self, l: fl::Loan) -> Option<bd::Loan> {
        at(&self.body_loan, l.index()).map(bd::Loan)
    }

    /// The flow local a diverging call writes and a switch on a constant reads, if any.
    pub fn scratch(&self) -> Option<fl::Local> {
        self.scratch
    }

    /// The cleanup blocks, left out with their rows (a flow has no unwind edge).
    pub fn left_out(&self) -> &[bd::Block] {
        &self.left_out
    }

    /// What drop elaboration does at each projected body `Drop`, in row order, from the move
    /// check's two dataflows (`blaze_flow::moves::drop_kind`).
    pub fn drop_kinds(&self, alloc: A) -> Result<Vec<(bd::Stmt, DropKind), A>, Bound> {
        let st = fl::moves::inits(&self.flow, fl::record::Plant::None, alloc)?;
        let mut out = Vec::with_capacity_in(self.drops.len(), alloc);
        for &(row, s, p) in self.drops.iter() {
            out.push((row, fl::moves::drop_kind(&self.flow, &st, s, p)));
        }
        Ok(out)
    }
}

fn filled<A: Allocator>(n: usize, alloc: A) -> Vec<u32, A> {
    let mut v = Vec::with_capacity_in(n, alloc);
    v.resize(n, NONE);
    v
}

fn borrow_kind(k: bd::BorrowKind) -> fl::BorrowKind {
    match k {
        bd::BorrowKind::Shared => fl::BorrowKind::Shared,
        bd::BorrowKind::Mut => fl::BorrowKind::Mut,
        bd::BorrowKind::TwoPhase => fl::BorrowKind::TwoPhase,
        bd::BorrowKind::Shallow => fl::BorrowKind::Shallow,
    }
}

fn span(s: bd::Span) -> fl::Span {
    fl::Span { lo: s.lo, hi: s.hi }
}

/// Projects `body` to its flow (the module doc gives each row's form).
pub fn project<A: Allocator + Copy>(body: Verified<'_>, alloc: A) -> Result<Projected<A>, ProjectError> {
    let locals = body.locals();
    let args = body.args();
    for i in 1..=args {
        match body.local(bd::Local(i)).kind {
            bd::LocalKind::Arg(k) if k.checked_add(1) == Some(i) => {}
            _ => return Err(ProjectError::ArgOrder { local: bd::Local(i) }),
        }
    }
    let mut b = FlowBuilder::new_in(locals, args, alloc);
    for o in 0..body.origins() {
        b.origin(body.placeholder(bd::Origin(o)));
    }
    for (l, o) in body.local_origins() {
        b.mentions(l.0, fl::Origin(o.0));
    }
    for (from, into) in body.known() {
        b.known(fl::Origin(from.0), fl::Origin(into.0));
    }

    // Places in row order: a parent is an earlier row (verify), so it is mapped first.
    let mut flow_place = filled(body.places() as usize, alloc);
    for i in 0..body.places() {
        let row = body.place(bd::Place(i));
        let p = match (row.parent, row.proj) {
            (Some(up), Some(proj)) => {
                let parent = fl::Place(flow_place.get(up.index()).copied().unwrap_or(NONE));
                let proj = match proj {
                    Proj::Field { index, .. } => fl::Proj::Field(index),
                    Proj::Deref => fl::Proj::Deref,
                    Proj::DerefBox => fl::Proj::DerefBox,
                    Proj::Index(_) => fl::Proj::Index,
                    Proj::Downcast(v) => fl::Proj::Downcast(v),
                    Proj::ConstIndex { .. } | Proj::Subslice { .. } => {
                        return Err(ProjectError::SlicePlace { place: bd::Place(i) });
                    }
                };
                b.project_marked(parent, proj, row.of_drop_type)
            }
            _ => b.local(row.local.0),
        };
        if let Some(slot) = flow_place.get_mut(i as usize) {
            *slot = p.0;
        }
    }

    // Blocks: every normal block in id order, so the body's entry is the flow's.
    let mut flow_block = filled(body.blocks() as usize, alloc);
    let mut body_block = Vec::new_in(alloc);
    let mut left_out = Vec::new_in(alloc);
    let mut cleanup = Vec::with_capacity_in(body.blocks() as usize, alloc);
    for x in 0..body.blocks() {
        let kind = body.block(bd::Block(x)).kind;
        cleanup.push(kind == BlockKind::Cleanup);
        match kind {
            BlockKind::Normal => {
                if let Some(slot) = flow_block.get_mut(x as usize) {
                    *slot = b.block().0;
                }
                body_block.push(x);
            }
            BlockKind::Cleanup => left_out.push(bd::Block(x)),
        }
    }

    // Loans: the flow numbers them as the borrows are pushed, which is row order.
    let mut flow_loan = filled(body.loans() as usize, alloc);
    let mut body_loan = Vec::new_in(alloc);
    for s in 0..body.stmts() {
        if cleanup.get(body.block_of(bd::Stmt(s)).index()).copied().unwrap_or(false) {
            continue;
        }
        if let StmtKind::Assign { value: Value::Borrow { loan, .. }, .. } = body.stmt(bd::Stmt(s)).kind {
            match flow_loan.get_mut(loan.index()) {
                Some(slot) if *slot == NONE => {
                    *slot = body_loan.len() as u32;
                    body_loan.push(loan.0);
                }
                _ => return Err(ProjectError::LoanRow { stmt: bd::Stmt(s), loan }),
            }
        }
    }

    let mut subs = Vec::new_in(alloc);
    subs.extend(body.subsets());
    let mut e = Emit {
        body,
        b,
        cleanup,
        flow_place,
        flow_block,
        flow_loan,
        flow_stmt: filled(body.stmts() as usize, alloc),
        body_stmt: Vec::new_in(alloc),
        ops: Vec::new_in(alloc),
        targets: Vec::new_in(alloc),
        subs,
        sub_at: 0,
        scratch: None,
        drops: Vec::new_in(alloc),
    };
    let n = body.stmts();
    let mut s = 0;
    while s < n {
        let row = bd::Stmt(s);
        e.skip_subsets(row)?;
        let blk = body.block_of(row);
        if e.cleanup.get(blk.index()).copied().unwrap_or(false) {
            s += 1;
            continue;
        }
        if body.block(blk).stmts.start == s
            && let Some(&fb) = e.flow_block.get(blk.index())
        {
            e.b.start(fl::Block(fb));
        }
        s += e.row(row)?;
    }
    e.skip_subsets(bd::Stmt(n))?;

    let flow = e.b.finish().map_err(ProjectError::Flow)?;
    let mut body_place = filled(flow.places.len(), alloc);
    for (i, &p) in e.flow_place.iter().enumerate() {
        if let Some(slot) = body_place.get_mut(p as usize)
            && *slot == NONE
        {
            *slot = i as u32;
        }
    }
    Ok(Projected {
        flow,
        flow_stmt: e.flow_stmt,
        body_stmt: e.body_stmt,
        flow_block: e.flow_block,
        body_block,
        flow_place: e.flow_place,
        body_place,
        flow_loan: e.flow_loan,
        body_loan,
        locals,
        scratch: e.scratch,
        left_out,
        drops: e.drops,
    })
}

/// The projection's state: the builder and the maps as they fill.
struct Emit<'v, A: Allocator + Copy> {
    body: Verified<'v>,
    b: FlowBuilder<A>,
    /// Per body block: whether it is a cleanup block (left out).
    cleanup: Vec<bool, A>,
    flow_place: Vec<u32, A>,
    flow_block: Vec<u32, A>,
    flow_loan: Vec<u32, A>,
    flow_stmt: Vec<u32, A>,
    body_stmt: Vec<u32, A>,
    /// Scratch for one row's operands and one switch's targets.
    ops: Vec<fl::Operand, A>,
    targets: Vec<fl::Block, A>,
    subs: Vec<bd::Subset, A>,
    sub_at: usize,
    scratch: Option<fl::Local>,
    drops: Vec<(bd::Stmt, fl::Stmt, fl::Place), A>,
}

impl<A: Allocator + Copy> Emit<'_, A> {
    fn place(&self, p: bd::Place) -> fl::Place {
        fl::Place(self.flow_place.get(p.index()).copied().unwrap_or(NONE))
    }

    fn op(&self, o: Operand) -> fl::Operand {
        match o {
            Operand::Copy(p) => fl::Operand::Copy(self.place(p)),
            Operand::Move(p) => fl::Operand::Move(self.place(p)),
            Operand::Const(_) => fl::Operand::Const,
        }
    }

    fn block(&self, row: bd::Stmt, t: bd::Block) -> Result<fl::Block, ProjectError> {
        match at(&self.flow_block, t.index()) {
            Some(x) => Ok(fl::Block(x)),
            None => Err(ProjectError::EdgeIntoCleanup { stmt: row, block: t }),
        }
    }

    /// The scratch local's place, made at its first use.
    fn scratch(&mut self) -> fl::Place {
        let l = match self.scratch {
            Some(l) => l,
            None => {
                let l = self.b.add_local();
                self.scratch = Some(l);
                l
            }
        };
        self.b.local(l.0)
    }

    /// Records that the next flow statement is row `row`'s own: the one its findings name.
    fn primary(&mut self, row: bd::Stmt) -> fl::Stmt {
        let s = self.body_stmt.len() as u32;
        if let Some(slot) = self.flow_stmt.get_mut(row.index()) {
            *slot = s;
        }
        self.body_stmt.push(row.0);
        fl::Stmt(s)
    }

    /// Records that the next flow statement was made for row `row`, beside its own.
    fn extra(&mut self, row: bd::Stmt) {
        self.body_stmt.push(row.0);
    }

    /// Passes the subsets before `row`: each must be at a row of a left-out block (the rows
    /// projected took theirs).
    fn skip_subsets(&mut self, row: bd::Stmt) -> Result<(), ProjectError> {
        while let Some(&sub) = self.subs.get(self.sub_at) {
            if sub.at >= row {
                break;
            }
            if !self.cleanup.get(self.body.block_of(sub.at).index()).copied().unwrap_or(false) {
                return Err(ProjectError::SubsetOrder { at: sub.at });
            }
            self.sub_at += 1;
        }
        Ok(())
    }

    /// Row `row`'s subsets, at the Mid of the flow statement pushed last (its own).
    fn subsets_at(&mut self, row: bd::Stmt) {
        while let Some(&sub) = self.subs.get(self.sub_at) {
            if sub.at != row {
                break;
            }
            self.b.subset(fl::Origin(sub.from.0), fl::Origin(sub.into.0));
            self.sub_at += 1;
        }
    }

    /// Projects row `row` (and the switch after a discriminant read); returns the rows taken.
    fn row(&mut self, row: bd::Stmt) -> Result<u32, ProjectError> {
        let body = self.body;
        let r = body.stmt(row);
        let sp = span(r.span);
        match r.kind {
            StmtKind::Assign { dest, value } => return self.assign(row, dest, value, sp),
            StmtKind::StorageLive(l) => {
                self.primary(row);
                self.b.stmt(fl::StmtKind::StorageLive(fl::Local(l.0)), sp);
            }
            StmtKind::StorageDead(l) => {
                self.primary(row);
                self.b.stmt(fl::StmtKind::StorageDead(fl::Local(l.0)), sp);
            }
            StmtKind::Activate(loan) => {
                let Some(x) = at(&self.flow_loan, loan.index()) else {
                    return Err(ProjectError::LoanLeftOut { stmt: row, loan });
                };
                self.primary(row);
                self.b.stmt(fl::StmtKind::Activate(fl::Loan(x)), sp);
            }
            StmtKind::FakeRead(p) => {
                let q = self.place(p);
                self.primary(row);
                self.b.assign(q, fl::Operand::Copy(q), sp);
            }
            // The place's value ends: a move out of the whole place, into the scratch local.
            StmtKind::Deinit(p) => {
                let q = self.place(p);
                let k = self.scratch();
                self.primary(row);
                self.b.assign(k, fl::Operand::Move(q), sp);
            }
            // The tag is written: a write to the place, of a value the flow does not hold.
            StmtKind::SetDiscriminant { place, .. } => {
                let q = self.place(place);
                self.primary(row);
                self.b.assign(q, fl::Operand::Const, sp);
            }
            StmtKind::Goto(t) => {
                let t = self.block(row, t)?;
                self.primary(row);
                self.b.goto(t, sp);
            }
            StmtKind::Switch { on, cases, otherwise } => {
                let on = match on {
                    Operand::Copy(p) | Operand::Move(p) => self.place(p),
                    Operand::Const(_) => {
                        let k = self.scratch();
                        self.extra(row);
                        self.b.assign(k, fl::Operand::Const, sp);
                        k
                    }
                };
                self.switch(row, on, cases, otherwise, sp)?;
            }
            StmtKind::Call { callee, args, dest, target, .. } => {
                self.ops.clear();
                if let Callee::Value(o) = body.callee(callee) {
                    let o = self.op(o);
                    self.ops.push(o);
                }
                for o in body.ops(args) {
                    let o = self.op(o);
                    self.ops.push(o);
                }
                match (dest, target) {
                    (Some(d), Some(t)) => {
                        let (d, t) = (self.place(d), self.block(row, t)?);
                        self.primary(row);
                        self.b.call(&self.ops, d, sp);
                        self.subsets_at(row);
                        self.extra(row);
                        self.b.goto(t, sp);
                    }
                    // A diverging call (verify: no destination and no target) writes nothing a
                    // later row reads, and nothing follows it.
                    _ => {
                        let k = self.scratch();
                        self.primary(row);
                        self.b.call(&self.ops, k, sp);
                        self.subsets_at(row);
                        self.extra(row);
                        self.b.stmt(fl::StmtKind::Unreachable, sp);
                    }
                }
                return Ok(1);
            }
            StmtKind::Drop { place, target, .. } => {
                let (p, t) = (self.place(place), self.block(row, target)?);
                let s = self.primary(row);
                self.b.stmt(fl::StmtKind::Drop(p), sp);
                self.drops.push((row, s, p));
                self.subsets_at(row);
                self.extra(row);
                self.b.goto(t, sp);
                return Ok(1);
            }
            StmtKind::Assert { cond, target, .. } => {
                let t = self.block(row, target)?;
                self.primary(row);
                match cond {
                    Operand::Copy(p) | Operand::Move(p) => {
                        let p = self.place(p);
                        self.b.switch(p, &[t], sp);
                    }
                    Operand::Const(_) => {
                        self.b.goto(t, sp);
                    }
                }
            }
            StmtKind::Suspend { resume, cancel } => {
                let (resume, cancel) = (self.block(row, resume)?, self.block(row, cancel)?);
                self.primary(row);
                self.b.stmt(fl::StmtKind::Suspend { resume, cancel }, sp);
            }
            StmtKind::Return => {
                self.primary(row);
                self.b.ret(sp);
            }
            // No successor and nothing read: the process aborts, or (verify keeps `resume` in
            // cleanup blocks, left out) unwinding goes on.
            StmtKind::Unreachable | StmtKind::Terminate | StmtKind::Resume => {
                self.primary(row);
                self.b.stmt(fl::StmtKind::Unreachable, sp);
            }
            StmtKind::InlineAsm { .. } => return Err(ProjectError::InlineAsm { stmt: row }),
        }
        self.subsets_at(row);
        Ok(1)
    }

    /// The switch of row `row` on flow place `on`: its cases' blocks in value order, then
    /// `otherwise`. The caller has recorded any row made before it.
    fn switch(
        &mut self,
        row: bd::Stmt,
        on: fl::Place,
        cases: bd::Ops,
        otherwise: bd::Block,
        sp: fl::Span,
    ) -> Result<(), ProjectError> {
        self.targets.clear();
        for (_, t) in self.body.cases(cases) {
            let t = self.block(row, t)?;
            self.targets.push(t);
        }
        let o = self.block(row, otherwise)?;
        self.targets.push(o);
        self.primary(row);
        self.b.switch(on, &self.targets, sp);
        Ok(())
    }

    fn assign(&mut self, row: bd::Stmt, dest: bd::Place, value: Value, sp: fl::Span) -> Result<u32, ProjectError> {
        let body = self.body;
        let d = self.place(dest);
        self.ops.clear();
        match value {
            Value::Use(o) => {
                let o = self.op(o);
                self.primary(row);
                self.b.assign(d, o, sp);
            }
            Value::Borrow { kind, place, loan } => {
                let lr = body.loan(loan);
                if lr.place != place || lr.kind != kind || lr.at != row {
                    return Err(ProjectError::LoanRow { stmt: row, loan });
                }
                let p = self.place(place);
                self.primary(row);
                let got = self.b.borrow(d, borrow_kind(kind), p, fl::Origin(lr.origin.0), sp);
                if Some(got.0) != at(&self.flow_loan, loan.index()) {
                    return Err(ProjectError::LoanRow { stmt: row, loan });
                }
            }
            Value::RawAddr { place, .. } | Value::Len(place) => {
                let p = self.place(place);
                self.ops.push(fl::Operand::Copy(p));
                self.primary(row);
                self.b.aggregate(d, &self.ops, sp);
            }
            Value::Binary { lhs, rhs, .. } | Value::CheckedBinary { lhs, rhs, .. } => {
                let (l, r) = (self.op(lhs), self.op(rhs));
                self.ops.push(l);
                self.ops.push(r);
                self.primary(row);
                self.b.aggregate(d, &self.ops, sp);
            }
            Value::Unary { operand, .. } | Value::Cast { operand, .. } | Value::Repeat { operand, .. } => {
                let o = self.op(operand);
                self.ops.push(o);
                self.primary(row);
                self.b.aggregate(d, &self.ops, sp);
            }
            Value::Aggregate { operands, .. } => {
                for o in body.ops(operands) {
                    let o = self.op(o);
                    self.ops.push(o);
                }
                self.primary(row);
                // blaze-flow-lower's `aggregate`: no operand is a constant value.
                if self.ops.is_empty() {
                    self.b.assign(d, fl::Operand::Const, sp);
                } else {
                    self.b.aggregate(d, &self.ops, sp);
                }
            }
            Value::SizeOf(_) | Value::AlignOf(_) | Value::ThreadLocalAddr(_) => {
                self.primary(row);
                self.b.assign(d, fl::Operand::Const, sp);
            }
            Value::Discriminant(p) => {
                // `d = discr p; switch d`: blaze-flow-lower's switch on the matched place, whose
                // read is the shallow one of a switch. Both rows are the one flow statement.
                // An assignment is never a block's last row (verify), so the next row exists
                // and is in this block.
                let next = bd::Stmt(row.0 + 1);
                let StmtKind::Switch { on: Operand::Copy(x) | Operand::Move(x), cases, otherwise } =
                    body.stmt(next).kind
                else {
                    return Err(ProjectError::LoneDiscriminant { stmt: row });
                };
                if x != dest {
                    return Err(ProjectError::LoneDiscriminant { stmt: row });
                }
                let on = self.place(p);
                self.switch(row, on, cases, otherwise, sp)?;
                self.subsets_at(row);
                if let Some(&mapped) = self.flow_stmt.get(row.index())
                    && let Some(slot) = self.flow_stmt.get_mut(next.index())
                {
                    *slot = mapped;
                }
                self.subsets_at(next);
                return Ok(2);
            }
        }
        self.subsets_at(row);
        Ok(1)
    }
}
