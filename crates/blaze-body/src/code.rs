//! The stable encoding of every stored form, in one place: the fixed-width row of each table,
//! the number (and text name) of each enum, and the encode and decode of each form with fields.
//!
//! A number is never reused: a form that goes keeps its number retired, and a change of what a
//! row means is a new [`crate::record::VERSION`]. A decode accepts only the words its encode
//! writes (every unused word [`NONE`], every unused flag 0), so decode then encode gives back the
//! same bytes (rule 26), and a row that is no form is refused, never read as a near one.
//!
//! The statement row: a `tag` ([`tag`]), six words `a..f` and sixteen bits of flags:
//!
//! | flags bits | meaning |
//! |---|---|
//! | 0-1 | the first operand's kind ([`operand_bits`]: 0 copy, 1 move, 2 const) |
//! | 2-3 | the second operand's kind |
//! | 4-5 | the unwind's kind (0 continue, 1 cleanup block in `f`, 2 unreachable, 3 terminate) |
//! | 6 | an `Assert`'s expected value |
//!
//! | tag | words |
//! |---|---|
//! | `USE` .. `TLS` (assignments) | `a` dest; `b..f` the value, as [`encode_stmt`] writes it |
//! | `STORAGE_LIVE`, `STORAGE_DEAD` | `a` local |
//! | `ACTIVATE` | `a` loan |
//! | `FAKE_READ` | `a` place |
//! | `GOTO` | `a` block |
//! | `SWITCH` | `a` operand, `b..c` cases, `d` otherwise |
//! | `CALL` | `a` callee, `b..c` args, `d` dest, `e` target, `f` unwind block |
//! | `DROP` | `a` place, `b` target, `f` unwind block |
//! | `ASSERT` | `a` cond, `b` target, `c` kind (low byte) and op (next byte), `d..e` msg, `f` unwind block |
//! | `SUSPEND` | `a` resume, `b` cancel |
//! | `INLINE_ASM` | `a` asm ordinal, `b..c` operands, `e` target, `f` unwind block |
//! | `SET_DISCRIMINANT` | `a` place, `b` variant index |
//! | `DEINIT` | `a` place |
//!
//! The overlay type row ([`TyWords`]): a `tag` ([`ty_tag`]) and nine words, as [`encode_ty`]
//! writes them; an item key is four words (little end first), a name two, a list two (start,
//! length), every word a form leaves unused [`NONE`].
#![blaze::sans_io]

use blaze_typeck::{FloatTy, FnPtrHeader, IntTy, Name, PNode, RecordSchema, UintTy};

use crate::body::{
    AggregateKind, AssertKind, BinOp, BlockKind, BlockRow, BorrowKind, Block, Callee, CalleeId,
    CastKind, Const, ConstDecl, ConstKind, ExprId, ItemKey, List, Loan, LoanRow, Local, LocalDecl,
    LocalKind, Mutbl, NONE, Operand, Ops, Origin, PTy, PatId, Place, PlaceRow, Proj, Role, Stmt,
    StmtKind, UnOp, Unwind, Value,
};

blaze_format_core::record! {
    /// A statement or terminator row (the module's table says what each word holds).
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
    pub struct StmtWords: 1 {
        pub a: u32,
        pub b: u32,
        pub c: u32,
        pub d: u32,
        pub e: u32,
        pub f: u32,
        pub tag: u16,
        pub flags: u16,
    }
}

blaze_format_core::record! {
    /// A local's row: its type, its kind's number ([`encode_local`]) and two words of identity.
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
    pub struct LocalWords: 1 {
        pub ty: u32,
        pub kind: u32,
        pub a: u32,
        pub b: u32,
        pub mutbl: u32,
    }
}

blaze_format_core::record! {
    /// A place's row: its local, its parent ([`NONE`] for a local itself), the projection's
    /// number and two words, its depth, and flags (bit 0 of a drop type, bit 1 from the end).
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
    pub struct PlaceWords: 1 {
        pub local: u32,
        pub parent: u32,
        pub a: u32,
        pub b: u32,
        pub depth: u32,
        pub proj: u16,
        pub flags: u16,
    }
}

blaze_format_core::record! {
    /// A constant's row: 128 bits (a scalar's or an item's key, little end first), its type,
    /// its kind's number and two words.
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
    pub struct ConstWords: 1 {
        pub bits: [u32; 4],
        pub ty: u32,
        pub kind: u32,
        pub a: u32,
        pub b: u32,
    }
}

blaze_format_core::record! {
    /// A callee's row: an item's key, its kind's number and up to three words.
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
    pub struct CalleeWords: 1 {
        pub key: [u32; 4],
        pub kind: u32,
        pub a: u32,
        pub b: u32,
        pub c: u32,
    }
}

blaze_format_core::record! {
    /// An operand's row: its kind's number and its place or constant.
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
    pub struct OperandWords: 1 {
        pub kind: u32,
        pub id: u32,
    }
}

blaze_format_core::record! {
    /// A switch case's row: the value (little end first) and its block.
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
    pub struct CaseWords: 1 {
        pub value: [u32; 4],
        pub target: u32,
    }
}

blaze_format_core::record! {
    /// A block's row: its statements' range and its kind's number.
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
    pub struct BlockWords: 1 {
        pub start: u32,
        pub end: u32,
        pub kind: u32,
    }
}

blaze_format_core::record! {
    /// A loan's row.
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
    pub struct LoanWords: 1 {
        pub place: u32,
        pub kind: u32,
        pub origin: u32,
        pub at: u32,
    }
}

blaze_format_core::record! {
    /// A subset's row: the statement and the two origins.
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
    pub struct SubsetWords: 1 {
        pub at: u32,
        pub from: u32,
        pub into: u32,
    }
}

blaze_format_core::record! {
    /// A pair of ids: `(local, origin)` of a mention, `(from, into)` of a known relation.
    #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
    pub struct PairWords: 1 {
        pub a: u32,
        pub b: u32,
    }
}

