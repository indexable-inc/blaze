//! cargo's command lines, read as cargo's clap reads them (`cargo/src/bin/cargo/commands/`,
//! `util/command_prelude.rs`): which flags each command takes, which take a value and how, what
//! is the program's. A command line clap would refuse is refused with clap's text, which exits
//! 1. Everything else goes to the daemon as typed (`Cargo::args`), whose load is cargo's.

use std::ffi::OsString;

use super::say;

pub use blaze_units::query::Command;

/// What clap knows of a command: its usage line and its flags.
trait Clap: Sized {
    /// The command as clap's usage lines name it.
    fn usage(self) -> &'static str;
    fn has_positionals(self) -> bool;
    fn flags(self) -> &'static [&'static [Flag]];
    fn flag(self, found: impl Fn(&Flag) -> bool) -> Option<&'static Flag>;
}

impl Clap for Command {
    fn usage(self) -> &'static str {
        match self {
            Command::Build => "cargo build",
            Command::Check => "cargo check",
            Command::Run => "cargo run",
            Command::Test => "cargo test",
            Command::Bench => "cargo bench",
            Command::Doc => "cargo doc",
        }
    }

    fn has_positionals(self) -> bool {
        matches!(self, Command::Run | Command::Test | Command::Bench)
    }

    fn flags(self) -> &'static [&'static [Flag]] {
        match self {
            Command::Build => &[
                GLOBAL, FUTURE, MESSAGE, SILENT, PACKAGES, TARGETS, FEATURES, RELEASE, DEBUG, PROFILE, JOBS,
                KEEP_GOING, TARGET, ARTIFACT, REPORTS, COMPILE_TIME_DEPS, MANIFEST, REFUSED,
            ],
            Command::Check => &[
                GLOBAL, CHECK_ONLY, BASELINES, FUTURE, MESSAGE, SILENT, PACKAGES, TARGETS, FEATURES, JOBS,
                KEEP_GOING, RELEASE, PROFILE, TARGET, REPORTS, COMPILE_TIME_DEPS, MANIFEST, REFUSED,
            ],
            Command::Run => &[
                GLOBAL, MESSAGE, SILENT, PACKAGE, RUN_TARGETS, FEATURES, JOBS, KEEP_GOING, RELEASE, PROFILE,
                TARGET, MANIFEST, REPORTS, REFUSED,
            ],
            Command::Test => &[
                GLOBAL, RUNNING, FUTURE, MESSAGE, PACKAGES, TARGETS, TEST_ONLY, FEATURES, JOBS, NO_KEEP_GOING,
                RELEASE, PROFILE, TARGET, REPORTS, MANIFEST, REFUSED,
            ],
            Command::Bench => &[
                GLOBAL, RUNNING, MESSAGE, SILENT, PACKAGES, TARGETS, FEATURES, JOBS, NO_KEEP_GOING, PROFILE,
                TARGET, REPORTS, MANIFEST, REFUSED,
            ],
            Command::Doc => &[
                GLOBAL, DOC_ONLY, MESSAGE, SILENT, PACKAGES, DOC_TARGETS, FEATURES, JOBS, KEEP_GOING, RELEASE,
                PROFILE, TARGET, REPORTS, MANIFEST,
            ],
        }
    }

    fn flag(self, found: impl Fn(&Flag) -> bool) -> Option<&'static Flag> {
        self.flags().iter().flat_map(|it| it.iter()).find(|it| found(it))
    }
}

/// How a flag takes its value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Takes {
    Switch,
    /// `-v`, `-vv`.
    Count,
    /// The rest of its word, or the next word.
    Value,
    /// The rest of its word, or the next word unless that is a flag (`num_args(0..=1)`).
    Optional,
}

/// Where a hidden flag that cargo refuses points: clap's error for it is an unknown argument with
/// this tip (cargo's `UnknownArgumentValueParser::suggest_arg` and `suggest`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Points {
    Arg(&'static str),
    Says(&'static str),
}

#[derive(Debug, PartialEq, Eq)]
pub struct Flag {
    long: Option<&'static str>,
    short: Option<char>,
    takes: Takes,
    value: &'static str,
    possible: &'static [&'static str],
    /// Given more than once, its values add up (clap's `ArgAction::Append`); a flag that is not
    /// is refused the second time.
    many: bool,
    /// Its value may start with `-` (`allow_hyphen_values`).
    hyphen: bool,
    /// Out of help and of clap's usage lines.
    hidden: bool,
    points: Option<Points>,
}

impl Flag {
    const fn new(long: &'static str, short: Option<char>, takes: Takes, value: &'static str) -> Flag {
        Flag { long: Some(long), short, takes, value, possible: &[], many: false, hyphen: false, hidden: false, points: None }
    }

    const fn switch(long: &'static str) -> Flag {
        Flag::new(long, None, Takes::Switch, "")
    }

    const fn many(self) -> Flag {
        Flag { many: true, ..self }
    }

    const fn hidden(self) -> Flag {
        Flag { hidden: true, ..self }
    }

    /// A hidden flag cargo has only to point to another.
    const fn pointing(long: Option<&'static str>, short: Option<char>, points: Points) -> Flag {
        Flag { long, short, points: Some(points), hidden: true, ..Flag::switch("") }
    }

    /// As clap shows it (`Arg`'s `Display`): `--features <FEATURES>`, `--bin [<NAME>]`,
    /// `--verbose...`, `-Z <FLAG>`.
    fn shown(&self) -> String {
        self.painted(false)
    }

    /// [`Flag::shown`] in clap's literal and placeholder styles (`Arg::stylized`) when `color`.
    fn painted(&self, color: bool) -> String {
        let paint = |style: &str, text: &str| say::paint(style, text, color);
        let name = match (self.long, self.short) {
            (Some(long), _) => format!("--{long}"),
            (None, Some(short)) => format!("-{short}"),
            (None, None) => String::new(),
        };
        let name = paint(say::VALID, &name);
        let value = || paint(say::PLACEHOLDER, &format!("<{}>", self.value));
        match self.takes {
            Takes::Switch => name,
            Takes::Count => format!("{name}{}", paint(say::PLACEHOLDER, "...")),
            Takes::Value => format!("{name}{}{}", paint(say::PLACEHOLDER, " "), value()),
            Takes::Optional => format!("{name}{}{}{}", paint(say::PLACEHOLDER, " ["), value(), paint(say::PLACEHOLDER, "]")),
        }
    }
}

