//! The program's native library record (NATIVE.md 3.1, 3.2, 3.3, 3.10; piece W7): the link line
//! read from the program's crates (their build scripts' `rustc-link-lib` and `rustc-link-search`
//! lines, then their `#[link]` attributes), the record's key computed from it before any work,
//! and the record's bytes: blaze-native's plan and layout, then blaze-export's Mach-O dylib
//! writer. Every program has one, even one without archives (it names only libSystem / libc).
//!
//! The link line (NATIVE.md 3.2, decided):
//! - Crates in the order given (reverse topological, the program's crate first). Within a
//!   crate: its script's `rustc-link-lib` lines in print order, then its `#[link]` attributes in
//!   source order (W6's `CrateLinks::decls`, cfg already applied).
//! - Text form `[KIND[:MODS]=]NAME`; KIND is `static`, `dylib` or `framework`. Reference,
//!   items.extern.attributes.link.modifiers: "Modifiers are specified as a comma-delimited string
//!   with each modifier prefixed with either a + or - to indicate that the modifier is enabled or
//!   disabled"; of `bundle` and of `whole-archive`: "This modifier is only compatible with the
//!   static linking kind. Using any other kind will result in a compiler error."
//! - A search path is taken only under the script's OUT_DIR (the directory itself, or a path
//!   `OutputsRef::file_at` answers for); any other, of any kind, is `SearchOutsideOutDir`.
//!   `native=` and `all=` (and no kind) paths are searched for archives, in print order.
//! - `static=NAME`: `lib<NAME>.a` (`NAME` itself with `+verbatim`) under one of the crate's
//!   search paths, else `StaticNotFound`. The default kind and `dylib=NAME`: as the linker's `-l`
//!   search, a static input when that file exists (c-kzg's second `ckzg` line), else a library
//!   through the install-name table. `framework=NAME`: the table. The same archive named twice is
//!   one input, at its first position (whole when either asked for it, as lld pulls every member
//!   of an archive once any mention of it is `--whole-archive`).
//! - An input is an archive when its bytes start with `!<arch>\n`, else one plain object (the
//!   linker decides by content, never by name).
//! - Libraries: `c` first (libSystem on macOS, libc.so.6 on Linux), then every dylib and
//!   framework in link-line order, deduplicated by install name. A name with no install name is
//!   refused here (`Resolve(Library(..))`), before anything is read.
//! - `rustc-link-arg`: `-pthread` accepted, any other `LinkArg`. Only the program's own crate's
//!   lines for a binary (`rustc-link-arg`, `-bins`, `-bin=`) are read: the Cargo book, build
//!   scripts, says the instruction passes `-C link-arg` "only when building supported targets"
//!   of the package whose script printed it, never its dependents.
//!
//! The key (NATIVE.md 3.1, rule 21): the record's FORMAT number ([`NATIVE_FORMAT`]),
//! blaze-native's [`LAYOUT_VERSION`], the producer's id (D105: no row of its own yet, so
//! [`Whole`]), the platform, the ordered inputs (outputs-record id, package, file name, kind,
//! whole), the sorted root names, the sorted Rust export names and the ordered libraries. Edits to
//! Rust bodies change none of these; only `#[link]`, extern blocks, exported names or script
//! outputs do.
//!
//! Determinism (NATIVE.md 3.10): every list is in link order or sorted; the record's code
//! signature identifier is the key's 64 hex digits.
//!
//! Names given to resolve and layout live as long as the allocator (`alloc` is the task's frame:
//! they are given up into it, never freed one by one).

use blaze_build_script::instr::{LinkTo, SearchKind};
use blaze_build_script::outputs::OutputsRef;
use blaze_export::native_dylib::{self, DylibError};
use blaze_hash::FixedState;
use blaze_ids::producer::{ProducerId, Whole};
use blaze_ids::{ContentId, Hasher};
use blaze_native::Platform;
use blaze_native::descriptor::{self, DescriptorError, Names};
use blaze_native::layout::{self, LAYOUT_VERSION, Layout, LayoutError};
use blaze_native::libraries::{self, InstallName, Library, LibraryKind};
use blaze_native::resolve::{self, Input, InputKind, Origin, ResolveError, Wanted};
use blaze_own::{AVec, Alloc};

use crate::front2::links::{CrateLinks, LinkKind};

