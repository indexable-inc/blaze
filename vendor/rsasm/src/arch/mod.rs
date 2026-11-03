//! Architecture backends.
//!
//! Every CPU architecture implements [`Architecture`]. Backends are compiled
//! in behind cargo features and looked up by name, so a single source file can
//! switch between them with `.arch` and emit, say, x86 and ARM code into
//! different sections of the same object.
//!
//! [`lookup`] and [`available`] are how a caller names one. The backends
//! themselves — their opcode tables, operand parsers, encoders and relocation
//! numbers — are the crate's own and are not API; [`Architecture`] is the
//! documented seam between the core and one, and some of the types in its
//! signatures are internal too.

use crate::cursor::Cursor;
use crate::diag::DiagBag;
use crate::expr::{ExprArena, ExprParser, ExprRef};
use crate::intern::{Interner, Name};
use crate::lexer::{LitPool, Token};
use crate::section::{FragKind, SectionId, Variant};
use crate::source::Span;
use crate::symbol::{Binding, SymbolId, SymbolTable, SymbolValue};

// Backend internals — the opcode tables, the operand parsers, the encoders
// and the relocation numbers — are the crate's own, not API anyone depends
// on, so every backend module is crate-visible and only the `lookup` and
// `NAMES` below reach into it.
#[cfg(feature = "aarch64")]
pub(crate) mod aarch64;
#[cfg(feature = "arm")]
pub(crate) mod arm;
#[cfg(feature = "avr")]
pub(crate) mod avr;
#[cfg(feature = "m68k")]
pub(crate) mod m68k;
#[cfg(feature = "mips")]
pub(crate) mod mips;
#[cfg(feature = "msp430")]
pub(crate) mod msp430;
#[cfg(feature = "powerpc")]
pub(crate) mod powerpc;
#[cfg(feature = "riscv")]
pub(crate) mod riscv;
#[cfg(feature = "rl78")]
pub(crate) mod rl78;
#[cfg(feature = "rx")]
pub(crate) mod rx;
#[cfg(feature = "sparc")]
pub(crate) mod sparc;
#[cfg(feature = "superh")]
pub(crate) mod superh;
#[cfg(feature = "v850")]
pub(crate) mod v850;
#[cfg(feature = "x86")]
pub(crate) mod x86;

// Not API. These two are reached from the integration tests, which are their
// own crates, so `pub(crate)` is not enough for them; they are no more part
// of the supported surface than the backends above.
#[cfg(feature = "k78")]
#[doc(hidden)]
pub mod k78;
#[cfg(feature = "retro")]
#[doc(hidden)]
pub mod retro;

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum Endian {
    Little,
    Big,
}

impl Endian {
    pub fn write(self, out: &mut [u8], value: u64) {
        let n = out.len();
        for (i, slot) in out.iter_mut().enumerate() {
            let shift = match self {
                Endian::Little => i,
                Endian::Big => n - 1 - i,
            };
            *slot = (value >> (shift * 8)) as u8;
        }
    }

    /// Reads up to eight bytes back as an integer in this byte order.
    pub fn read(self, src: &[u8]) -> u64 {
        let n = src.len();
        let mut v = 0u64;
        for (i, b) in src.iter().enumerate() {
            let shift = match self {
                Endian::Little => i,
                Endian::Big => n - 1 - i,
            };
            v |= (*b as u64) << (shift * 8);
        }
        v
    }

    pub fn bytes(self, value: u64, n: usize) -> Vec<u8> {
        let mut v = vec![0u8; n];
        self.write(&mut v, value);
        v
    }
}

/// A bit of [`ArchState::features`] set by NASM's `default rel`: a memory
/// operand with no register in it is RIP-relative. Only x86 reads it.
#[doc(hidden)]
pub const FEATURE_DEFAULT_REL: u64 = 1 << 63;

/// One of the three target-selecting options GNU as spells `-march=`,
/// `-mcpu=` and `-mfpu=`; see [`Architecture::select_option`].
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum CpuOption {
    /// An architecture: `-march=armv7-a`, with `+`-separated extensions
    /// after it.
    Arch,
    /// A CPU, which on both ARM and AArch64 wins over an architecture given
    /// as well: `-mcpu=cortex-a53`, extensions likewise.
    Cpu,
    /// A floating-point and SIMD unit: `-mfpu=neon-vfpv4`.
    Fpu,
}

impl CpuOption {
    /// How the option is written, for a diagnostic.
    pub fn flag(self) -> &'static str {
        match self {
            CpuOption::Arch => "-march=",
            CpuOption::Cpu => "-mcpu=",
            CpuOption::Fpu => "-mfpu=",
        }
    }
}

/// Operand syntax flavour. Distinct from the [`crate::lexer::Dialect`]: GAS can
/// assemble Intel-syntax operands via `.intel_syntax`, keeping `#` comments.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum Syntax {
    /// `mov %rbx, %rax` — source first, sigils on registers and immediates.
    Att,
    /// `mov rax, rbx` — destination first.
    Intel,
}

/// Which strings start a comment, for GNU-style source.
///
/// This is a per-target choice in GNU as, not a dialect-wide one, because the
/// characters it would otherwise use are taken: ARM, AArch64 and SPARC all
/// write immediates as `#1`, so on those targets `#` can only be a comment at
/// the start of a line.
#[derive(Copy, Clone, Debug)]
#[non_exhaustive]
pub struct CommentSyntax {
    /// Start a comment anywhere on a line.
    pub anywhere: &'static [&'static str],
    /// Start a comment only in the first column (after leading whitespace).
    pub line_start: &'static [&'static str],
}

impl CommentSyntax {
    /// `#` and `//` everywhere: x86, RISC-V, MIPS and PowerPC.
    pub const HASH: CommentSyntax = CommentSyntax {
        anywhere: &["#", "//"],
        line_start: &[],
    };
}

/// What a relocation modifier makes of a value in a flat binary; see
/// [`Architecture::flat_modifier`].
#[derive(Copy, Clone, Debug)]
#[non_exhaustive]
pub enum FlatModifier {
    /// The value itself. `call foo@PLT` in an image with no PLT calls `foo`.
    Plain,
    /// The value relative to the field, even in a field that is otherwise
    /// absolute: x86-64's `R_X86_64_PLT32` is `L + A - P`, so `.long foo@PLT`
    /// in a static image is the distance to `foo`.
    PcRelative,
    /// Something only a linker creates, such as a GOT entry; refused.
    LinkerOnly,
    /// Part of the value, taken as it is written into the field: AVR's
    /// `lo8(x)` is the low byte of `x`, and `pm(x)` is `x` counted in words,
    /// which `x` has to be a multiple of `unit` for. Such a modifier is
    /// arithmetic rather than something only a linker can do, so it applies
    /// wherever the value is known, in an object as well as in a flat image.
    /// `write` is the field's [`FieldEncoding::Scatter`] function.
    ///
    /// [`FieldEncoding::Scatter`]: crate::section::FieldEncoding::Scatter
    Field {
        write: fn(u64, i64) -> u64,
        unit: u8,
    },
}

/// What a relocation modifier means for an instruction's fixup; see
/// [`Architecture::fixup_modifier_reloc`].
#[derive(Copy, Clone, Debug)]
#[non_exhaustive]
pub enum FixupModifier {
    /// The relocation the modifier selects.
    Reloc(u32),
    /// The backend read the modifier as it encoded the instruction, so the
    /// fixup already carries the relocation it chose: PowerPC picks one from
    /// the whole chain of modifiers and the shape of the field, and SPARC's
    /// thread-local operators are operand syntax of their own.
    Encoded,
    /// Not a modifier this backend has a relocation for on this field, which
    /// is refused rather than relocated as if the modifier were not there.
    Unknown,
}

/// A relocation modifier spelled the way this target's GNU as writes it, for
/// a diagnostic that names it back: `lo8()` on AVR, `%dtprel()` on AArch64,
/// `(GOT)` on ARM, and `x@got` everywhere else. The directive a
/// [`Architecture::percent_modifiers`] spelling belongs to is one that reads
/// them all, since the name is what is being described rather than the field.
pub(crate) fn written_modifier(arch: &dyn Architecture, name: &str) -> String {
    if arch.expr_modifiers().contains(&name) {
        format!("`{name}()`")
    } else if arch.percent_modifiers(".xword").contains(&name) {
        format!("`%{name}()`")
    } else if arch.data_paren_modifiers().contains(&name) {
        format!("`({name})`")
    } else {
        format!("`@{name}`")
    }
}

/// Something a backend asks the core to do to the section, which it cannot do
/// itself because sections belong to the core. Queued on
/// [`AsmCtx::requests`] and carried out once the statement is assembled.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum Request {
    /// Pads to a multiple of `align` bytes with zeros, even in code: GNU as's
    /// `frag_align (n, 0, 0)`, which ARM's `.arm` and literal pools use.
    AlignZero(u64),
    /// Raises the section's alignment to at least this many bytes, without
    /// padding anything: GNU as's `record_alignment`.
    RecordAlign(u64),
    /// Pads with no-ops to a multiple of `align` bytes, but only where that
    /// takes at most `max_skip`, as `.p2align` with a maximum does. Unlike
    /// the other requests, this one is carried out *before* the instruction
    /// that asked for it is emitted, so the padding goes in front of it and a
    /// label on the line before stays on the padding: both references do that
    /// to keep a POWER10 prefixed instruction from straddling a 64-byte
    /// boundary.
    AlignCode { align: u64, max_skip: u64 },
    /// A value the instruction loads from the section's literal pool; see
    /// [`AsmCtx::literal`].
    Literal(LiteralRequest),
    /// Writes out the section's literal pool here: `.ltorg`.
    FlushLiterals,
    /// Bytes a directive stands for, with the fixups into them, which the
    /// backend built itself. MIPS's `.cpload` and its relatives are each a
    /// short instruction sequence that sets `$gp` up, and its `.gpword` and
    /// thread-local data directives are a word carrying a relocation no
    /// generic data directive can spell. `code` says the bytes are
    /// instructions, so that the section counts as holding some.
    Emit { bytes: Variant, code: bool },
    /// A relocation that covers no bytes, placed where the section stands,
    /// so on whatever is emitted next: AArch64's `.tlsdesccall sym` marks the
    /// `blr` after it for the linker that rewrites the sequence, and ARM's
    /// `.tlsdescseq sym` the next instruction of a descriptor sequence. The
    /// fixup's kind is zero bytes wide.
    ///
    /// ARM's GNU as treats its directive as data for the mapping symbols,
    /// which the instruction after it then marks again as code: `as_data`
    /// says so. It also gives the relocation a field of `within` bytes that
    /// has to lie inside one of its fragments, so that many bytes must follow
    /// before anything that starts another — an alignment, a `.space`, the
    /// end of a relaxable instruction, or the end of the section. Zero asks
    /// for nothing more than something after the mark.
    Mark {
        expr: ExprRef,
        kind: crate::section::FixupKind,
        as_data: bool,
        within: u8,
    },
    /// What an attribute directive — ARM's `.eabi_attribute`, RISC-V's
    /// `.attribute`, PowerPC's `.gnu_attribute` — said one tag of the
    /// object's build attributes is. It replaces whatever
    /// [`Architecture::elf_attributes`] gave that tag of the section whose
    /// [`AttrBody::Tags`] names this vendor, and is added in tag order if
    /// the backend left it out.
    Attribute {
        vendor: &'static str,
        tag: u32,
        value: AttrValue,
    },
    /// The bytes this statement emits start with an `it` instruction the
    /// backend made up for a conditional Thumb instruction that had no block
    /// of its own; see [`crate::assembler::ImplicitIt`]. The core remembers
    /// which fragment they went into, because the `it`'s mask says how many
    /// instructions the block covers and every instruction that joins it has
    /// to write the halfword again.
    ImplicitIt,
    /// Writes the halfword of the `it` the last [`Request::ImplicitIt`]
    /// marked, which another instruction joining its block has widened. It
    /// goes into every variant of that fragment, since layout has yet to
    /// pick one, and is dropped where the fragment is not in this section:
    /// GNU as keeps its `it` state per section, so the block does not
    /// outlive one.
    ImplicitItMask([u8; 2]),
}

