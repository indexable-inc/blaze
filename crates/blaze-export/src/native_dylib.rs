//! The program's native library record as a Mach-O `MH_DYLIB` for arm64 macOS (NATIVE.md 3.1,
//! 3.4, 3.5, 3.6, 3.10; piece W4): headers and load commands placed around a
//! [`blaze_native::image::Image`] that `blaze_native::layout::lay` already laid out at
//! image-relative addresses. Nothing here moves a byte of the image: every region keeps its
//! address, file offset equals address up to the end of initialized data, and only the fixup
//! slots are rewritten into dyld's chained form.
//!
//! PUBLIC SIGNATURE (W7 makes the record with it, W8 loads it, W9 reads the same image):
//!
//! ```text
//! pub enum DylibError<'a> {
//!     NotMac,                                         // image.platform is not MacosArm64
//!     Library { name: &'a str },                      // a library resolved for another platform
//!     TooLarge { what: &'static str, len: u64 },      // a field of the format too narrow
//!     Slot { what: &'static str, value: u64, at: u64 }, // a chained slot's value its field cannot hold
//!     Alloc,                                          // the output could not be reserved
//!     Identifier { len: usize },                      // not 64 lowercase hex digits
//!     TlsRelocs { count: usize },                     // ELF TLS relocations in a Mach-O image
//!     Shape { what: &'static str, at: u64 },          // the image breaks a promise of image.rs
//! }
//! pub const FLAT_LOOKUP: u16;   // 0xFFFE: every import's ordinal (BIND_SPECIAL_DYLIB_FLAT_LOOKUP)
//! pub const MIN_MACOS: u32;     // 0x000E_0000: macOS 14.0 in LC_BUILD_VERSION
//! pub fn write<'a>(image: &Image<'a>, identifier: &str, out: &mut AVec<'_, u8>, alloc: Alloc<'_>)
//!     -> Result<(), DylibError<'a>>;
//! ```
//!
//! The file, in order (`<mach-o/loader.h>`, `<mach-o/fixup-chains.h>`, `<mach-o/nlist.h>`):
//!
//! | segment | sections | protection |
//! |---|---|---|
//! | `__TEXT` | `__text`, `__const`, `__eh_frame`, `__unwind_info` | r-x; the header and load commands in the image's first 0x4000 bytes |
//! | `__DATA_CONST` | `__got` | rw-, read-only once dyld has bound it (`SG_READ_ONLY`); absent when the Got is empty |
//! | `__DATA` | `__data` (the descriptor `__blaze_native_v1` first), `__blaze_words`, `__blaze_inits`, `__thread_vars`, `__thread_data`, `__thread_bss`, `__bss` | rw- |
//! | `__LINKEDIT` | none | r--: chained fixups, export trie, function starts (empty), symbols of the exports, the signature last |
//!
//! Load commands: the segments, `LC_ID_DYLIB` (the identifier), one `LC_LOAD_DYLIB` per distinct
//! install name (libSystem first, then the image's libraries in order), `LC_DYLD_CHAINED_FIXUPS`
//! (`DYLD_CHAINED_PTR_64_OFFSET` slots, `DYLD_CHAINED_IMPORT_ADDEND64` rows, every row's ordinal
//! `FLAT_LOOKUP`, its weak bit from `Import::weak`), `LC_DYLD_EXPORTS_TRIE`, `LC_SYMTAB`,
//! `LC_DYSYMTAB`, `LC_UUID` (a digest of the content), `LC_BUILD_VERSION` (macOS 14.0, sdk 0),
//! `LC_FUNCTION_STARTS` (empty), `LC_CODE_SIGNATURE` ([`crate::sign::sign`], not a main binary).
//!
//! Decided here:
//! - No `__mod_init_func`: the members' initializers are `__blaze_inits`, which the launcher runs
//!   after it has filled the words (NATIVE.md 3.4); dlopen runs only the libraries' own.
//! - `__got` is `S_REGULAR`, not `S_NON_LAZY_SYMBOL_POINTERS`: its cells are imports, local
//!   rebases and Rust words alike, and chained fixups need no indirect symbol table.
//! - Imports with an addend get their own row (`DYLD_CHAINED_IMPORT_ADDEND64` holds it), appended
//!   after the image's imports in (import, addend) order; the slot's 8-bit inline addend stays 0.
//! - A `FixTo::Word` fixup (a C data word holding a Rust function's address) gets no dynamic
//!   fixup: the descriptor's pointer rows list its cell (BNL2, blaze-native descriptor.rs) and the
//!   launcher writes it with the function's one address, its slot's thunk.
//! - The chained-fixup encoder is macho.rs's (`chained_fixups`), shared with the executable
//!   writer; the unwind tables are already inside the image (W3 writes them with
//!   `blaze_native::unwind`, the writer P10 shares): this file writes no unwind table.
//!
//! Determinism (NATIVE.md 3.10): the bytes are a function of the image and the identifier: no
//! path but the install names, exports in the image's name order, imports in its order, the
//! UUID a digest of the content, the signature's identifier the record's hex id.

