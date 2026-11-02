//! One function's frame on x86-64, laid out and entered as Cranelift 0.136.2's
//! `X64ABIMachineSpec` (`gen_prologue_frame_setup`, `gen_inline_probestack`,
//! `gen_clobber_save`, `gen_clobber_restore`, `gen_epilogue_frame_restore`, `gen_return`) and
//! `Callee::gen_prologue` write it, for a function that clobbers every callee-saved register
//! of its convention (stencils may write all of them):
//!
//! ```text
//!   caller's rsp -> incoming stack arguments
//!                   return address, saved rbp                          <- rbp
//!                   rbx, r12 .. r15 (System V)                     clobber bytes
//!                   rbx, rsi, rdi, r12 .. r15, xmm6 .. xmm15 (Windows)
//!                   the StructReturn pointer        16, when the function has one
//!                   the area (value and stack slots)                   <- r12
//!                   realignment                     align - 16, when align > 16
//!                   outgoing stack arguments                           <- rsp
//! ```
//!
//! Cranelift's fixed storage (its stack slots) starts where the area does, so `r12` is
//! Cranelift's stack slot base, `rsp + outgoing`, unless the area asks for more than 16 bytes
//! of alignment: then `r12` alone is rounded up, within `align - 16` bytes reserved for it
//! (what it does not use lies above the StructReturn pointer), and `rsp` stays where the
//! convention has it.

use std::alloc::Allocator;

use cranelift_codegen::{
    ir::{self, ArgumentPurpose, GlobalValueData, InstructionData, Opcode},
    isa::CallConv,
};

use super::{
    abi::{self, Part, Place},
    asm::{self, ADD, AND, Mem, R10, R11, R12, RBP, RSP, SUB, at},
};
use crate::{Error, Os, Target, TlsModel, arch::Area, buf::Buf, code::Rule, enc::Form};

/// A register a prologue saves.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Save {
    Gpr(u8),
    Xmm(u8),
}

/// System V's callee-saved registers (but `rbp`, the frame pointer), in Cranelift's order
/// (hardware number), each with its offset into the save area: `compute_clobber_size` makes
/// 40 bytes 48.
const SYSV: [(Save, u32); 5] = [
    (Save::Gpr(asm::RBX), 0),
    (Save::Gpr(asm::R12), 8),
    (Save::Gpr(asm::R13), 16),
    (Save::Gpr(asm::R14), 24),
    (Save::Gpr(asm::R15), 32),
];
const SYSV_BYTES: u32 = 48;

/// Windows': the general ones, then `xmm6` to `xmm15` 16-aligned.
const WIN: [(Save, u32); 17] = [
    (Save::Gpr(asm::RBX), 0),
    (Save::Gpr(asm::RSI), 8),
    (Save::Gpr(asm::RDI), 16),
    (Save::Gpr(asm::R12), 24),
    (Save::Gpr(asm::R13), 32),
    (Save::Gpr(asm::R14), 40),
    (Save::Gpr(asm::R15), 48),
    (Save::Xmm(6), 64),
    (Save::Xmm(7), 80),
    (Save::Xmm(8), 96),
    (Save::Xmm(9), 112),
    (Save::Xmm(10), 128),
    (Save::Xmm(11), 144),
    (Save::Xmm(12), 160),
    (Save::Xmm(13), 176),
    (Save::Xmm(14), 192),
    (Save::Xmm(15), 208),
];
const WIN_BYTES: u32 = 224;

/// The DWARF numbers of the general registers, by hardware number; `xmm<n>` is `17 + n`.
const DWARF_GPR: [u16; 16] = [0, 2, 1, 3, 7, 6, 4, 5, 8, 9, 10, 11, 12, 13, 14, 15];
const DWARF_XMM0: u16 = 17;
pub const DWARF_RBP: u16 = 6;
pub const DWARF_RSP: u16 = 7;
pub const DWARF_R12: u16 = 12;

pub fn dwarf(save: Save) -> u16 {
    match save {
        Save::Gpr(reg) => DWARF_GPR[usize::from(reg)],
        Save::Xmm(reg) => DWARF_XMM0 + u16::from(reg),
    }
}

/// How the prologue touches the pages of a frame larger than one: Cranelift's
/// `gen_inline_probestack`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Probe {
    #[default]
    None,
    /// `count` (at most 4) steps of `guard` bytes, each touched, then `rsp` put back.
    Unroll { count: u32, guard: u32 },
    /// `StackProbeLoop`: a loop over `size` bytes (the frame rounded up to `guard`).
    Loop { size: u32, guard: u32 },
}

