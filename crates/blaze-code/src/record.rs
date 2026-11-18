//! One function instance's code record (run.md, "End state", "The code record"): the value
//! blaze-keyed's `CodeRecord` (tag 27) holds.
//!
//! - `code`: AArch64 machine code and its pure read-only data, position-independent. It holds no
//!   address: every reference outside the record is a [`Site`] that loads an import word, and
//!   every address in data is a [`Word`] of the `relro` part.
//! - `targets`: what the sites and words name, each a [`Target`] (a space and a symbol), sorted
//!   and unique: never an address, a slot number or a path.
//! - `frame`: the function's compact unwind word (the emitter's encoding, opaque here) and
//!   `pads`: its landing pads, item-relative.
//! - Outside names (the C library's) are kept as text by symbol, for the launcher's `dlsym`.
//! - `positions`: the item-relative source of each zero line or column slot in code or relro.
//!   Placement fills the slot from the item's current source.
//! - `traps`: per `brk` of an `unreachable` the code holds, WHICH construct it ends ([`TrapRow`]:
//!   its kind, and the construct's first byte counted from its item's first byte, rule 71):
//!   what `blaze run` says when the program strikes one (blaze-cli run_fault.rs). Data only:
//!   no instruction reads it, and the key is the function's, unchanged by it.
//!
//! [`CodeRecord`] is the builder; [`CodeRecord::seal`] checks it and puts it in its one canonical
//! form ([`Sealed`]), which alone encodes, so two builders of one record write equal bytes
//! whatever order they added things in (rule 25). [`CodeView`] reads an encoded record in place.

use core::fmt;

use blaze_ids::Hasher;
use blaze_own::{AVec, Alloc, Vec};

/// The first eight bytes of every encoded record: the form's version. `bzcode04` added the trap
/// table ([`TrapRow`]), `bzcode05` its held values ([`Held`]) and subjects ([`Subject`]),
/// `bzcode06` the alignment each part asks of its placement ([`CodeRecord::code_align`],
/// [`CodeRecord::relro_align`]: data aligned past 16, as `vzipq_u8`'s 64-aligned table).
/// `bzcode07` adds item-relative source positions ([`PositionRow`]). `02`
/// and `03` are blaze-tier's own forms (record/form.rs), so this form skips them and a record
/// of one form is never read as another's. A record of an older form of this one is refused
/// naming its form ([`DecodeError::Older`]) and rebuilt, never read (rule 26).
pub const MAGIC: [u8; 8] = *b"bzcode07";

/// The forms before [`MAGIC`] that were this record's: refused by name, never read.
const OLDER: [[u8; 8]; 3] = [*b"bzcode04", *b"bzcode05", *b"bzcode06"];

/// The least alignment, as log2, of a record's code: 16 bytes, what every placement gives
/// (image.rs `lay_out`, blaze-launch-rt run_mode.rs) and what a 16-aligned data row of the code
/// needed before the field existed.
pub const LEAST_CODE_ALIGN: u8 = 4;
/// The least alignment, as log2, of a record's relro part: 8 bytes, its words'.
pub const LEAST_RELRO_ALIGN: u8 = 3;
/// The most alignment, as log2, a part may ask: a 4 KiB page, the least page a target has
/// (image.rs `lay_out` starts every segment on a page of at least 4096 bytes), so any
/// segment layout can give it; blaze-code datum.rs `MOST_ALIGN` is the same bound for statics.
pub const MOST_PART_ALIGN: u8 = 12;

/// The bytes before the sections: magic, frame, the two parts' alignments and two zero bytes,
/// eleven counts and four zero bytes (so the first section starts 8-aligned).
const HEAD: usize = 64;

/// A [`Word`]'s target meaning this record's own code: the word holds the address of code byte
/// `addend`.
pub const OWN_CODE: u32 = u32::MAX;
/// A [`Word`]'s target meaning this record's own relro part: the address of relro byte `addend`.
pub const OWN_RELRO: u32 = u32::MAX - 1;
/// A [`SiteKind::Relro`] site's target field: it names no target.
pub const NO_TARGET: u32 = u32::MAX;

const NAME_DOMAIN: &str = "blaze-code outside name 1";

/// What a target is, by an identity that does not move when the named thing's body is edited:
/// for a function its item's stable id and type arguments by content (blaze-program's
/// `Symbol`); for an outside name the hash of the name ([`Symbol::of_name`]).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Symbol(pub [u8; 32]);

impl Symbol {
    /// The symbol of a C library name.
    pub fn of_name(name: &str) -> Symbol {
        Symbol(Hasher::new(NAME_DOMAIN).update(name.as_bytes()).finish().0)
    }
}

impl fmt::Display for Symbol {
    /// The first 12 hex digits.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0[..6].iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

/// The kind of thing a target is. The order is the encoding's and the sort's.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(u8)]
pub enum Space {
    /// A function instance: its import word holds its body's address.
    Func = 0,
    /// A static: its address.
    Static = 1,
    /// A vtable: its address.
    Vtable = 2,
    /// A thread-local: its import word holds its offset in a thread's block, not an address.
    Tls = 3,
    /// A C library name, bound at start by `dlsym`; the record keeps the name.
    Outside = 4,
}

impl Space {
    const ALL: [Space; 5] = [Space::Func, Space::Static, Space::Vtable, Space::Tls, Space::Outside];

    fn from_byte(byte: u8) -> Option<Space> {
        Space::ALL.get(usize::from(byte)).copied()
    }
}

/// What a site or word names.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Target {
    pub space: Space,
    pub symbol: Symbol,
}

/// The instructions at a site, which the segment's layout completes.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(u8)]
pub enum SiteKind {
    /// `adrp xN, word@PAGE` at `at`, `ldr xN, [xN, word@PAGEOFF]` at `at + 4`: the target's import
    /// word's value in `xN`.
    Load = 0,
    /// A [`SiteKind::Load`] into `x16`, then `blr x16` at `at + 8`: a call of a function.
    Call = 1,
    /// `adrp xN, r@PAGE` at `at`, `add xN, xN, r@PAGEOFF` at `at + 4`: the address of byte
    /// `addend` of this record's relro part. Its target is [`NO_TARGET`].
    Relro = 2,
}

impl SiteKind {
    const ALL: [SiteKind; 3] = [SiteKind::Load, SiteKind::Call, SiteKind::Relro];

    fn from_byte(byte: u8) -> Option<SiteKind> {
        SiteKind::ALL.get(usize::from(byte)).copied()
    }

    /// The bytes of code the site covers.
    pub fn len(self) -> u32 {
        match self {
            SiteKind::Load | SiteKind::Relro => 8,
            SiteKind::Call => 12,
        }
    }
}

