//! `blaze-fast`: `build`, `check`, `test`, `why` and `status` asked of the daemon by a program
//! that is only the client library (docs/client.md). It links none of the compiler, so it
//! starts in the time a small program starts in; a warm request is then one round trip on the
//! daemon's socket.
//!
//! It reads the command and the options listed in [`USAGE`], and nothing else: a word it does
//! not know is refused by name, saying to use `blaze`. It never runs `blaze` in its place (rule
//! no-fallback); the one time it starts `blaze` is to start the daemon when none listens.
//!
//! It prints each diagnostic as the daemon rendered it (rustc's text, which is what `blaze`
//! prints), each of blaze's own reports through blaze-report's renderer (the CLI's), and
//! cargo's status lines for the units, the fetches and the end. The CLI's printer
//! (`crates/blaze-cargo/src/cli/print.rs`) says more that this does not: the per-unit
//! "generated N warnings" lines, the live view, `--message-format`. With `--json` every
//! response is one line of the API's JSON on stdout and nothing else is printed.

use std::{
    ffi::OsString,
    io::{IsTerminal as _, Write as _},
    path::PathBuf,
    process::ExitCode,
    time::{Duration, Instant},
};

use blaze_api::{Envelope, FetchVerb, Response, TestOutcome, Verb, json};
use blaze_client::{Client, Connect, Error, Options, Start, WhyOptions, place};
use blaze_report::{Caps, Report, Roots};

const USAGE: &str = "\
blaze-fast: build, check, test, why and status asked of blaze's daemon, with no compiler linked

usage: blaze-fast [--home DIR] [--blaze PROGRAM] [--within SECONDS] [--json] COMMAND [OPTIONS]

  build | check | test [TESTNAME] [-- LIBTEST ARGS]
      -p, --package NAME      --features LIST, -F LIST   --all-features
      --no-default-features   -r, --release              --profile NAME
      --target TRIPLE         --manifest-path PATH       -v, --verbose
      -q, --quiet             --color auto|always|never  --workspace
      --all-targets --lib --bins --tests --examples --benches
      --bin NAME --test NAME --example NAME --bench NAME --exclude NAME
      --locked --offline --frozen --keep-going -j, --jobs N
      test only: --reached (only the tests the change reached)
      `test` runs the tests in the daemon; `blaze test` runs the executables itself.
  why [ITEM] [--all] [--unexplained]
  status

  --within SECONDS   how long the request may take (3600 for a build, a check and a test,
                     60 for why and status)
  --json             each response as one line of the API's JSON on stdout

Anything else is `blaze`'s: this program stops, naming the word.";

/// cargo's exit code of a command that failed.
const FAILED: u8 = 101;

/// What the command line asked.
#[derive(Debug)]
struct Asked {
    home: Option<PathBuf>,
    blaze: Option<PathBuf>,
    within: Option<Duration>,
    json: bool,
    command: Command,
}

