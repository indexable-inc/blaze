//! Native Zero-Wasm Proc-Macro & Build-Script Execution Bridge (D175 fast path).
//!
//! Eliminates the `wasm32-wasip1` + `-Zbuild-std` bootstrap bottleneck (which breaks
//! `synstructure` / `asn1-rs-derive`, `cc`, `cmake`, `bindgen`, `clang-sys`, and `libloading` on
//! real workspaces like `tempo`) by:
//!
//! 1. **Warm Native Proc-Macro Server (`NativeProcMacroServer`)**: talks over stdio newline-JSON
//!    to the pinned toolchain's native `rust-analyzer-proc-macro-srv` (API v6) to inspect and
//!    expand native host `.dylib`/`.so` proc-macro crates directly — zero Wasmtime, zero
//!    `-Zbuild-std`, 100% compatibility with `synstructure`/`serde_derive`/`alloy-sol-macro`.
//! 2. **Native Build-Script Confined Execution (`run_native_build_script`)**: runs native host
//!    `build-script-build` binaries inside macOS Seatbelt confinement (or direct confined host
//!    execution), capturing `cargo:` directives and `OUT_DIR` files deterministically.
//! 3. **Content-Addressed Native Artifact Cache (`NativeArtifactIndex`)**: indexes native `.dylib`
//!    proc macros and native `build-script-build` binaries by SHA-256 so repeated runs hit cache
//!    in microseconds.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::Duration;

use blaze_core::wait::Deadline;
use blaze_expand::ProcKind;
use blaze_own::{AVec, Alloc, Vec as OVec};
use blaze_roots::{Digest, Mount, Root, RootId, RootKind, Roots};
use bstd::mem::TASK_FRAME;
use bstd::procs::{
    Exit,
    hermetic::{
        Budget, Capture, ClockSpec, Hermetic, HermeticCapOpts, HermeticOpts, HermeticSpec,
        HermeticStreams, Level, Program, ReportSpec,
    },
};

use crate::native_script::OutDir;
use crate::record::native_world;

/// Errors returned by the native zero-Wasm proc-macro / build-script bridge.
#[derive(Debug)]
pub enum NativeBridgeError {
    /// Failed to locate `rust-analyzer-proc-macro-srv` in the pinned toolchain sysroot.
    ServerNotFound(String),
    /// Failed to spawn or communicate with the native server process.
    Io(std::io::Error),
    /// The native server returned an unexpected or malformed JSON reply.
    Protocol(String),
    /// A native proc-macro or build-script reported a failure.
    Failed(String),
}

impl core::fmt::Display for NativeBridgeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::ServerNotFound(msg) => write!(f, "native proc-macro server not found: {msg}"),
            Self::Io(e) => write!(f, "native comptime I/O error: {e}"),
            Self::Protocol(msg) => write!(f, "native proc-macro protocol error: {msg}"),
            Self::Failed(msg) => write!(f, "native comptime execution failed: {msg}"),
        }
    }
}

impl std::error::Error for NativeBridgeError {}

impl From<std::io::Error> for NativeBridgeError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// One exported proc macro discovered from a native `.dylib` / `.so`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct NativeExport {
    pub name: String,
    pub kind: ProcKind,
}

/// Locates `rust-analyzer-proc-macro-srv` in the pinned nightly sysroot (or `rustc --print sysroot`).
pub fn find_proc_macro_srv() -> Result<PathBuf, NativeBridgeError> {
    if let Ok(explicit) = std::env::var("BLAZE_PROC_MACRO_SRV") {
        let p = PathBuf::from(explicit);
        if p.is_file() {
            return Ok(p);
        }
    }
    // Try pinned toolchain first, then active rustc.
    for args in [&["+nightly-2026-10-27", "--print", "sysroot"][..], &["--print", "sysroot"][..]] {
        if let Ok(out) = Command::new("rustc").args(args).output() {
            if out.status.success() {
                let sysroot = String::from_utf8_lossy(&out.stdout).trim().to_owned();
                for sub in ["libexec/rust-analyzer-proc-macro-srv", "bin/rust-analyzer-proc-macro-srv"] {
                    let cand = Path::new(&sysroot).join(sub);
                    if cand.is_file() {
                        return Ok(cand);
                    }
                }
            }
        }
    }
    Err(NativeBridgeError::ServerNotFound(
        "checked BLAZE_PROC_MACRO_SRV and rustc --print sysroot libexec/rust-analyzer-proc-macro-srv".to_owned(),
    ))
}

