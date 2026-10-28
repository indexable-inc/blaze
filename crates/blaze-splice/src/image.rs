//! Patching onto an image file: the same patch as the splice's, written into an executable on
//! disk that blaze lays out itself (docs/patching.md).
//!
//! A cold build is a patch onto an empty image. [`Layout::empty`] has nothing placed, so every
//! function and every piece of data of the inputs is new, and the planner places each of them
//! where the layout has room: behind what is there, which in an empty layout is nothing. A
//! later stage keeps the layout and patches the image it describes; the planner is the same.
//!
//! This is stage 1 of docs/patching.md: an ELF executable for x86-64 and AArch64 Linux, enough
//! for a program over blaze's standard library. The inputs are what codegen writes today (its
//! objects, which hold the store's functions as they were stitched) and the C runtime's files
//! (start files, `libc.so` and its `GROUP` script, `libgcc_s`), with the shared libraries read
//! only for the names they define and their versions. The image is a PIE, bound at load
//! (`BIND_NOW`), with `PT_INTERP`, `PT_TLS`, `PT_GNU_STACK`, `PT_GNU_RELRO`, `.eh_frame_hdr`
//! and a build ID. What reaches nothing from `_start` and the start-up tables is not placed.
//!
//! Nothing here falls back: an input, a relocation or a symbol the patch has no answer for is
//! an error that names it and where it is. No product path writes images yet;
//! `harness/coldimage.py` writes hello both ways, with this and with lld, and compares them.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail, ensure};
use object::read::archive::ArchiveFile;
use object::read::elf::{ElfFile64, FileHeader as _, SectionHeader as _, Sym as _};
use object::{
    Endianness, Object, ObjectComdat, ObjectSection, ObjectSymbol, RelocationFlags,
    RelocationTarget, SectionIndex, SymbolSection, elf,
};
use rustc_hash::{FxHashMap, FxHashSet};
use serde_derive::Deserialize;

use super::Machine;

mod write;

/// What `blaze patch-image` patches onto an empty image, as one JSON object.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub machine: Arch,
    /// The image to write.
    pub output: PathBuf,
    /// The dynamic loader, as `PT_INTERP` names it.
    pub interpreter: String,
    /// What the program starts at: `_start` when not said.
    #[serde(default)]
    pub entry: Option<String>,
    /// Where `lib` inputs, and the `-l` names of a linker script, are looked for, in order.
    #[serde(default)]
    pub search: Vec<PathBuf>,
    /// The inputs in the order the program's line has them: it decides which definition of a
    /// name wins and which archive member is taken.
    pub inputs: Vec<Input>,
    /// What becomes of the inputs' DWARF. Only `leave_out` is written yet (stage 2 writes it):
    /// the caller says so, so an image without its debug info is never a quiet difference.
    pub debug_info: DebugInfo,
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Arch {
    #[serde(rename = "x86_64")]
    X86_64,
    #[serde(rename = "aarch64")]
    Aarch64,
}

/// One input: a file (an object, an archive, a shared library or a linker script, told by its
/// bytes), or a library by name (`lib`, as `-l` has it: `libNAME.so`, then `libNAME.a`, or the
/// file itself for `:NAME`).
#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Input {
    #[serde(default)]
    pub path: Option<PathBuf>,
    #[serde(default)]
    pub lib: Option<String>,
    /// A shared library is needed only when something placed refers to it (`--as-needed`).
    #[serde(default)]
    pub as_needed: bool,
    /// Every member of an archive is taken, not only those that define what is missing.
    #[serde(default)]
    pub whole: bool,
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq)]
pub enum DebugInfo {
    #[serde(rename = "leave_out")]
    LeaveOut,
    #[serde(rename = "keep")]
    Keep,
}

/// Where each placed piece of the image is. Empty for a cold build. Stage 4 keeps it with the
/// image and patches into it; until then every patch is onto an empty one.
#[derive(Default)]
pub struct Layout {
    /// The end of what is placed, per output section, by name.
    placed: FxHashMap<String, u64>,
}

impl Layout {
    pub fn empty() -> Layout {
        Layout::default()
    }

