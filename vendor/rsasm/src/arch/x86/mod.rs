//! The x86 / x86-64 backend.

pub mod encode;
pub mod insn;
pub mod operand;
pub mod reg;
pub mod reloc;

use crate::arch::{ArchState, Architecture, AsmCtx, Endian, FlatModifier, InsnRequest, Syntax};
use crate::cursor::Cursor;
use crate::dwarf::{CfiTarget, DwarfTarget, Flavor, cfi, numbered_register};
use crate::expr::ExprRef;
use crate::lexer::{Punct, TokKind};
use crate::section::Variant;
use crate::source::Span;
use crate::symbol::Binding;
use encode::Prefixes;
use insn::{ATT_ONLY, DEF64, Def, Enc, INTEL_ONLY, NO64, NOTACC, ONLY64, Op};
use operand::{Operand, OperandKind, OperandParser, RoundCtl};
use reg::RegClass;

pub const NAMES: &[&str] = &["x86-64", "i386", "i8086"];

pub fn lookup(name: &str) -> Option<Box<dyn Architecture>> {
    let bits = match name {
        "x86-64" | "x86_64" | "amd64" | "x64" => 64,
        "i386" | "x86" | "i486" | "i586" | "i686" => 32,
        "i8086" | "i286" | "16" => 16,
        _ => return None,
    };
    Some(Box::new(X86 { bits }))
}

pub struct X86 {
    /// Default operating mode, before any `.code16`/`.code32`/`.code64`.
    bits: u8,
}

