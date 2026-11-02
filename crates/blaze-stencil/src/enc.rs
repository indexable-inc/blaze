//! How a hole's field is encoded, and patching one: the relocation forms LLVM leaves in
//! stencils and the glue writes, for both architectures. Shared with the generator
//! (`build.rs` includes this file with `#[path]`); it uses nothing but `std`.
//!
//! A value is written as ELF computes a relocation's: `S + A` for an absolute form, and
//! `S + A - P` for a PC-relative one, `P` being the field's offset (x86-64) or the
//! instruction's (AArch64, whose fields are in 4-byte instructions). The patcher computes
//! that value and [`write`] puts it in the field, refusing one that does not fit.

use std::fmt;

/// The encoding of a hole's or a fixup's field.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
#[repr(u8)]
pub enum Form {
    /// 1 byte, the value as an unsigned or signed 8-bit integer (`R_X86_64_8`, from an
    /// `!absolute_symbol` range below 256).
    Abs8,
    /// 4 bytes, zero-extended (`R_X86_64_32`).
    Abs32,
    /// 4 bytes, sign-extended (`R_X86_64_32S`): displacements and immediates.
    Abs32S,
    /// 8 bytes (`R_X86_64_64`, `movabs`).
    Abs64,
    /// 4 bytes, `S + A - P` as a signed 32-bit integer (`R_X86_64_PC32`, `PLT32`): x86-64
    /// `jmp`/`jcc`/RIP-relative operands, and the entries of a jump table on both.
    Rel32,
    /// AArch64 `movz`/`movk` imm16, bits 0..16 of the value, checked below 2^16
    /// (`R_AARCH64_MOVW_UABS_G0`).
    A64MovwG0,
    /// Bits 0..16, unchecked (`MOVW_UABS_G0_NC`).
    A64MovwG0Nc,
    /// Bits 16..32, checked below 2^32 (`MOVW_UABS_G1`).
    A64MovwG1,
    A64MovwG1Nc,
    /// Bits 32..48, checked below 2^48 (`MOVW_UABS_G2`).
    A64MovwG2,
    A64MovwG2Nc,
    /// Bits 48..64 (`MOVW_UABS_G3`).
    A64MovwG3,
    /// AArch64 `add` imm12 of an absolute value, checked below 4096 (on an absolute value the
    /// whole value must fit, unlike ELF's `_NC` page offset).
    A64AddLo12,
    /// AArch64 `ldr`/`str` imm12 of an absolute value scaled by the access size (1, 2, 4,
    /// 8, 16 bytes): checked to be a multiple of it and below 4096 times it.
    A64Ldst8Lo12,
    A64Ldst16Lo12,
    A64Ldst32Lo12,
    A64Ldst64Lo12,
    A64Ldst128Lo12,
    /// AArch64 `b`/`bl` imm26, `(S + A - P) / 4` (`JUMP26`, `CALL26`): ±128 MB.
    A64Jump26,
    /// AArch64 `b.cond`/`cbz`/`cbnz` imm19 (`CONDBR19`): ±1 MB.
    A64CondBr19,
    /// AArch64 `tbz`/`tbnz` imm14 (`TSTBR14`): ±32 KB.
    A64TestBr14,
    /// AArch64 `adr` immlo:immhi (`ADR_PREL_LO21`): ±1 MB.
    A64AdrLo21,
    /// AArch64 `ldr` (literal) imm19 (`LD_PREL_LO19`): ±1 MB, word-aligned.
    A64LdrLit19,
}

impl Form {
    pub const ALL: [Form; 23] = [
        Form::Abs8,
        Form::Abs32,
        Form::Abs32S,
        Form::Abs64,
        Form::Rel32,
        Form::A64MovwG0,
        Form::A64MovwG0Nc,
        Form::A64MovwG1,
        Form::A64MovwG1Nc,
        Form::A64MovwG2,
        Form::A64MovwG2Nc,
        Form::A64MovwG3,
        Form::A64AddLo12,
        Form::A64Ldst8Lo12,
        Form::A64Ldst16Lo12,
        Form::A64Ldst32Lo12,
        Form::A64Ldst64Lo12,
        Form::A64Ldst128Lo12,
        Form::A64Jump26,
        Form::A64CondBr19,
        Form::A64TestBr14,
        Form::A64AdrLo21,
        Form::A64LdrLit19,
    ];

