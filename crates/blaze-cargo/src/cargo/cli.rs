//! `blaze <subcommand> [flags] [-- args]`: the flags are cargo's and go to cargo as they
//! are. The few read here decide whether blaze has a part in the build and which build of
//! the checkout it is.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use sha2::{Digest as _, Sha256};

use crate::plan::Selection;

/// The flags of cargo's build commands that take their value as the next argument.
const VALUED: &[&str] = &[
    "--events",
    "-p",
    "--package",
    "--exclude",
    "-j",
    "--jobs",
    "--bin",
    "--example",
    "--test",
    "--bench",
    "-F",
    "--features",
    "--profile",
    "--target",
    "--target-dir",
    "--artifact-dir",
    "--manifest-path",
    "--lockfile-path",
    "--message-format",
    "--color",
    "--config",
    "-Z",
    "-C",
];

/// The flags that pick targets of a package, after which cargo runs no doc tests.
const TARGET_SELECTION: &[&str] = &[
    "--lib",
    "--bins",
    "--bin",
    "--examples",
    "--example",
    "--tests",
    "--test",
    "--benches",
    "--bench",
    "--all-targets",
];

/// The flags that pick targets cargo builds with the dev-dependencies: examples, tests and
/// benches.
const DEV_TARGETS: &[&str] =
    &["--examples", "--example", "--tests", "--test", "--benches", "--bench", "--all-targets"];

/// The flags of `cargo test` that are about running and not about building, and blaze's
/// `--why` and `--json`, which say why each test runs or not instead of running them.
const RUNNING: &[&str] = &["--no-run", "--no-fail-fast", "--why", "--json"];

/// The flags blaze alone reads: none is cargo's, and none changes which build this is.
const BLAZES: &[&str] = &["--events"];

/// The flags that change only what cargo prints, with `--color`'s value: the build is the
/// same.
const SHOWING: &[&str] = &["-v", "-vv", "--verbose", "-q", "--quiet", "--color"];

/// The variables that change what cargo builds or what the daemon compiles with, by name
/// or by what the name starts with. Their values are part of which build this is.
const BUILD_ENV: &[&str] = &[
    "RUSTFLAGS",
    "RUSTC",
    "RUSTC_WRAPPER",
    "RUSTUP_TOOLCHAIN",
    "CARGO_ENCODED_RUSTFLAGS",
    "CARGO_TARGET_DIR",
    "CARGO_BUILD_",
    "CARGO_PROFILE_",
    "CARGO_INCREMENTAL",
    "BLAZE_BACKEND",
    "BLAZE_OPT",
    "BLAZE_MIR_OPT",
    "BLAZE_EMIT",
    "BLAZE_SPLICE",
    "BLAZE_BUILT_IN",
    "BLAZE_LLVM_INLINE_THRESHOLD",
    super::own_macro::SERVER,
    super::own_macro::WASM,
];

/// The variables a worker is started with: the ones that pick its toolchain and its backend,
/// what its first build is made over, and the ones cargo reads flags and profiles from. One
/// left out reaches a worker only from whoever started the daemon.
const WORKER_ENV: &[&str] = &[
    "RUSTFLAGS",
    "CARGO_ENCODED_RUSTFLAGS",
    "RUSTDOCFLAGS",
    "CARGO_ENCODED_RUSTDOCFLAGS",
    "CARGO_BUILD_",
    "CARGO_PROFILE_",
    "RUSTUP_TOOLCHAIN",
    "RUSTUP_HOME",
    "CARGO_HOME",
    "PATH",
    "BLAZE_BACKEND",
    "BLAZE_OPT",
    "BLAZE_MIR_OPT",
    "BLAZE_EMIT",
    "BLAZE_SPLICE",
    "BLAZE_BUILT_IN",
    "BLAZE_LLVM_INLINE_THRESHOLD",
    super::own_macro::SERVER,
    super::own_macro::WASM,
    super::layout::NEW_LAYOUT,
];

/// The variables of [`WORKER_ENV`] a worker has only as its build has them: unset there, they
/// are unset for the worker too, and not the daemon's. Which proc-macro server a load uses
/// ([`super::own_macro::SERVER`]) is the build's, read when it asks, never the one the daemon
/// was started with (#460). So are the flags and profiles cargo reads from the environment: a
/// build without `RUSTFLAGS` is not built with those of whoever started the daemon. So is the
/// layout of the build directory: plain cargo's build, which the daemon loads too, is in the
/// layout the user's cargo chose.
const WORKER_ENV_EXACT: &[&str] = &[
    "RUSTFLAGS",
    "CARGO_ENCODED_RUSTFLAGS",
    "RUSTDOCFLAGS",
    "CARGO_ENCODED_RUSTDOCFLAGS",
    "CARGO_BUILD_",
    "CARGO_PROFILE_",
    super::own_macro::SERVER,
    super::own_macro::WASM,
    super::layout::NEW_LAYOUT,
];

