//! `build`, `check`, `test`, `why` and `status` as typed options and typed answers: what each
//! sends ([`request`], [`why`]) and what its responses come to ([`Call`], [`Outcome`]).
//!
//! The options mirror the API's request: [`Options`]'s fields are those of
//! [`blaze_api::Targets`] and [`blaze_api::Cargo`]. A command is sent as the CLI sends it, with
//! its cargo command line ([`blaze_api::Targets::cargo`]), its environment and cargo's config
//! files outside the checkout ([`crate::place`]), so the daemon builds what `blaze` run in the
//! same directory with the same words would. The command line's words are made from the typed
//! fields, in one fixed order, then [`Options::args`] as given: the API's own form of them is
//! an argv ([`blaze_api::Cargo::args`]).

use std::path::PathBuf;

use blaze_api::{
    ApiError, Artifact, Cargo, Diagnostic, Rebuilt, Report, Request, Response, Status, Targets,
    TestResult, Ub, Unchecked, UnitOf, ViewRef, Worked,
};
use serde::{Deserialize, Serialize};

use crate::{Error, Fault, place};

/// How many of the functions blaze could not compile a build's error names
/// (`blaze_cargo::REFUSED_SHOWN`, the CLI's without `--refused`).
const REFUSED_SHOWN: u32 = 16;

/// What a build, a check or a test run is asked with.
#[derive(Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub struct Options {
    /// The directory the command runs in.
    pub cwd: PathBuf,
    /// How long the request may take before it is cancelled, in milliseconds.
    pub within_ms: u64,
    /// The environment the command runs with; `None` for this process's.
    #[serde(default)]
    pub env: Option<Vec<(String, String)>>,
    /// More words of the cargo command line, after those the fields here make, as cargo
    /// reads them (`--lib`, `--locked`, `--bin`, `app`). `--config`, `--manifest-path` and
    /// `-Z` are refused here: the first and the last are the CLI's, the second a field.
    #[serde(default)]
    pub args: Vec<String>,
    /// Whether diagnostics are rendered with colors.
    #[serde(default)]
    pub color: bool,
    /// The columns diagnostics are rendered in; `None` for rustc's own default.
    #[serde(default)]
    pub width: Option<u32>,
    /// A target triple; `None` for the machine's.
    #[serde(default)]
    pub target: Option<String>,
    /// `None` for the command's own: `dev` of a build and a check, `test` of a test.
    #[serde(default)]
    pub profile: Option<String>,
    /// How many `-v`.
    #[serde(default)]
    pub verbose: u8,
    #[serde(default)]
    pub features: Vec<String>,
    /// Packages by name; empty for the workspace's default members.
    #[serde(default)]
    pub packages: Vec<String>,
    #[serde(default)]
    pub all_features: bool,
    /// cargo's `--manifest-path`, against `cwd`.
    #[serde(default)]
    pub manifest_path: Option<String>,
    #[serde(default)]
    pub no_default_features: bool,
    /// A test only: the tests run are those whose names contain one of these; empty for all.
    #[serde(default)]
    pub filter: Vec<String>,
    /// A test only: run only the tests the view's change reached.
    #[serde(default)]
    pub reached: bool,
    /// A test only: libtest's selection arguments.
    #[serde(default)]
    pub test_args: Vec<String>,
    /// A test only: run in blaze's MIR evaluator, checked for undefined behavior.
    #[serde(default)]
    pub ub: Option<Ub>,
}

impl Options {
    /// A command run in `cwd` with nothing else said, to end within `within_ms`.
    pub fn new(cwd: impl Into<PathBuf>, within_ms: u64) -> Options {
        Options {
            cwd: cwd.into(),
            within_ms,
            env: None,
            args: Vec::new(), // alloc: none
            color: false,
            width: None,
            target: None,
            profile: None,
            verbose: 0,
            features: Vec::new(), // alloc: none
            packages: Vec::new(), // alloc: none
            all_features: false,
            manifest_path: None,
            no_default_features: false,
            filter: Vec::new(), // alloc: none
            reached: false,
            test_args: Vec::new(), // alloc: none
            ub: None,
        }
    }
}