blaze_format_core::record! {
    /// A type of the body's overlay: its form's number ([`ty_tag`]) and nine words (the widest
    /// form, `ExProj`, is a key, a name, a list and a type: 4 + 2 + 2 + 1).
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
    pub struct TyWords: 1 {
        pub w: [u32; 9],
        pub tag: u32,
    }
}

/// The overlay type tags, one per form of blaze-typeck's `PNode`. 0 is no form.
pub mod ty_tag {
    pub const BOOL: u32 = 1;
    pub const CHAR: u32 = 2;
    pub const INT: u32 = 3;
    pub const UINT: u32 = 4;
    pub const FLOAT: u32 = 5;
    pub const STR: u32 = 6;
    pub const NEVER: u32 = 7;
    pub const TUPLE: u32 = 8;
    pub const REF: u32 = 9;
    pub const PTR: u32 = 10;
    pub const ARRAY: u32 = 11;
    pub const SLICE: u32 = 12;
    pub const PAT: u32 = 13;
    pub const ADT: u32 = 14;
    pub const FN_DEF: u32 = 15;
    pub const FN_PTR: u32 = 16;
    pub const CLOSURE: u32 = 17;
    pub const FUTURE: u32 = 18;
    pub const COROUTINE: u32 = 19;
    pub const PARAM: u32 = 20;
    pub const ALIAS: u32 = 21;
    pub const OPAQUE: u32 = 22;
    pub const INFER: u32 = 23;
    pub const ERROR: u32 = 24;
    pub const CONST_VALUE: u32 = 25;
    pub const CONST_PARAM: u32 = 26;
    pub const DYNAMIC: u32 = 27;
    pub const EX_TRAIT: u32 = 28;
    pub const EX_PROJ: u32 = 29;
    pub const EX_AUTO: u32 = 30;
    pub const FOREIGN: u32 = 31;
    pub const RECORD: u32 = 32;
    pub const UNION: u32 = 33;
}

/// The statement tags. 0 is no form.
pub mod tag {
    pub const USE: u16 = 1;
    pub const BORROW: u16 = 2;
    pub const RAW_ADDR: u16 = 3;
    pub const BINARY: u16 = 4;
    pub const CHECKED_BINARY: u16 = 5;
    pub const UNARY: u16 = 6;
    pub const CAST: u16 = 7;
    pub const AGGREGATE: u16 = 8;
    pub const REPEAT: u16 = 9;
    pub const LEN: u16 = 10;
    pub const DISCRIMINANT: u16 = 11;
    pub const SIZE_OF: u16 = 12;
    pub const ALIGN_OF: u16 = 13;
    pub const THREAD_LOCAL: u16 = 14;
    pub const STORAGE_LIVE: u16 = 20;
    pub const STORAGE_DEAD: u16 = 21;
    pub const ACTIVATE: u16 = 22;
    pub const FAKE_READ: u16 = 23;
    pub const SET_DISCRIMINANT: u16 = 24;
    pub const DEINIT: u16 = 25;
    pub const GOTO: u16 = 40;
    pub const SWITCH: u16 = 41;
    pub const CALL: u16 = 42;
    pub const DROP: u16 = 43;
    pub const ASSERT: u16 = 44;
    pub const SUSPEND: u16 = 45;
    pub const RETURN: u16 = 46;
    pub const UNREACHABLE: u16 = 47;
    pub const RESUME: u16 = 48;
    pub const TERMINATE: u16 = 49;
    pub const INLINE_ASM: u16 = 50;
}

/// For each fieldless enum: its number and its text name, one table each.
macro_rules! codes {
    ($name:ident { $($var:ident = $code:literal $text:literal),* $(,)? }) => {
        impl $name {
            /// Every form, in number order.
            pub const ALL: &[$name] = &[$($name::$var),*];

            /// Its stored number.
            pub const fn code(self) -> u32 {
                match self {
                    $($name::$var => $code,)*
                }
            }

            /// The form numbered `code`; `None` for a number no form has.
            pub const fn from_code(code: u32) -> Option<$name> {
                match code {
                    $($code => Some($name::$var),)*
                    _ => None,
                }
            }

            /// Its name in the text form.
            pub const fn name(self) -> &'static str {
                match self {
                    $($name::$var => $text,)*
                }
            }

            /// The form named `name` in the text form.
            pub fn from_name(name: &str) -> Option<$name> {
                match name {
                    $($text => Some($name::$var),)*
                    _ => None,
                }
            }
        }
    };
}

codes!(BorrowKind { Shared = 0 "shared", Mut = 1 "mut", TwoPhase = 2 "2ph", Shallow = 3 "shallow" });

codes!(BinOp {
    Add = 0 "add",
    Sub = 1 "sub",
    Mul = 2 "mul",
    Div = 3 "div",
    Rem = 4 "rem",
    BitXor = 5 "bitxor",
    BitAnd = 6 "bitand",
    BitOr = 7 "bitor",
    Shl = 8 "shl",
    Shr = 9 "shr",
    Eq = 10 "eq",
    Lt = 11 "lt",
    Le = 12 "le",
    Ne = 13 "ne",
    Ge = 14 "ge",
    Gt = 15 "gt",
    Cmp = 16 "cmp",
    Offset = 17 "offset",
});

codes!(UnOp { Not = 0 "not", Neg = 1 "neg", PtrMetadata = 2 "meta" });

codes!(CastKind {
    IntToInt = 0 "int_to_int",
    IntToFloat = 1 "int_to_float",
    FloatToInt = 2 "float_to_int",
    FloatToFloat = 3 "float_to_float",
    PtrToPtr = 4 "ptr_to_ptr",
    FnPtrToPtr = 5 "fnptr_to_ptr",
    ExposeAddr = 6 "expose_addr",
    FromExposedAddr = 7 "from_exposed_addr",
    Transmute = 8 "transmute",
    Unsize = 9 "unsize",
    ReifyFnPointer = 10 "reify_fn_pointer",
    ClosureFnPointer = 11 "closure_fn_pointer",
    UnsafeFnPointer = 12 "unsafe_fn_pointer",
    MutToConstPointer = 13 "mut_to_const",
    ArrayToPointer = 14 "array_to_pointer",
});

