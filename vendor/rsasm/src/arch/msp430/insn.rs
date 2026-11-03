//! The instruction set.
//!
//! # Where the encodings come from
//!
//! The opcode table below is `msp430_opcodes` from
//! `include/opcode/msp430.h` (binutils 2.47), and the encoder follows
//! `msp430_operands` in `gas/config/tc-msp430.c` case for case. Every byte
//! was checked against `msp430-elf-as`; the corpora in
//! `tools/xas-diff/msp430.txt` and `msp430x.txt` hold a case for each form.
//!
//! # How the opcode space is laid out
//!
//! There are only three real instruction formats, and 24 of the 27
//! instructions people write are one of them:
//!
//! * **Two operands**, `0oooSSSS AdBASSSS`: the opcode in bits 15 to 12, the
//!   source register in 11 to 8, the destination's mode in 7, the byte flag
//!   in 6, the source's mode in 5 to 4, and the destination register in 3
//!   to 0.
//! * **One operand**, `000100oo obADDDDD`: `mov`'s neighbours `rrc`, `swpb`,
//!   `rra`, `sxt`, `push`, `call` and `reti`.
//! * **A jump**, `001ccDDD DDDDDDDD`: a condition and ten bits of signed
//!   word displacement.
//!
//! Everything else people write is an emulated instruction: `ret` is
//! `mov @sp+, pc`, `clr x` is `mov #0, x` through a constant generator, and
//! so on. The reference keeps them in the same table with a format of its
//! own, and so does this.
//!
//! The MSP430X adds a 20-bit address space and reaches it two ways: an
//! *extension word* in front of an ordinary instruction (`movx`, `addx`,
//! …), which carries the top four bits of each operand and a repeat count,
//! and a handful of *address instructions* (`mova`, `calla`, `adda`) in the
//! gaps of the one-operand opcode space.

use super::encode::Enc;
use super::operand::{self, Mode, Operand, Part, Rules};
use super::reg;
use super::reloc::{self, Bfd};
use super::{Isa, repeat_of, with_repeat};
use crate::arch::{AsmCtx, InsnRequest};
use crate::expr::ExprKind;
use crate::lexer::{Punct, Token};
use crate::section::{FixupKind, LinkValue, Variant};
use crate::source::Span;

/// One entry of the reference's `msp430_opcodes` table.
pub struct Opcode {
    pub name: &'static str,
    /// The instruction format. A negative one is an MSP430X instruction:
    /// −1 is an emulated or address instruction, −2 a two-operand one and
    /// −3 a one-operand one.
    pub fmt: i8,
    /// How many operands the instruction takes, or, for format 0 and the
    /// MSP430X address instructions, which encoder to use.
    pub opnumb: u8,
    pub bin: u16,
}

const fn insn(name: &'static str, fmt: i8, opnumb: u8, bin: u16) -> Opcode {
    Opcode {
        name,
        fmt,
        opnumb,
        bin,
    }
}

/// `msp430_opcodes`, in its own order: a name that appears twice resolves to
/// the first entry, as GNU as's hash table does.
pub const OPCODES: &[Opcode] = &[
    insn("and", 1, 2, 0xf000),
    insn("inv", 0, 1, 0xe330),
    insn("xor", 1, 2, 0xe000),
    insn("setz", 0, 0, 0xd322),
    insn("setc", 0, 0, 0xd312),
    insn("eint", 0, 0, 0xd232),
    insn("setn", 0, 0, 0xd222),
    insn("bis", 1, 2, 0xd000),
    insn("clrz", 0, 0, 0xc322),
    insn("clrc", 0, 0, 0xc312),
    insn("dint", 0, 0, 0xc232),
    insn("clrn", 0, 0, 0xc222),
    insn("bic", 1, 2, 0xc000),
    insn("bit", 1, 2, 0xb000),
    insn("dadc", 0, 1, 0xa300),
    insn("dadd", 1, 2, 0xa000),
    insn("tst", 0, 1, 0x9300),
    insn("cmp", 1, 2, 0x9000),
    insn("decd", 0, 1, 0x8320),
    insn("dec", 0, 1, 0x8310),
    insn("sub", 1, 2, 0x8000),
    insn("sbc", 0, 1, 0x7300),
    insn("subc", 1, 2, 0x7000),
    insn("adc", 0, 1, 0x6300),
    insn("rlc", 0, 2, 0x6000),
    insn("addc", 1, 2, 0x6000),
    insn("incd", 0, 1, 0x5320),
    insn("inc", 0, 1, 0x5310),
    insn("rla", 0, 2, 0x5000),
    insn("add", 1, 2, 0x5000),
    insn("nop", 0, 0, 0x4303),
    insn("clr", 0, 1, 0x4300),
    insn("ret", 0, 0, 0x4130),
    insn("pop", 0, 1, 0x4130),
    insn("br", 0, 3, 0x4000),
    insn("mov", 1, 2, 0x4000),
    insn("jmp", 3, 1, 0x3c00),
    insn("jl", 3, 1, 0x3800),
    insn("jge", 3, 1, 0x3400),
    insn("jn", 3, 1, 0x3000),
    insn("jc", 3, 1, 0x2c00),
    insn("jhs", 3, 1, 0x2c00),
    insn("jnc", 3, 1, 0x2800),
    insn("jlo", 3, 1, 0x2800),
    insn("jz", 3, 1, 0x2400),
    insn("jeq", 3, 1, 0x2400),
    insn("jnz", 3, 1, 0x2000),
    insn("jne", 3, 1, 0x2000),
    insn("reti", 2, 0, 0x1300),
    insn("call", 2, 1, 0x1280),
    insn("push", 2, 1, 0x1200),
    insn("sxt", 2, 1, 0x1180),
    insn("rra", 2, 1, 0x1100),
    insn("swpb", 2, 1, 0x1080),
    insn("rrc", 2, 1, 0x1000),
    // Simple polymorphs: one conditional jump, or the opposite jump over a
    // long branch.
    insn("beq", 4, 0, 0),
    insn("bne", 4, 1, 0),
    insn("blt", 4, 2, 0),
    insn("bltu", 4, 3, 0),
    insn("bge", 4, 4, 0),
    insn("bgeu", 4, 5, 0),
    insn("bltn", 4, 6, 0),
    insn("jump", 4, 7, 0),
    // Long polymorphs: conditions the hardware has no single jump for.
    insn("bgt", 5, 0, 0),
    insn("bgtu", 5, 1, 0),
    insn("bleu", 5, 2, 0),
    insn("ble", 5, 3, 0),
    // MSP430X two-operand instructions, with an extension word.
    insn("addcx", -2, 2, 0x6000),
    insn("addx", -2, 2, 0x5000),
    insn("andx", -2, 2, 0xf000),
    insn("bicx", -2, 2, 0xc000),
    insn("bisx", -2, 2, 0xd000),
    insn("bitx", -2, 2, 0xb000),
    insn("cmpx", -2, 2, 0x9000),
    insn("daddx", -2, 2, 0xa000),
    insn("movx", -2, 2, 0x4000),
    insn("subcx", -2, 2, 0x7000),
    insn("subx", -2, 2, 0x8000),
    insn("xorx", -2, 2, 0xe000),
    // MSP430X emulated instructions.
    insn("adcx", -1, 1, 0x6300),
    insn("clra", -1, 1, 0x4300),
    insn("clrx", -1, 1, 0x4300),
    insn("dadcx", -1, 1, 0xa300),
    insn("decx", -1, 1, 0x8310),
    insn("decda", -1, 1, 0x8320),
    insn("decdx", -1, 1, 0x8320),
    insn("incx", -1, 1, 0x5310),
    insn("incda", -1, 1, 0x5320),
    insn("incdx", -1, 1, 0x5320),
    insn("invx", -1, 1, 0xe330),
    insn("popx", -1, 1, 0x4130),
    insn("rlax", -1, 2, 0x5000),
    insn("rlcx", -1, 2, 0x6000),
    insn("sbcx", -1, 1, 0x7300),
    insn("tsta", -1, 1, 0x9300),
    insn("tstx", -1, 1, 0x9300),
    // MSP430X one-operand instructions.
    insn("pushx", -3, 1, 0x1200),
    insn("rrax", -3, 1, 0x1100),
    insn("rrcx", -3, 1, 0x1000),
    insn("rrux", -3, 1, 0x1000),
    insn("swpbx", -3, 1, 0x1080),
    insn("sxtx", -3, 1, 0x1180),
    // MSP430X address instructions, which need no extension word.
    insn("calla", -1, 4, 0x1300),
    insn("popm", -1, 5, 0x1600),
    insn("pushm", -1, 5, 0x1400),
    insn("rrcm", -1, 6, 0x0040),
    insn("rram", -1, 6, 0x0140),
    insn("rlam", -1, 6, 0x0240),
    insn("rrum", -1, 6, 0x0340),
    insn("adda", -1, 8, 0x00a0),
    insn("cmpa", -1, 8, 0x0090),
    insn("suba", -1, 8, 0x00b0),
    insn("reta", -1, 9, 0x0110),
    insn("bra", -1, 9, 0x0000),
    insn("mova", -1, 9, 0x0000),
    // The repeat prefix, which sets a field of the next extension word.
    insn("rpt", -1, 10, 0x0000),
];

