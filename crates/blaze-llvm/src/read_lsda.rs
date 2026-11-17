//! LLVM's language-specific data area (LSDA: `__gcc_except_tab` on Mach-O, `.gcc_except_table`
//! on ELF) read back into blaze's unwind rows (UNWIND.md 3.8, piece P8b). The optimizing tier's
//! record then carries rows only ([`blaze_code::unwind::Row`]); the LSDA a placed record is
//! unwound with is made again by blaze-code eh.rs at placement, the one entry writer for every
//! tier. So this reader is the inverse of that writer and of LLVM's `EHStreamer`, and it reads
//! exactly what the pinned library's personality routine reads.
//!
//! # The steps, quoted, and what is read for each
//!
//! The Itanium C++ ABI's exception table as the pinned library parses it
//! (library/std/src/sys/personality/dwarf/eh.rs, `find_eh_action` and `interpret_cs_action`,
//! nightly-2026-10-27):
//!
//! - `let start_encoding = reader.read::<u8>(); if start_encoding != DW_EH_PE_omit {
//!   read_encoded_pointer(..) } else { func_start }`: only `DW_EH_PE_omit` is read, so every
//!   call site and landing pad is an offset from the function's start, as [`Row`] holds them.
//!   Any other encoding is refused ([`LsdaRefusal::LpStartEncoding`]).
//! - `let ttype_encoding = reader.read::<u8>(); if ttype_encoding != DW_EH_PE_omit {
//!   reader.read_uleb128(); }`: `DW_EH_PE_omit`, or a 4-byte entry form (`DW_EH_PE_udata4`, as
//!   blaze-code eh.rs writes; `DW_EH_PE_pcrel | DW_EH_PE_sdata4`, with or without
//!   `DW_EH_PE_indirect`, as LLVM writes on Mach-O and on PIC ELF) followed by the offset from
//!   the end of that field to the type table's base. The library skips the table ("Rust doesn't
//!   analyze exception types"); this reader reads it, to refuse a catch of a named type.
//! - `reader.read::<u8>()` (the call-site encoding) and `let call_site_table_length =
//!   reader.read_uleb128();`: `DW_EH_PE_udata4` (blaze-code eh.rs) or `DW_EH_PE_uleb128` (LLVM's
//!   `EHStreamer` for every non-SjLj target); the action table starts where the call-site table
//!   ends (`reader.ptr.add(call_site_table_length as usize)`).
//! - per row: `let cs_start = read_encoded_offset(..); let cs_len = ..; let cs_lpad = ..; let
//!   cs_action_entry = reader.read_uleb128();`, sorted by start ("Callsite table is sorted by
//!   cs_start, so if we've passed the ip, we may stop searching"): a row starting before the
//!   previous one ended is refused, as is a row or landing past the function.
//! - `if cs_lpad == 0 { return Ok(EHAction::None) }`: [`Action::Continue`] (an action entry
//!   beside a zero landing is refused: the library never reads it, LLVM never writes it).
//! - `if cs_action_entry == 0 { EHAction::Cleanup(lpad) }`: [`Action::Cleanup`].
//! - `let action_record = action_table.offset(cs_action_entry as isize - 1); ttype_index =
//!   read_sleb128(); next_action = read_sleb128();`, then `if next_action != 0 {
//!   EHAction::Catch(lpad) } else if ttype_index == 0 { EHAction::Cleanup(lpad) } else if
//!   ttype_index > 0 { EHAction::Catch(lpad) } else { EHAction::Filter(lpad) }`: a positive
//!   index whose type entry is the null catch-all (`catch ptr null`, index 1 in both writers)
//!   is [`Action::Catch`]; a negative index naming an empty exception specification (`filter
//!   [0 x ptr] zeroinitializer`, the record `(-1, 0)`) is [`Action::Terminate`]. Refused by
//!   name: a chained record (`next_action != 0`), the record `(0, 0)` (a cleanup said the long
//!   way, which neither writer makes), a type entry that is not null (a catch of one type,
//!   which Rust never asks for), a filter that names a type.
//! - "Ip is not present in the table. This indicates a nounwind call." (`EHAction::Terminate`):
//!   a call with no row has no row here either, which is what blaze's rows mean by an absent
//!   call (blaze-code unwind.rs, "A pad").
//!
//! The table is read from `lsda[0]`; bytes past the last one any step reads are not this
//! function's (a section holds every function's table back to back) and are not looked at.
//! Every multi-byte field is little-endian: both hosts in scope are arm64, little-endian.

