//! The one new code record's on-disk form, `bzcode08` (D100 point 11; D106 added the declared
//! instances, declared.rs, and the closing digest; `bzcode08` the trap rows, the parts'
//! alignments and the source-position slots, record/mod.rs): [`seal`] puts an
//! [`Emitted`] in its one canonical form and checks every rule of it once, [`encode`] writes a
//! [`Sealed`] record's bytes, [`read`] reads them in place as a [`RecordView`], checking every tag
//! and every rule again (bytes from a store or a fetch are checked once, here: rule 74), and
//! `place` (place.rs) makes a view code at an address.
//!
//! What [`seal`] SORTS (the order is canonical, not the caller's, so two builders of one record
//! write equal bytes: rule 25): the targets by (space, symbol), each kept once, every index into
//! them renumbered; the relro words by offset; the landing pads by (start, len, landing,
//! action); the unwind rows by (start, len, landing, action); the names by symbol, each once, the
//! text rebuilt in that order and a name no target uses dropped; the trap rows by offset, each
//! subject's discriminants past its count zeroed; the source-position slots by (part, offset).
//!
//! What [`seal`] REFUSES unsorted, each by its own [`FormError`] variant (piece 0's rows say the
//! caller sorts them): the relocations by (offset, part), at most one a byte; the loop headers by
//! offset; each header's live values by value; the polls by offset; the positions by offset;
//! the read list by fact, unique. Inline frames: a parent before its children.
//!
//! The ranges checked: every relocation's field inside its part (code or relro) and on its
//! alignment, its kind in the part it completes, an import-word kind never on the record's own
//! code or relro and never with an addend, an address kind never on a thread-local, an outside
//! name never with an addend; every relro word inside the relro part, 8-aligned, apart from
//! every other word and from every `Abs64` relocation; every pad and unwind row inside the code
//! (the rows also by blaze-code unwind.rs's rules); every loop header and poll in the code, every
//! header's live range inside the live list, every poll's header a loop; every position in the
//! code and naming a frame; every inline frame's parent before it; every name's text inside
//! the text and UTF-8; every outside target named; each part's alignment from its least
//! (blaze-code's `LEAST_CODE_ALIGN`, `LEAST_RELRO_ALIGN`) to a page; every trap row a whole
//! instruction of the code, one a `brk`, its held register at most `x30`; every subject row a
//! trap row's, rising; every source-position slot 4-aligned, inside its part, four zero bytes
//! there, once, and sharing no byte with a relocation's field or a relro word.
//!
//! Layout (little-endian; every section starts 8-aligned from the record's first byte): `MAGIC`;
//! `frame: u32` (the frame's size below the frame record); the unwind frame word (blaze-code
//! unwind.rs's `Frame::word`, 0 for none); nineteen `u32` counts (code bytes, relro bytes,
//! targets, relocations, words, pads, unwind rows, loop headers, live values, polls, inline
//! frames, positions, reads, names, declared instances, trap rows, subjects, source-position
//! slots, text bytes); `flags: u32` (0); `code_align: u8`, `relro_align: u8` (log2), two zero
//! bytes; a reserved `u32` (0); then the sections in that order, each padded to 8; then the
//! DIGEST: 32 bytes of BLAKE3
//! (domain [`DIGEST_DOMAIN`]) over every byte before it, so any changed byte of a shared record
//! is refused ([`FormError::Digest`]). The names' text holds the C names, then the declared
//! names, in row order, tiling it exactly. A record of another form number (`bzcode03`, or
//! blaze-code's `bzcode07`) is refused naming both numbers ([`FormError::OtherForm`]): an old
//! record is reported and rebuilt, never migrated (rule 26).
//!
//! | section | row |
//! |---|---|
//! | target (40) | space `u8`, 7 zero, symbol 32 |
//! | relocation (24) | `at: u32`, in relro `u8` (0 or 1), kind `u8`, scale `u8` (PageOff12's; else 0), 0 `u8`, `target: u32`, 0 `u32`, `addend: i64` |
//! | word (16) | `at: u32`, `target: u32`, `addend: i64` |
//! | pad (16) | `start`, `len`, `landing`, `action`, each `u32` |
//! | unwind row (16) | `start`, `len`, `landing`, the action's code, each `u32` |
//! | loop header (12) | `at`, `first`, `count`, each `u32` |
//! | live value (12) | `value: u32`, place `u8` (0 cell, 1 register, 2 slot), register `u8`, 0 `u16`, `sp_offset: u32` |
//! | poll (8) | `at: u32`, `header: u32` |
//! | inline frame (48) | `parent`, `call_lo`, `call_hi`, 0, each `u32`; instance 32 |
//! | position (16) | `at`, `frame`, `span_lo`, `span_hi`, each `u32` |
//! | read (64) | fact 32, digest 32 |
//! | name (40) | symbol 32, text `at: u32`, `len: u32` |
//! | declared instance (64) | symbol 32, `item: u128`, kind `u8`, 3 zero, name `at: u32`, `len: u32`, 4 zero (declared.rs) |
//! | trap row (32) | `at: u32`, kind `u8` (blaze-code `TrapKind`), held tag `u8` (0 unknown, 1 register, 2 stack), held register `u8` (tag 1; else 0), 0 `u8`, `site: u32` (`u32::MAX` none), held stack offset `u32` (tag 2; else 0), `item: u128` (0 with no site) |
//! | subject (88) | its trap row's `at: u32`, `count: u32`, `item: u128`, eight `u64` discriminants (past `count` 0) |
//! | source-position slot (32) | `item: u128`, `site: u32`, `at: u32`, part `u8` (0 code, 1 relro), field `u8` (0 line, 1 column), 6 zero |
//!
//! Relocation kinds by byte: Page21 0, PageOff12 1, Branch26 2, GotPage21 3, GotPageOff12 4, Abs64
//! 5, Pc32 6, GotPc32 7 (piece 0's discriminants).

use core::fmt;

use blaze_code::record::{
    DecodeError, Held, LEAST_CODE_ALIGN, LEAST_RELRO_ALIGN, MOST_PART_ALIGN, OWN_CODE, OWN_RELRO, Pad, PositionField, PositionPart,
    PositionRow, RecordError, SUBJECT_SHOWN, Space, Subject, Symbol, Target, TrapKind, TrapRow, Word,
};
use blaze_code::unwind::{Action, Frame, INST, Row, UnwindError};
use blaze_ids::Hasher;
use blaze_own::{AVec, Alloc, Vec};
use blaze_run::InstanceKey;
use blaze_run::parent::Read;

use super::declared::{self, Declared, DeclaredKind, DeclaredRow};
use super::{Emitted, InlineFrame, Live, LoopHeader, OUTERMOST, Place, Poll, Position, Reloc, RelocKind};
use crate::Reg;

/// The first eight bytes of every encoded record of this form: the ONE place its number is.
pub const MAGIC: [u8; 8] = *b"bzcode08";

/// [`MAGIC`]'s number, for [`FormError::OtherForm`].
const FORM_NUMBER: u8 = (MAGIC[6] - b'0') * 10 + (MAGIC[7] - b'0');

/// The bytes before the sections: magic, frame, unwind word, nineteen counts, flags, the
/// alignments' word, reserved (104: the first section starts 8-aligned).
const HEAD: usize = 8 + 4 + 4 + 4 * SECTIONS + 4 + 4 + 4;
const SECTIONS: usize = 19;
/// The head's offsets of the flags, the alignments' word and the reserved word.
const FLAGS_AT: usize = 16 + 4 * SECTIONS;
const ALIGNS_AT: usize = FLAGS_AT + 4;
const RESERVED_AT: usize = ALIGNS_AT + 4;

/// The digest that ends every record: its bytes and its hash domain.
const DIGEST: usize = 32;
const DIGEST_DOMAIN: &str = "blaze-tier code record bzcode08 digest";

const CODE: usize = 0;
const RELRO: usize = 1;
const TARGETS: usize = 2;
const RELOCS: usize = 3;
const WORDS: usize = 4;
const PADS: usize = 5;
const ROWS: usize = 6;
const LOOPS: usize = 7;
const LIVE: usize = 8;
const POLLS: usize = 9;
const FRAMES: usize = 10;
const POSITIONS: usize = 11;
const READS: usize = 12;
const NAMES: usize = 13;
const DECLARED: usize = 14;
const TRAPS: usize = 15;
const SUBJECTS: usize = 16;
const SLOTS: usize = 17;
const TEXT: usize = 18;

/// A trap row, a subject row and a source-position slot's row in bytes.
const TRAP_ROW: usize = 32;
const SUBJECT_ROW: usize = 24 + 8 * SUBJECT_SHOWN;
const SLOT_ROW: usize = 32;

/// A trap row's site field meaning none (its item is then 0).
const NO_SITE: u32 = u32::MAX;
/// The highest general register a [`Held::Reg`] names (`x30`).
const MOST_HELD_REG: u8 = 30;

/// Each section's row in bytes (1 for the byte sections).
const ROW: [usize; SECTIONS] =
    [1, 1, 40, 24, 16, 16, 16, 12, 12, 8, 48, 16, 64, 40, declared::ROW_BYTES, TRAP_ROW, SUBJECT_ROW, SLOT_ROW, 1];

/// Each section's name, for [`FormError::Tag`] and [`FormError::TooLarge`].
const SECTION_NAME: [&str; SECTIONS] = [
    "code",
    "relro part",
    "targets",
    "relocations",
    "relro words",
    "landing pads",
    "unwind rows",
    "loop headers",
    "live values",
    "polls",
    "inline frames",
    "positions",
    "read list",
    "names",
    "declared instances",
    "trap rows",
    "trap subjects",
    "source-position slots",
    "names' text",
];