/// The FORMAT number of the native library record: the form of its bytes (a Mach-O `MH_DYLIB`
/// with the `__blaze_native_v1` descriptor of NATIVE.md 3.4). A change to that form says so here.
/// 2: the descriptor is BNL2 (blaze-native descriptor.rs: pointer rows and the names table).
pub(crate) const NATIVE_FORMAT: u32 = 2;

const KEY_DOMAIN: &str = "blaze native library record key";

/// `ar`'s global header: an input with it is an archive, any other a plain object.
const AR_MAGIC: &[u8] = b"!<arch>\n";

/// Reference, items.extern.attributes.link.modifiers-whole-archive and -bundle.
const STATIC_ONLY: &str = "`whole-archive` and `bundle` are only compatible with the `static` kind \
     (Reference, items.extern.attributes.link.modifiers)";

/// What the program's native library is made from.
pub(crate) struct NativeInputs<'a> {
    pub platform: Platform,
    /// The producer's id (D105), handed down from the entry.
    pub producer: &'a ProducerId<Whole>,
    /// Reverse topological order, the program's crate first.
    pub crates: &'a [CrateNative<'a>],
}

/// One crate of the program, as its native link sees it.
pub(crate) struct CrateNative<'a> {
    /// `name@version`.
    pub package: &'a str,
    /// The build script's outputs record and its id, when the crate has a script.
    pub outputs: Option<(ContentId, OutputsRef<'a>)>,
    pub links: &'a CrateLinks<'a>,
}

/// Why the program's native library was refused; each names the package, the line or the name,
/// and the rule (NATIVE.md 3.2), or carries the refusal of the stage that made it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum NativeError<'a> {
    /// A `rustc-link-lib` text (or a `#[link]`'s name) not taken; `why` names the rule.
    LinkLine { package: &'a str, line: &'a str, why: &'static str },
    SearchOutsideOutDir { package: &'a str, path: &'a str },
    StaticNotFound { package: &'a str, name: &'a str },
    LinkArg { package: &'a str, arg: &'a str },
    UnknownModifier { package: &'a str, text: &'a str },
    Resolve(ResolveError<'a>),
    Layout(LayoutError<'a>),
    Write(DylibError<'a>),
    Alloc,
    /// The record's bytes give no BNL2 descriptor (blaze-native descriptor.rs).
    Descriptor(DescriptorError),
}

/// The record's word names in word order, read from its own descriptor (main's ruling (a),
/// 2026-11-16: a record is its own index). On a store hit and after a build alike: the call site
/// pairs name i with the slot it declares for word i (`Down::Native.words`).
pub(crate) fn word_names<'r>(record: &'r [u8], platform: Platform) -> Result<Names<'r>, NativeError<'static>> {
    descriptor::read(record, platform).map(|(_, names)| names).map_err(NativeError::Descriptor)
}

/// What one record holds, for the run's report.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct NativeSummary {
    pub inputs: u32,
    pub members: u32,
    pub words: u32,
    pub inits: u32,
    pub imports: u32,
}

/// The key of the program's native library (NATIVE.md 3.1), computed before any work.
pub(crate) fn native_key<'a>(inputs: &NativeInputs<'a>, alloc: Alloc<'a>) -> Result<ContentId, NativeError<'a>> {
    let line = link_line(inputs, alloc)?;
    let roots = sorted_names(inputs.crates, |links| links.foreign.as_slice(), alloc)?;
    let rust = sorted_names(inputs.crates, |links| links.exports.as_slice(), alloc)?;
    Ok(key_of(inputs, &line, &roots, &rust))
}

/// The record's bytes: plan, lay, write; appended to `out`. The caller stores them under
/// [`native_key`] (a store write as run.rs does for code records) and never calls this when the
/// store has the key. The word names are the record's own ([`word_names`]).
pub(crate) fn native_library<'a>(
    inputs: &NativeInputs<'a>,
    seed: FixedState,
    out: &mut AVec<'_, u8>,
    alloc: Alloc<'a>,
) -> Result<NativeSummary, NativeError<'a>> {
    let platform = inputs.platform;
    let line = link_line(inputs, alloc)?;
    let root_names = sorted_names(inputs.crates, |links| links.foreign.as_slice(), alloc)?;
    let rust_names = sorted_names(inputs.crates, |links| links.exports.as_slice(), alloc)?;
    let key = key_of(inputs, &line, &root_names, &rust_names);
    let roots = mangled(platform, &root_names, alloc)?;
    let rust = mangled(platform, &rust_names, alloc)?;

    let LinkLine { inputs: listed, libraries } = line;
    let libraries: &'a [Library<'a>] = libraries.leak();
    let mut objects: AVec<'a, Input<'a>> = AVec::try_with_capacity_in(listed.len(), alloc).map_err(|_| NativeError::Alloc)?;
    for entry in listed.iter() {
        push(&mut objects, Input { from: entry.from, bytes: entry.bytes, kind: entry.kind() })?;
    }

    let plan = resolve::plan(platform, &objects, Wanted { roots, rust, libraries }, seed, alloc).map_err(NativeError::Resolve)?;
    let image = layout::lay(&plan, &objects, layout_of(platform), alloc).map_err(NativeError::Layout)?;
    let mut hex = [0u8; 64];
    let identifier = hex_of(&key, &mut hex)?;
    native_dylib::write(&image, identifier, out, alloc).map_err(NativeError::Write)?;
    Ok(NativeSummary {
        inputs: narrow("inputs", objects.len())?,
        members: narrow("members", plan.objects.len())?,
        words: narrow("words", image.words.len())?,
        inits: image.inits,
        imports: narrow("imports", image.imports.len())?,
    })
}

