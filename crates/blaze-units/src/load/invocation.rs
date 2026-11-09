//! The load's first query, the only one that reads the view's configuration
//! (docs/v2/plan.md section 1): the cargo command line parsed as cargo parses
//! it, and of the environment only the variables cargo reads by name, so a change to any other
//! variable reruns this query alone.

use blaze_format::SchemaHash;
use blaze_ids::ContentId;
use blaze_query::{Cancelled, Cx, Query, QueryKind};
use serde::{Deserialize, Serialize};

use super::files::{MountTable, ancestors};
use super::key_id;
use crate::query::{BuildConfig, Cbor, Command, LoadError, Versioned, producer};

/// The command line as cargo reads it, and what of the environment cargo reads.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug, Default)]
pub(crate) struct Asked {
    pub command: Option<Command>,
    pub root: String,
    pub cwd: String,
    /// Sorted by name: only what cargo reads by name.
    pub env: Vec<(String, String)>,
    pub host: String,
    pub library: String,
    /// [`crate::query::BuildConfig::std`].
    pub std: String,
    /// [`crate::query::BuildConfig::builds`].
    pub builds: String,
    /// [`crate::query::BuildConfig::sources`].
    pub sources: String,
    pub packages: Vec<String>,
    pub workspace: bool,
    pub exclude: Vec<String>,
    pub lib: bool,
    pub bins: Vec<String>,
    pub all_bins: bool,
    pub examples: Vec<String>,
    pub all_examples: bool,
    pub tests: Vec<String>,
    pub all_tests: bool,
    pub benches: Vec<String>,
    pub all_benches: bool,
    pub all_targets: bool,
    /// `cargo test --doc`.
    pub doc: bool,
    pub features: Vec<String>,
    pub all_features: bool,
    pub no_default_features: bool,
    pub release: bool,
    pub profile: Option<String>,
    pub targets: Vec<String>,
    pub target_dir: Option<String>,
    pub manifest_path: Option<String>,
    pub locked: bool,
    pub offline: bool,
    pub frozen: bool,
    /// `--no-locked`: this build may change the lock files whatever `CI` or the workspace says
    /// ([`crate::lock::policy`]).
    pub no_locked: bool,
    /// The environment says this runs on CI ([`crate::lock::ci_says`]): `--locked` is the
    /// default. Read here, not kept in [`Asked::env`]: `CI` is no variable cargo's load reads.
    pub ci: bool,
    /// `BLAZE_LOCK=write` ([`crate::lock::LockAsk::env_writes`]), read here like `CI`.
    pub lock_env_writes: bool,
    /// `BLAZE_IGNORE_PIN_FOR`: the root manifest a blaze above this one was told
    /// `--ignore-pin` for, where this blaze is not the one the workspace pins.
    pub ignore_pin_for: Option<String>,
    /// [`crate::query::BuildConfig::blaze`]: the version of the blaze that serves the build.
    pub blaze: String,
    /// `--config`, in order.
    pub config: Vec<String>,
    /// `-Z`, in order.
    pub unstable: Vec<String>,
    pub jobs: Option<u32>,
    pub ignore_rust_version: bool,
    /// `cargo doc --open`.
    pub open: bool,
}

impl Asked {
    pub fn command(&self) -> Command {
        self.command.unwrap_or(Command::Build)
    }

    /// The command as cargo names it, for `alias.<command>`.
    pub fn command_name(&self) -> &'static str {
        self.command().name()
    }

    pub fn env(&self, name: &str) -> Option<&str> {
        self.env.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str())
    }

    /// `--offline`, `--frozen` or `net.offline`'s flag, as cargo's messages name it.
    pub fn offline_flag(&self, net_offline: bool) -> Option<&'static str> {
        match () {
            _ if self.frozen => Some("--frozen"),
            _ if self.offline || net_offline => Some("--offline"),
            _ => None,
        }
    }

    /// `--locked`, `--frozen`, as cargo's lock file error names the flag.
    pub fn locked_flag(&self) -> Option<&'static str> {
        match () {
            _ if self.frozen => Some("--frozen"),
            _ if self.locked => Some("--locked"),
            _ => None,
        }
    }

    /// What the command line and the environment say of the lock files.
    pub fn lock_ask(&self) -> crate::lock::LockAsk {
        crate::lock::LockAsk { flag: self.locked_flag(), no_locked: self.no_locked, ci: self.ci, env_writes: self.lock_env_writes }
    }
}

/// The view's configuration as the load reads it.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub(crate) struct Configured {
    pub asked: Result<Asked, LoadError>,
    pub mounts: MountTable,
}

