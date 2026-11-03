//! The C extension, as a peephole over finished 32-bit instruction words.
//!
//! RVC is not a separate instruction set so much as a shorthand: every
//! compressed instruction is a 32-bit one whose operands happen to fit in the
//! narrower fields. Assemblers therefore encode normally and shrink afterwards,
//! and this module is that step. It mirrors the `CompressPat` list in LLVM's
//! `RISCVInstrInfoC.td`, in the same order, because the order decides which of
//! two possible forms an instruction like `addi a0, a0, 0` ends up in.
//!
//! Anything whose immediate is still a fixup is left alone here; branches and
//! jumps get their compressed form from the relaxation candidates instead,
//! since only layout knows whether the displacement fits.

use super::reg::Reg;

/// Sign-extends the low `bits` of `v`.
fn sext(v: u32, bits: u32) -> i64 {
    let shift = 64 - bits;
    ((v as u64) << shift) as i64 >> shift
}

fn popular(n: u32) -> bool {
    (8..=15).contains(&n)
}

fn p(n: u32) -> u32 {
    n & 7
}

fn simm6(v: i64) -> bool {
    (-32..=31).contains(&v)
}

/// The fields every 32-bit instruction has in the same place.
struct Fields {
    opcode: u32,
    rd: u32,
    funct3: u32,
    rs1: u32,
    rs2: u32,
    funct7: u32,
    /// I-type immediate, sign-extended.
    imm_i: i64,
    /// S-type immediate, sign-extended.
    imm_s: i64,
}

fn decode(w: u32) -> Fields {
    Fields {
        opcode: w & 0x7f,
        rd: (w >> 7) & 31,
        funct3: (w >> 12) & 7,
        rs1: (w >> 15) & 31,
        rs2: (w >> 20) & 31,
        funct7: w >> 25,
        imm_i: sext(w >> 20, 12),
        imm_s: sext(((w >> 25) << 5) | ((w >> 7) & 31), 12),
    }
}

// ---- compressed encodings -------------------------------------------------

fn ciw_addi4spn(rd: u32, imm: i64) -> u16 {
    let imm = imm as u32;
    (((imm >> 4) & 3) << 11) as u16
        | (((imm >> 6) & 15) << 7) as u16
        | (((imm >> 2) & 1) << 6) as u16
        | (((imm >> 3) & 1) << 5) as u16
        | (p(rd) << 2) as u16
}

/// Quadrant 0 loads and stores: `imm[5:3]` in bits 12:10 and the remaining
/// two bits in 6:5, which differ between the word and doubleword forms.
fn cl_word(funct3: u32, rt: u32, base: u32, imm: i64, op: u32) -> u16 {
    let imm = imm as u32;
    ((funct3 << 13)
        | (((imm >> 3) & 7) << 10)
        | (p(base) << 7)
        | (((imm >> 2) & 1) << 6)
        | (((imm >> 6) & 1) << 5)
        | (p(rt) << 2)
        | op) as u16
}

fn cl_double(funct3: u32, rt: u32, base: u32, imm: i64, op: u32) -> u16 {
    let imm = imm as u32;
    ((funct3 << 13)
        | (((imm >> 3) & 7) << 10)
        | (p(base) << 7)
        | (((imm >> 6) & 3) << 5)
        | (p(rt) << 2)
        | op) as u16
}

/// Quadrant 1/2 immediate form: `imm[5]` in bit 12 and `imm[4:0]` in bits 6:2.
fn ci(funct3: u32, op: u32, reg: u32, imm: i64) -> u16 {
    let imm = imm as u32;
    ((funct3 << 13) | (((imm >> 5) & 1) << 12) | (reg << 7) | ((imm & 31) << 2) | op) as u16
}

fn c_addi16sp(imm: i64) -> u16 {
    let imm = imm as u32;
    (0b011 << 13)
        | (((imm >> 9) & 1) << 12) as u16
        | (2 << 7)
        | (((imm >> 4) & 1) << 6) as u16
        | (((imm >> 6) & 1) << 5) as u16
        | (((imm >> 7) & 3) << 3) as u16
        | (((imm >> 5) & 1) << 2) as u16
        | 0b01
}

/// `c.lui` keeps bits 17:12 of the value, which are the low six bits of the
/// 20-bit field `lui` itself takes.
fn c_lui(rd: u32, imm20: u32) -> u16 {
    (0b011 << 13)
        | (((imm20 >> 5) & 1) << 12) as u16
        | ((rd & 31) << 7) as u16
        | ((imm20 & 31) << 2) as u16
        | 0b01
}

