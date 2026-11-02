//! The Mach-O format of the linker: macOS executables (`MH_EXECUTE`, position-independent)
//! and dynamic libraries (`MH_DYLIB`) on AArch64 and x86-64, laid out as ld64 lays them.
//!
//! Segments, in address order: `__PAGEZERO` (an executable's first 4 GiB, unmapped),
//! `__TEXT` (the headers, code, read-only data, `__unwind_info`, `__eh_frame`: read and
//! run), `__DATA_CONST` (what the loader fixes up and then makes read-only: the GOT,
//! initializers, constant data with pointers, the C objects' `__compact_unwind`), `__DATA`
//! (the jump table's cells, data, thread-locals, zeroed data), a dev run's reservations
//! (`__BLAZE_ARENA`, `__BLAZE_CELLS`, `__BLAZE_FIXED`: no access and no file bytes, which its
//! start hook maps into, [`crate::DevRun`]), `__DWARF` when the image carries debug
//! info, and `__LINKEDIT` (what dyld reads: the rebase and bind opcodes, the export trie, the
//! symbol table, the function starts and, last, the code signature, without which macOS on
//! arm64 kills the program). Addresses the loader fixes up are told it by opcodes
//! (`LC_DYLD_INFO_ONLY`), not in chains.
//!
//! Every region but a zeroed one is in the file at its address less the base, so a region's
//! offset never moves while its address doesn't. dyld finds the thread-local variable
//! descriptors by their section's type (`S_THREAD_LOCAL_VARIABLES`): they are all in
//! [`Class::ThreadVars`], C objects' too.
//!
//! Names in Mach-O carry a leading underscore and the core's don't ([`push_core_name`],
//! [`spelled`]); a Mach-O name without one (`section$start$...`) is the core's with a
//! leading `\x01`, as LLVM writes a name taken as it is.

mod archive;
mod chained;
mod dylib;
mod input;
mod linkedit;
mod sign;
mod tbd;
#[cfg(test)]
mod tests;
mod unwind;

pub(crate) use input::{Autolink, asked_libraries, autolink};

/// The libraries the Mach-O dynamic library or `.tbd` stub `bytes` (`name` on the link line)
/// re-exports from outside itself ([`crate::model::Shared::inputs`]), by install name: a
/// dylib's `LC_REEXPORT_DYLIB`s, a stub's re-exports that are no document of it. ld64 loads
/// each with the library, so the link line has them too ([`crate::target`]). A universal file
/// is read for `arch`'s slice.
pub(crate) fn reexported(name: &str, bytes: &[u8], arch: Arch, platform: u32) -> Result<Vec<String>, String> {
    let cpu = match arch {
        Arch::Aarch64 => macho::CPU_TYPE_ARM64,
        Arch::X86_64 => macho::CPU_TYPE_X86_64,
        // alloc: error path
        Arch::Wasm32 => return Err(format!("{name}: a Mach-O library in a wasm32 link")),
    };
    let bytes = input::slice(name, bytes, cpu)?;
    let magic = bytes.get(..4).map(|it| u32::from_le_bytes([it[0], it[1], it[2], it[3]]));
    let shared = match magic == Some(macho::MH_MAGIC_64) {
        true => dylib::read(name, bytes, arch)?,
        false => {
            let text = std::str::from_utf8(bytes).map_err(|_| {
                // alloc: error path
                format!("{name} is neither a Mach-O dynamic library nor a `.tbd` stub")
            })?;
            tbd::read(name, text, arch, platform)?
        }
    };
    Ok(shared.inputs)
}
#[cfg(test)]
pub(crate) use input::tests::linking as linking_object;
#[cfg(feature = "fuzz")]
pub(crate) use dylib::trie_differential;
#[cfg(feature = "fuzz")]
pub(crate) use tbd::differential as tbd_differential;

use std::cmp::Ordering;

use blaze_codegen::Arch;
use blaze_format::cursor::Put;
use blaze_graph::par;
use blaze_ids::radix;
use blaze_sha256 as sha256;
use object::macho;
use parking_lot::Mutex;

use crate::cow::CowBytes;
use crate::model::{
    Class, Counts, Data, Delta, File, Holds, Name, NativeFile, Names, Placed, Plan, Prot, Region,
    Reserve, SectionKind, SectionName, Special, Writer,
};
use smallvec::SmallVec;
use crate::output::Platform;
use crate::reset::{self, Reset};
use crate::{Format, LinkError, NativeKind, Output, OutputKind};

/// Where an executable's image starts: after `__PAGEZERO`, which catches 32-bit pointers.
const BASE: u64 = 0x1_0000_0000;
/// The room for the header and the load commands, which grow with the libraries and with an
/// incremental link's sections.
const HEADERS: u64 = 0x4000;
const HEADER: usize = 32;
const SEGMENT: usize = 72;
const SECTION: usize = 80;
const NLIST: usize = 16;
const UNWIND_INFO: &str = "__unwind_info";
/// The dev runtime's start hook, as the core names it (`_blaze_inproc_start` in Mach-O).
const START: &str = "blaze_inproc_start";

fn error(what: impl Into<String>) -> LinkError {
    LinkError { function: None, what: what.into(), refusal: None }
}

pub(crate) fn writer(output: &Output) -> Result<MachO, LinkError> {
    let (cpu, subtype, page) = match output.arch {
        Arch::Aarch64 => (macho::CPU_TYPE_ARM64.0, macho::CPU_SUBTYPE_ARM64_ALL.0, 0x4000),
        Arch::X86_64 => (macho::CPU_TYPE_X86_64.0, macho::CPU_SUBTYPE_X86_64_ALL.0, 0x1000),
        Arch::Wasm32 => return Err(error("a Mach-O image for wasm32, which has no Mach-O")),
    };
    if output.dev_run.is_some() {
        if output.arch != Arch::Aarch64 {
            return Err(error(
                "a dev run's Mach-O executable for x86-64: a run's start hook overlays the code of \
                 arm64 executables only (`blaze_inproc::image`)",
            ));
        }
        if !matches!(output.kind, OutputKind::Executable { .. }) {
            return Err(error(
                "a dev run of a dynamic library: only an executable has the start hook that \
                 overlays its code",
            ));
        }
    }
    let Some(platform) = output.platform else {
        return Err(error(
            "a Mach-O image without a platform (`Output::platform`): `LC_BUILD_VERSION` says \
             which OS and SDK it is for, and dyld refuses an image without it",
        ));
    };
    let cache = output.incremental.then(Mutex::default);
    // alloc: clone: the writer and its copy of the output, once per image
    Ok(MachO { output: output.clone(), cpu, subtype, page, platform, cache })
}

pub(crate) struct MachO {
    output: Output,
    cpu: u32,
    subtype: u32,
    page: u64,
    platform: Platform,
    /// What the last link made, for an image linked again in place. Locked only by
    /// [`Writer::synthesize`] and [`Writer::file`], each called once per link of the image
    /// one after the other, so never contended and never held across a call out of the
    /// writer.
    cache: Option<Mutex<Kept>>,
}

/// [`push_core_name`] as a `String`: the readers' tests compare against it.
#[cfg(any(test, feature = "fuzz"))]
pub(super) fn core_name(name: &[u8]) -> Result<String, String> {
    let name = std::str::from_utf8(name)
        // alloc: the error's message
        .map_err(|_| format!("the symbol `{}` is not UTF-8", String::from_utf8_lossy(name)))?;
    // alloc: an owned name for tests and the fuzz differential only
    Ok(match name.strip_prefix('_') {
        // alloc: as above
        Some(rest) => rest.to_owned(),
        // alloc: as above
        None => format!("\x01{name}"),
    })
}

/// The name the core knows a Mach-O symbol by, added to a file's `names`: without its leading
/// underscore, or with a leading `\x01` when it has none. No `String` of its own: a name
/// without an underscore (clang's kept `ltmp0`, `l_` labels) gains its `\x01` on the stack.
pub(super) fn push_core_name(names: &mut Names, name: &[u8]) -> Result<Name, String> {
    let text = std::str::from_utf8(name)
        // alloc: error path
        .map_err(|_| format!("the symbol `{}` is not UTF-8", String::from_utf8_lossy(name)))?;
    if let Some(rest) = text.strip_prefix('_') {
        return names.push(rest);
    }
    let mut buffer = [0u8; 256];
    match buffer.get_mut(1..=text.len()) {
        Some(tail) => {
            tail.copy_from_slice(text.as_bytes());
            buffer[0] = 1;
            let marked = std::str::from_utf8(&buffer[..=text.len()])
                .expect("`\\x01` and a `str` are UTF-8");
            names.push(marked)
        }
        // alloc: a name without `_` longer than 255 bytes, which no compiler writes: pushed once
        None => names.push(&format!("\x01{text}")),
    }
}

/// The Mach-O name of a symbol the core names `name`.
#[cfg(test)]
pub(super) fn macho_name(name: &str) -> String {
    let (prefix, rest) = spelled(name);
    let mut out = String::with_capacity(prefix.len() + rest.len());
    out.push_str(prefix);
    out.push_str(rest);
    out
}

/// [`macho_name`] in its two parts, built without allocating: the underscore it gains (or
/// none), and the rest.
fn spelled(name: &str) -> (&'static str, &str) {
    match name.strip_prefix('\x01') {
        Some(rest) => ("", rest),
        None => ("_", name),
    }
}

/// Two core names in the order of their Mach-O names' bytes, as the symbol table and the
/// trie sort them.
fn macho_order(a: &str, b: &str) -> Ordering {
    let ((a_prefix, a), (b_prefix, b)) = (spelled(a), spelled(b));
    if a_prefix == b_prefix {
        return a.as_bytes().cmp(b.as_bytes());
    }
    (a_prefix.bytes().chain(a.bytes())).cmp(b_prefix.bytes().chain(b.bytes()))
}

/// The locals' `nlist_64` entries and names, appended to `nlist` and `strings` in the order of
/// `locals`: each 4,096 filled by its own task at offsets from the names' prefix sums. The
/// error is the first local's in order.
fn locals_into(
    locals: &[(u64, &str)],
    section_at: &(impl Fn(u64, &str) -> Result<u8, LinkError> + Sync),
    strings: &mut Vec<u8>,
    nlist: &mut Vec<u8>,
) -> Result<(), LinkError> {
    const CHUNK: usize = 4096;
    let spelled_len = |name: &str| {
        let (prefix, rest) = spelled(name);
        prefix.len() + rest.len() + 1
    };
    let sizes: Vec<usize> =
        // alloc: one size per 4,096 locals, per link
        par::map(par::chunks(locals, CHUNK), |chunk| chunk.iter().map(|it| spelled_len(it.1)).sum());
    let (names_at, entries_at) = (strings.len(), nlist.len());
    strings.resize(names_at + sizes.iter().sum::<usize>(), 0);
    nlist.resize(entries_at + locals.len() * NLIST, 0);
    // alloc: one slice of the names per 4,096 locals, per link
    let mut jobs = Vec::with_capacity(sizes.len());
    let (mut left, mut at) = (&mut strings[names_at..], names_at);
    for &size in &sizes {
        let (these, after) = std::mem::take(&mut left).split_at_mut(size);
        jobs.push((at, these));
        (left, at) = (after, at + size);
    }
    // alloc: one result per 4,096 locals, per link
    let results: Vec<Result<(), LinkError>> = par::map(
        par::zip(par::zip(par::chunks(locals, CHUNK), par::chunks_mut(&mut nlist[entries_at..], CHUNK * NLIST)), jobs),
        |((chunk, entries), (start, names))| {
            let mut written = 0;
            for (&(addr, name), entry) in chunk.iter().zip(entries.chunks_exact_mut(NLIST)) {
                let (prefix, rest) = spelled(name);
                entry[0..4].copy_from_slice(&((start + written) as u32).to_le_bytes());
                entry[4] = macho::N_SECT.0;
                entry[5] = section_at(addr, name)?;
                entry[6..8].copy_from_slice(&0u16.to_le_bytes());
                entry[8..16].copy_from_slice(&addr.to_le_bytes());
                let middle = written + prefix.len();
                names[written..middle].copy_from_slice(prefix.as_bytes());
                names[middle..middle + rest.len()].copy_from_slice(rest.as_bytes());
                written = middle + rest.len() + 1;
            }
            Ok(())
        },
    );
    for result in results {
        result?;
    }
    Ok(())
}