impl Versioned for Configured {
    const SCHEMA: SchemaHash = blaze_format::schema("blaze v2 cargo configured 8");
}

impl Versioned for Asked {
    const SCHEMA: SchemaHash = blaze_format::schema("blaze v2 cargo asked 6");
}

/// Reads `Cx::config`: the [`BuildConfig`] narrowed.
pub(crate) struct Invocation;

impl Query for Invocation {
    const KIND: QueryKind = QueryKind(0x6010);
    const NAME: &'static str = "cargo invocation";
    const PRODUCER: ContentId = producer("blaze v2 cargo invocation 6");
    type Key = ();
    type Value = Cbor<Configured>;

    fn input(key: &()) -> ContentId {
        key_id("blaze v2 cargo invocation key 1", key)
    }

    fn execute(cx: &mut Cx<'_>, _: &()) -> Result<Cbor<Configured>, Cancelled> {
        let bytes = cx.config()?;
        Ok(Cbor(configure(bytes)))
    }
}

/// Of the invocation, the command line alone.
pub(crate) struct Asking;

impl Query for Asking {
    const KIND: QueryKind = QueryKind(0x6011);
    const NAME: &'static str = "cargo asked";
    const PRODUCER: ContentId = producer("blaze v2 cargo asked 4");
    type Key = ();
    type Value = Cbor<Result<Asked, LoadError>>;

    fn input(key: &()) -> ContentId {
        key_id("blaze v2 cargo asked key 1", key)
    }

    fn execute(cx: &mut Cx<'_>, _: &()) -> Result<Cbor<Result<Asked, LoadError>>, Cancelled> {
        Ok(Cbor(cx.get::<Invocation>(&())?.get().asked))
    }
}

impl Versioned for Result<Asked, LoadError> {
    const SCHEMA: SchemaHash = blaze_format::schema("blaze v2 cargo asked or not 5");
}

fn configure(bytes: &[u8]) -> Configured {
    let empty = MountTable { mounts: Vec::new(), outside: Vec::new(), known: Vec::new() };
    let config = match BuildConfig::decode(bytes) {
        Ok(config) => config,
        Err(what) => return Configured { asked: Err(LoadError { what, at: None }), mounts: empty },
    };
    let invocation = config.invocation;
    let whole = |name: &str| crate::query::env_var(&invocation.env, name);
    let ci = crate::lock::ci_says(whole("CI"), whole("TF_BUILD"));
    let lock_env_writes = match crate::lock::env_writes(whole(crate::lock::LOCK_ENV)) {
        Ok(it) => it,
        Err(what) => return Configured { asked: Err(LoadError { what, at: None }), mounts: empty },
    };
    let ignore_pin_for = whole("BLAZE_IGNORE_PIN_FOR").filter(|it| !it.is_empty()).map(str::to_owned);
    let env: Vec<(String, String)> =
        invocation.env.into_iter().filter(|(name, _)| read_by_cargo(name)).collect();
    let mut known: Vec<String> = Vec::new();
    for dir in ancestors(&format!("{}/x", invocation.root))
        .into_iter()
        .chain(ancestors(&format!("{}/x", invocation.cwd)))
    {
        if !known.contains(&dir) {
            known.push(dir);
        }
    }
    let lookup =
        |name: &str| env.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str());
    let home = match lookup("CARGO_HOME") {
        Some(home) if home.starts_with('/') => Some(home.to_owned()),
        Some(home) => Some(format!("{}/{home}", invocation.cwd)),
        None => lookup("HOME").filter(|it| !it.is_empty()).map(|home| format!("{home}/.cargo")),
    };
    if let Some(home) =
        home.as_deref().and_then(|it| super::files::normal(std::path::Path::new(it)))
    {
        if !known.contains(&home) {
            known.push(home);
        }
    }
    known.sort();
    let mut outside = invocation.files;
    outside.sort();
    let mut mounts = config.mounts;
    mounts.sort();
    let mounts = MountTable { mounts, outside, known };
    let mut asked = Asked {
        root: invocation.root,
        cwd: invocation.cwd,
        env,
        host: config.host,
        library: config.library,
        std: config.std,
        builds: config.builds,
        sources: config.sources,
        ci,
        lock_env_writes,
        ignore_pin_for,
        blaze: config.blaze,
        ..Asked::default()
    };
    asked.command = Some(invocation.command);
    let asked = parse(invocation.command, &invocation.args, asked)
        .map_err(|what| LoadError { what, at: None });
    Configured { asked, mounts }
}