codes!(Role { Value = 0 "value", Borrow = 1 "borrow", Awaitee = 2 "awaitee", Pin = 3 "pin", Result = 4 "result" });

codes!(BlockKind { Normal = 0 "normal", Cleanup = 1 "cleanup" });

/// `Mutbl`'s number (blaze-typeck's enum, so a function here rather than the table above).
pub const fn mutbl_code(m: Mutbl) -> u32 {
    match m {
        Mutbl::Not => 0,
        Mutbl::Mut => 1,
    }
}

/// The `Mutbl` numbered `code`.
pub const fn mutbl_of(code: u32) -> Option<Mutbl> {
    match code {
        0 => Some(Mutbl::Not),
        1 => Some(Mutbl::Mut),
        _ => None,
    }
}

/// 128 bits as four words, little end first.
pub const fn split(v: u128) -> [u32; 4] {
    [v as u32, (v >> 32) as u32, (v >> 64) as u32, (v >> 96) as u32]
}

/// Four words, little end first, as 128 bits.
pub const fn join(w: [u32; 4]) -> u128 {
    (w[0] as u128) | ((w[1] as u128) << 32) | ((w[2] as u128) << 64) | ((w[3] as u128) << 96)
}

fn opt(v: Option<u32>) -> u32 {
    v.unwrap_or(NONE)
}

fn unopt(v: u32) -> Option<u32> {
    (v != NONE).then_some(v)
}

/// An operand as its kind's number and its id.
pub fn operand_bits(o: Operand) -> (u16, u32) {
    match o {
        Operand::Copy(p) => (0, p.0),
        Operand::Move(p) => (1, p.0),
        Operand::Const(c) => (2, c.0),
    }
}

/// The operand of kind number `kind` and id `id`.
pub fn operand_of(kind: u16, id: u32) -> Option<Operand> {
    match kind {
        0 => Some(Operand::Copy(Place(id))),
        1 => Some(Operand::Move(Place(id))),
        2 => Some(Operand::Const(Const(id))),
        _ => None,
    }
}

/// An unwind as its kind's number and its block word.
pub fn unwind_bits(u: Unwind) -> (u16, u32) {
    match u {
        Unwind::Continue => (0, NONE),
        Unwind::Cleanup(b) => (1, b.0),
        Unwind::Unreachable => (2, NONE),
        Unwind::Terminate => (3, NONE),
    }
}

/// The unwind of kind number `kind` and block word `block`.
pub fn unwind_of(kind: u16, block: u32) -> Option<Unwind> {
    match kind {
        0 => Some(Unwind::Continue),
        1 => Some(Unwind::Cleanup(Block(block))),
        2 => Some(Unwind::Unreachable),
        3 => Some(Unwind::Terminate),
        _ => None,
    }
}

fn aggregate_bits(k: AggregateKind) -> (u32, u32) {
    match k {
        AggregateKind::Tuple => (0, NONE),
        AggregateKind::Array => (1, NONE),
        AggregateKind::Adt(v) => (2, v),
        AggregateKind::Closure => (3, NONE),
        AggregateKind::Coroutine => (4, NONE),
    }
}

fn aggregate_of(code: u32, variant: u32) -> Option<AggregateKind> {
    match code {
        0 => Some(AggregateKind::Tuple),
        1 => Some(AggregateKind::Array),
        2 => Some(AggregateKind::Adt(variant)),
        3 => Some(AggregateKind::Closure),
        4 => Some(AggregateKind::Coroutine),
        _ => None,
    }
}

/// An assert kind as one word: the kind in the low byte, an overflow's operator in the next.
pub fn assert_code(k: AssertKind) -> u32 {
    match k {
        AssertKind::Overflow(op) => op.code() << 8,
        AssertKind::OverflowNeg => 1,
        AssertKind::Bounds => 2,
        AssertKind::DivisionByZero => 3,
        AssertKind::RemainderByZero => 4,
        AssertKind::ResumedAfterReturn => 5,
        AssertKind::ResumedAfterPanic => 6,
    }
}

/// The assert kind of word `code`.
pub fn assert_of(code: u32) -> Option<AssertKind> {
    let k = match code & 0xff {
        0 => AssertKind::Overflow(BinOp::from_code(code >> 8)?),
        1 => AssertKind::OverflowNeg,
        2 => AssertKind::Bounds,
        3 => AssertKind::DivisionByZero,
        4 => AssertKind::RemainderByZero,
        5 => AssertKind::ResumedAfterReturn,
        6 => AssertKind::ResumedAfterPanic,
        _ => return None,
    };
    (assert_code(k) == code).then_some(k)
}

const EMPTY: StmtWords = StmtWords { a: NONE, b: NONE, c: NONE, d: NONE, e: NONE, f: NONE, tag: 0, flags: 0 };

