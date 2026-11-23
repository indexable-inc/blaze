//! The unwind tables an exported file carries (UNWIND.md 3.9, piece P10): what lets a panic
//! raised in the file unwind through its functions, land in their cleanups and be caught.
//! Sans-IO: the laid-out program and its records in, each section's bytes and image offset out;
//! the platform's writer copies them into the code segment (macho.rs `write`, elf.rs `write`).
//!
//! # What is written
//!
//! - `eh_frame` (Mach-O `__TEXT,__eh_frame`, ELF `.eh_frame`): one CIE `zR` for frames whose
//!   rows do not land, one CIE `zPLR` naming the program's personality routine when any row
//!   lands, then one FDE per placed function whose record has a frame description, then a zero
//!   length ending the run. The form is DWARF 5 section 6.4.1 with the Linux Standard Base's
//!   `.eh_frame` changes, as blaze-code eh.rs writes it for one record; every pointer here is
//!   `DW_EH_PE_pcrel | DW_EH_PE_sdata4` (0x1b), because a file is slid as a whole by its loader
//!   and a pc-relative pointer needs no fixup. The frame's instructions are blaze-code eh.rs's
//!   `frame_program`, the one home of "what the prologue saves where".
//! - `except_tab` (`__TEXT,__gcc_except_tab`, `.gcc_except_table`): each landing function's
//!   language-specific data area, made by blaze-code eh.rs's `lsda_table` (the Itanium C++
//!   ABI's table in the form the pinned library's `find_eh_action` reads), each 4-aligned.
//! - On Mach-O, `unwind_info` (`__TEXT,__unwind_info`, the form of
//!   `<mach-o/compact_unwind_encoding.h>`): one second-level regular page per 511 functions, an
//!   entry for every placed function in address order (so no function's range runs into the
//!   next), each with its compact encoding ([`compact`]) where one describes the frame and the
//!   function has no LSDA, `UNWIND_ARM64_MODE_DWARF | fde_offset` where it has an FDE
//!   otherwise, and `0` (no unwind information) where its record has no frame description.
//! - On ELF, `eh_frame_hdr` (`.eh_frame_hdr`, found through `PT_GNU_EH_FRAME`): version 1,
//!   `eh_frame_ptr` as `pcrel|sdata4`, the FDE count as `udata4`, and the table sorted by
//!   initial location as `datarel|sdata4` pairs (LSB 5.0, "Exception Frames", `.eh_frame_hdr`).
//!
//! # Choices, with their reasons
//!
//! - The personality pointer is direct (`pcrel|sdata4`), not `indirect|pcrel|sdata4` through a
//!   GOT cell as blaze-link unwind.rs writes it: in an exported file the routine is std's
//!   `rust_eh_personality` instance placed in the same image like any function (UNWIND.md 3.9),
//!   so a pc-relative pointer reaches it and no loader-fixed word is needed.
//! - A function with an LSDA is given DWARF mode in `unwind_info`, never a compact encoding
//!   with `UNWIND_HAS_LSDA`: a compact encoding names its personality through the section's
//!   personality array, whose entries are image offsets of pointer cells the loader fills,
//!   which this file does not have; libunwind reads the personality and the LSDA from the FDE
//!   in DWARF mode (its `setInfoBasedOnIPRegister`: "Found info in table, done unless encoding
//!   says to use dwarf"). The LSDA index is therefore empty.
//! - [`compact`] gives `UNWIND_ARM64_MODE_FRAME` with the frame's pair bits. Apple's frame mode
//!   restores a saved pair from `fp - 8` downwards, its lower register first (libunwind
//!   `stepWithCompactEncodingFrame`: `savedRegisterLoc = fp - 8`, x19 there, x20 at `fp - 16`),
//!   which is blaze's one frame form (blaze-code unwind.rs, "The frame word"; unwind-P8a-4): the
//!   frame word's pair bits ARE the encoding's `UNWIND_ARM64_FRAME_*_PAIR` bits.
//! - A function whose rows never land carries no LSDA, as blaze-code eh.rs `entry` decides
//!   (`Unwind::lands`): an unwind through any of its calls goes on to the caller.
//!
//! The section sizes do not depend on addresses (every pointer has a fixed width), so
//! [`eh_room`] measures them before layout by writing them for the functions laid end to end,
//! and the layout keeps that many bytes between the code and the const region
//! (`Laid::tables_room`); [`eh_sections`] refuses, in every build, tables that end past it
//! (`ROOM_SLACK` says why the measure is an upper bound).

