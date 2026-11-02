//! What the standard library's own build scripts print (docs/v2/plan/lanes/usercode/scripts.md,
//! "The standard library's own scripts"). std's scripts can't be compiled: blaze has no
//! sysroot, and a host std to build them against is built from this same library, whose units
//! need these scripts' output first. So each one the pinned nightly's `library/` has is ported
//! here, for the exact files it is made of ([`PINNED`], by BLAKE3): what it prints, in its
//! order, from the environment cargo starts it in. A script of other bytes, or of a package
//! with no port, is an error naming it and its hash, never run or guessed at; the nightly bump
//! ports it again.
//!
//! Where a script starts a program, the port answers as that program does for blaze: `$RUSTC`
//! is blaze's rustc stand-in, which says `rustc` [`target::rustc_version`] to `--version`. A
//! program blaze has no answer of (`emcc`, `freebsd-version`, a `RUSTC_WRAPPER`) and the C the
//! `c` feature of compiler_builtins compiles are refused, naming them.

use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::path::Path;

use super::target;

/// A build script of the standard library blaze gives the output of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Script {
    CompilerBuiltins,
    Libc,
    Object,
    Std,
    Test,
}

/// Each script blaze has a port of: its package, and the files it is made of by path under
/// `library/` with their BLAKE3, the script's root first.
pub static PINNED: [(Script, &str, &[(&str, &str)]); 5] = [
    (
        Script::CompilerBuiltins,
        "compiler_builtins",
        &[
            ("compiler-builtins/compiler-builtins/build.rs", "979ca625fb14be73bc07c761f21d37c8cd562a077ba5da3aac5165063fc6e189"),
            ("compiler-builtins/libm/configure.rs", "dd98992599d656cec2ccfc9bcb85116b3eaa698d1109e19b49c89431b4d064c9"),
        ],
    ),
    (Script::Libc, "libc", &[("vendor/libc-0.2.191/build.rs", "9eb10374afeb9cd3215c832b538db85b93f04b88de43f9e5d193b98da6c81100")]),
    (Script::Object, "object", &[("vendor/object-0.39.1/build.rs", "4872405dc10050f2ce4c3e81150a88f1e81fa4ff327c2337e80cabf84446000b")]),
    (Script::Std, "std", &[("std/build.rs", "3be73c2bf16ec86c52fb2d8973c92f1b0ef3c61f9412e8e647f7c71086f00871")]),
    (Script::Test, "test", &[("test/build.rs", "70ac360f5b21e655e46643be153bc59cc8a89f7fc822683c76badef8545a1fc6")]),
];

/// The script of std's package `package` whose root file is at `root` (its compile's
/// `src_path`), checked: every file of it read through `read` (a path named as `root` is) has
/// the BLAKE3 [`PINNED`] says.
pub fn identify<B, R>(package: &str, root: &str, mut read: R) -> Result<Script, String>
where
    B: AsRef<[u8]>,
    R: FnMut(&str) -> Result<B, String>,
{
    let Some((script, _, files)) = PINNED.iter().find(|it| it.1 == package) else {
        return Err(format!( // alloc: error path, the one the build stops with
            "the standard library's `{package}` has a build script ({root}), and blaze has the output of \
             none of that package's (`PINNED` in crates/blaze-units/src/stdlib/scripts.rs): std's own \
             scripts are never compiled, so each is ported there"
        ));
    };
    let first = files[0].0;
    let library = root
        .strip_suffix(first)
        .filter(|it| it.ends_with('/'))
        .ok_or_else(|| format!("std's `{package}` build script is {root}, and blaze's port is of library/{first}"))?; // alloc: error path, the one the build stops with
    for (path, want) in files.iter() {
        let at = format!("{library}{path}"); // alloc: once per file of a std script, once per build
        let bytes = read(&at)?;
        let got = blaze_format::hex::encode(&blaze_ids::b3::hash(bytes.as_ref()));
        if got != *want {
            return Err(format!( // alloc: error path, the one the build stops with
                "{at} has BLAKE3 hash {got}, and blaze gives the output of std's `{package}` build script \
                 whose library/{path} has {want} (`PINNED` in crates/blaze-units/src/stdlib/scripts.rs): \
                 the nightly bump ports the script again"
            ));
        }
    }
    Ok(*script)
}

