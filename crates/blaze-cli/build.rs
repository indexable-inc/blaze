//! Links the `blaze` binary on macOS with `-dead_strip_dylibs`: ld64 then drops every library a
//! crate's `#[link]` named that no symbol of the binary is bound to, so dyld neither loads nor
//! initialises it at each start. Measured 2026-11-16 on a release `blaze` (the coordinator's
//! harness): without it the binary linked VideoToolbox, CoreVideo, CoreMedia, libobjc, Foundation,
//! Security and libiconv besides CoreFoundation, CoreServices and libSystem; with it only the
//! last three, and `blaze` with no arguments started about 0.4 ms sooner. Safe for this binary:
//! nothing in it relies on a library loaded for its initialisers alone (an Objective-C category,
//! a `+load` method, a constructor with no symbol bound), which this flag would drop.
//! `tests/links.rs` holds the result: the binary links libSystem and nothing else.
//!
//! Also computes `CODE_BUILDER_SOURCE` (D105, `docs/v2/plan/design/producer-id.md`, Rule 21):
//! a deterministic compile-time BLAKE3 digest of the transitive crate source closure of
//! `Row::CodeBuilder` (via `blaze-producers`, excluding `blaze-export` and CLI glue) mixed with
//! target/debug_assertions build settings, written to `$OUT_DIR/code_builder_producer.rs`.
//! Rebuilding `blaze-cli` after editing `blaze-export` or CLI glue changes the binary's `LC_UUID`
//! without invalidating cached `CodeRecord`s in `~/.blaze/v2`.

use std::fmt::Write as _;
use std::path::Path;
use std::process::ExitCode;

use blaze_ids::Hasher;
use blaze_ids::producer::Row;
use blaze_producers::{Digest, Entry, Step, Walk};

const CODE_BUILDER_DOMAIN: &str = "blaze-cli code-builder producer 1";

fn main() -> ExitCode {
    #[expect(clippy::disallowed_methods, reason = "cargo hands a build script its target only as `CARGO_CFG_TARGET_OS`")]
    let os = std::env::var("CARGO_CFG_TARGET_OS").expect("cargo sets CARGO_CFG_TARGET_OS for every build script");
    if os == "macos" {
        println!("cargo::rustc-link-arg-bins=-Wl,-dead_strip_dylibs");
    }
    // The tool's identity is the linker's id of the binary (src/build_id.rs): ld64 writes
    // LC_UUID by default; on Linux ask for a build-id note, a digest of the output (`sha1`), so
    // equal inputs give one id. Tests read it too, so every target gets it.
    if os == "linux" {
        println!("cargo::rustc-link-arg=-Wl,--build-id=sha1");
    }
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-env-changed=CARGO_CFG_DEBUG_ASSERTIONS");

    if let Err(why) = emit_code_builder_producer(&os) {
        eprintln!("blaze-cli build.rs: failed to compute CodeBuilder producer digest: {why}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn emit_code_builder_producer(os: &str) -> Result<(), String> {
    #[expect(clippy::disallowed_methods, reason = "cargo hands a build script its manifest/out dirs and cfg via env")]
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").map_err(|e| format!("CARGO_MANIFEST_DIR: {e}"))?;
    #[expect(clippy::disallowed_methods, reason = "cargo hands a build script its out dir via env")]
    let out_dir = std::env::var("OUT_DIR").map_err(|e| format!("OUT_DIR: {e}"))?;
    #[expect(clippy::disallowed_methods, reason = "cargo hands a build script its target arch via env")]
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").map_err(|e| format!("CARGO_CFG_TARGET_ARCH: {e}"))?;
    #[expect(clippy::disallowed_methods, reason = "cargo hands a build script its target env via env")]
    let env_name = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    #[expect(clippy::disallowed_methods, reason = "cargo sets CARGO_CFG_DEBUG_ASSERTIONS when debug assertions are on")]
    let debug_assertions = std::env::var_os("CARGO_CFG_DEBUG_ASSERTIONS").is_some();

    let workspace_root = Path::new(&manifest_dir).join("../..");
    let alloc = blaze_own::Heap::process().alloc();
    let mut walk = Walk::for_row(Row::CodeBuilder, alloc).map_err(|why| format!("{why}"))?;
    loop {
        let (is_list, path) = match walk.next().map_err(|why| format!("{why}"))? {
            Step::Done => break,
            Step::Read(path) => (false, path.to_owned()),
            Step::List(path) => (true, path.to_owned()),
        };
        let full = workspace_root.join(&path);
        if !is_list {
            let bytes = match std::fs::read(&full) {
                Ok(b) => Some(b),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(format!("read {path}: {e}")),
            };
            walk.read(bytes.as_deref()).map_err(|why| format!("{why}"))?;
            continue;
        }
        let dir = match std::fs::read_dir(&full) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                walk.listed(None).map_err(|why| format!("{why}"))?;
                continue;
            }
            Err(e) => return Err(format!("list {path}: {e}")),
        };
        let mut names: Vec<(String, bool)> = Vec::new();
        for entry in dir {
            let entry = entry.map_err(|e| format!("list {path}: {e}"))?;
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            let is_dir = std::fs::metadata(entry.path())
                .map_err(|e| format!("metadata {path}/{name}: {e}"))?
                .is_dir();
            names.push((name, is_dir));
        }
        let entries: Vec<Entry<'_>> = names.iter().map(|(n, d)| Entry { name: n.as_str(), is_dir: *d }).collect();
        walk.listed(Some(&entries)).map_err(|why| format!("{why}"))?;
    }

    let single = walk.finish_row(Row::CodeBuilder).map_err(|why| format!("{why}"))?;
    let row_digest = match single.digest {
        Digest::Source(bytes) => bytes,
        Digest::Refused(why) => return Err(format!("CodeBuilder row refused: {why}")),
    };

    for rel in single.watch.iter() {
        println!("cargo::rerun-if-changed={}", workspace_root.join(rel.as_str()).display());
    }

    let mut hasher = Hasher::new(CODE_BUILDER_DOMAIN);
    hasher
        .part(&row_digest)
        .part(arch.as_bytes())
        .part(os.as_bytes())
        .part(env_name.as_bytes())
        .part(&[u8::from(debug_assertions)]);
    let final_digest = hasher.finish().0;

    let mut generated = String::with_capacity(320);
    generated.push_str("/// Compile-time `ProducerId<CodeBuilder>` digest from `blaze-producers` + build cfg (Rule 21).\n");
    generated.push_str("pub(crate) const CODE_BUILDER_SOURCE: [u8; 32] = [");
    for (i, b) in final_digest.iter().enumerate() {
        if i > 0 {
            generated.push_str(", ");
        }
        let _ = write!(generated, "0x{b:02x}");
    }
    generated.push_str("];\n");
    let _ = writeln!(generated, "#[cfg(test)] pub(crate) const CODE_BUILDER_TARGET_ENV: &str = {env_name:?};");

    let out_path = Path::new(&out_dir).join("code_builder_producer.rs");
    let unchanged = std::fs::read(&out_path).ok().is_some_and(|old| old == generated.as_bytes());
    if !unchanged {
        std::fs::write(&out_path, generated.as_bytes()).map_err(|e| format!("write {}: {e}", out_path.display()))?;
    }
    Ok(())
}
