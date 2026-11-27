//! One statement of an `asm!` template to its instruction word: the mnemonic's rows of
//! [`ROWS`], each one form (an operand shape) with its encoder in enc.rs; the first row whose
//! shape the statement's operands have gives the word.
//!
//! Operands are read as LLVM's AArch64 assembler reads them (AArch64AsmParser.cpp): names and
//! mnemonics in any case; `x0`..`x30`, `w0`..`w30`, `xzr`/`wzr`, `sp`/`wsp`, `lr`, `fp`;
//! `b`/`h`/`s`/`d`/`q` `0`..`31`; `v<n>.<arrangement>`; an immediate with or without `#`, in
//! decimal or `0x` hex, with a sign; and a name (a system register, a condition, a barrier
//! option). A statement whose mnemonic has no row, or whose operands no row of it takes, is
//! refused naming the mnemonic or the statement; a value a row's shape takes but its field
//! cannot hold (an immediate past its range, a read-only register written) names the operand.
//!
//! The set is what the dependencies of tempo, reth and blaze hold in `asm!` on arm64 (the
//! survey in pass/asm.rs's lane report): barriers and hints, system registers (the DIT and
//! timer reads), `pacib`, `tst`/`csel`/`eor`/`cmp` (cmov's), scalar `fmadd`/`frintn`/`fsqrt`/
//! `fcvt`, CRC32, and NEON `cmeq`/`and`/`shrn` (constant_time_eq's).

use crate::enc::{self, Cond, Field, Fp1, Ftype, Pstate, Reg, Size, SysReg, ZR};

/// The most operands one statement has here (`fmadd` and post-index `ldp`/`stp` have four).
const ARGS_MOST: usize = 5;

/// One operand's shape and value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Arg {
    /// A general register; 31 is `xzr`/`wzr`, or `sp`/`wsp` when `sp`.
    Gpr { reg: Reg, wide: bool, sp: bool },
    /// A memory operand `[Xn|SP]` / `[Xn|SP, #off]` (`pre = false`) or `[Xn|SP, #off]!` (`pre = true`).
    Addr { base: Reg, off: i128, pre: bool },
    /// A scalar SIMD&FP register.
    Fp { reg: Reg, width: FpWidth },
    /// A vector register with its arrangement.
    Vec { reg: Reg, arr: Arr },
    Imm(i128),
    /// A `sym` operand placeholder (`$sym<index>` fed by `pass/asm.rs`).
    Sym(u32),
    /// A word (a system register, a condition, a barrier option): its text is the operand's.
    Name,
    /// Anything else (a shifted register, a bare `v0`): no row takes it.
    Other,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FpWidth {
    B,
    H,
    S,
    D,
    Q,
}

impl FpWidth {
    const fn ftype(self) -> Option<Ftype> {
        match self {
            FpWidth::H => Some(Ftype::H),
            FpWidth::S => Some(Ftype::S),
            FpWidth::D => Some(Ftype::D),
            FpWidth::B | FpWidth::Q => None,
        }
    }
}

/// A vector arrangement: `q` the 128-bit form, `size` log2 of the element's bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Arr {
    q: bool,
    size: u32,
}

/// A statement's operands, each with its text.
struct Ops<'t> {
    mnemonic: &'t [u8],
    args: [Arg; ARGS_MOST],
    texts: [&'t [u8]; ARGS_MOST],
    n: usize,
}

impl<'t> Ops<'t> {
    fn parse(mnemonic: &'t [u8], rest: &'t [u8]) -> Result<Ops<'t>, (&'static str, &'t [u8])> {
        let mut ops = Ops { mnemonic, args: [Arg::Other; ARGS_MOST], texts: [&[]; ARGS_MOST], n: 0 };
        if rest.is_empty() {
            return Ok(ops);
        }
        let mut in_address = false;
        for piece in rest.split(|&b| match b {
            b'[' => {
                in_address = true;
                false
            }
            b']' => {
                in_address = false;
                false
            }
            b',' => !in_address,
            _ => false,
        }) {
            if ops.n == ARGS_MOST {
                return Err(("a statement of more than five operands", rest));
            }
            let text = super::trim(piece);
            ops.args[ops.n] = arg(text);
            ops.texts[ops.n] = text;
            ops.n += 1;
        }
        Ok(ops)
    }

    fn args(&self) -> &[Arg] {
        &self.args[..self.n]
    }

    fn text(&self, at: usize) -> &'t [u8] {
        self.texts[at]
    }
}

