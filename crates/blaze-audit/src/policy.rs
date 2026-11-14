//! What a workspace says about its audit, in `[workspace.metadata.blaze.audit]` of its root
//! `Cargo.toml`, and what each finding counts as under it.
//!
//! ```toml
//! [workspace.metadata.blaze.audit]
//! database = "../advisory-db"                      # or BLAZE_AUDIT_DB; relative to the root
//! allow-unpinned-paths = ["local-fork"]            # outside path dependencies, by package
//! accept = [
//!   { id = "RUSTSEC-2020-0071", reason = "time 0.1 only in a dev tool", until = "2027-01-31" },
//! ]
//! ```
//!
//! A vulnerability and an unsound advisory are errors; unmaintained and notice are warnings;
//! a yanked version is an error. An acceptance needs a reason and an expiry: through its
//! `until` day it turns its advisory's findings into accepted ones; from the day after, they
//! are errors again and say the acceptance ran out. An acceptance that matches no finding is
//! reported as stale, so a list of exceptions cannot outlive what it excused.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::alloc::Global;
use std::fmt;

use crate::advisory::is_id;
use crate::date::Date;

/// One acceptance of an advisory.
#[derive(Clone, PartialEq, Eq, Debug, serde::Serialize)]
pub struct Accept {
    pub id: String,
    pub reason: String,
    #[serde(serialize_with = "crate::report::as_text")]
    pub until: Date,
}

/// The workspace's audit settings.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Settings {
    /// The database's directory as written, relative to the workspace root.
    pub database: Option<String>,
    /// Packages whose outside path dependency is allowed unpinned, sorted.
    pub allow_unpinned_paths: Vec<String, Global>,
    /// Sorted by id; each id once.
    pub accept: Vec<Accept, Global>,
}

/// Why the settings were not read: the key and what is wrong with it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SettingsRefused {
    pub key: String,
    pub why: String,
}

impl fmt::Display for SettingsRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "`workspace.metadata.blaze.audit.{}` {}", self.key, self.why)
    }
}

impl Settings {
    /// The `[workspace.metadata.blaze.audit]` table, or `None` when the workspace has none.
    pub fn parse(table: Option<&toml::Value>) -> Result<Settings, SettingsRefused> {
        let refuse = |key: &str, why: &str| SettingsRefused { key: key.to_owned(), why: why.to_owned() };
        let mut settings = Settings::default();
        let Some(table) = table else {
            return Ok(settings);
        };
        let table = table.as_table().ok_or_else(|| SettingsRefused {
            key: String::new(),
            why: "is not a table".to_owned(),
        })?;
        for (key, value) in table {
            match key.as_str() {
                "database" => {
                    let path = value.as_str().ok_or_else(|| refuse(key, "is not a string"))?;
                    if path.is_empty() {
                        return Err(refuse(key, "is empty"));
                    }
                    settings.database = Some(path.to_owned());
                }
                "allow-unpinned-paths" => {
                    let list = value.as_array().ok_or_else(|| refuse(key, "is not a list of package names"))?;
                    for name in list {
                        let name = name.as_str().ok_or_else(|| refuse(key, "has an item that is not a package name"))?;
                        settings.allow_unpinned_paths.push(name.to_owned());
                    }
                    settings.allow_unpinned_paths.sort();
                    settings.allow_unpinned_paths.dedup();
                }
                "accept" => {
                    let list = value.as_array().ok_or_else(|| refuse(key, "is not a list of `{ id, reason, until }`"))?;
                    for (at, item) in list.iter().enumerate() {
                        settings.accept.push(accept(at, item)?);
                    }
                    settings.accept.sort_by(|a, b| a.id.cmp(&b.id));
                    if let Some(pair) = settings.accept.windows(2).find(|it| it[0].id == it[1].id) {
                        return Err(refuse(key, &format!("accepts `{}` twice: one acceptance, one reason", pair[0].id)));
                    }
                }
                other => {
                    return Err(refuse(
                        other,
                        "is not a setting of `blaze audit` (`database`, `allow-unpinned-paths` and `accept` are)",
                    ));
                }
            }
        }
        Ok(settings)
    }

    /// The acceptance of advisory `id`, if any.
    pub fn accepted(&self, id: &str) -> Option<&Accept> {
        let at = self.accept.binary_search_by(|it| it.id.as_str().cmp(id)).ok()?;
        self.accept.get(at)
    }

    pub fn allows_unpinned_path(&self, package: &str) -> bool {
        self.allow_unpinned_paths.binary_search_by(|it| it.as_str().cmp(package)).is_ok()
    }
}

fn accept(at: usize, item: &toml::Value) -> Result<Accept, SettingsRefused> {
    let key = format!("accept[{at}]");
    let refuse = |why: String| SettingsRefused { key: key.clone(), why };
    let table = item.as_table().ok_or_else(|| refuse("is not a table `{ id, reason, until }`".to_owned()))?;
    if let Some(other) = table.keys().find(|it| !["id", "reason", "until"].contains(&it.as_str())) {
        return Err(refuse(format!("has `{other}`, which is not known (`id`, `reason` and `until` are)")));
    }
    let field = |name: &str| -> Result<String, SettingsRefused> {
        match table.get(name) {
            Some(toml::Value::String(it)) => Ok(it.clone()),
            Some(toml::Value::Datetime(it)) => Ok(it.to_string()),
            Some(_) => Err(refuse(format!("has `{name}`, which is not a string"))),
            None => Err(refuse(format!("has no `{name}`: an acceptance needs an id, a reason and an expiry"))),
        }
    };
    let id = field("id")?;
    if !is_id(&id) {
        return Err(refuse(format!("has the id `{id}`, which is no RUSTSEC-YYYY-NNNN")));
    }
    let reason = field("reason")?;
    if reason.trim().is_empty() {
        return Err(refuse(format!("accepts `{id}` with an empty reason: say why it is safe here")));
    }
    let until = field("until")?;
    let until = Date::parse(&until).map_err(|why| refuse(format!("accepts `{id}` until {why}")))?;
    Ok(Accept { id, reason, until })
}

/// How a finding counts.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Error,
    Warning,
    /// An error or warning a current acceptance covers.
    Accepted,
}

/// What an acceptance does to a finding of `base` severity on `today`: the severity, and
/// whether the acceptance ran out (`Some(until)` when it did).
pub fn judged(base: Severity, accept: Option<&Accept>, today: Date) -> (Severity, Option<Date>) {
    match accept {
        None => (base, None),
        Some(it) if today <= it.until => (Severity::Accepted, None),
        Some(it) => (Severity::Error, Some(it.until)),
    }
}
