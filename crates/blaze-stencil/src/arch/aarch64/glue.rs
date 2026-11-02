//! The glue's bodies on AArch64, each the sequence Cranelift 0.136.2's `isa/aarch64` lowers
//! the same instruction to (`lower.isle`, `abi.rs`, `inst/emit.rs`), with the values read from
//! and written to their slots above `x20`. Memory to memory moves go through `x17` (a 16-byte
//! value as two halves), far offsets and constants through `x16`.

use cranelift_codegen::{
    binemit::Reloc,
    ir::{self, GlobalValueData, InstructionData as Data, Opcode, types},
    isa::CallConv,
};

use super::{
    Ctx,
    abi::{self, Place, Reg},
    frame::{self, Frame},
    inst::{self, FP, SP, X0, X1, X8, X16, X17, X20},
};
use crate::{
    Error, Target, TlsModel,
    arch::Loc,
    buf::Buf,
    code::{Name, Rule},
    enc::Form,
};

/// The cells of a function's counters ([`crate::tier`]): its calls, then its back edges.
const CALLS: i64 = 0;
pub(super) const BACK_EDGES: i64 = 8;

/// The most bytes one argument or return value's move writes: an `i128` constant to the stack.
const MOVE_MAX: u32 = 2 * (16 + inst::LDST_MAX);
/// The most bytes one call result's store writes: an `i128`'s two.
const RESULT_MAX: u32 = 2 * inst::LDST_MAX;
/// The most bytes [`load_name`] writes: the GOT's `adrp` and `ldr`, and an offset's add.
const NAME_MAX: u32 = 8 + 16 + 4;
/// The most bytes a callee's address and the call write.
const CALLEE_MAX: u32 = NAME_MAX + 4;
/// The bytes [`count`] writes.
const COUNT_LEN: u32 = 20;
/// The most bytes [`relay`] writes.
const RELAY_MAX: u32 = 4 * inst::LDST_MAX;
/// The most bytes a thread-local's address takes: ELF's TLS descriptor call.
const TLS_MAX: u32 = 28;

/// Whether the glue writes an instruction of `opcode`. AArch64 has no libcall operations:
/// Cranelift lowers every arithmetic one inline on the baseline (`fma`, the roundings and
/// the rest are instructions), and the IR builder makes the rest calls itself.
pub(super) fn takes(opcode: Opcode) -> bool {
    matches!(
        opcode,
        Opcode::Return
            | Opcode::Call
            | Opcode::CallIndirect
            | Opcode::TryCall
            | Opcode::TryCallIndirect
            | Opcode::ReturnCall
            | Opcode::ReturnCallIndirect
            | Opcode::BrTable
            | Opcode::Trap
            | Opcode::Debugtrap
            | Opcode::FuncAddr
            | Opcode::SymbolValue
            | Opcode::TlsValue
            | Opcode::GetFramePointer
            | Opcode::GetStackPointer
            | Opcode::GetReturnAddress
            | Opcode::GetPinnedReg
            | Opcode::SetPinnedReg
            | Opcode::GetExceptionHandlerAddress
    )
}

/// The most bytes [`inst`] writes for `inst`.
pub(super) fn bound(func: &ir::Function, target: &Target, inst: ir::Inst) -> u32 {
    let _ = target;
    let args = func.dfg.inst_args(inst).len() as u32;
    match func.dfg.insts[inst].opcode() {
        Opcode::Return => args * MOVE_MAX + inst::LDST_MAX + frame::EXIT_MAX,
        Opcode::Call | Opcode::CallIndirect | Opcode::TryCall | Opcode::TryCallIndirect => {
            let rets = frame::call_signature(func, inst)
                .map_or(0, |sig| func.dfg.signatures[sig].returns.len() as u32);
            // A `tail` callee's popped arguments taken back.
            args * MOVE_MAX + CALLEE_MAX + inst::ADD_ANY_MAX + rets * RESULT_MAX
        }
        // The arguments, the callee, the epilogue but its `ret`, `sp` up, the jump.
        Opcode::ReturnCall | Opcode::ReturnCallIndirect => {
            args * MOVE_MAX + NAME_MAX + frame::UNFRAME_MAX + inst::ADD_ANY_MAX + 4
        }
        Opcode::BrTable => {
            let Data::BranchTable { table, .. } = func.dfg.insts[inst] else { return 0 };
            let entries = func.dfg.jump_tables[table].as_slice().len() as u32;
            // The index, the compare, the default's index, five instructions, the table.
            16 + 20 + 16 + 5 * 4 + 4 * (entries + 1)
        }
        Opcode::Trap | Opcode::Debugtrap => 4,
        Opcode::FuncAddr | Opcode::SymbolValue => NAME_MAX + inst::LDST_MAX,
        Opcode::TlsValue => TLS_MAX + inst::LDST_MAX,
        Opcode::GetFramePointer | Opcode::GetStackPointer | Opcode::GetReturnAddress => {
            4 + inst::LDST_MAX
        }
        _ => 0,
    }
}

