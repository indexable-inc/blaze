//! AArch64's glue: AAPCS64 (Linux, Windows) and Apple's variant, written as Cranelift
//! 0.136.2's `isa/aarch64` writes the same IR for the same flags (stencil.md, "The glue per
//! architecture"). The stencils' frame register is `x20`; every function keeps `x29` as its
//! frame pointer; `x16` and `x17` are the glue's scratch, `x18` is never written.

use cranelift_codegen::ir;

use super::{Area, Cx, Glue, Loc};
use crate::{
    Error, Target,
    buf::{Buf, Label},
    code::{self, Name, Rule},
};

mod abi;
mod frame;
mod glue;
mod inst;
#[cfg(test)]
pub(crate) mod tests;

pub use frame::Frame;

/// AArch64.
pub struct Aarch64;

/// What the glue bodies read and record: [`Cx`], or a test's stand-in for the patcher. `'f` is
/// the function's borrows, `'m` the compile's memory frame the buffer is on.
trait Ctx<'f, 'm> {
    fn buf(&mut self) -> &mut Buf<'m>;
    fn func(&self) -> &'f ir::Function;
    fn target(&self) -> &'f Target;
    fn frame(&self) -> &'f Frame;
    fn loc(&self, value: ir::Value) -> Loc;
    fn returned(&self, inst: ir::Inst, k: usize) -> Loc;
    fn payload(&self, inst: ir::Inst, k: usize) -> Loc;
    fn edge(&self, inst: ir::Inst, i: usize) -> Label;
    fn rule_at(&mut self, at: u32, rule: Rule);
    fn call_site(&mut self, ret: u32, inst: ir::Inst);
    fn name(&self, name: &ir::ExternalName) -> Result<Name, Error>;
}

impl<'a, 'm> Ctx<'a, 'm> for Cx<'a, 'm, Aarch64> {
    fn buf(&mut self) -> &mut Buf<'m> {
        self.buf
    }

    fn func(&self) -> &'a ir::Function {
        self.func
    }

    fn target(&self) -> &'a Target {
        self.target
    }

    fn frame(&self) -> &'a Frame {
        self.frame
    }

    fn loc(&self, value: ir::Value) -> Loc {
        Cx::loc(self, value)
    }

    fn returned(&self, inst: ir::Inst, k: usize) -> Loc {
        Cx::returned(self, inst, k)
    }

    fn payload(&self, inst: ir::Inst, k: usize) -> Loc {
        Cx::payload(self, inst, k)
    }

    fn edge(&self, inst: ir::Inst, i: usize) -> Label {
        Cx::edge(self, inst, i)
    }

    fn rule_at(&mut self, at: u32, rule: Rule) {
        Cx::rule_at(self, at, rule);
    }

    fn call_site(&mut self, ret: u32, inst: ir::Inst) {
        Cx::call_site(self, ret, inst);
    }

    fn name(&self, name: &ir::ExternalName) -> Result<Name, Error> {
        Cx::name(self, name)
    }
}

impl Glue for Aarch64 {
    const ARCH: blaze_codegen::Arch = blaze_codegen::Arch::Aarch64;
    /// `x20`.
    const FRAME: u8 = 20;
    /// `ldr`/`str` with an imm12 scaled by 8 and by 16.
    const NEAR: [u32; 2] = [4096 * 8, 4096 * 16];
    /// `nop`: `d503201f`.
    const NOP: &'static [u8] = &[0x1f, 0x20, 0x03, 0xd5];
    type Frame = Frame;

    fn frame(func: &ir::Function, target: &Target, area: Area) -> Result<Frame, Error> {
        let mut frame = frame::layout(func, target, area)?;
        frame.entry = glue::entry_bound(func, &frame);
        Ok(frame)
    }

    fn takes(func: &ir::Function, target: &Target, inst: ir::Inst) -> bool {
        let _ = target;
        glue::takes(func.dfg.insts[inst].opcode())
    }

    fn bound(func: &ir::Function, target: &Target, inst: ir::Inst) -> u32 {
        glue::bound(func, target, inst)
    }

    fn bound_fixed(frame: &Frame) -> u32 {
        glue::bound_fixed(frame)
    }

    fn prologue(cx: &mut Cx<'_, '_, Aarch64>) -> Result<(), Error> {
        glue::prologue(cx)
    }

    fn inst(cx: &mut Cx<'_, '_, Aarch64>, inst: ir::Inst) -> Result<(), Error> {
        glue::inst(cx, inst)
    }

    fn landing(cx: &mut Cx<'_, '_, Aarch64>, inst: ir::Inst) -> Result<(), Error> {
        glue::landing(cx, inst)
    }

    fn trap(cx: &mut Cx<'_, '_, Aarch64>, code: ir::TrapCode) {
        let _ = code;
        cx.buf.put4(inst::UDF);
    }

    fn count_back_edge(cx: &mut Cx<'_, '_, Aarch64>) {
        glue::count(cx.buf, glue::BACK_EDGES);
    }

    fn relay(cx: &mut Cx<'_, '_, Aarch64>, from: u32, to: u32, bytes: u8) {
        glue::relay(cx.buf, from, to, bytes);
    }

    fn unwind(cx: &mut Cx<'_, '_, Aarch64>) -> Result<Option<code::UnwindKind>, Error> {
        let len = cx.buf.len();
        if cx.frame.windows {
            let layout = cx.frame;
            frame::windows_codes(layout, cx.windows_info());
            return Ok(Some(code::UnwindKind::WindowsArm64 { len }));
        }
        Ok(Some(code::UnwindKind::Dwarf { len }))
    }

    fn slot_base(frame: &Frame) -> (u16, i32) {
        frame::slot_base(frame)
    }

    fn frame_size(frame: &Frame) -> u32 {
        frame::SAVES + frame.stack
    }
}