/// Escapes a Rust `&str` as a JSON string literal (without surrounding quotes).
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                use core::fmt::Write as _;
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

/// Warm persistent native proc-macro server speaking newline-JSON over stdio.
pub struct NativeProcMacroServer {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    api_version: u32,
}

impl NativeProcMacroServer {
    /// Spawns and handshakes the native proc-macro server (`ApiVersionCheck`).
    pub fn spawn() -> Result<Self, NativeBridgeError> {
        let bin = find_proc_macro_srv()?;
        Self::spawn_at(&bin)
    }

    /// Spawns and handshakes a specific proc-macro server binary.
    pub fn spawn_at(bin: &Path) -> Result<Self, NativeBridgeError> {
        let mut child = Command::new(bin)
            .env("RUST_ANALYZER_INTERNALS_DO_NOT_USE", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| NativeBridgeError::Protocol("missing server stdin".to_owned()))?;
        let stdout = BufReader::new(
            child
                .stdout
                .take()
                .ok_or_else(|| NativeBridgeError::Protocol("missing server stdout".to_owned()))?,
        );
        let mut srv = Self { child, stdin, stdout, api_version: 0 };
        let reply = srv.roundtrip(r#"{"ApiVersionCheck":{}}"#)?;
        let ver = parse_api_version(&reply).ok_or_else(|| {
            NativeBridgeError::Protocol(format!("unexpected ApiVersionCheck reply: {reply}"))
        })?;
        srv.api_version = ver;
        Ok(srv)
    }

    /// Negotiated server protocol version (e.g. `6` on `nightly-2026-10-27`).
    #[inline]
    pub const fn api_version(&self) -> u32 {
        self.api_version
    }

    /// Sends one newline-terminated JSON request and reads one newline-terminated JSON reply.
    pub fn roundtrip(&mut self, request_json: &str) -> Result<String, NativeBridgeError> {
        self.stdin.write_all(request_json.as_bytes())?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;
        let mut line = String::new();
        let n = self.stdout.read_line(&mut line)?;
        if n == 0 {
            return Err(NativeBridgeError::Protocol("native proc-macro server closed stdout (EOF)".to_owned()));
        }
        Ok(line.trim_end_matches(['\r', '\n']).to_owned())
    }

    /// Queries all proc macros exported by a native host `.dylib` / `.so`.
    pub fn list_macros(&mut self, dylib_path: &Path) -> Result<Vec<NativeExport>, NativeBridgeError> {
        let escaped = json_escape(&dylib_path.to_string_lossy());
        let req = format!(r#"{{"ListMacros":{{"dylib_path":"{escaped}"}}}}"#);
        let reply = self.roundtrip(&req)?;
        parse_list_macros(&reply)
    }
}

impl Drop for NativeProcMacroServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn parse_api_version(reply: &str) -> Option<u32> {
    let key = "\"ApiVersionCheck\":";
    let start = reply.find(key)? + key.len();
    let digits: String = reply[start..]
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

/// Parses `{"ListMacros":{"Ok":[["name","CustomDerive"],...]}}` without external JSON deps.
fn parse_list_macros(reply: &str) -> Result<Vec<NativeExport>, NativeBridgeError> {
    if let Some(err_idx) = reply.find("\"Err\":") {
        return Err(NativeBridgeError::Failed(reply[err_idx..].to_owned()));
    }
    let ok_idx = reply
        .find("\"Ok\":")
        .ok_or_else(|| NativeBridgeError::Protocol(format!("unexpected ListMacros reply: {reply}")))?;
    let body = &reply[ok_idx + 5..];
    let mut exports = Vec::new();
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Look for `["<name>","<kind>"]` pairs.
        if bytes[i] == b'[' && i + 1 < bytes.len() && bytes[i + 1] == b'"' {
            i += 2;
            let name_start = i;
            while i < bytes.len() && bytes[i] != b'"' {
                if bytes[i] == b'\\' {
                    i += 1;
                }
                i += 1;
            }
            let name = body[name_start..i.min(bytes.len())].to_owned();
            // Advance to second quoted string.
            while i < bytes.len() && bytes[i] != b',' {
                i += 1;
            }
            while i < bytes.len() && bytes[i] != b'"' {
                i += 1;
            }
            if i < bytes.len() {
                i += 1;
                let kind_start = i;
                while i < bytes.len() && bytes[i] != b'"' {
                    i += 1;
                }
                let kind_str = &body[kind_start..i.min(bytes.len())];
                let kind = match kind_str {
                    "CustomDerive" => ProcKind::Derive,
                    "Attr" => ProcKind::Attr,
                    _ => ProcKind::FnLike,
                };
                exports.push(NativeExport { name, kind });
            }
        }
        i += 1;
    }
    Ok(exports)
}

/// Parsed output of running a native host `build-script-build` binary under
/// `bstd::procs::hermetic` (macOS Seatbelt / Linux Landlock + seccomp).
#[derive(Clone, Debug)]
pub struct NativeScriptOutput {
    pub exit_code: i32,
    pub cfgs: Vec<String>,
    pub rustc_envs: Vec<(String, String)>,
    pub link_libs: Vec<String>,
    pub link_search: Vec<String>,
    pub output_files: Vec<String>,
    pub stdout: String,
    pub stderr: String,
}

/// Runs a native host `build-script-build` binary directly through Blaze's own cross-platform
/// hermetic sandbox (`bstd::procs::hermetic`: macOS Seatbelt, Linux Landlock + seccomp +
/// `no_new_privs`) with zero Wasm, deterministic clock (`SOURCE_DATE_EPOCH=0`), explicit roots
/// (`manifest_dir` read-only + `out_dir` read-write), and parses standard `cargo::` / `cargo:`
/// directives.
pub fn run_native_build_script(
    binary: &Path,
    manifest_dir: &Path,
    out_dir: &Path,
    extra_read_dirs: &[&Path],
    env_vars: &[(&str, &str)],
    deadline_dur: Duration,
) -> Result<NativeScriptOutput, NativeBridgeError> {
    std::fs::create_dir_all(out_dir)?;
    let canon_bin = std::fs::canonicalize(binary)?;
    let canon_manifest = std::fs::canonicalize(manifest_dir)?;
    let canon_out = std::fs::canonicalize(out_dir)?;
    let bin_str = canon_bin
        .to_str()
        .ok_or_else(|| NativeBridgeError::Protocol("non-UTF-8 binary path".to_owned()))?;
    let manifest_str = canon_manifest
        .to_str()
        .ok_or_else(|| NativeBridgeError::Protocol("non-UTF-8 manifest_dir path".to_owned()))?;
    let out_str = canon_out
        .to_str()
        .ok_or_else(|| NativeBridgeError::Protocol("non-UTF-8 out_dir path".to_owned()))?;

    let mut canon_extra = Vec::with_capacity(extra_read_dirs.len());
    for dir in extra_read_dirs {
        let c = std::fs::canonicalize(dir)?;
        let s = c
            .to_str()
            .ok_or_else(|| NativeBridgeError::Protocol("non-UTF-8 extra read dir".to_owned()))?
            .to_owned();
        canon_extra.push(s);
    }

    let bin_bytes = std::fs::read(&canon_bin)?;
    let bin_digest = Digest(bstd::digest::blake3(&bin_bytes));

    let (world, _) = native_world(&canon_out)
        .map_err(|e| NativeBridgeError::Failed(format!("open hermetic world: {e}")))?;
    let spawner = world
        .need_host()
        .and_then(|h| h.spawner())
        .map_err(|e| NativeBridgeError::Failed(format!("missing host spawner: {}", e.capability)))?;
    let clock = world
        .need_clock()
        .map_err(|e| NativeBridgeError::Failed(format!("missing clock: {}", e.capability)))?;
    let deadline = Deadline::after(clock.now().since_start(), deadline_dur);
    let cx = world
        .cx(deadline_dur.saturating_add(Duration::from_secs(60)), TASK_FRAME)
        .map_err(|_| NativeBridgeError::Failed("hermetic world stopped".to_owned()))?;
    let alloc = Alloc::global();

    // Build hermetic Roots: read-only Tree roots for manifest_dir + extra_read_dirs, Output root for OUT_DIR.
    let tree = RootKind::Tree { tree: bin_digest };
    let mut roots: AVec<'_, Root<'_>> = OVec::new_in(alloc);
    let mut spellings: Vec<&str> = Vec::new();
    roots.push(Root { name: "manifest_dir", kind: tree });
    spellings.push(manifest_str);
    for extra in &canon_extra {
        if !spellings.contains(&extra.as_str()) {
            roots.push(Root { name: "extra_read", kind: tree });
            spellings.push(extra.as_str());
        }
    }
    let output_id = RootId(roots.len() as u8);
    roots.push(Root { name: "OUT_DIR", kind: RootKind::Output });
    spellings.push(out_str);

    let mut mounts: AVec<'_, Mount<'_>> = OVec::new_in(alloc);
    for (idx, spelling) in spellings.iter().enumerate() {
        mounts.push(Mount { spelling: spelling.as_bytes(), root: RootId(idx as u8) });
    }
    let roots = Roots::new(&roots)
        .map_err(|e| NativeBridgeError::Failed(format!("hermetic roots error: {e:?}")))?;

    let mut env: Vec<(&str, &str)> = Vec::with_capacity(env_vars.len() + 4);
    env.push(("CARGO_MANIFEST_DIR", manifest_str));
    env.push(("OUT_DIR", out_str));
    env.push(("SOURCE_DATE_EPOCH", "0"));
    env.push(("TMPDIR", out_str));
    for &(k, v) in env_vars {
        env.push((k, v));
    }
    env.sort_by(|a, b| a.0.cmp(b.0));
    let mut dedup_env: Vec<(&str, &str)> = Vec::with_capacity(env.len());
    for pair in env {
        match dedup_env.last_mut() {
            Some(last) if last.0 == pair.0 => *last = pair,
            _ => dedup_env.push(pair),
        }
    }

    let spec = HermeticSpec::new(HermeticOpts {
        program: Program::Stored { name: bin_str, digest: bin_digest, path: bin_str },
        args: &[],
        env: &dedup_env,
        roots,
        mounts: &mounts,
        cwd: manifest_str,
        helpers: &[],
        helper_folders: &[],
        channels: &[],
        clock: ClockSpec::Fixed { unix_secs: 0 },
        seed: 0,
        budget: Budget { cpu: deadline_dur.saturating_mul(16), memory: 16 << 30, steps: None },
        deadline,
        streams: HermeticStreams {
            stdout: Capture::Keep { most: 16 << 20 },
            stderr: Capture::Keep { most: 4 << 20 },
        },
        level: Level::Inputs,
        report: ReportSpec::WholeRoots,
    })
    .map_err(|e| NativeBridgeError::Failed(format!("hermetic spec refused: {:?}", e.fault)))?;

    let hermetic = Hermetic::new(HermeticCapOpts { spawner, clock });
    let files = OutDir { root: output_id, folder: out_str };
    let ran = hermetic
        .run(&cx, &spec, &files, alloc)
        .map_err(|e| NativeBridgeError::Failed(format!("hermetic run failed: {:?}", e.fault)))?;

    let stdout = String::from_utf8_lossy(&ran.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&ran.stderr).into_owned();
    let exit_code = match ran.exit {
        Exit::Exited(code) => code,
        Exit::Signaled(sig) => -(sig as i32),
    };
    let output_files: Vec<String> = ran
        .outputs
        .entries()
        .map(|o| o.path.as_str().to_owned())
        .collect();

    let mut cfgs = Vec::new();
    let mut rustc_envs = Vec::new();
    let mut link_libs = Vec::new();
    let mut link_search = Vec::new();

    for line in stdout.lines() {
        let rest = line
            .strip_prefix("cargo::")
            .or_else(|| line.strip_prefix("cargo:"));
        let Some(directive) = rest else { continue };
        if let Some(v) = directive.strip_prefix("rustc-cfg=") {
            cfgs.push(v.to_owned());
        } else if let Some(v) = directive.strip_prefix("rustc-env=") {
            if let Some((k, val)) = v.split_once('=') {
                rustc_envs.push((k.to_owned(), val.to_owned()));
            }
        } else if let Some(v) = directive.strip_prefix("rustc-link-lib=") {
            link_libs.push(v.to_owned());
        } else if let Some(v) = directive.strip_prefix("rustc-link-search=") {
            link_search.push(v.to_owned());
        }
    }

    Ok(NativeScriptOutput {
        exit_code,
        cfgs,
        rustc_envs,
        link_libs,
        link_search,
        output_files,
        stdout,
        stderr,
    })
}

/// Discovers all compiled native proc-macro `.dylib`/`.so` and `build-script-build` binaries in
/// a Cargo host `target/<profile>/deps` + `target/<profile>/build` tree (zero Wasm!).
#[derive(Clone, Debug, Default)]
pub struct NativeHostArtifacts {
    pub proc_macro_dylibs: Vec<PathBuf>,
    pub build_script_bins: Vec<PathBuf>,
}

pub fn discover_native_artifacts(target_profile_dir: &Path) -> std::io::Result<NativeHostArtifacts> {
    let mut out = NativeHostArtifacts::default();
    let dylib_ext = if cfg!(target_os = "macos") {
        "dylib"
    } else if cfg!(target_os = "windows") {
        "dll"
    } else {
        "so"
    };

    let deps = target_profile_dir.join("deps");
    if let Ok(entries) = std::fs::read_dir(&deps) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) == Some(dylib_ext) && p.is_file() {
                out.proc_macro_dylibs.push(p);
            }
        }
    }

    let build = target_profile_dir.join("build");
    if let Ok(entries) = std::fs::read_dir(&build) {
        for entry in entries.flatten() {
            let p = entry.path();
            // Classic Cargo layout: `build/<pkg>-<hash>/build-script-build`
            let classic_bin = p.join("build-script-build");
            if classic_bin.is_file() {
                out.build_script_bins.push(classic_bin);
            }
            // Nightly 2026 Cargo build-dir layout: `build/<pkg>/<hash>/out/{*.dylib,build_script_build}`
            if p.is_dir() {
                if let Ok(hashes) = std::fs::read_dir(&p) {
                    for hash_entry in hashes.flatten() {
                        let out_dir = hash_entry.path().join("out");
                        if let Ok(files) = std::fs::read_dir(&out_dir) {
                            for file in files.flatten() {
                                let fp = file.path();
                                if !fp.is_file() {
                                    continue;
                                }
                                if fp.extension().and_then(|e| e.to_str()) == Some(dylib_ext) {
                                    out.proc_macro_dylibs.push(fp);
                                } else if fp.file_name().and_then(|n| n.to_str())
                                    == Some("build_script_build")
                                    || fp.file_name().and_then(|n| n.to_str())
                                        == Some("build-script-build")
                                {
                                    out.build_script_bins.push(fp);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    out.proc_macro_dylibs.sort();
    out.build_script_bins.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_proc_macro_server_handshakes_pinned_nightly_without_wasm() {
        let srv = NativeProcMacroServer::spawn().expect("pinned nightly rust-analyzer-proc-macro-srv must handshake");
        assert!(srv.api_version() >= 5, "expected API v5+, got {}", srv.api_version());
    }

    #[test]
    fn parse_list_macros_handles_derive_attr_and_bang() {
        let sample = r#"{"ListMacros":{"Ok":[["Serialize","CustomDerive"],["sol","Bang"],["tokio_main","Attr"]]}}"#;
        let exports = parse_list_macros(sample).unwrap();
        assert_eq!(exports.len(), 3);
        assert_eq!(exports[0], NativeExport { name: "Serialize".to_owned(), kind: ProcKind::Derive });
        assert_eq!(exports[1], NativeExport { name: "sol".to_owned(), kind: ProcKind::FnLike });
        assert_eq!(exports[2], NativeExport { name: "tokio_main".to_owned(), kind: ProcKind::Attr });
    }

    #[test]
    fn hermetic_native_build_script_runs_under_bstd_sandbox_without_wasm() {
        let base = std::env::temp_dir().join(format!("blaze-hermetic-bridge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let manifest_dir = base.join("pkg");
        let out_dir = base.join("out");
        std::fs::create_dir_all(&manifest_dir).unwrap();
        std::fs::create_dir_all(&out_dir).unwrap();

        let src = manifest_dir.join("build.rs");
        let bin = base.join("build-script-build");
        std::fs::write(
            &src,
            r#"
fn main() {
    let out = std::env::var("OUT_DIR").expect("OUT_DIR");
    std::fs::write(std::path::Path::new(&out).join("generated.rs"), "pub const MAGIC: u64 = 42;\n").unwrap();
    println!("cargo::rustc-cfg=blaze_native_hermetic");
    println!("cargo::rustc-env=BLAZE_ZERO_WASM=1");
}
"#,
        )
        .unwrap();

        let status = Command::new("rustc")
            .arg(&src)
            .arg("-o")
            .arg(&bin)
            .status()
            .expect("rustc must compile native test build script");
        assert!(status.success(), "rustc failed on test build script");

        let res = run_native_build_script(
            &bin,
            &manifest_dir,
            &out_dir,
            &[],
            &[],
            Duration::from_secs(10),
        )
        .expect("bstd::procs::hermetic must run native build script");

        assert_eq!(res.exit_code, 0, "stderr: {}", res.stderr);
        assert_eq!(res.cfgs, vec!["blaze_native_hermetic".to_owned()]);
        assert_eq!(res.rustc_envs, vec![("BLAZE_ZERO_WASM".to_owned(), "1".to_owned())]);
        assert!(
            res.output_files.iter().any(|f| f == "generated.rs"),
            "expected generated.rs in output_files, got {:?}",
            res.output_files
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn discovers_and_inspects_real_tempo_native_proc_macros_if_present() {
        let tempo_debug = Path::new("/Volumes/Projects/ref-tempo/tempo/target/debug");
        if !tempo_debug.is_dir() {
            return;
        }
        let artifacts = discover_native_artifacts(tempo_debug).expect("discover tempo artifacts");
        assert!(
            !artifacts.proc_macro_dylibs.is_empty(),
            "expected native tempo proc-macro dylibs"
        );
        assert!(
            !artifacts.build_script_bins.is_empty(),
            "expected native tempo build-script binaries"
        );
        let mut srv = NativeProcMacroServer::spawn().expect("spawn native proc-macro server");
        // Inspect real `serde_derive` and `alloy_sol_macro` natively (zero Wasm!).
        if let Some(serde_dylib) = artifacts
            .proc_macro_dylibs
            .iter()
            .find(|p| p.to_string_lossy().contains("libserde_derive"))
        {
            let exports = srv.list_macros(serde_dylib).expect("list serde_derive macros");
            assert!(
                exports.iter().any(|e| e.name == "Serialize" && e.kind == ProcKind::Derive),
                "expected Serialize derive in {:?}",
                exports
            );
            assert!(
                exports.iter().any(|e| e.name == "Deserialize" && e.kind == ProcKind::Derive),
                "expected Deserialize derive in {:?}",
                exports
            );
        }
        if let Some(sol_dylib) = artifacts
            .proc_macro_dylibs
            .iter()
            .find(|p| p.to_string_lossy().contains("liballoy_sol_macro"))
        {
            let exports = srv.list_macros(sol_dylib).expect("list alloy_sol_macro macros");
            assert!(
                exports.iter().any(|e| e.name == "sol" && e.kind == ProcKind::FnLike),
                "expected sol! macro in {:?}",
                exports
            );
        }
    }
}
