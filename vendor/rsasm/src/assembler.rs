//! The assembler driver: section state, statement processing, layout and
//! fixup resolution.

use crate::arch::{ArchState, Architecture, AsmCtx, CpuOption, InsnRequest, Syntax};
use crate::cursor::Cursor;
use crate::diag::{DiagBag, Diagnostic};
use crate::dialect;
use crate::expr::{self, EvalCtx, EvalError, ExprArena, ExprKind, ExprRef, Value};
use crate::intern::{Interner, Name};
use crate::lexer::{Dialect, LexConfig, LitPool, LocalDir, Punct};
use crate::macros::{self, MacroDef};
use crate::parser::{Body, LabelDef, Parser, Statement};
use crate::reloc::RelocDesc;
use crate::section::{FragKind, Fragment, Section, SectionFlags, SectionId, SectionKind};
use crate::source::{FileId, SourceMap, Span};
use crate::symbol::{SymbolId, SymbolTable, SymbolValue};
use std::collections::HashMap;
use std::path::PathBuf;

/// A relocation the linker must apply.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Relocation {
    pub section: SectionId,
    /// Offset within the section.
    pub offset: u64,
    /// `None` for a reference to a plain number, which ELF relocates against
    /// symbol 0.
    pub symbol: Option<SymbolId>,
    pub addend: i64,
    /// Architecture-specific relocation type, in ELF's numbering.
    pub kind: u32,
    /// The same relocation described in terms no format owns, which is what
    /// a writer that numbers relocations differently reads; see
    /// the crate's `reloc` module. Not API.
    #[doc(hidden)]
    pub desc: RelocDesc,
}

/// When a conditional Thumb instruction with no `it` block of its own gets
/// one made up for it: GNU as's `-mimplicit-it`.
///
/// A32 carries a condition in every instruction and T32 takes it from an
/// `it` block, so source written to assemble for both leaves the block to
/// the assembler. The four modes are GNU as's, and each says what becomes of
/// a conditional instruction that stands outside a block in each instruction
/// set: it is accepted, accepted with a warning, or an error. Only Thumb code
/// can have the block made up; ARM code needs none, so a mode that does not
/// accept the instruction there warns rather than refusing it.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
#[non_exhaustive]
pub enum ImplicitIt {
    /// `never`: a warning in ARM code, an error in Thumb code.
    Never,
    /// `arm`, the default: accepted in ARM code, an error in Thumb code.
    #[default]
    Arm,
    /// `thumb`: a warning in ARM code, and in Thumb code the `it` is made up.
    Thumb,
    /// `always`: accepted in ARM code, and in Thumb code the `it` is made up.
    Always,
}

impl ImplicitIt {
    /// The mode `-mimplicit-it=` names, as GNU as's `arm_parse_it_mode`
    /// spells the four.
    pub fn from_name(name: &str) -> Option<ImplicitIt> {
        Some(match name {
            "never" => ImplicitIt::Never,
            "arm" => ImplicitIt::Arm,
            "thumb" => ImplicitIt::Thumb,
            "always" => ImplicitIt::Always,
            _ => return None,
        })
    }

    /// Whether a conditional Thumb instruction outside a block gets an `it`
    /// made up for it rather than being refused.
    pub fn makes_thumb_block(self) -> bool {
        matches!(self, ImplicitIt::Thumb | ImplicitIt::Always)
    }

    /// Whether a conditional ARM instruction outside a block is left alone
    /// rather than warned about.
    pub fn allows_arm(self) -> bool {
        matches!(self, ImplicitIt::Arm | ImplicitIt::Always)
    }
}

/// How a source file is read and what is made of it.
///
/// Built with [`Options::new`] and the `with_*` methods; the struct is
/// `#[non_exhaustive]` so that a later release can describe something new
/// without breaking callers.
///
/// ```
/// use rsasm::assembler::Options;
/// use rsasm::lexer::Dialect;
///
/// let options = Options::new()
///     .with_dialect(Dialect::Nasm)
///     .with_relocatable(false)
///     .with_base_addr(0x7c00);
/// assert_eq!(options.dialect(), Dialect::Nasm);
/// ```
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Options {
    /// Produce a relocatable object (emit relocations) rather than resolving
    /// every reference to a final address.
    pub(crate) relocatable: bool,
    /// Base address for absolute output.
    pub(crate) base_addr: u64,
    /// Directories searched by `.include`.
    pub(crate) include_paths: Vec<PathBuf>,
    pub(crate) dialect: Dialect,
    pub(crate) syntax: Option<Syntax>,
    /// The DWARF version asked for on the command line, which the line table
    /// and `.debug_frame` follow unless the source asks for version 5 with
    /// `.file 0`.
    pub(crate) dwarf_version: Option<u8>,
    /// Describe the assembly source itself in a line table and a
    /// compilation unit, as `-g` asks GNU as and llvm-mc to.
    pub(crate) debug_source: bool,
    /// The object format being written, which the source can see: Mach-O
    /// names its sections differently, counts `.align` in bits rather than
    /// bytes, and decides what a linker is told by rules of its own; COFF
    /// has directives of its own, keeps relocation addends in the bytes they
    /// relocate, and starts its sections with alignments of its own. Flat
    /// output leaves this at its default, since `relocatable` already says
    /// there is no object.
    pub(crate) format: crate::output::Format,
    /// Whether a conditional Thumb instruction with no `it` block of its own
    /// gets one made up for it, which only the ARM backend reads.
    pub(crate) implicit_it: ImplicitIt,
    /// What `-march=`, `-mcpu=` and `-mfpu=` named, which decides what the
    /// target has before the source says anything. Applied in that order, so
    /// that a CPU wins over an architecture as it does in GNU as.
    pub(crate) cpu_options: Vec<(CpuOption, String)>,
    /// The target triple the architecture and format were picked from, where
    /// one was given. A Darwin triple says which release of the operating
    /// system the object is for as well, and that is the one thing in it
    /// Mach-O output still needs after the two have been settled; see
    /// [`Options::with_target_triple`].
    pub(crate) target_triple: Option<String>,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            relocatable: true,
            base_addr: 0,
            include_paths: Vec::new(),
            dialect: Dialect::Gas,
            syntax: None,
            dwarf_version: None,
            debug_source: false,
            format: crate::output::Format::Elf,
            implicit_it: ImplicitIt::Arm,
            cpu_options: Vec::new(),
            target_triple: None,
        }
    }
}

impl Options {
    /// The defaults: a relocatable ELF object based at 0, read as GNU as
    /// reads it, with no debug information and no `.include` path.
    pub fn new() -> Options {
        Options::default()
    }

    /// Whether to emit relocations rather than resolve every reference to a
    /// final address. Flat output (`bin`, `ihex`) needs `false`.
    pub fn with_relocatable(mut self, yes: bool) -> Options {
        self.relocatable = yes;
        self
    }

    /// The address the first section is loaded at, for absolute output.
    pub fn with_base_addr(mut self, addr: u64) -> Options {
        self.base_addr = addr;
        self
    }

    /// Adds one directory to the `.include` search path.
    pub fn with_include_path(mut self, dir: impl Into<PathBuf>) -> Options {
        self.include_paths.push(dir.into());
        self
    }

    /// Adds several directories to the `.include` search path, in order.
    pub fn with_include_paths<I>(mut self, dirs: I) -> Options
    where
        I: IntoIterator,
        I::Item: Into<PathBuf>,
    {
        self.include_paths.extend(dirs.into_iter().map(Into::into));
        self
    }

    /// The source dialect. [`Architecture::default_dialect`] names the one a
    /// target is usually written in.
    ///
    /// [`Architecture::default_dialect`]: crate::arch::Architecture::default_dialect
    pub fn with_dialect(mut self, dialect: Dialect) -> Options {
        self.dialect = dialect;
        self
    }

    /// The initial operand syntax, where the target has more than one.
    /// Left alone, each backend starts in the syntax its reference
    /// assembler starts in.
    pub fn with_syntax(mut self, syntax: Syntax) -> Options {
        self.syntax = Some(syntax);
        self
    }

    /// The DWARF version to write, 2 to 5. The source may still ask for
    /// version 5 with `.file 0`.
    pub fn with_dwarf_version(mut self, version: u8) -> Options {
        self.dwarf_version = Some(version);
        self
    }

    /// Describe the assembly source itself in DWARF, as `-g` asks GNU as to.
    pub fn with_debug_source(mut self, yes: bool) -> Options {
        self.debug_source = yes;
        self
    }

    /// The object format being written, which some directives can see.
    pub fn with_format(mut self, format: crate::output::Format) -> Options {
        self.format = format;
        self
    }

    /// When a conditional Thumb instruction with no `it` block of its own
    /// gets one made up for it; see [`ImplicitIt`].
    pub fn with_implicit_it(mut self, mode: ImplicitIt) -> Options {
        self.implicit_it = mode;
        self
    }

    /// Adds what one `-march=`, `-mcpu=` or `-mfpu=` named, for a backend
    /// that gates what it assembles on the target selected; see
    /// [`Architecture::select_option`]. A name the backend refuses makes
    /// [`Assembler::new`] report it as a diagnostic with no source position.
    ///
    /// [`Architecture::select_option`]: crate::arch::Architecture::select_option
    pub fn with_cpu_option(mut self, opt: CpuOption, name: impl Into<String>) -> Options {
        self.cpu_options.push((opt, name.into()));
        self
    }

    /// The target triple the architecture and the format were chosen from,
    /// which both have to be given here as well.
    ///
    /// Only Mach-O output reads it, and only for the deployment target a
    /// Darwin triple names: `x86_64-apple-macos10.6` writes an
    /// `LC_VERSION_MIN_MACOSX`, and on x86 a deployment target of macOS 10.6
    /// or later gives the linker a compact unwind table as well. llvm-mc
    /// takes both from the triple, so rsasm has to have it to write the same
    /// object.
    pub fn with_target_triple(mut self, triple: impl Into<String>) -> Options {
        self.target_triple = Some(triple.into());
        self
    }

    /// Whether relocations are emitted; see [`Options::with_relocatable`].
    pub fn relocatable(&self) -> bool {
        self.relocatable
    }

    /// The base address for absolute output.
    pub fn base_addr(&self) -> u64 {
        self.base_addr
    }

    /// The `.include` search path, in the order it is searched.
    pub fn include_paths(&self) -> &[PathBuf] {
        &self.include_paths
    }

    /// The source dialect.
    pub fn dialect(&self) -> Dialect {
        self.dialect
    }

    /// The initial operand syntax, if one was asked for.
    pub fn syntax(&self) -> Option<Syntax> {
        self.syntax
    }

    /// The DWARF version asked for, if any.
    pub fn dwarf_version(&self) -> Option<u8> {
        self.dwarf_version
    }

    /// Whether the assembly source itself is described in DWARF.
    pub fn debug_source(&self) -> bool {
        self.debug_source
    }

    /// The object format being written.
    pub fn format(&self) -> crate::output::Format {
        self.format
    }

    /// When an `it` block is made up for a conditional Thumb instruction.
    pub fn implicit_it(&self) -> ImplicitIt {
        self.implicit_it
    }