/// Why a record was not sealed or not read: the first broken rule, naming the offset, row or
/// symbol it is about. Offsets are the record's (item-relative: rule 71).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FormError {
    /// A part (`what`) longer than a `u32` counts.
    TooLarge { what: &'static str },
    /// The bytes do not start with [`MAGIC`].
    Magic,
    /// The head's counts need `need` bytes; there are `have`.
    Length { need: u64, have: u64 },
    /// The head's flags word is not 0.
    Flags { flags: u32 },
    /// Row `row` of `section` has a tag byte no form defines.
    Tag { section: &'static str, row: u32, byte: u8 },
    /// Target `row` is not after the target before it (targets are sorted and unique).
    TargetsUnsorted { row: u32 },
    /// A relocation or word at `at` names a target index the record does not have.
    NoSuchTarget { at: u32, target: u32 },
    /// The relocation at `at` is not after the one before it by (offset, part).
    RelocsUnsorted { at: u32 },
    /// The relocation at `at` overlaps the one before it in its part.
    RelocsOverlap { at: u32 },
    /// The relocation at `at` is in the part its kind does not complete (`Abs64` in relro, every
    /// other kind in code).
    RelocPart { at: u32, kind: RelocKind },
    /// The relocation at `at` is not on its kind's alignment.
    RelocMisaligned { at: u32, kind: RelocKind },
    /// The relocation at `at` runs past its part.
    RelocOutside { at: u32, kind: RelocKind },
    /// A `PageOff12` at `at` scales by `1 << scale` past 16 bytes.
    RelocScale { at: u32, scale: u8 },
    /// An import-word relocation at `at` names the record's own code or relro (they have no
    /// import word) or carries an addend (a word is loaded whole).
    RelocImport { at: u32 },
    /// An address relocation at `at` names a thread-local, whose word holds an offset, not an
    /// address.
    RelocTlsAddress { at: u32 },
    /// A relocation or word at `at` names an outside name with an addend: `dlsym` binds a name,
    /// not a name plus an offset.
    OutsideAddend { at: u32 },
    /// A relro word at `at` is not 8-aligned.
    WordMisaligned { at: u32 },
    /// A relro word at `at` is past the relro part's `relro` bytes.
    WordOutside { at: u32, relro: u32 },
    /// Two relro words overlap at `at`.
    WordsOverlap { at: u32 },
    /// A relro word and an `Abs64` relocation overlap at `at`.
    WordRelocOverlap { at: u32 },
    /// A landing pad from `start`, or its landing, is not inside the code.
    PadOutside { start: u32 },
    /// The unwind frame or rows break blaze-code unwind.rs's rules.
    Unwind(UnwindError),
    /// The loop header at `at` is not after the one before it.
    LoopsUnsorted { at: u32 },
    /// The loop header at `at` is not inside the code.
    LoopOutside { at: u32 },
    /// Loop header `header`'s live range runs past the live values.
    LiveRange { header: u32 },
    /// Loop header `header`'s live values are not sorted by value, each once.
    LiveUnsorted { header: u32 },
    /// The poll at `at` is not after the one before it.
    PollsUnsorted { at: u32 },
    /// The poll at `at` (two instructions) is not inside the code or not on an instruction.
    PollOutside { at: u32 },
    /// The poll at `at` names loop `header`, which the record does not have.
    PollHeader { at: u32, header: u32 },
    /// Inline frame `frame` names a parent that is not before it.
    FrameParent { frame: u32, parent: u32 },
    /// Inline frame `frame`'s call span ends before it starts.
    FrameSpan { frame: u32 },
    /// The position at `at` is not after the one before it.
    PositionsUnsorted { at: u32 },
    /// The position at `at` is not inside the code.
    PositionOutside { at: u32 },
    /// The position at `at` names inline frame `frame`, which the record does not have.
    PositionFrame { at: u32, frame: u32 },
    /// The position at `at` has a span that ends before it starts.
    PositionSpan { at: u32 },
    /// Read `row` is not after the read before it by fact (the list is sorted and unique).
    ReadsUnsorted { row: u32 },
    /// Name `row` is not after the name before it by symbol.
    NamesUnsorted { row: u32 },
    /// The name of `symbol` is past the text or not UTF-8.
    NameText { symbol: Symbol },
    /// A name is kept for `symbol` and no outside target is it.
    NameUnused { symbol: Symbol },
    /// An outside target `symbol` has no name kept.
    NameMissing { symbol: Symbol },
    /// The bytes are a code record of form `bzcode{found}`; this blaze reads `bzcode{reads}`
    /// only. An old record is reported and rebuilt, never migrated (rule 26).
    OtherForm { found: u8, reads: u8 },
    /// The closing digest is not the digest of the bytes before it: a changed byte.
    Digest,
    /// The head's reserved word is not 0.
    Reserved { word: u32 },
    /// Row `row` of `section` (the C names, then the declared names) does not start where the
    /// one before it ends in the names' text, or the rows do not end at the text's end.
    TextLayout { section: &'static str, row: u32 },
    /// The declared name of `symbol` is past the text the producer gave.
    DeclaredText { symbol: Symbol },
    /// Two different declared rows have the symbol `symbol`.
    DeclaredTwice { symbol: Symbol },
    /// Declared row `row` is not after the one before it by symbol.
    DeclaredUnsorted { row: u32 },
    /// The declared row of `symbol` has an item or a name its kind does not take
    /// (declared.rs's table).
    DeclaredShape { symbol: Symbol, kind: DeclaredKind },
    /// A row declares `symbol` as a `kind`, and no target of that kind's space is it.
    DeclaredUnused { symbol: Symbol, kind: DeclaredKind },
    /// The in-program target `symbol` in `space` has no declared row.
    DeclaredMissing { symbol: Symbol, space: Space },
    /// The target `symbol` is in `space` and its declared row is a `kind` of another space.
    DeclaredSpace { symbol: Symbol, space: Space, kind: DeclaredKind },
    /// The `part`'s (`"code"` or `"relro part"`) alignment `align` (log2) is below its least
    /// (blaze-code's `LEAST_CODE_ALIGN`, `LEAST_RELRO_ALIGN`) or past a page (`MOST_PART_ALIGN`).
    Align { part: &'static str, align: u8 },
    /// The trap row at `at` is not after the one before it (sorted by offset, one a `brk`).
    TrapsUnsorted { at: u32 },
    /// The trap row's `brk` at `at` is not a whole instruction of the code.
    TrapOutside { at: u32 },
    /// The trap row at `at` names site byte `u32::MAX`, the form's word for no site.
    TrapSite { at: u32 },
    /// The trap row at `at` says register `x{reg}` holds the switched value, past `x30`.
    HeldReg { at: u32, reg: u8 },
    /// The trap row at `at`'s subject keeps a nonzero discriminant past its count (one form:
    /// rule 25).
    SubjectRest { at: u32 },
    /// Subject row `row` is not after the one before it, or no trap row is at its offset.
    SubjectRow { row: u32 },
    /// The `part`'s source-position slot at `at` is not 4-aligned.
    SlotMisaligned { part: PositionPart, at: u32 },
    /// The `part`'s four-byte source-position slot at `at` runs past the part.
    SlotOutside { part: PositionPart, at: u32 },
    /// The `part`'s source-position slot at `at` is not four zero bytes in the record.
    SlotNonzero { part: PositionPart, at: u32 },
    /// The source-position slot at (`part`, `at`) is not after the one before it (each slot once).
    SlotsUnsorted { part: PositionPart, at: u32 },
    /// The `part`'s source-position slot at `at` shares a byte with a relocation or relro word.
    SlotOverlapsReference { part: PositionPart, at: u32 },
    /// [`super::from_v1`]: the `bzcode01` record did not seal.
    V1Record(RecordError),
    /// [`super::from_v1`]: the sealed `bzcode01` record did not read back.
    V1Decode(DecodeError),
}

impl fmt::Display for FormError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rule = "(blaze-tier record/form.rs, D100 point 11)";
        match *self {
            FormError::TooLarge { what } => write!(f, "a code record's {what} is longer than a u32 counts {rule}"),
            FormError::Magic => write!(f, "the bytes are not a code record: no `bzcode` head {rule}"),
            FormError::Length { need, have } => write!(f, "a code record's head counts {need} bytes; it has {have} {rule}"),
            FormError::Flags { flags } => write!(f, "a code record's flags are {flags:#x}; this form defines none {rule}"),
            FormError::Tag { section, row, byte } => write!(f, "a code record's {section} row {row} has the unknown tag {byte} {rule}"),
            FormError::TargetsUnsorted { row } => write!(f, "a code record's target {row} is not after the one before it {rule}"),
            FormError::NoSuchTarget { at, target } => {
                write!(f, "a code record's reference at {at} names target {target}, which the record does not list {rule}")
            }
            FormError::RelocsUnsorted { at } => write!(f, "a code record's relocation at {at} is not after the one before it {rule}"),
            FormError::RelocsOverlap { at } => write!(f, "a code record's relocation at {at} overlaps the one before it {rule}"),
            FormError::RelocPart { at, kind } => write!(f, "a code record's {kind:?} relocation at {at} is in the wrong part {rule}"),
            FormError::RelocMisaligned { at, kind } => write!(f, "a code record's {kind:?} relocation at {at} is misaligned {rule}"),
            FormError::RelocOutside { at, kind } => write!(f, "a code record's {kind:?} relocation at {at} runs past its part {rule}"),
            FormError::RelocScale { at, scale } => write!(f, "a code record's page offset at {at} scales by 2^{scale}, past 16 bytes {rule}"),
            FormError::RelocImport { at } => {
                write!(f, "a code record's import-word relocation at {at} names its own code or relro, or has an addend {rule}")
            }
            FormError::RelocTlsAddress { at } => {
                write!(f, "a code record's relocation at {at} takes the address of a thread-local, whose word is an offset {rule}")
            }
            FormError::OutsideAddend { at } => {
                write!(f, "a code record's reference at {at} names an outside symbol with an addend; a name is bound by `dlsym` alone {rule}")
            }
            FormError::WordMisaligned { at } => write!(f, "a code record's relro word at {at} is not 8-aligned {rule}"),
            FormError::WordOutside { at, relro } => write!(f, "a code record's relro word at {at} is past its {relro} relro bytes {rule}"),
            FormError::WordsOverlap { at } => write!(f, "a code record has two relro words at {at} {rule}"),
            FormError::WordRelocOverlap { at } => write!(f, "a code record's relro word and Abs64 relocation overlap at {at} {rule}"),
            FormError::PadOutside { start } => write!(f, "a code record's landing pad from {start} is not inside its code {rule}"),
            FormError::Unwind(error) => write!(f, "{error}"),
            FormError::LoopsUnsorted { at } => write!(f, "a code record's loop header at {at} is not after the one before it {rule}"),
            FormError::LoopOutside { at } => write!(f, "a code record's loop header at {at} is not inside its code {rule}"),
            FormError::LiveRange { header } => write!(f, "a code record's loop header {header} has live values past the list {rule}"),
            FormError::LiveUnsorted { header } => {
                write!(f, "a code record's loop header {header} has live values not sorted by value, each once {rule}")
            }
            FormError::PollsUnsorted { at } => write!(f, "a code record's poll at {at} is not after the one before it {rule}"),
            FormError::PollOutside { at } => write!(f, "a code record's poll at {at} is not two instructions of its code {rule}"),
            FormError::PollHeader { at, header } => write!(f, "a code record's poll at {at} names loop {header}, which it does not have {rule}"),
            FormError::FrameParent { frame, parent } => {
                write!(f, "a code record's inline frame {frame} names parent {parent}, which is not before it {rule}")
            }
            FormError::FrameSpan { frame } => write!(f, "a code record's inline frame {frame} has a call span that ends before it starts {rule}"),
            FormError::PositionsUnsorted { at } => write!(f, "a code record's position at {at} is not after the one before it {rule}"),
            FormError::PositionOutside { at } => write!(f, "a code record's position at {at} is not inside its code {rule}"),
            FormError::PositionFrame { at, frame } => {
                write!(f, "a code record's position at {at} names inline frame {frame}, which it does not have {rule}")
            }
            FormError::PositionSpan { at } => write!(f, "a code record's position at {at} has a span that ends before it starts {rule}"),
            FormError::ReadsUnsorted { row } => write!(f, "a code record's read {row} is not after the one before it by fact {rule}"),
            FormError::NamesUnsorted { row } => write!(f, "a code record's name {row} is not after the one before it by symbol {rule}"),
            FormError::NameText { symbol } => write!(f, "a code record's name of {symbol} is past its text or not UTF-8 {rule}"),
            FormError::NameUnused { symbol } => write!(f, "a code record keeps a name for {symbol}, which no outside target is {rule}"),
            FormError::NameMissing { symbol } => write!(f, "a code record names the outside symbol {symbol} but keeps no name for it {rule}"),
            FormError::OtherForm { found, reads } => write!(
                f,
                "a `bzcode{found:02}` code record: this blaze reads only `bzcode{reads:02}`; an old record is reported and rebuilt, never migrated (rule 26) {rule}"
            ),
            FormError::Digest => write!(f, "a code record's closing digest does not match its bytes: a byte changed in the store or on the way {rule}"),
            FormError::Reserved { word } => write!(f, "a code record's reserved head word is {word:#x}, not 0 {rule}"),
            FormError::TextLayout { section, row } => {
                write!(f, "a code record's {section} row {row} does not start where the one before it ends in the names' text {rule}")
            }
            FormError::DeclaredText { symbol } => write!(f, "the declared name of {symbol} is past the text it was given (blaze-tier record/declared.rs, D106)"),
            FormError::DeclaredTwice { symbol } => write!(f, "a code record declares {symbol} twice, differently (blaze-tier record/declared.rs, D106)"),
            FormError::DeclaredUnsorted { row } => write!(f, "a code record's declared row {row} is not after the one before it by symbol (blaze-tier record/declared.rs, D106)"),
            FormError::DeclaredShape { symbol, kind } => {
                write!(f, "a code record declares {symbol} as a {kind} with an item or a name a {kind} does not take (blaze-tier record/declared.rs, D106)")
            }
            FormError::DeclaredUnused { symbol, kind } => write!(
                f,
                "a code record declares {symbol} as a {kind}, and no target of its code is it: a walk would follow a symbol the code never names (blaze-tier record/declared.rs, D106)"
            ),
            FormError::DeclaredMissing { symbol, space } => write!(
                f,
                "a code record's code names {symbol} ({space:?}) and its section does not declare it: a walk could not make its key (blaze-tier record/declared.rs, D106)"
            ),
            FormError::DeclaredSpace { symbol, space, kind } => {
                write!(f, "a code record's target {symbol} is in {space:?} and is declared as a {kind} (blaze-tier record/declared.rs, D106)")
            }
            FormError::Align { part, align } => write!(
                f,
                "a code record's {part} asks an alignment of 2^{align}: below its least (code 2^{LEAST_CODE_ALIGN}, relro 2^{LEAST_RELRO_ALIGN}) or past a page (2^{MOST_PART_ALIGN}) {rule}"
            ),
            FormError::TrapsUnsorted { at } => write!(f, "a code record's trap row at {at} is not after the one before it {rule}"),
            FormError::TrapOutside { at } => write!(f, "a code record's trap row at {at} is not an instruction of its code {rule}"),
            FormError::TrapSite { at } => write!(f, "a code record's trap row at {at} names site {NO_SITE}, the form's word for no site {rule}"),
            FormError::HeldReg { at, reg } => {
                write!(f, "a code record's trap row at {at} says register x{reg} holds the switched value, past x{MOST_HELD_REG} {rule}")
            }
            FormError::SubjectRest { at } => {
                write!(f, "a code record's trap row at {at} keeps a nonzero discriminant past its subject's count {rule}")
            }
            FormError::SubjectRow { row } => {
                write!(f, "a code record's subject row {row} is not after the one before it or is no trap row's {rule}")
            }
            FormError::SlotMisaligned { part, at } => write!(f, "a code record's {part:?} source-position slot at {at} is not 4-aligned {rule}"),
            FormError::SlotOutside { part, at } => write!(f, "a code record's {part:?} source-position slot at {at} runs past its part {rule}"),
            FormError::SlotNonzero { part, at } => {
                write!(f, "a code record's {part:?} source-position slot at {at} is not four zero bytes {rule}")
            }
            FormError::SlotsUnsorted { part, at } => {
                write!(f, "a code record's {part:?} source-position slot at {at} is not after the one before it {rule}")
            }
            FormError::SlotOverlapsReference { part, at } => {
                write!(f, "a code record's {part:?} source-position slot at {at} shares a byte with a relocation or relro word {rule}")
            }
            FormError::V1Record(error) => write!(f, "{error} (turning a bzcode07 record into this form)"),
            FormError::V1Decode(error) => write!(f, "{error} (turning a bzcode07 record into this form)"),
        }
    }
}

impl core::error::Error for FormError {}

/// A record in its one canonical form ([`seal`]): the only form that encodes.
pub struct Sealed<'a> {
    record: Emitted<'a>,
    /// The unwind frame's word (`Frame::word`), 0 for none: computed once by the seal.
    unwind_word: u32,
}

