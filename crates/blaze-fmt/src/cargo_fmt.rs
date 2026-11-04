//! `blaze fmt`: rustfmt's `src/cargo-fmt/main.rs` at rust-lang/rust 12433c307 (MIT or
//! Apache-2.0). The workspace comes from `blaze-manifest` instead of `cargo metadata`, the
//! arguments are read by hand instead of by clap, and rustfmt runs in this process through
//! [`crate::cli::main`] instead of as a child.

// Inspired by Paul Woolcock's cargo-fmt (https://github.com/pwoolcoc/cargo-fmt/).

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use blaze_manifest::features::Edition;
use blaze_manifest::metadata::root_manifest;
use blaze_manifest::targets::TargetKind;
use blaze_manifest::{Config, Context, Workspace};

const fn is_nightly() -> bool {
    match option_env!("CFG_RELEASE_CHANNEL") {
        None => true,
        Some(c) => matches!(c.as_bytes(), b"nightly" | b"dev"),
    }
}

const MESSAGE_FORMATS: &str = if is_nightly() {
    "short|json|human"
} else {
    "short|human"
};

#[derive(Default)]
pub struct Opts {
    /// No output printed to stdout
    quiet: bool,

    /// Use verbose output
    verbose: bool,

    /// Print rustfmt version and exit
    version: bool,

    /// Specify package to format
    packages: Vec<String>,

    /// Specify path to Cargo.toml
    manifest_path: Option<String>,

    message_format: Option<String>,

    /// Options passed to rustfmt
    rustfmt_options: Vec<String>,

    /// Format all packages, and also their local path-based dependencies
    format_all: bool,

    /// Run rustfmt in check mode
    check: bool,
}

fn help() -> String {
    format!(
        "Formats every target of the current package (or, at a workspace's root, of every member)
with blaze's formatter: no rustfmt or cargo-fmt is started. Each target is formatted with its
edition, under the nearest rustfmt.toml or .rustfmt.toml.

Usage: blaze fmt [OPTIONS] [-- <rustfmt_options>...]
       blaze fmt [rustfmt_options] <FILE.rs>...

Arguments:
  [rustfmt_options]...  Options passed to rustfmt

Options:
  -q, --quiet
          No output printed to stdout
  -v, --verbose
          Use verbose output
      --version
          Print rustfmt version and exit
  -p, --package <package>...
          Specify package to format
      --manifest-path <manifest-path>
          Specify path to Cargo.toml
      --message-format <message-format>
          Specify message-format: {MESSAGE_FORMATS}
      --all
          Format all packages, and also their local path-based dependencies
      --check
          Run rustfmt in check mode
  -h, --help
          Print help"
    )
}

/// What clap makes of cargo-fmt's arguments: the options, help asked for, or a usage error.
enum Parsed {
    Opts(Opts),
    Help,
    Error(String),
}

fn parse_opts(args: Vec<String>) -> Parsed {
    let mut opts = Opts::default();
    let mut args = args.into_iter().peekable();
    let is_flag = |a: &str| a.starts_with('-') && a.len() > 1;
    while let Some(arg) = args.next() {
        let (name, attached) = match arg.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n.to_owned(), Some(v.to_owned())),
            _ => (arg.clone(), None),
        };
        let mut value = |what: &str| match attached.clone().or_else(|| args.next()) {
            Some(v) => Ok(v),
            None => Err(format!(
                "a value is required for '{what}' but none was supplied"
            )),
        };
        match name.as_str() {
            "--" => {
                opts.rustfmt_options.extend(args.by_ref());
                break;
            }
            "-q" | "--quiet" => opts.quiet = true,
            "-v" | "--verbose" => opts.verbose = true,
            "--version" => opts.version = true,
            "--all" => opts.format_all = true,
            "--check" => opts.check = true,
            "-h" | "--help" => return Parsed::Help,
            "-p" | "--package" => {
                match value("--package <package>...") {
                    Ok(v) => opts.packages.push(v),
                    Err(e) => return Parsed::Error(e),
                }
                if attached.is_none() {
                    while let Some(more) = args.next_if(|a| !is_flag(a)) {
                        opts.packages.push(more);
                    }
                }
            }
            "--manifest-path" => match value("--manifest-path <manifest-path>") {
                Ok(v) => opts.manifest_path = Some(v),
                Err(e) => return Parsed::Error(e),
            },
            "--message-format" => match value("--message-format <message-format>") {
                Ok(v) => opts.message_format = Some(v),
                Err(e) => return Parsed::Error(e),
            },
            _ => return Parsed::Error(format!("unexpected argument '{arg}' found")),
        }
    }
    Parsed::Opts(opts)
}