/// What `why` is asked with ([`Request::Rebuilt`]).
#[derive(Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub struct WhyOptions {
    /// Every item of each group, not its first eight.
    #[serde(default)]
    pub all: bool,
    /// The workspace whose last build is asked of, by an absolute directory in it; `None` for
    /// the newest build.
    #[serde(default)]
    pub cwd: Option<String>,
    /// One item, for its chain of reasons.
    #[serde(default)]
    pub item: Option<String>,
    /// How long the request may take, in milliseconds.
    pub within_ms: u64,
    /// Only the steps whose chain ends unexplained.
    #[serde(default)]
    pub unexplained: bool,
}

/// What a call is, which says what it sends and what its answer is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Method {
    Build,
    Check,
    Test,
    Why,
    Status,
    /// A request given whole: its answer is its `Done`.
    Request,
}

impl Method {
    pub const ALL: [Method; 6] =
        [Method::Build, Method::Check, Method::Test, Method::Why, Method::Status, Method::Request];

    pub const fn name(self) -> &'static str {
        match self {
            Method::Build => "build",
            Method::Check => "check",
            Method::Test => "test",
            Method::Why => "why",
            Method::Status => "status",
            Method::Request => "request",
        }
    }

    /// The method [`Method::name`] names `name`.
    pub fn named(name: &str) -> Option<Method> {
        Method::ALL.into_iter().find(|it| it.name() == name)
    }

    /// The profile cargo builds the command in when none is named.
    const fn profile(self) -> &'static str {
        match self {
            Method::Test => "test",
            _ => "dev",
        }
    }
}

/// The request of `method` (a build, a check or a test) asked with `options`, sent with the
/// environment `env`.
pub fn request(
    method: Method,
    options: &Options,
    env: &[(String, String)],
) -> Result<Request, String> {
    let test = method == Method::Test;
    let only_test = [
        ("filter", !options.filter.is_empty()),
        ("reached", options.reached),
        ("test_args", !options.test_args.is_empty()),
        ("ub", options.ub.is_some()),
    ];
    if let Some((name, _)) = only_test.iter().find(|(_, set)| *set && !test) {
        return Err(format!("the option `{name}` is a test's, and this is a {}", method.name()));
    }
    for word in &options.args {
        let refused = ["--config", "--manifest-path", "-Z", "--message-format", "--color"];
        if let Some(flag) = refused.iter().find(|it| word.starts_with(**it)) {
            return Err(format!(
                "`{word}` among `args`: `{flag}` is not carried as a word of the command line by this client \
                 (`manifest_path` and `color` are options of their own; the rest are `blaze`'s)"
            ));
        }
    }
    let place = place::find(&options.cwd, options.manifest_path.as_deref())?;
    let mut files = place::config_files(&place, place::cargo_home(env, &place.cwd).as_deref())?;
    if method == Method::Check {
        files.extend(place::clippy_files(&place)?);
    }
    // alloc: the request's command line, once a command.
    let mut args = Vec::new();
    let mut word = |it: &str| args.push(it.to_owned());
    for package in &options.packages {
        word("-p");
        word(package);
    }
    if !options.features.is_empty() {
        word("--features");
        word(&options.features.join(","));
    }
    if options.all_features {
        word("--all-features");
    }
    if options.no_default_features {
        word("--no-default-features");
    }
    if let Some(profile) = &options.profile {
        word("--profile");
        word(profile);
    }
    if let Some(target) = &options.target {
        word("--target");
        word(target);
    }
    if let Some(manifest_path) = &options.manifest_path {
        word("--manifest-path");
        word(manifest_path);
    }
    for _ in 0..options.verbose {
        word("-v");
    }
    // clone: the request owns its words.
    args.extend(options.args.iter().cloned());
    let root = place::utf8(&place.root)?;
    let cargo = Cargo {
        cwd: place::utf8(&place.cwd)?,
        // clone: the request owns its environment.
        env: env.to_vec(),
        args,
        // clone: the view names the root too.
        root: root.clone(),
        color: options.color,
        files,
        width: options.width,
        command: method.name().to_owned(),
        refused: Some(REFUSED_SHOWN),
        verbose: options.verbose,
    };
    let targets = Targets {
        cargo: Some(cargo),
        // clone: the request owns what it names, here and below.
        target: options.target.clone(),
        profile: options.profile.clone().unwrap_or_else(|| method.profile().to_owned()),
        features: options.features.clone(),
        packages: options.packages.clone(),
        all_features: options.all_features,
        no_default_features: options.no_default_features,
    };
    let view = ViewRef::Head(root);
    match method {
        Method::Build => Ok(Request::Build { view, targets }),
        Method::Check => Ok(Request::Check { view, targets }),
        Method::Test => Ok(Request::Test {
            ub: options.ub,
            all: !options.reached,
            args: options.test_args.clone(),
            view,
            filter: options.filter.clone(),
            targets,
        }),
        Method::Why | Method::Status | Method::Request => {
            Err(format!("`{}` is not asked with a build's options", method.name()))
        }
    }
}

