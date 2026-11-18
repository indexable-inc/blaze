//! The flow's forms (flow.md section 1): typed ids, the body as flat tables, a builder for
//! hand-made flows and a printer with a stable text form.
#![blaze::sans_io]

use blaze_own::Vec;
use core::alloc::Allocator;
use core::fmt;

macro_rules! ids {
    ($($(#[$m:meta])* $name:ident = $tag:literal;)*) => {$(
        $(#[$m])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
        pub struct $name(pub u32);
        impl $name {
            pub fn index(self) -> usize {
                self.0 as usize
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!($tag, "{}"), self.0)
            }
        }
    )*};
}

ids! {
    /// A local of the body; `Local(0)` is the return place, `1..=args` the arguments.
    Local = "_";
    /// A row of the place table: a local, or a parent place and one projection.
    Place = "p";
    /// A row of the statement array (terminators included).
    Stmt = "s";
    /// A block: a range of the statement array whose last row is its terminator.
    Block = "bb";
    /// A loan: one borrow statement's borrow.
    Loan = "L";
    /// An origin: a set of loans a reference may hold (Polonius' "origin").
    Origin = "'o";
}

/// A point: a statement's Start (accesses, kills) or its Mid (issues, `subset_base`).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Point(pub u32);

impl Point {
    pub fn start(s: Stmt) -> Point {
        Point(s.0 * 2)
    }
    pub fn mid(s: Stmt) -> Point {
        Point(s.0 * 2 + 1)
    }
    pub fn stmt(self) -> Stmt {
        Stmt(self.0 / 2)
    }
    pub fn is_mid(self) -> bool {
        self.0 % 2 == 1
    }
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

impl fmt::Display for Point {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let half = if self.is_mid() { "mid" } else { "start" };
        write!(f, "{}.{half}", self.stmt())
    }
}

/// An item-relative byte range: where in its item a statement came from (rule 71).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct Span {
    pub lo: u32,
    pub hi: u32,
}

/// One projection of a place.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Proj {
    Field(u32),
    /// Through a reference or a raw pointer: not owned.
    Deref,
    /// Through a `Box`: owned, so moves through it are allowed (the Reference).
    DerefBox,
    /// Any index: two indices may overlap.
    Index,
    /// rustc's `ConstantIndex`: the element `offset` from the start (`from_end`: from the end,
    /// 1 the last) of a value at least `min_length` long.
    ConstantIndex { offset: u32, min_length: u32, from_end: bool },
    /// rustc's `Subslice`: elements `from..to` (`from_end`: `from..len - to`).
    Subslice { from: u32, to: u32, from_end: bool },
    Downcast(u32),
}

/// A row of the place table.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PlaceRow {
    pub local: Local,
    pub parent: Option<Place>,
    pub proj: Option<Proj>,
    /// Projections from the local: 0 for the local itself.
    pub depth: u32,
    /// A `Field` of a type that implements `Drop`: not movable out of (the Reference).
    pub of_drop_type: bool,
    /// An element or subslice of an UNSIZED `[T]` (a slice pattern under a reference or a
    /// `Box<[T]>`): not movable out of (E0508, "cannot move out of type `[T]`, a non-copy
    /// slice"), where the same projection of an array `[T; N]` is. Its own fact, never read
    /// from [`Self::of_drop_type`] (lane B12 item 6).
    pub of_slice: bool,
}

