//! Link arguments: what `-C link-arg`, `-C link-args` and a build script's
//! `cargo:rustc-link-arg*` give the link, as typed options blaze's linker implements.
//!
//! rustc hands each such argument to the target's linker driver: the C driver (`cc`) on Linux
//! and macOS, which hands `-Wl,a,b` and `-Xlinker a` on to the system linker (GNU ld or lld,
//! ld64), and `link.exe` or `wasm-ld` itself on Windows and wasm. blaze runs none of them.
//! [`parse`] takes the driver's layer off, reads each word in the spelling of the linker the
//! target's rustc would run ([`Spelling`]), and gives a [`LinkOption`] for each; [`apply`]
//! sets them on the [`Output`] and hands back the libraries and search directories they add.
//!
//! An argument blaze's linker does not implement is an error that names it, the word of it
//! refused, where it came from and the target ([`ArgError`]); every refused argument of a
//! link is named in one error. None is dropped: an option that changes nothing blaze writes
//! is one of two kinds, each with its reason in [`LinkOption`]: [`LinkOption::Already`]
//! (blaze's linker always does it: `--gc-sections`, `-z now`) and [`LinkOption::NoEffect`]
//! (it asks nothing of the image: `-fuse-ld=mold`, which only picks the system linker blaze
//! does not run). docs/cargo.md, "Link arguments", lists every spelling.
//!
//! The rest of the rustflags that are the link's (`-l`, `-L`, `-C strip`, `-C rpath`,
//! `-C linker`, ...) are read by [`rustflags`], and [`apply`] takes or refuses their `-C`
//! options alike.

pub mod rustflags;
#[cfg(test)]
mod tests;

use std::path::PathBuf;

use blaze_codegen::{NativeLib, NativeLibKind};

use crate::{Format, LinkError, Output, OutputKind, Undefined};

/// Where a link argument was given.
#[derive(Clone, PartialEq, Eq, Hash, Debug)] // clone: an error names it
pub enum ArgFrom {
    /// `-C link-arg` or `-C link-args` in the unit's rustflags (`RUSTFLAGS`,
    /// `CARGO_ENCODED_RUSTFLAGS`, `build.rustflags`, `target.<triple>.rustflags`, a profile's
    /// `rustflags`).
    Rustflags,
    /// A `cargo:rustc-link-arg*` (or `rustc-cdylib-link-arg`) of the build script of
    /// `package` (its package id).
    BuildScript { package: String },
    /// A `-C` option in the unit's rustflags that rustc turns into link arguments of its own
    /// (`-C strip`, `-C relro-level`, ...: [`rustflags::CodegenOption`]).
    RustflagOption,
}

impl std::fmt::Display for ArgFrom {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ArgFrom::Rustflags => f.write_str(
                "from `-C link-arg`/`-C link-args` in the rustflags (`RUSTFLAGS`, \
                 `CARGO_ENCODED_RUSTFLAGS`, `build.rustflags`, `target.<triple>.rustflags` or a \
                 profile's `rustflags`)",
            ),
            ArgFrom::BuildScript { package } => {
                write!(f, "from `cargo:rustc-link-arg*` of the build script of `{package}`")
            }
            ArgFrom::RustflagOption => f.write_str(
                "in the rustflags (`RUSTFLAGS`, `CARGO_ENCODED_RUSTFLAGS`, `build.rustflags`, \
                 `target.<triple>.rustflags` or a profile's `rustflags`)",
            ),
        }
    }
}

/// One link argument as rustc would hand it to the linker driver, and where it was given.
#[derive(Clone, PartialEq, Eq, Hash, Debug)] // clone: an error names it
pub struct GivenArg {
    pub arg: String,
    pub from: ArgFrom,
}

/// Whose spelling a target's link arguments are read in: the linker the target's rustc runs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Spelling {
    /// Apple's ld64 behind `cc` (Mach-O).
    Ld64,
    /// GNU ld or lld behind `cc` (ELF).
    Gnu,
    /// `link.exe`, run directly (PE).
    Msvc,
    /// `wasm-ld`, run directly (wasm).
    WasmLd,
}

impl Spelling {
    pub fn of(format: Format) -> Spelling {
        match format {
            Format::MachO => Spelling::Ld64,
            Format::Elf => Spelling::Gnu,
            Format::Pe => Spelling::Msvc,
            Format::Wasm => Spelling::WasmLd,
        }
    }

    /// Whether rustc hands the arguments to a C driver, whose layer (`-Wl,`, `-Xlinker`) comes
    /// off first.
    fn has_driver(self) -> bool {
        matches!(self, Spelling::Ld64 | Spelling::Gnu)
    }
}

