//! The descriptor's reader for a Linux record: an ELF shared object as blaze-export's W5 writes
//! it (native_so.rs, NATIVE.md W5). The descriptor is the first 48 bytes of the section named
//! `.data` (found through the section-name table, never by an offset guess), exactly as on macOS
//! it is the first 48 bytes of `__DATA,__data`; its image-relative addresses (`name_at`) map to
//! file offsets through the `PT_LOAD` program headers, as the Mach-O half maps them through the
//! `LC_SEGMENT_64` commands. The shared tail (tag check, names table) is the parent's
//! [`super::descriptor_at`], so both halves give one descriptor for one image.
//!
//! Structures from the System V gABI, "Object Files" (`Elf64_Ehdr`: `e_ident` "\x7fELF",
//! `EI_CLASS` at 4 = `ELFCLASS64` 2, `EI_DATA` at 5 = `ELFDATA2LSB` 1, `EI_VERSION` at 6 =
//! `EV_CURRENT` 1; `e_type` u16 at 16, `ET_DYN` 3; `e_machine` u16 at 18; `e_version` u32 at 20;
//! `e_phoff` at 32; `e_shoff` at 40; `e_ehsize` at 52; `e_phentsize` at 54; `e_phnum` at 56;
//! `e_shentsize` at 58; `e_shnum` at 60; `e_shstrndx` at 62. `Elf64_Phdr`, 56 bytes: `p_type` at
//! 0, `p_offset` at 8, `p_vaddr` at 16, `p_filesz` at 32; `PT_LOAD` 1. `Elf64_Shdr`, 64 bytes:
//! `sh_name` at 0, `sh_type` at 4, `sh_offset` at 24, `sh_size` at 32, `sh_link` at 40,
//! `sh_info` at 44; `SHT_PROGBITS` 1, `SHT_STRTAB` 3. "Extended Section Numbering": `e_shnum` 0
//! means the count is section 0's `sh_size`, `e_shstrndx` `SHN_XINDEX` (0xffff) means the index
//! is section 0's `sh_link`, `e_phnum` `PN_XNUM` (0xffff) means the count is section 0's
//! `sh_info`). Machines: `EM_X86_64` 62 and `EM_AARCH64` 183 (glibc `<elf.h>`); the descriptor's
//! form is the same on both, so either is read; any other is refused by name.
//!
//! Every field is read through a bounds check: a value past the bytes is
//! [`DescriptorError::Truncated`] naming the field, a value this reader refuses is
//! [`DescriptorError::ElfField`] naming the field and holding what it held.

use super::{Descriptor, DescriptorError, LEN, Names, descriptor_at};

const MAGIC: u32 = u32::from_le_bytes([0x7f, b'E', b'L', b'F']);
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
const EV_CURRENT: u8 = 1;
const ET_DYN: u16 = 3;
const EM_X86_64: u16 = 62;
const EM_AARCH64: u16 = 183;
const EHDR: u16 = 64;
const PHDR: u16 = 56;
const SHDR: u16 = 64;
const PT_LOAD: u32 = 1;
const SHT_PROGBITS: u32 = 1;
const SHT_STRTAB: u32 = 3;
const SHN_XINDEX: u16 = 0xffff;
const PN_XNUM: u16 = 0xffff;
/// The section the descriptor starts (native_so.rs's `Region::Data` row).
const DATA: &[u8] = b".data";

fn bytes<'f>(file: &'f [u8], at: u64, len: u64, what: &'static str) -> Result<&'f [u8], DescriptorError> {
    let truncated = DescriptorError::Truncated { what, at };
    let start = usize::try_from(at).map_err(|_| truncated)?;
    let end = at.checked_add(len).and_then(|e| usize::try_from(e).ok()).ok_or(truncated)?;
    file.get(start..end).ok_or(truncated)
}

fn u8_at(file: &[u8], at: u64, what: &'static str) -> Result<u8, DescriptorError> {
    Ok(bytes(file, at, 1, what)?[0])
}

fn u16_at(file: &[u8], at: u64, what: &'static str) -> Result<u16, DescriptorError> {
    let b = bytes(file, at, 2, what)?;
    Ok(u16::from_le_bytes([b[0], b[1]]))
}