/// Runs `cargo fmt` with `args`, the arguments after `fmt`, and returns its exit code.
pub fn main(args: Vec<String>) -> i32 {
    let exit_status = execute(args);
    let _ = std::io::stdout().flush();
    exit_status
}

const SUCCESS: i32 = 0;
const FAILURE: i32 = 1;
/// clap's exit code for arguments it cannot read.
const USAGE: i32 = 2;

fn execute(args: Vec<String>) -> i32 {
    let opts = match parse_opts(args) {
        Parsed::Opts(opts) => opts,
        Parsed::Help => {
            println!("{}", help());
            return SUCCESS;
        }
        Parsed::Error(msg) => {
            eprintln!(
                "error: {msg}\n\nUsage: blaze fmt [OPTIONS] [-- <rustfmt_options>...]\n\n\
                 For more information, try '--help'."
            );
            return USAGE;
        }
    };

    let verbosity = match (opts.verbose, opts.quiet) {
        (false, false) => Verbosity::Normal,
        (false, true) => Verbosity::Quiet,
        (true, false) => Verbosity::Verbose,
        (true, true) => {
            print_usage_to_stderr("quiet mode and verbose mode are not compatible");
            return FAILURE;
        }
    };

    if opts.version {
        return handle_command_status(get_rustfmt_info(&[String::from("--version")]));
    }
    if opts.rustfmt_options.iter().any(|s| {
        ["--print-config", "-h", "--help", "-V", "--version"].contains(&s.as_str())
            || s.starts_with("--help=")
            || s.starts_with("--print-config=")
    }) {
        return handle_command_status(get_rustfmt_info(&opts.rustfmt_options));
    }

    let strategy = CargoFmtStrategy::from_opts(&opts);
    let mut rustfmt_args = opts.rustfmt_options;
    if opts.check {
        let check_flag = "--check";
        if !rustfmt_args.iter().any(|o| o == check_flag) {
            rustfmt_args.push(check_flag.to_owned());
        }
    }
    if let Some(message_format) = opts.message_format {
        if let Err(msg) = convert_message_format_to_rustfmt_args(&message_format, &mut rustfmt_args)
        {
            print_usage_to_stderr(&msg);
            return FAILURE;
        }
    }

    if let Some(specified_manifest_path) = opts.manifest_path {
        if !specified_manifest_path.ends_with("Cargo.toml") {
            print_usage_to_stderr("the manifest-path must be a path to a Cargo.toml file");
            return FAILURE;
        }
        let manifest_path = PathBuf::from(specified_manifest_path);
        handle_command_status(format_crate(
            verbosity,
            &strategy,
            &rustfmt_args,
            Some(&manifest_path),
        ))
    } else {
        handle_command_status(format_crate(verbosity, &strategy, &rustfmt_args, None))
    }
}

