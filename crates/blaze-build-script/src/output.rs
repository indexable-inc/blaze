//! What a build script prints, read as cargo 1.99 reads it: `BuildOutput::parse` in
//! `src/compiler/custom_build.rs` of rust-lang/cargo at 5f94df478. Lines that start with
//! neither `cargo:` nor `cargo::`, and lines that are not UTF-8, are the script's own.
//!
//! One pass over the bytes ([`read`]): memchr's SIMD search for line breaks, a line that is
//! not a directive refused on its first byte past ASCII space, a directive's key matched by
//! its length and then its bytes to a [`Key`], and every value borrowed from the output (a
//! [`Directive`]) unless the `OUT_DIR` it names moved. [`parse`] owns what [`Output`] keeps;
//! another [`Sink`] (what cargo watches, [`crate::watch::watched_in`]) keeps only what it reads.

use std::{
    borrow::Cow,
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
};

use serde_derive::Serialize;

use crate::{
    Error,
    unit::{Package, Unfilled, Unit},
};

/// A message a script asked cargo to show.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub enum Severity {
    Error,
    Warning,
}

/// A directory of `rustc-link-search` or `rustc-flags`' `-L`, as written (`native=/x`).
/// cargo passes those inside the script's `OUT_DIR` first, each kind in the order printed.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub enum LibraryPath {
    CargoArtifact(PathBuf),
    External(PathBuf),
}

impl LibraryPath {
    fn new(path: PathBuf, out_dir: &Path) -> LibraryPath {
        match search_path_of(&path).starts_with(out_dir) {
            true => LibraryPath::CargoArtifact(path),
            false => LibraryPath::External(path),
        }
    }

    pub fn path(&self) -> &Path {
        match self {
            LibraryPath::CargoArtifact(it) | LibraryPath::External(it) => it,
        }
    }
}

/// The directory of a search path, without its kind (`native=`, `crate=`, `dependency=`,
/// `framework=`, `all=`).
pub fn search_path_of(path: &Path) -> &Path {
    match path.to_str().and_then(|it| it.split_once('=')) {
        Some(("native" | "crate" | "dependency" | "framework" | "all", path)) => Path::new(path),
        _ => path,
    }
}

/// Which of the package's targets a `rustc-link-arg*` is for.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub enum LinkArgTarget {
    /// `rustc-link-arg`.
    All,
    /// `rustc-cdylib-link-arg` and `rustc-link-arg-cdylib`.
    Cdylib,
    /// `rustc-link-arg-bins`.
    Bin,
    /// `rustc-link-arg-bin=NAME=ARG`.
    SingleBin(String),
    /// `rustc-link-arg-tests`.
    Test,
    /// `rustc-link-arg-benches`.
    Bench,
    /// `rustc-link-arg-examples`.
    Example,
}

/// What a script printed, as cargo keeps it.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct Output {
    /// `-L` for the package and everything that links it.
    pub library_paths: Vec<LibraryPath>,
    /// `-l` for the package's library (or its programs when it has none), as written:
    /// `[KIND[:MODIFIERS]=]NAME[:RENAME]`.
    pub library_links: Vec<String>,
    pub linker_args: Vec<(LinkArgTarget, String)>,
    /// `--cfg` for every target of the package.
    pub cfgs: Vec<String>,
    /// `--check-cfg` for every target of the package.
    pub check_cfgs: Vec<String>,
    /// Variables for rustc's runs on the package's targets, and for `cargo run` and `cargo
    /// test` of them.
    pub env: Vec<(String, String)>,
    /// What the scripts of packages that depend on this one directly see as
    /// `DEP_<links>_<key>`, when this one has `links`.
    pub metadata: Vec<(String, String)>,
    /// Relative to the package's directory, or absolute.
    pub rerun_if_changed: Vec<PathBuf>,
    pub rerun_if_env_changed: Vec<String>,
    pub log_messages: Vec<(Severity, String)>,
}