/// What a link argument asks, typed: every one [`parse`] takes.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: tests compare tables of them
pub enum LinkOption {
    /// `-lNAME`, `-l:FILE` (GNU, verbatim), `-framework NAME`, `-needed_framework NAME`,
    /// `-needed-lNAME` (ld64 loads every library asked for, as blaze's linker does), `-pthread`
    /// (the driver's `-lpthread`).
    Lib(NativeLib),
    /// `-L DIR`, and ld64's `-F DIR`: blaze searches a link's directories for frameworks too.
    Search(PathBuf),
    /// `-rpath DIR`: `LC_RPATH` or `DT_RUNPATH`.
    Rpath(String),
    /// ld64 `-headerpad SIZE` (hexadecimal).
    HeaderPad(u64),
    /// ld64 `-headerpad_max_install_names`.
    HeaderPadMaxInstallNames,
    /// ld64 `-undefined error|dynamic_lookup`, ELF `-z defs`.
    Undefined(Undefined),
    /// ld64 `-stack_size SIZE` of an executable: `LC_MAIN`'s stack size.
    StackSize(u64),
    /// ld64 `-exported_symbol NAME` of a shared library: one more export, by the core's name
    /// (the Mach-O name without its leading `_`).
    Export(String),
    /// ld64 `-install_name NAME`, ELF `-soname NAME`, of a shared library: its
    /// `LC_ID_DYLIB` or `DT_SONAME`.
    Name(String),
    /// ELF `-z nodelete` of a shared library.
    NoDelete,
    /// ELF `--export-dynamic-symbol PATTERN` of an executable: each name the image defines
    /// that the pattern matches ([`glob`]) is in its dynamic symbol table, for `dlsym` and
    /// the libraries it loads.
    ExportDynamic(String),
    /// What blaze's linker does to every image anyway, and why that is the option.
    Already(&'static str),
    /// What asks nothing of the image blaze writes, and why.
    NoEffect(&'static str),
}

/// A link argument refused: the argument as given, the word of it refused (a `-Wl,` part),
/// where it came from, the target and why.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: tests compare them
pub struct ArgError {
    pub arg: String,
    pub word: String,
    pub from: ArgFrom,
    pub triple: String,
    pub why: String,
}

impl std::fmt::Display for ArgError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.from {
            ArgFrom::RustflagOption => write!(f, "`{}`", self.arg)?,
            ArgFrom::Rustflags | ArgFrom::BuildScript { .. } => {
                write!(f, "the link argument `{}`", self.arg)?;
            }
        }
        if self.word != self.arg {
            write!(f, " (its `{}`)", self.word)?;
        }
        write!(f, " {}, linking for {}: {}", self.from, self.triple, self.why)
    }
}

/// Why each refused option is: the text the error carries after the option.
mod why {
    pub(super) const UNKNOWN: &str = "blaze's linker does not implement it";
    pub(super) const NO_VALUE: &str = "it takes a value and none follows";
    pub(super) const EMPTY: &str = "an empty argument";
    pub(super) const INPUT_FILE: &str = "an input file given as a link argument, which blaze's linker \
                                         does not take; give it as `cargo:rustc-link-lib` with \
                                         `cargo:rustc-link-search`";
    pub(super) const WEAK: &str = "blaze's linker has no weak or re-exported library on its link line \
                                   yet (`LC_LOAD_WEAK_DYLIB`, `LC_REEXPORT_DYLIB`)";
    pub(super) const EXPORT_DYNAMIC: &str = "blaze's linker exports from an executable only the \
                                             symbols a shared library it links also defines or \
                                             refers to and those `--export-dynamic-symbol` \
                                             names (ELF), and a shared library exports what its \
                                             crate exports";
    pub(super) const SCRIPT: &str = "blaze's linker reads only the linker scripts system libraries \
                                     ship (`GROUP`, `INPUT`, `AS_NEEDED`), not a layout script";
    pub(super) const STATIC: &str = "blaze links a dynamic, position-independent image with the C \
                                     runtime's start files; a static or start-file-less link is not \
                                     implemented";
    pub(super) const SHARED_ONLY: &str = "it applies to a shared library, and this image is an \
                                          executable";
    pub(super) const EXECUTABLE_ONLY: &str = "it applies to an executable, and this image is a \
                                              shared library";
    pub(super) const AFTER_NO_AS_NEEDED: &str = "a library after `--no-as-needed`, which blaze's \
                                                 linker would load only if used";
    pub(super) const NOT_ALWAYS: &str = "blaze's linker always links with `-z now`, `-z relro` and \
                                         `-z noexecstack`, and has no way to undo them";
    pub(super) const NO_DRIVER: &str = "rustc hands it to the linker itself, with no C driver, and \
                                        blaze's linker takes no link argument for this target yet";
    pub(super) const UNDEFINED: &str = "`-undefined warning` and `-undefined suppress` need \
                                        `-flat_namespace`, which blaze's linker does not implement";
    pub(super) const SIZE: &str = "not a hexadecimal size, as ld64 reads it";
    pub(super) const STACK_PAGES: &str = "ld64 takes a stack size in whole 4 KiB pages";
    pub(super) const HEADERPAD_MAX: &str = "more than the 1 MiB of header room blaze's linker keeps";
    pub(super) const EXPORT_NAME: &str = "a Mach-O name without its leading `_`, or a pattern, which \
                                          blaze's linker does not export by";
}

