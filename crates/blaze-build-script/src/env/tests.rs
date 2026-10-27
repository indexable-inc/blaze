//! The cases of cargo's own tests of a script's environment (`tests/testsuite/build_script.rs`
//! and `build_script_env.rs` in rust-lang/cargo at 5f94df478, MIT or Apache-2.0), ported to
//! the description they are run from. Each names the test it comes from.

use super::*;
use crate::unit::{ConfigEnv, Dependency, DylibSearch, Package, Profile, Target, Tools};

fn bar() -> Unit {
    Unit {
        package: Package {
            name: "bar".into(),
            version: "0.5.0".into(),
            manifest_dir: "/p/foo/bar".into(),
            manifest_path: "/p/foo/bar/Cargo.toml".into(),
            authors: vec!["wycats@example.com".into()],
            targets: vec![
                Target {
                    name: "bar".into(),
                    kind: vec!["lib".into()],
                    crate_types: vec!["lib".into()],
                },
                Target {
                    name: "build-script-build".into(),
                    kind: vec!["custom-build".into()],
                    crate_types: vec!["bin".into()],
                },
            ],
            ..Package::default()
        },
        features: vec!["foo".into(), "other-feature".into()],
        profile: Profile {
            name: "dev".into(),
            release: false,
            opt_level: "0".into(),
            debuginfo: true,
            debug_assertions: true,
        },
        target: "x86_64-unknown-linux-gnu".into(),
        host: "x86_64-unknown-linux-gnu".into(),
        target_cfgs: [
            "debug_assertions",
            "panic=\"unwind\"",
            "target_arch=\"x86_64\"",
            "target_family=\"unix\"",
            "target_feature=\"fxsr\"",
            "target_feature=\"sse\"",
            "target_feature=\"sse2\"",
            "target_has_atomic=\"16\"",
            "target_has_atomic=\"32\"",
            "target_os=\"linux\"",
            "unix",
        ]
        .map(String::from)
        .to_vec(),
        jobs: 8,
        script: "/p/foo/target/debug/build/bar-1/build-script-build".into(),
        out_dir: "/p/foo/target/debug/build/bar-2/out".into(),
        tools: Tools {
            cargo: "/t/bin/cargo".into(),
            rustc: "rustc".into(),
            rustdoc: "rustdoc".into(),
            ..Tools::default()
        },
        dylib_search: DylibSearch {
            host_root: Some("/p/foo/target/debug".into()),
            host_deps: vec!["/p/foo/target/debug/deps".into()],
            sysroot_libdir: "/t/lib/rustlib/x86_64-unknown-linux-gnu/lib".into(),
            plugin_search: Vec::new(),
        },
        ..Unit::default()
    }
}

fn os(vars: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
    vars.iter().map(|(name, value)| (name.into(), value.into())).collect()
}

fn var<'a>(env: &'a Env, name: &str) -> Option<&'a str> {
    env.get(name).map(|it| it.to_str().unwrap())
}