impl Architecture for X86 {
    fn name(&self) -> &'static str {
        match self.bits {
            64 => "x86-64",
            32 => "i386",
            _ => "i8086",
        }
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["x86_64", "amd64", "x64", "x86", "i486", "i686", "i286"]
    }

    fn endian(&self) -> Endian {
        Endian::Little
    }

    fn pointer_bytes(&self, state: &ArchState) -> u8 {
        state.bits / 8
    }

    fn initial_state(&self) -> ArchState {
        ArchState {
            bits: self.bits,
            syntax: Syntax::Att,
            features: 0,
            cpu_features: [0; 3],
            intel_register_prefix: false,
            used: 0,
            private: 0,
        }
    }

    fn supports_syntax(&self, _syntax: Syntax) -> bool {
        true
    }

    fn elf_machine(&self) -> u16 {
        match self.bits {
            64 => 62, // EM_X86_64
            _ => 3,   // EM_386
        }
    }

    fn pcrel_number_is_address(&self) -> bool {
        true
    }

    /// GNU as, the x86 reference, relocates a reference to a weak symbol in
    /// its own section, and one to a global symbol except from a jump it
    /// relaxes: `jmp global` and `jz global` are resolved, since without
    /// `-shared` it takes a global symbol to stay where it is, but `call
    /// global`, `lea global(%rip)` and `jmp global@PLT` are not. llvm-mc
    /// relocates all of them, and a `call local@PLT` besides.
    fn defers_to_linker(&self, r: &crate::arch::SameSectionRef<'_>) -> bool {
        match r.binding {
            Binding::Local => false,
            Binding::Weak => true,
            Binding::Global => !r.relaxable || r.modifier.is_some(),
        }
    }

    /// GNU as writes `call local` and `call local@PLT` as `PC32` against the
    /// label's section; llvm-mc keeps `PLT32`.
    fn section_relative_reloc(&self, reloc: u32) -> u32 {
        reloc::Abi::for_object_bits(self.bits).plt_as_pc32(reloc)
    }

    fn data_reloc(&self, size: u8, pcrel: bool) -> Option<u32> {
        let abi = reloc::Abi::for_object_bits(self.bits);
        if pcrel {
            abi.pcrel(size)
        } else {
            abi.abs(size)
        }
    }

    fn modifier_reloc(&self, name: &str, size: u8, pcrel: bool) -> Option<u32> {
        let abi = reloc::Abi::for_object_bits(self.bits);
        match name {
            // `wrt ..sym` relocates against the symbol itself, with the plain
            // absolute or PC-relative type for the field.
            "sym" => {
                if pcrel {
                    abi.pcrel(size)
                } else {
                    abi.abs(size)
                }
            }
            // The TLS descriptor call is marked rather than filled in, so its
            // fixup is zero bytes wide; see `Abi::tlsdesc_call`.
            "tlscall" => (size == 0).then(|| abi.tlsdesc_call()),
            // `@SIZE` is the one modifier both ABIs spell the same way and
            // number differently, and the only one i386 shares with x86-64
            // outside the GOT and PLT.
            "size" => abi.size(size),
            // Every other i386 modifier names a 32-bit relocation.
            _ if abi == reloc::Abi::I386 => match name {
                "gotpc" => abi.gotpc(size),
                _ if size == 4 => reloc::Abi::i386_modifier(name),
                _ => None,
            },
            "plt" => Some(abi.plt32()),
            // NASM spells the RIP-relative GOT load `wrt ..got`; `..gotpcrel`
            // is accepted too, as the GNU `@GOTPCREL` name.
            "gotpcrel" => abi.gotpcrel(size),
            "got" => abi.got(size, pcrel),
            "gotoff" => abi.gotoff(size),
            "gotpc" => abi.gotpc(size),
            _ => reloc::Abi::x86_64_tls_modifier(name, size),
        }
    }

    /// Mach-O's x86-64 modifiers are `@GOTPCREL` -- a load through the GOT in
    /// a RIP-relative operand, or in data the address of the GOT slot
    /// relative to the field -- and `@TLVP`, the address of a thread-local
    /// variable's descriptor, which llvm-mc takes in a RIP-relative operand
    /// and nowhere else. A branch cannot go through either.
    ///
    /// i386 has `@TLVP` alone, and takes it in an absolute operand instead,
    /// since the machine has no PC-relative addressing: `movl _v@TLVP, %eax`
    /// where the descriptor's address is known at link time, and
    /// `movl _v@TLVP-L0$pb(%eax), %ecx` where position-independent code has
    /// to reach it through a base register.
    fn modifier_class(
        &self,
        name: &str,
        kind: &crate::section::FixupKind,
    ) -> Option<crate::reloc::RelocClass> {
        use crate::reloc::RelocClass;
        match (name, kind.class) {
            (_, RelocClass::Branch) => None,
            ("gotpcrel", RelocClass::GotLoad) if self.bits == 64 => Some(RelocClass::GotLoad),
            ("gotpcrel", _) if self.bits == 64 => Some(RelocClass::Got),
            ("tlvp", _) if self.bits == 64 && kind.pcrel && kind.size == 4 => {
                Some(RelocClass::ThreadVariable)
            }
            ("tlvp", _) if self.bits == 32 && !kind.pcrel && kind.size == 4 => {
                Some(RelocClass::ThreadVariable)
            }
            _ => None,
        }
    }

    /// GNU as creates `_GLOBAL_OFFSET_TABLE_` for every modifier but `@PLT`,
    /// `@PLTOFF` and `@SIZE`, and marks the target of a TLS one thread-local.
    fn modifier_symbols(&self, name: &str) -> crate::arch::ModifierSymbols {
        let tls = matches!(
            name,
            "tlsgd"
                | "tlsldm"
                | "tlsld"
                | "gottpoff"
                | "tpoff"
                | "ntpoff"
                | "dtpoff"
                | "gotntpoff"
                | "indntpoff"
                | "tlsdesc"
                | "tlscall"
        );
        let got = tls
            || matches!(name, "got" | "gotoff" | "gotpcrel" | "gotplt")
            || name == "gotpc" && reloc::Abi::for_object_bits(self.bits) == reloc::Abi::I386;
        crate::arch::ModifierSymbols {
            needs: got.then_some("_GLOBAL_OFFSET_TABLE_"),
            tls,
            ..crate::arch::ModifierSymbols::default()
        }
    }

    fn fixup_modifier_reloc(
        &self,
        name: &str,
        kind: &crate::section::FixupKind,
    ) -> crate::arch::FixupModifier {
        use crate::arch::FixupModifier;
        let abi = reloc::Abi::for_object_bits(self.bits);
        // The encoder marks the `@GOT` loads the linker may relax on i386,
        // and the `@GOTPCREL` ones on x86-64, where the number also says
        // whether the instruction carries a REX prefix.
        if abi == reloc::Abi::I386 && name == "got" && kind.reloc == reloc::Abi::I386_GOT32X {
            return FixupModifier::Reloc(kind.reloc);
        }
        if name == "gotpcrel" && reloc::Abi::is_gotpcrelx(kind.reloc) {
            return FixupModifier::Reloc(kind.reloc);
        }
        // A size is unsigned, and no psABI has a sign-extending form of
        // `@SIZE`, so a 64-bit-mode displacement or a 64-bit operation's
        // immediate cannot carry one; GNU as refuses that pair. The same
        // field written with a 32-bit address size is zero-extended and takes
        // it: `movl x@SIZE(%eax), %ecx` assembles where `movl x@SIZE(%rax),
        // %ecx` does not.
        if name == "size" && abi.is_abs32_signed(kind.reloc) {
            return FixupModifier::Unknown;
        }
        match self.modifier_reloc(name, kind.size, kind.pcrel) {
            Some(r) => FixupModifier::Reloc(r),
            None => FixupModifier::Unknown,
        }
    }

    /// `@SIZE` is x86's size modifier, in both psABIs and both syntaxes.
    fn modifier_is_size(&self, name: &str) -> bool {
        name == "size"
    }

    /// `@PLT` is `L + A - P`, and in a static image the PLT entry `L` is the
    /// function itself; `@GOT` and `@GOTPCREL` need a GOT.
    fn flat_modifier(&self, name: &str) -> FlatModifier {
        if name == "plt" {
            FlatModifier::PcRelative
        } else {
            FlatModifier::LinkerOnly
        }
    }

    /// GNU as's conventions, as for every x86 encoding, except in a Mach-O
    /// object, which GNU as does not write at all. The object's class
    /// decides, not a `.code32` in a 64-bit file.
    ///
    /// Three constants move with the format. In a PE object GNU as numbers
    /// the return address column 32 rather than 16 on x86-64
    /// (`x86_dwarf2_return_column` under `OBJ_COFF` and `TE_PE`); llvm-mc,
    /// which writes the Mach-O objects, keeps the psABI's 16 and encodes an
    /// FDE's address as a pointer-sized distance (`DW_EH_PE_pcrel`) rather
    /// than the four-byte one GNU as writes.
    fn dwarf(&self, _state: &ArchState, format: crate::output::Format) -> DwarfTarget {
        let macho = format == crate::output::Format::MachO;
        let ra64 = if format.is_coff() { 32 } else { 16 };
        let cfi = match self.bits {
            64 => CfiTarget {
                data_align: -8,
                ra_column: ra64,
                initial: vec![cfi::Insn::DefCfa(7, 8), cfi::Insn::Offset(ra64, -8)],
                fde_encoding: if macho { 0x10 } else { 0x1b },
                eh_frame_align: 8,
                cie_version: 1,
            },
            // The stack pointer is register 5 in Darwin's i386 numbering
            // and 4 in the psABI's; see `dwarf_register`.
            _ => CfiTarget {
                data_align: -4,
                ra_column: 8,
                initial: vec![
                    cfi::Insn::DefCfa(if macho { 5 } else { 4 }, 4),
                    cfi::Insn::Offset(8, -4),
                ],
                fde_encoding: if macho { 0x10 } else { 0x1b },
                eh_frame_align: 4,
                cie_version: 1,
            },
        };
        let flavor = if macho { Flavor::Llvm } else { Flavor::Gnu };
        DwarfTarget {
            cfi: Some(cfi),
            ..DwarfTarget::lines_only(flavor, 1)
        }
    }

    /// GNU as's `dw2_regnum` table, for the names it accepts in each class:
    /// the psABI numbering, which differs between the two word sizes.
    ///
    /// Darwin's i386 unwinding numbers two of them its own way, swapping
    /// `%esp` and `%ebp` (LLVM's `DWARFFlavour::X86_32_DarwinEH`), and
    /// numbers the rest as the psABI does.
    fn dwarf_register(
        &self,
        _state: &ArchState,
        format: crate::output::Format,
        name: &str,
    ) -> Option<u32> {
        let name = name.strip_prefix('%').unwrap_or(name);
        // `%st(0)` and `%st` are the same register.
        let name = match name.replace(' ', "").as_str() {
            "st" => "st0".to_string(),
            n if n.starts_with("st(") && n.ends_with(')') => format!("st{}", &n[3..n.len() - 1]),
            n => n.to_string(),
        };
        let name = name.as_str();
        // `ymm` and `zmm` registers unwind as the `xmm` register they extend.
        let vector = |max| {
            ["xmm", "ymm", "zmm"]
                .iter()
                .find_map(|p| numbered_register(name, p, max))
        };
        if self.bits == 64 {
            const GPR: [&str; 17] = [
                "rax", "rdx", "rcx", "rbx", "rsi", "rdi", "rbp", "rsp", "r8", "r9", "r10", "r11",
                "r12", "r13", "r14", "r15", "rip",
            ];
            const SEG: [&str; 6] = ["es", "cs", "ss", "ds", "fs", "gs"];
            if let Some(i) = GPR.iter().position(|r| *r == name) {
                return Some(i as u32);
            }
            if let Some(i) = SEG.iter().position(|r| *r == name) {
                return Some(50 + i as u32);
            }
            if let Some(v) = vector(31) {
                return Some(if v < 16 { 17 + v } else { 67 + v - 16 });
            }
            return match name {
                "rflags" | "eflags" => Some(49),
                "fs.base" => Some(58),
                "gs.base" => Some(59),
                "tr" => Some(62),
                "ldtr" => Some(63),
                "mxcsr" => Some(64),
                "fcw" => Some(65),
                "fsw" => Some(66),
                _ => numbered_register(name, "st", 7)
                    .map(|n| 33 + n)
                    .or_else(|| numbered_register(name, "mm", 7).map(|n| 41 + n))
                    .or_else(|| numbered_register(name, "k", 7).map(|n| 118 + n))
                    .or_else(|| numbered_register(name, "bnd", 3).map(|n| 126 + n)),
            };
        }
        const GPR: [&str; 10] = [
            "eax", "ecx", "edx", "ebx", "esp", "ebp", "esi", "edi", "eip", "eflags",
        ];
        const GPR_DARWIN: [&str; 10] = [
            "eax", "ecx", "edx", "ebx", "ebp", "esp", "esi", "edi", "eip", "eflags",
        ];
        let gpr = if format == crate::output::Format::MachO {
            &GPR_DARWIN
        } else {
            &GPR
        };
        const SEG: [&str; 6] = ["es", "cs", "ss", "ds", "fs", "gs"];
        if let Some(i) = gpr.iter().position(|r| *r == name) {
            return Some(i as u32);
        }
        if let Some(i) = SEG.iter().position(|r| *r == name) {
            return Some(40 + i as u32);
        }
        match name {
            "fcw" => Some(37),
            "fsw" => Some(38),
            "mxcsr" => Some(39),
            "tr" => Some(48),
            "ldtr" => Some(49),
            _ => numbered_register(name, "st", 7)
                .map(|n| 11 + n)
                .or_else(|| vector(7).map(|n| 21 + n))
                .or_else(|| numbered_register(name, "mm", 7).map(|n| 29 + n))
                .or_else(|| numbered_register(name, "k", 7).map(|n| 93 + n)),
        }
    }

    /// The compact unwind word `__LD,__compact_unwind` holds for a frame,
    /// which on x86 describes a frame pointer prologue, a frameless function
    /// whose stack adjustment the word itself holds, or a frameless one whose
    /// adjustment the word only points the linker at.
    ///
    /// `DarwinX86AsmBackend::generateCompactUnwindEncoding` reads the
    /// directives a compiler writes and gives up on anything else:
    /// `.cfi_def_cfa_register` has to name the frame pointer, each
    /// `.cfi_offset` has to name one of the six callee-saved registers the
    /// word can hold, and behind a frame pointer the lowest of them has to
    /// sit directly below the saved frame pointer. Giving up means
    /// `UNWIND_X86_MODE_DWARF`, which asks the linker for the frame table
    /// instead. Where arm64 gives each pair of saved registers a bit of its
    /// own, x86 writes the registers themselves -- in the order they were
    /// saved, three bits each, behind a frame pointer, and as the number of
    /// the permutation they form where there is none, which is what makes
    /// six of them fit in ten bits.
    ///
    /// A frame with no directives at all gets a word of zero, which is no
    /// entry in the table rather than an entry describing nothing.
    fn macho_compact_unwind(&self, insns: &[cfi::Insn], canonical: bool) -> Option<u32> {
        use cfi::Insn;

        const MODE_BP_FRAME: u32 = 0x0100_0000;
        const MODE_STACK_IMMD: u32 = 0x0200_0000;
        const MODE_STACK_IND: u32 = 0x0300_0000;
        const MODE_DWARF: u32 = 0x0400_0000;
        // Five of the six registers is all a frame pointer's word has room
        // for; llvm-mc drops the sixth rather than give up.
        const BP_FRAME_REGISTERS: u32 = 0x0000_7fff;

        let wide = self.bits == 64;
        // The six registers the word can name, numbered 1 to 6 in this
        // order, as `.cfi_offset` names them: the callee-saved ones of each
        // ABI, and the frame pointer last. A 32-bit number is Darwin's,
        // which is the only numbering a compact unwind table is read in; see
        // [`X86::dwarf_register`].
        let order: [u32; 6] = if wide {
            [3, 12, 13, 14, 15, 6]
        } else {
            [3, 1, 2, 7, 6, 4]
        };
        let frame_pointer = if wide { 6 } else { 4 };
        // The stack slot a saved register takes, which is also the unit the
        // word counts a stack adjustment in.
        let slot = if wide { 8i64 } else { 4 };

        // `.cfi_signal_frame` is not an instruction, only a note on the
        // frame, and llvm-mc's list of instructions never holds one.
        let insns: Vec<&Insn> = insns.iter().filter(|i| **i != Insn::Mark).collect();
        if insns.is_empty() {
            return Some(0);
        }
        if !canonical {
            return Some(MODE_DWARF);
        }

        let mut saved: Vec<u32> = Vec::new();
        let mut has_fp = false;
        let mut stack = 0i64;
        let mut lowest = i64::MAX;
        // The bytes of the pushes that saved the registers, which the
        // indirect form counts to find the stack adjustment's own
        // instruction.
        let mut pushes = 0u32;
        for insn in insns {
            match *insn {
                Insn::DefCfaRegister(reg) => {
                    if reg != frame_pointer {
                        return Some(MODE_DWARF);
                    }
                    // What came before set the frame pointer up, and the
                    // mode says so by itself.
                    saved.clear();
                    lowest = i64::MAX;
                    has_fp = true;
                }
                // The last adjustment is the frame's size; the ones before
                // it described the prologue on its way there.
                Insn::DefCfaOffset(by) => stack = by,
                Insn::Offset(reg, at) => {
                    if saved.len() == order.len() {
                        return Some(MODE_DWARF);
                    }
                    saved.push(reg);
                    lowest = lowest.min(at.abs());
                    pushes += if wide && (12..=15).contains(&reg) {
                        2
                    } else {
                        1
                    };
                }
                _ => return Some(MODE_DWARF),
            }
        }
        let Some(numbers) = saved
            .iter()
            .map(|r| order.iter().position(|o| o == r).map(|i| i as u32 + 1))
            .collect::<Option<Vec<u32>>>()
        else {
            return Some(MODE_DWARF);
        };
        let count = numbers.len() as u32;

        if has_fp {
            // A register saved anywhere but in the run directly below the
            // saved frame pointer, which is itself below the return address,
            // is one the word cannot place.
            if count != 0 && lowest != 3 * slot {
                return Some(MODE_DWARF);
            }
            let regs = numbers
                .iter()
                .enumerate()
                .fold(0, |w, (i, n)| w | (n << (3 * i)));
            return Some(MODE_BP_FRAME | (count << 16) | (regs & BP_FRAME_REGISTERS));
        }

        let size = stack / slot;
        let registers = (count << 10) | compact_unwind_permutation(&numbers);
        Some(match u8::try_from(size) {
            // The adjustment fits in the eight bits the word has for it.
            Ok(size) => MODE_STACK_IMMD | (u32::from(size) << 16) | registers,
            // It does not, so the linker reads it out of the `sub` that made
            // the frame. The word carries the offset of that instruction's
            // immediate, past the pushes that saved the registers and past
            // the two or three bytes of opcode before it, and how many slots
            // those pushes took, which the `sub` does not account for.
            Err(_) => {
                let at = if wide { 3 } else { 2 } + pushes;
                MODE_STACK_IND | (at << 16) | ((count + 1) << 13) | registers
            }
        })
    }

    /// The same swap of `%esp` and `%ebp` as [`X86::dwarf_register`], which
    /// takes a Darwin i386 register back to the number the psABI gives it.
    fn dwarf_debug_register(&self, format: crate::output::Format, reg: u32) -> u32 {
        if self.bits != 32 || format != crate::output::Format::MachO {
            return reg;
        }
        match reg {
            4 => 5,
            5 => 4,
            _ => reg,
        }
    }

    fn is_mnemonic(&self, name: &str) -> bool {
        insn::is_mnemonic(name) || prefix_kind(name).is_some()
    }

    fn nop_fill(&self, state: &ArchState, len: u64) -> Vec<u8> {
        encode::nop_bytes(state.bits, len as usize)
    }

    fn assemble(&self, cx: &mut AsmCtx<'_>, req: &InsnRequest<'_>) -> Option<Vec<Variant>> {
        let mnemonic = cx.name(req.mnemonic).to_ascii_lowercase();
        let abi = reloc::Abi::for_object_bits(self.bits);
        assemble_inner(cx, req, &mnemonic, Prefixes::default(), abi, 0)
    }

    fn directive(&self, cx: &mut AsmCtx<'_>, name: &str, cur: &mut Cursor<'_>) -> bool {
        match name {
            ".code16" | ".code32" | ".code64" => {
                let bits: u8 = name[5..].parse().expect("literal is numeric");
                cx.state.bits = bits;
                cx.state.features &= !CODE16GCC;
                true
            }
            ".code16gcc" => {
                cx.state.bits = 16;
                cx.state.features |= CODE16GCC;
                true
            }
            ".intel_syntax" => {
                cx.state.syntax = Syntax::Intel;
                // `noprefix` (the usual spelling) means registers are written
                // bare; `prefix` keeps the AT&T `%` sigil.
                if let TokKind::Ident(n) = cur.peek().kind {
                    let word = cx.interner.get(n).to_ascii_lowercase();
                    cur.advance();
                    cx.state.intel_register_prefix = word == "prefix";
                }
                true
            }
            ".att_syntax" => {
                cx.state.syntax = Syntax::Att;
                if let TokKind::Ident(_) = cur.peek().kind {
                    cur.advance();
                }
                true
            }
            _ => false,
        }
    }
}

