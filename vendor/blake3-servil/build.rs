use std::env;
use std::path::PathBuf;

fn defined(var: &str) -> bool {
    println!("cargo::rerun-if-env-changed={}", var);
    env::var_os(var).is_some()
}

fn is_ci() -> bool {
    defined("BLAKE3_CI")
}

fn warn(warning: &str) {
    assert!(!warning.contains("\n"));
    println!("cargo::warning={}", warning);
    if is_ci() {
        println!("cargo::warning=Warnings in CI are treated as errors. Build failed.");
        std::process::exit(1);
    }
}

fn target_components() -> Vec<String> {
    let target = env::var("TARGET").unwrap();
    target.split("-").map(|s| s.to_string()).collect()
}

fn is_x86_64() -> bool {
    target_components()[0] == "x86_64"
}

fn is_aarch64() -> bool {
    target_components()[0] == "aarch64"
}

fn new_build() -> cc::Build {
    let mut build = cc::Build::new();
    build.flag("-std=c11");
    // Do NOT trigger a rebuild any time the env changes (e.g. $PATH).
    // This prevents all downstream crates from being rebuilt when `cargo check`
    // or `cargo build` are run in different environments, like Rust Analyzer
    // vs. in the terminal vs. in a Git pre-commit hook.
    build.emit_rerun_if_env_changed(false);
    build
}

/// The x86-64 kernels' assembler: a C compiler that takes the AVX-512
/// flags, as GCC 12 and Clang do. Without one the build stops, as on
/// AArch64, pointing to official BLAKE3: this crate runs its assembly or
/// nothing.
fn assert_assembles_x86() {
    let build = new_build();
    let compiler = build.get_compiler().path().to_owned();
    // Both flags we use. If -mavx512f works, then -mavx512vl will probably
    // always work too, but we might as well be thorough.
    let takes = |flag| build.is_flag_supported(flag).unwrap_or(false);
    assert!(
        takes("-mavx512f") && takes("-mavx512vl"),
        "blake3-servil: {compiler:?} is not a C compiler that assembles the x86-64 kernels \
         (GCC 12 or later, or Clang); use official BLAKE3 for a build without assembly (https://github.com/BLAKE3-team/BLAKE3)"
    );
}

fn build_sse41_avx2_assembly() {
    // Build the assembly implementations for SSE4.1 and AVX2.
    assert!(is_x86_64());
    println!("cargo::rustc-cfg=blake3_sse41_ffi");
    println!("cargo::rustc-cfg=blake3_avx2_ffi");
    let mut build = new_build();
    // The Unix assembly (Linux and macOS: the files carry macOS's one
    // difference themselves).
    build.file("c/blake3_sse41_x86-64_unix.S");
    build.file("c/blake3_avx2_x86-64_unix.S");
    // The fork's own kernels (the pass with a partial chunk, the chunk root
    // in one call).
    build.file("c/blake3_partial_x86-64_unix.S");
    build.file("c/blake3_chunk_root_x86-64_unix.S");
    build.compile("blake3_sse41_avx2_assembly");
}

fn build_avx512_assembly() {
    // Build the assembly implementation for AVX-512.
    assert!(is_x86_64());
    println!("cargo::rustc-cfg=blake3_avx512_ffi");
    let mut build = new_build();
    build.file("c/blake3_avx512_x86-64_unix.S");
    // The wide kernels (32 inputs a pass; tools/gen_avx512_32.py): ELF
    // directives, so Linux, the one x86-64 target.
    if env::var("CARGO_CFG_TARGET_OS").unwrap() == "linux" {
        build.file("c/blake3_avx512_32.S");
    }
    // Older versions of Clang require these flags, even for assembly. See
    // https://github.com/BLAKE3-team/BLAKE3/issues/79.
    build.flag("-mavx512f");
    build.flag("-mavx512vl");
    build.compile("blake3_avx512_assembly");
}

// Whether this build assembles `source` with `flag`, as it will assemble the
// kernel files: with the caller's CFLAGS, and with the `.arch` directive that
// opens each kernel file, which reaches the assembler whatever -march the
// flags end with. A test of the flag alone passes under CFLAGS=-march=native,
// whose -march overrides the flag, while an assembler that predates the
// extension still rejects the kernel file's `.arch` line.
fn assembles(name: &str, flag: &str, source: &str) -> bool {
    let path = PathBuf::from(env::var("OUT_DIR").unwrap()).join(format!("probe_{name}.S"));
    std::fs::write(&path, source).unwrap();
    let mut build = new_build();
    build.file(&path).flag(flag).cargo_metadata(false).cargo_warnings(false);
    build.try_compile(&format!("probe_{name}")).is_ok()
}

// The SME2 assembly implementation needs an assembler that understands
// `.arch armv9-a+sme2` (Clang/LLVM 17+, Xcode 15+, or binutils 2.41+ with
// GCC 14+). With an older compiler the crate builds without the SME2 kernel,
// with a warning: only CPUs with SME2 (Apple M4 and later) run it, and the
// NEON and scalar kernels serve every AArch64 CPU. Common Linux systems
// (Debian 12, Raspberry Pi OS) ship such compilers.
fn c_compiler_supports_sme2() -> bool {
    let compiler = new_build().get_compiler().path().to_owned();
    let supported = assembles("sme2", "-march=armv9-a+sme2", ".arch armv9-a+sme2\nsmstart sm\nsmstop sm\n");
    if !supported {
        warn(&format!(
            "blake3-servil: {compiler:?} cannot assemble SME2 (-march=armv9-a+sme2), so this \
             build leaves out the SME2 kernel, which Apple M4 and later CPUs run. Clang/LLVM 17 \
             or later (for example CC=clang-19), Xcode 15 or later, or GCC 14 or later builds it."
        ));
    }
    supported
}

