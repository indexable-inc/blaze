//! The instructions the glue writes on x86-64, each as Cranelift 0.136.2's `isa/x64`
//! lowers it (`lower.isle`, `lower.rs`, `inst/emit.rs`) for the same IR and flags: calls,
//! returns, symbol and thread-local addresses, global values, jump tables, traps, the frame
//! queries, and the float operations a baseline CPU has no instruction for, as libcalls.
//!
//! Operands come from their slots above `r12` (or are materialized, when folded), results go
//! to theirs; `r10` and `r11` are the glue's scratch, neither an argument register in either
//! convention, so nothing passed is clobbered by a copy.

use cranelift_codegen::{
    binemit::Reloc,
    ir::{self, InstructionData, LibCall, Opcode, types},
    isa::CallConv,
};

use super::{
    X64,
    abi::{self, Part, Place},
    asm::{self, ADD, Mem, R8, R10, R11, RAX, RBP, RCX, RDI, RDX, RSI, RSP, Sse, at},
    frame::{self, GV_DEPTH, blame, slot},
};
use crate::{
    Error, Target, TlsModel,
    arch::{Cx, Loc},
    code::Name,
    enc::Form,
};

/// Whether the glue writes an instruction of `opcode`.
pub fn takes(opcode: Opcode) -> bool {
    matches!(
        opcode,
        Opcode::Return
            | Opcode::Call
            | Opcode::CallIndirect
            | Opcode::TryCall
            | Opcode::TryCallIndirect
            | Opcode::ReturnCall
            | Opcode::ReturnCallIndirect
            | Opcode::FuncAddr
            | Opcode::SymbolValue
            | Opcode::TlsValue
            | Opcode::BrTable
            | Opcode::Trap
            | Opcode::Debugtrap
            | Opcode::GetFramePointer
            | Opcode::GetStackPointer
            | Opcode::GetReturnAddress
            | Opcode::StackSwitch
            | Opcode::Ceil
            | Opcode::Floor
            | Opcode::Nearest
            | Opcode::Trunc
            | Opcode::Fma
            | Opcode::Swizzle
            | Opcode::Shuffle
    )
}

/// An operand that has no place: the patcher's mistake, named.
fn no_place(inst: ir::Inst, value: ir::Value) -> Error {
    // alloc: the error's message.
    Error::at(inst, format!("the operand {value} has no place"))
}

/// The 64 bits a folded constant of `ty` (its bits in `bits`) is materialized as, extended
/// as `ext` says (`Some(signed)`) or zero-extended.
fn konst(bits: i64, ty: ir::Type, ext: Option<bool>) -> u64 {
    let width = ty.bits();
    if width >= 64 {
        return bits as u64;
    }
    let shift = 64 - width;
    match ext {
        Some(true) => (((bits as u64) << shift) as i64 >> shift) as u64,
        _ => ((bits as u64) << shift) >> shift,
    }
}

/// `value` into general register `dst`, extended to 64 bits as `ext` says.
fn to_gpr(
    cx: &mut Cx<'_, '_, X64>,
    inst: ir::Inst,
    dst: u8,
    value: ir::Value,
    ext: Option<bool>,
) -> Result<(), Error> {
    let ty = cx.func.dfg.value_type(value);
    match cx.loc(value) {
        Loc::Slot(off) => match ext {
            Some(signed) => asm::load_ext(cx.buf, dst, slot(off), ty.bits(), signed),
            None => asm::load(cx.buf, dst, slot(off)),
        },
        Loc::Const(bits) => asm::imm(cx.buf, dst, konst(bits, ty, ext)),
        Loc::Dead => return Err(no_place(inst, value)),
    }
    Ok(())
}

/// `value` into XMM register `dst`.
fn to_xmm(cx: &mut Cx<'_, '_, X64>, inst: ir::Inst, dst: u8, value: ir::Value) -> Result<(), Error> {
    let ty = cx.func.dfg.value_type(value);
    match cx.loc(value) {
        Loc::Slot(off) => asm::xload(cx.buf, Sse::of(ty.bytes()), dst, slot(off)),
        Loc::Const(bits) => {
            asm::imm(cx.buf, R10, konst(bits, ty, None));
            asm::movq_to_xmm(cx.buf, dst, R10);
        }
        Loc::Dead => return Err(no_place(inst, value)),
    }
    Ok(())
}

/// The slot of a result, or `None` when it is never used.
fn result_slot(inst: ir::Inst, loc: Loc) -> Result<Option<u32>, Error> {
    match loc {
        Loc::Slot(off) => Ok(Some(off)),
        Loc::Dead => Ok(None),
        Loc::Const(_) => Err(Error::at(inst, "a result folded to a constant")),
    }
}

/// The slot of a 16-byte operand, which is never folded.
fn wide_slot(cx: &Cx<'_, '_, X64>, inst: ir::Inst, value: ir::Value) -> Result<u32, Error> {
    match cx.loc(value) {
        Loc::Slot(off) => Ok(off),
        _ => Err(no_place(inst, value)),
    }
}