/// The variables cargo reads by name (cargo-notes section 1), and clippy's driver's
/// `CLIPPY_CONF_DIR`: no other one is kept.
fn read_by_cargo(name: &str) -> bool {
    name.starts_with("CARGO_")
        || matches!(
            name,
            "RUSTFLAGS"
                | "CLIPPY_CONF_DIR"
                | "RUSTDOCFLAGS"
                | "RUSTC_WORKSPACE_WRAPPER"
                | "HOME"
                | "USERPROFILE"
                | "__CARGO_DEFAULT_LIB_METADATA"
                | "__CARGO_TEMPORARY_BUILD_DIR_NEW_LAYOUT_OPT_OUT"
                | "__CARGO_TEMPORARY_STOP_RLIB_UPLIFT_OPT_OUT"
        )
}

/// How an argument of cargo's takes a value.
enum Takes {
    Flag(fn(&mut Asked)),
    Value(fn(&mut Asked, String) -> Result<(), String>),
}

/// cargo's arguments of the building commands, by long name; `short` is its letter.
fn argument(command: Command, name: &str) -> Option<Takes> {
    // `cargo doc` selects with `arg_targets_lib_bins_examples` alone.
    if command == Command::Doc
        && matches!(name, "test" | "tests" | "bench" | "benches" | "all-targets")
    {
        return None;
    }
    let common: Option<Takes> = match name {
        "package" => Some(Takes::Value(|a, v| {
            a.packages.push(v);
            Ok(())
        })),
        "workspace" | "all" => Some(Takes::Flag(|a| a.workspace = true)),
        "exclude" => Some(Takes::Value(|a, v| {
            a.exclude.push(v);
            Ok(())
        })),
        "lib" => Some(Takes::Flag(|a| a.lib = true)),
        "bin" => Some(Takes::Value(|a, v| {
            a.bins.push(v);
            Ok(())
        })),
        "bins" => Some(Takes::Flag(|a| a.all_bins = true)),
        "example" => Some(Takes::Value(|a, v| {
            a.examples.push(v);
            Ok(())
        })),
        "examples" => Some(Takes::Flag(|a| a.all_examples = true)),
        "test" => Some(Takes::Value(|a, v| {
            a.tests.push(v);
            Ok(())
        })),
        "tests" => Some(Takes::Flag(|a| a.all_tests = true)),
        "bench" => Some(Takes::Value(|a, v| {
            a.benches.push(v);
            Ok(())
        })),
        "benches" => Some(Takes::Flag(|a| a.all_benches = true)),
        "all-targets" => Some(Takes::Flag(|a| a.all_targets = true)),
        "features" => Some(Takes::Value(|a, v| {
            a.features.extend(v.split([' ', ',']).filter(|it| !it.is_empty()).map(str::to_owned));
            Ok(())
        })),
        "all-features" => Some(Takes::Flag(|a| a.all_features = true)),
        "no-default-features" => Some(Takes::Flag(|a| a.no_default_features = true)),
        "release" => Some(Takes::Flag(|a| a.release = true)),
        "profile" => Some(Takes::Value(|a, v| {
            a.profile = Some(v);
            Ok(())
        })),
        "target" => Some(Takes::Value(|a, v| {
            a.targets.push(v);
            Ok(())
        })),
        "target-dir" => Some(Takes::Value(|a, v| {
            a.target_dir = Some(v);
            Ok(())
        })),
        "manifest-path" => Some(Takes::Value(|a, v| {
            a.manifest_path = Some(v);
            Ok(())
        })),
        "locked" => Some(Takes::Flag(|a| a.locked = true)),
        "offline" => Some(Takes::Flag(|a| a.offline = true)),
        "frozen" => Some(Takes::Flag(|a| a.frozen = true)),
        "no-locked" => Some(Takes::Flag(|a| a.no_locked = true)),
        "config" => Some(Takes::Value(|a, v| {
            a.config.push(v);
            Ok(())
        })),
        "jobs" => Some(Takes::Value(|a, v| {
            let jobs: i64 = v.parse().map_err(|_| {
                format!("invalid value '{v}' for '--jobs <N>': invalid digit found in string")
            })?;
            if jobs == 0 {
                return Err("\"0\" is not a valid value for jobs".to_owned());
            }
            a.jobs = Some(if jobs < 0 { 1 } else { u32::try_from(jobs).unwrap_or(u32::MAX) });
            Ok(())
        })),
        "ignore-rust-version" => Some(Takes::Flag(|a| a.ignore_rust_version = true)),
        // What changes only what is printed or how the build runs, not what is built: the CLI's.
        "verbose"
        | "quiet"
        | "keep-going"
        | "fail-fast"
        | "future-incompat-report"
        | "unit-graph" => Some(Takes::Flag(|_| {})),
        "color" | "message-format" | "lockfile-path" | "artifact-dir" => {
            Some(Takes::Value(|_, _| Ok(())))
        }
        _ => None,
    };
    if common.is_some() {
        return common;
    }
    match (command, name) {
        (Command::Test, "doc") => Some(Takes::Flag(|a| a.doc = true)),
        (Command::Test | Command::Bench, "no-run" | "no-fail-fast") => Some(Takes::Flag(|_| {})),
        // Whether the plan reads `doc.browser`; the pages are opened by the CLI.
        (Command::Doc, "open") => Some(Takes::Flag(|a| a.open = true)),
        // What the documentation is made of, not which units: the request's (`Request::Doc`).
        (Command::Doc, "no-deps" | "document-private-items") => Some(Takes::Flag(|_| {})),
        _ => None,
    }
}