use core::fmt;

use blaze_code::unwind::{Action, Row};
use blaze_own::{AVec, Alloc, AllocFailure, Vec};

/// `DW_EH_PE_omit`, `DW_EH_PE_uleb128`, `DW_EH_PE_udata4` (LSB 5.0, "DWARF Exception Header
/// Encoding"; the pinned library's eh.rs constants).
pub const OMIT: u8 = 0xff;
pub const ULEB128: u8 = 0x01;
pub const UDATA4: u8 = 0x03;
/// `DW_EH_PE_pcrel | DW_EH_PE_sdata4`, and the same with `DW_EH_PE_indirect`: LLVM's type-table
/// encodings (its object-file lowering for Mach-O, and for ELF when position-independent).
pub const PCREL_SDATA4: u8 = 0x1b;
pub const INDIRECT_PCREL_SDATA4: u8 = 0x9b;

/// The bytes of one type-table entry in every type encoding read here.
const TYPE_ENTRY: u64 = 4;

/// A planted bug for the test's control (rule 53): `None` in every caller but that test.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Plant {
    None,
    /// A catch-all action record is read as a terminate.
    CatchReadsAsTerminate,
}

/// Why an LSDA was refused: the function it is for (`func_start`), the reader's position in
/// the LSDA when it refused (just past the field or call-site row that broke the rule), and
/// the rule.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LsdaError {
    pub func_start: u64,
    pub at: usize,
    pub refusal: LsdaRefusal,
}

