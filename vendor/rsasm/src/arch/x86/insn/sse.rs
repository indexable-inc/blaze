//! SSE through SSE4.2, plus AES-NI and `pclmulqdq`.
//!
//! The whole family is legacy-encoded: an optional mandatory prefix, a `0F`
//! escape (sometimes extended to `0F 38` or `0F 3A`), and `xmm` registers in
//! the ordinary ModRM fields. The mandatory prefix is what distinguishes the
//! four flavours of nearly every floating-point opcode:
//!
//! ```text
//! none  packed single   addps
//! 66    packed double   addpd
//! F3    scalar single   addss
//! F2    scalar double   addsd
//! ```
//!
//! and the same three bytes reappear in VEX and EVEX as the `pp` field, which
//! is why `avx.rs` can mirror these rows almost mechanically.

use super::mmx::{PACKED_BINARY, SHIFT_IMM};
use super::{ATT_ONLY, Def, ModRm, NO66, Op, R_IN_RM, Tbl, Vk, add, d};

/// The four floating-point flavours: name suffix, mandatory prefix, and the
/// width of a memory operand (a scalar form reads one element, not a vector).
const FLAVOURS: [(&str, u8, u8); 4] = [
    ("ps", 0x00, 0),
    ("pd", 0x66, 0),
    ("ss", 0xf3, 4),
    ("sd", 0xf2, 8),
];

/// Interns a generated mnemonic. There are a few hundred, and they live as
/// long as the table does.
fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

/// `op xmm, xmm/m`: the `/r` shape almost all of SSE has.
pub fn bin(pfx: u8, opcode: &[u8], memw: u8) -> Def {
    d(
        vec![Op::V(Vk::Xmm), Op::Vm(Vk::Xmm, memw)],
        opcode,
        ModRm::Reg,
        0,
    )
    .pfx(pfx)
}

/// The same with a trailing `ib`.
fn bin_imm(pfx: u8, opcode: &[u8], memw: u8) -> Def {
    d(
        vec![Op::V(Vk::Xmm), Op::Vm(Vk::Xmm, memw), Op::Imm(1)],
        opcode,
        ModRm::Reg,
        0,
    )
    .pfx(pfx)
}