    /// Where a piece of `size` bytes aligned to `align` goes in the output section `section`:
    /// behind what is placed there.
    fn place(&mut self, section: &str, size: u64, align: u64) -> u64 {
        let end = self.placed.entry(section.to_owned()).or_default();
        let at = end.next_multiple_of(align.max(1));
        *end = at + size;
        at
    }
}

/// What a patch onto an image wrote, for the log.
pub struct Patched {
    pub placed: usize,
    pub bytes: u64,
    pub imports: usize,
}

/// `blaze patch-image INPUT`: INPUT is a [`Request`] in JSON, inline, a file's path, or `-`
/// for stdin. Writes the image the request names, onto an empty layout.
pub fn command(mut args: impl Iterator<Item = String>) -> anyhow::Result<()> {
    let given = args.next().context("usage: blaze patch-image INPUT (a JSON object, a path or -)")?;
    ensure!(args.next().is_none(), "blaze patch-image takes one INPUT");
    let text = if given == "-" {
        std::io::read_to_string(std::io::stdin()).context("reading INPUT from stdin")?
    } else if given.trim_start().starts_with('{') {
        given
    } else {
        std::fs::read_to_string(&given).with_context(|| format!("reading INPUT {given}"))?
    };
    let request: Request = serde_json::from_str(&text).context("INPUT is not a request")?;
    let started = std::time::Instant::now();
    let patched = patch(&mut Layout::empty(), &request)?;
    eprintln!(
        "patched {} pieces onto an empty image: {} ({} bytes, {} imports) in {:.1?}",
        patched.placed,
        request.output.display(),
        patched.bytes,
        patched.imports,
        started.elapsed()
    );
    Ok(())
}

/// Patches what `request` names onto the image `layout` describes, and writes the image.
pub fn patch(layout: &mut Layout, request: &Request) -> anyhow::Result<Patched> {
    ensure!(
        layout.placed.is_empty(),
        "patching an image that has a layout comes in stage 4 (docs/patching.md); this one has one"
    );
    let machine = match request.machine {
        Arch::X86_64 => Machine::X86_64,
        Arch::Aarch64 => Machine::Aarch64,
    };
    let files = Files::expand(request)?;
    let mut link = Link::new(machine, &files);
    for (at, file) in files.list.iter().enumerate() {
        link.add(at, file)?;
    }
    link.define_synthetic();
    if request.debug_info == DebugInfo::Keep
        && let Some(object) = link.objects.iter().find(|it| it.has_debug)
    {
        bail!(
            "{}: has debug info, and writing DWARF into an image comes in stage 2 \
             (docs/patching.md); ask for debug_info: leave_out",
            object.name
        );
    }
    let entry = request.entry.as_deref().unwrap_or("_start");
    let image = write::write(&mut link, layout, request, entry)?;
    std::fs::write(&request.output, &image.bytes)
        .with_context(|| format!("writing {}", request.output.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&request.output, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("making {} executable", request.output.display()))?;
    }
    Ok(Patched { placed: image.placed, bytes: image.bytes.len() as u64, imports: image.imports })
}

/// The inputs, every linker script and library name followed to the files they mean, in order.
struct Files {
    maps: Vec<memmap2::Mmap>,
    list: Vec<File>,
}

struct File {
    /// Which of [`Files::maps`].
    map: usize,
    name: String,
    kind: Kind,
    as_needed: bool,
    whole: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Object,
    Archive,
    Shared,
}

impl Files {
    fn expand(request: &Request) -> anyhow::Result<Files> {
        let mut files = Files { maps: Vec::new(), list: Vec::new() };
        for input in &request.inputs {
            let path = match (&input.path, &input.lib) {
                (Some(path), None) => path.clone(),
                (None, Some(lib)) => find_lib(lib, &request.search)?,
                _ => bail!("an input has a `path` or a `lib`, and one of them"),
            };
            files.add(&path, input.as_needed, input.whole, &request.search, 0)?;
        }
        Ok(files)
    }

