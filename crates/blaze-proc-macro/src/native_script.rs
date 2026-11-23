//! D65/D69: one native build script run end to end, confined: its environment (cargo's, with
//! the real folders in place of the key's, and the pinned C toolchain's from
//! [`blaze_build_script::toolchain::c_env`]), the run a hermetic one (bstd `procs::hermetic`,
//! D177: the one way blaze runs a program confined), and what it
//! left made into the same outputs record a wasm script's run gives ([`Outputs`]):
//!
//! - its standard output with the real `OUT_DIR` and package folder written as the key's
//!   (`/target/debug/build/<name>-<config>/out`, `/package` or `/workspace/<dir>`), so every run of one key gives
//!   the same bytes (rule 25);
//! - every file it left under `OUT_DIR`, but clang's `-MD` dependency files, as a stored file
//!   of the record (name, length, mode, SHA-256), its bytes renamed into the home's file area
//!   and streamed once for the digest, never held ([`store_out_dir`]: librocksdb-sys leaves an
//!   810 MB archive);
//! - the `-MD` dependency files read into the headers read (each sysroot file's path under the
//!   sysroot and the SHA-256 of its bytes, [`headers_digest`]) and removed: they hold real
//!   paths.
//!
//! Files are read through the effect layer (a world rooted at each folder). A non-zero exit is
//! a typed failure with the end of its standard error; it is never tried again as wasm (rule 3).

use core::time::Duration;
use std::{path::Path, time::Instant};

use blaze_build_script::{
    Env,
    fact_shim,
    instr::OutputLimits,
    key::RunLimits,
    host_tools::{self, HostTool},
    outputs::{FileLimits, FileMode, Outputs},
    toolchain::{CInputs, c_env, depfile_reads, headers_digest, map_paths},
};
use blaze_core::wait::Deadline;
use blaze_ids::ContentId;
use blaze_own::{AString, AVec, Alloc, String, Vec};
use blaze_roots::{Digest, Mount, RelPath, Root, RootId, RootKind, Roots, Seen};
use bstd::{
    fs::Durable,
    mem::{Bytes, TASK_FRAME},
    procs::{
        Exit,
        hermetic::{
            Budget, Capture, ClockSpec, Fault, FilesError, FilesOp, Hermetic, HermeticCapOpts, HermeticOpts, HermeticSpec, HermeticStreams,
            Level, Program, ReportSpec, RootFiles,
        },
    },
};

use crate::record::native_world;

/// A confined workspace file list for certifying a native build script's reads: instead of
/// granting the whole package or workspace subtree, `workspace` is mounted as a
/// [`RootKind::Package`] whose only admitted paths are `files` (relative to `workspace`),
/// enforced file-by-file by the OS confinement (macOS Seatbelt literal rules / Linux Landlock).
#[derive(Clone, Copy, Debug)]
pub struct ConfinedFiles<'s> {
    pub workspace: &'s str,
    pub files: &'s [&'s str],
}

/// One script to run.
#[derive(Clone, Copy, Debug)]
pub struct NativeScript<'s> {
    /// The run's key (`native_unit`'s base key: the script, its package, its producers' views,
    /// the pins). While it is the same, nothing beneath a folder the run reads changes, so it
    /// names each read folder as a tree root and the program (D177: `RootKind::Tree`).
    pub key: ContentId,
    /// The script's host binary, canonical.
    pub program: &'s str,
    /// The C toolchain and folders: `c.package` is the copied package folder, `c.out_dir` an
    /// empty `OUT_DIR`, both canonical.
    pub c: CInputs<'s>,
    /// cargo's environment as the key holds it ([`blaze_build_script::env`]).
    pub env: &'s Env<'s>,
    /// More folders it reads, read-only: its `links` dependencies' trees as laid for this run
    /// ([`crate::script_sandbox::dep_mount::native::lay`], brief N8).
    pub reads: &'s [&'s str],
    /// More real folders its output may name, each with the text its record keeps instead (the
    /// laid trees, written as the wasm sight names them, brief N8): no path of this run is kept.
    pub shown: &'s [(&'s str, &'s str)],
    /// The host tools it is granted (D71, [`blaze_build_script::host_tools::granted`]), each
    /// `path` the resolved real path: started by a `PATH` of exactly their folders, read and
    /// executed, their `reads` read-only. Empty for nearly every script.
    pub host_tools: &'s [HostTool<'s>],
    /// D112 interim (brief N7): the run's fact shim folder ([`crate::native_facts::prepare`]):
    /// its `bin/` first on `PATH` (so `sw_vers` and `pkg-config` are answered from the pins and
    /// never reach the machine's tools), the shim granted execute, the folder read, its `log/`
    /// the one part written. `None` only in an example whose script asks no fact; a spawned
    /// probe then finds no program, as before.
    pub facts: Option<&'s crate::native_facts::ShimRun>,
    /// The command's jobserver ([`crate::jobserver`]): the run is told `NUM_JOBS` the workers
    /// and `CARGO_MAKEFLAGS` naming its fifo, granted read and write, and lent it for the run.
    /// `None` only in an example or test of something else: `NUM_JOBS` `1`, no jobserver. A
    /// command always gives one (`NativeSetting::jobs`).
    pub jobs: Option<&'s crate::jobserver::Jobserver>,
    /// When `Some`, omits `c.package` from tree-root grants and instead mounts `workspace` as a
    /// [`RootKind::Package`] admitting only `files` (used to certify exact workspace reads).
    pub confined: Option<ConfinedFiles<'s>>,
    pub limits: RunLimits,
    pub deadline: Duration,
    /// The home's file area (`<store>/build-script/files`, canonical, made): where each file
    /// the script leaves in `OUT_DIR` goes, by digest ([`store_out_dir`]); on the volume of
    /// `c.out_dir`.
    pub files: &'s str,
}

/// What one run made.
#[derive(Debug)]
pub struct NativeScriptRun<'a> {
    pub outputs: Outputs<'a>,
    /// The raw `cargo:rerun-if-changed=` / `cargo::rerun-if-changed=` paths printed by the
    /// script (before path remapping), used only as candidate hints for confined certification.
    pub rerun_if_changed: AVec<'a, AString<'a>>,
    /// The digest of the sysroot headers its compiles read, and how many.
    pub headers: ContentId,
    pub headers_read: usize,
    /// Files its compiles read outside the sysroot and the package (the toolchain's own
    /// headers are not listed by `-MD`): none expected.
    pub other_reads: usize,
    pub stderr: std::vec::Vec<u8>,
    pub wall: Duration,
}

