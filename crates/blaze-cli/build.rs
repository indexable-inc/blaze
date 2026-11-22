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
//! It digests no producer's source: every record's key holds the binary's linker id
//! (src/producers.rs says why), which the lines below make a digest of the output.

fn main() -> std::process::ExitCode {
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
