//! APX: the forms that need the extended EVEX prefix.
//!
//! The registers `r16`-`r31` live in [`crate::arch::x86::reg`] and the REX2
//! prefix in [`crate::arch::x86::encode`]; what is left for a table is the
//! instructions APX moved into an EVEX map, and the three kinds of row below.
//!
//! **Promoted rows** repeat an instruction already in the table under the
//! extended EVEX prefix. They are what `{evex}` asks for, what `{nf}` needs,
//! and the only way to reach an `r16`-`r31` operand of an instruction whose
//! legacy opcode is in the `0F 38` map, since REX2 covers the one-byte and
//! `0F` maps and nothing else. The map-4 opcode is usually the legacy opcode
//! with its escape bytes dropped, but `movbe`, the three bit counts and the
//! immediate `shld`/`shrd` were renumbered, and four instructions changed
//! their mandatory prefix, so each family states its own.
//!
//! **NDD rows** add a destination register, carried in `vvvv` with `EVEX.ND`
//! set, which turns the read-modify-write integer instructions into
//! three-operand ones that leave their sources alone. Each mirrors a legacy
//! row with one operand prepended, since the table is in Intel order.
//!
//! The rest are **new instructions**: the paired and hinted pushes, `jmpabs`,
//! the conditional compares with their default-flags mask, the conditionally
//! faulting moves, and the two forms that zero the upper half of their result.
//!
//! GNU as also promotes `cmp` and `test` by spelling them as `ccmp`/`ctest`
//! with the always-true condition, which is why those two share the rows the
//! conditional compares are built from.

use super::{
    APX_ND, APX_NF, APX_NF_ON, APX_REX2, CONDITIONS, DISTINCT_PAIR, Def, Enc, IMM64, ModRm,
    NO_REX_W, NO_RSP, NO64, ONLY64, Op, PLUSREG, Tbl, WIDTHS, add, d, opsize_bits,
};

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

/// An extended-EVEX row in map 4, where the promoted legacy opcodes and
/// everything APX added live. A promoted VEX row keeps the map it already had
/// and is cloned rather than written out, so it does not come through here.
fn m4(ops: Vec<Op>, opcode: u8, modrm: ModRm, opsize: u8) -> Def {
    d(ops, &[opcode], modrm, opsize).apx(4).flags(ONLY64)
}

/// The eight `add`-style groups, with the base opcode and `/digit` they share
/// with their legacy rows, and whether `{nf}` has a form for them. Carry and
/// borrow cannot be computed without the flags, so `adc` and `sbb` have none.
#[rustfmt::skip]
const ALU: &[(&str, u8, u8, bool)] = &[
    ("add", 0x00, 0, true),
    ("or",  0x08, 1, true),
    ("adc", 0x10, 2, false),
    ("sbb", 0x18, 3, false),
    ("and", 0x20, 4, true),
    ("sub", 0x28, 5, true),
    ("xor", 0x30, 6, true),
];

/// The shifts and rotates, with their `/digit`. The two that rotate through
/// the carry flag read it, so they have no no-flags form.
#[rustfmt::skip]
const SHIFTS: &[(&[&str], u8, bool)] = &[
    (&["rol"], 0, true),
    (&["ror"], 1, true),
    (&["rcl"], 2, false),
    (&["rcr"], 3, false),
    (&["shl", "sal"], 4, true),
    (&["shr"], 5, true),
    (&["sar"], 7, true),
];

/// The one-operand groups, by base opcode and `/digit`, with whether a new
/// destination register exists for the operation and whether `{nf}` does.
/// Only those that write one register take a destination; the multiplications
/// and divisions write `rDX:rAX` and have nowhere to put one.
#[rustfmt::skip]
const UNARY: &[(&str, u8, u8, bool, bool)] = &[
    ("inc",  0xfe, 0, true,  true),
    ("dec",  0xfe, 1, true,  true),
    ("not",  0xf6, 2, true,  false),
    ("neg",  0xf6, 3, true,  true),
    ("mul",  0xf6, 4, false, true),
    ("imul", 0xf6, 5, false, true),
    ("div",  0xf6, 6, false, true),
    ("idiv", 0xf6, 7, false, true),
];

