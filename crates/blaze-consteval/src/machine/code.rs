//! Code: a body compiled once, at one ground instance, into a flat list of operations over
//! numbered registers (consteval.md, "Comptime on the item layer", decision 1). Here, once:
//! every type is grounded, every adjustment becomes an operation, every literal is parsed, and
//! every offset, tag, constant's key and callee is resolved. A run then reads none of the body
//! form. A local whose address is never taken is a register; any other local is an
//! allocation, as a temporary is.
//!
//! What compiling meets that the language refuses, or a record not made yet, becomes an
//! operation that stops the run when it is reached (`Op::Late`), never the compile: a branch
//! not taken refuses nothing. A stop that names what a later step supplies (a need, a missing
//! definition, an unanswered projection) is PENDING and makes the code incomplete: the next
//! evaluation that asks for it compiles that body again (machine/late.rs).
//! The compiler is an explicit work stack, as the run is: no recursion on the input.

use core::alloc::Allocator;
use core::cell::Cell;

use blaze_hash::HashMap;
use blaze_ids::ContentId;
use blaze_own::{Alloc, Vec};
use blaze_typeck::{
    AdjustKind, AdtKind, BinOp, Def, Expr, ExprId, FloatTy, ItemKey, Lit, List, Mutbl, Name, PNode, PTy, Pat, PatId,
    Range, RecordSchema, Stmt, StmtId, UnOp, UnionPat,
};

use super::late::Late;
use super::{Machine, Mode, Need, NeedKind, R, Repr, Stop, Val, Work, int_info, malformed, mask, unsupported};
use crate::error::Refusal;
use crate::layout::{self, Layout};
use crate::lit;
use crate::record::{ConstFn, RecordKey};
use crate::soft::Format;
use crate::{ConstWorld, FnKind, FnTraitMethod, Intrinsic, Typed};

// `let` in a condition: `if let`, `while let`, let chains (machine/lets.rs).
#[path = "lets.rs"]
mod lets;

/// A register of a frame, by number.
pub(super) type Reg = u32;

/// How a scalar read at a type is checked ("Behavior considered undefined": invalid values).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Valid {
    /// A pointer, or anything else a pointer may be.
    Any,
    /// An integer or a float: no pointer.
    Num,
    Bool,
    Char,
}

/// What a type's value is, for the operations that read, write and make it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct TyInfo {
    pub(super) repr: Repr,
    pub(super) size: u64,
    pub(super) align: u8,
    pub(super) valid: Valid,
}

/// An operand type of arithmetic, packed into one byte so `Op::Binary` and `Op::StoreOp` fit in 16 bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Num {
    I8,
    I16,
    I32,
    I64,
    I128,
    U8,
    U16,
    U32,
    U64,
    U128,
    Bool,
    F16,
    F32,
    F64,
    F128,
    /// Not a number: refused once the values are seen to be no pointers.
    Other,
}

const _: () = assert!(core::mem::size_of::<Num>() == 1);

impl Num {
    #[inline(always)]
    pub(super) fn int(self) -> Option<(u32, bool)> {
        Some(match self {
            Num::I8 => (8, true),
            Num::I16 => (16, true),
            Num::I32 => (32, true),
            Num::I64 => (64, true),
            Num::I128 => (128, true),
            Num::U8 => (8, false),
            Num::U16 => (16, false),
            Num::U32 => (32, false),
            Num::U64 => (64, false),
            Num::U128 => (128, false),
            _ => return None,
        })
    }

    #[inline(always)]
    pub(super) fn float(self) -> Option<FloatTy> {
        Some(match self {
            Num::F16 => FloatTy::F16,
            Num::F32 => FloatTy::F32,
            Num::F64 => FloatTy::F64,
            Num::F128 => FloatTy::F128,
            _ => return None,
        })
    }
}

/// How a borrow treats a place's length and validity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Meta {
    /// `&x`: a safe reference, keeping the place's length as it is.
    Keep,
    /// An autoref to a sized pointee: no length.
    Drop,
    /// An autoref of an unsized place: it must have its length.
    Need,
    /// `&raw const x` / `&raw mut x`: a raw pointer, keeping the place's length without
    /// requiring provenance or a live allocation.
    Raw,
}

/// One operation, packed to 16 bytes for L1 cache density. Wide immediates and rarely-used
/// multi-word operands live in side tables on `Code`.
#[derive(Clone, Copy, Debug)]
pub(super) enum Op {
    /// Zero-sized immediate (`()`, fn item, empty tuple/array).
    ImmZst { dst: Reg },
    /// Scalar integer immediate up to 64 bits (covers `bool`, `char`, `usize`, `u8..u64`, `i8..i64`, `f16..f64`).
    ImmU64 { dst: Reg, v: u64 },
    /// Wide immediate in `Code::imms[k]` (128-bit integers/floats).
    Imm { dst: Reg, k: u32 },
    /// A string or byte-string literal (`Code::strs[k]`), cached once per evaluation.
    Str { dst: Reg, k: u32 },
    Move { dst: Reg, src: Reg },
    /// The place of a local held in memory, allocated when first used.
    Local { dst: Reg, slot: u32, ty: u32 },
    /// The place of a constant's value, by its record (`Code::consts`).
    Const { dst: Reg, k: u32 },
    /// The place of a static (`Code::statics[k]`).
    Static { dst: Reg, k: u32 },
    /// A place's value (a value stays as it is).
    Load { dst: Reg, src: Reg, ty: u32 },
    /// A value's place: a temporary holding it (a place stays as it is).
    Spill { dst: Reg, src: Reg, ty: u32 },
    /// The place a pointer value points to.
    Deref { dst: Reg, src: Reg },
    /// A pointer to a place.
    Borrow { dst: Reg, src: Reg, meta: Meta },
    /// A pointer to an array made a slice's pointer and length (`n` fits in `u32`).
    Unsize { dst: Reg, src: Reg, n: u32 },
    /// A pointer to an array made a slice's pointer and length (`Code::u64s[k]`).
    UnsizeWide { dst: Reg, src: Reg, k: u32 },
    /// A fn pointer to `Code::fns[k]`.
    FnPtr { dst: Reg, k: u32 },
    Unary { dst: Reg, src: Reg, op: UnOp, num: Num },
    Binary { dst: Reg, l: Reg, r: Reg, op: BinOp, num: Num, rnum: Num },
    /// Scalar/pointer cast (`Code::casts[k]` holds `(from, to)`).
    Cast { dst: Reg, src: Reg, k: u32 },
    /// The discriminant of the enum (`Code::discr_ops[k]`).
    Discr { dst: Reg, src: Reg, k: u32 },
    /// A field's place: the base place moved by `off`. `tail`: the field is the unsized tail
    /// of a dynamically sized struct, whose place keeps the base place's length.
    Field { dst: Reg, src: Reg, off: u32, tail: bool },
    /// A field's place with offset `Code::u64s[k]`.
    FieldWide { dst: Reg, src: Reg, k: u32, tail: bool },
    /// An element's place (`Code::indexes[k]`).
    Index { dst: Reg, k: u32 },
    /// A struct, variant, tuple or array value made of parts (`Code::builds[k]`).
    Build { dst: Reg, k: u32 },
    /// A struct expression with a base: the base's value with some fields set.
    Update { dst: Reg, base: Reg, k: u32 },
    /// `[v; n]`: `Code::repeats[k]`.
    Repeat { dst: Reg, src: Reg, k: u32 },
    /// `*place = value`.
    Store { place: Reg, src: Reg, ty: u32 },
    /// `place op= value` on a local in memory or any other place.
    StoreOp { place: Reg, src: Reg, ty: u32, op: BinOp, num: Num, rnum: Num },
    Jump { to: u32 },
    /// Jump when the `bool` in `cond` is `when`.
    Branch { cond: Reg, when: bool, to: u32 },
    /// A `const fn` call (`Code::calls[k]`).
    Call { dst: Reg, k: u32 },
    Intrinsic { dst: Reg, k: u32 },
    /// A panic with the call's arguments as its message (`Code::calls[k]`'s registers).
    Panic { k: u32 },
    Ret { src: Reg },
    /// The temporaries live at a statement's start, in `dst`; `Kill` ends those made since.
    Mark { dst: Reg },
    Kill { mark: Reg },
    /// Nothing (a `Mark` whose statement made no temporary).
    Nop,
    /// Jump unless the enum value at `place` (`Code::if_variants[k]` = `(e, variant)`) matches.
    IfVariant { place: Reg, k: u32, fail: u32 },
    /// Jump unless two values (`Code::if_sames[k]`) are equal.
    IfSame { k: u32, fail: u32 },
    /// Jump unless the slice at `place` matches length (`Code::if_lens[k]` = `(len, at_least)`).
    IfLen { place: Reg, k: u32, fail: u32 },
    /// The place of the element `back` from the end (`Code::slice_ops[k]`).
    FromEnd { dst: Reg, src: Reg, k: u32 },
    /// The subslice without its first `from` and last `back` elements (`Code::slice_ops[k]`).
    Subslice { dst: Reg, src: Reg, k: u32 },
    /// A binding by value into memory: `size` bytes from place `src` to place `dst`.
    Copy { dst: Reg, src: Reg, size: u32 },
    /// A binding by value into memory: `Code::u64s[k]` bytes from `src` to `dst`.
    CopyWide { dst: Reg, src: Reg, k: u32 },
    /// Stop: `Code::lates[k]`.
    Late { k: u32 },
}

const _: () = assert!(core::mem::size_of::<Op>() <= 16);

#[derive(Clone, Copy, Debug)]
pub(super) struct StrLit {
    pub(super) at: u32,
    pub(super) len: u32,
    pub(super) wide: bool,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct DiscrOp {
    pub(super) e: u32,
    pub(super) ty: u32,
    pub(super) bits: u32,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct IndexOp {
    pub(super) base: Reg,
    pub(super) index: Reg,
    pub(super) size: u64,
    pub(super) len: Option<u64>,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct IfSameOp {
    pub(super) a: Reg,
    pub(super) b: Reg,
    pub(super) ty: PTy,
    pub(super) konst: bool,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct SliceOp {
    pub(super) size: u64,
    pub(super) from: u64,
    pub(super) back: u64,
}

/// A made value's parts: each a register stored at an offset.
#[derive(Clone, Copy, Debug)]
pub(super) struct Build {
    pub(super) ty: u32,
    /// `Code::parts`.
    pub(super) parts: (u32, u32),
    pub(super) tag: Option<crate::TagWrite>,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Part {
    pub(super) src: Reg,
    pub(super) off: u64,
    pub(super) ty: u32,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Repeat {
    pub(super) ty: u32,
    pub(super) elem: u32,
    pub(super) n: u64,
}

/// A call: its callee and arguments, their registers and types, and the callee's code once
/// linked (`u32::MAX`: not yet).
#[derive(Clone, Debug)]
pub(super) struct Call {
    pub(super) item: ItemKey,
    pub(super) args: List,
    /// `Code::regs_of`: the argument registers, and each one's type.
    pub(super) regs: (u32, u32),
    pub(super) link: Cell<u32>,
    pub(super) intrinsic: Option<Intrinsic>,
    /// The call's type (an intrinsic's result).
    pub(super) ret: PTy,
}


/// One body compiled at one instance.
pub(super) struct Code<A: Allocator + Clone> {
    pub(super) item: ItemKey,
    /// The body text's hash: what every evaluation that runs it read.
    pub(super) hash: ContentId,
    /// The item's code identity: part of its record key.
    pub(super) code_id: ContentId,
    pub(super) ops: Vec<Op, A>,
    /// Each operation's site: the item-relative range of its expression (rule 71).
    pub(super) at: Vec<Range, A>,
    pub(super) regs: u32,
    /// Parameters: the first registers, each an argument.
    pub(super) params: u32,
    /// Locals held in memory.
    pub(super) slots: u32,
    /// The body's value's type.
    pub(super) ret: PTy,
    pub(super) tys: Vec<TyInfo, A>,
    pub(super) bytes: Vec<u8, A>,
    /// Side tables for compact 16-byte `Op`.
    pub(super) imms: Vec<Val, A>,
    pub(super) strs: Vec<StrLit, A>,
    pub(super) casts: Vec<(PTy, PTy), A>,
    pub(super) discr_ops: Vec<DiscrOp, A>,
    pub(super) indexes: Vec<IndexOp, A>,
    pub(super) if_variants: Vec<(u32, u32), A>,
    pub(super) if_sames: Vec<IfSameOp, A>,
    pub(super) if_lens: Vec<(u64, bool), A>,
    pub(super) slice_ops: Vec<SliceOp, A>,
    pub(super) u64s: Vec<u64, A>,
    pub(super) statics: Vec<(ItemKey, bool), A>,
    /// The constants it reads: item, arguments, record key.
    pub(super) consts: Vec<(ItemKey, List, RecordKey), A>,
    /// The fn pointers it makes: each one's key and the instance it was made from.
    pub(super) fns: Vec<ConstFn, A>,
    pub(super) calls: Vec<Call, A>,
    /// Argument registers of calls and their types.
    pub(super) regs_of: Vec<(Reg, PTy), A>,
    pub(super) builds: Vec<Build, A>,
    pub(super) parts: Vec<Part, A>,
    pub(super) repeats: Vec<Repeat, A>,
    pub(super) lates: Vec<Late, A>,
    /// The enums its operations read, each checked once when compiled (`Op::Discr`,
    /// `Op::IfVariant`).
    pub(super) enums: Vec<EnumFacts, A>,
    /// Their discriminants: `EnumFacts::at` indexes it.
    pub(super) discrs: Vec<i128, A>,
    /// The records compiling read (explicit discriminants): every run reads them too.
    pub(super) reads: Vec<ContentId, A>,
    /// No `Late::Pending` in it (machine/late.rs): kept for every later evaluation. Incomplete
    /// code is compiled again by the first later evaluation that asks for it.
    pub(super) complete: bool,
    /// The evaluation (`Codes::evaluation`) that compiled it: incomplete code is reused within
    /// it, since the world does not change while an evaluation runs.
    pub(super) compiled_in: u64,
    /// How many times this body at this instance was compiled (the tests' counter).
    pub(super) compiles: u32,
}

/// What a run needs to know of an enum, found once when its code is compiled (rule 74): where
/// its tag is and how the tag decodes (the layout's [`layout::Tag`]), and its discriminants,
/// each checked to fit the enum's integer, `n` of them from `at` in `Code::discrs`. Only
/// `Compiler::enum_facts` makes one, so an operation holding its index needs no layout lookup
/// and no check: `match`, `as` and `discriminant_value` all read through it. A type that is
/// not an enum has `n` 0 and the discriminant 0.
#[derive(Clone, Copy, Debug)]
pub(super) struct EnumFacts {
    ty: PTy,
    tag: layout::Tag,
    at: u32,
    n: u32,
}

impl EnumFacts {
    /// Where the tag is read (offset, size); `None`: nothing to read.
    pub(super) fn tag_at(&self) -> Option<(u64, u64)> {
        self.tag.at
    }

    /// The variant whose tag reads `bits`.
    pub(super) fn variant(&self, bits: u128, discrs: &[i128]) -> Result<u32, Refusal<'static>> {
        self.tag.variant(bits, self.discrs(discrs)?)
    }

    /// The variant when the tag slot holds a non-null pointer with provenance.
    pub(super) fn ptr_variant(&self) -> Option<u32> {
        self.tag.ptr_variant()
    }

    /// Variant `v`'s discriminant.
    pub(super) fn discriminant(&self, v: u32, discrs: &[i128]) -> Result<i128, Refusal<'static>> {
        if self.n == 0 {
            return Ok(0);
        }
        self.discrs(discrs)?.get(v as usize).copied().ok_or(Refusal::Malformed { what: "a variant past its enum" })
    }

    fn discrs<'d>(&self, discrs: &'d [i128]) -> Result<&'d [i128], Refusal<'static>> {
        discrs.get(self.at as usize..(self.at + self.n) as usize).ok_or(Refusal::Malformed { what: "an enum's discriminants past their code" })
    }
}

/// The compiled bodies of a unit, separated from `Work` so `Machine` can borrow `&mut Work`
/// and `&mut CodeTable` simultaneously with zero moves.
pub(super) struct CodeTable<A: Allocator + Clone> {
    pub(super) list: Vec<Code<A>, A>,
    pub(super) by: HashMap<(ItemKey, List), u32, A>,
    pub(super) alloc: A,
    /// The evaluations begun on this table.
    pub(super) evaluation: u64,
}

/// The code of a unit's bodies, by item and ground arguments: compiled once, run by every
/// evaluation in the unit. It lives as long as the unit's ground table and layouts, whose ids
/// it holds.
pub struct Codes<A: Allocator + Clone> {
    pub(super) table: CodeTable<A>,
    /// The machine's working vectors, kept for the next evaluation of the unit and borrowed
    /// in place (`&mut Work`) with zero struct moves.
    pub(super) work: Work<A>,
}

impl<A: Allocator + Clone> Codes<A> {
    pub fn new_in(alloc: A) -> Codes<A> {
        Codes {
            table: CodeTable {
                list: Vec::new_in(alloc.clone()),
                by: HashMap::new_in(alloc.clone()),
                alloc: alloc.clone(),
                evaluation: 0,
            },
            work: Work::new_in(alloc),
        }
    }

    /// Bodies compiled.
    pub fn len(&self) -> usize {
        self.table.list.len()
    }

    pub fn is_empty(&self) -> bool {
        self.table.list.is_empty()
    }
}

/// Where a binding lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Home {
    None,
    Reg(Reg),
    Slot(u32),
}

/// What a register holds while compiling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    V,
    P,
    /// A register local's own register (a value): read it through a copy.
    Local(PatId),
}

/// A `break` or `continue` target: its expression, its value's register, the jumps to its
/// end (a fixup list) and a loop's start.
#[derive(Clone, Copy, Debug)]
struct Target {
    e: ExprId,
    dst: Reg,
    ends: u32,
    start: Option<u32>,
}

/// A compile task: an expression to compile in a mode, or the step after its operands.
#[derive(Clone, Copy, Debug)]
enum C {
    Eval(ExprId, Mode),
    Adjust(ExprId, Mode),
    Unary(ExprId),
    Binary(ExprId),
    Cast(ExprId),
    Ref(ExprId),
    RawRef(ExprId),
    Deref(ExprId),
    Field(ExprId),
    Index(ExprId),
    Aggregate(ExprId),
    Repeat(ExprId),
    Struct(ExprId),
    Call(ExprId),
    Assign(ExprId),
    AssignOp(ExprId),
    IfCond(ExprId),
    IfThen { e: ExprId, dst: Reg, jump: u32 },
    IfElse { e: ExprId, dst: Reg, jump: u32 },
    Logic(ExprId),
    LogicEnd { e: ExprId, dst: Reg, jump: u32 },
    Block { e: ExprId, next: u32 },
    BlockEnd(ExprId),
    StmtEnd { e: ExprId, mark: Reg, at: u32 },
    Let { e: ExprId, s: StmtId },
    LetElse { e: ExprId, jump: u32 },
    LoopBody(ExprId),
    LoopEnd(ExprId),
    Break(ExprId),
    Return(ExprId),
    Match(ExprId),
    Arm { e: ExprId, arm: u32, place: Reg, ty: PTy, dst: Reg, ends: u32 },
    Guard { e: ExprId, arm: u32, place: Reg, ty: PTy, dst: Reg, ends: u32, fail: u32 },
    ArmBody { e: ExprId, arm: u32, place: Reg, ty: PTy, dst: Reg, ends: u32, fail: u32 },
    /// A link of an `if` condition compiled by links (machine/lets.rs): an `&&`, a `let` or a
    /// `bool`; every failure to fixup list `fail` (the `else` block).
    CondLink { e: ExprId, fail: u32 },
    /// A condition `let` after its scrutinee: its pattern's tests, failures to `fail`.
    CondLet { e: ExprId, fail: u32 },
    /// A condition `bool` link after its value: false to `fail`.
    CondBool { e: ExprId, fail: u32 },
    /// An `if` by links after its consequent: the `else` block at `fail`.
    IfLetEnd { e: ExprId, dst: Reg, fail: u32 },
}

impl C {
    fn expr(self) -> ExprId {
        match self {
            C::Eval(e, _)
            | C::Adjust(e, _)
            | C::Unary(e)
            | C::Binary(e)
            | C::Cast(e)
            | C::Ref(e)
            | C::RawRef(e)
            | C::Deref(e)
            | C::Field(e)
            | C::Index(e)
            | C::Aggregate(e)
            | C::Repeat(e)
            | C::Struct(e)
            | C::Call(e)
            | C::Assign(e)
            | C::AssignOp(e)
            | C::IfCond(e)
            | C::IfThen { e, .. }
            | C::IfElse { e, .. }
            | C::Logic(e)
            | C::LogicEnd { e, .. }
            | C::Block { e, .. }
            | C::BlockEnd(e)
            | C::StmtEnd { e, .. }
            | C::Let { e, .. }
            | C::LetElse { e, .. }
            | C::LoopBody(e)
            | C::LoopEnd(e)
            | C::Break(e)
            | C::Return(e)
            | C::Match(e)
            | C::Arm { e, .. }
            | C::Guard { e, .. }
            | C::ArmBody { e, .. }
            | C::CondLink { e, .. }
            | C::CondLet { e, .. }
            | C::CondBool { e, .. }
            | C::IfLetEnd { e, .. } => e,
        }
    }
}

/// A pattern compile item: a pattern at a place, or an or-pattern's next alternative.
#[derive(Clone, Copy, Debug)]
enum P {
    Pat { p: PatId, place: Reg, ty: PTy, fail: u32 },
    Alt { alts: List, i: u32, place: Reg, ty: PTy, fail: u32, join: u32 },
    AltEnd { alts: List, i: u32, place: Reg, ty: PTy, fail: u32, join: u32, next: u32 },
    Join { join: u32 },
}

/// One body's compile.
struct Cc<'w, 's> {
    typed: Typed<'w>,
    args: Vec<PTy, Alloc<'s>>,
    /// By body type: its ground type, once made (`u32::MAX`: not yet).
    tys: Vec<u32, Alloc<'s>>,
    /// By pattern.
    homes: Vec<Home, Alloc<'s>>,
    /// By pattern: a local whose address is taken, held in memory.
    addressed: &'s [bool],
    /// By register.
    kinds: Vec<Kind, Alloc<'s>>,
    tasks: Vec<C, Alloc<'s>>,
    rs: Vec<Reg, Alloc<'s>>,
    targets: Vec<Target, Alloc<'s>>,
    /// Jumps to patch: fixup list, operation.
    fix: Vec<(u32, u32), Alloc<'s>>,
    lists: u32,
    /// Operations that make a temporary, so far.
    temps: u32,
    at: Range,
    /// A register local whose address turned out to be taken: compile again with it in memory.
    restart: Option<PatId>,
}

fn rows<K: Ord + Copy>(col: &[K], key: K) -> core::ops::Range<usize> {
    let lo = col.partition_point(|&k| k < key);
    let hi = col.partition_point(|&k| k <= key);
    lo..hi
}

pub(super) fn num(n: PNode) -> Num {
    match n {
        PNode::Bool => Num::Bool,
        PNode::Float(FloatTy::F16) => Num::F16,
        PNode::Float(FloatTy::F32) => Num::F32,
        PNode::Float(FloatTy::F64) => Num::F64,
        PNode::Float(FloatTy::F128) => Num::F128,
        _ => match int_info(n) {
            Some((8, true)) => Num::I8,
            Some((16, true)) => Num::I16,
            Some((32, true)) => Num::I32,
            Some((64, true)) => Num::I64,
            Some((128, true)) => Num::I128,
            Some((8, false)) => Num::U8,
            Some((16, false)) => Num::U16,
            Some((32, false)) => Num::U32,
            Some((64, false)) => Num::U64,
            Some((128, false)) => Num::U128,
            _ => Num::Other,
        },
    }
}


impl Refusal<'_> {
    /// A refusal with no owned parts, as code keeps it; one with parts never arises while
    /// compiling.
    pub(super) fn detach<'b>(self) -> Refusal<'b> {
        match self {
            Refusal::Overflow { op } => Refusal::Overflow { op },
            Refusal::DivByZero => Refusal::DivByZero,
            Refusal::RemByZero => Refusal::RemByZero,
            Refusal::ShiftTooFar { amount, bits } => Refusal::ShiftTooFar { amount, bits },
            Refusal::IndexOutOfBounds { index, len } => Refusal::IndexOutOfBounds { index, len },
            Refusal::Uninit { offset } => Refusal::Uninit { offset },
            Refusal::OutOfBounds { offset, size, len } => Refusal::OutOfBounds { offset, size, len },
            Refusal::Dangling => Refusal::Dangling,
            Refusal::PointerAsInt => Refusal::PointerAsInt,
            Refusal::IntAsPointer => Refusal::IntAsPointer,
            Refusal::PartialPointer { offset } => Refusal::PartialPointer { offset },
            Refusal::PointerCompare => Refusal::PointerCompare,
            Refusal::WriteToImmutable => Refusal::WriteToImmutable,
            Refusal::MutableStatic { item } => Refusal::MutableStatic { item },
            Refusal::NotConstFn { item } => Refusal::NotConstFn { item },
            Refusal::UnknownIntrinsic { item } => Refusal::UnknownIntrinsic { item },
            Refusal::FnFact { item, fact } => Refusal::FnFact { item, fact },
            Refusal::InvalidBool { value } => Refusal::InvalidBool { value },
            Refusal::InvalidChar { value } => Refusal::InvalidChar { value },
            Refusal::InvalidTag { bits } => Refusal::InvalidTag { bits },
            Refusal::TransmuteSize { from, to } => Refusal::TransmuteSize { from, to },
            Refusal::Unreachable => Refusal::Unreachable,
            Refusal::NonzeroOnZero { name } => Refusal::NonzeroOnZero { name },
            Refusal::DiscriminantOverflow { variant } => Refusal::DiscriminantOverflow { variant },
            Refusal::StepLimit { limit } => Refusal::StepLimit { limit },
            Refusal::MemoryLimit { limit } => Refusal::MemoryLimit { limit },
            Refusal::CallDepth { limit } => Refusal::CallDepth { limit },
            Refusal::Unsupported { what } => Refusal::Unsupported { what },
            Refusal::Malformed { what } => Refusal::Malformed { what },
            Refusal::Literal(e) => Refusal::Literal(e),
            Refusal::Missing { what, item } => Refusal::Missing { what, item },
            Refusal::NeedsDrop { what } => Refusal::NeedsDrop { what },
            Refusal::Projection { trait_, name } => Refusal::Projection { trait_, name },
            Refusal::FieldGround { adt, field, name, projection } => Refusal::FieldGround { adt, field, name, projection },
            Refusal::NoSize { item } => Refusal::NoSize { item },
            Refusal::Panic { .. } | Refusal::Cycle { .. } => Refusal::Malformed { what: "a refusal with parts kept in code" },
        }
    }

