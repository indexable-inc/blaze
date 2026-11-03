//! Motorola 680x0 family and ColdFire. `EM_68K`.
//!
//! One backend covers every CPU GNU as knows, chosen by the names its
//! `-mcpu`/`-march` and `.cpu`/`.arch` take: `68000` through `68060`, `cpu32`
//! and the 683xx parts, Fido, and the ColdFire ISA levels and parts (`isac`,
//! `5475`). `m68k` is the 68020 with a 68881 and a 68851, which is what GNU as
//! assumes by default. Extensions follow a comma, as in GNU as:
//! `.arch 68000,68881`. A CPU is a set of `table::feature` bits, and an
//! instruction or addressing mode it lacks is refused with a message naming
//! what it needs.
//!
//! Motorola syntax is the default dialect, since that is what Amiga and Atari
//! source is written in; GNU syntax (`movew #1,%d0`) is the other. The core
//! lexes both and aligns code for the Motorola one; the backend parses
//! operands ([`operand`]) and encodes effective addresses ([`encode`]). The
//! integer instructions of the 68000-68020 are encoded by hand ([`ops`],
//! [`branch`]); everything else — the FPU, the MMUs, CAS, CALLM, MOVE16,
//! CPU32 and ColdFire additions — by `generic`, from a table generated out
//! of GNU's own (`table`).
//!
//! Everything here was checked against `m68k-elf-as` 2.47, in both its native
//! and `--mri` modes, with vasm as a second opinion where the two differ. The
//! differences that remain are deliberate and listed where they are decided:
//! no instruction substitution ([`ops`]), Motorola's word-sized default index
//! ([`operand`]), and extended-precision float immediates (`float`).

pub mod branch;
pub mod encode;
pub(crate) mod float;
pub(crate) mod generic;
pub mod insn;
pub mod operand;
pub mod ops;
pub mod reg;
pub mod reloc;
pub(crate) mod table;

use crate::arch::{ArchState, Architecture, AsmCtx, CommentSyntax, Endian, InsnRequest, Syntax};
use crate::dwarf::{CfiTarget, DwarfTarget, Flavor, cfi, numbered_register};
use crate::section::Variant;
use table::feature as f;

pub const NAMES: &[&str] = &["m68k"];

/// Every 68k CPU, as against ColdFire.
pub(crate) const M68000UP: u32 = f::M68000 | M68010UP;
/// The 68020 and the CPUs after it.
pub(crate) const M68020UP: u32 = f::M68020 | f::M68030 | f::M68040 | f::M68060;
/// The CPUs with a 32-bit `BRA`.
pub(crate) const LONG_BRANCH: u32 = M68020UP | f::CPU32 | f::FIDO_A | f::MCFISA_B;

/// The CPUs with a 32-bit branch on condition `cond`, 0 being `bra`.
pub(crate) fn long_branches(cond: u8) -> u32 {
    if cond == 0 {
        LONG_BRANCH
    } else {
        LONG_BRANCH | f::MCFISA_C
    }
}
/// The CPUs with the 68010's additions: `rtd`, `movec`, `move` from `ccr`.
pub(crate) const M68010UP: u32 = f::M68010 | f::CPU32 | f::FIDO_A | M68020UP;

/// The CPU being assembled for.
#[derive(Copy, Clone, Debug)]
pub(crate) struct Cpu {
    /// What it has, as `table::feature` bits.
    pub arch: u32,
    /// The control registers `movec` reaches on it, by `table::rid` number.
    pub ctrl: &'static [u16],
    /// The name it was chosen by.
    pub name: &'static str,
}

impl Cpu {
    pub fn has(self, arch: u32) -> bool {
        self.arch & arch != 0
    }

    pub fn coldfire(self) -> bool {
        self.has(f::MCFISA_A)
    }

    /// The 68020's addressing: full extension words, 32-bit displacements and
    /// no base register. GNU as's `cpu_of_arch (x) >= m68020 &&
    /// !arch_coldfire_p (x)`, which CPU32 and Fido pass.
    pub fn wide(self) -> bool {
        self.has(M68020UP | f::CPU32 | f::FIDO_A) && !self.coldfire()
    }