/// Cranelift's `PROBE_MAX_UNROLL` on x64.
const PROBE_MAX_UNROLL: u32 = 4;

/// Cranelift's `maximum_frame_size`.
const FRAME_LIMIT: u32 = 1 << 30;

/// One function's frame on x86-64.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Frame {
    /// The function's convention is Windows': its save set.
    pub win: bool,
    /// The target's unwind information is Windows'.
    pub win_unwind: bool,
    /// Bytes of the saved registers.
    pub clobber: u32,
    /// Bytes between the saves and the outgoing arguments: Cranelift's fixed storage.
    pub fixed: u32,
    /// Bytes of the largest call's stack arguments.
    pub outgoing: u32,
    /// The area's bytes and alignment.
    pub size: u32,
    pub align: u32,
    /// Where the function's own StructReturn pointer is kept, from `r12`.
    pub sret: Option<u32>,
    pub probe: Probe,
    /// The most bytes of the prologue.
    pub prologue: u32,
    /// A `tail` function's incoming argument area, which its `ret` pops: Cranelift's
    /// `tail_args_size`, the most of its own stack arguments' and its `return_call`s', 16-aligned.
    /// 0 in the other conventions, whose caller pops.
    pub pop: u32,
    /// How much the prologue grows the incoming argument area by, for a `return_call` of more
    /// stack arguments than the function was given: `pop` less its own stack arguments.
    pub grow: u32,
}

impl Frame {
    pub fn saves(&self) -> &'static [(Save, u32)] {
        if self.win { &WIN } else { &SYSV }
    }

    /// `rsp` to `rbp`.
    pub fn total(&self) -> u32 {
        self.clobber + self.fixed + self.outgoing
    }
}

/// What the function's instructions ask of the frame, or an error for the one the glue
/// cannot write.
struct Walk {
    outgoing: u32,
    calls: bool,
    /// The most stack argument bytes of a `return_call`.
    tail: u32,
}

/// The bytes of stack arguments of a libcall in the target's convention
/// (`CallConv::triple_default`): Windows' shadow space.
pub fn libcall_stack(target: &Target) -> u32 {
    if target.os == Os::Windows { 32 } else { 0 }
}

/// The stack arguments of a call to `sig` from `inst`, after checking the glue can make it.
/// Its StructArgument copies' `memcpy`s are in `sig`'s convention, so they fit in the same.
fn call_stack(
    func: &ir::Function,
    inst: ir::Inst,
    sig: ir::SigRef,
    try_call: bool,
) -> Result<u32, Error> {
    let sig = &func.dfg.signatures[sig];
    if try_call && !matches!(sig.call_conv, CallConv::SystemV | CallConv::Tail) {
        return Err(Error::at(
            inst,
            // alloc: the error's message.
            format!(
                "a `{}` to a {} function: Cranelift's x64 backend catches exceptions only from \
                 `system_v` (and `tail`) calls",
                func.dfg.insts[inst].opcode(),
                sig.call_conv
            ),
        ));
    }
    let stack = abi::stack_args(sig).map_err(blame(inst))?;
    abi::rets(sig, |_, _, _| Ok(())).map_err(blame(inst))?;
    Ok(stack)
}

/// Names `inst` in an error that names no instruction.
pub fn blame(inst: ir::Inst) -> impl FnOnce(Error) -> Error {
    move |mut e| {
        e.inst.get_or_insert(inst);
        e
    }
}

fn not_patchable(func: &ir::Function, inst: ir::Inst, func_ref: ir::FuncRef) -> Result<(), Error> {
    if func.dfg.ext_funcs[func_ref].patchable {
        return Err(Error::at(inst, "a patchable call, which no record of `Code` has a place for"));
    }
    Ok(())
}

/// The stack argument bytes of a `return_call` or `return_call_indirect`, after checking the
/// glue can make it: from a `tail` function to one (Cranelift's verifier and
/// `gen_call_args`), with no StructArgument, whose copy would go into the area it replaces.
fn return_call_stack(func: &ir::Function, inst: ir::Inst) -> Result<u32, Error> {
    let opcode = func.dfg.insts[inst].opcode();
    if func.signature.call_conv != CallConv::Tail {
        return Err(Error::at(
            inst,
            // alloc: the error's message.
            format!(
                "`{opcode}` in a {} function: Cranelift's x64 backend tail-calls only from \
                 `tail` functions",
                func.signature.call_conv
            ),
        ));
    }
    let sig = match func.dfg.insts[inst] {
        InstructionData::Call { func_ref, .. } => {
            not_patchable(func, inst, func_ref)?;
            func.dfg.ext_funcs[func_ref].signature
        }
        InstructionData::CallIndirect { sig_ref, .. } => sig_ref,
        _ => return Err(Error::at(inst, "a return call of another format")),
    };
    let sig = &func.dfg.signatures[sig];
    if sig.call_conv != CallConv::Tail {
        return Err(Error::at(
            inst,
            // alloc: the error's message.
            format!("`{opcode}` to a {} function: a tail call is to a `tail` function", sig.call_conv),
        ));
    }
    let stack = abi::args(sig, |_, _, place| match place {
        Place::Struct { .. } => Err(Error::new(
            "a StructArgument of a tail call: its copy would go into the argument area it replaces",
        )),
        _ => Ok(()),
    })
    .map_err(blame(inst))?;
    abi::rets(sig, |_, _, _| Ok(())).map_err(blame(inst))?;
    Ok(stack)
}

