//! The body's forms (backend-next.md, "The body"): typed ids, each row's form as a Rust value,
//! and the body as flat tables of fixed-width rows, owned ([`Body`]) or borrowed ([`BodyRef`]).
//! A [`BodyRef`] is one reader over both a body in memory and a record read in place
//! ([`crate::record::open`]): every row is decoded when it is asked for, and a row that is no
//! form is a [`BadRow`], never a panic.
//!
//! The ids and the borrow-fact tables mean what they mean in blaze-flow's `Flow`: `Local(0)` is
//! the return place, then the arguments; a block is a range of the statement array whose last
//! row is its terminator; a loan is one borrow statement's borrow; an origin is a set of loans.
#![blaze::sans_io]

use blaze_own::Vec;
use core::alloc::Allocator;
use core::fmt;

pub use blaze_typeck::{ExprId, ItemKey, List, Mutbl, PTy, PatId};

use blaze_typeck::{PNode, PTys};

use crate::code::{
    BlockWords, CalleeWords, CaseWords, ConstWords, LoanWords, LocalWords, OperandWords, PairWords,
    PlaceWords, StmtWords, SubsetWords, TyWords, decode_block, decode_callee, decode_case, decode_const,
    decode_loan, decode_local, decode_operand, decode_place, decode_stmt, decode_ty,
};

/// The word of an absent optional id in a row.
pub const NONE: u32 = u32::MAX;

macro_rules! ids {
    ($($(#[$m:meta])* $name:ident = $tag:literal;)*) => {$(
        $(#[$m])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
        pub struct $name(pub u32);
        impl $name {
            /// The id as an index into its table.
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
    /// A local of the body: `Local(0)` is the return place, `1..=args` the arguments, then
    /// bindings and temporaries (Rust's "local variables" and the values between them).
    Local = "_";
    /// A row of the place table: a local, or a parent place and one projection (a Rust place
    /// expression such as `(*x).f[i]`, interned: one id per distinct place).
    Place = "p";
    /// A row of the statement array, terminators included.
    Stmt = "s";
    /// A basic block: a range of the statement array whose last row is its terminator.
    Block = "bb";
    /// A loan: the borrow one borrow statement makes (`&x`, `&mut x`).
    Loan = "L";
    /// An origin: a set of loans a reference may hold (Polonius' "origin", Rust's lifetime).
    Origin = "'o";
    /// A row of the constant table: a value known before the body runs.
    Const = "c";
    /// A row of the callee table: what a call calls.
    CalleeId = "f";
}

/// A point: a statement's Start (accesses, kills) or its Mid (issues, `subset_base`), as in
/// blaze-flow.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Point(pub u32);

impl Point {
    /// Where statement `s` starts: its reads and writes happen here.
    pub fn start(s: Stmt) -> Point {
        Point(s.0 * 2)
    }
    /// The middle of statement `s`: its loans are issued and its subsets hold here.
    pub fn mid(s: Stmt) -> Point {
        Point(s.0 * 2 + 1)
    }
    /// The statement this point is of.
    pub fn stmt(self) -> Stmt {
        Stmt(self.0 / 2)
    }
    /// Whether this is a Mid point.
    pub fn is_mid(self) -> bool {
        self.0 % 2 == 1
    }
    /// The point as an index into a per-point table.
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

blaze_format_core::record! {
    /// An item-relative byte range: where in its item a statement came from (rule 71).
    #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
    pub struct Span: 1 {
        pub lo: u32,
        pub hi: u32,
    }
}

/// A range of a side table (operands, cases): `start..end`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct Ops {
    pub start: u32,
    pub end: u32,
}

impl Ops {
    /// How many rows it covers (0 when `end <= start`).
    pub fn len(self) -> u32 {
        self.end.saturating_sub(self.start)
    }
    /// Its rows as a `usize` range.
    pub fn range(self) -> core::ops::Range<usize> {
        self.start as usize..self.end as usize
    }
}

/// Why a local exists, which is also its identity across every reader (the 21:07 ruling).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum LocalKind {
    /// The return place, `_0`.
    Return,
    /// The function's argument at this index (0 is the first).
    Arg(u32),
    /// The variable a binding pattern introduces (`let x`, `Some(x) =>`).
    Binding(PatId),
    /// A temporary holding an expression's value in a role.
    Temp(ExprId, Role),
    /// The drop flag of a local: written by drop elaboration, true while it must be dropped.
    DropFlag(Local),
}

/// What a temporary holds of its expression: the roles blaze-flow-lower's identities name.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Role {
    /// The expression's value.
    Value,
    /// A borrow of it (an autoref, a `&` the lowering makes).
    Borrow,
    /// The future an `.await` polls.
    Awaitee,
    /// The pinned reference an `.await` polls through.
    Pin,
    /// The output of an `.await`.
    Result,
}

/// One local's row: its type (generic, of the checked record), why it exists, and whether it
/// may be assigned more than once (`let mut`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct LocalDecl {
    pub ty: PTy,
    pub kind: LocalKind,
    pub mutbl: Mutbl,
}

/// One projection of a place.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Proj {
    /// `.index` of a struct, tuple, variant or closure, whose type is `ty`.
    Field { index: u32, ty: PTy },
    /// `*` through a reference or a raw pointer: not owned.
    Deref,
    /// `*` through a `Box`: owned, so moves out through it are allowed (the Reference).
    DerefBox,
    /// `[i]` with the index held in a local.
    Index(Local),
    /// `[offset]` of a slice pattern, counted from the end when `from_end`; the slice is at
    /// least `min_length` long.
    ConstIndex { offset: u32, min_length: u32, from_end: bool },
    /// `[from..to]` of a slice pattern's rest; `to` counted from the end when `from_end`.
    Subslice { from: u32, to: u32, from_end: bool },
    /// The place viewed as enum variant `variant` (before reading its fields).
    Downcast(u32),
}

/// A row of the place table.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct PlaceRow {
    pub local: Local,
    pub parent: Option<Place>,
    pub proj: Option<Proj>,
    /// Projections from the local: 0 for the local itself.
    pub depth: u32,
    /// A `Field` of a type that implements `Drop`: not movable out of (the Reference).
    pub of_drop_type: bool,
}

