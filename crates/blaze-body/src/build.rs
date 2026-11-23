//! [`BodyBuilder`]: what a lowering writes a body with. It keeps the tables' structural
//! invariants as it goes: locals in order (the return place, the arguments, then the rest),
//! places and constants interned by hash (one row per distinct place or constant, each local's
//! own place made with it), types interned against the record's table with an overlay for the
//! ones it lacks ([`BodyBuilder::intern_ty`]), every id
//! a row names already in its table ([`crate::verify::check_refs`], verify's own check), each
//! block filled once and ended by exactly one terminator, switch cases sorted by value, loans
//! made only with their borrow. What it leaves to [`crate::verify::verify`] are the rules a
//! lowering can get wrong in meaning (unwind edges into the right kind of block, distinct case
//! values, a call's destination with its target), so a test can write a body that breaks them.
//!
//! Like blaze-flow's builder, an error is kept (the first) and returned by [`BodyBuilder::finish`],
//! and [`BodyBuilder::error`] reads it early.
#![blaze::sans_io]

use blaze_hash::{FixedState, Map, MapOpts};
use blaze_own::Vec;
use blaze_typeck::{PNode, PTys};
use core::alloc::Allocator;
use core::cell::Cell;
use core::hash::BuildHasher;

use crate::body::{
    Block, BlockKind, BlockRow, Body, BorrowKind, Callee, CalleeId, Const, ConstDecl, ConstKind, List,
    Local, LocalDecl, LocalKind, Loan, LoanRow, NONE, Operand, Ops, Origin, PTy, Place, PlaceRow,
    Proj, Span, Stmt, StmtKind, Table, Value,
};
use crate::code::{
    ConstWords, PairWords, PlaceWords, SubsetWords, TyWords, encode_block, encode_callee, encode_case,
    encode_const, encode_loan, encode_local, encode_operand, encode_place, encode_stmt, encode_ty, join,
    map_ty_lists, ty_children,
};
use crate::verify::{VerifyError, check_refs};

/// Why a body could not be built.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BuildError {
    /// A statement was pushed with no block open.
    NotInBlock,
    /// `start` of a block while another is open.
    BlockOpen { open: Block, start: Block },
    /// `start` of a block already filled.
    BlockFilled(Block),
    /// The open block was not ended by a terminator.
    Unterminated(Block),
    /// A block was made and never filled.
    UnfilledBlock(Block),
    /// The body has no block.
    Empty,
    /// [`BodyBuilder::push`] of a terminator, at the row it would have been.
    NotAStatement(Stmt),
    /// [`BodyBuilder::terminate`] of a statement, at the row it would have been.
    NotATerminator(Stmt),
    /// An `Assign` of a `Borrow` through `push`: a borrow is made by [`BodyBuilder::borrow`],
    /// which makes its loan.
    BorrowWithoutLoan(Stmt),
    /// A row names an id past its table (verify's check, at the row being added).
    Refs(VerifyError),
    /// A local out of order: the return place not first, or an argument not right after the
    /// return place and the arguments before it.
    LocalOrder(Local),
    /// `subset` before any statement.
    NoStatement,
    /// A table would pass `NONE - 1` rows.
    Full(Table),
    /// [`BodyBuilder::intern_ty`] or [`BodyBuilder::intern_list`] on a builder made without the
    /// record's type table ([`BodyBuilder::new_in`]).
    NoTypeTable,
    /// A type whose child type is past every type the body has, or whose list is neither the
    /// record's nor wholly the overlay's; for the record's table, a node whose list is past its
    /// list column.
    TypeChild { node: PNode },
    /// A list entry past every type the body has.
    ListEntry(PTy),
}

impl From<VerifyError> for BuildError {
    fn from(e: VerifyError) -> BuildError {
        BuildError::Refs(e)
    }
}

/// Builds a body: make locals, constants and blocks, then fill each block in turn, ending it
/// with a terminator.
pub struct BodyBuilder<A: Allocator + Copy> {
    body: Body<A>,
    filled: Vec<bool, A>,
    open: Option<Block>,
    error: Option<BuildError>,
    /// Each projected place's row to its id: one id per distinct place.
    places_index: Map<PlaceWords, u32, FixedState, A>,
    /// Each constant's row to its id: one id per distinct constant.
    consts_index: Map<ConstWords, u32, FixedState, A>,
    /// A string constant's `(hash of its bytes and type, ordinal among equal hashes)` to its id.
    bytes_index: Map<(u64, u32), u32, FixedState, A>,
    /// The record's type table, indexed, when made by [`Self::new_with_types_in`].
    types: Option<TypeIndex<A>>,
}

