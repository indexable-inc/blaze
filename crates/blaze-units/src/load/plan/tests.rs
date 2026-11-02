use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use blaze_hash::{HashMap, HashSet};
use blaze_manifest::Config;
use blaze_manifest::files::Disk;
use blaze_manifest::manifest::{Context, EitherManifest, Manifest, read_manifest};
use blaze_manifest::profiles::{Profile as BaseProfile, ProfileRoot};
use blaze_manifest::targets::{CrateType, TargetKind};
use blaze_resolve::{InternedString, PackageId, SourceId};
use cargo_util_schemas::manifest::TomlManifest;

use super::hash::{self, StableHasher};
use super::outputs::{self, CompileFiles, Layout};
use super::{PlanInput, env, finished, flags, plan};
use crate::query::UnitPlan;
use crate::schema::{self, DebugInfo, Mode, Unit, UnitDep};
use crate::stdlib::target::{self as platform, CfgArgs};
use crate::{Intent, Package};

const HOST: &str = "x86_64-unknown-linux-gnu";
const TARGET: &str = "aarch64-unknown-linux-gnu";
const H: &str = "0123456789abcdef";

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|it| (*it).to_owned()).collect()
}

fn get<'a>(env: &'a [(String, String)], name: &str) -> Option<&'a str> {
    env.iter().find(|(it, _)| it == name).map(|(_, value)| value.as_str())
}

// `util::StableHasher` is rustc's `StableSipHasher128`, as `blaze_source::short_hash` uses it,
// whose value for crates.io is the `-25cdd57fae9f0462` of cargo's registry directories.
#[test]
fn the_hasher_is_cargos() {
    let mut hasher = StableHasher::new();
    SourceId::crates_io().hash(&mut hasher);
    let hex = hash::short_hex(Hasher::finish(&hasher));
    assert_eq!(hex, "25cdd57fae9f0462");
    assert_eq!(hex, blaze_source::short_hash(&SourceId::crates_io()));
}

// `target_short_hash`: `short_hash(&(METADATA_VERSION, pkg.package_id().stable_hash(ws_root)))`,
// a path package under the workspace by its relative path (`SourceId::stable_hash`).
#[test]
fn target_short_hash_by_hand() {
    let source = SourceId::for_path(Path::new("/w/pm")).unwrap();
    let id = PackageId::new(InternedString::new("pm"), semver::Version::new(0, 1, 0), source);
    let mut hasher = StableHasher::new();
    2u8.hash(&mut hasher);
    "pm".hash(&mut hasher);
    semver::Version::new(0, 1, 0).hash(&mut hasher);
    source.kind().hash(&mut hasher);
    "pm".hash(&mut hasher);
    assert_eq!(hash::target_short_hash(id, Path::new("/w")).unwrap(), hash::short_hex(Hasher::finish(&hasher)));
}

// `CompileKind::fingerprint_hash`: 0 for the host, else the target's name hashed.
#[test]
fn kind_hashes() {
    assert_eq!(hash::kind_fingerprint_hash(None).unwrap(), 0);
    let mut hasher = StableHasher::new();
    TARGET.hash(&mut hasher);
    assert_eq!(hash::kind_fingerprint_hash(Some(TARGET)).unwrap(), Hasher::finish(&hasher));
    assert!(hash::kind_fingerprint_hash(Some("my-target.json")).is_err());
}

/// What `Profile::comparable` hashes of the dev profile (`opt-level = 0`, `lto = false`,
/// `debug = 2`, debug assertions and overflow checks, unwind, no strip).
fn dev_profile_by_hand(into: &mut StableHasher, debuginfo: isize) {
    "0".hash(into); // opt_level
    1isize.hash(into); // Lto::Bool
    false.hash(into);
    Option::<&str>::None.hash(into); // codegen_backend
    Option::<u32>::None.hash(into); // codegen_units
    debuginfo.hash(into); // TomlDebugInfo
    Option::<&str>::None.hash(into); // split_debuginfo
    true.hash(into); // debug_assertions
    true.hash(into); // overflow_checks
    false.hash(into); // rpath
    false.hash(into); // incremental
    0isize.hash(into); // PanicStrategy::Unwind
    0isize.hash(into); // StripInner::None
    Vec::<String>::new().hash(into); // rustflags
    0isize.hash(into); // TomlTrimPaths::None
}

/// `compute_metadata` of serde's library, a registry package with no dependencies, built for
/// `target` (none: the host) with `lto::generate`'s `OnlyObject`.
fn serde_metadata_by_hand(target: Option<&str>) -> (String, String) {
    let mut shared = StableHasher::new();
    2u8.hash(&mut shared); // METADATA_VERSION
    "serde".hash(&mut shared);
    semver::Version::new(1, 0, 219).hash(&mut shared);
    SourceId::crates_io().kind().hash(&mut shared);
    "https://github.com/rust-lang/crates.io-index".hash(&mut shared);
    strings(&["default", "std"]).hash(&mut shared); // features
    dev_profile_by_hand(&mut shared, 4); // Full
    1isize.hash(&mut shared); // CompileMode::Build
    4isize.hash(&mut shared); // Lto::OnlyObject
    hash::kind_fingerprint_hash(target).unwrap().hash(&mut shared);
    "serde".hash(&mut shared); // target name
    0isize.hash(&mut shared); // TargetKind::Lib
    1usize.hash(&mut shared);
    1isize.hash(&mut shared); // CrateType::Lib
    // hash_rustc_version of a nightly: the channel, and the host for a host unit.
    1isize.hash(&mut shared);
    "nightly".hash(&mut shared);
    if target.is_none() {
        HOST.hash(&mut shared);
    }
    false.hash(&mut shared); // is_std

    let mut c_metadata = shared.clone();
    0usize.hash(&mut c_metadata); // no dependencies
    let mut unit_id = shared;
    0usize.hash(&mut unit_id); // no dependencies
    0usize.hash(&mut unit_id); // no `cargo rustc` arguments
    0usize.hash(&mut unit_id); // no rustflags
    (format!("{:016x}", Hasher::finish(&c_metadata)), format!("-{:016x}", Hasher::finish(&unit_id)))
}