impl<'a> Sealed<'a> {
    pub fn record(&self) -> &Emitted<'a> {
        &self.record
    }

    pub fn into_record(self) -> Emitted<'a> {
        self.record
    }
}

/// `e` in its one canonical form: the module's documentation lists what is sorted and what is
/// refused. Every rule of the form is checked here once, so a [`Sealed`] needs no check again
/// (rule 74). The scratch lists are in the record's own allocator (the one its lists are in).
pub fn seal<'a>(e: Emitted<'a>) -> Result<Sealed<'a>, FormError> {
    let mut e = e;
    let alloc: Alloc<'a> = *e.code.allocator();
    let lens = [
        e.code.len(),
        e.relro.len(),
        e.targets.len(),
        e.relocs.len(),
        e.words.len(),
        e.pads.len(),
        e.unwind.len(),
        e.loops.len(),
        e.live.len(),
        e.polls.len(),
        e.frames.len(),
        e.positions.len(),
        e.reads.len(),
        e.names.len(),
        e.declared.len(),
        e.traps.len(),
        subjects_of(&e.traps),
        e.position_slots.len(),
        e.text.len(),
    ];
    for (section, &len) in lens.iter().enumerate() {
        if u32::try_from(len).is_err() {
            return Err(FormError::TooLarge { what: SECTION_NAME[section] });
        }
    }
    // The targets by value through an index list; equal targets become one; every index renumbered.
    let mut order: AVec<'a, u32> = Vec::with_capacity_in(e.targets.len(), alloc);
    for i in 0..e.targets.len() {
        order.push(i as u32);
    }
    order.sort_unstable_by(|&a, &b| e.targets[a as usize].cmp(&e.targets[b as usize]));
    let mut renumber: AVec<'a, u32> = Vec::from_elem_in(0, e.targets.len(), alloc);
    let mut sorted: AVec<'a, Target> = Vec::with_capacity_in(e.targets.len(), alloc);
    for &old in order.iter() {
        let t = e.targets[old as usize];
        if sorted.last() != Some(&t) {
            sorted.push(t);
        }
        renumber[old as usize] = (sorted.len() - 1) as u32;
    }
    drop(order);
    for reloc in e.relocs.iter_mut() {
        if reloc.target == OWN_CODE || reloc.target == OWN_RELRO {
            continue;
        }
        match renumber.get(reloc.target as usize) {
            Some(&new) => reloc.target = new,
            None => return Err(FormError::NoSuchTarget { at: reloc.at, target: reloc.target }),
        }
    }
    for word in e.words.iter_mut() {
        if word.target == OWN_CODE || word.target == OWN_RELRO {
            continue;
        }
        match renumber.get(word.target as usize) {
            Some(&new) => word.target = new,
            None => return Err(FormError::NoSuchTarget { at: word.at, target: word.target }),
        }
    }
    drop(renumber);
    e.targets = sorted;
    e.words.sort_unstable_by_key(|w| (w.at, w.target, w.addend));
    e.pads.sort_unstable_by_key(|p| (p.start, p.len, p.landing, p.action));
    e.unwind.sort_unstable_by_key(|r| (r.start, r.len, r.landing, r.action.code()));
    // Trap rows by offset (two at one offset are refused below); a subject's discriminants past
    // the ones it keeps are 0 (one form: rule 25). Slots by (part, offset).
    e.traps.sort_unstable_by_key(|t| t.at);
    for trap in e.traps.iter_mut() {
        if let Some(subject) = trap.subject.as_mut() {
            let kept = (subject.count as usize).min(SUBJECT_SHOWN);
            subject.discriminants[kept..].fill(0);
        }
    }
    e.position_slots.sort_unstable_by_key(|s| (s.part, s.at));
    // Names by symbol, each once, the text rebuilt in that order; a name no outside target uses
    // is dropped (the check below refuses an outside target with none).
    let mut names = core::mem::replace(&mut e.names, Vec::new_in(alloc));
    let text = core::mem::replace(&mut e.text, Vec::new_in(alloc));
    names.sort_unstable_by_key(|n| n.0);
    names.dedup_by_key(|n| n.0);
    for &(symbol, start, len) in names.iter() {
        let outside = Target { space: Space::Outside, symbol };
        if e.targets.binary_search(&outside).is_err() {
            continue;
        }
        let Some(bytes) = text.get(start as usize..start as usize + len as usize) else {
            return Err(FormError::NameText { symbol });
        };
        let at = e.text.len();
        if u32::try_from(at + bytes.len()).is_err() {
            return Err(FormError::TooLarge { what: SECTION_NAME[TEXT] });
        }
        e.text.extend_from_slice(bytes);
        e.names.push((symbol, at as u32, len));
    }
    drop(names);
    // The declared rows by symbol, equal rows merged, their names after the C names.
    declared::seal_rows(&mut e, &text, alloc)?;
    drop(text);
    let unwind_word = match e.unwind_frame {
        Some(frame) => frame.word().map_err(FormError::Unwind)?,
        None => 0,
    };
    check(&Mem(&e))?;
    Ok(Sealed { record: e, unwind_word })
}

