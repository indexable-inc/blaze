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

use super::{
    Arch, ObjectFormat, Target,
    avx512::{Load, Out},
    store::Store,
};

mod aarch64;

/// The name of the function in the text of a [`Wrapper`].
const SYMBOL: &str = "blaze_asm_block";

/// What the name of an [`Operand::Sym`] starts with.
const SYM: &str = "blaze_sym_";

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

/// AArch64's `fcvt` from the float in the first vector register to one of another width,
/// both named by their registers' letters (`h`, `s`, `d`): what LLVM converts an `f16` with
/// on a target without the half-precision instructions.
pub(super) fn fcvt(to: char, from: char) -> String {
    format!(".globl {SYMBOL}\n{SYMBOL}:\nfcvt {to}0, {from}0\nret\n")
}

/// An AArch64 instruction of the half-precision extension on the halves passed in `h0` up,
/// which leaves its result in `h0`: what LLVM does an `f16` with where the extension is enabled.
pub(super) fn half(instruction: &str) -> String {
    format!(".arch_extension fp16\n.globl {SYMBOL}\n{SYMBOL}:\n{instruction}\nret\n")
}

/// An x86-64 instruction of AVX512-FP16 on the halves passed in `xmm0` up, which leaves its
/// result in `xmm0`.
pub(super) fn x86_half(instruction: &str) -> String {
    format!(".globl {SYMBOL}\n{SYMBOL}:\n{instruction}\nret\n")
}

/// A function of the one AArch64 instruction whose word is `word`.
pub(super) fn instruction_word(word: u32) -> String {
    format!(".globl {SYMBOL}\n{SYMBOL}:\n.inst {word:#010x}\nret\n")
}

/// Writes a random number of AArch64's `RNDR` (or `RNDRRS`, `reseeded`) where its argument
/// points, and after it a byte that is 1 when there was none. The registers are named by
/// number, which needs no extension of the assembler.
pub(super) fn random(reseeded: bool) -> String {
    let register = if reseeded { "s3_3_c2_c4_1" } else { "s3_3_c2_c4_0" };
    format!(
        ".globl {SYMBOL}\n{SYMBOL}:\nmrs x9, {register}\ncset w10, eq\nstr x9, [x0]\n\
         strb w10, [x0, #8]\nret\n"
    )
}

/// AArch64's CRC-32 of its second argument into the checksum of its first, as `instruction`
/// (`crc32b` to `crc32cx`) says: the `x` ones read 64 bits.
pub(super) fn crc32(instruction: &str) -> String {
    let data = if instruction.ends_with('x') { "x1" } else { "w1" };
    format!(".arch_extension crc\n.globl {SYMBOL}\n{SYMBOL}:\n{instruction} w0, w0, {data}\nret\n")
}

/// x86's CRC-32C (SSE4.2's `crc32`) of its second argument, `bits` wide, into the checksum of
/// its first: LLVM's `x86.sse42.crc32.32.{8,16,32}` and `x86.sse42.crc32.64.64`.
pub(super) fn x86_crc32(bits: u32) -> String {
    let [crc, data, _] = Convention::of_target().arguments;
    let (into, crc, data) = match bits {
        64 => ("rax", GPR[crc][0], GPR[data][0]),
        32 => ("eax", GPR[crc][1], GPR[data][1]),
        16 => ("eax", GPR[crc][1], GPR[data][2]),
        _ => ("eax", GPR[crc][1], GPR[data][3]),
    };
    format!(".globl {SYMBOL}\n{SYMBOL}:\nmov {into}, {crc}\ncrc32 {into}, {data}\nret\n")
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
        "faminmax" => "faminmax",
        "fp8" => "fp8",
        _ => return None,
    })
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

