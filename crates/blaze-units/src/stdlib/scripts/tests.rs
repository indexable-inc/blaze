use std::ffi::OsString;
use std::path::Path;

use super::super::target::{self, CfgArgs, cargo_cfg_env, target_cfg};
use super::*;

const LIBRARY: &str = "/rustc/16d1ffac17ad8b739e32483ac2e65fe7e1804977/library/";
const BUILTINS_DIR: &str = "/rustc/16d1ffac17ad8b739e32483ac2e65fe7e1804977/library/compiler-builtins/compiler-builtins";

/// The checkout's `library/`, where a path named under [`LIBRARY`] is read from.
fn checkout(path: &str) -> Result<Vec<u8>, String> {
    let rest = path.strip_prefix(LIBRARY).ok_or_else(|| format!("{path} is not under the library"))?;
    let at = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../library").join(rest);
    std::fs::read(&at).map_err(|error| format!("{}: {error}", at.display()))
}

fn vars(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
    let mut vars: Vec<(OsString, OsString)> = pairs.iter().map(|(name, value)| (name.into(), value.into())).collect();
    vars.sort_by(|a, b| a.0.cmp(&b.0));
    vars.dedup_by(|later, first| later.0 == first.0);
    vars
}

fn lines(script: Script, pairs: &[(&str, &str)]) -> Vec<String> {
    let printed = printed(script, &vars(pairs), Path::new(BUILTINS_DIR)).unwrap();
    String::from_utf8(printed).unwrap().lines().map(str::to_owned).collect()
}

fn refused(script: Script, pairs: &[(&str, &str)]) -> String {
    printed(script, &vars(pairs), Path::new(BUILTINS_DIR)).unwrap_err()
}

/// What cargo gives a script's run for `triple`: its cfgs as `CARGO_CFG_*` (cargo's probe,
/// with `--crate-type proc-macro`), and the variables every run has.
fn cargo_env(triple: &str) -> Vec<(String, String)> {
    let cfgs = target_cfg(triple, &CfgArgs { proc_macro: true, ..CfgArgs::default() })
        .unwrap_or_else(|error| panic!("the cfgs of {triple}: {error}"));
    let mut env = cargo_cfg_env(&cfgs);
    for (name, value) in [
        ("TARGET", triple),
        ("HOST", "aarch64-apple-darwin"),
        ("OUT_DIR", "/t/build/out"),
        ("OPT_LEVEL", "0"),
        ("CARGO_MANIFEST_DIR", BUILTINS_DIR),
        ("RUSTC", "/blaze/tools/rustc"),
    ] {
        env.push((name.to_owned(), value.to_owned()));
    }
    env
}

const APPLE: [(&str, &str); 6] = [
    ("CARGO_CFG_TARGET_ARCH", "aarch64"),
    ("CARGO_CFG_TARGET_OS", "macos"),
    ("CARGO_CFG_TARGET_VENDOR", "apple"),
    ("CARGO_CFG_TARGET_ENV", ""),
    ("CARGO_CFG_TARGET_POINTER_WIDTH", "64"),
    ("RUSTC", "/blaze/tools/rustc"),
];

fn with<'a>(base: &[(&'a str, &'a str)], extra: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
    // `vars` keeps a name's first after a stable sort, so an extra goes first to win.
    extra.iter().chain(base).copied().collect()
}

#[test]
fn every_pinned_file_is_the_checkouts() {
    for (script, package, files) in PINNED {
        for (path, want) in files {
            let bytes = checkout(&format!("{LIBRARY}{path}")).unwrap();
            assert_eq!(&blaze_format::hex::encode(&blaze_ids::b3::hash(&bytes)), want, "library/{path}");
        }
        let root = format!("{LIBRARY}{}", files[0].0);
        assert_eq!(identify(package, &root, checkout), Ok(script));
    }
}