use blaze_code::eh::{self, CFA_DEF_CFA, CFA_NOP, CODE_ALIGN, DATA_ALIGN};
use blaze_code::record::Symbol;
use blaze_code::unwind::{self, Frame, Unwind, reg};
use blaze_own::{AVec, Alloc, Vec};

use blaze_native::image::{Image, Region as NativeRegion};
use blaze_native::unwind::{self as native_unwind, Row};

use crate::error::ExportError;
use crate::input::Platform;
use crate::layout::{Laid, Member};
use crate::native_merge::MergeError;

/// `DW_EH_PE_pcrel | DW_EH_PE_sdata4`.
pub const PCREL_SDATA4: u8 = 0x1b;
/// `DW_EH_PE_udata4`.
pub const UDATA4: u8 = 0x03;
/// `DW_EH_PE_datarel | DW_EH_PE_sdata4`.
pub const DATAREL_SDATA4: u8 = 0x3b;
/// `UNWIND_ARM64_MODE_FRAME`: `fp`, `lr` at `[fp]`, `[fp + 8]`, the caller's `sp` at `fp + 16`.
pub const COMPACT_FRAME: u32 = 0x0400_0000;
/// `UNWIND_ARM64_MODE_DWARF`: the low 24 bits are the FDE's offset in `__eh_frame`.
pub const COMPACT_DWARF: u32 = 0x0300_0000;
/// `UNWIND_ARM64_DWARF_SECTION_OFFSET`.
const DWARF_OFFSET: u32 = 0x00ff_ffff;
/// `UNWIND_ARM64_MODE_MASK`.
const MODE_MASK: u32 = 0x0f00_0000;
/// The page a native image's stand-in place is rounded to while the room is measured.
const NATIVE_PAGE: u64 = 0x4000;
/// The alignment of the first table (`eh_frame`'s entries are 8-byte multiples) after the code.
const TABLES_ALIGN: u64 = 8;
/// The alignment of `except_tab` and of the index after the table before each.
const TABLE_ALIGN: u64 = 4;
/// What the real sections can take past [`eh_room`]'s measure: only the start's 8-alignment
/// after the code (up to 7 bytes). The measure and the real tables both start 8-aligned, so the
/// two 4-alignments inside them ([`TABLE_ALIGN`] divides [`TABLES_ALIGN`]) fall alike and are
/// already in the measured length; the measure counts every function the layout can place, and
/// a subset (folded or dropped functions) is never longer: each table's length is a sum over
/// functions, `except_tab`'s padded tail is taken up by the index's 4-alignment either way, and
/// the Rust rows reach each other in `__unwind_info` (one entry each, no gap entries between
/// them). [`eh_sections`] refuses tables past the measure by type ([`ExportError::EhRoom`]).
const ROOM_SLACK: u64 = TABLES_ALIGN - 1;
const _: () = assert!(TABLES_ALIGN % TABLE_ALIGN == 0, "the measure's 4-alignments match the real ones only on an 8-aligned start");

/// The tables of one exported file, each at its image offset in the code segment, in this
/// order: `eh_frame`, `except_tab`, then `unwind_info` (Mach-O) or `eh_frame_hdr` (ELF). The
/// other platform's index is empty, at the index's place.
pub struct EhSections<'a> {
    pub platform: Platform,
    pub eh_frame_at: u64,
    pub eh_frame: AVec<'a, u8>,
    pub except_tab_at: u64,
    pub except_tab: AVec<'a, u8>,
    /// Mach-O only.
    pub unwind_info_at: u64,
    pub unwind_info: AVec<'a, u8>,
    /// ELF only.
    pub eh_frame_hdr_at: u64,
    pub eh_frame_hdr: AVec<'a, u8>,
    /// Functions given an FDE.
    pub functions: u32,
    /// Of them, those given an LSDA.
    pub landing: u32,
}

impl EhSections<'_> {
    /// The image offset past the last table.
    pub fn end(&self) -> u64 {
        let ends = [
            self.eh_frame_at + self.eh_frame.len() as u64,
            self.except_tab_at + self.except_tab.len() as u64,
            self.unwind_info_at + self.unwind_info.len() as u64,
            self.eh_frame_hdr_at + self.eh_frame_hdr.len() as u64,
        ];
        let mut end = 0;
        for e in ends {
            end = end.max(e);
        }
        end
    }
}

