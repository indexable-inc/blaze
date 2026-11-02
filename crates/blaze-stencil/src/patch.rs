//! From one function's Cranelift IR to its [`crate::Code`], in one pass over the IR and one buffer
//! (docs/v2/plan/lanes/fastcompile/stencil.md, "What a compile is"): scan the values, give
//! each a slot, lay out the frame, reserve the buffer, then per block in layout order copy
//! each instruction's stencil and patch its holes (or have the glue write it), write the
//! edges' moves, the trap stubs and the constants, resolve the labels and hand back the
//! code.
//!
//! Every column is on the compile's frame (`blaze_mem::frame::task`), indexed by the IR's
//! entity numbers: a compile allocates nothing of the heap, and the frame is freed at once at
//! its end.
//!
//! The passes, each a walk in layout order over columns of the one before:
//!
//! - [`scan`]: each instruction's and block's place in the layout, each value's uses, last
//!   use and defining block, its constant, and the pure instructions nothing uses;
//! - [`select`]: what writes each instruction (nothing, the glue, or a stencil and its base
//!   shape: immediates folded where a stencil takes them, every other operand from its slot),
//!   the fusions, and which constants need a slot;
//! - [`slots`]: the accumulators (a result kept in a register for the next stencil), each
//!   value's slot by a linear scan, the stack slots and the area;
//! - the frame, by the glue ([`Glue::frame`]);
//! - [`emit`]: each branch's edges (which have code of their own, which fall through), the
//!   size of the code for the one reservation, the labels, then the code, the edges with
//!   their parallel moves, the stubs and the constants.

mod emit;
mod scan;
mod select;
mod slots;

#[cfg(test)]
mod tests;

use blaze_mem::{
    Tag,
    frame::{self, Frame},
};
use cranelift_codegen::{ir, packed_option::ReservedValue};

use crate::{
    Error, Target,
    arch::{Area, Cx, Glue, Loc},
    buf::{self, Buf, Label},
    code::{CallSite, CodeRef, Handler, Rule, SlotsRef, UnwindKind, UnwindRef},
    shape::{Dst, Op, Shape, Src, Ty},
    table::{Library, format::Reader},
};

pub use emit::ALIGN;

/// A column entry for "none": no slot, no edge, no place in the layout.
const NONE: u32 = u32::MAX;

/// A value's constant, when an instruction of the function makes it one.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum Konst {
    #[default]
    None,
    /// An `iconst`, sign-extended from its type's width.
    Int(i64),
    F16(u16),
    F32(u32),
    F64(u64),
    /// A `vconst` or `f128const`: always in a slot.
    Wide,
}

impl Konst {
    /// The bits the glue materializes for a [`Loc::Const`], if the glue can take it so.
    fn loc(self) -> Option<i64> {
        match self {
            Konst::Int(it) => Some(it),
            Konst::F32(it) => Some(i64::from(it)),
            Konst::F64(it) => Some(it as i64),
            Konst::None | Konst::F16(_) | Konst::Wide => None,
        }
    }
}

/// What writes an instruction.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum Kind {
    /// Nothing: a `nop`, a `sequence_point`, a pure instruction nothing uses, or a constant
    /// every use takes as an immediate or the glue materializes.
    #[default]
    Skip,
    /// What its one user computes in its own stencil: an `icmp` the next instruction
    /// compares, or a step of a confined access's address.
    Fused,
    /// The architecture's glue.
    Glue,
    /// A `jump`: its edge, then a jump stencil when its block does not follow.
    Jump,
    /// The stencil of [`Sel::shape`].
    Stencil,
}

/// Where the 16 bytes of a `Const` hole come from.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum KonstRef {
    #[default]
    None,
    Pool(ir::Constant),
    Immediate(ir::Immediate),
}