fn install_moves(t: &mut Tbl) {
    // The aligned/unaligned pairs, each with a load and a store opcode.
    for (mnem, pfx, load) in [
        ("movups", 0x00u8, 0x10u8),
        ("movupd", 0x66, 0x10),
        ("movaps", 0x00, 0x28),
        ("movapd", 0x66, 0x28),
        ("movdqu", 0xf3, 0x6f),
    ] {
        let store = if load == 0x6f { 0x7f } else { load + 1 };
        add(
            t,
            mnem,
            vec![
                bin(pfx, &[0x0f, load], 0),
                d(
                    vec![Op::Vm(Vk::Xmm, 0), Op::V(Vk::Xmm)],
                    &[0x0f, store],
                    ModRm::Reg,
                    0,
                )
                .pfx(pfx),
            ],
        );
    }
    // `movdqa` is spelled without a `q` suffix, so it needs no special care,
    // but its opcodes are the `66` twins of `movdqu`'s.
    add(
        t,
        "movdqa",
        vec![
            bin(0x66, &[0x0f, 0x6f], 0),
            d(
                vec![Op::Vm(Vk::Xmm, 0), Op::V(Vk::Xmm)],
                &[0x0f, 0x7f],
                ModRm::Reg,
                0,
            )
            .pfx(0x66),
        ],
    );

    // Scalar moves. Register-to-register uses the load opcode, matching both
    // oracles; the store opcode is only reached with a memory destination.
    for (mnem, pfx, w) in [("movss", 0xf3u8, 4u8), ("movsd", 0xf2, 8)] {
        add(
            t,
            mnem,
            vec![
                bin(pfx, &[0x0f, 0x10], w),
                d(
                    vec![Op::Vm(Vk::Xmm, w), Op::V(Vk::Xmm)],
                    &[0x0f, 0x11],
                    ModRm::Reg,
                    0,
                )
                .pfx(pfx),
            ],
        );
    }

    // The 64-bit halves. `movhlps`/`movlhps` share opcodes with
    // `movlps`/`movhps` and are told apart by mod = 11 versus a memory
    // operand, so the register forms are listed first.
    add(
        t,
        "movhlps",
        vec![d(
            vec![Op::V(Vk::Xmm), Op::V(Vk::Xmm)],
            &[0x0f, 0x12],
            ModRm::Reg,
            0,
        )],
    );
    add(
        t,
        "movlhps",
        vec![d(
            vec![Op::V(Vk::Xmm), Op::V(Vk::Xmm)],
            &[0x0f, 0x16],
            ModRm::Reg,
            0,
        )],
    );
    for (mnem, pfx, load) in [
        ("movlps", 0x00u8, 0x12u8),
        ("movlpd", 0x66, 0x12),
        ("movhps", 0x00, 0x16),
        ("movhpd", 0x66, 0x16),
    ] {
        add(
            t,
            mnem,
            vec![
                d(vec![Op::V(Vk::Xmm), Op::M(8)], &[0x0f, load], ModRm::Reg, 0).pfx(pfx),
                d(
                    vec![Op::M(8), Op::V(Vk::Xmm)],
                    &[0x0f, load + 1],
                    ModRm::Reg,
                    0,
                )
                .pfx(pfx),
            ],
        );
    }

    // Non-temporal stores.
    for (mnem, pfx, opcode) in [
        ("movntps", 0x00u8, 0x2bu8),
        ("movntpd", 0x66, 0x2b),
        ("movntdq", 0x66, 0xe7),
    ] {
        add(
            t,
            mnem,
            vec![
                d(
                    vec![Op::M(16), Op::V(Vk::Xmm)],
                    &[0x0f, opcode],
                    ModRm::Reg,
                    0,
                )
                .pfx(pfx),
            ],
        );
    }
    add(
        t,
        "movntdqa",
        vec![
            d(
                vec![Op::V(Vk::Xmm), Op::M(16)],
                &[0x0f, 0x38, 0x2a],
                ModRm::Reg,
                0,
            )
            .pfx(0x66),
        ],
    );
    for (mnem, w) in [("movnti", 4u8), ("movntil", 4), ("movntiq", 8)] {
        add(
            t,
            mnem,
            vec![d(
                vec![Op::M(w), Op::R(w)],
                &[0x0f, 0xc3],
                ModRm::Reg,
                w * 8,
            )],
        );
    }

    // `movd`/`movq` between an xmm register and a GPR, memory or another xmm.
    add(
        t,
        "movd",
        vec![
            d(
                vec![Op::V(Vk::Xmm), Op::Rm(4)],
                &[0x0f, 0x6e],
                ModRm::Reg,
                0,
            )
            .pfx(0x66),
            d(
                vec![Op::Rm(4), Op::V(Vk::Xmm)],
                &[0x0f, 0x7e],
                ModRm::Reg,
                0,
            )
            .pfx(0x66),
            // Both references also take a 64-bit register, as `movq`.
            d(
                vec![Op::V(Vk::Xmm), Op::R(8)],
                &[0x0f, 0x6e],
                ModRm::Reg,
                64,
            )
            .pfx(0x66)
            .flags(R_IN_RM),
            d(
                vec![Op::R(8), Op::V(Vk::Xmm)],
                &[0x0f, 0x7e],
                ModRm::Reg,
                64,
            )
            .pfx(0x66)
            .flags(R_IN_RM),
        ],
    );
    // See the note in `mmx.rs` on how AT&T `movq` reaches these rows.
    add(
        t,
        "movq",
        vec![
            d(
                vec![Op::V(Vk::Xmm), Op::R(8)],
                &[0x0f, 0x6e],
                ModRm::Reg,
                64,
            )
            .pfx(0x66),
            d(
                vec![Op::R(8), Op::V(Vk::Xmm)],
                &[0x0f, 0x7e],
                ModRm::Reg,
                64,
            )
            .pfx(0x66),
            bin(0xf3, &[0x0f, 0x7e], 8),
            d(
                vec![Op::Vm(Vk::Xmm, 8), Op::V(Vk::Xmm)],
                &[0x0f, 0xd6],
                ModRm::Reg,
                0,
            )
            .pfx(0x66),
        ],
    );
    add(
        t,
        "movq2dq",
        vec![
            d(
                vec![Op::V(Vk::Xmm), Op::V(Vk::Mm)],
                &[0x0f, 0xd6],
                ModRm::Reg,
                0,
            )
            .pfx(0xf3),
        ],
    );
    add(
        t,
        "movdq2q",
        vec![
            d(
                vec![Op::V(Vk::Mm), Op::V(Vk::Xmm)],
                &[0x0f, 0xd6],
                ModRm::Reg,
                0,
            )
            .pfx(0xf2),
        ],
    );

    // SSE3 duplicating loads.
    add(t, "movsldup", vec![bin(0xf3, &[0x0f, 0x12], 0)]);
    add(t, "movshdup", vec![bin(0xf3, &[0x0f, 0x16], 0)]);
    add(t, "movddup", vec![bin(0xf2, &[0x0f, 0x12], 8)]);
    add(
        t,
        "lddqu",
        vec![
            d(
                vec![Op::V(Vk::Xmm), Op::M(16)],
                &[0x0f, 0xf0],
                ModRm::Reg,
                0,
            )
            .pfx(0xf2),
        ],
    );
    add(
        t,
        "maskmovdqu",
        vec![
            d(
                vec![Op::V(Vk::Xmm), Op::V(Vk::Xmm)],
                &[0x0f, 0xf7],
                ModRm::Reg,
                0,
            )
            .pfx(0x66),
        ],
    );
}

