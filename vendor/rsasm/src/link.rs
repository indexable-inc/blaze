//! Linking assembled objects into a program, through the qld linker.
//!
//! An assembler stops at a relocatable object: the addresses are not final,
//! nothing has resolved a reference from one file to another, and no loader
//! will run it. qld is a linker written in Rust, and a library as well as a
//! program, so the step that turns objects into an executable can happen in
//! the process that assembled them. [`link`] takes object bytes and returns
//! the bytes of the image, with no temporary file anywhere in between, and
//! everything qld has to say about the link arrives as ordinary rsasm
//! diagnostics rather than on a standard stream of its own.
//!
//! This module is the whole of the `link` cargo feature, and qld is the
//! whole of rsasm's dependency list. Building with `--no-default-features`
//! leaves both out.

use crate::diag::{DiagBag, Diagnostic, Severity};
use crate::output::Format;
use crate::source::Span;
use std::path::PathBuf;

/// ELF `e_machine` for x86-64.
const EM_X86_64: u16 = 62;
/// ELF `e_machine` for AArch64.
const EM_AARCH64: u16 = 183;

/// What the link produces.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
#[non_exhaustive]
pub enum Kind {
    /// An executable that carries everything it needs and names no
    /// interpreter. This is the default, because it is the only kind that
    /// can be linked from assembly alone.
    #[default]
    StaticExecutable,
    /// An executable that a dynamic loader completes, at a fixed address.
    Executable,
    /// A position-independent executable. Without a dynamic linker it is a
    /// static PIE.
    Pie,
    /// A shared library.
    Shared,
}

/// How to link: everything [`link`] needs beyond the objects themselves.
///
/// Build it with [`Options::new`] and the `with_*` methods; the struct is
/// `#[non_exhaustive]` because it will grow as more of qld is reached.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct Options {
    kind: Kind,
    target: Option<(Format, u16)>,
    entry: Option<String>,
    libraries: Vec<String>,
    search_paths: Vec<PathBuf>,
    dynamic_linker: Option<PathBuf>,
}

impl Options {
    /// Options for a static ELF executable with the target's usual entry
    /// symbol, no libraries and no search path.
    pub fn new() -> Options {
        Options::default()
    }

    /// What the link produces.
    pub fn with_kind(mut self, kind: Kind) -> Options {
        self.kind = kind;
        self
    }

    /// The object format and machine being linked, the machine named by
    /// its ELF number as
    /// [`arch::Architecture::elf_machine`](crate::arch::Architecture::elf_machine)
    /// gives it. [`supports`] says which pairs qld accepts.
    ///
    /// An ELF link needs none of this, because qld reads the class, the
    /// byte order and the machine out of the first object. A PE32+ link
    /// does: nothing in a COFF object says which of the two containers it
    /// is destined for.
    pub fn with_target(mut self, format: Format, elf_machine: u16) -> Options {
        self.target = Some((format, elf_machine));
        self
    }

    /// The entry symbol, as `-e` names it. Left unset, the target's usual
    /// one is used, which is `_start` everywhere rsasm can link.
    pub fn with_entry(mut self, symbol: impl Into<String>) -> Options {
        self.entry = Some(symbol.into());
        self
    }

    /// A library to link against, as `-l` names it: `c` for `libc`, or a
    /// name starting with `:` for an exact file name in the search path,
    /// which is how a startup file such as `:crt1.o` is reached.
    ///
    /// Libraries are searched in the order they were added, after every
    /// object.
    pub fn with_library(mut self, name: impl Into<String>) -> Options {
        self.libraries.push(name.into());
        self
    }

    /// A directory to search for libraries in, as `-L` names it.
    pub fn with_search_path(mut self, dir: impl Into<PathBuf>) -> Options {
        self.search_paths.push(dir.into());
        self
    }

    /// The program interpreter to record, as `--dynamic-linker` names it.
    /// Setting it is what makes a [`Kind::Pie`] link a dynamic one rather
    /// than a static PIE.
    pub fn with_dynamic_linker(mut self, path: impl Into<PathBuf>) -> Options {
        self.dynamic_linker = Some(path.into());
        self
    }
}

/// A link that produced no program.
///
/// The individual problems have already been reported to the [`DiagBag`]
/// [`link`] was given; this is what remains to say about the run as a
/// whole, so that a driver can fail with it rather than panic.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Error {
    /// What went wrong, in qld's words.
    pub message: String,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

/// Whether qld links objects of this format for this ELF machine.
///
/// qld reads and writes ELF and PE32+ for x86-64 and AArch64, which is a
/// part of what rsasm assembles, so a driver that offers to link asks first
/// and says plainly what it cannot do. The machine is named by its ELF
/// number even for a PE/COFF object, since that is what
/// [`arch::Architecture::elf_machine`](crate::arch::Architecture::elf_machine)
/// returns.
pub fn supports(format: Format, elf_machine: u16) -> bool {
    matches!(format, Format::Elf | Format::Coff) && matches!(elf_machine, EM_X86_64 | EM_AARCH64)
}