/// Why a native run gave no record; each names the script.
#[derive(Debug)]
pub enum NativeScriptError {
    /// The hermetic run (bstd `procs::hermetic`, D177) refused it or failed: the fault names the
    /// rule ([`Fault::rule`]).
    Refused { program: std::string::String, fault: Fault },
    /// The world the run is started from offers no `capability` (a host, its spawner, a clock).
    NoHost { program: std::string::String, capability: &'static str },
    /// The script ran to an end that was not exit 0. `exit` is how it ended (a status, or the
    /// signal that killed it); `stderr` and `stdout` the ends of its streams (on macOS
    /// `sandbox-exec`'s own refusals are on that standard error: an empty one means it said
    /// nothing); `confinement` the BLAKE3 of what confined it ([`bstd::procs::hermetic::confinement`],
    /// the SBPL profile on macOS) and `profile` that text, printed when the script gave no
    /// reason of its own (killed by a signal, or nothing on either stream).
    Failed {
        program: std::string::String,
        exit: Exit,
        stderr: std::string::String,
        stdout: std::string::String,
        confinement: Digest,
        profile: Option<std::string::String>,
    },
    Output { program: std::string::String, why: std::string::String },
    Files { folder: std::string::String, why: std::string::String },
    /// The script's environment lacks a variable cargo always sets for a build script (D96 P5:
    /// never shown as a default root).
    MissingVar { program: std::string::String, name: &'static str },
    /// The command's jobserver could not lend the run or take its tokens back.
    Jobs { program: std::string::String, error: crate::jobserver::JobserverError },
    /// The script left something in `OUT_DIR` that is not a plain file or a folder (a link, a
    /// socket): the record holds files only, and a link's target is not the script's output.
    NotAFile { folder: std::string::String, name: std::string::String, kind: bstd::fs::EntryKind },
}

impl std::fmt::Display for NativeScriptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NativeScriptError::Refused { program, fault: Fault::Stopped(stop) } => write!(
                f,
                "{program}: the native build script was stopped by its hermetic run ({stop:?}; rule 18); nothing it wrote is kept"
            ),
            NativeScriptError::Refused { program, fault } => {
                write!(f, "{program}: the native build script's hermetic run was refused by {:?}: {fault:?}", fault.rule())
            }
            NativeScriptError::NoHost { program, capability } => {
                write!(f, "{program}: the world a native build script is started from offers no `{capability}` (rule 9: nothing runs then)")
            }
            NativeScriptError::Failed { program, exit, stderr, stdout, confinement, profile } => {
                write!(f, "{program}: the native build script ")?;
                match *exit {
                    Exit::Exited(code) => write!(f, "exited with status {code}")?,
                    Exit::Signaled(signal) => write!(f, "was killed by signal {signal} ({})", signal_name(signal))?,
                }
                write!(f, " (not retried as wasm, rule 3); it ran confined by ")?;
                for byte in &confinement.0[..8] {
                    write!(f, "{byte:02x}")?;
                }
                if stderr.is_empty() {
                    write!(f, "; its standard error is empty")?;
                } else {
                    write!(f, "; its standard error ends:\n{stderr}")?;
                }
                if !stdout.is_empty() {
                    write!(f, "\nits standard output ends:\n{stdout}")?;
                }
                if let Some(profile) = profile {
                    write!(f, "\nit gave no reason of its own; what confined it:\n{profile}")?;
                }
                Ok(())
            }
            NativeScriptError::Output { program, why } => write!(f, "{program}: its outputs: {why}"),
            NativeScriptError::Files { folder, why } => write!(f, "{folder}: {why}"),
            NativeScriptError::MissingVar { program, name } => {
                write!(f, "{program}: the build script's environment has no `{name}`, which cargo always sets for a build script (decision D96: no default root is shown)")
            }
            NativeScriptError::Jobs { program, error } => write!(f, "{program}: {error}"),
            NativeScriptError::NotAFile { folder, name, kind } => write!(
                f,
                "{folder}/{name}: the build script left a {kind:?} in OUT_DIR, not a file or a folder; its outputs record holds files only (a link's target is not its output), so it is refused (rule 3)"
            ),
        }
    }
}

impl std::error::Error for NativeScriptError {}

/// The name of `signal` on this host, for a killed script's message.
fn signal_name(signal: i32) -> &'static str {
    match signal {
        libc::SIGABRT => "SIGABRT: it aborted, or the loader did",
        libc::SIGKILL => "SIGKILL: the kernel or a sandbox killed it, or its group's kill",
        libc::SIGSEGV => "SIGSEGV",
        libc::SIGBUS => "SIGBUS",
        libc::SIGILL => "SIGILL",
        libc::SIGTRAP => "SIGTRAP",
        libc::SIGSYS => "SIGSYS: a system call the confinement refuses",
        libc::SIGXCPU => "SIGXCPU: its CPU limit",
        libc::SIGTERM => "SIGTERM",
        libc::SIGPIPE => "SIGPIPE",
        _ => "another signal",
    }
}

/// The `OUT_DIR` and `CARGO_MANIFEST_DIR` the script is shown (what its outputs' real folders
/// are mapped back to), each from its environment; a missing one is refused by name (rule 3).
fn shown_folders<'e>(env: &'e Env<'_>, program: &str) -> Result<(&'e str, &'e str), NativeScriptError> {
    let missing = |name: &'static str| NativeScriptError::MissingVar { program: program.to_owned(), name };
    let out = env.get("OUT_DIR").ok_or_else(|| missing("OUT_DIR"))?;
    let package = env.get("CARGO_MANIFEST_DIR").ok_or_else(|| missing("CARGO_MANIFEST_DIR"))?;
    Ok((out, package))
}

#[cfg(test)]
mod d96_tests {
    use blaze_build_script::Env;
    use blaze_own::{AString, AVec, Alloc};

    use super::{NativeScriptError, shown_folders};

    fn env(pairs: &[(&str, &str)]) -> Env<'static> {
        let mut vars = AVec::new_in(Alloc::global());
        for (n, v) in pairs {
            vars.push((AString::from_str_in(n, Alloc::global()), AString::from_str_in(v, Alloc::global())));
        }
        vars.sort_by(|a: &(AString<'static>, AString<'static>), b| a.0.as_str().cmp(b.0.as_str()));
        Env { vars }
    }

    /// D96 P5: cargo always sets `OUT_DIR` and `CARGO_MANIFEST_DIR` for a build script (the
    /// cargo book, "Environment variables Cargo sets for build scripts"); an environment
    /// without one is refused naming it and the script, never shown `/out` or `/workspace`.
    /// Prints nothing; passes.
    #[test]
    fn a_missing_manifest_dir_or_out_dir_is_refused_by_name() {
        let full = env(&[("CARGO_MANIFEST_DIR", "/package"), ("OUT_DIR", "/target/debug/build/x-0/out")]);
        assert_eq!(shown_folders(&full, "build-script-build").map_err(|e| e.to_string()), Ok(("/target/debug/build/x-0/out", "/package")));
        let no_dir = env(&[("OUT_DIR", "/o")]);
        assert!(matches!(shown_folders(&no_dir, "s"), Err(NativeScriptError::MissingVar { name: "CARGO_MANIFEST_DIR", .. })));
        let no_out = env(&[("CARGO_MANIFEST_DIR", "/package")]);
        assert!(matches!(shown_folders(&no_out, "s"), Err(NativeScriptError::MissingVar { name: "OUT_DIR", .. })));
    }
}

/// One entry under a folder as its walk listed it (name, kind, length, execute bit): nothing
/// of it read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listed {
    /// Relative to the folder, `/`-separated.
    pub name: std::string::String,
    pub kind: bstd::fs::EntryKind,
    pub len: u64,
    pub executable: bool,
}

