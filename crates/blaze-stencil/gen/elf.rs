//! Reading the object LLVM made for one variant: each stencil's bytes, its holes (the
//! relocations LLVM left, as `(at, form, kind, addend)`), its `tail`, and its constants moved
//! into the library's rodata. Every check that refuses a stencil is here, and each refusal is
//! a panic naming the stencil, which fails blaze's build:
//!
//! - an instruction that touches the stack or a reserved register (`rsp`, `rbp`; `sp`, `x18`,
//!   `x29`, `x30`), or calls or returns: stencils are leaf and stackless, so this is also the
//!   stack-size check (a frame needs `rsp`/`sp`);
//! - a relocation of a type no [`Form`] is, against anything but a hole or a constant of
//!   LLVM's constant pool, a hole out of its stencil or of a kind its shape has no use for;
//! - a constant pool LLVM put relocations in (the patcher copies it verbatim).

use std::{cell::RefCell, collections::HashMap};

use object::{
    Object, ObjectSection, ObjectSymbol, RelocationFlags, RelocationTarget, SectionIndex,
    SymbolSection, elf,
};

use crate::{
    enc::Form,
    format::{self, ENTRY, Hole, HoleKind, Writer},
    generate::llvm::Disasm,
    shape::{Disp, Dst, Isa, Op, Shape, Src, pinned},
};

/// One stencil as the library holds it.
pub struct Stencil {
    pub code: Vec<u8>,
    pub tail: u8,
    pub holes: Vec<Hole>,
    /// Where each access of the op's memory starts (`format`'s "Faults").
    pub faults: Vec<u16>,
}

/// One instruction of a stencil: where it starts, its length and LLVM's text of it.
struct Inst {
    at: usize,
    len: usize,
    text: String,
}

/// The object of one variant, indexed once by section name.
pub struct Elf<'a> {
    file: object::File<'a>,
    isa: Isa,
    sections: HashMap<&'a str, SectionIndex>,
    /// Each defined symbol's section, address and size, by name.
    symbols: HashMap<&'a str, (SymbolSection, u64, u64)>,
    /// The constant-pool sections (`.rodata.cst4`, `8`, `16`) and their bytes.
    pools: HashMap<SectionIndex, &'a [u8]>,
    /// Every refused instruction so far (stencil, instruction, offset), reported together by
    /// [`Elf::refusals`] so one run names them all.
    refusals: RefCell<Vec<String>>,
}