/// The long forms of the simple polymorphs, from `msp430_rcodes`: the words
/// the reference's `md_convert_frag` writes when it gives up on the short
/// jump, the last of which is the 430's `br` (`mov x(pc), pc`). A zero word
/// is not written; the branch target's word is added by the encoder.
const RCODES: [[u16; 3]; 8] = [
    [0x2002, 0x4010, 0],      // beq: jne +4; br lab
    [0x2402, 0x4010, 0],      // bne
    [0x3402, 0x4010, 0],      // blt
    [0x2c02, 0x4010, 0],      // bltu
    [0x3802, 0x4010, 0],      // bge
    [0x2802, 0x4010, 0],      // bgeu
    [0x3001, 0x3c02, 0x4010], // bltn: jn +2; jmp +4; br lab
    [0x4010, 0, 0],           // jump: br lab
];

/// `msp430x_rcodes`: the same, with the 430X's `bra` (`mova x(pc), pc`).
///
/// The reference's table puts the three words of the 430X `bltn` in an order
/// that is not the 430's, and not a branch; it is reproduced because it is
/// what `msp430-elf-as` emits.
const RCODES_X: [[u16; 3]; 8] = [
    [0x2002, 0x0030, 0],
    [0x2402, 0x0030, 0],
    [0x3402, 0x0030, 0],
    [0x2c02, 0x0030, 0],
    [0x3802, 0x0030, 0],
    [0x2802, 0x0030, 0],
    [0x0031, 0x3c02, 0x3000],
    [0x0030, 0, 0],
];

/// `msp430_hcodes` and `msp430x_hcodes`, again in the long form: two jumps
/// and a branch. The last word is the 430's `br` or the 430X's `bra`.
const HCODES: [[u16; 3]; 4] = [
    [0x2403, 0x3802, 0x4010], // bgt
    [0x2403, 0x2802, 0x4010], // bgtu
    [0x2401, 0x2c02, 0x4010], // bleu
    [0x2401, 0x3402, 0x4010], // ble
];

/// The branch word each long polymorph ends with, by ISA.
fn branch_word(isa: Isa) -> u16 {
    if isa.is_430x() { 0x0030 } else { 0x4010 }
}

/// Looks a mnemonic up, as GNU as's hash table does: the first entry wins.
pub fn find(name: &str) -> Option<&'static Opcode> {
    OPCODES.iter().find(|o| o.name == name)
}

/// Whether `name`, lowercased, is one of this backend's mnemonics.
pub fn is_mnemonic(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name);
    find(stem).is_some()
}

/// The state one instruction is encoded in, which is what the reference
/// keeps in local variables of `msp430_operands`.
struct Ctx<'a, 'b> {
    cx: &'a mut AsmCtx<'b>,
    isa: Isa,
    /// The mnemonic as the table spells it, for diagnostics.
    name: &'static str,
    span: Span,
    /// A `.b` suffix.
    byte_op: bool,
    /// A `.a` suffix that has not been turned into another mnemonic.
    addr_op: bool,
    extended_op: bool,
    /// The extension word being built, `0x1800` plus whatever the operands
    /// and a pending `rpt` put in it.
    extended: u16,
    /// `imm_op` in the reference: set by a `#` operand, and by anything that
    /// reaches its indexed-addressing check, which is everything but `&addr`
    /// and `@rN`. It decides some relocation types.
    imm_op: bool,
}

impl Ctx<'_, '_> {
    /// `CHECK_RELOC_MSP430`: the relocation of an absolute operand word.
    fn abs_reloc(&mut self, op: &Operand) -> Option<u32> {
        let bfd = if self.isa.is_430x() {
            match (op.part, op.vshift) {
                (Part::All, _) => Bfd::Abs16,
                (_, 1) => Bfd::Hi16,
                _ => Bfd::Data16,
            }
        } else if self.imm_op || self.byte_op {
            Bfd::Data16
        } else {
            Bfd::Insn16
        };
        self.reloc(bfd)
    }

