//! Which program runs a command line: the thin `blaze` itself, or the engine.
//!
//! The thin `blaze` reads a command line only when every word of it is one of the few it
//! reads exactly as blaze's CLI does (`blaze_cargo::cli::args`, cargo's clap): `build` or
//! `run` (and their aliases) with `-q`/`--quiet` and `-r`/`--release`, each at most once, then
//! for `run` the program's arguments; and `--version` alone. Any other word (a flag it does
//! not list, a flag given twice, a cluster of short flags, a word that is not UTF-8 where the
//! CLI needs text) sends the whole command line to the engine, untouched: the engine's
//! parser is the one that refuses a command line, so nothing is read two ways.
//!
//! The environment routes too ([`env`]): a variable that changes what the engine's launcher
//! or its CLI does before the request (which blaze runs, the daemon's settings, the profile,
//! the core budget) is the engine's to read.

use std::ffi::{OsStr, OsString};

use crate::say;

/// The commands the thin `blaze` sends itself.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Command {
    Build,
    Run,
}

impl Command {
    /// The command as cargo names it (`blaze_api::Cargo::command`).
    pub const fn name(self) -> &'static str {
        match self {
            Command::Build => "build",
            Command::Run => "run",
        }
    }
}

/// A `build` or a `run` the thin `blaze` read whole.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Asked {
    pub command: Command,
    /// The words before the program's arguments, as typed (`blaze_api::Cargo::args`).
    pub args: Vec<String>,
    pub quiet: bool,
    pub release: bool,
    /// `run`'s arguments of the program.
    pub program: Vec<OsString>,
}

/// Why a command line is the engine's.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Why {
    /// No word at all: the engine prints the help.
    NoCommand,
    /// A first word that is not `build`, `run`, an alias of theirs or `--version` alone.
    Command(String),
    /// A word of the command that the thin `blaze` does not read.
    Word(String),
    /// A flag given a second time: clap's to refuse.
    Twice(String),
    /// `build` with `--`: clap's to refuse or take.
    Trailing,
    /// A `run` that says its program's command line (no `-q`), with an argument cargo would
    /// show quoted, or one that is not UTF-8: the engine's to quote.
    ProgramArg(String),
    /// A variable of the environment that the engine reads before it sends anything.
    Env(String),
}

impl std::fmt::Display for Why {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Why::NoCommand => f.write_str("no command"),
            Why::Command(word) => write!(f, "`{word}` is the engine's command"),
            Why::Word(word) => write!(f, "`{word}` is a word the engine's parser reads"),
            Why::Twice(word) => write!(f, "`{word}` is given twice"),
            Why::Trailing => f.write_str("`build` with `--`"),
            Why::ProgramArg(arg) => write!(f, "the program's argument `{arg}` is shown quoted"),
            Why::Env(name) => write!(f, "the environment sets `{name}`"),
        }
    }
}

/// Who runs a command line.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Route {
    /// `--version` or `-V`, alone.
    Version,
    Cargo(Asked),
    Engine(Why),
}

/// The route of `words`, the words after the program's name.
pub fn words(words: &[OsString]) -> Route {
    let Some((first, rest)) = words.split_first() else { return Route::Engine(Why::NoCommand) };
    let shown = |word: &OsStr| word.to_string_lossy().into_owned();
    let command = match first.to_str() {
        Some("--version" | "-V") if rest.is_empty() => return Route::Version,
        Some("build" | "b") => Command::Build,
        Some("run" | "r") => Command::Run,
        _ => return Route::Engine(Why::Command(shown(first))),
    };
    let mut asked = Asked {
        command,
        args: Vec::new(), // alloc: once a command, its words
        quiet: false,
        release: false,
        program: Vec::new(), // alloc: once a command, the program's arguments
    };
    let mut rest = rest.iter();
    while let Some(word) = rest.next() {
        let text = word.to_str();
        // `run`'s program has the rest once its first argument comes (clap's
        // `trailing_var_arg`), the first `--` among them dropped; after a `--` it has all.
        let (program, drops) = match text {
            Some("--") => (true, false),
            Some(text) if text.starts_with('-') && text != "-" => (false, false),
            _ => {
                asked.program.push(word.clone()); // clone: the program's argument, once
                (true, true)
            }
        };
        if program {
            if command == Command::Build {
                return Route::Engine(match text {
                    Some("--") => Why::Trailing,
                    _ => Why::Word(shown(word)),
                });
            }
            let mut dropped = !drops;
            for word in rest.by_ref() {
                if word == "--" && !dropped {
                    dropped = true;
                    continue;
                }
                asked.program.push(word.clone()); // clone: as above
            }
            break;
        }
        let flag = match text {
            Some("-q" | "--quiet") => &mut asked.quiet,
            Some("-r" | "--release") => &mut asked.release,
            _ => return Route::Engine(Why::Word(shown(word))),
        };
        if *flag {
            return Route::Engine(Why::Twice(shown(word)));
        }
        *flag = true;
        asked.args.extend(text.map(str::to_owned));
    }
    // `Running` shows the program's command line as cargo quotes it: the engine's, unless
    // every argument is shown as it is.
    if command == Command::Run && !asked.quiet {
        let quoted = asked.program.iter().find(|it| !it.to_str().is_some_and(say::plain));
        if let Some(arg) = quoted {
            return Route::Engine(Why::ProgramArg(shown(arg)));
        }
    }
    Route::Cargo(asked)
}

/// The one variable of blaze's own that the thin `blaze` reads itself: where blaze's home is.
const HOME: &str = "BLAZE_HOME";

