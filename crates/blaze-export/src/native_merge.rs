//! Export merges the native image (NATIVE.md 3.9, piece W9; main's rulings of 2026-11-16): the
//! program's native library image (blaze-native `layout::lay`, the same image the run path loads
//! as a dylib) placed inside the exported Mach-O executable, so the file runs with no blaze and
//! no native library record beside it.
//!
//! # Where the image goes
//!
//! The image is moved as ONE block by one `shift` (image address + `shift` = the executable's
//! image offset), a multiple of the page: its first byte after the dylib header room
//! (`Image::file_start`) lands on the program's end (`Laid::end`). Every pc-relative reference
//! inside the image (a branch, an `adrp` to its GOT, a word stub) keeps its distance, so
//! `layout::lay`'s resolution stands and only the 64-bit pointers (the fixups) move. Two
//! segments hold it: `__NATIVE_TEXT` (r-x: its code and constants; the image's own
//! `__eh_frame` and `__unwind_info` bytes ride along unread) and `__NATIVE_DATA` (rw-: GOT,
//! data, words, initializers, thread-local descriptors and templates, zero fill). In the file
//! the block starts on the page after the program's file bytes (the program's `__bss` takes no
//! file bytes); its file offsets are its image offsets less one constant ([`Merged::file_of`]).
//!
//! # What each part of the image becomes
//!
//! - A word (C calling Rust, NATIVE.md 3.4) becomes a direct address: word `i`'s cell and every
//!   `FixTo::Word { index: i }` cell (the BNL2 pointer rows) is a rebase to the Rust function the
//!   word names, found through `ProgramIn::exports` (sorted by name; the front end gives it from
//!   `#[no_mangle]` and `#[export_name]` items) and `Laid::placed`. No launcher writes anything.
//! - The initializers (`Inits`) are `__mod_init_func`: dyld runs them before `main`, after the
//!   libraries' own (the order of a normal executable, NATIVE.md 3.4).
//! - Thread-locals keep Mach-O's own form: `__thread_vars` (descriptors whose thunk binds
//!   `__tlv_bootstrap`), `__thread_data`, `__thread_bss`, and `MH_HAS_TLV_DESCRIPTORS`; the
//!   program's own code has none (export refuses Rust thread-locals by name).
//! - Imports bind by flat lookup (`BIND_SPECIAL_DYLIB_FLAT_LOOKUP`, NATIVE.md 3.5) among the
//!   file's libraries: libSystem first (the program's own ordinal 1), then one `LC_LOAD_DYLIB` per
//!   distinct install name of `Image::libraries`, in the image's order.
//! - A program import the image defines (a Rust call to a C function of the image) binds to it
//!   directly: its GOT cell is a rebase and its row leaves the import table ([`Merged::direct`]),
//!   as the run path's `dlsym(handle, name)` finds the image's definition first.
//! - The unwind rows and frame entries (`Image::unwind`, `frames`, `eh_pointers`) go to
//!   eh_out.rs, which builds the file's ONE `__eh_frame` and ONE `__unwind_info` from the Rust
//!   rows and these ([`crate::eh_out::Outside`]).
//!
//! ELF executables with a native image are refused by name ([`MergeError::ElfNotBuilt`]) until
//! the ELF twin of this piece is written (W5 is the run path's ELF shared object).

use core::fmt;

use blaze_code::record::Symbol;
use blaze_native::Platform as NativePlatform;
use blaze_native::image::{FixTo as NativeFix, Image, Region};
use blaze_own::{AString, AVec, Alloc, String, Vec};

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
/// The sections the two segments may hold, at most: `__text`, `__const`; `__got`, `__data`,
/// `__blaze_words`, `__mod_init_func`, `__thread_vars`, `__thread_data`, `__thread_bss`, `__bss`.
const MOST_SECTIONS: u64 = 10;
/// `dylib_command`'s name offset.
const DYLIB_NAME_AT: u64 = 24;

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
    /// macOS: its install name is no Mach-O load command's (NATIVE.md 3.5).
    Library { library: u32 },
    /// An image laid for another platform than the executable's.
    Platform,
    /// The image lacks region `region` (`layout::lay` lists every region, so this is a broken image).
    NoRegion { region: Region },
    /// The image's GOT does not start a page, so its two blocks could not move together.
    Unaligned { at: u64 },
    /// An ELF executable with a native image: not built (NATIVE.md W9, macOS first).
    ElfNotBuilt,
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
            MergeError::Library { library } => write!(f, "the native image's library {library} was resolved for another platform than macOS (NATIVE.md 3.5)"),
            MergeError::Platform => write!(f, "the native image was laid for another platform than the executable's"),
            MergeError::NoRegion { region } => write!(f, "the native image has no {region:?} region"),
            MergeError::Unaligned { at } => write!(f, "the native image's GOT at {at:#x} does not start a page"),
            MergeError::ElfNotBuilt => write!(f, "an ELF executable with a native image is not built yet (NATIVE.md W9: macOS first)"),
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