const COLORS: &[&str] = &["auto", "always", "never"];
const FORMATS: &[&str] = &[
    "human",
    "short",
    "json",
    "json-diagnostic-short",
    "json-diagnostic-rendered-ansi",
    "json-render-diagnostics",
];

const GLOBAL: &[Flag] = &[
    Flag::new("verbose", Some('v'), Takes::Count, ""),
    Flag::new("quiet", Some('q'), Takes::Switch, ""),
    Flag { possible: COLORS, ..Flag::new("color", None, Takes::Value, "WHEN") },
    Flag::pointing(None, Some('c'), Points::Arg("--config")),
    Flag::new("config", None, Takes::Value, "KEY=VALUE|PATH").many(),
    Flag::pointing(None, Some('z'), Points::Arg("-Z")),
    Flag { long: None, short: Some('Z'), ..Flag::new("", None, Takes::Value, "FLAG").many() },
    Flag::switch("frozen"),
    Flag::switch("locked"),
    Flag::switch("offline"),
    Flag::new("help", Some('h'), Takes::Switch, ""),
];
/// cargo's `-C`, before the command only (it is no global of clap's).
const DIRECTORY: Flag = Flag { long: None, short: Some('C'), ..Flag::new("", None, Takes::Value, "DIRECTORY") };
const MESSAGE: &[Flag] = &[Flag { possible: FORMATS, ..Flag::new("message-format", None, Takes::Value, "FMT").many() }];
const SILENT: &[Flag] = &[Flag::pointing(Some("silent"), Some('s'), Points::Arg("--quiet"))];
const PACKAGES: &[Flag] = &[
    Flag::new("package", Some('p'), Takes::Optional, "SPEC").many(),
    Flag::switch("workspace"),
    Flag::new("exclude", None, Takes::Value, "SPEC").many(),
    Flag::pointing(None, Some('x'), Points::Arg("--exclude")),
    Flag::switch("all"),
];
/// `run`'s `-p`, one package (`optional_opt`, not `optional_multi_opt`).
const PACKAGE: &[Flag] = &[Flag::new("package", Some('p'), Takes::Optional, "SPEC")];
const JOBS: &[Flag] = &[Flag { hyphen: true, ..Flag::new("jobs", Some('j'), Takes::Value, "N") }];
const KEEP_GOING: &[Flag] = &[Flag::switch("keep-going")];
/// `test` and `bench`: `arg_unsupported_keep_going`.
const NO_KEEP_GOING: &[Flag] = &[Flag::pointing(
    Some("keep-going"),
    None,
    Points::Says("use `--no-fail-fast` to run as many tests as possible regardless of failure"),
)];
const FUTURE: &[Flag] = &[Flag::switch("future-incompat-report")];
const TARGETS: &[Flag] = &[
    Flag::switch("lib"),
    Flag::switch("bins"),
    Flag::new("bin", None, Takes::Optional, "NAME").many(),
    Flag::switch("examples"),
    Flag::new("example", None, Takes::Optional, "NAME").many(),
    Flag::switch("tests"),
    Flag::new("test", None, Takes::Optional, "NAME").many(),
    Flag::switch("benches"),
    Flag::new("bench", None, Takes::Optional, "NAME").many(),
    Flag::switch("all-targets"),
];
/// `doc`'s `arg_targets_lib_bins_examples`.
const DOC_TARGETS: &[Flag] = &[
    Flag::switch("lib"),
    Flag::switch("bins"),
    Flag::new("bin", None, Takes::Optional, "NAME").many(),
    Flag::switch("examples"),
    Flag::new("example", None, Takes::Optional, "NAME").many(),
];
/// What `doc` makes of the crates it checks: the daemon's load takes them and plans nothing by
/// them; the request carries them ([`blaze_api::Request::Doc`]).
const DOC_ONLY: &[Flag] = &[Flag::switch("open"), Flag::switch("no-deps"), Flag::switch("document-private-items")];
const RUN_TARGETS: &[Flag] = &[
    Flag::new("bin", None, Takes::Optional, "NAME").many(),
    Flag::new("example", None, Takes::Optional, "NAME").many(),
];
const FEATURES: &[Flag] = &[
    Flag::new("features", Some('F'), Takes::Value, "FEATURES").many(),
    Flag::switch("all-features"),
    Flag::switch("no-default-features"),
];
const RELEASE: &[Flag] = &[Flag::new("release", Some('r'), Takes::Switch, "")];
/// `build`'s `arg_redundant_default_mode`.
const DEBUG: &[Flag] = &[Flag::pointing(
    Some("debug"),
    None,
    Points::Says("`--debug` is the default for `cargo build`; instead `--release` is supported"),
)];
const PROFILE: &[Flag] = &[Flag::new("profile", None, Takes::Value, "PROFILE-NAME")];
const TARGET: &[Flag] = &[
    Flag::new("target", None, Takes::Optional, "TRIPLE").many(),
    Flag::pointing(None, Some('t'), Points::Arg("--target")),
    Flag::new("target-dir", None, Takes::Value, "DIRECTORY"),
];
const ARTIFACT: &[Flag] = &[
    Flag::new("artifact-dir", None, Takes::Value, "PATH"),
    Flag::pointing(None, Some('O'), Points::Arg("--artifact-dir")),
    Flag::pointing(Some("out-dir"), None, Points::Arg("--artifact-dir")),
];
const REPORTS: &[Flag] = &[Flag::switch("unit-graph"), Flag::switch("timings"), Flag::switch("ignore-rust-version")];
const COMPILE_TIME_DEPS: &[Flag] = &[Flag::switch("compile-time-deps").hidden()];
const MANIFEST: &[Flag] = &[
    Flag::new("manifest-path", Some('m'), Takes::Value, "PATH"),
    Flag::pointing(Some("path"), None, Points::Arg("--manifest-path")),
];
/// `--reached` and `--ub*` are blaze's (extensions, opt-in): only the tests an edit reached run;
/// the tests run in blaze's MIR evaluator, checked for undefined behavior (`hir-ty-ub`).
const TEST_ONLY: &[Flag] = &[
    Flag::switch("doc"),
    Flag::switch("reached"),
    Flag::switch("ub"),
    Flag::new("ub-seed", None, Takes::Value, "N"),
    Flag::new("ub-schedules", None, Takes::Value, "K"),
];

