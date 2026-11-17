//! D3 (port27/WAVE-BACKEND.md; docs/v2/plan/design/backend-next.md, D100 points 10 and 11):
//! LLVM's object for one function read back into the one new code record, [`Emitted`].
//!
//! [`read_object`] reads a 64-bit little-endian relocatable object by hand: Mach-O for arm64
//! (`MH_OBJECT`, `CPU_TYPE_ARM64`) or ELF for aarch64 (`ET_REL`, `EM_AARCH64`). It reads the
//! section, symbol and relocation tables, nothing else, and gives:
//!
//! - `code`: the one executable section's bytes (`__TEXT,__text`; ELF's one non-empty
//!   `SHF_EXECINSTR` section). The function named `symbol` must be its one defined global and
//!   start at its first byte; local helpers in the same section stay in the code and are
//!   reached as the record's own code ([`OWN_CODE`] plus their offset).
//! - `relro`: the read-only data sections (`__TEXT,__const`, `__cstring`, `__literal4`,
//!   `__literal8`, `__literal16`, `__DATA,__const`, `__DATA_CONST,__const`; ELF's `.rodata*` and
//!   `.data.rel.ro*`) back to back, each on its own alignment, which must be at most 8: the
//!   record's relro part is placed 8-aligned (blaze-tier place.rs), so a 16-aligned literal
//!   could not keep its alignment and is refused by name ([`ReadError::SectionAlign`]).
//! - `relocs`: one [`Reloc`] per relocation (D100 point 11), by this table and no other:
//!
//! | Mach-O (`ARM64_RELOC_*`) | ELF (`R_AARCH64_*`) | [`RelocKind`] |
//! |---|---|---|
//! | `PAGE21` | `ADR_PREL_PG_HI21` | `Page21` |
//! | `PAGEOFF12` (scale from the instruction) | `ADD_ABS_LO12_NC`, `LDST{8,16,32,64,128}_ABS_LO12_NC` | `PageOff12 { scale }` |
//! | `BRANCH26` | `CALL26`, `JUMP26` | `Branch26` |
//! | `GOT_LOAD_PAGE21` | `ADR_GOT_PAGE` | `GotPage21` |
//! | `GOT_LOAD_PAGEOFF12` | `LD64_GOT_LO12_NC` | `GotPageOff12` |
//! | `UNSIGNED`, 8 bytes, in a relro section | `ABS64` in a relro section | `Abs64` |
//!
//! `ARM64_RELOC_ADDEND` gives the next relocation at its offset its addend; Mach-O's other
//! instruction relocations must leave their instruction's field zero (an addend there would be
//! one this reader cannot tell from an encoding: [`ReadError::ImplicitAddend`]). Every other
//! relocation (thread-local, subtractor pairs, `ADR_PREL_LO21`, `PREL32` jump tables, ..) is
//! refused by name ([`ReadError::Relocation`]). A target is the record's own code or relro (a
//! symbol defined in those sections), or a name the caller's `names` resolves; an outside C
//! name's text is kept for `dlsym` ([`Emitted::names`]); a name `names` does not know is refused
//! ([`ReadError::UnknownName`]).
//! - `unwind_frame` and `unwind`: the frame from the compact unwind entry (Mach-O) or the one
//!   FDE's call-frame program (ELF's `.eh_frame`), checked against blaze-code unwind.rs's ONE
//!   frame form (CFA `x29 + 16`, `x29` at CFA - 16, `x30` at CFA - 8, the `k`th saved pair's
//!   lower register at CFA - 32 - 16k and its higher at CFA - 24 - 16k); the rows from the LSDA
//!   by crate::read_lsda (the one LSDA reader). A frame the form cannot say is REFUSED BY NAME
//!   ([`ReadError::FrameLayout`]), never a record with a hole: the first panic through such a
//!   frame would restore the caller's registers wrong. NOTE what this refuses today: LLVM's
//!   arm64 prologue saves a callee-saved pair HIGH register first (`stp x20, x19, ..`, so
//!   libunwind's compact frame mode reads x19 at `fp - 8`, x20 at `fp - 16`; on Linux the frame
//!   record sits below the saved registers, CFA `x29 + 16 + 16n`), and blaze's form 1 says the
//!   lower register first: every optimized function that keeps a value in x19..x28 or d8..d15
//!   across a call is refused until blaze-code unwind.rs has a form for LLVM's layout.
//!   A function with no call is never between a throw and its catch: with no frame described
//!   (a frameless compact entry, or no entry) its `unwind_frame` is `None` (blaze-code unwind.rs:
//!   "the record cannot be unwound through", which no unwind asks of it); a frameless entry
//!   whose code calls is refused ([`ReadError::FramelessCall`]).
//!
//! What is NOT read: debug information (refused as an unknown section if present), so the
//! position map and inline frames stay empty; the read list (`reads`) is the caller's, which
//! knows the inlined set (D100 point 7); `frame` (the size below the frame record) is not in
//! either unwind form and stays 0, as from_v1 leaves it; landing pads are carried as unwind rows
//! only, as read_lsda.rs's doc says. Ignored on purpose and named here: Mach-O's
//! `LC_LINKER_OPTIMIZATION_HINT` (hints a linker MAY act on to relax adrp pairs; nothing here
//! relaxes, so dropping them changes no instruction), `LC_DATA_IN_CODE` (marks data inside the
//! text for disassemblers; the bytes are kept as they are), `LC_DYSYMTAB`, `LC_BUILD_VERSION`
//! and `LC_VERSION_MIN_MACOSX` (partition the symbol table, name the OS version); ELF's empty
//! `.note.GNU-stack` (asks a linker for a non-executable stack) and any section of no bytes and
//! no relocations (it holds nothing to lose).
//!
//! Every multi-byte field is little-endian (both hosts in scope are arm64). Nothing here
//! panics on malformed bytes: every read is bounds-checked and refused as
//! [`ReadError::Truncated`]. Pure: bytes in, a record out; scratch lists in the caller's
//! allocator.

use core::fmt;

use blaze_code::record::{OWN_CODE, OWN_RELRO, Space, Symbol, Target};
use blaze_code::unwind::Frame;
use blaze_own::{AVec, Alloc, AllocFailure, Vec};
use blaze_tier::{Emitted, Reloc, RelocKind};

use crate::read_lsda::{LsdaError, rows_of_lsda};

/// The personality routine every blaze function names (the pinned library's
/// `rust_eh_personality`); an LSDA read under any other would be read by the wrong routine.
pub const PERSONALITY: &str = "rust_eh_personality";
/// ELF's indirection word for it (LLVM's `DW.ref.` prefix, a hidden weak data symbol).
const ELF_PERSONALITY_REF: &[u8] = b"DW.ref.rust_eh_personality";
/// The section holding that word.
const ELF_PERSONALITY_SECTION: &[u8] = b".data.DW.ref.rust_eh_personality";

/// The bytes of a name kept inline in an error.
const NAME_BYTES: usize = 48;

/// A name from the object (a symbol or a section), kept inline so an error owns it: its first
/// 48 bytes, `cut` when longer.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ObjName {
    bytes: [u8; NAME_BYTES],
    len: u8,
    cut: bool,
}

impl ObjName {
    /// The parts back to back (a Mach-O section is named `segment,section`).
    pub fn of(parts: &[&[u8]]) -> ObjName {
        let mut name = ObjName { bytes: [0; NAME_BYTES], len: 0, cut: false };
        for part in parts {
            for &byte in part.iter() {
                if usize::from(name.len) == NAME_BYTES {
                    name.cut = true;
                    return name;
                }
                name.bytes[usize::from(name.len)] = byte;
                name.len += 1;
            }
        }
        name
    }

    /// The kept bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }
}

impl fmt::Display for ObjName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("`")?;
        for &byte in self.as_bytes() {
            if byte.is_ascii_graphic() || byte == b' ' {
                write!(f, "{}", char::from(byte))?;
            } else {
                write!(f, "\\x{byte:02x}")?;
            }
        }
        if self.cut {
            f.write_str("..")?;
        }
        f.write_str("`")
    }
}