/// Links `objects` into a program and returns its bytes.
///
/// Each object is a name to show in diagnostics — normally the file the
/// assembler would have written — and the bytes of a relocatable object, in
/// the order they should be linked. Libraries named in `options` are
/// searched after all of them.
///
/// Warnings and errors from the link are emitted into `diags`, in qld's
/// deterministic order, and rendered by
/// [`DiagBag::render`](crate::diag::DiagBag::render) like any other rsasm
/// diagnostic. A failure is an [`Error`], never a panic and never a message
/// on a standard stream: the link qld runs here reads no environment
/// variable, writes nothing and does not exit the process.
///
/// # Errors
///
/// Returns an [`Error`] when the link did not produce an image, whether
/// because errors were reported, because an input could not be read, or
/// because qld does not implement what was asked of it.
pub fn link(
    objects: Vec<(String, Vec<u8>)>,
    options: &Options,
    diags: &mut DiagBag,
) -> Result<Vec<u8>, Error> {
    let mut request = qld::LinkOptions::new();
    request.kind = match (options.kind, options.dynamic_linker.is_some()) {
        (Kind::StaticExecutable, _) => qld::OutputKind::StaticExecutable,
        (Kind::Executable, _) => qld::OutputKind::Executable,
        (Kind::Pie, true) => qld::OutputKind::Pie,
        (Kind::Pie, false) => qld::OutputKind::StaticPie,
        (Kind::Shared, _) => qld::OutputKind::Shared,
    };
    request.target = qld_target(options.target);
    request.entry = options.entry.clone();
    request.dynamic_linker = options.dynamic_linker.clone();
    request.search_paths = options.search_paths.clone();
    for (name, bytes) in objects {
        request.push_input(
            qld::InputKind::bytes(name, bytes),
            qld::InputAttrs::default(),
        );
    }
    for name in &options.libraries {
        // `-l:libfoo.a` names a file rather than a library, and is how a
        // startup file such as `crt1.o` is found in a search directory.
        let kind = match name.strip_prefix(':') {
            Some(file) => qld::InputKind::LibraryExact(file.to_string()),
            None => qld::InputKind::Library(name.clone()),
        };
        request.push_input(kind, qld::InputAttrs::default());
    }

    // A sink of qld's own, drained afterwards: qld reports from several
    // threads at once, so the diagnostics are collected first and put into
    // the `DiagBag` in qld's deterministic order.
    let sink = qld::diag::Collect::new();
    let image = qld::link_to_memory(&request, &sink);
    for diagnostic in sink.take_sorted() {
        report(diags, diagnostic);
    }
    image.map_err(|error| Error {
        message: error.to_string(),
    })
}

/// The target to hand qld, which is `None` — infer it from the first
/// object — unless the objects are PE/COFF, where the container has to be
/// named because nothing in the object names it.
fn qld_target(target: Option<(Format, u16)>) -> Option<qld::Target> {
    let (Format::Coff, machine) = target? else {
        return None;
    };
    let arch = match machine {
        EM_AARCH64 => qld::Architecture::Aarch64,
        _ => qld::Architecture::X86_64,
    };
    Some(qld::Target {
        format: qld::BinaryFormat::Pe,
        arch,
        endian: qld::Endianness::Little,
        pointer_width: qld::PointerWidth::Bits64,
        os: qld::OperatingSystem::Windows,
    })
}

/// Turns one of qld's diagnostics into rsasm's.
///
/// A linker diagnostic points into an object file, not into assembly
/// source, so every one of them carries a dummy span and renders as the
/// message alone. Whatever qld attaches underneath a message — further
/// locations, the `>>>` context lines, its notes — becomes a note of its
/// own, since rsasm's notes are labels on a source snippet and there is no
/// snippet to label.
fn report(diags: &mut DiagBag, diagnostic: qld::Diagnostic) {
    let severity = match diagnostic.severity {
        qld::Severity::Error => Severity::Error,
        qld::Severity::Warning => Severity::Warning,
        qld::Severity::Note => Severity::Note,
    };
    let message = match diagnostic.locations.first() {
        Some(location) => format!("{location}: {}", diagnostic.message),
        None => diagnostic.message.clone(),
    };
    diags.emit(Diagnostic::new(severity, Span::DUMMY, message));
    let rest = diagnostic
        .locations
        .iter()
        .skip(1)
        .map(ToString::to_string)
        .chain(diagnostic.details.iter().cloned())
        .chain(diagnostic.notes.iter().cloned());
    for note in rest {
        diags.emit(Diagnostic::new(Severity::Note, Span::DUMMY, note));
    }
}