/// A statement or terminator as its row.
pub fn encode_stmt(k: StmtKind) -> StmtWords {
    let mut w = EMPTY;
    match k {
        StmtKind::Assign { dest, value } => {
            w.a = dest.0;
            match value {
                Value::Use(o) => {
                    w.tag = tag::USE;
                    (w.flags, w.b) = operand_bits(o);
                }
                Value::Borrow { kind, place, loan } => {
                    w.tag = tag::BORROW;
                    (w.b, w.c, w.d) = (place.0, loan.0, kind.code());
                }
                Value::RawAddr { mutbl, place } => {
                    w.tag = tag::RAW_ADDR;
                    (w.b, w.d) = (place.0, mutbl_code(mutbl));
                }
                Value::Binary { op, lhs, rhs } | Value::CheckedBinary { op, lhs, rhs } => {
                    w.tag = match value {
                        Value::Binary { .. } => tag::BINARY,
                        _ => tag::CHECKED_BINARY,
                    };
                    let (ka, a) = operand_bits(lhs);
                    let (kb, b) = operand_bits(rhs);
                    (w.b, w.c, w.d, w.flags) = (a, b, op.code(), ka | (kb << 2));
                }
                Value::Unary { op, operand } => {
                    w.tag = tag::UNARY;
                    (w.flags, w.b) = operand_bits(operand);
                    w.d = op.code();
                }
                Value::Cast { kind, operand, ty } => {
                    w.tag = tag::CAST;
                    (w.flags, w.b) = operand_bits(operand);
                    (w.c, w.d) = (ty.0, kind.code());
                }
                Value::Aggregate { kind, ty, operands } => {
                    w.tag = tag::AGGREGATE;
                    (w.b, w.c, w.d) = (ty.0, operands.start, operands.end);
                    (w.e, w.f) = aggregate_bits(kind);
                }
                Value::Repeat { operand, count } => {
                    w.tag = tag::REPEAT;
                    (w.flags, w.b) = operand_bits(operand);
                    w.c = count.0;
                }
                Value::Len(p) => (w.tag, w.b) = (tag::LEN, p.0),
                Value::Discriminant(p) => (w.tag, w.b) = (tag::DISCRIMINANT, p.0),
                Value::SizeOf(t) => (w.tag, w.b) = (tag::SIZE_OF, t.0),
                Value::AlignOf(t) => (w.tag, w.b) = (tag::ALIGN_OF, t.0),
                Value::ThreadLocalAddr(c) => (w.tag, w.b) = (tag::THREAD_LOCAL, c.0),
            }
        }
        StmtKind::StorageLive(l) => (w.tag, w.a) = (tag::STORAGE_LIVE, l.0),
        StmtKind::StorageDead(l) => (w.tag, w.a) = (tag::STORAGE_DEAD, l.0),
        StmtKind::Activate(l) => (w.tag, w.a) = (tag::ACTIVATE, l.0),
        StmtKind::FakeRead(p) => (w.tag, w.a) = (tag::FAKE_READ, p.0),
        StmtKind::SetDiscriminant { place, variant } => (w.tag, w.a, w.b) = (tag::SET_DISCRIMINANT, place.0, variant),
        StmtKind::Deinit(p) => (w.tag, w.a) = (tag::DEINIT, p.0),
        StmtKind::Goto(b) => (w.tag, w.a) = (tag::GOTO, b.0),
        StmtKind::Switch { on, cases, otherwise } => {
            w.tag = tag::SWITCH;
            (w.flags, w.a) = operand_bits(on);
            (w.b, w.c, w.d) = (cases.start, cases.end, otherwise.0);
        }
        StmtKind::Call { callee, args, dest, target, unwind } => {
            w.tag = tag::CALL;
            (w.a, w.b, w.c) = (callee.0, args.start, args.end);
            (w.d, w.e) = (opt(dest.map(|p| p.0)), opt(target.map(|b| b.0)));
            let (k, f) = unwind_bits(unwind);
            (w.f, w.flags) = (f, k << 4);
        }
        StmtKind::Drop { place, target, unwind } => {
            w.tag = tag::DROP;
            (w.a, w.b) = (place.0, target.0);
            let (k, f) = unwind_bits(unwind);
            (w.f, w.flags) = (f, k << 4);
        }
        StmtKind::Assert { cond, expected, kind, msg, target, unwind } => {
            w.tag = tag::ASSERT;
            let (kc, a) = operand_bits(cond);
            let (ku, f) = unwind_bits(unwind);
            (w.a, w.b, w.c, w.d, w.e, w.f) = (a, target.0, assert_code(kind), msg.start, msg.end, f);
            w.flags = kc | (ku << 4) | (u16::from(expected) << 6);
        }
        StmtKind::Suspend { resume, cancel } => (w.tag, w.a, w.b) = (tag::SUSPEND, resume.0, cancel.0),
        StmtKind::Return => w.tag = tag::RETURN,
        StmtKind::Unreachable => w.tag = tag::UNREACHABLE,
        StmtKind::Resume => w.tag = tag::RESUME,
        StmtKind::Terminate => w.tag = tag::TERMINATE,
        StmtKind::InlineAsm { asm, operands, target, unwind } => {
            w.tag = tag::INLINE_ASM;
            (w.a, w.b, w.c, w.e) = (asm, operands.start, operands.end, opt(target.map(|b| b.0)));
            let (k, f) = unwind_bits(unwind);
            (w.f, w.flags) = (f, k << 4);
        }
    }
    w
}