    /// Index scales other than 1, which ColdFire has too.
    pub fn scales(self) -> bool {
        self.has(M68020UP | f::CPU32 | f::FIDO_A | f::MCFISA_A)
    }

    /// A 32-bit `BRA` displacement: GNU as's `HAVE_LONG_BRANCH`.
    pub fn long_branch(self) -> bool {
        self.has(LONG_BRANCH)
    }

    /// A 32-bit displacement for `bra` (condition 0), or for `bsr` or a
    /// `Bcc`, which ColdFire ISA_C also has: GNU as's `HAVE_LONG_COND`.
    pub fn long_branch_for(self, cond: u8) -> bool {
        self.has(long_branches(cond))
    }

    /// What a message calls this CPU.
    pub fn describe(self) -> String {
        let mut s = match self.name {
            "m68k" => "68020".to_string(),
            n => n.to_string(),
        };
        let base = table::M68K_ARCHS
            .iter()
            .chain(table::M68K_CPUS)
            .find(|c| c.name == self.name)
            .map_or(self.arch, |c| c.arch);
        for (bit, ext) in [
            (f::M68881, "68881"),
            (f::M68851, "68851"),
            (f::CFLOAT, "FPU"),
        ] {
            if self.arch & bit != 0 && base & bit == 0 {
                s.push_str(" with ");
                s.push_str(ext);
            }
        }
        s
    }
}

/// Names what an instruction needs, from the CPUs that have it, for "needs a
/// 68020 or later"-style messages.
pub(crate) fn describe_arch(arch: u32) -> String {
    let mut parts: Vec<String> = Vec::new();
    let chain = [
        (f::M68000, "68000"),
        (f::M68010, "68010"),
        (f::M68020, "68020"),
        (f::M68030, "68030"),
        (f::M68040, "68040"),
        (f::M68060, "68060"),
    ];
    // The longest run of 68k CPUs up to the 68060 reads "68020 or later".
    let mut i = 0;
    while i < chain.len() {
        if arch & chain[i].0 != 0 {
            if chain[i..].iter().all(|(b, _)| arch & b != 0) && i + 1 < chain.len() {
                parts.push(format!("a {} or later", chain[i].1));
                break;
            }
            parts.push(format!("a {}", chain[i].1));
        }
        i += 1;
    }
    for (bit, what) in [
        (f::CPU32, "a CPU32"),
        (f::FIDO_A, "a Fido"),
        (f::M68881, "a 68881/68882 FPU"),
        (f::M68851, "a 68851 MMU"),
        (f::MCFISA_A, "a ColdFire"),
        (f::MCFISA_AA, "a ColdFire ISA_A+"),
        (f::MCFISA_B, "a ColdFire ISA_B"),
        (f::MCFISA_C, "a ColdFire ISA_C"),
        (f::MCFHWDIV, "a ColdFire hardware divide"),
        (f::MCFUSP, "a ColdFire USP"),
        (f::CFLOAT, "a ColdFire FPU"),
    ] {
        if arch & bit != 0 {
            parts.push(what.to_string());
        }
    }
    match parts.len() {
        0 => "a CPU this backend does not know".to_string(),
        1 => parts.pop().unwrap_or_default(),
        n => format!("{} or {}", parts[..n - 1].join(", "), parts[n - 1]),
    }
}

