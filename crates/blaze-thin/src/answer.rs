//! What the daemon answers a build with, as the thin `blaze` takes it: the answer of a build
//! that went well with nothing to say but cargo's status lines ([`Plain`]), which it prints
//! itself, or the first response that is not such an answer's, at which it hands the whole
//! answer to the engine before it has written a byte ([`Step::HandOver`]).
//!
//! A plain answer is units fresh or compiled with no diagnostic, the build's outputs, its
//! work, and a `Done` that is ok. What it shows is then exactly what blaze's CLI shows of it,
//! made by the same core (blaze-screen, docs/terminal.md): on a terminal nothing for a `run`
//! that built nothing, one line for an edit or a `build`; off one the `blaze: ok` line. So an
//! edit's build that went well (a unit
//! compiled, its image patched) is answered here, and its program started, with no second
//! process: until 2026-11-11 a unit compiled handed the answer to the engine, which cost an
//! edit that program's start, a second connection for the launch and a third for its rows.
//! A diagnostic, a report, a fetch, an error, a failure: each is the engine's printer's, which
//! says more than this could without being that printer a second time. So is a build that
//! says why it recomputed what it did after `Finished` ([`Plain::of`]'s `reports`: a `build`
//! that is not quiet): that one is handed over at its first unit compiled, as it starts.
//!
//! A sans-IO core: responses in, a decision and the text out.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    time::Duration,
};

use blaze_api::{
    Artifact, LinkHow, Linked, Report, Response, Severity, TargetKind, Tiers, UnitStarted, Verb,
};
use blaze_report::terminal::Terminal;
use blaze_screen::{Built, Caps, Event, Link, Options, Screen, Why};

/// What taking a response came to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Step {
    /// Nothing to say of it; the answer goes on.
    More,
    /// The answer ended ok, and all of it was plain.
    Done,
    /// The response is not a plain answer's: the engine prints the answer, this one in it.
    HandOver(&'static str),
}

/// The most packages one answer's units may number theirs to, as the engine's printer holds
/// it (`blaze_cargo::cli::print`'s `IDS`): past it that printer says the daemon's error.
const IDS: u32 = 1 << 24;

/// A build's answer so far, every response of it plain.
#[derive(Debug, Default)]
pub struct Plain {
    /// The command says, after `Finished`, why the build recomputed what it did
    /// ([`Plain::of`]).
    reports: bool,
    /// The profile of [`Response::Built`], which `Finished` names; `None` until it came.
    profile: Option<String>,
    artifacts: Vec<Artifact>,
    links: Vec<Linked>,
    tiers: Option<Tiers>,
    /// The first unit compiled or checked of each package, in the order they came.
    compiled: Vec<UnitStarted>,
    /// Units made again and units replayed, as the build's work says
    /// ([`blaze_api::Worked::compiled`], [`blaze_api::Worked::found`]).
    made: u64,
    found: u64,
    /// Why the build made again what it did, when its answer carried it
    /// ([`blaze_api::Worked::why`]): what an edit's line names the functions and the cause from.
    why: Option<Vec<Report>>,
}

/// How a command shows its answer: the screen's options for it, what stderr is, and how a
/// program's size on disk is asked. The program makes one before it talks to the daemon
/// (`talk::Spoken::shown`).
#[derive(Clone, Debug)]
pub struct Shown {
    pub options: Options,
    pub caps: Caps,
    pub terminal: Terminal,
    pub size: fn(&str) -> Option<u64>,
}

impl Shown {
    /// A pipe's: lines that stay, no escape but a colour asked for by name (`color`), no size
    /// asked of any file. What a test speaks with.
    pub fn pipe(command: blaze_screen::Command, color: bool) -> Shown {
        Shown {
            options: Options::of(command),
            caps: Caps::PIPE,
            terminal: Terminal {
                caps: blaze_report::Caps { color, links: false, width: None },
                ..Terminal::PIPE
            },
            size: |_| None,
        }
    }
}

impl Plain {
    /// The answer of a command that says nothing of what its build recomputed: a `run`, or a
    /// quiet `build`.
    pub fn new() -> Plain {
        Plain::default()
    }

    /// The answer of a command; `reports` when it says, after `Finished`, why its build
    /// recomputed what it did (a `build` that is not quiet; a `run` starts its program
    /// instead, `blaze_cargo::cli::said_recomputed`). Those reports are the engine's
    /// printer's, so such an answer is handed over at its first unit compiled.
    pub fn of(reports: bool) -> Plain {
        Plain { reports, ..Plain::default() }
    }