/// Instructions of the code that name a target (or the record's own relro part).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Site {
    /// The first instruction's offset from the code's first byte (4-aligned).
    pub at: u32,
    pub kind: SiteKind,
    /// The target's index in the record's targets, or [`NO_TARGET`] for [`SiteKind::Relro`].
    pub target: u32,
    /// For [`SiteKind::Relro`], the relro byte addressed; 0 otherwise.
    pub addend: u32,
}

/// An 8-byte word of the relro part the image fills: the address of `target` plus `addend` (a
/// thread-local's offset plus `addend`), or of this record's own code or relro byte `addend`
/// ([`OWN_CODE`], [`OWN_RELRO`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Word {
    /// The word's offset in the relro part (8-aligned).
    pub at: u32,
    pub target: u32,
    pub addend: i64,
}

/// The part containing a source-position slot, in canonical sort order.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[repr(u8)]
pub enum PositionPart {
    Code = 0,
    Relro = 1,
}

/// Which one-based number of the site's source position the slot will hold.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum PositionField {
    Line = 0,
    Col = 1,
}

impl PositionField {
    pub fn value(self, (line, col): (u32, u32)) -> u32 {
        match self {
            Self::Line => line,
            Self::Col => col,
        }
    }
}

/// A four-byte, little-endian slot that placement fills. `site` counts bytes from the first
/// byte of `item`'s text. Only the placed copy holds the corresponding line or column.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PositionRow {
    pub part: PositionPart,
    pub at: u32,
    pub item: u128,
    pub site: u32,
    pub field: PositionField,
}

impl PositionRow {
    fn slot(&self) -> (PositionPart, u32) {
        (self.part, self.at)
    }

    /// Check the same slot contract when sealing builder input and when opening stored bytes.
    fn check_slot(&self, code: &[u8], relro: &[u8], previous: Option<(PositionPart, u32)>) -> Result<(), RecordError> {
        let (part, at) = self.slot();
        if at % 4 != 0 {
            return Err(RecordError::PositionMisaligned { part, at });
        }
        let bytes = match part {
            PositionPart::Code => code,
            PositionPart::Relro => relro,
        };
        let start = at as usize;
        let Some(slot) = bytes.get(start..).and_then(|tail| tail.get(..4)) else {
            return Err(RecordError::PositionOutside { part, at, len: bytes.len() as u32 });
        };
        if slot != &[0; 4] {
            return Err(RecordError::PositionNonzero { part, at });
        }
        if let Some(prev) = previous {
            if prev == (part, at) {
                return Err(RecordError::PositionsTwice { part, at });
            }
            if prev > (part, at) {
                return Err(RecordError::PositionsUnsorted { part, at });
            }
        }
        Ok(())
    }
}

/// A landing pad: a call in `[start, start + len)` that unwinds lands at `landing` with
/// `action`, all offsets from the code's first byte.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Pad {
    pub start: u32,
    pub len: u32,
    pub landing: u32,
    pub action: u32,
}

/// Which construct an `unreachable`'s `brk` ends: what the walk that built the function knew
/// when it wrote the `unreachable` (blaze-build `TrapSite`). The order is the encoding's.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(u8)]
pub enum TrapKind {
    /// A `match` whose scrutinee matched none of its arms.
    MatchNoArm = 0,
    /// A `let`-`else` whose `else` block ran to its end (it must diverge).
    LetElse = 1,
    /// An expression of type `!` produced a value.
    NeverValue = 2,
    /// A call typed to never return (`-> !`) returned.
    AfterDivergingCall = 3,
    /// `core::intrinsics::unreachable` was called (as `core::hint::unreachable_unchecked` does).
    Intrinsic = 4,
    /// An enum's discriminant was none of its variants'.
    Discriminant = 5,
    /// A slice's length matched none of the slice patterns.
    SliceLength = 6,
    /// An `asm!` block marked `noreturn` ran to its end.
    NoreturnAsm = 7,
    /// The body's end, past its last live statement, was reached.
    BodyEnd = 8,
    /// A panic call the build made (a bounds, arithmetic, validity or unwinding check's) returned.
    AfterPanic = 9,
}

impl TrapKind {
    const ALL: [TrapKind; 10] = [
        TrapKind::MatchNoArm,
        TrapKind::LetElse,
        TrapKind::NeverValue,
        TrapKind::AfterDivergingCall,
        TrapKind::Intrinsic,
        TrapKind::Discriminant,
        TrapKind::SliceLength,
        TrapKind::NoreturnAsm,
        TrapKind::BodyEnd,
        TrapKind::AfterPanic,
    ];

    fn from_byte(byte: u8) -> Option<TrapKind> {
        TrapKind::ALL.get(usize::from(byte)).copied()
    }

    /// The construct, as the end of a sentence that starts with it (`the `match` at F:L:C`).
    pub fn construct(self) -> &'static str {
        match self {
            TrapKind::MatchNoArm => "the `match`",
            TrapKind::LetElse => "the `let`-`else`",
            TrapKind::NeverValue => "the `!`-typed expression",
            TrapKind::AfterDivergingCall => "the call of a fn typed `-> !`",
            TrapKind::Intrinsic => "the call of `core::intrinsics::unreachable`",
            TrapKind::Discriminant => "the read of the enum's discriminant",
            TrapKind::SliceLength => "the slice pattern",
            TrapKind::NoreturnAsm => "the `noreturn` `asm!` block",
            TrapKind::BodyEnd => "the body",
            TrapKind::AfterPanic => "the check's panic call",
        }
    }

    /// What happened there, after [`TrapKind::construct`] and its place.
    pub fn happened(self) -> &'static str {
        match self {
            TrapKind::MatchNoArm => "took no arm",
            TrapKind::LetElse => "ran its `else` to the end (it must diverge)",
            TrapKind::NeverValue => "produced a value",
            TrapKind::AfterDivergingCall => "returned",
            TrapKind::Intrinsic => "ran (as `unreachable_unchecked` does): the caller promised it never would",
            TrapKind::Discriminant => "found no variant's discriminant",
            TrapKind::SliceLength => "matched no length",
            TrapKind::NoreturnAsm => "ran to its end",
            TrapKind::BodyEnd => "ran past its last live statement",
            TrapKind::AfterPanic => "returned",
        }
    }
}

/// Where the value a trap's construct switched on is when its `brk` strikes: what the emitter
/// knows of the code it made (blaze-emit pass.rs), never a guess.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Held {
    /// Not known: the `brk` is not reached from one switch alone, or the value is not kept.
    Unknown,
    /// In general register `xN` (`N` up to 30).
    Reg(u8),
    /// In the 8 bytes at `sp + offset`.
    Stack(u32),
}

impl Held {
    /// The encoding's tag byte.
    fn tag(self) -> u8 {
        match self {
            Held::Unknown => 0,
            Held::Reg(_) => 1,
            Held::Stack(_) => 2,
        }
    }
}