/// One value in an ELF build-attributes section.
#[derive(Clone, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum AttrValue {
    /// Written as a ULEB128.
    Int(u64),
    /// Written as a NUL-terminated string.
    Str(String),
}

/// What one of [`Architecture::elf_attributes`]'s sections holds.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum AttrBody {
    /// Bytes, written as they are: MIPS's `.reginfo` and V850's
    /// `.note.renesas` are structures, not attributes.
    Bytes(Vec<u8>),
    /// An ELF build-attributes section: one vendor section per vendor whose
    /// tags the object carries. The core encodes it, so that an attribute
    /// directive can change a tag first, or add a vendor: GNU as puts what
    /// `.gnu_attribute` says in `.ARM.attributes` and `.riscv.attributes`
    /// next to the processor's, where a target with no such section of its
    /// own gets a `.gnu.attributes` instead.
    Tags(Vec<AttrVendor>),
}

/// One vendor's tags in a build-attributes section.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct AttrVendor {
    pub name: &'static str,
    /// The `File` subsection's tags, in increasing tag order.
    pub tags: Vec<(u32, AttrValue)>,
}

/// A section the target's reference assembler writes into every object of
/// its own accord; see [`Architecture::elf_attributes`].
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct AttrSection {
    pub name: &'static str,
    /// `sh_type`, which is the target's own: `SHT_LOPROC + 3` for the
    /// processor attributes, `SHT_GNU_ATTRIBUTES` for `.gnu.attributes`, and
    /// `SHT_MIPS_REGINFO` and its relatives for MIPS.
    pub sh_type: u32,
    /// `sh_flags`, raw: `.reginfo` is allocated, and `.MIPS.options` also
    /// carries `SHF_MIPS_NOSTRIP`, which the section model has no name for.
    pub sh_flags: u64,
    pub align: u64,
    pub entsize: u64,
    pub body: AttrBody,
}

/// `SHT_LOPROC + 3`: the processor build attributes, which is what ARM,
/// RISC-V and MSP430 call their own section.
pub const SHT_ATTRIBUTES: u32 = 0x7000_0003;
/// `SHT_GNU_ATTRIBUTES`, the vendor-neutral `.gnu.attributes`.
pub const SHT_GNU_ATTRIBUTES: u32 = 0x6fff_fff5;

impl AttrSection {
    /// A build-attributes section with the processor attributes type, no
    /// flags and no alignment, which is how every such section is written.
    pub fn attributes(
        name: &'static str,
        vendor: &'static str,
        tags: Vec<(u32, AttrValue)>,
    ) -> AttrSection {
        AttrSection {
            name,
            sh_type: if name == ".gnu.attributes" {
                SHT_GNU_ATTRIBUTES
            } else {
                SHT_ATTRIBUTES
            },
            sh_flags: 0,
            align: 1,
            entsize: 0,
            body: AttrBody::Tags(vec![AttrVendor { name: vendor, tags }]),
        }
    }
}

/// An ELF build-attributes section: `'A'`, then a vendor section each,
/// holding a `File` subsection of `tag, value` pairs.
///
/// Both lengths count themselves and are written in the object's byte order,
/// so a big-endian PowerPC object's are not a little-endian ARM one's. The
/// order is the order of `tags`; GNU as and llvm-mc write them by increasing
/// tag number.
pub(crate) fn encode_attributes(vendors: &[AttrVendor], endian: Endian) -> Vec<u8> {
    let word = |v: u32| match endian {
        Endian::Little => v.to_le_bytes(),
        Endian::Big => v.to_be_bytes(),
    };
    let mut out = vec![b'A'];
    for vendor in vendors {
        let mut body = Vec::new();
        for (tag, value) in &vendor.tags {
            write_uleb(&mut body, u64::from(*tag));
            match value {
                AttrValue::Int(v) => write_uleb(&mut body, *v),
                AttrValue::Str(s) => {
                    body.extend_from_slice(s.as_bytes());
                    body.push(0);
                }
            }
        }
        // The `File` subsection: its tag, its length, and the pairs.
        let mut sub = vec![1u8];
        sub.extend_from_slice(&word(5 + body.len() as u32));
        sub.extend_from_slice(&body);
        out.extend_from_slice(&word(4 + vendor.name.len() as u32 + 1 + sub.len() as u32));
        out.extend_from_slice(vendor.name.as_bytes());
        out.push(0);
        out.extend_from_slice(&sub);
    }
    out
}

fn write_uleb(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// One use of a literal pool entry.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct LiteralRequest {
    /// The label the instruction refers to, defined where the entry lands.
    pub label: Name,
    pub value: Literal,
    /// Entry width in bytes.
    pub size: u8,
    /// Whether the number was written without a negation, which is GNU as's
    /// `X_unsigned` and part of what makes two of its ARM pool entries the
    /// same: `-1` and `0xffffffffffffffff` are one `i64` and two entries,
    /// and so are `4` and `8-4`. An entry that is not a number, and a pool
    /// whose backend groups its entries by width, has no use for it.
    pub unsigned: bool,
    pub span: Span,
}

/// A literal pool entry's value.
#[derive(Copy, Clone, Debug)]
#[non_exhaustive]
pub enum Literal {
    /// A number, as it was when the instruction was read.
    Const(i64),
    /// Anything else, which the entry relocates if it has to.
    Expr(ExprRef),
}

/// How a backend's literal pool is laid out; see
/// [`Architecture::literal_pool`] and the crate's `literals` module. The two
/// references differ, so each backend says which it follows.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum LiteralPool {
    /// A pool per entry width, written narrowest first, each run aligned to
    /// its own width: GNU as's AArch64 (`add_to_lit_pool` there keeps a
    /// `literal_pool` per size and `s_ltorg` walks them in order).
    ByWidth,
    /// One array of four-byte slots, filled in the order the literals were
    /// asked for: GNU as's ARM. An eight-byte entry takes two slots and has
    /// to start at an eight-aligned one, so a padding slot may go in front
    /// of it — and a later four-byte entry fills that padding slot rather
    /// than appending.
    Slots,
}

/// A PC-relative reference to a symbol defined in the fixup's own section,
/// as [`Architecture::defers_to_linker`] is asked about it.
#[derive(Copy, Clone, Debug)]
#[non_exhaustive]
pub struct SameSectionRef<'a> {
    /// The binding of the symbol as written: `alias` in `call alias` after
    /// `.set alias, target`, not `target`.
    pub binding: Binding,
    /// The relocation the reference would get, after any `@` modifier.
    pub reloc: u32,
    /// The `@` modifier, lowercased, if the reference was written with one.
    pub modifier: Option<&'a str>,
    /// Whether the instruction has more than one size to relax between.
    pub relaxable: bool,
}

/// How relaxation picks instruction sizes, as each reference assembler does;
/// see [`Architecture::relaxation`]. Where a file uses backends that relax
/// differently, the one listed last here wins for the whole file, since each
/// is a refinement of the ones before it.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[non_exhaustive]
pub enum Relaxation {
    /// Every size is chosen at once from the previous pass's addresses, and
    /// only grows. Layout starts each instruction at its smallest candidate,
    /// which finds the smallest layout whenever a form that reaches a target
    /// also reaches every nearer one.
    FromLastPass,
    /// Sizes are chosen walking each section in order, as GNU as's generic
    /// `relax_frag` chooses them, and only grow. The two can settle on
    /// different layouts: a branch that was out of reach at the previous
    /// pass's addresses, and is back in reach once an alignment has absorbed
    /// an earlier branch's growth, stays short when sized in order and grows
    /// when not. SuperH's GNU as works this way.
    InOrder,
    /// Sizes are picked afresh on every pass, walking each section in order,
    /// the way GNU as's ARM port picks them (`arm_relax_frag`). A target in a
    /// fragment the walk has yet to reach is taken to have moved by the
    /// growth so far, less what each alignment in between would absorb of it
    /// (GNU's `relaxed_symbol_addr`), which can let a fragment shrink back. A
    /// fragment that takes a larger candidate on a pass where nothing before
    /// it grew keeps that size for good, which is how GNU as stops the walk
    /// from cycling.
    EachPass,
    /// Every size is re-picked on each pass, and may shrink, with a target
    /// ahead moved by the growth so far as decided by its last-pass address.
    /// RX needs it: `bra.s` reaches 3 to 10 bytes forward, not 0 to 10, so a
    /// branch that was too close early on can come within reach once the code
    /// around it grows. GNU as's RX port re-picks every size for this reason,
    /// under a limit on how often one fragment may flip.
    Shrinking,
}