/// A function of AArch64 instructions on SIMD registers, an intrinsic of LLVM's as
/// `neon::lower` has it. The first argument is where the results go and each other points at
/// a value: `args[i]` says how many bytes of argument `i + 1` are read into `v{i+1}`, or that
/// it is a pointer, read into `x{i+1}`. `text` runs after the reads, with `directives` (the
/// extensions it needs) before it, and leaves the results in `v0` up: `results` says their
/// sizes, and each is written after the one before it.
pub(super) fn neon_function(
    directives: &str,
    args: &[Option<u32>],
    text: &str,
    results: &[(u32, u32)],
) -> String {
    let mut out = format!("{directives}.globl {SYMBOL}\n{SYMBOL}:\n");
    let register = |bytes: u32| match bytes {
        1 => 'b',
        2 => 'h',
        4 => 's',
        8 => 'd',
        _ => 'q',
    };
    for (i, arg) in args.iter().enumerate() {
        let k = i + 1;
        match arg {
            Some(bytes) => _ = writeln!(out, "ldr {}{k}, [x{k}]", register(*bytes)),
            None => _ = writeln!(out, "ldr x{k}, [x{k}]"),
        }
    }
    _ = writeln!(out, "{text}");
    for (k, (offset, bytes)) in results.iter().enumerate() {
        _ = writeln!(out, "str {}{k}, [x0, #{offset}]", register(*bytes));
    }
    out.push_str("ret\n");
    out
}

/// The register x86 names `name` and its bytes: `rax` to `al`, `xmm`, `ymm` and `zmm`.
fn x86_register(name: &str) -> Option<(Reg, u32)> {
    if let Some(row) = GPR.iter().position(|row| row.contains(&name)) {
        let column = GPR[row].iter().position(|&it| it == name)?;
        return Some((Reg::Gpr(row), 8 >> column));
    }
    [("xmm", 16), ("ymm", 32), ("zmm", 64)].into_iter().find_map(|(prefix, bytes)| {
        let number = name.strip_prefix(prefix)?.parse().ok()?;
        Some((Reg::Vector(number), bytes))
    })
}

/// The bytes of the x86 register `name`.
pub(super) fn x86_register_bytes(name: &str) -> Option<u32> {
    x86_register(name).map(|(_, bytes)| bytes)
}

/// The instruction that moves the register `name` to or from `memory`.
fn x86_move(name: &str, memory: &str, load: bool) -> String {
    let (mnemonic, width) = match x86_register(name).expect("a register") {
        (Reg::Gpr(_), 1) => ("mov", "byte"),
        (Reg::Gpr(_), 2) => ("mov", "word"),
        (Reg::Gpr(_), 4) => ("mov", "dword"),
        (Reg::Gpr(_), _) => ("mov", "qword"),
        (_, 16) => ("movups", "xmmword"),
        (_, 32) => ("vmovups", "ymmword"),
        (_, _) => ("vmovups", "zmmword"),
    };
    match load {
        true => format!("{mnemonic} {name}, {width} ptr [{memory}]\n"),
        false => format!("{mnemonic} {width} ptr [{memory}], {name}\n"),
    }
}

/// A function of x86 instructions on registers, an intrinsic of LLVM's as `x86::lower` has
/// it. The first argument points at the arguments and the second at where the results go:
/// `args` names the register each argument is read into and where it is, `results` the
/// register each result is written from and where. `clobbers` are the other registers `text`
/// writes. Each of these registers that the calling convention keeps is kept on the stack,
/// and the upper halves of the vector registers are zeroed once a 256- or 512-bit one is
/// used, as LLVM does before a function returns.
pub(super) fn x86_function(
    text: &str,
    args: &[(&str, u32)],
    results: &[(&str, u32)],
    clobbers: &[&str],
) -> Result<String, String> {
    let convention = Convention::of_target();
    let [from, to, _] = convention.arguments.map(|row| GPR[row][0]);
    let mut out = format!(".globl {SYMBOL}\n{SYMBOL}:\nmov r10, {from}\nmov r11, {to}\n");
    let names = args.iter().chain(results).map(|(name, _)| *name).chain(clobbers.iter().copied());
    let mut kept = Vec::new();
    let mut vectors = Vec::new();
    let mut wide = text.contains("ymm") || text.contains("zmm");
    for name in names {
        let Some((reg, bytes)) = x86_register(name) else {
            return Err(format!("the register `{name}`"));
        };
        wide |= bytes > 16;
        match reg {
            Reg::Gpr(row) if convention.kept.contains(&row) && !kept.contains(&row) => {
                kept.push(row)
            }
            Reg::Vector(number)
                if convention.kept_vectors.contains(&number) && !vectors.contains(&number) =>
            {
                vectors.push(number)
            }
            _ => {}
        }
    }
    for &row in &kept {
        _ = writeln!(out, "push {}", GPR[row][0]);
    }
    if !vectors.is_empty() {
        _ = writeln!(out, "sub rsp, {}", vectors.len() * 16);
    }
    for (i, number) in vectors.iter().enumerate() {
        _ = writeln!(out, "movups xmmword ptr [rsp + {}], xmm{number}", i * 16);
    }
    for (name, offset) in args {
        out.push_str(&x86_move(name, &format!("r10 + {offset}"), true));
    }
    _ = writeln!(out, "{text}");
    for (name, offset) in results {
        out.push_str(&x86_move(name, &format!("r11 + {offset}"), false));
    }
    for (i, number) in vectors.iter().enumerate() {
        _ = writeln!(out, "movups xmm{number}, xmmword ptr [rsp + {}]", i * 16);
    }
    if !vectors.is_empty() {
        _ = writeln!(out, "add rsp, {}", vectors.len() * 16);
    }
    for &row in kept.iter().rev() {
        _ = writeln!(out, "pop {}", GPR[row][0]);
    }
    if wide {
        out.push_str("vzeroupper\n");
    }
    out.push_str("ret\n");
    Ok(out)
}