/// `s`'s bytes, appended to `out` (the module's documentation says the layout). Equal sealed
/// records write equal bytes.
pub fn encode(s: &Sealed<'_>, out: &mut AVec<'_, u8>) {
    let r = &s.record;
    let start = out.len();
    out.extend_from_slice(&MAGIC);
    put32(out, r.frame);
    put32(out, s.unwind_word);
    let counts = [
        r.code.len(),
        r.relro.len(),
        r.targets.len(),
        r.relocs.len(),
        r.words.len(),
        r.pads.len(),
        r.unwind.len(),
        r.loops.len(),
        r.live.len(),
        r.polls.len(),
        r.frames.len(),
        r.positions.len(),
        r.reads.len(),
        r.names.len(),
        r.declared.len(),
        r.traps.len(),
        subjects_of(&r.traps),
        r.position_slots.len(),
        r.text.len(),
    ];
    for count in counts {
        put32(out, count as u32);
    }
    // The flags, the alignments' word, the reserved word.
    put32(out, 0);
    out.extend_from_slice(&[r.code_align, r.relro_align, 0, 0]);
    put32(out, 0);
    out.extend_from_slice(&r.code);
    pad8(out, start);
    out.extend_from_slice(&r.relro);
    pad8(out, start);
    for t in r.targets.iter() {
        out.push(t.space as u8);
        out.extend_from_slice(&[0; 7]);
        out.extend_from_slice(&t.symbol.0);
    }
    for x in r.relocs.iter() {
        let (kind, scale) = kind_byte(x.kind);
        put32(out, x.at);
        out.extend_from_slice(&[u8::from(x.in_relro), kind, scale, 0]);
        put32(out, x.target);
        put32(out, 0);
        out.extend_from_slice(&x.addend.to_le_bytes());
    }
    for w in r.words.iter() {
        put32(out, w.at);
        put32(out, w.target);
        out.extend_from_slice(&w.addend.to_le_bytes());
    }
    for p in r.pads.iter() {
        for v in [p.start, p.len, p.landing, p.action] {
            put32(out, v);
        }
    }
    for row in r.unwind.iter() {
        for v in [row.start, row.len, row.landing, row.action.code()] {
            put32(out, v);
        }
    }
    for h in r.loops.iter() {
        for v in [h.at, h.first, h.count] {
            put32(out, v);
        }
    }
    pad8(out, start);
    for l in r.live.iter() {
        let (tag, reg, offset) = match l.place {
            Place::Cell { sp_offset } => (0u8, 0u8, sp_offset),
            Place::Reg(reg) => (1, reg.0, 0),
            Place::Slot { sp_offset } => (2, 0, sp_offset),
        };
        put32(out, l.value);
        out.extend_from_slice(&[tag, reg, 0, 0]);
        put32(out, offset);
    }
    pad8(out, start);
    for p in r.polls.iter() {
        put32(out, p.at);
        put32(out, p.header);
    }
    for fr in r.frames.iter() {
        for v in [fr.parent, fr.call_lo, fr.call_hi, 0] {
            put32(out, v);
        }
        out.extend_from_slice(&fr.instance.0);
    }
    for p in r.positions.iter() {
        for v in [p.at, p.frame, p.span_lo, p.span_hi] {
            put32(out, v);
        }
    }
    for read in r.reads.iter() {
        out.extend_from_slice(&read.fact);
        out.extend_from_slice(&read.digest);
    }
    for &(symbol, at, len) in r.names.iter() {
        out.extend_from_slice(&symbol.0);
        put32(out, at);
        put32(out, len);
    }
    for row in r.declared.iter() {
        declared::put_row(out, row);
    }
    for t in r.traps.iter() {
        put_trap(out, t);
    }
    // The subjects, one per trap row that has one, in the rows' order (by offset).
    for t in r.traps.iter() {
        let Some(subject) = t.subject else { continue };
        put32(out, t.at);
        put32(out, subject.count);
        out.extend_from_slice(&subject.item.to_le_bytes());
        for d in subject.discriminants {
            out.extend_from_slice(&d.to_le_bytes());
        }
    }
    for s in r.position_slots.iter() {
        out.extend_from_slice(&s.item.to_le_bytes());
        put32(out, s.site);
        put32(out, s.at);
        out.push(s.part as u8);
        out.push(s.field as u8);
        out.extend_from_slice(&[0; 6]);
    }
    out.extend_from_slice(&r.text);
    pad8(out, start);
    let sum = digest(&out[start..]);
    out.extend_from_slice(&sum);
}

/// How many trap rows have a subject: the subjects section's rows.
fn subjects_of(traps: &[TrapRow]) -> usize {
    traps.iter().filter(|t| t.subject.is_some()).count()
}

/// A trap row (32): `at: u32`, kind `u8`, held tag `u8` (0 unknown, 1 a register, 2 a stack
/// slot), held register `u8`, 0 `u8`, `site: u32` ([`NO_SITE`] for none), held stack offset
/// `u32`, `item: u128` (0 for none).
fn put_trap(out: &mut AVec<'_, u8>, t: &TrapRow) {
    let (tag, reg, offset) = match t.held {
        Held::Unknown => (0u8, 0u8, 0u32),
        Held::Reg(reg) => (1, reg, 0),
        Held::Stack(offset) => (2, 0, offset),
    };
    put32(out, t.at);
    out.extend_from_slice(&[t.kind as u8, tag, reg, 0]);
    let (item, site) = t.site.unwrap_or((0, NO_SITE));
    put32(out, site);
    put32(out, offset);
    out.extend_from_slice(&item.to_le_bytes());
}

/// The trap kind a row's byte names (the encoding is `TrapKind as u8`); `None` for a byte no
/// kind has. This form's own reading of the leaf type's bytes: blaze-code's `bzcode07` reader
/// has its own until that form is retired.
fn trap_kind_of(byte: u8) -> Option<TrapKind> {
    Some(match byte {
        0 => TrapKind::MatchNoArm,
        1 => TrapKind::LetElse,
        2 => TrapKind::NeverValue,
        3 => TrapKind::AfterDivergingCall,
        4 => TrapKind::Intrinsic,
        5 => TrapKind::Discriminant,
        6 => TrapKind::SliceLength,
        7 => TrapKind::NoreturnAsm,
        8 => TrapKind::BodyEnd,
        9 => TrapKind::AfterPanic,
        _ => return None,
    })
}

/// Whether a trap row's bytes are of the one form [`put_trap`] writes: a known kind and held
/// tag, the zero byte, the register only under tag 1, the offset only under tag 2, the item 0
/// where the site is none. `Err` is the first wrong byte, for [`FormError::Tag`].
fn check_trap_bytes(b: &[u8]) -> Result<(), u8> {
    if trap_kind_of(b[4]).is_none() {
        return Err(b[4]);
    }
    let (tag, reg, zero) = (b[5], b[6], b[7]);
    if tag > 2 {
        return Err(tag);
    }
    if zero != 0 || (tag != 1 && reg != 0) {
        return Err(if zero != 0 { zero } else { reg });
    }
    if tag != 2 && get32(b, 12) != 0 {
        return Err(b[12..16].iter().copied().find(|&x| x != 0).unwrap_or(0));
    }
    if get32(b, 8) == NO_SITE && b[16..32].iter().any(|&x| x != 0) {
        return Err(b[16..32].iter().copied().find(|&x| x != 0).unwrap_or(0));
    }
    Ok(())
}

/// A trap row's bytes as the row (its subject `None`: the view adds it).
fn trap_of_bytes(b: &[u8]) -> TrapRow {
    let Some(kind) = trap_kind_of(b[4]) else {
        unreachable!("read checked every trap row's kind byte");
    };
    let held = match b[5] {
        0 => Held::Unknown,
        1 => Held::Reg(b[6]),
        _ => Held::Stack(get32(b, 12)),
    };
    let site = match get32(b, 8) {
        NO_SITE => None,
        site => Some((u128::from_le_bytes(bytes16(b, 16)), site)),
    };
    TrapRow { at: get32(b, 0), kind, site, held, subject: None }
}

/// Whether a slot row's bytes are of the one form: a known part and field, six zero bytes.
fn check_slot_bytes(b: &[u8]) -> Result<(), u8> {
    if b[24] > 1 {
        return Err(b[24]);
    }
    if b[25] > 1 {
        return Err(b[25]);
    }
    match b[26..32].iter().copied().find(|&x| x != 0) {
        Some(byte) => Err(byte),
        None => Ok(()),
    }
}

fn slot_of_bytes(b: &[u8]) -> PositionRow {
    let part = match b[24] {
        0 => PositionPart::Code,
        _ => PositionPart::Relro,
    };
    let field = match b[25] {
        0 => PositionField::Line,
        _ => PositionField::Col,
    };
    PositionRow { part, at: get32(b, 20), item: u128::from_le_bytes(bytes16(b, 0)), site: get32(b, 16), field }
}

/// The closing digest of a record's bytes before it.
fn digest(bytes: &[u8]) -> [u8; DIGEST] {
    let mut h = Hasher::new(DIGEST_DOMAIN);
    h.update(bytes);
    h.finish().0
}

/// The number of a `bzcodeNN` head; `None` for bytes that are no code record's head.
fn form_number(head: &[u8]) -> Option<u8> {
    let digit = |b: u8| b.is_ascii_digit().then(|| b - b'0');
    match head {
        [b'b', b'z', b'c', b'o', b'd', b'e', tens, ones] => Some(digit(*tens)? * 10 + digit(*ones)?),
        _ => None,
    }
}

