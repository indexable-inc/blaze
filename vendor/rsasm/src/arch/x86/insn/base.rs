//! The base integer instruction set: the 8086 through x86-64 core.
//!
//! Everything here predates SIMD; the vector families live in sibling modules.
//!
//! Rows whose operand size no operand decides — `push $1`, `pushf`, `ret` —
//! are listed once per size, each marked with the modes it exists in, and the
//! matcher takes the mode's default among them. Where the three modes differ
//! only in which prefix a size needs, the encoder works that out from the
//! row's size, so one row serves all three.

use super::{
    ADDR16, ADDR32, ATT_ONLY, CONDITIONS, DEF64, Def, IMM64, INTEL_ONLY, ModRm, NO_REX_W,
    NO_REX_W_GAS, NO_REX2, NO64, NO66, NOTACC, ONLY64, Op, PLUSREG, Tbl, WIDTHS, add, d,
    opsize_bits,
};

/// `add`-style group: eight instructions sharing one opcode layout.
fn alu_group(table: &mut Tbl, mnem: &'static str, base: u8, ext: u8) {
    let mut defs = Vec::new();

    // 8-bit forms.
    defs.push(d(vec![Op::Rm(1), Op::R(1)], &[base], ModRm::Reg, 8));
    defs.push(d(vec![Op::R(1), Op::Rm(1)], &[base + 2], ModRm::Reg, 8));

    for w in WIDTHS {
        let bits = opsize_bits(w);
        defs.push(d(vec![Op::Rm(w), Op::R(w)], &[base + 1], ModRm::Reg, bits));
        defs.push(d(vec![Op::R(w), Op::Rm(w)], &[base + 3], ModRm::Reg, bits));
    }

    // Immediate forms. `op al, imm8` is a byte shorter than the ModRM form.
    // For the wider sizes the sign-extended `imm8` encodings come first, so
    // the matcher prefers them whenever the value fits.
    defs.push(d(
        vec![Op::Fixed("al"), Op::Imm(1)],
        &[base + 4],
        ModRm::None,
        8,
    ));
    defs.push(d(vec![Op::Rm(1), Op::Imm(1)], &[0x80], ModRm::Ext(ext), 8));
    for w in WIDTHS {
        let bits = opsize_bits(w);
        defs.push(d(
            vec![Op::Rm(w), Op::Imm8s],
            &[0x83],
            ModRm::Ext(ext),
            bits,
        ));
    }
    // `op eAX, imm32` is one byte shorter than the ModRM form, so it is tried
    // before it but after imm8-sign-extended.
    defs.push(d(
        vec![Op::Fixed("ax"), Op::Imm(2)],
        &[base + 5],
        ModRm::None,
        16,
    ));
    defs.push(d(
        vec![Op::Fixed("eax"), Op::Imm(4)],
        &[base + 5],
        ModRm::None,
        32,
    ));
    defs.push(d(
        vec![Op::Fixed("rax"), Op::Imm(4)],
        &[base + 5],
        ModRm::None,
        64,
    ));
    for w in WIDTHS {
        let bits = opsize_bits(w);
        let imm = if w == 2 { 2 } else { 4 };
        defs.push(d(
            vec![Op::Rm(w), Op::Imm(imm)],
            &[0x81],
            ModRm::Ext(ext),
            bits,
        ));
    }

    table.insert(mnem, defs);
}

/// `clr`, GNU as's name for clearing a register by exclusive-or with itself.
/// The source writes the register once and the encoding holds it twice.
fn install_clr(table: &mut Tbl) {
    let mut defs = vec![d(vec![Op::R(1), Op::R(1)], &[0x30], ModRm::Reg, 8)];
    for w in WIDTHS {
        defs.push(d(
            vec![Op::R(w), Op::R(w)],
            &[0x31],
            ModRm::Reg,
            opsize_bits(w),
        ));
    }
    table.insert("clr", defs);
}