/// The native image's unwind data the file carries (native_merge.rs, NATIVE.md W9; main's ruling
/// (a)): its rows, FDEs and eh_frame pointers as data (blaze-native `Image::unwind`, `frames`,
/// `eh_pointers`), the image placed at `shift` (image address + `shift` = image offset here).
/// [`tables`] copies the image's eh_frame after the Rust FDEs (one `__eh_frame`), re-encodes each
/// of its pc-relative fields, and writes one `__unwind_info` over both row lists with
/// blaze-native's writer (one builder, rule 30); the image's personality is named by its GOT
/// cell in the personality array.
pub struct Outside<'o, 'n> {
    pub image: &'o Image<'n>,
    pub shift: u64,
}

impl Outside<'_, '_> {
    /// The image's merged eh_frame: its image address and bytes.
    fn eh_frame(&self) -> Result<(u64, &[u8]), ExportError> {
        let missing = ExportError::Native(MergeError::NoRegion { region: NativeRegion::EhFrame });
        let region = self.image.region(NativeRegion::EhFrame).ok_or(missing)?;
        let from = region.at.checked_sub(self.image.file_start).ok_or(missing)? as usize;
        let bytes = self.image.bytes.get(from..from + region.len as usize).ok_or(missing)?;
        Ok((region.at, bytes))
    }
}

/// The compact encoding of `frame`: frame mode, with `UNWIND_ARM64_FRAME_X19_X20_PAIR`
/// (0x001) .. `X27_X28_PAIR` (0x010) from the frame word's x bits and `D8_D9_PAIR` (0x100) ..
/// `D14_D15_PAIR` (0x800) from its d bits (`<mach-o/compact_unwind_encoding.h>`:296-304). The
/// layouts agree (the module's doc), so every frame has one.
pub fn compact(frame: Frame) -> u32 {
    COMPACT_FRAME | u32::from(frame.x_pairs) | (u32::from(frame.d_pairs) << 8)
}

/// One placed function the tables cover, at its image offset.
#[derive(Clone, Copy)]
struct Covered<'r> {
    symbol: Symbol,
    at: u64,
    unwind: Unwind<'r>,
}

/// What a function was given: its FDE's offset in `eh_frame` (exactly when its record has a
/// frame) and its LSDA's offset in `except_tab` (exactly when a row lands).
#[derive(Clone, Copy)]
struct Given {
    fde: Option<u32>,
    lsda: Option<u64>,
}

/// Where a pc-relative field in `eh_frame` points.
#[derive(Clone, Copy)]
enum PatchTo {
    /// An image offset.
    Image(u64),
    /// An offset in `except_tab`.
    Lsda(u64),
}

fn up(n: u64, to: u64) -> u64 {
    (n + to - 1) & !(to - 1)
}

fn pad(out: &mut AVec<'_, u8>, to: usize) {
    while out.len() % to != 0 {
        out.push(0);
    }
}

/// `target - field` as the 4 bytes of an `sdata4`, or the distance named as too large.
fn rel32(target: u64, field: u64, what: &'static str) -> Result<[u8; 4], ExportError> {
    let distance = i128::from(target) - i128::from(field);
    match i32::try_from(distance) {
        Ok(v) => Ok(v.to_le_bytes()),
        Err(_) => Err(ExportError::TooLarge { what, len: target.abs_diff(field) }),
    }
}

fn u32_of(value: u64, what: &'static str) -> Result<u32, ExportError> {
    u32::try_from(value).map_err(|_| ExportError::TooLarge { what, len: value })
}

/// Ends the CIE or FDE that began at `start`: `DW_CFA_nop` to a multiple of 8 bytes, then its
/// length, which counts what follows the length field (as blaze-code eh.rs `finish`).
fn finish(f: &mut AVec<'_, u8>, start: usize) -> Result<(), ExportError> {
    while (f.len() - start) % 8 != 0 {
        f.push(CFA_NOP);
    }
    let len = u32_of((f.len() - start - 4) as u64, "an eh_frame entry's length")?;
    f[start..start + 4].copy_from_slice(&len.to_le_bytes());
    Ok(())
}