/// What a place made by [`FlowBuilder::project_in`] is marked as: one fact per row, the
/// reason a move out of it is refused (moves.rs `move_out`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PlaceMark {
    None,
    /// [`PlaceRow::of_drop_type`].
    DropField,
    /// [`PlaceRow::of_slice`].
    SliceElement,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Operand {
    Copy(Place),
    Move(Place),
    Const,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum BorrowKind {
    Shared,
    Mut,
    /// Reserved (acts as shared) until its `Activate` (RFC 2025).
    TwoPhase,
    /// A guard's fake borrow: shallow, conflicts only with writes.
    Shallow,
}

/// A range of the operand array.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Ops {
    pub start: u32,
    pub end: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Value {
    Use(Operand),
    Borrow { kind: BorrowKind, place: Place, loan: Loan },
    Aggregate(Ops),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StmtKind {
    Assign { dest: Place, value: Value },
    /// No unwind edge: the rebuilt path aborts on panic (flow.md section 1).
    Call { args: Ops, dest: Place },
    StorageLive(Local),
    StorageDead(Local),
    Drop(Place),
    Activate(Loan),
    Goto(Block),
    /// A switch on a place's discriminant; targets are a range of the successor array.
    Switch { on: Place, targets: Ops },
    Return,
    Unreachable,
    /// An `.await`'s suspension: polled again, go on at `resume`; dropped while parked here, go
    /// to `cancel` (coroutines.md D2). A terminator.
    Suspend { resume: Block, cancel: Block },
}

impl StmtKind {
    pub fn is_terminator(self) -> bool {
        matches!(
            self,
            StmtKind::Goto(_)
                | StmtKind::Switch { .. }
                | StmtKind::Return
                | StmtKind::Unreachable
                | StmtKind::Suspend { .. }
        )
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StmtRow {
    pub kind: StmtKind,
    pub span: Span,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LoanRow {
    pub place: Place,
    pub kind: BorrowKind,
    pub origin: Origin,
    pub at: Stmt,
}

/// A dataflow exceeded its convergence bound or tier two exhausted its work budget.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Bound<N = ()> {
    Passes { what: &'static str, limit: u64 },
    TierTwo { function: N, points: usize, origins: usize, loans: usize, budget: u64 },
}

impl<F> Bound<F> {
    /// The flow tables are anonymous. The driver owns the function's spelling and
    /// can attach it without copying, truncating, or interning diagnostic text.
    pub fn in_function<N>(self, name: N) -> Bound<N> {
        match self {
            Self::Passes { what, limit } => Bound::Passes { what, limit },
            Self::TierTwo { points, origins, loans, budget, .. } =>
                Bound::TierTwo { function: name, points, origins, loans, budget },
        }
    }
}

impl<N: fmt::Display> fmt::Display for Bound<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Passes { what, limit } => write!(f, "{what} exceeded the limit of {limit}"),
            Self::TierTwo { function, points, origins, loans, budget } => write!(
                f, "tier two exceeded {budget} steps in function `{function}` ({points} points, {origins} origins, {loans} loans)"
            ),
        }
    }
}

/// A malformed hand-built flow.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FlowError {
    /// A block was reserved and never filled.
    UnfilledBlock(Block),
    /// A statement was added with no block started, or a block started inside another.
    NotInBlock,
    /// A block was left without a terminator.
    Unterminated(Block),
    /// The body has no block.
    Empty,
}

/// One body's flow: every table flat, every id an index into one of them.
pub struct Flow<A: Allocator> {
    pub locals: u32,
    pub args: u32,
    pub places: Vec<PlaceRow, A>,
    pub stmts: Vec<StmtRow, A>,
    /// Block `b` is `stmts[blocks[b].start..blocks[b].end]`; the last row is its terminator.
    pub blocks: Vec<Ops, A>,
    pub stmt_block: Vec<Block, A>,
    pub operands: Vec<Operand, A>,
    pub succ: Vec<Block, A>,
    pub loans: Vec<LoanRow, A>,
    /// `subset_base(from, into, Mid(stmt))`: the facts a statement makes (an assignment's,
    /// a reborrow's or a call's constraints), in statement order.
    pub subsets: Vec<(Stmt, Origin, Origin), A>,
    pub origins: u32,
    pub placeholder: Vec<bool, A>,
    /// `(local, origin)`: the local's type mentions the origin. Sorted.
    pub local_origins: Vec<(Local, Origin), A>,
    /// Known `from: into` between placeholders (the signature's bounds).
    pub known: Vec<(Origin, Origin), A>,
    /// Each local's own place: `Place(l)` for the locals the builder was made with, a later row
    /// for a local added while lowering ([`FlowBuilder::add_local`]).
    pub local_places: Vec<Place, A>,
}

impl<A: Allocator> Flow<A> {
    pub fn place(&self, p: Place) -> PlaceRow {
        self.places[p.index()]
    }

    /// The place that is local `l` itself.
    pub fn local_place(&self, l: Local) -> Place {
        self.local_places[l.index()]
    }

    pub fn stmt(&self, s: Stmt) -> StmtRow {
        self.stmts[s.index()]
    }

    pub fn loan(&self, l: Loan) -> LoanRow {
        self.loans[l.index()]
    }

    pub fn ops(&self, o: Ops) -> &[Operand] {
        &self.operands[o.start as usize..o.end as usize]
    }

    /// The prefix of `p` at `depth` (0 is the local's place).
    pub fn ancestor(&self, mut p: Place, depth: u32) -> Place {
        while self.place(p).depth > depth {
            p = self.place(p).parent.unwrap_or(p);
        }
        p
    }

    /// `a` is `b` or a prefix of `b`.
    pub fn is_prefix(&self, a: Place, b: Place) -> bool {
        let (ra, rb) = (self.place(a), self.place(b));
        ra.local == rb.local && ra.depth <= rb.depth && self.ancestor(b, ra.depth) == a
    }

    /// Whether the projections of `p` below depth `from` include a `Deref`.
    pub fn deref_below(&self, mut p: Place, from: u32) -> bool {
        while self.place(p).depth > from {
            if self.place(p).proj == Some(Proj::Deref) {
                return true;
            }
            p = self.place(p).parent.unwrap_or(p);
        }
        false
    }

    pub fn entry(&self) -> Stmt {
        Stmt(self.blocks[0].start)
    }

    /// Calls `f` on each successor statement of `s`.
    pub fn for_each_succ(&self, s: Stmt, mut f: impl FnMut(Stmt)) {
        match self.stmt(s).kind {
            StmtKind::Goto(b) => f(Stmt(self.blocks[b.index()].start)),
            StmtKind::Switch { targets, .. } => {
                for b in &self.succ[targets.start as usize..targets.end as usize] {
                    f(Stmt(self.blocks[b.index()].start));
                }
            }
            StmtKind::Suspend { resume, cancel } => {
                f(Stmt(self.blocks[resume.index()].start));
                f(Stmt(self.blocks[cancel.index()].start));
            }
            StmtKind::Return | StmtKind::Unreachable => {}
            _ => f(Stmt(s.0 + 1)),
        }
    }

    pub fn origin_live_mentions(&self, l: Local) -> impl Iterator<Item = Origin> + '_ {
        self.local_origins.iter().filter(move |(x, _)| *x == l).map(|(_, o)| *o)
    }

    /// Writes `p` in the stable text form: `_1.0.*[_].box as 2`.
    pub fn write_place(&self, p: Place, f: &mut impl fmt::Write) -> fmt::Result {
        let row = self.place(p);
        write!(f, "{}", row.local)?;
        for d in 1..=row.depth {
            match self.place(self.ancestor(p, d)).proj {
                Some(Proj::Field(i)) => write!(f, ".{i}")?,
                Some(Proj::Deref) => write!(f, ".*")?,
                Some(Proj::DerefBox) => write!(f, ".box")?,
                Some(Proj::Index) => write!(f, "[_]")?,
                Some(Proj::ConstantIndex { offset, from_end: false, .. }) => write!(f, "[{offset}]")?,
                Some(Proj::ConstantIndex { offset, from_end: true, .. }) => write!(f, "[-{offset}]")?,
                Some(Proj::Subslice { from, to, from_end: false }) => write!(f, "[{from}..{to}]")?,
                Some(Proj::Subslice { from, to, from_end: true }) => write!(f, "[{from}..-{to}]")?,
                Some(Proj::Downcast(v)) => write!(f, " as {v}")?,
                None => {}
            }
        }
        Ok(())
    }

    fn write_operand(&self, o: Operand, f: &mut impl fmt::Write) -> fmt::Result {
        match o {
            Operand::Copy(p) => {
                f.write_str("copy ")?;
                self.write_place(p, f)
            }
            Operand::Move(p) => {
                f.write_str("move ")?;
                self.write_place(p, f)
            }
            Operand::Const => f.write_str("const"),
        }
    }

    fn write_ops(&self, o: Ops, f: &mut impl fmt::Write) -> fmt::Result {
        for (i, op) in self.ops(o).iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            self.write_operand(*op, f)?;
        }
        Ok(())
    }

    /// The whole flow in the stable text form test expectations use.
    pub fn print(&self, f: &mut impl fmt::Write) -> fmt::Result {
        for (b, range) in self.blocks.iter().enumerate() {
            writeln!(f, "bb{b}:")?;
            for s in range.start..range.end {
                write!(f, "  s{s} ")?;
                match self.stmts[s as usize].kind {
                    StmtKind::Assign { dest, value } => {
                        self.write_place(dest, f)?;
                        f.write_str(" = ")?;
                        match value {
                            Value::Use(o) => self.write_operand(o, f)?,
                            Value::Borrow { kind, place, loan } => {
                                let k = match kind {
                                    BorrowKind::Shared => "&",
                                    BorrowKind::Mut => "&mut ",
                                    BorrowKind::TwoPhase => "&2ph ",
                                    BorrowKind::Shallow => "&shallow ",
                                };
                                f.write_str(k)?;
                                self.write_place(place, f)?;
                                let o = self.loan(loan).origin;
                                write!(f, " [{loan} in {o}]")?;
                            }
                            Value::Aggregate(ops) => {
                                f.write_str("(")?;
                                self.write_ops(ops, f)?;
                                f.write_str(")")?;
                            }
                        }
                    }
                    StmtKind::Call { args, dest } => {
                        self.write_place(dest, f)?;
                        f.write_str(" = call(")?;
                        self.write_ops(args, f)?;
                        f.write_str(")")?;
                    }
                    StmtKind::StorageLive(l) => write!(f, "live {l}")?,
                    StmtKind::StorageDead(l) => write!(f, "dead {l}")?,
                    StmtKind::Drop(p) => {
                        f.write_str("drop ")?;
                        self.write_place(p, f)?;
                    }
                    StmtKind::Activate(l) => write!(f, "activate {l}")?,
                    StmtKind::Goto(b) => write!(f, "goto {b}")?,
                    StmtKind::Switch { on, targets } => {
                        f.write_str("switch ")?;
                        self.write_place(on, f)?;
                        f.write_str(" ->")?;
                        for t in &self.succ[targets.start as usize..targets.end as usize] {
                            write!(f, " {t}")?;
                        }
                    }
                    StmtKind::Return => f.write_str("return")?,
                    StmtKind::Unreachable => f.write_str("unreachable")?,
                    StmtKind::Suspend { resume, cancel } => write!(f, "suspend -> {resume}, cancel {cancel}")?,
                }
                for &(at, from, into) in self.subsets.iter() {
                    if at.0 == s {
                        write!(f, "; {from}: {into}")?;
                    }
                }
                f.write_str("\n")?;
            }
        }
        Ok(())
    }
}

