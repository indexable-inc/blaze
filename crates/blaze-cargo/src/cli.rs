//! `blaze build|run|test|check|bench|doc` and `cargo blaze <the same>`, `blaze stop`, `blaze
//! status`: a client of the agent API with cargo's flags, output and exit codes
//! (docs/v2/plan/lanes/cargo/notes.md section 7). It reads no manifest and commits nothing (the
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
//! - [`baselines`]: `check --baselines`, a run's lint counts held to the checked-in ones.
//! - [`fix`]: `check --fix`, the check's machine-applicable suggestions made and checked.

#![forbid(unsafe_code)]

pub mod args;
pub mod baselines;
pub mod daemon;
pub mod doc;
pub mod fix;
pub mod inspect;
pub mod json;
pub mod observe;
pub mod place;
pub mod print;
pub mod replace;
pub mod run;
pub mod say;
pub mod test;

use std::{
    ffi::OsString,
    io::{self, IsTerminal as _, Write},
    process::ExitCode,
    time::Instant,
};

use blaze_api::{Cargo, Request, ResponseRef, TestResult, Ub, ViewRef};

use self::{
    args::{Asked, Command, Invocation},
    daemon::{Api, Start},
    place::Place,
    print::{Printer, Shown},
    test::{Programs, Speaking},
};

/// cargo's code for a build or test that failed, and for any error of its own.
pub const FAILED: i32 = 101;
/// clap's code for a command line it refuses.
pub const USAGE: i32 = 1;

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
    serve       Run the daemon the other commands are clients of
    stop        Make the daemon's state durable and stop it
    status      What the daemon is doing
    toolchain   Download the Rust blaze builds with, once, unless rustup has it
    daemon      install: the system starts the daemon on demand; uninstall
    codegen-inputs --format=json   Each instance the last build made, with its code hash
    trace --out PATH               Every result of the last build, with its edges, as CBOR
    tables --format=json           The size of each of the daemon's long-lived tables
    pack -p PACKAGE --out PATH     The program checked for wasm32, packed for the in-browser compiler
    explore [--dev] [--print]      The explorer: the daemon's builds and spans at https://ix.dev/explore
    debug profile [--seconds N]    The daemon's flight recorder as a Chrome trace
    debug live                     The daemon's threads as they run
    debug sql [--live] QUERY       SQL over the profile history (with `--live`, the flight recorder too)
    explain VIEW [NAME=VALUE ...]  One of the history's named views

Options:
    -V, --version  Print version info and exit
    -h, --help     Print help

Every command takes cargo's flags; `blaze help <COMMAND>` lists them.
";

