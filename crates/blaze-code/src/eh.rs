//! The bytes a platform's unwinder reads for one placed record (run.md, "End state", "Panics"):
//! a CIE, its one FDE, the zero length that ends a run of entries, and the record's
//! language-specific data area (LSDA), made from the record's unwind rows ([`crate::unwind`]).
//! Sans-IO: addresses in, bytes out; the launcher copies them where it said they would be and
//! tells the unwinder (blaze-launch-rt code_unwind.rs).
//!
//! # The references, and what is mirrored
//!
//! The CIE and FDE are `.eh_frame`'s form: DWARF 5 section 6.4.1 ("Structure of Call Frame
//! Information") with the Linux Standard Base's `.eh_frame` changes (a CIE id of 0, the FDE's
//! CIE pointer counted back from its own field, the `z` augmentation and its `P`, `L`, `R`
//! letters). Every address is `DW_EH_PE_absptr` (8 bytes as they are): the bytes are made for
//! the one address the launcher will copy them to, so nothing in them is relative.
//!
//! The LSDA is the Itanium C++ ABI's exception table in exactly the form the pinned library's
//! personality routine parses (library/std/src/sys/personality/dwarf/eh.rs, `find_eh_action`,
//! nightly-2026-10-27). Its steps, quoted, and what is written for each:
//!
//! - `let start_encoding = reader.read::<u8>(); if start_encoding != DW_EH_PE_omit {..} else
//!   { func_start }`: `DW_EH_PE_omit`, so landings are offsets from the function's start.
//! - `let ttype_encoding = reader.read::<u8>(); if ttype_encoding != DW_EH_PE_omit {
//!   reader.read_uleb128(); }`: `DW_EH_PE_udata4` and the type table's offset when a row
//!   catches or terminates, else `DW_EH_PE_omit`.
//! - `reader.read::<u8>()` (the call-site encoding): `DW_EH_PE_udata4`; then
//!   `let call_site_table_length = reader.read_uleb128();`.
//! - per row: `let cs_start = read_encoded_offset(..); let cs_len = ..; let cs_lpad = ..;
//!   let cs_action_entry = reader.read_uleb128();`, sorted by start ("Callsite table is sorted
//!   by cs_start, so if we've passed the ip, we may stop searching").
//! - `if cs_lpad == 0 { return Ok(EHAction::None) }`: [`Action::Continue`].
//! - `if cs_action_entry == 0 { EHAction::Cleanup(lpad) }`: [`Action::Cleanup`].
//! - `let action_record = action_table.offset(cs_action_entry as isize - 1)`, then
//!   `ttype_index = read_sleb128(); next_action = read_sleb128();` with `ttype_index > 0`
//!   `EHAction::Catch` and negative `EHAction::Filter`: [`Action::Catch`] is the record
//!   `(1, 0)` (type 1: the catch-all, a null entry in the type table), [`Action::Terminate`]
//!   the record `(-1, 0)` (an empty exception specification at the type table's base).
//! - The routine finds `ip` as `ip.wrapping_sub(1)` of the return address ("The return
//!   address points 1 byte past the call instruction"), so a row covers its call instruction.

use core::fmt;

use blaze_own::AVec;

use crate::unwind::{Action, Frame, INST, Unwind, reg};

/// `DW_EH_PE_absptr`, `DW_EH_PE_udata4`, `DW_EH_PE_omit`.
pub const ABSPTR: u8 = 0x00;
pub const UDATA4: u8 = 0x03;
pub const OMIT: u8 = 0xff;

/// The call frame instructions used (DWARF 5 section 6.4.2).
pub const CFA_ADVANCE_LOC: u8 = 0x40;
pub const CFA_OFFSET: u8 = 0x80;
pub const CFA_OFFSET_EXTENDED: u8 = 0x05;
pub const CFA_DEF_CFA: u8 = 0x0c;
pub const CFA_DEF_CFA_REGISTER: u8 = 0x0d;
pub const CFA_DEF_CFA_OFFSET: u8 = 0x0e;
pub const CFA_NOP: u8 = 0x00;

/// The CIE's factors: an advance counts instructions; an offset counts 8-byte words down.
pub const CODE_ALIGN: u64 = INST as u64;
pub const DATA_ALIGN: i64 = -8;

/// Where the record's code is, and the routine its landings need.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Placed {
    /// The address the entry's bytes will be copied to.
    pub entry_at: u64,
    /// The address of the record's first code byte.
    pub code_at: u64,
    /// The program's personality routine (its `eh_personality` lang item's entry thunk): asked
    /// only when a row lands.
    pub personality: Option<u64>,
}

/// Where the parts of an entry [`entry`] wrote are, as offsets in its bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Entry {
    /// The FDE: what Apple's unwinder is told of; an ELF unwinder of the whole run (offset 0).
    pub fde: usize,
    /// The LSDA, when a row lands.
    pub lsda: Option<usize>,
}