fn put32(out: &mut AVec<'_, u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn pad8(out: &mut AVec<'_, u8>, start: usize) {
    while (out.len() - start) % 8 != 0 {
        out.push(0);
    }
}

fn up8(n: u64) -> u64 {
    (n + 7) & !7
}

fn get32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn get64(b: &[u8], at: usize) -> u64 {
    u64::from(get32(b, at)) | (u64::from(get32(b, at + 4)) << 32)
}

fn bytes32(b: &[u8], at: usize) -> [u8; 32] {
    let mut s = [0u8; 32];
    s.copy_from_slice(&b[at..at + 32]);
    s
}

fn bytes16(b: &[u8], at: usize) -> [u8; 16] {
    let mut s = [0u8; 16];
    s.copy_from_slice(&b[at..at + 16]);
    s
}

/// A kind's byte and its scale byte.
pub(super) fn kind_byte(kind: RelocKind) -> (u8, u8) {
    match kind {
        RelocKind::Page21 => (0, 0),
        RelocKind::PageOff12 { scale } => (1, scale),
        RelocKind::Branch26 => (2, 0),
        RelocKind::GotPage21 => (3, 0),
        RelocKind::GotPageOff12 => (4, 0),
        RelocKind::Abs64 => (5, 0),
        RelocKind::Pc32 => (6, 0),
        RelocKind::GotPc32 => (7, 0),
    }
}

/// The kind a kind byte and scale byte name; `None` for a byte no kind has or a scale on a kind
/// that has none.
fn kind_of(kind: u8, scale: u8) -> Option<RelocKind> {
    let kind = match kind {
        0 => RelocKind::Page21,
        1 => return Some(RelocKind::PageOff12 { scale }),
        2 => RelocKind::Branch26,
        3 => RelocKind::GotPage21,
        4 => RelocKind::GotPageOff12,
        5 => RelocKind::Abs64,
        6 => RelocKind::Pc32,
        7 => RelocKind::GotPc32,
        _ => return None,
    };
    (scale == 0).then_some(kind)
}

fn space_of(byte: u8) -> Option<Space> {
    match byte {
        0 => Some(Space::Func),
        1 => Some(Space::Static),
        2 => Some(Space::Vtable),
        3 => Some(Space::Tls),
        4 => Some(Space::Outside),
        _ => None,
    }
}

/// The bytes a relocation's kind completes.
pub(super) fn field_len(kind: RelocKind) -> u32 {
    match kind {
        RelocKind::Abs64 => 8,
        _ => 4,
    }
}

/// Whether the kind completes an import word (a GOT entry) rather than an address.
pub(super) fn is_import(kind: RelocKind) -> bool {
    matches!(kind, RelocKind::GotPage21 | RelocKind::GotPageOff12 | RelocKind::GotPc32)
}

/// An encoded record read in place ([`read`]): every tag and every rule checked once, so the
/// accessors index without checking again.
#[derive(Clone, Copy)]
pub struct RecordView<'b> {
    bytes: &'b [u8],
    frame: u32,
    unwind_frame: Option<Frame>,
    code_align: u8,
    relro_align: u8,
    /// Each section's used bytes (the byte sections without their padding).
    parts: [&'b [u8]; SECTIONS],
}

/// Reads the record at the start of `bytes`, which must be exactly one record. The form number,
/// the digest, the lengths, every tag byte and every rule [`seal`] checks are checked here, once.
pub fn read(bytes: &[u8]) -> Result<RecordView<'_>, FormError> {
    let least = (HEAD + DIGEST) as u64;
    if bytes.len() < MAGIC.len() {
        return Err(FormError::Length { need: least, have: bytes.len() as u64 });
    }
    if bytes[..8] != MAGIC {
        return Err(match form_number(&bytes[..8]) {
            Some(found) => FormError::OtherForm { found, reads: FORM_NUMBER },
            None => FormError::Magic,
        });
    }
    if (bytes.len() as u64) < least {
        return Err(FormError::Length { need: least, have: bytes.len() as u64 });
    }
    let body = bytes.len() - DIGEST;
    if digest(&bytes[..body]) != bytes[body..] {
        return Err(FormError::Digest);
    }
    let frame = get32(bytes, 8);
    let unwind_frame = Frame::of_word(get32(bytes, 12)).map_err(FormError::Unwind)?;
    let flags = get32(bytes, FLAGS_AT);
    if flags != 0 {
        return Err(FormError::Flags { flags });
    }
    // The alignments' word: code, relro, two zero bytes (their range is `check`'s rule).
    let (code_align, relro_align) = (bytes[ALIGNS_AT], bytes[ALIGNS_AT + 1]);
    let after_aligns = get32(bytes, ALIGNS_AT) & 0xFFFF_0000;
    if after_aligns != 0 {
        return Err(FormError::Reserved { word: after_aligns });
    }
    let reserved = get32(bytes, RESERVED_AT);
    if reserved != 0 {
        return Err(FormError::Reserved { word: reserved });
    }
    let mut counts = [0u64; SECTIONS];
    let mut need = least;
    for (i, count) in counts.iter_mut().enumerate() {
        *count = u64::from(get32(bytes, 16 + 4 * i));
        need += up8(*count * ROW[i] as u64);
    }
    if need != bytes.len() as u64 {
        return Err(FormError::Length { need, have: bytes.len() as u64 });
    }
    let mut parts: [&[u8]; SECTIONS] = [&[]; SECTIONS];
    let mut at = HEAD;
    for (i, part) in parts.iter_mut().enumerate() {
        let used = counts[i] as usize * ROW[i];
        *part = &bytes[at..at + used];
        at += up8(used as u64) as usize;
    }
    let tag = |section: usize, row: usize, byte: u8| FormError::Tag { section: SECTION_NAME[section], row: row as u32, byte };
    for row in 0..counts[TARGETS] as usize {
        let byte = parts[TARGETS][row * ROW[TARGETS]];
        if space_of(byte).is_none() {
            return Err(tag(TARGETS, row, byte));
        }
    }
    for row in 0..counts[RELOCS] as usize {
        let b = &parts[RELOCS][row * ROW[RELOCS]..];
        if b[4] > 1 {
            return Err(tag(RELOCS, row, b[4]));
        }
        if kind_of(b[5], b[6]).is_none() {
            return Err(tag(RELOCS, row, b[5]));
        }
    }
    for row in 0..counts[ROWS] as usize {
        let code = get32(parts[ROWS], row * ROW[ROWS] + 12);
        if Action::of_code(code).is_none() {
            return Err(tag(ROWS, row, code.min(255) as u8));
        }
    }
    for row in 0..counts[LIVE] as usize {
        let byte = parts[LIVE][row * ROW[LIVE] + 4];
        if byte > 2 {
            return Err(tag(LIVE, row, byte));
        }
    }
    for row in 0..counts[DECLARED] as usize {
        let at = row * ROW[DECLARED];
        if let Err(byte) = declared::check_row_bytes(&parts[DECLARED][at..at + ROW[DECLARED]]) {
            return Err(tag(DECLARED, row, byte));
        }
    }
    for row in 0..counts[TRAPS] as usize {
        let at = row * TRAP_ROW;
        if let Err(byte) = check_trap_bytes(&parts[TRAPS][at..at + TRAP_ROW]) {
            return Err(tag(TRAPS, row, byte));
        }
    }
    for row in 0..counts[SLOTS] as usize {
        let at = row * SLOT_ROW;
        if let Err(byte) = check_slot_bytes(&parts[SLOTS][at..at + SLOT_ROW]) {
            return Err(tag(SLOTS, row, byte));
        }
    }
    // Each subject row after the one before it and at a trap row's offset, so the view's
    // `trap` finds a row's subject by halving and none is left over. The trap rows' order is
    // `check`'s; a subject row is searched among them only after it held.
    let view = RecordView { bytes, frame, unwind_frame, code_align, relro_align, parts };
    for row in 0..view.rows_of(SUBJECTS) {
        let at = get32(view.row_of(SUBJECTS, row), 0);
        if row > 0 && get32(view.row_of(SUBJECTS, row - 1), 0) >= at {
            return Err(FormError::SubjectRow { row: row as u32 });
        }
    }
    check(&view)?;
    for row in 0..view.rows_of(SUBJECTS) {
        let at = get32(view.row_of(SUBJECTS, row), 0);
        if search(view.traps(), |i| get32(view.row_of(TRAPS, i), 0), &at).is_none() {
            return Err(FormError::SubjectRow { row: row as u32 });
        }
    }
    Ok(view)
}