impl fmt::Debug for ObjName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// Why an object was refused: the thing (a section, a symbol, an offset in the code or a
/// section) and the rule.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReadError {
    /// The bytes are not the object form this reader reads (`what` says which part differs).
    Format { what: &'static str },
    /// `what` at byte `at` runs past the bytes it is in.
    Truncated { what: &'static str, at: u64 },
    /// `what` does not fit the record's `u32` offsets.
    TooLarge { what: &'static str },
    /// A Mach-O load command this reader does not know.
    LoadCommand { cmd: u32 },
    /// A section of bytes or relocations this reader does not know.
    Section { name: ObjName },
    /// A section of a known name has a type that is not plain bytes (zero-fill, ..).
    SectionType { name: ObjName, kind: u32 },
    /// A data section aligned past the relro part's 8 bytes.
    SectionAlign { name: ObjName, align: u64 },
    /// A second executable section with bytes.
    TwoTexts { name: ObjName },
    /// The function is not defined in the executable section.
    NoFunction { name: ObjName },
    /// The function does not start at its section's first byte (it is at `at`).
    EntryNotFirst { name: ObjName, at: u64 },
    /// A global other than the function is defined: a record defines one function.
    ExtraDefinition { name: ObjName },
    /// A symbol of a kind this reader does not resolve (`kind` says which).
    SymbolKind { name: ObjName, kind: &'static str },
    /// A relocation names symbol `index`, past the symbol table.
    NoSuchSymbol { index: u64 },
    /// A symbol or relocation names section `index`, which the object does not have.
    NoSuchSection { index: u64 },
    /// A relocation kind with no row in the record (`kind` is its name, `number` its type).
    Relocation { section: ObjName, at: u64, kind: &'static str, number: u32 },
    /// A relocation of a known kind in a shape this reader does not read (`what`).
    RelocationShape { section: ObjName, at: u64, what: &'static str },
    /// A Mach-O instruction relocation at `at` whose instruction field is not zero.
    ImplicitAddend { at: u64, inst: u32 },
    /// A page-offset relocation at `at` on an instruction that is neither an `add` nor a load or
    /// store with an unsigned offset.
    PageOffInstruction { at: u64, inst: u32 },
    /// `names` does not know this name.
    UnknownName { name: ObjName },
    /// `names` answered an outside target whose symbol is not the name's (`Symbol::of_name`).
    OutsideSymbol { name: ObjName },
    /// A name is not UTF-8.
    NameText { name: ObjName },
    /// A relocation at `at` targets a section that is neither code nor relro.
    TargetSection { name: ObjName, at: u64 },
    /// A compact unwind encoding with a mode or bits this reader does not read (DWARF mode
    /// among them).
    UnwindEncoding { encoding: u32 },
    /// More than one unwind entry: the text holds more than one described function.
    UnwindEntries { count: u64 },
    /// The unwind entry covers `[start, start + len)`, not the function's `code` bytes.
    UnwindCover { start: u64, len: u64, code: u64 },
    /// A frameless function calls at `at`: its return address has no described home.
    FramelessCall { at: u64 },
    /// The frame at the call at `at` (0 for a compact entry) is not blaze's frame form: `what`
    /// broke, at DWARF register `reg`, whose rule is `offset` from the CFA (0: none).
    FrameLayout { at: u64, what: &'static str, reg: u16, offset: i64 },
    /// The call at `at` has another frame than an earlier call: the record holds one frame.
    FrameChanges { at: u64 },
    /// The call-frame entry at `at` holds `what` (opcode `op` where it is an instruction).
    Cfi { at: u64, what: &'static str, op: u8 },
    /// The personality routine named is not blaze's ([`PERSONALITY`]).
    Personality { name: ObjName },
    /// The function has an LSDA and names no personality routine.
    LsdaWithoutPersonality,
    /// The LSDA section has relocations: a type-table entry would read as its unrelocated zero.
    LsdaRelocation { section: ObjName },
    /// The LSDA pointer is not inside the LSDA section.
    LsdaAddress { at: u64 },
    /// The LSDA has rows and the function has no frame described.
    RowsWithoutFrame { rows: u64 },
    /// The LSDA broke a rule of read_lsda.rs.
    Lsda(LsdaError),
    /// No memory in the caller's allocator.
    Alloc(AllocFailure),
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rule = "(blaze-llvm read.rs; D100 point 11)";
        match *self {
            ReadError::Format { what } => write!(f, "the object is {what}; only arm64 Mach-O and aarch64 ELF relocatable objects are read {rule}"),
            ReadError::Truncated { what, at } => write!(f, "the object's {what} at byte {at} runs past its bytes {rule}"),
            ReadError::TooLarge { what } => write!(f, "the object's {what} does not fit a code record's 32-bit offsets {rule}"),
            ReadError::LoadCommand { cmd } => write!(f, "the object has Mach-O load command {cmd:#x}, which this reader does not read {rule}"),
            ReadError::Section { name } => write!(f, "the object has section {name}, which a code record cannot hold {rule}"),
            ReadError::SectionType { name, kind } => write!(f, "section {name} has type {kind:#x}, not plain bytes {rule}"),
            ReadError::SectionAlign { name, align } => {
                write!(f, "section {name} is aligned to {align} bytes; a record's relro part is placed 8-aligned (blaze-tier place.rs) {rule}")
            }
            ReadError::TwoTexts { name } => write!(f, "section {name} is a second executable section; a record holds one function's code {rule}"),
            ReadError::NoFunction { name } => write!(f, "the function {name} is not defined in the object's executable section {rule}"),
            ReadError::EntryNotFirst { name, at } => write!(f, "the function {name} starts at byte {at} of its section, not its first {rule}"),
            ReadError::ExtraDefinition { name } => write!(f, "the object also defines global {name}; a record defines one function {rule}"),
            ReadError::SymbolKind { name, kind } => write!(f, "symbol {name} is {kind}, which this reader does not resolve {rule}"),
            ReadError::NoSuchSymbol { index } => write!(f, "a relocation names symbol {index}, past the symbol table {rule}"),
            ReadError::NoSuchSection { index } => write!(f, "the object names section {index}, which it does not have {rule}"),
            ReadError::Relocation { section, at, kind, number } => {
                write!(f, "the relocation at byte {at} of {section} is {kind} (type {number}), which no code record row expresses {rule}")
            }
            ReadError::RelocationShape { section, at, what } => write!(f, "the relocation at byte {at} of {section} is {what} {rule}"),
            ReadError::ImplicitAddend { at, inst } => {
                write!(f, "the instruction {inst:#010x} at code byte {at} carries a nonzero field under its relocation; Mach-O addends come by ARM64_RELOC_ADDEND {rule}")
            }
            ReadError::PageOffInstruction { at, inst } => {
                write!(f, "the page-offset relocation at code byte {at} is on {inst:#010x}, neither an add nor a load or store with an unsigned offset {rule}")
            }
            ReadError::UnknownName { name } => write!(f, "the object names {name}, which the caller's names do not resolve {rule}"),
            ReadError::OutsideSymbol { name } => write!(f, "the caller resolved {name} to an outside name whose symbol is not the name's own {rule}"),
            ReadError::NameText { name } => write!(f, "the name {name} is not UTF-8 {rule}"),
            ReadError::TargetSection { name, at } => {
                write!(f, "the relocation at byte {at} targets section {name}, which is neither the record's code nor its relro {rule}")
            }
            ReadError::UnwindEncoding { encoding } => {
                write!(f, "the compact unwind encoding {encoding:#010x} is not a frame or frameless form this reader reads (DWARF mode is refused) {rule}")
            }
            ReadError::UnwindEntries { count } => write!(f, "the object has {count} unwind entries; a record describes one function {rule}"),
            ReadError::UnwindCover { start, len, code } => {
                write!(f, "the unwind entry covers {len} bytes from {start}, not the function's {code} bytes from 0 {rule}")
            }
            ReadError::FramelessCall { at } => write!(f, "the frameless function calls at code byte {at}; its return address has no described home {rule}"),
            ReadError::FrameLayout { at, what, reg, offset } => write!(
                f,
                "the frame at code byte {at} does not fit blaze's frame form (blaze-code unwind.rs): {what}; DWARF register {reg} is at CFA {offset:+} {rule}"
            ),
            ReadError::FrameChanges { at } => write!(f, "the call at code byte {at} has another frame than an earlier call; a record holds one frame {rule}"),
            ReadError::Cfi { at, what, op } => write!(f, "the call-frame entry at byte {at} of the unwind section holds {what} (opcode {op:#04x}) {rule}"),
            ReadError::Personality { name } => write!(f, "the function names personality {name}, not `{PERSONALITY}` {rule}"),
            ReadError::LsdaWithoutPersonality => write!(f, "the function has an LSDA and names no personality routine {rule}"),
            ReadError::LsdaRelocation { section } => {
                write!(f, "the LSDA section {section} has relocations; a type-table entry would read as its unrelocated zero {rule}")
            }
            ReadError::LsdaAddress { at } => write!(f, "the LSDA pointer {at:#x} is not inside the LSDA section {rule}"),
            ReadError::RowsWithoutFrame { rows } => write!(f, "the LSDA has {rows} rows and the function has no frame described {rule}"),
            ReadError::Lsda(error) => write!(f, "{error}"),
            ReadError::Alloc(why) => write!(f, "no memory for the record: {why}"),
        }
    }
}

impl core::error::Error for ReadError {}

/// One function's object, LLVM's `compile` output, as the new code record, in `alloc`:
/// `symbol` is the function's name as the IR spelled it (Mach-O's leading `_` is the reader's),
/// `names` resolves every other name the code reaches. The module's documentation says what is
/// read and what is refused.
pub fn read_object<'a>(object: &[u8], symbol: &str, names: &dyn Fn(&str) -> Option<Target>, alloc: Alloc<'a>) -> Result<Emitted<'a>, ReadError> {
    let out = Out { e: Emitted::new_in(alloc), names };
    match object.get(0..4) {
        Some([0xcf, 0xfa, 0xed, 0xfe]) => read_macho(object, symbol, out, alloc),
        Some([0x7f, b'E', b'L', b'F']) => read_elf(object, symbol, out, alloc),
        _ => Err(ReadError::Format { what: "neither a 64-bit little-endian Mach-O nor an ELF file" }),
    }
}

// ---------------------------------------------------------------------------------------------
// The record being built, and the bounds-checked readers.

/// Where a relocation points before it is a target index.
#[derive(Clone, Copy)]
enum Dest<'o> {
    /// The record's own code byte.
    Code(u64),
    /// The record's own relro byte.
    Relro(u64),
    /// A name the caller resolves.
    Name(&'o [u8]),
}

struct Out<'a, 'n> {
    e: Emitted<'a>,
    names: &'n dyn Fn(&str) -> Option<Target>,
}

impl<'a> Out<'a, '_> {
    /// `dest` plus `addend` as a target index and addend.
    fn target(&mut self, dest: Dest<'_>, addend: i64) -> Result<(u32, i64), ReadError> {
        match dest {
            Dest::Code(off) => Ok((OWN_CODE, plus(off, addend)?)),
            Dest::Relro(off) => Ok((OWN_RELRO, plus(off, addend)?)),
            Dest::Name(bytes) => {
                let name = ObjName::of(&[bytes]);
                let text = core::str::from_utf8(bytes).map_err(|_| ReadError::NameText { name })?;
                let target = (self.names)(text).ok_or(ReadError::UnknownName { name })?;
                if target.space == Space::Outside {
                    if target.symbol != Symbol::of_name(text) {
                        return Err(ReadError::OutsideSymbol { name });
                    }
                    let start = u32::try_from(self.e.text.len()).map_err(|_| ReadError::TooLarge { what: "names' text" })?;
                    let len = u32::try_from(bytes.len()).map_err(|_| ReadError::TooLarge { what: "name" })?;
                    self.e.text.try_extend_from_slice(bytes).map_err(ReadError::Alloc)?;
                    self.e.names.try_push((target.symbol, start, len)).map_err(|refused| ReadError::Alloc(refused.why))?;
                }
                let index = u32::try_from(self.e.targets.len()).map_err(|_| ReadError::TooLarge { what: "target list" })?;
                self.e.targets.try_push(target).map_err(|refused| ReadError::Alloc(refused.why))?;
                Ok((index, addend))
            }
        }
    }

    fn reloc(&mut self, at: u64, in_relro: bool, kind: RelocKind, dest: Dest<'_>, addend: i64) -> Result<(), ReadError> {
        let (target, addend) = self.target(dest, addend)?;
        let at = u32::try_from(at).map_err(|_| ReadError::TooLarge { what: "relocation offset" })?;
        self.e.relocs.try_push(Reloc { at, in_relro, kind, target, addend }).map_err(|refused| ReadError::Alloc(refused.why))
    }

    /// Appends a data section to the relro part on its alignment; its first byte's offset.
    fn relro(&mut self, bytes: &[u8], align: u64, name: ObjName) -> Result<u64, ReadError> {
        if align > 8 {
            return Err(ReadError::SectionAlign { name, align });
        }
        let align = align.max(1);
        let len = self.e.relro.len() as u64;
        let pad = (align - len % align) % align;
        self.e.relro.try_extend_from_slice(&[0u8; 8][..pad as usize]).map_err(ReadError::Alloc)?;
        let at = self.e.relro.len() as u64;
        self.e.relro.try_extend_from_slice(bytes).map_err(ReadError::Alloc)?;
        Ok(at)
    }

    /// The relocations in their canonical order (the seal refuses any other), the frame and the
    /// LSDA's rows.
    fn finish(mut self, frame: Option<Frame>, lsda: Option<&[u8]>, alloc: Alloc<'a>) -> Result<Emitted<'a>, ReadError> {
        self.e.relocs.sort_unstable_by_key(|r| (r.at, r.in_relro));
        self.e.unwind_frame = frame;
        if let Some(lsda) = lsda {
            let len = u32::try_from(self.e.code.len()).map_err(|_| ReadError::TooLarge { what: "code" })?;
            let rows = rows_of_lsda(lsda, 0, len, alloc).map_err(ReadError::Lsda)?;
            if !rows.is_empty() && frame.is_none() {
                return Err(ReadError::RowsWithoutFrame { rows: rows.len() as u64 });
            }
            self.e.unwind = rows;
        }
        Ok(self.e)
    }
}

fn plus(off: u64, addend: i64) -> Result<i64, ReadError> {
    i64::try_from(off).ok().and_then(|off| off.checked_add(addend)).ok_or(ReadError::TooLarge { what: "offset plus its addend" })
}

fn bytes_at<'o>(b: &'o [u8], at: u64, len: u64, what: &'static str) -> Result<&'o [u8], ReadError> {
    let end = at.checked_add(len).ok_or(ReadError::Truncated { what, at })?;
    if end > b.len() as u64 {
        return Err(ReadError::Truncated { what, at });
    }
    Ok(&b[at as usize..end as usize])
}

fn u16_at(b: &[u8], at: u64, what: &'static str) -> Result<u16, ReadError> {
    let s = bytes_at(b, at, 2, what)?;
    Ok(u16::from_le_bytes([s[0], s[1]]))
}

fn u32_at(b: &[u8], at: u64, what: &'static str) -> Result<u32, ReadError> {
    let s = bytes_at(b, at, 4, what)?;
    Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn u64_at(b: &[u8], at: u64, what: &'static str) -> Result<u64, ReadError> {
    let s = bytes_at(b, at, 8, what)?;
    let mut word = [0u8; 8];
    word.copy_from_slice(s);
    Ok(u64::from_le_bytes(word))
}

/// The NUL-terminated name at `at`.
fn cstr_at<'o>(b: &'o [u8], at: u64, what: &'static str) -> Result<&'o [u8], ReadError> {
    let rest = b.get(at as usize..).ok_or(ReadError::Truncated { what, at })?;
    let end = rest.iter().position(|&c| c == 0).ok_or(ReadError::Truncated { what, at })?;
    Ok(&rest[..end])
}

/// A fixed 16-byte Mach-O name without its NUL padding.
fn fixed(b: &[u8]) -> &[u8] {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    &b[..end]
}

/// `bl` or `blr`: an instruction after which the function's frame must be described.
fn is_call(inst: u32) -> bool {
    inst & 0xFC00_0000 == 0x9400_0000 || inst & 0xFFFF_FC1F == 0xD63F_0000
}

/// The first call in `code`, if any.
fn first_call(code: &[u8]) -> Option<u64> {
    code.chunks_exact(4)
        .position(|w| is_call(u32::from_le_bytes([w[0], w[1], w[2], w[3]])))
        .map(|i| 4 * i as u64)
}

/// The scale of the page-offset field of `inst`: 0 for an `add` (immediate), the access size's
/// log2 for a load or store with an unsigned offset (blaze-tier place.rs reads the same bits).
fn page_off_scale(inst: u32, at: u64) -> Result<u8, ReadError> {
    if inst & 0x7F80_0000 == 0x1100_0000 {
        return Ok(0);
    }
    if inst & 0x3B00_0000 == 0x3900_0000 {
        let size = inst >> 30;
        let vector = (inst >> 26) & 1 == 1;
        let opc_high = (inst >> 23) & 1 == 1;
        return Ok(if vector && size == 0 && opc_high { 4 } else { size as u8 });
    }
    Err(ReadError::PageOffInstruction { at, inst })
}

// ---------------------------------------------------------------------------------------------
// The frame: blaze's one form, checked against what LLVM described.

/// DWARF registers tracked: x0..x30, sp (31), and v0..v31 (64..95).
const DWARF_REGS: usize = 96;
const FP: u16 = 29;
const LR: u16 = 30;
/// The lower register of each saved pair, in blaze's order (unwind.rs `Frame::pairs`): x pairs,
/// then d pairs.
const PAIRS: [u16; 9] = [19, 21, 23, 25, 27, 72, 74, 76, 78];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Rule {
    /// The register keeps its value (not saved).
    Same,
    /// Saved at CFA plus this offset.
    At(i64),
}

#[derive(Clone, Copy)]
struct CfiState {
    cfa_reg: u16,
    cfa_off: i64,
    rules: [Rule; DWARF_REGS],
}

impl CfiState {
    const START: CfiState = CfiState { cfa_reg: 31, cfa_off: 0, rules: [Rule::Same; DWARF_REGS] };
}

fn rule_offset(rule: Rule) -> i64 {
    match rule {
        Rule::Same => 0,
        Rule::At(off) => off,
    }
}

/// The blaze frame `state` is, or the first rule of the form it breaks; `at` names the call.
fn frame_of(state: &CfiState, at: u64) -> Result<Frame, ReadError> {
    if state.cfa_reg != FP || state.cfa_off != 16 {
        return Err(ReadError::FrameLayout {
            at,
            what: "the canonical frame address is not x29 + 16",
            reg: state.cfa_reg,
            offset: state.cfa_off,
        });
    }
    for (reg, want) in [(FP, -16i64), (LR, -8)] {
        let rule = state.rules[usize::from(reg)];
        if rule != Rule::At(want) {
            return Err(ReadError::FrameLayout {
                at,
                what: "the frame record is not x29 at CFA - 16 and x30 at CFA - 8",
                reg,
                offset: rule_offset(rule),
            });
        }
    }
    let (mut x_pairs, mut d_pairs, mut k) = (0u8, 0u8, 0i64);
    for (bit, &lo) in PAIRS.iter().enumerate() {
        let (a, b) = (state.rules[usize::from(lo)], state.rules[usize::from(lo) + 1]);
        match (a, b) {
            (Rule::Same, Rule::Same) => {}
            (Rule::At(p), Rule::At(q)) if p == -32 - 16 * k && q == -24 - 16 * k => {
                match bit < 5 {
                    true => x_pairs |= 1 << bit,
                    false => d_pairs |= 1 << (bit - 5),
                }
                k += 1;
            }
            _ => {
                return Err(ReadError::FrameLayout {
                    at,
                    what: "a saved pair is not its lower register at CFA - 32 - 16k and its higher at CFA - 24 - 16k",
                    reg: lo,
                    offset: rule_offset(a),
                });
            }
        }
    }
    for reg in 0..DWARF_REGS as u16 {
        let paired = (19..=28).contains(&reg) || (72..=79).contains(&reg) || reg == FP || reg == LR;
        let rule = state.rules[usize::from(reg)];
        if !paired && rule != Rule::Same {
            return Err(ReadError::FrameLayout { at, what: "a register outside blaze's pairs is saved", reg, offset: rule_offset(rule) });
        }
    }
    Ok(Frame { x_pairs, d_pairs })
}

/// The calls of the code checked against the frame rule in force at each.
struct Calls<'c> {
    code: &'c [u8],
    body: Option<Frame>,
}

impl Calls<'_> {
    fn region(&mut self, lo: u64, hi: u64, state: &CfiState) -> Result<(), ReadError> {
        let hi = hi.min(self.code.len() as u64);
        let mut at = lo.next_multiple_of(4);
        while at + 4 <= hi {
            let inst = u32_at(self.code, at, "instruction")?;
            if is_call(inst) {
                let frame = frame_of(state, at)?;
                match self.body {
                    Some(body) if body != frame => return Err(ReadError::FrameChanges { at }),
                    _ => self.body = Some(frame),
                }
            }
            at += 4;
        }
        Ok(())
    }
}

/// A reader over one call-frame entry's bytes; `base` is their offset in the unwind section.
struct Cursor<'b> {
    bytes: &'b [u8],
    pos: usize,
    base: u64,
}

impl<'b> Cursor<'b> {
    fn at(&self) -> u64 {
        self.base + self.pos as u64
    }

    fn take(&mut self, n: usize) -> Result<&'b [u8], ReadError> {
        let at = self.at();
        let end = self.pos.checked_add(n).ok_or(ReadError::Truncated { what: "call-frame entry", at })?;
        let s = self.bytes.get(self.pos..end).ok_or(ReadError::Truncated { what: "call-frame entry", at })?;
        self.pos = end;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8, ReadError> {
        Ok(self.take(1)?[0])
    }

    /// An unsigned field of `n` bytes (1, 2, 4 or 8).
    fn unsigned(&mut self, n: usize) -> Result<u64, ReadError> {
        let s = self.take(n)?;
        Ok(s.iter().rev().fold(0u64, |acc, &b| (acc << 8) | u64::from(b)))
    }

    fn uleb(&mut self) -> Result<u64, ReadError> {
        let (mut value, mut shift) = (0u64, 0u32);
        loop {
            let at = self.at();
            let byte = self.u8()?;
            if shift > 63 {
                return Err(ReadError::Cfi { at, what: "a LEB128 number past 64 bits", op: byte });
            }
            value |= u64::from(byte & 0x7f) << shift;
            shift += 7;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
    }

    fn sleb(&mut self) -> Result<i64, ReadError> {
        let (mut value, mut shift) = (0i64, 0u32);
        loop {
            let at = self.at();
            let byte = self.u8()?;
            if shift > 63 {
                return Err(ReadError::Cfi { at, what: "a LEB128 number past 64 bits", op: byte });
            }
            value |= i64::from(byte & 0x7f) << shift;
            shift += 7;
            if byte & 0x80 == 0 {
                if shift < 64 && byte & 0x40 != 0 {
                    value |= -1i64 << shift;
                }
                return Ok(value);
            }
        }
    }

    fn cstr(&mut self) -> Result<&'b [u8], ReadError> {
        let at = self.at();
        let rest = self.bytes.get(self.pos..).ok_or(ReadError::Truncated { what: "call-frame augmentation", at })?;
        let end = rest.iter().position(|&c| c == 0).ok_or(ReadError::Truncated { what: "call-frame augmentation", at })?;
        self.pos += end + 1;
        Ok(&rest[..end])
    }

    fn done(&self) -> bool {
        self.pos >= self.bytes.len()
    }
}

/// The deepest `DW_CFA_remember_state` nesting read (rule 18: a table has a cap); LLVM nests
/// one per epilogue.
const CFI_STACK: usize = 4;

/// The call-frame factors of a CIE.
#[derive(Clone, Copy)]
struct Factors {
    code_align: u64,
    data_align: i64,
}

fn set_rule(state: &mut CfiState, reg: u64, rule: Rule, at: u64, op: u8) -> Result<(), ReadError> {
    let slot = state.rules.get_mut(reg as usize).ok_or(ReadError::Cfi { at, what: "a register past v31", op })?;
    *slot = rule;
    Ok(())
}

fn reg16(reg: u64, at: u64, op: u8) -> Result<u16, ReadError> {
    match reg < DWARF_REGS as u64 {
        true => Ok(reg as u16),
        false => Err(ReadError::Cfi { at, what: "a register past v31", op }),
    }
}

/// Runs a call-frame program (DWARF 5 section 6.4.2) from `state`. `initial` is what
/// `DW_CFA_restore` returns to; `calls` checks every region a location advance closes (a CIE's
/// initial instructions have none, and an advance there is refused).
fn run_cfi(
    c: &mut Cursor<'_>,
    f: Factors,
    initial: &CfiState,
    state: &mut CfiState,
    loc: &mut u64,
    mut calls: Option<&mut Calls<'_>>,
) -> Result<(), ReadError> {
    let mut stack = [CfiState::START; CFI_STACK];
    let mut depth = 0usize;
    while !c.done() {
        let at = c.at();
        let op = c.u8()?;
        let low = op & 0x3f;
        let advance = match op >> 6 {
            1 => Some(u64::from(low)),
            2 => {
                let off = c.uleb()?;
                let off = i64::try_from(off).ok().and_then(|o| o.checked_mul(f.data_align));
                let off = off.ok_or(ReadError::Cfi { at, what: "an offset past 64 bits", op })?;
                set_rule(state, u64::from(low), Rule::At(off), at, op)?;
                None
            }
            3 => {
                let reg = usize::from(low);
                state.rules[reg] = initial.rules[reg];
                None
            }
            _ => match op {
                0x00 => None,
                0x02 => Some(u64::from(c.u8()?)),
                0x03 => Some(c.unsigned(2)?),
                0x04 => Some(c.unsigned(4)?),
                0x05 | 0x11 => {
                    let reg = c.uleb()?;
                    let off = match op {
                        0x05 => i64::try_from(c.uleb()?).ok(),
                        _ => Some(c.sleb()?),
                    };
                    let off = off.and_then(|o| o.checked_mul(f.data_align)).ok_or(ReadError::Cfi { at, what: "an offset past 64 bits", op })?;
                    set_rule(state, reg, Rule::At(off), at, op)?;
                    None
                }
                0x06 => {
                    let reg = usize::from(reg16(c.uleb()?, at, op)?);
                    state.rules[reg] = initial.rules[reg];
                    None
                }
                0x08 => {
                    let reg = c.uleb()?;
                    set_rule(state, reg, Rule::Same, at, op)?;
                    None
                }
                0x0a => {
                    if depth == CFI_STACK {
                        return Err(ReadError::Cfi { at, what: "a remembered state nested past the reader's cap of 4", op });
                    }
                    stack[depth] = *state;
                    depth += 1;
                    None
                }
                0x0b => {
                    if depth == 0 {
                        return Err(ReadError::Cfi { at, what: "a restored state never remembered", op });
                    }
                    depth -= 1;
                    *state = stack[depth];
                    None
                }
                0x0c => {
                    state.cfa_reg = reg16(c.uleb()?, at, op)?;
                    state.cfa_off = i64::try_from(c.uleb()?).map_err(|_| ReadError::Cfi { at, what: "an offset past 64 bits", op })?;
                    None
                }
                0x0d => {
                    state.cfa_reg = reg16(c.uleb()?, at, op)?;
                    None
                }
                0x0e => {
                    state.cfa_off = i64::try_from(c.uleb()?).map_err(|_| ReadError::Cfi { at, what: "an offset past 64 bits", op })?;
                    None
                }
                0x12 => {
                    state.cfa_reg = reg16(c.uleb()?, at, op)?;
                    state.cfa_off = c.sleb()?.checked_mul(f.data_align).ok_or(ReadError::Cfi { at, what: "an offset past 64 bits", op })?;
                    None
                }
                0x13 => {
                    state.cfa_off = c.sleb()?.checked_mul(f.data_align).ok_or(ReadError::Cfi { at, what: "an offset past 64 bits", op })?;
                    None
                }
                0x01 => return Err(ReadError::Cfi { at, what: "DW_CFA_set_loc", op }),
                0x07 => return Err(ReadError::Cfi { at, what: "DW_CFA_undefined", op }),
                0x09 => return Err(ReadError::Cfi { at, what: "DW_CFA_register (a register saved in another)", op }),
                0x0f | 0x10 | 0x16 => return Err(ReadError::Cfi { at, what: "a DWARF expression rule", op }),
                0x14 | 0x15 => return Err(ReadError::Cfi { at, what: "a value-offset rule", op }),
                0x2d => return Err(ReadError::Cfi { at, what: "DW_CFA_AARCH64_negate_ra_state (pointer authentication)", op }),
                _ => return Err(ReadError::Cfi { at, what: "an instruction this reader does not know", op }),
            },
        };
        if let Some(delta) = advance {
            let Some(calls) = calls.as_deref_mut() else {
                return Err(ReadError::Cfi { at, what: "a location advance in a CIE's initial instructions", op });
            };
            let next = delta.checked_mul(f.code_align).and_then(|d| loc.checked_add(d));
            let next = next.ok_or(ReadError::Cfi { at, what: "a location past 64 bits", op })?;
            calls.region(*loc, next, state)?;
            *loc = next;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Mach-O (arm64).

const MH_MAGIC_64: u32 = 0xfeed_facf;
const CPU_TYPE_ARM64: u32 = 0x0100_000c;
const MH_OBJECT: u32 = 1;
const LC_SEGMENT_64: u32 = 0x19;
const LC_SYMTAB: u32 = 0x2;
/// Read as named in the module's documentation (nothing in them changes a byte of the code).
const LC_IGNORED: [u32; 5] = [0xb, 0x32, 0x24, 0x2e, 0x29];
const N_STAB: u8 = 0xe0;
const N_PEXT: u8 = 0x10;
const N_TYPE: u8 = 0x0e;
const N_EXT: u8 = 0x01;
const N_UNDF: u8 = 0x0;
const N_SECT: u8 = 0xe;
const UNWIND_ENTRY: u64 = 32;
const UNWIND_MODE: u32 = 0x0F00_0000;
const UNWIND_FRAME: u32 = 0x0400_0000;
const UNWIND_FRAMELESS: u32 = 0x0200_0000;
/// The frame mode's saved pairs: x19_x20 .. x27_x28 (bits 0..4), d8_d9 .. d14_d15 (bits 8..11).
const UNWIND_FRAME_PAIRS: u32 = 0x0000_0F1F;
const UNWIND_FRAMELESS_SIZE: u32 = 0x00FF_F000;

/// What a section is to the record.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Code,
    Relro,
    /// Mach-O's `__LD,__compact_unwind`; ELF's `.eh_frame`.
    Unwind,
    Lsda,
    /// No bytes, no relocations.
    Empty,
    /// ELF: the symbol table.
    Symtab,
    /// ELF: a string table, read through the section that links it.
    Strings,
    /// ELF: relocations of another section.
    Rela,
    /// ELF: a COMDAT group (only the personality word's is read).
    Group,
    /// ELF: the personality's `DW.ref.` word.
    Personality,
    /// ELF: `.note.GNU-stack`, empty.
    Note,
}

#[derive(Clone, Copy)]
struct MSect<'o> {
    name: ObjName,
    addr: u64,
    size: u64,
    bytes: &'o [u8],
    reloff: u32,
    nreloc: u32,
    role: Role,
    relro_at: u64,
}

#[derive(Clone, Copy)]
struct MSym<'o> {
    name: &'o [u8],
    kind: u8,
    ext: bool,
    sect: u8,
    value: u64,
}

struct MachO<'o, 's> {
    o: &'o [u8],
    sects: &'s [MSect<'o>],
    symoff: u64,
    nsyms: u32,
    strs: &'o [u8],
}

#[derive(Clone, Copy)]
struct MReloc {
    at: u64,
    symnum: u32,
    pcrel: bool,
    length: u8,
    ext: bool,
    ty: u8,
}

/// `ARM64_RELOC_*`'s names, for a refusal.
fn macho_reloc_name(ty: u8) -> &'static str {
    match ty {
        0 => "ARM64_RELOC_UNSIGNED",
        1 => "ARM64_RELOC_SUBTRACTOR",
        2 => "ARM64_RELOC_BRANCH26",
        3 => "ARM64_RELOC_PAGE21",
        4 => "ARM64_RELOC_PAGEOFF12",
        5 => "ARM64_RELOC_GOT_LOAD_PAGE21",
        6 => "ARM64_RELOC_GOT_LOAD_PAGEOFF12",
        7 => "ARM64_RELOC_POINTER_TO_GOT",
        8 => "ARM64_RELOC_TLVP_LOAD_PAGE21",
        9 => "ARM64_RELOC_TLVP_LOAD_PAGEOFF12",
        10 => "ARM64_RELOC_ADDEND",
        11 => "ARM64_RELOC_AUTHENTICATED_POINTER",
        _ => "an ARM64_RELOC type with no name",
    }
}