fn walk(func: &ir::Function, target: &Target) -> Result<Walk, Error> {
    let mut walk = Walk { outgoing: 0, calls: false, tail: 0 };
    for block in func.layout.blocks() {
        for inst in func.layout.block_insts(block) {
            let data = &func.dfg.insts[inst];
            let opcode = data.opcode();
            if matches!(opcode, Opcode::ReturnCall | Opcode::ReturnCallIndirect) {
                // Cranelift's `FunctionCalls::TailOnly`: the probe covers the setup area.
                walk.tail = walk.tail.max(return_call_stack(func, inst)?);
                walk.calls = true;
                continue;
            }
            let stack = match *data {
                InstructionData::Call { func_ref, .. } => {
                    not_patchable(func, inst, func_ref)?;
                    let sig = func.dfg.ext_funcs[func_ref].signature;
                    Some(call_stack(func, inst, sig, false)?)
                }
                InstructionData::CallIndirect { sig_ref, .. } => {
                    Some(call_stack(func, inst, sig_ref, false)?)
                }
                InstructionData::TryCall { func_ref, exception, .. } => {
                    not_patchable(func, inst, func_ref)?;
                    let sig = func.dfg.exception_tables[exception].signature();
                    Some(call_stack(func, inst, sig, true)?)
                }
                InstructionData::TryCallIndirect { exception, .. } => {
                    let sig = func.dfg.exception_tables[exception].signature();
                    Some(call_stack(func, inst, sig, true)?)
                }
                InstructionData::UnaryGlobalValue { global_value, .. } => {
                    if tls_chain(func, inst, global_value)? && target.tls != TlsModel::Coff {
                        walk.calls = true;
                    }
                    None
                }
                _ => None,
            };
            if let Some(stack) = stack {
                walk.outgoing = walk.outgoing.max(stack);
                walk.calls = true;
            }
            match opcode {
                Opcode::StackSwitch => {
                    return Err(Error::at(
                        inst,
                        "`stack_switch`, which Cranelift's x64 backend lowers only for the \
                         Wasmtime stack-switching ABI",
                    ));
                }
                Opcode::Ceil | Opcode::Floor | Opcode::Nearest | Opcode::Trunc | Opcode::Fma => {
                    let ty = func.dfg.ctrl_typevar(inst);
                    if !matches!(
                        ty,
                        ir::types::F32 | ir::types::F64 | ir::types::F32X4 | ir::types::F64X2
                    ) {
                        return Err(Error::at(
                            inst,
                            // alloc: the error's message.
                            format!(
                                "`{opcode}` of {ty}, which Cranelift's x64 backend does not lower"
                            ),
                        ));
                    }
                    walk.outgoing = walk.outgoing.max(libcall_stack(target));
                    walk.calls = true;
                }
                _ => {}
            }
        }
    }
    Ok(walk)
}

/// How deep a chain of global values the glue follows.
pub const GV_DEPTH: u32 = 16;

/// Whether the chain of global values from `gv` ends at a thread-local symbol, after checking
/// every link of it is one the glue computes.
pub fn tls_chain(func: &ir::Function, inst: ir::Inst, gv: ir::GlobalValue) -> Result<bool, Error> {
    let mut gv = gv;
    for _ in 0..GV_DEPTH {
        match func.global_values[gv] {
            GlobalValueData::Symbol { tls, .. } => return Ok(tls),
            GlobalValueData::IAddImm { base, global_type, .. } => {
                pointer(inst, global_type)?;
                gv = base;
            }
            GlobalValueData::Load { base, global_type, flags, .. } => {
                pointer(inst, global_type)?;
                let endianness = func.dfg.mem_flags[flags].endianness(ir::Endianness::Little);
                if endianness != ir::Endianness::Little {
                    return Err(Error::at(inst, "a big-endian global value load on x86-64"));
                }
                gv = base;
            }
            GlobalValueData::VMContext => {
                return Err(Error::at(
                    inst,
                    "a `vmctx` global value: the function has no VM context parameter",
                ));
            }
            GlobalValueData::DynScaleTargetConst { .. } => {
                return Err(Error::at(inst, "a dynamic vector scale global value"));
            }
        }
    }
    Err(Error::at(
        inst,
        // alloc: the error's message.
        format!("a chain of more than {GV_DEPTH} global values"),
    ))
}