/// The image merged: where its blocks are, what the loader fixes in them, the import rows and
/// libraries they add, their sections. The executable writer (macho.rs `write_native`) copies
/// `bytes` and writes the rest.
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

fn up(n: u64, to: u64) -> u64 {
    n.div_ceil(to) * to
}

/// The install names of `image`'s libraries: distinct, in the image's order, libSystem (the
/// executable's first library) left out. Computed once, before the layout: the header's room
/// depends on them ([`command_room`]).
pub fn libraries<'a>(image: &Image<'_>, alloc: Alloc<'a>) -> Result<AVec<'a, AString<'a>>, MergeError> {
    let mut paths: AVec<'a, AString<'a>> = Vec::new_in(alloc);
    for (k, library) in image.libraries.iter().enumerate() {
        if library.platform() != NativePlatform::MacosArm64 {
            return Err(MergeError::Library { library: k as u32 });
        }
        let install = library.install();
        if install == macho::LIB_SYSTEM || paths.iter().any(|p| p.as_str() == install) {
            continue;
        }
        let mut path: AString<'a> = String::new_in(alloc);
        path.push_str(install);
        paths.push(path);
    }
    Ok(paths)
}

/// The load-command bytes the image adds at most: two segments with every section they may
/// hold, one `LC_LOAD_DYLIB` per library. The executable's header room grows by it
/// (macho.rs `plan_with`), so the code's place depends only on the image.
pub fn command_room(libraries: &[AString<'_>]) -> u64 {
    let mut room = 2 * macho::SEGMENT + MOST_SECTIONS * macho::SECTION;
    for path in libraries {
        room += DYLIB_NAME_AT + up(path.len() as u64 + 1, 8);
    }
    room
}

/// Places `image` after `laid` and turns its fixups, words, imports and regions into the
/// executable's terms (the module's doc says how). `exports` is `ProgramIn::exports`;
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
    for (k, pair) in exports.windows(2).enumerate() {
        match pair[0].name.cmp(pair[1].name) {
            core::cmp::Ordering::Less => {}
            core::cmp::Ordering::Equal => return Err(MergeError::TwoExports { first: pair[0].symbol, second: pair[1].symbol }),
            core::cmp::Ordering::Greater => return Err(MergeError::ExportsUnsorted { at: k as u32 + 1 }),
        }
    }
    let region = |r: Region| image.region(r).map(|p| (p.at, p.len, u32::from(p.align_log2))).ok_or(MergeError::NoRegion { region: r });
    let (got_at, _, _) = region(Region::Got)?;
    if got_at % PAGE != 0 || image.file_start % PAGE != 0 {
        return Err(MergeError::Unaligned { at: got_at });
    }
    if laid.end % PAGE != 0 {
        return Err(MergeError::Unaligned { at: laid.end });
    }
    let shift = laid.end.checked_sub(image.file_start).ok_or(MergeError::TooLarge { what: "the native image's header room", len: image.file_start })?;
    let text_at = image.file_start + shift;
    let data_at = got_at + shift;
    let file_end = image.file_start + image.bytes.len() as u64 + shift;
    let end = up(image.end.max(got_at) + shift, PAGE);

    // Each word's Rust function, by name (Mach-O names carry the platform's `_`).
    let mut word_at: AVec<'_, u64> = Vec::with_capacity_in(image.words.len(), alloc);
    for (k, &word) in image.words.iter().enumerate() {
        let missing = MergeError::RustWordMissing { word: k as u32 };
        let name = word.strip_prefix(b"_").ok_or(missing)?;
        let symbol = exports.binary_search_by(|e| e.name.cmp(name)).map(|i| exports[i].symbol).map_err(|_| missing)?;
        let at = laid.placed.iter().find(|p| p.symbol == symbol).map(|p| p.at).ok_or(missing)?;
        word_at.push(at);
    }

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
    let mut direct: AVec<'a, (u32, u64)> = Vec::new_in(alloc);
    let mut mangled: AVec<'_, u8> = Vec::new_in(alloc);
    for (k, import) in laid.imports.iter().enumerate() {
        mangled.clear();
        mangled.push(b'_');
        mangled.extend_from_slice(import.name.as_bytes());
        if let Ok(i) = image.exports.binary_search_by(|&(name, _)| name.cmp(&mangled[..])) {
            direct.push((k as u32, image.exports[i].1 + shift));
        }
    }
    let first_row = (laid.imports.len() - direct.len()) as u32;
    let mut imports: AVec<'a, ChainImport<'i>> = Vec::with_capacity_in(rows.len(), alloc);
    for &(index, addend) in rows.iter() {
        let row = image.imports.get(index as usize).ok_or(MergeError::TooLarge { what: "an import index of the native image", len: u64::from(index) })?;
        imports.push(ChainImport { lead: b"", name: row.name, ordinal: FLAT_LOOKUP, weak: row.weak, addend });
    }

    // The fixups: the image's, moved; then each word's cell, a rebase to its function.
    let (words_at, _, _) = region(Region::Words)?;
    let mut fixups: AVec<'a, (u64, Chain)> = Vec::with_capacity_in(image.fixups.len() + image.words.len(), alloc);
    for f in image.fixups.iter() {
        let to = match f.to {
            NativeFix::Rebase { target } => Chain::Rebase { target: target + shift },
            NativeFix::Import { index, addend } => {
                let k = rows.binary_search(&(index, addend)).map_err(|_| MergeError::TooLarge { what: "an import row of the native image", len: u64::from(index) })?;
                Chain::Bind { import: first_row + k as u32 }
            }
            NativeFix::Word { index } => {
                let at = word_at.get(index as usize).ok_or(MergeError::RustWordMissing { word: index })?;
                Chain::Rebase { target: *at }
            }
        };
        fixups.push((f.at + shift, to));
    }
    for (k, &at) in word_at.iter().enumerate() {
        fixups.push((words_at + 8 * k as u64 + shift, Chain::Rebase { target: at }));
    }
    fixups.sort_unstable_by_key(|&(at, _)| at);

    // The sections, at executable offsets; empty regions have none.
    let mut code: AVec<'a, Sect> = Vec::new_in(alloc);
    let mut data: AVec<'a, Sect> = Vec::new_in(alloc);
    let code_parts: [(Region, &'static str, u32, bool); 2] = [(Region::Text, "__text", S_CODE, false), (Region::Const, "__const", S_REGULAR, false)];
    let data_parts: [(Region, &'static str, u32, bool); 8] = [
        (Region::Got, "__got", S_REGULAR, false),
        (Region::Data, "__data", S_REGULAR, false),
        (Region::Words, "__blaze_words", S_REGULAR, false),
        (Region::Inits, "__mod_init_func", S_MOD_INIT_FUNC_POINTERS, false),
        (Region::ThreadVars, "__thread_vars", S_THREAD_LOCAL_VARIABLES, false),
        (Region::ThreadData, "__thread_data", S_THREAD_LOCAL_REGULAR, false),
        (Region::ThreadZero, "__thread_bss", S_THREAD_LOCAL_ZEROFILL, true),
        (Region::Zero, "__bss", S_ZEROFILL, true),
    ];
    for (r, name, flags, zero) in code_parts {
        let (at, len, align_log2) = region(r)?;
        if len > 0 {
            code.push(Sect { name, at: at + shift, len, zero, align_log2, flags });
        }
    }
    let mut thread_vars = false;
    for (r, name, flags, zero) in data_parts {
        let (at, len, align_log2) = region(r)?;
        if len > 0 {
            thread_vars |= r == Region::ThreadVars;
            data.push(Sect { name, at: at + shift, len, zero, align_log2, flags });
        }
    }

    Ok(Merged { shift, bytes: &image.bytes, text_at, data_at, file_end, end, fixups, direct, imports, libraries, code, data, thread_vars })
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
        let first = if word_pointer { FixTo::Word { index: 0 } } else { FixTo::Rebase { target: 0x4000 } };
        fixups.push(Fixup { at: 0xC020, to: first });
        fixups.push(Fixup { at: 0xC028, to: FixTo::Import { index: 1, addend: 8 } });
        fixups.push(Fixup { at: 0xC038, to: FixTo::Rebase { target: 0x4004 } });
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
        assert_eq!(find(0xC030 + SHIFT), Some(Chain::Rebase { target: 0x5000 }), "word 0's cell is the Rust function's address");
        assert_eq!(find(0xC020 + SHIFT), Some(Chain::Rebase { target: 0x5000 }), "a C pointer to a Rust function is its address");
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
        assert_eq!(m.fixups.iter().find(|f| f.0 == 0xC038 + SHIFT).map(|f| f.1), Some(Chain::Rebase { target: 0x4004 + SHIFT }));
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