impl<'b> RecordView<'b> {
    /// The whole encoding: equal records are equal bytes.
    pub fn bytes(&self) -> &'b [u8] {
        self.bytes
    }

    /// The frame's size in bytes below the frame record.
    pub fn frame(&self) -> u32 {
        self.frame
    }

    pub fn unwind_frame(&self) -> Option<Frame> {
        self.unwind_frame
    }

    pub fn code(&self) -> &'b [u8] {
        self.parts[CODE]
    }

    pub fn relro(&self) -> &'b [u8] {
        self.parts[RELRO]
    }

    fn rows_of(&self, section: usize) -> usize {
        self.parts[section].len() / ROW[section]
    }

    fn row_of(&self, section: usize, i: usize) -> &'b [u8] {
        let at = i * ROW[section];
        &self.parts[section][at..at + ROW[section]]
    }

    pub fn targets(&self) -> usize {
        self.rows_of(TARGETS)
    }

    pub fn target(&self, i: usize) -> Target {
        let b = self.row_of(TARGETS, i);
        let Some(space) = space_of(b[0]) else {
            unreachable!("read checked every target's space byte");
        };
        Target { space, symbol: Symbol(bytes32(b, 8)) }
    }

    pub fn relocs(&self) -> usize {
        self.rows_of(RELOCS)
    }

    pub fn reloc(&self, i: usize) -> Reloc {
        let b = self.row_of(RELOCS, i);
        let Some(kind) = kind_of(b[5], b[6]) else {
            unreachable!("read checked every relocation's kind and scale bytes");
        };
        Reloc { at: get32(b, 0), in_relro: b[4] == 1, kind, target: get32(b, 8), addend: get64(b, 16) as i64 }
    }

    pub fn words(&self) -> usize {
        self.rows_of(WORDS)
    }

    pub fn word(&self, i: usize) -> Word {
        let b = self.row_of(WORDS, i);
        Word { at: get32(b, 0), target: get32(b, 4), addend: get64(b, 8) as i64 }
    }

    pub fn pads(&self) -> usize {
        self.rows_of(PADS)
    }

    pub fn pad(&self, i: usize) -> Pad {
        let b = self.row_of(PADS, i);
        Pad { start: get32(b, 0), len: get32(b, 4), landing: get32(b, 8), action: get32(b, 12) }
    }

    pub fn unwind_rows(&self) -> usize {
        self.rows_of(ROWS)
    }

    pub fn unwind_row(&self, i: usize) -> Row {
        let b = self.row_of(ROWS, i);
        let Some(action) = Action::of_code(get32(b, 12)) else {
            unreachable!("read checked every unwind row's action");
        };
        Row { start: get32(b, 0), len: get32(b, 4), landing: get32(b, 8), action }
    }

    pub fn loops(&self) -> usize {
        self.rows_of(LOOPS)
    }

    pub fn loop_header(&self, i: usize) -> LoopHeader {
        let b = self.row_of(LOOPS, i);
        LoopHeader { at: get32(b, 0), first: get32(b, 4), count: get32(b, 8) }
    }

    pub fn lives(&self) -> usize {
        self.rows_of(LIVE)
    }

    pub fn live(&self, i: usize) -> Live {
        let b = self.row_of(LIVE, i);
        let place = match b[4] {
            0 => Place::Cell { sp_offset: get32(b, 8) },
            1 => Place::Reg(Reg(b[5])),
            2 => Place::Slot { sp_offset: get32(b, 8) },
            _ => unreachable!("read checked every live value's place byte"),
        };
        Live { value: get32(b, 0), place }
    }

    /// Header `header`'s live values, in place: `live[first..first + count]` as indices.
    pub fn live_of(&self, header: LoopHeader) -> core::ops::Range<usize> {
        header.first as usize..header.first as usize + header.count as usize
    }

    pub fn polls(&self) -> usize {
        self.rows_of(POLLS)
    }

    pub fn poll(&self, i: usize) -> Poll {
        let b = self.row_of(POLLS, i);
        Poll { at: get32(b, 0), header: get32(b, 4) }
    }

    pub fn inline_frames(&self) -> usize {
        self.rows_of(FRAMES)
    }

    pub fn inline_frame(&self, i: usize) -> InlineFrame {
        let b = self.row_of(FRAMES, i);
        InlineFrame { parent: get32(b, 0), call_lo: get32(b, 4), call_hi: get32(b, 8), instance: InstanceKey(bytes32(b, 16)) }
    }

    pub fn positions(&self) -> usize {
        self.rows_of(POSITIONS)
    }

    pub fn position(&self, i: usize) -> Position {
        let b = self.row_of(POSITIONS, i);
        Position { at: get32(b, 0), frame: get32(b, 4), span_lo: get32(b, 8), span_hi: get32(b, 12) }
    }

    pub fn reads(&self) -> usize {
        self.rows_of(READS)
    }

    pub fn read_row(&self, i: usize) -> Read {
        let b = self.row_of(READS, i);
        Read { fact: bytes32(b, 0), digest: bytes32(b, 32) }
    }

    pub fn names(&self) -> usize {
        self.rows_of(NAMES)
    }

    /// Name `i`: its symbol and its text's start and length in the text.
    pub fn name_row(&self, i: usize) -> (Symbol, u32, u32) {
        let b = self.row_of(NAMES, i);
        (Symbol(bytes32(b, 0)), get32(b, 32), get32(b, 36))
    }

    /// The name kept for an outside target's symbol, by halving over the sorted names.
    pub fn name(&self, symbol: Symbol) -> Option<&'b str> {
        let i = search(self.names(), |i| self.name_row(i).0, &symbol)?;
        let (_, at, len) = self.name_row(i);
        core::str::from_utf8(self.parts[TEXT].get(at as usize..at as usize + len as usize)?).ok()
    }

    /// The count of declared instances (D106: what a walk follows from this record).
    pub fn declared_count(&self) -> usize {
        self.rows_of(DECLARED)
    }

    /// Declared row `i`, its name borrowed from the record's bytes.
    pub fn declared(&self, i: usize) -> Declared<'b> {
        declared::view_of(self.declared_raw(i), self.parts[TEXT])
    }

    /// The declared row of `symbol` (a target's), by halving over the sorted rows.
    pub fn declared_of(&self, symbol: Symbol) -> Option<Declared<'b>> {
        let i = search(self.declared_count(), |i| self.declared_raw(i).symbol, &symbol)?;
        Some(self.declared(i))
    }

    fn declared_raw(&self, i: usize) -> DeclaredRow {
        declared::row_of_bytes(self.row_of(DECLARED, i))
    }

    /// log2 of the alignment the code's first byte must be placed at ([`Emitted::code_align`]).
    pub fn code_align(&self) -> u8 {
        self.code_align
    }

    /// log2 of the alignment the relro part's first byte must be placed at.
    pub fn relro_align(&self) -> u8 {
        self.relro_align
    }

    pub fn traps(&self) -> usize {
        self.rows_of(TRAPS)
    }

    /// Trap row `i` (by offset), with its subject.
    pub fn trap(&self, i: usize) -> TrapRow {
        let mut row = trap_of_bytes(self.row_of(TRAPS, i));
        let found = search(self.rows_of(SUBJECTS), |k| get32(self.row_of(SUBJECTS, k), 0), &row.at);
        row.subject = found.map(|k| {
            let b = self.row_of(SUBJECTS, k);
            let mut discriminants = [0u64; SUBJECT_SHOWN];
            for (n, d) in discriminants.iter_mut().enumerate() {
                *d = get64(b, 24 + 8 * n);
            }
            Subject { item: u128::from_le_bytes(bytes16(b, 8)), count: get32(b, 4), discriminants }
        });
        row
    }

    /// The trap row of the `brk` at code offset `at`, by halving: what a fault at that
    /// instruction ended (blaze-code trap.rs's question of a record).
    pub fn trap_at(&self, at: u32) -> Option<TrapRow> {
        let i = search(self.traps(), |i| get32(self.row_of(TRAPS, i), 0), &at)?;
        Some(self.trap(i))
    }

    /// The count of source-position slots: the placer's `values` hold one `u32` for each.
    pub fn position_slots(&self) -> usize {
        self.rows_of(SLOTS)
    }

    /// Source-position slot `i`, in (part, offset) order: the order of the placer's `values`.
    pub fn position_slot(&self, i: usize) -> PositionRow {
        slot_of_bytes(self.row_of(SLOTS, i))
    }

    /// The record back in memory, in `alloc`: what was sealed, so sealing it again encodes to
    /// these bytes.
    pub fn to_emitted<'a>(&self, alloc: Alloc<'a>) -> Emitted<'a> {
        let mut e = Emitted::new_in(alloc);
        e.code.extend_from_slice(self.code());
        e.relro.extend_from_slice(self.relro());
        e.frame = self.frame;
        e.unwind_frame = self.unwind_frame;
        for i in 0..self.targets() {
            e.targets.push(self.target(i));
        }
        for i in 0..self.relocs() {
            e.relocs.push(self.reloc(i));
        }
        for i in 0..self.words() {
            e.words.push(self.word(i));
        }
        for i in 0..self.pads() {
            e.pads.push(self.pad(i));
        }
        for i in 0..self.unwind_rows() {
            e.unwind.push(self.unwind_row(i));
        }
        for i in 0..self.loops() {
            e.loops.push(self.loop_header(i));
        }
        for i in 0..self.lives() {
            e.live.push(self.live(i));
        }
        for i in 0..self.polls() {
            e.polls.push(self.poll(i));
        }
        for i in 0..self.inline_frames() {
            e.frames.push(self.inline_frame(i));
        }
        for i in 0..self.positions() {
            e.positions.push(self.position(i));
        }
        for i in 0..self.reads() {
            e.reads.push(self.read_row(i));
        }
        for i in 0..self.names() {
            e.names.push(self.name_row(i));
        }
        for i in 0..self.declared_count() {
            e.declared.push(self.declared_raw(i));
        }
        e.text.extend_from_slice(self.parts[TEXT]);
        e.code_align = self.code_align;
        e.relro_align = self.relro_align;
        for i in 0..self.traps() {
            e.traps.push(self.trap(i));
        }
        for i in 0..self.position_slots() {
            e.position_slots.push(self.position_slot(i));
        }
        e
    }
}

/// The index of `key` among `n` sorted values `get(0..n)`.
fn search<T: Ord>(n: usize, get: impl Fn(usize) -> T, key: &T) -> Option<usize> {
    let (mut lo, mut hi) = (0usize, n);
    while lo < hi {
        let mid = (lo + hi) / 2;
        match get(mid).cmp(key) {
            core::cmp::Ordering::Less => lo = mid + 1,
            core::cmp::Ordering::Greater => hi = mid,
            core::cmp::Ordering::Equal => return Some(mid),
        }
    }
    None
}

/// A record's rows as the form's rules read them: a sealed record in memory ([`Mem`]) and an
/// encoded one ([`RecordView`]) are checked by the one [`check`].
trait Rows {
    fn code_len(&self) -> u32;
    fn relro_len(&self) -> u32;
    fn frame_of_unwind(&self) -> Option<Frame>;
    fn targets(&self) -> usize;
    fn target(&self, i: usize) -> Target;
    fn relocs(&self) -> usize;
    fn reloc(&self, i: usize) -> Reloc;
    fn words(&self) -> usize;
    fn word(&self, i: usize) -> Word;
    fn pads(&self) -> usize;
    fn pad(&self, i: usize) -> Pad;
    fn unwind_rows(&self) -> usize;
    fn unwind_row(&self, i: usize) -> Row;
    fn loops(&self) -> usize;
    fn loop_header(&self, i: usize) -> LoopHeader;
    fn lives(&self) -> usize;
    fn live(&self, i: usize) -> Live;
    fn polls(&self) -> usize;
    fn poll(&self, i: usize) -> Poll;
    fn inline_frames(&self) -> usize;
    fn inline_frame(&self, i: usize) -> InlineFrame;
    fn positions(&self) -> usize;
    fn position(&self, i: usize) -> Position;
    fn reads(&self) -> usize;
    fn read_row(&self, i: usize) -> Read;
    fn names(&self) -> usize;
    fn name_row(&self, i: usize) -> (Symbol, u32, u32);
    fn declared_rows(&self) -> usize;
    fn declared_row(&self, i: usize) -> DeclaredRow;
    fn text(&self) -> &[u8];
    /// (code, relro) alignments, log2.
    fn aligns(&self) -> (u8, u8);
    fn traps(&self) -> usize;
    fn trap(&self, i: usize) -> TrapRow;
    fn slots(&self) -> usize;
    fn slot(&self, i: usize) -> PositionRow;
    fn code_bytes(&self) -> &[u8];
    fn relro_bytes(&self) -> &[u8];
}