/// Mutable, architecture-specific assembler state.
///
/// Kept outside the [`Architecture`] object so backends stay `&self` and can be
/// shared, while `.code64`, `.arch` extensions and similar directives still
/// have somewhere to record what they changed.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ArchState {
    /// Operating mode width in bits (x86: 16, 32 or 64).
    pub bits: u8,
    pub syntax: Syntax,
    /// Bitset of optional instruction-set extensions the backend defines.
    pub features: u64,
    /// Which instructions the target selected has, as a bitset in the
    /// backend's own numbering: what `.arch`, `.cpu`, `.fpu` and
    /// `.arch_extension`, or `-march=`, `-mcpu=` and `-mfpu=`, settled on.
    /// Three words because that is what the two backends that gate on it
    /// need: GNU as's ARM feature set is four 32-bit words, and its AArch64
    /// one is 148 bits. Zero for a backend that assembles the same
    /// instructions whatever the source declares.
    pub cpu_features: [u64; 3],
    /// Set by `.intel_syntax noprefix` / `prefix`.
    pub intel_register_prefix: bool,
    /// Bitset of what the source has used so far, in the backend's own terms,
    /// for header fields that describe the object's contents rather than the
    /// options it was assembled with: GNU as for SuperH derives `e_flags` from
    /// the least capable CPU that has every instruction in the file.
    pub used: u64,
    /// What one statement leaves for the next in the backend's own terms:
    /// ARM's open `it` block, and a `.thumb_func` waiting for its label.
    pub private: u64,
}

/// What a PC-relative reference's target is, for
/// [`Architecture::interwork`].
#[derive(Copy, Clone, Debug)]
#[non_exhaustive]
pub struct InterworkTarget {
    /// The bits [`Architecture::label_flags`] gave the target's label.
    pub flags: u8,
    pub ty: crate::symbol::SymType,
    /// Defined in the section the reference is in.
    pub same_section: bool,
    /// Global or weak, or not defined here at all: another definition may
    /// take its place at link time.
    pub global: bool,
    /// Declared weak, whatever its visibility. GNU as resolves a PC-relative
    /// ARM load against a global symbol in the same section and refuses one
    /// against a weak symbol, which [`InterworkTarget::preemptible`] cannot
    /// tell apart.
    pub weak: bool,
    /// Global with default visibility, weak, or undefined: another object's
    /// definition can take its place even within one link.
    pub preemptible: bool,
    /// Whether the output is an object, with a linker still to come.
    pub relocatable: bool,
}

/// What a branch becomes once its target is known; see
/// [`Architecture::interwork`].
#[derive(Copy, Clone, Debug)]
#[non_exhaustive]
pub enum Interwork {
    /// Resolved, or relocated, as written.
    AsWritten,
    /// Left to the linker, however near the target is.
    Relocate,
    /// Something only a linker builds, described for the diagnostic.
    LinkerOnly(&'static str),
    /// Another instruction: the word, read in the target's byte order, put
    /// through `patch`, and the field written as `kind` describes.
    Becomes {
        patch: fn(u64) -> u64,
        kind: crate::section::FixupKind,
    },
}

/// One instruction to assemble, as the generic parser saw it.
#[non_exhaustive]
pub struct InsnRequest<'t> {
    pub mnemonic: Name,
    pub mnemonic_span: Span,
    /// Every token after the mnemonic, up to the end of the statement. The
    /// backend splits these itself, since operand grammar is arch-specific
    /// (x86 memory operands, ARM register lists, and so on).
    pub operands: &'t [Token],
    /// Span of the whole statement.
    pub span: Span,
}

impl InsnRequest<'_> {
    pub fn cursor(&self) -> Cursor<'_> {
        Cursor::new(self.operands)
    }
}

/// The slice of assembler state a backend may touch.
#[non_exhaustive]
pub struct AsmCtx<'a> {
    pub interner: &'a mut Interner,
    pub exprs: &'a mut ExprArena,
    pub diags: &'a mut DiagBag,
    pub pool: &'a LitPool,
    /// Read-only: backends resolve named constants, never define them.
    pub symbols: &'a SymbolTable,
    pub state: &'a mut ArchState,
    /// The source dialect, which decides operand spelling as much as lexing:
    /// the same m68k register is `%d0` to GNU as and `d0` in Motorola source.
    pub dialect: crate::lexer::Dialect,
    /// The object format being written, for
    /// [`AsmCtx::fixed_label_distance`]. Not API.
    #[doc(hidden)]
    pub format: crate::output::Format,
    /// [`Architecture::bit_addressing`] for the active backend, which decides
    /// whether `P1.3` in an expression is a bit address.
    pub bit_dot: bool,
    /// [`Architecture::uppercase_modifiers`] for the active backend, which
    /// decides whether `x@tlsgd` names a relocation modifier at all.
    pub upper_modifiers: bool,
    /// When a conditional Thumb instruction with no `it` block of its own
    /// gets one made up for it, which only the ARM backend reads.
    pub implicit_it: crate::assembler::ImplicitIt,
    /// Whether the `it` a [`Request::ImplicitIt`] made up is in this section,
    /// and so still a block this section's next instruction may join.
    pub implicit_it_here: bool,
    /// Read-only: what has been emitted so far, for
    /// [`AsmCtx::fixed_distance`].
    pub sections: &'a [crate::section::Section],
    /// The section the statement is assembled into.
    pub section: crate::section::SectionId,
    /// Set by a backend whose reference assembler would give this instruction
    /// a fragment that relaxation revisits, though it has one encoding here;
    /// see [`crate::section::Fragment::relaxable`].
    pub relaxable: bool,
    /// What the statement needs done to the section beyond its own bytes;
    /// see [`Request`].
    pub requests: Vec<Request>,
    /// Read-only: the source text, for an operand no token can carry, such as
    /// an m68k floating-point immediate (`#1.5`), which lexes as `1`, `.`,
    /// `5`.
    pub sources: &'a crate::source::SourceMap,
}

impl AsmCtx<'_> {
    /// Places `value` in the current section's literal pool and returns an
    /// expression for the address of its entry.
    ///
    /// The pool is written out at the next `.ltorg`, or at the end of the
    /// section, and equal entries are shared the way GNU as shares them: the
    /// same number, or the same symbol plus the same addend. Until then the
    /// entry's address is a label with a name no source can spell.
    pub fn literal(&mut self, value: Literal, size: u8, span: Span) -> ExprRef {
        self.literal_from(value, size, span, true)
    }

    /// The same, saying as well whether the number was written without a
    /// negation; see [`LiteralRequest::unsigned`].
    pub(crate) fn literal_from(
        &mut self,
        value: Literal,
        size: u8,
        span: Span,
        unsigned: bool,
    ) -> ExprRef {
        // The arena only grows, and grows below, so its length names each
        // entry once.
        let n = self.exprs.len();
        let label = self.interner.intern(&format!(".L\u{0}lit.{n}"));
        self.requests.push(Request::Literal(LiteralRequest {
            label,
            value,
            size,
            unsigned,
            span,
        }));
        self.exprs.alloc(crate::expr::ExprKind::Sym(label), span)
    }

    pub fn expr_parser(&mut self) -> ExprParser<'_> {
        ExprParser {
            arena: self.exprs,
            interner: self.interner,
            diags: self.diags,
            // `$` is an immediate marker in AT&T, not the location counter.
            dollar_is_here: self.dialect.dollar_is_here(),
            star_is_here: self.dialect.star_is_here(),
            dialect: self.dialect,
            bit_dot: self.bit_dot,
            strings: Some(self.pool),
            paren_modifiers: &[],
            upper_modifiers: self.upper_modifiers,
        }
    }

    pub fn name(&self, n: Name) -> &str {
        self.interner.get(n)
    }

    /// The first relocation modifier (`foo wrt ..got`, `foo@PLT`) in an
    /// expression, if any.
    pub fn find_modifier_for(&self, e: crate::expr::ExprRef) -> Option<Name> {
        use crate::expr::ExprKind::*;
        match &self.exprs.get(e).kind {
            Modifier(n, _) => Some(*n),
            Unary(_, a) => self.find_modifier_for(*a),
            Binary(_, a, b) => self
                .find_modifier_for(*a)
                .or_else(|| self.find_modifier_for(*b)),
            _ => None,
        }
    }

    /// The constant value of an expression, following `.set` definitions.
    ///
    /// Backends use this to choose an encoding width, so `.set n, 1` followed
    /// by `add $n, %rax` gets the same short form as `add $1, %rax`.
    pub fn constant(&self, e: crate::expr::ExprRef) -> Option<i64> {
        crate::expr::SymbolEnv::new(self.exprs, self.symbols).constant(e)
    }

    pub fn error(&mut self, span: Span, msg: impl Into<String>) {
        self.diags.error(span, msg);
    }

    /// The address `e` has now, in a flat image: a constant, or a label whose
    /// section starts at an address the source gave it (the 8-bit dialect's
    /// `ORG`) with nothing between that start and the label that could change
    /// size.
    ///
    /// ca65 reads a label after `.org` as the number it is, so it can choose
    /// zero-page addressing for one defined earlier, just as it does for a
    /// constant; this is what lets the 6502 backend do the same.
    pub fn address_now(&self, e: crate::expr::ExprRef) -> Option<i64> {
        let v = crate::expr::SymbolEnv::new(self.exprs, self.symbols).value(e)?;
        let at = |id: SymbolId| -> Option<i64> {
            let (section, frag) = self.label_position(id)?;
            let origin = self.sections[section.0 as usize].origin?;
            let d = self.fixed_distance((section, 0), (section, frag))?;
            Some(origin as i64 + d)
        };
        let plus = match v.plus {
            Some(p) => at(p)?,
            None => 0,
        };
        let minus = match v.minus {
            Some(m) => at(m)?,
            None => 0,
        };
        Some(v.addend + plus - minus)
    }

    /// Where a label was defined: its section, and the index of the fragment
    /// it starts.
    pub fn label_position(&self, id: SymbolId) -> Option<(SectionId, u32)> {
        match self.symbols.get(id).value {
            SymbolValue::Label { section, frag } => Some((section, frag)),
            _ => None,
        }
    }

    /// Where the statement being assembled starts, in the terms of
    /// [`AsmCtx::label_position`]; this is where its `.` is.
    pub fn here(&self) -> (SectionId, u32) {
        let s = &self.sections[self.section.0 as usize];
        (self.section, s.next_frag_index())
    }

    /// The distance from `from` to `to`, if nothing emitted between them can
    /// change size.
    ///
    /// This is when GNU as's expression parser folds a difference of labels
    /// to a constant as it reads it (`frag_offset_fixed_p`), which decides
    /// the encoding on targets whose backend chooses one by whether an operand
    /// is constant. An alignment, a `.org`, a `.space` or LEB128 value that is
    /// not a constant, or an instruction relaxation may resize between the
    /// two breaks it, even where layout later finds nothing to change.
    pub fn fixed_distance(&self, from: (SectionId, u32), to: (SectionId, u32)) -> Option<i64> {
        fixed_distance(self.sections, self.exprs, self.symbols, from, to)
    }

    /// [`AsmCtx::fixed_distance`] between two labels, for a value the object
    /// may still have to leave to the linker: in a Mach-O object the two also
    /// have to be in one atom, since the linker may move atoms apart, and
    /// llvm-mc leaves a difference that spans two to it. Positions carry the
    /// order the label was defined in ([`crate::symbol::Symbol::def_order`]),
    /// or `u32::MAX` for `.`, since of several labels at one place only those
    /// after a linker-visible one are in its atom.
    #[doc(hidden)]
    pub fn fixed_label_distance(
        &self,
        from: (SectionId, u32, u32),
        to: (SectionId, u32, u32),
    ) -> Option<i64> {
        if self.format == crate::output::Format::MachO
            && crate::output::macho::atom_starts_between(self.interner, self.symbols, from, to)
        {
            return None;
        }
        self.fixed_distance((from.0, from.1), (to.0, to.1))
    }
}