/// The most bytes the prologue of `func` writes in `frame`.
pub(super) fn entry_bound(func: &ir::Function, frame: &Frame) -> u32 {
    let params = func.signature.params.len() as u32;
    frame::enter_bound(frame) + frame::BASE_MAX + inst::LDST_MAX + params * MOVE_MAX + COUNT_LEN
}

/// The most bytes of the prologue, a landing pad, a relay, a back-edge count and a trap stub.
pub(super) fn bound_fixed(frame: &Frame) -> u32 {
    frame.entry.max(2 * inst::LDST_MAX).max(RELAY_MAX).max(COUNT_LEN).max(4)
}

/// The prologue: the frame with its rows, `x20`, the StructReturn pointer's own slot, the
/// parameters from their registers and the caller's stack to their slots, and the count.
pub(super) fn prologue<'f, 'm>(cx: &mut impl Ctx<'f, 'm>) -> Result<(), Error> {
    let (func, frame) = (cx.func(), cx.frame());
    // Cranelift's `gen_prologue_frame_setup` says it before anything on Apple's ("the macOS
    // unwinder seems to require this"), with return addresses left unsigned.
    if func.signature.call_conv == CallConv::AppleAarch64 {
        cx.rule_at(0, Rule::RaSignState(false));
    }
    let mut rows = frame::Rows::default();
    frame::enter(cx.buf(), frame, &mut rows);
    for &(at, rule) in rows.as_slice() {
        cx.rule_at(at, rule);
    }
    frame::set_base(cx.buf(), frame);
    if let Some(at) = frame.sret {
        inst::ldst(cx.buf(), inst::STRX, X8, SP, at, X16);
    }
    let entry = func.layout.entry_block().ok_or_else(|| Error::new("a function with no blocks"))?;
    let params = func.dfg.block_params(entry);
    if params.len() != func.signature.params.len() {
        // alloc: the error's message, once, as the function fails to compile.
        return Err(Error::new(format!(
            "an entry block of {} parameters for a signature of {}",
            params.len(),
            func.signature.params.len()
        )));
    }
    abi::args(&func.signature, |i, _, place| {
        let loc = cx.loc(params[i]);
        param(cx.buf(), frame.grow, place, loc)
    })?;
    count(cx.buf(), CALLS);
    Ok(())
}

/// Stores the parameter at `place` to `loc`; a stack one is above the frame record, which a
/// grown frame moved `grow` bytes down.
fn param(buf: &mut Buf<'_>, grow: u32, place: Place, loc: Loc) -> Result<(), Error> {
    let off = match loc {
        Loc::Dead => return Ok(()),
        Loc::Slot(off) => off,
        Loc::Const(_) => return Err(Error::new("a parameter folded to a constant")),
    };
    match place {
        Place::Reg(Reg::X(n), ty) => {
            inst::ldst(buf, inst::str_int(ty.bytes() as u8), n, X20, off, X16)
        }
        Place::Reg(Reg::V(n), ty) => inst::ldst(buf, inst::str_v(ty.bytes()), n, X20, off, X16),
        Place::Pair(lo, hi) => {
            inst::ldst(buf, inst::STRX, lo, X20, off, X16);
            inst::ldst(buf, inst::STRX, hi, X20, off + 8, X16);
        }
        Place::Stack(at, ty) => copy(buf, (FP, 16 + grow + at), (X20, off), ty.bytes()),
    }
    Ok(())
}

/// Copies `bytes` (1, 2, 4, 8 or 16) from `[from + at]` to `[to + off]` through `x17`: a
/// 16-byte value as two 8-byte halves.
fn copy(buf: &mut Buf<'_>, (from, at): (u8, u32), (to, off): (u8, u32), bytes: u32) {
    if bytes == 16 {
        for half in [0, 8] {
            inst::ldst(buf, inst::LDRX, X17, from, at + half, X16);
            inst::ldst(buf, inst::STRX, X17, to, off + half, X16);
        }
    } else {
        inst::ldst(buf, inst::ldr_int(bytes as u8), X17, from, at, X16);
        inst::ldst(buf, inst::str_int(bytes as u8), X17, to, off, X16);
    }
}

