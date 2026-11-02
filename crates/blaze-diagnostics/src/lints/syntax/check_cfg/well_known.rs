//! The names and values rustc expects once any `--check-cfg` is given, whatever the command
//! line says (`CheckCfg::fill_well_known`), in the order rustc adds them.
//!
//! Follows rustc's `compiler/rustc_session/src/config/cfg.rs` (`fill_well_known`), with the
//! values it derives from every target it builds in (`rustc_target/src/spec`: `Target::builtins`,
//! `RelocModel::ALL`, `PanicStrategy::ALL`, `SanitizerSet::all`, `FmtDebug::all`) and every
//! target feature (`rustc_target/src/target_features.rs`: `all_rust_features` that are in cfg,
//! and `RUSTC_SPECIFIC_FEATURES`), as `tests/ui/check-cfg/well-known-values.stderr` and
//! `target_feature.stderr` list them in the rustc checkout of July 2026 (the pinned nightly is
//! `nightly-2026-10-27`; a target or feature added between the two is missing here) (MIT or
//! Apache-2.0).

/// What a well-known name starts as before its values are added: `#[cfg(name)]` expected
/// (`no_values`) or nothing yet (`empty_values`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Start {
    NoValue,
    Empty,
}

/// Every well-known name, what it starts as, and the values rustc adds to it.
pub(super) const WELL_KNOWN: &[(&str, Start, &[&str])] = &[
    ("debug_assertions", Start::NoValue, &[]),
    ("fmt_debug", Start::Empty, &["full", "shallow", "none"]),
    ("clippy", Start::NoValue, &[]),
    ("doc", Start::NoValue, &[]),
    ("doctest", Start::NoValue, &[]),
    ("miri", Start::NoValue, &[]),
    ("rustfmt", Start::NoValue, &[]),
    ("overflow_checks", Start::NoValue, &[]),
    ("panic", Start::Empty, &["unwind", "abort", "immediate-abort"]),
    ("proc_macro", Start::NoValue, &[]),
    ("relocation_model", Start::Empty, &["static", "pic", "pie", "dynamic-no-pic", "ropi", "rwpi", "ropi-rwpi"]),
    ("sanitize", Start::Empty, SANITIZE),
    ("sanitizer_cfi_generalize_pointers", Start::NoValue, &[]),
    ("sanitizer_cfi_normalize_integers", Start::NoValue, &[]),
    ("target_feature", Start::Empty, TARGET_FEATURE),
    ("target_abi", Start::Empty, TARGET_ABI),
    ("target_arch", Start::Empty, TARGET_ARCH),
    ("target_endian", Start::Empty, &["big", "little"]),
    ("target_env", Start::Empty, TARGET_ENV),
    ("target_family", Start::Empty, &["unix", "wasm", "windows"]),
    ("target_object_format", Start::Empty, &["coff", "elf", "mach-o", "wasm", "xcoff"]),
    ("target_os", Start::Empty, TARGET_OS),
    ("target_pointer_width", Start::Empty, &["16", "32", "64"]),
    ("target_vendor", Start::Empty, TARGET_VENDOR),
    ("target_has_atomic", Start::NoValue, ATOMIC),
    ("target_has_atomic_load_store", Start::NoValue, ATOMIC),
    ("target_has_atomic_primitive_alignment", Start::Empty, ATOMIC),
    ("target_thread_local", Start::NoValue, &[]),
    ("target_has_threads", Start::NoValue, &[]),
    ("ub_checks", Start::NoValue, &[]),
    ("contract_checks", Start::NoValue, &[]),
    ("unix", Start::NoValue, &[]),
    ("windows", Start::NoValue, &[]),
];

const ATOMIC: &[&str] = &["ptr", "8", "16", "32", "64", "128"];

const SANITIZE: &[&str] = &[
    "address", "cfi", "dataflow", "hwaddress", "kcfi", "kernel-address", "kernel-hwaddress", "leak",
    "memory", "memtag", "realtime", "safestack", "shadow-call-stack", "thread",
];

const TARGET_ABI: &[&str] = &[
    "", "abi64", "abiv2", "abiv2hf", "eabi", "eabihf", "elfv1", "elfv2", "fortanix", "ilp32", "ilp32e",
    "llvm", "macabi", "pauthtest", "sim", "softfloat", "spe", "uwp", "vec-extabi", "x32",
];