#[derive(Debug)]
enum Command {
    Cargo { method: Method, options: Box<Options>, quiet: bool, color: Option<String> },
    Why(WhyOptions),
    Status,
    Help,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Method {
    Build,
    Check,
    Test,
}

/// The words cargo reads that are passed on as they are: flags, then those that take a value.
const FLAGS: [&str; 11] = [
    "--workspace", "--all-targets", "--lib", "--bins", "--tests", "--examples", "--benches",
    "--locked", "--offline", "--frozen", "--keep-going",
];
const VALUED: [&str; 7] = ["--bin", "--test", "--example", "--bench", "--exclude", "-j", "--jobs"];

fn refused(word: &str, of: &str) -> String {
    format!("blaze-fast does not handle `{word}`{of}: use `blaze` (blaze-fast --help lists what it handles)")
}

fn text(word: OsString) -> Result<String, String> {
    word.into_string().map_err(|it| format!("the argument {} is not UTF-8", it.to_string_lossy()))
}

/// Reads the command line. A flag's value is the next word, or what follows its `=`.
fn asked(words: impl Iterator<Item = OsString>, cwd: PathBuf) -> Result<Asked, String> {
    let mut words = words.map(text).collect::<Result<Vec<_>, _>>()?.into_iter();
    let mut asked = Asked { home: None, blaze: None, within: None, json: false, command: Command::Help };
    // The words before the command.
    let command = loop {
        let Some(word) = words.next() else { return Ok(asked) };
        let (flag, attached) = match word.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => (flag.to_owned(), Some(value.to_owned())),
            _ => (word.clone(), None),
        };
        let mut value = || {
            attached.clone().or_else(|| words.next()).ok_or_else(|| format!("`{flag}` takes a value"))
        };
        match flag.as_str() {
            "--home" => asked.home = Some(PathBuf::from(value()?)),
            "--blaze" => asked.blaze = Some(PathBuf::from(value()?)),
            "--within" => {
                let said = value()?;
                let seconds: u64 = said
                    .parse()
                    .map_err(|_| format!("`--within {said}` is no number of seconds"))?;
                asked.within = Some(Duration::from_secs(seconds));
            }
            "--json" => asked.json = true,
            "-h" | "--help" | "help" => return Ok(asked),
            _ if word.starts_with('-') => return Err(refused(&word, " before the command")),
            _ => break word,
        }
    };
    let method = match command.as_str() {
        "build" | "b" => Method::Build,
        "check" | "c" => Method::Check,
        "test" | "t" => Method::Test,
        "status" => {
            if let Some(word) = words.next() {
                return Err(refused(&word, " of `status`"));
            }
            asked.command = Command::Status;
            return Ok(asked);
        }
        "why" => {
            let cwd = place::utf8(&cwd)?;
            let mut options =
                WhyOptions { all: false, cwd: Some(cwd), item: None, within_ms: 0, unexplained: false };
            for word in words {
                match word.as_str() {
                    "--all" => options.all = true,
                    "--unexplained" => options.unexplained = true,
                    _ if word.starts_with('-') || options.item.is_some() => {
                        return Err(refused(&word, " of `why`"));
                    }
                    _ => options.item = Some(word),
                }
            }
            asked.command = Command::Why(options);
            return Ok(asked);
        }
        other => return Err(refused(other, " as a command")),
    };
    let of = format!(" of `{command}`");
    let mut options = Options::new(cwd, 0);
    let (mut quiet, mut color) = (false, None);
    while let Some(word) = words.next() {
        if word == "--" {
            if method != Method::Test {
                return Err(refused("--", &of));
            }
            options.test_args.extend(words.by_ref());
            break;
        }
        let (flag, attached) = match word.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => (flag.to_owned(), Some(value.to_owned())),
            _ => (word.clone(), None),
        };
        let mut value = || {
            attached.clone().or_else(|| words.next()).ok_or_else(|| format!("`{flag}` takes a value"))
        };
        match flag.as_str() {
            "-p" | "--package" => options.packages.push(value()?),
            "-F" | "--features" => {
                let list = value()?;
                let named = list.split([',', ' ']).filter(|it| !it.is_empty());
                options.features.extend(named.map(str::to_owned));
            }
            "--all-features" => options.all_features = true,
            "--no-default-features" => options.no_default_features = true,
            "-r" | "--release" => options.profile = Some("release".to_owned()),
            "--profile" => options.profile = Some(value()?),
            "--target" => options.target = Some(value()?),
            "--manifest-path" => options.manifest_path = Some(value()?),
            "-v" | "--verbose" => options.verbose += 1,
            "-q" | "--quiet" => {
                quiet = true;
                options.args.push(word);
            }
            "--color" => color = Some(value()?),
            "--reached" if method == Method::Test => options.reached = true,
            _ if FLAGS.contains(&flag.as_str()) && attached.is_none() => options.args.push(word),
            _ if VALUED.contains(&flag.as_str()) => {
                let value = value()?;
                options.args.push(flag.clone()); // clone: the word passed on
                options.args.push(value);
            }
            _ if !word.starts_with('-') && method == Method::Test && options.filter.is_empty() => {
                options.filter.push(word);
            }
            _ => return Err(refused(&word, &of)),
        }
    }
    if quiet && options.verbose > 0 {
        return Err("cannot set both --verbose and --quiet".to_owned());
    }
    asked.command = Command::Cargo { method, options: Box::new(options), quiet, color };
    Ok(asked)
}

