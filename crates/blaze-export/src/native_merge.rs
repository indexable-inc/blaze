//! Export merges the native image (NATIVE.md 3.9, piece W9; main's rulings of 2026-11-16): the
//! program's native library image (blaze-native `layout::lay`, the same image the run path loads
//! as a dylib or ELF shared object) placed inside the exported Mach-O or ELF executable, so the
//! file runs with no blaze and no native library record beside it.
//!
//! # Where the image goes
//!
//! The image is moved as ONE block by one `shift` (image address + `shift` = the executable's
//! image offset), a multiple of the platform's page: its first byte after the header room
//! (`Image::file_start`) lands on the program's end (`Laid::end`). Every pc-relative reference
//! inside the image (a branch, an `adrp` to its GOT, a word stub) keeps its distance, so
//! `layout::lay`'s resolution stands and only the 64-bit pointers (the fixups) move. Two
//! segments/loads hold it: code and constants (`__NATIVE_TEXT` on Mach-O, an `r-x` `PT_LOAD` on
//! ELF; the image's own unwind bytes ride along unread) and writable data (`__NATIVE_DATA` on
//! Mach-O, an `rw-` `PT_LOAD` on ELF: GOT, data, words, initializers, thread-local descriptors
//! and templates, zero fill). In the file the block starts on the page after the program's file
//! bytes (the program's `__bss`/zero-fill takes no file bytes); its file offsets are its image
//! offsets less one constant ([`Merged::file_of`] / [`MergedElf::file_of`]).
//!
//! # What each part of the image becomes
//!
//! - A word (C calling Rust, NATIVE.md 3.4) becomes a direct address: word `i`'s cell and every
//!   `FixTo::Word { index: i }` cell (the BNL2 pointer rows) is a rebase (`R_AARCH64_RELATIVE`
//!   on ELF) to the Rust function the word names, found through `ProgramIn::exports` (sorted by
//!   name; the front end gives it from `#[no_mangle]` and `#[export_name]` items) and
//!   `Laid::placed`. No launcher writes anything.
//! - The initializers (`Inits`) become `__mod_init_func` on Mach-O and `DT_INIT_ARRAY` /
//!   `DT_INIT_ARRAYSZ` on ELF: the platform's loader runs them before `main`, after the
//!   libraries' own (the order of a normal executable, NATIVE.md 3.4).
//! - Thread-locals: on Mach-O, `__thread_vars` (descriptors whose thunk binds `__tlv_bootstrap`),
//!   `__thread_data`, `__thread_bss`, and `MH_HAS_TLV_DESCRIPTORS` coexist with Rust's indexed
//!   descriptor; on ELF, C's `.tdata`/`.tbss` and Rust's 8-byte `ROOT` share the executable's
//!   single `PT_TLS` (C at `[0, c_memsz)`, Rust `ROOT` after it in the zero-filled tail at
//!   `up(c_memsz, 8)`, so every `R_AARCH64_TLSDESC`/`R_AARCH64_TLS_TPREL64` offset is unchanged).
//! - Imports: on Mach-O, flat lookup (`BIND_SPECIAL_DYLIB_FLAT_LOOKUP`, NATIVE.md 3.5) among
//!   libSystem and `LC_LOAD_DYLIB` per distinct install name of `Image::libraries`; on ELF,
//!   `.dynsym` + `R_AARCH64_GLOB_DAT`/`R_AARCH64_ABS64` against `libc.so.6` and `DT_NEEDED` per
//!   distinct soname of `Image::libraries`.
//! - A program import the image defines (a Rust call to a C function of the image) binds to it
//!   directly: its GOT cell becomes a rebase (`R_AARCH64_RELATIVE` on ELF) and its row leaves the
//!   import table (`direct`), as the run path's `dlsym(handle, name)` finds the image's definition
//!   first.
//! - The unwind rows and frame entries (`Image::unwind`, `frames`, `eh_pointers`) go to
//!   eh_out.rs, which builds the file's ONE `__eh_frame` and ONE `__unwind_info` / `.eh_frame_hdr`
//!   from the Rust rows and these ([`crate::eh_out::Outside`]).

use core::fmt;

use blaze_code::record::Symbol;
use blaze_native::Platform as NativePlatform;
use blaze_native::image::{Addr56, FixTo as NativeFix, Image, Region, TlsReloc};
use blaze_own::{AString, AVec, Alloc, String, Vec};

use crate::elf;
use crate::input::CExport;
use crate::layout::Laid;
use crate::macho::{self, Chain, ChainImport};
use crate::native_dylib::FLAT_LOOKUP;

/// The page both blocks of the image move by a multiple of (macOS on arm64).
const PAGE: u64 = macho::PAGE;
/// `S_ATTR_PURE_INSTRUCTIONS | S_ATTR_SOME_INSTRUCTIONS`.
const S_CODE: u32 = 0x8000_0000 | 0x400;
const S_REGULAR: u32 = 0x0;
const S_ZEROFILL: u32 = 0x1;
const S_MOD_INIT_FUNC_POINTERS: u32 = 0x9;
const S_THREAD_LOCAL_REGULAR: u32 = 0x11;
const S_THREAD_LOCAL_ZEROFILL: u32 = 0x12;
const S_THREAD_LOCAL_VARIABLES: u32 = 0x13;
/// `MH_HAS_TLV_DESCRIPTORS`.
const MH_HAS_TLV_DESCRIPTORS: u32 = 0x80_0000;
/// The segments' names (16 bytes at most, `segment_command_64::segname`).
pub(crate) const NATIVE_TEXT: &str = "__NATIVE_TEXT";
pub(crate) const NATIVE_DATA: &str = "__NATIVE_DATA";
/// The sections the two segments may hold, in order, as (region, name, flags, zero fill); an
/// empty region has none. The executable's header room counts every one (macho.rs
/// `command_room`, from [`CODE_SECTIONS`] and [`DATA_SECTIONS`]), never a hand count.
const CODE_PARTS: [(Region, &str, u32, bool); 2] = [(Region::Text, "__text", S_CODE, false), (Region::Const, "__const", S_REGULAR, false)];
const DATA_PARTS: [(Region, &str, u32, bool); 8] = [
    (Region::Got, "__got", S_REGULAR, false),
    (Region::Data, "__data", S_REGULAR, false),
    (Region::Words, "__blaze_words", S_REGULAR, false),
    (Region::Inits, "__mod_init_func", S_MOD_INIT_FUNC_POINTERS, false),
    (Region::ThreadVars, "__thread_vars", S_THREAD_LOCAL_VARIABLES, false),
    (Region::ThreadData, "__thread_data", S_THREAD_LOCAL_REGULAR, false),
    (Region::ThreadZero, "__thread_bss", S_THREAD_LOCAL_ZEROFILL, true),
    (Region::Zero, "__bss", S_ZEROFILL, true),
];
/// `__NATIVE_TEXT`'s sections at most.
pub(crate) const CODE_SECTIONS: usize = CODE_PARTS.len();
/// `__NATIVE_DATA`'s sections at most.
pub(crate) const DATA_SECTIONS: usize = DATA_PARTS.len();