/// What [`select`] and [`slots`] decided of one instruction.
#[derive(Clone, Copy, Debug)]
struct Sel {
    kind: Kind,
    shape: Shape,
    /// The stencil's operands (an `icmp`'s for a fusion), aliases resolved.
    args: [ir::Value; 3],
    nargs: u8,
    /// The stencil's index in the library, found when the size is added up.
    stencil: u32,
    /// The value of an `Iconst`/`Fconst` stencil's `Imm0`.
    imm: i64,
    /// The `Offset` hole's value: the memory access's offset, or the offset into `stack`.
    offset: i64,
    /// A `stack_addr`'s slot, whose frame offset is added to `offset` when it is written.
    stack: Option<ir::StackSlot>,
    trap: Option<ir::TrapCode>,
    konst: KonstRef,
    /// A `BrIcmp`'s condition, which the emitter may complement to fall through.
    cc: ir::condcodes::IntCC,
    /// A `BrIcmp` whose condition was complemented: `Target` is the else edge and `Next` the
    /// then edge, which follows.
    swapped: bool,
}

const NO_SHAPE: Shape =
    Shape { op: Op::Copy, ty: Ty::I8, aux: 0, srcs: [Src::None; 3], dst: Dst::None };

impl Default for Sel {
    fn default() -> Sel {
        Sel {
            kind: Kind::Skip,
            shape: NO_SHAPE,
            args: [ir::Value::reserved_value(); 3],
            nargs: 0,
            stencil: NONE,
            imm: 0,
            offset: 0,
            stack: None,
            trap: None,
            konst: KonstRef::None,
            cc: ir::condcodes::IntCC::Equal,
            swapped: false,
        }
    }
}

impl Sel {
    fn args(&self) -> &[ir::Value] {
        &self.args[..usize::from(self.nargs)]
    }
}

/// One successor of a branch: its block, its arguments and where its code starts.
#[derive(Clone, Copy, Debug)]
struct Edge {
    block: ir::Block,
    call: ir::BlockCall,
    /// The block's label, or the edge's own when it has code ([`Edge::own`]).
    label: Label,
    /// Whether the edge has code of its own: a landing pad, a back-edge count or moves.
    own: bool,
    back: bool,
    landing: bool,
}

/// Where the values a glue instruction gives go: its results, or a `try_call`'s returns
/// (`TryCallRet`) and exception payload (`TryCallExn`).
#[derive(Clone, Copy, Debug)]
struct Places {
    /// Into [`Plan::pseudo`]: the returns, then the payload values.
    first: u32,
    rets: u32,
    exns: u32,
}

/// What the patcher decided of a function before writing it: each value's place, the labels
/// of blocks and edges, the stack slots' offsets. The glue reads it through
/// [`crate::arch::Cx`].
pub(crate) struct Plan<'f> {
    // `scan`.
    /// By instruction: its place in layout order.
    ord: Vec<u32, &'f Frame>,
    /// By block: its place in layout order, `NONE` when not in the layout.
    bix: Vec<u32, &'f Frame>,
    /// By value: its uses (once per operand), less those of instructions dropped as dead.
    uses: Vec<u32, &'f Frame>,
    /// By value: the place of its last use when every use is in its own block, else
    /// [`slots::GLOBAL`].
    last: Vec<u32, &'f Frame>,
    /// By value: the number of its defining block, `NONE` when none is in the layout.
    def_block: Vec<u32, &'f Frame>,
    konst: Vec<Konst, &'f Frame>,
    /// By instruction: a pure instruction nothing uses.
    dead: Vec<bool, &'f Frame>,
    // `select`.
    sels: Vec<Sel, &'f Frame>,
    /// By value: a constant some stencil reads from a slot.
    kslot: Vec<bool, &'f Frame>,
    // `slots`.
    /// By value: its slot's offset from the start of the value slots, `NONE` for none.
    slot: Vec<u32, &'f Frame>,
    /// By value: its local slot is free again.
    released: Vec<bool, &'f Frame>,
    /// By instruction: into `places`, for a glue instruction that gives values.
    places_of: Vec<u32, &'f Frame>,
    places: Vec<Places, &'f Frame>,
    /// The slots of glue instructions' results and of `try_call`s' returns and payloads,
    /// like `slot`.
    pseudo: Vec<u32, &'f Frame>,
    /// By stack slot: its offset from the frame register.
    stack: Vec<u32, &'f Frame>,
    /// The frame offset of the value slots: after the scratch slots.
    base: u32,
    /// Whether the relay slots of far operands are reserved (after the glue's two).
    relays: bool,
    area: (u32, u32),
    // Labels, made once the buffer is reserved.
    blocks: Vec<Label, &'f Frame>,
    edges: Vec<Edge, &'f Frame>,
    /// By instruction: its first edge in `edges` and how many, `(NONE, 0)` for none.
    edges_of: Vec<(u32, u32), &'f Frame>,
    /// `slots`' free lists.
    free: [Vec<u32, &'f Frame>; 2],
}