/// An address into `dst` as Cranelift's `LoadExtName` writes it: from the GOT when the code
/// is position-independent (then the offset added), `lea` of a near symbol, `movabs` of a far
/// one.
fn load_ext_name(
    cx: &mut Cx<'_, '_, X64>,
    dst: u8,
    name: Name,
    offset: i64,
    near: bool,
) -> Result<(), Error> {
    let buf = &mut *cx.buf;
    if cx.target.pic {
        let field = asm::rip(buf, None, true, &[0x8b], dst);
        buf.reloc(field, Reloc::X86GOTPCRel4, name, -4);
        if offset != 0 {
            let offset = i32::try_from(offset).map_err(|_| {
                // alloc: the error's message.
                Error::new(format!("a symbol offset of {offset}, beyond 32 bits"))
            })?;
            asm::alu_imm32(buf, ADD, dst, offset);
        }
    } else if near {
        let field = asm::rip(buf, None, true, &[0x8d], dst);
        buf.reloc(field, Reloc::X86PCRel4, name, offset - 4);
    } else {
        let field = asm::movabs(buf, dst, 0);
        buf.reloc(field, Reloc::Abs8, name, offset);
    }
    Ok(())
}

/// The most bytes of [`load_ext_name`]: the GOT load and the `add`.
const EXT_NAME: u32 = 7 + 7;

/// A call to `libcall` through `r11` (Cranelift calls libcalls as `RelocDistance::Far`),
/// recorded as a call site of `inst` when `site`.
fn call_libcall(
    cx: &mut Cx<'_, '_, X64>,
    inst: ir::Inst,
    libcall: LibCall,
    site: bool,
) -> Result<(), Error> {
    load_ext_name(cx, R11, Name::LibCall(libcall), 0, false)?;
    asm::call_reg(cx.buf, R11);
    if site {
        let ret = cx.buf.len();
        cx.call_site(ret, inst);
    }
    Ok(())
}

/// [`call_libcall`]'s most bytes.
const LIBCALL: u32 = EXT_NAME + 3;

/// Writes `inst`.
pub fn inst(cx: &mut Cx<'_, '_, X64>, inst: ir::Inst) -> Result<(), Error> {
    let func = cx.func;
    let data = func.dfg.insts[inst];
    match data.opcode() {
        Opcode::Return => ret(cx, inst),
        Opcode::Call | Opcode::CallIndirect | Opcode::TryCall | Opcode::TryCallIndirect => {
            call(cx, inst)
        }
        Opcode::ReturnCall | Opcode::ReturnCallIndirect => return_call(cx, inst),
        Opcode::FuncAddr => {
            let InstructionData::FuncAddr { func_ref, .. } = data else {
                return Err(Error::at(inst, "a `func_addr` of another format"));
            };
            let Some(dst) = result_slot(inst, cx.loc(func.dfg.first_result(inst)))? else {
                return Ok(());
            };
            let ext = &func.dfg.ext_funcs[func_ref];
            let name = cx.name(&ext.name).map_err(blame(inst))?;
            load_ext_name(cx, RAX, name, 0, ext.colocated).map_err(blame(inst))?;
            asm::store(cx.buf, slot(dst), RAX);
            Ok(())
        }
        Opcode::SymbolValue | Opcode::TlsValue => {
            let InstructionData::UnaryGlobalValue { global_value: gv, .. } = data else {
                return Err(Error::at(inst, "a global value instruction of another format"));
            };
            let Some(dst) = result_slot(inst, cx.loc(func.dfg.first_result(inst)))? else {
                return Ok(());
            };
            let tls = match func.global_values[gv] {
                ir::GlobalValueData::Symbol { tls, .. } => Some(tls),
                _ => None,
            };
            match (data.opcode(), tls) {
                (Opcode::SymbolValue, Some(false)) => {}
                (Opcode::TlsValue, Some(true)) => {}
                (opcode, _) => {
                    return Err(Error::at(
                        inst,
                        // alloc: the error's message.
                        format!(
                            "a `{opcode}` of a global value other than a{} symbol",
                            if opcode == Opcode::TlsValue { " thread-local" } else { "n ordinary" }
                        ),
                    ));
                }
            }
            global_value(cx, inst, gv, GV_DEPTH)?;
            asm::store(cx.buf, slot(dst), RAX);
            Ok(())
        }
        Opcode::BrTable => br_table(cx, inst),
        Opcode::Trap => {
            asm::ud2(cx.buf);
            Ok(())
        }
        Opcode::Debugtrap => {
            asm::int3(cx.buf);
            Ok(())
        }
        Opcode::GetFramePointer | Opcode::GetStackPointer | Opcode::GetReturnAddress => {
            let Some(dst) = result_slot(inst, cx.loc(func.dfg.first_result(inst)))? else {
                return Ok(());
            };
            let src = match data.opcode() {
                Opcode::GetFramePointer => RBP,
                Opcode::GetStackPointer => RSP,
                _ => {
                    asm::load(cx.buf, RAX, at(RBP, 8));
                    RAX
                }
            };
            asm::store(cx.buf, slot(dst), src);
            Ok(())
        }
        Opcode::Ceil | Opcode::Floor | Opcode::Nearest | Opcode::Trunc | Opcode::Fma => {
            libcall_op(cx, inst)
        }
        Opcode::Swizzle | Opcode::Shuffle => bytes_op(cx, inst),
        opcode => Err(Error::at(
            inst,
            // alloc: the error's message.
            format!("`{opcode}`, which the frame's layout refuses before any of it is written"),
        )),
    }
}

