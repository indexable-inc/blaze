//! The x86-64 templates (x86_64.rs) checked by an INDEPENDENT decoder: every template, filled
//! with random registers, immediates, displacements and branch offsets of its forms, decodes
//! (by the small decoder below, written from the Intel SDM Vol. 2A opcode tables and its
//! REX/ModRM/SIB rules, sharing no code with the encoders) to exactly the instructions its
//! pattern means, in the canonical long form (REX on every instruction naming a register; every
//! memory operand `mod = 10` with a SIB byte and no index), with each branch's origin at the end
//! of its instruction and each rip-relative site's field ending its instruction.

use blaze_lir::{IntCond, Op};
use blaze_own::Alloc;
use blaze_sim::{Broke, Config, Tape};
use blaze_tier::table::{Field, FieldKind, Form, ImmClass, Pattern, RESULT, TEMP0, TEMP1};

use super::x86_64::{Built, MAX_BYTES, MAX_FIELDS, templates};

/// A decoded operand.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Arg {
    Reg(u8),
    /// `[base + disp]`, no index.
    Mem { base: u8, disp: i32 },
    /// `[rip + disp]`.
    Rip(i32),
    Imm(i64),
    /// A branch displacement.
    Rel(i32),
}

/// A decoded instruction: mnemonic, operand size in bits (0 for a branch or `ud2`), up to two
/// operands (destination first), its byte range, whether it had REX, and whether its memory
/// operand (if any) was in the long form.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Insn {
    mn: &'static str,
    bits: u8,
    args: [Option<Arg>; 2],
    start: usize,
    end: usize,
    rex: bool,
    long_mem: bool,
}

const NO_INSN: Insn = Insn { mn: "", bits: 0, args: [None, None], start: 0, end: 0, rex: false, long_mem: true };
const MAX_INSNS: usize = 6;

/// The condition nibble's names (SDM Vol. 2A, Jcc and SETcc: 0 O, 1 NO, 2 B, 3 AE, 4 E, 5 NE,
/// 6 BE, 7 A, 8 S, 9 NS, A P, B NP, C L, D GE, E LE, F G).
const JCC: [&str; 16] = ["jo", "jno", "jb", "jae", "je", "jne", "jbe", "ja", "js", "jns", "jp", "jnp", "jl", "jge", "jle", "jg"];
const SETCC: [&str; 16] = [
    "seto", "setno", "setb", "setae", "sete", "setne", "setbe", "seta", "sets", "setns", "setp", "setnp", "setl", "setge", "setle", "setg",
];
/// The group-1 extensions of `81 /n` (SDM: ADD, OR, ADC, SBB, AND, SUB, XOR, CMP).
const GROUP1: [&str; 8] = ["add", "or", "adc", "sbb", "and", "sub", "xor", "cmp"];
/// The group-2 extensions of `C1 /n` and `D3 /n` (SDM: ROL, ROR, RCL, RCR, SHL, SHR, SAL, SAR).
const GROUP2: [&str; 8] = ["rol", "ror", "rcl", "rcr", "shl", "shr", "sal", "sar"];

struct Reader<'c> {
    code: &'c [u8],
    at: usize,
}

impl Reader<'_> {
    fn byte(&mut self) -> Result<u8, String> {
        let byte = *self.code.get(self.at).ok_or_else(|| format!("truncated at byte {}", self.at))?;
        self.at += 1;
        Ok(byte)
    }

    fn i32(&mut self) -> Result<i32, String> {
        let mut out = [0u8; 4];
        for slot in &mut out {
            *slot = self.byte()?;
        }
        Ok(i32::from_le_bytes(out))
    }

    fn i64(&mut self) -> Result<i64, String> {
        let mut out = [0u8; 8];
        for slot in &mut out {
            *slot = self.byte()?;
        }
        Ok(i64::from_le_bytes(out))
    }
}

struct ModRm {
    /// ModRM.reg with REX.R.
    reg: u8,
    /// ModRM.reg alone (an opcode extension).
    ext: u8,
    rm: Arg,
    long_mem: bool,
}