/// Why a native image could not be merged into an executable. Indices name the image's own
/// tables (`Image::words`, `Image::libraries`), which the caller holds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MergeError {
    /// Word `word` (`Image::words[word]`, a C name C code calls) names no function of
    /// `ProgramIn::exports`, or one the program does not place (NATIVE.md 3.4).
    RustWordMissing { word: u32 },
    /// Two items export one C name (Reference, abi, `no_mangle`: "the symbol ... is the
    /// identifier"; two of them collide at link time).
    TwoExports { first: Symbol, second: Symbol },
    /// `ProgramIn::exports` is not sorted by name at row `at` (the front end sorts it).
    ExportsUnsorted { at: u32 },
    /// Library `library` (`Image::libraries[library]`) was resolved for another platform than
    /// the executable's (NATIVE.md 3.5).
    Library { library: u32 },
    /// An image laid for another platform than the executable's.
    Platform,
    /// The image lacks region `region` (`layout::lay` lists every region, so this is a broken image).
    NoRegion { region: Region },
    /// The image's GOT does not start a page, so its two blocks could not move together.
    Unaligned { at: u64 },
    /// The image breaks a structural invariant (`what`) at image address `at`.
    Shape { what: &'static str, at: u64 },
    /// A number past its field.
    TooLarge { what: &'static str, len: u64 },
}

impl fmt::Display for MergeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            MergeError::RustWordMissing { word } => write!(
                f,
                "the native image's word {word} names a Rust function no `#[no_mangle]` or `#[export_name]` item of the program exports (NATIVE.md 3.4)"
            ),
            MergeError::TwoExports { first, second } => write!(f, "functions {first} and {second} export one C name (Reference, abi: no_mangle)"),
            MergeError::ExportsUnsorted { at } => write!(f, "the program's C exports are not sorted by name at row {at}"),
            MergeError::Library { library } => write!(f, "the native image's library {library} was resolved for another platform than the executable's (NATIVE.md 3.5)"),
            MergeError::Platform => write!(f, "the native image was laid for another platform than the executable's"),
            MergeError::NoRegion { region } => write!(f, "the native image has no {region:?} region"),
            MergeError::Unaligned { at } => write!(f, "the native image's GOT at {at:#x} does not start a page"),
            MergeError::Shape { what, at } => write!(f, "the native image breaks {what} at {at:#x}"),
            MergeError::TooLarge { what, len } => write!(f, "{what} ({len:#x}) is past its field"),
        }
    }
}

/// One section of a native segment, at executable image offsets.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Sect {
    pub name: &'static str,
    pub at: u64,
    pub len: u64,
    /// No file bytes (`S_ZEROFILL`, `S_THREAD_LOCAL_ZEROFILL`).
    pub zero: bool,
    pub align_log2: u32,
    pub flags: u32,
}

/// The image merged for Mach-O: where its blocks are, what the loader fixes in them, the import
/// rows and libraries they add, their sections. The executable writer (macho.rs `write_native`)
/// copies `bytes` and writes the rest.
pub struct Merged<'a, 'i> {
    /// Image address + `shift` = executable image offset.
    pub shift: u64,
    /// The image's file bytes (from `Image::file_start`).
    pub(crate) bytes: &'i [u8],
    /// `__NATIVE_TEXT`'s start and `__NATIVE_DATA`'s start (both on a page), the end of the
    /// file bytes and the end of the block (on a page), as executable image offsets.
    pub text_at: u64,
    pub data_at: u64,
    pub file_end: u64,
    pub end: u64,
    /// Sorted by address; binds count from the program's import rows that stay (`import` +
    /// `Laid::imports.len()` - `direct.len()`).
    pub(crate) fixups: AVec<'a, (u64, Chain)>,
    /// The program's import rows the image defines, each with the executable address it binds
    /// to, sorted by row. macho.rs `chained` makes their slots rebases, drops the rows and
    /// numbers the rest in order (dyld resolves every row of the table at launch).
    pub(crate) direct: AVec<'a, (u32, u64)>,
    /// The image's import rows (flat lookup), after the program's.
    pub(crate) imports: AVec<'a, ChainImport<'i>>,
    /// Install names, distinct, in the image's order, libSystem left out (always first).
    pub libraries: AVec<'a, AString<'a>>,
    pub(crate) code: AVec<'a, Sect>,
    pub(crate) data: AVec<'a, Sect>,
    pub(crate) thread_vars: bool,
}

impl Merged<'_, '_> {
    /// The image's import rows the executable binds.
    pub fn imports(&self) -> usize {
        self.imports.len()
    }

    /// The executable's import rows when the program has `program` of its own: those the image
    /// does not define, then the image's.
    pub fn bound(&self, program: usize) -> usize {
        program - self.direct.len() + self.imports.len()
    }

    /// `mach_header_64::flags` the image adds.
    pub(crate) fn header_flags(&self) -> u32 {
        if self.thread_vars { MH_HAS_TLV_DESCRIPTORS } else { 0 }
    }

    /// The file offset of executable image offset `at`, the block's file start being `file`.
    pub(crate) fn file_of(&self, at: u64, file: u64) -> u64 {
        if at >= self.text_at { at - self.text_at + file } else { at }
    }
}

/// One non-relative ELF relocation emitted for the merged native image (at executable offsets).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct ElfRela {
    pub at: u64,
    pub kind: ElfRelaKind,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ElfRelaKind {
    /// `R_AARCH64_GLOB_DAT` (when `got && addend == 0`) or `R_AARCH64_ABS64`: `import` indexes
    /// [`MergedElf::imports`].
    Import { import: u32, addend: i64, got: bool },
    /// `R_AARCH64_TLSDESC` on a 16-byte GOT pair: symbol 0, addend = template offset.
    TlsDesc { offset: u64 },
    /// `R_AARCH64_TLS_TPREL64` on an 8-byte GOT cell: symbol 0, addend = template offset.
    TlsTpRel { offset: u64 },
}