/// `return`: the values to their registers as the function's signature says, the
/// StructReturn pointer to `rax`, then the epilogue.
fn ret(cx: &mut Cx<'_, '_, X64>, inst: ir::Inst) -> Result<(), Error> {
    let func = cx.func;
    let args = func.dfg.inst_args(inst);
    if args.len() != func.signature.returns.len() {
        return Err(Error::at(inst, "a `return` of other values than the signature's"));
    }
    // In `tail`, the XMM values first: a constant goes to one through `r10`, which `tail`
    // returns in.
    let tail = func.signature.call_conv == CallConv::Tail;
    for pass in 0..if tail { 2 } else { 1 } {
        abi::rets(&func.signature, |k, param, place| {
            if tail && (pass == 0) != matches!(place, Place::Xmm(_)) {
                return Ok(());
            }
            let value = args[k];
            match place {
                Place::Xmm(reg) => to_xmm(cx, inst, reg, value),
                Place::Gpr(reg) => to_gpr(cx, inst, reg, value, abi::extension(param)),
                Place::Pair(Part::Gpr(lo), Part::Gpr(hi)) => {
                    let off = wide_slot(cx, inst, value)?;
                    asm::load(cx.buf, lo, slot(off));
                    asm::load(cx.buf, hi, slot(off + 8));
                    Ok(())
                }
                _ => Err(Error::at(inst, "a return value on the stack")),
            }
        })
        .map_err(blame(inst))?;
    }
    if let Some(off) = cx.frame.sret {
        asm::load(cx.buf, RAX, slot(off));
    }
    let frame = *cx.frame;
    let (buf, side) = (&mut *cx.buf, &mut *cx.side);
    frame::leave(buf, &mut |at, rule| side.rule(at, rule), &frame);
    Ok(())
}

/// The callee of a call.
#[derive(Clone, Copy)]
enum Callee {
    Direct(ir::FuncRef),
    Indirect(ir::Value),
}

/// The callee, signature and arguments of a call.
fn call_of(func: &ir::Function, inst: ir::Inst) -> Result<(Callee, ir::SigRef, &[ir::Value]), Error> {
    let data = &func.dfg.insts[inst];
    Ok(match data.analyze_call(&func.dfg.value_lists, &func.dfg.exception_tables) {
        ir::instructions::CallInfo::Direct(f, args) => {
            (Callee::Direct(f), func.dfg.ext_funcs[f].signature, args)
        }
        ir::instructions::CallInfo::DirectWithSig(f, sig, args) => (Callee::Direct(f), sig, args),
        ir::instructions::CallInfo::Indirect(sig, args) => {
            (Callee::Indirect(func.dfg.inst_args(inst)[0]), sig, args)
        }
        ir::instructions::CallInfo::NotACall => {
            return Err(Error::at(inst, "a call the IR says is not one"));
        }
    })
}