/// The `blaze` program, and `cargo-blaze`, which cargo runs for `cargo blaze` as
/// `cargo-blaze blaze <command> ...`.
pub fn main(args: impl Iterator<Item = OsString>) -> ExitCode {
    blaze_profile::began();
    // The client and `blaze serve` both: with `BLAZE_PROFILE` each records from here on.
    blaze_profile::install();
    _ = blaze_profile::flight::count_frame_copies(|| {
        let it = blaze_mem::tag::copied_on_thread();
        blaze_profile::flight::FrameCopied { copies: it.copies, bytes: it.bytes }
    });
    _ = blaze_profile::flight::count_cpu(blaze_profile::thread_cpu);
    let words = words(args);
    if words.first().is_some_and(|it| it == "serve") {
        return crate::serve::main(words.into_iter().skip(1));
    }
    let code = command(words);
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

/// Whether stderr is colored when nothing says how: clap's choice for its own errors.
fn auto_color() -> bool {
    place::color(Some("auto"), |name| std::env::var(name).ok(), io::stderr().is_terminal()).unwrap_or(false)
}

/// One command line, run: its exit code.
pub fn command(words: Vec<OsString>) -> i32 {
    let asked = match args::parse(words) {
        Ok(asked) => asked,
        Err(usage) => {
            eprint!("{}", args::refusal(&usage, auto_color()));
            return USAGE;
        }
    };
    let said = |result: anyhow::Result<i32>| match result {
        Ok(code) => code,
        Err(error) => {
            eprint!("{}", say::error(&format!("{error:#}"), auto_color()));
            FAILED
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
                 doc, serve, stop, status, toolchain, daemon, codegen-inputs, trace, tables, pack, explore, debug \
                 and explain"
            );
            eprint!("{}", say::error(&what, auto_color()));
            FAILED
        }
        Asked::Stop => said(daemon::stop(&mut io::stderr().lock(), auto_color())),
        Asked::Status => said(daemon::status(&mut io::stdout().lock(), &mut io::stderr().lock(), auto_color())),
        Asked::Toolchain => match crate::toolchain::ensure() {
            Ok(chosen) => {
                println!("{chosen}");
                0
            }
            Err(what) => {
                eprint!("{}", say::error(&what, auto_color()));
                FAILED
            }
        },
        Asked::Daemon(verb) => said(daemon::units(verb, &mut io::stderr().lock(), auto_color())),
        Asked::Inspect(what, words) => inspect::main(what, words, auto_color()),
        Asked::Explore(words) => observe::explore(words, auto_color()),
        Asked::Debug(words) => observe::debug(words, auto_color()),
        Asked::Explain(words) => observe::explain(words, auto_color()),
        Asked::Cargo(asked) => cargo(&asked),
    }
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
        Command::Check => Request::Check { view, targets },
        Command::Doc => Request::Doc { view, targets, no_deps: flags.no_deps, private: flags.private },
        Command::Test | Command::Bench if in_daemon(asked) => {
            if flags.ub && flags.no_run {
                return Err("`--ub` runs the tests in blaze's evaluator, which `--no-run` says not to do".to_owned());
            }
            let ub = flags.ub.then_some(Ub { seed: flags.ub_seed, schedules: flags.ub_schedules });
            let args = asked.program.iter().map(|it| {
                it.to_str().map(str::to_owned).ok_or_else(|| {
                    format!("the test argument {} is not UTF-8, which a request to the daemon can't carry", it.to_string_lossy())
                })
            });
            let args = args.collect::<Result<_, _>>()?;
            Request::Test { view, targets, filter: asked.filter.iter().cloned().collect(), all: !flags.reached, args, ub }
        }
        Command::Build | Command::Run | Command::Test | Command::Bench => Request::Build { view, targets },
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
    let place = place::find(&cwd, asked.flags.manifest_path.as_deref(), place::script(&asked.args))?;
    let home = blaze_registry::cargo_home();
    let mut files = place::config_files(&place, &asked.flags.config, home.as_deref())?;
    if asked.command() == Command::Check {
        files.extend(place::clippy_files(&place)?);
    }
    let width = place::width(|name| std::env::var(name).ok(), io::stderr().is_terminal(), place::stderr_columns)?;
    let cargo = place::cargo(asked, &place, env, files, color, width)?;
    Ok((place, cargo))
}

