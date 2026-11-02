//! ELF: Linux executables (PIE: `ET_DYN` with `PT_INTERP`) and shared libraries on x86-64
//! and AArch64, laid out as lld lays out what rustc's link line asks of it (`-pie`, `-z relro
//! -z now`, `--eh-frame-hdr`, `--build-id`, `--as-needed`, `-Bsymbolic` for a shared library).
//!
//! Four segments, each its own `PT_LOAD` starting on a page: read-only (the headers, the
//! dynamic linking tables, `.rodata`, `.eh_frame_hdr`, `.eh_frame`), code, relro (what the
//! loader writes and then makes read-only: `.data.rel.ro`, the init arrays, `.dynamic`, the
//! GOT) and writable data with the TLS template and `.bss` last. A region's file offset is
//! its address, so a region never moves in the file while its address stays.
//!
//! A dev executable ([`crate::DevRun`]) adds a `PT_LOAD` after these for each reservation, of
//! no access and no file bytes, and its start hook's pointer is the first of
//! `.preinit_array`. The layout above already puts its code on pages of its own, all in the
//! file and never writable, and binds everything at load (`DF_BIND_NOW`, no lazy PLT): the
//! writer checks each of these and refuses a dev executable that breaks one
//! ([`image::file`]).

#![forbid(unsafe_code)]

mod dynamic;
mod image;
mod native;
mod read;
#[cfg(test)]
mod tests;

use blaze_codegen::Arch;
use blaze_ids::radix;
use parking_lot::Mutex;

use crate::model::{Class, Counts, File, NativeFile, Placed, Plan, Prot, Region, Reserve, SectionName, Special, Writer};
use crate::reset::{self, Reset};
use crate::{Format, LinkError, NativeKind, Output, OutputKind};

/// The ELF header and the most program headers an image has: `PT_PHDR`, `PT_INTERP`, four
/// `PT_LOAD`s, `PT_DYNAMIC`, `PT_GNU_RELRO`, `PT_GNU_EH_FRAME`, `PT_GNU_STACK`, `PT_TLS`,
/// `PT_NOTE`; a dev executable's reservations add one `PT_LOAD` each ([`RESERVES`]).
const EHDR: u64 = 64;
const PHDR: u64 = 56;
const PHDRS: u64 = 12;
const RESERVES: u64 = 3;

pub(crate) const INTERP: &str = ".interp";
pub(crate) const BUILD_ID: &str = ".note.gnu.build-id";
pub(crate) const DYNSYM: &str = ".dynsym";
pub(crate) const DYNSTR: &str = ".dynstr";
pub(crate) const GNU_HASH: &str = ".gnu.hash";
pub(crate) const VERSYM: &str = ".gnu.version";
pub(crate) const VERNEED: &str = ".gnu.version_r";
pub(crate) const RELA_DYN: &str = ".rela.dyn";
pub(crate) const DYNAMIC: &str = ".dynamic";
pub(crate) const EH_FRAME_HDR: &str = ".eh_frame_hdr";

/// [`Class::PreinitArray`]'s section: input sections of this name join a dev run's start
/// hook there, so `DT_PREINIT_ARRAY` names one array.
const PREINIT_ARRAY: &str = ".preinit_array";

pub(crate) fn writer(output: &Output) -> Result<Elf, LinkError> {
    if output.format != Format::Elf {
        return Err(error(format!("the ELF writer was asked for a {:?} image", output.format))); // alloc: error path
    }
    match output.arch {
        Arch::X86_64 | Arch::Aarch64 => {}
        Arch::Wasm32 => return Err(error("an ELF image for wasm32: wasm is linked as wasm".into())),
    }
    let last = output.incremental.then(Mutex::default);
    // clone: once per link, the writer owns its `Output` (the public API's type)
    Ok(Elf { output: output.clone(), last })
}

pub(crate) struct Elf {
    pub(crate) output: Output,
    /// An incremental output's last tables, which the next synthesis patches when its
    /// [`crate::model::Delta`] is from theirs.
    pub(crate) last: Option<Mutex<Last>>,
}