    /// `CHECK_RELOC_MSP430_PCREL`: the relocation of a symbolic operand word.
    fn pcrel_reloc(&mut self) -> Option<u32> {
        let bfd = if self.isa.is_430x() {
            Bfd::Pcr16
        } else if self.imm_op || self.byte_op {
            Bfd::Pcrel16
        } else {
            Bfd::Insn16Pcrel
        };
        self.reloc(bfd)
    }

    fn reloc(&mut self, bfd: Bfd) -> Option<u32> {
        match reloc::number(self.isa, bfd) {
            Some(n) => Some(n),
            None => {
                self.cx.error(
                    self.span,
                    format!("`{}` has no relocation on this MSP430 variant", self.name),
                );
                None
            }
        }
    }

    /// The fixup kind an absolute operand word gets, including the way
    /// `#hi()` and `#lo()` shift the value.
    fn abs_kind(&mut self, op: &Operand) -> Option<FixupKind> {
        let r = self.abs_reloc(op)?;
        Some(match op.part {
            Part::All => reloc::word16(r),
            // On the 430X `#hi(sym)` gets its own relocation; on the 430 the
            // reference has none for it and writes the low word, which the
            // linker then relocates as if `#lo()` had been written.
            Part::Hi if self.isa.is_430x() => reloc::hi16(r),
            _ => reloc::word16(r).link(LinkValue::Split(|v| v & 0xffff)),
        })
    }

    fn error(&mut self, msg: impl Into<String>) {
        let span = self.span;
        self.cx.error(span, msg);
    }
}

/// Assembles one instruction.
pub fn assemble(
    cx: &mut AsmCtx<'_>,
    req: &InsnRequest<'_>,
    mnemonic: &str,
    isa: Isa,
) -> Option<Vec<Variant>> {
    let (stem, suffix) = match mnemonic.split_once('.') {
        Some((s, sfx)) => (s, Some(sfx)),
        None => (mnemonic, None),
    };
    let Some(mut opcode) = find(stem) else {
        cx.error(req.mnemonic_span, format!("unknown instruction `{stem}`"));
        return None;
    };

    let mut bin = opcode.bin;
    let mut byte_op = false;
    let mut addr_op = false;
    match suffix {
        None | Some("") | Some("w") | Some("W") => {}
        Some("b") | Some("B") => {
            bin |= BYTE_OPERATION;
            byte_op = true;
        }
        Some("a") | Some("A") => {
            addr_op = true;
            bin |= BYTE_OPERATION;
        }
        Some(other) => {
            cx.error(
                req.mnemonic_span,
                format!("unrecognised instruction size modifier `.{other}`"),
            );
            return None;
        }
    }

    // `.a` on an instruction that has no address form names another
    // instruction: `mov.a` is `mova`, `tst.a` is `tsta`. Other MSP430
    // assemblers accept this spelling, so the reference does too.
    if addr_op && opcode.fmt >= 0 {
        let real = format!("{stem}a");
        let Some(found) = find(&real) else {
            cx.error(
                req.mnemonic_span,
                format!("instruction `{stem}.a` does not exist"),
            );
            return None;
        };
        opcode = found;
        addr_op = false;
        bin = opcode.bin;
    }

    let mut fmt = opcode.fmt;
    let mut extended_op = false;
    if fmt < 0 {
        if !isa.is_430x() {
            cx.error(
                req.mnemonic_span,
                format!("instruction `{}` needs an MSP430X CPU", opcode.name),
            );
            return None;
        }
        fmt = -fmt - 1;
        extended_op = true;
    }

    let ops = operand::split(req.operands);
    let ops: Vec<&[Token]> = ops.into_iter().filter(|p| !p.is_empty()).collect();
    if opcode.fmt != -1 && opcode.opnumb != 0 && ops.is_empty() {
        cx.error(
            req.span,
            format!(
                "instruction `{}` requires {} operand{}",
                opcode.name,
                opcode.opnumb,
                if opcode.opnumb == 1 { "" } else { "s" }
            ),
        );
        return None;
    }

    // A pending `rpt` sets the repeat field of this instruction's extension
    // word, and is consumed whether or not it can be used.
    let mut extended = 0x1800u16;
    match repeat_of(cx.state) {
        0 => {}
        n if extended_op => {
            if n > 0 {
                extended |= (n - 1) as u16;
            } else {
                extended |= (1 << 7) | (-n) as u16;
            }
        }
        _ => cx.error(req.span, format!("`{}` cannot be repeated", opcode.name)),
    }
    cx.state.private = with_repeat(cx.state, 0);

    let mut c = Ctx {
        cx,
        isa,
        name: opcode.name,
        span: req.span,
        byte_op,
        addr_op,
        extended_op,
        extended,
        imm_op: false,
    };

    match (fmt, opcode.opnumb) {
        (0, 0) => implied(&mut c, bin, &ops),
        (0, 1) => single_dst(&mut c, opcode, bin, &ops),
        (0, 2) => shift(&mut c, bin, &ops),
        (0, 3) => branch(&mut c, bin, &ops),
        (0, 4) => calla(&mut c, bin, &ops),
        (0, 5) => pushm(&mut c, opcode, &ops),
        (0, 6) => rotate(&mut c, opcode, &ops),
        (0, 8) => adda(&mut c, opcode, &ops),
        (0, 9) => mova(&mut c, opcode, &ops),
        (0, 10) => rpt(&mut c, &ops),
        (1, _) => two_operand(&mut c, opcode, bin, &ops),
        (2, 0) => implied(&mut c, bin, &ops),
        (2, _) => one_operand(&mut c, opcode, bin, &ops),
        (3, _) => jump(&mut c, bin, &ops),
        (4, _) => polymorph(&mut c, opcode.opnumb, &ops),
        (5, _) => polymorph_long(&mut c, opcode.opnumb, &ops),
        _ => {
            c.error("this instruction is not implemented");
            None
        }
    }
}

/// The byte-operation flag, which is also the extension word's A/L bit.
const BYTE_OPERATION: u16 = 1 << 6;
/// The extension word's Z/C bit, which `rrux` sets.
const IGNORE_CARRY_BIT: u16 = 1 << 8;

fn want(c: &mut Ctx<'_, '_>, ops: &[&[Token]], n: usize) -> bool {
    if ops.len() == n {
        return true;
    }
    let name = c.name;
    c.error(format!(
        "`{name}` takes {n} operand{}, not {}",
        if n == 1 { "" } else { "s" },
        ops.len()
    ));
    false
}