/// Builds a flow by hand: reserve blocks, then fill each in turn, ending it with a terminator.
pub struct FlowBuilder<A: Allocator + Clone> {
    flow: Flow<A>,
    filled: Vec<bool, A>,
    open: Option<Block>,
    error: Option<FlowError>,
}

impl<A: Allocator + Clone> FlowBuilder<A> {
    /// A body of `locals` locals, of which `1..=args` are arguments (`Local(0)` returns).
    pub fn new_in(locals: u32, args: u32, alloc: A) -> FlowBuilder<A> {
        let mut places = Vec::new_in(alloc.clone());
        let mut local_places = Vec::new_in(alloc.clone());
        for l in 0..locals {
            local_places.push(Place(l));
            places.push(PlaceRow {
                local: Local(l),
                parent: None,
                proj: None,
                depth: 0,
                of_drop_type: false,
                of_slice: false,
            });
        }
        FlowBuilder {
            flow: Flow {
                locals,
                args,
                places,
                stmts: Vec::new_in(alloc.clone()),
                blocks: Vec::new_in(alloc.clone()),
                stmt_block: Vec::new_in(alloc.clone()),
                operands: Vec::new_in(alloc.clone()),
                succ: Vec::new_in(alloc.clone()),
                loans: Vec::new_in(alloc.clone()),
                subsets: Vec::new_in(alloc.clone()),
                origins: 0,
                placeholder: Vec::new_in(alloc.clone()),
                local_origins: Vec::new_in(alloc.clone()),
                known: Vec::new_in(alloc.clone()),
                local_places,
            },
            filled: Vec::new_in(alloc),
            open: None,
            error: None,
        }
    }