#[test]
fn a_script_of_other_bytes_is_refused_naming_its_hash() {
    let root = format!("{LIBRARY}compiler-builtins/compiler-builtins/build.rs");
    let changed = |path: &str| -> Result<Vec<u8>, String> {
        let mut bytes = checkout(path)?;
        if path.ends_with("configure.rs") {
            bytes.extend_from_slice(b"\n// changed\n");
        }
        Ok(bytes)
    };
    let error = identify("compiler_builtins", &root, changed).unwrap_err();
    assert!(error.contains("compiler-builtins/libm/configure.rs has BLAKE3 hash"), "{error}");
    assert!(error.contains("dd98992599d656cec2ccfc9bcb85116b3eaa698d1109e19b49c89431b4d064c9"), "{error}");
}

#[test]
fn a_script_at_another_path_or_of_another_package_is_refused() {
    let bumped = format!("{LIBRARY}vendor/libc-0.2.192/build.rs");
    let error = identify("libc", &bumped, checkout).unwrap_err();
    assert!(error.contains("library/vendor/libc-0.2.191/build.rs"), "{error}");
    let other = format!("{LIBRARY}profiler_builtins/build.rs");
    let error = identify("profiler_builtins", &other, checkout).unwrap_err();
    assert!(error.contains("`profiler_builtins`"), "{error}");
}

#[test]
fn std_prints_what_its_script_prints() {
    let base = [
        "cargo:rerun-if-changed=build.rs",
        "cargo:rustc-check-cfg=cfg(netbsd10)",
        "cargo::rustc-check-cfg=cfg(no_global_oom_handling)",
        "cargo:rustc-check-cfg=cfg(restricted_std)",
        "cargo:rustc-check-cfg=cfg(backtrace_in_libstd)",
        "cargo:rustc-cfg=backtrace_in_libstd",
        "cargo:rustc-env=STD_ENV_ARCH=aarch64",
        "cargo:rustc-check-cfg=cfg(vxworks_lt_25_09)",
    ];
    assert_eq!(lines(Script::Std, &APPLE), base);

    let bare = [
        ("CARGO_CFG_TARGET_ARCH", "arm"),
        ("CARGO_CFG_TARGET_OS", "none"),
        ("CARGO_CFG_TARGET_VENDOR", "unknown"),
        ("CARGO_CFG_TARGET_ENV", ""),
    ];
    let got = lines(Script::Std, &bare);
    assert_eq!(got[3..5], ["cargo:rustc-check-cfg=cfg(restricted_std)", "cargo:rustc-cfg=restricted_std"]);
    assert!(got.contains(&"cargo:rustc-env=STD_ENV_ARCH=arm".to_owned()));
    let synthetic = lines(Script::Std, &with(&bare, &[("RUSTC_BOOTSTRAP_SYNTHETIC_TARGET", "1")]));
    assert!(!synthetic.iter().any(|it| it == "cargo:rustc-cfg=restricted_std"));

    let netbsd = [
        ("CARGO_CFG_TARGET_ARCH", "x86_64"),
        ("CARGO_CFG_TARGET_OS", "netbsd"),
        ("CARGO_CFG_TARGET_VENDOR", "unknown"),
        ("CARGO_CFG_TARGET_ENV", ""),
    ];
    assert!(!lines(Script::Std, &netbsd).iter().any(|it| it == "cargo:rustc-cfg=netbsd10"));
    let ten = lines(Script::Std, &with(&netbsd, &[("RUSTC_STD_NETBSD10", "")]));
    assert_eq!(ten[1..3], ["cargo:rustc-check-cfg=cfg(netbsd10)", "cargo:rustc-cfg=netbsd10"]);

    let vxworks = [
        ("CARGO_CFG_TARGET_ARCH", "aarch64"),
        ("CARGO_CFG_TARGET_OS", "vxworks"),
        ("CARGO_CFG_TARGET_VENDOR", "wrs"),
        ("CARGO_CFG_TARGET_ENV", "gnu"),
    ];
    let old = lines(Script::Std, &with(&vxworks, &[("WIND_RELEASE_ID", " 24.03 ")]));
    assert_eq!(old.last().unwrap(), "cargo:rustc-cfg=vxworks_lt_25_09");
    let new = lines(Script::Std, &with(&vxworks, &[("WIND_RELEASE_ID", "25.09")]));
    assert_eq!(new.last().unwrap(), "cargo:rustc-check-cfg=cfg(vxworks_lt_25_09)");
    assert_eq!(lines(Script::Std, &vxworks).last().unwrap(), "cargo:rustc-check-cfg=cfg(vxworks_lt_25_09)");

    let error = refused(Script::Std, &APPLE[1..]);
    assert!(error.contains("`CARGO_CFG_TARGET_ARCH` is not set"), "{error}");
}