use blaze_ids::Hasher;
use blaze_native::Platform;
use blaze_native::image::{FixTo, Image, Placed, Region};
use blaze_own::{AString, AVec, Alloc, String, Vec};

use crate::bytes;
use crate::commands::{Command, Commands, CommandsError, Header, Section, Segment};
use crate::macho::{self, Chain, ChainImport, ChainSeg, ImportRows, PAGE};
use crate::sign;

/// Why the image could not be written as a dylib; each names the thing and the rule.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DylibError<'a> {
    /// The image is not for arm64 macOS (NATIVE.md 3.1: Mach-O on macOS; Linux is W5's).
    NotMac,
    /// A library of the image resolved for another platform than macOS: its install name is
    /// no Mach-O load command's (NATIVE.md 3.5).
    Library { name: &'a str },
    /// A value does not fit the format's field `what` (chained fixups' widths, 32-bit offsets,
    /// the header room of `Layout::header_reserve`).
    TooLarge { what: &'static str, len: u64 },
    /// The chained slot at image address `at` holds `value`, which the slot's field `what`
    /// cannot (rule 19: the place, which the layout's fixup list names).
    Slot { what: &'static str, value: u64, at: u64 },
    /// The output's room could not be reserved.
    Alloc,
    /// The identifier is not a record id's 64 lowercase hex digits (NATIVE.md 3.10).
    Identifier { len: usize },
    /// The image carries ELF TLS relocations, which Mach-O has no form for (NATIVE.md 3.6).
    TlsRelocs { count: usize },
    /// The image breaks a promise image.rs makes (`what`), at image address `at`.
    Shape { what: &'static str, at: u64 },
    /// The header and load commands need `need` bytes and the image keeps `room` before its
    /// first region (`Layout::header_reserve`): many or long install names.
    HeaderRoom { need: u64, room: u64 },
}

fn commands_error<'a>(e: CommandsError) -> DylibError<'a> {
    match e {
        CommandsError::Room { need, room } => DylibError::HeaderRoom { need, room },
        CommandsError::Field { what, value } => DylibError::TooLarge { what, len: value },
    }
}

/// `BIND_SPECIAL_DYLIB_FLAT_LOOKUP` (-2) in `dyld_chained_import_addend64`'s 16-bit
/// `lib_ordinal` (dyld reads a value above 0xFFF0 as a negative special ordinal): every import is
/// looked up among the libraries the image loads (NATIVE.md 3.5).
pub const FLAT_LOOKUP: u16 = 0xFFFE;
/// macOS 14.0 (`xxxx.yy.zz` in nibbles), the image's minimum in `LC_BUILD_VERSION`.
pub const MIN_MACOS: u32 = 0x000E_0000;

const MH_DYLIB: u32 = 6;
const MH_DYLDLINK: u32 = 0x4;
const MH_TWOLEVEL: u32 = 0x80;
const MH_NO_REEXPORTED_DYLIBS: u32 = 0x10_0000;
/// dyld sets up `__thread_vars` only in an image with this flag.
const MH_HAS_TLV_DESCRIPTORS: u32 = 0x80_0000;
const LC_ID_DYLIB: u32 = 0xD;
const LC_DYLD_EXPORTS_TRIE: u32 = 0x8000_0033;
const S_THREAD_LOCAL_REGULAR: u32 = 0x11;
const S_THREAD_LOCAL_ZEROFILL: u32 = 0x12;
const S_THREAD_LOCAL_VARIABLES: u32 = 0x13;
/// `N_SECT | N_EXT`: a defined external symbol.
const N_SECT_EXT: u8 = 0x0E | 0x01;
const EXPORT_KIND_REGULAR: u64 = 0;
const EXPORT_KIND_THREAD_LOCAL: u64 = 1;
/// 1.0.0: the image's own versions, and what it asks of every library but libSystem.
pub(crate) const DYLIB_VERSION: u32 = 1 << 16;
const UUID_DOMAIN: &str = "blaze-export native dylib uuid 1";
/// The export trie's offsets settle in a few passes (each pass only grows a uleb); more is a bug.
const TRIE_PASSES: u32 = 32;

fn up(n: u64, to: u64) -> u64 {
    n.next_multiple_of(to)
}

fn shape<'a>(what: &'static str, at: u64) -> DylibError<'a> {
    DylibError::Shape { what, at }
}

fn end_of(p: &Placed) -> u64 {
    p.at + p.len
}

/// A section as its load command and the symbol table see it.
#[derive(Clone, Copy)]
struct Sect {
    name: &'static str,
    seg: &'static str,
    at: u64,
    len: u64,
    align: u32,
    flags: u32,
    zero: bool,
}

/// A segment: image addresses (the dylib's `__TEXT` is at address 0), its sections
/// `sects[first..first + count]`.
#[derive(Clone, Copy)]
struct Seg {
    name: &'static str,
    vm: u64,
    vm_size: u64,
    file: u64,
    file_size: u64,
    prot: u32,
    flags: u32,
    first: usize,
    count: usize,
    fixups: bool,
}

fn add(sects: &mut AVec<'_, Sect>, name: &'static str, seg: &'static str, p: &Placed, flags: u32, keep_empty: bool) {
    if p.len == 0 && !keep_empty {
        return;
    }
    let zero = flags == macho::S_ZEROFILL || flags == S_THREAD_LOCAL_ZEROFILL;
    sects.push(Sect { name, seg, at: p.at, len: p.len, align: u32::from(p.align_log2), flags, zero });
}

/// Writes `image` as an MH_DYLIB for arm64 macOS whose install name and code-signing identifier
/// are `identifier` (the record's 64-hex id), into `out` (its contents replaced). The libraries
/// are `image.libraries`, loaded by flat lookup.
pub fn write<'a, 'b>(image: &Image<'a>, identifier: &str, out: &mut AVec<'_, u8>, alloc: Alloc<'b>) -> Result<(), DylibError<'a>> {
    if image.platform != Platform::MacosArm64 {
        return Err(DylibError::NotMac);
    }
    if identifier.len() != 64 || !identifier.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        return Err(DylibError::Identifier { len: identifier.len() });
    }
    if !image.tls.is_empty() {
        return Err(DylibError::TlsRelocs { count: image.tls.len() });
    }
    if image.file_start != PAGE {
        return Err(shape("the image's header room is not macOS's 0x4000 (Layout::MACOS_ARM64)", image.file_start));
    }
    let region = |r: Region| image.region(r).ok_or(DylibError::Shape { what: "a region the image does not list", at: 0 });
    let text = region(Region::Text)?;
    let unwind = region(Region::UnwindInfo)?;
    let got = region(Region::Got)?;
    let data = region(Region::Data)?;
    let thread_vars = region(Region::ThreadVars)?;
    if text.at < image.file_start {
        return Err(shape("Text starts inside the header room", text.at));
    }

    // The segments' bounds (image.rs: the writable segment starts on a new page).
    let init_end = image.file_start + image.bytes.len() as u64;
    let text_size = up(end_of(unwind), PAGE);
    if got.at != text_size {
        return Err(shape("the Got does not start the page after UnwindInfo", got.at));
    }
    let has_got = got.len > 0;
    let data_at = if has_got { up(end_of(got), PAGE) } else { got.at };
    if data.at != data_at {
        return Err(shape("Data does not start the page after the Got (wiring/blaze-native/native-W4-1.md)", data.at));
    }
    if init_end <= data_at || image.end < init_end {
        return Err(shape("the image's file bytes end outside its writable regions", init_end));
    }
    let data_file = up(init_end, PAGE) - data_at;
    let file_end = data_at + data_file;
    let vm_end = up(image.end, PAGE);

    let mut sects: AVec<'_, Sect> = Vec::new_in(alloc);
    let mut segs: AVec<'_, Seg> = Vec::new_in(alloc);
    let rx = macho::VM_PROT_READ | macho::VM_PROT_EXECUTE;
    let rw = macho::VM_PROT_READ | macho::VM_PROT_WRITE;
    add(&mut sects, "__text", "__TEXT", text, macho::S_CODE, true);
    add(&mut sects, "__const", "__TEXT", region(Region::Const)?, macho::S_REGULAR, false);
    add(&mut sects, "__eh_frame", "__TEXT", region(Region::EhFrame)?, macho::S_EH_FRAME, false);
    add(&mut sects, "__unwind_info", "__TEXT", unwind, macho::S_REGULAR, false);
    segs.push(Seg { name: "__TEXT", vm: 0, vm_size: text_size, file: 0, file_size: text_size, prot: rx, flags: 0, first: 0, count: sects.len(), fixups: false });
    if has_got {
        let first = sects.len();
        add(&mut sects, "__got", "__DATA_CONST", got, macho::S_REGULAR, false);
        let size = data_at - got.at;
        segs.push(Seg { name: "__DATA_CONST", vm: got.at, vm_size: size, file: got.at, file_size: size, prot: rw, flags: macho::SG_READ_ONLY, first, count: 1, fixups: true });
    }
    let first = sects.len();
    add(&mut sects, "__data", "__DATA", data, macho::S_REGULAR, true);
    add(&mut sects, "__blaze_words", "__DATA", region(Region::Words)?, macho::S_REGULAR, false);
    add(&mut sects, "__blaze_inits", "__DATA", region(Region::Inits)?, macho::S_REGULAR, false);
    add(&mut sects, "__thread_vars", "__DATA", thread_vars, S_THREAD_LOCAL_VARIABLES, false);
    add(&mut sects, "__thread_data", "__DATA", region(Region::ThreadData)?, S_THREAD_LOCAL_REGULAR, false);
    add(&mut sects, "__thread_bss", "__DATA", region(Region::ThreadZero)?, S_THREAD_LOCAL_ZEROFILL, false);
    add(&mut sects, "__bss", "__DATA", region(Region::Zero)?, macho::S_ZEROFILL, false);
    let count = sects.len() - first;
    segs.push(Seg { name: "__DATA", vm: data_at, vm_size: vm_end - data_at, file: data_at, file_size: data_file, prot: rw, flags: 0, first, count, fixups: true });

    // The fixups as chained slots; imports with an addend get a row of their own.
    let n_imports = image.imports.len();
    let mut extra: AVec<'_, (u32, i64)> = Vec::new_in(alloc);
    let mut last: Option<u64> = None;
    for f in image.fixups.iter() {
        if last.is_some_and(|l| f.at <= l) {
            return Err(shape("fixups not sorted by address, or two at one address", f.at));
        }
        last = Some(f.at);
        if !f.at.is_multiple_of(8) || f.at < got.at || f.at + 8 > init_end {
            return Err(shape("a fixup outside the writable file bytes, or not 8-aligned", f.at));
        }
        match f.to {
            FixTo::Rebase { .. } => {}
            FixTo::Import { index, addend } => {
                if index as usize >= n_imports {
                    return Err(shape("a fixup's import index past the image's imports", f.at));
                }
                if addend != 0 {
                    extra.push((index, addend));
                }
            }
            // The launcher's cell (the descriptor's pointer rows): no dynamic fixup.
            FixTo::Word { .. } => {}
        }
    }
    extra.sort_unstable();
    extra.dedup();
    let mut chains: AVec<'_, (u64, Chain)> = Vec::with_capacity_in(image.fixups.len(), alloc);
    for f in image.fixups.iter() {
        let to = match f.to {
            FixTo::Rebase { address, high8 } => {
                // The tag rides in `high8`; the address must fit the chain's 36 bits, refused
                // naming the slot and the pointer as the program holds it (rule 19).
                let too_far = DylibError::Slot { what: macho::REBASE_TOO_FAR, value: address.tagged(high8), at: f.at };
                Chain::Rebase { target: macho::Target36::new(address.get()).ok_or(too_far)?, high8 }
            }
            FixTo::Import { index, addend: 0 } => Chain::Bind { import: index },
            FixTo::Import { index, addend } => match extra.binary_search(&(index, addend)) {
                Ok(k) => Chain::Bind { import: (n_imports + k) as u32 },
                Err(_) => return Err(shape("an import addend the first pass did not list", f.at)),
            },
            // The launcher's cell: left out of every chain, so dyld never writes it.
            FixTo::Word { .. } => continue,
        };
        chains.push((f.at, to));
    }
    let mut chain_segs: AVec<'_, ChainSeg> = Vec::with_capacity_in(segs.len() + 1, alloc);
    for s in segs.iter() {
        chain_segs.push(ChainSeg { vm: s.vm, vm_size: s.vm_size, fixups: s.fixups });
    }
    chain_segs.push(ChainSeg { vm: vm_end, vm_size: 0, fixups: false }); // __LINKEDIT
    let import = |i: usize| -> ChainImport<'a> {
        let (index, addend) = match i.checked_sub(n_imports) {
            None => (i, 0),
            Some(k) => (extra[k].0 as usize, extra[k].1),
        };
        let row = image.imports[index];
        ChainImport { lead: b"", name: row.name, ordinal: FLAT_LOOKUP, weak: row.weak, addend }
    };
    let (blob, slots) = macho::chained_fixups(&chains, |c: &(u64, Chain)| *c, &chain_segs, ImportRows::Addend64, n_imports + extra.len(), import, alloc)
        .map_err(|e| match e.at {
            Some(at) => DylibError::Slot { what: e.what, value: e.len, at },
            None => DylibError::TooLarge { what: e.what, len: e.len },
        })?;

    // The exports: the trie dyld and dlsym search, and the symbol table tools read.
    let trie = export_trie(image, thread_vars, alloc)?;
    let mut nsect: AVec<'_, u8> = Vec::with_capacity_in(image.exports.len(), alloc);
    for &(_, at) in image.exports.iter() {
        let inside = sects.iter().position(|s| at >= s.at && at < s.at + s.len);
        let k = inside.or_else(|| sects.iter().position(|s| at == s.at + s.len)).ok_or(shape("an export outside every section", at))?;
        nsect.push(u8::try_from(k + 1).map_err(|_| shape("an export in a section past n_sect's 255", at))?);
    }

    // The libraries: libSystem first, then each distinct install name in the image's order.
    let mut paths: AVec<'_, AString<'b>> = Vec::new_in(alloc);
    let mut system: AString<'b> = String::new_in(alloc);
    system.push_str(macho::LIB_SYSTEM);
    paths.push(system);
    for library in image.libraries.iter() {
        if library.platform() != Platform::MacosArm64 {
            return Err(DylibError::Library { name: library.library().name });
        }
        let install = library.install();
        if !paths.iter().any(|p| p.as_str() == install) {
            let mut path: AString<'b> = String::new_in(alloc);
            path.push_str(install);
            paths.push(path);
        }
    }

    // `__LINKEDIT`, at offsets from its start.
    let mut link: AVec<'_, u8> = Vec::new_in(alloc);
    let fixups_at = 0u64;
    link.extend_from_slice(&blob);
    bytes::pad(&mut link, 8);
    let trie_at = link.len() as u64;
    link.extend_from_slice(&trie);
    bytes::pad(&mut link, 8);
    let starts_at = link.len() as u64;
    link.push(0); // function starts: none (the terminating zero)
    bytes::pad(&mut link, 8);
    let starts_len = link.len() as u64 - starts_at;
    let symtab_at = link.len() as u64;
    let mut strx = 2u64;
    for (k, &(name, at)) in image.exports.iter().enumerate() {
        if strx > u64::from(u32::MAX) {
            return Err(DylibError::TooLarge { what: "an export name's 32-bit string offset", len: strx });
        }
        // `nlist_64`: n_strx, n_type, n_sect, n_desc, n_value.
        bytes::u32(&mut link, strx as u32);
        bytes::u8(&mut link, N_SECT_EXT);
        bytes::u8(&mut link, nsect[k]);
        bytes::u16(&mut link, 0);
        bytes::u64(&mut link, at);
        strx += name.len() as u64 + 1;
    }
    let strtab_at = link.len() as u64;
    link.extend_from_slice(b" \0");
    for &(name, _) in image.exports.iter() {
        link.extend_from_slice(name);
        link.push(0);
    }
    bytes::pad(&mut link, 8);
    let strtab_len = link.len() as u64 - strtab_at;
    bytes::pad(&mut link, 16);
    let code_limit = file_end + link.len() as u64;
    let sig_len = sign::len(identifier, code_limit);
    if code_limit + sig_len > u64::from(u32::MAX) {
        return Err(DylibError::TooLarge { what: "the file past 32-bit load command offsets", len: code_limit + sig_len });
    }
    let link_len = link.len() as u64 + sig_len;
    segs.push(Seg { name: "__LINKEDIT", vm: vm_end, vm_size: up(link_len, PAGE), file: file_end, file_size: link_len, prot: macho::VM_PROT_READ, flags: 0, first: sects.len(), count: 0, fixups: false });

    // The load commands, built first as a list (commands.rs): `ncmds` and `sizeofcmds` are the
    // list's, never a hand count (one short once left LC_CODE_SIGNATURE unread: every image
    // refused at `dlopen` as "missing code signature"), and the room the image keeps before
    // `Text` (`Layout::header_reserve`) is checked against the list in every build.
    let mut cmds = Commands::new(alloc);
    for seg in segs.iter() {
        let segment = Segment { name: seg.name, vmaddr: seg.vm, vm_size: seg.vm_size, file: seg.file, file_size: seg.file_size, prot: seg.prot, flags: seg.flags };
        let sections = sects[seg.first..seg.first + seg.count].iter().map(|s| Section {
            name: s.name,
            seg: s.seg,
            addr: s.at,
            size: s.len,
            offset: if s.zero { 0 } else { s.at },
            align: s.align,
            flags: s.flags,
            reserved1: 0,
        });
        cmds.segment(segment, sections);
    }
    cmds.push(Command::Dylib { cmd: LC_ID_DYLIB, name: identifier, timestamp: 1, current: DYLIB_VERSION, compat: DYLIB_VERSION });
    for (k, path) in paths.iter().enumerate() {
        let (current, compat) = if k == 0 { (macho::LIB_SYSTEM_CURRENT, macho::LIB_SYSTEM_COMPAT) } else { (DYLIB_VERSION, DYLIB_VERSION) };
        cmds.push(Command::Dylib { cmd: macho::LC_LOAD_DYLIB, name: path.as_str(), timestamp: 2, current, compat });
    }
    cmds.push(Command::LinkeditData { cmd: macho::LC_DYLD_CHAINED_FIXUPS, off: file_end + fixups_at, size: blob.len() as u64 });
    cmds.push(Command::LinkeditData { cmd: LC_DYLD_EXPORTS_TRIE, off: file_end + trie_at, size: trie.len() as u64 });
    let n = image.exports.len() as u64;
    cmds.push(Command::Symtab { symoff: file_end + symtab_at, nsyms: n, stroff: file_end + strtab_at, strsize: strtab_len });
    // Every symbol an external definition, sorted by name: iextdefsym 0, nextdefsym n.
    cmds.push(Command::Dysymtab { fields: [0, 0, 0, n, n, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0] });
    cmds.push(Command::Uuid);
    cmds.push(Command::BuildVersion { platform: macho::PLATFORM_MACOS, minos: MIN_MACOS, sdk: 0 });
    cmds.push(Command::LinkeditData { cmd: macho::LC_FUNCTION_STARTS, off: file_end + starts_at, size: starts_len });
    cmds.push(Command::LinkeditData { cmd: macho::LC_CODE_SIGNATURE, off: code_limit, size: sig_len });
    let mut flags = MH_DYLDLINK | MH_TWOLEVEL | MH_NO_REEXPORTED_DYLIBS;
    if thread_vars.len > 0 {
        flags |= MH_HAS_TLV_DESCRIPTORS;
    }
    out.clear();
    out.try_reserve((code_limit + sig_len) as usize).map_err(|_| DylibError::Alloc)?;
    let uuid_at = cmds.write(out, Header { filetype: MH_DYLIB, flags }, text.at).map_err(commands_error)?.uuid_at;

    // The image's bytes at their addresses, the chained slots in them, then `__LINKEDIT`.
    out.resize(image.file_start as usize, 0);
    out.extend_from_slice(&image.bytes);
    out.resize(file_end as usize, 0);
    for &(at, value) in slots.iter() {
        bytes::set64(out, at as usize, value);
    }
    out.extend_from_slice(&link);

    // The identity of the content, then the signature over every byte before it.
    let mut hasher = Hasher::new(UUID_DOMAIN);
    hasher.update(&out[..]);
    let digest = hasher.finish().0;
    out[uuid_at..uuid_at + 16].copy_from_slice(&digest[..16]);
    sign::sign(out, identifier, text_size, false);
    Ok(())
}