/// One library request: a `rustc-link-lib` line or a `#[link]`, as read.
#[derive(Clone, Copy)]
struct Request<'a> {
    kind: LinkKind,
    /// `+whole-archive`.
    whole: bool,
    /// A `bundle` modifier was written (either sign): static only.
    bundle: bool,
    verbatim: bool,
    name: &'a str,
}

/// One input on the link line.
#[derive(Clone, Copy)]
struct LineInput<'a> {
    /// The id of the outputs record the file is in.
    outputs: ContentId,
    from: Origin<'a>,
    bytes: &'a [u8],
    whole: bool,
}

impl LineInput<'_> {
    fn kind(&self) -> InputKind {
        if self.bytes.starts_with(AR_MAGIC) { InputKind::Archive { whole: self.whole } } else { InputKind::Object }
    }
}

struct LinkLine<'a> {
    inputs: AVec<'a, LineInput<'a>>,
    libraries: AVec<'a, Library<'a>>,
}

fn push<'e, T>(v: &mut AVec<'_, T>, value: T) -> Result<(), NativeError<'e>> {
    v.try_push(value).map_err(|_| NativeError::Alloc)
}

fn narrow<'a>(what: &'static str, len: usize) -> Result<u32, NativeError<'a>> {
    u32::try_from(len).map_err(|_| NativeError::Write(DylibError::TooLarge { what, len: u64::try_from(len).unwrap_or(u64::MAX) }))
}

fn layout_of(platform: Platform) -> Layout {
    match platform {
        Platform::MacosArm64 => Layout::MACOS_ARM64,
        Platform::LinuxArm64 => Layout::LINUX_ARM64,
    }
}

fn platform_code(platform: Platform) -> u8 {
    match platform {
        Platform::MacosArm64 => 0,
        Platform::LinuxArm64 => 1,
    }
}

/// The link line of the program (NATIVE.md 3.2).
fn link_line<'a>(inputs: &NativeInputs<'a>, alloc: Alloc<'a>) -> Result<LinkLine<'a>, NativeError<'a>> {
    let platform = inputs.platform;
    let mut line = LinkLine { inputs: AVec::new_in(alloc), libraries: AVec::new_in(alloc) };
    push(&mut line.libraries, Library { kind: LibraryKind::Dylib, name: "c" })?;
    for (at, krate) in inputs.crates.iter().enumerate() {
        let package = krate.package;
        let mut dirs: AVec<'a, &'a str> = AVec::new_in(alloc);
        let script = krate.outputs.as_ref().map(|(id, outputs)| (*id, outputs));
        if let Some((id, outputs)) = script {
            search_dirs(package, outputs, &mut dirs)?;
            if at == 0 {
                link_args(package, outputs)?;
            }
            for text in outputs.link_libs() {
                let request = parse_link_lib(package, text)?;
                take(&mut line, platform, package, Some((id, outputs)), &dirs, text, request)?;
            }
        }
        for decl in krate.links.decls.iter() {
            let request = Request {
                kind: decl.kind,
                whole: decl.modifiers.whole_archive,
                bundle: false,
                verbatim: decl.modifiers.verbatim,
                name: decl.name,
            };
            take(&mut line, platform, package, script, &dirs, decl.name, request)?;
        }
    }
    Ok(line)
}

