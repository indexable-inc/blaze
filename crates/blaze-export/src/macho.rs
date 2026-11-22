//! The Mach-O writer: a laid-out program as one arm64 macOS executable (`MH_EXECUTE`, `MH_PIE`),
//! bound by dyld through chained fixups and signed ad hoc. Every structure is
//! `<mach-o/loader.h>`'s (`mach_header_64`, `segment_command_64`, `section_64`,
//! `linkedit_data_command`, `symtab_command`, `dysymtab_command`, `dylinker_command`,
//! `uuid_command`, `build_version_command`, `entry_point_command`, `dylib_command`), the
//! fixups' `<mach-o/fixup-chains.h>`'s (`dyld_chained_fixups_header`,
//! `dyld_chained_starts_in_image`, `dyld_chained_starts_in_segment`, `dyld_chained_import`,
//! `dyld_chained_ptr_64_rebase`, `dyld_chained_ptr_64_bind`), the symbols' `<mach-o/nlist.h>`'s
//! (`nlist_64`), the signature's [`crate::sign`].
//!
//! The file, in order (file offset = image offset for every region but `__LINKEDIT`):
//!
//! | segment | sections | protection | why |
//! |---|---|---|---|
//! | `__PAGEZERO` | none | none | the first 4 GiB unmapped, so a null pointer faults |
//! | `__TEXT` | `__text` | r-x | the header and load commands, then the code |
//! | `__DATA_CONST` | `__got`, `__const` | rw-, then r-- (`SG_READ_ONLY`) | C names' words, relro parts, read-only data |
//! | `__DATA` | `__data`, `__bss` | rw- | writable and zeroed statics |
//! | `__LINKEDIT` | none | r-- | the chained fixups, function starts, symbols, the signature last |
//!
//! Facts marked "to verify" are believed from the headers' documentation and are checked only
//! by the run test (`examples/export_smoke.rs`): that dyld accepts an executable with no
//! `LC_DYLD_EXPORTS_TRIE` and no `__mh_execute_header` symbol; that `DYLD_CHAINED_PTR_64_OFFSET`
//! with a minimum macOS of 12.0 is bound by every dyld that runs arm64 executables of that
//! minimum; that each `dyld_chained_starts_in_segment` may sit 8-aligned.

use blaze_ids::Hasher;
use blaze_own::{AVec, Alloc, Vec};

use crate::bytes;
use crate::eh_out::EhSections;
use crate::error::ExportError;
use crate::layout::{FixTo, Laid, Plan};
use crate::native_dylib::{DYLIB_VERSION, dylib_command};
use crate::native_merge::{Merged, NATIVE_DATA, NATIVE_TEXT, Sect, command_room};
use crate::plant::{self, Plant};
use crate::sign;

/// `__TEXT`'s address in an unslid image: past `__PAGEZERO`'s 4 GiB, as ld64 places it.
pub const BASE: u64 = 0x1_0000_0000;
pub const PAGE: u64 = 0x4000;

pub(crate) const MH_MAGIC_64: u32 = 0xFEED_FACF;
pub(crate) const CPU_TYPE_ARM64: u32 = 0x0100_000C;
const MH_EXECUTE: u32 = 2;
/// `MH_NOUNDEFS | MH_DYLDLINK | MH_TWOLEVEL | MH_PIE`.
const MH_FLAGS: u32 = 0x1 | 0x4 | 0x80 | 0x20_0000;

const LC_SEGMENT_64: u32 = 0x19;
pub(crate) const LC_SYMTAB: u32 = 0x2;
pub(crate) const LC_DYSYMTAB: u32 = 0xB;
pub(crate) const LC_LOAD_DYLIB: u32 = 0xC;
const LC_LOAD_DYLINKER: u32 = 0xE;
pub(crate) const LC_UUID: u32 = 0x1B;
pub(crate) const LC_CODE_SIGNATURE: u32 = 0x1D;
pub(crate) const LC_FUNCTION_STARTS: u32 = 0x26;
pub(crate) const LC_BUILD_VERSION: u32 = 0x32;
const LC_MAIN: u32 = 0x8000_0028;
pub(crate) const LC_DYLD_CHAINED_FIXUPS: u32 = 0x8000_0034;

pub(crate) const VM_PROT_READ: u32 = 1;
pub(crate) const VM_PROT_WRITE: u32 = 2;
pub(crate) const VM_PROT_EXECUTE: u32 = 4;
/// `SG_READ_ONLY`: dyld makes the segment read-only once its fixups are done.
pub(crate) const SG_READ_ONLY: u32 = 0x10;

/// `S_ATTR_PURE_INSTRUCTIONS | S_ATTR_SOME_INSTRUCTIONS`.
pub(crate) const S_CODE: u32 = 0x8000_0000 | 0x400;
pub(crate) const S_REGULAR: u32 = 0;
/// ld64's `__eh_frame` flags: `S_COALESCED | S_ATTR_NO_TOC | S_ATTR_STRIP_STATIC_SYMS |
/// S_ATTR_LIVE_SUPPORT`.
pub(crate) const S_EH_FRAME: u32 = 0x6800_000B;
pub(crate) const S_ZEROFILL: u32 = 0x1;
const S_NON_LAZY_SYMBOL_POINTERS: u32 = 0x6;

