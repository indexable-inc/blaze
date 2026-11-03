//! Sections, fragments and fixups.
//!
//! Output is built as a list of *fragments* per section. A fragment whose size
//! is not yet known (an alignment, or a branch that may need a longer
//! displacement) keeps enough information for the layout loop to re-decide its
//! size until everything is stable.

use crate::expr::ExprRef;
use crate::intern::Name;
use crate::reloc::RelocClass;
use crate::source::Span;
use crate::symbol::SymbolId;

#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct SectionId(pub u32);

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum SectionKind {
    /// Occupies space in the output file.
    Progbits,
    /// Zero-filled at load time (`.bss`).
    Nobits,
    Note,
}

#[derive(Copy, Clone, PartialEq, Eq, Default, Debug)]
#[non_exhaustive]
pub struct SectionFlags {
    pub alloc: bool,
    pub write: bool,
    pub exec: bool,
    pub merge: bool,
    pub strings: bool,
    pub tls: bool,
    pub group: bool,
}

impl SectionFlags {
    pub fn text() -> SectionFlags {
        SectionFlags {
            alloc: true,
            exec: true,
            ..Default::default()
        }
    }
    pub fn data() -> SectionFlags {
        SectionFlags {
            alloc: true,
            write: true,
            ..Default::default()
        }
    }
    pub fn rodata() -> SectionFlags {
        SectionFlags {
            alloc: true,
            ..Default::default()
        }
    }
    pub fn bss() -> SectionFlags {
        SectionFlags {
            alloc: true,
            write: true,
            ..Default::default()
        }
    }
}

/// How a resolved value is placed into the bytes a fixup covers.
///
/// Not comparable: the `Scatter` variant holds a function pointer, and
/// comparing those says nothing useful.
#[derive(Copy, Clone, Debug, Default)]
#[non_exhaustive]
pub enum FieldEncoding {
    /// The value fills the field: it is written as an integer of `size` bytes
    /// in the target's byte order. This is what byte-oriented architectures
    /// need, and what every data directive uses.
    #[default]
    Whole,
    /// The architecture scatters the value through an instruction word.
    ///
    /// Fixed-width RISC encodings rarely have a contiguous displacement field:
    /// a RISC-V B-type immediate arrives in four pieces, and AArch64 branch
    /// offsets are pre-shifted. The function is handed the bytes already
    /// emitted, read as an integer in the target's byte order, plus the
    /// resolved value, and returns the patched word.
    ///
    /// A plain `fn` pointer keeps [`FixupKind`] `Copy` and lets each backend
    /// keep its bit-placement next to the instruction it belongs to.
    Scatter(fn(u64, i64) -> u64),
}