fn convert_message_format_to_rustfmt_args(
    message_format: &str,
    rustfmt_args: &mut Vec<String>,
) -> Result<(), String> {
    let mut contains_emit_mode = false;
    let mut contains_check = false;
    let mut contains_list_files = false;
    for arg in rustfmt_args.iter() {
        if arg.starts_with("--emit") {
            contains_emit_mode = true;
        }
        if arg == "--check" {
            contains_check = true;
        }
        if arg == "-l" || arg == "--files-with-diff" {
            contains_list_files = true;
        }
    }
    match message_format {
        "short" => {
            if !contains_list_files {
                rustfmt_args.push(String::from("-l"));
            }
            Ok(())
        }
        "json" => {
            if !is_nightly() {
                return Err(String::from(
                    "--message-format json is only supported in nightly builds",
                ));
            }
            if contains_emit_mode {
                return Err(String::from(
                    "cannot include --emit arg when --message-format is set to json",
                ));
            }
            if contains_check {
                return Err(String::from(
                    "cannot include --check arg when --message-format is set to json",
                ));
            }
            rustfmt_args.push(String::from("--emit"));
            rustfmt_args.push(String::from("json"));
            Ok(())
        }
        "human" => Ok(()),
        _ => Err(format!(
            "invalid --message-format value: {message_format}. Allowed values are: \
                {MESSAGE_FORMATS}"
        )),
    }
}