/// A CPU or architecture name, with GNU as's optional `m` or `mc` in front
/// of a 68k one (`m68030`, `mc68030`), then any extensions after commas.
pub fn lookup(name: &str) -> Option<Box<dyn Architecture>> {
    let mut parts = name.split(',');
    let base = parts.next()?;
    let (def_name, mut arch, ctrl) = if base == "m68k" {
        let d = table::M68K_ARCHS.iter().find(|c| c.name == "68020")?;
        ("m68k", d.arch, d.ctrl)
    } else {
        let bare = base
            .strip_prefix("mc")
            .or_else(|| base.strip_prefix('m'))
            .filter(|b| b.starts_with('6'))
            .unwrap_or(base);
        let d = table::M68K_ARCHS
            .iter()
            .chain(table::M68K_CPUS)
            .find(|c| c.name == bare)?;
        (d.name, d.arch, d.ctrl)
    };
    let mut off = 0;
    for ext in parts {
        let (neg, ext) = match ext.strip_prefix("no-") {
            Some(e) => (true, e),
            None => (false, ext),
        };
        let ext = ext
            .strip_prefix("mc")
            .or_else(|| ext.strip_prefix('m'))
            .filter(|b| b.starts_with('6'))
            .unwrap_or(ext);
        // `tc-m68k.c`'s `m68k_extensions`.
        let bits = match ext {
            "68851" => f::M68851,
            "68881" | "68882" => f::M68881,
            "float" => f::CFLOAT | f::M68881,
            "div" => f::MCFHWDIV,
            "usp" => f::MCFUSP,
            // `no-mac` turns off both kinds.
            "mac" if neg => f::MCFMAC | f::MCFEMAC,
            "mac" => f::MCFMAC,
            "emac" => f::MCFEMAC,
            _ => return None,
        };
        if neg {
            off |= bits;
        } else {
            arch |= bits;
        }
    }
    arch &= !off;
    // `float` is whichever of the two FPUs the CPU can have.
    if arch & (f::CFLOAT | f::M68881) == f::CFLOAT | f::M68881 {
        arch ^= if arch & (f::M68K_MASK & !f::M68881) != 0 {
            f::CFLOAT
        } else {
            f::M68881
        };
    }
    Some(Box::new(M68k {
        cpu: Cpu {
            arch,
            ctrl,
            name: def_name,
        },
    }))
}

pub struct M68k {
    cpu: Cpu,
}

