//! [`verify`]: the body's well-formedness, checked once, returning the [`Verified`] proof every
//! later pass takes (rule 74). What it checks, in order, each refusal naming the row:
//!
//! 1. The per-statement columns are as long as the statement array, and one own place per local.
//! 2. Every row of every table decodes ([`VerifyError::BadRow`]) and every id it names is in its
//!    table ([`VerifyError::OutOfRange`]); a place's parent comes before it and it is one
//!    projection deeper than its parent, of the same local; each local's own place is itself.
//!    The type overlay's children are the record's or earlier overlay types, its lists the
//!    record's or wholly its own, and no two of its types are one node.
//! 3. Every statement row's ids are in range ([`check_refs`], which the builder runs per row).
//! 4. Blocks partition the statement array; the entry block is not a cleanup block; every block
//!    ends in exactly one terminator.
//! 5. An unwind edge enters only a cleanup block, and only from a normal one; a cleanup block is
//!    entered only by an unwind edge or from another cleanup block, and a cleanup block's normal
//!    edges stay in cleanup blocks; `Resume` is only in a cleanup block.
//! 6. A `Switch`'s case values are ascending and distinct; a call's destination and target are
//!    both present, or both absent (it diverges).
//!
//! LEFT, needing the type table's reader (a `PTy` is a handle and says nothing alone): that each
//! operand's type is the one its use wants (an assignment's value and its place, a call's
//! arguments and the callee's signature, a switch's operand an integer or `bool`, an assert's
//! condition a `bool`, a field's type the parent's field); that a local is storage-live where it
//! is used (a dataflow, the check's work: P3).
#![blaze::sans_io]

use core::alloc::Allocator;

use blaze_typeck::{PNode, PTys};

use crate::body::{
    BadRow, Block, BlockKind, BlockRow, Body, BodyRef, BorrowKind, Callee, CalleeId, Const, ConstDecl,
    ConstKind, Edge, List, Local, LocalDecl, LocalKind, Loan, LoanRow, Mutbl, NONE, Operand, Ops,
    Origin, PTy, Place, PlaceRow, Proj, Span, Stmt, StmtKind, StmtRow, Subset, Table, TyError,
    TyListRef, Unwind, Value,
};
use crate::code::ty_children;

/// A row of a table: where an error is.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Site {
    pub table: Table,
    pub row: u32,
}

/// Why a body is not well formed, naming the row.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VerifyError {
    /// A row that is no form of its table.
    BadRow(BadRow),
    /// A per-statement or per-local column of the wrong length.
    ColumnLength { table: Table, len: u32, want: u32 },
    /// The row at `at` names `id` of `table`, which is past that table (or, for a place's
    /// parent, not before the place).
    OutOfRange { at: Site, table: Table, id: u32 },
    /// A local's own place is not the local itself.
    LocalPlace { local: Local },
    /// A place is not one projection deeper than its parent, of the same local.
    PlaceShape { place: Place },
    /// The body has no block.
    NoBlocks,
    /// A block with no rows.
    EmptyBlock { block: Block },
    /// A block's rows run past the statement array.
    BlockRange { block: Block },
    /// A statement is not in exactly the block its column names.
    StmtBlock { stmt: Stmt },
    /// The entry block is a cleanup block.
    EntryIsCleanup,
    /// A block whose last row is not a terminator.
    NoTerminator { block: Block, last: Stmt },
    /// A terminator before a block's last row.
    TerminatorInside { block: Block, stmt: Stmt },
    /// An unwind edge into a normal block.
    UnwindIntoNormal { stmt: Stmt, block: Block },
    /// An unwind edge to a cleanup block out of a cleanup block (a panic while unwinding
    /// aborts: its unwind is `terminate` or `unreachable`).
    UnwindFromCleanup { stmt: Stmt, block: Block },
    /// A normal edge from a normal block into a cleanup block.
    NormalIntoCleanup { stmt: Stmt, block: Block },
    /// A normal edge from a cleanup block back into a normal block.
    CleanupIntoNormal { stmt: Stmt, block: Block },
    /// `Resume` in a normal block.
    ResumeOutsideCleanup { stmt: Stmt },
    /// A switch with two cases of one value.
    DuplicateCase { stmt: Stmt, value: u128 },
    /// A switch whose case values descend.
    CasesUnsorted { stmt: Stmt },
    /// A call with a destination and no target, or a target and no destination.
    HalfDestination { stmt: Stmt },
    /// The overlay's bases and rows disagree: rows with no base, or a base with no rows.
    TypeBase { ty_base: u32, list_base: u32 },
    /// Overlay type `ty` names type `child`, which is neither the record's nor an overlay type
    /// before it.
    TypeChild { ty: PTy, child: PTy },
    /// Overlay type `ty` names a list that is neither the record's nor wholly the overlay's.
    TypeList { ty: PTy, list: List },
    /// Overlay type `ty` is the same node as the earlier overlay type `first`.
    DuplicateType { ty: PTy, first: PTy },
    /// Overlay list entry `entry` names type `child`, past every type the body has.
    ListEntry { entry: u32, child: PTy },
}

