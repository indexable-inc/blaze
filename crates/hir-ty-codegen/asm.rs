//! `asm!` blocks. Cranelift has no inline assembly, so a block becomes a function of its own
//! that the function around it calls, as in rustc's Cranelift backend: the caller writes the
//! inputs to a stack slot and passes its address, and the function loads the registers from
//! it, runs the block, and writes the outputs back.

use std::{
    fmt::Write,
    path::PathBuf,
    process::Command,
    sync::{Arc, Mutex, OnceLock},
};

use cranelift_codegen::binemit::Reloc;
use hir_def::hir::{AsmOptions, AsmPiece, InlineAsmRegOrRegClass};
use object::{
    Architecture, BinaryFormat, Object, ObjectSection, ObjectSymbol, RelocationFlags,
    RelocationTarget, elf, macho, pe,
};
use rustc_hash::FxHashMap;
use sha2::{Digest, Sha256};

use super::{Arch, ObjectFormat, Target, store::Store};

mod aarch64;

/// The name of the function in the text of a [`Wrapper`].
const SYMBOL: &str = "ra_jit_asm_block";

/// What the name of an [`Operand::Sym`] starts with.
const SYM: &str = "ra_jit_sym_";

/// A function of one instruction that Cranelift does not have: `xgetbv` reads the extended
/// control register its argument numbers.
pub(super) fn xgetbv() -> String {
    let [number, ..] = Convention::of_target().arguments;
    let number = GPR[number][1];
    format!(
        ".globl {SYMBOL}\n{SYMBOL}:\nmov ecx, {number}\nxgetbv\nshl rdx, 32\nor rax, rdx\nret\n"
    )
}

/// The hint a spin loop gives the processor between attempts.
pub(super) fn pause() -> String {
    format!(".globl {SYMBOL}\n{SYMBOL}:\npause\nret\n")
}

/// AArch64's instruction barrier, of the whole system as `__isb(SY)` asks for it.
pub(super) fn isb() -> String {
    format!(".globl {SYMBOL}\n{SYMBOL}:\nisb\nret\n")
}

/// AArch64's CRC-32 of its second argument into the checksum of its first, as `instruction`
/// (`crc32b` to `crc32cx`) says: the `x` ones read 64 bits.
pub(super) fn crc32(instruction: &str) -> String {
    let data = if instruction.ends_with('x') { "x1" } else { "w1" };
    format!(".arch_extension crc\n.globl {SYMBOL}\n{SYMBOL}:\n{instruction} w0, w0, {data}\nret\n")
}

/// The directives that have the assembler take the instructions of `features`, those a
/// function's `#[target_feature]` enables, for assembly of its `asm!`: rustc gives LLVM the
/// features with the function the `asm!` is in, and this assembly is assembled apart from
/// it. Only AArch64's assembler refuses an instruction the target lacks (`crc32w` on
/// `aarch64-unknown-linux-gnu`). A feature it has no extension for is taken with the version
/// of the architecture that has it (`mrs x0, dit` with `.arch armv8.4-a`), which comes first,
/// as `.arch` sets the extensions anew.
pub(super) fn extensions<'a>(arch: Arch, features: impl IntoIterator<Item = &'a str>) -> String {
    if arch != Arch::Aarch64 {
        return String::new();
    }
    let features: Vec<&str> = features.into_iter().collect();
    let versions = features.iter().filter_map(|it| aarch64_version(it));
    // Armv9.n has what Armv8.(n + 5) has.
    let version = versions.fold(None, |most: Option<(u8, u8)>, (major, minor)| {
        let later = |(major, minor): (u8, u8)| if major == 9 { minor + 5 } else { minor };
        let nine = major == 9 || most.is_some_and(|it| it.0 == 9);
        let minor = later((major, minor)).max(most.map_or(0, later));
        Some(if nine { (9, minor - 5) } else { (8, minor) })
    });
    let mut names: Vec<&str> = features.into_iter().filter_map(aarch64_extension).collect();
    names.sort_unstable();
    names.dedup();
    let version = version.map(|(major, minor)| match minor {
        0 => format!(".arch armv{major}-a\n"),
        _ => format!(".arch armv{major}.{minor}-a\n"),
    });
    version.into_iter().chain(names.iter().map(|it| format!(".arch_extension {it}\n"))).collect()
}

/// The version of AArch64, as major and minor, that has a target feature LLVM's assembler
/// has no extension of, or that the feature is (`v8.4a`).
fn aarch64_version(feature: &str) -> Option<(u8, u8)> {
    Some(match feature {
        "vh" => (8, 1),
        "fcma" | "jsconv" => (8, 3),
        "dit" | "lse2" | "rcpc2" => (8, 4),
        "bti" | "flagm2" | "frintts" => (8, 5),
        "ecv" => (8, 6),
        "wfxt" => (8, 7),
        _ => {
            // `v9a` is Armv9.0.
            let version = feature.strip_prefix('v')?.strip_suffix('a')?;
            let (major, minor) = version.split_once('.').unwrap_or((version, "0"));
            let major = major.parse().ok().filter(|it| matches!(it, 8 | 9))?;
            (major, minor.parse().ok()?)
        }
    })
}

/// The name LLVM's AArch64 assembler gives the extension of a target feature as Rust names
/// it, where the two differ.
fn aarch64_extension(feature: &str) -> Option<&'static str> {
    Some(match feature {
        "neon" => "simd",
        "fhm" => "fp16fml",
        "rand" => "rng",
        "mte" => "memtag",
        "dpb" => "ccpp",
        "dpb2" => "ccdp",
        "paca" | "pacg" => "pauth",
        "spe" => "profile",
        "crc" => "crc",
        "lse" => "lse",
        "rdm" => "rdm",
        "aes" => "aes",
        "sha2" => "sha2",
        "sha3" => "sha3",
        "sm4" => "sm4",
        "dotprod" => "dotprod",
        "fp16" => "fp16",
        "ras" => "ras",
        "rcpc" => "rcpc",
        "sb" => "sb",
        "ssbs" => "ssbs",
        "flagm" => "flagm",
        "i8mm" => "i8mm",
        "bf16" => "bf16",
        "f32mm" => "f32mm",
        "f64mm" => "f64mm",
        "sve" => "sve",
        "sve2" => "sve2",
        "sve2-aes" => "sve2-aes",
        "sve2-sm4" => "sve2-sm4",
        "sve2-sha3" => "sve2-sha3",
        "sve2-bitperm" => "sve2-bitperm",
        "tme" => "tme",
        "mops" => "mops",
        _ => return None,
    })
}

/// Returns its first float argument with the second left in the register it was passed in,
/// which is where a pair of floats is returned.
pub(super) fn join_floats() -> String {
    format!(".globl {SYMBOL}\n{SYMBOL}:\nret\n")
}

/// Answers what is in the two registers a pair of floats is returned in, as integers.
pub(super) fn split_floats() -> String {
    format!(".globl {SYMBOL}\n{SYMBOL}:\nmovq rax, xmm0\nmovq rdx, xmm1\nret\n")
}

