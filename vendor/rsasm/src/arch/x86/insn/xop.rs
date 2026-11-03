//! AMD's XOP and FMA4 vector extensions.
//!
//! XOP is VEX's three-byte layout behind an `8F` escape, with maps 8, 9 and
//! 10 of its own. Many of its operations take four operands, and let `W`
//! decide which of the last two is the r/m operand, so that either can be
//! memory: with `W0` the third operand is r/m and the fourth rides in the top
//! nibble of an immediate byte, with `W1` the other way round. Where both are
//! registers both encodings are valid; see `four` for which is assembled.
//! The shifts and rotates make the same choice between r/m and `vvvv`.
//!
//! FMA4 is the same idea in the ordinary VEX `0F 3A` map, and `vpermil2ps`
//! with an extra immediate sharing the `is4` byte.

use super::{Def, ModRm, Op, Tbl, Vk, add, d};

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

fn row(ops: Vec<Op>, pfx: u8, map: u8, op: u8, vlen: u16, w: bool) -> Def {
    d(ops, &[op], ModRm::Reg, if w { 64 } else { 0 })
        .pfx(pfx)
        .map(map)
        .vex(vlen)
}

fn vk(l: u16) -> Vk {
    if l == 256 { Vk::Ymm } else { Vk::Xmm }
}

/// The two operand orders of a four-operand XOP or FMA4 form. `memw` is the
/// width of the memory operand, 0 for the register's. Between registers
/// both references choose `W0` for XOP and `W1` for FMA4, which is what
/// `w1_first` says.
fn four(t: &mut Tbl, mnem: &'static str, op: (u8, u8, u8), l: u16, memw: u8, w1_first: bool) {
    let (pfx, map, op) = op;
    let k = vk(l);
    let w0 = row(
        vec![Op::V(k), Op::Nds(k), Op::Vm(k, memw), Op::Is4(k)],
        pfx,
        map,
        op,
        l,
        false,
    );
    let w1 = row(
        vec![Op::V(k), Op::Nds(k), Op::Is4(k), Op::Vm(k, memw)],
        pfx,
        map,
        op,
        l,
        true,
    );
    add(t, mnem, if w1_first { vec![w1, w0] } else { vec![w0, w1] });
}

fn install_fma4(t: &mut Tbl) {
    // (stem, packed opcode, whether the scalars follow at `+2` and `+3`)
    for (stem, op, scalar) in [
        ("vfmaddsub", 0x5cu8, false),
        ("vfmsubadd", 0x5e, false),
        ("vfmadd", 0x68, true),
        ("vfmsub", 0x6c, true),
        ("vfnmadd", 0x78, true),
        ("vfnmsub", 0x7c, true),
    ] {
        for (flav, bump) in [("ps", 0u8), ("pd", 1)] {
            let mnem = leak(format!("{stem}{flav}"));
            for l in [128, 256] {
                four(t, mnem, (0x66, 3, op + bump), l, 0, true);
            }
        }
        if scalar {
            for (flav, bump, memw) in [("ss", 2u8, 4u8), ("sd", 3, 8)] {
                four(
                    t,
                    leak(format!("{stem}{flav}")),
                    (0x66, 3, op + bump),
                    128,
                    memw,
                    true,
                );
            }
        }
    }
}