/// How many of an enum's discriminants a [`Subject`] keeps.
pub const SUBJECT_SHOWN: usize = 8;

/// The enum whose discriminant a [`TrapKind::Discriminant`] trap read, as the layout the build
/// used has it: what the report names and lists next to the value read. A function of the
/// layout, an input of the code too (rule 25).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Subject {
    /// The enum's item (its `ItemKey`'s bits), for the one printer of an item.
    pub item: u128,
    /// How many variants the enum has.
    pub count: u32,
    /// The first `min(count, SUBJECT_SHOWN)` variants' discriminants, as the switch compares
    /// them (masked to 64 bits); the rest are 0 (sealed so).
    pub discriminants: [u64; SUBJECT_SHOWN],
}

impl Subject {
    /// The discriminants kept.
    pub fn shown(&self) -> &[u64] {
        &self.discriminants[..(self.count as usize).min(SUBJECT_SHOWN)]
    }
}

/// One `unreachable`'s `brk` in the code: WHICH construct it ends.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TrapRow {
    /// The `brk`'s offset from the code's first byte (4-aligned).
    pub at: u32,
    pub kind: TrapKind,
    /// The construct's place, item-relative (rule 71): the item whose text holds it (the body's
    /// owner, by its `ItemKey`'s bits) and its first byte counted from the item's first byte;
    /// for one inside a macro's expansion its outermost call's. `None`: the build did not know.
    pub site: Option<(u128, u32)>,
    /// Where the value the construct switched on is when the `brk` strikes (the emitter's).
    pub held: Held,
    /// The enum whose discriminant was read (the build's, for a [`TrapKind::Discriminant`]).
    pub subject: Option<Subject>,
}

/// What a build knows of one `unreachable` it wrote, before the code exists: its `ordinal`
/// among the function's `unreachable` instructions in layout order (what every rewrite between
/// the build and the emitter keeps: legalize copies each one as it is), its kind, site and
/// subject. The emitter makes it the [`TrapRow`] of that instruction's `brk` (blaze-emit
/// record.rs), adding where the switched value is held.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TrapSource {
    pub ordinal: u32,
    pub kind: TrapKind,
    pub site: Option<(u128, u32)>,
    pub subject: Option<Subject>,
}

/// A [`TrapRow`]'s encoded site field meaning none.
const NO_SITE: u32 = u32::MAX;

/// The highest general register a [`Held::Reg`] names (`x30`).
const MOST_REG: u8 = 30;

/// Why a record was not sealed: the first broken rule, naming the offset it is about.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RecordError {
    /// A site or word at `at` names a target index the record does not have.
    NoSuchTarget { at: u32, target: u32 },
    /// A site at `at` covering `len` bytes is not inside the code (`code` bytes).
    SiteOutside { at: u32, len: u32, code: u32 },
    /// A site's first instruction at `at` is not 4-aligned.
    SiteMisaligned { at: u32 },
    /// The site at `at` overlaps the one before it.
    SitesOverlap { at: u32 },
    /// A relro site at `at` addresses relro byte `addend`, past the relro part's `relro` bytes.
    RelroOutside { at: u32, addend: u32, relro: u32 },
    /// A word at `at` is not inside the relro part (`relro` bytes).
    WordOutside { at: u32, relro: u32 },
    /// A word at `at` is not 8-aligned.
    WordMisaligned { at: u32 },
    /// Two words at `at`.
    WordsOverlap { at: u32 },
    PositionMisaligned { part: PositionPart, at: u32 },
    PositionOutside { part: PositionPart, at: u32, len: u32 },
    PositionNonzero { part: PositionPart, at: u32 },
    PositionsTwice { part: PositionPart, at: u32 },
    PositionsUnsorted { part: PositionPart, at: u32 },
    PositionOverlapsReference { part: PositionPart, at: u32 },
    UnresolvedPosition { operation: &'static str, row: PositionRow },
    /// A landing pad's range from `start`, or its landing, is not inside the code.
    PadOutside { start: u32 },
    /// An outside target with no name kept for it.
    NameMissing { symbol: Symbol },
    /// A part (`what`) longer than a `u32` counts.
    TooLarge { what: &'static str },
    /// A trap row's `brk` at `at` is not a whole instruction inside the code (`code` bytes).
    TrapOutside { at: u32, code: u32 },
    /// Two trap rows at `at`.
    TrapsTwice { at: u32 },
    /// A trap row's site names byte `NO_SITE`, the encoding's word for none.
    TrapSite { at: u32 },
    /// A trap row's held value names register `reg`, past `x30`.
    HeldReg { at: u32, reg: u8 },
    /// The `part`'s (`"code"` or `"relro part"`) alignment `align` (log2) is below its least
    /// ([`LEAST_CODE_ALIGN`], [`LEAST_RELRO_ALIGN`]) or past [`MOST_PART_ALIGN`].
    Align { part: &'static str, align: u8 },
}

impl fmt::Display for RecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            RecordError::NoSuchTarget { at, target } => write!(f, "a code record's reference at {at} names target {target}, which the record does not list (run.md, \"The code record\")"),
            RecordError::SiteOutside { at, len, code } => write!(f, "a code record's site at {at} covers {len} bytes past its {code} bytes of code"),
            RecordError::SiteMisaligned { at } => write!(f, "a code record's site at {at} is not on an instruction (4-aligned)"),
            RecordError::SitesOverlap { at } => write!(f, "a code record's site at {at} overlaps the site before it"),
            RecordError::RelroOutside { at, addend, relro } => write!(f, "a code record's site at {at} addresses relro byte {addend}, past its {relro} relro bytes"),
            RecordError::WordOutside { at, relro } => write!(f, "a code record's relro word at {at} is past its {relro} relro bytes"),
            RecordError::WordMisaligned { at } => write!(f, "a code record's relro word at {at} is not 8-aligned"),
            RecordError::WordsOverlap { at } => write!(f, "a code record has two relro words at {at}"),
            RecordError::PositionMisaligned { part, at } => write!(f, "a code record's {part:?} source-position slot at {at} is not 4-aligned"),
            RecordError::PositionOutside { part, at, len } => write!(f, "a code record's four-byte source-position slot at {at} extends past its {len}-byte {part:?} part"),
            RecordError::PositionNonzero { part, at } => write!(f, "a code record's {part:?} source-position slot at {at} must contain four zero bytes"),
            RecordError::PositionsTwice { part, at } => write!(f, "a code record has two source-position rows on the {part:?} slot at {at}"),
            RecordError::PositionsUnsorted { part, at } => write!(f, "a code record's {part:?} source-position slot at {at} is out of order"),
            RecordError::PositionOverlapsReference { part, at } => write!(f, "a code record's {part:?} source-position slot at {at} overlaps a reference"),
            RecordError::UnresolvedPosition { operation, row } => write!(f, "{operation} cannot resolve {:?} for item {:032x} at byte {} (record {:?} slot at {})", row.field, row.item, row.site, row.part, row.at),
            RecordError::PadOutside { start } => write!(f, "a code record's landing pad from {start} is not inside its code"),
            RecordError::NameMissing { symbol } => write!(f, "a code record names the outside symbol {symbol} but keeps no name for it"),
            RecordError::TooLarge { what } => write!(f, "a code record's {what} is longer than a u32 counts"),
            RecordError::TrapOutside { at, code } => write!(f, "a code record's trap row at {at} is not an instruction of its {code} bytes of code"),
            RecordError::TrapsTwice { at } => write!(f, "a code record has two trap rows at {at}"),
            RecordError::TrapSite { at } => write!(f, "a code record's trap row at {at} names site byte {NO_SITE}, the form's word for no site"),
            RecordError::Align { part, align } => write!(
                f,
                "a code record's {part} asks an alignment of 2^{align}: below its least (code 2^{LEAST_CODE_ALIGN}, relro 2^{LEAST_RELRO_ALIGN}) or past a page (2^{MOST_PART_ALIGN})"
            ),
            RecordError::HeldReg { at, reg } => write!(f, "a code record's trap row at {at} says register x{reg} holds the switched value, past x{MOST_REG}"),
        }
    }
}

