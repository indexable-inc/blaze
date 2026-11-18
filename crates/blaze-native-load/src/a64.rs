//! The A64 instruction forms a relocation patches, read back into the address they name: the
//! inverse of the relocation functions (blaze-native layout.rs writes the fields; this reads
//! them). A checker of a placed image (every patched site names its symbol) is built from
//! these; so are the tests. Each returns `None` for a word of another form.
//!
//! From the Arm Architecture Reference Manual (A64 base instructions):
//! - `B` (`0b000101`) and `BL` (`0b100101`) in bits 31..26: `offset = SignExtend(imm26:'00')`,
//!   target `PC + offset`: from `PC - 2^27` to `PC + 2^27 - 4`.
//! - `ADRP` (`op = 1`, bits 28..24 `0b10000`): `imm = SignExtend(immhi:immlo:Zeros(12))`, target
//!   `(PC AND NOT 0xFFF) + imm`: pages from `-2^20` to `2^20 - 1`, so +-4 GiB.
//! - `ADD (immediate)`, 64-bit, `sh = 0` (`0x91000000` under mask `0xFFC00000`): `imm12`.
//! - `LDR`/`STR (immediate, unsigned offset)` (bits 29..27 `0b111`, 25..24 `0b01`, 26 `0`): the
//!   byte offset is `imm12 << size` (`size` bits 31..30), and `<< 4` for a 128-bit `Q` access
//!   (`size = 00`, `opc` bit 23 set, `V = 1`).
//! - `B.cond` / `CBZ`/`CBNZ`: `imm19:'00'`, +-1 MiB; `TBZ`/`TBNZ`: `imm14:'00'`, +-32 KiB.

/// `value`'s low `bits` bits, sign-extended.
pub fn sext(value: u64, bits: u32) -> i64 {
    let shift = 64 - bits;
    ((value << shift) as i64) >> shift
}

/// Where a `B` or `BL` at `place` branches.
pub fn branch26(insn: u32, place: u64) -> Option<u64> {
    if insn & 0x7c00_0000 != 0x1400_0000 {
        return None;
    }
    Some(place.wrapping_add((sext(u64::from(insn & 0x03ff_ffff), 26) << 2) as u64))
}

/// Whether `insn` is `BL` (and not `B`).
pub fn is_bl(insn: u32) -> bool {
    insn & 0xfc00_0000 == 0x9400_0000
}

/// Where a `B.cond`, `CBZ` or `CBNZ` at `place` branches.
pub fn branch19(insn: u32, place: u64) -> Option<u64> {
    let cond = insn & 0xff00_0010 == 0x5400_0000;
    let cb = insn & 0x7e00_0000 == 0x3400_0000;
    if !cond && !cb {
        return None;
    }
    Some(place.wrapping_add((sext(u64::from((insn >> 5) & 0x7_ffff), 19) << 2) as u64))
}

/// Where a `TBZ` or `TBNZ` at `place` branches.
pub fn branch14(insn: u32, place: u64) -> Option<u64> {
    if insn & 0x7e00_0000 != 0x3600_0000 {
        return None;
    }
    Some(place.wrapping_add((sext(u64::from((insn >> 5) & 0x3fff), 14) << 2) as u64))
}

/// The page an `ADRP` at `place` names.
pub fn adrp(insn: u32, place: u64) -> Option<u64> {
    if insn & 0x9f00_0000 != 0x9000_0000 {
        return None;
    }
    let imm = (u64::from((insn >> 5) & 0x7_ffff) << 2) | u64::from((insn >> 29) & 3);
    Some((place & !0xfff).wrapping_add((sext(imm, 21) << 12) as u64))
}

/// The low 12 bits an `ADD (immediate)` (64-bit, unshifted) adds.
pub fn add_lo12(insn: u32) -> Option<u64> {
    if insn & 0xffc0_0000 != 0x9100_0000 {
        return None;
    }
    Some(u64::from((insn >> 10) & 0xfff))
}

/// The byte offset of a load or store (immediate, unsigned offset): `imm12` scaled by the
/// access size.
pub fn ldst_lo12(insn: u32) -> Option<u64> {
    if insn & 0x3b00_0000 != 0x3900_0000 {
        return None;
    }
    let size = insn >> 30;
    let vector = insn & 0x0400_0000 != 0;
    let q = vector && size == 0 && insn & 0x0080_0000 != 0;
    let scale = if q { 4 } else { size };
    Some(u64::from((insn >> 10) & 0xfff) << scale)
}

/// The address an `ADRP` at `place` and the `ADD` or load/store after it make together.
pub fn page_pair(adrp_insn: u32, place: u64, low: u32) -> Option<u64> {
    let page = adrp(adrp_insn, place)?;
    let lo = add_lo12(low).or_else(|| ldst_lo12(low))?;
    Some(page.wrapping_add(lo))
}