/// `vcvtps2ph` makes half-precision floats of the single-precision ones at its first
/// argument and writes them to its second. The rounding is part of the instruction, so there
/// is one for each of the eight, and the third argument says which runs.
pub(super) fn vcvtps2ph(register: &str) -> String {
    let [[from, ..], [to, ..], [which, which_low, ..]] =
        Convention::of_target().arguments.map(|row| GPR[row]);
    let mut text = format!(
        ".globl {SYMBOL}\n{SYMBOL}:\nvmovups {register}, [{from}]\nand {which_low}, 7\n\
         shl {which_low}, 4\nlea rax, [rip + .Lrounding]\nadd rax, {which}\njmp rax\n\
         .p2align 4\n.Lrounding:\n"
    );
    for rounding in 0..8 {
        _ = write!(text, ".p2align 4\nvcvtps2ph xmm0, {register}, {rounding}\njmp .Lconverted\n");
    }
    _ = write!(text, ".Lconverted:\nvmovdqu [{to}], xmm0\nvzeroupper\nret\n");
    text
}

/// A function of one instruction over 128-bit vectors that Cranelift does not have. `xmm0`
/// and `xmm1` are loaded from where the first two arguments point and `xmm2` from where the
/// third does, which is where `result` is written once `instruction` ran.
pub(super) fn vector_instruction(instruction: &str, result: &str) -> String {
    let [[a, ..], [b, ..], [c, ..]] = Convention::of_target().arguments.map(|row| GPR[row]);
    format!(
        ".globl {SYMBOL}\n{SYMBOL}:\nmovdqu xmm0, [{a}]\nmovdqu xmm1, [{b}]\n\
         movdqu xmm2, [{c}]\n{instruction}\nmovdqu [{c}], {result}\nret\n"
    )
}

/// The same for AArch64, with the instructions of `extension`: `v0` to `v2` are loaded from
/// where the three arguments point, and `v0` is written where the third does.
pub(super) fn neon_instruction(extension: &str, instruction: &str) -> String {
    format!(
        ".arch_extension {extension}\n.globl {SYMBOL}\n{SYMBOL}:\nldr q0, [x0]\nldr q1, [x1]\n\
         ldr q2, [x2]\n{instruction}\nstr q0, [x2]\nret\n"
    )
}

/// `sha1rnds4` does four rounds of SHA-1 on the vectors at its first two arguments. Which
/// function the rounds use is part of the instruction, so there is one for each of the four:
/// the byte at the third argument says which runs, and the result is written over it.
pub(super) fn sha1rnds4() -> String {
    let [[a, ..], [b, ..], [c, ..]] = Convention::of_target().arguments.map(|row| GPR[row]);
    let mut text = format!(
        ".globl {SYMBOL}\n{SYMBOL}:\nmovdqu xmm0, [{a}]\nmovdqu xmm1, [{b}]\n\
         movzx eax, byte ptr [{c}]\nand eax, 3\nshl eax, 4\nlea r11, [rip + .Lrounds]\n\
         add r11, rax\njmp r11\n.p2align 4\n.Lrounds:\n"
    );
    for function in 0..4 {
        _ = write!(text, ".p2align 4\nsha1rnds4 xmm0, xmm1, {function}\njmp .Lround\n");
    }
    _ = write!(text, ".Lround:\nmovdqu [{c}], xmm0\nret\n");
    text
}

/// Calls the variadic function whose address is the last argument with the arguments before
/// it, and says that up to eight vector registers hold arguments. `ints` of those are in
/// integer registers and `stack` of them on the stack, which says where the address is.
pub(super) fn counted_variadic(ints: usize, stack: usize) -> String {
    const ARGUMENTS: [&str; 6] = ["rdi", "rsi", "rdx", "rcx", "r8", "r9"];
    let callee = match ARGUMENTS.get(ints) {
        Some(register) => format!("mov r11, {register}"),
        None => format!("mov r11, qword ptr [rsp + {}]", 8 + stack * 8),
    };
    format!(".globl {SYMBOL}\n{SYMBOL}:\n{callee}\nmov eax, 8\njmp r11\n")
}

/// One operand of a block, as the function around it sees it.
pub(super) enum Operand<'a> {
    Reg {
        reg: &'a InlineAsmRegOrRegClass,
        /// How many bytes go in, come out, or both: a register is not used for two sizes.
        size: Option<u32>,
        input: bool,
        output: bool,
        /// Whether the block may change it, which it may with no `output` too.
        written: bool,
    },
    Const(i128),
    /// A `label` block. The function returns which one the block jumped to, counted from 1,
    /// or 0 when the block ran off its end.
    Label,
    /// A function or a static: the text has a name for it that [`Assembled::relocs`] gives
    /// back as the operand's index.
    Sym,
}

/// The name in the text of the function or static that is operand `index`.
fn sym(index: usize) -> String {
    format!("{SYM}{index}")
}

/// The name in a [`Wrapper`] of the label that is operand `index`.
fn label(format: ObjectFormat, index: usize) -> String {
    format!("{}ra_jit_label_{index}", format.local_label())
}

/// The name in a [`Wrapper`] of where every way out of the block meets.
fn done(format: ObjectFormat) -> String {
    format!("{}ra_jit_done", format.local_label())
}