fn pointer(inst: ir::Inst, ty: ir::Type) -> Result<(), Error> {
    if ty != ir::types::I64 {
        return Err(Error::at(
            inst,
            // alloc: the error's message.
            format!("a global value of {ty}, where x86-64's pointers are i64"),
        ));
    }
    Ok(())
}

/// Lays out `func`'s frame.
pub fn layout(func: &ir::Function, target: &Target, area: Area) -> Result<Frame, Error> {
    let sig = &func.signature;
    abi::check(sig.call_conv)?;
    abi::rets(sig, |_, _, _| Ok(()))?;
    if func.stack_limit.is_some() {
        return Err(Error::new(
            "a stack limit: the IR builder never sets one, and the stencil backend checks none",
        ));
    }
    if !func.dynamic_stack_slots.is_empty() {
        return Err(Error::new("dynamic stack slots, which Cranelift's x64 backend lowers none of"));
    }
    if area.align > 1 << 30 {
        return Err(Error::new(
            // alloc: the error's message.
            format!("a stack slot aligned to {} bytes", area.align),
        ));
    }
    let walk = walk(func, target)?;
    let win = sig.call_conv == CallConv::WindowsFastcall;
    let (pop, grow) = match sig.call_conv {
        CallConv::Tail => {
            let own = abi::stack_args(sig)?;
            let pop = own.max(walk.tail);
            if pop > u32::from(u16::MAX) {
                return Err(Error::new(
                    // alloc: the error's message.
                    format!("{pop} bytes of `tail` stack arguments, more than `ret imm16` pops"),
                ));
            }
            (pop, pop - own)
        }
        _ => (0, 0),
    };
    let clobber = if win { WIN_BYTES } else { SYSV_BYTES };
    let sret = sig.uses_special_param(ArgumentPurpose::StructReturn);
    let realign = area.align - 16;
    let fixed = u64::from(area.size) + u64::from(realign) + if sret { 16 } else { 0 };
    let total = u64::from(clobber) + fixed + u64::from(walk.outgoing) + u64::from(grow);
    if total > u64::from(FRAME_LIMIT) {
        return Err(Error::new(
            // alloc: the error's message.
            format!("a frame of {total} bytes, beyond Cranelift's limit of 1 GiB"),
        ));
    }
    let fixed = fixed as u32;
    let probe = probe_of(total as u32, walk.calls, target);
    let mut frame = Frame {
        win,
        win_unwind: target.os == Os::Windows,
        clobber,
        fixed,
        outgoing: walk.outgoing,
        size: area.size,
        align: area.align,
        sret: sret.then_some(area.size),
        probe,
        prologue: 0,
        pop,
        grow,
    };
    frame.prologue = prologue_bound(func, &frame)?;
    Ok(frame)
}

/// The probe of a frame of `total` bytes below `rbp`: `Callee::gen_prologue` probes what the
/// frame takes, and the setup area (return address and `rbp`) too when the function calls.
pub fn probe_of(total: u32, calls: bool, target: &Target) -> Probe {
    if !target.probe_stack {
        return Probe::None;
    }
    let guard = 1u32 << target.probe_log2;
    let size = total + if calls { 16 } else { 0 };
    match size / guard {
        0 => Probe::None,
        count if count <= PROBE_MAX_UNROLL => Probe::Unroll { count, guard },
        _ => Probe::Loop { size: size.next_multiple_of(guard), guard },
    }
}

/// The most bytes of the probe.
fn probe_bound(probe: Probe) -> u32 {
    match probe {
        Probe::None => 0,
        // `sub rsp, imm32` and `mov dword [rsp], 0` each step, then `add rsp, imm32`.
        Probe::Unroll { count, .. } => count * (7 + 7) + 7,
        // `mov`, `sub`, the loop's `sub`, `mov`, `cmp`, `jnz`, then `add`.
        Probe::Loop { .. } => 3 + 7 + 7 + 7 + 3 + 6 + 7,
    }
}

/// The most bytes of the saves, or of the restores, of a function of Windows' convention or
/// System V's: REX, opcode, ModRM, SIB and disp32 each general one, a prefix and a second
/// opcode byte more each XMM one.
fn saves_bound(win: bool) -> u32 {
    if win { 7 * 8 + 10 * 10 } else { 5 * 8 }
}