    /// The target triple given, if any; see [`Options::with_target_triple`].
    pub fn target_triple(&self) -> Option<&str> {
        self.target_triple.as_deref()
    }
}

/// The block constructs the statement walker has to recognise before the
/// ordinary directive table sees them.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum BlockKind {
    Macro,
    EndMacro,
    ExitMacro,
    Repeat(RepeatKind),
    EndRepeat,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum RepeatKind {
    Rept,
    Irp,
    Irpc,
}

/// A file being assembled, read a statement at a time.
pub(crate) struct Reader {
    pub(crate) parser: Parser,
    /// The [`Assembler::lex_epoch`] the parser's lexing rules were taken at.
    epoch: u64,
}

/// One level of `.if` / `.else` / `.endif`.
pub(crate) struct Cond {
    /// Whether code in the current branch is being assembled.
    pub(crate) active: bool,
    /// Whether any branch of this conditional has been taken yet.
    pub(crate) taken: bool,
    pub(crate) seen_else: bool,
    pub(crate) span: Span,
}

/// The assembler: source in, sections and relocations out.
///
/// Drive it with [`Assembler::assemble_str`], [`Assembler::assemble_path`] or
/// [`Assembler::assemble_file`], then [`Assembler::finish`], then hand it to
/// one of the writers in [`crate::output`] or read a section back with
/// [`Assembler::section_bytes`].
///
/// The fields are the crate's own working state and are not API, however
/// visible they have to be for rsasm's own tests; use the methods.
pub struct Assembler {
    /// Not API.
    #[doc(hidden)]
    pub sm: SourceMap,
    /// Not API.
    #[doc(hidden)]
    pub interner: Interner,
    pub(crate) pool: LitPool,
    /// Not API.
    #[doc(hidden)]
    pub diags: DiagBag,
    pub(crate) exprs: ExprArena,
    /// Not API.
    #[doc(hidden)]
    pub symbols: SymbolTable,
    /// Not API.
    #[doc(hidden)]
    pub sections: Vec<Section>,
    /// Not API.
    #[doc(hidden)]
    pub relocs: Vec<Relocation>,
    section_ids: HashMap<Name, SectionId>,
    pub(crate) cur: SectionId,
    /// Where `.previous` goes back to.
    previous: Option<SectionId>,
    section_stack: Vec<(SectionId, Option<SectionId>)>,
    /// Not API.
    #[doc(hidden)]
    pub arch: Box<dyn Architecture>,
    pub(crate) arch_state: ArchState,
    /// Every backend that has been active, in the order `.arch` made them
    /// so, with the state each was left in; a section's
    /// [`Section::arch_marks`] index this. Slot 0 is the backend the
    /// assembler was created with, and the active one's slot is `None`,
    /// since it lives in `arch` and `arch_state`.
    arch_slots: Vec<Option<(Box<dyn Architecture>, ArchState)>>,
    /// The active backend's slot in `arch_slots`.
    arch_slot: u32,
    /// Not API.
    #[doc(hidden)]
    pub options: Options,
    /// Bumped whenever the lexing rules may have changed, which only an
    /// `.arch` switch does, so a file being read knows to take them again
    /// without comparing configurations on every statement.
    lex_epoch: u64,
    /// The anonymous label standing in for `.` in the current statement.
    pub(crate) here_sym: Option<SymbolId>,
    /// The labels written on the current statement's own line.
    stmt_labels: Vec<SymbolId>,
    cond: Vec<Cond>,
    /// Guards against runaway `.include` recursion.
    include_depth: u32,
    /// Macros defined so far, keyed by the lowercased name the parser
    /// produces for a mnemonic.
    pub(crate) macros: HashMap<Name, MacroDef>,
    /// Bumped per macro invocation and substituted for `\@`, which is how
    /// macro bodies name labels that must not collide between calls.
    macro_counter: u64,
    macro_depth: u32,
    /// Set by `.exitm`; unwinds the innermost expansion.
    exiting_macro: bool,
    /// Set by a vendor `END`: nothing after it in the source is assembled.
    pub(crate) end_of_source: bool,
    /// While relaxation weighs another size for one fragment: labels in that
    /// section after that fragment, and still at an offset past `pc`, move
    /// by `shift` bytes. Fields: section, fragment index, `pc`, `shift`.
    pub(crate) relax_shift: Option<(SectionId, u32, u64, i64)>,
    /// The alignment fragments put ahead of data that must already be
    /// aligned, which are errors if they pad; see `Assembler::align_data`.
    pub(crate) align_tests: Vec<(SectionId, u32)>,
    /// Where a [`Request::Mark`](crate::arch::Request::Mark) put its
    /// relocation, and how many bytes have to follow it in its reference
    /// fragment: section, fragment index, offset in the fragment, bytes,
    /// and the directive to blame.
    pub(crate) mark_tests: Vec<(SectionId, u32, u32, u8, Span)>,
    /// What the attribute directives -- ARM's `.eabi_attribute`, RISC-V's
    /// `.attribute`, PowerPC's `.gnu_attribute` -- said, as
    /// (vendor, tag, value), in the order the source wrote them. Merged into
    /// what the backend gives when the object is laid out; see
    /// `Assembler::add_attributes_section`.
    pub(crate) attr_overrides: Vec<(&'static str, u32, crate::arch::AttrValue)>,
    /// CC-RH data values written without `#` that were not constants when
    /// read, to be refused at the end if they are labels; see
    /// `Assembler::cc_data`.
    pub(crate) cc_bare_labels: Vec<ExprRef>,
    /// Numbers the symbols a CC-RL/CC-RH `.LOCAL` renames, across the module.
    cc_local_counter: u64,
    /// CC-RX `.DEFINE` strings, by name; see `Assembler::ccrx_apply_defines`.
    pub(crate) ccrx_defines: Vec<(String, String)>,
    /// What CC-RX `.SECTION` and `.ORG` said about each section.
    pub(crate) ccrx_sections: HashMap<SectionId, crate::dialect_cc::RxSection>,
    /// Line table rows and call frame information, written out after layout.
    pub(crate) dwarf: crate::dwarf::DwarfState,
    /// The sections whose end layout rounded up to their alignment; see
    /// `Assembler::pad_section_tails`.
    pub(crate) tail_pads: Vec<SectionId>,
    /// Per section, in order, the offsets where a relaxing linker may delete
    /// bytes, so that a distance measured across one of them is not known
    /// here; see `Architecture::moves_code` and
    /// `Assembler::code_moves_between`. Settled with the layout, and empty
    /// for every target whose linker shortens nothing.
    pub(crate) moving_code: Vec<Vec<u64>>,
    /// Per section, in order, the fragments that pad to an alignment, whose
    /// size a reference settles after the shape of a line table's address
    /// advances; see `Assembler::code_uncertain_between`.
    pub(crate) aligning_code: Vec<Vec<u32>>,
    /// The sections whose relocations [`Arch::reloc_at`] picks by the offset
    /// of the field in its fragment rather than in the section, as llvm-mc
    /// picks them in the line tables it writes, where each sequence starts a
    /// fragment of its own.
    ///
    /// [`Arch::reloc_at`]: crate::arch::Arch::reloc_at
    pub(crate) relocs_by_fragment: Vec<SectionId>,
    /// The literals each section's next pool will hold; see
    /// [`crate::literals`].
    pub(crate) literal_pools: HashMap<SectionId, Vec<crate::arch::LiteralRequest>>,
    /// How far each section's next literal pool is aligned, where that is
    /// more than the four bytes a pool starts out wanting. GNU as's ARM port
    /// keeps the alignment on the pool itself, and a pool that a `.ltorg`
    /// emptied keeps it: once a section has held an eight-byte entry, every
    /// later pool in it is aligned to eight as well. See
    /// [`crate::literals`].
    pub(crate) literal_pool_align: HashMap<SectionId, u64>,
    /// Where the `it` a [`Request::ImplicitIt`] marked went, as the section
    /// and the fragment holding its halfword, so that an instruction joining
    /// its block can have the mask written again; see [`ImplicitIt`].
    ///
    /// [`Request::ImplicitIt`]: crate::arch::Request::ImplicitIt
    pub(crate) implicit_it_at: Option<(SectionId, u32)>,
    /// The mapping symbols of the finished object; see the crate's `mapping`
    /// module. Not API.
    #[doc(hidden)]
    pub mapping_symbols: Vec<crate::mapping::MappingSymbol>,
    /// The NASM dialect's preprocessor and assembler state.
    pub(crate) nasm: crate::nasm::State,
    /// What COFF output needs that ELF has no room for; see [`crate::coff`].
    pub(crate) coff: crate::coff::State,
    /// What the source said that only a Mach-O object records.
    pub(crate) macho: crate::output::macho::State,
    /// The backends whose [`Architecture::prelude`] has been assembled.
    pub(crate) arch_preludes: Vec<&'static str>,
    /// Where each assembled prelude's text lies in the source map, as
    /// (start, end) positions; a label may take a name defined there.
    pub(crate) arch_prelude_text: Vec<(u32, u32)>,
}

impl Assembler {
    pub fn new(arch: Box<dyn Architecture>, options: Options) -> Assembler {
        let mut interner = Interner::new();
        let mut option_errors = Vec::new();
        let arch_state = {
            let mut st = arch.initial_state();
            if let Some(s) = options.syntax {
                st.syntax = s;
            }
            // `-march=` before `-mcpu=`, so that a CPU named as well wins, as
            // it does in GNU as; `-mfpu=` is read last whichever way round it
            // was written.
            let mut opts: Vec<&(CpuOption, String)> = options.cpu_options.iter().collect();
            opts.sort_by_key(|(opt, _)| match opt {
                CpuOption::Arch => 0,
                CpuOption::Cpu => 1,
                CpuOption::Fpu => 2,
            });
            for (opt, name) in opts {
                if let Err(msg) = arch.select_option(&mut st, *opt, name) {
                    option_errors.push(msg);
                }
            }
            st
        };
        let text = interner.intern(".text");
        let mut asm = Assembler {
            sm: SourceMap::new(),
            interner,
            pool: LitPool::new(),
            diags: DiagBag::new(),
            exprs: ExprArena::new(),
            symbols: SymbolTable::new(),
            sections: Vec::new(),
            relocs: Vec::new(),
            section_ids: HashMap::new(),
            cur: SectionId(0),
            previous: None,
            section_stack: Vec::new(),
            arch,
            arch_state,
            arch_slots: vec![None],
            arch_slot: 0,
            options,
            lex_epoch: 0,
            here_sym: None,
            stmt_labels: Vec::new(),
            cond: Vec::new(),
            include_depth: 0,
            macros: HashMap::new(),
            macro_counter: 0,
            macro_depth: 0,
            exiting_macro: false,
            end_of_source: false,
            relax_shift: None,
            align_tests: Vec::new(),
            mark_tests: Vec::new(),
            attr_overrides: Vec::new(),
            cc_bare_labels: Vec::new(),
            cc_local_counter: 0,
            ccrx_defines: Vec::new(),
            ccrx_sections: HashMap::new(),
            dwarf: crate::dwarf::DwarfState::default(),
            tail_pads: Vec::new(),
            moving_code: Vec::new(),
            aligning_code: Vec::new(),
            relocs_by_fragment: Vec::new(),
            literal_pools: HashMap::new(),
            literal_pool_align: HashMap::new(),
            implicit_it_at: None,
            mapping_symbols: Vec::new(),
            nasm: crate::nasm::State::default(),
            coff: crate::coff::State::default(),
            macho: crate::output::macho::State::default(),
            arch_preludes: Vec::new(),
            arch_prelude_text: Vec::new(),
        };
        for msg in option_errors {
            asm.diags.error(Span::DUMMY, msg);
        }
        if asm.options.dialect == Dialect::CcRx {
            // The predefined names CC-RX defines whatever the options
            // (R20UT3248EJ0115 Table 5.36, pages 499-500, note 1), except the
            // version number, which would claim a Renesas release.
            for name in ["__ASRX__", "__RENESAS__"] {
                asm.ccrx_defines.push((name.to_string(), "1".to_string()));
            }
        }
        asm.cur = if asm.options.format == crate::output::Format::MachO {
            // The target triple decides the deployment target and with it
            // the compact unwind table, which `__TEXT,__text` and every
            // later section is created knowing about.
            if let Some(cpu) = crate::output::macho::Cpu::for_arch(asm.arch.as_ref()) {
                asm.macho.deployment =
                    crate::output::macho::Deployment::of(cpu, asm.options.target_triple.as_deref());
            }
            asm.macho_section("__TEXT", "__text", None)
        } else {
            asm.get_or_create_section(text, SectionKind::Progbits, SectionFlags::text(), 1)
        };
        if asm.options.dialect == Dialect::Nasm {
            // NASM operands are Intel's, and its ELF writer aligns `.text` to
            // 16; its standard macros are defined before any source is read.
            if asm.options.syntax.is_none() {
                asm.arch_state.syntax = Syntax::Intel;
            }
            if asm.options.relocatable {
                asm.sections[0].align = 16;
            }
            asm.nasm_prelude();
        }
        asm.arch_prelude();
        asm
    }

