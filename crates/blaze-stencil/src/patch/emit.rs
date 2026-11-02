//! Writing the function (stencil.md, "What a compile is", steps 4 to 7):
//!
//! - [`edges`]: each branch's successors, which of them have code of their own (a landing
//!   pad, a back-edge count or block-argument moves, written after the block at a label of
//!   their own) and which jump straight to their block; a `BrIcmp` whose taken edge is the
//!   code that follows is complemented so it falls through;
//! - [`size`]: an upper bound of the code, for the one reservation, and each stencil's index;
//! - [`labels`]: a label per block and per edge with code;
//! - [`function`]: the prologue, then per block in layout order each instruction's stencil
//!   (copied, its tail dropped when the next code follows, each hole patched) or glue, then
//!   the block's edges, then the trap stubs and the constants.
//!
//! The code that follows a block's terminator is its first edge with code written after it,
//! or else the next block: a branch to it is dropped (a `jump`) or falls through (`brif`'s
//! `Next`).

use blaze_ids::bits::BitArray;
use blaze_mem::frame::Frame;
use cranelift_codegen::ir::{self, BlockArg};

use super::{
    Edge, Kind, Konst, KonstRef, NONE, Plan, Sel, Side,
    scan::{reset, zero_extend},
    select::{complement, immediate, int_cc, ty_of},
    slots::has,
};
use crate::{
    Error, Target,
    arch::{Cx, Glue, Loc},
    buf::{Buf, Label},
    enc,
    shape::{Dst, Op, Shape, Src, Ty},
    table::format::{HoleKind, Reader, Stencil},
};

/// The alignment of a function's code: its constants are 16-aligned in it.
pub const ALIGN: u32 = 16;

const JUMP: Shape = Shape { op: Op::Jump, ty: Ty::I8, aux: 0, srcs: [Src::None; 3], dst: Dst::None };

/// The one reservation of a function: bytes, labels and fixups of the buffer, and lines.
pub(super) struct Size {
    pub(super) bytes: usize,
    pub(super) labels: usize,
    pub(super) fixups: usize,
    pub(super) lines: usize,
}

/// One block argument's move on an edge.
#[derive(Clone, Copy, Debug)]
pub(super) struct Move {
    from: From,
    /// The parameter's slot, from the frame register.
    to: u32,
    ty: Ty,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum From {
    /// A slot, from the frame register.
    Slot(u32),
    /// A constant no slot holds, materialized into the parameter's slot.
    Konst(Konst),
}

/// Where a stencil's `Next` hole goes.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Next {
    /// The code right after it: its tail is dropped.
    Falls,
    To(Label),
}

/// What a stencil's holes are patched with.
#[derive(Clone, Copy)]
struct Fill {
    /// The frame offset of each operand from a slot (a relay slot's for a far one).
    srcs: [u32; 3],
    /// The frame offset of each result's slot.
    dsts: [u32; 2],
    /// Each immediate operand, sign-extended from its width, and the width.
    imms: [(i64, u32); 3],
    offset: i64,
    target: Option<Label>,
    next: Next,
    trap: Option<ir::TrapCode>,
    konst: KonstRef,
}

impl Fill {
    const NONE: Fill = Fill {
        srcs: [0; 3],
        dsts: [0; 2],
        imms: [(0, 64); 3],
        offset: 0,
        target: None,
        next: Next::Falls,
        trap: None,
        konst: KonstRef::None,
    };
}

/// What follows a block's terminator: an edge (by its index in [`Plan::edges`]) or a block.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Place {
    Edge(u32),
    Block(ir::Block),
    End,
}

