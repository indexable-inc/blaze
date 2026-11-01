//! PE: Windows x86-64 executables and DLLs (`x86_64-pc-windows-msvc`), laid out as link.exe
//! lays out what rustc's link line asks of it (`/DYNAMICBASE`, `/HIGHENTROPYVA`, `/NXCOMPAT`,
//! `/OPT:REF`, `/DEBUG` without the PDB).
//!
//! Sections in link.exe's order, each starting on a page: `.text` (code, the jump table's
//! stubs, the import thunks), `.rdata` (read-only data, the import directory, the export
//! directory, the debug directory, the TLS directory, `.CRT$X*`), `.data` (data, the jump
//! table, the import address table, `.bss` last), `.pdata`, `.tls`, `.rsrc` and `.reloc`. A
//! region's file offset is its RVA, so a region never moves in the file while its address
//! stays.
//!
//! No PDB is written: the debug directory's CodeView entry names one by the image's GUID,
//! and that PDB is the one thing a Windows debugger looks for that blaze does not make, so
//! debuggers and `std`'s backtraces (dbghelp) show addresses, not names or lines.
//!
//! A dev run ([`crate::DevRun`]) lays the image out on 64 KiB granules, the unit a view of the
//! code memory maps at: the headers take the first granule, the code sections are one run from
//! the second, every change of protection after them starts a granule, and the reservations
//! (`.barena`, `.bcells`, `.bfixed`: no access, no raw data) come last. Each section's
//! `VirtualSize` reaches the next section, as the NT loader wants them adjacent. The start hook
//! is the first TLS callback (`.CRT$XLAB`, after the CRT's `__xl_a`), the TLS directory is
//! always there, no base relocation lands in code and there is no CFG.

mod coff;
mod image;
#[cfg(test)]
mod tests;

use std::fmt;

use blaze_codegen::Arch;
use blaze_ids::radix;
use parking_lot::Mutex;

use crate::model::{
    Class, Counts, File, NativeFile, Placed, Plan, Prot, Region, Reserve, SectionName, Special, Writer,
};
use crate::reset::{self, Reset};
use crate::{Format, LinkError, NativeKind, Output, OutputKind};

pub(crate) use coff::asked_libraries;

/// Where link.exe loads an executable and a DLL when the loader does not move them.
const EXE_BASE: u64 = 0x1_4000_0000;
const DLL_BASE: u64 = 0x1_8000_0000;
/// `SectionAlignment`, and the room for the headers before the first section.
pub(crate) const PAGE: u64 = 0x1000;
/// Windows' allocation granularity: a dev run's views of the code memory map at it, so its
/// code, the first section after the code and its reservations start on one.
pub(crate) const GRANULE: u64 = 0x1_0000;
/// The function a dev run's first TLS callback points at (`blaze_inproc::runtime`).
pub(crate) const START_HOOK: &str = "blaze_inproc_start";
/// Where the start hook's pointer goes: after the CRT's `__xl_a` (`.CRT$XLA`), before every
/// other TLS callback (std's is `.CRT$XLB`).
const START_HOOK_SECTION: &str = ".CRT$XLAB";

pub(crate) const IDATA: &str = "pe.idata";
pub(crate) const EDATA: &str = "pe.edata";
pub(crate) const DEBUG: &str = "pe.debug";
pub(crate) const TLS_DIR: &str = "pe.tls";
pub(crate) const TLS_INDEX: &str = "pe.tls_index";
pub(crate) const PDATA: &str = "pe.pdata";
pub(crate) const RELOC: &str = "pe.reloc";

/// The native objects' `.pdata`, which the writer merges with codegen's functions into its
/// own, and the resources the resource directory points at.
pub(crate) const OBJ_PDATA: &str = ".pdata";
pub(crate) const RSRC: &str = ".rsrc";
const CRT: &str = ".CRT";