/// What a row makes of a statement's operands.
enum Fit {
    Word(u32),
    Branch { word: u32, field: Field, target: usize },
    /// A branch (`b`) or call (`bl`) targeting `sym` operand `sym`.
    SymCall { sym: u32, link: bool },
    /// Not this row's shape.
    No,
    /// This row's shape, but operand `at` cannot be encoded, for `why`.
    Bad { why: &'static str, at: usize },
}

/// A form: the mnemonic and its encoder.
struct Row {
    mnemonic: &'static str,
    encode: fn(&Ops<'_>) -> Fit,
}

/// Every form the assembler has, by mnemonic.
const ROWS: &[Row] = &[
    Row { mnemonic: "b", encode: branch },                 // 000101 imm26 or sym jump
    Row { mnemonic: "bl", encode: bl_call },               // 100101 imm26 or sym call
    Row { mnemonic: "b.cond", encode: branch_cond },       // 01010100 imm19 0 cond
    Row { mnemonic: "br", encode: br_reg },                // 1101011 0000 11111 000000 Rn 00000
    Row { mnemonic: "blr", encode: blr_reg },              // 1101011 0001 11111 000000 Rn 00000
    Row { mnemonic: "ret", encode: ret_op },               // 1101011 0010 11111 000000 Rn 00000
    Row { mnemonic: "cbz", encode: |o| compare_branch(o, false) },  // sf 011010 0 imm19 Rt
    Row { mnemonic: "cbnz", encode: |o| compare_branch(o, true) },  // sf 011010 1 imm19 Rt
    Row { mnemonic: "tbz", encode: |o| test_branch(o, false) },     // b5 011011 0 b40 imm14 Rt
    Row { mnemonic: "tbnz", encode: |o| test_branch(o, true) },     // b5 011011 1 b40 imm14 Rt
    Row { mnemonic: "mov", encode: mov_reg },              // orr (non-sp) or add #0 (with sp)
    Row { mnemonic: "mov", encode: mov_imm },              // movz/movn #imm16
    Row { mnemonic: "sub", encode: |o| arithmetic_imm(o, enc::sub_imm) }, // sf 1 0 100010 0 imm12 Rn Rd
    Row { mnemonic: "sub", encode: |o| arithmetic_reg(o, enc::sub) },     // sf 1 0 01011 00 0 Rm 000000 Rn Rd
    Row { mnemonic: "add", encode: |o| arithmetic_imm(o, enc::add_imm) }, // sf 0 0 100010 0 imm12 Rn Rd
    Row { mnemonic: "add", encode: |o| arithmetic_reg(o, enc::add) },     // sf 0 0 01011 00 0 Rm 000000 Rn Rd
    Row { mnemonic: "stp", encode: |o| pair_mem(o, false) },
    Row { mnemonic: "ldp", encode: |o| pair_mem(o, true) },
    Row { mnemonic: "str", encode: |o| single_mem(o, false) },
    Row { mnemonic: "ldr", encode: |o| single_mem(o, true) },
    // Exclusive accesses: size 001000 0 L pair Rs ordered Rt2 Rn Rt.
    Row { mnemonic: "ldxr", encode: |o| load_exclusive(o, false) },  // L=1 pair=0 Rs=31 ordered=0 Rt2=31
    Row { mnemonic: "ldaxr", encode: |o| load_exclusive(o, true) },  // L=1 pair=0 Rs=31 ordered=1 Rt2=31
    Row { mnemonic: "stxr", encode: |o| store_exclusive(o, false) }, // L=0 pair=0 ordered=0 Rt2=31
    Row { mnemonic: "stlxr", encode: |o| store_exclusive(o, true) }, // L=0 pair=0 ordered=1 Rt2=31
    Row { mnemonic: "ldxp", encode: load_pair },                    // L=1 pair=1 Rs=31 ordered=0
    Row { mnemonic: "stxp", encode: store_pair },                   // L=0 pair=1 ordered=0
    Row { mnemonic: "nop", encode: nop },
    Row { mnemonic: "xpaclri", encode: xpaclri },          // HINT #7 (0xd503_20ff)
    Row { mnemonic: "isb", encode: isb },
    Row { mnemonic: "dsb", encode: dsb },
    Row { mnemonic: "dmb", encode: dmb },
    Row { mnemonic: "sb", encode: sb },
    Row { mnemonic: "mrs", encode: mrs },
    Row { mnemonic: "msr", encode: msr_reg },
    Row { mnemonic: "msr", encode: msr_imm },
    Row { mnemonic: "dc", encode: dc },
    Row { mnemonic: "ic", encode: ic },
    Row { mnemonic: "pacib", encode: pacib },
    Row { mnemonic: "tst", encode: tst_reg },
    Row { mnemonic: "tst", encode: tst_imm },
    Row { mnemonic: "csel", encode: csel },
    Row { mnemonic: "eor", encode: eor },
    Row { mnemonic: "cmp", encode: cmp_reg },
    Row { mnemonic: "cmp", encode: cmp_imm },
    Row { mnemonic: "fmadd", encode: fmadd },
    Row { mnemonic: "frintn", encode: frintn },
    Row { mnemonic: "fsqrt", encode: fsqrt },
    Row { mnemonic: "fcvt", encode: fcvt },
    Row { mnemonic: "crc32b", encode: crc32b },
    Row { mnemonic: "crc32h", encode: crc32h },
    Row { mnemonic: "crc32w", encode: crc32w },
    Row { mnemonic: "crc32x", encode: crc32x },
    Row { mnemonic: "crc32cb", encode: crc32cb },
    Row { mnemonic: "crc32ch", encode: crc32ch },
    Row { mnemonic: "crc32cw", encode: crc32cw },
    Row { mnemonic: "crc32cx", encode: crc32cx },
    Row { mnemonic: "cmeq", encode: cmeq },
    Row { mnemonic: "and", encode: and_v },
    Row { mnemonic: "shrn", encode: shrn },
];

pub(super) enum Instruction<'t> {
    Word(u32),
    Branch { word: u32, field: Field, target: &'t [u8] },
    SymCall { sym: u32, link: bool },
}

/// The instruction of a statement, retaining the target token until its offset is known.
pub(super) fn assemble(text: &[u8]) -> Result<Instruction<'_>, (&'static str, &[u8])> {
    let end = text.iter().position(|it| it.is_ascii_whitespace()).unwrap_or(text.len());
    let mnemonic = &text[..end];
    if mnemonic.first() == Some(&b'.') {
        return Err(("an assembler directive (the baseline tier's assembler takes instructions only)", mnemonic));
    }
    if let Some(colon) = mnemonic.iter().position(|&it| it == b':') {
        return Err(("a label other than a decimal local label", &mnemonic[..=colon]));
    }
    let ops = Ops::parse(mnemonic, super::trim(&text[end..]))?;
    let row_name = if mnemonic.get(..2).is_some_and(|s| s.eq_ignore_ascii_case(b"b.")) { b"b.cond" } else { mnemonic };
    let (mut known, mut bad) = (false, None);
    for row in ROWS {
        if !row.mnemonic.as_bytes().eq_ignore_ascii_case(row_name) {
            continue;
        }
        known = true;
        match (row.encode)(&ops) {
            Fit::Word(word) => return Ok(Instruction::Word(word)),
            Fit::SymCall { sym, link } => return Ok(Instruction::SymCall { sym, link }),
            Fit::Branch { word, field, target } => {
                let token = ops.text(target);
                match token.split_last() {
                    Some((b'b' | b'f', digits)) if !digits.is_empty() && digits.iter().all(u8::is_ascii_digit) => {
                        return Ok(Instruction::Branch { word, field, target: token });
                    }
                    _ => return Err(("expected a decimal local label followed by `b` or `f`", token)),
                }
            }
            Fit::No => {}
            Fit::Bad { why, at } => {
                bad = bad.or(Some((why, ops.text(at))));
            }
        }
    }
    match (known, bad) {
        (false, _) => Err(("a mnemonic the baseline tier's assembler has no row for", mnemonic)),
        (true, Some(bad)) => Err(bad),
        (true, None) => Err(("operands no form of the mnemonic in the assembler's table takes", text)),
    }
}

// Operands.

/// The shape of operand text `t` (trimmed).
fn arg(t: &[u8]) -> Arg {
    if let Some(rest) = t.strip_prefix(b"$sym") {
        if !rest.is_empty() && rest.iter().all(u8::is_ascii_digit) {
            if let Ok(s) = core::str::from_utf8(rest)
                && let Ok(n) = s.parse::<u32>()
            {
                return Arg::Sym(n);
            }
        }
        return Arg::Other;
    }
    let (addr_tok, pre) = match t.strip_suffix(b"!") {
        Some(inner) => (super::trim(inner), true),
        None => (t, false),
    };
    if let Some(address) = addr_tok.strip_prefix(b"[").and_then(|s| s.strip_suffix(b"]")) {
        let mut parts = address.split(|&b| b == b',');
        let base = parts.next().and_then(|s| gpr(super::trim(s)));
        let off = match parts.next() {
            None => 0,
            Some(offset) => {
                if parts.next().is_some() {
                    return Arg::Other;
                }
                let offset = super::trim(offset);
                let Some(v) = number(super::trim(offset.strip_prefix(b"#").unwrap_or(offset))) else {
                    return Arg::Other;
                };
                v
            }
        };
        return match base {
            Some(Arg::Gpr { reg, wide: true, sp }) if reg != ZR || sp => Arg::Addr { base: reg, off, pre },
            _ => Arg::Other,
        };
    }
    if let Some(rest) = t.strip_prefix(b"#") {
        return number(super::trim(rest)).map_or(Arg::Other, Arg::Imm);
    }
    if let Some(v) = number(t) {
        return Arg::Imm(v);
    }
    if let Some(gpr) = gpr(t) {
        return gpr;
    }
    if let Some(dot) = t.iter().position(|&b| b == b'.') {
        return match (reg_number(&t[..dot]), arrangement(&t[dot + 1..])) {
            (Some((b'v', reg)), Some(arr)) => Arg::Vec { reg, arr },
            _ => Arg::Other,
        };
    }
    if let Some((letter, reg)) = reg_number(t) {
        let width = match letter {
            b'b' => FpWidth::B,
            b'h' => FpWidth::H,
            b's' => FpWidth::S,
            b'd' => FpWidth::D,
            b'q' => FpWidth::Q,
            _ => return Arg::Other,
        };
        return Arg::Fp { reg, width };
    }
    match !t.is_empty() && t.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'_') {
        true => Arg::Name,
        false => Arg::Other,
    }
}