fn rules(c: &Ctx<'_, '_>, opcode: &Opcode, constants: bool) -> Rules {
    Rules {
        wide: c.extended_op,
        constants,
        // Silicon erratum CPU4: the original MSP430 decodes neither of
        // `push`'s short constant forms.
        push: opcode.bin == 0x1200 && !c.isa.is_430x(),
        xv2: c.isa == Isa::Msp430Xv2,
    }
}

/// An instruction with no operands.
fn implied(c: &mut Ctx<'_, '_>, bin: u16, ops: &[&[Token]]) -> Option<Vec<Variant>> {
    if !want(c, ops, 0) {
        return None;
    }
    let mut e = Enc::new();
    if c.extended_op {
        if !c.addr_op {
            c.extended |= BYTE_OPERATION;
        }
        e.word(c.extended);
    }
    e.word(bin);
    e.done()
}

/// Writes the operand word of an operand that needs one, and its fixup.
///
/// `pcrel` says the operand is symbolic, so the word holds a distance from
/// itself. An extended instruction leaves the word to the relocation on the
/// extension word instead, which is why `plain` is false there.
fn operand_word(c: &mut Ctx<'_, '_>, e: &mut Enc, op: &Operand, plain: bool, dst: bool) {
    let Some(x) = op.x else { return };
    if let Some(v) = op.value {
        e.word((v & 0xffff) as u16);
        return;
    }
    let at = e.word(0);
    if !plain {
        return;
    }
    // The source of a two-operand instruction reaches the PC-relative form
    // through mode 1 only; mode 3 with the PC is `#imm`, which is absolute.
    let symbolic = op.reg == reg::PC && (dst || op.am != 3);
    let kind = if symbolic {
        match c.pcrel_reloc() {
            Some(r) => reloc::pcrel16(r),
            None => return,
        }
    } else {
        match c.abs_kind(op) {
            Some(k) => k,
            None => return,
        }
    };
    e.fixup(at, x, kind);
}

/// The extension word's contribution from one operand: four more bits of
/// value, or a relocation over the whole 20-bit field.
struct ExtFixup {
    x: operand::Expr,
    kind: FixupKind,
}

fn ext_bits(c: &mut Ctx<'_, '_>, op: &Operand, src: bool, odst: bool) -> Option<ExtFixup> {
    let x = op.x?;
    if let Some(v) = op.value {
        let nibble = ((v >> 16) & 0xf) as u16;
        c.extended |= if src { nibble << 7 } else { nibble };
        return None;
    }
    // The PC-relative form is mode 1 through the PC; a source in mode 3 is
    // an immediate, whose value is absolute.
    let pcrel = op.reg == reg::PC && (!src || op.am != 3);
    let bfd = match (src, odst, pcrel) {
        (true, _, false) => Bfd::Abs20ExtSrc,
        (true, _, true) => Bfd::Pcr20ExtSrc,
        (false, false, false) => Bfd::Abs20ExtDst,
        (false, false, true) => Bfd::Pcr20ExtDst,
        (false, true, false) => Bfd::Abs20ExtOdst,
        (false, true, true) => Bfd::Pcr20ExtOdst,
    };
    let r = c.reloc(bfd)?;
    let kind = match (src, odst) {
        (true, _) => reloc::ext_src(r, pcrel),
        (false, false) => reloc::ext_dst(r, pcrel),
        (false, true) => reloc::ext_odst(r, pcrel),
    };
    Some(ExtFixup { x, kind })
}

/// An emulated instruction with one destination: `clr`, `inc`, `tst`, `pop`.
fn single_dst(
    c: &mut Ctx<'_, '_>,
    opcode: &Opcode,
    mut bin: u16,
    ops: &[&[Token]],
) -> Option<Vec<Variant>> {
    if !want(c, ops, 1) {
        return None;
    }
    let r = rules(c, opcode, true);
    let op1 = operand::dst(c.cx, ops[0], c.span, r)?;
    bin |= op1.reg as u16 | ((op1.am as u16) << 7);

    let mut e = Enc::new();
    let mut ext = None;
    if c.extended_op {
        if !c.addr_op {
            c.extended |= BYTE_OPERATION;
        }
        if op1.ol != 0 && (c.extended & 0xf) != 0 {
            c.error("a repeat count only applies to a register-mode instruction");
            c.extended &= !0xf;
        }
        ext = ext_bits(c, &op1, true, false);
        e.word(c.extended);
    }
    e.word(bin);
    operand_word(c, &mut e, &op1, !c.extended_op, true);
    if let Some(f) = ext {
        e.fixup(0, f.x, f.kind);
    }
    e.done()
}

/// `rla` and `rlc`, which are two-operand instructions with the same operand
/// twice.
fn shift(c: &mut Ctx<'_, '_>, mut bin: u16, ops: &[&[Token]]) -> Option<Vec<Variant>> {
    if !want(c, ops, 1) {
        return None;
    }
    let r = Rules {
        wide: c.extended_op,
        constants: true,
        push: false,
        xv2: c.isa == Isa::Msp430Xv2,
    };
    let mut imm = false;
    let op1 = operand::src(c.cx, ops[0], c.span, r, &mut imm)?;
    let op2 = operand::dst(c.cx, ops[0], c.span, r)?;
    c.imm_op = imm;

    if c.isa == Isa::Msp430Xv2 && op1.mode == Mode::Reg && op1.reg == reg::PC {
        c.error("the PC cannot be rotated");
        return None;
    }

    let mut e = Enc::new();
    let (mut fsrc, mut fdst) = (None, None);
    if c.extended_op {
        if !c.addr_op {
            c.extended |= BYTE_OPERATION;
        }
        if (op1.ol != 0 || op2.ol != 0) && (c.extended & 0xf) != 0 {
            c.error("a repeat count only applies to a register-mode instruction");
            c.extended &= !0xf;
        }
        fsrc = ext_bits(c, &op1, true, false);
        fdst = ext_bits(c, &op2, false, op1.mode == Mode::Exp);
        e.word(c.extended);
    }
    bin |=
        op2.reg as u16 | ((op1.reg as u16) << 8) | ((op1.am as u16) << 4) | ((op2.am as u16) << 7);
    e.word(bin);
    operand_word(c, &mut e, &op1, !c.extended_op, false);
    operand_word(c, &mut e, &op2, !c.extended_op, true);
    for f in [fsrc, fdst].into_iter().flatten() {
        e.fixup(0, f.x, f.kind);
    }
    e.done()
}