/// A call (`call`, `call_indirect`, `try_call`, `try_call_indirect`) as Cranelift's
/// `gen_call_args`, `CallKnown`/`CallUnknown` and `gen_call_rets` write it: each
/// StructArgument copied by a `memcpy` first, then the stack arguments, then the register
/// ones, the call, its return values to their places.
fn call(cx: &mut Cx<'_, '_, X64>, inst: ir::Inst) -> Result<(), Error> {
    let func = cx.func;
    let (callee, sig_ref, args) = call_of(func, inst)?;
    let sig = &func.dfg.signatures[sig_ref];
    if args.len() != sig.params.len() {
        return Err(Error::at(inst, "a call with other arguments than its signature's"));
    }
    // A `memcpy` is a call site of no handlers, which a `try_call`'s record can't say.
    let site = func.dfg.insts[inst].exception_table().is_none();
    let win = sig.call_conv == CallConv::WindowsFastcall;
    let stack = abi::args(sig, |i, _, place| match place {
        Place::Struct { at, size } => memcpy(cx, inst, win, args[i], at, size, site),
        _ => Ok(()),
    })
    .map_err(blame(inst))?;

    abi::args(sig, |i, param, place| {
        let value = args[i];
        let ext = abi::extension(param);
        match place {
            Place::Stack(at_) => {
                if param.value_type.bytes() > 8 {
                    let off = wide_slot(cx, inst, value)?;
                    copy(cx.buf, slot(off), at(RSP, at_ as i32));
                    copy(cx.buf, slot(off + 8), at(RSP, at_ as i32 + 8));
                } else {
                    to_gpr(cx, inst, R10, value, ext)?;
                    asm::store(cx.buf, at(RSP, at_ as i32), R10);
                }
            }
            Place::Pair(lo, hi) => {
                let off = wide_slot(cx, inst, value)?;
                for (k, part) in [lo, hi].into_iter().enumerate() {
                    if let Part::Stack(at_) = part {
                        copy(cx.buf, slot(off + 8 * k as u32), at(RSP, at_ as i32));
                    }
                }
            }
            Place::Implicit { pointer, copy: to } => {
                let off = wide_slot(cx, inst, value)?;
                copy(cx.buf, slot(off), at(RSP, to as i32));
                copy(cx.buf, slot(off + 8), at(RSP, to as i32 + 8));
                if let Part::Stack(at_) = pointer {
                    asm::lea(cx.buf, R10, at(RSP, to as i32));
                    asm::store(cx.buf, at(RSP, at_ as i32), R10);
                }
            }
            Place::Gpr(_) | Place::Xmm(_) | Place::Struct { .. } => {}
        }
        Ok(())
    })
    .map_err(blame(inst))?;

    abi::args(sig, |i, param, place| {
        let value = args[i];
        match place {
            Place::Gpr(reg) => to_gpr(cx, inst, reg, value, abi::extension(param))?,
            Place::Xmm(reg) => to_xmm(cx, inst, reg, value)?,
            Place::Pair(lo, hi) => {
                let off = wide_slot(cx, inst, value)?;
                for (k, part) in [lo, hi].into_iter().enumerate() {
                    if let Part::Gpr(reg) = part {
                        asm::load(cx.buf, reg, slot(off + 8 * k as u32));
                    }
                }
            }
            Place::Implicit { pointer: Part::Gpr(reg), copy: to } => {
                asm::lea(cx.buf, reg, at(RSP, to as i32));
            }
            Place::Stack(_) | Place::Struct { .. } | Place::Implicit { .. } => {}
        }
        Ok(())
    })
    .map_err(blame(inst))?;

    match callee {
        Callee::Direct(func_ref) => {
            let ext = &func.dfg.ext_funcs[func_ref];
            let name = cx.name(&ext.name).map_err(blame(inst))?;
            if ext.colocated {
                let field = asm::call_rel32(cx.buf);
                cx.buf.reloc(field, Reloc::X86CallPCRel4, name, -4);
            } else {
                load_ext_name(cx, R11, name, 0, false).map_err(blame(inst))?;
                asm::call_reg(cx.buf, R11);
            }
        }
        Callee::Indirect(value) => {
            to_gpr(cx, inst, R11, value, None)?;
            asm::call_reg(cx.buf, R11);
        }
    }
    let ret = cx.buf.len();
    cx.call_site(ret, inst);
    if sig.call_conv == CallConv::Tail && stack > 0 {
        // A `tail` callee popped its stack arguments: the outgoing area back (`callee_pop_size`).
        asm::alu_imm(cx.buf, asm::SUB, RSP, stack as i32);
    }

    abi::rets(sig, |k, _, place| {
        let Some(off) = result_slot(inst, cx.returned(inst, k))? else { return Ok(()) };
        match place {
            Place::Gpr(reg) => asm::store(cx.buf, slot(off), reg),
            Place::Xmm(reg) => {
                let bytes = sig.returns[k].value_type.bytes();
                asm::xstore(cx.buf, Sse::of(bytes), slot(off), reg);
            }
            Place::Pair(Part::Gpr(lo), Part::Gpr(hi)) => {
                asm::store(cx.buf, slot(off), lo);
                asm::store(cx.buf, slot(off + 8), hi);
            }
            _ => return Err(Error::at(inst, "a return value on the stack")),
        }
        Ok(())
    })
    .map_err(blame(inst))
}

/// `return_call` and `return_call_indirect` from a `tail` function to one (the frame's layout
/// checked both), as Cranelift's `gen_call_args` (`is_tail_call`) and
/// `emit_return_call_common_sequence` write them: the stack arguments into the top of the
/// incoming argument area (`IncomingArg`, `pop` bytes above the saved `rbp` and return
/// address), the callee to `r11`, the register arguments, the frame popped, the return
/// address moved up to below the new arguments and `rsp` with it, then the jump. Every slot is
/// read before the restores give `r12` back; the incoming area holds nothing the function
/// still reads, its own stack arguments copied to slots by the prologue.
fn return_call(cx: &mut Cx<'_, '_, X64>, inst: ir::Inst) -> Result<(), Error> {
    let func = cx.func;
    let (callee, sig_ref, args) = call_of(func, inst)?;
    let sig = &func.dfg.signatures[sig_ref];
    if args.len() != sig.params.len() {
        return Err(Error::at(inst, "a return call with other arguments than its signature's"));
    }
    let frame = *cx.frame;
    let new = abi::stack_args(sig).map_err(blame(inst))?;
    let base = 16 + frame.pop as i32 - new as i32;
    let to = |at_: u32| at(RBP, base + at_ as i32);

    abi::args(sig, |i, param, place| {
        let value = args[i];
        match place {
            Place::Stack(at_) => {
                if param.value_type.bytes() > 8 {
                    let off = wide_slot(cx, inst, value)?;
                    copy(cx.buf, slot(off), to(at_));
                    copy(cx.buf, slot(off + 8), to(at_ + 8));
                } else {
                    to_gpr(cx, inst, R10, value, abi::extension(param))?;
                    asm::store(cx.buf, to(at_), R10);
                }
            }
            Place::Pair(lo, hi) => {
                let off = wide_slot(cx, inst, value)?;
                for (k, part) in [lo, hi].into_iter().enumerate() {
                    if let Part::Stack(at_) = part {
                        copy(cx.buf, slot(off + 8 * k as u32), to(at_));
                    }
                }
            }
            Place::Struct { .. } | Place::Implicit { .. } => {
                return Err(Error::at(inst, "a tail call's argument copied to the stack"));
            }
            Place::Gpr(_) | Place::Xmm(_) => {}
        }
        Ok(())
    })
    .map_err(blame(inst))?;

    let near = match callee {
        Callee::Direct(func_ref) => {
            let ext = &func.dfg.ext_funcs[func_ref];
            let name = cx.name(&ext.name).map_err(blame(inst))?;
            if !ext.colocated {
                load_ext_name(cx, R11, name, 0, false).map_err(blame(inst))?;
            }
            ext.colocated.then_some(name)
        }
        Callee::Indirect(value) => {
            to_gpr(cx, inst, R11, value, None)?;
            None
        }
    };

    abi::args(sig, |i, param, place| {
        let value = args[i];
        match place {
            Place::Gpr(reg) => to_gpr(cx, inst, reg, value, abi::extension(param))?,
            Place::Xmm(reg) => to_xmm(cx, inst, reg, value)?,
            Place::Pair(lo, hi) => {
                let off = wide_slot(cx, inst, value)?;
                for (k, part) in [lo, hi].into_iter().enumerate() {
                    if let Part::Gpr(reg) = part {
                        asm::load(cx.buf, reg, slot(off + 8 * k as u32));
                    }
                }
            }
            _ => {}
        }
        Ok(())
    })
    .map_err(blame(inst))?;

    {
        let (buf, side) = (&mut *cx.buf, &mut *cx.side);
        frame::unframe(buf, &mut |at, rule| side.rule(at, rule), &frame);
    }
    let shrink = frame.pop - new;
    if shrink > 0 {
        asm::load(cx.buf, R10, at(RSP, 0));
        asm::store(cx.buf, at(RSP, shrink as i32), R10);
        asm::alu_imm32(cx.buf, ADD, RSP, shrink as i32);
    }
    match near {
        Some(name) => {
            let field = asm::jmp_rel32(cx.buf);
            cx.buf.reloc(field, Reloc::X86CallPCRel4, name, -4);
        }
        None => asm::jmp_reg(cx.buf, R11),
    }
    cx.side.rule(cx.buf.len(), crate::code::Rule::RestoreState);
    Ok(())
}

