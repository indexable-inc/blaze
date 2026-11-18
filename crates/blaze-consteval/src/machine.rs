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
use code::{EnumFacts, Meta, Num, Op, Reg, TyInfo, Valid};

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
    let work = codes.take_work();
    let mut m = Machine::new(world, layout, ground, limits, alloc, work);
    let out = m.evaluate(codes, item, args);
    let at = m.at;
    codes.put_work(m.into_work());
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
    let repr = match world.repr(adt).int {
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

/// A value: none (zero-sized), a scalar, a pair (a slice's or `str`'s pointer and length), or
/// an aggregate in memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Val {
    Zst,
    S(Scalar),
    Pair(Scalar, Scalar),
    Mem(Pointer),
}

/// A place: an address, and a length for a slice's or `str`'s.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Place {
    ptr: Pointer,
    meta: Option<u64>,
}

impl Place {
    fn of(ptr: Pointer) -> Place {
        Place { ptr, meta: None }
    }
}

/// What a register holds.
#[derive(Clone, Copy, Debug)]
enum Out {
    V(Val),
    P(Place),
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

/// Where the run goes after an operation.
enum Flow {
    Next,
    Goto(u32),
    /// A call or a return moved the frames; the new top's `pc` is set.
    Moved,
    /// The root returned.
    Done(Val),
}

/// The machine's working vectors, kept in [`Codes`] between evaluations of its unit: each
/// evaluation empties them and keeps their room, so after the first few nothing grows (the
/// per-evaluation setup was a fifth of a small constant's samples).
pub(crate) struct Work<K: Allocator + Clone> {
    mem: Memory<K>,
    frames: Vec<Frame, K>,
    regs: Vec<Out, K>,
    locals: Vec<Option<AllocId>, K>,
    temps: Vec<AllocId, K>,
    decoded: Vec<(RecordKey, AllocId), K>,
    statics: Vec<(ItemKey, AllocId), K>,
    fns: Vec<(ConstFn, AllocId), K>,
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
            reads: Vec::new_in(alloc.clone()),
            entered: Vec::new_in(alloc),
        }
    }
}

struct Machine<'w, 'l, 'g, 't, 'a, W: ConstWorld, L: Layout, K: Allocator + Clone> {
    world: &'w W,
    layout: &'l mut L,
    ground: &'g mut PortableTys<'t>,
    limits: Limits,
    alloc: Alloc<'a>,
    mem: Memory<K>,
    frames: Vec<Frame, K>,
    /// Every frame's registers, each frame's after its caller's.
    regs: Vec<Out, K>,
    /// Every frame's memory locals, allocated when first used.
    locals: Vec<Option<AllocId>, K>,
    /// Temporaries of the statements being run: each ended with its statement.
    temps: Vec<AllocId, K>,
    /// Records read and decoded: by key, the root allocation.
    decoded: Vec<(RecordKey, AllocId), K>,
    statics: Vec<(ItemKey, AllocId), K>,
    /// Each fn pointer's allocation, by the instance it points to ([`ConstFn`]).
    fns: Vec<(ConstFn, AllocId), K>,
    /// What the evaluation read, sorted and without repeats as it is added ([`Self::read_add`]).
    reads: Vec<ContentId, K>,
    /// The codes run so far, whose reads are in `reads`.
    entered: Vec<u32, K>,
    steps: u64,
    /// The operation that refused: what an error names.
    at: Option<At>,
    plant: Plant,
}

impl<'w, 'l, 'g, 't, 'a, W: ConstWorld, L: Layout, K: Allocator + Clone> Machine<'w, 'l, 'g, 't, 'a, W, L, K> {
    /// A machine over `work`, emptied (every evaluation starts from nothing).
    fn new(world: &'w W, layout: &'l mut L, ground: &'g mut PortableTys<'t>, limits: Limits, alloc: Alloc<'a>, work: Work<K>) -> Self {
        let Work { mut mem, mut frames, mut regs, mut locals, mut temps, mut decoded, mut statics, mut fns, mut reads, mut entered } = work;
        mem.reset(limits.memory);
        frames.clear();
        regs.clear();
        locals.clear();
        temps.clear();
        decoded.clear();
        statics.clear();
        fns.clear();
        reads.clear();
        entered.clear();
        Machine {
            world,
            layout,
            ground,
            limits,
            alloc,
            mem,
            frames,
            regs,
            locals,
            temps,
            decoded,
            statics,
            fns,
            reads,
            entered,
            steps: 0,
            at: None,
            plant: Plant::None,
        }
    }

    /// The working vectors, given back to be kept for the next evaluation.
    fn into_work(self) -> Work<K> {
        let Machine { mem, frames, regs, locals, temps, decoded, statics, fns, reads, entered, .. } = self;
        Work { mem, frames, regs, locals, temps, decoded, statics, fns, reads, entered }
    }

    /// Adds `id` to the reads, kept sorted and without repeats.
    fn read_add(&mut self, id: ContentId) {
        if let Err(at) = self.reads.binary_search(&id) {
            self.reads.insert(at, id);
        }
    }

    // ---- The evaluation ----