fn macho_role(seg: &[u8], name: &[u8], size: u64, nreloc: u32) -> Option<Role> {
    match (seg, name) {
        (b"__TEXT", b"__text") => Some(Role::Code),
        (b"__TEXT", b"__const" | b"__cstring" | b"__literal4" | b"__literal8" | b"__literal16") => Some(Role::Relro),
        (b"__DATA" | b"__DATA_CONST", b"__const") => Some(Role::Relro),
        (b"__LD", b"__compact_unwind") => Some(Role::Unwind),
        (b"__TEXT", b"__gcc_except_tab") => Some(Role::Lsda),
        _ if size == 0 && nreloc == 0 => Some(Role::Empty),
        _ => None,
    }
}

impl<'o> MachO<'o, '_> {
    fn sym(&self, index: u32) -> Result<MSym<'o>, ReadError> {
        if index >= self.nsyms {
            return Err(ReadError::NoSuchSymbol { index: u64::from(index) });
        }
        let at = self.symoff + 16 * u64::from(index);
        let strx = u32_at(self.o, at, "symbol")?;
        let n_type = *bytes_at(self.o, at + 4, 1, "symbol")?.first().unwrap_or(&0);
        let sect = *bytes_at(self.o, at + 5, 1, "symbol")?.first().unwrap_or(&0);
        let value = u64_at(self.o, at + 8, "symbol")?;
        let name = cstr_at(self.strs, u64::from(strx), "symbol name")?;
        if n_type & N_STAB != 0 {
            return Err(ReadError::SymbolKind { name: ObjName::of(&[name]), kind: "a debugging (stab) symbol" });
        }
        Ok(MSym { name, kind: n_type & N_TYPE, ext: n_type & (N_EXT | N_PEXT) != 0, sect, value })
    }

    /// Section `ordinal` (1-based) and the offset of `addr` in it.
    fn in_section(&self, ordinal: u64, addr: u64) -> Result<(&MSect<'o>, u64), ReadError> {
        let sect = ordinal.checked_sub(1).and_then(|i| self.sects.get(i as usize)).ok_or(ReadError::NoSuchSection { index: ordinal })?;
        match addr.checked_sub(sect.addr) {
            Some(off) if off <= sect.size => Ok((sect, off)),
            _ => Err(ReadError::TargetSection { name: sect.name, at: addr }),
        }
    }

    fn dest_in(sect: &MSect<'o>, off: u64, at: u64) -> Result<Dest<'o>, ReadError> {
        match sect.role {
            Role::Code => Ok(Dest::Code(off)),
            Role::Relro => Ok(Dest::Relro(sect.relro_at + off)),
            _ => Err(ReadError::TargetSection { name: sect.name, at }),
        }
    }

    /// Where an extern relocation's symbol is.
    fn dest_of_symbol(&self, index: u32, at: u64) -> Result<Dest<'o>, ReadError> {
        let s = self.sym(index)?;
        let name = ObjName::of(&[s.name]);
        match s.kind {
            N_SECT => {
                let (sect, off) = self.in_section(u64::from(s.sect), s.value)?;
                MachO::dest_in(sect, off, at)
            }
            N_UNDF if s.ext && s.value == 0 => match s.name.split_first() {
                Some((b'_', rest)) => Ok(Dest::Name(rest)),
                _ => Err(ReadError::SymbolKind { name, kind: "an undefined name without Mach-O's leading underscore" }),
            },
            N_UNDF => Err(ReadError::SymbolKind { name, kind: "a common or local undefined symbol" }),
            _ => Err(ReadError::SymbolKind { name, kind: "an absolute, indirect or prebound symbol" }),
        }
    }

    fn relocs(&self, sect: &MSect<'o>, i: u32) -> Result<MReloc, ReadError> {
        let base = u64::from(sect.reloff) + 8 * u64::from(i);
        let addr = u32_at(self.o, base, "relocation")?;
        let info = u32_at(self.o, base + 4, "relocation")?;
        if addr & 0x8000_0000 != 0 {
            return Err(ReadError::RelocationShape { section: sect.name, at: u64::from(addr & 0x7fff_ffff), what: "a scattered relocation" });
        }
        Ok(MReloc {
            at: u64::from(addr),
            symnum: info & 0x00ff_ffff,
            pcrel: (info >> 24) & 1 == 1,
            length: ((info >> 25) & 3) as u8,
            ext: (info >> 27) & 1 == 1,
            ty: (info >> 28) as u8,
        })
    }
}