fn install_arithmetic(t: &mut Tbl) {
    const ALL: &[&str] = &["ps", "pd", "ss", "sd"];
    const SINGLE: &[&str] = &["ps", "ss"];
    const PACKED: &[&str] = &["ps", "pd"];
    // `0F 5x`: one opcode per operation, one prefix per flavour.
    #[rustfmt::skip]
    const OPS: &[(&str, u8, &[&str])] = &[
        // (stem, opcode, the flavours that exist)
        ("sqrt", 0x51, ALL),
        // Reciprocal approximations were only ever single precision.
        ("rsqrt", 0x52, SINGLE), ("rcp", 0x53, SINGLE),
        // Bitwise operations make no sense on one scalar element.
        ("and", 0x54, PACKED), ("andn", 0x55, PACKED),
        ("or", 0x56, PACKED), ("xor", 0x57, PACKED),
        ("add", 0x58, ALL), ("mul", 0x59, ALL),
        ("sub", 0x5c, ALL), ("min", 0x5d, ALL),
        ("div", 0x5e, ALL), ("max", 0x5f, ALL),
    ];
    for &(stem, opcode, which) in OPS {
        for &(flav, pfx, memw) in &FLAVOURS {
            if !which.contains(&flav) {
                continue;
            }
            add(
                t,
                leak(format!("{stem}{flav}")),
                vec![bin(pfx, &[0x0f, opcode], memw)],
            );
        }
    }

    // Compares take a predicate immediate.
    for &(flav, pfx, memw) in &FLAVOURS {
        add(
            t,
            leak(format!("cmp{flav}")),
            vec![bin_imm(pfx, &[0x0f, 0xc2], memw)],
        );
    }
    // `2E` is the unordered compare, `2F` the ordered one.
    for (mnem, pfx, op, memw) in [
        ("ucomiss", 0x00u8, 0x2eu8, 4u8),
        ("ucomisd", 0x66, 0x2e, 8),
        ("comiss", 0x00, 0x2f, 4),
        ("comisd", 0x66, 0x2f, 8),
    ] {
        add(t, mnem, vec![bin(pfx, &[0x0f, op], memw)]);
    }

    // SSE3 horizontal and alternating arithmetic.
    for (mnem, pfx, op) in [
        ("addsubps", 0xf2u8, 0xd0u8),
        ("addsubpd", 0x66, 0xd0),
        ("haddps", 0xf2, 0x7c),
        ("haddpd", 0x66, 0x7c),
        ("hsubps", 0xf2, 0x7d),
        ("hsubpd", 0x66, 0x7d),
    ] {
        add(t, mnem, vec![bin(pfx, &[0x0f, op], 0)]);
    }
}