/// What a constant is.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ConstKind {
    /// A literal's bits (an integer, `bool`, `char` or float), little end first.
    Scalar { bits: u128 },
    /// A string or byte string literal: bytes `start..start + len` of the data table.
    Bytes { start: u32, len: u32 },
    /// A `const` item at generic arguments (a list of the checked record's type table).
    Item { key: ItemKey, args: List },
    /// The body's promoted constant at this index (`&3` lifted to `'static`).
    Promoted(u32),
    /// A fn item as a value (zero-sized), at generic arguments.
    FnItem { key: ItemKey, args: List },
    /// The address of a `static` item.
    StaticAddr(ItemKey),
    /// An inline `const { .. }` block, by its expression.
    Anon(ExprId),
    /// The item's const generic parameter at this index (`N` in `[T; N]`).
    Param(u32),
}

/// One constant's row: what it is and its type.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ConstDecl {
    pub kind: ConstKind,
    pub ty: PTy,
}

/// An operand: a place read by copy, a place moved out of, or a constant.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Operand {
    Copy(Place),
    Move(Place),
    Const(Const),
}

/// The kind of a borrow, as in blaze-flow.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum BorrowKind {
    /// `&place`.
    Shared,
    /// `&mut place`.
    Mut,
    /// A `&mut` reserved (acting as shared) until its `Activate` (RFC 2025).
    TwoPhase,
    /// A guard's fake borrow: shallow, conflicts only with writes.
    Shallow,
}

/// A binary operator on primitive operands (an overloaded operator is a call).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    BitXor,
    BitAnd,
    BitOr,
    Shl,
    Shr,
    Eq,
    Lt,
    Le,
    Ne,
    Ge,
    Gt,
    /// Three-way comparison to `Ordering` (`Ord::cmp` on a primitive).
    Cmp,
    /// Pointer plus element count (`ptr.offset(n)`).
    Offset,
}

/// A unary operator on primitive operands.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum UnOp {
    /// `!x`.
    Not,
    /// `-x`.
    Neg,
    /// A wide pointer's metadata (a slice's length, a trait object's vtable).
    PtrMetadata,
}