impl Architecture for M68k {
    fn name(&self) -> &'static str {
        self.cpu.name
    }

    fn aliases(&self) -> &'static [&'static str] {
        &[
            "68000", "68010", "68020", "68030", "68040", "68060", "cpu32", "fidoa", "isaa",
            "isaaplus", "isab", "isac", "cfv4", "cfv4e",
        ]
    }

    fn endian(&self) -> Endian {
        Endian::Big
    }

    fn pointer_bytes(&self, _state: &ArchState) -> u8 {
        4
    }

    fn initial_state(&self) -> ArchState {
        ArchState {
            bits: 32,
            syntax: Syntax::Att,
            features: 0,
            cpu_features: [0; 3],
            intel_register_prefix: false,
            used: 0,
            private: 0,
        }
    }

    fn supports_syntax(&self, syntax: Syntax) -> bool {
        syntax == Syntax::Att
    }

    fn elf_machine(&self) -> u16 {
        4 // EM_68K
    }

    /// What GNU as's `m68k_elf_final_processing` records: a 68000 or 68010,
    /// CPU32, Fido, or which ColdFire ISA, divide, USP, FPU and MAC the code
    /// was assembled for. A linker refuses to mix some of them.
    fn elf_flags(&self, _state: &ArchState) -> u32 {
        const CPU32: u32 = 0x0081_0000;
        const M68000: u32 = 0x0100_0000;
        const CFV4E: u32 = 0x0000_8000;
        const FIDO: u32 = 0x0200_0000;
        const CF_FLOAT: u32 = 0x40;
        let a = self.cpu.arch;
        let mut flags = 0;
        if a & f::CFLOAT != 0 {
            flags |= CFV4E;
        }
        if a & f::CPU32 != 0 {
            flags |= CPU32;
        } else if a & f::FIDO_A != 0 {
            flags |= FIDO;
        } else if a & M68000UP != 0 && a & M68020UP == 0 {
            flags |= M68000;
        }
        if a & f::MCFISA_A != 0 {
            let isa = a
                & (f::MCFISA_A
                    | f::MCFISA_AA
                    | f::MCFISA_B
                    | f::MCFISA_C
                    | f::MCFHWDIV
                    | f::MCFUSP);
            let cf = [
                (0x01, f::MCFISA_A),
                (0x02, f::MCFISA_A | f::MCFHWDIV),
                (0x03, f::MCFISA_A | f::MCFISA_AA | f::MCFHWDIV | f::MCFUSP),
                (0x04, f::MCFISA_A | f::MCFISA_B | f::MCFHWDIV),
                (0x05, f::MCFISA_A | f::MCFISA_B | f::MCFHWDIV | f::MCFUSP),
                (0x06, f::MCFISA_A | f::MCFISA_C | f::MCFHWDIV | f::MCFUSP),
                (0x07, f::MCFISA_A | f::MCFISA_C | f::MCFUSP),
            ];
            if let Some(&(bits, _)) = cf.iter().find(|&&(_, pattern)| pattern == isa) {
                flags |= bits;
                if a & f::CFLOAT != 0 {
                    flags |= CF_FLOAT | CFV4E;
                }
                flags |= match a & (f::MCFMAC | f::MCFEMAC) {
                    f::MCFMAC => 0x10,
                    f::MCFEMAC => 0x20,
                    _ => 0,
                };
            }
        }
        flags
    }

    fn pcrel_number_is_address(&self) -> bool {
        true
    }

    /// GNU as for m68k treats only a weak symbol as one the linker may
    /// replace: a branch to a global symbol in the same section is resolved.
    fn defers_to_linker(&self, r: &crate::arch::SameSectionRef<'_>) -> bool {
        r.binding == crate::symbol::Binding::Weak
    }

    /// And a relocation against a global symbol names its section.
    fn relocates_globals_by_section(&self) -> bool {
        true
    }

    /// GNU as aligns the three standard sections to 4 bytes from the start,
    /// and no other.
    fn section_align(
        &self,
        _state: &ArchState,
        name: &str,
        _flags: &crate::section::SectionFlags,
    ) -> u64 {
        match name {
            ".text" | ".data" | ".bss" => 4,
            _ => 1,
        }
    }

    fn default_dialect(&self) -> crate::lexer::Dialect {
        crate::lexer::Dialect::Motorola
    }

    fn align_unit(&self) -> u64 {
        2
    }

    /// GNU as for m68k comments with `|`, and with `#` or `*` only at the
    /// start of a line — its `line_comment_chars` — since `#` marks an
    /// immediate and `*` multiplies. `;` still separates statements.
    fn comments(&self) -> CommentSyntax {
        CommentSyntax {
            anywhere: &["|"],
            line_start: &["#", "*"],
        }
    }

    /// `.even` is the 680x0 port's `.align 2`, kept for the Sun assembler.
    /// GNU as's `s_even` pads with zeros wherever it stands, code included.
    fn directive(
        &self,
        cx: &mut AsmCtx<'_>,
        name: &str,
        _cur: &mut crate::cursor::Cursor<'_>,
    ) -> bool {
        if name != ".even" {
            return false;
        }
        cx.requests.push(crate::arch::Request::AlignZero(2));
        true
    }

    fn data_reloc(&self, size: u8, pcrel: bool) -> Option<u32> {
        reloc::data(size, pcrel)
    }

    /// GNU as compares an `@` suffix with `strncmp` in both `m68k_ip`'s lexer
    /// and `m68k_elf_suffix`, so `x@tlsgd` is a syntax error and only
    /// `x@TLSGD` names a model.
    fn uppercase_modifiers(&self) -> bool {
        true
    }

    /// Only `.long` reads a suffix in data, since only `.long` is the
    /// directive m68k's GNU as points at its own `m68k_elf_cons`; `.int`,
    /// `.4byte` and every `.dc.l` stay with the plain `cons`, which calls the
    /// `@` junk after the expression.
    fn directive_modifiers(&self) -> Option<&'static [&'static str]> {
        Some(&[".long"])
    }

    /// What `.long` takes. `m68k_elf_cons` reads one suffix, `@TLSLDO`, and
    /// refuses a relocation wider than the value, so the four-byte
    /// `R_68K_TLS_LDO32` fits only a four-byte field. The other eight
    /// suffixes are the operand parser's alone and have no spelling here at
    /// all.
    fn modifier_reloc(&self, name: &str, size: u8, _pcrel: bool) -> Option<u32> {
        (name == "tlsldo" && size == 4).then_some(reloc::R_68K_TLS_LDO32)
    }

    /// What an instruction operand takes: any of the nine suffixes, in
    /// whatever width the field the operand was encoded into has, which is
    /// GNU as's `get_reloc_code (n, pcrel, pic_reloc)` over the fixups
    /// `md_assemble` makes. Anything else is refused; see [`reloc`].
    fn fixup_modifier_reloc(
        &self,
        name: &str,
        kind: &crate::section::FixupKind,
    ) -> crate::arch::FixupModifier {
        match reloc::suffix(name, kind.size) {
            Some(r) => crate::arch::FixupModifier::Reloc(r),
            None => crate::arch::FixupModifier::Unknown,
        }
    }

    /// A thread-local model names a variable, which GNU as marks `STT_TLS`.
    /// None of the nine suffixes needs a symbol of its own: the m68k psABI
    /// reaches the GOT through a register the code already holds, so not
    /// even `@GOT` implies `_GLOBAL_OFFSET_TABLE_`, and an object GNU as
    /// writes for one has no such symbol.
    fn modifier_symbols(&self, name: &str) -> crate::arch::ModifierSymbols {
        crate::arch::ModifierSymbols {
            tls: reloc::is_tls(name),
            // What each of them asks for belongs to one symbol -- a GOT
            // slot, a PLT entry, a thread-local offset -- so the relocation
            // names it, where this backend relocates everything else against
            // its section. GNU as keeps even a plain local label for one.
            names_target: reloc::is_suffix(name),
            ..crate::arch::ModifierSymbols::default()
        }
    }

    /// GNU as's conventions, as for every m68k encoding: code counted in
    /// words, and a frame that starts with the return address just above the
    /// stack pointer.
    fn dwarf(&self, _state: &ArchState, _format: crate::output::Format) -> DwarfTarget {
        DwarfTarget {
            cfi: Some(CfiTarget {
                data_align: -4,
                ra_column: 24,
                initial: vec![cfi::Insn::DefCfa(15, 4), cfi::Insn::Offset(24, -4)],
                fde_encoding: 0x1b,
                eh_frame_align: 4,
                cie_version: 1,
            }),
            ..DwarfTarget::lines_only(Flavor::Gnu, 2)
        }
    }

    /// GNU as's numbering, for the names it accepts, with or without `%`:
    /// `d0`-`d7`, `a0`-`a6` and `sp` from 8, `fp0`-`fp7` from 16, and `pc`
    /// as 24. It takes neither `a7` nor `fp` here.
    fn dwarf_register(
        &self,
        _state: &ArchState,
        _format: crate::output::Format,
        name: &str,
    ) -> Option<u32> {
        let name = name.strip_prefix('%').unwrap_or(name);
        match name {
            "sp" => Some(15),
            "pc" => Some(24),
            _ => numbered_register(name, "d", 7)
                .or_else(|| numbered_register(name, "a", 6).map(|n| 8 + n))
                .or_else(|| numbered_register(name, "fp", 7).map(|n| 16 + n)),
        }
    }

    /// Zeroes, not `NOP`s: `m68k-elf-as` (in both syntaxes) and vasm pad code
    /// alignment with zero bytes, and matching them keeps the bytes identical.
    fn nop_fill(&self, _state: &ArchState, len: u64) -> Vec<u8> {
        vec![0; len as usize]
    }

    fn assemble(&self, cx: &mut AsmCtx<'_>, req: &InsnRequest<'_>) -> Option<Vec<Variant>> {
        let name = cx.name(req.mnemonic).to_ascii_lowercase();
        let before = cx.diags.error_count();
        let out = match insn::resolve(&name) {
            Ok((def, size, stem)) => {
                let gnu = format!("{stem}{}", size.map_or(String::new(), String::from));
                let stem = stem.to_string();
                let mut asm = ops::Asm {
                    cx,
                    cpu: self.cpu,
                    name: name.clone(),
                    span: req.span,
                };
                let out = asm.assemble(def, size, &stem, req);
                match out {
                    Some(v) if self.cpu.coldfire() => {
                        generic::coldfire_check(cx, self.cpu, &gnu, &name, &v[0], req).map(|()| v)
                    }
                    out => out,
                }
            }
            // GNU as looks a mnemonic up with its dot removed, so `fadd.x` is
            // the table's `faddx`.
            Err(msg) => match generic::forms(&name.replacen('.', "", 1)) {
                Some(forms) => generic::assemble(cx, self.cpu, &name, forms, req),
                None => {
                    cx.error(req.mnemonic_span, msg);
                    return None;
                }
            },
        };
        // Every failure path reports its own error. Should one ever not, the
        // statement must still not vanish without a word.
        if out.is_none() && cx.diags.error_count() == before {
            cx.error(req.span, "cannot assemble this instruction");
        }
        out
    }
}