/// What a relocation computes from its target, when that is more than the
/// target's value.
///
/// In an object file the relocation type carries this and the linker does
/// the arithmetic. A flat binary has no linker, so the core does the same
/// arithmetic itself and has to be told which: a page-relative `adrp` and a
/// PowerPC `@ha` both hold an address, and neither holds it truncated to the
/// field. Relocatable output is unaffected, except where a value resolves at
/// assembly time anyway.
#[derive(Copy, Clone, Debug, Default)]
#[non_exhaustive]
pub enum LinkValue {
    /// `S + A`, or `S + A - P` for a PC-relative field: the value itself,
    /// fitted into the field by its range check and [`FieldEncoding`].
    #[default]
    Plain,
    /// The distance from the page holding the fixup to the target's page,
    /// with pages of `1 << n` bytes: `Page(S + A) - Page(P)`, which is
    /// AArch64's `adrp`. That needs both addresses, not just their
    /// difference, so in relocatable output the fixup resolves as
    /// [`LinkValue::Plain`] would.
    Page(u8),
    /// A target that has to be in the same `1 << bits`-byte region as the
    /// address just past the field, because the CPU takes its top bits from
    /// there: the delay slot of a MIPS `j` or `jal`, and the 2 KiB block an
    /// MCS-51 `AJMP` reaches. The field holds the value as usual; only the
    /// check needs the fixup's address, so, as for [`LinkValue::Page`],
    /// relocatable output does without it. On a PC-relative field the region
    /// is the target's, not the distance's: `Region { bits: 16, .. }` keeps
    /// an MCS-51 branch inside the 64 KiB address space.
    ///
    /// `numbers` says whether a target written as a plain number is checked
    /// too. GNU ld lets one through on MIPS, taking the region from the PC
    /// the same way the CPU does, so a MIPS `j 0x400` assembles wherever it
    /// stands; the Macro Assembler AS refuses an MCS-51 `AJMP 0` that is not
    /// on the page, which is also what makes the generic `JMP` fall back to
    /// `LJMP` for one.
    Region { bits: u8, numbers: bool },
    /// The value put through a function before its range check: PowerPC's
    /// `@ha` is `(x + 0x8000) >> 16`, whether `x` is a label or a constant.
    Split(fn(i64) -> i64),
    /// The low half of a PC-relative pair that names its high half by label,
    /// like RISC-V's `%pcrel_lo(1b)`. The label is on the instruction that
    /// carries the high half, and the value is that instruction's own
    /// PC-relative value: its target, measured from it rather than from here.
    PairedLow,
    /// Something only a linker creates, described for the diagnostic: "a GOT
    /// entry". A flat binary refuses it.
    LinkerOnly(&'static str),
    /// A branch whose instruction depends on its target: ARM's `bl` becomes
    /// `blx` to a Thumb function, and a branch into the other instruction
    /// set is left to the linker. The backend's
    /// [`Architecture::interwork`](crate::arch::Architecture::interwork)
    /// decides, from this class of its own and the target symbol, both for
    /// what the assembler resolves and, in a flat binary, for what a linker
    /// would have.
    Interwork(u8),
}

/// How a fixup's value is written into the output.
#[derive(Copy, Clone, Debug)]
#[non_exhaustive]
pub struct FixupKind {
    /// Field width in bytes: 1, 2, 4 or 8, or 0 for a relocation that marks
    /// an instruction rather than filling a field in it, as x86's
    /// `@TLSCALL` does. A zero-width fixup is always left to the linker.
    pub size: u8,
    /// The value is relative to the address of the fixup itself (plus
    /// `adjust`), rather than absolute.
    pub pcrel: bool,
    /// Range-check the result as signed rather than allowing either sign.
    pub signed: bool,
    /// Added to the fixup's own address before subtracting, for PC-relative
    /// fields that are not at the end of their instruction. For x86 a `rel32`
    /// four bytes before the end of the instruction uses `adjust = 4`.
    pub adjust: i8,
    /// Relocation to emit if the value cannot be resolved at assembly time.
    /// `0` means "no relocation available"; an unresolved fixup is then an
    /// error.
    pub reloc: u32,
    /// How many bits of the value the field can hold. `0` means the whole
    /// field, `size * 8`.
    ///
    /// A 4-byte AArch64 instruction word carrying a 26-bit branch offset has
    /// `size: 4` but `value_bits: 28` — 26 encoded bits plus the two that the
    /// alignment supplies.
    pub value_bits: u8,
    /// The value must be a multiple of this. `1` means no constraint.
    ///
    /// Branch displacements on fixed-width architectures are counted in
    /// instructions, so a misaligned target is an error rather than something
    /// to round.
    pub value_align: u8,
    pub encoding: FieldEncoding,
    /// Whether a PC-relative relocation's addend carries the `adjust` bias.
    ///
    /// Relocations disagree about where "here" is. x86-64's `PC32` is
    /// `S + A - P` with `P` the field itself, so a field four bytes short of
    /// the end of its instruction needs `A = -4`, and GNU as writes that. The
    /// Renesas-lineage targets — RL78, RX, V850 — define theirs from the
    /// instruction, and GNU as writes `A = 0`. Same-section resolution is
    /// unaffected either way; this only decides what the linker is handed.
    pub bias_reloc_addend: bool,
    /// Narrower bounds than the field's width allows, where a reference
    /// assembler picks a form by a range that is not a power of two.
    pub limits: Option<(i64, i64)>,
    /// Bounds on the addend a relocation may leave behind in the field, where
    /// the field is narrower than the value it stands for and so cannot hold
    /// every addend the expression might carry. ARM's `:lower16:` and
    /// `:upper16:` fields hold sixteen bits of a 32-bit address, and GNU as
    /// refuses a larger addend rather than truncate it. `None`, the default,
    /// lets the field take whatever the expression gives it, which is right
    /// wherever the field is as wide as the value.
    pub addend_limits: Option<(i64, i64)>,
    /// Which symbol the relocation names, when there is one.
    pub reloc_symbol: RelocSymbol,
    /// Never fill the field in here, even where the value is known: only the
    /// linker can. A RISC-V GOT reference means the address of a GOT slot,
    /// not of the symbol, however close the symbol is. In a flat binary such
    /// a field is an error.
    pub always_reloc: bool,
    /// Left to the linker in relocatable output even where the value is
    /// known, for an instruction a linker may rewrite: an ARM `bl` becomes
    /// `blx` if its target turns out to be Thumb code, which llvm-mc allows
    /// for even when the target is a local label. An absolute field is
    /// relocated even for a plain number, against no symbol, as GNU as
    /// relocates MSP430's `calla #addr`. Unlike [`FixupKind::always_reloc`],
    /// a flat binary still resolves it.
    pub object_reloc: bool,
    /// What the value is, beyond the target itself; see [`LinkValue`].
    pub link: LinkValue,
    /// Resolve the expression with every symbol in it taken to be zero, so
    /// that the field holds the addend alone, less the PC where the fixup is
    /// PC-relative.
    ///
    /// That is what a REL relocation would have left behind, and GNU as's
    /// ARM port writes it where it resolves a fixup it has no relocation for
    /// against a symbol whose value it will not use: a Thumb `adr` of a weak
    /// symbol keeps the addend and drops the symbol, which its own comment
    /// calls "probably wrong due to symbol preemption". Nothing else wants
    /// it.
    pub without_symbol: bool,
    /// For a PC-relative field, the PC it is measured from, `here + adjust`,
    /// is first rounded down to a multiple of this. `1` means no rounding.
    ///
    /// SuperH's `mov.l label,rn` and Thumb's literal loads clear the low two
    /// bits of the PC, so the base depends on which boundary the instruction
    /// itself sits on. Rounding a base to `n` leaves the value congruent to
    /// the target modulo `n`, so `value_align` then tests the target. Such a
    /// field cannot be relocated, since no relocation rounds its base, so it
    /// is meant for fields with no `reloc`.
    pub pc_align: u8,
    /// Relaxation sizes this field the way GNU as's RX port sizes a symbolic
    /// immediate, rather than by whether the value fits.
    ///
    /// `rx_relax_frag` only knows a value for a difference of two local
    /// labels in the fixup's own section, and takes the smallest field whose
    /// signed range holds it. It does not move labels ahead of the
    /// instruction by the growth so far, as it does for a branch target:
    /// it adds that growth to the whole difference, and only when the
    /// difference, read as an unsigned address, lies past the instruction.
    /// Anything it cannot evaluate gets the widest field. `range` still
    /// decides whether the value that is finally written is accepted.
    pub relax_difference: bool,
    /// A further test the value has to pass, beyond range and alignment, for
    /// fields that hold only some of the values in their range: an ARM `adr`
    /// is an `add` or `sub` of a modified immediate, so it reaches `pc + 0x400`
    /// but not `pc + 0x3fc`. Relaxation weighs it like the range.
    pub accepts: Option<fn(i64) -> bool>,
    /// Said after a value that does not fit, when the field's limit alone
    /// does not tell the reader what to do about it: a literal load that
    /// does not reach its pool needs the pool moved, not the load.
    pub range_hint: Option<&'static str>,
    /// Relocated even where the target is in the fixup's own section and the
    /// distance is therefore known, for a linker that may delete
    /// instructions between the two.
    ///
    /// The distance still sizes the instruction — a RISC-V branch takes its
    /// compressed form when the distance as written fits — but it is not what
    /// the field ends up holding, nor is it range-checked: the linker works
    /// the distance out again and writes it from the addend. Unlike
    /// [`FixupKind::object_reloc`], which hides the value from layout as well
    /// and so gives a relaxable instruction its widest form, and unlike a
    /// flat image, which has no linker and keeps the value with no
    /// relocation.
    pub relocated_when_relaxed: bool,
    /// A second relocation at the same offset as the first, written after it,
    /// against no symbol and with no addend. `0` means none.
    ///
    /// It marks the first rather than describing a field of its own, so it
    /// only appears where the first is written at all. RISC-V's
    /// `R_RISCV_RELAX` is the one: it tells a linker that the sequence the
    /// first relocation belongs to may be rewritten, and GNU as writes it
    /// beside every relocation a linker knows how to relax.
    pub marker_reloc: u32,
    /// What the relocation computes, for a writer that does not number
    /// relocations the way [`FixupKind::reloc`] does; see the crate's
    /// `reloc::RelocClass`.
    ///
    /// The default, `RelocClass::Plain`, covers every field whose value is
    /// the target itself, which is all an ELF object ever needs to be told
    /// beyond the number. A backend names a class only where a format may
    /// write the same width and `pcrel` differently: a branch, a GOT load, an
    /// AArch64 page or page offset. Not API.
    #[doc(hidden)]
    pub class: RelocClass,
}

/// The symbol a relocation is written against.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
#[non_exhaustive]
pub enum RelocSymbol {
    /// The symbol in the expression, except that a local label is replaced
    /// by its section plus an offset, which is what linkers expect and what
    /// keeps local labels out of the symbol table.
    #[default]
    Section,
    /// The symbol in the expression, even a local label, which is then
    /// written to the symbol table. For relocations whose linker looks the
    /// symbol itself up, rather than just its address: RISC-V's
    /// `R_RISCV_PCREL_LO12_I` names the `auipc` that carries the high half,
    /// and lld finds that `auipc` by the symbol's value alone, ignoring an
    /// addend.
    Symbol,
    /// A label at the start of the fixup's own fragment, with no addend,
    /// while the value (where it resolves) is still the expression's. This
    /// is `Symbol` for a label the source never wrote: the low half of a
    /// RISC-V `la` names the `auipc` the same expansion emitted first.
    ///
    /// Such a fixup is taken to be the second of a pair on one expression,
    /// so where the first cannot be resolved without a linker it reports
    /// nothing itself, rather than repeat the diagnostic.
    FragmentStart,
}

impl FixupKind {
    pub fn data(size: u8) -> FixupKind {
        FixupKind {
            size,
            pcrel: false,
            signed: false,
            adjust: 0,
            reloc: 0,
            value_bits: 0,
            value_align: 1,
            encoding: FieldEncoding::Whole,
            bias_reloc_addend: true,
            limits: None,
            addend_limits: None,
            reloc_symbol: RelocSymbol::Section,
            always_reloc: false,
            object_reloc: false,
            link: LinkValue::Plain,
            without_symbol: false,
            pc_align: 1,
            relax_difference: false,
            accepts: None,
            range_hint: None,
            relocated_when_relaxed: false,
            marker_reloc: 0,
            class: RelocClass::Plain,
        }
    }

