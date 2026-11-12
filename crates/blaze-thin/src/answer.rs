//! What the daemon answers a build with, as the thin `blaze` takes it: the answer of a build
//! that had nothing to say ([`Plain`]), which it prints itself, or the first response that is
//! not such an answer's, at which it hands the whole answer to the engine before it has
//! written a byte ([`Step::HandOver`]).
//!
//! A plain answer is every unit fresh, the build's outputs, its work with nothing computed
//! again, and a `Done` that is ok. What it shows is then exactly what blaze's CLI shows of
//! it, made by the same core (blaze-screen, docs/terminal.md): on a terminal nothing for a
//! `run` and one dim line for a `build`; off one the `blaze: ok` line. A diagnostic, a unit
//! compiled, a report, a fetch, an error, a build that recomputed anything: each is the
//! engine's printer's, which says more than this could without being that printer a second
//! time.
//!
//! A sans-IO core: responses in, a decision and the text out.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    time::Duration,
};

use blaze_api::{Artifact, LinkHow, Linked, Response, TargetKind, Tiers, Verb};
use blaze_report::terminal::Terminal;
use blaze_screen::{Built, Caps, Event, Link, Options, Screen};

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

/// A build's answer so far, every response of it plain.
#[derive(Debug, Default)]
pub struct Plain {
    /// The profile of [`Response::Built`], which `Finished` names; `None` until it came.
    profile: Option<String>,
    artifacts: Vec<Artifact>,
    links: Vec<Linked>,
    tiers: Option<Tiers>,
    /// Units replayed from the store ([`blaze_api::Worked::found`]).
    found: u64,
}

impl Plain {
    pub fn new() -> Plain {
        Plain::default()
    }

    /// Takes the answer's next response.
    pub fn take(&mut self, response: Response) -> Step {
        match response {
            // Shown only under `-v`, which is the engine's.
            Response::Unit(unit) if unit.verb == Verb::Fresh => Step::More,
            Response::Unit(_) => Step::HandOver("a unit is compiled"),
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
                if worked.misses != 0 || worked.why.as_ref().is_some_and(|it| !it.is_empty()) {
                    return Step::HandOver("the build computed something again");
                }
                self.links = worked.links;
                self.tiers = worked.tiers;
                self.found = worked.found;
                Step::More
            }
            Response::Done { ok: true } => Step::Done,
            Response::Done { ok: false } => Step::HandOver("the build failed"),
            _ => Step::HandOver("a response the engine's printer says"),
        }
    }

    /// What blaze's CLI writes to stderr of this answer, the build over `took` after the
    /// command began: the frame blaze-screen's core makes of the build's end, as the bytes
    /// `terminal` takes. Nothing where the screen says nothing (a `run`
    /// on a terminal, `-q`).
    pub fn said(&self, options: Options, caps: Caps, terminal: &Terminal, took: Duration) -> String {
        let mut screen = Screen::new(options, caps);
        screen.advance_clock(u64::try_from(took.as_millis()).unwrap_or(u64::MAX));
        screen.take(Event::Built(Built {
            ok: true,
            compiled: 0,
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
            ..Built::default()
        }));
        let mut out = String::with_capacity(160); // alloc: once a command, what it prints
        if let Some(frame) = screen.frame() {
            blaze_screen::encode(&frame, terminal, &mut out);
        }
        out
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
    /// with no escape byte; on one a dim line for a `build` and nothing for a `run`.
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
        let piped = plain.said(Options::of(Command::Build), Caps::PIPE, &Terminal::PIPE, took);
        assert_eq!(piped, "blaze: ok built=0 cached=32 warnings=0 elapsed_ms=10\n");
        let caps = Caps { live: true, cols: 80, rows: 24, unicode: true };
        let build = plain.said(Options::of(Command::Build), caps, &Terminal::PIPE, took);
        assert!(build.contains("up to date · 10ms"), "{build:?}");
        assert_eq!(plain.said(Options::of(Command::Run), caps, &Terminal::PIPE, took), "");
        let quiet = Options { quiet: true, ..Options::of(Command::Build) };
        assert_eq!(plain.said(quiet, Caps::PIPE, &Terminal::PIPE, took), "");
        assert_eq!(plain.program(), None);
        let again = Response::Built { members: 1, profile: String::new(), artifacts: Vec::new() };
        assert!(matches!(plain.take(again), Step::HandOver(_)));
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