/// `custom_build_env_vars`.
#[test]
fn what_every_script_is_given() {
    let env = env(&bar(), &os(&[("PATH", "/bin")])).unwrap();
    assert_eq!(var(&env, "TARGET"), Some("x86_64-unknown-linux-gnu"));
    assert_eq!(var(&env, "HOST"), Some("x86_64-unknown-linux-gnu"));
    assert_eq!(var(&env, "NUM_JOBS"), Some("8"));
    assert_eq!(var(&env, "CARGO_MANIFEST_DIR"), Some("/p/foo/bar"));
    assert_eq!(var(&env, "CARGO_MANIFEST_PATH"), Some("/p/foo/bar/Cargo.toml"));
    assert_eq!(var(&env, "OPT_LEVEL"), Some("0"));
    assert_eq!(var(&env, "PROFILE"), Some("debug"));
    assert_eq!(var(&env, "DEBUG"), Some("true"));
    assert_eq!(var(&env, "OUT_DIR"), Some("/p/foo/target/debug/build/bar-2/out"));
    assert_eq!(var(&env, "CARGO_FEATURE_FOO"), Some("1"));
    assert_eq!(var(&env, "CARGO_FEATURE_OTHER_FEATURE"), Some("1"));
    assert_eq!(var(&env, "CARGO_CFG_FEATURE"), Some("foo,other-feature"));
    assert_eq!(var(&env, "CARGO"), Some("/t/bin/cargo"));
    assert_eq!(var(&env, "RUSTC"), Some("rustc"));
    assert_eq!(var(&env, "RUSTDOC"), Some("rustdoc"));
    assert_eq!(var(&env, "RUSTC_WRAPPER"), None);
    assert_eq!(var(&env, "RUSTC_WORKSPACE_WRAPPER"), None);
    assert_eq!(var(&env, "RUSTC_LINKER"), None);
    assert_eq!(var(&env, "RUSTFLAGS"), None);
    assert_eq!(var(&env, "CARGO_ENCODED_RUSTFLAGS"), Some(""));
    assert_eq!(var(&env, "PATH"), Some("/bin"));
    assert_eq!(var(&env, "CARGO_PKG_NAME"), Some("bar"));
    assert_eq!(var(&env, "CARGO_PKG_VERSION"), Some("0.5.0"));
    assert_eq!(var(&env, "CARGO_PKG_VERSION_PRE"), Some(""));
    assert_eq!(var(&env, "CARGO_PKG_AUTHORS"), Some("wycats@example.com"));
    assert_eq!(var(&env, "CARGO_PKG_DESCRIPTION"), Some(""));
    assert_eq!(var(&env, "CARGO_PKG_RUST_VERSION"), Some(""));
    // What cargo sets for rustc alone.
    for name in
        ["CARGO_CRATE_NAME", "CARGO_BIN_NAME", "CARGO_PRIMARY_PACKAGE", "CARGO_TARGET_TMPDIR"]
    {
        assert_eq!(var(&env, name), None, "{name}");
    }
}

/// `custom_build_env_var_rustc_wrapper` and `custom_build_env_var_rustc_workspace_wrapper`:
/// the workspace's wrapper is for its members alone, and one in cargo's environment that
/// cargo does not use is taken away.
#[test]
fn wrappers() {
    let inherited = os(&[("RUSTC_WORKSPACE_WRAPPER", "/w"), ("RUSTC_WRAPPER", "/old")]);
    let env = env(&bar(), &inherited).unwrap();
    assert_eq!(var(&env, "RUSTC_WRAPPER"), None);
    assert_eq!(var(&env, "RUSTC_WORKSPACE_WRAPPER"), None);
    let mut unit = bar();
    unit.tools.rustc_wrapper = Some("/wrap".into());
    unit.tools.rustc_workspace_wrapper = Some("/w".into());
    let env = super::env(&unit, &inherited).unwrap();
    assert_eq!(var(&env, "RUSTC_WRAPPER"), Some("/wrap"));
    assert_eq!(var(&env, "RUSTC_WORKSPACE_WRAPPER"), None, "not a member");
    unit.workspace_member = true;
    let env = super::env(&unit, &inherited).unwrap();
    assert_eq!(var(&env, "RUSTC_WORKSPACE_WRAPPER"), Some("/w"));
}

/// `custom_build_env_var_rustc_linker` and `custom_build_env_var_encoded_rustflags`.
#[test]
fn linker_and_flags() {
    let mut unit = bar();
    unit.tools.linker = Some("/usr/bin/my-linker".into());
    unit.rustflags = vec!["-Clink-arg=-B nope".into(), "--cfg=foo".into()];
    let env = env(&unit, &os(&[("RUSTFLAGS", "-Cfoo")])).unwrap();
    assert_eq!(var(&env, "RUSTC_LINKER"), Some("/usr/bin/my-linker"));
    assert_eq!(var(&env, "CARGO_ENCODED_RUSTFLAGS"), Some("-Clink-arg=-B nope\x1f--cfg=foo"));
    assert_eq!(var(&env, "RUSTFLAGS"), None);
}