/// An integer in decimal or `0x` hex, with an optional `-`, of magnitude at most `u64::MAX`.
fn number(t: &[u8]) -> Option<i128> {
    let (negative, t) = match t.strip_prefix(b"-") {
        Some(rest) => (true, rest),
        None => (false, t),
    };
    let (radix, digits) = match t {
        [b'0', b'x' | b'X', rest @ ..] => (16, rest),
        _ => (10, t),
    };
    if digits.is_empty() {
        return None;
    }
    let mut value: i128 = 0;
    for &d in digits {
        value = value * i128::from(radix) + i128::from(char::from(d).to_digit(radix)?);
        if value > i128::from(u64::MAX) {
            return None;
        }
    }
    Some(if negative { -value } else { value })
}

/// A letter and a register number 0..=31 (`x7`, `V16`), the letter lowercased.
fn reg_number(t: &[u8]) -> Option<(u8, u32)> {
    let (&letter, digits) = t.split_first()?;
    if !letter.is_ascii_alphabetic() {
        return None;
    }
    let n = match digits {
        [d] if d.is_ascii_digit() => u32::from(d - b'0'),
        [a, b] if a.is_ascii_digit() && b.is_ascii_digit() && *a != b'0' => u32::from(a - b'0') * 10 + u32::from(b - b'0'),
        _ => return None,
    };
    (n <= 31).then_some((letter.to_ascii_lowercase(), n))
}

fn gpr(t: &[u8]) -> Option<Arg> {
    let named = [
        ("xzr", Arg::Gpr { reg: ZR, wide: true, sp: false }),
        ("wzr", Arg::Gpr { reg: ZR, wide: false, sp: false }),
        ("sp", Arg::Gpr { reg: 31, wide: true, sp: true }),
        ("wsp", Arg::Gpr { reg: 31, wide: false, sp: true }),
        ("lr", Arg::Gpr { reg: 30, wide: true, sp: false }),
        ("fp", Arg::Gpr { reg: 29, wide: true, sp: false }),
    ];
    if let Some(&(_, it)) = named.iter().find(|(name, _)| name.as_bytes().eq_ignore_ascii_case(t)) {
        return Some(it);
    }
    match reg_number(t)? {
        (b'x', reg @ 0..=30) => Some(Arg::Gpr { reg, wide: true, sp: false }),
        (b'w', reg @ 0..=30) => Some(Arg::Gpr { reg, wide: false, sp: false }),
        _ => None,
    }
}

fn arrangement(t: &[u8]) -> Option<Arr> {
    let table = [("8b", false, 0), ("16b", true, 0), ("4h", false, 1), ("8h", true, 1), ("2s", false, 2), ("4s", true, 2), ("1d", false, 3), ("2d", true, 3)];
    table.iter().find(|(name, ..)| name.as_bytes().eq_ignore_ascii_case(t)).map(|&(_, q, size)| Arr { q, size })
}

fn named<T: Copy>(table: &[(&str, T)], t: &[u8]) -> Option<T> {
    table.iter().find(|(name, _)| name.as_bytes().eq_ignore_ascii_case(t)).map(|&(_, it)| it)
}

/// The condition codes (C1.2.4); `al` and `nv` are not taken.
const CONDS: &[(&str, Cond)] = &[
    ("eq", Cond::Eq),
    ("ne", Cond::Ne),
    ("cs", Cond::Hs),
    ("hs", Cond::Hs),
    ("cc", Cond::Lo),
    ("lo", Cond::Lo),
    ("mi", Cond::Mi),
    ("pl", Cond::Pl),
    ("vs", Cond::Vs),
    ("vc", Cond::Vc),
    ("hi", Cond::Hi),
    ("ls", Cond::Ls),
    ("ge", Cond::Ge),
    ("lt", Cond::Lt),
    ("gt", Cond::Gt),
    ("le", Cond::Le),
];

/// The `dmb`/`dsb` options and their CRm (C6.2.81).
const BARRIERS: &[(&str, u32)] = &[
    ("sy", 0b1111),
    ("st", 0b1110),
    ("ld", 0b1101),
    ("ish", 0b1011),
    ("ishst", 0b1010),
    ("ishld", 0b1001),
    ("nsh", 0b0111),
    ("nshst", 0b0110),
    ("nshld", 0b0101),
    ("osh", 0b0011),
    ("oshst", 0b0010),
    ("oshld", 0b0001),
];

const fn sys(op0: u32, op1: u32, crn: u32, crm: u32, op2: u32) -> SysReg {
    SysReg { op0, op1, crn, crm, op2 }
}

