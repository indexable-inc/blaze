//! The variable arguments of a C-variadic call (a signature with `SigRec::VARIADIC`: its
//! `params` are the declared ones, and the call's values past them are the `...` ones).
//!
//! Where they go is the platform's rule, not a choice:
//! - Linux on arm64 (AAPCS64, "Procedure Call Standard for the Arm 64-bit Architecture",
//!   "Parameter passing", stage C, applied to every argument in order, named or variadic):
//!   a variadic argument takes the next `x` register (an integer or a pointer) or the next `v`
//!   register (a float) as a named one would; past `x7`/`v7` it goes to the next stacked
//!   argument address (the NSAA, starting at `sp + 0` of the caller at the call), "rounded up to
//!   the larger of 8 or the Natural Alignment of the argument's type", an argument smaller than
//!   8 bytes taking 8 ("as if the argument was copied to the least significant bits of a 64-bit
//!   register"): one 8-byte slot each for the types built here.
//! - Apple arm64 ("Writing ARM64 code for Apple platforms", "Update code that passes arguments
//!   to variadic functions"): the caller places every variadic argument on the stack, each in
//!   an 8-byte-aligned slot from `sp + 0` up, floats included; no register carries one. (The
//!   named arguments are placed as the generic standard says.)
//!
//! (The quotes are as the first writer took them from the two documents; this redo had no
//! network to fetch them again.)
//!
//! The order at the call (the record's call site is the fixed shape `adrp x16; ldr x16; blr
//! x16` with nothing between: pass/sites.rs, `import_callee`; record_tests.rs checks it):
//!
//! 1. the named arguments into their registers (pass.rs, `call`);
//! 2. [`Pass::place_varargs`]: the register-placed variable arguments (Linux) loaded, then, if
//!    any is stacked, `sub sp, sp, #area` (`area` the slots rounded up to 16: AAPCS64 keeps
//!    `sp` 16-aligned at a public interface) and each slot stored through `x9` from its value's
//!    cell, whose `sp` offset is `area` higher while the area is open;
//! 3. the callee's address into `x16` (`call`; an indirect callee's cell read `area` higher),
//!    and `blr x16`: in the record form the call site, untouched;
//! 4. [`Pass::close_varargs`]: `add sp, sp, #area`, before the results are stored.
//!
//! Only `x9` (and, for a cell past the scaled reach, the far-address register `x16`, pass.rs
//! `FAR`, which the site then loads afresh) is written between the named arguments and the
//! site: no value is left in `x16` or `x17`, the site's and the linker's scratch. The
//! frame's `fp` is unchanged, so the frame record and the compact unwind's frame form stay true
//! across the call. A landing pad would be entered with `sp` still lowered, so an `invoke` with
//! stacked variable arguments is refused by name: a "C" callee may not unwind (the Reference,
//! "Behavior considered undefined": "unwinding past a stack frame that does not allow
//! unwinding (e.g. by calling a "C-unwind" function imported or transmuted as a "C" function or
//! function pointer)"), so the builder calls it with `call`.
//!
//! What C's default argument promotions remove never reaches here: the checker refuses an
//! `f32`, a `bool` and an integer narrower than `c_int` passed to `...` (E0617), so a variable
//! argument narrower than 32 bits is refused by name as a broken invariant of the input.

use alloc::alloc::Allocator;

use blaze_lir::op::Values;
use blaze_lir::{Op, Scalar, Ty};

use super::{EmitError, Loc, Pass, Regs, unsupported};
use crate::Os;
use crate::enc::{Size, X9};

/// Why a function whose OWN signature is variadic is not emitted (pass.rs, `emit`): a C-variadic
/// definition in Rust (`unsafe extern "C" fn f(x: i32, mut args: ...)`, the unstable
/// `c_variadic`) needs its `VaListImpl` laid out per platform (Apple: one pointer into the
/// stacked slots; Linux: AAPCS64's register save areas and five-field `va_list`), which no
/// rung builds yet. Calling a variadic function is built; defining one is refused by this name.
pub(crate) const VARIADIC_DEFINITION: &str =
    "a C-variadic definition (`...` in the function's own signature: its `va_list` and register save area are not built)";

/// The most stacked variable arguments one call takes: the area must fit one `sub`'s 12-bit
/// immediate as a multiple of 16 (4080 bytes).
pub(crate) const MOST_STACKED: u32 = 510;

/// Where one variable argument goes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum VarLoc {
    /// A register, as a named argument would take it (Linux, up to `x7`/`v7`).
    Reg(Loc),
    /// The 8-byte slot at `sp + offset` of the call's outgoing area (Apple always; Linux past
    /// the registers).
    Slot(u32),
}

/// Why a variable argument has no place.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum VarRefusal {
    /// Narrower than 32 bits, or an `f32`: C promotes it, Rust refuses it unpromoted (E0617).
    Unpromoted,
    /// Not an integer, a pointer or an `f64`.
    NotScalar,
    /// An `i128` (C's `__int128`), whose variadic slot rules differ (16-byte aligned on Apple,
    /// an even register pair on Linux): no rung asks for it.
    Wide,
    /// More than [`MOST_STACKED`] stacked.
    TooMany,
}

impl VarRefusal {
    pub(crate) const fn why(self) -> &'static str {
        match self {
            VarRefusal::Unpromoted => "a variable argument narrower than 32 bits or an `f32` (C's default promotions; the checker refuses it unpromoted, E0617)",
            VarRefusal::NotScalar => "a variable argument that is not an integer, a pointer or an `f64`",
            VarRefusal::Wide => "a 128-bit variable argument (`__int128` passed to `...`)",
            VarRefusal::TooMany => "more than 510 stacked variable arguments (one `sub` immediate)",
        }
    }
}

