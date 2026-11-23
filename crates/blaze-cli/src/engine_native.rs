//! The program's native image on the engine's path (run.md, "Run and test read the engine's
//! answer", D147, piece R2): [`image_for`], the record a start hands the parent, keyed by its
//! inputs and found by that key on a repeat. No stage and no task: the old `Links` and `Native`
//! drive kinds (native_image.rs) made the same record inside the old request; here it is made
//! from the engine's own records the first time a start lacks it.
//!
//! WHAT THE IMAGE IS MADE FROM, every one a record the engine's check already put:
//! - each crate of the program's link closure (native_image.rs [`native_image::closure`]: the
//!   program's crate first, each crate before every crate it depends on), read from the unit
//!   graph's crate list (`Standing::graph`);
//! - each such crate's LINK TABLE: the `links` section of its `CrateScope` record, the record the
//!   Names stage put (`Standing::names`, `NamesStage::input_of`), filed by its job from the names
//!   it resolved (blaze-ladder native_links.rs `scope_table`);
//! - each such crate's build script's RUN, its `Script` record (marked into the same names
//!   stage by the Scripts stage), opened as outputs (native_image.rs [`native_image::script_outputs`]).
//!
//! THE KEY ([`image_key`]), computed before any of the image's work (rule 65) and covering
//! every input of it (rule 21): the program's `Native` name (its root's path, the target, the
//! profile), the platform, the producer's id (D105), and for each closure crate in link order its
//! package, its `Links` name, the digest of its table's bytes (`table_digest`) and, when it has a
//! script, that run's name and out; on macOS the SDK's identity (blaze-apple-sdk `Sdk::identity`,
//! its version, never its folder), since each library's install name is read from that SDK's
//! stub. A body edit moves none of them, so a repeat start is one get.
//!
//! THE LIBRARIES' INSTALL NAMES (macOS), on a miss: each library the link line asks for
//! (native_links.rs `program_libraries`) is named by its SDK stub, `<SDK>/usr/lib/lib<N>.tbd`
//! or `<SDK>/System/Library/Frameworks/<N>.framework/<N>.tbd`, read through the file system the
//! command was handed ([`NativeAsk::fs`], rule 75) and parsed by blaze-native tbd.rs. A stub
//! that is missing or does not read is a typed refusal naming the library, the crate that asks
//! for it and the path asked ([`NativeError::Stub`]); nothing is guessed from the name (rule 3).
//! The install names read are in the image's own key (natives.rs `native_key`).
//! The record is a plain drive record (blaze-drive record.rs `write_plain_head`): name the
//! program's `Native` name, source the key's digest, out the image's own key
//! (natives.rs `native_key`), reads each crate's (`Links`, table digest) and (`Script`, out),
//! payload the BNL2 image, the same record the old `Native` task wrote.
//!
//! FLOOR (rule 67): a hit is one read of the unit graph's record, one of each closure crate's
//! names record and of its script's record, all in place from the store's mapping (nothing
//! copied), one hash over the tables, and one get. Nothing cheaper names what the image reads:
//! the `CrateScope` record's out moves on every names edit and is not the table's.
//!
//! A missing input is a typed error naming the crate ([`NativeError`]); a record at the key that
//! is not this program's image is refused by its key, never made again over (rule 3).

use core::fmt;

use blaze_drive::record::Stored;
use blaze_drive::{Input, Key, Name};
use blaze_id::CrateItemKey;
use blaze_ids::ContentId;
use blaze_ids::producer::{ProducerId, Whole};
use blaze_ladder::native_links::{self, Asked, LibraryKind, Linked, Platform, ProgramCrate, Source, Table, TableError, TbdError};
use blaze_own::{AVec, Vec};
use blaze_world::crate_list::CrateGraph;
use bstd::cx::Cx;
use bstd::mem::Frame;

use crate::CliError;
use crate::engine_check::Standing;
use crate::engine_seeds::CrateAt;
use crate::graph_payload::PayloadError;
use crate::native_image::{self, ScriptOpen};
use crate::store::Store;

/// The rule a refusal here names.
const RULE: &str = "engine_native.rs";

/// The domain of [`image_key`]. 3: the fields its doc lists, in that order, the SDK's identity
/// (its version and build, blaze-apple-sdk `SdkId::key`) last (2 held the version alone; 1 had
/// none, and named frameworks from a table).
const KEY_FORM: &str = "blaze-cli engine native image key 3";

/// The reserve of the image's scratch frame: the tables opened, the link line, the plan, the
/// layout and the image's bytes (a native script's archive is mapped, never copied here).
/// Virtual until touched; dropped when [`image_for`] returns.
const NATIVE_FRAME: usize = 4 << 30;

/// What the image needs beyond the check's records, handed down from the command's edge (R1 puts
/// it in `Standing::native`).
pub(crate) struct NativeAsk<'r> {
    /// The producer's id (D105): `facts.producer` on the old path, `producers.get::<Whole>()`.
    pub(crate) producer: ProducerId<Whole>,
    /// `HOME`, read at the edge: where a native script's stored archives are mapped from
    /// (scripts.rs `file_area`).
    pub(crate) home: Option<&'r str>,
    /// The command's frame, where a refusal's words are kept for the caller to print.
    pub(crate) words: &'r Frame,
    /// The file system the command was handed: the SDK's library stubs are read through it.
    pub(crate) fs: bstd::fs::Fs,
}

/// Why a library's SDK stub gave no install name.
#[derive(Debug)]
pub(crate) enum StubWhy<'w> {
    /// No file at the path asked: the SDK has no such library (a header-only framework such as
    /// `Kernel` has no stub).
    Missing,
    /// The folder or the file could not be opened or read (the file system's words).
    Unread { words: &'w str },
    /// The file is no text stub blaze-native tbd.rs reads, or names no macOS arm64 library.
    Form(TbdError),
}

impl fmt::Display for StubWhy<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StubWhy::Missing => write!(f, "is not there: the SDK has no such library"),
            StubWhy::Unread { words } => write!(f, "could not be read: {words}"),
            StubWhy::Form(why) => write!(f, "is refused: {why}"),
        }
    }
}

