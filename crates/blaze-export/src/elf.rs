//! The ELF writer: a laid-out program as one AArch64 Linux executable, position-independent
//! (`ET_DYN` with `DF_1_PIE`), started by glibc's dynamic loader, which binds its C names and
//! applies its relocations before `_start`. Structures: the System V gABI and its AArch64
//! supplement (`Elf64_Ehdr`, `Elf64_Phdr`, `Elf64_Dyn`, `Elf64_Sym`, `Elf64_Rela`, the SysV hash
//! table; `R_AARCH64_RELATIVE` 1027, `R_AARCH64_GLOB_DAT` 1025, `R_AARCH64_ABS64` 257), and
//! glibc's `<elf.h>` for the GNU program headers and `DT_FLAGS_1`.
//!
//! The file has no section headers: a loader reads program headers only.
//!
//! | load | holds | protection |
//! |---|---|---|
//! | text | the ELF header, program headers, the interpreter's path, the build-id note, the code | r-x |
//! | const | the dynamic section, the GOT, relro parts, read-only data (`PT_GNU_RELRO` over all of it) | rw-, then r-- |
//! | data | writable statics, zeroed ones past the file bytes | rw- |
//! | meta | `.dynsym`, `.dynstr`, the SysV hash, `.rela.dyn` | r-- |
//!
//! Facts marked "to verify" (by the Linux run test, not written yet: the host test is macOS's):
//! glibc's `ld.so` accepts the dynamic section's tables in a load after the data; an
//! unversioned reference binds to glibc's default version of the name; a one-bucket SysV hash
//! with every chain empty is a valid table for an executable that exports nothing.

use blaze_ids::Hasher;
use blaze_native::image::{Image, Region};
use blaze_own::{AString, AVec, Alloc, Vec};

use crate::bytes;
use crate::eh_out::EhSections;
use crate::error::ExportError;
use crate::layout::{FixTo, Laid, Plan};
use crate::native_merge::{ElfRelaKind, MergeError, MergedElf};

pub const PAGE: u64 = 0x1_0000;

pub(crate) const EHDR: u64 = 64;
pub(crate) const PHDR: u64 = 56;
/// Every program header at the most: PHDR, INTERP, NOTE, up to six LOADs (text, const, data,
/// native text, native data, meta), DYNAMIC, TLS, GNU_RELRO, GNU_EH_FRAME, GNU_STACK.
const MAX_PHDRS: u64 = 14;
const INTERP: &str = "/lib/ld-linux-aarch64.so.1";
const INTERP_AT: u64 = EHDR + PHDR * MAX_PHDRS;
const NOTE_AT: u64 = (INTERP_AT + INTERP.len() as u64 + 1 + 3) & !3;
/// `Elf64_Nhdr` (namesz, descsz, type), "GNU\0", a 16-byte id.
const NOTE_LEN: u64 = 12 + 4 + 16;
pub const TEXT_AT: u64 = (NOTE_AT + NOTE_LEN + 15) & !15;

pub(crate) const PT_LOAD: u32 = 1;
pub(crate) const PT_DYNAMIC: u32 = 2;
const PT_INTERP: u32 = 3;
const PT_NOTE: u32 = 4;
const PT_PHDR: u32 = 6;
pub(crate) const PT_TLS: u32 = 7;
/// AArch64 ELF TLS Variant I layout for the main executable's per-thread `ROOT` pointer:
/// TCB is 16 bytes at `tpidr_el0`, so with `p_align = 16` and `p_vaddr % p_align == 0`, both
/// glibc and musl place byte 0 of `PT_TLS` at `tpidr_el0 + align_up(16, 16) = tpidr_el0 + 16`.
pub(crate) const TLS_ROOT_ALIGN: u64 = 16;
pub(crate) const TLS_ROOT_OFFSET: u64 = 16;
pub(crate) const TLS_ROOT_MEMSZ: u64 = 8;
/// `.eh_frame_hdr`, which glibc's and libgcc's unwinders find through `dl_iterate_phdr`.
pub(crate) const PT_GNU_EH_FRAME: u32 = 0x6474_E550;
pub(crate) const PT_GNU_STACK: u32 = 0x6474_E551;
pub(crate) const PT_GNU_RELRO: u32 = 0x6474_E552;
pub(crate) const PF_X: u32 = 1;
pub(crate) const PF_W: u32 = 2;
pub(crate) const PF_R: u32 = 4;
const NT_GNU_BUILD_ID: u32 = 3;