/// A legacy instruction APX promoted into map 4, named by its mnemonic, the
/// first opcode byte of the legacy rows to take, the map-4 opcode that
/// replaces it, the mandatory prefix the promoted form carries, and anything
/// else the promoted row gains. A row whose last opcode byte is `legacy + k`
/// becomes one with opcode `map4 + k`, which is how the two directions of
/// `movbe` and `movrs` and the widths of `crc32` come along.
#[rustfmt::skip]
const PROMOTED: &[(&str, u8, u8, u8, u32)] = &[
    // The `0F 38` map, out of REX2's reach, which is why these needed a
    // promotion at all rather than just a wider prefix.
    ("clr",       0x30, 0x30, 0x00, APX_NF),
    ("movbe",     0xf0, 0x60, 0x00, 0),
    ("movrs",     0x8a, 0x8a, 0x00, 0),
    ("movdiri",   0xf9, 0xf9, 0x00, 0),
    ("movdir64b", 0xf8, 0xf8, 0x66, 0),
    ("enqcmd",    0xf8, 0xf8, 0xf2, 0),
    ("enqcmds",   0xf8, 0xf8, 0xf3, 0),
    ("adcx",      0xf6, 0x66, 0x66, 0),
    ("adox",      0xf6, 0x66, 0xf3, 0),
    ("wrssd",     0xf6, 0x66, 0x00, 0),
    ("wrssq",     0xf6, 0x66, 0x00, 0),
    ("wrussd",    0xf5, 0x65, 0x66, 0),
    ("wrussq",    0xf5, 0x65, 0x66, 0),
    ("aadd",      0xfc, 0xfc, 0x00, 0),
    ("aand",      0xfc, 0xfc, 0x66, 0),
    ("aor",       0xfc, 0xfc, 0xf2, 0),
    ("axor",      0xfc, 0xfc, 0xf3, 0),
    // `invpcid` is the one that swapped prefixes: `66` legacy, `F3` promoted.
    ("invpcid",   0x82, 0xf2, 0xf3, 0),
    // The CRC and the three bit counts drop their `F2`/`F3`, which in map 4
    // would name an operand size the row does not have.
    ("crc32",     0xf0, 0xf0, 0x00, 0),
    ("crc32b",    0xf0, 0xf0, 0x00, 0),
    ("crc32w",    0xf1, 0xf1, 0x00, 0),
    ("crc32l",    0xf1, 0xf1, 0x00, 0),
    ("crc32q",    0xf1, 0xf1, 0x00, 0),
    ("popcnt",    0xb8, 0x88, 0x00, APX_NF),
    ("lzcnt",     0xbd, 0xf5, 0x00, APX_NF),
    ("tzcnt",     0xbc, 0xf4, 0x00, APX_NF),
];

/// The VEX-encoded instructions APX repeated under the extended EVEX prefix,
/// with whether `{nf}` has a form for them. Nothing else about the encoding
/// changes: same map, same opcode, same `pp`, same `W`.
#[rustfmt::skip]
const PROMOTED_VEX: &[(&str, bool)] = &[
    ("andn", true), ("bextr", true), ("blsi", true), ("blsmsk", true),
    ("blsr", true), ("bzhi", true),
    ("mulx", false), ("pdep", false), ("pext", false), ("rorx", false),
    ("sarx", false), ("shlx", false), ("shrx", false),
    ("kmovb", false), ("kmovw", false), ("kmovd", false), ("kmovq", false),
    ("ldtilecfg", false), ("sttilecfg", false),
    ("tileloadd", false), ("tileloaddt1", false), ("tilestored", false),
];

pub fn install(t: &mut Tbl) {
    install_alu(t);
    install_shifts(t);
    install_unary(t);
    install_imul(t);
    install_double_shifts(t);
    install_conditional(t);
    install_stack(t);
    install_promoted(t);
    install_promoted_vex(t);
}