fn install_shuffles(t: &mut Tbl) {
    for (mnem, pfx, op) in [
        ("unpcklps", 0x00u8, 0x14u8),
        ("unpcklpd", 0x66, 0x14),
        ("unpckhps", 0x00, 0x15),
        ("unpckhpd", 0x66, 0x15),
    ] {
        add(t, mnem, vec![bin(pfx, &[0x0f, op], 0)]);
    }
    for (mnem, pfx) in [("shufps", 0x00u8), ("shufpd", 0x66)] {
        add(t, mnem, vec![bin_imm(pfx, &[0x0f, 0xc6], 0)]);
    }
    for (mnem, pfx) in [("pshufd", 0x66u8), ("pshuflw", 0xf2), ("pshufhw", 0xf3)] {
        add(t, mnem, vec![bin_imm(pfx, &[0x0f, 0x70], 0)]);
    }
    add(t, "palignr", vec![bin_imm(0x66, &[0x0f, 0x3a, 0x0f], 0)]);
    // The MMX form of `palignr` predates the xmm one and takes no prefix.
    add(
        t,
        "palignr",
        vec![d(
            vec![Op::V(Vk::Mm), Op::Vm(Vk::Mm, 0), Op::Imm(1)],
            &[0x0f, 0x3a, 0x0f],
            ModRm::Reg,
            0,
        )],
    );
}

fn install_conversions(t: &mut Tbl) {
    // Scalar integer conversions, where the AT&T `l`/`q` suffix picks the GPR
    // width and therefore REX.W.
    for (mnem, pfx) in [("cvtsi2ss", 0xf3u8), ("cvtsi2sd", 0xf2)] {
        add(
            t,
            mnem,
            vec![
                // The GPR is 32 bits in every mode without REX.W, 16-bit
                // mode included, so the size is only for matching.
                d(
                    vec![Op::V(Vk::Xmm), Op::Rm(4)],
                    &[0x0f, 0x2a],
                    ModRm::Reg,
                    32,
                )
                .pfx(pfx)
                .flags(NO66),
                d(
                    vec![Op::V(Vk::Xmm), Op::Rm(8)],
                    &[0x0f, 0x2a],
                    ModRm::Reg,
                    64,
                )
                .pfx(pfx),
            ],
        );
    }
    for (mnem, pfx, op, memw) in [
        ("cvttss2si", 0xf3u8, 0x2cu8, 4u8),
        ("cvttsd2si", 0xf2, 0x2c, 8),
        ("cvtss2si", 0xf3, 0x2d, 4),
        ("cvtsd2si", 0xf2, 0x2d, 8),
    ] {
        add(
            t,
            mnem,
            vec![
                d(
                    vec![Op::R(4), Op::Vm(Vk::Xmm, memw)],
                    &[0x0f, op],
                    ModRm::Reg,
                    32,
                )
                .pfx(pfx)
                .flags(NO66),
                d(
                    vec![Op::R(8), Op::Vm(Vk::Xmm, memw)],
                    &[0x0f, op],
                    ModRm::Reg,
                    64,
                )
                .pfx(pfx),
            ],
        );
    }

    // Packed conversions between xmm registers.
    for (mnem, pfx, op, memw) in [
        ("cvtps2pd", 0x00u8, 0x5au8, 8u8),
        ("cvtpd2ps", 0x66, 0x5a, 0),
        ("cvtss2sd", 0xf3, 0x5a, 4),
        ("cvtsd2ss", 0xf2, 0x5a, 8),
        ("cvtdq2ps", 0x00, 0x5b, 0),
        ("cvtps2dq", 0x66, 0x5b, 0),
        ("cvttps2dq", 0xf3, 0x5b, 0),
        ("cvtdq2pd", 0xf3, 0xe6, 8),
        ("cvtpd2dq", 0xf2, 0xe6, 0),
        ("cvttpd2dq", 0x66, 0xe6, 0),
    ] {
        add(t, mnem, vec![bin(pfx, &[0x0f, op], memw)]);
    }

    // The MMX bridges.
    for (mnem, pfx, op) in [
        ("cvtps2pi", 0x00u8, 0x2du8),
        ("cvttps2pi", 0x00, 0x2c),
        ("cvtpd2pi", 0x66, 0x2d),
        ("cvttpd2pi", 0x66, 0x2c),
    ] {
        add(
            t,
            mnem,
            vec![
                d(
                    vec![Op::V(Vk::Mm), Op::Vm(Vk::Xmm, 0)],
                    &[0x0f, op],
                    ModRm::Reg,
                    0,
                )
                .pfx(pfx),
            ],
        );
    }
    for (mnem, pfx) in [("cvtpi2ps", 0x00u8), ("cvtpi2pd", 0x66)] {
        add(
            t,
            mnem,
            vec![
                d(
                    vec![Op::V(Vk::Xmm), Op::Vm(Vk::Mm, 0)],
                    &[0x0f, 0x2a],
                    ModRm::Reg,
                    0,
                )
                .pfx(pfx),
            ],
        );
    }
}