/// Quadrant 1 shifts and `andi`, which share a two-bit sub-opcode in 11:10.
fn cb_alu(funct2: u32, reg: u32, imm: i64) -> u16 {
    let imm = imm as u32;
    (0b100 << 13)
        | (((imm >> 5) & 1) << 12) as u16
        | (funct2 << 10) as u16
        | (p(reg) << 7) as u16
        | ((imm & 31) << 2) as u16
        | 0b01
}

/// Quadrant 1 register-register form (`c.sub` and friends).
fn ca(hi: u32, lo: u32, rd: u32, src: u32) -> u16 {
    (0b100 << 13)
        | (hi << 12) as u16
        | (0b11 << 10)
        | (p(rd) << 7) as u16
        | (lo << 5) as u16
        | (p(src) << 2) as u16
        | 0b01
}

/// Quadrant 2 stack-relative loads: `uimm[5]` in bit 12, then `uimm[4:2]` and
/// the wrapped high bits.
fn c_lwsp(funct3: u32, rt: u32, imm: i64) -> u16 {
    let imm = imm as u32;
    ((funct3 << 13)
        | (((imm >> 5) & 1) << 12)
        | (rt << 7)
        | (((imm >> 2) & 7) << 4)
        | (((imm >> 6) & 3) << 2)
        | 0b10) as u16
}

fn c_ldsp(funct3: u32, rt: u32, imm: i64) -> u16 {
    let imm = imm as u32;
    ((funct3 << 13)
        | (((imm >> 5) & 1) << 12)
        | (rt << 7)
        | (((imm >> 3) & 3) << 5)
        | (((imm >> 6) & 7) << 2)
        | 0b10) as u16
}

fn c_swsp(funct3: u32, rt: u32, imm: i64) -> u16 {
    let imm = imm as u32;
    ((funct3 << 13) | (((imm >> 2) & 15) << 9) | (((imm >> 6) & 3) << 7) | (rt << 2) | 0b10) as u16
}

fn c_sdsp(funct3: u32, rt: u32, imm: i64) -> u16 {
    let imm = imm as u32;
    ((funct3 << 13) | (((imm >> 3) & 7) << 10) | (((imm >> 6) & 7) << 7) | (rt << 2) | 0b10) as u16
}

/// Quadrant 2 register form: `c.mv`, `c.add`, `c.jr`, `c.jalr`.
fn cr(hi: u32, r1: u32, r2: u32) -> u16 {
    (0b100 << 13)
        | ((hi & 1) << 12) as u16
        | ((r1 & 31) << 7) as u16
        | ((r2 & 31) << 2) as u16
        | 0b10
}

pub const C_NOP: u16 = 0x0001;
pub const C_EBREAK: u16 = 0x9002;
pub const C_UNIMP: u16 = 0x0000;

/// The compressed form of a branch, used by the relaxation candidates rather
/// than by [`compress`], whose caller does not know the displacement yet.
pub fn c_branch(eq: bool, rs1: Reg) -> u16 {
    let funct3 = if eq { 0b110 } else { 0b111 };
    ((funct3 << 13) | (rs1.popular_bits() << 7) | 0b01) as u16
}

/// The compressed form of `j` / `jal`, likewise without its displacement.
pub fn c_jump(link: bool) -> u16 {
    let funct3: u32 = if link { 0b001 } else { 0b101 };
    ((funct3 << 13) | 0b01) as u16
}

/// A shift's immediate is unsigned and may not exceed the register width.
fn shamt_ok(shamt: i64, xlen: u8) -> bool {
    shamt > 0 && shamt < xlen as i64
}

fn uimm(v: i64, max: i64, align: i64) -> bool {
    v >= 0 && v <= max && v % align == 0
}

