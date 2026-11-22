//! A build script's deferred C requests run as tasks (design A, c.md D174 "Existing crates"):
//! one object a task, one archive a task, each a record written once, never the script again.
//!
//! `check` runs a C script as wasm with [`crate::CMode::Deferred`]: every native tool spawn is
//! answered without running and kept, in spawn order, as its argv joined by NUL
//! ([`OutputsRef::c_requests`]). `run` and `test` need the archives those requests would have
//! made. This module is the part of that which does no IO:
//!
//! - [`Request`]: one recorded spawn read into a typed request: an object compile, an archive
//!   append, a process tree (`sh configure`, `make`: one task for the whole tree), or a probe
//!   that `run` never repeats (`-E`, `-?`, a flag check whose object nothing archives);
//! - [`plan`]: the tasks a set of demanded archives needs: each archive's members, each member
//!   the one compile that wrote it; and the script's assumptions, the compiles no `ar` request
//!   names, each discharged once ([`Plan`]). Compiles of an archive not asked for are not
//!   tasks (rule 77). Each compile reads `OUT_DIR` as the state it saw at its spawn
//!   (outputs.rs, THE C INPUTS), through [`Todays`];
//! - the keys: [`pre_key`] (the object's inputs known before the work), [`path_set_key`] and
//!   [`object_key`] (the object valid by its read list, checked against today), [`archive_key`];
//! - [`write_archive`]: blaze's own `ar`, deterministic (rule 25);
//! - [`run_crate`]: the reference driver of one crate's tasks over the seams the caller hands
//!   in: the [`CCompiler`] strategy (rule 78: `ShelfClang` now, `Cfront` later), the store, the
//!   file area and today's file facts. The command's driver runs the same phases
//!   ([`look`], [`admit`], [`archive`]) with its misses compiled in parallel.
//!
//! WHY THE MODE IS IN NO KEY HERE. The script's record (scripts.rs, `MODE = Check`) is the same
//! record for `check`, `run` and `test`: it holds the requests, never their outputs. The objects
//! and archives are records of their own, asked only by `run` and `test`; `check` never asks
//! for them, so a `check` after a `run` re-runs nothing and leaves the script's record as it was.
//!
//! THE OBJECT'S KEY. Its inputs known before the work ([`pre_key`]): the compiler's identity
//! (the pinned clang's digest and the strategy), the SDK's identity (its version and build, never
//! its folder), the target, the language, the cooked argv (every path a fixed guest spelling:
//! `/package`, the guest `OUT_DIR`, a dependency's guest `OUT_DIR`, the guest sysroot), and the
//! source's digest. The headers it reads are known only after: they are its read list. A record
//! is written once (the store refuses other bytes at a key), so the read list is kept as a
//! PATH SET record at [`path_set_key`]`(pre, slot)`, slot 0 first, a new slot only when a
//! compile's set of reads differs from every recorded one (an `#include` added or removed). The
//! OBJECT record is at [`object_key`]`(pre, path set, today's digest of each path, today's
//! listing of each searched folder)`: valid by its key, since its key holds what it read.
//! An edit to a header's text moves only the object key; an edit to the include structure adds
//! one path set. A new file in a folder the compile searches (`-I`, the folder of the source
//! or of a header it read) moves that folder's listing, so a header that would now shadow
//! another is never missed. At most [`PATH_SETS`] sets a pre key (rule 18).
//!
//! Nothing here reads a clock, a path of the host or the environment (rule 75).

#![blaze::sans_io]

use blaze_ids::{ContentId, Hasher};
use blaze_own::{AString, AVec, Alloc, String, Vec};

use crate::instr::SearchKind;
use crate::outputs::OutputsRef;

/// The domain of every key here; a change to what a key covers takes the next number.
pub const FORM: &str = "blaze-build-script c tasks 1";

/// The most path sets one object's pre key may have (rule 18): past it the object is refused
/// by name ([`TaskError::PathSets`]), never compiled over a set unrecorded.
pub const PATH_SETS: u32 = 32;

/// The most requests one script's record may hold that a plan reads (rule 18).
pub const MOST_REQUESTS: usize = 1 << 16;

/// How many of the first lines of a failed compile's standard error an error keeps.
pub const ERROR_LINES: usize = 12;

// ---------------------------------------------------------------------------------------------
// Requests

/// A C language a compile is of.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    C = 1,
    Cxx = 2,
    /// `.S`: assembly through the C preprocessor.
    AsmCpp = 3,
    /// `.s`: assembly as written.
    Asm = 4,
}

/// The driver a compile was spawned as: the pinned `clang` or `clang++` (c_request.rs
/// `pinned_name`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Driver {
    Clang = 1,
    ClangXx = 2,
}

/// One object compile, borrowed from the request's text: `args` is every argument but the
/// program, `-c`, `-o <output>` and the source, in order.
#[derive(Debug)]
pub struct Compile<'a, 'b> {
    /// Its index among the record's requests.
    pub request: u32,
    /// The `OUT_DIR` state it saw ([`OutputsRef::c_state`]): its paths under the guest
    /// `OUT_DIR` are that state's files, never the script's last.
    pub state: u32,
    pub driver: Driver,
    pub lang: Lang,
    /// The source's guest path.
    pub source: &'b str,
    /// The object's guest path, under the guest `OUT_DIR`.
    pub output: &'b str,
    pub args: AVec<'a, &'b str>,
    /// The folders the compile searches (`-I`, `-isystem`, `-iquote`, `-idirafter`), guest paths.
    pub searched: AVec<'a, &'b str>,
}

/// One `ar` append: the archive's guest path and the members' guest paths, in order.
#[derive(Debug)]
pub struct Append<'a, 'b> {
    pub request: u32,
    pub archive: &'b str,
    /// `r` (replace a member of the same path) rather than `q` (append).
    pub replace: bool,
    pub members: AVec<'a, &'b str>,
}

/// A recorded spawn, typed.
#[derive(Debug)]
pub enum Request<'a, 'b> {
    /// `cc -c`: one object, one task.
    Object(Compile<'a, 'b>),
    /// `ar q`/`ar r`: members put in an archive; the archive is one task.
    Archive(Append<'a, 'b>),
    /// A process tree (`sh configure`, `make`, `cmake`, `perl`): its grain is the whole tree,
    /// one task for all of a script's tree requests in order (tikv-jemalloc-sys).
    Tree { request: u32 },
    /// A probe answered at the script's run (`-E`, `-?`, `--version`, `ar s`, `xcrun`): never
    /// repeated by `run`. A flag check is an [`Request::Object`] nothing archives.
    Probe { request: u32 },
}

/// Why a recorded request is refused by name (rule 3: never guessed past).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RequestWhy {
    /// The text holds no program.
    Empty,
    /// An MSVC (`cl.exe`) form (`-Fo`, `-nologo`, `/Gy`): the script took the compiler for
    /// MSVC because a probe was answered wrongly (`cc -?` must fail: facts.rs). Its record must
    /// be made again with the probes answered as clang answers them.
    ClStyle,
    /// A flag no compile of this module's table takes (`ADMITTED`), or one it refuses
    /// (`REFUSED`: a plugin, a profile read, a temp file kept, a second config).
    UnknownFlag,
    /// A flag whose value is empty (`-mmacosx-version-min=`: the script's environment lacked
    /// `MACOSX_DEPLOYMENT_TARGET`).
    EmptyValue,
    /// A flag that takes a value is the last argument.
    NoValue,
    /// An absolute path outside every guest root (`/package`, `OUT_DIR`, a dependency's
    /// `OUT_DIR`, the guest sysroot), or one that climbs with `..`, or a relative path.
    HostPath,
    /// The object is not under the guest `OUT_DIR`.
    OutputOutside,
    /// Two sources in one compile.
    TwoSources,
    /// A source whose extension names no language this module compiles.
    UnknownSource,
    /// `ar` with an operation other than `q`, `r` or `s` (`t`, `x`, `d`, ...).
    ArOperation,
}

impl core::fmt::Display for RequestWhy {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            RequestWhy::Empty => "it names no program",
            RequestWhy::ClStyle => "it is an MSVC (`cl.exe`) form: a probe was answered as MSVC answers it, and the script's record must be made again",
            RequestWhy::UnknownFlag => "the C tasks take no such flag (c_tasks.rs `ADMITTED`, `REFUSED`)",
            RequestWhy::EmptyValue => "its value is empty, and only a define (`-DNAME=`) may be",
            RequestWhy::NoValue => "it takes a value and is the last argument",
            RequestWhy::HostPath => {
                "it is a path under no root the compile is given (the package, OUT_DIR, a dependency's OUT_DIR, the sysroot), a relative path, or one that climbs with `..`"
            }
            RequestWhy::OutputOutside => "the object is not under the guest OUT_DIR",
            RequestWhy::TwoSources => "it is a second source of one compile",
            RequestWhy::UnknownSource => "no language this module compiles has its extension",
            RequestWhy::ArOperation => "it is an `ar` operation other than `q`, `r` or `s`",
        })
    }
}