#[test]
fn the_restricted_targets_are_std_scripts() {
    assert!(!restricted_std("x86_64", "linux", "unknown", "gnu"));
    assert!(!restricted_std("aarch64", "macos", "apple", ""));
    assert!(!restricted_std("wasm32", "unknown", "unknown", ""));
    assert!(!restricted_std("x86_64", "unknown", "fortanix", "sgx"));
    assert!(!restricted_std("arm", "solid_asp3", "kmc", ""));
    assert!(!restricted_std("armv7", "horizon", "nintendo", "newlib"));
    assert!(restricted_std("arm", "none", "unknown", ""));
    assert!(restricted_std("mips", "psp", "sony", ""));
    assert!(restricted_std("nvptx64", "cuda", "nvidia", ""));
    assert!(restricted_std("avr", "none", "unknown", ""));
}

#[test]
fn object_prints_what_its_script_prints_of_blazes_rustc() {
    let minor: u32 = target::rustc_version().split('.').nth(1).unwrap().parse().unwrap();
    assert!(minor >= 81, "{}", target::rustc_version());
    assert_eq!(lines(Script::Object, &APPLE), ["cargo:rustc-cfg=core_error", "cargo:rustc-check-cfg=cfg(core_error)"]);
    let error = refused(Script::Object, &APPLE[..5]);
    assert!(error.contains("`RUSTC` is not set"), "{error}");
}

#[test]
fn test_prints_what_its_script_prints_of_blazes_rustc() {
    assert!(target::rustc_verbose_version("aarch64-apple-darwin").contains("nightly"));
    let host = with(&APPLE, &[("HOST", "aarch64-apple-darwin")]);
    assert_eq!(
        lines(Script::Test, &host),
        [
            "cargo:rerun-if-changed=build.rs",
            "cargo:rustc-check-cfg=cfg(enable_unstable_features)",
            "cargo:rustc-cfg=enable_unstable_features",
        ]
    );
    let error = refused(Script::Test, &APPLE);
    assert!(error.contains("`HOST` is not set"), "{error}");
    let error = refused(Script::Test, &with(&APPLE[..5], &[("HOST", "aarch64-apple-darwin")]));
    assert!(error.contains("`RUSTC` is not set"), "{error}");
}