fn short(letter: char) -> Option<&'static str> {
    Some(match letter {
        'p' => "package",
        'F' => "features",
        'r' => "release",
        'j' => "jobs",
        'v' => "verbose",
        'q' => "quiet",
        _ => return None,
    })
}

/// The arguments after the subcommand and before `--`, as clap reads cargo's; `clippy`'s after
/// `--` are its driver's.
fn parse(command: Command, args: &[String], mut asked: Asked) -> Result<Asked, String> {
    let mut args = args.iter();
    let mut positional = 0;
    while let Some(arg) = args.next() {
        let (name, inline): (String, Option<String>) = if let Some(long) = arg.strip_prefix("--") {
            match long.split_once('=') {
                Some((name, value)) => (name.to_owned(), Some(value.to_owned())),
                None => (long.to_owned(), None),
            }
        } else if let Some(rest) = arg.strip_prefix('-').filter(|it| !it.is_empty()) {
            let mut letters = rest.chars();
            let letter = letters.next().unwrap_or('-');
            if letter == 'Z' {
                let value = match letters.as_str() {
                    "" => args.next().cloned().ok_or_else(|| missing("-Z <FLAG>"))?,
                    attached => attached.to_owned(),
                };
                asked.unstable.push(value);
                continue;
            }
            let Some(long) = short(letter) else { return Err(unexpected(arg)) };
            let attached = letters.as_str();
            match argument(command, long) {
                Some(Takes::Flag(set)) => {
                    set(&mut asked);
                    for letter in attached.chars() {
                        match short(letter).and_then(|it| argument(command, it)) {
                            Some(Takes::Flag(set)) => set(&mut asked),
                            _ => return Err(unexpected(&format!("-{letter}"))),
                        }
                    }
                    continue;
                }
                Some(Takes::Value(_)) if !attached.is_empty() => {
                    (long.to_owned(), Some(attached.trim_start_matches('=').to_owned()))
                }
                _ => (long.to_owned(), None),
            }
        } else {
            match command {
                Command::Test | Command::Bench if positional == 0 => {
                    positional += 1;
                    continue;
                }
                Command::Run => break,
                _ => return Err(unexpected(arg)),
            }
        };
        match argument(command, &name) {
            Some(Takes::Flag(set)) => {
                if inline.is_some() {
                    return Err(format!(
                        "unexpected value for '--{name}' found; no more were expected"
                    ));
                }
                set(&mut asked);
            }
            Some(Takes::Value(set)) => {
                let value = match inline {
                    Some(value) => value,
                    None => {
                        args.next().cloned().ok_or_else(|| missing(&format!("--{name} <VALUE>")))?
                    }
                };
                set(&mut asked, value)?;
            }
            None => return Err(unexpected(&format!("--{name}"))),
        }
    }
    Ok(asked)
}

fn unexpected(arg: &str) -> String {
    format!("unexpected argument '{arg}' found")
}

