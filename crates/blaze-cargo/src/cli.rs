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
//! - [`inspect`]: blaze's own looks at the last build (`codegen-inputs`, `trace`, `tables`),
//!   and `pack`, the in-browser compiler's pack of a program.
//! - [`observe`]: blaze's looks at itself: `explore`, `debug profile|live|sql`, `explain`.
//! - [`replace`]: `check --replace`, a path moved through the front end's name resolution.
//! - [`reach`]: `reach`, what the checkout's edits since a root reach, building only what its
//!   front end applies.
//! - [`baselines`]: `check --baselines`, a run's lint counts held to the checked-in ones.
//! - [`fix`]: `check --fix`, the check's machine-applicable suggestions made and checked.
//! - [`clean`]: `clean`, a workspace's build directory under blaze's root removed; `--list`
//!   and `--all`, every workspace's.
//! - [`cache`]: `cache gc`, one run of the collector of what blaze keeps on disk.

#![forbid(unsafe_code)]

pub mod args;
pub mod baselines;
pub mod cache;
pub mod clean;
pub mod client_rows;
pub mod daemon;
pub mod doc;
pub mod fetch_url;
pub mod fix;
pub mod inspect;
pub mod json;
pub mod live;
pub mod observe;
pub mod perf;
pub mod place;
#[cfg(feature = "native")]
pub mod pprof;
pub mod print;
pub mod reach;
pub mod replace;
pub mod run;
pub mod say;
pub mod terminal;
pub mod test;

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