    /// Takes the answer's next response.
    pub fn take(&mut self, response: Response) -> Step {
        match response {
            // Shown only under `-v`, which is the engine's.
            Response::Unit(unit) if unit.verb == Verb::Fresh => Step::More,
            Response::Unit(_) if self.reports => {
                Step::HandOver("a unit is compiled, and the build says why after it")
            }
            Response::Unit(unit) if unit.package >= IDS => {
                Step::HandOver("a package numbered past any request's")
            }
            // cargo's `Compiling foo v0.1.0 (/w/foo)`, once a package.
            Response::Unit(unit) => {
                if !self.compiled.iter().any(|it| it.package == unit.package) {
                    self.compiled.push(unit);
                }
                Step::More
            }
            // A unit's end says its warnings' count or that it could not compile: of a unit
            // with no diagnostic, nothing. A diagnostic or a report is handed over as it comes.
            Response::UnitEnded(_) => Step::More,
            // What a build script's run said (its record's, for a script kept as for one
            // run): the printer writes it only as a JSON message
            // (`blaze_cargo::cli::print`), and a command line with `--message-format` is the
            // engine's (`crate::route`). Every program's build has them: the standard
            // library's own build scripts are units of its plan.
            Response::BuildScript(_) => Step::More,
            Response::Built { profile, artifacts, .. } => {
                if self.profile.is_some() {
                    return Step::HandOver("a second `Built`");
                }
                self.profile = Some(profile);
                self.artifacts = artifacts;
                Step::More
            }
            Response::Work(worked) => {
                // What it computed again is said after `Finished`, as reports.
                let again = worked.misses != 0 || worked.why.as_ref().is_some_and(|it| !it.is_empty());
                if self.reports && again {
                    return Step::HandOver("the build computed something again");
                }
                self.links = worked.links;
                self.tiers = worked.tiers;
                self.made = worked.compiled;
                self.found = worked.found;
                self.why = worked.why;
                Step::More
            }
            Response::Done { ok: true } => Step::Done,
            Response::Done { ok: false } => Step::HandOver("the build failed"),
            _ => Step::HandOver("a response the engine's printer says"),
        }
    }

    /// What is written to stderr of this answer for a command that shows it as `shown` says,
    /// `quiet` its `-q`, the build over `took` after the command began.
    pub fn said(&self, shown: &Shown, quiet: bool, took: Duration) -> String {
        let options = Options { quiet, ..shown.options.clone() }; // clone: once a command
        let size = placed(&self.artifacts).and_then(shown.size);
        self.shown(options, shown.caps, &shown.terminal, took, size)
    }

    /// The frame blaze-screen's core makes of the build's end and its why, as the bytes
    /// `terminal` takes: what the engine's printer shows of the same answer
    /// (`blaze_cargo::cli::print`). Nothing where the screen says nothing (a `run` that built
    /// nothing on a terminal, `-q`). `size` is the placed program's size on disk.
    pub fn shown(
        &self,
        options: Options,
        caps: Caps,
        terminal: &Terminal,
        took: Duration,
        size: Option<u64>,
    ) -> String {
        let mut screen = Screen::new(options, caps);
        screen.advance_clock(u64::try_from(took.as_millis()).unwrap_or(u64::MAX));
        screen.take(Event::Built(Built {
            ok: true,
            compiled: self.made,
            cached: self.found,
            tier: self.tiers.as_ref().map(Tiers::status),
            // alloc: a link an image, once a command
            links: self
                .links
                .iter()
                .map(|it| Link {
                    whole: matches!(it.how, LinkHow::Full { .. }),
                    how: it.how.to_string(),
                    path: it.path.clone(), // clone: as above
                })
                .collect(),
            program: placed(&self.artifacts).map(str::to_owned), // alloc: a path, once a command
            size_bytes: size,
            why_follows: self.why.is_some(),
            ..Built::default()
        }));
        if let Some(why) = &self.why {
            screen.take(Event::Why(why.iter().map(why_of).collect())); // alloc: a cause each, once
        }
        let mut out = String::with_capacity(160); // alloc: once a command, what it prints
        if let Some(frame) = screen.frame() {
            blaze_screen::encode(&frame, terminal, &mut out);
        }
        out
    }

    /// What the build placed.
    pub fn artifacts(&self) -> &[Artifact] {
        &self.artifacts
    }

    /// The one program a plain `run` (no `--bin`, no `--example`) runs of what was built:
    /// the only artifact that runs, with its executable and no runner. `None` where cargo
    /// would refuse (none, several, a library example) or a runner starts it: the engine
    /// says which (`blaze_cargo::cli::run::pick`).
    pub fn program(&self) -> Option<&Artifact> {
        let mut runnable = self.artifacts.iter().filter(|it| {
            !it.profile.test && matches!(it.target.kind, TargetKind::Bin | TargetKind::Example)
        });
        let one = runnable.next()?;
        if runnable.next().is_some() || !one.runner.is_empty() {
            return None;
        }
        one.executable.as_ref().map(|_| one)
    }
}