pub(crate) const PLATFORM_MACOS: u32 = 1;
/// macOS 12.0 (`xxxx.yy.zz` in nibbles): the first whose dyld binds every executable through
/// chained fixups of `DYLD_CHAINED_PTR_64_OFFSET`.
const MIN_MACOS: u32 = 0x000C_0000;

const DYLD: &str = "/usr/lib/dyld";
pub(crate) const LIB_SYSTEM: &str = "/usr/lib/libSystem.B.dylib";
/// libSystem's versions as a `dylib_command` names them: current 1311.0.0, compatibility 1.0.0.
pub(crate) const LIB_SYSTEM_CURRENT: u32 = 1311 << 16;
pub(crate) const LIB_SYSTEM_COMPAT: u32 = 1 << 16;

/// `DYLD_CHAINED_PTR_64_OFFSET`: a rebase's target is an image offset.
const DYLD_CHAINED_PTR_64_OFFSET: u16 = 6;
/// `DYLD_CHAINED_IMPORT`.
const DYLD_CHAINED_IMPORT: u32 = 1;
const DYLD_CHAINED_PTR_START_NONE: u16 = 0xFFFF;
/// A chain's `next` counts 4-byte strides.
const STRIDE: u64 = 4;

/// `N_EXT` with `N_UNDF`: an undefined external symbol.
const N_UNDEF_EXT: u8 = 0x01;
/// `SET_LIBRARY_ORDINAL(n_desc, 1)`: libSystem, the first `LC_LOAD_DYLIB`.
const LIBSYSTEM_ORDINAL: u16 = 1 << 8;

pub(crate) const SEGMENT: u64 = 72;
pub(crate) const SECTION: u64 = 80;
/// Every load command at its largest: the header's room is fixed, so the code's place does not
/// depend on which optional segments a program has.
const MAX_COMMANDS: u64 = SEGMENT // __PAGEZERO
    + SEGMENT + 4 * SECTION // __TEXT: __text, __eh_frame, __gcc_except_tab, __unwind_info
    + SEGMENT + 2 * SECTION // __DATA_CONST
    + SEGMENT + 2 * SECTION // __DATA
    + SEGMENT // __LINKEDIT
    + 16 // LC_DYLD_CHAINED_FIXUPS
    + 24 // LC_SYMTAB
    + 80 // LC_DYSYMTAB
    + 32 // LC_LOAD_DYLINKER
    + 24 // LC_UUID
    + 24 // LC_BUILD_VERSION
    + 24 // LC_MAIN
    + 56 // LC_LOAD_DYLIB
    + 16 // LC_FUNCTION_STARTS
    + 16; // LC_CODE_SIGNATURE
pub(crate) const HEADER: u64 = 32;
/// Where the code starts: past the header and every load command, 16-aligned.
pub const TEXT_AT: u64 = (HEADER + MAX_COMMANDS + 15) & !15;

const UUID_DOMAIN: &str = "blaze-export macho uuid 1";

/// The layout plan a Mach-O file needs.
pub fn plan() -> Plan {
    plan_with(0)
}

/// [`plan`] with `extra` bytes of load commands in the header's room (a native image's,
/// native_merge.rs `command_room`): the code's place depends on the image, never on the rest.
pub fn plan_with(extra: u64) -> Plan {
    Plan { page: PAGE, text_at: (TEXT_AT + extra + 15) & !15, const_head: 0 }
}

fn up(n: u64, to: u64) -> u64 {
    (n + to - 1) & !(to - 1)
}

/// A segment as the load commands and the chained fixups see it.
#[derive(Clone, Copy)]
struct Seg {
    name: &'static str,
    /// Image offset (distance from `__TEXT`); `__PAGEZERO`'s is below the image.
    vm: u64,
    vm_size: u64,
    file: u64,
    file_size: u64,
    prot: u32,
    flags: u32,
    /// The fixups of `[vm, vm + vm_size)`.
    fixups: bool,
}

/// What a chained slot becomes, in neither writer's own terms: the executable's [`FixTo`] and
/// the native dylib's (`native_dylib.rs`, `blaze_native::image::FixTo`) both map to it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Chain {
    /// The image's runtime address plus `target`, an image offset.
    Rebase { target: u64 },
    /// The address import row `import` names.
    Bind { import: u32 },
}

/// A segment as `dyld_chained_starts_in_image` counts it: every segment in load-command order,
/// `fixups` set on those whose `[vm, vm + vm_size)` (image offsets) holds chained slots.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct ChainSeg {
    pub vm: u64,
    pub vm_size: u64,
    pub fixups: bool,
}