/// Replaces each difference of two labels in `e` with the distance between
/// them, where nothing emitted in between can change size.
///
/// GNU as folds such a difference as it reads the expression, so
/// `addl $_GLOBAL_OFFSET_TABLE_+(.-1b), %ebx` is a relocation against the
/// GOT with a constant addend, and `movl $(2f-1f), %eax` a constant that can
/// choose a short immediate. The expression evaluator has room for only one
/// symbol on either side of a sum, so without this the first would be an
/// error. AArch64 folds a PC-relative target the same way, which decides
/// what the field holds: `adr x0, b - a` is the number `b - a`, taken as the
/// offset a number written there would be. ARM folds a `vldr`'s `=expr`,
/// where the fold decides whether the line assembles at all: an eight-byte
/// pool entry has to be a number.
#[cfg(any(feature = "x86", feature = "aarch64", feature = "arm"))]
pub(crate) fn fold_differences(cx: &mut AsmCtx<'_>, e: ExprRef) -> ExprRef {
    use crate::expr::{BinOp, ExprKind};
    let node = cx.exprs.get(e).clone();
    match node.kind {
        ExprKind::Binary(op, l, r) => {
            if op == BinOp::Sub
                && let (Some(to), Some(from)) =
                    (expr_label_position(cx, l), expr_label_position(cx, r))
                && let Some(d) = cx.fixed_label_distance(from, to)
            {
                return cx.exprs.int(d as u64, node.span);
            }
            let (fl, fr) = (fold_differences(cx, l), fold_differences(cx, r));
            if (fl, fr) == (l, r) {
                e
            } else {
                cx.exprs.alloc(ExprKind::Binary(op, fl, fr), node.span)
            }
        }
        ExprKind::Unary(op, x) => match fold_differences(cx, x) {
            fx if fx == x => e,
            fx => cx.exprs.alloc(ExprKind::Unary(op, fx), node.span),
        },
        ExprKind::Modifier(name, x) => match fold_differences(cx, x) {
            fx if fx == x => e,
            fx => cx.exprs.alloc(ExprKind::Modifier(name, fx), node.span),
        },
        _ => e,
    }
}

/// Where a label an expression names was defined, or where `.` is, with the
/// order it was defined in; see [`AsmCtx::fixed_label_distance`].
#[cfg(any(feature = "x86", feature = "aarch64", feature = "arm"))]
fn expr_label_position(cx: &AsmCtx<'_>, e: ExprRef) -> Option<(SectionId, u32, u32)> {
    use crate::expr::ExprKind;
    let node = cx.exprs.get(e);
    let id = match node.kind {
        ExprKind::Here => {
            let (section, frag) = cx.here();
            return Some((section, frag, u32::MAX));
        }
        ExprKind::SymId(id) => id,
        ExprKind::Sym(name) => cx.symbols.lookup(name)?,
        ExprKind::LocalRef(n, crate::lexer::LocalDir::Backward) => {
            cx.symbols.local_backward(n, node.span)?
        }
        _ => return None,
    };
    let (section, frag) = cx.label_position(id)?;
    Some((section, frag, cx.symbols.get(id).def_order))
}

/// [`AsmCtx::fixed_distance`], for callers outside a backend.
pub(crate) fn fixed_distance(
    sections: &[crate::section::Section],
    exprs: &ExprArena,
    symbols: &SymbolTable,
    from: (SectionId, u32),
    to: (SectionId, u32),
) -> Option<i64> {
    if from.0 != to.0 {
        return None;
    }
    let (lo, hi, sign) = if from.1 <= to.1 {
        (from.1, to.1, 1)
    } else {
        (to.1, from.1, -1)
    };
    let constant = |e| crate::expr::SymbolEnv::new(exprs, symbols).constant(e);
    let frags = &sections[from.0.0 as usize].frags;
    let mut total = 0i64;
    for f in frags.get(lo as usize..hi as usize)? {
        if f.relaxable {
            return None;
        }
        total += match &f.kind {
            FragKind::Bytes { variants, .. } if variants.len() == 1 => {
                variants[0].bytes.len() as i64
            }
            FragKind::Space { size, .. } => constant(*size).filter(|n| *n >= 0)?,
            FragKind::Leb128 { value, signed, .. } => {
                let v = constant(*value)?;
                let n = if *signed {
                    crate::layout::sleb128(v).len()
                } else {
                    crate::layout::uleb128(v as u64).len()
                };
                n as i64
            }
            FragKind::Align { align, .. } if *align <= 1 => 0,
            _ => return None,
        };
    }
    Some(sign * total)
}

/// An alignment or `.org` in a code section, as the layout finally placed
/// it; see [`Architecture::layout_records`].
#[derive(Copy, Clone, Debug)]
#[non_exhaustive]
pub struct LayoutPlace {
    pub kind: PlaceKind,
    pub section: SectionId,
    /// The offset just past the padding, where what follows it starts.
    pub offset: u64,
    /// The fill byte the source gave, or 0 where it gave none.
    pub fill: u8,
}

/// What made a [`LayoutPlace`].
#[derive(Copy, Clone, Debug)]
#[non_exhaustive]
pub enum PlaceKind {
    /// `.align` or one of its relatives, or the padding that rounds a
    /// section up to its alignment, which is a power of two: this is the
    /// exponent.
    Align(u32),
    /// `.org`, or an assignment to `.`, with the constant part of its target
    /// (4 in `. = . + 4`, 0 in `.org label`).
    Org(i64),
}

/// A section of records about the finished layout; see
/// [`Architecture::layout_records`].
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct LayoutRecords {
    pub name: &'static str,
    pub bytes: Vec<u8>,
    /// The four-byte fields in `bytes` that hold where a place is, each as its
    /// offset and the index of the place: absolute references, which an
    /// object relocates against the place's section.
    pub refs: Vec<(u32, usize)>,
}

/// See [`Architecture::modifier_symbols`].
#[derive(Copy, Clone, Default, Debug)]
#[non_exhaustive]
pub struct ModifierSymbols {
    /// The name of a symbol to add, undefined, to the object.
    pub needs: Option<&'static str>,
    /// The target is a thread-local variable.
    pub tls: bool,
    /// The relocation names the target itself on a target that would
    /// otherwise relocate it against its section; see
    /// [`Architecture::relocates_globals_by_section`]. What a GOT or PLT
    /// modifier asks for is an entry of that symbol's own, which a section
    /// symbol and an offset cannot ask the linker for.
    pub names_target: bool,
}

/// How `.lcomm` reserves an object in `.bss`; see
/// [`Architecture::local_common`].
#[derive(Copy, Clone, Debug)]
#[non_exhaustive]
pub struct LocalCommon {
    /// The alignment an object gets from its size when the directive names
    /// none.
    pub align: fn(u64) -> u64,
    /// Whether `.lcomm` reads a third operand, a byte alignment. On ELF only
    /// PowerPC's GNU as and llvm-mc do; COFF's always does.
    pub takes_align: bool,
    /// The symbol's type.
    pub ty: crate::symbol::SymType,
}

impl LocalCommon {
    /// GNU as's `TC_IMPLICIT_LCOMM_ALIGNMENT`: the largest of 8, 4 and 2 that
    /// the size reaches, else 1. llvm-mc aligns nothing.
    pub const GNU: LocalCommon = LocalCommon {
        align: |size| match size {
            8.. => 8,
            4.. => 4,
            2.. => 2,
            _ => 1,
        },
        takes_align: false,
        ty: crate::symbol::SymType::Object,
    };

    /// No alignment at all, for the targets whose GNU as packs `.bss`.
    pub const PACKED: LocalCommon = LocalCommon {
        align: |_| 1,
        ..LocalCommon::GNU
    };
}

pub trait Architecture {
    /// Canonical name, as accepted by `--arch` and `.arch`.
    fn name(&self) -> &'static str;

