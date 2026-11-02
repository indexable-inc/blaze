//! On macOS, the sandbox's library; on Linux, its interposer. Each is named in the user's cache
//! by the BLAKE3 hash of its bytes, taken at run time (`src/sandbox/`), so this script has no
//! build dependency. Which blaze this is (its version and commit) is crates/blaze-engine's
//! build.rs's, the one crate that changes with every commit anyway, and reaches this crate
//! through `engine::Engine`: this crate's build stays the same from commit to commit.

#![forbid(unsafe_code)]

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    sandbox_library();
    linux_interpose();
}

/// On Linux, the library every process of a traced sandbox loads through `LD_PRELOAD`
/// (`src/sandbox/linux_interpose.c`): it interposes `getenv`, `secure_getenv`, the realtime
/// clocks and the `exec`/`posix_spawn` family, and says it loaded, so the supervisor sees
/// variable and clock reads a seccomp filter cannot, and which programs ran without it. Built
/// with the C compiler of the system (`CC` when set), carried in the executable by
/// `src/sandbox/linux.rs`.
fn linux_interpose() {
    let source = "src/sandbox/linux_interpose.c";
    println!("cargo:rerun-if-changed={source}");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux") {
        return;
    }
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("libblaze_sandbox_interpose.so");
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_owned());
    let status = std::process::Command::new(&cc)
        .args(["-shared", "-fPIC", "-O2", "-Wall"])
        .arg("-o")
        .arg(&out)
        .arg(source)
        // After the source: under `--as-needed` a library named before the object it serves is
        // dropped.
        .arg("-ldl")
        .status()
        .unwrap_or_else(|error| panic!("{cc} did not run to build {source}: {error}"));
    assert!(status.success(), "{cc} could not build {source}");
}

/// On macOS, the library every process of a sandbox loads (`src/sandbox/macos_interpose.c`),
/// built with the C compiler of the system (`CC` when set), which `src/sandbox/macos.rs`
/// carries in the executable. A C file and no build dependency: dyld loads only a dynamic
/// library, which cargo builds from Rust only as a crate of its own.
fn sandbox_library() {
    let source = "src/sandbox/macos_interpose.c";
    println!("cargo:rerun-if-changed={source}");
    println!("cargo:rerun-if-env-changed=CC");
    println!("cargo:rerun-if-env-changed=MACOSX_DEPLOYMENT_TARGET");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    // Apple's own programs on Apple silicon (`cc`, `clang`, `ld`) are arm64e, and where macOS
    // does not drop `DYLD_*` for them (SIP off, as on GitHub's macOS runners) dyld aborts one
    // whose inserted library has no arm64e slice: every C build of a build script under the
    // sandbox (#147). The arm64 slice is for every other program.
    let arches: &[&str] = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("aarch64") => &["-arch", "arm64", "-arch", "arm64e"],
        Ok("x86_64") => &["-arch", "x86_64"],
        other => panic!("no sandbox library for the architecture {other:?}"),
    };
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("blaze_sandbox.dylib");
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_owned());
    let target = std::env::var("MACOSX_DEPLOYMENT_TARGET").unwrap_or_else(|_| "11.0".to_owned());
    let status = std::process::Command::new(&cc)
        .args(["-dynamiclib", "-O2", "-Wall"])
        .args(arches)
        .arg(format!("-mmacosx-version-min={target}"))
        .arg("-o")
        .arg(&out)
        .arg(source)
        .status()
        .unwrap_or_else(|error| panic!("{cc} did not run to build {source}: {error}"));
    assert!(status.success(), "{cc} could not build {source}");
}