/// The record's types and the overlay's, by node, and their lists, by entries: what
/// [`BodyBuilder::intern_ty`] and [`BodyBuilder::intern_list`] look up. Built once from the
/// record's table, O(its size); no borrow of the table is kept.
struct TypeIndex<A: Allocator + Copy> {
    ty_base: u32,
    list_base: u32,
    /// Every type, the record's then the overlay's, by its canonical node (each list the first
    /// list of its entries); the record's first of equal nodes wins.
    nodes: Map<PNode, u32, FixedState, A>,
    /// Every distinct non-empty list, by `(hash of its entries, ordinal among equal hashes)`.
    lists: Map<(u64, u32), List, FixedState, A>,
    /// A record list whose entries an earlier list has, to that list.
    canon: Map<List, List, FixedState, A>,
    /// Every list entry, the record's then the overlay's: one index space, as `List`s count.
    entries: Vec<PTy, A>,
}

fn next(len: usize, table: Table) -> Result<u32, BuildError> {
    match u32::try_from(len) {
        Ok(n) if n < NONE => Ok(n),
        _ => Err(BuildError::Full(table)),
    }
}

/// The list in `lists` whose entries are `want` (hashed `h`), or the first free ordinal.
fn find_list<A: Allocator>(
    lists: &Map<(u64, u32), List, FixedState, A>,
    entries: &[PTy],
    want: &[PTy],
    h: u64,
) -> Result<List, u32> {
    let mut k = 0u32;
    while let Some(l) = lists.get(&(h, k)) {
        if entries.get(l.range()) == Some(want) {
            return Ok(*l);
        }
        k += 1;
    }
    Err(k)
}

impl<A: Allocator + Copy> TypeIndex<A> {
    /// `l` as its canonical list: the empty list, the first list of its entries, or itself.
    fn canonical_list(&self, l: List) -> List {
        match (l.len, self.canon.get(&l)) {
            (0, _) => List::EMPTY,
            (_, Some(first)) => *first,
            (_, None) => l,
        }
    }

    /// Indexes a record list; false when it is past the record's list column.
    fn index_list(&mut self, l: List) -> bool {
        if l.len == 0 || self.canon.get(&l).is_some() {
            return true;
        }
        let Some(want) = self.entries.get(l.range()) else {
            return false;
        };
        let h = FixedState.hash_one(want);
        match find_list(&self.lists, &self.entries, want, h) {
            Ok(first) if first == l => {}
            Ok(first) => {
                self.canon.insert(l, first);
            }
            Err(k) => {
                self.lists.insert((h, k), l);
            }
        }
        true
    }

    /// The handle of every type the body has, plus one: the next overlay type's.
    fn types(&self, body_tys: usize) -> u64 {
        u64::from(self.ty_base) + body_tys as u64
    }

    fn list<A2: Allocator>(&mut self, body: &mut Body<A2>, want: &[PTy]) -> Result<List, BuildError> {
        let all = self.types(body.tys.len());
        if let Some(t) = want.iter().find(|t| u64::from(t.0) >= all) {
            return Err(BuildError::ListEntry(*t));
        }
        if want.is_empty() {
            return Ok(List::EMPTY);
        }
        let h = FixedState.hash_one(want);
        let k = match find_list(&self.lists, &self.entries, want, h) {
            Ok(l) => return Ok(l),
            Err(k) => k,
        };
        let start = next(self.entries.len(), Table::TyLists)?;
        let len = u32::try_from(want.len()).map_err(|_| BuildError::Full(Table::TyLists))?;
        if start.checked_add(len).is_none_or(|end| end >= NONE) {
            return Err(BuildError::Full(Table::TyLists));
        }
        self.entries.extend_from_slice(want);
        for t in want {
            body.ty_lists.push(t.0);
        }
        (body.ty_base, body.list_base) = (self.ty_base, self.list_base);
        let l = List { start, len };
        self.lists.insert((h, k), l);
        Ok(l)
    }