fn uleb_len(mut value: u64) -> u64 {
    let mut n = 1;
    while value >= 0x80 {
        value >>= 7;
        n += 1;
    }
    n
}

/// One edge of the export trie: `label` (a piece of a name) leads to node `child`.
#[derive(Clone, Copy)]
struct Edge<'n> {
    label: &'n [u8],
    child: u32,
}

/// A node of the export trie: an export's (flags, address) if a name ends here, and its edges.
struct Node<'n, 'b> {
    terminal: Option<(u64, u64)>,
    edges: AVec<'b, Edge<'n>>,
}

/// The export trie (`<mach-o/loader.h>`, "dyld_info_command"'s export format, which
/// `LC_DYLD_EXPORTS_TRIE` carries): a radix tree over the exported names, each node
/// `terminal_size` (uleb), then `flags` and `address` (uleb) when a name ends there, a child
/// count (one byte), and per child its label (NUL-terminated) and the child's offset (uleb).
/// Built without recursion: names are inserted in the image's sorted order (each edge list stays
/// in name order), nodes are laid in preorder by an explicit stack, and offsets are found by
/// passes until no uleb grows.
fn export_trie<'a, 'b>(image: &Image<'a>, thread_vars: &Placed, alloc: Alloc<'b>) -> Result<AVec<'b, u8>, DylibError<'a>> {
    let mut nodes: AVec<'b, Node<'a, 'b>> = Vec::new_in(alloc);
    nodes.push(Node { terminal: None, edges: Vec::new_in(alloc) });
    let mut previous: Option<&[u8]> = None;
    for &(name, at) in image.exports.iter() {
        if name.is_empty() || name.contains(&0) {
            return Err(shape("an export name that is empty or holds a NUL", at));
        }
        if previous.is_some_and(|p| p >= name) {
            return Err(shape("exports not sorted by name, or one name twice", at));
        }
        previous = Some(name);
        let in_tlv = at >= thread_vars.at && at < end_of(thread_vars);
        let terminal = Some((if in_tlv { EXPORT_KIND_THREAD_LOCAL } else { EXPORT_KIND_REGULAR }, at));
        let mut node = 0usize;
        let mut rest: &'a [u8] = name;
        loop {
            let found = nodes[node].edges.iter().position(|e| e.label[0] == rest[0]);
            let Some(k) = found else {
                let leaf = nodes.len() as u32;
                nodes.push(Node { terminal, edges: Vec::new_in(alloc) });
                nodes[node].edges.push(Edge { label: rest, child: leaf });
                break;
            };
            let edge = nodes[node].edges[k];
            let common = edge.label.iter().zip(rest.iter()).take_while(|(a, b)| a == b).count();
            if common < edge.label.len() {
                // Split the edge at the common prefix: a new node takes the edge's tail.
                let mid = nodes.len() as u32;
                let mut edges: AVec<'b, Edge<'a>> = Vec::new_in(alloc);
                edges.push(Edge { label: &edge.label[common..], child: edge.child });
                nodes.push(Node { terminal: None, edges });
                nodes[node].edges[k] = Edge { label: &edge.label[..common], child: mid };
            }
            rest = &rest[common..];
            node = nodes[node].edges[k].child as usize;
            if rest.is_empty() {
                if nodes[node].terminal.is_some() {
                    return Err(shape("one export name twice", at));
                }
                nodes[node].terminal = terminal;
                break;
            }
        }
    }

    // Preorder, children in name order.
    let mut order: AVec<'b, u32> = Vec::with_capacity_in(nodes.len(), alloc);
    let mut stack: AVec<'b, u32> = Vec::new_in(alloc);
    stack.push(0);
    while let Some(n) = stack.pop() {
        order.push(n);
        for e in nodes[n as usize].edges.iter().rev() {
            stack.push(e.child);
        }
    }
    for node in nodes.iter() {
        if node.edges.len() > usize::from(u8::MAX) {
            return Err(DylibError::TooLarge { what: "an export trie node's one-byte child count", len: node.edges.len() as u64 });
        }
    }
    let info_len = |node: &Node<'_, '_>| node.terminal.map_or(0, |(flags, at)| uleb_len(flags) + uleb_len(at));
    let mut offset: AVec<'b, u64> = Vec::with_capacity_in(nodes.len(), alloc);
    offset.resize(nodes.len(), 0);
    let mut passes = 0;
    let total = loop {
        passes += 1;
        if passes > TRIE_PASSES {
            return Err(shape("the export trie's offsets did not settle", 0));
        }
        let mut at = 0u64;
        let mut changed = false;
        for &n in order.iter() {
            let node = &nodes[n as usize];
            if offset[n as usize] != at {
                offset[n as usize] = at;
                changed = true;
            }
            let info = info_len(node);
            at += uleb_len(info) + info + 1;
            for e in node.edges.iter() {
                at += e.label.len() as u64 + 1 + uleb_len(offset[e.child as usize]);
            }
        }
        if !changed {
            break at;
        }
    };

    let mut trie: AVec<'b, u8> = Vec::new_in(alloc);
    trie.try_reserve(total as usize).map_err(|_| DylibError::Alloc)?;
    for &n in order.iter() {
        let node = &nodes[n as usize];
        bytes::uleb(&mut trie, info_len(node));
        if let Some((flags, at)) = node.terminal {
            bytes::uleb(&mut trie, flags);
            bytes::uleb(&mut trie, at);
        }
        trie.push(node.edges.len() as u8);
        for e in node.edges.iter() {
            trie.extend_from_slice(e.label);
            trie.push(0);
            bytes::uleb(&mut trie, offset[e.child as usize]);
        }
    }
    if trie.len() as u64 != total {
        return Err(shape("the export trie's size changed between its measure and its write", total));
    }
    Ok(trie)
}