/// `CARGO_TARGET_<triple>_RUSTFLAGS`, which cargo reads as `[target.<triple>] rustflags`.
fn target_rustflags(name: &str) -> bool {
    name.strip_prefix("CARGO_TARGET_").is_some_and(|it| it.ends_with("_RUSTFLAGS"))
}

fn is_named(names: &[&str], name: &str) -> bool {
    names.iter().any(|it| if it.ends_with('_') { name.starts_with(it) } else { name == *it })
}

/// Whether the variable `name` is one of [`WORKER_ENV`], which a worker has from its build.
fn worker_named(name: &str) -> bool {
    is_named(WORKER_ENV, name) || target_rustflags(name)
}

/// Whether the variable `name` is blaze's own: where the daemon keeps its files, the store,
/// its traces. The daemon's are what its workers run with.
fn blazes(name: &str) -> bool {
    name.starts_with("BLAZE_")
}

/// The variables of this process that a worker of the build whose variables are `env`
/// ([`super::daemon::WorkerConfig::builds_env`]) does not have, to be taken away from what it
/// inherits: of [`WORKER_ENV`], those of [`WORKER_ENV_EXACT`], and every other one that is not
/// blaze's own. A build script reads the command's environment, as cargo's does, and a
/// variable it says it reads (`rerun-if-env-changed`) the command left unset is unset.
pub fn not_the_builds(env: &BTreeMap<String, String>) -> Vec<String> {
    taken_away(std::env::vars_os().filter_map(|(name, _)| name.into_string().ok()), env)
}