/// The tables the writer made last, each with the [`Placed::synthesis`] it was made in (0
/// when it holds none: never made, or a build of it failed).
#[derive(Default)]
pub(crate) struct Last {
    pub(crate) rela: dynamic::KeptRela,
    pub(crate) eh_frame_hdr: dynamic::KeptHdr,
    pub(crate) symtab: image::KeptSymtab,
}

/// Which `PT_LOAD` a class goes in, in address order.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) enum Segment {
    Read,
    Code,
    Relro,
    Write,
    /// A dev run's reservation, one `PT_LOAD` each, after every other segment.
    Reserve(Reserve),
}

impl Segment {
    pub(crate) fn of(class: &Class) -> Segment {
        match class {
            Class::Synthetic(DYNAMIC) => Segment::Relro,
            Class::Synthetic(_) | Class::Rodata | Class::EhFrame => Segment::Read,
            Class::Text | Class::Patch | Class::Stubs | Class::Plt | Class::Veneers => Segment::Code,
            Class::RelRo | Class::PreinitArray | Class::InitArray | Class::FiniArray | Class::Got => Segment::Relro,
            Class::Table
            | Class::Data
            | Class::PatchData
            | Class::ThreadVars
            | Class::Tdata
            | Class::Tbss
            | Class::Bss => Segment::Write,
            Class::Reserve(reserve) => Segment::Reserve(*reserve),
            Class::Named { write, exec, .. } => match (*write, *exec) {
                (_, true) => Segment::Code,
                (true, false) => Segment::Write,
                (false, false) => Segment::Read,
            },
        }
    }

    /// Its place in address order, as a radix key.
    fn rank(self) -> u32 {
        match self {
            Segment::Read => 0,
            Segment::Code => 1,
            Segment::Relro => 2,
            Segment::Write => 3,
            Segment::Reserve(reserve) => 4 + reserve as u32,
        }
    }

    /// The `p_flags` of its `PT_LOAD`.
    pub(crate) fn flags(self) -> u32 {
        const R: u32 = 4;
        const W: u32 = 2;
        const X: u32 = 1;
        match self {
            Segment::Read => R,
            Segment::Code => R | X,
            Segment::Relro | Segment::Write => R | W,
            Segment::Reserve(_) => 0,
        }
    }

    /// Whether the loader may write it: where dynamic relocations may point.
    pub(crate) fn loader_writes(self) -> bool {
        matches!(self, Segment::Relro | Segment::Write)
    }
}

/// Whether a region's bytes are in the file: everything but the zeroed tail of the writable
/// segment and the reservations.
pub(crate) fn in_file(class: &Class) -> bool {
    !matches!(class, Class::Tbss | Class::Bss | Class::Reserve(_))
}

/// A class's place in the address space: its segment, its rank in it, then its name.
fn order_key(class: &Class) -> (Segment, u32, &str) {
    let segment = Segment::of(class);
    let rank = match (segment, class) {
        (Segment::Read, Class::Synthetic(name)) => match *name {
            INTERP => 0,
            BUILD_ID => 1,
            DYNSYM => 2,
            DYNSTR => 3,
            GNU_HASH => 4,
            VERSYM => 5,
            VERNEED => 6,
            RELA_DYN => 7,
            EH_FRAME_HDR => 10,
            _ => 12,
        },
        (Segment::Read, Class::Rodata) => 8,
        (Segment::Read, Class::EhFrame) => 11,
        (Segment::Read, _) => 9,

        (Segment::Code, Class::Text) => 0,
        (Segment::Code, Class::Patch) => 2,
        (Segment::Code, Class::Stubs) => 3,
        (Segment::Code, Class::Plt) => 4,
        (Segment::Code, Class::Veneers) => 5,
        (Segment::Code, _) => 1,

        (Segment::Relro, Class::RelRo) => 0,
        (Segment::Relro, Class::PreinitArray) => 1,
        (Segment::Relro, Class::InitArray) => 2,
        (Segment::Relro, Class::FiniArray) => 3,
        (Segment::Relro, Class::Synthetic(_)) => 4,
        (Segment::Relro, Class::Got) => 5,
        (Segment::Relro, _) => 6,

        (Segment::Write, Class::Table) => 0,
        (Segment::Write, Class::Data) => 1,
        (Segment::Write, Class::PatchData) => 3,
        (Segment::Write, Class::ThreadVars) => 4,
        (Segment::Write, Class::Tdata) => 4,
        (Segment::Write, Class::Tbss) => 5,
        (Segment::Write, Class::Bss) => 6,
        (Segment::Write, _) => 2,

        (Segment::Reserve(_), _) => 0,
    };
    let name = match class {
        Class::Named { name, .. } => name.as_str(),
        Class::Synthetic(name) => name,
        _ => "",
    };
    (segment, rank, name)
}