/// Why each option taken without a field is.
mod reason {
    pub(super) const GC: &str = "blaze's linker drops every section nothing reachable refers to";
    pub(super) const BIND_NOW: &str = "blaze's linker binds everything at load (`DF_BIND_NOW`)";
    pub(super) const RELRO: &str = "blaze's linker writes `PT_GNU_RELRO`";
    pub(super) const NOEXECSTACK: &str = "blaze's linker writes a non-executable `PT_GNU_STACK`";
    pub(super) const AS_NEEDED: &str = "blaze's linker loads an ELF shared library only if a symbol of \
                                        it is used";
    pub(super) const NO_AS_NEEDED: &str = "no library follows it among the link arguments";
    pub(super) const EH_FRAME_HDR: &str = "blaze's linker writes `.eh_frame_hdr`";
    pub(super) const SYMBOLIC: &str = "blaze's linker binds an image's references to its own \
                                       definitions";
    pub(super) const GNU_HASH: &str = "blaze's linker writes `DT_GNU_HASH`";
    pub(super) const PIE: &str = "blaze's linker makes every executable position-independent";
    pub(super) const DEFS: &str = "an executable's undefined symbol is an error in blaze's linker";
    pub(super) const SEARCH_FIRST: &str = "blaze searches each directory for every kind of library \
                                           before the next, as ld64 does by default";
    pub(super) const ADHOC: &str = "blaze's linker signs every Mach-O image ad hoc";
    pub(super) const UNDEFINED_ERROR: &str = "an executable's undefined symbol is an error in blaze's \
                                              linker";
    pub(super) const LINKER: &str = "it picks the system linker, and blaze links with its own";
    pub(super) const LD_O: &str = "GNU ld's `-O` only tunes its own hash tables; blaze's linker \
                                   writes its own";
    pub(super) const WARNINGS: &str = "it only silences ld64's warnings";
    pub(super) const NOT_UNLOADED: &str = "an executable is never unloaded";
    pub(super) const SHARED_EXPORTS: &str = "it adds to an executable's dynamic symbol table; a \
                                             shared library's holds what its crate exports, as \
                                             the version script rustc links one with decides";
}

/// One word of the linker's command line: its text, the argument it came from, and whether
/// the C driver reads it (not a `-Wl,` part nor after `-Xlinker`).
#[derive(Clone, Copy, Debug)]
struct Word<'a> {
    text: &'a str,
    at: usize,
    driver: bool,
}

/// The words of `args`, the driver's layer taken off: `-Wl,a,b` is the linker's `a` and `b`,
/// `-Xlinker a` the linker's `a`; any other is the driver's. Without a driver every word is
/// the linker's.
fn words(spelling: Spelling, args: &[GivenArg]) -> Vec<Word<'_>> {
    // alloc: once per link with link arguments, a word or a few per argument
    let mut out = Vec::with_capacity(args.len());
    let mut xlinker = false;
    for (at, given) in args.iter().enumerate() {
        let arg = given.arg.as_str();
        if !spelling.has_driver() {
            out.push(Word { text: arg, at, driver: false });
            continue;
        }
        if xlinker {
            xlinker = false;
            out.push(Word { text: arg, at, driver: false });
            continue;
        }
        if arg == "-Xlinker" {
            xlinker = true;
            continue;
        }
        match arg.strip_prefix("-Wl,") {
            Some(rest) => out.extend(rest.split(',').map(|text| Word { text, at, driver: false })),
            None => out.push(Word { text: arg, at, driver: true }),
        }
    }
    if xlinker && let Some(at) = args.len().checked_sub(1) {
        // `-Xlinker` last: the value missing is the error of the word itself.
        out.push(Word { text: "-Xlinker", at, driver: true });
    }
    out
}