/// What a run of `script` prints to stdout, started in `cwd` (its package's directory, as
/// cargo starts it) with `vars`, its whole environment sorted by name, each name once
/// (`blaze_build_script::Env::vars`). Where the script would panic, the error says why.
pub fn printed(script: Script, vars: &[(OsString, OsString)], cwd: &Path) -> Result<Vec<u8>, String> {
    let vars = Vars(vars);
    let mut out = String::with_capacity(4096);
    match script {
        Script::CompilerBuiltins => port_compiler_builtins(&vars, cwd, &mut out)?,
        Script::Libc => port_libc(&vars, &mut out)?,
        Script::Object => port_object(&vars, &mut out)?,
        Script::Std => port_std(&vars, &mut out)?,
        Script::Test => port_test(&vars, &mut out)?,
    }
    Ok(out.into_bytes())
}

/// Whether std's build script sets `restricted_std` for a target of these cfgs, when
/// `RUSTC_BOOTSTRAP_SYNTHETIC_TARGET` is not set: on every target it does not list (bare
/// metal, JSON targets, new ones), std is unstable to name.
pub fn restricted_std(arch: &str, os: &str, vendor: &str, env: &str) -> bool {
    let listed = matches!(
        os,
        "linux" | "android" | "netbsd" | "dragonfly" | "openbsd" | "freebsd" | "solaris" | "illumos"
            | "macos" | "ios" | "tvos" | "watchos" | "visionos" | "windows" | "fuchsia" | "motor"
            | "hermit" | "trusty" | "l4re" | "redox" | "haiku" | "vxworks" | "espidf" | "vita"
            | "aix" | "nto" | "qnx" | "xous" | "hurd" | "uefi" | "teeos" | "zkvm" | "rtems"
            | "nuttx" | "cygwin" | "vexos"
    ) || (vendor == "fortanix" && env == "sgx")
        || matches!(arch, "wasm32" | "wasm64")
        || os.starts_with("solid")
        || (vendor == "nintendo" && env == "newlib");
    !listed
}

/// One line of a script's stdout, as `println!` writes it.
macro_rules! say {
    ($out:expr, $($arg:tt)*) => {{
        let _ = writeln!($out, $($arg)*);
    }};
}

/// A script's environment, sorted by name.
struct Vars<'a>(&'a [(OsString, OsString)]);

/// `env::var`'s answer.
#[derive(Clone, Copy)]
enum Var<'a> {
    Set(&'a str),
    NotUnicode,
    NotPresent,
}

impl<'a> Var<'a> {
    fn ok(self) -> Option<&'a str> {
        match self {
            Var::Set(it) => Some(it),
            Var::NotUnicode | Var::NotPresent => None,
        }
    }

    fn is_ok(self) -> bool {
        matches!(self, Var::Set(_))
    }
}

impl<'a> Vars<'a> {
    /// `env::var_os`.
    fn os(&self, name: &str) -> Option<&'a OsStr> {
        let at = self.0.binary_search_by(|(it, _)| it.as_os_str().cmp(OsStr::new(name))).ok()?;
        Some(self.0[at].1.as_os_str())
    }

    /// `env::var`.
    fn var(&self, name: &str) -> Var<'a> {
        match self.os(name) {
            None => Var::NotPresent,
            Some(value) => value.to_str().map_or(Var::NotUnicode, Var::Set),
        }
    }

    /// `env::var(name).unwrap()`.
    fn need(&self, name: &str) -> Result<&'a str, String> {
        match self.var(name) {
            Var::Set(it) => Ok(it),
            Var::NotUnicode => Err(format!("`{name}` is not unicode, and the script unwraps it")), // alloc: error path, the one the build stops with
            Var::NotPresent => Err(format!("`{name}` is not set, and the script unwraps it")), // alloc: error path, the one the build stops with
        }
    }

    /// The scripts' `env_flag`: set to anything but `0`.
    fn flag(&self, name: &str) -> Result<bool, String> {
        match self.var(name) {
            Var::Set(it) => Ok(it != "0"),
            Var::NotPresent => Ok(false),
            Var::NotUnicode => Err(format!("non-unicode var for `{name}`")), // alloc: error path, the one the build stops with
        }
    }

    /// `env::var_os("RUSTC_WRAPPER")`, refused when it names a wrapper: the script would start
    /// it, and blaze answers only for its own rustc.
    fn no_wrapper(&self) -> Result<(), String> {
        match self.os("RUSTC_WRAPPER") {
            Some(it) if !it.is_empty() => Err(format!( // alloc: error path, the one the build stops with
                "`RUSTC_WRAPPER` is {}, which the script would start, and blaze answers only as its own rustc",
                it.to_string_lossy()
            )),
            _ => Ok(()),
        }
    }
}