/// libc's check-cfgs, as its script prints them past Rust 1.80.
const LIBC_CHECK_CFG: [&str; 26] = [
    "cargo:rustc-check-cfg=cfg(libc_deny_warnings)",
    "cargo:rustc-check-cfg=cfg(emscripten_old_stat_abi)",
    "cargo:rustc-check-cfg=cfg(emscripten_fsblkcnt_is_32bit)",
    "cargo:rustc-check-cfg=cfg(emscripten_sigset_large)",
    "cargo:rustc-check-cfg=cfg(emscripten_nfds_is_ulong)",
    "cargo:rustc-check-cfg=cfg(espidf_picolibc)",
    "cargo:rustc-check-cfg=cfg(espidf_time32)",
    "cargo:rustc-check-cfg=cfg(freebsd10)",
    "cargo:rustc-check-cfg=cfg(freebsd11)",
    "cargo:rustc-check-cfg=cfg(freebsd12)",
    "cargo:rustc-check-cfg=cfg(freebsd13)",
    "cargo:rustc-check-cfg=cfg(freebsd14)",
    "cargo:rustc-check-cfg=cfg(freebsd15)",
    "cargo:rustc-check-cfg=cfg(libc_elfv2)",
    "cargo:rustc-check-cfg=cfg(vxworks_lt_25_09)",
    "cargo:rustc-check-cfg=cfg(libc_pauthtest)",
    "cargo:rustc-check-cfg=cfg(gnu_file_offset_bits64)",
    "cargo:rustc-check-cfg=cfg(gnu_time_bits64)",
    "cargo:rustc-check-cfg=cfg(linux_time_bits64)",
    "cargo:rustc-check-cfg=cfg(musl_v1_2)",
    "cargo:rustc-check-cfg=cfg(musl32_time64)",
    "cargo:rustc-check-cfg=cfg(musl_redir_time64)",
    "cargo:rustc-check-cfg=cfg(uclibc32_time64)",
    "cargo:rustc-check-cfg=cfg(target_os,values(\"switch\",\"aix\",\"ohos\",\"hurd\",\"rtems\",\"visionos\",\"nuttx\",\"cygwin\",\"qurt\",\"qnx\",\"helenos\"))",
    "cargo:rustc-check-cfg=cfg(target_env,values(\"illumos\",\"wasi\",\"aix\",\"ohos\",\"nto71_iosock\"))",
    "cargo:rustc-check-cfg=cfg(target_arch,values(\"loongarch64\",\"mips32r6\",\"mips64r6\",\"csky\"))",
];

fn libc_expected(head: &[&str], cfgs: &[&str]) -> Vec<String> {
    let mut want: Vec<String> = [
        "cargo:rerun-if-changed=build.rs",
        "cargo:rerun-if-env-changed=LIBC_BUILD_VERBOSE",
    ]
    .iter()
    .chain(head)
    .map(|it| (*it).to_owned())
    .collect();
    want.push("cargo:rerun-if-env-changed=RUST_LIBC_UNSTABLE_FREEBSD_VERSION".to_owned());
    want.extend(LIBC_CHECK_CFG.iter().map(|it| (*it).to_owned()));
    want.extend(cfgs.iter().map(|it| format!("cargo:rustc-cfg={it}")));
    want
}