    pub fn local(&self, l: u32) -> Place {
        self.flow.local_places[l as usize]
    }

    /// The projection that made `p` from its parent; `None` for a local's own place (or a
    /// place this builder never made).
    pub fn proj_of(&self, p: Place) -> Option<Proj> {
        self.flow.places.get(p.index()).and_then(|row| row.proj)
    }

    /// A new local (a temporary or a binding found while lowering): its place is a new row.
    pub fn add_local(&mut self) -> Local {
        let l = Local(self.flow.locals);
        self.flow.locals += 1;
        self.flow.places.push(PlaceRow { local: l, parent: None, proj: None, depth: 0, of_drop_type: false, of_slice: false });
        self.flow.local_places.push(Place(self.flow.places.len() as u32 - 1));
        l
    }

    /// The place `parent` projected by `proj`, interned: one id per distinct place.
    pub fn project(&mut self, parent: Place, proj: Proj) -> Place {
        self.project_marked(parent, proj, false)
    }

    /// As [`Self::project`], marking a field of a type that implements `Drop`.
    pub fn project_marked(&mut self, parent: Place, proj: Proj, of_drop_type: bool) -> Place {
        self.project_in(parent, proj, if of_drop_type { PlaceMark::DropField } else { PlaceMark::None })
    }

    /// As [`Self::project`], the new row marked `mark` (a place is interned by its parent and
    /// projection: the mark is a fact of those, so a second ask finds the first row).
    pub fn project_in(&mut self, parent: Place, proj: Proj, mark: PlaceMark) -> Place {
        for (i, row) in self.flow.places.iter().enumerate() {
            if row.parent == Some(parent) && row.proj == Some(proj) {
                return Place(i as u32);
            }
        }
        let up = self.flow.place(parent);
        self.flow.places.push(PlaceRow {
            local: up.local,
            parent: Some(parent),
            proj: Some(proj),
            depth: up.depth + 1,
            of_drop_type: mark == PlaceMark::DropField,
            of_slice: mark == PlaceMark::SliceElement,
        });
        Place(self.flow.places.len() as u32 - 1)
    }