/// A record in memory after the seal's sorting; every length fits a `u32` (checked first).
struct Mem<'r, 'a>(&'r Emitted<'a>);

impl Rows for Mem<'_, '_> {
    fn code_len(&self) -> u32 {
        self.0.code.len() as u32
    }
    fn relro_len(&self) -> u32 {
        self.0.relro.len() as u32
    }
    fn frame_of_unwind(&self) -> Option<Frame> {
        self.0.unwind_frame
    }
    fn targets(&self) -> usize {
        self.0.targets.len()
    }
    fn target(&self, i: usize) -> Target {
        self.0.targets[i]
    }
    fn relocs(&self) -> usize {
        self.0.relocs.len()
    }
    fn reloc(&self, i: usize) -> Reloc {
        self.0.relocs[i]
    }
    fn words(&self) -> usize {
        self.0.words.len()
    }
    fn word(&self, i: usize) -> Word {
        self.0.words[i]
    }
    fn pads(&self) -> usize {
        self.0.pads.len()
    }
    fn pad(&self, i: usize) -> Pad {
        self.0.pads[i]
    }
    fn unwind_rows(&self) -> usize {
        self.0.unwind.len()
    }
    fn unwind_row(&self, i: usize) -> Row {
        self.0.unwind[i]
    }
    fn loops(&self) -> usize {
        self.0.loops.len()
    }
    fn loop_header(&self, i: usize) -> LoopHeader {
        self.0.loops[i]
    }
    fn lives(&self) -> usize {
        self.0.live.len()
    }
    fn live(&self, i: usize) -> Live {
        self.0.live[i]
    }
    fn polls(&self) -> usize {
        self.0.polls.len()
    }
    fn poll(&self, i: usize) -> Poll {
        self.0.polls[i]
    }
    fn inline_frames(&self) -> usize {
        self.0.frames.len()
    }
    fn inline_frame(&self, i: usize) -> InlineFrame {
        self.0.frames[i]
    }
    fn positions(&self) -> usize {
        self.0.positions.len()
    }
    fn position(&self, i: usize) -> Position {
        self.0.positions[i]
    }
    fn reads(&self) -> usize {
        self.0.reads.len()
    }
    fn read_row(&self, i: usize) -> Read {
        self.0.reads[i]
    }
    fn names(&self) -> usize {
        self.0.names.len()
    }
    fn name_row(&self, i: usize) -> (Symbol, u32, u32) {
        self.0.names[i]
    }
    fn declared_rows(&self) -> usize {
        self.0.declared.len()
    }
    fn declared_row(&self, i: usize) -> DeclaredRow {
        self.0.declared[i]
    }
    fn text(&self) -> &[u8] {
        &self.0.text
    }
    fn aligns(&self) -> (u8, u8) {
        (self.0.code_align, self.0.relro_align)
    }
    fn traps(&self) -> usize {
        self.0.traps.len()
    }
    fn trap(&self, i: usize) -> TrapRow {
        self.0.traps[i]
    }
    fn slots(&self) -> usize {
        self.0.position_slots.len()
    }
    fn slot(&self, i: usize) -> PositionRow {
        self.0.position_slots[i]
    }
    fn code_bytes(&self) -> &[u8] {
        &self.0.code
    }
    fn relro_bytes(&self) -> &[u8] {
        &self.0.relro
    }
}

impl Rows for RecordView<'_> {
    fn code_len(&self) -> u32 {
        self.parts[CODE].len() as u32
    }
    fn relro_len(&self) -> u32 {
        self.parts[RELRO].len() as u32
    }
    fn frame_of_unwind(&self) -> Option<Frame> {
        self.unwind_frame
    }
    fn targets(&self) -> usize {
        RecordView::targets(self)
    }
    fn target(&self, i: usize) -> Target {
        RecordView::target(self, i)
    }
    fn relocs(&self) -> usize {
        RecordView::relocs(self)
    }
    fn reloc(&self, i: usize) -> Reloc {
        RecordView::reloc(self, i)
    }
    fn words(&self) -> usize {
        RecordView::words(self)
    }
    fn word(&self, i: usize) -> Word {
        RecordView::word(self, i)
    }
    fn pads(&self) -> usize {
        RecordView::pads(self)
    }
    fn pad(&self, i: usize) -> Pad {
        RecordView::pad(self, i)
    }
    fn unwind_rows(&self) -> usize {
        RecordView::unwind_rows(self)
    }
    fn unwind_row(&self, i: usize) -> Row {
        RecordView::unwind_row(self, i)
    }
    fn loops(&self) -> usize {
        RecordView::loops(self)
    }
    fn loop_header(&self, i: usize) -> LoopHeader {
        RecordView::loop_header(self, i)
    }
    fn lives(&self) -> usize {
        RecordView::lives(self)
    }
    fn live(&self, i: usize) -> Live {
        RecordView::live(self, i)
    }
    fn polls(&self) -> usize {
        RecordView::polls(self)
    }
    fn poll(&self, i: usize) -> Poll {
        RecordView::poll(self, i)
    }
    fn inline_frames(&self) -> usize {
        RecordView::inline_frames(self)
    }
    fn inline_frame(&self, i: usize) -> InlineFrame {
        RecordView::inline_frame(self, i)
    }
    fn positions(&self) -> usize {
        RecordView::positions(self)
    }
    fn position(&self, i: usize) -> Position {
        RecordView::position(self, i)
    }
    fn reads(&self) -> usize {
        RecordView::reads(self)
    }
    fn read_row(&self, i: usize) -> Read {
        RecordView::read_row(self, i)
    }
    fn names(&self) -> usize {
        RecordView::names(self)
    }
    fn name_row(&self, i: usize) -> (Symbol, u32, u32) {
        RecordView::name_row(self, i)
    }
    fn declared_rows(&self) -> usize {
        self.declared_count()
    }
    fn declared_row(&self, i: usize) -> DeclaredRow {
        self.declared_raw(i)
    }
    fn text(&self) -> &[u8] {
        self.parts[TEXT]
    }
    fn aligns(&self) -> (u8, u8) {
        (self.code_align, self.relro_align)
    }
    fn traps(&self) -> usize {
        RecordView::traps(self)
    }
    fn trap(&self, i: usize) -> TrapRow {
        RecordView::trap(self, i)
    }
    fn slots(&self) -> usize {
        self.position_slots()
    }
    fn slot(&self, i: usize) -> PositionRow {
        self.position_slot(i)
    }
    fn code_bytes(&self) -> &[u8] {
        self.parts[CODE]
    }
    fn relro_bytes(&self) -> &[u8] {
        self.parts[RELRO]
    }
}

