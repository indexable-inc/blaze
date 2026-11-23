//! The machine: one evaluation runs code (`code`), each body compiled once at its instance into
//! a flat list of operations over numbered registers, with a stack of frames and no recursion
//! on the input (consteval.md, "The machine"). Each operation run is one step of the bound;
//! each byte allocated counts against the memory bound; each call against the depth.

mod code;
mod late;
#[cfg(test)]
mod late_tests;

use core::alloc::Allocator;

use blaze_build::{InstanceKey, instance_key};
use blaze_ids::ContentId;
use blaze_own::{Alloc, Vec};
use blaze_typeck::{BinOp, FloatTy, IntTy, ItemKey, List, Mutbl, PNode, PTy, PTys, PortableTys, UnOp};

pub use code::Codes;
use code::{Code, CodeTable, EnumFacts, Meta, Num, Op, Reg, TyInfo, Valid};

// The rules on bits live in `arith` (rule 30); `code`, `late` and the tests reach them here.
pub(crate) use crate::arith::{Plant, int_binop, int_info, mask, sext};
use crate::arith::{int_bits, smax, smin, umax};
use crate::error::{At, ConstError, Op as Arith, PanicPart, Refusal};
use crate::layout::{self, IntRepr, Layout, LayoutCx};
use crate::mem::{AllocId, Memory, Origin, PTR, Pointer, Scalar};
use crate::record::{ConstFn, ConstRecord, DataAlloc, DataRecord, DataReloc, DataTarget, RecordKey};
use crate::soft::Format;
use crate::{ConstWorld, Intrinsic, Limits};

/// What a record or body an evaluation lacks is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NeedKind {
    /// Another constant's (or static's) value record: evaluate it first.
    Value,
    /// A body not checked yet: check it first.
    Body,
}

/// The one thing an evaluation stopped for: the item and its ground arguments (a list of the
/// caller's ground table).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Need {
    pub kind: NeedKind,
    pub item: ItemKey,
    pub args: List,
}

/// A result, or what it needs first.
#[derive(Debug, PartialEq, Eq)]
pub enum Answer<T> {
    Done(T),
    Need(Need),
}

/// What [`value_of`] gives.
pub type Outcome<'a> = Answer<ConstRecord<Alloc<'a>>>;

/// How an evaluation stopped early.
enum Stop<'a> {
    Need(Need),
    Refused(Refusal<'a>),
}

impl<'a> From<Refusal<'a>> for Stop<'a> {
    fn from(r: Refusal<'a>) -> Stop<'a> {
        Stop::Refused(r)
    }
}

/// A layout that reads a discriminant not made yet: the discriminant's value is the need.
impl<'a> From<layout::Stop> for Stop<'a> {
    fn from(s: layout::Stop) -> Stop<'a> {
        match s {
            layout::Stop::Refused(r) => Stop::Refused(r),
            layout::Stop::Need(item) => Stop::Need(Need { kind: NeedKind::Value, item, args: List::EMPTY }),
        }
    }
}

type R<'a, T> = Result<T, Stop<'a>>;

// Inlined: `ok_or(malformed(..))` on the step loop's paths then builds nothing (a call each
// was 3% of LVL's samples).
#[inline(always)]
const fn malformed<'a>(what: &'static str) -> Stop<'a> {
    Stop::Refused(Refusal::Malformed { what })
}

#[inline(always)]
const fn unsupported<'a>(what: &'static str) -> Stop<'a> {
    Stop::Refused(Refusal::Unsupported { what })
}

/// The value of `item` (a const, a static, an inline const, an explicit discriminant) at
/// ground arguments `args` (a list of `ground`), as its record. The builder's seam
/// (consteval.md, "Seams"): one call per constant used, its record placed as data. `codes` is
/// the unit's compiled bodies: it lives as long as `ground` and `layout`, whose ids it holds.
#[allow(clippy::too_many_arguments)]
pub fn value_of<'a, W: ConstWorld, L: Layout, A: Allocator + Clone>(
    world: &W,
    layout: &mut L,
    codes: &mut Codes<A>,
    item: ItemKey,
    args: List,
    ground: &mut PortableTys<'_>,
    limits: Limits,
    alloc: Alloc<'a>,
) -> Result<Outcome<'a>, ConstError<'a>> {
    codes.table.evaluation += 1;
    let Codes { table, work } = codes;
    let mut m = Machine::new(world, layout, ground, limits, alloc, work);
    let out = m.evaluate(table, item, args);
    let at = m.at;
    match out {
        Ok(record) => Ok(Answer::Done(record)),
        Err(Stop::Need(n)) => Ok(Answer::Need(n)),
        Err(Stop::Refused(refusal)) => Err(ConstError { constant: item, at, refusal }),
    }
}

/// An array length or a const generic argument of type `usize`: the value of the anonymous
/// const `item` (the length's expression) as a `u64`, and its record to keep. The checker's
/// seam: `World::const_value` answers from the record ([`len_of`]).
pub fn eval_len<'a, W: ConstWorld, L: Layout, A: Allocator + Clone>(
    world: &W,
    layout: &mut L,
    codes: &mut Codes<A>,
    item: ItemKey,
    ground: &mut PortableTys<'_>,
    limits: Limits,
    alloc: Alloc<'a>,
) -> Result<Answer<(u64, ConstRecord<Alloc<'a>>)>, ConstError<'a>> {
    Ok(match value_of(world, layout, codes, item, List::EMPTY, ground, limits, alloc)? {
        Answer::Done(record) => {
            let n = len_of(&record).map_err(|refusal| ConstError { constant: item, at: None, refusal })?;
            Answer::Done((n, record))
        }
        Answer::Need(n) => Answer::Need(n),
    })
}

/// A `usize` constant's value from its record.
pub fn len_of<'a, A: core::alloc::Allocator>(record: &ConstRecord<A>) -> Result<u64, Refusal<'a>> {
    let root = record.data.allocs.first().ok_or(Refusal::Malformed { what: "a record with no root" })?;
    if !root.relocs.is_empty() {
        return Err(Refusal::PointerAsInt);
    }
    let b: [u8; 8] = root.bytes.as_slice().try_into().map_err(|_| Refusal::Malformed { what: "a length that is not 8 bytes" })?;
    Ok(u64::from_le_bytes(b))
}

/// The discriminant of variant `variant` of enum `adt` (the Reference, "Enumerations":
/// "Discriminants"): its explicit value, or the previous variant's plus one, the first's 0,
/// in the enum's `repr` integer type (`isize` without one). The checker's seam.
pub fn eval_discriminant<'a, W: ConstWorld>(world: &W, adt: ItemKey, variant: u32, alloc: Alloc<'a>) -> Result<Answer<i128>, ConstError<'a>> {
    let mut reads = Vec::new_in(alloc);
    match discriminant(world, adt, variant, &mut reads, alloc) {
        Ok(v) => Ok(Answer::Done(v)),
        Err(Stop::Need(n)) => Ok(Answer::Need(n)),
        Err(Stop::Refused(refusal)) => Err(ConstError { constant: adt, at: None, refusal }),
    }
}

/// The discriminant of `variant` of `adt` (`eval_discriminant`'s rule), reading explicit
/// discriminants' records.
fn discriminant<'a, W: ConstWorld>(world: &W, adt: ItemKey, variant: u32, reads: &mut Vec<ContentId, Alloc<'a>>, alloc: Alloc<'a>) -> R<'a, i128> {
    discriminant_planted(world, adt, variant, reads, alloc, Plant::None)
}

fn discriminant_planted<'a, W: ConstWorld>(
    world: &W,
    adt: ItemKey,
    variant: u32,
    reads: &mut Vec<ContentId, Alloc<'a>>,
    alloc: Alloc<'a>,
    plant: Plant,
) -> R<'a, i128> {
    let variant = if plant == Plant::DiscriminantOffByOne { variant.saturating_sub(1) } else { variant };
    let repr = discriminant_repr(world, adt)?;
    let mut base: i128 = 0;
    let mut from = 0u32;
    let mut j = variant;
    loop {
        if let Some(d) = explicit_discriminant(world, adt, j, repr, reads, alloc)? {
            base = d;
            from = j;
            break;
        }
        if j == 0 {
            break;
        }
        j -= 1;
    }
    discriminant_fits(base + i128::from(variant - from), variant, repr)
}

/// Every discriminant of enum `adt` (`n` variants), in one pass from the first, pushed on
/// `out`: what `discriminant_planted` gives each variant, without walking back per variant.
pub(crate) fn discriminants_planted<'a, W: ConstWorld, T: Allocator>(
    world: &W,
    adt: ItemKey,
    n: u32,
    out: &mut Vec<i128, T>,
    reads: &mut Vec<ContentId, Alloc<'a>>,
    alloc: Alloc<'a>,
    plant: Plant,
) -> R<'a, ()> {
    let repr = discriminant_repr(world, adt)?;
    let first = out.len();
    let mut next: Option<i128> = Some(0);
    for v in 0..n {
        let d = match explicit_discriminant(world, adt, v, repr, reads, alloc)? {
            Some(d) => d,
            None => next.ok_or(Stop::Refused(Refusal::DiscriminantOverflow { variant: v }))?,
        };
        let d = discriminant_fits(d, v, repr)?;
        out.push(d);
        next = d.checked_add(1);
    }
    if plant == Plant::DiscriminantOffByOne {
        for v in (1..n as usize).rev() {
            out[first + v] = out[first + v - 1];
        }
    }
    Ok(())
}

/// The integer an enum's discriminants are in: its `repr`, or `isize`.
fn discriminant_repr<'a, W: ConstWorld>(world: &W, adt: ItemKey) -> R<'a, (u32, bool)> {
    // An unmade head is the head's need, as an unanswered `adt` (consts.rs makes it, D194).
    let attrs = world.repr(adt).map_err(|m| Stop::Refused(Refusal::Missing { what: "ADT definition", item: m.item }))?;
    let repr = match attrs.int {
        Some(IntRepr::Signed(i)) => PNode::Int(i),
        Some(IntRepr::Unsigned(u)) => PNode::Uint(u),
        None => PNode::Int(IntTy::Isize),
    };
    Ok(int_info(repr).ok_or(malformed("an enum repr that is not an integer"))?)
}

/// Variant `v`'s written discriminant `= n`; `None`: none written.
///
/// ONE home (rule 30): the value the layout writes and reads the variant's tag by
/// (`LayoutFacts::discriminant`, blaze-build layout.rs `enum_`), so the machine's variant of a
/// tag and the tag of a variant agree with every other reader of the enum. The class: the
/// machine read its own answer (`ConstWorld::explicit_discriminant`'s record), front2's world
/// answers that `None` for every variant (its discriminants are the head's, front2 heads.rs
/// `discriminants`), so core's `AlignmentEnum` (`_Align1Shl0 = 1 << 0`, ..) was numbered 0, 1,
/// 2, .. here while its layout's tags are 1, 2, 4, ..: a constant `Alignment` built by variant
/// held 0, and the program's first copy of it found no variant (std hello, 2026-11-17: "the
/// read of the enum's discriminant found no variant's discriminant" in
/// `Layout::from_size_align_unchecked`). The record a world names for the discriminant, when it
/// names one, joins `reads` (the record's key covers it, rule 21).
fn explicit_discriminant<'a, W: ConstWorld>(world: &W, adt: ItemKey, v: u32, (bits, signed): (u32, bool), reads: &mut Vec<ContentId, Alloc<'a>>, alloc: Alloc<'a>) -> R<'a, Option<i128>> {
    let written = crate::layout::LayoutFacts::discriminant(world, adt, v).map_err(|m| Stop::Need(Need { kind: NeedKind::Value, item: m.item, args: List::EMPTY }))?;
    let Some(d) = written else { return Ok(None) };
    if let Some(item) = world.explicit_discriminant(adt, v) {
        let key = RecordKey::of(instance_key(world.code_id(item), List::EMPTY, PTys::EMPTY, alloc).map_err(|_| malformed("an instance key"))?);
        reads.push(key.0);
    }
    let x = d as u128;
    Ok(Some(if signed { sext(x, bits) } else { mask(x, bits) as i128 }))
}

/// `d`, the discriminant of `variant`, when it fits its enum's integer.
fn discriminant_fits<'a>(d: i128, variant: u32, (bits, signed): (u32, bool)) -> R<'a, i128> {
    let fits = if signed { d <= smax(bits) } else { (d as u128) <= umax(bits) };
    if !fits {
        return Err(Stop::Refused(Refusal::DiscriminantOverflow { variant }));
    }
    Ok(d)
}

/// The sans-IO loop a driver runs to settle one constant and everything it needs, which names
/// a cycle (consteval.md, "Records and cycles"). The driver asks [`Settle::next`], evaluates it
/// with [`value_of`], and reports [`Settle::done`] or [`Settle::need`].
pub struct Settle<'a> {
    stack: Vec<Need, Alloc<'a>>,
    alloc: Alloc<'a>,
    depth: u32,
}

impl<'a> Settle<'a> {
    /// Settles `root`; needs nest at most `depth` deep (rule 18).
    pub fn new(root: Need, depth: u32, alloc: Alloc<'a>) -> Settle<'a> {
        let mut stack = Vec::new_in(alloc);
        stack.push(root);
        Settle { stack, alloc, depth }
    }

    /// The constant to evaluate now; `None` when the root is done.
    pub fn next(&self) -> Option<Need> {
        self.stack.last().copied()
    }

    /// The constant `next` gave is evaluated and its record stored.
    pub fn done(&mut self) {
        self.stack.pop();
    }

    /// The constant `next` gave needs `need` first. A value already being settled is a cycle:
    /// refused naming every constant on it.
    pub fn need(&mut self, need: Need) -> Result<(), ConstError<'a>> {
        if need.kind == NeedKind::Value {
            if let Some(first) = self.stack.iter().position(|n| n.kind == NeedKind::Value && n.item == need.item && n.args == need.args) {
                let mut chain = Vec::new_in(self.alloc);
                for n in self.stack[first..].iter() {
                    chain.push(n.item);
                }
                chain.push(need.item);
                return Err(ConstError { constant: need.item, at: None, refusal: Refusal::Cycle { chain } });
            }
        }
        if self.stack.len() as u32 >= self.depth {
            return Err(ConstError { constant: need.item, at: None, refusal: Refusal::CallDepth { limit: self.depth } });
        }
        self.stack.push(need);
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Value,
    Place,
}

/// Sentinel for an integer word (no allocation provenance) in a compact [`Pair`].
const NO_ALLOC: u32 = u32::MAX;

/// Two 8-byte scalars (a wide pointer's address and metadata), packed into 24 bytes (align 8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Pair {
    alloc0: u32,
    alloc1: u32,
    bits0: u64,
    bits1: u64,
}