/// The class and rank of an allocated section of this name and these flags, as lld places
/// input sections: `.init_array.N` before `.init_array`, `.ctors.N` as `.init_array` of
/// priority `65535 - N`, `.text.*` in `.text`, and any other name an output section of its
/// own (an orphan), kept whole for its `__start_`/`__stop_` symbols.
pub(crate) fn section(name: &str, write: bool, exec: bool, tls: bool) -> Result<(Class, u32), String> {
    if name.is_empty() {
        return Err("a section with no name".into());
    }
    if write && exec {
        return Err(format!("section `{name}` is both writable and executable")); // alloc: error path
    }
    let under = |prefix: &str| {
        name == prefix || name.strip_prefix(prefix).is_some_and(|rest| rest.starts_with('.'))
    };
    let suffix = |prefix: &str| name.strip_prefix(prefix).and_then(|rest| rest.strip_prefix('.'));
    let priority = |prefix: &str, reversed: bool| match suffix(prefix).map(str::parse::<u32>) {
        Some(Ok(n)) if n <= 65535 => if reversed { 65535 - n } else { n },
        _ => 65536,
    };
    if tls || under(".tdata") || under(".tbss") {
        let class = if under(".tbss") { Class::Tbss } else { Class::Tdata };
        return Ok((class, 0));
    }
    let placed = if under(".init_array") {
        (Class::InitArray, priority(".init_array", false))
    } else if under(".ctors") {
        (Class::InitArray, priority(".ctors", true))
    } else if under(".fini_array") {
        (Class::FiniArray, priority(".fini_array", false))
    } else if under(".dtors") {
        (Class::FiniArray, priority(".dtors", true))
    } else if under(PREINIT_ARRAY) {
        (Class::PreinitArray, 0)
    } else if under(".data.rel.ro") {
        (Class::RelRo, 0)
    } else if (under(".rodata") || under(".gcc_except_table")) && !write && !exec {
        (Class::Rodata, 0)
    } else if under(".data") && !exec {
        (Class::Data, 0)
    } else if under(".bss") && !exec {
        (Class::Bss, 0)
    } else if under(".text") && !write {
        (Class::Text, 0)
    } else {
        (Class::Named { name: SectionName::new(name), write, exec }, 0)
    };
    Ok(placed)
}

/// A name `__start_X` and `__stop_X` are defined for: a C identifier.
fn c_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|it| it == '_' || it.is_ascii_alphabetic())
        && chars.all(|it| it == '_' || it.is_ascii_alphanumeric())
}

fn start(regions: &[Region], matches: impl Fn(&Class) -> bool) -> Option<u64> {
    regions.iter().filter(|it| matches(&it.class)).map(|it| it.addr).min()
}

fn end(regions: &[Region], matches: impl Fn(&Class) -> bool) -> Option<u64> {
    regions.iter().filter(|it| matches(&it.class)).map(|it| it.addr + it.size).max()
}

fn preinit(class: &Class) -> bool {
    *class == Class::PreinitArray
}