/// Adds one to the function's counter cell at `addend` (`Name::Counter`). Cranelift's
/// relocations have no 64-bit load's low 12 bits, so the cell's address is made whole first.
pub(super) fn count(buf: &mut Buf<'_>, addend: i64) {
    let at = buf.len();
    buf.reloc(at, Reloc::Aarch64AdrPrelPgHi21, Name::Counter, addend);
    buf.put4(inst::adrp(X16));
    let at = buf.len();
    buf.reloc(at, Reloc::Aarch64AddAbsLo12Nc, Name::Counter, addend);
    buf.put4(inst::add_imm(X16, X16, 0, false, false));
    buf.put4(inst::at0(inst::LDRX, X17, X16));
    buf.put4(inst::add_imm(X17, X17, 1, false, false));
    buf.put4(inst::at0(inst::STRX, X17, X16));
}

/// Copies `bytes` (8 or 16) between two slots, either past `NEAR`, through `x16` and `x17`.
pub(super) fn relay(buf: &mut Buf<'_>, from: u32, to: u32, bytes: u8) {
    copy(buf, (X20, from), (X20, to), u32::from(bytes));
}

/// Writes `inst`, which [`takes`] says the glue writes.
pub(super) fn inst<'f, 'm>(cx: &mut impl Ctx<'f, 'm>, inst: ir::Inst) -> Result<(), Error> {
    let func = cx.func();
    match func.dfg.insts[inst].opcode() {
        Opcode::Return => ret(cx, inst),
        Opcode::Call | Opcode::CallIndirect | Opcode::TryCall | Opcode::TryCallIndirect => {
            call(cx, inst)
        }
        Opcode::ReturnCall | Opcode::ReturnCallIndirect => return_call(cx, inst),
        Opcode::BrTable => br_table(cx, inst),
        Opcode::Trap => {
            cx.buf().put4(inst::UDF);
            Ok(())
        }
        Opcode::Debugtrap => {
            cx.buf().put4(inst::BRK);
            Ok(())
        }
        Opcode::FuncAddr | Opcode::SymbolValue => address(cx, inst),
        Opcode::TlsValue => tls(cx, inst),
        Opcode::GetFramePointer => result(cx, inst, FP),
        Opcode::GetStackPointer => {
            cx.buf().put4(inst::mov_from_sp(X16));
            result(cx, inst, X16)
        }
        Opcode::GetReturnAddress => {
            // `preserve_frame_pointers`: the frame record's return address.
            inst::ldst(cx.buf(), inst::LDRX, X16, FP, 8, X17);
            result(cx, inst, X16)
        }
        Opcode::GetPinnedReg | Opcode::SetPinnedReg => Err(Error::at(
            inst,
            "the pinned register: Cranelift gives AArch64 one (`x21`) only with \
             `enable_pinned_reg`, which blaze's ISA leaves off and the stencils' accumulator is",
        )),
        Opcode::GetExceptionHandlerAddress => Err(Error::at(
            inst,
            "`get_exception_handler_address`, which the glue does not write: its handler's \
             address is the patcher's edge label, which no relocation names",
        )),
        opcode => {
            // alloc: the error's message, once, as the function fails to compile.
            Err(Error::at(inst, format!("`{opcode}`, which the AArch64 glue does not write")))
        }
    }
}

/// Stores the X register `reg` to the slot of `inst`'s result.
fn result<'f, 'm>(cx: &mut impl Ctx<'f, 'm>, inst: ir::Inst, reg: u8) -> Result<(), Error> {
    let value = cx.func().dfg.first_result(inst);
    let loc = cx.loc(value);
    store(cx.buf(), inst, Place::Reg(Reg::X(reg), types::I64), loc)
}