/// `shl`-style group: shifts and rotates.
fn shift_group(table: &mut Tbl, mnems: &[&'static str], ext: u8) {
    let mut defs = Vec::new();
    defs.push(d(vec![Op::Rm(1), Op::One], &[0xd0], ModRm::Ext(ext), 8));
    defs.push(d(
        vec![Op::Rm(1), Op::Fixed("cl")],
        &[0xd2],
        ModRm::Ext(ext),
        8,
    ));
    defs.push(d(vec![Op::Rm(1), Op::Imm(1)], &[0xc0], ModRm::Ext(ext), 8));
    for w in WIDTHS {
        let bits = opsize_bits(w);
        defs.push(d(vec![Op::Rm(w), Op::One], &[0xd1], ModRm::Ext(ext), bits));
        defs.push(d(
            vec![Op::Rm(w), Op::Fixed("cl")],
            &[0xd3],
            ModRm::Ext(ext),
            bits,
        ));
        defs.push(d(
            vec![Op::Rm(w), Op::Imm(1)],
            &[0xc1],
            ModRm::Ext(ext),
            bits,
        ));
    }
    // A shift with no count means "by one".
    defs.push(d(vec![Op::Rm(1)], &[0xd0], ModRm::Ext(ext), 8));
    for w in WIDTHS {
        defs.push(d(vec![Op::Rm(w)], &[0xd1], ModRm::Ext(ext), opsize_bits(w)));
    }
    for m in mnems {
        table.insert(m, defs.clone());
    }
}

/// `not`/`neg`/`mul`/`div`-style unary group: `F6`/`F7 /digit`.
fn unary_group(table: &mut Tbl, mnem: &'static str, ext: u8) {
    let mut defs = vec![d(vec![Op::Rm(1)], &[0xf6], ModRm::Ext(ext), 8)];
    for w in WIDTHS {
        defs.push(d(vec![Op::Rm(w)], &[0xf7], ModRm::Ext(ext), opsize_bits(w)));
    }
    // The divisions may name the accumulator they divide as a first operand.
    if ext >= 6 {
        for (w, acc) in [(1u8, "al"), (2, "ax"), (4, "eax"), (8, "rax")] {
            let op = if w == 1 { 0xf6 } else { 0xf7 };
            defs.push(d(
                vec![Op::Fixed(acc), Op::Rm(w)],
                &[op],
                ModRm::Ext(ext),
                opsize_bits(w),
            ));
        }
    }
    table.insert(mnem, defs);
}

/// The same operands at each of the three operand sizes a stack or control
/// transfer instruction has: 16 bits anywhere, 32 bits outside long mode, and
/// 64 bits, the default there, in long mode only.
fn stack_sizes(ops: impl Fn(u8) -> Vec<Op>, opcode: &[u8], modrm: ModRm, flags: u32) -> Vec<Def> {
    vec![
        d(ops(2), opcode, modrm, 16).flags(flags),
        d(ops(4), opcode, modrm, 32).flags(flags | NO64),
        d(ops(8), opcode, modrm, 64).flags(flags | DEF64 | ONLY64),
    ]
}

/// The same operands at 16, 32 and 64 bits, where the 64-bit form needs
/// REX.W rather than being long mode's default: `iret`, `lret`, `lss`.
fn rex_sizes(ops: impl Fn(u8) -> Vec<Op>, opcode: &[u8], modrm: ModRm, flags: u32) -> Vec<Def> {
    vec![
        d(ops(2), opcode, modrm, 16).flags(flags),
        d(ops(4), opcode, modrm, 32).flags(flags),
        d(ops(8), opcode, modrm, 64).flags(flags | ONLY64),
    ]
}

/// A row of every operand size, each with the same operand shape.
fn all_widths(ops: impl Fn(u8) -> Vec<Op>, opcode: &[u8], modrm: ModRm) -> Vec<Def> {
    WIDTHS
        .iter()
        .map(|&w| d(ops(w), opcode, modrm, opsize_bits(w)))
        .collect()
}

pub fn install(t: &mut Tbl) {
    alu_group(t, "add", 0x00, 0);
    alu_group(t, "or", 0x08, 1);
    alu_group(t, "adc", 0x10, 2);
    alu_group(t, "sbb", 0x18, 3);
    alu_group(t, "and", 0x20, 4);
    alu_group(t, "sub", 0x28, 5);
    alu_group(t, "xor", 0x30, 6);
    alu_group(t, "cmp", 0x38, 7);
    install_clr(t);

    shift_group(t, &["rol"], 0);
    shift_group(t, &["ror"], 1);
    shift_group(t, &["rcl"], 2);
    shift_group(t, &["rcr"], 3);
    shift_group(t, &["shl", "sal"], 4);
    shift_group(t, &["shr"], 5);
    shift_group(t, &["sar"], 7);

    unary_group(t, "not", 2);
    unary_group(t, "neg", 3);
    unary_group(t, "mul", 4);
    unary_group(t, "div", 6);
    unary_group(t, "idiv", 7);

    install_moves(t);
    install_arith(t);
    install_stack(t);
    install_branches(t);
    install_strings(t);
    install_bits(t);
    install_system(t);
    install_misc(t);
}

fn install_moves(t: &mut Tbl) {
    // `test` has no sign-extended immediate, and one opcode for either
    // direction: the operation is symmetric, so both assemblers take the
    // register and the r/m operand in either order.
    {
        let mut defs = vec![
            d(vec![Op::Rm(1), Op::R(1)], &[0x84], ModRm::Reg, 8),
            d(vec![Op::R(1), Op::Rm(1)], &[0x84], ModRm::Reg, 8),
            d(vec![Op::Fixed("al"), Op::Imm(1)], &[0xa8], ModRm::None, 8),
            d(vec![Op::Rm(1), Op::Imm(1)], &[0xf6], ModRm::Ext(0), 8),
        ];
        for w in WIDTHS {
            let bits = opsize_bits(w);
            defs.push(d(vec![Op::Rm(w), Op::R(w)], &[0x85], ModRm::Reg, bits));
            defs.push(d(vec![Op::R(w), Op::Rm(w)], &[0x85], ModRm::Reg, bits));
        }
        defs.push(d(
            vec![Op::Fixed("ax"), Op::Imm(2)],
            &[0xa9],
            ModRm::None,
            16,
        ));
        defs.push(d(
            vec![Op::Fixed("eax"), Op::Imm(4)],
            &[0xa9],
            ModRm::None,
            32,
        ));
        defs.push(d(
            vec![Op::Fixed("rax"), Op::Imm(4)],
            &[0xa9],
            ModRm::None,
            64,
        ));
        for w in WIDTHS {
            let bits = opsize_bits(w);
            let imm = if w == 2 { 2 } else { 4 };
            defs.push(d(
                vec![Op::Rm(w), Op::Imm(imm)],
                &[0xf7],
                ModRm::Ext(0),
                bits,
            ));
        }
        t.insert("test", defs);
    }

    // mov
    {
        // Outside long mode, a load from or store to an absolute address in
        // the accumulator has a form without ModRM, one byte shorter. Long
        // mode keeps those opcodes for `movabs` and its 64-bit addresses.
        let mut defs = vec![
            d(vec![Op::Fixed("al"), Op::Moffs(1)], &[0xa0], ModRm::None, 8).flags(NO64),
            d(
                vec![Op::Fixed("ax"), Op::Moffs(2)],
                &[0xa1],
                ModRm::None,
                16,
            )
            .flags(NO64),
            d(
                vec![Op::Fixed("eax"), Op::Moffs(4)],
                &[0xa1],
                ModRm::None,
                32,
            )
            .flags(NO64),
            d(vec![Op::Moffs(1), Op::Fixed("al")], &[0xa2], ModRm::None, 8).flags(NO64),
            d(
                vec![Op::Moffs(2), Op::Fixed("ax")],
                &[0xa3],
                ModRm::None,
                16,
            )
            .flags(NO64),
            d(
                vec![Op::Moffs(4), Op::Fixed("eax")],
                &[0xa3],
                ModRm::None,
                32,
            )
            .flags(NO64),
            d(vec![Op::Rm(1), Op::R(1)], &[0x88], ModRm::Reg, 8),
            d(vec![Op::R(1), Op::Rm(1)], &[0x8a], ModRm::Reg, 8),
        ];
        for w in WIDTHS {
            let bits = opsize_bits(w);
            defs.push(d(vec![Op::Rm(w), Op::R(w)], &[0x89], ModRm::Reg, bits));
            defs.push(d(vec![Op::R(w), Op::Rm(w)], &[0x8b], ModRm::Reg, bits));
        }
        // `B0+r`/`B8+r` load an immediate straight into a register.
        defs.push(d(vec![Op::R(1), Op::Imm(1)], &[0xb0], ModRm::None, 8).flags(PLUSREG));
        defs.push(d(vec![Op::R(2), Op::Imm(2)], &[0xb8], ModRm::None, 16).flags(PLUSREG));
        defs.push(d(vec![Op::R(4), Op::Imm(4)], &[0xb8], ModRm::None, 32).flags(PLUSREG));
        // C7 /0 sign-extends imm32 to 64 bits and is shorter than movabs, so
        // it is preferred whenever the value fits.
        defs.push(d(vec![Op::Rm(8), Op::Imm(4)], &[0xc7], ModRm::Ext(0), 64));
        defs.push(d(vec![Op::R(8), Op::Imm(8)], &[0xb8], ModRm::None, 64).flags(PLUSREG | IMM64));
        defs.push(d(vec![Op::Rm(1), Op::Imm(1)], &[0xc6], ModRm::Ext(0), 8));
        defs.push(d(vec![Op::Rm(2), Op::Imm(2)], &[0xc7], ModRm::Ext(0), 16));
        defs.push(d(vec![Op::Rm(4), Op::Imm(4)], &[0xc7], ModRm::Ext(0), 32));

        // Segment registers. A selector is 16 bits, but stored into a
        // register it is zero-extended to the register's size, which the
        // operand size then names; stored into memory it is always a word.
        // Loading one takes whatever the source is without a prefix.
        for w in WIDTHS {
            defs.push(d(
                vec![Op::R(w), Op::Seg],
                &[0x8c],
                ModRm::Reg,
                opsize_bits(w),
            ));
        }
        defs.push(d(vec![Op::M(2), Op::Seg], &[0x8c], ModRm::Reg, 16).flags(NO66));
        defs.push(d(vec![Op::Seg, Op::R(2)], &[0x8e], ModRm::Reg, 16).flags(NO66));
        defs.push(d(vec![Op::Seg, Op::R(4)], &[0x8e], ModRm::Reg, 32).flags(NO66));
        defs.push(d(vec![Op::Seg, Op::R(8)], &[0x8e], ModRm::Reg, 64));
        defs.push(d(vec![Op::Seg, Op::M(2)], &[0x8e], ModRm::Reg, 16).flags(NO66));

        // Control and debug registers are as wide as the mode, and the
        // register operand always goes in r/m, whatever ModRM.mod says.
        for (reg, load, store) in [(Op::Cr, 0x20u8, 0x22u8), (Op::Dr, 0x21, 0x23)] {
            defs.push(d(vec![Op::R(4), reg], &[0x0f, load], ModRm::Reg, 32).flags(NO64 | NO66));
            defs.push(
                d(vec![Op::R(8), reg], &[0x0f, load], ModRm::Reg, 64).flags(ONLY64 | NO_REX_W),
            );
            defs.push(d(vec![reg, Op::R(4)], &[0x0f, store], ModRm::Reg, 32).flags(NO64 | NO66));
            defs.push(
                d(vec![reg, Op::R(8)], &[0x0f, store], ModRm::Reg, 64).flags(ONLY64 | NO_REX_W),
            );
        }
        t.insert("mov", defs);

        // `movabs` always takes the full-width immediate form.
        t.insert(
            "movabs",
            // And the accumulator forms with a 64-bit address.
            vec![
                d(vec![Op::R(8), Op::Imm(8)], &[0xb8], ModRm::None, 64).flags(PLUSREG | IMM64),
                d(vec![Op::Fixed("al"), Op::Moffs(1)], &[0xa0], ModRm::None, 8).flags(ONLY64),
                d(
                    vec![Op::Fixed("ax"), Op::Moffs(2)],
                    &[0xa1],
                    ModRm::None,
                    16,
                )
                .flags(ONLY64),
                d(
                    vec![Op::Fixed("eax"), Op::Moffs(4)],
                    &[0xa1],
                    ModRm::None,
                    32,
                )
                .flags(ONLY64),
                d(
                    vec![Op::Fixed("rax"), Op::Moffs(8)],
                    &[0xa1],
                    ModRm::None,
                    64,
                )
                .flags(ONLY64),
                d(vec![Op::Moffs(1), Op::Fixed("al")], &[0xa2], ModRm::None, 8).flags(ONLY64),
                d(
                    vec![Op::Moffs(2), Op::Fixed("ax")],
                    &[0xa3],
                    ModRm::None,
                    16,
                )
                .flags(ONLY64),
                d(
                    vec![Op::Moffs(4), Op::Fixed("eax")],
                    &[0xa3],
                    ModRm::None,
                    32,
                )
                .flags(ONLY64),
                d(
                    vec![Op::Moffs(8), Op::Fixed("rax")],
                    &[0xa3],
                    ModRm::None,
                    64,
                )
                .flags(ONLY64),
            ],
        );
    }

    // Sign- and zero-extending moves.
    for (mnem, op) in [("movzx", 0xb6u8), ("movsx", 0xbeu8)] {
        let mut defs = Vec::new();
        for dst in [2u8, 4, 8] {
            defs.push(d(
                vec![Op::R(dst), Op::Rm(1)],
                &[0x0f, op],
                ModRm::Reg,
                opsize_bits(dst),
            ));
        }
        for dst in [4u8, 8] {
            defs.push(d(
                vec![Op::R(dst), Op::Rm(2)],
                &[0x0f, op + 1],
                ModRm::Reg,
                opsize_bits(dst),
            ));
        }
        // GNU as also spells `movsxd` this way.
        if op == 0xbe {
            defs.push(d(vec![Op::R(8), Op::Rm(4)], &[0x63], ModRm::Reg, 64).flags(ONLY64));
        }
        t.insert(mnem, defs);
    }
    // 32-to-64 sign extension has its own opcode.
    t.insert(
        "movsxd",
        vec![d(vec![Op::R(8), Op::Rm(4)], &[0x63], ModRm::Reg, 64).flags(ONLY64)],
    );

    t.insert(
        "lea",
        all_widths(|w| vec![Op::R(w), Op::M(0)], &[0x8d], ModRm::Reg),
    );

    t.insert("xchg", {
        // `xchg rax, rax` is spelled `nop` by GNU as and `48 90` by NASM, and
        // `xchg ax, ax` is `66 90`; both are shorter than the ModRM forms, so
        // they come first. Outside long mode, where it clears no upper half,
        // so is `xchg eax, eax`.
        let mut defs = vec![
            d(
                vec![Op::Fixed("rax"), Op::Fixed("rax")],
                &[0x90],
                ModRm::None,
                64,
            )
            .flags(NO_REX_W_GAS),
            d(
                vec![Op::Fixed("eax"), Op::Fixed("eax")],
                &[0x90],
                ModRm::None,
                32,
            )
            .flags(NO64),
            d(
                vec![Op::Fixed("ax"), Op::Fixed("ax")],
                &[0x90],
                ModRm::None,
                16,
            ),
        ];
        // `xchg rAX, r` has a one-byte encoding.
        for (w, acc) in [(2u8, "ax"), (4, "eax"), (8, "rax")] {
            let bits = opsize_bits(w);
            defs.push(
                d(vec![Op::Fixed(acc), Op::R(w)], &[0x90], ModRm::None, bits)
                    .flags(PLUSREG | NOTACC),
            );
            defs.push(
                d(vec![Op::R(w), Op::Fixed(acc)], &[0x90], ModRm::None, bits)
                    .flags(PLUSREG | NOTACC),
            );
        }
        defs.push(d(vec![Op::Rm(1), Op::R(1)], &[0x86], ModRm::Reg, 8));
        defs.push(d(vec![Op::R(1), Op::Rm(1)], &[0x86], ModRm::Reg, 8));
        for w in WIDTHS {
            defs.push(d(
                vec![Op::Rm(w), Op::R(w)],
                &[0x87],
                ModRm::Reg,
                opsize_bits(w),
            ));
            defs.push(d(
                vec![Op::R(w), Op::Rm(w)],
                &[0x87],
                ModRm::Reg,
                opsize_bits(w),
            ));
        }
        defs
    });

    // `movbe` swaps the byte order as it moves.
    add(t, "movbe", {
        let mut defs = all_widths(
            |w| vec![Op::R(w), Op::M(w)],
            &[0x0f, 0x38, 0xf0],
            ModRm::Reg,
        );
        defs.extend(all_widths(
            |w| vec![Op::M(w), Op::R(w)],
            &[0x0f, 0x38, 0xf1],
            ModRm::Reg,
        ));
        defs
    });

    // Far pointer loads. The pointer's size follows the register's.
    for (mnem, opcode, legacy) in [
        ("les", &[0xc4u8][..], true),
        ("lds", &[0xc5], true),
        ("lss", &[0x0f, 0xb2], false),
        ("lfs", &[0x0f, 0xb4], false),
        ("lgs", &[0x0f, 0xb5], false),
    ] {
        let defs = if legacy {
            vec![
                d(vec![Op::R(2), Op::M(0)], opcode, ModRm::Reg, 16).flags(NO64),
                d(vec![Op::R(4), Op::M(0)], opcode, ModRm::Reg, 32).flags(NO64),
            ]
        } else {
            rex_sizes(|w| vec![Op::R(w), Op::M(0)], opcode, ModRm::Reg, 0)
        };
        t.insert(mnem, defs);
    }

    // A table lookup through `bx`, which may be written with its segment.
    for name in ["xlat", "xlatb"] {
        t.insert(
            name,
            vec![
                d(vec![], &[0xd7], ModRm::None, 0),
                d(vec![Op::StrSrc(1)], &[0xd7], ModRm::None, 0),
            ],
        );
    }
}

fn install_arith(t: &mut Tbl) {
    // imul has three shapes: one-operand (into rDX:rAX), two-operand, and
    // three-operand with an immediate.
    t.insert("imul", {
        let mut defs = vec![d(vec![Op::Rm(1)], &[0xf6], ModRm::Ext(5), 8)];
        for w in WIDTHS {
            let bits = opsize_bits(w);
            let imm = if w == 2 { 2 } else { 4 };
            defs.push(d(
                vec![Op::R(w), Op::Rm(w), Op::Imm8s],
                &[0x6b],
                ModRm::Reg,
                bits,
            ));
            defs.push(d(
                vec![Op::R(w), Op::Rm(w), Op::Imm(imm)],
                &[0x69],
                ModRm::Reg,
                bits,
            ));
            defs.push(d(
                vec![Op::R(w), Op::Rm(w)],
                &[0x0f, 0xaf],
                ModRm::Reg,
                bits,
            ));
            defs.push(d(vec![Op::Rm(w)], &[0xf7], ModRm::Ext(5), bits));
        }
        defs
    });

    for (mnem, ext) in [("inc", 0u8), ("dec", 1)] {
        // `40+r` and `48+r` are one byte, but long mode took them for REX.
        let short = if ext == 0 { 0x40 } else { 0x48 };
        let mut defs = vec![
            d(vec![Op::R(2)], &[short], ModRm::None, 16).flags(PLUSREG | NO64),
            d(vec![Op::R(4)], &[short], ModRm::None, 32).flags(PLUSREG | NO64),
            d(vec![Op::Rm(1)], &[0xfe], ModRm::Ext(ext), 8),
        ];
        for w in WIDTHS {
            defs.push(d(vec![Op::Rm(w)], &[0xff], ModRm::Ext(ext), opsize_bits(w)));
        }
        t.insert(mnem, defs);
    }

    // Decimal adjustment, gone from long mode.
    for (mnem, op) in [("daa", 0x27u8), ("das", 0x2f), ("aaa", 0x37), ("aas", 0x3f)] {
        t.insert(mnem, vec![d(vec![], &[op], ModRm::None, 0).flags(NO64)]);
    }
    for (mnem, op) in [("aam", 0xd4u8), ("aad", 0xd5)] {
        t.insert(
            mnem,
            vec![
                // The base is an immediate byte, ten unless written.
                d(vec![], &[op, 0x0a], ModRm::None, 0).flags(NO64),
                d(vec![Op::Imm(1)], &[op], ModRm::None, 0).flags(NO64),
            ],
        );
    }

    // `cbw` family: sign-extend the accumulator, into itself or into rDX.
    for (names, op, bits) in [
        (&["cbw", "cbtw"][..], 0x98u8, 16u8),
        (&["cwde", "cwtl"], 0x98, 32),
        (&["cdqe", "cltq"], 0x98, 64),
        (&["cwd", "cwtd"], 0x99, 16),
        (&["cdq", "cltd"], 0x99, 32),
        (&["cqo", "cqto"], 0x99, 64),
    ] {
        for name in names {
            t.insert(name, vec![d(vec![], &[op], ModRm::None, bits)]);
        }
    }

    // Bit scans and population counts share the `r, r/m` shape.
    for (mnem, op) in [("bsf", 0xbcu8), ("bsr", 0xbd)] {
        t.insert(
            mnem,
            all_widths(|w| vec![Op::R(w), Op::Rm(w)], &[0x0f, op], ModRm::Reg),
        );
    }

    // Exchange-and-add and compare-and-exchange.
    for (mnem, op) in [("xadd", 0xc0u8), ("cmpxchg", 0xb0)] {
        let mut defs = vec![d(vec![Op::Rm(1), Op::R(1)], &[0x0f, op], ModRm::Reg, 8)];
        defs.extend(all_widths(
            |w| vec![Op::Rm(w), Op::R(w)],
            &[0x0f, op + 1],
            ModRm::Reg,
        ));
        t.insert(mnem, defs);
    }
    t.insert(
        "cmpxchg8b",
        vec![d(vec![Op::M(8)], &[0x0f, 0xc7], ModRm::Ext(1), 0)],
    );
    t.insert(
        "cmpxchg16b",
        vec![d(vec![Op::M(16)], &[0x0f, 0xc7], ModRm::Ext(1), 64).flags(ONLY64)],
    );

    t.insert(
        "bswap",
        vec![
            d(vec![Op::R(4)], &[0x0f, 0xc8], ModRm::None, 32).flags(PLUSREG),
            d(vec![Op::R(8)], &[0x0f, 0xc8], ModRm::None, 64).flags(PLUSREG),
        ],
    );

    // Hardware random numbers: a register only.
    for (mnem, ext) in [("rdrand", 6u8), ("rdseed", 7)] {
        t.insert(
            mnem,
            all_widths(|w| vec![Op::R(w)], &[0x0f, 0xc7], ModRm::Ext(ext)),
        );
    }

    // Double-precision shifts: an immediate count, or `cl`.
    for (mnem, op) in [("shld", 0xa4u8), ("shrd", 0xac)] {
        let mut defs = all_widths(
            |w| vec![Op::Rm(w), Op::R(w), Op::Imm(1)],
            &[0x0f, op],
            ModRm::Reg,
        );
        defs.extend(all_widths(
            |w| vec![Op::Rm(w), Op::R(w), Op::Fixed("cl")],
            &[0x0f, op + 1],
            ModRm::Reg,
        ));
        // AT&T lets the `cl` go unwritten.
        defs.extend(all_widths(
            |w| vec![Op::Rm(w), Op::R(w)],
            &[0x0f, op + 1],
            ModRm::Reg,
        ));
        t.insert(mnem, defs);
    }
}

fn install_bits(t: &mut Tbl) {
    for (mnem, op, ext) in [
        ("bt", 0xa3u8, 4u8),
        ("bts", 0xab, 5),
        ("btr", 0xb3, 6),
        ("btc", 0xbb, 7),
    ] {
        let mut defs = all_widths(|w| vec![Op::Rm(w), Op::R(w)], &[0x0f, op], ModRm::Reg);
        defs.extend(all_widths(
            |w| vec![Op::Rm(w), Op::Imm(1)],
            &[0x0f, 0xba],
            ModRm::Ext(ext),
        ));
        t.insert(mnem, defs);
    }

    for &(suffix, tttn) in CONDITIONS {
        // Both references also take `setneb` and friends in Intel syntax,
        // whose byte suffix says nothing new.
        for suffix in [suffix, &format!("{suffix}b")] {
            let setcc: &'static str = Box::leak(format!("set{suffix}").into_boxed_str());
            t.insert(
                setcc,
                vec![d(vec![Op::Rm(1)], &[0x0f, 0x90 + tttn], ModRm::Ext(0), 8)],
            );
        }
        let cmovcc: &'static str = Box::leak(format!("cmov{suffix}").into_boxed_str());
        t.insert(
            cmovcc,
            all_widths(
                |w| vec![Op::R(w), Op::Rm(w)],
                &[0x0f, 0x40 + tttn],
                ModRm::Reg,
            ),
        );
    }
}

fn install_stack(t: &mut Tbl) {
    let mut push = stack_sizes(|_| vec![Op::Imm8s], &[0x6a], ModRm::None, 0);
    push.extend(stack_sizes(
        |w| vec![Op::R(w)],
        &[0x50],
        ModRm::None,
        PLUSREG,
    ));
    // The 64-bit form's immediate is 32 bits, sign-extended.
    push.extend(stack_sizes(
        |w| vec![Op::Imm(w.min(4))],
        &[0x68],
        ModRm::None,
        0,
    ));
    push.extend(stack_sizes(|w| vec![Op::Rm(w)], &[0xff], ModRm::Ext(6), 0));
    let mut pop = stack_sizes(|w| vec![Op::R(w)], &[0x58], ModRm::None, PLUSREG);
    pop.extend(stack_sizes(|w| vec![Op::Rm(w)], &[0x8f], ModRm::Ext(0), 0));

    // Segment registers. Long mode dropped the one-byte forms, and nothing
    // ever popped `cs`.
    for (seg, push_op, pop_op) in [
        ("es", &[0x06u8][..], Some(&[0x07u8][..])),
        ("cs", &[0x0e], None),
        ("ss", &[0x16], Some(&[0x17][..])),
        ("ds", &[0x1e], Some(&[0x1f][..])),
    ] {
        let rows = |op: &[u8]| {
            vec![
                d(vec![Op::Fixed(seg)], op, ModRm::None, 16).flags(NO64),
                d(vec![Op::Fixed(seg)], op, ModRm::None, 32).flags(NO64),
            ]
        };
        push.extend(rows(push_op));
        if let Some(op) = pop_op {
            pop.extend(rows(op));
        }
    }
    for (seg, push_op, pop_op) in [("fs", 0xa0u8, 0xa1u8), ("gs", 0xa8, 0xa9)] {
        push.extend(stack_sizes(
            |_| vec![Op::Fixed(seg)],
            &[0x0f, push_op],
            ModRm::None,
            0,
        ));
        pop.extend(stack_sizes(
            |_| vec![Op::Fixed(seg)],
            &[0x0f, pop_op],
            ModRm::None,
            0,
        ));
    }
    t.insert("push", push);
    t.insert("pop", pop);

    // All the general registers at once, which long mode dropped.
    for (mnem, op) in [("pusha", 0x60u8), ("popa", 0x61)] {
        t.insert(
            mnem,
            vec![
                d(vec![], &[op], ModRm::None, 16).flags(NO64),
                d(vec![], &[op], ModRm::None, 32).flags(NO64),
            ],
        );
        // Intel syntax names the sizes too, and has no suffix rule to do it.
        for (suffix, bits) in [("w", 16u8), ("d", 32)] {
            let name: &'static str = Box::leak(format!("{mnem}{suffix}").into_boxed_str());
            t.insert(name, vec![d(vec![], &[op], ModRm::None, bits).flags(NO64)]);
        }
    }
    for (mnem, op) in [("pushf", 0x9cu8), ("popf", 0x9d)] {
        t.insert(mnem, stack_sizes(|_| vec![], &[op], ModRm::None, 0));
        let rows = t[mnem].clone();
        for (suffix, bits) in [("w", 16u8), ("d", 32), ("q", 64)] {
            let name: &'static str = Box::leak(format!("{mnem}{suffix}").into_boxed_str());
            t.insert(
                name,
                rows.iter().filter(|r| r.opsize == bits).cloned().collect(),
            );
        }
    }

    t.insert(
        "enter",
        stack_sizes(|_| vec![Op::Imm(2), Op::Imm(1)], &[0xc8], ModRm::None, 0),
    );
    t.insert("leave", stack_sizes(|_| vec![], &[0xc9], ModRm::None, 0));
}

fn install_branches(t: &mut Tbl) {
    // Control transfer. `jmp`'s two relative forms differ in size, which is
    // what drives branch relaxation in the layout pass. The wide one carries
    // a displacement of the mode's operand size.
    let mut jmp = vec![
        d(vec![Op::Rel(1)], &[0xeb], ModRm::None, 0),
        d(vec![Op::Rel(4)], &[0xe9], ModRm::None, 0),
    ];
    jmp.extend(stack_sizes(
        |w| vec![Op::IndirectRm(w)],
        &[0xff],
        ModRm::Ext(4),
        0,
    ));
    jmp.push(d(vec![Op::Fword], &[0xff], ModRm::Ext(5), 32));
    jmp.push(d(vec![Op::FarDword], &[0xff], ModRm::Ext(5), 16));
    jmp.extend(far_direct(0xea));
    t.insert("jmp", jmp);

    // `callw` outside 16-bit mode is a 16-bit call, with a 16-bit
    // displacement and return address, and `calll` in 16-bit mode a 32-bit
    // one. (`jmpw` and `jmpl` are refused there.)
    let mut call = vec![
        d(vec![Op::Rel(4)], &[0xe8], ModRm::None, 0),
        d(vec![Op::Rel(2)], &[0xe8], ModRm::None, 16).flags(NO64),
        d(vec![Op::Rel(4)], &[0xe8], ModRm::None, 32).flags(NO64),
    ];
    call.extend(stack_sizes(
        |w| vec![Op::IndirectRm(w)],
        &[0xff],
        ModRm::Ext(2),
        0,
    ));
    call.push(d(vec![Op::Fword], &[0xff], ModRm::Ext(3), 32));
    call.push(d(vec![Op::FarDword], &[0xff], ModRm::Ext(3), 16));
    call.extend(far_direct(0x9a));
    t.insert("call", call);

    // Far branches: a direct `seg:offset`, or through a pointer in memory.
    for (mnem, direct, ext) in [("ljmp", 0xeau8, 5u8), ("lcall", 0x9a, 3)] {
        let mut defs = far_direct(direct);
        defs.extend(rex_sizes(|_| vec![Op::FarM], &[0xff], ModRm::Ext(ext), 0));
        t.insert(mnem, defs);
    }

    let mut ret = stack_sizes(|_| vec![], &[0xc3], ModRm::None, 0);
    ret.extend(stack_sizes(|_| vec![Op::Imm(2)], &[0xc2], ModRm::None, 0));
    t.insert("ret", ret);
    let mut lret = rex_sizes(|_| vec![], &[0xcb], ModRm::None, 0);
    lret.extend(rex_sizes(|_| vec![Op::Imm(2)], &[0xca], ModRm::None, 0));
    t.insert("lret", lret.clone());
    t.insert("retf", lret.clone());
    // Intel syntax names the 64-bit far return too.
    t.insert(
        "retfq",
        lret.into_iter().filter(|r| r.opsize == 64).collect(),
    );
    t.insert("iret", rex_sizes(|_| vec![], &[0xcf], ModRm::None, 0));
    for (name, bits) in [("iretw", 16u8), ("iretd", 32), ("iretq", 64)] {
        let rows = t["iret"]
            .iter()
            .filter(|r| r.opsize == bits)
            .cloned()
            .collect();
        t.insert(name, rows);
    }

    for &(suffix, tttn) in CONDITIONS {
        let jcc: &'static str = Box::leak(format!("j{suffix}").into_boxed_str());
        t.insert(
            jcc,
            vec![
                d(vec![Op::Rel(1)], &[0x70 + tttn], ModRm::None, 0),
                d(vec![Op::Rel(4)], &[0x0f, 0x80 + tttn], ModRm::None, 0),
            ],
        );
    }

    // Branches on the count register, which only have a short form. Which
    // register is a matter of address size, not operand size.
    t.insert(
        "jcxz",
        vec![d(vec![Op::Rel(1)], &[0xe3], ModRm::None, 0).flags(NO64 | ADDR16)],
    );
    t.insert(
        "jecxz",
        vec![d(vec![Op::Rel(1)], &[0xe3], ModRm::None, 0).flags(ADDR32)],
    );
    t.insert(
        "jrcxz",
        vec![d(vec![Op::Rel(1)], &[0xe3], ModRm::None, 0).flags(ONLY64)],
    );
    for (names, op) in [
        (&["loop"][..], 0xe2u8),
        (&["loope", "loopz"], 0xe1),
        (&["loopne", "loopnz"], 0xe0),
    ] {
        for name in names {
            t.insert(name, vec![d(vec![Op::Rel(1)], &[op], ModRm::None, 0)]);
        }
    }

    t.insert(
        "int",
        vec![
            d(vec![Op::Three], &[0xcc], ModRm::None, 0),
            d(vec![Op::Imm(1)], &[0xcd], ModRm::None, 0),
        ],
    );
    t.insert("into", vec![d(vec![], &[0xce], ModRm::None, 0).flags(NO64)]);
}

/// `ljmp $seg, $offset` and its Intel twin, outside long mode.
fn far_direct(op: u8) -> Vec<Def> {
    vec![
        d(vec![Op::Far], &[op], ModRm::None, 16).flags(NO64),
        d(vec![Op::Far], &[op], ModRm::None, 32).flags(NO64),
    ]
}

fn install_strings(t: &mut Tbl) {
    // String instructions: one opcode for bytes and one for the wider sizes.
    // AT&T names the size with a suffix and Intel with `d` for 32 bits, and
    // `movsd` and `cmpsd` are also SSE instructions, told apart by their
    // operands. The implicit operands may be written out, which is how a
    // source segment or the other address size is asked for.
    for (stem, op) in [
        ("movs", 0xa4u8),
        ("cmps", 0xa6),
        ("stos", 0xaa),
        ("lods", 0xac),
        ("scas", 0xae),
        ("ins", 0x6c),
        ("outs", 0x6e),
    ] {
        let io = matches!(stem, "ins" | "outs");
        let rows = |bits: u8| -> Vec<Def> {
            // Port I/O has no 64-bit operand size: `insl` is the widest.
            if io && bits == 64 {
                return vec![];
            }
            let opcode = if bits == 8 { op } else { op + 1 };
            let flags = if bits == 64 { ONLY64 } else { 0 };
            let w = bits / 8;
            let acc = match w {
                1 => "al",
                2 => "ax",
                4 => "eax",
                _ => "rax",
            };
            let (src, dst) = (Op::StrSrc(w), Op::StrDst(w));
            let shapes = match stem {
                "movs" => vec![vec![], vec![dst, src]],
                "cmps" => vec![vec![], vec![src, dst]],
                "stos" => vec![vec![], vec![dst], vec![dst, Op::Fixed(acc)]],
                "lods" => vec![vec![], vec![src], vec![Op::Fixed(acc), src]],
                "scas" => vec![vec![], vec![dst], vec![Op::Fixed(acc), dst]],
                "ins" => vec![vec![], vec![dst, Op::Dx]],
                _ => vec![vec![], vec![Op::Dx, src]],
            };
            shapes
                .into_iter()
                .map(|ops| d(ops, &[opcode], ModRm::None, bits).flags(flags))
                .collect()
        };
        let mut all = Vec::new();
        for bits in [8u8, 16, 32, 64] {
            all.extend(rows(bits));
        }
        t.insert(stem, all);
        for (suffix, bits) in [("b", 8u8), ("w", 16), ("l", 32), ("d", 32), ("q", 64)] {
            let name: &'static str = Box::leak(format!("{stem}{suffix}").into_boxed_str());
            // With operands, the `l` spelling is AT&T's and the `d` one
            // Intel's.
            let only = match suffix {
                "l" => ATT_ONLY,
                "d" => INTEL_ONLY,
                _ => 0,
            };
            let rows = rows(bits)
                .into_iter()
                .map(|r| if r.ops.is_empty() { r } else { r.flags(only) })
                .collect();
            add(t, name, rows);
        }
    }

    // Port I/O with the accumulator.
    let mut in_ = Vec::new();
    let mut out = Vec::new();
    for (w, acc) in [(1u8, "al"), (2, "ax"), (4, "eax")] {
        let bits = opsize_bits(w);
        let wide = u8::from(w > 1);
        in_.push(d(
            vec![Op::Fixed(acc), Op::Imm(1)],
            &[0xe4 + wide],
            ModRm::None,
            bits,
        ));
        in_.push(d(
            vec![Op::Fixed(acc), Op::Dx],
            &[0xec + wide],
            ModRm::None,
            bits,
        ));
        out.push(d(
            vec![Op::Imm(1), Op::Fixed(acc)],
            &[0xe6 + wide],
            ModRm::None,
            bits,
        ));
        out.push(d(
            vec![Op::Dx, Op::Fixed(acc)],
            &[0xee + wide],
            ModRm::None,
            bits,
        ));
    }
    // AT&T may leave the accumulator implied.
    for (w, bits) in [(0u8, 8u8), (1, 16), (1, 32)] {
        let implied = |ops: Vec<Op>, op: u8| d(ops, &[op + w], ModRm::None, bits).flags(ATT_ONLY);
        in_.push(implied(vec![Op::Imm(1)], 0xe4));
        in_.push(implied(vec![Op::Dx], 0xec));
        out.push(implied(vec![Op::Imm(1)], 0xe6));
        out.push(implied(vec![Op::Dx], 0xee));
    }
    t.insert("in", in_);
    t.insert("out", out);
}

fn install_system(t: &mut Tbl) {
    // Group 6: the local descriptor table and task register.
    for (mnem, ext, stores) in [
        ("sldt", 0u8, true),
        ("str", 1, true),
        ("lldt", 2, false),
        ("ltr", 3, false),
        ("verr", 4, false),
        ("verw", 5, false),
    ] {
        t.insert(mnem, selector_rows(&[0x0f, 0x00], ext, stores));
    }
    // Group 7: the descriptor tables themselves.
    for (mnem, ext) in [("sgdt", 0u8), ("sidt", 1), ("lgdt", 2), ("lidt", 3)] {
        // AT&T may give these a suffix, which then asks for its operand size
        // prefix; the CPU only cares in 16-bit mode, where it chooses a
        // 24-bit base.
        let row = |bits: u8, flags: u32| {
            d(vec![Op::M(0)], &[0x0f, 0x01], ModRm::Ext(ext), bits).flags(flags)
        };
        t.insert(
            mnem,
            vec![
                row(0, 0),
                row(16, NO64),
                row(32, NO64),
                row(64, ONLY64 | NO_REX_W),
            ],
        );
        // Both references take the word spelling in Intel syntax as well.
        let name: &'static str = Box::leak(format!("{mnem}w").into_boxed_str());
        t.insert(name, vec![row(16, NO64)]);
    }
    t.insert(
        "invlpg",
        vec![d(vec![Op::M(0)], &[0x0f, 0x01], ModRm::Ext(7), 0)],
    );
    t.insert("smsw", selector_rows(&[0x0f, 0x01], 4, true));
    t.insert("lmsw", selector_rows(&[0x0f, 0x01], 6, false));

    // Access rights and segment limits: a selector in, a register out.
    for (mnem, op) in [("lar", 0x02u8), ("lsl", 0x03)] {
        let mut defs = all_widths(|w| vec![Op::R(w), Op::M(2)], &[0x0f, op], ModRm::Reg);
        defs.extend(all_widths(
            |w| vec![Op::R(w), Op::R(w)],
            &[0x0f, op],
            ModRm::Reg,
        ));
        defs.extend(all_widths(
            |w| vec![Op::R(w), Op::R(2)],
            &[0x0f, op],
            ModRm::Reg,
        ));
        // The result is at most 32 bits, so GNU as gives a 64-bit register
        // no REX.W (llvm-mc does).
        let defs = defs
            .into_iter()
            .map(|r| if r.opsize == 64 { r.flags(NO_REX_W) } else { r })
            .collect();
        t.insert(mnem, defs);
    }

    t.insert(
        "arpl",
        vec![d(vec![Op::Rm(2), Op::R(2)], &[0x63], ModRm::Reg, 16).flags(NO64 | NO66)],
    );
    t.insert(
        "bound",
        // The bounds are a pair of the register's size, which is the size an
        // Intel memory operand is written with: `qword ptr` for `eax`. GNU as
        // insists on that; llvm-mc also takes the register's own size.
        vec![
            d(vec![Op::R(2), Op::M(4)], &[0x62], ModRm::Reg, 16).flags(NO64),
            d(vec![Op::R(4), Op::M(8)], &[0x62], ModRm::Reg, 32).flags(NO64),
        ],
    );

    // Processor state saves, with a memory operand of no particular size.
    // The extended ones address it with `rbx` as well as the register
    // written, which is why APX leaves them out: both references refuse
    // `xsave (%r16)` although REX2 could reach it. The two `fx` forms and
    // `clflush` are not in that group and take it.
    for (mnem, opcode, ext, egpr) in [
        ("fxsave", &[0x0f, 0xae][..], 0u8, true),
        ("fxrstor", &[0x0f, 0xae], 1, true),
        ("xsave", &[0x0f, 0xae], 4, false),
        ("xrstor", &[0x0f, 0xae], 5, false),
        ("xsaveopt", &[0x0f, 0xae], 6, false),
        ("clflush", &[0x0f, 0xae], 7, true),
        ("xrstors", &[0x0f, 0xc7], 3, false),
        ("xsavec", &[0x0f, 0xc7], 4, false),
        ("xsaves", &[0x0f, 0xc7], 5, false),
    ] {
        let extra = if egpr { 0 } else { NO_REX2 };
        let mut defs = vec![d(vec![Op::M(0)], opcode, ModRm::Ext(ext), 0).flags(extra)];
        // The state saves have 64-bit forms, `xsaveq` in AT&T, which differ
        // by REX.W.
        if ext != 7 {
            defs.push(d(vec![Op::M(0)], opcode, ModRm::Ext(ext), 64).flags(ONLY64 | extra));
        }
        t.insert(mnem, defs);
    }

    t.insert(
        "ud1",
        all_widths(|w| vec![Op::R(w), Op::Rm(w)], &[0x0f, 0xb9], ModRm::Reg),
    );

    // Add with carry or overflow only, whose mandatory prefix is not an
    // operand size override: 32 bits in every mode, 64 with REX.W.
    for (mnem, pfx) in [("adcx", 0x66u8), ("adox", 0xf3)] {
        t.insert(
            mnem,
            vec![
                d(
                    vec![Op::R(4), Op::Rm(4)],
                    &[0x0f, 0x38, 0xf6],
                    ModRm::Reg,
                    32,
                )
                .pfx(pfx)
                .flags(NO66),
                d(
                    vec![Op::R(8), Op::Rm(8)],
                    &[0x0f, 0x38, 0xf6],
                    ModRm::Reg,
                    64,
                )
                .pfx(pfx),
            ],
        );
    }
}

/// A selector operand: a word in memory, or a register of any size, whose
/// operand size is only honoured for the instructions that store one.
fn selector_rows(opcode: &[u8], ext: u8, stores: bool) -> Vec<Def> {
    let mut defs = vec![d(vec![Op::M(2)], opcode, ModRm::Ext(ext), 16).flags(NO66)];
    if stores {
        defs.extend(all_widths(|w| vec![Op::R(w)], opcode, ModRm::Ext(ext)));
    } else {
        // Unsized, so that a `w` suffix is one the instruction can take.
        defs.insert(0, d(vec![Op::Rm(2)], opcode, ModRm::Ext(ext), 0));
        defs.push(d(vec![Op::R(2)], opcode, ModRm::Ext(ext), 16).flags(NO66));
    }
    defs
}

fn install_misc(t: &mut Tbl) {
    // Zero-operand instructions.
    for (mnem, bytes) in [
        ("nop", &[0x90u8] as &[u8]),
        ("hlt", &[0xf4]),
        ("int3", &[0xcc]),
        ("int1", &[0xf1]),
        ("ud2", &[0x0f, 0x0b]),
        ("syscall", &[0x0f, 0x05]),
        ("sysret", &[0x0f, 0x07]),
        ("sysenter", &[0x0f, 0x34]),
        ("sysexit", &[0x0f, 0x35]),
        ("cpuid", &[0x0f, 0xa2]),
        ("rdtsc", &[0x0f, 0x31]),
        ("rdtscp", &[0x0f, 0x01, 0xf9]),
        ("rdmsr", &[0x0f, 0x32]),
        ("wrmsr", &[0x0f, 0x30]),
        ("rdpmc", &[0x0f, 0x33]),
        ("rsm", &[0x0f, 0xaa]),
        ("clts", &[0x0f, 0x06]),
        ("invd", &[0x0f, 0x08]),
        ("wbinvd", &[0x0f, 0x09]),
        ("xgetbv", &[0x0f, 0x01, 0xd0]),
        ("xsetbv", &[0x0f, 0x01, 0xd1]),
        ("clac", &[0x0f, 0x01, 0xca]),
        ("stac", &[0x0f, 0x01, 0xcb]),
        ("pause", &[0xf3, 0x90]),
        ("cld", &[0xfc]),
        ("std", &[0xfd]),
        ("cli", &[0xfa]),
        ("sti", &[0xfb]),
        ("clc", &[0xf8]),
        ("stc", &[0xf9]),
        ("cmc", &[0xf5]),
        ("lahf", &[0x9f]),
        ("sahf", &[0x9e]),
        ("endbr64", &[0xf3, 0x0f, 0x1e, 0xfa]),
        ("endbr32", &[0xf3, 0x0f, 0x1e, 0xfb]),
    ] {
        t.insert(mnem, vec![d(vec![], bytes, ModRm::None, 0)]);
    }
    // `sysretl` returns to 32-bit code and `sysretq` to 64-bit code.
    if let Some(defs) = t.get_mut("sysret") {
        defs.push(d(vec![], &[0x0f, 0x07], ModRm::None, 32).flags(NO66));
        defs.push(d(vec![], &[0x0f, 0x07], ModRm::None, 64).flags(ONLY64));
    }
    t.insert(
        "sysretq",
        vec![d(vec![], &[0x0f, 0x07], ModRm::None, 64).flags(ONLY64)],
    );
    t.insert("salc", vec![d(vec![], &[0xd6], ModRm::None, 0).flags(NO64)]);
    t.insert(
        "swapgs",
        vec![d(vec![], &[0x0f, 0x01, 0xf8], ModRm::None, 0).flags(ONLY64)],
    );

    // `nop` with an operand is the canonical multi-byte no-op.
    if let Some(defs) = t.get_mut("nop") {
        defs.extend(all_widths(
            |w| vec![Op::Rm(w)],
            &[0x0f, 0x1f],
            ModRm::Ext(0),
        ));
    }
}