/// The glue's two 16-byte scratch slots, at the bottom of the area.
const SCRATCH: u32 = 32;
/// The patcher's relay slots of far operands: three operands and two results.
const RELAYS: u32 = 5;

impl<'f> Plan<'f> {
    pub(crate) fn new_in(frame: &'f Frame) -> Plan<'f> {
        Plan {
            ord: Vec::new_in(frame),
            bix: Vec::new_in(frame),
            uses: Vec::new_in(frame),
            last: Vec::new_in(frame),
            def_block: Vec::new_in(frame),
            konst: Vec::new_in(frame),
            dead: Vec::new_in(frame),
            sels: Vec::new_in(frame),
            kslot: Vec::new_in(frame),
            slot: Vec::new_in(frame),
            released: Vec::new_in(frame),
            places_of: Vec::new_in(frame),
            places: Vec::new_in(frame),
            pseudo: Vec::new_in(frame),
            stack: Vec::new_in(frame),
            base: 0,
            relays: false,
            area: (0, 0),
            blocks: Vec::new_in(frame),
            edges: Vec::new_in(frame),
            edges_of: Vec::new_in(frame),
            free: [Vec::new_in(frame), Vec::new_in(frame)],
        }
    }

    pub(crate) fn loc(&self, func: &ir::Function, value: ir::Value) -> Loc {
        let value = func.dfg.resolve_aliases(value);
        self.loc_of(value)
    }

    /// Where `value` (no alias) is.
    fn loc_of(&self, value: ir::Value) -> Loc {
        let i = value.as_u32() as usize;
        match self.slot.get(i) {
            Some(&slot) if slot != NONE => Loc::Slot(self.base + slot),
            _ => match self.konst.get(i).and_then(|it| it.loc()) {
                Some(bits) if self.uses[i] > 0 => Loc::Const(bits),
                _ => Loc::Dead,
            },
        }
    }

    fn pseudo_loc(&self, at: u32) -> Loc {
        match self.pseudo[at as usize] {
            NONE => Loc::Dead,
            slot => Loc::Slot(self.base + slot),
        }
    }

    fn places(&self, inst: ir::Inst) -> Option<&Places> {
        match self.places_of.get(inst.as_u32() as usize) {
            Some(&at) if at != NONE => Some(&self.places[at as usize]),
            _ => None,
        }
    }

    pub(crate) fn returned(&self, inst: ir::Inst, k: usize) -> Loc {
        match self.places(inst) {
            Some(places) if (k as u32) < places.rets => self.pseudo_loc(places.first + k as u32),
            _ => Loc::Dead,
        }
    }

    pub(crate) fn payload(&self, inst: ir::Inst, k: usize) -> Loc {
        match self.places(inst) {
            Some(places) if (k as u32) < places.exns => {
                self.pseudo_loc(places.first + places.rets + k as u32)
            }
            _ => Loc::Dead,
        }
    }

    pub(crate) fn block(&self, block: ir::Block) -> Label {
        self.blocks[block.as_u32() as usize]
    }

    pub(crate) fn edge(&self, inst: ir::Inst, i: usize) -> Label {
        self.edges_of(inst)[i].label
    }

    fn edges_of(&self, inst: ir::Inst) -> &[Edge] {
        match self.edges_of.get(inst.as_u32() as usize) {
            Some(&(first, count)) if first != NONE => {
                &self.edges[first as usize..(first + count) as usize]
            }
            _ => &[],
        }
    }

    pub(crate) fn stack_slot(&self, slot: ir::StackSlot) -> u32 {
        self.stack[slot.as_u32() as usize]
    }

    pub(crate) fn scratch(&self) -> [u32; 2] {
        [0, 16]
    }

    /// The patcher's relay slots, when reserved: after the glue's scratch.
    fn relay(&self, n: u32) -> Option<u32> {
        (self.relays && n < RELAYS).then_some(SCRATCH + 16 * n)
    }

    fn area(&self) -> Area {
        Area { size: self.area.0, align: self.area.1 }
    }
}

/// What is recorded while the function is written: unwind rows, call sites, trap stubs
/// wanted, lines.
pub(crate) struct Side<'f> {
    rows: Vec<(u32, Rule), &'f Frame>,
    /// Each recorded call: where it returns to, and the instruction.
    sites: Vec<(u32, ir::Inst), &'f Frame>,
    /// The stubs wanted, in the order first used.
    traps: Vec<(ir::TrapCode, Label), &'f Frame>,
    /// By trap code: its stub's label, `NONE` while none is wanted.
    trap_of: TrapOf,
    lines: Vec<(u32, ir::SourceLoc), &'f Frame>,
    /// Each place that traps, in the order written (by offset): memory accesses, then the
    /// stubs ([`crate::Code::traps`]).
    trap_sites: Vec<(u32, ir::TrapCode), &'f Frame>,
    /// By constant-pool handle and by immediate handle: the constant's label, `NONE` while
    /// not placed.
    pool_of: Vec<u32, &'f Frame>,
    imm_of: Vec<u32, &'f Frame>,
    /// In the order first used, for a deterministic layout.
    const_order: Vec<([u8; 16], Label), &'f Frame>,
    /// The library rodata entries placed, with their labels, in the order first used.
    rodata: Vec<(u32, Label), &'f Frame>,
    /// The parallel moves of the edge being written.
    moves: Vec<emit::Move, &'f Frame>,
    calls: Vec<CallSite, &'f Frame>,
    handlers: Vec<Handler, &'f Frame>,
    offsets: Vec<i32, &'f Frame>,
    /// Windows' unwind information, when the glue writes it ([`Cx::windows_info`]).
    info: Vec<u8, &'f Frame>,
}

