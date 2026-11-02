//! The typed tables of the configuration: rustflags, `[env]`, runners and linkers, `[source]`,
//! `[build]` and `[net]`, each as cargo 1.99 takes them (the rustflags order and `[env]`
//! values checked against `cargo build -v` and `cargo run`).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use blaze_manifest::Config;
use blaze_manifest::files::Disk;
use blaze_manifest::config::{CompileKind, Flags, Replacement};
use cargo_platform::Cfg;

struct Tree {
    root: PathBuf,
}

impl Tree {
    fn new(name: &str, files: &[(&str, &str)]) -> Tree {
        let root =
            std::env::temp_dir().join(format!("blaze-config-typed-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (path, contents) in files {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }
        std::fs::create_dir_all(root.join("home")).unwrap();
        // Not the `\\?\` form `canonicalize` gives on Windows, which a path read back from a
        // `file:` URL is not, as cargo's is not.
        Tree { root: dunce::canonicalize(root).unwrap() }
    }

    fn config(&self, env: &[(&str, &str)]) -> Config<Disk> {
        let mut vars =
            vec![("CARGO_HOME".to_owned(), self.root.join("home").display().to_string())];
        vars.extend(env.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())));
        Config::load(&self.root, vars, &[]).unwrap()
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn macos() -> Vec<Cfg> {
    ["unix", "target_os=\"macos\"", "target_arch=\"aarch64\""]
        .iter()
        .map(|c| c.parse().unwrap())
        .collect()
}

const HOST: &str = "aarch64-apple-darwin";

const RUSTFLAGS: &str = r#"
[build]
rustflags = ["--cfg", "from_build"]
[target.aarch64-apple-darwin]
rustflags = ["--cfg", "from_triple"]
[target.'cfg(unix)']
rustflags = ["--cfg", "from_unix"]
[target.'cfg(target_os = "macos")']
rustflags = ["--cfg", "from_macos"]
[target.'cfg(windows)']
rustflags = ["--cfg", "from_windows"]
"#;

fn flags(config: &Config<Disk>, requested: &[CompileKind], kind: &CompileKind) -> Vec<String> {
    config.extra_args(requested, HOST, Some(&macos()), kind, Flags::Rust).unwrap()
}

#[test]
fn rustflags_take_the_triple_and_matching_cfgs_by_key_over_build() {
    let tree = Tree::new("rustflags", &[(".cargo/config.toml", RUSTFLAGS)]);
    let host = [CompileKind::Host];
    let config = tree.config(&[]);
    assert_eq!(
        flags(&config, &host, &CompileKind::Host),
        ["--cfg", "from_triple", "--cfg", "from_macos", "--cfg", "from_unix"]
    );
    let config = tree.config(&[("RUSTFLAGS", "--cfg from_env  --cfg x")]);
    assert_eq!(flags(&config, &host, &CompileKind::Host), ["--cfg", "from_env", "--cfg", "x"]);
    let config = tree.config(&[("CARGO_ENCODED_RUSTFLAGS", "a b\x1fc"), ("RUSTFLAGS", "x")]);
    assert_eq!(flags(&config, &host, &CompileKind::Host), ["a b", "c"]);
    let config = tree.config(&[("CARGO_ENCODED_RUSTFLAGS", "")]);
    assert!(flags(&config, &host, &CompileKind::Host).is_empty());
}

#[test]
fn rustflags_skip_host_units_with_a_target_and_fall_back_to_build() {
    let tree = Tree::new("rustflags-target", &[(".cargo/config.toml", RUSTFLAGS)]);
    let config = tree.config(&[("RUSTFLAGS", "--cfg from_env")]);
    let wasm = CompileKind::Target("wasm32-unknown-unknown".to_owned());
    let requested = [wasm.clone()];
    assert!(flags(&config, &requested, &CompileKind::Host).is_empty());
    let config = tree.config(&[]);
    let none: Vec<Cfg> = Vec::new();
    let got = config.extra_args(&requested, HOST, Some(&none), &wasm, Flags::Rust).unwrap();
    assert_eq!(got, ["--cfg", "from_build"]);
    let got = config.extra_args(&requested, HOST, None, &wasm, Flags::Rustdoc).unwrap();
    assert!(got.is_empty());
}

#[test]
fn env_table_resolves_relative_values_and_yields_to_the_process() {
    let tree = Tree::new(
        "env",
        &[(
            ".cargo/config.toml",
            "[env]\nFOO = \"bar\"\nREL = { value = \"x/y\", relative = true }\n\
             FORCED = { value = \"f\", force = true }\nKEPT = \"k\"\n",
        )],
    );
    let config = tree.config(&[("FORCED", "proc"), ("KEPT", "proc")]);
    let env = config.env_config().unwrap();
    let got: Vec<(&str, &OsString)> = env.iter().map(|(k, v)| (k.as_str(), v)).collect();
    assert_eq!(
        got,
        [
            ("FOO", &OsString::from("bar")),
            ("FORCED", &OsString::from("f")),
            ("REL", &tree.root.join("x/y").into_os_string()),
        ]
    );
}

#[test]
fn env_table_refuses_cargo_home() {
    let tree = Tree::new("env-home", &[(".cargo/config.toml", "[env]\nCARGO_HOME = \"x\"\n")]);
    let err = tree.config(&[]).env_config().unwrap_err();
    assert_eq!(
        err.to_string(),
        "setting the `CARGO_HOME` environment variable is not supported in the `[env]` configuration table"
    );
}

#[test]
fn runner_and_linker_come_from_the_triple_then_one_matching_cfg() {
    let tree = Tree::new(
        "runner",
        &[(
            ".cargo/config.toml",
            "[target.aarch64-apple-darwin]\nrunner = \"tools/run --flag\"\n\
             [target.'cfg(unix)']\nlinker = \"cc\"\nrunner = \"unix-runner\"\n\
             [target.'cfg(windows)']\nrunner = \"win\"\n",
        )],
    );
    let config = tree.config(&[]);
    let (path, args) = config.target_runner(&CompileKind::Host, HOST, &macos()).unwrap().unwrap();
    assert_eq!(path, tree.root.join("tools/run"));
    assert_eq!(args, ["--flag"]);
    let wasm = CompileKind::Target("wasm32-unknown-unknown".to_owned());
    let (path, _) = config.target_runner(&wasm, HOST, &macos()).unwrap().unwrap();
    assert_eq!(path, Path::new("unix-runner"));
    let linker = config.target_linker(&wasm, HOST, &macos()).unwrap();
    assert_eq!(linker.as_deref(), Some(Path::new("cc")));
    assert_eq!(config.target_linker(&wasm, HOST, &[]).unwrap(), None);
}

#[test]
fn two_matching_cfg_runners_are_an_error() {
    let tree = Tree::new(
        "runners",
        &[(
            ".cargo/config.toml",
            "[target.'cfg(unix)']\nrunner = \"a\"\n[target.'cfg(target_os = \"macos\")']\nrunner = \"b\"\n",
        )],
    );
    let err = tree.config(&[]).target_runner(&CompileKind::Host, HOST, &macos()).unwrap_err();
    let def = tree.root.join(".cargo").join("config.toml").display().to_string();
    assert_eq!(
        err.to_string(),
        format!(
            "several matching instances of `target.'cfg(..)'.runner` in configurations\n\
             first match `cfg(target_os = \"macos\")` located in {def}\n\
             second match `cfg(unix)` located in {def}"
        )
    );
}

#[test]
fn target_cfg_tables_warn_of_keys_they_do_not_know() {
    let tree = Tree::new(
        "cfg-unused",
        &[(".cargo/config.toml", "[target.'cfg(unix)']\nrustflags = []\nlinkr = \"cc\"\n")],
    );
    let config = tree.config(&[]);
    assert_eq!(config.target_cfgs().unwrap().len(), 1);
    assert_eq!(config.take_warnings(), ["unused key `linkr` in [target] config table `cfg(unix)`"]);
}

#[test]
fn build_table_warns_of_keys_it_does_not_know() {
    let tree = Tree::new(
        "build-unused",
        &[(".cargo/config.toml", "[build]\njobs = 2\ntarget-dri = \"x\"\n")],
    );
    let config = tree.config(&[]);
    assert!(matches!(
        config.build_config().unwrap().jobs,
        Some(blaze_manifest::config::JobsConfig::Integer(2))
    ));
    let def = tree.root.join(".cargo").join("config.toml").display().to_string();
    assert_eq!(
        config.take_warnings(),
        [format!("unused config key `build.target-dri` in `{def}`")]
    );
}

#[test]
fn net_table_reads_files_and_variables() {
    let tree = Tree::new("net", &[(".cargo/config.toml", "[net]\nretry = 5\noffline = false\n")]);
    let net = tree.config(&[("CARGO_NET_OFFLINE", "true")]).net_config().unwrap();
    assert_eq!((net.retry, net.offline), (Some(5), Some(true)));
}

#[test]
fn profile_tables_read_as_manifest_profiles_with_their_variables() {
    let tree = Tree::new(
        "profile",
        &[(
            ".cargo/config.toml",
            "[profile.dev]\nopt-level = 2\ndebug = 'line-tables-only'\n\
             [profile.dev.package.\"*\"]\nopt-level = 3\n\
             [profile.custom]\ninherits = \"release\"\nlto = true\n",
        )],
    );
    let config = tree.config(&[
        ("CARGO_PROFILE_DEV_OPT_LEVEL", "s"),
        ("CARGO_PROFILE_DEV_INCREMENTAL", "false"),
    ]);
    let dev = config.config_profile("dev").unwrap().unwrap().val;
    let json = serde_json::to_value(&dev).unwrap();
    assert_eq!(json["opt-level"], "s");
    assert_eq!(json["debug"], "line-tables-only");
    assert_eq!(json["incremental"], false);
    assert_eq!(json["package"]["*"]["opt-level"], 3);
    let custom = config.config_profile("custom").unwrap().unwrap().val;
    assert_eq!(custom.inherits.as_deref(), Some("release"));
    assert!(config.config_profile("bench").unwrap().is_none());
}

#[test]
fn source_replacement_follows_replace_with() {
    let tree = Tree::new(
        "source",
        &[(
            ".cargo/config.toml",
            "[source.crates-io]\nreplace-with = \"mirror\"\n\
             [source.mirror]\nregistry = \"https://mirror.example\"\nreplace-with = \"vendored\"\n\
             [source.vendored]\ndirectory = \"vendor\"\n\
             [source.alt]\nregistry = \"https://alt.example\"\nreplace-with = \"my-registry\"\n\
             [registries.my-registry]\nindex = \"sparse+https://example.com/index/\"\n",
        )],
    );
    let config = tree.config(&[]);
    let sources = config.source_configs().unwrap();
    assert_eq!(sources["vendored"].id.url().to_file_path().unwrap(), tree.root.join("vendor"));
    assert_eq!(config.replacement("crates-io").unwrap(), Replacement::Source("vendored".into()));
    assert_eq!(config.replacement("vendored").unwrap(), Replacement::None);
    assert_eq!(config.replacement("alt").unwrap(), Replacement::Registry("my-registry".into()));
}

#[test]
fn source_tables_refuse_what_cargo_refuses() {
    fn def(tree: &Tree) -> String {
        tree.root.join(".cargo").join("config.toml").display().to_string()
    }
    let cases: &[(&str, &str, fn(&Tree) -> String)] = &[
        (
            "source-branch",
            "[source.a]\nregistry = \"https://example.com\"\nbranch = \"main\"\n",
            |t| {
                format!(
                    "source definition `source.a` specifies `branch`, but that requires a `git` key to be specified (in {})",
                    def(t)
                )
            },
        ),
        ("source-none", "[source.a]\nreplace-with = \"b\"\n", |_| {
            "no source location specified for `source.a`, need `registry`, `local-registry`, `directory`, or `git` defined".into()
        }),
        ("source-two", "[source.a]\ndirectory = \"v\"\ngit = \"https://example.com/r\"\n", |_| {
            "more than one source location specified for `source.a`".into()
        }),
        ("source-url", "[source.a]\nregistry = \"not a url\"\n", |t| {
            format!(
                "configuration key `source.a.registry` specified an invalid URL (in {})",
                def(t)
            )
        }),
    ];
    for (name, contents, want) in cases {
        let tree = Tree::new(name, &[(".cargo/config.toml", contents)]);
        let err = tree.config(&[]).source_configs().unwrap_err();
        assert_eq!(err.to_string(), want(&tree), "{name}");
    }
}

#[test]
fn source_replacement_cycles_and_missing_names_are_errors() {
    let tree = Tree::new(
        "source-cycle",
        &[(
            ".cargo/config.toml",
            "[source.a]\nregistry = \"https://a.example\"\nreplace-with = \"b\"\n\
             [source.b]\nregistry = \"https://b.example\"\nreplace-with = \"a\"\n\
             [source.c]\nregistry = \"https://c.example\"\nreplace-with = \"nowhere\"\n",
        )],
    );
    let config = tree.config(&[]);
    let def = tree.root.join(".cargo").join("config.toml").display().to_string();
    assert_eq!(
        config.replacement("a").unwrap_err().to_string(),
        format!(
            "detected a cycle of `replace-with` sources, the source `a` is eventually replaced with itself (configuration in `{def}`)"
        )
    );
    assert_eq!(
        config.replacement("c").unwrap_err().to_string(),
        format!(
            "could not find a configured source with the name `nowhere` when attempting to lookup `c` (configuration in `{def}`)"
        )
    );
}