    /// Says what the relocation computes, for the writers that need more than
    /// the ELF number; see [`RelocClass`]. Not API.
    #[doc(hidden)]
    pub fn with_class(mut self, class: RelocClass) -> FixupKind {
        self.class = class;
        self
    }

    /// Accepts only values `f` accepts, within the range; see
    /// [`FixupKind::accepts`].
    pub fn accepting(mut self, f: fn(i64) -> bool) -> FixupKind {
        self.accepts = Some(f);
        self
    }

    /// Adds advice to the out-of-range diagnostic; see
    /// [`FixupKind::range_hint`].
    pub fn with_range_hint(mut self, hint: &'static str) -> FixupKind {
        self.range_hint = Some(hint);
        self
    }

    pub fn pcrel(size: u8, adjust: i8) -> FixupKind {
        FixupKind {
            pcrel: true,
            signed: true,
            adjust,
            ..FixupKind::data(size)
        }
    }

    pub fn with_reloc(mut self, reloc: u32) -> FixupKind {
        self.reloc = reloc;
        self
    }

    pub fn signed(mut self) -> FixupKind {
        self.signed = true;
        self
    }

    /// Constrains the field to `bits` bits of value, requiring the value to be
    /// a multiple of `align`.
    pub fn with_field(mut self, bits: u8, align: u8) -> FixupKind {
        self.value_bits = bits;
        self.value_align = align.max(1);
        self
    }