/// `[KIND[:MODS]=]NAME`, as `rustc -l` reads it.
fn parse_link_lib<'a>(package: &'a str, line: &'a str) -> Result<Request<'a>, NativeError<'a>> {
    let bad = |why: &'static str| NativeError::LinkLine { package, line, why };
    let (head, name) = match line.split_once('=') {
        Some((head, name)) => (Some(head), name),
        None => (None, line),
    };
    let mut request = Request { kind: LinkKind::Dylib, whole: false, bundle: false, verbatim: false, name };
    if let Some(head) = head {
        let (kind, mods) = match head.split_once(':') {
            Some((kind, mods)) => (kind, Some(mods)),
            None => (head, None),
        };
        request.kind = match kind {
            "static" => LinkKind::Static,
            "dylib" => LinkKind::Dylib,
            "framework" => LinkKind::Framework,
            "raw-dylib" => return Err(bad("`raw-dylib` is only valid for Windows targets (Reference, items.extern.attributes.link.raw-dylib)")),
            _ => return Err(bad("the kind is not `static`, `dylib` or `framework` (Reference, items.extern.attributes.link)")),
        };
        for text in mods.into_iter().flat_map(|mods| mods.split(',')) {
            let (on, modifier) = match (text.strip_prefix('+'), text.strip_prefix('-')) {
                (Some(modifier), _) => (true, modifier),
                (_, Some(modifier)) => (false, modifier),
                _ => return Err(NativeError::UnknownModifier { package, text }),
            };
            match modifier {
                "whole-archive" => request.whole = on,
                "verbatim" => request.verbatim = on,
                "bundle" => request.bundle = true,
                "as-needed" => {}
                _ => return Err(NativeError::UnknownModifier { package, text }),
            }
        }
    }
    if name.is_empty() {
        return Err(bad("the library name is empty"));
    }
    if name.contains(':') {
        return Err(bad("the rename form `NAME:RENAME` is not taken (NATIVE.md 3.2)"));
    }
    Ok(request)
}

/// The crate's archive search directories, relative to its OUT_DIR, in print order; every
/// search path of any kind must be under OUT_DIR.
fn search_dirs<'a>(package: &'a str, outputs: &OutputsRef<'a>, dirs: &mut AVec<'a, &'a str>) -> Result<(), NativeError<'a>> {
    let out_dir = outputs.out_dir();
    for (kind, path) in outputs.link_search() {
        let outside = NativeError::SearchOutsideOutDir { package, path };
        if path.split('/').any(|part| part == "..") {
            return Err(outside);
        }
        let relative = if path == out_dir {
            ""
        } else {
            // `file_at` answers exactly for `OUT_DIR/..`: the rest after the separator.
            if outputs.file_at(path).is_none() {
                return Err(outside);
            }
            path.get(out_dir.len() + 1..).ok_or(outside)?
        };
        match kind {
            SearchKind::Native | SearchKind::All | SearchKind::Unspecified => push(dirs, relative.trim_end_matches('/'))?,
            SearchKind::Dependency | SearchKind::Crate | SearchKind::Framework => {}
        }
    }
    Ok(())
}

/// The program's own crate's link arguments for a binary: `-pthread` only (NATIVE.md 3.2).
fn link_args<'a>(package: &'a str, outputs: &OutputsRef<'a>) -> Result<(), NativeError<'a>> {
    let binary = [LinkTo::All.code(), LinkTo::Bins.code(), LinkTo::Bin("").code()];
    for (to, _bin, arg) in outputs.link_args() {
        if binary.contains(&to) && arg != "-pthread" {
            return Err(NativeError::LinkArg { package, arg });
        }
    }
    Ok(())
}

/// Whether the record's file `file` is the archive `name` asks for in directory `dir`.
fn names_file(file: &str, dir: &str, name: &str, verbatim: bool) -> bool {
    let rest = if dir.is_empty() {
        Some(file)
    } else {
        file.strip_prefix(dir).and_then(|rest| rest.strip_prefix('/'))
    };
    match rest {
        Some(rest) if verbatim => rest == name,
        Some(rest) => rest.strip_prefix("lib").and_then(|rest| rest.strip_suffix(".a")) == Some(name),
        None => false,
    }
}

