//! What the app asks the daemon about the things an agent's words point at, for their chips and hover cards: a GitHub
//! pull request's or issue's state ([`GhRef`] in, [`GhItem`] out, all of a chat's in one batched query) and a web page's
//! OpenGraph card ([`PageMeta`]). The daemon fetches (crates/harness/harness/src/github.rs, page.rs); nothing here is
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