/// The class the C objects' `__compact_unwind` sections are placed in: relocated as data,
/// where each entry's personality field is a cell the loader fixes up, which `__unwind_info`
/// refers to the personality by.
pub(super) fn compact_unwind() -> Class {
    Class::Named { name: "__LD,__compact_unwind".into(), write: true, exec: false }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Segment {
    Text,
    DataConst,
    Data,
    /// A dev run's reservations ([`Reserve`]), after every segment with bytes and before
    /// `__DWARF` and `__LINKEDIT`: no access, no file bytes and no sections, as `__PAGEZERO`.
    Arena,
    Cells,
    Fixed,
}

impl Segment {
    fn reserves(self) -> bool {
        matches!(self, Segment::Arena | Segment::Cells | Segment::Fixed)
    }

    fn of(name: &str) -> Option<Segment> {
        match name {
            "__TEXT" => Some(Segment::Text),
            "__DATA_CONST" | "__LD" => Some(Segment::DataConst),
            "__DATA" => Some(Segment::Data),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Segment::Text => "__TEXT",
            Segment::DataConst => "__DATA_CONST",
            Segment::Data => "__DATA",
            Segment::Arena => "__BLAZE_ARENA",
            Segment::Cells => "__BLAZE_CELLS",
            Segment::Fixed => "__BLAZE_FIXED",
        }
    }

    fn prot(self) -> Prot {
        match self {
            Segment::Text => Prot::RX,
            // Written only by the loader, before the program runs.
            Segment::DataConst => Prot::R,
            Segment::Data => Prot::RW,
            Segment::Arena | Segment::Cells | Segment::Fixed => Prot::NONE,
        }
    }

    /// `initprot` and `maxprot` of its load command.
    fn vm_prot(self) -> u32 {
        match self {
            Segment::Text => macho::VM_PROT_READ.0 | macho::VM_PROT_EXECUTE.0,
            // `__DATA_CONST` is written by the loader, which makes it read-only after.
            Segment::DataConst | Segment::Data => macho::VM_PROT_READ.0 | macho::VM_PROT_WRITE.0,
            // The start hook maps over them with `VM_FLAGS_OVERWRITE`, which no `maxprot` limits.
            Segment::Arena | Segment::Cells | Segment::Fixed => 0,
        }
    }
}

/// The segment of a class and its place in the segment.
fn place(class: &Class) -> (Segment, u8) {
    match class {
        Class::Text => (Segment::Text, 0),
        Class::Patch => (Segment::Text, 1),
        Class::Stubs => (Segment::Text, 2),
        Class::Plt => (Segment::Text, 3),
        Class::Veneers => (Segment::Text, 4),
        Class::Named { name, .. } => match Segment::of(split(name).0) {
            Some(Segment::Text) => (Segment::Text, 5),
            Some(Segment::DataConst) => (Segment::DataConst, 4),
            Some(Segment::Data) | None => (Segment::Data, 3),
            Some(reserve @ (Segment::Arena | Segment::Cells | Segment::Fixed)) => (reserve, 0),
        },
        Class::Rodata => (Segment::Text, 6),
        Class::Synthetic(_) => (Segment::Text, 7),
        Class::EhFrame => (Segment::Text, 8),
        Class::Got => (Segment::DataConst, 0),
        Class::InitArray => (Segment::DataConst, 1),
        // Refused before the file is written ([`MachO::check_placed`]): Mach-O has none.
        Class::PreinitArray => (Segment::DataConst, 1),
        Class::FiniArray => (Segment::DataConst, 2),
        Class::RelRo => (Segment::DataConst, 3),
        Class::Table => (Segment::Data, 0),
        Class::Data => (Segment::Data, 1),
        Class::PatchData => (Segment::Data, 2),
        Class::ThreadVars => (Segment::Data, 4),
        Class::Tdata => (Segment::Data, 5),
        Class::Tbss => (Segment::Data, 6),
        Class::Bss => (Segment::Data, 7),
        Class::Reserve(Reserve::Arena) => (Segment::Arena, 0),
        Class::Reserve(Reserve::Cells) => (Segment::Cells, 0),
        Class::Reserve(Reserve::Fixed) => (Segment::Fixed, 0),
    }
}

fn split(name: &str) -> (&str, &str) {
    name.split_once(',').unwrap_or((name, ""))
}

/// Not in the file: zeroed data, and a dev run's reservations, which hold nothing.
fn zeroed(class: &Class) -> bool {
    matches!(class, Class::Tbss | Class::Bss | Class::Reserve(_))
}

/// The segment, name and flags of the section a region is in the file.
fn section_of(class: &Class) -> (Segment, &str, u32) {
    let code = macho::S_REGULAR.to_flags().0 | macho::S_ATTR_PURE_INSTRUCTIONS.0 | macho::S_ATTR_SOME_INSTRUCTIONS.0;
    let (segment, _) = place(class);
    let (name, flags) = match class {
        Class::Text => ("__text", code),
        Class::Patch => ("__blaze_patch", code),
        Class::Stubs => ("__blaze_stubs", code),
        Class::Plt => ("__stubs", code),
        Class::Veneers => ("__blaze_veneers", code),
        Class::Rodata => ("__const", macho::S_REGULAR.to_flags().0),
        Class::Synthetic(name) => (*name, macho::S_REGULAR.to_flags().0),
        Class::EhFrame => (
            "__eh_frame",
            macho::S_COALESCED.to_flags().0
                | macho::S_ATTR_NO_TOC.0
                | macho::S_ATTR_STRIP_STATIC_SYMS.0
                | macho::S_ATTR_LIVE_SUPPORT.0,
        ),
        Class::InitArray => ("__mod_init_func", macho::S_MOD_INIT_FUNC_POINTERS.to_flags().0),
        Class::PreinitArray => ("__preinit_array", macho::S_REGULAR.to_flags().0),
        Class::FiniArray => ("__mod_term_func", macho::S_MOD_TERM_FUNC_POINTERS.to_flags().0),
        Class::Got => ("__got", macho::S_NON_LAZY_SYMBOL_POINTERS.to_flags().0),
        Class::RelRo => ("__const", macho::S_REGULAR.to_flags().0),
        Class::Table => ("__blaze_table", macho::S_REGULAR.to_flags().0),
        Class::Data => ("__data", macho::S_REGULAR.to_flags().0),
        Class::PatchData => ("__blaze_pdata", macho::S_REGULAR.to_flags().0),
        Class::ThreadVars => ("__thread_vars", macho::S_THREAD_LOCAL_VARIABLES.to_flags().0),
        Class::Tdata => ("__thread_data", macho::S_THREAD_LOCAL_REGULAR.to_flags().0),
        Class::Tbss => ("__thread_bss", macho::S_THREAD_LOCAL_ZEROFILL.to_flags().0),
        Class::Bss => ("__bss", macho::S_ZEROFILL.to_flags().0),
        // A reservation's segment has no section ([`MachO::image`]): the name is for errors.
        Class::Reserve(_) => ("__reserved", macho::S_ZEROFILL.to_flags().0),
        Class::Named { name, .. } => {
            let (seg, sect) = split(name);
            let flags = match sect {
                "__thread_init" => macho::S_THREAD_LOCAL_INIT_FUNCTION_POINTERS.to_flags().0,
                "__cstring" => macho::S_CSTRING_LITERALS.to_flags().0,
                _ => macho::S_REGULAR.to_flags().0,
            };
            let sect = match (seg, sect) {
                ("__LD", "__compact_unwind") => "__blaze_unwind",
                _ => sect,
            };
            return (segment, sect, flags);
        }
    };
    (segment, name, flags)
}

/// Where a Mach-O section goes, by its segment, name and flags (`S_*` and `S_ATTR_*`): the
/// class and rank of what is loaded, or what else it is. The same name always gives the same
/// class, so `section$start$SEG$SECT` finds what an object or a `#[link_section]` put there.
pub(super) fn classify(segment: &str, section: &str, flags: u32) -> Result<SectionKind, String> {
    let alloc = |class| Ok(SectionKind::Alloc(class, u32::MAX));
    let named = |segment: &str, section: &str| {
        // Mach-O's names are at most 16 bytes each, so `SEG,SECT` fits inline.
        let mut text: SmallVec<[u8; 40]> = SmallVec::with_capacity(segment.len() + 1 + section.len());
        text.extend_from_slice(segment.as_bytes());
        text.push(b',');
        text.extend_from_slice(section.as_bytes());
        let name = std::str::from_utf8(&text).expect("two `str`s joined by a comma");
        Class::Named { name: SectionName::new(name), write: segment != "__TEXT", exec: false }
    };
    // Before the debug check: LLVM marks `__compact_unwind` `S_ATTR_DEBUG`.
    match (segment, section) {
        ("__LD", "__compact_unwind") => return Ok(SectionKind::Unwind(compact_unwind())),
        ("__TEXT", "__eh_frame") => return Ok(SectionKind::EhFrame),
        _ => {}
    }
    if segment == "__DWARF" || segment == "__LLVM" || flags & macho::S_ATTR_DEBUG.0 != 0 {
        return Ok(SectionKind::Dropped);
    }
    if Segment::of(segment).is_none() || segment == "__LD" {
        // alloc: error path
        return Err(format!(
            "the section `{segment},{section}` is in a segment blaze's Mach-O writer doesn't lay \
             out (it has `__TEXT`, `__DATA_CONST` and `__DATA`)"
        ));
    }
    let text = segment == "__TEXT";
    match macho::SectionFlags(flags).typ() {
        macho::S_MOD_INIT_FUNC_POINTERS => return alloc(Class::InitArray),
        macho::S_MOD_TERM_FUNC_POINTERS => return alloc(Class::FiniArray),
        macho::S_THREAD_LOCAL_REGULAR => return alloc(Class::Tdata),
        macho::S_THREAD_LOCAL_ZEROFILL => return alloc(Class::Tbss),
        macho::S_THREAD_LOCAL_VARIABLES => return alloc(Class::ThreadVars),
        macho::S_THREAD_LOCAL_INIT_FUNCTION_POINTERS => {
            return alloc(named("__DATA", "__thread_init"));
        }
        macho::S_ZEROFILL | macho::S_GB_ZEROFILL if !text => return alloc(Class::Bss),
        macho::S_CSTRING_LITERALS
        | macho::S_4BYTE_LITERALS
        | macho::S_8BYTE_LITERALS
        | macho::S_16BYTE_LITERALS
            if text =>
        {
            return alloc(Class::Rodata);
        }
        macho::S_NON_LAZY_SYMBOL_POINTERS
        | macho::S_LAZY_SYMBOL_POINTERS
        | macho::S_LAZY_DYLIB_SYMBOL_POINTERS
        | macho::S_SYMBOL_STUBS => {
            // alloc: error path
            return Err(format!(
                "the section `{segment},{section}` is of symbol stubs or pointers, which only a \
                 linker makes"
            ));
        }
        macho::S_DTRACE_DOF => {
            // alloc: error path
            return Err(format!("the section `{segment},{section}` holds DTrace probes (DOF)"));
        }
        // Laid out as their bytes by name below: the writer keeps no section type but these
        // need none (literals and coalesced sections are only deduplicated by ld64).
        macho::S_REGULAR
        | macho::S_COALESCED
        | macho::S_LITERAL_POINTERS
        | macho::S_CSTRING_LITERALS
        | macho::S_4BYTE_LITERALS
        | macho::S_8BYTE_LITERALS
        | macho::S_16BYTE_LITERALS => {}
        // dyld finds interposing tuples by this name as well as by the type.
        macho::S_INTERPOSING if (segment, section) == ("__DATA", "__interpose") => {}
        other => {
            // alloc: error path
            return Err(format!(
                "the section `{segment},{section}` has section type {:#x}, which blaze's Mach-O \
                 writer does not lay out",
                other.0
            ));
        }
    }
    match (segment, section) {
        ("__TEXT", "__text" | "__textcoal_nt" | "__StaticInit") => alloc(Class::Text),
        (
            "__TEXT",
            "__const" | "__cstring" | "__literal4" | "__literal8" | "__literal16" | "__ustring"
            | "__gcc_except_tab" | "__const_coal",
        ) => alloc(Class::Rodata),
        (_, "__mod_init_func") => alloc(Class::InitArray),
        (_, "__mod_term_func") => alloc(Class::FiniArray),
        ("__DATA", "__thread_data") => alloc(Class::Tdata),
        ("__DATA", "__thread_bss") => alloc(Class::Tbss),
        ("__DATA", "__thread_vars") => alloc(Class::ThreadVars),
        ("__DATA", "__thread_init") => alloc(named("__DATA", "__thread_init")),
        ("__DATA", "__data" | "__datacoal_nt") => alloc(Class::Data),
        ("__DATA" | "__DATA_CONST", "__const" | "__const_coal") => alloc(Class::RelRo),
        ("__DATA", "__bss" | "__common") => alloc(Class::Bss),
        _ => alloc(named(segment, section)),
    }
}

/// `S_*` of a section type as an assembler's `.section` and rustc's `#[link_section]` name it.
fn section_type(name: &str) -> Option<u32> {
    Some(match name {
        "regular" => macho::S_REGULAR.to_flags().0,
        "zerofill" => macho::S_ZEROFILL.to_flags().0,
        "cstring_literals" => macho::S_CSTRING_LITERALS.to_flags().0,
        "4byte_literals" => macho::S_4BYTE_LITERALS.to_flags().0,
        "8byte_literals" => macho::S_8BYTE_LITERALS.to_flags().0,
        "16byte_literals" => macho::S_16BYTE_LITERALS.to_flags().0,
        "literal_pointers" => macho::S_LITERAL_POINTERS.to_flags().0,
        "non_lazy_symbol_pointers" => macho::S_NON_LAZY_SYMBOL_POINTERS.to_flags().0,
        "lazy_symbol_pointers" => macho::S_LAZY_SYMBOL_POINTERS.to_flags().0,
        "symbol_stubs" => macho::S_SYMBOL_STUBS.to_flags().0,
        "mod_init_funcs" => macho::S_MOD_INIT_FUNC_POINTERS.to_flags().0,
        "mod_term_funcs" => macho::S_MOD_TERM_FUNC_POINTERS.to_flags().0,
        "coalesced" => macho::S_COALESCED.to_flags().0,
        "interposing" => macho::S_INTERPOSING.to_flags().0,
        "thread_local_regular" => macho::S_THREAD_LOCAL_REGULAR.to_flags().0,
        "thread_local_zerofill" => macho::S_THREAD_LOCAL_ZEROFILL.to_flags().0,
        "thread_local_variables" => macho::S_THREAD_LOCAL_VARIABLES.to_flags().0,
        "thread_local_variable_pointers" => macho::S_THREAD_LOCAL_VARIABLE_POINTERS.to_flags().0,
        "thread_local_init_function_pointers" => macho::S_THREAD_LOCAL_INIT_FUNCTION_POINTERS.to_flags().0,
        _ => return None,
    })
}

fn section_attribute(name: &str) -> Option<u32> {
    Some(match name {
        "pure_instructions" => macho::S_ATTR_PURE_INSTRUCTIONS.0,
        "no_toc" => macho::S_ATTR_NO_TOC.0,
        "strip_static_syms" => macho::S_ATTR_STRIP_STATIC_SYMS.0,
        "no_dead_strip" => macho::S_ATTR_NO_DEAD_STRIP.0,
        "live_support" => macho::S_ATTR_LIVE_SUPPORT.0,
        "self_modifying_code" => macho::S_ATTR_SELF_MODIFYING_CODE.0,
        "debug" => macho::S_ATTR_DEBUG.0,
        _ => return None,
    })
}

impl MachO {
    fn shared(&self) -> bool {
        matches!(self.output.kind, OutputKind::Shared { .. })
    }

    fn base(&self) -> u64 {
        if self.shared() { 0 } else { BASE }
    }

    /// The class of the section `segment,section`, when one is loaded.
    fn class_by_name(segment: &str, section: &str) -> Option<Class> {
        match classify(segment, section, macho::S_REGULAR.to_flags().0).ok()? {
            SectionKind::Alloc(class, _) | SectionKind::Unwind(class) => Some(class),
            SectionKind::EhFrame => Some(Class::EhFrame),
            SectionKind::Dropped => None,
        }
    }

    /// The architecture's slice of a universal file and where it starts, or the file.
    fn thin<'a>(&self, name: &str, bytes: &'a [u8]) -> Result<(usize, &'a [u8]), String> {
        use object::read::macho::{FatArch, MachOFatFile32, MachOFatFile64};
        let magic = bytes.get(..4).map(|it| u32::from_be_bytes([it[0], it[1], it[2], it[3]]));
        let slices: Vec<(u32, u64, u64)> = match magic {
            Some(macho::FAT_MAGIC) => MachOFatFile32::parse(bytes)
                // alloc: error path
                .map_err(|it| format!("{name}: {it}"))?
                .arches()
                .iter()
                .map(|it| (it.cputype().0, it.offset().into(), it.size().into()))
                // alloc: once per universal file: its slices
                .collect(),
            Some(macho::FAT_MAGIC_64) => MachOFatFile64::parse(bytes)
                // alloc: error path
                .map_err(|it| format!("{name}: {it}"))?
                .arches()
                .iter()
                .map(|it| (it.cputype().0, it.offset(), it.size()))
                // alloc: once per universal file: its slices
                .collect(),
            _ => return Ok((0, bytes)),
        };
        let Some(&(_, offset, size)) = slices.iter().find(|it| it.0 == self.cpu) else {
            // alloc: error path
            let has: Vec<String> = slices.iter().map(|it| format!("{:#x}", it.0)).collect();
            // alloc: error path
            return Err(format!(
                "{name} is a universal file without a slice for CPU type {:#x} (it has {})",
                self.cpu,
                has.join(", ")
            ));
        };
        let (start, end) = (offset as usize, offset.saturating_add(size) as usize);
        let slice = bytes
            .get(start..end)
            // alloc: error path
            .ok_or_else(|| format!("{name}: a universal file's slice past its end"))?;
        Ok((start, slice))
    }
}