/// A StructArgument's copy as Cranelift's `gen_memcpy` makes it: `memcpy(rsp + to, value,
/// size)` in the callee's convention, through `r11`.
fn memcpy(
    cx: &mut Cx<'_, '_, X64>,
    inst: ir::Inst,
    win: bool,
    value: ir::Value,
    to: u32,
    size: u32,
    site: bool,
) -> Result<(), Error> {
    let (dst, src, len) = if win { (RCX, RDX, R8) } else { (RDI, RSI, RDX) };
    asm::lea(cx.buf, dst, at(RSP, to as i32));
    to_gpr(cx, inst, src, value, None)?;
    asm::imm(cx.buf, len, u64::from(size));
    call_libcall(cx, inst, LibCall::Memcpy, site)
}

/// 8 bytes from `from` to `to` through `r10`.
fn copy(buf: &mut crate::buf::Buf<'_>, from: Mem, to: Mem) {
    asm::load(buf, R10, from);
    asm::store(buf, to, R10);
}

/// `landing`: the exception payload registers of System V (`rax`, `rdx`, Cranelift's
/// `exception_payload_regs`) to their places.
pub fn landing(cx: &mut Cx<'_, '_, X64>, inst: ir::Inst) -> Result<(), Error> {
    for (k, reg) in [RAX, RDX].into_iter().enumerate() {
        if let Some(off) = result_slot(inst, cx.payload(inst, k))? {
            asm::store(cx.buf, slot(off), reg);
        }
    }
    Ok(())
}

/// The most bytes of [`landing`].
pub const LANDING: u32 = 2 * 8;

/// The value of global value `gv` into `rax`, as Cranelift's legalizer expands it: a symbol's
/// address (or a thread-local's), plus an immediate, or loaded from.
fn global_value(
    cx: &mut Cx<'_, '_, X64>,
    inst: ir::Inst,
    gv: ir::GlobalValue,
    depth: u32,
) -> Result<(), Error> {
    if depth == 0 {
        return Err(Error::at(inst, "a chain of global values too deep"));
    }
    let func = cx.func;
    match func.global_values[gv] {
        ir::GlobalValueData::Symbol { ref name, offset, colocated, tls } => {
            let name = cx.name(name).map_err(blame(inst))?;
            if tls {
                thread_local(cx, name);
            } else {
                load_ext_name(cx, RAX, name, offset.bits(), colocated).map_err(blame(inst))?;
            }
        }
        ir::GlobalValueData::IAddImm { base, offset, .. } => {
            global_value(cx, inst, base, depth - 1)?;
            let offset = offset.bits();
            if offset != 0 {
                match i32::try_from(offset) {
                    Ok(offset) => asm::alu_imm(cx.buf, ADD, RAX, offset),
                    Err(_) => {
                        asm::movabs(cx.buf, R10, offset as u64);
                        asm::add(cx.buf, RAX, R10);
                    }
                }
            }
        }
        ir::GlobalValueData::Load { base, offset, .. } => {
            global_value(cx, inst, base, depth - 1)?;
            asm::load(cx.buf, RAX, at(RAX, i32::from(offset)));
        }
        ir::GlobalValueData::VMContext | ir::GlobalValueData::DynScaleTargetConst { .. } => {
            return Err(Error::at(inst, "a global value the frame's layout refuses"));
        }
    }
    Ok(())
}