/// What link.exe defines for an image built without `/guard:cf`, which the CRT's load
/// configuration refers to.
const ABSENT: &[&str] = &[
    "__guard_fids_count",
    "__guard_fids_table",
    "__guard_flags",
    "__guard_iat_count",
    "__guard_iat_table",
    "__guard_longjmp_count",
    "__guard_longjmp_table",
    "__guard_eh_cont_count",
    "__guard_eh_cont_table",
    "__enclave_config",
];

pub(crate) fn writer(output: &Output) -> Result<Pe, LinkError> {
    if output.format != Format::Pe {
        return Err(error(format_args!("the PE writer was asked for a {:?} image", output.format)));
    }
    if output.arch != Arch::X86_64 {
        return Err(error(format_args!(
            "a PE image for {:?}: blaze links PE for x86_64-pc-windows-msvc only",
            output.arch
        )));
    }
    if output.dev_run.is_some() && !matches!(output.kind, OutputKind::Executable { .. }) {
        return Err(error("a dev run of a DLL: only an executable has a start hook and a code memory"));
    }
    let last = output.incremental.then(Mutex::default);
    // alloc: once per link: the writer's own copy of the output it makes
    Ok(Pe { output: output.clone(), descriptors: Mutex::default(), last })
}

/// An error, formatted only when it is returned.
pub(crate) fn error(what: impl fmt::Display) -> LinkError {
    // alloc: error path
    LinkError { function: None, what: what.to_string(), refusal: None }
}

/// A [`Class::Named`] of a section name.
pub(crate) fn named(name: SectionName, write: bool, exec: bool) -> Class {
    Class::Named { name, write, exec }
}

pub(crate) struct Pe {
    pub(crate) output: Output,
    /// The DLL each long-format import library names by its `__IMPORT_DESCRIPTOR_<lib>`,
    /// from the descriptor members of the archives read so far: a long import member names
    /// only the descriptor. Written once per archive that has descriptors; read once per long
    /// import member the core reads on its own (an import library read whole uses its own).
    pub(crate) descriptors: Mutex<coff::Descriptors>,
    /// An incremental output's last tables, which the next synthesis patches when its
    /// [`crate::model::Delta`] is from theirs.
    pub(crate) last: Option<Mutex<image::Last>>,
}

impl Pe {
    /// The last tables, when the output is incremental; a synthesis that panicked holding them
    /// leaves them empty, so the next synthesis builds every table.
    pub(crate) fn last(&self) -> Option<Reset<'_, image::Last>> {
        self.last.as_ref().map(reset::lock)
    }

    pub(crate) fn dll(&self) -> bool {
        matches!(self.output.kind, OutputKind::Shared { .. })
    }

    pub(crate) fn base(&self) -> u64 {
        if self.dll() { DLL_BASE } else { EXE_BASE }
    }

    pub(crate) fn dev_run(&self) -> bool {
        self.output.dev_run.is_some()
    }
}

/// Which section of the image a class goes in, in address order.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) enum Segment {
    Text,
    Rdata,
    Data,
    Pdata,
    Tls,
    Rsrc,
    Reloc,
    /// A dev run's reservations, one section each, in [`Reserve`]'s order.
    Arena,
    Cells,
    Fixed,
}

impl Segment {
    pub(crate) fn name(self) -> &'static [u8] {
        match self {
            Segment::Text => b".text",
            Segment::Rdata => b".rdata",
            Segment::Data => b".data",
            Segment::Pdata => b".pdata",
            Segment::Tls => b".tls",
            Segment::Rsrc => b".rsrc",
            Segment::Reloc => b".reloc",
            Segment::Arena => b".barena",
            Segment::Cells => b".bcells",
            Segment::Fixed => b".bfixed",
        }
    }

    pub(crate) fn reserve(self) -> bool {
        matches!(self, Segment::Arena | Segment::Cells | Segment::Fixed)
    }
}