/// The statement a row encodes; `None` for a row no statement encodes to.
pub fn decode_stmt(w: StmtWords) -> Option<StmtKind> {
    let assign = |value| StmtKind::Assign { dest: Place(w.a), value };
    let first = |w: StmtWords, id: u32| operand_of(w.flags & 3, id);
    let unwind = unwind_of((w.flags >> 4) & 3, w.f);
    let k = match w.tag {
        tag::USE => assign(Value::Use(first(w, w.b)?)),
        tag::BORROW => assign(Value::Borrow { kind: BorrowKind::from_code(w.d)?, place: Place(w.b), loan: Loan(w.c) }),
        tag::RAW_ADDR => assign(Value::RawAddr { mutbl: mutbl_of(w.d)?, place: Place(w.b) }),
        tag::BINARY | tag::CHECKED_BINARY => {
            let (op, lhs, rhs) = (BinOp::from_code(w.d)?, first(w, w.b)?, operand_of((w.flags >> 2) & 3, w.c)?);
            assign(match w.tag {
                tag::BINARY => Value::Binary { op, lhs, rhs },
                _ => Value::CheckedBinary { op, lhs, rhs },
            })
        }
        tag::UNARY => assign(Value::Unary { op: UnOp::from_code(w.d)?, operand: first(w, w.b)? }),
        tag::CAST => assign(Value::Cast { kind: CastKind::from_code(w.d)?, operand: first(w, w.b)?, ty: PTy(w.c) }),
        tag::AGGREGATE => assign(Value::Aggregate {
            kind: aggregate_of(w.e, w.f)?,
            ty: PTy(w.b),
            operands: Ops { start: w.c, end: w.d },
        }),
        tag::REPEAT => assign(Value::Repeat { operand: first(w, w.b)?, count: Const(w.c) }),
        tag::LEN => assign(Value::Len(Place(w.b))),
        tag::DISCRIMINANT => assign(Value::Discriminant(Place(w.b))),
        tag::SIZE_OF => assign(Value::SizeOf(PTy(w.b))),
        tag::ALIGN_OF => assign(Value::AlignOf(PTy(w.b))),
        tag::THREAD_LOCAL => assign(Value::ThreadLocalAddr(Const(w.b))),
        tag::STORAGE_LIVE => StmtKind::StorageLive(Local(w.a)),
        tag::STORAGE_DEAD => StmtKind::StorageDead(Local(w.a)),
        tag::ACTIVATE => StmtKind::Activate(Loan(w.a)),
        tag::FAKE_READ => StmtKind::FakeRead(Place(w.a)),
        tag::SET_DISCRIMINANT => StmtKind::SetDiscriminant { place: Place(w.a), variant: w.b },
        tag::DEINIT => StmtKind::Deinit(Place(w.a)),
        tag::GOTO => StmtKind::Goto(Block(w.a)),
        tag::SWITCH => StmtKind::Switch {
            on: first(w, w.a)?,
            cases: Ops { start: w.b, end: w.c },
            otherwise: Block(w.d),
        },
        tag::CALL => StmtKind::Call {
            callee: CalleeId(w.a),
            args: Ops { start: w.b, end: w.c },
            dest: unopt(w.d).map(Place),
            target: unopt(w.e).map(Block),
            unwind: unwind?,
        },
        tag::DROP => StmtKind::Drop { place: Place(w.a), target: Block(w.b), unwind: unwind? },
        tag::ASSERT => StmtKind::Assert {
            cond: first(w, w.a)?,
            expected: (w.flags >> 6) & 1 == 1,
            kind: assert_of(w.c)?,
            msg: Ops { start: w.d, end: w.e },
            target: Block(w.b),
            unwind: unwind?,
        },
        tag::SUSPEND => StmtKind::Suspend { resume: Block(w.a), cancel: Block(w.b) },
        tag::RETURN => StmtKind::Return,
        tag::UNREACHABLE => StmtKind::Unreachable,
        tag::RESUME => StmtKind::Resume,
        tag::TERMINATE => StmtKind::Terminate,
        tag::INLINE_ASM => StmtKind::InlineAsm {
            asm: w.a,
            operands: Ops { start: w.b, end: w.c },
            target: unopt(w.e).map(Block),
            unwind: unwind?,
        },
        _ => return None,
    };
    (encode_stmt(k) == w).then_some(k)
}

/// A local as its row.
pub fn encode_local(d: LocalDecl) -> LocalWords {
    let (kind, a, b) = match d.kind {
        LocalKind::Return => (0, NONE, NONE),
        LocalKind::Arg(i) => (1, i, NONE),
        LocalKind::Binding(p) => (2, p.0, NONE),
        LocalKind::Temp(e, role) => (3, e.0, role.code()),
        LocalKind::DropFlag(l) => (4, l.0, NONE),
    };
    LocalWords { ty: d.ty.0, kind, a, b, mutbl: mutbl_code(d.mutbl) }
}

/// The local a row encodes.
pub fn decode_local(w: LocalWords) -> Option<LocalDecl> {
    let kind = match w.kind {
        0 => LocalKind::Return,
        1 => LocalKind::Arg(w.a),
        2 => LocalKind::Binding(PatId(w.a)),
        3 => LocalKind::Temp(ExprId(w.a), Role::from_code(w.b)?),
        4 => LocalKind::DropFlag(Local(w.a)),
        _ => return None,
    };
    let d = LocalDecl { ty: PTy(w.ty), kind, mutbl: mutbl_of(w.mutbl)? };
    (encode_local(d) == w).then_some(d)
}

/// A place as its row.
pub fn encode_place(r: PlaceRow) -> PlaceWords {
    let (proj, a, b, from_end) = match r.proj {
        None => (0, NONE, NONE, false),
        Some(Proj::Field { index, ty }) => (1, index, ty.0, false),
        Some(Proj::Deref) => (2, NONE, NONE, false),
        Some(Proj::DerefBox) => (3, NONE, NONE, false),
        Some(Proj::Index(l)) => (4, l.0, NONE, false),
        Some(Proj::ConstIndex { offset, min_length, from_end }) => (5, offset, min_length, from_end),
        Some(Proj::Subslice { from, to, from_end }) => (6, from, to, from_end),
        Some(Proj::Downcast(v)) => (7, v, NONE, false),
    };
    PlaceWords {
        local: r.local.0,
        parent: opt(r.parent.map(|p| p.0)),
        a,
        b,
        depth: r.depth,
        proj,
        flags: u16::from(r.of_drop_type) | (u16::from(from_end) << 1),
    }
}

/// The place a row encodes: a local itself has neither parent nor projection, any other
/// place both.
pub fn decode_place(w: PlaceWords) -> Option<PlaceRow> {
    let from_end = w.flags & 2 != 0;
    let proj = match w.proj {
        0 => None,
        1 => Some(Proj::Field { index: w.a, ty: PTy(w.b) }),
        2 => Some(Proj::Deref),
        3 => Some(Proj::DerefBox),
        4 => Some(Proj::Index(Local(w.a))),
        5 => Some(Proj::ConstIndex { offset: w.a, min_length: w.b, from_end }),
        6 => Some(Proj::Subslice { from: w.a, to: w.b, from_end }),
        7 => Some(Proj::Downcast(w.a)),
        _ => return None,
    };
    let parent = unopt(w.parent).map(Place);
    if parent.is_some() != proj.is_some() {
        return None;
    }
    let r = PlaceRow { local: Local(w.local), parent, proj, depth: w.depth, of_drop_type: w.flags & 1 != 0 };
    (encode_place(r) == w).then_some(r)
}

/// A constant as its row.
pub fn encode_const(d: ConstDecl) -> ConstWords {
    let item = |key: ItemKey| split(key.0);
    let (bits, kind, a, b) = match d.kind {
        ConstKind::Scalar { bits } => (split(bits), 0, NONE, NONE),
        ConstKind::Bytes { start, len } => ([0; 4], 1, start, len),
        ConstKind::Item { key, args } => (item(key), 2, args.start, args.len),
        ConstKind::Promoted(i) => ([0; 4], 3, i, NONE),
        ConstKind::FnItem { key, args } => (item(key), 4, args.start, args.len),
        ConstKind::StaticAddr(key) => (item(key), 5, NONE, NONE),
        ConstKind::Anon(e) => ([0; 4], 6, e.0, NONE),
        ConstKind::Param(i) => ([0; 4], 7, i, NONE),
    };
    ConstWords { bits, ty: d.ty.0, kind, a, b }
}

