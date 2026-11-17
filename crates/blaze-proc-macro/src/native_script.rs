//! D65/D69: one native build script run end to end, confined: its environment (cargo's, with
//! the real folders in place of the key's, and the pinned C toolchain's from
//! [`blaze_build_script::toolchain::c_env`]), the run in [`crate::native_sandbox`], and what it
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
    native::Grant,
    outputs::{FileLimits, FileMode, Outputs},
    toolchain::{CInputs, c_env, depfile_reads, headers_digest, map_paths},
};
use blaze_ids::ContentId;
use blaze_own::{AString, AVec, Alloc, String, Vec};
use bstd::{fs::Durable, mem::Bytes, mem::TASK_FRAME};

use crate::{
    native_sandbox::{NativeRefusal, NativeRun, run},
    record::native_world,
};

/// One script to run.
#[derive(Clone, Copy, Debug)]
pub struct NativeScript<'s> {
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
    Refused { program: std::string::String, refusal: NativeRefusal },
    Failed { program: std::string::String, code: Option<i32>, stderr: std::string::String },
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
            NativeScriptError::Refused { program, refusal } => write!(f, "{program}: the native build script was refused: {refusal}"),
            NativeScriptError::Failed { program, code, stderr } => {
                write!(f, "{program}: the native build script failed (exit {code:?}; not retried as wasm, rule 3); its standard error ends:\n{stderr}")
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
    let limits = bstd::fs::WalkLimits { folders: 4096, entries: 1 << 18, depth: 64 };
    let mut out = std::vec::Vec::new();
    root.walk(&cx, None, limits, cx.frame(), |entries, enter| {
        for i in 0..entries.len() {
            let kind = entries.kind(i);
            if kind == bstd::fs::EntryKind::Dir {
                continue;
            }
            let path = entries.path(i).map_err(|e| format!("{}: not a path: {e:?}", entries.name(i)))?;
            out.push(Listed { name: path.as_str().to_owned(), kind, len: entries.len_of(i), executable: entries.executable(i) });
        }
        enter.enter_all();
        Ok::<(), std::string::String>(())
    })
    .map_err(|e| failed(e.to_string()))?;
    // Sorted by the paths' bytes, as the outputs record has them (the walk's own order
    // compares part by part).
    out.sort_unstable_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
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
    let jobs = match script.jobs {
        Some(server) => server.jobs(),
        None => blaze_build_script::Jobs::One,
    };
    let run_env = script.env.for_run(jobs, alloc);
    let mut vars: AVec<'a, (&str, &str)> = Vec::new_in(alloc);
    for (name, value) in run_env.vars.iter() {
        let value = match name.as_str() {
            "OUT_DIR" => c.out_dir,
            "CARGO_MANIFEST_DIR" => c.package,
            "CARGO_MANIFEST_PATH" => manifest.as_str(),
            _ => value.as_str(),
        };
        vars.push((name.as_str(), value));
    }
    let toolchain = c_env(c, alloc);
    for (name, value) in toolchain.iter() {
        vars.push((name.as_str(), value.as_str()));
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
    for folder in folders.iter() {
        if !path.is_empty() {
            path.push(':');
        }
        path.push_str(folder);
    }
    vars.push(("PATH", path.as_str()));
    vars.push(("TMPDIR", c.out_dir));
    if let Some(facts) = script.facts {
        vars.push((fact_shim::FOLDER_VAR, facts.folder.as_str()));
    }

    // D112: a script granted a tool that runs what the script built under OUT_DIR (sh running
    // configure's test programs) links those programs: the pinned linker too (the shelf's
    // clang.cfg says `-fuse-ld=lld`; clang starts `ld64.lld`, a link to `lld`), and it may start
    // what is under OUT_DIR. Every other script is granted neither.
    let runs_out_dir = host_tools::runs_out_dir(script.host_tools);
    let linker: &[&str] = if runs_out_dir { &["bin/ld64.lld", "bin/lld"] } else { &[] };
    let mut exec: AVec<'a, AString<'a>> = Vec::new_in(alloc);
    for name in ["bin/clang-23", "bin/clang", "bin/clang++", "bin/llvm-ar"].iter().chain(linker) {
        let mut path: AString<'a> = String::from_str_in(c.shelf.dir, alloc);
        path.push('/');
        path.push_str(name);
        exec.push(path);
    }
    // Each tool and the programs it starts itself (perl's piped `open`: /bin/sh, /bin/bash), at
    // their real paths: a shelf row's (nm-row: nm, the shelf's llvm-nm) under the shelf.
    for tool in script.host_tools {
        for program in tool.programs_in(c.shelf.dir, alloc) {
            exec.push(program);
        }
    }
    let exec: AVec<'a, &str> = {
        let mut out = Vec::new_in(alloc);
        for path in exec.iter() {
            out.push(path.as_str());
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
    reads.extend_from_slice(&[c.package, c.shelf.dir, c.sysroot]);
    reads.extend_from_slice(script.reads);
    for tool in script.host_tools {
        for folder in tool.read_folders() {
            reads.push(folder);
        }
    }
    // The fact shim's folder: its links and pins read; only its log written (N7).
    let mut writes: AVec<'a, &str> = Vec::new_in(alloc);
    writes.push(c.out_dir);
    if let Some(facts) = script.facts {
        reads.push(facts.folder.as_str());
        writes.push(facts.log_dir.as_str());
    }
    // The jobserver's fifo, read and written (a client opens it so: `jobserver`'s `fifo:`).
    if let Some(server) = script.jobs {
        writes.push(server.fifo());
    }
    let out_only = [c.out_dir];
    let exec_under: &[&str] = if runs_out_dir { &out_only } else { &[] };
    // Lent before the child starts, ended after it and its process group are gone (`run`
    // kills the group): a token it kept comes back by count (jobserver.rs).
    let lent = match script.jobs {
        Some(server) => Some(server.lend().map_err(|error| NativeScriptError::Jobs { program: program(), error })?),
        None => None,
    };
    let started = Instant::now();
    let ran = run(
        &NativeRun {
            grant: Grant { program: script.program, exec: &exec, read: &reads, write: &writes },
            args: &[],
            env: &vars,
            dir: c.package,
            deadline: script.deadline,
            exec_under,
        },
        alloc,
    );
    if let Some(lent) = lent {
        lent.end().map_err(|error| NativeScriptError::Jobs { program: program(), error })?;
    }
    let ran = ran.map_err(|refusal| NativeScriptError::Refused { program: program(), refusal })?;
    let wall = started.elapsed();
    if ran.code != Some(0) {
        // `cc` passes the compiler's own messages on as `cargo:warning=` lines on standard
        // output: they come first, then standard error's end.
        let out = std::string::String::from_utf8_lossy(&ran.stdout);
        let err = std::string::String::from_utf8_lossy(&ran.stderr);
        let warned = out.lines().filter(|l| l.contains("warning=") && (l.contains("error") || l.contains("fatal")));
        let text: std::vec::Vec<&str> = warned.take(10).chain(err.lines()).collect();
        let tail: std::vec::Vec<&str> = text.iter().copied().rev().take(30).collect();
        let tail: std::vec::Vec<&str> = tail.into_iter().rev().collect();
        return Err(NativeScriptError::Failed { program: program(), code: ran.code, stderr: tail.join("\n") });
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
    Ok(NativeScriptRun { outputs, headers: digest, headers_read: headers.len(), other_reads, stderr: ran.stderr, wall })
}