/// The system registers by name (Arm ARM D23, each `op0 op1 CRn CRm op2`) and whether `msr`
/// writes them: what user code reads (the timers, the thread pointers, the ID registers the
/// kernels expose to EL0) and the PSTATE registers it writes.
const SYSREGS: &[(&str, (SysReg, bool))] = &[
    ("nzcv", (sys(3, 3, 4, 2, 0), true)),
    ("fpcr", (sys(3, 3, 4, 4, 0), true)),
    ("fpsr", (sys(3, 3, 4, 4, 1), true)),
    ("dit", (sys(3, 3, 4, 2, 5), true)),
    ("ssbs", (sys(3, 3, 4, 2, 6), true)),
    ("tco", (sys(3, 3, 4, 2, 7), true)),
    ("tpidr_el0", (sys(3, 3, 13, 0, 2), true)),
    ("tpidrro_el0", (SysReg::TPIDRRO_EL0, true)),
    ("cntfrq_el0", (sys(3, 3, 14, 0, 0), true)),
    ("cntpct_el0", (sys(3, 3, 14, 0, 1), false)),
    ("cntvct_el0", (sys(3, 3, 14, 0, 2), false)),
    ("cntpctss_el0", (sys(3, 3, 14, 0, 5), false)),
    ("cntvctss_el0", (sys(3, 3, 14, 0, 6), false)),
    ("ctr_el0", (sys(3, 3, 0, 0, 1), false)),
    ("dczid_el0", (sys(3, 3, 0, 0, 7), false)),
    ("rndr", (sys(3, 3, 2, 4, 0), false)),
    ("rndrrs", (sys(3, 3, 2, 4, 1), false)),
    ("midr_el1", (sys(3, 0, 0, 0, 0), false)),
    ("mpidr_el1", (sys(3, 0, 0, 0, 5), false)),
    ("revidr_el1", (sys(3, 0, 0, 0, 6), false)),
    ("id_aa64pfr0_el1", (sys(3, 0, 0, 4, 0), false)),
    ("id_aa64pfr1_el1", (sys(3, 0, 0, 4, 1), false)),
    ("id_aa64zfr0_el1", (sys(3, 0, 0, 4, 4), false)),
    ("id_aa64smfr0_el1", (sys(3, 0, 0, 4, 5), false)),
    ("id_aa64dfr0_el1", (sys(3, 0, 0, 5, 0), false)),
    ("id_aa64isar0_el1", (sys(3, 0, 0, 6, 0), false)),
    ("id_aa64isar1_el1", (sys(3, 0, 0, 6, 1), false)),
    ("id_aa64isar2_el1", (sys(3, 0, 0, 6, 2), false)),
    ("id_aa64mmfr0_el1", (sys(3, 0, 0, 7, 0), false)),
    ("id_aa64mmfr1_el1", (sys(3, 0, 0, 7, 1), false)),
    ("id_aa64mmfr2_el1", (sys(3, 0, 0, 7, 2), false)),
];

/// The PSTATE fields `msr #imm` writes from EL0 (C5.2), each taking 0 or 1.
const PSTATES: &[(&str, Pstate)] =
    &[("dit", Pstate { op1: 3, op2: 2 }), ("ssbs", Pstate { op1: 3, op2: 1 }), ("tco", Pstate { op1: 3, op2: 4 })];

/// A system register by name, or by the generic `s<op0>_<op1>_c<n>_c<m>_<op2>` (both readable
/// and writable, as LLVM takes it), and whether `msr` writes it.
fn sysreg(t: &[u8]) -> Result<(SysReg, bool), &'static str> {
    if let Some(it) = named(SYSREGS, t) {
        return Ok(it);
    }
    generic_sysreg(t).map(|it| (it, true)).ok_or("a system register outside the assembler's table")
}

fn generic_sysreg(t: &[u8]) -> Option<SysReg> {
    fn after(t: &[u8], letter: u8) -> Option<&[u8]> {
        match t.split_first() {
            Some((&c, rest)) if c.to_ascii_lowercase() == letter => Some(rest),
            _ => None,
        }
    }
    fn small(t: &[u8], most: u32) -> Option<u32> {
        let v = match t {
            [d] if d.is_ascii_digit() => u32::from(d - b'0'),
            [a, b] if a.is_ascii_digit() && b.is_ascii_digit() && *a != b'0' => u32::from(a - b'0') * 10 + u32::from(b - b'0'),
            _ => return None,
        };
        (v <= most).then_some(v)
    }
    let mut parts = t.split(|&b| b == b'_');
    let op0 = small(after(parts.next()?, b's')?, 3)?;
    let op1 = small(parts.next()?, 7)?;
    let crn = small(after(parts.next()?, b'c')?, 15)?;
    let crm = small(after(parts.next()?, b'c')?, 15)?;
    let op2 = small(parts.next()?, 7)?;
    (parts.next().is_none() && op0 >= 2).then_some(SysReg { op0, op1, crn, crm, op2 })
}

/// A barrier option's immediate, 0..=15.
fn crm(v: i128) -> Option<u32> {
    (0..=15).contains(&v).then_some(v as u32)
}

/// Immediate `v` as the bits of a `wide` (64) or 32-bit operand: a negative one in two's
/// complement; `None` past the width.
fn imm_bits(v: i128, wide: bool) -> Option<u64> {
    match wide {
        true => (i128::from(i64::MIN)..=i128::from(u64::MAX)).contains(&v).then_some(v as u64),
        false => (i128::from(i32::MIN)..=i128::from(u32::MAX)).contains(&v).then_some(v as u64 & 0xffff_ffff),
    }
}

// The rows' encoders: each takes its shape or answers `No`.

fn branch(ops: &Ops<'_>) -> Fit {
    match *ops.args() {
        [Arg::Sym(sym)] => Fit::SymCall { sym, link: false },
        [_] => Fit::Branch { word: enc::b(0), field: Field::Imm26, target: 0 },
        _ => Fit::No,
    }
}

fn bl_call(ops: &Ops<'_>) -> Fit {
    match *ops.args() {
        [Arg::Sym(sym)] => Fit::SymCall { sym, link: true },
        [_] => Fit::Branch { word: enc::bl(0), field: Field::Imm26, target: 0 },
        _ => Fit::No,
    }
}

fn blr_reg(ops: &Ops<'_>) -> Fit {
    let [Arg::Gpr { reg, wide: true, sp: false }] = *ops.args() else { return Fit::No };
    Fit::Word(enc::blr(reg))
}

fn ret_op(ops: &Ops<'_>) -> Fit {
    match *ops.args() {
        [] => Fit::Word(enc::ret()),
        [Arg::Gpr { reg, wide: true, sp: false }] => Fit::Word(enc::ret_reg(reg)),
        _ => Fit::No,
    }
}

fn arithmetic_reg(ops: &Ops<'_>, encode: fn(Reg, Reg, Reg) -> u32) -> Fit {
    let [
        Arg::Gpr { reg: rd, wide, sp: false },
        Arg::Gpr { reg: rn, wide: w1, sp: false },
        Arg::Gpr { reg: rm, wide: w2, sp: false },
    ] = *ops.args()
    else {
        return Fit::No;
    };
    if w1 != wide || w2 != wide {
        return Fit::No;
    }
    Fit::Word(enc::sf(wide, encode(rd, rn, rm)))
}

fn mov_imm(ops: &Ops<'_>) -> Fit {
    let [Arg::Gpr { reg: rd, wide, sp: false }, Arg::Imm(v)] = *ops.args() else { return Fit::No };
    if (0..=0xffff).contains(&v)
        && let Ok(imm) = u32::try_from(v)
    {
        Fit::Word(enc::sf(wide, enc::movz(rd, imm, 0)))
    } else if (-0x10000..=-1).contains(&v)
        && let Ok(inv) = u32::try_from(!v)
    {
        Fit::Word(enc::sf(wide, enc::movn(rd, inv & 0xffff, 0)))
    } else {
        Fit::Bad { why: "a `mov` immediate outside -65536..=65535", at: 1 }
    }
}

