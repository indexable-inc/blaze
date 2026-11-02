//! The service's one input: a JSON file (rule scripts-take-typed-input), read into structs that
//! refuse unknown fields. One file serves both machines: `blaze-ci serve` reads all of it,
//! `blaze-ci standby` its `standby` part.
//!
//! Secrets (the App's key, the webhook's secret, the primary's token) are never in the JSON: it
//! names their files, read only when needed ([`Secret`]), never printed.

use std::{
    collections::BTreeSet,
    fmt,
    net::SocketAddr,
    path::{Path, PathBuf},
};

use blaze_api::{Request, Targets, ViewRef};
use serde::Deserialize;

use crate::oracle;

#[derive(Deserialize, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// `owner/name`: deliveries for any other repository are refused.
    pub repo: String,
    pub app: AppConfig,
    pub webhook: WebhookConfig,
    pub daemon: DaemonConfig,
    pub git: GitConfig,
    /// The branch whose green runs are committed to the head `ci/<branch>` and snapshotted.
    pub default_branch: String,
    /// What runs on every push and pull request.
    pub checks: Vec<Check>,
    /// What runs on a tag, one release at a time.
    pub release_checks: Vec<Check>,
    pub oracle: OracleConfig,
    pub snapshot: SnapshotConfig,
    pub standby: StandbyConfig,
    /// The service's own state (each ref's checkout, the snapshots), rewritten after each
    /// change; absent on the first start.
    pub state: PathBuf,
}

#[derive(Deserialize, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    pub id: u64,
    pub installation: u64,
    /// The App's private key, PEM.
    pub key_file: PathBuf,
    /// The API's host, `api.github.com`.
    pub api: String,
}

#[derive(Deserialize, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub struct WebhookConfig {
    /// Where the listener binds, behind the VM's proxy.
    pub listen: SocketAddr,
    pub secret_file: PathBuf,
}

#[derive(Deserialize, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub struct DaemonConfig {
    /// The warm daemon's agent API socket.
    pub socket: PathBuf,
}

#[derive(Deserialize, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub struct GitConfig {
    /// The VM's clone of the repository.
    pub dir: PathBuf,
    pub remote: String,
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum CheckKind {
    Check,
    Build,
    Test,
}

/// One check run per event.
// clone: once, at start, into the service's settings
#[derive(Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub struct Check {
    /// The check run's name on GitHub, which required checks name.
    pub name: String,
    pub kind: CheckKind,
    pub targets: TargetsConfig,
    /// For a test check only: test names to run.
    #[serde(default)]
    pub filter: Vec<String>,
    /// For a test check only: every test, not only those the change reached.
    #[serde(default)]
    pub all: bool,
}

impl Check {
    /// The request this check sends for `view`.
    pub fn request(&self, view: ViewRef) -> Request {
        let targets = self.targets.to_targets();
        match self.kind {
            CheckKind::Check => Request::Check { view, targets },
            CheckKind::Build => Request::Build { view, targets },
            CheckKind::Test => {
                Request::Test {
                    view,
                    targets,
                    filter: self.filter.clone(), // clone: once per test check run, its request
                    all: self.all,
                    args: Vec::new(), // alloc: none, an empty list
                    ub: None,
                }
            }
        }
    }
}

/// [`blaze_api::Targets`] as the file spells it.
// clone: once at start, and once per sampled view the oracle verifies
#[derive(Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub struct TargetsConfig {
    /// Empty for the workspace's default members.
    #[serde(default)]
    pub packages: Vec<String>,
    pub profile: String,
    /// `None` for the machine's.
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub features: Vec<String>,
    #[serde(default)]
    pub all_features: bool,
    #[serde(default)]
    pub no_default_features: bool,
}

impl TargetsConfig {
    pub fn to_targets(&self) -> Targets {
        Targets {
            packages: self.packages.clone(), // clone: once per check run, its request
            profile: self.profile.clone(), // clone: once per check run, its request
            target: self.target.clone(), // clone: once per check run, its request
            features: self.features.clone(), // clone: once per check run, its request
            all_features: self.all_features,
            no_default_features: self.no_default_features,
            cargo: None,
        }
    }
}

#[derive(Deserialize, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub struct OracleConfig {
    /// One view in this many is built again from nothing ([`crate::oracle::sampled`]); 0 for
    /// none.
    pub one_in: u64,
    /// What a sampled view is verified for.
    pub targets: TargetsConfig,
}

// clone: once per snapshot, the argv a snapshot thread runs
#[derive(Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub struct SnapshotConfig {
    /// The least time between two snapshots.
    pub every_secs: u64,
    /// Takes a snapshot of this VM; `{name}` is replaced by the snapshot's name. Its stdout's
    /// last nonblank line is the snapshot's id.
    pub command: Vec<String>,
    /// Removes a snapshot; `{id}` is replaced by its id.
    pub remove: Vec<String>,
    /// How many snapshots are kept; older ones are removed.
    pub keep: usize,
}

