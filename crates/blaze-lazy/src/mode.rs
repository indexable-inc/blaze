//! Which builds may compile lazily: a process the daemon runs for a dev profile, never a
//! release build and never a build that writes an output (`blaze build`, an executable, a
//! test binary on disk). Those are compiled whole, as rustc compiles them, so what is written
//! never depends on what one run happened to call. Asking otherwise is an error naming the
//! profile or the output, never a quiet switch to a whole build.

use std::fmt;

use blaze_codegen::plan::{Mode, OptLevel, Policy};

/// What a build asking for lazy compilation is.
#[derive(Clone, Copy, Debug)]
pub struct Purpose<'a> {
    /// The cargo profile's name: `dev`, `test`, `release`, `bench` or a custom one.
    pub profile: &'a str,
    /// How the build compiles every function.
    pub policy: &'a Policy,
    /// The file the build writes, if it writes one.
    pub output: Option<&'a str>,
}

/// Why a build may not compile lazily.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error, once per build, inside `SessionError`
pub enum Refused {
    /// The build writes `path`: an output is compiled whole.
    Output { profile: String, path: String },
    /// The policy compiles a file, each function once ([`Mode::Output`]).
    Whole { profile: String },
    /// A release profile: by name (`release`, `bench`) or optimized (`opt-level` 2, 3, `s`,
    /// `z`), as one inheriting from `release` is.
    Release { profile: String, level: OptLevel },
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refused::Output { profile, path } => write!(
                f,
                "lazy compilation of a build that writes {path} (profile `{profile}`): an output is compiled whole"
            ),
            Refused::Whole { profile } => write!(
                f,
                "lazy compilation of profile `{profile}` asked of a build that compiles a file: an output is compiled whole"
            ),
            Refused::Release { profile, level } => write!(
                f,
                "lazy compilation of profile `{profile}` (opt-level {}): release builds are compiled whole",
                level_name(*level)
            ),
        }
    }
}

impl std::error::Error for Refused {}

fn level_name(level: OptLevel) -> &'static str {
    match level {
        OptLevel::O0 => "0",
        OptLevel::O1 => "1",
        OptLevel::O2 => "2",
        OptLevel::O3 => "3",
        OptLevel::S => "\"s\"",
        OptLevel::Z => "\"z\"",
    }
}

/// Whether `purpose` may compile lazily: `Ok` only for a process the daemon runs, with no
/// output, under a profile that is not a release one.
pub fn allow(purpose: &Purpose<'_>) -> Result<(), Refused> {
    let profile = || purpose.profile.to_owned(); // alloc: called only for a refusal
    if let Some(path) = purpose.output {
        return Err(Refused::Output { profile: profile(), path: path.to_owned() }); // alloc: a refusal
    }
    if purpose.policy.mode == Mode::Output {
        return Err(Refused::Whole { profile: profile() });
    }
    let level = purpose.policy.profile.opt_level;
    let named_release = matches!(purpose.profile, "release" | "bench");
    let optimized = !matches!(level, OptLevel::O0 | OptLevel::O1);
    if named_release || optimized {
        return Err(Refused::Release { profile: profile(), level });
    }
    Ok(())
}