impl Pair {
    #[inline(always)]
    fn of(a: Scalar, b: Scalar) -> Pair {
        let (alloc0, bits0) = match a {
            Scalar::Int(x) => (NO_ALLOC, x as u64),
            Scalar::Ptr(p) => (p.alloc.0, p.offset),
        };
        let (alloc1, bits1) = match b {
            Scalar::Int(x) => (NO_ALLOC, x as u64),
            Scalar::Ptr(p) => (p.alloc.0, p.offset),
        };
        Pair { alloc0, alloc1, bits0, bits1 }
    }

    #[inline(always)]
    fn first(self) -> Scalar {
        if self.alloc0 == NO_ALLOC {
            Scalar::Int(u128::from(self.bits0))
        } else {
            Scalar::Ptr(Pointer { alloc: AllocId(self.alloc0), offset: self.bits0 })
        }
    }

    #[inline(always)]
    fn second(self) -> Scalar {
        if self.alloc1 == NO_ALLOC {
            Scalar::Int(u128::from(self.bits1))
        } else {
            Scalar::Ptr(Pointer { alloc: AllocId(self.alloc1), offset: self.bits1 })
        }
    }
}

/// A value: none (zero-sized), a scalar (fast-path <=64-bit int, 128-bit int, or pointer),
/// a pair (a slice's or `str`'s pointer and length), or an aggregate in memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Val {
    Zst,
    Int(u64),
    Int128([u64; 2]),
    Ptr(Pointer),
    Pair(Pair),
    Mem(Pointer),
}

impl Val {
    #[inline(always)]
    fn int(v: u128) -> Val {
        if v <= u64::MAX as u128 {
            Val::Int(v as u64)
        } else {
            Val::Int128([v as u64, (v >> 64) as u64])
        }
    }

    #[inline(always)]
    fn scalar(s: Scalar) -> Val {
        match s {
            Scalar::Int(v) => Val::int(v),
            Scalar::Ptr(p) => Val::Ptr(p),
        }
    }

    #[inline(always)]
    fn pair(a: Scalar, b: Scalar) -> Val {
        Val::Pair(Pair::of(a, b))
    }

    #[inline(always)]
    fn as_scalar(self) -> Option<Scalar> {
        match self {
            Val::Int(v) => Some(Scalar::Int(u128::from(v))),
            Val::Int128([lo, hi]) => Some(Scalar::Int(u128::from(lo) | (u128::from(hi) << 64))),
            Val::Ptr(p) => Some(Scalar::Ptr(p)),
            _ => None,
        }
    }

    #[inline(always)]
    fn as_int(self) -> Option<u128> {
        match self {
            Val::Int(v) => Some(u128::from(v)),
            Val::Int128([lo, hi]) => Some(u128::from(lo) | (u128::from(hi) << 64)),
            _ => None,
        }
    }
}

/// A place: an address (an allocation and offset, or an integer address with `alloc == NO_ALLOC`
/// when formed by dereferencing a provenance-free raw pointer), and a length for a slice's or
/// `str`'s.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Place {
    alloc: u32,
    has_meta: bool,
    offset: u64,
    meta: u64,
}

impl Place {
    #[inline(always)]
    fn of(ptr: Pointer) -> Place {
        Place { alloc: ptr.alloc.0, has_meta: false, offset: ptr.offset, meta: 0 }
    }

    #[inline(always)]
    fn with_meta(ptr: Pointer, meta: Option<u64>) -> Place {
        match meta {
            Some(m) => Place { alloc: ptr.alloc.0, has_meta: true, offset: ptr.offset, meta: m },
            None => Place { alloc: ptr.alloc.0, has_meta: false, offset: ptr.offset, meta: 0 },
        }
    }

    #[inline(always)]
    fn from_int(addr: u64, meta: Option<u64>) -> Place {
        match meta {
            Some(m) => Place { alloc: NO_ALLOC, has_meta: true, offset: addr, meta: m },
            None => Place { alloc: NO_ALLOC, has_meta: false, offset: addr, meta: 0 },
        }
    }

    #[inline(always)]
    fn ptr(self) -> Result<Pointer, Refusal<'static>> {
        if self.alloc == NO_ALLOC {
            Err(Refusal::IntAsPointer)
        } else {
            Ok(Pointer { alloc: AllocId(self.alloc), offset: self.offset })
        }
    }

    #[inline(always)]
    fn meta(self) -> Option<u64> {
        self.has_meta.then_some(self.meta)
    }
}

/// What a register holds, flattened to 32 bytes (down from 80 bytes).
#[derive(Clone, Copy, Debug)]
enum Out {
    Zst,
    Int(u64),
    Int128([u64; 2]),
    Ptr(Pointer),
    Pair(Pair),
    Mem(Pointer),
    Place(Place),
}

const _: () = assert!(core::mem::size_of::<Val>() <= 32);
const _: () = assert!(core::mem::size_of::<Out>() <= 32);

impl Out {
    #[inline(always)]
    fn val(v: Val) -> Out {
        match v {
            Val::Zst => Out::Zst,
            Val::Int(x) => Out::Int(x),
            Val::Int128(x) => Out::Int128(x),
            Val::Ptr(p) => Out::Ptr(p),
            Val::Pair(p) => Out::Pair(p),
            Val::Mem(p) => Out::Mem(p),
        }
    }

    #[inline(always)]
    fn to_val(self) -> Option<Val> {
        Some(match self {
            Out::Zst => Val::Zst,
            Out::Int(x) => Val::Int(x),
            Out::Int128(x) => Val::Int128(x),
            Out::Ptr(p) => Val::Ptr(p),
            Out::Pair(p) => Val::Pair(p),
            Out::Mem(p) => Val::Mem(p),
            Out::Place(_) => return None,
        })
    }
}

/// How a type's value is held.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Repr {
    Zst,
    Scalar(u64),
    Pair,
    Mem(u64),
}

/// A running call: its code, the operation next, where its registers and memory locals start,
/// and the caller's register its value goes to.
#[derive(Clone, Copy, Debug)]
struct Frame {
    code: u32,
    pc: u32,
    base: u32,
    locals: u32,
    dst: Reg,
}

/// Where the run goes after an operation: tiny `Copy` enum returned in a single register.
/// Return value (`Done`) and error/need (`Stop`) live on `Machine`.
#[derive(Clone, Copy)]
enum Flow {
    Next,
    Goto(u32),
    /// A call or a return moved the frames; the new top's `pc` is set.
    Moved,
    /// The root returned (`Machine::result` holds the value).
    Done,
    /// Stopped early (`Machine::error` holds the `Stop`).
    Stop,
}

const _: () = assert!(core::mem::size_of::<Flow>() <= 8);

/// The machine's working vectors, kept in [`Codes`] between evaluations of its unit and
/// borrowed in place (`&mut Work`): each evaluation empties them and keeps their room, so
/// after the first few nothing grows and nothing moves by value.
pub(crate) struct Work<K: Allocator + Clone> {
    mem: Memory<K>,
    frames: Vec<Frame, K>,
    regs: Vec<Out, K>,
    locals: Vec<Option<AllocId>, K>,
    temps: Vec<AllocId, K>,
    decoded: Vec<(RecordKey, AllocId), K>,
    statics: Vec<(ItemKey, AllocId), K>,
    fns: Vec<(ConstFn, AllocId), K>,
    /// Cached immutable string/byte-string literal allocations by `(code_idx << 32) | str_idx`.
    strs: Vec<(u64, AllocId), K>,
    /// Cached `type_name::<T>()` allocations by ground `PTy`: `(ty, alloc, len)`.
    type_names: Vec<(PTy, AllocId, u64), K>,
    reads: Vec<ContentId, K>,
    entered: Vec<u32, K>,
}

impl<K: Allocator + Clone> Work<K> {
    pub(crate) fn new_in(alloc: K) -> Work<K> {
        Work {
            mem: Memory::new_in(0, alloc.clone()),
            frames: Vec::new_in(alloc.clone()),
            regs: Vec::new_in(alloc.clone()),
            locals: Vec::new_in(alloc.clone()),
            temps: Vec::new_in(alloc.clone()),
            decoded: Vec::new_in(alloc.clone()),
            statics: Vec::new_in(alloc.clone()),
            fns: Vec::new_in(alloc.clone()),
            strs: Vec::new_in(alloc.clone()),
            type_names: Vec::new_in(alloc.clone()),
            reads: Vec::new_in(alloc.clone()),
            entered: Vec::new_in(alloc),
        }
    }

    fn reset(&mut self, memory_limit: u64) {
        self.mem.reset(memory_limit);
        self.frames.clear();
        self.regs.clear();
        self.locals.clear();
        self.temps.clear();
        self.decoded.clear();
        self.statics.clear();
        self.fns.clear();
        self.strs.clear();
        self.type_names.clear();
        self.reads.clear();
        self.entered.clear();
    }
}

struct Machine<'w, 'l, 'g, 't, 'wk, 'a, W: ConstWorld, L: Layout, K: Allocator + Clone> {
    world: &'w W,
    layout: &'l mut L,
    ground: &'g mut PortableTys<'t>,
    limits: Limits,
    alloc: Alloc<'a>,
    work: &'wk mut Work<K>,
    steps: u64,
    /// The operation that refused: what an error names.
    at: Option<At>,
    /// Parked root return value when `step` returns `Flow::Done`.
    result: Val,
    /// Parked stop when `step` returns `Flow::Stop`.
    error: Option<Stop<'a>>,
    plant: Plant,
}

impl<'w, 'l, 'g, 't, 'wk, 'a, W: ConstWorld, L: Layout, K: Allocator + Clone> Machine<'w, 'l, 'g, 't, 'wk, 'a, W, L, K> {
    /// A machine borrowing `work` in place, emptied (every evaluation starts from nothing).
    fn new(world: &'w W, layout: &'l mut L, ground: &'g mut PortableTys<'t>, limits: Limits, alloc: Alloc<'a>, work: &'wk mut Work<K>) -> Self {
        work.reset(limits.memory);
        Machine {
            world,
            layout,
            ground,
            limits,
            alloc,
            work,
            steps: 0,
            at: None,
            result: Val::Zst,
            error: None,
            plant: Plant::None,
        }
    }

    #[cold]
    #[inline(never)]
    fn fail(&mut self, s: Stop<'a>) -> Flow {
        self.error = Some(s);
        Flow::Stop
    }

    /// Adds `id` to the reads, kept sorted and without repeats.
    fn read_add(&mut self, id: ContentId) {
        if let Err(at) = self.work.reads.binary_search(&id) {
            self.work.reads.insert_shift(at, id);
        }
    }

    // ---- The evaluation ----

    fn evaluate<A: Allocator + Clone>(&mut self, codes: &mut CodeTable<A>, item: ItemKey, args: List) -> R<'a, ConstRecord<Alloc<'a>>> {
        let ci = self.code(codes, item, args)?;
        let (code_id, ret) = {
            let c = &codes.list[ci as usize];
            (c.code_id, c.ret)
        };
        let key = RecordKey::of(instance_key(code_id, args, self.ground.view(), self.alloc).map_err(|_| malformed("an instance's arguments past the ground table"))?);
        self.enter(codes, ci, 0, 0)?;
        let v = self.run(codes)?;
        let held = match self.world.is_static(item) {
            true => Held::Static,
            false => Held::Copied,
        };
        let data = self.intern(v, ret, held)?;
        data.check().map_err(|_| malformed("a record that breaks blaze-build's data form"))?;
        let mut reads = Vec::with_capacity_in(self.work.reads.len(), self.alloc);
        reads.extend_from_slice(&self.work.reads);
        let fns = self.fns_named(&data)?;
        Ok(ConstRecord { key, data, reads, fns })
    }

    /// The instance of each fn word of `data` ([`ConstFn`]), sorted by key, each once: every
    /// fn allocation this evaluation made or decoded was made from one (`fn_ptr`), so a word
    /// with none is malformed, never a word the record leaves unnamed.
    fn fns_named(&self, data: &DataRecord<Alloc<'a>>) -> R<'a, Vec<ConstFn, Alloc<'a>>> {
        let mut fns: Vec<ConstFn, Alloc<'a>> = Vec::new_in(self.alloc);
        for a in data.allocs.iter() {
            for r in a.relocs.iter() {
                let DataTarget::Fn(key) = r.target else { continue };
                let Err(at) = fns.binary_search_by_key(&key, |f| f.key) else { continue };
                let named = self.work.fns.iter().find(|(f, _)| f.key == key).ok_or(malformed("a fn word whose instance the evaluation did not make"))?;
                fns.insert_shift(at, named.0);
            }
        }
        Ok(fns)
    }

