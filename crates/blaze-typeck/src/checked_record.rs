//! The checked body as one record (D163 request R1; typeck.md, "Results: one record in
//! columns"): what a run reads in place of checking the body again (blaze-ladder's
//! front2/program/lazy/typed.rs `TypedRecords::typed`).
//!
//! The record is the columns of a CLEAN check that later passes read: the type table (nodes,
//! list entries and the list rows, so a decode lays every type and list at the index the check
//! gave it), the per-node types, and the sparse tables. It holds no diagnostic, suggestion,
//! stalled goal, finding, undecided place or read list: a record is kept only after a clean
//! check ([`encode`] refuses any other), a finding is a record of its own (rule 81), and the
//! reads are the record's key, not its value (rule 51).
//!
//! The key (decided, rule 21): `typed_key(check, out)`, the check's name AND the body row's
//! `out` digest, never the name alone, so two checks of one name with other answers are two
//! records.
//!
//! A body whose check left stalled goals is decided later (blaze-ladder front2
//! stalled_goals.rs). When every one held on its exact interior (`Decision::Proved`) the body
//! keeps the record a clean check keeps ([`encode_decided`]: no column holds stalled rows, and a
//! run reads none; F18). Any other decision files a diagnostic, a gap or a refusal, and
//! [`encode`] refuses its stalled rows (`NotClean { what: "stalled" }`): the body's
//! `BodyReads::typed_stalled` (front2 tables/task.rs) is set and a run's census counts it.
//!
//! Layout, little-endian, nothing of the machine (rule 25):
//!
//! ```text
//! magic "bzchkd06" | u32 column count (28) | u32 rows per column, in [`Col`] order
//! | each column's rows, fixed width ([`Col::width`]), unused bytes zero
//! ```
//!
//! Every column is a run of fixed-width rows at an offset the header gives, so [`CheckedRecord`]
//! reads a row in place by index (rule 51); [`CheckedRecord::checked_in`] builds the owned
//! [`Checked`] the checker's readers take. A decoded record re-encodes to equal bytes (rule 26),
//! which the suite's `checked_record` tests hold, with planted controls.

use blaze_own::{Alloc, Soa, Vec};

use crate::columns::{AdjustKind, CaptureKind, Checked, CoroutineWitness, SigFrom, UnionPat};
use crate::error::CheckError;
use crate::ids::{ExprId, FloatTy, FnPtrHeader, IntTy, ItemKey, LangItem, List, Mutbl, Name, PatId, PathIdx, RecordSchema, ScopeId, UintTy};
use crate::portable::{PNode, PTy, PortableTys};
use crate::rules::suggest::Suggests;
use crate::selected::{Projected, Selected};
use crate::ty::ClosureKind;
use crate::world::{CtorKind, Def, Needs, Prim, Question, ReadList, Shape};

/// The record's first bytes: its form and version. A record of another version is refused by
/// name ([`CheckedRecordError::Magic`]), never read.
/// 02: closure captures are places (capture_places.rs): the capture rows, the closure kinds and
/// the environment tuples (`closures` column 3) are the places', not the whole bindings'.
/// 03: a selection row names a non-impl source (`Selected` tags 4 to 7: `ParamEnv`,
/// `OpaqueBound`, `ObjectBound`, `Builtin`).
/// 04: the captured places themselves (`capture_places`, `capture_projs`: one row per
/// environment field of each closure the place analysis decided, RFC 2229).
/// 05: a decided async block that suspends is `NeedsSavedLocals` (its interior's captures are
/// its places, rules/closure.rs `analyze_closures`), no longer `NeedsCapturePlaces`; and a
/// borrow of a `repr(packed)` struct's field captures the struct (capture_places.rs
/// `packed_field`).
/// 06: a type node may be an extern type (`PNode::Foreign`, node tag 30).
/// 07: structural record schemas/names, union pattern narrowing (`union_pats`), and union
/// coercion adjustments (`AdjustKind::UnionInject`, `AdjustKind::UnionWiden`).
pub const CHECKED_RECORD_MAGIC: [u8; 8] = *b"bzchkd07";

/// The record's columns, in the order the header and the body lay them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Col {
    TyNodes,
    TyLists,
    TyListRows,
    ExprTy,
    PatTy,
    LocalTy,
    Adjusts,
    Callees,
    ExprDefs,
    ConstArgs,
    PatConstArgs,
    PatDefs,
    Fields,
    Captures,
    Closures,
    Hidden,
    Promoted,
    CoroutineWitnesses,
    Awaited,
    Selected,
    FruFields,
    DefaultFields,
    IndexMut,
    NeedQuestions,
    NeedMissing,
    NeedFirst,
    CapturePlaces,
    CaptureProjs,
    TyRecordSchemas,
    TyRecordNames,
    UnionPats,
}

/// Every column, in layout order.
pub const COLS: [Col; 31] = [
    Col::TyNodes,
    Col::TyLists,
    Col::TyListRows,
    Col::ExprTy,
    Col::PatTy,
    Col::LocalTy,
    Col::Adjusts,
    Col::Callees,
    Col::ExprDefs,
    Col::ConstArgs,
    Col::PatConstArgs,
    Col::PatDefs,
    Col::Fields,
    Col::Captures,
    Col::Closures,
    Col::Hidden,
    Col::Promoted,
    Col::CoroutineWitnesses,
    Col::Awaited,
    Col::Selected,
    Col::FruFields,
    Col::DefaultFields,
    Col::IndexMut,
    Col::NeedQuestions,
    Col::NeedMissing,
    Col::NeedFirst,
    Col::CapturePlaces,
    Col::CaptureProjs,
    Col::TyRecordSchemas,
    Col::TyRecordNames,
    Col::UnionPats,
];

/// A type node: a tag and at most 36 payload bytes (`ExProj`).
const NODE: usize = 40;
/// A [`Def`]: tag, two sub-bytes, a pad byte, a `u32`, a key.
const DEF: usize = 24;
/// A [`Shape`]: tag, a sub-byte or flag, a `u32` or a key.
const SHAPE: usize = 18;
/// A [`Question`]: tag, a key, a [`Shape`] (`ImplsOf`).
const QUESTION: usize = 1 + 16 + SHAPE;
/// The widest row (`Selected`), the stack buffer a row is written in.
const MOST_WIDTH: usize = 72;

impl Col {
    /// The bytes of one row.
    pub const fn width(self) -> usize {
        match self {
            Col::TyNodes => NODE,
            Col::TyLists => 4,
            Col::TyListRows => 8,
            Col::ExprTy | Col::PatTy | Col::LocalTy | Col::Promoted | Col::IndexMut => 4,
            Col::Adjusts => 4 + 2 + 4 + 4,
            Col::Callees => 4 + 16 + 8,
            Col::ExprDefs | Col::PatDefs => 4 + DEF,
            Col::ConstArgs | Col::PatConstArgs => 4 + 8,
            Col::Fields => 4 + 4 + 4,
            Col::Captures => 4 + 4 + 1,
            Col::CapturePlaces => 4 + 4 + 4 + 4 + 1 + 4,
            Col::CaptureProjs => 1 + 4 + 4 + 4,
            Col::Closures => 4 + 1 + 4 + 5 + 14,
            Col::Hidden => 16 + 4,
            Col::CoroutineWitnesses => 4 + 1,
            Col::Awaited => 4 + 4,
            Col::Selected => 4 + 4 + 16 + 9 + 8 + 1 + 25,
            Col::FruFields => 4 + 4 + 4,
            Col::DefaultFields => 4 + 4 + 16,
            Col::NeedQuestions | Col::NeedMissing | Col::NeedFirst => QUESTION,
            Col::TyRecordSchemas => 8 + 8,
            Col::TyRecordNames => 8,
            Col::UnionPats => 4 + 1 + 4,
        }
    }

    /// The column's name, as errors print it (`Checked`'s field it holds).
    pub const fn name(self) -> &'static str {
        match self {
            Col::TyNodes => "types.nodes",
            Col::TyLists => "types.lists",
            Col::TyListRows => "types.list_rows",
            Col::ExprTy => "expr_ty",
            Col::PatTy => "pat_ty",
            Col::LocalTy => "local_ty",
            Col::Adjusts => "adjusts",
            Col::Callees => "callees",
            Col::ExprDefs => "expr_defs",
            Col::ConstArgs => "const_args",
            Col::PatConstArgs => "pat_const_args",
            Col::PatDefs => "pat_defs",
            Col::Fields => "fields",
            Col::Captures => "captures",
            Col::CapturePlaces => "capture_places",
            Col::CaptureProjs => "capture_projs",
            Col::Closures => "closures",
            Col::Hidden => "hidden",
            Col::Promoted => "promoted",
            Col::CoroutineWitnesses => "coroutine_witnesses",
            Col::Awaited => "awaited",
            Col::Selected => "selected",
            Col::FruFields => "fru_fields",
            Col::DefaultFields => "default_fields",
            Col::IndexMut => "index_mut",
            Col::NeedQuestions => "needs.questions",
            Col::NeedMissing => "needs.missing",
            Col::NeedFirst => "needs.first",
            Col::TyRecordSchemas => "types.record_schemas",
            Col::TyRecordNames => "types.record_names",
            Col::UnionPats => "union_pats",
        }
    }

    const fn at(self) -> usize {
        self as usize
    }
}