/// The import table's row form (`dyld_chained_fixups_header::imports_format`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ImportRows {
    /// `DYLD_CHAINED_IMPORT`: 32-bit rows, an 8-bit ordinal, a weak bit, a 23-bit name offset.
    Plain,
    /// `DYLD_CHAINED_IMPORT_ADDEND64`: 16-byte rows, a 16-bit ordinal, a weak bit, a 32-bit name
    /// offset, a 64-bit addend.
    Addend64,
}

/// `DYLD_CHAINED_IMPORT_ADDEND64`.
const DYLD_CHAINED_IMPORT_ADDEND64: u32 = 3;

/// One import row; its symbol name in the pool is `lead` then `name`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct ChainImport<'n> {
    pub lead: &'static [u8],
    pub name: &'n [u8],
    pub ordinal: u16,
    pub weak: bool,
    pub addend: i64,
}

/// A value too wide for the chained form's field `what`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct ChainTooLarge {
    pub what: &'static str,
    pub len: u64,
    /// The slot's image offset, when the value is a slot's (a table's field has none).
    pub at: Option<u64>,
}

/// The chained fixups blob (`dyld_chained_fixups_header`, the starts, the import rows, the
/// symbol pool), and each slot's encoded `DYLD_CHAINED_PTR_64_OFFSET` value by file offset.
/// `fixups` are sorted by address, `slot` reads one as (image offset, what it becomes); `import`
/// gives row `i` of `imports`. Shared by the executable ([`write`]) and the native dylib
/// (`native_dylib.rs`): one encoder of the format.
pub(crate) fn chained_fixups<'a, 'n, F>(
    fixups: &[F],
    slot: impl Fn(&F) -> (u64, Chain),
    segs: &[ChainSeg],
    rows: ImportRows,
    imports: usize,
    import: impl Fn(usize) -> ChainImport<'n>,
    alloc: Alloc<'a>,
) -> Result<(AVec<'a, u8>, AVec<'a, (u64, u64)>), ChainTooLarge> {
    let mut slots: AVec<'a, (u64, u64)> = Vec::with_capacity_in(fixups.len(), alloc);
    for (k, f) in fixups.iter().enumerate() {
        let (at, to) = slot(f);
        let page_of = |at: u64| at / PAGE;
        let next = match fixups.get(k + 1).map(&slot) {
            Some((n, _)) if page_of(n) == page_of(at) => (n - at) / STRIDE,
            _ => 0,
        };
        let value = match to {
            // `dyld_chained_ptr_64_rebase`: target (36 bits), high8 (8), reserved (7), next (12),
            // bind (1); dyld makes `header + (high8 << 56 | target)`. A pointer the program tags
            // in its top byte (C's `(uintptr_t)&x | 1ull << 63` in a static initializer) keeps
            // the tag in `high8`, as ld64 writes it; only bits 36..55 cannot be held.
            Chain::Rebase { target } => {
                if target & blaze_native::layout::REBASE_MIDDLE != 0 {
                    return Err(ChainTooLarge { what: "a chained rebase's target: bits 36..55 set (neither the 36-bit offset nor the top byte)", len: target, at: Some(at) });
                }
                (target & ((1 << 36) - 1)) | ((target >> 56) << 36) | (next << 51)
            }
            Chain::Bind { import } => {
                if import >= 1 << 24 {
                    return Err(ChainTooLarge { what: "a chained bind's 24-bit ordinal", len: u64::from(import), at: Some(at) });
                }
                u64::from(import) | (next << 51) | (1 << 63)
            }
        };
        slots.push((at, value));
    }

    let mut blob: AVec<'a, u8> = Vec::new_in(alloc);
    // `dyld_chained_fixups_header`, its offsets filled below.
    for _ in 0..7 {
        bytes::u32(&mut blob, 0);
    }
    bytes::pad(&mut blob, 8);
    let starts_at = blob.len();
    bytes::u32(&mut blob, segs.len() as u32);
    let infos_at = blob.len();
    for _ in segs {
        bytes::u32(&mut blob, 0);
    }
    for (s, seg) in segs.iter().enumerate() {
        if !seg.fixups {
            continue;
        }
        bytes::pad(&mut blob, 8);
        let at = blob.len();
        bytes::set32(&mut blob, infos_at + 4 * s, (at - starts_at) as u32);
        let pages = (seg.vm_size / PAGE) as usize;
        if pages > usize::from(u16::MAX) {
            return Err(ChainTooLarge { what: "a segment's 16-bit page count", len: seg.vm_size, at: None });
        }
        bytes::u32(&mut blob, (22 + 2 * pages) as u32);
        bytes::u16(&mut blob, PAGE as u16);
        bytes::u16(&mut blob, DYLD_CHAINED_PTR_64_OFFSET);
        bytes::u64(&mut blob, seg.vm);
        bytes::u32(&mut blob, 0); // max_valid_pointer: 64-bit
        bytes::u16(&mut blob, pages as u16);
        let first = blob.len();
        for _ in 0..pages {
            bytes::u16(&mut blob, DYLD_CHAINED_PTR_START_NONE);
        }
        // Each page's first fixup (fixups are sorted by address).
        for (at, _) in fixups.iter().map(&slot).filter(|&(at, _)| at >= seg.vm && at < seg.vm + seg.vm_size).rev() {
            let page = ((at - seg.vm) / PAGE) as usize;
            let offset = ((at - seg.vm) % PAGE) as u16;
            blob[first + 2 * page..first + 2 * page + 2].copy_from_slice(&offset.to_le_bytes());
        }
    }
    match rows {
        ImportRows::Plain => bytes::pad(&mut blob, 4),
        ImportRows::Addend64 => bytes::pad(&mut blob, 8),
    }
    let imports_at = blob.len();
    let mut name_at = 0u64;
    for i in 0..imports {
        let row = import(i);
        match rows {
            ImportRows::Plain => {
                if name_at >= 1 << 23 {
                    return Err(ChainTooLarge { what: "a chained import's 23-bit name offset", len: name_at, at: None });
                }
                if row.ordinal > 0xFF {
                    return Err(ChainTooLarge { what: "a DYLD_CHAINED_IMPORT row's 8-bit ordinal", len: u64::from(row.ordinal), at: None });
                }
                if row.addend != 0 {
                    return Err(ChainTooLarge { what: "an addend in a DYLD_CHAINED_IMPORT row, which holds none", len: row.addend as u64, at: None });
                }
                // `dyld_chained_import`: lib_ordinal, weak_import, name_offset.
                bytes::u32(&mut blob, u32::from(row.ordinal) | (u32::from(row.weak) << 8) | ((name_at as u32) << 9));
            }
            ImportRows::Addend64 => {
                if name_at > u64::from(u32::MAX) {
                    return Err(ChainTooLarge { what: "a chained import's 32-bit name offset", len: name_at, at: None });
                }
                // `dyld_chained_import_addend64`: lib_ordinal, weak_import, reserved, name_offset; addend.
                bytes::u64(&mut blob, u64::from(row.ordinal) | (u64::from(row.weak) << 16) | (name_at << 32));
                bytes::u64(&mut blob, row.addend as u64);
            }
        }
        name_at += (row.lead.len() + row.name.len() + 1) as u64;
    }
    let symbols_at = blob.len();
    for i in 0..imports {
        let row = import(i);
        blob.extend_from_slice(row.lead);
        blob.extend_from_slice(row.name);
        blob.push(0);
    }
    bytes::pad(&mut blob, 8);
    bytes::set32(&mut blob, 0, 0); // fixups_version
    bytes::set32(&mut blob, 4, starts_at as u32);
    bytes::set32(&mut blob, 8, imports_at as u32);
    bytes::set32(&mut blob, 12, symbols_at as u32);
    bytes::set32(&mut blob, 16, imports as u32);
    let format = match rows {
        ImportRows::Plain => DYLD_CHAINED_IMPORT,
        ImportRows::Addend64 => DYLD_CHAINED_IMPORT_ADDEND64,
    };
    bytes::set32(&mut blob, 20, format);
    bytes::set32(&mut blob, 24, 0); // symbols_format: not compressed
    Ok((blob, slots))
}