/// The first of `names` (the environment's variables) that makes the command the engine's:
/// every `BLAZE_*` but `BLAZE_HOME` (the pin a launcher ran this blaze under, the daemon's
/// settings, the core budget, the profile and the statistics a command asks for), `IX_PROF`
/// (the sampler) and cargo's override of the terminal's width, each read by the engine before
/// it sends its request. blaze-cargo's tests hold every setting the daemon keeps to a name
/// this refuses.
pub fn env(names: impl Iterator<Item = OsString>) -> Option<Why> {
    for name in names {
        let refused = match name.to_str() {
            Some(name) => refuses(name),
            // No variable blaze reads has a name that is not UTF-8; the request can't carry
            // one, which is the engine's to say.
            None => true,
        };
        if refused {
            return Some(Why::Env(name.to_string_lossy().into_owned()));
        }
    }
    None
}

/// Whether the variable `name` makes a command the engine's ([`env`]).
pub fn refuses(name: &str) -> bool {
    (name.starts_with("BLAZE_") && name != HOME)
        || name == "IX_PROF"
        || name == "__CARGO_TEST_TTY_WIDTH_DO_NOT_USE_THIS"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(words: &[&str]) -> Vec<OsString> {
        words.iter().map(OsString::from).collect()
    }

    fn asked(typed: &[&str]) -> Asked {
        match words(&os(typed)) {
            Route::Cargo(asked) => asked,
            other => panic!("{typed:?} is not the thin blaze's: {other:?}"),
        }
    }

    #[test]
    fn a_plain_build_and_run_are_read_as_the_cli_reads_them() {
        let build = asked(&["build"]);
        assert_eq!((build.command, build.args.len(), build.quiet, build.release), (Command::Build, 0, false, false));
        let quiet = asked(&["b", "-q", "--release"]);
        assert_eq!(quiet.args, ["-q", "--release"]);
        assert!(quiet.quiet && quiet.release);
        let run = asked(&["run", "-q", "a", "--", "-b", "--", "c"]);
        assert_eq!((run.command, run.args.as_slice()), (Command::Run, ["-q".to_owned()].as_slice()));
        // The first `--` after the program's first argument is dropped; a later one is its.
        assert_eq!(run.program, os(&["a", "-b", "--", "c"]));
        let after = asked(&["r", "-q", "--", "--flag", "--", "x y"]);
        assert_eq!(after.program, os(&["--flag", "--", "x y"]));
        assert_eq!(asked(&["run", "-"]).program, os(&["-"]));
    }

    #[test]
    fn any_other_command_line_is_the_engines_whole() {
        let engine = |typed: &[&str]| match words(&os(typed)) {
            Route::Engine(why) => why,
            other => panic!("{typed:?} is not the engine's: {other:?}"),
        };
        assert_eq!(engine(&[]), Why::NoCommand);
        for first in ["test", "check", "blaze", "help", "--help", "-h", "status", "serve", "--ignore-pin", "-C", "+nightly"] {
            assert_eq!(engine(&[first, "build"]), Why::Command(first.to_owned()), "{first}");
        }
        // `--version` with anything after it is blaze's cargo's own, which says the binary.
        assert_eq!(engine(&["--version", "-v"]), Why::Command("--version".to_owned()));
        for word in ["-v", "--verbose", "-p", "--bin", "--release=1", "-qr", "--color", "--message-format=json",
            "--manifest-path", "-Zscript", "--features", "--target", "--profile", "-j8", "--goal", "--refused", "--help", "-h"]
        {
            assert_eq!(engine(&["build", word]), Why::Word(word.to_owned()), "{word}");
            assert_eq!(engine(&["run", "-q", word, "x"]), Why::Word(word.to_owned()), "{word}");
        }
        assert_eq!(engine(&["build", "-q", "--quiet"]), Why::Twice("--quiet".to_owned()));
        assert_eq!(engine(&["run", "-r", "-r"]), Why::Twice("-r".to_owned()));
        assert_eq!(engine(&["build", "--"]), Why::Trailing);
        assert_eq!(engine(&["build", "x"]), Why::Word("x".to_owned()));
        // Shown by `Running`, so quoted as cargo quotes it: the engine's.
        assert_eq!(engine(&["run", "a b"]), Why::ProgramArg("a b".to_owned()));
        assert_eq!(engine(&["run", "--", ""]), Why::ProgramArg(String::new()));
        assert_eq!(asked(&["run", "-q", "a b"]).program, os(&["a b"]));
    }

    #[test]
    fn version_alone_is_answered_here() {
        assert_eq!(words(&os(&["--version"])), Route::Version);
        assert_eq!(words(&os(&["-V"])), Route::Version);
    }

    #[test]
    fn blazes_own_variables_are_the_engines_to_read() {
        let names = |names: &[&str]| env(os(names).into_iter());
        assert_eq!(names(&["PATH", "HOME", "BLAZE_HOME", "CARGO_TERM_COLOR", "BLAZEX"]), None);
        for name in ["BLAZE_CORES", "BLAZE_PINNED_BY", "BLAZE_IGNORE_PIN_FOR", "BLAZE_RUN_AS_DEFAULT", "BLAZE_PROFILE",
            "BLAZE_STATS", "BLAZE_UI", "BLAZE_LAUNCHED", "IX_PROF", "__CARGO_TEST_TTY_WIDTH_DO_NOT_USE_THIS"]
        {
            assert_eq!(names(&["PATH", name]), Some(Why::Env(name.to_owned())), "{name}");
        }
    }
}