    fn instance(&self, item: ItemKey, args: List) -> R<'a, InstanceKey> {
        instance_key(self.world.code_id(item), args, self.ground.view(), self.alloc).map_err(|_| malformed("an instance's arguments past the ground table"))
    }

    /// Pushes a frame running code `ci` with registers from `base`; its value goes to the
    /// caller's register `dst`.
    fn enter<A: Allocator + Clone>(&mut self, codes: &CodeTable<A>, ci: u32, dst: Reg, base: u32) -> R<'a, ()> {
        if self.work.frames.len() as u32 >= self.limits.depth {
            return Err(Stop::Refused(Refusal::CallDepth { limit: self.limits.depth }));
        }
        let c = &codes.list[ci as usize];
        // What the code read, once per evaluation: its body, and the records its compile read.
        if self.work.entered.scan_for(&ci).is_none() {
            self.work.entered.push(ci);
            self.world.entered_body(c.item);
            self.read_add(c.hash);
            for &r in c.reads.iter() {
                self.read_add(r);
            }
        }
        let need = base as usize + c.regs as usize;
        if self.work.regs.len() < need {
            self.work.regs.resize(need, Out::Zst);
        }
        let locals = self.work.locals.len() as u32;
        self.work.locals.resize(self.work.locals.len() + c.slots as usize, None);
        self.work.frames.push(Frame { code: ci, pc: 0, base, locals, dst });
        Ok(())
    }

    // Not inlined: inside `value_of`'s large frame the loop's values were spilled and reloaded
    // through the stack on every step.
    #[inline(never)]
    fn run<A: Allocator + Clone>(&mut self, codes: &mut CodeTable<A>) -> R<'a, Val> {
        'frame: loop {
            let top = self.work.frames.len().checked_sub(1).ok_or(malformed("no frame"))?;
            let mut f = self.work.frames[top];
            let code = &codes.list[f.code as usize];
            let ops = code.ops.as_slice();
            loop {
                let Some(&op) = ops.get(f.pc as usize) else {
                    return Err(malformed("a jump past its code"));
                };
                self.steps += 1;
                if self.steps > self.limits.steps {
                    self.at = None;
                    return Err(Stop::Refused(Refusal::StepLimit { limit: self.limits.steps }));
                }
                if let Op::Call { dst, k } = op {
                    if let Err(s) = self.call(codes, f, dst, k) {
                        let c = &codes.list[f.code as usize];
                        self.at = Some(At { body: c.item, range: c.at.get(f.pc as usize).copied().unwrap_or_default() });
                        return Err(s);
                    }
                    continue 'frame;
                }
                match self.step(code, f, op) {
                    Flow::Next => f.pc += 1,
                    Flow::Goto(to) => f.pc = to,
                    Flow::Moved => continue 'frame,
                    Flow::Done => return Ok(self.result),
                    Flow::Stop => {
                        self.at = Some(At { body: code.item, range: code.at.get(f.pc as usize).copied().unwrap_or_default() });
                        return Err(self.error.take().unwrap_or_else(|| malformed("stopped with no error")));
                    }
                }
            }
        }
    }

    #[inline(always)]
    fn set(&mut self, base: u32, r: Reg, o: Out) {
        self.work.regs[(base + r) as usize] = o;
    }

    #[inline(always)]
    fn val(&self, base: u32, r: Reg) -> R<'a, Val> {
        self.work.regs[(base + r) as usize].to_val().ok_or_else(|| malformed("a place where a value was asked"))
    }

    #[inline(always)]
    fn place(&self, base: u32, r: Reg) -> R<'a, Place> {
        match self.work.regs[(base + r) as usize] {
            Out::Place(p) => Ok(p),
            _ => Err(malformed("a value where a place was asked")),
        }
    }

    #[inline(always)]
    fn bool_of(&self, base: u32, r: Reg) -> R<'a, bool> {
        match self.work.regs[(base + r) as usize] {
            Out::Int(0) => Ok(false),
            Out::Int(1) => Ok(true),
            Out::Int(v) => Err(Stop::Refused(Refusal::InvalidBool { value: u128::from(v) })),
            Out::Int128([lo, hi]) => Err(Stop::Refused(Refusal::InvalidBool { value: u128::from(lo) | (u128::from(hi) << 64) })),
            _ => Err(malformed("a condition that is not a `bool`")),
        }
    }

    #[inline(always)]
    fn binary_u64_fast(&self, op: BinOp, num: Num, rnum: Num, a: u64, b: u64) -> Option<u64> {
        if self.plant != Plant::None {
            return None;
        }
        let (bits, signed) = match num {
            Num::U8 | Num::Bool => (8u32, false),
            Num::U16 => (16, false),
            Num::U32 => (32, false),
            Num::U64 => (64, false),
            Num::I8 => (8, true),
            Num::I16 => (16, true),
            Num::I32 => (32, true),
            Num::I64 => (64, true),
            _ => return None,
        };
        let m = if bits == 64 { u64::MAX } else { (1u64 << bits) - 1 };
        match op {
            BinOp::BitAnd => Some(a & b),
            BinOp::BitOr => Some(a | b),
            BinOp::BitXor => Some(a ^ b),
            BinOp::Eq => Some(u64::from(a == b)),
            BinOp::Ne => Some(u64::from(a != b)),
            BinOp::Lt if !signed => Some(u64::from(a < b)),
            BinOp::Le if !signed => Some(u64::from(a <= b)),
            BinOp::Gt if !signed => Some(u64::from(a > b)),
            BinOp::Ge if !signed => Some(u64::from(a >= b)),
            BinOp::Add if !signed => a.checked_add(b).filter(|&r| r <= m),
            BinOp::Sub if !signed => a.checked_sub(b),
            BinOp::Mul if !signed => a.checked_mul(b).filter(|&r| r <= m),
            BinOp::Shr if !signed => {
                if !matches!(rnum, Num::F16 | Num::F32 | Num::F64 | Num::F128) && b < u64::from(bits) {
                    Some(a >> (b as u32))
                } else {
                    None
                }
            }
            BinOp::Shl => {
                if !matches!(rnum, Num::F16 | Num::F32 | Num::F64 | Num::F128) && b < u64::from(bits) {
                    Some((a << (b as u32)) & m)
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Runs one operation of frame `f`, returning a register-sized `Flow`.
    fn step<A: Allocator + Clone>(&mut self, code: &Code<A>, f: Frame, op: Op) -> Flow {
        macro_rules! try_step {
            ($e:expr) => {
                match $e {
                    Ok(v) => v,
                    Err(e) => return self.fail(e.into()),
                }
            };
        }
        let b = f.base;
        match op {
            Op::ImmZst { dst } => self.set(b, dst, Out::Zst),
            Op::ImmU64 { dst, v } => self.set(b, dst, Out::Int(v)),
            Op::Imm { dst, k } => self.set(b, dst, Out::val(code.imms[k as usize])),
            Op::Str { dst, k } => {
                let lit = code.strs[k as usize];
                let cache_key = (u64::from(f.code) << 32) | u64::from(k);
                let id = match self.work.strs.iter().find(|&&(ck, _)| ck == cache_key) {
                    Some(&(_, id)) => id,
                    None => {
                        let bytes = try_step!(code.bytes.get(lit.at as usize..(lit.at + lit.len) as usize).ok_or(malformed("a string past its code's bytes")));
                        let id = try_step!(self.work.mem.allocate(u64::from(lit.len), 0, true, Origin::Data));
                        try_step!(self.work.mem.fill(id, bytes, &[]));
                        try_step!(self.work.mem.get_mut(id)).mutable = false;
                        self.work.strs.push((cache_key, id));
                        id
                    }
                };
                let p = Pointer { alloc: id, offset: 0 };
                let out = if lit.wide {
                    Out::Pair(Pair::of(Scalar::Ptr(p), Scalar::Int(u128::from(lit.len))))
                } else {
                    Out::Ptr(p)
                };
                self.set(b, dst, out);
            }
            Op::Move { dst, src } => self.work.regs[(b + dst) as usize] = self.work.regs[(b + src) as usize],
            Op::Local { dst, slot, ty } => {
                let i = (f.locals + slot) as usize;
                let id = match self.work.locals[i] {
                    Some(id) => id,
                    None => {
                        let t = code.tys[ty as usize];
                        let id = try_step!(self.work.mem.allocate(t.size, t.align, true, Origin::Local));
                        self.work.locals[i] = Some(id);
                        id
                    }
                };
                self.set(b, dst, Out::Place(Place::of(Pointer { alloc: id, offset: 0 })));
            }
            Op::Const { dst, k } => {
                let (item, args, key) = code.consts[k as usize];
                let pl = try_step!(self.const_place(item, args, key));
                self.set(b, dst, Out::Place(pl));
            }
            Op::Static { dst, k } => {
                let (item, mutable) = code.statics[k as usize];
                let id = try_step!(self.static_alloc(item, mutable));
                self.set(b, dst, Out::Place(Place::of(Pointer { alloc: id, offset: 0 })));
            }
            Op::Load { dst, src, ty } => {
                let out = match self.work.regs[(b + src) as usize] {
                    Out::Place(pl) => Out::val(try_step!(self.load_i(pl, code.tys[ty as usize]))),
                    other => other,
                };
                self.set(b, dst, out);
            }
            Op::Spill { dst, src, ty } => {
                let pl = match self.work.regs[(b + src) as usize] {
                    Out::Place(pl) => pl,
                    Out::Mem(p) => Place::of(p),
                    other => {
                        let v = try_step!(other.to_val().ok_or(malformed("a place where a value was asked")));
                        let t = code.tys[ty as usize];
                        let pl = Place::of(try_step!(self.temp_i(t)));
                        try_step!(self.store_i(pl, t, v));
                        pl
                    }
                };
                self.set(b, dst, Out::Place(pl));
            }
            Op::Deref { dst, src } => {
                let v = try_step!(self.val(b, src));
                let pl = try_step!(self.deref(v));
                self.set(b, dst, Out::Place(pl));
            }
            Op::Borrow { dst, src, meta } => {
                let mut pl = try_step!(self.place(b, src));
                let raw = match meta {
                    Meta::Keep => false,
                    Meta::Drop => {
                        pl.has_meta = false;
                        false
                    }
                    Meta::Need => {
                        if !pl.has_meta {
                            return self.fail(malformed("an unsized place with no length"));
                        }
                        false
                    }
                    Meta::Raw => true,
                };
                let v = try_step!(self.borrow(pl, raw));
                self.set(b, dst, Out::val(v));
            }
            Op::Unsize { dst, src, n } => match try_step!(self.val(b, src)).as_scalar() {
                Some(p) => self.set(b, dst, Out::Pair(Pair::of(p, Scalar::Int(u128::from(n))))),
                None => return self.fail(malformed("an unsizing coercion of a wide pointer")),
            },
            Op::UnsizeWide { dst, src, k } => {
                let n = code.u64s[k as usize];
                match try_step!(self.val(b, src)).as_scalar() {
                    Some(p) => self.set(b, dst, Out::Pair(Pair::of(p, Scalar::Int(u128::from(n))))),
                    None => return self.fail(malformed("an unsizing coercion of a wide pointer")),
                }
            }
            Op::FnPtr { dst, k } => {
                let named = code.fns[k as usize];
                let p = try_step!(self.fn_ptr(named));
                self.set(b, dst, Out::Ptr(p));
            }
            Op::Unary { dst, src, op, num } => {
                let v = try_step!(self.val(b, src));
                let r = try_step!(self.unary(op, num, v));
                self.set(b, dst, Out::val(r));
            }
            Op::Binary { dst, l, r, op, num, rnum } => {
                if let (Out::Int(a), Out::Int(bv)) = (self.work.regs[(b + l) as usize], self.work.regs[(b + r) as usize]) {
                    if let Some(res) = self.binary_u64_fast(op, num, rnum, a, bv) {
                        self.work.regs[(b + dst) as usize] = Out::Int(res);
                        return Flow::Next;
                    }
                }
                let (lv, rv) = (try_step!(self.val(b, l)), try_step!(self.val(b, r)));
                let v = try_step!(self.binary(op, num, rnum, lv, rv));
                self.set(b, dst, Out::val(v));
            }
            Op::Cast { dst, src, k } => {
                let (from, to) = code.casts[k as usize];
                let v = try_step!(self.val(b, src));
                let r = try_step!(self.cast(v, from, to));
                self.set(b, dst, Out::val(r));
            }
            Op::Discr { dst, src, k } => {
                let d = code.discr_ops[k as usize];
                let pl = match self.work.regs[(b + src) as usize] {
                    Out::Place(pl) => pl,
                    Out::Mem(p) => Place::of(p),
                    other => {
                        let v = try_step!(other.to_val().ok_or(malformed("a place where a value was asked")));
                        let t = code.tys[d.ty as usize];
                        let pl = Place::of(try_step!(self.temp_i(t)));
                        try_step!(self.store_i(pl, t, v));
                        pl
                    }
                };
                let facts = try_step!(code.enums.get(d.e as usize).ok_or(malformed("an enum past its code")));
                let variant = try_step!(self.variant_of(pl, facts, &code.discrs));
                let discr = try_step!(facts.discriminant(variant, &code.discrs));
                self.set(b, dst, Out::val(Val::int(mask(discr as u128, d.bits))));
            }
            Op::Field { dst, src, off, tail } => {
                let pl = try_step!(self.place(b, src));
                let meta = if tail { pl.meta() } else { None };
                let next = try_step!(self.place_offset(pl, u64::from(off), meta));
                self.set(b, dst, Out::Place(next));
            }
            Op::FieldWide { dst, src, k, tail } => {
                let pl = try_step!(self.place(b, src));
                let off = code.u64s[k as usize];
                let meta = if tail { pl.meta() } else { None };
                let next = try_step!(self.place_offset(pl, off, meta));
                self.set(b, dst, Out::Place(next));
            }
            Op::Index { dst, k } => {
                let idx = code.indexes[k as usize];
                let i = try_step!(self.val(b, idx.index));
                let pl = try_step!(self.place(b, idx.base));
                let len = match idx.len {
                    Some(n) => n,
                    None => try_step!(pl.meta().ok_or(malformed("a slice place with no length"))),
                };
                let Some(i) = i.as_int() else { return self.fail(Stop::Refused(Refusal::PointerAsInt)) };
                if i >= u128::from(len) {
                    return self.fail(Stop::Refused(Refusal::IndexOutOfBounds { index: i as u64, len }));
                }
                let Some(off) = idx.size.checked_mul(i as u64) else {
                    return self.fail(Stop::Refused(Refusal::OutOfBounds { offset: u64::MAX, size: 0, len }));
                };
                let next = try_step!(self.place_offset(pl, off, None));
                self.set(b, dst, Out::Place(next));
            }
            Op::FromEnd { dst, src, k } => {
                let s = code.slice_ops[k as usize];
                let pl = try_step!(self.place(b, src));
                let len = try_step!(pl.meta().ok_or(malformed("a slice place with no length")));
                let i = try_step!(len.checked_sub(s.back).ok_or(malformed("a slice pattern's element past a slice its length test let through")));
                let next = try_step!(self.place_offset(pl, s.size * i, None));
                self.set(b, dst, Out::Place(next));
            }
            Op::Subslice { dst, src, k } => {
                let s = code.slice_ops[k as usize];
                let pl = try_step!(self.place(b, src));
                let len = try_step!(pl.meta().ok_or(malformed("a slice place with no length")));
                let left = try_step!(len.checked_sub(s.from + s.back).ok_or(malformed("a slice pattern's `..` past a slice its length test let through")));
                let next = try_step!(self.place_offset(pl, s.size * s.from, Some(left)));
                self.set(b, dst, Out::Place(next));
            }
            Op::Build { dst, k } => {
                let build = code.builds[k as usize];
                let p = try_step!(self.temp_i(code.tys[build.ty as usize]));
                for part in &code.parts[build.parts.0 as usize..(build.parts.0 + build.parts.1) as usize] {
                    let v = try_step!(self.val(b, part.src));
                    try_step!(self.store_i(Place::of(p.at(part.off)), code.tys[part.ty as usize], v));
                }
                if let Some(w) = build.tag {
                    try_step!(self.work.mem.write(p.at(w.offset), w.size, Scalar::Int(w.value)));
                }
                self.set(b, dst, Out::Mem(p));
            }
            Op::Update { dst, base, k } => {
                let build = code.builds[k as usize];
                let pl = try_step!(self.place(b, base));
                let v0 = try_step!(self.load_i(pl, code.tys[build.ty as usize]));
                if let Val::Mem(p) = v0 {
                    for part in &code.parts[build.parts.0 as usize..(build.parts.0 + build.parts.1) as usize] {
                        let v = try_step!(self.val(b, part.src));
                        try_step!(self.store_i(Place::of(p.at(part.off)), code.tys[part.ty as usize], v));
                    }
                }
                self.set(b, dst, Out::val(v0));
            }
            Op::Repeat { dst, src, k } => {
                let rep = code.repeats[k as usize];
                let v = try_step!(self.val(b, src));
                // The memory bound first: an array too large to hold is refused as that before
                // any of its `n` stores is counted (rule 18: each bound names its own cause).
                let p = try_step!(self.temp_i(code.tys[rep.ty as usize]));
                self.steps = self.steps.saturating_add(rep.n);
                if self.steps > self.limits.steps {
                    return self.fail(Stop::Refused(Refusal::StepLimit { limit: self.limits.steps }));
                }
                let el = code.tys[rep.elem as usize];
                for k in 0..rep.n {
                    try_step!(self.store_i(Place::of(p.at(el.size * k)), el, v));
                }
                self.set(b, dst, Out::Mem(p));
            }
            Op::Store { place, src, ty } => {
                let pl = try_step!(self.place(b, place));
                let v = try_step!(self.val(b, src));
                try_step!(self.store_i(pl, code.tys[ty as usize], v));
            }
            Op::StoreOp { place, src, ty, op, num, rnum } => {
                let pl = try_step!(self.place(b, place));
                let r = try_step!(self.val(b, src));
                let t = code.tys[ty as usize];
                let l = try_step!(self.load_i(pl, t));
                let v = try_step!(self.binary(op, num, rnum, l, r));
                try_step!(self.store_i(pl, t, v));
            }
            Op::Jump { to } => return Flow::Goto(to),
            Op::Branch { cond, when, to } => {
                if try_step!(self.bool_of(b, cond)) == when {
                    return Flow::Goto(to);
                }
            }
            Op::Call { .. } => return self.fail(malformed("a call run out of turn")),
            Op::Intrinsic { dst, k } => {
                let call = &code.calls[k as usize];
                let i = try_step!(call.intrinsic.ok_or(malformed("an intrinsic call with no intrinsic")));
                let arg_slice = &code.regs_of[call.regs.0 as usize..(call.regs.0 + call.regs.1) as usize];
                let v = if arg_slice.len() <= 4 {
                    let mut buf = [(Val::Zst, PTy(0)); 4];
                    for (slot, &(r, t)) in buf.iter_mut().zip(arg_slice.iter()) {
                        *slot = (try_step!(self.val(b, r)), t);
                    }
                    try_step!(self.intrinsic(i, call.args, call.ret, &buf[..arg_slice.len()]))
                } else {
                    let mut vals: Vec<(Val, PTy), Alloc<'a>> = Vec::with_capacity_in(arg_slice.len(), self.alloc);
                    for &(r, t) in arg_slice {
                        vals.push((try_step!(self.val(b, r)), t));
                    }
                    try_step!(self.intrinsic(i, call.args, call.ret, &vals))
                };
                self.set(b, dst, Out::val(v));
            }
            Op::Panic { k } => {
                let call = &code.calls[k as usize];
                let mut parts = Vec::new_in(self.alloc);
                for &(r, t) in &code.regs_of[call.regs.0 as usize..(call.regs.0 + call.regs.1) as usize] {
                    let v = try_step!(self.val(b, r));
                    parts.push(try_step!(self.panic_part(v, t)));
                }
                return self.fail(Stop::Refused(Refusal::Panic { parts }));
            }
            Op::Ret { src } => {
                let v = self.work.regs[(b + src) as usize];
                self.work.frames.pop();
                while self.work.locals.len() > f.locals as usize {
                    if let Some(Some(id)) = self.work.locals.pop() {
                        self.work.mem.kill(id);
                    }
                }
                return match self.work.frames.last() {
                    None => match v.to_val() {
                        Some(val) => {
                            self.result = val;
                            Flow::Done
                        }
                        None => self.fail(malformed("a body whose value is a place")),
                    },
                    Some(c) => {
                        let at = c.base;
                        self.set(at, f.dst, v);
                        Flow::Moved
                    }
                };
            }
            Op::Mark { dst } => self.set(b, dst, Out::Int(self.work.temps.len() as u64)),
            Op::Kill { mark } => {
                // An expression statement's temporaries end with it (the Reference, "Temporary
                // scopes"); no `let` extends them.
                let Out::Int(n) = self.work.regs[(b + mark) as usize] else {
                    return self.fail(malformed("a statement mark that is not a count"));
                };
                while self.work.temps.len() > n as usize {
                    if let Some(id) = self.work.temps.pop() {
                        self.work.mem.kill(id);
                    }
                }
            }
            Op::Nop => {}
            Op::IfVariant { place, k, fail } => {
                let (e, variant) = code.if_variants[k as usize];
                let pl = try_step!(self.place(b, place));
                let facts = try_step!(code.enums.get(e as usize).ok_or(malformed("an enum past its code")));
                if try_step!(self.variant_of(pl, facts, &code.discrs)) != variant {
                    return Flow::Goto(fail);
                }
            }
            Op::IfSame { k, fail } => {
                let s = code.if_sames[k as usize];
                let (want, have) = (try_step!(self.val(b, s.a)), try_step!(self.val(b, s.b)));
                if s.konst && matches!(want, Val::Mem(_)) {
                    return self.fail(unsupported("a constant pattern of an aggregate type"));
                }
                if !try_step!(self.same(want, have, s.ty)) {
                    return Flow::Goto(fail);
                }
            }
            Op::IfLen { place, k, fail } => {
                let (len, at_least) = code.if_lens[k as usize];
                let pl = try_step!(self.place(b, place));
                let have = try_step!(pl.meta().ok_or(malformed("a slice place with no length")));
                if if at_least { have < len } else { have != len } {
                    return Flow::Goto(fail);
                }
            }
            Op::Copy { dst, src, size } => {
                let (d, s) = (try_step!(self.place(b, dst)), try_step!(self.place(b, src)));
                let (dp, sp) = (try_step!(d.ptr()), try_step!(s.ptr()));
                try_step!(self.ensure_loaded(sp.alloc));
                try_step!(self.ensure_loaded(dp.alloc));
                try_step!(self.work.mem.copy(sp, dp, u64::from(size)));
            }
            Op::CopyWide { dst, src, k } => {
                let size = code.u64s[k as usize];
                let (d, s) = (try_step!(self.place(b, dst)), try_step!(self.place(b, src)));
                let (dp, sp) = (try_step!(d.ptr()), try_step!(s.ptr()));
                try_step!(self.ensure_loaded(sp.alloc));
                try_step!(self.ensure_loaded(dp.alloc));
                try_step!(self.work.mem.copy(sp, dp, size));
            }
            Op::Late { k } => {
                // A pending stop made this code incomplete when it was compiled
                // (machine/late.rs): the next evaluation compiles it again before running it.
                return self.fail(try_step!(code.lates.get(k as usize).ok_or(malformed("a stop past its code"))).stop());
            }
        }
        Flow::Next
    }

    /// A `const fn` call: the callee's code (compiled when first called), its frame, its
    /// arguments in its first registers.
    fn call<A: Allocator + Clone>(&mut self, codes: &mut CodeTable<A>, f: Frame, dst: Reg, k: u32) -> R<'a, ()> {
        let (item, args, link) = {
            let c = &codes.list[f.code as usize].calls[k as usize];
            (c.item, c.args, c.link.get())
        };
        let ci = if link != u32::MAX && codes.list[link as usize].complete {
            link
        } else {
            let ci = self.code(codes, item, args)?;
            codes.list[f.code as usize].calls[k as usize].link.set(ci);
            ci
        };
        let caller = &codes.list[f.code as usize];
        let regs = caller.calls[k as usize].regs;
        if regs.1 != codes.list[ci as usize].params {
            return Err(malformed("a call with another count of arguments than parameters"));
        }
        let base = f.base + caller.regs;
        let top = self.work.frames.len() - 1;
        self.work.frames[top].pc = f.pc + 1;
        self.enter(codes, ci, dst, base)?;
        for (i, &(r, _)) in caller.regs_of[regs.0 as usize..(regs.0 + regs.1) as usize].iter().enumerate() {
            self.work.regs[base as usize + i] = self.work.regs[(f.base + r) as usize];
        }
        Ok(())
    }

    // ---- Types ----

    fn node(&self, t: PTy) -> R<'a, PNode> {
        self.ground.view().node(t).map_err(|_| malformed("a type past the ground table"))
    }

    /// The value of ground array length `n` (a `ConstValue` once grounded).
    fn len(&self, n: PTy) -> R<'a, u64> {
        self.ground.view().value_u64(n).ok_or(malformed("an array length that is not a value"))
    }

    fn cx(&mut self) -> (LayoutCx<'_, 't>, &mut L) {
        (LayoutCx { ground: self.ground, facts: self.world, scratch: self.alloc }, self.layout)
    }

    fn size_align(&mut self, t: PTy) -> R<'a, (u64, u8)> {
        let (mut cx, l) = self.cx();
        Ok(l.size_align(t, &mut cx)?)
    }

    fn field_offset(&mut self, t: PTy, variant: u32, field: u32) -> R<'a, u64> {
        let (mut cx, l) = self.cx();
        Ok(l.field_offset(t, variant, field, &mut cx)?)
    }

    fn repr(&mut self, t: PTy) -> R<'a, Repr> {
        Ok(match self.node(t)? {
            PNode::Bool | PNode::Char | PNode::Int(_) | PNode::Uint(_) => Repr::Scalar(layout::int_size(self.node(t)?).ok_or(malformed("an int size"))?),
            PNode::Float(FloatTy::F32) => Repr::Scalar(4),
            PNode::Float(FloatTy::F64) => Repr::Scalar(8),
            // binary16 and binary128: size and alignment their width (the targets' ABIs).
            PNode::Float(FloatTy::F16) => Repr::Scalar(2),
            PNode::Float(FloatTy::F128) => Repr::Scalar(16),
            PNode::Ref(_, p) | PNode::Ptr(_, p) => {
                if layout::is_unsized(self.world, self.ground, p, self.alloc)? {
                    Repr::Pair
                } else {
                    Repr::Scalar(PTR)
                }
            }
            PNode::FnPtr(..) => Repr::Scalar(PTR),
            PNode::FnDef(..) | PNode::Never => Repr::Zst,
            PNode::Str | PNode::Slice(_) => return Err(unsupported("a value of an unsized type")),
            _ => match self.size_align(t)?.0 {
                0 => Repr::Zst,
                n => Repr::Mem(n),
            },
        })
    }

    /// What reading, writing and making a value of `t` needs.
    fn tyinfo(&mut self, t: PTy) -> R<'a, TyInfo> {
        let repr = self.repr(t)?;
        let (size, align) = match repr {
            Repr::Zst => (0, 0),
            Repr::Scalar(n) => (n, n.trailing_zeros() as u8),
            Repr::Pair => (2 * PTR, 3),
            Repr::Mem(_) => self.size_align(t)?,
        };
        let valid = match self.node(t)? {
            PNode::Bool => Valid::Bool,
            PNode::Char => Valid::Char,
            PNode::Int(_) | PNode::Uint(_) | PNode::Float(_) => Valid::Num,
            _ => Valid::Any,
        };
        Ok(TyInfo { repr, size, align, valid })
    }

    /// The ground type of field `field` (of the variant's own numbering) of ADT, tuple, record or
    /// structural union `t`.
    fn field_ty(&mut self, t: PTy, variant: u32, field: u32) -> R<'a, PTy> {
        match self.node(t)? {
            PNode::Tuple(l) | PNode::Record(_, l) => self
                .ground
                .view()
                .list(l)
                .ok()
                .and_then(|l| usize::try_from(field).ok().and_then(|i| l.get(i).copied()))
                .ok_or(malformed("a tuple field past its tuple")),
            PNode::Union(l) => {
                if field != 0 {
                    return Err(malformed("a union variant field other than 0"));
                }
                self.ground
                    .view()
                    .list(l)
                    .ok()
                    .and_then(|l| usize::try_from(variant).ok().and_then(|i| l.get(i).copied()))
                    .ok_or(malformed("a union variant past its union"))
            }
            PNode::Adt(key, args) => {
                let adt = self.world.adt(key).ok_or(Stop::Refused(Refusal::Missing { what: "ADT definition", item: key }))?;
                let v = adt.variants.get(variant as usize).ok_or(malformed("a variant past its ADT"))?;
                if field >= v.fields.len {
                    return Err(malformed("a field past its variant"));
                }
                Ok(layout::field_ty(key, &adt, v.fields.start + field, args, self.ground, self.alloc, self.world)?)
            }
            PNode::Array(e, _) | PNode::Slice(e) => Ok(e),
            _ => Err(malformed("a field of a type with none")),
        }
    }

    fn variant_fields(&self, t: PTy, variant: u32) -> R<'a, u32> {
        match self.node(t)? {
            PNode::Adt(key, _) => {
                let adt = self.world.adt(key).ok_or(Stop::Refused(Refusal::Missing { what: "ADT definition", item: key }))?;
                Ok(adt.variants.get(variant as usize).ok_or(malformed("a variant past its ADT"))?.fields.len)
            }
            _ => Err(malformed("a tuple-struct pattern of a type that is not an ADT")),
        }
    }

    fn resolve(&mut self, item: ItemKey, args: List) -> R<'a, (ItemKey, List)> {
        let world = self.world;
        world.resolve(item, args, self.ground, self.alloc).ok_or(Stop::Refused(Refusal::Missing { what: "impl for an associated item", item }))
    }

    // ---- Memory ----

    fn temp_i(&mut self, t: TyInfo) -> R<'a, Pointer> {
        let id = self.work.mem.allocate(t.size, t.align, true, Origin::Data)?;
        self.work.temps.push(id);
        Ok(Pointer { alloc: id, offset: 0 })
    }

    /// A static's or decoded record's allocation, its bytes loaded.
    fn ensure_loaded(&mut self, id: AllocId) -> R<'a, ()> {
        let a = self.work.mem.get(id)?;
        if a.mut_static {
            let Origin::Static(item) = a.origin else { return Err(malformed("a mutable static with no item")) };
            return Err(Stop::Refused(Refusal::MutableStatic { item }));
        }
        if a.loaded {
            return Ok(());
        }
        let Origin::Static(item) = a.origin else { return Ok(()) };
        let key = RecordKey::of(self.instance(item, List::EMPTY)?);
        let world = self.world;
        let rec = world.record(key).map_err(|_| Stop::Need(Need { kind: NeedKind::Value, item, args: List::EMPTY }))?;
        self.read_add(key.0);
        self.decode(rec, Some(id))?;
        Ok(())
    }

    fn read(&mut self, p: Pointer, n: u64) -> R<'a, Scalar> {
        self.ensure_loaded(p.alloc)?;
        Ok(self.work.mem.read(p, n)?)
    }

    fn load_i(&mut self, pl: Place, t: TyInfo) -> R<'a, Val> {
        let ptr = pl.ptr()?;
        self.ensure_loaded(ptr.alloc)?;
        Ok(match t.repr {
            Repr::Zst => {
                self.work.mem.check_access(ptr, 0)?;
                Val::Zst
            }
            Repr::Scalar(n) => {
                let s = self.work.mem.read(ptr, n)?;
                valid(s, t.valid)?;
                Val::scalar(s)
            }
            Repr::Pair => Val::pair(self.work.mem.read(ptr, PTR)?, self.work.mem.read(ptr.at(PTR), PTR)?),
            Repr::Mem(n) => {
                let to = self.temp_i(t)?;
                self.work.mem.copy(ptr, to, n)?;
                Val::Mem(to)
            }
        })
    }

    fn load(&mut self, pl: Place, t: PTy) -> R<'a, Val> {
        let t = self.tyinfo(t)?;
        self.load_i(pl, t)
    }

    fn store_i(&mut self, pl: Place, t: TyInfo, v: Val) -> R<'a, ()> {
        let ptr = pl.ptr()?;
        self.ensure_loaded(ptr.alloc)?;
        match (t.repr, v) {
            (Repr::Zst, _) => Ok(()),
            (Repr::Scalar(n), v) => {
                let Some(s) = v.as_scalar() else { return Err(malformed("a value of another shape than its type")) };
                Ok(self.work.mem.write(ptr, n, s)?)
            }
            (Repr::Pair, Val::Pair(pair)) => {
                self.work.mem.write(ptr, PTR, pair.first())?;
                Ok(self.work.mem.write(ptr.at(PTR), PTR, pair.second())?)
            }
            (Repr::Mem(n), Val::Mem(src)) => {
                self.ensure_loaded(src.alloc)?;
                Ok(self.work.mem.copy(src, ptr, n)?)
            }
            _ => Err(malformed("a value of another shape than its type")),
        }
    }

    fn store(&mut self, pl: Place, t: PTy, v: Val) -> R<'a, ()> {
        let t = self.tyinfo(t)?;
        self.store_i(pl, t, v)
    }

    fn spill(&mut self, v: Val, t: PTy) -> R<'a, Place> {
        if let Val::Mem(p) = v {
            return Ok(Place::of(p));
        }
        let t = self.tyinfo(t)?;
        let pl = Place::of(self.temp_i(t)?);
        self.store_i(pl, t, v)?;
        Ok(pl)
    }

    /// The place a pointer value points to. Does not load or read the pointee: `&raw const *p`
    /// may form a place from a null, integer, uninitialized, or dangling pointer without
    /// dereferencing it; reads, writes, non-zero projections, and safe references check
    /// provenance, liveness, and bounds.
    fn deref(&self, v: Val) -> R<'a, Place> {
        Ok(match v {
            Val::Ptr(p) => Place::of(p),
            Val::Int(addr) => Place::from_int(addr, None),
            Val::Pair(pair) => {
                let Scalar::Int(n) = pair.second() else {
                    return Err(malformed("a dereference of a value that is not a pointer"));
                };
                let meta = Some(u64::try_from(n).map_err(|_| malformed("a wide pointer's length past `u64`"))?);
                match pair.first() {
                    Scalar::Ptr(p) => Place::with_meta(p, meta),
                    Scalar::Int(addr) => {
                        let addr = u64::try_from(addr).map_err(|_| Stop::Refused(Refusal::IntAsPointer))?;
                        Place::from_int(addr, meta)
                    }
                }
            }
            Val::Int128(_) => return Err(Stop::Refused(Refusal::IntAsPointer)),
            _ => return Err(malformed("a dereference of a value that is not a pointer")),
        })
    }

    /// Offsets a place by `off` bytes (field or index projection), replacing its metadata with
    /// `meta`. A zero-byte projection preserves provenance-free and dangling places as Rust's
    /// `&raw const (*p).first_field` allows; a non-zero projection requires a live allocation
    /// and (when its size is loaded) stays within `0..=len`.
    fn place_offset(&self, pl: Place, off: u64, meta: Option<u64>) -> R<'a, Place> {
        if off == 0 {
            return Ok(Place { alloc: pl.alloc, has_meta: meta.is_some(), offset: pl.offset, meta: meta.unwrap_or(0) });
        }
        let ptr = pl.ptr()?;
        let a = self.work.mem.get(ptr.alloc)?;
        if !a.live {
            return Err(Stop::Refused(Refusal::Dangling));
        }
        let new_off = ptr.offset.checked_add(off).ok_or(Stop::Refused(Refusal::OutOfBounds {
            offset: u64::MAX,
            size: 0,
            len: a.len(),
        }))?;
        if a.loaded && new_off > a.len() {
            return Err(Stop::Refused(Refusal::OutOfBounds { offset: new_off, size: 0, len: a.len() }));
        }
        Ok(Place::with_meta(Pointer { alloc: ptr.alloc, offset: new_off }, meta))
    }

    /// A reference (`!raw`) or raw pointer (`raw`) to `pl`. A constant's value used as a place
    /// is a fresh temporary at each use (the Reference, "Constant items": "constants are
    /// essentially inlined wherever they are used"; "Temporaries"): borrowing it copies its
    /// decoded record into a temporary of its own, so two borrows of one constant are two
    /// allocations, as two uses may be in the program, and a write through `&mut` or `*mut`
    /// reaches only its own copy. Reads take the record in place.
    fn borrow(&mut self, mut pl: Place, raw: bool) -> R<'a, Val> {
        if pl.alloc == NO_ALLOC {
            if !raw {
                return Err(Stop::Refused(Refusal::IntAsPointer));
            }
            return Ok(match pl.meta() {
                Some(n) => Val::pair(Scalar::Int(u128::from(pl.offset)), Scalar::Int(u128::from(n))),
                None => Val::Int(pl.offset),
            });
        }
        let id = AllocId(pl.alloc);
        if !raw && !self.work.mem.get(id)?.live {
            return Err(Stop::Refused(Refusal::Dangling));
        }
        if self.work.decoded.iter().any(|&(_, d)| d == id) {
            let (size, align) = {
                let a = self.work.mem.get(id)?;
                (a.len(), a.align_log2)
            };
            let copy = self.work.mem.allocate(size, align, true, Origin::Data)?;
            self.work.temps.push(copy);
            self.work.mem.copy(Pointer { alloc: id, offset: 0 }, Pointer { alloc: copy, offset: 0 }, size)?;
            pl.alloc = copy.0;
        }
        let ptr = Pointer { alloc: AllocId(pl.alloc), offset: pl.offset };
        Ok(match pl.meta() {
            Some(n) => Val::pair(Scalar::Ptr(ptr), Scalar::Int(u128::from(n))),
            None => Val::Ptr(ptr),
        })
    }

    /// The pointer to fn instance `named`: one zero-sized allocation per instance key an
    /// evaluation, which remembers the instance so the record can name it ([`ConstFn`]).
    fn fn_ptr(&mut self, named: ConstFn) -> R<'a, Pointer> {
        let id = match self.work.fns.iter().find(|(f, _)| f.key == named.key) {
            Some(&(_, id)) => id,
            None => {
                let id = self.work.mem.allocate(0, 0, false, Origin::Fn(named.key))?;
                self.work.fns.push((named, id));
                id
            }
        };
        Ok(Pointer { alloc: id, offset: 0 })
    }

    fn static_alloc(&mut self, item: ItemKey, mutable: bool) -> R<'a, AllocId> {
        if let Some(&(_, id)) = self.work.statics.iter().find(|(k, _)| *k == item) {
            if mutable {
                self.work.mem.get_mut(id)?.mut_static = true;
            }
            return Ok(id);
        }
        let id = self.work.mem.allocate(0, 0, false, Origin::Static(item))?;
        let a = self.work.mem.get_mut(id)?;
        a.loaded = false;
        a.mut_static = mutable;
        self.work.statics.push((item, id));
        Ok(id)
    }

    /// A record's allocations in memory: the root at `root` (a static's own) or a new one. A
    /// fn word's instance is the one the record names ([`ConstRecord::fns`]): its arguments are
    /// of this evaluation's ground table, as every record of one evaluator's world is (the
    /// records are made over the one table the evaluator keeps: blaze-ladder front2/consts.rs
    /// `Consts::unit`, run.rs `eval_consts`).
    fn decode<A: core::alloc::Allocator>(&mut self, record: &ConstRecord<A>, root: Option<AllocId>) -> R<'a, AllocId> {
        let rec = &record.data;
        let mut ids: Vec<AllocId, Alloc<'a>> = Vec::new_in(self.alloc);
        for (i, a) in rec.allocs.iter().enumerate() {
            let id = match (i, root) {
                (0, Some(r)) => {
                    self.work.mem.resize_unloaded(r, a.bytes.len() as u64)?;
                    self.work.mem.get_mut(r)?.align_log2 = a.align;
                    r
                }
                _ => self.work.mem.allocate(a.bytes.len() as u64, a.align, false, Origin::Data)?,
            };
            ids.push(id);
        }
        let mut relocs: Vec<(u64, AllocId), Alloc<'a>> = Vec::new_in(self.alloc);
        for (i, a) in rec.allocs.iter().enumerate() {
            relocs.clear();
            for r in a.relocs.iter() {
                let target = match r.target {
                    DataTarget::Local(j) => *ids.get(j as usize).ok_or(malformed("a relocation past its record"))?,
                    DataTarget::Static(k) => self.static_alloc(k, false)?,
                    DataTarget::Fn(key) => {
                        let named = record.fn_named(key).ok_or(malformed("a record's fn word whose instance the record does not name"))?;
                        self.fn_ptr(named)?.alloc
                    }
                    // A `&dyn Tr` in a const (codegen.md, "Trait objects"): the builder refuses
                    // it in a const's record, so none reaches here yet; refused by name.
                    DataTarget::Vtable(_) => return Err(unsupported("a vtable's address in a constant (a `&dyn` in a const)")),
                };
                relocs.push((u64::from(r.offset), target));
            }
            self.work.mem.fill(ids[i], &a.bytes, &relocs)?;
        }
        Ok(ids[0])
    }

    /// The place of constant `item`'s value (record `key`), its record decoded once per
    /// evaluation.
    fn const_place(&mut self, item: ItemKey, args: List, key: RecordKey) -> R<'a, Place> {
        if let Some(&(_, id)) = self.work.decoded.iter().find(|(k, _)| *k == key) {
            return Ok(Place::of(Pointer { alloc: id, offset: 0 }));
        }
        let world = self.world;
        let rec = world.record(key).map_err(|_| Stop::Need(Need { kind: NeedKind::Value, item, args }))?;
        self.read_add(key.0);
        let id = self.decode(rec, None)?;
        self.work.decoded.push((key, id));
        Ok(Place::of(Pointer { alloc: id, offset: 0 }))
    }

    /// `ptr_guaranteed_cmp` (`<*const T>::guaranteed_eq`/`_ne`, `is_null`): 1 when the two
    /// addresses are equal in every run, 0 when they differ in every run, 2 when this
    /// evaluation cannot know (the intrinsic's documented contract: never wrong, unknown when
    /// not certain). `==` and `<` on pointers stay refused (`PointerCompare`): the Reference
    /// allows comparison operators in constants only on integers, floats, `bool` and `char`.
    fn guaranteed_cmp(&mut self, x: Scalar, y: Scalar) -> R<'a, u128> {
        Ok(match (x, y) {
            (Scalar::Int(i), Scalar::Int(j)) => u128::from(i == j),
            // No allocation is at address 0, so a pointer into one or one past its end is not
            // null; past that, its address is not known.
            (Scalar::Ptr(p), Scalar::Int(i)) | (Scalar::Int(i), Scalar::Ptr(p)) => {
                if self.work.mem.get(p.alloc)?.mut_static {
                    if i == 0 && p.offset == 0 { 0 } else { 2 }
                } else {
                    self.ensure_loaded(p.alloc)?;
                    let len = self.work.mem.get(p.alloc)?.len();
                    if i == 0 && p.offset <= len { 0 } else { 2 }
                }
            }
            // One allocation: one base, so the addresses differ exactly when the offsets do.
            (Scalar::Ptr(p), Scalar::Ptr(q)) if p.alloc == q.alloc => u128::from(p.offset == q.offset),
            // Two allocations: distinct objects alive together do not overlap, so two pointers
            // strictly inside them differ, when at least one has an address of its own (a live
            // local of this evaluation, a static). A temporary or another constant's data may
            // become data merged with an equal value in the program, a fn target may be merged
            // with an equal fn, and one past the end of one may be the start of another: not
            // known.
            (Scalar::Ptr(p), Scalar::Ptr(q)) => {
                if self.work.mem.get(p.alloc)?.mut_static || self.work.mem.get(q.alloc)?.mut_static {
                    return Ok(2);
                }
                self.ensure_loaded(p.alloc)?;
                self.ensure_loaded(q.alloc)?;
                let (pa, qa) = (self.work.mem.get(p.alloc)?, self.work.mem.get(q.alloc)?);
                let own = |a: &crate::mem::Allocation| match a.origin {
                    Origin::Local => a.live,
                    Origin::Static(_) => true,
                    Origin::Data | Origin::Fn(_) => false,
                };
                let inside = p.offset < pa.len() && q.offset < qa.len();
                if inside && (own(pa) || own(qa)) { 0 } else { 2 }
            }
        })
    }

    /// The variant of the enum value at `pl`, its tag read and decoded by `facts`: no layout
    /// lookup.
    fn variant_of(&mut self, pl: Place, facts: &EnumFacts, discrs: &[i128]) -> R<'a, u32> {
        let ptr = pl.ptr()?;
        let bits = match facts.tag_at() {
            Some((off, size)) => match self.read(ptr.at(off), size)? {
                Scalar::Int(bits) => bits,
                Scalar::Ptr(p) => {
                    let in_bounds = if self.work.mem.get(p.alloc)?.mut_static {
                        p.offset == 0
                    } else {
                        self.ensure_loaded(p.alloc)?;
                        p.offset <= self.work.mem.get(p.alloc)?.len()
                    };
                    if in_bounds && let Some(v) = facts.ptr_variant() {
                        return Ok(v);
                    }
                    return Err(Stop::Refused(Refusal::PointerAsInt));
                }
            },
            None => {
                self.ensure_loaded(ptr.alloc)?;
                self.work.mem.check_access(ptr, 0)?;
                0
            }
        };
        Ok(facts.variant(bits, discrs)?)
    }

    // ---- Operators ----

    fn unary(&mut self, op: UnOp, num: Num, v: Val) -> R<'a, Val> {
        let Some(a) = v.as_int() else { return Err(Stop::Refused(Refusal::PointerAsInt)) };
        let res = match (op, num) {
            (UnOp::Not, Num::Bool) => a ^ 1,
            (UnOp::Not, n) => {
                let (bits, _) = n.int().ok_or(malformed("`!` of a type that is not an integer"))?;
                mask(!a, bits)
            }
            (UnOp::Neg, Num::F32) => a ^ (1 << 31),
            (UnOp::Neg, Num::F64) => a ^ (1 << 63),
            (UnOp::Neg, Num::F16) => a ^ Format::of(FloatTy::F16).sign(),
            (UnOp::Neg, Num::F128) => a ^ Format::of(FloatTy::F128).sign(),
            (UnOp::Neg, n) => {
                let (bits, signed) = n.int().ok_or(malformed("`-` of a type that is not a number"))?;
                if !signed {
                    return Err(malformed("`-` of an unsigned integer"));
                }
                if sext(a, bits) == smin(bits) && self.plant != Plant::WrapOnOverflow {
                    return Err(Stop::Refused(Refusal::Overflow { op: Arith::Neg }));
                }
                mask(a.wrapping_neg(), bits)
            }
            (UnOp::Deref, _) => return Err(malformed("a deref as arithmetic")),
        };
        Ok(Val::int(res))
    }

    fn binary(&mut self, op: BinOp, num: Num, rnum: Num, l: Val, r: Val) -> R<'a, Val> {
        let (a, b) = match (l.as_int(), r.as_int()) {
            (Some(a), Some(b)) => (a, b),
            _ => match (l, r) {
                (Val::Ptr(p), Val::Ptr(q)) if self.plant == Plant::ComparePointers => {
                    let eq = p == q;
                    return Ok(Val::Int(u64::from(match op {
                        BinOp::Eq => eq,
                        BinOp::Ne => !eq,
                        _ => p.offset < q.offset,
                    })));
                }
                (Val::Int(_) | Val::Int128(_) | Val::Ptr(_) | Val::Pair(_), Val::Int(_) | Val::Int128(_) | Val::Ptr(_) | Val::Pair(_))
                    if matches!(op, BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge) =>
                {
                    return Err(Stop::Refused(Refusal::PointerCompare));
                }
                (Val::Ptr(_), _) | (_, Val::Ptr(_)) => return Err(Stop::Refused(Refusal::PointerAsInt)),
                _ => return Err(unsupported("an operator on a type that is not a scalar (a const trait)")),
            },
        };
        if let Some(f) = num.float() {
            return Ok(Val::int(crate::arith::float_binop(op, f, a, b)?));
        }
        let (bits, signed) = match num.int() {
            Some(info) => info,
            None if num == Num::Bool => (8, false),
            None => return Err(unsupported("an operator on this type")),
        };
        let rhs = match rnum.int() {
            Some(info) => info,
            None if rnum == Num::Bool => (8, false),
            None => (bits, signed),
        };
        Ok(Val::int(int_binop(op, a, b, bits, signed, rhs, self.plant)?))
    }

    /// `as` (the Reference, "Type cast expressions"): pointers here, every number in
    /// [`crate::arith::num_cast`].
    fn cast(&mut self, v: Val, from: PTy, to: PTy) -> R<'a, Val> {
        let (fnode, tnode) = (self.node(from)?, self.node(to)?);
        match (fnode, tnode) {
            (PNode::Ref(..) | PNode::Ptr(..) | PNode::FnPtr(..), PNode::Ptr(_, p)) => {
                return Ok(match (v, layout::is_unsized(self.world, self.ground, p, self.alloc)?) {
                    (Val::Pair(pair), false) => Val::scalar(pair.first()),
                    (v, _) => v,
                });
            }
            (PNode::Ptr(..) | PNode::FnPtr(..) | PNode::Ref(..), PNode::Int(_) | PNode::Uint(_)) => {
                return match v.as_int() {
                    Some(a) => Ok(Val::int(a)),
                    None => Err(Stop::Refused(Refusal::PointerAsInt)),
                };
            }
            // A fieldless enum to its discriminant is `Op::Discr`.
            (PNode::Adt(..), _) => return Err(malformed("an enum cast compiled as a scalar cast")),
            _ => {}
        }
        let Some(a) = v.as_int() else { return Err(Stop::Refused(Refusal::PointerAsInt)) };
        Ok(Val::int(crate::arith::num_cast(fnode, tnode, a)?))
    }

    // ---- Calls ----

    fn panic_part(&mut self, v: Val, t: PTy) -> R<'a, PanicPart<'a>> {
        let node = self.node(t)?;
        Ok(match (node, v) {
            (PNode::Ref(_, s), Val::Pair(pair)) if self.node(s)? == PNode::Str => {
                let Scalar::Int(n) = pair.second() else { return Ok(PanicPart::Other) };
                let ptr = self.deref(Val::Pair(pair))?.ptr()?;
                self.ensure_loaded(ptr.alloc)?;
                let slice = self.work.mem.read_bytes(ptr, n as u64)?;
                let mut bytes = Vec::with_capacity_in(slice.len(), self.alloc);
                bytes.extend_from_slice(slice);
                PanicPart::Str(bytes)
            }
            (PNode::Int(i), v) if v.as_int().is_some() => PanicPart::Int(sext(v.as_int().unwrap(), int_bits(i))),
            (PNode::Uint(_), v) if v.as_int().is_some() => PanicPart::Uint(v.as_int().unwrap()),
            _ => PanicPart::Other,
        })
    }

    /// An intrinsic's result: `rt` the call's type, `args` its generic arguments.
    fn intrinsic(&mut self, i: Intrinsic, args: List, rt: PTy, vals: &[(Val, PTy)]) -> R<'a, Val> {
        let (t0, t1) = {
            let list = self.ground.view().list(args).map_err(|_| malformed("arguments past the ground table"))?;
            (list.first().copied(), list.get(1).copied())
        };
        let int = |k: usize| -> R<'a, u128> {
            match vals.get(k) {
                Some((v, _)) => match v.as_int() {
                    Some(a) => Ok(a),
                    None if matches!(v, Val::Ptr(_)) => Err(Stop::Refused(Refusal::PointerAsInt)),
                    None => Err(malformed("an intrinsic's argument that is not an integer")),
                },
                None => Err(malformed("an intrinsic's argument that is not an integer")),
            }
        };
        let info = |m: &Self, t: PTy| -> R<'a, (u32, bool)> { int_info(m.node(t)?).ok_or(malformed("an intrinsic of a type that is not an integer")) };
        let s = |v: u128| Ok(Val::int(v));
        let wrap = |op: BinOp, a: u128, b: u128, bits: u32| -> u128 {
            mask(
                match op {
                    BinOp::Add => a.wrapping_add(b),
                    BinOp::Sub => a.wrapping_sub(b),
                    _ => a.wrapping_mul(b),
                },
                bits,
            )
        };
        match i {
            Intrinsic::SizeOf | Intrinsic::AlignOf => {
                let t = t0.ok_or(malformed("`size_of` with no type"))?;
                let (size, align) = self.size_align(t)?;
                s(if i == Intrinsic::SizeOf { u128::from(size) } else { 1u128 << align })
            }
            // `transmute_unchecked` is `transmute` without the compile-time size check (the
            // pinned core, library/core/src/intrinsics/mod.rs: different sizes are "Undefined
            // Behavior at runtime"): in a constant that is refused, so both take one arm.
            Intrinsic::Transmute | Intrinsic::TransmuteUnchecked => {
                let (from, to) = (t0.ok_or(malformed("`transmute` types"))?, t1.ok_or(malformed("`transmute` types"))?);
                let (fs, ts) = (self.size_align(from)?.0, self.size_align(to)?.0);
                if fs != ts {
                    return Err(Stop::Refused(Refusal::TransmuteSize { from: fs, to: ts }));
                }
                let (v, _) = *vals.first().ok_or(malformed("`transmute` with no value"))?;
                let pl = self.spill(v, from)?;
                self.load(pl, to)
            }
            Intrinsic::WrappingAdd | Intrinsic::WrappingSub | Intrinsic::WrappingMul => {
                let (bits, _) = info(self, rt)?;
                let op = match i {
                    Intrinsic::WrappingAdd => BinOp::Add,
                    Intrinsic::WrappingSub => BinOp::Sub,
                    _ => BinOp::Mul,
                };
                s(wrap(op, int(0)?, int(1)?, bits))
            }
            Intrinsic::AddWithOverflow | Intrinsic::SubWithOverflow | Intrinsic::MulWithOverflow => {
                let t = vals.first().ok_or(malformed("an overflow intrinsic's arguments"))?.1;
                let (bits, signed) = info(self, t)?;
                let op = match i {
                    Intrinsic::AddWithOverflow => BinOp::Add,
                    Intrinsic::SubWithOverflow => BinOp::Sub,
                    _ => BinOp::Mul,
                };
                let (a, b) = (int(0)?, int(1)?);
                let over = int_binop(op, a, b, bits, signed, (bits, signed), Plant::None).is_err();
                let r = wrap(op, a, b, bits);
                let bool_t = self.field_ty(rt, 0, 1)?;
                self.build_tuple(rt, &[(Val::int(r), t), (Val::Int(u64::from(over)), bool_t)])
            }
            Intrinsic::UncheckedAdd | Intrinsic::UncheckedSub | Intrinsic::UncheckedMul | Intrinsic::UncheckedShl | Intrinsic::UncheckedShr | Intrinsic::ExactDiv => {
                let t = vals.first().ok_or(malformed("an unchecked intrinsic's arguments"))?.1;
                let (bits, signed) = info(self, t)?;
                let rhs = info(self, vals.get(1).ok_or(malformed("an unchecked intrinsic's arguments"))?.1)?;
                let op = match i {
                    Intrinsic::UncheckedAdd => BinOp::Add,
                    Intrinsic::UncheckedSub => BinOp::Sub,
                    Intrinsic::UncheckedMul => BinOp::Mul,
                    Intrinsic::UncheckedShl => BinOp::Shl,
                    Intrinsic::UncheckedShr => BinOp::Shr,
                    _ => BinOp::Div,
                };
                let (a, b) = (int(0)?, int(1)?);
                if i == Intrinsic::ExactDiv && b != 0 && int_binop(BinOp::Rem, a, b, bits, signed, rhs, Plant::None).is_ok_and(|r| r != 0) {
                    return Err(Stop::Refused(Refusal::Overflow { op: Arith::Div }));
                }
                s(int_binop(op, a, b, bits, signed, rhs, Plant::None)?)
            }
            Intrinsic::SaturatingAdd | Intrinsic::SaturatingSub => {
                let (bits, signed) = info(self, rt)?;
                let (a, b) = (int(0)?, int(1)?);
                let op = if i == Intrinsic::SaturatingAdd { BinOp::Add } else { BinOp::Sub };
                s(match int_binop(op, a, b, bits, signed, (bits, signed), Plant::None) {
                    Ok(r) => r,
                    Err(_) if signed => {
                        let up = if op == BinOp::Add { sext(b, bits) > 0 } else { sext(b, bits) < 0 };
                        mask(if up { smax(bits) } else { smin(bits) } as u128, bits)
                    }
                    Err(_) => {
                        if op == BinOp::Add {
                            umax(bits)
                        } else {
                            0
                        }
                    }
                })
            }
            Intrinsic::RotateLeft | Intrinsic::RotateRight => {
                let (bits, _) = info(self, rt)?;
                let (a, n) = (int(0)?, int(1)? % u128::from(bits));
                let n = n as u32;
                let n = if i == Intrinsic::RotateLeft { n } else { (bits - n) % bits };
                s(if n == 0 { a } else { mask((a << n) | (a >> (bits - n)), bits) })
            }
            Intrinsic::Ctpop | Intrinsic::Ctlz | Intrinsic::CtlzNonzero | Intrinsic::Cttz | Intrinsic::CttzNonzero | Intrinsic::Bswap | Intrinsic::Bitreverse => {
                let t = vals.first().ok_or(malformed("a bit intrinsic's argument"))?.1;
                let (bits, _) = info(self, t)?;
                let a = int(0)?;
                // The pinned core (intrinsics/mod.rs): `ctlz_nonzero` and `cttz_nonzero` are
                // `ctlz` and `cttz` except "returns `undef` when given an `x` with value `0`":
                // undefined behaviour, which a constant refuses (rustc's const evaluator,
                // interpret/intrinsics.rs: "`{name}` called on 0"). Before (tempo t1935d, 7
                // lines): ruint's `LowerHex` constant refused "ctlz_nonzero is not evaluated yet".
                if a == 0 && matches!(i, Intrinsic::CtlzNonzero | Intrinsic::CttzNonzero) {
                    let name = if i == Intrinsic::CtlzNonzero { "ctlz_nonzero" } else { "cttz_nonzero" };
                    return Err(Stop::Refused(Refusal::NonzeroOnZero { name }));
                }
                s(match i {
                    Intrinsic::Ctpop => u128::from(a.count_ones()),
                    Intrinsic::Ctlz | Intrinsic::CtlzNonzero => u128::from(a.leading_zeros() - (128 - bits)),
                    Intrinsic::Cttz | Intrinsic::CttzNonzero => u128::from(a.trailing_zeros().min(bits)),
                    Intrinsic::Bswap => a.swap_bytes() >> (128 - bits),
                    _ => a.reverse_bits() >> (128 - bits),
                })
            }
            Intrinsic::Offset => {
                let &(pv, pt) = vals.first().ok_or(malformed("`offset` arguments"))?;
                let count = int(1)?;
                let (ctbits, csigned) = info(self, vals.get(1).ok_or(malformed("`offset` arguments"))?.1)?;
                let (PNode::Ptr(_, pointee) | PNode::Ref(_, pointee)) = self.node(pt)? else { return Err(malformed("`offset` of a value that is not a pointer")) };
                let (size, _) = self.size_align(pointee)?;
                let Val::Ptr(p) = pv else { return Err(Stop::Refused(Refusal::IntAsPointer)) };
                let count_signed = if csigned {
                    sext(count, ctbits)
                } else {
                    i128::try_from(count).map_err(|_| Stop::Refused(Refusal::OutOfBounds { offset: u64::MAX, size: 0, len: 0 }))?
                };
                if count_signed == 0 && self.work.mem.get(p.alloc)?.mut_static {
                    return Ok(Val::Ptr(p));
                }
                self.ensure_loaded(p.alloc)?;
                let len = self.work.mem.get(p.alloc)?.len();
                let Some(delta) = count_signed.checked_mul(i128::from(size)) else {
                    return Err(Stop::Refused(Refusal::OutOfBounds { offset: u64::MAX, size: 0, len }));
                };
                if delta.unsigned_abs() > isize::MAX as u128 {
                    return Err(Stop::Refused(Refusal::OutOfBounds { offset: u64::MAX, size: 0, len }));
                }
                let Some(off) = i128::from(p.offset).checked_add(delta) else {
                    return Err(Stop::Refused(Refusal::OutOfBounds { offset: u64::MAX, size: 0, len }));
                };
                if off < 0 || off > i128::from(len) {
                    let clamped = u64::try_from(off).unwrap_or(if off < 0 { 0 } else { u64::MAX });
                    return Err(Stop::Refused(Refusal::OutOfBounds { offset: clamped, size: 0, len }));
                }
                Ok(Val::Ptr(Pointer { alloc: p.alloc, offset: off as u64 }))
            }
            Intrinsic::DiscriminantValue => {
                Err(malformed("`discriminant_value` not compiled to its enum's facts"))
            }
            Intrinsic::Unreachable => Err(Stop::Refused(Refusal::Unreachable)),
            Intrinsic::Assume => {
                if int(0)? == 0 {
                    return Err(Stop::Refused(Refusal::Unreachable));
                }
                Ok(Val::Zst)
            }
            Intrinsic::BlackBox | Intrinsic::Likely | Intrinsic::Unlikely => Ok(vals.first().ok_or(malformed("`black_box` or `likely` argument"))?.0),
            Intrinsic::ColdPath => Ok(Val::Zst),
            // The pinned core (intrinsics/mod.rs, `ub_checks`): "In consteval, this function
            // currently returns `true`."
            Intrinsic::UbChecks => s(1),
            // The pinned core (intrinsics/mod.rs, `overflow_checks`): "In consteval, this
            // function currently returns `true`. This is because the value of the
            // `overflow_checks` configuration can differ across crates", so a constant never
            // reads the build's setting.
            Intrinsic::OverflowChecks => s(1),
            Intrinsic::PtrMetadata => {
                let &(v, t) = vals.first().ok_or(malformed("`ptr_metadata` argument"))?;
                let v = match v {
                    Val::Mem(p) => self.load(Place::of(p), t)?,
                    v => v,
                };
                match v {
                    Val::Pair(pair) => Ok(Val::scalar(pair.second())),
                    Val::Int(_) | Val::Int128(_) | Val::Ptr(_) => Ok(Val::Zst),
                    _ => Err(malformed("`ptr_metadata` of a value that is not a pointer")),
                }
            }
            Intrinsic::AggregateRawPtr => {
                let data = vals.first().ok_or(malformed("`aggregate_raw_ptr` arguments"))?.0;
                let meta = vals.get(1).ok_or(malformed("`aggregate_raw_ptr` arguments"))?.0;
                let Some(d) = data.as_scalar() else { return Err(malformed("`aggregate_raw_ptr` of a data pointer that is not a scalar")) };
                match meta {
                    Val::Zst => Ok(Val::scalar(d)),
                    v if v.as_scalar().is_some() => Ok(Val::pair(d, v.as_scalar().unwrap())),
                    _ => Err(malformed("`aggregate_raw_ptr` metadata that is neither `()` nor one scalar")),
                }
            }
            Intrinsic::PtrGuaranteedCmp => {
                let a = vals.first().ok_or(malformed("`ptr_guaranteed_cmp` arguments"))?.0;
                let b = vals.get(1).ok_or(malformed("`ptr_guaranteed_cmp` arguments"))?.0;
                let (Some(x), Some(y)) = (a.as_scalar(), b.as_scalar()) else { return s(2) };
                s(self.guaranteed_cmp(x, y)?)
            }
            // The pinned core, library/core/src/intrinsics/mod.rs: "Returns `true` if the actual
            // type given as `T` requires drop glue": the one rule (blaze-build drop_facts.rs).
            Intrinsic::NeedsDrop => {
                let t = t0.ok_or(malformed("`needs_drop` with no type"))?;
                s(u128::from(crate::drop_ask::needs_drop(self.world, t, self.ground, self.alloc)?))
            }
            // Compiled as a call of its const fn.
            Intrinsic::ConstEvalSelect => Err(malformed("`const_eval_select` run as an intrinsic")),
            // The pinned core, library/core/src/intrinsics/mod.rs: "Gets a static string slice
            // containing the name of a type": the ground type printed by the one printer
            // (blaze-build type_name.rs), its bytes cached once per ground type per evaluation.
            Intrinsic::TypeName => {
                let t = t0.ok_or(malformed("`type_name` with no type"))?;
                let (id, len) = match self.work.type_names.iter().find(|&&(ty, _, _)| ty == t) {
                    Some(&(_, id, len)) => (id, len),
                    None => {
                        let world = self.world;
                        let names = world.type_names().ok_or(unsupported("`type_name` in a world that names no items (`ConstWorld::type_names`)"))?;
                        let text = blaze_build::type_name::type_name_text(self.ground.view(), t, names, self.alloc).map_err(|error| match error {
                            blaze_build::type_name::TypeNameError::NoPath { item } => Stop::Refused(Refusal::Missing { what: "the path of an item `type_name` prints", item }),
                            other => unsupported(other.what()),
                        })?;
                        let len = text.len() as u64;
                        let id = self.work.mem.allocate(len, 0, true, Origin::Data)?;
                        self.work.mem.fill(id, text.as_bytes(), &[])?;
                        self.work.mem.get_mut(id)?.mutable = false;
                        self.work.type_names.push((t, id, len));
                        (id, len)
                    }
                };
                Ok(Val::pair(Scalar::Ptr(Pointer { alloc: id, offset: 0 }), Scalar::Int(u128::from(len))))
            }
            // The pinned core, library/core/src/intrinsics/reflection.rs: "Gets an identifier which
            // is globally unique to the specified type". The one function the builder calls too
            // (blaze-build instance.rs `type_id_hash`), its 128 bits laid out as `TypeId`'s two
            // `data` words, little-endian, so `TypeId::as_u128` reads them back whole.
            Intrinsic::TypeId => {
                let t = t0.ok_or(malformed("`type_id` with no type"))?;
                let bits = blaze_build::instance::type_id_hash(t, self.ground.view(), self.alloc)
                    .map_err(|_| unsupported("`type_id` of a type that is not ground (an alias, parameter or inference variable left)"))?;
                let info = self.tyinfo(rt)?;
                if info.repr != Repr::Mem(16) {
                    return Err(malformed("a `type_id` result that is not `TypeId`'s 16 bytes"));
                }
                let p = self.temp_i(info)?;
                self.work.mem.write(p, 8, Scalar::Int(bits & u128::from(u64::MAX)))?;
                self.work.mem.write(p.at(8), 8, Scalar::Int(bits >> 64))?;
                Ok(Val::Mem(p))
            }
            // blaze's `blaze_int_cast::<S, D>(x)` (library/core/src/intrinsics/mod.rs): "`x as D`
            // between two integer types of any width ... it truncates to `D`'s width, or extends
            // by `S`'s sign": the one rule (blaze-build intrinsics.rs `int_cast_bits`).
            Intrinsic::BlazeIntCast => {
                let (from, to) = (t0.ok_or(malformed("`blaze_int_cast` types"))?, t1.ok_or(malformed("`blaze_int_cast` types"))?);
                let ((fb, fs), (tb, _)) = (info(self, from)?, info(self, to)?);
                s(blaze_build::intrinsics::int_cast_bits(int(0)?, fb, fs, tb))
            }
            // blaze's structural reflection intrinsics: the one rule in blaze-build intrinsics.rs
            // (`field_count`, `records_disjoint`, `record_schema_id`).
            Intrinsic::BlazeFieldCount => {
                let t = t0.ok_or(malformed("`blaze_field_count` with no type"))?;
                let count = blaze_build::intrinsics::field_count(t, self.ground.view(), self.world).map_err(|e| match e {
                    blaze_build::intrinsics::FieldCountError::MissingType => malformed("`blaze_field_count` of a type not in the ground table"),
                    blaze_build::intrinsics::FieldCountError::NoAdt(item) => Stop::Refused(Refusal::Missing { what: "ADT definition", item }),
                })?;
                s(u128::from(count))
            }
            Intrinsic::BlazeRecordsDisjoint => {
                let (a, b) = (t0.ok_or(malformed("`blaze_records_disjoint` types"))?, t1.ok_or(malformed("`blaze_records_disjoint` types"))?);
                let disjoint = blaze_build::intrinsics::records_disjoint(a, b, self.ground.view()).map_err(|e| match e {
                    blaze_build::intrinsics::DisjointError::MissingType => malformed("`blaze_records_disjoint` of a type not in the ground table"),
                    blaze_build::intrinsics::DisjointError::MissingRecordNames => malformed("`blaze_records_disjoint` of a record without registered schema field names"),
                })?;
                s(u128::from(disjoint))
            }
            Intrinsic::BlazeRecordSchemaId => {
                let t = t0.ok_or(malformed("`blaze_record_schema_id` with no type"))?;
                let id = blaze_build::intrinsics::record_schema_id(t, self.ground.view())
                    .ok_or(malformed("`blaze_record_schema_id` of a type not in the ground table"))?;
                s(u128::from(id))
            }
            // `raw_eq::<T>(a, b)` ("Determines whether the raw bytes of the two values are
            // equal") and `compare_bytes(left, right, bytes)` ("Lexicographically compare ... as
            // unsigned bytes"): one bounds, init and provenance check per slice, then direct
            // slice comparison with zero heap allocations.
            Intrinsic::RawEq | Intrinsic::CompareBytes => {
                let (a, b) = (vals.first().ok_or(malformed("a byte comparison's arguments"))?.0, vals.get(1).ok_or(malformed("a byte comparison's arguments"))?.0);
                let n = match i {
                    Intrinsic::RawEq => self.size_align(t0.ok_or(malformed("`raw_eq` with no type"))?)?.0,
                    _ => u64::try_from(int(2)?).map_err(|_| malformed("a `usize` past 64 bits"))?,
                };
                let (pa, pb) = (self.deref(a)?.ptr()?, self.deref(b)?.ptr()?);
                self.ensure_loaded(pa.alloc)?;
                self.ensure_loaded(pb.alloc)?;
                let left = self.work.mem.read_bytes(pa, n)?;
                let right = self.work.mem.read_bytes(pb, n)?;
                let order = blaze_build::intrinsics::byte_order(left, right);
                match i {
                    Intrinsic::RawEq => s(u128::from(order == 0)),
                    _ => s(u128::from(order as u32)),
                }
            }
            // A row of the table the evaluator does not compute: refused by its name.
            other => Err(unsupported(other.entry().name)),
        }
    }

    fn build_tuple(&mut self, t: PTy, parts: &[(Val, PTy)]) -> R<'a, Val> {
        let info = self.tyinfo(t)?;
        let p = self.temp_i(info)?;
        for (k, &(v, ft)) in parts.iter().enumerate() {
            let off = self.field_offset(t, 0, k as u32)?;
            self.store(Place::of(p.at(off)), ft, v)?;
        }
        Ok(Val::Mem(p))
    }

    /// Two values of type `t` compare equal (a literal or constant pattern).
    fn same(&mut self, a: Val, b: Val, t: PTy) -> R<'a, bool> {
        if let (Some(x), Some(y)) = (a.as_int(), b.as_int()) {
            return Ok(match self.node(t)? {
                PNode::Float(FloatTy::F64) => f64::from_bits(x as u64) == f64::from_bits(y as u64),
                PNode::Float(FloatTy::F32) => f32::from_bits(x as u32) == f32::from_bits(y as u32),
                PNode::Float(f @ (FloatTy::F16 | FloatTy::F128)) => Format::of(f).compare(x, y) == Some(core::cmp::Ordering::Equal),
                _ => x == y,
            });
        }
        Ok(match (a, b) {
            (Val::Pair(p), Val::Pair(q)) => {
                let (Scalar::Int(n), Scalar::Int(m)) = (p.second(), q.second()) else {
                    return Err(unsupported("a pattern compared with a value of this shape"));
                };
                if n != m {
                    return Ok(false);
                }
                let (pa, pb) = (self.deref(Val::Pair(p))?.ptr()?, self.deref(Val::Pair(q))?.ptr()?);
                self.ensure_loaded(pa.alloc)?;
                self.ensure_loaded(pb.alloc)?;
                let left = self.work.mem.read_bytes(pa, n as u64)?;
                let right = self.work.mem.read_bytes(pb, m as u64)?;
                left == right
            }
            (Val::Zst, Val::Zst) => true,
            _ => return Err(unsupported("a pattern compared with a value of this shape")),
        })
    }

    // ---- The result ----

    /// The value as a record: the root, then every allocation it reaches, in the order reached
    /// (rule 25: no address, no map order).
    /// `held`: a static's record marks the allocations written in place
    /// ([`Machine::written_in_place`]); a copied value's marks none.
    fn intern(&mut self, v: Val, t: PTy, held: Held) -> R<'a, DataRecord<Alloc<'a>>> {
        let (size, align) = self.size_align(t)?;
        // A value already a whole temporary of its size is the root as it is.
        let whole = match v {
            Val::Mem(p) if p.offset == 0 => {
                let a = self.work.mem.get(p.alloc)?;
                (a.live && a.origin == Origin::Data && a.len() == size).then_some(p.alloc)
            }
            _ => None,
        };
        let root = match whole {
            Some(id) => id,
            None => {
                let root = self.work.mem.allocate(size, align, true, Origin::Data)?;
                self.store(Place::of(Pointer { alloc: root, offset: 0 }), t, v)?;
                root
            }
        };
        let mut order: Vec<AllocId, Alloc<'a>> = Vec::new_in(self.alloc);
        order.push(root);
        // blaze-build's data form holds core's `Vec` in the caller's allocator.
        let mut allocs = alloc::vec::Vec::new_in(self.alloc);
        let mut next = 0;
        while let Some(&id) = order.get(next) {
            next += 1;
            self.ensure_loaded(id)?;
            let mut bytes = alloc::vec::Vec::new_in(self.alloc);
            bytes.extend_from_slice(self.work.mem.bytes(id)?);
            // Bytes never written (padding) are zero in the record.
            self.work.mem.zero_unwritten(id, &mut bytes);
            let a = self.work.mem.get(id)?;
            let mut relocs = alloc::vec::Vec::new_in(self.alloc);
            let pairs: Vec<(u64, AllocId), Alloc<'a>> = {
                let mut v = Vec::new_in(self.alloc);
                v.extend_from_slice(self.work.mem.relocs(id)?);
                v
            };
            let align_log2 = a.align_log2;
            for (off, target) in pairs.iter().copied() {
                let t = self.work.mem.get(target)?;
                let target = match t.origin {
                    Origin::Static(k) => DataTarget::Static(k),
                    Origin::Fn(key) => DataTarget::Fn(key),
                    Origin::Data | Origin::Local => {
                        if !t.live {
                            return Err(Stop::Refused(Refusal::Dangling));
                        }
                        let at = match order.iter().position(|&x| x == target) {
                            Some(i) => i,
                            None => {
                                order.push(target);
                                order.len() - 1
                            }
                        };
                        DataTarget::Local(at as u32)
                    }
                };
                relocs.push(DataReloc { offset: u32::try_from(off).map_err(|_| unsupported("a pointer past 4 GiB in a constant"))?, target });
            }
            allocs.push(DataAlloc { bytes, align: align_log2, mutable: false, relocs });
        }
        if held == Held::Static {
            let written = self.written_in_place(&allocs, t)?;
            for (a, w) in allocs.iter_mut().zip(written.iter()) {
                a.mutable = *w;
            }
        }
        Ok(DataRecord { allocs })
    }

    /// Which allocations of a static's record, of type `t`, are written in place: the
    /// Reference, "Static items": a static "may contain interior mutability", so it is not
    /// placed in read-only memory when its type is not `Freeze` (core/src/marker.rs); nor is an
    /// allocation it points to whose pointee type is not (`static D: &AtomicU8 =
    /// &AtomicU8::new(0)`: the temporary lives as long as the static and is written through
    /// `D`), nor one behind a `&mut` or `*mut` (`static mut S: &mut [u8] = &mut [1, 2]`).
    ///
    /// The walk follows `t` over the record's bytes, only into parts that hold a relocation:
    /// each struct, tuple and union field at its offset, an array's and a slice's elements, and
    /// EVERY variant of an enum (the record does not say which is live, so a pointer any
    /// variant could hold at an offset is typed by each; one that is not `Freeze` decides).
    /// An allocation the walk reaches by no typed pointer (one behind a trait object, an
    /// unsized struct, a closure's environment) is written in place: read-only is decided
    /// only by a type that proves it.
    fn written_in_place(&mut self, allocs: &[DataAlloc<Alloc<'a>>], t: PTy) -> R<'a, Vec<bool, Alloc<'a>>> {
        /// The most parts one walk visits (rule 18).
        const MOST_PARTS: usize = 1 << 20;
        let n = allocs.len();
        let mut written: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(n, self.alloc);
        let mut typed: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(n, self.alloc);
        for _ in 0..n {
            written.push(false);
            typed.push(false);
        }
        written[0] = self.interior_mut(t)?;
        typed[0] = true;
        let mut todo: Vec<(u32, u64, PTy), Alloc<'a>> = Vec::new_in(self.alloc);
        todo.push((0, 0, t));
        let mut parts = 0usize;
        let too_many = || unsupported("a static whose value has more than 2^20 parts holding pointers to walk");
        while let Some((i, base, ty)) = todo.pop() {
            parts += 1;
            if parts > MOST_PARTS || todo.len() > MOST_PARTS {
                return Err(too_many());
            }
            let a = allocs.get(i as usize).ok_or(malformed("a relocation to an allocation past the record"))?;
            let end = base.saturating_add(self.size_align(ty)?.0);
            if !a.relocs.iter().any(|r| (base..end).contains(&u64::from(r.offset))) {
                continue;
            }
            match self.node(ty)? {
                PNode::Ref(m, p) | PNode::Ptr(m, p) => {
                    let Some(r) = a.relocs.iter().find(|r| u64::from(r.offset) == base) else { continue };
                    let DataTarget::Local(j) = r.target else { continue };
                    let ju = j as usize;
                    let target = allocs.get(ju).ok_or(malformed("a relocation to an allocation past the record"))?;
                    // The word holds the addend: where in the target the pointee starts.
                    let addend = word(&a.bytes, base)?;
                    typed[ju] = true;
                    written[ju] |= m == Mutbl::Mut;
                    match self.node(p)? {
                        PNode::Str => {}
                        PNode::Slice(e) => {
                            written[ju] |= self.interior_mut(e)?;
                            let len = word(&a.bytes, base + PTR)?;
                            let es = self.size_align(e)?.0;
                            let to = addend.saturating_add(len.saturating_mul(es));
                            if es != 0 && target.relocs.iter().any(|r| (addend..to).contains(&u64::from(r.offset))) {
                                if len > MOST_PARTS as u64 {
                                    return Err(too_many());
                                }
                                for k in 0..len {
                                    todo.push((j, addend + k * es, e));
                                }
                            }
                        }
                        PNode::Dynamic(_) => written[ju] = true,
                        _ if layout::is_unsized(self.world, self.ground, p, self.alloc)? => written[ju] = true,
                        _ => {
                            written[ju] |= self.interior_mut(p)?;
                            todo.push((j, addend, p));
                        }
                    }
                }
                PNode::Tuple(l) | PNode::Record(_, l) => {
                    let len = self.ground.view().list(l).map_err(|_| malformed("a list past the ground table"))?.len() as u32;
                    for f in 0..len {
                        let (off, ft) = (self.field_offset(ty, 0, f)?, self.field_ty(ty, 0, f)?);
                        todo.push((i, base + off, ft));
                    }
                }
                PNode::Union(l) => {
                    let variants = l.len;
                    let (mut cx, lay) = self.cx();
                    let tag = lay.tag(ty, &mut cx)?;
                    let live = match tag.at {
                        None if variants == 0 => 0..0,
                        None => {
                            let index = tag.variant(0, &[])?;
                            index..index + 1
                        }
                        Some(_) => 0..variants,
                    };
                    for v in live {
                        let (off, ft) = (self.field_offset(ty, v, 0)?, self.field_ty(ty, v, 0)?);
                        todo.push((i, base + off, ft));
                    }
                }
                PNode::Array(e, _) => {
                    let es = self.size_align(e)?.0;
                    if es != 0 {
                        let count = (end - base) / es;
                        if count > MOST_PARTS as u64 {
                            return Err(too_many());
                        }
                        for k in 0..count {
                            todo.push((i, base + k * es, e));
                        }
                    }
                }
                PNode::Adt(key, _) => {
                    let variants = self.world.adt(key).ok_or(Stop::Refused(Refusal::Missing { what: "ADT definition", item: key }))?.variants.len() as u32;
                    // One laid-out variant (a struct, a union, an enum whose others are
                    // uninhabited): only its fields have offsets.
                    let (mut cx, l) = self.cx();
                    let tag = l.tag(ty, &mut cx)?;
                    let live = match tag.at {
                        None => {
                            let index = tag.variant(0, &[])?;
                            index..index + 1
                        }
                        Some(_) => 0..variants,
                    };
                    for v in live {
                        for f in 0..self.variant_fields(ty, v)? {
                            let (off, ft) = (self.field_offset(ty, v, f)?, self.field_ty(ty, v, f)?);
                            todo.push((i, base + off, ft));
                        }
                    }
                }
                PNode::Pat(e, _) => todo.push((i, base, e)),
                // A closure's environment, a coroutine: the pointers they hold are typed by
                // nothing here, so what they point to is written in place (below).
                _ => {}
            }
        }
        for (w, t) in written.iter_mut().zip(typed.iter()).skip(1) {
            *w |= !*t;
        }
        Ok(written)
    }

    fn interior_mut(&mut self, t: PTy) -> R<'a, bool> {
        let (mut cx, l) = self.cx();
        Ok(l.interior_mut(t, &mut cx)?)
    }
}