/// ModRM, then SIB and displacement where they follow (SDM Vol. 2A Tables 2-2 and 2-3, with
/// 2.2.1.6 for `mod = 00, rm = 101` as rip-relative in 64-bit mode).
fn modrm(read: &mut Reader<'_>, rex: u8) -> Result<ModRm, String> {
    let (r, x, b) = ((rex >> 2) & 1, (rex >> 1) & 1, rex & 1);
    let m = read.byte()?;
    let (md, ext, low) = (m >> 6, (m >> 3) & 7, m & 7);
    let reg = ext | (r << 3);
    if md == 3 {
        return Ok(ModRm { reg, ext, rm: Arg::Reg(low | (b << 3)), long_mem: true });
    }
    if md == 0 && low == 5 {
        return Ok(ModRm { reg, ext, rm: Arg::Rip(read.i32()?), long_mem: true });
    }
    let (base, has_sib) = if low == 4 {
        let sib = read.byte()?;
        let (scale, index, base) = (sib >> 6, ((sib >> 3) & 7) | (x << 3), sib & 7);
        if index != 4 {
            return Err(format!("an index register {index} where the templates have none"));
        }
        if scale != 0 {
            return Err(format!("a scale {scale} with no index"));
        }
        if md == 0 && base == 5 {
            return Err("a SIB with no base".to_string());
        }
        (base | (b << 3), true)
    } else {
        (low | (b << 3), false)
    };
    let disp = match md {
        0 => 0,
        1 => i32::from(read.byte()? as i8),
        _ => read.i32()?,
    };
    Ok(ModRm { reg, ext, rm: Arg::Mem { base, disp }, long_mem: md == 2 && has_sib })
}

/// Decodes `code` whole into instructions, or says where it could not.
fn decode(code: &[u8]) -> Result<([Insn; MAX_INSNS], usize), String> {
    let mut out = [NO_INSN; MAX_INSNS];
    let mut count = 0;
    let mut read = Reader { code, at: 0 };
    while read.at < code.len() {
        let start = read.at;
        let mut p66 = false;
        if code[read.at] == 0x66 {
            p66 = true;
            read.at += 1;
        }
        let mut rex = None;
        let next = code.get(read.at).copied();
        if let Some(byte) = next.filter(|byte| byte & 0xf0 == 0x40) {
            rex = Some(byte);
            read.at += 1;
        }
        let bits_rex = rex.unwrap_or(0);
        let w = bits_rex & 8 != 0;
        let size = if w {
            64
        } else if p66 {
            16
        } else {
            32
        };
        let op = read.byte()?;
        let mut long_mem = true;
        let (mn, bits, args): (&'static str, u8, [Option<Arg>; 2]) = match op {
            0x0f => {
                let op2 = read.byte()?;
                match op2 {
                    0xaf => {
                        let m = modrm(&mut read, bits_rex)?;
                        long_mem = m.long_mem;
                        ("imul", size, [Some(Arg::Reg(m.reg)), Some(m.rm)])
                    }
                    0xb6 | 0xb7 => {
                        let m = modrm(&mut read, bits_rex)?;
                        long_mem = m.long_mem;
                        (if op2 == 0xb6 { "movzx8" } else { "movzx16" }, size, [Some(Arg::Reg(m.reg)), Some(m.rm)])
                    }
                    0x80..=0x8f => (JCC[usize::from(op2 & 15)], 0, [Some(Arg::Rel(read.i32()?)), None]),
                    0x90..=0x9f => {
                        let m = modrm(&mut read, bits_rex)?;
                        if m.ext != 0 {
                            return Err(format!("SETcc with ModRM.reg {}", m.ext));
                        }
                        long_mem = m.long_mem;
                        (SETCC[usize::from(op2 & 15)], 8, [Some(m.rm), None])
                    }
                    0x0b => ("ud2", 0, [None, None]),
                    _ => return Err(format!("unknown opcode 0F {op2:02X} at byte {start}")),
                }
            }
            0x01 | 0x09 | 0x21 | 0x29 | 0x31 | 0x39 | 0x89 | 0x88 | 0x84 | 0x85 => {
                let m = modrm(&mut read, bits_rex)?;
                long_mem = m.long_mem;
                let (mn, bits) = match op {
                    0x01 => ("add", size),
                    0x09 => ("or", size),
                    0x21 => ("and", size),
                    0x29 => ("sub", size),
                    0x31 => ("xor", size),
                    0x39 => ("cmp", size),
                    0x89 => ("mov", size),
                    0x88 => ("mov", 8),
                    0x84 => ("test", 8),
                    _ => ("test", size),
                };
                (mn, bits, [Some(m.rm), Some(Arg::Reg(m.reg))])
            }
            0x8b => {
                let m = modrm(&mut read, bits_rex)?;
                long_mem = m.long_mem;
                ("mov", size, [Some(Arg::Reg(m.reg)), Some(m.rm)])
            }
            0x81 => {
                let m = modrm(&mut read, bits_rex)?;
                long_mem = m.long_mem;
                (GROUP1[usize::from(m.ext)], size, [Some(m.rm), Some(Arg::Imm(i64::from(read.i32()?)))])
            }
            0xc1 => {
                let m = modrm(&mut read, bits_rex)?;
                long_mem = m.long_mem;
                (GROUP2[usize::from(m.ext)], size, [Some(m.rm), Some(Arg::Imm(i64::from(read.byte()?)))])
            }
            0xd3 => {
                let m = modrm(&mut read, bits_rex)?;
                long_mem = m.long_mem;
                (GROUP2[usize::from(m.ext)], size, [Some(m.rm), Some(Arg::Reg(1))])
            }
            0xf7 => {
                let m = modrm(&mut read, bits_rex)?;
                long_mem = m.long_mem;
                let mn = match m.ext {
                    2 => "not",
                    3 => "neg",
                    ext => return Err(format!("F7 /{ext} is not in the templates")),
                };
                (mn, size, [Some(m.rm), None])
            }
            0xc7 => {
                let m = modrm(&mut read, bits_rex)?;
                if m.ext != 0 {
                    return Err(format!("C7 /{}", m.ext));
                }
                long_mem = m.long_mem;
                ("mov", size, [Some(m.rm), Some(Arg::Imm(i64::from(read.i32()?)))])
            }
            0xb8..=0xbf => {
                let reg = (op & 7) | ((bits_rex & 1) << 3);
                let imm = if w { read.i64()? } else { i64::from(read.i32()?) };
                ("mov", size, [Some(Arg::Reg(reg)), Some(Arg::Imm(imm))])
            }
            0xe9 => ("jmp", 0, [Some(Arg::Rel(read.i32()?)), None]),
            0xff => {
                let m = modrm(&mut read, bits_rex)?;
                if m.ext != 2 {
                    return Err(format!("FF /{}", m.ext));
                }
                long_mem = m.long_mem;
                ("call", 64, [Some(m.rm), None])
            }
            _ => return Err(format!("unknown opcode {op:02X} at byte {start}")),
        };
        let slot = out.get_mut(count).ok_or_else(|| "more instructions than a template holds".to_string())?;
        *slot = Insn { mn, bits, args, start, end: read.at, rex: rex.is_some(), long_mem };
        count += 1;
    }
    Ok((out, count))
}