fn install_xop(t: &mut Tbl) {
    let x = Vk::Xmm;
    // Four-operand forms in map 8.
    for l in [128u16, 256] {
        four(t, "vpcmov", (0x00, 8, 0xa2), l, 0, false);
    }
    four(t, "vpperm", (0x00, 8, 0xa3), 128, 0, false);
    for (mnem, op) in [
        ("vpmacsww", 0x95u8),
        ("vpmacswd", 0x96),
        ("vpmacsdd", 0x9e),
        ("vpmacsdql", 0x97),
        ("vpmacsdqh", 0x9f),
        ("vpmacssww", 0x85),
        ("vpmacsswd", 0x86),
        ("vpmacssdd", 0x8e),
        ("vpmacssdql", 0x87),
        ("vpmacssdqh", 0x8f),
        ("vpmadcsswd", 0xa6),
        ("vpmadcswd", 0xb6),
    ] {
        // These have no `W1` form: the fourth operand is always a register.
        add(
            t,
            mnem,
            vec![row(
                vec![Op::V(x), Op::Nds(x), Op::Vm(x, 0), Op::Is4(x)],
                0x00,
                8,
                op,
                128,
                false,
            )],
        );
    }
    // Integer compares with a predicate immediate.
    for (flav, op) in [
        ("b", 0xccu8),
        ("w", 0xcd),
        ("d", 0xce),
        ("q", 0xcf),
        ("ub", 0xec),
        ("uw", 0xed),
        ("ud", 0xee),
        ("uq", 0xef),
    ] {
        add(
            t,
            leak(format!("vpcom{flav}")),
            vec![row(
                vec![Op::V(x), Op::Nds(x), Op::Vm(x, 0), Op::Imm(1)],
                0x00,
                8,
                op,
                128,
                false,
            )],
        );
    }
    // Rotates and shifts: by an immediate in map 8, or by a vector of counts
    // in map 9, where `W` again picks which operand is r/m.
    for (i, elem) in ["b", "w", "d", "q"].iter().enumerate() {
        let i = i as u8;
        add(
            t,
            leak(format!("vprot{elem}")),
            vec![
                row(
                    vec![Op::V(x), Op::Vm(x, 0), Op::Nds(x)],
                    0x00,
                    9,
                    0x90 + i,
                    128,
                    false,
                ),
                row(
                    vec![Op::V(x), Op::Nds(x), Op::Vm(x, 0)],
                    0x00,
                    9,
                    0x90 + i,
                    128,
                    true,
                ),
                row(
                    vec![Op::V(x), Op::Vm(x, 0), Op::Imm(1)],
                    0x00,
                    8,
                    0xc0 + i,
                    128,
                    false,
                ),
            ],
        );
        for (stem, base) in [("vpshl", 0x94u8), ("vpsha", 0x98)] {
            add(
                t,
                leak(format!("{stem}{elem}")),
                vec![
                    row(
                        vec![Op::V(x), Op::Vm(x, 0), Op::Nds(x)],
                        0x00,
                        9,
                        base + i,
                        128,
                        false,
                    ),
                    row(
                        vec![Op::V(x), Op::Nds(x), Op::Vm(x, 0)],
                        0x00,
                        9,
                        base + i,
                        128,
                        true,
                    ),
                ],
            );
        }
    }
    // Two-operand forms in map 9.
    for l in [128u16, 256] {
        let k = vk(l);
        for (mnem, op) in [("vfrczps", 0x80u8), ("vfrczpd", 0x81)] {
            add(
                t,
                mnem,
                vec![row(vec![Op::V(k), Op::Vm(k, 0)], 0x00, 9, op, l, false)],
            );
        }
    }
    for (mnem, op, memw) in [("vfrczss", 0x82u8, 4u8), ("vfrczsd", 0x83, 8)] {
        add(
            t,
            mnem,
            vec![row(
                vec![Op::V(x), Op::Vm(x, memw)],
                0x00,
                9,
                op,
                128,
                false,
            )],
        );
    }
    for (mnem, op) in [
        ("vphaddbw", 0xc1u8),
        ("vphaddbd", 0xc2),
        ("vphaddbq", 0xc3),
        ("vphaddwd", 0xc6),
        ("vphaddwq", 0xc7),
        ("vphadddq", 0xcb),
        ("vphaddubw", 0xd1),
        ("vphaddubd", 0xd2),
        ("vphaddubq", 0xd3),
        ("vphadduwd", 0xd6),
        ("vphadduwq", 0xd7),
        ("vphaddudq", 0xdb),
        ("vphsubbw", 0xe1),
        ("vphsubwd", 0xe2),
        ("vphsubdq", 0xe3),
    ] {
        add(
            t,
            mnem,
            vec![row(vec![Op::V(x), Op::Vm(x, 0)], 0x00, 9, op, 128, false)],
        );
    }
    // `vpermil2ps`: four vector operands and a four-bit control that shares
    // the `is4` byte.
    for (mnem, op) in [("vpermil2ps", 0x48u8), ("vpermil2pd", 0x49)] {
        for l in [128u16, 256] {
            let k = vk(l);
            add(
                t,
                mnem,
                vec![
                    row(
                        vec![Op::V(k), Op::Nds(k), Op::Vm(k, 0), Op::Is4(k), Op::Imm(1)],
                        0x66,
                        3,
                        op,
                        l,
                        false,
                    ),
                    row(
                        vec![Op::V(k), Op::Nds(k), Op::Is4(k), Op::Vm(k, 0), Op::Imm(1)],
                        0x66,
                        3,
                        op,
                        l,
                        true,
                    ),
                ],
            );
        }
    }
}

pub fn install(t: &mut Tbl) {
    install_fma4(t);
    install_xop(t);
}