/// What [`parse`] makes of one word: an option, and how many words after it it took.
type Read = Result<(LinkOption, usize), &'static str>;

/// Whether the image is an executable or a shared library: some options apply to one only.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Executable,
    Shared,
}

impl Kind {
    pub fn of(output: &Output) -> Kind {
        match output.kind {
            OutputKind::Executable { .. } => Kind::Executable,
            OutputKind::Shared { .. } => Kind::Shared,
        }
    }
}

/// Every option of `args` for an image of `kind` linked for `triple` in `spelling`'s
/// spelling, in order; every argument refused, each once, in order, when any is.
pub fn parse(
    spelling: Spelling,
    kind: Kind,
    triple: &str,
    args: &[GivenArg],
) -> Result<Vec<LinkOption>, Vec<ArgError>> {
    let words = words(spelling, args);
    // alloc: once per link with link arguments
    let mut options = Vec::with_capacity(words.len());
    // alloc: none unless an argument is refused
    let mut errors: Vec<ArgError> = Vec::new();
    let mut refuse = |word: &Word<'_>, why: &str| {
        let given = &args[word.at];
        if errors.last().is_some_and(|it| it.arg == given.arg && it.word == word.text) {
            return;
        }
        errors.push(ArgError {
            // clone: an error owns what it names
            arg: given.arg.clone(),
            // alloc: as above
            word: word.text.to_owned(),
            from: given.from.clone(),
            // alloc: as above
            triple: triple.to_owned(),
            // alloc: as above
            why: why.to_owned(),
        });
    };
    // The `--no-as-needed` in effect, which a library after it is refused under.
    let mut no_as_needed = false;
    let mut at = 0;
    while at < words.len() {
        let word = words[at];
        let value = words.get(at + 1).map(|it| it.text);
        let read = match spelling {
            Spelling::Ld64 => ld64(word, value, kind),
            Spelling::Gnu => gnu(word, value, kind),
            Spelling::Msvc | Spelling::WasmLd => Err(why::NO_DRIVER),
        };
        match read {
            Ok((option, took)) => {
                match (&option, word.text) {
                    (_, "--no-as-needed") => no_as_needed = true,
                    (_, "--as-needed") => no_as_needed = false,
                    (LinkOption::Lib(_), _) if no_as_needed && spelling == Spelling::Gnu => {
                        refuse(&word, why::AFTER_NO_AS_NEEDED);
                    }
                    _ => {}
                }
                options.push(option);
                at += 1 + took;
            }
            Err(why) => {
                refuse(&word, why);
                at += 1;
                // The rest of a refused argument (`-Wl,-sectcreate,__TEXT,...`) is its own, and
                // so is a value given apart after it (`-weak_framework`, `Metal`): not refused
                // again as words of their own.
                let last = words.get(at).is_none_or(|it| it.at != word.at);
                while words.get(at).is_some_and(|it| it.at == word.at) {
                    at += 1;
                }
                if last && words.get(at).is_some_and(|it| !it.text.is_empty() && !it.text.starts_with('-')) {
                    at += 1;
                }
            }
        }
    }
    match errors.is_empty() {
        true => Ok(options),
        false => Err(errors),
    }
}

/// A library linked as `-l` links it.
fn lib(name: &str, kind: NativeLibKind, verbatim: bool) -> LinkOption {
    // alloc: once per library among the link arguments, the name the link line owns
    LinkOption::Lib(NativeLib { name: name.to_owned(), kind, verbatim })
}

/// `-lNAME` (`-l:FILE` verbatim, GNU only).
fn dash_l(name: &str, gnu: bool) -> Result<LinkOption, &'static str> {
    match name.strip_prefix(':') {
        Some(file) if gnu && !file.is_empty() => Ok(lib(file, NativeLibKind::Dylib, true)),
        _ if name.is_empty() || name.starts_with(':') => Err(why::NO_VALUE),
        _ => Ok(lib(name, NativeLibKind::Dylib, false)),
    }
}

/// An option that takes the next word, or its value joined after `prefix` (`-LDIR`).
fn joined_or_next<'a>(
    text: &'a str,
    prefix: &str,
    value: Option<&'a str>,
) -> Option<Result<(&'a str, usize), &'static str>> {
    let rest = text.strip_prefix(prefix)?;
    Some(match rest.is_empty() {
        false => Ok((rest, 0)),
        true => value.map(|it| (it, 1)).ok_or(why::NO_VALUE),
    })
}

/// ld64's hexadecimal size, as `-headerpad` and `-stack_size` take it (`strtoull` in base 16:
/// `0x` optional).
fn hex(value: &str) -> Result<u64, &'static str> {
    let digits = value.strip_prefix("0x").or_else(|| value.strip_prefix("0X")).unwrap_or(value);
    u64::from_str_radix(digits, 16).map_err(|_| why::SIZE)
}