/// lld defines the bounds of an absent array at the image's start (address 0: `Plan::base`).
fn init_array_start(regions: &[Region]) -> u64 {
    start(regions, |it| *it == Class::InitArray).unwrap_or(0)
}
fn init_array_end(regions: &[Region]) -> u64 {
    end(regions, |it| *it == Class::InitArray).unwrap_or(0)
}
fn fini_array_start(regions: &[Region]) -> u64 {
    start(regions, |it| *it == Class::FiniArray).unwrap_or(0)
}
fn fini_array_end(regions: &[Region]) -> u64 {
    end(regions, |it| *it == Class::FiniArray).unwrap_or(0)
}
fn preinit_array_start(regions: &[Region]) -> u64 {
    start(regions, preinit).unwrap_or(0)
}
fn preinit_array_end(regions: &[Region]) -> u64 {
    end(regions, preinit).unwrap_or(0)
}
fn dynamic_start(regions: &[Region]) -> u64 {
    start(regions, |it| *it == Class::Synthetic(DYNAMIC)).unwrap_or(0)
}
/// Code computes the GOT's address with `GOTPC32` and offsets from it with `GOTOFF64`, so
/// any one address serves; lld's is the GOT's start on AArch64.
fn got_base(regions: &[Region]) -> u64 {
    start(regions, |it| *it == Class::Got).unwrap_or_else(|| dynamic_start(regions))
}
fn eh_frame_hdr_start(regions: &[Region]) -> u64 {
    start(regions, |it| *it == Class::Synthetic(EH_FRAME_HDR)).unwrap_or(0)
}
fn tls_start(regions: &[Region]) -> u64 {
    start(regions, |it| matches!(it, Class::Tdata | Class::Tbss)).unwrap_or(0)
}
/// The end of the last region whose bytes are in the file.
fn edata(regions: &[Region]) -> u64 {
    end(regions, in_file).unwrap_or(0)
}
/// The end of the image's own memory: a dev run's reservations are not its.
fn image_end(regions: &[Region]) -> u64 {
    end(regions, |it| !matches!(it, Class::Reserve(_))).unwrap_or(0)
}
fn bss_start(regions: &[Region]) -> u64 {
    start(regions, |it| *it == Class::Bss).unwrap_or_else(|| edata(regions))
}
fn etext(regions: &[Region]) -> u64 {
    end(regions, |it| Segment::of(it) == Segment::Code).unwrap_or(0)
}

impl Writer for Elf {
    fn format(&self) -> Format {
        Format::Elf
    }

    fn arch(&self) -> Arch {
        self.output.arch
    }

    fn plan(&self) -> Plan {
        Plan {
            base: 0,
            headers: EHDR + PHDR * self.phdrs(),
            page: self.page(),
            indexed_code: false,
            got_by_library: false,
            tls_descriptors: false,
        }
    }

    fn order(&self, classes: &mut Vec<Class>) {
        let rank = |class: &Class| {
            let (segment, rank, _) = order_key(class);
            radix::pack_32(segment.rank(), rank)
        };
        radix::sort_by_key(classes, rank);
        for run in classes.chunk_by_mut(|a, b| rank(a) == rank(b)) {
            run.sort_by(|a, b| order_key(a).2.cmp(order_key(b).2));
        }
    }

    /// Relro is read-only once the loader has relocated it (`-z now`): `Prot::R` here, so it
    /// starts on a page of its own; its `PT_LOAD` is writable for the loader.
    fn prot(&self, class: &Class) -> Prot {
        match Segment::of(class) {
            Segment::Read | Segment::Relro => Prot::R,
            Segment::Code => Prot::RX,
            Segment::Write => Prot::RW,
            Segment::Reserve(_) => Prot::NONE,
        }
    }

    fn section(&self, name: &str, write: bool, exec: bool, tls: bool) -> Result<(Class, u32), String> {
        section(name, write, exec, tls)
    }

    /// The first entry of `.preinit_array`: the loader calls it once every object is relocated
    /// and before any initializer of the executable or of a shared library. A shared library's
    /// `.preinit_array` is never called, so it has none.
    fn start_hook(&self) -> Result<(Class, u32), String> {
        match self.output.kind {
            OutputKind::Executable { .. } => Ok((Class::PreinitArray, 0)),
            OutputKind::Shared { .. } => {
                Err("a shared library has no start hook: the loader calls only an executable's .preinit_array".into())
            }
        }
    }