/// The chained fixups blob, and each fixup slot's encoded value by image offset: the
/// program's imports are libSystem's (ordinal 1), named with the platform's `_`; a native
/// image's (native_merge.rs) follow, by flat lookup, with their addends (`Addend64` rows then).
fn chained<'a>(laid: &Laid<'_, '_>, native: Option<&Merged<'_, '_>>, segs: &[Seg], alloc: Alloc<'a>) -> Result<(AVec<'a, u8>, AVec<'a, (u64, u64)>), ExportError> {
    let mut chain_segs: AVec<'a, ChainSeg> = Vec::with_capacity_in(segs.len(), alloc);
    for seg in segs {
        chain_segs.push(ChainSeg { vm: seg.vm, vm_size: seg.vm_size, fixups: seg.fixups });
    }
    // A program import the image defines is a rebase to it (`Merged::direct`); the rows left
    // keep their order (`kept`, and each import's new row in `row_of`).
    let direct = |import: u32| native.and_then(|m| m.direct.binary_search_by_key(&import, |&(row, _)| row).ok().map(|i| m.direct[i].1));
    let mut kept: AVec<'a, u32> = Vec::with_capacity_in(laid.imports.len(), alloc);
    let mut row_of: AVec<'a, u32> = Vec::with_capacity_in(laid.imports.len(), alloc);
    for k in 0..laid.imports.len() as u32 {
        row_of.push(kept.len() as u32);
        if direct(k).is_none() {
            kept.push(k);
        }
    }
    // The program's slots (all before its end), then the image's (all after it): sorted.
    let mut all: AVec<'a, (u64, Chain)> = Vec::with_capacity_in(laid.fixups.len() + native.map_or(0, |m| m.fixups.len()), alloc);
    for f in laid.fixups.iter() {
        let to = match f.to {
            FixTo::Rebase { target } => Chain::Rebase { target },
            FixTo::Bind { import } => match direct(import) {
                Some(target) => Chain::Rebase { target },
                None => Chain::Bind { import: row_of[import as usize] },
            },
        };
        all.push((f.at, to));
    }
    if let Some(m) = native {
        all.extend_from_slice(&m.fixups);
    }
    let program = kept.len();
    let import = |i: usize| match (i.checked_sub(program), native) {
        (Some(k), Some(m)) => m.imports[k],
        _ => ChainImport { lead: b"_", name: laid.imports[kept[i] as usize].name.as_bytes(), ordinal: 1, weak: false, addend: 0 },
    };
    let (rows, count) = match native {
        None => (ImportRows::Plain, program),
        Some(m) => (ImportRows::Addend64, program + m.imports.len()),
    };
    chained_fixups(&all, |c: &(u64, Chain)| *c, &chain_segs, rows, count, import, alloc)
        .map_err(|e| ExportError::TooLarge { what: e.what, len: e.len })
}