/// The labels among the operands, by index.
fn labels<'a>(operands: &'a [Operand<'_>]) -> impl Iterator<Item = usize> + 'a {
    operands.iter().enumerate().filter(|it| matches!(it.1, Operand::Label)).map(|it| it.0)
}

/// The text of a `#[naked]` function, which is its template and nothing else.
pub(super) fn naked(
    arch: Arch,
    template: &[AsmPiece],
    options: AsmOptions,
    operands: &[Operand<'_>],
) -> Result<String, String> {
    let att = options.contains(AsmOptions::ATT_SYNTAX) && arch == Arch::X86_64;
    let mut text = format!(".globl {SYMBOL}\n{SYMBOL}:\n");
    if att {
        text.push_str(".att_syntax\n");
    }
    for piece in template {
        match piece {
            AsmPiece::Lit(lit) => text.push_str(lit),
            AsmPiece::Operand { index, .. } => match operands.get(*index) {
                Some(Operand::Const(value)) => _ = write!(text, "{value}"),
                Some(Operand::Sym) => text.push_str(&sym(*index)),
                _ => {
                    return Err("a `naked_asm!` operand that is a register or a label".to_owned());
                }
            },
        }
    }
    text.push('\n');
    if att {
        text.push_str(".intel_syntax noprefix\n");
    }
    match arch {
        Arch::X86_64 | Arch::Aarch64 => Ok(text),
        Arch::Wasm32 => Err("inline assembly in a wasm module".to_owned()),
    }
}

pub(super) struct Wrapper {
    /// Assembly of a function that takes the address of the slot.
    pub(super) text: String,
    pub(super) slot_size: u32,
    /// Where in the slot each operand that has a value is.
    pub(super) offsets: Vec<Option<u32>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Reg {
    /// By its row in `GPR`.
    Gpr(usize),
    /// `xmm`, `ymm` and `zmm` of one number are one register.
    Vector(usize),
    /// AVX-512's mask registers `k1` to `k7`: `k0` cannot mask, and `asm!` takes it only as
    /// one a block overwrites.
    Mask(usize),
    /// One a call does not keep and that holds no value here: nothing to do.
    Scratch,
}

const GPR: [[&str; 4]; 16] = [
    ["rax", "eax", "ax", "al"],
    ["rcx", "ecx", "cx", "cl"],
    ["rdx", "edx", "dx", "dl"],
    ["rsi", "esi", "si", "sil"],
    ["rdi", "edi", "di", "dil"],
    ["r8", "r8d", "r8w", "r8b"],
    ["r9", "r9d", "r9w", "r9b"],
    ["r10", "r10d", "r10w", "r10b"],
    ["r11", "r11d", "r11w", "r11b"],
    ["r12", "r12d", "r12w", "r12b"],
    ["r13", "r13d", "r13w", "r13b"],
    ["r14", "r14d", "r14w", "r14b"],
    ["r15", "r15d", "r15w", "r15b"],
    // `asm!` takes none of these as an operand: the first holds the slot here.
    ["rbx", "ebx", "bx", "bl"],
    ["rbp", "ebp", "bp", "bpl"],
    ["rsp", "esp", "sp", "spl"],
];
/// What a calling convention says of the registers a block may name.
struct Convention {
    /// The rows of `GPR` the first three arguments are in.
    arguments: [usize; 3],
    /// The rows of `GPR` a function has to leave as it found them.
    kept: &'static [usize],
    /// The vector registers a function has to leave the low sixteen bytes of as it found
    /// them.
    kept_vectors: std::ops::Range<usize>,
}

impl Convention {
    fn of_target() -> Convention {
        match super::Target::selected().map(|target| target.call_conv) {
            Ok(cranelift_codegen::isa::CallConv::WindowsFastcall) => Convention {
                arguments: [1, 2, 5],
                kept: &[3, 4, 9, 10, 11, 12],
                kept_vectors: 6..16,
            },
            _ => Convention { arguments: [4, 3, 2], kept: &[9, 10, 11, 12], kept_vectors: 0..0 },
        }
    }
}

const OPERANDS: usize = 13;
const HIGH: [&str; 3] = ["ah", "ch", "dh"];

fn named(name: &str) -> Result<Reg, String> {
    let name = name.trim_matches('"');
    if let Some(row) = GPR.iter().position(|row| row.contains(&name)) {
        if row >= OPERANDS {
            return Err(format!("inline assembly that uses `{name}`"));
        }
        return Ok(Reg::Gpr(row));
    }
    if let Some(row) = HIGH.iter().position(|&it| it == name) {
        return Ok(Reg::Gpr(row));
    }
    for prefix in ["xmm", "ymm", "zmm"] {
        if let Some(Ok(number)) = name.strip_prefix(prefix).map(str::parse::<usize>) {
            return Ok(Reg::Vector(number));
        }
    }
    if let Some(Ok(number @ 1..=7)) = name.strip_prefix('k').map(str::parse::<usize>) {
        return Ok(Reg::Mask(number));
    }
    let scratch = ["k", "mm", "st", "tmm"].iter().any(|prefix| {
        name.strip_prefix(prefix)
            .is_some_and(|rest| rest.trim_matches(['(', ')']).chars().all(|it| it.is_ascii_digit()))
    });
    if scratch { Ok(Reg::Scratch) } else { Err(format!("the register `{name}`")) }
}

fn vector_name(number: usize, size: u32) -> String {
    let prefix = match size {
        ..=16 => "xmm",
        32 => "ymm",
        _ => "zmm",
    };
    format!("{prefix}{number}")
}

/// The instruction that moves `size` bytes between `reg` and the slot, and the two as it
/// names them.
fn transfer(reg: Reg, size: u32, offset: u32) -> Result<(&'static str, String, String), String> {
    let width = match size {
        1 => "byte",
        2 => "word",
        4 => "dword",
        8 => "qword",
        16 => "xmmword",
        32 => "ymmword",
        64 => "zmmword",
        _ => return Err(format!("an inline assembly operand of {size} bytes")),
    };
    let memory = format!("{width} ptr [rbx + {offset}]");
    match reg {
        Reg::Gpr(row) if size <= 8 => {
            Ok(("mov", GPR[row][3 - size.trailing_zeros() as usize].to_owned(), memory))
        }
        Reg::Vector(number) if size >= 4 => {
            let instruction = match size {
                4 => "movd",
                8 => "movq",
                16 => "movups",
                _ => "vmovups",
            };
            Ok((instruction, vector_name(number, size), memory))
        }
        _ => Err(format!("an inline assembly operand of {size} bytes in such a register")),
    }
}

/// The instructions that move `size` bytes between mask register `k{number}` and the slot,
/// into the register when `load`. A value of 32 or 64 bits needs AVX512BW, as rustc's `kreg`
/// takes one only with it, and one of 16 bits AVX512F. A byte goes through `eax`, kept on
/// the stack meanwhile: `kmovb` would need AVX512DQ, which rustc's `kreg` does not ask for
/// a byte, and LLVM's `kmovw` from a register zero-extended to it fills the mask as this
/// does.
fn mask_transfer(number: usize, size: u32, offset: u32, load: bool) -> Result<String, String> {
    let memory = |width: &str| format!("{width} ptr [rbx + {offset}]");
    Ok(match (size, load) {
        (1, true) => {
            format!("push rax\nmovzx eax, {}\nkmovw k{number}, eax\npop rax", memory("byte"))
        }
        (1, false) => {
            format!("push rax\nkmovw eax, k{number}\nmov {}, al\npop rax", memory("byte"))
        }
        (2 | 4 | 8, _) => {
            let (instruction, width) = match size {
                2 => ("kmovw", "word"),
                4 => ("kmovd", "dword"),
                _ => ("kmovq", "qword"),
            };
            match load {
                true => format!("{instruction} k{number}, {}", memory(width)),
                false => format!("{instruction} {}, k{number}", memory(width)),
            }
        }
        _ => return Err(format!("an inline assembly operand of {size} bytes in a mask register")),
    })
}

/// The function for a block, in the assembly of the architecture.
pub(super) fn wrapper(
    target: &Target,
    template: &[AsmPiece],
    options: AsmOptions,
    operands: &[Operand<'_>],
) -> Result<Wrapper, String> {
    if options.contains(AsmOptions::MAY_UNWIND) {
        return Err("inline assembly that unwinds".to_owned());
    }
    let format = target.object_format;
    match target.arch {
        Arch::X86_64 => wrapper_x86_64(format, template, options, operands),
        Arch::Aarch64 => aarch64::wrapper(format, template, options, operands),
        Arch::Wasm32 => Err("inline assembly in a wasm module".to_owned()),
    }
}

/// The function for a block on x86-64.
fn wrapper_x86_64(
    format: ObjectFormat,
    template: &[AsmPiece],
    options: AsmOptions,
    operands: &[Operand<'_>],
) -> Result<Wrapper, String> {
    // The registers the operands name come first: a class takes one of what is left.
    let mut regs = vec![None; operands.len()];
    for (index, operand) in operands.iter().enumerate() {
        if let Operand::Reg { reg: InlineAsmRegOrRegClass::Reg(name), .. } = operand {
            regs[index] = Some(named(name.as_str())?);
        }
    }
    let mut classes = vec![""; operands.len()];
    for (index, operand) in operands.iter().enumerate() {
        let Operand::Reg { reg: InlineAsmRegOrRegClass::RegClass(class), .. } = operand else {
            continue;
        };
        let class = class.as_str();
        classes[index] = class;
        let free = |reg: Reg| !regs.contains(&Some(reg));
        let found = match class {
            "reg" | "reg_byte" => (0..OPERANDS).map(Reg::Gpr).find(|&it| free(it)),
            "reg_abcd" => (0..3).map(Reg::Gpr).find(|&it| free(it)),
            "xmm_reg" | "ymm_reg" | "zmm_reg" => (0..16).map(Reg::Vector).find(|&it| free(it)),
            "kreg" => (1..8).map(Reg::Mask).find(|&it| free(it)),
            _ => return Err(format!("the register class `{class}`")),
        };
        let Some(found) = found else {
            return Err(format!("inline assembly with more `{class}` operands than registers"));
        };
        regs[index] = Some(found);
    }

    let convention = Convention::of_target();
    let kept: Vec<usize> = convention
        .kept
        .iter()
        .copied()
        .filter(|&row| regs.contains(&Some(Reg::Gpr(row))))
        .collect();
    let kept_vectors: Vec<usize> =
        convention.kept_vectors.filter(|&it| regs.contains(&Some(Reg::Vector(it)))).collect();
    let kept_at = kept_vectors.len() * 16;
    let mut slot_size = (kept_at + kept.len() * 8) as u32;
    let mut offsets = vec![None; operands.len()];
    for (index, operand) in operands.iter().enumerate() {
        let Operand::Reg { size: Some(size), .. } = *operand else { continue };
        if !size.is_power_of_two() {
            return Err(format!("an inline assembly operand of {size} bytes"));
        }
        slot_size = slot_size.next_multiple_of(size.min(16));
        offsets[index] = Some(slot_size);
        slot_size += size;
    }
    let values = || {
        operands.iter().enumerate().filter_map(|(index, operand)| match *operand {
            Operand::Reg { size: Some(size), input, output, .. } => {
                Some((regs[index]?, size, offsets[index]?, input, output))
            }
            _ => None,
        })
    };

    let slot = GPR[convention.arguments[0]][0];
    let mut text = format!(".globl {SYMBOL}\n{SYMBOL}:\npush rbx\nmov rbx, {slot}\n");
    for (at, &row) in kept.iter().enumerate() {
        _ = writeln!(text, "mov qword ptr [rbx + {}], {}", kept_at + at * 8, GPR[row][0]);
    }
    for (at, &number) in kept_vectors.iter().enumerate() {
        _ = writeln!(text, "movups xmmword ptr [rbx + {}], xmm{number}", at * 16);
    }
    for (reg, size, offset, input, _) in values() {
        if let (true, Reg::Mask(number)) = (input, reg) {
            _ = writeln!(text, "{}", mask_transfer(number, size, offset, true)?);
        } else if input {
            let (instruction, reg, memory) = transfer(reg, size, offset)?;
            _ = writeln!(text, "{instruction} {reg}, {memory}");
        }
    }
    let att = options.contains(AsmOptions::ATT_SYNTAX);
    if att {
        text.push_str(".att_syntax\n");
    }
    for piece in template {
        let (index, modifier) = match piece {
            AsmPiece::Lit(lit) => {
                text.push_str(lit);
                continue;
            }
            AsmPiece::Operand { index, modifier } => (*index, *modifier),
        };
        let name = match (operands.get(index), regs.get(index).copied().flatten()) {
            (Some(Operand::Const(value)), _) => {
                _ = write!(text, "{value}");
                continue;
            }
            (Some(Operand::Label), _) => {
                text.push_str(&label(format, index));
                continue;
            }
            (_, Some(Reg::Gpr(row))) => {
                let modifier = modifier.or((classes[index] == "reg_byte").then_some('l'));
                match modifier {
                    None | Some('r') => GPR[row][0],
                    Some('e') => GPR[row][1],
                    Some('x') => GPR[row][2],
                    Some('l') => GPR[row][3],
                    Some('h') if row < HIGH.len() => HIGH[row],
                    Some(other) => return Err(format!("the operand modifier `{other}`")),
                }
                .to_owned()
            }
            (_, Some(Reg::Vector(number))) => {
                let size = match (modifier, classes[index]) {
                    (Some('x'), _) | (None, "xmm_reg") => 16,
                    (Some('y'), _) | (None, "ymm_reg") => 32,
                    (Some('z'), _) | (None, _) => 64,
                    (Some(other), _) => return Err(format!("the operand modifier `{other}`")),
                };
                vector_name(number, size)
            }
            (_, Some(Reg::Mask(number))) => match modifier {
                None => format!("k{number}"),
                Some(other) => return Err(format!("the operand modifier `{other}`")),
            },
            _ => return Err("an inline assembly template that names no operand".to_owned()),
        };
        if att {
            text.push('%');
        }
        text.push_str(&name);
    }
    text.push('\n');
    if att {
        text.push_str(".intel_syntax noprefix\n");
    }
    let returns = !options.contains(AsmOptions::NORETURN);
    if returns {
        for (reg, size, offset, _, output) in values() {
            if let (true, Reg::Mask(number)) = (output, reg) {
                _ = writeln!(text, "{}", mask_transfer(number, size, offset, false)?);
            } else if output {
                let (instruction, reg, memory) = transfer(reg, size, offset)?;
                _ = writeln!(text, "{instruction} {memory}, {reg}");
            }
        }
        if labels(operands).next().is_some() {
            text.push_str("xor eax, eax\n");
        }
    } else {
        text.push_str("ud2\n");
    }
    let mut jumped = false;
    for (at, index) in labels(operands).enumerate() {
        if returns || jumped {
            _ = writeln!(text, "jmp {}", done(format));
        }
        _ = writeln!(text, "{}:\nmov eax, {}", label(format, index), at + 1);
        jumped = true;
    }
    if returns || jumped {
        _ = writeln!(text, "{}:", done(format));
        for (at, &row) in kept.iter().enumerate() {
            _ = writeln!(text, "mov {}, qword ptr [rbx + {}]", GPR[row][0], kept_at + at * 8);
        }
        for (at, &number) in kept_vectors.iter().enumerate() {
            _ = writeln!(text, "movups xmm{number}, xmmword ptr [rbx + {}]", at * 16);
        }
        text.push_str("pop rbx\nret\n");
    }
    Ok(Wrapper { text, slot_size: slot_size.next_multiple_of(16).max(16), offsets })
}

/// A block as LLVM's inline assembly, which a compile LLVM lowers puts where the call of the
/// block's [`Wrapper`] is, as rustc gives LLVM the block: the inputs are read from the slot
/// the wrapper is passed, the outputs written back to it, at the wrapper's offsets.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Inline {
    /// The template, with each operand as LLVM numbers it.
    pub(super) text: String,
    pub(super) constraints: String,
    /// Where each output goes in the slot, `None` for one only overwritten, and its size, in
    /// the order of the constraints; then where each input comes from.
    pub(super) outputs: Vec<(Option<u32>, u32)>,
    pub(super) inputs: Vec<(u32, u32)>,
    pub(super) options: AsmOptions,
}

/// The blocks given to LLVM as inline assembly, by the text of their wrappers, which is what
/// the request of the call of one says. `None` where two blocks of one wrapper are not one
/// block.
static INLINE: std::sync::LazyLock<Mutex<FxHashMap<String, Option<Arc<Inline>>>>> =
    std::sync::LazyLock::new(Mutex::default);

/// Keeps `inline` as the block the wrapper of `text` is.
pub(super) fn keep_inline(text: &str, inline: Option<Inline>) {
    let mut known = INLINE.lock().unwrap_or_else(|e| e.into_inner());
    match known.get_mut(text) {
        Some(kept) => {
            if kept.as_deref() != inline.as_ref() {
                *kept = None;
            }
        }
        None => _ = known.insert(text.to_owned(), inline.map(Arc::new)),
    }
}

/// The block the wrapper of `text` is, as LLVM's inline assembly.
pub(super) fn inline_of(text: &str) -> Option<Arc<Inline>> {
    INLINE.lock().unwrap_or_else(|e| e.into_inner()).get(text).cloned().flatten()
}

/// The block as LLVM's inline assembly, where it is one LLVM takes as rustc's LLVM does:
/// operands in general registers, of a block that returns and does not unwind, has no labels
/// nor `sym`s, and asks for no ABI's registers. LLVM ends the process on assembly it rejects,
/// so a template that defines a label of a name, which is defined twice where LLVM copies the
/// block, is left to the wrapper too.
pub(super) fn inline(
    arch: Arch,
    template: &[AsmPiece],
    options: AsmOptions,
    operands: &[Operand<'_>],
    offsets: &[Option<u32>],
    clobber_abi: bool,
) -> Option<Inline> {
    static ASKED: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
        std::env::var_os("RA_JIT_INLINE_ASM").is_none_or(|it| it != "0")
    });
    let jumps = operands.iter().any(|it| matches!(it, Operand::Label | Operand::Sym));
    if !*ASKED
        || !matches!(arch, Arch::X86_64 | Arch::Aarch64)
        || jumps
        || clobber_abi
        || options.intersects(AsmOptions::NORETURN | AsmOptions::MAY_UNWIND)
    {
        return None;
    }
    let named_label = |line: &str| {
        let line = line.trim();
        line.split_once(':').is_some_and(|(name, _)| {
            !name.is_empty()
                && !name.bytes().all(|it| it.is_ascii_digit())
                && name.bytes().all(|it| it.is_ascii_alphanumeric() || b"_.$".contains(&it))
        })
    };
    // A block that reads the frame pointer expects the frame of the function it is in, which
    // LLVM may not have set up yet where the block is: a call of the wrapper makes it.
    let frame_pointer: &[&str] = match arch {
        Arch::X86_64 => &["rbp", "ebp", "bp"],
        _ => &["x29", "w29", "fp"],
    };
    let names_frame = |lit: &str| {
        lit.split(|it: char| !it.is_ascii_alphanumeric())
            .any(|word| frame_pointer.contains(&word.to_ascii_lowercase().as_str()))
    };
    for piece in template {
        if let AsmPiece::Lit(lit) = piece
            && (lit.split(['\n', ';']).any(named_label) || names_frame(lit))
        {
            return None;
        }
    }
    // An instruction that names a high byte register (`ah`, `{x:h}`) cannot encode one that
    // needs a REX prefix (`sil`, `r8b`), and LLVM's assembler refuses `mov ah, sil`. rustc's
    // `q` leaves it to the allocation: a block that gets `sil` fails to build, one that gets
    // `cl` builds. So in such a block a `reg_byte` is one of a, b, c and d (`Q`), which always
    // encodes; elsewhere any byte register (`q`), as rustc's.
    let high_byte = arch == Arch::X86_64
        && (template.iter().any(|piece| match piece {
            AsmPiece::Operand { modifier, .. } => *modifier == Some('h'),
            AsmPiece::Lit(lit) => lit
                .split(|it: char| !it.is_ascii_alphanumeric())
                .any(|word| ["ah", "bh", "ch", "dh"].contains(&word.to_ascii_lowercase().as_str())),
        }) || operands.iter().any(|operand| {
            matches!(operand, Operand::Reg { reg: InlineAsmRegOrRegClass::Reg(name), .. }
                if HIGH.contains(&name.as_str().trim_matches('"')))
        }));
    let constraint = |reg: &InlineAsmRegOrRegClass| -> Option<String> {
        match (arch, reg) {
            (Arch::X86_64, InlineAsmRegOrRegClass::Reg(name)) => {
                let name = name.as_str().trim_matches('"');
                match named(name).ok()? {
                    Reg::Gpr(row) if !HIGH.contains(&name) => {
                        Some(format!("{{{}}}", X86_LLVM[row]))
                    }
                    _ => None,
                }
            }
            (Arch::X86_64, InlineAsmRegOrRegClass::RegClass(class)) => match class.as_str() {
                "reg" => Some("r".to_owned()),
                "reg_abcd" => Some("Q".to_owned()),
                "reg_byte" if high_byte => Some("Q".to_owned()),
                "reg_byte" => Some("q".to_owned()),
                _ => None,
            },
            (Arch::Aarch64, InlineAsmRegOrRegClass::Reg(name)) => {
                aarch64::llvm_register(name.as_str())
            }
            (Arch::Aarch64, InlineAsmRegOrRegClass::RegClass(class)) => {
                (class.as_str() == "reg").then(|| "r".to_owned())
            }
            _ => None,
        }
    };
    let (mut outputs, mut inputs, mut clobbers) = (Vec::new(), Vec::new(), Vec::new());
    let (mut out_codes, mut in_codes) = (Vec::new(), Vec::new());
    // What LLVM numbers each operand that is a register: the outputs first.
    let mut numbers = vec![None; operands.len()];
    for (index, operand) in operands.iter().enumerate() {
        let Operand::Reg { reg, size, output, input, written } = *operand else { continue };
        let code = constraint(reg)?;
        let size = match size {
            Some(size @ (1 | 2 | 4 | 8)) => size,
            Some(_) => return None,
            None => 8,
        };
        match (input, output, written) {
            (_, true, _) => {
                numbers[index] = Some(out_codes.len());
                out_codes.push(format!("=&{code}"));
                outputs.push((Some(offsets[index]?), size));
            }
            (false, false, _) if matches!(reg, InlineAsmRegOrRegClass::Reg(_)) => {
                clobbers.push(format!("~{code}"));
            }
            // An output nothing reads: a register the block overwrites, an input's too.
            (false, false, _) | (true, false, true) => {
                numbers[index] = Some(out_codes.len());
                out_codes.push(format!("=&{code}"));
                outputs.push((None, size));
            }
            (true, false, false) => {}
        }
    }
    for (index, operand) in operands.iter().enumerate() {
        let Operand::Reg { reg, size: Some(size), input: true, output, written } = *operand else {
            continue;
        };
        let code = match (output || written, numbers[index]) {
            (true, Some(tied)) => tied.to_string(),
            _ => {
                numbers[index] = Some(out_codes.len() + in_codes.len());
                constraint(reg)?
            }
        };
        in_codes.push(code);
        inputs.push((offsets[index]?, size));
    }
    let mut text = String::new();
    for piece in template {
        let (index, modifier) = match piece {
            AsmPiece::Lit(lit) => {
                text.push_str(&lit.replace('$', "$$"));
                continue;
            }
            AsmPiece::Operand { index, modifier } => (*index, *modifier),
        };
        match operands.get(index)? {
            Operand::Const(value) => _ = write!(text, "{value}"),
            Operand::Reg { reg: InlineAsmRegOrRegClass::RegClass(class), .. } => {
                let modifier = match (arch, class.as_str(), modifier) {
                    (Arch::X86_64, "reg_byte", None) => None,
                    (Arch::X86_64, "reg" | "reg_abcd", None | Some('r')) => Some('q'),
                    (Arch::X86_64, "reg" | "reg_abcd", Some('e')) => Some('k'),
                    (Arch::X86_64, "reg" | "reg_abcd", Some('x')) => Some('w'),
                    (Arch::X86_64, "reg" | "reg_abcd", Some('l')) => Some('b'),
                    (Arch::X86_64, "reg_abcd", Some('h')) => Some('h'),
                    (Arch::Aarch64, "reg", None | Some('x')) => Some('x'),
                    (Arch::Aarch64, "reg", Some('w')) => Some('w'),
                    _ => return None,
                };
                let number = numbers[index]?;
                match modifier {
                    Some(modifier) => _ = write!(text, "${{{number}:{modifier}}}"),
                    None => _ = write!(text, "${{{number}}}"),
                }
            }
            _ => return None,
        }
    }
    if !options.contains(AsmOptions::PRESERVES_FLAGS) {
        match arch {
            Arch::X86_64 => {
                clobbers.extend(["~{dirflag}", "~{fpsr}", "~{flags}"].map(String::from))
            }
            _ => clobbers.push("~{cc}".to_owned()),
        }
    }
    if !options.contains(AsmOptions::NOMEM) {
        clobbers.push("~{memory}".to_owned());
    }
    let constraints = [out_codes, in_codes, clobbers].concat().join(",");
    Some(Inline { text, constraints, outputs, inputs, options })
}

/// How LLVM's inline assembly names each row of `GPR`.
const X86_LLVM: [&str; 13] =
    ["ax", "cx", "dx", "si", "di", "r8", "r9", "r10", "r11", "r12", "r13", "r14", "r15"];

/// The machine code of a function, and where in it the operands it names go.
#[derive(Default)]
pub(super) struct Assembled {
    pub(super) code: Vec<u8>,
    pub(super) relocs: Vec<Named>,
}

/// A place in the code that the address of an [`Operand::Sym`] goes.
pub(super) struct Named {
    pub(super) offset: u32,
    pub(super) kind: Reloc,
    /// The type ELF has for it, when Cranelift has no kind for that and `kind` stands in:
    /// see [`Code::elf_types`](super::fragment::Code).
    pub(super) elf_type: Option<u32>,
    /// The index of the operand.
    pub(super) operand: u32,
    pub(super) addend: i64,
}

/// The relocations a `sym` is assembled to, each with the number it is stored as.
const RELOCS: [Reloc; 9] = [
    Reloc::X86PCRel4,
    Reloc::X86CallPLTRel4,
    Reloc::X86GOTPCRel4,
    Reloc::Abs8,
    Reloc::Arm64Call,
    Reloc::Aarch64AdrPrelPgHi21,
    Reloc::Aarch64AddAbsLo12Nc,
    Reloc::Aarch64AdrGotPage21,
    Reloc::Aarch64Ld64GotLo12Nc,
];

impl Named {
    /// What Cranelift calls the relocation an assembler wrote for a `sym`, by the format
    /// of the object and the architecture, and the type ELF has for it where Cranelift has
    /// no kind that says as much.
    fn kind(
        format: BinaryFormat,
        arch: Architecture,
        flags: RelocationFlags,
    ) -> Option<(Reloc, Option<u32>)> {
        let kind = match (format, arch, flags) {
            (BinaryFormat::Elf, Architecture::X86_64, RelocationFlags::Elf { r_type }) => {
                match r_type {
                    elf::R_X86_64_PC32 => Reloc::X86PCRel4,
                    elf::R_X86_64_PLT32 => Reloc::X86CallPLTRel4,
                    // The forms a linker may relax are loads from the GOT like any other.
                    elf::R_X86_64_GOTPCREL
                    | elf::R_X86_64_GOTPCRELX
                    | elf::R_X86_64_REX_GOTPCRELX => Reloc::X86GOTPCRel4,
                    elf::R_X86_64_64 => Reloc::Abs8,
                    _ => return None,
                }
            }
            (BinaryFormat::Elf, Architecture::Aarch64, RelocationFlags::Elf { r_type }) => {
                match r_type {
                    // A jump out of the function is a call that does not come back.
                    elf::R_AARCH64_CALL26 | elf::R_AARCH64_JUMP26 => Reloc::Arm64Call,
                    elf::R_AARCH64_ADR_PREL_PG_HI21 => Reloc::Aarch64AdrPrelPgHi21,
                    elf::R_AARCH64_ADD_ABS_LO12_NC => Reloc::Aarch64AddAbsLo12Nc,
                    elf::R_AARCH64_ADR_GOT_PAGE => Reloc::Aarch64AdrGotPage21,
                    elf::R_AARCH64_LD64_GOT_LO12_NC => Reloc::Aarch64Ld64GotLo12Nc,
                    elf::R_AARCH64_ABS64 => Reloc::Abs8,
                    // The low bits of an address in a load or store go in in units of what
                    // it moves: the kind of an `add` stands in.
                    elf::R_AARCH64_LDST8_ABS_LO12_NC
                    | elf::R_AARCH64_LDST16_ABS_LO12_NC
                    | elf::R_AARCH64_LDST32_ABS_LO12_NC
                    | elf::R_AARCH64_LDST64_ABS_LO12_NC
                    | elf::R_AARCH64_LDST128_ABS_LO12_NC => {
                        return Some((Reloc::Aarch64AddAbsLo12Nc, Some(r_type)));
                    }
                    _ => return None,
                }
            }
            (BinaryFormat::Coff, Architecture::X86_64, RelocationFlags::Coff { typ }) => {
                match typ {
                    pe::IMAGE_REL_AMD64_ADDR64 => Reloc::Abs8,
                    // The numbered forms say how many bytes of the instruction follow the
                    // field, which `object` has put in the addend.
                    pe::IMAGE_REL_AMD64_REL32
                    | pe::IMAGE_REL_AMD64_REL32_1
                    | pe::IMAGE_REL_AMD64_REL32_2
                    | pe::IMAGE_REL_AMD64_REL32_3
                    | pe::IMAGE_REL_AMD64_REL32_4
                    | pe::IMAGE_REL_AMD64_REL32_5 => Reloc::X86PCRel4,
                    _ => return None,
                }
            }
            (BinaryFormat::MachO, Architecture::Aarch64, RelocationFlags::MachO { r_type, .. }) => {
                match r_type {
                    macho::ARM64_RELOC_BRANCH26 => Reloc::Arm64Call,
                    macho::ARM64_RELOC_PAGE21 => Reloc::Aarch64AdrPrelPgHi21,
                    macho::ARM64_RELOC_PAGEOFF12 => Reloc::Aarch64AddAbsLo12Nc,
                    macho::ARM64_RELOC_GOT_LOAD_PAGE21 => Reloc::Aarch64AdrGotPage21,
                    macho::ARM64_RELOC_GOT_LOAD_PAGEOFF12 => Reloc::Aarch64Ld64GotLo12Nc,
                    macho::ARM64_RELOC_UNSIGNED => Reloc::Abs8,
                    _ => return None,
                }
            }
            _ => return None,
        };
        Some((kind, None))
    }
}

impl Assembled {
    /// The bytes the store keeps.
    fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = (self.code.len() as u32).to_le_bytes().to_vec();
        bytes.extend_from_slice(&self.code);
        for named in &self.relocs {
            let kind = RELOCS.iter().position(|it| *it == named.kind).unwrap_or(RELOCS.len());
            bytes.extend_from_slice(&named.offset.to_le_bytes());
            bytes.push(kind as u8);
            bytes.extend_from_slice(&named.elf_type.unwrap_or(0).to_le_bytes());
            bytes.extend_from_slice(&named.operand.to_le_bytes());
            bytes.extend_from_slice(&named.addend.to_le_bytes());
        }
        bytes
    }

    fn from_bytes(bytes: &[u8]) -> Option<Assembled> {
        let (len, rest) = bytes.split_first_chunk()?;
        let (code, rest) = rest.split_at_checked(u32::from_le_bytes(*len) as usize)?;
        let (records, []) = rest.as_chunks::<21>() else { return None };
        let relocs = records.iter().map(|record| {
            let (offset, rest) = record.split_first_chunk()?;
            let (&kind, rest) = rest.split_first()?;
            let (elf_type, rest) = rest.split_first_chunk()?;
            let (operand, rest) = rest.split_first_chunk()?;
            Some(Named {
                offset: u32::from_le_bytes(*offset),
                kind: *RELOCS.get(usize::from(kind))?,
                // No relocation that is read has the type 0.
                elf_type: Some(u32::from_le_bytes(*elf_type)).filter(|it| *it != 0),
                operand: u32::from_le_bytes(*operand),
                addend: i64::from_le_bytes(rest.try_into().ok()?),
            })
        });
        Some(Assembled { code: code.to_vec(), relocs: relocs.collect::<Option<_>>()? })
    }
}

/// The machine code of the function that `text` defines. Each text is assembled once: the
/// store keeps the code, as it does a compiled function's.
pub(super) fn assemble(triple: &str, text: &str) -> Result<Arc<Assembled>, String> {
    static ASSEMBLED: OnceLock<Mutex<FxHashMap<[u8; 32], Arc<Assembled>>>> = OnceLock::new();
    let mut hasher = Sha256::new();
    hasher.update(b"ra-jit asm 3");
    hasher.update(triple);
    hasher.update([0]);
    hasher.update(text);
    let key: [u8; 32] = hasher.finalize().into();
    let assembled = ASSEMBLED.get_or_init(Mutex::default);
    // Also after a thread panicked with it: the map is whole between any two statements.
    let known =
        |key: &[u8; 32]| assembled.lock().unwrap_or_else(|e| e.into_inner()).get(key).cloned();
    if let Some(code) = known(&key) {
        return Ok(code);
    }
    let store = Store::global();
    let kept = store.and_then(|it| it.get(&key)).and_then(|it| Assembled::from_bytes(&it));
    let code = Arc::new(match kept {
        Some(code) => code,
        None => {
            let code = Assembler::selected().assemble(triple, text, &key)?;
            if let Some(store) = store {
                store.insert(&key, &code.to_bytes());
            }
            code
        }
    });
    assembled.lock().unwrap_or_else(|e| e.into_inner()).insert(key, code.clone());
    Ok(code)
}

/// What turns the text of a block into an object file. `RA_JIT_ASSEMBLER=cc` picks the C
/// compiler in a build with LLVM.
#[derive(Clone, Copy)]
enum Assembler {
    /// The platform's C compiler, run as a process: clang, or a `cc` for the host.
    Cc,
    /// LLVM's, in this process, when ra-jit is built with LLVM.
    #[cfg(feature = "llvm")]
    Llvm,
}

impl Assembler {
    fn selected() -> Assembler {
        if std::env::var("RA_JIT_ASSEMBLER").as_deref() == Ok("cc") {
            return Assembler::Cc;
        }
        #[cfg(feature = "llvm")]
        return Assembler::Llvm;
        #[cfg(not(feature = "llvm"))]
        Assembler::Cc
    }

    fn assemble(self, triple: &str, text: &str, key: &[u8; 32]) -> Result<Assembled, String> {
        match self {
            Assembler::Cc => run_cc(triple, text, key).unwrap_or_else(|| Err(no_assembler(triple))),
            #[cfg(feature = "llvm")]
            Assembler::Llvm => code(&super::llvm::assemble(text)?),
        }
    }
}

/// The text as rustc hands it to the assembler: `asm!` is in Intel's syntax on x86-64.
fn as_rustc_gives_it(triple: &str, text: &str) -> String {
    if triple.starts_with("x86_64") {
        format!(".intel_syntax noprefix\n{text}\n.att_syntax\n")
    } else {
        format!("{text}\n")
    }
}

/// The C compiler that assembles for `triple`, with the arguments that say the target:
/// clang for any target it knows, else `cc` for the host's alone. `None` when there is none.
fn c_compiler(triple: &str) -> Option<(PathBuf, Vec<String>)> {
    let paths = std::env::var_os("PATH")?;
    let find = |name: &str| {
        let name = format!("{name}{}", std::env::consts::EXE_SUFFIX);
        std::env::split_paths(&paths).map(|dir| dir.join(&name)).find(|it| it.is_file())
    };
    if let Some(clang) = find("clang") {
        return Some((clang, vec![format!("--target={triple}")]));
    }
    let host = triple.starts_with(std::env::consts::ARCH)
        && match std::env::consts::OS {
            "macos" => triple.contains("-apple-darwin"),
            "linux" => triple.contains("-linux-"),
            _ => false,
        };
    Some((find("cc").filter(|_| host)?, Vec::new()))
}

/// Why a block for `triple` is not assembled by a build of ra-jit without LLVM, and what to
/// install that would.
fn no_assembler(triple: &str) -> String {
    let host = triple.starts_with(std::env::consts::ARCH)
        && matches!(std::env::consts::OS, "macos" | "linux");
    let or_cc = if host { " or a C compiler (`cc`)" } else { "" };
    format!(
        "inline assembly: nothing here assembles it for {triple}. This ra-jit was built without \
         LLVM, so it assembles `asm!` with clang{or_cc}, and there is none on PATH: install \
         clang, or use a build of ra-jit with LLVM (`--features llvm`)"
    )
}

/// The block assembled by the platform's C compiler. `None` when there is none for `triple`.
fn run_cc(triple: &str, text: &str, key: &[u8; 32]) -> Option<Result<Assembled, String>> {
    let (compiler, target) = c_compiler(triple)?;
    static RUNS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let run = RUNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name: String =
        key[..8].iter().map(|it| format!("{it:02x}")).chain([format!("-{run}.o")]).collect();
    let dir = std::env::temp_dir().join(format!("ra-jit-asm-{}", std::process::id()));
    let failed = |what: &str, e: &dyn std::fmt::Display| format!("inline assembly: {what}: {e}");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return Some(Err(failed("no directory to assemble in", &e)));
    }
    let object = dir.join(name);
    #[allow(clippy::disallowed_methods)]
    let child = Command::new(&compiler)
        .args(&target)
        .args(["-c", "-x", "assembler", "-o"])
        .arg(&object)
        .arg("-")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn();
    let output = child.and_then(|mut child| {
        let mut stdin = child.stdin.take().ok_or(std::io::ErrorKind::BrokenPipe)?;
        std::io::Write::write_all(&mut stdin, as_rustc_gives_it(triple, text).as_bytes())?;
        drop(stdin);
        child.wait_with_output()
    });
    let said = compiler.display().to_string();
    let code = match output {
        Ok(output) if output.status.success() => {
            std::fs::read(&object).map_err(|e| failed("no object", &e)).and_then(|it| code(&it))
        }
        Ok(output) => Err(failed(&said, &String::from_utf8_lossy(&output.stderr).trim())),
        Err(e) => Err(failed(&format!("{said} did not run"), &e)),
    };
    _ = std::fs::remove_file(&object);
    Some(code)
}

/// The function in an object that defines it alone.
fn code(object: &[u8]) -> Result<Assembled, String> {
    let failed = |e: &dyn std::fmt::Display| format!("inline assembly: {e}");
    let file = object::File::parse(object).map_err(|e| failed(&e))?;
    let symbol = file
        .symbols()
        .find(|it| it.name().is_ok_and(|name| name.trim_start_matches('_') == SYMBOL))
        .ok_or_else(|| failed(&"the object lacks the function"))?;
    let section = symbol
        .section_index()
        .and_then(|it| file.section_by_index(it).ok())
        .ok_or_else(|| failed(&"the function is in no section"))?;
    let bytes = section.data().map_err(|e| failed(&e))?;
    let start = symbol.address() - section.address();
    let mut code = bytes[start as usize..].to_vec();
    let format = file.format();
    // FIXME: a block that names a symbol of its own, or has data in another section.
    let relocs = section.relocations().map(|(offset, relocation)| {
        let operand = match relocation.target() {
            RelocationTarget::Symbol(index) => file.symbol_by_index(index).ok(),
            _ => None,
        };
        // The name is as the text has it: an assembler adds nothing to one, on Mach-O either.
        let operand = operand
            .and_then(|it| it.name().ok()?.strip_prefix(SYM)?.parse().ok())
            .ok_or("inline assembly that names a symbol")?;
        let (kind, elf_type) = Named::kind(format, file.architecture(), relocation.flags())
            .ok_or("inline assembly that names a symbol in a way that is not read")?;
        let offset = offset.checked_sub(start).ok_or("inline assembly before its function")?;
        let mut addend = relocation.addend();
        // COFF keeps what is added in the field, and Mach-O does for an address that is data,
        // which whoever applies this is given apart. `object` says as much of every Mach-O
        // relocation that nothing is added to, where the field is an instruction.
        let in_field = match format {
            BinaryFormat::MachO => kind == Reloc::Abs8,
            _ => true,
        };
        if in_field && relocation.has_implicit_addend() {
            let len = if kind == Reloc::Abs8 { 8 } else { 4 };
            let field = code
                .get_mut(offset as usize..)
                .and_then(|it| it.get_mut(..len))
                .ok_or("inline assembly with a relocation outside it")?;
            let mut held = [0; 8];
            held[..len].copy_from_slice(field);
            field.fill(0);
            addend += match len {
                8 => i64::from_le_bytes(held),
                _ => i64::from(i32::from_le_bytes([held[0], held[1], held[2], held[3]])),
            };
        }
        Ok(Named { offset: offset as u32, kind, elf_type, operand, addend })
    });
    let relocs = relocs.collect::<Result<_, &str>>()?;
    Ok(Assembled { code, relocs })
}

#[cfg(test)]
mod tests {
    use super::{Arch, extensions};

    /// constant_time_eq's `#[target_feature(enable = "dit")]` reads DIT, which LLVM's
    /// assembler has no extension of: the architecture that has it comes before the
    /// extensions, which `.arch` would otherwise take back.
    #[test]
    fn a_feature_with_no_extension_is_taken_with_its_architecture() {
        let directives = extensions(Arch::Aarch64, ["sb", "dit", "crc", "jsconv"]);
        assert_eq!(directives, ".arch armv8.4-a\n.arch_extension crc\n.arch_extension sb\n");
        assert_eq!(extensions(Arch::Aarch64, ["v9.2a", "bti"]), ".arch armv9.2-a\n");
        assert_eq!(extensions(Arch::Aarch64, ["v9a", "ecv"]), ".arch armv9.1-a\n");
        assert_eq!(extensions(Arch::Aarch64, ["v9a", "dit"]), ".arch armv9-a\n");
        assert_eq!(extensions(Arch::Aarch64, ["neon"]), ".arch_extension simd\n");
        assert_eq!(extensions(Arch::X86_64, ["dit"]), "");
    }
}