#[test]
fn libc_prints_what_its_script_prints() {
    assert_eq!(lines(Script::Libc, &APPLE), libc_expected(&[], &[]));

    let linux = [
        ("CARGO_CFG_TARGET_ARCH", "x86_64"),
        ("CARGO_CFG_TARGET_OS", "linux"),
        ("CARGO_CFG_TARGET_VENDOR", "unknown"),
        ("CARGO_CFG_TARGET_ENV", "gnu"),
        ("CARGO_CFG_TARGET_POINTER_WIDTH", "64"),
        ("RUSTC", "/blaze/tools/rustc"),
    ];
    assert_eq!(lines(Script::Libc, &linux), libc_expected(&[], &["linux_time_bits64"]));

    let i686 = [
        ("CARGO_CFG_TARGET_ARCH", "x86"),
        ("CARGO_CFG_TARGET_OS", "linux"),
        ("CARGO_CFG_TARGET_ENV", "gnu"),
        ("CARGO_CFG_TARGET_POINTER_WIDTH", "32"),
        ("RUSTC", "/blaze/tools/rustc"),
    ];
    assert_eq!(lines(Script::Libc, &i686), libc_expected(&[], &[]));
    let time64 = with(&i686, &[("CARGO_CFG_LIBC_UNSTABLE_GNU_TIME_BITS", "64")]);
    assert_eq!(
        lines(Script::Libc, &time64),
        libc_expected(&[], &["linux_time_bits64", "gnu_file_offset_bits64", "gnu_time_bits64"])
    );
    let error = refused(Script::Libc, &with(&i686, &[("CARGO_CFG_LIBC_UNSTABLE_GNU_TIME_BITS", "48")]));
    assert!(error.contains("Must be 32, 64, or unset"), "{error}");

    let musl32 = [
        ("CARGO_CFG_TARGET_ARCH", "arm"),
        ("CARGO_CFG_TARGET_OS", "linux"),
        ("CARGO_CFG_TARGET_ENV", "musl"),
        ("CARGO_CFG_TARGET_POINTER_WIDTH", "32"),
        ("CARGO_CFG_LIBC_UNSTABLE_MUSL_V1_2", "1"),
        ("RUSTC", "/blaze/tools/rustc"),
    ];
    assert_eq!(
        lines(Script::Libc, &musl32),
        libc_expected(&[], &["linux_time_bits64", "musl_v1_2", "musl32_time64", "musl_redir_time64"])
    );

    let freebsd = [
        ("CARGO_CFG_TARGET_ARCH", "x86_64"),
        ("CARGO_CFG_TARGET_OS", "freebsd"),
        ("CARGO_CFG_TARGET_POINTER_WIDTH", "64"),
        ("RUSTC", "/blaze/tools/rustc"),
    ];
    assert_eq!(lines(Script::Libc, &freebsd), libc_expected(&[], &["freebsd12"]));
    let pinned = lines(Script::Libc, &with(&freebsd, &[("CARGO_CFG_LIBC_UNSTABLE_FREEBSD_VERSION", "14")]));
    assert!(pinned.contains(&"cargo:warning=setting FreeBSD version to 14".to_owned()), "{pinned:?}");
    assert_eq!(pinned.last().unwrap(), "cargo:rustc-cfg=freebsd14");
    let error = refused(Script::Libc, &with(&freebsd, &[("LIBC_CI", "1")]));
    assert!(error.contains("freebsd-version"), "{error}");

    let verbose = lines(Script::Libc, &with(&linux, &[("LIBC_BUILD_VERBOSE", "1")]));
    let minor = target::rustc_version().split('.').nth(1).unwrap();
    assert_eq!(verbose[2], format!("cargo:warning=info: detected rust 1.{minor}, nightly=true"));
    assert_eq!(verbose.last().unwrap(), "cargo:warning=info: setting config `linux_time_bits64`");
    let quiet = lines(Script::Libc, &with(&linux, &[("LIBC_BUILD_VERBOSE", "0")]));
    assert_eq!(quiet, libc_expected(&[], &["linux_time_bits64"]));

    let error = refused(Script::Libc, &with(&linux, &[("RUSTC_WRAPPER", "/usr/bin/sccache")]));
    assert!(error.contains("RUSTC_WRAPPER"), "{error}");
    assert_eq!(lines(Script::Libc, &with(&linux, &[("RUSTC_WRAPPER", "")])), libc_expected(&[], &["linux_time_bits64"]));
    let error = refused(Script::Libc, &linux[..5]);
    assert!(error.contains("missing RUSTC env"), "{error}");
}

fn builtins_env(triple: &str, arch: &str, os: &str, opt: &str) -> Vec<(&'static str, String)> {
    vec![
        ("TARGET", triple.to_owned()),
        ("CARGO_MANIFEST_DIR", BUILTINS_DIR.to_owned()),
        ("OUT_DIR", "/t/build/out".to_owned()),
        ("OPT_LEVEL", opt.to_owned()),
        ("CARGO_CFG_TARGET_ARCH", arch.to_owned()),
        ("CARGO_CFG_TARGET_ENV", String::new()),
        ("CARGO_CFG_TARGET_OS", os.to_owned()),
        ("CARGO_CFG_TARGET_VENDOR", "unknown".to_owned()),
        ("CARGO_CFG_TARGET_FEATURE", "neon,fp-armv8".to_owned()),
        ("CARGO_FEATURE_COMPILER_BUILTINS", String::new()),
        ("CARGO_FEATURE_UNMANGLED_NAMES", String::new()),
    ]
}

