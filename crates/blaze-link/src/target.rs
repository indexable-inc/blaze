//! A target's facts for the link, and where the files of its link line are.
//!
//! [`output`] is rustc's target spec of a triple (the pinned nightly's `rustc_target::spec`) as
//! an [`Output`]: format, architecture, entry, the ELF interpreter, the Mach-O platform and the
//! stack. [`locate`] finds the files of a link line, in its order: the C runtime's start
//! objects, the sanitizer runtimes in blaze's toolchain, the libraries the crates ask for (`-l`,
//! `#[link]`), the libraries the C toolchain's driver adds, then its end objects.
//!
//! blaze never links with `cc` (docs/linker.md, "The C toolchain stays a source of files, not
//! a linker"): the platform's C toolchain is asked where its files are, never to compile or
//! link.
//!
//! - Linux: `$CC_<triple with _>`, else `$CC`, else `cc`, with `-print-search-dirs` and
//!   `-print-file-name=<file>`, all at once: distro GCC and Clang and nix's cc-wrapper answer
//!   alike. The interpreter is the one that toolchain's `cc` passes: its own libc's loader
//!   where that libc is outside the FHS directories (nix), and the glibc ABI's path otherwise.
//! - macOS: the SDK at `$SDKROOT`, else `xcrun --sdk macosx --show-sdk-path`: its `.tbd`s and
//!   frameworks, and its version from `SDKSettings.json`. The minimum OS is
//!   `$MACOSX_DEPLOYMENT_TARGET`, at least rustc's default for the triple.
//! - Linux and macOS: the directories the C driver (`$CC_<triple with _>`, else `$CC`, else
//!   `cc`) passes its linker, from the jobs `-###` prints without running them: the `-L`s and
//!   `-F`s nix's cc-wrapper adds from `NIX_LDFLAGS`, `LIBRARY_PATH`'s, the driver's own. rustc's
//!   link gets them through `cc`; a library only there (nix's libiconv on macOS) is found.
//! - Windows (MSVC): the directories of `$LIB`; without it, the MSVC tools vswhere names and
//!   the newest Windows SDK the registry names, as `find_msvc_tools` finds them.
//! - wasm32-wasip1: wasi-libc's `crt1-command.o` (`crt1-reactor.o` for a [`reactor`]) and
//!   `libc.a`, in blaze's toolchain's self-contained directory (the `sysroot` [`locate`] is
//!   given).
//! - wasm64-blaze-wasip1 (blaze's own memory64 target, the bootstrap seed's): no C runtime
//!   files. Its C runtime (`_start`, `malloc`, the libc functions std calls) is a Rust crate of
//!   blaze's `library/`, linked as any other crate of the program, so the `c` the `libc` crate
//!   asks for is provided by it, never searched for ([`Target::provided_by_rust`]).
//!
//! What the toolchain says is asked once per process for each triple and each value of the
//! environment variables read, and kept. Each file found is still hashed by the caller, so a
//! changed file relinks; a toolchain whose directory layout changed (a compiler upgraded, an
//! SDK moved) is seen only by a daemon started after the change.
//!
//! A library is searched for as rustc passes `-l` and the system linker finds it: the link's
//! own directories first, then the C driver's, then the toolchain's, and in each directory the
//! files the kind
//! allows, in the linker's order (`lib{name}.so` before `lib{name}.a` for a `dylib` on ELF;
//! `.tbd`, `.dylib`, `.so`, `.a` on Mach-O; `{name}.lib` on PE; `lib{name}.a` on wasm). A
//! GNU linker script found as a shared library (glibc's `libc.so`, Debian's `libgcc_s.so`) is
//! followed on the line by the files it names ([`crate::script_inputs`]), its `-lNAME`s
//! searched for as `dylib`s, which the engine wants there. A PE archive is followed by the
//! libraries its members ask for with `/DEFAULTLIB`, transitively (`msvcrt.lib` brings
//! `vcruntime.lib` and `ucrt.lib`), which the engine checks are there for every member it
//! takes. A Mach-O object or archive is followed by the libraries and frameworks its objects'
//! `LC_LINKER_OPTION`s ask for (`-lz`, `-framework Security`), transitively, found as ld64
//! finds them, which the engine checks are there for every object it loads. The engine
//! resolves archives wherever they are on the line, so each file is on it once.

#[cfg(test)]
mod tests;

use std::cell::RefCell;
use std::fmt::Write as _;
use std::io::Read as _;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use blaze_codegen::settings::{Sanitize, Sanitizers};
use blaze_codegen::{Arch, NativeLib, NativeLibKind};
use blaze_hash::{HashMap, HashSet};
use std::alloc::Global;
use blaze_ids::ContentId;
use parking_lot::Mutex;
use smallvec::SmallVec;

use crate::{
    Debug, Format, LinkError, Native, NativeKind, NotFound, Output, OutputKind, Platform, Refusal,
    Searched,
};

/// A file of the link line, as [`locate`] found it.
#[derive(PartialEq, Eq, Debug)]
pub struct Located {
    pub path: PathBuf,
    /// Its path as the link line has it: what errors call it, and what a linker script that
    /// names it is matched against.
    pub name: String,
    pub kind: NativeKind,
}

/// The main thread's stack where the format says it: link.exe's default, which rustc keeps,
/// and wasm-ld's `-z stack-size` as rustc passes it.
const STACK: u64 = 1 << 20;

/// The main thread's stack on `wasm64-blaze-wasip1`, the seed's target: 64 MiB of linear memory
/// for `__stack_pointer`, room for the front end's deepest recursion with margin
/// (todo/bootstrap/plan.md, "Stack in the seed"). A property of the target, so every program
/// built for it, the seed among them, links with it and no flag is needed.
const SEED_STACK: u64 = 64 << 20;

/// `LC_BUILD_VERSION`'s platform of macOS.
const MACOS: u32 = 1;

/// rustc's minimum macOS for aarch64-apple-darwin, 11.0.0, as `xxxx.yy.zz` nibbles.
const MACOS_MINIMUM: u32 = 0x000b_0000;

/// The domain a linker script's bytes are named in for the engine's reader.
const SCRIPT: &str = "blaze v2 link target script";

/// The domain a PE archive's bytes are named in for what its members ask for.
const ASKED: &str = "blaze v2 link target default libraries";

/// The domains a Mach-O object's or archive's bytes are named in for what its
/// `LC_LINKER_OPTION`s ask for: one per architecture, whose slice of a universal file is read.
const AUTOLINKED_AARCH64: &str = "blaze v2 link target autolinked libraries aarch64";
const AUTOLINKED_X86_64: &str = "blaze v2 link target autolinked libraries x86_64";

/// The domains a Mach-O library's bytes are named in for the install names it re-exports from
/// outside itself, one per architecture; the cache keys them by platform too.
const REEXPORTED_AARCH64: &str = "blaze v2 link target re-exported libraries aarch64";
const REEXPORTED_X86_64: &str = "blaze v2 link target re-exported libraries x86_64";

const LINKED: &str = "x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu, aarch64-apple-darwin, \
                      x86_64-pc-windows-msvc, wasm32-wasip1, wasm32-unknown-unknown and \
                      wasm64-blaze-wasip1";

/// The process's answers of the machine's toolchains.
static CACHE: Cache = Cache::new();

/// How `triple` links: an executable, or the shared library `shared` names (its file name and
/// exports) for a `cdylib` or proc macro. `incremental` keeps room to grow in the image, where
/// the format has room to keep: a wasm module (wasm32 and wasm64) is written whole by every
/// link and its code reached by index, so it is never incremental, and each link of it is a
/// whole layout (no patch is refused for want of room).
pub fn output(
    triple: &str,
    shared: Option<(String, Vec<String>)>,
    debug: Debug,
    incremental: bool,
) -> Result<Output, LinkError> {
    output_with(&CACHE, &System, triple, shared, debug, incremental)
}

/// The files of `output`'s link line, in its order: the C runtime's start objects, the
/// runtimes of [`Output::sanitize`]'s sanitizers, `libs` (searched in `search`, the link's
/// `-L`s, then the toolchain's directories), the libraries the toolchain's driver adds, the C
/// runtime's end objects. A linker script's files follow it, and so do the libraries a PE
/// archive's members ask for and those a Mach-O object's or archive member's
/// `LC_LINKER_OPTION`s ask for. A library not found is an error naming it (and the script,
/// object or member that asked) and every directory searched. `sysroot` is blaze's toolchain
/// (`blaze toolchain --path`), whose self-contained C runtime WASI links and whose target
/// library directory has the sanitizer runtimes. Sets [`Output::rpaths`] to where the loader
/// finds a shared runtime; set [`Output::sanitize`] first.
pub fn locate(
    output: &mut Output,
    libs: &[NativeLib],
    search: &[PathBuf],
    sysroot: &Path,
) -> Result<Vec<Located>, LinkError> {
    let located = locate_with(&CACHE, &System, output, libs, search, sysroot)?;
    output.rpaths = rpaths(Target::parse(&output.triple)?, output, sysroot)?;
    Ok(located)
}

/// The release channel in the sanitizer runtimes' names, as rustc's `CFG_RELEASE_CHANNEL`
/// names them: blaze's toolchain is a nightly.
const CHANNEL: &str = "nightly";

/// The sanitizers with a runtime: rustc's name for each, and what errors call it.
const RUNTIMES: [(Sanitizers, &str, &str); 3] = [
    (Sanitizers::ADDRESS, "asan", "AddressSanitizer"),
    (Sanitizers::THREAD, "tsan", "ThreadSanitizer"),
    (Sanitizers::MEMORY, "msan", "MemorySanitizer"),
];

/// The toolchain's target library directory, where rustc's `find_sanitizer_runtime` finds the
/// runtimes: `<sysroot>/lib/rustlib/<triple>/lib`.
fn runtime_dir(target: Target, sysroot: &Path) -> PathBuf {
    sysroot.join("lib").join("rustlib").join(target.triple()).join("lib")
}