/// `links_passes_env_vars`, `non_links_can_pass_env_vars` and `links_with_dots`: metadata
/// reaches a script only from the packages with `links` it depends on.
#[test]
fn metadata_of_dependencies() {
    let mut unit = bar();
    unit.package.links = Some("a.b-c".into());
    unit.dependencies = vec![
        Dependency {
            name: "a".into(),
            links: Some("foo".into()),
            metadata: vec![("foo".into(), "bar".into()), ("bar-baz".into(), "baz".into())],
        },
        Dependency { name: "b".into(), links: None, metadata: vec![("x".into(), "y".into())] },
    ];
    let env = env(&unit, &[]).unwrap();
    assert_eq!(var(&env, "CARGO_MANIFEST_LINKS"), Some("a.b-c"));
    assert_eq!(var(&env, "DEP_FOO_FOO"), Some("bar"));
    assert_eq!(var(&env, "DEP_FOO_BAR_BAZ"), Some("baz"));
    assert!(!env.vars.keys().any(|it| it.to_str().unwrap().contains("_X")));
    assert_eq!(envify("a.b-c"), "A.B_C");
}

/// `profile_and_opt_level_set_correctly` and the `build_script_debug_assertions_*` tests:
/// `debug_assertions` is the profile's, whatever rustc said.
#[test]
fn profiles() {
    let mut unit = bar();
    unit.profile = Profile {
        name: "bench".into(),
        release: true,
        opt_level: "3".into(),
        debuginfo: false,
        debug_assertions: false,
    };
    let env = env(&unit, &[]).unwrap();
    assert_eq!(var(&env, "OPT_LEVEL"), Some("3"));
    assert_eq!(var(&env, "PROFILE"), Some("release"));
    assert_eq!(var(&env, "DEBUG"), Some("false"));
    assert_eq!(var(&env, "CARGO_CFG_DEBUG_ASSERTIONS"), None);
    unit.profile.debug_assertions = true;
    unit.target_cfgs.retain(|it| it != "debug_assertions");
    let env = super::env(&unit, &[]).unwrap();
    assert_eq!(var(&env, "CARGO_CFG_DEBUG_ASSERTIONS"), Some(""));
}

/// `cfg_env_vars_available`, `build_script_sees_cfg_target_feature` and
/// `rustc_cfg_with_and_without_value`.
#[test]
fn cfgs() {
    let mut unit = bar();
    unit.features.clear();
    unit.target_cfgs.push("my_cfg".into());
    unit.target_cfgs.push("my_cfg=\"a\"".into());
    unit.target_cfgs.push("other=\"b\"".into());
    unit.target_cfgs.push("other".into());
    let env = env(&unit, &[]).unwrap();
    assert_eq!(var(&env, "CARGO_CFG_TARGET_FAMILY"), Some("unix"));
    assert_eq!(var(&env, "CARGO_CFG_UNIX"), Some(""));
    assert_eq!(var(&env, "CARGO_CFG_TARGET_FEATURE"), Some("fxsr,sse,sse2"));
    assert_eq!(var(&env, "CARGO_CFG_TARGET_HAS_ATOMIC"), Some("16,32"));
    assert_eq!(var(&env, "CARGO_CFG_PANIC"), Some("unwind"));
    assert_eq!(var(&env, "CARGO_CFG_FEATURE"), Some(""), "set even with no feature");
    // A name and then a value adds the value; a value and then the name empties it.
    assert_eq!(var(&env, "CARGO_CFG_MY_CFG"), Some("a"));
    assert_eq!(var(&env, "CARGO_CFG_OTHER"), Some(""));
}

/// `[env]` (`env_config` and `apply_env_config`): it does not change cargo's environment
/// unless forced, never what cargo itself sets for any process, and what cargo sets for a
/// script alone (`TARGET`) wins over it.
#[test]
fn config_env() {
    let mut unit = bar();
    let set = |name: &str, value: &str, force| ConfigEnv {
        name: name.into(),
        value: value.into(),
        force,
    };
    unit.config_env = vec![
        set("FOO", "config", false),
        set("HOME", "/config-home", false),
        set("USER", "forced", true),
        set("CARGO_PKG_NAME", "nope", true),
        set("TARGET", "nope", true),
        set("RUSTFLAGS", "nope", true),
    ];
    let env = env(&unit, &os(&[("HOME", "/home/me"), ("USER", "me")])).unwrap();
    assert_eq!(var(&env, "FOO"), Some("config"));
    assert_eq!(var(&env, "HOME"), Some("/home/me"));
    assert_eq!(var(&env, "USER"), Some("forced"));
    assert_eq!(var(&env, "CARGO_PKG_NAME"), Some("bar"));
    assert_eq!(var(&env, "TARGET"), Some("x86_64-unknown-linux-gnu"));
    assert_eq!(var(&env, "RUSTFLAGS"), None);
}

