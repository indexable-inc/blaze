//! The newer system and utility instructions: CET, AMX, Key Locker, TSX,
//! user interrupts, the `0F 01` group's later additions, and the handful of
//! one-off extensions each processor generation brought.
//!
//! Most are legacy-encoded with a mandatory prefix; AMX is VEX. Their shapes
//! are the base set's, and so are the conventions, with a few specific to
//! this group:
//!
//! - an instruction whose register operand is an address (`umonitor`,
//!   `movdir64b`, the implicit `monitor` operands) takes its size from the
//!   address size, so a 32-bit register in 64-bit mode asks for a `67` prefix
//!   rather than being an error;
//! - the shadow-stack and state-save instructions spell their 64-bit forms as
//!   separate mnemonics (`incsspq`, `xsave64`), each with `REX.W`;
//! - `rdpid` and `senduipi` take a 64-bit register without `REX.W`.

use super::avx::split_escape;
use super::{
    ADDR16, ADDR32, CONDITIONS, Def, ModRm, NO_REX2, NO64, NO66, ONLY64, Op, SIBMEM, Tbl, Vk, add,
    d,
};

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

/// A legacy row with a mandatory prefix.
fn pre(ops: Vec<Op>, pfx: u8, opcode: &[u8], modrm: ModRm, opsize: u8) -> Def {
    d(ops, opcode, modrm, opsize).pfx(pfx)
}

/// An AMX row: VEX, 128 bits, `W0`, 64-bit mode only.
fn amx(ops: Vec<Op>, pfx: u8, op: u8, modrm: ModRm) -> Def {
    let (map, op) = split_escape(&[0x0f, 0x38, op]);
    d(ops, &[op], modrm, 0)
        .pfx(pfx)
        .map(map)
        .vex(128)
        .flags(ONLY64)
}