/// The first file under the crate's search directories that `request` names.
fn find_static<'a>(outputs: &OutputsRef<'a>, dirs: &[&'a str], request: &Request<'_>) -> Option<(&'a str, &'a [u8])> {
    dirs.iter().find_map(|dir| outputs.files().find(|(file, _)| names_file(file, dir, request.name, request.verbatim)))
}

/// Puts one request on the link line.
fn take<'a>(
    line: &mut LinkLine<'a>,
    platform: Platform,
    package: &'a str,
    script: Option<(ContentId, &OutputsRef<'a>)>,
    dirs: &[&'a str],
    written: &'a str,
    request: Request<'a>,
) -> Result<(), NativeError<'a>> {
    if (request.whole || request.bundle) && request.kind != LinkKind::Static {
        return Err(NativeError::LinkLine { package, line: written, why: STATIC_ONLY });
    }
    let found = script.and_then(|(id, outputs)| find_static(outputs, dirs, &request).map(|file| (id, file)));
    match (request.kind, found) {
        (LinkKind::Framework, _) => add_library(line, platform, Library { kind: LibraryKind::Framework, name: request.name }),
        (LinkKind::Static, None) => Err(NativeError::StaticNotFound { package, name: request.name }),
        (LinkKind::Static | LinkKind::Dylib, Some((id, (file, bytes)))) => {
            add_input(line, LineInput { outputs: id, from: Origin { package, file }, bytes, whole: request.whole })
        }
        (LinkKind::Dylib, None) => add_library(line, platform, Library { kind: LibraryKind::Dylib, name: request.name }),
    }
}

/// An input, once: a second mention keeps the first position.
fn add_input<'a>(line: &mut LinkLine<'a>, input: LineInput<'a>) -> Result<(), NativeError<'a>> {
    if let Some(first) = line.inputs.iter_mut().find(|first| first.from == input.from) {
        first.whole |= input.whole;
        return Ok(());
    }
    push(&mut line.inputs, input)
}

/// A library, once per install name; refused when it has none.
fn add_library<'a>(line: &mut LinkLine<'a>, platform: Platform, library: Library<'a>) -> Result<(), NativeError<'a>> {
    let named = |library: Library<'a>| libraries::install_name(platform, library).map_err(|e| NativeError::Resolve(ResolveError::Library(e)));
    let install = named(library)?;
    for &listed in line.libraries.iter() {
        let same = match (named(listed)?, install) {
            (InstallName::Fixed(a), InstallName::Fixed(b)) => a == b,
            (InstallName::Framework, InstallName::Framework) => listed.name == library.name,
            _ => false,
        };
        if same {
            return Ok(());
        }
    }
    push(&mut line.libraries, library)
}

/// Every crate's names `pick` gives, sorted and unique.
fn sorted_names<'a>(
    crates: &'a [CrateNative<'a>],
    pick: impl Fn(&'a CrateLinks<'a>) -> &'a [&'a str],
    alloc: Alloc<'a>,
) -> Result<AVec<'a, &'a str>, NativeError<'a>> {
    let mut names = AVec::new_in(alloc);
    for krate in crates {
        for &name in pick(krate.links) {
            push(&mut names, name)?;
        }
    }
    names.sort_unstable();
    names.dedup();
    Ok(names)
}