fn builtins(pairs: &[(&'static str, String)]) -> Result<Vec<String>, String> {
    let borrowed: Vec<(&str, &str)> = pairs.iter().map(|(name, value)| (*name, value.as_str())).collect();
    let printed = printed(Script::CompilerBuiltins, &vars(&borrowed), Path::new(BUILTINS_DIR))?;
    Ok(String::from_utf8(printed).unwrap().lines().map(str::to_owned).collect())
}

#[test]
fn compiler_builtins_prints_what_its_script_prints() {
    let got = builtins(&builtins_env("aarch64-apple-darwin", "aarch64", "macos", "0")).unwrap();
    let head = [
        "cargo:rerun-if-env-changed=LIBM_BUILD_VERBOSE",
        "cargo::rerun-if-changed=build.rs",
        "cargo::rerun-if-changed=../libm/configure.rs",
        "cargo:rustc-check-cfg=cfg(thumb)",
        "cargo:rustc-check-cfg=cfg(thumb1_only)",
        "cargo:rustc-check-cfg=cfg(target_abi_eabihf)",
        "cargo:rustc-check-cfg=cfg(x86_no_sse2)",
        "cargo:rustc-check-cfg=cfg(target_feature, values(\"vfp2sp\", \"vfp2\"))",
        "cargo:rustc-check-cfg=cfg(assert_no_panic)",
        "cargo:rustc-check-cfg=cfg(intrinsics_enabled)",
        "cargo:rustc-cfg=intrinsics_enabled",
        "cargo:rustc-check-cfg=cfg(optimizations_enabled)",
        "cargo:rustc-check-cfg=cfg(f16_enabled)",
        "cargo:rustc-check-cfg=cfg(f128_enabled)",
    ];
    assert_eq!(got[..head.len()], head);
    // 24 of compiler-rt's optimized C, 20 `cas` and 80 more of aarch64's atomics.
    let optimized: Vec<&String> = got.iter().filter(|it| it.ends_with("values(\"optimized-c\"))")).collect();
    assert_eq!(optimized.len(), 124);
    assert_eq!(optimized[0], "cargo::rustc-check-cfg=cfg(__ashldi3, values(\"optimized-c\"))");
    assert_eq!(optimized[24], "cargo::rustc-check-cfg=cfg(__aarch64_cas1_relax, values(\"optimized-c\"))");
    assert_eq!(optimized[43], "cargo::rustc-check-cfg=cfg(__aarch64_cas16_acq_rel, values(\"optimized-c\"))");
    assert_eq!(optimized[123], "cargo::rustc-check-cfg=cfg(__aarch64_swp8_acq_rel, values(\"optimized-c\"))");
    let tail = [
        "cargo::rustc-check-cfg=cfg(target_feature, values(\"vis3\"))".to_owned(),
        format!("cargo:compiler-rt={BUILTINS_DIR}/compiler-rt"),
        "cargo:rustc-check-cfg=cfg(mem_unaligned)".to_owned(),
        "cargo:rustc-cfg=mem_unaligned".to_owned(),
        "cargo:rustc-check-cfg=cfg(kernel_user_helpers)".to_owned(),
    ];
    assert_eq!(got[head.len() + 124..], tail);

    // Optimized, with reliable f16 and f128, for wasm without WASI: `mem` on, no unaligned access.
    let mut wasm = builtins_env("wasm32-unknown-unknown", "wasm32", "unknown", "3");
    wasm.push(("CARGO_CFG_TARGET_HAS_RELIABLE_F16", String::new()));
    wasm.push(("CARGO_CFG_TARGET_HAS_RELIABLE_F128", String::new()));
    let got = builtins(&wasm).unwrap();
    for line in ["cargo:rustc-cfg=optimizations_enabled", "cargo:rustc-cfg=f16_enabled", "cargo:rustc-cfg=f128_enabled", "cargo:rustc-cfg=feature=\"mem\""] {
        assert!(got.iter().any(|it| it == line), "{line} in {got:?}");
    }
    assert!(!got.iter().any(|it| it == "cargo:rustc-cfg=mem_unaligned"));

    let mut thumb = builtins_env("thumbv6m-none-eabi", "arm", "none", "0");
    thumb.push(("LIBM_BUILD_VERBOSE", "1".to_owned()));
    let error = builtins(&thumb).unwrap_err();
    assert!(error.contains("thumb mode"), "{error}");
    thumb.retain(|it| it.0 != "CARGO_CFG_TARGET_FEATURE");
    thumb.push(("CARGO_CFG_TARGET_FEATURE", "thumb-mode".to_owned()));
    let got = builtins(&thumb).unwrap();
    assert_eq!(got[1], "cargo:warning=feature `compiler-builtins` enabled");
    assert_eq!(got[2], "cargo:warning=feature `unmangled-names` enabled");
    let thumb1 = ["cargo:rustc-check-cfg=cfg(thumb1_only)", "cargo:warning=setting config `thumb1_only`", "cargo:rustc-cfg=thumb1_only"];
    assert!(got.windows(3).any(|it| it == thumb1), "{got:?}");

    let mut openbsd = builtins_env("x86_64-unknown-openbsd", "x86_64", "openbsd", "0");
    openbsd.push(("CARGO_FEATURE_C", String::new()));
    let got = builtins(&openbsd).unwrap();
    assert_eq!(got[got.len() - 2..], ["cargo:rustc-link-search=native=/usr/lib", "cargo:rustc-link-lib=compiler_rt"]);
    let mut c = builtins_env("x86_64-unknown-linux-gnu", "x86_64", "linux", "0");
    c.push(("CARGO_FEATURE_C", String::new()));
    let error = builtins(&c).unwrap_err();
    assert!(error.contains("`cc`"), "{error}");
}

/// Every target blaze knows, with the variables cargo gives a run for it: std's, object's,
/// compiler_builtins' and test's ports print without failing, and libc's but where its script
/// would start `emcc`.
#[test]
fn every_known_target_runs_each_port() {
    for triple in target::triples() {
        let env = cargo_env(triple);
        let pairs: Vec<(&str, &str)> = env.iter().map(|(name, value)| (name.as_str(), value.as_str())).collect();
        let vars = vars(&pairs);
        let cwd = Path::new(BUILTINS_DIR);
        for script in [Script::Std, Script::Object, Script::CompilerBuiltins, Script::Test] {
            if let Err(error) = printed(script, &vars, cwd) {
                panic!("{script:?} for {triple}: {error}");
            }
        }
        let os = pairs.iter().find(|it| it.0 == "CARGO_CFG_TARGET_OS").map_or("", |it| it.1);
        match (printed(Script::Libc, &vars, cwd), os) {
            (Err(error), "emscripten") => assert!(error.contains("emcc"), "{error}"),
            (Ok(_), "emscripten") => panic!("libc for {triple} started no emcc"),
            (Err(error), _) => panic!("libc for {triple}: {error}"),
            (Ok(_), _) => {}
        }
        let std = String::from_utf8(printed(Script::Std, &vars, cwd).unwrap()).unwrap();
        let arch = pairs.iter().find(|it| it.0 == "CARGO_CFG_TARGET_ARCH").map_or("", |it| it.1);
        assert!(std.contains(&format!("cargo:rustc-env=STD_ENV_ARCH={arch}\n")), "{triple}: {std}");
    }
}