    /// Leaves the `adjust` bias out of the relocation addend; see
    /// [`FixupKind::bias_reloc_addend`].
    pub fn unbiased_reloc(mut self) -> FixupKind {
        self.bias_reloc_addend = false;
        self
    }

    /// Accepts only values from `lo` to `hi`, within what the field holds.
    pub fn with_limits(mut self, lo: i64, hi: i64) -> FixupKind {
        self.limits = Some((lo, hi));
        self
    }

    /// Accepts only relocation addends from `lo` to `hi`; see
    /// [`FixupKind::addend_limits`].
    pub fn with_addend_limits(mut self, lo: i64, hi: i64) -> FixupKind {
        self.addend_limits = Some((lo, hi));
        self
    }

    /// Chooses the symbol the relocation names; see [`RelocSymbol`].
    pub fn with_reloc_symbol(mut self, sym: RelocSymbol) -> FixupKind {
        self.reloc_symbol = sym;
        self
    }

    /// Leaves the field to the linker even when its value is known; see
    /// [`FixupKind::always_reloc`].
    pub fn linker_only(mut self) -> FixupKind {
        self.always_reloc = true;
        self
    }

    /// Leaves the field to the linker in relocatable output; see
    /// [`FixupKind::object_reloc`].
    pub fn relocated_in_objects(mut self) -> FixupKind {
        self.object_reloc = true;
        self
    }

