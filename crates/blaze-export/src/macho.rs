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
use blaze_own::{AString, AVec, Alloc, Vec};

use crate::bytes;
use crate::commands::{self, Command, Commands, CommandsError, Header, Section, Segment, segment_size};
use crate::eh_out::EhSections;
use crate::error::ExportError;
use crate::layout::{FixTo, Laid, Plan};
use crate::native_dylib::DYLIB_VERSION;
use crate::native_merge::{self, Merged, NATIVE_DATA, NATIVE_TEXT, Sect};
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

pub(crate) const LC_SEGMENT_64: u32 = 0x19;
pub(crate) const LC_SYMTAB: u32 = 0x2;
pub(crate) const LC_DYSYMTAB: u32 = 0xB;
pub(crate) const LC_LOAD_DYLIB: u32 = 0xC;
pub(crate) const LC_LOAD_DYLINKER: u32 = 0xE;
pub(crate) const LC_UUID: u32 = 0x1B;
pub(crate) const LC_CODE_SIGNATURE: u32 = 0x1D;
pub(crate) const LC_FUNCTION_STARTS: u32 = 0x26;
pub(crate) const LC_BUILD_VERSION: u32 = 0x32;
pub(crate) const LC_MAIN: u32 = 0x8000_0028;
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
const S_MOD_INIT_FUNC_POINTERS: u32 = 0x9;

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

/// `__TEXT`'s sections, each always written (an empty one has size 0): `__text`, `__eh_frame`,
/// `__gcc_except_tab`, `__unwind_info`.
const TEXT_SECTIONS: usize = 4;
/// `__DATA_CONST`'s sections at most: `__got`, `__mod_init_func`, `__const`.
const DATA_CONST_SECTIONS: usize = 3;
/// `__DATA`'s sections at most: `__data`, `__bss`.
const DATA_SECTIONS: usize = 2;

const UUID_DOMAIN: &str = "blaze-export macho uuid 1";

/// The layout plan a Mach-O file with no native image needs.
pub fn plan() -> Plan {
    plan_for(None)
}