pub(crate) const DT_NULL: u64 = 0;
pub(crate) const DT_NEEDED: u64 = 1;
const DT_HASH: u64 = 4;
pub(crate) const DT_STRTAB: u64 = 5;
pub(crate) const DT_SYMTAB: u64 = 6;
pub(crate) const DT_RELA: u64 = 7;
pub(crate) const DT_RELASZ: u64 = 8;
pub(crate) const DT_RELAENT: u64 = 9;
pub(crate) const DT_STRSZ: u64 = 10;
pub(crate) const DT_SYMENT: u64 = 11;
const DT_DEBUG: u64 = 21;
const DT_INIT_ARRAY: u64 = 25;
const DT_INIT_ARRAYSZ: u64 = 27;
pub(crate) const DT_FLAGS: u64 = 30;
pub(crate) const DT_RELACOUNT: u64 = 0x6FFF_FFF9;
pub(crate) const DT_FLAGS_1: u64 = 0x6FFF_FFFB;
pub(crate) const DF_BIND_NOW: u64 = 0x8;
const DF_STATIC_TLS: u64 = 0x10;
/// `DF_1_NOW | DF_1_PIE`.
const DF_1: u64 = 0x1 | 0x0800_0000;
/// The base dynamic section's entries (excluding extra native `DT_NEEDED` and `DT_INIT_ARRAY`).
const DYN_COUNT: u64 = 14;

pub(crate) const R_AARCH64_ABS64: u64 = 257;
pub(crate) const R_AARCH64_GLOB_DAT: u64 = 1025;
pub(crate) const R_AARCH64_RELATIVE: u64 = 1027;
const R_AARCH64_TLS_TPREL64: u64 = 1030;
const R_AARCH64_TLSDESC: u64 = 1031;
/// `STB_GLOBAL << 4 | STT_FUNC`.
const GLOBAL_FUNC: u8 = 0x12;
const STB_GLOBAL: u8 = 1;
const STB_WEAK: u8 = 2;
const STT_NOTYPE: u8 = 0;

pub(crate) const LIBC: &str = "libc.so.6";
const ID_DOMAIN: &str = "blaze-export elf build id 1";

/// The layout plan an ELF file with no native image needs: the dynamic section heads the const region.
pub fn plan() -> Plan {
    plan_for(None, false)
}

