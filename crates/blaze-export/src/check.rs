//! Readers that check a written file against the formats' own structures, independent of the
//! writers: every offset and size in bounds, every load page-aligned, every chained fixup and
//! relocation inside a writable region and naming something that exists, and on Mach-O every
//! code-signature page hash equal to the SHA-256 of the page as it is in the file. The tests run
//! them on every file written; nothing else trusts them.

use core::fmt;

use crate::bytes::{get_be32, get_be64, get16, get32, get64};
use crate::sign;

/// What a checker found wrong, naming the structure and where.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CheckError {
    /// `what` at file offset `at` runs past the file (or past the structure holding it).
    Bounds { what: &'static str, at: u64 },
    /// `what` at `at` is not aligned as its format demands.
    Align { what: &'static str, at: u64 },
    /// `what` holds `value`, which its format (or the writer's promise) does not allow.
    Field { what: &'static str, value: u64 },
    /// The code directory's hash of page `page` is not the SHA-256 of that page.
    PageHash { page: u32 },
    /// A structure the file needs is not there.
    Missing { what: &'static str },
}

impl fmt::Display for CheckError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            CheckError::Bounds { what, at } => write!(f, "check: {what} at {at:#x} runs past what holds it"),
            CheckError::Align { what, at } => write!(f, "check: {what} at {at:#x} is not aligned"),
            CheckError::Field { what, value } => write!(f, "check: {what} is {value:#x}, which the format does not allow"),
            CheckError::PageHash { page } => write!(f, "check: the code directory's hash of page {page} is not the page's SHA-256"),
            CheckError::Missing { what } => write!(f, "check: the file has no {what}"),
        }
    }
}

impl core::error::Error for CheckError {}

fn within(file: &[u8], at: u64, len: u64, what: &'static str) -> Result<(), CheckError> {
    match at.checked_add(len) {
        Some(end) if end <= file.len() as u64 => Ok(()),
        _ => Err(CheckError::Bounds { what, at }),
    }
}

fn r16(file: &[u8], at: u64, what: &'static str) -> Result<u16, CheckError> {
    within(file, at, 2, what)?;
    Ok(get16(file, at as usize))
}

fn r32(file: &[u8], at: u64, what: &'static str) -> Result<u32, CheckError> {
    within(file, at, 4, what)?;
    Ok(get32(file, at as usize))
}

fn r64(file: &[u8], at: u64, what: &'static str) -> Result<u64, CheckError> {
    within(file, at, 8, what)?;
    Ok(get64(file, at as usize))
}

fn field(ok: bool, what: &'static str, value: u64) -> Result<(), CheckError> {
    match ok {
        true => Ok(()),
        false => Err(CheckError::Field { what, value }),
    }
}

/// A Mach-O segment as read.
#[derive(Clone, Copy, Default)]
struct MSeg {
    name: [u8; 16],
    vm: u64,
    vm_size: u64,
    file: u64,
    file_size: u64,
    prot: u32,
}

const MAX_SEGS: usize = 8;

/// What a checked Mach-O file says.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MachoFacts {
    /// The entry, as an offset from `__TEXT`.
    pub entry: u64,
    pub rebases: u32,
    pub binds: u32,
    /// The pages the code directory hashes.
    pub pages: u32,
    pub uuid: [u8; 16],
    /// `__text`'s file offset and size.
    pub text: (u64, u64),
    /// `__DATA_CONST,__mod_init_func`'s file offset and size when present.
    pub mod_init: Option<(u64, u64)>,
}