/// `br`, which is `mov src, pc`.
fn branch(c: &mut Ctx<'_, '_>, mut bin: u16, ops: &[&[Token]]) -> Option<Vec<Variant>> {
    if !want(c, ops, 1) {
        return None;
    }
    let r = Rules {
        wide: false,
        constants: false,
        push: false,
        xv2: c.isa == Isa::Msp430Xv2,
    };
    let mut imm = false;
    let op1 = operand::src(c.cx, ops[0], c.span, r, &mut imm)?;
    // The reference forgets both flags again before choosing the relocation.
    c.byte_op = false;
    c.imm_op = false;
    bin |= ((op1.reg as u16) << 8) | ((op1.am as u16) << 4);

    let mut e = Enc::new();
    e.word(bin);
    if let Some(x) = op1.x {
        match op1.value {
            Some(v) => {
                e.word((v & 0xffff) as u16);
            }
            None => {
                let at = e.word(0);
                let kind = if op1.reg != reg::PC || op1.am == 3 {
                    reloc::word16(c.abs_reloc(&op1)?)
                } else {
                    reloc::pcrel16(c.pcrel_reloc()?)
                };
                e.fixup(at, x, kind);
            }
        }
    }
    e.done()
}

/// `calla`, whose operand chooses between six encodings.
fn calla(c: &mut Ctx<'_, '_>, mut bin: u16, ops: &[&[Token]]) -> Option<Vec<Variant>> {
    if !want(c, ops, 1) {
        return None;
    }
    let r = Rules {
        wide: true,
        constants: false,
        push: false,
        xv2: c.isa == Isa::Msp430Xv2,
    };
    let mut imm = false;
    let op1 = operand::src(c.cx, ops[0], c.span, r, &mut imm)?;
    c.imm_op = imm;

    // Which of the two 20-bit relocations, if either, covers the whole
    // instruction rather than just its second word.
    let mut wide: Option<Bfd> = None;
    if imm {
        match op1.am {
            3 => {
                bin |= 0xb0;
                wide = Some(Bfd::Abs20AdrDst);
            }
            1 if op1.reg == reg::PC => {
                bin |= 0x90;
                wide = Some(Bfd::Pcr20Call);
            }
            1 => bin |= 0x50 | op1.reg as u16,
            0 => bin |= 0x40 | op1.reg as u16,
            // `0(rN)`, which the operand parser has made `@rN`, reaches none
            // of the reference's cases and keeps the bare opcode.
            _ => {}
        }
    } else {
        match op1.am {
            1 => {
                bin |= 0x80;
                wide = Some(Bfd::Abs20AdrDst);
            }
            2 => bin |= 0x60 | op1.reg as u16,
            _ => bin |= 0x70 | op1.reg as u16,
        }
    }

    let mut e = Enc::new();
    e.word(bin);
    let Some(mut x) = op1.x else {
        return e.done();
    };
    // Unlike every other instruction, `calla` relocates its operand even
    // when it is a number, against no symbol; the linker fills it in. The
    // number is the one `#hi()` or `#lo()` has already extracted.
    if let Some(v) = op1.value {
        let n = c.cx.exprs.alloc(ExprKind::Int(v.unsigned_abs()), x.span);
        x.e = if v < 0 {
            c.cx.exprs
                .alloc(ExprKind::Unary(crate::expr::UnOp::Neg, n), x.span)
        } else {
            n
        };
    }
    e.word(0);
    match wide {
        Some(bfd @ Bfd::Pcr20Call) => {
            let r = c.reloc(bfd)?;
            e.fixup(0, x, reloc::adr_dst(r, true));
        }
        Some(bfd) => {
            let r = c.reloc(bfd)?;
            e.fixup(0, x, reloc::adr_dst(r, false).relocated_in_objects());
        }
        None => {
            let r = c.reloc(Bfd::Data16)?;
            e.fixup(1, x, reloc::word16(r).relocated_in_objects());
        }
    }
    e.done()
}

/// `pushm` and `popm`.
fn pushm(c: &mut Ctx<'_, '_>, opcode: &Opcode, ops: &[&[Token]]) -> Option<Vec<Variant>> {
    if !want(c, ops, 2) {
        return None;
    }
    let n = hash_constant(c, ops[0], "the register count", 1, 16)?;
    let Some(r) = single_reg(c, ops[1]) else {
        c.error("expected a register as the second operand");
        return None;
    };
    let mut bin = opcode.bin;
    if !c.addr_op {
        bin |= 0x100;
    }
    bin |= ((n - 1) as u16) << 4;
    if opcode.name == "pushm" {
        bin |= r as u16;
    } else {
        if (r as i64) - n + 1 < 0 {
            c.error("too many registers popped");
            return None;
        }
        // Silicon erratum CPU21: `popm` cannot restore the status register.
        if c.isa == Isa::Msp430Xv2 && (r as i64) - n + 1 < 3 && r >= reg::SR {
            c.error("`popm` cannot restore the SR register");
            return None;
        }
        bin |= (r as i64 - n + 1) as u16;
    }
    let mut e = Enc::new();
    e.word(bin);
    e.done()
}

/// `rrcm`, `rram`, `rlam` and `rrum`, which rotate a register one to four
/// places.
fn rotate(c: &mut Ctx<'_, '_>, opcode: &Opcode, ops: &[&[Token]]) -> Option<Vec<Variant>> {
    if (c.extended & 0xff) != 0 {
        let name = c.name;
        c.error(format!("a repeat count cannot be used with `{name}`"));
        return None;
    }
    if !want(c, ops, 2) {
        return None;
    }
    let n = hash_constant(c, ops[0], "the rotate count", 1, 4)?;
    let Some(r) = single_reg(c, ops[1]) else {
        c.error("expected a register as the second operand");
        return None;
    };
    if c.isa == Isa::Msp430Xv2 && r == reg::PC {
        c.error("the PC cannot be rotated");
        return None;
    }
    let mut bin = opcode.bin;
    if !c.addr_op {
        bin |= 0x10;
    }
    bin |= ((n - 1) as u16) << 10;
    bin |= r as u16;
    let mut e = Enc::new();
    e.word(bin);
    e.done()
}