    pub const fn from_u8(it: u8) -> Option<Form> {
        if (it as usize) < Form::ALL.len() { Some(Form::ALL[it as usize]) } else { None }
    }

    /// Whether the value is `S + A - P`.
    pub const fn pc_relative(self) -> bool {
        matches!(
            self,
            Form::Rel32
                | Form::A64Jump26
                | Form::A64CondBr19
                | Form::A64TestBr14
                | Form::A64AdrLo21
                | Form::A64LdrLit19
        )
    }

    /// Bytes of code the field is in, from its offset: the field itself on x86-64, the
    /// instruction on AArch64.
    pub const fn bytes(self) -> u8 {
        match self {
            Form::Abs8 => 1,
            Form::Abs64 => 8,
            _ => 4,
        }
    }
}

/// A value that does not fit the field of its form.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Overflow {
    pub form: Form,
    pub value: i64,
}

impl fmt::Display for Overflow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} does not fit a {:?} field", self.value, self.form)
    }
}

/// Writes `value` into the field of `form` at `code[at..]`, keeping the instruction's other
/// bits. `code[at..at + form.bytes()]` must be in bounds (the library's holes are checked
/// against their stencil's length when it is loaded).
#[inline]
pub fn write(form: Form, code: &mut [u8], at: usize, value: i64) -> Result<(), Overflow> {
    let overflow = Overflow { form, value };
    match form {
        Form::Abs8 => {
            if !(-128..=255).contains(&value) {
                return Err(overflow);
            }
            code[at] = value as u8;
        }
        Form::Abs32 => {
            if !(0..=i64::from(u32::MAX)).contains(&value) {
                return Err(overflow);
            }
            put4(code, at, value as u32);
        }
        Form::Abs32S | Form::Rel32 => {
            if i32::try_from(value).is_err() {
                return Err(overflow);
            }
            put4(code, at, value as i32 as u32);
        }
        Form::Abs64 => code[at..at + 8].copy_from_slice(&value.to_le_bytes()),
        _ => {
            let insn = a64(form, get4(code, at), value).ok_or(overflow)?;
            put4(code, at, insn);
        }
    }
    Ok(())
}

/// Whether `value` fits the field of `form`: what [`write`] would refuse, without code.
#[inline]
pub fn check(form: Form, value: i64) -> Result<(), Overflow> {
    let mut field = [0u8; 8];
    write(form, &mut field, 0, value)
}

/// The value the field of `form` at `code[at..]` holds, as [`write`] would have been given
/// it (the inverse, for tests and the generator's checks). For a scaled or partial form it is
/// the part the field holds, shifted back into place. `Abs8` and `Abs32` read zero-extended.
pub fn read(form: Form, code: &[u8], at: usize) -> i64 {
    let insn = || get4(code, at);
    let movw = |shift: u32| (u64::from((insn() >> 5) & 0xffff) << shift) as i64;
    let imm12 = || i64::from((insn() >> 10) & 0xfff);
    let signed = |bits: u32, shift: u32| {
        let field = (insn() >> shift) & ((1 << bits) - 1);
        sign_extend(i64::from(field), bits) << 2
    };
    match form {
        Form::Abs8 => i64::from(code[at]),
        Form::Abs32 => i64::from(get4(code, at)),
        Form::Abs32S | Form::Rel32 => i64::from(get4(code, at) as i32),
        Form::Abs64 => {
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(&code[at..at + 8]);
            i64::from_le_bytes(bytes)
        }
        Form::A64MovwG0 | Form::A64MovwG0Nc => movw(0),
        Form::A64MovwG1 | Form::A64MovwG1Nc => movw(16),
        Form::A64MovwG2 | Form::A64MovwG2Nc => movw(32),
        Form::A64MovwG3 => movw(48),
        Form::A64AddLo12 => imm12(),
        Form::A64Ldst8Lo12 => imm12(),
        Form::A64Ldst16Lo12 => imm12() * 2,
        Form::A64Ldst32Lo12 => imm12() * 4,
        Form::A64Ldst64Lo12 => imm12() * 8,
        Form::A64Ldst128Lo12 => imm12() * 16,
        Form::A64Jump26 => signed(26, 0),
        Form::A64CondBr19 | Form::A64LdrLit19 => signed(19, 5),
        Form::A64TestBr14 => signed(14, 5),
        Form::A64AdrLo21 => {
            let insn = insn();
            let lo = i64::from((insn >> 29) & 3);
            let hi = i64::from((insn >> 5) & 0x7ffff);
            sign_extend(hi << 2 | lo, 21)
        }
    }
}