    fn add(
        &mut self,
        path: &Path,
        as_needed: bool,
        whole: bool,
        search: &[PathBuf],
        depth: usize,
    ) -> anyhow::Result<()> {
        ensure!(depth < 8, "{}: linker scripts that include each other", path.display());
        let file =
            std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
        // SAFETY: the inputs are the build's and the C toolchain's files, which nothing
        // writes while the image is made.
        let map = unsafe { memmap2::Mmap::map(&file) }
            .with_context(|| format!("reading {}", path.display()))?;
        let name = path.display().to_string();
        let kind = if map.starts_with(b"\x7fELF") {
            let file = ElfFile64::<Endianness>::parse(&*map)
                .with_context(|| format!("{name}: not a 64-bit ELF file"))?;
            match file.elf_header().e_type(file.endian()) {
                elf::ET_REL => Kind::Object,
                elf::ET_DYN => Kind::Shared,
                other => bail!("{name}: an ELF file of type {other}, neither an object nor a shared library"),
            }
        } else if map.starts_with(b"!<arch>\n") {
            Kind::Archive
        } else if map.starts_with(b"!<thin>\n") {
            bail!("{name}: a thin archive, which blaze's patching does not read yet (docs/patching.md)")
        } else {
            let text = std::str::from_utf8(&map)
                .with_context(|| format!("{name}: neither ELF, an archive nor a linker script"))?;
            let dir = path.parent().unwrap_or(Path::new("."));
            for (item, needed) in script(text).with_context(|| format!("{name}: as a linker script"))? {
                let path = match item.strip_prefix("-l") {
                    Some(lib) => find_lib(lib, search)?,
                    None if Path::new(&item).is_absolute() => PathBuf::from(&item),
                    // A name alone is looked for where the script is, then in the search
                    // directories, as GNU ld and lld look.
                    None => std::iter::once(dir.to_owned())
                        .chain(search.iter().cloned())
                        .map(|it| it.join(&item))
                        .find(|it| it.is_file())
                        .with_context(|| format!("{name}: {item} is in no search directory"))?,
                };
                self.add(&path, as_needed || needed, false, search, depth + 1)?;
            }
            return Ok(());
        };
        self.maps.push(map);
        self.list.push(File { map: self.maps.len() - 1, name, kind, as_needed, whole });
        Ok(())
    }
}

/// `libNAME.so`, then `libNAME.a`, in the first directory of `search` that has either;
/// `:NAME` is the file itself.
fn find_lib(lib: &str, search: &[PathBuf]) -> anyhow::Result<PathBuf> {
    let names = match lib.strip_prefix(':') {
        Some(file) => vec![file.to_owned()],
        None => vec![format!("lib{lib}.so"), format!("lib{lib}.a")],
    };
    for dir in search {
        for name in &names {
            let path = dir.join(name);
            if path.is_file() {
                return Ok(path);
            }
        }
    }
    bail!("-l{lib}: none of {} is in the search directories {search:?}", names.join(", "))
}

/// The files a linker script of the kind system libraries ship names: `GROUP`, `INPUT` and
/// `AS_NEEDED` in them, with whether each is as needed. `OUTPUT_FORMAT` and `OUTPUT_ARCH` say
/// nothing the image does not. Anything else is an error: the full script language is not
/// taken (docs/patching.md).
fn script(text: &str) -> anyhow::Result<Vec<(String, bool)>> {
    let mut clean = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("/*") {
        clean.push_str(&rest[..start]);
        let end = rest[start..].find("*/").context("a comment that does not end")?;
        rest = &rest[start + end + 2..];
    }
    clean.push_str(rest);
    let spaced = clean.replace('(', " ( ").replace(')', " ) ");
    let mut tokens = spaced.split_whitespace().peekable();
    let mut files = Vec::new();
    while let Some(token) = tokens.next() {
        match token {
            "GROUP" | "INPUT" => {
                ensure!(tokens.next() == Some("("), "{token} without (");
                let mut needed = false;
                loop {
                    match tokens.next().context("a GROUP that does not end")? {
                        ")" if needed => needed = false,
                        ")" => break,
                        "AS_NEEDED" => {
                            ensure!(tokens.next() == Some("("), "AS_NEEDED without (");
                            needed = true;
                        }
                        file => files.push((file.trim_end_matches(',').to_owned(), needed)),
                    }
                }
            }
            "OUTPUT_FORMAT" | "OUTPUT_ARCH" => {
                ensure!(tokens.next() == Some("("), "{token} without (");
                while tokens.next().context("a list that does not end")? != ")" {}
            }
            other => bail!("`{other}`, which blaze's patching does not take in a linker script"),
        }
    }
    Ok(files)
}