/// `segment_command_64` at address `vmaddr` (`maxprot` = `initprot` = `prot`), for the
/// executable ([`segment`]) and the native dylib (`native_dylib.rs`, whose `__TEXT` is at 0).
#[allow(clippy::too_many_arguments)]
pub(crate) fn segment_command(out: &mut AVec<'_, u8>, name: &str, vmaddr: u64, vm_size: u64, file: u64, file_size: u64, prot: u32, nsects: u32, flags: u32) {
    bytes::u32(out, LC_SEGMENT_64);
    bytes::u32(out, (SEGMENT + SECTION * u64::from(nsects)) as u32);
    bytes::name16(out, name);
    bytes::u64(out, vmaddr);
    bytes::u64(out, vm_size);
    bytes::u64(out, file);
    bytes::u64(out, file_size);
    bytes::u32(out, prot);
    bytes::u32(out, prot);
    bytes::u32(out, nsects);
    bytes::u32(out, flags);
}

fn segment(out: &mut AVec<'_, u8>, seg: &Seg, nsects: u32) {
    let vmaddr = if seg.name == "__PAGEZERO" { 0 } else { BASE + seg.vm };
    segment_command(out, seg.name, vmaddr, seg.vm_size, seg.file, seg.file_size, seg.prot, nsects, seg.flags);
}

/// `section_64` at address `addr`, `offset` its file offset (0 for zero fill).
#[allow(clippy::too_many_arguments)]
pub(crate) fn section_command(out: &mut AVec<'_, u8>, name: &str, seg: &str, addr: u64, size: u64, offset: u64, align: u32, flags: u32, reserved1: u32) {
    bytes::name16(out, name);
    bytes::name16(out, seg);
    bytes::u64(out, addr);
    bytes::u64(out, size);
    bytes::u32(out, offset as u32);
    bytes::u32(out, align);
    bytes::u32(out, 0); // reloff
    bytes::u32(out, 0); // nreloc
    bytes::u32(out, flags);
    bytes::u32(out, reserved1);
    bytes::u32(out, 0);
    bytes::u32(out, 0);
}

/// `section_64`: `addr` an image offset, `offset` its file offset (0 for zero fill).
#[allow(clippy::too_many_arguments)]
fn section(out: &mut AVec<'_, u8>, name: &str, seg: &str, addr: u64, size: u64, offset: u64, align: u32, flags: u32, reserved1: u32) {
    section_command(out, name, seg, BASE + addr, size, offset, align, flags, reserved1);
}

pub(crate) fn linkedit_data(out: &mut AVec<'_, u8>, cmd: u32, off: u64, size: u64) {
    bytes::u32(out, cmd);
    bytes::u32(out, 16);
    bytes::u32(out, off as u32);
    bytes::u32(out, size as u32);
}

/// Writes `laid` as a Mach-O executable into `out` (its contents replaced): the file, whole,
/// signed. `identifier` is the signature's; `seed` joins the content in the file's `LC_UUID`
/// (the profile's digest, or zeros). Returns the UUID.
pub fn write<'a>(laid: &Laid<'_, '_>, eh: &EhSections<'_>, identifier: &str, seed: &[u8; 32], out: &mut AVec<'a, u8>, alloc: Alloc<'a>) -> Result<[u8; 16], ExportError> {
    write_native(laid, eh, None, identifier, seed, out, alloc)
}