    /// Alternative spellings accepted by `.arch`.
    fn aliases(&self) -> &'static [&'static str] {
        &[]
    }

    fn endian(&self) -> Endian;

    /// Width of a pointer in bytes, in the current mode.
    fn pointer_bytes(&self, state: &ArchState) -> u8;

    fn initial_state(&self) -> ArchState;

    fn supports_syntax(&self, syntax: Syntax) -> bool;

    /// `EM_*` value for ELF output.
    fn elf_machine(&self) -> u16;

    /// ELF relocation type for an `size`-byte data reference, or `None` if the
    /// architecture has no such relocation (which makes an unresolved
    /// reference of that width an error).
    fn data_reloc(&self, size: u8, pcrel: bool) -> Option<u32>;

    /// Relocation type selected by a source-level `@` modifier such as
    /// `foo@PLT`. `None` means the modifier is not recognised.
    fn modifier_reloc(&self, _name: &str, _size: u8, _pcrel: bool) -> Option<u32> {
        None
    }

    /// The data directives whose values carry a relocation modifier at all,
    /// spelled as the source spells them: with the dot for one of GNU as's
    /// own, without for one a dialect reads (`dc.l`).
    ///
    /// `None`, the default, is every data directive, which is how GNU as
    /// reads most targets: one `cons` serves `.long`, `.int`, `.4byte` and
    /// `.dc.l` alike, and whatever the target's `TC_CONS_FIX_NEW` reads it
    /// reads in all of them. m68k is the exception. Its `md_pseudo_table`
    /// replaces the `long` entry with an `m68k_elf_cons` of its own and
    /// leaves the other spellings to the plain `cons`, which has no `@` in
    /// its grammar, so `.int x@TLSLDO` is junk after the expression where
    /// `.long x@TLSLDO` is a thread-local offset.
    ///
    /// [`Architecture::modifier_reloc`] then says which relocation a value in
    /// one of the directives named here takes.
    fn directive_modifiers(&self) -> Option<&'static [&'static str]> {
        None
    }

    /// Whether a source-level `@` modifier is spelled in upper case and
    /// nothing else. Every GNU as port but one matches the suffix without
    /// regard to case; the m68k's compares it with `strncmp`, so `x@tlsgd`
    /// there is not a modifier at all but a `@` left in the expression, which
    /// is a syntax error.
    fn uppercase_modifiers(&self) -> bool {
        false
    }

    /// What a relocation modifier implies about the symbols of the object,
    /// beyond the relocation it selects: GNU as on x86 marks the target of a
    /// TLS modifier `STT_TLS`, and adds an undefined `_GLOBAL_OFFSET_TABLE_`
    /// for any modifier the linker needs a GOT for.
    fn modifier_symbols(&self, _name: &str) -> ModifierSymbols {
        ModifierSymbols::default()
    }

    /// The relocation a modifier selects for an instruction's fixup, which
    /// can depend on more than its size: i386 marks a `@GOT` load the linker
    /// may rewrite with a relocation of its own. The backend says so in the
    /// fixup it built. Defaults to [`Architecture::modifier_reloc`], with a
    /// name it does not know refused: a modifier that selected nothing used
    /// to leave the plain relocation behind, so `movl x@FOOBAR(%rip), %eax`
    /// quietly became an ordinary reference to `x`, which is a different
    /// program. A backend that reads its modifiers earlier answers
    /// [`FixupModifier::Encoded`] instead.
    fn fixup_modifier_reloc(&self, name: &str, kind: &crate::section::FixupKind) -> FixupModifier {
        match self.modifier_reloc(name, kind.size, kind.pcrel) {
            Some(r) => FixupModifier::Reloc(r),
            None => FixupModifier::Unknown,
        }
    }

    /// The format-neutral class a source-level `@` modifier gives the
    /// relocation of a fixup of `kind`, for a writer that does not number
    /// relocations as ELF does; see [`crate::reloc`]. `None`, the default,
    /// refuses the modifier there.
    #[doc(hidden)]
    fn modifier_class(
        &self,
        _name: &str,
        _kind: &crate::section::FixupKind,
    ) -> Option<crate::reloc::RelocClass> {
        None
    }

    /// What a source-level relocation modifier means where the value is
    /// known: in a flat binary, which has no relocation for it to choose and
    /// no linker to build what it names, and for a
    /// [`FlatModifier::Field`] modifier in an object too. Only asked about
    /// modifiers on fixups whose [`FixupKind::link`] is plain; the default
    /// refuses them all.
    ///
    /// [`FixupKind::link`]: crate::section::FixupKind::link
    fn flat_modifier(&self, _name: &str) -> FlatModifier {
        FlatModifier::LinkerOnly
    }

    /// True for a modifier whose value is the target's size rather than its
    /// address: x86's `@SIZE`, which is so far the only one.
    ///
    /// A size is a number the assembler can often settle itself, and GNU as
    /// does whenever the file both defines the symbol and keeps it to itself.
    /// Saying so here is what lets the core work that out and write the
    /// number, so a backend leaving this false always relocates.
    fn modifier_is_size(&self, _name: &str) -> bool {
        false
    }

    /// Relocation modifiers this target's GNU as writes around the whole of
    /// a data directive's value as a call, `.word pm(main)`, rather than as
    /// the `main@pm` suffix the GNU syntax otherwise uses. Only AVR has them.
    /// A name is one only where a `(` follows it, so a symbol of the same
    /// name still works; [`Architecture::modifier_reloc`] then says which
    /// relocation it picks and [`Architecture::flat_modifier`] what it
    /// computes.
    fn expr_modifiers(&self) -> &'static [&'static str] {
        &[]
    }

    /// Relocation modifiers this target's GNU as reads as a suffix in
    /// parentheses after a symbol in a data directive, `.word sym(GOT)`,
    /// rather than as the `sym@GOT` the GNU syntax otherwise uses. Only ARM
    /// has them. [`Architecture::modifier_reloc`] then says which relocation
    /// each picks, as it does for the `@` spelling.
    fn data_paren_modifiers(&self) -> &'static [&'static str] {
        &[]
    }

    /// Relocation modifiers this target's GNU as reads as a `%name(...)` call
    /// around the whole value of the data directive `directive`: AArch64's
    /// `.xword %dtprel(sym)`, which it reads in `.word`, `.long`, `.xword`
    /// and `.dword` alone. Written only in lower case, and only as the whole
    /// value. [`Architecture::modifier_reloc`] then says which relocation
    /// each picks, and in which widths.
    fn percent_modifiers(&self, _directive: &str) -> &'static [&'static str] {
        &[]
    }

    /// The PC-relative form of `reloc` in a `size`-byte field, for
    /// `sym - label` written where the field is otherwise absolute: that is
    /// `sym` measured from the field, which is what a PC-relative relocation
    /// holds. `None` refuses the difference.
    ///
    /// The default covers the plain data relocations, which is what
    /// `.long target - .` needs. A backend adds to it where an instruction
    /// field has a pair of its own: ARM's `:lower16:` is `R_ARM_MOVW_ABS_NC`
    /// against a symbol and `R_ARM_MOVW_PREL_NC` against a difference.
    fn pcrel_reloc(&self, reloc: u32, size: u8) -> Option<u32> {
        if reloc != 0 && Some(reloc) == self.data_reloc(size, false) {
            self.data_reloc(size, true)
        } else {
            None
        }
    }

    /// The relocation a reference gets from the symbol it names rather than
    /// from its own field: GNU as relocates a four-byte ARM data reference to
    /// `_GLOBAL_OFFSET_TABLE_` as `R_ARM_BASE_PREL`, the distance from the
    /// field to the GOT, which is how position-independent ARM code finds it.
    fn reloc_for_symbol(&self, reloc: u32, _name: &str) -> u32 {
        reloc
    }

    /// Comment characters in GNU-style source. Ignored for the NASM dialect,
    /// which uses `;` on every target.
    fn comments(&self) -> CommentSyntax {
        CommentSyntax::HASH
    }

    /// The alignment unit instructions and multi-byte data need, in bytes.
    ///
    /// The 68000 raises an address error on a word or long at an odd address,
    /// so its unit is 2. Dialects that align automatically — Motorola does,
    /// GNU as does not — use this to decide how far; everyone else needs 1.
    fn align_unit(&self) -> u64 {
        1
    }

    /// The dialect a source is assumed to be in when none is named.
    ///
    /// Amiga and Atari m68k source is overwhelmingly Motorola syntax, and
    /// nobody has written 78K0 source in anything but Renesas's own; for those
    /// targets defaulting to GNU as would reject the source people have.
    fn default_dialect(&self) -> crate::lexer::Dialect {
        crate::lexer::Dialect::Gas
    }

    /// Recognises this backend's mnemonics, lowercased, for the 8-bit dialect,
    /// in which a word in the first column is a label unless it names an
    /// instruction or a directive. `None`, the default, makes every such word
    /// that is not a directive a label.
    fn mnemonics(&self) -> Option<fn(&str) -> bool> {
        None
    }

    /// Adjusts GNU-dialect lexing beyond comment characters, for targets whose
    /// GNU as port differs: RL78's accepts `10H`, m68k's comments with `|`.
    /// Only called for the GNU dialect; the vendor dialects are fixed.
    fn tune_lexer(&self, _cfg: &mut crate::lexer::LexConfig) {}

    /// Width of `.word` in bytes.
    ///
    /// Not derivable from anything else: it is an assembler convention per
    /// target rather than a property of the instruction set. x86 keeps the
    /// 16-bit word of its 8086 origins, and so — less obviously — does
    /// PowerPC, while ARM, AArch64, RISC-V, MIPS and SPARC use 4. The default
    /// is 2 because that is the value for x86 and for every 8-bit target.
    fn word_bytes(&self) -> u8 {
        2
    }

    /// How relaxation picks the sizes of instructions with more than one
    /// encoding; see [`Relaxation`].
    fn relaxation(&self) -> Relaxation {
        Relaxation::FromLastPass
    }

    /// Whether `.short`, `.word`, `.int`, `.long` and `.quad` must each start
    /// on a boundary of their own width.
    ///
    /// SuperH's GNU as raises the section's alignment to the data's width,
    /// pads up to the boundary, and refuses the data as misaligned if that
    /// took any padding; the padding still moves everything after it while
    /// branches are sized. `.2byte`, `.4byte` and `.8byte` stay unaligned,
    /// and so do the SuperH spellings `.uaword`, `.ualong` and `.uaquad`,
    /// which a target returning true also accepts.
    fn aligns_data(&self) -> bool {
        false
    }

    /// Whether a plain number as a PC-relative target (`call 0x1000`) is an
    /// absolute address, which relocatable output must relocate against no
    /// symbol, rather than an offset into the current section.
    ///
    /// The references split on this. GNU as on x86, m68k, RL78 and RX takes
    /// the address; GNU as on SuperH, V850 and AVR, and llvm-mc everywhere
    /// except x86, measure from the start of the section, and resolve the
    /// branch, so a flat image measures it from there too. The 8-bit targets,
    /// which write no objects, take the address.
    fn pcrel_number_is_address(&self) -> bool {
        false
    }

    /// Whether a PC-relative reference to a symbol in the fixup's own section
    /// is left to the linker in relocatable output, rather than resolved.
    ///
    /// A global or weak symbol can be preempted: the linker may bind the name
    /// to a definition in another object, whether from a shared library or,
    /// for a weak one, a strong definition elsewhere. So on AArch64, ARM,
    /// PowerPC, MIPS, SPARC, RX and V850, and for calls on x86 and RISC-V, both
    /// references relocate a reference to a global or weak symbol, whatever
    /// its visibility, and resolve one to a local symbol, including a local
    /// `.set` alias of a global one; that is the default. A difference of two
    /// labels in one section is never affected: both references fold `.long
    /// weak - .` to a constant. A field no relocation can describe is
    /// resolved, unless the instruction has a larger form that one can.
    ///
    /// The ports whose reference differs override this, and every binding is
    /// checked in the `*-relocs.txt` corpora of `tools/gas-diff`,
    /// `tools/mc-diff` and `tools/xas-diff`.
    fn defers_to_linker(&self, r: &SameSectionRef<'_>) -> bool {
        r.binding != Binding::Local
    }

    /// What goes in the field of a PC-relative fixup at section offset `pc`
    /// that is relocated against a `binding` symbol in its own section, where
    /// that is not zero. A linker overwrites the field, so this only matters
    /// for matching the reference byte for byte: GNU as for V850 measures
    /// such a reference from the fixup as if the symbol were at 0, and writes
    /// `-pc`.
    fn relocated_pcrel_field(&self, _binding: Binding, _pc: u64) -> Option<i64> {
        None
    }

    /// The relocation pair, adding one symbol and subtracting another, that
    /// a data field holding a difference the file cannot fold is written as,
    /// if the target has one.
    ///
    /// Without one, only `sym - label` with the label in the field's own
    /// section can be relocated, as `sym` relative to the field. RISC-V's
    /// linker relaxation needs every difference it cannot see through kept as
    /// its two symbols, so llvm-mc writes `R_RISCV_ADD32`/`R_RISCV_SUB32` for
    /// that one too, and for a difference across sections.
    ///
    /// The width decides it for most fields, which is why MSP430 reads
    /// nothing else; a field that shares its bytes with something the
    /// relocation must not disturb needs a pair that sets the field rather
    /// than adding to it, and RISC-V tells those apart by
    /// [`FixupKind::reloc`](crate::section::FixupKind::reloc).
    fn difference_relocs(&self, _kind: &crate::section::FixupKind) -> Option<(u32, u32)> {
        None
    }

    /// Whether the subtrahend's relocation of a
    /// [`difference_relocs`](Architecture::difference_relocs) pair comes
    /// first. GNU as for MSP430 writes `R_MSP430_SYM_DIFF` before the value's
    /// relocation; llvm-mc for RISC-V writes `ADD` before `SUB`.
    fn difference_subtrahend_first(&self) -> bool {
        false
    }

    /// The relocations, subtracting and then setting a value, that a
    /// `.uleb128` of a difference of two labels in one section with
    /// `symbols_in` flags is left to the linker as, where the target leaves
    /// it at all. GNU as for MSP430 writes `R_MSP430_GNU_SUB_ULEB128` and
    /// `R_MSP430_GNU_SET_ULEB128` for one whose labels are in code, which its
    /// linker may relax; the value the file computes stays in the field.
    ///
    /// `code_moves` says whether a linker may delete bytes between the two
    /// labels, which is what RISC-V reads instead of the section's flags; see
    /// [`Architecture::moves_code`].
    fn uleb128_difference_relocs(
        &self,
        _symbols_in: &crate::section::SectionFlags,
        _code_moves: bool,
    ) -> Option<(u32, u32)> {
        None
    }

    /// Whether a difference of two labels in one section, in a field of
    /// `kind`, is still left to the linker as a
    /// [`difference_relocs`](Architecture::difference_relocs) pair rather
    /// than folded. `symbols_in` is the flags of the labels' section. GNU as
    /// for MSP430 keeps every difference of labels in code as a pair, since
    /// its linker may relax the code between them.
    ///
    /// `code_moves` says whether a linker may delete bytes between the two
    /// labels, which llvm-mc works out and acts on for RISC-V; see
    /// [`Architecture::moves_code`].
    fn defers_difference(
        &self,
        _kind: &crate::section::FixupKind,
        _symbols_in: &crate::section::SectionFlags,
        _code_moves: bool,
    ) -> bool {
        false
    }

    /// Whether a `.sleb128` of a difference the linker would have to work out
    /// is refused rather than written as the distance it is here.
    ///
    /// The pair a `.uleb128` leaves such a difference as sets an unsigned
    /// value, so nothing can describe a signed one. GNU as for RISC-V refuses
    /// the expression, as llvm-mc does; GNU as for MSP430 folds it and writes
    /// a number its own linker may then make wrong.
    fn refuses_signed_leb128_difference(&self) -> bool {
        false
    }

    /// Whether a relaxing linker may change the size of the sequence this
    /// fixup belongs to, so that the distance across it is not known until
    /// the link.
    ///
    /// Only a target that folds some differences of labels and defers others
    /// reads this; the ones that defer every difference of labels in code,
    /// MSP430 among them, need nothing of the sort. For RISC-V it is a fixup
    /// the object itself marks as the linker's to rewrite, a branch or jump
    /// left to the linker for the same reason, and the alignment
    /// `R_RISCV_ALIGN` hands over.
    fn moves_code(&self, _kind: &crate::section::FixupKind) -> bool {
        false
    }

    /// The field a call frame address advance goes in, for a target whose
    /// linker works the advance out: `bits` of a `size`-byte field hold it,
    /// the rest being the `DW_CFA_advance_loc` opcode it shares its first
    /// byte with.
    ///
    /// RISC-V is the one target that has these. The advance is a difference
    /// of two labels, so the relocation is the first of a
    /// [`difference_relocs`](Architecture::difference_relocs) pair, and it
    /// sets the field rather than adding to it: `R_RISCV_SET6` for the
    /// six bits packed beside the opcode, `R_RISCV_SET8` and its relatives
    /// for the wider forms.
    fn cfa_advance_field(&self, _size: u8, _bits: u8) -> Option<crate::section::FixupKind> {
        None
    }

    /// Whether a relocation against a global symbol defined in this object
    /// names the symbol's section plus an offset, as one against a local
    /// label does, rather than the symbol. GNU as for m68k does this for all
    /// but weak symbols.
    fn relocates_globals_by_section(&self) -> bool {
        false
    }

    /// The relocation to write for a PC-relative `reloc` that names a local
    /// label's section. GNU as for x86 writes a `call` to a local label as
    /// `PC32` rather than `PLT32`, there being no PLT entry to go through.
    fn section_relative_reloc(&self, reloc: u32) -> u32 {
        reloc
    }

    /// The alignment a section is given when it is created, before anything
    /// in it asks for more.
    ///
    /// This shows as `sh_addralign` in an object, and as where the section
    /// starts in a flat image. The references decide it mostly by name, and
    /// not all of them agree: llvm-mc aligns `.text` on every target, every
    /// executable section on AArch64, and `.data` and `.bss` too on MIPS,
    /// while GNU as aligns `.text`, `.data` and `.bss` on m68k, `.text` alone
    /// on MIPS and RISC-V, and on ARM and AArch64 whatever section an
    /// instruction is assembled into. Where both references exist, the one
    /// whose harness checks the target wins; each override says which.
    fn section_align(
        &self,
        _state: &ArchState,
        _name: &str,
        _flags: &crate::section::SectionFlags,
    ) -> u64 {
        1
    }

    /// `e_flags` for ELF output, given the state at the end of the source.
    fn elf_flags(&self, _state: &ArchState) -> u32 {
        0
    }

    /// `EI_OSABI` for ELF output. BFD writes `ELFOSABI_STANDALONE` for
    /// MSP430; everything else here is `ELFOSABI_NONE`.
    fn elf_osabi(&self) -> u8 {
        0
    }

    /// The sections the target's reference assembler writes into every
    /// object of its own accord, given the state at the end of the source:
    /// what the object needs of a linker and a loader rather than what the
    /// source put in it. MSP430's `.MSP430.attributes` records the
    /// instruction set and the memory model, ARM's `.ARM.attributes` the CPU
    /// and its floating-point unit, RISC-V's `.riscv.attributes` the ISA
    /// string, MIPS's `.reginfo` which registers the code touches, and
    /// V850's `.note.renesas` the ABI its objects may be linked with.
    ///
    /// An [`AttrBody::Tags`] section is encoded by the core, which also
    /// applies what `.eabi_attribute`, `.attribute` and `.gnu_attribute`
    /// asked for; see [`Request::Attribute`].
    fn elf_attributes(&self, _state: &ArchState) -> Vec<AttrSection> {
        Vec::new()
    }

    /// Undefined symbols an object refers to because it has a section of
    /// this name: GNU as for MSP430 adds `__crt0_movedata` for `.data` and
    /// `__crt0_init_bss` for `.bss`, so that the C runtime's code for setting
    /// each up is only linked in where some object needs it. Asked when a
    /// `.section` directive names the section, and again at the end for each
    /// section with contents.
    fn section_symbols(&self, _name: &str) -> &'static [&'static str] {
        &[]
    }

    /// Undefined symbols `.comm` and `.lcomm` refer to, for the same reason
    /// as [`Architecture::section_symbols`].
    fn common_symbols(&self) -> &'static [&'static str] {
        &[]
    }

    /// How `.lcomm` lays out what it reserves in an ELF object. Every ELF
    /// target's GNU as aligns an object to 8, 4 or 2 bytes by its size, as
    /// [`LocalCommon::GNU`] does, except where the target overrides that:
    /// AVR and MSP430 pack `.bss`, and PowerPC aligns every object to 8 and
    /// leaves its symbol untyped. llvm-mc packs `.bss` on every target, and
    /// GNU as is followed here, since the object the linker then places is
    /// the one the assembler of a GNU toolchain aligned. A COFF object
    /// follows llvm-mc, as it does in everything else.
    ///
    /// A `.comm` of a symbol `.local` named first is reserved the same way
    /// but aligned only as the directive asks, by both references; and a
    /// global `.comm` that names no alignment is given one by the core, the
    /// same on every ELF target.
    fn local_common(&self) -> LocalCommon {
        LocalCommon::GNU
    }

    /// Whether a relocation of type `reloc` keeps its addend in the relocated
    /// field, given whether the object's relocation sections are `RELA`.
    ///
    /// Under `REL` there is nowhere else to put it. A `RELA` target normally
    /// writes it into the entry and leaves the field zero, but the SuperH
    /// relocations are `partial_inplace` in BFD, whose linker then reads the
    /// field and ignores the entry's addend; GNU as writes both accordingly.
    fn addend_in_field(&self, _reloc: u32, rela: bool) -> bool {
        !rela
    }

    /// What a relocation of type `reloc` whose addend lives in its field
    /// (see [`Architecture::addend_in_field`]) writes there, given the addend
    /// and the value of the symbol the relocation names: its offset in its
    /// section where it is defined, zero where it is undefined or common.
    /// `by_section` says the relocation names a section rather than the
    /// symbol the source wrote, which is how a local label is relocated.
    ///
    /// Normally the addend. GNU as for ARM leaves the symbol's value in the
    /// field of some thread-local relocations as well; see `Arm::rel_field`,
    /// and MIPS writes `R_MIPS_GOT16`'s addend as a high half where the
    /// target is local; see `Mips::rel_field`.
    fn rel_field(&self, _reloc: u32, addend: i64, _symbol_value: i64, _by_section: bool) -> i64 {
        addend
    }

    /// Whether a `RELA` relocation of type `reloc` against a label defined in
    /// the file also leaves the label's value in the field, as well as in the
    /// entry's addend.
    ///
    /// GNU as for RX resolves the field as far as it can, which for a label
    /// in the object is all the way; against an undefined symbol it leaves
    /// the field zero. [`Architecture::addend_in_field`] takes precedence.
    fn local_value_in_field(&self, _reloc: u32) -> bool {
        false
    }

    /// The relocation to write for one of type `reloc` at offset `offset` in
    /// its section, for targets that have a variant for a field off its
    /// natural boundary: llvm-mc's SPARC writer uses `R_SPARC_UA32` for a
    /// four-byte field not on a multiple of four.
    fn reloc_at(&self, reloc: u32, _offset: u64) -> u32 {
        reloc
    }

    /// Whether a relocation of type `reloc` against a local label names the
    /// label itself rather than its section plus an offset.
    ///
    /// A linker reads the two the same, but under `REL` the offset lives in
    /// the relocated field, so the bytes differ: llvm-mc's ARM writer names
    /// the label for everything but `R_ARM_ABS32` and `R_ARM_PREL31`, and
    /// leaves the field zero.
    fn relocates_with_label(&self, _reloc: u32) -> bool {
        false
    }

    /// Whether `.align n` means 2^n bytes rather than n.
    ///
    /// GNU as decides this per target, for historical reasons only: x86 ELF,
    /// SPARC, m68k and RX count bytes, while ARM, AArch64, RISC-V, MIPS,
    /// PowerPC, RL78, V850 and SuperH count low-order zero bits. `.balign` and
    /// `.p2align` mean the same everywhere.
    fn align_is_log2(&self) -> bool {
        false
    }

    /// How an alignment in a code section is left to a linker that may delete
    /// instructions out from under it, for a target with linker relaxation:
    /// the boundary the assembler still reaches itself, and the fixup that
    /// asks the linker for the rest. `None`, the default, means the assembler
    /// aligns the section itself, which is right wherever nothing shortens a
    /// section after it is written.
    ///
    /// RISC-V is the one target that needs it. A relaxing linker deletes
    /// instructions, so an offset the assembler aligned is no longer aligned
    /// by the time the program runs; GNU as therefore emits the most padding
    /// the alignment could ever need and an `R_RISCV_ALIGN` saying how much
    /// of it the linker may take back. The assembler still aligns to the
    /// width of the shortest instruction, which no relaxation changes, since
    /// the padding it writes has to start on an instruction boundary.
    ///
    /// The padding is `align - unit` bytes of [`Architecture::nop_fill`], and
    /// the fixup goes at the start of it with `align - unit` as its value.
    /// Only a relocatable object can carry one.
    fn align_reloc(
        &self,
        _state: &ArchState,
        _align: u64,
    ) -> Option<(u64, crate::section::FixupKind)> {
        None
    }

    /// Whether GNU as rounds the end of a section with these flags up to the
    /// section's alignment.
    ///
    /// The Renesas ports (RL78, RX, V850) round every section, SuperH only
    /// code sections; the padding counts towards the section's size, so a
    /// linker placing the next object's section sees it.
    fn pads_section_tail(&self, _flags: &crate::section::SectionFlags) -> bool {
        false
    }

    /// The most a section tail is padded to, where
    /// [`Architecture::pads_section_tail`] pads it at all. ARM's GNU as pads
    /// code to its alignment only up to a word.
    fn section_tail_align_limit(&self) -> u64 {
        u64::MAX
    }

    /// Whether no-op padding is for the state the last instruction before it
    /// was assembled in, rather than the state in force where the padding is
    /// written. GNU as's ARM port pads so (its PR 9814), so that padding
    /// after Thumb code is Thumb no-ops even once `.arm` has been seen; its
    /// x86 port pads for the mode in force. See `Section::nop_state`.
    fn pads_as_last_instruction(&self) -> bool {
        false
    }

    /// The mapping symbol that marks code this backend emits in `state`, and
    /// the alignment in bytes such code gives its section, for a target whose
    /// ELF objects mark code and data apart: ARM's `$a` and `$t`. `None`, the
    /// default, for a target that does not.
    ///
    /// Where the marks go follows GNU as's ARM port; see
    /// `Assembler::map_code`.
    fn code_mapping(&self, _state: &ArchState) -> Option<(&'static str, u64)> {
        None
    }

    /// The mapping symbol that marks data, for a target with
    /// [`Architecture::code_mapping`].
    fn data_mapping(&self) -> &'static str {
        "$d"
    }

    /// Whether alignment padding written as zeros in a code section counts
    /// as code for the mapping symbols. GNU as decides this per target:
    /// `aarch64_init_frag` marks such a fragment as instructions, where
    /// `arm_init_frag` marks it as data. It shows between the runs of a
    /// literal pool, which are aligned to each run's width.
    fn align_padding_is_code(&self) -> bool {
        false
    }

    /// How the core lays out this backend's literal pools; see
    /// [`LiteralPool`] and the crate's `literals` module.
    fn literal_pool(&self) -> LiteralPool {
        LiteralPool::ByWidth
    }

    /// Bits to record on a label as it is defined, in the backend's own
    /// terms, from the state it is defined in: ARM marks a label in Thumb
    /// code, and the one a `.thumb_func` names. `name` is the label's, and
    /// `in_code` whether its section is executable.
    fn label_flags(&self, _state: &mut ArchState, _name: &str, _in_code: bool) -> u8 {
        0
    }

    /// Whether `name` is a directive the target spells instead of `.set`, and
    /// the [`Architecture::label_flags`] bits it puts on the alias: ARM's
    /// `.thumb_set`, whose alias is a Thumb function however its value was
    /// written, so that a call through it lands in Thumb state.
    fn alias_directive(&self, _name: &str) -> Option<u8> {
        None
    }

    /// The type and value an ELF symbol table gives a symbol with these
    /// label flags: an ARM Thumb function is `STT_FUNC` with its low bit set.
    fn elf_symbol(
        &self,
        _flags: u8,
        ty: crate::symbol::SymType,
        _defined: bool,
        value: u64,
    ) -> (crate::symbol::SymType, u64) {
        (ty, value)
    }

    /// Whether a relocation against this local label names the label rather
    /// than its section: an ARM linker needs to see a function symbol, to
    /// know which instruction set it is in.
    fn keeps_reloc_symbol(&self, _flags: u8, _ty: crate::symbol::SymType) -> bool {
        false
    }

    /// What an ARM linker adds to a symbol's value in a field of relocation
    /// type `reloc`, which a flat binary has to add itself: the low bit of a
    /// Thumb function's address.
    fn link_bias(&self, _reloc: u32, _flags: u8, _ty: crate::symbol::SymType) -> i64 {
        0
    }

    /// For a fixup whose [`LinkValue`](crate::section::LinkValue) is
    /// `Interwork(class)`, what the instruction becomes given its target:
    /// an ARM `bl` to a Thumb function is a `blx`, and a branch to a function
    /// in the other instruction set, which only a linker can make reach,
    /// keeps its relocation. `class` is the backend's own.
    fn interwork(&self, _class: u8, _target: &InterworkTarget) -> Interwork {
        Interwork::AsWritten
    }

    /// A section describing where the code sections were padded, for a
    /// linker that deletes code and has to keep those places where they
    /// belong: GNU as for AVR, preparing an object for linker relaxation,
    /// writes every `.align` and `.org` in a code section to `.avr.prop`.
    /// Given each of them in every executable section, section by section in
    /// order (the padding that rounds a section's end up to its alignment
    /// included), the backend returns the section to add, or `None` — the
    /// default — to add none. Only asked for relocatable output.
    fn layout_records(&self, _places: &[LayoutPlace]) -> Option<LayoutRecords> {
        None
    }

    /// Padding for `.align` in an executable section: real no-ops where the
    /// architecture has them, so padding stays executable.
    fn nop_fill(&self, state: &ArchState, len: u64) -> Vec<u8>;

    /// Assembles one instruction. Returns the candidate encodings, smallest
    /// first; layout picks among them. Returns `None` after reporting a
    /// diagnostic.
    fn assemble(&self, cx: &mut AsmCtx<'_>, insn: &InsnRequest<'_>) -> Option<Vec<Variant>>;

    /// Whether an instruction's operands refer to the instruction's own
    /// address without spelling it `.`, so that [`Architecture::assemble`]
    /// may build `ExprKind::Here` nodes for
    /// it. SuperH's `@(8,pc)` means `. + 8`.
    fn operands_use_location(&self, _interner: &Interner, _operands: &[Token]) -> bool {
        false
    }

    /// What this target's DWARF sections look like: whose conventions they
    /// follow, the line table's instruction unit and the CIE's constants.
    ///
    /// The object format is part of the question, unlike the relocation
    /// numbering: which reference writes DWARF for a target depends on the
    /// format — there is no GNU as for Mach-O and no llvm-mc for m68k — and
    /// the two write different CIEs. Only the targets that come out as
    /// something other than ELF have to read it.
    ///
    /// The default is a line table in GNU as's conventions counted in bytes,
    /// with no call frame information, which is what GNU as has for the
    /// targets where it has none (RX, RL78, V850) and what is safe for a
    /// target no reference writes DWARF for.
    fn dwarf(
        &self,
        _state: &ArchState,
        _format: crate::output::Format,
    ) -> crate::dwarf::DwarfTarget {
        crate::dwarf::DwarfTarget::lines_only(crate::dwarf::Flavor::Gnu, 1)
    }

    /// The Mach-O compact unwind word describing a frame whose `.cfi_*`
    /// directives became `insns`, where the machine has such a word
    /// (`MCAsmBackend::generateCompactUnwindEncoding`).
    ///
    /// Darwin's linker reads one word per frame from `__LD,__compact_unwind`
    /// and goes to the frame table only for a word that says to, so the word
    /// is what decides whether `__TEXT,__eh_frame` is written at all.
    /// `canonical_personality` is whether the frame's personality routine is
    /// one of the two the format keeps a slot for, which is the only thing
    /// outside the instructions that the word depends on.
    ///
    /// The default is `None`: the machine has no compact unwind table, and
    /// every frame is described in the frame table alone.
    fn macho_compact_unwind(
        &self,
        _insns: &[crate::dwarf::cfi::Insn],
        _canonical_personality: bool,
    ) -> Option<u32> {
        None
    }

    /// The number `.debug_frame` gives a register a `.cfi_*` directive
    /// named, where that is not the number the unwinding tables give it.
    ///
    /// A register in a `.cfi_*` directive is read in the numbering the
    /// exception tables use, and llvm-mc maps it back when it writes
    /// `.debug_frame` (`MCRegisterInfo::getDwarfRegNumFromDwarfEHRegNum`).
    /// The default is the number as it stands, which is right for every
    /// target with one numbering; Darwin's i386 is the one here with two.
    fn dwarf_debug_register(&self, _format: crate::output::Format, reg: u32) -> u32 {
        reg
    }

    /// Where the line table row of an instruction goes, as a distance back
    /// from the instruction's end, given its smallest encoding; `None`, the
    /// default, puts it at the start.
    ///
    /// GNU as's RX port records the row after it has written the
    /// instruction, measuring back from the end by the offset of the
    /// instruction's last relocated field rather than by its length, and a
    /// relaxed branch is longer by then; see `Rx::dwarf_row_back`.
    fn dwarf_row_back(&self, _smallest: &Variant) -> Option<u32> {
        None
    }

    /// The DWARF register number of a register named in a `.cfi_*`
    /// directive, lowercased and spelled as the source wrote it, with any
    /// prefix such as `%` or `$` still on.
    ///
    /// The object format is part of the question, as it is for
    /// [`Architecture::dwarf`]: a target's unwinding numbers are whatever the
    /// ABI that reads them says, and Darwin's i386 numbers two of the
    /// registers differently from the psABI.
    fn dwarf_register(
        &self,
        _state: &ArchState,
        _format: crate::output::Format,
        _name: &str,
    ) -> Option<u32> {
        None
    }

    /// Whether `name` (lowercased) could be one of this backend's
    /// instructions. NASM source may write a label without a colon, and a
    /// first word that is not an instruction is taken as one; a backend that
    /// cannot tell says yes, which makes the colon required.
    fn is_mnemonic(&self, _name: &str) -> bool {
        true
    }

    /// Handles an architecture-specific directive such as `.code64`. Returns
    /// false if the name is not one of this backend's directives.
    fn directive(&self, _cx: &mut AsmCtx<'_>, _name: &str, _cur: &mut Cursor<'_>) -> bool {
        false
    }

    /// Whether `.arch` — or `.cpu`, when `cpu` is set — naming this selects a
    /// CPU of this backend's own rather than switching to another backend,
    /// and records it in `state`. ARM's `.arch armv6` and `.cpu cortex-a8`
    /// name one of GNU as's ARM CPUs, and AArch64's `.arch armv8.5-a+memtag`
    /// one of its architectures, which decides what the object says it was
    /// built for and, on both, which instructions assemble; everywhere else
    /// `.arch` names a target and this stays `Ok(false)`. Asked before the
    /// name is looked up as a target, so a backend must claim only the names
    /// its reference assembler gives the directive.
    ///
    /// The `Err` is for a name the backend claims and then finds fault with,
    /// which is what an unknown `+name` suffix after an architecture it does
    /// know is; the caller reports it against the directive.
    fn selects_cpu(&self, _state: &mut ArchState, _name: &str, _cpu: bool) -> Result<bool, String> {
        Ok(false)
    }

    /// What `-march=`, `-mcpu=` or `-mfpu=` on the command line selects.
    ///
    /// GNU as's options are not the directives of the same name, so this is
    /// not [`Architecture::selects_cpu`]: `-march=armv7-a+mp` takes the
    /// extension suffixes that `.arch` refuses, and `-mfpu=neon` merges the
    /// unit into the CPU's own where `.fpu neon` replaces it. The `Err` is
    /// the diagnostic, which says what was wrong with the name — or, by
    /// default, that this backend has no such option.
    fn select_option(
        &self,
        _state: &mut ArchState,
        opt: CpuOption,
        _name: &str,
    ) -> Result<(), String> {
        Err(format!(
            "`{}` is not an option of the {} backend",
            opt.flag(),
            self.name()
        ))
    }

    /// Words that define a symbol where a label would go, beyond the `EQU`
    /// family every vendor dialect has: the MCS-51's `BIT`, `DATA`, `CODE`,
    /// `IDATA` and `XDATA`, which give a name a value and, on a real MCS-51
    /// toolchain, an address space with it. Consulted only in the 8-bit
    /// dialect, and only for a word that is not one of this backend's
    /// instructions.
    fn equates(&self) -> &'static [&'static str] {
        &[]
    }

    /// Source assembled ahead of the input the first time this backend is
    /// active, in `dialect`: names its reference assemblers predefine, such
    /// as the MCS-51's register names. Empty for most backends.
    fn prelude(&self, _dialect: crate::lexer::Dialect) -> String {
        String::new()
    }

    /// Whether `A.B` in an expression selects a bit of the byte at `A`, as it
    /// does on the MCS-51; see `expr::BinOp::BitAddr`. Off
    /// everywhere else, so `.` keeps its usual meaning.
    fn bit_addressing(&self) -> bool {
        false
    }
}