    /// Leaves the field to the linker although the distance is known; see
    /// [`FixupKind::relocated_when_relaxed`].
    pub fn relocated_when_relaxed(mut self) -> FixupKind {
        self.relocated_when_relaxed = true;
        self
    }

    /// Adds a second relocation at the same offset, marking the first; see
    /// [`FixupKind::marker_reloc`].
    pub fn marked_by(mut self, reloc: u32) -> FixupKind {
        self.marker_reloc = reloc;
        self
    }

    /// Sets what the value is computed as; see [`LinkValue`].
    pub fn link(mut self, link: LinkValue) -> FixupKind {
        self.link = link;
        self
    }

    /// Leaves the target out of the value; see
    /// [`FixupKind::without_symbol`].
    pub fn without_symbol(mut self) -> FixupKind {
        self.without_symbol = true;
        self
    }

    /// Rounds the PC this field is measured from down to a multiple of
    /// `align`; see [`FixupKind::pc_align`].
    pub fn with_pc_align(mut self, align: u8) -> FixupKind {
        self.pc_align = align.max(1);
        self
    }

    /// Sizes the field during relaxation as GNU as's RX port does; see
    /// [`FixupKind::relax_difference`].
    pub fn relaxed_as_difference(mut self) -> FixupKind {
        self.relax_difference = true;
        self
    }

    /// Sets the function that scatters the value through the instruction word.
    pub fn scatter(mut self, f: fn(u64, i64) -> u64) -> FixupKind {
        self.encoding = FieldEncoding::Scatter(f);
        self
    }

    /// How many bits of value the field holds.
    pub fn bits(&self) -> u32 {
        if self.value_bits > 0 {
            self.value_bits as u32
        } else {
            self.size as u32 * 8
        }
    }

    /// Inclusive range of values this field can hold.
    pub fn range(&self) -> (i128, i128) {
        let (lo, hi) = self.field_range();
        match self.limits {
            Some((l, h)) => (lo.max(l as i128), hi.min(h as i128)),
            None => (lo, hi),
        }
    }

    fn field_range(&self) -> (i128, i128) {
        let bits = self.bits();
        if bits >= 128 {
            return (i128::MIN, i128::MAX);
        }
        // A field of no width holds nothing. Such a fixup exists only to
        // place a relocation on an instruction — x86's `@TLSCALL` — and is
        // always left to the linker, so this is reached only if one were
        // ever asked to carry a value.
        if bits == 0 {
            return (0, 0);
        }
        if self.signed {
            (-(1i128 << (bits - 1)), (1i128 << (bits - 1)) - 1)
        } else {
            // Accept both the unsigned and the sign-extended reading, which is
            // what assemblers do for `.byte -1` as well as `.byte 255`.
            (-(1i128 << (bits - 1)), (1i128 << bits) - 1)
        }
    }

    pub fn fits(&self, v: i128) -> bool {
        if self.value_align > 1 && v % self.value_align as i128 != 0 {
            return false;
        }
        if let Some(f) = self.accepts
            && !i64::try_from(v).is_ok_and(f)
        {
            return false;
        }
        if self.bits() >= 64 && self.limits.is_none() {
            return true;
        }
        let (lo, hi) = self.range();
        v >= lo && v <= hi
    }

    /// Applies `value` to the `size` bytes at `dst`, in the target's byte
    /// order.
    pub fn write(&self, endian: crate::arch::Endian, dst: &mut [u8], value: i64) {
        let word = match self.encoding {
            FieldEncoding::Whole => value as u64,
            FieldEncoding::Scatter(f) => f(endian.read(dst), value),
        };
        endian.write(dst, word);
    }
}

#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Fixup {
    /// Byte offset within the fragment's bytes.
    pub offset: u32,
    pub expr: ExprRef,
    pub kind: FixupKind,
    pub span: Span,
}