/// The ten bits a frameless compact unwind word gives the registers it
/// saved, which hold their order rather than the registers themselves.
///
/// There are six registers the word can name and at most six saved, so there
/// are 1,956 possibilities and ten bits is enough for a number of one of
/// them. `encodeCompactUnwindRegistersWithoutFrame` arrives at that number
/// by renumbering each register by how many of the ones saved before it are
/// lower -- which leaves one fewer choice at each step -- and reading the
/// result as a number whose digits run in a radix of their own.
fn compact_unwind_permutation(numbers: &[u32]) -> u32 {
    // The registers sit at the end of six slots, so that the digits of a
    // shorter list are the last digits of a longer one's.
    let mut slots = [0u32; 6];
    let first = slots.len() - numbers.len();
    slots[first..].copy_from_slice(numbers);
    let mut digit = [0u32; 6];
    for i in first..slots.len() {
        let lower = (first..i).filter(|&j| slots[j] < slots[i]).count() as u32;
        digit[i] = slots[i] - lower - 1;
    }
    let d = digit;
    match numbers.len() {
        6 => 120 * d[0] + 24 * d[1] + 6 * d[2] + 2 * d[3] + d[4],
        5 => 120 * d[1] + 24 * d[2] + 6 * d[3] + 2 * d[4] + d[5],
        4 => 60 * d[2] + 12 * d[3] + 3 * d[4] + d[5],
        3 => 20 * d[3] + 4 * d[4] + d[5],
        2 => 5 * d[4] + d[5],
        1 => d[5],
        _ => 0,
    }
}

/// Set in `ArchState::features` by `.code16gcc`: 16-bit code in which the
/// stack instructions and calls default to 32 bits, as GCC's 32-bit output
/// assumes when it is assembled to run in real mode.
pub const CODE16GCC: u64 = 1 << 0;

/// A prefix mnemonic, which attaches to the instruction written after it.
enum PrefixKind {
    Lock,
    Rep(u8),
    Segment(u8),
    /// `data16`/`data32`: the operand size override, with its size.
    Data(u8),
    /// `addr16`/`addr32`: the address size override, with its size.
    Addr(u8),
    /// `{vex}`, `{vex3}`, `{evex}`, `{rex}`, `{rex2}`: which encoding to
    /// choose. Emits nothing of its own.
    Encoding(encode::EncodingPrefix),
    /// `{nf}`: APX's no-flags form of the instruction.
    NoFlags,
}

fn prefix_kind(mnemonic: &str) -> Option<PrefixKind> {
    use encode::EncodingPrefix as E;
    Some(match mnemonic {
        "{vex}" | "{vex2}" => PrefixKind::Encoding(E::Vex),
        "{vex3}" => PrefixKind::Encoding(E::Vex3),
        "{evex}" => PrefixKind::Encoding(E::Evex),
        "{rex}" => PrefixKind::Encoding(E::Rex),
        "{rex2}" => PrefixKind::Encoding(E::Rex2),
        "{nf}" => PrefixKind::NoFlags,
        "lock" => PrefixKind::Lock,
        "rep" | "repe" | "repz" => PrefixKind::Rep(0xf3),
        "repne" | "repnz" => PrefixKind::Rep(0xf2),
        "es" | "cs" | "ss" | "ds" | "fs" | "gs" => {
            let r = reg::lookup(mnemonic).expect("segment names are in the register table");
            PrefixKind::Segment(encode::segment_prefix(r).expect("segment has a prefix byte"))
        }
        "data16" => PrefixKind::Data(16),
        "data32" => PrefixKind::Data(32),
        "addr16" => PrefixKind::Addr(16),
        "addr32" => PrefixKind::Addr(32),
        _ => return None,
    })
}