/// The rule an LSDA broke. A call site is named by its `start`, an offset in the function.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LsdaRefusal {
    /// The table ends inside `what`.
    Truncated { what: &'static str },
    /// A LEB128 `what` holds more than 64 bits.
    LebTooLong { what: &'static str },
    /// The landing-pad base is encoded, not omitted: landings would not be function-relative.
    LpStartEncoding { encoding: u8 },
    /// The type table's encoding is not omit or one of the 4-byte forms.
    TypeEncoding { encoding: u8 },
    /// The type table's base lies past the table's bytes.
    TypesOutside { offset: u64, lsda: usize },
    /// The call-site encoding is not `DW_EH_PE_udata4` or `DW_EH_PE_uleb128`.
    CallSiteEncoding { encoding: u8 },
    /// The call-site table runs past the table's bytes.
    CallSiteTableOutside { len: u64, lsda: usize },
    /// A call-site row runs past the end of the call-site table.
    CallSiteTorn { table_end: usize },
    /// A call site runs past the function.
    CallSiteOutside { start: u64, len: u64, func_len: u32 },
    /// A call site starts before the previous one ended.
    CallSitesUnsorted { start: u64, previous_end: u64 },
    /// A landing pad is past the function.
    LandingOutside { start: u64, landing: u64, func_len: u32 },
    /// A call site with no landing names an action.
    ActionWithoutLanding { start: u64, entry: u64 },
    /// A call site names an action record and the table has no type table.
    ActionWithoutTypes { start: u64 },
    /// A call site's action entry is past the table's bytes.
    ActionOutside { start: u64, entry: u64 },
    /// An action record chains to another (`next_action != 0`).
    ChainedAction { start: u64, next: i64 },
    /// The action record `(0, 0)`: a cleanup named through the action table.
    CleanupRecord { start: u64 },
    /// A type index names an entry outside the type table.
    TypeOutside { start: u64, index: i64 },
    /// A catch of a type that is not the null catch-all.
    TypedCatch { start: u64, index: i64, entry: u32 },
    /// A filter whose exception specification names a type.
    NonEmptyFilter { start: u64, index: i64, first: u64 },
    /// No memory for the rows.
    Alloc(AllocFailure),
}

impl fmt::Display for LsdaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let LsdaError { func_start, at, refusal } = *self;
        let rule = "(blaze-llvm read_lsda.rs; LIB std/src/sys/personality/dwarf/eh.rs `find_eh_action`)";
        write!(f, "the LSDA of the function at {func_start:#x}, at byte {at}: ")?;
        match refusal {
            LsdaRefusal::Truncated { what } => write!(f, "the table ends inside its {what} {rule}"),
            LsdaRefusal::LebTooLong { what } => write!(f, "its {what} holds more than 64 bits {rule}"),
            LsdaRefusal::LpStartEncoding { encoding } => {
                write!(f, "its landing-pad base has encoding {encoding:#x}; only omit (0xff, landings from the function's start) is read {rule}")
            }
            LsdaRefusal::TypeEncoding { encoding } => {
                write!(f, "its type table has encoding {encoding:#x}; only omit, udata4 and pcrel sdata4 (direct or indirect) are read {rule}")
            }
            LsdaRefusal::TypesOutside { offset, lsda } => write!(f, "its type table's base, {offset} bytes on, lies past its {lsda} bytes {rule}"),
            LsdaRefusal::CallSiteEncoding { encoding } => write!(f, "its call sites have encoding {encoding:#x}; only udata4 and uleb128 are read {rule}"),
            LsdaRefusal::CallSiteTableOutside { len, lsda } => write!(f, "its call-site table of {len} bytes runs past its {lsda} bytes {rule}"),
            LsdaRefusal::CallSiteTorn { table_end } => write!(f, "a call-site row runs past the call-site table's end at {table_end} {rule}"),
            LsdaRefusal::CallSiteOutside { start, len, func_len } => {
                write!(f, "the call site at {start}, {len} bytes, runs past the function's {func_len} bytes {rule}")
            }
            LsdaRefusal::CallSitesUnsorted { start, previous_end } => {
                write!(f, "the call site at {start} starts before the previous one ended at {previous_end} (\"Callsite table is sorted by cs_start\") {rule}")
            }
            LsdaRefusal::LandingOutside { start, landing, func_len } => {
                write!(f, "the call site at {start} lands at {landing}, past the function's {func_len} bytes {rule}")
            }
            LsdaRefusal::ActionWithoutLanding { start, entry } => write!(f, "the call site at {start} has no landing and names action entry {entry} {rule}"),
            LsdaRefusal::ActionWithoutTypes { start } => write!(f, "the call site at {start} names an action record and the table has no type table {rule}"),
            LsdaRefusal::ActionOutside { start, entry } => write!(f, "the call site at {start} names action entry {entry}, past the table {rule}"),
            LsdaRefusal::ChainedAction { start, next } => {
                write!(f, "the call site at {start} has a chained action record (next {next}); blaze's rows hold one action a call {rule}")
            }
            LsdaRefusal::CleanupRecord { start } => write!(f, "the call site at {start} names the action record (0, 0); a cleanup is read only as action entry 0 {rule}"),
            LsdaRefusal::TypeOutside { start, index } => write!(f, "the call site at {start} names type index {index}, outside the type table {rule}"),
            LsdaRefusal::TypedCatch { start, index, entry } => {
                write!(f, "the call site at {start} catches type {index}, entry {entry:#x}, not the null catch-all; Rust catches only all {rule}")
            }
            LsdaRefusal::NonEmptyFilter { start, index, first } => {
                write!(f, "the call site at {start} has filter {index} naming type {first}; only the empty filter (terminate) is read {rule}")
            }
            LsdaRefusal::Alloc(why) => write!(f, "no memory for its rows: {why}"),
        }
    }
}

impl core::error::Error for LsdaError {}

/// The rows of `lsda`, the LSDA of the function at `func_start`, `func_len` bytes long, in
/// `alloc`; the steps are in the module's doc. `func_start` names the function in an error:
/// every offset read is already function-relative (the landing-pad base is omitted).
pub fn rows_of_lsda<'a>(lsda: &[u8], func_start: u64, func_len: u32, alloc: Alloc<'a>) -> Result<AVec<'a, Row>, LsdaError> {
    rows_of_lsda_planted(lsda, func_start, func_len, alloc, Plant::None)
}

/// [`rows_of_lsda`] with `plant` misbehaving on purpose (the test's control).
pub fn rows_of_lsda_planted<'a>(
    lsda: &[u8],
    func_start: u64,
    func_len: u32,
    alloc: Alloc<'a>,
    plant: Plant,
) -> Result<AVec<'a, Row>, LsdaError> {
    let mut reader = Reader { bytes: lsda, pos: 0 };
    let mut rows: AVec<'a, Row> = Vec::new_in(alloc);
    match read(&mut reader, func_len, plant, &mut rows) {
        Ok(()) => Ok(rows),
        Err(refusal) => Err(LsdaError { func_start, at: reader.pos, refusal }),
    }
}