/// `insn` with the field of the AArch64 `form` set to `value`, or `None` when it does not fit.
#[inline]
fn a64(form: Form, insn: u32, value: i64) -> Option<u32> {
    let unsigned = value as u64;
    let movw = |chunk: u64| (insn & !(0xffff << 5)) | (((chunk & 0xffff) as u32) << 5);
    let imm12 = |field: u64| (insn & !(0xfff << 10)) | ((field as u32) << 10);
    let scaled = |size: i64| {
        (value % size == 0 && (0..4096 * size).contains(&value)).then(|| imm12((value / size) as u64))
    };
    let branch = |bits: u32, shift: u32| {
        let words = value >> 2;
        let half = 1i64 << (bits - 1);
        (value & 3 == 0 && (-half..half).contains(&words)).then(|| {
            let mask = (1u32 << bits) - 1;
            (insn & !(mask << shift)) | (((words as u32) & mask) << shift)
        })
    };
    match form {
        Form::A64MovwG0 => (unsigned < 1 << 16).then(|| movw(unsigned)),
        Form::A64MovwG0Nc => Some(movw(unsigned)),
        Form::A64MovwG1 => (unsigned < 1 << 32).then(|| movw(unsigned >> 16)),
        Form::A64MovwG1Nc => Some(movw(unsigned >> 16)),
        Form::A64MovwG2 => (unsigned < 1 << 48).then(|| movw(unsigned >> 32)),
        Form::A64MovwG2Nc => Some(movw(unsigned >> 32)),
        Form::A64MovwG3 => Some(movw(unsigned >> 48)),
        Form::A64AddLo12 => (unsigned < 4096).then(|| imm12(unsigned)),
        Form::A64Ldst8Lo12 => scaled(1),
        Form::A64Ldst16Lo12 => scaled(2),
        Form::A64Ldst32Lo12 => scaled(4),
        Form::A64Ldst64Lo12 => scaled(8),
        Form::A64Ldst128Lo12 => scaled(16),
        Form::A64Jump26 => branch(26, 0),
        Form::A64CondBr19 | Form::A64LdrLit19 => branch(19, 5),
        Form::A64TestBr14 => branch(14, 5),
        Form::A64AdrLo21 => ((-(1i64 << 20))..(1i64 << 20)).contains(&value).then(|| {
            let lo = (value & 3) as u32;
            let hi = ((value >> 2) & 0x7ffff) as u32;
            (insn & !((3 << 29) | (0x7ffff << 5))) | (lo << 29) | (hi << 5)
        }),
        Form::Abs8 | Form::Abs32 | Form::Abs32S | Form::Abs64 | Form::Rel32 => None,
    }
}

const fn sign_extend(value: i64, bits: u32) -> i64 {
    let shift = 64 - bits;
    (value << shift) >> shift
}

#[inline]
fn get4(code: &[u8], at: usize) -> u32 {
    let mut bytes = [0u8; 4];
    bytes.copy_from_slice(&code[at..at + 4]);
    u32::from_le_bytes(bytes)
}