impl<'a> Elf<'a> {
    pub fn new(bytes: &'a [u8], isa: Isa, variant: &str) -> Elf<'a> {
        let file = object::File::parse(bytes)
            .unwrap_or_else(|e| panic!("the {variant} stencils' object does not parse: {e}"));
        let mut sections = HashMap::new();
        let mut pools = HashMap::new();
        for section in file.sections() {
            let Ok(name) = section.name() else { continue };
            if name.starts_with(".rodata") {
                let pool = matches!(name, ".rodata.cst4" | ".rodata.cst8" | ".rodata.cst16");
                assert!(
                    pool && section.align() <= ENTRY as u64,
                    "the {variant} stencils have read-only data in `{name}`, which is no \
                     constant pool of entries of at most {ENTRY} bytes"
                );
                assert!(
                    section.relocations().next().is_none(),
                    "the {variant} stencils' constant pool `{name}` has relocations, and the \
                     patcher copies it as it is"
                );
                let data = section
                    .data()
                    .unwrap_or_else(|e| panic!("the {variant} stencils' `{name}`: {e}"));
                pools.insert(section.index(), data);
            }
            sections.insert(name, section.index());
        }
        let mut symbols = HashMap::new();
        for symbol in file.symbols() {
            if let Ok(name) = symbol.name()
                && symbol.is_definition()
            {
                symbols.insert(name, (symbol.section(), symbol.address(), symbol.size()));
            }
        }
        Elf { file, isa, sections, symbols, pools, refusals: RefCell::new(Vec::new()) }
    }

    /// The function `name` (`what` in messages): its bytes, holes and tail, its constants
    /// added to `writer`'s rodata.
    pub fn stencil(
        &self,
        name: &str,
        what: &str,
        shape: Option<Shape>,
        disasm: &Disasm,
        writer: &mut Writer,
    ) -> Stencil {
        let section_name = format!(".text.{name}");
        let index = *self
            .sections
            .get(section_name.as_str())
            .unwrap_or_else(|| panic!("stencil `{what}`: LLVM made no section `{section_name}`"));
        let section = self.file.section_by_index(index).expect("an indexed section");
        let code = section.data().unwrap_or_else(|e| panic!("stencil `{what}`: {e}")).to_vec();
        self.check_symbol(name, what, index, code.len());
        let insts = self.disassemble(what, &code, disasm);

        let mut holes = Vec::new();
        for (at, reloc) in section.relocations() {
            let RelocationFlags::Elf { r_type } = reloc.flags() else {
                panic!("stencil `{what}`: a relocation that is not ELF's")
            };
            let form = form(self.isa, r_type).unwrap_or_else(|| {
                panic!("stencil `{what}`: relocation type {} at {at:#x} is no hole form", r_type.0)
            });
            let at = usize::try_from(at).expect("an offset in memory");
            if at + usize::from(form.bytes()) > code.len() {
                panic!("stencil `{what}`: a {form:?} hole at {at:#x} is out of its {} bytes", code.len());
            }
            let place = u16::try_from(at)
                .unwrap_or_else(|_| panic!("stencil `{what}`: a hole at {at:#x}, past 64 KB"));
            let addend = reloc.addend();
            let symbol = match reloc.target() {
                RelocationTarget::Symbol(symbol) => self
                    .file
                    .symbol_by_index(symbol)
                    .unwrap_or_else(|e| panic!("stencil `{what}`: {e}")),
                target => panic!("stencil `{what}`: a relocation against {target:?}"),
            };
            let hole = match symbol.section() {
                SymbolSection::Undefined => {
                    let symbol_name = symbol.name().unwrap_or("");
                    let kind = HoleKind::from_symbol(symbol_name).unwrap_or_else(|| {
                        panic!("stencil `{what}`: a relocation against `{symbol_name}`, which is no hole")
                    });
                    let addend = i32::try_from(addend).unwrap_or_else(|_| {
                        panic!("stencil `{what}`: the {kind:?} hole's addend {addend} is past 32 bits")
                    });
                    Hole { at: place, form, kind, addend }
                }
                SymbolSection::Section(pool) if self.pools.contains_key(&pool) => {
                    let end = match self.isa {
                        // x86-64's `P` is the field: the target is past the instruction's end.
                        Isa::X86_64 => {
                            let inst = insts
                                .iter()
                                .find(|it| it.at <= at && at < it.at + it.len)
                                .unwrap_or_else(|| panic!("stencil `{what}`: a hole at {at:#x} in no instruction"));
                            (inst.at + inst.len - at) as i64
                        }
                        // AArch64's `P` is the instruction, which the hole's `at` is.
                        Isa::Aarch64 => 0,
                    };
                    let target = symbol.address() as i64 + addend + end;
                    let bytes = self.pools[&pool];
                    if target < 0 || target as usize >= bytes.len() {
                        panic!("stencil `{what}`: a constant at {target:#x}, out of its pool");
                    }
                    let start = target as usize & !(ENTRY - 1);
                    let entry = writer
                        .rodata(&bytes[start..bytes.len().min(start + ENTRY)], ENTRY as u32)
                        .unwrap_or_else(|e| panic!("stencil `{what}`: {e}"));
                    let adjust = target - start as i64 - end;
                    let addend = Hole::rodata_addend(entry, adjust).unwrap_or_else(|| {
                        panic!("stencil `{what}`: rodata entry {entry} with addend {adjust} does not fit a hole")
                    });
                    Hole { at: place, form, kind: HoleKind::Rodata, addend }
                }
                _ => panic!(
                    "stencil `{what}`: a relocation against `{}`, which is neither a hole nor a \
                     constant",
                    symbol.name().unwrap_or("")
                ),
            };
            if !format::fits(hole.kind, hole.form) {
                panic!("stencil `{what}`: a {:?} hole in a {form:?} field", hole.kind);
            }
            if let Some(shape) = shape
                && !allowed(shape, hole.kind)
            {
                panic!("stencil `{what}`: a {:?} hole, which its shape has no use for", hole.kind);
            }
            holes.push(hole);
        }
        holes.sort_by_key(|it| (it.at, it.kind as u8));
        if let Some(shape) = shape {
            for kind in required(shape) {
                if !holes.iter().any(|it| it.kind == kind) {
                    panic!("stencil `{what}`: no {kind:?} hole, which its shape writes");
                }
            }
        }
        let tail = self.tail(&code, &insts, &holes);
        let faults = match shape {
            Some(shape) if accesses_memory(shape.op) => {
                let faults: Vec<u16> =
                    insts.iter().filter(|it| user_access(self.isa, &it.text)).map(|it| it.at as u16).collect();
                if faults.is_empty() {
                    panic!("stencil `{what}`: no instruction of it accesses the op's memory");
                }
                faults
            }
            _ => Vec::new(),
        };
        Stencil { code, tail, holes, faults }
    }

    /// The function's symbol: defined at the start of its section and as long as it, so the
    /// section is the function and nothing else.
    fn check_symbol(&self, name: &str, what: &str, section: SectionIndex, len: usize) {
        let (in_section, address, size) = *self
            .symbols
            .get(name)
            .unwrap_or_else(|| panic!("stencil `{what}`: LLVM made no symbol `{name}`"));
        if in_section != SymbolSection::Section(section) || address != 0 || size != len as u64 {
            panic!("stencil `{what}`: its section holds {len} bytes, its function {size} at {address:#x}");
        }
    }

    /// Every instruction of `code`, each checked to leave the stack and the reserved
    /// registers alone.
    fn disassemble(&self, what: &str, code: &[u8], disasm: &Disasm) -> Vec<Inst> {
        let mut insts = Vec::new();
        let mut at = 0;
        while at < code.len() {
            let (len, text) = disasm.inst(&code[at..], at as u64).unwrap_or_else(|| {
                panic!("stencil `{what}`: bytes at {at:#x} that are no instruction: {:02x?}", &code[at..])
            });
            if let Some(why) = refused(self.isa, &text) {
                self.refusals.borrow_mut().push(format!("stencil `{what}`: `{}` at {at:#x} {why}", text.trim()));
            }
            insts.push(Inst { at, len, text });
            at += len;
        }
        insts
    }

    /// The bytes at the end of `code` that are its jump to `Next` (x86-64's `jmp rel32`,
    /// AArch64's `b imm26`), or 0.
    fn tail(&self, code: &[u8], insts: &[Inst], holes: &[Hole]) -> u8 {
        let Some(last) = insts.last() else { return 0 };
        let next_at = |at: usize| holes.iter().any(|it| it.kind == HoleKind::Next && usize::from(it.at) == at);
        match self.isa {
            Isa::X86_64 if last.len == 5 && code[last.at] == 0xe9 && next_at(last.at + 1) => 5,
            Isa::Aarch64 if last.len == 4 && next_at(last.at) => {
                let insn = u32::from_le_bytes([code[last.at], code[last.at + 1], code[last.at + 2], code[last.at + 3]]);
                if insn & 0xfc00_0000 == 0x1400_0000 { 4 } else { 0 }
            }
            _ => 0,
        }
    }

    /// The refused instructions of every stencil read so far, each once (the probe is
    /// disassembled twice).
    pub fn refusals(self) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        let mut refusals = self.refusals.into_inner();
        refusals.retain(|it| seen.insert(it.clone())); // clone: build time, refusals only
        refusals
    }