impl Writer for MachO {
    fn format(&self) -> Format {
        Format::MachO
    }

    fn arch(&self) -> Arch {
        self.output.arch
    }

    fn plan(&self) -> Plan {
        Plan {
            base: self.base(),
            headers: HEADERS,
            page: self.page,
            indexed_code: false,
            got_by_library: false,
            tls_descriptors: true,
        }
    }

    fn order(&self, classes: &mut Vec<Class>) {
        classes.sort_by(|a, b| place(a).cmp(&place(b)).then_with(|| a.cmp(b)));
    }

    fn prot(&self, class: &Class) -> Prot {
        place(class).0.prot()
    }

    fn section(
        &self,
        name: &str,
        write: bool,
        exec: bool,
        tls: bool,
    ) -> Result<(Class, u32), String> {
        let _ = (write, exec);
        let mut parts = name.split(',').map(str::trim);
        let segment = parts.next().unwrap_or_default();
        let shape = || {
            // alloc: error path
            format!(
                "`{name}` is not a Mach-O section: `SEGMENT,section[,type[,attributes]]`, each \
                 name of 1 to 16 bytes"
            )
        };
        let Some(section) = parts.next() else { return Err(shape()) };
        if [segment, section].iter().any(|it| it.is_empty() || it.len() > 16) {
            return Err(shape());
        }
        let mut flags = match parts.next() {
            None => macho::S_REGULAR.to_flags().0,
            Some(kind) => {
                // alloc: error path
                section_type(kind).ok_or_else(|| format!("`{name}`: no section type `{kind}`"))?
            }
        };
        if let Some(attributes) = parts.next() {
            for attribute in attributes.split('+').map(str::trim) {
                flags |= section_attribute(attribute)
                    // alloc: error path
                    .ok_or_else(|| format!("`{name}`: no section attribute `{attribute}`"))?;
            }
        }
        // A stub size, after the attributes, only `symbol_stubs` has, which is refused below.
        if tls {
            let zero = matches!(macho::SectionFlags(flags).typ(), macho::S_THREAD_LOCAL_ZEROFILL)
                || section == "__thread_bss";
            return Ok((if zero { Class::Tbss } else { Class::Tdata }, u32::MAX));
        }
        match classify(segment, section, flags)? {
            SectionKind::Alloc(class, rank) => Ok((class, rank)),
            // alloc: error path
            _ => Err(format!("`{name}` is not a section a static can be placed in")),
        }
    }

    fn start_hook(&self) -> Result<(Class, u32), String> {
        // Below the `u32::MAX` that [`classify`] gives every input's and `#[link_section]`'s
        // `__mod_init_func`, so the hook is the first pointer dyld calls of this image.
        Ok((Class::InitArray, 0))
    }

    fn special(&self, name: &str) -> Option<Special> {
        match name {
            "_mh_execute_header" if !self.shared() => return Some(Special::ImageBase),
            "_mh_dylib_header" if self.shared() => return Some(Special::ImageBase),
            "__dso_handle" => return Some(Special::ImageBase),
            "__blaze_live" => return Some(Special::LiveDescriptor),
            _ => {}
        }
        let name = name.strip_prefix('\x01').unwrap_or(name);
        for (prefix, end) in [("section$start$", false), ("section$end$", true)] {
            let Some((segment, section)) = name.strip_prefix(prefix).and_then(|it| it.split_once('$'))
            else {
                continue;
            };
            return match Self::class_by_name(segment, section)? {
                Class::Named { name, .. } => Some(Special::Section { name, end }),
                class if end => Some(Special::Stop(class)),
                class => Some(Special::Start(class)),
            };
        }
        let at: fn(&[Region]) -> u64 = match (name, self.page) {
            ("segment$start$__TEXT", _) => return Some(Special::ImageBase),
            ("segment$end$__TEXT", 0x4000) => segment_bound::<0, true, 0x4000>,
            ("segment$end$__TEXT", _) => segment_bound::<0, true, 0x1000>,
            ("segment$start$__DATA_CONST", 0x4000) => segment_bound::<1, false, 0x4000>,
            ("segment$start$__DATA_CONST", _) => segment_bound::<1, false, 0x1000>,
            ("segment$end$__DATA_CONST", 0x4000) => segment_bound::<1, true, 0x4000>,
            ("segment$end$__DATA_CONST", _) => segment_bound::<1, true, 0x1000>,
            ("segment$start$__DATA", 0x4000) => segment_bound::<2, false, 0x4000>,
            ("segment$start$__DATA", _) => segment_bound::<2, false, 0x1000>,
            ("segment$end$__DATA", 0x4000) => segment_bound::<2, true, 0x4000>,
            ("segment$end$__DATA", _) => segment_bound::<2, true, 0x1000>,
            _ => return None,
        };
        Some(Special::Placed(at))
    }

    fn read(&self, name: &str, bytes: &[u8], kind: &NativeKind) -> Result<NativeFile, String> {
        let (at, bytes) = self.thin(name, bytes)?;
        let magic = bytes.get(..4).map(|it| u32::from_le_bytes([it[0], it[1], it[2], it[3]]));
        match kind {
            NativeKind::Object => {
                let mut object = input::read(name, bytes, self.output.arch)?;
                // The reader's ranges are of the slice it read: of this file, past a universal
                // header.
                if at != 0 {
                    for section in &mut object.sections {
                        if let Data::Input(range) = &mut section.data {
                            *range = range.start + at as u64..range.end + at as u64;
                        }
                    }
                }
                Ok(NativeFile::Object(object))
            }
            NativeKind::Archive { .. } => {
                let mut archive = archive::read(name, bytes)?;
                for member in &mut archive.members {
                    member.bytes = member.bytes.start + at..member.bytes.end + at;
                }
                Ok(NativeFile::Archive(archive))
            }
            NativeKind::Shared { .. } if magic == Some(macho::MH_MAGIC_64) => {
                dylib::read(name, bytes, self.output.arch).map(NativeFile::Shared)
            }
            NativeKind::Shared { .. } => {
                let text = std::str::from_utf8(bytes).map_err(|_| {
                    // alloc: error path
                    format!("{name} is neither a Mach-O dynamic library nor a `.tbd` stub")
                })?;
                tbd::read(name, text, self.output.arch, self.platform.platform)
                    .map(NativeFile::Shared)
            }
        }
    }

