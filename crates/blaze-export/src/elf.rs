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
use blaze_own::{AVec, Alloc, Vec};

use crate::bytes;
use crate::eh_out::EhSections;
use crate::error::ExportError;
use crate::layout::{FixTo, Laid, Plan};

pub const PAGE: u64 = 0x1_0000;

pub(crate) const EHDR: u64 = 64;
pub(crate) const PHDR: u64 = 56;
/// Every program header at the most: PHDR, INTERP, NOTE, four LOADs, DYNAMIC, TLS, GNU_RELRO,
/// GNU_EH_FRAME, GNU_STACK.
const MAX_PHDRS: u64 = 12;
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
pub(crate) const DT_FLAGS: u64 = 30;
pub(crate) const DT_RELACOUNT: u64 = 0x6FFF_FFF9;
pub(crate) const DT_FLAGS_1: u64 = 0x6FFF_FFFB;
pub(crate) const DF_BIND_NOW: u64 = 0x8;
/// `DF_1_NOW | DF_1_PIE`.
const DF_1: u64 = 0x1 | 0x0800_0000;
/// The dynamic section's entries, in order (`dynamic` writes them).
const DYN_COUNT: u64 = 14;

pub(crate) const R_AARCH64_ABS64: u64 = 257;
pub(crate) const R_AARCH64_GLOB_DAT: u64 = 1025;
pub(crate) const R_AARCH64_RELATIVE: u64 = 1027;
/// `STB_GLOBAL << 4 | STT_FUNC`.
const GLOBAL_FUNC: u8 = 0x12;

pub(crate) const LIBC: &str = "libc.so.6";
const ID_DOMAIN: &str = "blaze-export elf build id 1";

/// The layout plan an ELF file needs: the dynamic section heads the const region.
pub fn plan() -> Plan {
    Plan { page: PAGE, text_at: TEXT_AT, const_head: DYN_COUNT * 16 }
}