/// Each branch's edges, and the orientation of its `BrIcmp`.
pub(super) fn edges(plan: &mut Plan<'_>, func: &ir::Function, library: &Reader<'_>) -> Result<(), Error> {
    plan.edges.clear();
    reset(&mut plan.edges_of, func.dfg.num_insts(), (NONE, 0));
    for block in func.layout.blocks() {
        let Some(term) = func.layout.last_inst(block) else {
            // alloc: the error's message, once per failed compile.
            return Err(Error::new(format!("{block} has no instructions")));
        };
        let first = plan.edges.len() as u32;
        let data = &func.dfg.insts[term];
        match data.exception_table() {
            Some(table) => {
                let table = &func.dfg.exception_tables[table];
                edge(plan, func, block, term, *table.normal_return(), false)?;
                for item in table.items() {
                    match item {
                        ir::ExceptionTableItem::Tag(_, call) | ir::ExceptionTableItem::Default(call) => {
                            edge(plan, func, block, term, call, true)?;
                        }
                        ir::ExceptionTableItem::Context(_) => {}
                    }
                }
            }
            None => {
                for &call in data.branch_destination(&func.dfg.jump_tables, &func.dfg.exception_tables)
                {
                    edge(plan, func, block, term, call, false)?;
                }
            }
        }
        let count = plan.edges.len() as u32 - first;
        if count > 0 {
            plan.edges_of[term.as_u32() as usize] = (first, count);
        }
        orient(plan, func, library, block, term);
    }
    Ok(())
}

fn edge(
    plan: &mut Plan<'_>,
    func: &ir::Function,
    from: ir::Block,
    term: ir::Inst,
    call: ir::BlockCall,
    landing: bool,
) -> Result<(), Error> {
    let pool = &func.dfg.value_lists;
    let block = call.block(pool);
    let bix = plan.bix[block.as_u32() as usize];
    if bix == NONE {
        // alloc: the error's message, once per failed compile.
        return Err(Error::at(term, format!("a branch to {block}, which is not in the layout")));
    }
    let back = bix <= plan.bix[from.as_u32() as usize];
    let params = func.dfg.block_params(block);
    let moves = call.args(pool).zip(params).any(|(arg, &param)| {
        let to = plan.slot[param.as_u32() as usize];
        to != NONE
            && match arg {
                BlockArg::Value(value) => {
                    plan.slot[func.dfg.resolve_aliases(value).as_u32() as usize] != to
                }
                BlockArg::TryCallRet(_) | BlockArg::TryCallExn(_) => true,
            }
    });
    plan.edges.push(Edge { block, call, label: Label(NONE), own: landing || back || moves, back, landing });
    Ok(())
}

/// The place of the code edge `at` starts at: its own, or its block's.
fn start(plan: &Plan<'_>, at: u32) -> Place {
    let edge = &plan.edges[at as usize];
    if edge.own { Place::Edge(at) } else { Place::Block(edge.block) }
}

/// The edges of `term` written inline: a `try_call`'s normal return, right after the call.
fn inline(func: &ir::Function, term: ir::Inst) -> u32 {
    u32::from(func.dfg.insts[term].exception_table().is_some())
}

/// What follows `term`, the terminator of `block`.
fn follows(plan: &Plan<'_>, func: &ir::Function, block: ir::Block, term: ir::Inst) -> Place {
    let (first, count) = plan.edges_of[term.as_u32() as usize];
    if first != NONE {
        for at in first + inline(func, term)..first + count {
            if plan.edges[at as usize].own {
                return Place::Edge(at);
            }
        }
    }
    match func.layout.next_block(block) {
        Some(next) => Place::Block(next),
        None => Place::End,
    }
}

/// Complements a `BrIcmp` whose taken edge follows and whose other edge does not.
fn orient(plan: &mut Plan<'_>, func: &ir::Function, library: &Reader<'_>, block: ir::Block, term: ir::Inst) {
    let sel = plan.sels[term.as_u32() as usize];
    if sel.kind != Kind::Stencil || sel.shape.op != Op::BrIcmp {
        return;
    }
    let (first, _) = plan.edges_of[term.as_u32() as usize];
    let follow = follows(plan, func, block, term);
    if start(plan, first) != follow || start(plan, first + 1) == follow {
        return;
    }
    let cc = complement(sel.cc);
    let shape = Shape { aux: int_cc(cc) as u8, ..sel.shape };
    if has(library, shape) {
        let sel = &mut plan.sels[term.as_u32() as usize];
        sel.shape = shape;
        sel.cc = cc;
        sel.swapped = true;
    }
}

fn copy(ty: Ty) -> Shape {
    Shape { op: Op::Copy, ty, aux: 0, srcs: [Src::Slot, Src::None, Src::None], dst: Dst::Slot }
}