    /// A kept refusal, for a run.
    pub(super) fn attach<'b>(&self) -> Refusal<'b> {
        match self {
            Refusal::Unsupported { what } => Refusal::Unsupported { what },
            Refusal::Malformed { what } => Refusal::Malformed { what },
            Refusal::Missing { what, item } => Refusal::Missing { what, item: *item },
            Refusal::NeedsDrop { what } => Refusal::NeedsDrop { what },
            Refusal::Projection { trait_, name } => Refusal::Projection { trait_: *trait_, name: *name },
            Refusal::FieldGround { adt, field, name, projection } => Refusal::FieldGround { adt: *adt, field: *field, name: *name, projection: *projection },
            Refusal::NoSize { item } => Refusal::NoSize { item: *item },
            Refusal::MutableStatic { item } => Refusal::MutableStatic { item: *item },
            Refusal::NotConstFn { item } => Refusal::NotConstFn { item: *item },
            Refusal::UnknownIntrinsic { item } => Refusal::UnknownIntrinsic { item: *item },
            Refusal::Literal(e) => Refusal::Literal(*e),
            Refusal::Unreachable => Refusal::Unreachable,
            Refusal::DiscriminantOverflow { variant } => Refusal::DiscriminantOverflow { variant: *variant },
            Refusal::InvalidTag { bits } => Refusal::InvalidTag { bits: *bits },
            Refusal::Overflow { op } => Refusal::Overflow { op: *op },
            _ => Refusal::Malformed { what: "a kept refusal of a kind compiling does not make" },
        }
    }
}