/// Checks a Mach-O arm64 executable.
pub fn macho(file: &[u8]) -> Result<MachoFacts, CheckError> {
    const PAGE: u64 = 0x4000;
    field(r32(file, 0, "mach_header_64.magic")? == 0xFEED_FACF, "mach_header_64.magic", 0)?;
    field(r32(file, 4, "cputype")? == 0x0100_000C, "mach_header_64.cputype", u64::from(get32(file, 4)))?;
    field(r32(file, 12, "filetype")? == 2, "mach_header_64.filetype", u64::from(get32(file, 12)))?;
    let ncmds = r32(file, 16, "ncmds")?;
    let sizeofcmds = u64::from(r32(file, 20, "sizeofcmds")?);
    within(file, 32, sizeofcmds, "the load commands")?;
    let mut segs = [MSeg::default(); MAX_SEGS];
    let mut nsegs = 0usize;
    let mut text_section = None;
    let mut mod_init = None;
    let mut entry = None;
    let mut signature = None;
    let mut fixups = None;
    let mut uuid = None;
    let mut symtab = None;
    let mut starts = None;
    let (mut dylinker, mut dylib) = (false, false);
    let mut at = 32u64;
    for _ in 0..ncmds {
        let cmd = r32(file, at, "load_command.cmd")?;
        let size = u64::from(r32(file, at + 4, "load_command.cmdsize")?);
        if size < 8 || size % 8 != 0 || at + size > 32 + sizeofcmds {
            return Err(CheckError::Bounds { what: "a load command", at });
        }
        match cmd {
            0x19 => {
                if nsegs == MAX_SEGS {
                    return Err(CheckError::Field { what: "the number of segments", value: nsegs as u64 + 1 });
                }
                let mut name = [0u8; 16];
                name.copy_from_slice(&file[at as usize + 8..at as usize + 24]);
                let seg = MSeg {
                    name,
                    vm: r64(file, at + 24, "vmaddr")?,
                    vm_size: r64(file, at + 32, "vmsize")?,
                    file: r64(file, at + 40, "fileoff")?,
                    file_size: r64(file, at + 48, "filesize")?,
                    prot: r32(file, at + 60, "initprot")?,
                };
                let nsects = u64::from(r32(file, at + 64, "nsects")?);
                field(size == 72 + 80 * nsects, "segment_command_64.cmdsize", size)?;
                if seg.vm != 0 || seg.vm_size != 0x1_0000_0000 {
                    if seg.vm % PAGE != 0 || seg.vm_size % PAGE != 0 {
                        return Err(CheckError::Align { what: "a segment's address or size", at });
                    }
                    if seg.file_size > 0 && seg.file % PAGE != 0 {
                        return Err(CheckError::Align { what: "a segment's file offset", at });
                    }
                    field(seg.file_size <= seg.vm_size, "a segment's file size past its size", seg.file_size)?;
                    within(file, seg.file, seg.file_size, "a segment's file bytes")?;
                }
                if nsegs > 0 && seg.vm < segs[nsegs - 1].vm + segs[nsegs - 1].vm_size {
                    return Err(CheckError::Field { what: "a segment's address (overlap or out of order)", value: seg.vm });
                }
                for k in 0..nsects {
                    let s = at + 72 + 80 * k;
                    let addr = r64(file, s + 32, "section addr")?;
                    let len = r64(file, s + 40, "section size")?;
                    let offset = u64::from(r32(file, s + 48, "section offset")?);
                    let flags = r32(file, s + 64, "section flags")?;
                    if addr < seg.vm || addr + len > seg.vm + seg.vm_size {
                        return Err(CheckError::Bounds { what: "a section in its segment", at: s });
                    }
                    if flags & 0xFF != 0x1 {
                        let placed = offset >= seg.file && offset - seg.file == addr - seg.vm;
                        field(placed, "a section's file offset against its address", offset)?;
                        within(file, offset, len, "a section's bytes")?;
                    }
                    if &file[s as usize..s as usize + 6] == b"__text" {
                        text_section = Some((offset, len));
                    }
                    if flags & 0xFF == 0x9 {
                        field(len % 8 == 0, "__mod_init_func's size", len)?;
                        if seg.name.starts_with(b"__DATA_CONST\0")
                            && let Some(s_idx) = usize::try_from(s).ok()
                            && file.get(s_idx..s_idx + 15) == Some(b"__mod_init_func".as_slice())
                        {
                            mod_init = Some((offset, len));
                        }
                    }
                }
                segs[nsegs] = seg;
                nsegs += 1;
            }
            0x8000_0028 => entry = Some(r64(file, at + 8, "entry_point_command.entryoff")?),
            0x1D => signature = Some((u64::from(r32(file, at + 8, "dataoff")?), u64::from(r32(file, at + 12, "datasize")?))),
            0x8000_0034 => fixups = Some((u64::from(r32(file, at + 8, "dataoff")?), u64::from(r32(file, at + 12, "datasize")?))),
            0x26 => starts = Some((u64::from(r32(file, at + 8, "dataoff")?), u64::from(r32(file, at + 12, "datasize")?))),
            0x1B => {
                let mut id = [0u8; 16];
                within(file, at + 8, 16, "uuid")?;
                id.copy_from_slice(&file[at as usize + 8..at as usize + 24]);
                uuid = Some(id);
            }
            0x2 => {
                let symoff = u64::from(r32(file, at + 8, "symoff")?);
                let nsyms = u64::from(r32(file, at + 12, "nsyms")?);
                let stroff = u64::from(r32(file, at + 16, "stroff")?);
                let strsize = u64::from(r32(file, at + 20, "strsize")?);
                symtab = Some((symoff, nsyms * 16, stroff, strsize));
            }
            0xE => dylinker = true,
            0xC => dylib = true,
            _ => {}
        }
        at += size;
    }
    let segs = &segs[..nsegs];
    let Some(text) = segs.iter().find(|s| s.name.starts_with(b"__TEXT\0")) else {
        return Err(CheckError::Missing { what: "__TEXT" });
    };
    let Some(link) = segs.iter().find(|s| s.name.starts_with(b"__LINKEDIT\0")) else {
        return Err(CheckError::Missing { what: "__LINKEDIT" });
    };
    field(text.file == 0, "__TEXT's file offset", text.file)?;
    field(text.prot == 5, "__TEXT's protection", u64::from(text.prot))?;
    let in_link = |(off, len): (u64, u64), what: &'static str| -> Result<(), CheckError> {
        match off >= link.file && off + len <= link.file + link.file_size {
            true => Ok(()),
            false => Err(CheckError::Bounds { what, at: off }),
        }
    };
    let Some(text_section) = text_section else { return Err(CheckError::Missing { what: "__text" }) };
    let Some(entry) = entry else { return Err(CheckError::Missing { what: "LC_MAIN" }) };
    field(entry >= text_section.0 && entry < text_section.0 + text_section.1, "LC_MAIN's entryoff outside __text", entry)?;
    let Some(uuid) = uuid else { return Err(CheckError::Missing { what: "LC_UUID" }) };
    field(uuid != [0; 16], "LC_UUID", 0)?;
    field(dylinker, "LC_LOAD_DYLINKER present", 0)?;
    field(dylib, "LC_LOAD_DYLIB present", 0)?;
    if let Some((symoff, symlen, stroff, strsize)) = symtab {
        in_link((symoff, symlen), "the symbol table")?;
        in_link((stroff, strsize), "the string table")?;
    }
    if let Some(starts) = starts {
        in_link(starts, "the function starts")?;
    }

    // The chained fixups: every chain walked, every slot inside a writable segment.
    let Some((fx, fx_len)) = fixups else { return Err(CheckError::Missing { what: "LC_DYLD_CHAINED_FIXUPS" }) };
    in_link((fx, fx_len), "the chained fixups")?;
    let blob = &file[fx as usize..(fx + fx_len) as usize];
    let starts_off = u64::from(r32(blob, 4, "starts_offset")?);
    let imports_off = u64::from(r32(blob, 8, "imports_offset")?);
    let symbols_off = u64::from(r32(blob, 12, "symbols_offset")?);
    let imports_count = r32(blob, 16, "imports_count")?;
    field(r32(blob, 20, "imports_format")? == 1, "imports_format", u64::from(get32(blob, 20)))?;
    for k in 0..u64::from(imports_count) {
        let import = r32(blob, imports_off + 4 * k, "dyld_chained_import")?;
        field(import & 0xFF == 1, "dyld_chained_import.lib_ordinal", u64::from(import & 0xFF))?;
        let name = symbols_off + u64::from(import >> 9);
        let Some(end) = blob.get(name as usize..).and_then(|rest| rest.iter().position(|&b| b == 0)) else {
            return Err(CheckError::Bounds { what: "an import's name", at: name });
        };
        field(end > 1, "an import's name length", end as u64)?;
    }
    let seg_count = r32(blob, starts_off, "seg_count")? as usize;
    field(seg_count == segs.len(), "dyld_chained_starts_in_image.seg_count", seg_count as u64)?;
    let image_end = link.vm + link.vm_size - text.vm;
    let (mut rebases, mut binds) = (0u32, 0u32);
    for (s, seg) in segs.iter().enumerate() {
        let info = u64::from(r32(blob, starts_off + 4 + 4 * s as u64, "seg_info_offset")?);
        if info == 0 {
            continue;
        }
        let at = starts_off + info;
        if at % 8 != 0 {
            return Err(CheckError::Align { what: "dyld_chained_starts_in_segment", at });
        }
        field(r16(blob, at + 4, "page_size")? == PAGE as u16, "page_size", u64::from(get16(blob, at as usize + 4)))?;
        field(r16(blob, at + 6, "pointer_format")? == 6, "pointer_format", u64::from(get16(blob, at as usize + 6)))?;
        field(r64(blob, at + 8, "segment_offset")? == seg.vm - text.vm, "segment_offset", get64(blob, at as usize + 8))?;
        field(seg.prot & 2 != 0, "the protection of a segment with fixups", u64::from(seg.prot))?;
        let pages = u64::from(r16(blob, at + 20, "page_count")?);
        field(pages * PAGE <= seg.vm_size, "page_count", pages)?;
        for p in 0..pages {
            let start = r16(blob, at + 22 + 2 * p, "page_start")?;
            if start == 0xFFFF {
                continue;
            }
            let mut slot = seg.vm + p * PAGE + u64::from(start);
            loop {
                let off = seg.file + (slot - seg.vm);
                if slot % 4 != 0 || off + 8 > seg.file + seg.file_size {
                    return Err(CheckError::Bounds { what: "a chained fixup's slot", at: off });
                }
                let value = r64(file, off, "a chained fixup")?;
                if value >> 63 == 1 {
                    field(((value & 0xFF_FFFF) as u32) < imports_count, "a bind's ordinal", value & 0xFF_FFFF)?;
                    binds += 1;
                } else {
                    let target = value & 0xF_FFFF_FFFF;
                    field(target < image_end, "a rebase's target", target)?;
                    rebases += 1;
                }
                let next = (value >> 51) & 0xFFF;
                if next == 0 {
                    break;
                }
                let following = slot + 4 * next;
                field((following - seg.vm) / PAGE == p, "a chain crossing its page", following)?;
                slot = following;
            }
        }
    }

    // The signature: last in the file, every page's hash the page's SHA-256.
    let Some((sig, sig_len)) = signature else { return Err(CheckError::Missing { what: "LC_CODE_SIGNATURE" }) };
    if sig % 16 != 0 {
        return Err(CheckError::Align { what: "the code signature", at: sig });
    }
    field(sig + sig_len == file.len() as u64, "the code signature's end", sig + sig_len)?;
    in_link((sig, sig_len), "the code signature")?;
    let blob = &file[sig as usize..];
    within(blob, 0, sign::BLOB_HEADER as u64, "CS_SuperBlob")?;
    field(get_be32(blob, 0) == sign::SUPER_BLOB, "CS_SuperBlob.magic", u64::from(get_be32(blob, 0)))?;
    field(u64::from(get_be32(blob, 4)) <= sig_len, "CS_SuperBlob.length", u64::from(get_be32(blob, 4)))?;
    field(get_be32(blob, 8) >= 1 && get_be32(blob, 12) == 0, "the code directory's slot", u64::from(get_be32(blob, 12)))?;
    let cd = u64::from(get_be32(blob, 16));
    within(blob, cd, sign::DIRECTORY_HEADER as u64, "CS_CodeDirectory")?;
    let d = &blob[cd as usize..];
    field(get_be32(d, 0) == sign::CODE_DIRECTORY, "CS_CodeDirectory.magic", u64::from(get_be32(d, 0)))?;
    field(get_be32(d, 8) == sign::VERSION, "CS_CodeDirectory.version", u64::from(get_be32(d, 8)))?;
    let hash_offset = u64::from(get_be32(d, 16));
    let slots = get_be32(d, 28);
    let limit = u64::from(get_be32(d, 32));
    field(limit == sig, "CS_CodeDirectory.codeLimit", limit)?;
    field(d[36] == sign::HASH as u8 && d[37] == 2 && d[39] == 12, "the hash size, type or page", u64::from(d[39]))?;
    field(u64::from(slots) == limit.div_ceil(sign::PAGE as u64), "CS_CodeDirectory.nCodeSlots", u64::from(slots))?;
    field(get_be64(d, 72) == text.file_size, "CS_CodeDirectory.execSegLimit", get_be64(d, 72))?;
    within(d, hash_offset, u64::from(slots) * sign::HASH as u64, "the code directory's hashes")?;
    for page in 0..slots as usize {
        let from = page * sign::PAGE;
        let to = (from + sign::PAGE).min(limit as usize);
        let want = blaze_sha256::digest(&file[from..to]);
        let at = hash_offset as usize + page * sign::HASH;
        if d[at..at + sign::HASH] != want {
            return Err(CheckError::PageHash { page: page as u32 });
        }
    }
    Ok(MachoFacts { entry, rebases, binds, pages: slots, uuid, text: text_section, mod_init })
}

