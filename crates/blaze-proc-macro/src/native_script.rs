//! D65/D69: one native build script run end to end, confined: its environment (cargo's, with
//! the real folders in place of the key's, and the pinned C toolchain's from
//! [`blaze_build_script::toolchain::c_env`]), the run in [`crate::native_sandbox`], and what it
//! left made into the same outputs record a wasm script's run gives ([`Outputs`]):
//!
//! - its standard output with the real `OUT_DIR` and package folder written as the key's
//!   (`/target/debug/build/<name>-<config>/out`, `/workspace`), so every run of one key gives
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
        }
    }
}

impl std::error::Error for NativeScriptError {}

/// The bytes of every file under `folder`, sorted by path, through the effect layer.
fn files_under(folder: &str) -> Result<std::vec::Vec<(std::string::String, std::vec::Vec<u8>)>, NativeScriptError> {
    let failed = |why: std::string::String| NativeScriptError::Files { folder: folder.to_owned(), why };
    let (io, root) = native_world(Path::new(folder)).map_err(|e| failed(e.to_string()))?;
    let cx = io.cx(Duration::from_secs(60), TASK_FRAME).map_err(|_| failed("its world stopped".to_owned()))?;
    let listed = root.list(&cx, cx.frame()).map_err(|e| failed(e.to_string()))?;
    let mut out = std::vec::Vec::new();
    for (path, meta) in listed.iter() {
        if meta.dir {
            continue;
        }
        let cx = io.cx(Duration::from_secs(60), TASK_FRAME).map_err(|_| failed("its world stopped".to_owned()))?;
        let mut bytes = Bytes::new_in(cx.frame(), "a native build script's output file");
        root.read_all(&cx, path, &mut bytes).map_err(|e| failed(format!("{}: {e}", path.as_str())))?;
        out.push((path.as_str().to_owned(), bytes.as_slice().to_vec()));
    }
    Ok(out)
}

fn remove(folder: &str, name: &str) -> Result<(), NativeScriptError> {
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
    let shown_out = script.env.get("OUT_DIR").unwrap_or("/out");
    let shown_package = script.env.get("CARGO_MANIFEST_DIR").unwrap_or("/workspace");
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
    vars.push(("PATH", ""));
    vars.push(("TMPDIR", c.out_dir));

    let mut exec: AVec<'a, AString<'a>> = Vec::new_in(alloc);
    for name in ["bin/clang-23", "bin/clang", "bin/clang++", "bin/llvm-ar"] {
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
        out
    };
    let mut reads: AVec<'a, &str> = Vec::new_in(alloc);
    reads.extend_from_slice(&[c.package, c.shelf.dir, c.sysroot]);
    reads.extend_from_slice(script.reads);
    let writes = [c.out_dir];
    let started = Instant::now();
    let ran = run(
        &NativeRun {
            grant: Grant { program: script.program, exec: &exec, read: &reads, write: &writes },
            args: &[],
            env: &vars,
            dir: c.package,
            deadline: script.deadline,
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