#[test]
fn finished_texts() {
    assert_eq!(finished("dev", "0", true), "`dev` profile [unoptimized + debuginfo]");
    assert_eq!(finished("release", "3", false), "`release` profile [optimized]");
    assert_eq!(finished("bench", "3", true), "`bench` profile [optimized + debuginfo]");
}

#[test]
fn check_cfg_strings() {
    assert_eq!(flags::check_cfg_of([].into_iter()), ["cfg(docsrs,test)", "cfg(feature, values())"]);
    assert_eq!(
        flags::check_cfg_of(["default", "foo-bar"].into_iter()),
        ["cfg(docsrs,test)", "cfg(feature, values(\"default\", \"foo-bar\"))"]
    );
}

// `features_args` between the profile's and the kind's `--cfg`s.
#[test]
fn cfgs_in_cargos_order() {
    let cfgs = flags::cfgs(&strings(&["--cfg", "p"]), &strings(&["a"]), &strings(&["--cfg=k", "-Cx"]));
    assert_eq!(cfgs, ["p", "feature=\"a\"", "k"]);
}

// A profile's `-C panic` that says its `panic` goes, in each spelling rustc takes; the
// kind's rustflags follow, kept as they are.
#[test]
fn a_profiles_own_panic_flag_goes() {
    let profile = strings(&["-Cpanic=abort", "-C", "panic=abort", "--codegen", "panic=abort", "--codegen=panic=abort", "-C", "embed-bitcode=yes", "-Zunstable-options"]);
    let rest = strings(&["-Cpanic=abort", "--cfg", "k"]);
    let flags = flags::unit_rustflags(&profile, "abort", "panic_abort", &rest).unwrap();
    assert_eq!(flags, ["-C", "embed-bitcode=yes", "-Zunstable-options", "-Cpanic=abort", "--cfg", "k"]);
}

#[test]
fn a_profiles_other_panic_flag_is_refused_naming_its_package() {
    let error = flags::unit_rustflags(&strings(&["-Cpanic=abort"]), "unwind", "p", &[]).unwrap_err();
    assert!(error.what.contains("`p`'s profile rustflags say `-C panic=abort`"), "{}", error.what);
    assert!(error.what.contains("`panic = \"unwind\"`"), "{}", error.what);
}

// `lints_to_rustflags`: by priority, then by name backwards; `[lints.cargo]` is cargo's own.
#[test]
fn lints_order() {
    let toml: TomlManifest = toml::from_str(
        "[package]\nname = \"a\"\nversion = \"0.1.0\"\n\n\
         [lints.rust]\nunsafe_code = \"forbid\"\nunused = { level = \"allow\", priority = -1 }\n\
         unexpected_cfgs = { level = \"warn\", check-cfg = ['cfg(has_foo)'] }\n\n\
         [lints.clippy]\nall = { level = \"warn\", priority = -1 }\npedantic = \"deny\"\n\n\
         [lints.cargo]\nunused_optional_dependency = \"warn\"\n",
    )
    .unwrap();
    let (lints, check_cfg) = flags::lints_of("a", &toml).unwrap();
    assert_eq!(
        lints,
        [
            "--allow=unused",
            "--warn=clippy::all",
            "--forbid=unsafe_code",
            "--warn=unexpected_cfgs",
            "--deny=clippy::pedantic",
        ]
    );
    assert_eq!(check_cfg, ["cfg(has_foo)"]);
}

#[test]
fn the_workspace_sets_blaze_lints_in_its_metadata() {
    let manifest = Path::new("/w/Cargo.toml");
    let of = |text: &str| flags::workspace_lints(Some(&toml::Value::Table(toml::from_str(text).unwrap())), manifest);
    assert_eq!(
        of("[blaze.lints]\nhot_clone = \"deny\"\nhot_alloc = \"warn\"\n").unwrap(),
        ["--warn=blaze::hot_alloc", "--deny=blaze::hot_clone"]
    );
    assert_eq!(of("[blaze]\nexperimental = []\n").unwrap(), Vec::<String>::new());
    assert_eq!(flags::workspace_lints(None, manifest).unwrap(), Vec::<String>::new());
    let level = of("[blaze.lints]\nhot_alloc = \"loud\"\n").unwrap_err();
    assert_eq!(
        level.what,
        "[workspace.metadata.blaze.lints] in /w/Cargo.toml: the level of `hot_alloc` is `allow`, `warn`, `deny` or `forbid`"
    );
    let path = of("[blaze.lints]\n\"blaze::hot_alloc\" = \"warn\"\n").unwrap_err();
    assert!(path.what.ends_with("is named without its tool: these are blaze's own lints"), "{}", path.what);
    assert_eq!(
        of("[blaze.lints]\nhot = \"deny\"\nconst_impure = \"allow\"\n").unwrap(),
        ["--allow=blaze::const_impure", "--deny=blaze::hot"]
    );
    let unknown = of("[blaze.lints]\nhot_allocs = \"warn\"\n").unwrap_err();
    assert_eq!(
        unknown.what,
        "[workspace.metadata.blaze.lints] in /w/Cargo.toml: `hot_allocs` is no lint or group of blaze's (`blaze::hot_allocs`)"
    );
    assert_eq!(unknown.at.as_deref(), Some("/w/Cargo.toml"));
}