/// The `add` family: every legacy row that map 4 repeats, then the same rows
/// again with a new destination register in front.
fn install_alu(t: &mut Tbl) {
    for &(mnem, base, ext, nf) in ALU {
        let nf = if nf { APX_NF } else { 0 };
        // The group's rows, with or without the leading destination
        // register, in the order the legacy group lists its forms so that the
        // matcher goes on preferring the sign-extended byte immediate.
        let rows = |ndd: bool| {
            let head = |mut ops: Vec<Op>, w: u8| {
                if ndd {
                    ops.insert(0, Op::NdsR(w));
                }
                ops
            };
            let nd = if ndd { APX_ND } else { 0 };
            let mut defs = vec![
                m4(head(vec![Op::Rm(1), Op::R(1)], 1), base, ModRm::Reg, 8),
                m4(head(vec![Op::R(1), Op::Rm(1)], 1), base + 2, ModRm::Reg, 8),
            ];
            for w in WIDTHS {
                let bits = opsize_bits(w);
                defs.push(m4(
                    head(vec![Op::Rm(w), Op::R(w)], w),
                    base + 1,
                    ModRm::Reg,
                    bits,
                ));
                defs.push(m4(
                    head(vec![Op::R(w), Op::Rm(w)], w),
                    base + 3,
                    ModRm::Reg,
                    bits,
                ));
            }
            defs.push(m4(
                head(vec![Op::Rm(1), Op::Imm(1)], 1),
                0x80,
                ModRm::Ext(ext),
                8,
            ));
            for w in WIDTHS {
                let bits = opsize_bits(w);
                defs.push(m4(
                    head(vec![Op::Rm(w), Op::Imm8s], w),
                    0x83,
                    ModRm::Ext(ext),
                    bits,
                ));
            }
            for w in WIDTHS {
                let bits = opsize_bits(w);
                let imm = if w == 2 { 2 } else { 4 };
                defs.push(m4(
                    head(vec![Op::Rm(w), Op::Imm(imm)], w),
                    0x81,
                    ModRm::Ext(ext),
                    bits,
                ));
            }
            defs.into_iter()
                .map(|x| x.flags(nf | nd))
                .collect::<Vec<_>>()
        };
        add(t, mnem, rows(false));
        add(t, mnem, rows(true));
    }
}

fn install_shifts(t: &mut Tbl) {
    for &(mnems, ext, nf) in SHIFTS {
        let nf = if nf { APX_NF } else { 0 };
        // The count the shift is written with, and the opcode it picks. The
        // form with no count at all is not promoted: GNU as refuses
        // `{evex} rol %rax` although `{evex} rol $1, %rax` is the same byte.
        let counts: [(Op, u8); 3] = [(Op::One, 0xd0), (Op::Fixed("cl"), 0xd2), (Op::Imm(1), 0xc0)];
        let rows = |ndd: bool| {
            let nd = if ndd { APX_ND } else { 0 };
            let mut defs = Vec::new();
            for (count, op) in counts {
                let head = |w: u8, byte: bool| {
                    let mut ops = vec![Op::Rm(w), count];
                    if ndd {
                        ops.insert(0, Op::NdsR(w));
                    }
                    (ops, if byte { op } else { op + 1 })
                };
                let (ops, op8) = head(1, true);
                defs.push(m4(ops, op8, ModRm::Ext(ext), 8).flags(nf | nd));
                for w in WIDTHS {
                    let (ops, opw) = head(w, false);
                    defs.push(m4(ops, opw, ModRm::Ext(ext), opsize_bits(w)).flags(nf | nd));
                }
            }
            defs
        };
        for mnem in mnems {
            add(t, mnem, rows(false));
            add(t, mnem, rows(true));
        }
    }
}

fn install_unary(t: &mut Tbl) {
    for &(mnem, op, ext, ndd, nf) in UNARY {
        let nf = if nf { APX_NF } else { 0 };
        let mut defs = vec![m4(vec![Op::Rm(1)], op, ModRm::Ext(ext), 8).flags(nf)];
        for w in WIDTHS {
            defs.push(m4(vec![Op::Rm(w)], op + 1, ModRm::Ext(ext), opsize_bits(w)).flags(nf));
        }
        // The divisions may name the accumulator they divide, as their legacy
        // rows do.
        if op == 0xf6 && ext >= 6 {
            for (w, acc) in [(1u8, "al"), (2, "ax"), (4, "eax"), (8, "rax")] {
                let op = if w == 1 { op } else { op + 1 };
                defs.push(
                    m4(
                        vec![Op::Fixed(acc), Op::Rm(w)],
                        op,
                        ModRm::Ext(ext),
                        opsize_bits(w),
                    )
                    .flags(nf),
                );
            }
        }
        if ndd {
            defs.push(m4(vec![Op::NdsR(1), Op::Rm(1)], op, ModRm::Ext(ext), 8).flags(nf | APX_ND));
            for w in WIDTHS {
                defs.push(
                    m4(
                        vec![Op::NdsR(w), Op::Rm(w)],
                        op + 1,
                        ModRm::Ext(ext),
                        opsize_bits(w),
                    )
                    .flags(nf | APX_ND),
                );
            }
        }
        add(t, mnem, defs);
    }
}