/// What `$RUSTC --version` prints: blaze's rustc stand-in's answer.
fn rustc_says() -> String {
    format!("rustc {}\n", target::rustc_version()) // alloc: once per std script run, once per build
}

/// The scripts' `vxworks_version_code`: `WIND_RELEASE_ID` as `(major, minor)`.
fn vxworks_version(vars: &Vars<'_>) -> Option<(u32, u32)> {
    let version = vars.var("WIND_RELEASE_ID").ok()?;
    let mut pieces = version.trim().split('.');
    let major: u32 = pieces.next().and_then(|it| it.parse().ok()).unwrap_or(0);
    let minor: u32 = pieces.next().and_then(|it| it.parse().ok()).unwrap_or(0);
    Some((major, minor))
}

/// library/std/build.rs.
fn port_std(vars: &Vars<'_>, out: &mut String) -> Result<(), String> {
    say!(out, "cargo:rerun-if-changed=build.rs");
    let arch = vars.need("CARGO_CFG_TARGET_ARCH")?;
    let os = vars.need("CARGO_CFG_TARGET_OS")?;
    let vendor = vars.need("CARGO_CFG_TARGET_VENDOR")?;
    let env = vars.need("CARGO_CFG_TARGET_ENV")?;
    say!(out, "cargo:rustc-check-cfg=cfg(netbsd10)");
    if os == "netbsd" && vars.var("RUSTC_STD_NETBSD10").is_ok() {
        say!(out, "cargo:rustc-cfg=netbsd10");
    }
    say!(out, "cargo::rustc-check-cfg=cfg(no_global_oom_handling)");
    say!(out, "cargo:rustc-check-cfg=cfg(restricted_std)");
    if restricted_std(arch, os, vendor, env) && !vars.var("RUSTC_BOOTSTRAP_SYNTHETIC_TARGET").is_ok() {
        say!(out, "cargo:rustc-cfg=restricted_std");
    }
    say!(out, "cargo:rustc-check-cfg=cfg(backtrace_in_libstd)");
    say!(out, "cargo:rustc-cfg=backtrace_in_libstd");
    say!(out, "cargo:rustc-env=STD_ENV_ARCH={arch}");
    say!(out, "cargo:rustc-check-cfg=cfg(vxworks_lt_25_09)");
    if os == "vxworks"
        && let Some(version) = vxworks_version(vars)
        && version < (25, 9)
    {
        say!(out, "cargo:rustc-cfg=vxworks_lt_25_09");
    }
    Ok(())
}

/// library/test/build.rs: `$RUSTC -vV`, which blaze's rustc stand-in answers for `HOST`.
fn port_test(vars: &Vars<'_>, out: &mut String) -> Result<(), String> {
    say!(out, "cargo:rerun-if-changed=build.rs");
    say!(out, "cargo:rustc-check-cfg=cfg(enable_unstable_features)");
    vars.need("RUSTC")?;
    let said = target::rustc_verbose_version(vars.need("HOST")?);
    if said.contains("nightly") || said.contains("dev") {
        say!(out, "cargo:rustc-cfg=enable_unstable_features");
    }
    Ok(())
}

/// library/vendor/object-*/build.rs.
fn port_object(vars: &Vars<'_>, out: &mut String) -> Result<(), String> {
    vars.need("RUSTC")?;
    let said = rustc_says();
    let mut pieces = said.split('.');
    let minor: u32 = match pieces.next() {
        Some("rustc 1") => pieces.next().and_then(|it| it.parse().ok()).unwrap_or(0),
        _ => 0,
    };
    if minor >= 81 {
        say!(out, "cargo:rustc-cfg=core_error");
    }
    if minor >= 80 {
        say!(out, "cargo:rustc-check-cfg=cfg(core_error)");
    }
    Ok(())
}

/// The functions compiler_builtins may take from compiler-rt's optimized C
/// (`configure_check_cfg`'s `HAS_OPTIMIZED_C`, its repeat kept).
const HAS_OPTIMIZED_C: [&str; 24] = [
    "__ashldi3",
    "__ashlsi3",
    "__ashrdi3",
    "__ashrsi3",
    "__bswapsi2",
    "__bswapdi2",
    "__bswapti2",
    "__divdi3",
    "__divsi3",
    "__divmoddi4",
    "__divmodsi4",
    "__divmodsi4",
    "__divmodti4",
    "__lshrdi3",
    "__lshrsi3",
    "__moddi3",
    "__modsi3",
    "__muldi3",
    "__udivdi3",
    "__udivmoddi4",
    "__udivmodsi4",
    "__udivsi3",
    "__umoddi3",
    "__umodsi3",
];