/// The one program among what a build placed (`artifacts`), where it placed exactly one: its
/// path, which the build's line names. The thin `blaze` and the engine's printer ask this one
/// function, so they name the same.
pub fn placed(artifacts: &[Artifact]) -> Option<&str> {
    let mut programs = artifacts.iter().filter(|it| {
        !it.profile.test && matches!(it.target.kind, TargetKind::Bin | TargetKind::Example)
    });
    let one = programs.next()?;
    match programs.next() {
        Some(_) => None,
        None => one.executable.as_deref(),
    }
}

/// What the file at `path` takes on disk, in bytes: the blocks it holds, which is what a
/// person means by a program's size (a dev executable's length is mostly room for patches
/// that holds nothing). None where it can't be asked.
#[cfg(unix)]
pub fn on_disk(path: &str) -> Option<u64> {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::metadata(path).ok().map(|it| it.blocks().saturating_mul(512))
}

/// Off Unix a file's length is what there is to ask.
#[cfg(not(unix))]
pub fn on_disk(path: &str) -> Option<u64> {
    std::fs::metadata(path).ok().map(|it| it.len())
}

/// One cause of a build's why (`Rebuilt::summary`'s report of it), as the screen takes it:
/// its headline as the report has it, each part with its role; the cause's own words (what
/// follows `because` in the headline); the functions compiled again for it (its `codegen`
/// facts); and whether it is work that should not have been done (said as a warning). The
/// thin `blaze` and the engine's printer ask this one function.
pub fn why_of(report: &Report) -> Why {
    use blaze_report::Span;
    let mut cause = blaze_api::Text::new();
    let mut found = false;
    for span in &report.headline.0 {
        match span {
            Span::Plain(words) if !found => {
                if let Some((_, after)) = words.split_once(" because ") {
                    found = true;
                    if !after.is_empty() {
                        cause.push(Span::Plain(after.to_owned())); // alloc: once a cause
                    }
                }
            }
            span if found => cause.push(span.clone()), // clone: as above
            _ => {}
        }
    }
    let mut functions: Vec<String> = Vec::new(); // alloc: as above
    for fact in &report.facts {
        let text = fact.text.flat();
        let Some((step, names)) = text.split_once(": ") else { continue };
        if !step.starts_with("codegen") {
            continue;
        }
        for name in names.split(", ") {
            let name = name.trim_matches('`');
            // `main` beside `hello::main` is the program's entry, the same function.
            let last = |it: &str| it.rsplit("::").next().unwrap_or(it).to_owned();
            if !functions.iter().any(|it| it == name || last(it) == name) {
                functions.retain(|it| *it != last(name) || name == it);
                functions.push(name.to_owned()); // alloc: as above
            }
        }
    }
    Why {
        wasted: report.severity == Severity::Warning,
        cause,
        headline: report.headline.clone(), // clone: as above
        functions,
        more: report.rule.as_ref().map(|rule| format!("blaze explain {rule}")), // alloc: as above
    }
}

/// The executable as cargo runs it from `cwd`: below it, relative, and `./foo` for one in it.
pub fn shown_exe(path: &Path, cwd: &Path) -> PathBuf {
    match path.strip_prefix(cwd) {
        Ok(relative) if relative.file_name() == Some(relative.as_os_str()) => {
            Path::new(".").join(relative)
        }
        Ok(relative) => relative.to_owned(),
        Err(_) => path.to_owned(),
    }
}

/// The program a `run` becomes.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Exec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    /// What cargo sets, over the command's own environment.
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
}