/// The most bytes of [`global_value`] of `gv`.
fn gv_bound(func: &ir::Function, target: &Target, gv: ir::GlobalValue) -> u32 {
    let mut bytes = 0;
    let mut gv = gv;
    for _ in 0..GV_DEPTH {
        match func.global_values[gv] {
            ir::GlobalValueData::Symbol { tls: false, .. } => return bytes + EXT_NAME,
            ir::GlobalValueData::Symbol { tls: true, .. } => {
                return bytes
                    + match target.tls {
                        TlsModel::ElfGd => 8 + 8,
                        TlsModel::MachO => 7 + 2,
                        TlsModel::Coff => 6 + 9 + 4 + 7,
                    };
            }
            // `add rax, imm32`, or `movabs r10` and `add rax, r10`.
            ir::GlobalValueData::IAddImm { base, .. } => {
                bytes += asm::MOVABS + 3;
                gv = base;
            }
            // `mov rax, [rax + disp32]`.
            ir::GlobalValueData::Load { base, .. } => {
                bytes += 7;
                gv = base;
            }
            ir::GlobalValueData::VMContext | ir::GlobalValueData::DynScaleTargetConst { .. } => {
                return bytes;
            }
        }
    }
    bytes
}

/// A thread-local's address into `rax`, as Cranelift's `ElfTlsGetAddr`, `MachOTlsGetAddr`
/// and `CoffTlsGetAddr` write it (the symbol's offset is not added, as Cranelift does not).
fn thread_local(cx: &mut Cx<'_, '_, X64>, name: Name) {
    let buf = &mut *cx.buf;
    match cx.target.tls {
        TlsModel::ElfGd => {
            // `data16 lea rdi, [rip + sym@tlsgd]`, `data16 data16 rex.w call __tls_get_addr@plt`.
            let field = asm::rip(buf, Some(0x66), true, &[0x8d], RDI);
            buf.reloc(field, Reloc::ElfX86_64TlsGd, name, -4);
            buf.put(&[0x66, 0x66, 0x48, 0xe8]);
            let field = buf.len();
            buf.put4(0);
            buf.reloc(field, Reloc::X86CallPLTRel4, Name::LibCall(LibCall::ElfTlsGetAddr), -4);
        }
        TlsModel::MachO => {
            // `mov rdi, [rip + sym@tlv]`, `call [rdi]`.
            let field = asm::rip(buf, None, true, &[0x8b], RDI);
            buf.reloc(field, Reloc::MachOX86_64Tlv, name, -4);
            asm::call_mem(buf, at(RDI, 0));
        }
        TlsModel::Coff => {
            // `mov eax, [rip + _tls_index]`, `mov rcx, gs:[0x58]`, `mov rax, [rcx + rax*8]`,
            // `lea rax, [rax + sym@secrel]`.
            let field = asm::rip(buf, None, false, &[0x8b], RAX);
            buf.reloc(field, Reloc::X86PCRel4, Name::Known(ir::KnownSymbol::CoffTlsIndex), -4);
            buf.put(&[0x65, 0x48, 0x8b, 0x0c, 0x25, 0x58, 0x00, 0x00, 0x00]);
            buf.put(&[0x48, 0x8b, 0x04, 0xc1]);
            buf.put(&[0x48, 0x8d, 0x80]);
            let field = buf.len();
            buf.put4(0);
            buf.reloc(field, Reloc::X86SecRel, name, 0);
        }
    }
}

/// `br_table` as Cranelift's `JmpTableSeq`: the index zero-extended and clamped to the
/// table's length (where the default is), then a jump through a table of 32-bit offsets from
/// its start, inline after the jump.
fn br_table(cx: &mut Cx<'_, '_, X64>, inst: ir::Inst) -> Result<(), Error> {
    let InstructionData::BranchTable { arg, table, .. } = cx.func.dfg.insts[inst] else {
        return Err(Error::at(inst, "a `br_table` of another format"));
    };
    let count = cx.func.dfg.jump_tables[table].as_slice().len();
    let count = u32::try_from(count).map_err(|_| Error::at(inst, "a jump table too long"))?;
    to_gpr(cx, inst, RAX, arg, Some(false))?;
    let buf = &mut *cx.buf;
    asm::imm(buf, RCX, u64::from(count));
    asm::cmp(buf, RAX, RCX);
    asm::cmov(buf, asm::CC_AE, RAX, RCX);
    let field = asm::rip(buf, None, true, &[0x8d], RCX);
    // The table starts after the `movsxd`, `add` and `jmp`.
    buf.bytes_mut(field)[..4].copy_from_slice(&9i32.to_le_bytes());
    asm::movsxd_index4(buf, RDX, RCX, RAX);
    asm::add(buf, RCX, RDX);
    asm::jmp_reg(buf, RCX);
    debug_assert_eq!(buf.len(), field + 4 + 9);
    for k in 0..=count {
        // The default (edge 0) is the last entry.
        let edge = if k == count { 0 } else { k as usize + 1 };
        let label = cx.edge(inst, edge);
        let entry = cx.buf.len();
        cx.buf.put4(0);
        cx.buf.fixup(entry, Form::Rel32, label, i64::from(4 * k));
    }
    Ok(())
}