    fn ty<A2: Allocator>(&mut self, body: &mut Body<A2>, node: PNode) -> Result<PTy, BuildError> {
        let all = self.types(body.tys.len());
        let (list_base, entries) = (u64::from(self.list_base), self.entries.len() as u64);
        let bad = Cell::new(false);
        ty_children(
            node,
            |t| bad.set(bad.get() || u64::from(t.0) >= all),
            |l| {
                let end = u64::from(l.start) + u64::from(l.len);
                let record = end <= list_base;
                let own = u64::from(l.start) >= list_base && end <= entries;
                bad.set(bad.get() || !(l.len == 0 || record || own));
            },
        );
        if bad.get() {
            return Err(BuildError::TypeChild { node });
        }
        let key = map_ty_lists(node, |l| self.canonical_list(l));
        if let Some(t) = self.nodes.get(&key) {
            return Ok(PTy(*t));
        }
        let t = match u32::try_from(all) {
            Ok(t) if t < NONE => t,
            _ => return Err(BuildError::Full(Table::Types)),
        };
        body.tys.push(encode_ty(key));
        (body.ty_base, body.list_base) = (self.ty_base, self.list_base);
        self.nodes.insert(key, t);
        Ok(PTy(t))
    }
}

impl<A: Allocator + Copy> BodyBuilder<A> {
    /// An empty body that names the types of `record` (the checked record's table, from
    /// `ck.types.view()`) and may add its own: [`Self::intern_ty`] answers the record's handle
    /// for a node the record holds, else a handle numbered after the record's types. Indexes
    /// the record's nodes and lists once, in `alloc`.
    pub fn new_with_types_in(record: PTys<'_>, alloc: A) -> BodyBuilder<A> {
        let mut b = BodyBuilder::new_in(alloc);
        // The table the body's handles index, named once so the kept record (kept.rs) can
        // refuse this body written against another table of the same size.
        b.body.table = Some(crate::kept::TableId::of(record));
        let (ty_base, list_base) = match (next(record.len(), Table::Types), next(record.lists_len(), Table::TyLists)) {
            (Ok(t), Ok(l)) => (t, l),
            (Err(e), _) | (_, Err(e)) => {
                b.fail(e);
                return b;
            }
        };
        let mut ix = TypeIndex {
            ty_base,
            list_base,
            nodes: Map::new(MapOpts {
                hasher: FixedState,
                alloc,
                capacity: Some(ty_base as usize),
            }),
            lists: Map::new(MapOpts {
                hasher: FixedState,
                alloc,
                capacity: Some((list_base as usize).min(ty_base as usize)),
            }),
            canon: Map::new(MapOpts { hasher: FixedState, alloc, capacity: None }),
            entries: Vec::with_capacity_in(list_base as usize, alloc),
        };
        for t in record.lists() {
            ix.entries.push(*t);
        }
        if ix.entries.len() != list_base as usize {
            b.fail(BuildError::Full(Table::TyLists));
            return b;
        }
        for n in record.nodes() {
            let mut fits = true;
            ty_children(*n, |_| {}, |l| fits &= ix.index_list(l));
            if !fits {
                b.fail(BuildError::TypeChild { node: *n });
                return b;
            }
        }
        for (i, n) in record.nodes().enumerate() {
            let key = map_ty_lists(*n, |l| ix.canonical_list(l));
            ix.nodes.get_or_insert_with(&key, |&k| (k, i as u32));
        }
        b.types = Some(ix);
        b
    }

    /// The handle of the type whose node is `node`: the record's when its table holds an
    /// equal node, else the body's own, made once (an overlay numbered after the record's
    /// types). Its children must be types the body has; a list in it, one from
    /// [`Self::intern_list`] or the record's. A hash lookup, not a scan.
    pub fn intern_ty(&mut self, node: PNode) -> Result<PTy, BuildError> {
        let r = match self.types.as_mut() {
            Some(ix) => ix.ty(&mut self.body, node),
            None => Err(BuildError::NoTypeTable),
        };
        if let Err(e) = r {
            self.fail(e);
        }
        r
    }

    /// The list whose entries are `entries`: the record's when it has one (a list some record
    /// node names), else the body's own, made once.
    pub fn intern_list(&mut self, entries: &[PTy]) -> Result<List, BuildError> {
        let r = match self.types.as_mut() {
            Some(ix) => ix.list(&mut self.body, entries),
            None => Err(BuildError::NoTypeTable),
        };
        if let Err(e) = r {
            self.fail(e);
        }
        r
    }