impl core::error::Error for RecordError {}

/// A code record being built. Targets may be added in any order and more than once;
/// [`CodeRecord::seal`] sorts, merges and renumbers them.
pub struct CodeRecord<'a> {
    pub code: AVec<'a, u8>,
    pub relro: AVec<'a, u8>,
    pub targets: AVec<'a, Target>,
    pub sites: AVec<'a, Site>,
    pub words: AVec<'a, Word>,
    pub positions: AVec<'a, PositionRow>,
    pub frame: u32,
    /// log2 of the alignment the code's first byte is placed at: the most its data rows ask
    /// (each row's offset in `code` is a multiple of its own alignment), at least
    /// [`LEAST_CODE_ALIGN`], at most [`MOST_PART_ALIGN`]. Every placement honours it
    /// (image.rs `lay_out`; place.rs `place` refuses an address that does not).
    pub code_align: u8,
    /// log2 of the alignment the relro part's first byte is placed at, as `code_align` for its
    /// rows; at least [`LEAST_RELRO_ALIGN`].
    pub relro_align: u8,
    pub pads: AVec<'a, Pad>,
    /// Per `unreachable`'s `brk`, the construct it ends, in any order (sealed by offset).
    pub traps: AVec<'a, TrapRow>,
    /// Each outside name: its symbol and its text's range in `text`.
    names: AVec<'a, (Symbol, u32, u32)>,
    text: AVec<'a, u8>,
    alloc: Alloc<'a>,
}

