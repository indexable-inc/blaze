//! MMX and 3DNow!.
//!
//! MMX is the oldest SIMD family and the simplest to encode: a `0F` escape, no
//! mandatory prefix, and the `mm0`-`mm7` registers in the ordinary ModRM
//! fields. Most of these opcodes gained an `xmm` twin under a `66` prefix in
//! SSE2, so the opcode lists here are shared with `sse.rs`, which re-issues
//! them against `xmm` under a `66` prefix.
//!
//! 3DNow! is the odd one out. `0F 0F` is a prefix rather than an opcode, and
//! the operation is chosen by a byte *after* the ModRM and displacement, where
//! an immediate would sit. See [`Def::suffix`].

use super::{Def, ModRm, Op, R_IN_RM, Tbl, Vk, add, d};

/// `op mm, mm/m64`: the shape almost every MMX instruction has.
fn bin(op: u8) -> Vec<Def> {
    vec![d(
        vec![Op::V(Vk::Mm), Op::Vm(Vk::Mm, 0)],
        &[0x0f, op],
        ModRm::Reg,
        0,
    )]
}

/// `op mm, imm8`: the shift group's `0F 71`-`0F 73 /digit` forms.
fn shift_imm(op: u8, ext: u8) -> Def {
    d(
        vec![Op::V(Vk::Mm), Op::Imm(1)],
        &[0x0f, op],
        ModRm::Ext(ext),
        0,
    )
}

/// The `op dst, src/mem` opcodes MMX and SSE2 share.
///
/// SSE2 gave every one of these an `xmm` twin at the same opcode under a `66`
/// prefix, so `sse.rs` builds its integer set straight from this list.
#[rustfmt::skip]
pub const PACKED_BINARY: &[(&str, u8)] = &[
        ("punpcklbw", 0x60), ("punpcklwd", 0x61), ("punpckldq", 0x62),
        ("packsswb", 0x63),
        ("pcmpgtb", 0x64), ("pcmpgtw", 0x65), ("pcmpgtd", 0x66),
        ("packuswb", 0x67),
        ("punpckhbw", 0x68), ("punpckhwd", 0x69), ("punpckhdq", 0x6a),
        ("packssdw", 0x6b),
        ("pcmpeqb", 0x74), ("pcmpeqw", 0x75), ("pcmpeqd", 0x76),
        ("psrlw", 0xd1), ("psrld", 0xd2), ("psrlq", 0xd3),
        ("paddq", 0xd4), ("pmullw", 0xd5),
        ("psubusb", 0xd8), ("psubusw", 0xd9), ("pminub", 0xda), ("pand", 0xdb),
        ("paddusb", 0xdc), ("paddusw", 0xdd), ("pmaxub", 0xde), ("pandn", 0xdf),
        ("pavgb", 0xe0), ("psraw", 0xe1), ("psrad", 0xe2), ("pavgw", 0xe3),
        ("pmulhuw", 0xe4), ("pmulhw", 0xe5),
        ("psubsb", 0xe8), ("psubsw", 0xe9), ("pminsw", 0xea), ("por", 0xeb),
        ("paddsb", 0xec), ("paddsw", 0xed), ("pmaxsw", 0xee), ("pxor", 0xef),
        ("psllw", 0xf1), ("pslld", 0xf2), ("psllq", 0xf3),
        ("pmuludq", 0xf4), ("pmaddwd", 0xf5), ("psadbw", 0xf6),
        ("psubb", 0xf8), ("psubw", 0xf9), ("psubd", 0xfa), ("psubq", 0xfb),
        ("paddb", 0xfc), ("paddw", 0xfd), ("paddd", 0xfe),
];

/// The `/digit` count-immediate forms of the shift group, shared with SSE2.
#[rustfmt::skip]
pub const SHIFT_IMM: &[(&str, u8, u8)] = &[
    ("psrlw", 0x71, 2), ("psraw", 0x71, 4), ("psllw", 0x71, 6),
    ("psrld", 0x72, 2), ("psrad", 0x72, 4), ("pslld", 0x72, 6),
    ("psrlq", 0x73, 2),                     ("psllq", 0x73, 6),
];