impl<'w, 'l, 'g, 't, 'wk, 'a, W: ConstWorld, L: Layout, K: Allocator + Clone> Machine<'w, 'l, 'g, 't, 'wk, 'a, W, L, K> {
    /// The code of `item` at ground arguments `args`: the unit's, or compiled now.
    pub(super) fn code<A: Allocator + Clone>(&mut self, codes: &mut CodeTable<A>, item: ItemKey, args: List) -> R<'a, u32> {
        let old = codes.by.get(&(item, args)).copied();
        if let Some(i) = old {
            let c = &codes.list[i as usize];
            if c.complete || c.compiled_in == codes.evaluation {
                return Ok(i);
            }
        }
        let world: &'w W = self.world;
        let typed = match world.body(item) {
            Err(_) => return Err(Stop::Need(Need { kind: NeedKind::Body, item, args })),
            Ok(None) => return Err(Stop::Refused(Refusal::Missing { what: "body", item })),
            Ok(Some(t)) => t,
        };
        let mut addressed: Vec<bool, Alloc<'a>> = Vec::new_in(self.alloc);
        addressed.resize(typed.body.pats.len(), false);
        let code = loop {
            let mut code = Code {
                item,
                hash: typed.hash,
                code_id: world.code_id(item),
                ops: Vec::new_in(codes.alloc.clone()),
                at: Vec::new_in(codes.alloc.clone()),
                regs: 0,
                params: typed.body.params.len() as u32,
                slots: 0,
                ret: PTy(0),
                tys: Vec::new_in(codes.alloc.clone()),
                bytes: Vec::new_in(codes.alloc.clone()),
                imms: Vec::new_in(codes.alloc.clone()),
                strs: Vec::new_in(codes.alloc.clone()),
                casts: Vec::new_in(codes.alloc.clone()),
                discr_ops: Vec::new_in(codes.alloc.clone()),
                indexes: Vec::new_in(codes.alloc.clone()),
                if_variants: Vec::new_in(codes.alloc.clone()),
                if_sames: Vec::new_in(codes.alloc.clone()),
                if_lens: Vec::new_in(codes.alloc.clone()),
                slice_ops: Vec::new_in(codes.alloc.clone()),
                u64s: Vec::new_in(codes.alloc.clone()),
                statics: Vec::new_in(codes.alloc.clone()),
                consts: Vec::new_in(codes.alloc.clone()),
                fns: Vec::new_in(codes.alloc.clone()),
                calls: Vec::new_in(codes.alloc.clone()),
                regs_of: Vec::new_in(codes.alloc.clone()),
                builds: Vec::new_in(codes.alloc.clone()),
                parts: Vec::new_in(codes.alloc.clone()),
                repeats: Vec::new_in(codes.alloc.clone()),
                lates: Vec::new_in(codes.alloc.clone()),
                enums: Vec::new_in(codes.alloc.clone()),
                discrs: Vec::new_in(codes.alloc.clone()),
                reads: Vec::new_in(codes.alloc.clone()),
                complete: true,
                compiled_in: codes.evaluation,
                compiles: 1,
            };
            let mut args_v = Vec::new_in(self.alloc);
            args_v.extend_from_slice(self.ground.view().list(args).map_err(|_| malformed("arguments past the ground table"))?);
            let mut cc = Cc {
                typed,
                args: args_v,
                tys: Vec::new_in(self.alloc),
                homes: Vec::new_in(self.alloc),
                addressed: &addressed,
                kinds: Vec::new_in(self.alloc),
                tasks: Vec::new_in(self.alloc),
                rs: Vec::new_in(self.alloc),
                targets: Vec::new_in(self.alloc),
                fix: Vec::new_in(self.alloc),
                lists: 0,
                temps: 0,
                at: Range::default(),
                restart: None,
            };
            cc.homes.resize(typed.body.pats.len(), Home::None);
            self.body(&mut cc, &mut code)?;
            let restart = cc.restart;
            drop(cc);
            match restart {
                Some(p) => {
                    let slot = addressed.get_mut(p.ix()).ok_or(malformed("a pattern past the body"))?;
                    if *slot {
                        return Err(malformed("a local taken by address twice"));
                    }
                    *slot = true;
                }
                None => break code,
            }
        };
        Ok(match old {
            Some(i) => {
                // Compiled again in place: callers' links to `i` stay valid, and no frame runs
                // the old code (it was compiled in an earlier evaluation, and this is the first
                // ask for it in this one).
                let mut code = code;
                code.compiles = codes.list[i as usize].compiles.saturating_add(1);
                self.charge_recompile(item, code.ops.len())?;
                codes.list[i as usize] = code;
                i
            }
            None => {
                let i = codes.list.len() as u32;
                codes.list.push(code);
                codes.by.insert((item, args), i);
                i
            }
        })
    }

    /// Compiles the whole body: the parameters' bindings, then its expression, then `Ret`.
    fn body<A: Allocator + Clone>(&mut self, cc: &mut Cc<'w, '_>, code: &mut Code<A>) -> R<'a, ()> {
        let typed = cc.typed;
        let body = typed.body;
        // The arguments arrive in registers 0..n.
        for _ in body.params.iter() {
            self.reg(cc, Kind::V);
        }
        for (i, &pat) in body.params.iter().enumerate() {
            let i = i as u32;
            let t = match self.cg(cc, *typed.pat_ty.get(pat.ix()).ok_or(malformed("a pattern with no type"))?) {
                Ok(t) => t,
                Err(s) => {
                    self.late(code, cc, s);
                    continue;
                }
            };
            if let Pat::Bind { ann: blaze_typeck::BindAnn { by_ref: None, .. }, sub: None, .. } = body.pats[pat.ix()]
                && self.union_pat_c(cc, pat).is_none()
            {
                if self.reg_home(cc, pat, t)? {
                    cc.homes[pat.ix()] = Home::Reg(i);
                    cc.kinds[i as usize] = Kind::Local(pat);
                    continue;
                }
            }
            let r = (|| -> R<'a, ()> {
                let ty = self.ty(code, t)?;
                let place = self.reg(cc, Kind::P);
                self.emit(code, cc, Op::Spill { dst: place, src: i, ty });
                let fail = self.list(cc);
                self.pattern(cc, code, pat, place, t, fail)?;
                let ok = self.jump(code, cc);
                self.patch(code, cc, fail);
                let k = self.keep_late(code, Late::Final(Refusal::Malformed { what: "a refutable parameter pattern" }));
                self.emit(code, cc, Op::Late { k });
                self.patch_one(code, ok);
                Ok(())
            })();
            if let Err(s) = r {
                if cc.restart.is_some() {
                    return Ok(());
                }
                self.late(code, cc, s);
            }
        }
        code.ret = match self.final_ty_c(cc, body.root) {
            Ok(t) => t,
            Err(s) => {
                self.late(code, cc, s);
                PTy(0)
            }
        };
        cc.tasks.push(C::Eval(body.root, Mode::Value));
        while let Some(task) = cc.tasks.pop() {
            if let Some(r) = typed.body.expr_ranges.get(task.expr().ix()) {
                cc.at = *r;
            }
            let (rs, ts, tg) = (cc.rs.len(), cc.tasks.len(), cc.targets.len());
            if let Err(s) = self.c(cc, code, task) {
                if cc.restart.is_some() {
                    return Ok(());
                }
                let (consumes, produces) = shape(task, &typed)?;
                cc.rs.truncate(rs.saturating_sub(consumes as usize));
                cc.tasks.truncate(ts);
                cc.targets.truncate(tg);
                self.late(code, cc, s);
                if produces {
                    let d = self.reg(cc, Kind::V);
                    cc.rs.push(d);
                }
            }
        }
        let v = cc.rs.pop().ok_or(malformed("the body left no value"))?;
        self.emit(code, cc, Op::Ret { src: v });
        code.regs = cc.kinds.len() as u32;
        Ok(())
    }

    // ---- Compile helpers ----

    fn reg(&self, cc: &mut Cc<'w, '_>, k: Kind) -> Reg {
        cc.kinds.push(k);
        (cc.kinds.len() - 1) as Reg
    }

    fn emit<A: Allocator + Clone>(&self, code: &mut Code<A>, cc: &mut Cc<'w, '_>, op: Op) -> u32 {
        if matches!(op, Op::Spill { .. } | Op::Discr { .. } | Op::Load { .. } | Op::Build { .. } | Op::Update { .. } | Op::Repeat { .. } | Op::Call { .. } | Op::Intrinsic { .. } | Op::Borrow { .. }) {
            cc.temps += 1;
        }
        code.ops.push(op);
        code.at.push(cc.at);
        (code.ops.len() - 1) as u32
    }

    fn emit_imm<A: Allocator + Clone>(&self, code: &mut Code<A>, cc: &mut Cc<'w, '_>, dst: Reg, v: Val) -> u32 {
        let op = match v {
            Val::Zst => Op::ImmZst { dst },
            Val::Int(n) => Op::ImmU64 { dst, v: n },
            other => {
                let k = code.imms.len() as u32;
                code.imms.push(other);
                Op::Imm { dst, k }
            }
        };
        self.emit(code, cc, op)
    }

    fn emit_field<A: Allocator + Clone>(&self, code: &mut Code<A>, cc: &mut Cc<'w, '_>, dst: Reg, src: Reg, off: u64, tail: bool) -> u32 {
        let op = match u32::try_from(off) {
            Ok(off) => Op::Field { dst, src, off, tail },
            Err(_) => {
                let k = code.u64s.len() as u32;
                code.u64s.push(off);
                Op::FieldWide { dst, src, k, tail }
            }
        };
        self.emit(code, cc, op)
    }

    fn emit_copy<A: Allocator + Clone>(&self, code: &mut Code<A>, cc: &mut Cc<'w, '_>, dst: Reg, src: Reg, size: u64) -> u32 {
        let op = match u32::try_from(size) {
            Ok(size) => Op::Copy { dst, src, size },
            Err(_) => {
                let k = code.u64s.len() as u32;
                code.u64s.push(size);
                Op::CopyWide { dst, src, k }
            }
        };
        self.emit(code, cc, op)
    }

    fn emit_unsize<A: Allocator + Clone>(&self, code: &mut Code<A>, cc: &mut Cc<'w, '_>, dst: Reg, src: Reg, n: u64) -> u32 {
        let op = match u32::try_from(n) {
            Ok(n) => Op::Unsize { dst, src, n },
            Err(_) => {
                let k = code.u64s.len() as u32;
                code.u64s.push(n);
                Op::UnsizeWide { dst, src, k }
            }
        };
        self.emit(code, cc, op)
    }

    /// Fn item type `ty`'s value as a fn pointer: the one reification, for the checker's
    /// `ReifyFnPointer` adjustment and for an `as` cast of a fn item (`id::<T> as fn(T) -> T`)
    /// alike. The record names the instance ([`ConstFn`]).
    fn reify<A: Allocator + Clone>(&mut self, code: &mut Code<A>, cc: &mut Cc<'w, '_>, ty: PTy) -> R<'a, Reg> {
        let PNode::FnDef(item, args) = self.node(ty)? else { return Err(malformed("a fn pointer made of a value that is not a fn item")) };
        let (item, args) = self.resolve(item, args)?;
        let key = self.instance(item, args)?;
        code.fns.push(ConstFn { key, instance: blaze_build::Instance { item, args } });
        let dst = self.reg(cc, Kind::V);
        self.emit(code, cc, Op::FnPtr { dst, k: code.fns.len() as u32 - 1 });
        Ok(dst)
    }

    fn late<A: Allocator + Clone>(&self, code: &mut Code<A>, cc: &mut Cc<'w, '_>, s: Stop<'a>) {
        let k = self.keep_late(code, Late::of(s));
        self.emit(code, cc, Op::Late { k });
    }

    /// Keeps a stop in the code; a pending one makes the code incomplete (machine/late.rs).
    fn keep_late<A: Allocator + Clone>(&self, code: &mut Code<A>, l: Late) -> u32 {
        if l.makes_incomplete(self.plant) {
            code.complete = false;
        }
        code.lates.push(l);
        (code.lates.len() - 1) as u32
    }

    fn list(&self, cc: &mut Cc<'w, '_>) -> u32 {
        cc.lists += 1;
        cc.lists
    }

    /// A jump to be patched later: its operation's index.
    fn jump<A: Allocator + Clone>(&self, code: &mut Code<A>, cc: &mut Cc<'w, '_>) -> u32 {
        self.emit(code, cc, Op::Jump { to: u32::MAX })
    }

    /// Adds jump `op` to fixup list `list`.
    fn to_list(&self, cc: &mut Cc<'w, '_>, list: u32, op: u32) {
        cc.fix.push((list, op));
    }

    /// Points every jump of `list` here.
    fn patch<A: Allocator + Clone>(&self, code: &mut Code<A>, cc: &mut Cc<'w, '_>, list: u32) {
        let here = code.ops.len() as u32;
        let mut i = 0;
        while i < cc.fix.len() {
            if cc.fix[i].0 == list {
                let op = cc.fix[i].1;
                set_target(&mut code.ops[op as usize], here);
                cc.fix.remove_swap(i);
            } else {
                i += 1;
            }
        }
    }

    fn patch_one<A: Allocator + Clone>(&self, code: &mut Code<A>, op: u32) {
        let here = code.ops.len() as u32;
        set_target(&mut code.ops[op as usize], here);
    }

    /// `t` of the body's type table, at the instance's arguments, in the ground table.
    fn cg(&mut self, cc: &mut Cc<'w, '_>, t: PTy) -> R<'a, PTy> {
        if let Some(&c) = cc.tys.get(t.0 as usize) {
            if c != u32::MAX {
                return Ok(PTy(c));
            }
        }
        // A projection no impl answered is pending (machine/late.rs): the front end makes the
        // impls and asks again. Any other grounding refusal is final.
        let out = blaze_build::ground(cc.typed.types, t, &cc.args, self.ground, self.alloc, self.world).map_err(|e| match e {
            blaze_build::BuildError::Projection { trait_, name } => Stop::Refused(Refusal::Projection { trait_, name }),
            _ => unsupported("a type with no ground form (an alias, an opaque type, a closure)"),
        })?;
        if cc.tys.len() <= t.0 as usize {
            cc.tys.resize(t.0 as usize + 1, u32::MAX);
        }
        cc.tys[t.0 as usize] = out.0;
        Ok(out)
    }

    fn ety_c(&mut self, cc: &mut Cc<'w, '_>, e: ExprId) -> R<'a, PTy> {
        let t = *cc.typed.expr_ty.get(e.ix()).ok_or(malformed("an expression with no type"))?;
        self.cg(cc, t)
    }

    fn final_ty_c(&mut self, cc: &mut Cc<'w, '_>, e: ExprId) -> R<'a, PTy> {
        let typed = cc.typed;
        let r = rows(typed.adjusts.0, e);
        match r.end.checked_sub(1).filter(|&l| l >= r.start) {
            Some(last) => self.cg(cc, typed.adjusts.2[last]),
            None => self.ety_c(cc, e),
        }
    }

    fn expr_c(&self, cc: &Cc<'w, '_>, e: ExprId) -> R<'a, Expr> {
        cc.typed.body.exprs.get(e.ix()).copied().ok_or(malformed("an expression past the body"))
    }

    fn expr_def_c(&self, cc: &Cc<'w, '_>, e: ExprId) -> Option<Def> {
        let t = &cc.typed;
        rows(t.expr_defs.0, e).next().map(|i| t.expr_defs.1[i])
    }

    /// The ground arguments a value path gives the constant it names, when the path itself
    /// says them: `<Q as Trait<A..>>::NAME` with ONE segment after the anchor. The Reference,
    /// "Paths", "Qualified paths": "Fully qualified paths allow for disambiguating the path
    /// for trait implementations and for specifying canonical paths": the constant is
    /// `Trait`'s own `NAME` (the checker records `Def::Const` of the TRAIT's item:
    /// blaze-typeck rules/lit_path.rs `anchored_value`) at `Self = Q` followed by the
    /// arguments written on the trait's path. So the list is `[Q, A..]`, each grounded at
    /// this instance's arguments, and the caller asks [`ConstWorld::resolve`] for the impl's
    /// item, as for a call (the pinned core, library/core/src/mem/mod.rs: `size_of::<T>()`
    /// is `<T as SizedTypeProperties>::SIZE`, which has a value only at a ground `T`).
    ///
    /// `List::EMPTY` for every other path (`NAME`, `Ty::NAME`, `Self::NAME`, `<Q>::NAME`,
    /// `<Q as Trait>::Assoc::NAME`): the checker selected by type there and a checked body
    /// records no arguments for a constant path (owed in blaze-typeck: `Checked` holds
    /// `Def::Const(key)` alone), so such a path names its constant at no arguments, as before.
    /// A trait parameter after `Self` that the path does not write (a default, or one the
    /// checker inferred) is not in the list: the world's `resolve` is asked with what is
    /// written and refuses a list it cannot select by, never a guessed impl.
    fn qualified_const_args(&mut self, cc: &Cc<'w, '_>, p: blaze_typeck::PathIdx) -> R<'a, List> {
        let body = cc.typed.body;
        let bp = body.paths.get(p.ix()).copied().ok_or(malformed("a path past the body"))?;
        let Some(anchor) = bp.anchor else { return Ok(List::EMPTY) };
        let Some(trait_path) = anchor.trait_ else { return Ok(List::EMPTY) };
        if bp.segments.len != 1 {
            return Ok(List::EMPTY);
        }
        let tp = body.paths.get(trait_path.ix()).copied().ok_or(malformed("a trait's path past the body"))?;
        let segments = body.segments.get(tp.segments.range()).ok_or(malformed("a trait path's segments past the body"))?;
        let written: &[PTy] = match segments.last() {
            Some(s) => body.type_lists.get(s.generic_args.range()).ok_or(malformed("a trait path's arguments past the body"))?,
            None => &[],
        };
        let no_ground = "a qualified constant path `<T as Trait>::NAME` whose type has no ground form (a hole `_`, an alias, an opaque type)";
        let mut all: Vec<PTy, Alloc<'a>> = Vec::with_capacity_in(1 + written.len(), self.alloc);
        all.push(blaze_build::ground(body.types, anchor.ty, &cc.args, self.ground, self.alloc, self.world).map_err(|_| unsupported(no_ground))?);
        for &t in written {
            all.push(blaze_build::ground(body.types, t, &cc.args, self.ground, self.alloc, self.world).map_err(|_| unsupported(no_ground))?);
        }
        self.ground.intern_list(&all).map_err(|_| malformed("the ground table is full"))
    }

    /// The arguments the check recorded for constant path `e` (blaze-typeck
    /// `Checked::const_args`: `Self::NAME` in a trait's default is `[Self]`), each grounded at
    /// this instance's arguments; `List::EMPTY` for a path with no row (a constant with no
    /// generics). The Reference, "Associated Items", "Associated constants": "definitions that
    /// include generic parameters are evaluated after monomorphization", so `Self::SIZE` in
    /// `SizedTypeProperties::IS_ZST` at `Self = u8` is `u8`'s.
    fn recorded_const_args(&mut self, cc: &Cc<'w, '_>, e: ExprId) -> R<'a, List> {
        let t = &cc.typed;
        let Some(i) = rows(t.const_args.0, e).next() else { return Ok(List::EMPTY) };
        let recorded = t.types.list(t.const_args.1[i]).map_err(|_| malformed("a constant path's arguments past the record's types"))?;
        let no_ground = "a constant path's recorded arguments with no ground form (a hole `_`, an alias, an opaque type)";
        let mut all: Vec<PTy, Alloc<'a>> = Vec::with_capacity_in(recorded.len(), self.alloc);
        for &ty in recorded {
            all.push(blaze_build::ground(t.types, ty, &cc.args, self.ground, self.alloc, self.world).map_err(|_| unsupported(no_ground))?);
        }
        self.ground.intern_list(&all).map_err(|_| malformed("the ground table is full"))
    }

    /// The value of const generic parameter `i` at this instance: its ground argument, a
    /// `ConstValue` whose bits are the scalar (blaze-typeck portable.rs: little-endian as
    /// this evaluator writes them). An argument that is not a value is malformed: the
    /// evaluation runs at ground arguments, and the checker typed the path as a value only
    /// for a const parameter.
    fn const_param_value(&mut self, cc: &Cc<'w, '_>, i: u32) -> R<'a, Val> {
        let arg = *cc.args.get(i as usize).ok_or(malformed("a const parameter past the instance's arguments"))?;
        let PNode::ConstValue { ty, bits } = self.node(arg)? else {
            return Err(malformed("a const parameter whose argument at the instance is not a value"));
        };
        let node = self.node(ty)?;
        Ok(match node {
            PNode::Bool | PNode::Char => Val::int(bits),
            PNode::Int(_) | PNode::Uint(_) => {
                let (width, _) = int_info(node).ok_or(malformed("a const parameter's integer type with no size"))?;
                Val::int(mask(bits, width))
            }
            // Under `adt_const_params` the bits are a fieldless enum's variant index, not its
            // stored tag: building that value is not written yet.
            PNode::Adt(..) => return Err(unsupported("a const parameter of an enum type (`adt_const_params`) as a value in a constant")),
            _ => return Err(malformed("a const parameter's value of a type that is not bool, char, an integer or an enum")),
        })
    }

    fn pat_def_c(&self, cc: &Cc<'w, '_>, p: PatId) -> Option<Def> {
        let t = &cc.typed;
        rows(t.pat_defs.0, p).next().map(|i| t.pat_defs.1[i])
    }

    fn union_pat_c(&self, cc: &Cc<'w, '_>, p: PatId) -> Option<UnionPat> {
        let t = &cc.typed;
        let i = t.union_pats.0.binary_search(&p).ok()?;
        t.union_pats.1.get(i).copied()
    }

    fn record_names_c(&self, cc: &Cc<'w, '_>, schema: RecordSchema) -> R<'a, Vec<Name, Alloc<'a>>> {
        let names = self
            .ground
            .view()
            .record_names(schema)
            .or_else(|| cc.typed.types.record_names(schema))
            .ok_or(malformed("a structural record schema missing its field names"))?;
        let mut out: Vec<Name, Alloc<'a>> = Vec::with_capacity_in(names.len(), self.alloc);
        out.extend_from_slice(names);
        Ok(out)
    }

    fn record_field_index_c(&self, cc: &Cc<'w, '_>, schema: RecordSchema, name: Name) -> R<'a, u32> {
        self.ground
            .view()
            .record_field_index(schema, name)
            .or_else(|| cc.typed.types.record_field_index(schema, name))
            .ok_or(malformed("a structural record field name its schema lacks"))
    }

    /// Binds pattern `p` (of binding type `lt`, by reference when `by_ref`) from place `pl`.
    fn bind_at<A: Allocator + Clone>(&mut self, cc: &mut Cc<'w, '_>, code: &mut Code<A>, p: PatId, pl: Reg, lt: PTy, by_ref: bool) -> R<'a, ()> {
        match self.home(cc, &mut code.slots, p)? {
            Home::Reg(r) => {
                if by_ref {
                    self.emit(code, cc, Op::Borrow { dst: r, src: pl, meta: Meta::Keep });
                } else {
                    let ty = self.ty(code, lt)?;
                    self.emit(code, cc, Op::Load { dst: r, src: pl, ty });
                }
            }
            Home::Slot(_) => {
                let local = self.local_place(cc, code, p)?;
                if by_ref {
                    let b = self.reg(cc, Kind::V);
                    self.emit(code, cc, Op::Borrow { dst: b, src: pl, meta: Meta::Keep });
                    let ty = self.ty(code, lt)?;
                    self.emit(code, cc, Op::Store { place: local, src: b, ty });
                } else {
                    let (size, _) = self.size_align(lt)?;
                    self.emit_copy(code, cc, local, pl, size);
                }
            }
            Home::None => return Err(malformed("a binding with no home")),
        }
        Ok(())
    }

    /// Remaps a structural union at place `src_pl` (of ground type `src_ty`) into a value of
    /// ground union type `dst_ty`, copying whichever member appears in both. When `on_miss` is
    /// `Some(fail)`, a source variant absent from `dst_ty` jumps to fixup list `fail`
    /// (`UnionPat::Residual`); when `None`, it stops with `Refusal::Unreachable`
    /// (`AdjustKind::UnionWiden`).
    fn remap_union<A: Allocator + Clone>(
        &mut self,
        cc: &mut Cc<'w, '_>,
        code: &mut Code<A>,
        src_pl: Reg,
        src_ty: PTy,
        dst_ty: PTy,
        on_miss: Option<u32>,
    ) -> R<'a, Reg> {
        if src_ty == dst_ty {
            return self.as_value(cc, code, src_pl, src_ty);
        }
        let PNode::Union(src_l) = self.node(src_ty)? else {
            return Err(malformed("a union remap from a type that is not a structural union"));
        };
        let PNode::Union(dst_l) = self.node(dst_ty)? else {
            return Err(malformed("a union remap to a type that is not a structural union"));
        };
        let mut src_members: Vec<PTy, Alloc<'a>> = Vec::new_in(self.alloc);
        src_members.extend_from_slice(self.ground.view().list(src_l).map_err(|_| malformed("a source union's members past the ground table"))?);
        let mut dst_members: Vec<PTy, Alloc<'a>> = Vec::new_in(self.alloc);
        dst_members.extend_from_slice(self.ground.view().list(dst_l).map_err(|_| malformed("a target union's members past the ground table"))?);
        if let [only] = src_members.as_slice() {
            let Some(j_idx) = dst_members.iter().position(|dm| dm == only) else {
                let dst = self.reg(cc, Kind::V);
                match on_miss {
                    Some(fail) => {
                        let jmp = self.jump(code, cc);
                        self.to_list(cc, fail, jmp);
                    }
                    None => {
                        let k = self.keep_late(code, Late::Final(Refusal::Unreachable));
                        self.emit(code, cc, Op::Late { k });
                    }
                }
                return Ok(dst);
            };
            let j = u32::try_from(j_idx).map_err(|_| malformed("a union variant index past `u32`"))?;
            let soff = self.field_offset(src_ty, 0, 0)?;
            let fpl = self.reg(cc, Kind::P);
            self.emit_field(code, cc, fpl, src_pl, soff, false);
            return self.build(cc, code, dst_ty, j, &[(0, fpl, *only)], false);
        }
        let src_e = self.enum_facts(code, src_ty)?;
        let facts = *code
            .enums
            .get(usize::try_from(src_e).map_err(|_| malformed("an enum index past `usize`"))?)
            .ok_or(malformed("an enum past its code"))?;
        if facts.tag_at().is_none() {
            let index = facts.variant(0, &code.discrs)?;
            let idx_usize = usize::try_from(index).map_err(|_| malformed("a union variant index past `usize`"))?;
            let found = src_members
                .get(idx_usize)
                .copied()
                .and_then(|sm| dst_members.iter().position(|&dm| dm == sm).map(|j| (sm, j)));
            let Some((sm, j_idx)) = found else {
                let dst = self.reg(cc, Kind::V);
                match on_miss {
                    Some(fail) => {
                        let jmp = self.jump(code, cc);
                        self.to_list(cc, fail, jmp);
                    }
                    None => {
                        let k = self.keep_late(code, Late::Final(Refusal::Unreachable));
                        self.emit(code, cc, Op::Late { k });
                    }
                }
                return Ok(dst);
            };
            let j = u32::try_from(j_idx).map_err(|_| malformed("a union variant index past `u32`"))?;
            let soff = self.field_offset(src_ty, index, 0)?;
            let fpl = self.reg(cc, Kind::P);
            self.emit_field(code, cc, fpl, src_pl, soff, false);
            return self.build(cc, code, dst_ty, j, &[(0, fpl, sm)], false);
        }
        let dst = self.reg(cc, Kind::V);
        let done = self.list(cc);
        for (i_idx, &sm) in src_members.iter().enumerate() {
            let Some(j_idx) = dst_members.iter().position(|&dm| dm == sm) else { continue };
            let i = u32::try_from(i_idx).map_err(|_| malformed("a union variant index past `u32`"))?;
            let j = u32::try_from(j_idx).map_err(|_| malformed("a union variant index past `u32`"))?;
            let next_arm = self.list(cc);
            self.if_variant(cc, code, src_pl, src_ty, i, next_arm)?;
            let soff = self.field_offset(src_ty, i, 0)?;
            let fpl = self.reg(cc, Kind::P);
            self.emit_field(code, cc, fpl, src_pl, soff, false);
            let built = self.build(cc, code, dst_ty, j, &[(0, fpl, sm)], false)?;
            self.emit(code, cc, Op::Move { dst, src: built });
            let jmp = self.jump(code, cc);
            self.to_list(cc, done, jmp);
            self.patch(code, cc, next_arm);
        }
        match on_miss {
            Some(fail) => {
                let jmp = self.jump(code, cc);
                self.to_list(cc, fail, jmp);
            }
            None => {
                let k = self.keep_late(code, Late::Final(Refusal::Unreachable));
                self.emit(code, cc, Op::Late { k });
            }
        }
        self.patch(code, cc, done);
        Ok(dst)
    }

    /// The type facts of `t`, kept in the code's table.
    fn ty<A: Allocator + Clone>(&mut self, code: &mut Code<A>, t: PTy) -> R<'a, u32> {
        let info = self.tyinfo(t)?;
        if let Some(i) = code.tys.iter().position(|x| *x == info) {
            return Ok(i as u32);
        }
        code.tys.push(info);
        Ok((code.tys.len() - 1) as u32)
    }

    /// Whether binding `p` of type `t` can be a register: never taken by address, and not an
    /// aggregate.
    fn reg_home(&mut self, cc: &mut Cc<'w, '_>, p: PatId, t: PTy) -> R<'a, bool> {
        if cc.addressed.get(p.ix()).copied().unwrap_or(true) {
            return Ok(false);
        }
        Ok(!matches!(self.repr(t), Ok(Repr::Mem(_)) | Err(_)))
    }

    /// Binding `p`'s home, made when first asked.
    fn home(&mut self, cc: &mut Cc<'w, '_>, code_slots: &mut u32, p: PatId) -> R<'a, Home> {
        let h = *cc.homes.get(p.ix()).ok_or(malformed("a pattern past the body"))?;
        if h != Home::None {
            return Ok(h);
        }
        let lt = *cc.typed.local_ty.get(p.ix()).ok_or(malformed("a pattern with no type"))?;
        let t = self.cg(cc, lt)?;
        let simple = matches!(cc.typed.body.pats.get(p.ix()), Some(Pat::Bind { ann: blaze_typeck::BindAnn { by_ref: None, .. }, .. }));
        let h = if simple && self.reg_home(cc, p, t)? {
            let r = self.reg(cc, Kind::Local(p));
            Home::Reg(r)
        } else {
            *code_slots += 1;
            Home::Slot(*code_slots - 1)
        };
        cc.homes[p.ix()] = h;
        Ok(h)
    }

    /// The place of a memory local, or a restart when it is a register.
    fn local_place<A: Allocator + Clone>(&mut self, cc: &mut Cc<'w, '_>, code: &mut Code<A>, p: PatId) -> R<'a, Reg> {
        match self.home(cc, &mut code.slots, p)? {
            Home::Slot(slot) => {
                let lt = cc.typed.local_ty[p.ix()];
                let t = self.cg(cc, lt)?;
                let ty = self.ty(code, t)?;
                let dst = self.reg(cc, Kind::P);
                self.emit(code, cc, Op::Local { dst, slot, ty });
                Ok(dst)
            }
            _ => {
                cc.restart = Some(p);
                Err(malformed("a register local taken by address"))
            }
        }
    }

    /// Register `r` as a value (a copy of a register local's).
    fn as_value<A: Allocator + Clone>(&mut self, cc: &mut Cc<'w, '_>, code: &mut Code<A>, r: Reg, t: PTy) -> R<'a, Reg> {
        Ok(match cc.kinds[r as usize] {
            Kind::V => r,
            Kind::Local(_) => {
                let dst = self.reg(cc, Kind::V);
                self.emit(code, cc, Op::Move { dst, src: r });
                dst
            }
            Kind::P => {
                let ty = self.ty(code, t)?;
                let dst = self.reg(cc, Kind::V);
                self.emit(code, cc, Op::Load { dst, src: r, ty });
                dst
            }
        })
    }

    /// Register `r` as a place.
    fn as_place<A: Allocator + Clone>(&mut self, cc: &mut Cc<'w, '_>, code: &mut Code<A>, r: Reg, t: PTy) -> R<'a, Reg> {
        Ok(match cc.kinds[r as usize] {
            Kind::P => r,
            Kind::Local(p) => {
                cc.restart = Some(p);
                return Err(malformed("a register local taken by address"));
            }
            Kind::V => {
                let ty = self.ty(code, t)?;
                let dst = self.reg(cc, Kind::P);
                self.emit(code, cc, Op::Spill { dst, src: r, ty });
                dst
            }
        })
    }

    fn pop_r(&self, cc: &mut Cc<'w, '_>) -> R<'a, Reg> {
        cc.rs.pop().ok_or(malformed("the value stack ran out"))
    }

    /// Whether `e`, through parentheses, is a local kept in a register, with no adjustment.
    fn reg_local(&self, cc: &Cc<'w, '_>, mut e: ExprId) -> Option<Reg> {
        loop {
            if !rows(cc.typed.adjusts.0, e).is_empty() {
                return None;
            }
            match cc.typed.body.exprs.get(e.ix())? {
                Expr::Paren(i) => e = *i,
                Expr::Local(p) => {
                    return match cc.homes.get(p.ix())? {
                        Home::Reg(r) => Some(*r),
                        _ => None,
                    };
                }
                _ => return None,
            }
        }
    }

    /// Whether `e` is a place expression (the Reference, "Place Expressions and Value Expressions").
    fn natural_c(&self, cc: &Cc<'w, '_>, mut e: ExprId) -> R<'a, Mode> {
        loop {
            return Ok(match self.expr_c(cc, e)? {
                Expr::Paren(i) => {
                    e = i;
                    continue;
                }
                Expr::Local(_) | Expr::Field { .. } | Expr::TupleField { .. } | Expr::Index { .. } | Expr::Unary { op: UnOp::Deref, .. } => Mode::Place,
                Expr::Path(_) => match self.expr_def_c(cc, e) {
                    Some(Def::Static(..) | Def::Const(_)) => Mode::Place,
                    _ => Mode::Value,
                },
                _ => Mode::Value,
            });
        }
    }

    /// A literal's value at type `t` as an operation; `neg`: under a unary minus.
    fn lit_op<A: Allocator + Clone>(&mut self, cc: &mut Cc<'w, '_>, code: &mut Code<A>, l: Lit, src: &str, t: PTy, neg: bool) -> R<'a, Reg> {
        let dst = self.reg(cc, Kind::V);
        let node = self.node(t)?;
        let v = match l {
            Lit::Bool(b) => Val::Int(u64::from(b)),
            Lit::Int(_) => {
                let v = lit::int(src).map_err(Refusal::Literal)?;
                match node {
                    // Rounded once from the integer, at every float width.
                    PNode::Float(f) => Val::int(Format::of(f).from_int(neg, v)),
                    _ => {
                        let (bits, _) = int_info(node).ok_or(malformed("an integer literal of a type that is not an integer"))?;
                        Val::int(mask(if neg { v.wrapping_neg() } else { v }, bits))
                    }
                }
            }
            Lit::Float(_) => {
                let PNode::Float(f) = node else { return Err(malformed("a float literal of a type that is not a float")) };
                let fm = Format::of(f);
                let bits = match f {
                    FloatTy::F32 => u128::from(lit::float(src, false).map_err(Refusal::Literal)?),
                    FloatTy::F64 => u128::from(lit::float(src, true).map_err(Refusal::Literal)?),
                    // The host parses neither: converted once from the digits (soft.rs).
                    FloatTy::F16 | FloatTy::F128 => fm.parse(src, self.alloc).map_err(Refusal::Literal)?,
                };
                let sign = if neg { fm.sign() } else { 0 };
                Val::int(bits ^ sign)
            }
            Lit::Char => Val::Int(u64::from(lit::char(src).map_err(Refusal::Literal)?)),
            Lit::Str | Lit::ByteStr(_) => {
                let bytes = lit::bytes(src, self.alloc).map_err(Refusal::Literal)?;
                let at = code.bytes.len() as u32;
                code.bytes.extend_from_slice(&bytes);
                let k = code.strs.len() as u32;
                code.strs.push(StrLit { at, len: bytes.len() as u32, wide: matches!(l, Lit::Str) });
                self.emit(code, cc, Op::Str { dst, k });
                return Ok(dst);
            }
            // `&CStr`: a wide pointer whose metadata is the length of its `[c_char]`, the
            // bytes and their terminating NUL (core's `CStr { inner: [c_char] }`).
            Lit::CStr => {
                let bytes = lit::c_bytes(src, self.alloc).map_err(Refusal::Literal)?;
                let at = code.bytes.len() as u32;
                code.bytes.extend_from_slice(&bytes);
                let k = code.strs.len() as u32;
                code.strs.push(StrLit { at, len: bytes.len() as u32, wide: true });
                self.emit(code, cc, Op::Str { dst, k });
                return Ok(dst);
            }
        };
        self.emit_imm(code, cc, dst, v);
        Ok(dst)
    }

    /// The parts of a value of `t` (variant `variant`) built of `(field, register)` pairs.
    fn build<A: Allocator + Clone>(&mut self, cc: &mut Cc<'w, '_>, code: &mut Code<A>, t: PTy, variant: u32, fields: &[(u32, Reg, PTy)], array: bool) -> R<'a, Reg> {
        let ty = self.ty(code, t)?;
        let dst = self.reg(cc, Kind::V);
        if code.tys[ty as usize].repr == Repr::Zst {
            self.emit(code, cc, Op::ImmZst { dst });
            return Ok(dst);
        }
        if !matches!(code.tys[ty as usize].repr, Repr::Mem(_)) {
            return Err(malformed("an ADT held as a scalar"));
        }
        let first = code.parts.len() as u32;
        for &(f, src, _) in fields {
            let ft = self.field_ty(t, variant, f)?;
            let off = if array { self.size_align(ft)?.0 * u64::from(f) } else { self.field_offset(t, variant, f)? };
            let fty = self.ty(code, ft)?;
            let src = self.as_value(cc, code, src, ft)?;
            code.parts.push(Part { src, off, ty: fty });
        }
        let tag = if array || matches!(self.node(t)?, PNode::Tuple(_) | PNode::Record(..)) {
            None
        } else {
            let (mut cx, l) = self.cx();
            l.tag_write(t, variant, &mut cx)?
        };
        code.builds.push(Build { ty, parts: (first, code.parts.len() as u32 - first), tag });
        let k = code.builds.len() as u32 - 1;
        self.emit(code, cc, Op::Build { dst, k });
        Ok(dst)
    }

    /// A call's argument registers as values, kept in the code: their range.
    fn arg_regs<A: Allocator + Clone>(&mut self, cc: &mut Cc<'w, '_>, code: &mut Code<A>, regs: &[(Reg, PTy)]) -> R<'a, (u32, u32)> {
        let first = code.regs_of.len() as u32;
        for &(r, t) in regs {
            let v = self.as_value(cc, code, r, t)?;
            code.regs_of.push((v, t));
        }
        Ok((first, regs.len() as u32))
    }

    // ---- Compile steps ----

    fn c<A: Allocator + Clone>(&mut self, cc: &mut Cc<'w, '_>, code: &mut Code<A>, task: C) -> R<'a, ()> {
        let typed = cc.typed;
        let body = typed.body;
        let list = |l: List| body.expr_lists.get(l.range()).ok_or(malformed("a list past `expr_lists`"));
        match task {
            C::Eval(e, mode) => {
                cc.tasks.push(C::Adjust(e, mode));
                self.raw(cc, code, e)?;
            }
            C::Adjust(e, mode) => self.adjust(cc, code, e, mode)?,
            C::Unary(e) => {
                let Expr::Unary { op, operand } = self.expr_c(cc, e)? else { return Err(malformed("a unary task")) };
                let t = self.final_ty_c(cc, operand)?;
                let src = self.pop_r(cc)?;
                let num = num(self.node(t)?);
                let dst = self.reg(cc, Kind::V);
                self.emit(code, cc, Op::Unary { dst, src, op, num });
                cc.rs.push(dst);
            }
            C::Binary(e) => {
                let Expr::Binary { op, lhs, rhs } = self.expr_c(cc, e)? else { return Err(malformed("a binary task")) };
                let (lt, rt) = (self.final_ty_c(cc, lhs)?, self.final_ty_c(cc, rhs)?);
                let r = self.pop_r(cc)?;
                let l = self.pop_r(cc)?;
                let (num, rnum) = (num(self.node(lt)?), num(self.node(rt)?));
                let dst = self.reg(cc, Kind::V);
                self.emit(code, cc, Op::Binary { dst, l, r, op, num, rnum });
                cc.rs.push(dst);
            }
            C::Logic(e) => {
                let Expr::Binary { op, rhs, .. } = self.expr_c(cc, e)? else { return Err(malformed("a logic task")) };
                let l = self.pop_r(cc)?;
                let dst = self.reg(cc, Kind::V);
                self.emit(code, cc, Op::Move { dst, src: l });
                let jump = self.emit(code, cc, Op::Branch { cond: dst, when: op == BinOp::Or, to: u32::MAX });
                cc.tasks.push(C::LogicEnd { e, dst, jump });
                cc.tasks.push(C::Eval(rhs, Mode::Value));
            }
            C::LogicEnd { dst, jump, .. } => {
                let r = self.pop_r(cc)?;
                self.emit(code, cc, Op::Move { dst, src: r });
                self.patch_one(code, jump);
                cc.rs.push(dst);
            }
            C::Cast(e) => {
                let Expr::Cast { operand, .. } = self.expr_c(cc, e)? else { return Err(malformed("a cast task")) };
                let from = self.final_ty_c(cc, operand)?;
                let to = self.ety_c(cc, e)?;
                let src = self.pop_r(cc)?;
                match (self.node(from)?, self.node(to)?) {
                    // A fn item cast to a fn pointer (`id::<T> as fn(T) -> T`) where the checker
                    // left the operand a fn item: reified as the adjustment is. Its value is
                    // zero-sized, so `src` holds nothing to read.
                    (PNode::FnDef(..), PNode::FnPtr(..)) => {
                        let dst = self.reify(code, cc, from)?;
                        cc.rs.push(dst);
                        return Ok(());
                    }
                    // A fn pointer to a fn pointer type (the operand already reified by its
                    // adjustment): the same word, which names its instance in the record. Never
                    // a numeric cast (`Machine::cast` would read the pointer as an integer).
                    (PNode::FnPtr(..), PNode::FnPtr(..)) => {
                        cc.rs.push(src);
                        return Ok(());
                    }
                    _ => {}
                }
                let dst = self.reg(cc, Kind::V);
                if let PNode::Adt(..) = self.node(from)? {
                    // A fieldless enum to its discriminant.
                    let (bits, _) = int_info(self.node(to)?).ok_or(malformed("an enum cast to a type that is not an integer"))?;
                    let e = self.enum_facts(code, from)?;
                    let ty = self.ty(code, from)?;
                    let k = code.discr_ops.len() as u32;
                    code.discr_ops.push(DiscrOp { e, ty, bits });
                    self.emit(code, cc, Op::Discr { dst, src, k });
                } else {
                    let k = code.casts.len() as u32;
                    code.casts.push((from, to));
                    self.emit(code, cc, Op::Cast { dst, src, k });
                }
                cc.rs.push(dst);
            }
            C::Ref(_) => {
                let src = self.pop_r(cc)?;
                let dst = self.reg(cc, Kind::V);
                self.emit(code, cc, Op::Borrow { dst, src, meta: Meta::Keep });
                cc.rs.push(dst);
            }
            C::RawRef(_) => {
                let src = self.pop_r(cc)?;
                let dst = self.reg(cc, Kind::V);
                self.emit(code, cc, Op::Borrow { dst, src, meta: Meta::Raw });
                cc.rs.push(dst);
            }
            C::Deref(e) => {
                let Expr::Unary { operand, .. } = self.expr_c(cc, e)? else { return Err(malformed("a deref task")) };
                let t = self.final_ty_c(cc, operand)?;
                if !matches!(self.node(t)?, PNode::Ref(..) | PNode::Ptr(..)) {
                    return Err(unsupported("an overloaded `*` in a constant (a const trait)"));
                }
                let src = self.pop_r(cc)?;
                let dst = self.reg(cc, Kind::P);
                self.emit(code, cc, Op::Deref { dst, src });
                cc.rs.push(dst);
            }
            C::Field(e) => {
                let src = self.pop_r(cc)?;
                let (base, variant, field) = match self.expr_c(cc, e)? {
                    Expr::TupleField { base, index } => (base, 0, index),
                    Expr::Field { base, .. } => {
                        let i = rows(typed.fields.0, e).next().ok_or(malformed("a field access with no resolution"))?;
                        (base, typed.fields.1[i], typed.fields.2[i])
                    }
                    _ => return Err(malformed("a field task")),
                };
                let bt = self.final_ty_c(cc, base)?;
                let off = self.field_offset(bt, variant, field)?;
                let ft = self.field_ty(bt, variant, field)?;
                let tail = layout::is_unsized(self.world, self.ground, ft, self.alloc)?;
                let dst = self.reg(cc, Kind::P);
                self.emit_field(code, cc, dst, src, off, tail);
                cc.rs.push(dst);
            }
            C::Index(e) => {
                let Expr::Index { base, index: ie } = self.expr_c(cc, e)? else { return Err(malformed("an index task")) };
                let index = self.pop_r(cc)?;
                let b = self.pop_r(cc)?;
                let bt = self.final_ty_c(cc, base)?;
                let (elem, len) = match self.node(bt)? {
                    PNode::Array(el, n) => (el, Some(self.len(n)?)),
                    PNode::Slice(el) => (el, None),
                    _ => return Err(unsupported("an overloaded `Index` in a constant (a const trait)")),
                };
                let _ = ie;
                let (size, _) = self.size_align(elem)?;
                let dst = self.reg(cc, Kind::P);
                let k = code.indexes.len() as u32;
                code.indexes.push(IndexOp { base: b, index, size, len });
                self.emit(code, cc, Op::Index { dst, k });
                cc.rs.push(dst);
            }
            C::Aggregate(e) => {
                let elems = match self.expr_c(cc, e)? {
                    Expr::Tuple { elems } | Expr::Array { elems } => elems,
                    _ => return Err(malformed("an aggregate task")),
                };
                let t = self.ety_c(cc, e)?;
                let n = elems.len as usize;
                let at = cc.rs.len().checked_sub(n).ok_or(malformed("the value stack ran out"))?;
                let mut fields: Vec<(u32, Reg, PTy), Alloc<'a>> = Vec::new_in(self.alloc);
                let array = matches!(self.node(t)?, PNode::Array(..));
                for (k, &r) in cc.rs[at..].iter().enumerate() {
                    fields.push((k as u32, r, PTy(0)));
                }
                cc.rs.truncate(at);
                let dst = self.build(cc, code, t, 0, &fields, array)?;
                cc.rs.push(dst);
            }
            C::Repeat(e) => {
                let t = self.ety_c(cc, e)?;
                let PNode::Array(el, n) = self.node(t)? else { return Err(malformed("a repeat of a type that is not an array")) };
                let n = self.len(n)?;
                let src = self.pop_r(cc)?;
                let ty = self.ty(code, t)?;
                let elem = self.ty(code, el)?;
                let dst = self.reg(cc, Kind::V);
                if code.tys[ty as usize].repr == Repr::Zst {
                    self.emit(code, cc, Op::ImmZst { dst });
                } else {
                    code.repeats.push(Repeat { ty, elem, n });
                    let k = code.repeats.len() as u32 - 1;
                    self.emit(code, cc, Op::Repeat { dst, src, k });
                }
                cc.rs.push(dst);
            }
            C::Struct(e) => {
                let (fields, base, is_record) = match self.expr_c(cc, e)? {
                    Expr::Struct { fields, base, .. } => (fields, base, false),
                    Expr::Record { fields, base } => (fields, base, true),
                    _ => return Err(malformed("a struct task")),
                };
                let t = self.ety_c(cc, e)?;
                let base_r = match base {
                    blaze_typeck::StructBase::Expr(_) => Some(self.pop_r(cc)?),
                    // `S { a: 1, .. }` (RFC 3681) builds as a literal; its unwritten fields
                    // are added below from their defaults.
                    blaze_typeck::StructBase::None | blaze_typeck::StructBase::Defaults => None,
                };
                let inits = body.field_inits.get(fields.range()).ok_or(malformed("field inits past their column"))?;
                let at = cc.rs.len().checked_sub(inits.len()).ok_or(malformed("the value stack ran out"))?;
                let mut vals: Vec<(u32, Reg, PTy), Alloc<'a>> = Vec::new_in(self.alloc);
                let (variant, field_count, default_args) = if is_record {
                    let PNode::Record(schema, flist) = self.node(t)? else {
                        return Err(malformed("a record expression of a type that is not a structural record"));
                    };
                    let slice = cc.rs.get(at..).ok_or(malformed("the value stack ran out"))?;
                    for (fi, &r) in inits.iter().zip(slice.iter()) {
                        let idx = self.record_field_index_c(cc, schema, fi.name)?;
                        vals.push((idx, r, PTy(0)));
                    }
                    let count = usize::try_from(flist.len).map_err(|_| malformed("a record field count past `usize`"))?;
                    (0u32, count, List::EMPTY)
                } else {
                    let PNode::Adt(key, args) = self.node(t)? else {
                        return Err(malformed("a struct expression of a type that is not an ADT"));
                    };
                    let adt = self.world.adt(key).ok_or(Stop::Refused(Refusal::Missing { what: "ADT definition", item: key }))?;
                    let variant = match self.expr_def_c(cc, e) {
                        Some(Def::Ctor { variant, .. }) => variant,
                        _ => 0,
                    };
                    let v = adt.variants.get(variant as usize).ok_or(malformed("a variant past its ADT"))?;
                    let names = adt.fields.get(v.fields.range()).ok_or(malformed("fields past their ADT"))?;
                    if adt.kind == AdtKind::Union && inits.len() != 1 {
                        return Err(malformed("a union expression of other than one field"));
                    }
                    let slice = cc.rs.get(at..).ok_or(malformed("the value stack ran out"))?;
                    for (fi, &r) in inits.iter().zip(slice.iter()) {
                        let idx = names.iter().position(|f| f.name == fi.name).ok_or(malformed("a field name its variant lacks"))?;
                        let idx = u32::try_from(idx).map_err(|_| malformed("a field index past `u32`"))?;
                        vals.push((idx, r, PTy(0)));
                    }
                    (variant, names.len(), args)
                };
                cc.rs.truncate(at);
                // RFC 3681: each field `S { a: 1, .. }` does not write is its default
                // (`Typed::default_fields`), a constant of the field's type over the ADT's
                // generics (or empty args for a structural record), read at this expression's
                // type's ground arguments as a constant path is read (`Op::Const`).
                if base_r.is_none() {
                    let typed = cc.typed;
                    for i in rows(typed.default_fields.0, e) {
                        let Some(&idx) = typed.default_fields.1.get(i) else { return Err(malformed("a field default past its column")) };
                        let Some(&k) = typed.default_fields.2.get(i) else { return Err(malformed("a field default past its column")) };
                        let idx_usize = usize::try_from(idx).map_err(|_| malformed("a field default index past `usize`"))?;
                        if idx_usize >= field_count || vals.iter().any(|&(f, _, _)| f == idx) {
                            return Err(malformed("a field default past its variant, or of a field the expression writes"));
                        }
                        let key = RecordKey::of(self.instance(k, default_args)?);
                        code.consts.push((k, default_args, key));
                        let dst = self.reg(cc, Kind::P);
                        let ck = u32::try_from(code.consts.len() - 1).map_err(|_| malformed("const table past `u32`"))?;
                        self.emit(code, cc, Op::Const { dst, k: ck });
                        vals.push((idx, dst, PTy(0)));
                    }
                }
                let dst = match base_r {
                    Some(b) => {
                        let ty = self.ty(code, t)?;
                        let first = code.parts.len() as u32;
                        for &(f, src, _) in vals.iter() {
                            let ft = self.field_ty(t, variant, f)?;
                            let off = self.field_offset(t, variant, f)?;
                            let fty = self.ty(code, ft)?;
                            let src = self.as_value(cc, code, src, ft)?;
                            code.parts.push(Part { src, off, ty: fty });
                        }
                        code.builds.push(Build { ty, parts: (first, code.parts.len() as u32 - first), tag: None });
                        let k = code.builds.len() as u32 - 1;
                        let dst = self.reg(cc, Kind::V);
                        self.emit(code, cc, Op::Update { dst, base: b, k });
                        dst
                    }
                    None => self.build(cc, code, t, variant, &vals, false)?,
                };
                cc.rs.push(dst);
            }
            C::IfCond(e) => {
                let Expr::If { then, .. } = self.expr_c(cc, e)? else { return Err(malformed("an if task")) };
                let cond = self.pop_r(cc)?;
                let dst = self.reg(cc, Kind::V);
                let jump = self.emit(code, cc, Op::Branch { cond, when: false, to: u32::MAX });
                cc.tasks.push(C::IfThen { e, dst, jump });
                cc.tasks.push(C::Eval(then, Mode::Value));
            }
            C::IfThen { e, dst, jump } => {
                let Expr::If { else_, .. } = self.expr_c(cc, e)? else { return Err(malformed("an if task")) };
                let t = self.pop_r(cc)?;
                self.emit(code, cc, Op::Move { dst, src: t });
                let end = self.jump(code, cc);
                self.patch_one(code, jump);
                match else_ {
                    Some(x) => {
                        cc.tasks.push(C::IfElse { e, dst, jump: end });
                        cc.tasks.push(C::Eval(x, Mode::Value));
                    }
                    None => {
                        self.emit(code, cc, Op::ImmZst { dst });
                        self.patch_one(code, end);
                        cc.rs.push(dst);
                    }
                }
            }
            C::IfElse { dst, jump, .. } => {
                let x = self.pop_r(cc)?;
                self.emit(code, cc, Op::Move { dst, src: x });
                self.patch_one(code, jump);
                cc.rs.push(dst);
            }
            C::Block { e, next } => {
                let (Expr::Block { stmts, tail, .. } | Expr::ConstBlock { stmts, tail, .. }) = self.expr_c(cc, e)? else { return Err(malformed("a block task")) };
                let l = body.stmt_lists.get(stmts.range()).ok_or(malformed("statements past their column"))?;
                match l.get(next as usize) {
                    Some(&s) => {
                        cc.tasks.push(C::Block { e, next: next + 1 });
                        match body.stmts.get(s.ix()).copied().ok_or(malformed("a statement past the body"))? {
                            Stmt::Let { init: Some(init), pat, .. } => {
                                cc.tasks.push(C::Let { e, s });
                                // A plain binding into a register takes the value; any other
                                // pattern matches the place.
                                let simple = matches!(body.pats.get(pat.ix()), Some(Pat::Bind { ann: blaze_typeck::BindAnn { by_ref: None, .. }, sub: None, .. }))
                                    && self.union_pat_c(cc, pat).is_none();
                                let reg = simple && {
                                    let t = self.cg(cc, *typed.pat_ty.get(pat.ix()).ok_or(malformed("a pattern with no type"))?)?;
                                    self.reg_home(cc, pat, t)?
                                };
                                cc.tasks.push(C::Eval(init, if reg { Mode::Value } else { Mode::Place }));
                            }
                            Stmt::Let { init: None, .. } | Stmt::Item => {}
                            Stmt::Expr { expr, .. } => {
                                let mark = self.reg(cc, Kind::V);
                                let at = self.emit(code, cc, Op::Mark { dst: mark });
                                cc.temps = 0;
                                cc.tasks.push(C::StmtEnd { e, mark, at });
                                cc.tasks.push(C::Eval(expr, Mode::Value));
                            }
                        }
                    }
                    None => match tail {
                        Some(t) => cc.tasks.push(C::Eval(t, Mode::Value)),
                        None => {
                            let d = self.reg(cc, Kind::V);
                            self.emit(code, cc, Op::ImmZst { dst: d });
                            cc.rs.push(d);
                        }
                    },
                }
            }
            C::StmtEnd { mark, at, .. } => {
                self.pop_r(cc)?;
                // An expression statement's temporaries end with it (the Reference,
                // "Temporary scopes"); a statement that made none needs no mark.
                if cc.temps == 0 {
                    code.ops[at as usize] = Op::Nop;
                } else {
                    self.emit(code, cc, Op::Kill { mark });
                }
                cc.temps = 1;
            }
            C::BlockEnd(e) => {
                let v = self.pop_r(cc)?;
                let t = cc.targets.pop().ok_or(malformed("a block with no target"))?;
                if t.e != e {
                    return Err(malformed("targets out of order"));
                }
                self.emit(code, cc, Op::Move { dst: t.dst, src: v });
                self.patch(code, cc, t.ends);
                cc.rs.push(t.dst);
            }
            C::Let { s, e } => {
                let Stmt::Let { pat, else_, .. } = body.stmts.get(s.ix()).copied().ok_or(malformed("a statement past the body"))? else {
                    return Err(malformed("a let task"));
                };
                let init = self.pop_r(cc)?;
                let t = self.cg(cc, *typed.pat_ty.get(pat.ix()).ok_or(malformed("a pattern with no type"))?)?;
                if cc.kinds[init as usize] != Kind::P {
                    // A plain binding kept in a register.
                    let Home::Reg(r) = self.home(cc, &mut code.slots, pat)? else { return Err(malformed("a value for a binding in memory")) };
                    let v = self.as_value(cc, code, init, t)?;
                    self.emit(code, cc, Op::Move { dst: r, src: v });
                    return Ok(());
                }
                let fail = self.list(cc);
                self.pattern(cc, code, pat, init, t, fail)?;
                match else_ {
                    Some(x) => {
                        let ok = self.jump(code, cc);
                        self.patch(code, cc, fail);
                        cc.tasks.push(C::LetElse { e, jump: ok });
                        cc.tasks.push(C::Eval(x, Mode::Value));
                    }
                    None => {
                        let ok = self.jump(code, cc);
                        self.patch(code, cc, fail);
                        let k = self.keep_late(code, Late::Final(Refusal::Malformed { what: "a refutable `let` with no `else`" }));
                        self.emit(code, cc, Op::Late { k });
                        self.patch_one(code, ok);
                    }
                }
            }
            C::LetElse { jump, .. } => {
                self.pop_r(cc)?;
                self.patch_one(code, jump);
            }
            C::LoopBody(e) => {
                self.pop_r(cc)?;
                let t = *cc.targets.iter().rev().find(|t| t.e == e).ok_or(malformed("a loop with no target"))?;
                self.emit(code, cc, Op::Jump { to: t.start.ok_or(malformed("a loop target with no start"))? });
            }
            C::LoopEnd(e) => {
                let t = cc.targets.pop().ok_or(malformed("a loop with no target"))?;
                if t.e != e {
                    return Err(malformed("targets out of order"));
                }
                self.patch(code, cc, t.ends);
                cc.rs.push(t.dst);
            }
            C::Break(e) => {
                let Expr::Break { target, .. } = self.expr_c(cc, e)? else { return Err(malformed("a break task")) };
                let v = self.pop_r(cc)?;
                let t = *cc.targets.iter().rev().find(|t| t.e == target).ok_or(malformed("a `break` with no target"))?;
                self.emit(code, cc, Op::Move { dst: t.dst, src: v });
                let j = self.jump(code, cc);
                self.to_list(cc, t.ends, j);
                let d = self.reg(cc, Kind::V);
                cc.rs.push(d);
            }
            C::Return(_) => {
                let v = self.pop_r(cc)?;
                self.emit(code, cc, Op::Ret { src: v });
                let d = self.reg(cc, Kind::V);
                cc.rs.push(d);
            }
            C::Call(e) => self.call_c(cc, code, e)?,
            C::CondLink { .. } | C::CondLet { .. } | C::CondBool { .. } | C::IfLetEnd { .. } => self.let_c(cc, code, task)?,
            C::Match(e) => {
                let Expr::Match { scrutinee, .. } = self.expr_c(cc, e)? else { return Err(malformed("a match task")) };
                let s = self.pop_r(cc)?;
                let ty = self.final_ty_c(cc, scrutinee)?;
                let place = self.as_place(cc, code, s, ty)?;
                let dst = self.reg(cc, Kind::V);
                let ends = self.list(cc);
                cc.tasks.push(C::Arm { e, arm: 0, place, ty, dst, ends });
            }
            C::Arm { e, arm, place, ty, dst, ends } => {
                let Expr::Match { arms, .. } = self.expr_c(cc, e)? else { return Err(malformed("an arm task")) };
                let l = body.arms.get(arms.range()).ok_or(malformed("arms past their column"))?;
                match l.get(arm as usize) {
                    None => {
                        let k = self.keep_late(code, Late::Final(Refusal::Unreachable));
                        self.emit(code, cc, Op::Late { k });
                        self.patch(code, cc, ends);
                        cc.rs.push(dst);
                    }
                    Some(a) => {
                        let fail = self.list(cc);
                        self.pattern(cc, code, a.pat, place, ty, fail)?;
                        match a.guard {
                            Some(g) => {
                                cc.tasks.push(C::Guard { e, arm, place, ty, dst, ends, fail });
                                cc.tasks.push(C::Eval(g, Mode::Value));
                            }
                            None => {
                                cc.tasks.push(C::ArmBody { e, arm, place, ty, dst, ends, fail });
                                cc.tasks.push(C::Eval(a.body, Mode::Value));
                            }
                        }
                    }
                }
            }
            C::Guard { e, arm, place, ty, dst, ends, fail } => {
                let Expr::Match { arms, .. } = self.expr_c(cc, e)? else { return Err(malformed("a guard task")) };
                let a = *body.arms.get(arms.range()).and_then(|l| l.get(arm as usize)).ok_or(malformed("arms past their column"))?;
                let g = self.pop_r(cc)?;
                let j = self.emit(code, cc, Op::Branch { cond: g, when: false, to: u32::MAX });
                self.to_list(cc, fail, j);
                cc.tasks.push(C::ArmBody { e, arm, place, ty, dst, ends, fail });
                cc.tasks.push(C::Eval(a.body, Mode::Value));
            }
            C::ArmBody { e, arm, place, ty, dst, ends, fail } => {
                let b = self.pop_r(cc)?;
                self.emit(code, cc, Op::Move { dst, src: b });
                let j = self.jump(code, cc);
                self.to_list(cc, ends, j);
                self.patch(code, cc, fail);
                cc.tasks.push(C::Arm { e, arm: arm + 1, place, ty, dst, ends });
            }
            C::Assign(e) => {
                let Expr::Assign { lhs, .. } = self.expr_c(cc, e)? else { return Err(malformed("an assign task")) };
                let t = self.final_ty_c(cc, lhs)?;
                match self.reg_local(cc, lhs) {
                    Some(r) => {
                        let v = self.pop_r(cc)?;
                        let v = self.as_value(cc, code, v, t)?;
                        self.emit(code, cc, Op::Move { dst: r, src: v });
                    }
                    None => {
                        let place = self.pop_r(cc)?;
                        let v = self.pop_r(cc)?;
                        let src = self.as_value(cc, code, v, t)?;
                        let ty = self.ty(code, t)?;
                        self.emit(code, cc, Op::Store { place, src, ty });
                    }
                }
                let d = self.reg(cc, Kind::V);
                self.emit(code, cc, Op::ImmZst { dst: d });
                cc.rs.push(d);
            }
            C::AssignOp(e) => {
                let Expr::AssignOp { op, lhs, rhs } = self.expr_c(cc, e)? else { return Err(malformed("an assign-op task")) };
                let (lt, rt) = (self.final_ty_c(cc, lhs)?, self.final_ty_c(cc, rhs)?);
                let (num, rnum) = (num(self.node(lt)?), num(self.node(rt)?));
                match self.reg_local(cc, lhs) {
                    Some(r) => {
                        let v = self.pop_r(cc)?;
                        let v = self.as_value(cc, code, v, rt)?;
                        self.emit(code, cc, Op::Binary { dst: r, l: r, r: v, op, num, rnum });
                    }
                    None => {
                        let place = self.pop_r(cc)?;
                        let v = self.pop_r(cc)?;
                        let src = self.as_value(cc, code, v, rt)?;
                        let ty = self.ty(code, lt)?;
                        self.emit(code, cc, Op::StoreOp { place, src, ty, op, num, rnum });
                    }
                }
                let d = self.reg(cc, Kind::V);
                self.emit(code, cc, Op::ImmZst { dst: d });
                cc.rs.push(d);
            }
        }
        let _ = list;
        Ok(())
    }

    /// Starts expression `e` in its natural mode: its operation, or the tasks that make it.
    fn raw<A: Allocator + Clone>(&mut self, cc: &mut Cc<'w, '_>, code: &mut Code<A>, e: ExprId) -> R<'a, ()> {
        let typed = cc.typed;
        let body = typed.body;
        let list = |l: List| body.expr_lists.get(l.range()).ok_or(malformed("a list past `expr_lists`"));
        match self.expr_c(cc, e)? {
            Expr::Missing => return Err(malformed("a missing expression")),
            Expr::Lit(l) => {
                let t = self.ety_c(cc, e)?;
                let src = typed.lits.expr(e).ok_or(malformed("a literal expression with no written text"))?;
                let r = self.lit_op(cc, code, l, src, t, false)?;
                cc.rs.push(r);
            }
            Expr::Local(p) => match self.home(cc, &mut code.slots, p)? {
                Home::Reg(r) => cc.rs.push(r),
                _ => {
                    let r = self.local_place(cc, code, p)?;
                    cc.rs.push(r);
                }
            },
            Expr::Path(p) => {
                let t = self.ety_c(cc, e)?;
                match self.expr_def_c(cc, e) {
                    Some(Def::Const(k)) => {
                        // `<Q as Trait>::NAME` names the trait's constant at `Self = Q`
                        // ([`Self::qualified_const_args`]): the impl is selected for the
                        // ground `Q`, as a call's callee is. Any other path stays at no
                        // arguments.
                        // The check's recorded arguments first (`Self::NAME`, `T::NAME` and
                        // inferred trait arguments included); a path with no row keeps what it
                        // writes.
                        let recorded = self.recorded_const_args(cc, e)?;
                        let written = if recorded.len != 0 { recorded } else { self.qualified_const_args(cc, p)? };
                        // No impl selected is `Refusal::Missing`, a pending stop: this code
                        // is not complete (machine/late.rs), so a world that then makes the
                        // trait's impls and asks again has it compiled again.
                        let (k, args) = if written.len == 0 { (k, List::EMPTY) } else { self.resolve(k, written)? };
                        let key = RecordKey::of(self.instance(k, args)?);
                        code.consts.push((k, args, key));
                        let dst = self.reg(cc, Kind::P);
                        self.emit(code, cc, Op::Const { dst, k: code.consts.len() as u32 - 1 });
                        cc.rs.push(dst);
                    }
                    Some(Def::Static(k, m)) => {
                        let sk = code.statics.len() as u32;
                        code.statics.push((k, m == Mutbl::Mut));
                        let dst = self.reg(cc, Kind::P);
                        self.emit(code, cc, Op::Static { dst, k: sk });
                        cc.rs.push(dst);
                    }
                    Some(Def::Ctor { variant, kind: blaze_typeck::CtorKind::Unit, .. }) => {
                        let r = self.build(cc, code, t, variant, &[], false)?;
                        cc.rs.push(r);
                    }
                    // A const generic parameter as a value (the Reference, "Const generics":
                    // "const parameters may be used ... as a standalone expression"): its
                    // value is this instance's argument at the parameter's index, a ground
                    // `ConstValue` (`CAP` in `const LAP: usize = CAP + 1` of
                    // `impl<T, const CAP: usize>`, `N` in a const block of a fn generic over
                    // `const N`).
                    Some(Def::TyParam(i)) => {
                        let v = self.const_param_value(cc, i)?;
                        let dst = self.reg(cc, Kind::V);
                        self.emit_imm(code, cc, dst, v);
                        cc.rs.push(dst);
                    }
                    _ => match self.node(t)? {
                        PNode::FnDef(..) => {
                            let dst = self.reg(cc, Kind::V);
                            self.emit(code, cc, Op::ImmZst { dst });
                            cc.rs.push(dst);
                        }
                        _ => return Err(malformed("a path with no definition")),
                    },
                }
            }
            Expr::Paren(i) => {
                let mode = self.natural_c(cc, i)?;
                cc.tasks.push(C::Eval(i, mode));
            }
            // The Reference, "Block expressions", "Const blocks": "A const block is a variant of a
            // block expression whose body evaluates at compile-time instead of at runtime", with
            // the enclosing generic parameters in scope. Inside an evaluation (already compile
            // time, at the evaluation's arguments) its value is its block's; it is also the root
            // of a const block's own evaluation (blaze-ladder front2/const_at.rs).
            Expr::Block { .. } | Expr::ConstBlock { .. } => {
                let dst = self.reg(cc, Kind::V);
                let ends = self.list(cc);
                cc.targets.push(Target { e, dst, ends, start: None });
                cc.tasks.push(C::BlockEnd(e));
                cc.tasks.push(C::Block { e, next: 0 });
            }
            Expr::If { cond, .. } if self.cond_by_links(cc, cond)? => self.if_let(cc, e, cond)?,
            Expr::If { cond, .. } => {
                cc.tasks.push(C::IfCond(e));
                cc.tasks.push(C::Eval(cond, Mode::Value));
            }
            Expr::Match { scrutinee, arms } => {
                cc.tasks.push(C::Match(e));
                // A scrutinee kept in a register is matched through a copy unless an arm binds
                // by reference.
                let by_ref = body.arms.get(arms.range()).is_some_and(|l| l.iter().any(|a| self.binds_by_ref(cc, a.pat)));
                let mode = if !by_ref && self.reg_local(cc, scrutinee).is_some() { Mode::Value } else { Mode::Place };
                cc.tasks.push(C::Eval(scrutinee, mode));
            }
            Expr::Loop { body: b } => {
                let dst = self.reg(cc, Kind::V);
                let ends = self.list(cc);
                let start = code.ops.len() as u32;
                cc.targets.push(Target { e, dst, ends, start: Some(start) });
                cc.tasks.push(C::LoopEnd(e));
                cc.tasks.push(C::LoopBody(e));
                cc.tasks.push(C::Eval(b, Mode::Value));
            }
            Expr::Break { value, .. } => {
                cc.tasks.push(C::Break(e));
                match value {
                    Some(v) => cc.tasks.push(C::Eval(v, Mode::Value)),
                    None => {
                        let d = self.reg(cc, Kind::V);
                        self.emit(code, cc, Op::ImmZst { dst: d });
                        cc.rs.push(d);
                    }
                }
            }
            Expr::Continue { target } => {
                let t = *cc.targets.iter().rev().find(|t| t.e == target).ok_or(malformed("a `continue` with no target"))?;
                self.emit(code, cc, Op::Jump { to: t.start.ok_or(malformed("a `continue` whose target is no loop"))? });
                let d = self.reg(cc, Kind::V);
                cc.rs.push(d);
            }
            Expr::Return { value } => {
                cc.tasks.push(C::Return(e));
                match value {
                    Some(v) => cc.tasks.push(C::Eval(v, Mode::Value)),
                    None => {
                        let d = self.reg(cc, Kind::V);
                        self.emit(code, cc, Op::ImmZst { dst: d });
                        cc.rs.push(d);
                    }
                }
            }
            Expr::Call { args, .. } => {
                cc.tasks.push(C::Call(e));
                for &a in list(args)?.iter().rev() {
                    cc.tasks.push(C::Eval(a, Mode::Value));
                }
            }
            Expr::MethodCall { receiver, args, .. } => {
                cc.tasks.push(C::Call(e));
                for &a in list(args)?.iter().rev() {
                    cc.tasks.push(C::Eval(a, Mode::Value));
                }
                cc.tasks.push(C::Eval(receiver, Mode::Value));
            }
            Expr::Field { base, .. } | Expr::TupleField { base, .. } => {
                cc.tasks.push(C::Field(e));
                cc.tasks.push(C::Eval(base, Mode::Place));
            }
            Expr::Unary { op: UnOp::Deref, operand } => {
                cc.tasks.push(C::Deref(e));
                cc.tasks.push(C::Eval(operand, Mode::Value));
            }
            Expr::Unary { op: UnOp::Neg, operand } if matches!(self.expr_c(cc, operand)?, Expr::Lit(Lit::Int(_) | Lit::Float(_))) => {
                let Expr::Lit(l) = self.expr_c(cc, operand)? else { return Err(malformed("a literal")) };
                let t = self.ety_c(cc, e)?;
                let src = typed.lits.expr(operand).ok_or(malformed("a literal expression with no written text"))?;
                let r = self.lit_op(cc, code, l, src, t, true)?;
                cc.rs.push(r);
            }
            Expr::Unary { operand, .. } => {
                cc.tasks.push(C::Unary(e));
                cc.tasks.push(C::Eval(operand, Mode::Value));
            }
            Expr::Binary { op: BinOp::And | BinOp::Or, lhs, .. } => {
                cc.tasks.push(C::Logic(e));
                cc.tasks.push(C::Eval(lhs, Mode::Value));
            }
            Expr::Binary { lhs, rhs, .. } => {
                cc.tasks.push(C::Binary(e));
                cc.tasks.push(C::Eval(rhs, Mode::Value));
                cc.tasks.push(C::Eval(lhs, Mode::Value));
            }
            Expr::Assign { lhs, rhs } => {
                // The right operand first, then the place (the Reference, "Assignment expressions").
                cc.tasks.push(C::Assign(e));
                if self.reg_local(cc, lhs).is_none() {
                    cc.tasks.push(C::Eval(lhs, Mode::Place));
                }
                cc.tasks.push(C::Eval(rhs, Mode::Value));
            }
            Expr::AssignOp { lhs, rhs, .. } => {
                // Both primitive: the modifying operand first (the Reference, "Compound assignment").
                cc.tasks.push(C::AssignOp(e));
                if self.reg_local(cc, lhs).is_none() {
                    cc.tasks.push(C::Eval(lhs, Mode::Place));
                }
                cc.tasks.push(C::Eval(rhs, Mode::Value));
            }
            Expr::Ref { operand, .. } => {
                cc.tasks.push(C::Ref(e));
                cc.tasks.push(C::Eval(operand, Mode::Place));
            }
            Expr::Cast { operand, .. } => {
                cc.tasks.push(C::Cast(e));
                cc.tasks.push(C::Eval(operand, Mode::Value));
            }
            Expr::Tuple { elems } | Expr::Array { elems } => {
                cc.tasks.push(C::Aggregate(e));
                for &a in list(elems)?.iter().rev() {
                    cc.tasks.push(C::Eval(a, Mode::Value));
                }
            }
            Expr::Repeat { elem, .. } => {
                cc.tasks.push(C::Repeat(e));
                cc.tasks.push(C::Eval(elem, Mode::Value));
            }
            Expr::Struct { fields, base, .. } | Expr::Record { fields, base } => {
                cc.tasks.push(C::Struct(e));
                if let Some(b) = base.expr() {
                    cc.tasks.push(C::Eval(b, Mode::Place));
                }
                let inits = body.field_inits.get(fields.range()).ok_or(malformed("field inits past their column"))?;
                for fi in inits.iter().rev() {
                    cc.tasks.push(C::Eval(fi.expr, Mode::Value));
                }
            }
            Expr::Index { base, index } => {
                cc.tasks.push(C::Index(e));
                cc.tasks.push(C::Eval(index, Mode::Value));
                cc.tasks.push(C::Eval(base, Mode::Place));
            }
            // A closure's value is its environment, one field per capture (codegen.md,
            // "Closures and the Fn traits"). One the checker coerced to a fn pointer captures
            // nothing (the Reference, "Type coercions": "Non capturing closures to `fn`
            // pointers"; rules/coerce.rs makes `ClosureFnPointer` only then), so its value is
            // zero-sized and its adjustment (`Self::adjust`) makes the pointer; that needs no
            // closure facts of the world (`thread_local!`'s `LocalKey::new(const { |_| .. })`).
            // Any other closure is zero-sized exactly when its layout says so, and a zero-sized
            // value has no bytes to make. A closure holding bytes of a capture would need the
            // captured bindings, which `Typed` does not carry: refused by name.
            Expr::Closure { is_async, .. } => {
                if is_async {
                    return Err(unsupported("an `async` closure in a constant"));
                }
                let coerced = rows(typed.adjusts.0, e).any(|i| typed.adjusts.1[i] == AdjustKind::ClosureFnPointer);
                if !coerced {
                    let t = self.ety_c(cc, e)?;
                    let ty = self.ty(code, t)?;
                    if code.tys[ty as usize].repr != Repr::Zst {
                        return Err(unsupported("a closure whose captures hold bytes in a constant"));
                    }
                }
                let dst = self.reg(cc, Kind::V);
                self.emit(code, cc, Op::ImmZst { dst });
                cc.rs.push(dst);
            }
            // An `if` condition's `let` links are compiled by machine/lets.rs; a `let` reached
            // here is not a condition operand (inside parentheses, under `||`).
            Expr::Let { .. } => return Err(unsupported("`let` outside an `if` or `while` condition's `&&` chain")),
            Expr::FormatArgs { .. } => return Err(unsupported("`format_args!` in a constant")),
            Expr::Await(_) | Expr::AsyncBlock { .. } => return Err(unsupported("`async` in a constant")),
            Expr::Underscore => return Err(unsupported("`_` (`_ = e`) in a constant")),
            // The Reference, inline-assembly: "asm! ... cannot be used in const contexts".
            Expr::Asm { .. } => return Err(unsupported("inline assembly in a constant")),
            Expr::RawRef { operand, .. } => {
                cc.tasks.push(C::RawRef(e));
                cc.tasks.push(C::Eval(operand, Mode::Place));
            }
            // `offset_of!(T, a.0.b)`: a `usize`, the sum of each step's offset in the layout
            // ([`Self::offset_of`]). core's `ptr::read_unaligned` asserts
            // `mem::offset_of!(Unaligned::<T>, 0) == 0` in a `const { .. }` block at every `T`.
            Expr::OffsetOf { ty, fields } => {
                let off = self.offset_of(cc, ty, fields)?;
                let dst = self.reg(cc, Kind::V);
                self.emit(code, cc, Op::ImmU64 { dst, v: off });
                cc.rs.push(dst);
            }
        }
        Ok(())
    }

    /// The value of `offset_of!(ty, fields..)` at this instance: the sum of each step's offset
    /// in the layout this evaluator lays types out with (blaze-build's `Layouts`, the build's
    /// own rows, so the value equals what blaze-build walk/strs.rs `offset_of` builds at run
    /// time). The Reference, "Type layout": a field's offset is its container's layout's
    /// (`repr(C)`, `packed` and `align` included); core's `mem::offset_of!`: "Expands to the
    /// offset in bytes of a field from the beginning of the given type", nested fields allowed
    /// (`a.0.b`). A tuple's or tuple struct's step is named by its digits (`Name::of_index`), a
    /// struct's or union's by its field name. A step through an enum variant (unstable
    /// `offset_of_enum`) is refused by name.
    fn offset_of(&mut self, cc: &mut Cc<'w, '_>, ty: PTy, fields: List) -> R<'a, u64> {
        let body = cc.typed.body;
        let steps = body.segments.get(fields.range()).ok_or(malformed("an `offset_of!` path past the body's segments"))?;
        if steps.is_empty() {
            return Err(malformed("an `offset_of!` with no field"));
        }
        // `ty` is the body's WRITTEN type (`BodyView::types`: the checker lowers it with
        // `lower_body_ty`, blaze-typeck rules/field.rs `expr_offset_of`), not a row of the
        // record's table `cc.typed.types` that [`Self::cg`] grounds: an index read in the wrong
        // table is another type (here one with no ground form). Grounded at this instance's
        // arguments as blaze-build walk/strs.rs `offset_of` does at run time.
        let mut t = blaze_build::ground(body.types, ty, &cc.args, self.ground, self.alloc, self.world).map_err(|e| match e {
            blaze_build::BuildError::Projection { trait_, name } => Stop::Refused(Refusal::Projection { trait_, name }),
            _ => unsupported("an `offset_of!` type with no ground form at the instance (an alias, an opaque type, a closure)"),
        })?;
        let mut total: u64 = 0;
        for step in steps {
            let index = match self.node(t)? {
                PNode::Tuple(l) => {
                    let n = self.ground.view().list(l).map_err(|_| malformed("a tuple's list past the ground table"))?.len() as u32;
                    (0..n).find(|&i| blaze_typeck::Name::of_index(i) == step.name)
                }
                PNode::Record(schema, _) => self
                    .ground
                    .view()
                    .record_field_index(schema, step.name)
                    .or_else(|| cc.typed.types.record_field_index(schema, step.name)),
                PNode::Adt(key, _) => {
                    let adt = self.world.adt(key).ok_or(Stop::Refused(Refusal::Missing { what: "ADT definition", item: key }))?;
                    if adt.kind == AdtKind::Enum {
                        return Err(unsupported("`offset_of!` through an enum variant (`offset_of_enum`)"));
                    }
                    let v = adt.variants.first().ok_or(malformed("a struct with no variant"))?;
                    let own = adt.fields.get(v.fields.range()).ok_or(malformed("a variant's fields past its ADT"))?;
                    own.iter().position(|f| f.name == step.name).map(|i| i as u32)
                }
                _ => return Err(unsupported("`offset_of!` through a field of a type with no fields")),
            };
            let index = index.ok_or(malformed("an `offset_of!` step naming a field its container lacks"))?;
            let off = self.field_offset(t, 0, index)?;
            total = total.checked_add(off).ok_or(malformed("an `offset_of!` past `u64`"))?;
            t = self.field_ty(t, 0, index)?;
        }
        Ok(total)
    }

    /// Whether pattern `top` binds anything by reference (`ref`, `ref mut`).
    fn binds_by_ref(&self, cc: &Cc<'w, '_>, top: PatId) -> bool {
        // Patterns are numbered after their parts: a pattern's subpatterns are below it, so the
        // ones from the lowest part up to `top` cover its tree (and maybe more: only `true`
        // costs, never correctness).
        let pats = cc.typed.body.pats;
        pats.get(..=top.ix()).is_none_or(|ps| ps.iter().any(|p| matches!(p, Pat::Bind { ann: blaze_typeck::BindAnn { by_ref: Some(_), .. }, .. })))
    }

    /// Applies `e`'s adjustments to its raw result and gives it in `mode`.
    fn adjust<A: Allocator + Clone>(&mut self, cc: &mut Cc<'w, '_>, code: &mut Code<A>, e: ExprId, mode: Mode) -> R<'a, ()> {
        let typed = cc.typed;
        let mut out = self.pop_r(cc)?;
        let mut ty = self.ety_c(cc, e)?;
        for i in rows(typed.adjusts.0, e) {
            let next = self.cg(cc, typed.adjusts.2[i])?;
            out = match typed.adjusts.1[i] {
                // A fn pointer's safety, like a raw pointer's mutability: the same value.
                AdjustKind::NeverToAny | AdjustKind::MutToConstPointer | AdjustKind::UnsafeFnPointer => out,
                AdjustKind::Deref { overloaded: None } => {
                    let src = match cc.kinds[out as usize] {
                        Kind::Local(_) => out,
                        _ => self.as_value(cc, code, out, ty)?,
                    };
                    let dst = self.reg(cc, Kind::P);
                    self.emit(code, cc, Op::Deref { dst, src });
                    dst
                }
                AdjustKind::Deref { overloaded: Some(_) } => return Err(unsupported("an overloaded `Deref` in a constant (a const trait)")),
                AdjustKind::Borrow { .. } => {
                    let src = self.as_place(cc, code, out, ty)?;
                    let unsized_ = matches!(self.node(ty)?, PNode::Str | PNode::Slice(_));
                    let thin = match self.node(next)? {
                        PNode::Ref(_, p) | PNode::Ptr(_, p) => !layout::is_unsized(self.world, self.ground, p, self.alloc)?,
                        _ => false,
                    };
                    let meta = if thin {
                        Meta::Drop
                    } else if unsized_ {
                        Meta::Need
                    } else {
                        Meta::Keep
                    };
                    let dst = self.reg(cc, Kind::V);
                    self.emit(code, cc, Op::Borrow { dst, src, meta });
                    dst
                }
                AdjustKind::ReifyFnPointer => self.reify(code, cc, ty)?,
                // The pointer is the closure's `FnOnce::call_once` at `[C, (A..)]`, the one
                // instance blaze-build's `closure_fn_addr` takes for the same coercion, so the
                // record's fn word names the function the build makes. The arguments' tuple is
                // the target pointer's inputs: the checker unified that pointer with the
                // closure's signature (rules/coerce.rs), so no closure facts are read. The
                // trait method is a trait's, never resolved: a closure has no impl to select.
                AdjustKind::ClosureFnPointer => {
                    let PNode::Closure { owner, .. } = self.node(ty)? else {
                        return Err(malformed("a closure's fn pointer made of a value that is not a closure"));
                    };
                    let PNode::FnPtr(sig, _) = self.node(next)? else { return Err(malformed("a closure coerced to a type that is not a fn pointer")) };
                    let call_once = match self.world.fn_trait_method(blaze_build::FnTrait::FnOnce) {
                        FnTraitMethod::Found(item) => item,
                        FnTraitMethod::Absent => return Err(unsupported(
                            "a closure as a fn pointer in a world that names no `FnOnce::call_once` (ConstWorld::fn_trait_method)",
                        )),
                        FnTraitMethod::Pending { trait_ } => return Err(Stop::Refused(Refusal::Missing {
                            what: "the trait members for `FnOnce::call_once` (ConstWorld::fn_trait_method)",
                            item: trait_,
                        })),
                    };
                    let mut inputs: Vec<PTy, Alloc<'a>> = Vec::new_in(self.alloc);
                    let all = self.ground.view().list(sig).map_err(|_| malformed("a list past the ground table"))?;
                    let (_, ins) = all.split_last().ok_or(malformed("a fn pointer type without its output"))?;
                    inputs.extend_from_slice(ins);
                    let list = self.ground.intern_list(&inputs).map_err(|_| malformed("the ground table is full"))?;
                    let tuple = self.ground.intern(PNode::Tuple(list)).map_err(|_| malformed("the ground table is full"))?;
                    // Keyed over the owner's code identity by the one form the run keys a
                    // closure's method instance by (`blaze_build::closure_method_key_args`), so
                    // the image's fill finds the slot the run built.
                    let own = self.ground.intern_list(&[ty, tuple]).map_err(|_| malformed("the ground table is full"))?;
                    let method = blaze_build::Instance { item: call_once, args: own };
                    let args = blaze_build::closure_method_key_args(method, self.ground, self.alloc)
                        .map_err(|_| malformed("a closure's `call_once` instance past the ground table"))?;
                    let key = self.instance(owner, args)?;
                    // The record names the method instance the build calls, under the key the
                    // run's slot of it has.
                    code.fns.push(ConstFn { key, instance: method });
                    let dst = self.reg(cc, Kind::V);
                    self.emit(code, cc, Op::FnPtr { dst, k: code.fns.len() as u32 - 1 });
                    dst
                }
                AdjustKind::Unsize => {
                    let src = self.as_value(cc, code, out, ty)?;
                    let n = match self.node(ty)? {
                        PNode::Ref(_, a) | PNode::Ptr(_, a) => match self.node(a)? {
                            PNode::Array(_, n) => self.len(n)?,
                            _ => return Err(unsupported("an unsizing coercion other than an array to a slice")),
                        },
                        _ => return Err(unsupported("an unsizing coercion of a value that is not a pointer")),
                    };
                    let dst = self.reg(cc, Kind::V);
                    self.emit_unsize(code, cc, dst, src, n);
                    dst
                }
                AdjustKind::UnionInject { variant } => {
                    let PNode::Union(ul) = self.node(next)? else {
                        return Err(malformed("a union injection to a type that is not a structural union"));
                    };
                    if variant >= ul.len {
                        return Err(malformed("a union injection variant past its union"));
                    }
                    self.build(cc, code, next, variant, &[(0, out, ty)], false)?
                }
                AdjustKind::UnionWiden => {
                    if ty == next {
                        out
                    } else {
                        let src_pl = self.as_place(cc, code, out, ty)?;
                        self.remap_union(cc, code, src_pl, ty, next, None)?
                    }
                }
                AdjustKind::RecordProject => {
                    if ty == next {
                        out
                    } else {
                        let PNode::Record(src_schema, _) = self.node(ty)? else {
                            return Err(malformed("a record projection from a type that is not a structural record"));
                        };
                        let PNode::Record(dst_schema, _) = self.node(next)? else {
                            return Err(malformed("a record projection to a type that is not a structural record"));
                        };
                        let dst_ty = self.ty(code, next)?;
                        let dst_is_zst = code
                            .tys
                            .get(usize::try_from(dst_ty).map_err(|_| malformed("a type index past `usize`"))?)
                            .is_some_and(|info| info.repr == Repr::Zst);
                        if dst_is_zst {
                            let dst = self.reg(cc, Kind::V);
                            self.emit(code, cc, Op::ImmZst { dst });
                            dst
                        } else {
                            let src_pl = self.as_place(cc, code, out, ty)?;
                            let src_names = self.record_names_c(cc, src_schema)?;
                            let dst_names = self.record_names_c(cc, dst_schema)?;
                            let mut parts: Vec<(u32, Reg, PTy), Alloc<'a>> = Vec::with_capacity_in(dst_names.len(), self.alloc);
                            for (di_idx, &nm) in dst_names.iter().enumerate() {
                                let si_idx = src_names
                                    .iter()
                                    .position(|&sn| sn == nm)
                                    .ok_or(malformed("a projected record missing a target field in source"))?;
                                let di = u32::try_from(di_idx).map_err(|_| malformed("a record field index past `u32`"))?;
                                let si = u32::try_from(si_idx).map_err(|_| malformed("a record field index past `u32`"))?;
                                let ft = self.field_ty(ty, 0, si)?;
                                let soff = self.field_offset(ty, 0, si)?;
                                let fpl = self.reg(cc, Kind::P);
                                self.emit_field(code, cc, fpl, src_pl, soff, false);
                                parts.push((di, fpl, ft));
                            }
                            self.build(cc, code, next, 0, &parts, false)?
                        }
                    }
                }
            };
            ty = next;
        }
        let out = match mode {
            Mode::Value => self.as_value(cc, code, out, ty)?,
            Mode::Place => self.as_place(cc, code, out, ty)?,
        };
        cc.rs.push(out);
        Ok(())
    }

    /// A call: a `const fn`, a constructor, a panic entry or an intrinsic.
    fn call_c<A: Allocator + Clone>(&mut self, cc: &mut Cc<'w, '_>, code: &mut Code<A>, e: ExprId) -> R<'a, ()> {
        let typed = cc.typed;
        let body = typed.body;
        let (arg_exprs, receiver) = match self.expr_c(cc, e)? {
            Expr::Call { args, .. } => (args, None),
            Expr::MethodCall { receiver, args, .. } => (args, Some(receiver)),
            _ => return Err(malformed("a call task")),
        };
        let mut exprs: Vec<ExprId, Alloc<'a>> = Vec::new_in(self.alloc);
        exprs.extend(receiver);
        exprs.extend_from_slice(body.expr_lists.get(arg_exprs.range()).ok_or(malformed("a list past `expr_lists`"))?);
        let at = cc.rs.len().checked_sub(exprs.len()).ok_or(malformed("the value stack ran out"))?;
        let mut vals: Vec<(Reg, PTy), Alloc<'a>> = Vec::new_in(self.alloc);
        for (k, &x) in exprs.iter().enumerate() {
            let t = self.final_ty_c(cc, x)?;
            vals.push((cc.rs[at + k], t));
        }
        cc.rs.truncate(at);
        let row = rows(typed.callees.0, e).next();
        let (item, args) = match row {
            Some(i) => {
                let src = typed.types.list(typed.callees.2[i]).map_err(|_| malformed("a list past the body's types"))?;
                let mut out: Vec<PTy, Alloc<'a>> = Vec::new_in(self.alloc);
                for &t in src {
                    out.push(self.cg(cc, t)?);
                }
                let l = self.ground.intern_list(&out).map_err(|_| malformed("the ground table is full"))?;
                (typed.callees.1[i], l)
            }
            None => {
                let Expr::Call { callee, .. } = self.expr_c(cc, e)? else { return Err(malformed("a method call with no callee")) };
                let ct = self.final_ty_c(cc, callee)?;
                match self.node(ct)? {
                    PNode::FnDef(k, l) => (k, l),
                    // Only a `const` closure may be called in a constant (E0015 otherwise; the
                    // unstable `const_closures`): its body is part of its owner's, compiled
                    // with its captures as the environment's fields, which is not built.
                    PNode::Closure { .. } => return Err(unsupported("a call of a `const` closure in a constant")),
                    _ => return Err(unsupported("a call through a fn pointer in a constant")),
                }
            }
        };
        let (item, args) = self.resolve(item, args)?;
        let ret = self.ety_c(cc, e)?;
        let dst = match self.world.fn_kind(item) {
            FnKind::Const => {
                let regs = self.arg_regs(cc, code, &vals)?;
                code.calls.push(Call { item, args, regs, link: Cell::new(u32::MAX), intrinsic: None, ret });
                let dst = self.reg(cc, Kind::V);
                self.emit(code, cc, Op::Call { dst, k: code.calls.len() as u32 - 1 });
                dst
            }
            FnKind::NotConst => return Err(Stop::Refused(Refusal::NotConstFn { item })),
            FnKind::UnknownIntrinsic => return Err(Stop::Refused(Refusal::UnknownIntrinsic { item })),
            FnKind::Missing(fact) => return Err(Stop::Refused(Refusal::FnFact { item, fact })),
            FnKind::Ctor { variant, .. } => {
                let mut fields: Vec<(u32, Reg, PTy), Alloc<'a>> = Vec::new_in(self.alloc);
                for (k, &(r, t)) in vals.iter().enumerate() {
                    fields.push((k as u32, r, t));
                }
                self.build(cc, code, ret, variant, &fields, false)?
            }
            FnKind::Panic => {
                let regs = self.arg_regs(cc, code, &vals)?;
                code.calls.push(Call { item, args, regs, link: Cell::new(u32::MAX), intrinsic: None, ret });
                self.emit(code, cc, Op::Panic { k: code.calls.len() as u32 - 1 });
                self.reg(cc, Kind::V)
            }
            FnKind::Intrinsic(Intrinsic::ConstEvalSelect) => {
                // `const_eval_select(args, const_fn, rt_fn)`: the const fn, called with the
                // tuple's fields.
                let (&(tuple, tt), &f) = (vals.first().ok_or(malformed("`const_eval_select` arguments"))?, exprs.get(1).ok_or(malformed("`const_eval_select` arguments"))?);
                let ft = self.final_ty_c(cc, f)?;
                let PNode::FnDef(k, l) = self.node(ft)? else { return Err(malformed("`const_eval_select` of a value that is not a fn item")) };
                let PNode::Tuple(parts) = self.node(tt)? else { return Err(malformed("`const_eval_select` arguments that are not a tuple")) };
                let place = self.as_place(cc, code, tuple, tt)?;
                let mut inner: Vec<(Reg, PTy), Alloc<'a>> = Vec::new_in(self.alloc);
                for k in 0..parts.len {
                    let ft = self.field_ty(tt, 0, k)?;
                    let off = self.field_offset(tt, 0, k)?;
                    let tail = layout::is_unsized(self.world, self.ground, ft, self.alloc)?;
                    let fp = self.reg(cc, Kind::P);
                    self.emit_field(code, cc, fp, place, off, tail);
                    inner.push((fp, ft));
                }
                let (k, l) = self.resolve(k, l)?;
                let regs = self.arg_regs(cc, code, &inner)?;
                code.calls.push(Call { item: k, args: l, regs, link: Cell::new(u32::MAX), intrinsic: None, ret });
                let dst = self.reg(cc, Kind::V);
                self.emit(code, cc, Op::Call { dst, k: code.calls.len() as u32 - 1 });
                dst
            }
            FnKind::Intrinsic(Intrinsic::DiscriminantValue) => {
                // `discriminant_value(&v)`: the referent's discriminant, read through its
                // enum's facts as `as` reads it.
                let &(v, t) = vals.first().ok_or(malformed("`discriminant_value` argument"))?;
                let PNode::Ref(_, et) = self.node(t)? else { return Err(malformed("`discriminant_value` of a value that is not a reference")) };
                let (bits, _) = int_info(self.node(ret)?).ok_or(malformed("a discriminant that is not an integer"))?;
                let e = self.enum_facts(code, et)?;
                let ty = self.ty(code, et)?;
                let place = self.reg(cc, Kind::P);
                self.emit(code, cc, Op::Deref { dst: place, src: v });
                let dst = self.reg(cc, Kind::V);
                let k = code.discr_ops.len() as u32;
                code.discr_ops.push(DiscrOp { e, ty, bits });
                self.emit(code, cc, Op::Discr { dst, src: place, k });
                dst
            }
            FnKind::Intrinsic(i) => {
                let regs = self.arg_regs(cc, code, &vals)?;
                code.calls.push(Call { item, args, regs, link: Cell::new(u32::MAX), intrinsic: Some(i), ret });
                let dst = self.reg(cc, Kind::V);
                self.emit(code, cc, Op::Intrinsic { dst, k: code.calls.len() as u32 - 1 });
                dst
            }
        };
        cc.rs.push(dst);
        Ok(())
    }

    // ---- Patterns ----

    /// Compiles a match of `top` against the value at `place` of type `ty`, binding its
    /// bindings; a mismatch jumps to fixup list `fail`. An explicit work list; an or-pattern
    /// tries its alternatives in order, the first that matches binds.
    fn pattern<A: Allocator + Clone>(&mut self, cc: &mut Cc<'w, '_>, code: &mut Code<A>, top: PatId, place: Reg, ty: PTy, fail: u32) -> R<'a, ()> {
        let typed = cc.typed;
        let body = typed.body;
        let mut work: Vec<P, Alloc<'a>> = Vec::new_in(self.alloc);
        work.push(P::Pat { p: top, place, ty, fail });
        while let Some(w) = work.pop() {
            let (p, pl, t, fail) = match w {
                P::Pat { p, place, ty, fail } => (p, place, ty, fail),
                P::Alt { alts, i, place, ty, fail, join } => {
                    let alt = *body.pat_lists.get((alts.start + i) as usize).ok_or(malformed("an or-pattern's alternative past its column"))?;
                    if i + 1 < alts.len {
                        let next = self.list(cc);
                        work.push(P::AltEnd { alts, i, place, ty, fail, join, next });
                        work.push(P::Pat { p: alt, place, ty, fail: next });
                    } else {
                        work.push(P::Join { join });
                        work.push(P::Pat { p: alt, place, ty, fail });
                    }
                    continue;
                }
                P::AltEnd { alts, i, place, ty, fail, join, next } => {
                    let j = self.jump(code, cc);
                    self.to_list(cc, join, j);
                    self.patch(code, cc, next);
                    work.push(P::Alt { alts, i: i + 1, place, ty, fail, join });
                    continue;
                }
                P::Join { join } => {
                    self.patch(code, cc, join);
                    continue;
                }
            };
            let pat = body.pats.get(p.ix()).copied().ok_or(malformed("a pattern past the body"))?;
            let node = self.node(t)?;
            // Default binding modes: a non-reference pattern against a reference matches
            // through it.
            if let PNode::Ref(_, inner) = node {
                let through = match pat {
                    Pat::Tuple { .. } | Pat::TupleStruct { .. } | Pat::Struct { .. } | Pat::Record { .. } | Pat::Slice { .. } | Pat::Range { .. } => true,
                    Pat::Bind { .. } => self.union_pat_c(cc, p).is_some(),
                    Pat::Lit(l) | Pat::NegLit(l) => !matches!(l, Lit::Str | Lit::ByteStr(_) | Lit::CStr),
                    Pat::Path(_) => matches!(self.pat_def_c(cc, p), Some(Def::Ctor { .. })),
                    _ => false,
                };
                if through {
                    let v = self.as_value(cc, code, pl, t)?;
                    let q = self.reg(cc, Kind::P);
                    self.emit(code, cc, Op::Deref { dst: q, src: v });
                    work.push(P::Pat { p, place: q, ty: inner, fail });
                    continue;
                }
            }
            match pat {
                Pat::Missing => return Err(malformed("a missing pattern")),
                Pat::Wild => {}
                Pat::Bind { ann, sub, .. } => {
                    let lt = self.cg(cc, *typed.local_ty.get(p.ix()).ok_or(malformed("a binding with no type"))?)?;
                    match self.union_pat_c(cc, p) {
                        None => {
                            let by_ref = ann.by_ref.is_some() || (lt != t && matches!(self.node(lt)?, PNode::Ref(_, i) if i == t));
                            self.bind_at(cc, code, p, pl, lt, by_ref)?;
                            if let Some(s) = sub {
                                work.push(P::Pat { p: s, place: pl, ty: t, fail });
                            }
                        }
                        Some(UnionPat::Variant(v)) => {
                            let PNode::Union(ul) = node else {
                                return Err(malformed("a union variant pattern over a type that is not a structural union"));
                            };
                            if v >= ul.len {
                                return Err(malformed("a union variant pattern past its union"));
                            }
                            if ul.len > 1 {
                                self.if_variant(cc, code, pl, t, v, fail)?;
                            }
                            let ft = self.field_ty(t, v, 0)?;
                            let off = self.field_offset(t, v, 0)?;
                            let fpl = self.reg(cc, Kind::P);
                            self.emit_field(code, cc, fpl, pl, off, false);
                            let by_ref = ann.by_ref.is_some() || (lt != ft && matches!(self.node(lt)?, PNode::Ref(_, i) if i == ft));
                            self.bind_at(cc, code, p, fpl, lt, by_ref)?;
                            if let Some(s) = sub {
                                work.push(P::Pat { p: s, place: fpl, ty: ft, fail });
                            }
                        }
                        Some(UnionPat::Residual) => {
                            let (by_ref, dst_ty) = match self.node(lt)? {
                                PNode::Ref(_, inner) if matches!(self.node(inner)?, PNode::Union(_)) => (true, inner),
                                PNode::Union(_) => (ann.by_ref.is_some(), lt),
                                _ => return Err(malformed("a union residual binding whose type is not a structural union")),
                            };
                            let narrowed = self.remap_union(cc, code, pl, t, dst_ty, Some(fail))?;
                            let npl = self.as_place(cc, code, narrowed, dst_ty)?;
                            self.bind_at(cc, code, p, npl, lt, by_ref)?;
                            if let Some(s) = sub {
                                work.push(P::Pat { p: s, place: npl, ty: dst_ty, fail });
                            }
                        }
                    }
                }
                Pat::Ref { inner, .. } => {
                    let (PNode::Ref(_, it) | PNode::Ptr(_, it)) = node else { return Err(malformed("a reference pattern of a type that is not a reference")) };
                    let v = self.as_value(cc, code, pl, t)?;
                    let q = self.reg(cc, Kind::P);
                    self.emit(code, cc, Op::Deref { dst: q, src: v });
                    work.push(P::Pat { p: inner, place: q, ty: it, fail });
                }
                Pat::Lit(_) | Pat::NegLit(_) => {
                    let want = self.pat_value(cc, code, p, t)?;
                    let have = self.as_value(cc, code, pl, t)?;
                    let k = code.if_sames.len() as u32;
                    code.if_sames.push(IfSameOp { a: want, b: have, ty: t, konst: false });
                    let j = self.emit(code, cc, Op::IfSame { k, fail: u32::MAX });
                    self.to_list(cc, fail, j);
                }
                Pat::Path(_) => match self.pat_def_c(cc, p) {
                    Some(Def::Ctor { variant, .. }) => self.if_variant(cc, code, pl, t, variant, fail)?,
                    Some(Def::Const(_)) => {
                        let want = self.pat_value(cc, code, p, t)?;
                        let have = self.as_value(cc, code, pl, t)?;
                        let k = code.if_sames.len() as u32;
                        code.if_sames.push(IfSameOp { a: want, b: have, ty: t, konst: true });
                        let j = self.emit(code, cc, Op::IfSame { k, fail: u32::MAX });
                        self.to_list(cc, fail, j);
                    }
                    _ => return Err(malformed("a path pattern with no definition")),
                },
                // `lo..=hi`, `lo..hi`, `lo..`, `..=hi`: the value compared with each bound
                // written, a jump to `fail` on the first it is outside.
                Pat::Range { lo, hi, inclusive } => {
                    let have = self.as_value(cc, code, pl, t)?;
                    let n = num(node);
                    let upper = if inclusive { BinOp::Le } else { BinOp::Lt };
                    for (bound, op) in [(lo, BinOp::Ge), (hi, upper)] {
                        let Some(b) = bound else { continue };
                        let want = self.pat_value(cc, code, b, t)?;
                        let within = self.reg(cc, Kind::V);
                        self.emit(code, cc, Op::Binary { dst: within, l: have, r: want, op, num: n, rnum: n });
                        let j = self.emit(code, cc, Op::Branch { cond: within, when: false, to: u32::MAX });
                        self.to_list(cc, fail, j);
                    }
                }
                Pat::TupleStruct { elems, rest, .. } => {
                    let Some(Def::Ctor { variant, .. }) = self.pat_def_c(cc, p) else { return Err(malformed("a tuple-struct pattern with no constructor")) };
                    self.if_variant(cc, code, pl, t, variant, fail)?;
                    let pats = body.pat_lists.get(elems.range()).ok_or(malformed("patterns past their column"))?;
                    let count = self.variant_fields(t, variant)?;
                    self.row(cc, code, &mut work, pats, rest, count, pl, t, variant, fail)?;
                }
                Pat::Tuple { elems, rest } => {
                    let PNode::Tuple(l) = node else { return Err(malformed("a tuple pattern of a type that is not a tuple")) };
                    let pats = body.pat_lists.get(elems.range()).ok_or(malformed("patterns past their column"))?;
                    self.row(cc, code, &mut work, pats, rest, l.len, pl, t, 0, fail)?;
                }
                Pat::Struct { fields, .. } => {
                    let variant = match self.pat_def_c(cc, p) {
                        Some(Def::Ctor { variant, .. }) => variant,
                        _ => 0,
                    };
                    self.if_variant(cc, code, pl, t, variant, fail)?;
                    let PNode::Adt(key, _) = node else { return Err(malformed("a struct pattern of a type that is not an ADT")) };
                    let adt = self.world.adt(key).ok_or(Stop::Refused(Refusal::Missing { what: "ADT definition", item: key }))?;
                    let v = adt.variants.get(variant as usize).ok_or(malformed("a variant past its ADT"))?;
                    let names = adt.fields.get(v.fields.range()).ok_or(malformed("fields past their ADT"))?;
                    let fps = body.field_pats.get(fields.range()).ok_or(malformed("field patterns past their column"))?;
                    for fp in fps.iter().rev() {
                        let idx = names.iter().position(|f| f.name == fp.name).ok_or(malformed("a field name its variant lacks"))? as u32;
                        let ft = self.field_ty(t, variant, idx)?;
                        let off = self.field_offset(t, variant, idx)?;
                        let tail = layout::is_unsized(self.world, self.ground, ft, self.alloc)?;
                        let fpl = self.reg(cc, Kind::P);
                        self.emit_field(code, cc, fpl, pl, off, tail);
                        work.push(P::Pat { p: fp.pat, place: fpl, ty: ft, fail });
                    }
                }
                Pat::Record { fields, .. } => {
                    let PNode::Record(schema, _) = node else {
                        return Err(malformed("a record pattern of a type that is not a structural record"));
                    };
                    let fps = body.field_pats.get(fields.range()).ok_or(malformed("field patterns past their column"))?;
                    for fp in fps.iter().rev() {
                        let idx = self.record_field_index_c(cc, schema, fp.name)?;
                        let ft = self.field_ty(t, 0, idx)?;
                        let off = self.field_offset(t, 0, idx)?;
                        let tail = layout::is_unsized(self.world, self.ground, ft, self.alloc)?;
                        let fpl = self.reg(cc, Kind::P);
                        self.emit_field(code, cc, fpl, pl, off, tail);
                        work.push(P::Pat { p: fp.pat, place: fpl, ty: ft, fail });
                    }
                }
                // The Reference, "Slice patterns": the elements before a `..` at their index,
                // those after it counted from the end, and the `..` itself (a wildcard, or the
                // pattern a `rest @ ..` binding is over) the part between, of the checker's type
                // for it: `[T]` over a slice, `[T; n - k]` over an array of `n` with `k` other
                // elements.
                Pat::Slice { elems, rest } => {
                    let (el, len) = match node {
                        PNode::Array(el, n) => (el, Some(self.len(n)?)),
                        PNode::Slice(el) => (el, None),
                        _ => return Err(malformed("a slice pattern of a type that is not an array or slice")),
                    };
                    let count = u64::from(elems.len);
                    let rest = rest.map(u64::from);
                    if rest.is_some_and(|r| r >= count) {
                        return Err(malformed("a slice pattern's `..` past its elements"));
                    }
                    let fixed = count - u64::from(rest.is_some());
                    match len {
                        Some(n) if n < fixed || (rest.is_none() && n != fixed) => {
                            let j = self.jump(code, cc);
                            self.to_list(cc, fail, j);
                            continue;
                        }
                        Some(_) => {}
                        None => {
                            let k = code.if_lens.len() as u32;
                            code.if_lens.push((fixed, rest.is_some()));
                            let j = self.emit(code, cc, Op::IfLen { place: pl, k, fail: u32::MAX });
                            self.to_list(cc, fail, j);
                        }
                    }
                    let (size, _) = self.size_align(el)?;
                    let pats = body.pat_lists.get(elems.range()).ok_or(malformed("patterns past their column"))?;
                    for (k, &sp) in pats.iter().enumerate().rev() {
                        let k = k as u64;
                        let q = self.reg(cc, Kind::P);
                        let ty = match rest {
                            Some(r) if k == r => {
                                let back = count - 1 - r;
                                match len {
                                    Some(_) => { self.emit_field(code, cc, q, pl, size * k, false); }
                                    None => {
                                        let sk = code.slice_ops.len() as u32;
                                        code.slice_ops.push(SliceOp { size, from: k, back });
                                        self.emit(code, cc, Op::Subslice { dst: q, src: pl, k: sk });
                                    }
                                };
                                self.cg(cc, *typed.pat_ty.get(sp.ix()).ok_or(malformed("a pattern with no type"))?)?
                            }
                            Some(r) if k > r => {
                                let back = count - k;
                                match len {
                                    Some(n) => { self.emit_field(code, cc, q, pl, size * (n - back), false); }
                                    None => {
                                        let sk = code.slice_ops.len() as u32;
                                        code.slice_ops.push(SliceOp { size, from: 0, back });
                                        self.emit(code, cc, Op::FromEnd { dst: q, src: pl, k: sk });
                                    }
                                };
                                el
                            }
                            _ => {
                                self.emit_field(code, cc, q, pl, size * k, false);
                                el
                            }
                        };
                        work.push(P::Pat { p: sp, place: q, ty, fail });
                    }
                }
                Pat::Or { alts } => {
                    if alts.len == 0 {
                        return Err(malformed("an or-pattern with no alternatives"));
                    }
                    let join = self.list(cc);
                    work.push(P::Alt { alts, i: 0, place: pl, ty: t, fail, join });
                }
            }
        }
        Ok(())
    }

    /// The value a literal, negated literal or constant pattern `p` of type `t` stands for: a
    /// literal or constant pattern's, and each bound of a range pattern.
    fn pat_value<A: Allocator + Clone>(&mut self, cc: &mut Cc<'w, '_>, code: &mut Code<A>, p: PatId, t: PTy) -> R<'a, Reg> {
        let typed = cc.typed;
        match typed.body.pats.get(p.ix()).copied().ok_or(malformed("a pattern past the body"))? {
            pat @ (Pat::Lit(l) | Pat::NegLit(l)) => {
                let neg = matches!(pat, Pat::NegLit(_));
                let src = typed.lits.pat(p).ok_or(malformed("a literal pattern with no written text"))?;
                // `- 5` is one pattern: the literal is after the minus and its whitespace.
                let src = if neg { src.trim_start().strip_prefix('-').ok_or(malformed("a negative literal pattern with no minus"))?.trim_start() } else { src };
                self.lit_op(cc, code, l, src, t, neg)
            }
            Pat::Path(_) => {
                let Some(Def::Const(k)) = self.pat_def_c(cc, p) else { return Err(malformed("a pattern's value that is not a constant")) };
                let key = RecordKey::of(self.instance(k, List::EMPTY)?);
                code.consts.push((k, List::EMPTY, key));
                let cp = self.reg(cc, Kind::P);
                self.emit(code, cc, Op::Const { dst: cp, k: code.consts.len() as u32 - 1 });
                self.as_value(cc, code, cp, t)
            }
            _ => Err(malformed("a range pattern's bound that is not a literal or a constant")),
        }
    }

    /// A jump to `fail` unless the value at `place` is variant `variant` (a type with one
    /// variant and no tag is variant 0).
    fn if_variant<A: Allocator + Clone>(&mut self, cc: &mut Cc<'w, '_>, code: &mut Code<A>, place: Reg, ty: PTy, variant: u32, fail: u32) -> R<'a, ()> {
        let e = self.enum_facts(code, ty)?;
        let facts = code.enums[e as usize];
        let j = match facts.tag_at() {
            Some(_) => {
                let k = code.if_variants.len() as u32;
                code.if_variants.push((e, variant));
                self.emit(code, cc, Op::IfVariant { place, k, fail: u32::MAX })
            }
            // No tag: the one variant the layout names, known now.
            None if facts.variant(0, &code.discrs)? == variant => return Ok(()),
            None => self.jump(code, cc),
        };
        self.to_list(cc, fail, j);
        Ok(())
    }

    /// The index in `code.enums` of type `t`'s facts, found and checked the first time this
    /// code asks ([`EnumFacts`]); the explicit discriminants' records it reads join
    /// `code.reads`.
    fn enum_facts<A: Allocator + Clone>(&mut self, code: &mut Code<A>, t: PTy) -> R<'a, u32> {
        if let Some(k) = code.enums.iter().position(|f| f.ty == t) {
            return Ok(k as u32);
        }
        let tag = {
            let (mut cx, l) = self.cx();
            l.tag(t, &mut cx)?
        };
        let at = code.discrs.len() as u32;
        let n = match self.node(t)? {
            PNode::Adt(adt, _) => {
                let def = self.world.adt(adt).ok_or(Stop::Refused(Refusal::Missing { what: "ADT definition", item: adt }))?;
                if def.kind == AdtKind::Enum {
                    let n = def.variants.len() as u32;
                    let mut reads = Vec::new_in(self.alloc);
                    super::discriminants_planted(self.world, adt, n, &mut code.discrs, &mut reads, self.alloc, self.plant)?;
                    code.reads.extend_from_slice(&reads);
                    n
                } else {
                    0
                }
            }
            PNode::Union(l) => {
                for v in 0..l.len {
                    code.discrs.push(i128::from(v));
                }
                l.len
            }
            _ => 0,
        };
        code.enums.push(EnumFacts { ty: t, tag, at, n });
        Ok(code.enums.len() as u32 - 1)
    }

    /// The patterns of a row (a tuple's or a variant's fields) with an optional `..` at `rest`.
    #[allow(clippy::too_many_arguments)]
    fn row<A: Allocator + Clone>(
        &mut self,
        cc: &mut Cc<'w, '_>,
        code: &mut Code<A>,
        work: &mut Vec<P, Alloc<'a>>,
        pats: &[PatId],
        rest: Option<u32>,
        count: u32,
        pl: Reg,
        t: PTy,
        variant: u32,
        fail: u32,
    ) -> R<'a, ()> {
        let n = pats.len() as u32;
        for (k, &sp) in pats.iter().enumerate().rev() {
            let k = k as u32;
            let field = match rest {
                Some(r) if k >= r => count - (n - k),
                _ => k,
            };
            let ft = self.field_ty(t, variant, field)?;
            let off = self.field_offset(t, variant, field)?;
            let tail = layout::is_unsized(self.world, self.ground, ft, self.alloc)?;
            let q = self.reg(cc, Kind::P);
            self.emit_field(code, cc, q, pl, off, tail);
            work.push(P::Pat { p: sp, place: q, ty: ft, fail });
        }
        Ok(())
    }
}