impl Exec {
    /// `executable` (an artifact's, with no runner), run from `cwd` with `args` and the
    /// variables cargo sets for it (`env`).
    pub fn of(executable: &str, env: &[(String, String)], args: &[OsString], cwd: &Path) -> Exec {
        Exec {
            program: shown_exe(Path::new(executable), cwd),
            args: args.to_vec(), // clone: the program's arguments, once a run
            env: env.to_vec(),   // clone: cargo's variables for it, once a run
            cwd: cwd.to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use blaze_api::ApiError;

    use super::*;

    #[test]
    fn a_response_that_is_not_a_plain_answers_hands_over() {
        let mut plain = Plain::new();
        assert_eq!(plain.take(Response::Done { ok: false }), Step::HandOver("the build failed"));
        assert!(matches!(plain.take(Response::Error(ApiError::new("no", None))), Step::HandOver(_)));
        assert!(matches!(plain.take(Response::Flushed { seq: 1 }), Step::HandOver(_)));
        assert_eq!(plain.take(Response::Done { ok: true }), Step::Done);
    }

    /// The standard library's build scripts are units of every plan, so every answer has
    /// their `BuildScript` responses: an answer with them is still plain (nothing is printed
    /// of one outside JSON), or no build is ever answered here.
    #[test]
    fn what_a_build_script_said_is_plain() {
        let mut plain = Plain::new();
        let ran = blaze_api::BuildScriptRan {
            env: vec![("STD_ENV_ARCH".to_owned(), "aarch64".to_owned())],
            cfgs: vec!["backtrace_in_libstd".to_owned()],
            out_dir: "/std/build/std/1f/out".to_owned(),
            package_id: "path+file:///rustc/library/std#0.0.0".to_owned(),
            linked_libs: Vec::new(),
            linked_paths: Vec::new(),
        };
        assert_eq!(plain.take(Response::BuildScript(ran)), Step::More);
        assert_eq!(plain.take(Response::Done { ok: true }), Step::Done);
    }

    /// What a plain answer shows is the screen's: off a terminal the one `blaze: ok` line,
    /// with no escape byte; on one a line for a `build` and nothing for a `run`.
    #[test]
    fn a_plain_answer_says_what_the_screen_says() {
        use blaze_screen::Command;
        let mut plain = Plain::new();
        let built = Response::Built {
            members: 1,
            profile: "`dev` profile [unoptimized + debuginfo]".to_owned(),
            artifacts: Vec::new(),
        };
        assert_eq!(plain.take(built), Step::More);
        plain.links = vec![Linked { how: LinkHow::Unchanged, path: "/w/target/debug/a".to_owned() }];
        plain.found = 32;
        let took = Duration::from_millis(10);
        let piped = plain.shown(Options::of(Command::Build), Caps::PIPE, &Terminal::PIPE, took, None);
        assert_eq!(piped, "blaze: ok built=0 cached=32 warnings=0 elapsed_ms=10\n");
        let pipe = Shown::pipe(Command::Build, false);
        assert_eq!(plain.said(&pipe, false, took), piped);
        assert_eq!(plain.said(&pipe, true, took), "");
        let caps = Caps { live: true, cols: 80, rows: 24, unicode: true };
        let build = plain.shown(Options::of(Command::Build), caps, &Terminal::PIPE, took, None);
        assert!(build.contains("up to date · 10ms"), "{build:?}");
        assert_eq!(plain.shown(Options::of(Command::Run), caps, &Terminal::PIPE, took, None), "");
        assert_eq!(plain.program(), None);
        let again = Response::Built { members: 1, profile: String::new(), artifacts: Vec::new() };
        assert!(matches!(plain.take(again), Step::HandOver(_)));
    }

    fn compiling(package: u32) -> Response {
        Response::Unit(UnitStarted {
            name: "foo".to_owned(),
            verb: Verb::Compiling,
            source: Some("(/w/foo)".to_owned()),
            package,
            version: "0.1.0".to_owned(),
            package_id: "path+file:///w/foo#0.1.0".to_owned(),
        })
    }

    /// An edit's answer: a unit compiled is said once a package, and nothing is handed over.
    /// The control: the same unit of a command that reports what it recomputed is handed over.
    #[test]
    fn a_unit_compiled_is_said_once_a_package_unless_the_command_reports() {
        let mut plain = Plain::new();
        assert_eq!(plain.take(compiling(0)), Step::More);
        assert_eq!(plain.take(compiling(0)), Step::More);
        assert_eq!(plain.take(Response::Done { ok: true }), Step::Done);
        assert_eq!(plain.compiled.len(), 1);
        let pipe = Shown::pipe(blaze_screen::Command::Run, false);
        assert!(plain.said(&pipe, false, Duration::ZERO).starts_with("blaze: ok built=0 "));
        assert_eq!(plain.said(&pipe, true, Duration::ZERO), "");
        assert!(matches!(Plain::of(true).take(compiling(0)), Step::HandOver(_)));
        assert!(matches!(Plain::new().take(compiling(IDS)), Step::HandOver(_)));
    }

    #[test]
    fn the_program_is_shown_from_the_commands_directory() {
        let cwd = Path::new("/w");
        let exec = Exec::of("/w/target/debug/app", &[], &[OsString::from("--n=1")], cwd);
        assert_eq!(exec.program, Path::new("target/debug/app"));
        assert_eq!(shown_exe(Path::new("/w/app"), cwd), Path::new("./app"));
        assert_eq!(shown_exe(Path::new("/x/app"), cwd), Path::new("/x/app"));
    }
}