/// Appends a CIE: id 0, version 1, the augmentation (`zPLR` with a personality, else `zR`),
/// code factor 4, data factor -8, return address `x30`, the augmentation data, and the frame
/// address `sp + 0` at a function's entry.
fn cie(f: &mut AVec<'_, u8>, personality: Option<u64>, patches: &mut AVec<'_, (usize, PatchTo)>) -> Result<(), ExportError> {
    let start = f.len();
    f.extend_from_slice(&[0; 8]);
    f.push(1);
    match personality {
        Some(_) => f.extend_from_slice(b"zPLR\0"),
        None => f.extend_from_slice(b"zR\0"),
    }
    eh::uleb(f, CODE_ALIGN);
    eh::sleb(f, DATA_ALIGN);
    eh::uleb(f, u64::from(reg::LR));
    match personality {
        Some(routine) => {
            // P: its encoding and the pointer; L: the FDEs' LSDA encoding; R: their pc encoding.
            eh::uleb(f, 1 + 4 + 1 + 1);
            f.push(PCREL_SDATA4);
            patches.push((f.len(), PatchTo::Image(routine)));
            f.extend_from_slice(&[0; 4]);
            f.push(PCREL_SDATA4);
            f.push(PCREL_SDATA4);
        }
        None => {
            eh::uleb(f, 1);
            f.push(PCREL_SDATA4);
        }
    }
    f.push(CFA_DEF_CFA);
    eh::uleb(f, u64::from(reg::SP));
    eh::uleb(f, 0);
    finish(f, start)
}

