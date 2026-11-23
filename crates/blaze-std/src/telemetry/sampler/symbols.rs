//! Symbolization as a pure function of an address, the executable's bytes and its slide: the
//! function symbols of a Mach-O `MH_EXECUTE`/`MH_DYLIB` (`LC_SYMTAB`'s `nlist_64` entries in
//! `__TEXT,__text`) or an ELF `ET_EXEC`/`ET_DYN` (`.symtab`'s `STT_FUNC` entries), sorted by
//! address; an address is the function whose range holds it, after the slide is taken off.
//!
//! Not blaze-native's readers: `blaze_native::macho::read` and `elf::read` take only
//! relocatable objects (`MH_OBJECT`, `ET_REL`) and build the linker's model (sections,
//! relocations, symbols by section), none of which an executable's symbol table needs, and
//! bstd does not depend on blaze-native. This reader is the 150 lines the question needs:
//! every read bounds-checked, every refusal typed.
//!
//! Names are as the table holds them (mangled); see the module's UNSURE note.

use core::{alloc::Allocator, fmt};

use blaze_own::Vec;

/// One function: `[start, end)` before the slide, and its name's place in the bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Func {
    start: u64,
    end: u64,
    name_at: u32,
    name_len: u32,
}

/// What the reader refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SymbolsError {
    /// The bytes end before `what` at `at`.
    Short { what: &'static str, at: u64 },
    /// Neither a 64-bit little-endian Mach-O nor an ELF: the first four bytes.
    Magic { found: u32 },
    /// A file of a kind with no executable symbols (a relocatable object, a core file).
    Kind { found: u32 },
    /// No symbol table (`LC_SYMTAB`, `.symtab`): a stripped executable.
    NoTable,
    /// No text section to bound the last function.
    NoText,
    /// A name at `at` without its NUL, or not UTF-8.
    BadName { at: u64 },
    Reserve { bytes: usize },
}

impl fmt::Display for SymbolsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SymbolsError::Short { what, at } => write!(f, "the executable ends before its {what} at byte {at}"),
            SymbolsError::Magic { found } => write!(f, "the executable is neither a 64-bit Mach-O nor an ELF (magic {found:#x})"),
            SymbolsError::Kind { found } => write!(f, "the file is of kind {found}, not an executable or shared library"),
            SymbolsError::NoTable => f.write_str("the executable has no symbol table (it was stripped): the sampler names no function"),
            SymbolsError::NoText => f.write_str("the executable has no text section"),
            SymbolsError::BadName { at } => write!(f, "the symbol name at byte {at} has no NUL or is not UTF-8"),
            SymbolsError::Reserve { bytes } => write!(f, "could not reserve {bytes} bytes for the symbol table"),
        }
    }
}

/// A function an address is in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Found<'b> {
    /// Its index in the table: a key equal for every address in it, in address order.
    pub index: u32,
    pub name: &'b str,
    /// The address's offset from the function's start.
    pub offset: u64,
}

/// The function symbols of one image.
pub struct Symbols<'b, A: Allocator> {
    bytes: &'b [u8],
    funcs: Vec<Func, A>,
    slide: u64,
}

fn get<const N: usize>(bytes: &[u8], at: u64, what: &'static str) -> Result<[u8; N], SymbolsError> {
    let short = SymbolsError::Short { what, at };
    let start = usize::try_from(at).map_err(|_| short)?;
    let end = start.checked_add(N).ok_or(short)?;
    let got = bytes.get(start..end).ok_or(short)?;
    let mut out = [0u8; N];
    out.copy_from_slice(got);
    Ok(out)
}

fn u16_at(bytes: &[u8], at: u64, what: &'static str) -> Result<u16, SymbolsError> {
    get::<2>(bytes, at, what).map(u16::from_le_bytes)
}

fn u32_at(bytes: &[u8], at: u64, what: &'static str) -> Result<u32, SymbolsError> {
    get::<4>(bytes, at, what).map(u32::from_le_bytes)
}

fn u64_at(bytes: &[u8], at: u64, what: &'static str) -> Result<u64, SymbolsError> {
    get::<8>(bytes, at, what).map(u64::from_le_bytes)
}

/// The NUL-terminated UTF-8 name at `at` in `bytes`, at most to `end`: its length.
fn name_len(bytes: &[u8], at: u64, end: u64) -> Result<u32, SymbolsError> {
    let bad = SymbolsError::BadName { at };
    let start = usize::try_from(at).map_err(|_| bad)?;
    let end = usize::try_from(end).map_err(|_| bad)?.min(bytes.len());
    let rest = bytes.get(start..end).ok_or(bad)?;
    let len = rest.iter().position(|b| *b == 0).ok_or(bad)?;
    core::str::from_utf8(&rest[..len]).map_err(|_| bad)?;
    u32::try_from(len).map_err(|_| bad)
}