/// The stencil writing `konst` into a slot of `ty`, and its `Imm0`.
fn materialize(konst: Konst, ty: Ty) -> Option<(Shape, i64)> {
    let op = match konst {
        Konst::Int(_) => Op::Iconst,
        Konst::F32(_) | Konst::F64(_) => Op::Fconst,
        Konst::None | Konst::F16(_) | Konst::Wide => return None,
    };
    let (imm, wide) = immediate(konst);
    let shape = Shape { op, ty, aux: u8::from(wide), srcs: [Src::None; 3], dst: Dst::Slot };
    Some((shape, imm))
}

fn missing(inst: ir::Inst, shape: Shape, library: &Reader<'_>) -> Error {
    // alloc: the error's message, once per failed compile.
    Error::at(inst, format!("no `{shape}` stencil in the {:?} library", library.variant()))
}

/// The bound of the code, and each stencil's index.
pub(super) fn size<A: Glue>(
    plan: &mut Plan<'_>,
    func: &ir::Function,
    target: &Target,
    frame: &A::Frame,
    library: &Reader<'_>,
) -> Result<Size, Error> {
    let fixed = A::bound_fixed(frame) as usize;
    let jump = library.find(JUMP.key()).map_or(0, |at| library.stencil(at).code.len());
    let len = |shape: Shape| library.find(shape.key()).map_or(0, |at| library.stencil(at).code.len());
    let mut bytes = fixed;
    let mut fixups = 0usize;
    let mut consts = 0usize;
    let mut insts = 0usize;
    let mut codes = BitArray::<4, u8>::EMPTY;
    let mut mark = |code: ir::TrapCode| {
        codes.insert(code.as_raw().get());
    };
    let mut glue_stubs = 0usize;
    for block in func.layout.blocks() {
        for inst in func.layout.block_insts(block) {
            insts += 1;
            let i = inst.as_u32() as usize;
            match plan.sels[i].kind {
                Kind::Stencil => {
                    let sel = plan.sels[i];
                    let at = library.find(sel.shape.key()).ok_or_else(|| missing(inst, sel.shape, library))?;
                    plan.sels[i].stencil = at;
                    let stencil = library.stencil(at);
                    bytes += stencil.code.len();
                    for hole in stencil.holes() {
                        match hole.kind {
                            HoleKind::Trap => {
                                if let Some(code) = sel.trap {
                                    mark(code);
                                }
                            }
                            HoleKind::TrapOverflow => mark(ir::TrapCode::INTEGER_OVERFLOW),
                            HoleKind::Const | HoleKind::Rodata => consts += 1,
                            _ => {}
                        }
                        fixups += usize::from(hole.kind.is_place());
                    }
                    if plan.relays {
                        bytes += fixed * (usize::from(sel.nargs) + func.dfg.inst_results(inst).len());
                    }
                }
                Kind::Glue => {
                    bytes += A::bound(func, target, inst) as usize;
                    glue_stubs += usize::from(func.dfg.insts[inst].opcode().can_trap());
                }
                Kind::Jump => bytes += jump,
                Kind::Skip | Kind::Fused => {}
            }
        }
    }
    let pool = &func.dfg.value_lists;
    for edge in &plan.edges {
        bytes += jump + fixed * (usize::from(edge.landing) + usize::from(edge.back));
        fixups += 1;
        if !edge.own {
            continue;
        }
        let params = func.dfg.block_params(edge.block);
        for (arg, &param) in edge.call.args(pool).zip(params) {
            if plan.slot[param.as_u32() as usize] == NONE {
                continue;
            }
            let Some(ty) = ty_of(func.dfg.value_type(param)) else { continue };
            let konst = match arg {
                BlockArg::Value(value) => plan.konst[func.dfg.resolve_aliases(value).as_u32() as usize],
                BlockArg::TryCallRet(_) | BlockArg::TryCallExn(_) => Konst::None,
            };
            let one = materialize(konst, ty).map_or(0, |(shape, _)| len(shape)).max(len(copy(ty)));
            // The move and, in a cycle, the save of what it overwrites.
            bytes += 2 * one;
            if plan.relays {
                bytes += 4 * fixed;
            }
        }
    }
    let stubs = codes.count() + glue_stubs + 1;
    bytes += stubs * fixed + (consts + 1) * ALIGN as usize;
    let labels = func.dfg.num_blocks() + plan.edges.len() + stubs + consts;
    Ok(Size { bytes, labels, fixups: fixups + plan.edges.len(), lines: insts })
}