impl From<BadRow> for VerifyError {
    fn from(b: BadRow) -> VerifyError {
        VerifyError::BadRow(b)
    }
}

/// The proof that a body is well formed: only [`verify`] makes one. Its readers do not fail:
/// every row decodes and every id is in range.
#[derive(Clone, Copy)]
pub struct Verified<'b> {
    body: BodyRef<'b>,
}

impl<'b> Verified<'b> {
    /// The body this proof is of.
    pub fn body(&self) -> BodyRef<'b> {
        self.body
    }

    /// How many locals: `_0` the return place, then the arguments, then the rest.
    pub fn locals(&self) -> u32 {
        self.body.locals()
    }

    /// How many arguments: the locals `1..=args`.
    pub fn args(&self) -> u32 {
        self.body.args()
    }

    /// Local `l`'s type, kind and mutability.
    pub fn local(&self, l: Local) -> LocalDecl {
        self.body.local(l).unwrap_or(LocalDecl { ty: PTy(NONE), kind: LocalKind::Return, mutbl: Mutbl::Not })
    }

    /// The place that is local `l` itself.
    pub fn local_place(&self, l: Local) -> Place {
        self.body.local_place(l).unwrap_or(Place(NONE))
    }

    /// How many places (each distinct place once, a parent before its children).
    pub fn places(&self) -> u32 {
        self.body.places()
    }

    /// Place `p`'s row: its local, parent, projection, depth and drop-type mark.
    pub fn place(&self, p: Place) -> PlaceRow {
        self.body
            .place(p)
            .unwrap_or(PlaceRow { local: Local(NONE), parent: None, proj: None, depth: 0, of_drop_type: false })
    }

    /// How many constants.
    pub fn consts(&self) -> u32 {
        self.body.consts()
    }

    /// Constant `c`'s kind and type.
    pub fn konst(&self, c: Const) -> ConstDecl {
        self.body.konst(c).unwrap_or(ConstDecl { kind: ConstKind::Scalar { bits: 0 }, ty: PTy(NONE) })
    }

    /// The bytes of a string or byte-string constant; `None` for a constant of another kind.
    pub fn bytes(&self, c: Const) -> Option<&'b [u8]> {
        match self.konst(c).kind {
            ConstKind::Bytes { .. } => self.body.bytes(c).ok(),
            _ => None,
        }
    }

    /// Callee `f`: what call `f`'s row calls.
    pub fn callee(&self, f: CalleeId) -> Callee {
        self.body.callee(f).unwrap_or(Callee::Value(Operand::Const(Const(NONE))))
    }

    /// How many callees: one per call.
    pub fn callees(&self) -> u32 {
        self.body.callees()
    }

    /// How many statement rows (terminators included).
    pub fn stmts(&self) -> u32 {
        self.body.stmts()
    }

    /// Statement row `s`: its kind and span.
    pub fn stmt(&self, s: Stmt) -> StmtRow {
        self.body.stmt(s).unwrap_or(StmtRow { kind: StmtKind::Unreachable, span: Span::default() })
    }

    /// The block row `s` is in.
    pub fn block_of(&self, s: Stmt) -> Block {
        self.body.block_of(s).unwrap_or(Block(NONE))
    }

    /// How many blocks; `Block(0)` is the entry.
    pub fn blocks(&self) -> u32 {
        self.body.blocks()
    }

    /// Block `b`'s rows and kind.
    pub fn block(&self, b: Block) -> BlockRow {
        self.body.block(b).unwrap_or(BlockRow { stmts: Ops { start: 0, end: 0 }, kind: BlockKind::Normal })
    }

    /// The first row of the entry block.
    pub fn entry(&self) -> Stmt {
        Stmt(self.block(Block(0)).stmts.start)
    }

    /// The row that ends block `b`.
    pub fn terminator(&self, b: Block) -> Stmt {
        Stmt(self.block(b).stmts.end - 1)
    }

    /// The operands of a range (a call's arguments, an aggregate's fields).
    pub fn ops(&self, o: Ops) -> impl Iterator<Item = Operand> + 'b {
        let body = self.body;
        (o.start..o.end).filter_map(move |i| body.operand(i).ok())
    }

    /// The cases of a switch's range: values ascending, each once.
    pub fn cases(&self, o: Ops) -> impl Iterator<Item = (u128, Block)> + 'b {
        let body = self.body;
        (o.start..o.end).filter_map(move |i| body.case(i).ok())
    }

    /// How many loans: one per borrow statement.
    pub fn loans(&self) -> u32 {
        self.body.loans()
    }

    /// Loan `l`: the place borrowed, how, into which origin, and at which statement.
    pub fn loan(&self, l: Loan) -> LoanRow {
        self.body.loan(l).unwrap_or(LoanRow {
            place: Place(NONE),
            kind: BorrowKind::Shared,
            origin: Origin(NONE),
            at: Stmt(NONE),
        })
    }

    /// The subsets, in statement order.
    pub fn subsets(&self) -> impl Iterator<Item = Subset> + 'b {
        self.body.subsets()
    }

    /// How many origins.
    pub fn origins(&self) -> u32 {
        self.body.origins()
    }

    /// Whether origin `o` is a placeholder: a lifetime of the signature.
    pub fn placeholder(&self, o: Origin) -> bool {
        self.body.placeholder(o).unwrap_or(false)
    }

    /// `(local, origin)`: the local's type mentions the origin. Sorted, each once.
    pub fn local_origins(&self) -> impl Iterator<Item = (Local, Origin)> + 'b {
        self.body.local_origins()
    }

    /// Known `from: into` between placeholders (the signature's bounds).
    pub fn known(&self) -> impl Iterator<Item = (Origin, Origin)> + 'b {
        self.body.known()
    }

    /// The record's type count the overlay is numbered after; `None` when the body has none.
    pub fn ty_base(&self) -> Option<u32> {
        self.body.ty_base()
    }

    /// How many types the overlay holds.
    pub fn own_tys(&self) -> u32 {
        self.body.own_tys()
    }

    /// The overlay's node for `t`; `Ok(None)` for a handle of the record. Every overlay row
    /// decodes (verify's check), so the one refusal is a handle past the overlay.
    pub fn own_ty(&self, t: PTy) -> Result<Option<PNode>, BadRow> {
        self.body.own_ty(t)
    }

    /// The node of any type handle the body names, against the record it was built with:
    /// refused only when `record` is another table or the handle is in neither.
    pub fn ty(&self, record: &PTys<'_>, t: PTy) -> Result<PNode, TyError> {
        self.body.ty(record, t)
    }

    /// The entries of any list the body names, against the record it was built with.
    pub fn ty_list<'r>(&self, record: &PTys<'r>, l: List) -> Result<TyListRef<'b, 'r>, TyError> {
        self.body.ty_list(record, l)
    }

    /// Calls `f` on each block edge out of terminator `kind`, with how it is taken.
    pub fn for_each_edge(&self, kind: &StmtKind, f: impl FnMut(Block, Edge)) {
        let _ = self.body.for_each_edge(kind, f);
    }

    /// Calls `f` on each successor row of `s`: the next row for a statement, each edge's
    /// block's first row (unwind edges included) for a terminator.
    pub fn for_each_succ(&self, s: Stmt, mut f: impl FnMut(Stmt)) {
        let kind = self.stmt(s).kind;
        if !kind.is_terminator() {
            f(Stmt(s.0 + 1));
            return;
        }
        let body = self.body;
        self.for_each_edge(&kind, |b, _| {
            if let Ok(row) = body.block(b) {
                f(Stmt(row.stmts.start));
            }
        });
    }

    /// The prefix of `p` at `depth` (0 is the local's place).
    pub fn ancestor(&self, mut p: Place, depth: u32) -> Place {
        loop {
            let row = self.place(p);
            match row.parent {
                Some(up) if row.depth > depth => p = up,
                _ => return p,
            }
        }
    }

    /// `a` is `b` or a prefix of `b`.
    pub fn is_prefix(&self, a: Place, b: Place) -> bool {
        let (ra, rb) = (self.place(a), self.place(b));
        ra.local == rb.local && ra.depth <= rb.depth && self.ancestor(b, ra.depth) == a
    }

    /// Whether the projections of `p` below depth `from` include a `Deref` (not owned).
    pub fn deref_below(&self, mut p: Place, from: u32) -> bool {
        loop {
            let row = self.place(p);
            if row.depth <= from {
                return false;
            }
            if row.proj == Some(Proj::Deref) {
                return true;
            }
            match row.parent {
                Some(up) => p = up,
                None => return false,
            }
        }
    }
}

