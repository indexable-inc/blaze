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
use std::fs;
use std::path::Path;
use std::process::ExitCode;

use blaze_ids::Hasher;
use blaze_ids::producer::Row;
use blaze_producers::{Digest, Entry, Step, Walk};

const BUILD_CFG_DOMAIN: &str = "blaze-cli code-builder cfg 1";
const PACK_FILE_ITEMS_CFG_DOMAIN: &str = "blaze-cli pack-file-items cfg 1";
const PACK_FILE_ITEMS_ROOTS: [&str; 2] = ["blaze-file-items", "blaze-drive"];
const PACK_FILE_ITEMS_CLI_FILES: [&str; 3] = [
    "crates/blaze-cli/src/engine_bodies.rs",
    "crates/blaze-cli/src/front_load.rs",
    "crates/blaze-cli/src/sweep.rs",
];

fn main() -> ExitCode {
    #[expect(clippy::disallowed_methods, reason = "cargo hands a build script its target only as `CARGO_CFG_TARGET_OS`")]
    let os = std::env::var("CARGO_CFG_TARGET_OS").expect("cargo sets CARGO_CFG_TARGET_OS for every build script");
    if os == "macos" {
        println!("cargo::rustc-link-arg-bins=-Wl,-dead_strip_dylibs");
    }
    // The tool's identity is the linker's id of the binary (src/build_id.rs): ld64 writes
    // LC_UUID by default, and Blaze's own ELF exporter always writes a deterministic build-id
    // note natively (`crates/blaze-export/src/elf.rs`). Under external linkers (non-wasm64
    // build script), ask GNU ld/lld for a sha1 build-id note on Linux.
    if os == "linux" && !cfg!(target_arch = "wasm64") {
        println!("cargo::rustc-link-arg=-Wl,--build-id=sha1");
    }
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-env-changed=CARGO_CFG_DEBUG_ASSERTIONS");

    let alloc = blaze_own::Heap::process().alloc();
    if let Err(why) = emit_producers(&os, alloc) {
        eprintln!("blaze-cli build.rs: failed to compute producer digests: {why}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn emit_producers(os: &str, alloc: blaze_own::Alloc<'_>) -> Result<(), String> {
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
    let out_dir_path = Path::new(&out_dir);
    let drive_walk = |walk: &mut Walk<'_>| {
        loop {
            let (is_list, path) = match walk.next().map_err(|why| format!("{why}"))? {
                Step::Done => return Ok(()),
                Step::Read(path) => (false, path.to_owned()),
                Step::List(path) => (true, path.to_owned()),
            };
            let full = workspace_root.join(&path);
            if !is_list {
                let bytes = match fs::read(&full) {
                    Ok(b) => Some(b),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                    Err(e) => return Err(format!("read {path}: {e}")),
                };
                walk.read(bytes.as_deref()).map_err(|why| format!("{why}"))?;
                continue;
            }
            let dir = match fs::read_dir(&full) {
                Ok(d) => d,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    walk.listed(None).map_err(|why| format!("{why}"))?;
                    continue;
                }
                Err(e) => return Err(format!("list {path}: {e}")),
            };
            let mut names = blaze_own::Vec::new_in(alloc);
            for entry in dir {
                let entry = entry.map_err(|e| format!("list {path}: {e}"))?;
                let Ok(name) = entry.file_name().into_string() else {
                    continue;
                };
                let is_dir = fs::metadata(entry.path())
                    .map_err(|e| format!("metadata {path}/{name}: {e}"))?
                    .is_dir();
                names.push((name, is_dir));
            }
            let mut entries = blaze_own::Vec::with_capacity_in(names.len(), alloc);
            for (n, d) in names.iter() {
                entries.push(Entry { name: n.as_str(), is_dir: *d });
            }
            walk.listed(Some(entries.as_slice())).map_err(|why| format!("{why}"))?;
        }
    };
    let write_const = |file_name: &str, doc: &str, const_name: &str, digest: &[u8; 32], extra: &str| {
        let mut generated = blaze_own::String::new_in(alloc);
        generated.push_str(doc);
        let _ = write!(generated, "pub(crate) const {const_name}: [u8; 32] = [");
        for (i, b) in digest.iter().enumerate() {
            if i > 0 {
                generated.push_str(", ");
            }
            let _ = write!(generated, "0x{b:02x}");
        }
        generated.push_str("];\n");
        generated.push_str(extra);
        let out_path = out_dir_path.join(file_name);
        let unchanged = fs::read(&out_path).ok().is_some_and(|old| old == generated.as_bytes());
        if unchanged {
            Ok(())
        } else {
            fs::write(&out_path, generated.as_bytes()).map_err(|e| format!("write {}: {e}", out_path.display()))
        }
    };

    // 1. `CODE_BUILDER_SOURCE`: `Row::CodeBuilder` transitive closure + build cfg.
    let mut cb_walk = Walk::for_row(Row::CodeBuilder, alloc).map_err(|why| format!("{why}"))?;
    drive_walk(&mut cb_walk)?;
    let single = cb_walk.finish_row(Row::CodeBuilder).map_err(|why| format!("{why}"))?;
    let cb_row_digest = match single.digest {
        Digest::Source(bytes) => bytes,
        Digest::Refused(why) => return Err(format!("CodeBuilder row refused: {why}")),
    };
    for rel in single.watch.iter() {
        println!("cargo::rerun-if-changed={}", workspace_root.join(rel.as_str()).display());
    }
    let mut cb_hasher = Hasher::new(BUILD_CFG_DOMAIN);
    cb_hasher
        .part(&cb_row_digest)
        .part(arch.as_bytes())
        .part(os.as_bytes())
        .part(env_name.as_bytes())
        .part(&[u8::from(debug_assertions)]);
    let cb_digest = cb_hasher.finish().0;
    let cb_extra = format!("#[cfg(test)] pub(crate) const CODE_BUILDER_TARGET_ENV: &str = {env_name:?};\n");
    write_const(
        "code_builder_producer.rs",
        "/// Compile-time `ProducerId<CodeBuilder>` digest from `blaze-producers` + build cfg (Rule 21).\n",
        "CODE_BUILDER_SOURCE",
        &cb_digest,
        &cb_extra,
    )?;

    // 2. `PACK_FILE_ITEMS_SOURCE`: dedicated pack `FileItems` closure (`blaze-file-items` +
    // `blaze-drive`) plus the exact `blaze-cli` pack `FileItems` encode/decode/domain files
    // (`engine_bodies.rs`, `front_load.rs`, `sweep.rs`) and build cfg. Excludes codegen crates
    // (`blaze-emit`, `blaze-lir`, `blaze-ladder`, etc.) and unrelated CLI files (Rule 21, 52, 54).
    let mut pfi_walk = Walk::for_crates(&PACK_FILE_ITEMS_ROOTS, alloc).map_err(|why| format!("{why}"))?;
    drive_walk(&mut pfi_walk)?;
    let pfi = pfi_walk.finish_crates("pack-file-items").map_err(|why| format!("{why}"))?;
    let pfi_closure_digest = match pfi.digest {
        Digest::Source(bytes) => bytes,
        Digest::Refused(why) => return Err(format!("pack-file-items closure refused: {why}")),
    };
    for rel in pfi.watch.iter() {
        println!("cargo::rerun-if-changed={}", workspace_root.join(rel.as_str()).display());
    }
    let mut pfi_hasher = Hasher::new(PACK_FILE_ITEMS_CFG_DOMAIN);
    pfi_hasher.part(&pfi_closure_digest);
    for rel in PACK_FILE_ITEMS_CLI_FILES {
        let full = workspace_root.join(rel);
        println!("cargo::rerun-if-changed={}", full.display());
        let bytes = fs::read(&full).map_err(|e| format!("read {rel}: {e}"))?;
        pfi_hasher.part(rel.as_bytes()).part(&bytes);
    }
    pfi_hasher
        .part(arch.as_bytes())
        .part(os.as_bytes())
        .part(env_name.as_bytes())
        .part(&[u8::from(debug_assertions)]);
    let pfi_digest = pfi_hasher.finish().0;
    write_const(
        "pack_file_items_producer.rs",
        "/// Compile-time pack `FileItems` producer digest (`blaze-file-items` + `blaze-drive` closures + CLI pack `FileItems` files + build cfg; Rule 21, 52, 54).\n",
        "PACK_FILE_ITEMS_SOURCE",
        &pfi_digest,
        "",
    )?;
    Ok(())
}