/// The runtime of each of `output`'s sanitizers, as rustc's `add_sanitizer_libraries` links
/// it: on macOS `librustc-nightly_rt.<name>.dylib`, a shared library found by its
/// `@rpath` install name ([`rpaths`]); elsewhere `librustc-nightly_rt.<name>.a`, whole. An
/// ELF shared library gets none: the executable that loads it has the runtime. MSVC's
/// AddressSanitizer is MSVC's own runtime (`/INFERASANLIBS`), which the PE link does not
/// take, and wasm has none; MemorySanitizer is only Linux's.
fn runtimes(target: Target, output: &Output, sysroot: &Path) -> Result<SmallVec<[(PathBuf, NativeKind); 1]>, LinkError> {
    let mut out = SmallVec::new();
    let sanitizers = output.sanitize.sanitizers;
    if sanitizers.is_empty() {
        return Ok(out);
    }
    let named = RUNTIMES.iter().filter(|it| sanitizers.contains(it.0));
    // clone: a filter over three entries, copied to peek at its first two
    if let (Some(first), Some(second)) = (named.clone().next(), named.clone().nth(1)) {
        return Err(fail(
            // alloc: error path
            format!(
                "{} and {} in one build: rustc builds with one of AddressSanitizer, ThreadSanitizer \
                 and MemorySanitizer at a time",
                first.2, second.2
            ),
        ));
    }
    let executable = matches!(output.kind, OutputKind::Executable { .. });
    let triple = target.triple();
    for &(sanitizer, name, what) in named {
        match target {
            Target::Wasip1 | Target::WasmUnknown | Target::Wasm64Wasip1 => {
                // alloc: error path
                return Err(fail(format!("{what} on {triple}: rustc has no sanitizer runtime for wasm")));
            }
            Target::Msvc => {
                return Err(fail(
                    // alloc: error path
                    format!(
                        "{what} on {triple}: rustc links MSVC's own runtime there (`/INFERASANLIBS`), \
                         which blaze's PE link does not take yet"
                    ),
                ));
            }
            Target::MacOs if sanitizer == Sanitizers::MEMORY => {
                // alloc: error path
                return Err(fail(format!("{what} on {triple}: it has a runtime only on Linux and FreeBSD")));
            }
            _ => {}
        }
        if target.format() == Format::Elf && !executable {
            continue;
        }
        let (file, kind) = match target {
            // alloc: once per sanitized link, the runtime's file name
            Target::MacOs => (format!("librustc-{CHANNEL}_rt.{name}.dylib"), NativeKind::Shared { as_needed: false }),
            // alloc: once per sanitized link, the runtime's file name
            _ => (format!("librustc-{CHANNEL}_rt.{name}.a"), NativeKind::Archive { whole: true }),
        };
        let path = runtime_dir(target, sysroot).join(file);
        if !path.is_file() {
            return Err(fail(
                // alloc: error path
                format!(
                    "{what}'s runtime {} is not in blaze's toolchain: the pinned nightly's `rust-std` \
                     for {triple} ships it there",
                    path.display()
                ),
            ));
        }
        out.push((path, kind));
    }
    Ok(out)
}

/// Where the loader finds the shared runtimes [`runtimes`] put on the line, as rustc passes
/// `-rpath`: on macOS the target library directory, absolute; none elsewhere.
fn rpaths(target: Target, output: &Output, sysroot: &Path) -> Result<Vec<String>, LinkError> {
    if target != Target::MacOs || output.sanitize.sanitizers.is_empty() {
        // alloc: none, an empty `Vec`
        return Ok(Vec::new());
    }
    let dir = runtime_dir(target, sysroot);
    if !dir.is_absolute() {
        return Err(fail(
            // alloc: error path
            format!("the sanitizer runtimes' directory {} is not absolute, as an rpath must be", dir.display()),
        ));
    }
    let dir = dir
        .to_str()
        // alloc: error path
        .ok_or_else(|| fail(format!("the sanitizer runtimes' directory {} is not UTF-8", dir.display())))?;
    // alloc: once per sanitized link, the one rpath
    Ok(vec![dir.to_owned()])
}

/// The entry of a WASI reactor, which wasi-libc's `crt1-reactor.o` defines: it runs the
/// constructors, and the host calls the module's exports after it.
const REACTOR_ENTRY: &str = "_initialize";

/// How a proc macro built for wasm32-wasip1 links: a WASI reactor, as rustc links a binary
/// under `-Z wasi-exec-model=reactor`. It is entered at `_initialize` and exports its
/// `#[no_mangle]` functions (`blaze_proc_macro_expand` and the rest), which its code headers
/// name, and its memory.
pub fn reactor(debug: Debug, incremental: bool) -> Result<Output, LinkError> {
    reactor_with(&CACHE, &System, debug, incremental)
}

fn reactor_with<A: Ask>(cache: &Cache, ask: &A, debug: Debug, incremental: bool) -> Result<Output, LinkError> {
    let mut output = output_with(cache, ask, Target::Wasip1.triple(), None, debug, incremental)?;
    // alloc: once per link, the entry's name
    output.kind = OutputKind::Executable { entry: REACTOR_ENTRY.to_owned() };
    Ok(output)
}

fn output_with<A: Ask>(
    cache: &Cache,
    ask: &A,
    triple: &str,
    shared: Option<(String, Vec<String>)>,
    debug: Debug,
    incremental: bool,
) -> Result<Output, LinkError> {
    let target = Target::parse(triple)?;
    let kind = match shared {
        Some((name, exports)) => OutputKind::Shared { name, exports },
        None if target == Target::WasmUnknown => {
            return Err(fail(
                "an executable for wasm32-unknown-unknown: rustc links it `--no-entry`, its C-level \
                 symbols exported, and an executable `Output` has an entry; build it as a `cdylib`"
                    // alloc: error path
                    .to_owned(),
            ));
        }
        // alloc: once per link, the entry's name
        None => OutputKind::Executable { entry: target.entry().to_owned() },
    };
    let executable = matches!(kind, OutputKind::Executable { .. });
    let interpreter = match target {
        // clone: once per link, the path kept for the target
        Target::LinuxX86_64 | Target::LinuxAarch64 if executable => cache.found(ask, target, None)?.interpreter.clone(),
        _ => None,
    };
    let platform = match target {
        Target::MacOs => Some(Platform { platform: MACOS, minimum: minimum(ask)?, sdk: cache.found(ask, target, None)?.sdk }),
        _ => None,
    };
    Ok(Output {
        format: target.format(),
        arch: target.arch(),
        kind,
        interpreter,
        platform,
        // A wasm module keeps no room ([`crate::model::Plan::indexed_code`]): its writer writes
        // the whole module each link, so a layout that kept room would save no write, and one
        // that kept none refused every added function.
        incremental: incremental && target.format() != Format::Wasm,
        // A wasm module's debug info is its name section, which the writer always writes: its
        // functions are indices, not addresses, so DWARF line tables made for an image's
        // addresses would be wrong, and the writer refuses them. Every debug level of a wasm
        // target is that name section.
        debug: match target.format() {
            Format::Wasm => Debug::None,
            Format::Elf | Format::MachO | Format::Pe => debug,
        },
        live: None,
        tags: 0,
        stack: target.stack(),
        windows_subsystem: false,
        dev_run: None,
        exec_log: false,
        // alloc: once per link, the triple the output is for
        triple: triple.to_owned(),
        sanitize: Sanitize::default(),
        // alloc: none, an empty `Vec` until `locate` sets it
        rpaths: Vec::new(),
    })
}

fn locate_with<A: Ask>(
    cache: &Cache,
    ask: &A,
    output: &Output,
    libs: &[NativeLib],
    search: &[PathBuf],
    sysroot: &Path,
) -> Result<Vec<Located>, LinkError> {
    let target = Target::parse(&output.triple)?;
    if (output.format, output.arch) != (target.format(), target.arch()) {
        return Err(fail(
            // alloc: error path
            format!(
                "an output of {:?} for {:?} says it is for {}, which is {:?} for {:?}",
                output.format,
                output.arch,
                output.triple,
                target.format(),
                target.arch()
            ),
        ));
    }
    let runtimes = runtimes(target, output, sysroot)?;
    let found = cache.found(ask, target, Some(sysroot))?;
    let mut line =
        Line { cache, output, search, found: &found, out: Vec::with_capacity(libs.len() + 8), listings: RefCell::default() };
    for name in target.start(&output.kind)? {
        line.object(name)?;
    }
    for (path, kind) in runtimes {
        line.push(path, kind)?;
    }
    for lib in libs {
        if !lib.verbatim && target.provided_by_rust(&lib.name) {
            continue;
        }
        line.library(&lib.name, lib.kind, lib.verbatim)?;
    }
    for &(name, kind) in target.late() {
        line.library(name, kind, false)?;
    }
    for name in target.end() {
        line.object(name)?;
    }
    Ok(line.out)
}

/// The targets blaze links: those `hir_ty_codegen::v2::toolchain` compiles for whose link
/// rustc's spec says.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Target {
    LinuxX86_64,
    LinuxAarch64,
    /// aarch64-apple-darwin.
    MacOs,
    /// x86_64-pc-windows-msvc.
    Msvc,
    Wasip1,
    WasmUnknown,
    /// wasm64-blaze-wasip1: memory64, WASI preview 1 with 64-bit pointers, no threads.
    Wasm64Wasip1,
}

impl Target {
    fn parse(triple: &str) -> Result<Target, LinkError> {
        Ok(match triple {
            "x86_64-unknown-linux-gnu" => Target::LinuxX86_64,
            "aarch64-unknown-linux-gnu" => Target::LinuxAarch64,
            "aarch64-apple-darwin" => Target::MacOs,
            "x86_64-pc-windows-msvc" => Target::Msvc,
            "wasm32-wasip1" => Target::Wasip1,
            "wasm32-unknown-unknown" => Target::WasmUnknown,
            "wasm64-blaze-wasip1" => Target::Wasm64Wasip1,
            "wasm32-wasip2" => {
                return Err(fail(
                    "wasm32-wasip2: rustc links a component for it (wasm-component-ld), and \
                     blaze's wasm writer makes core modules"
                        // alloc: error path
                        .to_owned(),
                ));
            }
            // alloc: error path
            _ => return Err(fail(format!("blaze links no {triple}: it links {LINKED}"))),
        })
    }