    /// The entries of any list the builder knows, the record's or its own; `None` past both
    /// or without a type table.
    pub fn list_entries(&self, l: List) -> Option<&[PTy]> {
        self.types.as_ref()?.entries.get(l.range())
    }

    /// An empty body, every table in `alloc`.
    pub fn new_in(alloc: A) -> BodyBuilder<A> {
        BodyBuilder {
            body: Body {
                locals: Vec::new_in(alloc),
                local_places: Vec::new_in(alloc),
                places: Vec::new_in(alloc),
                consts: Vec::new_in(alloc),
                data: Vec::new_in(alloc),
                callees: Vec::new_in(alloc),
                stmts: Vec::new_in(alloc),
                spans: Vec::new_in(alloc),
                stmt_block: Vec::new_in(alloc),
                blocks: Vec::new_in(alloc),
                operands: Vec::new_in(alloc),
                cases: Vec::new_in(alloc),
                loans: Vec::new_in(alloc),
                subsets: Vec::new_in(alloc),
                placeholder: Vec::new_in(alloc),
                local_origins: Vec::new_in(alloc),
                known: Vec::new_in(alloc),
                ty_base: NONE,
                list_base: NONE,
                tys: Vec::new_in(alloc),
                ty_lists: Vec::new_in(alloc),
                table: None,
            },
            filled: Vec::new_in(alloc),
            open: None,
            error: None,
            places_index: Map::new(MapOpts { hasher: FixedState, alloc, capacity: None }),
            consts_index: Map::new(MapOpts { hasher: FixedState, alloc, capacity: None }),
            bytes_index: Map::new(MapOpts { hasher: FixedState, alloc, capacity: None }),
            types: None,
        }
    }

    /// The first error so far.
    pub fn error(&self) -> Option<BuildError> {
        self.error
    }

    fn fail(&mut self, e: BuildError) {
        self.error.get_or_insert(e);
    }

    /// The body as built so far, to read.
    pub fn body(&self) -> &Body<A> {
        &self.body
    }

    /// A new local; its own place is made with it. Locals come in order: the return place,
    /// then the arguments (`Arg(0)`, `Arg(1)`, ..), then bindings, temporaries and flags.
    pub fn new_local(&mut self, decl: LocalDecl) -> Local {
        let n = match next(self.body.locals.len(), Table::Locals) {
            Ok(n) => n,
            Err(e) => {
                self.fail(e);
                return Local(NONE);
            }
        };
        let l = Local(n);
        let in_order = match decl.kind {
            LocalKind::Return => n == 0,
            LocalKind::Arg(i) => i.checked_add(1) == Some(n) && self.body.view().args() == i,
            LocalKind::DropFlag(of) => n > 0 && of.0 < n,
            LocalKind::Binding(_) | LocalKind::Temp(..) => n > 0,
        };
        if !in_order {
            self.fail(BuildError::LocalOrder(l));
        }
        self.body.locals.push(encode_local(decl));
        let p = match next(self.body.places.len(), Table::Places) {
            Ok(p) => p,
            Err(e) => {
                self.fail(e);
                return l;
            }
        };
        let row = PlaceRow { local: l, parent: None, proj: None, depth: 0, of_drop_type: false };
        self.body.places.push(encode_place(row));
        self.body.local_places.push(p);
        l
    }

    /// The place that is local `l` itself.
    pub fn local_place(&mut self, l: Local) -> Place {
        match self.body.local_places.get(l.index()) {
            Some(p) => Place(*p),
            None => {
                self.fail(BuildError::Refs(VerifyError::OutOfRange {
                    at: crate::verify::Site { table: Table::LocalPlaces, row: l.0 },
                    table: Table::Locals,
                    id: l.0,
                }));
                Place(NONE)
            }
        }
    }

    /// The place `parent` projected by `proj`, interned: one id per distinct place.
    pub fn project(&mut self, parent: Place, proj: Proj) -> Place {
        self.project_marked(parent, proj, false)
    }