/// Every entry under `folder` but its folders (which are walked), sorted by path, listed
/// through the effect layer: no byte of any file is read.
pub fn listed_under(folder: &str) -> Result<std::vec::Vec<Listed>, NativeScriptError> {
    let failed = |why: std::string::String| NativeScriptError::Files { folder: folder.to_owned(), why };
    let (io, root) = native_world(Path::new(folder)).map_err(|e| failed(e.to_string()))?;
    let cx = io.cx(Duration::from_secs(60), TASK_FRAME).map_err(|_| failed("its world stopped".to_owned()))?;
    // `folder` is one build script's `OUT_DIR` (or a folder of the command's own work): the
    // walk enters every folder of it. The limits fit the largest outputs met (a C library's
    // objects and generated headers: a few thousand files in a few dozen folders) with room,
    // and refuse, naming the folder, a script that left a tree no record should hold: 4,096
    // folders, 262,144 entries in all, 64 levels deep.
    let limits = bstd::fs::WalkLimits { folders: 32768, entries: 1 << 18, depth: 64 };
    let mut out = std::vec::Vec::new();
    root.walk(&cx, None, limits, cx.frame(), |entries, enter| {
        for i in 0..entries.len() {
            let kind = entries.kind(i);
            if kind == bstd::fs::EntryKind::Dir {
                let name = entries.name(i);
                if name != "target" && name != ".blaze" && name != ".git" {
                    enter.enter(i);
                }
                continue;
            }
            let path = entries.path(i).map_err(|e| format!("{}: not a path: {e:?}", entries.name(i)))?;
            out.push(Listed { name: path.as_str().to_owned(), kind, len: entries.len_of(i), executable: entries.executable(i) });
        }
        Ok::<(), std::string::String>(())
    })
    .map_err(|e| failed(e.to_string()))?;
    // Sorted by the paths' bytes, as the outputs record has them (the walk's own order
    // compares part by part).
    out.sort_unstable_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Lists every subdirectory and non-directory entry under `folder` (skipping `target`, `.blaze`,
/// `.git`), sorted by path bytes, allocated in `alloc`.
pub(crate) fn walk_tree<'a>(folder: &str, alloc: Alloc<'a>) -> Option<(AVec<'a, AString<'a>>, AVec<'a, AString<'a>>)> {
    let (io, root) = native_world(Path::new(folder)).ok()?;
    let cx = io.cx(Duration::from_secs(60), TASK_FRAME).ok()?;
    let limits = bstd::fs::WalkLimits { folders: 32768, entries: 1 << 18, depth: 64 };
    let mut dirs: AVec<'a, AString<'a>> = Vec::new_in(alloc);
    let mut files: AVec<'a, AString<'a>> = Vec::new_in(alloc);
    root.walk(&cx, None, limits, cx.frame(), |entries, enter| {
        for i in 0..entries.len() {
            let kind = entries.kind(i);
            let path = entries.path(i).map_err(|_| "entry is not a valid path")?;
            if kind == bstd::fs::EntryKind::Dir {
                let name = entries.name(i);
                if name != "target" && name != ".blaze" && name != ".git" {
                    dirs.push(String::from_str_in(path.as_str(), alloc));
                    enter.enter(i);
                }
                continue;
            }
            files.push(String::from_str_in(path.as_str(), alloc));
        }
        Ok::<(), &'static str>(())
    })
    .ok()?;
    dirs.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
    files.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
    Some((dirs, files))
}

/// The bytes one read of a stored file's digest takes: one buffer, reused for every chunk of
/// every file.
const CHUNK: usize = 256 << 10;

/// The limit of the arena the digests are streamed through: one chunk with room. A read that
/// needed more stops, naming the arena; nothing grows it.
const DIGEST_ARENA: usize = 1 << 20;

/// The arena's name, as a stop names it.
const DIGEST_ARENA_NAME: &str = "a native build script's output file, streamed to its digest";

/// What [`store_out_dir`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StoredOutputs {
    /// The digest arena's high-water mark, in bytes: the most of the command's memory any
    /// file's bytes took (never more than [`DIGEST_ARENA`]).
    pub peak: usize,
    pub files: usize,
    pub bytes: u64,
    /// Files whose digest the area held already (left where they were).
    pub present: usize,
}

/// The longest folder both canonical paths are under (`/` at least).
fn common_folder<'p>(a: &'p str, b: &str) -> &'p str {
    let ends = a.match_indices('/').map(|(at, _)| at).filter(|at| *at > 0).chain(core::iter::once(a.len()));
    let mut best = "/";
    for end in ends {
        let prefix = &a[..end];
        let holds = b.strip_prefix(prefix).is_some_and(|rest| rest.is_empty() || rest.starts_with('/'));
        if !holds {
            break;
        }
        best = prefix;
    }
    best
}

/// The lowercase hex of a digest: a stored file's name in the area.
fn hex64(digest: &[u8; 32]) -> std::string::String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = std::string::String::with_capacity(64);
    for byte in digest {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 15)]));
    }
    out
}

/// Each file of `listed` (as [`listed_under`] gave them for `out_dir`) taken into `outputs`
/// as a stored file (name, length, mode, SHA-256), its bytes put in the home's file area
/// `area` at `<area>/<sha256 hex>` (`blaze_build_script::outputs::FILE_AREA` under the
/// store).
///
/// No byte of a file passes through the command's memory but one [`CHUNK`] at a time, for its
/// digest, through one buffer of a named arena (its high-water mark is
/// [`StoredOutputs::peak`]); and none is copied: the file is renamed into the area (one name
/// change, on the one volume both folders are under: a second volume is the driver's error,
/// naming both, and nothing copies in its place, rule 3). A digest the area holds already
/// leaves the file where it was (`OUT_DIR` is emptied before the key's next run). A link, or
/// anything else that is not a plain file, is refused by name: its target is not an output
/// the record can hold. No length is limited; the count of files is (`limits.files`).
///
/// `out_dir` and `area` are canonical and `area` exists.
pub fn store_out_dir<'a>(out_dir: &str, area: &str, listed: &[Listed], outputs: &mut Outputs<'a>, limits: FileLimits, alloc: Alloc<'a>) -> Result<StoredOutputs, NativeScriptError> {
    let failed = |why: std::string::String| NativeScriptError::Files { folder: out_dir.to_owned(), why };
    let top = common_folder(out_dir, area);
    let under = |path: &str| path.get(top.len()..).unwrap_or("").trim_start_matches('/').to_owned();
    let (out_rel, area_rel) = (under(out_dir), under(area));
    let (io, root) = native_world(Path::new(top)).map_err(|e| failed(e.to_string()))?;
    let mem = io.mem().ok_or_else(|| failed("the world over it offers no memory to stream a digest through".to_owned()))?;
    let arena = mem.arena(DIGEST_ARENA_NAME, DIGEST_ARENA).map_err(|e| failed(format!("{e:?}")))?;
    let mut chunk = arena.bytes();
    let mut stored = StoredOutputs { peak: 0, files: 0, bytes: 0, present: 0 };
    for file in listed {
        if file.kind != bstd::fs::EntryKind::File {
            return Err(NativeScriptError::NotAFile { folder: out_dir.to_owned(), name: file.name.clone(), kind: file.kind });
        }
        let named = |why: std::string::String| failed(format!("{}: {why}", file.name));
        // An hour for one file's digest and rename: a deadline, never a hang (rule 18); an
        // 810 MB archive hashes in seconds.
        let cx = io.cx(Duration::from_secs(3600), TASK_FRAME).map_err(|_| named("its world stopped".to_owned()))?;
        let joined = |rel: &str, name: &str| if rel.is_empty() { name.to_owned() } else { format!("{rel}/{name}") };
        let from_text = joined(&out_rel, &file.name);
        let from = bstd::path::Path::new(&from_text).map_err(|_| named("not a relative path".to_owned()))?;
        let opened = root.open_read(&cx, &from).map_err(|e| named(e.to_string()))?;
        let mut sha = blaze_sha256::Sha256::new();
        let mut at = 0u64;
        loop {
            chunk.clear();
            let n = opened.read_at(&cx, at, CHUNK, &mut chunk).map_err(|e| named(e.to_string()))?;
            if n == 0 {
                break;
            }
            sha.update(chunk.as_slice());
            at += n as u64;
        }
        if at != file.len {
            return Err(named(format!("it was listed with {} bytes and read with {at}: it changed after its script ended", file.len)));
        }
        let sha256 = sha.finish();
        let to_text = joined(&area_rel, &hex64(&sha256));
        let to = bstd::path::Path::new(&to_text).map_err(|_| named(format!("{to_text}: not a relative path")))?;
        match root.stat(&cx, &to).map_err(|e| named(e.to_string()))? {
            Some(_) => stored.present += 1,
            None => root.rename_replacing_unsynced(&cx, &from, &to, bstd::fs::Unsynced::No).map_err(|e| named(format!("renamed into the file area {area}: {e}")))?,
        }
        let mode = if file.executable { FileMode::Executable } else { FileMode::Plain };
        outputs.add_stored(&file.name, at, mode, sha256, limits, alloc).map_err(|e| named(format!("{e:?}")))?;
        stored.files += 1;
        stored.bytes += at;
    }
    stored.peak = arena.used();
    Ok(stored)
}

#[cfg(test)]
mod stored_tests {
    use std::io::Write as _;
    use std::time::{Duration, Instant};

    use blaze_build_script::outputs::{FileLimits, FileMode, Outputs, OutputsRef};
    use blaze_own::AVec;