/// One directive as [`read`] reads it, in the order printed: borrowed from the output, or
/// owned when its value named the `OUT_DIR` the script ran with and that moved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Directive<'a> {
    /// `rustc-link-search`, or `-L` of `rustc-flags`, as written: `[KIND=]PATH`.
    LinkSearch(Cow<'a, str>),
    /// `rustc-link-lib`, or `-l` of `rustc-flags`, as written.
    LinkLib(Cow<'a, str>),
    LinkArg(LinkArgTarget, Cow<'a, str>),
    Cfg(Cow<'a, str>),
    CheckCfg(Cow<'a, str>),
    /// `rustc-env=NAME=VALUE`, for any name but `RUSTC_BOOTSTRAP`.
    Env(Cow<'a, str>, Cow<'a, str>),
    /// A key and its value.
    Metadata(Cow<'a, str>, Cow<'a, str>),
    RerunIfChanged(Cow<'a, str>),
    RerunIfEnvChanged(Cow<'a, str>),
    /// `error`, `warning`, and what cargo warns of a directive.
    Log(Severity, Cow<'a, str>),
}

/// What [`read`] gives each directive to. When `read` fails, what the sink was given is of
/// an output cargo refuses, and is to be dropped.
pub trait Sink<'a> {
    fn directive(&mut self, directive: Directive<'a>);
}

impl<'a> Sink<'a> for Vec<Directive<'a>> {
    fn directive(&mut self, directive: Directive<'a>) {
        self.push(directive);
    }
}

/// The sink of [`parse`]: what [`Output`] keeps, owned.
struct Collect<'c> {
    out: Output,
    out_dir: &'c Path,
}

impl<'a> Sink<'a> for Collect<'_> {
    fn directive(&mut self, directive: Directive<'a>) {
        let out = &mut self.out;
        match directive {
            Directive::LinkSearch(it) => {
                let path = PathBuf::from(it.into_owned());
                out.library_paths.push(LibraryPath::new(path, self.out_dir))
            }
            Directive::LinkLib(it) => out.library_links.push(it.into_owned()),
            Directive::LinkArg(to, it) => out.linker_args.push((to, it.into_owned())),
            Directive::Cfg(it) => out.cfgs.push(it.into_owned()),
            Directive::CheckCfg(it) => out.check_cfgs.push(it.into_owned()),
            Directive::Env(name, value) => out.env.push((name.into_owned(), value.into_owned())),
            Directive::Metadata(key, value) => {
                out.metadata.push((key.into_owned(), value.into_owned()))
            }
            Directive::RerunIfChanged(it) => {
                out.rerun_if_changed.push(PathBuf::from(it.into_owned()))
            }
            Directive::RerunIfEnvChanged(it) => out.rerun_if_env_changed.push(it.into_owned()),
            Directive::Log(severity, it) => out.log_messages.push((severity, it.into_owned())),
        }
    }
}

/// What reading a script's output depends on besides the output.
#[derive(Clone, Debug)]
pub struct Context<'a> {
    pub package: &'a Package,
    /// The `OUT_DIR` the script ran with, and the one it has now: a path the output names in
    /// the first is taken to be in the second, as when the target directory moved.
    pub out_dir_when_generated: &'a Path,
    pub out_dir: &'a Path,
    /// Whether cargo allows nightly features: its channel, or `RUSTC_BOOTSTRAP=1`.
    pub nightly_features_allowed: bool,
    /// `RUSTC_BOOTSTRAP` in cargo's environment.
    pub rustc_bootstrap: Option<String>,
}

impl<'a> Context<'a> {
    /// The context of `unit`'s run under a stable cargo whose environment is `cargo_env`.
    pub fn of(unit: &'a Unit, cargo_env: &[(OsString, OsString)]) -> Context<'a> {
        let var = |name: &str| {
            cargo_env
                .iter()
                .find(|(it, _)| it == OsStr::new(name))
                .and_then(|(_, value)| value.to_str())
        };
        let rustc_bootstrap = var("RUSTC_BOOTSTRAP");
        let channel = var("__CARGO_TEST_CHANNEL_OVERRIDE_DO_NOT_USE_THIS")
            .or_else(|| (rustc_bootstrap == Some("1")).then_some("dev"));
        Context {
            package: &unit.package,
            out_dir_when_generated: &unit.out_dir,
            out_dir: &unit.out_dir,
            nightly_features_allowed: matches!(channel, Some("nightly" | "dev")),
            rustc_bootstrap: rustc_bootstrap.map(str::to_owned),
        }
    }
}

const DOCS: &str = "See https://doc.rust-lang.org/cargo/reference/build-scripts.html#outputs-of-the-build-script \
                    for more information about build script outputs.";

/// The keys cargo reads, one byte each.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Key {
    Flags,
    LinkLib,
    LinkSearch,
    LinkArgCdylib,
    CdylibLinkArg,
    LinkArgBins,
    LinkArgBin,
    LinkArgTests,
    LinkArgBenches,
    LinkArgExamples,
    LinkArg,
    Cfg,
    CheckCfg,
    Env,
    Warning,
    RerunIfChanged,
    RerunIfEnvChanged,
    /// Of `cargo::` alone: `cargo:error=` is metadata.
    Error,
    /// Of `cargo::` alone: `cargo:metadata=` is metadata of the key `metadata`.
    Metadata,
}

/// Each [`Key`]'s name, by its byte.
const KEYS: [&str; 19] = [
    "rustc-flags",
    "rustc-link-lib",
    "rustc-link-search",
    "rustc-link-arg-cdylib",
    "rustc-cdylib-link-arg",
    "rustc-link-arg-bins",
    "rustc-link-arg-bin",
    "rustc-link-arg-tests",
    "rustc-link-arg-benches",
    "rustc-link-arg-examples",
    "rustc-link-arg",
    "rustc-cfg",
    "rustc-check-cfg",
    "rustc-env",
    "warning",
    "rerun-if-changed",
    "rerun-if-env-changed",
    "error",
    "metadata",
];

impl Key {
    /// The key named `name`: its length leaves one candidate, or two told apart by one
    /// byte, and one comparison of the bytes decides.
    pub fn of(name: &[u8]) -> Option<Key> {
        let candidate = match name.len() {
            5 => Key::Error,
            7 => Key::Warning,
            8 => Key::Metadata,
            // `rustc-cfg`, `rustc-env`.
            9 if name[6] == b'c' => Key::Cfg,
            9 => Key::Env,
            11 => Key::Flags,
            // `rustc-link-lib`, `rustc-link-arg`.
            14 if name[11] == b'l' => Key::LinkLib,
            14 => Key::LinkArg,
            15 => Key::CheckCfg,
            16 => Key::RerunIfChanged,
            17 => Key::LinkSearch,
            18 => Key::LinkArgBin,
            19 => Key::LinkArgBins,
            // `rustc-link-arg-tests`, `rerun-if-env-changed`.
            20 if name[1] == b'u' => Key::LinkArgTests,
            20 => Key::RerunIfEnvChanged,
            // `rustc-link-arg-cdylib`, `rustc-cdylib-link-arg`.
            21 if name[6] == b'l' => Key::LinkArgCdylib,
            21 => Key::CdylibLinkArg,
            22 => Key::LinkArgBenches,
            23 => Key::LinkArgExamples,
            _ => return None,
        };
        (name == candidate.name().as_bytes()).then_some(candidate)
    }

    pub fn name(self) -> &'static str {
        KEYS[self as usize]
    }

    /// Whether the old syntax reads `cargo:KEY=` as this key, not as metadata: no new one
    /// can be added, since `cargo:KEY=VALUE` is metadata for any other.
    fn reserved(self) -> bool {
        !matches!(self, Key::Error | Key::Metadata)
    }
}

/// The keys of the old syntax that are not metadata.
fn reserved(key: &str) -> bool {
    Key::of(key.as_bytes()).is_some_and(Key::reserved)
}

/// Whether `flag` starts with a reserved key and its `=`: cargo's prefix check, the same as
/// the first `=` ending a reserved key, since no key has an `=` in it.
pub fn has_reserved_prefix(flag: &str) -> bool {
    flag.split_once('=').is_some_and(|(key, _)| reserved(key))
}

/// `rust-version` as cargo reads it: one to three numbers.
fn rust_version(text: &str) -> Option<(u64, Option<u64>, Option<u64>)> {
    let mut parts = text.trim().split('.');
    let mut number = |required: bool| match parts.next() {
        Some(it) => it.parse::<u64>().ok().map(Some),
        None => (!required).then_some(None),
    };
    let version = (number(true)??, number(false)?, number(false)?);
    parts.next().is_none().then_some(version)
}

/// The package's `rust-version` as cargo shows it, when it is older than 1.77, which brought
/// `cargo::`: then it cannot use that syntax. One that is not a version is an error, as
/// cargo's manifest reader refuses it.
fn older_than_new_syntax(package: &Package) -> Result<Option<String>, Unfilled> {
    let Some(written) = package.rust_version.as_deref() else { return Ok(None) };
    let Some((major, minor, patch)) = rust_version(written) else {
        // alloc: error path.
        return Err(Unfilled::RustVersion { package: package.descr(), rust_version: written.to_owned() });
    };
    let compatible = major == 1 && (minor.unwrap_or(0), patch.unwrap_or(0)) >= (77, 0);
    if compatible {
        return Ok(None);
    }
    let shown =
        // alloc: error path: the version shown when `cargo::` is refused.
        [Some(major), minor, patch].iter().flatten().map(u64::to_string).collect::<Vec<_>>();
    Ok(Some(shown.join(".")))
}

/// Why a `cargo::` line of a package whose `rust-version` is `msrv`
/// ([`older_than_new_syntax`]) is refused.
fn new_syntax_refused(descr: Descr<'_>, msrv: &str, flag: &str) -> Error {
    let suggestion = if has_reserved_prefix(flag) {
        // alloc: error path.
        format!("Switch to the old `cargo:{flag}` syntax (note the single colon).\n")
    } else if let Some(old) = flag.strip_prefix("metadata=") {
        // alloc: error path.
        format!(
            "Switch to the old `cargo:{old}` syntax instead of `cargo::{flag}` (note the single colon).\n"
        )
    } else {
        // alloc: error path (empty, allocates nothing).
        String::new()
    };
    // alloc: error path.
    Error(format!(
        "the `cargo::` syntax for build script output instructions was added in \
         Rust 1.77.0, but the minimum supported Rust version of `{descr}` is {msrv}.\n\
         {suggestion}{DOCS}"
    ))
}

/// `KEY=VALUE`, the value's end trimmed.
fn key_value<'a>(
    package: &Package,
    line: &str,
    data: &'a str,
    syntax: &str,
) -> Result<(&'a str, &'a str), Error> {
    match data.split_once('=') {
        Some((key, value)) => Ok((key, value.trim_end())),
        // alloc: error path.
        None => Err(Error(format!(
            "invalid output in {}: `{line}`\n\
             Expected a line with `{syntax}KEY=VALUE` with an `=` character, \
             but none was found.\n\
             {DOCS}",
            Whence(package)
        ))),
    }
}

/// The package as a message names it, made only when a message is.
struct Descr<'a>(&'a Package);