/// Why the program has no native image; each names the crate, record or key it is about (rule
/// 19) and this file as the rule.
#[derive(Debug)]
pub(crate) enum NativeError<'w> {
    /// The command's store refused a read or a put, or a record it holds does not open.
    Store(CliError),
    /// This build's target has no image form (blaze-native `Platform`).
    Platform { triple: &'static str },
    /// The scratch frame could not be reserved ([`NATIVE_FRAME`]).
    Frame,
    /// The closure handed in holds no crate, so it names no program.
    NoProgram,
    /// The unit graph's record is not a `UnitGraph` payload.
    Graph { name: Name, why: PayloadError },
    /// The unit graph made no world, so no crate has a row (its record's finding says why).
    NoWorld { name: Name },
    /// The closure's first crate, the program's, is not a row of the unit graph's crate list.
    NotListed { krate: CrateItemKey },
    /// The program's crate is not a linked crate of the checkout (a library of the macro side,
    /// a proc-macro crate, or a crate whose root is not under the checkout's `/0/`).
    NotAProgram { package: &'w str },
    /// A crate the program links that the closure handed in does not hold.
    NotHanded { package: &'w str },
    /// A crate the program links whose names failed: it has no `CrateScope` record.
    NoNames { package: &'w str },
    /// A crate whose `CrateScope` record holds no link table (`why`).
    NoTable { package: &'w str, why: &'static str },
    /// A crate's link table is not one of this form.
    TableForm { package: &'w str, why: TableError },
    /// A crate's table is its refusal (front2 links.rs): the program cannot link it.
    TableRefused { package: &'w str, words: &'w str },
    /// A crate list row names a build-script run of no kind (crate_list.rs `script`).
    ScriptKind { package: &'w str },
    /// A crate's build script has no run record (it failed, or was never run).
    NoScript { package: &'w str, script: Name },
    /// A crate's `Script` record is not a build script's outputs.
    Script { package: &'w str, script: Name, why: ScriptOpen },
    /// The link refused the program's crates (blaze-ladder natives.rs `NativeError`, its words).
    Link { what: &'static str, words: &'w str },
    /// macOS: the command has no SDK (its refusal's words), and every library's install name is
    /// its SDK stub's.
    Sdk { words: &'w str },
    /// macOS: `package` links `library` (`-l<N>` or `-framework <N>`), whose stub at `path` (the
    /// SDK's folder, then the stub's path under it) gave no install name.
    Stub { package: &'w str, library: &'w str, path: &'w str, why: StubWhy<'w> },
    /// A record at the image's key that is not this program's image.
    Damaged { key: Key, why: &'static str },
    /// `package`'s build script deferred C requests (its compiles were answered and kept), and
    /// they gave no built record: `words` say why (native_runner_c.rs `CTaskWhy`: a compile's
    /// first lines, an assumption that failed, a refusal).
    CTasks { package: &'w str, words: &'w str },
    /// The image could not be made ready for the program's start ([`send_of`]): its record
    /// did not read back, its descriptor or its Mach-O headers did not read, a word names no
    /// crate's export, or its file could not be put under the records folder.
    Send { words: &'w str },
}

/// What a program's start is handed of its native image (blaze-launch wire/run.rs
/// `Down::Native`'s rule): sent exactly when the image holds native code of its crates.
#[derive(Clone, Copy, Debug)]
pub(crate) struct NativeSend<'w> {
    /// The library's file in `<HOME>/` [`crate::run_parent::RECORDS`]: named by the image's
    /// key (which covers every input of its bytes, rule 21), so a found image is filed once and
    /// never hashed again; `BLAZE_LAUNCH_NATIVE` and `Down::Native.library` both name it.
    pub(crate) library: blaze_launch::wire::RecordId,
    /// `library`'s 64 hex digits, for the start's environment.
    pub(crate) hex: &'w str,
    /// In word order (the descriptor's names table), each word's Rust function by its fn
    /// instance symbol (blaze-ladder symbols.rs `fn_symbol` of the export's item, no type
    /// arguments: an exported fn is not generic). One symbol, one slot: a Rust caller of the
    /// same fn and C's word share its thunk, so the function has one address.
    pub(crate) words: &'w [blaze_launch::wire::run::Symbol],
    /// Each word's export by its library item, in word order: what the run's roots root
    /// (blaze-ladder run/special.rs `run_roots`'s `exports`), as no Rust record need call it.
    pub(crate) items: &'w [blaze_id::ItemKey],
}

/// The program's image as `blaze run` hands it on: its key (the start list is rooted beside
/// it) and what its start is sent, `None` for an image with no native code.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ProgramImage<'w> {
    pub(crate) key: Key,
    pub(crate) send: Option<NativeSend<'w>>,
}

/// The program's laid-out native image and Rust C exports for `blaze export`
/// (`engine_export.rs`, `blaze_export::ProgramIn::natives` and `exports`).
pub(crate) struct ExportNative<'a> {
    pub(crate) image: blaze_native::image::Image<'a>,
    /// Sorted by `name`, for `blaze_export::ProgramIn::exports`.
    pub(crate) exports: AVec<'a, blaze_export::CExport<'a>>,
    /// The library items of the Rust functions the native image's C words call, for `run_roots`.
    pub(crate) items: AVec<'a, blaze_id::ItemKey>,
}

/// The install name every image links and the launcher has already loaded.
const LIB_SYSTEM: &[u8] = b"/usr/lib/libSystem.B.dylib";
/// The base C library soname every Linux image links and the launcher has already loaded.
const LIBC_LINUX: &[u8] = b"libc.so.6";

/// Why an ELF shared object's headers could not be inspected by [`elf_holds_native`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ElfNativeRead {
    NotElf,
    Bounds(&'static str),
    BadPhEntSize,
    StrtabOutsideLoads,
}

impl fmt::Display for ElfNativeRead {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            ElfNativeRead::NotElf => f.write_str("the record is not a 64-bit little-endian ELF file"),
            ElfNativeRead::Bounds(what) => write!(f, "{what} runs past the ELF file's end"),
            ElfNativeRead::BadPhEntSize => f.write_str("unexpected e_phentsize in ELF header"),
            ElfNativeRead::StrtabOutsideLoads => f.write_str("DT_STRTAB lies outside every PT_LOAD"),
        }
    }
}

/// Whether the arm64 ELF shared object `file` holds native code of the program's crates: a byte
/// of `.text`, or a `DT_NEEDED` library besides `libc.so.6` (which the launcher already has).
fn elf_holds_native(file: &[u8]) -> Result<bool, ElfNativeRead> {
    let u16_at = |at: usize| file.get(at..at + 2).and_then(|b| b.try_into().ok()).map(u16::from_le_bytes).ok_or(ElfNativeRead::Bounds("an ELF header field"));
    let u32_at = |at: usize| file.get(at..at + 4).and_then(|b| b.try_into().ok()).map(u32::from_le_bytes).ok_or(ElfNativeRead::Bounds("an ELF field"));
    let u64_at = |at: usize| file.get(at..at + 8).and_then(|b| b.try_into().ok()).map(u64::from_le_bytes).ok_or(ElfNativeRead::Bounds("an ELF field"));
    if file.get(0..6) != Some(&[0x7F, b'E', b'L', b'F', 2, 1][..]) {
        return Err(ElfNativeRead::NotElf);
    }
    // Check `.text` section size via section headers (native_so.rs always emits them).
    let shoff = usize::try_from(u64_at(40)?).map_err(|_| ElfNativeRead::Bounds("e_shoff"))?;
    let shentsize = usize::from(u16_at(58)?);
    let shnum = usize::from(u16_at(60)?);
    let shstrndx = usize::from(u16_at(62)?);
    if shoff > 0 && shnum > 0 && shentsize == 64 && shstrndx < shnum {
        let shstr_hdr = shoff + shstrndx * 64;
        let shstr_off = usize::try_from(u64_at(shstr_hdr + 24)?).map_err(|_| ElfNativeRead::Bounds(".shstrtab offset"))?;
        let shstr_len = usize::try_from(u64_at(shstr_hdr + 32)?).map_err(|_| ElfNativeRead::Bounds(".shstrtab size"))?;
        let shstr = file.get(shstr_off..shstr_off.saturating_add(shstr_len)).ok_or(ElfNativeRead::Bounds(".shstrtab"))?;
        for k in 0..shnum {
            let s = shoff + k * 64;
            let name_off = usize::try_from(u32_at(s)?).map_err(|_| ElfNativeRead::Bounds("sh_name"))?;
            if let Some(rest) = shstr.get(name_off..) {
                let name = rest.split(|b| *b == 0).next().unwrap_or(rest);
                if name == b".text" && u64_at(s + 32)? > 0 {
                    return Ok(true);
                }
            }
        }
    }
    // Check `DT_NEEDED` entries via `PT_DYNAMIC` and `PT_LOAD`.
    let phoff = usize::try_from(u64_at(32)?).map_err(|_| ElfNativeRead::Bounds("e_phoff"))?;
    let phentsize = usize::from(u16_at(54)?);
    let phnum = usize::from(u16_at(56)?);
    if phentsize != 56 {
        return Err(ElfNativeRead::BadPhEntSize);
    }
    let mut dynamic: Option<(usize, usize)> = None;
    for k in 0..phnum {
        let p = phoff + k * 56;
        if u32_at(p)? == 2 {
            let off = usize::try_from(u64_at(p + 8)?).map_err(|_| ElfNativeRead::Bounds("PT_DYNAMIC offset"))?;
            let len = usize::try_from(u64_at(p + 32)?).map_err(|_| ElfNativeRead::Bounds("PT_DYNAMIC size"))?;
            dynamic = Some((off, len));
            break;
        }
    }
    let Some((dyn_off, dyn_len)) = dynamic else { return Ok(false) };
    let (mut strtab_va, mut strsz) = (None, None);
    let mut off = 0usize;
    while off + 16 <= dyn_len {
        let tag = u64_at(dyn_off + off)?;
        let val = u64_at(dyn_off + off + 8)?;
        match tag {
            0 => break,
            5 => strtab_va = Some(val),
            10 => strsz = Some(val),
            _ => {}
        }
        off += 16;
    }
    let (Some(strtab_va), Some(strsz)) = (strtab_va, strsz) else { return Ok(false) };
    let mut strtab_off = None;
    for k in 0..phnum {
        let p = phoff + k * 56;
        if u32_at(p)? == 1 {
            let p_off = u64_at(p + 8)?;
            let p_vaddr = u64_at(p + 16)?;
            let p_filesz = u64_at(p + 32)?;
            if strtab_va >= p_vaddr && strtab_va.saturating_add(strsz) <= p_vaddr.saturating_add(p_filesz) {
                strtab_off = Some(usize::try_from(p_off + (strtab_va - p_vaddr)).map_err(|_| ElfNativeRead::Bounds("DT_STRTAB offset"))?);
                break;
            }
        }
    }
    let Some(strtab_off) = strtab_off else { return Err(ElfNativeRead::StrtabOutsideLoads) };
    let strsz_usize = usize::try_from(strsz).map_err(|_| ElfNativeRead::Bounds("DT_STRSZ"))?;
    let strtab = file.get(strtab_off..strtab_off.saturating_add(strsz_usize)).ok_or(ElfNativeRead::Bounds("DT_STRTAB"))?;
    let mut off = 0usize;
    while off + 16 <= dyn_len {
        let tag = u64_at(dyn_off + off)?;
        let val = u64_at(dyn_off + off + 8)?;
        if tag == 0 {
            break;
        }
        if tag == 1 {
            let name_off = usize::try_from(val).map_err(|_| ElfNativeRead::Bounds("DT_NEEDED offset"))?;
            let rest = strtab.get(name_off..).ok_or(ElfNativeRead::Bounds("DT_NEEDED"))?;
            let name = rest.split(|b| *b == 0).next().unwrap_or(rest);
            if name != LIBC_LINUX {
                return Ok(true);
            }
        }
        off += 16;
    }
    Ok(false)
}

/// Whether the arm64 Mach-O image `file` holds native code of the program's crates: a byte of
/// `__TEXT,__text`, or a library besides libSystem (which the launcher has). The words and the
/// initializers live in that text, so an image with neither holds nothing a start would load.
fn macho_holds_native(file: &[u8]) -> Result<bool, &'static str> {
    let u32_at = |at: usize| file.get(at..at + 4).and_then(|b| b.try_into().ok()).map(u32::from_le_bytes).ok_or("a load command past the file's end");
    let u64_at = |at: usize| file.get(at..at + 8).and_then(|b| b.try_into().ok()).map(u64::from_le_bytes).ok_or("a section past the file's end");
    if u32_at(0)? != 0xfeed_facf {
        return Err("the record is not a 64-bit Mach-O file");
    }
    let ncmds = u32_at(16)?;
    let mut at = 32usize;
    for _ in 0..ncmds {
        let (cmd, size) = (u32_at(at)?, u32_at(at + 4)? as usize);
        match cmd {
            // LC_SEGMENT_64: its sections, 80 bytes each from +72 (`<mach-o/loader.h>`).
            0x19 => {
                let nsects = u32_at(at + 64)? as usize;
                for k in 0..nsects {
                    let s = at + 72 + k * 80;
                    let names = file.get(s..s + 32).ok_or("a section past the file's end")?;
                    if names.starts_with(b"__text\0") && names[16..].starts_with(b"__TEXT\0") && u64_at(s + 40)? > 0 {
                        return Ok(true);
                    }
                }
            }
            // LC_LOAD_DYLIB, LC_LOAD_WEAK_DYLIB: the name from `name.offset`.
            0xc | 0x8000_0018 => {
                let name_at = at + u32_at(at + 8)? as usize;
                let end = file.get(name_at..at + size).ok_or("a library name past its command")?;
                let name = end.split(|b| *b == 0).next().unwrap_or(end);
                if name != LIB_SYSTEM {
                    return Ok(true);
                }
            }
            _ => {}
        }
        if size < 8 {
            return Err("a load command shorter than its header");
        }
        at += size;
    }
    Ok(false)
}

/// What the start of the program whose image is `payload` is sent ([`NativeSend`]), `None`
/// when the image holds no native code: on macOS by its Mach-O headers
/// ([`macho_holds_native`]); on Linux by its descriptor and ELF headers ([`elf_holds_native`]).
/// The file is put at `<HOME>/` [`crate::run_parent::RECORDS`]`/<the key's hex>` when it is not
/// there.
fn send_of<'w>(cx: &Cx, key: Key, payload: &[u8], crates: &[CrateIn<'_>], platform: Platform, ask: &NativeAsk<'w>, scratch: &Frame) -> Result<Option<NativeSend<'w>>, NativeError<'w>> {
    let refused = |words: &str| NativeError::Send { words: held(words, ask) };
    let (descriptor, names) = blaze_native::descriptor::read(payload, platform).map_err(|why| refused(bstd::mem::format_in!(scratch, "its descriptor: {why:?}").as_str()))?;
    let described = descriptor.words > 0 || descriptor.inits > 0 || descriptor.pointers > 0;
    let holds = match platform {
        Platform::MacosArm64 => described || macho_holds_native(payload).map_err(refused)?,
        Platform::LinuxArm64 => described || elf_holds_native(payload).map_err(|why| refused(bstd::mem::format_in!(scratch, "{why}").as_str()))?,
    };
    if !holds {
        return Ok(None);
    }
    // Each word's Rust function: the export of that name among the crates' tables.
    let erased = blaze_own::Erased::new(scratch);
    let alloc = erased.handle();
    // Every crate's exports, each by its platform spelling (the names table's), read once.
    let mut exports: Vec<(AVec<'_, u8>, blaze_id::ItemKey), &Frame> = Vec::new_in(scratch);
    for c in crates.iter() {
        let links = match native_links::open_table(c.table, alloc) {
            Ok(Table::Links(links)) => links,
            Ok(Table::Refused(words)) => return Err(NativeError::TableRefused { package: held(c.package, ask), words: held(words, ask) }),
            Err(why) => return Err(NativeError::TableForm { package: held(c.package, ask), why }),
        };
        for export in links.exports.iter() {
            let mangled = blaze_native::resolve::mangle(platform, export.name, alloc).map_err(|why| refused(bstd::mem::format_in!(scratch, "the export `{}`: {why:?}", export.name).as_str()))?;
            exports.push((mangled, export.item));
        }
    }
    let mut words: Vec<blaze_launch::wire::run::Symbol, &'w Frame> = Vec::with_capacity_in(names.len(), ask.words);
    let mut items: Vec<blaze_id::ItemKey, &'w Frame> = Vec::with_capacity_in(names.len(), ask.words);
    for i in 0..names.len() {
        let Some(name) = names.get(i) else { return Err(refused("a word's name is past its table")) };
        let found = exports.iter().find(|(spelled, _)| spelled.as_slice() == name).map(|(_, item)| *item);
        let Some(item) = found else {
            return Err(refused(bstd::mem::format_in!(scratch, "word {i} `{}` is no crate's export", core::str::from_utf8(name).unwrap_or("(not UTF-8)")).as_str()));
        };
        let checker = blaze_ladder::front2::keys::checker_of(item);
        let symbol = blaze_ladder::symbols::fn_symbol(checker, blaze_typeck::List::EMPTY, blaze_typeck::PTys::EMPTY, alloc)
            .map_err(|why| refused(bstd::mem::format_in!(scratch, "word {i}'s symbol: {why:?}").as_str()))?;
        words.push(blaze_launch::wire::run::Symbol(symbol.0));
        items.push(item);
    }
    // The file the child opens, under the command's own home (a folder only its user writes).
    let Some(home) = ask.home else { return Err(refused("HOME is not set, so the records folder has no place")) };
    let records = bstd::path::Path::new(crate::run_parent::RECORDS).map_err(|why| refused(bstd::mem::format_in!(scratch, "{why}").as_str()))?;
    let dir = ask.fs.home(cx, home, &records).map_err(|why| refused(bstd::mem::format_in!(scratch, "the records folder: {why}").as_str()))?;
    let hex = bstd::mem::format_in!(scratch, "{}", Hex(&key.0.0));
    let name = bstd::path::Path::new(hex.as_str()).map_err(|why| refused(bstd::mem::format_in!(scratch, "{why}").as_str()))?;
    if dir.stat(cx, &name).map_err(|why| refused(bstd::mem::format_in!(scratch, "{why}").as_str()))?.is_none() {
        dir.publish(cx, &name, bstd::fs::Content::bytes(payload), bstd::fs::Durable::No).map_err(|why| refused(bstd::mem::format_in!(scratch, "the library's file: {why}").as_str()))?;
    }
    Ok(Some(NativeSend { library: blaze_launch::wire::RecordId(key.0.0), hex: held(hex.as_str(), ask), words: words.into_bump_slice(), items: items.into_bump_slice() }))
}

/// 32 bytes as 64 lowercase hex digits.
struct Hex<'b>(&'b [u8; 32]);

impl fmt::Display for Hex<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

impl fmt::Display for NativeError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NativeError::Store(error) => write!(f, "the program's native image: {error} ({RULE})"),
            NativeError::Platform { triple } => write!(f, "no native image is made for the target `{triple}`: only arm64 macOS and arm64 Linux (blaze-native `Platform`, {RULE})"),
            NativeError::Frame => write!(f, "the native image's frame of {NATIVE_FRAME} bytes could not be reserved ({RULE} `NATIVE_FRAME`)"),
            NativeError::NoProgram => write!(f, "the native image was asked for no crate: the closure handed in is empty ({RULE} `image_for`)"),
            NativeError::Graph { name, why } => write!(f, "the unit graph {name}, read for the program's crates: {why} ({RULE})"),
            NativeError::NoWorld { name } => write!(f, "the unit graph {name} made no world, so the program has no crates (the graph's record says why; {RULE})"),
            NativeError::NotListed { krate } => write!(f, "the program's crate {krate:?} is not a row of the unit graph's crate list ({RULE})"),
            NativeError::NotAProgram { package } => write!(f, "`{package}` is not a program of the checkout: no linked crate rooted under it ({RULE})"),
            NativeError::NotHanded { package } => write!(f, "the program links `{package}`, which the closure handed in does not hold ({RULE} `image_for`)"),
            NativeError::NoNames { package } => write!(f, "the program links `{package}`, whose names failed: it has no `CrateScope` record, so no link table ({RULE})"),
            NativeError::NoTable { package, why } => write!(f, "the `CrateScope` record of `{package}` holds no link table: {why} ({RULE})"),
            NativeError::TableForm { package, why } => write!(f, "the link table of `{package}` does not open: {why:?} (blaze-ladder native_links.rs `open_table`, {RULE})"),
            NativeError::TableRefused { package, words } => write!(f, "the link table of `{package}` was refused: {words} ({RULE})"),
            NativeError::ScriptKind { package } => write!(f, "the crate list row of `{package}` names a build-script run of no kind (crate_list.rs `script`, {RULE})"),
            NativeError::NoScript { package, script } => write!(f, "the build script of `{package}` has no run record ({script}): it failed or was not run ({RULE})"),
            NativeError::Script { package, script, why } => write!(f, "the build script run {script} of `{package}` is not a script's outputs: {why:?} (native_image.rs `script_outputs`, {RULE})"),
            NativeError::Link { what, words } => write!(f, "{what}: {words} ({RULE})"),
            NativeError::Sdk { words } => write!(f, "the program's native image names each library by the macOS SDK's stub, and {words} ({RULE})"),
            NativeError::Stub { package, library, path, why } => {
                write!(f, "`{package}` links {library}, whose SDK stub {path} {why}; its install name is read from that stub, never made from the name (blaze-native libraries.rs, rule 3; {RULE})")
            }
            NativeError::Damaged { key, why } => write!(f, "the record at the native image's key {key:?} is {why} ({RULE})"),
            NativeError::CTasks { package, words } => write!(f, "the build script of `{package}` deferred its C compiles to the program's image: {words} (native_runner_c.rs, {RULE})"),
            NativeError::Send { words } => write!(f, "the program's native image could not be handed to its start: {words} ({RULE} `send_of`, blaze-launch wire/run.rs `Down::Native`)"),
        }
    }
}