    fn evaluate<A: Allocator + Clone>(&mut self, codes: &mut Codes<A>, item: ItemKey, args: List) -> R<'a, ConstRecord<Alloc<'a>>> {
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
        let mut reads = Vec::with_capacity_in(self.reads.len(), self.alloc);
        reads.extend_from_slice(&self.reads);
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
                let named = self.fns.iter().find(|(f, _)| f.key == key).ok_or(malformed("a fn word whose instance the evaluation did not make"))?;
                fns.insert(at, named.0);
            }
        }
        Ok(fns)
    }

    fn instance(&self, item: ItemKey, args: List) -> R<'a, InstanceKey> {
        instance_key(self.world.code_id(item), args, self.ground.view(), self.alloc).map_err(|_| malformed("an instance's arguments past the ground table"))
    }

    /// Pushes a frame running code `ci` with registers from `base`; its value goes to the
    /// caller's register `dst`.
    fn enter<A: Allocator + Clone>(&mut self, codes: &Codes<A>, ci: u32, dst: Reg, base: u32) -> R<'a, ()> {
        if self.frames.len() as u32 >= self.limits.depth {
            return Err(Stop::Refused(Refusal::CallDepth { limit: self.limits.depth }));
        }
        let c = &codes.list[ci as usize];
        // What the code read, once per evaluation: its body, and the records its compile read.
        if !self.entered.contains(&ci) {
            self.entered.push(ci);
            self.read_add(c.hash);
            for &r in c.reads.iter() {
                self.read_add(r);
            }
        }
        let need = base as usize + c.regs as usize;
        if self.regs.len() < need {
            self.regs.resize(need, Out::V(Val::Zst));
        }
        let locals = self.locals.len() as u32;
        self.locals.resize(self.locals.len() + c.slots as usize, None);
        self.frames.push(Frame { code: ci, pc: 0, base, locals, dst });
        Ok(())
    }

    // Not inlined: inside `value_of`'s large frame the loop's values were spilled and reloaded
    // through the stack on every step.
    #[inline(never)]
    fn run<A: Allocator + Clone>(&mut self, codes: &mut Codes<A>) -> R<'a, Val> {
        loop {
            let top = self.frames.len().checked_sub(1).ok_or(malformed("no frame"))?;
            let f = self.frames[top];
            let op = *codes.list[f.code as usize].ops.get(f.pc as usize).ok_or(malformed("a jump past its code"))?;
            self.steps += 1;
            if self.steps > self.limits.steps {
                self.at = None;
                return Err(Stop::Refused(Refusal::StepLimit { limit: self.limits.steps }));
            }
            match self.step(codes, f, op) {
                Ok(Flow::Next) => self.frames[top].pc = f.pc + 1,
                Ok(Flow::Goto(to)) => self.frames[top].pc = to,
                Ok(Flow::Moved) => {}
                Ok(Flow::Done(v)) => return Ok(v),
                Err(s) => {
                    let c = &codes.list[f.code as usize];
                    self.at = Some(At { body: c.item, range: c.at.get(f.pc as usize).copied().unwrap_or_default() });
                    return Err(s);
                }
            }
        }
    }

    #[inline]
    fn set(&mut self, base: u32, r: Reg, o: Out) {
        self.regs[(base + r) as usize] = o;
    }

    #[inline]
    fn val(&self, base: u32, r: Reg) -> R<'a, Val> {
        match self.regs[(base + r) as usize] {
            Out::V(v) => Ok(v),
            Out::P(_) => Err(malformed("a place where a value was asked")),
        }
    }

    #[inline]
    fn place(&self, base: u32, r: Reg) -> R<'a, Place> {
        match self.regs[(base + r) as usize] {
            Out::P(p) => Ok(p),
            Out::V(_) => Err(malformed("a value where a place was asked")),
        }
    }

    fn bool_of(&self, base: u32, r: Reg) -> R<'a, bool> {
        match self.val(base, r)? {
            Val::S(Scalar::Int(0)) => Ok(false),
            Val::S(Scalar::Int(1)) => Ok(true),
            Val::S(Scalar::Int(v)) => Err(Stop::Refused(Refusal::InvalidBool { value: v })),
            _ => Err(malformed("a condition that is not a `bool`")),
        }
    }

    /// Runs one operation of frame `f`.
    fn step<A: Allocator + Clone>(&mut self, codes: &mut Codes<A>, f: Frame, op: Op) -> R<'a, Flow> {
        let b = f.base;
        if let Op::Call { dst, k } = op {
            return self.call(codes, f, dst, k);
        }
        let code = &codes.list[f.code as usize];
        match op {
            Op::Imm { dst, v } => self.set(b, dst, Out::V(v)),
            Op::Str { dst, at, len, wide } => {
                let bytes = code.bytes.get(at as usize..(at + len) as usize).ok_or(malformed("a string past its code's bytes"))?;
                let id = self.mem.allocate(u64::from(len), 0, true, Origin::Data)?;
                self.mem.fill(id, bytes, &[])?;
                self.mem.get_mut(id)?.mutable = false;
                let p = Scalar::Ptr(Pointer { alloc: id, offset: 0 });
                self.set(b, dst, Out::V(if wide { Val::Pair(p, Scalar::Int(u128::from(len))) } else { Val::S(p) }));
            }
            Op::Move { dst, src } => self.regs[(b + dst) as usize] = self.regs[(b + src) as usize],
            Op::Local { dst, slot, ty } => {
                let i = (f.locals + slot) as usize;
                let id = match self.locals[i] {
                    Some(id) => id,
                    None => {
                        let t = code.tys[ty as usize];
                        let id = self.mem.allocate(t.size, t.align, true, Origin::Local)?;
                        self.locals[i] = Some(id);
                        id
                    }
                };
                self.set(b, dst, Out::P(Place::of(Pointer { alloc: id, offset: 0 })));
            }
            Op::Const { dst, k } => {
                let (item, args, key) = code.consts[k as usize];
                let pl = self.const_place(item, args, key)?;
                self.set(b, dst, Out::P(pl));
            }
            Op::Static { dst, item } => {
                let id = self.static_alloc(item)?;
                self.set(b, dst, Out::P(Place::of(Pointer { alloc: id, offset: 0 })));
            }
            Op::Load { dst, src, ty } => {
                let v = match self.regs[(b + src) as usize] {
                    Out::V(v) => v,
                    Out::P(pl) => self.load_i(pl, code.tys[ty as usize])?,
                };
                self.set(b, dst, Out::V(v));
            }
            Op::Spill { dst, src, ty } => {
                let pl = match self.regs[(b + src) as usize] {
                    Out::P(pl) => pl,
                    Out::V(Val::Mem(p)) => Place::of(p),
                    Out::V(v) => {
                        let t = code.tys[ty as usize];
                        let pl = Place::of(self.temp_i(t)?);
                        self.store_i(pl, t, v)?;
                        pl
                    }
                };
                self.set(b, dst, Out::P(pl));
            }
            Op::Deref { dst, src } => {
                let v = self.val(b, src)?;
                let pl = self.deref(v)?;
                self.set(b, dst, Out::P(pl));
            }
            Op::Borrow { dst, src, meta } => {
                let mut pl = self.place(b, src)?;
                match meta {
                    Meta::Keep => {}
                    Meta::Drop => pl.meta = None,
                    Meta::Need => {
                        if pl.meta.is_none() {
                            return Err(malformed("an unsized place with no length"));
                        }
                    }
                }
                let v = self.borrow(pl)?;
                self.set(b, dst, Out::V(v));
            }
            Op::Unsize { dst, src, n } => match self.val(b, src)? {
                Val::S(p) => self.set(b, dst, Out::V(Val::Pair(p, Scalar::Int(u128::from(n))))),
                _ => return Err(malformed("an unsizing coercion of a wide pointer")),
            },
            Op::FnPtr { dst, k } => {
                let named = code.fns[k as usize];
                let p = self.fn_ptr(named)?;
                self.set(b, dst, Out::V(Val::S(Scalar::Ptr(p))));
            }
            Op::Unary { dst, src, op, num } => {
                let v = self.val(b, src)?;
                let r = self.unary(op, num, v)?;
                self.set(b, dst, Out::V(r));
            }
            Op::Binary { dst, l, r, op, num, rnum } => {
                let (lv, rv) = (self.val(b, l)?, self.val(b, r)?);
                let v = self.binary(op, num, rnum, lv, rv)?;
                self.set(b, dst, Out::V(v));
            }
            Op::Cast { dst, src, from, to } => {
                let v = self.val(b, src)?;
                let r = self.cast(v, from, to)?;
                self.set(b, dst, Out::V(r));
            }
            Op::Discr { dst, src, e, ty, bits } => {
                let pl = match self.regs[(b + src) as usize] {
                    Out::P(pl) => pl,
                    Out::V(Val::Mem(p)) => Place::of(p),
                    Out::V(v) => {
                        let t = code.tys[ty as usize];
                        let pl = Place::of(self.temp_i(t)?);
                        self.store_i(pl, t, v)?;
                        pl
                    }
                };
                let facts = code.enums.get(e as usize).ok_or(malformed("an enum past its code"))?;
                let variant = self.variant_of(pl, facts, &code.discrs)?;
                let d = facts.discriminant(variant, &code.discrs)?;
                self.set(b, dst, Out::V(Val::S(Scalar::Int(mask(d as u128, bits)))));
            }
            Op::Field { dst, src, off, tail } => {
                let pl = self.place(b, src)?;
                // The unsized tail keeps the struct's length: without it a borrow of the tail
                // (`&self.0` in crypto-bigint's `UintRef`) had no length ("an unsized place with
                // no length") or, borrowed as written, a thin pointer to a slice.
                let meta = if tail { pl.meta } else { None };
                self.set(b, dst, Out::P(Place { ptr: pl.ptr.at(off), meta }));
            }
            Op::Index { dst, base, index, size, len } => {
                let i = self.val(b, index)?;
                let pl = self.place(b, base)?;
                let len = match len {
                    Some(n) => n,
                    None => pl.meta.ok_or(malformed("a slice place with no length"))?,
                };
                let Val::S(Scalar::Int(i)) = i else { return Err(Stop::Refused(Refusal::PointerAsInt)) };
                if i >= u128::from(len) {
                    return Err(Stop::Refused(Refusal::IndexOutOfBounds { index: i as u64, len }));
                }
                self.set(b, dst, Out::P(Place::of(pl.ptr.at(size * i as u64))));
            }
            Op::FromEnd { dst, src, size, back } => {
                let pl = self.place(b, src)?;
                let len = pl.meta.ok_or(malformed("a slice place with no length"))?;
                let i = len.checked_sub(back).ok_or(malformed("a slice pattern's element past a slice its length test let through"))?;
                self.set(b, dst, Out::P(Place::of(pl.ptr.at(size * i))));
            }
            Op::Subslice { dst, src, size, from, back } => {
                let pl = self.place(b, src)?;
                let len = pl.meta.ok_or(malformed("a slice place with no length"))?;
                let left = len.checked_sub(from + back).ok_or(malformed("a slice pattern's `..` past a slice its length test let through"))?;
                self.set(b, dst, Out::P(Place { ptr: pl.ptr.at(size * from), meta: Some(left) }));
            }
            Op::Build { dst, k } => {
                let build = code.builds[k as usize];
                let p = self.temp_i(code.tys[build.ty as usize])?;
                for part in &code.parts[build.parts.0 as usize..(build.parts.0 + build.parts.1) as usize] {
                    let v = self.val(b, part.src)?;
                    self.store_i(Place::of(p.at(part.off)), code.tys[part.ty as usize], v)?;
                }
                if let Some(w) = build.tag {
                    self.mem.write(p.at(w.offset), w.size, Scalar::Int(w.value))?;
                }
                self.set(b, dst, Out::V(Val::Mem(p)));
            }
            Op::Update { dst, base, k } => {
                let build = code.builds[k as usize];
                let pl = self.place(b, base)?;
                let v0 = self.load_i(pl, code.tys[build.ty as usize])?;
                if let Val::Mem(p) = v0 {
                    for part in &code.parts[build.parts.0 as usize..(build.parts.0 + build.parts.1) as usize] {
                        let v = self.val(b, part.src)?;
                        self.store_i(Place::of(p.at(part.off)), code.tys[part.ty as usize], v)?;
                    }
                }
                self.set(b, dst, Out::V(v0));
            }
            Op::Repeat { dst, src, k } => {
                let rep = code.repeats[k as usize];
                let v = self.val(b, src)?;
                // The memory bound first: an array too large to hold is refused as that before
                // any of its `n` stores is counted (rule 18: each bound names its own cause).
                let p = self.temp_i(code.tys[rep.ty as usize])?;
                self.steps = self.steps.saturating_add(rep.n);
                if self.steps > self.limits.steps {
                    return Err(Stop::Refused(Refusal::StepLimit { limit: self.limits.steps }));
                }
                let el = code.tys[rep.elem as usize];
                for k in 0..rep.n {
                    self.store_i(Place::of(p.at(el.size * k)), el, v)?;
                }
                self.set(b, dst, Out::V(Val::Mem(p)));
            }
            Op::Store { place, src, ty } => {
                let pl = self.place(b, place)?;
                let v = self.val(b, src)?;
                self.store_i(pl, code.tys[ty as usize], v)?;
            }
            Op::StoreOp { place, src, ty, op, num, rnum } => {
                let pl = self.place(b, place)?;
                let r = self.val(b, src)?;
                let t = code.tys[ty as usize];
                let l = self.load_i(pl, t)?;
                let v = self.binary(op, num, rnum, l, r)?;
                self.store_i(pl, t, v)?;
            }
            Op::Jump { to } => return Ok(Flow::Goto(to)),
            Op::Branch { cond, when, to } => {
                if self.bool_of(b, cond)? == when {
                    return Ok(Flow::Goto(to));
                }
            }
            Op::Call { .. } => return Err(malformed("a call run out of turn")),
            Op::Intrinsic { dst, k } => {
                let call = &code.calls[k as usize];
                let i = call.intrinsic.ok_or(malformed("an intrinsic call with no intrinsic"))?;
                let mut vals: Vec<(Val, PTy), Alloc<'a>> = Vec::new_in(self.alloc);
                for &(r, t) in &code.regs_of[call.regs.0 as usize..(call.regs.0 + call.regs.1) as usize] {
                    vals.push((self.val(b, r)?, t));
                }
                let v = self.intrinsic(i, call.args, call.ret, &vals)?;
                self.set(b, dst, Out::V(v));
            }
            Op::Panic { k } => {
                let call = &code.calls[k as usize];
                let mut parts = Vec::new_in(self.alloc);
                for &(r, t) in &code.regs_of[call.regs.0 as usize..(call.regs.0 + call.regs.1) as usize] {
                    let v = self.val(b, r)?;
                    parts.push(self.panic_part(v, t)?);
                }
                return Err(Stop::Refused(Refusal::Panic { parts }));
            }
            Op::Ret { src } => {
                let v = self.regs[(b + src) as usize];
                self.frames.pop();
                while self.locals.len() > f.locals as usize {
                    if let Some(Some(id)) = self.locals.pop() {
                        self.mem.kill(id);
                    }
                }
                return match self.frames.last() {
                    None => match v {
                        Out::V(v) => Ok(Flow::Done(v)),
                        Out::P(_) => Err(malformed("a body whose value is a place")),
                    },
                    Some(c) => {
                        let at = c.base;
                        self.set(at, f.dst, v);
                        Ok(Flow::Moved)
                    }
                };
            }
            Op::Mark { dst } => self.set(b, dst, Out::V(Val::S(Scalar::Int(self.temps.len() as u128)))),
            Op::Kill { mark } => {
                // An expression statement's temporaries end with it (the Reference, "Temporary
                // scopes"); no `let` extends them.
                let Val::S(Scalar::Int(n)) = self.val(b, mark)? else { return Err(malformed("a statement mark that is not a count")) };
                while self.temps.len() > n as usize {
                    if let Some(id) = self.temps.pop() {
                        self.mem.kill(id);
                    }
                }
            }
            Op::Nop => {}
            Op::IfVariant { place, e, variant, fail } => {
                let pl = self.place(b, place)?;
                let facts = code.enums.get(e as usize).ok_or(malformed("an enum past its code"))?;
                if self.variant_of(pl, facts, &code.discrs)? != variant {
                    return Ok(Flow::Goto(fail));
                }
            }
            Op::IfSame { a, b: other, ty, konst, fail } => {
                let (want, have) = (self.val(b, a)?, self.val(b, other)?);
                if konst && matches!(want, Val::Mem(_)) {
                    return Err(unsupported("a constant pattern of an aggregate type"));
                }
                if !self.same(want, have, ty)? {
                    return Ok(Flow::Goto(fail));
                }
            }
            Op::IfLen { place, len, at_least, fail } => {
                let pl = self.place(b, place)?;
                let have = pl.meta.ok_or(malformed("a slice place with no length"))?;
                if if at_least { have < len } else { have != len } {
                    return Ok(Flow::Goto(fail));
                }
            }
            Op::Copy { dst, src, size } => {
                let (d, s) = (self.place(b, dst)?, self.place(b, src)?);
                self.ensure_loaded(s.ptr.alloc)?;
                self.mem.copy(s.ptr, d.ptr, size)?;
            }
            Op::Late { k } => {
                // A pending stop made this code incomplete when it was compiled
                // (machine/late.rs): the next evaluation compiles it again before running it.
                return Err(code.lates.get(k as usize).ok_or(malformed("a stop past its code"))?.stop());
            }
        }
        Ok(Flow::Next)
    }

    /// A `const fn` call: the callee's code (compiled when first called), its frame, its
    /// arguments in its first registers.
    fn call<A: Allocator + Clone>(&mut self, codes: &mut Codes<A>, f: Frame, dst: Reg, k: u32) -> R<'a, Flow> {
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
        let top = self.frames.len() - 1;
        self.frames[top].pc = f.pc + 1;
        self.enter(codes, ci, dst, base)?;
        for (i, &(r, _)) in caller.regs_of[regs.0 as usize..(regs.0 + regs.1) as usize].iter().enumerate() {
            self.regs[base as usize + i] = self.regs[(f.base + r) as usize];
        }
        Ok(Flow::Moved)
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

    /// The ground type of field `field` (of the variant's own numbering) of ADT or tuple `t`.
    fn field_ty(&mut self, t: PTy, variant: u32, field: u32) -> R<'a, PTy> {
        match self.node(t)? {
            PNode::Tuple(l) => self.ground.view().list(l).ok().and_then(|l| l.get(field as usize).copied()).ok_or(malformed("a tuple field past its tuple")),
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
        let id = self.mem.allocate(t.size, t.align, true, Origin::Data)?;
        self.temps.push(id);
        Ok(Pointer { alloc: id, offset: 0 })
    }

    /// A static's or decoded record's allocation, its bytes loaded.
    fn ensure_loaded(&mut self, id: AllocId) -> R<'a, ()> {
        let a = self.mem.get(id)?;
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
        Ok(self.mem.read(p, n)?)
    }

    fn load_i(&mut self, pl: Place, t: TyInfo) -> R<'a, Val> {
        self.ensure_loaded(pl.ptr.alloc)?;
        Ok(match t.repr {
            Repr::Zst => {
                self.mem.check_access(pl.ptr, 0)?;
                Val::Zst
            }
            Repr::Scalar(n) => {
                let s = self.mem.read(pl.ptr, n)?;
                valid(s, t.valid)?;
                Val::S(s)
            }
            Repr::Pair => Val::Pair(self.mem.read(pl.ptr, PTR)?, self.mem.read(pl.ptr.at(PTR), PTR)?),
            Repr::Mem(n) => {
                let to = self.temp_i(t)?;
                self.mem.copy(pl.ptr, to, n)?;
                Val::Mem(to)
            }
        })
    }

    fn load(&mut self, pl: Place, t: PTy) -> R<'a, Val> {
        let t = self.tyinfo(t)?;
        self.load_i(pl, t)
    }

    fn store_i(&mut self, pl: Place, t: TyInfo, v: Val) -> R<'a, ()> {
        match (t.repr, v) {
            (Repr::Zst, _) => Ok(()),
            (Repr::Scalar(n), Val::S(s)) => Ok(self.mem.write(pl.ptr, n, s)?),
            (Repr::Pair, Val::Pair(a, b)) => {
                self.mem.write(pl.ptr, PTR, a)?;
                Ok(self.mem.write(pl.ptr.at(PTR), PTR, b)?)
            }
            (Repr::Mem(n), Val::Mem(src)) => {
                self.ensure_loaded(src.alloc)?;
                Ok(self.mem.copy(src, pl.ptr, n)?)
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

    /// The place a pointer value points to; checks the allocation is live.
    fn deref(&mut self, v: Val) -> R<'a, Place> {
        let (ptr, meta) = match v {
            Val::S(Scalar::Ptr(p)) => (p, None),
            Val::Pair(Scalar::Ptr(p), Scalar::Int(n)) => (p, Some(n as u64)),
            Val::S(Scalar::Int(_)) | Val::Pair(Scalar::Int(_), _) => return Err(Stop::Refused(Refusal::IntAsPointer)),
            _ => return Err(malformed("a dereference of a value that is not a pointer")),
        };
        self.ensure_loaded(ptr.alloc)?;
        if !self.mem.get(ptr.alloc)?.live {
            return Err(Stop::Refused(Refusal::Dangling));
        }
        Ok(Place { ptr, meta })
    }

    /// A reference to `pl`. A constant's value used as a place is a fresh temporary at each use
    /// (the Reference, "Constant items": "constants are essentially inlined wherever they are
    /// used"; "Temporaries"): borrowing it copies its decoded record into a temporary of its own,
    /// so two borrows of one constant are two allocations, as two uses may be in the program,
    /// and a write through `&mut` reaches only its own copy. Reads take the record in place.
    fn borrow(&mut self, mut pl: Place) -> R<'a, Val> {
        if self.decoded.iter().any(|&(_, id)| id == pl.ptr.alloc) {
            let (size, align) = {
                let a = self.mem.get(pl.ptr.alloc)?;
                (a.len(), a.align_log2)
            };
            let id = self.mem.allocate(size, align, true, Origin::Data)?;
            self.temps.push(id);
            self.mem.copy(Pointer { alloc: pl.ptr.alloc, offset: 0 }, Pointer { alloc: id, offset: 0 }, size)?;
            pl.ptr.alloc = id;
        }
        Ok(match pl.meta {
            Some(n) => Val::Pair(Scalar::Ptr(pl.ptr), Scalar::Int(u128::from(n))),
            None => Val::S(Scalar::Ptr(pl.ptr)),
        })
    }

    /// The pointer to fn instance `named`: one zero-sized allocation per instance key an
    /// evaluation, which remembers the instance so the record can name it ([`ConstFn`]).
    fn fn_ptr(&mut self, named: ConstFn) -> R<'a, Pointer> {
        let id = match self.fns.iter().find(|(f, _)| f.key == named.key) {
            Some(&(_, id)) => id,
            None => {
                let id = self.mem.allocate(0, 0, false, Origin::Fn(named.key))?;
                self.fns.push((named, id));
                id
            }
        };
        Ok(Pointer { alloc: id, offset: 0 })
    }

    fn static_alloc(&mut self, item: ItemKey) -> R<'a, AllocId> {
        if let Some(&(_, id)) = self.statics.iter().find(|(k, _)| *k == item) {
            return Ok(id);
        }
        let id = self.mem.allocate(0, 0, false, Origin::Static(item))?;
        self.mem.get_mut(id)?.loaded = false;
        self.statics.push((item, id));
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
                    self.mem.resize_unloaded(r, a.bytes.len() as u64)?;
                    self.mem.get_mut(r)?.align_log2 = a.align;
                    r
                }
                _ => self.mem.allocate(a.bytes.len() as u64, a.align, false, Origin::Data)?,
            };
            ids.push(id);
        }
        let mut relocs: Vec<(u64, AllocId), Alloc<'a>> = Vec::new_in(self.alloc);
        for (i, a) in rec.allocs.iter().enumerate() {
            relocs.clear();
            for r in a.relocs.iter() {
                let target = match r.target {
                    DataTarget::Local(j) => *ids.get(j as usize).ok_or(malformed("a relocation past its record"))?,
                    DataTarget::Static(k) => self.static_alloc(k)?,
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
            self.mem.fill(ids[i], &a.bytes, &relocs)?;
        }
        Ok(ids[0])
    }

    /// The place of constant `item`'s value (record `key`), its record decoded once per
    /// evaluation.
    fn const_place(&mut self, item: ItemKey, args: List, key: RecordKey) -> R<'a, Place> {
        if let Some(&(_, id)) = self.decoded.iter().find(|(k, _)| *k == key) {
            return Ok(Place::of(Pointer { alloc: id, offset: 0 }));
        }
        let world = self.world;
        let rec = world.record(key).map_err(|_| Stop::Need(Need { kind: NeedKind::Value, item, args }))?;
        self.read_add(key.0);
        let id = self.decode(rec, None)?;
        self.decoded.push((key, id));
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
                self.ensure_loaded(p.alloc)?;
                let len = self.mem.get(p.alloc)?.len();
                if i == 0 && p.offset <= len { 0 } else { 2 }
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
                self.ensure_loaded(p.alloc)?;
                self.ensure_loaded(q.alloc)?;
                let (pa, qa) = (self.mem.get(p.alloc)?, self.mem.get(q.alloc)?);
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
        let bits = match facts.tag_at() {
            Some((off, size)) => match self.read(pl.ptr.at(off), size)? {
                Scalar::Int(bits) => bits,
                _ => return Err(Stop::Refused(Refusal::PointerAsInt)),
            },
            None => 0,
        };
        Ok(facts.variant(bits, discrs)?)
    }

    // ---- Operators ----

    fn unary(&mut self, op: UnOp, num: Num, v: Val) -> R<'a, Val> {
        let Val::S(Scalar::Int(a)) = v else { return Err(Stop::Refused(Refusal::PointerAsInt)) };
        Ok(Val::S(Scalar::Int(match (op, num) {
            (UnOp::Not, Num::Bool) => a ^ 1,
            (UnOp::Not, Num::Int { bits, .. }) => mask(!a, bits),
            (UnOp::Not, _) => return Err(malformed("`!` of a type that is not an integer")),
            (UnOp::Neg, Num::Float(FloatTy::F32)) => a ^ (1 << 31),
            (UnOp::Neg, Num::Float(FloatTy::F64)) => a ^ (1 << 63),
            (UnOp::Neg, Num::Float(f)) => a ^ Format::of(f).sign(),
            (UnOp::Neg, Num::Int { bits, signed }) => {
                if !signed {
                    return Err(malformed("`-` of an unsigned integer"));
                }
                if sext(a, bits) == smin(bits) && self.plant != Plant::WrapOnOverflow {
                    return Err(Stop::Refused(Refusal::Overflow { op: Arith::Neg }));
                }
                mask(a.wrapping_neg(), bits)
            }
            (UnOp::Neg, _) => return Err(malformed("`-` of a type that is not a number")),
            (UnOp::Deref, _) => return Err(malformed("a deref as arithmetic")),
        })))
    }

    fn binary(&mut self, op: BinOp, num: Num, rnum: Num, l: Val, r: Val) -> R<'a, Val> {
        let (a, b) = match (l, r) {
            (Val::S(Scalar::Int(a)), Val::S(Scalar::Int(b))) => (a, b),
            (Val::S(Scalar::Ptr(p)), Val::S(Scalar::Ptr(q))) if self.plant == Plant::ComparePointers => {
                let eq = p == q;
                return Ok(Val::S(Scalar::Int(u128::from(match op {
                    BinOp::Eq => eq,
                    BinOp::Ne => !eq,
                    _ => p.offset < q.offset,
                }))));
            }
            (Val::S(_) | Val::Pair(..), Val::S(_) | Val::Pair(..)) if matches!(op, BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge) => {
                return Err(Stop::Refused(Refusal::PointerCompare));
            }
            (Val::S(Scalar::Ptr(_)), _) | (_, Val::S(Scalar::Ptr(_))) => return Err(Stop::Refused(Refusal::PointerAsInt)),
            _ => return Err(unsupported("an operator on a type that is not a scalar (a const trait)")),
        };
        let (bits, signed) = match num {
            Num::Float(f) => return Ok(Val::S(Scalar::Int(crate::arith::float_binop(op, f, a, b)?))),
            Num::Int { bits, signed } => (bits, signed),
            Num::Bool => (8, false),
            Num::Other => return Err(unsupported("an operator on this type")),
        };
        let rhs = match rnum {
            Num::Int { bits, signed } => (bits, signed),
            Num::Bool => (8, false),
            _ => (bits, signed),
        };
        Ok(Val::S(Scalar::Int(int_binop(op, a, b, bits, signed, rhs, self.plant)?)))
    }

    /// `as` (the Reference, "Type cast expressions"): pointers here, every number in
    /// [`crate::arith::num_cast`].
    fn cast(&mut self, v: Val, from: PTy, to: PTy) -> R<'a, Val> {
        let (fnode, tnode) = (self.node(from)?, self.node(to)?);
        match (fnode, tnode) {
            (PNode::Ref(..) | PNode::Ptr(..) | PNode::FnPtr(..), PNode::Ptr(_, p)) => {
                return Ok(match (v, layout::is_unsized(self.world, self.ground, p, self.alloc)?) {
                    (Val::Pair(a, _), false) => Val::S(a),
                    (v, _) => v,
                });
            }
            (PNode::Ptr(..) | PNode::FnPtr(..) | PNode::Ref(..), PNode::Int(_) | PNode::Uint(_)) => {
                return match v {
                    Val::S(Scalar::Int(a)) => Ok(Val::S(Scalar::Int(a))),
                    _ => Err(Stop::Refused(Refusal::PointerAsInt)),
                };
            }
            // A fieldless enum to its discriminant is `Op::Discr`.
            (PNode::Adt(..), _) => return Err(malformed("an enum cast compiled as a scalar cast")),
            _ => {}
        }
        let Val::S(Scalar::Int(a)) = v else { return Err(Stop::Refused(Refusal::PointerAsInt)) };
        Ok(Val::S(Scalar::Int(crate::arith::num_cast(fnode, tnode, a)?)))
    }

    // ---- Calls ----

    fn panic_part(&mut self, v: Val, t: PTy) -> R<'a, PanicPart<'a>> {
        let node = self.node(t)?;
        Ok(match (node, v) {
            (PNode::Ref(_, s), Val::Pair(p, Scalar::Int(n))) if self.node(s)? == PNode::Str => {
                let pl = self.deref(Val::Pair(p, Scalar::Int(n)))?;
                let mut bytes = Vec::new_in(self.alloc);
                for k in 0..n as u64 {
                    let Scalar::Int(b) = self.read(pl.ptr.at(k), 1)? else { return Err(Stop::Refused(Refusal::PointerAsInt)) };
                    bytes.push(b as u8);
                }
                PanicPart::Str(bytes)
            }
            (PNode::Int(i), Val::S(Scalar::Int(a))) => PanicPart::Int(sext(a, int_bits(i))),
            (PNode::Uint(_), Val::S(Scalar::Int(a))) => PanicPart::Uint(a),
            _ => PanicPart::Other,
        })
    }

    /// An intrinsic's result: `rt` the call's type, `args` its generic arguments.
    fn intrinsic(&mut self, i: Intrinsic, args: List, rt: PTy, vals: &[(Val, PTy)]) -> R<'a, Val> {
        let targs: Vec<PTy, Alloc<'a>> = {
            let mut v = Vec::new_in(self.alloc);
            v.extend_from_slice(self.ground.view().list(args).map_err(|_| malformed("arguments past the ground table"))?);
            v
        };
        let int = |k: usize| -> R<'a, u128> {
            match vals.get(k) {
                Some((Val::S(Scalar::Int(a)), _)) => Ok(*a),
                Some((Val::S(Scalar::Ptr(_)), _)) => Err(Stop::Refused(Refusal::PointerAsInt)),
                _ => Err(malformed("an intrinsic's argument that is not an integer")),
            }
        };
        let info = |m: &Self, t: PTy| -> R<'a, (u32, bool)> { int_info(m.node(t)?).ok_or(malformed("an intrinsic of a type that is not an integer")) };
        let s = |v: u128| Ok(Val::S(Scalar::Int(v)));
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
                let t = *targs.first().ok_or(malformed("`size_of` with no type"))?;
                let (size, align) = self.size_align(t)?;
                s(if i == Intrinsic::SizeOf { u128::from(size) } else { 1u128 << align })
            }
            // `transmute_unchecked` is `transmute` without the compile-time size check (the
            // pinned core, library/core/src/intrinsics/mod.rs: different sizes are "Undefined
            // Behavior at runtime"): in a constant that is refused, so both take one arm.
            Intrinsic::Transmute | Intrinsic::TransmuteUnchecked => {
                let (&from, &to) = (targs.first().ok_or(malformed("`transmute` types"))?, targs.get(1).ok_or(malformed("`transmute` types"))?);
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
                self.build_tuple(rt, &[(Val::S(Scalar::Int(r)), t), (Val::S(Scalar::Int(u128::from(over))), bool_t)])
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
            Intrinsic::Ctpop | Intrinsic::Ctlz | Intrinsic::Cttz | Intrinsic::Bswap | Intrinsic::Bitreverse => {
                let t = vals.first().ok_or(malformed("a bit intrinsic's argument"))?.1;
                let (bits, _) = info(self, t)?;
                let a = int(0)?;
                s(match i {
                    Intrinsic::Ctpop => u128::from(a.count_ones()),
                    Intrinsic::Ctlz => u128::from(a.leading_zeros() - (128 - bits)),
                    Intrinsic::Cttz => u128::from(a.trailing_zeros().min(bits)),
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
                let Val::S(Scalar::Ptr(p)) = pv else { return Err(Stop::Refused(Refusal::IntAsPointer)) };
                let delta = if csigned { sext(count, ctbits) } else { count as i128 } * i128::from(size);
                let off = i128::from(p.offset) + delta;
                self.ensure_loaded(p.alloc)?;
                let len = self.mem.get(p.alloc)?.len();
                if off < 0 || off > i128::from(len) {
                    return Err(Stop::Refused(Refusal::OutOfBounds { offset: off.max(0) as u64, size: 0, len }));
                }
                Ok(Val::S(Scalar::Ptr(Pointer { alloc: p.alloc, offset: off as u64 })))
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
                    Val::Pair(_, meta) => Ok(Val::S(meta)),
                    Val::S(_) => Ok(Val::Zst),
                    _ => Err(malformed("`ptr_metadata` of a value that is not a pointer")),
                }
            }
            Intrinsic::AggregateRawPtr => {
                let data = vals.first().ok_or(malformed("`aggregate_raw_ptr` arguments"))?.0;
                let meta = vals.get(1).ok_or(malformed("`aggregate_raw_ptr` arguments"))?.0;
                let Val::S(d) = data else { return Err(malformed("`aggregate_raw_ptr` of a data pointer that is not a scalar")) };
                match meta {
                    Val::Zst => Ok(Val::S(d)),
                    Val::S(m) => Ok(Val::Pair(d, m)),
                    _ => Err(malformed("`aggregate_raw_ptr` metadata that is neither `()` nor one scalar")),
                }
            }
            Intrinsic::PtrGuaranteedCmp => {
                let a = vals.first().ok_or(malformed("`ptr_guaranteed_cmp` arguments"))?.0;
                let b = vals.get(1).ok_or(malformed("`ptr_guaranteed_cmp` arguments"))?.0;
                let (Val::S(x), Val::S(y)) = (a, b) else { return s(2) };
                s(self.guaranteed_cmp(x, y)?)
            }
            // The pinned core, library/core/src/intrinsics/mod.rs: "Returns `true` if the actual
            // type given as `T` requires drop glue": the one rule (blaze-build drop_facts.rs).
            Intrinsic::NeedsDrop => {
                let t = *targs.first().ok_or(malformed("`needs_drop` with no type"))?;
                s(u128::from(crate::drop_ask::needs_drop(self.world, t, self.ground, self.alloc)?))
            }
            // Compiled as a call of its const fn.
            Intrinsic::ConstEvalSelect => Err(malformed("`const_eval_select` run as an intrinsic")),
            // The pinned core, library/core/src/intrinsics/mod.rs: "Gets a static string slice
            // containing the name of a type": the ground type printed by the one printer
            // (blaze-build type_name.rs), its bytes an immutable data allocation, the value the
            // pair of its address and length (as a string literal's, `Op::Str`).
            Intrinsic::TypeName => {
                let t = *targs.first().ok_or(malformed("`type_name` with no type"))?;
                let world = self.world;
                let names = world.type_names().ok_or(unsupported("`type_name` in a world that names no items (`ConstWorld::type_names`)"))?;
                let text = blaze_build::type_name::type_name_text(self.ground.view(), t, names, self.alloc).map_err(|error| match error {
                    blaze_build::type_name::TypeNameError::NoPath { item } => Stop::Refused(Refusal::Missing { what: "the path of an item `type_name` prints", item }),
                    other => unsupported(other.what()),
                })?;
                let len = text.len() as u64;
                let id = self.mem.allocate(len, 0, true, Origin::Data)?;
                self.mem.fill(id, text.as_bytes(), &[])?;
                self.mem.get_mut(id)?.mutable = false;
                Ok(Val::Pair(Scalar::Ptr(Pointer { alloc: id, offset: 0 }), Scalar::Int(u128::from(len))))
            }
            // The pinned core, library/core/src/intrinsics/reflection.rs: "Gets an identifier which
            // is globally unique to the specified type". The one function the builder calls too
            // (blaze-build instance.rs `type_id_hash`), its 128 bits laid out as `TypeId`'s two
            // `data` words, little-endian, so `TypeId::as_u128` reads them back whole.
            Intrinsic::TypeId => {
                let t = *targs.first().ok_or(malformed("`type_id` with no type"))?;
                let bits = blaze_build::instance::type_id_hash(t, self.ground.view(), self.alloc)
                    .map_err(|_| unsupported("`type_id` of a type that is not ground (an alias, parameter or inference variable left)"))?;
                let info = self.tyinfo(rt)?;
                if info.repr != Repr::Mem(16) {
                    return Err(malformed("a `type_id` result that is not `TypeId`'s 16 bytes"));
                }
                let p = self.temp_i(info)?;
                self.mem.write(p, 8, Scalar::Int(bits & u128::from(u64::MAX)))?;
                self.mem.write(p.at(8), 8, Scalar::Int(bits >> 64))?;
                Ok(Val::Mem(p))
            }
            // blaze's `blaze_int_cast::<S, D>(x)` (library/core/src/intrinsics/mod.rs): "`x as D`
            // between two integer types of any width ... it truncates to `D`'s width, or extends
            // by `S`'s sign": the one rule (blaze-build intrinsics.rs `int_cast_bits`).
            Intrinsic::BlazeIntCast => {
                let (&from, &to) = (targs.first().ok_or(malformed("`blaze_int_cast` types"))?, targs.get(1).ok_or(malformed("`blaze_int_cast` types"))?);
                let ((fb, fs), (tb, _)) = (info(self, from)?, info(self, to)?);
                s(blaze_build::intrinsics::int_cast_bits(int(0)?, fb, fs, tb))
            }
            // `raw_eq::<T>(a, b)` ("Determines whether the raw bytes of the two values are
            // equal") and `compare_bytes(left, right, bytes)` ("Lexicographically compare ... as
            // unsigned bytes"): the bytes read one at a time, so an uninitialized byte or one of
            // a pointer ("At compile-time, it is furthermore UB to call this if any of the bytes
            // ... have provenance") is refused by name, then the one rule (blaze-build
            // intrinsics.rs `byte_order`). Every byte of both ranges is read ("this applies to
            // the whole range, not just until the first byte that differs").
            Intrinsic::RawEq | Intrinsic::CompareBytes => {
                let (a, b) = (vals.first().ok_or(malformed("a byte comparison's arguments"))?.0, vals.get(1).ok_or(malformed("a byte comparison's arguments"))?.0);
                let n = match i {
                    Intrinsic::RawEq => self.size_align(*targs.first().ok_or(malformed("`raw_eq` with no type"))?)?.0,
                    _ => u64::try_from(int(2)?).map_err(|_| malformed("a `usize` past 64 bits"))?,
                };
                let (pa, pb) = (self.deref(a)?.ptr, self.deref(b)?.ptr);
                let mut left: Vec<u8, Alloc<'a>> = Vec::new_in(self.alloc);
                let mut right: Vec<u8, Alloc<'a>> = Vec::new_in(self.alloc);
                for k in 0..n {
                    for (p, out) in [(pa, &mut left), (pb, &mut right)] {
                        match self.read(p.at(k), 1)? {
                            Scalar::Int(byte) => out.push(byte as u8),
                            Scalar::Ptr(_) => return Err(Stop::Refused(Refusal::PointerAsInt)),
                        }
                    }
                }
                let order = blaze_build::intrinsics::byte_order(&left, &right);
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
        Ok(match (a, b) {
            (Val::S(Scalar::Int(x)), Val::S(Scalar::Int(y))) => match self.node(t)? {
                PNode::Float(FloatTy::F64) => f64::from_bits(x as u64) == f64::from_bits(y as u64),
                PNode::Float(FloatTy::F32) => f32::from_bits(x as u32) == f32::from_bits(y as u32),
                PNode::Float(f @ (FloatTy::F16 | FloatTy::F128)) => Format::of(f).compare(x, y) == Some(core::cmp::Ordering::Equal),
                _ => x == y,
            },
            (Val::Pair(p, Scalar::Int(n)), Val::Pair(q, Scalar::Int(m))) => {
                if n != m {
                    return Ok(false);
                }
                let (pa, pb) = (self.deref(Val::Pair(p, Scalar::Int(n)))?, self.deref(Val::Pair(q, Scalar::Int(m)))?);
                for k in 0..n as u64 {
                    if self.read(pa.ptr.at(k), 1)? != self.read(pb.ptr.at(k), 1)? {
                        return Ok(false);
                    }
                }
                true
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
                let a = self.mem.get(p.alloc)?;
                (a.live && a.origin == Origin::Data && a.len() == size).then_some(p.alloc)
            }
            _ => None,
        };
        let root = match whole {
            Some(id) => id,
            None => {
                let root = self.mem.allocate(size, align, true, Origin::Data)?;
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
            bytes.extend_from_slice(self.mem.bytes(id)?);
            // Bytes never written (padding) are zero in the record.
            self.mem.zero_unwritten(id, &mut bytes);
            let a = self.mem.get(id)?;
            let mut relocs = alloc::vec::Vec::new_in(self.alloc);
            let pairs: Vec<(u64, AllocId), Alloc<'a>> = {
                let mut v = Vec::new_in(self.alloc);
                v.extend_from_slice(self.mem.relocs(id)?);
                v
            };
            let align_log2 = a.align_log2;
            for (off, target) in pairs.iter().copied() {
                let t = self.mem.get(target)?;
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
                PNode::Tuple(l) => {
                    let len = self.ground.view().list(l).map_err(|_| malformed("a list past the ground table"))?.len() as u32;
                    for f in 0..len {
                        let (off, ft) = (self.field_offset(ty, 0, f)?, self.field_ty(ty, 0, f)?);
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
    let work = codes.take_work();
    let mut m = Machine::new(world, layout, ground, Limits::DEFAULT, alloc, work);
    m.plant = plant;
    let out = m.evaluate(codes, item, List::EMPTY);
    let at = m.at;
    codes.put_work(m.into_work());
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