/// `--refused N|all` is blaze's: how many of the functions blaze could not compile a build's
/// error names, the first `N` or every one ([`blaze_api::Cargo::refused`]).
const REFUSED: &[Flag] = &[Flag::new("refused", None, Takes::Value, "N|all")];

/// `check`'s edits, blaze's: `--fix` makes every machine-applicable suggestion of the check
/// ([`super::fix`]), only those of each `--lint NAME` when any is given; `--replace FROM=TO`
/// retargets every reference to the item `FROM` to `TO` by name resolution
/// ([`super::replace`]), and checks nothing.
const CHECK_ONLY: &[Flag] = &[
    Flag::switch("fix"),
    Flag::new("lint", None, Takes::Value, "NAME").many(),
    Flag::new("replace", None, Takes::Value, "FROM=TO").many(),
];

/// `check`'s baselines, blaze's (`blaze_lints::baseline`): `--baselines DIR` checks the run's
/// counts against the baselines in `DIR`, `--bless-baselines` lowers them to the run's, and
/// `--new-baseline LINT[,LINT...]`, given any number of times, starts each `LINT`'s at the run's.
const BASELINES: &[Flag] = &[
    Flag::new("baselines", None, Takes::Value, "DIR"),
    Flag::switch("bless-baselines"),
    Flag::new("new-baseline", None, Takes::Value, "LINT").many(),
];

/// The flags read here and never handed to the daemon's load: blaze's own, which the client
/// does.
fn blaze_only(flag: &Flag) -> bool {
    matches!(
        flag.long,
        Some(
            "reached"
                | "ub"
                | "ub-seed"
                | "ub-schedules"
                | "refused"
                | "baselines"
                | "bless-baselines"
                | "new-baseline"
                | "fix"
                | "lint"
                | "replace"
        )
    )
}
const RUNNING: &[Flag] = &[Flag::switch("no-run"), Flag::switch("no-fail-fast")];

/// The flags clap's `conflicts_with` keeps apart (`arg_release`).
fn conflicts(a: &Flag, b: &Flag) -> bool {
    matches!((a.long, b.long), (Some("release"), Some("profile")) | (Some("profile"), Some("release")))
}