/// What [`image_in`] did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Image {
    /// Where the record is: the key a start hands the parent (`Down::Native`).
    pub(crate) key: Key,
    /// Made now and put (`false`: found by its key).
    pub(crate) made: bool,
    /// The image's bytes (the record's payload).
    pub(crate) bytes: usize,
}

/// One crate of the program as the image takes it, in link order.
#[derive(Clone, Copy)]
pub(crate) struct CrateIn<'a> {
    /// `name@version`, as messages and the image name it.
    pub(crate) package: &'a str,
    /// Its `Links` name (`Name::links_of` its `CrateScope`): what the record's reads name.
    pub(crate) links: Name,
    /// Its link table's bytes (native_links.rs form), in place.
    pub(crate) table: &'a [u8],
    pub(crate) script: Option<ScriptIn<'a>>,
}

/// A crate's build-script run as the image takes it.
#[derive(Clone, Copy)]
pub(crate) struct ScriptIn<'a> {
    pub(crate) name: Name,
    /// The run record's out: the outputs' id the link line names.
    pub(crate) out: ContentId,
    /// The record's payload (scripts.rs `payload_parts` form), in place.
    pub(crate) payload: &'a [u8],
}

/// The image's platform as one byte of the key.
fn platform_code(platform: Platform) -> u8 {
    match platform {
        Platform::MacosArm64 => 0,
        Platform::LinuxArm64 => 1,
    }
}

