//! The native library record's descriptor, BNL2 (port27/tempo-run/NATIVE.md 3.4 as ruled on
//! 2026-11-16): the one home of its form. The layout writes it (layout.rs), the launcher reads it
//! through `dlsym` (blaze-launch-rt native_lib.rs), and the parent reads it from the record's
//! bytes on a store hit ([`read`]): a record is its own index, so no link plan is rebuilt to learn
//! the word names.
//!
//! At the start of the image's `Data` region (`__DATA,__data`), exported as `__blaze_native_v1`,
//! 48 bytes, little-endian, every address image-relative (a number, never a pointer, so the
//! descriptor needs no fixup):
//!
//! | offset | field | |
//! |---|---|---|
//! | 0 | `magic: u32` | `BNL2` = 0x424e_4c32 |
//! | 4 | `words: u32` | Rust words: 8-byte cells at `word_at`; the launcher writes word i |
//! | 8 | `inits: u32` | initializers: 8-byte rebased cells at `init_at`, link order |
//! | 12 | `pointers: u32` | pointer rows at `pointer_at` |
//! | 16 | `word_at: u64` | |
//! | 24 | `init_at: u64` | |
//! | 32 | `pointer_at: u64` | rows `{at: u64, word: u32, pad: u32 = 0}`, sorted by `at`: the C data cell at `at` holds word `word`'s function; the launcher writes it, so a function has one address, its slot's thunk |
//! | 40 | `name_at: u64` | the names table |
//!
//! The names table, at `name_at`, right after the descriptor: `words` rows `{offset: u32, len:
//! u32}` (offset from `name_at`), then the name bytes. Row i names word i: the table is in WORD
//! ORDER, which is `Plan::words`' order (sorted by name, resolve.rs), so it is also sorted. Word
//! order is chosen over a separately sorted table because the one thing a reader does with it is
//! pair name i with word i (`Down::Native.words`, pair i fills word i): a sorted table in another
//! order would need a permutation beside it, and Plan::words is already sorted, so word order
//! costs nothing a sorted table would give. The names are the image's own symbol names (Mach-O's
//! leading `_` included), as `Image::words` holds them.
//!
//! BNL1 (32 bytes, no pointers, no names) is never read beside BNL2 (rule 24): its tag is a typed
//! refusal ([`DescriptorError::Old`]), and the record is rebuilt (rule 26).

use crate::Platform;

mod so;

/// "BNL2".
pub const MAGIC: u32 = 0x424e_4c32;
/// "BNL1", the form before the pointer rows and the names: refused by name.
pub const MAGIC_BNL1: u32 = 0x424e_4c31;
/// The descriptor's size.
pub const LEN: u64 = 48;
/// A pointer row's size.
pub const POINTER_ROW_LEN: u64 = 16;
/// A names-table row's size.
pub const NAME_ROW_LEN: u64 = 8;

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Descriptor {
    pub magic: u32,
    pub words: u32,
    pub inits: u32,
    pub pointers: u32,
    pub word_at: u64,
    pub init_at: u64,
    pub pointer_at: u64,
    pub name_at: u64,
}

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PointerRow {
    pub at: u64,
    pub word: u32,
    /// Zero.
    pub pad: u32,
}

impl Descriptor {
    /// The descriptor's bytes, little-endian.
    pub fn to_bytes(&self) -> [u8; LEN as usize] {
        let mut out = [0u8; LEN as usize];
        out[0..4].copy_from_slice(&self.magic.to_le_bytes());
        out[4..8].copy_from_slice(&self.words.to_le_bytes());
        out[8..12].copy_from_slice(&self.inits.to_le_bytes());
        out[12..16].copy_from_slice(&self.pointers.to_le_bytes());
        out[16..24].copy_from_slice(&self.word_at.to_le_bytes());
        out[24..32].copy_from_slice(&self.init_at.to_le_bytes());
        out[32..40].copy_from_slice(&self.pointer_at.to_le_bytes());
        out[40..48].copy_from_slice(&self.name_at.to_le_bytes());
        out
    }

    /// Reads 48 little-endian bytes, any tag (the caller checks it: [`Descriptor::check`]).
    pub fn from_bytes(b: &[u8; LEN as usize]) -> Descriptor {
        let u32_at = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        let u64_at = |i: usize| u64::from(u32_at(i)) | (u64::from(u32_at(i + 4)) << 32);
        Descriptor {
            magic: u32_at(0),
            words: u32_at(4),
            inits: u32_at(8),
            pointers: u32_at(12),
            word_at: u64_at(16),
            init_at: u64_at(24),
            pointer_at: u64_at(32),
            name_at: u64_at(40),
        }
    }