    fn triple(self) -> &'static str {
        match self {
            Target::LinuxX86_64 => "x86_64-unknown-linux-gnu",
            Target::LinuxAarch64 => "aarch64-unknown-linux-gnu",
            Target::MacOs => "aarch64-apple-darwin",
            Target::Msvc => "x86_64-pc-windows-msvc",
            Target::Wasip1 => "wasm32-wasip1",
            Target::WasmUnknown => "wasm32-unknown-unknown",
            Target::Wasm64Wasip1 => "wasm64-blaze-wasip1",
        }
    }

    fn format(self) -> Format {
        match self {
            Target::LinuxX86_64 | Target::LinuxAarch64 => Format::Elf,
            Target::MacOs => Format::MachO,
            Target::Msvc => Format::Pe,
            Target::Wasip1 | Target::WasmUnknown | Target::Wasm64Wasip1 => Format::Wasm,
        }
    }

    fn arch(self) -> Arch {
        match self {
            Target::LinuxX86_64 | Target::Msvc => Arch::X86_64,
            Target::LinuxAarch64 | Target::MacOs => Arch::Aarch64,
            Target::Wasip1 | Target::WasmUnknown => Arch::Wasm32,
            Target::Wasm64Wasip1 => Arch::Wasm64,
        }
    }

    /// The executable's entry: the C runtime's `_start` (`Scrt1.o`, wasi-libc's
    /// `crt1-command.o`, wasm64-blaze-wasip1's Rust runtime crate, which calls Rust's
    /// `__main_void`), dyld's `main` (the writer adds Mach-O's `_`), the MSVC CRT's
    /// `mainCRTStartup`.
    fn entry(self) -> &'static str {
        match self.format() {
            Format::Elf | Format::Wasm => "_start",
            Format::MachO => "main",
            Format::Pe => "mainCRTStartup",
        }
    }

    fn stack(self) -> Option<u64> {
        if self == Target::Wasm64Wasip1 {
            return Some(SEED_STACK);
        }
        match self.format() {
            Format::Pe | Format::Wasm => Some(STACK),
            Format::Elf | Format::MachO => None,
        }
    }

    /// The environment the toolchain is found from, in [`Key::vars`]'s order. `PATH`,
    /// `LIBRARY_PATH` and `NIX_LDFLAGS` are read by the commands asked: the C driver passes its
    /// linker a directory of each, and nix's cc-wrapper the `-L`s of the last.
    fn vars(self) -> &'static [&'static str] {
        match self {
            Target::LinuxX86_64 => &["CC_x86_64_unknown_linux_gnu", "CC", "PATH", "LIBRARY_PATH", "NIX_LDFLAGS"],
            Target::LinuxAarch64 => &["CC_aarch64_unknown_linux_gnu", "CC", "PATH", "LIBRARY_PATH", "NIX_LDFLAGS"],
            Target::MacOs => {
                &["SDKROOT", "DEVELOPER_DIR", "PATH", "CC_aarch64_apple_darwin", "CC", "LIBRARY_PATH", "NIX_LDFLAGS"]
            }
            Target::Msvc => &["LIB", "ProgramFiles(x86)", "PATH"],
            Target::Wasip1 | Target::WasmUnknown | Target::Wasm64Wasip1 => &[],
        }
    }

    /// The C runtime's objects first on the line: glibc's as the system `cc` adds them for a
    /// PIE (rustc's glibc specs leave them to it), wasi-libc's command or reactor start by the
    /// entry, as rustc's `pre_link_objects_self_contained` has them (none for a `cdylib`).
    fn start(self, kind: &OutputKind) -> Result<&'static [&'static str], LinkError> {
        Ok(match (self, kind) {
            (Target::LinuxX86_64 | Target::LinuxAarch64, OutputKind::Executable { .. }) => {
                &["Scrt1.o", "crti.o", "crtbeginS.o"]
            }
            (Target::LinuxX86_64 | Target::LinuxAarch64, OutputKind::Shared { .. }) => &["crti.o", "crtbeginS.o"],
            (Target::Wasip1, OutputKind::Executable { entry }) => match entry.as_str() {
                "_start" => &["crt1-command.o"],
                REACTOR_ENTRY => &["crt1-reactor.o"],
                _ => {
                    return Err(fail(
                        // alloc: error path
                        format!(
                            "a wasm32-wasip1 executable entered at `{entry}`: wasi-libc starts a command \
                             at `_start` and a reactor at `{REACTOR_ENTRY}`"
                        ),
                    ));
                }
            },
            _ => &[],
        })
    }

    /// The C runtime's objects last on the line.
    fn end(self) -> &'static [&'static str] {
        match self {
            Target::LinuxX86_64 | Target::LinuxAarch64 => &["crtendS.o", "crtn.o"],
            _ => &[],
        }
    }

    /// The libraries after the crates': what the system `cc` adds (`-lgcc -lgcc_s -lc` for
    /// glibc, `-lSystem` on macOS) and wasi-libc. MSVC's CRT comes in by the `/DEFAULTLIB`s of
    /// the libraries that link it ([`Line::defaults`]), and rustc's spec adds nothing. What a
    /// Mach-O object autolinks follows it ([`Line::autolinks`]).
    fn late(self) -> &'static [(&'static str, NativeLibKind)] {
        const STATIC: NativeLibKind = NativeLibKind::Static { whole: false, bundle: true };
        match self {
            Target::LinuxX86_64 | Target::LinuxAarch64 => {
                &[("gcc", STATIC), ("gcc_s", NativeLibKind::Dylib), ("c", NativeLibKind::Dylib)]
            }
            Target::MacOs => &[("System", NativeLibKind::Dylib)],
            Target::Wasip1 => &[("c", STATIC)],
            Target::Msvc | Target::WasmUnknown | Target::Wasm64Wasip1 => &[],
        }
    }

    /// Whether a library the crates ask for is provided by a Rust crate of the program on this
    /// target, and so never searched for: wasm64-blaze-wasip1 has no wasi-libc, and its C
    /// runtime is a crate of blaze's `library/`, which defines what the `libc` crate's
    /// `#[link(name = "c")]` names. Every other library is searched for, and one not found is
    /// an error.
    fn provided_by_rust(self, name: &str) -> bool {
        matches!((self, name), (Target::Wasm64Wasip1, "c"))
    }
}

/// `$MACOSX_DEPLOYMENT_TARGET`, at least rustc's minimum, as rustc takes it.
fn minimum<A: Ask>(ask: &A) -> Result<u32, LinkError> {
    let Some(given) = ask.var("MACOSX_DEPLOYMENT_TARGET")? else { return Ok(MACOS_MINIMUM) };
    let version = parse_version(&given).ok_or_else(|| {
        // alloc: error path
        fail(format!("MACOSX_DEPLOYMENT_TARGET={given}: not a version (`major[.minor[.patch]]`)"))
    })?;
    Ok(version.max(MACOS_MINIMUM))
}

/// `major[.minor[.patch]]` as `xxxx.yy.zz` nibbles.
fn parse_version(text: &str) -> Option<u32> {
    let mut parts = text.trim().split('.');
    let part = |it: Option<&str>, max: u32| -> Option<u32> {
        match it {
            None => Some(0),
            Some(it) if it.bytes().all(|b| b.is_ascii_digit()) => it.parse().ok().filter(|it| *it <= max),
            Some(_) => None,
        }
    };
    let major = part(Some(parts.next()?), 0xffff)?;
    let minor = part(parts.next(), 0xff)?;
    let patch = part(parts.next(), 0xff)?;
    if parts.next().is_some() {
        return None;
    }
    Some(major << 16 | minor << 8 | patch)
}

/// The string value of `key` in the outermost object of the JSON `text`, unescaped as it is
/// written; `None` when it has none or is not well formed there.
fn top_level_string<'t>(text: &'t str, key: &str) -> Option<&'t str> {
    let bytes = text.as_bytes();
    let mut depth = 0u32;
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'{' | b'[' => {
                depth += 1;
                at += 1;
            }
            b'}' | b']' => {
                depth = depth.checked_sub(1)?;
                at += 1;
            }
            b'"' => {
                let (string, end) = string_at(text, at)?;
                at = end;
                if depth != 1 || string != key {
                    continue;
                }
                let Some(value) = text[at..].trim_start().strip_prefix(':') else { continue };
                let value = value.trim_start();
                if !value.starts_with('"') {
                    return None;
                }
                return string_at(text, text.len() - value.len()).map(|it| it.0);
            }
            _ => at += 1,
        }
    }
    None
}

/// The JSON string whose `"` is at `at`, and where it ends.
fn string_at(text: &str, at: usize) -> Option<(&str, usize)> {
    let bytes = text.as_bytes();
    let mut i = at + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return Some((&text[at + 1..i], i + 1)),
            _ => i += 1,
        }
    }
    None
}

/// What the toolchain is found from: the environment, and the answers of the commands that
/// say where its files are. [`System`] asks the machine; tests answer.
pub(crate) trait Ask {
    /// The variable `name`; an error for one that is not UTF-8.
    fn var(&self, name: &str) -> Result<Option<String>, LinkError>;
    /// `program` run once per argument list, all at once: each one's standard output, or why
    /// it failed.
    fn run(&self, program: &Path, runs: &[Vec<String>]) -> Vec<Result<String, String>>;
    /// `program` run once with `args`: its standard error, where a C driver prints `-###`'s
    /// jobs, or why it failed.
    fn said(&self, program: &Path, args: &[String]) -> Result<String, String>;
}

/// The machine: its environment and its commands.
pub(crate) struct System;

impl Ask for System {
    fn var(&self, name: &str) -> Result<Option<String>, LinkError> {
        match std::env::var_os(name) {
            None => Ok(None),
            Some(value) => value
                .into_string()
                .map(Some)
                // alloc: error path
                .map_err(|value| fail(format!("{name}={}: not UTF-8", value.to_string_lossy()))),
        }
    }