/// The most bytes of the epilogue, from the restores to the `ret` (`ret imm16` in `tail`).
pub fn epilogue_bound(win: bool) -> u32 {
    saves_bound(win) + 7 + 3 + 1 + 3
}

/// The most bytes of the growth of the incoming argument area: `sub rsp, imm32`, `mov rbp,
/// rsp`, and two 8-byte moves from `[rsp + disp32]` to `[rsp + disp8]` through `r11`.
const GROW: u32 = 7 + 3 + 2 * (8 + 5);

fn prologue_bound(func: &ir::Function, frame: &Frame) -> Result<u32, Error> {
    let mut bytes = 1 + 3 + probe_bound(frame.probe) + 7 + saves_bound(frame.win);
    if frame.grow > 0 {
        bytes += GROW;
    }
    // `lea r12`, and its `and`; the calls counter.
    bytes += 8 + 7 + 8;
    if frame.sret.is_some() {
        bytes += 7 + 8;
    }
    abi::args(&func.signature, |_, _, place| {
        bytes += match place {
            Place::Gpr(_) => 8,
            Place::Xmm(_) => 10,
            // A load from `[rbp + disp32]` and a store, per 8 bytes.
            Place::Stack(_) | Place::Pair(..) => 2 * (7 + 8),
            Place::Struct { .. } => 7 + 8,
            Place::Implicit { .. } => 7 + 2 * (7 + 8),
        };
        Ok(())
    })?;
    Ok(bytes)
}

/// The slot at `off` above `r12`.
pub fn slot(off: u32) -> Mem {
    at(R12, off as i32)
}

/// Where Cranelift's caller left the stack arguments: above the return address and the saved
/// `rbp`, which a grown frame moved `grow` bytes down.
fn incoming(grow: u32, off: u32) -> Mem {
    at(RBP, (16 + grow + off) as i32)
}

/// Writes the prologue up to the entry block's parameters.
pub fn enter(buf: &mut Buf<'_>, rows: &mut impl FnMut(u32, Rule), frame: &Frame) -> Result<(), Error> {
    asm::push(buf, RBP);
    rows(buf.len(), Rule::CfaOffset(16));
    rows(buf.len(), Rule::Offset(DWARF_RBP, -16));
    asm::mov(buf, RBP, RSP);
    rows(buf.len(), Rule::CfaRegister(DWARF_RBP));
    probe(buf, frame.probe)?;
    if frame.grow > 0 {
        // `gen_clobber_save`: room for a `return_call`'s stack arguments below the caller's,
        // the saved `rbp` and return address moved down to its bottom. The CFA stays `rbp + 16`,
        // where the return address now is, as Cranelift's rows have it.
        let grow = frame.grow as i32;
        asm::alu_imm32(buf, SUB, RSP, grow);
        asm::mov(buf, RBP, RSP);
        for word in [0, 8] {
            asm::load(buf, R11, at(RSP, grow + word));
            asm::store(buf, at(RSP, word), R11);
        }
    }
    asm::alu_imm(buf, SUB, RSP, frame.total() as i32);
    let base = frame.fixed + frame.outgoing;
    let to_cfa = 16 + frame.clobber as i32;
    for &(save, off) in frame.saves() {
        let m = at(RSP, (base + off) as i32);
        match save {
            Save::Gpr(reg) => asm::store(buf, m, reg),
            Save::Xmm(reg) => asm::xstore(buf, asm::Sse::Dqu, m, reg),
        }
        rows(buf.len(), Rule::Offset(dwarf(save), off as i32 - to_cfa));
    }
    if frame.align > 16 {
        asm::lea(buf, R12, at(RSP, (frame.outgoing + frame.align - 16) as i32));
        asm::alu_imm(buf, AND, R12, (frame.align as i32).wrapping_neg());
    } else {
        asm::lea(buf, R12, at(RSP, frame.outgoing as i32));
    }
    Ok(())
}