/// compiler-builtins/libm/configure.rs's `set_cfg`.
fn set_cfg(out: &mut String, verbose: bool, name: &str, set: bool) {
    say!(out, "cargo:rustc-check-cfg=cfg({name})");
    if !set {
        return;
    }
    if verbose {
        say!(out, "cargo:warning=setting config `{name}`");
    }
    say!(out, "cargo:rustc-cfg={name}");
}

/// library/compiler-builtins/compiler-builtins/build.rs, with libm/configure.rs's
/// `Config::from_env` and `emit` for `Library::CompilerBuiltins`.
fn port_compiler_builtins(vars: &Vars<'_>, cwd: &Path, out: &mut String) -> Result<(), String> {
    say!(out, "cargo:rerun-if-env-changed=LIBM_BUILD_VERBOSE");
    let verbose = vars.flag("LIBM_BUILD_VERBOSE")?;
    let triple = vars.need("TARGET")?;
    let split: Vec<&str> = triple.split('-').collect(); // alloc: once per std script run: the script's own list
    let features: Vec<&str> = match vars.var("CARGO_CFG_TARGET_FEATURE") {
        Var::Set(it) => it.split(',').collect(), // alloc: once per std script run: the script's own list
        Var::NotUnicode | Var::NotPresent => Vec::new(), // alloc: none: an empty `Vec` holds no buffer
    };
    // `env::vars()`, which panics at a variable that is not unicode.
    let mut cargo_features = Vec::new(); // alloc: once per std script run: the script's own list
    for (name, value) in vars.0 {
        let (Some(name), Some(_)) = (name.to_str(), value.to_str()) else {
            return Err(format!("the variable `{}` is not unicode, and the script reads `env::vars()`", name.to_string_lossy())); // alloc: error path, the one the build stops with
        };
        if let Some(feature) = name.strip_prefix("CARGO_FEATURE_") {
            cargo_features.push(feature.to_lowercase().replace('_', "-"));
        }
    }
    if verbose {
        for feature in &cargo_features {
            say!(out, "cargo:warning=feature `{feature}` enabled");
        }
    }
    let manifest_dir = vars.need("CARGO_MANIFEST_DIR")?;
    vars.need("OUT_DIR")?;
    let opt_level = vars.need("OPT_LEVEL")?;
    let arch = vars.need("CARGO_CFG_TARGET_ARCH")?;
    let abi = vars.var("CARGO_CFG_TARGET_ABI").ok();
    vars.need("CARGO_CFG_TARGET_ENV")?;
    let os = vars.need("CARGO_CFG_TARGET_OS")?;
    vars.need("CARGO_CFG_TARGET_VENDOR")?;
    let reliable_f128 = vars.os("CARGO_CFG_TARGET_HAS_RELIABLE_F128").is_some();
    let reliable_f16 = vars.os("CARGO_CFG_TARGET_HAS_RELIABLE_F16").is_some();

    match Path::new(manifest_dir).file_name().is_some_and(|it| it == "builtins-shim") {
        true => say!(out, "cargo::rerun-if-changed=../compiler-builtins/build.rs"),
        false => say!(out, "cargo::rerun-if-changed=build.rs"),
    }
    say!(out, "cargo::rerun-if-changed=../libm/configure.rs");

    // `configure::emit`.
    let opt = !matches!(opt_level, "0" | "1");
    let thumb = split[0].starts_with("thumb");
    let thumb1_only = matches!(split[0], "thumbv4t" | "thumbv5te" | "thumbv6" | "thumbv6m" | "thumbv8m.base");
    let eabihf = abi == Some("eabihf");
    let x86_no_sse2 = arch == "x86" && !features.contains(&"sse2");
    let assert_no_panic = vars.flag("ENSURE_NO_PANIC")?;
    if verbose && !features.is_empty() {
        let has = |name: &str| features.contains(&name);
        let holds = match (thumb, thumb1_only) {
            (_, true) => has("thumb-mode") && !has("thumb2"),
            (true, false) => has("thumb-mode") && has("thumb2"),
            (false, false) => true,
        };
        if !holds {
            return Err(format!( // alloc: error path, the one the build stops with
                "the target features of `{triple}` ({features:?}) are not its thumb mode's, which the script asserts"
            ));
        }
    }
    set_cfg(out, verbose, "thumb", thumb);
    set_cfg(out, verbose, "thumb1_only", thumb1_only);
    set_cfg(out, verbose, "target_abi_eabihf", eabihf);
    set_cfg(out, verbose, "x86_no_sse2", x86_no_sse2);
    say!(out, "cargo:rustc-check-cfg=cfg(target_feature, values(\"vfp2sp\", \"vfp2\"))");
    set_cfg(out, verbose, "assert_no_panic", assert_no_panic);
    set_cfg(out, verbose, "intrinsics_enabled", true);
    set_cfg(out, verbose, "optimizations_enabled", opt);
    set_cfg(out, verbose, "f16_enabled", reliable_f16);
    set_cfg(out, verbose, "f128_enabled", reliable_f128);

    // `configure_check_cfg`.
    for name in HAS_OPTIMIZED_C {
        say!(out, "cargo::rustc-check-cfg=cfg({name}, values(\"optimized-c\"))");
    }
    for op in ["cas", "ldadd", "ldclr", "ldeor", "ldset", "swp"] {
        let sizes: &[u8] = match op {
            "cas" => &[1, 2, 4, 8, 16],
            _ => &[1, 2, 4, 8],
        };
        for size in sizes {
            for ordering in ["relax", "acq", "rel", "acq_rel"] {
                say!(out, "cargo::rustc-check-cfg=cfg(__aarch64_{op}{size}_{ordering}, values(\"optimized-c\"))");
            }
        }
    }
    say!(out, "cargo::rustc-check-cfg=cfg(target_feature, values(\"vis3\"))");

    say!(out, "cargo:compiler-rt={}", cwd.join("compiler-rt").display());
    if (triple.contains("wasm") && !triple.contains("wasi"))
        || (triple.contains("sgx") && triple.contains("fortanix"))
        || triple.contains("-none")
        || triple.contains("nvptx")
        || triple.contains("uefi")
        || triple.contains("xous")
    {
        say!(out, "cargo:rustc-cfg=feature=\"mem\"");
    }
    let mem_unaligned = arch.contains("x86_64") || arch.contains("x86") || arch.contains("aarch64") || arch.contains("bpf");
    set_cfg(out, verbose, "mem_unaligned", mem_unaligned);
    let kernel_user_helpers = split[0] == "armv4t" || split[0] == "armv5te" || triple == "arm-linux-androideabi";
    set_cfg(out, verbose, "kernel_user_helpers", kernel_user_helpers);
    let mut maybe_build_c = os != "emscripten";
    if os == "openbsd" {
        say!(out, "cargo:rustc-link-search=native=/usr/lib");
        say!(out, "cargo:rustc-link-lib=compiler_rt");
        maybe_build_c = false;
    }
    if arch.contains("nvptx") {
        maybe_build_c = false;
    }
    let featured = |name: &str| vars.os(name).is_some();
    if featured("CARGO_FEATURE_UNMANGLED_NAMES") && featured("CARGO_FEATURE_C") && maybe_build_c {
        return Err(
            "its `c` feature (std's `compiler-builtins-c`) compiles compiler-rt's C with the `cc` crate, \
             which blaze's port of the script does not do"
                .to_owned(), // alloc: error path, the one the build stops with
        );
    }
    Ok(())
}