/// Stores what is at the register `place` to `loc`, as the value's own width.
fn store(buf: &mut Buf<'_>, inst: ir::Inst, place: Place, loc: Loc) -> Result<(), Error> {
    let off = match loc {
        Loc::Dead => return Ok(()),
        Loc::Slot(off) => off,
        Loc::Const(_) => return Err(Error::at(inst, "a result folded to a constant")),
    };
    match place {
        Place::Reg(Reg::X(n), ty) => {
            inst::ldst(buf, inst::str_int(ty.bytes() as u8), n, X20, off, X16)
        }
        Place::Reg(Reg::V(n), ty) => inst::ldst(buf, inst::str_v(ty.bytes()), n, X20, off, X16),
        Place::Pair(lo, hi) => {
            inst::ldst(buf, inst::STRX, lo, X20, off, X16);
            inst::ldst(buf, inst::STRX, hi, X20, off + 8, X16);
        }
        Place::Stack(..) => return Err(Error::at(inst, "a result on the stack")),
    }
    Ok(())
}

/// `value` of `ty` as it is in a register: its low bits, sign- or zero-extended as `ext` says
/// (zero-extended when it says nothing, as Cranelift's `iconst` lowering does).
fn extended(value: i64, ty: ir::Type, ext: Option<bool>) -> u64 {
    let bits = ty.bits();
    if bits >= 64 {
        return value as u64;
    }
    let low = value as u64 & ((1u64 << bits) - 1);
    if ext == Some(true) { ((low << (64 - bits)) as i64 >> (64 - bits)) as u64 } else { low }
}

/// Puts the value at `loc` where `place` says, extended as `ext` says: a register (an X
/// one, a V one, a pair) or, for a call's argument, the outgoing stack at `sp`.
fn load(
    buf: &mut Buf<'_>,
    inst: ir::Inst,
    place: Place,
    loc: Loc,
    ext: Option<bool>,
) -> Result<(), Error> {
    let v = match loc {
        Loc::Dead => return Err(Error::at(inst, "an argument or return value with no slot")),
        Loc::Slot(off) => {
            match place {
                Place::Reg(Reg::X(n), ty) => load_x(buf, n, off, ty, ext),
                Place::Reg(Reg::V(n), ty) => {
                    inst::ldst(buf, inst::ldr_v(ty.bytes()), n, X20, off, X16);
                }
                Place::Pair(lo, hi) => {
                    inst::ldst(buf, inst::LDRX, lo, X20, off, X16);
                    inst::ldst(buf, inst::LDRX, hi, X20, off + 8, X16);
                }
                Place::Stack(at, ty) => copy(buf, (X20, off), (SP, at), ty.bytes()),
            }
            return Ok(());
        }
        Loc::Const(v) => v,
    };
    match place {
        Place::Reg(Reg::X(n), ty) => inst::mov_imm(buf, n, extended(v, ty, ext)),
        Place::Reg(Reg::V(n), ty) if ty.bytes() == 4 => {
            inst::mov_imm(buf, X16, v as u64 & 0xffff_ffff);
            buf.put4(inst::fmov_s_w(n, X16));
        }
        Place::Reg(Reg::V(n), ty) if ty.bytes() == 8 => {
            inst::mov_imm(buf, X16, v as u64);
            buf.put4(inst::fmov_d_x(n, X16));
        }
        Place::Reg(Reg::V(_), ty) => {
            // alloc: the error's message, once, as the function fails to compile.
            return Err(Error::at(inst, format!("a {ty} constant in a vector register")));
        }
        Place::Pair(lo, hi) => {
            inst::mov_imm(buf, lo, v as u64);
            inst::mov_imm(buf, hi, (v >> 63) as u64);
        }
        Place::Stack(at, ty) if ty.bytes() == 16 => {
            for (half, bits) in [(0, v as u64), (8, (v >> 63) as u64)] {
                inst::mov_imm(buf, X17, bits);
                inst::ldst(buf, inst::STRX, X17, SP, at + half, X16);
            }
        }
        Place::Stack(at, ty) => {
            inst::mov_imm(buf, X17, extended(v, ty, None));
            inst::ldst(buf, inst::str_int(ty.bytes() as u8), X17, SP, at, X16);
        }
    }
    Ok(())
}

/// The value at `loc` into the X register `rd`, extended as `ext` says.
fn load_loc(
    buf: &mut Buf<'_>,
    inst: ir::Inst,
    rd: u8,
    loc: Loc,
    ty: ir::Type,
    ext: Option<bool>,
) -> Result<(), Error> {
    match loc {
        Loc::Slot(off) => load_x(buf, rd, off, ty, ext),
        Loc::Const(v) => inst::mov_imm(buf, rd, extended(v, ty, ext)),
        Loc::Dead => return Err(Error::at(inst, "an operand with no slot")),
    }
    Ok(())
}