impl<'a> CodeRecord<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> CodeRecord<'a> {
        CodeRecord {
            code: Vec::new_in(alloc),
            relro: Vec::new_in(alloc),
            targets: Vec::new_in(alloc),
            sites: Vec::new_in(alloc),
            words: Vec::new_in(alloc),
            positions: Vec::new_in(alloc),
            frame: 0,
            code_align: LEAST_CODE_ALIGN,
            relro_align: LEAST_RELRO_ALIGN,
            pads: Vec::new_in(alloc),
            traps: Vec::new_in(alloc),
            names: Vec::new_in(alloc),
            text: Vec::new_in(alloc),
            alloc,
        }
    }

    /// `target`'s index, for a site or word made before [`CodeRecord::seal`].
    pub fn target(&mut self, target: Target) -> u32 {
        self.targets.push(target);
        (self.targets.len() - 1) as u32
    }

    /// The index of the outside name `name`, kept for the launcher's `dlsym`.
    pub fn outside(&mut self, name: &str) -> u32 {
        let symbol = Symbol::of_name(name);
        let start = self.text.len() as u32;
        self.text.extend_from_slice(name.as_bytes());
        self.names.push((symbol, start, name.len() as u32));
        self.target(Target { space: Space::Outside, symbol })
    }

    /// The record in its one canonical form: targets sorted and unique (every index renumbered),
    /// sites and words sorted by offset, pads by start, names by symbol; every rule of the form
    /// checked once here, so a [`Sealed`] needs no check again (rule 74).
    pub fn seal(self) -> Result<Sealed<'a>, RecordError> {
        let CodeRecord { code, relro, targets, mut sites, mut words, mut positions, frame, code_align, relro_align, mut pads, mut traps, names, text, alloc } = self;
        if !(LEAST_CODE_ALIGN..=MOST_PART_ALIGN).contains(&code_align) {
            return Err(RecordError::Align { part: "code", align: code_align });
        }
        if !(LEAST_RELRO_ALIGN..=MOST_PART_ALIGN).contains(&relro_align) {
            return Err(RecordError::Align { part: "relro part", align: relro_align });
        }
        fits(code.len(), "code")?;
        fits(relro.len(), "relro part")?;
        // Sort the targets by value through an index list; equal targets become one.
        let mut order: AVec<'a, u32> = Vec::with_capacity_in(targets.len(), alloc);
        for i in 0..targets.len() {
            order.push(i as u32);
        }
        order.sort_unstable_by(|&a, &b| targets[a as usize].cmp(&targets[b as usize]));
        let mut renumber: AVec<'a, u32> = Vec::from_elem_in(0, targets.len(), alloc);
        let mut sorted: AVec<'a, Target> = Vec::with_capacity_in(targets.len(), alloc);
        for &old in order.iter() {
            let t = targets[old as usize];
            if sorted.last() != Some(&t) {
                sorted.push(t);
            }
            renumber[old as usize] = (sorted.len() - 1) as u32;
        }
        drop(order);
        let code_len = code.len() as u32;
        let relro_len = relro.len() as u32;
        for site in sites.iter_mut() {
            match site.kind {
                SiteKind::Relro => {
                    if site.target != NO_TARGET {
                        return Err(RecordError::NoSuchTarget { at: site.at, target: site.target });
                    }
                    if u64::from(site.addend) >= u64::from(relro_len) {
                        return Err(RecordError::RelroOutside { at: site.at, addend: site.addend, relro: relro_len });
                    }
                }
                SiteKind::Load | SiteKind::Call => match renumber.get(site.target as usize) {
                    Some(&new) => site.target = new,
                    None => return Err(RecordError::NoSuchTarget { at: site.at, target: site.target }),
                },
            }
        }
        sites.sort_unstable_by_key(|s| s.at);
        let mut end = 0u64;
        for site in sites.iter() {
            if site.at % 4 != 0 {
                return Err(RecordError::SiteMisaligned { at: site.at });
            }
            if u64::from(site.at) < end {
                return Err(RecordError::SitesOverlap { at: site.at });
            }
            end = u64::from(site.at) + u64::from(site.kind.len());
            if end > u64::from(code_len) {
                return Err(RecordError::SiteOutside { at: site.at, len: site.kind.len(), code: code_len });
            }
        }
        for word in words.iter_mut() {
            if word.target == OWN_CODE || word.target == OWN_RELRO {
                continue;
            }
            match renumber.get(word.target as usize) {
                Some(&new) => word.target = new,
                None => return Err(RecordError::NoSuchTarget { at: word.at, target: word.target }),
            }
        }
        drop(renumber);
        words.sort_unstable_by_key(|w| w.at);
        let mut next = 0u64;
        for word in words.iter() {
            if word.at % 8 != 0 {
                return Err(RecordError::WordMisaligned { at: word.at });
            }
            if u64::from(word.at) < next {
                return Err(RecordError::WordsOverlap { at: word.at });
            }
            next = u64::from(word.at) + 8;
            if next > u64::from(relro_len) {
                return Err(RecordError::WordOutside { at: word.at, relro: relro_len });
            }
        }
        fits(positions.len(), "source-position table")?;
        positions.sort_unstable_by_key(PositionRow::slot);
        let mut previous = None;
        for row in positions.iter() {
            row.check_slot(&code, &relro, previous)?;
            previous = Some(row.slot());
        }
        check_position_references(&positions, |row| (row.part as u8, row.at), sites.iter().copied(), words.iter().copied())?;
        pads.sort_unstable_by_key(|p| (p.start, p.len, p.landing, p.action));
        for pad in pads.iter() {
            let past = u64::from(pad.start) + u64::from(pad.len);
            if past > u64::from(code_len) || pad.landing >= code_len {
                return Err(RecordError::PadOutside { start: pad.start });
            }
        }
        fits(traps.len(), "trap table")?;
        traps.sort_unstable_by_key(|t| t.at);
        for (i, trap) in traps.iter().enumerate() {
            if trap.at % 4 != 0 || u64::from(trap.at) + 4 > u64::from(code_len) {
                return Err(RecordError::TrapOutside { at: trap.at, code: code_len });
            }
            if i > 0 && traps[i - 1].at == trap.at {
                return Err(RecordError::TrapsTwice { at: trap.at });
            }
            if let Some((_, NO_SITE)) = trap.site {
                return Err(RecordError::TrapSite { at: trap.at });
            }
            if let Held::Reg(reg) = trap.held
                && reg > MOST_REG
            {
                return Err(RecordError::HeldReg { at: trap.at, reg });
            }
        }
        // One form (rule 25): a subject's discriminants past the ones it keeps are 0.
        for trap in traps.iter_mut() {
            if let Some(subject) = trap.subject.as_mut() {
                let kept = (subject.count as usize).min(SUBJECT_SHOWN);
                subject.discriminants[kept..].fill(0);
            }
        }
        // Names by symbol, each once, the text rebuilt in that order.
        let mut by_symbol = names;
        by_symbol.sort_unstable_by_key(|n| n.0);
        by_symbol.dedup_by_key(|n| n.0);
        let mut kept: AVec<'a, (Symbol, u32, u32)> = Vec::with_capacity_in(by_symbol.len(), alloc);
        let mut kept_text: AVec<'a, u8> = Vec::new_in(alloc);
        for &(symbol, start, len) in by_symbol.iter() {
            let at = kept_text.len() as u32;
            kept_text.extend_from_slice(&text[start as usize..(start + len) as usize]);
            kept.push((symbol, at, len));
        }
        drop(by_symbol);
        drop(text);
        // Every outside target has a name; a name no target uses is dropped.
        kept.retain(|n| sorted.binary_search(&Target { space: Space::Outside, symbol: n.0 }).is_ok());
        let mut packed: AVec<'a, u8> = Vec::new_in(alloc);
        let mut packed_names: AVec<'a, (Symbol, u32, u32)> = Vec::with_capacity_in(kept.len(), alloc);
        for &(symbol, start, len) in kept.iter() {
            let at = packed.len() as u32;
            packed.extend_from_slice(&kept_text[start as usize..(start + len) as usize]);
            packed_names.push((symbol, at, len));
        }
        for t in sorted.iter() {
            if t.space == Space::Outside && packed_names.binary_search_by_key(&t.symbol, |n| n.0).is_err() {
                return Err(RecordError::NameMissing { symbol: t.symbol });
            }
        }
        fits(packed.len(), "names' text")?;
        Ok(Sealed(CodeRecord {
            code,
            relro,
            targets: sorted,
            sites,
            words,
            positions,
            frame,
            code_align,
            relro_align,
            pads,
            traps,
            names: packed_names,
            text: packed,
            alloc,
        }))
    }
}

fn fits(len: usize, what: &'static str) -> Result<(), RecordError> {
    match u32::try_from(len) {
        Ok(_) => Ok(()),
        Err(_) => Err(RecordError::TooLarge { what }),
    }
}

/// A position and an address patch must never own the same bytes. Search only when the
/// record has position rows, using the already checked slot order, in memory or on disk.
fn check_position_references<T>(
    positions: &[T], slot: impl Fn(&T) -> (u8, u32), sites: impl Iterator<Item = Site>, words: impl Iterator<Item = Word>,
) -> Result<(), RecordError> {
    if positions.is_empty() {
        return Ok(());
    }
    let code = sites.map(|s| (PositionPart::Code, s.at, s.kind.len()));
    let relro = words.map(|w| (PositionPart::Relro, w.at, 8));
    for (part, at, len) in code.chain(relro) {
        // A malformed reference could start within a position slot, so include the slot
        // containing its first byte in the search.
        let i = positions.partition_point(|row| slot(row) < (part as u8, at & !3));
        if let Some((tag, offset)) = positions.get(i).map(&slot)
            && tag == part as u8 && u64::from(offset) < u64::from(at) + u64::from(len)
        {
            return Err(RecordError::PositionOverlapsReference { part, at: offset });
        }
    }
    Ok(())
}

/// A record in its canonical form ([`CodeRecord::seal`]): the only form that encodes.
pub struct Sealed<'a>(CodeRecord<'a>);