/// The request of a `why`.
pub fn why(options: &WhyOptions) -> Request {
    Request::Rebuilt {
        all: options.all,
        // clone: the request owns what it names.
        cwd: options.cwd.clone(),
        item: options.item.clone(),
        build: None,
        unexplained: options.unexplained,
    }
}

/// A build's output, placed ([`Response::Built`]).
#[derive(Serialize, Clone, PartialEq, Eq, Debug)]
pub struct Built {
    pub members: u32,
    pub profile: String,
    pub artifacts: Vec<Artifact>,
}

/// Something blaze itself said, of a unit or of the request ([`Response::Report`]).
#[derive(Serialize, Clone, PartialEq, Eq, Debug)]
pub struct Said {
    pub unit: Option<UnitOf>,
    pub report: Report,
}

/// What a build, a check or a test run came to: its `Done` and what it said on the way that
/// is its answer. What it only streams (units starting, what it is doing) is the events'.
#[derive(Serialize, Clone, PartialEq, Eq, Debug, Default)]
pub struct Outcome {
    /// The request's `Done`.
    pub ok: bool,
    pub work: Option<Worked>,
    /// `None` of a check, and of a build that placed nothing.
    pub built: Option<Built>,
    pub tests: Vec<TestResult>,
    /// The errors that ended the request ([`Response::Error`]).
    pub errors: Vec<Report>,
    pub reports: Vec<Said>,
    pub unchecked: Option<Unchecked>,
    /// Every diagnostic, in the order sent, each with rustc's rendering of it.
    pub diagnostics: Vec<Diagnostic>,
}

/// A call's answer.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Answer {
    Outcome(Box<Outcome>),
    Rebuilt(Box<Rebuilt>),
    Status(Box<Status>),
    Done { ok: bool },
}

impl Answer {
    /// The answer in the API's JSON rendering: an [`Outcome`], a [`Rebuilt`] or a [`Status`]
    /// as serde writes it, or `{"ok":true}`.
    pub fn json(&self) -> String {
        let written = match self {
            Answer::Outcome(outcome) => sonic_rs::to_string(outcome),
            Answer::Rebuilt(rebuilt) => sonic_rs::to_string(rebuilt),
            Answer::Status(status) => sonic_rs::to_string(status),
            Answer::Done { ok } => return format!("{{\"ok\":{ok}}}"), // alloc: the answer handed out
        };
        // The API's types key no map by anything but text, so every value has a JSON rendering.
        written.expect("an answer of the API has a JSON rendering")
    }
}

/// One call under way: what its responses have come to so far.
#[derive(Debug)]
pub struct Call {
    method: Method,
    outcome: Outcome,
    rebuilt: Option<Rebuilt>,
    status: Option<Status>,
    error: Option<ApiError>,
}

impl Call {
    pub fn new(method: Method) -> Call {
        Call { method, outcome: Outcome::default(), rebuilt: None, status: None, error: None }
    }

    pub fn method(&self) -> Method {
        self.method
    }

    /// Takes the call's next response.
    pub fn take(&mut self, response: Response) {
        let outcome = &mut self.outcome;
        match response {
            Response::Diagnostic(diagnostic) => outcome.diagnostics.push(diagnostic),
            Response::Built { members, profile, artifacts } => {
                outcome.built = Some(Built { members, profile, artifacts });
            }
            Response::Work(worked) => outcome.work = Some(worked),
            Response::Unchecked(unchecked) => outcome.unchecked = Some(unchecked),
            Response::TestResult(result) => outcome.tests.push(result),
            Response::Report { unit, report } => outcome.reports.push(Said { unit, report }),
            Response::Error(error) => {
                // clone: error path; the outcome lists it and the call keeps the first.
                outcome.errors.push(error.report().clone());
                self.error.get_or_insert(error);
            }
            Response::Rebuilt(rebuilt) => self.rebuilt = Some(rebuilt),
            Response::Status(status) => self.status = Some(status),
            _ => {}
        }
    }