/// Checks a signed 7-bit scaled pair offset (`off % scale == 0` and `-64 <= off/scale <= 63`).
fn pair_imm7(off: i128, scale: i128) -> Option<i32> {
    if off % scale != 0 {
        return None;
    }
    let scaled = off / scale;
    if (-64..=63).contains(&scaled) { i32::try_from(scaled).ok() } else { None }
}

fn pair_mem(ops: &Ops<'_>, load: bool) -> Fit {
    enum Mode {
        Off(Reg, i128, usize),
        Pre(Reg, i128, usize),
        Post(Reg, i128, usize),
    }
    let (a, b, mode) = match *ops.args() {
        [a, b, Arg::Addr { base, off, pre: false }] => (a, b, Mode::Off(base, off, 2)),
        [a, b, Arg::Addr { base, off, pre: true }] => (a, b, Mode::Pre(base, off, 2)),
        [a, b, Arg::Addr { base, off: 0, pre: false }, Arg::Imm(off)] => (a, b, Mode::Post(base, off, 3)),
        _ => return Fit::No,
    };
    match (a, b) {
        (Arg::Gpr { reg: r1, wide: true, sp: false }, Arg::Gpr { reg: r2, wide: true, sp: false }) => {
            let (base, off, at) = match mode {
                Mode::Off(b, o, at) | Mode::Pre(b, o, at) | Mode::Post(b, o, at) => (b, o, at),
            };
            let Some(off) = pair_imm7(off, 8).and_then(|_| i32::try_from(off).ok()) else {
                return Fit::Bad { why: "an `x` pair offset outside -512..=504 or not 8-aligned", at };
            };
            Fit::Word(match (load, mode) {
                (false, Mode::Off(..)) => enc::stp_off(r1, r2, base, off),
                (true, Mode::Off(..)) => enc::ldp_off(r1, r2, base, off),
                (false, Mode::Pre(..)) => enc::stp_pre(r1, r2, base, off),
                (true, Mode::Pre(..)) => enc::ldp_pre(r1, r2, base, off),
                (false, Mode::Post(..)) => enc::stp_post(r1, r2, base, off),
                (true, Mode::Post(..)) => enc::ldp_post(r1, r2, base, off),
            })
        }
        (Arg::Gpr { reg: r1, wide: false, sp: false }, Arg::Gpr { reg: r2, wide: false, sp: false }) => {
            let Mode::Off(base, off, at) = mode else { return Fit::No };
            let Some(off) = pair_imm7(off, 4).and_then(|_| i32::try_from(off).ok()) else {
                return Fit::Bad { why: "a `w` pair offset outside -256..=252 or not 4-aligned", at };
            };
            Fit::Word(if load { enc::ldp_w_off(r1, r2, base, off) } else { enc::stp_w_off(r1, r2, base, off) })
        }
        (Arg::Fp { reg: r1, width: FpWidth::Q }, Arg::Fp { reg: r2, width: FpWidth::Q }) => {
            let (base, off, at) = match mode {
                Mode::Off(b, o, at) | Mode::Pre(b, o, at) | Mode::Post(b, o, at) => (b, o, at),
            };
            let Some(imm7) = pair_imm7(off, 16) else {
                return Fit::Bad { why: "a `q` pair offset outside -1024..=1008 or not 16-aligned", at };
            };
            Fit::Word(match (load, mode) {
                (false, Mode::Off(..)) => enc::stp_q_off(r1, r2, base, imm7),
                (true, Mode::Off(..)) => enc::ldp_q_off(r1, r2, base, imm7),
                (false, Mode::Pre(..)) => enc::stp_q_pre(r1, r2, base, imm7),
                (true, Mode::Pre(..)) => enc::ldp_q_pre(r1, r2, base, imm7),
                (false, Mode::Post(..)) => enc::stp_q_post(r1, r2, base, imm7),
                (true, Mode::Post(..)) => enc::ldp_q_post(r1, r2, base, imm7),
            })
        }
        (Arg::Fp { reg: r1, width: FpWidth::D }, Arg::Fp { reg: r2, width: FpWidth::D }) => {
            let (base, off, at) = match mode {
                Mode::Off(b, o, at) | Mode::Pre(b, o, at) | Mode::Post(b, o, at) => (b, o, at),
            };
            let Some(off) = pair_imm7(off, 8).and_then(|_| i32::try_from(off).ok()) else {
                return Fit::Bad { why: "a `d` pair offset outside -512..=504 or not 8-aligned", at };
            };
            Fit::Word(match (load, mode) {
                (false, Mode::Off(..)) => enc::stp_d_off(r1, r2, base, off),
                (true, Mode::Off(..)) => enc::ldp_d_off(r1, r2, base, off),
                (false, Mode::Pre(..)) => enc::stp_pre_d(r1, r2, base, off),
                (true, Mode::Pre(..)) => enc::ldp_pre_d(r1, r2, base, off),
                (false, Mode::Post(..)) => enc::stp_post_d(r1, r2, base, off),
                (true, Mode::Post(..)) => enc::ldp_post_d(r1, r2, base, off),
            })
        }
        (Arg::Fp { reg: r1, width: FpWidth::S }, Arg::Fp { reg: r2, width: FpWidth::S }) => {
            let Mode::Off(base, off, at) = mode else { return Fit::No };
            let Some(off) = pair_imm7(off, 4).and_then(|_| i32::try_from(off).ok()) else {
                return Fit::Bad { why: "an `s` pair offset outside -256..=252 or not 4-aligned", at };
            };
            Fit::Word(if load { enc::ldp_s_off(r1, r2, base, off) } else { enc::stp_s_off(r1, r2, base, off) })
        }
        _ => Fit::No,
    }
}

fn single_mem(ops: &Ops<'_>, load: bool) -> Fit {
    match *ops.args() {
        [Arg::Gpr { reg, wide, sp: false }, Arg::Addr { base, off, pre: false }] => {
            let (size, scale) = if wide { (Size::X, 8i128) } else { (Size::W, 4i128) };
            if off >= 0 && off % scale == 0 && (off / scale) < 4096
                && let Ok(imm12) = u32::try_from(off / scale)
            {
                Fit::Word(if load { enc::ldr(size, reg, base, imm12) } else { enc::str(size, reg, base, imm12) })
            } else if (-256..=255).contains(&off)
                && let Ok(off) = i32::try_from(off)
            {
                Fit::Word(if load { enc::ldur(size, reg, base, off) } else { enc::stur(size, reg, base, off) })
            } else {
                Fit::Bad { why: "a `ldr`/`str` offset out of unsigned scaled / unscaled range", at: 1 }
            }
        }
        [Arg::Gpr { reg, wide: true, sp: false }, Arg::Addr { base, off: 0, pre: false }, Arg::Imm(off)] => {
            if (-256..=255).contains(&off)
                && let Ok(off) = i32::try_from(off)
            {
                Fit::Word(if load { enc::ldr_post(reg, base, off) } else { enc::str_post(reg, base, off) })
            } else {
                Fit::Bad { why: "a post-index `ldr`/`str` offset outside -256..=255", at: 2 }
            }
        }
        [Arg::Fp { reg, width: FpWidth::Q }, Arg::Addr { base, off, pre: false }] => {
            if off >= 0 && off % 16 == 0 && (off / 16) < 4096
                && let Ok(imm12) = u32::try_from(off / 16)
            {
                Fit::Word(if load { enc::ldr_q(reg, base, imm12) } else { enc::str_q(reg, base, imm12) })
            } else {
                Fit::Bad { why: "a `ldr`/`str q` offset out of unsigned 16-aligned range", at: 1 }
            }
        }
        [Arg::Fp { reg, width: w @ (FpWidth::D | FpWidth::S) }, Arg::Addr { base, off, pre: false }] => {
            let (size, scale) = if w == FpWidth::D { (Size::X, 8i128) } else { (Size::W, 4i128) };
            if off >= 0 && off % scale == 0 && (off / scale) < 4096
                && let Ok(imm12) = u32::try_from(off / scale)
            {
                let word = if load { enc::ldr(size, reg, base, imm12) } else { enc::str(size, reg, base, imm12) };
                Fit::Word(enc::fp_access(word))
            } else {
                Fit::Bad { why: "a `ldr`/`str d/s` offset out of unsigned scaled range", at: 1 }
            }
        }
        _ => Fit::No,
    }
}