    use super::{DIGEST_ARENA, NativeScriptError, StoredOutputs, common_folder, hex64, listed_under, store_out_dir};

    /// A fresh `<temp>/blaze-scr-<test>-<pid>` with `out` and `files` folders, canonical (the
    /// Mac's temp folder is behind a link), removed by the caller.
    fn folders(test: &str) -> (std::path::PathBuf, std::string::String, std::string::String) {
        let root = std::env::temp_dir().join(format!("blaze-scr-{test}-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&root);
        for dir in ["out", "files"] {
            std::fs::create_dir_all(root.join(dir)).expect("a test folder");
        }
        let root = root.canonicalize().expect("the folder's real path");
        let text = |dir: &str| root.join(dir).to_str().expect("a UTF-8 temp path").to_owned();
        let (out, files) = (text("out"), text("files"));
        (root, out, files)
    }

    fn store(out: &str, files: &str, outputs: &mut Outputs<'static>) -> Result<StoredOutputs, NativeScriptError> {
        let alloc = blaze_own::testing::heap().alloc();
        let listed = listed_under(out)?;
        store_out_dir(out, files, &listed, outputs, FileLimits::DEFAULT, alloc)
    }

    /// The librocksdb.a stop (an 810 MB archive read whole into a 1 GiB arena): a 64 MiB file,
    /// written here in 1 MiB chunks, is recorded with its length and streamed digest while the
    /// digest arena's HIGH-WATER MARK (`StoredOutputs::peak`, the arena's `used()` after the
    /// last file: the most it ever held) stays under 1 MiB, within 240 s; its bytes are renamed
    /// into the area, not copied. Expected line:
    /// `test native_script::stored_tests::a_64_mib_file_is_stored_through_under_1_mib ... ok`.
    #[test]
    fn a_64_mib_file_is_stored_through_under_1_mib() {
        const MIB: usize = 1 << 20;
        let started = Instant::now();
        let (root, out, files) = folders("big");
        let mut expected = blaze_sha256::Sha256::new();
        {
            let mut file = std::fs::File::create(format!("{out}/librocksdb.a")).expect("the big file");
            let mut chunk = vec![0u8; MIB];
            for i in 0..64u32 {
                for (at, byte) in chunk.iter_mut().enumerate() {
                    *byte = (at as u32 ^ i.wrapping_mul(2_654_435_761)) as u8;
                }
                expected.update(&chunk);
                file.write_all(&chunk).expect("a chunk");
            }
        }
        let expected = expected.finish();
        let alloc = blaze_own::testing::heap().alloc();
        let mut outputs = Outputs::new_in("/target/debug/build/librocksdb-sys-0/out", alloc);
        let stored = store(&out, &files, &mut outputs);
        let in_area = std::fs::metadata(format!("{files}/{}", hex64(&expected))).map(|m| m.len());
        let left = std::path::Path::new(&format!("{out}/librocksdb.a")).exists();
        _ = std::fs::remove_dir_all(&root);
        let stored = stored.unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(stored.files, 1);
        assert_eq!(stored.bytes, 64 * MIB as u64);
        assert!(stored.peak > 0, "the digest went through the arena (the check ran)");
        assert!(stored.peak < MIB, "the digest arena's high-water mark is {} bytes", stored.peak);
        assert!(stored.peak <= DIGEST_ARENA);
        let file = outputs.stored_file("librocksdb.a").expect("the archive is a stored file");
        assert_eq!((file.len, file.mode, file.sha256), (64 * MIB as u64, FileMode::Plain, expected));
        assert!(outputs.files.is_empty(), "nothing of it is held inline");
        assert_eq!(in_area.ok(), Some(64 * MIB as u64), "its bytes are in the area under its digest");
        assert!(!left, "renamed, not copied");
        assert!(started.elapsed() < Duration::from_secs(240), "took {:?}", started.elapsed());
    }

    /// Rule 26 and rule 25: a small file and one in a folder are recorded, the record encodes,
    /// opens and lists them back equal, the area holds their bytes equal under their digests,
    /// and a second run from another folder writes the same record bytes (the area already
    /// holding both digests, it leaves them in place). An executable file keeps its bit.
    #[test]
    fn small_files_round_trip_through_the_area() {
        let (root, out, files) = folders("small");
        std::fs::create_dir_all(format!("{out}/sub")).expect("a sub folder");
        std::fs::write(format!("{out}/bindings.rs"), b"pub const X: u32 = 1;\n").expect("a file");
        std::fs::write(format!("{out}/sub/tool"), b"#!/bin/sh\n").expect("a file");
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(format!("{out}/sub/tool"), std::fs::Permissions::from_mode(0o755)).expect("its mode");
        }
        let alloc = blaze_own::testing::heap().alloc();
        let record = |out: &str| {
            let mut outputs = Outputs::new_in("/target/debug/build/x-0/out", alloc);
            let stored = store(out, &files, &mut outputs).unwrap_or_else(|e| panic!("{e}"));
            let mut bytes: AVec<'static, u8> = AVec::new_in(alloc);
            outputs.encode(&mut bytes);
            (stored, bytes.to_vec())
        };
        let (first, bytes) = record(&out);
        let opened = OutputsRef::open(&bytes).expect("the record opens");
        let listed: std::vec::Vec<(std::string::String, u64, FileMode, [u8; 32])> = opened.stored().map(|f| (f.name.to_owned(), f.len, f.mode, *f.sha256)).collect();
        let area = |sha: &[u8; 32]| std::fs::read(format!("{files}/{}", hex64(sha))).expect("the stored file in the area");
        let read_back: std::vec::Vec<std::vec::Vec<u8>> = listed.iter().map(|f| area(&f.3)).collect();

        // The same files from a second folder: the same record.
        let again = root.join("out2");
        std::fs::create_dir_all(again.join("sub")).expect("a second folder");
        std::fs::write(again.join("bindings.rs"), b"pub const X: u32 = 1;\n").expect("a file");
        std::fs::write(again.join("sub/tool"), b"#!/bin/sh\n").expect("a file");
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(again.join("sub/tool"), std::fs::Permissions::from_mode(0o755)).expect("its mode");
        }
        let (second, bytes_again) = record(again.to_str().expect("a UTF-8 path"));
        _ = std::fs::remove_dir_all(&root);

        assert_eq!((first.files, first.present), (2, 0));
        assert_eq!((second.files, second.present), (2, 2), "the area held both digests already");
        assert_eq!(bytes, bytes_again, "two runs from two folders write equal record bytes");
        assert_eq!(listed.iter().map(|f| (f.0.as_str(), f.1, f.2)).collect::<std::vec::Vec<_>>(), [("bindings.rs", 22, FileMode::Plain), ("sub/tool", 10, FileMode::Executable)]);
        assert_eq!(read_back, [b"pub const X: u32 = 1;\n".to_vec(), b"#!/bin/sh\n".to_vec()]);
        let mut sha = blaze_sha256::Sha256::new();
        sha.update(b"pub const X: u32 = 1;\n");
        assert_eq!(listed[0].3, sha.finish());
    }

    /// A link left in `OUT_DIR` is refused naming it (its target is not the script's output);
    /// a folder is walked (the round-trip test stores `sub/tool`).
    #[test]
    fn a_link_in_out_dir_is_refused_by_name() {
        let (root, out, files) = folders("link");
        std::fs::write(format!("{out}/real.h"), b"int x;\n").expect("a file");
        std::os::unix::fs::symlink(format!("{out}/real.h"), format!("{out}/alias.h")).expect("a link");
        let alloc = blaze_own::testing::heap().alloc();
        let mut outputs = Outputs::new_in("/target/debug/build/x-0/out", alloc);
        let refused = store(&out, &files, &mut outputs);
        _ = std::fs::remove_dir_all(&root);
        let error = refused.expect_err("a link is refused");
        assert!(error.to_string().contains("alias.h"), "the refusal names the link: {error}");
        assert!(matches!(&error, NativeScriptError::NotAFile { name, .. } if name == "alias.h"), "{error:?}");
    }