/// The value of `ty` at slot `off` into the X register `rd`, extended as `ext` says.
fn load_x(buf: &mut Buf<'_>, rd: u8, off: u32, ty: ir::Type, ext: Option<bool>) {
    let tmp = if rd == X16 { X17 } else { X16 };
    inst::ldst(buf, inst::ldr_ext(ty.bytes(), ext == Some(true)), rd, X20, off, tmp);
}

/// `return`: the values to their registers (Apple's convention extending them as the
/// signature says), the StructReturn pointer back into `x8`, and the epilogue.
fn ret<'f, 'm>(cx: &mut impl Ctx<'f, 'm>, inst: ir::Inst) -> Result<(), Error> {
    let (func, frame) = (cx.func(), cx.frame());
    let sig = &func.signature;
    let args = func.dfg.inst_args(inst);
    if args.len() != sig.returns.len() {
        return Err(Error::at(inst, "a return of other values than the signature's"));
    }
    abi::rets(sig, |k, param, place| {
        let loc = cx.loc(args[k]);
        load(cx.buf(), inst, place, loc, abi::extension(sig.call_conv, param, place))
    })
    .map_err(|e| Error::at(inst, e.what))?;
    if let Some(at) = frame.sret {
        inst::ldst(cx.buf(), inst::LDRX, X8, SP, at, X16);
    }
    frame::exit(cx.buf(), frame);
    Ok(())
}

/// What a call calls.
enum Callee {
    /// A named function: `bl` when it is colocated, its address loaded otherwise.
    Named { name: Name, near: bool },
    /// The address in a value.
    Value(ir::Value),
}

/// `call`, `call_indirect`, `try_call`, `try_call_indirect`: the arguments to their
/// registers and the outgoing area, the callee, the call, its site and the results to their
/// slots. A `try_call`'s normal edge is written next by the patcher.
fn call<'f, 'm>(cx: &mut impl Ctx<'f, 'm>, inst: ir::Inst) -> Result<(), Error> {
    let (func, target) = (cx.func(), cx.target());
    let data = &func.dfg.insts[inst];
    let sig = frame::call_signature(func, inst)
        .ok_or_else(|| Error::at(inst, "a call with no signature"))?;
    let sig = &func.dfg.signatures[sig];
    let all = func.dfg.inst_args(inst);
    let (callee, args) = match *data {
        Data::Call { func_ref, .. } | Data::TryCall { func_ref, .. } => {
            let ext = &func.dfg.ext_funcs[func_ref];
            if ext.patchable {
                return Err(Error::at(
                    inst,
                    "a patchable call, whose patch metadata the glue does not write",
                ));
            }
            (Callee::Named { name: cx.name(&ext.name)?, near: ext.colocated }, all)
        }
        _ => {
            let (callee, args) = all
                .split_first()
                .ok_or_else(|| Error::at(inst, "an indirect call with no callee"))?;
            (Callee::Value(*callee), args)
        }
    };
    if matches!(data.opcode(), Opcode::TryCall | Opcode::TryCallIndirect)
        && !abi::catches(sig.call_conv)
    {
        return Err(Error::at(
            inst,
            // alloc: the error's message, once, as the function fails to compile.
            format!(
                "a `try_call` in the {} convention, which has no exception payload registers in \
                 Cranelift's AArch64 backend",
                sig.call_conv
            ),
        ));
    }
    if args.len() != sig.params.len() {
        return Err(Error::at(inst, "a call of other arguments than its signature's"));
    }
    let outgoing = abi::args(sig, |i, param, place| {
        let loc = cx.loc(args[i]);
        load(cx.buf(), inst, place, loc, abi::extension(sig.call_conv, param, place))
    })
    .map_err(|e| Error::at(inst, e.what))?;
    if outgoing > cx.frame().outgoing {
        return Err(Error::at(
            inst,
            // alloc: the error's message, once, as the function fails to compile.
            format!(
                "a call with {outgoing} bytes of stack arguments in a frame laid out for {}",
                cx.frame().outgoing
            ),
        ));
    }
    match callee {
        Callee::Named { name, near: true } => {
            let buf = cx.buf();
            let at = buf.len();
            buf.reloc(at, Reloc::Arm64Call, name, 0);
            buf.put4(inst::BL);
        }
        Callee::Named { name, near: false } => {
            load_name(cx.buf(), target, X16, name, false, 0);
            cx.buf().put4(inst::blr(X16));
        }
        Callee::Value(value) => {
            let loc = cx.loc(value);
            load_loc(cx.buf(), inst, X16, loc, types::I64, None)?;
            cx.buf().put4(inst::blr(X16));
        }
    }
    let at = cx.buf().len();
    cx.call_site(at, inst);
    if sig.call_conv == CallConv::Tail && outgoing > 0 {
        // A `tail` callee popped its stack arguments: the outgoing area back (`callee_pop_size`).
        inst::add_any(cx.buf(), SP, SP, u64::from(outgoing), true, X16);
    }
    abi::rets(sig, |k, _, place| {
        let loc = cx.returned(inst, k);
        store(cx.buf(), inst, place, loc)
    })
    .map_err(|e| Error::at(inst, e.what))
}