/// [`write`] with the program's native image merged (native_merge.rs): its two segments after
/// the program's, its libraries after libSystem, its fixups and import rows in the one chained
/// blob, `MH_HAS_TLV_DESCRIPTORS` when it has thread-locals. The header's room must be
/// `plan_with(command_room(..))`'s.
#[allow(clippy::too_many_arguments)]
pub fn write_native<'a>(
    laid: &Laid<'_, '_>,
    eh: &EhSections<'_>,
    native: Option<&Merged<'_, '_>>,
    identifier: &str,
    seed: &[u8; 32],
    out: &mut AVec<'a, u8>,
    alloc: Alloc<'a>,
) -> Result<[u8; 16], ExportError> {
    let has_const = !laid.konst.is_empty();
    let has_data = !laid.data.is_empty() || laid.zero_len > 0;
    // `__TEXT` holds the code and, after it, the unwind tables (eh_out.rs).
    let text_size = up(eh.end(), PAGE);
    let const_size = up(laid.konst.len() as u64, PAGE);
    let data_file = up(laid.data.len() as u64, PAGE);
    let mut file_end = text_size;
    let mut segs: AVec<'_, Seg> = Vec::new_in(alloc);
    let none = 0;
    segs.push(Seg { name: "__PAGEZERO", vm: 0, vm_size: BASE, file: 0, file_size: 0, prot: none, flags: 0, fixups: false });
    segs.push(Seg { name: "__TEXT", vm: 0, vm_size: text_size, file: 0, file_size: text_size, prot: VM_PROT_READ | VM_PROT_EXECUTE, flags: 0, fixups: false });
    if has_const {
        segs.push(Seg {
            name: "__DATA_CONST",
            vm: laid.const_at,
            vm_size: const_size,
            file: laid.const_at,
            file_size: const_size,
            prot: VM_PROT_READ | VM_PROT_WRITE,
            flags: SG_READ_ONLY,
            fixups: true,
        });
        file_end = laid.const_at + const_size;
    }
    if has_data {
        let (file, file_size) = match laid.data.is_empty() {
            true => (0, 0),
            false => (laid.data_at, data_file),
        };
        segs.push(Seg {
            name: "__DATA",
            vm: laid.data_at,
            vm_size: laid.end - laid.data_at,
            file,
            file_size,
            prot: VM_PROT_READ | VM_PROT_WRITE,
            flags: 0,
            fixups: true,
        });
        if file_size > 0 {
            file_end = laid.data_at + data_file;
        }
    }
    // The native image: its code segment, then its data segment, after every region of the
    // program; in the file, on the page after the program's file bytes.
    let mut vm_end = laid.end;
    let mut native_file = 0u64;
    if let Some(m) = native {
        native_file = up(file_end, PAGE);
        let code = m.data_at - m.text_at;
        segs.push(Seg {
            name: NATIVE_TEXT,
            vm: m.text_at,
            vm_size: code,
            file: native_file,
            file_size: code,
            prot: VM_PROT_READ | VM_PROT_EXECUTE,
            flags: 0,
            fixups: false,
        });
        let data_file = up(m.file_end.max(m.data_at) - m.data_at, PAGE);
        segs.push(Seg {
            name: NATIVE_DATA,
            vm: m.data_at,
            vm_size: m.end - m.data_at,
            file: native_file + code,
            file_size: data_file,
            prot: VM_PROT_READ | VM_PROT_WRITE,
            flags: 0,
            fixups: true,
        });
        file_end = native_file + code + data_file;
        vm_end = m.end;
    }
    // `__LINKEDIT`'s pieces, at offsets from its start.
    let (fixups_blob, slots) = chained(laid, native, &segs_with_linkedit(&segs, vm_end, alloc), alloc)?;
    let mut link: AVec<'_, u8> = Vec::new_in(alloc);
    let link_off = file_end;
    let fixups_at = link.len() as u64;
    link.extend_from_slice(&fixups_blob);
    bytes::pad(&mut link, 8);
    let starts_at = link.len() as u64;
    let mut homes: AVec<'_, u64> = Vec::with_capacity_in(laid.placed.len(), alloc);
    for p in laid.placed.iter() {
        homes.push(p.at);
    }
    homes.sort_unstable();
    homes.dedup();
    let mut last = 0u64;
    for &home in homes.iter() {
        bytes::uleb(&mut link, home - last);
        last = home;
    }
    link.push(0);
    bytes::pad(&mut link, 8);
    let starts_len = link.len() as u64 - starts_at;
    let symtab_at = link.len() as u64;
    let n = laid.imports.len();
    let mut strx = 2u32;
    for import in laid.imports.iter() {
        // `nlist_64`: n_strx, n_type, n_sect, n_desc, n_value.
        bytes::u32(&mut link, strx);
        bytes::u8(&mut link, N_UNDEF_EXT);
        bytes::u8(&mut link, 0);
        bytes::u16(&mut link, LIBSYSTEM_ORDINAL);
        bytes::u64(&mut link, 0);
        strx += import.name.len() as u32 + 2;
    }
    let indirect_at = link.len() as u64;
    for k in 0..n {
        bytes::u32(&mut link, k as u32);
    }
    let strtab_at = link.len() as u64;
    link.extend_from_slice(b" \0");
    for import in laid.imports.iter() {
        link.push(b'_');
        link.extend_from_slice(import.name.as_bytes());
        link.push(0);
    }
    bytes::pad(&mut link, 8);
    let strtab_len = link.len() as u64 - strtab_at;
    bytes::pad(&mut link, 16);
    let code_limit = link_off + link.len() as u64;
    let sig_len = sign::len(identifier, code_limit);
    let link_len = link.len() as u64 + sig_len;
    let link_seg = Seg {
        name: "__LINKEDIT",
        vm: vm_end,
        vm_size: up(link_len, PAGE),
        file: link_off,
        file_size: link_len,
        prot: VM_PROT_READ,
        flags: 0,
        fixups: false,
    };
    segs.push(link_seg);

    // The header and load commands.
    out.clear();
    for _ in 0..8 {
        bytes::u32(out, 0);
    }
    let got_size = 8 * n as u64;
    for seg in segs.iter() {
        match seg.name {
            "__TEXT" => {
                segment(out, seg, 4);
                section(out, "__text", "__TEXT", laid.text_at, laid.text.len() as u64, laid.text_at, 4, S_CODE, 0);
                section(out, "__eh_frame", "__TEXT", eh.eh_frame_at, eh.eh_frame.len() as u64, eh.eh_frame_at, 3, S_EH_FRAME, 0);
                section(out, "__gcc_except_tab", "__TEXT", eh.except_tab_at, eh.except_tab.len() as u64, eh.except_tab_at, 2, S_REGULAR, 0);
                section(out, "__unwind_info", "__TEXT", eh.unwind_info_at, eh.unwind_info.len() as u64, eh.unwind_info_at, 2, S_REGULAR, 0);
            }
            "__DATA_CONST" => {
                let rest_at = laid.got_at + got_size;
                let rest = laid.const_end() - rest_at;
                let nsects = u32::from(n > 0) + u32::from(rest > 0);
                segment(out, seg, nsects);
                if n > 0 {
                    section(out, "__got", "__DATA_CONST", laid.got_at, got_size, laid.got_at, 3, S_NON_LAZY_SYMBOL_POINTERS, 0);
                }
                if rest > 0 {
                    section(out, "__const", "__DATA_CONST", rest_at, rest, rest_at, 4, S_REGULAR, 0);
                }
            }
            "__DATA" => {
                let data = laid.data.len() as u64;
                let nsects = u32::from(data > 0) + u32::from(laid.zero_len > 0);
                segment(out, seg, nsects);
                if data > 0 {
                    section(out, "__data", "__DATA", laid.data_at, data, laid.data_at, 4, S_REGULAR, 0);
                }
                if laid.zero_len > 0 {
                    section(out, "__bss", "__DATA", laid.zero_at, laid.zero_len, 0, 4, S_ZEROFILL, 0);
                }
            }
            NATIVE_TEXT | NATIVE_DATA => {
                let sects: &[Sect] = match native {
                    Some(m) if seg.name == NATIVE_TEXT => &m.code,
                    Some(m) => &m.data,
                    None => &[],
                };
                segment(out, seg, sects.len() as u32);
                for s in sects {
                    let offset = if s.zero { 0 } else { s.at - seg.vm + seg.file };
                    section(out, s.name, seg.name, s.at, s.len, offset, s.align_log2, s.flags, 0);
                }
            }
            _ => segment(out, seg, 0),
        }
    }
    linkedit_data(out, LC_DYLD_CHAINED_FIXUPS, link_off + fixups_at, fixups_blob.len() as u64);
    bytes::u32(out, LC_SYMTAB);
    bytes::u32(out, 24);
    bytes::u32(out, (link_off + symtab_at) as u32);
    bytes::u32(out, n as u32);
    bytes::u32(out, (link_off + strtab_at) as u32);
    bytes::u32(out, strtab_len as u32);
    bytes::u32(out, LC_DYSYMTAB);
    bytes::u32(out, 80);
    // ilocalsym, nlocalsym, iextdefsym, nextdefsym, iundefsym, nundefsym, tocoff, ntoc,
    // modtaboff, nmodtab, extrefsymoff, nextrefsyms, indirectsymoff, nindirectsyms, extreloff,
    // nextrel, locreloff, nlocrel.
    let dysymtab = [0, 0, 0, 0, 0, n as u32, 0, 0, 0, 0, 0, 0, (link_off + indirect_at) as u32, n as u32, 0, 0, 0, 0];
    for field in dysymtab {
        bytes::u32(out, field);
    }
    bytes::u32(out, LC_LOAD_DYLINKER);
    bytes::u32(out, 32);
    bytes::u32(out, 12);
    let at = out.len();
    out.extend_from_slice(DYLD.as_bytes());
    out.resize(at + 20, 0);
    bytes::u32(out, LC_UUID);
    bytes::u32(out, 24);
    let uuid_at = out.len();
    out.extend_from_slice(&[0u8; 16]);
    bytes::u32(out, LC_BUILD_VERSION);
    bytes::u32(out, 24);
    bytes::u32(out, PLATFORM_MACOS);
    bytes::u32(out, MIN_MACOS);
    bytes::u32(out, MIN_MACOS); // sdk
    bytes::u32(out, 0); // ntools
    bytes::u32(out, LC_MAIN);
    bytes::u32(out, 24);
    bytes::u64(out, laid.entry);
    bytes::u64(out, 0); // stacksize: the default
    bytes::u32(out, LC_LOAD_DYLIB);
    bytes::u32(out, 56);
    bytes::u32(out, 24); // name offset
    bytes::u32(out, 2); // timestamp, as ld64 writes it
    bytes::u32(out, LIB_SYSTEM_CURRENT);
    bytes::u32(out, LIB_SYSTEM_COMPAT);
    let at = out.len();
    out.extend_from_slice(LIB_SYSTEM.as_bytes());
    out.resize(at + 32, 0);
    // The native image's libraries, after libSystem (flat lookup binds among all of them).
    if let Some(m) = native {
        for path in m.libraries.iter() {
            dylib_command(out, LC_LOAD_DYLIB, path.as_str(), 2, DYLIB_VERSION, DYLIB_VERSION);
        }
    }
    linkedit_data(out, LC_FUNCTION_STARTS, link_off + starts_at, starts_len);
    linkedit_data(out, LC_CODE_SIGNATURE, code_limit, sig_len);
    // Counted from the commands as written (native_dylib.rs `commands_in`), never by hand; each
    // command above has a cmdsize that is a positive multiple of 8, so a `None` is this
    // writer's own bug, said as the header's room outgrown.
    let Some(ncmds) = crate::native_dylib::commands_in(&out[HEADER as usize..]) else {
        return Err(ExportError::TooLarge { what: "the load commands as written: one's cmdsize is not a positive multiple of 8 (macho.rs `write_native`)", len: out.len() as u64 });
    };
    let sizeofcmds = out.len() as u64 - HEADER;
    debug_assert!(sizeofcmds <= MAX_COMMANDS + native.map_or(0, |m| command_room(&m.libraries)), "the load commands outgrew the header's room");
    bytes::set32(out, 0, MH_MAGIC_64);
    bytes::set32(out, 4, CPU_TYPE_ARM64);
    bytes::set32(out, 8, 0); // CPU_SUBTYPE_ARM64_ALL
    bytes::set32(out, 12, MH_EXECUTE);
    bytes::set32(out, 16, ncmds);
    bytes::set32(out, 20, sizeofcmds as u32);
    bytes::set32(out, 24, MH_FLAGS | native.map_or(0, |m| m.header_flags()));

    // The regions at their offsets, the fixups' slots in them.
    out.resize(laid.text_at as usize, 0);
    out.extend_from_slice(&laid.text);
    out.resize(eh.eh_frame_at as usize, 0);
    out.extend_from_slice(&eh.eh_frame);
    out.resize(eh.except_tab_at as usize, 0);
    out.extend_from_slice(&eh.except_tab);
    out.resize(eh.unwind_info_at as usize, 0);
    out.extend_from_slice(&eh.unwind_info);
    out.resize(text_size as usize, 0);
    if has_const {
        out.extend_from_slice(&laid.konst);
        out.resize((laid.const_at + const_size) as usize, 0);
    }
    if has_data && !laid.data.is_empty() {
        out.resize(laid.data_at as usize, 0);
        out.extend_from_slice(&laid.data);
        out.resize((laid.data_at + data_file) as usize, 0);
    }
    if let Some(m) = native {
        out.resize(native_file as usize, 0);
        out.extend_from_slice(m.bytes);
        out.resize(file_end as usize, 0);
    }
    debug_assert_eq!(out.len() as u64, link_off);
    // A slot's file offset: its image offset, but for the native image's (moved in the file).
    let file_of = |at: u64| native.map_or(at, |m| m.file_of(at, native_file));
    let late = plant::on(Plant::SignBeforeLastFixup) && !slots.is_empty();
    let written = if late { slots.len() - 1 } else { slots.len() };
    for &(at, value) in slots[..written].iter() {
        bytes::set64(out, file_of(at) as usize, value);
    }
    out.extend_from_slice(&link);

    // The identity of the content, then the signature over every byte before it.
    let mut hasher = Hasher::new(UUID_DOMAIN);
    hasher.update(seed);
    hasher.update(&out[..]);
    let digest = hasher.finish().0;
    let mut uuid = [0u8; 16];
    uuid.copy_from_slice(&digest[..16]);
    out[uuid_at..uuid_at + 16].copy_from_slice(&uuid);
    sign::sign(out, identifier, text_size, true);
    if late {
        let (at, value) = slots[slots.len() - 1];
        bytes::set64(out, file_of(at) as usize, value);
    }
    Ok(uuid)
}

/// The segments in load-command order with `__LINKEDIT` last, as the chained fixups count them.
fn segs_with_linkedit<'a>(segs: &[Seg], end: u64, alloc: Alloc<'a>) -> AVec<'a, Seg> {
    let mut all: AVec<'a, Seg> = Vec::with_capacity_in(segs.len() + 1, alloc);
    all.extend_from_slice(segs);
    all.push(Seg { name: "__LINKEDIT", vm: end, vm_size: 0, file: 0, file_size: 0, prot: VM_PROT_READ, flags: 0, fixups: false });
    all
}