/// A function of x86 instructions on vector and mask registers, an intrinsic of LLVM's as
/// `avx512::lower` has it. Its argument points at the address of the result and after it the
/// address of each operand: `loads[i]` says what of operand `i` is read, and into which
/// register. `text` runs after the reads and leaves the result where `out` says, which is
/// written to the first address. A mask of 8 bits is read with AVX512F's `kmovw`, as LLVM
/// moves one without AVX512DQ.
pub(super) fn avx512_function(loads: &[Load], text: &str, out: Out) -> String {
    let slot = GPR[Convention::of_target().arguments[0]][0];
    let mut s = format!(".globl {SYMBOL}\n{SYMBOL}:\nmov r10, {slot}\n");
    for (i, load) in loads.iter().enumerate() {
        let (k, at) = (i + 1, 8 * (i + 1));
        match *load {
            Load::Immediate => {}
            Load::Pointer => {
                _ = writeln!(s, "mov r11, qword ptr [r10 + {at}]\nmov r11, qword ptr [r11]")
            }
            Load::Int(bytes) => {
                let (to, width) = if bytes == 8 { ("rcx", "qword") } else { ("ecx", "dword") };
                _ = writeln!(s, "mov rax, qword ptr [r10 + {at}]\nmov {to}, {width} ptr [rax]");
            }
            Load::Vector(8) => {
                let name = vector_name(k, 16);
                _ = writeln!(s, "mov rax, qword ptr [r10 + {at}]\nvmovq {name}, qword ptr [rax]");
            }
            Load::Vector(4) => {
                let name = vector_name(k, 16);
                _ = writeln!(s, "mov rax, qword ptr [r10 + {at}]\nvmovd {name}, dword ptr [rax]");
            }
            Load::Vector(2) => {
                let name = vector_name(k, 16);
                _ = writeln!(
                    s,
                    "mov rax, qword ptr [r10 + {at}]\nmovzx eax, word ptr [rax]\nvmovd {name}, eax"
                );
            }
            Load::Vector(bytes) => {
                let name = vector_name(k, bytes);
                let width = vector_width(bytes);
                _ = writeln!(
                    s,
                    "mov rax, qword ptr [r10 + {at}]\nvmovups {name}, {width} ptr [rax]"
                );
            }
            Load::Mask(bytes) => {
                _ = writeln!(s, "mov rax, qword ptr [r10 + {at}]");
                _ = match bytes {
                    1 => writeln!(s, "movzx eax, byte ptr [rax]\nkmovw k{k}, eax"),
                    2 => writeln!(s, "kmovw k{k}, word ptr [rax]"),
                    4 => writeln!(s, "kmovd k{k}, dword ptr [rax]"),
                    _ => writeln!(s, "kmovq k{k}, qword ptr [rax]"),
                };
            }
        }
    }
    _ = writeln!(s, "{text}\nmov r10, qword ptr [r10]");
    let mask = |s: &mut String, k: u32, bytes: u32, at: u32| {
        _ = match bytes {
            1 => writeln!(s, "kmovw eax, k{k}\nmov byte ptr [r10 + {at}], al"),
            2 => writeln!(s, "kmovw word ptr [r10 + {at}], k{k}"),
            4 => writeln!(s, "kmovd dword ptr [r10 + {at}], k{k}"),
            _ => writeln!(s, "kmovq qword ptr [r10 + {at}], k{k}"),
        };
    };
    match out {
        Out::Nothing => {}
        Out::Vector(8) => _ = writeln!(s, "vmovq qword ptr [r10], xmm0"),
        Out::Vector(4) => _ = writeln!(s, "vmovd dword ptr [r10], xmm0"),
        Out::Vector(2) => _ = writeln!(s, "vmovd eax, xmm0\nmov word ptr [r10], ax"),
        Out::Vector(bytes) => {
            let (name, width) = (vector_name(0, bytes), vector_width(bytes));
            _ = writeln!(s, "vmovups {width} ptr [r10], {name}");
        }
        Out::Mask(bytes) => mask(&mut s, 0, bytes, 0),
        Out::Int(8) => _ = writeln!(s, "mov qword ptr [r10], rax"),
        Out::Int(_) => _ = writeln!(s, "mov dword ptr [r10], eax"),
        Out::Masks(bytes, at) => {
            mask(&mut s, 0, bytes, 0);
            mask(&mut s, 1, bytes, at);
        }
    }
    s.push_str("vzeroupper\nret\n");
    s
}