/// `adda`, `cmpa` and `suba`.
fn adda(c: &mut Ctx<'_, '_>, opcode: &Opcode, ops: &[&[Token]]) -> Option<Vec<Variant>> {
    if (c.extended & 0xff) != 0 {
        let name = c.name;
        c.error(format!("a repeat count cannot be used with `{name}`"));
        return None;
    }
    if !want(c, ops, 2) {
        return None;
    }
    let mut bin = opcode.bin;
    let mut immediate = None;
    if ops[0].first().is_some_and(|t| t.is_punct(Punct::Hash)) {
        let x = expr_of(c, &ops[0][1..])?;
        match operand::known(c.cx, x.e) {
            Some(v) => {
                if !(-0x80000..=0xfffff).contains(&v) {
                    c.error(format!("value {v:#x} does not fit in 20 bits"));
                    return None;
                }
                bin |= (((v >> 16) & 0xf) as u16) << 8;
                immediate = Some((x, Some(v)));
            }
            None => immediate = Some((x, None)),
        }
    } else {
        let Some(n) = single_reg(c, ops[0]) else {
            c.error("expected a register or an immediate as the first operand");
            return None;
        };
        bin |= ((n as u16) << 8) | (1 << 6);
    }
    let Some(r) = single_reg(c, ops[1]) else {
        c.error("expected a register as the second operand");
        return None;
    };
    bin |= r as u16;

    let mut e = Enc::new();
    e.word(bin);
    match immediate {
        None => {}
        Some((_, Some(v))) => {
            e.word((v & 0xffff) as u16);
        }
        Some((x, None)) => {
            e.word(0);
            let rel = c.reloc(Bfd::Abs20AdrSrc)?;
            e.fixup(0, x, reloc::adr_src(rel));
        }
    }
    e.done()
}

/// `mova`, `bra` and `reta`, which share one encoder because `bra` is
/// `mova src, pc` and `reta` is `mova @sp+, pc`.
fn mova(c: &mut Ctx<'_, '_>, opcode: &Opcode, ops: &[&[Token]]) -> Option<Vec<Variant>> {
    let r = Rules {
        wide: true,
        constants: false,
        push: false,
        xv2: c.isa == Isa::Msp430Xv2,
    };
    let (op1, op2) = match opcode.name {
        "reta" => {
            if !want(c, ops, 0) {
                return None;
            }
            (
                Operand {
                    am: 3,
                    reg: reg::SP,
                    ol: 0,
                    mode: Mode::Reg,
                    x: None,
                    value: None,
                    part: Part::All,
                    vshift: 0,
                    span: c.span,
                },
                Operand {
                    am: 0,
                    reg: reg::PC,
                    ol: 0,
                    mode: Mode::Reg,
                    x: None,
                    value: None,
                    part: Part::All,
                    vshift: 0,
                    span: c.span,
                },
            )
        }
        "bra" => {
            if !want(c, ops, 1) {
                return None;
            }
            let mut imm = false;
            let op1 = operand::src(c.cx, ops[0], c.span, r, &mut imm)?;
            c.imm_op = imm;
            (
                op1,
                Operand {
                    am: 0,
                    reg: reg::PC,
                    ol: 0,
                    mode: Mode::Reg,
                    x: None,
                    value: None,
                    part: Part::All,
                    vshift: 0,
                    span: c.span,
                },
            )
        }
        _ => {
            if !want(c, ops, 2) {
                return None;
            }
            let mut imm = false;
            let op1 = operand::src(c.cx, ops[0], c.span, r, &mut imm)?;
            c.imm_op = imm;
            let op2 = operand::dst(
                c.cx,
                ops[1],
                c.span,
                Rules {
                    constants: true,
                    ..r
                },
            )?;
            (op1, op2)
        }
    };
    encode_mova(c, opcode.bin, &op1, &op2)
}

/// `try_encode_mova`: the restricted set of addressing modes the address
/// instructions have.
fn encode_mova(
    c: &mut Ctx<'_, '_>,
    mut bin: u16,
    op1: &Operand,
    op2: &Operand,
) -> Option<Vec<Variant>> {
    let mut e = Enc::new();
    let name = c.name;
    if c.imm_op {
        if op1.mode == Mode::Exp {
            if op2.mode != Mode::Reg {
                c.error(format!("`{name}` needs a register as its second operand"));
                return None;
            }
            let x = op1.x.expect("an expression operand has an expression");
            if op1.am == 3 {
                // MOVA #imm20, Rdst
                bin |= 0x80 | op2.reg as u16;
                match op1.value {
                    Some(v) => {
                        e.word(bin | ((((v >> 16) & 0xf) as u16) << 8));
                        e.word((v & 0xffff) as u16);
                    }
                    None => {
                        e.word(bin);
                        e.word(0);
                        let r = c.reloc(Bfd::Abs20AdrSrc)?;
                        e.fixup(0, x, reloc::adr_src(r));
                    }
                }
                return e.done();
            }
            if op1.am == 1 {
                // MOVA z16(Rsrc), Rdst
                bin |= 0x30 | ((op1.reg as u16) << 8) | op2.reg as u16;
                e.word(bin);
                match op1.value {
                    Some(v) => {
                        if !(-0x7fff..=0xffff).contains(&v) {
                            c.error(format!("index {v:#x} is too big for `{name}`"));
                            return None;
                        }
                        e.word((v & 0xffff) as u16);
                    }
                    None => {
                        let at = e.word(0);
                        let kind = if op1.reg == reg::PC {
                            reloc::pcrel16(c.reloc(Bfd::Pcr16)?)
                        } else {
                            reloc::word16(c.reloc(Bfd::Abs16)?)
                        };
                        e.fixup(at, x, kind);
                    }
                }
                return e.done();
            }
            c.error(format!(
                "this addressing mode is not available for `{name}`"
            ));
            return None;
        }
        if op1.am == 0 {
            if op2.mode == Mode::Reg {
                // MOVA Rsrc, Rdst
                e.word(bin | 0xc0 | ((op1.reg as u16) << 8) | op2.reg as u16);
                return e.done();
            }
            if op2.am == 1 {
                let x = op2.x.expect("an expression operand has an expression");
                if op2.reg == reg::SR {
                    // MOVA Rsrc, &abs20
                    bin |= 0x60 | ((op1.reg as u16) << 8);
                    match op2.value {
                        Some(v) => {
                            e.word(bin | (((v >> 16) & 0xf) as u16));
                            e.word((v & 0xffff) as u16);
                        }
                        None => {
                            e.word(bin);
                            e.word(0);
                            let r = c.reloc(Bfd::Abs20AdrDst)?;
                            e.fixup(0, x, reloc::adr_dst(r, false));
                        }
                    }
                    return e.done();
                }
                // MOVA Rsrc, z16(Rdst)
                bin |= 0x70 | ((op1.reg as u16) << 8) | op2.reg as u16;
                e.word(bin);
                match op2.value {
                    Some(v) => {
                        if !(-0x7fff..=0xffff).contains(&v) {
                            c.error(format!("index {v:#x} is too big for `{name}`"));
                            return None;
                        }
                        e.word((v & 0xffff) as u16);
                    }
                    None => {
                        let at = e.word(0);
                        let kind = if op2.reg == reg::PC {
                            reloc::pcrel16(c.reloc(Bfd::Pcr16)?)
                        } else {
                            reloc::word16(c.reloc(Bfd::Abs16)?)
                        };
                        e.fixup(at, x, kind);
                    }
                }
                return e.done();
            }
            c.error(format!(
                "this addressing mode is not available for `{name}`"
            ));
            return None;
        }
    }

    if op1.reg == reg::SR && op1.am == 1 && op1.mode == Mode::Exp {
        // MOVA &abs20, Rdst
        if op2.mode != Mode::Reg {
            c.error(format!("`{name}` needs a register as its second operand"));
            return None;
        }
        if op2.reg == reg::SR || op2.reg == reg::CG {
            c.error(format!(
                "`{name}` cannot write to a constant generator register"
            ));
            return None;
        }
        let x = op1.x.expect("an expression operand has an expression");
        bin |= 0x20 | op2.reg as u16;
        match op1.value {
            Some(v) => {
                e.word(bin | ((((v >> 16) & 0xf) as u16) << 8));
                e.word((v & 0xffff) as u16);
            }
            None => {
                e.word(bin);
                e.word(0);
                let r = c.reloc(Bfd::Abs20AdrSrc)?;
                e.fixup(0, x, reloc::adr_src(r));
            }
        }
        return e.done();
    }
    if op1.mode == Mode::Reg && (op1.am == 2 || op1.am == 3) {
        // MOVA @Rsrc, Rdst and MOVA @Rsrc+, Rdst
        if op2.mode != Mode::Reg {
            c.error(format!("`{name}` needs a register as its second operand"));
            return None;
        }
        if op2.reg == reg::SR || op2.reg == reg::CG {
            c.error(format!(
                "`{name}` cannot write to a constant generator register"
            ));
            return None;
        }
        if op1.reg == reg::SR || op1.reg == reg::CG {
            c.error(format!(
                "`{name}` cannot read through a constant generator register"
            ));
            return None;
        }
        let mode = if op1.am == 3 { 0x10 } else { 0x00 };
        e.word(bin | mode | ((op1.reg as u16) << 8) | op2.reg as u16);
        return e.done();
    }
    c.error(format!(
        "this addressing mode is not available for `{name}`"
    ));
    None
}

