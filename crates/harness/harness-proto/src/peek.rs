//! What the app asks the daemon about the things an agent's words point at, for their chips and hover cards: a GitHub
//! pull request's or issue's state ([`GhRef`] in, [`GhItem`] out, all of a chat's in one batched query) and a web page's
//! OpenGraph card ([`PageMeta`]); and, while a card shows a pull request whose checks are not all done, each check by
//! itself ([`GhChecks`]: what runs, its step, since when). The daemon fetches (crates/harness/harness/src/github.rs, page.rs); nothing here is
//! guessed: what could not be fetched comes back with `error` naming it and its state unset.
use serde::{Deserialize, Serialize};

/// A pull request or an issue.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum GhKind {
    Pr,
    Issue,
}

/// One thing a chip names: `repo` is `owner/name`, `n` its number.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GhRef {
    pub kind: GhKind,
    pub repo: String,
    pub n: u64,
}

/// Where it stands. A pull request is open, a draft, merged or closed; an issue open, closed (completed) or closed as not
/// planned.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GhState {
    Open,
    Draft,
    Merged,
    Closed,
    NotPlanned,
}

/// A pull request's head commit's combined checks (GitHub's `statusCheckRollup`): `ERROR` reads as a failure, `EXPECTED`
/// as pending.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GhCi {
    Success,
    Failure,
    Pending,
}

/// What the daemon found for one [`GhRef`]. With `error` set (the repository or number does not exist, `gh` is not
/// signed in, the query failed) `state` and `ci` are none: the chip shows its unknown state, never a guess.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct GhItem {
    pub kind: GhKind,
    pub repo: String,
    pub n: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<GhState>,
    /// None for an issue, a pull request with no checks, or one whose checks could not be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ci: Option<GhCi>,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub author: String,
    /// Lines added and removed (a pull request's).
    #[serde(default)]
    pub additions: u64,
    #[serde(default)]
    pub deletions: u64,
    /// The first lines of its description, cut at [`BODY_MAX`] bytes on a character boundary.
    #[serde(default)]
    pub body: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
}

impl GhItem {
    /// An empty answer to `r`, nothing known yet.
    #[must_use]
    pub fn of(r: &GhRef) -> Self {
        Self {
            kind: r.kind,
            repo: r.repo.clone(),
            n: r.n,
            state: None,
            ci: None,
            title: String::new(),
            url: String::new(),
            author: String::new(),
            additions: 0,
            deletions: 0,
            body: String::new(),
            error: String::new(),
        }
    }

    /// One that could not be fetched, `error` saying why (it names the repository and number itself).
    #[must_use]
    pub fn failed(r: &GhRef, error: String) -> Self {
        Self { error, ..Self::of(r) }
    }

    /// The ref it answers.
    #[must_use]
    pub fn at(&self) -> GhRef {
        GhRef { kind: self.kind, repo: self.repo.clone(), n: self.n }
    }
}

/// Where one check of a pull request's head commit stands, in the order a card lists them: what failed first, then what
/// runs, then what waits; passed and skipped ones are counted, not listed. A check run's `CANCELLED`, `TIMED_OUT`,
/// `ACTION_REQUIRED` and `STARTUP_FAILURE` read as failed, `NEUTRAL`, `SKIPPED` and `STALE` as skipped; a commit status's
/// `ERROR` as failed, `PENDING` as running and `EXPECTED` as queued.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum GhCheckState {
    Failed,
    Running,
    Queued,
    Passed,
    Skipped,
}

/// One check: a GitHub Actions job or another app's check run, or a commit status. Times are Unix seconds, 0 when GitHub
/// gave none (`started` is when it was queued while it waits, when it started once it runs).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct GhCheck {
    pub name: String,
    pub state: GhCheckState,
    /// The workflow it is a job of (empty for a check run that is no job, or a commit status).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub workflow: String,
    /// The step in progress while it runs, by the job's own step list (empty when it reports none).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub step: String,
    #[serde(default)]
    pub started: i64,
    #[serde(default)]
    pub completed: i64,
    /// Its page: a job's log, a check run's details, a status's target.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
}

/// A pull request's head commit's checks one by one (`github_checks`): at most [`CHECKS_MAX`] of `total`. With `error`
/// set (the pull request could not be read, `gh` failed) `checks` is empty.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct GhChecks {
    pub repo: String,
    pub n: u64,
    #[serde(default)]
    pub checks: Vec<GhCheck>,
    /// How many checks the commit has: `checks.len()`, or more past [`CHECKS_MAX`].
    #[serde(default)]
    pub total: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
}

impl GhChecks {
    /// Nothing known yet of `r`'s checks.
    #[must_use]
    pub fn of(r: &GhRef) -> Self {
        Self { repo: r.repo.clone(), n: r.n, checks: Vec::new(), total: 0, error: String::new() }
    }

    /// `r`'s checks could not be read, `error` saying why (it names the repository and number itself).
    #[must_use]
    pub fn failed(r: &GhRef, error: String) -> Self {
        Self { error, ..Self::of(r) }
    }

    /// Whether a check has yet to finish (a shown card asks again while one has).
    #[must_use]
    pub fn unfinished(&self) -> bool {
        self.checks.iter().any(|c| matches!(c.state, GhCheckState::Running | GhCheckState::Queued))
    }
}

/// The most checks one `github_checks` answer lists.
pub const CHECKS_MAX: usize = 50;
/// The most steps of each check it reads to find the one in progress.
pub const STEPS_MAX: usize = 60;

/// How much of a description an item carries.
pub const BODY_MAX: usize = 600;
/// The most refs one request may ask after (a chat with more asks in batches).
pub const REFS_MAX: usize = 100;

/// A web page's card: its OpenGraph (else `<title>` and meta description) title, description and site name, its
/// `og:image` saved where the app can read it (`image`, a local path; empty when it has none or it could not be fetched),
/// and its favicon the same way (`icon`, a local PNG or JPEG: the page's `apple-touch-icon` or `icon` link, else the
/// site's `/favicon.ico`; empty when it has none the app can draw, and the app shows a globe). With `error` set nothing
/// else is known.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct PageMeta {
    pub url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub site: String,
    #[serde(default)]
    pub image: String,
    #[serde(default)]
    pub icon: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
}