    /// As [`Self::project`], marking a field of a type that implements `Drop`.
    pub fn project_marked(&mut self, parent: Place, proj: Proj, of_drop_type: bool) -> Place {
        let up = match self.body.view().place(parent) {
            Ok(up) => up,
            Err(_) => {
                self.fail(self.out_of_range(Table::Places, parent.0));
                return Place(NONE);
            }
        };
        if let Proj::Index(l) = proj {
            if l.0 >= self.body.view().locals() {
                self.fail(self.out_of_range(Table::Locals, l.0));
            }
        }
        let row = PlaceRow { local: up.local, parent: Some(parent), proj: Some(proj), depth: up.depth + 1, of_drop_type };
        // The row's words are the key: equal words (parent, projection, drop-type mark) are
        // the same place.
        let words = encode_place(row);
        if let Some(p) = self.places_index.get(&words) {
            return Place(*p);
        }
        match next(self.body.places.len(), Table::Places) {
            Ok(n) => {
                self.body.places.push(words);
                self.places_index.insert(words, n);
                Place(n)
            }
            Err(e) => {
                self.fail(e);
                Place(NONE)
            }
        }
    }

    fn out_of_range(&self, table: Table, id: u32) -> BuildError {
        let at = crate::verify::Site { table: Table::Stmts, row: self.body.stmts.len() as u32 };
        BuildError::Refs(VerifyError::OutOfRange { at, table, id })
    }

    /// A constant, interned: one id per distinct row (kind, words and type). A
    /// [`ConstKind::Bytes`] one names bytes already in the data table: [`Self::new_bytes`]
    /// adds them.
    pub fn new_const(&mut self, decl: ConstDecl) -> Const {
        if let Some(c) = self.consts_index.get(&encode_const(decl)) {
            return Const(*c);
        }
        self.push_const(decl)
    }

    /// A constant appended without a lookup, indexed when its row is new: the text form's
    /// declarations, which may repeat a constant and keep their numbers.
    pub(crate) fn push_const(&mut self, decl: ConstDecl) -> Const {
        if let ConstKind::Bytes { start, len } = decl.kind {
            if start as usize + len as usize > self.body.data.len() {
                self.fail(self.out_of_range(Table::Data, start.saturating_add(len)));
            }
        }
        match next(self.body.consts.len(), Table::Consts) {
            Ok(n) => {
                let words = encode_const(decl);
                self.body.consts.push(words);
                if self.consts_index.get(&words).is_none() {
                    self.consts_index.insert(words, n);
                }
                Const(n)
            }
            Err(e) => {
                self.fail(e);
                Const(NONE)
            }
        }
    }

    /// A string or byte-string constant of type `ty`, interned by its bytes and type: new
    /// bytes go into the data table.
    pub fn new_bytes(&mut self, bytes: &[u8], ty: PTy) -> Const {
        let h = FixedState.hash_one((bytes, ty.0));
        let mut k = 0u32;
        while let Some(c) = self.bytes_index.get(&(h, k)).copied() {
            if let Some(&w) = self.body.consts.get(c as usize) {
                let (start, len) = (w.a as usize, w.b as usize);
                if w.ty == ty.0 && self.body.data.get(start..start + len) == Some(bytes) {
                    return Const(c);
                }
            }
            k += 1;
        }
        let c = self.push_bytes(bytes, ty);
        if c.0 != NONE {
            self.bytes_index.insert((h, k), c.0);
        }
        c
    }

    /// A string constant appended without a lookup: the text form's declarations.
    pub(crate) fn push_bytes(&mut self, bytes: &[u8], ty: PTy) -> Const {
        let (start, len) = match (next(self.body.data.len(), Table::Data), u32::try_from(bytes.len())) {
            (Ok(s), Ok(l)) if s.checked_add(l).is_some_and(|e| e < NONE) => (s, l),
            _ => {
                self.fail(BuildError::Full(Table::Data));
                return Const(NONE);
            }
        };
        self.body.data.extend_from_slice(bytes);
        self.push_const(ConstDecl { kind: ConstKind::Bytes { start, len }, ty })
    }

    /// Marks local `l`'s own place as of a type that implements `Drop` (a projection's mark
    /// is [`Self::project_marked`]'s).
    pub fn mark_local_drop_type(&mut self, l: Local) {
        let p = self.local_place(l);
        if let Some(w) = self.body.places.get_mut(p.index()) {
            w.flags |= 1;
        }
    }

    /// The text form's overlay header: the record's type and list-entry counts.
    pub(crate) fn set_type_bases(&mut self, ty_base: u32, list_base: u32) {
        (self.body.ty_base, self.body.list_base) = (ty_base, list_base);
    }

    /// An overlay row as the text form wrote it; verify checks it.
    pub(crate) fn push_ty_row(&mut self, w: TyWords) {
        self.body.tys.push(w);
    }