impl<'a> Sealed<'a> {
    pub fn record(&self) -> &CodeRecord<'a> {
        &self.0
    }

    /// The record's bytes, appended to `out`: the head, then each section 8-aligned (module doc
    /// of [`CodeView`] says the layout). Equal sealed records write equal bytes.
    pub fn encode(&self, out: &mut AVec<'_, u8>) {
        let r = &self.0;
        let start = out.len();
        out.extend_from_slice(&MAGIC);
        put32(out, r.frame);
        out.extend_from_slice(&[r.code_align, r.relro_align, 0, 0]);
        let subjects = r.traps.iter().filter(|t| t.subject.is_some()).count();
        for count in [r.code.len(), r.relro.len(), r.targets.len(), r.sites.len(), r.words.len(), r.pads.len(), r.names.len(), r.text.len(), r.traps.len(), subjects, r.positions.len()] {
            put32(out, count as u32);
        }
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
        for s in r.sites.iter() {
            put32(out, s.at);
            out.push(s.kind as u8);
            out.extend_from_slice(&[0; 3]);
            put32(out, s.target);
            put32(out, s.addend);
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
        for t in r.traps.iter() {
            put32(out, t.at);
            out.push(t.kind as u8);
            out.push(t.held.tag());
            out.push(match t.held {
                Held::Reg(reg) => reg,
                Held::Unknown | Held::Stack(_) => 0,
            });
            out.push(0);
            let (item, site) = t.site.unwrap_or((0, NO_SITE));
            put32(out, site);
            put32(out, match t.held {
                Held::Stack(offset) => offset,
                Held::Unknown | Held::Reg(_) => 0,
            });
            out.extend_from_slice(&item.to_le_bytes());
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
        for &(symbol, at, len) in r.names.iter() {
            out.extend_from_slice(&symbol.0);
            put32(out, at);
            put32(out, len);
        }
        out.extend_from_slice(&r.text);
        pad8(out, start);
        for row in r.positions.iter() {
            out.extend_from_slice(&row.item.to_le_bytes());
            put32(out, row.site);
            put32(out, row.at);
            out.push(row.part as u8);
            out.push(row.field as u8);
            out.extend_from_slice(&[0; 6]);
        }
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

const TARGET_ROW: usize = 40;
const SITE_ROW: usize = 16;
const WORD_ROW: usize = 16;
const PAD_ROW: usize = 16;
const NAME_ROW: usize = 40;
const TRAP_ROW: usize = 32;
const SUBJECT_ROW: usize = 24 + 8 * SUBJECT_SHOWN;
const POSITION_ROW: usize = 32;

/// Why bytes are not a code record.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DecodeError {
    /// The bytes do not start with [`MAGIC`].
    Magic,
    /// The head's counts need `need` bytes; there are `have`.
    Length { need: usize, have: usize },
    /// Target `row` has the space byte `byte`.
    Space { row: u32, byte: u8 },
    /// Site `row` has the kind byte `byte`.
    Kind { row: u32, byte: u8 },
    /// Trap row `row` has the kind byte `byte`.
    TrapKind { row: u32, byte: u8 },
    /// Trap row `row`'s held value is malformed: an unknown tag, or a register past `x30`.
    Held { row: u32 },
    /// Subject row `row` is not the subject of a trap row (offsets rising, each a trap row's).
    Subject { row: u32 },
    /// The source-position row has an unknown tag or a nonzero reserved byte.
    PositionByte { row: u32, what: &'static str, byte: u8 },
    /// The decoded position would violate the same slot contract as builder input.
    Position(RecordError),
    /// The word after the header counts is reserved and must be zero.
    Reserved { value: u32 },
    /// The bytes are a record of an older form of this one (`form` its eight-byte head, e.g.
    /// `bzcode05`, which held no part alignments): never read, rebuilt (rule 26).
    Older { form: [u8; 8] },
    /// The head's `part` alignment `align` (log2) is outside what [`CodeRecord::seal`] allows,
    /// or the two bytes after the alignments are not zero (`align` is then the first such byte).
    Align { part: &'static str, align: u8 },
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            DecodeError::Magic => write!(f, "the bytes are not a code record: no `bzcode07` head"),
            DecodeError::Older { form } => write!(
                f,
                "the bytes are a code record of the older form `{}`, which held no source-position table: rebuilt, never read (rule 26)",
                core::str::from_utf8(&form).unwrap_or("bzcode0?")
            ),
            DecodeError::Align { part, align } => write!(
                f,
                "a code record's head gives its {part} the alignment byte {align}: outside code 2^{LEAST_CODE_ALIGN}..2^{MOST_PART_ALIGN}, relro 2^{LEAST_RELRO_ALIGN}..2^{MOST_PART_ALIGN}, or a byte after them not zero"
            ),
            DecodeError::TrapKind { row, byte } => write!(f, "a code record's trap row {row} has the unknown kind {byte}"),
            DecodeError::Held { row } => write!(f, "a code record's trap row {row} has a malformed held value (an unknown tag or a register past x30)"),
            DecodeError::Subject { row } => write!(f, "a code record's subject row {row} is not the subject of one of its trap rows"),
            DecodeError::PositionByte { row, what, byte } => write!(f, "a code record's source-position row {row} has invalid {what} byte {byte}"),
            DecodeError::Position(error) => fmt::Display::fmt(&error, f),
            DecodeError::Reserved { value } => write!(f, "a code record's reserved word after the header counts is {value}, expected zero"),
            DecodeError::Length { need, have } => write!(f, "a code record's head counts {need} bytes; it has {have}"),
            DecodeError::Space { row, byte } => write!(f, "a code record's target {row} has the unknown space {byte}"),
            DecodeError::Kind { row, byte } => write!(f, "a code record's site {row} has the unknown kind {byte}"),
        }
    }
}

impl core::error::Error for DecodeError {}

/// An encoded record read in place. Layout (little-endian; every section starts 8-aligned from
/// the record's first byte): `MAGIC`; `frame: u32`, `code_align: u8`, `relro_align: u8` (log2
/// of each part's placement alignment), two zero bytes; eleven `u32` counts (code
/// bytes, relro bytes, targets, sites, words, pads, names, text bytes, traps, subjects,
/// positions) and four zero bytes; the
/// code; the relro part; targets (space `u8`, 7 zero bytes, symbol 32); sites (`at: u32`, kind
/// `u8`, 3 zero bytes, `target: u32`, `addend: u32`); words (`at: u32`, `target: u32`, `addend:
/// i64`); pads (four `u32`); traps (`at: u32`, kind `u8`, held tag `u8` (0 unknown, 1 a
/// register, 2 a stack slot), held register `u8`, a zero byte, `site: u32` (`u32::MAX` for
/// none), held stack offset `u32`, `item: u128`, 0 for none), by `at`; subjects (`at: u32` of
/// its trap row, `count: u32`, `item: u128`, eight `u64` discriminants), by `at`; names
/// (symbol 32, text `at: u32`, `len: u32`); the padded text; source positions (`item: u128`,
/// `site: u32`, `at: u32`, part and field tags, six zero bytes), ordered by (part, at).
#[derive(Clone, Copy)]
pub struct CodeView<'b> {
    bytes: &'b [u8],
    frame: u32,
    code_align: u8,
    relro_align: u8,
    code: &'b [u8],
    relro: &'b [u8],
    targets: &'b [u8],
    sites: &'b [u8],
    words: &'b [u8],
    pads: &'b [u8],
    traps: &'b [u8],
    subjects: &'b [u8],
    positions: &'b [[u8; POSITION_ROW]],
    names: &'b [u8],
    text: &'b [u8],
}

fn get32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn get64(b: &[u8], at: usize) -> u64 {
    u64::from(get32(b, at)) | (u64::from(get32(b, at + 4)) << 32)
}

fn up8(n: usize) -> usize {
    (n + 7) & !7
}

/// The `used` bytes at `*at`, then `*at` moved past the section's `len` (its padded length).
/// The caller checked that every section fits.
fn part<'b>(bytes: &'b [u8], at: &mut usize, len: usize, used: usize) -> &'b [u8] {
    let section = &bytes[*at..*at + used];
    *at += len;
    section
}

