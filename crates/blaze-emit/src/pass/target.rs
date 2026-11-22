//! `target_intrinsic`: the aarch64 barriers and hints the library reaches by `link_name`
//! (THREADS.md D1; the set's home is blaze-build `intrinsics::target::Target`).
//!
//! Encodings (Arm ARM, A64 "System instructions", the CRm field at bits 8..12 for the
//! barriers and CRm:op2 at bits 5..12 for `hint`):
//!
//! - `isb #imm` = `0xd50330df | imm << 8`, imm 0..=15 (`SY` = 15: `isb sy` = `0xd5033fdf`);
//! - `dmb #imm` = `0xd50330bf | imm << 8`, imm 0..=15;
//! - `dsb #imm` = `0xd503309f | imm << 8`, imm 0..=15;
//! - `hint #imm` = `0xd503201f | imm << 5`, imm 0..=127 (`yield` = `hint #1` = `0xd503203f`).
//!
//! The immediate must be a constant (an `iconst`): it is a field of the instruction. A name
//! outside the four, a non-constant immediate or one past its field is refused by name.

use alloc::alloc::Allocator;

use blaze_lir::Op;

use super::{EmitError, Pass, unsupported};

/// The base word of each target intrinsic and the most its immediate may be, with its shift.
struct Encoding {
    base: u32,
    most: u64,
    shift: u32,
}

/// The encoding of link name `name`, if blaze has one.
fn encoding(name: &[u8]) -> Option<Encoding> {
    Some(match name {
        b"llvm.aarch64.isb" => Encoding { base: 0xd503_30df, most: 15, shift: 8 },
        b"llvm.aarch64.dmb" => Encoding { base: 0xd503_30bf, most: 15, shift: 8 },
        b"llvm.aarch64.dsb" => Encoding { base: 0xd503_309f, most: 15, shift: 8 },
        b"llvm.aarch64.hint" => Encoding { base: 0xd503_201f, most: 127, shift: 5 },
        _ => return None,
    })
}

/// The word of the target intrinsic named `name` with immediate `imm`.
fn word(name: &[u8], imm: u64) -> Result<u32, &'static str> {
    let Some(e) = encoding(name) else { return Err("a target intrinsic blaze has no encoding for") };
    if imm > e.most {
        return Err("a target intrinsic's immediate past its field (4 bits for `isb`, `dmb`, `dsb`; 7 for `hint`)");
    }
    // `imm <= most <= 127`: the cast keeps every bit.
    Ok(e.base | (imm as u32) << e.shift)
}

impl<A: Allocator> Pass<'_, '_, A> {
    /// Target intrinsic `inst`: its one operand an `iconst`, encoded into the instruction.
    pub(super) fn target_intrinsic(&mut self, inst: usize, op: Op) -> Result<(), EmitError> {
        let mut o = self.view.operands(inst);
        let name = self.view.intrinsic_name(o.intrinsic());
        let args = o.values();
        if args.len() != 1 {
            return Err(unsupported(inst, op, "a target intrinsic of other than one immediate operand"));
        }
        let Some(imm) = self.constant_of(args.get(0)) else {
            return Err(unsupported(inst, op, "an immediate that is not a constant"));
        };
        let word = word(name, imm).map_err(|why| unsupported(inst, op, why))?;
        self.put(word);
        Ok(())
    }
}
