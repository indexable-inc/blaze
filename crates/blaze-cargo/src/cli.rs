//! `blaze build|run|test|check|bench|doc` and `cargo blaze <the same>`, `blaze stop`, `blaze
//! status`: a client of the agent API with cargo's flags, output and exit codes
//! (docs/v2/plan.md section 7). It reads no manifest and commits nothing (the
//! daemon's watcher has the edits): it finds the checkout, sends one request with the whole
//! command line, prints the responses as cargo prints its build, and runs what `run` and `test`
//! run itself, on the terminal.
//!
//! - [`args`]: cargo's command lines, as its clap reads them.
//! - [`place`]: the checkout, the config files outside it, the environment, the color.
//! - [`daemon`]: finding and starting `blaze serve`, and the [`daemon::Api`] it is spoken to by.
//! - [`print`]: a build's responses as cargo's lines; [`say`] and [`json`] are their texts.
//! - [`run`] and [`test`]: what runs after the build; [`doc`]: `doc`'s pages, written.
//! - [`resume`]: a build or a run the thin `blaze` began (blaze-thin, docs/client.md), gone on
//!   with from the connection and the answer it hands over.
//! - [`inspect`]: blaze's own looks at the last build (`codegen-inputs`, `trace`, `tables`).
//! - [`observe`]: blaze's looks at itself: `explore`, `debug profile|live|sql`, `explain`.
//! - [`replace`]: `check --replace`, a path moved through the front end's name resolution.
//! - [`reach`]: `reach`, what the checkout's edits since a root reach, building only what its
//!   front end applies.
//! - [`baselines`]: `check --baselines`, a run's lint counts held to the checked-in ones.
//! - [`fix`]: `check --fix`, the check's machine-applicable suggestions made and checked.
//! - [`clean`]: `clean`, a workspace's build directory under blaze's root removed; `--list`
//!   and `--all`, every workspace's.
//! - [`cache`]: `cache gc`, one run of the collector of what blaze keeps on disk.
//! - [`new`], [`tree`], [`metadata`]: cargo's `new`, `init`, `tree` and `metadata`, each answered
//!   in this process from the workspace's files (blaze-new, blaze-tree, blaze-metadata), with
//!   the command-line reader they share in [`own`].

#![forbid(unsafe_code)]

pub mod agents;
pub mod args;
pub mod baselines;
pub mod cache;
// Lane packs-1112: `blaze store export`, `import` and `install`.
pub mod store;
pub mod clean;
pub mod client_rows;
pub mod daemon;
pub mod diagnostics;
pub mod doc;
pub mod doctor;
mod duckdb;
pub mod edits;
pub mod fetch_url;
pub mod fix;
pub mod inspect;
pub mod json;
pub mod live;
#[cfg(feature = "native")]
pub mod metadata;
pub mod new;
pub mod observe;
pub mod own;
pub mod perf;
pub mod place;
#[cfg(feature = "native")]
pub mod pprof;
pub mod print;
pub mod reach;
pub mod replace;
pub mod resume;
pub mod run;
pub mod say;
pub mod show;
pub mod terminal;
pub mod test;
#[cfg(feature = "native")]
pub mod tree;
pub mod which;

use std::{
    ffi::OsString,
    io::{self, IsTerminal as _, Write},
    process::ExitCode,
    time::Instant,
};

use blaze_api::{Cargo, Request, ResponseRef, TestResult, Ub, ViewRef};
use blaze_core::exit::Exit;

use self::{
    args::{Asked, Command, Invocation},
    daemon::{Api, Start},
    place::Place,
    print::{Printer, Shown},
    test::{Programs, Speaking},
};

const HELP: &str = r#"blaze: Rust's cargo, built by blaze's daemon

Usage: blaze <COMMAND> [OPTIONS]
       cargo blaze <COMMAND> [OPTIONS]