/// Whether stderr is colored: `--color`, else `CARGO_TERM_COLOR`, else for a terminal as
/// anstream chooses (`NO_COLOR`, `CLICOLOR_FORCE`, `CLICOLOR`, `TERM`, `CI`); the rule of
/// blaze-cargo's `cli::place::color`.
fn colored(flag: Option<&str>, var: impl Fn(&str) -> Option<String>, terminal: bool) -> Result<bool, String> {
    let when = flag.map(str::to_owned).or_else(|| var("CARGO_TERM_COLOR"));
    match when.as_deref() {
        Some("always") => Ok(true),
        Some("never") => Ok(false),
        Some("auto") | None => {
            let clicolor = var("CLICOLOR").map(|it| it != "0");
            if var("NO_COLOR").is_some_and(|it| !it.is_empty()) {
                return Ok(false);
            }
            if var("CLICOLOR_FORCE").is_some_and(|it| it != "0") {
                return Ok(true);
            }
            if clicolor == Some(false) {
                return Ok(false);
            }
            let term = var("TERM").is_some_and(|it| it != "dumb");
            Ok(terminal && (term || clicolor == Some(true) || var("CI").is_some()))
        }
        Some(other) => {
            Err(format!("argument for --color must be auto, always, or never, but found `{other}`"))
        }
    }
}

/// A status line as cargo writes one: `word` right-aligned in twelve columns, bold green.
fn status(word: &str, rest: &str, color: bool) -> String {
    match color {
        true => format!("\x1b[1m\x1b[92m{word:>12}\x1b[0m {rest}\n"),
        false => format!("{word:>12} {rest}\n"),
    }
}

/// A time as cargo's `util::elapsed` writes it.
fn elapsed(took: Duration) -> String {
    let secs = took.as_secs();
    match secs >= 60 {
        true => format!("{}m {:02}s", secs / 60, secs % 60),
        false => format!("{secs}.{:02}s", took.subsec_nanos() / 10_000_000),
    }
}

fn said(report: &Report, color: bool) -> String {
    blaze_report::render(report, Caps { color, links: false, width: None }, &Roots::NONE)
}

fn error(what: &str, color: bool) -> String {
    said(&Report::text(what, None), color)
}

/// What a build's responses print as, on stderr.
struct Printer {
    color: bool,
    quiet: bool,
    verbose: bool,
    profile: Option<String>,
    passed: u32,
    failed: u32,
    ignored: u32,
}

impl Printer {
    fn on(&mut self, response: &Response) -> String {
        match response {
            Response::Diagnostic(diagnostic) => diagnostic.rendered.clone(), // clone: the line printed
            Response::Report { report, .. } => said(report, self.color),
            Response::Error(error) => said(error.report(), self.color),
            Response::Unit(unit) if !self.quiet => {
                let verb = match unit.verb {
                    Verb::Compiling => "Compiling",
                    Verb::Checking => "Checking",
                    Verb::Fresh if self.verbose => "Fresh",
                    Verb::Fresh => return String::new(),
                };
                let source = unit.source.as_ref().map(|it| format!(" {it}")).unwrap_or_default();
                status(verb, &format!("{} v{}{source}", unit.name, unit.version), self.color)
            }
            Response::Fetched { verb, what } if !self.quiet => {
                let verb = match verb {
                    FetchVerb::Updating => "Updating",
                    FetchVerb::Downloaded => "Downloaded",
                };
                status(verb, what, self.color)
            }
            Response::Built { profile, .. } => {
                self.profile = Some(profile.clone()); // clone: kept for the last line
                String::new()
            }
            Response::TestResult(result) => {
                let (word, shown) = match result.outcome {
                    TestOutcome::Passed => ("ok", false),
                    TestOutcome::Failed => ("FAILED", true),
                    TestOutcome::Ignored => ("ignored", false),
                    TestOutcome::Unreached => ("unreached", false),
                };
                match result.outcome {
                    TestOutcome::Passed | TestOutcome::Unreached => self.passed += 1,
                    TestOutcome::Failed => self.failed += 1,
                    TestOutcome::Ignored => self.ignored += 1,
                }
                let mut line = format!("test {} ... {word}\n", result.name);
                if shown {
                    line.push_str(&String::from_utf8_lossy(&result.output));
                }
                line
            }
            _ => String::new(),
        }
    }
}