    fn run(&self, program: &Path, runs: &[Vec<String>]) -> Vec<Result<String, String>> {
        let mut children = Vec::with_capacity(runs.len());
        for args in runs {
            children.push(
                Command::new(program)
                    .args(args)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn(),
            );
        }
        let mut answers = Vec::with_capacity(runs.len());
        for child in children {
            answers.push((|| {
                // alloc: error path
                let done = child.map_err(|error| error.to_string())?.wait_with_output().map_err(|error| error.to_string())?;
                if !done.status.success() {
                    // alloc: error path
                    return Err(format!("{}: {}", done.status, String::from_utf8_lossy(&done.stderr).trim()));
                }
                // alloc: error path
                String::from_utf8(done.stdout).map_err(|_| "it printed what is not UTF-8".to_owned())
            })());
        }
        answers
    }

    fn said(&self, program: &Path, args: &[String]) -> Result<String, String> {
        let done = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .output()
            // alloc: error path
            .map_err(|error| error.to_string())?;
        if !done.status.success() {
            // alloc: error path
            return Err(format!("{}: {}", done.status, String::from_utf8_lossy(&done.stderr).trim()));
        }
        // alloc: error path
        String::from_utf8(done.stderr).map_err(|_| "it printed what is not UTF-8".to_owned())
    }
}

/// What a toolchain was found from: the target, each of [`Target::vars`] (an empty one is
/// unset) and, for WASI, blaze's toolchain.
#[derive(PartialEq, Eq, Debug)]
struct Key {
    target: Target,
    vars: Vec<Option<String>>,
    sysroot: Option<PathBuf>,
}

impl Key {
    fn of<A: Ask>(ask: &A, target: Target, sysroot: Option<&Path>) -> Result<Key, LinkError> {
        let names = target.vars();
        let mut vars = Vec::with_capacity(names.len());
        for name in names {
            vars.push(ask.var(name)?.filter(|it| !it.is_empty()));
        }
        let sysroot = match target {
            Target::Wasip1 => sysroot.map(Path::to_path_buf),
            _ => None,
        };
        Ok(Key { target, vars, sysroot })
    }

    fn var(&self, at: usize) -> Option<&str> {
        self.vars[at].as_deref()
    }

    /// The variable `name` of [`Target::vars`]; `None` where it is unset or the target reads
    /// no such variable.
    fn named(&self, name: &str) -> Option<&str> {
        let at = self.target.vars().iter().position(|it| *it == name)?;
        self.var(at)
    }
}

/// The default libraries each member of a PE archive asks for, as `(member, library)`
/// ([`crate::pe::asked_libraries`]), or the `LC_LINKER_OPTION`s of a Mach-O object or
/// archive's members, as `(member, option)` ([`crate::macho::asked_libraries`]).
type Asked = Arc<Vec<(String, String)>>;

/// The toolchains found, by what each was found from, and what the PE archives read for their
/// default libraries ask for, by their bytes: a few of each per process, searched in order.
pub(crate) struct Cache {
    found: Mutex<Vec<(Key, Arc<Found>)>>,
    asked: Mutex<Vec<(ContentId, u32, Asked)>>,
}

impl Cache {
    pub(crate) const fn new() -> Cache {
        // alloc: none, an empty `Vec` holds no buffer
        Cache { found: Mutex::new(Vec::new()), asked: Mutex::new(Vec::new()) }
    }

    /// What the PE archive, or the Mach-O object or archive for `arch`, at `path` (`name` on
    /// the line) asks for: read once for each content the file has had.
    fn asked(&self, path: &Path, name: &str, format: Format, arch: Arch) -> Result<Asked, LinkError> {
        let domain = match (format, arch) {
            (Format::MachO, Arch::X86_64) => AUTOLINKED_X86_64,
            (Format::MachO, _) => AUTOLINKED_AARCH64,
            _ => ASKED,
        };
        self.read(path, name, (domain, 0), |bytes| match format {
            Format::MachO => crate::macho::asked_libraries(name, bytes, arch),
            _ => crate::pe::asked_libraries(name, bytes),
        })
    }

    /// The install names the Mach-O library for `arch` and `platform` at `path` (`name` on the
    /// line) re-exports from outside itself, as `("", install)`
    /// ([`crate::macho::reexported`]): read once for each content the file has had.
    fn reexported(&self, path: &Path, name: &str, arch: Arch, platform: u32) -> Result<Asked, LinkError> {
        let domain = match arch {
            Arch::X86_64 => REEXPORTED_X86_64,
            _ => REEXPORTED_AARCH64,
        };
        self.read(path, name, (domain, platform), |bytes| {
            let installs = crate::macho::reexported(name, bytes, arch, platform)?;
            // alloc: once per process and library content, its re-exports
            Ok(installs.into_iter().map(|it| (String::new(), it)).collect())
        })
    }