/// A request refused: its index, the argument at fault (its index in the argv) and why.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RequestError {
    pub request: u32,
    pub arg: u32,
    pub why: RequestWhy,
}

/// A refused request in words (rule 19): which request, its argv joined by spaces with the
/// argument at fault between `>>` and `<<`, and why. `text` is the request's recorded text
/// (its argv joined by NUL, [`OutputsRef::c_requests`]).
pub struct RefusedRequest<'t> {
    pub error: RequestError,
    pub text: &'t str,
}

impl core::fmt::Display for RefusedRequest<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let RequestError { request, arg, why } = self.error;
        let fault = self.text.split('\0').nth(arg as usize).unwrap_or("");
        write!(f, "request {request}, argument {arg} `{fault}`: {why}: ")?;
        for (at, part) in self.text.split('\0').enumerate() {
            if at > 0 {
                f.write_str(" ")?;
            }
            if at == arg as usize {
                write!(f, ">>{part}<<")?;
            } else {
                f.write_str(part)?;
            }
        }
        Ok(())
    }
}

/// The recorded text of `error`'s request; empty when the record holds no such request.
fn request_text<'b>(outputs: &OutputsRef<'b>, error: &RequestError) -> &'b str {
    outputs.c_requests().nth(error.request as usize).unwrap_or("")
}

/// Where a guest path is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GuestRoot<'b> {
    Package,
    Out,
    /// A `links` dependency's tree, named by its whole spelling: its guest `OUT_DIR`
    /// (`<builds>/<name>/out`, `DEP_*_INCLUDE`) or the tree it was served
    /// (`<deps>/<links>`, dep_paths.rs `DepMount::Tree`).
    Dep(&'b str),
    Sysroot,
}

/// The guest spellings of the roots a request may name: what the script was shown.
#[derive(Clone, Copy, Debug)]
pub struct GuestRoots<'g> {
    /// The package's mount (`/package` for a dependency, D96).
    pub package: &'g str,
    /// The script's guest `OUT_DIR` ([`OutputsRef::out_dir`]).
    pub out: &'g str,
    /// The folder every crate's build folder is under (`/target/debug/build`): a dependency's
    /// `OUT_DIR` is `<builds>/<name>/out`.
    pub builds: &'g str,
    /// Where a reading script sees each `links` producer's served tree
    /// (dep_paths.rs [`crate::dep_paths::DEPS_ROOT`]): `<deps>/<links>`.
    pub deps: &'g str,
    /// The guest sysroot the script's `SDKROOT` names (a fixed spelling, never the host's).
    pub sysroot: &'g str,
}

impl<'g> GuestRoots<'g> {
    /// The root `path` is under, or `None` (a host path, a relative one, or one with `..`).
    pub fn root_of<'b>(&self, path: &'b str) -> Option<GuestRoot<'b>> {
        if !path.starts_with('/') || path.split('/').any(|part| part == "..") {
            return None;
        }
        if under(path, self.out) {
            return Some(GuestRoot::Out);
        }
        if under(path, self.package) {
            return Some(GuestRoot::Package);
        }
        if under(path, self.sysroot) {
            return Some(GuestRoot::Sysroot);
        }
        // `<deps>/<links>`: one plain part names the producer (no two packages of a graph
        // share a `links` value, the cargo book's "links").
        if let Some(rest) = path.strip_prefix(self.deps).and_then(|rest| rest.strip_prefix('/')) {
            let links = rest.split('/').next().unwrap_or("");
            if links.is_empty() || links == "." {
                return None;
            }
            return Some(GuestRoot::Dep(&path[..self.deps.len() + 1 + links.len()]));
        }
        let rest = path.strip_prefix(self.builds)?.strip_prefix('/')?;
        let (name, after) = rest.split_once('/')?;
        let after_out = after.strip_prefix("out")?;
        if name.is_empty() || !(after_out.is_empty() || after_out.starts_with('/')) {
            return None;
        }
        let end = self.builds.len() + 1 + name.len() + 1 + "out".len();
        Some(GuestRoot::Dep(&path[..end]))
    }
}

/// Every `links` dependency's tree ([`GuestRoot::Dep`]) the record's deferred requests name, in
/// an argument (whole, after a flag's letters `-I/..`, or after `=`, `:` or `,`) or in a process
/// tree's folder or environment: what the runner must lay before any of them runs. Sorted,
/// each once.
pub fn dep_roots<'a, 'b>(outputs: &OutputsRef<'b>, roots: &GuestRoots<'_>, alloc: Alloc<'a>) -> AVec<'a, &'b str> {
    let mut found: AVec<'a, &'b str> = Vec::new_in(alloc);
    let scan = |text: &'b str, found: &mut AVec<'a, &'b str>| {
        for piece in text.split(['\0', ' ', '\t', '\n', '=', ':', ',', '"', '\'']) {
            let path = match piece.find('/') {
                Some(0) => piece,
                Some(at) if piece.starts_with('-') && !piece[..at].contains('/') => &piece[at..],
                _ => continue,
            };
            if let Some(GuestRoot::Dep(root)) = roots.root_of(path) {
                found.push(root);
            }
        }
    };
    for text in outputs.c_requests() {
        scan(text, &mut found);
    }
    for tree in outputs.c_trees() {
        scan(tree.cwd, &mut found);
        for (_, value) in tree.env() {
            scan(value, &mut found);
        }
    }
    found.sort_unstable();
    found.dedup();
    found
}

/// Whether `path` is `root` or under it.
fn under(path: &str, root: &str) -> bool {
    path == root || path.strip_prefix(root).is_some_and(|rest| rest.starts_with('/'))
}

/// The last component of a spawned program.
fn name_of(program: &str) -> &str {
    program.rsplit('/').next().unwrap_or(program)
}

/// Flags that take their value as the next argument; those marked `true` name a path.
const SEPARATE: &[(&str, bool)] = &[
    ("-I", true),
    ("-isystem", true),
    ("-iquote", true),
    ("-idirafter", true),
    ("-include", true),
    ("-imacros", true),
    ("-isysroot", true),
    ("-MF", true),
    ("-D", false),
    ("-U", false),
    ("-x", false),
    ("-MT", false),
    ("-MQ", false),
    ("-arch", false),
    ("-target", false),
    // `--param ssp-buffer-size=4` (aws-lc-sys): a codegen tunable, never a path. Missing here,
    // its value was taken for a source and rooted (`/package/ssp-buffer-size=4`).
    ("--param", false),
];

/// Flags that search a folder: the folder's listing is in the object's key.
const SEARCHING: &[&str] = &["-I", "-isystem", "-iquote", "-idirafter"];

/// Flags whose joined value is a path (`-I/x`, `-include/x`; `--include=/x`, clang's spelling
/// of `-include` that aws-lc-sys passes for its prefix headers).
const JOINED_PATH: &[&str] = &["--include=", "-isystem", "-iquote", "-idirafter", "-include", "-imacros", "-isysroot", "-MF", "-I"];

/// Prefixes of the flags a compile may carry. Anything else is [`RequestWhy::UnknownFlag`].
const ADMITTED: &[&str] = &[
    "-I", "-D", "-U", "-O", "-g", "-f", "-W", "-w", "-m", "-std=", "-x", "--target=", "-target", "-isystem", "-iquote",
    "-idirafter", "-include", "-imacros", "-isysroot", "-MD", "-MMD", "-MP", "-MF", "-MT", "-MQ", "-pthread", "-pedantic",
    "-ansi", "-arch", "-nostdinc", "-no-canonical-prefixes", "-Qunused-arguments", "--include=", "--param",
];

/// Flags refused even under an admitted prefix: each reads or writes outside the task's roots,
/// loads code, or takes a second configuration.
const REFUSED: &[&str] = &[
    "-fplugin", "-fpass-plugin", "-fprofile-use", "-fprofile-instr-use", "-fprofile-sample-use", "-fmodules-cache-path",
    "-fcrash-diagnostics-dir", "-ftime-trace", "-fsave-optimization-record", "-mllvm",
];

/// Arguments only MSVC's `cl.exe` takes.
const CL_ONLY: &[&str] = &["-nologo", "-Brepro", "-Z7", "-Zi", "-EHsc", "-GS-", "-Gy"];

