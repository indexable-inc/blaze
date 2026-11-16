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
use crate::error::ExportError;
use crate::layout::{FixTo, Laid, Plan};
use crate::plant::{self, Plant};
use crate::sign;

/// `__TEXT`'s address in an unslid image: past `__PAGEZERO`'s 4 GiB, as ld64 places it.
pub const BASE: u64 = 0x1_0000_0000;
pub const PAGE: u64 = 0x4000;

const MH_MAGIC_64: u32 = 0xFEED_FACF;
const CPU_TYPE_ARM64: u32 = 0x0100_000C;
const MH_EXECUTE: u32 = 2;
/// `MH_NOUNDEFS | MH_DYLDLINK | MH_TWOLEVEL | MH_PIE`.
const MH_FLAGS: u32 = 0x1 | 0x4 | 0x80 | 0x20_0000;

const LC_SEGMENT_64: u32 = 0x19;
const LC_SYMTAB: u32 = 0x2;
const LC_DYSYMTAB: u32 = 0xB;
const LC_LOAD_DYLIB: u32 = 0xC;
const LC_LOAD_DYLINKER: u32 = 0xE;
const LC_UUID: u32 = 0x1B;
const LC_CODE_SIGNATURE: u32 = 0x1D;
const LC_FUNCTION_STARTS: u32 = 0x26;
const LC_BUILD_VERSION: u32 = 0x32;
const LC_MAIN: u32 = 0x8000_0028;
const LC_DYLD_CHAINED_FIXUPS: u32 = 0x8000_0034;

const VM_PROT_READ: u32 = 1;
const VM_PROT_WRITE: u32 = 2;
const VM_PROT_EXECUTE: u32 = 4;
/// `SG_READ_ONLY`: dyld makes the segment read-only once its fixups are done.
const SG_READ_ONLY: u32 = 0x10;

/// `S_ATTR_PURE_INSTRUCTIONS | S_ATTR_SOME_INSTRUCTIONS`.
const S_CODE: u32 = 0x8000_0000 | 0x400;
const S_REGULAR: u32 = 0;
const S_ZEROFILL: u32 = 0x1;
const S_NON_LAZY_SYMBOL_POINTERS: u32 = 0x6;

const PLATFORM_MACOS: u32 = 1;
/// macOS 12.0 (`xxxx.yy.zz` in nibbles): the first whose dyld binds every executable through
/// chained fixups of `DYLD_CHAINED_PTR_64_OFFSET`.
const MIN_MACOS: u32 = 0x000C_0000;

const DYLD: &str = "/usr/lib/dyld";
const LIB_SYSTEM: &str = "/usr/lib/libSystem.B.dylib";
/// libSystem's versions as a `dylib_command` names them: current 1311.0.0, compatibility 1.0.0.
const LIB_SYSTEM_CURRENT: u32 = 1311 << 16;
const LIB_SYSTEM_COMPAT: u32 = 1 << 16;

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

const SEGMENT: u64 = 72;
const SECTION: u64 = 80;
/// Every load command at its largest: the header's room is fixed, so the code's place does not
/// depend on which optional segments a program has.
const MAX_COMMANDS: u64 = SEGMENT // __PAGEZERO
    + SEGMENT + SECTION // __TEXT
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
const HEADER: u64 = 32;
/// Where the code starts: past the header and every load command, 16-aligned.
pub const TEXT_AT: u64 = (HEADER + MAX_COMMANDS + 15) & !15;

const UUID_DOMAIN: &str = "blaze-export macho uuid 1";

/// The layout plan a Mach-O file needs.
pub fn plan() -> Plan {
    Plan { page: PAGE, text_at: TEXT_AT, const_head: 0 }
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

/// The chained fixups blob, and each fixup slot's encoded value by file offset.
fn chained<'a>(laid: &Laid<'_, '_>, segs: &[Seg], alloc: Alloc<'a>) -> Result<(AVec<'a, u8>, AVec<'a, (u64, u64)>), ExportError> {
    let mut slots: AVec<'a, (u64, u64)> = Vec::with_capacity_in(laid.fixups.len(), alloc);
    for (k, f) in laid.fixups.iter().enumerate() {
        let page_of = |at: u64| at / PAGE;
        let next = match laid.fixups.get(k + 1) {
            Some(n) if page_of(n.at) == page_of(f.at) => (n.at - f.at) / STRIDE,
            _ => 0,
        };
        let value = match f.to {
            FixTo::Rebase { target } => {
                if target >= 1 << 36 {
                    return Err(ExportError::TooLarge { what: "a chained rebase's 36-bit target", len: target });
                }
                target | (next << 51)
            }
            FixTo::Bind { import } => {
                if import >= 1 << 24 {
                    return Err(ExportError::TooLarge { what: "a chained bind's 24-bit ordinal", len: u64::from(import) });
                }
                u64::from(import) | (next << 51) | (1 << 63)
            }
        };
        slots.push((f.at, value));
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
            return Err(ExportError::TooLarge { what: "a segment's 16-bit page count", len: seg.vm_size });
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
        for f in laid.fixups.iter().filter(|f| f.at >= seg.vm && f.at < seg.vm + seg.vm_size).rev() {
            let page = ((f.at - seg.vm) / PAGE) as usize;
            let offset = ((f.at - seg.vm) % PAGE) as u16;
            blob[first + 2 * page..first + 2 * page + 2].copy_from_slice(&offset.to_le_bytes());
        }
    }
    bytes::pad(&mut blob, 4);
    let imports_at = blob.len();
    let mut name_at = 0u32;
    for import in laid.imports.iter() {
        if name_at >= 1 << 23 {
            return Err(ExportError::TooLarge { what: "a chained import's 23-bit name offset", len: u64::from(name_at) });
        }
        // `dyld_chained_import`: lib_ordinal 1 (libSystem), not weak, the name's offset.
        bytes::u32(&mut blob, 1 | (name_at << 9));
        name_at += import.name.len() as u32 + 2;
    }
    let symbols_at = blob.len();
    for import in laid.imports.iter() {
        blob.push(b'_');
        blob.extend_from_slice(import.name.as_bytes());
        blob.push(0);
    }
    bytes::pad(&mut blob, 8);
    bytes::set32(&mut blob, 0, 0); // fixups_version
    bytes::set32(&mut blob, 4, starts_at as u32);
    bytes::set32(&mut blob, 8, imports_at as u32);
    bytes::set32(&mut blob, 12, symbols_at as u32);
    bytes::set32(&mut blob, 16, laid.imports.len() as u32);
    bytes::set32(&mut blob, 20, DYLD_CHAINED_IMPORT);
    bytes::set32(&mut blob, 24, 0); // symbols_format: not compressed
    Ok((blob, slots))
}