/// By raw trap code: a label, `NONE` for none.
struct TrapOf([u32; 256]);

impl Default for TrapOf {
    fn default() -> TrapOf {
        TrapOf([NONE; 256])
    }
}

impl<'f> Side<'f> {
    fn new_in(frame: &'f Frame) -> Side<'f> {
        Side {
            rows: Vec::new_in(frame),
            sites: Vec::new_in(frame),
            traps: Vec::new_in(frame),
            trap_of: TrapOf::default(),
            lines: Vec::new_in(frame),
            trap_sites: Vec::new_in(frame),
            pool_of: Vec::new_in(frame),
            imm_of: Vec::new_in(frame),
            const_order: Vec::new_in(frame),
            rodata: Vec::new_in(frame),
            moves: Vec::new_in(frame),
            calls: Vec::new_in(frame),
            handlers: Vec::new_in(frame),
            offsets: Vec::new_in(frame),
            info: Vec::new_in(frame),
        }
    }

    pub(crate) fn retain_rows(&mut self, keep: impl FnMut(&(u32, Rule)) -> bool) {
        self.rows.retain(keep);
    }

    pub(crate) fn windows_info(&mut self) -> &mut Vec<u8, &'f Frame> {
        &mut self.info
    }

    pub(crate) fn rule(&mut self, at: u32, rule: Rule) {
        self.rows.push((at, rule));
    }

    pub(crate) fn rows(&self) -> &[(u32, Rule)] {
        &self.rows
    }