/// The arguments that ask for something other than an object: a probe.
const NOT_AN_OBJECT: &[&str] = &["-E", "-S", "-M", "-MM", "-?", "--version", "-dumpversion", "-dumpfullversion", "-dumpmachine", "-###", "-v", "--help"];

/// Reads the recorded spawn `text` (argv joined by NUL), the record's `request`-th, which saw
/// the `OUT_DIR` state `state`.
pub fn read<'a, 'b>(request: u32, state: u32, text: &'b str, roots: &GuestRoots<'_>, alloc: Alloc<'a>) -> Result<Request<'a, 'b>, RequestError> {
    let refuse = |arg: usize, why: RequestWhy| RequestError { request, arg: arg as u32, why };
    let mut argv: AVec<'a, &'b str> = Vec::new_in(alloc);
    argv.extend(text.split('\0'));
    let Some(program) = argv.first().copied().filter(|p| !p.is_empty()) else {
        return Err(refuse(0, RequestWhy::Empty));
    };
    match name_of(program) {
        "cc" | "clang" | "gcc" => compile(request, state, Driver::Clang, &argv, roots, alloc),
        "c++" | "clang++" | "g++" => compile(request, state, Driver::ClangXx, &argv, roots, alloc),
        "ar" | "llvm-ar" => append(request, &argv, roots, alloc),
        _ if is_tree(program) => Ok(Request::Tree { request }),
        _ => Ok(Request::Probe { request }),
    }
}

/// Whether a deferred spawn of `program` is a process-tree request ([`Request::Tree`]): its
/// grain is the whole tree it starts (c_tasks_tree.rs). The one home of the names: the script
/// sandbox keeps a tree request's folder and environment by it (outputs.rs `CTree`).
pub fn is_tree(program: &str) -> bool {
    matches!(name_of(program), "sh" | "make" | "gmake" | "cmake" | "perl")
}

/// A deferred spawn's argv as its record holds it: joined by NUL, each RELATIVE path argument
/// made absolute against `cwd`, the guest folder the spawn ran in (the package's mount, D96,
/// unless the script named another). clang and `ar` resolve a relative path against their
/// folder, and the request runs later in another one (a task folder), so the record names what
/// the script meant (hidapi-rusb's `-I etc/hidapi/hidapi ... -c etc/hidapi/mac/hid.c`, cc-rs's
/// `.file()` as given): a read is a guest path from the record on (rule 71's spirit: no
/// spelling that depends on where a step runs), and [`read`] refuses any path still relative.
///
/// Which arguments are paths is [`read`]'s own tables' answer (one home, rule 30): for a
/// compile, each positional argument, `-o`'s value, a [`SEPARATE`] flag's value marked a path
/// and a [`JOINED_PATH`] flag's joined value; for `ar`, every argument after the operation.
/// Any other program is kept as spelled. A source named relatively has the guest path as its
/// `__FILE__` (`/package/etc/hidapi/mac/hid.c`), as one named absolutely already has.
pub fn rooted(argv: &[&str], cwd: &str, out: &mut dyn core::fmt::Write) -> core::fmt::Result {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Program {
        Compile,
        Archive,
        Other,
    }
    let program = match argv.first().map(|p| name_of(p)) {
        Some("cc" | "clang" | "gcc" | "c++" | "clang++" | "g++") => Program::Compile,
        Some("ar" | "llvm-ar") => Program::Archive,
        _ => Program::Other,
    };
    // `Some(names_path)`: the argument before was a flag whose value is this one.
    let mut value_of: Option<bool> = None;
    let mut positional_only = false;
    for (at, arg) in argv.iter().copied().enumerate() {
        if at > 0 {
            out.write_char('\0')?;
        }
        // The argument as a flag's spelling and a path to root, or `None` to keep it whole.
        let path: Option<(&str, &str)> = match program {
            _ if at == 0 => None,
            Program::Other => None,
            Program::Archive => (at >= 2 && !arg.starts_with('-')).then_some(("", arg)),
            Program::Compile => match value_of.take() {
                Some(names_path) => names_path.then_some(("", arg)),
                None if positional_only || !arg.starts_with('-') => Some(("", arg)),
                None if arg == "--" => {
                    positional_only = true;
                    None
                }
                None if arg == "-o" => {
                    value_of = Some(true);
                    None
                }
                None => {
                    if let Some(&(_, names_path)) = SEPARATE.iter().find(|(flag, _)| *flag == arg) {
                        value_of = Some(names_path);
                        None
                    } else if let Some(value) = arg.strip_prefix("-o") {
                        Some(("-o", value))
                    } else {
                        JOINED_PATH
                            .iter()
                            .find(|flag| arg.len() > flag.len() && arg.starts_with(**flag) && !arg[flag.len()..].starts_with('-'))
                            .map(|flag| (*flag, &arg[flag.len()..]))
                    }
                }
            },
        };
        match path {
            Some((flag, path)) if !path.is_empty() && !path.starts_with('/') => {
                out.write_str(flag)?;
                out.write_str(cwd)?;
                let mut rest = path;
                while let Some(after) = rest.strip_prefix("./") {
                    rest = after.trim_start_matches('/');
                }
                if !rest.is_empty() && rest != "." {
                    out.write_char('/')?;
                    out.write_str(rest)?;
                }
            }
            _ => out.write_str(arg)?,
        }
    }
    Ok(())
}