/// `return_call` and `return_call_indirect` from a `tail` function to one (the frame's layout
/// checked both), as Cranelift's `gen_call_args` (`is_tail_call`) and
/// `emit_return_call_common_sequence` write them: the stack arguments into the top of the
/// incoming argument area (`IncomingArg`, `pop` bytes above the frame record), the register
/// arguments, the callee to `x17`, the frame popped, `sp` up to the new arguments, then `b` or
/// `br x17`. Every slot is read before the restores give `x20` back; the incoming area holds
/// nothing the function still reads, its own stack arguments copied to slots by the prologue.
fn return_call<'f, 'm>(cx: &mut impl Ctx<'f, 'm>, inst: ir::Inst) -> Result<(), Error> {
    let (func, target, frame) = (cx.func(), cx.target(), cx.frame());
    let sig = frame::return_call_signature(func, inst)?
        .ok_or_else(|| Error::at(inst, "a return call with no signature"))?;
    let sig = &func.dfg.signatures[sig];
    let all = func.dfg.inst_args(inst);
    let (callee, args) = match func.dfg.insts[inst] {
        Data::Call { func_ref, .. } => {
            let ext = &func.dfg.ext_funcs[func_ref];
            (Callee::Named { name: cx.name(&ext.name)?, near: ext.colocated }, all)
        }
        _ => {
            let (callee, args) = all
                .split_first()
                .ok_or_else(|| Error::at(inst, "an indirect return call with no callee"))?;
            (Callee::Value(*callee), args)
        }
    };
    if args.len() != sig.params.len() {
        return Err(Error::at(inst, "a return call of other arguments than its signature's"));
    }
    let new = abi::stack_args(sig).map_err(|e| Error::at(inst, e.what))?;
    let base = 16 + frame.pop - new;
    abi::args(sig, |i, _, place| {
        let Place::Stack(at, ty) = place else { return Ok(()) };
        match cx.loc(args[i]) {
            Loc::Slot(off) => copy(cx.buf(), (X20, off), (FP, base + at), ty.bytes()),
            Loc::Const(v) if ty.bytes() == 16 => {
                for (half, bits) in [(0, v as u64), (8, (v >> 63) as u64)] {
                    inst::mov_imm(cx.buf(), X17, bits);
                    inst::ldst(cx.buf(), inst::STRX, X17, FP, base + at + half, X16);
                }
            }
            Loc::Const(v) => {
                inst::mov_imm(cx.buf(), X17, extended(v, ty, None));
                inst::ldst(cx.buf(), inst::str_int(ty.bytes() as u8), X17, FP, base + at, X16);
            }
            Loc::Dead => return Err(Error::at(inst, "an argument with no slot")),
        }
        Ok(())
    })
    .map_err(|e| Error::at(inst, e.what))?;
    abi::args(sig, |i, param, place| {
        if matches!(place, Place::Stack(..)) {
            return Ok(());
        }
        let loc = cx.loc(args[i]);
        load(cx.buf(), inst, place, loc, abi::extension(sig.call_conv, param, place))
    })
    .map_err(|e| Error::at(inst, e.what))?;
    let near = match callee {
        Callee::Named { name, near: true } => Some(name),
        Callee::Named { name, near: false } => {
            load_name(cx.buf(), target, X17, name, false, 0);
            None
        }
        Callee::Value(value) => {
            let loc = cx.loc(value);
            load_loc(cx.buf(), inst, X17, loc, types::I64, None)?;
            None
        }
    };
    let buf = cx.buf();
    frame::unframe(buf, frame);
    if frame.pop > new {
        inst::add_any(buf, SP, SP, u64::from(frame.pop - new), false, X16);
    }
    match near {
        Some(name) => {
            let at = buf.len();
            buf.reloc(at, Reloc::Arm64Call, name, 0);
            buf.put4(inst::b(0));
        }
        None => buf.put4(inst::br(X17)),
    }
    Ok(())
}