/// The constant a row encodes.
pub fn decode_const(w: ConstWords) -> Option<ConstDecl> {
    let key = ItemKey(join(w.bits));
    let args = List { start: w.a, len: w.b };
    let kind = match w.kind {
        0 => ConstKind::Scalar { bits: join(w.bits) },
        1 => ConstKind::Bytes { start: w.a, len: w.b },
        2 => ConstKind::Item { key, args },
        3 => ConstKind::Promoted(w.a),
        4 => ConstKind::FnItem { key, args },
        5 => ConstKind::StaticAddr(key),
        6 => ConstKind::Anon(ExprId(w.a)),
        7 => ConstKind::Param(w.a),
        _ => return None,
    };
    let d = ConstDecl { kind, ty: PTy(w.ty) };
    (encode_const(d) == w).then_some(d)
}

/// A callee as its row.
pub fn encode_callee(c: Callee) -> CalleeWords {
    match c {
        Callee::FnItem { key, args } => CalleeWords { key: split(key.0), kind: 0, a: args.start, b: args.len, c: NONE },
        Callee::Method { key, args } => CalleeWords { key: split(key.0), kind: 1, a: args.start, b: args.len, c: NONE },
        Callee::Value(o) => {
            let (k, id) = operand_bits(o);
            CalleeWords { key: [0; 4], kind: 2, a: u32::from(k), b: id, c: NONE }
        }
        Callee::Intrinsic { key, args } => {
            CalleeWords { key: split(key.0), kind: 3, a: args.start, b: args.len, c: NONE }
        }
    }
}

/// The callee a row encodes.
pub fn decode_callee(w: CalleeWords) -> Option<Callee> {
    let key = ItemKey(join(w.key));
    let args = List { start: w.a, len: w.b };
    let c = match w.kind {
        0 => Callee::FnItem { key, args },
        1 => Callee::Method { key, args },
        2 => Callee::Value(operand_of(u16::try_from(w.a).ok()?, w.b)?),
        3 => Callee::Intrinsic { key, args },
        _ => return None,
    };
    (encode_callee(c) == w).then_some(c)
}

/// An operand as its row.
pub fn encode_operand(o: Operand) -> OperandWords {
    let (k, id) = operand_bits(o);
    OperandWords { kind: u32::from(k), id }
}

/// The operand a row encodes.
pub fn decode_operand(w: OperandWords) -> Option<Operand> {
    operand_of(u16::try_from(w.kind).ok()?, w.id)
}

/// A switch case as its row.
pub fn encode_case(value: u128, target: Block) -> CaseWords {
    CaseWords { value: split(value), target: target.0 }
}

/// The case a row encodes.
pub fn decode_case(w: CaseWords) -> Option<(u128, Block)> {
    Some((join(w.value), Block(w.target)))
}

/// A block as its row.
pub fn encode_block(r: BlockRow) -> BlockWords {
    BlockWords { start: r.stmts.start, end: r.stmts.end, kind: r.kind.code() }
}

/// The block a row encodes.
pub fn decode_block(w: BlockWords) -> Option<BlockRow> {
    Some(BlockRow { stmts: Ops { start: w.start, end: w.end }, kind: BlockKind::from_code(w.kind)? })
}

/// A loan as its row.
pub fn encode_loan(r: LoanRow) -> LoanWords {
    LoanWords { place: r.place.0, kind: r.kind.code(), origin: r.origin.0, at: r.at.0 }
}

/// The loan a row encodes.
pub fn decode_loan(w: LoanWords) -> Option<LoanRow> {
    Some(LoanRow { place: Place(w.place), kind: BorrowKind::from_code(w.kind)?, origin: Origin(w.origin), at: Stmt(w.at) })
}

/// `IntTy`'s number (its index) and text name: blaze-typeck's enum, so a table here rather
/// than `codes!`.
pub const INT_TYS: [(IntTy, &str); 6] = [
    (IntTy::Isize, "isize"),
    (IntTy::I8, "i8"),
    (IntTy::I16, "i16"),
    (IntTy::I32, "i32"),
    (IntTy::I64, "i64"),
    (IntTy::I128, "i128"),
];

/// `UintTy`'s number (its index) and text name.
pub const UINT_TYS: [(UintTy, &str); 6] = [
    (UintTy::Usize, "usize"),
    (UintTy::U8, "u8"),
    (UintTy::U16, "u16"),
    (UintTy::U32, "u32"),
    (UintTy::U64, "u64"),
    (UintTy::U128, "u128"),
];

/// `FloatTy`'s number (its index) and text name.
pub const FLOAT_TYS: [(FloatTy, &str); 4] = [(FloatTy::F16, "f16"), (FloatTy::F32, "f32"), (FloatTy::F64, "f64"), (FloatTy::F128, "f128")];

/// The index of `x` in a table of numbered forms; every form is in its table.
fn code_in<T: PartialEq + Copy>(table: &[(T, &str)], x: T) -> u32 {
    table.iter().position(|&(t, _)| t == x).unwrap_or(table.len()) as u32
}

fn form_of<T: Copy>(table: &[(T, &str)], code: u32) -> Option<T> {
    table.get(code as usize).map(|(t, _)| *t)
}

fn put_key(w: &mut [u32; 9], at: usize, k: ItemKey) {
    if let Some(dst) = w.get_mut(at..at + 4) {
        dst.copy_from_slice(&split(k.0));
    }
}

fn put_list(w: &mut [u32; 9], at: usize, l: List) {
    if let Some([a, b]) = w.get_mut(at..at + 2) {
        (*a, *b) = (l.start, l.len);
    }
}

fn put_name(w: &mut [u32; 9], at: usize, n: Name) {
    if let Some([a, b]) = w.get_mut(at..at + 2) {
        (*a, *b) = (n.0 as u32, (n.0 >> 32) as u32);
    }
}