    fn synthetic(&self, counts: &Counts) -> Vec<(Class, u64)> {
        // alloc: `Writer::synthetic` returns a `Vec`: one entry, once per link
        vec![(Class::Synthetic(UNWIND_INFO), unwind::bound(counts.functions, counts.fdes, counts.unwind))]
    }

    fn apply(
        &self,
        r_type: u32,
        bytes: &mut [u8],
        p: u64,
        s: u64,
        addend: i64,
    ) -> Result<(), String> {
        let _ = (bytes, s, addend);
        // alloc: error path
        Err(format!(
            "a Mach-O relocation of type {r_type} at {p:#x}: every Mach-O relocation is read into \
             one the core applies"
        ))
    }

    fn synthesize(&self, placed: &Placed<'_>) -> Result<Vec<(Class, CowBytes)>, LinkError> {
        let unwind = match &self.cache {
            Some(cache) => unwind::build_kept(placed, self.output.arch, &mut lock(cache).unwind)?,
            // alloc: once per link, the table's `Arc`: an image linked once keeps nothing
            None => CowBytes::from_vec(unwind::build(placed, self.output.arch)?),
        };
        // alloc: `Writer::synthesize` returns a `Vec`: one entry, once per link
        Ok(vec![(Class::Synthetic(UNWIND_INFO), unwind)])
    }

    fn encode(&self, placed: &Placed<'_>) -> Result<Vec<(u64, u64)>, LinkError> {
        self.check_placed(placed)?;
        if !chained::chained(&self.platform) {
            // alloc: none: an empty `Vec` holds no buffer
            return Ok(Vec::new());
        }
        chained::encode(placed, self.page)
    }

    fn offset(&self, placed: &Placed<'_>, region: &Region) -> Option<u64> {
        (!zeroed(&region.class)).then(|| region.addr - placed.plan.base)
    }

    fn file(&self, placed: &Placed<'_>) -> Result<File, LinkError> {
        self.image(placed)
    }
}

// The readers view an input in place wherever it starts (an archive's member starts where
// its header and name end, at any byte): every structure `object` reads is byte-aligned.
const _: () = {
    use std::mem::align_of;
    type E = object::LittleEndian;
    assert!(align_of::<macho::FatHeader>() == 1 && align_of::<macho::FatArch64>() == 1);
    assert!(align_of::<macho::MachHeader64<E>>() == 1 && align_of::<macho::LoadCommand<E>>() == 1);
    assert!(align_of::<macho::SegmentCommand64<E>>() == 1 && align_of::<macho::Section64<E>>() == 1);
    assert!(align_of::<macho::SymtabCommand<E>>() == 1 && align_of::<macho::Nlist64<E>>() == 1);
    assert!(align_of::<macho::Relocation<E>>() == 1 && align_of::<macho::DylibCommand<E>>() == 1);
    assert!(align_of::<macho::DyldInfoCommand<E>>() == 1);
    assert!(align_of::<macho::LinkeditDataCommand<E>>() == 1);
};

/// A segment as its load command says it.
struct SegmentOut {
    name: &'static str,
    vmaddr: u64,
    vmsize: u64,
    fileoff: u64,
    filesize: u64,
    prot: u32,
    flags: u32,
    sections: Vec<SectionOut>,
}

struct SectionOut {
    /// As the load command holds it: NUL-padded to 16 bytes.
    name: [u8; 16],
    segment: &'static str,
    addr: u64,
    size: u64,
    offset: u32,
    align: u32,
    flags: u32,
    /// `__got`: its first entry in the indirect symbol table.
    indirect: u32,
}

/// A little-endian writer of load commands and tables.
#[derive(Default)]
struct Out(Vec<u8>);

impl Out {
    fn name16(&mut self, name: &str) {
        self.0.extend_from_slice(&field16(name));
    }
}

impl std::ops::Deref for Out {
    type Target = Vec<u8>;
    fn deref(&self) -> &Vec<u8> {
        &self.0
    }
}

impl std::ops::DerefMut for Out {
    fn deref_mut(&mut self) -> &mut Vec<u8> {
        &mut self.0
    }
}

fn page_up(value: u64, page: u64) -> u64 {
    value.next_multiple_of(page)
}

/// `segment$start$SEG` (`END` false) or `segment$end$SEG` of the segment `SEGMENT` in
/// [`Segment`] order: where its first region starts, or the page after its last region's room.
/// A segment with no regions is empty, both where it would start.
fn segment_bound<const SEGMENT: u8, const END: bool, const PAGE: u64>(regions: &[Region]) -> u64 {
    let order = |it: &&Region| place(&it.class).0 as u8;
    let end_of = |it: &Region| page_up(it.addr + it.capacity, PAGE);
    let mine = || regions.iter().filter(|it| order(it) == SEGMENT);
    match (mine().map(|it| it.addr).min(), mine().map(end_of).max()) {
        (Some(start), Some(end)) => if END { end } else { start },
        _ => (regions.iter().filter(|it| order(it) < SEGMENT).map(end_of).max())
            .unwrap_or(BASE + HEADERS),
    }
}

fn u32_of(value: u64, what: &str) -> Result<u32, LinkError> {
    // alloc: error path
    u32::try_from(value).map_err(|_| error(format!("{what} ({value:#x}) doesn't fit 32 bits")))
}

/// A segment or section name as a load command holds it; `name` is at most 16 bytes.
fn field16(name: &str) -> [u8; 16] {
    let mut field = [0u8; 16];
    field[..name.len()].copy_from_slice(name.as_bytes());
    field
}

/// `__debug_line` for `.debug_line`, at most 16 bytes, as Apple's tools name them.
fn dwarf_name(name: &str) -> [u8; 16] {
    let mut field = [0u8; 16];
    field[..2].copy_from_slice(b"__");
    let rest = name.trim_start_matches('.').as_bytes();
    let len = rest.len().min(14);
    field[2..2 + len].copy_from_slice(&rest[..len]);
    field
}

/// What the symbol table and the indirect symbol table say.
struct Symbols {
    nlist: Vec<u8>,
    strings: Vec<u8>,
    count: u32,
    locals: u32,
    defined: u32,
    undefined: u32,
    /// Each import's Mach-O name in `strings` (offset, length), by import.
    import_names: Vec<(u32, u32)>,
    /// Each import's symbol, by import: what the indirect symbol table names.
    index_of: Vec<u32>,
    /// What the export trie holds: the defined symbols in name order, each its name in
    /// `strings`, its `EXPORT_SYMBOL_FLAGS_*` and its offset from the image's start.
    exports: Vec<((u32, u32), u64, u64)>,
    /// By function (in `Placed::functions`), its local's entry or `u32::MAX` for none:
    /// filled for an image linked again in place, whose locals are in the functions' order.
    locals_of: Vec<u32>,
}

