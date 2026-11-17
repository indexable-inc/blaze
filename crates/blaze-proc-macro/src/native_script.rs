//! D65/D69: one native build script run end to end, confined: its environment (cargo's, with
//! the real folders in place of the key's, and the pinned C toolchain's from
//! [`blaze_build_script::toolchain::c_env`]), the run in [`crate::native_sandbox`], and what it
//! left made into the same outputs record a wasm script's run gives ([`Outputs`]):
//!
//! - its standard output with the real `OUT_DIR` and package folder written as the key's
//!   (`/target/debug/build/<name>-<config>/out`, `/package` or `/workspace/<dir>`), so every run of one key gives
//!   the same bytes (rule 25);
//! - every file it left under `OUT_DIR`, but clang's `-MD` dependency files, which are read
//!   into the headers read (each sysroot file's path under the sysroot and the SHA-256 of its
//!   bytes, [`headers_digest`]) and removed: they hold real paths.
//!
//! Files are read through the effect layer (a world rooted at each folder). A non-zero exit is
//! a typed failure with the end of its standard error; it is never tried again as wasm (rule 3).

use core::time::Duration;
use std::{path::Path, time::Instant};

use blaze_build_script::{
    Env,
    instr::OutputLimits,
    key::RunLimits,
    host_tools::{self, HostTool},
    native::Grant,
    outputs::{FileLimits, Outputs},
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
    /// More folders it reads: its dependencies' outputs.
    pub reads: &'s [&'s str],
    /// The host tools it is granted (D71, [`blaze_build_script::host_tools::granted`]), each
    /// `path` the resolved real path: started by a `PATH` of exactly their folders, read and
    /// executed, their `reads` read-only. Empty for nearly every script.
    pub host_tools: &'s [HostTool<'s>],
    pub limits: RunLimits,
    pub deadline: Duration,
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