#[derive(Deserialize, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
pub struct StandbyConfig {
    /// The primary's agent API over HTTP.
    pub primary: SocketAddr,
    /// The bearer token of the primary's HTTP API.
    pub token_file: PathBuf,
    pub every_secs: u64,
    /// Failed polls in a row after which the standby takes over.
    pub misses: u32,
    /// Prints the newest snapshot's id as its stdout's last nonblank line.
    pub latest: Vec<String>,
    /// Restores snapshot `{id}` here.
    pub restore: Vec<String>,
    /// Makes the restored VM the primary (moves its address, its webhook).
    pub promote: Vec<String>,
}

/// Why the file is refused.
#[derive(PartialEq, Eq, Debug)]
pub struct ConfigError {
    pub what: String,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.what)
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    pub fn read(path: &Path) -> Result<Config, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|error| ConfigError {
            what: format!("could not read {}: {error}", path.display()), // alloc: error path
        })?;
        Config::parse(&text).map_err(|error| ConfigError {
            what: format!("{}: {}", path.display(), error.what), // alloc: error path
        })
    }

    pub fn parse(text: &str) -> Result<Config, ConfigError> {
        let config: Config = serde_json::from_str(text)
            .map_err(|error| ConfigError { what: error.to_string() })?; // alloc: error path
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        let refuse = |what: String| Err(ConfigError { what });
        if self.repo.split('/').filter(|part| !part.is_empty()).count() != 2
            || self.repo.matches('/').count() != 1
        {
            // alloc: error path, the file is refused
            return refuse(format!("repo {:?} is not owner/name", self.repo));
        }
        if self.default_branch.is_empty() {
            // alloc: error path, the file is refused
            return refuse("default_branch is empty".to_owned());
        }
        for (list, checks) in [("checks", &self.checks), ("release_checks", &self.release_checks)] {
            let mut names = BTreeSet::new();
            for check in checks {
                if check.name.is_empty() {
                    // alloc: error path, the file is refused
                    return refuse(format!("a check of {list} has no name"));
                }
                if check.name == oracle::CHECK {
                    return refuse(format!( // alloc: error path, the file is refused
                        "check {} of {list} has the oracle's name, which the service posts",
                        check.name
                    ));
                }
                if !names.insert(check.name.as_str()) {
                    // alloc: error path, the file is refused
                    return refuse(format!("check {} is named twice in {list}", check.name));
                }
                if check.kind != CheckKind::Test && (!check.filter.is_empty() || check.all) {
                    return refuse(format!( // alloc: error path, the file is refused
                        "check {} of {list} has a test filter or `all`, and is not a test",
                        check.name
                    ));
                }
            }
        }
        if self.checks.is_empty() {
            // alloc: error path, the file is refused
            return refuse("checks is empty: a push would post nothing".to_owned());
        }
        if self.release_checks.is_empty() {
            // alloc: error path, the file is refused
            return refuse("release_checks is empty: a tag would post nothing".to_owned());
        }
        let snapshot = &self.snapshot;
        if snapshot.command.is_empty() {
            // alloc: error path, the file is refused
            return refuse("snapshot.command is empty".to_owned());
        }
        if snapshot.remove.is_empty() {
            // alloc: error path, the file is refused
            return refuse("snapshot.remove is empty".to_owned());
        }
        if snapshot.keep == 0 {
            // alloc: error path, the file is refused
            return refuse("snapshot.keep is 0: the snapshot just taken would be removed".to_owned());
        }
        let standby = &self.standby;
        for (name, argv) in
            [("latest", &standby.latest), ("restore", &standby.restore), ("promote", &standby.promote)]
        {
            if argv.is_empty() {
                // alloc: error path, the file is refused
                return refuse(format!("standby.{name} is empty"));
            }
        }
        if standby.every_secs == 0 {
            // alloc: error path, the file is refused
            return refuse("standby.every_secs is 0".to_owned());
        }
        if standby.misses == 0 {
            // alloc: error path, the file is refused
            return refuse("standby.misses is 0: the standby would take over at once".to_owned());
        }
        Ok(())
    }
}

/// The bytes of a secret's file, without a trailing newline. Debug never shows them.
#[derive(PartialEq, Eq)]
pub struct Secret(Vec<u8>);

impl Secret {
    pub fn read(path: &Path) -> Result<Secret, ConfigError> {
        let mut bytes = std::fs::read(path).map_err(|error| ConfigError {
            // alloc: error path
            what: format!("could not read the secret file {}: {error}", path.display()),
        })?;
        while bytes.last().is_some_and(|byte| *byte == b'\n' || *byte == b'\r') {
            bytes.pop();
        }
        if bytes.is_empty() {
            // alloc: error path
            return Err(ConfigError { what: format!("the secret file {} is empty", path.display()) });
        }
        Ok(Secret(bytes))
    }

    pub fn expose(&self) -> &[u8] {
        &self.0
    }

    /// The secret as text (a PEM key, a token).
    pub fn text(&self, path: &Path) -> Result<String, ConfigError> {
        // alloc: once at start, the key or token as text
        String::from_utf8(self.0.clone()).map_err(|_| ConfigError {
            what: format!("the secret file {} is not UTF-8", path.display()), // alloc: error path
        })
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(..)")
    }
}

#[cfg(test)]
mod tests;