impl MachO {
    fn image(&self, placed: &Placed<'_>) -> Result<File, LinkError> {
        let base = placed.plan.base;
        let page = self.page;
        let shared = self.shared();

        // The segments of the address space, and the sections of the regions in them.
        // `__PAGEZERO`, the six of `Segment`, `__DWARF` and `__LINKEDIT`.
        let mut segments: Vec<SegmentOut> = Vec::with_capacity(9);
        if !shared {
            segments.push(SegmentOut {
                name: "__PAGEZERO",
                vmaddr: 0,
                vmsize: base,
                fileoff: 0,
                filesize: 0,
                prot: 0,
                flags: 0,
                sections: Vec::new(), // alloc: empty, which doesn't allocate
            });
        }
        let mut has_tlv = false;
        for segment in
            [Segment::Text, Segment::DataConst, Segment::Data, Segment::Arena, Segment::Cells, Segment::Fixed]
        {
            let regions =
                || (placed.regions.iter().enumerate()).filter(move |(_, it)| place(&it.class).0 == segment);
            let first = regions().next();
            if first.is_none() && segment != Segment::Text {
                continue;
            }
            let vmaddr = match (segment, first) {
                (Segment::Text, _) => base,
                (_, Some((_, first))) => first.addr,
                (_, None) => unreachable!("only `__TEXT` is kept without regions"),
            };
            if vmaddr % page != 0 {
                // alloc: error path
                return Err(error(format!(
                    "the segment {} starts at {vmaddr:#x}, which is not at a page",
                    segment.name()
                )));
            }
            let start = match segment {
                Segment::Text => base + placed.plan.headers,
                _ => vmaddr,
            };
            let (mut vmend, mut filend) = (start, start);
            let mut zero_seen = None;
            let mut sections = Vec::with_capacity(if segment.reserves() { 0 } else { regions().count() });
            for (index, region) in regions() {
                let end = region.addr + region.capacity;
                if region.addr < start {
                    // alloc: error path
                    return Err(error(format!(
                        "the region of {:?} at {:#x} is before its segment {} (at {start:#x})",
                        region.class,
                        region.addr,
                        segment.name()
                    )));
                }
                if let Some(last) = segments.last().filter(|_| segment != Segment::Text)
                    && region.addr < last.vmaddr + last.vmsize
                {
                    // alloc: error path
                    return Err(error(format!(
                        "the region of {:?} at {:#x} overlaps the segment {}",
                        region.class, region.addr, last.name
                    )));
                }
                if region.size > region.capacity
                    || placed.len(index) as u64 > region.capacity
                {
                    // alloc: error path
                    return Err(error(format!(
                        "the region of {:?} at {:#x} holds more than its capacity",
                        region.class, region.addr
                    )));
                }
                vmend = vmend.max(end);
                if zeroed(&region.class) {
                    zero_seen.get_or_insert(region.addr);
                    if placed.len(index) != 0 {
                        // alloc: error path
                        return Err(error(format!(
                            "the zeroed region of {:?} at {:#x} has bytes",
                            region.class, region.addr
                        )));
                    }
                } else {
                    if let Some(zero) = zero_seen {
                        // alloc: error path
                        return Err(error(format!(
                            "the region of {:?} at {:#x} is behind zeroed data at {zero:#x}: \
                             the file has no bytes there",
                            region.class, region.addr
                        )));
                    }
                    filend = filend.max(end);
                }
                if segment.reserves() {
                    continue;
                }
                let offset = match zeroed(&region.class) {
                    true => 0,
                    false => u32_of(region.addr - base, "a section's file offset")?,
                };
                let align = region.align.max(1).trailing_zeros();
                let (_, name, flags) = section_of(&region.class);
                has_tlv |= flags == macho::S_THREAD_LOCAL_VARIABLES.to_flags().0 && region.size != 0;
                sections.push(SectionOut {
                    name: field16(name),
                    segment: segment.name(),
                    addr: region.addr,
                    size: region.size,
                    offset,
                    align,
                    flags,
                    indirect: 0,
                });
            }
            let vmend = page_up(vmend, page);
            let filesize = (page_up(filend, page) - vmaddr).min(vmend - vmaddr);
            segments.push(SegmentOut {
                name: segment.name(),
                vmaddr,
                vmsize: vmend - vmaddr,
                // A segment without file bytes at offset 0, as `__PAGEZERO`: its address less
                // the base is past the file's end, which dyld refuses.
                fileoff: if segment.reserves() { 0 } else { vmaddr - base },
                filesize,
                prot: segment.vm_prot(),
                flags: if segment == Segment::DataConst { macho::SG_READ_ONLY.0 } else { 0 },
                sections,
            });
        }
        let last = segments.last().expect("`__TEXT` is always there");
        let mut vm_next = last.vmaddr + last.vmsize;
        let mut file_next = (segments.iter())
            .map(|it| page_up(it.fileoff + it.filesize, page))
            .max()
            .unwrap_or(0);

        // The debug info, in a segment no one runs: the core's sections where `File::debug`
        // says, which the file lays out without a copy here.
        let debug = || placed.debug.iter().enumerate().filter(|it| !it.1.1.is_empty());
        let mut debug_at: Vec<(u64, usize)> = Vec::with_capacity(debug().count());
        if debug().next().is_some() {
            let (vmaddr, fileoff) = (vm_next, file_next);
            let mut sections = Vec::with_capacity(debug().count());
            let mut filesize = 0u64;
            for (nth, (name, bytes)) in debug() {
                sections.push(SectionOut {
                    name: dwarf_name(name),
                    segment: "__DWARF",
                    addr: vmaddr + filesize,
                    size: bytes.len() as u64,
                    offset: u32_of(fileoff + filesize, "a debug section's offset")?,
                    align: 0,
                    flags: macho::S_REGULAR.to_flags().0 | macho::S_ATTR_DEBUG.0,
                    indirect: 0,
                });
                debug_at.push((fileoff + filesize, nth));
                filesize += bytes.len() as u64;
            }
            segments.push(SegmentOut {
                name: "__DWARF",
                vmaddr,
                vmsize: page_up(filesize, page),
                fileoff,
                filesize,
                prot: macho::VM_PROT_READ.0,
                flags: 0,
                sections,
            });
            vm_next = page_up(vmaddr + filesize, page);
            file_next = page_up(fileoff + filesize, page);
        }

        // Which libraries are loaded, and each one's ordinal.
        // alloc: two tables per link, each sized once by the libraries
        let mut used = vec![(0usize, 0usize); placed.libraries.len()];
        for import in placed.imports {
            if let Some(library) = import.library {
                let it = used.get_mut(library as usize).ok_or_else(|| {
                    // alloc: error path
                    error(format!("the import {} is of no library ({library})", import.name))
                })?;
                it.0 += 1;
                it.1 += usize::from(import.weak);
            }
        }
        let mut ordinals = vec![0u32; placed.libraries.len()]; // alloc: as `used`
        let mut loaded = Vec::with_capacity(placed.libraries.len());
        for (nth, library) in placed.libraries.iter().enumerate() {
            if library.as_needed && used[nth].0 == 0 {
                continue;
            }
            loaded.push((library, used[nth].0 != 0 && used[nth].0 == used[nth].1));
            ordinals[nth] = loaded.len() as u32;
        }
        if loaded.len() > 253 {
            // alloc: error path
            return Err(error(format!(
                "{} libraries: a two-level namespace image names at most 253",
                loaded.len()
            )));
        }
        let ordinal = |import: u32| -> Result<i64, LinkError> {
            let import = placed
                .imports
                .get(import as usize)
                // alloc: error path
                .ok_or_else(|| error(format!("a bind of no import ({import})")))?;
            Ok(match import.library {
                Some(library) => i64::from(ordinals[library as usize]),
                None => i64::from(macho::BIND_SPECIAL_DYLIB_FLAT_LOOKUP.0),
            })
        };

        // What the loader fixes up.
        let mut spans: Vec<linkedit::Span> = Vec::with_capacity(segments.len());
        spans.extend((segments.iter()).filter(|it| it.name != "__DWARF").map(|it| linkedit::Span {
            name: it.name,
            start: it.vmaddr,
            end: it.vmaddr + it.vmsize,
            writable: it.prot & macho::VM_PROT_WRITE.0 != 0,
        }));
        let chained = chained::chained(&self.platform);
        if !chained && !placed.encoded.is_empty() {
            return Err(error("chained slots for an image fixed up by opcodes"));
        }

        // What an incremental image kept from its last link, and whether this link's changes
        // are from it: the same regions, and the core's synthesis the kept tables are of.
        let mut kept = self.cache.as_ref().map(lock);
        let last = kept.as_deref().map_or(0, |it| it.synthesis);
        if let Some(kept) = kept.as_deref_mut() {
            kept.synthesis = 0;
        }
        let delta = placed.delta.filter(|it| last != 0 && it.from == last && it.same_regions);

        // The symbol table first: its strings are the names the binds and the trie use.
        let mut sections: Vec<&mut SectionOut> =
            Vec::with_capacity(segments.iter().map(|it| it.sections.len()).sum());
        sections.extend(segments.iter_mut().flat_map(|it| it.sections.iter_mut()));
        let ranges = section_ranges(&sections)?;
        let mut symbols = kept.as_deref_mut().and_then(|it| it.symbols.take());
        let patched = match (&mut symbols, delta) {
            (Some(symbols), Some(delta)) => {
                delta.same_names
                    && delta.patches(last, &[delta.functions])
                    && patch_locals(symbols, placed, delta, &ranges)?
            }
            _ => false,
        };
        let symbols = match symbols {
            Some(symbols) if patched => symbols,
            _ => self.symbols(placed, &ranges, &ordinals, kept.is_some())?,
        };
        let (indirect, indirect_count) = indirect(placed, &mut sections, &symbols.index_of)?;
        let name_at = |(at, len): (u32, u32)| &symbols.strings[at as usize..(at + len) as usize];
        // Each segment before `__LINKEDIT`, (start, the size of its bytes in the file): the
        // chained fixups' page starts. dyld's page-in linking maps every page they count from the
        // file, so a zero-fill page counted is mapped over with the bytes after the segment.
        // alloc: none: at most eight segments, inline
        let laid: SmallVec<[(u64, u64); 8]> = segments.iter().map(|it| (it.vmaddr, it.filesize)).collect();

        // alloc: none for a chained image, whose binds are its slots
        let mut binds = Vec::with_capacity(if chained { 0 } else { placed.binds.len() + placed.got.len() });
        for bind in placed.binds.iter().filter(|_| !chained) {
            let import = placed.imports.get(bind.import as usize);
            binds.push(linkedit::Bind {
                addr: bind.addr,
                ordinal: ordinal(bind.import)?,
                name: name_at(symbols.import_names[bind.import as usize]),
                weak: import.is_some_and(|it| it.weak),
                addend: bind.addend,
            });
        }
        for cell in placed.got.iter().filter(|_| !chained) {
            match &cell.holds {
                Holds::Local(_) => {}
                &Holds::Import(import) => binds.push(linkedit::Bind {
                    addr: cell.addr,
                    ordinal: ordinal(import)?,
                    name: name_at(symbols.import_names[import as usize]),
                    weak: placed.imports[import as usize].weak,
                    addend: 0,
                }),
                other => {
                    // alloc: error path
                    return Err(error(format!(
                        "a GOT cell at {:#x} holding {other:?}, which is ELF's and not Mach-O's",
                        cell.addr
                    )));
                }
            }
        }
        // What the image gives other images: what the symbol table defines, in its order.
        let mut exports: Vec<(&[u8], u64, u64)> = (symbols.exports.iter())
            .map(|&(name, flags, offset)| (name_at(name), flags, offset))
            // alloc: one buffer per link, of exactly the exports
            .collect();

        // `__LINKEDIT`, laid out as ld64 does: the signature last, at 16 bytes. A table whose
        // inputs this link didn't change is the kept one's bytes.
        let linkedit_at = file_next;
        let fixups_len = match chained {
            true => {
                // Only a segment dyld writes has slots, and so page starts: never a reservation's
                // gigabytes of pages.
                let pages = (segments.iter())
                    .filter(|it| it.prot & macho::VM_PROT_WRITE.0 != 0)
                    .map(|it| it.filesize.div_ceil(chained::PAGE) as usize)
                    .sum::<usize>();
                let names = placed.imports.iter().map(|it| it.name.len() + 2).sum::<usize>();
                64 + 40 * laid.len() + 2 * pages + 8 * placed.imports.len() + names
            }
            false => 0,
        };
        let rebases = if chained { 0 } else { placed.rebase.len() };
        // An incremental image builds it in its last link's buffer: see [`linkedit_part`].
        let mut linkedit = Out(kept.as_deref_mut().map(|it| std::mem::take(&mut it.scratch)).unwrap_or_default());
        linkedit.0.clear();
        // alloc: once per link, `__LINKEDIT`'s bytes; none for a relink in place after the
        // first, whose buffer is the last one's
        linkedit.0.reserve(
            linkedit::capacity(rebases, &binds, &exports, placed.functions.len())
                + fixups_len
                + symbols.nlist.len()
                + indirect.len()
                + symbols.strings.len()
                + 64,
        );
        let reuse = [
            delta.is_some_and(|it| it.rebase.is_empty()),
            delta.is_some_and(|it| it.same_names && it.binds.is_empty() && it.got.is_empty()),
            delta.is_some_and(|it| it.same_names),
            delta.is_some_and(|it| it.functions.is_empty()),
        ];
        // A kept table's (start, length) in the kept `__LINKEDIT`.
        let kept_table = |nth: usize| -> Option<(usize, usize)> {
            let kept = kept.as_deref().filter(|it| reuse[nth] && it.linkedit.is_some())?;
            Some(kept.tables[nth])
        };
        let at_of = |start: usize, len: usize| -> Result<(u32, u32), LinkError> {
            if len == 0 {
                return Ok((0, 0));
            }
            Ok((u32_of(linkedit_at + start as u64, "`__LINKEDIT`'s offsets")?, len as u32))
        };
        // Each kept table's (start, length) in `linkedit`: rebases, binds, trie, starts; and
        // each reused one's in the kept `__LINKEDIT`, left zeros in `linkedit`.
        let mut tables = [(0usize, 0usize); 4];
        let mut reused = [None; 4];
        let out = &mut linkedit.0;
        let mut fixups_at = (0, 0);
        if chained {
            let start = out.len();
            let name = |import: u32| name_at(symbols.import_names[import as usize]);
            let len = chained::fixups(placed, placed.encoded, &laid, &ordinal, name, out)?;
            fixups_at = at_of(start, len)?;
        } else {
            let start = out.len();
            reused[0] = kept_table(0);
            let len = match reused[0] {
                Some((_, len)) => room(out, len),
                None => linkedit::rebases(placed.rebase, &spans, out).map_err(error)?,
            };
            tables[0] = (start, len);
            let start = out.len();
            reused[1] = kept_table(1);
            let len = match reused[1] {
                Some((_, len)) => room(out, len),
                None => linkedit::binds(&mut binds, &spans, out).map_err(error)?,
            };
            tables[1] = (start, len);
        }
        let start = out.len();
        reused[2] = kept_table(2);
        let len = match reused[2] {
            Some((_, len)) => room(out, len),
            None => linkedit::trie(&mut exports, out).map_err(error)?,
        };
        tables[2] = (start, len);
        let start = out.len();
        reused[3] = kept_table(3);
        let len = match reused[3] {
            Some((_, len)) => room(out, len),
            None => {
                // alloc: one buffer per link, of exactly the functions
                let starts: Vec<u64> = par::map(placed.functions, |it| it.addr);
                linkedit::function_starts(base, &starts, out)
            }
        };
        tables[3] = (start, len);
        let [rebase_at, bind_at, trie_at, starts_at] =
            [tables[0], tables[1], tables[2], tables[3]].map(|(start, len)| at_of(start, len));
        let (rebase_at, bind_at, trie_at, starts_at) = (rebase_at?, bind_at?, trie_at?, starts_at?);
        let in_code_at = (u32_of(linkedit_at + out.len() as u64, "`__LINKEDIT`")?, 0);
        let mut append = |bytes: &[u8]| -> Result<(u32, u32), LinkError> {
            let at = at_of(out.len(), bytes.len())?;
            out.extend_from_slice(bytes);
            out.pad_to(8);
            Ok(at)
        };
        let nlist_at = append(&symbols.nlist)?;
        let indirect_at = append(&indirect)?;
        let strings_at = append(&symbols.strings)?;
        linkedit.pad_to(16);
        let linkedit = linkedit_part(kept.as_deref_mut(), linkedit.0, &tables, &reused);
        let code_limit = linkedit_at + linkedit.len() as u64;
        let identifier = placed.name;
        if identifier.is_empty() || identifier.contains(['/', '\0']) {
            // alloc: error path
            return Err(error(format!(
                "`{identifier}` is not a file name, which the code signature's identifier is"
            )));
        }
        let signature_len = sign::len(identifier, code_limit);
        let linkedit_size = linkedit.len() as u64 + signature_len;
        segments.push(SegmentOut {
            name: "__LINKEDIT",
            vmaddr: vm_next,
            vmsize: page_up(linkedit_size, page),
            fileoff: linkedit_at,
            filesize: linkedit_size,
            prot: macho::VM_PROT_READ.0,
            flags: 0,
            sections: Vec::new(), // alloc: empty, which doesn't allocate
        });
        if self.output.dev_run.is_some() {
            code_alone(&segments, page)?;
        }

        // The load commands.
        let mut commands = Out::default();
        let mut count = 0u32;
        for segment in &segments {
            count += 1;
            commands.put_u32(macho::LC_SEGMENT_64.0);
            commands.put_u32((SEGMENT + SECTION * segment.sections.len()) as u32);
            commands.name16(segment.name);
            commands.put_u64(segment.vmaddr);
            commands.put_u64(segment.vmsize);
            commands.put_u64(segment.fileoff);
            commands.put_u64(segment.filesize);
            commands.put_u32(segment.prot);
            commands.put_u32(segment.prot);
            commands.put_u32(segment.sections.len() as u32);
            commands.put_u32(segment.flags);
            for section in &segment.sections {
                commands.extend_from_slice(&section.name);
                commands.name16(section.segment);
                commands.put_u64(section.addr);
                commands.put_u64(section.size);
                commands.put_u32(section.offset);
                commands.put_u32(section.align);
                commands.put_u32(0);
                commands.put_u32(0);
                commands.put_u32(section.flags);
                commands.put_u32(section.indirect);
                commands.put_u32(0);
                commands.put_u32(0);
            }
        }
        if chained {
            for (command, (at, len)) in
                [(macho::LC_DYLD_CHAINED_FIXUPS.0, fixups_at), (macho::LC_DYLD_EXPORTS_TRIE.0, trie_at)]
            {
                count += 1;
                commands.put_u32(command);
                commands.put_u32(16);
                commands.put_u32(at);
                commands.put_u32(len);
            }
        } else {
            count += 1;
            commands.put_u32(macho::LC_DYLD_INFO_ONLY.0);
            commands.put_u32(48);
            for (at, len) in [rebase_at, bind_at, (0, 0), (0, 0), trie_at] {
                commands.put_u32(at);
                commands.put_u32(len);
            }
        }
        count += 1;
        commands.put_u32(macho::LC_SYMTAB.0);
        commands.put_u32(24);
        commands.put_u32(nlist_at.0);
        commands.put_u32(symbols.count);
        commands.put_u32(strings_at.0);
        commands.put_u32(strings_at.1);
        count += 1;
        commands.put_u32(macho::LC_DYSYMTAB.0);
        commands.put_u32(80);
        for field in [
            0,
            symbols.locals,
            symbols.locals,
            symbols.defined,
            symbols.locals + symbols.defined,
            symbols.undefined,
            0,
            0,
            0,
            0,
            0,
            0,
            indirect_at.0,
            indirect_count,
            0,
            0,
            0,
            0,
        ] {
            commands.put_u32(field);
        }
        match &self.output.kind {
            OutputKind::Executable { .. } => {
                count += 1;
                path_command(&mut commands, macho::LC_LOAD_DYLINKER.0, 12, "/usr/lib/dyld", &[]);
            }
            OutputKind::Shared { name, .. } => {
                count += 1;
                path_command(&mut commands, macho::LC_ID_DYLIB.0, 24, name, &[1, 0, 0]);
            }
        }
        count += 1;
        commands.put_u32(macho::LC_UUID.0);
        commands.put_u32(24);
        commands.0.extend_from_slice(&placed.id);
        count += 1;
        commands.put_u32(macho::LC_BUILD_VERSION.0);
        commands.put_u32(24);
        commands.put_u32(self.platform.platform);
        commands.put_u32(self.platform.minimum);
        commands.put_u32(self.platform.sdk);
        commands.put_u32(0);
        count += 1;
        commands.put_u32(macho::LC_SOURCE_VERSION.0);
        commands.put_u32(16);
        commands.put_u64(0);
        if !shared {
            let entry = placed.entry.ok_or_else(|| error("an executable without an entry"))?;
            let text = &segments[1];
            if !(text.vmaddr..text.vmaddr + text.vmsize).contains(&entry) {
                // alloc: error path
                return Err(error(format!("the entry {entry:#x} is not in `__TEXT`")));
            }
            count += 1;
            commands.put_u32(macho::LC_MAIN.0);
            commands.put_u32(24);
            commands.put_u64(entry - base);
            commands.put_u64(self.output.stack.unwrap_or(0));
        }
        for (library, weak) in &loaded {
            count += 1;
            let command = if *weak { macho::LC_LOAD_WEAK_DYLIB.0 } else { macho::LC_LOAD_DYLIB.0 };
            path_command(&mut commands, command, 24, &library.name, &[2, 0, 0]);
        }
        for rpath in &self.output.rpaths {
            count += 1;
            path_command(&mut commands, macho::LC_RPATH.0, 12, rpath, &[]);
        }
        for (command, (at, len)) in [
            (macho::LC_FUNCTION_STARTS.0, starts_at),
            (macho::LC_DATA_IN_CODE.0, in_code_at),
        ] {
            count += 1;
            commands.put_u32(command);
            commands.put_u32(16);
            commands.put_u32(at);
            commands.put_u32(len);
        }
        count += 1;
        commands.put_u32(macho::LC_CODE_SIGNATURE.0);
        commands.put_u32(16);
        commands.put_u32(u32_of(code_limit, "the code signature's offset")?);
        commands.put_u32(u32_of(signature_len, "the code signature's length")?);

        if (HEADER + commands.0.len()) as u64 > placed.plan.headers {
            // alloc: error path
            return Err(error(format!(
                "the load commands take {} bytes, more than the {} the image keeps for them",
                HEADER + commands.0.len(),
                placed.plan.headers
            )));
        }
        let mut flags = macho::MH_NOUNDEFS.0 | macho::MH_DYLDLINK.0 | macho::MH_TWOLEVEL.0;
        flags |= match shared {
            true => macho::MH_NO_REEXPORTED_DYLIBS.0,
            false => macho::MH_PIE.0,
        };
        if has_tlv {
            flags |= macho::MH_HAS_TLV_DESCRIPTORS.0;
        }
        let mut headers = Out::default();
        headers.put_u32(macho::MH_MAGIC_64);
        headers.put_u32(self.cpu);
        headers.put_u32(self.subtype);
        headers.put_u32(if shared { macho::MH_DYLIB.0 } else { macho::MH_EXECUTE.0 });
        headers.put_u32(count);
        headers.put_u32(commands.0.len() as u32);
        headers.put_u32(flags);
        headers.put_u32(0);
        headers.0.extend_from_slice(&commands.0);

        // The file as the loader reads it, hashed where its bytes are: the chained slots
        // over the regions.
        let mut pieces: Vec<(u64, &[u8])> = Vec::with_capacity(placed.regions.len() + debug_at.len() + 2);
        pieces.push((0, &headers.0[..]));
        for (index, region) in placed.regions.iter().enumerate() {
            if let Some(at) = self.offset(placed, region) {
                placed.runs(index, |offset, bytes| pieces.push((at + offset as u64, bytes)));
            }
        }
        pieces.extend(debug_at.iter().map(|&(at, nth)| (at, &placed.debug[nth].1[..])));
        linkedit.runs(|offset, bytes| pieces.push((linkedit_at + offset as u64, bytes)));
        let file = sign::View::new(code_limit, pieces)
            .and_then(|it| it.with_slots(placed.encoded, base))
            .map_err(error)?;
        let text = &segments[usize::from(!shared)];
        let (text_at, text_len) = (text.fileoff, text.filesize);
        let mut signature = Vec::with_capacity(signature_len as usize);
        match kept.as_deref_mut() {
            None => sign::signature(&file, identifier, text_at, text_len, !shared, None, &mut signature),
            Some(kept) => {
                let written = written_since(placed, kept, &debug_at);
                let since = (written.as_deref())
                    .zip(placed.delta)
                    .map(|(written, delta)| sign::Since { from: delta.from, written });
                let compared = [(0, placed.plan.headers), (linkedit_at, code_limit - linkedit_at)];
                let cached = sign::Cached { cache: &mut kept.sign, synthesis: placed.synthesis, since, kept: &compared };
                sign::signature(&file, identifier, text_at, text_len, !shared, Some(cached), &mut signature);
            }
        }
        if signature.len() as u64 != signature_len {
            // alloc: error path
            return Err(error(format!(
                "a code signature of {} bytes where {signature_len} were kept for it",
                signature.len()
            )));
        }
        if let Some(kept) = kept.as_deref_mut() {
            // clone: two reference counts, `__LINKEDIT` shared with the state that holds it
            kept.keep(placed, linkedit.clone(), tables, symbols, &debug_at);
        }

        // The signature is one part: an incremental link, which changes few of its hashes,
        // writes only the blocks of it that changed.
        let mut parts = Vec::with_capacity(3);
        // alloc: once per link, the headers' and the signature's `Arc`s
        parts.push((0, CowBytes::from_vec(headers.0)));
        parts.push((linkedit_at, linkedit));
        parts.push((code_limit, CowBytes::from_vec(signature)));
        Ok(File { len: linkedit_at + linkedit_size, parts, debug: debug_at })
    }