fn build_sme2_assembly() {
    // The SME2 implementation is assembly only. It requires runtime
    // detection (see platform.rs), and it requires a 512-bit streaming vector
    // length, which the kernel checks for itself.
    assert!(is_aarch64());
    println!("cargo::rustc-cfg=blake3_sme2");
    let mut build = new_build();
    build.file("c/blake3_sme2_aarch64.S");
    // Generated by tools/gen_sme2_hybrid.py: SME2 groups with an integer lane.
    build.file("c/blake3_sme2_hybrid_aarch64.S");
    build.flag("-march=armv9-a+sme2");
    build.compile("blake3_sme2_assembly");
}

// The integer + NEON kernels need an assembler that knows armv8.2-a+sha3
// (GNU as 2.30+, LLVM 7+). With an older one the build stops and says so.
fn assert_assembles_sha3() {
    let compiler = new_build().get_compiler().path().to_owned();
    assert!(
        assembles("sha3", "-march=armv8.2-a+sha3", ".arch armv8.2-a+sha3\nxar v0.2d, v1.2d, v2.2d, #1\n"),
        "blake3-servil: {compiler:?} cannot assemble armv8.2-a+sha3, which the AArch64 kernels need \
         (GNU as 2.30 or later, or Clang/LLVM 7 or later); use official BLAKE3 for a build without assembly (https://github.com/BLAKE3-team/BLAKE3)"
    );
}

fn build_neon_hybrid_assembly() {
    // Integer + NEON kernels for one to fifteen chunks (and parents). The
    // pair kernels use `xar` from the SHA-3 extension, detected at runtime;
    // the scalar and quad kernels run everywhere. Assembling needs an
    // assembler that knows armv8.2-a+sha3 (GNU as 2.30+, LLVM 7+).
    assert!(is_aarch64());
    let mut build = new_build();
    build.file("c/blake3_neon_hybrid_aarch64.S");
    build.compile("blake3_neon_hybrid_assembly");
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The supported targets, and every other one stopped here, before
    // anything is built: a build elsewhere would run code no machine of
    // ours tests, where official BLAKE3 supports it (#160, #180).
    let var = |name: &str| env::var(name).unwrap_or_default();
    let (arch, os) = (var("CARGO_CFG_TARGET_ARCH"), var("CARGO_CFG_TARGET_OS"));
    let supported = var("CARGO_CFG_TARGET_ENDIAN") == "little"
        && match arch.as_str() {
            "x86_64" => os == "linux",
            "aarch64" => os == "linux" || os == "macos",
            _ => false,
        };
    assert!(
        supported,
        "blake3-servil runs on x86-64 Linux and on 64-bit Arm Linux and macOS; for {}, use official BLAKE3 (https://github.com/BLAKE3-team/BLAKE3)",
        var("TARGET")
    );
    // If CFLAGS is set, make sure LTO is disabled by appending -fno-lto. (We can't do this with
    // .flag() later on, because CFLAGS takes precedence.) Any mismatch between the C compiler and
    // the Rust linker can cause linker errors in our C intrinsics implementations if LTO is
    // enabled. See https://github.com/BLAKE3-team/BLAKE3/issues/550.
    if let Some(mut flags) = env::var_os("CFLAGS") {
        flags.push(" -fno-lto");
        // SAFETY: This build script is single-threaded.
        unsafe {
            env::set_var("CFLAGS", flags);
        }
    }

    // As of Rust 1.80, unrecognized config names are warnings. Give Cargo all of our config names.
    // `shuttle` gates tests/shuttle_*.rs (RUSTFLAGS="--cfg shuttle"); the
    // library compiles the same either way.
    let all_cfgs = ["blake3_sse41_ffi", "blake3_avx2_ffi", "blake3_avx512_ffi", "blake3_neon", "blake3_sme2", "shuttle"];
    for cfg_name in all_cfgs {
        // https://doc.rust-lang.org/cargo/reference/build-scripts.html#outputs-of-the-build-script
        println!("cargo::rustc-check-cfg=cfg({cfg_name}, values(none()))");
    }

    if is_x86_64() {
        assert_assembles_x86();
        build_sse41_avx2_assembly();
        build_avx512_assembly();
    }

    if is_aarch64() {
        println!("cargo::rustc-cfg=blake3_neon");
        assert_assembles_sha3();
        build_neon_hybrid_assembly();

        // SME2 sits on top of NEON: the SME2 wrapper falls back to NEON for
        // inputs that don't fill a group of sixteen. It is built on every
        // AArch64 target with NEON enabled and runtime detection (Apple
        // platforms and Linux, the same cfg as platform.rs's sme2_reported
        // and Cargo.toml's libc dependency) whose C compiler assembles SME2.
        // Platform::detect() selects the SME2 kernels when the CPU reports
        // SME2 with 512-bit streaming vectors and the NEON hybrids otherwise.
        let target_vendor = env::var("CARGO_CFG_TARGET_VENDOR").unwrap_or_default();
        let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
        if target_vendor == "apple" || target_os == "linux" {
            if c_compiler_supports_sme2() {
                build_sme2_assembly();
            }
        }
    }

    // The `cc` crate doesn't automatically emit rerun-if directives for the
    // environment variables it supports, in particular for $CC. We expect to
    // do a lot of benchmarking across different compilers, so we explicitly
    // add the variables that we're likely to need.
    println!("cargo::rerun-if-env-changed=CC");
    println!("cargo::rerun-if-env-changed=CFLAGS");

    // Ditto for source files, though these shouldn't change as often.
    for file in std::fs::read_dir("c")? {
        println!("cargo::rerun-if-changed={}", file?.path().to_str().expect("utf-8"));
    }

    Ok(())
}