/// The bytes of `built` with `fields` filled from `values` (by operand number): each field holds
/// bits `from_bit..from_bit + width` of its operand's value, low bit first, at bit `at`.
fn fill(built: &Built, fields: &[Field], values: &[u64; 7]) -> ([u8; MAX_BYTES], usize) {
    let mut out = [0u8; MAX_BYTES];
    let len = built.bytes().len();
    out[..len].copy_from_slice(built.bytes());
    for field in fields {
        let value = values[usize::from(field.operand)] >> field.from_bit;
        for i in 0..usize::from(field.width) {
            if (value >> i) & 1 == 1 {
                let bit = usize::from(field.at) + i;
                out[bit / 8] |= 1 << (bit % 8);
            }
        }
    }
    (out, len)
}

fn sext32(value: u64) -> i64 {
    i64::from(value as u32 as i32)
}

/// The suffix a condition takes, by the SDM's mnemonics (unsigned: below/above; signed:
/// less/greater), written apart from the generator's nibble table.
fn suffix(variant: u8) -> Result<&'static str, String> {
    let cond = IntCond::ALL.get(usize::from(variant)).ok_or_else(|| format!("variant {variant} is no condition"))?;
    Ok(match cond {
        IntCond::Eq => "e",
        IntCond::Ne => "ne",
        IntCond::Ult => "b",
        IntCond::Ule => "be",
        IntCond::Ugt => "a",
        IntCond::Uge => "ae",
        IntCond::Slt => "l",
        IntCond::Sle => "le",
        IntCond::Sgt => "g",
        IntCond::Sge => "ge",
    })
}