fn segment(out: &mut AVec<'_, u8>, seg: &Seg, nsects: u32) {
    bytes::u32(out, LC_SEGMENT_64);
    bytes::u32(out, (SEGMENT + SECTION * u64::from(nsects)) as u32);
    bytes::name16(out, seg.name);
    bytes::u64(out, if seg.name == "__PAGEZERO" { 0 } else { BASE + seg.vm });
    bytes::u64(out, seg.vm_size);
    bytes::u64(out, seg.file);
    bytes::u64(out, seg.file_size);
    bytes::u32(out, seg.prot);
    bytes::u32(out, seg.prot);
    bytes::u32(out, nsects);
    bytes::u32(out, seg.flags);
}

/// `section_64`: `addr` an image offset, `offset` its file offset (0 for zero fill).
#[allow(clippy::too_many_arguments)]
fn section(out: &mut AVec<'_, u8>, name: &str, seg: &str, addr: u64, size: u64, offset: u64, align: u32, flags: u32, reserved1: u32) {
    bytes::name16(out, name);
    bytes::name16(out, seg);
    bytes::u64(out, BASE + addr);
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

fn linkedit_data(out: &mut AVec<'_, u8>, cmd: u32, off: u64, size: u64) {
    bytes::u32(out, cmd);
    bytes::u32(out, 16);
    bytes::u32(out, off as u32);
    bytes::u32(out, size as u32);
}

/// Writes `laid` as a Mach-O executable into `out` (its contents replaced): the file, whole,
/// signed. `identifier` is the signature's; `seed` joins the content in the file's `LC_UUID`
/// (the profile's digest, or zeros). Returns the UUID.
pub fn write<'a>(laid: &Laid<'_, '_>, identifier: &str, seed: &[u8; 32], out: &mut AVec<'a, u8>, alloc: Alloc<'a>) -> Result<[u8; 16], ExportError> {
    let has_const = !laid.konst.is_empty();
    let has_data = !laid.data.is_empty() || laid.zero_len > 0;
    let text_size = up(laid.text_end(), PAGE);
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
    // `__LINKEDIT`'s pieces, at offsets from its start.
    let (fixups_blob, slots) = chained(laid, &segs_with_linkedit(&segs, laid.end, alloc), alloc)?;
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
        vm: laid.end,
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
    let mut ncmds = 0u32;
    let got_size = 8 * n as u64;
    for seg in segs.iter() {
        ncmds += 1;
        match seg.name {
            "__TEXT" => {
                segment(out, seg, 1);
                section(out, "__text", "__TEXT", laid.text_at, laid.text.len() as u64, laid.text_at, 4, S_CODE, 0);
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
    linkedit_data(out, LC_FUNCTION_STARTS, link_off + starts_at, starts_len);
    linkedit_data(out, LC_CODE_SIGNATURE, code_limit, sig_len);
    ncmds += 10;
    let sizeofcmds = out.len() as u64 - HEADER;
    debug_assert!(sizeofcmds <= MAX_COMMANDS, "the load commands outgrew the header's room");
    bytes::set32(out, 0, MH_MAGIC_64);
    bytes::set32(out, 4, CPU_TYPE_ARM64);
    bytes::set32(out, 8, 0); // CPU_SUBTYPE_ARM64_ALL
    bytes::set32(out, 12, MH_EXECUTE);
    bytes::set32(out, 16, ncmds);
    bytes::set32(out, 20, sizeofcmds as u32);
    bytes::set32(out, 24, MH_FLAGS);

    // The regions at their offsets, the fixups' slots in them.
    out.resize(laid.text_at as usize, 0);
    out.extend_from_slice(&laid.text);
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
    debug_assert_eq!(out.len() as u64, link_off);
    let late = plant::on(Plant::SignBeforeLastFixup) && !slots.is_empty();
    let written = if late { slots.len() - 1 } else { slots.len() };
    for &(at, value) in slots[..written].iter() {
        bytes::set64(out, at as usize, value);
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
        bytes::set64(out, at as usize, value);
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