/// A pointer field of the compact unwind entry, resolved.
#[derive(Clone, Copy)]
enum Pointer<'o> {
    /// No relocation: the field must be 0.
    None,
    /// Section index (0-based) and offset in it.
    In(usize, u64),
    Name(&'o [u8]),
}

fn read_macho<'a>(o: &[u8], symbol: &str, mut out: Out<'a, '_>, alloc: Alloc<'a>) -> Result<Emitted<'a>, ReadError> {
    if u32_at(o, 0, "Mach-O header")? != MH_MAGIC_64 {
        return Err(ReadError::Format { what: "not a 64-bit Mach-O file" });
    }
    if u32_at(o, 4, "Mach-O header")? != CPU_TYPE_ARM64 {
        return Err(ReadError::Format { what: "a Mach-O file for a CPU other than arm64" });
    }
    if u32_at(o, 12, "Mach-O header")? != MH_OBJECT {
        return Err(ReadError::Format { what: "a Mach-O file that is not a relocatable object (MH_OBJECT)" });
    }
    let ncmds = u32_at(o, 16, "Mach-O header")?;
    let cmds_end = 32 + u64::from(u32_at(o, 20, "Mach-O header")?);
    let mut sects: AVec<'a, MSect<'_>> = Vec::new_in(alloc);
    let mut symtab: Option<(u64, u32, &[u8])> = None;
    let mut at = 32u64;
    for _ in 0..ncmds {
        let cmd = u32_at(o, at, "load command")?;
        let size = u64::from(u32_at(o, at + 4, "load command")?);
        if size < 8 || at + size > cmds_end {
            return Err(ReadError::Truncated { what: "load command", at });
        }
        match cmd {
            LC_SEGMENT_64 => {
                let nsects = u64::from(u32_at(o, at + 64, "segment command")?);
                if 72 + 80 * nsects > size {
                    return Err(ReadError::Truncated { what: "segment command's sections", at });
                }
                for i in 0..nsects {
                    let s = at + 72 + 80 * i;
                    let sectname = fixed(bytes_at(o, s, 16, "section")?);
                    let segname = fixed(bytes_at(o, s + 16, 16, "section")?);
                    let name = ObjName::of(&[segname, &b","[..], sectname]);
                    let addr = u64_at(o, s + 32, "section")?;
                    let size = u64_at(o, s + 40, "section")?;
                    let offset = u32_at(o, s + 48, "section")?;
                    let align_log2 = u32_at(o, s + 52, "section")?;
                    let reloff = u32_at(o, s + 56, "section")?;
                    let nreloc = u32_at(o, s + 60, "section")?;
                    let kind = u32_at(o, s + 64, "section")? & 0xff;
                    let role = macho_role(segname, sectname, size, nreloc).ok_or(ReadError::Section { name })?;
                    let mut bytes: &[u8] = &[];
                    if role != Role::Empty {
                        // S_REGULAR, S_CSTRING_LITERALS, S_4BYTE_LITERALS, S_8BYTE_LITERALS,
                        // S_16BYTE_LITERALS: bytes in the file; zero-fill and the rest are not.
                        if !matches!(kind, 0x0 | 0x2 | 0x3 | 0x4 | 0xe) {
                            return Err(ReadError::SectionType { name, kind });
                        }
                        bytes = bytes_at(o, u64::from(offset), size, "section bytes")?;
                    }
                    let align = 1u64.checked_shl(align_log2).ok_or(ReadError::SectionAlign { name, align: u64::MAX })?;
                    let relro_at = match role {
                        Role::Relro => out.relro(bytes, align, name)?,
                        _ => 0,
                    };
                    sects.try_push(MSect { name, addr, size, bytes, reloff, nreloc, role, relro_at }).map_err(|refused| ReadError::Alloc(refused.why))?;
                }
            }
            LC_SYMTAB => {
                let symoff = u64::from(u32_at(o, at + 8, "symtab command")?);
                let nsyms = u32_at(o, at + 12, "symtab command")?;
                let stroff = u64::from(u32_at(o, at + 16, "symtab command")?);
                let strsize = u64::from(u32_at(o, at + 20, "symtab command")?);
                bytes_at(o, symoff, 16 * u64::from(nsyms), "symbol table")?;
                symtab = Some((symoff, nsyms, bytes_at(o, stroff, strsize, "string table")?));
            }
            cmd if LC_IGNORED.contains(&cmd) => {}
            cmd => return Err(ReadError::LoadCommand { cmd }),
        }
        at += size;
    }
    let want = ObjName::of(&[&b"_"[..], symbol.as_bytes()]);
    let text_index = sects.iter().position(|s| s.role == Role::Code).ok_or(ReadError::NoFunction { name: want })?;
    let text = sects[text_index];
    let Some((symoff, nsyms, strs)) = symtab else {
        return Err(ReadError::NoFunction { name: want });
    };
    let m = MachO { o, sects: &sects, symoff, nsyms, strs };

    // The function: the one defined global, at the text's first byte.
    let mut found = false;
    for i in 0..nsyms {
        let s = m.sym(i)?;
        if s.kind != N_SECT || !s.ext {
            continue;
        }
        let name = ObjName::of(&[s.name]);
        if s.name.first() != Some(&b'_') || &s.name[1..] != symbol.as_bytes() {
            return Err(ReadError::ExtraDefinition { name });
        }
        if usize::from(s.sect) != text_index + 1 {
            return Err(ReadError::NoFunction { name });
        }
        if s.value != text.addr {
            return Err(ReadError::EntryNotFirst { name, at: s.value.wrapping_sub(text.addr) });
        }
        found = true;
    }
    if !found {
        return Err(ReadError::NoFunction { name: want });
    }
    out.e.code.try_extend_from_slice(text.bytes).map_err(ReadError::Alloc)?;

    let mut unwind: Option<MSect<'_>> = None;
    for sect in sects.iter() {
        match sect.role {
            Role::Code => macho_code_relocs(&m, sect, &mut out)?,
            Role::Relro => macho_relro_relocs(&m, sect, &mut out)?,
            Role::Lsda if sect.nreloc != 0 => return Err(ReadError::LsdaRelocation { section: sect.name }),
            Role::Unwind => match unwind {
                Some(_) => return Err(ReadError::Section { name: sect.name }),
                None => unwind = Some(*sect),
            },
            _ => {}
        }
    }
    let (frame, lsda) = match unwind {
        Some(cu) => macho_unwind(&m, &cu, &out.e.code[..])?,
        None => (None, None),
    };
    out.finish(frame, lsda, alloc)
}