fn assemble_inner(
    cx: &mut AsmCtx<'_>,
    req: &InsnRequest<'_>,
    mnemonic: &str,
    mut prefixes: Prefixes,
    abi: reloc::Abi,
    depth: u32,
) -> Option<Vec<Variant>> {
    if depth > 4 {
        cx.error(req.span, "too many instruction prefixes");
        return None;
    }

    // `lock`, `rep`, `fs` and friends prefix the instruction that follows.
    if let Some(kind) = prefix_kind(mnemonic) {
        let bits = cx.state.bits;
        match kind {
            PrefixKind::Lock => prefixes.lock = true,
            PrefixKind::Rep(r) => prefixes.rep = Some(r),
            PrefixKind::Segment(s) => prefixes.seg = Some(s),
            PrefixKind::Encoding(e) => {
                if req.cursor().at_end() {
                    cx.error(
                        req.mnemonic_span,
                        format!("`{mnemonic}` needs an instruction after it"),
                    );
                    return None;
                }
                // `{rex2}` wins over `{rex}` whichever order the two were
                // written in, as GNU as resolves the pair.
                if prefixes.encoding != Some(encode::EncodingPrefix::Rex2)
                    || e != encode::EncodingPrefix::Rex
                {
                    prefixes.encoding = Some(e);
                }
            }
            PrefixKind::NoFlags => {
                if req.cursor().at_end() {
                    cx.error(
                        req.mnemonic_span,
                        format!("`{mnemonic}` needs an instruction after it"),
                    );
                    return None;
                }
                prefixes.nf = true;
            }
            // A size prefix names the size it switches to, so the mode's own
            // size is refused as redundant, and long mode has no 32-bit
            // operand or 16-bit address prefix to write.
            PrefixKind::Data(size) | PrefixKind::Addr(size) => {
                let data = matches!(kind, PrefixKind::Data(_));
                let (native, missing) = if data { (32, 32) } else { (64, 16) };
                let native = if bits == 64 { native } else { bits };
                if bits == 64 && size == missing {
                    cx.error(
                        req.mnemonic_span,
                        format!("`{mnemonic}` is not available in 64-bit mode"),
                    );
                    return None;
                }
                if size == native {
                    cx.error(
                        req.mnemonic_span,
                        format!("`{mnemonic}` is redundant in {bits}-bit mode"),
                    );
                    return None;
                }
                if data {
                    prefixes.data = true;
                } else {
                    prefixes.addr = true;
                }
            }
        }
        let mut cur = req.cursor();
        if cur.at_end() {
            // A bare prefix on its own line emits just the prefix byte.
            let mut bytes = Vec::new();
            if let Some(s) = prefixes.seg {
                bytes.push(s);
            }
            if prefixes.addr {
                bytes.push(0x67);
            }
            if prefixes.data {
                bytes.push(0x66);
            }
            if let Some(r) = prefixes.rep {
                bytes.push(r);
            }
            if prefixes.lock {
                bytes.push(0xf0);
            }
            return Some(vec![Variant::new(bytes)]);
        }
        let tok = cur.advance();
        // The parser reads a `{...}` pseudo-prefix as a mnemonic only where a
        // statement begins, so a second one -- `{evex} {nf} add` -- still has
        // its braces here and is put back together the same way.
        let braced = tok.is_punct(Punct::LBrace)
            && matches!(cur.peek().kind, TokKind::Ident(_))
            && cur.nth(1).is_punct(Punct::RBrace);
        let (next, next_span) = if braced {
            let TokKind::Ident(word) = cur.advance().kind else {
                unreachable!("the token was checked to be an identifier")
            };
            let close = cur.advance();
            let text = format!("{{{}}}", cx.name(word).to_ascii_lowercase());
            (cx.interner.intern(&text), tok.span.to(close.span))
        } else if let Some(n) = tok.ident() {
            (n, tok.span)
        } else {
            cx.error(tok.span, "expected an instruction after a prefix");
            return None;
        };
        let next_text = cx.name(next).to_ascii_lowercase();
        let sub = InsnRequest {
            mnemonic: next,
            mnemonic_span: next_span,
            operands: cur.rest(),
            span: req.span,
        };
        return assemble_inner(cx, &sub, &next_text, prefixes, abi, depth + 1);
    }

    let syntax = cx.state.syntax;
    let bits = cx.state.bits;

    let Some(resolved) = resolve_mnemonic(mnemonic, syntax) else {
        cx.error(
            req.mnemonic_span,
            format!("unknown instruction `{mnemonic}`"),
        );
        return None;
    };
    // A `q` suffix names a size only long mode has. (`movq` in 32-bit code
    // is still the MMX and SSE move.)
    if bits != 64 && resolved.opsize == Some(64) && resolved.fallback.is_empty() {
        cx.error(
            req.mnemonic_span,
            format!("`{mnemonic}` is only available in 64-bit mode"),
        );
        return None;
    }

    // Parse the operand list. `short` and `near` are only read as the width
    // of a displacement, so they are offered to the parser for an instruction
    // that has a relative form and are an unknown name anywhere else, as they
    // are to NASM.
    let mut cur = req.cursor();
    // `{dfv=...}` qualifies the instruction rather than an operand, and GNU as
    // writes it between the mnemonic and the first operand with no comma, so
    // it is read here instead of being parsed as one.
    if cur.check_punct(Punct::LBrace) {
        let mut p = OperandParser {
            cx,
            syntax,
            addr_size: if bits == 64 { 8 } else { bits / 8 },
            relative: false,
        };
        if let Some(found) = p.dfv_suffix(&mut cur) {
            prefixes.dfv = Some(found?);
        }
    }
    let pieces = cur.split_commas();
    let relative = resolved
        .defs
        .iter()
        .any(|d| d.ops.first().is_some_and(|o| matches!(o, Op::Rel(_))));
    let mut ops: Vec<Operand> = Vec::with_capacity(pieces.len());
    for piece in &pieces {
        if piece.is_empty() {
            cx.error(req.span, "empty operand");
            return None;
        }
        let mut pc = Cursor::new(piece);
        let mut p = OperandParser {
            cx,
            syntax,
            addr_size: if bits == 64 { 8 } else { bits / 8 },
            relative,
        };
        let o = p.parse(&mut pc)?;
        if !pc.at_end() && !pc.is_empty() {
            cx.error(pc.peek().span, "unexpected token after operand");
            return None;
        }
        ops.push(o);
    }
    for o in &mut ops {
        fold_operand(cx, o);
    }

    // The table is written in Intel order, so AT&T operands are reversed —
    // except for `enter` and `bound`, whose AT&T operands GNU as has always
    // taken in Intel order, and llvm-mc with it, and for the instructions
    // that only name their implicit registers, which GNU as lists in the same
    // order in both syntaxes.
    let named = |name: &str| {
        mnemonic
            .strip_prefix(name)
            .is_some_and(|s| matches!(s, "" | "b" | "w" | "l" | "q"))
    };
    let implicit = matches!(
        mnemonic,
        "monitor" | "monitorx" | "mwait" | "mwaitx" | "tpause" | "umwait"
    );
    if syntax == Syntax::Att && !named("enter") && !named("bound") && !implicit {
        ops.reverse();
    }
    // `imul $imm, %reg` multiplies the register in place: it is the
    // three-operand form with the register as both source and destination.
    // APX's `imulzu` has the same shape, and `clr` names the one register its
    // encoding holds twice.
    if (named("imul") || mnemonic == "imulzu")
        && ops.len() == 2
        && ops[0].reg().is_some()
        && matches!(ops[1].kind, OperandKind::Imm(_))
    {
        ops.insert(1, ops[0].clone());
    }
    if mnemonic == "clr" && ops.len() == 1 && ops[0].reg().is_some() {
        ops.push(ops[0].clone());
    }
    // AT&T writes a direct far pointer as two immediates, segment first.
    if syntax == Syntax::Att
        && ops.len() == 2
        && resolved.defs.iter().any(|d| d.ops == [Op::Far])
        && let (OperandKind::Imm(off), OperandKind::Imm(seg)) = (&ops[0].kind, &ops[1].kind)
    {
        let span = ops[1].span.to(ops[0].span);
        ops = vec![Operand {
            kind: OperandKind::FarPtr {
                seg: *seg,
                off: *off,
            },
            size_hint: None,
            decor: Default::default(),
            span,
        }];
    }

    // NASM moves the accumulator to or from a bare address with the one-byte
    // `A0`-`A3` opcodes, a byte shorter than the ModRM form, which is why
    // `mov eax, [var]` is `a1` there and `8b 05` under GNU as.
    if cx.dialect == crate::lexer::Dialect::Nasm
        && (mnemonic == "mov" || mnemonic == "movq")
        && let Some(v) = try_moffs(cx, bits, abi, &ops, req.span)
    {
        return Some(vec![v]);
    }

    // A size keyword on one operand fixes the operation width: `mov [eax],
    // byte 1` is a byte store, though the memory operand itself is unsized.
    // NASM lets the keyword ride on whichever operand it likes, so the hint
    // is carried to an unsized memory operand from a sized sibling.
    if let Some(hint) = ops
        .iter()
        .find(|o| matches!(o.kind, OperandKind::Imm(_)) && o.size_hint.is_some())
        .and_then(|o| o.size_hint)
    {
        for o in &mut ops {
            if o.is_mem() && o.size_hint.is_none() {
                o.size_hint = Some(hint);
            }
        }
    }

    // `{rn-sae}` occupies an operand slot in the source but encodes as bits in
    // the EVEX prefix, so it is lifted out before the operands are matched.
    let mut rounding: Option<(RoundCtl, Span)> = None;
    for o in &ops {
        if let Some(ctl) = o.rounding() {
            if rounding.is_some() {
                cx.error(o.span, "only one rounding-control decorator is allowed");
                return None;
            }
            rounding = Some((ctl, o.span));
        }
    }
    if let Some(pos) = ops.iter().position(|o| o.rounding().is_some())
        && cx.dialect != crate::lexer::Dialect::Nasm
        && !rounding_in_place(&ops, pos, syntax)
    {
        cx.error(ops[pos].span, "the rounding-control operand is misplaced");
        return None;
    }
    ops.retain(|o| o.rounding().is_none());

    // NASM's default optimizer loads a 64-bit register from a non-negative
    // immediate that fits 32 bits with the `mov r32, imm32` form, which
    // zero-extends and is two bytes shorter than the sign-extending one. GNU
    // as leaves it as written; the difference shows only in the NASM dialect.
    if cx.dialect == crate::lexer::Dialect::Nasm
        && bits == 64
        && (mnemonic == "mov" || mnemonic == "movq")
        && let [dst, src] = ops.as_slice()
        && let (OperandKind::Reg(r), OperandKind::Imm(e)) = (&dst.kind, &src.kind)
        && r.is_gpr()
        && r.size == 8
        && cx
            .constant(*e)
            .is_some_and(|v| (0..=0xffff_ffff).contains(&v))
    {
        ops[0].kind = OperandKind::Reg(reg::Reg { size: 4, ..*r });
        ops[0].size_hint = Some(4);
    }

    let mut matches = select(cx, bits, resolved.defs, &resolved, &ops);
    if matches.is_empty() {
        report_no_match(cx, req, mnemonic, &resolved, &ops);
        return None;
    }
    // A `{1toN}` says how long the vector is where a memory operand alone
    // cannot, as in `vcvtpd2ps (%rax){1to4}, %xmm0`. A count no row has is
    // left for the encoder to report.
    if let Some(n) = ops.iter().find_map(|o| o.decor.broadcast.map(|b| b.count))
        && matches.iter().any(|d| d.broadcast_count() == Some(n))
    {
        matches.retain(|d| d.broadcast_count() == Some(n));
    }
    // `{vex}` and `{evex}` narrow the choice to one encoding. That is the only
    // way to reach the VEX forms of AVX-VNNI and AVX-IFMA, whose mnemonics
    // AVX-512 already spells, and the only way to ask for an APX form of an
    // instruction whose legacy one is shorter. `{rex}` and `{rex2}` ask for a
    // legacy row instead, being prefixes of that encoding rather than
    // encodings of their own.
    if let Some(want) = prefixes.encoding {
        use encode::EncodingPrefix as E;
        let (name, keep): (_, fn(&Def) -> bool) = match want {
            E::Evex => ("EVEX", |d| matches!(d.enc, Enc::Evex | Enc::Apx)),
            E::Rex | E::Rex2 => ("legacy", |d| d.enc == Enc::Legacy),
            _ => ("VEX", |d| d.enc == Enc::Vex),
        };
        matches.retain(|d| keep(d));
        if matches.is_empty() {
            cx.error(
                req.span,
                format!("`{mnemonic}` has no {name} encoding for these operands"),
            );
            return None;
        }
    }
    // `{nf}` exists only for the rows Intel gave a no-flags form, and asking
    // for it is also what chooses the APX encoding over the legacy one.
    if prefixes.nf {
        matches.retain(|d| d.flags & insn::APX_NF != 0);
        if matches.is_empty() {
            cx.error(
                req.span,
                format!("`{mnemonic}` has no `{{nf}}` form for these operands"),
            );
            return None;
        }
    }
    // `{dfv=...}` only means anything where the prefix holds a condition for
    // it to be compared against.
    if let Some((_, dspan)) = prefixes.dfv {
        matches.retain(|d| d.scc.is_some());
        if matches.is_empty() {
            cx.error(
                dspan,
                format!("`{mnemonic}` takes no `{{dfv=...}}` decorator"),
            );
            return None;
        }
    }

    // NASM loads a 64-bit register from a symbol with the full `movabs`
    // (64-bit immediate) form, since the address is unknown and might not fit
    // 32 bits; GNU as uses the sign-extending 32-bit form. This shows only in
    // the NASM dialect and only for a still-symbolic immediate.
    //
    // GNU as reaches for the same form for `movq $x@SIZE, %rax` in either
    // dialect: a size is unsigned, and the 32-bit form's relocation
    // sign-extends, so only the 64-bit immediate can hold one. `addq
    // $x@SIZE, %rax` has no such form to fall back on and is refused.
    if bits == 64
        && (mnemonic == "mov" || mnemonic == "movq")
        && let [dst, src] = ops.as_slice()
        && let (OperandKind::Reg(r), OperandKind::Imm(e)) = (&dst.kind, &src.kind)
        && r.is_gpr()
        && r.size == 8
        && (cx
            .find_modifier_for(*e)
            .is_some_and(|m| cx.name(m) == "size")
            || cx.dialect == crate::lexer::Dialect::Nasm && cx.constant(*e).is_none())
        && let Some(pos) = matches.iter().position(|d| d.flags & insn::IMM64 != 0)
    {
        matches.swap(0, pos);
    }

    // Stack and branch instructions have a default operand size in every
    // mode, which the rest do not; the rows long mode widened say which.
    let stack = resolved.defs.iter().any(|d| d.flags & DEF64 != 0);
    // The extending moves always need the source's size, even where the
    // destination leaves only one.
    let extends = matches!(mnemonic, "movzx" | "movsx")
        && ops.iter().any(|o| o.is_mem() && o.size_hint.is_none());
    if syntax == Syntax::Intel && !stack && (extends || ambiguous_memory_size(bits, &matches, &ops))
    {
        cx.error(
            req.span,
            format!("`{mnemonic}` needs the size of its memory operand, as in `dword ptr [...]`"),
        );
        return None;
    }
    // AT&T has no size keyword, so a vector instruction whose memory operand
    // could be any of several lengths — the narrowing conversions, and
    // `vfpclassps` — is spelled with an `x`, `y` or `z` on the end instead.
    // Nothing else can be ambiguous there: an AT&T suffix already names the
    // width, and every other form is pinned by a register operand.
    if syntax == Syntax::Att
        && matches.iter().all(|d| d.enc != Enc::Legacy)
        && ambiguous_memory_size(bits, &matches, &ops)
    {
        cx.error(
            req.span,
            format!(
                "`{mnemonic}` needs the size of its memory operand, \
                 as in `{mnemonic}x` or `{mnemonic}y`"
            ),
        );
        return None;
    }
    // `.code16gcc` widens what GCC's 32-bit code expects of the stack: the
    // pushes and pops, calls and returns, and the frame instructions. Jumps
    // and `iret` stay 16-bit.
    let gcc16 = cx.state.features & CODE16GCC != 0
        && matches!(
            mnemonic,
            "push"
                | "pop"
                | "pushf"
                | "popf"
                | "pusha"
                | "popa"
                | "call"
                | "ret"
                | "lret"
                | "retf"
                | "enter"
                | "leave"
        );
    let matches = prefer_default_size(bits, gcc16, stack, matches, &resolved);
    // An instruction with only immediates is the mode's size when it has a
    // form of that size, and an immediate that does not fit that is not a
    // reason to pick another one: `push $0xffffffff` in 64-bit code is an
    // error, not a `pushw`, and so is `lret $0x10000` in 32-bit code. A size
    // keyword names the size outright, so there is no guess left to report.
    let default_size = default_operand_size(bits, gcc16, stack);
    if resolved.opsize.is_none()
        && !ops.is_empty()
        && ops
            .iter()
            .all(|o| matches!(o.kind, OperandKind::Imm(_)) && o.size_hint.is_none())
        && matches[0].opsize != default_size
        && matches[0].opsize != 0
        && resolved
            .defs
            .iter()
            .any(|d| d.opsize == default_size && d.ops.len() == ops.len() && available(cx, bits, d))
    {
        cx.error(
            req.span,
            format!("the immediate does not fit `{mnemonic}` at the {bits}-bit mode's size"),
        );
        return None;
    }
    let matches = prefer_evex_when_required(matches, &ops, rounding.is_some());
    let matches = prefer_apx_when_required(matches, &ops);

    // A relative branch gets one variant per displacement width, smallest
    // first, so the layout pass can shorten it once addresses are known.
    let is_rel = matches[0]
        .ops
        .first()
        .is_some_and(|o| matches!(o, Op::Rel(_)));
    let chosen: Vec<&Def> = if is_rel {
        let mut v: Vec<&Def> = matches
            .iter()
            .copied()
            .filter(|d| {
                d.opsize == matches[0].opsize
                    && d.ops.first().is_some_and(|o| matches!(o, Op::Rel(_)))
            })
            .collect();
        v.sort_by_key(|d| d.ops[0].width());
        v.dedup_by_key(|d| d.ops[0].width());
        // `short`, `near` and a size keyword pin the displacement instead,
        // leaving the layout pass nothing to choose; a width the instruction
        // has no form for is an error rather than the next size up, since the
        // source asked for that one. The wide row writes the operand size
        // rather than its own four bytes, so in 16-bit code it is the
        // two-byte form `near` and `word` both name.
        if let Some(w) = ops[0].size_hint {
            let written = |d: &Def| {
                let w = d.ops[0].width();
                if w == 4 && bits == 16 && d.opsize == 0 {
                    2
                } else {
                    w
                }
            };
            if v.len() == 1 && written(v[0]) == 1 {
                cx.error(
                    req.span,
                    format!(
                        "`{mnemonic}` has one branch form only, so it takes neither `short` nor `near`"
                    ),
                );
                return None;
            }
            if !v.iter().any(|d| written(d) == w) {
                cx.error(
                    req.span,
                    format!("no form of `{mnemonic}` has a {w}-byte branch displacement"),
                );
                return None;
            }
            v.retain(|d| written(d) == w);
        }
        v
    } else {
        vec![matches[0]]
    };

    let mut variants = Vec::with_capacity(chosen.len());
    for def in &chosen {
        variants.push(encode::encode(
            cx,
            encode::Target { bits, abi },
            def,
            &ops,
            prefixes,
            rounding,
            req.span,
        )?);
    }

    if !is_rel && chosen[0].enc == Enc::Vex {
        prefer_shorter_vex(
            cx,
            encode::Target { bits, abi },
            &matches,
            &ops,
            prefixes,
            &mut variants[0],
        );
    }
    Some(variants)
}