/// The layout plan: the code starts past [`command_room`], 16-aligned. The room is fixed before
/// the layout from the native image's libraries alone (`native_merge::libraries`), so the code's
/// place depends on them, never on which optional segments the program has.
pub fn plan_for(native_libraries: Option<&[AString<'_>]>) -> Plan {
    Plan { page: PAGE, text_at: up(command_room(native_libraries), 16), const_head: 0 }
}

/// The executable's header and load commands at their largest, in bytes: the list
/// [`write_native`] builds, with every optional segment present at its most sections and the
/// same commands after them ([`tail`], with zeros: no size depends on a value), sized by the same
/// `Command::size` and `segment_size` the writer's bytes follow. `write_native` checks the list
/// it builds against the room this kept, as a typed error in every build
/// (`ExportError::HeaderRoom`).
pub fn command_room(native_libraries: Option<&[AString<'_>]>) -> u64 {
    // __PAGEZERO, __TEXT, __DATA_CONST, __DATA and __LINKEDIT; the native image's two.
    let mut room = commands::HEADER
        + segment_size(0)
        + segment_size(TEXT_SECTIONS as u64)
        + segment_size(DATA_CONST_SECTIONS as u64)
        + segment_size(DATA_SECTIONS as u64)
        + segment_size(0);
    if native_libraries.is_some() {
        room += segment_size(native_merge::CODE_SECTIONS as u64) + segment_size(native_merge::DATA_SECTIONS as u64);
    }
    tail(&Tail::default(), native_libraries.unwrap_or(&[]), |command| room += command.size());
    room
}

/// What the executable's commands after its segments say: `__LINKEDIT`'s pieces as file
/// offsets and sizes, the import count, the entry.
#[derive(Clone, Copy, Default)]
struct Tail {
    fixups: (u64, u64),
    /// `symoff` and `nsyms` (one undefined symbol per import, also one indirect row each).
    symtab: (u64, u64),
    strtab: (u64, u64),
    indirect_at: u64,
    entry: u64,
    starts: (u64, u64),
    signature: (u64, u64),
}

/// The executable's commands after its segments, in order: [`write_native`] builds them with
/// the file's values and [`command_room`] with zeros, so the room is this list's size.
fn tail<'n>(t: &Tail, libraries: &'n [AString<'_>], mut push: impl FnMut(Command<'n>)) {
    push(Command::LinkeditData { cmd: LC_DYLD_CHAINED_FIXUPS, off: t.fixups.0, size: t.fixups.1 });
    push(Command::Symtab { symoff: t.symtab.0, nsyms: t.symtab.1, stroff: t.strtab.0, strsize: t.strtab.1 });
    // Every symbol undefined (iundefsym 0, nundefsym n), one indirect row per GOT cell.
    let n = t.symtab.1;
    push(Command::Dysymtab { fields: [0, 0, 0, 0, 0, n, 0, 0, 0, 0, 0, 0, t.indirect_at, n, 0, 0, 0, 0] });
    push(Command::Dylinker { name: DYLD });
    push(Command::Uuid);
    push(Command::BuildVersion { platform: PLATFORM_MACOS, minos: MIN_MACOS, sdk: MIN_MACOS });
    push(Command::Main { entry: t.entry, stack: 0 }); // stacksize 0: the default
    // libSystem first (LIBSYSTEM_ORDINAL), then the native image's libraries (flat lookup binds
    // among all of them); timestamp 2, as ld64 writes it.
    push(Command::Dylib { cmd: LC_LOAD_DYLIB, name: LIB_SYSTEM, timestamp: 2, current: LIB_SYSTEM_CURRENT, compat: LIB_SYSTEM_COMPAT });
    for path in libraries {
        push(Command::Dylib { cmd: LC_LOAD_DYLIB, name: path.as_str(), timestamp: 2, current: DYLIB_VERSION, compat: DYLIB_VERSION });
    }
    push(Command::LinkeditData { cmd: LC_FUNCTION_STARTS, off: t.starts.0, size: t.starts.1 });
    push(Command::LinkeditData { cmd: LC_CODE_SIGNATURE, off: t.signature.0, size: t.signature.1 });
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

/// A chained rebase's target: an image offset that `dyld_chained_ptr_64_rebase.target`'s 36
/// bits hold, by construction ([`Target36::new`] refuses one at or above 2^36 where the value is
/// made, so the encoder only places fields).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) struct Target36(u64);

impl Target36 {
    /// `offset`, or `None` when its 36-bit field cannot hold it.
    pub(crate) const fn new(offset: u64) -> Option<Target36> {
        if offset >> 36 == 0 { Some(Target36(offset)) } else { None }
    }

    pub(crate) const fn get(self) -> u64 {
        self.0
    }
}

/// What a chained slot becomes, in neither writer's own terms: the executable's [`FixTo`] and
/// the native dylib's (`native_dylib.rs`, `blaze_native::image::FixTo`) both map to it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Chain {
    /// dyld writes `header + (high8 << 56 | target)`: the image's runtime address plus `target`,
    /// with `high8` the top byte the program keeps in the pointer (arm64 non-unique RTTI's bit
    /// 63 on a typeinfo's name; 0 for every other pointer).
    Rebase { target: Target36, high8: u8 },
    /// The address import row `import` names.
    Bind { import: u32 },
}

impl Chain {
    /// The untagged rebase to image offset `offset`, or `None` past the 36-bit field.
    pub(crate) const fn untagged(offset: u64) -> Option<Chain> {
        match Target36::new(offset) {
            Some(target) => Some(Chain::Rebase { target, high8: 0 }),
            None => None,
        }
    }
}

/// What a chained rebase's target past 36 bits is refused as (each writer names the slot).
pub(crate) const REBASE_TOO_FAR: &str = "a chained rebase's 36-bit target (below its top byte, the tag)";

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
            // bind (1). Each field is its own typed value ([`Target36`], `u8`), so nothing here
            // can overlap another: a target past 36 bits was refused where the chain was made.
            Chain::Rebase { target, high8 } => target.get() | (u64::from(high8) << 36) | (next << 51),
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
    let rebase = |target: u64| Chain::untagged(target).ok_or(ExportError::TooLarge { what: REBASE_TOO_FAR, len: target });
    for f in laid.fixups.iter() {
        let to = match f.to {
            FixTo::Rebase { target } => rebase(target)?,
            FixTo::Bind { import } => match direct(import) {
                Some(target) => rebase(target)?,
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

/// A segment's command: at its image offset past `__PAGEZERO`, which is at 0.
fn segment(seg: &Seg) -> Segment<'static> {
    let vmaddr = if seg.name == "__PAGEZERO" { 0 } else { BASE + seg.vm };
    Segment { name: seg.name, vmaddr, vm_size: seg.vm_size, file: seg.file, file_size: seg.file_size, prot: seg.prot, flags: seg.flags }
}

/// `section_64`: `addr` an image offset, `offset` its file offset (0 for zero fill).
fn section(name: &'static str, seg: &'static str, addr: u64, size: u64, offset: u64, align: u32, flags: u32) -> Section<'static> {
    Section { name, seg, addr: BASE + addr, size, offset, align, flags, reserved1: 0 }
}

/// Writes `laid` as a Mach-O executable into `out` (its contents replaced): the file, whole,
/// signed. `identifier` is the signature's; `seed` joins the content in the file's `LC_UUID`
/// (the profile's digest, or zeros). Returns the UUID.
pub fn write<'a>(laid: &Laid<'_, '_>, eh: &EhSections<'_>, identifier: &str, seed: &[u8; 32], out: &mut AVec<'a, u8>, alloc: Alloc<'a>) -> Result<[u8; 16], ExportError> {
    write_native(laid, eh, None, identifier, seed, out, alloc)
}

/// [`write`] with the program's native image merged (native_merge.rs): its two segments after
/// the program's, its libraries after libSystem, its fixups and import rows in the one chained
/// blob, `MH_HAS_TLV_DESCRIPTORS` when it has thread-locals. The layout's plan must be
/// [`plan_for`] of the image's libraries: a header past the room it kept is
/// `ExportError::HeaderRoom`.
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

    // The header and load commands, built first as a list (commands.rs): `ncmds` and
    // `sizeofcmds` are the list's, and the room `plan_for` kept before the code (`text_at`,
    // [`command_room`] of the same commands at their largest) is checked against it in every
    // build.
    let got_size = 8 * n as u64;
    let mut cmds = Commands::new(alloc);
    for seg in segs.iter() {
        match seg.name {
            "__TEXT" => {
                let sections: [Section<'_>; TEXT_SECTIONS] = [
                    section("__text", "__TEXT", laid.text_at, laid.text.len() as u64, laid.text_at, 4, S_CODE),
                    section("__eh_frame", "__TEXT", eh.eh_frame_at, eh.eh_frame.len() as u64, eh.eh_frame_at, 3, S_EH_FRAME),
                    section("__gcc_except_tab", "__TEXT", eh.except_tab_at, eh.except_tab.len() as u64, eh.except_tab_at, 2, S_REGULAR),
                    section("__unwind_info", "__TEXT", eh.unwind_info_at, eh.unwind_info.len() as u64, eh.unwind_info_at, 2, S_REGULAR),
                ];
                cmds.segment(segment(seg), sections);
            }
            "__DATA_CONST" => {
                let mut cursor = laid.got_at + got_size;
                let mod_init = laid.mod_init_at.map(|at| {
                    cursor = at + 8;
                    section("__mod_init_func", "__DATA_CONST", at, 8, at, 3, S_MOD_INIT_FUNC_POINTERS)
                });
                let const_start = up(cursor, 16);
                let rest = laid.const_end().saturating_sub(const_start);
                let sections: [Option<Section<'_>>; DATA_CONST_SECTIONS] = [
                    (n > 0).then(|| section("__got", "__DATA_CONST", laid.got_at, got_size, laid.got_at, 3, S_NON_LAZY_SYMBOL_POINTERS)),
                    mod_init,
                    (rest > 0).then(|| section("__const", "__DATA_CONST", const_start, rest, const_start, 4, S_REGULAR)),
                ];
                cmds.segment(segment(seg), sections.into_iter().flatten());
            }
            "__DATA" => {
                let data = laid.data.len() as u64;
                let sections: [Option<Section<'_>>; DATA_SECTIONS] = [
                    (data > 0).then(|| section("__data", "__DATA", laid.data_at, data, laid.data_at, 4, S_REGULAR)),
                    (laid.zero_len > 0).then(|| section("__bss", "__DATA", laid.zero_at, laid.zero_len, 0, 4, S_ZEROFILL)),
                ];
                cmds.segment(segment(seg), sections.into_iter().flatten());
            }
            NATIVE_TEXT | NATIVE_DATA => {
                let sects: &[Sect] = match native {
                    Some(m) if seg.name == NATIVE_TEXT => &m.code,
                    Some(m) => &m.data,
                    None => &[],
                };
                let sections = sects.iter().map(|s| {
                    let offset = if s.zero { 0 } else { s.at - seg.vm + seg.file };
                    section(s.name, seg.name, s.at, s.len, offset, s.align_log2, s.flags)
                });
                cmds.segment(segment(seg), sections);
            }
            _ => cmds.segment(segment(seg), []),
        }
    }
    let t = Tail {
        fixups: (link_off + fixups_at, fixups_blob.len() as u64),
        symtab: (link_off + symtab_at, n as u64),
        strtab: (link_off + strtab_at, strtab_len),
        indirect_at: link_off + indirect_at,
        entry: laid.entry,
        starts: (link_off + starts_at, starts_len),
        signature: (code_limit, sig_len),
    };
    let libraries: &[AString<'_>] = match native {
        Some(m) => &m.libraries,
        None => &[],
    };
    tail(&t, libraries, |command| cmds.push(command));
    out.clear();
    let header = Header { filetype: MH_EXECUTE, flags: MH_FLAGS | native.map_or(0, |m| m.header_flags()) };
    let uuid_at = match cmds.write(out, header, laid.text_at) {
        Ok(written) => written.uuid_at,
        Err(CommandsError::Room { need, room }) => return Err(ExportError::HeaderRoom { what: "the executable", need, room }),
        Err(CommandsError::Field { what, value }) => return Err(ExportError::TooLarge { what, len: value }),
    };

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
        let const_at = usize::try_from(laid.const_at).map_err(|_| ExportError::TooLarge { what: "__DATA_CONST offset", len: laid.const_at })?;
        out.resize(const_at, 0);
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
    // `__LINKEDIT`'s offset was measured before the regions were written, and every load
    // command above already names it: refused in every build when the regions came out other.
    if out.len() as u64 != link_off {
        return Err(ExportError::Remeasured { what: "the executable's regions before __LINKEDIT", measured: link_off, written: out.len() as u64 });
    }
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