/// libc's cfgs, in its `Cfg`'s order, which its script sorts them by.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Cfg {
    LibcDenyWarnings,
    LibcElfv2,
    LibcPauthtest,
    EmscriptenOldStatAbi,
    EmscriptenFsblkcntIs32bit,
    EmscriptenSigsetLarge,
    EmscriptenNfdsIsULong,
    EspidfPicolibc,
    EspidfTime32,
    Freebsd10,
    Freebsd11,
    Freebsd12,
    Freebsd13,
    Freebsd14,
    Freebsd15,
    VxworksLt25_09,
    LinuxTimeBits64,
    GnuFileOffsetBits64,
    GnuTimeBits64,
    MuslV1_2,
    Musl32Time64,
    MuslRedirTime64,
    Uclibc32Time64,
}

impl Cfg {
    fn name(self) -> &'static str {
        match self {
            Cfg::LibcDenyWarnings => "libc_deny_warnings",
            Cfg::EmscriptenOldStatAbi => "emscripten_old_stat_abi",
            Cfg::EmscriptenFsblkcntIs32bit => "emscripten_fsblkcnt_is_32bit",
            Cfg::EmscriptenSigsetLarge => "emscripten_sigset_large",
            Cfg::EmscriptenNfdsIsULong => "emscripten_nfds_is_ulong",
            Cfg::EspidfPicolibc => "espidf_picolibc",
            Cfg::EspidfTime32 => "espidf_time32",
            Cfg::Freebsd10 => "freebsd10",
            Cfg::Freebsd11 => "freebsd11",
            Cfg::Freebsd12 => "freebsd12",
            Cfg::Freebsd13 => "freebsd13",
            Cfg::Freebsd14 => "freebsd14",
            Cfg::Freebsd15 => "freebsd15",
            Cfg::LibcElfv2 => "libc_elfv2",
            Cfg::VxworksLt25_09 => "vxworks_lt_25_09",
            Cfg::LibcPauthtest => "libc_pauthtest",
            Cfg::GnuFileOffsetBits64 => "gnu_file_offset_bits64",
            Cfg::GnuTimeBits64 => "gnu_time_bits64",
            Cfg::LinuxTimeBits64 => "linux_time_bits64",
            Cfg::MuslV1_2 => "musl_v1_2",
            Cfg::Musl32Time64 => "musl32_time64",
            Cfg::MuslRedirTime64 => "musl_redir_time64",
            Cfg::Uclibc32Time64 => "uclibc32_time64",
        }
    }
}