    /// An overlay list entry as the text form wrote it; verify checks it.
    pub(crate) fn push_ty_entry(&mut self, t: PTy) {
        self.body.ty_lists.push(t.0);
    }

    /// A new callee row, for one call.
    pub fn callee(&mut self, c: Callee) -> CalleeId {
        if let Callee::Value(o) = c {
            self.operand_in_range(o);
        }
        match next(self.body.callees.len(), Table::Callees) {
            Ok(n) => {
                self.body.callees.push(encode_callee(c));
                CalleeId(n)
            }
            Err(e) => {
                self.fail(e);
                CalleeId(NONE)
            }
        }
    }

    fn operand_in_range(&mut self, o: Operand) {
        let v = self.body.view();
        let bad = match o {
            Operand::Copy(p) | Operand::Move(p) => (p.0 >= v.places()).then_some((Table::Places, p.0)),
            Operand::Const(c) => (c.0 >= v.consts()).then_some((Table::Consts, c.0)),
        };
        if let Some((table, id)) = bad {
            self.fail(self.out_of_range(table, id));
        }
    }

    /// An operand list (a call's arguments, an aggregate's fields, an assert's message).
    pub fn operands(&mut self, ops: &[Operand]) -> Ops {
        for o in ops {
            self.operand_in_range(*o);
        }
        let start = self.body.operands.len() as u32;
        for o in ops {
            self.body.operands.push(encode_operand(*o));
        }
        let end = self.body.operands.len() as u32;
        if next(self.body.operands.len(), Table::Operands).is_err() {
            self.fail(BuildError::Full(Table::Operands));
        }
        Ops { start, end }
    }

    /// A switch's cases, sorted by value (a value given twice stays twice: verify refuses it).
    pub fn cases(&mut self, cases: &[(u128, Block)]) -> Ops {
        let start = self.body.cases.len();
        for &(value, b) in cases {
            if b.0 >= self.body.view().blocks() {
                self.fail(self.out_of_range(Table::Blocks, b.0));
            }
            self.body.cases.push(encode_case(value, b));
        }
        if let Some(slice) = self.body.cases.get_mut(start..) {
            slice.sort_unstable_by_key(|c| join(c.value));
        }
        if next(self.body.cases.len(), Table::Cases).is_err() {
            self.fail(BuildError::Full(Table::Cases));
        }
        Ops { start: start as u32, end: self.body.cases.len() as u32 }
    }

    /// A new origin; a placeholder is a lifetime of the signature.
    pub fn origin(&mut self, placeholder: bool) -> Origin {
        match next(self.body.placeholder.len(), Table::Origins) {
            Ok(n) => {
                self.body.placeholder.push(u8::from(placeholder));
                Origin(n)
            }
            Err(e) => {
                self.fail(e);
                Origin(NONE)
            }
        }
    }

    /// The type of `local` mentions `origin`.
    pub fn mentions(&mut self, local: Local, origin: Origin) {
        self.local_in_range(local);
        self.origin_in_range(origin);
        let row = PairWords { a: local.0, b: origin.0 };
        if let Err(at) = self.body.local_origins.binary_search(&row) {
            self.body.local_origins.insert(at, row);
        }
    }

    /// The signature says `from: into` between two placeholders.
    pub fn known(&mut self, from: Origin, into: Origin) {
        self.origin_in_range(from);
        self.origin_in_range(into);
        self.body.known.push(PairWords { a: from.0, b: into.0 });
    }

    fn local_in_range(&mut self, l: Local) {
        if l.0 >= self.body.view().locals() {
            self.fail(self.out_of_range(Table::Locals, l.0));
        }
    }

    fn origin_in_range(&mut self, o: Origin) {
        if o.0 >= self.body.view().origins() {
            self.fail(self.out_of_range(Table::Origins, o.0));
        }
    }

    /// A new block, to be filled later with [`Self::start`].
    pub fn new_block(&mut self, kind: BlockKind) -> Block {
        match next(self.body.blocks.len(), Table::Blocks) {
            Ok(n) => {
                self.body.blocks.push(encode_block(BlockRow { stmts: Ops { start: 0, end: 0 }, kind }));
                self.filled.push(false);
                Block(n)
            }
            Err(e) => {
                self.fail(e);
                Block(NONE)
            }
        }
    }