/// Why a checked body was not written as a record, or a record not read: each names the
/// column and row (rule 19), under the record's rule (rule 26: a record round-trips or is
/// refused).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckedRecordError {
    /// The check was not clean: a record is kept only of a body with no diagnostic, stalled
    /// goal or undecided place (`what` names which column was not empty).
    NotClean { what: &'static str, rows: u32 },
    /// A column longer than a `u32` counts.
    TooBig { column: &'static str },
    /// A row's payload past its width: the encoder's own bug.
    Width { column: &'static str, row: u32 },
    /// The bytes do not start with [`CHECKED_RECORD_MAGIC`].
    Magic,
    /// The header names `have` columns where this version has 28.
    Columns { have: u32 },
    /// The bytes are `have` long where the header's columns need `need`.
    Length { need: u64, have: u64 },
    /// A tag (or a sub-byte, a flag) no value of the column's type has.
    Tag { column: &'static str, row: u32, tag: u8 },
    /// A type id at or past the type table's length, or a node's child not before it.
    TypeOrder { column: &'static str, row: u32, ty: u32 },
    /// A list outside the type table's list entries.
    ListOutside { column: &'static str, row: u32, start: u32, len: u32 },
    /// The list rows do not cover the list entries in order: row `row` starts at `start`
    /// where the rows before it end at `expected`.
    ListRows { row: u32, start: u32, expected: u32 },
    /// Re-interning the type table did not give the record's index back (a repeated node or
    /// list: the table was not hash-consed).
    NotCanonical { column: &'static str, row: u32 },
    /// The type table refused a node or list.
    Types(CheckError),
}

impl core::fmt::Display for CheckedRecordError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        const AT: &str = "checked-body record (blaze-typeck checked_record.rs, rule 26)";
        match *self {
            CheckedRecordError::NotClean { what, rows } => write!(f, "{AT}: the check is not clean ({rows} rows of `{what}`): no record is kept of it"),
            CheckedRecordError::TooBig { column } => write!(f, "{AT}: column `{column}` has more rows than a u32 counts"),
            CheckedRecordError::Width { column, row } => write!(f, "{AT}: row {row} of `{column}` is wider than its column"),
            CheckedRecordError::Magic => write!(f, "{AT}: the bytes are not a checked-body record of this version"),
            CheckedRecordError::Columns { have } => write!(f, "{AT}: the header names {have} columns, this version {}", COLS.len()),
            CheckedRecordError::Length { need, have } => write!(f, "{AT}: {have} bytes where the header's columns need {need}"),
            CheckedRecordError::Tag { column, row, tag } => write!(f, "{AT}: row {row} of `{column}` has tag {tag}, which no value has"),
            CheckedRecordError::TypeOrder { column, row, ty } => write!(f, "{AT}: row {row} of `{column}` names type {ty}, not in the table before it"),
            CheckedRecordError::ListOutside { column, row, start, len } => {
                write!(f, "{AT}: row {row} of `{column}` names the list {start}+{len}, outside the list entries")
            }
            CheckedRecordError::ListRows { row, start, expected } => write!(f, "{AT}: list row {row} starts at {start}, where the rows before it end at {expected}"),
            CheckedRecordError::NotCanonical { column, row } => write!(f, "{AT}: row {row} of `{column}` is not at the index re-interning gives it"),
            CheckedRecordError::Types(e) => write!(f, "{AT}: the type table refused a row: {e:?}"),
        }
    }
}

type E = CheckedRecordError;

fn rows_u32(n: usize, column: &'static str) -> Result<u32, E> {
    u32::try_from(n).map_err(|_| E::TooBig { column })
}

/// One row being written: a stack buffer, its unused bytes zero.
struct Put {
    buf: [u8; MOST_WIDTH],
    at: usize,
    over: bool,
}

impl Put {
    fn new() -> Put {
        Put { buf: [0; MOST_WIDTH], at: 0, over: false }
    }
    fn bytes(&mut self, b: &[u8]) {
        match self.buf.get_mut(self.at..self.at + b.len()) {
            Some(to) => to.copy_from_slice(b),
            None => self.over = true,
        }
        self.at += b.len();
    }
    fn u8(&mut self, v: u8) {
        self.bytes(&[v]);
    }
    fn u32(&mut self, v: u32) {
        self.bytes(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.bytes(&v.to_le_bytes());
    }
    fn u128(&mut self, v: u128) {
        self.bytes(&v.to_le_bytes());
    }
    fn key(&mut self, k: ItemKey) {
        self.u128(k.0);
    }
    fn ty(&mut self, t: PTy) {
        self.u32(t.0);
    }
    fn list(&mut self, l: List) {
        self.u32(l.start);
        self.u32(l.len);
    }
    /// Skips to `at` (zeros): a variant shorter than its column.
    fn to(&mut self, at: usize) {
        if self.at > at {
            self.over = true;
        }
        self.at = at;
    }
    /// The row, `col.width()` bytes, appended to `out`.
    fn finish(self, col: Col, row: usize, out: &mut Vec<u8, Alloc<'_>>) -> Result<(), E> {
        let width = col.width();
        if self.over || self.at > width {
            return Err(E::Width { column: col.name(), row: u32::try_from(row).unwrap_or(u32::MAX) });
        }
        out.extend_from_slice(&self.buf[..width]);
        Ok(())
    }
}

/// One row being read, in place.
struct Get<'r> {
    row: &'r [u8],
    at: usize,
    col: Col,
    n: u32,
}

impl<'r> Get<'r> {
    fn take<const N: usize>(&mut self) -> Result<[u8; N], E> {
        let b = self.row.get(self.at..self.at + N).ok_or(E::Width { column: self.col.name(), row: self.n })?;
        self.at += N;
        let mut out = [0; N];
        out.copy_from_slice(b);
        Ok(out)
    }
    fn u8(&mut self) -> Result<u8, E> {
        Ok(self.take::<1>()?[0])
    }
    fn u32(&mut self) -> Result<u32, E> {
        Ok(u32::from_le_bytes(self.take()?))
    }
    fn u64(&mut self) -> Result<u64, E> {
        Ok(u64::from_le_bytes(self.take()?))
    }
    fn u128(&mut self) -> Result<u128, E> {
        Ok(u128::from_le_bytes(self.take()?))
    }
    fn key(&mut self) -> Result<ItemKey, E> {
        Ok(ItemKey(self.u128()?))
    }
    fn raw_ty(&mut self) -> Result<PTy, E> {
        Ok(PTy(self.u32()?))
    }
    fn raw_list(&mut self) -> Result<List, E> {
        Ok(List { start: self.u32()?, len: self.u32()? })
    }
    fn to(&mut self, at: usize) {
        self.at = at;
    }
    fn bad(&self, tag: u8) -> E {
        E::Tag { column: self.col.name(), row: self.n, tag }
    }
}

// ---- small enums ---------------------------------------------------------------------------

fn mutbl_tag(m: Mutbl) -> u8 {
    match m {
        Mutbl::Not => 0,
        Mutbl::Mut => 1,
    }
}
fn mutbl_of(g: &Get<'_>, t: u8) -> Result<Mutbl, E> {
    match t {
        0 => Ok(Mutbl::Not),
        1 => Ok(Mutbl::Mut),
        _ => Err(g.bad(t)),
    }
}
fn int_tag(i: IntTy) -> u8 {
    match i {
        IntTy::Isize => 0,
        IntTy::I8 => 1,
        IntTy::I16 => 2,
        IntTy::I32 => 3,
        IntTy::I64 => 4,
        IntTy::I128 => 5,
    }
}
fn int_of(g: &Get<'_>, t: u8) -> Result<IntTy, E> {
    Ok(match t {
        0 => IntTy::Isize,
        1 => IntTy::I8,
        2 => IntTy::I16,
        3 => IntTy::I32,
        4 => IntTy::I64,
        5 => IntTy::I128,
        _ => return Err(g.bad(t)),
    })
}
fn uint_tag(u: UintTy) -> u8 {
    match u {
        UintTy::Usize => 0,
        UintTy::U8 => 1,
        UintTy::U16 => 2,
        UintTy::U32 => 3,
        UintTy::U64 => 4,
        UintTy::U128 => 5,
    }
}
fn uint_of(g: &Get<'_>, t: u8) -> Result<UintTy, E> {
    Ok(match t {
        0 => UintTy::Usize,
        1 => UintTy::U8,
        2 => UintTy::U16,
        3 => UintTy::U32,
        4 => UintTy::U64,
        5 => UintTy::U128,
        _ => return Err(g.bad(t)),
    })
}
fn float_tag(f: FloatTy) -> u8 {
    match f {
        FloatTy::F16 => 0,
        FloatTy::F32 => 1,
        FloatTy::F64 => 2,
        FloatTy::F128 => 3,
    }
}
fn float_of(g: &Get<'_>, t: u8) -> Result<FloatTy, E> {
    Ok(match t {
        0 => FloatTy::F16,
        1 => FloatTy::F32,
        2 => FloatTy::F64,
        3 => FloatTy::F128,
        _ => return Err(g.bad(t)),
    })
}
fn bool_of(g: &Get<'_>, t: u8) -> Result<bool, E> {
    match t {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(g.bad(t)),
    }
}

// ---- types ---------------------------------------------------------------------------------

fn put_node(p: &mut Put, n: PNode) {
    match n {
        PNode::Bool => p.u8(0),
        PNode::Char => p.u8(1),
        PNode::Int(i) => {
            p.u8(2);
            p.u8(int_tag(i));
        }
        PNode::Uint(u) => {
            p.u8(3);
            p.u8(uint_tag(u));
        }
        PNode::Float(x) => {
            p.u8(4);
            p.u8(float_tag(x));
        }
        PNode::Str => p.u8(5),
        PNode::Never => p.u8(6),
        PNode::Tuple(l) => {
            p.u8(7);
            p.list(l);
        }
        PNode::Ref(m, t) => {
            p.u8(8);
            p.u8(mutbl_tag(m));
            p.ty(t);
        }
        PNode::Ptr(m, t) => {
            p.u8(9);
            p.u8(mutbl_tag(m));
            p.ty(t);
        }
        PNode::Array(e, len) => {
            p.u8(10);
            p.ty(e);
            p.ty(len);
        }
        PNode::Slice(t) => {
            p.u8(11);
            p.ty(t);
        }
        PNode::Pat(base, ranges) => {
            p.u8(12);
            p.ty(base);
            p.list(ranges);
        }
        PNode::Adt(k, l) => {
            p.u8(13);
            p.key(k);
            p.list(l);
        }
        PNode::FnDef(k, l) => {
            p.u8(14);
            p.key(k);
            p.list(l);
        }
        PNode::FnPtr(l, h) => {
            p.u8(15);
            p.u8(h.bits());
            p.list(l);
        }
        PNode::Closure { owner, ordinal, args } => {
            p.u8(16);
            p.key(owner);
            p.u32(ordinal);
            p.list(args);
        }
        PNode::Future(e, out, interior) => {
            p.u8(17);
            p.u32(e.0);
            p.ty(out);
            p.ty(interior);
        }
        PNode::Coroutine { owner, ordinal, args } => {
            p.u8(18);
            p.key(owner);
            p.u32(ordinal);
            p.list(args);
        }
        PNode::Param(i) => {
            p.u8(19);
            p.u32(i);
        }
        PNode::Alias { trait_, name, args } => {
            p.u8(20);
            p.key(trait_);
            p.u64(name.0);
            p.list(args);
        }
        PNode::Opaque(k, l) => {
            p.u8(21);
            p.key(k);
            p.list(l);
        }
        PNode::Infer => p.u8(22),
        PNode::Error => p.u8(23),
        PNode::ConstValue { ty, bits } => {
            p.u8(24);
            p.ty(ty);
            p.u128(bits);
        }
        PNode::ConstParam(i) => {
            p.u8(25);
            p.u32(i);
        }
        PNode::Dynamic(l) => {
            p.u8(26);
            p.list(l);
        }
        PNode::ExTrait(k, l) => {
            p.u8(27);
            p.key(k);
            p.list(l);
        }
        PNode::ExProj { trait_, name, args, term } => {
            p.u8(28);
            p.key(trait_);
            p.u64(name.0);
            p.list(args);
            p.ty(term);
        }
        PNode::ExAuto(k) => {
            p.u8(29);
            p.key(k);
        }
        PNode::Foreign(k) => {
            p.u8(30);
            p.key(k);
        }
        PNode::Record(s, l) => {
            p.u8(31);
            p.u64(s.0);
            p.list(l);
        }
        PNode::Union(l) => {
            p.u8(32);
            p.list(l);
        }
    }
}

/// What a decode checks every type id and list against: the table's lengths.
#[derive(Clone, Copy)]
struct Bounds {
    nodes: u32,
    lists: u32,
}

impl Bounds {
    /// A type id of a column (`below`: the node's own index, for a child).
    fn ty(self, g: &mut Get<'_>, below: u32) -> Result<PTy, E> {
        let t = g.raw_ty()?;
        if t.0 >= below.min(self.nodes) {
            return Err(E::TypeOrder { column: g.col.name(), row: g.n, ty: t.0 });
        }
        Ok(t)
    }
    fn list(self, g: &mut Get<'_>) -> Result<List, E> {
        let l = g.raw_list()?;
        if u64::from(l.start) + u64::from(l.len) > u64::from(self.lists) {
            return Err(E::ListOutside { column: g.col.name(), row: g.n, start: l.start, len: l.len });
        }
        Ok(l)
    }
    fn opt_ty(self, g: &mut Get<'_>) -> Result<Option<PTy>, E> {
        let flag = g.u8()?;
        if !bool_of(g, flag)? {
            // An absent type is written as id 0 (zeros), checked against nothing.
            let _ = g.raw_ty()?;
            return Ok(None);
        }
        Ok(Some(self.ty(g, u32::MAX)?))
    }
}

fn get_node(g: &mut Get<'_>, b: Bounds) -> Result<PNode, E> {
    let me = g.n;
    let tag = g.u8()?;
    Ok(match tag {
        0 => PNode::Bool,
        1 => PNode::Char,
        2 => {
            let t = g.u8()?;
            PNode::Int(int_of(g, t)?)
        }
        3 => {
            let t = g.u8()?;
            PNode::Uint(uint_of(g, t)?)
        }
        4 => {
            let t = g.u8()?;
            PNode::Float(float_of(g, t)?)
        }
        5 => PNode::Str,
        6 => PNode::Never,
        7 => PNode::Tuple(b.list(g)?),
        8 | 9 => {
            let t = g.u8()?;
            let m = mutbl_of(g, t)?;
            let to = b.ty(g, me)?;
            if tag == 8 { PNode::Ref(m, to) } else { PNode::Ptr(m, to) }
        }
        10 => PNode::Array(b.ty(g, me)?, b.ty(g, me)?),
        11 => PNode::Slice(b.ty(g, me)?),
        12 => PNode::Pat(b.ty(g, me)?, b.list(g)?),
        13 => PNode::Adt(g.key()?, b.list(g)?),
        14 => PNode::FnDef(g.key()?, b.list(g)?),
        15 => {
            let h = g.u8()?;
            let header = FnPtrHeader::from_bits(h).ok_or(g.bad(h))?;
            PNode::FnPtr(b.list(g)?, header)
        }
        16 => PNode::Closure { owner: g.key()?, ordinal: g.u32()?, args: b.list(g)? },
        17 => PNode::Future(ExprId(g.u32()?), b.ty(g, me)?, b.ty(g, me)?),
        18 => PNode::Coroutine { owner: g.key()?, ordinal: g.u32()?, args: b.list(g)? },
        19 => PNode::Param(g.u32()?),
        20 => PNode::Alias { trait_: g.key()?, name: Name(g.u64()?), args: b.list(g)? },
        21 => PNode::Opaque(g.key()?, b.list(g)?),
        22 => PNode::Infer,
        23 => PNode::Error,
        24 => PNode::ConstValue { ty: b.ty(g, me)?, bits: g.u128()? },
        25 => PNode::ConstParam(g.u32()?),
        26 => PNode::Dynamic(b.list(g)?),
        27 => PNode::ExTrait(g.key()?, b.list(g)?),
        28 => PNode::ExProj { trait_: g.key()?, name: Name(g.u64()?), args: b.list(g)?, term: b.ty(g, me)? },
        29 => PNode::ExAuto(g.key()?),
        30 => PNode::Foreign(g.key()?),
        31 => PNode::Record(RecordSchema(g.u64()?), b.list(g)?),
        32 => PNode::Union(b.list(g)?),
        _ => return Err(g.bad(tag)),
    })
}

// ---- rows of the sparse tables -------------------------------------------------------------

fn put_adjust(p: &mut Put, k: AdjustKind) {
    let (tag, arg, variant) = match k {
        AdjustKind::NeverToAny => (0, 0, 0),
        AdjustKind::Deref { overloaded: None } => (1, 0, 0),
        AdjustKind::Deref { overloaded: Some(Mutbl::Not) } => (1, 1, 0),
        AdjustKind::Deref { overloaded: Some(Mutbl::Mut) } => (1, 2, 0),
        AdjustKind::Borrow { mutbl, two_phase, raw } => {
            (2, mutbl_tag(mutbl) | u8::from(two_phase) << 1 | u8::from(raw) << 2, 0)
        }
        AdjustKind::ReifyFnPointer => (3, 0, 0),
        AdjustKind::ClosureFnPointer => (4, 0, 0),
        AdjustKind::UnsafeFnPointer => (5, 0, 0),
        AdjustKind::MutToConstPointer => (6, 0, 0),
        AdjustKind::Unsize => (7, 0, 0),
        AdjustKind::UnionInject { variant } => (8, 0, variant),
        AdjustKind::UnionWiden => (9, 0, 0),
        AdjustKind::RecordProject => (10, 0, 0),
    };
    p.u8(tag);
    p.u8(arg);
    p.u32(variant);
}

fn get_adjust(g: &mut Get<'_>) -> Result<AdjustKind, E> {
    let tag = g.u8()?;
    let arg = g.u8()?;
    let variant = g.u32()?;
    let plain = |k: AdjustKind| if arg == 0 && variant == 0 { Ok(k) } else { Err(g.bad(arg)) };
    match tag {
        0 => plain(AdjustKind::NeverToAny),
        1 if variant == 0 => Ok(AdjustKind::Deref {
            overloaded: match arg {
                0 => None,
                1 => Some(Mutbl::Not),
                2 => Some(Mutbl::Mut),
                _ => return Err(g.bad(arg)),
            },
        }),
        2 if arg < 8 && variant == 0 => Ok(AdjustKind::Borrow {
            mutbl: if arg & 1 == 0 { Mutbl::Not } else { Mutbl::Mut },
            two_phase: arg & 2 != 0,
            raw: arg & 4 != 0,
        }),
        1 | 2 => Err(g.bad(arg)),
        3 => plain(AdjustKind::ReifyFnPointer),
        4 => plain(AdjustKind::ClosureFnPointer),
        5 => plain(AdjustKind::UnsafeFnPointer),
        6 => plain(AdjustKind::MutToConstPointer),
        7 => plain(AdjustKind::Unsize),
        8 if arg == 0 => Ok(AdjustKind::UnionInject { variant }),
        8 => Err(g.bad(arg)),
        9 => plain(AdjustKind::UnionWiden),
        10 => plain(AdjustKind::RecordProject),
        _ => Err(g.bad(tag)),
    }
}

fn prim_tags(p: Prim) -> (u8, u8) {
    match p {
        Prim::Bool => (0, 0),
        Prim::Char => (1, 0),
        Prim::Str => (2, 0),
        Prim::Int(i) => (3, int_tag(i)),
        Prim::Uint(u) => (4, uint_tag(u)),
        Prim::Float(f) => (5, float_tag(f)),
    }
}

fn ctor_tag(k: CtorKind) -> u8 {
    match k {
        CtorKind::Braces => 0,
        CtorKind::Tuple => 1,
        CtorKind::Unit => 2,
    }
}

/// A [`Def`] in [`DEF`] bytes: tag, a, b, pad, u32, key.
fn put_def(p: &mut Put, d: Def) {
    let (tag, a, b, n, key) = match d {
        Def::Fn(k) => (0, 0, 0, 0, k),
        Def::Const(k) => (1, 0, 0, 0, k),
        Def::Static(k, m) => (2, mutbl_tag(m), 0, 0, k),
        Def::Ctor { adt, variant, kind } => (3, ctor_tag(kind), 0, variant, adt),
        Def::Adt(k) => (4, 0, 0, 0, k),
        Def::Trait(k) => (5, 0, 0, 0, k),
        Def::TyParam(i) => (6, 0, 0, i, ItemKey(0)),
        Def::Prim(prim) => {
            let (a, b) = prim_tags(prim);
            (7, a, b, 0, ItemKey(0))
        }
        Def::SelfTy => (8, 0, 0, 0, ItemKey(0)),
        Def::Alias(k) => (9, 0, 0, 0, k),
    };
    p.u8(tag);
    p.u8(a);
    p.u8(b);
    p.u8(0);
    p.u32(n);
    p.key(key);
}

fn get_def(g: &mut Get<'_>) -> Result<Def, E> {
    let tag = g.u8()?;
    let a = g.u8()?;
    let b = g.u8()?;
    let _pad = g.u8()?;
    let n = g.u32()?;
    let key = g.key()?;
    Ok(match tag {
        0 => Def::Fn(key),
        1 => Def::Const(key),
        2 => Def::Static(key, mutbl_of(g, a)?),
        3 => Def::Ctor {
            adt: key,
            variant: n,
            kind: match a {
                0 => CtorKind::Braces,
                1 => CtorKind::Tuple,
                2 => CtorKind::Unit,
                _ => return Err(g.bad(a)),
            },
        },
        4 => Def::Adt(key),
        5 => Def::Trait(key),
        6 => Def::TyParam(n),
        7 => Def::Prim(match a {
            0 => Prim::Bool,
            1 => Prim::Char,
            2 => Prim::Str,
            3 => Prim::Int(int_of(g, b)?),
            4 => Prim::Uint(uint_of(g, b)?),
            5 => Prim::Float(float_of(g, b)?),
            _ => return Err(g.bad(a)),
        }),
        8 => Def::SelfTy,
        9 => Def::Alias(key),
        _ => return Err(g.bad(tag)),
    })
}

/// A [`Shape`] in [`SHAPE`] bytes: tag, a sub-byte or flag, then a `u32` or a key.
fn put_shape(p: &mut Put, s: Shape) {
    let start = p.at;
    match s {
        Shape::Bool => p.u8(0),
        Shape::Char => p.u8(1),
        Shape::Int(i) => {
            p.u8(2);
            p.u8(int_tag(i));
        }
        Shape::Uint(u) => {
            p.u8(3);
            p.u8(uint_tag(u));
        }
        Shape::Float(f) => {
            p.u8(4);
            p.u8(float_tag(f));
        }
        Shape::Str => p.u8(5),
        Shape::Never => p.u8(6),
        Shape::Tuple(n) => {
            p.u8(7);
            p.u8(0);
            p.u32(n);
        }
        Shape::Ref(m) => {
            p.u8(8);
            p.u8(mutbl_tag(m));
        }
        Shape::Ptr(m) => {
            p.u8(9);
            p.u8(mutbl_tag(m));
        }
        Shape::Array => p.u8(10),
        Shape::Slice => p.u8(11),
        Shape::Pat => p.u8(12),
        Shape::Adt(k) => {
            p.u8(13);
            p.u8(0);
            p.key(k);
        }
        Shape::FnDef(k) => {
            p.u8(14);
            p.u8(0);
            p.key(k);
        }
        Shape::FnPtr(n) => {
            p.u8(15);
            p.u8(0);
            p.u32(n);
        }
        Shape::Closure => p.u8(16),
        Shape::Param => p.u8(17),
        Shape::Opaque(k) => {
            p.u8(18);
            p.u8(0);
            p.key(k);
        }
        Shape::Foreign(k) => {
            p.u8(19);
            p.u8(0);
            p.key(k);
        }
        Shape::Dynamic(k) => {
            p.u8(20);
            p.u8(u8::from(k.is_some()));
            p.key(k.unwrap_or(ItemKey(0)));
        }
        Shape::AnyInt => p.u8(21),
        Shape::AnyFloat => p.u8(22),
        Shape::Record(s, n) => {
            p.u8(23);
            p.u8(0);
            p.u64(s);
            p.u32(n);
        }
        Shape::Union(n) => {
            p.u8(24);
            p.u8(0);
            p.u32(n);
        }
    }
    p.to(start + SHAPE);
}

fn get_shape(g: &mut Get<'_>) -> Result<Shape, E> {
    let start = g.at;
    let tag = g.u8()?;
    let a = g.u8()?;
    let s = match tag {
        0 => Shape::Bool,
        1 => Shape::Char,
        2 => Shape::Int(int_of(g, a)?),
        3 => Shape::Uint(uint_of(g, a)?),
        4 => Shape::Float(float_of(g, a)?),
        5 => Shape::Str,
        6 => Shape::Never,
        7 => Shape::Tuple(g.u32()?),
        8 => Shape::Ref(mutbl_of(g, a)?),
        9 => Shape::Ptr(mutbl_of(g, a)?),
        10 => Shape::Array,
        11 => Shape::Slice,
        12 => Shape::Pat,
        13 => Shape::Adt(g.key()?),
        14 => Shape::FnDef(g.key()?),
        15 => Shape::FnPtr(g.u32()?),
        16 => Shape::Closure,
        17 => Shape::Param,
        18 => Shape::Opaque(g.key()?),
        19 => Shape::Foreign(g.key()?),
        20 => {
            let some = bool_of(g, a)?;
            let k = g.key()?;
            Shape::Dynamic(some.then_some(k))
        }
        21 => Shape::AnyInt,
        22 => Shape::AnyFloat,
        23 => Shape::Record(g.u64()?, g.u32()?),
        24 => Shape::Union(g.u32()?),
        _ => return Err(g.bad(tag)),
    };
    g.to(start + SHAPE);
    Ok(s)
}

fn key_name(p: &mut Put, tag: u8, k: ItemKey, n: Name) {
    p.u8(tag);
    p.key(k);
    p.u64(n.0);
}

fn key(p: &mut Put, tag: u8, k: ItemKey) {
    p.u8(tag);
    p.key(k);
}

fn get_lang(g: &mut Get<'_>) -> Result<LangItem, E> {
    let t = g.u8()?;
    LangItem::of_tag(t).ok_or(g.bad(t))
}

/// A [`Question`] in [`QUESTION`] bytes: tag, then its fields.
fn put_question(p: &mut Put, q: Question) {
    match q {
        Question::FnSig(k) => key(p, 0, k),
        Question::Predicates(k) => key(p, 1, k),
        Question::Adt(k) => key(p, 2, k),
        Question::ImplsOf(k, s) => {
            key(p, 3, k);
            put_shape(p, s);
        }
        Question::ImplHeader(k) => key(p, 4, k),
        Question::Inherent(s, n) => {
            p.u8(5);
            put_shape(p, s);
            p.u64(n.0);
        }
        Question::AssocItem(k, n) => key_name(p, 6, k, n),
        Question::TraitsInScope(s) => {
            p.u8(7);
            p.u32(s.0);
        }
        Question::Path(s, i) => {
            p.u8(8);
            p.u32(s.0);
            p.u32(i.0);
        }
        Question::Lang(l) => {
            p.u8(9);
            p.u8(l as u8);
        }
        Question::LangPath(l) => {
            p.u8(10);
            p.u8(l as u8);
        }
        Question::Const(k) => key(p, 11, k),
        Question::Opaque(k) => key(p, 12, k),
        Question::Attrs(k) => key(p, 13, k),
        Question::TraitDef(k) => key(p, 14, k),
        Question::AssocParent(k) => key(p, 15, k),
        Question::ConstHolds(k, h) => {
            key(p, 16, k);
            p.u64(h);
        }
        Question::ConstParams(k) => key(p, 17, k),
        Question::Closure(k, i) => {
            key(p, 18, k);
            p.u32(i);
        }
        Question::Coroutine(k, i) => {
            key(p, 19, k);
            p.u32(i);
        }
        Question::AssocGenerics(k, n) => key_name(p, 20, k, n),
        Question::TypeAlias(k) => key(p, 21, k),
        Question::FieldDefaults(k) => key(p, 22, k),
        Question::MemberVisible(k, n) => key_name(p, 23, k, n),
        Question::MemberUnstable(k, n) => key_name(p, 24, k, n),
    }
}

fn get_question(g: &mut Get<'_>) -> Result<Question, E> {
    let tag = g.u8()?;
    Ok(match tag {
        0 => Question::FnSig(g.key()?),
        1 => Question::Predicates(g.key()?),
        2 => Question::Adt(g.key()?),
        3 => Question::ImplsOf(g.key()?, get_shape(g)?),
        4 => Question::ImplHeader(g.key()?),
        5 => Question::Inherent(get_shape(g)?, Name(g.u64()?)),
        6 => Question::AssocItem(g.key()?, Name(g.u64()?)),
        7 => Question::TraitsInScope(ScopeId(g.u32()?)),
        8 => Question::Path(ScopeId(g.u32()?), PathIdx(g.u32()?)),
        9 => Question::Lang(get_lang(g)?),
        10 => Question::LangPath(get_lang(g)?),
        11 => Question::Const(g.key()?),
        12 => Question::Opaque(g.key()?),
        13 => Question::Attrs(g.key()?),
        14 => Question::TraitDef(g.key()?),
        15 => Question::AssocParent(g.key()?),
        16 => Question::ConstHolds(g.key()?, g.u64()?),
        17 => Question::ConstParams(g.key()?),
        18 => Question::Closure(g.key()?, g.u32()?),
        19 => Question::Coroutine(g.key()?, g.u32()?),
        20 => Question::AssocGenerics(g.key()?, Name(g.u64()?)),
        21 => Question::TypeAlias(g.key()?),
        22 => Question::FieldDefaults(g.key()?),
        23 => Question::MemberVisible(g.key()?, Name(g.u64()?)),
        24 => Question::MemberUnstable(g.key()?, Name(g.u64()?)),
        _ => return Err(g.bad(tag)),
    })
}

fn closure_kind_tag(k: ClosureKind) -> u8 {
    match k {
        ClosureKind::Fn => 0,
        ClosureKind::FnMut => 1,
        ClosureKind::FnOnce => 2,
    }
}

fn capture_tag(k: CaptureKind) -> u8 {
    match k {
        CaptureKind::ByRef => 0,
        CaptureKind::ByMutRef => 1,
        CaptureKind::ByValue => 2,
    }
}

fn witness_tag(w: CoroutineWitness) -> u8 {
    match w {
        CoroutineWitness::NeedsCapturePlaces => 0,
        CoroutineWitness::NeedsSavedLocals => 1,
        CoroutineWitness::Complete => 2,
    }
}

// ---- the encoder ---------------------------------------------------------------------------

/// The writer of one record: the header's row counts, then each column in order.
struct Writer<'o, 'b> {
    out: &'o mut Vec<u8, Alloc<'b>>,
}

impl Writer<'_, '_> {
    fn column<T>(&mut self, col: Col, rows: impl Iterator<Item = T>, mut row: impl FnMut(&mut Put, T)) -> Result<(), E> {
        for (i, r) in rows.enumerate() {
            let mut p = Put::new();
            row(&mut p, r);
            p.finish(col, i, self.out)?;
        }
        Ok(())
    }
}

/// The record of the clean check `c` (rule 26: [`decode`] of it re-encodes to these bytes), in
/// `alloc`. Refuses a check with a diagnostic, a stalled goal or an undecided place
/// ([`CheckedRecordError::NotClean`]): a record is kept only after a clean check.
pub fn encode<'b>(c: &Checked<'_>, alloc: Alloc<'b>) -> Result<Vec<u8, Alloc<'b>>, CheckedRecordError> {
    encode_rows(c, false, alloc)
}

/// [`encode`] of a check whose stalled goals were each decided to hold on its exact interior
/// afterwards (blaze-ladder front2 stalled_goals.rs `Decision::Proved`, its one caller,
/// borrows.rs `check_borrows_in`): the record holds no stalled column (a run reads none), so its
/// bytes are a clean check's, and [`decode`] of them re-encodes to them (F18).
pub fn encode_decided<'b>(c: &Checked<'_>, alloc: Alloc<'b>) -> Result<Vec<u8, Alloc<'b>>, CheckedRecordError> {
    encode_rows(c, true, alloc)
}

/// [`encode`]'s one body: `stalled_proved` says the check's stalled rows were each decided to
/// hold ([`encode_decided`]), so they do not make it unclean.
fn encode_rows<'b>(c: &Checked<'_>, stalled_proved: bool, alloc: Alloc<'b>) -> Result<Vec<u8, Alloc<'b>>, CheckedRecordError> {
    let stalled = if stalled_proved { 0 } else { c.stalled.len() };
    for (what, n) in [("diags", c.diags.len()), ("why", c.why.len()), ("stalled", stalled), ("undecided", c.undecided.len())] {
        if n != 0 {
            return Err(E::NotClean { what, rows: rows_u32(n, what)? });
        }
    }
    let view = c.types.view();
    let list_rows = c.types.list_rows_in(alloc).map_err(E::Types)?;
    let first: &[Question] = match &c.needs.first {
        Some(q) => core::slice::from_ref(q),
        None => &[],
    };
    let rows: [usize; 31] = [
        view.len(),
        view.lists_len(),
        list_rows.len(),
        c.expr_ty.len(),
        c.pat_ty.len(),
        c.local_ty.len(),
        c.adjusts.len(),
        c.callees.len(),
        c.expr_defs.len(),
        c.const_args.len(),
        c.pat_const_args.len(),
        c.pat_defs.len(),
        c.fields.len(),
        c.captures.len(),
        c.closures.len(),
        c.hidden.len(),
        c.promoted.len(),
        c.coroutine_witnesses.len(),
        c.awaited.len(),
        c.selected.len(),
        c.fru_fields.len(),
        c.default_fields.len(),
        c.index_mut.len(),
        c.needs.questions.len(),
        c.needs.missing.len(),
        first.len(),
        c.capture_places.len(),
        c.capture_projs.len(),
        view.record_schemas_len(),
        view.record_names_len(),
        c.union_pats.len(),
    ];
    let mut size = CHECKED_RECORD_MAGIC.len() + 4 + 4 * COLS.len();
    for col in COLS {
        size += rows[col.at()] * col.width();
    }
    let mut out = Vec::with_capacity_in(size, alloc);
    out.extend_from_slice(&CHECKED_RECORD_MAGIC);
    out.extend_from_slice(&rows_u32(COLS.len(), "columns")?.to_le_bytes());
    for col in COLS {
        out.extend_from_slice(&rows_u32(rows[col.at()], col.name())?.to_le_bytes());
    }
    let mut w = Writer { out: &mut out };
    w.column(Col::TyNodes, view.nodes(), |p, &n| put_node(p, n))?;
    w.column(Col::TyLists, view.lists(), |p, &t| p.ty(t))?;
    w.column(Col::TyListRows, list_rows.iter(), |p, &l| p.list(l))?;
    w.column(Col::ExprTy, c.expr_ty.iter(), |p, &t| p.ty(t))?;
    w.column(Col::PatTy, c.pat_ty.iter(), |p, &t| p.ty(t))?;
    w.column(Col::LocalTy, c.local_ty.iter(), |p, &t| p.ty(t))?;
    let a = &c.adjusts;
    w.column(Col::Adjusts, 0..a.len(), |p, i| {
        p.u32(a.column::<0>()[i].0);
        put_adjust(p, a.column::<1>()[i]);
        p.ty(a.column::<2>()[i]);
    })?;
    let a = &c.callees;
    w.column(Col::Callees, 0..a.len(), |p, i| {
        p.u32(a.column::<0>()[i].0);
        p.key(a.column::<1>()[i]);
        p.list(a.column::<2>()[i]);
    })?;
    let a = &c.expr_defs;
    w.column(Col::ExprDefs, 0..a.len(), |p, i| {
        p.u32(a.column::<0>()[i].0);
        put_def(p, a.column::<1>()[i]);
    })?;
    let a = &c.const_args;
    w.column(Col::ConstArgs, 0..a.len(), |p, i| {
        p.u32(a.column::<0>()[i].0);
        p.list(a.column::<1>()[i]);
    })?;
    let a = &c.pat_const_args;
    w.column(Col::PatConstArgs, 0..a.len(), |p, i| {
        p.u32(a.column::<0>()[i].0);
        p.list(a.column::<1>()[i]);
    })?;
    let a = &c.pat_defs;
    w.column(Col::PatDefs, 0..a.len(), |p, i| {
        p.u32(a.column::<0>()[i].0);
        put_def(p, a.column::<1>()[i]);
    })?;
    let a = &c.fields;
    w.column(Col::Fields, 0..a.len(), |p, i| {
        p.u32(a.column::<0>()[i].0);
        p.u32(a.column::<1>()[i]);
        p.u32(a.column::<2>()[i]);
    })?;
    let a = &c.captures;
    w.column(Col::Captures, 0..a.len(), |p, i| {
        p.u32(a.column::<0>()[i].0);
        p.u32(a.column::<1>()[i].0);
        p.u8(capture_tag(a.column::<2>()[i]));
    })?;
    let a = &c.closures;
    w.column(Col::Closures, 0..a.len(), |p, i| {
        p.u32(a.column::<0>()[i].0);
        p.u8(closure_kind_tag(a.column::<1>()[i]));
        p.ty(a.column::<2>()[i]);
        let places = a.column::<3>()[i];
        p.u8(u8::from(places.is_some()));
        p.ty(places.unwrap_or(PTy(0)));
        match a.column::<4>()[i] {
            SigFrom::Clause { expr, nth, output } => {
                p.u8(0);
                p.u32(expr.0);
                p.u32(nth);
                p.u8(u8::from(output.is_some()));
                p.u32(output.unwrap_or(0));
            }
            SigFrom::Written => p.u8(1),
            SigFrom::Inferred => p.u8(2),
            SigFrom::Object => p.u8(3),
            SigFrom::FnPtr => p.u8(4),
        }
    })?;
    w.column(Col::Hidden, c.hidden.iter(), |p, &(k, t)| {
        p.key(k);
        p.ty(t);
    })?;
    w.column(Col::Promoted, c.promoted.iter(), |p, &e| p.u32(e.0))?;
    w.column(Col::CoroutineWitnesses, c.coroutine_witnesses.iter(), |p, &(e, wit)| {
        p.u32(e.0);
        p.u8(witness_tag(wit));
    })?;
    w.column(Col::Awaited, c.awaited.iter(), |p, &(e, t)| {
        p.u32(e.0);
        p.ty(t);
    })?;
    w.column(Col::Selected, c.selected.iter(), |p, s| {
        p.u32(s.expr.0);
        p.u32(s.at);
        p.key(s.trait_);
        p.u8(u8::from(s.name.is_some()));
        p.u64(s.name.map_or(0, |n| n.0));
        p.list(s.args);
        p.u8(u8::from(s.global_env));
        match s.selected {
            Selected::Impl { imp, args } => {
                p.u8(0);
                p.key(imp);
                p.list(args);
            }
            Selected::Generic => p.u8(1),
            Selected::NotImpl => p.u8(2),
            Selected::Unanswered => p.u8(3),
            Selected::ParamEnv { clause } => {
                p.u8(4);
                p.u32(clause);
            }
            Selected::OpaqueBound => p.u8(5),
            Selected::ObjectBound => p.u8(6),
            Selected::Builtin => p.u8(7),
        }
    })?;
    let a = &c.fru_fields;
    w.column(Col::FruFields, 0..a.len(), |p, i| {
        p.u32(a.column::<0>()[i].0);
        p.u32(a.column::<1>()[i]);
        p.ty(a.column::<2>()[i]);
    })?;
    let a = &c.default_fields;
    w.column(Col::DefaultFields, 0..a.len(), |p, i| {
        p.u32(a.column::<0>()[i].0);
        p.u32(a.column::<1>()[i]);
        p.key(a.column::<2>()[i]);
    })?;
    w.column(Col::IndexMut, c.index_mut.iter(), |p, &e| p.u32(e.0))?;
    w.column(Col::NeedQuestions, c.needs.questions.iter(), |p, &q| put_question(p, q))?;
    w.column(Col::NeedMissing, c.needs.missing.iter(), |p, &q| put_question(p, q))?;
    w.column(Col::NeedFirst, first.iter(), |p, &q| put_question(p, q))?;
    let a = &c.capture_places;
    w.column(Col::CapturePlaces, 0..a.len(), |p, i| {
        p.u32(a.column::<0>()[i].0);
        p.u32(a.column::<1>()[i].0);
        p.u32(a.column::<2>()[i]);
        p.u32(a.column::<3>()[i]);
        p.u8(capture_tag(a.column::<4>()[i]));
        p.ty(a.column::<5>()[i]);
    })?;
    let a = &c.capture_projs;
    w.column(Col::CaptureProjs, 0..a.len(), |p, i| {
        p.u8(a.column::<0>()[i]);
        p.u32(a.column::<1>()[i]);
        p.u32(a.column::<2>()[i]);
        p.ty(a.column::<3>()[i]);
    })?;
    w.column(Col::TyRecordSchemas, view.record_schema_rows(), |p, &(s, l)| {
        p.u64(s.0);
        p.list(l);
    })?;
    w.column(Col::TyRecordNames, view.record_names_iter(), |p, &n| p.u64(n.0))?;
    let a = &c.union_pats;
    w.column(Col::UnionPats, a.column::<0>().iter().copied().zip(a.column::<1>().iter().copied()), |p, (pat, kind)| {
        p.u32(pat.0);
        match kind {
            UnionPat::Variant(v) => {
                p.u8(0);
                p.u32(v);
            }
            UnionPat::Residual => {
                p.u8(1);
                p.u32(0);
            }
        }
    })?;
    Ok(out)
}

// ---- the record, read in place -------------------------------------------------------------

/// A checked-body record, its header checked once ([`CheckedRecord::open`]): each column read
/// in place by row (rule 51), the owned [`Checked`] built by [`CheckedRecord::checked_in`].
#[derive(Clone, Copy, Debug)]
pub struct CheckedRecord<'r> {
    bytes: &'r [u8],
    rows: [u32; 31],
    at: [usize; 31],
}

impl<'r> CheckedRecord<'r> {
    /// The record in `bytes`: its magic, column count and length checked (rule 74: the rest
    /// of the reads take this as their proof that every row lies in the bytes).
    pub fn open(bytes: &'r [u8]) -> Result<CheckedRecord<'r>, CheckedRecordError> {
        let magic = CHECKED_RECORD_MAGIC.len();
        if bytes.get(..magic) != Some(&CHECKED_RECORD_MAGIC[..]) {
            return Err(E::Magic);
        }
        let word = |at: usize| -> Result<u32, E> {
            let b = bytes.get(at..at + 4).ok_or(E::Length { need: (at + 4) as u64, have: bytes.len() as u64 })?;
            Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        };
        let have = word(magic)?;
        if have as usize != COLS.len() {
            return Err(E::Columns { have });
        }
        let mut rows = [0u32; 31];
        let mut at = [0usize; 31];
        let mut next = (magic + 4 + 4 * COLS.len()) as u64;
        for col in COLS {
            let n = word(magic + 4 + 4 * col.at())?;
            rows[col.at()] = n;
            at[col.at()] = next as usize;
            next += u64::from(n) * col.width() as u64;
        }
        if next != bytes.len() as u64 {
            return Err(E::Length { need: next, have: bytes.len() as u64 });
        }
        Ok(CheckedRecord { bytes, rows, at })
    }

    /// The rows of `col`.
    pub fn rows(&self, col: Col) -> u32 {
        self.rows[col.at()]
    }

    /// Row `i` of `col`, in place.
    pub fn row(&self, col: Col, i: u32) -> Option<&'r [u8]> {
        if i >= self.rows(col) {
            return None;
        }
        let w = col.width();
        let start = self.at[col.at()] + i as usize * w;
        self.bytes.get(start..start + w)
    }

    fn one_ty(&self, col: Col, i: u32) -> Option<PTy> {
        let r = self.row(col, i)?;
        Some(PTy(u32::from_le_bytes([r[0], r[1], r[2], r[3]])))
    }

    /// Expression `e`'s type, read in place (a type of the record's table).
    pub fn expr_ty(&self, e: ExprId) -> Option<PTy> {
        self.one_ty(Col::ExprTy, e.0)
    }

    /// Pattern `p`'s type, read in place.
    pub fn pat_ty(&self, p: PatId) -> Option<PTy> {
        self.one_ty(Col::PatTy, p.0)
    }

    /// Pattern `p`'s binding type, read in place.
    pub fn local_ty(&self, p: PatId) -> Option<PTy> {
        self.one_ty(Col::LocalTy, p.0)
    }

    /// The resolved callee of the call `e` and its generic arguments, read in place: a binary
    /// search over the column's expressions (sorted, as the check writes it).
    pub fn callee_of(&self, e: ExprId) -> Option<(ItemKey, List)> {
        let n = self.rows(Col::Callees);
        let expr = |i: u32| self.row(Col::Callees, i).map(|r| u32::from_le_bytes([r[0], r[1], r[2], r[3]]));
        let (mut lo, mut hi) = (0u32, n);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if expr(mid)? < e.0 { lo = mid + 1 } else { hi = mid }
        }
        if expr(lo)? != e.0 {
            return None;
        }
        let r = self.row(Col::Callees, lo)?;
        let mut g = Get { row: r, at: 4, col: Col::Callees, n: lo };
        Some((g.key().ok()?, g.raw_list().ok()?))
    }

    fn each(&self, col: Col, mut f: impl FnMut(&mut Get<'r>) -> Result<(), E>) -> Result<(), E> {
        for i in 0..self.rows(col) {
            let row = self.row(col, i).ok_or(E::Width { column: col.name(), row: i })?;
            let mut g = Get { row, at: 0, col, n: i };
            f(&mut g)?;
        }
        Ok(())
    }

    /// The record as the owned [`Checked`] the checker's readers take, in `alloc`: its own
    /// type table, every type and list at the index the check gave it. The columns the record
    /// does not hold are empty (a clean check: no diagnostic, suggestion, stalled goal,
    /// finding, undecided place; the read list is the record's key, not its value).
    pub fn checked_in<'a>(&self, alloc: Alloc<'a>) -> Result<Checked<'a>, CheckedRecordError> {
        let b = Bounds { nodes: self.rows(Col::TyNodes), lists: self.rows(Col::TyLists) };
        let mut types = PortableTys::new_in(alloc);
        self.each(Col::TyNodes, |g| {
            let n = get_node(g, b)?;
            let t = types.intern(n).map_err(E::Types)?;
            if t.0 != g.n {
                return Err(E::NotCanonical { column: Col::TyNodes.name(), row: g.n });
            }
            Ok(())
        })?;
        let mut entries = Vec::with_capacity_in(b.lists as usize, alloc);
        self.each(Col::TyLists, |g| {
            entries.push(b.ty(g, u32::MAX)?);
            Ok(())
        })?;
        let mut end = 0u32;
        self.each(Col::TyListRows, |g| {
            let l = b.list(g)?;
            if l.start != end || l.len == 0 {
                return Err(E::ListRows { row: g.n, start: l.start, expected: end });
            }
            end = l.start + l.len;
            let got = types.intern_list(&entries[l.start as usize..end as usize]).map_err(E::Types)?;
            if got != l {
                return Err(E::NotCanonical { column: Col::TyListRows.name(), row: g.n });
            }
            Ok(())
        })?;
        if end != b.lists {
            return Err(E::ListRows { row: self.rows(Col::TyListRows), start: b.lists, expected: end });
        }
        let mut record_names = Vec::with_capacity_in(usize::try_from(self.rows(Col::TyRecordNames)).unwrap_or(0), alloc);
        self.each(Col::TyRecordNames, |g| {
            record_names.push(Name(g.u64()?));
            Ok(())
        })?;
        let n_record_names = u64::try_from(record_names.len()).unwrap_or(u64::MAX);
        self.each(Col::TyRecordSchemas, |g| {
            let schema = RecordSchema(g.u64()?);
            let l = g.raw_list()?;
            if u64::from(l.start) + u64::from(l.len) > n_record_names {
                return Err(E::ListOutside { column: Col::TyRecordSchemas.name(), row: g.n, start: l.start, len: l.len });
            }
            let names = if l.len == 0 {
                &[][..]
            } else {
                record_names
                    .get(l.range())
                    .ok_or(E::ListOutside { column: Col::TyRecordSchemas.name(), row: g.n, start: l.start, len: l.len })?
            };
            types
                .register_record_schema(schema, names)
                .map_err(|_| E::Types(CheckError::Limit(crate::error::Limit::Table)))?;
            Ok(())
        })?;
        let tys = |col: Col| -> Result<Vec<PTy, Alloc<'a>>, E> {
            let mut v = Vec::with_capacity_in(self.rows(col) as usize, alloc);
            self.each(col, |g| {
                v.push(b.ty(g, u32::MAX)?);
                Ok(())
            })?;
            Ok(v)
        };
        let exprs = |col: Col| -> Result<Vec<ExprId, Alloc<'a>>, E> {
            let mut v = Vec::with_capacity_in(self.rows(col) as usize, alloc);
            self.each(col, |g| {
                v.push(ExprId(g.u32()?));
                Ok(())
            })?;
            Ok(v)
        };
        let questions = |col: Col| -> Result<Vec<Question, Alloc<'a>>, E> {
            let mut v = Vec::with_capacity_in(self.rows(col) as usize, alloc);
            self.each(col, |g| {
                v.push(get_question(g)?);
                Ok(())
            })?;
            Ok(v)
        };
        let expr_ty = tys(Col::ExprTy)?;
        let pat_ty = tys(Col::PatTy)?;
        let local_ty = tys(Col::LocalTy)?;
        let mut adjusts = Soa::new_in(alloc);
        self.each(Col::Adjusts, |g| {
            let e = ExprId(g.u32()?);
            let k = get_adjust(g)?;
            adjusts.push((e, k, b.ty(g, u32::MAX)?));
            Ok(())
        })?;
        let mut callees = Soa::new_in(alloc);
        self.each(Col::Callees, |g| {
            let e = ExprId(g.u32()?);
            let k = g.key()?;
            callees.push((e, k, b.list(g)?));
            Ok(())
        })?;
        let mut expr_defs = Soa::new_in(alloc);
        self.each(Col::ExprDefs, |g| {
            let e = ExprId(g.u32()?);
            expr_defs.push((e, get_def(g)?));
            Ok(())
        })?;
        let mut const_args = Soa::new_in(alloc);
        self.each(Col::ConstArgs, |g| {
            let e = ExprId(g.u32()?);
            const_args.push((e, b.list(g)?));
            Ok(())
        })?;
        let mut pat_const_args = Soa::new_in(alloc);
        self.each(Col::PatConstArgs, |g| {
            let p = PatId(g.u32()?);
            pat_const_args.push((p, b.list(g)?));
            Ok(())
        })?;
        let mut pat_defs = Soa::new_in(alloc);
        self.each(Col::PatDefs, |g| {
            let p = PatId(g.u32()?);
            pat_defs.push((p, get_def(g)?));
            Ok(())
        })?;
        let mut union_pats = Soa::new_in(alloc);
        self.each(Col::UnionPats, |g| {
            let p = PatId(g.u32()?);
            let tag = g.u8()?;
            let v = g.u32()?;
            let kind = match tag {
                0 => UnionPat::Variant(v),
                1 if v == 0 => UnionPat::Residual,
                _ => return Err(g.bad(tag)),
            };
            union_pats.push((p, kind));
            Ok(())
        })?;
        let mut fields = Soa::new_in(alloc);
        self.each(Col::Fields, |g| {
            fields.push((ExprId(g.u32()?), g.u32()?, g.u32()?));
            Ok(())
        })?;
        let mut captures = Soa::new_in(alloc);
        self.each(Col::Captures, |g| {
            let e = ExprId(g.u32()?);
            let p = PatId(g.u32()?);
            let t = g.u8()?;
            let k = match t {
                0 => CaptureKind::ByRef,
                1 => CaptureKind::ByMutRef,
                2 => CaptureKind::ByValue,
                _ => return Err(g.bad(t)),
            };
            captures.push((e, p, k));
            Ok(())
        })?;
        let mut capture_projs = Soa::new_in(alloc);
        self.each(Col::CaptureProjs, |g| {
            let t = g.u8()?;
            if t > 2 {
                return Err(g.bad(t));
            }
            let v = g.u32()?;
            let f = g.u32()?;
            capture_projs.push((t, v, f, b.ty(g, u32::MAX)?));
            Ok(())
        })?;
        let mut capture_places = Soa::new_in(alloc);
        let n_projs = capture_projs.len() as u64;
        self.each(Col::CapturePlaces, |g| {
            let e = ExprId(g.u32()?);
            let p = PatId(g.u32()?);
            let start = g.u32()?;
            let len = g.u32()?;
            if u64::from(start) + u64::from(len) > n_projs {
                return Err(E::ListOutside { column: Col::CapturePlaces.name(), row: capture_places.len() as u32, start, len });
            }
            let t = g.u8()?;
            let k = match t {
                0 => CaptureKind::ByRef,
                1 => CaptureKind::ByMutRef,
                2 => CaptureKind::ByValue,
                _ => return Err(g.bad(t)),
            };
            capture_places.push((e, p, start, len, k, b.ty(g, u32::MAX)?));
            Ok(())
        })?;
        let mut closures = Soa::new_in(alloc);
        self.each(Col::Closures, |g| {
            let e = ExprId(g.u32()?);
            let t = g.u8()?;
            let kind = match t {
                0 => ClosureKind::Fn,
                1 => ClosureKind::FnMut,
                2 => ClosureKind::FnOnce,
                _ => return Err(g.bad(t)),
            };
            let sig = b.ty(g, u32::MAX)?;
            let places = b.opt_ty(g)?;
            let t = g.u8()?;
            let from = match t {
                0 => {
                    let expr = ExprId(g.u32()?);
                    let nth = g.u32()?;
                    let f = g.u8()?;
                    let some = bool_of(g, f)?;
                    let output = g.u32()?;
                    SigFrom::Clause { expr, nth, output: some.then_some(output) }
                }
                1 => SigFrom::Written,
                2 => SigFrom::Inferred,
                3 => SigFrom::Object,
                4 => SigFrom::FnPtr,
                _ => return Err(g.bad(t)),
            };
            closures.push((e, kind, sig, places, from));
            Ok(())
        })?;
        let mut hidden = Vec::with_capacity_in(self.rows(Col::Hidden) as usize, alloc);
        self.each(Col::Hidden, |g| {
            let k = g.key()?;
            hidden.push((k, b.ty(g, u32::MAX)?));
            Ok(())
        })?;
        let promoted = exprs(Col::Promoted)?;
        let mut coroutine_witnesses = Vec::with_capacity_in(self.rows(Col::CoroutineWitnesses) as usize, alloc);
        self.each(Col::CoroutineWitnesses, |g| {
            let e = ExprId(g.u32()?);
            let t = g.u8()?;
            let w = match t {
                0 => CoroutineWitness::NeedsCapturePlaces,
                1 => CoroutineWitness::NeedsSavedLocals,
                2 => CoroutineWitness::Complete,
                _ => return Err(g.bad(t)),
            };
            coroutine_witnesses.push((e, w));
            Ok(())
        })?;
        let mut awaited = Vec::with_capacity_in(self.rows(Col::Awaited) as usize, alloc);
        self.each(Col::Awaited, |g| {
            let e = ExprId(g.u32()?);
            awaited.push((e, b.ty(g, u32::MAX)?));
            Ok(())
        })?;
        let mut selected = Vec::with_capacity_in(self.rows(Col::Selected) as usize, alloc);
        self.each(Col::Selected, |g| {
            let expr = ExprId(g.u32()?);
            let at = g.u32()?;
            let trait_ = g.key()?;
            let f = g.u8()?;
            let has_name = bool_of(g, f)?;
            let name = Name(g.u64()?);
            let args = b.list(g)?;
            let f = g.u8()?;
            let global_env = bool_of(g, f)?;
            let t = g.u8()?;
            let sel = match t {
                0 => Selected::Impl { imp: g.key()?, args: b.list(g)? },
                1 => Selected::Generic,
                2 => Selected::NotImpl,
                3 => Selected::Unanswered,
                4 => Selected::ParamEnv { clause: g.u32()? },
                5 => Selected::OpaqueBound,
                6 => Selected::ObjectBound,
                7 => Selected::Builtin,
                _ => return Err(g.bad(t)),
            };
            selected.push(Projected { expr, at, trait_, name: has_name.then_some(name), args, global_env, selected: sel });
            Ok(())
        })?;
        let mut fru_fields = Soa::new_in(alloc);
        self.each(Col::FruFields, |g| {
            let e = ExprId(g.u32()?);
            let i = g.u32()?;
            fru_fields.push((e, i, b.ty(g, u32::MAX)?));
            Ok(())
        })?;
        let mut default_fields = Soa::new_in(alloc);
        self.each(Col::DefaultFields, |g| {
            default_fields.push((ExprId(g.u32()?), g.u32()?, g.key()?));
            Ok(())
        })?;
        let index_mut = exprs(Col::IndexMut)?;
        let needs_questions = questions(Col::NeedQuestions)?;
        let needs_missing = questions(Col::NeedMissing)?;
        let first = questions(Col::NeedFirst)?;
        if first.len() > 1 {
            return Err(E::Tag { column: Col::NeedFirst.name(), row: 1, tag: 0 });
        }
        Ok(Checked {
            types,
            expr_ty,
            pat_ty,
            local_ty,
            adjusts,
            callees,
            expr_defs,
            const_args,
            pat_const_args,
            pat_defs,
            union_pats,
            fields,
            captures,
            capture_places,
            capture_projs,
            closures,
            diags: Vec::new_in(alloc),
            why: Vec::new_in(alloc),
            reads: ReadList { rows: Soa::new_in(alloc) },
            needs: Needs { questions: needs_questions, missing: needs_missing, first: first.first().copied() },
            suggests: Suggests::new_in(alloc),
            hidden,
            promoted,
            stalled: Vec::new_in(alloc),
            coroutine_witnesses,
            awaited,
            selected,
            fru_fields,
            default_fields,
            index_mut,
            findings: Vec::new_in(alloc),
            undecided: Vec::new_in(alloc),
            inferred_output: None,
        })
    }
}

/// The owned [`Checked`] of the record `bytes`, in `alloc` ([`CheckedRecord::open`], then
/// [`CheckedRecord::checked_in`]).
pub fn decode<'a>(bytes: &[u8], alloc: Alloc<'a>) -> Result<Checked<'a>, CheckedRecordError> {
    CheckedRecord::open(bytes)?.checked_in(alloc)
}