    /// Checks the probe (`ir::PROBE`): it stores `frame` to `Result0`, `acc` to `Result1` and
    /// `facc` to `Slot0`, so the register of each store's value, and the base of all three,
    /// show where LLVM passes them. They must be the ones the glue pins.
    pub fn probe(&self, name: &str, variant: &str, disasm: &Disasm, writer: &mut Writer) {
        let what = format!("{name} ({variant})");
        let probe = self.stencil(name, &what, None, disasm, writer);
        let insts = self.disassemble(&what, &probe.code, disasm);
        let (frame, acc, facc, base) = match self.isa {
            Isa::X86_64 => {
                let reg = |n: u8| format!("%r{n}");
                let (frame, acc) = (reg(pinned::X64.0), reg(pinned::X64.1));
                let base = format!("({frame})");
                (frame, acc, String::from("%xmm0"), base)
            }
            Isa::Aarch64 => {
                let reg = |n: u8| format!("x{n}");
                let (frame, acc) = (reg(pinned::AARCH64.0), reg(pinned::AARCH64.1));
                let base = format!("[{frame}");
                (frame, acc, String::from("q0"), base)
            }
        };
        for (kind, value, what_value) in
            [(HoleKind::Result0, &frame, "frame"), (HoleKind::Result1, &acc, "acc"), (HoleKind::Slot0, &facc, "facc")]
        {
            let hole = probe
                .holes
                .iter()
                .find(|it| it.kind == kind)
                .unwrap_or_else(|| panic!("the {variant} probe has no {kind:?} hole"));
            let at = usize::from(hole.at);
            let inst = insts
                .iter()
                .find(|it| it.at <= at && at < it.at + it.len)
                .unwrap_or_else(|| panic!("the {variant} probe's {kind:?} hole is in no instruction"));
            let text = inst.text.split_whitespace().collect::<Vec<_>>().join(" ");
            if !text.contains(&format!("{value}, ")) || !text.contains(&base) {
                panic!(
                    "the {variant} stencils' convention passes `{what_value}` elsewhere than \
                     {value} with the frame in {frame}: the probe stores it with `{text}`. \
                     LLVM's `preserve_none` changed; the glue's pinned registers \
                     (shape::pinned, arch::*::FRAME) must follow it"
                );
            }
        }
    }
}