/// How a call-site row's three offsets are encoded.
#[derive(Clone, Copy)]
enum CallSiteForm {
    Udata4,
    Uleb128,
}

impl CallSiteForm {
    fn read(self, reader: &mut Reader<'_>, what: &'static str) -> Result<u64, LsdaRefusal> {
        match self {
            CallSiteForm::Udata4 => reader.u32(what).map(u64::from),
            CallSiteForm::Uleb128 => reader.uleb(what),
        }
    }
}

/// The header, then every call-site row into `rows`.
fn read(reader: &mut Reader<'_>, func_len: u32, plant: Plant, rows: &mut AVec<'_, Row>) -> Result<(), LsdaRefusal> {
    let lsda = reader.bytes;
    let lsda_len = lsda.len() as u64;
    let start_encoding = reader.u8("landing-pad base encoding")?;
    if start_encoding != OMIT {
        return Err(LsdaRefusal::LpStartEncoding { encoding: start_encoding });
    }
    let types_base = match reader.u8("type table encoding")? {
        OMIT => None,
        UDATA4 | PCREL_SDATA4 | INDIRECT_PCREL_SDATA4 => {
            let offset = reader.uleb("type table offset")?;
            // The offset counts from the end of its own field.
            let base = (reader.pos as u64)
                .checked_add(offset)
                .filter(|base| *base <= lsda_len)
                .ok_or(LsdaRefusal::TypesOutside { offset, lsda: lsda.len() })?;
            Some(base)
        }
        encoding => return Err(LsdaRefusal::TypeEncoding { encoding }),
    };
    let form = match reader.u8("call-site encoding")? {
        UDATA4 => CallSiteForm::Udata4,
        ULEB128 => CallSiteForm::Uleb128,
        encoding => return Err(LsdaRefusal::CallSiteEncoding { encoding }),
    };
    let table_len = reader.uleb("call-site table length")?;
    let table_end = (reader.pos as u64)
        .checked_add(table_len)
        .filter(|end| *end <= lsda_len)
        .ok_or(LsdaRefusal::CallSiteTableOutside { len: table_len, lsda: lsda.len() })?;
    // `table_end <= lsda.len()`, so it is a `usize`.
    let table_end = table_end as usize;
    let table = Tables { lsda, actions: table_end, types_base };
    let func = u64::from(func_len);
    let mut previous_end = 0u64;
    while reader.pos < table_end {
        let start = form.read(reader, "call-site start")?;
        let len = form.read(reader, "call-site length")?;
        let lpad = form.read(reader, "call-site landing pad")?;
        let entry = reader.uleb("call-site action entry")?;
        if reader.pos > table_end {
            return Err(LsdaRefusal::CallSiteTorn { table_end });
        }
        if start < previous_end {
            return Err(LsdaRefusal::CallSitesUnsorted { start, previous_end });
        }
        let end = start.checked_add(len).filter(|end| *end <= func).ok_or(LsdaRefusal::CallSiteOutside { start, len, func_len })?;
        previous_end = end;
        let action = match (lpad, entry) {
            (0, 0) => Action::Continue,
            (0, entry) => return Err(LsdaRefusal::ActionWithoutLanding { start, entry }),
            (_, 0) => Action::Cleanup,
            (_, entry) => table.action(start, entry, plant)?,
        };
        if lpad >= func && lpad != 0 {
            return Err(LsdaRefusal::LandingOutside { start, landing: lpad, func_len });
        }
        // `start + len <= func_len` and `lpad < func_len`: each fits a `u32`.
        let row = Row { start: start as u32, len: len as u32, landing: lpad as u32, action };
        rows.try_push(row).map_err(|refused| LsdaRefusal::Alloc(refused.why))?;
    }
    Ok(())
}

/// Where the action table and the type table's base are in the LSDA.
struct Tables<'b> {
    lsda: &'b [u8],
    /// The action table's first byte: the call-site table's end.
    actions: usize,
    /// The type table's base, `None` when its encoding is omit.
    types_base: Option<u64>,
}