fn branch_cond(ops: &Ops<'_>) -> Fit {
    let [_] = ops.args() else { return Fit::No };
    let suffix = &ops.mnemonic[2..];
    let cond = named(CONDS, suffix).or_else(|| named(&[("al", Cond::Al), ("nv", Cond::Nv)], suffix));
    match cond {
        Some(cond) => Fit::Branch { word: enc::b_cond(cond, 0), field: Field::Imm19, target: 0 },
        None => Fit::No,
    }
}

fn compare_branch(ops: &Ops<'_>, nonzero: bool) -> Fit {
    let [Arg::Gpr { reg, wide, sp: false }, _] = *ops.args() else { return Fit::No };
    let word = if nonzero { enc::cbnz(reg, 0) } else { enc::cbz(reg, 0) };
    Fit::Branch { word: enc::sf(wide, word), field: Field::Imm19, target: 1 }
}

fn test_branch(ops: &Ops<'_>, nonzero: bool) -> Fit {
    let [Arg::Gpr { reg, wide, sp: false }, Arg::Imm(bit), _] = *ops.args() else { return Fit::No };
    let width = if wide { 64 } else { 32 };
    if !(0..width).contains(&bit) {
        return Fit::Bad { why: "a tested bit outside the register width", at: 1 };
    }
    Fit::Branch { word: enc::test_branch(nonzero, reg, bit as u32), field: Field::Imm14, target: 2 }
}

fn arithmetic_imm(ops: &Ops<'_>, encode: fn(Reg, Reg, u32, bool) -> u32) -> Fit {
    let [Arg::Gpr { reg: rd, wide, sp: sd }, Arg::Gpr { reg: rn, wide: w, sp: sn }, Arg::Imm(imm)] = *ops.args() else {
        return Fit::No;
    };
    if w != wide || (rd == ZR && !sd) || (rn == ZR && !sn) {
        return Fit::No;
    }
    if !(0..=4095).contains(&imm) {
        return Fit::Bad { why: "an unshifted arithmetic immediate outside 0..=4095", at: 2 };
    }
    Fit::Word(enc::sf(wide, encode(rd, rn, imm as u32, false)))
}

fn load_exclusive(ops: &Ops<'_>, acquire: bool) -> Fit {
    let [Arg::Gpr { reg, wide, sp: false }, Arg::Addr { base, off: 0, pre: false }] = *ops.args() else { return Fit::No };
    Fit::Word(enc::ldxr(if wide { Size::X } else { Size::W }, acquire, reg, base))
}

fn store_exclusive(ops: &Ops<'_>, release: bool) -> Fit {
    let [Arg::Gpr { reg: status, wide: false, sp: false }, Arg::Gpr { reg, wide, sp: false }, Arg::Addr { base, off: 0, pre: false }] = *ops.args() else {
        return Fit::No;
    };
    if status == reg || (base != enc::SP && status == base) {
        return Fit::Bad { why: "exclusive store status overlaps data or address", at: 0 };
    }
    Fit::Word(enc::stxr(if wide { Size::X } else { Size::W }, release, status, reg, base))
}

fn load_pair(ops: &Ops<'_>) -> Fit {
    let [Arg::Gpr { reg, wide, sp: false }, Arg::Gpr { reg: other, wide: w, sp: false }, Arg::Addr { base, off: 0, pre: false }] = *ops.args() else {
        return Fit::No;
    };
    if w != wide {
        return Fit::No;
    }
    if reg == other {
        return Fit::Bad { why: "exclusive pair load has overlapping destinations", at: 1 };
    }
    Fit::Word(enc::exclusive_pair_width(wide, enc::ldxp(false, reg, other, base)))
}

fn store_pair(ops: &Ops<'_>) -> Fit {
    let [
        Arg::Gpr { reg: status, wide: false, sp: false }, Arg::Gpr { reg, wide, sp: false },
        Arg::Gpr { reg: other, wide: w, sp: false }, Arg::Addr { base, off: 0, pre: false },
    ] = *ops.args() else { return Fit::No };
    if w != wide {
        return Fit::No;
    }
    if status == reg || status == other || (base != enc::SP && status == base) {
        return Fit::Bad { why: "exclusive store status overlaps data or address", at: 0 };
    }
    Fit::Word(enc::exclusive_pair_width(wide, enc::stxp(false, status, reg, other, base)))
}

fn nop(ops: &Ops<'_>) -> Fit {
    match ops.args() {
        [] => Fit::Word(enc::nop()),
        _ => Fit::No,
    }
}

fn xpaclri(ops: &Ops<'_>) -> Fit {
    match ops.args() {
        [] => Fit::Word(0xd503_20ff),
        _ => Fit::No,
    }
}

fn br_reg(ops: &Ops<'_>) -> Fit {
    let [Arg::Gpr { reg, wide: true, sp: false }] = *ops.args() else { return Fit::No };
    Fit::Word(enc::br(reg))
}

fn mov_reg(ops: &Ops<'_>) -> Fit {
    let [Arg::Gpr { reg: rd, wide, sp: rd_sp }, Arg::Gpr { reg: rm, wide: rm_wide, sp: rm_sp }] = *ops.args() else {
        return Fit::No;
    };
    if wide != rm_wide {
        return Fit::No;
    }
    if rd_sp || rm_sp {
        if rd == ZR && !rd_sp {
            return Fit::Bad { why: "the zero register in `mov` with `sp` (register 31 is `sp` in `add #0`)", at: 0 };
        }
        if rm == ZR && !rm_sp {
            return Fit::Bad { why: "the zero register in `mov` with `sp` (register 31 is `sp` in `add #0`)", at: 1 };
        }
        Fit::Word(enc::sf(wide, enc::add_imm(rd, rm, 0, false)))
    } else {
        Fit::Word(enc::sf(wide, enc::mov(rd, rm)))
    }
}