    pub fn origin(&mut self, placeholder: bool) -> Origin {
        self.flow.origins += 1;
        self.flow.placeholder.push(placeholder);
        Origin(self.flow.origins - 1)
    }

    /// The type of `local` mentions `origin`.
    pub fn mentions(&mut self, local: u32, origin: Origin) {
        let row = (Local(local), origin);
        if let Err(at) = self.flow.local_origins.binary_search(&row) {
            self.flow.local_origins.insert(at, row);
        }
    }

    /// The signature says `from: into` between two placeholders.
    pub fn known(&mut self, from: Origin, into: Origin) {
        self.flow.known.push((from, into));
    }

    pub fn block(&mut self) -> Block {
        self.flow.blocks.push(Ops { start: 0, end: 0 });
        self.filled.push(false);
        Block(self.flow.blocks.len() as u32 - 1)
    }

    /// Starts filling `b`: every statement until its terminator goes into it.
    pub fn start(&mut self, b: Block) {
        if self.open.is_some() || self.filled[b.index()] {
            self.error.get_or_insert(FlowError::NotInBlock);
        }
        self.open = Some(b);
        let at = self.flow.stmts.len() as u32;
        self.flow.blocks[b.index()] = Ops { start: at, end: at };
    }

    fn push(&mut self, kind: StmtKind, span: Span) -> Stmt {
        let Some(b) = self.open else {
            self.error.get_or_insert(FlowError::NotInBlock);
            return Stmt(0);
        };
        self.flow.stmts.push(StmtRow { kind, span });
        self.flow.stmt_block.push(b);
        let s = self.flow.stmts.len() as u32;
        self.flow.blocks[b.index()].end = s;
        if kind.is_terminator() {
            self.filled[b.index()] = true;
            self.open = None;
        }
        Stmt(s - 1)
    }