/// A package's `[lints.blaze]` names blaze's lints as the workspace's table does.
#[test]
fn a_packages_unknown_blaze_lint_is_an_error_naming_it() {
    let manifest = |lints: &str| -> TomlManifest {
        toml::from_str(&format!("[package]\nname = \"a\"\nversion = \"0.1.0\"\n\n[lints.blaze]\n{lints}")).unwrap()
    };
    let (lints, _) = flags::lints_of("a", &manifest("wall_clock = \"deny\"\n")).unwrap();
    assert_eq!(lints, ["--deny=blaze::wall_clock"]);
    let unknown = flags::lints_of("a", &manifest("wallclock = \"deny\"\n")).unwrap_err();
    assert_eq!(
        unknown.what,
        "the `[lints.blaze]` of `a` set `wallclock`, which is no lint or group of blaze's (`blaze::wallclock`)"
    );
}

/// cargo hands every `[lints]` table to rustc as flags, and rustc stops at a tool it does not
/// know (E0602, `unknown lint tool: blaze`), so a cargo build of this workspace breaks; blaze's
/// own lints are set in `[workspace.metadata.blaze.lints]` instead.
#[test]
fn no_manifest_of_this_workspace_names_a_lint_tool_rustc_does_not_know() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut manifests = vec![root.join("Cargo.toml")];
    for entry in std::fs::read_dir(root.join("crates")).unwrap() {
        let at = entry.unwrap().path().join("Cargo.toml");
        if at.is_file() {
            manifests.push(at);
        }
    }
    for at in manifests {
        let text = std::fs::read_to_string(&at).unwrap();
        let manifest: toml::Table = toml::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", at.display()));
        let workspace = manifest.get("workspace").and_then(|it| it.get("lints"));
        for lints in [manifest.get("lints"), workspace].into_iter().flatten() {
            let Some(tools) = lints.as_table() else { continue };
            for tool in tools.keys() {
                assert!(
                    ["rust", "clippy", "rustdoc", "cargo", "workspace"].contains(&tool.as_str()),
                    "{} names the lint tool `{tool}`, which a cargo build hands rustc",
                    at.display()
                );
            }
        }
    }
}

/// A configuration seen from `cwd` with `env` and `--config` arguments `cli`.
fn config(cwd: &Path, env: &[(&str, &str)], cli: &[&str]) -> Config<Disk> {
    let mut vars = vec![("CARGO_HOME".to_owned(), cwd.join("cargo-home").display().to_string())];
    vars.extend(env.iter().map(|(name, value)| ((*name).to_owned(), (*value).to_owned())));
    Config::load(cwd, vars, &strings(cli)).unwrap()
}

// `extra_args`: `CARGO_ENCODED_RUSTFLAGS` over `RUSTFLAGS` over the config; the host's units of
// a build with `--target` get none, `target-applies-to-host` and `[host]` being unstable.
#[test]
fn rustflags_precedence() {
    let dir = Scratch::new("rustflags");
    let cli = ["build.rustflags=['-Clink-arg=config']", "target-applies-to-host=true"];
    let rustflags = |env: &[(&str, &str)], target: Option<&str>, kind: Option<&str>| {
        flags::kind_info(&config(&dir.0, env, &cli), HOST, target, kind).unwrap().rustflags
    };
    assert_eq!(rustflags(&[], None, None), ["-Clink-arg=config"]);
    assert_eq!(
        rustflags(&[("RUSTFLAGS", "-Clink-arg=env  -Clink-arg=more")], None, None),
        ["-Clink-arg=env", "-Clink-arg=more"]
    );
    let both = [("RUSTFLAGS", "-Clink-arg=env"), ("CARGO_ENCODED_RUSTFLAGS", "-Clink-arg=a b\x1f-Clink-arg=c")];
    assert_eq!(rustflags(&both, None, None), ["-Clink-arg=a b", "-Clink-arg=c"]);
    assert_eq!(rustflags(&[], Some(TARGET), Some(TARGET)), ["-Clink-arg=config"]);
    assert!(rustflags(&[], Some(TARGET), None).is_empty());
    assert!(rustflags(&[("RUSTFLAGS", "-Clink-arg=env")], Some(TARGET), None).is_empty());
}

// A `[target.'cfg(..)']` rustflag that only matches with a cfg the rustflags give: cargo's
// second turn of `TargetInfo::new`.
#[test]
fn rustflags_of_target_cfg() {
    let dir = Scratch::new("target-cfg");
    let cli = ["build.rustflags=['--cfg=blaze']", "target.'cfg(blaze)'.rustflags=['-Clink-arg=matched']"];
    let info = flags::kind_info(&config(&dir.0, &[], &cli), HOST, None, None).unwrap();
    assert_eq!(info.rustflags, ["-Clink-arg=matched"]);
    assert!(!info.cfg_lines.iter().any(|it| it == "blaze"));
}