/// `imul`'s two- and three-operand shapes, and `imulzu`, which is the
/// three-operand form restricted to a word and told to zero the upper half of
/// the destination rather than leave it alone.
fn install_imul(t: &mut Tbl) {
    let mut defs = Vec::new();
    for w in WIDTHS {
        let bits = opsize_bits(w);
        let imm = if w == 2 { 2 } else { 4 };
        defs.push(m4(vec![Op::R(w), Op::Rm(w), Op::Imm8s], 0x6b, ModRm::Reg, bits).flags(APX_NF));
        defs.push(
            m4(
                vec![Op::R(w), Op::Rm(w), Op::Imm(imm)],
                0x69,
                ModRm::Reg,
                bits,
            )
            .flags(APX_NF),
        );
        defs.push(m4(vec![Op::R(w), Op::Rm(w)], 0xaf, ModRm::Reg, bits).flags(APX_NF));
        defs.push(
            m4(
                vec![Op::NdsR(w), Op::R(w), Op::Rm(w)],
                0xaf,
                ModRm::Reg,
                bits,
            )
            .flags(APX_NF | APX_ND),
        );
    }
    add(t, "imul", defs);
    add(
        t,
        "imulzu",
        vec![
            m4(vec![Op::R(2), Op::Rm(2), Op::Imm8s], 0x6b, ModRm::Reg, 16).flags(APX_NF | APX_ND),
            m4(vec![Op::R(2), Op::Rm(2), Op::Imm(2)], 0x69, ModRm::Reg, 16).flags(APX_NF | APX_ND),
        ],
    );
}

/// `shld` and `shrd`. Their immediate form was renumbered on the way into
/// map 4; the `cl` form kept its opcode byte.
fn install_double_shifts(t: &mut Tbl) {
    for (mnem, imm_op, cl_op) in [("shld", 0x24u8, 0xa5u8), ("shrd", 0x2c, 0xad)] {
        let mut defs = Vec::new();
        for ndd in [false, true] {
            let nd = if ndd { APX_ND } else { 0 };
            for (count, op) in [(Op::Imm(1), imm_op), (Op::Fixed("cl"), cl_op)] {
                for w in WIDTHS {
                    let mut ops = vec![Op::Rm(w), Op::R(w), count];
                    if ndd {
                        ops.insert(0, Op::NdsR(w));
                    }
                    defs.push(m4(ops, op, ModRm::Reg, opsize_bits(w)).flags(APX_NF | nd));
                }
            }
        }
        add(t, mnem, defs);
    }
}

/// The conditions `ccmp` and `ctest` cannot take. Their condition is tested
/// against the mask `{dfv=...}` supplies, which holds only the overflow,
/// sign, zero and carry flags, so there is nothing for a parity condition to
/// read.
const NO_SCC: &[&str] = &["p", "pe", "np", "po"];

/// `ccmp`'s rows: `cmp`'s own, with the condition in the prefix.
fn ccmp_rows(scc: u8) -> Vec<Def> {
    let mut defs = vec![
        m4(vec![Op::Rm(1), Op::R(1)], 0x38, ModRm::Reg, 8),
        m4(vec![Op::R(1), Op::Rm(1)], 0x3a, ModRm::Reg, 8),
    ];
    for w in WIDTHS {
        let bits = opsize_bits(w);
        defs.push(m4(vec![Op::Rm(w), Op::R(w)], 0x39, ModRm::Reg, bits));
        defs.push(m4(vec![Op::R(w), Op::Rm(w)], 0x3b, ModRm::Reg, bits));
    }
    defs.push(m4(vec![Op::Rm(1), Op::Imm(1)], 0x80, ModRm::Ext(7), 8));
    for w in WIDTHS {
        let bits = opsize_bits(w);
        defs.push(m4(vec![Op::Rm(w), Op::Imm8s], 0x83, ModRm::Ext(7), bits));
    }
    for w in WIDTHS {
        let bits = opsize_bits(w);
        let imm = if w == 2 { 2 } else { 4 };
        defs.push(m4(vec![Op::Rm(w), Op::Imm(imm)], 0x81, ModRm::Ext(7), bits));
    }
    defs.into_iter().map(|x| x.scc(scc)).collect()
}