fn up(n: u64, to: u64) -> u64 {
    (n + to - 1) & !(to - 1)
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

/// Writes `laid` as an ELF executable into `out` (its contents replaced). `seed` joins the
/// content in the build id (the profile's digest, or zeros). Returns the build id.
pub fn write<'a>(laid: &Laid<'_, '_>, eh: &EhSections<'_>, seed: &[u8; 32], out: &mut AVec<'a, u8>, alloc: Alloc<'a>) -> Result<[u8; 16], ExportError> {
    let n = laid.imports.len();
    let has_data = !laid.data.is_empty() || laid.zero_len > 0;
    let const_mem = up(laid.konst.len() as u64, PAGE);
    let mut file_end = laid.const_at + const_mem;
    if !laid.data.is_empty() {
        file_end = laid.data_at + up(laid.data.len() as u64, PAGE);
    }
    let meta_off = file_end;
    let meta_at = laid.end;

    // The meta load: symbols, names, hash, relocations.
    let mut meta: AVec<'_, u8> = Vec::new_in(alloc);
    let dynsym_at = 0u64;
    meta.extend_from_slice(&[0u8; 24]);
    let mut dynstr: AVec<'_, u8> = Vec::new_in(alloc);
    dynstr.push(0);
    let needed = dynstr.len() as u32;
    dynstr.extend_from_slice(LIBC.as_bytes());
    dynstr.push(0);
    for import in laid.imports.iter() {
        // `Elf64_Sym`: st_name, st_info, st_other, st_shndx (SHN_UNDEF), st_value, st_size.
        bytes::u32(&mut meta, dynstr.len() as u32);
        bytes::u8(&mut meta, GLOBAL_FUNC);
        bytes::u8(&mut meta, 0);
        bytes::u16(&mut meta, 0);
        bytes::u64(&mut meta, 0);
        bytes::u64(&mut meta, 0);
        dynstr.extend_from_slice(import.name.as_bytes());
        dynstr.push(0);
    }
    let dynstr_at = meta.len() as u64;
    meta.extend_from_slice(&dynstr);
    bytes::pad(&mut meta, 8);
    let hash_at = meta.len() as u64;
    let nsyms = 1 + n as u32;
    bytes::u32(&mut meta, 1); // nbucket
    bytes::u32(&mut meta, nsyms); // nchain
    bytes::u32(&mut meta, 0); // bucket[0]: no symbol is defined here
    for _ in 0..nsyms {
        bytes::u32(&mut meta, 0);
    }
    bytes::pad(&mut meta, 8);
    let rela_at = meta.len() as u64;
    let got_end = laid.got_at + 8 * n as u64;
    let mut relative = 0u64;
    for f in laid.fixups.iter() {
        if let FixTo::Rebase { target } = f.to {
            bytes::u64(&mut meta, f.at);
            bytes::u64(&mut meta, R_AARCH64_RELATIVE);
            bytes::u64(&mut meta, target);
            relative += 1;
        }
    }
    for f in laid.fixups.iter() {
        if let FixTo::Bind { import } = f.to {
            let kind = if f.at >= laid.got_at && f.at < got_end { R_AARCH64_GLOB_DAT } else { R_AARCH64_ABS64 };
            bytes::u64(&mut meta, f.at);
            bytes::u64(&mut meta, (u64::from(import + 1) << 32) | kind);
            bytes::u64(&mut meta, 0);
        }
    }
    let rela_len = meta.len() as u64 - rela_at;
    if meta_at + meta.len() as u64 > 1 << 32 {
        return Err(ExportError::TooLarge { what: "the image's 4 GiB", len: meta_at + meta.len() as u64 });
    }

    // The ELF header and program headers.
    out.clear();
    out.extend_from_slice(&[0x7F, b'E', b'L', b'F', 2, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    let phnum = MAX_PHDRS - u64::from(!has_data) - u64::from(!laid.has_tls);
    bytes::u16(out, 3); // ET_DYN
    bytes::u16(out, 183); // EM_AARCH64
    bytes::u32(out, 1); // EV_CURRENT
    bytes::u64(out, laid.entry);
    bytes::u64(out, EHDR); // e_phoff
    bytes::u64(out, 0); // e_shoff
    bytes::u32(out, 0); // e_flags
    bytes::u16(out, EHDR as u16);
    bytes::u16(out, PHDR as u16);
    bytes::u16(out, phnum as u16);
    bytes::u16(out, 64); // e_shentsize
    bytes::u16(out, 0); // e_shnum
    bytes::u16(out, 0); // e_shstrndx
    phdr(out, PT_PHDR, PF_R, EHDR, EHDR, PHDR * phnum, PHDR * phnum, 8);
    let interp_len = INTERP.len() as u64 + 1;
    phdr(out, PT_INTERP, PF_R, INTERP_AT, INTERP_AT, interp_len, interp_len, 1);
    phdr(out, PT_NOTE, PF_R, NOTE_AT, NOTE_AT, NOTE_LEN, NOTE_LEN, 4);
    // The executable load holds the code and, after it, the unwind tables (eh_out.rs).
    phdr(out, PT_LOAD, PF_R | PF_X, 0, 0, eh.end(), eh.end(), PAGE);
    let konst = laid.konst.len() as u64;
    phdr(out, PT_LOAD, PF_R | PF_W, laid.const_at, laid.const_at, konst, const_mem, PAGE);
    if has_data {
        let data = laid.data.len() as u64;
        phdr(out, PT_LOAD, PF_R | PF_W, laid.data_at, laid.data_at, data, laid.end - laid.data_at, PAGE);
    }
    let meta_len = meta.len() as u64;
    phdr(out, PT_LOAD, PF_R, meta_off, meta_at, meta_len, meta_len, PAGE);
    phdr(out, PT_DYNAMIC, PF_R | PF_W, laid.const_at, laid.const_at, DYN_COUNT * 16, DYN_COUNT * 16, 8);
    if laid.has_tls {
        // Zero-initialized 8-byte `ROOT` pointer per thread at `tpidr_el0 + TLS_ROOT_OFFSET`
        // (`align_up(16, 16) = 16`). `laid.data_at` is page-aligned (`p_vaddr % p_align == 0`)
        // and inside a mapped `PT_LOAD` segment.
        phdr(out, PT_TLS, PF_R, laid.data_at, laid.data_at, 0, TLS_ROOT_MEMSZ, TLS_ROOT_ALIGN);
    }
    phdr(out, PT_GNU_RELRO, PF_R, laid.const_at, laid.const_at, konst, const_mem, 1);
    let hdr_len = eh.eh_frame_hdr.len() as u64;
    phdr(out, PT_GNU_EH_FRAME, PF_R, eh.eh_frame_hdr_at, eh.eh_frame_hdr_at, hdr_len, hdr_len, 4);
    phdr(out, PT_GNU_STACK, PF_R | PF_W, 0, 0, 0, 0, 16);
    out.resize(INTERP_AT as usize, 0);
    out.extend_from_slice(INTERP.as_bytes());
    out.push(0);
    out.resize(NOTE_AT as usize, 0);
    bytes::u32(out, 4); // namesz
    bytes::u32(out, 16); // descsz
    bytes::u32(out, NT_GNU_BUILD_ID);
    out.extend_from_slice(b"GNU\0");
    let id_at = out.len();
    out.extend_from_slice(&[0u8; 16]);

    // The loads' bytes at their offsets.
    out.resize(laid.text_at as usize, 0);
    out.extend_from_slice(&laid.text);
    out.resize(eh.eh_frame_at as usize, 0);
    out.extend_from_slice(&eh.eh_frame);
    out.resize(eh.except_tab_at as usize, 0);
    out.extend_from_slice(&eh.except_tab);
    out.resize(eh.eh_frame_hdr_at as usize, 0);
    out.extend_from_slice(&eh.eh_frame_hdr);
    out.resize(laid.const_at as usize, 0);
    out.extend_from_slice(&laid.konst);
    // Exactly `DYN_COUNT` entries by type: the plan's `const_head` and `PT_DYNAMIC` name that many.
    let entries: [(u64, u64); DYN_COUNT as usize] = [
        (DT_NEEDED, u64::from(needed)),
        (DT_HASH, meta_at + hash_at),
        (DT_STRTAB, meta_at + dynstr_at),
        (DT_SYMTAB, meta_at + dynsym_at),
        (DT_STRSZ, dynstr.len() as u64),
        (DT_SYMENT, 24),
        (DT_RELA, meta_at + rela_at),
        (DT_RELASZ, rela_len),
        (DT_RELAENT, 24),
        (DT_RELACOUNT, relative),
        (DT_FLAGS, DF_BIND_NOW),
        (DT_FLAGS_1, DF_1),
        (DT_DEBUG, 0),
        (DT_NULL, 0),
    ];
    for (k, (tag, value)) in entries.iter().enumerate() {
        let at = laid.const_at as usize + 16 * k;
        bytes::set64(out, at, *tag);
        bytes::set64(out, at + 8, *value);
    }
    out.resize((laid.const_at + const_mem) as usize, 0);
    if !laid.data.is_empty() {
        out.resize(laid.data_at as usize, 0);
        out.extend_from_slice(&laid.data);
    }
    out.resize(meta_off as usize, 0);
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