    /// The call's answer, its request having ended with `Done { ok }`.
    pub fn finish(self, ok: bool) -> Result<Answer, Error> {
        let Call { method, mut outcome, rebuilt, status, error } = self;
        let one = |answer: Option<Answer>| match (answer, error) {
            (Some(answer), _) => Ok(answer),
            (None, Some(error)) => Err(Error::Daemon(error)),
            // alloc: error path, a daemon that ended a request with nothing.
            (None, None) => Err(Error::Fault(Fault::Refused(ApiError::new(
                format!("the daemon ended a `{}` with neither its answer nor an error", method.name()),
                None,
            )))),
        };
        match method {
            Method::Build | Method::Check | Method::Test => {
                outcome.ok = ok;
                Ok(Answer::Outcome(Box::new(outcome))) // alloc: the answer handed out
            }
            Method::Why => one(rebuilt.map(|it| Answer::Rebuilt(Box::new(it)))), // alloc: as above
            Method::Status => one(status.map(|it| Answer::Status(Box::new(it)))), // alloc: as above
            Method::Request => Ok(Answer::Done { ok }),
        }
    }
}

/// A call whose answer is another's: a bug here, said as one.
pub(crate) fn unexpected(method: Method, answer: &Answer) -> Error {
    let kind = match answer {
        Answer::Outcome(_) => "an outcome",
        Answer::Rebuilt(_) => "a `Rebuilt`",
        Answer::Status(_) => "a `Status`",
        Answer::Done { .. } => "a `Done`",
    };
    // alloc: error path.
    Error::Asked(format!("a `{}` was answered with {kind}: a bug in blaze-client", method.name()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_are_read_from_json_and_an_unknown_one_is_refused() {
        let options: Options =
            sonic_rs::from_str(r#"{"cwd":"/w","within_ms":5,"packages":["a"],"reached":true}"#).unwrap();
        let mut want = Options::new("/w", 5);
        want.packages = vec!["a".to_owned()];
        want.reached = true;
        assert_eq!(options, want);
        let refused = sonic_rs::from_str::<Options>(r#"{"cwd":"/w","within_ms":5,"pkg":[]}"#);
        assert!(refused.unwrap_err().to_string().contains("pkg"));
        let refused = sonic_rs::from_str::<Options>(r#"{"cwd":"/w"}"#);
        assert!(refused.unwrap_err().to_string().contains("within_ms"));
    }

    #[test]
    fn a_tests_option_on_a_build_is_refused_by_name() {
        let mut options = Options::new("/w", 5);
        options.reached = true;
        let refused = request(Method::Build, &options, &[]).unwrap_err();
        assert_eq!(refused, "the option `reached` is a test's, and this is a build");
        let mut options = Options::new("/w", 5);
        options.args = vec!["--config=a.toml".to_owned()];
        let refused = request(Method::Build, &options, &[]).unwrap_err();
        assert!(refused.starts_with("`--config=a.toml` among `args`"), "{refused}");
    }

    #[test]
    fn a_call_comes_to_its_methods_answer() {
        let mut call = Call::new(Method::Build);
        call.take(Response::Diagnostic(Diagnostic::note("said".to_owned())));
        call.take(Response::Flushed { seq: 1 });
        let Ok(Answer::Outcome(outcome)) = call.finish(false) else { panic!("no outcome") };
        assert_eq!((outcome.ok, outcome.diagnostics.len()), (false, 1));
        let answer = Answer::Outcome(outcome).json();
        assert!(answer.starts_with(r#"{"ok":false,"work":null,"built":null,"tests":[]"#), "{answer}");

        let mut call = Call::new(Method::Status);
        call.take(Response::Error(ApiError::new("no", None)));
        let Err(Error::Daemon(error)) = call.finish(false) else { panic!("no error") };
        assert_eq!(error.what, "no");
        let ended = Call::new(Method::Why).finish(true).unwrap_err();
        assert_eq!(ended.code(), 2, "{ended}");
        assert_eq!(Call::new(Method::Request).finish(true).unwrap().json(), r#"{"ok":true}"#);
    }
}