const MH_MAGIC_64: u32 = 0xfeed_facf;
const MH_EXECUTE: u32 = 2;
const MH_DYLIB: u32 = 6;
const LC_SYMTAB: u32 = 0x2;
const LC_SEGMENT_64: u32 = 0x19;
const N_STAB: u8 = 0xe0;
const N_TYPE: u8 = 0x0e;
const N_SECT: u8 = 0x0e;
const ELF_MAGIC: u32 = u32::from_le_bytes(*b"\x7fELF");
const ET_EXEC: u16 = 2;
const ET_DYN: u16 = 3;
const SHT_SYMTAB: u32 = 2;
const STT_FUNC: u8 = 2;

impl<'b, A: Allocator> Symbols<'b, A> {
    /// The table of `bytes` (the executable's file), at `slide` (what the loader added to its
    /// addresses: `_dyld_get_image_vmaddr_slide(0)` on macOS, the load bias on Linux).
    pub fn read(bytes: &'b [u8], slide: u64, alloc: A) -> Result<Symbols<'b, A>, SymbolsError> {
        let magic = u32_at(bytes, 0, "magic")?;
        let mut funcs = Vec::new_in(alloc);
        if magic == MH_MAGIC_64 {
            macho(bytes, &mut funcs)?;
        } else if magic == ELF_MAGIC {
            elf(bytes, &mut funcs)?;
        } else {
            return Err(SymbolsError::Magic { found: magic });
        }
        // By start, then by name's place: one function per start, the first in the table.
        funcs.sort_unstable_by(|a: &Func, b: &Func| a.start.cmp(&b.start).then(a.name_at.cmp(&b.name_at)));
        let mut kept = 0usize;
        for at in 0..funcs.len() {
            if kept > 0 && funcs[kept - 1].start == funcs[at].start {
                continue;
            }
            funcs[kept] = funcs[at];
            kept += 1;
        }
        funcs.truncate(kept);
        // A function with no size ends where the next begins.
        for at in 0..funcs.len() {
            let next = funcs.get(at + 1).map(|f| f.start);
            let f = &mut funcs[at];
            if f.end <= f.start {
                f.end = next.unwrap_or(f.start);
            }
            if let Some(next) = next {
                f.end = f.end.min(next);
            }
        }
        Ok(Symbols { bytes, funcs, slide })
    }

    /// The function `address` (as the running process sees it) is in.
    pub fn find(&self, address: u64) -> Option<Found<'b>> {
        let at = address.wrapping_sub(self.slide);
        let after = self.funcs.partition_point(|f| f.start <= at);
        let index = after.checked_sub(1)?;
        let f = self.funcs[index];
        if at >= f.end {
            return None;
        }
        Some(Found { index: index as u32, name: self.name(index as u32)?, offset: at - f.start })
    }

    /// The name of function `index`.
    pub fn name(&self, index: u32) -> Option<&'b str> {
        let f = self.funcs.get(index as usize)?;
        let start = f.name_at as usize;
        let bytes: &'b [u8] = self.bytes;
        core::str::from_utf8(bytes.get(start..start + f.name_len as usize)?).ok()
    }

    pub fn len(&self) -> usize {
        self.funcs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.funcs.is_empty()
    }
}

fn push<A: Allocator>(funcs: &mut Vec<Func, A>, f: Func) -> Result<(), SymbolsError> {
    funcs.try_reserve(1).map_err(|_| SymbolsError::Reserve { bytes: size_of::<Func>() })?;
    funcs.push(f);
    Ok(())
}