/// A label per block in the layout and per edge with code of its own.
pub(super) fn labels(plan: &mut Plan<'_>, func: &ir::Function, buf: &mut Buf<'_>) {
    let Plan { blocks, edges, .. } = plan;
    blocks.clear();
    blocks.resize(func.dfg.num_blocks(), Label(NONE));
    for block in func.layout.blocks() {
        blocks[block.as_u32() as usize] = buf.label();
    }
    for edge in edges.iter_mut() {
        edge.label = if edge.own { buf.label() } else { blocks[edge.block.as_u32() as usize] };
    }
}

/// Writes the function into `buf`.
pub(super) fn function<'f, A: Glue>(
    buf: &mut Buf<'f>,
    plan: &Plan<'f>,
    side: &mut Side<'f>,
    func: &ir::Function,
    target: &Target,
    frame: &A::Frame,
    library: &Reader<'_>,
) -> Result<(), Error> {
    Out::<A> { buf, plan, side, func, target, frame, library }.function()
}

struct Out<'a, 'f, A: Glue> {
    buf: &'a mut Buf<'f>,
    plan: &'a Plan<'f>,
    side: &'a mut Side<'f>,
    func: &'a ir::Function,
    target: &'a Target,
    frame: &'a A::Frame,
    library: &'a Reader<'a>,
}