fn install_integer(t: &mut Tbl) {
    // Every MMX packed operation again, on `xmm` under a `66` prefix.
    for &(mnem, op) in PACKED_BINARY {
        add(t, mnem, vec![bin(0x66, &[0x0f, op], 0)]);
    }
    for &(mnem, op, ext) in SHIFT_IMM {
        add(
            t,
            mnem,
            vec![
                d(
                    vec![Op::V(Vk::Xmm), Op::Imm(1)],
                    &[0x0f, op],
                    ModRm::Ext(ext),
                    0,
                )
                .pfx(0x66),
            ],
        );
    }
    // Whole-register byte shifts, which have no MMX equivalent.
    for (mnem, ext) in [("psrldq", 3u8), ("pslldq", 7)] {
        add(
            t,
            mnem,
            vec![
                d(
                    vec![Op::V(Vk::Xmm), Op::Imm(1)],
                    &[0x0f, 0x73],
                    ModRm::Ext(ext),
                    0,
                )
                .pfx(0x66),
            ],
        );
    }

    // The mask lands in a 32-bit register, which may be written as its
    // 64-bit whole without asking for REX.W.
    for (mnem, pfx, op) in [
        ("pmovmskb", 0x66u8, 0xd7u8),
        ("movmskps", 0x00, 0x50),
        ("movmskpd", 0x66, 0x50),
    ] {
        add(
            t,
            mnem,
            [4u8, 8]
                .iter()
                .map(|&w| d(vec![Op::R(w), Op::V(Vk::Xmm)], &[0x0f, op], ModRm::Reg, 0).pfx(pfx))
                .collect(),
        );
    }

    // SSSE3, which lives on the `0F 38` map and exists for both register files.
    #[rustfmt::skip]
    const SSSE3: &[(&str, u8)] = &[
        ("pshufb", 0x00), ("phaddw", 0x01), ("phaddd", 0x02), ("phaddsw", 0x03),
        ("pmaddubsw", 0x04), ("phsubw", 0x05), ("phsubd", 0x06), ("phsubsw", 0x07),
        ("psignb", 0x08), ("psignw", 0x09), ("psignd", 0x0a), ("pmulhrsw", 0x0b),
        ("pabsb", 0x1c), ("pabsw", 0x1d), ("pabsd", 0x1e),
    ];
    for &(mnem, op) in SSSE3 {
        add(
            t,
            mnem,
            vec![d(
                vec![Op::V(Vk::Mm), Op::Vm(Vk::Mm, 0)],
                &[0x0f, 0x38, op],
                ModRm::Reg,
                0,
            )],
        );
        add(t, mnem, vec![bin(0x66, &[0x0f, 0x38, op], 0)]);
    }

    // SSE4.1 and SSE4.2 on the `0F 38` map, all `66`-prefixed.
    #[rustfmt::skip]
    const SSE4_38: &[(&str, u8)] = &[
        ("ptest", 0x17),
        ("pmuldq", 0x28), ("pcmpeqq", 0x29), ("packusdw", 0x2b),
        ("pminsb", 0x38), ("pminsd", 0x39), ("pminuw", 0x3a), ("pminud", 0x3b),
        ("pmaxsb", 0x3c), ("pmaxsd", 0x3d), ("pmaxuw", 0x3e), ("pmaxud", 0x3f),
        ("pmulld", 0x40), ("phminposuw", 0x41),
        ("pcmpgtq", 0x37),
        ("aesimc", 0xdb), ("aesenc", 0xdc), ("aesenclast", 0xdd),
        ("aesdec", 0xde), ("aesdeclast", 0xdf),
    ];
    for &(mnem, op) in SSE4_38 {
        add(t, mnem, vec![bin(0x66, &[0x0f, 0x38, op], 0)]);
    }
    // The sign-extending and zero-extending loads read a fraction of a
    // register, so their memory operands are narrower than 128 bits.
    #[rustfmt::skip]
    const PMOVX: &[(&str, u8, u8)] = &[
        ("pmovsxbw", 0x20, 8), ("pmovsxbd", 0x21, 4), ("pmovsxbq", 0x22, 2),
        ("pmovsxwd", 0x23, 8), ("pmovsxwq", 0x24, 4), ("pmovsxdq", 0x25, 8),
        ("pmovzxbw", 0x30, 8), ("pmovzxbd", 0x31, 4), ("pmovzxbq", 0x32, 2),
        ("pmovzxwd", 0x33, 8), ("pmovzxwq", 0x34, 4), ("pmovzxdq", 0x35, 8),
    ];
    for &(mnem, op, memw) in PMOVX {
        add(t, mnem, vec![bin(0x66, &[0x0f, 0x38, op], memw)]);
    }
    // The `blendv` group takes `xmm0` implicitly as its mask.
    for (mnem, op) in [("pblendvb", 0x10u8), ("blendvps", 0x14), ("blendvpd", 0x15)] {
        add(t, mnem, vec![bin(0x66, &[0x0f, 0x38, op], 0)]);
        add(
            t,
            mnem,
            vec![
                d(
                    vec![Op::V(Vk::Xmm), Op::Vm(Vk::Xmm, 0), Op::Fixed("xmm0")],
                    &[0x0f, 0x38, op],
                    ModRm::Reg,
                    0,
                )
                .pfx(0x66),
            ],
        );
    }

    // SSE4.1 and SSE4.2 on the `0F 3A` map, all with an immediate.
    #[rustfmt::skip]
    const SSE4_3A: &[(&str, u8)] = &[
        ("roundps", 0x08), ("roundpd", 0x09),
        ("blendps", 0x0c), ("blendpd", 0x0d), ("pblendw", 0x0e),
        ("dpps", 0x40), ("dppd", 0x41), ("mpsadbw", 0x42),
        ("pcmpestrm", 0x60), ("pcmpestri", 0x61),
        ("pcmpistrm", 0x62), ("pcmpistri", 0x63),
        ("pclmulqdq", 0x44), ("aeskeygenassist", 0xdf),
    ];
    for &(mnem, op) in SSE4_3A {
        add(t, mnem, vec![bin_imm(0x66, &[0x0f, 0x3a, op], 0)]);
    }
    for (mnem, op, memw) in [("roundss", 0x0au8, 4u8), ("roundsd", 0x0b, 8)] {
        add(t, mnem, vec![bin_imm(0x66, &[0x0f, 0x3a, op], memw)]);
    }
    add(t, "insertps", vec![bin_imm(0x66, &[0x0f, 0x3a, 0x21], 4)]);

    // Element insert and extract. The GPR halves are ordinary `r/m` operands.
    add(
        t,
        "pinsrw",
        vec![
            d(
                vec![Op::V(Vk::Xmm), Op::R(4), Op::Imm(1)],
                &[0x0f, 0xc4],
                ModRm::Reg,
                0,
            )
            .pfx(0x66),
            d(
                vec![Op::V(Vk::Xmm), Op::R(8), Op::Imm(1)],
                &[0x0f, 0xc4],
                ModRm::Reg,
                0,
            )
            .pfx(0x66),
            d(
                vec![Op::V(Vk::Xmm), Op::M(2), Op::Imm(1)],
                &[0x0f, 0xc4],
                ModRm::Reg,
                0,
            )
            .pfx(0x66),
        ],
    );
    add(
        t,
        "pextrw",
        vec![
            d(
                vec![Op::R(4), Op::V(Vk::Xmm), Op::Imm(1)],
                &[0x0f, 0xc5],
                ModRm::Reg,
                0,
            )
            .pfx(0x66),
            d(
                vec![Op::R(8), Op::V(Vk::Xmm), Op::Imm(1)],
                &[0x0f, 0xc5],
                ModRm::Reg,
                0,
            )
            .pfx(0x66),
            // The SSE4.1 form can write memory, which `0F C5` cannot.
            d(
                vec![Op::M(2), Op::V(Vk::Xmm), Op::Imm(1)],
                &[0x0f, 0x3a, 0x15],
                ModRm::Reg,
                0,
            )
            .pfx(0x66),
        ],
    );
    for (mnem, op, w, opsize) in [
        ("pextrd", 0x16u8, 4u8, 0u8),
        ("pextrq", 0x16, 8, 64),
        ("extractps", 0x17, 4, 0),
    ] {
        add(
            t,
            mnem,
            vec![
                d(
                    vec![Op::Rm(w), Op::V(Vk::Xmm), Op::Imm(1)],
                    &[0x0f, 0x3a, op],
                    ModRm::Reg,
                    opsize,
                )
                .pfx(0x66),
            ],
        );
    }
    // `extractps` also writes a 64-bit register, zero-extended, with no
    // `REX.W`.
    add(
        t,
        "extractps",
        vec![
            d(
                vec![Op::R(8), Op::V(Vk::Xmm), Op::Imm(1)],
                &[0x0f, 0x3a, 0x17],
                ModRm::Reg,
                0,
            )
            .pfx(0x66)
            .flags(R_IN_RM),
        ],
    );
    for (mnem, op, w, opsize) in [("pinsrd", 0x22u8, 4u8, 0u8), ("pinsrq", 0x22, 8, 64)] {
        add(
            t,
            mnem,
            vec![
                d(
                    vec![Op::V(Vk::Xmm), Op::Rm(w), Op::Imm(1)],
                    &[0x0f, 0x3a, op],
                    ModRm::Reg,
                    opsize,
                )
                .pfx(0x66),
            ],
        );
    }
}