const TARGET_ARCH: &[&str] = &[
    "aarch64", "amdgpu", "arm", "arm64ec", "avr", "bpf", "csky", "hexagon", "loongarch32", "loongarch64",
    "m68k", "mips", "mips32r6", "mips64", "mips64r6", "msp430", "nvptx64", "powerpc", "powerpc64",
    "riscv32", "riscv64", "s390x", "sparc", "sparc64", "wasm32", "wasm64", "x86", "x86_64", "xtensa",
];

const TARGET_ENV: &[&str] = &[
    "", "gnu", "macabi", "mlibc", "msvc", "musl", "newlib", "nto70", "nto71", "nto71_iosock", "ohos",
    "p1", "p2", "p3", "relibc", "sgx", "sim", "uclibc", "v5",
];

const TARGET_OS: &[&str] = &[
    "aix", "amdhsa", "android", "cuda", "cygwin", "dragonfly", "emscripten", "espidf", "freebsd",
    "fuchsia", "haiku", "helenos", "hermit", "horizon", "hurd", "illumos", "ios", "l4re", "linux",
    "lynxos178", "macos", "managarm", "motor", "netbsd", "none", "nto", "nuttx", "openbsd", "psp",
    "psx", "qnx", "qurt", "redox", "rtems", "solaris", "solid_asp3", "teeos", "trusty", "tvos", "uefi",
    "unknown", "vexos", "visionos", "vita", "vxworks", "wasi", "watchos", "windows", "xous", "zkvm",
];

const TARGET_VENDOR: &[&str] = &[
    "amd", "apple", "espressif", "fortanix", "ibm", "kmc", "mti", "nintendo", "nvidia", "openwrt", "pc",
    "risc0", "sony", "sun", "unikraft", "unknown", "uwp", "vex", "win7", "wrs",
];