pub fn probe(buf: &mut Buf<'_>, probe: Probe) -> Result<(), Error> {
    match probe {
        Probe::None => {}
        Probe::Unroll { count, guard } => {
            for _ in 0..count {
                asm::alu_imm(buf, SUB, RSP, guard as i32);
                asm::store_imm32(buf, at(RSP, 0), 0);
            }
            asm::alu_imm(buf, ADD, RSP, (guard * count) as i32);
        }
        Probe::Loop { size, guard } => {
            asm::mov(buf, R11, RSP);
            asm::alu_imm(buf, SUB, R11, size as i32);
            let top = buf.len();
            asm::alu_imm(buf, SUB, RSP, guard as i32);
            asm::store_imm32(buf, at(RSP, 0), 0);
            asm::cmp(buf, R11, RSP);
            let field = asm::jcc(buf, asm::CC_NZ);
            buf.patch(field, Form::Rel32, i64::from(top) - i64::from(field + 4))
                // alloc: the error's message.
                .map_err(|e| Error::new(e.to_string()))?;
            asm::alu_imm(buf, ADD, RSP, size as i32);
        }
    }
    Ok(())
}

/// Stores the entry block's parameter `value`, passed at `place`, to its slot; `grow` is the
/// frame's.
pub fn param(
    buf: &mut Buf<'_>,
    grow: u32,
    place: Place,
    ty: ir::Type,
    dst: crate::arch::Loc,
) -> Result<(), Error> {
    let crate::arch::Loc::Slot(off) = dst else {
        return match dst {
            crate::arch::Loc::Dead => Ok(()),
            _ => Err(Error::new("an entry block parameter folded to a constant")),
        };
    };
    let copy = |buf: &mut Buf<'_>, from: Mem, to: Mem| {
        asm::load(buf, R10, from);
        asm::store(buf, to, R10);
    };
    match place {
        Place::Gpr(reg) => asm::store(buf, slot(off), reg),
        Place::Xmm(reg) => asm::xstore(buf, asm::Sse::of(ty.bytes()), slot(off), reg),
        Place::Stack(at_) => {
            copy(buf, incoming(grow, at_), slot(off));
            if ty.bytes() > 8 {
                copy(buf, incoming(grow, at_ + 8), slot(off + 8));
            }
        }
        Place::Pair(lo, hi) => {
            for (k, part) in [lo, hi].into_iter().enumerate() {
                let to = slot(off + 8 * k as u32);
                match part {
                    Part::Gpr(reg) => asm::store(buf, to, reg),
                    Part::Stack(at_) => copy(buf, incoming(grow, at_), to),
                }
            }
        }
        Place::Struct { at: at_, .. } => {
            asm::lea(buf, R10, incoming(grow, at_));
            asm::store(buf, slot(off), R10);
        }
        Place::Implicit { pointer, .. } => {
            let from = match pointer {
                Part::Gpr(reg) => reg,
                Part::Stack(at_) => {
                    asm::load(buf, R10, incoming(grow, at_));
                    R10
                }
            };
            copy_via(buf, R11, at(from, 0), slot(off));
            copy_via(buf, R11, at(from, 8), slot(off + 8));
        }
    }
    Ok(())
}

fn copy_via(buf: &mut Buf<'_>, tmp: u8, from: Mem, to: Mem) {
    asm::load(buf, tmp, from);
    asm::store(buf, to, tmp);
}

/// Where the function's StructReturn pointer arrives, to keep in its slot.
pub fn sret_place(func: &ir::Function) -> Result<Option<Place>, Error> {
    let mut found = None;
    abi::args(&func.signature, |_, param, place| {
        if param.purpose == ArgumentPurpose::StructReturn {
            found = Some(place);
        }
        Ok(())
    })?;
    Ok(found)
}

/// Writes the epilogue after the return values are in their registers: the restores, the
/// frame popped, `ret`; with the rows that keep the CFA right through it and after it.
pub fn leave(buf: &mut Buf<'_>, rows: &mut impl FnMut(u32, Rule), frame: &Frame) {
    unframe(buf, rows, frame);
    match frame.pop {
        0 => asm::ret(buf),
        pop => asm::ret_imm(buf, pop as u16),
    }
    rows(buf.len(), Rule::RestoreState);
}

/// The restores and the frame popped, `rsp` left at the return address: the epilogue but its
/// `ret`, and what a `return_call` does before it jumps. The state it remembers is restored
/// after the `ret` or the jump.
pub fn unframe(buf: &mut Buf<'_>, rows: &mut impl FnMut(u32, Rule), frame: &Frame) {
    let base = frame.fixed + frame.outgoing;
    for &(save, off) in frame.saves() {
        let m = at(RSP, (base + off) as i32);
        match save {
            Save::Gpr(reg) => asm::load(buf, reg, m),
            Save::Xmm(reg) => asm::xload(buf, asm::Sse::Dqu, reg, m),
        }
    }
    asm::alu_imm(buf, ADD, RSP, frame.total() as i32);
    asm::mov(buf, RSP, RBP);
    asm::pop(buf, RBP);
    rows(buf.len(), Rule::RememberState);
    rows(buf.len(), Rule::Cfa(DWARF_RSP, 8));
}