/// How the evaluated value is held: a static's record is the static's memory (its
/// interior-mutable allocations written in place); every other constant's is copied from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Held {
    Static,
    Copied,
}

/// The little-endian word at `at` of `bytes` (a relocation's addend, a slice's length).
fn word<'a>(bytes: &[u8], at: u64) -> R<'a, u64> {
    let at = usize::try_from(at).map_err(|_| malformed("a word past its allocation"))?;
    let w = bytes.get(at..at + 8).ok_or(malformed("a word past its allocation"))?;
    Ok(u64::from_le_bytes([w[0], w[1], w[2], w[3], w[4], w[5], w[6], w[7]]))
}

/// The validity of a scalar read at a type ("Behavior considered undefined": invalid values).
fn valid<'a>(s: Scalar, v: Valid) -> R<'a, ()> {
    match (s, v) {
        (Scalar::Ptr(_), Valid::Num | Valid::Bool | Valid::Char) => Err(Stop::Refused(Refusal::PointerAsInt)),
        (Scalar::Int(x), Valid::Bool) if x > 1 => Err(Stop::Refused(Refusal::InvalidBool { value: x })),
        (Scalar::Int(x), Valid::Char) if u32::try_from(x).ok().and_then(char::from_u32).is_none() => Err(Stop::Refused(Refusal::InvalidChar { value: x })),
        _ => Ok(()),
    }
}