    /// Assembles what the active backend predefines, the first time that
    /// backend is active; see [`Architecture::prelude`].
    pub(crate) fn arch_prelude(&mut self) {
        let name = self.arch.name();
        if self.arch_preludes.contains(&name) {
            return;
        }
        self.arch_preludes.push(name);
        let text = self.arch.prelude(self.options.dialect);
        if text.is_empty() {
            return;
        }
        let file = self.sm.add(format!("<{name} predefined names>"), text);
        let f = self.sm.file(file);
        self.arch_prelude_text.push((f.start, f.end()));
        self.assemble_file(file);
    }

    /// Whether symbol `id` was last defined by a backend's prelude, which a
    /// label is allowed to replace: `P0:` is a label to sdas8051, which
    /// predefines `P0` too, and to AS, which does not.
    fn predefined(&self, id: crate::symbol::SymbolId) -> bool {
        let at = self.symbols.get(id).def_span.lo;
        self.arch_prelude_text
            .iter()
            .any(|&(lo, hi)| at >= lo && at < hi)
    }

    // ---- sections ---------------------------------------------------------

    /// The diagnostics reported so far. Render them with
    /// [`DiagBag::render`] and [`Assembler::source_map`].
    pub fn diags(&self) -> &DiagBag {
        &self.diags
    }

    /// The files read so far, which a diagnostic's span points into.
    pub fn source_map(&self) -> &SourceMap {
        &self.sm
    }

    /// The options the assembler was created with, as
    /// [`Assembler::new`] received them.
    pub fn options(&self) -> &Options {
        &self.options
    }

    /// Not API.
    #[doc(hidden)]
    pub fn section(&self, id: SectionId) -> &Section {
        &self.sections[id.0 as usize]
    }

    /// Not API.
    #[doc(hidden)]
    pub fn section_mut(&mut self, id: SectionId) -> &mut Section {
        &mut self.sections[id.0 as usize]
    }

    /// Not API.
    #[doc(hidden)]
    pub fn cur_section(&mut self) -> &mut Section {
        let id = self.cur;
        &mut self.sections[id.0 as usize]
    }

    /// Not API.
    #[doc(hidden)]
    pub fn get_or_create_section(
        &mut self,
        name: Name,
        kind: SectionKind,
        flags: SectionFlags,
        align: u64,
    ) -> SectionId {
        if let Some(&id) = self.section_ids.get(&name) {
            return id;
        }
        let id = SectionId(self.sections.len() as u32);
        let mut s = Section::new(id, name, kind, flags);
        // The backend active where a section is first named decides its
        // starting alignment, as the reference for that backend would. In a
        // COFF or Mach-O object the format decides instead: llvm-mc aligns
        // the three sections COFF always has to four bytes on every machine,
        // gives a COFF section the source names none of its own, and starts
        // every Mach-O section unaligned.
        let default = if self.options.format.is_coff() {
            crate::output::coff::default_align(self.interner.get(name))
        } else if self.options.format == crate::output::Format::MachO {
            1
        } else {
            self.arch
                .section_align(&self.arch_state, self.interner.get(name), &flags)
        };
        s.align = align.max(default).max(1);
        s.mark_arch(self.arch_slot);
        self.sections.push(s);
        self.section_ids.insert(name, id);
        id
    }

    /// Switches to `id`, remembering where `.previous` should return to.
    ///
    /// Where the section is the one already current, the switch is still
    /// recorded: GNU as's `obj_elf_section_change_hook` runs before every
    /// section directive, so `.pushsection .foo` twice leaves `.previous`
    /// pointing at `.foo`, not at what came before the first one.
    pub(crate) fn set_section(&mut self, id: SectionId) {
        if id != self.cur {
            self.dwarf_section_switch();
        }
        self.previous = Some(self.cur);
        self.cur = id;
    }

    pub(crate) fn swap_previous(&mut self) {
        if let Some(prev) = self.previous {
            if prev != self.cur {
                self.dwarf_section_switch();
            }
            self.previous = Some(self.cur);
            self.cur = prev;
        }
    }

    pub(crate) fn push_section_stack(&mut self) {
        self.section_stack.push((self.cur, self.previous));
    }

    pub(crate) fn pop_section(&mut self) -> Option<SectionId> {
        let (cur, prev) = self.section_stack.pop()?;
        self.previous = prev;
        Some(cur)
    }

    /// Resolves one of the shorthand section directives.
    pub(crate) fn standard_section(&mut self, name: &str) -> SectionId {
        if self.options.format == crate::output::Format::MachO
            && let Some(s) = crate::output::macho::shorthand(name)
        {
            let id = self.macho_section(s.segment, s.section, Some((s.ty, s.attrs, s.reserved2)));
            let section = self.section_mut(id);
            section.align = section.align.max(s.align);
            return id;
        }
        let (kind, flags, align) = match name {
            ".text" => (SectionKind::Progbits, SectionFlags::text(), 1),
            ".data" => (SectionKind::Progbits, SectionFlags::data(), 1),
            ".bss" => (SectionKind::Nobits, SectionFlags::bss(), 1),
            ".rodata" => (SectionKind::Progbits, SectionFlags::rodata(), 1),
            _ => (SectionKind::Progbits, SectionFlags::default(), 1),
        };
        let n = self.interner.intern(name);
        self.get_or_create_section(n, kind, flags, align)
    }

    // ---- symbols ----------------------------------------------------------

    /// Creates an unnamed label pinned to the current position.
    pub(crate) fn anon_label(&mut self, span: Span) -> SymbolId {
        self.cur_section().seal();
        let frag = self.cur_section().next_frag_index();
        let section = self.cur;
        let n = self.symbols.len();
        let name = self.interner.intern(&format!(".L\u{0}anon.{n}"));
        let id = self.symbols.intern(name, span);
        let sym = self.symbols.get_mut(id);
        sym.value = SymbolValue::Label { section, frag };
        sym.def_span = span;
        self.symbols.mark_defined(id);
        id
    }

    pub(crate) fn define_label(&mut self, label: &LabelDef) -> Option<SymbolId> {
        let (id, span) = match *label {
            LabelDef::Named(name, span) => {
                let id = self.symbols.intern(name, span);
                (id, span)
            }
            LabelDef::Numeric(n, span) => {
                let id = self.symbols.local_define_slot(n, span, &mut self.interner);
                (id, span)
            }
        };
        if self.symbols.get(id).is_defined() && !self.predefined(id) {
            let prev = self.symbols.get(id).def_span;
            let name = self.display_name(id);
            self.diags.emit(
                Diagnostic::error(span, format!("symbol `{name}` is already defined"))
                    .with_note(prev, "previous definition is here"),
            );
            return None;
        }
        self.cur_section().seal();
        let frag = self.cur_section().next_frag_index();
        let section = self.cur;
        let in_code = self.section(section).flags.exec;
        let name = self.interner.get(self.symbols.get(id).name);
        let flags = self.arch.label_flags(&mut self.arch_state, name, in_code);
        let sym = self.symbols.get_mut(id);
        sym.value = SymbolValue::Label { section, frag };
        sym.def_span = span;
        sym.target_flags = flags;
        self.symbols.mark_defined(id);
        if self.dwarf.line.mark_labels {
            self.dwarf_label_defined();
        }
        if self.dwarf.line.source.on
            && let LabelDef::Named(name, _) = *label
        {
            let name = self.interner.get(name).to_string();
            self.dwarf_source_label(&name, span);
        }
        Some(id)
    }

    /// The name to show for a symbol in diagnostics.
    /// Not API.
    #[doc(hidden)]
    pub fn display_name(&self, id: SymbolId) -> String {
        let s = self.symbols.get(id);
        match s.local_number {
            Some(n) => format!("{n}"),
            None => {
                let raw = self.interner.get(s.name);
                match raw.split('\u{0}').next() {
                    Some(prefix) if raw.contains('\u{0}') => format!("{prefix}(anonymous)"),
                    _ => raw.to_string(),
                }
            }
        }
    }

    // ---- driving ----------------------------------------------------------

    pub fn assemble_path(&mut self, path: &std::path::Path) -> std::io::Result<()> {
        let file = self.sm.load(path)?;
        self.dwarf_start_generating(file);
        self.assemble_file(file);
        Ok(())
    }

    /// Assembles `src` as a source file called `name`; the first file
    /// assembled is the one `-g` describes.
    pub fn assemble_str(&mut self, name: &str, src: &str) {
        let file = self.sm.add(name, src);
        self.dwarf_start_generating(file);
        self.assemble_file(file);
    }

    /// Assembles `src` ahead of the source files, as definitions from the
    /// command line, which `-g` does not describe.
    pub fn assemble_prelude(&mut self, name: &str, src: &str) {
        let file = self.sm.add(name, src);
        self.assemble_file(file);
    }

    pub fn assemble_file(&mut self, file: FileId) {
        self.assemble_file_in(file, true);
    }