fn install_fixed(t: &mut Tbl) {
    #[rustfmt::skip]
    const NONE: &[(&str, &[u8])] = &[
        ("getsec", &[0x0f, 0x37]),
        ("pconfig", &[0x0f, 0x01, 0xc5]),
        ("wrmsrns", &[0x0f, 0x01, 0xc6]),
        ("rdmsrlist", &[0xf2, 0x0f, 0x01, 0xc6]),
        ("wrmsrlist", &[0xf3, 0x0f, 0x01, 0xc6]),
        ("vmfunc", &[0x0f, 0x01, 0xd4]),
        ("xend", &[0x0f, 0x01, 0xd5]),
        ("xtest", &[0x0f, 0x01, 0xd6]),
        ("serialize", &[0x0f, 0x01, 0xe8]),
        ("xsusldtrk", &[0xf2, 0x0f, 0x01, 0xe8]),
        ("xresldtrk", &[0xf2, 0x0f, 0x01, 0xe9]),
        ("setssbsy", &[0xf3, 0x0f, 0x01, 0xe8]),
        ("saveprevssp", &[0xf3, 0x0f, 0x01, 0xea]),
        ("uiret", &[0xf3, 0x0f, 0x01, 0xec]),
        ("testui", &[0xf3, 0x0f, 0x01, 0xed]),
        ("clui", &[0xf3, 0x0f, 0x01, 0xee]),
        ("stui", &[0xf3, 0x0f, 0x01, 0xef]),
        ("rdpkru", &[0x0f, 0x01, 0xee]),
        ("wrpkru", &[0x0f, 0x01, 0xef]),
        ("rdpru", &[0x0f, 0x01, 0xfd]),
        ("wbnoinvd", &[0xf3, 0x0f, 0x09]),
    ];
    for &(mnem, bytes) in NONE {
        add(t, mnem, vec![d(vec![], bytes, ModRm::None, 0)]);
    }
    // The user interrupt and MSR list instructions exist only in long mode.
    for mnem in ["uiret", "testui", "clui", "stui", "rdmsrlist", "wrmsrlist"] {
        if let Some(defs) = t.get_mut(mnem) {
            defs[0].flags |= ONLY64;
        }
    }

    // `monitor`, `mwait` and their AMD `x` twins name their implicit
    // registers optionally. The address register's size is the address size;
    // the rest are always 32 bits, or 64 where long mode allows it.
    for (mnem, op) in [("monitor", 0xc8u8), ("monitorx", 0xfa)] {
        let bytes = [0x0f, 0x01, op];
        let fx = |names: [&'static str; 3]| names.iter().map(|n| Op::Fixed(n)).collect();
        add(
            t,
            mnem,
            vec![
                d(vec![], &bytes, ModRm::None, 0),
                d(fx(["rax", "ecx", "edx"]), &bytes, ModRm::None, 0).flags(ONLY64),
                d(fx(["rax", "rcx", "rdx"]), &bytes, ModRm::None, 0).flags(ONLY64),
                d(fx(["eax", "ecx", "edx"]), &bytes, ModRm::None, 0).flags(ADDR32),
                d(fx(["ax", "ecx", "edx"]), &bytes, ModRm::None, 0).flags(ADDR16 | NO64),
            ],
        );
    }
    for (mnem, op, third) in [
        ("mwait", 0xc9u8, None),
        ("mwaitx", 0xfb, Some(("ebx", "rbx"))),
    ] {
        let bytes = [0x0f, 0x01, op];
        let mut short = vec![Op::Fixed("eax"), Op::Fixed("ecx")];
        let mut long = vec![Op::Fixed("rax"), Op::Fixed("rcx")];
        if let Some((e, r)) = third {
            short.push(Op::Fixed(e));
            long.push(Op::Fixed(r));
        }
        add(
            t,
            mnem,
            vec![
                d(vec![], &bytes, ModRm::None, 0),
                d(short, &bytes, ModRm::None, 0),
                d(long, &bytes, ModRm::None, 0).flags(ONLY64),
            ],
        );
    }
    let clzero = [0x0f, 0x01, 0xfc];
    add(
        t,
        "clzero",
        vec![
            d(vec![], &clzero, ModRm::None, 0),
            d(vec![Op::Fixed("rax")], &clzero, ModRm::None, 0).flags(ONLY64),
            d(vec![Op::Fixed("eax")], &clzero, ModRm::None, 0).flags(ADDR32),
            d(vec![Op::Fixed("ax")], &clzero, ModRm::None, 0).flags(ADDR16 | NO64),
        ],
    );

    // TSX: an abort code, and a transaction's fallback address.
    add(
        t,
        "xabort",
        vec![d(vec![Op::Imm(1)], &[0xc6, 0xf8], ModRm::None, 0)],
    );
    add(
        t,
        "xbegin",
        vec![d(vec![Op::Rel(4)], &[0xc7, 0xf8], ModRm::None, 0)],
    );
    // HRESET's ModRM is fixed, and the reset request comes in `eax`.
    add(
        t,
        "hreset",
        vec![pre(
            vec![Op::Imm(1)],
            0xf3,
            &[0x0f, 0x3a, 0xf0, 0xc0],
            ModRm::None,
            0,
        )],
    );
}

fn install_registers(t: &mut Tbl) {
    // A register sized by the mode's address size.
    let by_address = |pfx: u8, opcode: &[u8], ext: u8| {
        vec![
            pre(vec![Op::R(8)], pfx, opcode, ModRm::Ext(ext), 0).flags(ONLY64),
            pre(vec![Op::R(4)], pfx, opcode, ModRm::Ext(ext), 0).flags(ADDR32),
            pre(vec![Op::R(2)], pfx, opcode, ModRm::Ext(ext), 0).flags(ADDR16 | NO64),
        ]
    };
    add(t, "umonitor", by_address(0xf3, &[0x0f, 0xae], 6));

    // The FS and GS base loads and stores: 32 or 64 bits, long mode only.
    for (mnem, ext) in [
        ("rdfsbase", 0u8),
        ("rdgsbase", 1),
        ("wrfsbase", 2),
        ("wrgsbase", 3),
    ] {
        add(
            t,
            mnem,
            vec![
                pre(vec![Op::R(4)], 0xf3, &[0x0f, 0xae], ModRm::Ext(ext), 32).flags(ONLY64 | NO66),
                pre(vec![Op::R(8)], 0xf3, &[0x0f, 0xae], ModRm::Ext(ext), 64).flags(ONLY64),
            ],
        );
    }
    add(
        t,
        "rdpid",
        vec![
            pre(vec![Op::R(4)], 0xf3, &[0x0f, 0xc7], ModRm::Ext(7), 0).flags(NO64),
            pre(vec![Op::R(8)], 0xf3, &[0x0f, 0xc7], ModRm::Ext(7), 0).flags(ONLY64),
        ],
    );
    add(
        t,
        "senduipi",
        vec![pre(vec![Op::R(8)], 0xf3, &[0x0f, 0xc7], ModRm::Ext(6), 0).flags(ONLY64)],
    );
    add(
        t,
        "ptwrite",
        vec![
            pre(vec![Op::Rm(4)], 0xf3, &[0x0f, 0xae], ModRm::Ext(4), 32).flags(NO66),
            pre(vec![Op::Rm(8)], 0xf3, &[0x0f, 0xae], ModRm::Ext(4), 64).flags(ONLY64),
        ],
    );
    // WAITPKG: a TSC deadline, with the rest of it in `edx:eax`.
    for (mnem, pfx) in [("tpause", 0x66u8), ("umwait", 0xf2)] {
        add(
            t,
            mnem,
            vec![
                pre(vec![Op::R(4)], pfx, &[0x0f, 0xae], ModRm::Ext(6), 0),
                pre(
                    vec![Op::R(4), Op::Fixed("edx"), Op::Fixed("eax")],
                    pfx,
                    &[0x0f, 0xae],
                    ModRm::Ext(6),
                    0,
                ),
            ],
        );
    }
    // The PCID invalidation, whose descriptor is 128 bits of memory and whose
    // type register is the mode's word.
    add(
        t,
        "invpcid",
        vec![
            pre(
                vec![Op::R(4), Op::M(16)],
                0x66,
                &[0x0f, 0x38, 0x82],
                ModRm::Reg,
                0,
            )
            .flags(NO64),
            pre(
                vec![Op::R(8), Op::M(16)],
                0x66,
                &[0x0f, 0x38, 0x82],
                ModRm::Reg,
                0,
            )
            .flags(ONLY64),
        ],
    );

    // CET shadow stacks.
    for (stem, opcode, ext) in [
        ("incssp", &[0x0fu8, 0xae] as &[u8], 5u8),
        ("rdssp", &[0x0f, 0x1e], 1),
    ] {
        add(
            t,
            leak(format!("{stem}d")),
            vec![pre(vec![Op::R(4)], 0xf3, opcode, ModRm::Ext(ext), 32).flags(NO66)],
        );
        add(
            t,
            leak(format!("{stem}q")),
            vec![pre(vec![Op::R(8)], 0xf3, opcode, ModRm::Ext(ext), 64).flags(ONLY64)],
        );
    }
    add(
        t,
        "rstorssp",
        vec![pre(vec![Op::M(8)], 0xf3, &[0x0f, 0x01], ModRm::Ext(5), 0)],
    );
    add(
        t,
        "clrssbsy",
        vec![pre(vec![Op::M(8)], 0xf3, &[0x0f, 0xae], ModRm::Ext(6), 0)],
    );
    for (stem, pfx, op) in [("wrss", 0x00u8, 0xf6u8), ("wruss", 0x66, 0xf5)] {
        add(
            t,
            leak(format!("{stem}d")),
            vec![
                pre(
                    vec![Op::M(4), Op::R(4)],
                    pfx,
                    &[0x0f, 0x38, op],
                    ModRm::Reg,
                    32,
                )
                .flags(NO66),
            ],
        );
        add(
            t,
            leak(format!("{stem}q")),
            vec![
                pre(
                    vec![Op::M(8), Op::R(8)],
                    pfx,
                    &[0x0f, 0x38, op],
                    ModRm::Reg,
                    64,
                )
                .flags(ONLY64),
            ],
        );
    }

    // Direct stores, and the work queue submissions whose register is the
    // destination address: sized by the address size like `umonitor`'s.
    add(
        t,
        "movdiri",
        vec![
            pre(
                vec![Op::M(4), Op::R(4)],
                0,
                &[0x0f, 0x38, 0xf9],
                ModRm::Reg,
                32,
            )
            .flags(NO66),
            pre(
                vec![Op::M(8), Op::R(8)],
                0,
                &[0x0f, 0x38, 0xf9],
                ModRm::Reg,
                64,
            )
            .flags(ONLY64),
        ],
    );
    for (mnem, pfx) in [("movdir64b", 0x66u8), ("enqcmd", 0xf2), ("enqcmds", 0xf3)] {
        let esc = [0x0f, 0x38, 0xf8];
        add(
            t,
            mnem,
            vec![
                pre(vec![Op::R(8), Op::M(0)], pfx, &esc, ModRm::Reg, 0).flags(ONLY64),
                pre(vec![Op::R(4), Op::M(0)], pfx, &esc, ModRm::Reg, 0).flags(ADDR32),
                pre(vec![Op::R(2), Op::M(0)], pfx, &esc, ModRm::Reg, 0).flags(ADDR16 | NO64),
            ],
        );
    }

    // RAO-INT: atomic updates of a memory operand.
    for (mnem, pfx) in [
        ("aadd", 0x00u8),
        ("aand", 0x66),
        ("aor", 0xf2),
        ("axor", 0xf3),
    ] {
        let esc = [0x0f, 0x38, 0xfc];
        add(
            t,
            mnem,
            vec![
                pre(vec![Op::M(4), Op::R(4)], pfx, &esc, ModRm::Reg, 32).flags(NO66),
                pre(vec![Op::M(8), Op::R(8)], pfx, &esc, ModRm::Reg, 64).flags(ONLY64),
            ],
        );
    }

    // CMPccXADD, one VEX opcode per condition, long mode only.
    for &(cc, n) in CONDITIONS {
        let mnem = leak(format!("cmp{cc}xadd"));
        for w in [4u8, 8] {
            add(
                t,
                mnem,
                vec![
                    d(
                        vec![Op::M(w), Op::R(w), Op::NdsR(w)],
                        &[0xe0 + n],
                        ModRm::Reg,
                        w * 8,
                    )
                    .pfx(0x66)
                    .map(2)
                    .vex(128)
                    .flags(ONLY64),
                ],
            );
        }
    }
}

fn install_memory(t: &mut Tbl) {
    // Cache and prefetch hints on memory of any size.
    for (mnem, pfx, opcode, ext) in [
        ("clflushopt", 0x66u8, &[0x0fu8, 0xae] as &[u8], 7u8),
        ("clwb", 0x66, &[0x0f, 0xae], 6),
        ("cldemote", 0x00, &[0x0f, 0x1c], 0),
        ("prefetchwt1", 0x00, &[0x0f, 0x0d], 2),
        ("prefetchit0", 0x00, &[0x0f, 0x18], 7),
        ("prefetchit1", 0x00, &[0x0f, 0x18], 6),
        ("prefetchrst2", 0x00, &[0x0f, 0x18], 4),
    ] {
        add(
            t,
            mnem,
            vec![pre(vec![Op::M(0)], pfx, opcode, ModRm::Ext(ext), 0)],
        );
    }
    // MOVRS: a load that reads its memory at most once, at any width.
    add(
        t,
        "movrs",
        vec![
            d(vec![Op::R(1), Op::M(1)], &[0x0f, 0x38, 0x8a], ModRm::Reg, 8),
            d(
                vec![Op::R(2), Op::M(2)],
                &[0x0f, 0x38, 0x8b],
                ModRm::Reg,
                16,
            ),
            d(
                vec![Op::R(4), Op::M(4)],
                &[0x0f, 0x38, 0x8b],
                ModRm::Reg,
                32,
            ),
            d(
                vec![Op::R(8), Op::M(8)],
                &[0x0f, 0x38, 0x8b],
                ModRm::Reg,
                64,
            ),
        ],
    );
    // The 64-bit state saves under their own names. The extended ones are out
    // of APX's reach, as their unsuffixed spellings are.
    for (mnem, opcode, ext, egpr) in [
        ("fxsave64", &[0x0fu8, 0xae] as &[u8], 0u8, true),
        ("fxrstor64", &[0x0f, 0xae], 1, true),
        ("xsave64", &[0x0f, 0xae], 4, false),
        ("xrstor64", &[0x0f, 0xae], 5, false),
        ("xsaveopt64", &[0x0f, 0xae], 6, false),
        ("xrstors64", &[0x0f, 0xc7], 3, false),
        ("xsavec64", &[0x0f, 0xc7], 4, false),
        ("xsaves64", &[0x0f, 0xc7], 5, false),
    ] {
        let extra = if egpr { 0 } else { NO_REX2 };
        add(
            t,
            mnem,
            vec![d(vec![Op::M(0)], opcode, ModRm::Ext(ext), 64).flags(ONLY64 | extra)],
        );
    }

    // Key Locker.
    let x = Vk::Xmm;
    add(
        t,
        "loadiwkey",
        vec![pre(
            vec![Op::V(x), Op::V(x)],
            0xf3,
            &[0x0f, 0x38, 0xdc],
            ModRm::Reg,
            0,
        )],
    );
    // The key handle's 32-bit registers take a `66` in 16-bit mode from GNU
    // as, which sizes them like any other 32-bit operand; llvm-mc leaves it
    // out. rsasm follows GNU as.
    for (mnem, op) in [("encodekey128", 0xfau8), ("encodekey256", 0xfb)] {
        add(
            t,
            mnem,
            vec![pre(
                vec![Op::R(4), Op::R(4)],
                0xf3,
                &[0x0f, 0x38, op],
                ModRm::Reg,
                32,
            )],
        );
    }
    for (mnem, op) in [
        ("aesenc128kl", 0xdcu8),
        ("aesdec128kl", 0xdd),
        ("aesenc256kl", 0xde),
        ("aesdec256kl", 0xdf),
    ] {
        add(
            t,
            mnem,
            vec![pre(
                vec![Op::V(x), Op::M(0)],
                0xf3,
                &[0x0f, 0x38, op],
                ModRm::Reg,
                0,
            )],
        );
    }
    for (mnem, ext) in [
        ("aesencwide128kl", 0u8),
        ("aesdecwide128kl", 1),
        ("aesencwide256kl", 2),
        ("aesdecwide256kl", 3),
    ] {
        add(
            t,
            mnem,
            vec![pre(
                vec![Op::M(0)],
                0xf3,
                &[0x0f, 0x38, 0xd8],
                ModRm::Ext(ext),
                0,
            )],
        );
    }
}

fn install_amx(t: &mut Tbl) {
    let tmm = Vk::Tmm;
    for (mnem, pfx) in [("ldtilecfg", 0x00u8), ("sttilecfg", 0x66)] {
        add(t, mnem, vec![amx(vec![Op::M(0)], pfx, 0x49, ModRm::Ext(0))]);
    }
    // `tilerelease` has a fixed ModRM byte, and `tilezero` a register in
    // ModRM.reg with nothing in r/m; the suffix byte and a register-only
    // ModRM say both.
    add(
        t,
        "tilerelease",
        vec![amx(vec![], 0x00, 0x49, ModRm::None).suffix(0xc0)],
    );
    add(
        t,
        "tilezero",
        vec![amx(vec![Op::V(tmm)], 0xf2, 0x49, ModRm::Reg)],
    );
    for (mnem, pfx) in [("tileloadd", 0xf2u8), ("tileloaddt1", 0x66)] {
        add(
            t,
            mnem,
            vec![amx(vec![Op::V(tmm), Op::M(0)], pfx, 0x4b, ModRm::Reg).flags(SIBMEM)],
        );
    }
    add(
        t,
        "tilestored",
        vec![amx(vec![Op::M(0), Op::V(tmm)], 0xf3, 0x4b, ModRm::Reg).flags(SIBMEM)],
    );
    // Tile arithmetic: `op dst, src1, src2`, with `src2` in `vvvv`.
    for (mnem, pfx, op) in [
        ("tdpbssd", 0xf2u8, 0x5eu8),
        ("tdpbsud", 0xf3, 0x5e),
        ("tdpbusd", 0x66, 0x5e),
        ("tdpbuud", 0x00, 0x5e),
        ("tdpbf16ps", 0xf3, 0x5c),
        ("tdpfp16ps", 0xf2, 0x5c),
        ("tcmmimfp16ps", 0x66, 0x6c),
        ("tcmmrlfp16ps", 0x00, 0x6c),
    ] {
        add(
            t,
            mnem,
            vec![amx(
                vec![Op::V(tmm), Op::V(tmm), Op::Nds(tmm)],
                pfx,
                op,
                ModRm::Reg,
            )],
        );
    }
}

pub fn install(t: &mut Tbl) {
    install_fixed(t);
    install_registers(t);
    install_memory(t);
    install_amx(t);
}
