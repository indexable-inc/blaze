//! Where AAPCS64 and Apple's variant put each argument and return value: Cranelift 0.136.2's
//! `AArch64MachineDeps::compute_arg_locs` and `get_ext_mode` (`isa/aarch64/abi.rs`), with
//! `SigSet::from_func_sig`'s handling of `StructReturn` (`machinst/abi.rs`), for the two
//! conventions the IR builder makes AArch64 code in (`SystemV`, `AppleAarch64`). The
//! assignment is a walk over the signature with nothing allocated.

use cranelift_codegen::{
    ir::{self, ArgumentExtension, ArgumentPurpose, types},
    isa::CallConv,
};

use crate::Error;

/// A register an argument or return value is passed in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Reg {
    /// `x0` to `x8`.
    X(u8),
    /// `v0` to `v7`.
    V(u8),
}

/// Where one argument or return value is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Place {
    /// In one register, `ty` its type.
    Reg(Reg, ir::Type),
    /// An `i128` in two X registers, the low half in the first.
    Pair(u8, u8),
    /// On the stack, this many bytes above the stack pointer at the call: `ty` bytes of it
    /// (an `i128` as two `i64`s, the low one first).
    Stack(u32, ir::Type),
}

/// Whether `conv` is one the glue passes values in, as an error naming it otherwise.
/// Windows' (`windows_fastcall`, AArch64 Windows triples' default) is AAPCS64 to Cranelift.
pub(super) fn check(conv: CallConv) -> Result<(), Error> {
    match conv {
        CallConv::SystemV
        | CallConv::AppleAarch64
        | CallConv::WindowsFastcall
        | CallConv::Tail => Ok(()),
        // alloc: the error's message, once, as the function fails to compile.
        other => Err(Error::new(format!(
            "the {other} calling convention on AArch64: the glue passes values in `system_v`, \
             `apple_aarch64`, `windows_fastcall` and `tail` only"
        ))),
    }
}

/// Whether a `try_call` in `conv` has exception payload registers (`x0`, `x1`): Cranelift's
/// `exception_payload_regs` gives Windows' none, and panics on its exception clobbers.
pub(super) fn catches(conv: CallConv) -> bool {
    matches!(conv, CallConv::SystemV | CallConv::AppleAarch64 | CallConv::Tail)
}

/// Whether a value of `ty` goes in X registers (and how many) or in one V register.
fn classes(ty: ir::Type) -> Result<(bool, u8), Error> {
    match ty {
        types::I8 | types::I16 | types::I32 | types::I64 => Ok((true, 1)),
        types::I128 => Ok((true, 2)),
        types::F16 | types::F32 | types::F64 | types::F128 => Ok((false, 1)),
        ty if ty.is_vector() && ty.bits() <= 128 => Ok((false, 1)),
        // alloc: the error's message, once, as the function fails to compile.
        ty => Err(Error::new(format!("a {ty} argument or return value on AArch64"))),
    }
}

/// Assigns places in the order of a signature's arguments or of its returns.
struct Assign {
    apple: bool,
    tail: bool,
    rets: bool,
    next_x: u8,
    next_v: u8,
    next_stack: u32,
    remaining: u8,
}

impl Assign {
    fn new(conv: CallConv, rets: bool) -> Assign {
        let tail = conv == CallConv::Tail;
        Assign {
            apple: conv == CallConv::AppleAarch64,
            tail,
            rets,
            // `tail` keeps `x0` for a return area pointer and `x1` for an indirect callee, in
            // its arguments and its returns alike.
            next_x: if tail { 2 } else { 0 },
            next_v: 0,
            next_stack: 0,
            remaining: 16,
        }
    }