fn compile<'a, 'b>(request: u32, state: u32, driver: Driver, argv: &[&'b str], roots: &GuestRoots<'_>, alloc: Alloc<'a>) -> Result<Request<'a, 'b>, RequestError> {
    let refuse = |arg: usize, why: RequestWhy| RequestError { request, arg: arg as u32, why };
    let path = |at: usize, text: &'b str| -> Result<GuestRoot<'b>, RequestError> { roots.root_of(text).ok_or(refuse(at, RequestWhy::HostPath)) };
    let (mut object, mut output, mut source, mut lang) = (false, None, None, None);
    let mut args: AVec<'a, &'b str> = Vec::new_in(alloc);
    let mut searched: AVec<'a, &'b str> = Vec::new_in(alloc);
    let mut at = 1;
    let mut positional_only = false;
    while at < argv.len() {
        let arg = argv[at];
        if positional_only || !arg.starts_with('-') {
            // `/Gy`, `/Wall`: a cl.exe flag is one component with no other slash.
            if arg.starts_with('/') && !arg[1..].contains('/') {
                return Err(refuse(at, RequestWhy::ClStyle));
            }
            path(at, arg)?;
            if source.replace(arg).is_some() {
                return Err(refuse(at, RequestWhy::TwoSources));
            }
            at += 1;
            continue;
        }
        if CL_ONLY.contains(&arg) || arg.starts_with("-Fo") || arg.starts_with("-Fe") {
            return Err(refuse(at, RequestWhy::ClStyle));
        }
        if NOT_AN_OBJECT.contains(&arg) {
            return Ok(Request::Probe { request });
        }
        match arg {
            "--" => positional_only = true,
            "-c" => object = true,
            "-o" => {
                let value = *argv.get(at + 1).ok_or(refuse(at, RequestWhy::NoValue))?;
                output = Some((at + 1, value));
                at += 1;
            }
            _ if arg.starts_with("-o") => output = Some((at, &arg[2..])),
            _ => {
                if REFUSED.iter().any(|refused| arg.starts_with(refused)) || !ADMITTED.iter().any(|prefix| arg.starts_with(prefix)) {
                    return Err(refuse(at, RequestWhy::UnknownFlag));
                }
                // The value is what follows the flag's first `=`. An empty define is C
                // (`-DPRINTF_FORMAT(a, b)=`, libusb1-sys: the macro expands to nothing), and a
                // value may end in `=` (`-ffile-prefix-map=/package=`, aws-lc-sys: the package
                // maps to nothing); an empty value of any other flag is a missing input
                // (`-mmacosx-version-min=`).
                if !arg.starts_with("-D") && arg.split_once('=').is_some_and(|(_, value)| value.is_empty()) {
                    return Err(refuse(at, RequestWhy::EmptyValue));
                }
                if let Some(&(flag, names_path)) = SEPARATE.iter().find(|(flag, _)| *flag == arg) {
                    let value = *argv.get(at + 1).ok_or(refuse(at, RequestWhy::NoValue))?;
                    if names_path {
                        path(at + 1, value)?;
                    }
                    if SEARCHING.contains(&flag) {
                        searched.push(value);
                    }
                    if flag == "-x" {
                        lang = Some((at + 1, value));
                    }
                    args.push(arg);
                    args.push(value);
                    at += 2;
                    continue;
                }
                if let Some(flag) = JOINED_PATH.iter().find(|flag| arg.len() > flag.len() && arg.starts_with(**flag)) {
                    let value = &arg[flag.len()..];
                    // `-include-pch`, `-isystem-after`: a longer flag, not a joined path.
                    if value.starts_with('/') {
                        path(at, value)?;
                        if SEARCHING.contains(flag) {
                            searched.push(value);
                        }
                    }
                }
                if let Some(value) = arg.strip_prefix("-x") {
                    lang = Some((at, value));
                }
                // Each `=`-separated absolute part (`-ffile-prefix-map=/a=/b`) is a guest path.
                for part in arg.split('=').skip(1) {
                    if part.starts_with('/') {
                        path(at, part)?;
                    }
                }
                args.push(arg);
            }
        }
        at += 1;
    }
    let (Some(source), true) = (source, object) else {
        // No `-c` or no source: a link or a probe, never an object task.
        return Ok(Request::Probe { request });
    };
    let source_at = argv.iter().position(|a| *a == source).unwrap_or(0);
    let Some((output_at, output)) = output else {
        return Err(refuse(source_at, RequestWhy::OutputOutside));
    };
    if path(output_at, output)? != GuestRoot::Out {
        return Err(refuse(output_at, RequestWhy::OutputOutside));
    }
    let lang = match lang {
        Some((_, "c")) => Lang::C,
        Some((_, "c++")) => Lang::Cxx,
        Some((_, "assembler-with-cpp")) => Lang::AsmCpp,
        Some((_, "assembler")) => Lang::Asm,
        Some((at, _)) => return Err(refuse(at, RequestWhy::UnknownSource)),
        None => match source.rsplit_once('.').map(|(_, ext)| ext) {
            Some("c") => Lang::C,
            Some("cc" | "cpp" | "cxx" | "C" | "c++") => Lang::Cxx,
            Some("S") => Lang::AsmCpp,
            Some("s") => Lang::Asm,
            _ => return Err(refuse(source_at, RequestWhy::UnknownSource)),
        },
    };
    Ok(Request::Object(Compile { request, state, driver, lang, source, output, args, searched }))
}

fn append<'a, 'b>(request: u32, argv: &[&'b str], roots: &GuestRoots<'_>, alloc: Alloc<'a>) -> Result<Request<'a, 'b>, RequestError> {
    let refuse = |arg: usize, why: RequestWhy| RequestError { request, arg: arg as u32, why };
    let Some(&op) = argv.get(1) else { return Err(refuse(0, RequestWhy::ArOperation)) };
    let op = op.strip_prefix('-').unwrap_or(op);
    let (mut append, mut replace) = (false, false);
    for letter in op.chars() {
        match letter {
            'q' => append = true,
            'r' => replace = true,
            // `c` (create quietly), `s` (an index: the linker indexes members itself), `D` and
            // `U` (blaze's writer is deterministic either way), `u` (only newer: all are new).
            'c' | 's' | 'D' | 'U' | 'u' => {}
            _ => return Err(refuse(1, RequestWhy::ArOperation)),
        }
    }
    if !append && !replace {
        // `ar s`: an index alone; nothing to make.
        return Ok(Request::Probe { request });
    }
    let Some(archive) = argv.get(2).copied() else { return Err(refuse(1, RequestWhy::NoValue)) };
    if roots.root_of(archive) != Some(GuestRoot::Out) {
        return Err(refuse(2, RequestWhy::OutputOutside));
    }
    let mut members: AVec<'a, &'b str> = Vec::new_in(alloc);
    for (at, member) in argv.iter().enumerate().skip(3) {
        if roots.root_of(member) != Some(GuestRoot::Out) {
            return Err(refuse(at, RequestWhy::HostPath));
        }
        members.push(*member);
    }
    Ok(Request::Archive(Append { request, archive, replace: replace && !append, members }))
}

// ---------------------------------------------------------------------------------------------
// The plan

/// One archive to make: its name relative to the guest `OUT_DIR` (the name the record's file
/// takes, natives.rs `names_file`), and its members, each a unit of [`Plan::units`], in order.
#[derive(Debug)]
pub struct ArchivePlan<'a, 'b> {
    pub path: &'b str,
    pub name: &'b str,
    pub members: AVec<'a, u32>,
}

/// What a script's requests come to.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Counts {
    /// Every request of the record.
    pub requests: u32,
    /// Object compiles an archive asked for: tasks.
    pub objects: u32,
    /// Archives asked for: tasks.
    pub archives: u32,
    /// Compiles an `ar` request names whose archive was not asked for (a test library).
    pub unused: u32,
    /// Compiles no `ar` request names: the script's ASSUMPTIONS ([`Plan::units`] after
    /// `planned`), each a task.
    pub assumed: u32,
    /// Probes answered at the script's run.
    pub probes: u32,
    /// Process-tree requests (one task for them all).
    pub trees: u32,
}

/// The tasks one crate's demanded archives need, and its assumptions.
///
/// AN ASSUMPTION is a deferred compile no `ar` request names: the script was answered "exit 0"
/// and may have branched on it (aws-lc-sys's `compiler_check`: `c11`, `stdalign_check`,
/// `builtin_swap_check`, `neon_sha3_check`, `apple_core_services_check`). `run` and `test`
/// discharge each once (a check leaves a proof, rule 74): it is compiled or found by key like
/// any object, and a failure is [`TaskError::Assumed`]. An assumed FAILURE cannot be said
/// today: a check expected to fail on this target is answered success and the discharge
/// reports the wrong branch as an error; the end state is a pinned answer per (clang, target,
/// source digest) for compile-only checks, as pins/answers/clang.json holds for flag checks.
#[derive(Debug)]
pub struct Plan<'a, 'b> {
    /// The demanded archives' objects (the first `planned`), then the assumptions.
    pub units: AVec<'a, Compile<'a, 'b>>,
    pub planned: u32,
    pub archives: AVec<'a, ArchivePlan<'a, 'b>>,
    /// The process-tree requests, in spawn order: one task.
    pub trees: AVec<'a, u32>,
    pub counts: Counts,
}

/// Why a crate's requests make no plan.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PlanError<'b> {
    /// More requests than [`MOST_REQUESTS`].
    TooMany { requests: usize },
    /// A request refused, with its recorded text (its argv joined by NUL) to name it by.
    Request { error: RequestError, text: &'b str },
    /// A demanded archive's member that no compile wrote.
    MemberNotCompiled { archive: &'b str, member: &'b str },
    /// A member two compiles wrote.
    TwoWriters { member: &'b str },
}

/// The archives the script's own link lines ask for: for each `rustc-link-lib` of kind
/// `static` or no kind (a dylib a found archive answers, natives.rs `take`), each of its
/// native search folders under the guest `OUT_DIR`, `lib<NAME>.a` (or `NAME` with
/// `+verbatim`) there, relative to `OUT_DIR`. The caller adds what a crate's `#[link]`
/// declarations ask for the same way.
pub fn script_demand<'a, 'b>(outputs: &OutputsRef<'b>, alloc: Alloc<'a>) -> AVec<'a, AString<'a>> {
    link_demand(outputs, &mut outputs.link_libs(), alloc)
}

/// [`script_demand`] over the link lines `lines` (`[KIND[:MODS]=]NAME`, as `rustc -l` reads
/// them): the script's own, then a crate's `#[link]` declarations written the same way.
pub fn link_demand<'a, 'l>(outputs: &OutputsRef<'_>, lines: &mut dyn Iterator<Item = &'l str>, alloc: Alloc<'a>) -> AVec<'a, AString<'a>> {
    let out = outputs.out_dir();
    let mut dirs: AVec<'a, &str> = Vec::new_in(alloc);
    for (kind, path) in outputs.link_search() {
        if matches!(kind, SearchKind::Native | SearchKind::All | SearchKind::Unspecified) {
            if path == out {
                dirs.push("");
            } else if let Some(rest) = path.strip_prefix(out).and_then(|r| r.strip_prefix('/')) {
                dirs.push(rest.trim_end_matches('/'));
            }
        }
    }
    let mut demand: AVec<'a, AString<'a>> = Vec::new_in(alloc);
    for line in lines {
        let (head, name) = line.split_once('=').map_or((None, line), |(h, n)| (Some(h), n));
        let kind = head.map(|h| h.split_once(':').map_or(h, |(k, _)| k));
        if !matches!(kind, None | Some("static") | Some("dylib")) {
            continue;
        }
        let verbatim = head.is_some_and(|h| h.split_once(':').is_some_and(|(_, mods)| mods.split(',').any(|m| m == "+verbatim")));
        for dir in dirs.iter() {
            let mut file = String::new_in(alloc);
            if !dir.is_empty() {
                file.push_str(dir);
                file.push('/');
            }
            if verbatim {
                file.push_str(name);
            } else {
                file.push_str("lib");
                file.push_str(name);
                file.push_str(".a");
            }
            if !demand.iter().any(|d| d.as_str() == file.as_str()) {
                demand.push(file);
            }
        }
    }
    demand
}

/// The plan for the archives `demand` names (relative to the guest `OUT_DIR`). A demanded name
/// no `ar` request makes is left out: the script made that file itself, or a process tree
/// does, or the link refuses it by name (natives.rs `StaticNotFound`).
pub fn plan<'a, 'b>(outputs: &OutputsRef<'b>, roots: &GuestRoots<'_>, demand: &[&str], alloc: Alloc<'a>) -> Result<Plan<'a, 'b>, PlanError<'b>> {
    let mut counts = Counts::default();
    // Every compile by its output: a well-formed one, or one refused (an error only if an
    // archive asks for its object: a flag check the script tried and dropped is no task).
    let mut compiles: AVec<'a, (&'b str, Result<Option<Compile<'a, 'b>>, RequestError>)> = Vec::new_in(alloc);
    let mut appends: AVec<'a, Append<'a, 'b>> = Vec::new_in(alloc);
    let mut trees: AVec<'a, u32> = Vec::new_in(alloc);
    for ((at, text), state) in outputs.c_requests().enumerate().zip(outputs.c_request_states()) {
        if at >= MOST_REQUESTS {
            return Err(PlanError::TooMany { requests: outputs.c_requests().count() });
        }
        counts.requests += 1;
        match read(at as u32, state, text, roots, alloc) {
            Ok(Request::Object(compile)) => compiles.push((compile.output, Ok(Some(compile)))),
            Ok(Request::Archive(append)) => appends.push(append),
            Ok(Request::Tree { request }) => {
                counts.trees += 1;
                trees.push(request);
            }
            Ok(Request::Probe { .. }) => counts.probes += 1,
            Err(error) => match refused_output(text) {
                Some(output) => compiles.push((output, Err(error))),
                None => counts.probes += 1,
            },
        }
    }
    let out = outputs.out_dir();
    let mut units: AVec<'a, Compile<'a, 'b>> = Vec::new_in(alloc);
    let mut taken: AVec<'a, Option<u32>> = Vec::new_in(alloc);
    taken.resize(compiles.len(), None);
    let mut archives: AVec<'a, ArchivePlan<'a, 'b>> = Vec::new_in(alloc);
    for wanted in demand {
        // Every append to this archive, in order.
        let mut members: AVec<'a, &'b str> = Vec::new_in(alloc);
        let mut path = None;
        for append in appends.iter() {
            let name = append.archive.get(out.len() + 1..).unwrap_or("");
            if name != *wanted {
                continue;
            }
            path = Some((append.archive, name));
            for member in append.members.iter().copied() {
                match members.iter().position(|m| *m == member) {
                    Some(at) if append.replace => members[at] = member,
                    _ => members.push(member),
                }
            }
        }
        let Some((path, name)) = path else { continue };
        let mut indices: AVec<'a, u32> = Vec::new_in(alloc);
        for member in members.iter().copied() {
            let mut writers = compiles.iter().enumerate().filter(|(_, (output, _))| *output == member);
            let Some((at, _)) = writers.next() else {
                return Err(PlanError::MemberNotCompiled { archive: path, member });
            };
            if writers.next().is_some() {
                return Err(PlanError::TwoWriters { member });
            }
            let unit = match taken[at] {
                Some(unit) => unit,
                None => {
                    let compile = match &mut compiles[at].1 {
                        Err(error) => return Err(PlanError::Request { error: *error, text: request_text(outputs, error) }),
                        Ok(compile) => compile.take().expect("c_tasks plan: a compile is taken once, `taken` says when"),
                    };
                    let unit = units.len() as u32;
                    units.push(compile);
                    taken[at] = Some(unit);
                    unit
                }
            };
            indices.push(unit);
        }
        archives.push(ArchivePlan { path, name, members: indices });
    }
    let planned = units.len() as u32;
    // The assumptions: compiles no `ar` request names, in spawn order.
    let mut named: AVec<'a, &'b str> = Vec::new_in(alloc);
    for append in appends.iter() {
        named.extend(append.members.iter().copied());
    }
    named.sort_unstable();
    for (output, compile) in compiles.iter_mut() {
        if named.binary_search(output).is_ok() {
            continue;
        }
        match compile {
            Err(error) => return Err(PlanError::Request { error: *error, text: request_text(outputs, error) }),
            Ok(compile) => units.push(compile.take().expect("c_tasks plan: an unnamed compile is no member, so none took it")),
        }
    }
    counts.objects = planned;
    counts.archives = archives.len() as u32;
    counts.assumed = units.len() as u32 - planned;
    counts.unused = (compiles.len() - units.len()) as u32;
    Ok(Plan { units, planned, archives, trees, counts })
}

/// The `-o` of a refused compile's text, if it has one: an archive may still ask for it.
fn refused_output(text: &str) -> Option<&str> {
    let mut args = text.split('\0');
    let program = args.next()?;
    if !matches!(name_of(program), "cc" | "clang" | "gcc" | "c++" | "clang++" | "g++") {
        return None;
    }
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        if arg == "-o" {
            return args.next();
        }
        if let Some(rest) = arg.strip_prefix("-Fo").or_else(|| arg.strip_prefix("-o")) {
            if !rest.is_empty() {
                return Some(rest);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------------------------
// Keys

/// The C compiler strategy (rule 78): its id is in every object's key, so objects of one never
/// answer for the other until a test holds them equal and the id is shared on purpose.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CompilerKind {
    /// The shelf's pinned `bin/clang-23` (pins/clang.json), run under the hermetic Seatbelt
    /// grant the native script runner uses (blaze-proc-macro native_script.rs).
    ShelfClang = 1,
    /// blaze-cfront (c.md D174), the pinned clang's libraries in blaze's helper; not built yet
    /// (pins/cfront.json has no target).
    Cfront = 2,
}

/// What every object's key holds of the toolchain.
#[derive(Clone, Copy, Debug)]
pub struct Toolchain<'t> {
    pub kind: CompilerKind,
    /// The compiler's identity: the shelf's digest (its folder's name), never its path.
    pub compiler: [u8; 32],
    /// The SDK's identity bytes (blaze-apple-sdk `SdkId::key`: version and build, the one
    /// identity every key holds), never its folder.
    pub sdk: &'t [u8],
    pub target: &'t str,
}

/// The object's inputs known before the work.
pub fn pre_key(unit: &Compile<'_, '_>, source: [u8; 32], toolchain: &Toolchain<'_>) -> ContentId {
    let mut hasher = Hasher::new(FORM);
    hasher
        .part(b"object pre")
        .update(&[toolchain.kind as u8])
        .part(&toolchain.compiler)
        .part(toolchain.sdk)
        .part(toolchain.target.as_bytes())
        .update(&[unit.driver as u8, unit.lang as u8])
        .update(&(unit.args.len() as u64).to_le_bytes());
    for arg in unit.args.iter() {
        hasher.part(arg.as_bytes());
    }
    hasher.part(unit.source.as_bytes()).part(&source).part(unit.output.as_bytes());
    hasher.finish()
}

/// The key of the `slot`-th path set of `pre`.
pub fn path_set_key(pre: ContentId, slot: u32) -> ContentId {
    let mut hasher = Hasher::new(FORM);
    hasher.part(b"path set").id(&pre).update(&slot.to_le_bytes());
    hasher.finish()
}

/// A path set: the guest paths an object's compile read (the source aside, which its pre key
/// holds) and the folders whose listings decide what it would find, each list sorted, in
/// the record's bytes: `BCPS1`, the counts, then each path ended by NUL.
pub const PATH_SET_MAGIC: &[u8; 5] = b"BCPS1";

/// Encodes a path set: `reads` and `listed` sorted and without repeats.
pub fn encode_path_set(reads: &[&str], listed: &[&str], out: &mut AVec<'_, u8>) {
    out.clear();
    out.extend_from_slice(PATH_SET_MAGIC);
    out.extend_from_slice(&(reads.len() as u32).to_le_bytes());
    out.extend_from_slice(&(listed.len() as u32).to_le_bytes());
    for path in reads.iter().chain(listed.iter()) {
        out.extend_from_slice(path.as_bytes());
        out.push(0);
    }
}

/// A path set read in place.
#[derive(Clone, Copy, Debug)]
pub struct PathSet<'p> {
    reads: u32,
    listed: u32,
    text: &'p [u8],
}

impl<'p> PathSet<'p> {
    /// Opens `bytes`, or `None` when they are no path set of this form.
    pub fn open(bytes: &'p [u8]) -> Option<PathSet<'p>> {
        let rest = bytes.strip_prefix(PATH_SET_MAGIC.as_slice())?;
        let reads = u32::from_le_bytes(rest.get(..4)?.try_into().ok()?);
        let listed = u32::from_le_bytes(rest.get(4..8)?.try_into().ok()?);
        let text = rest.get(8..)?;
        let set = PathSet { reads, listed, text };
        (set.paths().count() == (reads + listed) as usize && text.last().is_none_or(|b| *b == 0)).then_some(set)
    }

    fn paths(&self) -> impl Iterator<Item = &'p str> + use<'p> {
        let text = self.text;
        text.split(|b| *b == 0).filter(|p| !p.is_empty()).map(|p| core::str::from_utf8(p).unwrap_or("\u{fffd}"))
    }

    /// The files read.
    pub fn reads(&self) -> impl Iterator<Item = &'p str> + use<'p> {
        self.paths().take(self.reads as usize)
    }

    /// The folders listed.
    pub fn listed(&self) -> impl Iterator<Item = &'p str> + use<'p> {
        self.paths().skip(self.reads as usize).take(self.listed as usize)
    }
}

/// Today's facts of the guest's files: what a path set is checked against.
pub trait Today {
    /// The digest of the file at the guest path `path` now, or `None` when there is none.
    fn file(&self, path: &str) -> Result<Option<[u8; 32]>, FactError>;
    /// The digest of the names under the guest folder `path` now, every depth, sorted (names
    /// only: an edit to a file's text does not move it), or `None` when there is no folder.
    fn listing(&self, path: &str) -> Result<Option<[u8; 32]>, FactError>;
}

/// A fact of today's files could not be had (the root's owner's words are the caller's).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FactError {
    pub code: u16,
}

/// The object's key: its pre key, its path set, and today's digest of each path and listing of
/// each folder in it.
pub fn object_key(pre: ContentId, set: &PathSet<'_>, today: &dyn Today) -> Result<ContentId, FactError> {
    let mut hasher = Hasher::new(FORM);
    hasher.part(b"object").id(&pre).part(set.text);
    for path in set.reads() {
        match today.file(path)? {
            Some(digest) => hasher.update(&[1]).part(&digest),
            None => hasher.update(&[0]),
        };
    }
    for path in set.listed() {
        match today.listing(path)? {
            Some(digest) => hasher.update(&[1]).part(&digest),
            None => hasher.update(&[0]),
        };
    }
    Ok(hasher.finish())
}

/// A file in the home's file area: its bytes are named by their sha256 (scripts.rs
/// `file_area`), as a native script's stored files are.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FileRef {
    pub sha256: [u8; 32],
    pub len: u64,
}

/// An object's or archive's record: `BCOB1`, the file's sha256 and length.
pub const FILE_RECORD_MAGIC: &[u8; 5] = b"BCOB1";

pub fn encode_file_record(file: FileRef) -> [u8; 45] {
    let mut out = [0u8; 45];
    out[..5].copy_from_slice(FILE_RECORD_MAGIC);
    out[5..37].copy_from_slice(&file.sha256);
    out[37..].copy_from_slice(&file.len.to_le_bytes());
    out
}

pub fn decode_file_record(bytes: &[u8]) -> Option<FileRef> {
    let rest = bytes.strip_prefix(FILE_RECORD_MAGIC.as_slice())?;
    if rest.len() != 40 {
        return None;
    }
    Some(FileRef { sha256: rest[..32].try_into().ok()?, len: u64::from_le_bytes(rest[32..].try_into().ok()?) })
}

/// The archive's key: its name and, in order, each member's name in the archive (its file
/// name) and its object's digest and length.
pub fn archive_key(name: &str, members: &[(&str, FileRef)]) -> ContentId {
    let mut hasher = Hasher::new(FORM);
    hasher.part(b"archive").part(name.as_bytes()).update(&(members.len() as u64).to_le_bytes());
    for (member, file) in members {
        hasher.part(member.as_bytes()).part(&file.sha256).update(&file.len.to_le_bytes());
    }
    hasher.finish()
}

// ---------------------------------------------------------------------------------------------
// The archive writer

/// The `ar` global header.
pub const AR_MAGIC: &[u8; 8] = b"!<arch>\n";

/// Writes the archive of `members` (name, bytes), in order, into `out`: BSD form, every name
/// written `#1/<len>` (blaze-native ar.rs reads it), padded with NULs (at least one) so each
/// member's data starts at a multiple of 8 from the archive's start, as Apple's `libtool`
/// lays it; date, uid and gid 0, mode `644`; no symbol index (blaze-native indexes the
/// members' own symbol tables and skips one). Equal members give equal bytes.
pub fn write_archive(members: &[(&str, &[u8])], out: &mut AVec<'_, u8>) {
    out.clear();
    out.extend_from_slice(AR_MAGIC);
    for (name, bytes) in members {
        // The data starts at `out.len() + 60 + padded`; members start at even offsets.
        let header_end = out.len() + 60;
        let padded = (header_end + name.len() + 1).next_multiple_of(8) - header_end;
        let size = padded + bytes.len();
        let mut head = [b' '; 60];
        put_field(&mut head[0..16], b"#1/", padded as u64);
        put_field(&mut head[16..28], b"", 0);
        put_field(&mut head[28..34], b"", 0);
        put_field(&mut head[34..40], b"", 0);
        put_octal(&mut head[40..48], 0o644);
        put_field(&mut head[48..58], b"", size as u64);
        head[58] = b'`';
        head[59] = b'\n';
        out.extend_from_slice(&head);
        out.extend_from_slice(name.as_bytes());
        out.resize(out.len() + (padded - name.len()), 0);
        out.extend_from_slice(bytes);
        if size % 2 == 1 {
            out.push(b'\n');
        }
    }
}

/// `prefix` then `value` in decimal, left-aligned, padded with spaces.
fn put_field(field: &mut [u8], prefix: &[u8], value: u64) {
    field[..prefix.len()].copy_from_slice(prefix);
    let mut digits = [0u8; 20];
    let mut n = value;
    let mut len = 0;
    loop {
        digits[len] = b'0' + (n % 10) as u8;
        len += 1;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    for at in 0..len {
        field[prefix.len() + at] = digits[len - 1 - at];
    }
}

fn put_octal(field: &mut [u8], value: u32) {
    let mut digits = [0u8; 11];
    let mut n = value;
    let mut len = 0;
    loop {
        digits[len] = b'0' + (n % 8) as u8;
        len += 1;
        n /= 8;
        if n == 0 {
            break;
        }
    }
    for at in 0..len {
        field[at] = digits[len - 1 - at];
    }
}

// ---------------------------------------------------------------------------------------------
// The seams and the phases

/// The C compiler strategy (rule 78). One compile: the unit's cooked argv run against the
/// guest roots; the object put in the file area; the files it read (its depfile's), guest
/// paths. A compile that exits nonzero is [`Compiled::Failed`], with its first error lines.
pub trait CCompiler {
    const KIND: CompilerKind;
    fn compile<'a>(&mut self, unit: &Compile<'_, '_>, alloc: Alloc<'a>) -> Result<Compiled<'a>, Refused<'a>>;
}

/// What a compile came to.
#[derive(Debug)]
pub enum Compiled<'a> {
    Made { object: FileRef, reads: AVec<'a, AString<'a>> },
    /// The compiler said no: its status and the first [`ERROR_LINES`] lines of its standard
    /// error, its paths shown as the guest's.
    Failed { status: i32, first_lines: AString<'a> },
}

/// The compile could not be run at all (the sandbox refused it, the deadline passed, the file
/// area failed): the strategy's words.
#[derive(Debug)]
pub struct Refused<'a> {
    pub words: AString<'a>,
}