/// Narrows the candidates to EVEX forms when the operands need one.
///
/// Where AVX and AVX-512 both define an instruction, the VEX row comes first
/// and wins for plain operands, as it does in both reference assemblers. But
/// `xmm16`, a writemask, a broadcast or a rounding mode can only be carried by
/// EVEX; the VEX row still *matches* those operands by class, so it is set
/// aside here rather than left to fail during encoding.
///
/// When no EVEX form matched, the list is left alone, so the encoder can
/// explain what went wrong with the form that was closest.
fn prefer_evex_when_required<'d>(
    matches: Vec<&'d Def>,
    ops: &[Operand],
    rounding: bool,
) -> Vec<&'d Def> {
    // A general register's fourth number bit is APX's business, not the
    // vector file's, and REX2 carries it on a legacy row.
    let high = |r: &reg::Reg| r.needs_evex_ext() && !r.is_gpr();
    let needs_evex = rounding
        || ops.iter().any(|o| {
            !o.decor.is_empty()
                || match &o.kind {
                    OperandKind::Reg(r) => high(r),
                    OperandKind::Mem(m) => m.index.as_ref().is_some_and(high),
                    _ => false,
                }
        });
    if !needs_evex || !matches.iter().any(|d| d.enc == Enc::Evex) {
        return matches;
    }
    matches.into_iter().filter(|d| d.enc == Enc::Evex).collect()
}

/// Narrows the candidates to the forms that can carry an `r16`-`r31` operand.
///
/// Such an operand is encodable either way where the instruction has a REX2
/// encoding, and GNU as prefers that: `addq %r16, %rbx` is four bytes under
/// REX2 and six under the extended EVEX prefix. Where the preferred row is out
/// of REX2's reach -- the `0F 38` map, or a VEX row, neither of which REX2
/// covers -- only an EVEX row is left, promoted or AVX-512's own.
fn prefer_apx_when_required<'d>(matches: Vec<&'d Def>, ops: &[Operand]) -> Vec<&'d Def> {
    let egpr = |r: &reg::Reg| r.needs_rex2();
    let needs = ops.iter().any(|o| match &o.kind {
        OperandKind::Reg(r) => egpr(r),
        OperandKind::Mem(m) => {
            m.base.as_ref().is_some_and(egpr) || m.index.as_ref().is_some_and(egpr)
        }
        OperandKind::Indirect(inner) => match &**inner {
            OperandKind::Reg(r) => egpr(r),
            OperandKind::Mem(m) => {
                m.base.as_ref().is_some_and(egpr) || m.index.as_ref().is_some_and(egpr)
            }
            _ => false,
        },
        _ => false,
    });
    if !needs || matches[0].egpr_ok() || !matches.iter().any(|d| d.egpr_ok()) {
        return matches;
    }
    matches.into_iter().filter(|d| d.egpr_ok()).collect()
}