// `custom_build.rs`'s `build_work`: features as `CARGO_FEATURE_*`, the target's cfgs as
// `CARGO_CFG_*` with the features first under `feature`, `[env]` under what cargo sets.
#[test]
fn build_script_env() {
    let cfg_lines = platform::target_cfg(HOST, &CfgArgs { proc_macro: true, ..CfgArgs::default() }).unwrap();
    let package = vec![("CARGO_PKG_NAME".to_owned(), "app".to_owned())];
    let features = strings(&["default", "foo-bar"]);
    let rustflags = strings(&["-Ca", "-Cb"]);
    let config_env = vec![
        ("FROM_ENV".to_owned(), "1".to_owned()),
        ("RUSTFLAGS".to_owned(), "-Cgone".to_owned()),
        ("TARGET".to_owned(), "not-this".to_owned()),
    ];
    let env = env::run_env(
        env::RunEnv {
            package: &package,
            out_dir: "/w/target/debug/build/app/h/out",
            jobs: Some(4),
            target: HOST,
            host: HOST,
            debug: false,
            opt_level: "0",
            profile: "debug",
            linker: None,
            links: Some("z"),
            features: &features,
            debug_assertions: true,
            cfg_lines: &cfg_lines,
            rustflags: &rustflags,
        },
        &config_env,
    );
    assert!(env.windows(2).all(|it| it[0].0 < it[1].0), "{env:?}");
    for (name, value) in [
        ("CARGO_FEATURE_DEFAULT", "1"),
        ("CARGO_FEATURE_FOO_BAR", "1"),
        ("CARGO_CFG_TARGET_OS", "linux"),
        ("CARGO_CFG_TARGET_ARCH", "x86_64"),
        ("CARGO_CFG_UNIX", ""),
        ("CARGO_CFG_FEATURE", "default,foo-bar"),
        ("CARGO_CFG_DEBUG_ASSERTIONS", ""),
        ("CARGO_ENCODED_RUSTFLAGS", "-Ca\x1f-Cb"),
        ("CARGO_MANIFEST_LINKS", "z"),
        ("CARGO_PKG_NAME", "app"),
        ("DEBUG", "false"),
        ("FROM_ENV", "1"),
        ("HOST", HOST),
        ("NUM_JOBS", "4"),
        ("OPT_LEVEL", "0"),
        ("OUT_DIR", "/w/target/debug/build/app/h/out"),
        ("PROFILE", "debug"),
        ("TARGET", HOST),
    ] {
        assert_eq!(get(&env, name), Some(value), "{name}");
    }
    assert_eq!(get(&env, "RUSTFLAGS"), None);
    assert_eq!(get(&env, "RUSTC_LINKER"), None);
}

/// A compile unit's files in a build directory at `/w/build` and a target directory at
/// `/w/target`, its package directory `app/0123456789abcdef`.
fn files(triple: &str, mode: hash::CompileMode, kind: &TargetKind, name: &str, extra: Option<&str>, is_root: bool) -> Vec<String> {
    let target = (triple != HOST).then_some(triple);
    let layout = Layout::new(Path::new("/w/target"), Path::new("/w/build"), target, "debug");
    let host_layout = Layout::new(Path::new("/w/target"), Path::new("/w/build"), None, "debug");
    let crate_name = name.replace('-', "_");
    outputs::compile_outputs(&CompileFiles {
        mode,
        kind,
        name,
        crate_name: &crate_name,
        bin_name: None,
        package: "app",
        triple,
        extra,
        pkg_dir: &format!("app/{H}"),
        layout: &layout,
        host_layout: &host_layout,
        new_layout: true,
        artifact_dir: true,
        stop_rlib_uplift: true,
        is_root,
    })
    .unwrap()
}

#[test]
fn outputs_on_linux() {
    let out = format!("/w/build/debug/build/app/{H}/out");
    let lib = TargetKind::Lib(vec![CrateType::Lib]);
    assert_eq!(
        files(HOST, hash::CompileMode::Build, &lib, "app", Some(H), true),
        [format!("{out}/libapp-{H}.rlib"), format!("{out}/libapp-{H}.rmeta"), format!("{out}/app-{H}.d")]
    );
    assert_eq!(
        files(HOST, hash::CompileMode::Build, &TargetKind::Bin, "my-app", None, true),
        [
            format!("{out}/my_app"),
            "/w/target/debug/my-app".to_owned(),
            format!("{out}/my_app.dwp"),
            "/w/target/debug/my-app.dwp".to_owned(),
            format!("{out}/my_app.d"),
            "/w/target/debug/my-app.d".to_owned(),
        ]
    );
    assert_eq!(
        files(HOST, hash::CompileMode::Test, &TargetKind::Test, "it", Some(H), true),
        [format!("{out}/it-{H}"), format!("{out}/it-{H}.dwp"), format!("{out}/it-{H}.d")]
    );
    assert_eq!(
        files(HOST, hash::CompileMode::Build, &TargetKind::ExampleBin, "ex", None, true),
        [
            format!("{out}/ex"),
            "/w/target/debug/examples/ex".to_owned(),
            format!("{out}/ex.dwp"),
            "/w/target/debug/examples/ex.dwp".to_owned(),
            format!("{out}/ex.d"),
            "/w/target/debug/examples/ex.d".to_owned(),
        ]
    );
    assert_eq!(
        files(HOST, hash::CompileMode::Build, &TargetKind::CustomBuild, "build-script-build", None, false),
        [format!("{out}/build_script_build"), format!("{out}/build_script_build.dwp"), format!("{out}/build_script_build.d")]
    );
    assert_eq!(
        files(HOST, hash::CompileMode::Check { test: false }, &lib, "app", Some(H), true),
        [format!("{out}/libapp-{H}.rmeta"), format!("{out}/app-{H}.d")]
    );
    // A unit for `--target` under the target's directories.
    let target_out = format!("/w/build/{TARGET}/debug/build/app/{H}/out");
    assert_eq!(
        files(TARGET, hash::CompileMode::Build, &TargetKind::Bin, "app", None, true)[..2],
        [format!("{target_out}/app"), format!("/w/target/{TARGET}/debug/app")]
    );
    let (run_out, run) = outputs::run_files(&Layout::new(Path::new("/w/target"), Path::new("/w/build"), None, "debug"), true, "app/h");
    assert_eq!(run_out, "/w/build/debug/build/app/h/out");
    assert_eq!(
        run,
        ["/w/build/debug/build/app/h/run/stdout", "/w/build/debug/build/app/h/run/stderr", "/w/build/debug/build/app/h/run/root-output"]
    );
}

