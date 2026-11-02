//! Where System V, Windows x64 and `tail` put each argument and return value: Cranelift
//! 0.137's `X64ABIMachineSpec::compute_arg_locs`, `get_intreg_for_arg`, `get_fltreg_for_arg`,
//! `get_intreg_for_retval`, `get_fltreg_for_retval` and `get_ext_mode` (`isa/x64/abi.rs`),
//! with `enable_llvm_abi_extensions` set as `hir-ty-codegen/target.rs` sets it, and
//! `SigSet::from_func_sig`'s handling of `StructReturn` (`machinst/abi.rs`): the conventions
//! the IR builder makes x86-64 code in (`system_v`, `windows_fastcall`), and `tail`, which
//! `return_call` needs. The assignment is a walk over the signature with nothing allocated.

use cranelift_codegen::{
    ir::{self, ArgumentExtension, ArgumentPurpose, types},
    isa::CallConv,
};

use super::asm::{R8, R9, R10, RAX, RCX, RDI, RDX, RSI};
use crate::Error;

/// One 8-byte part of an argument: in a general register or on the stack, this many bytes
/// above the stack pointer at the call.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Part {
    Gpr(u8),
    Stack(u32),
}

/// Where one argument or return value is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Place {
    /// An integer of at most 64 bits, or one 8-byte half of nothing else.
    Gpr(u8),
    /// A float or vector.
    Xmm(u8),
    /// A value other than an `i128` on the stack, this many bytes above the stack pointer:
    /// 8 bytes, or 16 for a value of 16.
    Stack(u32),
    /// An `i128`, the low half first.
    Pair(Part, Part),
    /// A `StructArgument` of `size` bytes, copied to the stack at `at`.
    Struct { at: u32, size: u32 },
    /// A Windows argument of 16 bytes (a 128-bit vector, an `f128`), passed as a pointer to
    /// a copy the caller makes on the stack at `copy`.
    Implicit { pointer: Part, copy: u32 },
}

/// Whether `conv` is one the glue passes values in, as an error naming it otherwise.
pub fn check(conv: CallConv) -> Result<(), Error> {
    match conv {
        CallConv::SystemV | CallConv::WindowsFastcall | CallConv::Tail => Ok(()),
        // alloc: the error's message.
        other => Err(Error::new(format!(
            "the {other} calling convention on x86-64: the glue passes values in `system_v`, \
             `windows_fastcall` and `tail` only"
        ))),
    }
}

/// Whether a value of `ty` goes in general registers (and how many) or in one XMM register:
/// `Inst::rc_for_type`.
fn classes(ty: ir::Type) -> Result<(bool, u8), Error> {
    match ty {
        types::I8 | types::I16 | types::I32 | types::I64 => Ok((true, 1)),
        types::I128 => Ok((true, 2)),
        types::F16 | types::F32 | types::F64 | types::F128 => Ok((false, 1)),
        ty if ty.is_vector() && ty.bits() <= 128 => Ok((false, 1)),
        // alloc: the error's message.
        ty => Err(Error::new(format!("a {ty} argument or return value on x86-64"))),
    }
}

fn int_arg(win: bool, gpr: usize, idx: usize) -> Option<u8> {
    if win {
        [RCX, RDX, R8, R9].get(idx).copied()
    } else {
        [RDI, RSI, RDX, RCX, R8, R9].get(gpr).copied()
    }
}

fn float_arg(win: bool, vreg: usize, idx: usize) -> Option<u8> {
    let (i, count) = if win { (idx, 4) } else { (vreg, 8) };
    (i < count).then_some(i as u8)
}

/// `tail` returns in eight of each (`get_intreg_for_retval`, `get_fltreg_for_retval`), its
/// second integer in `rcx`; `r11` is Cranelift's scratch.
fn int_ret(win: bool, tail: bool, gpr: usize) -> Option<u8> {
    let regs: &[u8] = match (win, tail) {
        (_, true) => &[RAX, RCX, RDX, RSI, RDI, R8, R9, R10],
        (true, false) => &[RAX, RDX],
        (false, false) => &[RAX, RDX, RCX],
    };
    regs.get(gpr).copied()
}