/// The form of an ELF relocation type of `isa`, if it is one.
fn form(isa: Isa, r_type: elf::RelocationType) -> Option<Form> {
    Some(match isa {
        Isa::X86_64 => match r_type {
            elf::R_X86_64_8 => Form::Abs8,
            elf::R_X86_64_32 => Form::Abs32,
            elf::R_X86_64_32S => Form::Abs32S,
            elf::R_X86_64_64 => Form::Abs64,
            elf::R_X86_64_PC32 | elf::R_X86_64_PLT32 => Form::Rel32,
            _ => return None,
        },
        Isa::Aarch64 => match r_type {
            elf::R_AARCH64_MOVW_UABS_G0 => Form::A64MovwG0,
            elf::R_AARCH64_MOVW_UABS_G0_NC => Form::A64MovwG0Nc,
            elf::R_AARCH64_MOVW_UABS_G1 => Form::A64MovwG1,
            elf::R_AARCH64_MOVW_UABS_G1_NC => Form::A64MovwG1Nc,
            elf::R_AARCH64_MOVW_UABS_G2 => Form::A64MovwG2,
            elf::R_AARCH64_MOVW_UABS_G2_NC => Form::A64MovwG2Nc,
            elf::R_AARCH64_MOVW_UABS_G3 => Form::A64MovwG3,
            elf::R_AARCH64_ADD_ABS_LO12_NC => Form::A64AddLo12,
            elf::R_AARCH64_LDST8_ABS_LO12_NC => Form::A64Ldst8Lo12,
            elf::R_AARCH64_LDST16_ABS_LO12_NC => Form::A64Ldst16Lo12,
            elf::R_AARCH64_LDST32_ABS_LO12_NC => Form::A64Ldst32Lo12,
            elf::R_AARCH64_LDST64_ABS_LO12_NC => Form::A64Ldst64Lo12,
            elf::R_AARCH64_LDST128_ABS_LO12_NC => Form::A64Ldst128Lo12,
            elf::R_AARCH64_JUMP26 => Form::A64Jump26,
            elf::R_AARCH64_CONDBR19 => Form::A64CondBr19,
            elf::R_AARCH64_TSTBR14 => Form::A64TestBr14,
            elf::R_AARCH64_ADR_PREL_LO21 => Form::A64AdrLo21,
            elf::R_AARCH64_LD_PREL_LO19 => Form::A64LdrLit19,
            _ => return None,
        },
    })
}

/// Why LLVM's text of an instruction is refused in a stencil, if it is: it names the stack
/// pointer, the frame pointer, the link register or `x18`, or it pushes, pops, calls or
/// returns.
fn refused(isa: Isa, text: &str) -> Option<&'static str> {
    // Comments: `#` on x86-64 (whose immediates are `$`), `//` on AArch64 (whose are `#`).
    let text = match isa {
        Isa::X86_64 => text.split('#').next().unwrap_or(""),
        Isa::Aarch64 => text.split("//").next().unwrap_or(""),
    };
    let mut tokens = text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).filter(|it| !it.is_empty());
    let mnemonic = tokens.next().unwrap_or("");
    let reserved: &[&str] = match isa {
        Isa::X86_64 => &["rsp", "esp", "sp", "spl", "rbp", "ebp", "bp", "bpl"],
        Isa::Aarch64 => &["sp", "wsp", "x18", "w18", "x29", "w29", "fp", "x30", "w30", "lr"],
    };
    let stack = match isa {
        Isa::X86_64 => {
            mnemonic.starts_with("push")
                || (mnemonic.starts_with("pop") && !mnemonic.starts_with("popcnt"))
                || mnemonic.starts_with("call")
                || mnemonic.starts_with("ret")
                || mnemonic.starts_with("enter")
                || mnemonic.starts_with("leave")
        }
        Isa::Aarch64 => mnemonic == "bl" || mnemonic.starts_with("blr") || mnemonic.starts_with("ret"),
    };
    if stack {
        return Some("uses the stack or the link register: a stencil is leaf and stackless");
    }
    if tokens.any(|it| reserved.contains(&it)) {
        return Some("names a register stencils may not touch (the stack, frame or link register, or x18)");
    }
    None
}