    fn special(&self, name: &str) -> Option<Special> {
        let placed = |at: fn(&[Region]) -> u64| Some(Special::Placed(at));
        match name {
            "__ehdr_start" | "__executable_start" | "__dso_handle" => Some(Special::ImageBase),
            "__blaze_live" => Some(Special::LiveDescriptor),
            "_DYNAMIC" => placed(dynamic_start),
            "_GLOBAL_OFFSET_TABLE_" => placed(got_base),
            "__init_array_start" => placed(init_array_start),
            "__init_array_end" => placed(init_array_end),
            "__fini_array_start" => placed(fini_array_start),
            "__fini_array_end" => placed(fini_array_end),
            "__preinit_array_start" => placed(preinit_array_start),
            "__preinit_array_end" => placed(preinit_array_end),
            "__GNU_EH_FRAME_HDR" => placed(eh_frame_hdr_start),
            "_TLS_MODULE_BASE_" => placed(tls_start),
            "__bss_start" => placed(bss_start),
            "_edata" | "edata" => placed(edata),
            "_end" | "end" => placed(image_end),
            "etext" | "_etext" | "__etext" => placed(etext),
            _ => {
                let (section, end) = match (name.strip_prefix("__start_"), name.strip_prefix("__stop_")) {
                    (Some(section), _) => (section, false),
                    (_, Some(section)) => (section, true),
                    _ => return None,
                };
                c_identifier(section).then(|| Special::Section { name: SectionName::new(section), end })
            }
        }
    }

    fn read(&self, name: &str, bytes: &[u8], kind: &NativeKind) -> Result<NativeFile, String> {
        read::read(self.output.arch, name, bytes, kind)
    }

    fn synthetic(&self, counts: &Counts) -> Vec<(Class, u64)> {
        dynamic::sizes(&self.output, counts)
    }

    fn apply(&self, r_type: u32, bytes: &mut [u8], p: u64, s: u64, addend: i64) -> Result<(), String> {
        native::apply(self.output.arch, r_type, bytes, p, s, addend)
    }

    fn synthesize(&self, placed: &Placed<'_>) -> Result<Vec<(Class, crate::cow::CowBytes)>, LinkError> {
        Ok(dynamic::Tables::of(self, placed)?.regions(&self.output))
    }

    fn offset(&self, placed: &Placed<'_>, region: &Region) -> Option<u64> {
        in_file(&region.class).then(|| region.addr - placed.plan.base)
    }

    fn file(&self, placed: &Placed<'_>) -> Result<File, LinkError> {
        image::file(self, placed)
    }
}

impl Elf {
    /// The kept tables, locked; `None` for an output that keeps none. A panic mid-build while
    /// they are held drops what they held.
    pub(crate) fn last(&self) -> Option<Reset<'_, Last>> {
        self.last.as_ref().map(reset::lock)
    }

    /// The program headers the headers have room for: a dev executable's reservations add
    /// theirs, so every other image keeps the layout it had.
    pub(crate) fn phdrs(&self) -> u64 {
        PHDRS + if self.output.dev_run.is_some() { RESERVES } else { 0 }
    }

    /// lld's maximum page size: what a `PT_LOAD` is aligned to.
    fn page(&self) -> u64 {
        match self.output.arch {
            Arch::Aarch64 => 0x10000,
            Arch::X86_64 | Arch::Wasm32 => 0x1000,
        }
    }

    pub(crate) fn machine(&self) -> u16 {
        match self.output.arch {
            Arch::Aarch64 => object::elf::EM_AARCH64.0,
            Arch::X86_64 | Arch::Wasm32 => object::elf::EM_X86_64.0,
        }
    }
}

pub(crate) fn error(what: String) -> LinkError {
    LinkError { function: None, what, refusal: None }
}