/// The records' store: a key's bytes, written once (other bytes at a held key are refused).
pub trait CStore {
    /// The bytes at `key` into `into` (cleared first); `false` when there are none.
    fn get(&mut self, key: ContentId, into: &mut AVec<'_, u8>) -> Result<bool, StoreFailed>;
    fn put(&mut self, key: ContentId, bytes: &[u8]) -> Result<(), StoreFailed>;
}

/// The store refused (the caller's words are its own).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StoreFailed;

/// The file area: bytes by sha256.
pub trait Area {
    fn put(&mut self, bytes: &[u8]) -> Result<FileRef, StoreFailed>;
    fn read(&self, file: FileRef, into: &mut AVec<'_, u8>) -> Result<(), StoreFailed>;
}

/// Why one crate's C tasks gave no archives. Each names the crate (rule 19); a failed compile
/// names its source and its first error lines.
#[derive(Debug)]
pub enum TaskError<'a, 'b> {
    Plan { package: &'b str, why: PlanError<'b> },
    /// The source the plan names is not there.
    NoSource { package: &'b str, source: &'b str },
    Compile { package: &'b str, source: &'b str, status: i32, first_lines: AString<'a> },
    /// The script assumed `source` compiles ([`Plan`]); it does not.
    Assumed { package: &'b str, source: &'b str, status: i32, first_lines: AString<'a> },
    Refused { package: &'b str, source: &'b str, words: AString<'a> },
    /// The compile read a path outside every guest root (its depfile's).
    ReadOutside { package: &'b str, source: &'b str, path: AString<'a> },
    PathSets { package: &'b str, source: &'b str },
    Fact { package: &'b str, path: &'b str, error: FactError },
    Store { package: &'b str, what: &'static str },
    /// A record at a key that is not of its form.
    Damaged { package: &'b str, key: ContentId },
}

/// One unit looked up: found by its key, or to compile (with the slot its new path set takes
/// if the compile reads a set no slot holds).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Looked {
    Hit(FileRef),
    Miss { pre: ContentId, free_slot: u32 },
}

/// Looks `unit` up: its pre key from today's source, then each recorded path set in slot
/// order, the first whose object key today holds an object.
pub fn look<'a, 'b>(package: &'b str, unit: &Compile<'_, 'b>, toolchain: &Toolchain<'_>, today: &dyn Today, store: &mut dyn CStore, alloc: Alloc<'_>) -> Result<Looked, TaskError<'a, 'b>> {
    let source = match today.file(unit.source) {
        Ok(Some(digest)) => digest,
        Ok(None) => return Err(TaskError::NoSource { package, source: unit.source }),
        Err(error) => return Err(TaskError::Fact { package, path: unit.source, error }),
    };
    let pre = pre_key(unit, source, toolchain);
    let mut held = Vec::new_in(alloc);
    let mut object = Vec::new_in(alloc);
    for slot in 0..PATH_SETS {
        let set_key = path_set_key(pre, slot);
        if !store.get(set_key, &mut held).map_err(|_| TaskError::Store { package, what: "a path set" })? {
            return Ok(Looked::Miss { pre, free_slot: slot });
        }
        let Some(set) = PathSet::open(&held) else { return Err(TaskError::Damaged { package, key: set_key }) };
        let key = object_key(pre, &set, today).map_err(|error| TaskError::Fact { package, path: unit.source, error })?;
        if store.get(key, &mut object).map_err(|_| TaskError::Store { package, what: "an object" })? {
            return decode_file_record(&object).map(Looked::Hit).ok_or(TaskError::Damaged { package, key });
        }
    }
    Err(TaskError::PathSets { package, source: unit.source })
}

/// Records a compile of `unit` that looked `Miss { pre, free_slot }`: its path set (its reads
/// and the folders it searched, sorted) put at the slot that already holds an equal set, else
/// at `free_slot`; the object's record at its key.
#[expect(clippy::too_many_arguments, reason = "each is one input of the records, named at the call")]
pub fn admit<'a, 'b>(
    package: &'b str,
    unit: &Compile<'_, 'b>,
    pre: ContentId,
    free_slot: u32,
    object: FileRef,
    reads: &[AString<'_>],
    roots: &GuestRoots<'_>,
    today: &dyn Today,
    store: &mut dyn CStore,
    alloc: Alloc<'a>,
) -> Result<(), TaskError<'a, 'b>> {
    let mut files: AVec<'_, &str> = Vec::new_in(alloc);
    let mut folders: AVec<'_, &str> = Vec::new_in(alloc);
    folders.extend(unit.searched.iter().copied());
    folders.push(parent(unit.source));
    for read in reads {
        let path = read.as_str();
        if roots.root_of(path).is_none() {
            return Err(TaskError::ReadOutside { package, source: unit.source, path: String::from_str_in(path, alloc) });
        }
        // The guest sysroot is in the toolchain's identity: its files are not listed one by one.
        if path == unit.source || roots.root_of(path) == Some(GuestRoot::Sysroot) {
            continue;
        }
        files.push(path);
        folders.push(parent(path));
    }
    files.sort_unstable();
    files.dedup();
    folders.retain(|f| roots.root_of(f).is_some_and(|r| r != GuestRoot::Sysroot));
    folders.sort_unstable();
    folders.dedup();
    let mut bytes = Vec::new_in(alloc);
    encode_path_set(&files, &folders, &mut bytes);
    let mut held = Vec::new_in(alloc);
    let mut slot = free_slot;
    for at in 0..free_slot {
        if store.get(path_set_key(pre, at), &mut held).map_err(|_| TaskError::Store { package, what: "a path set" })? && held.as_slice() == bytes.as_slice() {
            slot = at;
            break;
        }
    }
    if slot >= PATH_SETS {
        return Err(TaskError::PathSets { package, source: unit.source });
    }
    if slot == free_slot {
        store.put(path_set_key(pre, slot), &bytes).map_err(|_| TaskError::Store { package, what: "a path set" })?;
    }
    let Some(set) = PathSet::open(&bytes) else { return Err(TaskError::Damaged { package, key: path_set_key(pre, slot) }) };
    let key = object_key(pre, &set, today).map_err(|error| TaskError::Fact { package, path: unit.source, error })?;
    store.put(key, &encode_file_record(object)).map_err(|_| TaskError::Store { package, what: "an object" })
}

fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(dir, _)| if dir.is_empty() { "/" } else { dir })
}

/// One archive made or found: its name relative to the guest `OUT_DIR` and its file.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Made<'b> {
    pub name: &'b str,
    pub file: FileRef,
}

/// The archive `plan` names from its members' objects (`objects[unit]`): found at its key,
/// else written by [`write_archive`], put in the area and recorded. `ran` is told whether it
/// was made now.
pub fn archive<'a, 'b>(
    package: &'b str,
    plan: &ArchivePlan<'_, 'b>,
    units: &[Compile<'_, 'b>],
    objects: &[FileRef],
    area: &mut dyn Area,
    store: &mut dyn CStore,
    alloc: Alloc<'_>,
) -> Result<(Made<'b>, bool), TaskError<'a, 'b>> {
    let mut members: AVec<'_, (&str, FileRef)> = Vec::with_capacity_in(plan.members.len(), alloc);
    for unit in plan.members.iter() {
        let unit = *unit as usize;
        members.push((file_name(units[unit].output), objects[unit]));
    }
    let key = archive_key(plan.name, &members);
    let mut held = Vec::new_in(alloc);
    if store.get(key, &mut held).map_err(|_| TaskError::Store { package, what: "an archive" })? {
        let file = decode_file_record(&held).ok_or(TaskError::Damaged { package, key })?;
        return Ok((Made { name: plan.name, file }, false));
    }
    let mut bodies: AVec<'_, AVec<'_, u8>> = Vec::with_capacity_in(members.len(), alloc);
    for (_, file) in members.iter() {
        let mut body = Vec::new_in(alloc);
        area.read(*file, &mut body).map_err(|_| TaskError::Store { package, what: "an object's bytes" })?;
        bodies.push(body);
    }
    let mut named: AVec<'_, (&str, &[u8])> = Vec::with_capacity_in(members.len(), alloc);
    for (at, &(name, _)) in members.iter().enumerate() {
        named.push((name, bodies[at].as_slice()));
    }
    let mut bytes = Vec::new_in(alloc);
    write_archive(&named, &mut bytes);
    let file = area.put(&bytes).map_err(|_| TaskError::Store { package, what: "an archive's bytes" })?;
    store.put(key, &encode_file_record(file)).map_err(|_| TaskError::Store { package, what: "an archive" })?;
    Ok((Made { name: plan.name, file }, true))
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// What one crate's tasks came to.
#[derive(Debug)]
pub struct CrateRun<'a, 'b> {
    pub made: AVec<'a, Made<'b>>,
    pub counts: Counts,
    /// Object tasks run now (misses).
    pub objects_ran: u32,
    /// Archive tasks run now.
    pub archives_ran: u32,
}

/// A state a driver cannot lay ([`laid_state`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LayError<'b> {
    /// The record has no state `state`.
    NoState { state: u32 },
    /// `name` is the record's final file, kept in the file area, not inline: no state of
    /// today's records names one (script.rs `c_inputs` marks only inline files final).
    Unheld { state: u32, name: &'b str },
}

impl core::fmt::Display for LayError<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            LayError::NoState { state } => write!(f, "the record names OUT_DIR state {state}, which it does not hold"),
            LayError::Unheld { state, name } => write!(f, "OUT_DIR state {state}'s file {name} is neither held nor inline in the record"),
        }
    }
}

/// The `OUT_DIR` state `state` as a driver lays it for the compiles that saw it: each entry's
/// name from `OUT_DIR`, `None` for a folder, else the file's bytes as the script left them at
/// the spawn (outputs.rs, THE C INPUTS).
pub fn laid_state<'a, 'b>(outputs: &OutputsRef<'b>, state: u32, alloc: Alloc<'a>) -> Result<AVec<'a, (&'b str, Option<&'b [u8]>)>, LayError<'b>> {
    let entries = outputs.c_state(state).ok_or(LayError::NoState { state })?;
    let mut laid = Vec::new_in(alloc);
    for (name, snap) in entries {
        let bytes = match snap {
            crate::outputs::Snap::Folder => None,
            _ => Some(outputs.c_snap_bytes(name, snap).ok_or(LayError::Unheld { state, name })?),
        };
        laid.push((name, bytes));
    }
    Ok(laid)
}

/// [`FactError::code`] of a state the record does not hold, or of a final file it holds
/// neither inline nor in the file area.
pub const NO_STATE: u16 = u16::MAX - 8;

/// Today's facts of one `OUT_DIR` state, from the record alone (no file is read): a path under
/// the guest `OUT_DIR` is the state's entry, any other path `rest`'s. A held file's digest is
/// its [`crate::outputs::snap_digest`], a final inline one the same of its bytes, a final
/// stored one its sha256; a run whose objects all hit lays nothing (rule 77).
pub struct StateToday<'t, 'b> {
    pub outputs: &'t OutputsRef<'b>,
    pub state: u32,
    pub rest: &'t dyn Today,
}

impl StateToday<'_, '_> {
    /// `path` from the guest `OUT_DIR` (`""` itself), or `None` outside it.
    fn under<'p>(&self, path: &'p str) -> Option<&'p str> {
        let out = self.outputs.out_dir();
        if path == out {
            return Some("");
        }
        path.strip_prefix(out)?.strip_prefix('/')
    }
}