/// `rpt`, which emits nothing and sets the repeat field of the next
/// extension word.
fn rpt(c: &mut Ctx<'_, '_>, ops: &[&[Token]]) -> Option<Vec<Variant>> {
    if !want(c, ops, 1) {
        return None;
    }
    let n = if ops[0].first().is_some_and(|t| t.is_punct(Punct::Hash)) {
        let x = expr_of(c, &ops[0][1..])?;
        let Some(v) = operand::known(c.cx, x.e) else {
            c.error("`rpt` needs a constant repeat count");
            return None;
        };
        if !(1..=16).contains(&v) {
            c.error(format!("repeat count {v} is out of range (2 to 16)"));
            return None;
        }
        // A count of one is accepted and means nothing.
        if v > 1 { v as i8 } else { 0 }
    } else {
        let Some(r) = single_reg(c, ops[0]) else {
            c.error("`rpt` needs a constant or a register");
            return None;
        };
        if r == reg::PC {
            // The reference warns and repeats nothing.
            let span = c.span;
            c.cx.diags
                .warning(span, "the PC cannot hold a repeat count; `rpt` ignored");
            0
        } else {
            -(r as i8)
        }
    };
    c.cx.state.private = with_repeat(c.cx.state, n);
    Some(vec![Variant::new(Vec::new())])
}

/// A two-operand instruction: `mov`, `add`, `cmp` and the rest.
fn two_operand(
    c: &mut Ctx<'_, '_>,
    opcode: &Opcode,
    mut bin: u16,
    ops: &[&[Token]],
) -> Option<Vec<Variant>> {
    if !want(c, ops, 2) {
        return None;
    }
    let r = rules(c, opcode, true);
    let mut imm = false;
    let op1 = operand::src(c.cx, ops[0], c.span, r, &mut imm)?;
    let op2 = operand::dst(c.cx, ops[1], c.span, r)?;
    c.imm_op = imm;

    bin |=
        op2.reg as u16 | ((op1.reg as u16) << 8) | ((op1.am as u16) << 4) | ((op2.am as u16) << 7);

    let mut e = Enc::new();
    let (mut fsrc, mut fdst) = (None, None);
    if c.extended_op {
        if !c.addr_op {
            c.extended |= BYTE_OPERATION;
        }
        if (op1.ol != 0 || op2.ol != 0) && (c.extended & 0xf) != 0 {
            c.error("a repeat count only applies to a register-mode instruction");
            c.extended &= !0xf;
        }
        fsrc = ext_bits(c, &op1, true, false);
        fdst = ext_bits(c, &op2, false, op1.mode == Mode::Exp);
        e.word(c.extended);
    }
    e.word(bin);
    operand_word(c, &mut e, &op1, !c.extended_op, false);
    operand_word(c, &mut e, &op2, !c.extended_op, true);
    for f in [fsrc, fdst].into_iter().flatten() {
        e.fixup(0, f.x, f.kind);
    }
    e.done()
}

/// A one-operand instruction: `rrc`, `swpb`, `rra`, `sxt`, `push`, `call`.
fn one_operand(
    c: &mut Ctx<'_, '_>,
    opcode: &Opcode,
    mut bin: u16,
    ops: &[&[Token]],
) -> Option<Vec<Variant>> {
    if !want(c, ops, 1) {
        return None;
    }
    let r = rules(c, opcode, true);
    let mut imm = false;
    let op1 = operand::src(c.cx, ops[0], c.span, r, &mut imm)?;
    c.imm_op = imm;

    if c.isa == Isa::Msp430Xv2
        && op1.mode == Mode::Reg
        && op1.reg == reg::PC
        && matches!(opcode.name, "rrax" | "rrcx" | "rra" | "rrc")
    {
        c.error("the PC cannot be rotated");
        return None;
    }

    let mut e = Enc::new();
    let mut ext = None;
    if c.extended_op {
        if matches!(opcode.name, "swpbx" | "sxtx") {
            // These two spell the operand width the other way round: the
            // instruction's B/W bit is always clear, and only the extension
            // word's A/L bit distinguishes a word from an address.
            bin &= !BYTE_OPERATION;
            if c.byte_op {
                let name = c.name;
                c.error(format!("`{name}` has no `.b` form"));
                return None;
            }
            if !c.addr_op {
                c.extended |= BYTE_OPERATION;
            }
        } else if !c.addr_op {
            c.extended |= BYTE_OPERATION;
        }
        if opcode.name == "rrux" {
            c.extended |= IGNORE_CARRY_BIT;
        }
        if op1.ol != 0 && (c.extended & 0xf) != 0 {
            c.error("a repeat count only applies to a register-mode instruction");
            c.extended &= !0xf;
        }
        ext = ext_bits(c, &op1, true, false);
        e.word(c.extended);
    }
    bin |= op1.reg as u16 | ((op1.am as u16) << 4);
    e.word(bin);
    operand_word(c, &mut e, &op1, !c.extended_op, false);
    if let Some(f) = ext {
        e.fixup(0, f.x, f.kind);
    }
    e.done()
}