/// The kind of an `as` cast or a coercion that changes representation.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum CastKind {
    IntToInt,
    IntToFloat,
    FloatToInt,
    FloatToFloat,
    /// `*const T as *const U` (thin or with the same metadata).
    PtrToPtr,
    /// A fn pointer to a raw pointer.
    FnPtrToPtr,
    /// A pointer to its address, exposing its provenance (`ptr as usize`).
    ExposeAddr,
    /// An address to a pointer with exposed provenance (`usize as *const T`).
    FromExposedAddr,
    /// `transmute`: the same bits at another type.
    Transmute,
    /// An unsizing coercion (`&[T; N]` to `&[T]`, `&T` to `&dyn Trait`).
    Unsize,
    /// A fn item to a fn pointer.
    ReifyFnPointer,
    /// A non-capturing closure to a fn pointer.
    ClosureFnPointer,
    /// A safe fn pointer to an `unsafe` one.
    UnsafeFnPointer,
    /// `*mut T` to `*const T`.
    MutToConstPointer,
    /// `*const [T; N]` to `*const T`.
    ArrayToPointer,
}

/// What an aggregate builds.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum AggregateKind {
    Tuple,
    Array,
    /// A struct (variant 0) or an enum's variant, its fields in order.
    Adt(u32),
    /// A closure from its captures.
    Closure,
    /// A coroutine (an async block's future) from its upvars.
    Coroutine,
}

/// The right side of an assignment (Rust's "rvalue").
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Value {
    Use(Operand),
    /// `&place` of a kind, issuing `loan`.
    Borrow { kind: BorrowKind, place: Place, loan: Loan },
    /// `&raw const place` or `&raw mut place`: no loan.
    RawAddr { mutbl: Mutbl, place: Place },
    Binary { op: BinOp, lhs: Operand, rhs: Operand },
    /// The result and an overflow flag, as a pair (what an overflow check asserts on).
    CheckedBinary { op: BinOp, lhs: Operand, rhs: Operand },
    Unary { op: UnOp, operand: Operand },
    Cast { kind: CastKind, operand: Operand, ty: PTy },
    /// A value of type `ty` built from operands (a range of the operand table).
    Aggregate { kind: AggregateKind, ty: PTy, operands: Ops },
    /// `[operand; count]`.
    Repeat { operand: Operand, count: Const },
    /// The length of an array or slice place.
    Len(Place),
    /// The discriminant of an enum place.
    Discriminant(Place),
    /// `size_of::<ty>()`.
    SizeOf(PTy),
    /// `align_of::<ty>()`.
    AlignOf(PTy),
    /// The address of a `#[thread_local]` static (a [`ConstKind::StaticAddr`] constant) on
    /// this thread.
    ThreadLocalAddr(Const),
}

/// Where a terminator goes when what it runs panics.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Unwind {
    /// Out of this body to its caller, with nothing to clean up here.
    Continue,
    /// Into this cleanup block (drops, then `Resume`).
    Cleanup(Block),
    /// It cannot unwind.
    Unreachable,
    /// Abort: a panic here is a panic in a destructor or a `nounwind` frame.
    Terminate,
}

/// What a call calls.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Callee {
    /// A fn item (free, inherent or a ctor) at generic arguments.
    FnItem { key: ItemKey, args: List },
    /// A trait method at the trait's and the method's arguments: resolved to an impl or a
    /// vtable slot when the instance is ground.
    Method { key: ItemKey, args: List },
    /// A value: a fn pointer, or a closure called through it.
    Value(Operand),
    /// A compiler intrinsic, by its item, at generic arguments.
    Intrinsic { key: ItemKey, args: List },
}

/// What an `Assert` checks, for its panic message; its operands are the assert's `msg` range.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum AssertKind {
    /// An arithmetic overflow of `op` (operands: the two sides).
    Overflow(BinOp),
    /// `-x` of the minimum value (operand: `x`).
    OverflowNeg,
    /// An index past a length (operands: the length, the index).
    Bounds,
    /// Division by zero (operand: the dividend).
    DivisionByZero,
    /// Remainder by zero (operand: the dividend).
    RemainderByZero,
    /// A coroutine resumed after it returned.
    ResumedAfterReturn,
    /// A coroutine resumed after it panicked.
    ResumedAfterPanic,
}

