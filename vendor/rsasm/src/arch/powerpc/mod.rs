//! PowerPC, 32- and 64-bit, big and little endian. `EM_PPC` / `EM_PPC64`.
//!
//! Three targets share one backend, differing only in pointer width and byte
//! order: `powerpc` (32-bit big endian), `powerpc64` (64-bit big endian) and
//! `powerpc64le`. The instruction encodings are identical in all three — a
//! PowerPC instruction word is the same 32 bits whichever way round it is
//! written to memory — so the only thing byte order changes is [`Endian`], and
//! everything the core writes goes through that.
//!
//! Two things about PowerPC assembly shape the code here. First, register
//! operands are conventionally bare numbers: nothing in `add 3, 4, 5` says
//! which `3` is a register, so operands are parsed without classification and
//! read as whatever field the instruction form asks for (see
//! [`operand`]). Second, a large fraction of real PowerPC assembly is written
//! in extended mnemonics — `li`, `mr`, `slwi`, `beq` — that are aliases for
//! awkward base instructions. Those are table entries in their own right
//! rather than a rewriting pass, so a bad operand is reported against the
//! mnemonic the programmer actually wrote.

pub mod encode;
pub mod insn;
pub mod operand;
pub mod reg;
pub mod reloc;
pub mod vector;

use crate::arch::{
    ArchState, Architecture, AsmCtx, Endian, FlatModifier, InsnRequest, ModifierSymbols, Request,
    Syntax,
};
use crate::dwarf::{CfiTarget, DwarfTarget, Flavor, cfi, numbered_register};
use crate::lexer::Punct;
use crate::section::Variant;

pub const NAMES: &[&str] = &["powerpc", "powerpc64", "powerpc64le"];