/// libc's `ALLOWED_CFGS`, in its order: the check-cfgs it prints.
const ALLOWED_CFGS: [Cfg; 23] = [
    Cfg::LibcDenyWarnings,
    Cfg::EmscriptenOldStatAbi,
    Cfg::EmscriptenFsblkcntIs32bit,
    Cfg::EmscriptenSigsetLarge,
    Cfg::EmscriptenNfdsIsULong,
    Cfg::EspidfPicolibc,
    Cfg::EspidfTime32,
    Cfg::Freebsd10,
    Cfg::Freebsd11,
    Cfg::Freebsd12,
    Cfg::Freebsd13,
    Cfg::Freebsd14,
    Cfg::Freebsd15,
    Cfg::LibcElfv2,
    Cfg::VxworksLt25_09,
    Cfg::LibcPauthtest,
    Cfg::GnuFileOffsetBits64,
    Cfg::GnuTimeBits64,
    Cfg::LinuxTimeBits64,
    Cfg::MuslV1_2,
    Cfg::Musl32Time64,
    Cfg::MuslRedirTime64,
    Cfg::Uclibc32Time64,
];

/// libc's `CHECK_CFG_EXTRA`.
const CHECK_CFG_EXTRA: [(&str, &[&str]); 3] = [
    ("target_os", &["switch", "aix", "ohos", "hurd", "rtems", "visionos", "nuttx", "cygwin", "qurt", "qnx", "helenos"]),
    ("target_env", &["illumos", "wasi", "aix", "ohos", "nto71_iosock"]),
    ("target_arch", &["loongarch64", "mips32r6", "mips64r6", "csky"]),
];