/// The text's relocations, each one [`Reloc`].
fn macho_code_relocs(m: &MachO<'_, '_>, sect: &MSect<'_>, out: &mut Out<'_, '_>) -> Result<(), ReadError> {
    let mut pending: Option<(u64, i64)> = None;
    for i in 0..sect.nreloc {
        let r = m.relocs(sect, i)?;
        let shape = |what| ReadError::RelocationShape { section: sect.name, at: r.at, what };
        if r.ty == 10 {
            if pending.is_some() {
                return Err(shape("a second ARM64_RELOC_ADDEND before any relocation takes the first"));
            }
            // r_symbolnum holds the addend, 24 bits, signed.
            let addend = i64::from(((r.symnum << 8) as i32) >> 8);
            pending = Some((r.at, addend));
            continue;
        }
        let addend = match pending.take() {
            Some((at, addend)) if at == r.at => addend,
            Some(_) => return Err(shape("an ARM64_RELOC_ADDEND not followed by a relocation at its offset")),
            None => 0,
        };
        let inst = u32_at(sect.bytes, r.at, "relocated instruction")?;
        let (kind, field) = match r.ty {
            2 => (RelocKind::Branch26, inst & 0x03FF_FFFF),
            3 => (RelocKind::Page21, inst & 0x60FF_FFE0),
            4 => (RelocKind::PageOff12 { scale: page_off_scale(inst, r.at)? }, (inst >> 10) & 0xFFF),
            5 => (RelocKind::GotPage21, inst & 0x60FF_FFE0),
            6 => (RelocKind::GotPageOff12, (inst >> 10) & 0xFFF),
            ty => {
                return Err(ReadError::Relocation { section: sect.name, at: r.at, kind: macho_reloc_name(ty), number: u32::from(ty) });
            }
        };
        if r.length != 2 || !r.ext {
            return Err(shape("an instruction relocation that is not 4 bytes against a symbol"));
        }
        if r.pcrel != matches!(r.ty, 2 | 3 | 5) {
            return Err(shape("an instruction relocation with the wrong pc-relative flag for its kind"));
        }
        if field != 0 {
            return Err(ReadError::ImplicitAddend { at: r.at, inst });
        }
        let dest = m.dest_of_symbol(r.symnum, r.at)?;
        out.reloc(r.at, false, kind, dest, addend)?;
    }
    match pending {
        Some(_) => Err(ReadError::RelocationShape { section: sect.name, at: 0, what: "an ARM64_RELOC_ADDEND with no relocation after it" }),
        None => Ok(()),
    }
}