    #[test]
    fn the_common_folder_is_a_whole_part() {
        assert_eq!(common_folder("/a/b/out", "/a/b/files"), "/a/b");
        assert_eq!(common_folder("/a/bc/out", "/a/b/files"), "/a");
        assert_eq!(common_folder("/x/out", "/y/files"), "/");
        assert_eq!(common_folder("/a/b", "/a/b/files"), "/a/b");
    }
}

pub(crate) fn remove(folder: &str, name: &str) -> Result<(), NativeScriptError> {
    let failed = |why: std::string::String| NativeScriptError::Files { folder: folder.to_owned(), why };
    let (io, root) = native_world(Path::new(folder)).map_err(|e| failed(e.to_string()))?;
    let cx = io.cx(Duration::from_secs(60), TASK_FRAME).map_err(|_| failed("its world stopped".to_owned()))?;
    let path = bstd::path::Path::new(name).map_err(|_| failed(format!("{name}: not a relative path")))?;
    root.remove(&cx, &path, Durable::No).map_err(|e| failed(format!("{name}: {e}")))
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut sha = blaze_sha256::Sha256::new();
    sha.update(bytes);
    sha.finish()
}

/// How a native run ended.
struct Ran {
    exit: Exit,
    stdout: std::vec::Vec<u8>,
    stderr: std::vec::Vec<u8>,
}

/// A native build script's budget (D177 requires one; native_sandbox held none but the
/// deadline). CPU: the deadline times [`NativeBudget::cpus`], since a script's group (make,
/// clang) runs that many at once; on Linux `RLIMIT_CPU` holds each process of the group alone,
/// on macOS the meter reads the script's own process. Memory: address space on Linux (each
/// process alone), the script's footprint on macOS; 16 GiB is past every C compile met
/// (librocksdb-sys's largest clang stays under 2 GiB) and under every supported machine.
pub(crate) struct NativeBudget {
    cpus: u32,
    memory: u64,
}

pub(crate) const NATIVE_BUDGET: NativeBudget =NativeBudget { cpus: 16, memory: 16 << 30 };

impl NativeBudget {
    pub(crate) fn of(&self, deadline: Duration) -> Budget {
        Budget { cpu: deadline.saturating_mul(self.cpus), memory: self.memory, steps: None }
    }
}

/// `OUT_DIR` as the hermetic run's output root: listed with digests after the run, emptied
/// after a stopped one. When a certification run mounts the workspace as a [`RootKind::Package`],
/// `package_root` holds that root's id and its admitted relative file paths.
pub(crate) struct OutDir<'d> {
    pub(crate) root: RootId,
    pub(crate) folder: &'d str,
    pub(crate) package_root: Option<(RootId, &'d [&'d str])>,
}

impl OutDir<'_> {
    fn refused(&self, op: FilesOp, error: Option<&std::io::Error>) -> Fault {
        let code = error.and_then(std::io::Error::raw_os_error).and_then(|code| u16::try_from(code).ok()).unwrap_or(0);
        Fault::Files(FilesError { op, root: self.root, code })
    }
}

impl RootFiles for OutDir<'_> {
    fn package_files(&self, root: RootId, each: &mut dyn FnMut(RelPath<'_>, Digest, u64) -> Result<(), Fault>) -> Result<(), Fault> {
        let Some((expected, files)) = self.package_root else {
            return Err(Fault::Files(FilesError { op: FilesOp::PackageFiles, root, code: 0 }));
        };
        if root != expected {
            return Err(Fault::Files(FilesError { op: FilesOp::PackageFiles, root, code: 0 }));
        }
        for &rel in files {
            let path = RelPath::new(rel, blaze_roots::Limits::SHIPPED.most_path)
                .map_err(|_| Fault::Files(FilesError { op: FilesOp::PackageFiles, root, code: 0 }))?;
            each(path, Digest([0; 32]), 0)?;
        }
        Ok(())
    }

    fn seen(&self, root: RootId, _: RelPath<'_>) -> Result<Seen, Fault> {
        Err(Fault::Files(FilesError { op: FilesOp::Seen, root, code: 0 }))
    }

    fn read_output(&self, _: RelPath<'_>, _: u32, _: &mut AVec<'_, u8>) -> Result<(), Fault> {
        Err(self.refused(FilesOp::ReadOutput, None))
    }

    /// Every plain file, its BLAKE3 streamed through one buffer. A link or another kind is not
    /// listed here: [`store_out_dir`] refuses it by name next ([`NativeScriptError::NotAFile`]).
    fn list_output(&self, each: &mut dyn FnMut(RelPath<'_>, Digest, u64) -> Result<(), Fault>) -> Result<(), Fault> {
        use std::io::Read as _;
        let listed = listed_under(self.folder).map_err(|_| self.refused(FilesOp::ListOutput, None))?;
        let mut buf = vec![0u8; CHUNK];
        for file in listed.iter().filter(|file| file.kind == bstd::fs::EntryKind::File) {
            let path = RelPath::new(&file.name, blaze_roots::Limits::SHIPPED.most_path).map_err(|_| self.refused(FilesOp::ListOutput, None))?;
            let mut opened = std::fs::File::open(Path::new(self.folder).join(&file.name)).map_err(|e| self.refused(FilesOp::ListOutput, Some(&e)))?;
            let mut hasher = bstd::digest::Blake3::new();
            let mut len = 0u64;
            loop {
                let n = opened.read(&mut buf).map_err(|e| self.refused(FilesOp::ListOutput, Some(&e)))?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                len += n as u64;
            }
            each(path, Digest(hasher.finalize()), len)?;
        }
        Ok(())
    }

    fn clear_output(&self) -> Result<(), Fault> {
        std::fs::remove_dir_all(self.folder).map_err(|e| self.refused(FilesOp::ClearOutput, Some(&e)))?;
        std::fs::create_dir(self.folder).map_err(|e| self.refused(FilesOp::ClearOutput, Some(&e)))
    }
}

/// Resolves and caches the pinned nightly toolchain's canonical `(sysroot, rustc_bin)` once so
/// native build scripts (`serde_core`, `serde_json`, `thiserror`, `libc`, `autocfg`, etc.) can
/// invoke the pinned `rustc` inside `bstd::procs::hermetic` read-only confinement with zero ambient paths.
fn pinned_rustc_paths() -> Option<&'static (std::string::String, std::string::String)> {
    static CACHE: std::sync::OnceLock<Option<(std::string::String, std::string::String)>> = std::sync::OnceLock::new();
    CACHE
        .get_or_init(|| {
            for args in [
                &[format!("+{}", blaze_pins::TOOLCHAIN), "--print".to_owned(), "sysroot".to_owned()][..],
                &["--print".to_owned(), "sysroot".to_owned()][..],
            ] {
                if let Ok(out) = std::process::Command::new("rustc").args(args).output() {
                    if out.status.success() {
                        let raw = std::string::String::from_utf8_lossy(&out.stdout).trim().to_owned();
                        let Ok(sysroot) = std::fs::canonicalize(&raw) else { continue };
                        let Ok(rustc_bin) = std::fs::canonicalize(sysroot.join("bin/rustc")) else { continue };
                        if let (Some(sr), Some(rb)) = (sysroot.to_str(), rustc_bin.to_str()) {
                            return Some((sr.to_owned(), rb.to_owned()));
                        }
                    }
                }
            }
            None
        })
        .as_ref()
}