/// library/vendor/libc-*/build.rs.
fn port_libc(vars: &Vars<'_>, out: &mut String) -> Result<(), String> {
    say!(out, "cargo:rerun-if-changed=build.rs");
    say!(out, "cargo:rerun-if-env-changed=LIBC_BUILD_VERBOSE");
    let verbose = vars.flag("LIBC_BUILD_VERBOSE")?;
    let mut cfgs: Vec<Cfg> = Vec::new(); // alloc: once per std script run: the script's own list

    // `rustc_minor_nightly`, of `$RUSTC --version`.
    if vars.os("RUSTC").is_none() {
        return Err("Failed to get rustc version: missing RUSTC env".to_owned()); // alloc: error path, the one the build stops with
    }
    vars.no_wrapper()?;
    let said = rustc_says();
    let failed = || "Failed to get rustc version".to_owned(); // alloc: error path, the one the build stops with
    let mut pieces = said.split('.');
    if pieces.next() != Some("rustc 1") {
        return Err(failed());
    }
    let minor = pieces.next();
    let nightly = pieces
        .next()
        .ok_or_else(failed)?
        .split('-')
        .nth(1)
        .is_some_and(|raw| raw.starts_with("dev") || raw.starts_with("nightly"));
    let rustc_minor: u32 = minor.ok_or_else(failed)?.parse().map_err(|_| failed())?;
    if verbose {
        say!(out, "cargo:warning=info: detected rust 1.{rustc_minor}, nightly={nightly}");
    }

    let libc_ci = vars.flag("LIBC_CI")?;
    let or_empty = |name: &str| vars.var(name).ok().unwrap_or("");
    let target_env = or_empty("CARGO_CFG_TARGET_ENV");
    let target_os = or_empty("CARGO_CFG_TARGET_OS");
    let ptr_width = or_empty("CARGO_CFG_TARGET_POINTER_WIDTH");
    let target_arch = or_empty("CARGO_CFG_TARGET_ARCH");
    let target_abi = or_empty("CARGO_CFG_TARGET_ABI");

    if target_abi == "elfv2" {
        cfgs.push(Cfg::LibcElfv2);
    }
    if target_abi == "pauthtest" {
        cfgs.push(Cfg::LibcPauthtest);
    }

    say!(out, "cargo:rerun-if-env-changed=RUST_LIBC_UNSTABLE_FREEBSD_VERSION");
    if vars.var("RUST_LIBC_UNSTABLE_FREEBSD_VERSION").is_ok() {
        say!(
            out,
            "cargo:warning=RUST_LIBC_UNSTABLE_FREEBSD_VERSION has been removed; set the cfg libc_unstable_freebsd_version via RUSTFLAGS instead"
        );
    }

    if target_os == "freebsd" {
        let which: i32 = if let Var::Set(version) = vars.var("CARGO_CFG_LIBC_UNSTABLE_FREEBSD_VERSION") {
            let version: i32 = version
                .parse()
                .map_err(|_| format!("`CARGO_CFG_LIBC_UNSTABLE_FREEBSD_VERSION` is {version}, which the script unwraps as a number"))?; // alloc: error path, the one the build stops with
            say!(out, "cargo:warning=setting FreeBSD version to {version}");
            version
        } else if libc_ci {
            return Err("under `LIBC_CI` the script starts `freebsd-version`, which blaze has no answer of".to_owned()); // alloc: error path, the one the build stops with
        } else {
            12
        };
        cfgs.push(match which {
            x if x < 10 => return Err("FreeBSD older than 10 is not supported".to_owned()), // alloc: error path, the one the build stops with
            10 => Cfg::Freebsd10,
            11 => Cfg::Freebsd11,
            12 => Cfg::Freebsd12,
            13 => Cfg::Freebsd13,
            14 => Cfg::Freebsd14,
            _ => Cfg::Freebsd15,
        });
    }

    if target_os == "emscripten" {
        return Err("for emscripten the script starts `emcc -dumpversion`, which blaze has no answer of".to_owned()); // alloc: error path, the one the build stops with
    }

    if target_os == "vxworks" && vxworks_version(vars).is_some_and(|it| it < (25, 9)) {
        cfgs.push(Cfg::VxworksLt25_09);
    }

    let mut musl_v1_2_env = vars.flag("CARGO_CFG_LIBC_UNSTABLE_MUSL_V1_2")?;
    if let Var::Set(old) = vars.var("CARGO_CFG_LIBC_UNSTABLE_MUSL_V1_2_3") {
        if !musl_v1_2_env {
            say!(
                out,
                "cargo:warning=`--cfg=libc_unstable_musl_v1_2_3` will be removed; set `--cfg=libc_unstable_musl_v1_2`instead"
            );
        }
        musl_v1_2_env |= old != "0";
    }
    if let Var::Set(old) = vars.var("RUST_LIBC_UNSTABLE_MUSL_V1_2_3") {
        if !musl_v1_2_env {
            say!(
                out,
                "cargo:warning=RUST_LIBC_UNSTABLE_MUSL_V1_2_3 will be removed; set `--cfg=libc_unstable_musl_v1_2` via RUSTFLAGS instead"
            );
        }
        musl_v1_2_env |= old != "0";
    }

    let only_v1_2_on_musl = target_arch == "loongarch64"
        || target_arch == "hexagon"
        || target_arch == "riscv32"
        || target_env == "ohos"
        || target_abi == "pauthtest";
    let time64 = vars.flag("CARGO_CFG_LIBC_UNSTABLE_TIME64")?;
    let musl = target_env == "musl" || target_env == "ohos";
    if musl && (musl_v1_2_env || only_v1_2_on_musl || time64) {
        cfgs.push(Cfg::MuslV1_2);
        cfgs.push(Cfg::LinuxTimeBits64);
        if ptr_width == "32" {
            cfgs.push(Cfg::Musl32Time64);
            if !only_v1_2_on_musl {
                cfgs.push(Cfg::MuslRedirTime64);
            }
        }
    }

    let mut gnu_time_bits = vars.var("CARGO_CFG_LIBC_UNSTABLE_GNU_TIME_BITS").ok();
    if let Var::Set(old) = vars.var("RUST_LIBC_UNSTABLE_GNU_TIME_BITS") {
        say!(
            out,
            "cargo:warning=RUST_LIBC_UNSTABLE_GNU_TIME_BITS will be removed; set `--cfg=libc_unstable_gnu_time_bits=\"...\"` via RUSTFLAGS instead"
        );
        gnu_time_bits = gnu_time_bits.or(Some(old));
    }
    if vars.var("RUST_LIBC_UNSTABLE_GNU_FILE_OFFSET_BITS").is_ok()
        || vars.var("CARGO_CFG_LIBC_UNSTABLE_GNU_FILE_OFFSET_BITS").is_ok()
    {
        say!(out, "cargo:warning=glibc file offset can no longer be set independently of `gnu_time_bits`");
    }
    let gnu32_timebits = match gnu_time_bits {
        None => "32",
        Some(bits @ ("64" | "32")) => bits,
        Some(_) => return Err("Invalid value for libc_unstable_gnu_time_bits. Must be 32, 64, or unset.".to_owned()), // alloc: error path, the one the build stops with
    };

    let gnu32_already_time64 =
        target_arch == "riscv32" || target_arch == "x86_64" || !matches!(target_os, "linux" | "windows" | "hurd");
    let gnu = target_env == "gnu";
    let gnu32_time64 = gnu && ptr_width == "32" && (gnu32_timebits == "64" || time64);
    if gnu32_time64 && !gnu32_already_time64 {
        if target_os == "linux" {
            cfgs.push(Cfg::LinuxTimeBits64);
        }
        cfgs.push(Cfg::GnuFileOffsetBits64);
        cfgs.push(Cfg::GnuTimeBits64);
    }

    let uclibc_time64 = vars.flag("CARGO_CFG_LIBC_UNSTABLE_UCLIBC_TIME64")?;
    if target_env == "uclibc" && ptr_width == "32" && (uclibc_time64 || time64) {
        if target_os == "linux" {
            cfgs.push(Cfg::LinuxTimeBits64);
        }
        cfgs.push(Cfg::Uclibc32Time64);
    }

    if target_os == "linux" && ptr_width == "64" {
        cfgs.push(Cfg::LinuxTimeBits64);
    }
    if libc_ci {
        cfgs.push(Cfg::LibcDenyWarnings);
    }

    if rustc_minor >= 80 {
        for cfg in ALLOWED_CFGS {
            say!(out, "cargo:rustc-check-cfg=cfg({})", cfg.name());
        }
        for (name, values) in CHECK_CFG_EXTRA {
            say!(out, "cargo:rustc-check-cfg=cfg({name},values(\"{}\"))", values.join("\",\""));
        }
    }

    cfgs.sort_unstable();
    cfgs.dedup();
    libc_valid(&cfgs, target_env, target_os, ptr_width)?;
    for cfg in cfgs {
        say!(out, "cargo:rustc-cfg={}", cfg.name());
        if verbose {
            say!(out, "cargo:warning=info: setting config `{}`", cfg.name());
        }
    }
    Ok(())
}