/// A `try_call`'s landing pad: the exception payload (`x0`, `x1`) to its slots.
pub(super) fn landing<'f, 'm>(cx: &mut impl Ctx<'f, 'm>, inst: ir::Inst) -> Result<(), Error> {
    for (k, reg) in [X0, X1].into_iter().enumerate() {
        let loc = cx.payload(inst, k);
        store(cx.buf(), inst, Place::Reg(Reg::X(reg), types::I64), loc)?;
    }
    Ok(())
}

/// `br_table` as Cranelift's `JTSequence`: the index (zero-extended) checked against the
/// table's length with the default taken at or past it, clamped to zero by `csel` (Spectre),
/// then a 32-bit offset of each target from the table's start, which follows the `br`.
fn br_table<'f, 'm>(cx: &mut impl Ctx<'f, 'm>, inst: ir::Inst) -> Result<(), Error> {
    let func = cx.func();
    let Data::BranchTable { arg, table, .. } = func.dfg.insts[inst] else {
        return Err(Error::at(inst, "a `br_table` with no table"));
    };
    let entries = func.dfg.jump_tables[table].as_slice().len();
    let loc = cx.loc(arg);
    let default = cx.edge(inst, 0);
    let buf = cx.buf();
    load_loc(buf, inst, X16, loc, func.dfg.value_type(arg), Some(false))?;
    match inst::imm12(entries as u64) {
        Some((imm, shift)) => buf.put4(inst::cmp_w_imm(X16, imm, shift)),
        None => {
            inst::mov_imm(buf, X17, entries as u64);
            buf.put4(inst::cmp_w(X16, X17));
        }
    }
    // An index out of bounds is clamped to the default's entry, the table's last, as
    // Cranelift 0.137 does: no branch to the default, whose range the table could outgrow.
    inst::mov_imm(buf, X17, entries as u64);
    buf.put4(inst::csel(X17, X16, X17, inst::LO));
    buf.put4(inst::adr(X16, 16));
    buf.put4(inst::ldrsw_uxtw2(X17, X16, X17));
    buf.put4(inst::add(X16, X16, X17));
    buf.put4(inst::br(X16));
    let start = cx.buf().len();
    for i in 0..=entries {
        let label = if i < entries { cx.edge(inst, 1 + i) } else { default };
        let buf = cx.buf();
        let at = buf.len();
        buf.fixup(at, Form::Rel32, label, i64::from(at - start));
        buf.put4(0);
    }
    Ok(())
}

/// `name + offset` into `rd`, as Cranelift's `load_ext_name`: through the GOT in
/// position-independent code (the offset added after), `adrp` and `add` when it is near, and
/// otherwise an absolute literal jumped over.
fn load_name(buf: &mut Buf<'_>, target: &Target, rd: u8, name: Name, near: bool, offset: i64) {
    let tmp = if rd == X16 { X17 } else { X16 };
    if target.pic {
        let at = buf.len();
        buf.reloc(at, Reloc::Aarch64AdrGotPage21, name, 0);
        buf.put4(inst::adrp(rd));
        let at = buf.len();
        buf.reloc(at, Reloc::Aarch64Ld64GotLo12Nc, name, 0);
        buf.put4(inst::at0(inst::LDRX, rd, rd));
        if offset != 0 {
            inst::mov_imm(buf, tmp, offset as u64);
            buf.put4(inst::add(rd, rd, tmp));
        }
    } else if near {
        let at = buf.len();
        buf.reloc(at, Reloc::Aarch64AdrPrelPgHi21, name, offset);
        buf.put4(inst::adrp(rd));
        let at = buf.len();
        buf.reloc(at, Reloc::Aarch64AddAbsLo12Nc, name, offset);
        buf.put4(inst::add_imm(rd, rd, 0, false, false));
    } else {
        buf.put4(inst::ldr_lit(rd, 8));
        buf.put4(inst::b(3));
        let at = buf.len();
        buf.reloc(at, Reloc::Abs8, name, offset);
        buf.put8(0);
    }
}