impl std::fmt::Display for Descr<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.write_descr(f)
    }
}

/// Where a message says a line came from, made only when a message is.
struct Whence<'a>(&'a Package);

impl std::fmt::Display for Whence<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "build script of `{}`", Descr(self.0))
    }
}

/// The bytes `str::trim` takes from a line's start that are ASCII: the ASCII characters of
/// Unicode's `White_Space`.
fn is_ascii_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r')
}

/// The line, trimmed, when it can be a directive: what `from_utf8` and `trim` make of it,
/// for a line that then starts with `cargo:`. Every other line is the script's own, and most
/// are known to be from their first bytes alone, without reading the rest as UTF-8: a line
/// whose first byte past ASCII space is ASCII and does not start `cargo:` is trimmed to a
/// start that does not either. A first byte past ASCII (a space of Unicode's, or none) takes
/// the whole check.
fn directive(line: &[u8]) -> Option<&str> {
    let start = line.iter().position(|it| !is_ascii_space(*it))?;
    if line[start].is_ascii() && !line[start..].starts_with(b"cargo:") {
        return None;
    }
    let line = std::str::from_utf8(line).ok()?.trim();
    line.starts_with("cargo:").then_some(line)
}

/// `part`, a slice of `whole`, borrowed as long as `whole` is.
fn piece<'a>(whole: &Cow<'a, str>, part: &str) -> Cow<'a, str> {
    match whole {
        Cow::Borrowed(whole) => {
            let whole: &'a str = *whole;
            let start = part.as_ptr() as usize - whole.as_ptr() as usize;
            Cow::Borrowed(&whole[start..start + part.len()])
        }
        // alloc: only when `OUT_DIR` moved, which made the value anew.
        Cow::Owned(_) => Cow::Owned(part.to_owned()),
    }
}