/// The bytes of every file under `folder`, sorted by path, through the effect layer.
pub(crate) fn files_under(folder: &str) -> Result<std::vec::Vec<(std::string::String, std::vec::Vec<u8>)>, NativeScriptError> {
    let failed = |why: std::string::String| NativeScriptError::Files { folder: folder.to_owned(), why };
    let (io, root) = native_world(Path::new(folder)).map_err(|e| failed(e.to_string()))?;
    let cx = io.cx(Duration::from_secs(60), TASK_FRAME).map_err(|_| failed("its world stopped".to_owned()))?;
    // `folder` is one build script's `OUT_DIR`: everything the script left there is its
    // output, so the walk enters every folder of it (never the package's sources or a
    // checkout: those are not under it). The limits fit the largest outputs met (a C
    // library's objects and generated headers: a few thousand files in a few dozen folders)
    // with room, and refuse, naming the folder, a script that left a tree no record should
    // hold: 4,096 folders, 262,144 entries in all, 64 levels deep.
    let limits = bstd::fs::WalkLimits { folders: 4096, entries: 1 << 18, depth: 64 };
    let mut paths = std::vec::Vec::new();
    root.walk(&cx, None, limits, cx.frame(), |entries, enter| {
        for i in 0..entries.len() {
            // A folder is walked, not read; anything else is an output, as it was listed.
            if entries.kind(i) == bstd::fs::EntryKind::Dir {
                continue;
            }
            paths.push(entries.path(i).map_err(|e| format!("{}: not a path: {e:?}", entries.name(i)))?);
        }
        enter.enter_all();
        Ok::<(), std::string::String>(())
    })
    .map_err(|e| failed(e.to_string()))?;
    let mut out = std::vec::Vec::new();
    for path in &paths {
        let cx = io.cx(Duration::from_secs(60), TASK_FRAME).map_err(|_| failed("its world stopped".to_owned()))?;
        let mut bytes = Bytes::new_in(cx.frame(), "a native build script's output file");
        root.read_all(&cx, path, &mut bytes).map_err(|e| failed(format!("{}: {e}", path.as_str())))?;
        out.push((path.as_str().to_owned(), bytes.as_slice().to_vec()));
    }
    // Sorted by the paths' bytes, as the outputs record has always had them (the walk's own
    // order compares part by part).
    out.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
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

    // cargo's variables, the folders made real; then the C toolchain's; `PATH` empty (no
    // program is found by searching), `TMPDIR` the `OUT_DIR` (the only place it may write).
    let mut vars: AVec<'a, (&str, &str)> = Vec::new_in(alloc);
    for (name, value) in script.env.vars.iter() {
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
    // `PATH`: empty, or exactly the folders of the granted host tools (D71: sha3-asm starts
    // `Command::new("perl")` and reads no variable naming it, so the search must find the
    // granted path; any other program the search finds there is not granted and the sandbox
    // refuses its start).
    let mut path: AString<'a> = String::new_in(alloc);
    let mut folders: AVec<'a, &str> = Vec::new_in(alloc);
    for tool in script.host_tools {
        let (folder, _) = tool.path.rsplit_once('/').unwrap_or(("", tool.path));
        folders.push(if folder.is_empty() { "/" } else { folder });
    }
    folders.sort_unstable();
    folders.dedup();
    for (at, folder) in folders.iter().enumerate() {
        if at > 0 {
            path.push(':');
        }
        path.push_str(folder);
    }
    vars.push(("PATH", path.as_str()));
    vars.push(("TMPDIR", c.out_dir));

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
    let exec: AVec<'a, &str> = {
        let mut out = Vec::new_in(alloc);
        for path in exec.iter() {
            out.push(path.as_str());
        }
        // Each tool and the programs it starts itself (perl's piped `open`: /bin/sh, /bin/bash).
        for tool in script.host_tools {
            out.extend(tool.programs());
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
    let writes = [c.out_dir];
    let out_only = [c.out_dir];
    let exec_under: &[&str] = if runs_out_dir { &out_only } else { &[] };
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
    )
    .map_err(|refusal| NativeScriptError::Refused { program: program(), refusal })?;
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

    let pairs = [(c.out_dir, shown_out), (c.package, shown_package)];
    let stdout = map_paths(&ran.stdout, &pairs, alloc);
    let output_limits = OutputLimits { line: script.limits.line as usize, output: script.limits.output as usize };
    let mut outputs = Outputs::read(&stdout, shown_out, output_limits, alloc).map_err(|e| NativeScriptError::Output { program: program(), why: format!("{e:?}") })?;
    let file_limits = FileLimits { bytes: script.limits.out_dir_bytes, files: script.limits.out_dir_files as usize };

    let mut sysroot_prefix: AString<'a> = String::from_str_in(c.sysroot, alloc);
    sysroot_prefix.push('/');
    let mut headers: std::vec::Vec<(std::string::String, [u8; 32])> = std::vec::Vec::new();
    let mut other_reads = 0usize;
    for (name, bytes) in files_under(c.out_dir)? {
        if !name.ends_with(".d") {
            outputs.add_file(&name, &bytes, file_limits, alloc).map_err(|e| NativeScriptError::Output { program: program(), why: format!("{name}: {e:?}") })?;
            continue;
        }
        let text = std::string::String::from_utf8_lossy(&bytes);
        let mut read: AVec<'a, AString<'a>> = Vec::new_in(alloc);
        depfile_reads(&text, &mut read, alloc);
        for path in read.iter() {
            if let Some(under) = path.strip_prefix(sysroot_prefix.as_str()) {
                headers.push((under.to_owned(), [0; 32]));
            } else if path.starts_with('/') && !path.starts_with(c.package) && !path.starts_with(c.out_dir) && !path.starts_with(c.shelf.dir) {
                other_reads += 1;
            }
        }
        remove(c.out_dir, &name)?;
    }
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