    pub(crate) fn call_site(&mut self, ret: u32, inst: ir::Inst) {
        self.sites.push((ret, inst));
    }

    #[inline]
    pub(crate) fn trap(&mut self, buf: &mut Buf<'_>, code: ir::TrapCode) -> Label {
        let at = usize::from(code.as_raw().get());
        if self.trap_of.0[at] != NONE {
            return Label(self.trap_of.0[at]);
        }
        let label = buf.label();
        self.trap_of.0[at] = label.0;
        self.traps.push((code, label));
        label
    }

    /// Records that the code from `at` is of `loc`.
    fn line(&mut self, at: u32, loc: ir::SourceLoc) {
        match self.lines.last_mut() {
            Some(last) if last.1 == loc => {}
            Some(last) if last.0 == at => {
                last.1 = loc;
                if self.lines.len() >= 2 && self.lines[self.lines.len() - 2].1 == loc {
                    self.lines.pop();
                }
            }
            None if loc.is_default() => {}
            _ => self.lines.push((at, loc)),
        }
    }

    /// The label of the constant `bytes` in the constant area, placed once per handle: the
    /// pool's `index` when `pool` (the pool keeps one handle per distinct bytes), else the
    /// immediate's.
    fn konst(&mut self, buf: &mut Buf<'_>, pool: bool, index: u32, bytes: [u8; 16]) -> Label {
        let table = if pool { &mut self.pool_of } else { &mut self.imm_of };
        let at = index as usize;
        if table.len() <= at {
            table.resize(at + 1, NONE);
        }
        if table[at] != NONE {
            return Label(table[at]);
        }
        let label = buf.label();
        table[at] = label.0;
        self.const_order.push((bytes, label));
        label
    }

    /// The label of library rodata entry `entry`, placed once. A function uses a few entries
    /// (float masks and bounds), so a scan of the ones placed beats a map.
    fn rodata(&mut self, buf: &mut Buf<'_>, entry: u32) -> Label {
        if let Some(&(_, label)) = self.rodata.iter().find(|it| it.0 == entry) {
            return label;
        }
        let label = buf.label();
        self.rodata.push((entry, label));
        label
    }

    /// The call sites with their handlers, now that every edge's label is bound.
    fn finish_calls(&mut self, func: &ir::Function, plan: &Plan<'_>, buf: &Buf<'_>) -> Result<(), Error> {
        buf::sort_by_key_in_place(&mut self.sites, |it| it.0);
        for &(ret, inst) in &self.sites {
            let first = self.handlers.len() as u32;
            if let Some(table) = func.dfg.insts[inst].exception_table() {
                let edges = plan.edges_of(inst);
                let mut handler = 1;
                for item in func.dfg.exception_tables[table].items() {
                    let landing = |handler: usize| {
                        let edge = edges.get(handler).ok_or_else(|| {
                            Error::at(inst, "an exception handler without an edge")
                        })?;
                        buf.offset(edge.label)
                            .ok_or_else(|| Error::at(inst, "a landing pad never placed"))
                    };
                    match item {
                        ir::ExceptionTableItem::Tag(tag, _) => {
                            let landing = landing(handler)?;
                            self.handlers.push(Handler::Tag { tag: tag.as_u32(), landing });
                            handler += 1;
                        }
                        ir::ExceptionTableItem::Default(_) => {
                            let landing = landing(handler)?;
                            self.handlers.push(Handler::Default { landing });
                            handler += 1;
                        }
                        ir::ExceptionTableItem::Context(_) => {
                            return Err(Error::at(
                                inst,
                                "an exception table's dynamic context, which no call site \
                                 record of the LSDA has a place for",
                            ));
                        }
                    }
                }
            }
            self.calls.push(CallSite { ret, handlers: first..self.handlers.len() as u32 });
        }
        Ok(())
    }
}