/// `xmm0` and `xmm1` on System V and Windows (Cranelift returns a Rust float pair in them on
/// Windows too), `xmm0` to `xmm7` in `tail`.
fn float_ret(tail: bool, vreg: usize) -> Option<u8> {
    (vreg < if tail { 8 } else { 2 }).then_some(vreg as u8)
}

/// Assigns places in the order of a signature's arguments or of its returns.
struct Assign {
    win: bool,
    /// `tail`: System V's arguments, its own return registers.
    tail: bool,
    rets: bool,
    gpr: usize,
    vreg: usize,
    /// Windows counts every argument, of either class.
    idx: usize,
    stack: u32,
    /// Where the next implicit copy goes, once the arguments' own stack is known.
    copy: u32,
}

impl Assign {
    fn new(conv: CallConv, rets: bool, copy: u32) -> Assign {
        let win = conv == CallConv::WindowsFastcall;
        // Windows' 32 bytes of shadow space for the first four arguments.
        let stack = if win && !rets { 32 } else { 0 };
        let tail = conv == CallConv::Tail;
        Assign { win, tail, rets, gpr: 0, vreg: 0, idx: 0, stack, copy }
    }

    fn int(&mut self) -> Option<u8> {
        if self.rets {
            int_ret(self.win, self.tail, self.gpr)
        } else {
            int_arg(self.win, self.gpr, self.idx)
        }
    }

    fn float(&mut self) -> Option<u8> {
        if self.rets {
            float_ret(self.tail, self.vreg)
        } else {
            float_arg(self.win, self.vreg, self.idx)
        }
    }

    fn too_many(&self) -> Result<(), Error> {
        if self.rets {
            return Err(Error::new(
                "too many return values to fit in registers: Cranelift asks for a StructReturn \
                 argument instead",
            ));
        }
        Ok(())
    }

    /// One 8-byte general part, as the generic loop of `compute_arg_locs` places it.
    fn part(&mut self) -> Result<Part, Error> {
        let reg = self.int();
        self.idx += 1;
        if let Some(reg) = reg {
            self.gpr += 1;
            return Ok(Part::Gpr(reg));
        }
        self.too_many()?;
        self.stack = self.stack.next_multiple_of(8);
        let at = self.stack;
        self.stack += 8;
        Ok(Part::Stack(at))
    }

    fn next(&mut self, param: &ir::AbiParam) -> Result<Place, Error> {
        let ty = param.value_type;
        if let ArgumentPurpose::StructArgument(size) = param.purpose {
            if self.rets {
                return Err(Error::new("a StructArgument return value"));
            }
            if size % 8 != 0 {
                // alloc: the error's message.
                return Err(Error::new(format!(
                    "a StructArgument of {size} bytes, which Cranelift requires to be a multiple of 8"
                )));
            }
            let at = self.stack;
            self.stack += size;
            return Ok(Place::Struct { at, size });
        }
        let (int, regs) = classes(ty)?;
        if self.win && !self.rets && !int && ty.bits() >= 128 {
            let pointer = match int_arg(true, self.gpr, self.idx) {
                Some(reg) => {
                    self.gpr += 1;
                    Part::Gpr(reg)
                }
                None => {
                    self.stack = self.stack.next_multiple_of(8) + 8;
                    Part::Stack(self.stack - 8)
                }
            };
            self.idx += 1;
            self.copy = self.copy.next_multiple_of(16);
            let copy = self.copy;
            self.copy += 16;
            return Ok(Place::Implicit { pointer, copy });
        }
        if regs == 2 {
            if !self.win && !self.rets {
                // System V passes an `i128` in two registers or on the stack, 16-aligned, and
                // uses up two registers either way.
                let place = match (int_arg(false, self.gpr, 0), int_arg(false, self.gpr + 1, 0)) {
                    (Some(lo), Some(hi)) => Place::Pair(Part::Gpr(lo), Part::Gpr(hi)),
                    _ => {
                        self.stack = self.stack.next_multiple_of(16);
                        let at = self.stack;
                        self.stack += 16;
                        Place::Pair(Part::Stack(at), Part::Stack(at + 8))
                    }
                };
                self.gpr += 2;
                self.idx += 2;
                return Ok(place);
            }
            let lo = self.part()?;
            let hi = self.part()?;
            return Ok(Place::Pair(lo, hi));
        }
        if int {
            return Ok(match self.part()? {
                Part::Gpr(reg) => Place::Gpr(reg),
                Part::Stack(at) => Place::Stack(at),
            });
        }
        let reg = self.float();
        self.idx += 1;
        if let Some(reg) = reg {
            self.vreg += 1;
            return Ok(Place::Xmm(reg));
        }
        self.too_many()?;
        let size = ty.bytes().max(8);
        self.stack = self.stack.next_multiple_of(size);
        let at = self.stack;
        self.stack += size;
        Ok(Place::Stack(at))
    }
}