/// A relro section's relocations: 8-byte absolute words only.
fn macho_relro_relocs(m: &MachO<'_, '_>, sect: &MSect<'_>, out: &mut Out<'_, '_>) -> Result<(), ReadError> {
    for i in 0..sect.nreloc {
        let r = m.relocs(sect, i)?;
        if r.ty != 0 {
            return Err(ReadError::Relocation { section: sect.name, at: r.at, kind: macho_reloc_name(r.ty), number: u32::from(r.ty) });
        }
        if r.length != 3 || r.pcrel {
            return Err(ReadError::RelocationShape { section: sect.name, at: r.at, what: "an ARM64_RELOC_UNSIGNED that is not an 8-byte absolute word" });
        }
        let implicit = u64_at(sect.bytes, r.at, "relocated word")?;
        let (dest, addend) = match r.ext {
            true => (m.dest_of_symbol(r.symnum, r.at)?, implicit as i64),
            // Section-relative: the word holds the target's address in the object.
            false => {
                let (target, off) = m.in_section(u64::from(r.symnum), implicit)?;
                (MachO::dest_in(target, off, r.at)?, 0)
            }
        };
        let at = sect.relro_at + r.at;
        // The word's bytes are the placer's to write; the record keeps them zero, so two
        // builders write equal bytes whatever the object held there.
        let start = at as usize;
        out.e.relro[start..start + 8].copy_from_slice(&[0u8; 8]);
        out.reloc(at, true, RelocKind::Abs64, dest, addend)?;
    }
    Ok(())
}

/// The compact unwind entry: the frame, and the LSDA's bytes from the function's table on.
fn macho_unwind<'o>(m: &MachO<'o, '_>, cu: &MSect<'o>, code: &[u8]) -> Result<(Option<Frame>, Option<&'o [u8]>), ReadError> {
    if cu.size % UNWIND_ENTRY != 0 {
        return Err(ReadError::Truncated { what: "compact unwind entry", at: cu.size - cu.size % UNWIND_ENTRY });
    }
    let count = cu.size / UNWIND_ENTRY;
    if count == 0 {
        return Ok((None, None));
    }
    if count > 1 {
        return Err(ReadError::UnwindEntries { count });
    }
    // The entry's three pointer fields: the function (0), the personality (16), the LSDA (24).
    let mut fields: [Pointer<'o>; 3] = [Pointer::None; 3];
    for i in 0..cu.nreloc {
        let r = m.relocs(cu, i)?;
        let shape = |what| ReadError::RelocationShape { section: cu.name, at: r.at, what };
        if r.ty != 0 || r.length != 3 || r.pcrel {
            return Err(shape("not an 8-byte ARM64_RELOC_UNSIGNED"));
        }
        let slot = match r.at {
            0 => 0,
            16 => 1,
            24 => 2,
            _ => return Err(shape("on a field of the unwind entry that holds no pointer")),
        };
        let implicit = u64_at(cu.bytes, r.at, "unwind entry")?;
        fields[slot] = match r.ext {
            true => {
                let s = m.sym(r.symnum)?;
                match s.kind {
                    N_SECT => {
                        let (_, off) = m.in_section(u64::from(s.sect), s.value)?;
                        Pointer::In(usize::from(s.sect) - 1, off.wrapping_add(implicit))
                    }
                    N_UNDF if implicit == 0 => match s.name.split_first() {
                        Some((b'_', rest)) => Pointer::Name(rest),
                        _ => return Err(ReadError::SymbolKind { name: ObjName::of(&[s.name]), kind: "an undefined name without Mach-O's leading underscore" }),
                    },
                    _ => return Err(shape("a pointer to an undefined name with an addend, or to an absolute symbol")),
                }
            }
            false => {
                let (target, off) = m.in_section(u64::from(r.symnum), implicit)?;
                let index = m.sects.iter().position(|s| core::ptr::eq(s, target)).ok_or(ReadError::NoSuchSection { index: u64::from(r.symnum) })?;
                Pointer::In(index, off)
            }
        };
    }
    for (slot, field_at) in [(0usize, 0u64), (1, 16), (2, 24)] {
        if matches!(fields[slot], Pointer::None) && u64_at(cu.bytes, field_at, "unwind entry")? != 0 {
            return Err(ReadError::RelocationShape { section: cu.name, at: field_at, what: "a nonzero pointer with no relocation" });
        }
    }
    let len = u64::from(u32_at(cu.bytes, 8, "unwind entry")?);
    let encoding = u32_at(cu.bytes, 12, "unwind entry")?;
    let code_len = code.len() as u64;
    match fields[0] {
        Pointer::In(index, 0) if m.sects[index].role == Role::Code && len == code_len => {}
        Pointer::In(_, start) => return Err(ReadError::UnwindCover { start, len, code: code_len }),
        _ => return Err(ReadError::RelocationShape { section: cu.name, at: 0, what: "an unwind entry whose function is not a section address" }),
    }
    let personality = match fields[1] {
        Pointer::None => false,
        Pointer::Name(name) if name == PERSONALITY.as_bytes() => true,
        Pointer::Name(name) => return Err(ReadError::Personality { name: ObjName::of(&[name]) }),
        Pointer::In(index, _) => return Err(ReadError::Personality { name: m.sects[index].name }),
    };
    let lsda = match fields[2] {
        Pointer::None => None,
        Pointer::In(index, off) if m.sects[index].role == Role::Lsda => {
            let bytes = m.sects[index].bytes;
            Some(bytes.get(off as usize..).ok_or(ReadError::LsdaAddress { at: off })?)
        }
        Pointer::In(_, off) => return Err(ReadError::LsdaAddress { at: off }),
        Pointer::Name(name) => return Err(ReadError::SymbolKind { name: ObjName::of(&[name]), kind: "an LSDA outside the object" }),
    };
    if lsda.is_some() && !personality {
        return Err(ReadError::LsdaWithoutPersonality);
    }
    let frame = match encoding & UNWIND_MODE {
        UNWIND_FRAME => {
            if encoding & !(UNWIND_MODE | UNWIND_FRAME_PAIRS) != 0 {
                return Err(ReadError::UnwindEncoding { encoding });
            }
            Some(frame_of(&compact_state(encoding), 0)?)
        }
        UNWIND_FRAMELESS => {
            if encoding & !(UNWIND_MODE | UNWIND_FRAMELESS_SIZE) != 0 {
                return Err(ReadError::UnwindEncoding { encoding });
            }
            if let Some(at) = first_call(code) {
                return Err(ReadError::FramelessCall { at });
            }
            None
        }
        0 if encoding == 0 => None,
        _ => return Err(ReadError::UnwindEncoding { encoding }),
    };
    Ok((frame, lsda))
}

/// The rules libunwind reads from a compact frame-mode encoding (libunwind
/// CompactUnwinder.hpp, `stepWithCompactEncodingFrame` for arm64): CFA `fp + 16`, fp and lr at
/// `CFA - 16` and `CFA - 8`, and from `fp - 8` down, for each saved pair in the order x19_x20 ..
/// x27_x28, d8_d9 .. d14_d15, the LOWER register first at the higher address.
fn compact_state(encoding: u32) -> CfiState {
    let mut s = CfiState::START;
    s.cfa_reg = FP;
    s.cfa_off = 16;
    s.rules[usize::from(FP)] = Rule::At(-16);
    s.rules[usize::from(LR)] = Rule::At(-8);
    let mut loc = -24i64;
    let bits = [0x1u32, 0x2, 0x4, 0x8, 0x10, 0x100, 0x200, 0x400, 0x800];
    for (bit, &lo) in bits.iter().zip(PAIRS.iter()) {
        if encoding & bit != 0 {
            s.rules[usize::from(lo)] = Rule::At(loc);
            s.rules[usize::from(lo) + 1] = Rule::At(loc - 8);
            loc -= 16;
        }
    }
    s
}

// ---------------------------------------------------------------------------------------------
// ELF (aarch64).

const EM_AARCH64: u16 = 183;
const ET_REL: u16 = 1;
const SHT_NULL: u32 = 0;
const SHT_PROGBITS: u32 = 1;
const SHT_SYMTAB: u32 = 2;
const SHT_STRTAB: u32 = 3;
const SHT_RELA: u32 = 4;
const SHT_REL: u32 = 9;
const SHT_GROUP: u32 = 17;
const SHF_EXECINSTR: u64 = 0x4;
const SHN_UNDEF: u16 = 0;
const SHN_LORESERVE: u16 = 0xff00;
const STB_LOCAL: u8 = 0;
const STT_SECTION: u8 = 3;
const STT_FILE: u8 = 4;
const STT_TLS: u8 = 6;
const R_ABS64: u32 = 257;
const R_PREL32: u32 = 261;

#[derive(Clone, Copy)]
struct ESect<'o> {
    oname: ObjName,
    bytes: &'o [u8],
    size: u64,
    link: u32,
    info: u32,
    role: Role,
    relro_at: u64,
}