/// Resolves and caches the real macOS `dsymutil` binary (never the `/usr/bin/dsymutil` xcrun shim)
/// so `configure` scripts that compile+link `conftest.c` with `-g` succeed under hermetic confinement.
fn pinned_dsymutil_paths() -> Option<&'static (std::string::String, std::string::String)> {
    static CACHE: std::sync::OnceLock<Option<(std::string::String, std::string::String)>> = std::sync::OnceLock::new();
    CACHE
        .get_or_init(|| {
            let candidate = std::path::Path::new("/Applications/Xcode.app/Contents/Developer/Toolchains/XcodeDefault.xctoolchain/usr/bin/dsymutil");
            let path = if candidate.is_file() {
                Some(candidate.to_path_buf())
            } else {
                std::process::Command::new("xcrun")
                    .args(["--find", "dsymutil"])
                    .output()
                    .ok()
                    .filter(|o| o.status.success())
                    .map(|o| std::path::PathBuf::from(std::string::String::from_utf8_lossy(&o.stdout).trim()))
            }?;
            let real = std::fs::canonicalize(&path).ok()?;
            let dir = real.parent()?;
            Some((dir.to_str()?.to_owned(), real.to_str()?.to_owned()))
        })
        .as_ref()
}

/// Runs `script` confined and makes its record's outputs.
pub fn run_script<'a>(script: &NativeScript<'_>, alloc: Alloc<'a>) -> Result<NativeScriptRun<'a>, NativeScriptError> {
    let c = &script.c;
    let program = || script.program.to_owned();
    let (shown_out, shown_package) = shown_folders(script.env, script.program)?;
    let mut manifest: AString<'a> = String::from_str_in(c.package, alloc);
    manifest.push_str("/Cargo.toml");

    // cargo's variables, the folders made real, and the job variables (`NUM_JOBS`, and
    // `CARGO_MAKEFLAGS` naming the command's jobserver: never keyed, `Env::for_run`); then the C
    // toolchain's; `PATH` empty (no program is found by searching), `TMPDIR` the `OUT_DIR` (the
    // only place it may write).
    let runs_out_dir = host_tools::runs_out_dir(script.host_tools);
    let xcode_dev_dir = if runs_out_dir && Path::new("/Applications/Xcode.app/Contents/Developer/usr/bin/gnumake").is_file() {
        Some("/Applications/Xcode.app/Contents/Developer/usr/bin")
    } else {
        None
    };
    // Apple's GNU Make 3.81 does not support GNU Make 4.4's `--jobserver-fds=fifo:...` syntax;
    // omit the fifo jobserver when Apple `make` (`gnumake`) is in play so `configure`/`make` succeed.
    let jobs = match (script.jobs, xcode_dev_dir) {
        (Some(server), None) => server.jobs(),
        _ => blaze_build_script::Jobs::One,
    };
    let run_env = script.env.for_run(jobs, alloc);
    let pinned_rustc = pinned_rustc_paths();
    let mut vars: AVec<'a, (&str, &str)> = Vec::new_in(alloc);
    for (name, value) in run_env.vars.iter() {
        let value = match name.as_str() {
            "OUT_DIR" => c.out_dir,
            "CARGO_MANIFEST_DIR" => c.package,
            "CARGO_MANIFEST_PATH" => manifest.as_str(),
            "RUSTC" => pinned_rustc.map(|(_, rb)| rb.as_str()).unwrap_or(value.as_str()),
            _ => value.as_str(),
        };
        vars.push((name.as_str(), value));
    }
    let toolchain = c_env(c, alloc);
    let mut cleaned_flags: AVec<'a, AString<'a>> = Vec::new_in(alloc);
    if runs_out_dir {
        for (name, value) in toolchain.iter() {
            if matches!(name.as_str(), "CFLAGS" | "CXXFLAGS") && value.as_str().contains(" -MD") {
                let mut s = String::new_in(alloc);
                for part in value.as_str().split(" -MD") {
                    s.push_str(part);
                }
                cleaned_flags.push(s);
            }
        }
    }
    let mut cleaned_idx = 0usize;
    for (name, value) in toolchain.iter() {
        let val = if runs_out_dir && matches!(name.as_str(), "CFLAGS" | "CXXFLAGS") && value.as_str().contains(" -MD") {
            let s = cleaned_flags[cleaned_idx].as_str();
            cleaned_idx += 1;
            s
        } else {
            value.as_str()
        };
        vars.push((name.as_str(), val));
    }
    // nm-row: each granted tool the table names by a variable, at its real path (tikv-jemalloc-
    // sys's configure: `NM=<shelf>/bin/llvm-nm`, never the machine's /usr/bin/nm shim).
    let tool_env = host_tools::tool_env(script.host_tools, c.shelf.dir, alloc);
    for (name, value) in tool_env.iter() {
        vars.push((name.as_str(), value.as_str()));
    }
    // `PATH`: empty, or exactly the folders of the granted host tools found by search (D71:
    // sha3-asm starts `Command::new("perl")` and reads no variable naming it, so the search must
    // find the granted path; any other program the search finds there is not granted and the
    // sandbox refuses its start). A tool named by a variable adds no folder.
    let mut path: AString<'a> = String::new_in(alloc);
    let folders = host_tools::path_folders(script.host_tools, alloc);
    // D112 (N7): the fact shim's links come first: a probe is answered from the pins, never
    // by a granted folder's program of the same name.
    if let Some(facts) = script.facts {
        path.push_str(&facts.bin);
    }
    let dsymutil = if runs_out_dir { pinned_dsymutil_paths() } else { None };
    if let Some((dsym_dir, _)) = dsymutil {
        if !path.is_empty() {
            path.push(':');
        }
        path.push_str(dsym_dir.as_str());
    }
    for folder in folders.iter() {
        if !path.is_empty() {
            path.push(':');
        }
        path.push_str(folder);
    }
    vars.push(("PATH", path.as_str()));
    vars.push(("TMPDIR", c.out_dir));
    // Hermetic deterministic output for `vergen` / `smolgen` build scripts (zero ambient `git`).
    vars.push(("VERGEN_IDEMPOTENT", "true"));
    if xcode_dev_dir.is_some() {
        vars.push(("DEVELOPER_DIR", "/Applications/Xcode.app/Contents/Developer"));
    }
    if let Some(facts) = script.facts {
        vars.push((fact_shim::FOLDER_VAR, facts.folder.as_str()));
    }

    // D112: a script granted a tool that runs what the script built under OUT_DIR (sh running
    // configure's test programs) links those programs: the pinned linker too (the shelf's
    // clang.cfg says `-fuse-ld=lld`; clang starts `ld64.lld`, a link to `lld`), and it may start
    // what is under OUT_DIR. Every other script is granted neither.
    let linker: &[&str] = if runs_out_dir { &["bin/ld64.lld", "bin/ld.lld", "bin/lld"] } else { &[] };
    let mut exec: AVec<'a, AString<'a>> = Vec::new_in(alloc);
    for name in ["bin/clang-23", "bin/clang", "bin/clang++", "bin/llvm-ar"].iter().chain(linker) {
        let mut path: AString<'a> = String::from_str_in(c.shelf.dir, alloc);
        path.push('/');
        path.push_str(name);
        // Linux Landlock opens every plan path (`O_PATH`) when building the ruleset; skip shelf
        // binaries absent on disk (e.g. empty test shelves or platform-specific linkers) without
        // weakening confinement.
        if Path::new(path.as_str()).exists() {
            exec.push(path);
        }
    }
    // Each tool and the programs it starts itself (perl's piped `open`: /bin/sh, /bin/bash), at
    // their real paths: a shelf row's (nm-row: nm, the shelf's llvm-nm) under the shelf.
    for tool in script.host_tools {
        for program in tool.programs_in(c.shelf.dir, alloc) {
            if Path::new(program.as_str()).exists() {
                exec.push(program);
            }
        }
    }
    let exec: AVec<'a, &str> = {
        let mut out = Vec::new_in(alloc);
        for path in exec.iter() {
            out.push(path.as_str());
        }
        // Pinned nightly `rustc` binary so native Rust build scripts (`autocfg`, `serde_core`,
        // `thiserror`, `libc`, etc.) can probe the pinned compiler hermetically.
        if let Some((_, rustc_bin)) = pinned_rustc {
            out.push(rustc_bin.as_str());
        }
        if let Some((_, dsym_bin)) = dsymutil {
            out.push(dsym_bin.as_str());
        }
        if xcode_dev_dir.is_some() {
            out.push("/Applications/Xcode.app/Contents/Developer/usr/bin/gnumake");
        }
        if runs_out_dir && Path::new("/usr/bin/cmp").is_file() {
            out.push("/usr/bin/cmp");
        }
        // The fact shim (N7), started through its links.
        if let Some(facts) = script.facts {
            out.push(facts.shim.as_str());
            for link in facts.links.iter() {
                out.push(link.as_str());
            }
        }
        out
    };
    let mut reads: AVec<'a, &str> = Vec::new_in(alloc);
    if script.confined.is_none() {
        reads.push(c.package);
    }
    reads.extend_from_slice(&[c.shelf.dir, c.sysroot]);
    if let Some((sysroot_dir, _)) = pinned_rustc {
        reads.push(sysroot_dir.as_str());
    }
    if let Some((dsym_dir, _)) = dsymutil {
        reads.push(dsym_dir.as_str());
    }
    if let Some(dev_bin) = xcode_dev_dir {
        reads.push(dev_bin);
    }
    reads.extend_from_slice(script.reads);
    for tool in script.host_tools {
        for folder in tool.read_folders() {
            reads.push(folder);
        }
    }
    let out_only = [c.out_dir];
    let exec_under: &[&str] = if runs_out_dir { &out_only } else { &[] };

    // D177: the run is a hermetic spec at `Level::Inputs`. Each read folder is a tree root
    // named by the key; `OUT_DIR` the output root; the jobserver's fifo and the fact shim's
    // folder channels (neither an input nor an output: tokens, and questions the record keeps
    // by `native_facts::reads`); the granted programs helpers; `OUT_DIR` a helper folder when a
    // tool runs what the script built there (D112).
    let tree = RootKind::Tree { tree: Digest(script.key.0) };
    let mut roots: AVec<'_, Root<'_>> = Vec::new_in(alloc);
    let mut spellings: AVec<'_, &str> = Vec::new_in(alloc);
    let facts_folder = script.facts.map(|facts| facts.folder.as_str());
    for folder in reads.iter() {
        // One mount a spelling: a folder named twice (a tool's and the shelf's) is one root.
        // The fact shim's folder is a channel, not a root (below).
        if spellings.scan_for(folder).is_none() && Some(*folder) != facts_folder {
            roots.push(Root { name: "a folder a native build script reads", kind: tree });
            spellings.push(folder);
        }
    }
    let out_err = |why: &str| NativeScriptError::Output { program: program(), why: why.to_owned() };
    let all_glob = [blaze_roots::Glob::new("**").map_err(|_| out_err("invalid confined glob"))?];
    let mut package_root: Option<(RootId, &[&str])> = None;
    if let Some(confined) = script.confined {
        if spellings.scan_for(&confined.workspace).is_some() || Some(confined.workspace) == facts_folder {
            return Err(out_err("confined workspace overlaps another mount"));
        }
        let id = RootId(u8::try_from(roots.len()).map_err(|_| out_err("more than 255 folders read"))?);
        roots.push(Root { name: "confined workspace files", kind: RootKind::Package { globs: &all_glob } });
        spellings.push(confined.workspace);
        package_root = Some((id, confined.files));
    }
    roots.push(Root { name: "OUT_DIR", kind: RootKind::Output });
    spellings.push(c.out_dir);
    let output = RootId(u8::try_from(roots.len() - 1).map_err(|_| out_err("more than 255 folders read"))?);
    let mut mounts: AVec<'_, Mount<'_>> = Vec::new_in(alloc);
    for (at, spelling) in spellings.iter().enumerate() {
        mounts.push(Mount { spelling: spelling.as_bytes(), root: RootId(at as u8) });
    }
    let roots = Roots::new(&roots).map_err(|e| NativeScriptError::Output { program: program(), why: format!("its roots: {e:?}") })?;
    let mut channels: AVec<'_, &str> = Vec::new_in(alloc);
    if let Some(facts) = script.facts {
        channels.push(facts.folder.as_str());
    }
    if let Some(server) = script.jobs {
        channels.push(server.fifo());
    }
    // The environment in one spelling: sorted by name, a later pair of one name replacing an
    // earlier (the toolchain's and the run's own variables are pushed after cargo's, and were
    // the ones a started process saw: std's `Command::env` in native_sandbox.rs kept the last
    // too). Variable for variable what native_sandbox.rs's start was given (the same `vars`,
    // built above unchanged by the D177 move):
    // - every pair of `script.env` (cargo's: `TARGET`, `HOST`, `PROFILE`, `OPT_LEVEL`, `DEBUG`,
    //   `RUSTC`, `CARGO_PKG_*`, `CARGO_CFG_*`, `CARGO_FEATURE_*`, `DEP_*`, ...), with `OUT_DIR`,
    //   `CARGO_MANIFEST_DIR` and `CARGO_MANIFEST_PATH` made the real folders;
    // - the job variables (`Env::for_run`): `NUM_JOBS`, and `CARGO_MAKEFLAGS` with a jobserver;
    // - the C toolchain's (`c_env`: `CC`, `CXX`, `AR`, `CFLAGS`, `CXXFLAGS`, `SDKROOT`,
    //   `MACOSX_DEPLOYMENT_TARGET`, `LIBCLANG_PATH`, `BINDGEN_EXTRA_CLANG_ARGS`,
    //   `SOURCE_DATE_EPOCH`: the clock below is read from it);
    // - each granted tool's variable (`host_tools::tool_env`, `NM`, ...);
    // - `PATH` (the fact shim's links, then the granted tools' folders), `TMPDIR` (`OUT_DIR`),
    //   and the fact shim's folder variable when it is used.
    // The fact-shim test (tests/native_fact_shim.rs) asserts what the confined script saw.
    vars.sort_by(|a, b| a.0.cmp(b.0));
    let mut env: AVec<'_, (&str, &str)> = Vec::new_in(alloc);
    for &pair in vars.iter() {
        match env.last_mut() {
            Some(last) if last.0 == pair.0 => *last = pair,
            _ => env.push(pair),
        }
    }
    let clock = match env.iter().find(|(name, _)| *name == "SOURCE_DATE_EPOCH") {
        // A value that is no decimal leaves the spec's own check to refuse it by name.
        Some((_, value)) => value.parse::<u64>().map_or(ClockSpec::NeverAsks, |unix_secs| ClockSpec::Fixed { unix_secs }),
        None => ClockSpec::NeverAsks,
    };
    let (world, _) = native_world(Path::new(c.out_dir)).map_err(|e| NativeScriptError::Files { folder: c.out_dir.to_owned(), why: e.to_string() })?;
    let no_host = |missing: bstd::io::Missing| NativeScriptError::NoHost { program: program(), capability: missing.capability };
    let spawner = world.need_host().and_then(|host| host.spawner()).map_err(no_host)?;
    let driver_clock = world.need_clock().map_err(no_host)?;
    let deadline = Deadline::after(driver_clock.now().since_start(), script.deadline);
    let cx = world
        .cx(script.deadline.saturating_add(Duration::from_secs(60)), TASK_FRAME)
        .map_err(|_| NativeScriptError::Files { folder: c.out_dir.to_owned(), why: "its world stopped".to_owned() })?;
    let hermetic = Hermetic::new(HermeticCapOpts { spawner, clock: driver_clock });
    let most_stream = u32::try_from(script.limits.output)
        .map_err(|_| NativeScriptError::Output { program: program(), why: format!("an output limit of {} bytes is past what a stream keeps (4 GiB)", script.limits.output) })?;
    let spec = HermeticSpec::new(HermeticOpts {
        program: Program::Stored { name: script.program, digest: Digest(script.key.0), path: script.program },
        args: &[],
        env: &env,
        roots,
        mounts: &mounts,
        cwd: c.package,
        helpers: &exec,
        helper_folders: exec_under,
        channels: &channels,
        clock,
        seed: 0,
        budget: NATIVE_BUDGET.of(script.deadline),
        deadline,
        streams: HermeticStreams { stdout: Capture::Keep { most: most_stream }, stderr: Capture::Keep { most: most_stream } },
        level: Level::Inputs,
        report: ReportSpec::WholeRoots,
    })
    .map_err(|error| NativeScriptError::Refused { program: program(), fault: error.fault })?;
    let files = OutDir { root: output, folder: c.out_dir, package_root };
    // Lent before the child starts, ended after it and its process group are gone (the run
    // kills the group): a token it kept comes back by count (jobserver.rs).
    let lent = match script.jobs {
        Some(server) => Some(server.lend().map_err(|error| NativeScriptError::Jobs { program: program(), error })?),
        None => None,
    };
    let started = Instant::now();
    let ran = hermetic.run(&cx, &spec, &files, alloc);
    if let Some(lent) = lent {
        lent.end().map_err(|error| NativeScriptError::Jobs { program: program(), error })?;
    }
    let ran = ran.map_err(|error| NativeScriptError::Refused { program: program(), fault: error.fault })?;
    let ran = Ran { exit: ran.exit, stdout: ran.stdout.to_vec(), stderr: ran.stderr.to_vec() };
    let wall = started.elapsed();
    if ran.exit != Exit::Exited(0) {
        // `cc` passes the compiler's own messages on as `cargo:warning=` lines on standard
        // output: they come first, then standard error's end.
        let out = std::string::String::from_utf8_lossy(&ran.stdout);
        let err = std::string::String::from_utf8_lossy(&ran.stderr);
        let warned = out.lines().filter(|l| l.contains("warning=") && (l.contains("error") || l.contains("fatal")));
        let text: std::vec::Vec<&str> = warned.take(10).chain(err.lines()).collect();
        let tail: std::vec::Vec<&str> = text.iter().copied().rev().take(30).collect();
        let tail: std::vec::Vec<&str> = tail.into_iter().rev().collect();
        let out_tail: std::vec::Vec<&str> = out.lines().rev().take(10).collect();
        let out_tail: std::vec::Vec<&str> = out_tail.into_iter().rev().collect();
        // What confined it, rendered again from the same spec (one plan, one rendering).
        let confinement = bstd::procs::hermetic::confinement(&spec, &files, alloc)
            .map_err(|fault| NativeScriptError::Refused { program: program(), fault })?;
        let gave_no_reason = matches!(ran.exit, Exit::Signaled(_)) || (ran.stdout.is_empty() && ran.stderr.is_empty());
        return Err(NativeScriptError::Failed {
            program: program(),
            exit: ran.exit,
            stderr: tail.join("\n"),
            stdout: out_tail.join("\n"),
            confinement: bstd::procs::hermetic::confinement_digest(&confinement),
            profile: gave_no_reason.then(|| std::string::String::from_utf8_lossy(&confinement).into_owned()),
        });
    }

    let mut rerun_if_changed: AVec<'a, AString<'a>> = Vec::new_in(alloc);
    for raw_line in ran.stdout.split(|&b| b == b'\n') {
        let line = raw_line.strip_suffix(b"\r").unwrap_or(raw_line);
        if let blaze_build_script::instr::Line::Instr(_, blaze_build_script::instr::Instr::RerunIfChanged(path)) = blaze_build_script::instr::parse(line) {
            rerun_if_changed.push(String::from_str_in(path, alloc));
        }
    }

    // The laid trees first (brief N8): they are the most specific folders; then `OUT_DIR`, then
    // the package.
    let mut pairs: AVec<'a, (&str, &str)> = Vec::new_in(alloc);
    pairs.extend_from_slice(script.shown);
    pairs.push((c.out_dir, shown_out));
    pairs.push((c.package, shown_package));
    let stdout = map_paths(&ran.stdout, &pairs, alloc);
    let output_limits = OutputLimits { line: script.limits.line as usize, output: script.limits.output as usize };
    let mut outputs = Outputs::read(&stdout, shown_out, output_limits, alloc).map_err(|e| NativeScriptError::Output { program: program(), why: format!("{e:?}") })?;
    let file_limits = FileLimits { bytes: script.limits.out_dir_bytes, files: script.limits.out_dir_files as usize };

    let mut sysroot_prefix: AString<'a> = String::from_str_in(c.sysroot, alloc);
    sysroot_prefix.push('/');
    let mut headers: std::vec::Vec<(std::string::String, [u8; 32])> = std::vec::Vec::new();
    let mut other_reads = 0usize;
    let listed = listed_under(c.out_dir)?;
    // clang's `-MD` dependency files (a plain file named `.d`) are read here, each alone, and
    // removed; every other entry is a stored file of the record ([`store_out_dir`]): its
    // bytes are never read whole.
    let (depfiles, kept): (std::vec::Vec<&Listed>, std::vec::Vec<&Listed>) = listed.iter().partition(|file| file.kind == bstd::fs::EntryKind::File && file.name.ends_with(".d"));
    let kept: std::vec::Vec<Listed> = kept.into_iter().cloned().collect();
    for depfile in depfiles {
        let name = depfile.name.as_str();
        let (io, root) = native_world(Path::new(c.out_dir)).map_err(|e| NativeScriptError::Files { folder: c.out_dir.to_owned(), why: e.to_string() })?;
        let cx = io.cx(Duration::from_secs(60), TASK_FRAME).map_err(|_| NativeScriptError::Files { folder: c.out_dir.to_owned(), why: "its world stopped".to_owned() })?;
        let path = bstd::path::Path::new(name).map_err(|_| NativeScriptError::Files { folder: c.out_dir.to_owned(), why: format!("{name}: not a relative path") })?;
        let mut bytes = Bytes::new_in(cx.frame(), "a native build script's dependency file");
        root.read_all(&cx, &path, &mut bytes).map_err(|e| NativeScriptError::Files { folder: c.out_dir.to_owned(), why: format!("{name}: {e}") })?;
        let text = std::string::String::from_utf8_lossy(bytes.as_slice());
        let mut read: AVec<'a, AString<'a>> = Vec::new_in(alloc);
        depfile_reads(&text, &mut read, alloc);
        for path in read.iter() {
            if let Some(under) = path.strip_prefix(sysroot_prefix.as_str()) {
                headers.push((under.to_owned(), [0; 32]));
            } else if path.starts_with('/') && !path.starts_with(c.package) && !path.starts_with(c.out_dir) && !path.starts_with(c.shelf.dir) {
                other_reads += 1;
            }
        }
        remove(c.out_dir, name)?;
    }
    store_out_dir(c.out_dir, script.files, &kept, &mut outputs, file_limits, alloc)?;
    headers.sort();
    headers.dedup();
    for (under, digest) in headers.iter_mut() {
        let (io, root) = native_world(Path::new(c.sysroot)).map_err(|e| NativeScriptError::Files { folder: c.sysroot.to_owned(), why: e.to_string() })?;
        let cx = io.cx(Duration::from_secs(60), TASK_FRAME).map_err(|_| NativeScriptError::Files { folder: c.sysroot.to_owned(), why: "its world stopped".to_owned() })?;
        let path = bstd::path::Path::new(under).map_err(|_| NativeScriptError::Files { folder: c.sysroot.to_owned(), why: format!("{under}: not a relative path") })?;
        let mut bytes = Bytes::new_in(cx.frame(), "a sysroot header a native script's compile read");
        root.read_all(&cx, &path, &mut bytes).map_err(|e| NativeScriptError::Files { folder: c.sysroot.to_owned(), why: format!("{under}: {e}") })?;
        *digest = sha256(bytes.as_slice());
    }
    let mut keyed: std::vec::Vec<(&str, [u8; 32])> = headers.iter().map(|(p, d)| (p.as_str(), *d)).collect();
    let digest = headers_digest(&mut keyed);
    Ok(NativeScriptRun { outputs, rerun_if_changed, headers: digest, headers_read: headers.len(), other_reads, stderr: ran.stderr, wall })
}