impl Today for StateToday<'_, '_> {
    fn file(&self, path: &str) -> Result<Option<[u8; 32]>, FactError> {
        let Some(rel) = self.under(path) else { return self.rest.file(path) };
        let mut entries = self.outputs.c_state(self.state).ok_or(FactError { code: NO_STATE })?;
        let Some((name, snap)) = entries.find(|(name, _)| *name == rel) else { return Ok(None) };
        match snap {
            crate::outputs::Snap::Folder => Ok(None),
            crate::outputs::Snap::Held(digest) => Ok(Some(digest)),
            crate::outputs::Snap::Final => match self.outputs.file(name) {
                Some(crate::outputs::OutFileRef::Inline(bytes)) => Ok(Some(crate::outputs::snap_digest(bytes))),
                Some(crate::outputs::OutFileRef::Stored(stored)) => Ok(Some(*stored.sha256)),
                None => Err(FactError { code: NO_STATE }),
            },
        }
    }

    fn listing(&self, path: &str) -> Result<Option<[u8; 32]>, FactError> {
        let Some(rel) = self.under(path) else { return self.rest.listing(path) };
        let entries = self.outputs.c_state(self.state).ok_or(FactError { code: NO_STATE })?;
        let mut hasher = Hasher::new("c_tasks state listing");
        let mut folder = rel.is_empty();
        for (name, snap) in entries {
            if name == rel {
                folder = matches!(snap, crate::outputs::Snap::Folder);
                continue;
            }
            let below = if rel.is_empty() { Some(name) } else { name.strip_prefix(rel).and_then(|r| r.strip_prefix('/')) };
            if let Some(below) = below {
                hasher.update(below.as_bytes());
                hasher.update(b"\n");
            }
        }
        Ok(folder.then(|| hasher.finish().0))
    }
}