    /// The symbol table: the functions' names, then what the image exports, then what it
    /// imports, each with its library's ordinal. `keep` for an image linked again in place,
    /// whose next link may patch the locals ([`Symbols::locals_of`]).
    fn symbols(
        &self,
        placed: &Placed<'_>,
        ranges: &[(u64, u64, u8)],
        ordinals: &[u32],
        keep: bool,
    ) -> Result<Symbols, LinkError> {
        let base = placed.plan.base;
        let section_at = |addr: u64, what: &str| -> Result<u8, LinkError> { section_in(ranges, addr, what) };

        let exported: blaze_hash::HashSet<&str> =
            // alloc: one set per link, sized once by the exports
            placed.exports.iter().map(|it| it.name.as_str()).collect();
        let mut locals: Vec<(u64, &str)> = Vec::with_capacity(placed.functions.len());
        // alloc: none unless kept, one index per function
        let mut locals_of = Vec::with_capacity(if keep { placed.functions.len() } else { 0 });
        for function in placed.functions {
            let local = function.name.as_deref().filter(|it| !exported.contains(it));
            if keep {
                locals_of.push(if local.is_some() { locals.len() as u32 } else { u32::MAX });
            }
            if let Some(name) = local {
                locals.push((function.addr, name));
            }
        }
        if !locals.is_sorted() {
            locals.sort_unstable();
            // The locals aren't in the functions' order: a patch can't find them by it.
            locals_of.clear();
        }

        // The defined: what the image exports and, in an executable, its header.
        let mut defined: Vec<(&str, u64, u16, u64)> = Vec::with_capacity(placed.exports.len() + 1);
        for export in placed.exports {
            let flags = match export.kind {
                crate::model::SymbolKind::Tls => {
                    u64::from(macho::EXPORT_SYMBOL_FLAGS_KIND_THREAD_LOCAL.0)
                }
                _ => u64::from(macho::EXPORT_SYMBOL_FLAGS_KIND_REGULAR.0),
            };
            if export.addr < base {
                // alloc: error path
                return Err(error(format!(
                    "the export {} at {:#x} is before the image",
                    export.name, export.addr
                )));
            }
            defined.push((export.name.as_str(), export.addr, 0, flags));
        }
        if !self.shared() {
            let flags = u64::from(macho::EXPORT_SYMBOL_FLAGS_KIND_REGULAR.0);
            defined.push(("_mh_execute_header", base, macho::REFERENCED_DYNAMICALLY.0, flags));
        }
        defined.sort_unstable_by(|a, b| {
            macho_order(a.0, b.0).then_with(|| (a.1, a.2, a.3).cmp(&(b.1, b.2, b.3)))
        });

        let mut undefined: Vec<(&str, u32)> =
            // alloc: one buffer per link, of exactly the imports
            (placed.imports.iter().enumerate()).map(|(nth, it)| (it.name.as_str(), nth as u32)).collect();
        undefined.sort_unstable_by(|a, b| macho_order(a.0, b.0).then(a.1.cmp(&b.1)));

        // Every name once, as Mach-O spells it: the binds and the trie take theirs from here.
        let count = locals.len() + defined.len() + undefined.len();
        let names = (locals.iter().map(|it| it.1))
            .chain(defined.iter().map(|it| it.0))
            .chain(undefined.iter().map(|it| it.0))
            .map(|it| it.len() + 2)
            .sum::<usize>();
        let mut strings = Out(Vec::with_capacity(names + 16));
        strings.0.extend_from_slice(b" \0");
        let mut nlist = Out(Vec::with_capacity(count * NLIST));
        locals_into(&locals, &section_at, &mut strings.0, &mut nlist.0)?;
        let mut entry = |name: &str, kind: u8, sect: u8, desc: u16, value: u64| -> (u32, u32) {
            let at = strings.0.len() as u32;
            let (prefix, rest) = spelled(name);
            nlist.put_u32(at);
            strings.0.extend_from_slice(prefix.as_bytes());
            strings.0.extend_from_slice(rest.as_bytes());
            strings.put_u8(0);
            nlist.put_u8(kind);
            nlist.put_u8(sect);
            nlist.put_u16(desc);
            nlist.put_u64(value);
            (at, (prefix.len() + rest.len()) as u32)
        };

        let mut exports = Vec::with_capacity(defined.len());
        for &(name, addr, desc, flags) in &defined {
            // The header is in no section; ld64 names it in the first, as it does here.
            let sect = match name == "_mh_execute_header" && !self.shared() {
                true => (ranges.iter().map(|it| it.2).min())
                    .ok_or_else(|| error("the image has no section for _mh_execute_header to name"))?,
                false => section_at(addr, name)?,
            };
            let at = entry(name, macho::N_SECT.0 | macho::N_EXT.0, sect, desc, addr);
            exports.push((at, flags, addr - base));
        }
        let first_undefined = (locals.len() + defined.len()) as u32;
        // alloc: two tables per link, each sized once by the imports
        let mut index_of = vec![0u32; placed.imports.len()];
        let mut import_names = vec![(0u32, 0u32); placed.imports.len()]; // alloc: as `index_of`
        for (nth, &(name, index)) in undefined.iter().enumerate() {
            let import = &placed.imports[index as usize];
            let library = match import.library {
                Some(library) => ordinals[library as usize] as u16,
                None => u16::from(macho::DYNAMIC_LOOKUP_ORDINAL.0),
            };
            let weak = if import.weak { macho::N_WEAK_REF.0 } else { 0 };
            import_names[index as usize] = entry(name, macho::N_UNDF.0 | macho::N_EXT.0, 0, library << 8 | weak, 0);
            index_of[index as usize] = first_undefined + nth as u32;
        }
        // Every `n_strx` is a name's start, below the table's length: past 4 GiB one wrapped.
        if strings.0.len() > 1 << 32 {
            // alloc: error path
            return Err(error(format!(
                "a string table of {} bytes: `n_strx` reaches only 4 GiB",
                strings.0.len()
            )));
        }
        strings.pad_to(8);

        Ok(Symbols {
            count: count as u32,
            locals: locals.len() as u32,
            defined: defined.len() as u32,
            undefined: undefined.len() as u32,
            nlist: nlist.0,
            strings: strings.0,
            import_names,
            index_of,
            exports,
            locals_of,
        })
    }
}