/// Every rule of the form (the module's documentation lists them), once, in one place: the seal
/// runs it on the sorted record, [`read`] on the bytes.
fn check(r: &impl Rows) -> Result<(), FormError> {
    let code = r.code_len();
    let relro = r.relro_len();
    for i in 1..r.targets() {
        if r.target(i - 1) >= r.target(i) {
            return Err(FormError::TargetsUnsorted { row: i as u32 });
        }
    }
    let target_of = |at: u32, index: u32| -> Result<Target, FormError> {
        match (index as usize) < r.targets() {
            true => Ok(r.target(index as usize)),
            false => Err(FormError::NoSuchTarget { at, target: index }),
        }
    };
    // Relocations: sorted by (offset, part), each field inside its part, apart from the one
    // before it in that part.
    let mut prev: Option<(u32, bool)> = None;
    let mut ends = [0u64; 2];
    for i in 0..r.relocs() {
        let x = r.reloc(i);
        let key = (x.at, x.in_relro);
        if prev.is_some_and(|p| p >= key) {
            return Err(FormError::RelocsUnsorted { at: x.at });
        }
        prev = Some(key);
        if x.in_relro != (x.kind == RelocKind::Abs64) {
            return Err(FormError::RelocPart { at: x.at, kind: x.kind });
        }
        if let RelocKind::PageOff12 { scale } = x.kind
            && scale > 4
        {
            return Err(FormError::RelocScale { at: x.at, scale });
        }
        let align = match x.kind {
            RelocKind::Abs64 => 8,
            RelocKind::Pc32 | RelocKind::GotPc32 => 1,
            _ => INST,
        };
        if x.at % align != 0 {
            return Err(FormError::RelocMisaligned { at: x.at, kind: x.kind });
        }
        let end = u64::from(x.at) + u64::from(field_len(x.kind));
        let part = if x.in_relro { relro } else { code };
        if end > u64::from(part) {
            return Err(FormError::RelocOutside { at: x.at, kind: x.kind });
        }
        let slot = usize::from(x.in_relro);
        if u64::from(x.at) < ends[slot] {
            return Err(FormError::RelocsOverlap { at: x.at });
        }
        ends[slot] = end;
        let import = is_import(x.kind);
        match x.target {
            OWN_CODE | OWN_RELRO => {
                if import {
                    return Err(FormError::RelocImport { at: x.at });
                }
            }
            index => {
                let target = target_of(x.at, index)?;
                if import && x.addend != 0 {
                    return Err(FormError::RelocImport { at: x.at });
                }
                if target.space == Space::Tls && !import && x.kind != RelocKind::Abs64 {
                    return Err(FormError::RelocTlsAddress { at: x.at });
                }
                if target.space == Space::Outside && x.addend != 0 {
                    return Err(FormError::OutsideAddend { at: x.at });
                }
            }
        }
    }
    // Relro words: sorted, 8-aligned, inside, apart.
    let mut next = 0u64;
    for i in 0..r.words() {
        let w = r.word(i);
        if w.at % 8 != 0 {
            return Err(FormError::WordMisaligned { at: w.at });
        }
        if u64::from(w.at) < next {
            return Err(FormError::WordsOverlap { at: w.at });
        }
        next = u64::from(w.at) + 8;
        if next > u64::from(relro) {
            return Err(FormError::WordOutside { at: w.at, relro });
        }
        if w.target != OWN_CODE && w.target != OWN_RELRO {
            let target = target_of(w.at, w.target)?;
            if target.space == Space::Outside && w.addend != 0 {
                return Err(FormError::OutsideAddend { at: w.at });
            }
        }
    }
    // A relro word and an Abs64 relocation never share a byte: both lists are sorted by offset.
    let mut w = 0usize;
    for i in 0..r.relocs() {
        let x = r.reloc(i);
        if !x.in_relro {
            continue;
        }
        let (lo, hi) = (u64::from(x.at), u64::from(x.at) + u64::from(field_len(x.kind)));
        while w < r.words() && u64::from(r.word(w).at) + 8 <= lo {
            w += 1;
        }
        if w < r.words() && u64::from(r.word(w).at) < hi {
            return Err(FormError::WordRelocOverlap { at: x.at });
        }
    }
    for i in 0..r.pads() {
        let pad = r.pad(i);
        let past = u64::from(pad.start) + u64::from(pad.len);
        if past > u64::from(code) || pad.landing >= code {
            return Err(FormError::PadOutside { start: pad.start });
        }
    }
    check_unwind(r, code)?;
    // The loop map.
    for i in 0..r.loops() {
        let h = r.loop_header(i);
        if i > 0 && r.loop_header(i - 1).at >= h.at {
            return Err(FormError::LoopsUnsorted { at: h.at });
        }
        if h.at >= code || h.at % INST != 0 {
            return Err(FormError::LoopOutside { at: h.at });
        }
        let from = h.first as u64;
        let to = from + u64::from(h.count);
        if to > r.lives() as u64 {
            return Err(FormError::LiveRange { header: i as u32 });
        }
        for k in from as usize + 1..to as usize {
            if r.live(k - 1).value >= r.live(k).value {
                return Err(FormError::LiveUnsorted { header: i as u32 });
            }
        }
    }
    for i in 0..r.polls() {
        let p = r.poll(i);
        if i > 0 && r.poll(i - 1).at >= p.at {
            return Err(FormError::PollsUnsorted { at: p.at });
        }
        if p.at % INST != 0 || u64::from(p.at) + 2 * u64::from(INST) > u64::from(code) {
            return Err(FormError::PollOutside { at: p.at });
        }
        if p.header as usize >= r.loops() {
            return Err(FormError::PollHeader { at: p.at, header: p.header });
        }
    }
    // The position map.
    for i in 0..r.inline_frames() {
        let fr = r.inline_frame(i);
        if fr.parent != OUTERMOST && fr.parent as usize >= i {
            return Err(FormError::FrameParent { frame: i as u32, parent: fr.parent });
        }
        if fr.call_lo > fr.call_hi {
            return Err(FormError::FrameSpan { frame: i as u32 });
        }
    }
    for i in 0..r.positions() {
        let p = r.position(i);
        if i > 0 && r.position(i - 1).at >= p.at {
            return Err(FormError::PositionsUnsorted { at: p.at });
        }
        if p.at >= code {
            return Err(FormError::PositionOutside { at: p.at });
        }
        if p.frame != OUTERMOST && p.frame as usize >= r.inline_frames() {
            return Err(FormError::PositionFrame { at: p.at, frame: p.frame });
        }
        if p.span_lo > p.span_hi {
            return Err(FormError::PositionSpan { at: p.at });
        }
    }
    for i in 1..r.reads() {
        if r.read_row(i - 1).fact >= r.read_row(i).fact {
            return Err(FormError::ReadsUnsorted { row: i as u32 });
        }
    }
    let (code_align, relro_align) = r.aligns();
    if !(LEAST_CODE_ALIGN..=MOST_PART_ALIGN).contains(&code_align) {
        return Err(FormError::Align { part: "code", align: code_align });
    }
    if !(LEAST_RELRO_ALIGN..=MOST_PART_ALIGN).contains(&relro_align) {
        return Err(FormError::Align { part: "relro part", align: relro_align });
    }
    check_traps(r, code)?;
    check_slots(r)?;
    check_names(r)?;
    declared::check(
        r.declared_rows(),
        |i| r.declared_row(i),
        r.targets(),
        |i| r.target(i),
        r.names(),
        |i| r.name_row(i),
        r.text().len(),
    )
}

/// The unwind frame and rows by blaze-code unwind.rs's rules (its `read`, on this form's rows).
fn check_unwind(r: &impl Rows, code: u32) -> Result<(), FormError> {
    let rows = r.unwind_rows();
    let Some(frame) = r.frame_of_unwind() else {
        return match rows {
            0 => Ok(()),
            n => Err(FormError::Unwind(UnwindError::PadsWithoutFrame { pads: n as u32 })),
        };
    };
    let prologue = frame.prologue();
    let mut end = 0u64;
    for i in 0..rows {
        let row = r.unwind_row(i);
        if row.len == 0 || row.start % INST != 0 || row.len % INST != 0 {
            return Err(FormError::Unwind(UnwindError::PadShape { start: row.start, len: row.len }));
        }
        if row.start < prologue {
            return Err(FormError::Unwind(UnwindError::PadInPrologue { start: row.start, prologue }));
        }
        let past = u64::from(row.start) + u64::from(row.len);
        if past > u64::from(code) {
            return Err(FormError::Unwind(UnwindError::PadOutside { start: row.start, len: row.len, code }));
        }
        if u64::from(row.start) < end {
            return Err(FormError::Unwind(UnwindError::PadsOverlap { start: row.start }));
        }
        end = past;
        let landing_ok = match row.action {
            Action::Continue => row.landing == 0,
            _ => row.landing >= prologue && row.landing % INST == 0 && row.landing < code,
        };
        if !landing_ok {
            return Err(FormError::Unwind(UnwindError::PadLanding { start: row.start, landing: row.landing, action: row.action }));
        }
    }
    Ok(())
}

/// Trap rows: by offset, one a `brk`, each a whole instruction of the code, no site the form's
/// word for none, no held register past `x30`, no subject discriminant kept past its count.
fn check_traps(r: &impl Rows, code: u32) -> Result<(), FormError> {
    for i in 0..r.traps() {
        let t = r.trap(i);
        if i > 0 && r.trap(i - 1).at >= t.at {
            return Err(FormError::TrapsUnsorted { at: t.at });
        }
        if t.at % INST != 0 || u64::from(t.at) + u64::from(INST) > u64::from(code) {
            return Err(FormError::TrapOutside { at: t.at });
        }
        if let Some((_, NO_SITE)) = t.site {
            return Err(FormError::TrapSite { at: t.at });
        }
        if let Held::Reg(reg) = t.held
            && reg > MOST_HELD_REG
        {
            return Err(FormError::HeldReg { at: t.at, reg });
        }
        if let Some(subject) = t.subject {
            let kept = (subject.count as usize).min(SUBJECT_SHOWN);
            if subject.discriminants[kept..].iter().any(|&d| d != 0) {
                return Err(FormError::SubjectRest { at: t.at });
            }
        }
    }
    Ok(())
}

/// Source-position slots: by (part, offset), each once, 4-aligned, inside its part, four zero
/// bytes there, and sharing no byte with a relocation's field or a relro word (the placer
/// writes both; neither may overwrite the other).
fn check_slots(r: &impl Rows) -> Result<(), FormError> {
    let slots = r.slots();
    for i in 0..slots {
        let s = r.slot(i);
        let (part, at) = (s.part, s.at);
        if i > 0 {
            let prev = r.slot(i - 1);
            if (prev.part, prev.at) >= (part, at) {
                return Err(FormError::SlotsUnsorted { part, at });
            }
        }
        if at % 4 != 0 {
            return Err(FormError::SlotMisaligned { part, at });
        }
        let bytes = match part {
            PositionPart::Code => r.code_bytes(),
            PositionPart::Relro => r.relro_bytes(),
        };
        match bytes.get(at as usize..).and_then(|tail| tail.get(..4)) {
            None => return Err(FormError::SlotOutside { part, at }),
            Some(slot) if slot != [0; 4] => return Err(FormError::SlotNonzero { part, at }),
            Some(_) => {}
        }
    }
    if slots == 0 {
        return Ok(());
    }
    // The first slot at or after the 4-byte slot holding a reference's first byte is the only
    // one it can share a byte with (the slots are sorted, 4 bytes, 4-aligned, apart).
    let overlap = |part: PositionPart, at: u32, len: u32| -> Result<(), FormError> {
        let key = (part, at & !3);
        let (mut lo, mut hi) = (0usize, slots);
        while lo < hi {
            let mid = (lo + hi) / 2;
            let s = r.slot(mid);
            match (s.part, s.at) < key {
                true => lo = mid + 1,
                false => hi = mid,
            }
        }
        if lo < slots {
            let s = r.slot(lo);
            if s.part == part && u64::from(s.at) < u64::from(at) + u64::from(len) {
                return Err(FormError::SlotOverlapsReference { part, at: s.at });
            }
        }
        Ok(())
    };
    for i in 0..r.relocs() {
        let x = r.reloc(i);
        let part = if x.in_relro { PositionPart::Relro } else { PositionPart::Code };
        overlap(part, x.at, field_len(x.kind))?;
    }
    for i in 0..r.words() {
        overlap(PositionPart::Relro, r.word(i).at, 8)?;
    }
    Ok(())
}

/// Names: sorted by symbol and unique, each inside the text and UTF-8, each an outside target's,
/// and every outside target named.
fn check_names(r: &impl Rows) -> Result<(), FormError> {
    let text = r.text();
    for i in 0..r.names() {
        let (symbol, at, len) = r.name_row(i);
        if i > 0 && r.name_row(i - 1).0 >= symbol {
            return Err(FormError::NamesUnsorted { row: i as u32 });
        }
        let bytes = text.get(at as usize..at as usize + len as usize).ok_or(FormError::NameText { symbol })?;
        if core::str::from_utf8(bytes).is_err() {
            return Err(FormError::NameText { symbol });
        }
        if search(r.targets(), |t| r.target(t), &Target { space: Space::Outside, symbol }).is_none() {
            return Err(FormError::NameUnused { symbol });
        }
    }
    for t in 0..r.targets() {
        let target = r.target(t);
        if target.space == Space::Outside && search(r.names(), |n| r.name_row(n).0, &target.symbol).is_none() {
            return Err(FormError::NameMissing { symbol: target.symbol });
        }
    }
    Ok(())
}