/// One row of the statement array: a statement, or a terminator (the last row of a block).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum StmtKind {
    /// `dest = value`.
    Assign { dest: Place, value: Value },
    /// The local's storage begins (it is uninitialized).
    StorageLive(Local),
    /// The local's storage ends.
    StorageDead(Local),
    /// A two-phase borrow becomes a mutable one (RFC 2025).
    Activate(Loan),
    /// A read of a place that is not a use: a `match` scrutinee the arms inspect.
    FakeRead(Place),
    /// Writes the tag of an enum place's `variant` (an index of its variants), its fields
    /// written before by their own assignments; MIR's `SetDiscriminant`.
    SetDiscriminant { place: Place, variant: u32 },
    /// The place's bytes become uninitialized (its value moved out whole, nothing dropped):
    /// MIR's `Deinit`, which an aggregate's field-by-field build starts with.
    Deinit(Place),
    /// Terminator: go to a block.
    Goto(Block),
    /// Terminator: on `on`'s value, go to the case's block (a range of the case table,
    /// values ascending, each once), else `otherwise`.
    Switch { on: Operand, cases: Ops, otherwise: Block },
    /// Terminator: call `callee` with the argument operands; on return write `dest` and go to
    /// `target` (both absent: the call diverges); on a panic go by `unwind`. Its lifetime
    /// constraints are the subsets at its Mid.
    Call { callee: CalleeId, args: Ops, dest: Option<Place>, target: Option<Block>, unwind: Unwind },
    /// Terminator: drop `place` if it is initialized (un-elaborated), then `target`.
    Drop { place: Place, target: Block, unwind: Unwind },
    /// Terminator: go on to `target` if `cond` is `expected`, else panic with `kind`.
    Assert { cond: Operand, expected: bool, kind: AssertKind, msg: Ops, target: Block, unwind: Unwind },
    /// Terminator: an `.await`'s suspension: polled again, go on at `resume`; dropped while
    /// parked here, go to `cancel` (coroutines.md D2).
    Suspend { resume: Block, cancel: Block },
    /// Terminator: return `_0` to the caller.
    Return,
    /// Terminator: this point is never reached.
    Unreachable,
    /// Terminator: go on unwinding to the caller (the end of a cleanup path).
    Resume,
    /// Terminator: abort the process (a panic while unwinding).
    Terminate,
    /// Terminator: an `asm!` (its ordinal among the body's `asm!`s) with operands; then
    /// `target`, or none when it is `noreturn`.
    InlineAsm { asm: u32, operands: Ops, target: Option<Block>, unwind: Unwind },
}

impl StmtKind {
    /// Whether this row ends a block.
    pub fn is_terminator(self) -> bool {
        !matches!(
            self,
            StmtKind::Assign { .. }
                | StmtKind::StorageLive(_)
                | StmtKind::StorageDead(_)
                | StmtKind::Activate(_)
                | StmtKind::FakeRead(_)
                | StmtKind::SetDiscriminant { .. }
                | StmtKind::Deinit(_)
        )
    }
}

/// A statement row and where it came from.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct StmtRow {
    pub kind: StmtKind,
    pub span: Span,
}

/// Whether a block is ordinary code or a cleanup path, run only while unwinding.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum BlockKind {
    Normal,
    Cleanup,
}

/// A block's row: its statements (the last its terminator) and its kind.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct BlockRow {
    pub stmts: Ops,
    pub kind: BlockKind,
}

/// A loan's row, as in blaze-flow: the place borrowed, how, into which origin, and where.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct LoanRow {
    pub place: Place,
    pub kind: BorrowKind,
    pub origin: Origin,
    pub at: Stmt,
}

/// `subset_base(from, into, Mid(at))`: `from` outlives `into` from the statement on.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Subset {
    pub at: Stmt,
    pub from: Origin,
    pub into: Origin,
}

/// How a block edge is taken.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Edge {
    /// On the terminator's ordinary completion.
    Normal,
    /// On a panic in what it runs.
    Unwind,
}

/// A table of the body, named in errors.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Table {
    Locals,
    LocalPlaces,
    Places,
    Consts,
    Data,
    Callees,
    Stmts,
    Spans,
    StmtBlocks,
    Blocks,
    Operands,
    Cases,
    Loans,
    Subsets,
    Origins,
    LocalOrigins,
    Known,
    /// The body's own types, numbered after the record's.
    Types,
    /// The entries of the body's own type lists, numbered after the record's.
    TyLists,
}

/// A row that is past its table or is no form of it: named by table, row and the code (the
/// row's tag or kind word; [`NONE`] for a row past the table).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct BadRow {
    pub table: Table,
    pub row: u32,
    pub code: u32,
}