#[derive(Clone, Copy)]
struct ESym<'o> {
    name: &'o [u8],
    bind: u8,
    kind: u8,
    shndx: u16,
    value: u64,
}

struct Elf<'o, 's> {
    sects: &'s [ESect<'o>],
    syms: &'o [u8],
    strs: &'o [u8],
}

/// `R_AARCH64_*`'s names, for a refusal.
fn elf_reloc_name(ty: u32) -> &'static str {
    match ty {
        257 => "R_AARCH64_ABS64",
        258 => "R_AARCH64_ABS32",
        260 => "R_AARCH64_PREL64",
        261 => "R_AARCH64_PREL32",
        274 => "R_AARCH64_ADR_PREL_LO21",
        275 => "R_AARCH64_ADR_PREL_PG_HI21",
        277 => "R_AARCH64_ADD_ABS_LO12_NC",
        278 => "R_AARCH64_LDST8_ABS_LO12_NC",
        282 => "R_AARCH64_JUMP26",
        283 => "R_AARCH64_CALL26",
        284 => "R_AARCH64_LDST16_ABS_LO12_NC",
        285 => "R_AARCH64_LDST32_ABS_LO12_NC",
        286 => "R_AARCH64_LDST64_ABS_LO12_NC",
        299 => "R_AARCH64_LDST128_ABS_LO12_NC",
        311 => "R_AARCH64_ADR_GOT_PAGE",
        312 => "R_AARCH64_LD64_GOT_LO12_NC",
        541..=573 => "an R_AARCH64 TLS relocation",
        _ => "an R_AARCH64 type with no row",
    }
}

fn elf_role(name: &[u8], ty: u32, flags: u64, size: u64) -> Option<Role> {
    match ty {
        SHT_NULL | SHT_STRTAB => return Some(Role::Strings),
        SHT_SYMTAB => return Some(Role::Symtab),
        SHT_RELA => return Some(Role::Rela),
        SHT_GROUP => return Some(Role::Group),
        SHT_REL => return None,
        _ => {}
    }
    if flags & SHF_EXECINSTR != 0 {
        return Some(if size == 0 { Role::Empty } else { Role::Code });
    }
    if name == b".eh_frame" {
        return Some(Role::Unwind);
    }
    if name.starts_with(b".gcc_except_table") {
        return Some(Role::Lsda);
    }
    if name == ELF_PERSONALITY_SECTION {
        return Some(Role::Personality);
    }
    if name == b".note.GNU-stack" && size == 0 {
        return Some(Role::Note);
    }
    if name == b".rodata" || name.starts_with(b".rodata.") || name == b".data.rel.ro" || name.starts_with(b".data.rel.ro.") {
        return Some(Role::Relro);
    }
    if size == 0 {
        return Some(Role::Empty);
    }
    None
}

impl<'o> Elf<'o, '_> {
    fn sect(&self, index: u64) -> Result<&ESect<'o>, ReadError> {
        self.sects.get(index as usize).ok_or(ReadError::NoSuchSection { index })
    }

    fn sym(&self, index: u32) -> Result<ESym<'o>, ReadError> {
        let at = 24 * u64::from(index);
        if index == 0 || at + 24 > self.syms.len() as u64 {
            return Err(ReadError::NoSuchSymbol { index: u64::from(index) });
        }
        let name = cstr_at(self.strs, u64::from(u32_at(self.syms, at, "symbol")?), "symbol name")?;
        let info = self.syms[at as usize + 4];
        Ok(ESym { name, bind: info >> 4, kind: info & 0xf, shndx: u16_at(self.syms, at + 6, "symbol")?, value: u64_at(self.syms, at + 8, "symbol")? })
    }

    fn count(&self) -> u32 {
        (self.syms.len() / 24) as u32
    }

    fn dest_of_symbol(&self, index: u32, at: u64) -> Result<Dest<'o>, ReadError> {
        let s = self.sym(index)?;
        let name = ObjName::of(&[s.name]);
        if s.kind == STT_TLS {
            return Err(ReadError::SymbolKind { name, kind: "a thread-local" });
        }
        match s.shndx {
            SHN_UNDEF if s.bind != STB_LOCAL => Ok(Dest::Name(s.name)),
            SHN_UNDEF => Err(ReadError::SymbolKind { name, kind: "a local undefined symbol" }),
            shndx if shndx >= SHN_LORESERVE => Err(ReadError::SymbolKind { name, kind: "an absolute, common or extended-index symbol" }),
            shndx => {
                let sect = self.sect(u64::from(shndx))?;
                match sect.role {
                    Role::Code => Ok(Dest::Code(s.value)),
                    Role::Relro => Ok(Dest::Relro(sect.relro_at + s.value)),
                    _ => Err(ReadError::TargetSection { name: sect.oname, at }),
                }
            }
        }
    }

    /// Relocation `i` of `rela`: (offset, symbol, type, addend).
    fn rela(rela: &ESect<'o>, i: u64) -> Result<(u64, u32, u32, i64), ReadError> {
        let at = 24 * i;
        let offset = u64_at(rela.bytes, at, "relocation")?;
        let info = u64_at(rela.bytes, at + 8, "relocation")?;
        let addend = u64_at(rela.bytes, at + 16, "relocation")? as i64;
        Ok((offset, (info >> 32) as u32, info as u32, addend))
    }
}

fn read_elf<'a>(o: &[u8], symbol: &str, mut out: Out<'a, '_>, alloc: Alloc<'a>) -> Result<Emitted<'a>, ReadError> {
    let ident = bytes_at(o, 0, 16, "ELF header")?;
    if ident[4] != 2 || ident[5] != 1 || ident[6] != 1 {
        return Err(ReadError::Format { what: "an ELF file that is not 64-bit little-endian version 1" });
    }
    if u16_at(o, 16, "ELF header")? != ET_REL {
        return Err(ReadError::Format { what: "an ELF file that is not a relocatable object (ET_REL)" });
    }
    if u16_at(o, 18, "ELF header")? != EM_AARCH64 {
        return Err(ReadError::Format { what: "an ELF object for a machine other than aarch64" });
    }
    let shoff = u64_at(o, 0x28, "ELF header")?;
    if u16_at(o, 0x3a, "ELF header")? != 64 {
        return Err(ReadError::Format { what: "an ELF object whose section headers are not 64 bytes" });
    }
    let shnum = u64::from(u16_at(o, 0x3c, "ELF header")?);
    let shstrndx = u64::from(u16_at(o, 0x3e, "ELF header")?);
    if shnum == 0 || shstrndx >= shnum {
        return Err(ReadError::Format { what: "an ELF object with extended section numbering" });
    }
    let shstr_at = shoff + 64 * shstrndx;
    let shstrtab = bytes_at(o, u64_at(o, shstr_at + 24, "section header")?, u64_at(o, shstr_at + 32, "section header")?, "section names")?;
    let mut sects: AVec<'a, ESect<'_>> = Vec::new_in(alloc);
    let mut text: Option<usize> = None;
    let mut symtab: Option<usize> = None;
    for i in 0..shnum {
        let h = shoff + 64 * i;
        let name = cstr_at(shstrtab, u64::from(u32_at(o, h, "section header")?), "section name")?;
        let oname = ObjName::of(&[name]);
        let ty = u32_at(o, h + 4, "section header")?;
        let flags = u64_at(o, h + 8, "section header")?;
        let offset = u64_at(o, h + 24, "section header")?;
        let size = u64_at(o, h + 32, "section header")?;
        let link = u32_at(o, h + 40, "section header")?;
        let info = u32_at(o, h + 44, "section header")?;
        let align = u64_at(o, h + 48, "section header")?;
        let role = elf_role(name, ty, flags, size).ok_or(ReadError::Section { name: oname })?;
        let bytes: &[u8] = match (role, ty) {
            (Role::Empty | Role::Note, _) => &[],
            (Role::Code | Role::Relro | Role::Unwind | Role::Lsda | Role::Personality, SHT_PROGBITS) => bytes_at(o, offset, size, "section bytes")?,
            (Role::Code | Role::Relro | Role::Unwind | Role::Lsda | Role::Personality, ty) => return Err(ReadError::SectionType { name: oname, kind: ty }),
            (_, SHT_NULL) => &[],
            _ => bytes_at(o, offset, size, "section bytes")?,
        };
        let relro_at = match role {
            Role::Relro => out.relro(bytes, align, oname)?,
            _ => 0,
        };
        match role {
            Role::Code if text.is_some() => return Err(ReadError::TwoTexts { name: oname }),
            Role::Code => text = Some(i as usize),
            Role::Symtab if symtab.is_some() => return Err(ReadError::Section { name: oname }),
            Role::Symtab => symtab = Some(i as usize),
            _ => {}
        }
        sects.try_push(ESect { oname, bytes, size, link, info, role, relro_at }).map_err(|refused| ReadError::Alloc(refused.why))?;
    }
    let want = ObjName::of(&[symbol.as_bytes()]);
    let (Some(text), Some(symtab)) = (text, symtab) else {
        return Err(ReadError::NoFunction { name: want });
    };
    let strs = sects.get(sects[symtab].link as usize).ok_or(ReadError::NoSuchSection { index: u64::from(sects[symtab].link) })?.bytes;
    let e = Elf { sects: &sects, syms: sects[symtab].bytes, strs };

    // A COMDAT group is read only as the personality word's.
    for g in sects.iter().filter(|s| s.role == Role::Group) {
        for k in 1..g.bytes.len() as u64 / 4 {
            let member = e.sect(u64::from(u32_at(g.bytes, 4 * k, "group member")?))?;
            let personal = match member.role {
                Role::Personality => true,
                Role::Rela => e.sect(u64::from(member.info))?.role == Role::Personality,
                _ => false,
            };
            if !personal {
                return Err(ReadError::Section { name: g.oname });
            }
        }
    }

    // The function: the one defined global, at the text's first byte.
    let mut found = false;
    for i in 1..e.count() {
        let s = e.sym(i)?;
        if s.bind == STB_LOCAL || s.kind == STT_FILE || s.kind == STT_SECTION || s.shndx == SHN_UNDEF {
            continue;
        }
        let name = ObjName::of(&[s.name]);
        if s.name != symbol.as_bytes() {
            if s.shndx < SHN_LORESERVE && e.sect(u64::from(s.shndx))?.role == Role::Personality && s.name == ELF_PERSONALITY_REF {
                continue;
            }
            return Err(ReadError::ExtraDefinition { name });
        }
        if usize::from(s.shndx) != text {
            return Err(ReadError::NoFunction { name });
        }
        if s.value != 0 {
            return Err(ReadError::EntryNotFirst { name, at: s.value });
        }
        found = true;
    }
    if !found {
        return Err(ReadError::NoFunction { name: want });
    }
    out.e.code.try_extend_from_slice(sects[text].bytes).map_err(ReadError::Alloc)?;

    let mut eh: AVec<'a, (u64, u32, i64)> = Vec::new_in(alloc);
    let mut unwind: Option<usize> = None;
    for (index, rela) in sects.iter().enumerate() {
        match rela.role {
            Role::Unwind if unwind.is_some() => return Err(ReadError::Section { name: rela.oname }),
            Role::Unwind => unwind = Some(index),
            _ => {}
        }
        if rela.role != Role::Rela {
            continue;
        }
        let target = e.sect(u64::from(rela.info))?;
        let n = rela.size / 24;
        for i in 0..n {
            let (at, sym, ty, addend) = Elf::rela(rela, i)?;
            let refuse = ReadError::Relocation { section: target.oname, at, kind: elf_reloc_name(ty), number: ty };
            match target.role {
                Role::Code => {
                    let kind = match ty {
                        275 => RelocKind::Page21,
                        277 | 278 => RelocKind::PageOff12 { scale: 0 },
                        284 => RelocKind::PageOff12 { scale: 1 },
                        285 => RelocKind::PageOff12 { scale: 2 },
                        286 => RelocKind::PageOff12 { scale: 3 },
                        299 => RelocKind::PageOff12 { scale: 4 },
                        282 | 283 => RelocKind::Branch26,
                        311 => RelocKind::GotPage21,
                        312 => RelocKind::GotPageOff12,
                        _ => return Err(refuse),
                    };
                    bytes_at(target.bytes, at, 4, "relocated instruction")?;
                    let dest = e.dest_of_symbol(sym, at)?;
                    out.reloc(at, false, kind, dest, addend)?;
                }
                Role::Relro => {
                    if ty != R_ABS64 {
                        return Err(refuse);
                    }
                    bytes_at(target.bytes, at, 8, "relocated word")?;
                    let dest = e.dest_of_symbol(sym, at)?;
                    out.reloc(target.relro_at + at, true, RelocKind::Abs64, dest, addend)?;
                }
                Role::Unwind => {
                    if ty != R_PREL32 {
                        return Err(refuse);
                    }
                    eh.try_push((at, sym, addend)).map_err(|refused| ReadError::Alloc(refused.why))?;
                }
                Role::Lsda => return Err(ReadError::LsdaRelocation { section: target.oname }),
                Role::Personality => {
                    let s = e.sym(sym)?;
                    if ty != R_ABS64 || s.name != PERSONALITY.as_bytes() || addend != 0 {
                        return Err(ReadError::Personality { name: ObjName::of(&[s.name]) });
                    }
                }
                _ => {
                    return Err(ReadError::RelocationShape { section: target.oname, at, what: "a relocation of a section the record does not hold" });
                }
            }
        }
    }
    let (frame, lsda) = match unwind {
        Some(index) => elf_unwind(&e, &sects[index], &eh, &out.e.code[..])?,
        None => (None, None),
    };
    out.finish(frame, lsda, alloc)
}