/// The store key of the image of `program` linking `crates` (the module's THE KEY): from the
/// inputs alone, before the work. `sdk`: the macOS SDK's identity (`None` on Linux), whose
/// stubs state the libraries' install names.
pub(crate) fn image_key(program: Name, platform: Platform, producer: &ProducerId<Whole>, crates: &[CrateIn<'_>], sdk: Option<blaze_apple_sdk::SdkId>) -> Key {
    let mut h = blaze_ids::Hasher::new(KEY_FORM);
    h.update(&[program.kind as u8]).update(&program.subject.0);
    h.update(&[platform_code(platform)]).update(&producer.id().0);
    h.update(&(crates.len() as u64).to_le_bytes());
    for c in crates.iter() {
        h.update(&(c.package.len() as u64).to_le_bytes()).update(c.package.as_bytes());
        h.update(&[c.links.kind as u8]).update(&c.links.subject.0).update(&native_links::table_digest(c.table).0);
        match c.script {
            None => {
                h.update(&[0]);
            }
            Some(script) => {
                h.update(&[1, script.name.kind as u8]).update(&script.name.subject.0).update(&script.out.0);
            }
        }
    }
    match sdk {
        None => {
            h.update(&[0]);
        }
        Some(id) => {
            h.update(&[1]).update(&id.key());
        }
    }
    Key(h.finish())
}

/// The store as the image reads and writes it (rule 78: one seam; the command's store, and the
/// tests' model).
pub(crate) trait ImageStore<'m> {
    /// The record at `key`, in place; `None` when there is none.
    fn get(&mut self, cx: &Cx, key: Key) -> Result<Option<&'m [u8]>, CliError>;
    /// The record whose bytes are the concatenation of `parts`, at `key`.
    fn put_parts(&mut self, cx: &Cx, key: Key, parts: &[&[u8]]) -> Result<(), CliError>;
}

/// The command's store as an [`ImageStore`].
pub(crate) struct OnStore<'s, 'm>(pub(crate) &'s mut dyn Store<'m>);

impl<'m> ImageStore<'m> for OnStore<'_, 'm> {
    fn get(&mut self, cx: &Cx, key: Key) -> Result<Option<&'m [u8]>, CliError> {
        Ok(self.0.get(cx, key)?.map(|held| held.bytes))
    }

    fn put_parts(&mut self, cx: &Cx, key: Key, parts: &[&[u8]]) -> Result<(), CliError> {
        self.0.put_parts(cx, key, parts)
    }
}

/// `text` kept in the command's frame for the error that names it.
fn held<'w>(text: &str, ask: &NativeAsk<'w>) -> &'w str {
    crate::work::held_str(text, ask.words)
}

struct PreparedCrates<'r, 's> {
    platform: Platform,
    sdk: Option<blaze_apple_sdk::Sdk<'r>>,
    program: Name,
    crates: Vec<CrateIn<'s>, &'s Frame>,
}

/// Gathers the program's link-closure crates in link order, runs any deferred C tasks on macOS,
/// and reads each crate's link table into `scratch`. Shared by [`image_for`] (`run`/`test`) and
/// [`export_native_for`] (`export`).
fn prepare_crates<'r, 'm: 's, 's>(
    cx: &Cx,
    standing: &mut Standing<'r, 'm>,
    closure: &[CrateAt],
    watch: &dyn blaze_screen::Watch,
    scratch: &'s Frame,
) -> Result<PreparedCrates<'r, 's>, NativeError<'r>> {
    let Some(platform) = native_image::platform() else {
        return Err(NativeError::Platform { triple: crate::graph::TRIPLE });
    };
    let Some(first) = closure.first() else { return Err(NativeError::NoProgram) };
    // Disjoint fields: the ask and the readers are shared, the store is the one mutable borrow.
    let (ask, names, graph) = (&standing.native, standing.names, standing.graph);
    // macOS: each library's install name is the SDK's stub's, so the image needs the SDK.
    let sdk = match (platform, standing.place.sdk) {
        (Platform::MacosArm64, Ok(sdk)) => Some(sdk),
        (Platform::MacosArm64, Err(why)) => return Err(NativeError::Sdk { words: held(bstd::mem::format_in!(scratch, "{why}").as_str(), ask) }),
        (Platform::LinuxArm64, _) => None,
    };
    let store: &mut dyn Store<'m> = &mut *standing.store;
    let err = standing.err;
    // A crate whose script deferred C requests: run here, before the link (native_runner_c.rs;
    // macOS only, where the pinned clang and the SDK are; elsewhere the link refuses the record
    // by name, natives.rs `DeferredC`).
    let mut c_tasks = sdk.map(|sdk| crate::native_runner_c::CTasks::new(ask.home, sdk, &ask.fs, standing.workers.get(), standing.heap));

    // The crate list, in place from the unit graph's record.
    let payload = native_image::payload_in_place(cx, store, &graph).map_err(NativeError::Store)?;
    let sections = crate::graph_payload::sections(payload).map_err(|why| NativeError::Graph { name: graph.name, why })?;
    let Some(list) = sections.crates().map_err(|why| NativeError::Graph { name: graph.name, why })? else {
        return Err(NativeError::NoWorld { name: graph.name });
    };
    let Some(root) = (0..list.len()).find(|r| list.key(*r) == first.key) else {
        return Err(NativeError::NotListed { krate: first.key });
    };
    let program = match list.root(root).strip_prefix("/0/") {
        Some(path) if native_image::linked(&list, root) => native_image::program_name(path),
        _ => return Err(NativeError::NotAProgram { package: held(list.name(root), ask) }),
    };

    // The crates handed in, by key, to check each crate the walk reaches against.
    let mut handed: Vec<[u8; 32], &Frame> = Vec::with_capacity_in(closure.len(), scratch);
    handed.extend(closure.iter().map(|c| c.key.0.0));
    handed.sort_unstable();

    let order = native_image::closure(&list, root, &handed, scratch);
    // Each crate's script as it ran (the deferred record, when its C was deferred), read in
    // place before any crate's C runs: a reader's `OUT_DIR` spellings are found among them
    // (native_runner_c.rs `dep_out`).
    let mut ran: Vec<(usize, ScriptIn<'m>), &'s Frame> = Vec::with_capacity_in(order.len(), scratch);
    for r in order.iter().copied() {
        let package = list.name(r);
        if handed.binary_search(&list.key(r).0.0).is_err() {
            return Err(NativeError::NotHanded { package: held(package, ask) });
        }
        let Some(bytes) = list.script(r) else { continue };
        let Some(name) = native_image::script_name(bytes) else { return Err(NativeError::ScriptKind { package: held(package, ask) }) };
        let Some(run) = names(name) else { return Err(NativeError::NoScript { package: held(package, ask), script: name }) };
        let payload = native_image::payload_in_place(cx, store, &run).map_err(NativeError::Store)?;
        ran.push((r, ScriptIn { name, out: run.out, payload }));
    }
    let mut done: Vec<(usize, ScriptIn<'s>), &'s Frame> = Vec::with_capacity_in(ran.len(), scratch);
    let mut refused: Vec<usize, &Frame> = Vec::new_in(scratch);
    let deferred_total: u32 = if c_tasks.is_some() {
        let count = ran
            .iter()
            .filter(|(_, script)| {
                crate::scripts::payload_parts(script.payload)
                    .and_then(|(_, record)| blaze_build_script::outputs::OutputsRef::open(record).ok())
                    .is_some_and(|o| o.c_requests().next().is_some())
            })
            .count();
        u32::try_from(count).unwrap_or(u32::MAX)
    } else {
        0
    };
    let mut deferred_done = 0u32;
    for r in order.iter().rev().copied() {
        let Some(script) = ran.iter().find(|(at, _)| *at == r).map(|(_, script)| *script) else { continue };
        let script = match c_tasks.as_mut() {
            None => script,
            Some(c) => {
                let package = list.name(r);
                let defers = crate::scripts::payload_parts(script.payload).and_then(|(_, record)| blaze_build_script::outputs::OutputsRef::open(record).ok()).is_some_and(|o| o.c_requests().next().is_some());
                if defers && let Some(dep) = list.deps(r).find(|d| refused.contains(d)) {
                    watch.see(blaze_screen::Step::Says);
                    err.line(cx, format_args!("the C of {package}: not built, as the C of {} it depends on was refused (said above); a call into either stops naming its C name ({RULE} `image_for`)", list.name(dep)))
                        .map_err(|e| NativeError::Store(CliError::Out(e)))?;
                    refused.push(r);
                    deferred_done = deferred_done.saturating_add(1);
                    continue;
                }
                if defers {
                    use core::fmt::Write as _;
                    let mut label = blaze_own::FixedString::<{ blaze_screen::ITEM_LABEL_BYTES }>::new();
                    let _ = write!(label, "{package} · cc/c++");
                    let items = [blaze_screen::ActiveItemRef { label: label.as_str(), elapsed_ms: 0 }];
                    let remaining = deferred_total.saturating_sub(deferred_done).saturating_sub(1);
                    let groups = [blaze_screen::ActiveGroupRef {
                        kind: blaze_screen::WorkKind::BuildScript,
                        items: &items,
                        extra: u16::try_from(remaining).unwrap_or(u16::MAX),
                    }];
                    watch.see(blaze_screen::Step::Activity(blaze_screen::Activity {
                        verb: "building",
                        progress: (deferred_total > 0).then_some((deferred_done, deferred_total)),
                        active: 1,
                        hint: None,
                        groups: &groups,
                    }));
                }
                let producers = crate::native_runner_c::Producers { scripts: &ran, done: &done, refused: &refused, graph: sections.graph };
                let built = crate::native_runner_c::built_for(cx, c, store, &list, r, package, script, &producers, scratch);
                if defers {
                    deferred_done = deferred_done.saturating_add(1);
                }
                match built {
                    Ok((script, report)) => {
                        if let Some(report) = report {
                            watch.see(blaze_screen::Step::Says);
                            err.line(cx, format_args!("{}", crate::native_runner_c::Said { package, report: &report })).map_err(|e| NativeError::Store(CliError::Out(e)))?;
                        }
                        script
                    }
                    Err(why) => {
                        watch.see(blaze_screen::Step::Says);
                        let refusal = NativeError::CTasks { package: held(package, ask), words: held(bstd::mem::format_in!(scratch, "{why}").as_str(), ask) };
                        err.line(cx, format_args!("{refusal}; its archives are left out of this image, and only code that calls one of its C names stops, naming it"))
                            .map_err(|e| NativeError::Store(CliError::Out(e)))?;
                        refused.push(r);
                        continue;
                    }
                }
            }
        };
        done.push((r, script));
    }
    let mut crates: Vec<CrateIn<'s>, &'s Frame> = Vec::with_capacity_in(order.len(), scratch);
    for r in order.iter().copied() {
        let (package, key) = (list.name(r), list.key(r));
        let scope = blaze_drive::key::crate_scope_name(&key);
        let Some(scope_in) = names(scope) else { return Err(NativeError::NoNames { package: held(package, ask) }) };
        let table = table_of(cx, store, &scope_in, package, ask)?;
        let script = done.iter().find(|(at, _)| *at == r).map(|(_, script)| *script);
        crates.push(CrateIn { package, links: Name::links_of(scope), table, script });
    }
    {
        let root_pkg = list.name(root);
        let items = [blaze_screen::ActiveItemRef { label: root_pkg, elapsed_ms: 0 }];
        let groups = [blaze_screen::ActiveGroupRef {
            kind: blaze_screen::WorkKind::Blaze,
            items: &items,
            extra: 0,
        }];
        watch.see(blaze_screen::Step::Activity(blaze_screen::Activity {
            verb: "linking",
            progress: None,
            active: 1,
            hint: None,
            groups: &groups,
        }));
    }
    Ok(PreparedCrates { platform, sdk, program, crates })
}

/// THE CONTRACT OF E3: the native image of the program whose crates are `closure`, found by its
/// key ([`image_key`]), else made now from each crate's link table and its build script's
/// record and put. A program with no C input has a real image that links the system library.
///
/// `closure[0]` is the program's crate; the rest is the set of crates it may link, in any order
/// (engine_seeds.rs `crates_of` with the program's first will do). The link order is the unit
/// graph's (native_image.rs `closure`), never the caller's, and a crate the program links that is
/// not in `closure` is refused by name ([`NativeError::NotHanded`]). Each crate's records are
/// read through `Standing::names`, the one reader of the check's records (rule 30).
pub(crate) fn image_for<'r>(
    cx: &Cx,
    standing: &mut Standing<'r, '_>,
    closure: &[CrateAt],
    watch: &dyn blaze_screen::Watch,
) -> Result<ProgramImage<'r>, NativeError<'r>> {
    let cx = cx.span("native: the program's image by key");
    let scratch = Frame::with_reserve(NATIVE_FRAME, bstd::mem::Tag::Other).map_err(|_| NativeError::Frame)?;
    let prepared = prepare_crates(&cx, standing, closure, watch, &scratch)?;
    let ask = &standing.native;
    let store: &mut dyn Store<'_> = &mut *standing.store;
    let image = image_in(&cx, &mut OnStore(store), prepared.program, &prepared.crates, prepared.platform, prepared.sdk, ask, &scratch)?;
    // Read back in place (found or just put: the store lends its own puts), for its start.
    let gone = || NativeError::Send { words: "the image's record does not read back from the store just after `image_in`" };
    let bytes = OnStore(store).get(&cx, image.key).map_err(NativeError::Store)?.ok_or_else(gone)?;
    let Ok(Stored::Whole(view)) = blaze_drive::record::open(bytes) else { return Err(NativeError::Damaged { key: image.key, why: "it does not open as the whole record `image_in` checked" }) };
    let send = send_of(&cx, image.key, view.payload, &prepared.crates, prepared.platform, ask, &scratch)?;
    Ok(ProgramImage { key: image.key, send })
}