/// What Intel's syntax calls a memory operand of a vector of `bytes`.
fn vector_width(bytes: u32) -> &'static str {
    match bytes {
        ..=16 => "xmmword",
        32 => "ymmword",
        _ => "zmmword",
    }
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
    format!("{}blaze_label_{index}", format.local_label())
}

/// The name in a [`Wrapper`] of where every way out of the block meets.
fn done(format: ObjectFormat) -> String {
    format!("{}blaze_done", format.local_label())
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
    /// `k1` to `k7`, AVX-512's masks.
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
    if let Some(number @ 1..8) = name.strip_prefix('k').and_then(|it| it.parse().ok()) {
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

/// The instructions that move a mask of `size` bytes between `k{number}` and the slot (`load`)
/// or back. AVX512F moves 16 bits; a byte goes through `eax`, as LLVM moves one without
/// AVX512DQ, and 32 and 64 bits need AVX512BW, which LLVM needs for a mask of them too.
fn mask_transfer(load: bool, number: usize, size: u32, offset: u32) -> Result<String, String> {
    let memory = |width: &str| format!("{width} ptr [rbx + {offset}]");
    Ok(match (load, size) {
        (true, 1) => format!("movzx eax, {}\nkmovw k{number}, eax", memory("byte")),
        (false, 1) => format!("kmovw eax, k{number}\nmov {}, al", memory("byte")),
        (true, 2) => format!("kmovw k{number}, {}", memory("word")),
        (true, 4) => format!("kmovd k{number}, {}", memory("dword")),
        (true, 8) => format!("kmovq k{number}, {}", memory("qword")),
        (false, 2) => format!("kmovw {}, k{number}", memory("word")),
        (false, 4) => format!("kmovd {}, k{number}", memory("dword")),
        (false, 8) => format!("kmovq {}, k{number}", memory("qword")),
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
    // A mask of one byte goes through `eax`, before any operand is there.
    for (reg, size, offset, input, _) in values() {
        if let (Reg::Mask(number), true) = (reg, input) {
            _ = writeln!(text, "{}", mask_transfer(true, number, size, offset)?);
        }
    }
    for (reg, size, offset, input, _) in values() {
        if input && !matches!(reg, Reg::Mask(_)) {
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
            // rustc hands LLVM each `$` as `$$`, which LLVM prints as `$` in AT&T syntax and as
            // nothing in Intel's: Windows' `abort_internal` is `int $$0x29`, assembled `int 41`.
            AsmPiece::Lit(lit) if att => {
                text.push_str(lit);
                continue;
            }
            AsmPiece::Lit(lit) => {
                text.extend(lit.chars().filter(|&it| it != '$'));
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
            (_, Some(Reg::Mask(number))) if modifier.is_none() => format!("k{number}"),
            (_, Some(Reg::Vector(number))) => {
                let size = match (modifier, classes[index]) {
                    (Some('x'), _) | (None, "xmm_reg") => 16,
                    (Some('y'), _) | (None, "ymm_reg") => 32,
                    (Some('z'), _) | (None, _) => 64,
                    (Some(other), _) => return Err(format!("the operand modifier `{other}`")),
                };
                vector_name(number, size)
            }
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
            if output && !matches!(reg, Reg::Mask(_)) {
                let (instruction, reg, memory) = transfer(reg, size, offset)?;
                _ = writeln!(text, "{instruction} {memory}, {reg}");
            }
        }
        // After every other output is written, as one of a byte goes through `eax`.
        for (reg, size, offset, _, output) in values() {
            if let (Reg::Mask(number), true) = (reg, output) {
                _ = writeln!(text, "{}", mask_transfer(false, number, size, offset)?);
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
        std::env::var_os("BLAZE_INLINE_ASM").is_none_or(|it| it != "0")
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
pub(super) fn assemble(
    assembler: &Assembler,
    triple: &str,
    text: &str,
) -> Result<Arc<Assembled>, String> {
    static ASSEMBLED: OnceLock<Mutex<FxHashMap<[u8; 32], Arc<Assembled>>>> = OnceLock::new();
    let mut hasher = Sha256::new();
    hasher.update(b"blaze asm 4");
    hasher.update(format!("{assembler:?}"));
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
            let code = assembler.assemble(triple, text, &key)?;
            if let Some(store) = store {
                store.insert(&key, &code.to_bytes());
            }
            code
        }
    });
    assembled.lock().unwrap_or_else(|e| e.into_inner()).insert(key, code.clone());
    Ok(code)
}

/// What turns the text of a block into an object file. `BLAZE_ASSEMBLER=cc` picks the C
/// compiler in a build with LLVM.
#[derive(Clone, Debug)]
pub(super) enum Assembler {
    /// The platform's C compiler, run as a process: clang, or a `cc` for the host.
    Cc(Option<String>),
    /// LLVM's, in this process, when blaze is built with LLVM.
    #[cfg(feature = "llvm")]
    Llvm,
}

impl Assembler {
    pub(super) fn for_build(db: &dyn hir_ty::db::HirDatabase) -> Assembler {
        #[cfg(feature = "llvm")]
        if matches!(Self::selected(None), Self::Llvm) {
            return Self::Llvm;
        }
        let path = match base_db::ProcessEnv::try_get(db) {
            Some(env) => env.vars(db).get("PATH"),
            None => std::env::var("PATH").ok(),
        };
        Self::Cc(path)
    }

    pub(super) fn path(&self) -> Option<Option<String>> {
        match self {
            Self::Cc(path) => Some(path.clone()),
            #[cfg(feature = "llvm")]
            Self::Llvm => None,
        }
    }

    pub(super) fn selected(path: Option<String>) -> Assembler {
        if std::env::var("BLAZE_ASSEMBLER").as_deref() == Ok("cc") {
            return Assembler::Cc(path);
        }
        #[cfg(feature = "llvm")]
        return Assembler::Llvm;
        #[cfg(not(feature = "llvm"))]
        Assembler::Cc(path)
    }

    fn assemble(&self, triple: &str, text: &str, key: &[u8; 32]) -> Result<Assembled, String> {
        match self {
            Assembler::Cc(path) => run_cc(path.as_deref(), triple, text, key)
                .unwrap_or_else(|| Err(no_assembler(triple))),
            #[cfg(feature = "llvm")]
            Assembler::Llvm => code(&super::llvm::assemble(text)?),
        }
    }
}

/// The text as rustc hands it to the assembler: `asm!` is in Intel's syntax on x86-64.
fn as_rustc_gives_it(triple: &str, text: &str) -> String {
    if triple.starts_with("x86_64") {
        let text: Vec<String> = text.lines().map(hash_comment).collect();
        format!(".intel_syntax noprefix\n{}\n.att_syntax\n", text.join("\n"))
    } else {
        format!("{text}\n")
    }
}

/// The line with a comment of `//`, which LLVM's assembler takes on every target and GNU's
/// on x86 only at the start of a line, as the `#` both take. A `//` in a string or a
/// character (`'c'`, `'\c'`) is not one, as LLVM's lexer reads them; a line with a `'`
/// before it that is not a character is left as it is.
fn hash_comment(line: &str) -> String {
    let mut chars = line.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        match c {
            '"' => loop {
                match chars.next() {
                    Some((_, '\\')) => _ = chars.next(),
                    Some((_, '"')) => break,
                    Some(_) => {}
                    None => return line.to_owned(),
                }
            },
            '\'' => {
                if let Some((_, '\\')) = chars.next() {
                    chars.next();
                }
                if !matches!(chars.next(), Some((_, '\''))) {
                    return line.to_owned();
                }
            }
            '/' if matches!(chars.peek(), Some((_, '/'))) => {
                return format!("{}#{}", &line[..at], &line[at + 2..]);
            }
            _ => {}
        }
    }
    line.to_owned()
}

/// The C compiler that assembles for `triple`, with the arguments that say the target:
/// clang for any target it knows, else `cc` for the host's alone. `None` when there is none.
fn c_compiler(paths: Option<&str>, triple: &str) -> Option<(PathBuf, Vec<String>)> {
    let paths = paths?;
    let find = |name: &str| {
        let name = format!("{name}{}", std::env::consts::EXE_SUFFIX);
        std::env::split_paths(paths).map(|dir| dir.join(&name)).find(|it| it.is_file())
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

/// Why a block for `triple` is not assembled by a build of blaze without LLVM, and what to
/// install that would.
fn no_assembler(triple: &str) -> String {
    let host = triple.starts_with(std::env::consts::ARCH)
        && matches!(std::env::consts::OS, "macos" | "linux");
    let or_cc = if host { " or a C compiler (`cc`)" } else { "" };
    format!(
        "inline assembly: nothing here assembles it for {triple}. This blaze was built without \
         LLVM, so it assembles `asm!` with clang{or_cc}, and there is none on PATH: install \
         clang, or use a build of blaze with LLVM (`--features llvm`)"
    )
}

/// The block assembled by the platform's C compiler. `None` when there is none for `triple`.
fn run_cc(
    path: Option<&str>,
    triple: &str,
    text: &str,
    key: &[u8; 32],
) -> Option<Result<Assembled, String>> {
    let (compiler, target) = c_compiler(path, triple)?;
    static RUNS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let run = RUNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name: String =
        key[..8].iter().map(|it| format!("{it:02x}")).chain([format!("-{run}.o")]).collect();
    let dir = std::env::temp_dir().join(format!("blaze-asm-{}", std::process::id()));
    let failed = |what: &str, e: &dyn std::fmt::Display| format!("inline assembly: {what}: {e}");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return Some(Err(failed("no directory to assemble in", &e)));
    }
    let object = dir.join(name);
    #[allow(clippy::disallowed_methods)]
    let mut command = Command::new(&compiler);
    match path {
        Some(path) => command.env("PATH", path),
        None => command.env_remove("PATH"),
    };
    let child = command
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
    use super::{Arch, extensions, hash_comment};

    /// compiler_builtins' `__rust_probestack` ends an instruction with `// ...`.
    #[test]
    fn a_comment_of_two_slashes_is_one_gnu_as_reads() {
        assert_eq!(hash_comment("mov r11, rax // dup"), "mov r11, rax # dup");
        assert_eq!(hash_comment(".ascii \"a//b\\\"//\" // c"), ".ascii \"a//b\\\"//\" # c");
        assert_eq!(hash_comment("// we're guaranteed"), "# we're guaranteed");
        assert_eq!(hash_comment("mov al, '/' // c'"), "mov al, '/' # c'");
        assert_eq!(hash_comment("mov al, '\\'' // c"), "mov al, '\\'' # c");
        assert_eq!(hash_comment("mov al, 'ab' // c"), "mov al, 'ab' // c");
        assert_eq!(hash_comment("div rcx"), "div rcx");
    }

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

    /// What rustc 1.99's LLVM assembled of these: `int 41` and `mov eax, 5`, and in AT&T
    /// syntax the template as it is written.
    #[test]
    fn a_dollar_is_nothing_in_intel_syntax() {
        use super::{AsmOptions, AsmPiece, ObjectFormat, wrapper_x86_64};
        let text = |template: &str, options| {
            let template = [AsmPiece::Lit(template.into())];
            wrapper_x86_64(ObjectFormat::Coff, &template, options, &[]).unwrap().text
        };
        let intel = text("int $$0x29\nmov eax, $$$$5", AsmOptions::NORETURN);
        assert!(intel.contains("int 0x29\nmov eax, 5\n"), "{intel}");
        let att = text("movl $$7, %eax", AsmOptions::NORETURN | AsmOptions::ATT_SYNTAX);
        assert!(att.contains("movl $$7, %eax\n"), "{att}");
    }
}