/// The tables for `covered` (sorted by address), the personality routine at `personality_at`,
/// the first table at `at` ([`TABLES_ALIGN`]-aligned).
fn tables<'a>(
    covered: &[Covered<'_>],
    personality_at: Option<u64>,
    at: u64,
    platform: Platform,
    outside: Option<&Outside<'_, '_>>,
    alloc: Alloc<'a>,
) -> Result<EhSections<'a>, ExportError> {
    // The LSDAs first: their bytes do not depend on where anything is.
    let mut except: AVec<'a, u8> = Vec::new_in(alloc);
    let mut given: AVec<'_, Given> = Vec::with_capacity_in(covered.len(), alloc);
    let mut landing = 0u32;
    for c in covered {
        let lsda = match c.unwind.frame().is_some() && c.unwind.lands() {
            false => None,
            true => {
                if personality_at.is_none() {
                    return Err(ExportError::NoPersonality { symbol: c.symbol });
                }
                pad(&mut except, 4);
                let offset = except.len() as u64;
                eh::lsda_table(&c.unwind, &mut except).map_err(|error| ExportError::Lsda { symbol: c.symbol, error })?;
                landing += 1;
                Some(offset)
            }
        };
        given.push(Given { fde: None, lsda });
    }

    // `eh_frame`, its pc-relative fields filled once every table's place is known.
    let mut f: AVec<'a, u8> = Vec::new_in(alloc);
    let mut patches: AVec<'_, (usize, PatchTo)> = Vec::new_in(alloc);
    let plain_cie = f.len();
    cie(&mut f, None, &mut patches)?;
    let landing_cie = match (landing, personality_at) {
        (0, _) | (_, None) => plain_cie,
        (_, Some(routine)) => {
            let offset = f.len();
            cie(&mut f, Some(routine), &mut patches)?;
            offset
        }
    };
    let mut functions = 0u32;
    for (k, c) in covered.iter().enumerate() {
        let Some(frame) = c.unwind.frame() else {
            continue;
        };
        let start = f.len();
        f.extend_from_slice(&[0; 4]);
        let cie_at = match given[k].lsda {
            Some(_) => landing_cie,
            None => plain_cie,
        };
        let back = u32_of((f.len() - cie_at) as u64, "an FDE's CIE pointer")?;
        f.extend_from_slice(&back.to_le_bytes());
        patches.push((f.len(), PatchTo::Image(c.at)));
        f.extend_from_slice(&[0; 4]);
        f.extend_from_slice(&c.unwind.code_len().to_le_bytes());
        match given[k].lsda {
            Some(lsda) => {
                eh::uleb(&mut f, 4);
                patches.push((f.len(), PatchTo::Lsda(lsda)));
                f.extend_from_slice(&[0; 4]);
            }
            None => eh::uleb(&mut f, 0),
        }
        eh::frame_program(frame, &mut f);
        finish(&mut f, start)?;
        given[k].fde = Some(u32_of(start as u64, "an FDE's offset in eh_frame")?);
        functions += 1;
    }
    // The native image's CIEs and FDEs, after Rust's, before the one terminator.
    let native_at = f.len();
    if let Some(o) = outside {
        let (_, bytes) = o.eh_frame()?;
        f.extend_from_slice(bytes);
        functions += o.image.frames.len() as u32;
    }
    f.extend_from_slice(&[0; 4]);

    let eh_frame_at = at;
    let except_tab_at = up(eh_frame_at + f.len() as u64, TABLE_ALIGN);
    let index_at = up(except_tab_at + except.len() as u64, TABLE_ALIGN);
    for &(field, to) in patches.iter() {
        let target = match to {
            PatchTo::Image(image) => image,
            PatchTo::Lsda(lsda) => except_tab_at + lsda,
        };
        let bytes = rel32(target, eh_frame_at + field as u64, "a pc-relative pointer in eh_frame")?;
        f[field..field + 4].copy_from_slice(&bytes);
    }
    // The image's pc-relative fields: an address inside its eh_frame moved with the copy, any
    // other with the image (`shift`); each field is the difference of the two, re-encoded.
    if let Some(o) = outside {
        let (was, bytes) = o.eh_frame()?;
        let len = bytes.len() as u64;
        let now = eh_frame_at + native_at as u64;
        let map = |x: u64| if x >= was && x < was + len { now + (x - was) } else { x + o.shift };
        for p in o.image.eh_pointers.iter() {
            let field = native_at + (p.at - was) as usize;
            let value = map(p.to).wrapping_sub(map(p.minus));
            match p.width {
                4 => {
                    let v = i32::try_from(value as i64)
                        .map_err(|_| ExportError::TooLarge { what: "a native eh_frame pointer past 32 bits once moved", len: value })?;
                    f[field..field + 4].copy_from_slice(&v.to_le_bytes());
                }
                _ => f[field..field + 8].copy_from_slice(&value.to_le_bytes()),
            }
        }
    }

    let mut unwind_info: AVec<'a, u8> = Vec::new_in(alloc);
    let mut eh_frame_hdr: AVec<'a, u8> = Vec::new_in(alloc);
    match platform {
        Platform::MacosArm64 => write_unwind_info(covered, &given, outside, native_at, alloc, &mut unwind_info)?,
        Platform::LinuxArm64 => write_eh_frame_hdr(covered, &given, outside, native_at, eh_frame_at, index_at, functions, &mut eh_frame_hdr)?,
        Platform::Wasm64 => return Err(ExportError::Wasm64RequiresWasmProgram),
    }
    Ok(EhSections {
        platform,
        eh_frame_at,
        eh_frame: f,
        except_tab_at,
        except_tab: except,
        unwind_info_at: index_at,
        unwind_info,
        eh_frame_hdr_at: index_at,
        eh_frame_hdr,
        functions,
        landing,
    })
}

/// The `unwind_info` encoding of one function (the module's doc says which form when).
fn encoding(c: &Covered<'_>, given: Given) -> Result<u32, ExportError> {
    let Some(fde) = given.fde else {
        return Ok(0);
    };
    let compact_word = match given.lsda {
        Some(_) => None,
        None => c.unwind.frame().map(compact),
    };
    match compact_word {
        Some(word) => Ok(word),
        None if fde & !DWARF_OFFSET == 0 => Ok(COMPACT_DWARF | fde),
        None => Err(ExportError::TooLarge { what: "an FDE's offset in __unwind_info's DWARF mode (24 bits)", len: u64::from(fde) }),
    }
}