/// The unwind codes of a Windows `UNWIND_INFO`, as Cranelift's `winx64::UnwindCode` has them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WinCode {
    Push { at: u8 },
    SetFp { at: u8 },
    Save { at: u8, reg: u8, xmm: bool, off: u32 },
}

fn win_offset(at: u32) -> Result<u8, Error> {
    u8::try_from(at).map_err(|_| {
        // alloc: the error's message.
        Error::new(format!(
            "a prologue of more than 255 bytes ({at}), beyond what Windows' UNWIND_INFO describes"
        ))
    })
}

/// Windows' hardware numbers by DWARF number, for the registers a prologue saves.
fn win_reg(dwarf: u16) -> Option<(u8, bool)> {
    match dwarf {
        0..=15 => DWARF_GPR.iter().position(|&it| it == dwarf).map(|reg| (reg as u8, false)),
        17..=32 => Some(((dwarf - DWARF_XMM0) as u8, true)),
        _ => None,
    }
}

/// Calls `f` with each Windows unwind code the prologue's rows say, in order: the push of
/// `rbp` (its `CfaOffset`), the frame pointer (`CfaRegister`), each save (`Offset`). The rows
/// past the prologue (the epilogues') say nothing Windows records.
pub fn win_codes(
    rows: &[(u32, Rule)],
    clobber: u32,
    mut f: impl FnMut(WinCode),
) -> Result<(), Error> {
    for &(offset, rule) in rows {
        match rule {
            Rule::CfaOffset(16) => f(WinCode::Push { at: win_offset(offset)? }),
            Rule::CfaRegister(DWARF_RBP) => f(WinCode::SetFp { at: win_offset(offset)? }),
            Rule::Offset(DWARF_RBP, -16) => {}
            Rule::Offset(reg, cfa) => {
                let (reg, xmm) = win_reg(reg).ok_or_else(|| {
                    // alloc: the error's message.
                    Error::new(format!("a save of DWARF register {reg} on Windows"))
                })?;
                let off = u32::try_from(cfa + 16 + clobber as i32)
                    .map_err(|_| Error::new("a save outside the save area"))?;
                f(WinCode::Save { at: win_offset(offset)?, reg, xmm, off });
            }
            Rule::RememberState | Rule::RestoreState | Rule::Cfa(DWARF_RSP, 8) => {}
            other => {
                return Err(Error::new(
                    // alloc: the error's message.
                    format!("an unwind rule {other:?} Windows' UNWIND_INFO has no code for"),
                ));
            }
        }
    }
    Ok(())
}

/// The `UNWIND_INFO` Cranelift's `winx64::UnwindInfo::emit` writes for the prologue's rows
/// (`create_unwind_info_from_insts`): what `with_saves` then corrects.
pub fn win_raw(rows: &[(u32, Rule)], clobber: u32, out: &mut Vec<u8, impl Allocator>) -> Result<(), Error> {
    let mut nodes = 0usize;
    let mut last = 0u8;
    win_codes(rows, clobber, |code| {
        nodes += match code {
            WinCode::Save { off, .. } if off / 16 > u32::from(u16::MAX) => 3,
            WinCode::Save { .. } => 2,
            _ => 1,
        };
        last = match code {
            WinCode::Push { at } | WinCode::SetFp { at } | WinCode::Save { at, .. } => at,
        };
    })?;
    if nodes > 256 {
        return Err(Error::new("more than 256 Windows unwind codes"));
    }
    let frame_offset = u8::try_from(clobber).map_err(|_| {
        // alloc: the error's message.
        Error::new(format!("{clobber} bytes of saves, beyond what Windows' UNWIND_INFO describes"))
    })?;
    let size = 4 + nodes * 2 + if nodes & 1 == 1 { 2 } else { 0 };
    out.clear();
    out.resize(size, 0);
    out[0] = 1;
    out[1] = last;
    out[2] = nodes as u8;
    out[3] = (frame_offset << 4) | 5;
    // The codes go last first: fill from the end of the nodes.
    let mut end = 4 + nodes * 2;
    win_codes(rows, clobber, |code| {
        let mut node = [0u8; 6];
        let len = match code {
            WinCode::Push { at } => {
                node[..2].copy_from_slice(&[at, (5 << 4) | UWOP_PUSH_NONVOL]);
                2
            }
            WinCode::SetFp { at } => {
                node[..2].copy_from_slice(&[at, UWOP_SET_FPREG]);
                2
            }
            WinCode::Save { at, reg, xmm, off } => {
                let (small, far) = if xmm {
                    (UWOP_SAVE_XMM128, UWOP_SAVE_XMM128_FAR)
                } else {
                    (UWOP_SAVE_NONVOL, UWOP_SAVE_NONVOL_FAR)
                };
                let scaled = off / 16;
                if scaled <= u32::from(u16::MAX) {
                    node[..2].copy_from_slice(&[at, (reg << 4) | small]);
                    node[2..4].copy_from_slice(&(scaled as u16).to_le_bytes());
                    4
                } else {
                    node[..2].copy_from_slice(&[at, (reg << 4) | far]);
                    node[2..4].copy_from_slice(&(off as u16).to_le_bytes());
                    node[4..6].copy_from_slice(&((off >> 16) as u16).to_le_bytes());
                    6
                }
            }
        };
        end -= len;
        out[end..end + len].copy_from_slice(&node[..len]);
    })?;
    Ok(())
}