fn u32_at(file: &[u8], at: u64, what: &'static str) -> Result<u32, DescriptorError> {
    let b = bytes(file, at, 4, what)?;
    Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn u64_at(file: &[u8], at: u64, what: &'static str) -> Result<u64, DescriptorError> {
    let b = bytes(file, at, 8, what)?;
    Ok(u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
}

fn refuse<T>(field: &'static str, got: u64) -> Result<T, DescriptorError> {
    Err(DescriptorError::ElfField { field, got })
}

/// The file offset of a table's row `index` of `size` bytes from `base`, checked for overflow.
fn row(base: u64, index: u64, size: u16, what: &'static str) -> Result<u64, DescriptorError> {
    index
        .checked_mul(u64::from(size))
        .and_then(|o| base.checked_add(o))
        .ok_or(DescriptorError::Truncated { what, at: base })
}

/// The header fields the reader uses, each checked.
struct Header {
    phoff: u64,
    phnum: u64,
    shoff: u64,
    shnum: u64,
    shstrndx: u64,
}

fn header(file: &[u8]) -> Result<Header, DescriptorError> {
    let magic = u32_at(file, 0, "e_ident")?;
    if magic != MAGIC {
        return refuse("e_ident[EI_MAG0..=EI_MAG3]", u64::from(magic));
    }
    let class = u8_at(file, 4, "e_ident")?;
    if class != ELFCLASS64 {
        return refuse("e_ident[EI_CLASS] (ELFCLASS64 only)", u64::from(class));
    }
    let data = u8_at(file, 5, "e_ident")?;
    if data != ELFDATA2LSB {
        return refuse("e_ident[EI_DATA] (ELFDATA2LSB only)", u64::from(data));
    }
    let version = u8_at(file, 6, "e_ident")?;
    if version != EV_CURRENT {
        return refuse("e_ident[EI_VERSION]", u64::from(version));
    }
    let kind = u16_at(file, 16, "e_type")?;
    if kind != ET_DYN {
        return refuse("e_type (ET_DYN only)", u64::from(kind));
    }
    let machine = u16_at(file, 18, "e_machine")?;
    if machine != EM_AARCH64 && machine != EM_X86_64 {
        return refuse("e_machine (EM_AARCH64 or EM_X86_64 only)", u64::from(machine));
    }
    let e_version = u32_at(file, 20, "e_version")?;
    if e_version != u32::from(EV_CURRENT) {
        return refuse("e_version", u64::from(e_version));
    }
    let phoff = u64_at(file, 32, "e_phoff")?;
    let shoff = u64_at(file, 40, "e_shoff")?;
    let ehsize = u16_at(file, 52, "e_ehsize")?;
    if ehsize != EHDR {
        return refuse("e_ehsize", u64::from(ehsize));
    }
    let phentsize = u16_at(file, 54, "e_phentsize")?;
    let phnum = u16_at(file, 56, "e_phnum")?;
    let shentsize = u16_at(file, 58, "e_shentsize")?;
    let shnum = u16_at(file, 60, "e_shnum")?;
    let shstrndx = u16_at(file, 62, "e_shstrndx")?;
    if shoff == 0 {
        return refuse("e_shoff (no section headers: `.data` cannot be found by name)", 0);
    }
    if shentsize != SHDR {
        return refuse("e_shentsize", u64::from(shentsize));
    }
    if phnum != 0 && phentsize != PHDR {
        return refuse("e_phentsize", u64::from(phentsize));
    }
    // Extended numbering: section 0 holds the true values (a saturated offset is past the bytes,
    // so it reads as Truncated).
    let shnum = if shnum == 0 { u64_at(file, shoff.saturating_add(32), "section 0's sh_size (the extended e_shnum)")? } else { u64::from(shnum) };
    let shstrndx = if shstrndx == SHN_XINDEX { u64::from(u32_at(file, shoff.saturating_add(40), "section 0's sh_link (the extended e_shstrndx)")?) } else { u64::from(shstrndx) };
    let phnum = if phnum == PN_XNUM { u64::from(u32_at(file, shoff.saturating_add(44), "section 0's sh_info (the extended e_phnum)")?) } else { u64::from(phnum) };
    if shstrndx == 0 || shstrndx >= shnum {
        return refuse("e_shstrndx (no section-name table among the sections)", shstrndx);
    }
    // The tables lie wholly inside the bytes.
    let sh_end = row(shoff, shnum, SHDR, "the section headers")?;
    bytes(file, shoff, sh_end - shoff, "the section headers")?;
    let ph_end = row(phoff, phnum, PHDR, "the program headers")?;
    bytes(file, phoff, ph_end - phoff, "the program headers")?;
    Ok(Header { phoff, phnum, shoff, shnum, shstrndx })
}

/// The file offset of image address `at`, through the `PT_LOAD` program headers' file bytes.
fn file_offset(file: &[u8], h: &Header, at: u64, what: &'static str) -> Result<usize, DescriptorError> {
    for k in 0..h.phnum {
        let p = row(h.phoff, k, PHDR, "a program header")?;
        if u32_at(file, p, "a program header's p_type")? != PT_LOAD {
            continue;
        }
        let offset = u64_at(file, p + 8, "a program header's p_offset")?;
        let vaddr = u64_at(file, p + 16, "a program header's p_vaddr")?;
        let filesz = u64_at(file, p + 32, "a program header's p_filesz")?;
        if at >= vaddr && at - vaddr < filesz {
            let off = offset.checked_add(at - vaddr).ok_or(DescriptorError::Truncated { what, at })?;
            // The mapped byte is in the file (a PT_LOAD may claim bytes the file lacks).
            bytes(file, off, 1, what)?;
            return usize::try_from(off).map_err(|_| DescriptorError::Truncated { what, at: off });
        }
    }
    Err(DescriptorError::Unmapped { what, at })
}

/// The descriptor and the names table of a W5 shared object, read in place and checked.
pub(super) fn read(file: &[u8]) -> Result<(Descriptor, Names<'_>), DescriptorError> {
    let h = header(file)?;
    let names_sh = row(h.shoff, h.shstrndx, SHDR, "the section-name table's header")?;
    let names_kind = u32_at(file, names_sh + 4, "the section-name table's sh_type")?;
    if names_kind != SHT_STRTAB {
        return refuse("the section-name table's sh_type (SHT_STRTAB only)", u64::from(names_kind));
    }
    let names_off = u64_at(file, names_sh + 24, "the section-name table's sh_offset")?;
    let names_len = u64_at(file, names_sh + 32, "the section-name table's sh_size")?;
    let names = bytes(file, names_off, names_len, "the section-name table")?;

    // `.data`, by name: exactly one.
    let mut data: Option<u64> = None;
    for k in 1..h.shnum {
        let s = row(h.shoff, k, SHDR, "a section header")?;
        let name = u64::from(u32_at(file, s, "a section's sh_name")?);
        let tail = usize::try_from(name).ok().and_then(|n| names.get(n..)).ok_or(DescriptorError::Truncated { what: "a section's sh_name", at: s })?;
        let end = tail.iter().position(|&c| c == 0).ok_or(DescriptorError::Truncated { what: "a section's name (no NUL in the section-name table)", at: s })?;
        if &tail[..end] != DATA {
            continue;
        }
        if data.is_some() {
            return refuse("a second `.data` section's index", k);
        }
        data = Some(s);
    }
    let s = data.ok_or(DescriptorError::NoData)?;
    let kind = u32_at(file, s + 4, "`.data`'s sh_type")?;
    if kind != SHT_PROGBITS {
        return refuse("`.data`'s sh_type (SHT_PROGBITS only)", u64::from(kind));
    }
    let offset = u64_at(file, s + 24, "`.data`'s sh_offset")?;
    let size = u64_at(file, s + 32, "`.data`'s sh_size")?;
    if size < LEN {
        return Err(DescriptorError::Truncated { what: "the descriptor (`.data` shorter than 48 bytes)", at: offset });
    }
    bytes(file, offset, LEN, "the descriptor")?;
    let off = usize::try_from(offset).map_err(|_| DescriptorError::Truncated { what: "the descriptor", at: offset })?;
    descriptor_at(file, off, |at, what| file_offset(file, &h, at, what))
}