// The layout `__CARGO_TEMPORARY_BUILD_DIR_NEW_LAYOUT_OPT_OUT=1` keeps.
#[test]
fn outputs_in_the_old_layout() {
    let layout = Layout::new(Path::new("/w/target"), Path::new("/w/target"), None, "debug");
    let lib = TargetKind::Lib(vec![CrateType::Lib]);
    let it = CompileFiles {
        mode: hash::CompileMode::Build,
        kind: &lib,
        name: "app",
        crate_name: "app",
        bin_name: None,
        package: "app",
        triple: HOST,
        extra: Some(H),
        pkg_dir: &format!("app-{H}"),
        layout: &layout,
        host_layout: &layout,
        new_layout: false,
        artifact_dir: true,
        stop_rlib_uplift: true,
        is_root: true,
    };
    assert_eq!(
        outputs::compile_outputs(&it).unwrap(),
        [
            format!("/w/target/debug/deps/libapp-{H}.rlib"),
            format!("/w/target/debug/deps/libapp-{H}.rmeta"),
            format!("/w/target/debug/deps/app-{H}.d"),
        ]
    );
    let script = CompileFiles { kind: &TargetKind::CustomBuild, name: "build-script-build", crate_name: "build_script_build", extra: Some(H), ..it };
    assert_eq!(
        outputs::compile_outputs(&script).unwrap()[..2],
        [format!("/w/target/debug/build/app-{H}/build_script_build-{H}"), format!("/w/target/debug/build/app-{H}/build-script-build")]
    );
    let (run_out, run) = outputs::run_files(&layout, false, "app-h");
    assert_eq!(run_out, "/w/target/debug/build/app-h/out");
    assert_eq!(run[0], "/w/target/debug/build/app-h/output");
}

#[test]
fn uplifted_on_macos() {
    let mac = "aarch64-apple-darwin";
    let out = format!("/w/build/{mac}/debug/build/app/{H}/out");
    assert_eq!(
        files(mac, hash::CompileMode::Build, &TargetKind::Bin, "my-app", None, true),
        [
            format!("{out}/my_app"),
            format!("/w/target/{mac}/debug/my-app"),
            format!("{out}/my_app.dSYM"),
            format!("/w/target/{mac}/debug/my-app.dSYM"),
            format!("{out}/my_app.d"),
            format!("/w/target/{mac}/debug/my-app.d"),
        ]
    );
    // A dylib is uplifted though no root; no `.d` beside it then.
    let dylib = TargetKind::Lib(vec![CrateType::Dylib]);
    assert_eq!(
        files(mac, hash::CompileMode::Build, &dylib, "foo", None, false),
        [
            format!("{out}/libfoo.dylib"),
            format!("/w/target/{mac}/debug/libfoo.dylib"),
            format!("{out}/libfoo.dylib.dSYM"),
            format!("/w/target/{mac}/debug/libfoo.dylib.dSYM"),
            format!("{out}/foo.d"),
        ]
    );
}

#[test]
fn uplifted_on_windows() {
    let msvc = "x86_64-pc-windows-msvc";
    let out = format!("/w/build/{msvc}/debug/build/app/{H}/out");
    let dest = format!("/w/target/{msvc}/debug");
    // The `.pdb`'s copy has the crate's name, the `.exe`'s the bin's.
    assert_eq!(
        files(msvc, hash::CompileMode::Build, &TargetKind::Bin, "my-app", None, true),
        [
            format!("{out}/my_app.exe"),
            format!("{dest}/my-app.exe"),
            format!("{out}/my_app.pdb"),
            format!("{dest}/my_app.pdb"),
            format!("{out}/my_app.d"),
            format!("{dest}/my-app.d"),
        ]
    );
    let dylib = TargetKind::Lib(vec![CrateType::Dylib]);
    assert_eq!(
        files(msvc, hash::CompileMode::Build, &dylib, "foo", None, false),
        [
            format!("{out}/foo.dll"),
            format!("{dest}/foo.dll"),
            format!("{out}/foo.dll.lib"),
            format!("{dest}/foo.dll.lib"),
            format!("{out}/foo.dll.exp"),
            format!("{dest}/foo.dll.exp"),
            format!("{out}/foo.pdb"),
            format!("{dest}/foo.pdb"),
            format!("{out}/foo.d"),
        ]
    );
    let gnu = "x86_64-pc-windows-gnu";
    let gnu_out = format!("/w/build/{gnu}/debug/build/app/{H}/out");
    assert_eq!(
        files(gnu, hash::CompileMode::Build, &dylib, "foo", None, false)[..4],
        [
            format!("{gnu_out}/foo.dll"),
            format!("/w/target/{gnu}/debug/foo.dll"),
            format!("{gnu_out}/libfoo.dll.a"),
            format!("/w/target/{gnu}/debug/libfoo.dll.a"),
        ]
    );
}

/// A directory of the test's own under the system's temporary one, removed after.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("blaze-plan-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir.canonicalize().unwrap())
    }

    fn write(&self, path: &str, contents: &str) -> PathBuf {
        let path = self.0.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, contents).unwrap();
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn manifest(path: &Path, source: SourceId, config: Config<Disk>) -> Manifest {
    match read_manifest(path, source, &Context::new(config)).unwrap() {
        EitherManifest::Real(it) => *it,
        EitherManifest::Virtual(_) => panic!("`{}` has no package", path.display()),
    }
}

fn unit(pkg: &str, kind: &[&str], crate_types: &[&str], name: &str, platform: Option<&str>, mode: Mode, features: &[&str], deps: &[usize]) -> Unit {
    Unit {
        pkg_id: pkg.to_owned(),
        target: schema::Target {
            kind: strings(kind),
            crate_types: strings(crate_types),
            name: name.to_owned(),
            src_path: PathBuf::from(format!("/src/{name}.rs")),
            edition: "2021".to_owned(),
            required_features: None,
            doc: false,
            doctest: false,
            test: true,
        },
        // A build script is compiled with `build-override`'s `debug = 0`, and run with the
        // profile of the library it is for.
        profile: schema::Profile {
            debuginfo: DebugInfo::Level(if kind == ["custom-build"] && mode == Mode::Build { 0 } else { 2 }),
            ..schema::Profile::dev()
        },
        platform: platform.map(str::to_owned),
        mode,
        checked_as_test: false,
        features: strings(features),
        is_std: false,
        dependencies: deps
            .iter()
            .map(|&index| UnitDep { index, extern_crate_name: format!("dep{index}"), public: None, noprelude: None, nounused: None })
            .collect(),
    }
}