    /// The tag check every reader makes first: BNL2, or the refusal naming the tag.
    pub fn check(&self) -> Result<(), DescriptorError> {
        match self.magic {
            MAGIC => Ok(()),
            MAGIC_BNL1 => Err(DescriptorError::Old { tag: self.magic }),
            got => Err(DescriptorError::Magic { got }),
        }
    }
}

/// Why a record's bytes give no BNL2 descriptor, naming the thing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DescriptorError {
    /// The bytes are not an arm64 Mach-O `MH_DYLIB` (W4's record form).
    NotDylib,
    /// The Linux record's ELF field `field` holds `got`, which the reader refuses (ELF64
    /// little-endian `ET_DYN` for `EM_AARCH64` or `EM_X86_64` only; descriptor/so.rs).
    ElfField { field: &'static str, got: u64 },
    /// The record has no `__DATA,__data` (Mach-O) or `.data` (ELF) section, where the descriptor
    /// starts.
    NoData,
    /// `what` at file offset `at` runs past the record's bytes.
    Truncated { what: &'static str, at: u64 },
    /// Image address `at` (of `what`) lies in no segment's file bytes.
    Unmapped { what: &'static str, at: u64 },
    /// The descriptor is BNL1: an old record, rebuilt (rule 26).
    Old { tag: u32 },
    /// The descriptor's tag is `got`: not a native library descriptor.
    Magic { got: u32 },
}

/// The names table, checked whole when read: [`Names::get`] answers for every `i < len`.
#[derive(Clone, Copy, Debug)]
pub struct Names<'r> {
    /// From `name_at` to the end of the record's bytes.
    table: &'r [u8],
    count: u32,
}

impl<'r> Names<'r> {
    pub fn len(&self) -> usize {
        self.count as usize
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Word `i`'s name.
    pub fn get(&self, i: usize) -> Option<&'r [u8]> {
        if i >= self.len() {
            return None;
        }
        let row = i * NAME_ROW_LEN as usize;
        let offset = u32::from_le_bytes(self.table.get(row..row + 4)?.try_into().ok()?) as usize;
        let len = u32::from_le_bytes(self.table.get(row + 4..row + 8)?.try_into().ok()?) as usize;
        self.table.get(offset..offset.checked_add(len)?)
    }
}

const MH_MAGIC_64: u32 = 0xfeed_facf;
const MH_DYLIB: u32 = 6;
const LC_SEGMENT_64: u32 = 0x19;
const HEADER_LEN: usize = 32;
const SEGMENT_LEN: usize = 72;
const SECTION_LEN: usize = 80;

fn u32_at(b: &[u8], at: usize, what: &'static str) -> Result<u32, DescriptorError> {
    let bytes = b.get(at..at + 4).ok_or(DescriptorError::Truncated { what, at: at as u64 })?;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn u64_at(b: &[u8], at: usize, what: &'static str) -> Result<u64, DescriptorError> {
    Ok(u64::from(u32_at(b, at, what)?) | (u64::from(u32_at(b, at + 4, what)?) << 32))
}

fn name16(b: &[u8], at: usize, want: &[u8]) -> bool {
    match b.get(at..at + 16) {
        Some(field) => {
            let end = field.iter().position(|&c| c == 0).unwrap_or(16);
            &field[..end] == want
        }
        None => false,
    }
}

/// The load commands of an arm64 Mach-O dylib, each `(offset of the command, cmd)`, walked in a
/// loop (no recursion); `visit` returns `Some` to stop.
fn each_command<T>(
    file: &[u8],
    mut visit: impl FnMut(usize, u32) -> Result<Option<T>, DescriptorError>,
) -> Result<Option<T>, DescriptorError> {
    if u32_at(file, 0, "the Mach-O header")? != MH_MAGIC_64 || u32_at(file, 12, "the Mach-O header")? != MH_DYLIB {
        return Err(DescriptorError::NotDylib);
    }
    let ncmds = u32_at(file, 16, "the Mach-O header")?;
    let mut at = HEADER_LEN;
    for _ in 0..ncmds {
        let cmd = u32_at(file, at, "a load command")?;
        let size = u32_at(file, at + 4, "a load command")? as usize;
        if size < 8 {
            return Err(DescriptorError::Truncated { what: "a load command shorter than its head", at: at as u64 });
        }
        if let Some(found) = visit(at, cmd)? {
            return Ok(Some(found));
        }
        at += size;
    }
    Ok(None)
}

/// The file offset of image address `at` (an address inside some segment's file bytes).
fn file_offset(file: &[u8], at: u64, what: &'static str) -> Result<usize, DescriptorError> {
    let found = each_command(file, |c, cmd| {
        if cmd != LC_SEGMENT_64 {
            return Ok(None);
        }
        let vmaddr = u64_at(file, c + 24, "a segment command")?;
        let fileoff = u64_at(file, c + 40, "a segment command")?;
        let filesize = u64_at(file, c + 48, "a segment command")?;
        Ok((at >= vmaddr && at - vmaddr < filesize).then_some(fileoff + (at - vmaddr)))
    })?;
    found.map(|off| off as usize).ok_or(DescriptorError::Unmapped { what, at })
}

/// The descriptor and the names table of a native library record's bytes (W4's dylib on macOS,
/// W5's shared object on Linux: descriptor/so.rs), read in place, checked: BNL2, and every name
/// inside the bytes.
pub fn read(file: &[u8], platform: Platform) -> Result<(Descriptor, Names<'_>), DescriptorError> {
    match platform {
        Platform::MacosArm64 => {}
        Platform::LinuxArm64 => return so::read(file),
    }
    // `__DATA,__data`'s address: the descriptor is its first 48 bytes (layout.rs).
    let data = each_command(file, |c, cmd| {
        if cmd != LC_SEGMENT_64 || !name16(file, c + 8, b"__DATA") {
            return Ok(None);
        }
        let nsects = u32_at(file, c + 64, "a segment command")? as usize;
        for k in 0..nsects {
            let s = c + SEGMENT_LEN + k * SECTION_LEN;
            if name16(file, s, b"__data") && name16(file, s + 16, b"__DATA") {
                return Ok(Some(u64_at(file, s + 32, "a section")?));
            }
        }
        Ok(None)
    })?
    .ok_or(DescriptorError::NoData)?;
    let off = file_offset(file, data, "the descriptor")?;
    descriptor_at(file, off, |at, what| file_offset(file, at, what))
}

/// The tail both formats share: the descriptor at file offset `off`, its tag checked, and its
/// names table found through `map` (image address to file offset, the format's segment map),
/// every name checked inside the bytes.
fn descriptor_at<'f>(
    file: &'f [u8],
    off: usize,
    map: impl Fn(u64, &'static str) -> Result<usize, DescriptorError>,
) -> Result<(Descriptor, Names<'f>), DescriptorError> {
    let bytes: &[u8; LEN as usize] = off
        .checked_add(LEN as usize)
        .and_then(|end| file.get(off..end))
        .and_then(|b| b.try_into().ok())
        .ok_or(DescriptorError::Truncated { what: "the descriptor", at: off as u64 })?;
    let d = Descriptor::from_bytes(bytes);
    d.check()?;
    if d.words == 0 {
        return Ok((d, Names { table: &[], count: 0 }));
    }
    let names_off = map(d.name_at, "the names table")?;
    let table = file.get(names_off..).ok_or(DescriptorError::Truncated { what: "the names table", at: names_off as u64 })?;
    let names = Names { table, count: d.words };
    for i in 0..names.len() {
        if names.get(i).is_none() {
            let at = (names_off + i * NAME_ROW_LEN as usize) as u64;
            return Err(DescriptorError::Truncated { what: "a word's name", at });
        }
    }
    Ok((d, names))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The descriptor's bytes read back as written; BNL1 is refused by its tag, another tag by
    /// its value. Prints "descriptor: BNL2 round-trips in 48 bytes; BNL1 refused as Old".
    #[test]
    fn descriptor_round_trips_and_bnl1_is_old() {
        let d = Descriptor {
            magic: MAGIC,
            words: 3,
            inits: 1,
            pointers: 2,
            word_at: 0xc100,
            init_at: 0xc118,
            pointer_at: 0xc120,
            name_at: 0xc030,
        };
        let bytes = d.to_bytes();
        assert_eq!(Descriptor::from_bytes(&bytes), d);
        assert_eq!(d.check(), Ok(()));
        assert_eq!(Descriptor { magic: MAGIC_BNL1, ..d }.check(), Err(DescriptorError::Old { tag: MAGIC_BNL1 }));
        assert_eq!(Descriptor { magic: 7, ..d }.check(), Err(DescriptorError::Magic { got: 7 }));
        assert_eq!(core::mem::size_of::<Descriptor>() as u64, LEN);
        assert_eq!(core::mem::size_of::<PointerRow>() as u64, POINTER_ROW_LEN);
        std::println!("descriptor: BNL2 round-trips in {} bytes; BNL1 refused as Old", bytes.len());
    }

    /// The names table answers each word's name and nothing past the count. Prints
    /// "descriptor: names [_a, _bc] in word order".
    #[test]
    fn names_in_word_order() {
        // Two rows, then "_a", "_bc".
        let mut table = [0u8; 16 + 5];
        table[0..4].copy_from_slice(&16u32.to_le_bytes());
        table[4..8].copy_from_slice(&2u32.to_le_bytes());
        table[8..12].copy_from_slice(&18u32.to_le_bytes());
        table[12..16].copy_from_slice(&3u32.to_le_bytes());
        table[16..21].copy_from_slice(b"_a_bc");
        let names = Names { table: &table, count: 2 };
        assert_eq!(names.get(0), Some(&b"_a"[..]));
        assert_eq!(names.get(1), Some(&b"_bc"[..]));
        assert_eq!(names.get(2), None);
        std::println!("descriptor: names [_a, _bc] in word order");
    }

    /// A minimal arm64 `MH_DYLIB`: one `__DATA` segment at address 0x4000, file offset 0x100,
    /// whose `__data` section starts with `d` and the names table `_a`, `_bc` after it.
    fn dylib(magic: u32) -> [u8; 0x200] {
        let mut f = [0u8; 0x200];
        let put32 = |f: &mut [u8; 0x200], at: usize, v: u32| f[at..at + 4].copy_from_slice(&v.to_le_bytes());
        let put64 = |f: &mut [u8; 0x200], at: usize, v: u64| f[at..at + 8].copy_from_slice(&v.to_le_bytes());
        put32(&mut f, 0, MH_MAGIC_64);
        put32(&mut f, 4, 0x0100_000c);
        put32(&mut f, 12, MH_DYLIB);
        put32(&mut f, 16, 1);
        put32(&mut f, 20, (SEGMENT_LEN + SECTION_LEN) as u32);
        let c = HEADER_LEN;
        put32(&mut f, c, LC_SEGMENT_64);
        put32(&mut f, c + 4, (SEGMENT_LEN + SECTION_LEN) as u32);
        f[c + 8..c + 14].copy_from_slice(b"__DATA");
        put64(&mut f, c + 24, 0x4000);
        put64(&mut f, c + 32, 0x1000);
        put64(&mut f, c + 40, 0x100);
        put64(&mut f, c + 48, 0x100);
        put32(&mut f, c + 64, 1);
        let s = c + SEGMENT_LEN;
        f[s..s + 6].copy_from_slice(b"__data");
        f[s + 16..s + 22].copy_from_slice(b"__DATA");
        put64(&mut f, s + 32, 0x4000);
        put64(&mut f, s + 40, 0x100);
        put32(&mut f, s + 48, 0x100);
        let d = Descriptor {
            magic,
            words: 2,
            inits: 0,
            pointers: 0,
            word_at: 0x4080,
            init_at: 0x4090,
            pointer_at: 0x4048,
            name_at: 0x4030,
        };
        f[0x100..0x130].copy_from_slice(&d.to_bytes());
        put32(&mut f, 0x130, 16);
        put32(&mut f, 0x134, 2);
        put32(&mut f, 0x138, 18);
        put32(&mut f, 0x13c, 3);
        f[0x140..0x145].copy_from_slice(b"_a_bc");
        f
    }

    /// The store-hit reader finds the descriptor at `__DATA,__data` through the load commands
    /// and the names through the segment's address-to-file map; BNL1 is refused by its tag; a
    /// dylib read as a Linux record is refused at its ELF magic, by name. Prints "descriptor:
    /// read 2 names [_a, _bc] from a dylib's bytes; BNL1 Old; a dylib read as ELF refused at
    /// e_ident".
    #[test]
    fn read_finds_names_in_a_dylib() {
        let file = dylib(MAGIC);
        let (d, names) = read(&file, Platform::MacosArm64).expect("a BNL2 dylib reads");
        assert_eq!((d.words, names.len()), (2, 2));
        assert_eq!((names.get(0), names.get(1)), (Some(&b"_a"[..]), Some(&b"_bc"[..])));
        let old = dylib(MAGIC_BNL1);
        assert_eq!(read(&old, Platform::MacosArm64).map(|(d, _)| d), Err(DescriptorError::Old { tag: MAGIC_BNL1 }));
        assert_eq!(
            read(&file, Platform::LinuxArm64).map(|(d, _)| d),
            Err(DescriptorError::ElfField { field: "e_ident[EI_MAG0..=EI_MAG3]", got: 0xfeed_facf })
        );
        assert_eq!(read(&file[..0x110], Platform::MacosArm64).map(|(d, _)| d), Err(DescriptorError::Truncated { what: "the descriptor", at: 0x100 }));
        std::println!("descriptor: read 2 names [_a, _bc] from a dylib's bytes; BNL1 Old; a dylib read as ELF refused at e_ident");
    }
}