fn print_usage_to_stderr(reason: &str) {
    eprintln!("{reason}");
    eprintln!("{}", help());
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verbosity {
    Verbose,
    Normal,
    Quiet,
}

fn handle_command_status(status: Result<i32, io::Error>) -> i32 {
    match status {
        Err(e) => {
            print_usage_to_stderr(&e.to_string());
            FAILURE
        }
        Ok(status) => status,
    }
}

fn get_rustfmt_info(args: &[String]) -> Result<i32, io::Error> {
    Ok(crate::cli::main(args.to_vec()))
}

fn format_crate(
    verbosity: Verbosity,
    strategy: &CargoFmtStrategy,
    rustfmt_args: &[String],
    manifest_path: Option<&Path>,
) -> Result<i32, io::Error> {
    let targets = get_targets(strategy, manifest_path)?;

    // Currently only bin and lib files get formatted.
    run_rustfmt(&targets, rustfmt_args, verbosity)
}

/// Target uses a `path` field for equality and hashing.
#[derive(Debug)]
pub struct Target {
    /// A path to the main source file of the target.
    path: PathBuf,
    /// A kind of target (e.g., lib, bin, example, ...).
    kind: String,
    /// Rust edition for this target.
    edition: Edition,
}

impl Target {
    /// `None` for a metabuild script, which has no source file.
    pub fn from_target(target: &blaze_manifest::targets::Target) -> Option<Self> {
        let path = target.src_path.clone()?;
        let canonicalized = fs::canonicalize(&path).unwrap_or(path);

        Some(Target {
            path: canonicalized,
            kind: kind_name(&target.kind).to_owned(),
            edition: target.edition,
        })
    }
}

/// The first of the target's kinds as `cargo metadata` lists them.
fn kind_name(kind: &TargetKind) -> &str {
    match kind {
        TargetKind::Lib(kinds) => kinds.first().map_or("lib", |k| k.as_str()),
        TargetKind::Bin => "bin",
        TargetKind::ExampleBin | TargetKind::ExampleLib(_) => "example",
        TargetKind::Test => "test",
        TargetKind::CustomBuild => "custom-build",
        TargetKind::Bench => "bench",
    }
}

impl PartialEq for Target {
    fn eq(&self, other: &Target) -> bool {
        self.path == other.path
    }
}

impl PartialOrd for Target {
    fn partial_cmp(&self, other: &Target) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Target {
    fn cmp(&self, other: &Target) -> Ordering {
        self.path.cmp(&other.path)
    }
}

impl Eq for Target {}

impl Hash for Target {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.path.hash(state);
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum CargoFmtStrategy {
    /// Format every packages and dependencies.
    All,
    /// Format packages that are specified by the command line argument.
    Some(Vec<String>),
    /// Format the root packages only.
    Root,
}

impl CargoFmtStrategy {
    pub fn from_opts(opts: &Opts) -> CargoFmtStrategy {
        match (opts.format_all, opts.packages.is_empty()) {
            (false, true) => CargoFmtStrategy::Root,
            (true, _) => CargoFmtStrategy::All,
            (false, false) => CargoFmtStrategy::Some(opts.packages.clone()),
        }
    }
}

/// Based on the specified `CargoFmtStrategy`, returns a set of main source files.
fn get_targets(
    strategy: &CargoFmtStrategy,
    manifest_path: Option<&Path>,
) -> Result<BTreeSet<Target>, io::Error> {
    let mut targets = BTreeSet::new();

    match *strategy {
        CargoFmtStrategy::Root => get_targets_root_only(manifest_path, &mut targets)?,
        CargoFmtStrategy::All => {
            get_targets_recursive(manifest_path, &mut targets, &mut BTreeSet::new())?
        }
        CargoFmtStrategy::Some(ref hitlist) => {
            get_targets_with_hitlist(manifest_path, hitlist, &mut targets)?
        }
    }

    if targets.is_empty() {
        Err(io::Error::other("Failed to find targets".to_owned()))
    } else {
        Ok(targets)
    }
}

/// A package of the workspace as `cargo metadata --no-deps` lists it.
struct Package {
    name: String,
    manifest_path: PathBuf,
    targets: Vec<Target>,
    /// The directories of its path dependencies, with their names.
    path_dependencies: Vec<(String, PathBuf)>,
}

/// The workspace root and member packages of the workspace `manifest_path` (or the working
/// directory) is in: what rustfmt reads from `cargo metadata --no-deps`.
fn get_cargo_metadata(manifest_path: Option<&Path>) -> Result<(PathBuf, Vec<Package>), io::Error> {
    let load = || -> anyhow::Result<(PathBuf, Vec<Package>)> {
        let ctx = Context::new(Config::from_process()?);
        let ws = Workspace::new(&root_manifest(manifest_path, &ctx)?, &ctx)?;
        let packages = ws
            .members()
            .map(|(path, m)| Package {
                name: m.package_id.name.clone(),
                manifest_path: path.to_owned(),
                targets: m.targets.iter().filter_map(Target::from_target).collect(),
                path_dependencies: m
                    .dependencies
                    .iter()
                    .filter_map(|d| Some((d.name.clone(), d.source_id.local_path()?)))
                    .collect(),
            })
            .collect();
        Ok((ws.root().to_owned(), packages))
    };
    load().map_err(|e| io::Error::other(format!("{e:#}")))
}

fn get_targets_root_only(
    manifest_path: Option<&Path>,
    targets: &mut BTreeSet<Target>,
) -> Result<(), io::Error> {
    let (workspace_root, packages) = get_cargo_metadata(manifest_path)?;
    // `workspace_root_path` is the path to the workspace's root directory
    let workspace_root_path = workspace_root.canonicalize()?;
    let (in_workspace_root, current_dir_manifest) = if let Some(target_manifest) = manifest_path {
        // `target_manifest` is the canonicalized path to a `Cargo.toml` file
        let target_manifest = target_manifest.canonicalize()?;
        (
            target_manifest
                .parent()
                .is_some_and(|manifest_dir| workspace_root_path == manifest_dir),
            target_manifest,
        )
    } else {
        let current_dir = env::current_dir()?.canonicalize()?;
        (
            workspace_root_path == current_dir,
            current_dir.join("Cargo.toml"),
        )
    };

    let package_targets: Vec<Target> = match packages.len() {
        1 => packages.into_iter().flat_map(|p| p.targets).collect(),
        _ => packages
            .into_iter()
            .filter(|p| {
                in_workspace_root
                    || p.manifest_path.canonicalize().unwrap_or_default() == current_dir_manifest
            })
            .flat_map(|p| p.targets)
            .collect(),
    };

    for target in package_targets {
        targets.insert(target);
    }

    Ok(())
}

fn get_targets_recursive(
    manifest_path: Option<&Path>,
    targets: &mut BTreeSet<Target>,
    visited: &mut BTreeSet<String>,
) -> Result<(), io::Error> {
    let (_, packages) = get_cargo_metadata(manifest_path)?;
    let manifests: Vec<PathBuf> = packages.iter().map(|p| p.manifest_path.clone()).collect();
    for package in packages {
        targets.extend(package.targets);

        // Look for local dependencies using information available since cargo v1.51
        // It's theoretically possible someone could use a newer version of rustfmt with
        // a much older version of `cargo`, but we don't try to explicitly support that scenario.
        // If someone reports an issue with path-based deps not being formatted, be sure to
        // confirm their version of `cargo` (not `cargo-fmt`) is >= v1.51
        // https://github.com/rust-lang/cargo/pull/8994
        for (name, path) in &package.path_dependencies {
            if visited.contains(name) {
                continue;
            }

            let manifest_path = path.join("Cargo.toml");
            if manifest_path.exists() && !manifests.iter().any(|p| p.eq(&manifest_path)) {
                visited.insert(name.to_owned());
                get_targets_recursive(Some(&manifest_path), targets, visited)?;
            }
        }
    }

    Ok(())
}

fn get_targets_with_hitlist(
    manifest_path: Option<&Path>,
    hitlist: &[String],
    targets: &mut BTreeSet<Target>,
) -> Result<(), io::Error> {
    let (_, packages) = get_cargo_metadata(manifest_path)?;
    let mut workspace_hitlist: BTreeSet<&str> = hitlist.iter().map(|s| s.as_str()).collect();

    for package in packages {
        if workspace_hitlist.remove(package.name.as_str()) {
            targets.extend(package.targets);
        }
    }

    if workspace_hitlist.is_empty() {
        Ok(())
    } else {
        let package = workspace_hitlist.iter().next().copied().unwrap_or_default();
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("package `{package}` is not a member of the workspace"),
        ))
    }
}

fn run_rustfmt(
    targets: &BTreeSet<Target>,
    fmt_args: &[String],
    verbosity: Verbosity,
) -> Result<i32, io::Error> {
    let by_edition = targets
        .iter()
        .inspect(|t| {
            if verbosity == Verbosity::Verbose {
                println!("[{} ({})] {:?}", t.kind, t.edition, t.path)
            }
        })
        .fold(BTreeMap::new(), |mut h, t| {
            h.entry(&t.edition).or_insert_with(Vec::new).push(&t.path);
            h
        });

    let mut status = vec![];
    for (edition, files) in by_edition {
        if verbosity == Verbosity::Verbose {
            print!("rustfmt");
            print!(" --edition {edition}");
            fmt_args.iter().for_each(|f| print!(" {}", f));
            files.iter().for_each(|f| print!(" {}", f.display()));
            println!();
        }

        let mut args: Vec<String> = files.iter().map(|f| f.display().to_string()).collect();
        args.extend(["--edition".to_owned(), edition.to_string()]);
        args.extend(fmt_args.iter().cloned());
        let _quiet = (verbosity == Verbosity::Quiet)
            .then(StdoutToNull::new)
            .flatten();
        status.push(crate::cli::main(args));
    }

    Ok(status
        .iter()
        .copied()
        .find(|&s| s != SUCCESS)
        .unwrap_or(SUCCESS))
}

/// Standard output sent to `/dev/null` while it lives, as cargo-fmt gives `-q`'s rustfmt child.
struct StdoutToNull {
    #[cfg(unix)]
    saved: std::os::fd::OwnedFd,
}

impl StdoutToNull {
    #[cfg(unix)]
    fn new() -> Option<StdoutToNull> {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        let null = fs::OpenOptions::new().write(true).open("/dev/null").ok()?;
        let _ = io::stdout().flush();
        // SAFETY: `dup` and `dup2` on the process's standard output and a file it owns;
        // the duplicate is owned by `saved` and restored on drop.
        unsafe {
            let saved = libc::dup(libc::STDOUT_FILENO);
            if saved < 0 {
                return None;
            }
            let saved = OwnedFd::from_raw_fd(saved);
            if libc::dup2(null.as_raw_fd(), libc::STDOUT_FILENO) < 0 {
                return None;
            }
            Some(StdoutToNull { saved })
        }
    }

    #[cfg(not(unix))]
    fn new() -> Option<StdoutToNull> {
        None
    }
}

impl Drop for StdoutToNull {
    fn drop(&mut self) {
        let _ = io::stdout().flush();
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            // SAFETY: puts back the standard output `new` saved.
            unsafe {
                libc::dup2(self.saved.as_raw_fd(), libc::STDOUT_FILENO);
            }
        }
    }
}