/// Native C TLS template metadata on ELF (when `memsz > 0`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct ElfTls {
    /// Start of `.tdata` (or `.tbss` when `.tdata` is empty) at executable image offset.
    pub vaddr: u64,
    /// `.tdata` length (bytes in file).
    pub filesz: u64,
    /// `.tdata` + `.tbss` length (bytes in memory).
    pub memsz: u64,
    /// Required power-of-two alignment (`1 << max(tdata.align_log2, tbss.align_log2)`).
    pub align: u64,
}

/// Native `DT_INIT_ARRAY` metadata on ELF (when `Region::Inits` is non-empty).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct ElfInits {
    pub vaddr: u64,
    pub len: u64,
}

/// One external symbol imported by the native ELF image.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct ElfImport<'i> {
    pub name: &'i [u8],
    pub weak: bool,
}

/// The image merged for ELF arm64: where its two loads sit, its relative and dynamic
/// relocations, its imports, `DT_NEEDED` sonames, optional `DT_INIT_ARRAY`, and optional C TLS
/// template. The executable writer (`elf::write_native`) copies `bytes` and writes the rest.
pub struct MergedElf<'a, 'i> {
    /// Image address + `shift` = executable image offset.
    pub shift: u64,
    /// The image's file bytes (from `Image::file_start`).
    pub(crate) bytes: &'i [u8],
    /// Native text (`r-x`) and data (`rw-`) load starts, end of file bytes, and end in memory.
    pub text_at: u64,
    pub data_at: u64,
    pub file_end: u64,
    pub end: u64,
    /// Program import rows defined by the native image: `(program_row, target_vaddr)`, sorted.
    pub(crate) direct: AVec<'a, (u32, u64)>,
    /// External imports of the native image, in `Image::imports` order.
    pub(crate) imports: AVec<'a, ElfImport<'i>>,
    /// `R_AARCH64_RELATIVE` entries `(r_offset, r_addend)`, sorted by `r_offset`.
    pub(crate) relative: AVec<'a, (u64, u64)>,
    /// Non-relative relocations (`GLOB_DAT`, `ABS64`, `TLSDESC`, `TLS_TPREL64`), sorted by `at`.
    pub(crate) other: AVec<'a, ElfRela>,
    /// Distinct `DT_NEEDED` sonames in image order, `libc.so.6` excluded.
    pub libraries: AVec<'a, AString<'a>>,
    /// `DT_INIT_ARRAY` / `DT_INIT_ARRAYSZ` when `Region::Inits` is non-empty.
    pub(crate) inits: Option<ElfInits>,
    /// Native C TLS template when `.tdata` / `.tbss` is non-empty.
    pub(crate) tls: Option<ElfTls>,
    /// True when any `TlsReloc::TpRel` is present (`DF_STATIC_TLS`).
    pub(crate) static_tls: bool,
}

impl MergedElf<'_, '_> {
    /// The image's external import symbols.
    pub fn imports(&self) -> usize {
        self.imports.len()
    }

    /// The executable's bound external symbols when the program has `program` of its own.
    pub fn bound(&self, program: usize) -> usize {
        program - self.direct.len() + self.imports.len()
    }

    /// The file offset of executable image offset `at`, the block's file start being `file`.
    pub(crate) fn file_of(&self, at: u64, file: u64) -> u64 {
        if at >= self.text_at { at - self.text_at + file } else { at }
    }
}

fn up(n: u64, to: u64) -> u64 {
    n.div_ceil(to) * to
}

fn u64_of_usize(n: usize, what: &'static str) -> Result<u64, MergeError> {
    u64::try_from(n).map_err(|_| MergeError::TooLarge { what, len: u64::MAX })
}

fn u32_of_usize(n: usize, what: &'static str) -> Result<u32, MergeError> {
    u32::try_from(n).map_err(|_| MergeError::TooLarge { what, len: u64::try_from(n).unwrap_or(u64::MAX) })
}

/// The install names (macOS) or sonames (Linux) of `image`'s libraries: distinct, in the image's
/// order, with the platform's base C library (`libSystem.B.dylib` or `libc.so.6`, always first)
/// left out. Computed once, before layout, since header / dynamic room depends on them.
pub fn libraries<'a>(image: &Image<'_>, alloc: Alloc<'a>) -> Result<AVec<'a, AString<'a>>, MergeError> {
    let base = match image.platform {
        NativePlatform::MacosArm64 => macho::LIB_SYSTEM,
        NativePlatform::LinuxArm64 => elf::LIBC,
    };
    let mut paths: AVec<'a, AString<'a>> = Vec::new_in(alloc);
    for (k, library) in image.libraries.iter().enumerate() {
        if library.platform() != image.platform {
            return Err(MergeError::Library { library: u32_of_usize(k, "a library index")? });
        }
        let install = library.install();
        if install == base || paths.iter().any(|p| p.as_str() == install) {
            continue;
        }
        let mut path: AString<'a> = String::new_in(alloc);
        path.push_str(install);
        paths.push(path);
    }
    Ok(paths)
}

fn check_exports(exports: &[CExport<'_>]) -> Result<(), MergeError> {
    for (k, pair) in exports.windows(2).enumerate() {
        match pair[0].name.cmp(pair[1].name) {
            core::cmp::Ordering::Less => {}
            core::cmp::Ordering::Equal => return Err(MergeError::TwoExports { first: pair[0].symbol, second: pair[1].symbol }),
            core::cmp::Ordering::Greater => return Err(MergeError::ExportsUnsorted { at: u32_of_usize(k + 1, "an export row")? }),
        }
    }
    Ok(())
}

struct Blocks {
    shift: u64,
    text_at: u64,
    data_at: u64,
    file_end: u64,
    end: u64,
    got_at: u64,
    got_len: u64,
    init_end: u64,
}

fn place_blocks(laid: &Laid<'_, '_>, image: &Image<'_>, page: u64) -> Result<Blocks, MergeError> {
    let got = image.region(Region::Got).ok_or(MergeError::NoRegion { region: Region::Got })?;
    if got.at % page != 0 || image.file_start % page != 0 {
        return Err(MergeError::Unaligned { at: got.at });
    }
    if laid.end % page != 0 {
        return Err(MergeError::Unaligned { at: laid.end });
    }
    let shift = laid.end.checked_sub(image.file_start).ok_or(MergeError::TooLarge { what: "the native image's header room", len: image.file_start })?;
    let bytes_len = u64_of_usize(image.bytes.len(), "the native image's bytes")?;
    let init_end = image.file_start + bytes_len;
    let text_at = image.file_start + shift;
    let data_at = got.at + shift;
    let file_end = init_end + shift;
    let end = up(image.end.max(got.at) + shift, page);
    Ok(Blocks { shift, text_at, data_at, file_end, end, got_at: got.at, got_len: got.len, init_end })
}

fn resolve_words<'a>(
    laid: &Laid<'_, '_>,
    image: &Image<'_>,
    exports: &[CExport<'_>],
    strip_underscore: bool,
    alloc: Alloc<'a>,
) -> Result<AVec<'a, u64>, MergeError> {
    let mut word_at: AVec<'a, u64> = Vec::with_capacity_in(image.words.len(), alloc);
    for (k, &word) in image.words.iter().enumerate() {
        let missing = MergeError::RustWordMissing { word: u32_of_usize(k, "a word index")? };
        let name = if strip_underscore { word.strip_prefix(b"_").ok_or(missing)? } else { word };
        let symbol = exports
            .binary_search_by(|e| e.name.cmp(name))
            .ok()
            .and_then(|i| exports.get(i).map(|e| e.symbol))
            .ok_or(missing)?;
        let at = laid.placed.iter().find(|p| p.symbol == symbol).map(|p| p.at).ok_or(missing)?;
        word_at.push(at);
    }
    Ok(word_at)
}