/// `value_of` with a planted bug, for the tests' controls.
#[cfg(test)]
pub(crate) fn value_of_planted<'a, W: ConstWorld, L: Layout, A: Allocator + Clone>(
    world: &W,
    layout: &mut L,
    codes: &mut Codes<A>,
    item: ItemKey,
    ground: &mut PortableTys<'_>,
    plant: Plant,
    alloc: Alloc<'a>,
) -> Result<Outcome<'a>, ConstError<'a>> {
    codes.table.evaluation += 1;
    let Codes { table, work } = codes;
    let mut m = Machine::new(world, layout, ground, Limits::DEFAULT, alloc, work);
    m.plant = plant;
    let out = m.evaluate(table, item, List::EMPTY);
    let at = m.at;
    match out {
        Ok(record) => Ok(Answer::Done(record)),
        Err(Stop::Need(n)) => Ok(Answer::Need(n)),
        Err(Stop::Refused(refusal)) => Err(ConstError { constant: item, at, refusal }),
    }
}

/// `eval_discriminant` with a planted bug.
#[cfg(test)]
pub(crate) fn discriminant_with_plant<'a, W: ConstWorld>(world: &W, adt: ItemKey, variant: u32, plant: Plant, alloc: Alloc<'a>) -> Result<Answer<i128>, ConstError<'a>> {
    let mut reads = Vec::new_in(alloc);
    match discriminant_planted(world, adt, variant, &mut reads, alloc, plant) {
        Ok(v) => Ok(Answer::Done(v)),
        Err(Stop::Need(n)) => Ok(Answer::Need(n)),
        Err(Stop::Refused(refusal)) => Err(ConstError { constant: adt, at: None, refusal }),
    }
}