/// Whether the op reads or writes memory through its address operand.
fn accesses_memory(op: Op) -> bool {
    matches!(
        op,
        Op::Load
            | Op::Store
            | Op::Uload8
            | Op::Sload8
            | Op::Istore8
            | Op::Uload16
            | Op::Sload16
            | Op::Istore16
            | Op::Uload32
            | Op::Sload32
            | Op::Istore32
            | Op::Uload8x8
            | Op::Sload8x8
            | Op::Uload16x4
            | Op::Sload16x4
            | Op::Uload32x2
            | Op::Sload32x2
            | Op::AtomicLoad
            | Op::AtomicStore
            | Op::AtomicRmw
            | Op::AtomicCas
            | Op::LoadConfined
            | Op::StoreConfined
    )
}

/// Whether LLVM's text of an instruction is an access of memory other than the slots (whose
/// base is the frame register) and the constants (PC-relative): an access of the op's own
/// memory, which can fault.
fn user_access(isa: Isa, text: &str) -> bool {
    let mut words = text.split_whitespace();
    let mut mnemonic = words.next().unwrap_or("");
    match isa {
        Isa::X86_64 => {
            if mnemonic == "lock" {
                mnemonic = words.next().unwrap_or("");
            }
            if ["lea", "nop", "prefetch"].iter().any(|it| mnemonic.starts_with(it)) {
                return false;
            }
            let text = text.split('#').next().unwrap_or("");
            let Some(open) = text.find('(') else { return false };
            let base = text[open + 1..].split([',', ')']).next().unwrap_or("").trim();
            base != format!("%r{}", pinned::X64.0) && base != "%rip"
        }
        Isa::Aarch64 => {
            let memory = ["ld", "st", "cas", "swp"].iter().any(|it| mnemonic.starts_with(it));
            let text = text.split("//").next().unwrap_or("");
            let Some(open) = text.find('[') else { return false };
            let base = text[open + 1..].split([',', ']']).next().unwrap_or("").trim();
            memory && base != format!("x{}", pinned::AARCH64.0)
        }
    }
}

/// Whether a stencil of `shape` may have a hole of `kind`.
fn allowed(shape: Shape, kind: HoleKind) -> bool {
    let info = shape.op.info();
    let src = |n: usize| shape.srcs[n];
    let memory = matches!(
        shape.op,
        Op::Load
            | Op::Store
            | Op::Uload8
            | Op::Sload8
            | Op::Istore8
            | Op::Uload16
            | Op::Sload16
            | Op::Istore16
            | Op::Uload32
            | Op::Sload32
            | Op::Istore32
            | Op::Uload8x8
            | Op::Sload8x8
            | Op::Uload16x4
            | Op::Sload16x4
            | Op::Uload32x2
            | Op::Sload32x2
            | Op::StackAddr
    );
    match kind {
        HoleKind::Slot0 => src(0) == Src::Slot,
        HoleKind::Slot1 => src(1) == Src::Slot,
        HoleKind::Slot2 => src(2) == Src::Slot,
        HoleKind::Imm0 => src(0) == Src::Imm || matches!(shape.op, Op::Iconst | Op::Fconst),
        HoleKind::Imm1 => src(1) == Src::Imm,
        HoleKind::Imm2 => src(2) == Src::Imm,
        HoleKind::Result0 => matches!(shape.dst, Dst::Slot | Dst::Both),
        HoleKind::Result1 => info.results == 2,
        HoleKind::Offset => memory || offset(shape),
        HoleKind::Next | HoleKind::Rodata => true,
        HoleKind::Target => info.branches,
        HoleKind::Trap => info.traps,
        HoleKind::TrapOverflow => matches!(shape.op, Op::Sdiv | Op::FcvtToUint | Op::FcvtToSint),
        HoleKind::Const => matches!(shape.op, Op::Vconst | Op::Shuffle),
    }
}

/// Whether `shape` is a confined access that adds its offset.
fn offset(shape: Shape) -> bool {
    shape.confined().is_some_and(|(_, disp)| disp != Disp::Zero)
}

/// The holes every stencil of `shape` has: its results' slots, which it always writes, a
/// confined access's offset when it adds one, and the jump to its continuation.
fn required(shape: Shape) -> impl Iterator<Item = HoleKind> {
    let result0 = matches!(shape.dst, Dst::Slot | Dst::Both).then_some(HoleKind::Result0);
    let result1 = (shape.op.info().results == 2).then_some(HoleKind::Result1);
    let offset = offset(shape).then_some(HoleKind::Offset);
    [result0, result1, offset, Some(HoleKind::Next)].into_iter().flatten()
}