/// The most header room `-headerpad` may ask: the image's 32-bit offsets and its first page's
/// layout take no more without a reason to.
pub const HEADERPAD_MAX: u64 = 1 << 20;

/// What the C driver does with a word of its own that both drivers read alike, or `None` for
/// one of the linker's.
fn driver_common(word: Word<'_>) -> Option<Read> {
    if !word.driver {
        return None;
    }
    let text = word.text;
    Some(match text {
        "" => Err(why::EMPTY),
        "-pthread" => Ok((lib("pthread", NativeLibKind::Dylib, false), 0)),
        "-static" | "-static-pie" | "-nostartfiles" | "-nostdlib" | "-nodefaultlibs" => {
            Err(why::STATIC)
        }
        "-shared" | "-dynamiclib" | "-bundle" | "-r" => Err(why::UNKNOWN),
        "-rdynamic" => Err(why::EXPORT_DYNAMIC),
        "-Xlinker" => Err(why::NO_VALUE),
        _ if text.starts_with("-fuse-ld=") || text.starts_with("--ld-path=") => {
            Ok((LinkOption::NoEffect(reason::LINKER), 0))
        }
        _ => return None,
    })
}

/// A word in ld64's spelling (the driver's forwards of it alike: `-framework`, `-undefined`,
/// `-rpath`, `-headerpad_max_install_names`, ...).
fn ld64(word: Word<'_>, value: Option<&str>, kind: Kind) -> Read {
    if let Some(read) = driver_common(word) {
        return read;
    }
    let text = word.text;
    let shared = kind == Kind::Shared;
    let next = || value.ok_or(why::NO_VALUE);
    Ok(match text {
        "" => return Err(why::EMPTY),
        "-framework" | "-needed_framework" => {
            (lib(next()?, NativeLibKind::Framework, false), 1)
        }
        "-weak_framework" | "-reexport_framework" | "-lazy_framework" | "-weak_library"
        | "-reexport_library" | "-lazy_library" | "-upward_framework" | "-upward_library" => {
            return Err(why::WEAK);
        }
        // alloc: the rpath the output owns
        "-rpath" => (LinkOption::Rpath(next()?.to_owned()), 1),
        "-headerpad" => {
            let size = hex(next()?)?;
            if size > HEADERPAD_MAX {
                return Err(why::HEADERPAD_MAX);
            }
            (LinkOption::HeaderPad(size), 1)
        }
        "-headerpad_max_install_names" => (LinkOption::HeaderPadMaxInstallNames, 0),
        "-undefined" => match next()? {
            "dynamic_lookup" => (LinkOption::Undefined(Undefined::DynamicLookup), 1),
            "error" if shared => (LinkOption::Undefined(Undefined::Error), 1),
            "error" => (LinkOption::Already(reason::UNDEFINED_ERROR), 1),
            "warning" | "suppress" => return Err(why::UNDEFINED),
            _ => return Err(why::UNKNOWN),
        },
        "-stack_size" if shared => return Err(why::EXECUTABLE_ONLY),
        "-stack_size" => {
            let size = hex(next()?)?;
            if size % 0x1000 != 0 {
                return Err(why::STACK_PAGES);
            }
            (LinkOption::StackSize(size), 1)
        }
        "-exported_symbol" if !shared => return Err(why::EXPORT_DYNAMIC),
        "-exported_symbol" => match next()?.strip_prefix('_') {
            Some(core) if !core.is_empty() && !core.contains(['*', '?', '[']) => {
                // alloc: the export the output owns
                (LinkOption::Export(core.to_owned()), 1)
            }
            _ => return Err(why::EXPORT_NAME),
        },
        "-install_name" | "-dylib_install_name" if !shared => return Err(why::SHARED_ONLY),
        // alloc: the name the output owns
        "-install_name" | "-dylib_install_name" => (LinkOption::Name(next()?.to_owned()), 1),
        "-dead_strip" => (LinkOption::Already(reason::GC), 0),
        "-pie" if shared => return Err(why::EXECUTABLE_ONLY),
        "-pie" => (LinkOption::Already(reason::PIE), 0),
        "-no_pie" => return Err(why::STATIC),
        "-search_paths_first" => (LinkOption::Already(reason::SEARCH_FIRST), 0),
        "-adhoc_codesign" => (LinkOption::Already(reason::ADHOC), 0),
        "-w" => (LinkOption::NoEffect(reason::WARNINGS), 0),
        "-export_dynamic" | "-exported_symbols_list" | "-unexported_symbol"
        | "-unexported_symbols_list" => return Err(why::EXPORT_DYNAMIC),
        _ => {
            for prefix in ["-L", "-F"] {
                if let Some(it) = joined_or_next(text, prefix, value) {
                    let (dir, took) = it?;
                    return Ok((LinkOption::Search(PathBuf::from(dir)), took));
                }
            }
            if let Some(name) = text.strip_prefix("-needed-l") {
                return Ok((dash_l(name, false)?, 0));
            }
            if ["-weak-l", "-reexport-l", "-lazy-l", "-upward-l"]
                .iter()
                .any(|it| text.starts_with(it))
            {
                return Err(why::WEAK);
            }
            if let Some(name) = text.strip_prefix("-l") {
                return Ok((dash_l(name, false)?, 0));
            }
            if !text.starts_with('-') {
                return Err(why::INPUT_FILE);
            }
            return Err(why::UNKNOWN);
        }
    })
}