/// One possible encoding of a fragment.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct Variant {
    pub bytes: Vec<u8>,
    pub fixups: Vec<Fixup>,
}

impl Variant {
    pub fn new(bytes: Vec<u8>) -> Variant {
        Variant {
            bytes,
            fixups: Vec::new(),
        }
    }
}

#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum FragKind {
    /// Literal bytes. Instructions that can be encoded several ways list all
    /// candidates smallest-first; layout raises `chosen` until every fixup
    /// fits, and never lowers it, so the loop terminates.
    Bytes {
        variants: Vec<Variant>,
        chosen: usize,
    },
    /// Pad to a multiple of `align`, at most `max_skip` bytes.
    Align {
        align: u64,
        fill: Vec<u8>,
        max_skip: Option<u64>,
        /* filled by layout */ pad: u64,
        /// For no-op padding, the backend state the padding was written in,
        /// where that is not simply the state at the end of the source: an
        /// alignment in ARM code keeps ARM no-ops however the file goes on,
        /// as one in x86 `.code32` keeps 32-bit ones.
        nop_state: Option<crate::arch::ArchState>,
    },
    /// Advance the location counter to an absolute offset within the section.
    Org {
        target: ExprRef,
        fill: u8,
        size: u64,
    },
    /// `.space` / `.skip`: `size` bytes of `fill`.
    Space {
        size: ExprRef,
        fill: ExprRef,
        resolved: u64,
    },
    /// A variable-length integer whose width depends on its value.
    Leb128 {
        value: ExprRef,
        signed: bool,
        encoded: Vec<u8>,
    },
}

#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Fragment {
    pub kind: FragKind,
    pub span: Span,
    /// Offset from the start of the section, assigned by layout.
    pub offset: u64,
    /// The reference assembler gives this instruction a fragment that its
    /// relaxation revisits, even though it has only one encoding here. A
    /// difference of labels on either side of it is then not a constant
    /// while the file is read; see [`crate::arch::AsmCtx::fixed_distance`].
    pub relaxable: bool,
}

impl Fragment {
    pub fn new(kind: FragKind, span: Span) -> Fragment {
        Fragment {
            kind,
            span,
            offset: 0,
            relaxable: false,
        }
    }

    /// Current size in bytes, based on the last layout decision.
    pub fn size(&self) -> u64 {
        match &self.kind {
            FragKind::Bytes { variants, chosen } => {
                variants.get(*chosen).map_or(0, |v| v.bytes.len() as u64)
            }
            FragKind::Align { pad, .. } => *pad,
            FragKind::Org { size, .. } => *size,
            FragKind::Space { resolved, .. } => *resolved,
            FragKind::Leb128 { encoded, .. } => encoded.len() as u64,
        }
    }

    pub fn is_plain_data(&self) -> bool {
        matches!(&self.kind, FragKind::Bytes { variants, .. } if variants.len() == 1)
    }
}

#[non_exhaustive]
pub struct Section {
    pub id: SectionId,
    pub name: Name,
    pub kind: SectionKind,
    pub flags: SectionFlags,
    /// Required alignment of the section itself.
    pub align: u64,
    /// Entry size for mergeable sections; 0 otherwise.
    pub entsize: u64,
    pub frags: Vec<Fragment>,
    /// Total size after the last layout pass.
    pub size: u64,
    /// Base address, for absolute output formats.
    pub addr: u64,
    /// The address the source put the section at, when it said: the 8-bit
    /// dialect's `ORG` before anything was emitted. Absolute output lays the
    /// section out there rather than after the one before it.
    pub origin: Option<u64>,
    /// The section symbol, created lazily when a relocation needs it.
    pub sym: Option<SymbolId>,
    /// Index of the trailing fragment that new data may be appended to, if
    /// any. Cleared by anything that must not be merged across, such as a
    /// label definition.
    open_data: Option<usize>,
    /// The `.subsection`-style saved location counter is not modelled yet;
    /// this records the section's declared group name if it has one.
    pub group: Option<Name>,
    /// Where the backend that emitted this section's fragments changes, as
    /// (first fragment index, backend slot) pairs in order; see
    /// `Assembler::switch_arch`. Empty while every fragment is the first
    /// backend's, which is the case in any file without `.arch`.
    pub arch_marks: Vec<(u32, u32)>,
    /// The mapping symbol in force where the section ends so far, or `None`
    /// before anything has been marked; see the crate's `mapping` module.
    pub map_state: Option<&'static str>,
    /// The mapping symbols recorded so far, placed once the layout is known.
    pub map_events: Vec<crate::mapping::MapEvent>,
    /// The backend state the last instruction was assembled in, while no
    /// other kind of fragment has followed it, for a backend whose
    /// [`Architecture::pads_as_last_instruction`]: an alignment here pads
    /// with that state's no-ops, as GNU as's ARM port pads with the
    /// instruction set of the last instruction in the fragment, whatever
    /// `.arm` or `.thumb` has said since.
    ///
    /// [`Architecture::pads_as_last_instruction`]: crate::arch::Architecture::pads_as_last_instruction
    pub nop_state: Option<crate::arch::ArchState>,
    /// Whether any instruction has been assembled into the section, which a
    /// Mach-O section header records (`S_ATTR_SOME_INSTRUCTIONS`). Not API.
    #[doc(hidden)]
    pub has_instructions: bool,
}