/// The most bytes of stack arguments Cranelift takes (`STACK_ARG_RET_SIZE_LIMIT`).
const STACK_LIMIT: u32 = 128 * 1024 * 1024;

/// Calls `f` with each argument of `sig` and its place, in order; gives the bytes of the
/// stack argument area (with Windows' shadow space and implicit copies), 16-aligned.
pub fn args(
    sig: &ir::Signature,
    mut f: impl FnMut(usize, &ir::AbiParam, Place) -> Result<(), Error>,
) -> Result<u32, Error> {
    check(sig.call_conv)?;
    // The implicit copies go after every argument's own stack: one walk finds where that
    // ends, the second places them.
    let mut first = Assign::new(sig.call_conv, false, 0);
    for param in &sig.params {
        first.next(param)?;
    }
    let mut assign = Assign::new(sig.call_conv, false, first.stack);
    for (i, param) in sig.params.iter().enumerate() {
        let place = assign.next(param)?;
        f(i, param, place)?;
    }
    let size = assign.stack.max(assign.copy).next_multiple_of(16);
    if size > STACK_LIMIT {
        return Err(Error::new("more than 128 MB of stack arguments"));
    }
    Ok(size)
}

/// The bytes of `sig`'s stack argument area.
pub fn stack_args(sig: &ir::Signature) -> Result<u32, Error> {
    args(sig, |_, _, _| Ok(()))
}

/// Calls `f` with each return value of `sig` and its place (a register, or a pair of them),
/// in order. A signature with a `StructReturn` argument returns nothing but that pointer, in
/// `rax`; `f` is not given it (`k` counts `sig.returns`), and a caller ignores it, as
/// Cranelift's does.
pub fn rets(
    sig: &ir::Signature,
    mut f: impl FnMut(usize, &ir::AbiParam, Place) -> Result<(), Error>,
) -> Result<(), Error> {
    check(sig.call_conv)?;
    if sig.uses_special_return(ArgumentPurpose::StructReturn) {
        return Err(Error::new("an explicit StructReturn return value"));
    }
    if sig.uses_special_param(ArgumentPurpose::StructReturn) && !sig.returns.is_empty() {
        return Err(Error::new("return values beside a StructReturn argument"));
    }
    let mut assign = Assign::new(sig.call_conv, true, 0);
    for (k, param) in sig.returns.iter().enumerate() {
        let place = assign.next(param)?;
        f(k, param, place)?;
    }
    Ok(())
}

/// The extension a caller applies to an argument (or a callee to a return value), in a
/// register or on the stack alike: x64's `get_ext_mode` keeps the signature's, to 64 bits.
pub fn extension(param: &ir::AbiParam) -> Option<bool> {
    if param.value_type.bits() >= 64 {
        return None;
    }
    match param.extension {
        ArgumentExtension::None => None,
        ArgumentExtension::Uext => Some(false),
        ArgumentExtension::Sext => Some(true),
    }
}