/// `ctest`'s rows, which follow `test`'s: one opcode for either operand
/// order, and no sign-extended byte immediate.
fn ctest_rows(scc: u8) -> Vec<Def> {
    let mut defs = vec![
        m4(vec![Op::Rm(1), Op::R(1)], 0x84, ModRm::Reg, 8),
        m4(vec![Op::R(1), Op::Rm(1)], 0x84, ModRm::Reg, 8),
    ];
    for w in WIDTHS {
        let bits = opsize_bits(w);
        defs.push(m4(vec![Op::Rm(w), Op::R(w)], 0x85, ModRm::Reg, bits));
        defs.push(m4(vec![Op::R(w), Op::Rm(w)], 0x85, ModRm::Reg, bits));
    }
    defs.push(m4(vec![Op::Rm(1), Op::Imm(1)], 0xf6, ModRm::Ext(0), 8));
    for w in WIDTHS {
        let bits = opsize_bits(w);
        let imm = if w == 2 { 2 } else { 4 };
        defs.push(m4(vec![Op::Rm(w), Op::Imm(imm)], 0xf7, ModRm::Ext(0), bits));
    }
    defs.into_iter().map(|x| x.scc(scc)).collect()
}

/// Everything keyed on a condition code: the conditional compares and their
/// flag mask, the conditionally faulting moves, the three-operand `cmov`, and
/// the two spellings of `setcc` that write more than a byte.
fn install_conditional(t: &mut Tbl) {
    // `ccmp`/`ctest` take two conditions no `Jcc` has: always and never,
    // which GNU as spells `t` and `f`.
    for (suffix, scc) in CONDITIONS
        .iter()
        .copied()
        .chain([("t", 0xa), ("f", 0xb)])
        .filter(|(s, _)| !NO_SCC.contains(s))
    {
        add(t, leak(format!("ccmp{suffix}")), ccmp_rows(scc));
        add(t, leak(format!("ctest{suffix}")), ctest_rows(scc));
    }
    // The always-true condition is how GNU as promotes plain `cmp` and
    // `test`, which have no map-4 opcode of their own.
    add(t, "cmp", ccmp_rows(0xa));
    add(t, "test", ctest_rows(0xa));

    for &(suffix, tttn) in CONDITIONS {
        let op = 0x40 + tttn;
        // `cmov` gained only the three-operand form; its two-operand one
        // stays legacy, as GNU as's refusal of `{evex} cmovcc` says.
        let cmov: Vec<Def> = WIDTHS
            .iter()
            .map(|&w| {
                m4(
                    vec![Op::NdsR(w), Op::R(w), Op::Rm(w)],
                    op,
                    ModRm::Reg,
                    opsize_bits(w),
                )
                .flags(APX_ND)
            })
            .collect();
        add(t, leak(format!("cmov{suffix}")), cmov);

        // `cfcmov` faults on neither side when the condition is false, so it
        // has a store form as well as a load one. The two share an opcode and
        // are told apart by `EVEX.NF`, which is why the store form carries it
        // whether or not it was asked for.
        let mut cfcmov = Vec::new();
        for w in WIDTHS {
            cfcmov.push(m4(
                vec![Op::R(w), Op::Rm(w)],
                op,
                ModRm::Reg,
                opsize_bits(w),
            ));
        }
        for w in WIDTHS {
            cfcmov.push(
                m4(vec![Op::Rm(w), Op::R(w)], op, ModRm::Reg, opsize_bits(w)).flags(APX_NF_ON),
            );
        }
        for w in WIDTHS {
            cfcmov.push(
                m4(
                    vec![Op::NdsR(w), Op::R(w), Op::Rm(w)],
                    op,
                    ModRm::Reg,
                    opsize_bits(w),
                )
                .flags(APX_ND | APX_NF_ON),
            );
        }
        add(t, leak(format!("cfcmov{suffix}")), cfcmov);

        // `setcc` promoted keeps its byte destination, and gains a wider one
        // that zeroes the register above the byte it wrote. GNU as spells that
        // second form both as `setcc` with a wider register and as `setzucc`
        // with a byte one; the encoding is the same either way.
        let wide: Vec<Def> = [4u8, 8]
            .iter()
            .map(|&w| {
                m4(vec![Op::R(w)], op, ModRm::Ext(0), opsize_bits(w))
                    .pfx(0xf2)
                    .flags(APX_ND)
            })
            .collect();
        for s in [suffix.to_string(), format!("{suffix}b")] {
            let mut defs = vec![m4(vec![Op::Rm(1)], op, ModRm::Ext(0), 8).pfx(0xf2)];
            defs.extend(wide.iter().cloned());
            add(t, leak(format!("set{s}")), defs);
        }
        add(
            t,
            leak(format!("setzu{suffix}")),
            vec![
                m4(vec![Op::R(1)], op, ModRm::Ext(0), 8)
                    .pfx(0xf2)
                    .flags(APX_ND),
            ],
        );
    }
}