/// True if a `{rn-sae}` or `{sae}` operand at `pos` (in Intel order) is where
/// GNU as accepts one.
///
/// In AT&T syntax it comes first, or after one immediate or one general
/// register (`vcvtsi2sd %rax, {rz-sae}, %xmm1, %xmm2`), and a general register
/// cannot be the first register written after it. Intel syntax writes it
/// after the register and memory operands and before any immediate, with at
/// most one general register behind it. llvm-mc is stricter in Intel syntax,
/// where it wants the mirror of the AT&T order.
fn rounding_in_place(ops: &[Operand], pos: usize, syntax: Syntax) -> bool {
    let gpr = |o: &Operand| o.reg().is_some_and(|r| r.is_gpr());
    let imm = |o: &Operand| matches!(o.kind, OperandKind::Imm(_));
    let (before, after) = (&ops[..pos], &ops[pos + 1..]);
    if before.iter().any(imm) {
        return false;
    }
    match syntax {
        Syntax::Att => {
            // `after` is what the source wrote before the decorator.
            let last_reg = ops.iter().rposition(|o| o.reg().is_some());
            after.len() <= 1
                && after.iter().all(|o| imm(o) || gpr(o))
                && !last_reg.is_some_and(|i| i < pos && gpr(&ops[i]))
        }
        Syntax::Intel => {
            after.iter().all(|o| imm(o) || gpr(o)) && after.iter().filter(|o| gpr(o)).count() <= 1
        }
    }
}

/// Swaps in a later VEX form when it encodes shorter than the preferred one.
///
/// A register-to-register `vmovaps` can be written with the load opcode or the
/// store opcode, and the two put the source register in different ModRM
/// fields. When the source is `xmm8`-`xmm15` and the destination is not, only
/// the store opcode lets the extension bit ride in `R`, which the two-byte VEX
/// prefix has, rather than `B`, which it does not. GNU as and llvm-mc both
/// make that choice, so rsasm does too.
///
/// llvm-mc also swaps the two sources of a commutative operation such as
/// `vaddps` for the same reason. GNU as does not, and rsasm follows GNU as.
fn prefer_shorter_vex(
    cx: &mut AsmCtx<'_>,
    target: encode::Target,
    matches: &[&Def],
    ops: &[Operand],
    prefixes: Prefixes,
    best: &mut Variant,
) {
    // Only register operands can land in either field; with memory involved
    // the forms are not interchangeable.
    if !ops
        .iter()
        .all(|o| o.reg().is_some() || matches!(o.kind, OperandKind::Imm(_)))
    {
        return;
    }
    let first = matches[0];
    for alt in matches.iter().skip(1) {
        if alt.enc != Enc::Vex || alt.vlen != first.vlen {
            continue;
        }
        // An alternative that cannot be encoded is simply not a candidate, so
        // whatever it would have reported is discarded.
        let mark = cx.diags.len();
        let v = encode::encode(cx, target, alt, ops, prefixes, None, Span::DUMMY);
        truncate_diags(cx, mark);
        if let Some(v) = v
            && v.bytes.len() < best.bytes.len()
        {
            *best = v;
        }
    }
}

/// Drops every diagnostic recorded after the first `len`.
fn truncate_diags(cx: &mut AsmCtx<'_>, len: usize) {
    if cx.diags.len() <= len {
        return;
    }
    let kept: Vec<_> = cx.diags.take().into_iter().take(len).collect();
    for d in kept {
        cx.diags.emit(d);
    }
}

/// What a mnemonic resolved to, including any width implied by an AT&T suffix.
struct Resolved {
    defs: &'static [Def],
    /// Required `Def::opsize`, from a suffix such as the `l` in `movl`.
    opsize: Option<u8>,
    /// Required width of the r/m operand, for `movzbl`-style double suffixes.
    rm_width: Option<u8>,
    /// Rows to try, unconstrained, when nothing in `defs` matched. See the
    /// note on `movq` in `resolve_mnemonic`.
    fallback: &'static [Def],
    /// The suffix is on `call`, where it can name the mode's size.
    branch: bool,
}

fn suffix_width(c: u8) -> Option<u8> {
    Some(match c {
        b'b' => 1,
        b'w' => 2,
        b'l' => 4,
        b'q' => 8,
        _ => return None,
    })
}

fn resolve_mnemonic(mnemonic: &str, syntax: Syntax) -> Option<Resolved> {
    // `movq` names two instructions in AT&T syntax: `mov` with a `q` suffix,
    // and the MMX/SSE quadword move. The GPR reading is tried first, as GNU as
    // does, and the vector rows only if it matched nothing. Intel syntax has no
    // size suffixes, so there `movq` is only the vector instruction and the
    // ordinary exact lookup below handles it.
    if syntax == Syntax::Att
        && mnemonic == "movq"
        && let (Some(defs), Some(vector)) = (insn::lookup("mov"), insn::lookup("movq"))
    {
        return Some(Resolved {
            defs,
            opsize: Some(64),
            rm_width: None,
            fallback: vector,
            branch: false,
        });
    }

    // An exact table entry always wins, so the string instruction `movsb` is
    // never mistaken for `movs` with a `b` suffix.
    if let Some(defs) = insn::lookup(mnemonic) {
        return Some(Resolved {
            defs,
            opsize: None,
            rm_width: None,
            fallback: &[],
            branch: false,
        });
    }
    if syntax != Syntax::Att {
        return None;
    }
    let b = mnemonic.as_bytes();

    // `movzbl`, `movswq`, `movslq`: source width then destination width.
    if b.len() == 6
        && (mnemonic.starts_with("movz") || mnemonic.starts_with("movs"))
        && let (Some(src), Some(dst)) = (suffix_width(b[4]), suffix_width(b[5]))
        && src < dst
    {
        let base = if mnemonic.starts_with("movz") {
            "movzx"
        } else if src == 4 {
            // 32-to-64 sign extension has its own opcode.
            "movsxd"
        } else {
            "movsx"
        };
        if let Some(defs) = insn::lookup(base) {
            return Some(Resolved {
                defs,
                opsize: Some(dst * 8),
                rm_width: Some(src),
                fallback: &[],
                branch: false,
            });
        }
    }

    // A single trailing size letter. Only an ASCII byte can be one, which is
    // also what makes `len - 1` a safe place to split.
    let last = *mnemonic.as_bytes().last()?;
    if !last.is_ascii() {
        return None;
    }
    let w = suffix_width(last)?;
    let stem = &mnemonic[..mnemonic.len() - 1];
    let defs = insn::lookup(stem)?;
    // `crc32b` and its siblings are already suffixed, and so is `movq`.
    if stem.starts_with("crc32") || stem == "movq" {
        return None;
    }
    // On the Intel-style names of the extending moves, GNU as reads the
    // suffix as the width of the source: `movsxb %al, %ecx`.
    if matches!(stem, "movzx" | "movsx") {
        return Some(Resolved {
            defs,
            opsize: None,
            rm_width: Some(w),
            fallback: &[],
            branch: false,
        });
    }
    // Only an instruction that comes in more than one size takes a suffix:
    // `cwtl` and `lodsl` already name theirs, so `cwtll` is no instruction.
    // An AVX-512 row's size is its `EVEX.W`, which no suffix selects, unless
    // the row's `W` sizes a general register, as `vcvtusi2sd`'s does.
    let gpr_sized = |d: &Def| {
        d.ops
            .iter()
            .any(|o| matches!(o, Op::Rm(x) | Op::R(x) if *x == w))
    };
    if defs.iter().all(|d| d.opsize == defs[0].opsize)
        || !defs
            .iter()
            .any(|d| d.opsize == w * 8 && (d.enc != Enc::Evex || gpr_sized(d)))
    {
        return None;
    }
    Some(Resolved {
        defs,
        opsize: Some(w * 8),
        rm_width: None,
        fallback: &[],
        branch: stem == "call",
    })
}

/// True if `def` exists in the current mode and syntax.
fn available(cx: &AsmCtx<'_>, bits: u8, def: &Def) -> bool {
    let mode_ok = if bits == 64 {
        def.flags & NO64 == 0
    } else {
        def.flags & ONLY64 == 0
    };
    let syntax_ok = match cx.state.syntax {
        Syntax::Att => def.flags & INTEL_ONLY == 0,
        Syntax::Intel => def.flags & ATT_ONLY == 0,
    };
    mode_ok && syntax_ok
}

/// Every definition that accepts `ops`, in table (preference) order.
fn select<'d>(
    cx: &mut AsmCtx<'_>,
    bits: u8,
    defs: &'d [Def],
    resolved: &Resolved,
    ops: &[Operand],
) -> Vec<&'d Def> {
    let mut out = Vec::new();
    for def in defs {
        if def.ops.len() != ops.len() || !available(cx, bits, def) {
            continue;
        }
        if let Some(want) = resolved.opsize
            && def.opsize != want
        {
            continue;
        }
        if let Some(want) = resolved.rm_width {
            let rm = def.ops.iter().find_map(|o| match o {
                Op::Rm(w) | Op::M(w) => Some(*w),
                _ => None,
            });
            if rm != Some(want) {
                continue;
            }
        }
        if def.flags & NOTACC != 0 && all_accumulator(ops) {
            continue;
        }
        if def
            .ops
            .iter()
            .zip(ops)
            .all(|(p, o)| op_matches(cx, bits, def, p, o))
        {
            out.push(def);
        }
    }
    // A relative `call` has no operand size of its own to match, but takes
    // the suffix of the mode's: `callq` in 64-bit code. Any other suffix on
    // an instruction without sizes is refused, as GNU as refuses it, and so
    // is any suffix on a direct `jmp`.
    let branch_suffix = matches!((resolved.opsize, bits), (Some(32), 32) | (Some(64), 64));
    if out.is_empty() && resolved.branch && branch_suffix {
        for def in defs {
            if def.ops.len() == ops.len()
                && available(cx, bits, def)
                && matches!(def.ops.first(), Some(Op::Rel(_)))
                && def
                    .ops
                    .iter()
                    .zip(ops)
                    .all(|(p, o)| op_matches(cx, bits, def, p, o))
            {
                out.push(def);
            }
        }
    }
    if out.is_empty() {
        for def in resolved.fallback {
            if def.ops.len() == ops.len()
                && available(cx, bits, def)
                && def
                    .ops
                    .iter()
                    .zip(ops)
                    .all(|(p, o)| op_matches(cx, bits, def, p, o))
            {
                out.push(def);
            }
        }
    }
    prefer_imm_size(out, ops)
}