fn sb(ops: &Ops<'_>) -> Fit {
    match ops.args() {
        [] => Fit::Word(enc::sb()),
        _ => Fit::No,
    }
}

fn isb(ops: &Ops<'_>) -> Fit {
    match *ops.args() {
        [] => Fit::Word(enc::isb(0b1111)),
        [Arg::Name] if ops.text(0).eq_ignore_ascii_case(b"sy") => Fit::Word(enc::isb(0b1111)),
        [Arg::Name] => Fit::Bad { why: "an `isb` option other than `sy`", at: 0 },
        [Arg::Imm(v)] => crm(v).map_or(Fit::Bad { why: "a barrier option outside 0..=15", at: 0 }, |c| Fit::Word(enc::isb(c))),
        _ => Fit::No,
    }
}

fn barrier(ops: &Ops<'_>, word: fn(u32) -> u32) -> Fit {
    match *ops.args() {
        [Arg::Name] => named(BARRIERS, ops.text(0))
            .map_or(Fit::Bad { why: "a barrier option `dmb` and `dsb` have not", at: 0 }, |c| Fit::Word(word(c))),
        [Arg::Imm(v)] => crm(v).map_or(Fit::Bad { why: "a barrier option outside 0..=15", at: 0 }, |c| Fit::Word(word(c))),
        _ => Fit::No,
    }
}

fn dsb(ops: &Ops<'_>) -> Fit {
    barrier(ops, enc::dsb)
}

fn dmb(ops: &Ops<'_>) -> Fit {
    barrier(ops, enc::dmb_crm)
}

fn mrs(ops: &Ops<'_>) -> Fit {
    let [Arg::Gpr { reg, wide: true, sp: false }, Arg::Name] = *ops.args() else { return Fit::No };
    match sysreg(ops.text(1)) {
        Ok((sysreg, _)) => Fit::Word(enc::mrs(sysreg, reg)),
        Err(why) => Fit::Bad { why, at: 1 },
    }
}

fn msr_reg(ops: &Ops<'_>) -> Fit {
    let [Arg::Name, Arg::Gpr { reg, wide: true, sp: false }] = *ops.args() else { return Fit::No };
    match sysreg(ops.text(0)) {
        Ok((sysreg, true)) => Fit::Word(enc::msr(sysreg, reg)),
        Ok((_, false)) => Fit::Bad { why: "a read-only system register written by `msr`", at: 0 },
        Err(why) => Fit::Bad { why, at: 0 },
    }
}

fn msr_imm(ops: &Ops<'_>) -> Fit {
    let [Arg::Name, Arg::Imm(v)] = *ops.args() else { return Fit::No };
    match (named(PSTATES, ops.text(0)), v) {
        (Some(field), 0..=1) => Fit::Word(enc::msr_imm(field, v as u32)),
        (Some(_), _) => Fit::Bad { why: "a PSTATE field's immediate outside 0..=1", at: 1 },
        (None, _) => Fit::Bad { why: "a PSTATE field outside the assembler's table (`dit`, `ssbs`, `tco`)", at: 0 },
    }
}

/// `dc cvau, Xt`: `sys #3, c7, c11, #1, Xt` (`0xd50b_7b20 | rt`).
fn dc(ops: &Ops<'_>) -> Fit {
    let [Arg::Name, Arg::Gpr { reg, wide: true, sp: false }] = *ops.args() else { return Fit::No };
    if ops.text(0).eq_ignore_ascii_case(b"cvau") {
        Fit::Word(0xd50b_7b20 | u32::from(reg))
    } else {
        Fit::Bad { why: "a `dc` cache-maintenance op outside the assembler's table (`cvau`)", at: 0 }
    }
}

/// `ic ivau, Xt`: `sys #3, c7, c5, #1, Xt` (`0xd50b_7520 | rt`).
fn ic(ops: &Ops<'_>) -> Fit {
    let [Arg::Name, Arg::Gpr { reg, wide: true, sp: false }] = *ops.args() else { return Fit::No };
    if ops.text(0).eq_ignore_ascii_case(b"ivau") {
        Fit::Word(0xd50b_7520 | u32::from(reg))
    } else {
        Fit::Bad { why: "an `ic` cache-maintenance op outside the assembler's table (`ivau`)", at: 0 }
    }
}

fn pacib(ops: &Ops<'_>) -> Fit {
    let [Arg::Gpr { reg: rd, wide: true, sp: false }, Arg::Gpr { reg: rn, wide: true, sp }] = *ops.args() else {
        return Fit::No;
    };
    match rn == 31 && !sp {
        true => Fit::Bad { why: "`xzr` as `pacib`'s modifier (31 is `sp` there; the zero modifier is `pacizb`)", at: 1 },
        false => Fit::Word(enc::pacib(rd, rn)),
    }
}

fn tst_reg(ops: &Ops<'_>) -> Fit {
    match *ops.args() {
        [Arg::Gpr { reg: rn, wide, sp: false }, Arg::Gpr { reg: rm, wide: w, sp: false }] if w == wide => {
            Fit::Word(enc::sf(wide, enc::ands(ZR, rn, rm)))
        }
        _ => Fit::No,
    }
}

fn tst_imm(ops: &Ops<'_>) -> Fit {
    let [Arg::Gpr { reg: rn, wide, sp: false }, Arg::Imm(v)] = *ops.args() else { return Fit::No };
    let size = if wide { 64 } else { 32 };
    match imm_bits(v, wide).and_then(|bits| enc::logical_imm(bits, size)) {
        Some(nrs) => Fit::Word(enc::sf(wide, enc::ands_imm(ZR, rn, nrs as u32))),
        None => Fit::Bad { why: "an immediate no logical immediate encodes (a repeated, rotated run of ones)", at: 1 },
    }
}

fn csel(ops: &Ops<'_>) -> Fit {
    let [
        Arg::Gpr { reg: rd, wide, sp: false },
        Arg::Gpr { reg: rn, wide: w1, sp: false },
        Arg::Gpr { reg: rm, wide: w2, sp: false },
        Arg::Name,
    ] = *ops.args()
    else {
        return Fit::No;
    };
    if w1 != wide || w2 != wide {
        return Fit::No;
    }
    match named(CONDS, ops.text(3)) {
        Some(cond) => Fit::Word(enc::sf(wide, enc::csel(rd, rn, rm, cond))),
        None => Fit::Bad { why: "a condition outside `eq`..`le` (`al` and `nv` are not taken)", at: 3 },
    }
}

fn eor(ops: &Ops<'_>) -> Fit {
    match *ops.args() {
        [
            Arg::Gpr { reg: rd, wide, sp: false },
            Arg::Gpr { reg: rn, wide: w1, sp: false },
            Arg::Gpr { reg: rm, wide: w2, sp: false },
        ] if w1 == wide && w2 == wide => Fit::Word(enc::sf(wide, enc::eor(rd, rn, rm))),
        _ => Fit::No,
    }
}

