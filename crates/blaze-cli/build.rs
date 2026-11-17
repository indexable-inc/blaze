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
//! It also digests each producer's source (decision D105, build/producers.rs) into
//! `$OUT_DIR/producer_sources.rs`, which src/producers.rs includes. A row it cannot digest is a
//! warning, never a stop; only the disk failing under it stops the build, by name.

#[path = "build/producers.rs"]
mod producers;

fn main() -> std::process::ExitCode {
    #[expect(clippy::disallowed_methods, reason = "cargo hands a build script its directories only as `CARGO_MANIFEST_DIR` and `OUT_DIR`")]
    let manifest_dir = std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR for every build script");
    #[expect(clippy::disallowed_methods, reason = "cargo hands a build script its directories only as `CARGO_MANIFEST_DIR` and `OUT_DIR`")]
    let out_dir = std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR for every build script");
    // The build step is a program's entry: it names the process's heap (D102).
    let alloc = blaze_own::Heap::process().alloc();
    if let Err(failed) = producers::write(std::path::Path::new(&manifest_dir), std::path::Path::new(&out_dir), alloc) {
        eprintln!("error: {failed}");
        return std::process::ExitCode::FAILURE;
    }
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
    std::process::ExitCode::SUCCESS
}