impl MachO {
    /// What the placed image may hold, checked before anything is encoded ([`Writer::encode`]
    /// runs before [`Writer::file`]): no `.preinit_array`, a reservation exactly where the dev
    /// run asks for one, a dev run's start hook first in `__mod_init_func`, and nothing dyld
    /// writes in code.
    fn check_placed(&self, placed: &Placed<'_>) -> Result<(), LinkError> {
        if let Some(region) = placed.regions.iter().find(|it| it.class == Class::PreinitArray) {
            // alloc: error path
            return Err(error(format!(
                "a `.preinit_array` region at {:#x} in a Mach-O image, which has no pre-initializers: \
                 a dev run's start hook is the first entry of `__mod_init_func`",
                region.addr
            )));
        }
        let asked = self.output.dev_run.map_or([0; 3], |it| [it.arena, it.cells, it.fixed]);
        for (reserve, asked) in [Reserve::Arena, Reserve::Cells, Reserve::Fixed].into_iter().zip(asked) {
            let class = Class::Reserve(reserve);
            let room = (placed.regions.iter())
                .filter(|it| it.class == class)
                .fold(0u64, |room, it| room.saturating_add(it.capacity));
            if (asked == 0) != (room == 0) || room < asked {
                let segment = place(&class).0.name();
                return Err(error(match self.output.dev_run {
                    // alloc: error path
                    None => format!("`{segment}` reserves {room:#x} bytes in an image that is not a dev run"),
                    // alloc: error path
                    Some(_) => format!("`{segment}` reserves {room:#x} bytes where the dev run asks {asked:#x}"),
                }));
            }
        }
        if self.output.dev_run.is_some() {
            start_hook_first(placed)?;
        }
        self.fixed_code(placed)
    }

    /// Nothing dyld writes is in `__TEXT`, so its bytes are the file's at every load address:
    /// no pointer it slides, no bind, no GOT cell. Each is an error naming its symbol and
    /// section.
    fn fixed_code(&self, placed: &Placed<'_>) -> Result<(), LinkError> {
        let base = placed.plan.base;
        let text = (placed.regions.iter())
            .filter(|it| place(&it.class).0 == Segment::Text)
            .fold(base + placed.plan.headers, |end, it| end.max(it.addr + it.capacity));
        let code = base..page_up(text, self.page);
        let mut found = None;
        if let Some(&at) = placed.rebase.iter().find(|it| code.contains(*it)) {
            found = Some((at, "slides a pointer", None));
        }
        if let Some(bind) = placed.binds.iter().find(|it| code.contains(&it.addr)) {
            found = found.filter(|it| it.0 < bind.addr).or(Some((bind.addr, "binds a pointer", Some(bind.import))));
        }
        if let Some(cell) = placed.got.iter().find(|it| code.contains(&it.addr)) {
            let import = match cell.holds {
                Holds::Import(import) => Some(import),
                _ => None,
            };
            found = found.filter(|it| it.0 < cell.addr).or(Some((cell.addr, "fixes up a GOT cell", import)));
        }
        let Some((addr, what, import)) = found else { return Ok(()) };
        let section = (placed.regions.iter())
            .find(|it| (it.addr..it.addr + it.capacity.max(it.size)).contains(&addr))
            // alloc: error path
            .map_or_else(|| "the headers".to_owned(), |it| format!("`__TEXT,{}`", section_of(&it.class).1));
        let symbol = match import.and_then(|it| placed.imports.get(it as usize)) {
            Some(import) => {
                let (prefix, rest) = spelled(&import.name);
                // alloc: error path
                format!("to `{prefix}{rest}`")
            }
            None => match function_at(placed, addr) {
                Some(name) => {
                    let (prefix, rest) = spelled(name);
                    // alloc: error path
                    format!("in `{prefix}{rest}`")
                }
                // alloc: error path
                None => "in no function".to_owned(),
            },
        };
        // alloc: error path
        Err(error(format!(
            "dyld {what} at {addr:#x} {symbol}, in {section}: code is written by no loader, so \
             its bytes are the file's at every address it loads at (a pointer in code is a text \
             relocation, which arm64 macOS refuses)"
        )))
    }
}

/// A dev run's first initializer is its start hook: the first pointer of `__mod_init_func`,
/// as the region holds it before dyld slides it, is `_blaze_inproc_start`.
fn start_hook_first(placed: &Placed<'_>) -> Result<(), LinkError> {
    let start = (placed.symbol)(START).ok_or_else(|| {
        error(
            "a dev run whose image doesn't define `_blaze_inproc_start`: its dev runtime \
             (`blaze_inproc::runtime`) is the start hook that overlays its code",
        )
    })?;
    let first = (placed.region(&Class::InitArray)).and_then(|(index, _)| placed.view(index).u64(0));
    match first {
        Some(first) if first == start => Ok(()),
        // alloc: error path
        Some(first) => Err(error(format!(
            "a dev run whose first initializer is {first:#x}, not `_blaze_inproc_start` at {start:#x}: \
             the start hook overlays the code before any other initializer runs"
        ))),
        None => Err(error(
            "a dev run with no `__mod_init_func`: its first entry is the start hook, \
             `_blaze_inproc_start`",
        )),
    }
}

/// The name of the function `addr` is in, by [`Placed::functions`] (in address order).
fn function_at<'p>(placed: &'p Placed<'_>, addr: u64) -> Option<&'p str> {
    let at = placed.functions.partition_point(|it| it.addr <= addr).checked_sub(1)?;
    let function = &placed.functions[at];
    if addr >= function.addr + function.size.max(1) {
        return None;
    }
    function.name.as_deref()
}

/// A dev run's code segments as its start hook overlays them (`blaze_inproc::image`): each
/// starts on a page in the file and in memory, is all file bytes, is never writable, and
/// shares no page of memory or of the file with another segment.
fn code_alone(segments: &[SegmentOut], page: u64) -> Result<(), LinkError> {
    for (nth, code) in segments.iter().enumerate().filter(|it| it.1.prot & macho::VM_PROT_EXECUTE.0 != 0) {
        let memory = code.vmaddr..page_up(code.vmaddr + code.vmsize, page);
        let file = code.fileoff..page_up(code.fileoff + code.filesize, page);
        let shared = (segments.iter().enumerate()).find(|&(other, it)| {
            other != nth
                && ((it.vmsize != 0 && it.vmaddr < memory.end && memory.start < it.vmaddr + it.vmsize)
                    || (it.filesize != 0 && it.fileoff < file.end && file.start < it.fileoff + it.filesize))
        });
        let why = if code.prot & macho::VM_PROT_WRITE.0 != 0 {
            // alloc: error path
            "is writable".to_owned()
        } else if code.vmaddr % page != 0 || code.fileoff % page != 0 {
            // alloc: error path
            "does not start on a page".to_owned()
        } else if code.filesize != code.vmsize {
            // alloc: error path
            "is not all file bytes".to_owned()
        } else if let Some((_, other)) = shared {
            // alloc: error path
            format!("shares a page with `{}`", other.name)
        } else {
            continue;
        };
        // alloc: error path
        return Err(error(format!(
            "a dev run's code segment `{}` at {:#x} (file offset {:#x}) {why}: its start hook overlays \
             whole pages of {page:#x} of code with the file's bytes",
            code.name, code.vmaddr, code.fileoff
        )));
    }
    Ok(())
}

/// The indirect symbol table of the `__got`s, and how many entries it has; each `__got`'s
/// first entry is set in its section. `index_of` is each import's symbol.
fn indirect(
    placed: &Placed<'_>,
    sections: &mut [&mut SectionOut],
    index_of: &[u32],
) -> Result<(Vec<u8>, u32), LinkError> {
    // The GOT's cells by address, walked alongside each `__got` section's cells.
    // alloc: one buffer per link, of exactly the GOT's cells
    let mut got: Vec<(u64, &Holds)> = placed.got.iter().map(|it| (it.addr, &it.holds)).collect();
    if !got.is_sorted_by_key(|it| it.0) {
        radix::sort_by_key(&mut got, |it| it.0);
    }
    let mut indirect = Out::default();
    let mut indirect_count = 0u32;
    for section in sections.iter_mut() {
        if section.flags != macho::S_NON_LAZY_SYMBOL_POINTERS.to_flags().0 {
            continue;
        }
        if section.size % 8 != 0 {
            // alloc: error path
            return Err(error(format!(
                "the `__got` at {:#x} is {} bytes, not of whole cells",
                section.addr, section.size
            )));
        }
        section.indirect = indirect_count;
        indirect.0.reserve(section.size as usize / 2);
        let mut next = got.partition_point(|it| it.0 < section.addr);
        for nth in 0..section.size / 8 {
            let addr = section.addr + nth * 8;
            while got.get(next).is_some_and(|it| it.0 < addr) {
                next += 1;
            }
            let cell = got.get(next).filter(|it| it.0 == addr).map(|it| it.1);
            indirect.put_u32(match cell {
                Some(&Holds::Import(import)) => index_of[import as usize],
                Some(Holds::Local(_)) => macho::INDIRECT_SYMBOL_LOCAL.0,
                _ => macho::INDIRECT_SYMBOL_ABS.0,
            });
            indirect_count += 1;
        }
    }
    Ok((indirect.0, indirect_count))
}