fn key_at(w: &[u32; 9], at: usize) -> ItemKey {
    match w.get(at..at + 4) {
        Some(&[a, b, c, d]) => ItemKey(join([a, b, c, d])),
        _ => ItemKey(0),
    }
}

fn list_at(w: &[u32; 9], at: usize) -> List {
    match w.get(at..at + 2) {
        Some(&[start, len]) => List { start, len },
        _ => List { start: 0, len: 0 },
    }
}

fn name_at(w: &[u32; 9], at: usize) -> Name {
    match w.get(at..at + 2) {
        Some(&[lo, hi]) => Name(u64::from(lo) | (u64::from(hi) << 32)),
        _ => Name(0),
    }
}

/// An overlay type as its row.
pub fn encode_ty(n: PNode) -> TyWords {
    let mut w = [NONE; 9];
    let tag = match n {
        PNode::Bool => ty_tag::BOOL,
        PNode::Char => ty_tag::CHAR,
        PNode::Int(t) => {
            w[0] = code_in(&INT_TYS, t);
            ty_tag::INT
        }
        PNode::Uint(t) => {
            w[0] = code_in(&UINT_TYS, t);
            ty_tag::UINT
        }
        PNode::Float(t) => {
            w[0] = code_in(&FLOAT_TYS, t);
            ty_tag::FLOAT
        }
        PNode::Str => ty_tag::STR,
        PNode::Never => ty_tag::NEVER,
        PNode::Tuple(l) => {
            put_list(&mut w, 0, l);
            ty_tag::TUPLE
        }
        PNode::Record(s, l) => {
            put_name(&mut w, 0, Name(s.0));
            put_list(&mut w, 2, l);
            ty_tag::RECORD
        }
        PNode::Union(l) => {
            put_list(&mut w, 0, l);
            ty_tag::UNION
        }
        PNode::Ref(m, t) | PNode::Ptr(m, t) => {
            (w[0], w[1]) = (mutbl_code(m), t.0);
            match n {
                PNode::Ref(..) => ty_tag::REF,
                _ => ty_tag::PTR,
            }
        }
        PNode::Array(t, len) => {
            (w[0], w[1]) = (t.0, len.0);
            ty_tag::ARRAY
        }
        PNode::Slice(t) => {
            w[0] = t.0;
            ty_tag::SLICE
        }
        PNode::Pat(t, l) => {
            w[0] = t.0;
            put_list(&mut w, 1, l);
            ty_tag::PAT
        }
        PNode::Adt(k, l) | PNode::FnDef(k, l) | PNode::Opaque(k, l) | PNode::ExTrait(k, l) => {
            put_key(&mut w, 0, k);
            put_list(&mut w, 4, l);
            match n {
                PNode::Adt(..) => ty_tag::ADT,
                PNode::FnDef(..) => ty_tag::FN_DEF,
                PNode::Opaque(..) => ty_tag::OPAQUE,
                _ => ty_tag::EX_TRAIT,
            }
        }
        PNode::FnPtr(l, h) => {
            put_list(&mut w, 0, l);
            w[2] = u32::from(h.bits());
            ty_tag::FN_PTR
        }
        PNode::Closure { owner, ordinal, args } | PNode::Coroutine { owner, ordinal, args } => {
            put_key(&mut w, 0, owner);
            w[4] = ordinal;
            put_list(&mut w, 5, args);
            match n {
                PNode::Closure { .. } => ty_tag::CLOSURE,
                _ => ty_tag::COROUTINE,
            }
        }
        PNode::Future(e, a, b) => {
            (w[0], w[1], w[2]) = (e.0, a.0, b.0);
            ty_tag::FUTURE
        }
        PNode::Param(i) => {
            w[0] = i;
            ty_tag::PARAM
        }
        PNode::ConstParam(i) => {
            w[0] = i;
            ty_tag::CONST_PARAM
        }
        PNode::Alias { trait_, name, args } => {
            put_key(&mut w, 0, trait_);
            put_name(&mut w, 4, name);
            put_list(&mut w, 6, args);
            ty_tag::ALIAS
        }
        PNode::ExProj { trait_, name, args, term } => {
            put_key(&mut w, 0, trait_);
            put_name(&mut w, 4, name);
            put_list(&mut w, 6, args);
            w[8] = term.0;
            ty_tag::EX_PROJ
        }
        PNode::Infer => ty_tag::INFER,
        PNode::Error => ty_tag::ERROR,
        PNode::ConstValue { ty, bits } => {
            w[0] = ty.0;
            w[1..5].copy_from_slice(&split(bits));
            ty_tag::CONST_VALUE
        }
        PNode::Dynamic(l) => {
            put_list(&mut w, 0, l);
            ty_tag::DYNAMIC
        }
        PNode::ExAuto(k) => {
            put_key(&mut w, 0, k);
            ty_tag::EX_AUTO
        }
        PNode::Foreign(k) => {
            put_key(&mut w, 0, k);
            ty_tag::FOREIGN
        }
    };
    TyWords { w, tag }
}