/// One body: every table flat, every id an index into one of them, every row fixed-width.
pub struct Body<A: Allocator> {
    pub(crate) locals: Vec<LocalWords, A>,
    /// Each local's own place (depth 0).
    pub(crate) local_places: Vec<u32, A>,
    pub(crate) places: Vec<PlaceWords, A>,
    pub(crate) consts: Vec<ConstWords, A>,
    /// The bytes of the string and byte-string constants.
    pub(crate) data: Vec<u8, A>,
    pub(crate) callees: Vec<CalleeWords, A>,
    pub(crate) stmts: Vec<StmtWords, A>,
    pub(crate) spans: Vec<Span, A>,
    pub(crate) stmt_block: Vec<u32, A>,
    pub(crate) blocks: Vec<BlockWords, A>,
    pub(crate) operands: Vec<OperandWords, A>,
    pub(crate) cases: Vec<CaseWords, A>,
    pub(crate) loans: Vec<LoanWords, A>,
    /// In statement order.
    pub(crate) subsets: Vec<SubsetWords, A>,
    /// One byte per origin: 1 for a placeholder (a lifetime of the signature), else 0.
    pub(crate) placeholder: Vec<u8, A>,
    /// `(local, origin)`: the local's type mentions the origin. Sorted, each once.
    pub(crate) local_origins: Vec<PairWords, A>,
    /// Known `from: into` between placeholders (the signature's bounds).
    pub(crate) known: Vec<PairWords, A>,
    /// The record's type count: the overlay's first type is `PTy(ty_base)`. [`NONE`] when the
    /// body has no overlay, every handle then the record's.
    pub(crate) ty_base: u32,
    /// The record's list-entry count: the overlay's first list entry is at `list_base`.
    /// [`NONE`] with `ty_base`.
    pub(crate) list_base: u32,
    /// The overlay: types the record's table lacks, each made once, children before parents.
    pub(crate) tys: Vec<TyWords, A>,
    /// The overlay's list entries (type handles), the lists its types name.
    pub(crate) ty_lists: Vec<u32, A>,
    /// The record table this body's type handles index, as [`crate::kept::TableId::of`]
    /// names it: set by [`crate::BodyBuilder::new_with_types_in`], `None` for a body built
    /// or parsed without one. Not part of the body record's bytes (record.rs): the kept
    /// record (kept.rs) writes it beside the table, and refuses a body against another.
    pub(crate) table: Option<crate::kept::TableId>,
}

impl<A: Allocator> Body<A> {
    /// The record table this body was built against ([`crate::kept::TableId`]); `None` when
    /// it was built or parsed without one.
    pub fn table(&self) -> Option<crate::kept::TableId> {
        self.table
    }

    /// The body's reader.
    pub fn view(&self) -> BodyRef<'_> {
        BodyRef {
            locals: &self.locals,
            local_places: &self.local_places,
            places: &self.places,
            consts: &self.consts,
            data: &self.data,
            callees: &self.callees,
            stmts: &self.stmts,
            spans: &self.spans,
            stmt_block: &self.stmt_block,
            blocks: &self.blocks,
            operands: &self.operands,
            cases: &self.cases,
            loans: &self.loans,
            subsets: &self.subsets,
            placeholder: &self.placeholder,
            local_origins: &self.local_origins,
            known: &self.known,
            ty_base: self.ty_base,
            list_base: self.list_base,
            tys: &self.tys,
            ty_lists: &self.ty_lists,
        }
    }
}

/// A body read in place: of a [`Body`] in memory or of a record. Rows decode when asked.
#[derive(Clone, Copy)]
pub struct BodyRef<'b> {
    pub(crate) locals: &'b [LocalWords],
    pub(crate) local_places: &'b [u32],
    pub(crate) places: &'b [PlaceWords],
    pub(crate) consts: &'b [ConstWords],
    pub(crate) data: &'b [u8],
    pub(crate) callees: &'b [CalleeWords],
    pub(crate) stmts: &'b [StmtWords],
    pub(crate) spans: &'b [Span],
    pub(crate) stmt_block: &'b [u32],
    pub(crate) blocks: &'b [BlockWords],
    pub(crate) operands: &'b [OperandWords],
    pub(crate) cases: &'b [CaseWords],
    pub(crate) loans: &'b [LoanWords],
    pub(crate) subsets: &'b [SubsetWords],
    pub(crate) placeholder: &'b [u8],
    pub(crate) local_origins: &'b [PairWords],
    pub(crate) known: &'b [PairWords],
    pub(crate) ty_base: u32,
    pub(crate) list_base: u32,
    pub(crate) tys: &'b [TyWords],
    pub(crate) ty_lists: &'b [u32],
}