/// A float operation of a baseline CPU's Cranelift lowers to libcalls (`x64_round_*` without
/// SSE4.1, `fma` without FMA): one per lane, each lane's operands to `xmm0`.. and its result
/// from `xmm0`.
fn libcall_op(cx: &mut Cx<'_, '_, X64>, inst: ir::Inst) -> Result<(), Error> {
    let func = cx.func;
    let opcode = func.dfg.insts[inst].opcode();
    let ty = func.dfg.ctrl_typevar(inst);
    let lane = ty.lane_type();
    let libcall = libcall_of(opcode, lane)
        // alloc: the error's message.
        .ok_or_else(|| Error::at(inst, format!("`{opcode}` of {ty} on x86-64")))?;
    let Some(dst) = result_slot(inst, cx.loc(func.dfg.first_result(inst)))? else {
        return Ok(());
    };
    let args = func.dfg.inst_args(inst);
    let sse = Sse::of(lane.bytes());
    if !ty.is_vector() {
        for (j, &arg) in args.iter().enumerate() {
            to_xmm(cx, inst, j as u8, arg)?;
        }
        call_libcall(cx, inst, libcall, true)?;
        asm::xstore(cx.buf, sse, slot(dst), 0);
        return Ok(());
    }
    for k in 0..ty.lane_count() {
        let lane_at = k * lane.bytes();
        for (j, &arg) in args.iter().enumerate() {
            let off = wide_slot(cx, inst, arg)?;
            asm::xload(cx.buf, sse, j as u8, slot(off + lane_at));
        }
        call_libcall(cx, inst, libcall, true)?;
        asm::xstore(cx.buf, sse, slot(dst + lane_at), 0);
    }
    Ok(())
}

/// `swizzle` and `shuffle` of `i8x16`, byte by byte through general registers. Without SSSE3
/// Cranelift calls the `X86Pshufb` libcall for them (`lower_pshufb`), which no runtime blaze
/// links provides; the bytes are the same. A lane takes byte `k` of `a` for an index `k` below
/// 16, byte `k - 16` of `b` (`shuffle`'s second operand) below 32, and 0 for any other, which
/// is what Cranelift's `paddusb` of 0x70 makes `pshufb` do for `swizzle`. Each half is built
/// in `rax` and `rdx` from its last byte down before either is stored, so a result in an
/// operand's slot reads no byte it wrote.
fn bytes_op(cx: &mut Cx<'_, '_, X64>, inst: ir::Inst) -> Result<(), Error> {
    let func = cx.func;
    let data = func.dfg.insts[inst];
    // Neither op is polymorphic, so its controlling type is INVALID: the result's is the lanes'.
    let ty = func.dfg.value_type(func.dfg.first_result(inst));
    if ty != types::I8X16 {
        // alloc: the error's message.
        return Err(Error::at(inst, format!("`{}` of {ty} on x86-64", data.opcode())));
    }
    let Some(dst) = result_slot(inst, cx.loc(func.dfg.first_result(inst)))? else {
        return Ok(());
    };
    let args = func.dfg.inst_args(inst);
    let (a, b) = (wide_slot(cx, inst, args[0])?, wide_slot(cx, inst, args[1])?);
    let mask = match data {
        InstructionData::Shuffle { imm, .. } => {
            let mask = func.dfg.immediates.get(imm).map(|it| it.as_slice());
            match mask {
                Some(mask) if mask.len() == 16 => Some(mask),
                _ => return Err(Error::at(inst, "a `shuffle` whose mask is not 16 bytes")),
            }
        }
        _ => None,
    };
    let buf = &mut *cx.buf;
    if mask.is_none() {
        asm::lea(buf, R11, slot(a));
    }
    for (half, acc) in [RAX, RDX].into_iter().enumerate() {
        asm::imm(buf, acc, 0);
        for lane in (8 * half..8 * half + 8).rev() {
            asm::shl_imm(buf, acc, 8);
            match mask {
                Some(mask) => {
                    let from = match mask[lane] {
                        k @ 0..16 => a + u32::from(k),
                        k @ 16..32 => b + u32::from(k - 16),
                        _ => continue,
                    };
                    asm::load_ext(buf, RCX, slot(from), 8, false);
                }
                None => {
                    // `r10` is all ones when the index is below 16, else 0.
                    asm::load_ext(buf, RCX, slot(b + lane as u32), 8, false);
                    asm::alu_imm(buf, asm::CMP, RCX, 16);
                    asm::sbb(buf, R10, R10);
                    asm::alu_imm(buf, asm::AND, RCX, 15);
                    asm::load_u8_index(buf, RCX, R11, RCX);
                    asm::and(buf, RCX, R10);
                }
            }
            asm::or(buf, acc, RCX);
        }
    }
    asm::store(buf, slot(dst), RAX);
    asm::store(buf, slot(dst + 8), RDX);
    Ok(())
}

/// [`bytes_op`]'s most bytes: the `lea`; per half the zero, then per lane the shift, the
/// lane's byte (at most the load, `cmp`, `sbb`, `and`, the indexed load and `and`) and the
/// `or`; the two stores.
const BYTES_OP: u32 = 8 + 2 * (5 + 8 * (4 + asm::MEM_MAX + 4 + 3 + 4 + 5 + 3 + 3)) + 2 * 8;

