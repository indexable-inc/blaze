//! The opcode table, transcribed from `include/opcode/avr.h` of binutils
//! 2.47.
//!
//! Each row is one form: its mnemonic, the operand constraints, the bit
//! pattern, its length in 16-bit words, the instruction-set bit a device
//! needs to have it, and the base word the operands are OR'd into.
//!
//! The order matters, twice. Instructions sharing a mnemonic are listed
//! together and tried in order until one the target has is found, which is
//! how `lds` picks between the AVR-tiny one-word form and the two-word one.
//! And a form whose constraints begin with `?` is the operand-less version of
//! the next row, which is how `lpm`, `elpm` and `spm` take an optional
//! operand.
//!
//! # The constraint letters
//!
//! The table's own, from the comment in that header:
//!
//! | | |
//! |---|---|
//! | `r` | any register |
//! | `d` | an `ldi` register, r16-r31 |
//! | `v` | an even register, for `movw` |
//! | `a` | an `fmul` register, r16-r23 |
//! | `w` | r24, r26, r28 or r30, for `adiw` |
//! | `e` | a pointer register, X, Y or Z, with `-` or `+` |
//! | `b` | Y or Z with a displacement |
//! | `z` | Z, with `+`, for `lpm`/`elpm`/`spm` |
//! | `M` | an 8-bit immediate, which may carry a `lo8()`-style modifier |
//! | `n` | a constant 0 to 255, complemented (`cbr`) |
//! | `N` | a constant 0 to 255 |
//! | `s` | a bit number, 0 to 7 |
//! | `S` | a bit number, 0 to 7, shifted left by 4 |
//! | `E` | a constant 0 to 15, shifted left by 4 (`des`) |
//! | `P` | an I/O address, 0 to 63 (`in`, `out`) |
//! | `p` | an I/O address, 0 to 31 (`cbi`, `sbi`, `sbic`, `sbis`) |
//! | `K` | a constant 0 to 63 (`adiw`, `sbiw`) |
//! | `i` | a 16-bit data address (`lds`, `sts`) |
//! | `j` | a 7-bit data address, 0x40 to 0xbf (AVR-tiny `lds`, `sts`) |
//! | `l` | a PC-relative branch target, -64 to 63 words |
//! | `L` | a PC-relative branch target, -2048 to 2047 words |
//! | `h` | an absolute code address (`call`, `jmp`) |
//! | `?` | this row if there are no operands, else the next one |
//! | `=` | the second operand is the first (`clr r5` is `eor r5, r5`) |

use super::isa::*;

/// One form of one instruction.
#[derive(Copy, Clone, Debug)]
pub struct Insn {
    pub name: &'static str,
    /// The constraint letters, comma-separated as in the table.
    pub ops: &'static str,
    /// The 16-bit pattern, kept because the `z` constraint reads the
    /// position of its `+` out of it: `lpm Rd, Z+` sets bit 0 and `spm Z+`
    /// bit 4, and only the pattern says which.
    pub bits: &'static str,
    pub words: u8,
    pub isa: Isa,
    pub base: u16,
}

impl Insn {
    /// The bit the `z` constraint's `+` sets, from the pattern.
    pub fn postinc_bit(&self) -> u16 {
        let mut v = 0;
        for (i, c) in self.bits.bytes().enumerate() {
            if c == b'+' {
                v |= 1 << (15 - i);
            }
        }
        v
    }
}

/// Every form with this mnemonic, in table order.
pub fn forms(name: &str) -> &'static [Insn] {
    let start = INSNS.iter().position(|i| i.name == name);
    let Some(start) = start else {
        return &[];
    };
    let end = INSNS[start..]
        .iter()
        .position(|i| i.name != name)
        .map_or(INSNS.len(), |n| start + n);
    &INSNS[start..end]
}

/// Whether any form has this mnemonic, whatever the target's instruction set.
pub fn is_mnemonic(name: &str) -> bool {
    INSNS.iter().any(|i| i.name == name)
}