fn missing(what: &str) -> String {
    format!("a value is required for '{what}' but none was supplied")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asked(command: Command, args: &[&str]) -> Result<Asked, String> {
        let args: Vec<String> = args.iter().map(|it| (*it).to_owned()).collect();
        parse(command, &args, Asked::default())
    }

    #[test]
    fn cargos_flags_are_read() {
        let it = asked(
            Command::Build,
            &[
                "-p",
                "a",
                "--package=b",
                "-Fx,y",
                "--features",
                "z w",
                "-rv",
                "--target",
                "wasm32-wasip1",
                "-j4",
                "-Zbuild-std",
            ],
        )
        .unwrap();
        assert_eq!(it.packages, ["a", "b"]);
        assert_eq!(it.features, ["x", "y", "z", "w"]);
        assert!(it.release);
        assert_eq!(it.targets, ["wasm32-wasip1"]);
        assert_eq!(it.jobs, Some(4));
        assert_eq!(it.unstable, ["build-std"]);
    }

    #[test]
    fn a_test_takes_one_filter_and_run_its_programs_arguments() {
        let it = asked(Command::Test, &["foo", "--lib", "--doc"]).unwrap();
        assert!(it.lib && it.doc);
        assert_eq!(
            asked(Command::Test, &["foo", "bar"]).unwrap_err(),
            "unexpected argument 'bar' found"
        );
        let run = asked(Command::Run, &["--bin", "x", "arg", "--release"]).unwrap();
        assert_eq!(run.bins, ["x"]);
        assert!(!run.release, "what follows the program's first argument is its own");
        assert_eq!(
            asked(Command::Build, &["--nope"]).unwrap_err(),
            "unexpected argument '--nope' found"
        );
        assert_eq!(
            asked(Command::Build, &["--doc"]).unwrap_err(),
            "unexpected argument '--doc' found"
        );
        assert!(
            asked(Command::Build, &["--target"]).unwrap_err().starts_with("a value is required")
        );
    }

    #[test]
    fn doc_takes_its_own_flags_and_cargo_docs_targets_alone() {
        let it = asked(
            Command::Doc,
            &["--open", "--no-deps", "--document-private-items", "-p", "a", "--lib", "--bin", "x"],
        )
        .unwrap();
        assert_eq!(it.packages, ["a"]);
        assert!(it.lib);
        assert_eq!(it.bins, ["x"]);
        assert!(it.open);
        assert!(!asked(Command::Doc, &["--no-deps"]).unwrap().open);
        assert!(asked(Command::Doc, &["--examples", "--example", "e", "--release"]).is_ok());
        for refused in ["--tests", "--benches", "--all-targets", "--doc"] {
            assert_eq!(
                asked(Command::Doc, &[refused]).unwrap_err(),
                format!("unexpected argument '{refused}' found")
            );
        }
        assert_eq!(
            asked(Command::Doc, &["--test", "t"]).unwrap_err(),
            "unexpected argument '--test' found"
        );
        assert_eq!(
            asked(Command::Check, &["--no-deps"]).unwrap_err(),
            "unexpected argument '--no-deps' found"
        );
        assert_eq!(Command::named("d"), Some(Command::Doc));
        assert_eq!(Command::Doc.profile(), "dev");
    }

    /// `check` lints, so there is no `clippy` command, and no driver flags after `--`.
    #[test]
    fn clippy_is_no_command_and_check_takes_no_drivers_flags() {
        assert_eq!(Command::named("clippy"), None);
        assert_eq!(Command::named("fix"), None);
        assert_eq!(
            asked(Command::Check, &["--", "-D"]).unwrap_err(),
            "unexpected argument '--' found"
        );
        assert_eq!(
            asked(Command::Check, &["--no-deps"]).unwrap_err(),
            "unexpected argument '--no-deps' found"
        );
        assert_eq!(asked(Command::Check, &["-p", "a", "--all-targets"]).unwrap().packages, ["a"]);
    }

    #[test]
    fn only_what_cargo_reads_is_kept() {
        let config = BuildConfig {
            invocation: crate::query::Invocation {
                command: Command::Build,
                args: vec![],
                root: "/w".to_owned(),
                cwd: "/w/a".to_owned(),
                env: vec![
                    ("CARGO_TARGET_DIR".to_owned(), "/t".to_owned()),
                    ("HOME".to_owned(), "/home/u".to_owned()),
                    ("PATH".to_owned(), "/bin".to_owned()),
                    ("RUSTFLAGS".to_owned(), "-Dwarnings".to_owned()),
                    ("SECRET".to_owned(), "x".to_owned()),
                ],
                files: vec![],
            },
            mounts: vec![],
            host: "x86_64-unknown-linux-gnu".to_owned(),
            library: "/rustc/0/library".to_owned(),
            std: "/s/std".to_owned(),
            builds: "/s/build".to_owned(),
            sources: "/s/sources".to_owned(),
            blaze: String::new(),
        };
        let configured = configure(&config.encode());
        let asked = configured.asked.unwrap();
        let names: Vec<&str> = asked.env.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ["CARGO_TARGET_DIR", "HOME", "RUSTFLAGS"]);
        assert!(configured.mounts.known.contains(&"/home/u/.cargo".to_owned()));
        assert!(configured.mounts.known.contains(&"/w".to_owned()));
        assert!(configured.mounts.known.contains(&"/".to_owned()));
    }
}