fn run(asked: Asked) -> Result<u8, String> {
    let var = |name: &str| std::env::var(name).ok();
    let terminal = std::io::stderr().is_terminal();
    let connect = Connect {
        home: asked.home.as_deref(),
        blaze: asked.blaze.as_deref(),
        start: Start::IfMissing,
        // A daemon's cold start reads its image: as long as the CLI gives a phase of it.
        within: Duration::from_secs(60),
    };
    let said_error = |error: Error| error.to_string();
    let within = |default: u64| asked.within.unwrap_or(Duration::from_secs(default));
    let mut out = std::io::stdout().lock();
    let line = |out: &mut std::io::StdoutLock<'_>, text: &str| {
        out.write_all(text.as_bytes()).and_then(|()| out.write_all(b"\n")).map_err(|it| it.to_string())
    };
    match asked.command {
        Command::Help => {
            line(&mut out, USAGE)?;
            Ok(0)
        }
        Command::Status => {
            let mut client = Client::connect_with(&connect).map_err(said_error)?;
            let status = client.status(within(60)).map_err(said_error)?;
            line(&mut out, &sonic_rs::to_string(&status).map_err(|it| it.to_string())?)?;
            Ok(0)
        }
        Command::Why(mut options) => {
            options.within_ms = within(60).as_millis() as u64;
            let mut client = Client::connect_with(&connect).map_err(said_error)?;
            let rebuilt = client.why(&options).map_err(said_error)?;
            line(&mut out, &sonic_rs::to_string(&rebuilt).map_err(|it| it.to_string())?)?;
            Ok(0)
        }
        Command::Cargo { method, mut options, quiet, color } => {
            let started = Instant::now();
            options.color = colored(color.as_deref(), var, terminal)?;
            options.width = match var("__CARGO_TEST_TTY_WIDTH_DO_NOT_USE_THIS") {
                Some(width) => Some(width.parse().map_err(|_| format!("`{width}` is no number of columns"))?),
                None => terminal.then(place::stderr_columns).flatten().map(u32::from),
            };
            options.within_ms = within(3600).as_millis() as u64;
            let mut printer = Printer {
                color: options.color,
                quiet,
                verbose: options.verbose > 0,
                profile: None,
                passed: 0,
                failed: 0,
                ignored: 0,
            };
            let mut client = Client::connect_with(&connect).map_err(said_error)?;
            let mut err = std::io::stderr().lock();
            let mut unwritten = None;
            let mut id = 0u64;
            let on_event = |response: &Response| {
                let written = match asked.json {
                    true => {
                        id += 1;
                        // clone: the envelope written owns its body.
                        let envelope = Envelope::new(id, response.clone());
                        line(&mut out, &json::response(&envelope))
                    }
                    false => err.write_all(printer.on(response).as_bytes()).map_err(|it| it.to_string()),
                };
                if let Err(error) = written {
                    unwritten.get_or_insert(error);
                }
            };
            let outcome = match method {
                Method::Build => client.build(&options, on_event),
                Method::Check => client.check(&options, on_event),
                Method::Test => client.test(&options, on_event),
            }
            .map_err(said_error)?;
            if let Some(error) = unwritten {
                return Err(error);
            }
            if asked.json {
                let done = Envelope::new(id + 1, Response::Done { ok: outcome.ok });
                line(&mut out, &json::response(&done))?;
                return Ok(if outcome.ok { 0 } else { FAILED });
            }
            let mut last = String::new();
            if method == Method::Test && !outcome.tests.is_empty() {
                last.push_str(&format!(
                    "\ntest result: {}. {} passed; {} failed; {} ignored\n",
                    if printer.failed == 0 { "ok" } else { "FAILED" },
                    printer.passed,
                    printer.failed,
                    printer.ignored
                ));
            }
            match (outcome.ok, &printer.profile) {
                (true, Some(profile)) if !quiet => {
                    let rest = format!("{profile} target(s) in {}", elapsed(started.elapsed()));
                    last.push_str(&status("Finished", &rest, options.color));
                }
                (true, _) => {}
                (false, _) if outcome.errors.is_empty() && outcome.diagnostics.is_empty() => {
                    last.push_str(&error("the daemon said the request failed and said no more", options.color));
                }
                (false, _) => {}
            }
            err.write_all(last.as_bytes()).map_err(|it| it.to_string())?;
            Ok(if outcome.ok { 0 } else { FAILED })
        }
    }
}

fn main() -> ExitCode {
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(why) => {
            eprint!("{}", error(&format!("the current directory: {why}"), false));
            return ExitCode::from(FAILED);
        }
    };
    let asked = match asked(std::env::args_os().skip(1), cwd) {
        Ok(asked) => asked,
        Err(what) => {
            eprint!("{}", error(&what, false));
            return ExitCode::from(2);
        }
    };
    match run(asked) {
        Ok(code) => ExitCode::from(code),
        Err(what) => {
            eprint!("{}", error(&what, false));
            ExitCode::from(FAILED)
        }
    }
}