/// Compiles `func` for `target` with `A`'s glue and `library`'s stencils, handing the code to
/// `sink` while it is in the compile's frame.
pub fn compile<A: Glue, R>(
    func: &ir::Function,
    target: &Target,
    library: &Library,
    sink: impl FnOnce(&CodeRef<'_>) -> R,
) -> Result<R, Error> {
    compile_in::<A, R>(func, target, library.reader(), |code, _| sink(code))
}

/// What keeps `func` from compiling with `A`'s glue and `library`'s stencils: every
/// instruction selection refuses (each with its [`crate::Gap`]) or, when it refuses none, the
/// error the rest of the compile stops at. Empty when `func` compiles.
pub fn survey<A: Glue>(func: &ir::Function, target: &Target, library: &Library) -> Vec<Error> {
    survey_in::<A>(func, target, library.reader())
}

/// [`survey`] from a library's reader.
pub(crate) fn survey_in<A: Glue>(func: &ir::Function, target: &Target, reader: &Reader<'_>) -> Vec<Error> {
    // alloc: once per function surveyed, its refusals.
    let mut out = Vec::new();
    if func.layout.entry_block().is_none() {
        out.push(Error::new("a function with no blocks"));
        return out;
    }
    frame::task(Tag::Codegen, |temp| {
        let mut plan = Plan::new_in(temp);
        match scan::run(&mut plan, func) {
            Ok(()) => select::survey::<A>(&mut plan, func, target, reader, &mut out),
            Err(error) => out.push(error),
        }
    });
    if out.is_empty()
        && let Err(error) = compile_in::<A, ()>(func, target, reader, |_, _| ())
    {
        out.push(error);
    }
    out
}

/// [`compile`] from a library's reader, also showing `sink` the plan: the tests' own libraries
/// come in here, and the tests read where values went.
pub(crate) fn compile_in<A: Glue, R>(
    func: &ir::Function,
    target: &Target,
    library: &Reader<'_>,
    sink: impl FnOnce(&CodeRef<'_>, &Plan<'_>) -> R,
) -> Result<R, Error> {
    if func.layout.entry_block().is_none() {
        return Err(Error::new("a function with no blocks"));
    }
    frame::task(Tag::Codegen, |temp| -> Result<R, Error> {
        let (mut buf, mut plan, mut side) = (Buf::new_in(temp), Plan::new_in(temp), Side::new_in(temp));
        let (buf, plan, side) = (&mut buf, &mut plan, &mut side);
        scan::run(plan, func)?;
        select::run::<A>(plan, func, target, library)?;
        slots::run(plan, func, library, A::NEAR)?;
        let frame = A::frame(func, target, plan.area())?;
        emit::edges(plan, func, library)?;
        let size = emit::size::<A>(plan, func, target, &frame, library)?;
        buf.reserve(size.bytes, size.labels, size.fixups);
        side.lines.reserve(size.lines);
        emit::labels(plan, func, buf);
        emit::function::<A>(buf, plan, side, func, target, &frame, library)?;
        buf.resolve()?;
        let unwind = A::unwind(&mut Cx { buf, func, target, frame: &frame, plan, side })?;
        side.finish_calls(func, plan, buf)?;
        let (base, delta) = A::slot_base(&frame);
        side.offsets.extend(
            func.sized_stack_slots.keys().map(|slot| delta + plan.stack_slot(slot) as i32),
        );
        buf.sort_relocs();
        let unwind = unwind.map(|kind| match kind {
            UnwindKind::Dwarf { len } => UnwindRef::Dwarf { len, rows: &side.rows },
            UnwindKind::Windows { len } => UnwindRef::Windows { len, info: &side.info },
            UnwindKind::WindowsArm64 { len } => UnwindRef::WindowsArm64 { len, codes: &side.info },
        });
        let code = CodeRef {
            bytes: buf.bytes(),
            align: emit::ALIGN,
            relocs: buf.relocs(),
            unwind,
            calls: &side.calls,
            handlers: &side.handlers,
            lines: &side.lines,
            traps: &side.trap_sites,
            slots: SlotsRef { base, offsets: &side.offsets },
            frame: A::frame_size(&frame),
        };
        Ok(sink(&code, plan))
    })
}