/// A word in GNU ld's and lld's spelling (the driver's forwards of it alike: `-z`, `-L`,
/// `-l`).
fn gnu(word: Word<'_>, value: Option<&str>, kind: Kind) -> Read {
    if let Some(read) = driver_common(word) {
        return read;
    }
    let text = word.text;
    let shared = kind == Kind::Shared;
    let next = || value.ok_or(why::NO_VALUE);
    Ok(match text {
        "" => return Err(why::EMPTY),
        "-z" => (z(next()?, kind)?, 1),
        "-l" => (dash_l(next()?, true)?, 1),
        // `-R DIR` is an rpath only where DIR is a directory, and a file of symbols otherwise:
        // `$ORIGIN` is one always.
        "-R" | "-rpath" | "--rpath" => {
            let dir = next()?;
            if text == "-R" && !dir.starts_with("$ORIGIN") && !std::path::Path::new(dir).is_dir()
            {
                return Err(why::UNKNOWN);
            }
            // alloc: the rpath the output owns
            (LinkOption::Rpath(dir.to_owned()), 1)
        }
        "-soname" | "-h" if !shared => return Err(why::SHARED_ONLY),
        // alloc: the name the output owns
        "-soname" | "-h" => (LinkOption::Name(next()?.to_owned()), 1),
        "--gc-sections" => (LinkOption::Already(reason::GC), 0),
        "--as-needed" => (LinkOption::Already(reason::AS_NEEDED), 0),
        "--no-as-needed" => (LinkOption::NoEffect(reason::NO_AS_NEEDED), 0),
        "--eh-frame-hdr" => (LinkOption::Already(reason::EH_FRAME_HDR), 0),
        "-Bsymbolic" | "-Bsymbolic-functions" => (LinkOption::Already(reason::SYMBOLIC), 0),
        "--hash-style=gnu" => (LinkOption::Already(reason::GNU_HASH), 0),
        "-pie" | "--pie" if shared => return Err(why::EXECUTABLE_ONLY),
        "-pie" | "--pie" => (LinkOption::Already(reason::PIE), 0),
        "-O0" | "-O1" | "-O2" if !word.driver => (LinkOption::NoEffect(reason::LD_O), 0),
        "--export-dynamic" | "-E" | "-export-dynamic" => return Err(why::EXPORT_DYNAMIC),
        "--export-dynamic-symbol" | "-export-dynamic-symbol" => (export_dynamic(next()?, kind)?, 1),
        "-static" | "-Bstatic" | "--no-dynamic-linker" | "-no-pie" | "--no-pie" => {
            return Err(why::STATIC);
        }
        _ => {
            if let Some(keyword) = text.strip_prefix("-z") {
                return Ok((z(keyword, kind)?, 0));
            }
            if let Some(it) = joined_or_next(text, "-L", value) {
                let (dir, took) = it?;
                return Ok((LinkOption::Search(PathBuf::from(dir)), took));
            }
            if let Some(dir) = text.strip_prefix("--library-path=") {
                return match dir.is_empty() {
                    true => Err(why::NO_VALUE),
                    false => Ok((LinkOption::Search(PathBuf::from(dir)), 0)),
                };
            }
            if let Some(name) = text.strip_prefix("--library=") {
                return Ok((dash_l(name, true)?, 0));
            }
            if let Some(name) = text.strip_prefix("-l") {
                return Ok((dash_l(name, true)?, 0));
            }
            for prefix in ["-rpath=", "--rpath="] {
                if let Some(dir) = text.strip_prefix(prefix) {
                    // alloc: the rpath the output owns
                    return Ok((LinkOption::Rpath(dir.to_owned()), 0));
                }
            }
            for prefix in ["-soname=", "--soname="] {
                if let Some(name) = text.strip_prefix(prefix) {
                    return match shared {
                        // alloc: the name the output owns
                        true => Ok((LinkOption::Name(name.to_owned()), 0)),
                        false => Err(why::SHARED_ONLY),
                    };
                }
            }
            for prefix in ["--export-dynamic-symbol=", "-export-dynamic-symbol="] {
                if let Some(pattern) = text.strip_prefix(prefix) {
                    return Ok((export_dynamic(pattern, kind)?, 0));
                }
            }
            // `--export-dynamic-symbol-list FILE` too: a file of patterns, which is not read.
            if ["--version-script", "--export-dynamic-symbol", "--dynamic-list"]
                .iter()
                .any(|it| text.starts_with(it))
            {
                return Err(why::EXPORT_DYNAMIC);
            }
            if text.starts_with("-T") || text.starts_with("--script") {
                return Err(why::SCRIPT);
            }
            if !text.starts_with('-') {
                return Err(why::INPUT_FILE);
            }
            return Err(why::UNKNOWN);
        }
    })
}