/// `build_script_with_dynamic_native_dependency` and
/// `build_script_dylib_search_path_excludes_target_dylibs`: the host's directories, then
/// cargo's environment's, then what host-only dependencies' scripts added below the target
/// directory.
#[test]
fn dynamic_libraries_on_linux() {
    let mut unit = bar();
    unit.dylib_search.plugin_search =
        vec!["native=/p/foo/target/debug/build/dep-1/out".into(), "/usr/lib".into()];
    let env = env(&unit, &os(&[("LD_LIBRARY_PATH", "/opt/lib")])).unwrap();
    assert_eq!(
        var(&env, "LD_LIBRARY_PATH"),
        Some(
            "/p/foo/target/debug:/p/foo/target/debug/deps:/t/lib/rustlib/x86_64-unknown-linux-gnu/lib\
             :/opt/lib:/p/foo/target/debug/build/dep-1/out"
        )
    );
    // A cargo run by a cargo has the outer one's directories in front already.
    let outer = "/p/foo/target/debug:/p/foo/target/debug/deps:/t/lib/rustlib/x86_64-unknown-linux-gnu/lib:/x";
    let env = super::env(&bar(), &os(&[("LD_LIBRARY_PATH", outer)])).unwrap();
    assert_eq!(var(&env, "LD_LIBRARY_PATH"), Some(outer));
    // `cargo check` writes nothing to `target/debug`, which is then neither searched nor what
    // the host-only dependencies' directories are kept below (found by harness/scripts.py).
    unit.dylib_search.host_root = None;
    let env = super::env(&unit, &os(&[("LD_LIBRARY_PATH", "/opt/lib")])).unwrap();
    assert_eq!(
        var(&env, "LD_LIBRARY_PATH"),
        Some("/p/foo/target/debug/deps:/t/lib/rustlib/x86_64-unknown-linux-gnu/lib:/opt/lib")
    );
}

/// On macOS dyld's defaults follow, since setting the variable takes them away.
// `$HOME/lib` is joined as cargo joins it, with the separator of the machine it runs on, which
// is the host: a macOS host is never a Windows machine's.
#[cfg(unix)]
#[test]
fn dynamic_libraries_on_macos() {
    let mut unit = bar();
    unit.host = "aarch64-apple-darwin".into();
    let env = env(&unit, &os(&[("HOME", "/Users/me")])).unwrap();
    assert_eq!(
        var(&env, "DYLD_FALLBACK_LIBRARY_PATH"),
        Some(
            "/p/foo/target/debug:/p/foo/target/debug/deps:/t/lib/rustlib/x86_64-unknown-linux-gnu/lib\
             :/Users/me/lib:/usr/local/lib:/usr/lib"
        )
    );
    assert_eq!(var(&env, "LD_LIBRARY_PATH"), None);
    let env = super::env(&unit, &os(&[("DYLD_FALLBACK_LIBRARY_PATH", "/x")])).unwrap();
    assert!(var(&env, "DYLD_FALLBACK_LIBRARY_PATH").unwrap().ends_with("/lib:/x"));
}