/// Narrows a match list to the operation size a size keyword on an immediate
/// asked for.
///
/// NASM reads `dword` before an immediate as the size of the operation rather
/// than the width of the field, so `push dword 0x20` in 16-bit code is the
/// one-byte-immediate form under an operand-size prefix, `66 6a 20`. That
/// reading only holds where the field follows the operation: `ret` counts its
/// bytes in sixteen bits whatever the mode, so `ret word 4` names that field
/// and leaves the operand size alone. A keyword whose size no remaining row
/// has is left alone too, as `mov rax, dword 1` is.
fn prefer_imm_size<'d>(matches: Vec<&'d Def>, ops: &[Operand]) -> Vec<&'d Def> {
    let Some(hint) = ops.iter().find_map(|o| match o.kind {
        OperandKind::Imm(_) => o.size_hint,
        _ => None,
    }) else {
        return matches;
    };
    let imm_width = |d: &Def| {
        d.ops
            .iter()
            .find(|o| matches!(o, Op::Imm(_) | Op::Imm8s))
            .map(|o| o.width())
    };
    let want = u16::from(hint) * 8;
    if matches
        .iter()
        .all(|d| imm_width(d) == imm_width(matches[0]))
        || !matches.iter().any(|d| u16::from(d.opsize) == want)
    {
        return matches;
    }
    let mut matches = matches;
    matches.retain(|d| u16::from(d.opsize) == want);
    matches
}

/// True when every operand is a register and all of them are the accumulator.
fn all_accumulator(ops: &[Operand]) -> bool {
    !ops.is_empty()
        && ops
            .iter()
            .all(|o| o.reg().is_some_and(|r| r.is_gpr() && r.num == 0))
}

fn fits_unsigned_or_signed(v: i64, width: u8) -> bool {
    match width {
        1 => (-128..=255).contains(&v),
        2 => (-32768..=65535).contains(&v),
        4 => (-(1i64 << 31)..=(1i64 << 32) - 1).contains(&v),
        _ => true,
    }
}

fn op_matches(cx: &mut AsmCtx<'_>, bits: u8, def: &Def, pat: &Op, o: &Operand) -> bool {
    match *pat {
        Op::R(w) => o.reg().is_some_and(|r| r.is_gpr() && r.size == w),
        Op::Rm(w) => match &o.kind {
            OperandKind::Reg(r) => r.is_gpr() && r.size == w,
            OperandKind::Mem(_) => o.size_hint.is_none_or(|h| h == w),
            _ => false,
        },
        Op::M(w) => {
            matches!(o.kind, OperandKind::Mem(_)) && (w == 0 || o.size_hint.is_none_or(|h| h == w))
        }
        Op::Imm(w) => {
            let OperandKind::Imm(e) = &o.kind else {
                return false;
            };
            match cx.constant(*e) {
                Some(v) => {
                    if w == 4 && def.opsize == 64 {
                        // A 32-bit immediate in a 64-bit operation is
                        // sign-extended to 64 bits, so it must fit as signed.
                        (-(1i64 << 31)..(1i64 << 31)).contains(&v)
                    } else if w * 8 == def.opsize {
                        // An immediate as wide as the operation is truncated
                        // to it: GNU as warns and llvm-mc agrees on the
                        // bytes, so `movb $0x100, %al` stores zero. A narrower
                        // field, like a shift count or the frame size of a
                        // 32-bit `enter`, has to hold the value.
                        true
                    } else {
                        fits_unsigned_or_signed(v, w)
                    }
                }
                // A symbolic value is relocated at whatever width the field
                // has.
                None => true,
            }
        }
        Op::Imm8s => {
            let OperandKind::Imm(e) = &o.kind else {
                return false;
            };
            // A value that fits the operation's size is taken at that size
            // first, so a 16-bit `0xffff` is the -1 that fits a sign-extended
            // byte. GNU as also reads anything that fits 32 bits as a 32-bit
            // value, so `0xffffffff` is -1 to a 16-bit operation too.
            cx.constant(*e).is_some_and(|v| {
                let v = match def.opsize {
                    16 if (0..=0xffff).contains(&v) => v as i16 as i64,
                    16 | 32 if (0..=0xffff_ffff).contains(&v) => v as i32 as i64,
                    _ => v,
                };
                (-128..=127).contains(&v)
            })
        }
        Op::One | Op::Three => {
            let OperandKind::Imm(e) = &o.kind else {
                return false;
            };
            // NASM writes `int 3` as the two bytes it asks for.
            let nasm_int3 = *pat == Op::Three && cx.dialect == crate::lexer::Dialect::Nasm;
            !nasm_int3 && cx.constant(*e) == Some(if *pat == Op::One { 1 } else { 3 })
        }
        Op::V(k) | Op::Nds(k) | Op::Is4(k) => o.reg().is_some_and(|r| k.accepts(r)),
        Op::NdsR(w) => o.reg().is_some_and(|r| r.is_gpr() && r.size == w),
        Op::Vm(k, msz) => match &o.kind {
            OperandKind::Reg(r) => k.accepts(*r),
            OperandKind::Mem(_) => {
                // Under `{1toN}` an Intel size keyword names the element being
                // broadcast, not the vector.
                let w = if o.decor.broadcast.is_some() {
                    match def.tuple {
                        insn::Tuple::Hv => 4,
                        insn::Tuple::Fvw | insn::Tuple::Hvw | insn::Tuple::Qvw => 2,
                        _ if def.vex_w() => 8,
                        _ => 4,
                    }
                } else if msz == 0 {
                    k.width()
                } else {
                    msz
                };
                o.size_hint.is_none_or(|h| h == w)
            }
            _ => false,
        },
        // A vector index must be of the class this row expects, since that is
        // what tells a 128-bit gather from a 256-bit one with the same
        // destination. A GPR index or none at all is let through so the
        // encoder can say what is missing.
        Op::Vsib(k) => match &o.kind {
            OperandKind::Mem(m) => m.index.is_none_or(|i| !i.is_vector() || k.accepts(i)),
            _ => false,
        },
        Op::Fixed(name) => o.reg() == reg::lookup(name),
        Op::Seg => o.reg().is_some_and(|r| r.class == RegClass::Segment),
        Op::Cr => o.reg().is_some_and(|r| r.class == RegClass::Control),
        Op::Dr => o.reg().is_some_and(|r| r.class == RegClass::Debug),
        Op::St => o.reg().is_some_and(|r| r.class == RegClass::St),
        Op::Moffs(w) => match &o.kind {
            OperandKind::Mem(m) => {
                // GNU as keeps a `@GOT` load in ModRM form, which is the
                // one a linker knows how to relax.
                m.base.is_none()
                    && m.index.is_none()
                    && !m.rip_relative
                    && m.disp
                        .is_some_and(|e| encode::modifier(cx, e).as_deref() != Some("got"))
                    && o.size_hint.is_none_or(|h| h == w)
            }
            _ => false,
        },
        Op::Far => matches!(o.kind, OperandKind::FarPtr { .. }),
        // Neither reference checks that a string operand names `si` or `di`:
        // the registers only give the address size.
        Op::StrSrc(w) | Op::StrDst(w) => o.is_mem() && o.size_hint.is_none_or(|h| h == w),
        Op::FarM | Op::Fword | Op::FarDword => {
            let is_mem = match &o.kind {
                OperandKind::Mem(_) => true,
                OperandKind::Indirect(inner) => matches!(**inner, OperandKind::Mem(_)),
                _ => false,
            };
            is_mem
                && match *pat {
                    Op::Fword => o.size_hint == Some(6),
                    Op::FarDword => {
                        o.size_hint == Some(4) && bits != 32 && cx.state.syntax == Syntax::Intel
                    }
                    _ => o.size_hint.is_none_or(|h| h == 6),
                }
        }
        Op::Dx => match &o.kind {
            OperandKind::Reg(r) => *r == reg::lookup("dx").expect("dx is a register"),
            // AT&T also spells the port `(%dx)`.
            OperandKind::Mem(m) => {
                m.base == reg::lookup("dx")
                    && m.index.is_none()
                    && m.disp.is_none()
                    && m.seg.is_none()
            }
            _ => false,
        },
        Op::Rel(_) => encode::rel_expr(o).is_some(),
        Op::IndirectRm(w) => {
            // AT&T marks indirect branches with `*`; Intel does not.
            // Without it GNU as still reads an address in parentheses as
            // one, with a warning, but a bare address is a direct target.
            let explicit = matches!(o.kind, OperandKind::Indirect(_));
            let bracketed = matches!(&o.kind, OperandKind::Mem(m) if m.bracketed);
            if !explicit && cx.state.syntax == Syntax::Att && !bracketed {
                return false;
            }
            match encode::indirect_inner(o) {
                Some(inner) => match inner.kind {
                    OperandKind::Reg(r) => r.is_gpr() && r.size == w,
                    OperandKind::Mem(_) => {
                        let _ = bits;
                        o.size_hint.is_none_or(|h| h == w)
                    }
                    _ => false,
                },
                None => false,
            }
        }
    }
}