/// Points a jump at `to`.
fn set_target(op: &mut Op, to: u32) {
    match op {
        Op::Jump { to: t }
        | Op::Branch { to: t, .. }
        | Op::IfVariant { fail: t, .. }
        | Op::IfSame { fail: t, .. }
        | Op::IfLen { fail: t, .. } => *t = to,
        _ => {}
    }
}

/// How many results a task takes from the stack, and whether it leaves one.
fn shape<'a>(task: C, typed: &Typed<'_>) -> R<'a, (u32, bool)> {
    let body = typed.body;
    let expr = |e: ExprId| body.exprs.get(e.ix()).copied().ok_or(malformed("an expression past the body"));
    Ok(match task {
        C::Eval(..) | C::Block { .. } | C::LoopEnd(_) | C::Arm { .. } => (0, true),
        C::Adjust(..)
        | C::Unary(_)
        | C::Cast(_)
        | C::Ref(_)
        | C::RawRef(_)
        | C::Deref(_)
        | C::Field(_)
        | C::Repeat(_)
        | C::IfCond(_)
        | C::IfThen { .. }
        | C::IfElse { .. }
        | C::Logic(_)
        | C::LogicEnd { .. }
        | C::BlockEnd(_)
        | C::Break(_)
        | C::Return(_)
        | C::Match(_)
        | C::Guard { .. }
        | C::ArmBody { .. } => (1, true),
        C::Binary(_) | C::Index(_) => (2, true),
        C::StmtEnd { .. } | C::Let { .. } | C::LetElse { .. } | C::LoopBody(_) => (1, false),
        C::CondLink { .. } => (0, false),
        C::CondLet { .. } | C::CondBool { .. } => (1, false),
        C::IfLetEnd { .. } => (1, true),
        C::Aggregate(e) => match expr(e)? {
            Expr::Tuple { elems } | Expr::Array { elems } => (elems.len, true),
            _ => (0, true),
        },
        C::Struct(e) => match expr(e)? {
            Expr::Struct { fields, base, .. } | Expr::Record { fields, base } => {
                (fields.len + u32::from(base.expr().is_some()), true)
            }
            _ => (0, true),
        },
        C::Call(e) => match expr(e)? {
            Expr::Call { args, .. } => (args.len, true),
            Expr::MethodCall { args, .. } => (args.len + 1, true),
            _ => (0, true),
        },
        // The place is on the stack unless the left side is a register local; the compile of
        // the task decides that, so both are counted from the expression.
        C::Assign(e) | C::AssignOp(e) => match expr(e)? {
            Expr::Assign { lhs, .. } | Expr::AssignOp { lhs, .. } => (if lhs_is_local(body, lhs) { 1 } else { 2 }, true),
            _ => (0, true),
        },
    })
}

/// Whether `e`, through parentheses, is a local (a register local's assignment takes no place).
fn lhs_is_local(body: blaze_typeck::BodyView<'_>, mut e: ExprId) -> bool {
    loop {
        match body.exprs.get(e.ix()) {
            Some(Expr::Paren(i)) => e = *i,
            Some(Expr::Local(_)) => return true,
            _ => return false,
        }
    }
}