fn libcall_of(opcode: Opcode, lane: ir::Type) -> Option<LibCall> {
    let f32 = lane == types::F32;
    if !f32 && lane != types::F64 {
        return None;
    }
    Some(match (opcode, f32) {
        (Opcode::Ceil, true) => LibCall::CeilF32,
        (Opcode::Ceil, false) => LibCall::CeilF64,
        (Opcode::Floor, true) => LibCall::FloorF32,
        (Opcode::Floor, false) => LibCall::FloorF64,
        (Opcode::Nearest, true) => LibCall::NearestF32,
        (Opcode::Nearest, false) => LibCall::NearestF64,
        (Opcode::Trunc, true) => LibCall::TruncF32,
        (Opcode::Trunc, false) => LibCall::TruncF64,
        (Opcode::Fma, true) => LibCall::FmaF32,
        (Opcode::Fma, false) => LibCall::FmaF64,
        _ => return None,
    })
}

/// Bytes of a value to a general register: a load with a SIB and a disp32, or `movabs`.
const GPR: u32 = asm::MOVABS;
/// To an XMM register: a load, or a constant through `r10`.
const XMM: u32 = asm::MOVABS + 5;
/// 8 bytes from a slot to the stack through `r10`.
const COPY: u32 = 8 + 8;

/// The most bytes [`inst`] writes for `inst`; 0 for what the frame's layout refuses.
pub fn bound(func: &ir::Function, target: &Target, inst: ir::Inst) -> u32 {
    let data = &func.dfg.insts[inst];
    match data.opcode() {
        Opcode::Return => {
            let win = func.signature.call_conv == CallConv::WindowsFastcall;
            let mut bytes = 8 + frame::epilogue_bound(win);
            let _ = abi::rets(&func.signature, |_, _, place| {
                bytes += place_bound(place);
                Ok(())
            });
            bytes
        }
        Opcode::Call | Opcode::CallIndirect | Opcode::TryCall | Opcode::TryCallIndirect => {
            let Ok((_, sig, _)) = call_of(func, inst) else { return 0 };
            let sig = &func.dfg.signatures[sig];
            // The callee: a GOT load and `call r11`, or a slot to `r11`.
            let mut bytes = EXT_NAME.max(GPR) + 3;
            let _ = abi::args(sig, |_, _, place| {
                bytes += match place {
                    // `lea`, the source, the size, the call.
                    Place::Struct { .. } => 8 + GPR + 6 + LIBCALL,
                    Place::Stack(_) | Place::Pair(..) => 2 * (GPR + 8),
                    Place::Implicit { .. } => 2 * COPY + 8 + 8,
                    place => place_bound(place),
                };
                Ok(())
            });
            let _ = abi::rets(sig, |_, _, place| {
                bytes += match place {
                    Place::Pair(..) => 16,
                    _ => 10,
                };
                Ok(())
            });
            // A `tail` callee's popped arguments taken back.
            bytes + 7
        }
        Opcode::ReturnCall | Opcode::ReturnCallIndirect => {
            let Ok((_, sig, _)) = call_of(func, inst) else { return 0 };
            let sig = &func.dfg.signatures[sig];
            let win = func.signature.call_conv == CallConv::WindowsFastcall;
            // The callee, the epilogue but its `ret`, the return address moved and `rsp` with
            // it, the jump.
            let mut bytes = EXT_NAME.max(GPR) + frame::epilogue_bound(win) + 8 + 8 + 7 + 5;
            let _ = abi::args(sig, |_, _, place| {
                bytes += match place {
                    Place::Stack(_) | Place::Pair(..) => 2 * (GPR + 8),
                    place => place_bound(place),
                };
                Ok(())
            });
            bytes
        }
        // The address and its store.
        Opcode::FuncAddr => EXT_NAME + 8,
        Opcode::SymbolValue | Opcode::TlsValue => {
            let InstructionData::UnaryGlobalValue { global_value, .. } = *data else { return 0 };
            gv_bound(func, target, global_value) + 8
        }
        Opcode::BrTable => {
            let InstructionData::BranchTable { table, .. } = *data else { return 0 };
            let count = func.dfg.jump_tables[table].as_slice().len() as u32;
            // The index, `mov ecx`, `cmp`, `cmov`, `lea`, `movsxd`, `add`, `jmp`, the table.
            GPR + 5 + 3 + 4 + 7 + 4 + 3 + 2 + 4 * (count + 1)
        }
        Opcode::Trap => 2,
        Opcode::Debugtrap => 1,
        Opcode::GetFramePointer | Opcode::GetStackPointer | Opcode::GetReturnAddress => 4 + 8,
        Opcode::Ceil | Opcode::Floor | Opcode::Nearest | Opcode::Trunc | Opcode::Fma => {
            let ty = func.dfg.ctrl_typevar(inst);
            let args = func.dfg.inst_args(inst).len() as u32;
            ty.lane_count() * (args * XMM + LIBCALL + asm::MEM_MAX)
        }
        Opcode::Swizzle | Opcode::Shuffle => BYTES_OP,
        _ => 0,
    }
}

/// The most bytes moving a value to or from `place` takes, for a register place.
fn place_bound(place: Place) -> u32 {
    match place {
        Place::Gpr(_) => GPR,
        Place::Xmm(_) => XMM,
        Place::Pair(..) => 2 * 8,
        _ => 0,
    }
}