const TARGET_FEATURE: &[&str] = &[
    "10e60", "2e3", "32s", "3e3r1", "3e3r2", "3e3r3", "3e7", "7e10",
    "a", "aclass", "acquire-release", "addsubiw", "adx", "aes", "allows-misaligned-mem-access", "altivec",
    "alu32", "amx-avx512", "amx-bf16", "amx-complex", "amx-fp16", "amx-fp8", "amx-int8", "amx-movrs",
    "amx-tile", "apxf", "atomics", "audio", "avx", "avx10.1", "avx10.2", "avx2",
    "avx512bf16", "avx512bitalg", "avx512bw", "avx512cd", "avx512dq", "avx512f", "avx512fp16", "avx512ifma",
    "avx512vbmi", "avx512vbmi2", "avx512vl", "avx512vnni", "avx512vp2intersect", "avx512vpopcntdq", "avxifma", "avxneconvert",
    "avxvnni", "avxvnniint16", "avxvnniint8", "b", "backchain", "bf16", "bmi1", "bmi2",
    "bool", "break", "bti", "bulk-memory", "c", "cache", "clflushopt", "cmpxchg16b",
    "concurrent-functions", "coprocessor", "crc", "crt-static", "cssc", "d", "d32", "debug",
    "deflate-conversion", "dit", "div32", "doloop", "dotprod", "dpb", "dpb2", "dsp",
    "dsp1e2", "dspe60", "e", "e1", "e2", "ecv", "edsp", "eijmpcall",
    "elpm", "elpmx", "elrw", "enhanced-sort", "ermsb", "exception", "exception-handling", "extended-const",
    "extendedl32r", "f", "f16c", "f32mm", "f64mm", "faminmax", "fcma", "fdivdu",
    "fhm", "flagm", "flagm2", "float1e2", "float1e3", "float3e4", "float7e60", "floate1",
    "fma", "fma4", "fp", "fp-armv8", "fp16", "fp64", "fp8", "fp8dot2",
    "fp8dot4", "fp8fma", "fpregs", "fpuv2_df", "fpuv2_sf", "fpuv3_df", "fpuv3_hf", "fpuv3_hi",
    "fpuv3_sf", "frecipe", "frintts", "fxsr", "gc", "gfni", "guarded-storage", "hard-float",
    "hard-float-abi", "hard-tp", "hbc", "high-registers", "high-word", "highpriinterrupts", "hvx", "hvx-ieee-fp",
    "hvx-length128b", "hvx-length64b", "hvx-qfloat", "hvxv60", "hvxv62", "hvxv65", "hvxv66", "hvxv67",
    "hvxv68", "hvxv69", "hvxv71", "hvxv73", "hvxv75", "hvxv79", "hwdiv", "i8mm",
    "ijmpcall", "interrupt", "isa-68000", "isa-68010", "isa-68020", "isa-68030", "isa-68040", "isa-68060",
    "isa-68881", "isa-68882", "jmpcall", "jsconv", "kl", "lahfsahf", "lam-bh", "lamcas",
    "lasx", "lbt", "ld-seq-sa", "leoncasa", "loop", "lor", "lowbytefirst", "lpm",
    "lpmx", "lse", "lse128", "lse2", "lsx", "lut", "lvz", "lzcnt",
    "m", "mac16", "mclass", "message-security-assist-extension12", "message-security-assist-extension3",
    "message-security-assist-extension4", "message-security-assist-extension5", "message-security-assist-extension8",
    "message-security-assist-extension9", "miscellaneous-extensions-2", "miscellaneous-extensions-3",
    "miscellaneous-extensions-4", "miscsr", "mops", "movbe", "movrs",
    "movw", "mp", "mp1e2", "msa", "msync", "mte", "mul", "mul32",
    "mul32high", "multivalue", "mutable-globals", "mve", "mve.fp", "neon", "nnp-assist", "nontrapping-fptoint",
    "nsa", "nvic", "outline-atomics", "paca", "pacg", "pan", "partword-atomics", "pauth-lr",
    "pclmulqdq", "pmuv3", "popcnt", "power10-vector", "power8-altivec", "power8-crypto", "power8-vector", "power9-altivec",
    "power9-vector", "prfchw", "prid", "ptx70", "ptx71", "ptx72", "ptx73", "ptx74",
    "ptx75", "ptx76", "ptx77", "ptx78", "ptx80", "ptx81", "ptx82", "ptx83",
    "ptx84", "ptx85", "ptx86", "ptx87", "quadword-atomics", "rand", "ras", "rclass",
    "rcpc", "rcpc2", "rcpc3", "rdm", "rdrand", "rdseed", "reference-types", "regprotect",
    "relax", "relaxed-simd", "rmw", "rtm", "rva23u64", "rvector", "s32c1i", "sb",
    "scq", "sext", "sha", "sha2", "sha3", "sha512", "sign-ext", "simd128",
    "sm3", "sm4", "sm_100", "sm_100a", "sm_101", "sm_101a", "sm_120", "sm_120a",
    "sm_70", "sm_72", "sm_75", "sm_80", "sm_86", "sm_87", "sm_89", "sm_90",
    "sm_90a", "sme", "sme-b16b16", "sme-f16f16", "sme-f64f64", "sme-f8f16", "sme-f8f32", "sme-fa64",
    "sme-i16i64", "sme-lutv2", "sme2", "sme2p1", "soft-float", "spe", "spm", "spmx",
    "ssbs", "sse", "sse2", "sse3", "sse4.1", "sse4.2", "sse4a", "ssse3",
    "ssve-fp8dot2", "ssve-fp8dot4", "ssve-fp8fma", "supm", "sve", "sve-b16b16", "sve2", "sve2-aes",
    "sve2-bitperm", "sve2-sha3", "sve2-sm4", "sve2p1", "tail-call", "tbm", "threadptr", "thumb-mode",
    "thumb2", "tinyencoding", "tme", "transactional-execution", "trust", "trustzone", "ual", "unaligned-scalar-mem",
    "unaligned-vector-mem", "v", "v5te", "v6", "v60", "v62", "v65", "v66",
    "v67", "v68", "v69", "v6k", "v6m", "v6t2", "v7", "v71",
    "v73", "v75", "v79", "v8", "v8.1a", "v8.1m.main", "v8.2a", "v8.3a",
    "v8.4a", "v8.5a", "v8.6a", "v8.7a", "v8.8a", "v8.9a", "v8m", "v8m.main",
    "v8plus", "v9", "v9.1a", "v9.2a", "v9.3a", "v9.4a", "v9.5a", "v9a",
    "vaes", "vdsp2e60f", "vdspv1", "vdspv2", "vector", "vector-enhancements-1", "vector-enhancements-2", "vector-enhancements-3",
    "vector-packed-decimal", "vector-packed-decimal-enhancement", "vector-packed-decimal-enhancement-2",
    "vector-packed-decimal-enhancement-3", "vfp2", "vfp3", "vfp4", "vh",
    "virt", "virtualization", "vpclmulqdq", "vsx", "wfxt", "wide-arithmetic", "widekl", "x87",
    "xop", "xsave", "xsavec", "xsaveopt", "xsaves", "za128rs", "za64rs", "zaamo",
    "zabha", "zacas", "zalrsc", "zama16b", "zawrs", "zba", "zbb", "zbc",
    "zbkb", "zbkc", "zbkx", "zbs", "zca", "zcb", "zcmop", "zdinx",
    "zfa", "zfbfmin", "zfh", "zfhmin", "zfinx", "zhinx", "zhinxmin", "zic64b",
    "zicbom", "zicbop", "zicboz", "ziccamoa", "ziccif", "zicclsm", "ziccrse", "zicntr",
    "zicond", "zicsr", "zifencei", "zihintntl", "zihintpause", "zihpm", "zimop", "zk",
    "zkn", "zknd", "zkne", "zkne_or_zknd", "zknh", "zkr", "zks", "zksed",
    "zksh", "zkt", "zreg", "ztso", "zvbb", "zvbc", "zve32f", "zve32x",
    "zve64d", "zve64f", "zve64x", "zvfbfmin", "zvfbfwma", "zvfh", "zvfhmin", "zvkb",
    "zvkg", "zvkn", "zvknc", "zvkned", "zvkng", "zvknha", "zvknhb", "zvks",
    "zvksc", "zvksed", "zvksg", "zvksh", "zvkt", "zvl1024b", "zvl128b", "zvl16384b",
    "zvl2048b", "zvl256b", "zvl32768b", "zvl32b", "zvl4096b", "zvl512b", "zvl64b", "zvl65536b",
    "zvl8192b",
];