/// `build`, `check`, `run`, `test` or `bench`.
fn cargo(asked: &Invocation) -> i32 {
    let started = Instant::now();
    let refused = |what: &str, color: bool| {
        eprint!("{}", say::error(what, color));
        FAILED
    };
    // cargo's `cli::main`, then `GlobalContext::configure`, before the shell has its color.
    if let Some(directory) = &asked.directory {
        if !asked.unstable_options {
            let what = "the `-C` flag is unstable, pass `-Z unstable-options` on the nightly channel to enable it";
            return refused(what, auto_color());
        }
        if let Err(error) = std::env::set_current_dir(directory) {
            eprint!("{}", run::caused("could not change to requested directory", &[error.to_string()], auto_color()));
            return FAILED;
        }
    }
    if asked.flags.verbose > 0 && asked.flags.quiet {
        return refused("cannot set both --verbose and --quiet", auto_color());
    }
    let var = |name: &str| std::env::var(name).ok();
    let color = match place::color(asked.flags.color.as_deref(), var, io::stderr().is_terminal()) {
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
    let shown = Shown { format, quiet: asked.flags.quiet, verbose: asked.flags.verbose, color, stats };
    let (mut out, mut err) = (io::stdout().lock(), io::stderr().lock());
    let then = drive(&mut client, asked, &place, request, shown, started, &mut out, &mut err, &mut test::Spawned);
    drop(client);
    // The daemon wrote its part before its last response, so every part is in by now.
    if blaze_profile::on() {
        blaze_profile::note("client", blaze_profile::CLIENT, blaze_profile::began());
        blaze_profile::finish();
    }
    match then {
        Ok(Then::Exit(code)) => code,
        Ok(Then::Exec(exec)) => {
            drop(out);
            run::exec(&exec, &mut err, run::Told { color, quiet: shown.quiet, verbose: shown.verbose })
        }
        Ok(Then::Fix(found)) => {
            let Some(pinned) = &pinned else { return refused("`--fix` was not asked for", color) };
            match fix::run(found, pinned, &place.root) {
                Ok(fixed) => {
                    let files = match fixed.files.is_empty() {
                        true => String::new(),
                        false => format!(": {}", fixed.files.join(", ")),
                    };
                    let rest = format!("{} suggestion(s) in {} file(s){files}", fixed.made, fixed.files.len());
                    let _ = err.write_all(say::status("Fixed", &rest, color).as_bytes());
                    if fixed.left > 0 {
                        let left = format!("{} suggestion(s) are not machine-applicable, or are outside the checkout", fixed.left);
                        let _ = err.write_all(say::status("Left", &left, color).as_bytes());
                    }
                    if fixed.errors == 0 { 0 } else { FAILED }
                }
                Err(what) => {
                    let _ = err.write_all(say::error(&what, color).as_bytes());
                    FAILED
                }
            }
        }
        // stdout or stderr closed under it: nothing more can be said.
        Err(_) => FAILED,
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
            let rest = format!("{} item(s) in {} file(s): {}", edited.items.len(), files.len(), files.join(", "));
            eprint!("{}", say::status("Replaced", &rest, color));
            0
        }
        Err(what) => {
            eprint!("{}", say::error(&what, color));
            FAILED
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
#[expect(clippy::too_many_arguments, reason = "the command, where it runs, how it speaks and what it speaks to")]
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
    // alloc: once a request, for `--reached` and `--ub`; it grows by doubling, not by a result.
    let mut results: Vec<TestResult> = Vec::new();
    let mut unwritten = None;
    let mut docs = doc::Docs::default();
    let mut counted = asked.flags.baselines.is_some().then(baselines::Counted::default);
    let mut found = (asked.command() == Command::Check && asked.flags.fix).then(|| fix::Found::new(&asked.flags.lints));
    let called = api.call(request, |response, more| {
        if let ResponseRef::Diagnostic(diagnostic) = &response {
            if let Some(counted) = &mut counted {
                counted.add(diagnostic);
            }
            if let Some(found) = &mut found {
                found.add(diagnostic);
            }
        }
        let printed = match response {
            ResponseRef::Documented(documented) => {
                let printed = documented.crates.iter().try_for_each(|it| printer.status("Documenting", &it.package));
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
                Some(Ok(died)) => format!("{error}: {died}"),
                Some(Err(why)) => format!("{error}, and what came of the daemon is not known: {why:#}"),
                None => error.to_string(),
            };
            printer.err().write_all(say::error(&what, shown.color).as_bytes())?;
            return Ok(Then::Exit(FAILED));
        }
    };
    // The pages are written before cargo's `Finished`, and a page not written fails the command.
    let documented = match (asked.command(), ok) {
        (Command::Doc, true) => match docs.finish() {
            Ok(documented) => Some(documented),
            Err(what) => {
                printer.finish(false, started.elapsed())?;
                printer.err().write_all(say::error(&what, shown.color).as_bytes())?;
                return Ok(Then::Exit(FAILED));
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
    if !ok {
        return Ok(Then::Exit(FAILED));
    }
    let speaking =
        Speaking { cwd: &place.cwd, color: shown.color, quiet: shown.quiet, verbose: shown.verbose, json: shown.format.json() };
    let artifacts = printer.take_artifacts();
    let members = printer.members();
    match asked.command() {
        Command::Build => Ok(Then::Exit(0)),
        Command::Check => {
            let (Some(counted), Some(dir)) = (counted, &asked.flags.baselines) else { return Ok(Then::Exit(0)) };
            match counted.finish(&place.root, dir, asked.flags.bless_baselines, &asked.flags.new_baselines) {
                Ok(said) => {
                    for line in said {
                        printer.err().write_all(say::status("Baseline", &line, shown.color).as_bytes())?;
                    }
                    Ok(Then::Exit(0))
                }
                Err(what) => {
                    printer.err().write_all(say::error(&what, shown.color).as_bytes())?;
                    Ok(Then::Exit(FAILED))
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
            let exec = run::pick(&artifacts, &asked.flags).and_then(|it| run::planned(it, &asked.program, &place.cwd));
            match exec {
                Ok(exec) => {
                    if !shown.quiet {
                        printer.err().write_all(say::status("Running", &exec.shown_at(shown.verbose), shown.color).as_bytes())?;
                    }
                    printer.err().flush()?;
                    Ok(Then::Exec(exec))
                }
                Err(what) => {
                    printer.err().write_all(say::error(&what, shown.color).as_bytes())?;
                    Ok(Then::Exit(FAILED))
                }
            }
        }
        Command::Test | Command::Bench if in_daemon(asked) => {
            let (out, err) = printer.streams();
            let passed = test::reached(&results, speaking, out, err)?;
            Ok(Then::Exit(if passed { 0 } else { FAILED }))
        }
        Command::Test | Command::Bench => {
            test::run(asked, &artifacts, members, speaking, printer.err(), programs).map(Then::Exit)
        }
    }
}

#[cfg(test)]
mod tests;