#[test]
fn dynamic_libraries_on_windows() {
    let mut unit = bar();
    unit.host = "x86_64-pc-windows-msvc".into();
    unit.dylib_search.host_root = Some(r"C:\p\target\debug".into());
    unit.dylib_search.host_deps = vec![r"C:\p\target\debug\deps".into()];
    unit.dylib_search.sysroot_libdir = r"C:\t;x\lib".into();
    let env = env(&unit, &os(&[("PATH", r#"C:\bin;"C:\a;b""#)])).unwrap();
    assert_eq!(
        var(&env, "PATH"),
        Some(r#"C:\p\target\debug;C:\p\target\debug\deps;"C:\t;x\lib";C:\bin;"C:\a;b""#)
    );
}

/// A path cargo cannot join fails as cargo's run does.
#[test]
fn a_path_with_the_separator() {
    let mut unit = bar();
    unit.dylib_search.host_root = Some("/p/a:b/target/debug".into());
    let error = env(&unit, &[]).unwrap_err();
    assert!(
        error.0.starts_with("failed to join paths from `$LD_LIBRARY_PATH` together"),
        "{error}"
    );
}

#[test]
fn jobserver_and_versions() {
    let mut unit = bar();
    unit.jobserver = Some("3,4".into());
    unit.package.version = "1.22.333-beta.1+build.5".into();
    unit.package.authors = vec!["a".into(), "b <b@x>".into()];
    let env = env(&unit, &[]).unwrap();
    assert_eq!(var(&env, "CARGO_MAKEFLAGS"), Some("-j --jobserver-fds=3,4 --jobserver-auth=3,4"));
    assert_eq!(var(&env, "CARGO_PKG_VERSION"), Some("1.22.333-beta.1+build.5"));
    assert_eq!(var(&env, "CARGO_PKG_VERSION_MAJOR"), Some("1"));
    assert_eq!(var(&env, "CARGO_PKG_VERSION_MINOR"), Some("22"));
    assert_eq!(var(&env, "CARGO_PKG_VERSION_PATCH"), Some("333"));
    assert_eq!(var(&env, "CARGO_PKG_VERSION_PRE"), Some("beta.1"));
    assert_eq!(var(&env, "CARGO_PKG_AUTHORS"), Some("a:b <b@x>"));
}

/// cargo on Linux gives its scripts the certificates libgit2 found for it (found by
/// harness/scripts.py on dev-aws-8, where cargo's scripts had `SSL_CERT_FILE` and
/// `SSL_CERT_DIR` and blaze's did not).
#[test]
fn certificates_libgit2_finds() {
    let dir = std::env::temp_dir().join(format!("blaze-build-script-certs-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (empty, pki, ssl) = (dir.join("empty"), dir.join("pki"), dir.join("ssl"));
    std::fs::create_dir_all(&empty).unwrap();
    std::fs::create_dir_all(pki.join("certs")).unwrap();
    std::fs::write(pki.join("certs/ca-bundle.crt"), "").unwrap();
    std::fs::create_dir_all(ssl.join("certs")).unwrap();
    std::fs::write(ssl.join("cert.pem"), "").unwrap();
    let dirs = [dir.join("missing"), empty, pki.clone(), ssl.clone()];
    let dirs: Vec<&Path> = dirs.iter().map(PathBuf::as_path).collect();
    let linux = "x86_64-unknown-linux-gnu";
    let started = os(&[("PATH", "/bin")]);

    let env = cargo_started_in(&started, linux, &dirs);
    let mut expected = started.clone();
    expected.push(("SSL_CERT_FILE".into(), pki.join("certs/ca-bundle.crt").into()));
    expected.push(("SSL_CERT_DIR".into(), pki.join("certs").into()));
    assert_eq!(env, expected);

    // What names a file and a directory that exist stays; what names none is replaced.
    let file = ssl.join("cert.pem");
    let started = os(&[("SSL_CERT_FILE", file.to_str().unwrap()), ("SSL_CERT_DIR", "/nowhere")]);
    let env = cargo_started_in(&started, linux, &dirs);
    assert_eq!(
        env,
        [("SSL_CERT_FILE".into(), file.into()), ("SSL_CERT_DIR".into(), pki.join("certs").into())]
    );

    for host in ["aarch64-apple-darwin", "x86_64-pc-windows-msvc"] {
        assert_eq!(
            cargo_started_in(&os(&[("PATH", "/bin")]), host, &dirs),
            os(&[("PATH", "/bin")])
        );
    }
}