/// The two-byte form of `word`, if the C extension has one.
pub fn compress(word: u32, xlen: u8) -> Option<u16> {
    let f = decode(word);
    let rv64 = xlen == 64;
    match (f.opcode, f.funct3) {
        // ---- addi: six different compressed spellings, in LLVM's order ----
        (0x13, 0b000) => {
            if popular(f.rd) && f.rs1 == 2 && uimm(f.imm_i, 1020, 4) && f.imm_i != 0 {
                return Some(ciw_addi4spn(f.rd, f.imm_i));
            }
            if f.rd == 0 && f.rs1 == 0 && f.imm_i == 0 {
                return Some(C_NOP);
            }
            if f.rd != 0 && f.rd == f.rs1 && f.imm_i != 0 && simm6(f.imm_i) {
                return Some(ci(0b000, 0b01, f.rd, f.imm_i));
            }
            if f.rd != 0 && f.rs1 == 0 && simm6(f.imm_i) {
                return Some(ci(0b010, 0b01, f.rd, f.imm_i));
            }
            if f.rd == 2
                && f.rs1 == 2
                && f.imm_i != 0
                && (-512..=496).contains(&f.imm_i)
                && f.imm_i % 16 == 0
            {
                return Some(c_addi16sp(f.imm_i));
            }
            if f.rd != 0 && f.rs1 != 0 && f.imm_i == 0 {
                return Some(cr(0, f.rd, f.rs1));
            }
            None
        }
        // ---- addiw (RV64 only) ----
        (0x1b, 0b000) if rv64 => {
            if f.rd != 0 && f.rd == f.rs1 && simm6(f.imm_i) {
                return Some(ci(0b001, 0b01, f.rd, f.imm_i));
            }
            if f.rd != 0 && f.rs1 == 0 && simm6(f.imm_i) {
                return Some(ci(0b010, 0b01, f.rd, f.imm_i));
            }
            None
        }
        // ---- slli / srli / srai / andi ----
        (0x13, 0b001) if f.funct7 >> 1 == 0 => {
            let shamt = ((word >> 20) & 0x3f) as i64;
            (f.rd != 0 && f.rd == f.rs1 && shamt_ok(shamt, xlen))
                .then(|| ci(0b000, 0b10, f.rd, shamt))
        }
        (0x13, 0b101) => {
            let shamt = ((word >> 20) & 0x3f) as i64;
            // `funct6`, since bit 25 belongs to the shift amount on RV64.
            let arith = match f.funct7 >> 1 {
                0b00_0000 => false,
                0b01_0000 => true,
                _ => return None,
            };
            (popular(f.rd) && f.rd == f.rs1 && shamt_ok(shamt, xlen))
                .then(|| cb_alu(if arith { 0b01 } else { 0b00 }, f.rd, shamt))
        }
        (0x13, 0b111) => {
            (popular(f.rd) && f.rd == f.rs1 && simm6(f.imm_i)).then(|| cb_alu(0b10, f.rd, f.imm_i))
        }

        // ---- loads and stores ----
        (0x03, 0b010) => {
            if popular(f.rd) && popular(f.rs1) && uimm(f.imm_i, 124, 4) {
                return Some(cl_word(0b010, f.rd, f.rs1, f.imm_i, 0b00));
            }
            (f.rd != 0 && f.rs1 == 2 && uimm(f.imm_i, 252, 4)).then(|| c_lwsp(0b010, f.rd, f.imm_i))
        }
        (0x03, 0b011) if rv64 => {
            if popular(f.rd) && popular(f.rs1) && uimm(f.imm_i, 248, 8) {
                return Some(cl_double(0b011, f.rd, f.rs1, f.imm_i, 0b00));
            }
            (f.rd != 0 && f.rs1 == 2 && uimm(f.imm_i, 504, 8)).then(|| c_ldsp(0b011, f.rd, f.imm_i))
        }
        (0x23, 0b010) => {
            if popular(f.rs2) && popular(f.rs1) && uimm(f.imm_s, 124, 4) {
                return Some(cl_word(0b110, f.rs2, f.rs1, f.imm_s, 0b00));
            }
            (f.rs1 == 2 && uimm(f.imm_s, 252, 4)).then(|| c_swsp(0b110, f.rs2, f.imm_s))
        }
        (0x23, 0b011) if rv64 => {
            if popular(f.rs2) && popular(f.rs1) && uimm(f.imm_s, 248, 8) {
                return Some(cl_double(0b111, f.rs2, f.rs1, f.imm_s, 0b00));
            }
            (f.rs1 == 2 && uimm(f.imm_s, 504, 8)).then(|| c_sdsp(0b111, f.rs2, f.imm_s))
        }
        // `fld`/`fsd` compress on both widths; `flw`/`fsw` only on RV32,
        // where the slots RV64 uses for `ld`/`sd` are free.
        (0x07, 0b011) => {
            if popular(f.rd) && popular(f.rs1) && uimm(f.imm_i, 248, 8) {
                return Some(cl_double(0b001, f.rd, f.rs1, f.imm_i, 0b00));
            }
            (f.rs1 == 2 && uimm(f.imm_i, 504, 8)).then(|| c_ldsp(0b001, f.rd, f.imm_i))
        }
        (0x27, 0b011) => {
            if popular(f.rs2) && popular(f.rs1) && uimm(f.imm_s, 248, 8) {
                return Some(cl_double(0b101, f.rs2, f.rs1, f.imm_s, 0b00));
            }
            (f.rs1 == 2 && uimm(f.imm_s, 504, 8)).then(|| c_sdsp(0b101, f.rs2, f.imm_s))
        }
        (0x07, 0b010) if !rv64 => {
            if popular(f.rd) && popular(f.rs1) && uimm(f.imm_i, 124, 4) {
                return Some(cl_word(0b011, f.rd, f.rs1, f.imm_i, 0b00));
            }
            (f.rs1 == 2 && uimm(f.imm_i, 252, 4)).then(|| c_lwsp(0b011, f.rd, f.imm_i))
        }
        (0x27, 0b010) if !rv64 => {
            if popular(f.rs2) && popular(f.rs1) && uimm(f.imm_s, 124, 4) {
                return Some(cl_word(0b111, f.rs2, f.rs1, f.imm_s, 0b00));
            }
            (f.rs1 == 2 && uimm(f.imm_s, 252, 4)).then(|| c_swsp(0b111, f.rs2, f.imm_s))
        }

        // ---- lui ----
        (0x37, _) => {
            let imm20 = word >> 12;
            let in_range = (1..=31).contains(&imm20) || (0xf_ffe0..=0xf_ffff).contains(&imm20);
            (f.rd != 0 && f.rd != 2 && in_range).then(|| c_lui(f.rd, imm20))
        }

        // ---- register-register ----
        (0x33, _) => compress_alu(&f, 0),
        (0x3b, _) if rv64 => compress_alu(&f, 1),

        // ---- jalr ----
        (0x67, 0b000) if f.imm_i == 0 && f.rs1 != 0 => match f.rd {
            0 => Some(cr(0, f.rs1, 0)),
            1 => Some(cr(1, f.rs1, 0)),
            _ => None,
        },

        _ => {
            // `unimp` is deliberately absent: `c.unimp` is a row of its own
            // in binutils' table, chosen for the mnemonic, not a compression
            // of `csrrw x0, cycle, x0`. Both references leave that spelling
            // four bytes wide, whatever it is written as.
            if word == 0x0010_0073 {
                Some(C_EBREAK)
            } else {
                None
            }
        }
    }
}