fn symbol_at(b: &[u8], at: usize) -> Symbol {
    let mut s = [0u8; 32];
    s.copy_from_slice(&b[at..at + 32]);
    Symbol(s)
}

fn decode_position(bytes: &[u8; POSITION_ROW], index: usize) -> Result<PositionRow, DecodeError> {
    let row = index as u32;
    let part = match bytes[24] {
        0 => PositionPart::Code,
        1 => PositionPart::Relro,
        byte => return Err(DecodeError::PositionByte { row, what: "part", byte }),
    };
    let field = match bytes[25] {
        0 => PositionField::Line,
        1 => PositionField::Col,
        byte => return Err(DecodeError::PositionByte { row, what: "field", byte }),
    };
    let mut item = [0u8; 16];
    item.copy_from_slice(&bytes[..16]);
    Ok(PositionRow { part, field, item: u128::from_le_bytes(item), site: get32(bytes, 16), at: get32(bytes, 20) })
}

impl<'b> CodeView<'b> {
    /// Reads the record at the start of `bytes`, which must be exactly one record. The lengths and
    /// every tag byte are checked here, once: the accessors below index without checking again.
    pub fn read(bytes: &'b [u8]) -> Result<CodeView<'b>, DecodeError> {
        if bytes.len() < MAGIC.len() {
            return Err(DecodeError::Length { need: MAGIC.len(), have: bytes.len() });
        }
        if bytes[..8] != MAGIC {
            let mut form = [0u8; 8];
            form.copy_from_slice(&bytes[..8]);
            return Err(match OLDER.contains(&form) {
                true => DecodeError::Older { form },
                false => DecodeError::Magic,
            });
        }
        if bytes.len() < HEAD {
            return Err(DecodeError::Length { need: HEAD, have: bytes.len() });
        }
        let reserved = get32(bytes, 60);
        if reserved != 0 {
            return Err(DecodeError::Reserved { value: reserved });
        }
        let frame = get32(bytes, 8);
        let (code_align, relro_align) = (bytes[12], bytes[13]);
        if !(LEAST_CODE_ALIGN..=MOST_PART_ALIGN).contains(&code_align) {
            return Err(DecodeError::Align { part: "code", align: code_align });
        }
        if !(LEAST_RELRO_ALIGN..=MOST_PART_ALIGN).contains(&relro_align) {
            return Err(DecodeError::Align { part: "relro part", align: relro_align });
        }
        if let Some(&byte) = bytes[14..16].iter().find(|&&b| b != 0) {
            return Err(DecodeError::Align { part: "head's zero bytes", align: byte });
        }
        let count = |i: usize| get32(bytes, 16 + 4 * i) as usize;
        let lens = [
            up8(count(0)),
            up8(count(1)),
            count(2) * TARGET_ROW,
            count(3) * SITE_ROW,
            count(4) * WORD_ROW,
            count(5) * PAD_ROW,
            count(6) * NAME_ROW,
            up8(count(7)),
            count(8) * TRAP_ROW,
            count(9) * SUBJECT_ROW,
            count(10) * POSITION_ROW,
        ];
        let need = HEAD + lens.iter().sum::<usize>();
        if need != bytes.len() {
            return Err(DecodeError::Length { need, have: bytes.len() });
        }
        let mut at = HEAD;
        let code = part(bytes, &mut at, lens[0], count(0));
        let relro = part(bytes, &mut at, lens[1], count(1));
        let targets = part(bytes, &mut at, lens[2], lens[2]);
        let sites = part(bytes, &mut at, lens[3], lens[3]);
        let words = part(bytes, &mut at, lens[4], lens[4]);
        let pads = part(bytes, &mut at, lens[5], lens[5]);
        let traps = part(bytes, &mut at, lens[8], lens[8]);
        let subjects = part(bytes, &mut at, lens[9], lens[9]);
        let names = part(bytes, &mut at, lens[6], lens[6]);
        let text = part(bytes, &mut at, lens[7], count(7));
        let (positions, _) = part(bytes, &mut at, lens[10], lens[10]).as_chunks::<POSITION_ROW>();
        let mut previous = None;
        for (i, bytes) in positions.iter().enumerate() {
            let position = decode_position(bytes, i)?;
            if let Some(&byte) = bytes[26..].iter().find(|&&b| b != 0) {
                return Err(DecodeError::PositionByte { row: i as u32, what: "reserved", byte });
            }
            position.check_slot(code, relro, previous).map_err(DecodeError::Position)?;
            previous = Some(position.slot());
        }
        for row in 0..count(8) {
            let byte = traps[row * TRAP_ROW + 4];
            if TrapKind::from_byte(byte).is_none() {
                return Err(DecodeError::TrapKind { row: row as u32, byte });
            }
            let (tag, reg) = (traps[row * TRAP_ROW + 5], traps[row * TRAP_ROW + 6]);
            if tag > 2 || (tag == 1 && reg > MOST_REG) {
                return Err(DecodeError::Held { row: row as u32 });
            }
        }
        // Each subject names a trap row, the subjects' offsets rising: so `trap` finds a row's
        // subject by halving over them.
        let trap_ats = |at: u32| (0..count(8)).any(|row| get32(traps, row * TRAP_ROW) == at);
        for row in 0..count(9) {
            let at = get32(subjects, row * SUBJECT_ROW);
            let rising = row == 0 || get32(subjects, (row - 1) * SUBJECT_ROW) < at;
            if !rising || !trap_ats(at) {
                return Err(DecodeError::Subject { row: row as u32 });
            }
        }
        for row in 0..count(2) {
            let byte = targets[row * TARGET_ROW];
            if Space::from_byte(byte).is_none() {
                return Err(DecodeError::Space { row: row as u32, byte });
            }
        }
        for row in 0..count(3) {
            let byte = sites[row * SITE_ROW + 4];
            if SiteKind::from_byte(byte).is_none() {
                return Err(DecodeError::Kind { row: row as u32, byte });
            }
        }
        let view = CodeView { bytes, frame, code_align, relro_align, code, relro, targets, sites, words, pads, traps, subjects, positions, names, text };
        check_position_references(
            positions,
            |row| (row[24], get32(row, 20)),
            (0..view.sites()).map(|i| view.site(i)),
            (0..view.words()).map(|i| view.word(i)),
        ).map_err(DecodeError::Position)?;
        Ok(view)
    }

    /// log2 of the alignment the code's first byte must be placed at ([`CodeRecord::code_align`]).
    pub fn code_align(&self) -> u8 {
        self.code_align
    }

    /// log2 of the alignment the relro part's first byte must be placed at
    /// ([`CodeRecord::relro_align`]).
    pub fn relro_align(&self) -> u8 {
        self.relro_align
    }

    /// The whole encoding: equal records are equal bytes.
    pub fn bytes(&self) -> &'b [u8] {
        self.bytes
    }