/// The units of `cargo build --all-targets`-like build of the workspace's one package `app`
/// (a library, a bin `app`, a test `it`, a build script, features `default` and `foo-bar`),
/// depending on serde from crates.io and the path proc macro `pm`, for `target` (none: the
/// host).
fn app_units(target: Option<&str>) -> Vec<Unit> {
    let features = ["default", "foo-bar"];
    vec![
        unit("serde", &["lib"], &["lib"], "serde", target, Mode::Build, &["default", "std"], &[]),
        unit("pm", &["proc-macro"], &["proc-macro"], "pm", None, Mode::Build, &[], &[]),
        unit("app", &["custom-build"], &["bin"], "build-script-build", None, Mode::Build, &features, &[]),
        unit("app", &["custom-build"], &["bin"], "build-script-build", target, Mode::RunCustomBuild, &features, &[2]),
        unit("app", &["lib"], &["lib"], "app", target, Mode::Build, &features, &[0, 1, 3]),
        unit("app", &["bin"], &["bin"], "app", target, Mode::Build, &features, &[4, 0, 3]),
        unit("app", &["test"], &["bin"], "it", target, Mode::Test, &features, &[4, 3]),
    ]
}

const APP: &str = "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\nbuild = \"build.rs\"\n\n\
                   [features]\ndefault = [\"foo-bar\"]\nfoo-bar = []\n\n\
                   [dependencies]\nserde = \"1\"\npm = { path = \"pm\" }\n\n\
                   [[test]]\nname = \"it\"\nharness = false\n\n\
                   [lints.rust]\nunsafe_code = \"forbid\"\nunexpected_cfgs = { level = \"warn\", check-cfg = ['cfg(has_foo)'] }\n";
const PM: &str = "[package]\nname = \"pm\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\nproc-macro = true\n";
const SERDE: &str = "[package]\nname = \"serde\"\nversion = \"1.0.219\"\nedition = \"2018\"\n\n[features]\ndefault = [\"std\"]\nstd = []\n";

/// Each unit's plan of [`app_units`] for `target`.
fn plan_app(target: Option<&str>) -> (PathBuf, Vec<UnitPlan>, String) {
    plan_app_as(target, false)
}