fn cmp_reg(ops: &Ops<'_>) -> Fit {
    match *ops.args() {
        [Arg::Gpr { reg: rn, wide, sp: false }, Arg::Gpr { reg: rm, wide: w, sp: false }] if w == wide => {
            Fit::Word(enc::sf(wide, enc::cmp(rn, rm)))
        }
        _ => Fit::No,
    }
}

fn cmp_imm(ops: &Ops<'_>) -> Fit {
    let [Arg::Gpr { reg: rn, wide, sp }, Arg::Imm(v)] = *ops.args() else { return Fit::No };
    if rn == 31 && !sp {
        return Fit::Bad { why: "the zero register in `cmp`'s immediate form (31 is `sp` there)", at: 0 };
    }
    match v {
        0..=4095 => Fit::Word(enc::sf(wide, enc::cmp_imm(rn, v as u32))),
        -4095..=-1 => Fit::Word(enc::sf(wide, enc::cmn_imm(rn, (-v) as u32))),
        _ => Fit::Bad { why: "a `cmp` immediate outside -4095..=4095 (the `lsl #12` form is not in the table)", at: 1 },
    }
}

fn fmadd(ops: &Ops<'_>) -> Fit {
    let [
        Arg::Fp { reg: rd, width },
        Arg::Fp { reg: rn, width: w1 },
        Arg::Fp { reg: rm, width: w2 },
        Arg::Fp { reg: ra, width: w3 },
    ] = *ops.args()
    else {
        return Fit::No;
    };
    if w1 != width || w2 != width || w3 != width {
        return Fit::No;
    }
    match width.ftype() {
        Some(ft) => Fit::Word(enc::fmadd(ft, rd, rn, rm, ra)),
        None => Fit::Bad { why: "a floating-point width outside `h`, `s`, `d`", at: 0 },
    }
}

fn fp1(ops: &Ops<'_>, op: Fp1) -> Fit {
    let [Arg::Fp { reg: rd, width }, Arg::Fp { reg: rn, width: w }] = *ops.args() else { return Fit::No };
    if w != width {
        return Fit::No;
    }
    match width.ftype() {
        Some(ft) => Fit::Word(enc::fp1(op, ft, rd, rn)),
        None => Fit::Bad { why: "a floating-point width outside `h`, `s`, `d`", at: 0 },
    }
}

fn frintn(ops: &Ops<'_>) -> Fit {
    fp1(ops, Fp1::Frintn)
}

fn fsqrt(ops: &Ops<'_>) -> Fit {
    fp1(ops, Fp1::Fsqrt)
}

fn fcvt(ops: &Ops<'_>) -> Fit {
    let [Arg::Fp { reg: rd, width: to }, Arg::Fp { reg: rn, width: from }] = *ops.args() else { return Fit::No };
    match (to.ftype(), from.ftype()) {
        _ if to == from => Fit::Bad { why: "an `fcvt` between two registers of one width", at: 1 },
        (Some(to), Some(from)) => Fit::Word(enc::fcvt_between(from, to, rd, rn)),
        (None, _) => Fit::Bad { why: "a floating-point width outside `h`, `s`, `d`", at: 0 },
        (_, None) => Fit::Bad { why: "a floating-point width outside `h`, `s`, `d`", at: 1 },
    }
}

/// `crc32{c}{b,h,w}` takes three `w` registers, `crc32{c}x` a `w`, a `w` and an `x`.
fn crc(ops: &Ops<'_>, c: bool, sz: u32) -> Fit {
    let [
        Arg::Gpr { reg: rd, wide: false, sp: false },
        Arg::Gpr { reg: rn, wide: false, sp: false },
        Arg::Gpr { reg: rm, wide, sp: false },
    ] = *ops.args()
    else {
        return Fit::No;
    };
    match wide == (sz == 3) {
        true => Fit::Word(enc::crc32(c, sz, rd, rn, rm)),
        false => Fit::No,
    }
}

fn crc32b(ops: &Ops<'_>) -> Fit {
    crc(ops, false, 0)
}

fn crc32h(ops: &Ops<'_>) -> Fit {
    crc(ops, false, 1)
}

fn crc32w(ops: &Ops<'_>) -> Fit {
    crc(ops, false, 2)
}

fn crc32x(ops: &Ops<'_>) -> Fit {
    crc(ops, false, 3)
}

fn crc32cb(ops: &Ops<'_>) -> Fit {
    crc(ops, true, 0)
}

fn crc32ch(ops: &Ops<'_>) -> Fit {
    crc(ops, true, 1)
}

fn crc32cw(ops: &Ops<'_>) -> Fit {
    crc(ops, true, 2)
}

fn crc32cx(ops: &Ops<'_>) -> Fit {
    crc(ops, true, 3)
}

/// Three vector operands of one arrangement.
fn three_v(ops: &Ops<'_>) -> Option<(Arr, Reg, Reg, Reg)> {
    match *ops.args() {
        [Arg::Vec { reg: rd, arr }, Arg::Vec { reg: rn, arr: a1 }, Arg::Vec { reg: rm, arr: a2 }] if a1 == arr && a2 == arr => {
            Some((arr, rd, rn, rm))
        }
        _ => None,
    }
}

fn cmeq(ops: &Ops<'_>) -> Fit {
    let Some((arr, rd, rn, rm)) = three_v(ops) else { return Fit::No };
    match arr.size == 3 && !arr.q {
        true => Fit::Bad { why: "the `1d` arrangement (`cmeq` takes `8b` to `2d`)", at: 0 },
        false => Fit::Word(enc::cmeq_v(arr.q, arr.size, rd, rn, rm)),
    }
}

fn and_v(ops: &Ops<'_>) -> Fit {
    let Some((arr, rd, rn, rm)) = three_v(ops) else { return Fit::No };
    match arr.size {
        0 => Fit::Word(enc::and_v(arr.q, rd, rn, rm)),
        _ => Fit::Bad { why: "a vector `and` arrangement other than `8b` or `16b`", at: 0 },
    }
}

fn shrn(ops: &Ops<'_>) -> Fit {
    let [Arg::Vec { reg: rd, arr: to }, Arg::Vec { reg: rn, arr: from }, Arg::Imm(shift)] = *ops.args() else {
        return Fit::No;
    };
    if to.q || to.size > 2 || !from.q || from.size != to.size + 1 {
        return Fit::Bad { why: "`shrn` arrangements other than `8b`/`8h`, `4h`/`4s`, `2s`/`2d` (`shrn2` is not in the table)", at: 0 };
    }
    let esize = 8u32 << to.size;
    match (1..=i128::from(esize)).contains(&shift) {
        true => Fit::Word(enc::shrn(esize, shift as u32, rd, rn)),
        false => Fit::Bad { why: "a `shrn` shift outside 1 to the destination element's bits", at: 2 },
    }
}