/// The layout plan for an ELF file with an optional merged native image: reserves room at the head
/// of the const region for every `DT_NEEDED` soname and optional `DT_INIT_ARRAY` / `DT_INIT_ARRAYSZ`.
pub fn plan_for(native_libraries: Option<&[AString<'_>]>, has_inits: bool) -> Plan {
    let extra_libs = native_libraries.map_or(0, |libs| u64::try_from(libs.len()).unwrap_or(u64::MAX));
    let init_entries = if has_inits { 2u64 } else { 0u64 };
    let dyn_count = DYN_COUNT + extra_libs + init_entries;
    Plan { page: PAGE, text_at: TEXT_AT, const_head: dyn_count * 16 }
}

/// Computes the `tpidr_el0`-relative byte offset of Rust's 8-byte `ROOT` pointer in the
/// executable's single `PT_TLS` segment (AArch64 Variant I).
///
/// - Without native C TLS: `PT_TLS` holds only Rust's 8-byte `ROOT` with `p_align = 16`, so byte 0
///   sits at `tpidr_el0 + 16` ([`TLS_ROOT_OFFSET`]).
/// - With native C TLS (`c_memsz`, `c_align`): C's `.tdata` + `.tbss` occupy `[0, c_memsz)` of
///   `PT_TLS` (preserving every `R_AARCH64_TLSDESC` / `R_AARCH64_TLS_TPREL64` template offset
///   unchanged), Rust's zero-initialized `ROOT` sits after them in the zero-filled tail at
///   `up(c_memsz, 8)`, and `p_align = c_align.max(16)` with `p_vaddr % p_align == 0`, placing
///   byte 0 at `tpidr_el0 + p_align` and `ROOT` at `tpidr_el0 + p_align + up(c_memsz, 8)`.
pub(crate) fn tls_root_offset(natives: Option<&Image<'_>>) -> Result<u64, MergeError> {
    let Some(image) = natives else {
        return Ok(TLS_ROOT_OFFSET);
    };
    let Some(tdata) = image.region(Region::ThreadData) else {
        return Err(MergeError::NoRegion { region: Region::ThreadData });
    };
    let Some(tbss) = image.region(Region::ThreadZero) else {
        return Err(MergeError::NoRegion { region: Region::ThreadZero });
    };
    let tls_at = if tdata.len > 0 { tdata.at } else { tbss.at };
    let tls_end = if tbss.len > 0 { tbss.at + tbss.len } else { tdata.at + tdata.len };
    let c_memsz = tls_end.saturating_sub(tls_at);
    if c_memsz == 0 {
        return Ok(TLS_ROOT_OFFSET);
    }
    let log2 = tdata.align_log2.max(tbss.align_log2);
    if log2 > 16 {
        return Err(MergeError::TooLarge { what: "a TLS alignment past a 64 KiB page", len: u64::from(log2) });
    }
    let align = (1u64 << log2).max(TLS_ROOT_ALIGN);
    Ok(align + up(c_memsz, 8))
}

fn up(n: u64, to: u64) -> u64 {
    (n + to - 1) & !(to - 1)
}

fn u64_of(n: usize, what: &'static str) -> Result<u64, ExportError> {
    u64::try_from(n).map_err(|_| ExportError::TooLarge { what, len: u64::MAX })
}

fn u32_of(n: usize, what: &'static str) -> Result<u32, ExportError> {
    u32::try_from(n).map_err(|_| ExportError::TooLarge { what, len: u64::try_from(n).unwrap_or(u64::MAX) })
}

fn usize_of(n: u64, what: &'static str) -> Result<usize, ExportError> {
    usize::try_from(n).map_err(|_| ExportError::TooLarge { what, len: n })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn phdr(out: &mut AVec<'_, u8>, kind: u32, flags: u32, offset: u64, vaddr: u64, filesz: u64, memsz: u64, align: u64) {
    bytes::u32(out, kind);
    bytes::u32(out, flags);
    bytes::u64(out, offset);
    bytes::u64(out, vaddr);
    bytes::u64(out, vaddr); // p_paddr
    bytes::u64(out, filesz);
    bytes::u64(out, memsz);
    bytes::u64(out, align);
}

#[derive(Clone, Copy)]
struct TlsPhdr {
    offset: u64,
    vaddr: u64,
    filesz: u64,
    memsz: u64,
    align: u64,
}

/// Writes `laid` as an ELF executable into `out` (its contents replaced). `seed` joins the
/// content in the build id (the profile's digest, or zeros). Returns the build id.
pub fn write<'a>(laid: &Laid<'_, '_>, eh: &EhSections<'_>, seed: &[u8; 32], out: &mut AVec<'a, u8>, alloc: Alloc<'a>) -> Result<[u8; 16], ExportError> {
    write_native(laid, eh, None, seed, out, alloc)
}

/// [`write`] with an optional merged native C image ([`MergedElf`]): its `r-x` and `rw-` loads
/// follow the program's loads, its `DT_NEEDED` sonames follow `libc.so.6`, its `Inits` become
/// `DT_INIT_ARRAY`/`DT_INIT_ARRAYSZ`, its relocations join `.rela.dyn`, and its C TLS template
/// coexists with Rust `Space::Tls` in the executable's single `PT_TLS`.
pub fn write_native<'a>(
    laid: &Laid<'_, '_>,
    eh: &EhSections<'_>,
    native: Option<&MergedElf<'_, '_>>,
    seed: &[u8; 32],
    out: &mut AVec<'a, u8>,
    alloc: Alloc<'a>,
) -> Result<[u8; 16], ExportError> {
    // Program imports defined by the native image become R_AARCH64_RELATIVE and leave `.dynsym`.
    let direct = |import: u32| {
        native.and_then(|m| {
            m.direct
                .binary_search_by_key(&import, |&(row, _)| row)
                .ok()
                .and_then(|i| m.direct.get(i).map(|&(_, at)| at))
        })
    };
    let mut kept: AVec<'_, u32> = Vec::with_capacity_in(laid.imports.len(), alloc);
    let mut row_of: AVec<'_, u32> = Vec::with_capacity_in(laid.imports.len(), alloc);
    for k in 0..laid.imports.len() {
        row_of.push(u32_of(kept.len(), "an ELF import index")?);
        let idx = u32_of(k, "a program import index")?;
        if direct(idx).is_none() {
            kept.push(idx);
        }
    }

    let has_data = !laid.data.is_empty() || laid.zero_len > 0;
    let konst_len = u64_of(laid.konst.len(), "the const region")?;
    let data_len = u64_of(laid.data.len(), "the data region")?;
    let const_mem = up(konst_len, PAGE);
    let mut file_end = laid.const_at + const_mem;
    if !laid.data.is_empty() {
        file_end = laid.data_at + up(data_len, PAGE);
    }
    let mut vm_end = laid.end;
    let mut native_file = 0u64;
    let mut native_code_len = 0u64;
    let mut native_data_filesz = 0u64;
    if let Some(m) = native {
        native_file = up(file_end, PAGE);
        native_code_len = m.data_at - m.text_at;
        native_data_filesz = m.file_end.saturating_sub(m.data_at);
        file_end = up(native_file + native_code_len + native_data_filesz, PAGE);
        vm_end = m.end;
    }
    let meta_off = file_end;
    let meta_at = vm_end;

    // The meta load: symbols, names, hash, relocations, and (if needed for alignment) TLS `.tdata`.
    let mut meta: AVec<'_, u8> = Vec::new_in(alloc);
    let dynsym_at = 0u64;
    meta.extend_from_slice(&[0u8; 24]);
    let mut dynstr: AVec<'_, u8> = Vec::new_in(alloc);
    dynstr.push(0);
    let needed_libc = u32_of(dynstr.len(), "DT_STRTAB offset")?;
    dynstr.extend_from_slice(LIBC.as_bytes());
    dynstr.push(0);
    let mut extra_needed: AVec<'_, u32> = Vec::new_in(alloc);
    if let Some(m) = native {
        for lib in m.libraries.iter() {
            extra_needed.push(u32_of(dynstr.len(), "DT_STRTAB offset")?);
            dynstr.extend_from_slice(lib.as_str().as_bytes());
            dynstr.push(0);
        }
    }
    for &idx in kept.iter() {
        let imp_idx = usize::try_from(idx).map_err(|_| ExportError::TooLarge { what: "a program import index", len: u64::from(idx) })?;
        let Some(import) = laid.imports.get(imp_idx) else {
            return Err(ExportError::TooLarge { what: "a program import index", len: u64::from(idx) });
        };
        // `Elf64_Sym`: st_name, st_info, st_other, st_shndx (SHN_UNDEF), st_value, st_size.
        bytes::u32(&mut meta, u32_of(dynstr.len(), "DT_STRTAB offset")?);
        bytes::u8(&mut meta, GLOBAL_FUNC);
        bytes::u8(&mut meta, 0);
        bytes::u16(&mut meta, 0);
        bytes::u64(&mut meta, 0);
        bytes::u64(&mut meta, 0);
        dynstr.extend_from_slice(import.name.as_bytes());
        dynstr.push(0);
    }
    let native_sym_base = 1u64 + u64_of(kept.len(), "ELF symbol count")?;
    if let Some(m) = native {
        for imp in m.imports.iter() {
            let bind = if imp.weak { STB_WEAK } else { STB_GLOBAL };
            bytes::u32(&mut meta, u32_of(dynstr.len(), "DT_STRTAB offset")?);
            bytes::u8(&mut meta, (bind << 4) | STT_NOTYPE);
            bytes::u8(&mut meta, 0);
            bytes::u16(&mut meta, 0);
            bytes::u64(&mut meta, 0);
            bytes::u64(&mut meta, 0);
            dynstr.extend_from_slice(imp.name);
            dynstr.push(0);
        }
    }
    let dynstr_at = u64_of(meta.len(), "meta offset")?;
    meta.extend_from_slice(&dynstr);
    bytes::pad(&mut meta, 8);
    let hash_at = u64_of(meta.len(), "meta offset")?;
    let total_syms = 1usize + kept.len() + native.map_or(0, |m| m.imports.len());
    let nsyms = u32_of(total_syms, "DT_HASH nchain")?;
    bytes::u32(&mut meta, 1); // nbucket
    bytes::u32(&mut meta, nsyms); // nchain
    bytes::u32(&mut meta, 0); // bucket[0]: no symbol is defined here
    for _ in 0..nsyms {
        bytes::u32(&mut meta, 0);
    }
    bytes::pad(&mut meta, 8);
    let rela_at = u64_of(meta.len(), "meta offset")?;
    let got_end = laid.got_at + 8 * u64_of(laid.imports.len(), "program GOT length")?;
    let mut relative = 0u64;
    // All `R_AARCH64_RELATIVE` entries first (`DT_RELACOUNT`), program then native (sorted).
    for f in laid.fixups.iter() {
        match f.to {
            FixTo::Rebase { target } => {
                bytes::u64(&mut meta, f.at);
                bytes::u64(&mut meta, R_AARCH64_RELATIVE);
                bytes::u64(&mut meta, target);
                relative += 1;
            }
            FixTo::Bind { import } => {
                if let Some(target) = direct(import) {
                    bytes::u64(&mut meta, f.at);
                    bytes::u64(&mut meta, R_AARCH64_RELATIVE);
                    bytes::u64(&mut meta, target);
                    relative += 1;
                }
            }
        }
    }
    if let Some(m) = native {
        for &(at, target) in m.relative.iter() {
            bytes::u64(&mut meta, at);
            bytes::u64(&mut meta, R_AARCH64_RELATIVE);
            bytes::u64(&mut meta, target);
            relative += 1;
        }
    }
    // Non-relative relocations: program's remaining binds, then native's imports & TLS relocs.
    for f in laid.fixups.iter() {
        if let FixTo::Bind { import } = f.to
            && direct(import).is_none()
        {
            let kind = if f.at >= laid.got_at && f.at < got_end { R_AARCH64_GLOB_DAT } else { R_AARCH64_ABS64 };
            let idx = usize::try_from(import).map_err(|_| ExportError::TooLarge { what: "a program import index", len: u64::from(import) })?;
            let Some(&row) = row_of.get(idx) else {
                return Err(ExportError::TooLarge { what: "a program import index", len: u64::from(import) });
            };
            let sym = u64::from(row) + 1;
            bytes::u64(&mut meta, f.at);
            bytes::u64(&mut meta, (sym << 32) | kind);
            bytes::u64(&mut meta, 0);
        }
    }
    if let Some(m) = native {
        for r in m.other.iter() {
            bytes::u64(&mut meta, r.at);
            match r.kind {
                ElfRelaKind::Import { import, addend, got } => {
                    let kind = if got && addend == 0 { R_AARCH64_GLOB_DAT } else { R_AARCH64_ABS64 };
                    let sym = native_sym_base + u64::from(import);
                    bytes::u64(&mut meta, (sym << 32) | kind);
                    meta.extend_from_slice(&addend.to_le_bytes());
                }
                ElfRelaKind::TlsDesc { offset } => {
                    bytes::u64(&mut meta, R_AARCH64_TLSDESC);
                    bytes::u64(&mut meta, offset);
                }
                ElfRelaKind::TlsTpRel { offset } => {
                    bytes::u64(&mut meta, R_AARCH64_TLS_TPREL64);
                    bytes::u64(&mut meta, offset);
                }
            }
        }
    }
    let rela_len = u64_of(meta.len(), "meta offset")? - rela_at;

    // Single `PT_TLS` combining optional native C TLS (`[0, c_memsz)`) and optional Rust
    // `Space::Tls` 8-byte zero-initialized `ROOT` at `up(c_memsz, 8)`.
    let tls_phdr = match (laid.has_tls, native.and_then(|m| m.tls.map(|t| (m, t)))) {
        (false, None) => None,
        (true, None) => Some(TlsPhdr {
            offset: laid.data_at,
            vaddr: laid.data_at,
            filesz: 0,
            memsz: TLS_ROOT_MEMSZ,
            align: TLS_ROOT_ALIGN,
        }),
        (has_rust_tls, Some((m, c_tls))) => {
            let align = if has_rust_tls { c_tls.align.max(TLS_ROOT_ALIGN) } else { c_tls.align };
            let memsz = if has_rust_tls { up(c_tls.memsz, 8) + TLS_ROOT_MEMSZ } else { c_tls.memsz };
            let filesz = c_tls.filesz;
            let (offset, vaddr) = if filesz == 0 {
                (native_file + native_code_len, m.data_at)
            } else if c_tls.vaddr.is_multiple_of(align) {
                (m.file_of(c_tls.vaddr, native_file), c_tls.vaddr)
            } else {
                // Copy `.tdata` into read-only `meta` at an `align`-aligned offset so
                // `p_vaddr % p_align == 0` and `p_offset % p_align == 0` always hold.
                let align_usize = usize_of(align, "PT_TLS alignment")?;
                bytes::pad(&mut meta, align_usize);
                let in_meta = u64_of(meta.len(), "meta offset")?;
                let src_at = usize_of(c_tls.vaddr - m.text_at, "native .tdata offset")?;
                let src_len = usize_of(filesz, "native .tdata length")?;
                let Some(tdata_bytes) = m.bytes.get(src_at..src_at + src_len) else {
                    return Err(ExportError::Native(MergeError::Shape { what: "native .tdata past image bytes", at: c_tls.vaddr }));
                };
                meta.extend_from_slice(tdata_bytes);
                (meta_off + in_meta, meta_at + in_meta)
            };
            Some(TlsPhdr { offset, vaddr, filesz, memsz, align })
        }
    };

    let meta_len = u64_of(meta.len(), "meta length")?;
    if meta_at + meta_len > 1 << 32 {
        return Err(ExportError::TooLarge { what: "the image's 4 GiB", len: meta_at + meta_len });
    }

    // Dynamic section entries (written into the reserved head of `laid.konst`).
    let mut dyn_entries: AVec<'_, (u64, u64)> = Vec::with_capacity_in(16 + extra_needed.len(), alloc);
    dyn_entries.push((DT_NEEDED, u64::from(needed_libc)));
    for &at in extra_needed.iter() {
        dyn_entries.push((DT_NEEDED, u64::from(at)));
    }
    if let Some(inits) = native.and_then(|m| m.inits) {
        dyn_entries.push((DT_INIT_ARRAY, inits.vaddr));
        dyn_entries.push((DT_INIT_ARRAYSZ, inits.len));
    }
    let dt_flags = if native.is_some_and(|m| m.static_tls) { DF_BIND_NOW | DF_STATIC_TLS } else { DF_BIND_NOW };
    dyn_entries.extend_from_slice(&[
        (DT_HASH, meta_at + hash_at),
        (DT_STRTAB, meta_at + dynstr_at),
        (DT_SYMTAB, meta_at + dynsym_at),
        (DT_STRSZ, u64_of(dynstr.len(), "DT_STRSZ")?),
        (DT_SYMENT, 24),
        (DT_RELA, meta_at + rela_at),
        (DT_RELASZ, rela_len),
        (DT_RELAENT, 24),
        (DT_RELACOUNT, relative),
        (DT_FLAGS, dt_flags),
        (DT_FLAGS_1, DF_1),
        (DT_DEBUG, 0),
        (DT_NULL, 0),
    ]);
    let dyn_bytes = u64_of(dyn_entries.len(), "PT_DYNAMIC entry count")? * 16;
    let dyn_room = laid.got_at.saturating_sub(laid.const_at);
    if dyn_bytes > dyn_room {
        return Err(ExportError::HeaderRoom { what: "the ELF dynamic section", need: dyn_bytes, room: dyn_room });
    }

    // The ELF header and program headers.
    out.clear();
    out.extend_from_slice(&[0x7F, b'E', b'L', b'F', 2, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    let native_missing = if native.is_some() { 0u64 } else { 2u64 };
    let phnum = MAX_PHDRS - u64::from(!has_data) - u64::from(tls_phdr.is_none()) - native_missing;
    let phnum16 = u16::try_from(phnum).map_err(|_| ExportError::TooLarge { what: "e_phnum", len: phnum })?;
    bytes::u16(out, 3); // ET_DYN
    bytes::u16(out, 183); // EM_AARCH64
    bytes::u32(out, 1); // EV_CURRENT
    bytes::u64(out, laid.entry);
    bytes::u64(out, EHDR); // e_phoff
    bytes::u64(out, 0); // e_shoff
    bytes::u32(out, 0); // e_flags
    bytes::u16(out, 64); // e_ehsize
    bytes::u16(out, 56); // e_phentsize
    bytes::u16(out, phnum16);
    bytes::u16(out, 64); // e_shentsize
    bytes::u16(out, 0); // e_shnum
    bytes::u16(out, 0); // e_shstrndx
    phdr(out, PT_PHDR, PF_R, EHDR, EHDR, PHDR * phnum, PHDR * phnum, 8);
    let interp_len = u64_of(INTERP.len(), "PT_INTERP")? + 1;
    phdr(out, PT_INTERP, PF_R, INTERP_AT, INTERP_AT, interp_len, interp_len, 1);
    phdr(out, PT_NOTE, PF_R, NOTE_AT, NOTE_AT, NOTE_LEN, NOTE_LEN, 4);
    // The executable load holds the code and, after it, the unwind tables (eh_out.rs).
    phdr(out, PT_LOAD, PF_R | PF_X, 0, 0, eh.end(), eh.end(), PAGE);
    phdr(out, PT_LOAD, PF_R | PF_W, laid.const_at, laid.const_at, konst_len, const_mem, PAGE);
    if has_data {
        phdr(out, PT_LOAD, PF_R | PF_W, laid.data_at, laid.data_at, data_len, laid.end - laid.data_at, PAGE);
    }
    if let Some(m) = native {
        phdr(out, PT_LOAD, PF_R | PF_X, native_file, m.text_at, native_code_len, native_code_len, PAGE);
        phdr(out, PT_LOAD, PF_R | PF_W, native_file + native_code_len, m.data_at, native_data_filesz, m.end - m.data_at, PAGE);
    }
    phdr(out, PT_LOAD, PF_R, meta_off, meta_at, meta_len, meta_len, PAGE);
    phdr(out, PT_DYNAMIC, PF_R | PF_W, laid.const_at, laid.const_at, dyn_bytes, dyn_bytes, 8);
    if let Some(t) = tls_phdr {
        phdr(out, PT_TLS, PF_R, t.offset, t.vaddr, t.filesz, t.memsz, t.align);
    }
    phdr(out, PT_GNU_RELRO, PF_R, laid.const_at, laid.const_at, konst_len, const_mem, 1);
    let hdr_len = u64_of(eh.eh_frame_hdr.len(), "PT_GNU_EH_FRAME")?;
    phdr(out, PT_GNU_EH_FRAME, PF_R, eh.eh_frame_hdr_at, eh.eh_frame_hdr_at, hdr_len, hdr_len, 4);
    phdr(out, PT_GNU_STACK, PF_R | PF_W, 0, 0, 0, 0, 16);
    let ph_written = u64_of(out.len(), "program headers")?;
    if ph_written != EHDR + phnum * PHDR {
        return Err(ExportError::Remeasured { what: "the ELF program headers", measured: EHDR + phnum * PHDR, written: ph_written });
    }
    out.resize(usize_of(INTERP_AT, "INTERP_AT")?, 0);
    out.extend_from_slice(INTERP.as_bytes());
    out.push(0);
    out.resize(usize_of(NOTE_AT, "NOTE_AT")?, 0);
    bytes::u32(out, 4); // namesz
    bytes::u32(out, 16); // descsz
    bytes::u32(out, NT_GNU_BUILD_ID);
    out.extend_from_slice(b"GNU\0");
    let id_at = out.len();
    out.extend_from_slice(&[0u8; 16]);

    // The loads' bytes at their offsets.
    out.resize(usize_of(laid.text_at, "text_at")?, 0);
    out.extend_from_slice(&laid.text);
    out.resize(usize_of(eh.eh_frame_at, "eh_frame_at")?, 0);
    out.extend_from_slice(&eh.eh_frame);
    out.resize(usize_of(eh.except_tab_at, "except_tab_at")?, 0);
    out.extend_from_slice(&eh.except_tab);
    out.resize(usize_of(eh.eh_frame_hdr_at, "eh_frame_hdr_at")?, 0);
    out.extend_from_slice(&eh.eh_frame_hdr);
    out.resize(usize_of(laid.const_at, "const_at")?, 0);
    out.extend_from_slice(&laid.konst);
    let const_at_usize = usize_of(laid.const_at, "const_at")?;
    for (k, &(tag, value)) in dyn_entries.iter().enumerate() {
        let at = const_at_usize + 16 * k;
        bytes::set64(out, at, tag);
        bytes::set64(out, at + 8, value);
    }
    out.resize(usize_of(laid.const_at + const_mem, "const end")?, 0);
    if !laid.data.is_empty() {
        out.resize(usize_of(laid.data_at, "data_at")?, 0);
        out.extend_from_slice(&laid.data);
    }
    if let Some(m) = native {
        let native_file_usize = usize_of(native_file, "native_file")?;
        out.resize(native_file_usize, 0);
        out.extend_from_slice(m.bytes);
        let native_end = usize_of(native_file + native_code_len + native_data_filesz, "native end")?;
        if out.len() < native_end {
            out.resize(native_end, 0);
        }
        // RELA convention: every relocated native cell holds 0 in the file (addend is in Elf64_Rela).
        for &(at, _) in m.relative.iter() {
            let off = usize_of(m.file_of(at, native_file), "native relative slot")?;
            bytes::set64(out, off, 0);
        }
        for r in m.other.iter() {
            let off = usize_of(m.file_of(r.at, native_file), "native relocation slot")?;
            bytes::set64(out, off, 0);
            if matches!(r.kind, ElfRelaKind::TlsDesc { .. }) {
                bytes::set64(out, off + 8, 0);
            }
        }
    }
    out.resize(usize_of(meta_off, "meta_off")?, 0);
    out.extend_from_slice(&meta);

    let mut hasher = Hasher::new(ID_DOMAIN);
    hasher.update(seed);
    hasher.update(&out[..]);
    let digest = hasher.finish().0;
    let mut id = [0u8; 16];
    id.copy_from_slice(&digest[..16]);
    out[id_at..id_at + 16].copy_from_slice(&id);
    Ok(id)
}