/// `func_addr`, `symbol_value`: the address into `x16`, then to the result's slot.
fn address<'f, 'm>(cx: &mut impl Ctx<'f, 'm>, inst: ir::Inst) -> Result<(), Error> {
    let (func, target) = (cx.func(), cx.target());
    let (name, near, offset) = match func.dfg.insts[inst] {
        Data::FuncAddr { func_ref, .. } => {
            let ext = &func.dfg.ext_funcs[func_ref];
            (&ext.name, ext.colocated, 0)
        }
        Data::UnaryGlobalValue { global_value, .. } => match &func.global_values[global_value] {
            GlobalValueData::Symbol { name, offset, colocated, tls: false } => {
                (name, *colocated, offset.bits())
            }
            GlobalValueData::Symbol { tls: true, .. } => {
                return Err(Error::at(
                    inst,
                    "`symbol_value` of a thread-local: `tls_value` finds it",
                ));
            }
            other => {
                return Err(Error::at(
                    inst,
                    // alloc: the error's message, once, as the function fails to compile.
                    format!(
                        "`symbol_value` of {other}, which Cranelift's AArch64 backend lowers only for a symbol"
                    ),
                ));
            }
        },
        _ => return Err(Error::at(inst, "an address of no function or global")),
    };
    let name = cx.name(name)?;
    load_name(cx.buf(), target, X16, name, near, offset);
    result(cx, inst, X16)
}

/// `tls_value`: the [`thread_local`]'s address to the result's slot.
fn tls<'f, 'm>(cx: &mut impl Ctx<'f, 'm>, inst: ir::Inst) -> Result<(), Error> {
    let func = cx.func();
    let Data::UnaryGlobalValue { global_value, .. } = func.dfg.insts[inst] else {
        return Err(Error::at(inst, "a `tls_value` of no global"));
    };
    let GlobalValueData::Symbol { name, offset, tls: true, .. } = &func.global_values[global_value]
    else {
        return Err(Error::at(inst, "`tls_value` of what is not a thread-local symbol"));
    };
    thread_local(cx, inst, name, offset.bits())?;
    result(cx, inst, X0)
}

/// A thread-local's address into `x0`, as Cranelift's `ElfTlsGetAddr` (a TLS descriptor call,
/// the thread pointer added after) and `MachOTlsGetAddr` (the variable's `tlv` thunk called
/// with its descriptor in `x0`): each a call, recorded as one.
fn thread_local<'f, 'm>(
    cx: &mut impl Ctx<'f, 'm>,
    inst: ir::Inst,
    name: &ir::ExternalName,
    offset: i64,
) -> Result<(), Error> {
    let target = cx.target();
    if offset != 0 {
        return Err(Error::at(
            inst,
            "`tls_value` of a thread-local at an offset, which Cranelift's AArch64 backend drops",
        ));
    }
    let name = cx.name(name)?;
    let buf = cx.buf();
    match target.tls {
        TlsModel::ElfGd => {
            let at = buf.len();
            buf.reloc(at, Reloc::Aarch64TlsDescAdrPage21, name, 0);
            buf.put4(inst::adrp(X0));
            let at = buf.len();
            buf.reloc(at, Reloc::Aarch64TlsDescLd64Lo12, name, 0);
            buf.put4(inst::at0(inst::LDRX, X17, X0));
            let at = buf.len();
            buf.reloc(at, Reloc::Aarch64TlsDescAddLo12, name, 0);
            buf.put4(inst::add_imm(X0, X0, 0, false, false));
            let at = buf.len();
            buf.reloc(at, Reloc::Aarch64TlsDescCall, name, 0);
            buf.put4(inst::blr(X17));
            let at = buf.len();
            cx.call_site(at, inst);
            let buf = cx.buf();
            buf.put4(inst::mrs_tpidr_el0(X17));
            buf.put4(inst::add(X0, X0, X17));
        }
        TlsModel::MachO => {
            let at = buf.len();
            buf.reloc(at, Reloc::MachOAarch64TlsAdrPage21, name, 0);
            buf.put4(inst::adrp(X0));
            let at = buf.len();
            buf.reloc(at, Reloc::MachOAarch64TlsAdrPageOff12, name, 0);
            buf.put4(inst::at0(inst::LDRX, X0, X0));
            buf.put4(inst::at0(inst::LDRX, X1, X0));
            buf.put4(inst::blr(X1));
            let at = buf.len();
            cx.call_site(at, inst);
        }
        TlsModel::Coff => {
            return Err(Error::at(
                inst,
                "`tls_value` with COFF's model, which Cranelift's AArch64 backend does not lower",
            ));
        }
    }
    Ok(())
}