/// Where a class goes: its section, its place in it, and a name ordering the named ones.
pub(crate) fn place(class: &Class) -> (Segment, u8, &str) {
    match class {
        Class::Text => (Segment::Text, 0, ""),
        Class::Patch => (Segment::Text, 1, ""),
        Class::Stubs => (Segment::Text, 2, ""),
        Class::Plt => (Segment::Text, 3, ""),
        Class::Veneers => (Segment::Text, 4, ""),
        Class::Named { name, exec: true, .. } => (Segment::Text, 5, name),
        Class::Rodata => (Segment::Rdata, 0, ""),
        // The loader applies base relocations to read-only pages too: link.exe's `.rdata`
        // holds vtables.
        Class::RelRo => (Segment::Rdata, 1, ""),
        Class::EhFrame => (Segment::Rdata, 2, ""),
        Class::Synthetic(IDATA) => (Segment::Rdata, 3, ""),
        Class::Synthetic(EDATA) => (Segment::Rdata, 4, ""),
        Class::Synthetic(DEBUG) => (Segment::Rdata, 5, ""),
        Class::Synthetic(TLS_DIR) => (Segment::Rdata, 6, ""),
        Class::Named { name, .. } if name == OBJ_PDATA => (Segment::Pdata, 0, name),
        Class::Named { name, .. } if name == RSRC => (Segment::Rsrc, 0, name),
        Class::Named { name, write: false, .. } => (Segment::Rdata, 7, name),
        Class::Data => (Segment::Data, 0, ""),
        Class::Table => (Segment::Data, 1, ""),
        Class::Got => (Segment::Data, 2, ""),
        Class::PatchData => (Segment::Data, 3, ""),
        Class::InitArray => (Segment::Data, 4, ""),
        // ELF's alone: `image::sections` refuses one.
        Class::PreinitArray => (Segment::Data, 4, ""),
        Class::FiniArray => (Segment::Data, 5, ""),
        Class::Named { name, .. } => (Segment::Data, 6, name),
        Class::Synthetic(TLS_INDEX) => (Segment::Data, 7, ""),
        // Mach-O's alone (`Plan::tls_descriptors` is false): `image::file` refuses one.
        Class::ThreadVars => (Segment::Data, 8, ""),
        Class::Bss => (Segment::Data, 9, ""),
        Class::Synthetic(PDATA) => (Segment::Pdata, 1, ""),
        Class::Tdata => (Segment::Tls, 0, ""),
        Class::Tbss => (Segment::Tls, 1, ""),
        Class::Synthetic(RELOC) => (Segment::Reloc, 0, ""),
        Class::Synthetic(name) => (Segment::Rdata, 8, name),
        Class::Reserve(Reserve::Arena) => (Segment::Arena, 0, ""),
        Class::Reserve(Reserve::Cells) => (Segment::Cells, 0, ""),
        Class::Reserve(Reserve::Fixed) => (Segment::Fixed, 0, ""),
    }
}

/// Whether a class's regions are not in the file: zeros, or a reservation.
pub(crate) fn zeroed(class: &Class) -> bool {
    matches!(class, Class::Bss | Class::Tbss | Class::Reserve(_))
}

/// The rank of a grouped section's `$` suffix: link.exe sorts the pieces of a section by the
/// part after the `$`, so `.CRT$XCA` < `.CRT$XCU` < `.CRT$XCZ` and `.tls` < `.tls$` <
/// `.tls$ZZZ`. The first 4 bytes of the suffix, big-endian, order them.
pub(crate) fn rank(suffix: Option<&str>) -> u32 {
    match suffix {
        None => 0,
        Some("") => 1,
        Some(suffix) => {
            let mut key = [0; 4];
            for (to, byte) in key.iter_mut().zip(suffix.bytes()) {
                *to = byte;
            }
            u32::from_be_bytes(key).max(2)
        }
    }
}

impl Writer for Pe {
    fn format(&self) -> Format {
        Format::Pe
    }

    fn arch(&self) -> Arch {
        Arch::X86_64
    }

    fn plan(&self) -> Plan {
        let page = if self.dev_run() { GRANULE } else { PAGE };
        Plan {
            base: self.base(),
            headers: page,
            page,
            indexed_code: false,
            got_by_library: true,
            tls_descriptors: false,
        }
    }