/// `pextrb`/`pinsrb`: a byte element travels through a 32- or 64-bit register
/// or a single byte of memory, never through an 8-bit register. The 64-bit
/// register takes no `REX.W`: the upper half is zeroed either way. Unlike
/// `pextrw`'s `0F C5`, the extract's register is its r/m operand.
fn install_byte_elements(t: &mut Tbl) {
    add(
        t,
        "pextrb",
        vec![
            d(
                vec![Op::R(4), Op::V(Vk::Xmm), Op::Imm(1)],
                &[0x0f, 0x3a, 0x14],
                ModRm::Reg,
                0,
            )
            .pfx(0x66)
            .flags(R_IN_RM),
            d(
                vec![Op::R(8), Op::V(Vk::Xmm), Op::Imm(1)],
                &[0x0f, 0x3a, 0x14],
                ModRm::Reg,
                0,
            )
            .pfx(0x66)
            .flags(R_IN_RM),
            d(
                vec![Op::M(1), Op::V(Vk::Xmm), Op::Imm(1)],
                &[0x0f, 0x3a, 0x14],
                ModRm::Reg,
                0,
            )
            .pfx(0x66),
        ],
    );
    add(
        t,
        "pinsrb",
        vec![
            d(
                vec![Op::V(Vk::Xmm), Op::R(4), Op::Imm(1)],
                &[0x0f, 0x3a, 0x20],
                ModRm::Reg,
                0,
            )
            .pfx(0x66),
            d(
                vec![Op::V(Vk::Xmm), Op::R(8), Op::Imm(1)],
                &[0x0f, 0x3a, 0x20],
                ModRm::Reg,
                0,
            )
            .pfx(0x66),
            d(
                vec![Op::V(Vk::Xmm), Op::M(1), Op::Imm(1)],
                &[0x0f, 0x3a, 0x20],
                ModRm::Reg,
                0,
            )
            .pfx(0x66),
        ],
    );
    // The quadword unpacks exist only on xmm; MMX had no 64-bit elements.
    add(t, "punpcklqdq", vec![bin(0x66, &[0x0f, 0x6c], 0)]);
    add(t, "punpckhqdq", vec![bin(0x66, &[0x0f, 0x6d], 0)]);
}