/// libc's `validate_cfg`: what each cfg set implies of the target, which the script asserts.
fn libc_valid(list: &[Cfg], env: &str, os: &str, ptr_width: &str) -> Result<(), String> {
    for cfg in list {
        let holds = match cfg {
            Cfg::LibcDenyWarnings | Cfg::LibcElfv2 | Cfg::LibcPauthtest | Cfg::EspidfPicolibc | Cfg::EspidfTime32 => true,
            Cfg::EmscriptenOldStatAbi | Cfg::EmscriptenFsblkcntIs32bit | Cfg::EmscriptenSigsetLarge | Cfg::EmscriptenNfdsIsULong => {
                os == "emscripten"
            }
            Cfg::Freebsd10 | Cfg::Freebsd11 | Cfg::Freebsd12 | Cfg::Freebsd13 | Cfg::Freebsd14 | Cfg::Freebsd15 => os == "freebsd",
            Cfg::VxworksLt25_09 => os == "vxworks",
            Cfg::GnuFileOffsetBits64 => env == "gnu" && ptr_width == "32",
            Cfg::GnuTimeBits64 => env == "gnu" && ptr_width == "32" && list.contains(&Cfg::GnuFileOffsetBits64),
            Cfg::LinuxTimeBits64 => os == "linux",
            Cfg::MuslV1_2 => matches!(env, "musl" | "ohos"),
            Cfg::Musl32Time64 | Cfg::MuslRedirTime64 => {
                matches!(env, "musl" | "ohos") && ptr_width == "32" && list.contains(&Cfg::MuslV1_2)
            }
            Cfg::Uclibc32Time64 => {
                env == "uclibc" && ptr_width == "32" && (os != "linux" || list.contains(&Cfg::LinuxTimeBits64))
            }
        };
        if !holds {
            return Err(format!("{cfg:?} set on target_os {os:?}, target_env {env:?}, pointer width {ptr_width:?}")); // alloc: error path, the one the build stops with
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