/// The bytes of a pointer of DWARF encoding `enc` (LSB 5.0, "DWARF Exception Header Encoding").
fn enc_size(enc: u8, at: u64) -> Result<usize, ReadError> {
    match enc & 0x0f {
        0x0 | 0x4 | 0xc => Ok(8),
        0x2 | 0xa => Ok(2),
        0x3 | 0xb => Ok(4),
        _ => Err(ReadError::Cfi { at, what: "a pointer encoding this reader does not read", op: enc }),
    }
}

/// `.eh_frame`'s one FDE: its frame, checked at every call, and its LSDA.
fn elf_unwind<'o>(e: &Elf<'o, '_>, sect: &ESect<'o>, eh: &[(u64, u32, i64)], code: &[u8]) -> Result<(Option<Frame>, Option<&'o [u8]>), ReadError> {
    let bytes = sect.bytes;
    let reloc_at = |at: u64| eh.iter().find(|r| r.0 == at).map(|r| (r.1, r.2));
    // The entries: CIEs and exactly one FDE.
    let mut pos = 0u64;
    let mut fde: Option<(u64, u64, u64)> = None;
    let mut fdes = 0u64;
    while pos < bytes.len() as u64 {
        let len = u64::from(u32_at(bytes, pos, "call-frame entry")?);
        if len == 0 {
            pos += 4;
            continue;
        }
        if len == 0xffff_ffff {
            return Err(ReadError::Cfi { at: pos, what: "a 64-bit call-frame entry", op: 0 });
        }
        let end = pos + 4 + len;
        bytes_at(bytes, pos, 4 + len, "call-frame entry")?;
        let id = u64::from(u32_at(bytes, pos + 4, "call-frame entry")?);
        if id != 0 {
            fdes += 1;
            let cie = (pos + 4).checked_sub(id).ok_or(ReadError::Cfi { at: pos, what: "an FDE whose CIE is before the section", op: 0 })?;
            fde = Some((pos, end, cie));
        }
        pos = end;
    }
    if fdes > 1 {
        return Err(ReadError::UnwindEntries { count: fdes });
    }
    let Some((fde_at, fde_end, cie_at)) = fde else {
        return Ok((None, None));
    };

    // The CIE.
    let cie_len = u64::from(u32_at(bytes, cie_at, "CIE")?);
    let cie_end = cie_at + 4 + cie_len;
    let mut c = Cursor { bytes: bytes_at(bytes, 0, cie_end, "CIE")?, pos: cie_at as usize + 8, base: 0 };
    if u32_at(bytes, cie_at + 4, "CIE")? != 0 {
        return Err(ReadError::Cfi { at: cie_at, what: "an FDE's CIE pointer naming an FDE", op: 0 });
    }
    let version = c.u8()?;
    if version != 1 && version != 3 {
        return Err(ReadError::Cfi { at: cie_at, what: "a CIE version other than 1 or 3", op: version });
    }
    let aug = c.cstr()?;
    let code_align = c.uleb()?;
    let data_align = c.sleb()?;
    let ra = if version == 1 { u64::from(c.u8()?) } else { c.uleb()? };
    if ra != u64::from(LR) {
        return Err(ReadError::Cfi { at: cie_at, what: "a return address column other than x30", op: 0 });
    }
    let (mut fde_enc, mut lsda_enc, mut personality) = (0u8, None, false);
    if let Some((b'z', letters)) = aug.split_first() {
        let len = c.uleb()?;
        let end = c.pos as u64 + len;
        for &letter in letters {
            match letter {
                b'R' => fde_enc = c.u8()?,
                b'L' => lsda_enc = Some(c.u8()?),
                b'P' => {
                    let enc = c.u8()?;
                    let at = c.at();
                    c.take(enc_size(enc, at)?)?;
                    let (sym, _) = reloc_at(at).ok_or(ReadError::Cfi { at, what: "a personality pointer with no relocation", op: enc })?;
                    let s = e.sym(sym)?;
                    if s.name != ELF_PERSONALITY_REF && s.name != PERSONALITY.as_bytes() {
                        return Err(ReadError::Personality { name: ObjName::of(&[s.name]) });
                    }
                    personality = true;
                }
                _ => return Err(ReadError::Cfi { at: cie_at, what: "an augmentation letter this reader does not read", op: letter }),
            }
        }
        if c.pos as u64 != end {
            return Err(ReadError::Cfi { at: cie_at, what: "augmentation data of another length than its letters", op: 0 });
        }
    } else if !aug.is_empty() {
        return Err(ReadError::Cfi { at: cie_at, what: "an augmentation without its length ('z')", op: 0 });
    }
    let f = Factors { code_align, data_align };
    let mut initial = CfiState::START;
    let mut loc = 0u64;
    run_cfi(&mut c, f, &CfiState::START, &mut initial, &mut loc, None)?;

    // The FDE.
    let mut d = Cursor { bytes: bytes_at(bytes, 0, fde_end, "FDE")?, pos: fde_at as usize + 8, base: 0 };
    let pc_at = d.at();
    let size = enc_size(fde_enc, pc_at)?;
    d.take(size)?;
    let range = d.unsigned(size)?;
    let (sym, addend) = reloc_at(pc_at).ok_or(ReadError::Cfi { at: pc_at, what: "an FDE's start with no relocation", op: fde_enc })?;
    let code_len = code.len() as u64;
    match e.dest_of_symbol(sym, pc_at)? {
        Dest::Code(off) if (off as i64).wrapping_add(addend) == 0 && range == code_len => {}
        Dest::Code(off) => return Err(ReadError::UnwindCover { start: (off as i64).wrapping_add(addend) as u64, len: range, code: code_len }),
        _ => return Err(ReadError::Cfi { at: pc_at, what: "an FDE whose start is not in the code", op: fde_enc }),
    }
    let mut lsda = None;
    if aug.first() == Some(&b'z') {
        let len = d.uleb()?;
        let end = d.pos as u64 + len;
        if let Some(enc) = lsda_enc {
            let at = d.at();
            let raw = d.unsigned(enc_size(enc, at)?)?;
            match reloc_at(at) {
                Some((sym, addend)) => {
                    let s = e.sym(sym)?;
                    let target = e.sect(u64::from(s.shndx))?;
                    if s.shndx == SHN_UNDEF || s.shndx >= SHN_LORESERVE || target.role != Role::Lsda {
                        return Err(ReadError::LsdaAddress { at });
                    }
                    let off = (s.value as i64).checked_add(addend).filter(|&o| o >= 0).ok_or(ReadError::LsdaAddress { at })?;
                    lsda = Some(target.bytes.get(off as usize..).ok_or(ReadError::LsdaAddress { at: off as u64 })?);
                }
                None if raw == 0 => {}
                None => return Err(ReadError::Cfi { at, what: "a nonzero LSDA pointer with no relocation", op: enc }),
            }
        }
        d.pos = usize::try_from(end).map_err(|_| ReadError::Truncated { what: "FDE augmentation", at: end })?;
        if d.pos > d.bytes.len() {
            return Err(ReadError::Truncated { what: "FDE augmentation", at: end });
        }
    }
    if lsda.is_some() && !personality {
        return Err(ReadError::LsdaWithoutPersonality);
    }
    let mut calls = Calls { code, body: None };
    let mut state = initial;
    let mut loc = 0u64;
    run_cfi(&mut d, f, &initial, &mut state, &mut loc, Some(&mut calls))?;
    calls.region(loc, code_len, &state)?;
    Ok((calls.body, lsda))
}