/// GNU ld's and lld's `--export-dynamic-symbol PATTERN`.
fn export_dynamic(pattern: &str, kind: Kind) -> Result<LinkOption, &'static str> {
    if pattern.is_empty() {
        return Err(why::NO_VALUE);
    }
    Ok(match kind {
        // alloc: the pattern the output owns
        Kind::Executable => LinkOption::ExportDynamic(pattern.to_owned()),
        Kind::Shared => LinkOption::NoEffect(reason::SHARED_EXPORTS),
    })
}

/// Whether `name` matches `pattern` as GNU ld and lld match a symbol pattern (`fnmatch`
/// without flags): `*` is any run of bytes, `?` any one, `[...]` one of a set (`a-z` a
/// range, a leading `!` or `^` the bytes not in it, a `]` first is itself), `\` takes the
/// next byte as itself, and any other byte is itself. A `[` with no `]` is itself.
pub fn glob(pattern: &str, name: &str) -> bool {
    let (pattern, name) = (pattern.as_bytes(), name.as_bytes());
    // Where the last `*` was, and how much of the name it has taken: a mismatch after it
    // gives it one byte more. Linear in the name for each star, no recursion.
    let (mut p, mut n) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        let step = match pattern.get(p) {
            Some(b'*') => {
                star = Some((p, n));
                p += 1;
                continue;
            }
            Some(b'?') => Some(p + 1),
            Some(b'[') => match set(&pattern[p..], name[n]) {
                Some((true, len)) => Some(p + len),
                Some((false, _)) => None,
                None => (name[n] == b'[').then_some(p + 1),
            },
            Some(b'\\') if p + 1 < pattern.len() => (pattern[p + 1] == name[n]).then_some(p + 2),
            Some(&byte) => (byte == name[n]).then_some(p + 1),
            None => None,
        };
        match (step, star) {
            (Some(next), _) => {
                p = next;
                n += 1;
            }
            (None, Some((at, taken))) => {
                star = Some((at, taken + 1));
                p = at + 1;
                n = taken + 1;
            }
            (None, None) => return false,
        }
    }
    pattern[p..].iter().all(|&it| it == b'*')
}

/// Of a pattern at its `[`: whether the set holds `byte`, and the set's length with its
/// brackets; `None` when no `]` closes it.
fn set(pattern: &[u8], byte: u8) -> Option<(bool, usize)> {
    let mut at = 1;
    let negated = matches!(pattern.get(at), Some(b'!' | b'^'));
    if negated {
        at += 1;
    }
    let first = at;
    let mut holds = false;
    loop {
        let low = *pattern.get(at)?;
        if low == b']' && at > first {
            return Some((holds != negated, at + 1));
        }
        match (pattern.get(at + 1), pattern.get(at + 2)) {
            (Some(b'-'), Some(&high)) if high != b']' => {
                holds |= (low..=high).contains(&byte);
                at += 3;
            }
            _ => {
                holds |= low == byte;
                at += 1;
            }
        }
    }
}