    /// Assembles `file`. With `own_conditionals`, a conditional it leaves
    /// open is an error; without, the file is a piece of the one that caused
    /// it, such as one statement rewritten by CC-RX `.DEFINE`, and may open a
    /// conditional that one closes.
    fn assemble_file_in(&mut self, file: FileId, own_conditionals: bool) {
        let mut reader = Reader {
            parser: Parser::new(file, self.lex_config()),
            epoch: self.lex_epoch,
        };
        let depth = self.cond.len();
        self.run(&mut reader);
        // Only the conditionals this file opened are its to close. One a
        // macro left open by `.exitm` ends with the expansion.
        if own_conditionals && self.cond.len() > depth {
            for c in self.cond.split_off(depth) {
                if !self.exiting_macro {
                    self.diags
                        .error(c.span, "unterminated `.if`, expected `.endif`");
                }
            }
        }
    }

    /// The lexing rules for source read from now on: the dialect's, and in
    /// the GNU dialect the active backend's comment characters and tuning.
    /// The 8-bit dialect needs the backend's mnemonics to tell a label in the
    /// first column from an instruction.
    pub(crate) fn lex_config(&self) -> LexConfig {
        let mut config = LexConfig::for_dialect(self.options.dialect);
        if self.options.dialect == Dialect::Gas {
            let c = self.arch.comments();
            config.line_comment = c.anywhere.to_vec();
            config.line_start_comment = c.line_start.to_vec();
            self.arch.tune_lexer(&mut config);
            // COFF names carry `@`: MSVC's mangled C++ names, clang's
            // `__xmm@...` constants, `@feat.00`. A relocation modifier is then
            // split off the end of a name by the expression parser.
            if self.options.format.is_coff() {
                config.at_in_idents = true;
            }
            // Darwin's arm64 assembly comments with `;`, which everywhere
            // else separates statements: `bl _f ; call it`.
            if self.options.format == crate::output::Format::MachO
                && crate::output::macho::Cpu::for_arch(self.arch.as_ref())
                    == Some(crate::output::macho::Cpu::Arm64)
            {
                config.stmt_sep.retain(|&c| c != ';');
                config.line_comment.push(";");
            }
        }
        if self.options.dialect == Dialect::EightBit {
            config.mnemonic = self.arch.mnemonics();
            config.equates = self.arch.equates();
        }
        config.bit_dot = self.arch.bit_addressing();
        config
    }

    /// The next statement of `reader`'s file, lexed by the rules in force
    /// now. A statement is read only once the one before it has been carried
    /// out, so an `.arch` switch, in the file itself or in anything it
    /// includes or expands, applies from the next statement on.
    pub(crate) fn next_statement(&mut self, reader: &mut Reader) -> Option<Statement> {
        if self.diags.saturated() {
            return None;
        }
        if reader.epoch != self.lex_epoch {
            *reader.parser.config_mut() = self.lex_config();
            reader.epoch = self.lex_epoch;
        }
        reader.parser.next_statement(
            &self.sm,
            &mut self.interner,
            &mut self.pool,
            &mut self.diags,
        )
    }

    /// Walks a file's statements, expanding the block constructs as it goes.
    ///
    /// `.macro` and the repeat directives consume statements that follow
    /// them, so the handlers read from `reader` too.
    fn run(&mut self, reader: &mut Reader) {
        // NASM source goes through its preprocessor a line at a time.
        if self.options.dialect == Dialect::Nasm {
            self.run_nasm(reader);
            return;
        }
        while let Some(stmt) = self.next_statement(reader) {
            let more = self.run_statement(&stmt, reader);
            // Reusing the token buffer keeps an allocation and a free per
            // statement from interleaving with the fragments this one made,
            // which layout walks later: that is worth about a tenth of the
            // time on a large file.
            reader.parser.recycle(stmt);
            if !more {
                return;
            }
        }
    }

    /// Carries out one statement for [`Assembler::run`]. Returns whether the
    /// walk goes on to the next one.
    fn run_statement(&mut self, stmt: &Statement, reader: &mut Reader) -> bool {
        if self.options.dialect == Dialect::CcRx
            && let Some(text) = self.ccrx_apply_defines(stmt)
        {
            if self.macro_depth >= 64 {
                self.diags.error(
                    stmt.span,
                    "`.DEFINE` replacement nested too deeply; is it recursive?",
                );
                return false;
            }
            let file = self.sm.add("<.DEFINE>".to_string(), text);
            self.macro_depth += 1;
            self.assemble_file_in(file, false);
            self.macro_depth -= 1;
            return !(self.exiting_macro || self.end_of_source || self.diags.saturated());
        }

        // A block construct inside a false conditional is not a block at
        // all; its statements are skipped one by one like everything else.
        if self.cond_active() {
            match self.block_kind(stmt) {
                Some(BlockKind::Macro) => {
                    self.define_macro(stmt, reader);
                    return true;
                }
                Some(BlockKind::Repeat(kind)) => {
                    self.expand_repeat(stmt, kind, reader);
                    return true;
                }
                Some(BlockKind::EndMacro) => {
                    self.diags
                        .error(stmt.span, "`.endm` without a matching `.macro`");
                    return true;
                }
                Some(BlockKind::EndRepeat) => {
                    self.diags.error(
                        stmt.span,
                        "`.endr` without a matching `.rept`, `.irp` or `.irpc`",
                    );
                    return true;
                }
                Some(BlockKind::ExitMacro) => {
                    if self.macro_depth == 0 {
                        self.diags.error(stmt.span, "`.exitm` outside a macro");
                    } else {
                        self.exiting_macro = true;
                    }
                    return false;
                }
                None => {
                    if self.try_expand_macro(stmt) {
                        return !(self.exiting_macro
                            || self.end_of_source
                            || self.diags.saturated());
                    }
                }
            }
        }

        self.process(stmt);
        !(self.exiting_macro || self.end_of_source || self.diags.saturated())
    }

    /// The directive a statement names, in GNU as spelling.
    ///
    /// In a vendor dialect a block keyword arrives as a bare word the parser
    /// could not tell from an instruction; it is translated here, so the
    /// statement walker only ever has to know one spelling.
    fn directive_name(&self, stmt: &Statement) -> Option<&str> {
        match &stmt.body {
            Some(Body::Directive { name, .. }) => {
                let text = self.interner.get(*name);
                let bare = text.strip_prefix('.').unwrap_or(text);
                Some(dialect::block_keyword(self.options.dialect, bare).unwrap_or(text))
            }
            Some(Body::Insn { mnemonic, .. }) if self.options.dialect.dotless_directives() => {
                dialect::block_keyword(self.options.dialect, self.interner.get(*mnemonic))
            }
            _ => None,
        }
    }