impl<'f, A: Glue> Out<'_, 'f, A> {
    fn cx(&mut self) -> Cx<'_, 'f, A> {
        Cx {
            buf: &mut *self.buf,
            func: self.func,
            target: self.target,
            frame: self.frame,
            plan: self.plan,
            side: &mut *self.side,
        }
    }

    fn function(&mut self) -> Result<(), Error> {
        let (func, plan) = (self.func, self.plan);
        A::prologue(&mut self.cx())?;
        for block in func.layout.blocks() {
            self.buf.bind(plan.block(block));
            for inst in func.layout.block_insts(block) {
                let sel = &plan.sels[inst.as_u32() as usize];
                match sel.kind {
                    Kind::Skip | Kind::Fused => continue,
                    Kind::Stencil => {
                        self.line(inst);
                        self.stencil(block, inst, sel)?;
                    }
                    Kind::Glue => {
                        self.line(inst);
                        A::inst(&mut self.cx(), inst)?;
                        if inline(func, inst) == 1 {
                            self.normal(block, inst)?;
                        }
                    }
                    Kind::Jump => {
                        self.line(inst);
                        let to = plan.edges_of(inst)[0].label;
                        if self.after(block, inst) != Some(to) {
                            self.jump(inst, to)?;
                        }
                    }
                }
            }
            self.edges(block)?;
        }
        self.finish()
    }

    fn line(&mut self, inst: ir::Inst) {
        self.side.line(self.buf.len(), self.func.srcloc(inst));
    }

    /// The label of the code that follows `term`, the terminator of `block`.
    fn after(&self, block: ir::Block, term: ir::Inst) -> Option<Label> {
        match follows(self.plan, self.func, block, term) {
            Place::Edge(at) => Some(self.plan.edges[at as usize].label),
            Place::Block(next) => Some(self.plan.block(next)),
            Place::End => None,
        }
    }

    /// The slot `at` (`bytes` wide) as a stencil reaches it: itself, or relay slot `n` after
    /// the glue copies it there.
    fn near(&mut self, inst: ir::Inst, at: u32, bytes: u8, n: u32) -> Result<u32, Error> {
        if at < A::NEAR[usize::from(bytes == 16)] {
            return Ok(at);
        }
        let relay = self.relay(inst, n)?;
        A::relay(&mut self.cx(), at, relay, bytes);
        Ok(relay)
    }

    fn relay(&self, inst: ir::Inst, n: u32) -> Result<u32, Error> {
        self.plan.relay(n).ok_or_else(|| Error::at(inst, "a far slot with no relay slots reserved"))
    }

    fn stencil(&mut self, block: ir::Block, inst: ir::Inst, sel: &Sel) -> Result<(), Error> {
        let (func, plan) = (self.func, self.plan);
        let shape = sel.shape;
        let mut fill = Fill { offset: sel.offset, trap: sel.trap, konst: sel.konst, ..Fill::NONE };
        if shape.op.info().branches {
            let edges = plan.edges_of(inst);
            let (taken, other) = if sel.swapped { (edges[1], edges[0]) } else { (edges[0], edges[1]) };
            fill.target = Some(taken.label);
            if self.after(block, inst) != Some(other.label) {
                fill.next = Next::To(other.label);
            }
        }
        match shape.op {
            // Sign-extended from the type's width, which `imm` zero-extends for a field that
            // takes no negative value.
            Op::Iconst | Op::Fconst => fill.imms[0] = (sel.imm, u32::from(shape.ty.bytes()) * 8),
            _ => {
                for (n, &arg) in sel.args().iter().enumerate() {
                    match shape.srcs[n] {
                        Src::Slot => {
                            let slot = plan.slot[arg.as_u32() as usize];
                            if slot == NONE {
                                // alloc: the error's message, once per failed compile.
                                return Err(Error::at(inst, format!("operand {arg} of `{shape}` has no slot")));
                            }
                            fill.srcs[n] = self.near(inst, plan.base + slot, shape.arg(n).slot(), n as u32)?;
                        }
                        Src::Imm => {
                            let Konst::Int(value) = plan.konst[arg.as_u32() as usize] else {
                                // alloc: the error's message, once per failed compile.
                                return Err(Error::at(inst, format!("operand {arg} of `{shape}` is no constant")));
                            };
                            // The stencil's operand type, narrower than the value's for a shift
                            // amount (an `i8` it masks).
                            fill.imms[n] = (value, u32::from(shape.arg(n).bytes()) * 8);
                        }
                        Src::Acc | Src::None => {}
                    }
                }
            }
        }
        if let Some(slot) = sel.stack {
            fill.offset += i64::from(plan.stack_slot(slot));
        }
        let results = func.dfg.inst_results(inst);
        let mut back = [None; 2];
        for (k, &result) in results.iter().enumerate().take(2) {
            if k == 0 && !matches!(shape.dst, Dst::Slot | Dst::Both) {
                continue;
            }
            let bytes = shape.result(k).slot();
            fill.dsts[k] = match plan.slot[result.as_u32() as usize] {
                // Nothing reads it: a scratch slot takes the write.
                NONE => 16 * k as u32,
                slot if plan.base + slot < A::NEAR[usize::from(bytes == 16)] => plan.base + slot,
                slot => {
                    back[k] = Some((plan.base + slot, bytes));
                    self.relay(inst, 3 + k as u32)?
                }
            };
        }
        let stencil = self.library.stencil(sel.stencil);
        self.put(inst, &stencil, shape, &fill)?;
        for (k, back) in back.into_iter().enumerate() {
            if let Some((to, bytes)) = back {
                A::relay(&mut self.cx(), fill.dsts[k], to, bytes);
            }
        }
        Ok(())
    }

    /// Copies `stencil` and patches its holes from `fill`.
    fn put(&mut self, inst: ir::Inst, stencil: &Stencil<'_>, shape: Shape, fill: &Fill) -> Result<(), Error> {
        let code = if fill.next == Next::Falls { stencil.body() } else { stencil.code };
        let at = self.buf.len();
        self.buf.put(code);
        let end = i64::from(self.buf.len());
        if stencil.faults().len() > 0 {
            self.faults(inst, stencil, at, code.len(), shape)?;
        }
        for hole in stencil.holes() {
            if usize::from(hole.at) >= code.len() {
                continue;
            }
            let place = at + u32::from(hole.at);
            let addend = i64::from(hole.addend);
            let to = match hole.kind {
                HoleKind::Target => Some(fill.target.ok_or_else(|| {
                    // alloc: the error's message, once per failed compile.
                    Error::at(inst, format!("`{shape}` has a `Target` hole and no edge"))
                })?),
                HoleKind::Trap => {
                    let code = fill.trap.ok_or_else(|| {
                        // alloc: the error's message, once per failed compile.
                        Error::at(inst, format!("`{shape}` has a `Trap` hole and no trap code"))
                    })?;
                    Some(self.side.trap(self.buf, code))
                }
                HoleKind::TrapOverflow => Some(self.side.trap(self.buf, ir::TrapCode::INTEGER_OVERFLOW)),
                HoleKind::Const => {
                    let (pool, index, bytes) = self.konst(inst, fill.konst)?;
                    Some(self.side.konst(self.buf, pool, index, bytes))
                }
                _ => None,
            };
            if let Some(to) = to {
                self.buf.fixup(place, hole.form, to, addend);
                continue;
            }
            let value = match hole.kind {
                HoleKind::Slot0 => i64::from(fill.srcs[0]) + addend,
                HoleKind::Slot1 => i64::from(fill.srcs[1]) + addend,
                HoleKind::Slot2 => i64::from(fill.srcs[2]) + addend,
                HoleKind::Result0 => i64::from(fill.dsts[0]) + addend,
                HoleKind::Result1 => i64::from(fill.dsts[1]) + addend,
                HoleKind::Imm0 => imm(hole.form, fill.imms[0], addend),
                HoleKind::Imm1 => imm(hole.form, fill.imms[1], addend),
                HoleKind::Imm2 => imm(hole.form, fill.imms[2], addend),
                // A confined access adds its offset in 32 bits, which wrap.
                HoleKind::Offset => {
                    imm(hole.form, (fill.offset, if shape.confined().is_some() { 32 } else { 64 }), addend)
                }
                HoleKind::Next => match fill.next {
                    Next::Falls => end + addend - i64::from(place),
                    Next::To(to) => {
                        self.buf.fixup(place, hole.form, to, addend);
                        continue;
                    }
                },
                HoleKind::Rodata => {
                    let (entry, addend) = hole.rodata();
                    let to = self.side.rodata(self.buf, entry);
                    self.buf.fixup(place, hole.form, to, addend);
                    continue;
                }
                HoleKind::Target | HoleKind::Trap | HoleKind::TrapOverflow | HoleKind::Const => continue,
            };
            self.buf.patch(place, hole.form, value).map_err(|e| {
                // alloc: the error's message, once per failed compile.
                Error::at(inst, format!("the {:?} hole of `{shape}`: {e}", hole.kind))
            })?;
        }
        Ok(())
    }

    /// Records where the stencil copied at `at` (`len` bytes of it) accesses the
    /// instruction's memory, with its `MemFlags` trap code ([`Code::traps`]); a `notrap`
    /// access records nothing, as in Cranelift.
    ///
    /// [`Code::traps`]: crate::code::Code::traps
    fn faults(&mut self, inst: ir::Inst, stencil: &Stencil<'_>, at: u32, len: usize, shape: Shape) -> Result<(), Error> {
        let Some(flags) = self.func.dfg.insts[inst].memflags() else {
            // alloc: the error's message, once per failed compile.
            return Err(Error::at(inst, format!("`{shape}` accesses memory and the instruction has no flags")));
        };
        let Some(code) = self.func.dfg.mem_flags[flags].trap_code() else { return Ok(()) };
        for fault in stencil.faults() {
            if usize::from(fault) < len {
                self.side.trap_sites.push((at + u32::from(fault), code));
            }
        }
        Ok(())
    }

    /// A `Const` hole's constant: whether it is the pool's (else an immediate's), its handle's
    /// index and its 16 bytes.
    fn konst(&self, inst: ir::Inst, konst: KonstRef) -> Result<(bool, u32, [u8; 16]), Error> {
        let (pool, index, data) = match konst {
            KonstRef::Pool(handle) => (true, handle.as_u32(), self.func.dfg.constants.get(handle).as_slice()),
            KonstRef::Immediate(imm) => match self.func.dfg.immediates.get(imm) {
                Some(data) => (false, imm.as_u32(), data.as_slice()),
                // alloc: the error's message, once per failed compile.
                None => return Err(Error::at(inst, format!("{imm} is not in the function"))),
            },
            KonstRef::None => return Err(Error::at(inst, "a `Const` hole of an instruction with no constant")),
        };
        if data.len() > 16 {
            // alloc: the error's message, once per failed compile.
            return Err(Error::at(inst, format!("a constant of {} bytes, more than a `Const` hole's 16", data.len())));
        }
        let mut bytes = [0u8; 16];
        bytes[..data.len()].copy_from_slice(data);
        Ok((pool, index, bytes))
    }

    fn jump(&mut self, inst: ir::Inst, to: Label) -> Result<(), Error> {
        let at = self.library.find(JUMP.key()).ok_or_else(|| missing(inst, JUMP, self.library))?;
        let stencil = self.library.stencil(at);
        self.put(inst, &stencil, JUMP, &Fill { next: Next::To(to), ..Fill::NONE })
    }

    /// A `try_call`'s normal edge, right after the glue's call.
    fn normal(&mut self, block: ir::Block, term: ir::Inst) -> Result<(), Error> {
        let plan = self.plan;
        let edge = plan.edges_of(term)[0];
        if edge.own {
            self.buf.bind(edge.label);
            self.body(term, &edge)?;
        }
        let to = plan.block(edge.block);
        if self.after(block, term) != Some(to) {
            self.jump(term, to)?;
        }
        Ok(())
    }

    /// The edges of `block`'s terminator with code of their own, each followed by its jump to
    /// its block unless that block's code follows.
    fn edges(&mut self, block: ir::Block) -> Result<(), Error> {
        let (func, plan) = (self.func, self.plan);
        let Some(term) = func.layout.last_inst(block) else { return Ok(()) };
        let edges = plan.edges_of(term);
        let next = func.layout.next_block(block).map(|next| plan.block(next));
        for (at, edge) in edges.iter().enumerate().skip(inline(func, term) as usize) {
            if !edge.own {
                continue;
            }
            self.buf.bind(edge.label);
            if edge.landing {
                A::landing(&mut self.cx(), term)?;
            }
            self.body(term, edge)?;
            let to = plan.block(edge.block);
            let follows = edges[at + 1..].iter().find(|it| it.own).map(|it| it.label).or(next);
            if follows != Some(to) {
                self.jump(term, to)?;
            }
        }
        Ok(())
    }

    /// An edge's back-edge count and moves.
    fn body(&mut self, term: ir::Inst, edge: &Edge) -> Result<(), Error> {
        if edge.back {
            A::count_back_edge(&mut self.cx());
        }
        let (func, plan) = (self.func, self.plan);
        let pool = &func.dfg.value_lists;
        let mut moves = std::mem::replace(&mut self.side.moves, Vec::new_in(self.buf.temp()));
        moves.clear();
        for (arg, &param) in edge.call.args(pool).zip(func.dfg.block_params(edge.block)) {
            let to = match plan.slot[param.as_u32() as usize] {
                NONE => continue,
                slot => plan.base + slot,
            };
            let param_ty = func.dfg.value_type(param);
            let ty = ty_of(param_ty).ok_or_else(|| {
                // alloc: the error's message, once per failed compile.
                Error::at(term, format!("a block parameter of {param_ty}, which no stencil moves"))
            })?;
            let from = match arg {
                BlockArg::Value(value) => {
                    let value = func.dfg.resolve_aliases(value);
                    match plan.slot[value.as_u32() as usize] {
                        NONE => match plan.konst[value.as_u32() as usize] {
                            konst @ (Konst::Int(_) | Konst::F32(_) | Konst::F64(_)) => From::Konst(konst),
                            // alloc: the error's message, once per failed compile.
                            _ => return Err(Error::at(term, format!("block argument {value} has no place"))),
                        },
                        slot => From::Slot(plan.base + slot),
                    }
                }
                BlockArg::TryCallRet(k) => match plan.returned(term, k as usize) {
                    Loc::Slot(slot) => From::Slot(slot),
                    // alloc: the error's message, once per failed compile.
                    _ => return Err(Error::at(term, format!("return value {k} has no place"))),
                },
                BlockArg::TryCallExn(k) => match plan.payload(term, k as usize) {
                    Loc::Slot(slot) => From::Slot(slot),
                    // alloc: the error's message, once per failed compile.
                    _ => return Err(Error::at(term, format!("exception payload {k} has no place"))),
                },
            };
            if from != From::Slot(to) {
                moves.push(Move { from, to, ty });
            }
        }
        let done = self.parallel(term, &mut moves);
        self.side.moves = moves;
        done
    }

    /// Writes `moves` as if at once: each move whose slot no other move reads goes first, and
    /// a cycle is broken by saving one slot's value to the glue's scratch slot.
    fn parallel(&mut self, term: ir::Inst, moves: &mut Vec<Move, &Frame>) -> Result<(), Error> {
        let scratch = self.plan.scratch()[0];
        while !moves.is_empty() {
            let ready = (0..moves.len()).find(|&at| {
                let to = From::Slot(moves[at].to);
                moves.iter().all(|it| it.from != to)
            });
            match ready {
                Some(at) => {
                    let one = moves.swap_remove(at);
                    self.one(term, one)?;
                }
                None => {
                    let Move { to, ty, .. } = moves[0];
                    self.one(term, Move { from: From::Slot(to), to: scratch, ty })?;
                    for it in moves.iter_mut() {
                        if it.from == From::Slot(to) {
                            it.from = From::Slot(scratch);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// One move: a copy stencil, or a constant's.
    fn one(&mut self, term: ir::Inst, one: Move) -> Result<(), Error> {
        let bytes = one.ty.slot();
        let mut fill = Fill::NONE;
        let shape = match one.from {
            From::Slot(from) => {
                fill.srcs[0] = self.near(term, from, bytes, 0)?;
                copy(one.ty)
            }
            From::Konst(konst) => {
                let Some((shape, imm)) = materialize(konst, one.ty) else {
                    // alloc: the error's message, once per failed compile.
                    return Err(Error::at(term, format!("a constant block argument of {}", one.ty.name())));
                };
                fill.imms[0] = (imm, u32::from(shape.ty.bytes()) * 8);
                shape
            }
        };
        let far = one.to >= A::NEAR[usize::from(bytes == 16)];
        fill.dsts[0] = if far { self.relay(term, 3)? } else { one.to };
        let at = self.library.find(shape.key()).ok_or_else(|| missing(term, shape, self.library))?;
        let stencil = self.library.stencil(at);
        self.put(term, &stencil, shape, &fill)?;
        if far {
            A::relay(&mut self.cx(), fill.dsts[0], one.to, bytes);
        }
        Ok(())
    }

    /// The trap stubs, then the constants and the library rodata the code uses, 16-aligned.
    fn finish(&mut self) -> Result<(), Error> {
        let mut at = 0;
        while at < self.side.traps.len() {
            let (code, label) = self.side.traps[at];
            self.buf.bind(label);
            self.side.trap_sites.push((self.buf.len(), code));
            A::trap(&mut self.cx(), code);
            at += 1;
        }
        if self.side.const_order.is_empty() && self.side.rodata.is_empty() {
            return Ok(());
        }
        self.buf.align(ALIGN, A::NOP);
        for at in 0..self.side.const_order.len() {
            let (bytes, label) = self.side.const_order[at];
            self.buf.bind(label);
            self.buf.put(&bytes);
        }
        for at in 0..self.side.rodata.len() {
            let (entry, label) = self.side.rodata[at];
            self.buf.bind(label);
            self.buf.put(self.library.entry(entry));
        }
        Ok(())
    }
}

/// An `Imm` hole's value: the operand sign-extended from its width, or zero-extended when only
/// that fits the field (the same low bits).
#[inline]
fn imm(form: enc::Form, (value, bits): (i64, u32), addend: i64) -> i64 {
    let signed = value.wrapping_add(addend);
    if bits >= 64 || enc::check(form, signed).is_ok() {
        return signed;
    }
    zero_extend(value, bits).wrapping_add(addend)
}