    fn order(&self, classes: &mut Vec<Class>) {
        let rank = |class: &Class| {
            let (segment, rank, _) = place(class);
            radix::pack_32(segment as u32, u32::from(rank))
        };
        radix::sort_by_key(classes, rank);
        for run in classes.chunk_by_mut(|a, b| rank(a) == rank(b)) {
            run.sort_by(|a, b| place(a).2.cmp(place(b).2));
        }
        classes.dedup();
    }

    fn prot(&self, class: &Class) -> Prot {
        if let Class::Named { write, exec, .. } = class {
            return Prot { read: true, write: *write, exec: *exec };
        }
        match place(class).0 {
            Segment::Text => Prot::RX,
            Segment::Data | Segment::Tls => Prot::RW,
            Segment::Rdata | Segment::Pdata | Segment::Rsrc | Segment::Reloc => Prot::R,
            Segment::Arena | Segment::Cells | Segment::Fixed => Prot::NONE,
        }
    }

    fn section(
        &self,
        name: &str,
        write: bool,
        exec: bool,
        tls: bool,
    ) -> Result<(Class, u32), String> {
        let (base, suffix) = match name.split_once('$') {
            Some((base, suffix)) => (base, Some(suffix)),
            None => (name, None),
        };
        let class = match base {
            ".text" => Class::Text,
            ".rdata" | ".xdata" => Class::Rodata,
            ".data" => Class::Data,
            ".bss" => Class::Bss,
            ".tls" => Class::Tdata,
            OBJ_PDATA => named(OBJ_PDATA.into(), false, false),
            CRT => named(CRT.into(), false, false),
            RSRC => named(RSRC.into(), false, false),
            ".idata" | ".edata" | ".reloc" | ".didat" => {
                // alloc: error path
                return Err(format!(
                    "section {name} holds tables the linker makes; no input may define it"
                ));
            }
            _ if tls => Class::Tdata,
            _ => named(SectionName::new(base), write, exec),
        };
        Ok((class, rank(suffix)))
    }

    fn special(&self, name: &str) -> Option<Special> {
        match name {
            "__ImageBase" => Some(Special::ImageBase),
            "_tls_used" => Some(Special::Start(Class::Synthetic(TLS_DIR))),
            "_tls_index" => Some(Special::Start(Class::Synthetic(TLS_INDEX))),
            "__blaze_live" => Some(Special::LiveDescriptor),
            _ if ABSENT.contains(&name) => Some(Special::Absolute(0)),
            _ => name
                .strip_prefix("__imp_")
                .filter(|it| !it.is_empty())
                .map(|_| Special::Got { from: "__imp_".len() as u32 }),
        }
    }

    fn read(&self, name: &str, bytes: &[u8], kind: &NativeKind) -> Result<NativeFile, String> {
        coff::read(self, name, bytes, kind)
    }

    fn start_hook(&self) -> Result<(Class, u32), String> {
        self.section(START_HOOK_SECTION, false, false, false)
    }

    fn synthetic(&self, counts: &Counts) -> Vec<(Class, u64)> {
        image::synthetic(counts, self.dev_run())
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
            "PE has no relocation of its own (type {r_type} at {p:#x}): every AMD64 relocation \
             is read as one the core applies"
        ))
    }

    fn synthesize(&self, placed: &Placed<'_>) -> Result<Vec<(Class, crate::cow::CowBytes)>, LinkError> {
        image::synthesize(self, placed)
    }

    fn offset(&self, placed: &Placed<'_>, region: &Region) -> Option<u64> {
        if zeroed(&region.class) { None } else { Some(region.addr - placed.plan.base) }
    }

    fn file(&self, placed: &Placed<'_>) -> Result<File, LinkError> {
        image::file(self, placed)
    }

    fn shared_entry(&self) -> Option<&'static str> {
        self.dll().then_some("_DllMainCRTStartup")
    }

    fn own(&self, class: &Class) -> (Class, u32) {
        match class {
            Class::Tdata | Class::Tbss => (Class::Tdata, rank(Some(""))),
            // clone: the core asks once per class and keeps the answer (`stage.rs`'s `own`)
            _ => (class.clone(), u32::MAX),
        }
    }
}