    /// The directives that open and close a macro body and a repeat block.
    ///
    /// CC-RL and CC-RH close all three with `.ENDM`, so each has to count the
    /// others as nesting too.
    fn block_delimiters(&self, repeat: bool) -> (&'static [&'static str], &'static [&'static str]) {
        match (self.options.dialect.is_cc(), repeat) {
            (true, _) => (&[".macro", ".rept", ".irp"], &[".endm"]),
            (false, false) => (&[".macro"], &[".endm", ".endmacro"]),
            (false, true) => (&[".rept", ".irp", ".irpc"], &[".endr"]),
        }
    }

    fn block_kind(&self, stmt: &Statement) -> Option<BlockKind> {
        Some(match self.directive_name(stmt)? {
            ".macro" => BlockKind::Macro,
            ".endm" | ".endmacro" => BlockKind::EndMacro,
            ".exitm" => BlockKind::ExitMacro,
            ".endr" => BlockKind::EndRepeat,
            ".rept" => BlockKind::Repeat(RepeatKind::Rept),
            ".irp" => BlockKind::Repeat(RepeatKind::Irp),
            ".irpc" => BlockKind::Repeat(RepeatKind::Irpc),
            _ => return None,
        })
    }

    /// Reads the statements of a block up to its terminator, returning the
    /// source text between the two.
    ///
    /// `opens` and `closes` name the directives that nest, so a `.rept` inside
    /// a `.macro` body does not end the macro. That is all the statements are
    /// read for: the body is kept as text and lexed again where it is
    /// expanded, by the rules in force there, so a macro defined before an
    /// `.arch` switch and used after it reads as the new target's source. Only
    /// where the block ends is decided by the rules in force here.
    fn capture_block(
        &mut self,
        reader: &mut Reader,
        opens: &[&str],
        closes: &[&str],
        open_span: Span,
    ) -> Option<String> {
        // Whole lines, from just past the opening statement: in Motorola
        // source the indentation is what makes `dc.b` an instruction rather
        // than a label, and a comment by the rules here may be code by the
        // rules the body is expanded under.
        let lo = self.sm.file(reader.parser.file()).start + reader.parser.offset() as u32;
        let mut last = lo;
        let mut depth = 1usize;
        while let Some(stmt) = self.next_statement(reader) {
            if let Some(name) = self.directive_name(&stmt) {
                if opens.contains(&name) {
                    depth += 1;
                } else if closes.contains(&name) {
                    depth -= 1;
                    if depth == 0 {
                        // Up to the terminator's line, or to the terminator
                        // itself where a body statement shares that line.
                        let line = self.sm.line_start_of(stmt.span.lo);
                        let hi = if line >= last { line } else { stmt.span.lo };
                        return Some(self.sm.span_text(Span::new(lo, hi)).to_string());
                    }
                }
            }
            last = stmt.span.hi;
        }
        self.diags.error(
            open_span,
            format!("unterminated block, expected `{}`", closes[0]),
        );
        None
    }

    /// The source text of a statement's arguments, which is what the macro
    /// machinery works in.
    fn arg_text(&self, stmt: &Statement) -> String {
        let rest = &stmt.toks[stmt.args.min(stmt.toks.len())..];
        match (rest.first(), rest.last()) {
            (Some(a), Some(b)) => self
                .sm
                .span_text(Span::new(a.span.lo, b.span.hi))
                .to_string(),
            _ => String::new(),
        }
    }

    fn define_macro(&mut self, stmt: &Statement, reader: &mut Reader) {
        let header = self.arg_text(stmt);
        let (mut name_text, mut params_text) = macros::split_macro_header(&header);
        let cc = self.options.dialect.renesas_cc();
        // Devpac writes `name macro`, with the name where a label goes.
        // CC-RL, CC-RH and CC-RX write `NAME .MACRO params`, with the name in
        // the symbol field, and CC-RH allows the parameters in parentheses
        // (CC-RL page 527, CC-RH page 460, CC-RX R20UT3248EJ0115 page 486).
        // vasm and AS write it that way in the 8-bit dialect too, and AS puts
        // the parameters after the keyword: `LOAD MACRO VAL,ADDR`.
        let eight_bit = self.options.dialect == Dialect::EightBit;
        let label_name = match (cc, stmt.symbol, stmt.labels.as_slice()) {
            (true, Some((n, _)), _) => Some(self.interner.get(n).to_string()),
            (false, _, [LabelDef::Named(n, _)]) if name_text.is_empty() || eight_bit => {
                Some(self.interner.get(*n).to_string())
            }
            _ => None,
        };
        if eight_bit && label_name.is_some() {
            params_text = header.trim();
        }
        if cc {
            name_text = "";
            params_text = header.trim();
            if let Some(inner) = params_text
                .strip_prefix('(')
                .and_then(|p| p.strip_suffix(')'))
            {
                params_text = inner;
            }
        }
        if let Some(n) = &label_name {
            name_text = n;
        }
        let (opens, closes) = self.block_delimiters(false);
        let Some(body) = self.capture_block(reader, opens, closes, stmt.span) else {
            return;
        };
        if name_text.is_empty() {
            self.diags.error(stmt.span, "`.macro` needs a name");
            return;
        }
        let params = match macros::parse_params(params_text) {
            Ok(p) => p,
            Err(msg) => {
                self.diags.error(stmt.span, msg);
                return;
            }
        };
        let name = self.interner.intern(&name_text.to_ascii_lowercase());
        if let Some(prev) = self.macros.get(&name) {
            let prev_span = prev.def_span;
            self.diags.emit(
                Diagnostic::error(stmt.span, format!("macro `{name_text}` is already defined"))
                    .with_note(prev_span, "previous definition is here")
                    .with_help("use `.purgem` to remove it first"),
            );
            return;
        }
        self.macros.insert(
            name,
            MacroDef {
                name,
                params,
                body,
                def_span: stmt.span,
            },
        );
    }

    fn expand_repeat(&mut self, stmt: &Statement, kind: RepeatKind, reader: &mut Reader) {
        let header = self.arg_text(stmt);
        let (opens, closes) = self.block_delimiters(true);
        let Some(body) = self.capture_block(reader, opens, closes, stmt.span) else {
            return;
        };

        // Each iteration is substituted separately and the results
        // concatenated, so the whole repeat becomes one expansion.
        let mut text = String::new();
        let cc = self.options.dialect.renesas_cc();
        match kind {
            RepeatKind::Rept => {
                let count = if cc {
                    self.eval_count(stmt)
                } else {
                    self.eval_text_count(&header, stmt.span)
                };
                let Some(count) = count else {
                    return;
                };
                for n in 0..count {
                    if cc {
                        // CC-RX's `..MACREP` counts the expansions from 1
                        // (R20UT3248EJ0115 page 490).
                        let iteration = [
                            ("..MACREP".to_string(), (n + 1).to_string()),
                            ("..macrep".to_string(), (n + 1).to_string()),
                        ];
                        let bindings = self.cc_local_bindings(&body, &iteration);
                        text.push_str(&self.cc_substitute(&body, &bindings));
                    } else {
                        text.push_str(&body);
                    }
                    text.push('\n');
                }
            }
            RepeatKind::Irp | RepeatKind::Irpc => {
                let (var, rest) = macros::split_macro_header(&header);
                if var.is_empty() {
                    self.diags
                        .error(stmt.span, "`.irp` needs a symbol name and a list of values");
                    return;
                }
                let rest = rest.trim().trim_start_matches(',').trim();
                let values: Vec<String> = if kind == RepeatKind::Irp {
                    macros::split_args(rest)
                        .into_iter()
                        .map(str::to_string)
                        .collect()
                } else {
                    // `.irpc` walks the characters of its argument, with any
                    // surrounding quotes stripped.
                    let raw = rest.trim_matches('"');
                    raw.chars().map(|c| c.to_string()).collect()
                };
                for v in values {
                    let bindings = vec![(var.to_string(), v)];
                    if cc {
                        let bindings = self.cc_local_bindings(&body, &bindings);
                        text.push_str(&self.cc_substitute(&body, &bindings));
                    } else {
                        text.push_str(&macros::substitute(&body, &bindings, self.macro_counter));
                    }
                    text.push('\n');
                }
            }
        }

        let label = match kind {
            RepeatKind::Rept => "rept",
            RepeatKind::Irp => "irp",
            RepeatKind::Irpc => "irpc",
        };
        // Each copy of the block is its lines and the newline that ends it.
        let copy_lines = body.matches('\n').count() as u32 + 1;
        self.expand(label, text, stmt.span, Some(copy_lines));
    }

    /// A CC-RL/CC-RH `.REPT` count, which is an absolute expression rather
    /// than a plain number (CC-RL page 529; CC-RH page 463).
    fn eval_count(&mut self, stmt: &Statement) -> Option<i64> {
        let mut cur = stmt.arg_cursor();
        if cur.at_end() {
            self.diags.error(stmt.span, "`.REPT` needs a count");
            return None;
        }
        let e = self.parse_expr(&mut cur)?;
        self.expect_end(&mut cur);
        let n = self.eval_absolute(e, "`.REPT` count")?;
        if n < 0 {
            self.diags
                .error(stmt.span, format!("`.REPT` count {n} is negative"));
            return None;
        }
        Some(n)
    }

    /// Binds each name a CC-RL/CC-RH `.LOCAL` in `body` declares to a fresh
    /// symbol name, after `params` (CC-RL page 528; CC-RH page 462). The
    /// manuals' own generated names start with a dot, which keeps them out of
    /// the way of user symbols; rsasm's do too.
    fn cc_local_bindings(
        &mut self,
        body: &str,
        params: &[(String, String)],
    ) -> Vec<(String, String)> {
        let mut out = params.to_vec();
        for name in macros::cc_locals(body) {
            if out.iter().any(|(p, _)| *p == name) {
                continue;
            }
            let fresh = format!(".LL{:08X}", self.cc_local_counter);
            self.cc_local_counter += 1;
            out.push((name, fresh));
        }
        out
    }

    /// Substitutes a CC-RL/CC-RH/CC-RX macro body: whole-word parameters, and
    /// the concatenation symbol, `?` for CC-RL (page 557), `~` for CC-RH (page
    /// 489) and `@` for CC-RX (R20UT3248EJ0115 page 497). CC-RX also replaces
    /// a parameter inside single quotes (page 487).
    fn cc_substitute(&self, body: &str, bindings: &[(String, String)]) -> String {
        let (concat, quoted) = match self.options.dialect {
            Dialect::CcRl => ('?', false),
            Dialect::CcRx => ('@', true),
            _ => ('~', false),
        };
        macros::substitute_words(body, bindings, concat, quoted)
    }

    fn eval_text_count(&mut self, text: &str, span: Span) -> Option<i64> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            self.diags.error(span, "`.rept` needs a count");
            return None;
        }
        match trimmed.parse::<i64>() {
            Ok(n) if n >= 0 => Some(n),
            Ok(_) => {
                self.diags.error(span, "`.rept` count must not be negative");
                None
            }
            Err(_) => {
                self.diags
                    .error(span, "`.rept` count must be a plain number");
                None
            }
        }
    }

    /// Expands `stmt` if its mnemonic names a macro. Returns whether it did.
    fn try_expand_macro(&mut self, stmt: &Statement) -> bool {
        let Some(Body::Insn { mnemonic, span }) = stmt.body else {
            return false;
        };
        if !self.macros.contains_key(&mnemonic) {
            return false;
        }
        // Labels on the invocation line belong to the call site, not to the
        // expansion, so they are defined before anything is substituted.
        for l in &stmt.labels {
            self.define_label(l);
        }
        let def = self.macros[&mnemonic].clone();
        let args = self.arg_text(stmt);
        let Some(bindings) = self.bind_macro_args(&def, &args, span) else {
            return true;
        };
        self.macro_counter += 1;
        let counter = self.macro_counter;
        let positional = self.options.dialect.dotless_directives();
        let text = if self.options.dialect.renesas_cc() {
            let bindings = self.cc_local_bindings(&def.body, &bindings);
            self.cc_substitute(&def.body, &bindings)
        } else if self.options.dialect == Dialect::EightBit && !def.params.is_empty() {
            // ca65 names its parameters in the body as plain words. With no
            // parameters declared, vasm's `\1` is what the body uses.
            macros::substitute_words(&def.body, &bindings, '\0', false)
        } else {
            macros::substitute_with(&def.body, &bindings, counter, positional)
        };
        let name = self.interner.get(def.name).to_string();
        self.expand(&format!("macro {name}"), text, span, None);
        true
    }

    /// Matches a call's arguments to a macro's parameters.
    fn bind_macro_args(
        &mut self,
        def: &MacroDef,
        args: &str,
        span: Span,
    ) -> Option<Vec<(String, String)>> {
        let mut bound: Vec<(String, Option<String>)> =
            def.params.iter().map(|p| (p.name.clone(), None)).collect();

        let pieces = macros::split_args(args);

        // The Renesas assemblers bind arguments by position only. CC-RH wants
        // exactly as many as there are parameters (page 460); CC-RL warns
        // about extra ones and leaves missing ones empty (page 527); CC-RX
        // warns about any mismatch, takes an argument in double quotes without
        // them, and counts the arguments in `..MACPARA` (R20UT3248EJ0115
        // pages 487 and 490).
        if self.options.dialect.renesas_cc() {
            let dialect = self.options.dialect;
            let name = self.interner.get(def.name).to_string();
            let (want, got) = (def.params.len(), pieces.len());
            if got != want {
                let msg = format!("macro `{name}` takes {want} argument(s), but {got} were given");
                if dialect == Dialect::CcRh {
                    self.diags.error(span, msg);
                    return None;
                }
                if got > want || dialect == Dialect::CcRx {
                    self.diags.warning(span, msg);
                }
            }
            let mut out: Vec<(String, String)> = def
                .params
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    let arg = pieces.get(i).copied().unwrap_or("");
                    let arg = match dialect {
                        Dialect::CcRx => arg
                            .strip_prefix('"')
                            .and_then(|a| a.strip_suffix('"'))
                            .unwrap_or(arg),
                        _ => arg,
                    };
                    (p.name.clone(), arg.to_string())
                })
                .collect();
            if dialect == Dialect::CcRx {
                // Reserved words are not case-sensitive (page 500).
                out.push(("..MACPARA".to_string(), got.to_string()));
                out.push(("..macpara".to_string(), got.to_string()));
            }
            return Some(out);
        }

        // A vendor-dialect macro declared without parameters takes any number
        // of arguments, referred to by position as `\1`, `\2` and so on.
        if def.params.is_empty() && self.options.dialect.dotless_directives() {
            return Some(
                pieces
                    .iter()
                    .enumerate()
                    .map(|(i, p)| ((i + 1).to_string(), (*p).to_string()))
                    .collect(),
            );
        }

        let mut positional = 0usize;
        for (i, piece) in pieces.iter().enumerate() {
            // A `:vararg` parameter swallows the rest of the line verbatim,
            // commas included, so it is matched before anything is split off.
            if let Some(vi) = def.params.iter().position(|p| p.vararg)
                && positional == vi
            {
                bound[vi].1 = Some(pieces[i..].join(", "));
                break;
            }
            match macros::split_named_arg(piece) {
                Some((name, value)) if def.param(name).is_some() => {
                    let idx = def
                        .params
                        .iter()
                        .position(|p| p.name == name)
                        .expect("just checked");
                    bound[idx].1 = Some(value.to_string());
                }
                _ => {
                    if positional >= def.params.len() {
                        self.diags.error(
                            span,
                            format!(
                                "macro `{}` takes {} argument(s), but more were given",
                                self.interner.get(def.name),
                                def.params.len()
                            ),
                        );
                        return None;
                    }
                    bound[positional].1 = Some((*piece).to_string());
                    positional += 1;
                }
            }
        }

        let mut out = Vec::with_capacity(bound.len());
        for (p, (name, value)) in def.params.iter().zip(bound) {
            let value = match value.or_else(|| p.default.clone()) {
                Some(v) => v,
                None if p.required => {
                    self.diags.error(
                        span,
                        format!(
                            "macro `{}` requires an argument for `{name}`",
                            self.interner.get(def.name)
                        ),
                    );
                    return None;
                }
                None => String::new(),
            };
            out.push((name, value));
        }
        Some(out)
    }

    /// Assembles expanded text as if it were an included file.
    ///
    /// It becomes a real entry in the source map, so a diagnostic inside a
    /// macro points at the expanded line and names the macro it came from.
    ///
    /// A repeated block gives the lines each copy takes, which lets a line
    /// table put an instruction on its line in the block.
    fn expand(&mut self, what: &str, text: String, span: Span, copy_lines: Option<u32>) {
        if self.macro_depth >= 64 {
            self.diags
                .error(span, "macro expansion nested too deeply; is it recursive?");
            return;
        }
        let name = format!("<{what}>");
        let file = self.sm.add(name, text);
        if self.options.debug_source {
            self.dwarf_expansion(file, span, copy_lines);
        }
        self.macro_depth += 1;
        self.assemble_file(file);
        self.macro_depth -= 1;
        // `.exitm` unwinds exactly one expansion.
        self.exiting_macro = false;
    }

    pub(crate) fn cond_active(&self) -> bool {
        self.cond.last().is_none_or(|c| c.active)
    }

    /// Whether the conditional *enclosing* the innermost one is active. An
    /// `.else` may only turn its branch on if everything around it is on.
    pub(crate) fn enclosing_cond_active(&self) -> bool {
        let n = self.cond.len();
        if n < 2 { true } else { self.cond[n - 2].active }
    }

    pub(crate) fn push_cond(&mut self, c: Cond) {
        self.cond.push(c);
    }

    pub(crate) fn pop_cond(&mut self) -> Option<Cond> {
        self.cond.pop()
    }

    pub(crate) fn cond_top(&self) -> Option<&Cond> {
        self.cond.last()
    }

    pub(crate) fn set_cond_active(&mut self, active: bool) {
        if let Some(c) = self.cond.last_mut() {
            c.active = active;
        }
    }

    pub(crate) fn mark_cond_taken(&mut self) {
        if let Some(c) = self.cond.last_mut() {
            c.taken = true;
        }
    }

    pub(crate) fn mark_cond_else(&mut self) {
        if let Some(c) = self.cond.last_mut() {
            c.seen_else = true;
        }
    }

    /// Assembles another file in place, as `.include` does.
    pub(crate) fn include(&mut self, path: &std::path::Path, span: Span) {
        if self.include_depth > 32 {
            self.diags.error(span, "`.include` nested too deeply");
            return;
        }
        let file = match self.sm.load(path) {
            Ok(f) => f,
            Err(e) => {
                self.diags
                    .error(span, format!("cannot read `{}`: {e}", path.display()));
                return;
            }
        };
        self.include_depth += 1;
        self.assemble_file(file);
        self.include_depth -= 1;
    }

    /// Switches the active architecture backend mid-file.
    ///
    /// The backend switched away from is kept, with its state, for the
    /// fragments it emitted: layout resolves their fixups in its byte order
    /// and pads their alignment with its no-ops, whatever is active by then.
    pub(crate) fn switch_arch(&mut self, arch: Box<dyn Architecture>) {
        // A literal pool belongs to the backend whose instructions load from
        // it, and its entries are that backend's data, so a pool still open
        // is written out before another backend takes over.
        self.flush_all_literals();
        let syntax = self.arch_state.syntax;
        let mut state = arch.initial_state();
        // A syntax choice is the user's, not the architecture's, so it carries
        // across a `.arch` switch when the new backend supports it.
        if arch.supports_syntax(syntax) {
            state.syntax = syntax;
        }
        // What the source has used so far is recorded for the header of an
        // object for that machine (SuperH's `e_flags`), so it carries over
        // from the last backend for the same machine, as `.arch sh4` after
        // `sh` code does, rather than starting again.
        let machine = arch.elf_machine();
        if let Some(slot) = (0..self.arch_slots.len())
            .rev()
            .find(|&s| self.slot_arch(s).0.elf_machine() == machine)
        {
            state.used = self.slot_arch(slot).1.used;
        }
        let old = (
            std::mem::replace(&mut self.arch, arch),
            std::mem::replace(&mut self.arch_state, state),
        );
        self.arch_slots[self.arch_slot as usize] = Some(old);
        self.arch_slot = self.arch_slots.len() as u32;
        self.arch_slots.push(None);
        for s in &mut self.sections {
            s.mark_arch(self.arch_slot);
        }
        // Comment characters and number spellings are the backend's, so every
        // file being read takes its rules again from its next statement.
        self.lex_epoch += 1;
        self.arch_prelude();
    }

    /// A backend `.arch` made active at some point, with its state: the
    /// current one's, or the one it had when the source switched away.
    fn slot_arch(&self, slot: usize) -> (&dyn Architecture, &ArchState) {
        match &self.arch_slots[slot] {
            Some((arch, state)) => (arch.as_ref(), state),
            None => (self.arch.as_ref(), &self.arch_state),
        }
    }

    /// The backend that emitted fragment `fi` of section `si`, and its state.
    pub(crate) fn frag_arch(&self, si: usize, fi: usize) -> (&dyn Architecture, &ArchState) {
        self.slot_arch(self.sections[si].arch_slot(fi) as usize)
    }

    /// How the file's instruction sizes are picked: the most refined
    /// [`Relaxation`](crate::arch::Relaxation) of any backend the source used.
    pub(crate) fn relaxation(&self) -> crate::arch::Relaxation {
        (0..self.arch_slots.len())
            .map(|s| self.slot_arch(s).0.relaxation())
            .max()
            .unwrap_or(crate::arch::Relaxation::FromLastPass)
    }

    /// The backend the output is for: the one the assembler was created
    /// with, whatever `.arch` switched to since. An object file has one
    /// machine, class and byte order.
    pub fn target(&self) -> &dyn Architecture {
        self.slot_arch(0).0
    }

    /// The last backend for the target's machine to be active, and its state,
    /// which is what describes the object's contents in its header (see
    /// [`ArchState::used`]).
    /// Not API.
    #[doc(hidden)]
    pub fn target_state(&self) -> (&dyn Architecture, &ArchState) {
        let machine = self.target().elf_machine();
        let slot = (0..self.arch_slots.len())
            .rev()
            .find(|&s| self.slot_arch(s).0.elf_machine() == machine)
            .unwrap_or(0);
        self.slot_arch(slot)
    }

    /// The sections the target writes into every object of its own accord,
    /// and the one `.gnu_attribute` brings into being; see
    /// [`Architecture::elf_attributes`](crate::arch::Architecture::elf_attributes).
    /// Not API.
    #[doc(hidden)]
    pub fn attribute_sections(&self) -> Vec<crate::arch::AttrSection> {
        let (arch, state) = self.target_state();
        let mut sections = arch.elf_attributes(state);
        if !self.attr_overrides.iter().any(|&(v, _, _)| v == "gnu") {
            return sections;
        }
        // `.gnu_attribute` is every ELF target's, and none of them writes the
        // vendor-neutral tags of its own accord, so the directive is what
        // brings them into being. GNU as puts them beside the processor's
        // where the target has a build-attributes section — ARM's and
        // RISC-V's hold an `aeabi` or `riscv` vendor section and then a
        // `gnu` one — and in a `.gnu.attributes` of its own where it has
        // none, as on MIPS and PowerPC.
        let vendors = sections.iter_mut().find_map(|s| match &mut s.body {
            crate::arch::AttrBody::Tags(vendors) => Some(vendors),
            crate::arch::AttrBody::Bytes(_) => None,
        });
        match vendors {
            Some(vendors) if !vendors.iter().any(|v| v.name == "gnu") => {
                vendors.push(crate::arch::AttrVendor {
                    name: "gnu",
                    tags: Vec::new(),
                });
            }
            Some(_) => {}
            None => sections.push(crate::arch::AttrSection::attributes(
                ".gnu.attributes",
                "gnu",
                Vec::new(),
            )),
        }
        sections
    }

    fn process(&mut self, stmt: &Statement) {
        // While a conditional is false, only the directives that can end it
        // are looked at.
        if !self.cond_active() {
            match &stmt.body {
                Some(Body::Directive { name, .. }) => {
                    let text = self.interner.get(*name);
                    if matches!(
                        text,
                        ".if"
                            | ".ifdef"
                            | ".ifndef"
                            | ".ifeq"
                            | ".ifne"
                            | ".else"
                            | ".elseif"
                            | ".endif"
                    ) || (self.options.dialect.renesas_cc()
                        && dialect::is_conditional(
                            self.options.dialect,
                            text.strip_prefix('.').unwrap_or(text),
                        ))
                    {
                        self.directive(stmt, *name);
                    }
                }
                // A vendor `ELSE`/`ENDIF` is a bare word, and has to be seen
                // here too or a false branch could never end.
                Some(Body::Insn { mnemonic, .. }) => {
                    let word = self.interner.get(*mnemonic);
                    if !self.options.dialect.renesas_cc()
                        && dialect::is_conditional(self.options.dialect, word)
                        && let Some(alias) = dialect::lookup(self.options.dialect, word)
                    {
                        self.run_alias(stmt, alias);
                    }
                }
                _ => {}
            }
            return;
        }

        self.stmt_labels.clear();
        for l in &stmt.labels {
            if let Some(id) = self.define_label(l) {
                self.stmt_labels.push(id);
            }
        }

        // `.` refers to where the statement starts, so the anonymous label
        // standing in for it has to exist before anything is emitted.
        // Every spelling of the location counter the dialect has counts, not
        // just `.`: Motorola writes `*` and Renesas `$`. A `*` that turns out
        // to be multiplication only costs an unused label.
        let d = self.options.dialect;
        if stmt.toks.iter().any(|t| {
            t.is_punct(Punct::Dot)
                || (d.star_is_here() && t.is_punct(Punct::Star))
                || (d.dollar_is_here() && t.is_punct(Punct::Dollar))
        }) || (matches!(stmt.body, Some(Body::Insn { .. }))
            && self
                .arch
                .operands_use_location(&self.interner, stmt.arg_cursor().rest()))
        {
            self.here_sym = Some(self.anon_label(stmt.span));
        }
        let mark = self.exprs.len();

        match &stmt.body {
            None => {}
            Some(Body::Directive { name, span }) => {
                let _ = span;
                self.directive(stmt, *name);
            }
            Some(Body::Insn { mnemonic, span }) => {
                // A bare word may be a vendor directive before it is an
                // instruction; see `dialect::lookup`. Not in CC-RL, CC-RH or
                // CC-RX, whose directives are all dotted and whose `BT` is a
                // branch.
                let alias = if self.options.dialect.renesas_cc() {
                    None
                } else {
                    dialect::lookup(self.options.dialect, self.interner.get(*mnemonic))
                };
                match alias {
                    Some(alias) => self.run_alias(stmt, alias),
                    None => self.instruction(stmt, *mnemonic, *span),
                }
            }
            Some(Body::Assign { name, span }) => {
                let mut cur = stmt.arg_cursor();
                if let Some(mut e) = self.parse_expr(&mut cur) {
                    // A CC-RL/CC-RH `.SET` takes an absolute expression of
                    // symbols already defined (CC-RL page 504, Table 5.12 on
                    // page 483; CC-RH page 435), so it is evaluated where it
                    // stands; that is what lets `CNT .SET CNT + 1` count.
                    let is_set = self.options.dialect.is_cc()
                        && stmt.args >= 1
                        && stmt.toks[stmt.args - 1]
                            .ident()
                            .is_some_and(|w| self.interner.get(w).eq_ignore_ascii_case(".set"));
                    // The 8-bit dialect's redefinable `DEFL`, `SET` and
                    // `.set` count the same way, where the value is a number
                    // already; one that refers to a label keeps it.
                    let counts = self.options.dialect == Dialect::EightBit
                        && stmt.args >= 1
                        && stmt.toks[stmt.args - 1].ident().is_some_and(|w| {
                            let w = self.interner.get(w);
                            [".set", "set", "defl"]
                                .iter()
                                .any(|k| w.eq_ignore_ascii_case(k))
                        });
                    let value = if is_set {
                        self.eval_absolute(e, "a `.SET` value")
                            .map(|v| self.exprs.int(v as u64, self.exprs.span(e)))
                    } else if counts && let Some(v) = self.eval_ref(e).ok().and_then(|v| v.as_abs())
                    {
                        Some(self.exprs.int(v as u64, self.exprs.span(e)))
                    } else {
                        Some(e)
                    };
                    if let Some(v) = value {
                        e = v;
                        self.set_symbol(*name, e, *span);
                        // Only `.SET` names may be defined again (CC-RL page
                        // 502; CC-RH page 436), and CC-RX has no `.SET`.
                        if self.options.dialect.renesas_cc() && !is_set {
                            let id = self.symbols.intern(*name, *span);
                            self.symbols.get_mut(id).redefinable = false;
                        }
                    }
                }
                self.expect_end(&mut cur);
            }
            Some(Body::Unknown { span }) => {
                self.diags
                    .error(*span, "expected a label, directive or instruction");
            }
            Some(Body::SetLocation { span }) => {
                let mut cur = stmt.arg_cursor();
                if let Some(e) = self.parse_expr(&mut cur) {
                    // `* = $1000` is `ORG $1000` to the 8-bit references.
                    if self.options.dialect == Dialect::EightBit {
                        self.origin(e, *span);
                    } else {
                        self.emit_org(e, 0, *span);
                    }
                }
                self.expect_end(&mut cur);
            }
        }

        self.bind_positional(mark);
        self.here_sym = None;
    }

    /// Replaces each reference this statement made to a symbol that already
    /// has a constant value with that value.
    ///
    /// A CC-RL/CC-RH `.SET` symbol may be redefined, and every use means the
    /// value it had there (CC-RL page 504; CC-RH page 435), as does one
    /// defined with `DEFL` or `SET` in the 8-bit dialect. Instruction
    /// operands are otherwise evaluated once the whole file is read, when only
    /// the last value is left.
    fn bind_set_values(&mut self, mark: usize) {
        for i in mark..self.exprs.len() {
            let ExprKind::Sym(name) = self.exprs.nodes[i].kind else {
                continue;
            };
            let Some(id) = self.symbols.lookup(name) else {
                continue;
            };
            // A backend's predefined name stays a reference, so that a label
            // of the same name later in the file takes it over.
            if let SymbolValue::Expr(e) = self.symbols.get(id).value
                && !self.predefined(id)
                && let Some(v) = self.eval_ref(e).ok().and_then(|v| v.as_abs())
            {
                self.symbols.get_mut(id).used = true;
                self.exprs.nodes[i].kind = ExprKind::Int(v as u64);
            }
        }
    }

    /// Replaces each reference, among the expression nodes from `mark` on, to
    /// a symbol not defined yet with 0, as CC-RX's `.IF` and `.ELIF` read one
    /// (R20UT3248EJ0115 page 495).
    pub(crate) fn ccrx_undefined_as_zero(&mut self, mark: usize) {
        for i in mark..self.exprs.len() {
            if let ExprKind::Sym(name) = self.exprs.nodes[i].kind
                && !self
                    .symbols
                    .lookup(name)
                    .is_some_and(|id| self.symbols.get(id).is_defined())
            {
                self.exprs.nodes[i].kind = ExprKind::Int(0);
            }
        }
    }

    /// Binds the location counter in one item of a data list to where that
    /// item is emitted, rather than to the start of the statement.
    ///
    /// `.long ., .` is two different addresses to GNU as, and `.word *, *`
    /// to ca65 and vasm: each `.` is read as its item is. Instructions keep
    /// the statement's start, which is also where they begin.
    pub(crate) fn bind_here_to_item(&mut self, e: ExprRef) {
        let mut here = Vec::new();
        let mut stack = vec![e];
        while let Some(r) = stack.pop() {
            match &self.exprs.get(r).kind {
                ExprKind::Here => here.push(r),
                ExprKind::Unary(_, a) | ExprKind::Modifier(_, a) => stack.push(*a),
                ExprKind::Binary(_, a, b) => stack.extend([*a, *b]),
                _ => {}
            }
        }
        if here.is_empty() {
            return;
        }
        let span = self.exprs.span(e);
        let label = self.anon_label(span);
        for r in here {
            self.exprs.set_kind(r, ExprKind::SymId(label));
        }
    }

    /// Rewrites `.` and `1f`/`1b` nodes created by this statement into direct
    /// symbol references, now that the statement's position is known.
    pub(crate) fn bind_positional(&mut self, mark: usize) {
        if self.exprs.len() == mark {
            return;
        }
        // A symbol exists from its first mention, not only from when an
        // expression naming it is first evaluated. Only a Mach-O object can
        // tell the difference: it lists its local symbols in the order they
        // came to exist, as llvm-mc does.
        if self.options.format == crate::output::Format::MachO {
            for i in mark..self.exprs.len() {
                if let ExprKind::Sym(name) = self.exprs.nodes[i].kind {
                    let span = self.exprs.nodes[i].span;
                    self.symbols.intern(name, span);
                }
            }
        }
        if self.options.dialect.is_cc() || self.options.dialect == Dialect::EightBit {
            self.bind_set_values(mark);
        }
        let Assembler {
            exprs,
            symbols,
            interner,
            here_sym,
            diags,
            ..
        } = self;
        let here = *here_sym;
        expr::bind_positional(exprs, mark, |kind, span| match kind {
            ExprKind::Here => match here {
                Some(id) => Some(ExprKind::SymId(id)),
                None => {
                    // Only reachable if `.` appeared without a `.` token, which
                    // the pre-scan should have caught.
                    diags.error(span, "`.` is not valid here");
                    None
                }
            },
            ExprKind::LocalRef(n, LocalDir::Forward) => {
                Some(ExprKind::SymId(symbols.local_forward(*n, span, interner)))
            }
            ExprKind::LocalRef(n, LocalDir::Backward) => match symbols.local_backward(*n, span) {
                Some(id) => Some(ExprKind::SymId(id)),
                None => {
                    diags.error(span, format!("no previous local label `{n}:`"));
                    None
                }
            },
            ExprKind::SectionStart => None,
            _ => None,
        });
    }

    // ---- expressions ------------------------------------------------------

    /// Not API.
    #[doc(hidden)]
    pub fn parse_expr(&mut self, cur: &mut Cursor<'_>) -> Option<ExprRef> {
        self.parse_expr_with_suffixes(cur, &[])
    }

    /// The same, allowing the relocation suffixes `suffixes` names; see
    /// [`crate::expr::ExprParser::paren_modifiers`].
    pub(crate) fn parse_expr_with_suffixes(
        &mut self,
        cur: &mut Cursor<'_>,
        suffixes: &'static [&'static str],
    ) -> Option<ExprRef> {
        let mut p = expr::ExprParser {
            arena: &mut self.exprs,
            interner: &mut self.interner,
            diags: &mut self.diags,
            dollar_is_here: self.options.dialect.dollar_is_here(),
            star_is_here: self.options.dialect.star_is_here(),
            dialect: self.options.dialect,
            bit_dot: self.arch.bit_addressing(),
            strings: Some(&self.pool),
            paren_modifiers: suffixes,
            upper_modifiers: self.arch.uppercase_modifiers(),
        };
        p.parse(cur)
    }

    /// Not API.
    #[doc(hidden)]
    pub fn expect_end(&mut self, cur: &mut Cursor<'_>) {
        if !cur.at_end() && !cur.is_empty() {
            let span = cur.remaining_span();
            self.diags.error(span, "unexpected trailing tokens");
        }
    }

    /// Evaluates an expression against the current symbol table.
    /// Not API.
    #[doc(hidden)]
    pub fn eval(&mut self, e: ExprRef) -> Result<Value, EvalError> {
        let Assembler { exprs, symbols, .. } = self;
        let mut env = Env {
            exprs,
            symbols,
            depth: 0,
        };
        expr::eval(exprs, e, &mut env)
    }

    /// Evaluates an expression without recording symbol uses, so it can be
    /// called from the output writers, which only have `&Assembler`.
    /// Not API.
    #[doc(hidden)]
    pub fn eval_ref(&self, e: ExprRef) -> Result<Value, EvalError> {
        let mut env = expr::SymbolEnv::new(&self.exprs, &self.symbols);
        expr::eval(&self.exprs, e, &mut env)
    }

    /// Evaluates an expression to a number, if it resolves to one.
    /// Not API.
    #[doc(hidden)]
    pub fn eval_const(&self, e: ExprRef) -> Option<i64> {
        self.resolve_value(self.eval_ref(e).ok()?)
    }

    /// The number a symbol stands for, if it has one.
    ///
    /// A difference of two labels counts: `len = end - start` is a constant
    /// even though neither end of it is.
    /// Not API.
    #[doc(hidden)]
    pub fn symbol_number(&self, id: SymbolId) -> Option<i64> {
        let v = self.eval_ref_symbol(id).ok()?;
        if let Some(n) = v.as_abs() {
            return Some(n);
        }
        self.resolve_value(v)
    }

    /// The section and offset a symbol resolves to, for symbols defined by
    /// `.set` in terms of a label.
    /// Not API.
    #[doc(hidden)]
    pub fn symbol_target_section(&self, id: SymbolId) -> Option<(SectionId, u64)> {
        let v = self.eval_ref_symbol(id).ok()?;
        let (Some(p), None) = (v.plus, v.minus) else {
            return None;
        };
        let addr = self.symbol_addr(p)?.wrapping_add(v.addend);
        let section = match self.symbols.get(p).value {
            SymbolValue::Label { section, .. } => section,
            _ => return None,
        };
        Some((
            section,
            addr.saturating_sub(self.section(section).addr as i64) as u64,
        ))
    }

    pub(crate) fn eval_ref_symbol(&self, id: SymbolId) -> Result<Value, EvalError> {
        let mut env = expr::SymbolEnv::new(&self.exprs, &self.symbols);
        env.symbol_value(id, Span::DUMMY)
    }

    /// Evaluates an expression that must be a plain number right now.
    /// Not API.
    #[doc(hidden)]
    pub fn eval_absolute(&mut self, e: ExprRef, what: &str) -> Option<i64> {
        match self.eval(e) {
            Ok(v) => match v.as_abs() {
                Some(n) => Some(n),
                None => {
                    let span = self.exprs.span(e);
                    self.diags
                        .error(span, format!("{what} must be an absolute value"));
                    None
                }
            },
            Err(err) => {
                self.diags.emit(err.into_diagnostic());
                None
            }
        }
    }

    pub(crate) fn set_symbol(&mut self, name: Name, e: ExprRef, span: Span) {
        let id = self.symbols.intern(name, span);
        let sym = self.symbols.get_mut(id);
        if sym.is_defined() && !sym.redefinable {
            let prev = sym.def_span;
            let name = self.display_name(id);
            self.diags.emit(
                Diagnostic::error(span, format!("symbol `{name}` is already defined"))
                    .with_note(prev, "previous definition is here"),
            );
            return;
        }
        let sym = self.symbols.get_mut(id);
        sym.value = SymbolValue::Expr(e);
        sym.def_span = span;
        sym.redefinable = true;
        self.symbols.mark_defined(id);
    }

    // ---- emitting ---------------------------------------------------------

    /// Not API.
    #[doc(hidden)]
    pub fn emit_bytes(&mut self, bytes: &[u8], span: Span) {
        if self.check_nobits(span) {
            return;
        }
        self.cur_section().emit_bytes(bytes, span);
    }

    /// `.bss`-style sections hold no data, so anything but zero fill is an
    /// error rather than being silently dropped.
    pub(crate) fn check_nobits(&mut self, span: Span) -> bool {
        if self.section(self.cur).kind == SectionKind::Nobits {
            let name = self.interner.get(self.section(self.cur).name).to_string();
            self.diags.error(
                span,
                format!("cannot emit data into `{name}`, which allocates no file space"),
            );
            return true;
        }
        false
    }

    pub(crate) fn emit_org(&mut self, target: ExprRef, fill: u8, span: Span) {
        self.cur_section().push(Fragment::new(
            FragKind::Org {
                target,
                fill,
                size: 0,
            },
            span,
        ));
    }

    /// Symbols that were referenced but never given a definition anywhere.
    /// Not API.
    #[doc(hidden)]
    pub fn report_undefined_locals(&mut self) {
        let missing: Vec<(SymbolId, u32)> = self.symbols.undefined_locals().collect();
        for (id, n) in missing {
            let span = self.symbols.get(id).first_use;
            self.diags
                .error(span, format!("no local label `{n}:` after this point"));
        }
    }

    fn instruction(&mut self, stmt: &Statement, mnemonic: Name, span: Span) {
        let operands = &stmt.toks[stmt.args.min(stmt.toks.len())..];
        self.instruction_tokens(operands, mnemonic, span, stmt.span);
    }

    /// Assembles an instruction and emits it into the current section.
    pub(crate) fn instruction_tokens(
        &mut self,
        operands: &[crate::lexer::Token],
        mnemonic: Name,
        mnemonic_span: Span,
        span: Span,
    ) {
        let Some((variants, relaxable, mut requests)) =
            self.assemble_instruction(operands, mnemonic, mnemonic_span, span)
        else {
            return;
        };
        // Motorola syntax aligns code as well as data; see `motorola_align`.
        if self.options.dialect == Dialect::Motorola {
            let unit = self.arch.align_unit();
            self.align_to(unit, span);
        }
        if self.check_nobits(span) {
            return;
        }
        // Padding the instruction asked to be placed in front of it.
        let (before, after): (Vec<_>, Vec<_>) = requests
            .drain(..)
            .partition(|r| matches!(r, crate::arch::Request::AlignCode { .. }));
        requests = after;
        if !before.is_empty() {
            let at = self.cur_section().next_frag_index();
            self.run_requests(before, span);
            self.reattach_labels(at);
        }
        self.map_code();
        // A statement that emits nothing, such as MSP430's `rpt`, which only
        // sets up the next instruction, is no row of its own, and leaves a
        // pending `.loc` to that instruction.
        let emits = variants.iter().any(|v| !v.bytes.is_empty());
        if emits && (self.dwarf.line.pending || self.dwarf.line.source.on) {
            let pos = (self.cur, self.cur_section().next_frag_index());
            self.dwarf_instruction(pos, &variants, span);
        }
        // A relaxable instruction ends GNU as's fragment, and with it the
        // record of which instruction set later padding is for — including
        // one the backend had only one encoding for but the reference
        // assembler still gave a fragment of its own.
        let settled = !relaxable && variants.len() == 1;
        self.cur_section().has_instructions = true;
        let idx = self.cur_section().emit_variants(variants, span);
        self.cur_section().frags[idx as usize].relaxable = relaxable;
        if settled && self.arch.pads_as_last_instruction() {
            let state = self.arch_state.clone();
            self.cur_section().nop_state = Some(state);
        }
        self.run_requests(requests, span);
    }

    /// Moves the labels written on the current statement's line, and its `.`,
    /// from fragment `from` past the padding just pushed there. Padding an
    /// instruction asks for goes between it and a label on its own line, as
    /// GNU as and llvm-mc both place it; a label on a line of its own stays
    /// in front of the padding.
    fn reattach_labels(&mut self, from: u32) {
        let section = self.cur;
        self.cur_section().seal();
        let to = self.cur_section().next_frag_index();
        let ids: Vec<SymbolId> = self
            .stmt_labels
            .iter()
            .copied()
            .chain(self.here_sym)
            .collect();
        for id in ids {
            let sym = self.symbols.get_mut(id);
            if let SymbolValue::Label { section: s, frag } = sym.value
                && s == section
                && frag == from
            {
                sym.value = SymbolValue::Label { section, frag: to };
            }
        }
    }

    /// The candidate encodings of an instruction, and whether its fragment
    /// is one relaxation revisits, without emitting anything.
    pub(crate) fn assemble_instruction(
        &mut self,
        operands: &[crate::lexer::Token],
        mnemonic: Name,
        mnemonic_span: Span,
        span: Span,
    ) -> Option<(
        Vec<crate::section::Variant>,
        bool,
        Vec<crate::arch::Request>,
    )> {
        let req = InsnRequest {
            mnemonic,
            mnemonic_span,
            operands,
            span,
        };
        // Disjoint field borrows keep the architecture object accessible while
        // it mutates the interner, expression arena and diagnostics.
        let Assembler {
            arch,
            interner,
            exprs,
            diags,
            pool,
            symbols,
            arch_state,
            options,
            sections,
            cur,
            sm,
            implicit_it_at,
            ..
        } = self;
        let dialect = options.dialect;
        let bit_dot = arch.bit_addressing();
        let upper_modifiers = arch.uppercase_modifiers();
        let implicit_it = options.implicit_it;
        let implicit_it_here = implicit_it_at.is_some_and(|(s, _)| s == *cur);
        let mut cx = AsmCtx {
            interner,
            exprs,
            diags,
            pool,
            symbols,
            state: arch_state,
            dialect,
            format: options.format,
            bit_dot,
            upper_modifiers,
            implicit_it,
            implicit_it_here,
            sections,
            section: *cur,
            relaxable: false,
            requests: Vec::new(),
            sources: sm,
        };
        let variants = arch.assemble(&mut cx, &req);
        let relaxable = cx.relaxable;
        let requests = std::mem::take(&mut cx.requests);
        // A symbol a relocation modifier implies exists from where the
        // modifier is read, so it takes its place in the symbol table ahead
        // of the targets that layout interns later.
        // NASM declares every external symbol, and makes nothing of the kind;
        // nor does a Mach-O or COFF object, which have no
        // `_GLOBAL_OFFSET_TABLE_` for one to name.
        let nasm = self.options.dialect == crate::lexer::Dialect::Nasm
            || self.options.format == crate::output::Format::MachO;
        for f in variants.iter().flatten().flat_map(|v| &v.fixups) {
            if nasm || self.options.format.is_coff() {
                break;
            }
            if let Some(m) = self.find_modifier(f.expr) {
                let name = self.interner.get(m).to_string();
                if let Some(needs) = self.arch.modifier_symbols(&name).needs {
                    let name = self.interner.intern(needs);
                    let id = self.symbols.intern(name, span);
                    self.symbols.get_mut(id).used = true;
                }
            }
        }
        variants.map(|v| (v, relaxable, requests))
    }
}