/// Where a relocation's target is, before the image has addresses.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum To {
    /// In a section of an object, this far in.
    At { object: u32, section: u32, offset: u64 },
    Abs(u64),
    Global(u32),
}

#[derive(Clone, Copy)]
pub(crate) struct Reloc {
    pub(crate) offset: u64,
    pub(crate) r_type: u32,
    pub(crate) to: To,
    pub(crate) addend: i64,
}

pub(crate) struct Section<'a> {
    pub(crate) name: &'a str,
    pub(crate) sh_type: u32,
    pub(crate) flags: u64,
    pub(crate) align: u64,
    pub(crate) size: u64,
    pub(crate) data: &'a [u8],
    pub(crate) relocs: Vec<Reloc>,
    /// Loaded, and not a copy of a group another object has.
    pub(crate) kept: bool,
    pub(crate) live: bool,
    /// Its output section, once placed.
    pub(crate) out: u32,
    /// Where it is in the output section.
    pub(crate) at: u64,
}

pub(crate) struct Obj<'a> {
    pub(crate) name: String,
    pub(crate) sections: Vec<Section<'a>>,
    /// Its symbols that are defined: name, section, value, size and `st_info`, for the
    /// image's symbol table.
    pub(crate) symbols: Vec<(&'a str, u32, u64, u64, u8)>,
    pub(crate) has_debug: bool,
    pub(crate) has_property_note: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Def {
    None,
    At { object: u32, section: u32, offset: u64 },
    Abs(u64),
    /// Defined by a shared library, at a version of its own (an index of [`Lib::versions`]).
    Shared { lib: u32, version: Option<u32> },
    /// One the image defines: where a table or a section starts or ends.
    Synthetic,
}

pub(crate) struct Global<'a> {
    pub(crate) name: &'a str,
    pub(crate) def: Def,
    pub(crate) weak_def: bool,
    /// Whether some object refers to it other than weakly.
    pub(crate) strong_ref: bool,
    pub(crate) referred: bool,
    pub(crate) tls: bool,
    /// What the shared library that defines it says it is (`STT_*`).
    pub(crate) shared_type: u8,
    /// The archive member that defines it, for when something refers to it.
    lazy: Option<(usize, u64)>,
}

pub(crate) struct Lib<'a> {
    pub(crate) soname: &'a str,
    pub(crate) as_needed: bool,
    pub(crate) versions: Vec<(&'a str, u32)>,
}

/// The inputs read, and every name resolved to its definition.
pub(crate) struct Link<'a> {
    pub(crate) machine: Machine,
    files: &'a Files,
    pub(crate) objects: Vec<Obj<'a>>,
    pub(crate) libs: Vec<Lib<'a>>,
    pub(crate) globals: Vec<Global<'a>>,
    pub(crate) names: FxHashMap<&'a str, u32>,
    groups: FxHashSet<&'a str>,
    fetched: FxHashSet<(usize, u64)>,
    pending: Vec<(usize, u64)>,
}

/// The names the image defines when an input refers to one and none defines it, besides
/// `__start_SECTION` and `__stop_SECTION`.
pub(crate) const SYNTHETIC: &[&str] = &[
    "__ehdr_start",
    "__executable_start",
    "_DYNAMIC",
    "_GLOBAL_OFFSET_TABLE_",
    "__init_array_start",
    "__init_array_end",
    "__fini_array_start",
    "__fini_array_end",
    "__preinit_array_start",
    "__preinit_array_end",
    "__rela_iplt_start",
    "__rela_iplt_end",
    "__bss_start",
    "_edata",
    "edata",
    "_end",
    "end",
    "_etext",
    "etext",
    "__etext",
    "__GNU_EH_FRAME_HDR",
];