/// Reads what a script printed, as [`read`] does, into what cargo keeps of it.
pub fn parse(input: &[u8], cx: &Context<'_>) -> Result<Output, Error> {
    let mut sink = Collect { out: Output::default(), out_dir: cx.out_dir };
    read(input, cx, &mut sink)?;
    Ok(sink.out)
}

/// Reads what a script printed into `sink`, one directive at a time in the order printed,
/// or fails as cargo does at the first line it refuses. Nothing is allocated but a value
/// that names a moved `OUT_DIR`, the bin of a `rustc-link-arg-bin`, and a message's text.
pub fn read<'a>(input: &'a [u8], cx: &Context<'_>, sink: &mut impl Sink<'a>) -> Result<(), Error> {
    let package = cx.package;
    let (Some(generated), Some(out_dir_text)) =
        (cx.out_dir_when_generated.to_str(), cx.out_dir.to_str())
    else {
        // alloc: error path.
        return Err(Error("an `OUT_DIR` that is not UTF-8".to_owned()));
    };
    // A path the output names in the `OUT_DIR` it ran with is moved to the one it has now;
    // when they are the same nothing moves, and nothing is searched for or copied.
    let moved = (!generated.is_empty() && generated != out_dir_text)
        .then(|| memchr::memmem::Finder::new(generated.as_bytes()));
    // Whether `cargo::` is refused, worked out at the first such line.
    let mut new_syntax_msrv: Option<Option<String>> = None;
    let mut rest = Some(input);
    while let Some(bytes) = rest {
        let line = match memchr::memchr(b'\n', bytes) {
            Some(at) => {
                rest = Some(&bytes[at + 1..]);
                &bytes[..at]
            }
            None => {
                rest = None;
                bytes
            }
        };
        let Some(line) = directive(line) else { continue };
        // `line` starts with `cargo:`; a seventh byte `:` makes it the new syntax.
        let (old_syntax, key, value) = match line.as_bytes().get(6) {
            Some(b':') => {
                let data = &line[7..];
                if new_syntax_msrv.is_none() {
                    new_syntax_msrv = Some(older_than_new_syntax(package)?);
                }
                if let Some(Some(msrv)) = &new_syntax_msrv {
                    return Err(new_syntax_refused(Descr(package), msrv, data));
                }
                let (key, value) = key_value(package, line, data, "cargo::")?;
                let Some(key) = Key::of(key.as_bytes()) else {
                    // alloc: error path.
                    return Err(Error(format!(
                        "invalid output in {}: `{line}`\n\
                         Unknown key: `{key}`.\n\
                         {DOCS}",
                        Whence(package)
                    )));
                };
                (false, key, value)
            }
            _ => {
                let data = &line[6..];
                let reserved = memchr::memchr(b'=', data.as_bytes()).and_then(|at| {
                    let key = Key::of(&data.as_bytes()[..at]).filter(|it| it.reserved())?;
                    Some((key, data[at + 1..].trim_end()))
                });
                match reserved {
                    Some((key, value)) => (true, key, value),
                    None => (true, Key::Metadata, data),
                }
            }
        };
        let value: Cow<'a, str> = match &moved {
            Some(finder) if finder.find(value.as_bytes()).is_some() => {
                Cow::Owned(value.replace(generated, out_dir_text))
            }
            _ => Cow::Borrowed(value),
        };
        let (descr, whence) = (Descr(package), Whence(package));
        let syntax = if old_syntax { "cargo:" } else { "cargo::" };
        let name = key.name();
        let missing_target = |kind: &str| {
            // alloc: error path.
            Error(format!(
                "invalid instruction `{syntax}{name}` from {whence}\n\
                 The package {descr} does not have a {kind} target."
            ))
        };
        let targets = &package.targets;
        let directive = match key {
            Key::Flags => {
                each_flag(&value, &whence, |link, flag| {
                    let flag = piece(&value, flag);
                    sink.directive(match link {
                        true => Directive::LinkLib(flag),
                        false => Directive::LinkSearch(flag),
                    })
                })?;
                continue;
            }
            Key::LinkLib => Directive::LinkLib(value),
            Key::LinkSearch => Directive::LinkSearch(value),
            Key::LinkArgCdylib | Key::CdylibLinkArg => {
                if !targets.iter().any(|it| it.is_cdylib()) {
                    sink.directive(Directive::Log(
                        Severity::Warning,
                        // alloc: a warning's text, made only when the warning is.
                        Cow::Owned(format!(
                            "{syntax}{name} was specified in the build script of {descr}, \
                             but that package does not contain a cdylib target\n\
                             \n\
                             Allowing this was an unintended change in the 1.50 \
                             release, and may become an error in the future. \
                             For more information, see \
                             <https://github.com/rust-lang/cargo/issues/9562>."
                        )),
                    ));
                }
                Directive::LinkArg(LinkArgTarget::Cdylib, value)
            }
            Key::LinkArgBins => {
                if !targets.iter().any(|it| it.is_bin()) {
                    return Err(missing_target("bin"));
                }
                Directive::LinkArg(LinkArgTarget::Bin, value)
            }
            Key::LinkArgBin => {
                let Some((bin, arg)) = value.split_once('=') else {
                    // alloc: error path.
                    return Err(Error(format!(
                        "invalid instruction `{syntax}{name}={value}` from {whence}\n\
                         The instruction should have the form {syntax}{name}=BIN=ARG"
                    )));
                };
                if !targets.iter().any(|it| it.is_bin() && it.name == bin) {
                    // alloc: error path.
                    return Err(Error(format!(
                        "invalid instruction `{syntax}{name}` from {whence}\n\
                         The package {descr} does not have a bin target with the name `{bin}`."
                    )));
                }
                // alloc: a `rustc-link-arg-bin`'s bin name, which `LinkArgTarget` owns.
                Directive::LinkArg(LinkArgTarget::SingleBin(bin.to_owned()), piece(&value, arg))
            }
            Key::LinkArgTests => {
                if !targets.iter().any(|it| it.is_test()) {
                    return Err(missing_target("test"));
                }
                Directive::LinkArg(LinkArgTarget::Test, value)
            }
            Key::LinkArgBenches => {
                if !targets.iter().any(|it| it.is_bench()) {
                    return Err(missing_target("benchmark"));
                }
                Directive::LinkArg(LinkArgTarget::Bench, value)
            }
            Key::LinkArgExamples => {
                if !targets.iter().any(|it| it.is_example()) {
                    return Err(missing_target("example"));
                }
                Directive::LinkArg(LinkArgTarget::Example, value)
            }
            Key::LinkArg => Directive::LinkArg(LinkArgTarget::All, value),
            Key::Cfg => Directive::Cfg(value),
            Key::CheckCfg => Directive::CheckCfg(value),
            Key::Env => {
                let Some((name, val)) = value.split_once('=') else {
                    // alloc: error path.
                    return Err(Error(format!(
                        "Variable rustc-env has no value in {whence}: {value}"
                    )));
                };
                if name != "RUSTC_BOOTSTRAP" {
                    Directive::Env(piece(&value, name), piece(&value, val))
                } else {
                    // A crate setting it would leave the stability guarantees; what cargo was
                    // run with can allow it, and then it only warns.
                    let library = package.library_crate_name();
                    let allowed_by_env = library.as_deref().is_some_and(|name| {
                        cx.rustc_bootstrap
                            .as_deref()
                            .is_some_and(|it| it.split(',').any(|it| it == name))
                    });
                    // alloc: error path, or the warning it becomes.
                    let message = format!(
                        "cannot set `RUSTC_BOOTSTRAP={val}` from {whence}.\n\
                         note: crates cannot set `RUSTC_BOOTSTRAP` themselves, as doing so \
                         would subvert the stability guarantees of Rust for your project."
                    );
                    if !(cx.nightly_features_allowed || allowed_by_env) {
                        let name = library.as_deref().unwrap_or("1");
                        // alloc: error path.
                        return Err(Error(format!(
                            "{message}\n\
                             help: If you're sure you want to do this in your project, set the \
                             environment variable `RUSTC_BOOTSTRAP={name}` before running \
                             cargo instead."
                        )));
                    }
                    Directive::Log(Severity::Warning, Cow::Owned(message))
                }
            }
            Key::Error => Directive::Log(Severity::Error, value),
            Key::Warning => Directive::Log(Severity::Warning, value),
            Key::RerunIfChanged => Directive::RerunIfChanged(value),
            Key::RerunIfEnvChanged => Directive::RerunIfEnvChanged(value),
            Key::Metadata => {
                let syntax = if old_syntax { "cargo:" } else { "cargo::metadata=" };
                let (key, val) = key_value(package, line, &value, syntax)?;
                Directive::Metadata(piece(&value, key), piece(&value, val))
            }
        };
        sink.directive(directive);
    }
    Ok(())
}