/// Expression evaluation environment, borrowing the parts of the assembler
/// evaluation needs.
struct Env<'a> {
    exprs: &'a ExprArena,
    symbols: &'a mut SymbolTable,
    depth: u32,
}

impl EvalCtx for Env<'_> {
    fn lookup_symbol(&mut self, name: Name, span: Span) -> Result<Value, EvalError> {
        let id = self.symbols.intern(name, span);
        self.symbol_value(id, span)
    }

    fn symbol_value(&mut self, id: SymbolId, span: Span) -> Result<Value, EvalError> {
        self.symbols.get_mut(id).used = true;
        match self.symbols.get(id).value.clone() {
            // An `.equ` chain is followed through, unless it ends at a label:
            // then the name stands for that address itself, as a label's
            // does. Both references go by that name to decide whether a
            // reference can be preempted and which symbol a relocation
            // names, so after `.set alias, sym` a local `alias` is resolved
            // and relocated against `sym`'s section even where `sym` is
            // global, and a global `alias` is relocated against itself.
            // Anything else stays symbolic until addresses are known.
            SymbolValue::Expr(e) => {
                if self.depth > 64 {
                    return Err(EvalError::new(span, "symbol definition is circular"));
                }
                self.depth += 1;
                let exprs = self.exprs;
                let v = expr::eval(exprs, e, self);
                self.depth -= 1;
                match v {
                    // A label, or an alias that already stands for one.
                    Ok(Value {
                        plus: Some(p),
                        minus: None,
                        ..
                    }) if matches!(
                        self.symbols.get(p).value,
                        SymbolValue::Label { .. } | SymbolValue::Expr(_)
                    ) =>
                    {
                        Ok(Value::sym(id, 0))
                    }
                    v => v,
                }
            }
            _ => Ok(Value::sym(id, 0)),
        }
    }

    fn here(&mut self, span: Span) -> Result<Value, EvalError> {
        Err(EvalError::new(span, "`.` cannot be used here"))
    }

    fn section_start(&mut self, span: Span) -> Result<Value, EvalError> {
        Err(EvalError::new(span, "`$$` is not supported yet"))
    }

    fn local_ref(&mut self, n: u32, _: LocalDir, span: Span) -> Result<Value, EvalError> {
        Err(EvalError::new(
            span,
            format!("local label `{n}` was not resolved"),
        ))
    }

    fn modifier(&mut self, _name: Name, inner: Value, _span: Span) -> Result<Value, EvalError> {
        // Relocation modifiers do not change the value, only the relocation
        // chosen for it, which the fixup resolver handles.
        Ok(inner)
    }
}