/// `add`, `sub`, `xor`, `or`, `and` and their `w` forms.
///
/// The commutative ones have a second pattern with the operands the other way
/// round, which is why `add a0, a1, a0` compresses just like `add a0, a0, a1`.
fn compress_alu(f: &Fields, word_op: u32) -> Option<u16> {
    let sub = match f.funct7 {
        0x00 => false,
        0x20 => true,
        _ => return None,
    };
    let (rd, rs1, rs2) = (f.rd, f.rs1, f.rs2);
    match (word_op, f.funct3, sub) {
        // add
        (0, 0b000, false) => {
            if rd != 0 && rs1 == 0 && rs2 != 0 {
                return Some(cr(0, rd, rs2));
            }
            if rd != 0 && rs2 == 0 && rs1 != 0 {
                return Some(cr(0, rd, rs1));
            }
            if rd != 0 && rd == rs1 && rs2 != 0 {
                return Some(cr(1, rd, rs2));
            }
            if rd != 0 && rd == rs2 && rs1 != 0 {
                return Some(cr(1, rd, rs1));
            }
            None
        }
        // sub
        (0, 0b000, true) => {
            (popular(rd) && rd == rs1 && popular(rs2)).then(|| ca(0, 0b00, rd, rs2))
        }
        (0, 0b100, false) => commutative(rd, rs1, rs2, 0, 0b01),
        (0, 0b110, false) => commutative(rd, rs1, rs2, 0, 0b10),
        (0, 0b111, false) => commutative(rd, rs1, rs2, 0, 0b11),
        // subw / addw
        (1, 0b000, true) => {
            (popular(rd) && rd == rs1 && popular(rs2)).then(|| ca(1, 0b00, rd, rs2))
        }
        (1, 0b000, false) => commutative(rd, rs1, rs2, 1, 0b01),
        _ => None,
    }
}

fn commutative(rd: u32, rs1: u32, rs2: u32, hi: u32, lo: u32) -> Option<u16> {
    if !popular(rd) {
        return None;
    }
    if rd == rs1 && popular(rs2) {
        return Some(ca(hi, lo, rd, rs2));
    }
    if rd == rs2 && popular(rs1) {
        return Some(ca(hi, lo, rd, rs1));
    }
    None
}