impl Section {
    /// Not API.
    #[doc(hidden)]
    pub fn new(id: SectionId, name: Name, kind: SectionKind, flags: SectionFlags) -> Section {
        Section {
            id,
            name,
            kind,
            flags,
            align: 1,
            entsize: 0,
            frags: Vec::new(),
            size: 0,
            addr: 0,
            origin: None,
            sym: None,
            open_data: None,
            group: None,
            arch_marks: Vec::new(),
            map_state: None,
            map_events: Vec::new(),
            nop_state: None,
            has_instructions: false,
        }
    }

    /// Records that fragments from here on are emitted by backend `slot`.
    /// Not API.
    #[doc(hidden)]
    pub fn mark_arch(&mut self, slot: u32) {
        // Bytes emitted after the switch must not merge into a fragment
        // emitted before it, or one fragment would have two byte orders.
        self.seal();
        self.nop_state = None;
        let at = self.next_frag_index();
        let current = self.arch_marks.last().map_or(0, |&(_, s)| s);
        match self.arch_marks.last_mut() {
            _ if current == slot => {}
            // Nothing was emitted under the previous mark: replace it, so
            // switching back and forth adds nothing.
            Some(last) if last.0 == at => last.1 = slot,
            _ => self.arch_marks.push((at, slot)),
        }
    }

    /// The backend slot that emitted fragment `fi`.
    /// Not API.
    #[doc(hidden)]
    pub fn arch_slot(&self, fi: usize) -> u32 {
        match self
            .arch_marks
            .partition_point(|&(at, _)| at as usize <= fi)
        {
            0 => 0,
            n => self.arch_marks[n - 1].1,
        }
    }

    /// Index the next fragment will get. Labels record this to name a position.
    pub fn next_frag_index(&self) -> u32 {
        self.frags.len() as u32
    }

    /// Prevents further merging into the current data fragment, so that the
    /// next fragment index refers to a real position.
    /// Not API.
    #[doc(hidden)]
    pub fn seal(&mut self) {
        self.open_data = None;
    }

    /// Not API.
    #[doc(hidden)]
    pub fn push(&mut self, frag: Fragment) -> u32 {
        self.open_data = None;
        self.nop_state = None;
        let idx = self.frags.len() as u32;
        self.frags.push(frag);
        idx
    }

    /// Appends raw bytes, merging into the previous data fragment when that is
    /// safe. Merging keeps fragment counts (and therefore layout cost) low for
    /// data-heavy files.
    /// Not API.
    #[doc(hidden)]
    pub fn emit_bytes(&mut self, bytes: &[u8], span: Span) {
        if let Some(i) = self.open_data
            && let FragKind::Bytes { variants, .. } = &mut self.frags[i].kind
        {
            variants[0].bytes.extend_from_slice(bytes);
            self.frags[i].span = self.frags[i].span.to(span);
            return;
        }
        let idx = self.frags.len();
        self.frags.push(Fragment::new(
            FragKind::Bytes {
                variants: vec![Variant::new(bytes.to_vec())],
                chosen: 0,
            },
            span,
        ));
        self.open_data = Some(idx);
    }