pub fn lookup(name: &str) -> Option<Box<dyn Architecture>> {
    let target = match name {
        "powerpc" | "ppc" | "powerpc32" | "ppc32" => Target::Ppc32,
        "powerpc64" | "ppc64" => Target::Ppc64,
        "powerpc64le" | "ppc64le" | "powerpcle" => Target::Ppc64Le,
        _ => return None,
    };
    Some(Box::new(PowerPc { target }))
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum Target {
    Ppc32,
    Ppc64,
    Ppc64Le,
}

pub struct PowerPc {
    target: Target,
}

impl PowerPc {
    fn bits(&self) -> u8 {
        match self.target {
            Target::Ppc32 => 32,
            _ => 64,
        }
    }
}

impl Architecture for PowerPc {
    fn name(&self) -> &'static str {
        match self.target {
            Target::Ppc32 => "powerpc",
            Target::Ppc64 => "powerpc64",
            Target::Ppc64Le => "powerpc64le",
        }
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["ppc", "ppc32", "ppc64", "ppc64le", "powerpc32", "powerpcle"]
    }

    fn endian(&self) -> Endian {
        match self.target {
            Target::Ppc64Le => Endian::Little,
            _ => Endian::Big,
        }
    }

    fn pointer_bytes(&self, state: &ArchState) -> u8 {
        state.bits / 8
    }

    fn initial_state(&self) -> ArchState {
        ArchState {
            bits: self.bits(),
            syntax: Syntax::Att,
            features: 0,
            cpu_features: [0; 3],
            intel_register_prefix: false,
            used: 0,
            private: 0,
        }
    }

    /// PowerPC has one operand syntax. Refusing Intel here keeps an
    /// `.intel_syntax` left over from an x86 section from carrying across a
    /// `.arch` switch.
    fn supports_syntax(&self, syntax: Syntax) -> bool {
        syntax == Syntax::Att
    }

    fn elf_machine(&self) -> u16 {
        match self.target {
            Target::Ppc32 => 20, // EM_PPC
            _ => 21,             // EM_PPC64
        }
    }

    fn align_is_log2(&self) -> bool {
        true
    }

    /// `ppc_elf_lcomm`: an alignment of 8 unless the third operand gives
    /// another, and no symbol type. llvm-mc reads the operand too, but packs
    /// `.bss` without one and types the symbol `STT_OBJECT`.
    fn local_common(&self) -> crate::arch::LocalCommon {
        crate::arch::LocalCommon {
            align: |_| 8,
            takes_align: true,
            ty: crate::symbol::SymType::NoType,
        }
    }

    /// llvm-mc, the reference, aligns `.text` to 4 bytes; GNU as leaves it
    /// at 1.
    fn section_align(
        &self,
        _state: &ArchState,
        name: &str,
        _flags: &crate::section::SectionFlags,
    ) -> u64 {
        if name == ".text" { 4 } else { 1 }
    }

    fn data_reloc(&self, size: u8, pcrel: bool) -> Option<u32> {
        reloc::data(size, pcrel, self.bits() == 64)
    }

    /// Only the three thread-local data relocations; every other modifier
    /// in a data directive is refused. GNU as also puts a halfword
    /// relocation two bytes into a `.long x@tprel@l`, and a marker on a
    /// `.long x@tls`, and llvm-mc writes a plain address for both, so no
    /// answer would agree with the two.
    fn modifier_reloc(&self, name: &str, size: u8, pcrel: bool) -> Option<u32> {
        reloc::tls_data(name, size, pcrel, self.bits() == 64)
    }

    /// An instruction's relocation is always the one its encoder chose, from
    /// the whole chain of modifiers and the form of the field; the core,
    /// which sees only the last modifier of a chain, would read the `@tprel`
    /// of `paddi 3, 13, x@tprel, 0` as the eight-byte data relocation.
    fn fixup_modifier_reloc(
        &self,
        _name: &str,
        _kind: &crate::section::FixupKind,
    ) -> crate::arch::FixupModifier {
        crate::arch::FixupModifier::Encoded
    }

    /// Every thread-local modifier makes its target `STT_TLS`, in both
    /// references, except the markers: GNU as leaves the symbol named in
    /// `add 3, 3, x@tls` or `bl __tls_get_addr(x@tlsgd)` as it was, where
    /// llvm-mc makes it thread-local too.
    fn modifier_symbols(&self, name: &str) -> ModifierSymbols {
        let mut parts = name.split('@');
        let first = parts.next().unwrap_or_default();
        let tls = match first {
            "tprel" | "dtprel" | "dtpmod" => true,
            "got" => matches!(parts.next(), Some("tprel" | "dtprel" | "tlsgd" | "tlsld")),
            _ => false,
        };
        ModifierSymbols {
            tls,
            ..ModifierSymbols::default()
        }
    }

    /// `@pcrel` only says the field is relative to the instruction, which a
    /// flat image resolves as it would any PC-relative field, and a flat
    /// image has no PLT for `@plt` to go through or to keep `@local` out of,
    /// so both are the branch they are written on.
    fn flat_modifier(&self, name: &str) -> FlatModifier {
        match name {
            "pcrel" => FlatModifier::Plain,
            "plt" | "local" => FlatModifier::PcRelative,
            _ => FlatModifier::LinkerOnly,
        }
    }

    /// llvm-mc's conventions, as for every PowerPC encoding: addresses in
    /// the line table and CFA advances counted in four-byte instructions.
    fn dwarf(&self, _state: &ArchState, _format: crate::output::Format) -> DwarfTarget {
        let wide = self.bits() == 64;
        DwarfTarget {
            cfi: Some(CfiTarget {
                data_align: if wide { -8 } else { -4 },
                ra_column: 65,
                initial: vec![cfi::Insn::DefCfa(1, 0)],
                fde_encoding: 0x1b,
                eh_frame_align: if wide { 8 } else { 4 },
                cie_version: 1,
            }),
            ..DwarfTarget::lines_only(Flavor::Llvm, 4)
        }
    }

    /// The ELF ABI's DWARF numbering of the names llvm-mc accepts, with or
    /// without `%`: `r0`-`r31`, `f0`-`f31` from 32, `lr` 65, `ctr` 66,
    /// `cr0`-`cr7` from 68 and `v0`-`v31` from 77.
    fn dwarf_register(
        &self,
        _state: &ArchState,
        _format: crate::output::Format,
        name: &str,
    ) -> Option<u32> {
        let name = name.strip_prefix('%').unwrap_or(name);
        match name {
            "lr" => return Some(65),
            "ctr" => return Some(66),
            "xer" if self.bits() == 64 => return Some(76),
            "vrsave" => return Some(109),
            _ => {}
        }
        numbered_register(name, "r", 31)
            .or_else(|| numbered_register(name, "f", 31).map(|n| 32 + n))
            .or_else(|| numbered_register(name, "cr", 7).map(|n| 68 + n))
            .or_else(|| numbered_register(name, "v", 31).map(|n| 77 + n))
    }

    fn nop_fill(&self, _state: &ArchState, len: u64) -> Vec<u8> {
        encode::nop_bytes(self.endian(), len as usize)
    }

    fn assemble(&self, cx: &mut AsmCtx<'_>, req: &InsnRequest<'_>) -> Option<Vec<Variant>> {
        let mnemonic = cx.name(req.mnemonic).to_ascii_lowercase();
        let Some(resolved) = insn::lookup(&mnemonic) else {
            cx.error(
                req.mnemonic_span,
                format!("unknown instruction `{mnemonic}`"),
            );
            return None;
        };

        // `beq+`/`bne-` are static branch hints. The lexer splits the sign off
        // the mnemonic, where it would otherwise be read as a unary plus on
        // the first operand and silently ignored.
        let cur = req.cursor();
        let head = cur.peek();
        if !head.preceded_by_space
            && (head.is_punct(Punct::Plus) || head.is_punct(Punct::Minus))
            && mnemonic.starts_with('b')
        {
            cx.error(
                req.mnemonic_span.to(head.span),
                "static branch prediction hints (`+`/`-`) are not supported",
            );
            return None;
        }

        if resolved.def.flags & insn::P64 != 0 && cx.state.bits < 64 {
            cx.error(
                req.mnemonic_span,
                format!("`{mnemonic}` is a 64-bit instruction, but this is 32-bit code"),
            );
            return None;
        }

        let ops = operand::parse_list(cx, &cur)?;
        let variant = encode::Encoder::new(cx, self.endian()).encode(&resolved, &ops, req.span)?;
        // A prefixed instruction may not straddle a 64-byte boundary, where
        // the two words could land on different pages. Both references pad
        // one that would with a no-op in front, and give the section the
        // alignment that makes the rule mean the same after linking.
        if encode::prefixed(resolved.def) {
            cx.requests.push(Request::AlignCode {
                align: 64,
                max_skip: 4,
            });
            cx.requests.push(Request::RecordAlign(64));
        }
        Some(vec![variant])
    }
}

/// True if `name` is a register in this architecture.
#[allow(dead_code)]
pub fn is_register(name: &str) -> bool {
    reg::is_register(name)
}