#[inline]
fn put4(code: &mut [u8], at: usize, it: u32) {
    code[at..at + 4].copy_from_slice(&it.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use blaze_ids::rng::SplitMix64;

    /// Each form's bounds: the least and greatest value it takes, the step its values come in,
    /// and what reading the field back gives for a value it took.
    struct Bounds {
        form: Form,
        min: i64,
        max: i64,
        step: i64,
        /// Whether there is a value past each bound (the unchecked forms take every value).
        checked: bool,
    }

    const fn b(form: Form, min: i64, max: i64, step: i64) -> Bounds {
        Bounds { form, min, max, step, checked: true }
    }

    const fn any(form: Form) -> Bounds {
        Bounds { form, min: i64::MIN, max: i64::MAX, step: 1, checked: false }
    }

    const BOUNDS: [Bounds; 23] = [
        b(Form::Abs8, -128, 255, 1),
        b(Form::Abs32, 0, u32::MAX as i64, 1),
        b(Form::Abs32S, i32::MIN as i64, i32::MAX as i64, 1),
        any(Form::Abs64),
        b(Form::Rel32, i32::MIN as i64, i32::MAX as i64, 1),
        b(Form::A64MovwG0, 0, (1 << 16) - 1, 1),
        any(Form::A64MovwG0Nc),
        b(Form::A64MovwG1, 0, (1 << 32) - 1, 1),
        any(Form::A64MovwG1Nc),
        b(Form::A64MovwG2, 0, (1 << 48) - 1, 1),
        any(Form::A64MovwG2Nc),
        any(Form::A64MovwG3),
        b(Form::A64AddLo12, 0, 4095, 1),
        b(Form::A64Ldst8Lo12, 0, 4095, 1),
        b(Form::A64Ldst16Lo12, 0, 4095 * 2, 2),
        b(Form::A64Ldst32Lo12, 0, 4095 * 4, 4),
        b(Form::A64Ldst64Lo12, 0, 4095 * 8, 8),
        b(Form::A64Ldst128Lo12, 0, 4095 * 16, 16),
        b(Form::A64Jump26, -(1 << 27), (1 << 27) - 4, 4),
        b(Form::A64CondBr19, -(1 << 20), (1 << 20) - 4, 4),
        b(Form::A64TestBr14, -(1 << 15), (1 << 15) - 4, 4),
        b(Form::A64AdrLo21, -(1 << 20), (1 << 20) - 1, 1),
        b(Form::A64LdrLit19, -(1 << 20), (1 << 20) - 4, 4),
    ];

    /// What [`read`] gives for `value` written in `form`: the part of it the field holds.
    fn held(form: Form, value: i64) -> i64 {
        let bits = |lo: u32| ((value as u64 >> lo) & 0xffff) << lo;
        match form {
            Form::Abs8 => value & 0xff,
            Form::Abs32 => value & 0xffff_ffff,
            Form::A64MovwG0 | Form::A64MovwG0Nc => bits(0) as i64,
            Form::A64MovwG1 | Form::A64MovwG1Nc => bits(16) as i64,
            Form::A64MovwG2 | Form::A64MovwG2Nc => bits(32) as i64,
            Form::A64MovwG3 => bits(48) as i64,
            _ => value,
        }
    }

    /// The bits of the field in its 4-byte instruction.
    fn mask(form: Form) -> u32 {
        match form {
            Form::A64MovwG0
            | Form::A64MovwG0Nc
            | Form::A64MovwG1
            | Form::A64MovwG1Nc
            | Form::A64MovwG2
            | Form::A64MovwG2Nc
            | Form::A64MovwG3 => 0xffff << 5,
            Form::A64AddLo12
            | Form::A64Ldst8Lo12
            | Form::A64Ldst16Lo12
            | Form::A64Ldst32Lo12
            | Form::A64Ldst64Lo12
            | Form::A64Ldst128Lo12 => 0xfff << 10,
            Form::A64Jump26 => (1 << 26) - 1,
            Form::A64CondBr19 | Form::A64LdrLit19 => ((1 << 19) - 1) << 5,
            Form::A64TestBr14 => ((1 << 14) - 1) << 5,
            Form::A64AdrLo21 => (3 << 29) | (0x7ffff << 5),
            Form::Abs8 => 0xff,
            Form::Abs32 | Form::Abs32S | Form::Rel32 | Form::Abs64 => u32::MAX,
        }
    }

    fn round_trip(form: Form, value: i64) {
        for fill in [0x00u8, 0xff] {
            let mut code = [fill; 12];
            write(form, &mut code, 2, value).unwrap_or_else(|e| panic!("{e}"));
            assert_eq!(read(form, &code, 2), held(form, value), "{form:?} {value:#x} on {fill:#x}");
            assert_eq!(code[..2], [fill; 2], "{form:?} wrote before its field");
            let end = 2 + usize::from(form.bytes());
            assert!(code[end..].iter().all(|it| *it == fill), "{form:?} wrote past its field");
        }
        assert_eq!(check(form, value), Ok(()));
    }

    fn refused(form: Form, value: i64) {
        let mut code = [0x5a; 12];
        assert_eq!(write(form, &mut code, 2, value), Err(Overflow { form, value }), "{form:?} {value:#x}");
        assert_eq!(code, [0x5a; 12], "{form:?} wrote a value it refused");
        assert_eq!(check(form, value), Err(Overflow { form, value }));
    }

    #[test]
    fn the_bounds_cover_every_form_in_order() {
        for (at, bounds) in BOUNDS.iter().enumerate() {
            assert_eq!(bounds.form, Form::ALL[at]);
            assert_eq!(Form::from_u8(at as u8), Some(bounds.form));
        }
        assert_eq!(Form::from_u8(Form::ALL.len() as u8), None);
    }

    #[test]
    fn every_form_round_trips_at_its_bounds_and_refuses_one_past() {
        for Bounds { form, min, max, step, checked } in BOUNDS {
            for value in [min, min + step, 0, max - step, max] {
                round_trip(form, value);
            }
            if checked {
                refused(form, min - step);
                refused(form, max + step);
                if step > 1 {
                    refused(form, step / 2);
                    refused(form, max - step / 2);
                }
            }
        }
    }

    #[test]
    fn every_form_round_trips_a_spread_of_values() {
        let mut rng = SplitMix64::new(0x9e37_79b9_7f4a_7c15);
        for Bounds { form, min, max, step, .. } in BOUNDS {
            for _ in 0..2000 {
                let x = rng.next_u64();
                let span = (max as i128 - min as i128) / i128::from(step) + 1;
                let value = min as i128 + (i128::from(x) % span) * i128::from(step);
                round_trip(form, value as i64);
            }
        }
    }

    #[test]
    fn an_aarch64_field_keeps_the_instruction_s_other_bits() {
        for Bounds { form, .. } in BOUNDS.iter().filter(|it| it.form as u8 >= Form::A64MovwG0 as u8) {
            let mut code = u32::MAX.to_le_bytes();
            write(*form, &mut code, 0, 0).unwrap();
            assert_eq!(u32::from_le_bytes(code), !mask(*form), "{form:?}");
        }
    }

    #[test]
    fn pc_relative_forms_are_the_branches_and_literals() {
        for form in Form::ALL {
            let expected = matches!(
                form,
                Form::Rel32
                    | Form::A64Jump26
                    | Form::A64CondBr19
                    | Form::A64TestBr14
                    | Form::A64AdrLo21
                    | Form::A64LdrLit19
            );
            assert_eq!(form.pc_relative(), expected, "{form:?}");
        }
    }

    #[test]
    fn known_encodings() {
        // `b .+8`: imm26 = 2.
        let mut code = 0x1400_0000u32.to_le_bytes();
        write(Form::A64Jump26, &mut code, 0, 8).unwrap();
        assert_eq!(u32::from_le_bytes(code), 0x1400_0002);
        // `b.eq .-4`: imm19 all ones.
        let mut code = 0x5400_0000u32.to_le_bytes();
        write(Form::A64CondBr19, &mut code, 0, -4).unwrap();
        assert_eq!(u32::from_le_bytes(code), 0x54ff_ffe0);
        // `ldr x0, [x20, #16]`: imm12 = 2.
        let mut code = 0xf940_0280u32.to_le_bytes();
        write(Form::A64Ldst64Lo12, &mut code, 0, 16).unwrap();
        assert_eq!(u32::from_le_bytes(code), 0xf940_0a80);
        // `adr x0, .+5`: immlo = 1, immhi = 1.
        let mut code = 0x1000_0000u32.to_le_bytes();
        write(Form::A64AdrLo21, &mut code, 0, 5).unwrap();
        assert_eq!(u32::from_le_bytes(code), 0x3000_0020);
        // `movz x0, #0xbeef, lsl #16`.
        let mut code = 0xd2a0_0000u32.to_le_bytes();
        write(Form::A64MovwG1, &mut code, 0, 0xbeef_0000).unwrap();
        assert_eq!(u32::from_le_bytes(code), 0xd2b7_dde0);
        // x86-64 `jmp rel32` to 0x10 past its field.
        let mut code = [0xe9, 0, 0, 0, 0];
        write(Form::Rel32, &mut code, 1, 0x10).unwrap();
        assert_eq!(code, [0xe9, 0x10, 0, 0, 0]);
    }
}