/// [`plan_app`], under `check`'s lints when `lints`, with a `clippy.toml` at the workspace's
/// root.
fn plan_app_as(target: Option<&str>, lints: bool) -> (PathBuf, Vec<UnitPlan>, String) {
    let named = if lints { "lints-" } else { "" };
    let dir = Scratch::new(&format!("app-{named}{}", target.unwrap_or("host")));
    let ws = dir.0.join("w");
    if lints {
        dir.write("w/clippy.toml", "msrv = \"1.70\"\n");
    }
    let app_path = dir.write("w/Cargo.toml", APP);
    for file in ["w/build.rs", "w/src/lib.rs", "w/src/main.rs", "w/tests/it.rs"] {
        dir.write(file, "");
    }
    let pm_path = dir.write("w/pm/Cargo.toml", PM);
    dir.write("w/pm/src/lib.rs", "");
    let serde_path = dir.write("registry/serde-1.0.219/Cargo.toml", SERDE);
    dir.write("registry/serde-1.0.219/src/lib.rs", "");

    let config = || {
        config(
            &ws,
            &[("CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER", "valgrind -q")],
            &["env.FROM_CONFIG='1'", "target.aarch64-unknown-linux-gnu.runner=['qemu-aarch64', '-L', '/sysroot']"],
        )
    };
    let crates_io = SourceId::crates_io();
    let app = manifest(&app_path, blaze_manifest::source::for_manifest_path(&app_path).unwrap(), config());
    let pm = manifest(&pm_path, blaze_manifest::source::for_manifest_path(&pm_path).unwrap(), config());
    let serde = manifest(&serde_path, crates_io, config());

    let path_id = |name: &str, at: &Path| {
        PackageId::new(InternedString::new(name), semver::Version::new(0, 1, 0), SourceId::for_path(at).unwrap())
    };
    let ids = [
        ("app", path_id("app", &ws), &app, app_path.as_path(), true),
        ("pm", path_id("pm", &ws.join("pm")), &pm, pm_path.as_path(), false),
        (
            "serde",
            PackageId::new(InternedString::new("serde"), semver::Version::new(1, 0, 219), SourceId::crates_io()),
            &serde,
            serde_path.as_path(),
            false,
        ),
    ];
    let packages: HashMap<String, (PackageId, Package<'_>)> = ids
        .iter()
        .map(|(key, id, manifest, _, is_member)| ((*key).to_owned(), (*id, Package { manifest: *manifest, is_member: *is_member })))
        .collect();
    let manifests: HashMap<PackageId, (&Path, &Manifest)> =
        ids.iter().map(|(_, id, manifest, path, _)| (*id, (*path, *manifest))).collect();

    let units = app_units(target);
    let std_plans = vec![None; units.len()];
    let config = config();
    // cargo's `default_dev`, as far as the `Finished` line reads it.
    let base_profile = BaseProfile {
        name: "dev".to_owned(),
        root: ProfileRoot::Debug,
        debuginfo: blaze_manifest::profiles::DebugInfo::Resolved(cargo_util_schemas::manifest::TomlDebugInfo::Full),
        ..BaseProfile::default()
    };
    let target_dir = ws.join("target");
    let unsandboxed: HashSet<&str> = ["app"].into_iter().collect();
    let ws_metadata = Some(toml::Value::Table(toml::from_str("[blaze.lints]\nhot_alloc = \"warn\"\n").unwrap()));
    let input = PlanInput {
        units: &units,
        std_plans: &std_plans,
        packages: &packages,
        manifests: &manifests,
        roots: &[4, 5, 6],
        config: &config,
        intent: if lints { Intent::Check { test: false } } else { Intent::Build },
        lints,
        build_std: false,
        ws_root: &ws,
        ws_manifest: &app_path,
        ws_metadata: ws_metadata.as_ref(),
        sysroot: None,
        target_dir: &target_dir,
        build_dir: &target_dir,
        host: HOST,
        target,
        requested_profile: "dev",
        profile_dir: "debug",
        base_profile: &base_profile,
        jobs: Some(4),
        unsandboxed: &unsandboxed,
    };
    let planned = plan(&input).unwrap();
    let plans = planned.plans.into_iter().map(|it| it.expect("every unit is planned here")).collect();
    (ws, plans, planned.finished)
}

#[test]
fn check_lints_the_workspaces_crates_with_their_configuration() {
    let (_, plans, _) = plan_app_as(None, true);
    let linted: Vec<bool> = plans.iter().map(|it| it.clippy.is_some()).collect();
    assert_eq!(linted, [false, false, true, false, true, true, true], "the member's crates and build script, not its run");
    let lib = &plans[4];
    assert!(lib.cfgs.contains(&"clippy".to_owned()));
    let (path, text) = lib.clippy.as_ref().and_then(|it| it.conf.clone()).expect("the workspace's clippy.toml");
    assert!(path.ends_with("/w/clippy.toml"), "{path}");
    assert_eq!(text, "msrv = \"1.70\"\n");
    assert!(!plans[0].cfgs.contains(&"clippy".to_owned()), "a dependency is checked, not linted");
    // The build script is compiled to run, as under `cargo build`: one executable, no `.rmeta`.
    let script = &plans[2];
    assert!(script.outputs[0].ends_with("/out/build_script_build"), "{:?}", script.outputs);
    assert!(script.outputs.iter().all(|it| !it.ends_with(".rmeta")), "{:?}", script.outputs);
}

/// The hex of a plan's `-C extra-filename`.
fn extra(plan: &UnitPlan) -> &str {
    plan.extra_filename.strip_prefix('-').unwrap()
}

#[test]
fn plans_of_a_workspace() {
    let (ws, plans, finished) = plan_app(None);
    let t = ws.join("target").display().to_string();
    let build = format!("{t}/debug/build");
    assert_eq!(finished, "`dev` profile [unoptimized + debuginfo]");

    let serde = &plans[0];
    assert_eq!((serde.metadata.clone(), serde.extra_filename.clone()), serde_metadata_by_hand(None));
    let hex = extra(serde);
    assert_eq!(
        serde.outputs,
        [
            format!("{build}/serde/{hex}/out/libserde-{hex}.rlib"),
            format!("{build}/serde/{hex}/out/libserde-{hex}.rmeta"),
            format!("{build}/serde/{hex}/out/serde-{hex}.d"),
        ]
    );
    assert!(serde.lints.is_empty(), "a registry package's lints are capped");
    assert_eq!(serde.check_cfg, ["cfg(docsrs,test)", "cfg(feature, values(\"default\", \"std\"))"]);
    assert_eq!(serde.cfgs, ["feature=\"default\"", "feature=\"std\""]);
    assert_eq!(get(&serde.env, "CARGO_PKG_NAME"), Some("serde"));
    assert_eq!(get(&serde.env, "CARGO_PRIMARY_PACKAGE"), None);
    assert_eq!(get(&serde.env, "FROM_CONFIG"), Some("1"));

    let pm = &plans[1];
    assert!(pm.lints.is_empty(), "the workspace's blaze lints are its members'");
    let hex = extra(pm);
    assert_eq!(
        pm.outputs,
        [
            format!("{build}/pm/{hex}/out/libpm-{hex}.so"),
            format!("{build}/pm/{hex}/out/libpm-{hex}.so.dwp"),
            format!("{build}/pm/{hex}/out/pm-{hex}.d"),
        ]
    );

    let script = &plans[2];
    assert_eq!(script.extra_filename, "");
    assert_eq!(script.metadata.len(), 16);
    let script_dir = script.outputs[0].strip_suffix("/out/build_script_build").unwrap();
    assert!(script_dir.starts_with(&format!("{build}/app/")), "{script_dir}");

    let run = &plans[3];
    let out_dir = run.out_dir.clone().unwrap();
    let run_dir = out_dir.strip_suffix("/out").unwrap();
    assert!(run_dir.starts_with(&format!("{build}/app/")) && run_dir != script_dir, "{run_dir}");
    assert_eq!(run.outputs, [format!("{run_dir}/run/stdout"), format!("{run_dir}/run/stderr"), format!("{run_dir}/run/root-output")]);
    for (name, value) in [
        ("CARGO_FEATURE_FOO_BAR", "1"),
        ("CARGO_CFG_TARGET_OS", "linux"),
        ("CARGO_MANIFEST_DIR", ws.display().to_string().as_str()),
        ("DEBUG", "true"),
        ("HOST", HOST),
        ("TARGET", HOST),
        ("NUM_JOBS", "4"),
        ("PROFILE", "debug"),
        ("OUT_DIR", out_dir.as_str()),
    ] {
        assert_eq!(get(&run.env, name), Some(value), "{name}");
    }
    assert!(run.rustflags.is_empty() && run.cfgs.is_empty());

    let lib = &plans[4];
    let hex = extra(lib);
    assert_eq!(lib.outputs[0], format!("{build}/app/{hex}/out/libapp-{hex}.rlib"));
    assert_eq!(get(&lib.env, "OUT_DIR"), Some(out_dir.as_str()));
    assert_eq!(get(&lib.env, "CARGO_PRIMARY_PACKAGE"), Some("1"));
    assert_eq!(get(&lib.env, "CARGO_CRATE_NAME"), Some("app"));
    assert_eq!(get(&lib.env, "CARGO_BIN_NAME"), None);
    assert_eq!(lib.lints, ["--warn=blaze::hot_alloc", "--forbid=unsafe_code", "--warn=unexpected_cfgs"]);
    assert_eq!(lib.check_cfg[2], "cfg(has_foo)");
    assert_eq!(lib.cfgs, ["feature=\"default\"", "feature=\"foo-bar\""]);
    assert_ne!(lib.metadata, plans[5].metadata);

    let bin = &plans[5];
    assert_eq!(bin.extra_filename, "");
    let bin_dir = bin.outputs[0].strip_suffix("/out/app").unwrap();
    assert!(bin_dir.starts_with(&format!("{build}/app/")));
    assert_eq!(bin.outputs[1], format!("{t}/debug/app"));
    assert_eq!(bin.outputs.last().unwrap(), &format!("{t}/debug/app.d"));
    assert_eq!(get(&bin.env, "CARGO_BIN_NAME"), Some("app"));

    let test = &plans[6];
    let hex = extra(test);
    assert_eq!(
        test.outputs,
        [
            format!("{build}/app/{hex}/out/it-{hex}"),
            format!("{build}/app/{hex}/out/it-{hex}.dwp"),
            format!("{build}/app/{hex}/out/it-{hex}.d"),
        ]
    );
    assert_eq!(get(&test.env, "CARGO_BIN_EXE_app"), Some(format!("{t}/debug/app").as_str()));
    assert_eq!(get(&test.env, "CARGO_TARGET_TMPDIR"), Some(format!("{t}/tmp").as_str()));

    // `CARGO_TARGET_<TRIPLE>_RUNNER` of the host runs what is run: the binary and the test, not
    // a library or a build script.
    let unsandboxed: Vec<bool> = plans.iter().map(|it| it.unsandboxed).collect();
    assert_eq!(unsandboxed, [false, false, true, true, true, true, true]);
    let runners: Vec<&[String]> = plans.iter().map(|it| it.runner.as_slice()).collect();
    let valgrind = strings(&["valgrind", "-q"]);
    let (none, valgrind): (&[String], &[String]) = (&[], valgrind.as_slice());
    assert_eq!(runners, [none, none, none, none, none, valgrind, valgrind]);
    let harness: Vec<bool> = plans.iter().map(|it| it.harness).collect();
    assert_eq!(harness, [true, true, true, true, true, true, false]);
}

#[test]
fn plans_for_a_target() {
    let (ws, plans, _) = plan_app(Some(TARGET));
    let t = ws.join("target").display().to_string();
    let serde = &plans[0];
    assert_eq!((serde.metadata.clone(), serde.extra_filename.clone()), serde_metadata_by_hand(Some(TARGET)));
    let hex = extra(serde);
    assert_eq!(serde.outputs[0], format!("{t}/{TARGET}/debug/build/serde/{hex}/out/libserde-{hex}.rlib"));
    // The proc macro and the build script are the host's.
    assert!(plans[1].outputs[0].starts_with(&format!("{t}/debug/build/pm/")));
    assert!(plans[2].outputs[0].starts_with(&format!("{t}/debug/build/app/")));
    let run = &plans[3];
    assert!(run.out_dir.as_deref().unwrap().starts_with(&format!("{t}/{TARGET}/debug/build/app/")));
    assert_eq!(get(&run.env, "TARGET"), Some(TARGET));
    assert_eq!(get(&run.env, "HOST"), Some(HOST));
    assert_eq!(get(&run.env, "CARGO_CFG_TARGET_ARCH"), Some("aarch64"));
    assert_eq!(plans[5].outputs[1], format!("{t}/{TARGET}/debug/app"));
    assert_eq!(get(&plans[6].env, "CARGO_TARGET_TMPDIR"), Some(format!("{t}/{TARGET}/tmp").as_str()));
    assert_eq!(get(&plans[6].env, "CARGO_BIN_EXE_app"), Some(format!("{t}/{TARGET}/debug/app").as_str()));
    // The target's runner is the config's `target.<triple>.runner`; the host's goes to no
    // target unit.
    let qemu = strings(&["qemu-aarch64", "-L", "/sysroot"]);
    assert_eq!(plans[5].runner, qemu);
    assert_eq!(plans[6].runner, qemu);
    assert!(plans[..5].iter().all(|it| it.runner.is_empty()));
}

// What can't be planned as cargo plans it is an error naming it, never a guess.
#[test]
fn refusals() {
    let mut unit = unit("app", &["lib"], &["lib"], "app", None, Mode::Doc, &[], &[]);
    assert!(hash::CompileMode::of(&unit).unwrap_err().what.contains("rustdoc"));
    unit.profile.trim_paths = Some(serde_json::json!("all"));
    assert_eq!(hash::HashedProfile::of(&unit.profile).unwrap().trim_paths, hash::TrimPaths::All);
    unit.profile.panic = "sideways".to_owned();
    assert!(hash::HashedProfile::of(&unit.profile).is_err());
    assert!(hash::Meta::read("0123", H, "", "std").is_err());
    assert!(hash::Meta::read(H, "", "", "std").is_err());
    let other = "fedcba9876543210";
    assert!(hash::Meta::read(H, H, &format!("-{other}"), "std").is_err());
    let meta = hash::Meta::read(H, H, &format!("-{H}"), "std").unwrap();
    assert_eq!((meta.metadata(), meta.extra_filename()), (H.to_owned(), format!("-{H}")));
    // A build script's unit has no extra filename; its unit id is still read.
    let script = hash::Meta::read(H, other, "", "std").unwrap();
    assert_eq!((script.unit_id(), script.extra_filename()), (other.to_owned(), String::new()));
}