/// Today's facts of each `OUT_DIR` state: the guest `OUT_DIR` as the state laid it, every
/// other root as it is.
pub trait Todays {
    fn of(&self, state: u32) -> &dyn Today;
}

/// The reference driver of one crate's tasks, in order: plan, look each unit up (against its
/// state's today), compile each miss with `compiler`, record it, then each archive. The
/// command's driver runs the same phases with the misses in parallel; a test holds the two
/// equal.
#[expect(clippy::too_many_arguments, reason = "each is one seam or input, named at the call")]
pub fn run_crate<'a, 'b, C: CCompiler>(
    package: &'b str,
    outputs: &OutputsRef<'b>,
    roots: &GuestRoots<'_>,
    demand: &[&str],
    toolchain: &Toolchain<'_>,
    compiler: &mut C,
    todays: &dyn Todays,
    area: &mut dyn Area,
    store: &mut dyn CStore,
    alloc: Alloc<'a>,
) -> Result<CrateRun<'a, 'b>, TaskError<'a, 'b>> {
    debug_assert!(toolchain.kind == C::KIND, "the toolchain's kind is the compiler's");
    let plan = plan(outputs, roots, demand, alloc).map_err(|why| TaskError::Plan { package, why })?;
    let mut objects: AVec<'a, FileRef> = Vec::with_capacity_in(plan.units.len(), alloc);
    let mut objects_ran = 0;
    for (at, unit) in plan.units.iter().enumerate() {
        let assumed = at as u32 >= plan.planned;
        let today = todays.of(unit.state);
        let file = match look(package, unit, toolchain, today, store, alloc)? {
            Looked::Hit(file) => file,
            Looked::Miss { pre, free_slot } => {
                objects_ran += 1;
                match compiler.compile(unit, alloc) {
                    Ok(Compiled::Made { object, reads }) => {
                        admit(package, unit, pre, free_slot, object, &reads, roots, today, store, alloc)?;
                        object
                    }
                    Ok(Compiled::Failed { status, first_lines }) if assumed => return Err(TaskError::Assumed { package, source: unit.source, status, first_lines }),
                    Ok(Compiled::Failed { status, first_lines }) => return Err(TaskError::Compile { package, source: unit.source, status, first_lines }),
                    Err(refused) => return Err(TaskError::Refused { package, source: unit.source, words: refused.words }),
                }
            }
        };
        objects.push(file);
    }
    let planned = plan.planned as usize;
    let mut made = Vec::with_capacity_in(plan.archives.len(), alloc);
    let mut archives_ran = 0;
    for archive_plan in plan.archives.iter() {
        let (one, ran) = archive(package, archive_plan, &plan.units[..planned], &objects[..planned], area, store, alloc)?;
        archives_ran += ran as u32;
        made.push(one);
    }
    Ok(CrateRun { made, counts: plan.counts, objects_ran, archives_ran })
}