/// `avr_opcodes` from `gas/config/tc-avr.c`, which is
/// `include/opcode/avr.h` read as a table.
#[rustfmt::skip]
pub const INSNS: &[Insn] = &[
    Insn { name: "clc", ops: "", bits: "1001010010001000", words: 1, isa: ISA_1200, base: 0x9488 },
    Insn { name: "clh", ops: "", bits: "1001010011011000", words: 1, isa: ISA_1200, base: 0x94d8 },
    Insn { name: "cli", ops: "", bits: "1001010011111000", words: 1, isa: ISA_1200, base: 0x94f8 },
    Insn { name: "cln", ops: "", bits: "1001010010101000", words: 1, isa: ISA_1200, base: 0x94a8 },
    Insn { name: "cls", ops: "", bits: "1001010011001000", words: 1, isa: ISA_1200, base: 0x94c8 },
    Insn { name: "clt", ops: "", bits: "1001010011101000", words: 1, isa: ISA_1200, base: 0x94e8 },
    Insn { name: "clv", ops: "", bits: "1001010010111000", words: 1, isa: ISA_1200, base: 0x94b8 },
    Insn { name: "clz", ops: "", bits: "1001010010011000", words: 1, isa: ISA_1200, base: 0x9498 },
    Insn { name: "sec", ops: "", bits: "1001010000001000", words: 1, isa: ISA_1200, base: 0x9408 },
    Insn { name: "seh", ops: "", bits: "1001010001011000", words: 1, isa: ISA_1200, base: 0x9458 },
    Insn { name: "sei", ops: "", bits: "1001010001111000", words: 1, isa: ISA_1200, base: 0x9478 },
    Insn { name: "sen", ops: "", bits: "1001010000101000", words: 1, isa: ISA_1200, base: 0x9428 },
    Insn { name: "ses", ops: "", bits: "1001010001001000", words: 1, isa: ISA_1200, base: 0x9448 },
    Insn { name: "set", ops: "", bits: "1001010001101000", words: 1, isa: ISA_1200, base: 0x9468 },
    Insn { name: "sev", ops: "", bits: "1001010000111000", words: 1, isa: ISA_1200, base: 0x9438 },
    Insn { name: "sez", ops: "", bits: "1001010000011000", words: 1, isa: ISA_1200, base: 0x9418 },
    Insn { name: "bclr", ops: "S", bits: "100101001SSS1000", words: 1, isa: ISA_1200, base: 0x9488 },
    Insn { name: "bset", ops: "S", bits: "100101000SSS1000", words: 1, isa: ISA_1200, base: 0x9408 },
    Insn { name: "icall", ops: "", bits: "1001010100001001", words: 1, isa: ISA_2XXXA, base: 0x9509 },
    Insn { name: "ijmp", ops: "", bits: "1001010000001001", words: 1, isa: ISA_2XXXA, base: 0x9409 },
    Insn { name: "lpm", ops: "?", bits: "1001010111001000", words: 1, isa: ISA_TINY1, base: 0x95c8 },
    Insn { name: "lpm", ops: "r,z", bits: "1001000ddddd010+", words: 1, isa: ISA_LPMX, base: 0x9004 },
    Insn { name: "elpm", ops: "?", bits: "1001010111011000", words: 1, isa: ISA_ELPM, base: 0x95d8 },
    Insn { name: "elpm", ops: "r,z", bits: "1001000ddddd011+", words: 1, isa: ISA_ELPMX, base: 0x9006 },
    Insn { name: "nop", ops: "", bits: "0000000000000000", words: 1, isa: ISA_1200, base: 0x0000 },
    Insn { name: "ret", ops: "", bits: "1001010100001000", words: 1, isa: ISA_1200, base: 0x9508 },
    Insn { name: "reti", ops: "", bits: "1001010100011000", words: 1, isa: ISA_1200, base: 0x9518 },
    Insn { name: "sleep", ops: "", bits: "1001010110001000", words: 1, isa: ISA_1200, base: 0x9588 },
    Insn { name: "break", ops: "", bits: "1001010110011000", words: 1, isa: ISA_BRK, base: 0x9598 },
    Insn { name: "wdr", ops: "", bits: "1001010110101000", words: 1, isa: ISA_1200, base: 0x95a8 },
    Insn { name: "spm", ops: "?", bits: "1001010111101000", words: 1, isa: ISA_SPM, base: 0x95e8 },
    Insn { name: "spm", ops: "z", bits: "10010101111+1000", words: 1, isa: ISA_SPMX, base: 0x95e8 },
    Insn { name: "adc", ops: "r,r", bits: "000111rdddddrrrr", words: 1, isa: ISA_1200, base: 0x1c00 },
    Insn { name: "add", ops: "r,r", bits: "000011rdddddrrrr", words: 1, isa: ISA_1200, base: 0x0c00 },
    Insn { name: "and", ops: "r,r", bits: "001000rdddddrrrr", words: 1, isa: ISA_1200, base: 0x2000 },
    Insn { name: "cp", ops: "r,r", bits: "000101rdddddrrrr", words: 1, isa: ISA_1200, base: 0x1400 },
    Insn { name: "cpc", ops: "r,r", bits: "000001rdddddrrrr", words: 1, isa: ISA_1200, base: 0x0400 },
    Insn { name: "cpse", ops: "r,r", bits: "000100rdddddrrrr", words: 1, isa: ISA_1200, base: 0x1000 },
    Insn { name: "eor", ops: "r,r", bits: "001001rdddddrrrr", words: 1, isa: ISA_1200, base: 0x2400 },
    Insn { name: "mov", ops: "r,r", bits: "001011rdddddrrrr", words: 1, isa: ISA_1200, base: 0x2c00 },
    Insn { name: "mul", ops: "r,r", bits: "100111rdddddrrrr", words: 1, isa: ISA_MUL, base: 0x9c00 },
    Insn { name: "or", ops: "r,r", bits: "001010rdddddrrrr", words: 1, isa: ISA_1200, base: 0x2800 },
    Insn { name: "sbc", ops: "r,r", bits: "000010rdddddrrrr", words: 1, isa: ISA_1200, base: 0x0800 },
    Insn { name: "sub", ops: "r,r", bits: "000110rdddddrrrr", words: 1, isa: ISA_1200, base: 0x1800 },
    Insn { name: "clr", ops: "r=r", bits: "001001rdddddrrrr", words: 1, isa: ISA_1200, base: 0x2400 },
    Insn { name: "lsl", ops: "r=r", bits: "000011rdddddrrrr", words: 1, isa: ISA_1200, base: 0x0c00 },
    Insn { name: "rol", ops: "r=r", bits: "000111rdddddrrrr", words: 1, isa: ISA_1200, base: 0x1c00 },
    Insn { name: "tst", ops: "r=r", bits: "001000rdddddrrrr", words: 1, isa: ISA_1200, base: 0x2000 },
    Insn { name: "andi", ops: "d,M", bits: "0111KKKKddddKKKK", words: 1, isa: ISA_1200, base: 0x7000 },
    Insn { name: "cbr", ops: "d,n", bits: "0111KKKKddddKKKK", words: 1, isa: ISA_1200, base: 0x7000 },
    Insn { name: "ldi", ops: "d,M", bits: "1110KKKKddddKKKK", words: 1, isa: ISA_1200, base: 0xe000 },
    Insn { name: "ser", ops: "d", bits: "11101111dddd1111", words: 1, isa: ISA_1200, base: 0xef0f },
    Insn { name: "ori", ops: "d,M", bits: "0110KKKKddddKKKK", words: 1, isa: ISA_1200, base: 0x6000 },
    Insn { name: "sbr", ops: "d,M", bits: "0110KKKKddddKKKK", words: 1, isa: ISA_1200, base: 0x6000 },
    Insn { name: "cpi", ops: "d,M", bits: "0011KKKKddddKKKK", words: 1, isa: ISA_1200, base: 0x3000 },
    Insn { name: "sbci", ops: "d,M", bits: "0100KKKKddddKKKK", words: 1, isa: ISA_1200, base: 0x4000 },
    Insn { name: "subi", ops: "d,M", bits: "0101KKKKddddKKKK", words: 1, isa: ISA_1200, base: 0x5000 },
    Insn { name: "sbrc", ops: "r,s", bits: "1111110rrrrr0sss", words: 1, isa: ISA_1200, base: 0xfc00 },
    Insn { name: "sbrs", ops: "r,s", bits: "1111111rrrrr0sss", words: 1, isa: ISA_1200, base: 0xfe00 },
    Insn { name: "bld", ops: "r,s", bits: "1111100ddddd0sss", words: 1, isa: ISA_1200, base: 0xf800 },
    Insn { name: "bst", ops: "r,s", bits: "1111101ddddd0sss", words: 1, isa: ISA_1200, base: 0xfa00 },
    Insn { name: "in", ops: "r,P", bits: "10110PPdddddPPPP", words: 1, isa: ISA_1200, base: 0xb000 },
    Insn { name: "out", ops: "P,r", bits: "10111PPrrrrrPPPP", words: 1, isa: ISA_1200, base: 0xb800 },
    Insn { name: "adiw", ops: "w,K", bits: "10010110KKddKKKK", words: 1, isa: ISA_2XXX, base: 0x9600 },
    Insn { name: "sbiw", ops: "w,K", bits: "10010111KKddKKKK", words: 1, isa: ISA_2XXX, base: 0x9700 },
    Insn { name: "cbi", ops: "p,s", bits: "10011000pppppsss", words: 1, isa: ISA_1200, base: 0x9800 },
    Insn { name: "sbi", ops: "p,s", bits: "10011010pppppsss", words: 1, isa: ISA_1200, base: 0x9a00 },
    Insn { name: "sbic", ops: "p,s", bits: "10011001pppppsss", words: 1, isa: ISA_1200, base: 0x9900 },
    Insn { name: "sbis", ops: "p,s", bits: "10011011pppppsss", words: 1, isa: ISA_1200, base: 0x9b00 },
    Insn { name: "brcc", ops: "l", bits: "111101lllllll000", words: 1, isa: ISA_1200, base: 0xf400 },
    Insn { name: "brcs", ops: "l", bits: "111100lllllll000", words: 1, isa: ISA_1200, base: 0xf000 },
    Insn { name: "breq", ops: "l", bits: "111100lllllll001", words: 1, isa: ISA_1200, base: 0xf001 },
    Insn { name: "brge", ops: "l", bits: "111101lllllll100", words: 1, isa: ISA_1200, base: 0xf404 },
    Insn { name: "brhc", ops: "l", bits: "111101lllllll101", words: 1, isa: ISA_1200, base: 0xf405 },
    Insn { name: "brhs", ops: "l", bits: "111100lllllll101", words: 1, isa: ISA_1200, base: 0xf005 },
    Insn { name: "brid", ops: "l", bits: "111101lllllll111", words: 1, isa: ISA_1200, base: 0xf407 },
    Insn { name: "brie", ops: "l", bits: "111100lllllll111", words: 1, isa: ISA_1200, base: 0xf007 },
    Insn { name: "brlo", ops: "l", bits: "111100lllllll000", words: 1, isa: ISA_1200, base: 0xf000 },
    Insn { name: "brlt", ops: "l", bits: "111100lllllll100", words: 1, isa: ISA_1200, base: 0xf004 },
    Insn { name: "brmi", ops: "l", bits: "111100lllllll010", words: 1, isa: ISA_1200, base: 0xf002 },
    Insn { name: "brne", ops: "l", bits: "111101lllllll001", words: 1, isa: ISA_1200, base: 0xf401 },
    Insn { name: "brpl", ops: "l", bits: "111101lllllll010", words: 1, isa: ISA_1200, base: 0xf402 },
    Insn { name: "brsh", ops: "l", bits: "111101lllllll000", words: 1, isa: ISA_1200, base: 0xf400 },
    Insn { name: "brtc", ops: "l", bits: "111101lllllll110", words: 1, isa: ISA_1200, base: 0xf406 },
    Insn { name: "brts", ops: "l", bits: "111100lllllll110", words: 1, isa: ISA_1200, base: 0xf006 },
    Insn { name: "brvc", ops: "l", bits: "111101lllllll011", words: 1, isa: ISA_1200, base: 0xf403 },
    Insn { name: "brvs", ops: "l", bits: "111100lllllll011", words: 1, isa: ISA_1200, base: 0xf003 },
    Insn { name: "brbc", ops: "s,l", bits: "111101lllllllsss", words: 1, isa: ISA_1200, base: 0xf400 },
    Insn { name: "brbs", ops: "s,l", bits: "111100lllllllsss", words: 1, isa: ISA_1200, base: 0xf000 },
    Insn { name: "rcall", ops: "L", bits: "1101LLLLLLLLLLLL", words: 1, isa: ISA_1200, base: 0xd000 },
    Insn { name: "rjmp", ops: "L", bits: "1100LLLLLLLLLLLL", words: 1, isa: ISA_1200, base: 0xc000 },
    Insn { name: "call", ops: "h", bits: "1001010hhhhh111h", words: 2, isa: ISA_MEGA, base: 0x940e },
    Insn { name: "jmp", ops: "h", bits: "1001010hhhhh110h", words: 2, isa: ISA_MEGA, base: 0x940c },
    Insn { name: "asr", ops: "r", bits: "1001010rrrrr0101", words: 1, isa: ISA_1200, base: 0x9405 },
    Insn { name: "com", ops: "r", bits: "1001010rrrrr0000", words: 1, isa: ISA_1200, base: 0x9400 },
    Insn { name: "dec", ops: "r", bits: "1001010rrrrr1010", words: 1, isa: ISA_1200, base: 0x940a },
    Insn { name: "inc", ops: "r", bits: "1001010rrrrr0011", words: 1, isa: ISA_1200, base: 0x9403 },
    Insn { name: "lsr", ops: "r", bits: "1001010rrrrr0110", words: 1, isa: ISA_1200, base: 0x9406 },
    Insn { name: "neg", ops: "r", bits: "1001010rrrrr0001", words: 1, isa: ISA_1200, base: 0x9401 },
    Insn { name: "pop", ops: "r", bits: "1001000rrrrr1111", words: 1, isa: ISA_2XXXA, base: 0x900f },
    Insn { name: "push", ops: "r", bits: "1001001rrrrr1111", words: 1, isa: ISA_2XXXA, base: 0x920f },
    Insn { name: "ror", ops: "r", bits: "1001010rrrrr0111", words: 1, isa: ISA_1200, base: 0x9407 },
    Insn { name: "swap", ops: "r", bits: "1001010rrrrr0010", words: 1, isa: ISA_1200, base: 0x9402 },
    Insn { name: "xch", ops: "z,r", bits: "1001001rrrrr0100", words: 1, isa: ISA_RMW, base: 0x9204 },
    Insn { name: "las", ops: "z,r", bits: "1001001rrrrr0101", words: 1, isa: ISA_RMW, base: 0x9205 },
    Insn { name: "lac", ops: "z,r", bits: "1001001rrrrr0110", words: 1, isa: ISA_RMW, base: 0x9206 },
    Insn { name: "lat", ops: "z,r", bits: "1001001rrrrr0111", words: 1, isa: ISA_RMW, base: 0x9207 },
    Insn { name: "movw", ops: "v,v", bits: "00000001ddddrrrr", words: 1, isa: ISA_MOVW, base: 0x0100 },
    Insn { name: "muls", ops: "d,d", bits: "00000010ddddrrrr", words: 1, isa: ISA_MUL, base: 0x0200 },
    Insn { name: "mulsu", ops: "a,a", bits: "000000110ddd0rrr", words: 1, isa: ISA_MUL, base: 0x0300 },
    Insn { name: "fmul", ops: "a,a", bits: "000000110ddd1rrr", words: 1, isa: ISA_MUL, base: 0x0308 },
    Insn { name: "fmuls", ops: "a,a", bits: "000000111ddd0rrr", words: 1, isa: ISA_MUL, base: 0x0380 },
    Insn { name: "fmulsu", ops: "a,a", bits: "000000111ddd1rrr", words: 1, isa: ISA_MUL, base: 0x0388 },
    Insn { name: "sts", ops: "j,d", bits: "10101kkkddddkkkk", words: 1, isa: ISA_TINY, base: 0xA800 },
    Insn { name: "sts", ops: "i,r", bits: "1001001ddddd0000", words: 2, isa: ISA_2XXX, base: 0x9200 },
    Insn { name: "lds", ops: "d,j", bits: "10100kkkddddkkkk", words: 1, isa: ISA_TINY, base: 0xA000 },
    Insn { name: "lds", ops: "r,i", bits: "1001000ddddd0000", words: 2, isa: ISA_2XXX, base: 0x9000 },
    Insn { name: "ldd", ops: "r,b", bits: "10o0oo0dddddbooo", words: 1, isa: ISA_2XXX, base: 0x8000 },
    Insn { name: "ld", ops: "r,e", bits: "100!000dddddee-+", words: 1, isa: ISA_1200, base: 0x8000 },
    Insn { name: "std", ops: "b,r", bits: "10o0oo1rrrrrbooo", words: 1, isa: ISA_2XXX, base: 0x8200 },
    Insn { name: "st", ops: "e,r", bits: "100!001rrrrree-+", words: 1, isa: ISA_1200, base: 0x8200 },
    Insn { name: "eicall", ops: "", bits: "1001010100011001", words: 1, isa: ISA_EIND, base: 0x9519 },
    Insn { name: "eijmp", ops: "", bits: "1001010000011001", words: 1, isa: ISA_EIND, base: 0x9419 },
    Insn { name: "des", ops: "E", bits: "10010100EEEE1011", words: 1, isa: ISA_DES, base: 0x940B },
];