/// What a checked ELF file says.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ElfFacts {
    pub entry: u64,
    pub relative: u32,
    pub binds: u32,
    pub tls_relocs: u32,
    pub needed: u32,
    /// `DT_INIT_ARRAY`'s `(vaddr, size)` when present.
    pub init_array: Option<(u64, u64)>,
    /// `PT_TLS`'s `(p_filesz, p_memsz, p_align)` when present.
    pub tls: Option<(u64, u64, u64)>,
}

#[derive(Clone, Copy, Default)]
struct Load {
    offset: u64,
    vaddr: u64,
    filesz: u64,
    memsz: u64,
    flags: u32,
}

const MAX_LOADS: usize = 8;

/// Checks an AArch64 ELF executable (including executables with merged native C images).
pub fn elf(file: &[u8]) -> Result<ElfFacts, CheckError> {
    within(file, 0, 64, "Elf64_Ehdr")?;
    field(file[..4] == [0x7F, b'E', b'L', b'F'] && file[4] == 2 && file[5] == 1, "e_ident", 0)?;
    field(get16(file, 16) == 3, "e_type", u64::from(get16(file, 16)))?;
    field(get16(file, 18) == 183, "e_machine", u64::from(get16(file, 18)))?;
    let entry = get64(file, 24);
    let phoff = get64(file, 32);
    field(get16(file, 54) == 56, "e_phentsize", u64::from(get16(file, 54)))?;
    let phnum = u64::from(get16(file, 56));
    within(file, phoff, 56 * phnum, "the program headers")?;
    let mut loads = [Load::default(); MAX_LOADS];
    let mut nloads = 0usize;
    let (mut interp, mut dynamic, mut relro, mut eh_frame_hdr, mut stack, mut tls) = (None, None, None, None, false, None);
    for k in 0..phnum {
        let at = phoff + 56 * k;
        let at_idx = usize::try_from(at).map_err(|_| CheckError::Bounds { what: "a program header", at })?;
        let kind = get32(file, at_idx);
        let load = Load {
            flags: get32(file, at_idx + 4),
            offset: get64(file, at_idx + 8),
            vaddr: get64(file, at_idx + 16),
            filesz: get64(file, at_idx + 32),
            memsz: get64(file, at_idx + 40),
        };
        let align = get64(file, at_idx + 48);
        match kind {
            1 => {
                if nloads == MAX_LOADS {
                    return Err(CheckError::Field { what: "the number of loads", value: u64::try_from(nloads + 1).unwrap_or(u64::MAX) });
                }
                if !align.is_power_of_two() || load.offset % align != load.vaddr % align {
                    return Err(CheckError::Align { what: "a load's offset against its address", at });
                }
                field(load.filesz <= load.memsz, "a load's file size past its size", load.filesz)?;
                within(file, load.offset, load.filesz, "a load's bytes")?;
                if nloads > 0 && load.vaddr < loads[nloads - 1].vaddr + loads[nloads - 1].memsz {
                    return Err(CheckError::Field { what: "a load's address (overlap or out of order)", value: load.vaddr });
                }
                loads[nloads] = load;
                nloads += 1;
            }
            2 => dynamic = Some(load),
            3 => interp = Some(load),
            7 => {
                field(tls.is_none(), "duplicate PT_TLS", 7)?;
                if !align.is_power_of_two() || load.offset % align != load.vaddr % align {
                    return Err(CheckError::Align { what: "PT_TLS's alignment", at });
                }
                field(load.filesz <= load.memsz, "PT_TLS's filesz past its memsz", load.filesz)?;
                field(load.memsz > 0, "PT_TLS's memsz", load.memsz)?;
                if load.filesz > 0 {
                    within(file, load.offset, load.filesz, "PT_TLS's bytes")?;
                }
                tls = Some((load, align));
            }
            0x6474_E550 => eh_frame_hdr = Some(load),
            0x6474_E552 => relro = Some(load),
            0x6474_E551 => stack = true,
            _ => {}
        }
    }
    let loads = &loads[..nloads];
    let file_of = |vaddr: u64, len: u64, writable: bool, what: &'static str| -> Result<u64, CheckError> {
        let end = vaddr.checked_add(len).ok_or(CheckError::Bounds { what, at: vaddr })?;
        for l in loads {
            if vaddr >= l.vaddr && end <= l.vaddr + l.filesz && (!writable || l.flags & 2 != 0) {
                return Ok(l.offset + (vaddr - l.vaddr));
            }
        }
        Err(CheckError::Bounds { what, at: vaddr })
    };
    let executable = loads.iter().any(|l| l.flags & 1 != 0 && entry >= l.vaddr && entry < l.vaddr + l.filesz);
    field(executable, "e_entry outside an executable load", entry)?;
    field(stack, "PT_GNU_STACK present", 0)?;
    let Some(interp) = interp else { return Err(CheckError::Missing { what: "PT_INTERP" }) };
    field(interp.filesz > 0, "PT_INTERP's size", 0)?;
    let path = file_of(interp.vaddr, interp.filesz, false, "PT_INTERP")?;
    let interp_term = usize::try_from(path + interp.filesz - 1).map_err(|_| CheckError::Bounds { what: "PT_INTERP", at: path })?;
    field(file.get(interp_term).copied() == Some(0), "PT_INTERP's terminator", 0)?;
    let Some(relro) = relro else { return Err(CheckError::Missing { what: "PT_GNU_RELRO" }) };
    file_of(relro.vaddr, relro.filesz, true, "PT_GNU_RELRO")?;
    if let Some(eh) = eh_frame_hdr
        && eh.filesz > 0
    {
        file_of(eh.vaddr, eh.filesz, false, "PT_GNU_EH_FRAME")?;
    }
    let tls_facts = match tls {
        None => None,
        Some((load, align)) => {
            if load.filesz > 0 {
                let mapped = file_of(load.vaddr, load.filesz, false, "PT_TLS")?;
                field(mapped == load.offset, "PT_TLS's file offset against its load", load.offset)?;
            }
            Some((load.filesz, load.memsz, align))
        }
    };
    let Some(dynamic) = dynamic else { return Err(CheckError::Missing { what: "PT_DYNAMIC" }) };
    let dyn_at = file_of(dynamic.vaddr, dynamic.filesz, true, "PT_DYNAMIC")?;
    let (mut rela, mut relasz, mut relacount, mut strtab, mut strsz, mut hash) = (None, None, None, None, None, None);
    let (mut init_array, mut init_arraysz) = (None, None);
    let mut needed = 0u32;
    let mut k = 0u64;
    loop {
        if 16 * (k + 1) > dynamic.filesz {
            return Err(CheckError::Missing { what: "DT_NULL" });
        }
        let off = usize::try_from(dyn_at + 16 * k).map_err(|_| CheckError::Bounds { what: "PT_DYNAMIC", at: dyn_at })?;
        let tag = get64(file, off);
        let value = get64(file, off + 8);
        match tag {
            0 => break,
            1 => needed += 1,
            4 => hash = Some(value),
            5 => strtab = Some(value),
            7 => rela = Some(value),
            8 => relasz = Some(value),
            10 => strsz = Some(value),
            25 => init_array = Some(value),
            27 => init_arraysz = Some(value),
            0x6FFF_FFF9 => relacount = Some(value),
            _ => {}
        }
        k += 1;
    }
    field(needed > 0, "DT_NEEDED count", u64::from(needed))?;
    let (Some(rela), Some(relasz), Some(strtab), Some(strsz), Some(hash)) = (rela, relasz, strtab, strsz, hash) else {
        return Err(CheckError::Missing { what: "a dynamic entry (DT_RELA, DT_RELASZ, DT_STRTAB, DT_STRSZ, DT_HASH, DT_NEEDED)" });
    };
    let strings = file_of(strtab, strsz, false, "DT_STRTAB")?;
    let strings_end = usize::try_from(strings + strsz - 1).map_err(|_| CheckError::Bounds { what: "DT_STRTAB", at: strings })?;
    field(file.get(strings_end).copied() == Some(0), "the string table's terminator", 0)?;
    // Verify every `DT_NEEDED` names a non-empty NUL-terminated string inside `DT_STRTAB`.
    for i in 0..k {
        let off = usize::try_from(dyn_at + 16 * i).map_err(|_| CheckError::Bounds { what: "PT_DYNAMIC", at: dyn_at })?;
        if get64(file, off) == 1 {
            let name_off = get64(file, off + 8);
            field(name_off < strsz, "DT_NEEDED's name", name_off)?;
            let start = usize::try_from(strings + name_off).map_err(|_| CheckError::Bounds { what: "DT_NEEDED", at: name_off })?;
            let end = usize::try_from(strings + strsz).map_err(|_| CheckError::Bounds { what: "DT_STRTAB", at: strings })?;
            let len = file.get(start..end).and_then(|slice| slice.iter().position(|&b| b == 0)).unwrap_or(0);
            field(len > 0, "DT_NEEDED's name length", u64::try_from(len).unwrap_or(0))?;
        }
    }
    let init_facts = match (init_array, init_arraysz) {
        (None, None) => None,
        (Some(vaddr), Some(size)) => {
            if !vaddr.is_multiple_of(8) {
                return Err(CheckError::Align { what: "DT_INIT_ARRAY", at: vaddr });
            }
            field(size > 0 && size.is_multiple_of(8), "DT_INIT_ARRAYSZ", size)?;
            file_of(vaddr, size, true, "DT_INIT_ARRAY")?;
            Some((vaddr, size))
        }
        _ => return Err(CheckError::Missing { what: "matching DT_INIT_ARRAY / DT_INIT_ARRAYSZ" }),
    };
    let hash_at = file_of(hash, 8, false, "DT_HASH")?;
    let hash_idx = usize::try_from(hash_at).map_err(|_| CheckError::Bounds { what: "DT_HASH", at: hash_at })?;
    let nsyms = u64::from(get32(file, hash_idx + 4));
    field(relasz % 24 == 0, "DT_RELASZ", relasz)?;
    let rela_at = file_of(rela, relasz, false, "DT_RELA")?;
    let image_end = loads.iter().map(|l| l.vaddr + l.memsz).max().unwrap_or(0);
    let (mut relative, mut binds, mut tls_relocs) = (0u32, 0u32, 0u32);
    let mut saw_non_relative = false;
    for r in 0..relasz / 24 {
        let at = usize::try_from(rela_at + 24 * r).map_err(|_| CheckError::Bounds { what: "DT_RELA", at: rela_at })?;
        let offset = get64(file, at);
        let info = get64(file, at + 8);
        let addend = get64(file, at + 16);
        match info & 0xFFFF_FFFF {
            1027 => {
                file_of(offset, 8, true, "a relocation's word")?;
                field(!saw_non_relative, "an R_AARCH64_RELATIVE after a non-relative relocation", r)?;
                let target = addend & ((1u64 << 56) - 1);
                field(info >> 32 == 0 && target < image_end, "an R_AARCH64_RELATIVE", addend)?;
                relative += 1;
            }
            1025 | 257 => {
                saw_non_relative = true;
                file_of(offset, 8, true, "a relocation's word")?;
                field(info >> 32 != 0 && info >> 32 < nsyms, "a bind's symbol", info >> 32)?;
                binds += 1;
            }
            1030 => {
                saw_non_relative = true;
                file_of(offset, 8, true, "a TLS_TPREL64 relocation's word")?;
                let Some((_, tls_memsz, _)) = tls_facts else {
                    return Err(CheckError::Missing { what: "PT_TLS for R_AARCH64_TLS_TPREL64" });
                };
                field(info >> 32 == 0 && addend < tls_memsz, "an R_AARCH64_TLS_TPREL64", addend)?;
                tls_relocs += 1;
            }
            1031 => {
                saw_non_relative = true;
                file_of(offset, 16, true, "a TLSDESC relocation's pair")?;
                let Some((_, tls_memsz, _)) = tls_facts else {
                    return Err(CheckError::Missing { what: "PT_TLS for R_AARCH64_TLSDESC" });
                };
                field(info >> 32 == 0 && addend < tls_memsz, "an R_AARCH64_TLSDESC", addend)?;
                tls_relocs += 1;
            }
            other => return Err(CheckError::Field { what: "a relocation's type", value: other }),
        }
    }
    if let Some(count) = relacount {
        field(count == u64::from(relative), "DT_RELACOUNT", count)?;
    }
    Ok(ElfFacts { entry, relative, binds, tls_relocs, needed, init_array: init_facts, tls: tls_facts })
}