pub fn install(t: &mut Tbl) {
    for &(mnem, op) in PACKED_BINARY {
        add(t, mnem, bin(op));
    }
    add(t, "maskmovq", bin(0xf7));

    for &(mnem, op, ext) in SHIFT_IMM {
        add(t, mnem, vec![shift_imm(op, ext)]);
    }

    add(
        t,
        "pshufw",
        vec![d(
            vec![Op::V(Vk::Mm), Op::Vm(Vk::Mm, 0), Op::Imm(1)],
            &[0x0f, 0x70],
            ModRm::Reg,
            0,
        )],
    );
    add(
        t,
        "pmovmskb",
        vec![
            d(vec![Op::R(4), Op::V(Vk::Mm)], &[0x0f, 0xd7], ModRm::Reg, 0),
            d(vec![Op::R(8), Op::V(Vk::Mm)], &[0x0f, 0xd7], ModRm::Reg, 0),
        ],
    );
    add(
        t,
        "movntq",
        vec![d(
            vec![Op::M(8), Op::V(Vk::Mm)],
            &[0x0f, 0xe7],
            ModRm::Reg,
            0,
        )],
    );
    add(
        t,
        "pinsrw",
        vec![
            d(
                vec![Op::V(Vk::Mm), Op::R(4), Op::Imm(1)],
                &[0x0f, 0xc4],
                ModRm::Reg,
                0,
            ),
            d(
                vec![Op::V(Vk::Mm), Op::R(8), Op::Imm(1)],
                &[0x0f, 0xc4],
                ModRm::Reg,
                0,
            ),
            d(
                vec![Op::V(Vk::Mm), Op::M(2), Op::Imm(1)],
                &[0x0f, 0xc4],
                ModRm::Reg,
                0,
            ),
        ],
    );
    add(
        t,
        "pextrw",
        vec![
            d(
                vec![Op::R(4), Op::V(Vk::Mm), Op::Imm(1)],
                &[0x0f, 0xc5],
                ModRm::Reg,
                0,
            ),
            d(
                vec![Op::R(8), Op::V(Vk::Mm), Op::Imm(1)],
                &[0x0f, 0xc5],
                ModRm::Reg,
                0,
            ),
        ],
    );

    // `movd`/`movq` between an MMX register and a GPR or memory.
    add(
        t,
        "movd",
        vec![
            d(vec![Op::V(Vk::Mm), Op::Rm(4)], &[0x0f, 0x6e], ModRm::Reg, 0),
            d(vec![Op::Rm(4), Op::V(Vk::Mm)], &[0x0f, 0x7e], ModRm::Reg, 0),
            d(vec![Op::V(Vk::Mm), Op::R(8)], &[0x0f, 0x6e], ModRm::Reg, 64).flags(R_IN_RM),
            d(vec![Op::R(8), Op::V(Vk::Mm)], &[0x0f, 0x7e], ModRm::Reg, 64).flags(R_IN_RM),
        ],
    );
    // In AT&T syntax `movq` is first `mov` with a `q` suffix; these rows are
    // only reached when that matched nothing. See `resolve_mnemonic`.
    add(
        t,
        "movq",
        vec![
            d(vec![Op::V(Vk::Mm), Op::R(8)], &[0x0f, 0x6e], ModRm::Reg, 64),
            d(vec![Op::R(8), Op::V(Vk::Mm)], &[0x0f, 0x7e], ModRm::Reg, 64),
            d(
                vec![Op::V(Vk::Mm), Op::Vm(Vk::Mm, 0)],
                &[0x0f, 0x6f],
                ModRm::Reg,
                0,
            ),
            d(
                vec![Op::Vm(Vk::Mm, 0), Op::V(Vk::Mm)],
                &[0x0f, 0x7f],
                ModRm::Reg,
                0,
            ),
        ],
    );

    add(t, "emms", vec![d(vec![], &[0x0f, 0x77], ModRm::None, 0)]);

    // ---- 3DNow! -----------------------------------------------------------
    #[rustfmt::skip]
    let now: &[(&'static str, u8)] = &[
        // 3DNow!
        ("pi2fd", 0x0d), ("pf2id", 0x1d),
        ("pfcmpge", 0x90), ("pfmin", 0x94), ("pfrcp", 0x96), ("pfrsqrt", 0x97),
        ("pfsub", 0x9a), ("pfadd", 0x9e),
        ("pfcmpgt", 0xa0), ("pfmax", 0xa4), ("pfrcpit1", 0xa6), ("pfrsqit1", 0xa7),
        ("pfsubr", 0xaa), ("pfacc", 0xae),
        ("pfcmpeq", 0xb0), ("pfmul", 0xb4), ("pfrcpit2", 0xb6), ("pmulhrw", 0xb7),
        ("pavgusb", 0xbf),
        // 3DNow!+, added with the Athlon.
        ("pi2fw", 0x0c), ("pf2iw", 0x1c),
        ("pfnacc", 0x8a), ("pfpnacc", 0x8e), ("pswapd", 0xbb),
    ];
    for &(mnem, sfx) in now {
        add(
            t,
            mnem,
            vec![
                d(
                    vec![Op::V(Vk::Mm), Op::Vm(Vk::Mm, 0)],
                    &[0x0f, 0x0f],
                    ModRm::Reg,
                    0,
                )
                .suffix(sfx),
            ],
        );
    }
    add(t, "femms", vec![d(vec![], &[0x0f, 0x0e], ModRm::None, 0)]);
    for (mnem, ext) in [("prefetch", 0u8), ("prefetchw", 1)] {
        add(
            t,
            mnem,
            vec![d(vec![Op::M(0)], &[0x0f, 0x0d], ModRm::Ext(ext), 0)],
        );
    }
}