/// Mach-O's one `__unwind_info`, by blaze-native's writer (the one builder of the format, rule
/// 30): a row per placed Rust function, reaching the next one's start (so the padding between
/// two needs no entry of its own), then the native image's rows moved by its `shift`, their
/// DWARF-mode offsets moved by where its FDEs sit in the one eh_frame (`native_at`), their
/// personalities (GOT cells) and LSDAs by `shift`.
fn write_unwind_info(
    covered: &[Covered<'_>],
    given: &[Given],
    outside: Option<&Outside<'_, '_>>,
    native_at: usize,
    alloc: Alloc<'_>,
    out: &mut AVec<'_, u8>,
) -> Result<(), ExportError> {
    let native_rows = outside.map_or(0, |o| o.image.unwind.len());
    let mut rows: AVec<'_, Row> = Vec::with_capacity_in(covered.len() + native_rows, alloc);
    for (k, c) in covered.iter().enumerate() {
        let end = match covered.get(k + 1) {
            Some(next) => next.at,
            None => c.at + u64::from(c.unwind.code_len()),
        };
        let length = u32_of(end - c.at, "a function's length in __unwind_info")?;
        rows.push(Row { start: c.at, length, encoding: encoding(c, given[k])?, personality: None, lsda: None });
    }
    if let Some(o) = outside {
        let moved = u32_of(native_at as u64, "the native FDEs' offset in eh_frame")?;
        for r in o.image.unwind.iter() {
            let mut encoding = r.encoding;
            if encoding & MODE_MASK == COMPACT_DWARF {
                let fde = (encoding & DWARF_OFFSET) + moved;
                if fde & !DWARF_OFFSET != 0 {
                    return Err(ExportError::TooLarge { what: "an FDE's offset in __unwind_info's DWARF mode (24 bits)", len: u64::from(fde) });
                }
                encoding = (encoding & !DWARF_OFFSET) | fde;
            }
            rows.push(Row {
                start: r.start + o.shift,
                length: r.length,
                encoding,
                personality: r.personality.map(|cell| cell + o.shift),
                lsda: r.lsda.map(|lsda| lsda + o.shift),
            });
        }
    }
    let len = native_unwind::unwind_info_len(&rows).map_err(|error| ExportError::NativeUnwind { error })?;
    out.try_reserve_exact(len).map_err(|_| ExportError::TooLarge { what: "__unwind_info's bytes (allocation)", len: len as u64 })?;
    native_unwind::unwind_info(&rows, out).map_err(|error| ExportError::NativeUnwind { error })
}

/// ELF's `.eh_frame_hdr` at image offset `hdr_at`, for the `functions` FDEs of `eh_frame` at
/// `eh_frame_at` (both the program's FDEs and any merged native image's FDEs, sorted by initial
/// location as LSB 5.0 requires for binary search).
#[allow(clippy::too_many_arguments)]
fn write_eh_frame_hdr(
    covered: &[Covered<'_>],
    given: &[Given],
    outside: Option<&Outside<'_, '_>>,
    native_at: usize,
    eh_frame_at: u64,
    hdr_at: u64,
    functions: u32,
    out: &mut AVec<'_, u8>,
) -> Result<(), ExportError> {
    out.push(1);
    out.push(PCREL_SDATA4);
    out.push(UDATA4);
    out.push(DATAREL_SDATA4);
    out.extend_from_slice(&rel32(eh_frame_at, hdr_at + 4, "eh_frame_hdr's eh_frame_ptr")?);
    out.extend_from_slice(&functions.to_le_bytes());
    let table_start = out.len();
    for (k, c) in covered.iter().enumerate() {
        let Some(fde) = given[k].fde else {
            continue;
        };
        out.extend_from_slice(&rel32(c.at, hdr_at, "an initial location in eh_frame_hdr")?);
        out.extend_from_slice(&rel32(eh_frame_at + u64::from(fde), hdr_at, "an FDE address in eh_frame_hdr")?);
    }
    if let Some(o) = outside {
        let (was, _) = o.eh_frame()?;
        let native_off = u64::try_from(native_at).map_err(|_| ExportError::TooLarge { what: "native eh_frame offset", len: u64::MAX })?;
        let native_base = eh_frame_at + native_off;
        for frame in o.image.frames.iter() {
            let fde_off = frame.at.checked_sub(was).ok_or(ExportError::Native(MergeError::NoRegion { region: NativeRegion::EhFrame }))?;
            let fde_at = native_base + fde_off;
            let initial = frame.start + o.shift;
            out.extend_from_slice(&rel32(initial, hdr_at, "a native initial location in eh_frame_hdr")?);
            out.extend_from_slice(&rel32(fde_at, hdr_at, "a native FDE address in eh_frame_hdr")?);
        }
        if let Some(slice) = out.get_mut(table_start..) {
            let (table, _) = slice.as_chunks_mut::<8>();
            table.sort_unstable_by_key(|pair| {
                (
                    i32::from_le_bytes([pair[0], pair[1], pair[2], pair[3]]),
                    i32::from_le_bytes([pair[4], pair[5], pair[6], pair[7]]),
                )
            });
        }
    }
    Ok(())
}

/// The unwind rows of `symbol`'s record, found in `functions` through `by_symbol` (their
/// indices sorted by symbol), every row checked by blaze-code's reader.
fn read_rows<'r>(functions: &[Member<'r>], by_symbol: &[u32], symbol: Symbol) -> Result<Unwind<'r>, ExportError> {
    let Ok(k) = by_symbol.binary_search_by(|&i| functions[i as usize].symbol.cmp(&symbol)) else {
        return Err(ExportError::EhNoRecord { symbol });
    };
    let record = functions[by_symbol[k] as usize].record;
    unwind::read(record).map_err(|error| ExportError::UnwindRows { symbol, error })
}