/// The laid-out native image (`blaze_native::image::Image`) and sorted Rust C exports for
/// `blaze export`, allocated in `out_bump`. Returns `Ok(None)` when the program links no native
/// objects, initializers, Rust words, or libraries beyond libSystem/libc.
pub(crate) fn export_native_for<'r, 'm: 's, 's>(
    cx: &Cx,
    standing: &mut Standing<'r, 'm>,
    closure: &[CrateAt],
    watch: &dyn blaze_screen::Watch,
    out_frame: &'s Frame,
    out_bump: blaze_own::Bump<'s>,
) -> Result<Option<ExportNative<'s>>, NativeError<'r>> {
    let cx = cx.span("native: layout for export");
    let prepared = prepare_crates(&cx, standing, closure, watch, out_frame)?;
    let ask = &standing.native;
    let alloc = out_bump.alloc();
    let crates = &prepared.crates;
    let mut tables = Vec::with_capacity_in(crates.len(), out_frame);
    let mut outputs = Vec::with_capacity_in(crates.len(), out_frame);
    for c in crates.iter() {
        match native_links::open_table(c.table, alloc) {
            Ok(Table::Links(links)) => tables.push(links),
            Ok(Table::Refused(words)) => return Err(NativeError::TableRefused { package: held(c.package, ask), words: held(words, ask) }),
            Err(why) => return Err(NativeError::TableForm { package: held(c.package, ask), why }),
        }
        match c.script {
            None => outputs.push(None),
            Some(script) => match native_image::script_outputs(script.payload, ask.home) {
                Ok((opened, area)) => {
                    let area: &blaze_world::StoredFiles = blaze_own::Box::into_bump_ref(blaze_own::Box::new_in(area, out_frame));
                    outputs.push(Some((script.out, opened.with_area(area))));
                }
                Err(why) => return Err(NativeError::Script { package: held(c.package, ask), script: script.name, why }),
            },
        }
    }
    let tables = tables.into_bump_slice();
    let outputs = outputs.into_bump_slice();
    let mut linked = Vec::with_capacity_in(crates.len(), out_frame);
    for (c, (table, output)) in crates.iter().zip(tables.iter().zip(outputs.iter().copied())) {
        linked.push(ProgramCrate { package: c.package, outputs: output, links: table });
    }
    let linked: &'s [ProgramCrate<'s>] = linked.into_bump_slice();
    let producer: &'s ProducerId<Whole> = blaze_own::Box::into_bump_ref(blaze_own::Box::new_in(ask.producer, out_frame));
    let installs = match native_links::program_libraries(linked, prepared.platform, producer, alloc) {
        Ok(asked) => stubs_read(&cx, &asked, prepared.sdk, ask, out_frame)?,
        Err(error) => return Err(NativeError::Link { what: "the program's libraries", words: held(bstd::mem::format_in!(out_frame, "{error:?}").as_str(), ask) }),
    };
    let installs: &'s [Linked<'s>] = installs.into_bump_slice();
    let image = match native_links::program_layout(linked, installs, prepared.platform, producer, blaze_hash::FixedState, out_bump) {
        Ok(image) => image,
        Err(error) => return Err(NativeError::Link { what: "the program's native image", words: held(bstd::mem::format_in!(out_frame, "{error:?}").as_str(), ask) }),
    };
    let has_text = image.region(blaze_native::image::Region::Text).is_some_and(|r| r.len > 0);
    let has_extra_lib = image.libraries.iter().any(|l| l.install().as_bytes() != LIB_SYSTEM && l.install().as_bytes() != LIBC_LINUX);
    if !has_text && image.words.is_empty() && image.inits == 0 && image.tls.is_empty() && !has_extra_lib {
        return Ok(None);
    }
    // Collect every Rust C export (unmangled name -> fn symbol), sorted by name for `native_merge`.
    let mut exports: AVec<'s, blaze_export::CExport<'s>> = Vec::new_in(alloc);
    let mut items: AVec<'s, blaze_id::ItemKey> = Vec::new_in(alloc);
    for table in tables.iter() {
        for export in table.exports.iter() {
            let checker = blaze_ladder::front2::keys::checker_of(export.item);
            let symbol = blaze_ladder::symbols::fn_symbol(checker, blaze_typeck::List::EMPTY, blaze_typeck::PTys::EMPTY, alloc)
                .map_err(|why| NativeError::Send { words: held(bstd::mem::format_in!(out_frame, "export `{}` symbol: {why:?}", export.name).as_str(), ask) })?;
            exports.push(blaze_export::CExport { name: export.name.as_bytes(), symbol });
            if items.scan_for(&export.item).is_none() {
                items.push(export.item);
            }
        }
    }
    exports.sort_unstable_by(|a, b| a.name.cmp(b.name));
    Ok(Some(ExportNative { image, exports, items }))
}

/// The link table of the crate whose `CrateScope` record `scope` names: that record's `links`
/// section, in place.
fn table_of<'m, 'w>(cx: &Cx, store: &mut dyn Store<'m>, scope: &Input, package: &str, ask: &NativeAsk<'w>) -> Result<&'m [u8], NativeError<'w>> {
    let payload = native_image::payload_in_place(cx, store, scope).map_err(NativeError::Store)?;
    // A crate refused before its names were resolved files no sections.
    if payload.is_empty() {
        return Err(NativeError::NoTable { package: held(package, ask), why: "it holds no sections (its names were not resolved)" });
    }
    match crate::names::sections::sections(payload) {
        Ok(sections) => Ok(sections.links),
        Err(_) => Err(NativeError::NoTable { package: held(package, ask), why: "its payload does not open as a `CrateScope` payload (names/sections.rs `sections`)" }),
    }
}

