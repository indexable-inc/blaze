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
    let out =
        PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("libblaze_sandbox_interpose.so");
    println!("cargo:rerun-if-env-changed=CC");
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_owned());
    // The library is carried in the executable and loaded into every process of a traced run,
    // wherever blaze is installed: it must name no directory of this build. A nix shell's
    // compiler wrapper writes a RUNPATH of its own choosing (2026-11-12: its first entry was
    // this `OUT_DIR`, so every sandboxed process's loader looked for `libdl.so.2` and
    // `libc.so.6` there first: a traced read of one machine's build directory in every run,
    // and a pure script's proof refused for it); `NIX_DONT_SET_RPATH` tells it to write none.
    // With no RUNPATH the library may need only what every process has loaded already, which
    // is the C library: so it is linked without `libdl` where the C library has `dlsym` and
    // `dladdr` itself (glibc 2.34 and later, musl), which `-z defs` proves at link time, and
    // with `libdl` only where that link fails (an older glibc, whose loader finds `libdl.so.2`
    // in its default directories).
    // Measured on the NixOS VM, 2026-11-12 (gcc 16.2 under nixpkgs' wrapper): the wrapper's
    // linker writes the directory of every `.so` path among its arguments that exists, and the
    // output of `-o` is one of them once a build before this one has made it: so the first
    // build of a target directory wrote no build directory and every rebuild of the C file
    // wrote `OUT_DIR`. The output is removed before each link. The wrapper reads its switch
    // under the name salted with the target (`NIX_DONT_SET_RPATH_aarch64_unknown_linux_gnu`;
    // the unsalted name and `_FOR_TARGET` changed nothing there), so all three are set.
    let salted = format!(
        "NIX_DONT_SET_RPATH_{}",
        std::env::var("TARGET").unwrap_or_else(|_| panic!("cargo sets TARGET for a build script")).replace(['-', '.'], "_")
    );
    let link = |libdl: bool| {
        match std::fs::remove_file(&out) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("removing {} before linking it again: {error}", out.display()),
        }
        let mut command = std::process::Command::new(&cc);
        command
            .env("NIX_DONT_SET_RPATH", "1")
            .env("NIX_DONT_SET_RPATH_FOR_TARGET", "1")
            .env(&salted, "1")
            .args(["-shared", "-fPIC", "-O2", "-Wall", "-Wl,-z,defs"])
            .arg("-o")
            .arg(&out)
            .arg(source);
        if libdl {
            // After the source: under `--as-needed` a library named before the object it
            // serves is dropped.
            command.arg("-ldl");
        }
        command
    };
    let alone = link(false)
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap_or_else(|error| panic!("{cc} did not run to build {source}: {error}"));
    if !alone.success() {
        let status = link(true)
            .status()
            .unwrap_or_else(|error| panic!("{cc} did not run to build {source}: {error}"));
        assert!(status.success(), "{cc} could not build {source}");
    }
    // The guard: no path of this build in the library's bytes (a RUNPATH or RPATH entry, a
    // file name), whatever the compiler's wrapper did.
    let bytes = std::fs::read(&out).unwrap_or_else(|error| panic!("reading {}: {error}", out.display()));
    for name in ["OUT_DIR", "CARGO_MANIFEST_DIR"] {
        let dir = std::env::var(name).unwrap_or_else(|_| panic!("cargo sets {name} for a build script"));
        let held = !dir.is_empty() && bytes.windows(dir.len()).any(|it| it == dir.as_bytes());
        assert!(
            !held,
            "{} holds {dir} ({name}): the sandbox's interposer is loaded into every traced process and must \
             name no directory of the build that made it (a RUNPATH the compiler wrote? `readelf -d` shows it)",
            out.display()
        );
    }
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