    /// What `read` makes of the bytes at `path`, named in `domain` and read for `platform`:
    /// once for each content.
    fn read(
        &self,
        path: &Path,
        name: &str,
        (domain, platform): (&'static str, u32),
        read: impl FnOnce(&[u8]) -> Result<Vec<(String, String)>, String>,
    ) -> Result<Asked, LinkError> {
        // alloc: error path
        let bytes = std::fs::read(path).map_err(|error| fail(format!("{name}: {error}")))?;
        let id = ContentId::of(domain, &bytes);
        if let Some((.., asked)) = self.asked.lock().iter().find(|it| it.0 == id && it.1 == platform) {
            return Ok(Arc::clone(asked));
        }
        // alloc: once per process and file content, what it asks for
        let asked = Arc::new(read(&bytes).map_err(fail)?);
        self.asked.lock().push((id, platform, Arc::clone(&asked)));
        Ok(asked)
    }

    /// The toolchain of `target` as the environment is now, with blaze's toolchain at
    /// `sysroot` for WASI: asked once, then kept. The lock is held while it is asked, so no
    /// two links ask the same thing.
    fn found<A: Ask>(&self, ask: &A, target: Target, sysroot: Option<&Path>) -> Result<Arc<Found>, LinkError> {
        let key = Key::of(ask, target, sysroot)?;
        // Only a whole answer is pushed: one a panic interrupted left nothing.
        let mut kept = self.found.lock();
        if let Some((_, found)) = kept.iter().find(|(it, _)| *it == key) {
            return Ok(Arc::clone(found));
        }
        // alloc: once per process and toolchain, its answers
        let found = Arc::new(query(ask, &key)?);
        kept.push((key, Arc::clone(&found)));
        Ok(found)
    }
}

/// A target's toolchain: where its files are.
#[derive(Default, Debug)]
struct Found {
    /// The directories the C driver passes its linker (`-L`), searched after the link's own,
    /// in order ([`linker_dirs`]).
    linker: Vec<PathBuf>,
    /// Apple's: the framework directories the C driver passes its linker (`-F`).
    linker_frameworks: Vec<PathBuf>,
    /// The toolchain's own, searched after the C driver's, in order.
    dirs: Vec<PathBuf>,
    /// Apple's: where frameworks are, after the C driver's.
    frameworks: Vec<PathBuf>,
    /// The C runtime's start and end objects, by file name.
    objects: Vec<(&'static str, PathBuf)>,
    /// ELF: an executable's interpreter.
    interpreter: Option<String>,
    /// Mach-O: the SDK's version, as `xxxx.yy.zz` nibbles.
    sdk: u32,
    /// Mach-O: the SDK, which ld64's `-syslibroot` reroots an install name under.
    root: Option<PathBuf>,
    /// The `-L` directories of `NIX_LDFLAGS` the C driver does not pass its linker
    /// ([`unpassed`]): never searched, as rustc's link through `cc` never searches them, and
    /// named by a library's not-found error when one of them has it.
    unpassed: Vec<PathBuf>,
}

impl Found {
    fn object(&self, name: &str) -> Result<&Path, LinkError> {
        self.objects
            .iter()
            .find(|it| it.0 == name)
            .map(|it| it.1.as_path())
            // alloc: error path
            .ok_or_else(|| fail(format!("the C runtime's {name} is not among the toolchain's files")))
    }
}

fn query<A: Ask>(ask: &A, key: &Key) -> Result<Found, LinkError> {
    match key.target {
        Target::LinuxX86_64 | Target::LinuxAarch64 => linux(ask, key),
        Target::MacOs => apple(ask, key),
        Target::Msvc => msvc(ask, key),
        Target::Wasip1 => wasi(key),
        // Nothing to find: no C runtime files ([`Target::provided_by_rust`]).
        Target::WasmUnknown | Target::Wasm64Wasip1 => Ok(Found::default()),
    }
}

/// glibc's loader for `target`: its file name, where the glibc ABI puts it, and the
/// architecture's multiarch directory name.
fn loader(target: Target) -> (&'static str, &'static str, &'static str) {
    match target {
        Target::LinuxAarch64 => ("ld-linux-aarch64.so.1", "/lib/ld-linux-aarch64.so.1", "aarch64-linux-gnu"),
        _ => ("ld-linux-x86-64.so.2", "/lib64/ld-linux-x86-64.so.2", "x86_64-linux-gnu"),
    }
}

/// Whether `dir` is where an FHS system keeps its libc (`/usr/lib/x86_64-linux-gnu`,
/// `/usr/lib64`, ...), whose loader is at the glibc ABI's path.
fn fhs(dir: &Path, multiarch: &str) -> bool {
    let Some(dir) = dir.to_str() else { return false };
    let dir = dir.trim_end_matches('/');
    matches!(dir, "/lib" | "/lib64" | "/usr/lib" | "/usr/lib64")
        || dir.strip_prefix("/lib/").or_else(|| dir.strip_prefix("/usr/lib/")) == Some(multiarch)
}

/// The C toolchain for a Linux target, asked where its directories and its C runtime's files
/// are.
fn linux<A: Ask>(ask: &A, key: &Key) -> Result<Found, LinkError> {
    let triple = key.target.triple();
    let cc = c_driver(key, 0)?;
    let (program, given, said) = (cc.program, &cc.given, &cc.said);
    let (loader, abi, multiarch) = loader(key.target);
    let files = ["Scrt1.o", "crti.o", "crtbeginS.o", "crtendS.o", "crtn.o", "libc.so.6", loader];
    let args = |last: String| -> Vec<String> {
        let mut args = Vec::with_capacity(given.len() + 1);
        // alloc: once per toolchain asked, its commands
        args.extend(given.iter().map(|it| (*it).to_owned()));
        args.push(last);
        args
    };
    let mut runs = Vec::with_capacity(files.len() + 1);
    // alloc: once per toolchain asked, its commands
    runs.push(args("-print-search-dirs".to_owned()));
    for file in files {
        // alloc: once per toolchain asked, its commands
        runs.push(args(format!("-print-file-name={file}")));
    }
    // alloc: error path
    let shown = |at: usize| format!("`{program} {}`", runs[at].join(" "));
    let mut answers = ask.run(Path::new(program), &runs).into_iter();
    let mut answer = |at: usize| -> Result<String, LinkError> {
        answers
            .next()
            // alloc: error path
            .unwrap_or_else(|| Err("no answer".to_owned()))
            // alloc: error path
            .map_err(|why| fail(format!("{}: {why}; the C toolchain for {triple} is {said}", shown(at))))
    };

    let printed = answer(0)?;
    let listed = printed
        .lines()
        .find_map(|line| line.strip_prefix("libraries:"))
        // alloc: error path
        .ok_or_else(|| fail(format!("{} printed no `libraries:` line", shown(0))))?;
    // alloc: once per toolchain asked, its directories
    let mut dirs: Vec<PathBuf> = Vec::new();
    for dir in listed.trim().trim_start_matches('=').split(':').filter(|it| !it.is_empty()) {
        let dir = PathBuf::from(dir);
        if dir.is_dir() && !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    let mut objects = Vec::with_capacity(5);
    let mut libc = PathBuf::new();
    let mut found_loader = PathBuf::new();
    for (at, file) in files.into_iter().enumerate() {
        let printed = answer(at + 1)?;
        let path = PathBuf::from(printed.trim());
        if !path.is_absolute() || !path.is_file() {
            return Err(fail(
                // alloc: error path
                format!(
                    "{} did not find {file}: the C toolchain for {triple} is {said}, and it has no {file}",
                    shown(at + 1)
                ),
            ));
        }
        match at {
            5 => libc = path,
            6 => found_loader = path,
            _ => objects.push((file, path)),
        }
    }
    // alloc: error path
    let real = std::fs::canonicalize(&libc).map_err(|error| fail(format!("{}: {error}", libc.display())))?;
    let interpreter = match real.parent().is_some_and(|it| fhs(it, multiarch)) {
        // alloc: once per toolchain asked, the interpreter's path
        true => abi.to_owned(),
        false => found_loader
            .to_str()
            .ok_or_else(|| {
                // alloc: error path
                fail(format!("{}: an interpreter's path that is not UTF-8", found_loader.display()))
            })?
            // alloc: once per toolchain asked, the interpreter's path
            .to_owned(),
    };
    let (linker, _) = linker_dirs(ask, &cc, triple)?;
    dirs.retain(|it| !linker.contains(it));
    let unpassed = unpassed(key.named("NIX_LDFLAGS"), &linker);
    Ok(Found { linker, dirs, objects, interpreter: Some(interpreter), unpassed, ..Found::default() })
}

/// The C driver a target's toolchain is asked with: `$CC_<triple with _>`, else `$CC`, else
/// `cc`, as [`Target::vars`] names them from `at`; its program, the words given after it, and
/// what errors say of it.
struct CDriver<'k> {
    program: &'k str,
    given: SmallVec<[&'k str; 4]>,
    said: String,
}

fn c_driver(key: &Key, at: usize) -> Result<CDriver<'_>, LinkError> {
    let triple = key.target.triple();
    let names = key.target.vars();
    let (cc, said) = match (key.var(at), key.var(at + 1)) {
        // alloc: once per toolchain asked, what its errors say of it
        (Some(cc), _) => (cc, format!("`{cc}`, from {}", names[at])),
        // alloc: once per toolchain asked, what its errors say of it
        (None, Some(cc)) => (cc, format!("`{cc}`, from CC")),
        // alloc: once per toolchain asked, what its errors say of it
        (None, None) => ("cc", format!("`cc`, as neither {} nor CC is set", names[at])),
    };
    let mut words = cc.split_whitespace();
    // alloc: error path
    let program = words.next().ok_or_else(|| fail(format!("the C toolchain for {triple}: {said} names no program")))?;
    // alloc: none up to four words, which the `SmallVec` holds in place
    let given: SmallVec<[&str; 4]> = words.collect();
    Ok(CDriver { program, given, said })
}

/// The directories the C driver passes its linker, `-L`s then Apple's `-F`s, each in its
/// order, from the jobs `-###` prints (none is run): what a wrapper adds (nix's cc-wrapper,
/// from `NIX_LDFLAGS`), `LIBRARY_PATH`'s and the driver's own, as rustc's link gets them
/// through `cc`. One that is not a directory is left out, as the linker passes over it, and so
/// is a relative one: it names the linker's working directory, which blaze's link has none of.
fn linker_dirs<A: Ask>(ask: &A, cc: &CDriver<'_>, triple: &str) -> Result<(Vec<PathBuf>, Vec<PathBuf>), LinkError> {
    let mut args = Vec::with_capacity(cc.given.len() + 4);
    // alloc: once per toolchain asked, its command
    args.extend(cc.given.iter().chain(&["-###", "-x", "c", "/dev/null"]).map(|it| (*it).to_owned()));
    // alloc: error path
    let shown = || format!("`{} {}`", cc.program, args.join(" "));
    let printed = ask.said(Path::new(cc.program), &args).map_err(|why| {
        // alloc: error path
        fail(format!("{}: {why}; the C toolchain for {triple} is {}", shown(), cc.said))
    })?;
    let job = printed
        .lines()
        .filter(|it| it.starts_with(' '))
        .last()
        // alloc: error path
        .ok_or_else(|| fail(format!("{} printed no job: the C toolchain for {triple} is {}", shown(), cc.said)))?;
    // alloc: error path
    let words = quoted_words(job).ok_or_else(|| fail(format!("{} printed a linker job with an unclosed quote or a trailing `\\`: {job}", shown())))?;
    // alloc: once per toolchain asked, its directories
    let (mut dirs, mut frameworks): (Vec<PathBuf>, Vec<PathBuf>) = (Vec::new(), Vec::new());
    for (at, word) in words.iter().enumerate() {
        let (to, dir) = match (word.strip_prefix("-L"), word.strip_prefix("-F")) {
            (Some(dir), _) => (&mut dirs, dir),
            (None, Some(dir)) => (&mut frameworks, dir),
            (None, None) => continue,
        };
        let dir = match dir.is_empty() {
            true => words.get(at + 1).map(String::as_str).ok_or_else(|| {
                // alloc: error path
                fail(format!("{} printed a linker job ending in {word}, with no directory after it", shown()))
            })?,
            false => dir,
        };
        let dir = PathBuf::from(dir);
        if dir.is_absolute() && dir.is_dir() && !to.contains(&dir) {
            to.push(dir);
        }
    }
    Ok((dirs, frameworks))
}

/// The `-L` directories `NIX_LDFLAGS` names that are not in `passed`, the C driver's: nix's
/// cc-wrapper passes `NIX_LDFLAGS` to the linker only where the dev shell's role marker
/// (`NIX_CC_WRAPPER_TARGET_HOST_<target>`, with lowercase letters) is in the environment, so
/// an environment copied without it keeps `NIX_LDFLAGS` and loses what it adds. Only an
/// absolute directory that exists is kept, as [`linker_dirs`] keeps them.
fn unpassed(nix_ldflags: Option<&str>, passed: &[PathBuf]) -> Vec<PathBuf> {
    // alloc: none until a directory is left out, which a dev shell's environment never has
    let mut out: Vec<PathBuf> = Vec::new();
    let mut words = nix_ldflags.unwrap_or_default().split_whitespace();
    while let Some(word) = words.next() {
        let dir = match word.strip_prefix("-L") {
            Some("") => words.next(),
            Some(dir) => Some(dir),
            None => None,
        };
        let Some(dir) = dir.map(Path::new) else { continue };
        let seen = passed.iter().chain(&out).any(|it| it == dir);
        if dir.is_absolute() && dir.is_dir() && !seen {
            // alloc: once per toolchain asked, each directory left out
            out.push(dir.to_path_buf());
        }
    }
    out
}

/// The words of a job as a C driver's `-###` prints it, split as a shell would: a word is in
/// double quotes (clang, GCC before 16) or bare up to the next space (GCC 16 quotes only words
/// that need it), and `\` takes the next character as it is in either; `None` for a line with an
/// unclosed quote or a trailing `\`.
fn quoted_words(line: &str) -> Option<Vec<String>> {
    // alloc: once per toolchain asked, its linker's words
    let mut out = Vec::new();
    let mut chars = line.chars().peekable();
    loop {
        match chars.peek() {
            None => return Some(out),
            Some(' ') => {
                chars.next();
                continue;
            }
            Some(_) => {}
        }
        // alloc: once per toolchain asked, each word of its linker's job
        let mut word = String::new();
        let mut quoted = false;
        loop {
            match (chars.next(), quoted) {
                (None, false) => break,
                (None, true) => return None,
                (Some('"'), _) => quoted = !quoted,
                (Some('\\'), _) => word.push(chars.next()?),
                (Some(' '), false) => break,
                (Some(it), _) => word.push(it),
            }
        }
        out.push(word);
    }
}

#[cfg(test)]
#[test]
fn a_linker_job_splits_into_the_same_words_quoted_or_bare() {
    let quoted = r#" "/usr/bin/ld" "-L/usr/lib" "-o" "a b" "x\"y""#;
    let bare = r#" /usr/bin/ld -L/usr/lib -o "a b" x\"y"#;
    let want = ["/usr/bin/ld", "-L/usr/lib", "-o", "a b", "x\"y"].map(String::from).to_vec();
    assert_eq!(quoted_words(quoted), Some(want.clone()));
    assert_eq!(quoted_words(bare), Some(want));
    assert_eq!(quoted_words(r#" "open"#), None, "an unclosed quote is refused");
}

/// The macOS SDK: its library and framework directories and its version.
fn apple<A: Ask>(ask: &A, key: &Key) -> Result<Found, LinkError> {
    let sdk = match key.var(0) {
        Some(root) => PathBuf::from(root),
        None => {
            // alloc: once per toolchain asked, its command
            let run = ["--sdk", "macosx", "--show-sdk-path"].map(str::to_owned).to_vec();
            let printed = ask
                .run(Path::new("xcrun"), std::slice::from_ref(&run))
                .pop()
                // alloc: error path
                .unwrap_or_else(|| Err("no answer".to_owned()))
                .map_err(|why| {
                    // alloc: error path
                    fail(format!("the macOS SDK: SDKROOT is not set, and `xcrun --sdk macosx --show-sdk-path`: {why}"))
                })?;
            PathBuf::from(printed.trim())
        }
    };
    let settings = sdk.join("SDKSettings.json");
    let text = std::fs::read_to_string(&settings)
        // alloc: error path
        .map_err(|error| fail(format!("the macOS SDK's version: {}: {error}", settings.display())))?;
    let version = top_level_string(&text, "Version").and_then(parse_version).ok_or_else(|| {
        // alloc: error path
        fail(format!("{}: no `Version` that is `major[.minor[.patch]]`", settings.display()))
    })?;
    let (linker, linker_frameworks) = linker_dirs(ask, &c_driver(key, 3)?, key.target.triple())?;
    let unpassed = unpassed(key.named("NIX_LDFLAGS"), &linker);
    Ok(Found {
        linker,
        linker_frameworks,
        unpassed,
        // alloc: once per toolchain asked, its directories
        dirs: vec![sdk.join("usr/lib"), sdk.join("usr/local/lib")],
        // alloc: once per toolchain asked, its directories
        frameworks: vec![sdk.join("Library/Frameworks"), sdk.join("System/Library/Frameworks")],
        sdk: version,
        root: Some(sdk),
        ..Found::default()
    })
}

/// The MSVC and Windows SDK library directories.
fn msvc<A: Ask>(ask: &A, key: &Key) -> Result<Found, LinkError> {
    if let Some(lib) = key.var(0) {
        // alloc: once per toolchain asked, its directories
        let dirs = lib.split(';').filter(|it| !it.is_empty()).map(PathBuf::from).collect();
        return Ok(Found { dirs, ..Found::default() });
    }
    let missing = |what: String| {
        // alloc: error path
        fail(format!(
            "the MSVC and Windows SDK libraries for x86_64-pc-windows-msvc: LIB is not set (a \
             Developer Command Prompt sets it), and {what}"
        ))
    };
    let Some(programs) = key.var(1) else {
        // alloc: error path
        return Err(missing("ProgramFiles(x86), where vswhere is, is not set".to_owned()));
    };
    let vswhere = Path::new(programs).join("Microsoft Visual Studio").join("Installer").join("vswhere.exe");
    let run = [
        "-latest",
        "-products",
        "*",
        "-requires",
        "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
        "-property",
        "installationPath",
    ]
    .map(str::to_owned)
    // alloc: once per toolchain asked, its command
    .to_vec();
    let asked = |program: &Path, run: &Vec<String>| -> Result<String, LinkError> {
        ask.run(program, std::slice::from_ref(run))
            .pop()
            // alloc: error path
            .unwrap_or_else(|| Err("no answer".to_owned()))
            // alloc: error path
            .map_err(|why| missing(format!("`{} {}`: {why}", program.display(), run.join(" "))))
    };
    let install = asked(&vswhere, &run)?;
    let install = install.trim();
    if install.is_empty() {
        // alloc: error path
        return Err(missing(format!("{} names no Visual Studio with the MSVC x64 tools", vswhere.display())));
    }
    let install = Path::new(install);
    let default = install.join("VC").join("Auxiliary").join("Build").join("Microsoft.VCToolsVersion.default.txt");
    let version = std::fs::read_to_string(&default)
        // alloc: error path
        .map_err(|error| missing(format!("{}: {error}", default.display())))?;
    let tools = install.join("VC").join("Tools").join("MSVC").join(version.trim()).join("lib").join("x64");
    if !tools.is_dir() {
        // alloc: error path
        return Err(missing(format!("{} is not a directory", tools.display())));
    }
    let run = ["query", r"HKLM\SOFTWARE\Microsoft\Windows Kits\Installed Roots", "/v", "KitsRoot10", "/reg:32"]
        .map(str::to_owned)
        // alloc: once per toolchain asked, its command
        .to_vec();
    let printed = asked(Path::new("reg"), &run)?;
    let root = printed
        .lines()
        .find_map(|line| line.trim_start().strip_prefix("KitsRoot10"))
        .and_then(|rest| rest.trim_start().strip_prefix("REG_SZ"))
        .map(str::trim)
        .filter(|it| !it.is_empty())
        // alloc: error path
        .ok_or_else(|| missing("the registry names no Windows 10 SDK (KitsRoot10)".to_owned()))?;
    let lib = Path::new(root).join("Lib");
    let kit = newest_kit(&lib)
        // alloc: error path
        .map_err(|error| missing(format!("{}: {error}", lib.display())))?
        // alloc: error path
        .ok_or_else(|| missing(format!("{} has no SDK with um\\x64\\kernel32.lib", lib.display())))?;
    let kit = lib.join(kit);
    Ok(Found {
        // alloc: once per toolchain asked, its directories
        dirs: vec![tools, kit.join("ucrt").join("x64"), kit.join("um").join("x64")],
        ..Found::default()
    })
}

/// The newest Windows SDK under `lib` (`10.0.22621.0`) that has the x64 libraries.
fn newest_kit(lib: &Path) -> std::io::Result<Option<String>> {
    let mut newest: Option<(SmallVec<[u32; 4]>, String)> = None;
    for entry in std::fs::read_dir(lib)? {
        let entry = entry?;
        let Ok(name) = entry.file_name().into_string() else { continue };
        let Some(version) = name.split('.').map(|it| it.parse::<u32>().ok()).collect::<Option<SmallVec<[u32; 4]>>>()
        else {
            continue;
        };
        if !entry.path().join("um").join("x64").join("kernel32.lib").is_file() {
            continue;
        }
        if newest.as_ref().is_none_or(|(it, _)| version > *it) {
            newest = Some((version, name));
        }
    }
    Ok(newest.map(|it| it.1))
}

/// wasi-libc in blaze's toolchain.
fn wasi(key: &Key) -> Result<Found, LinkError> {
    let Some(sysroot) = &key.sysroot else {
        return Err(fail(
            "wasm32-wasip1's C runtime (wasi-libc) is in blaze's toolchain, and its files were \
             asked for without it: only `locate`, which is given the sysroot, asks for them"
                // alloc: error path
                .to_owned(),
        ));
    };
    let dir = sysroot.join("lib").join("rustlib").join("wasm32-wasip1").join("lib").join("self-contained");
    let start = dir.join("crt1-command.o");
    if !start.is_file() {
        return Err(fail(
            // alloc: error path
            format!(
                "{} is not there: blaze's toolchain at {} has no wasi-libc for wasm32-wasip1",
                start.display(),
                sysroot.display()
            ),
        ));
    }
    // Every wasm32-wasip1 link takes it (`late`); named here with its path, where a search's
    // miss would name only `-lc`.
    let libc = dir.join("libc.a");
    if !libc.is_file() {
        return Err(fail(
            // alloc: error path
            format!(
                "{} is not there: blaze's toolchain at {} has no wasi-libc for wasm32-wasip1",
                libc.display(),
                sysroot.display()
            ),
        ));
    }
    // alloc: once per toolchain asked, its files
    let mut objects = vec![("crt1-command.o", start)];
    // A toolchain without it links commands still; a reactor's line then names it missing.
    let reactor = dir.join("crt1-reactor.o");
    if reactor.is_file() {
        objects.push(("crt1-reactor.o", reactor));
    }
    Ok(Found { dirs: vec![dir], objects, ..Found::default() })
}

/// A link line as it is found.
struct Line<'a> {
    cache: &'a Cache,
    output: &'a Output,
    search: &'a [PathBuf],
    found: &'a Found,
    out: Vec<Located>,
    /// Each search directory's entry names, read once per link ([`listing`]).
    listings: RefCell<HashMap<PathBuf, Option<Listing>, Global>>,
}

/// A directory's entry names, ASCII lowercased.
type Listing = HashSet<Box<[u8]>, Global>;

/// The entry names of `dir`, ASCII lowercased; none when it or an entry can't be read, which
/// leaves every name of it to a stat, as a directory that can be searched but not listed is.
fn listing(dir: &Path) -> Option<Listing> {
    let mut names = Listing::default();
    for entry in std::fs::read_dir(dir).ok()? {
        // alloc: once per entry of a search directory per link
        names.insert(entry.ok()?.file_name().as_encoded_bytes().to_ascii_lowercase().into_boxed_slice());
    }
    Some(names)
}

impl Line<'_> {
    /// Where a library is searched for, and what named each directory: the link's, then those
    /// the C driver passes its linker, then the toolchain's.
    fn searched(&self, framework: bool) -> impl Iterator<Item = (&Path, Searched)> {
        let (driver, theirs) = match framework {
            true => (&self.found.linker_frameworks, &self.found.frameworks),
            false => (&self.found.linker, &self.found.dirs),
        };
        let link = self.search.iter().map(|it| (it.as_path(), Searched::Link));
        let driver = driver.iter().map(|it| (it.as_path(), Searched::CDriver));
        link.chain(driver).chain(theirs.iter().map(|it| (it.as_path(), Searched::Toolchain)))
    }

    fn dirs(&self, framework: bool) -> impl Iterator<Item = &Path> {
        self.searched(framework).map(|it| it.0)
    }

    /// Whether `dir` may have `file`: false only when no entry of `dir` is `file`'s first
    /// component with ASCII case ignored, so a miss costs no syscall and a case-insensitive file
    /// system still finds what a stat finds. A name the listing can't answer for (not ASCII, or
    /// not starting with a plain name) is a maybe.
    fn may_have(&self, dir: &Path, file: &str) -> bool {
        let Some(Component::Normal(first)) = Path::new(file).components().next() else { return true };
        if !file.is_ascii() {
            return true;
        }
        let mut listings = self.listings.borrow_mut();
        if !listings.contains_key(dir) {
            // alloc: once per search directory per link
            listings.insert(dir.to_path_buf(), listing(dir));
        }
        let Some(names) = &listings[dir] else { return true };
        let mut key: SmallVec<[u8; 64]> = SmallVec::new();
        // alloc: none for a name of up to 64 bytes
        key.extend(first.as_encoded_bytes().iter().map(u8::to_ascii_lowercase));
        names.contains(&key[..])
    }

    fn object(&mut self, name: &str) -> Result<(), LinkError> {
        let path = self.found.object(name)?.to_path_buf();
        self.push(path, NativeKind::Object)
    }

    fn library(&mut self, name: &str, kind: NativeLibKind, verbatim: bool) -> Result<(), LinkError> {
        let (path, kind) = self.find(name, kind, verbatim)?;
        self.push(path, kind)
    }

    /// The first file in the first directory that has one of those `kind` allows.
    fn find(&self, name: &str, kind: NativeLibKind, verbatim: bool) -> Result<(PathBuf, NativeKind), LinkError> {
        if name.is_empty() {
            // alloc: error path
            return Err(fail("a native library with no name".to_owned()));
        }
        let files = files(self.output.format, name, kind, verbatim)?;
        let framework = matches!(kind, NativeLibKind::Framework);
        for dir in self.dirs(framework) {
            for (file, kind) in &files {
                if !self.may_have(dir, file) {
                    continue;
                }
                let path = dir.join(file);
                if path.is_file() {
                    return Ok((path, *kind));
                }
            }
        }
        Err(not_found(name, &files, self.searched(framework), &self.found.unpassed))
    }

    /// `path` on the line, unless it is already, then the files it names when it is a linker
    /// script, the libraries its members ask for when it is a PE archive, and those its
    /// `LC_LINKER_OPTION`s ask for when it is a Mach-O object or archive.
    fn push(&mut self, path: PathBuf, kind: NativeKind) -> Result<(), LinkError> {
        let pe = self.output.format == Format::Pe;
        if let Some(it) =
            self.out.iter_mut().find(|it| it.path == path || (pe && it.path.as_os_str().eq_ignore_ascii_case(path.as_os_str())))
        {
            // A Mach-O library on the line as a re-export that the link also asks for is loaded
            // as ld64 loads one asked for, used or not.
            if self.output.format == Format::MachO && kind == (NativeKind::Shared { as_needed: false }) {
                it.kind = kind;
            }
            return Ok(());
        }
        let script =
            self.output.format == Format::Elf && matches!(kind, NativeKind::Shared { .. }) && sniff(&path)? == Sniffed::Text;
        let archive = pe && matches!(kind, NativeKind::Archive { .. });
        let autolinks =
            self.output.format == Format::MachO && matches!(kind, NativeKind::Archive { .. } | NativeKind::Object);
        let reexports = self.output.format == Format::MachO && matches!(kind, NativeKind::Shared { .. });
        // alloc: once per file of the line, the name the engine knows it by
        let name = path.to_string_lossy().into_owned();
        self.out.push(Located { path, name, kind });
        let at = self.out.len() - 1;
        if script {
            self.follow(at)?;
        }
        if archive {
            self.defaults(at)?;
        }
        if autolinks {
            self.autolinks(at)?;
        }
        if reexports {
            self.reexports(at)?;
        }
        Ok(())
    }

    /// The libraries the Mach-O library at `at` re-exports from outside itself, each after it
    /// on the line and followed in turn, as ld64 loads them with it ([`Line::reexported`]).
    /// Each is needed only when the image imports from it: ld64 binds such an import to the
    /// re-exporting library, blaze to the one that defines it, which the loader finds alike.
    fn reexports(&mut self, at: usize) -> Result<(), LinkError> {
        let platform = self.output.platform.as_ref().map_or(0, |it| it.platform);
        let asked = self.cache.reexported(&self.out[at].path, &self.out[at].name, self.output.arch, platform)?;
        for (_, install) in asked.iter() {
            let path = self.reexported(at, install).map_err(|it| LinkError {
                // alloc: error path
                what: format!("{} re-exports {install} (LC_REEXPORT_DYLIB), and {}", self.out[at].name, it.what),
                ..it
            })?;
            self.push(path, NativeKind::Shared { as_needed: true })?;
        }
        Ok(())
    }

    /// The file of the install name `install` that the Mach-O library at `at` re-exports, as
    /// ld64 finds it: under the SDK (`-syslibroot`) first, then as it is, a `.tbd` before its
    /// `.dylib` in each; `@loader_path/` from the library's own directory. `@rpath/` and
    /// `@executable_path/` name the image's run, which a re-export is not found by: an error.
    fn reexported(&self, at: usize, install: &str) -> Result<PathBuf, LinkError> {
        let (dir, file) = match install.strip_prefix("@loader_path/") {
            Some(rest) => {
                let Some(parent) = self.out[at].path.parent() else {
                    // alloc: error path
                    return Err(fail(format!("{} is in no directory", self.out[at].name)));
                };
                let path = parent.join(rest);
                // alloc: error path
                let file = path.file_name().map(|it| it.to_string_lossy().into_owned()).unwrap_or_default();
                (path.parent().map(Path::to_path_buf), file)
            }
            None if install.starts_with('@') => {
                // alloc: error path
                return Err(fail(format!("{install} names the image's run, which ld64 does not find a re-export by")));
            }
            None => {
                let path = Path::new(install);
                // alloc: error path
                let file = path.file_name().map(|it| it.to_string_lossy().into_owned()).unwrap_or_default();
                (path.parent().filter(|_| path.is_absolute()).map(Path::to_path_buf), file)
            }
        };
        let Some(dir) = dir.filter(|_| !file.is_empty()) else {
            // alloc: error path
            return Err(fail(format!("{install} is not an absolute path to a library")));
        };
        let mut searched: SmallVec<[(PathBuf, Searched); 2]> = SmallVec::new();
        if let Some(root) = &self.found.root
            && !install.starts_with('@')
        {
            // alloc: error path
            searched.push((root.join(dir.strip_prefix("/").unwrap_or(&dir)), Searched::Toolchain));
        }
        searched.push((dir, Searched::InstallName));
        let stub = Path::new(&file).with_extension("tbd").to_string_lossy().into_owned();
        let mut files: SmallVec<[(String, NativeKind); 2]> = SmallVec::new();
        if stub != file {
            files.push((stub, NativeKind::Shared { as_needed: true }));
        }
        files.push((file, NativeKind::Shared { as_needed: true }));
        for (dir, _) in &searched {
            for (file, _) in &files {
                let path = dir.join(file);
                if path.is_file() {
                    return Ok(path);
                }
            }
        }
        // An install name is found at its own path, never on a `-L`.
        Err(not_found(install, &files, searched.iter().map(|(dir, by)| (dir.as_path(), *by)), &[]))
    }

    /// The libraries the members of the PE archive at `at` ask for, each after it on the line
    /// and followed in turn, as link.exe follows `/DEFAULTLIB` from the members it takes. A
    /// member's own `/NODEFAULTLIB` takes its own out ([`crate::pe::asked_libraries`]); one in
    /// another member does not, since that member may not be taken. Every member's are
    /// followed, taken or not: a library on the line that nothing is taken from costs nothing,
    /// and the engine checks those of the members it takes.
    fn defaults(&mut self, at: usize) -> Result<(), LinkError> {
        let asked = self.cache.asked(&self.out[at].path, &self.out[at].name, Format::Pe, self.output.arch)?;
        for (member, library) in asked.iter() {
            let path = self.default_library(library).map_err(|it| LinkError {
                // alloc: error path
                what: format!("{}({member}) asks for {library} (/DEFAULTLIB), and {}", self.out[at].name, it.what),
                ..it
            })?;
            self.push(path, NativeKind::Archive { whole: false })?;
        }
        Ok(())
    }

    /// The libraries the `LC_LINKER_OPTION`s of the Mach-O object or archive at `at` ask for,
    /// each after it on the line and followed in turn, as ld64 follows them from every object
    /// and loaded member: `-lNAME` searched as a `dylib` is (`libNAME.tbd`, `.dylib`, `.so`,
    /// then `.a`, in the link's directories then the SDK's `usr/lib`), `-framework NAME` as
    /// a framework (in the link's directories then the SDK's frameworks). A weak form or any
    /// other option is an error ([`crate::macho::autolink`]). Every member's are followed,
    /// taken or not, and the engine checks those of the objects and members it loads.
    fn autolinks(&mut self, at: usize) -> Result<(), LinkError> {
        let asked = self.cache.asked(&self.out[at].path, &self.out[at].name, Format::MachO, self.output.arch)?;
        for (member, option) in asked.iter() {
            let by = |what: &str| match member.is_empty() {
                // alloc: error path
                true => fail(format!("{} asks for {option} (LC_LINKER_OPTION), and {what}", self.out[at].name)),
                // alloc: error path
                false => fail(format!("{}({member}) asks for {option} (LC_LINKER_OPTION), and {what}", self.out[at].name)),
            };
            let (name, kind) = match crate::macho::autolink(option).map_err(|what| by(&what))? {
                crate::macho::Autolink::Library(name) => (name, NativeLibKind::Dylib),
                crate::macho::Autolink::Framework(name) => (name, NativeLibKind::Framework),
            };
            let (path, kind) = self.find(name, kind, false).map_err(|it| LinkError { what: by(&it.what).what, ..it })?;
            self.push(path, kind)?;
        }
        Ok(())
    }

    /// The file a `/DEFAULTLIB` names: itself when it is a path, else the first in the line's
    /// directories with its name, ASCII case ignored as link.exe ignores it.
    fn default_library(&self, library: &str) -> Result<PathBuf, LinkError> {
        let given = Path::new(library);
        if given.is_absolute() {
            return match given.is_file() {
                true => Ok(given.to_path_buf()),
                // alloc: error path
                false => Err(fail(format!("{library} is not a file"))),
            };
        }
        for dir in self.dirs(false) {
            if !self.may_have(dir, library) {
                continue;
            }
            let path = dir.join(library);
            if path.is_file() {
                return Ok(path);
            }
            if let Some(path) = caseless(dir, library) {
                return Ok(path);
            }
        }
        // alloc: error path
        let files = [(library.to_owned(), NativeKind::Archive { whole: false })];
        Err(not_found(library, &files, self.searched(false), &self.found.unpassed))
    }

    /// The files the linker script at `at` names, after it on the line: an absolute path as it
    /// is, another name searched for as a file in the line's directories, `-lNAME` as a
    /// `dylib` is and `-l:FILE` as a `+verbatim` one.
    fn follow(&mut self, at: usize) -> Result<(), LinkError> {
        let script = &self.out[at];
        let bytes: Arc<[u8]> = std::fs::read(&script.path)
            // alloc: error path
            .map_err(|error| fail(format!("{}: {error}", script.name)))?
            .into();
        // clone: once per linker script, its name while the line grows
        let name = script.name.clone();
        let native = Native { id: ContentId::of(SCRIPT, &bytes), name: &name, bytes: &bytes, kind: script.kind, order: at as u32 };
        for input in crate::script_inputs(self.output, &native)? {
            let path = match input.strip_prefix("-l") {
                Some(lib) => {
                    let (lib, verbatim) = match lib.strip_prefix(':') {
                        Some(file) => (file, true),
                        None => (lib, false),
                    };
                    let (path, kind) = self
                        .find(lib, NativeLibKind::Dylib, verbatim)
                        // alloc: error path
                        .map_err(|it| LinkError { what: format!("{name} names {input}, and {}", it.what), ..it })?;
                    self.push(path, kind)?;
                    continue;
                }
                None if Path::new(&input).is_absolute() => PathBuf::from(&input),
                None => self.find(&input, NativeLibKind::Dylib, true)?.0,
            };
            if !path.is_file() {
                // alloc: error path
                return Err(fail(format!("{name} names {input}, which is not a file")));
            }
            let kind = match sniff(&path)? {
                Sniffed::Archive => NativeKind::Archive { whole: false },
                Sniffed::Object => NativeKind::Object,
                Sniffed::Shared | Sniffed::Text => NativeKind::Shared { as_needed: true },
            };
            self.push(path, kind)?;
        }
        Ok(())
    }
}

/// The file names a library may have, in the order the linker tries them in a directory, and
/// what each is read as. ELF's shared libraries are `--as-needed`, as rustc links them.
fn files(
    format: Format,
    name: &str,
    kind: NativeLibKind,
    verbatim: bool,
) -> Result<SmallVec<[(String, NativeKind); 4]>, LinkError> {
    let shared = NativeKind::Shared { as_needed: format == Format::Elf };
    let mut files = SmallVec::new();
    match kind {
        NativeLibKind::Framework if format != Format::MachO => {
            // alloc: error path
            return Err(fail(format!("the framework `{name}`: frameworks are Apple's, and this links {format:?}")));
        }
        NativeLibKind::Framework => {
            // alloc: once per library searched, its file names
            files.push((format!("{name}.framework/{name}.tbd"), shared));
            // alloc: once per library searched, its file names
            files.push((format!("{name}.framework/{name}"), shared));
        }
        // alloc: once per library searched, its file name
        _ if verbatim => files.push((name.to_owned(), by_extension(format, name, kind))),
        NativeLibKind::Static { whole, .. } => {
            let archive = NativeKind::Archive { whole };
            match format {
                // alloc: once per library searched, its file name
                Format::Pe => files.push((format!("{name}.lib"), archive)),
                // alloc: once per library searched, its file name
                Format::Elf | Format::MachO | Format::Wasm => files.push((format!("lib{name}.a"), archive)),
            }
        }
        NativeLibKind::Dylib => {
            let archive = NativeKind::Archive { whole: false };
            match format {
                Format::Elf => {
                    // alloc: once per library searched, its file names
                    files.push((format!("lib{name}.so"), shared));
                    // alloc: once per library searched, its file names
                    files.push((format!("lib{name}.a"), archive));
                }
                Format::MachO => {
                    for extension in ["tbd", "dylib", "so"] {
                        // alloc: once per library searched, its file names
                        files.push((format!("lib{name}.{extension}"), shared));
                    }
                    // alloc: once per library searched, its file names
                    files.push((format!("lib{name}.a"), archive));
                }
                // An import library or a static one: the archive reader takes both.
                // alloc: once per library searched, its file name
                Format::Pe => files.push((format!("{name}.lib"), archive)),
                // alloc: once per library searched, its file name
                Format::Wasm => files.push((format!("lib{name}.a"), archive)),
            }
        }
    }
    Ok(files)
}

/// What a `+verbatim` file is read as, by its extension, as the linker takes `-l:NAME`.
fn by_extension(format: Format, name: &str, kind: NativeLibKind) -> NativeKind {
    let whole = matches!(kind, NativeLibKind::Static { whole: true, .. });
    let extension = Path::new(name).extension().and_then(|it| it.to_str()).unwrap_or_default();
    let is = |it: &str| extension.eq_ignore_ascii_case(it);
    if is("o") || is("obj") {
        NativeKind::Object
    } else if is("a") || is("lib") {
        NativeKind::Archive { whole }
    } else {
        NativeKind::Shared { as_needed: format == Format::Elf }
    }
}

/// `unpassed` is [`Found::unpassed`]: the first of them with one of `files` is named, since
/// that library is there and the environment kept the C driver from passing it.
fn not_found<'a>(
    name: &str,
    files: &[(String, NativeKind)],
    searched: impl Iterator<Item = (&'a Path, Searched)>,
    unpassed: &[PathBuf],
) -> LinkError {
    // alloc: error path
    let searched: Vec<(PathBuf, Searched)> = searched.map(|(dir, by)| (dir.to_path_buf(), by)).collect();
    // alloc: error path
    let mut what = String::new();
    let _ = write!(what, "the native library `{name}` is not found: no ");
    for (at, (file, _)) in files.iter().enumerate() {
        what.push_str(if at == 0 { "" } else { ", " });
        what.push_str(file);
    }
    what.push_str(" in ");
    for (at, (dir, by)) in searched.iter().enumerate() {
        what.push_str(if at == 0 { "" } else { ", " });
        let by = match by {
            Searched::Link => "the link's -L",
            Searched::CDriver => "the C driver's linker -L",
            Searched::Toolchain => "the toolchain's",
            Searched::InstallName => "the re-exported install name's",
        };
        let _ = write!(what, "{} ({by})", dir.display());
    }
    if searched.is_empty() {
        what.push_str("any directory: the link, the C driver and the toolchain name none");
    }
    // alloc: error path
    let there = unpassed.iter().find_map(|dir| files.iter().map(|it| dir.join(&it.0)).find(|it| it.is_file()));
    if let Some(there) = there {
        let _ = write!(
            what,
            "; {} is there, in a directory NIX_LDFLAGS names that the C driver does not pass its \
             linker: nix's cc-wrapper reads NIX_LDFLAGS only when its role marker \
             `NIX_CC_WRAPPER_TARGET_HOST_<target>` is set, as `nix develop` sets it, and this \
             environment lacks it (copied without the names that have lowercase letters?)",
            there.display()
        );
    }
    let found = NotFound {
        // alloc: error path
        library: name.to_owned(),
        // alloc: error path
        files: files.iter().map(|it| it.0.clone()).collect(),
        searched,
    };
    // alloc: error path
    LinkError { function: None, what, refusal: Some(Refusal::NotFound(Box::new(found))) }
}

/// The file in `dir` named `file` with ASCII case ignored, where the directory is case
/// sensitive (a Windows SDK copied to Linux or macOS).
fn caseless(dir: &Path, file: &str) -> Option<PathBuf> {
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        if entry.file_name().to_str().is_some_and(|it| it.eq_ignore_ascii_case(file)) {
            let path = entry.path();
            if path.is_file() {
                return Some(path);
            }
        }
    }
    None
}

/// What a file's first bytes say it is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Sniffed {
    Archive,
    Object,
    Shared,
    /// Neither an archive nor an ELF file: a linker script, for the reader to read or refuse.
    Text,
}

