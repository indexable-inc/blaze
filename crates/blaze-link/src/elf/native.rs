//! The relocations of native objects no `Apply` names, as lld applies them: the narrow
//! data relocations and the AArch64 instruction forms C compilers rarely emit.

use blaze_codegen::Arch;
use object::elf;

pub(crate) fn apply(arch: Arch, r_type: u32, bytes: &mut [u8], p: u64, s: u64, addend: i64) -> Result<(), String> {
    let abs = s.wrapping_add(addend as u64) as i64;
    let rel = abs.wrapping_sub(p as i64);
    match (arch, elf::RelocationType(r_type)) {
        (Arch::X86_64, elf::R_X86_64_8) => put(bytes, &uint_or_int(abs, 8, "R_X86_64_8")?.to_le_bytes()[..1]),
        (Arch::X86_64, elf::R_X86_64_16) => put(bytes, &uint_or_int(abs, 16, "R_X86_64_16")?.to_le_bytes()[..2]),
        (Arch::X86_64, elf::R_X86_64_PC8) => put(bytes, &int(rel, 8, "R_X86_64_PC8")?.to_le_bytes()[..1]),
        (Arch::X86_64, elf::R_X86_64_PC16) => put(bytes, &int(rel, 16, "R_X86_64_PC16")?.to_le_bytes()[..2]),
        (Arch::Aarch64, elf::R_AARCH64_ABS16) => {
            put(bytes, &uint_or_int(abs, 16, "R_AARCH64_ABS16")?.to_le_bytes()[..2])
        }
        (Arch::Aarch64, elf::R_AARCH64_PREL16) => {
            put(bytes, &uint_or_int(rel, 16, "R_AARCH64_PREL16")?.to_le_bytes()[..2])
        }
        (Arch::Aarch64, elf::R_AARCH64_ADR_PREL_PG_HI21_NC) => {
            let page = (abs as u64 & !0xfff).wrapping_sub(p & !0xfff) as i64;
            let imm = ((page >> 12) as u32) & 0x1f_ffff;
            insn(bytes, !((0x3 << 29) | (0x7_ffff << 5)), ((imm & 0x3) << 29) | ((imm >> 2) << 5))
        }
        (Arch::Aarch64, elf::R_AARCH64_LD_PREL_LO19) => {
            if rel & 3 != 0 {
                // alloc: error path
                return Err(format!("R_AARCH64_LD_PREL_LO19 to {rel:#x} from the place: not 4-aligned"));
            }
            int(rel, 21, "R_AARCH64_LD_PREL_LO19")?;
            insn(bytes, !(0x7_ffff << 5), (((rel >> 2) as u32) & 0x7_ffff) << 5)
        }
        (Arch::Aarch64, elf::R_AARCH64_MOVW_SABS_G0) => smovw(bytes, abs, 0, true, "R_AARCH64_MOVW_SABS_G0"),
        (Arch::Aarch64, elf::R_AARCH64_MOVW_SABS_G1) => smovw(bytes, abs, 1, true, "R_AARCH64_MOVW_SABS_G1"),
        (Arch::Aarch64, elf::R_AARCH64_MOVW_SABS_G2) => smovw(bytes, abs, 2, true, "R_AARCH64_MOVW_SABS_G2"),
        (Arch::Aarch64, elf::R_AARCH64_MOVW_PREL_G0) => smovw(bytes, rel, 0, true, "R_AARCH64_MOVW_PREL_G0"),
        (Arch::Aarch64, elf::R_AARCH64_MOVW_PREL_G0_NC) => smovw(bytes, rel, 0, false, ""),
        (Arch::Aarch64, elf::R_AARCH64_MOVW_PREL_G1) => smovw(bytes, rel, 1, true, "R_AARCH64_MOVW_PREL_G1"),
        (Arch::Aarch64, elf::R_AARCH64_MOVW_PREL_G1_NC) => smovw(bytes, rel, 1, false, ""),
        (Arch::Aarch64, elf::R_AARCH64_MOVW_PREL_G2) => smovw(bytes, rel, 2, true, "R_AARCH64_MOVW_PREL_G2"),
        (Arch::Aarch64, elf::R_AARCH64_MOVW_PREL_G2_NC) => smovw(bytes, rel, 2, false, ""),
        (Arch::Aarch64, elf::R_AARCH64_MOVW_PREL_G3) => smovw(bytes, rel, 3, false, ""),
        // alloc: error path
        _ => Err(format!("relocation type {r_type} is not one the ELF writer applies for {arch:?}")),
    }
}

/// A value that fits `bits` as a signed or an unsigned number (lld's `checkIntUInt`).
fn uint_or_int(value: i64, bits: u32, what: &str) -> Result<u64, String> {
    if value < -(1i64 << (bits - 1)) || value >= 1i64 << bits {
        return Err(format!("{what}: {value:#x} does not fit {bits} bits")); // alloc: error path
    }
    Ok(value as u64)
}

fn int(value: i64, bits: u32, what: &str) -> Result<u64, String> {
    if value < -(1i64 << (bits - 1)) || value >= 1i64 << (bits - 1) {
        return Err(format!("{what}: {value:#x} does not fit {bits} signed bits")); // alloc: error path
    }
    Ok(value as u64)
}

fn put(bytes: &mut [u8], value: &[u8]) -> Result<(), String> {
    let place = bytes
        .get_mut(..value.len())
        .ok_or_else(|| format!("a {}-byte relocation at the end of its section", value.len()))?; // alloc: error path
    place.copy_from_slice(value);
    Ok(())
}

fn insn(bytes: &mut [u8], keep: u32, set: u32) -> Result<(), String> {
    let place = bytes.get_mut(..4).ok_or("an instruction relocation at the end of its section")?;
    let old = u32::from_le_bytes([place[0], place[1], place[2], place[3]]);
    place.copy_from_slice(&((old & keep) | set).to_le_bytes());
    Ok(())
}

/// 16 bits of `value` at `group * 16` into a `MOVZ`, `MOVN` or `MOVK`; a `MOVZ` or `MOVN`
/// becomes `MOVN` of the inverted bits when the value is negative (lld's `writeSMovWImm`).
/// `checked` groups must hold the whole value: 17, 33 or 49 signed bits.
fn smovw(bytes: &mut [u8], value: i64, group: u32, checked: bool, what: &str) -> Result<(), String> {
    if checked {
        int(value, 17 + 16 * group, what)?;
    }
    let place = bytes.get(..4).ok_or("an instruction relocation at the end of its section")?;
    let old = u32::from_le_bytes([place[0], place[1], place[2], place[3]]);
    let mut imm = ((value as u64) >> (16 * group)) as u32;
    let mut opcode = old & (1 << 30);
    if old & (1 << 29) == 0 {
        if imm & 0x1_0000 != 0 {
            imm ^= 0xffff;
            opcode = 0;
        } else {
            opcode = 1 << 30;
        }
    }
    insn(bytes, !((1 << 30) | (0xffff << 5)), opcode | ((imm & 0xffff) << 5))
}