    /// Starts filling `b`: every row until its terminator goes into it.
    pub fn start(&mut self, b: Block) {
        if let Some(open) = self.open {
            self.fail(BuildError::BlockOpen { open, start: b });
        }
        match self.filled.get(b.index()) {
            Some(false) => {}
            Some(true) => self.fail(BuildError::BlockFilled(b)),
            None => {
                self.fail(self.out_of_range(Table::Blocks, b.0));
                return;
            }
        }
        self.open = Some(b);
        let at = self.body.stmts.len() as u32;
        if let Some(slot) = self.body.blocks.get_mut(b.index()) {
            slot.start = at;
            slot.end = at;
        }
    }

    fn row(&mut self, kind: StmtKind, span: Span) -> Stmt {
        let at = Stmt(self.body.stmts.len() as u32);
        let Some(b) = self.open else {
            self.fail(BuildError::NotInBlock);
            return at;
        };
        if let Err(e) = next(self.body.stmts.len(), Table::Stmts) {
            self.fail(e);
            return at;
        }
        if let Err(e) = check_refs(&self.body.view(), at, &kind) {
            self.fail(BuildError::Refs(e));
        }
        self.body.stmts.push(encode_stmt(kind));
        self.body.spans.push(span);
        self.body.stmt_block.push(b.0);
        if let Some(slot) = self.body.blocks.get_mut(b.index()) {
            slot.end = at.0 + 1;
        }
        if kind.is_terminator() {
            if let Some(slot) = self.filled.get_mut(b.index()) {
                *slot = true;
            }
            self.open = None;
        }
        at
    }

    /// Adds a statement (not a terminator) to the open block.
    pub fn push(&mut self, kind: StmtKind, span: Span) -> Stmt {
        let at = Stmt(self.body.stmts.len() as u32);
        if kind.is_terminator() {
            self.fail(BuildError::NotAStatement(at));
            return at;
        }
        if let StmtKind::Assign { value: Value::Borrow { .. }, .. } = kind {
            self.fail(BuildError::BorrowWithoutLoan(at));
            return at;
        }
        self.row(kind, span)
    }

    /// `dest = value` (a value that is not a borrow: [`Self::borrow`] makes those).
    pub fn assign(&mut self, dest: Place, value: Value, span: Span) -> Stmt {
        self.push(StmtKind::Assign { dest, value }, span)
    }

    /// `dest = &kind place`, issuing a new loan into `origin`.
    pub fn borrow(&mut self, dest: Place, kind: BorrowKind, place: Place, origin: Origin, span: Span) -> Loan {
        let at = Stmt(self.body.stmts.len() as u32);
        self.origin_in_range(origin);
        let loan = match next(self.body.loans.len(), Table::Loans) {
            Ok(n) => Loan(n),
            Err(e) => {
                self.fail(e);
                return Loan(NONE);
            }
        };
        self.body.loans.push(encode_loan(LoanRow { place, kind, origin, at }));
        self.row(StmtKind::Assign { dest, value: Value::Borrow { kind, place, loan } }, span);
        loan
    }

    /// `subset_base(from, into)` at the Mid of the row added last: `from` outlives `into`.
    pub fn subset(&mut self, from: Origin, into: Origin) {
        self.origin_in_range(from);
        self.origin_in_range(into);
        match self.body.stmts.len().checked_sub(1) {
            Some(s) => self.body.subsets.push(SubsetWords { at: s as u32, from: from.0, into: into.0 }),
            None => self.fail(BuildError::NoStatement),
        }
    }

    /// Ends the open block with a terminator.
    pub fn terminate(&mut self, kind: StmtKind, span: Span) -> Stmt {
        let at = Stmt(self.body.stmts.len() as u32);
        if !kind.is_terminator() {
            self.fail(BuildError::NotATerminator(at));
            return at;
        }
        self.row(kind, span)
    }

    /// The body, or the first error.
    pub fn finish(self) -> Result<Body<A>, BuildError> {
        if let Some(e) = self.error {
            return Err(e);
        }
        if let Some(b) = self.open {
            return Err(BuildError::Unterminated(b));
        }
        if self.body.blocks.is_empty() {
            return Err(BuildError::Empty);
        }
        if let Some(b) = self.filled.iter().position(|f| !f) {
            return Err(BuildError::UnfilledBlock(Block(b as u32)));
        }
        Ok(self.body)
    }
}