/// The image of `program` linking `crates` (link order, the program's crate first): found at
/// [`image_key`] in `store`, else made in `scratch` and put there. The record found is checked
/// once to be this program's and these inputs' (its name and source), never made again over.
/// `sdk`: the macOS SDK whose stubs name the libraries (`None` on Linux).
#[expect(clippy::too_many_arguments, reason = "each is one input of the image or of its key, named at the call")]
pub(crate) fn image_in<'w>(
    cx: &Cx,
    store: &mut dyn ImageStore<'_>,
    program: Name,
    crates: &[CrateIn<'_>],
    platform: Platform,
    sdk: Option<blaze_apple_sdk::Sdk<'_>>,
    ask: &NativeAsk<'w>,
    scratch: &Frame,
) -> Result<Image, NativeError<'w>> {
    let cx = cx.span("native: find or make the image");
    if crates.is_empty() {
        return Err(NativeError::NoProgram);
    }
    let sdk = match (platform, sdk) {
        (Platform::MacosArm64, Some(sdk)) => Some(sdk),
        (Platform::MacosArm64, None) => return Err(NativeError::Sdk { words: "none was handed to the image (engine_native.rs `image_in`)" }),
        (Platform::LinuxArm64, _) => None,
    };
    let key = image_key(program, platform, &ask.producer, crates, sdk.map(|sdk| sdk.identity()));
    if let Some(bytes) = store.get(&cx, key).map_err(NativeError::Store)? {
        let view = match blaze_drive::record::open(bytes) {
            Ok(Stored::Whole(view)) => view,
            Ok(Stored::Shape(_)) => return Err(NativeError::Damaged { key, why: "a shape record, which holds no image" }),
            Err(_) => return Err(NativeError::Damaged { key, why: "bytes that are no drive record" }),
        };
        if view.name != program || view.source != key.0 {
            return Err(NativeError::Damaged { key, why: "the record of another program or of other inputs" });
        }
        if view.payload.is_empty() {
            return Err(NativeError::Damaged { key, why: "an empty image (a program with no C input still has one)" });
        }
        return Ok(Image { key, made: false, bytes: view.payload.len() });
    }

    // Made now: each crate's table and its script's outputs opened, in the scratch frame.
    let erased = blaze_own::Erased::new(scratch);
    let alloc = erased.handle();
    let mut tables = Vec::with_capacity_in(crates.len(), scratch);
    let mut outputs = Vec::with_capacity_in(crates.len(), scratch);
    for c in crates.iter() {
        match native_links::open_table(c.table, alloc) {
            Ok(Table::Links(links)) => tables.push(links),
            Ok(Table::Refused(words)) => return Err(NativeError::TableRefused { package: held(c.package, ask), words: held(words, ask) }),
            Err(why) => return Err(NativeError::TableForm { package: held(c.package, ask), why }),
        }
        match c.script {
            None => outputs.push(None),
            Some(script) => match native_image::script_outputs(script.payload, ask.home) {
                Ok((opened, area)) => {
                    // Held in the scratch frame for as long as the link reads the outputs.
                    let area: &blaze_world::StoredFiles = blaze_own::Box::into_bump_ref(blaze_own::Box::new_in(area, scratch));
                    outputs.push(Some((script.out, opened.with_area(area))));
                }
                Err(why) => return Err(NativeError::Script { package: held(c.package, ask), script: script.name, why }),
            },
        }
    }
    let mut linked = Vec::with_capacity_in(crates.len(), scratch);
    for (at, c) in crates.iter().enumerate() {
        linked.push(ProgramCrate { package: c.package, outputs: outputs[at], links: &tables[at] });
    }
    let installs = match native_links::program_libraries(&linked, platform, &ask.producer, alloc) {
        Ok(asked) => stubs_read(&cx, &asked, sdk, ask, scratch)?,
        Err(error) => return Err(NativeError::Link { what: "the program's libraries", words: held(bstd::mem::format_in!(scratch, "{error:?}").as_str(), ask) }),
    };
    let out = match native_links::program_key(&linked, &installs, platform, &ask.producer, alloc) {
        Ok(out) => out,
        Err(error) => return Err(NativeError::Link { what: "the program's native key", words: held(bstd::mem::format_in!(scratch, "{error:?}").as_str(), ask) }),
    };
    let mut image = AVec::new_in(alloc);
    if let Err(error) = native_links::program_image(&linked, &installs, platform, &ask.producer, blaze_hash::FixedState, &mut image, erased.bump()) {
        return Err(NativeError::Link { what: "the program's native image", words: held(bstd::mem::format_in!(scratch, "{error:?}").as_str(), ask) });
    }
    // What it read, in link order: each crate's table (by its digest) and its script's run.
    let mut reads = Vec::with_capacity_in(crates.len() * 2, scratch);
    for c in crates.iter() {
        reads.push((c.links, native_links::table_digest(c.table)));
        if let Some(script) = c.script {
            reads.push((script.name, script.out));
        }
    }
    let head = blaze_drive::record::write_plain_head(program, key.0, out, &reads, image.len(), alloc);
    store.put_parts(&cx, key, &[head.as_slice(), image.as_slice()]).map_err(NativeError::Store)?;
    Ok(Image { key, made: true, bytes: image.len() })
}

/// Each library `asked` names by an SDK stub (macOS), linked as its stub states: the stub read
/// through `ask.fs` under the SDK's folder into `scratch` and parsed by blaze-native tbd.rs.
/// Linux asks for none. A stub missing or unread is refused by name ([`NativeError::Stub`]).
fn stubs_read<'s, 'w>(cx: &Cx, asked: &[Asked<'s>], sdk: Option<blaze_apple_sdk::Sdk<'_>>, ask: &NativeAsk<'w>, scratch: &'s Frame) -> Result<Vec<Linked<'s>, &'s Frame>, NativeError<'w>> {
    let mut installs = Vec::with_capacity_in(asked.len(), scratch);
    for one in asked {
        let Source::Stub(at) = one.source else { continue };
        let Some(sdk) = sdk else { return Err(NativeError::Sdk { words: "none was handed to the image (engine_native.rs `stubs_read`)" }) };
        let library = at.library();
        let refused = |why: StubWhy<'w>| {
            let named = match library.kind {
                LibraryKind::Dylib => bstd::mem::format_in!(scratch, "`-l{}`", library.name),
                LibraryKind::Framework => bstd::mem::format_in!(scratch, "`-framework {}`", library.name),
            };
            let path = bstd::mem::format_in!(scratch, "{}/{at}", sdk.folder().trim_end_matches('/'));
            NativeError::Stub { package: held(one.package, ask), library: held(named.as_str(), ask), path: held(path.as_str(), ask), why }
        };
        let unread = |words: &str| refused(StubWhy::Unread { words: held(words, ask) });
        let folder = bstd::path::Path::new(at.folder()).map_err(|e| unread(bstd::mem::format_in!(scratch, "{e}").as_str()))?;
        let file = bstd::path::Path::new(bstd::mem::format_in!(scratch, "{}", at.file()).as_str()).map_err(|e| unread(bstd::mem::format_in!(scratch, "{e}").as_str()))?;
        let dir = ask.fs.home(cx, sdk.folder(), &folder).map_err(|e| unread(bstd::mem::format_in!(scratch, "{e}").as_str()))?;
        let mut bytes = bstd::mem::Bytes::new_in(scratch, "an SDK library stub");
        match dir.read_all(cx, &file, &mut bytes) {
            Ok(_) => {}
            Err(error) if error.kind == bstd::fs::FsKind::NotFound => return Err(refused(StubWhy::Missing)),
            Err(error) => return Err(unread(bstd::mem::format_in!(scratch, "{error}").as_str())),
        }
        // Held in the scratch frame for as long as the link reads the install name.
        let bytes: &'s bstd::mem::Bytes<'s> = blaze_own::Box::into_bump_ref(blaze_own::Box::new_in(bytes, scratch));
        installs.push(at.linked(bytes.as_slice()).map_err(|why| refused(StubWhy::Form(why)))?);
    }
    Ok(installs)
}

#[cfg(test)]
mod tests {
    //! The image by key over a model store: the tables are written in native_links.rs's form by
    //! hand (its doc: magic, form, state, counts, then each decl), the script runs are real
    //! outputs records (blaze-build-script `Outputs::read` of a script's printed lines).
    //! Sources: Reference, items.extern.attributes.link (`#[link(name = "z")]` names a native
    //! library the program links); Cargo book, build-scripts `cargo::rustc-link-lib=LIB` ("the
    //! library is passed to the linker").

    use blaze_build_script::instr::OutputLimits;
    use blaze_build_script::outputs::Outputs;
    use blaze_drive::Kind;
    use blaze_ids::producer::Producers;
    use bstd::io::{Io, Limits, SimWorld};

    use super::*;

    /// The store of one home: records by key, each put once (equal bytes again are the same
    /// record, other bytes under one key are a test failure: rule 21).
    #[derive(Default)]
    struct Model {
        held: std::vec::Vec<(Key, &'static [u8])>,
        gets: u32,
        puts: u32,
    }

    impl ImageStore<'static> for Model {
        fn get(&mut self, _cx: &Cx, key: Key) -> Result<Option<&'static [u8]>, CliError> {
            self.gets += 1;
            Ok(self.held.iter().find(|(at, _)| *at == key).map(|(_, bytes)| *bytes))
        }