/// What blaze was asked.
#[derive(Debug, PartialEq)]
pub enum Asked {
    Cargo(Box<Invocation>),
    /// `blaze stop`: the daemon made durable and ended.
    Stop,
    /// `blaze status`: what the daemon is doing.
    Status,
    /// `blaze toolchain`: blaze's Rust, downloaded once unless rustup has it.
    Toolchain,
    /// `blaze daemon install` or `blaze daemon uninstall`: the units that socket-activate it.
    Daemon(DaemonVerb),
    /// `blaze codegen-inputs`, `blaze trace` or `blaze tables`: what the daemon's last build of
    /// the checkout did; or `blaze pack`: a program packed for the in-browser compiler. With the
    /// words after the command, which [`super::inspect`] reads.
    Inspect(super::inspect::Inspected, Vec<OsString>),
    /// `blaze explore`, with the words after it ([`super::observe::explore`]).
    Explore(Vec<OsString>),
    /// `blaze debug profile|live|sql`, with the words after `debug` ([`super::observe::debug`]).
    Debug(Vec<OsString>),
    /// `blaze explain VIEW ...`, with the words after it ([`super::observe::explain`]).
    Explain(Vec<OsString>),
    Help(Option<Command>),
    Version,
    /// A command that is none of blaze's.
    Unknown(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DaemonVerb {
    Install,
    Uninstall,
}

/// One of cargo's build commands, read.
#[derive(Debug, PartialEq, Default)]
pub struct Invocation {
    pub command: Option<Command>,
    /// The words before `--`, as typed, flags typed before the command first: cargo's to read.
    /// None of `run`'s program arguments, nor blaze's own `--reached`, `--ub*` and `--refused`,
    /// nor `-C`.
    pub args: Vec<String>,
    /// `test`'s and `bench`'s `TESTNAME`.
    pub filter: Option<String>,
    /// `run`'s arguments of the program; what follows `--` for `test` and `bench`.
    pub program: Vec<OsString>,
    pub flags: Flags,
    /// `-h`/`--help` came before anything clap refuses: the command's help, and nothing else.
    pub help: bool,
    /// `-C`'s directory, which the command changes to before anything else.
    pub directory: Option<OsString>,
    /// `-Z unstable-options` came before the command, where cargo looks for it to allow `-C`.
    pub unstable_options: bool,
}

impl Invocation {
    pub fn command(&self) -> Command {
        self.command.unwrap_or(Command::Build)
    }

    /// What each test executable is run with: the filter, then what follows `--`, then for
    /// `bench` libtest's `--bench`, as cargo's `ops::run_tests` and `run_benches` pass them.
    pub fn test_args(&self) -> Vec<OsString> {
        let mut args: Vec<OsString> = self.filter.iter().map(OsString::from).collect();
        args.extend(self.program.iter().cloned());
        if self.command == Some(Command::Bench) {
            args.push("--bench".into());
        }
        args
    }

    /// The profile cargo's `Finished` line and the daemon's `Targets` name.
    pub fn profile(&self) -> String {
        match (&self.flags.profile, self.flags.release) {
            (Some(profile), _) => profile.clone(),
            (None, true) => "release".to_owned(),
            (None, false) => self.command().profile().to_owned(),
        }
    }
}

/// What cargo's command refuses of a command line clap took, before it builds anything:
/// `commands/test.rs`'s `--doc` with target selection or `--no-run`, and `run`'s glob
/// patterns (`commands/run.rs` for packages, `ops::run` for targets, as `is_glob_pattern` sees
/// one). cargo's code for each is 101.
pub fn refused(asked: &Invocation) -> Option<String> {
    let flags = &asked.flags;
    let glob = |it: &str| it.contains(['*', '?', '[', ']']);
    match asked.command() {
        Command::Test if flags.doc && flags.targeted => Some("can't mix --doc with other target selecting options".to_owned()),
        Command::Test if flags.doc && flags.no_run => Some("can't skip running doc tests with --no-run".to_owned()),
        Command::Run => {
            if let Some(pattern) = flags.packages.iter().find(|it| glob(it)) {
                return Some(format!("`cargo run` does not support glob pattern `{pattern}` on package selection"));
            }
            let mut targets = flags.bins.iter().chain(&flags.examples).flatten();
            targets.any(|it| glob(it)).then(|| "`cargo run` does not support glob patterns on target selection".to_owned())
        }
        // What `cargo doc` would write beside the pages, which `blaze doc` does not yet.
        Command::Doc => {
            if let Some(flag) = flags.reports.first() {
                return Some(format!("`blaze doc` does not write what `--{flag}` asks for yet"));
            }
            let mut formats = flags.message_format.iter().flat_map(|it| it.split(','));
            formats.find(|it| it.to_ascii_lowercase().starts_with("json")).map(|it| {
                format!("`blaze doc` writes no JSON messages for the pages yet (`--message-format {it}`)")
            })
        }
        Command::Check if !flags.replace.is_empty() && (flags.fix || flags.baselines.is_some()) => {
            Some("`--replace` makes its edits and checks nothing: run `--fix` and `--baselines` apart".to_owned())
        }
        Command::Check if flags.fix && flags.baselines.is_some() => {
            Some("`--fix` changes the findings `--baselines` counts: run them apart".to_owned())
        }
        Command::Check if !flags.fix && !flags.lints.is_empty() => {
            Some("`--lint` narrows the suggestions `--fix` makes: give `--fix` too".to_owned())
        }
        Command::Check if flags.baselines.is_none() && (flags.bless_baselines || !flags.new_baselines.is_empty()) => {
            Some("`--bless-baselines` and `--new-baseline` write the baselines `--baselines DIR` names".to_owned())
        }
        _ => None,
    }
}

/// The flags the client reads itself, or puts in `Targets`.
#[derive(Debug, PartialEq, Default)]
pub struct Flags {
    pub manifest_path: Option<String>,
    /// `-p` with a value; `-p` alone is cargo's to answer, with the packages it knows.
    pub packages: Vec<String>,
    pub workspace: bool,
    pub excludes: Vec<String>,
    pub features: Vec<String>,
    pub all_features: bool,
    pub no_default_features: bool,
    pub release: bool,
    pub profile: Option<String>,
    pub targets: Vec<String>,
    /// Each `--bin`, with its name when it has one.
    pub bins: Vec<Option<String>>,
    pub examples: Vec<Option<String>>,
    /// Any of the target selection flags (cargo's `CompileFilter::is_specific`).
    pub targeted: bool,
    pub message_format: Vec<String>,
    pub color: Option<String>,
    pub config: Vec<String>,
    pub verbose: u32,
    pub quiet: bool,
    pub doc: bool,
    /// `doc`'s `--open`, `--no-deps` and `--document-private-items`.
    pub open: bool,
    pub no_deps: bool,
    pub private: bool,
    pub no_run: bool,
    pub no_fail_fast: bool,
    pub reached: bool,
    /// `--ub`, or either of its values, which imply it.
    pub ub: bool,
    pub ub_seed: Option<u64>,
    pub ub_schedules: Option<u32>,
    /// `--refused`: `None` without it, `Some(None)` for `all`, else `Some(Some(N))`.
    pub refused: Option<Option<u32>>,
    /// `--unit-graph` and `--timings`, by name, as given.
    pub reports: Vec<&'static str>,
    /// `check`'s `--fix`, and each `--lint` it is narrowed to.
    pub fix: bool,
    pub lints: Vec<String>,
    /// `check`'s `--replace`, each `FROM=TO` as given.
    pub replace: Vec<String>,
    /// `check`'s `--baselines`, `--bless-baselines` and each `--new-baseline`.
    pub baselines: Option<String>,
    pub bless_baselines: bool,
    pub new_baselines: Vec<String>,
}

impl Flags {
    fn read(&mut self, flag: &Flag, value: Option<&str>) {
        let owned = || value.map(str::to_owned);
        match flag.long.unwrap_or("") {
            "manifest-path" => self.manifest_path = owned(),
            "package" => self.packages.extend(owned()),
            "workspace" | "all" => self.workspace = true,
            "exclude" => self.excludes.extend(owned()),
            "features" => {
                let features = value.unwrap_or_default().split([',', ' ']);
                self.features.extend(features.filter(|it| !it.is_empty()).map(str::to_owned));
            }
            "all-features" => self.all_features = true,
            "no-default-features" => self.no_default_features = true,
            "release" => self.release = true,
            "profile" => self.profile = owned(),
            "target" => self.targets.extend(owned()),
            "bin" => {
                self.targeted = true;
                self.bins.push(owned());
            }
            "example" => {
                self.targeted = true;
                self.examples.push(owned());
            }
            "lib" | "bins" | "examples" | "test" | "tests" | "bench" | "benches" | "all-targets" => self.targeted = true,
            "message-format" => self.message_format.extend(owned()),
            "color" => self.color = owned(),
            "config" => self.config.extend(owned()),
            "verbose" => self.verbose += 1,
            "quiet" => self.quiet = true,
            "doc" => self.doc = true,
            "open" => self.open = true,
            "no-deps" => self.no_deps = true,
            "document-private-items" => self.private = true,
            "no-run" => self.no_run = true,
            "no-fail-fast" => self.no_fail_fast = true,
            "reached" => self.reached = true,
            "ub" => self.ub = true,
            name @ ("unit-graph" | "timings") => self.reports.push(name),
            // `check_value` parsed them first.
            "ub-seed" => {
                self.ub = true;
                self.ub_seed = value.and_then(|it| it.parse().ok());
            }
            "ub-schedules" => {
                self.ub = true;
                self.ub_schedules = value.and_then(|it| it.parse().ok());
            }
            "refused" => self.refused = value.map(|it| it.parse().ok()),
            "fix" => self.fix = true,
            "lint" => self.lints.extend(owned()),
            "replace" => self.replace.extend(owned()),
            "baselines" => self.baselines = owned(),
            "bless-baselines" => self.bless_baselines = true,
            "new-baseline" => {
                let lints = value.unwrap_or_default().split(',');
                self.new_baselines.extend(lints.filter(|it| !it.is_empty()).map(str::to_owned));
            }
            _ => {}
        }
    }
}

/// A command line clap refuses, with what it says.
#[derive(Debug, PartialEq, Eq)]
pub struct Usage {
    /// `None` for the words before any command (cargo's own `cli`).
    pub command: Option<Command>,
    pub said: Said,
    /// What clap's usage line of the error names, as it had them when it refused.
    pub used: Vec<Used>,
}

/// An argument the command line had when clap refused it: what its "smart usage" lists.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Used {
    Flag(&'static Flag),
    /// `TESTNAME` or `BENCHNAME`.
    Filter,
    /// The arguments of `run`'s program, or after `--`.
    Program,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Said {
    /// No flag of the command, with clap's did-you-mean, and whether it says to pass it after `--`.
    Unknown { arg: String, similar: Option<String>, trailing: bool },
    /// A hidden flag cargo has only to point elsewhere.
    Points { flag: &'static Flag },
    NoValue { flag: &'static Flag },
    BadValue { value: String, flag: &'static Flag },
    ExtraValue { value: String, flag: &'static Flag },
    /// A number clap's `value_parser` refuses, with why (`invalid digit found in string`).
    NotNumber { value: String, flag: &'static Flag, why: String },
    /// A flag that takes one value, or none, given again.
    Twice { flag: &'static Flag },
    /// `flag`, then `with`, which clap's `conflicts_with` keeps apart.
    Conflict { flag: &'static Flag, with: &'static Flag },
    NotUtf8,
}

/// The arguments after `blaze` (or after the `blaze` cargo passes `cargo-blaze`), read.
pub fn parse(words: Vec<OsString>) -> Result<Asked, Usage> {
    let mut globals = Vec::new();
    let mut directory = None;
    let mut unstable_options = false;
    let mut words = words.into_iter();
    while let Some(word) = words.next() {
        let text = word.to_str().map(str::to_owned);
        match text.as_deref() {
            Some("--help" | "-h" | "help") => {
                let command = words.next().and_then(|it| it.to_str().and_then(Command::named));
                return Ok(Asked::Help(command));
            }
            Some("--version" | "-V") => return Ok(Asked::Version),
            Some("stop") => return Ok(Asked::Stop),
            Some("status") => return Ok(Asked::Status),
            Some("toolchain") => return Ok(Asked::Toolchain),
            Some("explore") => return Ok(Asked::Explore(words.collect())),
            Some("debug") => return Ok(Asked::Debug(words.collect())),
            Some("explain") => return Ok(Asked::Explain(words.collect())),
            Some("daemon") => {
                return Ok(match words.next().as_deref().and_then(std::ffi::OsStr::to_str) {
                    Some("install") => Asked::Daemon(DaemonVerb::Install),
                    Some("uninstall") => Asked::Daemon(DaemonVerb::Uninstall),
                    Some(other) => Asked::Unknown(format!("daemon {other}")),
                    None => Asked::Unknown("daemon".to_owned()),
                });
            }
            Some(other) if other.starts_with("-C") => {
                let value = match other[2..].strip_prefix('=').unwrap_or(&other[2..]) {
                    "" => words.next().filter(|it| it.to_str().is_none_or(|it| !it.starts_with('-'))),
                    attached => Some(OsString::from(attached)),
                };
                let Some(value) = value else {
                    return Err(Usage { command: None, said: Said::NoValue { flag: &DIRECTORY }, used: Vec::new() });
                };
                directory = Some(value);
            }
            Some(other) if other.starts_with('-') && other.len() > 1 => {
                // A global flag before the command, and its value when that is the next word.
                let attached = match other.strip_prefix("--") {
                    Some(long) => long.contains('='),
                    None => other.len() > 2,
                };
                let takes_next = !attached && global(other).is_some_and(|it| it.takes == Takes::Value);
                let z = other.strip_prefix("-Z").map(|it| it.strip_prefix('=').unwrap_or(it));
                unstable_options |= z == Some("unstable-options");
                globals.push(word);
                if takes_next && let Some(value) = words.next() {
                    unstable_options |= z == Some("") && value == "unstable-options";
                    globals.push(value);
                }
            }
            Some(name) => match Command::named(name) {
                Some(command) => {
                    let before = globals.len();
                    globals.extend(words);
                    return command_line_after(command, globals, before).map(|it| match it.help {
                        true => Asked::Help(Some(command)),
                        false => Asked::Cargo(Box::new(Invocation { directory, unstable_options, ..it })),
                    });
                }
                None => match super::inspect::Inspected::named(name) {
                    Some(what) => {
                        globals.extend(words);
                        return Ok(Asked::Inspect(what, globals));
                    }
                    None => return Ok(Asked::Unknown(name.to_owned())),
                },
            },
            None => return Ok(Asked::Unknown(word.to_string_lossy().into_owned())),
        }
    }
    Ok(Asked::Help(None))
}

fn global(word: &str) -> Option<&'static Flag> {
    let long = word.strip_prefix("--").map(|it| it.split('=').next().unwrap_or(it));
    let short = word.strip_prefix('-').and_then(|it| it.chars().next());
    GLOBAL.iter().find(|flag| match long {
        Some(long) => flag.long == Some(long),
        None => flag.short == short,
    })
}

/// The words after a build command, as clap reads them for it.
pub fn command_line(command: Command, words: Vec<OsString>) -> Result<Invocation, Usage> {
    command_line_after(command, words, 0)
}

/// [`command_line`] of `words` whose first `before` came before the command: cargo's globals,
/// which clap reads into the parent command, so that they neither repeat nor show in the
/// command's usage lines.
fn command_line_after(command: Command, words: Vec<OsString>, before: usize) -> Result<Invocation, Usage> {
    let mut asked = Invocation { command: Some(command), ..Invocation::default() };
    let mut used: Vec<Used> = Vec::new();
    let mut seen: Vec<&'static Flag> = Vec::new();
    let refused = |said, used: &[Used]| Usage { command: Some(command), said, used: used.to_vec() };
    let mut words = words.into_iter().enumerate().peekable();
    let mut ended = false;
    while let Some((at, word)) = words.next() {
        let parents = at < before;
        if word == "--" {
            ended = true;
            break;
        }
        let Some(text) = word.to_str() else {
            match command {
                Command::Run => {
                    asked.program.push(word);
                    run_program(&mut asked, words.by_ref().map(|(_, it)| it));
                    break;
                }
                _ => return Err(refused(Said::NotUtf8, &[])),
            }
        };
        let text = text.to_owned();
        if !text.starts_with('-') || text == "-" {
            match command {
                Command::Run => {
                    asked.program.push(word);
                    run_program(&mut asked, words.by_ref().map(|(_, it)| it));
                    break;
                }
                Command::Test | Command::Bench if asked.filter.is_none() => {
                    used.push(Used::Filter);
                    asked.args.push(text.clone());
                    asked.filter = Some(text);
                    continue;
                }
                _ => return Err(refused(Said::Unknown { arg: text, similar: None, trailing: false }, &[])),
            }
        }
        let mut typed = vec![text.clone()];
        let mut flags: Vec<(&'static Flag, Option<String>)> = Vec::new();
        if let Some(long) = text.strip_prefix("--") {
            let (name, attached) = match long.split_once('=') {
                Some((name, value)) => (name, Some(value.to_owned())),
                None => (long, None),
            };
            let Some(flag) = command.flag(|it| it.long == Some(name)) else {
                let similar = similar(command, name);
                if let Some(flag) = similar.and_then(|it| command.flag(|flag| flag.long == Some(it)))
                    && !flag.hidden
                    && !used.contains(&Used::Flag(flag))
                {
                    used.push(Used::Flag(flag));
                }
                let said = Said::Unknown {
                    arg: format!("--{name}"),
                    similar: similar.map(|it| format!("--{it}")),
                    trailing: command.has_positionals(),
                };
                return Err(refused(said, &used));
            };
            if flag.points.is_some() {
                return Err(refused(Said::Points { flag }, &[]));
            }
            let value = match (flag.takes, attached) {
                (Takes::Switch | Takes::Count, Some(value)) => {
                    return Err(refused(Said::ExtraValue { value, flag }, &[]));
                }
                (Takes::Switch | Takes::Count, None) => None,
                (Takes::Value | Takes::Optional, Some(value)) => Some(value),
                (Takes::Value, None) => match next_value(&mut words, flag.hyphen) {
                    Some(value) => {
                        typed.push(value.clone());
                        Some(value)
                    }
                    None => return Err(refused(Said::NoValue { flag }, &[])),
                },
                (Takes::Optional, None) => {
                    let value = next_value(&mut words, false);
                    typed.extend(value.clone());
                    value
                }
            };
            if flag.long == Some("help") {
                return Ok(Invocation { command: Some(command), help: true, ..Invocation::default() });
            }
            check_value(flag, value.as_deref()).map_err(|said| refused(said, &[]))?;
            flags.push((flag, value));
        } else {
            // A cluster of short flags: `-rq`, `-vv`, `-j8`, `-pfoo`, `-p=foo`.
            let shorts: Vec<char> = text[1..].chars().collect();
            let mut at = 0;
            while at < shorts.len() {
                let c = shorts[at];
                let Some(flag) = command.flag(|it| it.short == Some(c)) else {
                    let said = Said::Unknown { arg: format!("-{c}"), similar: None, trailing: command.has_positionals() };
                    return Err(refused(said, &[]));
                };
                if flag.points.is_some() {
                    return Err(refused(Said::Points { flag }, &[]));
                }
                at += 1;
                if flag.long == Some("help") {
                    return Ok(Invocation { command: Some(command), help: true, ..Invocation::default() });
                }
                let rest: String = shorts[at..].iter().collect();
                let rest = rest.strip_prefix('=').unwrap_or(&rest).to_owned();
                let value = match flag.takes {
                    Takes::Switch | Takes::Count => None,
                    Takes::Value | Takes::Optional if !rest.is_empty() => {
                        at = shorts.len();
                        Some(rest)
                    }
                    Takes::Value => match next_value(&mut words, flag.hyphen) {
                        Some(value) => {
                            typed.push(value.clone());
                            Some(value)
                        }
                        None => return Err(refused(Said::NoValue { flag }, &[])),
                    },
                    Takes::Optional => {
                        let value = next_value(&mut words, false);
                        typed.extend(value.clone());
                        value
                    }
                };
                check_value(flag, value.as_deref()).map_err(|said| refused(said, &[]))?;
                flags.push((flag, value));
            }
        }
        for (flag, value) in flags {
            if !parents {
                let again = seen.iter().any(|it| std::ptr::eq(*it, flag));
                if again && !flag.many && flag.takes != Takes::Count {
                    return Err(refused(Said::Twice { flag }, &[]));
                }
                if !again {
                    seen.push(flag);
                    if !flag.hidden {
                        used.push(Used::Flag(flag));
                    }
                }
            }
            asked.flags.read(flag, value.as_deref());
            if blaze_only(flag) {
                typed.clear();
            }
        }
        asked.args.extend(typed);
    }
    if ended {
        let rest: Vec<OsString> = words.map(|(_, it)| it).collect();
        match command {
            Command::Build | Command::Check | Command::Doc => {
                if let Some(first) = rest.first() {
                    let arg = first.to_string_lossy().into_owned();
                    return Err(refused(Said::Unknown { arg, similar: None, trailing: false }, &[]));
                }
            }
            _ => {
                used.push(Used::Program);
                asked.program.extend(rest);
            }
        }
    }
    if asked.command == Some(Command::Run) && !asked.program.is_empty() && !used.contains(&Used::Program) {
        used.push(Used::Program);
    }
    // clap's validator, once every word is read: the first flag given of a pair it keeps apart.
    for (at, &flag) in seen.iter().enumerate() {
        if let Some(&with) = seen.iter().skip(at + 1).chain(&seen[..at]).find(|it| conflicts(flag, it)) {
            let used: Vec<Used> = used.iter().copied().filter(|it| *it != Used::Flag(with)).collect();
            return Err(refused(Said::Conflict { flag, with }, &used));
        }
    }
    Ok(asked)
}

/// The next word as a flag's value, unless it is a flag itself and the flag takes no such value.
fn next_value(words: &mut std::iter::Peekable<impl Iterator<Item = (usize, OsString)>>, hyphen: bool) -> Option<String> {
    let next = words.peek()?.1.to_str()?;
    if !hyphen && next.starts_with('-') && next != "-" {
        return None;
    }
    words.next()?.1.into_string().ok()
}

/// `run`'s program has the rest: once its first argument comes, clap reads no more flags
/// (`trailing_var_arg`), and the first `--` among them is dropped.
fn run_program(asked: &mut Invocation, words: impl Iterator<Item = OsString>) {
    let mut dropped = false;
    for word in words {
        if word == "--" && !dropped {
            dropped = true;
            continue;
        }
        asked.program.push(word);
    }
}

fn check_value(flag: &'static Flag, value: Option<&str>) -> Result<(), Said> {
    let Some(value) = value else { return Ok(()) };
    let not_number = |why: String| Said::NotNumber { value: value.to_owned(), flag, why };
    match flag.long {
        Some("ub-seed") => return value.parse::<u64>().map(drop).map_err(|it| not_number(it.to_string())),
        Some("refused") if value == "all" => return Ok(()),
        Some("refused") => {
            return value.parse::<u32>().map(drop).map_err(|it| not_number(format!("{it}: a count, or `all`")));
        }
        // `value_parser!(u32).range(1..)`.
        Some("ub-schedules") => {
            return match value.parse::<u32>() {
                Ok(0) => Err(not_number(format!("0 is not in 1..={}", u32::MAX))),
                Ok(_) => Ok(()),
                Err(error) => Err(not_number(error.to_string())),
            };
        }
        _ => {}
    }
    if flag.possible.is_empty() {
        return Ok(());
    }
    let parts: Vec<&str> = match flag.long {
        // `value_delimiter(',')`.
        Some("message-format") => value.split(',').collect(),
        _ => vec![value],
    };
    // Both of cargo's flags with possible values are `ignore_case(true)`.
    for part in parts {
        if !flag.possible.iter().any(|it| it.eq_ignore_ascii_case(part)) {
            return Err(Said::BadValue { value: part.to_owned(), flag });
        }
    }
    Ok(())
}

/// clap's did-you-mean of a long flag: the command's flag most like `typed`, of a Jaro
/// similarity over 0.7, hidden ones too (`clap_builder::parser::features::suggestions`).
fn similar(command: Command, typed: &str) -> Option<&'static str> {
    let longs = command.flags().iter().flat_map(|it| it.iter()).filter_map(|it| it.long);
    best(typed, longs)
}

fn best<'a>(typed: &str, among: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    let mut best: Option<(f64, &str)> = None;
    for candidate in among {
        let confidence = jaro(typed, candidate);
        if confidence > 0.7 && best.is_none_or(|(it, _)| confidence > it) {
            best = Some((confidence, candidate));
        }
    }
    best.map(|(_, it)| it)
}

/// strsim's `jaro`.
pub fn jaro(a: &str, b: &str) -> f64 {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    if a.len() == 1 && b.len() == 1 {
        return if a[0] == b[0] { 1.0 } else { 0.0 };
    }
    let window = (a.len().max(b.len()) / 2).saturating_sub(1);
    let mut b_used = vec![false; b.len()];
    let mut matches = 0usize;
    let mut transpositions = 0usize;
    let mut b_at = 0usize;
    for (i, &ca) in a.iter().enumerate() {
        let start = i.saturating_sub(window);
        let end = (i + window + 1).min(b.len());
        for j in start..end {
            if !b_used[j] && b[j] == ca {
                b_used[j] = true;
                matches += 1;
                if j < b_at {
                    transpositions += 1;
                }
                b_at = j;
                break;
            }
        }
    }
    if matches == 0 {
        return 0.0;
    }
    let m = matches as f64;
    (m / a.len() as f64 + m / b.len() as f64 + (m - transpositions as f64) / m) / 3.0
}

/// clap's usage line of an error, after `Usage: `: with nothing `used`, the command and its
/// placeholders (`write_help_usage`); else the command, each flag used once in the order it came,
/// and the positionals (`write_smart_usage`). cargo's own, before any command, is its
/// `override_usage`. In cargo's styles when `color`.
fn usage_line(command: Option<Command>, used: &[Used], color: bool) -> String {
    let paint = |style: &str, text: &str| say::paint(style, text, color);
    let mut line = format!("{} ", paint(say::GOOD, "Usage:"));
    let Some(command) = command else {
        line.push_str(&format!(
            "{} {}\n       {} {} {} {}",
            paint(say::VALID, "cargo"),
            paint(say::PLACEHOLDER, "[OPTIONS] [COMMAND]"),
            paint(say::VALID, "cargo"),
            paint(say::PLACEHOLDER, "[OPTIONS]"),
            paint(say::VALID, "-Zscript"),
            paint(say::PLACEHOLDER, "<MANIFEST_RS> [ARGS]..."),
        ));
        return line;
    };
    let mut pieces = vec![paint(say::VALID, command.usage())];
    match used.is_empty() {
        true => pieces.push(paint(say::PLACEHOLDER, "[OPTIONS]")),
        false => pieces.extend(used.iter().filter_map(|it| match it {
            Used::Flag(flag) => Some(flag.painted(color)),
            Used::Filter | Used::Program => None,
        })),
    }
    let filter = used.contains(&Used::Filter);
    let program = used.contains(&Used::Program);
    match command {
        Command::Build | Command::Check | Command::Doc => {}
        Command::Run => pieces.push(paint(say::PLACEHOLDER, "[ARGS]...")),
        Command::Test | Command::Bench => {
            let name = if command == Command::Test { "TESTNAME" } else { "BENCHNAME" };
            match filter {
                true => pieces.push(paint(say::PLACEHOLDER, &format!("<{name}>"))),
                false => pieces.push(paint(say::PLACEHOLDER, &format!("[{name}]"))),
            }
            let args = paint(say::PLACEHOLDER, "[ARGS]...");
            match program {
                true => pieces.push(format!("{} {args}", paint(say::VALID, "--"))),
                false => pieces.push(format!("{} {args}{}", paint(say::VALID, "[--"), paint(say::VALID, "]"))),
            }
        }
    }
    line.push_str(&pieces.join(" "));
    line
}

/// clap's text for a refused command line, in cargo's styles when `color`; clap's exit code
/// for it in cargo is 1. Its layout is clap's `format_error_message`: the error, a blank line
/// and each tip, a blank line and the usage, and `For more information`.
pub fn refusal(usage: &Usage, color: bool) -> String {
    let paint = |style: &str, text: &str| say::paint(style, text, color);
    let error = paint(say::ERROR, "error:");
    let more = format!("\n\nFor more information, try '{}'.\n", paint(say::VALID, "--help"));
    let usage_line = |used: &[Used]| format!("\n\n{}", usage_line(usage.command, used, color));
    let possible = |flag: &Flag| {
        let values: Vec<String> = flag.possible.iter().map(|it| paint(say::VALID, it)).collect();
        format!("\n  [possible values: {}]", values.join(", "))
    };
    let tip = paint(say::VALID, "tip:");
    let similar = |kind: &str, it: &str| format!("\n\n  {tip} a similar {kind} exists: '{}'", paint(say::VALID, it));
    match &usage.said {
        Said::Unknown { arg, similar: like, trailing } => {
            let mut said = format!("{error} unexpected argument '{}' found", paint(say::WARN, arg));
            if let Some(like) = like {
                said += &similar("argument", like);
            }
            if *trailing {
                if like.is_none() {
                    said.push('\n');
                }
                let trailing = paint(say::VALID, &format!("-- {arg}"));
                said += &format!("\n  {tip} to pass '{}' as a value, use '{trailing}'", paint(say::WARN, arg));
            }
            format!("{said}{}{more}", usage_line(&usage.used))
        }
        Said::Points { flag } => {
            let mut said = format!("{error} unexpected argument '{}' found", paint(say::WARN, &flag.shown()));
            match flag.points {
                Some(Points::Arg(to)) => said += &similar("argument", to),
                Some(Points::Says(what)) => said += &format!("\n\n  {tip} {what}"),
                None => {}
            }
            format!("{said}{}{more}", usage_line(&[]))
        }
        Said::NoValue { flag } => {
            let mut said = format!(
                "{error} a value is required for '{}' but none was supplied",
                paint(say::WARN, &flag.shown())
            );
            if !flag.possible.is_empty() {
                said += &possible(flag);
            }
            format!("{said}{more}")
        }
        Said::BadValue { value, flag } => {
            let mut said = format!(
                "{error} invalid value '{}' for '{}'",
                paint(say::WARN, value),
                paint(say::VALID, &flag.shown())
            );
            said += &possible(flag);
            if let Some(like) = best(value, flag.possible.iter().copied()) {
                said += &similar("value", like);
            }
            format!("{said}{more}")
        }
        Said::ExtraValue { value, flag } => format!(
            "{error} unexpected value '{}' for '{}' found; no more were expected{}{more}",
            paint(say::WARN, value),
            paint(say::VALID, &flag.shown()),
            usage_line(&[])
        ),
        Said::NotNumber { value, flag, why } => format!(
            "{error} invalid value '{}' for '{}': {why}{more}",
            paint(say::WARN, value),
            paint(say::VALID, &flag.shown())
        ),
        Said::Twice { flag } => format!(
            "{error} the argument '{}' cannot be used multiple times{}{more}",
            paint(say::WARN, &flag.shown()),
            usage_line(&[])
        ),
        Said::Conflict { flag, with } => format!(
            "{error} the argument '{}' cannot be used with '{}'{}{more}",
            paint(say::WARN, &flag.shown()),
            paint(say::WARN, &with.shown()),
            usage_line(&usage.used)
        ),
        Said::NotUtf8 => format!("{error} invalid UTF-8 was detected in one or more arguments{}{more}", usage_line(&[])),
    }
}

/// `blaze <command> --help`: the command's flags as this client reads them.
pub fn help(command: Command) -> String {
    let usage = usage_line(Some(command), &[], false);
    let mut said = format!("{}: cargo's `{}`, built by blaze's daemon\n\n{usage}\n\nOptions:\n", command.name(), command.name());
    for flag in command.flags().iter().flat_map(|it| it.iter()).filter(|it| !it.hidden) {
        let line = match (flag.short, flag.long) {
            (Some(short), Some(_)) => format!("  -{short}, {}\n", flag.shown()),
            _ if flag.long.is_some() => format!("      {}\n", flag.shown()),
            _ => format!("  {}\n", flag.shown()),
        };
        said.push_str(&line);
    }
    said
}

#[cfg(test)]
mod tests;