fn install_scalar_bit_ops(t: &mut Tbl) {
    // `crc32` is written with an explicit suffix in AT&T because the suffix
    // names the *source* width while the destination is a plain GPR. A word
    // or doubleword source is read at the operand size, so `crc32w` has a
    // `66` outside 16-bit mode and `crc32l` in it; a byte source has none.
    for (mnem, src, opcode, opsize) in [
        ("crc32b", 1u8, 0xf0u8, 8u8),
        ("crc32w", 2, 0xf1, 16),
        ("crc32l", 4, 0xf1, 32),
        ("crc32q", 8, 0xf1, 64),
    ] {
        let dst = if src == 8 { 8 } else { 4 };
        let def = d(
            vec![Op::R(dst), Op::Rm(src)],
            &[0x0f, 0x38, opcode],
            ModRm::Reg,
            opsize,
        )
        .pfx(0xf2);
        // The suffixed spellings are AT&T's.
        add(t, mnem, vec![def.clone().flags(ATT_ONLY)]);
        add(t, "crc32", vec![def]);
    }
    // The 64-bit destination with a byte source needs REX.W but no `66`.
    let crc32bq = d(
        vec![Op::R(8), Op::Rm(1)],
        &[0x0f, 0x38, 0xf0],
        ModRm::Reg,
        64,
    )
    .pfx(0xf2);
    add(t, "crc32b", vec![crc32bq.clone().flags(ATT_ONLY)]);
    add(t, "crc32", vec![crc32bq]);

    for (mnem, op) in [("popcnt", 0xb8u8), ("lzcnt", 0xbd), ("tzcnt", 0xbc)] {
        add(
            t,
            mnem,
            [2u8, 4, 8]
                .iter()
                .map(|&w| d(vec![Op::R(w), Op::Rm(w)], &[0x0f, op], ModRm::Reg, w * 8).pfx(0xf3))
                .collect(),
        );
    }

    // Fences and cache control, which arrived with SSE and SSE2.
    for (mnem, bytes) in [
        ("sfence", &[0x0fu8, 0xae, 0xf8] as &[u8]),
        ("lfence", &[0x0f, 0xae, 0xe8]),
        ("mfence", &[0x0f, 0xae, 0xf0]),
    ] {
        add(t, mnem, vec![d(vec![], bytes, ModRm::None, 0)]);
    }
    for (mnem, ext) in [
        ("prefetchnta", 0u8),
        ("prefetcht0", 1),
        ("prefetcht1", 2),
        ("prefetcht2", 3),
    ] {
        add(
            t,
            mnem,
            vec![d(vec![Op::M(0)], &[0x0f, 0x18], ModRm::Ext(ext), 0)],
        );
    }
    add(
        t,
        "ldmxcsr",
        vec![d(vec![Op::M(4)], &[0x0f, 0xae], ModRm::Ext(2), 0)],
    );
    add(
        t,
        "stmxcsr",
        vec![d(vec![Op::M(4)], &[0x0f, 0xae], ModRm::Ext(3), 0)],
    );
}

pub fn install(t: &mut Tbl) {
    install_moves(t);
    install_arithmetic(t);
    install_shuffles(t);
    install_conversions(t);
    install_integer(t);
    install_byte_elements(t);
    install_scalar_bit_ops(t);
}