impl<'a> Link<'a> {
    fn new(machine: Machine, files: &'a Files) -> Link<'a> {
        Link {
            machine,
            files,
            objects: Vec::new(),
            libs: Vec::new(),
            globals: Vec::new(),
            names: FxHashMap::default(),
            groups: FxHashSet::default(),
            fetched: FxHashSet::default(),
            pending: Vec::new(),
        }
    }

    fn global(&mut self, name: &'a str) -> u32 {
        *self.names.entry(name).or_insert_with(|| {
            self.globals.push(Global {
                name,
                def: Def::None,
                weak_def: false,
                strong_ref: false,
                referred: false,
                tls: false,
                shared_type: elf::STT_NOTYPE,
                lazy: None,
            });
            (self.globals.len() - 1) as u32
        })
    }

    fn add(&mut self, at: usize, file: &'a File) -> anyhow::Result<()> {
        let bytes: &'a [u8] = &self.files.maps[file.map];
        match file.kind {
            Kind::Object => self.add_object(file.name.clone(), bytes)?,
            Kind::Shared => self.add_shared(&file.name, bytes, file.as_needed)?,
            Kind::Archive => {
                let archive = ArchiveFile::parse(bytes)
                    .with_context(|| format!("{}: not an archive", file.name))?;
                if file.whole {
                    for member in archive.members() {
                        let member = member.with_context(|| format!("{}: a member", file.name))?;
                        let name = format!(
                            "{}({})",
                            file.name,
                            String::from_utf8_lossy(member.name())
                        );
                        let data = member.data(bytes).with_context(|| format!("reading {name}"))?;
                        if data.starts_with(b"\x7fELF") {
                            self.add_object(name, data)?;
                        }
                    }
                } else {
                    let symbols = archive.symbols().with_context(|| {
                        format!("{}: reading the archive's symbol index", file.name)
                    })?;
                    let symbols = symbols.with_context(|| {
                        format!("{}: an archive without a symbol index", file.name)
                    })?;
                    for symbol in symbols {
                        let symbol = symbol.with_context(|| format!("{}: its index", file.name))?;
                        let name = std::str::from_utf8(symbol.name())
                            .with_context(|| format!("{}: a name that is not UTF-8", file.name))?;
                        let member = (at, symbol.offset().0);
                        let id = self.global(name);
                        let global = &mut self.globals[id as usize];
                        if global.def != Def::None || global.lazy.is_some() {
                            continue;
                        }
                        global.lazy = Some(member);
                        if global.strong_ref {
                            self.pending.push(member);
                        }
                    }
                }
            }
        }
        while let Some((file, offset)) = self.pending.pop() {
            if !self.fetched.insert((file, offset)) {
                continue;
            }
            let entry = &self.files.list[file];
            let bytes: &'a [u8] = &self.files.maps[entry.map];
            let archive = ArchiveFile::parse(bytes)?;
            let member = archive
                .member(object::read::archive::ArchiveOffset(offset))
                .with_context(|| format!("{}: the member at {offset}", entry.name))?;
            let name = format!("{}({})", entry.name, String::from_utf8_lossy(member.name()));
            let data = member.data(bytes).with_context(|| format!("reading {name}"))?;
            self.add_object(name, data)?;
        }
        Ok(())
    }

    fn add_object(&mut self, name: String, bytes: &'a [u8]) -> anyhow::Result<()> {
        let file = ElfFile64::<Endianness>::parse(bytes).with_context(|| format!("reading {name}"))?;
        let e = file.endian();
        let machine = match file.elf_header().e_machine(e) {
            elf::EM_X86_64 => Machine::X86_64,
            elf::EM_AARCH64 => Machine::Aarch64,
            other => bail!("{name}: an object for machine {other}"),
        };
        ensure!(machine == self.machine, "{name}: an object for another machine than the image's");
        let index = self.objects.len() as u32;
        let count = file.elf_section_table().len();
        let mut sections: Vec<Section<'a>> = (0..count)
            .map(|_| Section {
                name: "",
                sh_type: 0,
                flags: 0,
                align: 1,
                size: 0,
                data: &[],
                relocs: Vec::new(),
                kept: false,
                live: false,
                out: u32::MAX,
                at: 0,
            })
            .collect();
        let mut has_debug = false;
        let mut has_property_note = false;
        for section in file.sections() {
            let header = section.elf_section_header();
            let sh_type = header.sh_type(e);
            let flags = header.sh_flags(e);
            let section_name = section.name().with_context(|| format!("{name}: a section's name"))?;
            if section_name.starts_with(".debug_") {
                has_debug = true;
            }
            if section_name == ".note.gnu.property" {
                has_property_note = true;
                continue;
            }
            let slot = &mut sections[section.index().0];
            slot.name = section_name;
            slot.sh_type = sh_type;
            slot.flags = flags;
            slot.align = section.align().max(1);
            slot.size = section.size();
            slot.kept = flags & u64::from(elf::SHF_ALLOC) != 0 && sh_type != elf::SHT_GROUP;
            if slot.kept && sh_type != elf::SHT_NOBITS {
                slot.data = section.data().with_context(|| format!("{name}: {section_name}"))?;
            }
        }
        for comdat in file.comdats() {
            let signature = comdat.name().with_context(|| format!("{name}: a group's name"))?;
            if !self.groups.insert(signature) {
                for section in comdat.sections() {
                    sections[section.0].kept = false;
                }
            }
        }
        // Each symbol of the object, by its index, as a relocation reaches it.
        let mut to = vec![To::Abs(0); file.elf_symbol_table().len()];
        let mut symbols = Vec::new();
        for symbol in file.symbols() {
            let raw = symbol.elf_symbol();
            let sym_name = symbol.name().with_context(|| format!("{name}: a symbol's name"))?;
            let st_type = raw.st_type();
            let place = match symbol.section() {
                SymbolSection::Section(SectionIndex(section)) => Some(section),
                SymbolSection::Absolute => None,
                SymbolSection::Undefined | SymbolSection::None => None,
                SymbolSection::Common => {
                    bail!("{name}: `{sym_name}` is a common symbol, which blaze's patching does not place yet")
                }
                _ => bail!("{name}: `{sym_name}` is in a section blaze's patching does not know"),
            };
            if st_type == elf::STT_GNU_IFUNC && place.is_some() {
                bail!("{name}: `{sym_name}` is an IFUNC, which blaze's patching places in stage 2 (docs/patching.md)");
            }
            let defined_here = match place {
                Some(section) => sections[section].kept,
                None => symbol.section() == SymbolSection::Absolute,
            };
            if symbol.is_local() {
                to[symbol.index().0] = match place {
                    Some(section) => To::At {
                        object: index,
                        section: section as u32,
                        offset: symbol.address(),
                    },
                    None => To::Abs(symbol.address()),
                };
                if let Some(section) = place
                    && defined_here
                    && matches!(st_type, elf::STT_FUNC | elf::STT_OBJECT | elf::STT_NOTYPE)
                    && !sym_name.is_empty()
                {
                    symbols.push((sym_name, section as u32, symbol.address(), symbol.size(), raw.st_info));
                }
                continue;
            }
            let id = self.global(sym_name);
            to[symbol.index().0] = To::Global(id);
            let global = &mut self.globals[id as usize];
            global.tls |= st_type == elf::STT_TLS;
            if !defined_here {
                // Undefined here, or defined in a group another object has: that one's
                // definition is the one.
                global.referred = true;
                if !symbol.is_weak() && symbol.section() == SymbolSection::Undefined {
                    global.strong_ref = true;
                    if global.def == Def::None
                        && let Some(member) = global.lazy.take()
                    {
                        self.pending.push(member);
                    }
                }
                continue;
            }
            let def = match place {
                Some(section) => Def::At { object: index, section: section as u32, offset: symbol.address() },
                None => Def::Abs(symbol.address()),
            };
            let weak = symbol.is_weak();
            match global.def {
                Def::None | Def::Shared { .. } | Def::Synthetic => {
                    global.def = def;
                    global.weak_def = weak;
                }
                _ if weak => {}
                _ if global.weak_def => {
                    global.def = def;
                    global.weak_def = false;
                }
                _ => bail!("{name}: `{sym_name}` is defined here and in another input"),
            }
            if let Some(section) = place {
                symbols.push((sym_name, section as u32, symbol.address(), symbol.size(), raw.st_info));
            }
        }
        for section in file.sections() {
            let slot = section.index().0;
            if !sections[slot].kept {
                continue;
            }
            let mut relocs = Vec::new();
            for (offset, relocation) in section.relocations() {
                let RelocationFlags::Elf { r_type } = relocation.flags() else {
                    bail!("{name}: a relocation that is not ELF's")
                };
                let target = match relocation.target() {
                    RelocationTarget::Symbol(symbol) => *to.get(symbol.0).with_context(|| {
                        format!("{name}: a relocation of {} to a symbol that is not there", sections[slot].name)
                    })?,
                    RelocationTarget::Absolute => To::Abs(0),
                    _ => bail!("{name}: a relocation of {} to no symbol", sections[slot].name),
                };
                relocs.push(Reloc { offset, r_type, to: target, addend: relocation.addend() });
            }
            sections[slot].relocs = relocs;
        }
        self.objects.push(Obj { name, sections, symbols, has_debug, has_property_note });
        Ok(())
    }

    fn add_shared(&mut self, name: &str, bytes: &'a [u8], as_needed: bool) -> anyhow::Result<()> {
        let file = ElfFile64::<Endianness>::parse(bytes).with_context(|| format!("reading {name}"))?;
        let e = file.endian();
        let table = file.elf_section_table();
        let mut soname: Option<&'a str> = None;
        if let Some((dynamic, link)) = table.dynamic(e, bytes)? {
            let strings = table.strings(e, bytes, link)?;
            for entry in dynamic {
                if entry.d_tag.get(e) == u64::from(elf::DT_SONAME) {
                    let text = strings
                        .get(entry.d_val.get(e) as u32)
                        .map_err(|_| anyhow::anyhow!("{name}: its DT_SONAME"))?;
                    soname = Some(std::str::from_utf8(text)?);
                }
            }
        }
        let soname = match soname {
            Some(it) => it,
            None => bail!("{name}: a shared library without DT_SONAME, which blaze's patching does not name yet"),
        };
        let lib = self.libs.len() as u32;
        let versions = table.versions(e, bytes).with_context(|| format!("{name}: its versions"))?;
        let mut names: Vec<(&'a str, u32)> = Vec::new();
        let mut index_of: FxHashMap<u16, u32> = FxHashMap::default();
        for symbol in file.dynamic_symbols() {
            if symbol.is_undefined() || symbol.is_local() {
                continue;
            }
            let sym_name = symbol.name().with_context(|| format!("{name}: a symbol's name"))?;
            let mut version = None;
            if let Some(versions) = &versions {
                let at = versions.version_index(e, symbol.index());
                // `name@VERSION` that is not the default (`@@`) is for programs linked before
                // it: an unversioned reference binds the default one.
                if at.is_hidden() {
                    continue;
                }
                if let Some(found) = versions.version(at)? {
                    let next = names.len() as u32;
                    let id = *index_of.entry(at.index()).or_insert_with(|| {
                        let text = std::str::from_utf8(found.name()).unwrap_or("");
                        names.push((text, found.hash()));
                        next
                    });
                    version = Some(id);
                }
            }
            let id = self.global(sym_name);
            let global = &mut self.globals[id as usize];
            let st_type = symbol.elf_symbol().st_type();
            global.tls |= st_type == elf::STT_TLS;
            if global.def == Def::None && global.lazy.is_none() {
                global.def = Def::Shared { lib, version };
                global.shared_type = st_type;
            }
        }
        self.libs.push(Lib { soname, as_needed, versions: names });
        Ok(())
    }

    /// The names the image itself defines, for what refers to one and found none.
    fn define_synthetic(&mut self) {
        for global in &mut self.globals {
            let starts = global.name.starts_with("__start_") || global.name.starts_with("__stop_");
            if global.def == Def::None
                && global.referred
                && (starts || SYNTHETIC.contains(&global.name))
            {
                global.def = Def::Synthetic;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::script;

    #[test]
    fn glibc_scripts() {
        let libc = "/* GNU ld script\n   Use the shared library */\nOUTPUT_FORMAT(elf64-x86-64)\n\
                    GROUP ( /lib/x86_64-linux-gnu/libc.so.6 /usr/lib/x86_64-linux-gnu/libc_nonshared.a \
                    AS_NEEDED ( /lib64/ld-linux-x86-64.so.2 ) )\n";
        assert_eq!(
            script(libc).unwrap(),
            vec![
                ("/lib/x86_64-linux-gnu/libc.so.6".to_owned(), false),
                ("/usr/lib/x86_64-linux-gnu/libc_nonshared.a".to_owned(), false),
                ("/lib64/ld-linux-x86-64.so.2".to_owned(), true),
            ]
        );
        assert_eq!(
            script("GROUP ( libgcc_s.so.1 -lgcc )").unwrap(),
            vec![("libgcc_s.so.1".to_owned(), false), ("-lgcc".to_owned(), false)]
        );
        assert!(script("SECTIONS { }").is_err());
    }
}