Commands:
    build, b    Compile the current package
    check, c    Analyze the current package with every lint (rustc's, clippy's, blaze's) and report
                errors, but don't build object files; `--fix [--lint NAME]...` makes the
                machine-applicable suggestions, `--replace FROM=TO` retargets every reference to
                the item FROM to TO by name resolution, `--baselines DIR` holds lint counts to DIR's
    run, r      Run a binary or example of the local package
    test, t     Run the tests
    bench       Run the benchmarks
    doc, d      Build a package's documentation, from blaze's front end: no rustdoc runs
    lock        Resolve the workspace and write Cargo.lock and blaze.lock, saying what moved, even
                under CI or `lock = "required"`; nothing is checked or built
    clean       Remove the workspace's build directory (under blaze's root, never in the checkout);
                `--list` every workspace's with its size, `--all` removes them all
    cache gc    Collect what blaze keeps on disk now (`--dry-run` removes nothing, `--last`
                prints the daemon's last runs)
    fmt         Format the workspace's packages with blaze's own rustfmt (`--check`, `-p NAME`,
                `--all`, `-- RUSTFMT_ARGS`), or the `.rs` files named; `blaze fmt --help`
    new, init   Create a package at <PATH>, or in the directory it is run in (`--bin`, `--lib`,
                `--name`, `--edition`, `--vcs`): of the blaze edition where `--bstd <PATH>`
                says where bstd is, else, or with `--rust`, as cargo writes one
    tree        The workspace's dependency graph, with cargo's flags (`-e`, `-i`, `-p`, `--depth`)
    metadata    The workspace and its resolve as `cargo metadata --format-version 1` prints them
    serve       Run the daemon the other commands are clients of
    stop        Make the daemon's state durable and stop it
    status      What the daemon is doing (`--json`: one object, docs/cargo.md)
    toolchain   Download the Rust blaze builds with, once (`--json`: its name, host and sysroot)
    daemon      install: the system starts the daemon on demand; uninstall
    codegen-inputs --format=json   Each instance the last build made, with its code hash
    trace --out PATH               Every result of the last build, with its edges, as CBOR
    tables --format=json           The size of each of the daemon's long-lived tables
    explore [--dev] [--print]      The explorer: the daemon's builds and spans at https://ix.dev/explore
    debug profile [--seconds N]    The daemon's flight recorder as a Chrome trace
    debug live                     The daemon's threads as they run
    debug sql QUERY                SQL over the profile history, flushed first (every row recorded so far)
    explain RULE|PACKAGE|VIEW      A rule's long text, the last failure of PACKAGE with all it blocks, or a history view
    why [ITEM] [--all] [--json]    What the last build computed again instead of reusing, and why
    reach [ROOT] [--json]          What the edits since ROOT reach (items, functions, tests), and the root now
    slow [--build N] [--top N] [--json]   What made the last build slow, ranked by the time each fix saves
    diagnostics [--since TOKEN] [-p PKG] [--file F] [--json]   What a check says that is new since TOKEN, and the next token
    doctor [--json]                What is wrong with blaze here: its version, agents, daemon, state, toolchain, project, sandbox
    agents install|hook|status|remove [claude|codex|all]   Coding agents told to use blaze, and shown its diagnostics after each edit
    pprof FILE|URL [top|peek|tree|list|flame|diff|export|--http]   Any program's profile (.ixprof or pprof), like `go tool pprof`
    perf record|runs|diff          The performance history of programs blaze built: what changed between runs, and why

Options:
    -V, --version  Print version info and exit
    -h, --help     Print help

Every command takes cargo's flags; `blaze help <COMMAND>` lists them.
"#;

/// Why the build recomputed what it did, as the daemon sums it up (`observe::recomputed`,
/// docs/v2/plan/design/why.md): what the build's last line says the cause and the functions
/// from. A build that recomputed nothing has none; a summary the daemon could not give is
/// said as a warning, and the command's own outcome stands. None under `-q`, which says
/// nothing of a build that passed.
///
/// `ask`: whether a summary the build's own answer did not carry is asked of the daemon, which
/// waits for its history thread to have explained the build. A `run` does not ask: its program
/// starts as soon as its image is built, and `blaze why` has the reasons.
fn why_recomputed<O: Write, S: show::Sink>(
    printer: &mut Printer<'_, O, S>,
    shown: Shown,
    ask: bool,
) -> io::Result<Option<Vec<blaze_api::Report>>> {
    if shown.quiet {
        return Ok(None);
    }
    match observe::recomputed(ask) {
        Ok(summary) => Ok(Some(summary)),
        Err(why) => {
            printer.err().write_all(say::warning(&why, shown.color).as_bytes())?;
            Ok(None)
        }
    }
}

/// What this client recorded of its build, sent to the daemon's history, and with
/// `BLAZE_PROFILE` the build's trace written (its path said on `err`): a failure is said on
/// `err` as a warning naming the step and the build, and the command's own outcome stands.
///
/// They go on the command's own connection, `client`, before it closes.
/// `wait`: whether the daemon's answer to them is waited for. A `run` about to become its
/// program does not wait (unless `BLAZE_PROFILE` is to read them back): the rows are handed to
/// the daemon and the program starts.
fn sent_client_rows(client: &mut blaze_api::client::Client, err: &mut impl Write, color: bool, wait: bool) {
    let said = match client_rows::send(client, wait) {
        Ok(None) => return,
        Ok(Some(path)) => format!(
            "{}: the build's trace (ui.perfetto.dev): {}\n",
            client_rows::PROFILE,
            path.display()
        ),
        Err(why) => say::warning(&why, color),
    };
    _ = err.write_all(said.as_bytes());
}

/// This process's time counted from now ([`blaze_profile::began`]): what the program calls
/// first, before the work that comes before [`main`] (finding the workspace's pin).
pub fn began() {
    blaze_profile::began();
}

/// The flight recorder installed ([`blaze_profile::install`]), for the spans of the work
/// before [`main`], which installs it again to no effect. It starts no thread.
pub fn record() {
    blaze_profile::install();
}

/// The `blaze` program, and `cargo-blaze`, which cargo runs for `cargo blaze` as
/// `cargo-blaze blaze <command> ...`.
pub fn main(args: impl Iterator<Item = OsString>) -> ExitCode {
    blaze_profile::began();
    let words = words(args);
    // A command the thin `blaze` began (blaze-thin's handover): its connection and the answer
    // read so far come first, then the command line as it was typed.
    let (began, words) = match blaze_thin::handover::Began::take(words) {
        Ok(taken) => taken,
        Err(what) => {
            blaze_report::err!("{}", say::error(&what, auto_color()));
            return ended(Exit::Usage.code());
        }
    };
    // A test executable the daemon launched runs as its test, with the daemon's priority
    // already inherited: `BLAZE_CORES` is the CLI's and the daemon's, not the test's.
    let trampoline = words.first().is_some_and(|it| it == run::TRAMPOLINE);
    if !trampoline && let Err(code) = cores() {
        return ended(code);
    }
    // The client and `blaze serve` both record into the flight recorder from here on: the
    // daemon writes its history, and the client sends its rows there as its request ends
    // (`client_rows`), which `BLAZE_PROFILE` then exports as the build's trace.
    blaze_profile::install();
    _ = blaze_profile::flight::count_frame_copies(|| {
        let it = blaze_mem::tag::copied_on_thread();
        blaze_profile::flight::FrameCopied { copies: it.copies, bytes: it.bytes }
    });
    _ = blaze_profile::flight::count_cpu(blaze_profile::thread_cpu);
    // Spans too short for any reader to look at one are summed under their parent instead of a
    // row each: 86% of a self-build's spans (`flight::FOLD_BELOW`). Until 2026-11-04 nothing
    // set this, and one 52-minute build wrote 192 M spans in 6,175 files.
    blaze_profile::flight::fold_below(blaze_profile::flight::FOLD_BELOW);
    if words.first().is_some_and(|it| it == "serve") {
        #[cfg(feature = "native")]
        return crate::serve::main(words.into_iter().skip(1));
        #[cfg(not(feature = "native"))]
        {
            blaze_report::err!("{}", say::error(&format!("blaze serve: {}", daemon::NO_DAEMON), auto_color()));
            return ended(Exit::Failed.code());
        }
    }
    // A test executable started through this blaze, to run as the process the daemon launched.
    if trampoline {
        return ended(run::trampoline(words.into_iter().skip(1), auto_color()));
    }
    ended(command_began(words, began))
}

/// This process's core budget read (`BLAZE_CORES`, [`blaze_core::cores`]) and, unless it says
/// `all`, its priority lowered, before it starts any thread or process, so all of them
/// inherit it: the client's and `blaze serve`'s both, and the daemon a client starts inherits
/// the client's. A value it refuses ends the process with the error; a priority it can't lower
/// is a warning, and the process goes on ([`blaze_core::cores::lower`] says why).
fn cores() -> Result<(), i32> {
    let cores = match blaze_core::cores::get() {
        Ok(cores) => cores,
        Err(why) => {
            blaze_report::err!("{}", say::error(&why.to_string(), auto_color()));
            return Err(Exit::Usage.code());
        }
    };
    if let Err(why) = blaze_core::cores::lower(&cores) {
        blaze_report::err!("{}", say::warning(&why.to_string(), auto_color()));
    }
    Ok(())
}

/// The process's end with `code`: as `main`'s `ExitCode` where it fits in one, else at once.
fn ended(code: i32) -> ExitCode {
    match u8::try_from(code) {
        Ok(code) => ExitCode::from(code),
        Err(_) => std::process::exit(code),
    }
}

/// The words after the program's name, without the `blaze` cargo passes before them.
pub fn words(args: impl Iterator<Item = OsString>) -> Vec<OsString> {
    let mut words: Vec<OsString> = args.skip(1).collect();
    if words.first().is_some_and(|it| it == "blaze") {
        words.remove(0);
    }
    words
}

/// The words after `blaze status` or `blaze toolchain`: `--json` or nothing, whether JSON was
/// asked; any other word refused, naming it and the command.
fn json_flag(command: &str, words: Vec<OsString>) -> Result<bool, String> {
    let mut json = false;
    for word in words {
        match word.to_str() {
            Some("--json") => json = true,
            // alloc: the refusal, once
            _ => {
                return Err(format!(
                    "`blaze {command}` takes only `--json`, not `{}`",
                    word.to_string_lossy()
                ));
            }
        }
    }
    Ok(json)
}

/// Whether stderr is colored when nothing says how: clap's choice for its own errors.
fn auto_color() -> bool {
    place::color(Some("auto"), |name| std::env::var(name).ok(), io::stderr().is_terminal())
        .unwrap_or(false)
}

/// Every word that names a command (`args::parse`'s), as `blaze --help` orders them; the
/// one-letter aliases are left out, since any short typo is near one. A test parses each, so a
/// word here that is no command fails.
const COMMANDS: &[&str] = &[
    "build",
    "check",
    "run",
    "test",
    "bench",
    "doc",
    "lock",
    "clean",
    "cache",
    "gc",
    "store",
    "fmt",
    "new",
    "init",
    "tree",
    "metadata",
    "serve",
    "stop",
    "status",
    "toolchain",
    "daemon",
    "codegen-inputs",
    "trace",
    "tables",
    "explore",
    "debug",
    "explain",
    "why",
    "reach",
    "slow",
    "diagnostics",
    "doctor",
    "agents",
    "pprof",
    "perf",
];

/// What people type for a command that has another name: the words typed, the command meant,
/// and what to know about it. `blaze daemon` takes `install` and `uninstall` alone; the
/// daemon's other verbs are top-level commands, and nothing starts it by hand.
const ALIASES: &[(&str, &str, &str)] = &[
    ("daemon stop", "blaze stop", "it makes the daemon's state durable and stops it"),
    ("daemon kill", "blaze stop", "it makes the daemon's state durable and stops it"),
    ("daemon shutdown", "blaze stop", "it makes the daemon's state durable and stops it"),
    ("daemon quit", "blaze stop", "it makes the daemon's state durable and stops it"),
    ("daemon status", "blaze status", "it says what the daemon is doing"),
    ("daemon restart", "blaze stop", "the next command starts the daemon again"),
    ("daemon start", "blaze serve", "though any command starts the daemon when none runs"),
    ("daemon run", "blaze serve", "though any command starts the daemon when none runs"),
    ("daemon serve", "blaze serve", "though any command starts the daemon when none runs"),
    ("daemon", "blaze daemon install", "or `blaze daemon uninstall`"),
    ("shutdown", "blaze stop", "it makes the daemon's state durable and stops it"),
    ("kill", "blaze stop", "it makes the daemon's state durable and stops it"),
    ("start", "blaze serve", "though any command starts the daemon when none runs"),
    ("clippy", "blaze check", "it runs every lint, clippy's too"),
    ("fix", "blaze check --fix", "it makes the machine-applicable suggestions"),
];

/// The command `name` (the words typed, as `args::parse` hands them: `daemon stop`) most
/// likely meant, with what to know about it: a known other name for one ([`ALIASES`]), else
/// the nearest of [`COMMANDS`] by rustc's edit distance (`stdx::edit_distance`, the one
/// "did you mean" of the tree), for `daemon <word>` the nearest of its two verbs. None when
/// nothing is near: a guess that is far off sends a script further wrong than no guess.
fn meant(name: &str) -> Option<(String, &'static str)> {
    if let Some((_, command, note)) = ALIASES.iter().find(|(typed, _, _)| *typed == name) {
        // alloc: the suggestion of a refusal, once
        return Some(((*command).to_owned(), *note));
    }
    if let Some(verb) = name.strip_prefix("daemon ") {
        let verbs = ["install", "uninstall"];
        let near = stdx::edit_distance::find_best_match_for_name(&verbs, verb, None)?;
        // alloc: the suggestion of a refusal, once
        return Some((format!("blaze daemon {near}"), ""));
    }
    let near = stdx::edit_distance::find_best_match_for_name(COMMANDS, name, None)?;
    // alloc: the suggestion of a refusal, once
    Some((format!("blaze {near}"), ""))
}

/// The refusal of `blaze <name>` where `name` is no command: the command it most likely meant
/// ([`meant`]) said first, then every command. The caller exits nonzero either way: a script
/// that runs a command that does not exist has failed, whatever was meant.
fn no_such_command(name: &str) -> String {
    let commands = COMMANDS.join(", "); // alloc: the refusal, once
    // alloc: the refusal, once
    let help = match meant(name) {
        Some((command, "")) => format!("help: did you mean `{command}`?\n\n"),
        Some((command, note)) => format!("help: did you mean `{command}`? ({note})\n\n"),
        None => String::new(),
    };
    // alloc: the refusal, once
    format!("no such command: `blaze {name}`\n\n{help}help: blaze's commands are {commands}")
}

/// One command line, run: its exit code.
pub fn command(words: Vec<OsString>) -> i32 {
    command_began(words, None)
}

/// [`command`], of a command line the thin `blaze` began when `began` says so: the request is
/// on its connection already, and this goes on from the answer read so far ([`resume`]). Only
/// a build command is ever begun; any other with `began` is refused, naming it.
fn command_began(words: Vec<OsString>, began: Option<blaze_thin::handover::Began>) -> i32 {
    let parsing =
        tracing::info_span!(target: "blaze", "client: parse the command line").entered();
    let parsed = args::parse(words);
    drop(parsing);
    let asked = match parsed {
        Ok(asked) => asked,
        Err(usage) => {
            blaze_report::err!("{}", args::refusal(&usage, auto_color()));
            return Exit::Usage.code();
        }
    };
    let said = |result: anyhow::Result<i32>| match result {
        Ok(code) => code,
        Err(error) => {
            blaze_report::err!("{}", say::error(&format!("{error:#}"), auto_color()));
            Exit::Failed.code()
        }
    };
    // A command that only asks the daemon says first when that daemon is another build of
    // blaze than this binary (`which.rs`): its answer is of that build's work.
    if matches!(
        asked,
        Asked::Status(_)
            | Asked::Debug(_)
            | Asked::Explain(_)
            | Asked::Why(_)
            | Asked::Slow(_)
            | Asked::Diagnostics(_)
    ) {
        which::warn_if_other(auto_color());
    }
    if began.is_some() && !matches!(asked, Asked::Cargo(_)) {
        let what = format!(
            "`blaze {}` hands over a build or a run the thin `blaze` began, and what follows it is neither",
            blaze_thin::handover::WORD
        );
        blaze_report::err!("{}", say::error(&what, auto_color()));
        return Exit::Usage.code();
    }
    match asked {
        Asked::Help(None) => {
            blaze_report::out!("{HELP}");
            0
        }
        Asked::Help(Some(command)) => {
            blaze_report::out!("{}", args::help(command));
            0
        }
        Asked::Version => {
            // The release line, then which binary this is (`which.rs`).
            blaze_report::out!("{}", which::version());
            0
        }
        Asked::Unknown(name) => {
            blaze_report::err!("{}", say::error(&no_such_command(&name), auto_color()));
            Exit::Failed.code()
        }
        Asked::Stop => said(daemon::stop(&mut io::stderr().lock(), auto_color())),
        Asked::Status(words) => match json_flag("status", words) {
            Ok(json) => said(daemon::status(
                &mut io::stdout().lock(),
                &mut io::stderr().lock(),
                auto_color(),
                json,
            )),
            Err(what) => {
                blaze_report::err!("{}", say::error(&what, auto_color()));
                Exit::Usage.code()
            }
        },
        Asked::Toolchain(words) => {
            let json = match json_flag("toolchain", words) {
                Ok(json) => json,
                Err(what) => {
                    blaze_report::err!("{}", say::error(&what, auto_color()));
                    return Exit::Usage.code();
                }
            };
            match crate::toolchain::ensure().and_then(|chosen| match json {
                true => crate::toolchain::json(&chosen),
                false => Ok(format!("{chosen}\n")),
            }) {
                Ok(text) => {
                    blaze_report::out!("{text}");
                    0
                }
                Err(what) => {
                    blaze_report::err!("{}", say::error(&what, auto_color()));
                    Exit::Failed.code()
                }
            }
        }
        Asked::Daemon(verb) => said(daemon::units(verb, &mut io::stderr().lock(), auto_color())),
        Asked::Inspect(what, words) => inspect::main(what, words, auto_color()),
        Asked::Explore(words) => observe::explore(words, auto_color()),
        Asked::Debug(words) => observe::debug(words, auto_color()),
        Asked::Explain(words) => observe::explain(words, auto_color()),
        Asked::Why(words) => observe::why(words, auto_color()),
        Asked::Reach(words) => reach::main(words, auto_color()),
        Asked::Slow(words) => observe::slow(words, auto_color()),
        Asked::Doctor(words) => doctor::main(words, auto_color()),
        Asked::Diagnostics(words) => diagnostics::main(words, auto_color()),
        Asked::Agents(words) => agents::main(words, auto_color()),
        #[cfg(feature = "native")]
        Asked::Pprof(words) => pprof::main(words, auto_color()),
        #[cfg(not(feature = "native"))]
        Asked::Pprof(_) => said(Err(anyhow::anyhow!(
            "blaze pprof: this blaze is built without blaze-cargo's `native` feature (the seed profile, docs/future/bootstrap.md)"
        ))),
        Asked::Perf(words) => perf::main(words, auto_color()),
        Asked::Clean(words) => clean::main(words, auto_color()),
        Asked::Cache(words) => cache::main(&words, auto_color()),
        Asked::FetchUrl(words) => fetch_url::main(&words, auto_color()),
        Asked::Fmt(words) => fmt(words),
        Asked::New { init, words } => new::main(init, words, auto_color()),
        #[cfg(feature = "native")]
        Asked::Tree(words) => tree::main(words, auto_color()),
        #[cfg(feature = "native")]
        Asked::Metadata(words) => metadata::main(words, auto_color()),
        #[cfg(not(feature = "native"))]
        Asked::Tree(_) | Asked::Metadata(_) => said(Err(anyhow::anyhow!(
            "blaze tree and blaze metadata fetch the packages the lock file names: this blaze is built without blaze-cargo's `native` feature (the seed profile, docs/future/bootstrap.md), which has no network"
        ))),
        Asked::Cargo(asked) => cargo(&asked, began),
    }
}

/// `blaze fmt`: blaze's formatter over the words after `fmt`, in this process; no daemon, and no
/// rustfmt or cargo-fmt is started. A word that is not UTF-8 is refused, naming it: the
/// formatter's command line is rustfmt's and cargo-fmt's, which read only UTF-8.
fn fmt(words: Vec<OsString>) -> i32 {
    let mut args = Vec::with_capacity(words.len()); // alloc: the formatter's command line, once
    for word in words {
        match word.into_string() {
            Ok(word) => args.push(word),
            Err(word) => {
                let what =
                    format!("blaze fmt: the argument {} is not UTF-8", word.to_string_lossy());
                blaze_report::err!("{}", say::error(&what, auto_color()));
                return Exit::Usage.code();
            }
        }
    }
    blaze_fmt::command::main(args)
}

/// The request of a cargo command line, once its place and `Cargo` are known. `test` runs in
/// the daemon under blaze's `--reached` (the tests the edit reached) and `--ub` (every selected
/// test, or the reached ones, in the MIR evaluator, nothing compiled); else the CLI runs what the
/// build placed.
pub fn request(asked: &Invocation, cargo: Cargo) -> Result<Request, String> {
    let view = ViewRef::Head(cargo.root.clone());
    let targets = place::targets(asked, cargo).map_err(|it| it.to_string())?;
    let flags = &asked.flags;
    Ok(match asked.command() {
        Command::Check if asked.lock => Request::Lock { view, targets },
        Command::Check => Request::Check { view, targets },
        Command::Doc => {
            Request::Doc { view, targets, no_deps: flags.no_deps, private: flags.private }
        }
        Command::Test | Command::Bench if in_daemon(asked) => {
            if flags.ub && flags.no_run {
                return Err(
                    "`--ub` runs the tests in blaze's evaluator, which `--no-run` says not to do"
                        .to_owned(),
                );
            }
            let ub = flags.ub.then_some(Ub { seed: flags.ub_seed, schedules: flags.ub_schedules });
            let args = asked.program.iter().map(|it| {
                it.to_str().map(str::to_owned).ok_or_else(|| {
                    format!("the test argument {} is not UTF-8, which a request to the daemon can't carry", it.to_string_lossy())
                })
            });
            let args = args.collect::<Result<_, _>>()?;
            Request::Test {
                view,
                targets,
                filter: asked.filter.iter().cloned().collect(),
                all: !flags.reached,
                args,
                ub,
            }
        }
        Command::Build | Command::Run | Command::Test | Command::Bench => {
            Request::Build { view, targets }
        }
    })
}

/// The tests run in the daemon, which sends their results: blaze's `--reached` and `--ub`.
fn in_daemon(asked: &Invocation) -> bool {
    asked.flags.reached || asked.flags.ub
}

/// Everything of the machine the request needs: where it runs and what it sends.
fn prepared(asked: &Invocation, color: bool) -> Result<(Place, Request), String> {
    let (place, cargo) = prepared_cargo(asked, color)?;
    let request = request(asked, cargo)?;
    Ok((place, request))
}

/// Where the command runs, and its command line as the daemon's load reads it.
fn prepared_cargo(asked: &Invocation, color: bool) -> Result<(Place, Cargo), String> {
    let cwd = std::env::current_dir().map_err(|error| format!("the current directory: {error}"))?;
    let env = place::env(std::env::vars_os())?;
    let place =
        place::find(&cwd, asked.flags.manifest_path.as_deref(), place::script(&asked.args))?;
    let home = blaze_registry::cargo_home();
    let mut files = place::config_files(&place, &asked.flags.config, home.as_deref())?;
    if asked.command() == Command::Check {
        files.extend(place::clippy_files(&place)?);
    }
    let width = place::width(
        |name| std::env::var(name).ok(),
        io::stderr().is_terminal(),
        place::stderr_columns,
    )?;
    let cargo = place::cargo(asked, &place, env, files, color, width)?;
    Ok((place, cargo))
}

/// `build`, `check`, `run`, `test` or `bench`. `began`: the thin `blaze` sent the request
/// already ([`resume`]); this makes the same one, holds it to the one sent, and reads the
/// answer from where the thin `blaze` stopped.
fn cargo(asked: &Invocation, began: Option<blaze_thin::handover::Began>) -> i32 {
    let started = Instant::now();
    let refused = |what: &str, color: bool| {
        blaze_report::err!("{}", say::error(what, color));
        Exit::Failed.code()
    };
    // cargo's `cli::main`, then `GlobalContext::configure`, before the shell has its color.
    if let Some(directory) = &asked.directory {
        if !asked.unstable_options {
            let what = "the `-C` flag is unstable, pass `-Z unstable-options` on the nightly channel to enable it";
            return refused(what, auto_color());
        }
        if let Err(error) = std::env::set_current_dir(directory) {
            blaze_report::err!(
                "{}",
                run::caused(
                    "could not change to requested directory",
                    &[error.to_string()],
                    auto_color()
                )
            );
            return Exit::Failed.code();
        }
    }
    if asked.flags.verbose > 0 && asked.flags.quiet {
        return refused("cannot set both --verbose and --quiet", auto_color());
    }
    let var = |name: &str| std::env::var(name).ok();
    let config = match terminal::config() {
        Ok(config) => config,
        Err(what) => return refused(&what, auto_color()),
    };
    let flag = terminal::color_flag(asked.flags.color.as_deref(), &config, var);
    let color = match place::color(flag, var, io::stderr().is_terminal()) {
        Ok(color) => color,
        Err(what) => return refused(&what, auto_color()),
    };
    let format = match print::format(&asked.flags.message_format) {
        Ok(format) => format,
        Err(what) => return refused(&what, color),
    };
    if let Some(what) = args::refused(asked) {
        return refused(&what, color);
    }
    // Each stretch of the client's own time is a span (slow-is-p0-traced): they reach the
    // history with the client's rows (`client_rows`), under the build the daemon names.
    let preparing = tracing::info_span!(
        target: "blaze",
        "client: find the checkout, its config files and the environment"
    )
    .entered();
    let prepared = prepared(asked, color);
    drop(preparing);
    let (place, mut request) = match prepared {
        Ok(prepared) => prepared,
        Err(what) => return refused(&what, color),
    };
    crate::toolchain::say_toolchain_file(&place.cwd, asked.flags.verbose > 0);
    let finding = tracing::info_span!(target: "blaze", "client: find the toolchain").entered();
    let found = crate::toolchain::ensure();
    drop(finding);
    if let Err(what) = found {
        return refused(&what, color);
    }
    if asked.command() == Command::Check && !asked.flags.replace.is_empty() {
        return replaced(&place, &asked.flags.replace, color);
    }
    let pinned = match asked.command() == Command::Check && asked.flags.fix {
        true => match fix::pin(&mut request) {
            Ok(pinned) => Some(pinned),
            Err(what) => return refused(&what, color),
        },
        false => None,
    };
    let connecting =
        tracing::info_span!(target: "blaze", "client: connect to the daemon").entered();
    let connected = match began {
        Some(began) => resume::client(began).map(Some).map_err(anyhow::Error::msg),
        None => daemon::connect(match asked.flags.no_prebuilt {
            true => Start::IfMissingNoPrebuilt,
            false => Start::IfMissing,
        }),
    };
    drop(connecting);
    let mut client = match connected {
        Ok(Some(client)) => client,
        Ok(None) => return refused("no daemon answers, and none could be started", color),
        Err(error) => return refused(&format!("{error:#}"), color),
    };
    let stats = std::env::var_os("BLAZE_STATS").is_some();
    let fixing = pinned.is_some();
    let terminal = terminal::stderr(&config, color);
    // What the command shows goes through one screen (docs/terminal.md): on a terminal a
    // region redrawn while the build runs, elsewhere lines that stay.
    let var = |name: &str| std::env::var(name).ok();
    let caps = match format.json() {
        // The JSON messages are stdout's, a line each: nothing is redrawn around them.
        true => blaze_screen::Caps::PIPE,
        false => show::caps(io::stderr().is_terminal(), var),
    };
    let shown = Shown {
        format,
        quiet: asked.flags.quiet,
        verbose: asked.flags.verbose,
        color,
        stats,
        fixing,
        terminal,
        live: caps.live,
    };
    let options = blaze_screen::Options {
        command: match asked.command() {
            Command::Build => blaze_screen::Command::Build,
            Command::Check => blaze_screen::Command::Check,
            Command::Run => blaze_screen::Command::Run,
            Command::Test => blaze_screen::Command::Test,
            _ => blaze_screen::Command::Other,
        },
        quiet: shown.quiet,
        verbose: shown.verbose > 0,
        root: Some(place.root.to_string_lossy().into_owned()),
        cwd: Some(place.cwd.to_string_lossy().into_owned()),
        home: std::env::var("HOME").ok(),
        footer_after_ms: blaze_screen::FOOTER_AFTER_MS,
    };
    let screen = show::Show::new(options, caps, terminal, started, io::stderr());
    let mut out = io::stdout().lock();
    let then = match caps.live {
        // A region is redrawn: the screen has a thread of its own, which draws when a timer
        // of the region is due whether or not the daemon says anything.
        true => {
            if let Err(error) = show::watch_interrupts(screen.terminal()) {
                return refused(&format!("the screen's interrupt watcher could not start: {error}"), color);
            }
            let screen = match show::Threaded::start(screen) {
                Ok(screen) => screen,
                Err(error) => return refused(&format!("the screen's thread could not start: {error}"), color),
            };
            let mut to_screen = screen.to.clone(); // clone: a sender, once a command
            let then = match live::wanted(asked, shown, caps) {
                // `test` runs its executables while the build goes on.
                true => live::drive(&mut client, asked, &place, request, shown, started, to_screen),
                false => {
                    let then = drive(
                        &mut client,
                        asked,
                        &place,
                        request,
                        shown,
                        started,
                        &mut out,
                        &mut to_screen,
                        &mut test::Spawned,
                    );
                    drop(to_screen);
                    then
                }
            };
            // The last frame is written and the terminal as it was found before anything
            // else writes it: the program a `run` becomes starts on a clean terminal.
            match screen.finish() {
                Ok(()) => then,
                Err(error) => Err(error),
            }
        }
        false => {
            let mut screen = screen;
            drive(
                &mut client,
                asked,
                &place,
                request,
                shown,
                started,
                &mut out,
                &mut screen,
                &mut test::Spawned,
            )
        }
    };
    let mut err = io::stderr().lock();
    // A `run` about to become its program hands its rows to the daemon and does not wait for
    // the answer: the program starts as soon as its image is known to be built. They go on the
    // command's own connection, before it closes: a command is one connection.
    sent_client_rows(&mut client, &mut err, color, !matches!(then, Ok(Then::Exec(_))));
    // The seed's one-shot engine stops here, writing its image; a daemon's connection just closes.
    let closed = client.close();
    if let Err(error) = closed {
        let _ = err.write_all(
            say::error(&format!("closing the connection to the build engine: {error}"), color)
                .as_bytes(),
        );
        return Exit::Failed.code();
    }
    match then {
        Ok(Then::Exit(code)) => code,
        Ok(Then::Exec(exec)) => {
            drop(out);
            run::exec(
                &exec,
                &mut err,
                run::Told { color, quiet: shown.quiet, verbose: shown.verbose },
            )
        }
        Ok(Then::Fix(found)) => {
            let Some(pinned) = &pinned else { return refused("`--fix` was not asked for", color) };
            match fix::run(found, pinned, &place.root) {
                Ok(fixed) => {
                    let files = match fixed.files.is_empty() {
                        true => String::new(),
                        false => format!(": {}", fixed.files.join(", ")),
                    };
                    let rest = format!(
                        "{} suggestion(s) in {} file(s){files}",
                        fixed.made,
                        fixed.files.len()
                    );
                    let _ = err.write_all(say::status("Fixed", &rest, color).as_bytes());
                    if fixed.left > 0 {
                        let left = format!(
                            "{} suggestion(s) are not machine-applicable, or are outside the checkout",
                            fixed.left
                        );
                        let _ = err.write_all(say::status("Left", &left, color).as_bytes());
                    }
                    if fixed.others > 0 {
                        let others = format!(
                            "{} suggestion(s) are of packages the request did not ask for, which `--fix` does not edit: name one with `-p` to fix it",
                            fixed.others
                        );
                        let _ = err.write_all(say::status("Left", &others, color).as_bytes());
                    }
                    if fixed.errors == 0 { 0 } else { Exit::Failed.code() }
                }
                Err(what) => {
                    let _ = err.write_all(say::error(&what, color).as_bytes());
                    Exit::Failed.code()
                }
            }
        }
        // stdout or stderr closed under it: nothing more can be said.
        Err(_) => Exit::Failed.code(),
    }
}

/// `check --replace`'s edits made in the checkout: its exit code.
fn replaced(place: &Place, pairs: &[String], color: bool) -> i32 {
    let done = replace::edits(pairs).and_then(|edits| replace::replace(&place.root, edits));
    match done {
        Ok(edited) => {
            let mut files: Vec<&str> = edited.items.iter().map(|it| it.file.as_str()).collect(); // alloc: CLI edge, the files named once
            files.sort_unstable();
            files.dedup();
            let rest = format!(
                "{} item(s) in {} file(s): {}",
                edited.items.len(),
                files.len(),
                files.join(", ")
            );
            blaze_report::err!("{}", say::status("Replaced", &rest, color));
            0
        }
        Err(what) => {
            blaze_report::err!("{}", say::error(&what, color));
            Exit::Failed.code()
        }
    }
}

/// What follows the build.
#[derive(Debug, PartialEq, Eq)]
pub enum Then {
    Exit(i32),
    /// `run`'s program, to become.
    Exec(run::Exec),
    /// `check --fix`: what the check said, for its suggestions to be made.
    Fix(fix::Found),
}

/// The request sent and its responses printed, then what `run` or `test` runs after it.
#[expect(
    clippy::too_many_arguments,
    reason = "the command, where it runs, how it speaks and what it speaks to"
)]
pub fn drive<A: Api, O: Write, S: show::Sink, P: Programs>(
    api: &mut A,
    asked: &Invocation,
    place: &Place,
    request: Request,
    shown: Shown,
    started: Instant,
    out: &mut O,
    err: &mut S,
    programs: &mut P,
) -> io::Result<Then> {
    let mut printer = Printer::new(shown, &place.root, out, err);
    printer.place(
        // The user's home, which the printer shortens paths under to `~`; with none, or one
        // that is not UTF-8, paths are printed whole.
        blaze_core::home::user().ok().and_then(|it| it.into_os_string().into_string().ok()),
        observe::last_failure(),
    );
    // alloc: once a request, for `--reached` and `--ub`; it grows by doubling, not by a result.
    let mut results: Vec<TestResult> = Vec::new();
    let mut unwritten = None;
    let mut docs = doc::Docs::default();
    let mut counted = asked.flags.baselines.is_some().then(baselines::Counted::default);
    let mut found = (asked.command() == Command::Check && asked.flags.fix)
        .then(|| fix::Found::new(&asked.flags.lints));
    // The client's one wait: on the daemon's socket, from the request sent to its `Done`.
    let waiting = tracing::info_span!(
        target: "blaze",
        "client: wait for the daemon's answer to the request",
        command = asked.command().name(),
    )
    .entered();
    // A `wait` row owned by the daemon (`owned_by = 'the daemon'`), so `blaze slow` follows
    // the critical path across it into the daemon's answer; the ad hoc span above stays for
    // the timeline.
    let called = blaze_graph::wait::wait_for_daemon("the daemon's answer to the request", || {
        api.call(request, |response, more| {
        if let ResponseRef::Diagnostic(diagnostic) = &response {
            if let Some(counted) = &mut counted {
                counted.add(diagnostic);
            }
            if let Some(found) = &mut found {
                found.add(diagnostic);
            }
        }
        if let ResponseRef::Work(worked) = &response {
            client_rows::heard(worked.build_id, worked.misses);
            client_rows::summarized(worked.build_id, worked.why.as_deref());
        }
        let printed = match response {
            ResponseRef::Documented(documented) => {
                let printed = documented
                    .crates
                    .iter()
                    .try_for_each(|it| printer.status("Documenting", &it.package));
                docs.documented(documented);
                printed
            }
            ResponseRef::DocPage { path, page } => {
                docs.page(path, page);
                Ok(())
            }
            ResponseRef::TestResult(result) => {
                // alloc: a result held to the end, where libtest's format groups them by
                // executable; only `--reached` and `--ub` have them.
                results.push(result.into_owned());
                Ok(())
            }
            response => printer.on(response),
        };
        // Written once nothing more is held: what the daemon sent in one burst is one write, and
        // a line never waits on the daemon's next.
        let written = printed.and_then(|()| match more {
            true => Ok(()),
            false => printer.draw(),
        });
        if let Err(error) = written {
            unwritten.get_or_insert(error);
        }
        })
    });
    drop(waiting);
    if let Some(error) = unwritten {
        return Err(error);
    }
    let ok = match called {
        // A build that left units unchecked behind a failure failed: the exit status is of what
        // was asked for, all of it built. A daemon that says `Done { ok: true }` after it named
        // them is wrong, and is said so, never believed (0.20261028.7 ended a `build` 0 with
        // both of its binaries unbuilt).
        Ok(true) if printer.left_unchecked() > 0 => {
            printer.flush()?;
            // alloc: an error, once
            let what = format!(
                "the daemon said the build passed after it left {} unit(s) unchecked or unbuilt \
                 behind a failure (named above): it failed, and the daemon's answer is a bug in blaze",
                printer.left_unchecked()
            );
            printer.err().write_all(say::error(&what, shown.color).as_bytes())?;
            false
        }
        Ok(ok) => ok && !printer.refused(),
        Err(error) => {
            printer.flush()?;
            // alloc: the error a command ends with, once
            let what = match daemon::hung_up(&error).then(|| api.died()).flatten() {
                // Stopped at its memory ceiling, or killed by the kernel at its limit: that is
                // the failure, said as itself, never as a hang-up.
                Some(Ok(died)) if died.at_ceiling().is_some() => died.to_string(),
                Some(Ok(died)) => format!("{error}: {died}"),
                Some(Err(why)) => {
                    format!("{error}, and what came of the daemon is not known: {why:#}")
                }
                None => error.to_string(),
            };
            printer.err().write_all(say::error(&what, shown.color).as_bytes())?;
            return Ok(Then::Exit(Exit::Failed.code()));
        }
    };
    // `blaze lock` built nothing: no `Finished`, only what it wrote (said as notes) or its error.
    if asked.lock {
        printer.flush()?;
        return Ok(Then::Exit(if ok { 0 } else { Exit::Failed.code() }));
    }
    // The pages are written before cargo's `Finished`, and a page not written fails the command.
    let documented = match (asked.command(), ok) {
        (Command::Doc, true) => match docs.finish() {
            Ok(documented) => Some(documented),
            Err(what) => {
                printer.finish(false, started.elapsed(), None, false)?;
                printer.err().write_all(say::error(&what, shown.color).as_bytes())?;
                return Ok(Then::Exit(Exit::Failed.code()));
            }
        },
        _ => None,
    };
    let why = match ok {
        true => why_recomputed(&mut printer, shown, asked.command() != Command::Run)?,
        false => None,
    };
    printer.finish(ok, started.elapsed(), why, false)?;
    // A check that failed on its findings still has its suggestions made: a deny lint's
    // finding is an error, and its fix takes it away.
    if let Some(found) = found
        && !printer.refused()
    {
        return Ok(Then::Fix(found));
    }
    // Unless `--fail-fast`, the tests of every unit that built still run when another failed
    // (each failure was said, naming its unit), and the run then fails.
    let tests_past_failure =
        !asked.flags.fail_fast && matches!(asked.command(), Command::Test | Command::Bench);
    if !ok && !tests_past_failure {
        return Ok(Then::Exit(Exit::Failed.code()));
    }
    let speaking = Speaking {
        cwd: &place.cwd,
        color: shown.color,
        quiet: shown.quiet,
        verbose: shown.verbose,
        json: shown.format.json(),
    };
    let artifacts = printer.take_artifacts();
    let unbuilt = printer.take_unbuilt();
    let members = printer.members();
    match asked.command() {
        Command::Build => Ok(Then::Exit(0)),
        Command::Check => {
            let (Some(counted), Some(dir)) = (counted, &asked.flags.baselines) else {
                return Ok(Then::Exit(0));
            };
            // args.rs refuses `--baselines` without exactly one `--target`.
            let target = asked.flags.targets.first().map_or("", String::as_str);
            match counted.finish(
                &place.root,
                dir,
                target,
                asked.flags.bless_baselines,
                &asked.flags.new_baselines,
            ) {
                Ok(said) => {
                    for line in said {
                        printer
                            .err()
                            .write_all(say::status("Baseline", &line, shown.color).as_bytes())?;
                    }
                    Ok(Then::Exit(0))
                }
                Err(what) => {
                    printer.err().write_all(say::error(&what, shown.color).as_bytes())?;
                    Ok(Then::Exit(Exit::Failed.code()))
                }
            }
        }
        Command::Doc => {
            if let Some(documented) = &documented {
                doc::done(documented, asked.flags.open, shown.quiet, shown.color, &mut printer.err())?;
            }
            Ok(Then::Exit(0))
        }
        Command::Run => {
            let exec = run::pick(&artifacts, &asked.flags).and_then(|it| {
                let mut exec = run::planned(it, &asked.program, &place.cwd)?;
                programs.launch(it, &mut exec)?;
                Ok(exec)
            });
            match exec {
                Ok(exec) => {
                    // The program's command line is said only when asked for (`-v`): its
                    // output is what a `run` is for.
                    if shown.verbose > 0 && !shown.quiet {
                        let line = format!("  running {}\n", exec.shown_at(shown.verbose)); // alloc: `-v`, once
                        printer.err().write_all(line.as_bytes())?;
                    }
                    // The build is over: nothing of it is left live when the program starts.
                    printer.clear()?;
                    Ok(Then::Exec(exec))
                }
                Err(what) => {
                    printer.err().write_all(say::error(&what, shown.color).as_bytes())?;
                    Ok(Then::Exit(Exit::Failed.code()))
                }
            }
        }
        Command::Test | Command::Bench if in_daemon(asked) => {
            let (out, mut err) = printer.streams();
            let passed = test::reached(&results, speaking, &mut *out, &mut err)?;
            let code = if passed && ok { 0 } else { Exit::Failed.code() };
            // The daemon ran them: the names it sent are the tests this run knows exist.
            let names = || test::Names {
                names: results.iter().map(|it| it.name.clone()).collect(), // clone: a refusal's help, once
                unlisted: Vec::new(),                                      // alloc: none
            };
            let code = test::conclude(
                &test::Selection::of(asked),
                &test::tally_of(&results),
                code,
                ok,
                started.elapsed(),
                names,
                speaking,
                &mut *out,
                &mut err,
            )?;
            Ok(Then::Exit(code))
        }
        Command::Test | Command::Bench => {
            // The test executables write for themselves from here.
            printer.clear()?;
            let began = Instant::now();
            let mut tally = blaze_screen::Tally::default();
            let code = test::run(
                asked,
                &artifacts,
                &unbuilt,
                members,
                speaking,
                &mut printer.err(),
                programs,
                &mut tally,
            )?;
            // A run of none is refused here, and the run's counts are its last line.
            let (out, mut err) = printer.streams();
            let code = test::conclude(
                &test::Selection::of(asked),
                &tally,
                code,
                ok,
                began.elapsed(),
                || test::names_of(&artifacts, programs),
                speaking,
                &mut *out,
                &mut err,
            )?;
            Ok(Then::Exit(if ok { code } else { code.max(Exit::Failed.code()) }))
        }
    }
}

#[cfg(test)]
mod tests;