/// `rustc-flags`: only `-l` and `-L`, with or without a space before the value, each given
/// to `each` as whether it is `-l`, with its value.
fn each_flag<'v>(
    value: &'v str,
    whence: impl std::fmt::Display,
    mut each: impl FnMut(bool, &'v str),
) -> Result<(), Error> {
    let mut flags = value.trim().split(char::is_whitespace).filter(|it| !it.trim().is_empty());
    while let Some(flag) = flags.next() {
        let link = match flag.get(..2) {
            Some("-l") => true,
            Some("-L") => false,
            _ => {
                // alloc: error path.
                return Err(Error(format!(
                    "only `-l` and `-L` flags are allowed in {whence}: `{value}`"
                )));
            }
        };
        let mut rest = &flag[2..];
        if rest.is_empty() {
            rest = flags.next().ok_or_else(|| {
                // alloc: error path.
                Error(format!("flag in rustc-flags has no value in {whence}: {rest}"))
            })?;
        }
        each(link, rest);
    }
    Ok(())
}

/// A `rustc-link-lib` as rustc reads `-l`: `[KIND[:MODIFIERS]=]NAME[:RENAME]`. cargo passes
/// it on as written; rustc says what is wrong with one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkLib<'a> {
    /// `static`, `dylib`, `framework`, `raw-dylib` or `link-arg`; none when not written.
    pub kind: Option<&'a str>,
    /// `+bundle`, `-whole-archive` and the rest, each with whether it is turned on.
    pub modifiers: Vec<(bool, &'a str)>,
    pub name: &'a str,
    /// The name the library is linked under, after `:`.
    pub rename: Option<&'a str>,
}

impl<'a> LinkLib<'a> {
    pub fn parse(spec: &'a str) -> LinkLib<'a> {
        let (kind, name) = match spec.split_once('=') {
            Some((kind, name)) => (Some(kind), name),
            None => (None, spec),
        };
        let (kind, modifiers) = match kind.map(|it| it.split_once(':').unwrap_or((it, ""))) {
            Some((kind, modifiers)) => (Some(kind), modifiers),
            None => (None, ""),
        };
        let modifiers = modifiers
            .split(',')
            .filter(|it| !it.is_empty())
            .map(|it| match it.split_at(1) {
                ("+", name) => (true, name),
                ("-", name) => (false, name),
                _ => (true, it),
            })
            // alloc: the edge: `LinkLib` is read for a message, not on a run's path.
            .collect();
        let (name, rename) = match name.split_once(':') {
            Some((name, rename)) => (name, Some(rename)),
            None => (name, None),
        };
        LinkLib { kind, modifiers, name, rename }
    }

    /// Whether `modifier` is turned on.
    pub fn has(&self, modifier: &str) -> bool {
        self.modifiers.iter().rev().find(|(_, it)| *it == modifier).is_some_and(|(on, _)| *on)
    }
}

#[cfg(test)]
mod tests;