/// The script's outputs as the link reads them after its tasks ran: the record's own, its
/// requests dropped (they ran: natives.rs `DeferredC` fires only on a record whose requests did
/// not), with their states and process trees' folders; each file the process tree made or
/// changed (`tree`, c_tasks_tree.rs `run_tree`) a stored file at its name, in place of the
/// script's own file of that name; each archive a stored file at its name. The caller encodes
/// it ([`crate::Outputs::encode`]) and opens it with the file area the files were put in.
pub fn built<'a>(outputs: &OutputsRef<'_>, made: &[Made<'_>], tree: &[crate::outputs::StoredFile<'_>], alloc: Alloc<'a>) -> Result<crate::Outputs<'a>, crate::outputs::FileError> {
    let mut owned = outputs.to_owned(alloc);
    owned.c_requests.clear();
    owned.c_states.clear();
    owned.c_bytes.clear();
    owned.c_trees.clear();
    for file in tree {
        let name = file.name.as_str();
        owned.files.retain(|f| f.name.as_str() != name);
        owned.stored.retain(|f| f.name.as_str() != name);
        owned.add_stored(name, file.len, file.mode, file.sha256, crate::outputs::FileLimits::DEFAULT, alloc)?;
    }
    for one in made {
        owned.add_stored(one.name, one.file.len, crate::outputs::FileMode::Plain, one.file.sha256, crate::outputs::FileLimits::DEFAULT, alloc)?;
    }
    Ok(owned)
}