        fn put_parts(&mut self, _cx: &Cx, key: Key, parts: &[&[u8]]) -> Result<(), CliError> {
            self.puts += 1;
            let bytes: &'static [u8] = std::boxed::Box::leak(parts.concat().into_boxed_slice());
            match self.held.iter().find(|(at, _)| *at == key) {
                Some((_, old)) => assert_eq!(*old, bytes, "two records under one key (rule 21)"),
                None => self.held.push((key, bytes)),
            }
            Ok(())
        }
    }

    impl Model {
        /// The image's bytes (the payload of the record at `key`).
        fn image(&self, key: Key) -> &'static [u8] {
            let (_, bytes) = self.held.iter().find(|(at, _)| *at == key).unwrap_or_else(|| panic!("no record at {key:?}"));
            match blaze_drive::record::open(bytes) {
                Ok(Stored::Whole(view)) => view.payload,
                _ => panic!("the record at {key:?} is no whole record"),
            }
        }
    }

    /// A table of native_links.rs's form with one dylib decl a name, no foreign name, no export.
    fn table(decls: &[&str]) -> std::vec::Vec<u8> {
        let mut t = std::vec::Vec::new();
        t.extend_from_slice(&native_links::TABLE_MAGIC);
        t.extend_from_slice(&native_links::TABLE_FORM.to_le_bytes());
        t.extend_from_slice(&[0, 0]);
        for n in [decls.len(), 0, 0] {
            t.extend_from_slice(&u32::try_from(n).expect("a small count").to_le_bytes());
        }
        for name in decls {
            t.extend_from_slice(&u32::try_from(name.len()).expect("a short name").to_le_bytes());
            t.extend_from_slice(name.as_bytes());
            // Dylib, no modifiers, on the outline's item 0.
            t.extend_from_slice(&[0, 0]);
            t.extend_from_slice(&0u32.to_le_bytes());
        }
        t
    }

    /// A `Script` payload (scripts.rs `payload_parts`: the run's kind, then its outputs record)
    /// of a script that printed `printed`.
    fn script_payload(printed: &[u8]) -> std::vec::Vec<u8> {
        let frame = Frame::with_reserve(1 << 20, bstd::mem::Tag::Other).expect("the outputs' frame");
        let erased = blaze_own::Erased::new(&frame);
        let alloc = erased.handle();
        let outputs = Outputs::read(printed, "/out", OutputLimits::DEFAULT, alloc).expect("the script's lines read");
        let mut record = AVec::new_in(alloc);
        outputs.encode(&mut record);
        let mut payload = std::vec![1u8];
        payload.extend_from_slice(&record);
        payload
    }

    fn links(n: u8) -> Name {
        Name::links_of(Name::new(Kind::CrateScope, ContentId([n; 32])))
    }

    fn script(n: u8, payload: &'static [u8]) -> ScriptIn<'static> {
        ScriptIn { name: Name::new(Kind::Script, ContentId([n; 32])), out: ContentId::of("engine_native tests script out", payload), payload }
    }

    fn leak(bytes: std::vec::Vec<u8>) -> &'static [u8] {
        std::boxed::Box::leak(bytes.into_boxed_slice())
    }

    struct Fixture {
        io: Io,
        words: Frame,
        scratch: Frame,
        producer: ProducerId<Whole>,
        platform: Platform,
        sdk: blaze_apple_sdk::Sdk<'static>,
    }

    /// A text stub of the SDK's form (tbd v4, as the macOS 27.2 SDK's), naming `install`.
    fn stub(install: &str) -> std::string::String {
        std::format!("--- !tapi-tbd\ntbd-version:     4\ntargets:         [ x86_64-macos, arm64e-macos ]\ninstall-name:    '{install}'\nexports:\n  - targets: [ x86_64-macos, arm64e-macos ]\n    symbols: [ _f ]\n...\n")
    }

    /// The fake SDK at `/sdk`: `libc.tbd` (libSystem's), `libz.tbd`, Foundation under
    /// `Versions/C` and IOKit under `Versions/A`; no `Kernel` stub (a header-only framework).
    const SDK_STUBS: &[(&str, &str)] = &[
        ("sdk/usr/lib/libc.tbd", "/usr/lib/libSystem.B.dylib"),
        ("sdk/usr/lib/libz.tbd", "/usr/lib/libz.1.dylib"),
        ("sdk/System/Library/Frameworks/Foundation.framework/Foundation.tbd", "/System/Library/Frameworks/Foundation.framework/Versions/C/Foundation"),
        ("sdk/System/Library/Frameworks/IOKit.framework/IOKit.tbd", "/System/Library/Frameworks/IOKit.framework/Versions/A/IOKit"),
    ];

    /// `text` written at `path` beneath the sim's root, each folder made as a folder.
    fn put(io: &Io, path: &str, text: &str) {
        let cx = io.cx(bstd::test::BUDGET, bstd::mem::TASK_FRAME).expect("a root context");
        let root = io.need_fs().expect("files").root();
        for (at, _) in path.match_indices('/') {
            let folder = bstd::path::Path::new(&path[..at]).expect("a path");
            if root.stat(&cx, &folder).expect("stat").is_none() {
                root.make_dir(&cx, &folder, bstd::fs::Durable::No).expect("a folder");
            }
        }
        root.publish(&cx, &bstd::path::Path::new(path).expect("a path"), bstd::fs::Content::bytes(text.as_bytes()), bstd::fs::Durable::No).expect("write");
    }

    /// The SDK at `folder` whose settings name a version and whose build file names `build`, as
    /// the edge resolves one.
    fn sdk_at(folder: &'static str, settings: &'static str, build: &'static str) -> blaze_apple_sdk::Sdk<'static> {
        blaze_apple_sdk::resolve(
            Some(folder),
            || -> Result<&'static str, blaze_apple_sdk::XcrunFailed<'static>> { panic!("SDKROOT is set") },
            |_| -> Result<&'static str, blaze_apple_sdk::Unread<'static>> { Ok(settings) },
            |_| -> Result<&'static str, blaze_apple_sdk::Unread<'static>> { Ok(build) },
        )
        .expect("an SDK")
    }

    const BUILD_26B118: &str = "<key>ProductBuildVersion</key><string>26B118</string>";

    fn fixture(producer: &[u8]) -> Fixture {
        let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
        for (path, install) in SDK_STUBS {
            put(&io, path, &stub(install));
        }
        Fixture {
            io,
            words: Frame::with_reserve(1 << 20, bstd::mem::Tag::Other).expect("the words' frame"),
            scratch: Frame::with_reserve(256 << 20, bstd::mem::Tag::Other).expect("the image's frame"),
            producer: Producers::for_tests(producer).get::<Whole>(),
            // The fixture constructs a simulated Apple SDK at `/sdk` (`SDK_STUBS`) and exercises
            // Mach-O image writing purely over simulated IO, so pin the target platform rather than
            // reading the host OS (otherwise Linux CI fails with `Write(NotMac)`).
            platform: Platform::MacosArm64,
            sdk: sdk_at("/sdk", r#"{"Version":"27.2"}"#, BUILD_26B118),
        }
    }

    /// The SDK's part of [`image_key`] as [`image_in`] passes it.
    fn sdk_key(f: &Fixture) -> Option<blaze_apple_sdk::SdkId> {
        match f.platform {
            Platform::MacosArm64 => Some(f.sdk.identity()),
            Platform::LinuxArm64 => None,
        }
    }

    fn try_image<'f>(f: &'f Fixture, store: &mut Model, crates: &[CrateIn<'_>]) -> Result<Image, NativeError<'f>> {
        let cx = f.io.cx(bstd::test::BUDGET, bstd::mem::TASK_FRAME).expect("a root context");
        let ask = NativeAsk { producer: f.producer, home: None, words: &f.words, fs: f.io.need_fs().expect("files") };
        image_in(&cx, store, native_image::program_name("src/main.rs"), crates, f.platform, Some(f.sdk), &ask, &f.scratch)
    }

    fn image(f: &Fixture, store: &mut Model, crates: &[CrateIn<'_>]) -> Image {
        try_image(f, store, crates).unwrap_or_else(|e| panic!("the image: {e}"))
    }

    fn contains(bytes: &[u8], needle: &[u8]) -> bool {
        bytes.windows(needle.len()).any(|window| window == needle)
    }

    /// The hand-written tables are native_links.rs's form: they open to the decls written.
    #[test]
    fn the_fixture_tables_open() {
        let frame = Frame::with_reserve(1 << 20, bstd::mem::Tag::Other).expect("a frame");
        let erased = blaze_own::Erased::new(&frame);
        let bytes = table(&["z"]);
        match native_links::open_table(&bytes, erased.handle()) {
            Ok(Table::Links(links)) => assert_eq!(links.decls.iter().map(|d| d.name).collect::<std::vec::Vec<_>>(), ["z"]),
            _ => panic!("the fixture's table does not open"),
        }
    }

    /// A program with no C input (one crate, an empty table, no script) gets a real image that
    /// links the system library alone, made once; a second call is a HIT: no put, the same key.
    /// CONTROL: the same program with a `#[link(name = "z")]` table is another key, made, so the
    /// hit is the key's and not a key that ignores the table.
    ///
    /// Prints `engine_native: no C input: made 1 then 0, <n> bytes`.
    #[test]
    fn no_c_input_is_a_system_image_and_a_repeat_is_a_hit() {
        let f = fixture(b"engine_native no C input");
        let empty = leak(table(&[]));
        let crates = [CrateIn { package: "app", links: links(1), table: empty, script: None }];
        let mut store = Model::default();
        let first = image(&f, &mut store, &crates);
        assert!(first.made && store.puts == 1, "the first start makes the image: {first:?}");
        assert!(first.bytes > 0, "no empty case: the image is written");
        assert!(!contains(store.image(first.key), b"libz"), "a program with no C input links no libz");
        let again = image(&f, &mut store, &crates);
        assert_eq!((again.key, again.made, store.puts), (first.key, false, 1), "the repeat is a hit: 0 made");
        let with_z = [CrateIn { package: "app", links: links(1), table: leak(table(&["z"])), script: None }];
        let moved = image(&f, &mut store, &with_z);
        assert!(moved.made && moved.key != first.key, "control: a table that moved is another key, made");
        std::println!("engine_native: no C input: made 1 then 0, {} bytes", first.bytes);
    }

    /// A crate of the program with a `#[link(name = "z")]` block: its library is on the image's
    /// link line (the image names `libz`), and the record's reads list that crate's `Links`.
    /// CONTROL: the program without that crate names no `libz` (previous test).
    ///
    /// Prints `engine_native: #[link(name = "z")] in a dependency: the image names libz`.
    #[test]
    fn a_link_block_is_in_the_image() {
        let f = fixture(b"engine_native link block");
        let (app, zl) = (leak(table(&[])), leak(table(&["z"])));
        let crates = [CrateIn { package: "app", links: links(1), table: app, script: None }, CrateIn { package: "zl", links: links(2), table: zl, script: None }];
        let mut store = Model::default();
        let made = image(&f, &mut store, &crates);
        assert!(contains(store.image(made.key), b"libz"), "the image does not name libz");
        let (_, bytes) = store.held.iter().find(|(at, _)| *at == made.key).expect("the record");
        let Ok(Stored::Whole(view)) = blaze_drive::record::open(bytes) else { panic!("no whole record") };
        let reads: std::vec::Vec<(Name, ContentId)> = view.reads().collect();
        assert!(reads.contains(&(links(2), native_links::table_digest(zl))), "the record's reads do not list `zl`'s table: {reads:?}");
        std::println!("engine_native: #[link(name = \"z\")] in a dependency: the image names libz");
    }

    /// A build script that prints `cargo::rustc-link-lib=z`: its run's library is in the image.
    /// CONTROL: the same crate whose script prints only a cfg names no `libz`, under another key
    /// (the run's out is in the key).
    ///
    /// Prints `engine_native: a build script's link line: the image names libz; a cfg-only script does not`.
    #[test]
    fn a_build_scripts_link_line_is_in_the_image() {
        let f = fixture(b"engine_native script link");
        let empty = leak(table(&[]));
        let linking = leak(script_payload(b"cargo::rustc-link-lib=z\n"));
        let cfg_only = leak(script_payload(b"cargo::rustc-cfg=has_z\n"));
        let program = |run: ScriptIn<'static>| [CrateIn { package: "app", links: links(1), table: empty, script: None }, CrateIn { package: "zsys", links: links(2), table: empty, script: Some(run) }];
        let mut store = Model::default();
        let linked = image(&f, &mut store, &program(script(3, linking)));
        assert!(contains(store.image(linked.key), b"libz"), "the script's link line is not in the image");
        let plain = image(&f, &mut store, &program(script(3, cfg_only)));
        assert!(plain.made && plain.key != linked.key, "control: another run is another key");
        assert!(!contains(store.image(plain.key), b"libz"), "control: a cfg-only script links no libz");
        std::println!("engine_native: a build script's link line: the image names libz; a cfg-only script does not");
    }

    /// RESTORE EQUALS FRESH (rule 26): the image made in two homes of the same inputs is the same
    /// key and the same bytes, and the record a hit finds is the one made fresh. CONTROL: another
    /// producer is another key (the producer is in the key, D105).
    ///
    /// Prints `engine_native: two homes, one key, equal bytes (<n>)`.
    #[test]
    fn restore_equals_fresh() {
        let f = fixture(b"engine_native restore");
        let (app, zl) = (leak(table(&[])), leak(table(&["z"])));
        let run = script(3, leak(script_payload(b"cargo::rustc-link-lib=z\n")));
        let crates = [CrateIn { package: "app", links: links(1), table: app, script: None }, CrateIn { package: "zl", links: links(2), table: zl, script: Some(run) }];
        let (mut one, mut two) = (Model::default(), Model::default());
        let (a, b) = (image(&f, &mut one, &crates), image(&f, &mut two, &crates));
        assert_eq!(a.key, b.key, "two homes, two keys");
        assert_eq!(one.held[0].1, two.held[0].1, "two homes, two records");
        let hit = image(&f, &mut two, &crates);
        assert!(!hit.made && hit.key == a.key && two.image(hit.key) == one.image(a.key), "the hit's image is not the fresh one");
        let other = fixture(b"engine_native restore: another producer");
        assert_ne!(image_key(native_image::program_name("src/main.rs"), other.platform, &other.producer, &crates, sdk_key(&other)), a.key, "control: the producer is not in the key");
        std::println!("engine_native: two homes, one key, equal bytes ({})", a.bytes);
    }

    /// A record at the image's key that is not this program's image is refused by its key, never
    /// made again over. CONTROL: the same store emptied makes the image.
    #[test]
    fn a_damaged_record_is_refused_by_its_key() {
        let f = fixture(b"engine_native damaged");
        let crates = [CrateIn { package: "app", links: links(1), table: leak(table(&[])), script: None }];
        let key = image_key(native_image::program_name("src/main.rs"), f.platform, &f.producer, &crates, sdk_key(&f));
        let mut store = Model::default();
        store.held.push((key, &b"not a record"[..]));
        match try_image(&f, &mut store, &crates) {
            Err(NativeError::Damaged { key: at, .. }) => assert_eq!(at, key),
            other => panic!("a damaged record was not refused: {:?}", other.map(|image| image.key)),
        }
        assert_eq!(store.puts, 0, "nothing was put over it");
        let mut fresh = Model::default();
        assert!(image(&f, &mut fresh, &crates).made, "control: an empty store makes it");
    }

    /// The program's crates as one app crate over a `-sys` crate whose build script printed
    /// `printed`.
    fn over_script(printed: &[u8]) -> [CrateIn<'static>; 2] {
        let empty = leak(table(&[]));
        [CrateIn { package: "app", links: links(1), table: empty, script: None }, CrateIn { package: "ksys", links: links(2), table: empty, script: Some(script(3, leak(script_payload(printed)))) }]
    }

    /// macOS: each framework's load command names the install name its SDK stub states, read from
    /// the stub (Foundation under `Versions/C`, IOKit under `Versions/A`), never made from the
    /// name. TWIN (rule 3): a framework with no stub in the SDK (`Kernel`, header-only) is refused
    /// naming the library, the crate that links it and the path asked, and nothing is put.
    ///
    /// Prints `engine_native: Foundation is Versions/C and IOKit Versions/A, read from the SDK; Kernel refused: <refusal>`.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_frameworks_install_name_is_its_sdk_stubs() {
        let f = fixture(b"engine_native frameworks");
        let mut store = Model::default();
        let made = image(&f, &mut store, &over_script(b"cargo::rustc-link-lib=framework=Foundation\ncargo::rustc-link-lib=framework=IOKit\n"));
        let bytes = store.image(made.key);
        assert!(contains(bytes, b"/System/Library/Frameworks/Foundation.framework/Versions/C/Foundation"), "Foundation's stub's install name is not in the image");
        assert!(contains(bytes, b"/System/Library/Frameworks/IOKit.framework/Versions/A/IOKit"), "IOKit's stub's install name is not in the image");
        let mut empty = Model::default();
        let refused = try_image(&f, &mut empty, &over_script(b"cargo::rustc-link-lib=framework=Kernel\n"));
        let words = match refused {
            Err(error @ NativeError::Stub { package: "ksys", library: "`-framework Kernel`", path: "/sdk/System/Library/Frameworks/Kernel.framework/Kernel.tbd", why: StubWhy::Missing }) => std::format!("{error}"),
            other => panic!("a framework with no stub was not refused by name: {:?}", other.map(|image| image.key)),
        };
        assert_eq!(empty.puts, 0, "nothing was put for a refused image");
        std::println!("engine_native: Foundation is Versions/C and IOKit Versions/A, read from the SDK; Kernel refused: {words}");
    }

    /// CONTROL of the read (rule 21, 23): the same program over an SDK whose `libz.tbd` states
    /// another install name makes another image, whose own key (the record's out, natives.rs
    /// `native_key`) moved and whose load command names the new path; a stub that is no stub is
    /// refused by its form. And the SDK's identity is in the image's key: one version in two
    /// folders is one key (no path of the machine in it, rule 25), another version another key.
    ///
    /// Prints `engine_native: a stub's install name moves the image's own key; the SDK's version moves its key, its folder does not`.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_stubs_install_name_moves_the_images_key() {
        let f = fixture(b"engine_native stub control");
        let crates = [CrateIn { package: "app", links: links(1), table: leak(table(&["z"])), script: None }];
        let out = |store: &Model, key: Key| {
            let (_, bytes) = store.held.iter().find(|(at, _)| *at == key).expect("the record");
            let Ok(Stored::Whole(view)) = blaze_drive::record::open(bytes) else { panic!("no whole record") };
            view.out
        };
        let mut one = Model::default();
        let first = image(&f, &mut one, &crates);
        assert!(contains(one.image(first.key), b"/usr/lib/libz.1.dylib"));
        put(&f.io, "sdk/usr/lib/libz.tbd", &stub("/usr/lib/libz.2.dylib"));
        let mut two = Model::default();
        let second = image(&f, &mut two, &crates);
        assert_ne!(out(&one, first.key), out(&two, second.key), "the image's own key does not hold the stub's install name");
        assert!(contains(two.image(second.key), b"/usr/lib/libz.2.dylib"), "the image does not name the stub's install name");
        put(&f.io, "sdk/usr/lib/libz.tbd", "not a stub");
        match try_image(&f, &mut Model::default(), &crates) {
            Err(NativeError::Stub { package: "app", library: "`-lz`", why: StubWhy::Form(_), .. }) => {}
            other => panic!("a stub of no form was not refused: {:?}", other.map(|image| image.key)),
        }
        let program = native_image::program_name("src/main.rs");
        let key = |sdk: blaze_apple_sdk::Sdk<'static>| image_key(program, f.platform, &f.producer, &crates, Some(sdk.identity()));
        let tools = sdk_at("/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk", r#"{"Version":"27.2"}"#, BUILD_26B118);
        let older = sdk_at("/Library/Developer/CommandLineTools/SDKs/MacOSX26.5.sdk", r#"{"Version":"26.5"}"#, "<key>ProductBuildVersion</key><string>25F70</string>");
        let beta = sdk_at("/Library/Developer/CommandLineTools/SDKs/MacOSX27.2.sdk", r#"{"Version":"27.2"}"#, "<key>ProductBuildVersion</key><string>26B5044k</string>");
        assert_eq!(key(f.sdk), key(tools), "one SDK build in two folders keyed two images");
        assert_ne!(key(f.sdk), key(older), "two SDK versions keyed one image");
        assert_ne!(key(f.sdk), key(beta), "two builds of one SDK version keyed one image");
        std::println!("engine_native: a stub's install name moves the image's own key; the SDK's version and build move its key, its folder does not");
    }

    /// Linux ELF shared objects: `elf_holds_native` reports `false` for a bare `libc.so.6`-only
    /// image with empty `.text`, and `true` when the image links an extra library (`libm.so.6`).
    #[test]
    fn elf_holds_native_detects_extra_libraries_and_empty_text() {
        let f = fixture(b"engine_native elf holds");
        let cx = f.io.cx(bstd::test::BUDGET, bstd::mem::TASK_FRAME).expect("a root context");
        let ask = NativeAsk { producer: f.producer, home: None, words: &f.words, fs: f.io.need_fs().expect("files") };
        let empty = leak(table(&[]));
        let with_m = leak(table(&["m"]));
        let mut store = Model::default();
        let bare = image_in(
            &cx,
            &mut store,
            native_image::program_name("src/main.rs"),
            &[CrateIn { package: "app", links: links(1), table: empty, script: None }],
            Platform::LinuxArm64,
            None,
            &ask,
            &f.scratch,
        )
        .expect("bare linux image");
        assert_eq!(elf_holds_native(store.image(bare.key)), Ok(false), "empty .text + libc.so.6 alone holds no native code");
        let linked_m = image_in(
            &cx,
            &mut store,
            native_image::program_name("src/main.rs"),
            &[CrateIn { package: "app", links: links(1), table: with_m, script: None }],
            Platform::LinuxArm64,
            None,
            &ask,
            &f.scratch,
        )
        .expect("linux image linking libm");
        assert_eq!(elf_holds_native(store.image(linked_m.key)), Ok(true), "DT_NEEDED libm.so.6 holds native dependency");
    }
}