/// Each section that symbols can name, (start, end, its ordinal), by start: every section
/// but `__DWARF`'s, numbered from 1 in load-command order.
fn section_ranges(sections: &[&mut SectionOut]) -> Result<Vec<(u64, u64, u8)>, LinkError> {
    let mut ranges: Vec<(u64, u64, u8)> = Vec::with_capacity(sections.len());
    for (nth, section) in sections.iter().enumerate() {
        if section.segment != "__DWARF" {
            let ordinal = u8::try_from(nth + 1)
                .map_err(|_| error("more than 255 sections: symbols can't name them"))?;
            ranges.push((section.addr, section.addr + section.size, ordinal));
        }
    }
    radix::sort_copy_by_key(&mut ranges, |&(start, end, _)| radix::pack_64(start, end));
    Ok(ranges)
}

/// The ordinal of the section of `ranges` holding the symbol `what` at `addr`.
fn section_in(ranges: &[(u64, u64, u8)], addr: u64, what: &str) -> Result<u8, LinkError> {
    let at = ranges.partition_point(|it| it.0 <= addr);
    (ranges[..at].iter().rev())
        .find(|it| addr <= it.1)
        .map(|it| it.2)
        // alloc: error path
        .ok_or_else(|| error(format!("the symbol {what} at {addr:#x} is in no section")))
}

/// Patches the kept symbol table's locals for the functions that changed in place
/// ([`Delta::functions`], every run of the same length): each local or not as before, with a
/// name of the same length, and the locals still in order. `false` where one isn't, and
/// `symbols` is then to be built again; its bytes are a full build's either way.
fn patch_locals(
    symbols: &mut Symbols,
    placed: &Placed<'_>,
    delta: &Delta<'_>,
    ranges: &[(u64, u64, u8)],
) -> Result<bool, LinkError> {
    let functions = placed.functions;
    if symbols.locals_of.len() != functions.len() {
        return Ok(false);
    }
    // Whether the image exports `name`: the defined symbols are by Mach-O name.
    let exported = |strings: &[u8], name: &str| {
        let (prefix, rest) = spelled(name);
        (symbols.exports.binary_search_by(|&((at, len), _, _)| {
            let stored = &strings[at as usize..(at + len) as usize];
            stored.iter().cmp(prefix.as_bytes().iter().chain(rest.as_bytes()))
        }))
        .is_ok()
    };
    let changed = || delta.functions.iter().flat_map(|it| it.new.start as usize..it.new.end as usize);
    for nth in changed() {
        let function = &functions[nth];
        let local = symbols.locals_of[nth];
        let name = function.name.as_deref();
        // An executable's header is a defined symbol no export names.
        if name == Some("mh_execute_header") {
            return Ok(false);
        }
        let name = name.filter(|it| !exported(&symbols.strings, it));
        let Some(name) = name else {
            if local != u32::MAX {
                return Ok(false);
            }
            continue;
        };
        if local == u32::MAX {
            return Ok(false);
        }
        let entry = local as usize * NLIST;
        let strx = u32::from_le_bytes(symbols.nlist[entry..entry + 4].try_into().expect("4 bytes")) as usize;
        let (prefix, rest) = spelled(name);
        let len = prefix.len() + rest.len();
        let Some(stored) = symbols.strings.get(strx..strx + len + 1) else { return Ok(false) };
        if stored[len] != 0 || stored[..len].contains(&0) {
            return Ok(false);
        }
        let sect = section_in(ranges, function.addr, name)?;
        let entry = &mut symbols.nlist[entry..entry + NLIST];
        entry[5] = sect;
        entry[8..16].copy_from_slice(&function.addr.to_le_bytes());
        symbols.strings[strx..strx + prefix.len()].copy_from_slice(prefix.as_bytes());
        symbols.strings[strx + prefix.len()..strx + len].copy_from_slice(rest.as_bytes());
    }
    // In order: a full build sorts the locals by (address, name) only when they aren't.
    let key = |nth: usize| (functions[nth].addr, functions[nth].name.as_deref().unwrap_or_default());
    let locals_of = &symbols.locals_of;
    for nth in changed().filter(|&it| locals_of[it] != u32::MAX) {
        let before = (0..nth).rev().find(|&it| locals_of[it] != u32::MAX);
        let after = (nth + 1..functions.len()).find(|&it| locals_of[it] != u32::MAX);
        if before.is_some_and(|it| key(it) > key(nth)) || after.is_some_and(|it| key(nth) > key(it)) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Room for a kept table of `len` bytes appended to `out`, as zeros; `len`.
fn room(out: &mut Vec<u8>, len: usize) -> usize {
    out.resize(out.len() + len, 0);
    len
}

/// `__LINKEDIT` as the file's part, from `built`, laid out with each table at `tables` and the
/// ones `reused` names (by their (start, length) in the kept `__LINKEDIT`) left zeros. Where
/// each reused table is where it was in a kept `__LINKEDIT` of `built`'s length, that one with
/// the rest written over it, which copies only the pages whose bytes changed, and `built` is
/// kept to build the next in; otherwise `built` with the reused tables copied in.
fn linkedit_part(
    kept: Option<&mut Kept>,
    mut built: Vec<u8>,
    tables: &[(usize, usize); 4],
    reused: &[Option<(usize, usize)>; 4],
) -> CowBytes {
    let Some(kept) = kept else {
        // alloc: once per link, `__LINKEDIT`'s `Arc`: an image linked once keeps nothing
        return CowBytes::from_vec(built);
    };
    let in_place = reused.iter().zip(tables).all(|(reused, table)| reused.is_none_or(|it| it == *table));
    match kept.linkedit.as_ref() {
        Some(last) if in_place && last.len() == built.len() => {
            // clone: two reference counts; the writes below copy only the pages they change
            let mut part = last.clone();
            let mut at = 0;
            for (reused, &(start, len)) in reused.iter().zip(tables) {
                if reused.is_some() {
                    part.update(at, &built[at..start]);
                    at = start + len;
                }
            }
            part.update(at, &built[at..]);
            kept.scratch = built;
            part
        }
        last => {
            for (reused, &(start, len)) in reused.iter().zip(tables) {
                if let Some((from, _)) = *reused {
                    last.expect("a reused table is kept").copy_to(from, &mut built[start..start + len]);
                }
            }
            // alloc: once per link whose `__LINKEDIT` moved, its `Arc`
            CowBytes::from_vec(built)
        }
    }
}

/// What an incremental image keeps from its last link, behind [`MachO::cache`], for the next
/// to reuse or patch: only when that link's changes are from this one ([`Delta::from`]
/// equal to `synthesis`), so a rolled-back link never patches what it didn't make.
#[derive(Default)]
struct Kept {
    sign: sign::Cache,
    unwind: unwind::Last,
    /// The synthesis the tables are of; 0 for none.
    synthesis: u64,
    /// The last `__LINKEDIT`, as the last link's file holds it too, and in it the rebase and
    /// bind opcodes, the export trie and the function starts: each one's (start, length).
    linkedit: Option<CowBytes>,
    tables: [(usize, usize); 4],
    /// The buffer the next `__LINKEDIT` is built in, kept from the last.
    scratch: Vec<u8>,
    symbols: Option<Symbols>,
    /// The chained slots, which a page is signed again for when one of them changed.
    slots: Vec<(u64, u64)>,
    /// The debug sections, by where their bytes are and how many. The state the next link's
    /// changes are from holds them, so a buffer at the same place is the same buffer.
    debug: Vec<(usize, usize)>,
}

impl Kept {
    /// Keeps what this link made: `tables` are (start, length) in `linkedit`.
    fn keep(
        &mut self,
        placed: &Placed<'_>,
        linkedit: CowBytes,
        tables: [(usize, usize); 4],
        symbols: Symbols,
        debug_at: &[(u64, usize)],
    ) {
        self.linkedit = Some(linkedit);
        self.tables = tables;
        self.symbols = Some(symbols);
        self.slots.clear();
        self.slots.extend_from_slice(placed.encoded);
        self.debug.clear();
        self.debug.extend(debug_at.iter().map(|&(_, nth)| buffer(&placed.debug[nth].1)));
        self.synthesis = placed.synthesis;
    }
}

fn buffer(bytes: &[u8]) -> (usize, usize) {
    (bytes.as_ptr() as usize, bytes.len())
}

/// The file's byte ranges the core wrote since the link `kept` is of, (offset, length), by
/// offset and merged: the delta's addresses in the file, each chained slot that changed, and
/// the debug sections that aren't the same buffers. `None` when the core doesn't say.
fn written_since(placed: &Placed<'_>, kept: &Kept, debug_at: &[(u64, usize)]) -> Option<Vec<(u64, u64)>> {
    let delta = placed.delta.filter(|it| it.same_regions)?;
    let base = placed.plan.base;
    // alloc: one buffer per incremental link, of the ranges written
    let mut out = Vec::with_capacity(delta.written.len() + debug_at.len() + 16);
    for &(addr, len) in delta.written {
        let at = placed.regions.partition_point(|it| it.addr <= addr);
        if at.checked_sub(1).is_some_and(|at| !zeroed(&placed.regions[at].class)) {
            out.push((addr - base, len));
        }
    }
    let (old, new) = (&kept.slots[..], placed.encoded);
    let (mut i, mut j) = (0, 0);
    while i < old.len() || j < new.len() {
        let at = match (old.get(i), new.get(j)) {
            (Some(a), Some(b)) if a.0 == b.0 => {
                (i, j) = (i + 1, j + 1);
                if a.1 == b.1 {
                    continue;
                }
                a.0
            }
            (Some(a), b) if b.is_none_or(|b| a.0 < b.0) => {
                i += 1;
                a.0
            }
            (_, Some(b)) => {
                j += 1;
                b.0
            }
            (_, None) => unreachable!("one of the two lists has a slot left"),
        };
        out.push((at - base, 8));
    }
    let same_debug = kept.debug.len() == debug_at.len()
        && (debug_at.iter().zip(&kept.debug)).all(|(&(_, nth), &old)| buffer(&placed.debug[nth].1) == old);
    if !same_debug {
        out.extend(debug_at.iter().map(|&(at, nth)| (at, placed.debug[nth].1.len() as u64)));
    }
    radix::sort_copy_by_key(&mut out, |&(at, len)| radix::pack_64(at, len));
    let mut merged = 0usize;
    for nth in 0..out.len() {
        let (at, len) = out[nth];
        match merged.checked_sub(1).map(|it| &mut out[it]) {
            Some(last) if at <= last.0 + last.1 => last.1 = last.1.max(at + len - last.0),
            _ => {
                out[merged] = (at, len);
                merged += 1;
            }
        }
    }
    out.truncate(merged);
    Some(out)
}

/// The kept state, started over after a panic while it was held: a page may be without its
/// hash, a table half patched.
fn lock(cache: &Mutex<Kept>) -> Reset<'_, Kept> {
    reset::lock(cache)
}

/// A load command of a path at `at`, after `fields`: `LC_LOAD_DYLINKER`, `LC_*_DYLIB`.
fn path_command(out: &mut Out, command: u32, at: u32, path: &str, fields: &[u32]) {
    let size = (at as usize + path.len() + 1).next_multiple_of(8);
    out.put_u32(command);
    out.put_u32(size as u32);
    out.put_u32(at);
    for field in fields {
        out.put_u32(*field);
    }
    let start = out.0.len();
    out.0.extend_from_slice(path.as_bytes());
    out.0.resize(start + size - at as usize, 0);
}