const UWOP_PUSH_NONVOL: u8 = 0;
const UWOP_ALLOC_LARGE: u8 = 1;
const UWOP_ALLOC_SMALL: u8 = 2;
const UWOP_SET_FPREG: u8 = 3;
const UWOP_SAVE_NONVOL: u8 = 4;
const UWOP_SAVE_NONVOL_FAR: u8 = 5;
const UWOP_SAVE_XMM128: u8 = 8;
const UWOP_SAVE_XMM128_FAR: u8 = 9;

/// `hir-ty-codegen/unwind/seh.rs`'s `with_saves`, which the dev path applies to Cranelift's
/// `UNWIND_INFO`: Cranelift's loses the distance from the frame pointer down to the saves
/// (it shifts it out of the byte) and halves where each 8-byte save is, so this writes the
/// distance, each save's place, and an allocation of the distance at the frame pointer's code
/// for the unwinder to find the frame pointer's own slot from.
pub fn with_saves(info: &[u8], out: &mut Vec<u8, impl Allocator>) -> Result<(), Error> {
    let nodes = &info[4..4 + usize::from(info[2]) * 2];
    let code = |at: usize| -> Result<(u8, u8, u8, u16, usize), Error> {
        let (offset, op, register) = (nodes[at], nodes[at + 1] & 0xf, nodes[at + 1] >> 4);
        let following = match op {
            UWOP_PUSH_NONVOL | UWOP_SET_FPREG => 0,
            UWOP_SAVE_NONVOL | UWOP_SAVE_XMM128 => 1,
            // alloc: the error's message.
            _ => return Err(Error::new(format!("a Windows unwind code {op} the glue never writes"))),
        };
        let word = u16::from_le_bytes([nodes[at + 2 * following], nodes[at + 2 * following + 1]]);
        Ok((offset, op, register, word, 2 + 2 * following))
    };
    let (mut words, mut vectors_end) = (0usize, None::<usize>);
    let mut at = 0;
    while at < nodes.len() {
        let (_, op, _, word, len) = code(at)?;
        if op == UWOP_SAVE_NONVOL {
            words += 1;
        }
        if op == UWOP_SAVE_XMM128 {
            let end = usize::from(word) + 1;
            vectors_end = Some(vectors_end.map_or(end, |it| it.max(end)));
        }
        at += len;
    }
    let saved = vectors_end.map_or(words.next_multiple_of(2) * 8, |end| end * 16);
    if saved == 0 {
        out.extend_from_slice(info);
        return Ok(());
    }
    if saved > 240 {
        // alloc: the error's message.
        return Err(Error::new(format!("{saved} bytes of saved registers")));
    }
    out.reserve(info.len() + 4 + 4);
    out.extend([info[0], info[1], 0, ((saved / 16) as u8 * 16) | (info[3] & 0xf)]);
    let mut word = words;
    let mut at = 0;
    while at < nodes.len() {
        let (offset, op, register, following, len) = code(at)?;
        out.extend([offset, (register << 4) | op]);
        match op {
            UWOP_SAVE_NONVOL => {
                word -= 1;
                out.extend((word as u16).to_le_bytes());
            }
            UWOP_SAVE_XMM128 => out.extend(following.to_le_bytes()),
            UWOP_SET_FPREG if saved <= 128 => {
                out.extend([offset, (((saved - 8) / 8) as u8 * 16) | UWOP_ALLOC_SMALL]);
            }
            UWOP_SET_FPREG => {
                out.extend([offset, UWOP_ALLOC_LARGE]);
                out.extend((saved as u16 / 8).to_le_bytes());
            }
            _ => (),
        }
        at += len;
    }
    out[2] = ((out.len() - 4) / 2) as u8;
    out.resize(out.len().next_multiple_of(4), 0);
    Ok(())
}