/// Looks up a backend by canonical name or alias.
pub fn lookup(name: &str) -> Option<Box<dyn Architecture>> {
    let lower = name.to_ascii_lowercase();
    #[cfg(feature = "x86")]
    if let Some(a) = x86::lookup(&lower) {
        return Some(a);
    }
    #[cfg(feature = "aarch64")]
    if let Some(a) = aarch64::lookup(&lower) {
        return Some(a);
    }
    #[cfg(feature = "arm")]
    if let Some(a) = arm::lookup(&lower) {
        return Some(a);
    }
    #[cfg(feature = "riscv")]
    if let Some(a) = riscv::lookup(&lower) {
        return Some(a);
    }
    #[cfg(feature = "powerpc")]
    if let Some(a) = powerpc::lookup(&lower) {
        return Some(a);
    }
    #[cfg(feature = "mips")]
    if let Some(a) = mips::lookup(&lower) {
        return Some(a);
    }
    #[cfg(feature = "sparc")]
    if let Some(a) = sparc::lookup(&lower) {
        return Some(a);
    }
    #[cfg(feature = "retro")]
    if let Some(a) = retro::lookup(&lower) {
        return Some(a);
    }
    #[cfg(feature = "m68k")]
    if let Some(a) = m68k::lookup(&lower) {
        return Some(a);
    }
    #[cfg(feature = "v850")]
    if let Some(a) = v850::lookup(&lower) {
        return Some(a);
    }
    #[cfg(feature = "rl78")]
    if let Some(a) = rl78::lookup(&lower) {
        return Some(a);
    }
    #[cfg(feature = "rx")]
    if let Some(a) = rx::lookup(&lower) {
        return Some(a);
    }
    #[cfg(feature = "superh")]
    if let Some(a) = superh::lookup(&lower) {
        return Some(a);
    }
    #[cfg(feature = "k78")]
    if let Some(a) = k78::lookup(&lower) {
        return Some(a);
    }
    #[cfg(feature = "avr")]
    if let Some(a) = avr::lookup(&lower) {
        return Some(a);
    }
    #[cfg(feature = "msp430")]
    if let Some(a) = msp430::lookup(&lower) {
        return Some(a);
    }
    let _ = lower;
    None
}