    fn ops(&mut self, ops: &[Operand]) -> Ops {
        let start = self.flow.operands.len() as u32;
        self.flow.operands.extend_from_slice(ops);
        Ops { start, end: self.flow.operands.len() as u32 }
    }

    pub fn assign(&mut self, dest: Place, op: Operand, span: Span) -> Stmt {
        self.push(StmtKind::Assign { dest, value: Value::Use(op) }, span)
    }

    pub fn aggregate(&mut self, dest: Place, ops: &[Operand], span: Span) -> Stmt {
        let ops = self.ops(ops);
        self.push(StmtKind::Assign { dest, value: Value::Aggregate(ops) }, span)
    }

    /// `dest = &kind place`, issuing a new loan into `origin`.
    pub fn borrow(
        &mut self,
        dest: Place,
        kind: BorrowKind,
        place: Place,
        origin: Origin,
        span: Span,
    ) -> Loan {
        let loan = Loan(self.flow.loans.len() as u32);
        let at = Stmt(self.flow.stmts.len() as u32);
        self.flow.loans.push(LoanRow { place, kind, origin, at });
        self.push(StmtKind::Assign { dest, value: Value::Borrow { kind, place, loan } }, span);
        loan
    }

    pub fn call(&mut self, args: &[Operand], dest: Place, span: Span) -> Stmt {
        let args = self.ops(args);
        self.push(StmtKind::Call { args, dest }, span)
    }

    pub fn stmt(&mut self, kind: StmtKind, span: Span) -> Stmt {
        self.push(kind, span)
    }

    /// `subset_base(from, into)` at the Mid of the statement added last: `from` outlives `into`.
    pub fn subset(&mut self, from: Origin, into: Origin) {
        match self.flow.stmts.len().checked_sub(1) {
            Some(s) => self.flow.subsets.push((Stmt(s as u32), from, into)),
            None => {
                self.error.get_or_insert(FlowError::NotInBlock);
            }
        }
    }

    pub fn goto(&mut self, b: Block, span: Span) -> Stmt {
        self.push(StmtKind::Goto(b), span)
    }

    pub fn switch(&mut self, on: Place, targets: &[Block], span: Span) -> Stmt {
        let start = self.flow.succ.len() as u32;
        self.flow.succ.extend_from_slice(targets);
        let targets = Ops { start, end: self.flow.succ.len() as u32 };
        self.push(StmtKind::Switch { on, targets }, span)
    }

    pub fn ret(&mut self, span: Span) -> Stmt {
        self.push(StmtKind::Return, span)
    }

    /// Ends the open block with an `.await`'s suspension (span: the empty range, as the prompt's
    /// signature takes none).
    pub fn suspend(&mut self, resume: Block, cancel: Block) {
        self.push(StmtKind::Suspend { resume, cancel }, Span::default());
    }

    pub fn finish(self) -> Result<Flow<A>, FlowError> {
        if let Some(e) = self.error {
            return Err(e);
        }
        if let Some(b) = self.open {
            return Err(FlowError::Unterminated(b));
        }
        if self.flow.blocks.is_empty() {
            return Err(FlowError::Empty);
        }
        if let Some(b) = self.filled.iter().position(|f| !f) {
            return Err(FlowError::UnfilledBlock(Block(b as u32)));
        }
        Ok(self.flow)
    }
}