/// Why no entry was made.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EhError {
    /// The record has no frame description: nothing can be unwound through it.
    NoFrame,
    /// A row lands and the run gave no personality routine.
    NoPersonality,
    /// A length is past what its field holds.
    TooLarge { what: &'static str },
}

impl fmt::Display for EhError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EhError::NoFrame => write!(f, "a code record with no frame description has no unwind entry (blaze-code unwind.rs, \"The frame word\")"),
            EhError::NoPersonality => write!(f, "a code record's pads land, and the run named no personality routine (the program's `eh_personality` lang item)"),
            EhError::TooLarge { what } => write!(f, "an unwind entry's {what} is past what its field holds"),
        }
    }
}

impl core::error::Error for EhError {}

pub fn uleb(out: &mut AVec<'_, u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        out.push(if value == 0 { byte } else { byte | 0x80 });
        if value == 0 {
            return;
        }
    }
}

pub fn sleb(out: &mut AVec<'_, u8>, mut value: i64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        let done = (value == 0 && byte & 0x40 == 0) || (value == -1 && byte & 0x40 != 0);
        out.push(if done { byte } else { byte | 0x80 });
        if done {
            return;
        }
    }
}

/// The bytes `uleb` writes for `value`.
fn uleb_len(mut value: u64) -> usize {
    let mut n = 1;
    while value >= 0x80 {
        value >>= 7;
        n += 1;
    }
    n
}

/// Ends the CIE or FDE that began at `start`: pads it with `DW_CFA_nop` to a multiple of 8
/// bytes (the address size, DWARF 5 section 6.4.1) and writes its length, which counts what
/// follows the length field.
fn finish(out: &mut AVec<'_, u8>, start: usize) -> Result<(), EhError> {
    while (out.len() - start) % 8 != 0 {
        out.push(CFA_NOP);
    }
    let len = u32::try_from(out.len() - start - 4).map_err(|_| EhError::TooLarge { what: "CIE or FDE" })?;
    out[start..start + 4].copy_from_slice(&len.to_le_bytes());
    Ok(())
}

/// `DW_CFA_offset` of `reg` at `CFA - 8 * words` (the data factor is -8).
fn offset(out: &mut AVec<'_, u8>, reg: u16, words: u64) {
    match reg < 64 {
        true => out.push(CFA_OFFSET | reg as u8),
        false => {
            out.push(CFA_OFFSET_EXTENDED);
            uleb(out, u64::from(reg));
        }
    }
    uleb(out, words);
}

/// The FDE's instructions for `frame` (unwind.rs, "The frame word"): after instruction 0 the
/// frame address is `sp + 16` with `x29`, `x30` saved below it; after instruction 1 it is
/// `x29 + 16`; after the `k`th pair's `stp`, its registers are saved at `CFA - 32 - 16k` (the
/// first [`Frame::pairs`] names) and `CFA - 24 - 16k` (the second): x20 below x19.
pub fn frame_program(frame: Frame, out: &mut AVec<'_, u8>) {
    out.push(CFA_ADVANCE_LOC | 1);
    out.push(CFA_DEF_CFA_OFFSET);
    uleb(out, 16);
    offset(out, reg::FP, 2);
    offset(out, reg::LR, 1);
    out.push(CFA_ADVANCE_LOC | 1);
    out.push(CFA_DEF_CFA_REGISTER);
    uleb(out, u64::from(reg::FP));
    for (k, (at_lower, at_higher)) in frame.pairs().enumerate() {
        let below = 16 + 16 * (k as u64 + 1);
        out.push(CFA_ADVANCE_LOC | 1);
        offset(out, at_lower, below / 8);
        offset(out, at_higher, (below - 8) / 8);
    }
}