/// The bytes the tables of `functions` take at most, measured before layout: every function's
/// tables written as if laid end to end, plus what alignment can add. `lay_out` keeps this many
/// bytes after the code.
pub fn eh_room<'a>(functions: &[Member<'_>], personality: Option<Symbol>, platform: Platform, alloc: Alloc<'a>) -> Result<u64, ExportError> {
    eh_room_with(functions, personality, platform, None, alloc)
}

/// [`eh_room`] with the program's native image (native_merge.rs): its eh_frame bytes and rows
/// join the measure, the image at a stand-in place after the code (the size depends on the
/// rows' count, not their addresses).
pub fn eh_room_with<'a>(functions: &[Member<'_>], personality: Option<Symbol>, platform: Platform, natives: Option<&Image<'_>>, alloc: Alloc<'a>) -> Result<u64, ExportError> {
    let mut covered: AVec<'_, Covered<'_>> = Vec::with_capacity_in(functions.len(), alloc);
    let mut at = 0u64;
    for f in functions {
        let unwind = unwind::read(f.record).map_err(|error| ExportError::UnwindRows { symbol: f.symbol, error })?;
        at = up(at, 16);
        covered.push(Covered { symbol: f.symbol, at, unwind });
        at += u64::from(unwind.code_len());
    }
    let base = up(at, TABLES_ALIGN);
    let personality_at = personality.map(|_| 0);
    let page = match platform {
        Platform::MacosArm64 | Platform::Wasm64 => NATIVE_PAGE,
        Platform::LinuxArm64 => crate::elf::PAGE,
    };
    let standin = natives.map(|image| Outside { image, shift: up(base, page) + page });
    let measured = tables(&covered, personality_at, base, platform, standin.as_ref(), alloc)?;
    Ok(measured.end() - base + ROOM_SLACK)
}

/// The tables written for `laid` at their place after the code, refused unless they end within
/// the room [`eh_room`] measured (`laid.tables_room`): the second pass's check, in every build,
/// against the measure itself rather than the page `lay_out` rounded it up to (which would hide
/// a measure that came out short by less than the rounding).
fn within_room<'a>(laid: &Laid<'_, '_>, tables: EhSections<'a>) -> Result<EhSections<'a>, ExportError> {
    let need = tables.end() - laid.text_end();
    if need > laid.tables_room || tables.end() > laid.const_at {
        return Err(ExportError::EhRoom { need, room: laid.tables_room.min(laid.const_at - laid.text_end()) });
    }
    Ok(tables)
}

/// The tables of the laid-out program: one entry per placed function that is not folded (a
/// folded function's code, and so its rows, are its representative's), placed after the code
/// at the next 8-aligned offset, in the room [`eh_room`] measured. `functions` are the members
/// `laid` was made of; `personality` is the program's `eh_personality` routine, one of them.
pub fn eh_sections<'a>(laid: &Laid<'_, '_>, functions: &[Member<'_>], personality: Option<Symbol>, platform: Platform, alloc: Alloc<'a>) -> Result<EhSections<'a>, ExportError> {
    eh_sections_with(laid, functions, personality, platform, None, alloc)
}