/// Why a type handle or list does not resolve against a record's table and the body's overlay.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TyError {
    /// An overlay row that is no type, or past the overlay.
    Row(BadRow),
    /// A handle below the overlay that the record's table does not hold.
    NotInRecord(PTy),
    /// A list below the overlay that the record's table does not hold.
    ListNotInRecord(List),
    /// A list that starts below the overlay and ends in it.
    ListStraddles(List),
    /// The record given is not the one the overlay was numbered after: its type or list count
    /// is not the body's base.
    WrongRecord { types: u32, lists: u32, ty_base: u32, list_base: u32 },
}

impl From<BadRow> for TyError {
    fn from(b: BadRow) -> TyError {
        TyError::Row(b)
    }
}

/// A list's entries: the record's, or the overlay's (stored as words).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TyListRef<'b, 'r> {
    Record(&'r [PTy]),
    Own(&'b [u32]),
}

impl TyListRef<'_, '_> {
    /// How many entries.
    pub fn len(&self) -> usize {
        match self {
            TyListRef::Record(s) => s.len(),
            TyListRef::Own(s) => s.len(),
        }
    }

    /// Whether it has no entries.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Entry `i`.
    pub fn get(&self, i: usize) -> Option<PTy> {
        match self {
            TyListRef::Record(s) => s.get(i).copied(),
            TyListRef::Own(s) => s.get(i).copied().map(PTy),
        }
    }

    /// The entries in order.
    pub fn iter(&self) -> impl Iterator<Item = PTy> + '_ {
        (0..self.len()).filter_map(|i| self.get(i))
    }
}

fn row<T: Copy>(table: Table, all: &[T], i: u32) -> Result<T, BadRow> {
    all.get(i as usize).copied().ok_or(BadRow { table, row: i, code: NONE })
}

fn count(n: usize) -> u32 {
    // Every table's length is under `NONE`: the builder refuses a row past it, and a record's
    // slice lengths are `u32`s.
    n as u32
}

impl<'b> BodyRef<'b> {
    /// How many locals.
    pub fn locals(&self) -> u32 {
        count(self.locals.len())
    }

    pub fn local(&self, l: Local) -> Result<LocalDecl, BadRow> {
        let w = row(Table::Locals, self.locals, l.0)?;
        decode_local(w).ok_or(BadRow { table: Table::Locals, row: l.0, code: w.kind })
    }

    /// The place that is local `l` itself.
    pub fn local_place(&self, l: Local) -> Result<Place, BadRow> {
        row(Table::LocalPlaces, self.local_places, l.0).map(Place)
    }

    /// How many arguments: the locals `1..=args`.
    pub fn args(&self) -> u32 {
        count(self.locals.iter().filter(|w| matches!(decode_local(**w), Some(LocalDecl { kind: LocalKind::Arg(_), .. }))).count())
    }

    /// How many places.
    pub fn places(&self) -> u32 {
        count(self.places.len())
    }

    pub fn place(&self, p: Place) -> Result<PlaceRow, BadRow> {
        let w = row(Table::Places, self.places, p.0)?;
        decode_place(w).ok_or(BadRow { table: Table::Places, row: p.0, code: u32::from(w.proj) })
    }

    /// How many constants.
    pub fn consts(&self) -> u32 {
        count(self.consts.len())
    }

    pub fn konst(&self, c: Const) -> Result<ConstDecl, BadRow> {
        let w = row(Table::Consts, self.consts, c.0)?;
        decode_const(w).ok_or(BadRow { table: Table::Consts, row: c.0, code: w.kind })
    }