    fn next(&mut self, param: &ir::AbiParam) -> Result<Place, Error> {
        let ty = param.value_type;
        match param.purpose {
            ArgumentPurpose::StructReturn if self.tail => {
                return Err(Error::new(
                    "a StructReturn parameter of a `tail` function, which Cranelift's AArch64 \
                     backend does not pass",
                ));
            }
            // Cranelift gives it `x8` and counts it in no class.
            ArgumentPurpose::StructReturn => {
                if ty != types::I64 {
                    return Err(Error::new("a StructReturn pointer that is not an i64"));
                }
                return Ok(Place::Reg(Reg::X(8), ty));
            }
            ArgumentPurpose::StructArgument(_) => {
                return Err(Error::new(
                    "a StructArgument parameter on AArch64, which Cranelift does not pass either",
                ));
            }
            _ => {}
        }
        let (int, regs) = classes(ty)?;
        if regs == 2 {
            // AAPCS64 starts a 16-byte-aligned pair at an even register (stage C.8); Apple's
            // does not.
            if self.remaining >= 2 && 8 - self.next_x.min(8) >= 2 {
                if !self.apple && self.next_x % 2 != 0 {
                    self.next_x += 1;
                }
                let lo = self.next_x;
                self.next_x += 2;
                self.remaining -= 2;
                return Ok(Place::Pair(lo, lo + 1));
            }
        } else {
            let next = if int { &mut self.next_x } else { &mut self.next_v };
            if *next < 8 && self.remaining > 0 {
                let reg = if int { Reg::X(*next) } else { Reg::V(*next) };
                *next += 1;
                self.remaining -= 1;
                return Ok(Place::Reg(reg, ty));
            }
        }
        if self.rets {
            return Err(Error::new(
                "too many return values to fit in registers: Cranelift asks for a StructReturn \
                 argument instead",
            ));
        }
        let size = ty.bytes();
        // Apple's stack arguments take their natural size; AAPCS64's at least 8 bytes.
        let size = if self.apple { size } else { size.max(8) };
        self.next_stack = self.next_stack.next_multiple_of(size);
        let at = self.next_stack;
        self.next_stack += size;
        Ok(Place::Stack(at, ty))
    }
}

/// Calls `f` with each argument of `sig` and its place, in order; gives the bytes of the
/// stack argument area, 16-aligned.
pub(super) fn args(
    sig: &ir::Signature,
    mut f: impl FnMut(usize, &ir::AbiParam, Place) -> Result<(), Error>,
) -> Result<u32, Error> {
    check(sig.call_conv)?;
    let mut assign = Assign::new(sig.call_conv, false);
    for (i, param) in sig.params.iter().enumerate() {
        let place = assign.next(param)?;
        f(i, param, place)?;
    }
    let size = assign.next_stack.next_multiple_of(16);
    if size > 128 * 1024 * 1024 {
        return Err(Error::new("more than 128 MB of stack arguments"));
    }
    Ok(size)
}

/// The bytes of `sig`'s stack argument area.
pub(super) fn stack_args(sig: &ir::Signature) -> Result<u32, Error> {
    args(sig, |_, _, _| Ok(()))
}

/// Calls `f` with each return value of `sig` and its register, in order. A signature with a
/// `StructReturn` argument returns nothing but that pointer, in `x8`; `f` is not given it
/// (`k` counts `sig.returns`), and a caller ignores it, as Cranelift's does.
pub(super) fn rets(
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
    let mut assign = Assign::new(sig.call_conv, true);
    for (k, param) in sig.returns.iter().enumerate() {
        let place = assign.next(param)?;
        f(k, param, place)?;
    }
    Ok(())
}

/// The extension a caller applies to an argument (or a callee to a return value) at
/// `place`: Apple's convention extends what is in a register to 64 bits as the signature
/// says, AAPCS64 leaves the high bits unspecified, and neither extends what is on the stack.
pub(super) fn extension(conv: CallConv, param: &ir::AbiParam, place: Place) -> Option<bool> {
    let in_reg = matches!(place, Place::Reg(Reg::X(_), _));
    if conv != CallConv::AppleAarch64 || !in_reg || param.value_type.bits() >= 64 {
        return None;
    }
    match param.extension {
        ArgumentExtension::None => None,
        ArgumentExtension::Uext => Some(false),
        ArgumentExtension::Sext => Some(true),
    }
}