fn resolve_direct<'a>(
    laid: &Laid<'_, '_>,
    image: &Image<'_>,
    shift: u64,
    add_underscore: bool,
    alloc: Alloc<'a>,
) -> Result<AVec<'a, (u32, u64)>, MergeError> {
    let mut direct: AVec<'a, (u32, u64)> = Vec::new_in(alloc);
    let mut mangled: AVec<'_, u8> = Vec::new_in(alloc);
    for (k, import) in laid.imports.iter().enumerate() {
        let needle: &[u8] = if add_underscore {
            mangled.clear();
            mangled.push(b'_');
            mangled.extend_from_slice(import.name.as_bytes());
            &mangled[..]
        } else {
            import.name.as_bytes()
        };
        if let Ok(i) = image.exports.binary_search_by(|&(name, _)| name.cmp(needle))
            && let Some(&(_, at)) = image.exports.get(i)
        {
            direct.push((u32_of_usize(k, "a program import row")?, at + shift));
        }
    }
    Ok(direct)
}

/// Places a macOS `image` after `laid` and turns its fixups, words, imports and regions into the
/// Mach-O executable's terms (the module's doc says how). `exports` is `ProgramIn::exports`;
/// `libraries` is [`libraries`]'s answer for `image`.
pub fn merge<'a, 'i>(
    laid: &Laid<'_, '_>,
    image: &'i Image<'_>,
    exports: &[CExport<'_>],
    libraries: AVec<'a, AString<'a>>,
    alloc: Alloc<'a>,
) -> Result<Merged<'a, 'i>, MergeError> {
    if image.platform != NativePlatform::MacosArm64 {
        return Err(MergeError::Platform);
    }
    check_exports(exports)?;
    let region = |r: Region| image.region(r).map(|p| (p.at, p.len, u32::from(p.align_log2))).ok_or(MergeError::NoRegion { region: r });
    let blocks = place_blocks(laid, image, PAGE)?;
    let shift = blocks.shift;

    // Each word's Rust function, by name (Mach-O names carry the platform's `_`).
    let word_at = resolve_words(laid, image, exports, true, alloc)?;

    // Import rows: one per distinct (import, addend), in that order.
    let mut rows: AVec<'_, (u32, i64)> = Vec::new_in(alloc);
    for f in image.fixups.iter() {
        if let NativeFix::Import { index, addend } = f.to {
            rows.push((index, addend));
        }
    }
    rows.sort_unstable();
    rows.dedup();
    // The program's imports the image defines: bound to the definition, moved (`exports` of
    // the image are sorted by their mangled name).
    let direct = resolve_direct(laid, image, shift, true, alloc)?;
    let first_row = u32_of_usize(laid.imports.len() - direct.len(), "a chained import base")?;
    let mut imports: AVec<'a, ChainImport<'i>> = Vec::with_capacity_in(rows.len(), alloc);
    for &(index, addend) in rows.iter() {
        let idx = usize::try_from(index).map_err(|_| MergeError::TooLarge { what: "an import index of the native image", len: u64::from(index) })?;
        let row = image.imports.get(idx).ok_or(MergeError::TooLarge { what: "an import index of the native image", len: u64::from(index) })?;
        imports.push(ChainImport { lead: b"", name: row.name, ordinal: FLAT_LOOKUP, weak: row.weak, addend });
    }

    // The fixups: the image's, moved; then each word's cell, a rebase to its function.
    let (words_at, _, _) = region(Region::Words)?;
    let mut fixups: AVec<'a, (u64, Chain)> = Vec::with_capacity_in(image.fixups.len() + image.words.len(), alloc);
    // An executable image offset as a chained rebase's 36-bit target, refused where it is made.
    let target36 = |offset: u64| macho::Target36::new(offset).ok_or(MergeError::TooLarge { what: macho::REBASE_TOO_FAR, len: offset });
    for f in image.fixups.iter() {
        let to = match f.to {
            // The tag stays; the image address moves by `shift`.
            NativeFix::Rebase { address, high8 } => Chain::Rebase { target: target36(address.get() + shift)?, high8 },
            NativeFix::Import { index, addend } => {
                let k = rows.binary_search(&(index, addend)).map_err(|_| MergeError::TooLarge { what: "an import row of the native image", len: u64::from(index) })?;
                Chain::Bind { import: first_row + u32_of_usize(k, "an import row")? }
            }
            NativeFix::Word { index } => {
                let idx = usize::try_from(index).map_err(|_| MergeError::RustWordMissing { word: index })?;
                let at = word_at.get(idx).ok_or(MergeError::RustWordMissing { word: index })?;
                Chain::Rebase { target: target36(*at)?, high8: 0 }
            }
        };
        fixups.push((f.at + shift, to));
    }
    for (k, &at) in word_at.iter().enumerate() {
        let off = u64_of_usize(k, "a word cell index")?;
        fixups.push((words_at + 8 * off + shift, Chain::Rebase { target: target36(at)?, high8: 0 }));
    }
    fixups.sort_unstable_by_key(|&(at, _)| at);

    // The sections, at executable offsets; empty regions have none.
    let mut code: AVec<'a, Sect> = Vec::new_in(alloc);
    let mut data: AVec<'a, Sect> = Vec::new_in(alloc);
    for (r, name, flags, zero) in CODE_PARTS {
        let (at, len, align_log2) = region(r)?;
        if len > 0 {
            code.push(Sect { name, at: at + shift, len, zero, align_log2, flags });
        }
    }
    let mut thread_vars = false;
    for (r, name, flags, zero) in DATA_PARTS {
        let (at, len, align_log2) = region(r)?;
        if len > 0 {
            thread_vars |= r == Region::ThreadVars;
            data.push(Sect { name, at: at + shift, len, zero, align_log2, flags });
        }
    }

    Ok(Merged {
        shift,
        bytes: &image.bytes,
        text_at: blocks.text_at,
        data_at: blocks.data_at,
        file_end: blocks.file_end,
        end: blocks.end,
        fixups,
        direct,
        imports,
        libraries,
        code,
        data,
        thread_vars,
    })
}

/// Places a Linux arm64 `image` after `laid` and turns its fixups, words, imports, initializers
/// and TLS relocations into the ELF executable's terms (the module's doc says how). `exports` is
/// `ProgramIn::exports`; `libraries` is [`libraries`]'s answer for `image`.
pub fn merge_elf<'a, 'i>(
    laid: &Laid<'_, '_>,
    image: &'i Image<'_>,
    exports: &[CExport<'_>],
    libraries: AVec<'a, AString<'a>>,
    alloc: Alloc<'a>,
) -> Result<MergedElf<'a, 'i>, MergeError> {
    if image.platform != NativePlatform::LinuxArm64 {
        return Err(MergeError::Platform);
    }
    check_exports(exports)?;
    let region = |r: Region| image.region(r).ok_or(MergeError::NoRegion { region: r });
    let blocks = place_blocks(laid, image, elf::PAGE)?;
    let shift = blocks.shift;
    let got_at = blocks.got_at;
    let got_end = got_at + blocks.got_len;
    let init_end = blocks.init_end;

    let thread_vars = region(Region::ThreadVars)?;
    if thread_vars.len != 0 {
        return Err(MergeError::Shape { what: "Mach-O __thread_vars descriptors in an ELF image", at: thread_vars.at });
    }

    // Native C TLS template: ThreadData then ThreadZero.
    let tdata = region(Region::ThreadData)?;
    let tbss = region(Region::ThreadZero)?;
    let tls_at = if tdata.len > 0 { tdata.at } else { tbss.at };
    let tls_end = if tbss.len > 0 { tbss.at + tbss.len } else { tdata.at + tdata.len };
    let tls_len = tls_end.saturating_sub(tls_at);
    if !image.tls.is_empty() && tls_len == 0 {
        return Err(MergeError::Shape { what: "TLS relocations with no TLS template", at: tls_at });
    }
    let tls = if tls_len > 0 {
        let log2 = tdata.align_log2.max(tbss.align_log2);
        if log2 > 16 {
            return Err(MergeError::TooLarge { what: "a TLS alignment past a 64 KiB page", len: u64::from(log2) });
        }
        if tdata.len > 0 && (tdata.at < got_at || tdata.at + tdata.len > init_end) {
            return Err(MergeError::Shape { what: "ThreadData outside writable file bytes", at: tdata.at });
        }
        Some(ElfTls { vaddr: tls_at + shift, filesz: tdata.len, memsz: tls_len, align: 1u64 << log2 })
    } else {
        None
    };

    // Initializers: `DT_INIT_ARRAY` / `DT_INIT_ARRAYSZ` when non-empty.
    let inits_reg = region(Region::Inits)?;
    let inits = if inits_reg.len > 0 {
        if !inits_reg.at.is_multiple_of(8) || !inits_reg.len.is_multiple_of(8) {
            return Err(MergeError::Shape { what: "Inits not 8-byte aligned or sized", at: inits_reg.at });
        }
        if inits_reg.at < got_at || inits_reg.at + inits_reg.len > init_end {
            return Err(MergeError::Shape { what: "Inits outside writable file bytes", at: inits_reg.at });
        }
        Some(ElfInits { vaddr: inits_reg.at + shift, len: inits_reg.len })
    } else {
        None
    };

    // ELF C names carry no leading underscore.
    let word_at = resolve_words(laid, image, exports, false, alloc)?;
    let direct = resolve_direct(laid, image, shift, false, alloc)?;
    let mut imports: AVec<'a, ElfImport<'i>> = Vec::with_capacity_in(image.imports.len(), alloc);
    for imp in image.imports.iter() {
        imports.push(ElfImport { name: imp.name, weak: imp.weak });
    }

    let words_reg = region(Region::Words)?;
    let mut relative: AVec<'a, (u64, u64)> = Vec::with_capacity_in(image.fixups.len() + image.words.len(), alloc);
    let mut other: AVec<'a, ElfRela> = Vec::new_in(alloc);
    for f in image.fixups.iter() {
        if !f.at.is_multiple_of(8) || f.at < got_at || f.at + 8 > init_end {
            return Err(MergeError::Shape { what: "a fixup outside writable file bytes, or not 8-aligned", at: f.at });
        }
        match f.to {
            NativeFix::Rebase { address, high8 } => {
                let moved = Addr56::new(address.get() + shift).ok_or(MergeError::TooLarge { what: "a rebase target past 56 bits", len: address.get() + shift })?;
                relative.push((f.at + shift, moved.tagged(high8)));
            }
            NativeFix::Import { index, addend } => {
                let idx = usize::try_from(index).map_err(|_| MergeError::TooLarge { what: "an import index of the native image", len: u64::from(index) })?;
                if idx >= image.imports.len() {
                    return Err(MergeError::TooLarge { what: "an import index of the native image", len: u64::from(index) });
                }
                other.push(ElfRela { at: f.at + shift, kind: ElfRelaKind::Import { import: index, addend, got: f.at < got_end } });
            }
            NativeFix::Word { index } => {
                let idx = usize::try_from(index).map_err(|_| MergeError::RustWordMissing { word: index })?;
                let at = word_at.get(idx).ok_or(MergeError::RustWordMissing { word: index })?;
                relative.push((f.at + shift, *at));
            }
        }
    }
    for (k, &at) in word_at.iter().enumerate() {
        let cell = words_reg.at + 8 * u64_of_usize(k, "a word cell index")?;
        if !cell.is_multiple_of(8) || cell < got_at || cell + 8 > init_end {
            return Err(MergeError::Shape { what: "a word cell outside writable file bytes", at: cell });
        }
        relative.push((cell + shift, at));
    }
    let mut static_tls = false;
    for t in image.tls.iter() {
        let (at, offset, len, kind) = match *t {
            TlsReloc::Desc { got_at: cell, offset } => (cell, offset, 16u64, ElfRelaKind::TlsDesc { offset }),
            TlsReloc::TpRel { got_at: cell, offset } => {
                static_tls = true;
                (cell, offset, 8u64, ElfRelaKind::TlsTpRel { offset })
            }
        };
        if !at.is_multiple_of(8) || at < got_at || at + len > got_end {
            return Err(MergeError::Shape { what: "a TLS relocation's cell outside the Got, or not 8-aligned", at });
        }
        if offset >= tls_len {
            return Err(MergeError::Shape { what: "a TLS relocation's offset past the TLS template", at });
        }
        other.push(ElfRela { at: at + shift, kind });
    }
    relative.sort_unstable_by_key(|&(at, _)| at);
    for pair in relative.windows(2) {
        if pair[0].0 == pair[1].0 {
            return Err(MergeError::Shape { what: "two relative relocations at one cell", at: pair[1].0 });
        }
    }
    other.sort_unstable_by_key(|r| r.at);
    for pair in other.windows(2) {
        if pair[0].at == pair[1].at {
            return Err(MergeError::Shape { what: "two relocations at one cell", at: pair[1].at });
        }
    }

    Ok(MergedElf {
        shift,
        bytes: &image.bytes,
        text_at: blocks.text_at,
        data_at: blocks.data_at,
        file_end: blocks.file_end,
        end: blocks.end,
        direct,
        imports,
        relative,
        other,
        libraries,
        inits,
        tls,
        static_tls,
    })
}

/// Hand-laid images (the shape blaze-native `layout::lay` gives, as blaze-export's
/// tests/native_dylib.rs builds one) merged into a hand-laid program. Rules quoted: NATIVE.md 3.4
/// ("the launcher fills word i"; in an executable, a direct address), 3.9 ("`__blaze_inits`
/// become `__mod_init_func`, imports join with flat lookup ... the unwind rows of both go to one
/// `__unwind_info`"); libunwind reads a DWARF-mode entry's FDE at that offset of THE `__eh_frame`
/// (`<mach-o/compact_unwind_encoding.h>`: "UNWIND_ARM64_DWARF_SECTION_OFFSET").
#[cfg(test)]
mod tests {
    use blaze_code::record::Symbol;
    use blaze_native::Platform as NativePlatform;
    use blaze_native::image::{EhPointer, FixTo, Fixup, Frame, Image, Import, Placed, Region};
    use blaze_native::libraries::{Library, LibraryKind, Linked, Source, source};
    use blaze_native::unwind::Row;
    use blaze_own::{AVec, Alloc, Vec};

    use super::{MergeError, merge};
    use crate::eh_out::{Outside, eh_sections_with};
    use crate::input::{CExport, Platform};
    use crate::layout::{Laid, Placed as Home};
    use crate::macho::{Chain, PAGE};
    use crate::native_dylib::FLAT_LOOKUP;

    const START: u64 = 0x4000;
    const RET: u32 = 0xd65f_03c0;
    /// Where the hand-laid program ends (a page): the image's code lands here.
    const PROGRAM_END: u64 = 0x10000;
    const SHIFT: u64 = PROGRAM_END - START;

    fn alloc() -> Alloc<'static> {
        blaze_own::testing::heap().alloc()
    }

    /// `-l<name>` linked as the SDK's stub states `install` (blaze-native libraries.rs: a macOS
    /// library's install name is its stub's, read).
    fn mac_dylib(name: &'static str, install: &'static str) -> Linked<'static> {
        let Ok(Source::Stub(at)) = source(NativePlatform::MacosArm64, Library { kind: LibraryKind::Dylib, name }) else { panic!("`{name}` is named by its stub") };
        let stub = std::format!("--- !tapi-tbd\ntbd-version: 4\ntargets: [ arm64e-macos ]\ninstall-name: '{install}'\n...\n");
        at.linked(std::boxed::Box::leak(stub.into_bytes().into_boxed_slice())).unwrap_or_else(|e| panic!("the stub of `{name}`: {e}"))
    }

    fn put32(bytes: &mut [u8], at: u64, value: u32) {
        let i = (at - START) as usize;
        bytes[i..i + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn u32_at(b: &[u8], at: usize) -> u32 {
        u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
    }

    fn u16_at(b: &[u8], at: usize) -> u16 {
        u16::from_le_bytes([b[at], b[at + 1]])
    }

    /// Two functions (`_c_entry` at 0x4000, `_c_callback` at 0x4004); an eh_frame of 32 bytes
    /// whose one FDE (at 0x4010) covers `_c_callback` with a pc-relative `pc_begin` at 0x4018;
    /// rows: `_c_entry` compact with a personality through the GOT cell 0x8000 and an LSDA at
    /// 0x4008 (in the eh_frame's bytes, only an address here), `_c_callback` DWARF mode at
    /// FDE offset 8; one word `_rust_six` (and with `word_pointer` a C pointer cell to it at
    /// 0xC020), one init at 0xC038, imports `_puts` and `_weak_hook` (+8).
    fn image(word_pointer: bool) -> Image<'static> {
        let a = alloc();
        let mut regions: AVec<'static, Placed> = Vec::new_in(a);
        let rows: [(Region, u64, u64, u8); 12] = [
            (Region::Text, 0x4000, 8, 2),
            (Region::Const, 0x4008, 0, 0),
            (Region::EhFrame, 0x4008, 32, 3),
            (Region::UnwindInfo, 0x4028, 0, 2),
            (Region::Got, 0x8000, 8, 3),
            (Region::Data, 0xC000, 48, 3),
            (Region::Words, 0xC030, 8, 3),
            (Region::Inits, 0xC038, 8, 3),
            (Region::ThreadVars, 0xC040, 0, 3),
            (Region::ThreadData, 0xC040, 0, 0),
            (Region::ThreadZero, 0xC040, 0, 0),
            (Region::Zero, 0xC040, 16, 3),
        ];
        for (region, at, len, align_log2) in rows {
            regions.push(Placed { region, at, len, align_log2 });
        }
        let mut bytes: AVec<'static, u8> = Vec::new_in(a);
        bytes.resize((0xC040 - START) as usize, 0);
        put32(&mut bytes, 0x4000, RET);
        put32(&mut bytes, 0x4004, RET);
        put32(&mut bytes, 0x4010, 20); // the FDE's length
        put32(&mut bytes, 0x4018, (0x4004i64 - 0x4018) as i32 as u32); // pc_begin, as lay wrote it
        let mut fixups: AVec<'static, Fixup> = Vec::new_in(a);
        fixups.push(Fixup { at: 0x8000, to: FixTo::Import { index: 0, addend: 0 } });
        let first = if word_pointer { FixTo::Word { index: 0 } } else { FixTo::rebase(0x4000) };
        fixups.push(Fixup { at: 0xC020, to: first });
        fixups.push(Fixup { at: 0xC028, to: FixTo::Import { index: 1, addend: 8 } });
        fixups.push(Fixup { at: 0xC038, to: FixTo::rebase(0x4004) });
        let mut imports: AVec<'static, Import<'static>> = Vec::new_in(a);
        imports.push(Import { name: b"_puts", weak: false });
        imports.push(Import { name: b"_weak_hook", weak: true });
        let mut exports: AVec<'static, (&'static [u8], u64)> = Vec::new_in(a);
        exports.push((b"___blaze_native_v1", 0xC000));
        exports.push((b"_c_entry", 0x4000));
        let mut words: AVec<'static, &'static [u8]> = Vec::new_in(a);
        words.push(b"_rust_six");
        let mut libraries: AVec<'static, Linked<'static>> = Vec::new_in(a);
        libraries.push(mac_dylib("c", "/usr/lib/libSystem.B.dylib"));
        libraries.push(mac_dylib("c++", "/usr/lib/libc++.1.dylib"));
        let mut unwind: AVec<'static, Row> = Vec::new_in(a);
        unwind.push(Row { start: 0x4000, length: 4, encoding: 0x0400_0000, personality: Some(0x8000), lsda: Some(0x4008) });
        unwind.push(Row { start: 0x4004, length: 4, encoding: 0x0300_0000 | 8, personality: None, lsda: None });
        let mut frames: AVec<'static, Frame> = Vec::new_in(a);
        frames.push(Frame { at: 0x4010, len: 24, start: 0x4004, range: 4 });
        let mut eh_pointers: AVec<'static, EhPointer> = Vec::new_in(a);
        eh_pointers.push(EhPointer { at: 0x4018, width: 4, to: 0x4004, minus: 0x4018 });
        Image {
            platform: NativePlatform::MacosArm64,
            regions,
            bytes,
            file_start: START,
            end: 0xC050,
            fixups,
            tls: Vec::new_in(a),
            imports,
            exports,
            words,
            inits: 1,
            descriptor_at: 0xC000,
            libraries,
            unwind,
            frames,
            eh_pointers,
        }
    }

    /// A program of one placed function per `placed` (symbol, offset), ending at
    /// [`PROGRAM_END`], with one C import of its own (`_exit`).
    fn laid(placed: &[(Symbol, u64)]) -> Laid<'static, 'static> {
        let a = alloc();
        let mut homes: AVec<'static, Home> = Vec::new_in(a);
        for &(symbol, at) in placed {
            homes.push(Home { symbol, at, len: 4, tier: None, folded: false });
        }
        let mut imports = Vec::new_in(a);
        imports.push(crate::layout::Import { symbol: Symbol::of_name("exit"), name: "exit" });
        Laid {
            page: PAGE,
            text_at: START,
            text: Vec::new_in(a),
            const_at: 0x8000,
            konst: Vec::new_in(a),
            got_at: 0x8000,
            data_at: 0xC000,
            data: Vec::new_in(a),
            zero_at: 0xC000,
            zero_len: 0,
            end: PROGRAM_END,
            imports,
            fixups: Vec::new_in(a),
            entry: START,
            placed: homes,
            tables_room: 0x8000 - START,
            mod_init_at: None,
            has_tls: false,
            tls_count: 0,
        }
    }

    fn exports() -> [CExport<'static>; 1] {
        [CExport { name: b"rust_six", symbol: Symbol::of_name("rust_six") }]
    }

    /// Prints `word_becomes_direct_address: word cell 0x18030 and C pointer 0x18020 rebase to 0x5000`.
    #[test]
    fn word_becomes_direct_address() {
        let img = image(true);
        let program = laid(&[(Symbol::of_name("rust_six"), 0x5000)]);
        let libs = super::libraries(&img, alloc()).unwrap();
        let m = merge(&program, &img, &exports(), libs, alloc()).unwrap();
        assert_eq!(m.shift, SHIFT);
        let find = |at: u64| m.fixups.iter().find(|f| f.0 == at).map(|f| f.1);
        assert_eq!(find(0xC030 + SHIFT), Chain::untagged(0x5000), "word 0's cell is the Rust function's address");
        assert_eq!(find(0xC020 + SHIFT), Chain::untagged(0x5000), "a C pointer to a Rust function is its address");
        assert!(m.fixups.windows(2).all(|w| w[0].0 < w[1].0), "fixups sorted by address");
        // Control: no export of that name, the word is refused by its index.
        let libs = super::libraries(&img, alloc()).unwrap();
        assert_eq!(merge(&program, &img, &[], libs, alloc()).err(), Some(MergeError::RustWordMissing { word: 0 }));
        println!("word_becomes_direct_address: word cell {:#x} and C pointer {:#x} rebase to 0x5000", 0xC030 + SHIFT, 0xC020 + SHIFT);
    }

    /// Prints `merged_program_has_init_and_import: __mod_init_func at 0x18038, 2 flat imports after 1 of the program, libraries ["/usr/lib/libc++.1.dylib"]`.
    #[test]
    fn merged_program_has_init_and_import() {
        let img = image(false);
        let program = laid(&[(Symbol::of_name("rust_six"), 0x5000)]);
        let libs = super::libraries(&img, alloc()).unwrap();
        let m = merge(&program, &img, &exports(), libs, alloc()).unwrap();
        let init = m.data.iter().find(|s| s.name == "__mod_init_func").expect("the inits are __mod_init_func");
        assert_eq!((init.at, init.len, init.flags), (0xC038 + SHIFT, 8, 0x9));
        assert_eq!(m.imports.len(), 2);
        assert!(m.imports.iter().all(|i| i.ordinal == FLAT_LOOKUP), "every native import binds by flat lookup");
        assert_eq!((m.imports[1].name, m.imports[1].weak, m.imports[1].addend), (&b"_weak_hook"[..], true, 8));
        assert_eq!(m.fixups.iter().find(|f| f.0 == 0x8000 + SHIFT).map(|f| f.1), Some(Chain::Bind { import: 1 }), "rows count after the program's own");
        assert_eq!(m.fixups.iter().find(|f| f.0 == 0xC038 + SHIFT).map(|f| f.1), Chain::untagged(0x4004 + SHIFT));
        // libSystem (for `c`) is the executable's own first library: not repeated.
        assert_eq!(m.libraries.len(), 1);
        assert_eq!(m.libraries[0].as_str(), "/usr/lib/libc++.1.dylib");
        assert_eq!((m.text_at, m.data_at), (PROGRAM_END, 0x8000 + SHIFT));
        println!(
            "merged_program_has_init_and_import: __mod_init_func at {:#x}, 2 flat imports after 1 of the program, libraries [{:?}]",
            init.at,
            m.libraries[0].as_str()
        );
    }

    /// Prints `two_exports_of_one_name_refused: TwoExports { .. }`.
    #[test]
    fn two_exports_of_one_name_refused() {
        let img = image(false);
        let program = laid(&[]);
        let two = [
            CExport { name: b"rust_six", symbol: Symbol::of_name("a::rust_six") },
            CExport { name: b"rust_six", symbol: Symbol::of_name("b::rust_six") },
        ];
        let libs = super::libraries(&img, alloc()).unwrap();
        let e = merge(&program, &img, &two, libs, alloc()).err();
        assert_eq!(e, Some(MergeError::TwoExports { first: Symbol::of_name("a::rust_six"), second: Symbol::of_name("b::rust_six") }));
        println!("two_exports_of_one_name_refused: TwoExports {{ .. }}");
    }

    /// A program import the image defines binds to it directly and leaves the import table; the
    /// image's binds number after the rows left. Control: an import the image does not define
    /// stays a row and the image's binds start after both. Prints
    /// `program_import_the_image_defines_is_direct: row 1 (c_entry) rebases to 0x10000; the image's binds start at row 1; control: at row 2`.
    #[test]
    fn program_import_the_image_defines_is_direct() {
        let img = image(false);
        let with = |name: &'static str| {
            let mut program = laid(&[(Symbol::of_name("rust_six"), 0x5000)]);
            program.imports.push(crate::layout::Import { symbol: Symbol::of_name(name), name });
            let libs = super::libraries(&img, alloc()).unwrap();
            let m = merge(&program, &img, &exports(), libs, alloc()).unwrap();
            let first = m.fixups.iter().find(|f| f.0 == 0x8000 + SHIFT).map(|f| f.1);
            (m.direct.len(), m.direct.first().copied(), first, m.bound(program.imports.len()))
        };
        let (rows, direct, first, bound) = with("c_entry");
        assert_eq!((rows, direct), (1, Some((1u32, 0x4000 + SHIFT))), "the program's `c_entry` row binds to the image's `_c_entry`, moved");
        assert_eq!(first, Some(Chain::Bind { import: 1 }), "the image's rows follow the one program row left");
        assert_eq!(bound, 3, "`exit`, then the image's two");
        let (rows, _, first, bound) = with("c_elsewhere");
        assert_eq!(rows, 0, "control: a name the image does not define stays an import");
        assert_eq!(first, Some(Chain::Bind { import: 2 }));
        assert_eq!(bound, 4);
        println!(
            "program_import_the_image_defines_is_direct: row 1 (c_entry) rebases to {:#x}; the image's binds start at row 1; control: at row 2",
            0x4000 + SHIFT
        );
    }

    /// The image's rows join the file's ONE `__unwind_info` and its eh_frame the ONE
    /// `__eh_frame` (main's ruling (a)). Control: without the image's rows (`None`), the native
    /// function has no entry and the personality array is empty: a C++ throw in it would find
    /// no handler. Prints `native_rows_join_the_one_unwind_info: 1 personality, DWARF entry at
    /// eh_frame+.. for 0x10004; control: 0 entries for it`.
    #[test]
    fn native_rows_join_the_one_unwind_info() {
        let img = image(false);
        let program = laid(&[]);
        let outside = Outside { image: &img, shift: SHIFT };
        let eh = eh_sections_with(&program, &[], None, Platform::MacosArm64, Some(&outside), alloc()).unwrap();
        let u = &eh.unwind_info;
        assert_eq!(u32_at(u, 16), 1, "one personality, the image's");
        let personality_at = u32_at(u, 12) as usize;
        assert_eq!(u64::from(u32_at(u, personality_at)), 0x8000 + SHIFT, "named by its GOT cell, moved");
        // First-level entry 0, its regular page, the entries (function offset, encoding).
        let index_at = u32_at(u, 20) as usize;
        let page_at = u32_at(u, index_at + 4) as usize;
        assert_eq!(u32_at(u, page_at), 2, "a regular page");
        let entries_at = page_at + usize::from(u16_at(u, page_at + 4));
        let count = usize::from(u16_at(u, page_at + 6));
        let mut dwarf = None;
        for k in 0..count {
            if u64::from(u32_at(u, entries_at + 8 * k)) == 0x4004 + SHIFT {
                dwarf = Some(u32_at(u, entries_at + 8 * k + 4));
            }
        }
        let dwarf = dwarf.expect("the C function has an entry");
        assert_eq!(dwarf & 0x0F00_0000, 0x0300_0000, "DWARF mode");
        let fde = (dwarf & 0x00FF_FFFF) as usize;
        // The FDE is the image's, copied into the one eh_frame; its pc_begin re-encoded.
        assert_eq!(u32_at(&eh.eh_frame, fde), 20, "the entry's offset reaches the image's FDE");
        let field = eh.eh_frame_at + fde as u64 + 8;
        let pc_begin = i64::from(u32_at(&eh.eh_frame, fde + 8) as i32);
        assert_eq!(field.wrapping_add(pc_begin as u64), 0x4004 + SHIFT, "pc_begin reaches the moved function");

        // Control: the image's rows dropped.
        let bare = eh_sections_with(&program, &[], None, Platform::MacosArm64, None, alloc()).unwrap();
        let b = &bare.unwind_info;
        assert_eq!(u32_at(b, 16), 0, "control: no personality");
        let mut covered = 0;
        if b.len() > 28 {
            let index_at = u32_at(b, 20) as usize;
            let indexes = u32_at(b, 24) as usize;
            if indexes > 1 {
                let page_at = u32_at(b, index_at + 4) as usize;
                let entries_at = page_at + usize::from(u16_at(b, page_at + 4));
                for k in 0..usize::from(u16_at(b, page_at + 6)) {
                    covered += usize::from(u64::from(u32_at(b, entries_at + 8 * k)) == 0x4004 + SHIFT);
                }
            }
        }
        assert_eq!(covered, 0, "control: the C function is not covered without the image's rows");
        println!(
            "native_rows_join_the_one_unwind_info: 1 personality, DWARF entry at eh_frame+{fde:#x} for {:#x}; control: 0 entries for it",
            0x4004 + SHIFT
        );
    }
}