/// Of the variables `names` a process has, the ones [`not_the_builds`] takes away.
fn taken_away(names: impl Iterator<Item = String>, env: &BTreeMap<String, String>) -> Vec<String> {
    names
        .filter(|name| match worker_named(name) {
            true => is_named(WORKER_ENV_EXACT, name) || target_rustflags(name),
            false => !blazes(name),
        })
        .filter(|name| !env.contains_key(name))
        .collect()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Subcommand {
    Build,
    Run,
    Test,
    /// What a build of the same flags says of the source, and no program run.
    Check,
}

impl Subcommand {
    fn named(name: &str) -> Option<Subcommand> {
        match name {
            "build" | "b" => Some(Subcommand::Build),
            "run" | "r" => Some(Subcommand::Run),
            "test" | "t" => Some(Subcommand::Test),
            "check" | "c" => Some(Subcommand::Check),
            _ => None,
        }
    }
}

/// What the user asked for.
#[derive(Debug, PartialEq)]
pub enum Asked {
    Build(Box<Invocation>),
    /// A subcommand that is not a build, with its arguments: blaze's own, or one it has not
    /// written yet, which fails saying so (`own`). None is cargo's.
    Own(Vec<String>),
    /// `cache` and its arguments: the kept results of `#[blaze::cache]` functions, or the
    /// disk store.
    Cache(Vec<String>),
    Status,
    Stop,
    /// `why` and its arguments: what the last build compiled again and why.
    Why(Vec<String>),
    /// `watch`: every build the daemon does, as it does it, until interrupted.
    Watch,
    /// `ui`: where the build page is, opened in the browser unless `--no-open`.
    Ui {
        open: bool,
    },
    /// blaze's own toolchain, downloaded first when it has none; with `--path`, its directory
    /// alone, for the installer to link.
    Toolchain {
        path: bool,
    },
    /// `lock` and its arguments: what build scripts download, pinned (`fetches.rs`).
    Lock(Vec<String>),
    /// `pin` and its arguments: this blaze's version written into the workspace's manifest,
    /// so that the `blaze` anyone runs there runs this release (`versions.rs`).
    Pin(Vec<String>),
    Help,
    Version,
}

/// A build command and its arguments, split into what is cargo's to read and what is the
/// program's.
#[derive(Debug, PartialEq)]
pub struct Invocation {
    pub subcommand: Subcommand,
    /// Everything after the subcommand, as typed.
    pub typed: Vec<String>,
    /// The flags that say what to build.
    pub build_args: Vec<String>,
    /// The words before `--` that are no flags: arguments of the program for `run`, the
    /// names of the tests to run for `test`.
    pub positional: Vec<String>,
    /// What follows `--`, for the program or the test executables.
    pub trailing: Vec<String>,
    pub flags: Flags,
}

/// The flags blaze reads.
#[derive(Debug, Default, PartialEq)]
pub struct Flags {
    pub manifest_path: Option<PathBuf>,
    pub packages: Vec<String>,
    pub workspace: bool,
    /// The packages `--exclude` leaves out of `--workspace`.
    pub excludes: Vec<String>,
    pub features: Vec<String>,
    pub all_features: bool,
    pub no_default_features: bool,
    pub profile: Option<String>,
    pub config: Vec<String>,
    pub targets: Vec<String>,
    pub bin: Option<String>,
    pub example: Option<String>,
    /// The values of `--message-format`, as typed.
    pub message_format: Vec<String>,
    /// `--build-plan` or `--unit-graph`: what cargo would do, which only cargo says.
    pub cargos_plan: bool,
    /// The values of `-Z`.
    pub unstable: Vec<String>,
    pub quiet: bool,
    pub verbose: bool,
    /// `--events json` or `cbor`: the records of the build on standard output, as they come.
    pub events: Option<String>,
    pub no_run: bool,
    pub no_fail_fast: bool,
    /// `blaze test --why`: say of each test whether it is hermetic and why not, and run none.
    pub why: bool,
    /// With `--why`, the tests' lines of `tests.ndjson` on stdout instead of a table.
    pub json: bool,
    pub doc: bool,
    /// Whether a flag picks targets, so that `cargo test` runs no doc tests.
    pub selects_targets: bool,
    /// Whether a flag picks a target that has the dev-dependencies.
    pub dev_targets: bool,
}

impl Asked {
    /// From the arguments after `blaze`.
    pub fn parse(args: Vec<String>) -> Asked {
        let Some((first, rest)) = args.split_first() else {
            return Asked::Help;
        };
        match (first.as_str(), Subcommand::named(first)) {
            ("--help" | "-h" | "help", _) => Asked::Help,
            ("--version" | "-V", _) => Asked::Version,
            (_, Some(subcommand)) => Asked::Build(Box::new(Invocation::parse(subcommand, rest))),
            ("cache", _) => Asked::Cache(rest.to_vec()),
            ("status", _) => Asked::Status,
            ("stop", _) => Asked::Stop,
            ("why", _) => Asked::Why(rest.to_vec()),
            ("watch", _) => Asked::Watch,
            ("ui", _) => Asked::Ui { open: !rest.iter().any(|it| it == "--no-open") },
            ("toolchain", _) => Asked::Toolchain { path: rest.iter().any(|it| it == "--path") },
            ("lock", _) => Asked::Lock(rest.to_vec()),
            ("pin", _) => Asked::Pin(rest.to_vec()),
            _ => Asked::Own(args),
        }
    }
}

impl Invocation {
    fn parse(subcommand: Subcommand, typed: &[String]) -> Invocation {
        let (before, after) = match typed.iter().position(|it| it == "--") {
            Some(at) => (&typed[..at], &typed[at + 1..]),
            None => (typed, &[][..]),
        };
        let mut flags = Flags::default();
        let mut build_args = Vec::new();
        let mut positional = Vec::new();
        let mut words = before.iter();
        while let Some(word) = words.next() {
            if !word.starts_with('-') {
                positional.push(word.clone());
                continue;
            }
            let (flag, joined) = match word.split_once('=') {
                Some((flag, value)) if flag.starts_with("--") => (flag, Some(value.to_owned())),
                // `-pname`, `-j8`.
                _ if !word.starts_with("--") && word.len() > 2 && VALUED.contains(&&word[..2]) => {
                    (&word[..2], Some(word[2..].to_owned()))
                }
                _ => (word.as_str(), None),
            };
            let value = match joined {
                None if VALUED.contains(&flag) => words.next().cloned(),
                joined => joined,
            };
            flags.read(flag, value.as_deref());
            if RUNNING.contains(&flag) || BLAZES.contains(&flag) {
                continue;
            }
            build_args.push(flag.to_owned());
            build_args.extend(value);
        }
        let trailing = after.to_vec();
        Invocation { subcommand, typed: typed.to_vec(), build_args, positional, trailing, flags }
    }

    /// What the program or a test executable is run with: `cargo run a -- b` runs the
    /// program with `a b`, and `cargo test a -- b` the tests named like `a`, with `b`.
    pub fn program_args(&self) -> impl Iterator<Item = &String> {
        self.positional.iter().chain(&self.trailing)
    }

    /// Which build of the checkout this is: a hash of what cargo is asked to build, where,
    /// and with what in the environment.
    pub fn key(&self, cwd: &Path) -> String {
        let mut hash = Sha256::new();
        // `run` builds what `build` does.
        let builds = if self.subcommand == Subcommand::Test { "test" } else { "build" };
        hash.update(builds);
        hash.update(cwd.as_os_str().as_encoded_bytes());
        let mut args = self.build_args.iter();
        while let Some(arg) = args.next() {
            if SHOWING.contains(&arg.as_str()) {
                if arg == "--color" {
                    args.next();
                }
                continue;
            }
            hash.update([0]);
            hash.update(arg);
        }
        for (name, value) in build_env() {
            hash.update([1]);
            hash.update(name);
            hash.update([2]);
            hash.update(value);
        }
        hash.finalize().iter().take(8).map(|it| format!("{it:02x}")).collect()
    }

    /// The first flag typed that picks targets of a kind, tests or benches: `--bin` and
    /// `--example` name a program, `--lib` picks none or, of `cargo test`, the library's unit
    /// tests, and `--test` names an integration test there.
    pub fn kinds_of_targets(&self) -> Option<&str> {
        let one: &[&str] = match self.subcommand {
            Subcommand::Test => &["--lib", "--bin", "--example", "--test"],
            _ => &["--lib", "--bin", "--example"],
        };
        let typed = self.build_args.iter().map(|it| it.split('=').next().unwrap_or(it));
        typed.into_iter().find(|it| TARGET_SELECTION.contains(it) && !one.contains(it))
    }

    /// The values typed for `flag`, as `flag value` or `flag=value`.
    pub fn values_of(&self, flag: &str) -> Vec<&str> {
        let mut values = Vec::new();
        let mut words = self.build_args.iter();
        while let Some(word) = words.next() {
            match word.strip_prefix(flag) {
                Some("") => values.extend(words.next().map(String::as_str)),
                Some(value) if value.starts_with('=') => values.push(&value[1..]),
                _ => {}
            }
        }
        values
    }

    /// Why blaze does not link programs for the target of the build.
    pub fn target_not_linked(&self) -> Option<String> {
        self.target().and_then(|it| unsupported_target(&it))
    }

    /// The flags typed that pick targets, `--bin NAME` and the like with their values.
    pub fn target_flags(&self) -> Vec<String> {
        let mut flags = Vec::new();
        let mut words = self.build_args.iter();
        while let Some(word) = words.next() {
            if TARGET_SELECTION.contains(&word.as_str()) {
                flags.push(word.clone());
                if VALUED.contains(&word.as_str()) {
                    flags.extend(words.next().cloned());
                }
            }
        }
        flags
    }

    /// Whether `flag` was typed.
    pub fn typed(&self, flag: &str) -> bool {
        self.build_args.iter().any(|it| it == flag)
    }

    /// The profile, as cargo's `Finished` line names it.
    pub fn profile(&self) -> &str {
        let of_command = if self.subcommand == Subcommand::Test { "test" } else { "dev" };
        self.flags.profile.as_deref().unwrap_or(of_command)
    }

    /// Which build of the package the command is, as the daemon plans it. With no package
    /// named it is the one of the directory the worker is started in.
    pub fn selection(&self) -> Selection {
        Selection {
            profile: self.flags.profile.clone().unwrap_or_else(|| "dev".to_owned()),
            features: self.flags.features.clone(),
            no_default_features: self.flags.no_default_features,
            all_features: self.flags.all_features,
            packages: self.flags.packages.clone(),
            workspace: self.flags.workspace,
            excludes: self.flags.excludes.clone(),
            unstable: self.flags.unstable.clone(),
            target: self.target(),
            dev_deps: self.subcommand == Subcommand::Test || self.flags.dev_targets,
            config: self.flags.config.clone(),
            asked_in: None,
        }
    }

    /// The triple cargo builds for, when it is told one.
    pub fn target(&self) -> Option<String> {
        let told = std::env::var("CARGO_BUILD_TARGET").ok().filter(|it| !it.is_empty());
        self.flags.targets.first().cloned().or(told)
    }
}

impl Flags {
    fn read(&mut self, flag: &str, value: Option<&str>) {
        let owned = || value.map(str::to_owned);
        self.selects_targets |= TARGET_SELECTION.contains(&flag);
        self.dev_targets |= DEV_TARGETS.contains(&flag);
        match flag {
            "--manifest-path" => self.manifest_path = value.map(PathBuf::from),
            "-p" | "--package" => self.packages.extend(owned()),
            "--workspace" | "--all" => self.workspace = true,
            "--exclude" => self.excludes.extend(owned()),
            "-F" | "--features" => {
                let features = value.unwrap_or_default().split([',', ' ']);
                self.features.extend(features.filter(|it| !it.is_empty()).map(str::to_owned));
            }
            "--all-features" => self.all_features = true,
            "--no-default-features" => self.no_default_features = true,
            "-r" | "--release" => self.profile = Some("release".to_owned()),
            "--profile" => self.profile = owned(),
            "--config" => self.config.extend(owned()),
            "--target" => self.targets.extend(owned()),
            "--bin" => self.bin = owned(),
            "--example" => self.example = owned(),
            "--message-format" => self.message_format.extend(owned()),
            "--build-plan" | "--unit-graph" => self.cargos_plan = true,
            "-Z" => self.unstable.extend(owned()),
            "-q" | "--quiet" => self.quiet = true,
            "--events" => self.events = owned(),
            "-v" | "-vv" | "--verbose" => self.verbose = true,
            "--no-run" => self.no_run = true,
            "--no-fail-fast" => self.no_fail_fast = true,
            "--why" => self.why = true,
            "--json" => self.json = true,
            "--doc" => self.doc = true,
            _ => {}
        }
    }
}

/// Why blaze does not build for `triple`, when it does not.
fn unsupported_target(triple: &str) -> Option<String> {
    use hir_ty_codegen::{ObjectFormat, Target};
    match Target::select(triple) {
        Ok(target)
            if matches!(
                target.object_format,
                ObjectFormat::Elf | ObjectFormat::MachO | ObjectFormat::Coff | ObjectFormat::Wasm
            ) =>
        {
            None
        }
        Ok(_) => Some(format!("programs for {triple} are not linked through `blaze` yet")),
        Err(why) => Some(why),
    }
}

/// The variables of this process that are part of which build it is.
pub fn build_env() -> BTreeMap<String, String> {
    env_named(BUILD_ENV)
}

/// The variables of this process that a worker for its builds runs with.
pub fn worker_env() -> BTreeMap<String, String> {
    let mut env = env_named(WORKER_ENV);
    env.extend(std::env::vars().filter(|(name, _)| target_rustflags(name)));
    env
}

/// The variables of this process its build scripts may be given that [`worker_env`] leaves
/// out: what a script says it reads or the root manifest's `build-script-env` names is the
/// command's, not that of whoever started the daemon. Not blaze's own.
pub fn command_env() -> BTreeMap<String, String> {
    std::env::vars_os()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
        .filter(|(name, _)| !worker_named(name) && !blazes(name))
        .collect()
}

fn env_named(names: &[&str]) -> BTreeMap<String, String> {
    std::env::vars().filter(|(name, _)| is_named(names, name)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(subcommand: &str, args: &str) -> Invocation {
        let words = std::iter::once(subcommand).chain(args.split_whitespace());
        match Asked::parse(words.map(str::to_owned).collect()) {
            Asked::Build(invocation) => *invocation,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn flags_are_the_builds() {
        let exact = |name| is_named(WORKER_ENV_EXACT, name) || target_rustflags(name);
        assert!(exact("RUSTFLAGS") && exact("CARGO_ENCODED_RUSTDOCFLAGS"));
        assert!(exact("CARGO_PROFILE_RELEASE_DEBUG_ASSERTIONS") && exact("CARGO_BUILD_RUSTFLAGS"));
        assert!(exact("CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS"));
        assert!(!exact("CARGO_TARGET_DIR") && !exact("PATH") && !exact("RUSTUP_TOOLCHAIN"));
    }

    #[test]
    fn what_a_script_reads_is_the_commands() {
        let daemons = ["SDK_DIR", "RUSTFLAGS", "PATH", "RUSTUP_TOOLCHAIN", "BLAZE_CACHE", "CC"];
        let builds = BTreeMap::from([
            ("PATH".to_owned(), "/bin".to_owned()),
            ("CC".to_owned(), "cc".to_owned()),
        ]);
        let taken = taken_away(daemons.into_iter().map(str::to_owned), &builds);
        assert_eq!(taken, ["SDK_DIR", "RUSTFLAGS"]);
    }

    #[test]
    fn events_are_blazes_flag_and_not_the_builds() {
        let cwd = Path::new("/w");
        for typed in ["--events json -p a", "--events=cbor -p a"] {
            let asked = build("build", typed);
            assert!(asked.flags.events.is_some(), "{typed}");
            assert_eq!(asked.build_args, ["-p", "a"], "{typed}");
            assert_eq!(asked.key(cwd), build("build", "-p a").key(cwd), "{typed}");
        }
    }

    #[test]
    fn flags_in_both_spellings() {
        let asked =
            build("build", "-p a --features=x,y -F z --target t --manifest-path m/Cargo.toml -r");
        assert_eq!(asked.flags.packages, ["a"]);
        assert_eq!(build("build", "-pa -j8").flags.packages, ["a"]);
        assert_eq!(asked.flags.features, ["x", "y", "z"]);
        assert_eq!(asked.flags.targets, ["t"]);
        assert_eq!(asked.flags.manifest_path.as_deref(), Some(Path::new("m/Cargo.toml")));
        assert_eq!(asked.flags.profile.as_deref(), Some("release"));
        assert_eq!(asked.program_args().count(), 0);
    }

    #[test]
    fn what_is_the_programs() {
        let run = build("run", "--bin a one -- --two");
        assert_eq!(run.build_args, ["--bin", "a"]);
        assert_eq!(run.program_args().collect::<Vec<_>>(), ["one", "--two"]);
        let test = build("test", "--no-fail-fast -p a filter --no-run -- --nocapture");
        assert_eq!(test.build_args, ["-p", "a"]);
        assert_eq!(
            (&*test.positional, &*test.trailing),
            (&["filter".to_owned()][..], &["--nocapture".to_owned()][..])
        );
        assert!(test.flags.no_fail_fast && test.flags.no_run);
        let why = build("test", "--why --json -p a");
        assert!(why.flags.why && why.flags.json);
        assert_eq!(why.build_args, ["-p", "a"]);
    }

    #[test]
    fn a_flag_without_its_value_is_cargos_to_refuse() {
        let asked = build("build", "--features");
        assert_eq!(asked.build_args, ["--features"]);
        assert!(asked.flags.features.is_empty());
    }

    #[test]
    fn what_is_not_a_build() {
        assert_eq!(Asked::parse(vec!["doc".to_owned()]), Asked::Own(vec!["doc".to_owned()]));
        assert_eq!(Asked::parse(vec!["fmt".to_owned()]), Asked::Own(vec!["fmt".to_owned()]));
        assert_eq!(Asked::parse(Vec::new()), Asked::Help);
        assert_eq!(Asked::parse(vec!["--help".to_owned()]), Asked::Help);
        assert!(build("check", "--unit-graph").flags.cargos_plan);
        let check =
            build("check", "--workspace --exclude a --exclude=b -Zx -Z y --message-format=json");
        assert_eq!(check.flags.excludes, ["a", "b"]);
        assert_eq!(check.flags.unstable, ["x", "y"]);
        assert_eq!(check.flags.message_format, ["json"]);
        assert_eq!(build("test", "--release").profile(), "release");
        assert_eq!(build("test", "").profile(), "test");
    }

    /// `Target::select` keeps the one target of the process, so this is the host's only.
    #[cfg(windows)]
    #[test]
    fn msvc_programs_are_linked_through_blaze_cli() {
        assert_eq!(unsupported_target("x86_64-pc-windows-msvc"), None);
        assert!(unsupported_target("x86_64-pc-windows-gnu").is_some());
    }

    #[test]
    fn run_is_the_build_that_build_is() {
        let cwd = Path::new("/ws");
        assert_eq!(build("run", "--bin a x -- y").key(cwd), build("build", "--bin a").key(cwd));
        assert_ne!(build("test", "--bin a").key(cwd), build("build", "--bin a").key(cwd));
        assert_ne!(build("build", "").key(cwd), build("build", "").key(Path::new("/other")));
        assert_ne!(build("build", "--features a").key(cwd), build("build", "").key(cwd));
        let shown = build("test", "-v -p a --color never -q").key(cwd);
        assert_eq!(shown, build("test", "-p a").key(cwd));
        assert_eq!(build("test", "-p a --color=always").key(cwd), build("test", "-p a").key(cwd));
    }

    #[test]
    fn check_asks_of_the_build_of_its_flags() {
        let cwd = Path::new("/ws");
        assert_eq!(build("check", "--bin a").key(cwd), build("build", "--bin a").key(cwd));
    }
}