/// Resolves an operand size the source never pinned down.
///
/// `mov $1, (%rax)` could store one, two, four or eight bytes, and `push $1`
/// or `pushf` could push two, four or eight. GNU as picks the mode's default
/// operand size — 16 bits in 16-bit mode, 32 in 32- and 64-bit mode, except
/// for the stack and branch instructions long mode widened to 64 — and
/// existing sources rely on that, so rsasm does the same rather than
/// rejecting the line. NASM-dialect input should insist on an explicit size
/// instead; that belongs with the NASM front end, not here.
///
/// `.code16gcc` makes the stack instructions default to 32 bits as well.
fn prefer_default_size<'d>(
    bits: u8,
    gcc16: bool,
    stack: bool,
    matches: Vec<&'d Def>,
    resolved: &Resolved,
) -> Vec<&'d Def> {
    let first = matches[0];
    // A row with no operand size, such as a relative branch that also reads
    // as an indirect one through memory, is preferred as it stands, except
    // where `.code16gcc` asks for a 32-bit call.
    if resolved.opsize.is_some()
        || first.opsize == 0 && !(gcc16 && bits == 16)
        || matches.iter().all(|d| d.opsize == first.opsize)
    {
        return matches;
    }
    let default_size = default_operand_size(bits, gcc16, stack);
    let mut matches = matches;
    if let Some(pos) = matches.iter().position(|d| d.opsize == default_size) {
        let chosen = matches.remove(pos);
        matches.insert(0, chosen);
    }
    matches
}

/// The operand size an instruction has when nothing in the source names one.
fn default_operand_size(bits: u8, gcc16: bool, stack: bool) -> u8 {
    match bits {
        64 if stack => 64,
        16 if !gcc16 => 16,
        _ => 32,
    }
}

/// Folds the label differences in an operand's expressions; see
/// [`crate::arch::fold_differences`].
fn fold_operand(cx: &mut AsmCtx<'_>, o: &mut Operand) {
    let fold = |cx: &mut AsmCtx<'_>, e: &mut ExprRef| {
        if cx.constant(*e).is_none() {
            *e = crate::arch::fold_differences(cx, *e);
        }
    };
    match &mut o.kind {
        OperandKind::Imm(e) | OperandKind::Rel(e) => fold(cx, e),
        OperandKind::Mem(m) => {
            if let Some(e) = &mut m.disp {
                fold(cx, e);
            }
        }
        OperandKind::FarPtr { seg, off } => {
            fold(cx, seg);
            fold(cx, off);
        }
        _ => {}
    }
}

/// True when an unsized memory operand leaves more than one width possible.
///
/// GNU as's Intel syntax refuses `add [eax], 1` and `fld [eax]` for want of a
/// `dword ptr`, where its AT&T syntax only warns and takes a default.
fn ambiguous_memory_size(bits: u8, matches: &[&Def], ops: &[Operand]) -> bool {
    let Some(slot) = ops.iter().position(|o| o.is_mem() && o.size_hint.is_none()) else {
        return false;
    };
    let width = |d: &Def| match d.ops[slot] {
        Op::Rm(w) | Op::M(w) | Op::Moffs(w) | Op::IndirectRm(w) | Op::StrSrc(w) | Op::StrDst(w) => {
            w
        }
        v @ Op::Vm(..) => v.width(),
        _ => 0,
    };
    // A 64-bit operation outside long mode is no rival.
    let mut usable = matches.iter().filter(|d| {
        // A VEX or EVEX `W1` is no 64-bit operation unless it sizes a
        // general register, as `vcvtsi2sd`'s does.
        let vector_w =
            d.enc != Enc::Legacy && !d.ops.iter().any(|o| matches!(o, Op::Rm(_) | Op::R(_)));
        bits == 64 || d.opsize != 64 || d.flags & DEF64 != 0 || vector_w
    });
    let Some(first) = usable.next().map(|d| width(d)) else {
        return false;
    };
    usable.any(|d| width(d) != first)
}

fn report_no_match(
    cx: &mut AsmCtx<'_>,
    req: &InsnRequest<'_>,
    mnemonic: &str,
    resolved: &Resolved,
    ops: &[Operand],
) {
    let defs = resolved.defs;
    let bits = cx.state.bits;
    // Something that would have matched in another mode or syntax is worth
    // saying so about.
    let elsewhere = defs.iter().find(|def| {
        def.ops.len() == ops.len()
            && !available(cx, bits, def)
            && resolved.opsize.is_none_or(|w| def.opsize == w)
            && def
                .ops
                .iter()
                .zip(ops)
                .all(|(p, o)| op_matches(cx, bits, def, p, o))
    });
    if let Some(def) = elsewhere {
        let msg = if def.flags & NO64 != 0 && bits == 64 {
            format!("`{mnemonic}` with these operands is not available in 64-bit mode")
        } else if def.flags & ONLY64 != 0 && bits != 64 {
            format!("`{mnemonic}` with these operands is only available in 64-bit mode")
        } else {
            format!("`{mnemonic}` with these operands is not available in this syntax")
        };
        cx.error(req.span, msg);
        return;
    }
    let arities: Vec<usize> = {
        let mut v: Vec<usize> = defs.iter().map(|d| d.ops.len()).collect();
        v.sort_unstable();
        v.dedup();
        v
    };
    if !arities.contains(&ops.len()) {
        let want: Vec<String> = arities.iter().map(|n| n.to_string()).collect();
        cx.error(
            req.span,
            format!(
                "`{mnemonic}` takes {} operand(s), but {} were given",
                want.join(" or "),
                ops.len()
            ),
        );
        return;
    }
    let described: Vec<String> = ops.iter().map(|o| o.describe()).collect();
    cx.error(
        req.span,
        format!("no form of `{mnemonic}` accepts {}", described.join(", ")),
    );
}

/// Builds the accumulator-to-memory `mov` NASM prefers, `A0`-`A3` with the
/// address as a `moffs` of the current address size, if the operands fit that
/// shape: the accumulator and a bare-displacement memory operand with no base,
/// index or RIP. Returns `None` — building nothing — otherwise, so the caller
/// falls back to the ordinary encoding.
fn try_moffs(
    cx: &mut AsmCtx<'_>,
    bits: u8,
    abi: reloc::Abi,
    ops: &[Operand],
    span: Span,
) -> Option<crate::section::Variant> {
    use crate::section::{Fixup, FixupKind, Variant};
    let [a, b] = ops else { return None };
    // One operand is the accumulator, the other bare-displacement memory.
    let acc = a.reg().or_else(|| b.reg())?;
    if !(acc.is_gpr() && acc.num == 0) {
        return None;
    }
    let (mem_op, load) = match (&a.kind, &b.kind) {
        (OperandKind::Reg(_), OperandKind::Mem(m)) => (m, true),
        (OperandKind::Mem(m), OperandKind::Reg(_)) => (m, false),
        _ => return None,
    };
    if mem_op.base.is_some() || mem_op.index.is_some() || mem_op.rip_relative {
        return None;
    }
    // In long mode NASM uses `moffs` only for a genuine 64-bit address, not
    // for a symbol or a short constant, so the accumulator shortcut is a
    // 16-/32-bit affair here.
    if bits == 64 {
        return None;
    }
    let disp = mem_op.disp?;
    // moffs holds the whole address, in the mode's address size; a size
    // override would need a ModRM form, so this only fires at the native size.
    let addr_size = match bits {
        64 => 8u8,
        32 => 4,
        _ => 2,
    };
    let mut bytes = Vec::new();
    if let Some(seg) = mem_op.seg {
        bytes.push(encode::segment_prefix(seg)?);
    }
    // Operand-size prefix for a 16-bit accumulator outside 16-bit mode, or a
    // 32-bit one within it; REX.W for the 64-bit accumulator.
    if (acc.size == 2 && bits != 16) || (acc.size == 4 && bits == 16) {
        bytes.push(0x66);
    }
    if acc.size == 8 {
        bytes.push(0x48);
    }
    let opcode = match (acc.size == 1, load) {
        (true, true) => 0xa0,
        (true, false) => 0xa2,
        (false, true) => 0xa1,
        (false, false) => 0xa3,
    };
    bytes.push(opcode);
    let offset = bytes.len() as u32;
    let mut fixups = Vec::new();
    match cx.constant(disp) {
        Some(v) => bytes.extend_from_slice(&(v as u64).to_le_bytes()[..addr_size as usize]),
        None => {
            bytes.extend(std::iter::repeat_n(0u8, addr_size as usize));
            let reloc = cx
                .find_modifier_for(disp)
                .and_then(|m| {
                    // A `wrt ..got`/`..sym` on the address picks its own type.
                    let name = cx.name(m).to_string();
                    reloc_moffs_modifier(abi, &name, addr_size)
                })
                .or_else(|| abi.abs(addr_size))
                .unwrap_or(0);
            fixups.push(Fixup {
                offset,
                expr: disp,
                kind: FixupKind::data(addr_size).with_reloc(reloc),
                span: cx.exprs.span(disp),
            });
        }
    }
    let _ = span;
    Some(Variant { bytes, fixups })
}

/// The relocation a `wrt` modifier on a moffs address selects.
fn reloc_moffs_modifier(abi: reloc::Abi, name: &str, size: u8) -> Option<u32> {
    match name {
        "got" => abi.got(size, false),
        "gotoff" => abi.gotoff(size),
        "sym" => abi.abs(size),
        _ => None,
    }
}

/// True if `name` is a register, used by the generic parser to avoid treating
/// register names as symbols.
#[allow(dead_code)]
pub fn is_register(name: &str) -> bool {
    reg::is_register(name)
}

/// Convenience for tests and for the `--print-encoding` debug output.
#[allow(dead_code)]
pub fn describe_span(span: Span) -> String {
    format!("{span:?}")
}