/// The stack instructions APX added: a hinted push and pop, which are the
/// ordinary ones under REX2 with `W` set, and the paired forms, which push or
/// pop two registers at once. Only the pair refuses `rsp`; the hinted forms
/// take it, as `push` and `pop` do.
fn install_stack(t: &mut Tbl) {
    for (mnem, op) in [("pushp", 0x50u8), ("popp", 0x58)] {
        add(
            t,
            mnem,
            vec![d(vec![Op::R(8)], &[op], ModRm::None, 64).flags(PLUSREG | ONLY64 | APX_REX2)],
        );
    }
    // `push2` writes the pair in `vvvv` and r/m; `W` is the push-pop-acceleration
    // hint rather than an operand size, so the plain forms fix it at zero.
    for (mnem, op, ext, hint) in [
        ("push2", 0xffu8, 6u8, false),
        ("push2p", 0xff, 6, true),
        ("pop2", 0x8f, 0, false),
        ("pop2p", 0x8f, 0, true),
    ] {
        let distinct = if ext == 0 { DISTINCT_PAIR } else { 0 };
        let w = if hint { 0 } else { NO_REX_W };
        add(
            t,
            mnem,
            vec![
                m4(vec![Op::NdsR(8), Op::Rm(8)], op, ModRm::Ext(ext), 64)
                    .flags(APX_ND | NO_RSP | distinct | w),
            ],
        );
    }
    // `jmpabs` reuses the opcode of the 64-bit absolute `mov`, which long
    // mode had already dropped; REX2 is what tells the two apart.
    add(
        t,
        "jmpabs",
        vec![d(vec![Op::Imm(8)], &[0xa1], ModRm::None, 0).flags(ONLY64 | APX_REX2 | IMM64)],
    );
}

/// Repeats the legacy rows [`PROMOTED`] names under the extended EVEX prefix.
fn install_promoted(t: &mut Tbl) {
    for &(mnem, from, to, pfx, extra) in PROMOTED {
        let Some(rows) = t.get(mnem).cloned() else {
            continue;
        };
        let promoted: Vec<Def> = rows
            .iter()
            .filter(|x| x.enc == Enc::Legacy && x.flags & NO64 == 0)
            .filter_map(|x| {
                let last = *x.opcode.last()?;
                let shift = last.checked_sub(from).filter(|&n| n < 2)?;
                let mut y = x.clone();
                y.opcode = vec![to + shift];
                y.pfx = pfx;
                Some(y.apx(4).flags(ONLY64 | extra))
            })
            .collect();
        add(t, mnem, promoted);
    }
}

/// Repeats the VEX rows [`PROMOTED_VEX`] names under the extended EVEX
/// prefix. Nothing but the prefix changes, so the rows are taken as they are;
/// the XOP maps are left out, having no APX form.
fn install_promoted_vex(t: &mut Tbl) {
    // CMPccXADD, one mnemonic per condition, generated as its VEX rows were.
    let conditional: Vec<(&'static str, bool)> = CONDITIONS
        .iter()
        .map(|&(cc, _)| (leak(format!("cmp{cc}xadd")), false))
        .collect();
    for &(mnem, nf) in PROMOTED_VEX.iter().chain(&conditional) {
        let Some(rows) = t.get(mnem).cloned() else {
            continue;
        };
        let nf = if nf { APX_NF } else { 0 };
        let promoted: Vec<Def> = rows
            .iter()
            .filter(|x| x.enc == Enc::Vex && x.vlen == 128 && x.map <= 3)
            .map(|x| {
                let mut y = x.clone();
                // The extended EVEX prefix has no vector length and no
                // compressed displacement to scale.
                y.vlen = 0;
                y.tuple = super::Tuple::None;
                y.apx(x.map).flags(ONLY64 | nf)
            })
            .collect();
        add(t, mnem, promoted);
    }
}