/// Checks `body`, giving the proof on success.
pub fn verify<A: Allocator>(body: &Body<A>) -> Result<Verified<'_>, VerifyError> {
    verify_ref(body.view())
}

/// Checks a body read in place (of memory or of a record), giving the proof on success.
pub fn verify_ref(body: BodyRef<'_>) -> Result<Verified<'_>, VerifyError> {
    lengths(&body)?;
    tables(&body)?;
    for s in 0..body.stmts() {
        let row = body.stmt(Stmt(s))?;
        check_refs(&body, Stmt(s), &row.kind)?;
    }
    blocks(&body)?;
    for b in 0..body.blocks() {
        terminator(&body, Block(b))?;
    }
    Ok(Verified { body })
}

fn len(n: usize) -> u32 {
    n as u32
}

fn lengths(b: &BodyRef<'_>) -> Result<(), VerifyError> {
    let want = len(b.stmts.len());
    for (table, n) in [(Table::Spans, b.spans.len()), (Table::StmtBlocks, b.stmt_block.len())] {
        if len(n) != want {
            return Err(VerifyError::ColumnLength { table, len: len(n), want });
        }
    }
    if b.local_places.len() != b.locals.len() {
        return Err(VerifyError::ColumnLength {
            table: Table::LocalPlaces,
            len: len(b.local_places.len()),
            want: len(b.locals.len()),
        });
    }
    Ok(())
}

/// The id checks of one row's references, at `site`.
struct Refs<'r, 'b> {
    b: &'r BodyRef<'b>,
    site: Site,
}

impl Refs<'_, '_> {
    fn within(&self, table: Table, id: u32, n: u32) -> Result<(), VerifyError> {
        match id < n {
            true => Ok(()),
            false => Err(VerifyError::OutOfRange { at: self.site, table, id }),
        }
    }

    fn local(&self, l: Local) -> Result<(), VerifyError> {
        self.within(Table::Locals, l.0, self.b.locals())
    }

    fn place(&self, p: Place) -> Result<(), VerifyError> {
        self.within(Table::Places, p.0, self.b.places())
    }

    fn block(&self, x: Block) -> Result<(), VerifyError> {
        self.within(Table::Blocks, x.0, self.b.blocks())
    }

    fn konst(&self, c: Const) -> Result<(), VerifyError> {
        self.within(Table::Consts, c.0, self.b.consts())
    }

    fn loan(&self, l: Loan) -> Result<(), VerifyError> {
        self.within(Table::Loans, l.0, self.b.loans())
    }

    fn origin(&self, o: Origin) -> Result<(), VerifyError> {
        self.within(Table::Origins, o.0, self.b.origins())
    }

    fn stmt(&self, s: Stmt) -> Result<(), VerifyError> {
        self.within(Table::Stmts, s.0, self.b.stmts())
    }

    fn operand(&self, o: Operand) -> Result<(), VerifyError> {
        match o {
            Operand::Copy(p) | Operand::Move(p) => self.place(p),
            Operand::Const(c) => self.konst(c),
        }
    }

    /// A range of a side table of `n` rows.
    fn range(&self, table: Table, o: Ops, n: usize) -> Result<(), VerifyError> {
        if o.start > o.end {
            return Err(VerifyError::OutOfRange { at: self.site, table, id: o.start });
        }
        match o.end as usize <= n {
            true => Ok(()),
            false => Err(VerifyError::OutOfRange { at: self.site, table, id: o.end }),
        }
    }

    fn ops(&self, o: Ops) -> Result<(), VerifyError> {
        self.range(Table::Operands, o, self.b.operands.len())
    }

    fn unwind(&self, u: Unwind) -> Result<(), VerifyError> {
        match u {
            Unwind::Cleanup(x) => self.block(x),
            Unwind::Continue | Unwind::Unreachable | Unwind::Terminate => Ok(()),
        }
    }

    fn value(&self, v: Value) -> Result<(), VerifyError> {
        match v {
            Value::Use(o) | Value::Unary { operand: o, .. } | Value::Cast { operand: o, .. } => self.operand(o),
            Value::Borrow { place, loan, .. } => {
                self.place(place)?;
                self.loan(loan)
            }
            Value::RawAddr { place, .. } | Value::Len(place) | Value::Discriminant(place) => self.place(place),
            Value::Binary { lhs, rhs, .. } | Value::CheckedBinary { lhs, rhs, .. } => {
                self.operand(lhs)?;
                self.operand(rhs)
            }
            Value::Aggregate { operands, .. } => self.ops(operands),
            Value::Repeat { operand, count } => {
                self.operand(operand)?;
                self.konst(count)
            }
            Value::SizeOf(_) | Value::AlignOf(_) => Ok(()),
            Value::ThreadLocalAddr(c) => self.konst(c),
        }
    }
}

/// Whether every id statement `at` names (`kind`, not yet in the table while a builder adds
/// it) is in its table. The builder's check per row and step 3 of [`verify`]: one home.
pub fn check_refs(body: &BodyRef<'_>, at: Stmt, kind: &StmtKind) -> Result<(), VerifyError> {
    let r = Refs { b: body, site: Site { table: Table::Stmts, row: at.0 } };
    match *kind {
        StmtKind::Assign { dest, value } => {
            r.place(dest)?;
            r.value(value)
        }
        StmtKind::StorageLive(l) | StmtKind::StorageDead(l) => r.local(l),
        StmtKind::Activate(l) => r.loan(l),
        StmtKind::FakeRead(p) | StmtKind::Deinit(p) | StmtKind::SetDiscriminant { place: p, .. } => r.place(p),
        StmtKind::Goto(t) => r.block(t),
        StmtKind::Switch { on, cases, otherwise } => {
            r.operand(on)?;
            r.range(Table::Cases, cases, body.cases.len())?;
            r.block(otherwise)
        }
        StmtKind::Call { callee, args, dest, target, unwind } => {
            r.within(Table::Callees, callee.0, body.callees())?;
            r.ops(args)?;
            if let Some(p) = dest {
                r.place(p)?;
            }
            if let Some(t) = target {
                r.block(t)?;
            }
            r.unwind(unwind)
        }
        StmtKind::Drop { place, target, unwind } => {
            r.place(place)?;
            r.block(target)?;
            r.unwind(unwind)
        }
        StmtKind::Assert { cond, msg, target, unwind, .. } => {
            r.operand(cond)?;
            r.ops(msg)?;
            r.block(target)?;
            r.unwind(unwind)
        }
        StmtKind::Suspend { resume, cancel } => {
            r.block(resume)?;
            r.block(cancel)
        }
        StmtKind::InlineAsm { operands, target, unwind, .. } => {
            r.ops(operands)?;
            if let Some(t) = target {
                r.block(t)?;
            }
            r.unwind(unwind)
        }
        StmtKind::Return | StmtKind::Unreachable | StmtKind::Resume | StmtKind::Terminate => Ok(()),
    }
}

fn at(table: Table, row: u32) -> Site {
    Site { table, row }
}

fn tables(b: &BodyRef<'_>) -> Result<(), VerifyError> {
    let locals = b.locals();
    for i in 0..locals {
        if let LocalKind::DropFlag(of) = b.local(Local(i))?.kind {
            Refs { b, site: at(Table::Locals, i) }.local(of)?;
        }
    }
    for l in 0..locals {
        let p = b.local_place(Local(l))?;
        Refs { b, site: at(Table::LocalPlaces, l) }.place(p)?;
        let row = b.place(p)?;
        if row.local != Local(l) || row.parent.is_some() {
            return Err(VerifyError::LocalPlace { local: Local(l) });
        }
    }
    for i in 0..b.places() {
        let row = b.place(Place(i))?;
        let r = Refs { b, site: at(Table::Places, i) };
        r.local(row.local)?;
        if let Some(Proj::Index(l)) = row.proj {
            r.local(l)?;
        }
        let shaped = match row.parent {
            None => row.depth == 0,
            Some(up) => {
                r.within(Table::Places, up.0, i)?;
                let parent = b.place(up)?;
                parent.local == row.local && parent.depth.checked_add(1) == Some(row.depth)
            }
        };
        if !shaped {
            return Err(VerifyError::PlaceShape { place: Place(i) });
        }
    }
    for i in 0..b.consts() {
        if let ConstKind::Bytes { start, len } = b.konst(Const(i))?.kind {
            if u64::from(start) + u64::from(len) > b.data.len() as u64 {
                return Err(VerifyError::OutOfRange { at: at(Table::Consts, i), table: Table::Data, id: start });
            }
        }
    }
    for i in 0..b.callees() {
        if let Callee::Value(o) = b.callee(CalleeId(i))? {
            Refs { b, site: at(Table::Callees, i) }.operand(o)?;
        }
    }
    for i in 0..len(b.operands.len()) {
        Refs { b, site: at(Table::Operands, i) }.operand(b.operand(i)?)?;
    }
    for i in 0..len(b.cases.len()) {
        Refs { b, site: at(Table::Cases, i) }.block(b.case(i)?.1)?;
    }
    for i in 0..b.loans() {
        let row = b.loan(Loan(i))?;
        let r = Refs { b, site: at(Table::Loans, i) };
        r.place(row.place)?;
        r.origin(row.origin)?;
        r.stmt(row.at)?;
    }
    for (i, s) in b.subsets().enumerate() {
        let r = Refs { b, site: at(Table::Subsets, len(i)) };
        r.stmt(s.at)?;
        r.origin(s.from)?;
        r.origin(s.into)?;
    }
    for o in 0..b.origins() {
        b.placeholder(Origin(o))?;
    }
    for (i, (l, o)) in b.local_origins().enumerate() {
        let r = Refs { b, site: at(Table::LocalOrigins, len(i)) };
        r.local(l)?;
        r.origin(o)?;
    }
    for (i, (from, into)) in b.known().enumerate() {
        let r = Refs { b, site: at(Table::Known, len(i)) };
        r.origin(from)?;
        r.origin(into)?;
    }
    overlay(b)
}

/// The type overlay: bases present exactly when rows are; each row a type whose children are
/// the record's or overlay types before it, and whose lists are the record's or wholly the
/// overlay's; each list entry a type the body has; no two overlay rows the same node (rows
/// are canonical, so equal nodes are equal rows). Duplicates are found pairwise: verify
/// allocates nothing, and an overlay is the few types one lowering had to make.
fn overlay(b: &BodyRef<'_>) -> Result<(), VerifyError> {
    let empty = b.tys.is_empty() && b.ty_lists.is_empty();
    let based = match empty {
        true => b.ty_base == NONE && b.list_base == NONE,
        false => {
            let types = u64::from(b.ty_base) + b.tys.len() as u64;
            let lists = u64::from(b.list_base) + b.ty_lists.len() as u64;
            types < u64::from(NONE) && lists < u64::from(NONE)
        }
    };
    if !based {
        return Err(VerifyError::TypeBase { ty_base: b.ty_base, list_base: b.list_base });
    }
    let all = u64::from(b.ty_base).wrapping_add(b.tys.len() as u64);
    for (i, w) in b.ty_lists.iter().enumerate() {
        if u64::from(*w) >= all {
            return Err(VerifyError::ListEntry { entry: b.list_base + len(i), child: PTy(*w) });
        }
    }
    for (i, cur) in b.tys.iter().enumerate() {
        let ty = PTy(b.ty_base + len(i));
        let node = b.own_ty(ty)?.ok_or(BadRow { table: Table::Types, row: len(i), code: NONE })?;
        let bad: core::cell::Cell<Option<VerifyError>> = core::cell::Cell::new(None);
        let refuse = |e: VerifyError| {
            if bad.get().is_none() {
                bad.set(Some(e));
            }
        };
        ty_children(
            node,
            |child| {
                if child.0 >= ty.0 {
                    refuse(VerifyError::TypeChild { ty, child });
                }
            },
            |list| match b.own_list(list) {
                Ok(None) => {}
                Ok(Some(entries)) => {
                    if let Some(child) = entries.iter().find(|e| **e >= ty.0) {
                        refuse(VerifyError::TypeChild { ty, child: PTy(*child) });
                    }
                }
                Err(_) => refuse(VerifyError::TypeList { ty, list }),
            },
        );
        if let Some(e) = bad.get() {
            return Err(e);
        }
        if let Some(j) = b.tys.get(..i).and_then(|prev| prev.iter().position(|w| w == cur)) {
            return Err(VerifyError::DuplicateType { ty, first: PTy(b.ty_base + len(j)) });
        }
    }
    Ok(())
}

fn blocks(b: &BodyRef<'_>) -> Result<(), VerifyError> {
    let n = b.blocks();
    if n == 0 {
        return Err(VerifyError::NoBlocks);
    }
    for i in 0..n {
        let r = b.block(Block(i))?.stmts;
        if r.start >= r.end {
            return Err(VerifyError::EmptyBlock { block: Block(i) });
        }
        if r.end > b.stmts() {
            return Err(VerifyError::BlockRange { block: Block(i) });
        }
        for s in r.start..r.end {
            if b.block_of(Stmt(s))? != Block(i) {
                return Err(VerifyError::StmtBlock { stmt: Stmt(s) });
            }
        }
    }
    for s in 0..b.stmts() {
        let x = b.block_of(Stmt(s))?;
        Refs { b, site: at(Table::StmtBlocks, s) }.block(x)?;
        let r = b.block(x)?.stmts;
        if !(r.start <= s && s < r.end) {
            return Err(VerifyError::StmtBlock { stmt: Stmt(s) });
        }
    }
    if b.block(Block(0))?.kind == BlockKind::Cleanup {
        return Err(VerifyError::EntryIsCleanup);
    }
    for i in 0..n {
        let r = b.block(Block(i))?.stmts;
        let last = r.end - 1;
        if !b.stmt(Stmt(last))?.kind.is_terminator() {
            return Err(VerifyError::NoTerminator { block: Block(i), last: Stmt(last) });
        }
        for s in r.start..last {
            if b.stmt(Stmt(s))?.kind.is_terminator() {
                return Err(VerifyError::TerminatorInside { block: Block(i), stmt: Stmt(s) });
            }
        }
    }
    Ok(())
}

/// Steps 5 and 6 at block `x`'s terminator.
fn terminator(b: &BodyRef<'_>, x: Block) -> Result<(), VerifyError> {
    let row = b.block(x)?;
    let stmt = Stmt(row.stmts.end - 1);
    let kind = b.stmt(stmt)?.kind;
    let from_cleanup = row.kind == BlockKind::Cleanup;
    let mut found: Option<VerifyError> = None;
    b.for_each_edge(&kind, |to, edge| {
        if found.is_some() {
            return;
        }
        let into_cleanup = match b.block(to) {
            Ok(t) => t.kind == BlockKind::Cleanup,
            Err(e) => {
                found = Some(VerifyError::BadRow(e));
                return;
            }
        };
        found = match (edge, from_cleanup, into_cleanup) {
            (Edge::Unwind, true, _) => Some(VerifyError::UnwindFromCleanup { stmt, block: to }),
            (Edge::Unwind, false, false) => Some(VerifyError::UnwindIntoNormal { stmt, block: to }),
            (Edge::Normal, false, true) => Some(VerifyError::NormalIntoCleanup { stmt, block: to }),
            (Edge::Normal, true, false) => Some(VerifyError::CleanupIntoNormal { stmt, block: to }),
            (Edge::Unwind, false, true) | (Edge::Normal, false, false) | (Edge::Normal, true, true) => None,
        };
    })?;
    if let Some(e) = found {
        return Err(e);
    }
    match kind {
        StmtKind::Resume if !from_cleanup => Err(VerifyError::ResumeOutsideCleanup { stmt }),
        StmtKind::Switch { cases, .. } => {
            let mut prev: Option<u128> = None;
            for i in cases.start..cases.end {
                let (value, _) = b.case(i)?;
                match prev {
                    Some(p) if p == value => return Err(VerifyError::DuplicateCase { stmt, value }),
                    Some(p) if p > value => return Err(VerifyError::CasesUnsorted { stmt }),
                    _ => {}
                }
                prev = Some(value);
            }
            Ok(())
        }
        StmtKind::Call { dest, target, .. } if dest.is_some() != target.is_some() => {
            Err(VerifyError::HalfDestination { stmt })
        }
        _ => Ok(()),
    }
}