/// GNU ld's and lld's `-z KEYWORD`.
fn z(keyword: &str, kind: Kind) -> Result<LinkOption, &'static str> {
    let shared = kind == Kind::Shared;
    Ok(match keyword {
        "now" => LinkOption::Already(reason::BIND_NOW),
        "relro" => LinkOption::Already(reason::RELRO),
        "noexecstack" => LinkOption::Already(reason::NOEXECSTACK),
        "nodelete" if shared => LinkOption::NoDelete,
        "nodelete" => LinkOption::NoEffect(reason::NOT_UNLOADED),
        "defs" if shared => LinkOption::Undefined(Undefined::Error),
        "defs" => LinkOption::Already(reason::DEFS),
        "lazy" | "norelro" | "execstack" => return Err(why::NOT_ALWAYS),
        "" => return Err(why::NO_VALUE),
        _ => return Err(why::UNKNOWN),
    })
}

/// What [`apply`] adds to the link line: the libraries, after every other, and the
/// directories to search, after the link's own.
#[derive(Default, Debug)]
pub struct Applied {
    pub libs: Vec<NativeLib>,
    pub search: Vec<PathBuf>,
}

/// Sets what the rustflags' `-C` options (`options`, [`rustflags::Rustflags::options`]) and the
/// link arguments (`args`) ask on `output`, whose kind is set ([`Output::link`], its stack, a
/// shared library's name and exports, its debug info), and hands back the libraries and search
/// directories the arguments add. The options come first, as rustc gives its own link
/// arguments before the ones it is given: a later `-install_name` wins over `-C rpath`'s.
/// Every option and argument refused is named in the one error.
pub fn apply(
    output: &mut Output,
    options: &[rustflags::GivenOption],
    args: &[GivenArg],
) -> Result<Applied, LinkError> {
    let mut applied = Applied::default();
    // alloc: none unless an option or argument is refused
    let mut errors: Vec<ArgError> = Vec::new();
    for given in options {
        if let Err(why) = rustflags::set(output, &given.option) {
            errors.push(ArgError {
                // clone: an error owns what it names
                arg: given.flag.clone(),
                // clone: as above
                word: given.flag.clone(),
                from: ArgFrom::RustflagOption,
                // clone: as above
                triple: output.triple.clone(),
                // alloc: as above
                why: why.to_owned(),
            });
        }
    }
    if args.is_empty() {
        return match errors.is_empty() {
            true => Ok(applied),
            false => Err(refused(&errors)),
        };
    }
    let options = match parse(Spelling::of(output.format), Kind::of(output), &output.triple, args)
    {
        Ok(options) if errors.is_empty() => options,
        Ok(_) => return Err(refused(&errors)),
        Err(more) => {
            errors.extend(more);
            return Err(refused(&errors));
        }
    };
    for option in options {
        match option {
            LinkOption::Lib(lib) => applied.libs.push(lib),
            LinkOption::Search(dir) => applied.search.push(dir),
            LinkOption::Rpath(dir) => output.link.rpaths.push(dir),
            // ld64 keeps the last of each.
            LinkOption::HeaderPad(size) => output.link.headerpad = size,
            LinkOption::HeaderPadMaxInstallNames => output.link.headerpad_max_install_names = true,
            LinkOption::Undefined(undefined) => output.link.undefined = undefined,
            LinkOption::StackSize(size) => output.stack = Some(size),
            LinkOption::Export(name) => {
                if let OutputKind::Shared { exports, .. } = &mut output.kind
                    && !exports.contains(&name)
                {
                    exports.push(name);
                }
            }
            LinkOption::Name(name) => {
                if let OutputKind::Shared { name: was, .. } = &mut output.kind {
                    *was = name;
                }
            }
            LinkOption::NoDelete => output.link.nodelete = true,
            LinkOption::ExportDynamic(pattern) => {
                if !output.link.export_dynamic.contains(&pattern) {
                    output.link.export_dynamic.push(pattern);
                }
            }
            LinkOption::Already(_) | LinkOption::NoEffect(_) => {}
        }
    }
    Ok(applied)
}

/// The one error naming every option and argument refused.
fn refused(errors: &[ArgError]) -> LinkError {
    let mut what = String::from(
        "blaze's linker drops no link argument or link option of the rustflags, and refuses \
         each it does not implement (docs/cargo.md, \"Link arguments\"):",
    );
    for error in errors {
        what.push_str("\n  ");
        // `fmt::Write` to a `String` cannot fail.
        let _ = std::fmt::Write::write_fmt(&mut what, format_args!("{error}"));
    }
    LinkError { function: None, what, refusal: None }
}