/// [`eh_sections`] with the program's native image placed by native_merge.rs (`outside`): one
/// `__eh_frame` and one `__unwind_info` over the Rust functions and the image (main's ruling (a)).
pub fn eh_sections_with<'a>(
    laid: &Laid<'_, '_>,
    functions: &[Member<'_>],
    personality: Option<Symbol>,
    platform: Platform,
    outside: Option<&Outside<'_, '_>>,
    alloc: Alloc<'a>,
) -> Result<EhSections<'a>, ExportError> {
    let personality_at = match personality {
        None => None,
        Some(routine) => {
            let mut at = None;
            for p in laid.placed.iter() {
                if p.symbol == routine {
                    at = Some(p.at);
                    break;
                }
            }
            match at {
                Some(at) => Some(at),
                None => return Err(ExportError::PersonalityNotPlaced { personality: routine }),
            }
        }
    };
    let mut by_symbol: AVec<'_, u32> = Vec::with_capacity_in(functions.len(), alloc);
    for i in 0..functions.len() as u32 {
        by_symbol.push(i);
    }
    by_symbol.sort_unstable_by(|&a, &b| functions[a as usize].symbol.cmp(&functions[b as usize].symbol));
    let mut covered: AVec<'_, Covered<'_>> = Vec::with_capacity_in(laid.placed.len(), alloc);
    for p in laid.placed.iter() {
        if p.folded {
            continue;
        }
        let unwind = read_rows(functions, &by_symbol, p.symbol)?;
        covered.push(Covered { symbol: p.symbol, at: p.at, unwind });
    }
    covered.sort_unstable_by_key(|c| c.at);
    let at = up(laid.text_end(), TABLES_ALIGN);
    within_room(laid, tables(&covered, personality_at, at, platform, outside, alloc)?)
}

/// [`eh_room`] over rows already made one `Unwind` each (`(symbol, rows)`, each symbol once, in
/// any order): what export at the optimized tier (optimized.rs) hands in, from `bzcode02` rows by
/// blaze-code's `unwind::of_rows`. The same measure as [`eh_room`].
pub fn eh_room_rows<'a>(functions: &[(Symbol, Unwind<'_>)], personality: Option<Symbol>, platform: Platform, alloc: Alloc<'a>) -> Result<u64, ExportError> {
    let mut covered: AVec<'_, Covered<'_>> = Vec::with_capacity_in(functions.len(), alloc);
    let mut at = 0u64;
    for &(symbol, unwind) in functions {
        at = up(at, 16);
        covered.push(Covered { symbol, at, unwind });
        at += u64::from(unwind.code_len());
    }
    let base = up(at, TABLES_ALIGN);
    let personality_at = personality.map(|_| 0);
    let measured = tables(&covered, personality_at, base, platform, None, alloc)?;
    Ok(measured.end() - base + ROOM_SLACK)
}

/// [`eh_sections`] over rows already made one `Unwind` each (`(symbol, rows)`, each symbol once,
/// in any order; [`eh_room_rows`] measured their room). The same tables as [`eh_sections`].
pub fn eh_sections_rows<'a>(
    laid: &Laid<'_, '_>,
    functions: &[(Symbol, Unwind<'_>)],
    personality: Option<Symbol>,
    platform: Platform,
    alloc: Alloc<'a>,
) -> Result<EhSections<'a>, ExportError> {
    let personality_at = match personality {
        None => None,
        Some(routine) => match laid.placed.iter().find(|p| p.symbol == routine) {
            Some(p) => Some(p.at),
            None => return Err(ExportError::PersonalityNotPlaced { personality: routine }),
        },
    };
    let mut by_symbol: AVec<'_, u32> = Vec::with_capacity_in(functions.len(), alloc);
    for i in 0..functions.len() as u32 {
        by_symbol.push(i);
    }
    by_symbol.sort_unstable_by(|&a, &b| functions[a as usize].0.cmp(&functions[b as usize].0));
    let mut covered: AVec<'_, Covered<'_>> = Vec::with_capacity_in(laid.placed.len(), alloc);
    for p in laid.placed.iter() {
        if p.folded {
            continue;
        }
        let Ok(k) = by_symbol.binary_search_by(|&i| functions[i as usize].0.cmp(&p.symbol)) else {
            return Err(ExportError::EhNoRecord { symbol: p.symbol });
        };
        covered.push(Covered { symbol: p.symbol, at: p.at, unwind: functions[by_symbol[k] as usize].1 });
    }
    covered.sort_unstable_by_key(|c| c.at);
    let at = up(laid.text_end(), TABLES_ALIGN);
    within_room(laid, tables(&covered, personality_at, at, platform, None, alloc)?)
}

#[cfg(test)]
#[path = "eh_out_tests.rs"]
mod tests;
