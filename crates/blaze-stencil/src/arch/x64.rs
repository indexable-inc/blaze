//! x86-64's glue: System V (Linux, macOS) and Windows x64, written as Cranelift 0.136.2's
//! `isa/x64` writes the same IR for the same flags (stencil.md, "The glue per
//! architecture"). The stencils' frame register is `r12`; every function keeps `rbp` as its
//! frame pointer and saves every callee-saved register of its convention; `r10` and `r11`
//! are the glue's scratch.

use cranelift_codegen::ir;

use super::{Area, Cx, Glue};
use crate::{
    Error, Target,
    code::{self, Name},
    tier,
};

mod abi;
mod asm;
mod frame;
mod inst;
#[cfg(test)]
mod tests;

pub use frame::Frame;

/// x86-64.
pub struct X64;

/// `add qword [rip + disp32], 1`.
const COUNT: u32 = 8;

/// Adds one to the counter cell `cell` bytes into the function's cells: the field is 5 bytes
/// before the end of the instruction, which the addend takes back.
fn count(cx: &mut Cx<'_, '_, X64>, cell: i64) {
    let field = asm::add_rip_one(cx.buf);
    cx.buf.reloc(field, cranelift_codegen::binemit::Reloc::X86PCRel4, Name::Counter, cell - 5);
}

impl Glue for X64 {
    const ARCH: blaze_codegen::Arch = blaze_codegen::Arch::X86_64;
    /// `r12`.
    const FRAME: u8 = 12;
    /// Every slot is a disp32 from `r12` away.
    const NEAR: [u32; 2] = [i32::MAX as u32, i32::MAX as u32];
    /// `nop`.
    const NOP: &'static [u8] = &[0x90];
    type Frame = Frame;

    fn frame(func: &ir::Function, target: &Target, area: Area) -> Result<Frame, Error> {
        frame::layout(func, target, area)
    }

    fn takes(func: &ir::Function, target: &Target, inst: ir::Inst) -> bool {
        let _ = target;
        inst::takes(func.dfg.insts[inst].opcode())
    }

    fn bound(func: &ir::Function, target: &Target, inst: ir::Inst) -> u32 {
        inst::bound(func, target, inst)
    }

    fn bound_fixed(frame: &Frame) -> u32 {
        // The prologue, a landing pad, a back-edge count, a trap stub's `ud2`; no relays.
        frame.prologue.max(inst::LANDING).max(COUNT).max(2)
    }

    fn prologue(cx: &mut Cx<'_, '_, X64>) -> Result<(), Error> {
        let start = cx.buf.len();
        let frame = *cx.frame;
        {
            let (buf, side) = (&mut *cx.buf, &mut *cx.side);
            frame::enter(buf, &mut |at, rule| side.rule(at, rule), &frame)?;
        }
        count(cx, tier::CALLS);
        let func = cx.func;
        if let (Some(off), Some(place)) = (frame.sret, frame::sret_place(func)?) {
            frame::param(cx.buf, frame.grow, place, ir::types::I64, super::Loc::Slot(off))?;
        }
        let entry = func.layout.entry_block().ok_or_else(|| Error::new("a function with no blocks"))?;
        let params = func.dfg.block_params(entry);
        if params.len() != func.signature.params.len() {
            return Err(Error::new("an entry block whose parameters are not the signature's"));
        }
        abi::args(&func.signature, |i, param, place| {
            frame::param(cx.buf, frame.grow, place, param.value_type, cx.plan.loc(func, params[i]))
        })?;
        debug_assert!(cx.buf.len() - start <= frame.prologue, "the prologue's bound");
        Ok(())
    }

    fn inst(cx: &mut Cx<'_, '_, X64>, inst: ir::Inst) -> Result<(), Error> {
        let start = cx.buf.len();
        inst::inst(cx, inst)?;
        debug_assert!(
            cx.buf.len() - start <= inst::bound(cx.func, cx.target, inst),
            "the bound of {inst}"
        );
        Ok(())
    }

    fn landing(cx: &mut Cx<'_, '_, X64>, inst: ir::Inst) -> Result<(), Error> {
        inst::landing(cx, inst)
    }

    fn trap(cx: &mut Cx<'_, '_, X64>, code: ir::TrapCode) {
        let _ = code;
        asm::ud2(cx.buf);
    }

    fn count_back_edge(cx: &mut Cx<'_, '_, X64>) {
        count(cx, tier::BACK_EDGES);
    }

    fn relay(cx: &mut Cx<'_, '_, X64>, from: u32, to: u32, bytes: u8) {
        let _ = (cx, from, to, bytes);
        unreachable!(
            "a relay on x86-64, where every slot is a disp32 away (`NEAR`) and `frame` refuses \
             a frame of more than 1 GiB"
        );
    }

    fn unwind(cx: &mut Cx<'_, '_, X64>) -> Result<Option<code::UnwindKind>, Error> {
        let len = cx.buf.len();
        if cx.frame.win_unwind {
            let mut raw = Vec::new_in(cx.temp());
            frame::win_raw(cx.rows(), cx.frame.clobber, &mut raw)?;
            frame::with_saves(&raw, cx.windows_info())?;
            return Ok(Some(code::UnwindKind::Windows { len }));
        }
        // An epilogue at the very end leaves a row at `len`, past the code.
        cx.retain_rows(|row| row.0 < len);
        Ok(Some(code::UnwindKind::Dwarf { len }))
    }

    fn slot_base(frame: &Frame) -> (u16, i32) {
        let _ = frame;
        (frame::DWARF_R12, 0)
    }

    fn frame_size(frame: &Frame) -> u32 {
        frame.total()
    }
}