/// The cfgs rustc sets itself and refuses on the command line, each with the flag that sets
/// it: the name, whether it is refused with a value (`Some(true)`), without one (`Some(false)`)
/// or either way (`None`), and for `panic` only these values (`disallow_cfgs`).
pub(super) const DISALLOWED: &[(&str, Option<bool>, &str)] = &[
    ("overflow_checks", Some(false), "-C overflow-checks"),
    ("debug_assertions", Some(false), "-C debug-assertions"),
    ("ub_checks", Some(false), "-Z ub-checks"),
    ("contract_checks", Some(false), "-Z contract-checks"),
    ("sanitize", None, "-Z sanitizer"),
    ("sanitizer_cfi_generalize_pointers", None, "-Z sanitizer=cfi"),
    ("sanitizer_cfi_normalize_integers", None, "-Z sanitizer=cfi"),
    ("proc_macro", Some(false), "--crate-type proc-macro"),
    ("target_feature", Some(true), "-C target-feature"),
    ("unix", Some(false), "--target"),
    ("windows", Some(false), "--target"),
    ("relocation_model", Some(true), "--target"),
    ("target_abi", None, "--target"),
    ("target_arch", Some(true), "--target"),
    ("target_endian", Some(true), "--target"),
    ("target_env", None, "--target"),
    ("target_family", Some(true), "--target"),
    ("target_object_format", Some(true), "--target"),
    ("target_os", Some(true), "--target"),
    ("target_pointer_width", Some(true), "--target"),
    ("target_vendor", None, "--target"),
    ("target_has_atomic", Some(true), "--target"),
    ("target_has_threads", None, "--target"),
    ("target_has_atomic_primitive_alignment", Some(true), "--target"),
    ("target_has_atomic_load_store", Some(true), "--target"),
    ("target_has_reliable_f16", None, "--target"),
    ("target_has_reliable_f16_math", None, "--target"),
    ("target_has_reliable_f128", None, "--target"),
    ("target_has_reliable_f128_math", None, "--target"),
    ("target_thread_local", Some(false), "--target"),
    ("fmt_debug", None, "-Z fmt-debug"),
];

/// The values of `panic` rustc refuses on the command line (`-C panic`): those of its
/// strategies (`sym::immediate_abort` is `"immediate-abort"`).
pub(super) const DISALLOWED_PANIC: &[&str] = &["abort", "unwind", "immediate-abort"];