/// Clears `out` and writes the entry of `unwind`'s record placed as `at` says: a CIE (with a
/// personality and an LSDA encoding when a row lands), the FDE, a zero length ending the run,
/// then the LSDA. The same rows and addresses give the same bytes.
pub fn entry(unwind: &Unwind<'_>, at: Placed, out: &mut AVec<'_, u8>) -> Result<Entry, EhError> {
    let frame = unwind.frame().ok_or(EhError::NoFrame)?;
    let personality = match unwind.lands() {
        true => Some(at.personality.ok_or(EhError::NoPersonality)?),
        false => None,
    };
    out.clear();
    // The CIE: its length (by `finish`), id 0, version 1, the augmentation, the factors, the
    // return address register (x30), the augmentation data, then CFA = sp + 0 at entry.
    out.extend_from_slice(&[0; 8]);
    out.push(1);
    out.extend_from_slice(if personality.is_some() { b"zPLR\0" } else { b"zR\0" });
    uleb(out, CODE_ALIGN);
    sleb(out, DATA_ALIGN);
    uleb(out, u64::from(reg::LR));
    match personality {
        Some(routine) => {
            uleb(out, 1 + 8 + 1 + 1);
            out.push(ABSPTR);
            out.extend_from_slice(&routine.to_le_bytes());
            out.push(ABSPTR);
            out.push(ABSPTR);
        }
        None => {
            uleb(out, 1);
            out.push(ABSPTR);
        }
    }
    out.push(CFA_DEF_CFA);
    uleb(out, u64::from(reg::SP));
    uleb(out, 0);
    finish(out, 0)?;
    // The FDE: its length, then how far back its CIE begins, counted from this field.
    let fde = out.len();
    out.extend_from_slice(&[0; 4]);
    let back = u32::try_from(fde + 4).map_err(|_| EhError::TooLarge { what: "CIE pointer" })?;
    out.extend_from_slice(&back.to_le_bytes());
    out.extend_from_slice(&at.code_at.to_le_bytes());
    out.extend_from_slice(&u64::from(unwind.code_len()).to_le_bytes());
    let lsda_word = match personality {
        Some(_) => {
            uleb(out, 8);
            let word = out.len();
            out.extend_from_slice(&[0; 8]);
            Some(word)
        }
        None => {
            uleb(out, 0);
            None
        }
    };
    frame_program(frame, out);
    finish(out, fde)?;
    out.extend_from_slice(&[0; 4]);
    let lsda = match lsda_word {
        Some(word) => {
            let lsda = out.len();
            let address = at.entry_at.checked_add(lsda as u64).ok_or(EhError::TooLarge { what: "LSDA address" })?;
            out[word..word + 8].copy_from_slice(&address.to_le_bytes());
            lsda_table(unwind, out)?;
            Some(lsda)
        }
        None => None,
    };
    Ok(Entry { fde, lsda })
}

/// The LSDA's action records: `(1, 0)` for a catch, `(-1, 0)` for a terminate, each two
/// one-byte SLEBs. Gives each one's `cs_action_entry` (its offset plus one), 0 when absent.
fn action_entries(unwind: &Unwind<'_>) -> (u64, u64) {
    let has = |want: Action| (0..unwind.rows()).any(|i| unwind.row(i).action == want);
    let catch = match has(Action::Catch) {
        true => 1,
        false => 0,
    };
    let terminate = match has(Action::Terminate) {
        true => 1 + 2 * catch,
        false => 0,
    };
    (catch, terminate)
}

/// Appends the LSDA (the module's doc quotes each step it is read by). Every offset in it is
/// relative to the function's start, so the bytes do not depend on where they are placed:
/// blaze-export's eh_out.rs writes a file's `__gcc_except_tab` with it.
pub fn lsda_table(unwind: &Unwind<'_>, out: &mut AVec<'_, u8>) -> Result<(), EhError> {
    let (catch, terminate) = action_entries(unwind);
    let typed = catch != 0 || terminate != 0;
    // The sizes after the type table's offset field, to know that offset first.
    let mut table = 0usize;
    for i in 0..unwind.rows() {
        let entry = match unwind.row(i).action {
            Action::Catch => catch,
            Action::Terminate => terminate,
            Action::Continue | Action::Cleanup => 0,
        };
        table += 12 + uleb_len(entry);
    }
    let actions = 2 * usize::from(catch != 0) + 2 * usize::from(terminate != 0);
    // One 4-byte type entry (index 1, the catch-all) just below the type table's base.
    let types = if typed { 4 } else { 0 };
    out.push(OMIT);
    match typed {
        true => {
            out.push(UDATA4);
            uleb(out, (1 + uleb_len(table as u64) + table + actions + types) as u64);
        }
        false => out.push(OMIT),
    }
    out.push(UDATA4);
    uleb(out, table as u64);
    for i in 0..unwind.rows() {
        let row = unwind.row(i);
        let (lpad, entry) = match row.action {
            Action::Continue => (0, 0),
            Action::Cleanup => (row.landing, 0),
            Action::Catch => (row.landing, catch),
            Action::Terminate => (row.landing, terminate),
        };
        out.extend_from_slice(&row.start.to_le_bytes());
        out.extend_from_slice(&row.len.to_le_bytes());
        out.extend_from_slice(&lpad.to_le_bytes());
        uleb(out, entry);
    }
    if catch != 0 {
        sleb(out, 1);
        sleb(out, 0);
    }
    if terminate != 0 {
        sleb(out, -1);
        sleb(out, 0);
    }
    if typed {
        // Type 1: the catch-all (a null type), the 4 bytes before the base.
        out.extend_from_slice(&[0; 4]);
        // At the base: the empty exception specification a filter of -1 names (its list of
        // type indices, ended by 0).
        out.push(0);
    }
    Ok(())
}