/// Every architecture name this build can assemble, for `--list-arch` and for
/// the "unknown architecture" diagnostic.
pub fn available() -> Vec<&'static str> {
    // `mut` is only needed when at least one backend feature is on.
    #[allow(unused_mut)]
    let mut v = Vec::new();
    #[cfg(feature = "x86")]
    v.extend_from_slice(x86::NAMES);
    #[cfg(feature = "aarch64")]
    v.extend_from_slice(aarch64::NAMES);
    #[cfg(feature = "arm")]
    v.extend_from_slice(arm::NAMES);
    #[cfg(feature = "riscv")]
    v.extend_from_slice(riscv::NAMES);
    #[cfg(feature = "powerpc")]
    v.extend_from_slice(powerpc::NAMES);
    #[cfg(feature = "mips")]
    v.extend_from_slice(mips::NAMES);
    #[cfg(feature = "sparc")]
    v.extend_from_slice(sparc::NAMES);
    #[cfg(feature = "retro")]
    v.extend_from_slice(retro::NAMES);
    #[cfg(feature = "m68k")]
    v.extend_from_slice(m68k::NAMES);
    #[cfg(feature = "v850")]
    v.extend_from_slice(v850::NAMES);
    #[cfg(feature = "rl78")]
    v.extend_from_slice(rl78::NAMES);
    #[cfg(feature = "rx")]
    v.extend_from_slice(rx::NAMES);
    #[cfg(feature = "superh")]
    v.extend_from_slice(superh::NAMES);
    #[cfg(feature = "k78")]
    v.extend_from_slice(k78::NAMES);
    #[cfg(feature = "avr")]
    v.extend_from_slice(avr::NAMES);
    #[cfg(feature = "msp430")]
    v.extend_from_slice(msp430::NAMES);
    v
}

/// The backend used when nothing is specified: the host architecture if this
/// build supports it, else the first available one.
pub fn default_arch() -> Option<Box<dyn Architecture>> {
    #[cfg(all(feature = "x86", target_arch = "x86_64"))]
    if let Some(a) = lookup("x86-64") {
        return Some(a);
    }
    available().first().and_then(|n| lookup(n))
}