const HELP: &str = "\
blaze: Rust's cargo, built by blaze's daemon

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
                under CI or `lock = \"required\"`; nothing is checked or built
    clean       Remove the workspace's build directory (under blaze's root, never in the checkout);
                `--list` every workspace's with its size, `--all` removes them all
    cache gc    Collect what blaze keeps on disk now (`--dry-run` removes nothing, `--last`
                prints the daemon's last runs)
    fmt         Format the workspace's packages with blaze's own rustfmt (`--check`, `-p NAME`,
                `--all`, `-- RUSTFMT_ARGS`), or the `.rs` files named; `blaze fmt --help`
    serve       Run the daemon the other commands are clients of
    stop        Make the daemon's state durable and stop it
    status      What the daemon is doing (`--json`: one object, docs/cargo.md)
    toolchain   Download the Rust blaze builds with, once (`--json`: its name, host and sysroot)
    daemon      install: the system starts the daemon on demand; uninstall
    codegen-inputs --format=json   Each instance the last build made, with its code hash
    trace --out PATH               Every result of the last build, with its edges, as CBOR
    tables --format=json           The size of each of the daemon's long-lived tables
    pack -p PACKAGE --out PATH     The program checked for wasm32, packed for the in-browser compiler
    explore [--dev] [--print]      The explorer: the daemon's builds and spans at https://ix.dev/explore
    debug profile [--seconds N]    The daemon's flight recorder as a Chrome trace
    debug live                     The daemon's threads as they run
    debug sql QUERY                SQL over the profile history, flushed first (every row recorded so far)
    explain RULE|PACKAGE|VIEW      A rule's long text, the last failure of PACKAGE with all it blocks, or a history view
    why [ITEM] [--all] [--json]    What the last build computed again instead of reusing, and why
    reach [ROOT] [--json]          What the edits since ROOT reach (items, functions, tests), and the root now
    slow [--build N] [--top N] [--json]   What made the last build slow, ranked by the time each fix saves
    pprof FILE|URL [top|peek|tree|list|flame|diff|export|--http]   Any program's profile (.ixprof or pprof), like `go tool pprof`
    perf record|runs|diff          The performance history of programs blaze built: what changed between runs, and why

Options:
    -V, --version  Print version info and exit
    -h, --help     Print help

Every command takes cargo's flags; `blaze help <COMMAND>` lists them.
";

/// What this client recorded of its build, sent to the daemon's history, and with
/// `BLAZE_PROFILE` the build's trace written (its path said on `err`): a failure is said on
/// `err` as a warning naming the step and the build, and the command's own outcome stands.
fn sent_client_rows(err: &mut impl Write, color: bool) {
    let said = match client_rows::send() {
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

/// The `blaze` program, and `cargo-blaze`, which cargo runs for `cargo blaze` as
/// `cargo-blaze blaze <command> ...`.
pub fn main(args: impl Iterator<Item = OsString>) -> ExitCode {
    blaze_profile::began();
    let words = words(args);
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
            eprint!("{}", say::error(&format!("blaze serve: {}", daemon::NO_DAEMON), auto_color()));
            return ended(Exit::Failed.code());
        }
    }
    // A test executable started through this blaze, to run as the process the daemon launched.
    if trampoline {
        return ended(run::trampoline(words.into_iter().skip(1), auto_color()));
    }
    ended(command(words))
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
            eprint!("{}", say::error(&why.to_string(), auto_color()));
            return Err(Exit::Usage.code());
        }
    };
    if let Err(why) = blaze_core::cores::lower(&cores) {
        eprint!("{}", say::warning(&why.to_string(), auto_color()));
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

/// One command line, run: its exit code.
pub fn command(words: Vec<OsString>) -> i32 {
    let asked = match args::parse(words) {
        Ok(asked) => asked,
        Err(usage) => {
            eprint!("{}", args::refusal(&usage, auto_color()));
            return Exit::Usage.code();
        }
    };
    let said = |result: anyhow::Result<i32>| match result {
        Ok(code) => code,
        Err(error) => {
            eprint!("{}", say::error(&format!("{error:#}"), auto_color()));
            Exit::Failed.code()
        }
    };
    match asked {
        Asked::Help(None) => {
            print!("{HELP}");
            0
        }
        Asked::Help(Some(command)) => {
            print!("{}", args::help(command));
            0
        }
        Asked::Version => {
            println!("{}", crate::version());
            0
        }
        Asked::Unknown(name) => {
            let what = format!(
                "no such command: `{name}`\n\nhelp: blaze's commands are build, check, run, test, bench, \
                 doc, clean, cache, fmt, serve, stop, status, toolchain, daemon, codegen-inputs, trace, tables, pack, explore, debug, \
                 explain, why, reach and slow"
            );
            eprint!("{}", say::error(&what, auto_color()));
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
                eprint!("{}", say::error(&what, auto_color()));
                Exit::Usage.code()
            }
        },
        Asked::Toolchain(words) => {
            let json = match json_flag("toolchain", words) {
                Ok(json) => json,
                Err(what) => {
                    eprint!("{}", say::error(&what, auto_color()));
                    return Exit::Usage.code();
                }
            };
            match crate::toolchain::ensure().and_then(|chosen| match json {
                true => crate::toolchain::json(&chosen),
                false => Ok(format!("{chosen}\n")),
            }) {
                Ok(text) => {
                    print!("{text}");
                    0
                }
                Err(what) => {
                    eprint!("{}", say::error(&what, auto_color()));
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
        Asked::Cargo(asked) => cargo(&asked),
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
                eprint!("{}", say::error(&what, auto_color()));
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

/// `build`, `check`, `run`, `test` or `bench`.
fn cargo(asked: &Invocation) -> i32 {
    let started = Instant::now();
    let refused = |what: &str, color: bool| {
        eprint!("{}", say::error(what, color));
        Exit::Failed.code()
    };
    // cargo's `cli::main`, then `GlobalContext::configure`, before the shell has its color.
    if let Some(directory) = &asked.directory {
        if !asked.unstable_options {
            let what = "the `-C` flag is unstable, pass `-Z unstable-options` on the nightly channel to enable it";
            return refused(what, auto_color());
        }
        if let Err(error) = std::env::set_current_dir(directory) {
            eprint!(
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
    let (place, mut request) = match prepared(asked, color) {
        Ok(prepared) => prepared,
        Err(what) => return refused(&what, color),
    };
    crate::toolchain::say_toolchain_file(&place.cwd, asked.flags.verbose > 0);
    if let Err(what) = crate::toolchain::ensure() {
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
    let mut client = match daemon::connect(Start::IfMissing) {
        Ok(Some(client)) => client,
        Ok(None) => return refused("no daemon answers, and none could be started", color),
        Err(error) => return refused(&format!("{error:#}"), color),
    };
    let stats = std::env::var_os("BLAZE_STATS").is_some();
    let fixing = pinned.is_some();
    let terminal = terminal::stderr(&config, color);
    let shown = Shown {
        format,
        quiet: asked.flags.quiet,
        verbose: asked.flags.verbose,
        color,
        stats,
        fixing,
        terminal,
    };
    // On a terminal, `build`, `check` and `test` draw the live view, whose screen thread
    // writes stderr: the locks below are taken only off that path.
    if live::wanted(asked, shown) {
        let then = live::drive(&mut client, asked, &place, request, shown, started);
        // The seed's one-shot engine stops here, writing its image; a daemon's connection just closes.
        let closed = client.close();
        sent_client_rows(&mut io::stderr().lock(), color);
        if let Err(error) = closed {
            return refused(&format!("closing the connection to the build engine: {error}"), color);
        }
        return match then {
            Ok(Then::Exit(code)) => code,
            Ok(Then::Exec(_) | Then::Fix(_)) => {
                refused("the live view ended in neither an exit code nor an error", color)
            }
            Err(error) => refused(&error.to_string(), color),
        };
    }
    let (mut out, mut err) = (io::stdout().lock(), io::stderr().lock());
    let then = drive(
        &mut client,
        asked,
        &place,
        request,
        shown,
        started,
        &mut out,
        &mut err,
        &mut test::Spawned,
    );
    // The seed's one-shot engine stops here, writing its image; a daemon's connection just closes.
    let closed = client.close();
    sent_client_rows(&mut err, color);
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
            eprint!("{}", say::status("Replaced", &rest, color));
            0
        }
        Err(what) => {
            eprint!("{}", say::error(&what, color));
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
pub fn drive<A: Api, O: Write, E: Write, P: Programs>(
    api: &mut A,
    asked: &Invocation,
    place: &Place,
    request: Request,
    shown: Shown,
    started: Instant,
    out: &mut O,
    err: &mut E,
    programs: &mut P,
) -> io::Result<Then> {
    let mut printer = Printer::new(shown, &place.root, out, err);
    printer.place(std::env::var("HOME").ok(), observe::last_failure());
    // alloc: once a request, for `--reached` and `--ub`; it grows by doubling, not by a result.
    let mut results: Vec<TestResult> = Vec::new();
    let mut unwritten = None;
    let mut docs = doc::Docs::default();
    let mut counted = asked.flags.baselines.is_some().then(baselines::Counted::default);
    let mut found = (asked.command() == Command::Check && asked.flags.fix)
        .then(|| fix::Found::new(&asked.flags.lints));
    let called = api.call(request, |response, more| {
        if let ResponseRef::Diagnostic(diagnostic) = &response {
            if let Some(counted) = &mut counted {
                counted.add(diagnostic);
            }
            if let Some(found) = &mut found {
                found.add(diagnostic);
            }
        }
        if let ResponseRef::Work(worked) = &response {
            client_rows::heard(worked.build_id);
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
            false => printer.flush(),
        });
        if let Err(error) = written {
            unwritten.get_or_insert(error);
        }
    });
    if let Some(error) = unwritten {
        return Err(error);
    }
    let ok = match called {
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
                printer.finish(false, started.elapsed())?;
                printer.err().write_all(say::error(&what, shown.color).as_bytes())?;
                return Ok(Then::Exit(Exit::Failed.code()));
            }
        },
        _ => None,
    };
    printer.finish(ok, started.elapsed())?;
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
                doc::done(documented, asked.flags.open, shown.quiet, shown.color, printer.err())?;
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
                    if !shown.quiet {
                        printer.err().write_all(
                            say::status("Running", &exec.shown_at(shown.verbose), shown.color)
                                .as_bytes(),
                        )?;
                    }
                    printer.err().flush()?;
                    Ok(Then::Exec(exec))
                }
                Err(what) => {
                    printer.err().write_all(say::error(&what, shown.color).as_bytes())?;
                    Ok(Then::Exit(Exit::Failed.code()))
                }
            }
        }
        Command::Test | Command::Bench if in_daemon(asked) => {
            let (out, err) = printer.streams();
            let passed = test::reached(&results, speaking, out, err)?;
            Ok(Then::Exit(if passed && ok { 0 } else { Exit::Failed.code() }))
        }
        Command::Test | Command::Bench => {
            let code =
                test::run(asked, &artifacts, &unbuilt, members, speaking, printer.err(), programs)?;
            Ok(Then::Exit(if ok { code } else { code.max(Exit::Failed.code()) }))
        }
    }
}

#[cfg(test)]
mod tests;