fn macho<A: Allocator>(bytes: &[u8], funcs: &mut Vec<Func, A>) -> Result<(), SymbolsError> {
    let kind = u32_at(bytes, 12, "file type")?;
    if kind != MH_EXECUTE && kind != MH_DYLIB {
        return Err(SymbolsError::Kind { found: kind });
    }
    let commands = u32_at(bytes, 16, "command count")?;
    let mut at = 32u64;
    let mut ordinal = 0u32;
    let mut text: Option<(u32, u64, u64)> = None;
    let mut table: Option<(u64, u64, u64, u64)> = None;
    for _ in 0..commands {
        let cmd = u32_at(bytes, at, "load command")?;
        let size = u64::from(u32_at(bytes, at + 4, "load command size")?);
        if size < 8 {
            return Err(SymbolsError::Short { what: "load command of size under 8", at });
        }
        if cmd == LC_SEGMENT_64 {
            let sections = u32_at(bytes, at + 64, "segment's section count")?;
            for s in 0..u64::from(sections) {
                ordinal += 1;
                let header = at + 72 + s * 80;
                let sect = get::<16>(bytes, header, "section name")?;
                let seg = get::<16>(bytes, header + 16, "section's segment name")?;
                if sect.starts_with(b"__text\0") && seg.starts_with(b"__TEXT\0") {
                    let addr = u64_at(bytes, header + 32, "section address")?;
                    let len = u64_at(bytes, header + 40, "section size")?;
                    text = Some((ordinal, addr, addr.saturating_add(len)));
                }
            }
        } else if cmd == LC_SYMTAB {
            table = Some((
                u64::from(u32_at(bytes, at + 8, "symbol offset")?),
                u64::from(u32_at(bytes, at + 12, "symbol count")?),
                u64::from(u32_at(bytes, at + 16, "string offset")?),
                u64::from(u32_at(bytes, at + 20, "string size")?),
            ));
        }
        at += size;
    }
    let (symoff, nsyms, stroff, strsize) = table.ok_or(SymbolsError::NoTable)?;
    let (text_ordinal, _, text_end) = text.ok_or(SymbolsError::NoText)?;
    let str_end = stroff.saturating_add(strsize);
    for n in 0..nsyms {
        let entry = symoff + n * 16;
        let strx = u64::from(u32_at(bytes, entry, "nlist name")?);
        let [kind] = get::<1>(bytes, entry + 4, "nlist type")?;
        let [sect] = get::<1>(bytes, entry + 5, "nlist section")?;
        let value = u64_at(bytes, entry + 8, "nlist value")?;
        if kind & N_STAB != 0 || kind & N_TYPE != N_SECT || u32::from(sect) != text_ordinal {
            continue;
        }
        let name_at = stroff + strx;
        let len = name_len(bytes, name_at, str_end)?;
        // Mach-O has no symbol sizes: the end is the next start, the last one the text's end.
        let name_at = u32::try_from(name_at).map_err(|_| SymbolsError::BadName { at: name_at })?;
        push(funcs, Func { start: value, end: text_end, name_at, name_len: len })?;
    }
    Ok(())
}

fn elf<A: Allocator>(bytes: &[u8], funcs: &mut Vec<Func, A>) -> Result<(), SymbolsError> {
    let [class] = get::<1>(bytes, 4, "ELF class")?;
    let [order] = get::<1>(bytes, 5, "ELF byte order")?;
    if class != 2 || order != 1 {
        return Err(SymbolsError::Magic { found: ELF_MAGIC });
    }
    let kind = u16_at(bytes, 16, "ELF type")?;
    if kind != ET_EXEC && kind != ET_DYN {
        return Err(SymbolsError::Kind { found: u32::from(kind) });
    }
    let shoff = u64_at(bytes, 40, "section header offset")?;
    let shentsize = u64::from(u16_at(bytes, 58, "section header size")?);
    let shnum = u64::from(u16_at(bytes, 60, "section count")?);
    if shentsize != 64 {
        return Err(SymbolsError::Short { what: "64-byte section header", at: 58 });
    }
    let mut table = None;
    for s in 0..shnum {
        let header = shoff + s * 64;
        if u32_at(bytes, header + 4, "section type")? == SHT_SYMTAB {
            table = Some(header);
            break;
        }
    }
    let header = table.ok_or(SymbolsError::NoTable)?;
    let symoff = u64_at(bytes, header + 24, "symbol table offset")?;
    let symsize = u64_at(bytes, header + 32, "symbol table size")?;
    let link = u64::from(u32_at(bytes, header + 40, "symbol table's string table")?);
    let strings = shoff + link * 64;
    let stroff = u64_at(bytes, strings + 24, "string table offset")?;
    let str_end = stroff.saturating_add(u64_at(bytes, strings + 32, "string table size")?);
    for n in 0..symsize / 24 {
        let entry = symoff + n * 24;
        let strx = u64::from(u32_at(bytes, entry, "symbol name")?);
        let [info] = get::<1>(bytes, entry + 4, "symbol info")?;
        let shndx = u16_at(bytes, entry + 6, "symbol section")?;
        let value = u64_at(bytes, entry + 8, "symbol value")?;
        let size = u64_at(bytes, entry + 16, "symbol size")?;
        if info & 0xf != STT_FUNC || shndx == 0 || value == 0 {
            continue;
        }
        let name_at = stroff + strx;
        let len = name_len(bytes, name_at, str_end)?;
        let name_at = u32::try_from(name_at).map_err(|_| SymbolsError::BadName { at: name_at })?;
        push(funcs, Func { start: value, end: value.saturating_add(size), name_at, name_len: len })?;
    }
    Ok(())
}