    pub fn frame(&self) -> u32 {
        self.frame
    }

    pub fn code(&self) -> &'b [u8] {
        self.code
    }

    pub fn relro(&self) -> &'b [u8] {
        self.relro
    }

    pub fn positions(&self) -> usize {
        self.positions.len()
    }

    pub fn position(&self, i: usize) -> PositionRow {
        decode_position(&self.positions[i], i).expect("CodeView::read checked every source-position tag")
    }

    /// Refuse to copy canonical bytes into a form that cannot keep or resolve source slots.
    pub fn require_positionless(&self, operation: &'static str) -> Result<(), RecordError> {
        if self.positions.is_empty() {
            Ok(())
        } else {
            Err(RecordError::UnresolvedPosition { operation, row: self.position(0) })
        }
    }

    pub fn targets(&self) -> usize {
        self.targets.len() / TARGET_ROW
    }

    pub fn target(&self, i: usize) -> Target {
        let row = i * TARGET_ROW;
        let Some(space) = Space::from_byte(self.targets[row]) else {
            unreachable!("CodeView::read checked every target's space byte");
        };
        Target { space, symbol: symbol_at(self.targets, row + 8) }
    }

    pub fn sites(&self) -> usize {
        self.sites.len() / SITE_ROW
    }

    pub fn site(&self, i: usize) -> Site {
        let row = i * SITE_ROW;
        let Some(kind) = SiteKind::from_byte(self.sites[row + 4]) else {
            unreachable!("CodeView::read checked every site's kind byte");
        };
        Site { at: get32(self.sites, row), kind, target: get32(self.sites, row + 8), addend: get32(self.sites, row + 12) }
    }

    pub fn words(&self) -> usize {
        self.words.len() / WORD_ROW
    }

    pub fn word(&self, i: usize) -> Word {
        let row = i * WORD_ROW;
        Word { at: get32(self.words, row), target: get32(self.words, row + 4), addend: get64(self.words, row + 8) as i64 }
    }

    pub fn pads(&self) -> usize {
        self.pads.len() / PAD_ROW
    }

    pub fn pad(&self, i: usize) -> Pad {
        let row = i * PAD_ROW;
        Pad { start: get32(self.pads, row), len: get32(self.pads, row + 4), landing: get32(self.pads, row + 8), action: get32(self.pads, row + 12) }
    }

    pub fn traps(&self) -> usize {
        self.traps.len() / TRAP_ROW
    }

    pub fn trap(&self, i: usize) -> TrapRow {
        let row = i * TRAP_ROW;
        let Some(kind) = TrapKind::from_byte(self.traps[row + 4]) else {
            unreachable!("CodeView::read checked every trap row's kind byte");
        };
        let site = match get32(self.traps, row + 8) {
            NO_SITE => None,
            site => {
                let mut item = [0u8; 16];
                item.copy_from_slice(&self.traps[row + 16..row + 32]);
                Some((u128::from_le_bytes(item), site))
            }
        };
        let held = match self.traps[row + 5] {
            1 => Held::Reg(self.traps[row + 6]),
            2 => Held::Stack(get32(self.traps, row + 12)),
            _ => Held::Unknown,
        };
        let at = get32(self.traps, row);
        TrapRow { at, kind, site, held, subject: self.subject_at(at) }
    }

    /// The subject of the trap row at code offset `at`, by halving over the subjects (sorted by
    /// offset, checked by `read`).
    fn subject_at(&self, at: u32) -> Option<Subject> {
        let (mut lo, mut hi) = (0usize, self.subjects.len() / SUBJECT_ROW);
        while lo < hi {
            let mid = (lo + hi) / 2;
            let row = mid * SUBJECT_ROW;
            match get32(self.subjects, row).cmp(&at) {
                core::cmp::Ordering::Less => lo = mid + 1,
                core::cmp::Ordering::Greater => hi = mid,
                core::cmp::Ordering::Equal => {
                    let mut item = [0u8; 16];
                    item.copy_from_slice(&self.subjects[row + 8..row + 24]);
                    let mut discriminants = [0u64; SUBJECT_SHOWN];
                    for (i, d) in discriminants.iter_mut().enumerate() {
                        *d = get64(self.subjects, row + 24 + 8 * i);
                    }
                    return Some(Subject { item: u128::from_le_bytes(item), count: get32(self.subjects, row + 4), discriminants });
                }
            }
        }
        None
    }

    /// The trap row of the `brk` at code offset `at`, by halving over the rows (sorted by
    /// offset when sealed).
    pub fn trap_at(&self, at: u32) -> Option<TrapRow> {
        let (mut lo, mut hi) = (0usize, self.traps());
        while lo < hi {
            let mid = (lo + hi) / 2;
            match get32(self.traps, mid * TRAP_ROW).cmp(&at) {
                core::cmp::Ordering::Less => lo = mid + 1,
                core::cmp::Ordering::Greater => hi = mid,
                core::cmp::Ordering::Equal => return Some(self.trap(mid)),
            }
        }
        None
    }

    /// The name kept for an outside target's symbol, by halving over the sorted names.
    pub fn name(&self, symbol: Symbol) -> Option<&'b str> {
        let rows = self.names.len() / NAME_ROW;
        let (mut lo, mut hi) = (0usize, rows);
        while lo < hi {
            let mid = (lo + hi) / 2;
            let at = mid * NAME_ROW;
            match symbol_at(self.names, at).cmp(&symbol) {
                core::cmp::Ordering::Less => lo = mid + 1,
                core::cmp::Ordering::Greater => hi = mid,
                core::cmp::Ordering::Equal => {
                    let start = get32(self.names, at + 32) as usize;
                    let len = get32(self.names, at + 36) as usize;
                    return core::str::from_utf8(self.text.get(start..start + len)?).ok();
                }
            }
        }
        None
    }
}
