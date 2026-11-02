//! What a unit's work stops at where it once went on with a value guessed in its place
//! (AGENTS.md rule 3, no-fallback): each case names what is missing and where.

#![forbid(unsafe_code)]

use std::fmt;

use blaze_api::ApiError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UnitError {
    /// Unit `unit` names dependency `dep`, past the build's `count` units.
    Dependency { unit: usize, dep: usize, count: usize },
    /// Unit `unit`'s package `pkg_id` is none of the build's packages.
    Package { unit: usize, pkg_id: String },
    /// Package `package`'s manifest `path` is in no directory.
    ManifestDir { package: String, path: String },
    /// Build script compile `unit` names no output: its program has no path.
    ScriptProgram { unit: usize },
    /// The `OUT_DIR` of a run of `package`'s build script, `out_dir`, is under no
    /// `<profile>/build`: its dependencies' dynamic libraries have no directory to be found in.
    ProfileDir { package: String, out_dir: String },
    /// The plan of a run of `package`'s build script sets no `name`, which cargo always sets.
    NoVariable { package: String, name: &'static str },
    /// How many jobs a run of `package`'s build script is told (`NUM_JOBS`): the machine's
    /// parallelism could not be read, as cargo refuses to guess it.
    Parallelism { package: String, error: String },
    /// The front end has no crate for build script compile `unit`: its purity can't be asked.
    NoCrate { unit: usize },
    /// The program `target` that `cargo run` picked has no executable.
    NoExecutable { target: String },
    /// The test unit of `target` compiled no executable: its tests can't run.
    NoTestExecutable { target: String },
    /// A test run's directory under the daemon's state, `tests/<tags>`, is no name a file can have.
    TestDir { tags: u64 },
    /// The test binary `binary`'s selector was given back before its tests were chosen.
    NoSelector { binary: String },
    /// `cargo run`'s program ended as `how`, neither an exit nor a failure to start.
    RunEnded { how: String },
}

impl fmt::Display for UnitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UnitError::Dependency { unit, dep, count } => {
                write!(f, "unit {unit} depends on unit {dep}, and the build has {count} units")
            }
            UnitError::Package { unit, pkg_id } => {
                write!(f, "unit {unit} is of the package `{pkg_id}`, which the build has none of")
            }
            UnitError::ManifestDir { package, path } => {
                write!(f, "the manifest of `{package}`, `{path}`, is in no directory")
            }
            UnitError::ScriptProgram { unit } => {
                write!(f, "the build script compiled by unit {unit} has no output: its program has no path")
            }
            UnitError::ProfileDir { package, out_dir } => write!(
                f,
                "the OUT_DIR of `{package}`'s build script, `{out_dir}`, is under no `<profile>/build` directory"
            ),
            UnitError::NoVariable { package, name } => {
                write!(f, "the run of `{package}`'s build script is planned with no `{name}`, which cargo always sets")
            }
            UnitError::Parallelism { package, error } => write!(
                f,
                "the build script of `{package}` is told NUM_JOBS, and the machine's parallelism can't be read: {error}"
            ),
            UnitError::NoCrate { unit } => {
                write!(f, "the front end has no crate for the build script compiled by unit {unit}")
            }
            UnitError::NoExecutable { target } => write!(f, "the program `{target}` `cargo run` picked has no executable"),
            UnitError::NoTestExecutable { target } => {
                write!(f, "the test unit of `{target}` compiled no executable: its tests can't run")
            }
            UnitError::TestDir { tags } => {
                write!(f, "the test run's directory `tests/{tags:016x}` is no name blaze can give a file")
            }
            UnitError::NoSelector { binary } => {
                write!(f, "the test binary `{binary}` gave its selector back before its tests were chosen")
            }
            UnitError::RunEnded { how } => write!(f, "the program `cargo run` started ended as {how}"),
        }
    }
}

impl std::error::Error for UnitError {}

impl From<UnitError> for ApiError {
    fn from(error: UnitError) -> ApiError {
        ApiError::new(error.to_string(), None) // alloc: the edge: the error the build stops with
    }
}

/// The directory of `package`'s manifest at `path`.
pub(crate) fn manifest_dir(package: &str, path: &str) -> Result<std::path::PathBuf, UnitError> {
    std::path::Path::new(path)
        .parent()
        .filter(|it| !it.as_os_str().is_empty())
        .map(std::path::Path::to_path_buf)
        .ok_or_else(|| UnitError::ManifestDir { package: package.to_owned(), path: path.to_owned() }) // alloc: error path
}