/// The class of one variable argument of type `ty`, or why it has none.
fn var_class(ty: Ty) -> Result<Scalar, VarRefusal> {
    if !ty.is_scalar() {
        return Err(VarRefusal::NotScalar);
    }
    match ty.elem() {
        s @ (Scalar::I32 | Scalar::I64 | Scalar::Ptr | Scalar::F64) => Ok(s),
        Scalar::I1 | Scalar::I8 | Scalar::I16 | Scalar::F32 => Err(VarRefusal::Unpromoted),
        Scalar::I128 => Err(VarRefusal::Wide),
        _ => Err(VarRefusal::NotScalar),
    }
}

/// The outgoing area for `slots` stacked variable arguments: 8 bytes each, rounded up to 16.
pub(crate) const fn area_of(slots: u32) -> u32 {
    (8 * slots).next_multiple_of(16)
}

/// The place of the next variable argument, of type `ty`, on `os`: `regs` is the register walk
/// after the named arguments (advanced on Linux), `slots` the slots taken so far (advanced when
/// this one is stacked). A pure function of its inputs: the emitter and the tests share it.
pub(crate) fn place_of(os: Os, regs: &mut Regs, slots: &mut u32, ty: Ty) -> Result<VarLoc, VarRefusal> {
    var_class(ty)?;
    if os == Os::Linux {
        match regs.next(ty) {
            Some(Loc::Pair(_)) => return Err(VarRefusal::Wide),
            Some(at) => return Ok(VarLoc::Reg(at)),
            // Past `x7` (or `v7`): the NSAA, as a named argument there would go.
            None => {}
        }
    }
    if *slots >= MOST_STACKED {
        return Err(VarRefusal::TooMany);
    }
    let at = 8 * *slots;
    *slots += 1;
    Ok(VarLoc::Slot(at))
}

impl<A: Allocator> Pass<'_, '_, A> {
    /// Places the variable arguments `extra` of the call at `inst` after the named ones walked
    /// `regs`: the register-placed ones loaded first (every cell at its own offset), then the
    /// outgoing area opened and each slot stored. Call it after the named arguments and BEFORE
    /// the callee's address is loaded (the call site follows, unbroken); every `sp`-relative
    /// access after it is the returned `area` off until [`Pass::close_varargs`], called right
    /// after `blr`. Writes only `x9` (and `FAR` for a far cell) besides the argument registers.
    ///
    /// `named` are the named arguments past the registers (pass/stack.rs), taking the area's
    /// first `named_end` bytes; the stacked variable arguments follow from there rounded up to
    /// 8, and the named ones are stored first once the area is open.
    pub(super) fn place_varargs(
        &mut self,
        inst: usize,
        op: Op,
        regs: Regs,
        extra: Values<'_>,
        named: &[super::stack::StackedArg],
        named_end: u32,
    ) -> Result<u32, EmitError> {
        let view = self.view;
        let os = self.isa.os;
        // Every place is known before the first instruction: a refusal leaves no half call.
        // (`Regs` is `Copy`: each walk starts from the named arguments' end.)
        let (mut walk, mut slots) = (regs, 0u32);
        for value in extra.iter() {
            place_of(os, &mut walk, &mut slots, view.ty(value)).map_err(|why| unsupported(inst, op, why.why()))?;
        }
        if slots != 0 && matches!(op, Op::Invoke | Op::InvokeIndirect) {
            return Err(unsupported(inst, op, "an `invoke` with variable arguments on the stack (a \"C\" callee is called with `call`)"));
        }
        let base = named_end.next_multiple_of(8);
        let area = (base + area_of(slots)).next_multiple_of(16);
        if area >= super::stack::MOST_AREA || view.len() as u64 * 16 + u64::from(area) >= super::FRAME_LIMIT {
            return Err(EmitError::TooLarge { what: "a call's stacked arguments (1 MiB, and every cell below 16 MiB while they are open)" });
        }
        // The registers, while every cell is still at its own offset.
        let (mut walk, mut slots) = (regs, 0u32);
        for value in extra.iter() {
            let ty = view.ty(value);
            match place_of(os, &mut walk, &mut slots, ty).map_err(|why| unsupported(inst, op, why.why()))? {
                VarLoc::Reg(Loc::X(gpr)) => self.ld(gpr, value),
                VarLoc::Reg(Loc::V(v, w)) => self.v_out(v, value, w),
                VarLoc::Reg(Loc::Pair(_)) => return Err(unsupported(inst, op, VarRefusal::Wide.why())),
                VarLoc::Slot(_) => {}
            }
        }
        if area == 0 {
            return Ok(0);
        }
        // `area` a multiple of 16 below 1 MiB: `sp` stays 16-aligned at the `blr`.
        self.open_area(area);
        self.store_stacked(named, area);
        let (mut walk, mut slots) = (regs, 0u32);
        for value in extra.iter() {
            if let VarLoc::Slot(offset) =
                place_of(os, &mut walk, &mut slots, view.ty(value)).map_err(|why| unsupported(inst, op, why.why()))?
            {
                // The cell's offset moved up by the open area. The cell holds the value
                // zero-extended to 64 bits (lib.rs, "Values"); the callee's `va_arg` of an `int`
                // reads the slot's low 4 bytes (little-endian), of a `long`, a pointer or a
                // `double` all 8.
                self.ld_sp(X9, value.0 * 16 + area);
                self.st_sp_sized(Size::X, X9, base + offset);
            }
        }
        Ok(area)
    }

    /// Closes the outgoing area [`Pass::place_varargs`] opened: `add sp, sp, #area`.
    pub(super) fn close_varargs(&mut self, area: u32) {
        self.close_area(area);
    }
}