    /// The bytes of a [`ConstKind::Bytes`] constant.
    pub fn bytes(&self, c: Const) -> Result<&'b [u8], BadRow> {
        let w = row(Table::Consts, self.consts, c.0)?;
        let decl = decode_const(w).ok_or(BadRow { table: Table::Consts, row: c.0, code: w.kind })?;
        match decl.kind {
            ConstKind::Bytes { start, len } => {
                let end = start as usize + len as usize;
                self.data.get(start as usize..end).ok_or(BadRow { table: Table::Data, row: start, code: len })
            }
            _ => Err(BadRow { table: Table::Consts, row: c.0, code: w.kind }),
        }
    }

    /// The data table: every string and byte-string constant's bytes.
    pub fn data(&self) -> &'b [u8] {
        self.data
    }

    /// How many callees.
    pub fn callees(&self) -> u32 {
        count(self.callees.len())
    }

    pub fn callee(&self, f: CalleeId) -> Result<Callee, BadRow> {
        let w = row(Table::Callees, self.callees, f.0)?;
        decode_callee(w).ok_or(BadRow { table: Table::Callees, row: f.0, code: w.kind })
    }

    /// How many statement rows (terminators included).
    pub fn stmts(&self) -> u32 {
        count(self.stmts.len())
    }

    pub fn stmt(&self, s: Stmt) -> Result<StmtRow, BadRow> {
        let w = row(Table::Stmts, self.stmts, s.0)?;
        let kind = decode_stmt(w).ok_or(BadRow { table: Table::Stmts, row: s.0, code: u32::from(w.tag) })?;
        Ok(StmtRow { kind, span: row(Table::Spans, self.spans, s.0)? })
    }

    /// The block statement `s` is in.
    pub fn block_of(&self, s: Stmt) -> Result<Block, BadRow> {
        row(Table::StmtBlocks, self.stmt_block, s.0).map(Block)
    }

    /// How many blocks.
    pub fn blocks(&self) -> u32 {
        count(self.blocks.len())
    }

    pub fn block(&self, b: Block) -> Result<BlockRow, BadRow> {
        let w = row(Table::Blocks, self.blocks, b.0)?;
        decode_block(w).ok_or(BadRow { table: Table::Blocks, row: b.0, code: w.kind })
    }

    /// The operand at row `i` of the operand table (a row of some [`Ops`]).
    pub fn operand(&self, i: u32) -> Result<Operand, BadRow> {
        let w = row(Table::Operands, self.operands, i)?;
        decode_operand(w).ok_or(BadRow { table: Table::Operands, row: i, code: w.kind })
    }

    /// The case at row `i` of the case table: a value and its block.
    pub fn case(&self, i: u32) -> Result<(u128, Block), BadRow> {
        decode_case(row(Table::Cases, self.cases, i)?).ok_or(BadRow { table: Table::Cases, row: i, code: NONE })
    }

    /// How many loans.
    pub fn loans(&self) -> u32 {
        count(self.loans.len())
    }

    pub fn loan(&self, l: Loan) -> Result<LoanRow, BadRow> {
        let w = row(Table::Loans, self.loans, l.0)?;
        decode_loan(w).ok_or(BadRow { table: Table::Loans, row: l.0, code: w.kind })
    }

    /// The subsets, in statement order.
    pub fn subsets(&self) -> impl Iterator<Item = Subset> + 'b {
        self.subsets.iter().map(|w| Subset { at: Stmt(w.at), from: Origin(w.from), into: Origin(w.into) })
    }

    /// How many origins.
    pub fn origins(&self) -> u32 {
        count(self.placeholder.len())
    }

    /// Whether origin `o` is a placeholder: a lifetime of the signature.
    pub fn placeholder(&self, o: Origin) -> Result<bool, BadRow> {
        match row(Table::Origins, self.placeholder, o.0)? {
            0 => Ok(false),
            1 => Ok(true),
            other => Err(BadRow { table: Table::Origins, row: o.0, code: u32::from(other) }),
        }
    }

    /// `(local, origin)`: the local's type mentions the origin. Sorted, each once.
    pub fn local_origins(&self) -> impl Iterator<Item = (Local, Origin)> + 'b {
        self.local_origins.iter().map(|w| (Local(w.a), Origin(w.b)))
    }

    /// Known `from: into` between placeholders (the signature's bounds).
    pub fn known(&self) -> impl Iterator<Item = (Origin, Origin)> + 'b {
        self.known.iter().map(|w| (Origin(w.a), Origin(w.b)))
    }

    /// The record's type count the overlay is numbered after; `None` when the body has no
    /// overlay (every type handle is then the record's).
    pub fn ty_base(&self) -> Option<u32> {
        (self.ty_base != NONE).then_some(self.ty_base)
    }

    /// The record's list-entry count the overlay's lists are numbered after; `None` with
    /// [`Self::ty_base`].
    pub fn list_base(&self) -> Option<u32> {
        (self.list_base != NONE).then_some(self.list_base)
    }

    /// How many types the overlay holds.
    pub fn own_tys(&self) -> u32 {
        count(self.tys.len())
    }

    /// How many list entries the overlay holds.
    pub fn own_list_entries(&self) -> u32 {
        count(self.ty_lists.len())
    }

    /// The overlay's node for `t`; `Ok(None)` for a handle below the overlay (the record's).
    pub fn own_ty(&self, t: PTy) -> Result<Option<PNode>, BadRow> {
        if self.ty_base == NONE || t.0 < self.ty_base {
            return Ok(None);
        }
        let i = t.0 - self.ty_base;
        let w = row(Table::Types, self.tys, i)?;
        decode_ty(w).map(Some).ok_or(BadRow { table: Table::Types, row: i, code: w.tag })
    }

    /// The overlay's entries of `l`; `Ok(None)` for a list wholly below the overlay (the
    /// record's).
    pub fn own_list(&self, l: List) -> Result<Option<&'b [u32]>, TyError> {
        let end = u64::from(l.start) + u64::from(l.len);
        if self.list_base == NONE || end <= u64::from(self.list_base) {
            return Ok(None);
        }
        if l.start < self.list_base {
            return Err(TyError::ListStraddles(l));
        }
        let from = (l.start - self.list_base) as usize;
        match self.ty_lists.get(from..from + l.len as usize) {
            Some(s) => Ok(Some(s)),
            None => Err(TyError::Row(BadRow { table: Table::TyLists, row: from as u32, code: l.len })),
        }
    }

    fn same_record(&self, record: &PTys<'_>) -> Result<(), TyError> {
        let (types, lists) = (count(record.len()), count(record.lists_len()));
        match self.ty_base == NONE || (types == self.ty_base && lists == self.list_base) {
            true => Ok(()),
            false => Err(TyError::WrongRecord { types, lists, ty_base: self.ty_base, list_base: self.list_base }),
        }
    }

    /// The node of any type handle the body names: the record's below the overlay, the
    /// overlay's from [`Self::ty_base`] on. `record` must be the table the body was built
    /// against.
    pub fn ty(&self, record: &PTys<'_>, t: PTy) -> Result<PNode, TyError> {
        self.same_record(record)?;
        match self.own_ty(t)? {
            Some(n) => Ok(n),
            None => record.node(t).map_err(|_| TyError::NotInRecord(t)),
        }
    }

    /// The entries of any list the body names, the record's or the overlay's.
    pub fn ty_list<'r>(&self, record: &PTys<'r>, l: List) -> Result<TyListRef<'b, 'r>, TyError> {
        self.same_record(record)?;
        match self.own_list(l)? {
            Some(s) => Ok(TyListRef::Own(s)),
            None => record.list(l).map(TyListRef::Record).map_err(|_| TyError::ListNotInRecord(l)),
        }
    }

    /// Calls `f` on each block edge out of a terminator, with how it is taken; nothing for a
    /// statement (it falls through to the next row).
    pub fn for_each_edge(&self, kind: &StmtKind, mut f: impl FnMut(Block, Edge)) -> Result<(), BadRow> {
        fn unwind(u: Unwind, f: &mut impl FnMut(Block, Edge)) {
            if let Unwind::Cleanup(b) = u {
                f(b, Edge::Unwind);
            }
        }
        match *kind {
            StmtKind::Goto(b) => f(b, Edge::Normal),
            StmtKind::Switch { cases, otherwise, .. } => {
                for i in cases.start..cases.end {
                    f(self.case(i)?.1, Edge::Normal);
                }
                f(otherwise, Edge::Normal);
            }
            StmtKind::Call { target, unwind: u, .. } | StmtKind::InlineAsm { target, unwind: u, .. } => {
                if let Some(t) = target {
                    f(t, Edge::Normal);
                }
                unwind(u, &mut f);
            }
            StmtKind::Drop { target, unwind: u, .. } | StmtKind::Assert { target, unwind: u, .. } => {
                f(target, Edge::Normal);
                unwind(u, &mut f);
            }
            StmtKind::Suspend { resume, cancel } => {
                f(resume, Edge::Normal);
                f(cancel, Edge::Normal);
            }
            StmtKind::Return
            | StmtKind::Unreachable
            | StmtKind::Resume
            | StmtKind::Terminate
            | StmtKind::Assign { .. }
            | StmtKind::StorageLive(_)
            | StmtKind::StorageDead(_)
            | StmtKind::Activate(_)
            | StmtKind::FakeRead(_)
            | StmtKind::SetDiscriminant { .. }
            | StmtKind::Deinit(_) => {}
        }
        Ok(())
    }
}