    /// Appends `size` bytes to be filled in later from `expr`.
    /// Not API.
    #[doc(hidden)]
    pub fn emit_fixup(&mut self, size: u8, expr: ExprRef, kind: FixupKind, span: Span) {
        let placeholder = vec![0u8; size as usize];
        let (idx, base) = match self.open_data {
            Some(i) => {
                let FragKind::Bytes { variants, .. } = &self.frags[i].kind else {
                    unreachable!("open_data always points at a Bytes fragment")
                };
                (i, variants[0].bytes.len() as u32)
            }
            None => {
                let i = self.frags.len();
                self.frags.push(Fragment::new(
                    FragKind::Bytes {
                        variants: vec![Variant::default()],
                        chosen: 0,
                    },
                    span,
                ));
                self.open_data = Some(i);
                (i, 0)
            }
        };
        let FragKind::Bytes { variants, .. } = &mut self.frags[idx].kind else {
            unreachable!()
        };
        variants[0].bytes.extend_from_slice(&placeholder);
        variants[0].fixups.push(Fixup {
            offset: base,
            expr,
            kind,
            span,
        });
        self.frags[idx].span = self.frags[idx].span.to(span);
    }

    /// Appends a pre-encoded instruction with one or more size variants.
    /// Not API.
    #[doc(hidden)]
    pub fn emit_variants(&mut self, variants: Vec<Variant>, span: Span) -> u32 {
        debug_assert!(
            !variants.is_empty(),
            "an instruction needs at least one encoding"
        );
        self.push(Fragment::new(
            FragKind::Bytes {
                variants,
                chosen: 0,
            },
            span,
        ))
    }

    pub fn is_empty(&self) -> bool {
        self.frags.iter().all(|f| f.size() == 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arch::Endian;

    #[test]
    fn whole_fields_are_written_in_target_byte_order() {
        let k = FixupKind::data(4);
        let mut buf = [0u8; 4];
        k.write(Endian::Little, &mut buf, 0x1122_3344);
        assert_eq!(buf, [0x44, 0x33, 0x22, 0x11]);
        k.write(Endian::Big, &mut buf, 0x1122_3344);
        assert_eq!(buf, [0x11, 0x22, 0x33, 0x44]);
    }

    #[test]
    fn scattered_fields_merge_into_the_instruction_word() {
        // The shape every fixed-width RISC branch needs: keep the opcode bits
        // already emitted, drop the value's low zero bits, and mask it into
        // the field. This is AArch64's `b` — a 26-bit field of word offsets.
        fn aarch64_b(word: u64, value: i64) -> u64 {
            (word & !0x03ff_ffff) | (((value >> 2) as u64) & 0x03ff_ffff)
        }
        let k = FixupKind::pcrel(4, 0).with_field(28, 4).scatter(aarch64_b);

        // `b .+8` starting from the opcode word 0x1400_0000.
        let mut buf = 0x1400_0000u32.to_le_bytes();
        k.write(Endian::Little, &mut buf, 8);
        assert_eq!(u32::from_le_bytes(buf), 0x1400_0002);

        // A negative offset must not corrupt the opcode bits above the field.
        let mut buf = 0x1400_0000u32.to_le_bytes();
        k.write(Endian::Little, &mut buf, -8);
        assert_eq!(u32::from_le_bytes(buf), 0x17ff_fffe);
    }

    #[test]
    fn field_width_and_alignment_are_checked_separately_from_size() {
        // A 4-byte field that only carries 28 bits of value.
        let k = FixupKind::pcrel(4, 0).with_field(28, 4);
        assert!(k.fits(128 * 1024 * 1024 - 4));
        assert!(!k.fits(128 * 1024 * 1024), "out of range must not fit");
        assert!(k.fits(-(128 * 1024 * 1024)));
        // Misaligned targets are rejected rather than rounded.
        assert!(!k.fits(2));
        assert!(k.fits(4));
    }

    #[test]
    fn the_relocation_bias_is_on_by_default_and_can_be_dropped() {
        // Every existing backend relies on the x86-style bias, so it must stay
        // the default; the Renesas-lineage targets opt out.
        assert!(FixupKind::pcrel(4, 4).bias_reloc_addend);
        assert!(!FixupKind::pcrel(1, 1).unbiased_reloc().bias_reloc_addend);
        // Dropping the bias does not touch how the field resolves locally.
        let k = FixupKind::pcrel(1, 1).unbiased_reloc();
        assert_eq!(k.adjust, 1);
    }

    #[test]
    fn byte_fields_accept_both_signed_and_unsigned_spellings() {
        let k = FixupKind::data(1);
        assert!(k.fits(255));
        assert!(k.fits(-1));
        assert!(!k.fits(256));
        assert!(!k.fits(-129));
    }
}