fn named(prefix: &str, suffix: &str, table: &[&'static str; 16]) -> Result<&'static str, String> {
    table
        .iter()
        .copied()
        .find(|name| name.strip_prefix(prefix) == Some(suffix))
        .ok_or_else(|| format!("no `{prefix}{suffix}`"))
}

type Want = (&'static str, u8, [Option<Arg>; 2]);

/// What a pattern's template must decode to, for operand values `v` (by operand number).
fn expected(p: &Pattern, v: &[u64; 7]) -> Result<Vec<Want>, String> {
    use Arg::{Imm, Mem, Reg, Rel, Rip};
    let reg = |n: usize| Reg(v[n] as u8);
    let (r, t) = (usize::from(RESULT), usize::from(TEMP0));
    let bits = if p.width == 64 { 64 } else { 32 };
    let alu = match p.op {
        Op::Add => Some("add"),
        Op::Sub => Some("sub"),
        Op::And => Some("and"),
        Op::Or => Some("or"),
        Op::Xor => Some("xor"),
        Op::Mul => Some("imul"),
        Op::Shl => Some("shl"),
        Op::Lshr => Some("shr"),
        Op::Ashr => Some("sar"),
        Op::Rotl => Some("rol"),
        Op::Rotr => Some("ror"),
        Op::Not => Some("not"),
        Op::Neg => Some("neg"),
        _ => None,
    };
    let gpr = Form::Reg(blaze_tier::RegClass::Gpr);
    let into_r = ("mov", bits, [Some(reg(r)), Some(reg(t))]);
    let from_a = ("mov", bits, [Some(reg(t)), Some(reg(0))]);
    Ok(match (p.op, p.operands, alu) {
        (Op::Not | Op::Neg, [f0, Form::None, Form::None, Form::None], Some(mn)) if f0 == gpr => {
            vec![from_a, (mn, bits, [Some(reg(t)), None]), into_r]
        }
        (Op::Shl | Op::Lshr | Op::Ashr | Op::Rotl | Op::Rotr, [_, f1, _, _], Some(mn)) if f1 == gpr => vec![
            from_a,
            ("mov", 64, [Some(Reg(1)), Some(reg(1))]),
            (mn, bits, [Some(reg(t)), Some(Reg(1))]),
            into_r,
        ],
        (Op::Shl | Op::Lshr | Op::Ashr | Op::Rotl | Op::Rotr, [_, Form::Imm(_), _, _], Some(mn)) => {
            vec![from_a, (mn, bits, [Some(reg(t)), Some(Imm((v[1] & 0xff) as i64))]), into_r]
        }
        (_, [_, f1, _, _], Some(mn)) if f1 == gpr => vec![from_a, (mn, bits, [Some(reg(t)), Some(reg(1))]), into_r],
        (_, [_, Form::Imm(ImmClass::S32), _, _], Some(mn)) => {
            vec![from_a, (mn, bits, [Some(reg(t)), Some(Imm(sext32(v[1])))]), into_r]
        }
        (Op::Icmp, [_, f1, _, _], None) => {
            let cmp = if f1 == gpr {
                ("cmp", bits, [Some(reg(0)), Some(reg(1))])
            } else {
                ("cmp", bits, [Some(reg(0)), Some(Imm(sext32(v[1])))])
            };
            vec![cmp, (named("set", suffix(p.variant)?, &SETCC)?, 8, [Some(reg(t)), None]), ("movzx8", 64, [Some(reg(r)), Some(reg(t))])]
        }
        (Op::Br, [Form::None, Form::Block, _, f3], None) => {
            let inner = p.inner.ok_or_else(|| "a fused br without its inner icmp".to_string())?;
            let cmp = if f3 == gpr {
                ("cmp", bits, [Some(reg(2)), Some(reg(3))])
            } else {
                ("cmp", bits, [Some(reg(2)), Some(Imm(sext32(v[3])))])
            };
            vec![cmp, (named("j", suffix(inner.variant)?, &JCC)?, 0, [Some(Rel(v[1] as u32 as i32)), None])]
        }
        (Op::Br, [_, Form::Block, Form::None, Form::None], None) => {
            vec![("test", 8, [Some(reg(0)), Some(reg(0))]), ("jne", 0, [Some(Rel(v[1] as u32 as i32)), None])]
        }
        (Op::Jump, _, None) => vec![("jmp", 0, [Some(Rel(v[0] as u32 as i32)), None])],
        (Op::Iconst, _, None) => {
            let bits = if p.width == 64 { 64 } else { 32 };
            vec![("mov", bits, [Some(reg(r)), Some(Imm(sext32(v[0])))])]
        }
        (Op::IconstWide, _, None) => vec![("mov", 64, [Some(reg(r)), Some(Imm(v[0] as i64))])],
        (Op::Store, [_, Form::None, Form::None, Form::Slot], None) => {
            vec![("mov", p.width, [Some(Mem { base: 4, disp: v[3] as u32 as i32 }), Some(reg(0))])]
        }
        (Op::Load, [first, _, third, _], None) => {
            // Through a register `[a + offset]`, or a frame slot `[rsp + slot]` (base 4).
            let mem = if third == Form::Slot && first == Form::None {
                Mem { base: 4, disp: v[2] as u32 as i32 }
            } else {
                Mem { base: v[0] as u8, disp: v[1] as u32 as i32 }
            };
            let (mn, bits) = match p.width {
                8 => ("movzx8", 64),
                16 => ("movzx16", 64),
                32 => ("mov", 32),
                _ => ("mov", 64),
            };
            vec![(mn, bits, [Some(reg(r)), Some(mem)])]
        }
        (Op::Store, _, None) => {
            let mem = Mem { base: v[1] as u8, disp: v[2] as u32 as i32 };
            vec![("mov", p.width, [Some(mem), Some(reg(0))])]
        }
        (Op::Call, _, None) => vec![("call", 64, [Some(Rip(v[0] as u32 as i32)), None])],
        (Op::CallIndirect, _, None) => vec![("call", 64, [Some(reg(0)), None])],
        (Op::FuncAddr | Op::DataAddr, _, None) => vec![("mov", 64, [Some(reg(r)), Some(Rip(v[0] as u32 as i32))])],
        (Op::Unreachable, _, None) => vec![("ud2", 0, [None, None])],
        _ => return Err(format!("the test has no expectation for {p:?}")),
    })
}

/// One template with these field rows and operand values: decodes to its pattern's meaning, in
/// the long form, branch and site fields ending their instructions.
fn check_one(built: &Built, fields: &[Field], v: &[u64; 7]) -> Result<(), String> {
    let (bytes, len) = fill(built, fields, v);
    let (insns, count) = decode(&bytes[..len])?;
    let insns = &insns[..count];
    let want = expected(&built.pattern, v)?;
    if insns.len() != want.len() {
        return Err(format!("{} instructions decoded, {} expected: {insns:?}", insns.len(), want.len()));
    }
    for (insn, &(mn, bits, args)) in insns.iter().zip(&want) {
        if (insn.mn, insn.bits, insn.args) != (mn, bits, args) {
            return Err(format!("decoded {} {} {:?}, expected {mn} {bits} {args:?}", insn.mn, insn.bits, insn.args));
        }
        let names_a_register = insn.args.iter().flatten().any(|arg| matches!(arg, Arg::Reg(_) | Arg::Mem { .. } | Arg::Rip(_)));
        if names_a_register && !insn.rex {
            return Err(format!("{} without REX: not the canonical form", insn.mn));
        }
        if !insn.long_mem {
            return Err(format!("{}'s memory operand is not `mod = 10` with a SIB", insn.mn));
        }
    }
    for field in fields {
        let first = usize::from(field.at) / 8;
        let last = (usize::from(field.at) + usize::from(field.width)).div_ceil(8);
        let insn = insns
            .iter()
            .find(|insn| insn.start <= first && first < insn.end)
            .ok_or_else(|| format!("field {field:?} is in no instruction"))?;
        if last > insn.end {
            return Err(format!("field {field:?} runs past its instruction"));
        }
        match field.kind {
            FieldKind::Branch { scale, from } => {
                if scale != 0 || usize::from(from) != insn.end || last != insn.end {
                    return Err(format!("branch field {field:?}: the origin must be its instruction's end {}", insn.end));
                }
            }
            FieldKind::Site(_) => {
                if last != insn.end {
                    return Err(format!("site field {field:?} must end its instruction (rip-relative from its end)"));
                }
            }
            FieldKind::Reg | FieldKind::Imm | FieldKind::Frame { .. } => {}
        }
    }
    Ok(())
}

/// Draws a value for each operand: a register number 0..16 for a register operand, the result
/// and the temporaries; any 64-bit value for an immediate, block, site or slot operand.
fn draw(tape: &Tape, pattern: &Pattern) -> [u64; 7] {
    let mut v = [0u64; 7];
    for (n, form) in pattern.operands.iter().enumerate() {
        v[n] = match form {
            Form::Reg(_) => tape.below(16, "register"),
            Form::None => 0,
            Form::Imm(_) | Form::Site(_) | Form::Block | Form::Slot => tape.below(u64::MAX, "value"),
        };
    }
    for n in [RESULT, TEMP0, TEMP1] {
        v[usize::from(n)] = tape.below(16, "register");
    }
    v
}

/// Every template, filled with random operands of its forms, decodes by the independent decoder
/// to its pattern's instructions in the canonical long form.
/// Expected line: `test table_gen::x86_64_tests::every_template_decodes_as_its_pattern ... ok`.
#[test]
fn every_template_decodes_as_its_pattern() {
    let all = templates(Alloc::global()).expect("the x86-64 templates build");
    assert!(all.len() > 150, "{} templates", all.len());
    blaze_sim::check(Config::cases(300), |tape: &Tape| -> Result<(), Broke> {
        for built in all.iter() {
            let v = draw(tape, &built.pattern);
            check_one(built, built.fields(), &v).map_err(|why| Broke::Oracle(format!("{:?}: {why}", built.pattern)))?;
        }
        Ok(())
    });
}

/// Filled with zeros, every template's register fields name register 0 and its bytes are the
/// template's: every field's bits are zero in the bytes and inside them, and names an operand.
/// Expected line: `test table_gen::x86_64_tests::fields_are_zero_and_inside ... ok`.
#[test]
fn fields_are_zero_and_inside() {
    let all = templates(Alloc::global()).expect("the x86-64 templates build");
    for built in all.iter() {
        assert!(built.fields().len() <= MAX_FIELDS);
        for field in built.fields() {
            assert!(field.operand <= TEMP1, "{:?}: {field:?} names no operand", built.pattern);
            for i in 0..usize::from(field.width) {
                let bit = usize::from(field.at) + i;
                let byte = built.bytes().get(bit / 8).copied();
                assert!(byte.is_some(), "{:?}: {field:?} is past the bytes", built.pattern);
                assert!(byte.unwrap_or(0) & (1 << (bit % 8)) == 0, "{:?}: {field:?} is not zero", built.pattern);
            }
        }
    }
}

/// Built twice, the list is equal; it is sorted by pattern with no pattern twice.
/// Expected line: `test table_gen::x86_64_tests::templates_are_sorted_unique_and_repeatable ... ok`.
#[test]
fn templates_are_sorted_unique_and_repeatable() {
    let one = templates(Alloc::global()).expect("the x86-64 templates build");
    let two = templates(Alloc::global()).expect("the x86-64 templates build");
    assert!(one.as_slice() == two.as_slice());
    for pair in one.windows(2) {
        assert!(pair[0].pattern < pair[1].pattern, "{:?} then {:?}", pair[0].pattern, pair[1].pattern);
    }
}

/// PLANTED BUG: the `add` template at 64 bits with its REX bit-3 fields dropped (a generator that
/// forgets `from_bit = 3`) is caught as soon as an operand is r8..r15, and the real template
/// passes the same values (the control).
/// Expected line: `test table_gen::x86_64_tests::planted_dropped_rex_fields_are_caught ... ok`.
#[test]
fn planted_dropped_rex_fields_are_caught() {
    let all = templates(Alloc::global()).expect("the x86-64 templates build");
    let gpr = Form::Reg(blaze_tier::RegClass::Gpr);
    let add = all
        .iter()
        .find(|b| b.pattern.op == Op::Add && b.pattern.width == 64 && b.pattern.operands[1] == gpr)
        .expect("an add of two registers at 64 bits");
    let mut planted = [Field { at: 0, width: 0, operand: 0, from_bit: 0, kind: FieldKind::Imm }; MAX_FIELDS];
    let mut count = 0;
    for field in add.fields().iter().filter(|f| f.from_bit != 3) {
        planted[count] = *field;
        count += 1;
    }
    assert!(count < add.fields().len(), "the plant removed nothing");
    let mut v = [0u64; 7];
    v[0] = 9;
    v[1] = 2;
    v[usize::from(RESULT)] = 13;
    v[usize::from(TEMP0)] = 11;
    assert_eq!(check_one(add, add.fields(), &v), Ok(()));
    assert!(check_one(add, &planted[..count], &v).is_err(), "the planted template passed");
}