/// A conditional jump, and `jmp`.
fn jump(c: &mut Ctx<'_, '_>, bin: u16, ops: &[&[Token]]) -> Option<Vec<Variant>> {
    if !want(c, ops, 1) {
        return None;
    }
    let toks = ops[0];
    let dollar = toks.first().is_some_and(|t| t.is_punct(Punct::Dollar));
    let x = expr_of(c, if dollar { &toks[1..] } else { toks })?;
    let mut e = Enc::new();
    match operand::known(c.cx, x.e) {
        Some(mut v) => {
            if v & 1 != 0 {
                v += 1;
            }
            // A `$`-relative jump forward, and any jump backward, is measured
            // from the instruction; a plain positive number is measured from
            // the word after it, as the field itself is.
            if (dollar && v > 0) || v < 0 {
                v -= 2;
            }
            v >>= 1;
            if !(-511..=512).contains(&v) {
                c.error(format!("jump displacement {} is out of range", v << 1));
                return None;
            }
            e.word(bin | (v as u16 & 0x3ff));
        }
        None => {
            if dollar {
                c.error("a `$` jump needs a constant displacement, not a label");
                return None;
            }
            let r = c.reloc(Bfd::Jump10)?;
            e.word(bin);
            e.fixup(0, x, reloc::jump10(r));
        }
    }
    e.done()
}

/// A simple polymorph: `jump`, `beq` and their relatives.
///
/// GNU as only assembles these with `-mP`, and without `-mQ` — which its own
/// help text calls dangerous — it always takes the long form, since the
/// linker is the only thing that can shorten a branch in a relaxable object.
/// rsasm does the same, and needs no option for it.
fn polymorph(c: &mut Ctx<'_, '_>, index: u8, ops: &[&[Token]]) -> Option<Vec<Variant>> {
    if !want(c, ops, 1) {
        return None;
    }
    let x = target(c, ops[0])?;
    // The reference gives a polymorph a fragment its relaxation revisits, so
    // a difference of labels across one is not a number as the file is read.
    c.cx.relaxable = true;
    let table = if c.isa.is_430x() { &RCODES_X } else { &RCODES };
    let mut e = Enc::new();
    for w in table[index as usize] {
        if w != 0 {
            e.word(w);
        }
    }
    let at = e.word(0);
    let r = c.reloc(Bfd::RlPcrel)?;
    e.fixup(at, x, reloc::pcrel16(r));
    e.done()
}

/// A long polymorph: `bgt`, `bgtu`, `bleu` and `ble`, which need two jumps
/// even in their short form.
fn polymorph_long(c: &mut Ctx<'_, '_>, index: u8, ops: &[&[Token]]) -> Option<Vec<Variant>> {
    if !want(c, ops, 1) {
        return None;
    }
    let x = target(c, ops[0])?;
    c.cx.relaxable = true;
    let mut e = Enc::new();
    let words = HCODES[index as usize];
    e.word(words[0]);
    e.word(words[1]);
    e.word(branch_word(c.isa));
    let at = e.word(0);
    let r = c.reloc(Bfd::RlPcrel)?;
    e.fixup(at, x, reloc::pcrel16(r));
    e.done()
}

/// A polymorph's target, whose `#` or `$` the reference ignores.
fn target(c: &mut Ctx<'_, '_>, toks: &[Token]) -> Option<operand::Expr> {
    let rest = match toks.first() {
        Some(t) if t.is_punct(Punct::Hash) || t.is_punct(Punct::Dollar) => &toks[1..],
        _ => toks,
    };
    let x = expr_of(c, rest)?;
    let name = c.name;
    if operand::known(c.cx, x.e).is_some() {
        c.error(format!("`{name}` needs a label, not a number"));
        return None;
    }
    // The reference keeps only the symbol of `beq lab+2` and branches to
    // `lab`, so anything more than a label is refused rather than copied.
    if !matches!(
        c.cx.exprs.get(x.e).kind,
        ExprKind::Sym(_) | ExprKind::SymId(_) | ExprKind::LocalRef(..)
    ) {
        c.cx.error(
            x.span,
            format!(
                "`{name}` needs a label on its own: GNU as branches to the label and \
                 drops anything added to it"
            ),
        );
        return None;
    }
    Some(x)
}

/// `#n`, a constant in `lo..=hi`.
fn hash_constant(c: &mut Ctx<'_, '_>, toks: &[Token], what: &str, lo: i64, hi: i64) -> Option<i64> {
    if !toks.first().is_some_and(|t| t.is_punct(Punct::Hash)) {
        c.error(format!("expected `#n` for {what}"));
        return None;
    }
    let x = expr_of(c, &toks[1..])?;
    let Some(v) = operand::known(c.cx, x.e) else {
        c.error(format!("{what} must be a constant"));
        return None;
    };
    if !(lo..=hi).contains(&v) {
        c.error(format!("{what} {v} is out of range ({lo} to {hi})"));
        return None;
    }
    Some(v)
}

/// The register an operand names, for the forms that take nothing else; see
/// [`operand::leading_reg`].
fn single_reg(c: &Ctx<'_, '_>, toks: &[Token]) -> Option<u8> {
    operand::leading_reg(c.cx, toks)
}

/// Parses a whole operand as one expression.
fn expr_of(c: &mut Ctx<'_, '_>, toks: &[Token]) -> Option<operand::Expr> {
    if toks.is_empty() {
        c.error("missing operand");
        return None;
    }
    let span = toks[0].span.to(toks[toks.len() - 1].span);
    let mut cur = crate::cursor::Cursor::new(toks);
    let e = c.cx.expr_parser().parse(&mut cur)?;
    if !cur.at_end() {
        let at = cur.remaining_span();
        c.cx.error(at, "unexpected tokens after the operand");
        return None;
    }
    Some(operand::Expr { e, span })
}