fn sniff(path: &Path) -> Result<Sniffed, LinkError> {
    // alloc: error path
    let failed = |error: std::io::Error| fail(format!("{}: {error}", path.display()));
    let mut file = std::fs::File::open(path).map_err(failed)?;
    let mut head = [0u8; 18];
    let mut read = 0;
    while read < head.len() {
        match file.read(&mut head[read..]) {
            Ok(0) => break,
            Ok(n) => read += n,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(failed(error)),
        }
    }
    let head = &head[..read];
    if head.starts_with(b"!<arch>\n") {
        return Ok(Sniffed::Archive);
    }
    if !head.starts_with(b"\x7fELF") {
        return Ok(Sniffed::Text);
    }
    let e_type = match (head.get(5), head.get(16..18)) {
        (Some(1), Some(it)) => u16::from_le_bytes([it[0], it[1]]),
        (Some(2), Some(it)) => u16::from_be_bytes([it[0], it[1]]),
        // alloc: error path
        _ => return Err(fail(format!("{}: an ELF file too short to say what it is", path.display()))),
    };
    match e_type {
        1 => Ok(Sniffed::Object),
        3 => Ok(Sniffed::Shared),
        // alloc: error path
        other => Err(fail(format!("{}: an ELF file of type {other}, neither an object nor a shared library", path.display()))),
    }
}

fn fail(what: String) -> LinkError {
    LinkError { function: None, what, refusal: None }
}