/// `names` (sorted, unique) as the platform's symbol tables write them, given up into `alloc`
/// for as long as it lives (the plan and the image borrow them). A uniform prefix keeps the order.
fn mangled<'a>(platform: Platform, names: &[&'a str], alloc: Alloc<'a>) -> Result<&'a [&'a [u8]], NativeError<'a>> {
    let mut text: AVec<'a, u8> = AVec::new_in(alloc);
    let mut ends: AVec<'a, usize> = AVec::try_with_capacity_in(names.len(), alloc).map_err(|_| NativeError::Alloc)?;
    for &name in names {
        let one = resolve::mangle(platform, name, alloc).map_err(NativeError::Resolve)?;
        text.try_extend_from_slice(&one).map_err(|_| NativeError::Alloc)?;
        push(&mut ends, text.len())?;
    }
    let text: &'a [u8] = text.leak();
    let mut out: AVec<'a, &'a [u8]> = AVec::try_with_capacity_in(ends.len(), alloc).map_err(|_| NativeError::Alloc)?;
    let mut start = 0;
    for &end in ends.iter() {
        let name = text.get(start..end).ok_or(NativeError::Alloc)?;
        push(&mut out, name)?;
        start = end;
    }
    Ok(out.leak())
}

fn count(h: &mut Hasher, len: usize) {
    h.update(&u64::try_from(len).unwrap_or(u64::MAX).to_le_bytes());
}

/// The key over the link line and the names (NATIVE.md 3.1).
fn key_of(inputs: &NativeInputs<'_>, line: &LinkLine<'_>, roots: &[&str], rust: &[&str]) -> ContentId {
    let mut h = Hasher::new(KEY_DOMAIN);
    h.update(&NATIVE_FORMAT.to_le_bytes())
        .update(&LAYOUT_VERSION.to_le_bytes())
        .id(inputs.producer.id())
        .update(&[platform_code(inputs.platform)]);
    count(&mut h, line.inputs.len());
    for input in line.inputs.iter() {
        let kind = match input.kind() {
            InputKind::Archive { whole: false } => 0u8,
            InputKind::Archive { whole: true } => 1,
            InputKind::Object => 2,
        };
        h.id(&input.outputs).part(input.from.package.as_bytes()).part(input.from.file.as_bytes()).update(&[kind]);
    }
    for names in [roots, rust] {
        count(&mut h, names.len());
        for name in names {
            h.part(name.as_bytes());
        }
    }
    count(&mut h, line.libraries.len());
    for library in line.libraries.iter() {
        let kind = match library.kind {
            LibraryKind::Dylib => 0u8,
            LibraryKind::Framework => 1,
        };
        h.update(&[kind]).part(library.name.as_bytes());
    }
    h.finish()
}

/// The key's 64 lowercase hex digits: the record's identifier (NATIVE.md 3.10).
fn hex_of<'a, 'h>(key: &ContentId, buf: &'h mut [u8; 64]) -> Result<&'h str, NativeError<'a>> {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    for (at, byte) in key.0.iter().enumerate() {
        buf[2 * at] = DIGITS[usize::from(byte >> 4)];
        buf[2 * at + 1] = DIGITS[usize::from(byte & 0xF)];
    }
    core::str::from_utf8(buf).map_err(|_| NativeError::Write(DylibError::Identifier { len: 64 }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use blaze_build_script::instr::OutputLimits;
    use blaze_build_script::outputs::{FileLimits, Outputs};
    use blaze_ids::producer::Producers;

    use crate::front2::links::text_links;

    const OUT: &str = "/out";
    const MAC: &[(&str, Option<&str>)] = &[("target_os", Some("macos")), ("unix", None)];
    /// An empty archive: the global header alone.
    const ARCHIVE: &[u8] = b"!<arch>\n";

    /// A build script's outputs record: `printed` as its standard output, `files` left in OUT_DIR.
    fn record<'a>(printed: &str, files: &[(&str, &[u8])], alloc: Alloc<'a>) -> AVec<'a, u8> {
        let mut outputs = Outputs::read(printed.as_bytes(), OUT, OutputLimits::DEFAULT, alloc).unwrap_or_else(|e| panic!("{e:?}"));
        for &(name, bytes) in files {
            outputs.add_file(name, bytes, FileLimits::DEFAULT, alloc).unwrap_or_else(|e| panic!("{e:?}"));
        }
        let mut bytes = AVec::new_in(alloc);
        outputs.encode(&mut bytes);
        bytes
    }

    fn links<'a>(text: &'a str, alloc: Alloc<'a>) -> CrateLinks<'a> {
        text_links("t", text, MAC, alloc).unwrap_or_else(|e| panic!("links refused: {e:?}"))
    }

    fn key_of_text<'a>(text: &'a str, producer: &'a ProducerId<Whole>, alloc: Alloc<'a>) -> ContentId {
        let links = links(text, alloc);
        let crates = [CrateNative { package: "t@0.1.0", outputs: None, links: &links }];
        native_key(&NativeInputs { platform: Platform::MacosArm64, producer, crates: &crates }, alloc)
            .unwrap_or_else(|e| panic!("key refused: {e:?}"))
    }

    /// NATIVE.md 3.1: "Edits to Rust bodies do not change it; only `#[link]`, extern blocks,
    /// exported names or script outputs do." Two texts differing only in a body: equal keys; the
    /// control renames the export (Reference, abi, no_mangle: "The symbol for the item is the
    /// item's identifier") and the key moves.
    ///
    /// Prints `test natives::tests::key_ignores_rust_bodies ... ok`.
    /// Without the piece: "file not found for module `natives`".
    #[test]
    fn word_names_refuses_a_non_dylib() {
        // Not a Mach-O header at all.
        let junk = [0u8; 64];
        assert_eq!(
            super::word_names(&junk, blaze_native::Platform::MacosArm64).map(|n| n.len()),
            Err(super::NativeError::Descriptor(blaze_native::descriptor::DescriptorError::NotDylib))
        );
        // A BNL1 descriptor is refused by its tag: blaze-native's own reader test covers the walk;
        // here the error passes through by name.
        let old = super::NativeError::Descriptor(blaze_native::descriptor::DescriptorError::Old { tag: 0x424e_4c31 });
        assert!(matches!(old, super::NativeError::Descriptor(blaze_native::descriptor::DescriptorError::Old { .. })));
        println!("natives: word_names refuses bytes that are no dylib (NotDylib) and a BNL1 tag (Old)");
    }

    #[test]
    fn key_ignores_rust_bodies() {
        let alloc = blaze_own::testing::heap().alloc();
        let producers = Producers::for_tests(b"natives");
        let producer = producers.get::<Whole>();
        let before = "#[link(name = \"z\")]\nextern \"C\" { fn crc32(); }\n\
                      #[unsafe(no_mangle)] pub extern \"C\" fn cb() -> i32 { 1 }\n";
        let body = "#[link(name = \"z\")]\nextern \"C\" { fn crc32(); }\n\
                    #[unsafe(no_mangle)] pub extern \"C\" fn cb() -> i32 { let x = 2; x }\n";
        let renamed = "#[link(name = \"z\")]\nextern \"C\" { fn crc32(); }\n\
                       #[unsafe(no_mangle)] pub extern \"C\" fn cb2() -> i32 { 1 }\n";
        assert_eq!(key_of_text(before, &producer, alloc), key_of_text(body, &producer, alloc));
        assert_ne!(key_of_text(before, &producer, alloc), key_of_text(renamed, &producer, alloc));
    }

    /// c-kzg 2.1.8: cc's `try_compile("ckzg")` prints `cargo:rustc-link-lib=static=ckzg` and
    /// `cargo:rustc-link-search=native=<OUT_DIR>`; then bindings/rust/build.rs:52 prints
    /// `cargo:rustc-link-lib=ckzg`. The second, of the default kind, finds `libckzg.a` as the
    /// linker's `-l` search would (NATIVE.md 3.2): the same archive, one input, first position.
    ///
    /// Prints `test natives::tests::c_kzg_double_line_is_one_input ... ok`.
    #[test]
    fn c_kzg_double_line_is_one_input() {
        let alloc = blaze_own::testing::heap().alloc();
        let producers = Producers::for_tests(b"natives");
        let producer = producers.get::<Whole>();
        let printed = "cargo:rustc-link-lib=static=ckzg\ncargo:rustc-link-search=native=/out\ncargo:rustc-link-lib=ckzg\n";
        let bytes = record(printed, &[("libckzg.a", ARCHIVE)], alloc);
        let outputs = OutputsRef::open(&bytes).unwrap_or_else(|e| panic!("{e:?}"));
        let links = links("", alloc);
        let crates = [CrateNative { package: "c-kzg@2.1.8", outputs: Some((ContentId([7; 32]), outputs)), links: &links }];
        let inputs = NativeInputs { platform: Platform::MacosArm64, producer: &producer, crates: &crates };
        let line = link_line(&inputs, alloc).unwrap_or_else(|e| panic!("refused: {e:?}"));
        assert_eq!(line.inputs.len(), 1);
        assert_eq!(line.inputs[0].from, Origin { package: "c-kzg@2.1.8", file: "libckzg.a" });
        assert_eq!(line.inputs[0].kind(), InputKind::Archive { whole: false });
        assert_eq!(line.libraries.as_slice(), [Library { kind: LibraryKind::Dylib, name: "c" }]);
    }

    /// NATIVE.md 3.2: "A search path is accepted only when `OutputsRef::file_at` answers for it
    /// (under the script's OUT_DIR); any other path is refused `SearchOutsideOutDir`". `/outside`
    /// starts with `/out` but is not under it; `..` leaves it; `/out/lib` is under it and its
    /// archive is found.
    ///
    /// Prints `test natives::tests::search_outside_out_dir_refused ... ok`.
    #[test]
    fn search_outside_out_dir_refused() {
        let alloc = blaze_own::testing::heap().alloc();
        let producers = Producers::for_tests(b"natives");
        let producer = producers.get::<Whole>();
        let links = links("", alloc);
        let cases = [
            ("/usr/lib", "cargo:rustc-link-search=native=/usr/lib\ncargo:rustc-link-lib=static=z\n", true),
            ("/outside", "cargo:rustc-link-search=native=/outside\ncargo:rustc-link-lib=static=z\n", true),
            ("/out/../etc", "cargo:rustc-link-search=native=/out/../etc\ncargo:rustc-link-lib=static=z\n", true),
            ("/out/lib", "cargo:rustc-link-search=native=/out/lib\ncargo:rustc-link-lib=static=z\n", false),
        ];
        for (path, printed, refused) in cases {
            let bytes = record(printed, &[("lib/libz.a", ARCHIVE)], alloc);
            let outputs = OutputsRef::open(&bytes).unwrap_or_else(|e| panic!("{e:?}"));
            let crates = [CrateNative { package: "libz-sys@1.1.22", outputs: Some((ContentId([1; 32]), outputs)), links: &links }];
            let got = native_key(&NativeInputs { platform: Platform::MacosArm64, producer: &producer, crates: &crates }, alloc);
            if refused {
                assert_eq!(got, Err(NativeError::SearchOutsideOutDir { package: "libz-sys@1.1.22", path }), "{path}");
            } else {
                assert!(got.is_ok(), "{path}: {got:?}");
            }
        }
    }

    /// NATIVE.md 3.2: "`rustc-link-arg`: `-pthread` accepted (no library on macOS or glibc >=
    /// 2.34); any other refused `LinkArg{package, arg}`." A dependency's link arguments are not
    /// its dependents' (Cargo book, build scripts: `rustc-link-arg` applies to the package's own
    /// supported targets): the same line in the second crate is not read.
    ///
    /// Prints `test natives::tests::pthread_arg_accepted_others_refused ... ok`.
    #[test]
    fn pthread_arg_accepted_others_refused() {
        let alloc = blaze_own::testing::heap().alloc();
        let producers = Producers::for_tests(b"natives");
        let producer = producers.get::<Whole>();
        let links = links("", alloc);
        let pthread = record("cargo:rustc-link-arg=-pthread\n", &[], alloc);
        let rpath = record("cargo:rustc-link-arg=-Wl,-rpath,/x\n", &[], alloc);
        two_crates(&pthread, &pthread, &links, &producer, alloc, None);
        two_crates(&rpath, &pthread, &links, &producer, alloc, Some(NativeError::LinkArg { package: "app@0.1.0", arg: "-Wl,-rpath,/x" }));
        two_crates(&pthread, &rpath, &links, &producer, alloc, None);
    }

    /// Asserts the key's refusal (`None`: a key) for a program crate whose script record is
    /// `first` over one dependency whose record is `second`.
    fn two_crates<'a>(
        first: &'a [u8],
        second: &'a [u8],
        links: &'a CrateLinks<'a>,
        producer: &'a ProducerId<Whole>,
        alloc: Alloc<'a>,
        expected: Option<NativeError<'static>>,
    ) {
        let a = OutputsRef::open(first).unwrap_or_else(|e| panic!("{e:?}"));
        let b = OutputsRef::open(second).unwrap_or_else(|e| panic!("{e:?}"));
        let crates = [
            CrateNative { package: "app@0.1.0", outputs: Some((ContentId([2; 32]), a)), links },
            CrateNative { package: "dep@0.1.0", outputs: Some((ContentId([3; 32]), b)), links },
        ];
        let got = native_key(&NativeInputs { platform: Platform::MacosArm64, producer, crates: &crates }, alloc).err();
        assert_eq!(got, expected);
    }
}