/// The overlay type a row encodes; `None` for a row no type encodes to.
pub fn decode_ty(r: TyWords) -> Option<PNode> {
    let w = &r.w;
    let n = match r.tag {
        ty_tag::BOOL => PNode::Bool,
        ty_tag::CHAR => PNode::Char,
        ty_tag::INT => PNode::Int(form_of(&INT_TYS, w[0])?),
        ty_tag::UINT => PNode::Uint(form_of(&UINT_TYS, w[0])?),
        ty_tag::FLOAT => PNode::Float(form_of(&FLOAT_TYS, w[0])?),
        ty_tag::STR => PNode::Str,
        ty_tag::NEVER => PNode::Never,
        ty_tag::TUPLE => PNode::Tuple(list_at(w, 0)),
        ty_tag::RECORD => PNode::Record(RecordSchema(name_at(w, 0).0), list_at(w, 2)),
        ty_tag::UNION => PNode::Union(list_at(w, 0)),
        ty_tag::REF => PNode::Ref(mutbl_of(w[0])?, PTy(w[1])),
        ty_tag::PTR => PNode::Ptr(mutbl_of(w[0])?, PTy(w[1])),
        ty_tag::ARRAY => PNode::Array(PTy(w[0]), PTy(w[1])),
        ty_tag::SLICE => PNode::Slice(PTy(w[0])),
        ty_tag::PAT => PNode::Pat(PTy(w[0]), list_at(w, 1)),
        ty_tag::ADT => PNode::Adt(key_at(w, 0), list_at(w, 4)),
        ty_tag::FN_DEF => PNode::FnDef(key_at(w, 0), list_at(w, 4)),
        ty_tag::OPAQUE => PNode::Opaque(key_at(w, 0), list_at(w, 4)),
        ty_tag::EX_TRAIT => PNode::ExTrait(key_at(w, 0), list_at(w, 4)),
        ty_tag::FN_PTR => PNode::FnPtr(list_at(w, 0), FnPtrHeader::from_bits(u8::try_from(w[2]).ok()?)?),
        ty_tag::CLOSURE => PNode::Closure { owner: key_at(w, 0), ordinal: w[4], args: list_at(w, 5) },
        ty_tag::COROUTINE => PNode::Coroutine { owner: key_at(w, 0), ordinal: w[4], args: list_at(w, 5) },
        ty_tag::FUTURE => PNode::Future(ExprId(w[0]), PTy(w[1]), PTy(w[2])),
        ty_tag::PARAM => PNode::Param(w[0]),
        ty_tag::CONST_PARAM => PNode::ConstParam(w[0]),
        ty_tag::ALIAS => PNode::Alias { trait_: key_at(w, 0), name: name_at(w, 4), args: list_at(w, 6) },
        ty_tag::EX_PROJ => {
            PNode::ExProj { trait_: key_at(w, 0), name: name_at(w, 4), args: list_at(w, 6), term: PTy(w[8]) }
        }
        ty_tag::INFER => PNode::Infer,
        ty_tag::ERROR => PNode::Error,
        ty_tag::CONST_VALUE => PNode::ConstValue { ty: PTy(w[0]), bits: join([w[1], w[2], w[3], w[4]]) },
        ty_tag::DYNAMIC => PNode::Dynamic(list_at(w, 0)),
        ty_tag::EX_AUTO => PNode::ExAuto(key_at(w, 0)),
        ty_tag::FOREIGN => PNode::Foreign(key_at(w, 0)),
        _ => return None,
    };
    (encode_ty(n) == r).then_some(n)
}

/// Calls `ty` on each type and `list` on each list a node names directly (not a list's
/// entries): what verify and the builder check against the node's own handle.
pub fn ty_children(n: PNode, mut ty: impl FnMut(PTy), mut list: impl FnMut(List)) {
    match n {
        PNode::Ref(_, t) | PNode::Ptr(_, t) | PNode::Slice(t) | PNode::ConstValue { ty: t, .. } => ty(t),
        PNode::Array(a, b) | PNode::Future(_, a, b) => {
            ty(a);
            ty(b);
        }
        PNode::Pat(t, l) | PNode::ExProj { term: t, args: l, .. } => {
            ty(t);
            list(l);
        }
        PNode::Tuple(l)
        | PNode::Record(_, l)
        | PNode::Union(l)
        | PNode::Adt(_, l)
        | PNode::FnDef(_, l)
        | PNode::FnPtr(l, _)
        | PNode::Closure { args: l, .. }
        | PNode::Coroutine { args: l, .. }
        | PNode::Alias { args: l, .. }
        | PNode::Opaque(_, l)
        | PNode::Dynamic(l)
        | PNode::ExTrait(_, l) => list(l),
        PNode::Bool
        | PNode::Char
        | PNode::Int(_)
        | PNode::Uint(_)
        | PNode::Float(_)
        | PNode::Str
        | PNode::Never
        | PNode::Param(_)
        | PNode::ConstParam(_)
        | PNode::Infer
        | PNode::Error
        | PNode::ExAuto(_)
        | PNode::Foreign(_) => {}
    }
}

/// `n` with each list it names replaced by `f` of it: the builder's canonical form, where a
/// list is named by the first list of equal entries.
pub fn map_ty_lists(n: PNode, mut f: impl FnMut(List) -> List) -> PNode {
    match n {
        PNode::Tuple(l) => PNode::Tuple(f(l)),
        PNode::Record(s, l) => PNode::Record(s, f(l)),
        PNode::Union(l) => PNode::Union(f(l)),
        PNode::Pat(t, l) => PNode::Pat(t, f(l)),
        PNode::Adt(k, l) => PNode::Adt(k, f(l)),
        PNode::FnDef(k, l) => PNode::FnDef(k, f(l)),
        PNode::FnPtr(l, h) => PNode::FnPtr(f(l), h),
        PNode::Closure { owner, ordinal, args } => PNode::Closure { owner, ordinal, args: f(args) },
        PNode::Coroutine { owner, ordinal, args } => PNode::Coroutine { owner, ordinal, args: f(args) },
        PNode::Alias { trait_, name, args } => PNode::Alias { trait_, name, args: f(args) },
        PNode::Opaque(k, l) => PNode::Opaque(k, f(l)),
        PNode::Dynamic(l) => PNode::Dynamic(f(l)),
        PNode::ExTrait(k, l) => PNode::ExTrait(k, f(l)),
        PNode::ExProj { trait_, name, args, term } => PNode::ExProj { trait_, name, args: f(args), term },
        PNode::Bool
        | PNode::Char
        | PNode::Int(_)
        | PNode::Uint(_)
        | PNode::Float(_)
        | PNode::Str
        | PNode::Never
        | PNode::Ref(..)
        | PNode::Ptr(..)
        | PNode::Array(..)
        | PNode::Slice(_)
        | PNode::Future(..)
        | PNode::Param(_)
        | PNode::Infer
        | PNode::Error
        | PNode::ConstValue { .. }
        | PNode::ConstParam(_)
        | PNode::ExAuto(_)
        | PNode::Foreign(_) => n,
    }
}