impl Tables<'_> {
    /// The action of the call site at `start` whose action entry is `entry` (not 0).
    fn action(&self, start: u64, entry: u64, plant: Plant) -> Result<Action, LsdaRefusal> {
        let lsda_len = self.lsda.len() as u64;
        // `action_table.offset(cs_action_entry as isize - 1)`.
        let record = (self.actions as u64)
            .checked_add(entry - 1)
            .filter(|at| *at < lsda_len)
            .ok_or(LsdaRefusal::ActionOutside { start, entry })?;
        // `record < lsda.len()`, so it is a `usize`.
        let mut reader = Reader { bytes: self.lsda, pos: record as usize };
        let index = reader.sleb("action record's type index")?;
        let next = reader.sleb("action record's next action")?;
        if next != 0 {
            return Err(LsdaRefusal::ChainedAction { start, next });
        }
        let base = self.types_base.ok_or(LsdaRefusal::ActionWithoutTypes { start })?;
        if index == 0 {
            return Err(LsdaRefusal::CleanupRecord { start });
        }
        if index > 0 {
            // Entry `index` is the `TYPE_ENTRY` bytes `index` entries below the base, and the
            // type table lies after the action table.
            let below = (index as u64).checked_mul(TYPE_ENTRY).ok_or(LsdaRefusal::TypeOutside { start, index })?;
            let at = base
                .checked_sub(below)
                .filter(|at| *at >= self.actions as u64)
                .ok_or(LsdaRefusal::TypeOutside { start, index })?;
            // `at < base <= lsda.len()`, so it is a `usize`.
            let mut types = Reader { bytes: self.lsda, pos: at as usize };
            let word = types.u32("type table entry")?;
            if word != 0 {
                return Err(LsdaRefusal::TypedCatch { start, index, entry: word });
            }
            return Ok(match plant {
                Plant::None => Action::Catch,
                Plant::CatchReadsAsTerminate => Action::Terminate,
            });
        }
        // A filter: its exception specification is a list of ULEB128 type indices ended by 0,
        // `-index - 1` bytes past the base (the Itanium C++ ABI's exception table).
        let past = index.unsigned_abs() - 1;
        let at = base.checked_add(past).filter(|at| *at < lsda_len).ok_or(LsdaRefusal::TypeOutside { start, index })?;
        // `at < lsda.len()`, so it is a `usize`.
        let mut spec = Reader { bytes: self.lsda, pos: at as usize };
        let first = spec.uleb("filter's exception specification")?;
        if first != 0 {
            return Err(LsdaRefusal::NonEmptyFilter { start, index, first });
        }
        Ok(Action::Terminate)
    }
}

/// A cursor over the LSDA's bytes; every read is bounds-checked and names what it read.
struct Reader<'b> {
    bytes: &'b [u8],
    pos: usize,
}

impl Reader<'_> {
    fn u8(&mut self, what: &'static str) -> Result<u8, LsdaRefusal> {
        let byte = *self.bytes.get(self.pos).ok_or(LsdaRefusal::Truncated { what })?;
        self.pos += 1;
        Ok(byte)
    }

    fn u32(&mut self, what: &'static str) -> Result<u32, LsdaRefusal> {
        let end = self.pos.checked_add(4).ok_or(LsdaRefusal::Truncated { what })?;
        let bytes = self.bytes.get(self.pos..end).ok_or(LsdaRefusal::Truncated { what })?;
        let mut word = [0u8; 4];
        word.copy_from_slice(bytes);
        self.pos = end;
        Ok(u32::from_le_bytes(word))
    }

    fn uleb(&mut self, what: &'static str) -> Result<u64, LsdaRefusal> {
        let mut value = 0u64;
        let mut shift = 0u32;
        loop {
            let byte = self.u8(what)?;
            // At shift 63 only the lowest payload bit still fits; past it nothing does.
            if shift > 63 || (shift == 63 && byte & 0x7e != 0) {
                return Err(LsdaRefusal::LebTooLong { what });
            }
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
            shift += 7;
        }
    }

    fn sleb(&mut self, what: &'static str) -> Result<i64, LsdaRefusal> {
        let mut value = 0i64;
        let mut shift = 0u32;
        loop {
            let byte = self.u8(what)?;
            // At shift 63 the payload is the sign alone: all zero or all one.
            let payload = byte & 0x7f;
            if shift > 63 || (shift == 63 && payload != 0 && payload != 0x7f) {
                return Err(LsdaRefusal::LebTooLong { what });
            }
            value |= i64::from(payload) << shift;
            shift += 7;
            if byte & 0x80 == 0 {
                if shift < 64 && byte & 0x40 != 0 {
                    value |= -1i64 << shift;
                }
                return Ok(value);
            }
        }
    }
}
