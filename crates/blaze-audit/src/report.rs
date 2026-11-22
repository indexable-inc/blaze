//! What `blaze audit` found: one [`Finding`] per (package version, reason), in one order that
//! depends on nothing but the findings, as text for people or as JSON (`--json`), version
//! [`VERSION`]:
//!
//! ```text
//! {"version": 1, "today": DAY, "ok": BOOL, "errors": N, "warnings": N, "accepted": N,
//!  "packages": N,
//!  "database": {"path", "tree", "advisories", "withdrawn", "newest", "age_days"} | null,
//!  "not_checked": [TEXT],
//!  "pinned": [{"package", "source", "pin", "verified"}],
//!  "findings": [{"severity", "rule", "package", "version", "advisory", "title", "said",
//!                "chain", "fix", "compatible", "change", "functions", "url", "aliases",
//!                "cvss", "date", "accepted", "expired"}]}
//! ```
//!
//! `severity` is `error`, `warning` or `accepted`; `ok` is no error. The exit status is
//! nonzero exactly when `ok` is false.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use alloc::{borrow::ToOwned, format, string::String, string::ToString, vec::Vec};
use alloc::alloc::Global;
use core::fmt::Write as _;

use blaze_data::{Map, ToValue, Value};

use crate::policy::{Accept, Severity};

/// The version of [`Report`]'s JSON: bumped when a field changes meaning or goes away.
pub const VERSION: u32 = 1;

/// `key: value` appended to `map`, in insertion order: the JSON's field order is the
/// declaration order, as serde's derive wrote it.
fn put(map: &mut Map<Global>, key: &str, value: Value<Global>) {
    map.insert(blaze_own::String::from_str_in(key, Global), value);
}

fn text(text: &str) -> Value<Global> {
    text.to_value(Global)
}

fn opt_text(text: &Option<String>) -> Value<Global> {
    text.as_deref().to_value(Global)
}

fn texts(texts: &[String]) -> Value<Global> {
    Value::List(blaze_own::Vec::from_iter_in(texts.iter().map(|it| text(it)), Global))
}

impl ToValue<Global> for Severity {
    fn to_value(&self, _: Global) -> Value<Global> {
        text(match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Accepted => "accepted",
        })
    }
}

/// `until` as its `Display`, `YYYY-MM-DD`.
impl ToValue<Global> for Accept {
    fn to_value(&self, _: Global) -> Value<Global> {
        let mut map = Map::with_capacity_in(3, Global);
        put(&mut map, "id", text(&self.id));
        put(&mut map, "reason", text(&self.reason));
        put(&mut map, "until", text(&self.until.to_string()));
        Value::Map(map)
    }
}

impl ToValue<Global> for Finding {
    fn to_value(&self, _: Global) -> Value<Global> {
        let mut map = Map::with_capacity_in(18, Global);
        put(&mut map, "severity", self.severity.to_value(Global));
        put(&mut map, "rule", text(self.rule));
        put(&mut map, "package", text(&self.package));
        put(&mut map, "version", text(&self.version));
        put(&mut map, "advisory", opt_text(&self.advisory));
        put(&mut map, "title", text(&self.title));
        put(&mut map, "said", text(&self.said));
        put(&mut map, "chain", texts(&self.chain));
        put(&mut map, "fix", opt_text(&self.fix));
        put(&mut map, "compatible", Value::Bool(self.compatible));
        put(&mut map, "change", opt_text(&self.change));
        put(&mut map, "functions", texts(&self.functions));
        put(&mut map, "url", opt_text(&self.url));
        put(&mut map, "aliases", texts(&self.aliases));
        put(&mut map, "cvss", opt_text(&self.cvss));
        put(&mut map, "date", opt_text(&self.date));
        put(&mut map, "accepted", self.accepted.to_value(Global));
        put(&mut map, "expired", opt_text(&self.expired));
        Value::Map(map)
    }
}

impl ToValue<Global> for DatabaseFacts {
    fn to_value(&self, _: Global) -> Value<Global> {
        let mut map = Map::with_capacity_in(6, Global);
        put(&mut map, "path", text(&self.path));
        put(&mut map, "tree", text(&self.tree));
        put(&mut map, "advisories", self.advisories.to_value(Global));
        put(&mut map, "withdrawn", self.withdrawn.to_value(Global));
        put(&mut map, "newest", opt_text(&self.newest));
        put(&mut map, "age_days", self.age_days.to_value(Global));
        Value::Map(map)
    }
}

impl ToValue<Global> for PinnedFacts {
    fn to_value(&self, _: Global) -> Value<Global> {
        let mut map = Map::with_capacity_in(4, Global);
        put(&mut map, "package", text(&self.package));
        put(&mut map, "source", text(&self.source));
        put(&mut map, "pin", text(&self.pin));
        put(&mut map, "verified", Value::Bool(self.verified));
        Value::Map(map)
    }
}

impl ToValue<Global> for Report {
    fn to_value(&self, _: Global) -> Value<Global> {
        let mut map = Map::with_capacity_in(11, Global);
        put(&mut map, "version", self.version.to_value(Global));
        put(&mut map, "today", text(&self.today));
        put(&mut map, "ok", Value::Bool(self.ok));
        put(&mut map, "errors", self.errors.to_value(Global));
        put(&mut map, "warnings", self.warnings.to_value(Global));
        put(&mut map, "accepted", self.accepted.to_value(Global));
        put(&mut map, "packages", self.packages.to_value(Global));
        put(&mut map, "database", self.database.to_value(Global));
        put(&mut map, "not_checked", texts(&self.not_checked));
        put(&mut map, "pinned", self.pinned.as_slice().to_value(Global));
        put(&mut map, "findings", self.findings.as_slice().to_value(Global));
        Value::Map(map)
    }
}

/// One finding.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Finding {
    pub severity: Severity,
    /// The rule: one of the crate's rule names ([`crate::WEAK_SOURCE_PIN`], …).
    pub rule: &'static str,
    pub package: String,
    /// The locked version; empty for a path dependency named by its directory.
    pub version: String,
    /// The advisory's id, when an advisory says it.
    pub advisory: Option<String>,
    pub title: String,
    /// The whole sentence: what, where, and what to do.
    pub said: String,
    /// The shortest chain from a workspace member to the package, each `name version`.
    pub chain: Vec<String, Global>,
    /// The least version that fixes it, and whether it is semver-compatible.
    pub fix: Option<String>,
    pub compatible: bool,
    /// The line to change for a compatible fix: where, and what to.
    pub change: Option<String>,
    /// The functions the advisory names as affected. Whether the build reaches them was not
    /// checked.
    pub functions: Vec<String, Global>,
    pub url: Option<String>,
    pub aliases: Vec<String, Global>,
    pub cvss: Option<String>,
    pub date: Option<String>,
    /// The acceptance that covers it (or covered it, when `expired`).
    pub accepted: Option<Accept>,
    /// The day the acceptance ran out, when it did.
    pub expired: Option<String>,
}

impl Finding {
    /// A finding with only what every finding has.
    pub fn new(severity: Severity, rule: &'static str, package: &str, version: &str, said: String) -> Finding {
        Finding {
            severity,
            rule,
            package: package.to_owned(),
            version: version.to_owned(),
            advisory: None,
            title: String::new(),
            said,
            chain: Vec::new_in(Global),
            fix: None,
            compatible: false,
            change: None,
            functions: Vec::new_in(Global),
            url: None,
            aliases: Vec::new_in(Global),
            cvss: None,
            date: None,
            accepted: None,
            expired: None,
        }
    }

    /// The order findings are reported in: errors first, then by package, version, rule and
    /// advisory, then the sentence, so no two findings tie unless they are the same.
    fn key(&self) -> (Severity, &str, &str, &str, Option<&str>, &str) {
        (self.severity, &self.package, &self.version, self.rule, self.advisory.as_deref(), &self.said)
    }
}

/// The advisory database read, as the report says it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DatabaseFacts {
    pub path: String,
    /// Its tree's digest, as `blaze.lock` pins it.
    pub tree: String,
    pub advisories: usize,
    pub withdrawn: usize,
    /// The newest advisory's date; the database is at least `age_days` old.
    pub newest: Option<String>,
    pub age_days: Option<i64>,
}

/// One source the pin check passed.
#[derive(Clone, PartialEq, Eq, Debug, PartialOrd, Ord)]
pub struct PinnedFacts {
    pub package: String,
    pub source: String,
    /// The digest that pins it, or `allow-unpinned-paths`.
    pub pin: String,
    /// Whether the tree on disk was hashed and matched; false when it is not on disk yet.
    pub verified: bool,
}

/// What `blaze audit` found.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Report {
    pub version: u32,
    pub today: String,
    pub ok: bool,
    pub errors: usize,
    pub warnings: usize,
    pub accepted: usize,
    /// Packages of the lock.
    pub packages: usize,
    pub database: Option<DatabaseFacts>,
    /// What was not checked, and why, one sentence each.
    pub not_checked: Vec<String, Global>,
    pub pinned: Vec<PinnedFacts, Global>,
    pub findings: Vec<Finding, Global>,
}

impl Report {
    /// The report of `findings`, put in order and counted.
    pub fn new(
        today: String,
        packages: usize,
        database: Option<DatabaseFacts>,
        mut not_checked: Vec<String, Global>,
        mut pinned: Vec<PinnedFacts, Global>,
        mut findings: Vec<Finding, Global>,
    ) -> Report {
        findings.sort_by(|a, b| a.key().cmp(&b.key()));
        findings.dedup();
        pinned.sort();
        not_checked.sort();
        not_checked.dedup();
        let count = |severity| findings.iter().filter(|it| it.severity == severity).count();
        let (errors, warnings, accepted) = (count(Severity::Error), count(Severity::Warning), count(Severity::Accepted));
        Report {
            version: VERSION,
            today,
            ok: errors == 0,
            errors,
            warnings,
            accepted,
            packages,
            database,
            not_checked,
            pinned,
            findings,
        }
    }

    /// The report as JSON, one line.
    pub fn json(&self) -> String {
        let options = blaze_json::WriteOptions { keys: blaze_json::KeyOrder::Insertion, ..blaze_json::WriteOptions::COMPACT };
        match blaze_json::to_string_in(&self.to_value(Global), options, Global) {
            Ok(json) => json.as_str().to_owned(),
            Err(why) => unreachable!("a report of strings and numbers has a JSON form: {why}"),
        }
    }

    /// The report for people: the answer first, then each finding, then what was not checked.
    pub fn text(&self) -> String {
        let mut out = String::new();
        let plural = |n: usize, one: &str| if n == 1 { format!("1 {one}") } else { format!("{n} {one}s") };
        _ = write!(
            out,
            "blaze audit: {}, {}, {} accepted, over {} locked",
            plural(self.errors, "error"),
            plural(self.warnings, "warning"),
            self.accepted,
            plural(self.packages, "package")
        );
        match &self.database {
            Some(db) => {
                _ = write!(out, "; advisories from `{}` ({}, {} withdrawn", db.path, db.advisories, db.withdrawn);
                if let (Some(newest), Some(age)) = (&db.newest, db.age_days) {
                    _ = write!(out, ", newest {newest}, {age} days old");
                }
                out.push(')');
            }
            None => out.push_str("; no advisory database"),
        }
        out.push('\n');
        for finding in &self.findings {
            let severity = match finding.severity {
                Severity::Error => "error",
                Severity::Warning => "warning",
                Severity::Accepted => "accepted",
            };
            _ = write!(out, "\n{severity}[{}]", finding.rule);
            if let Some(id) = &finding.advisory {
                _ = write!(out, " {id}");
            }
            if finding.package.is_empty() {
            } else if finding.version.is_empty() {
                _ = write!(out, " `{}`", finding.package);
            } else {
                _ = write!(out, " `{} {}`", finding.package, finding.version);
            }
            if !finding.title.is_empty() {
                _ = write!(out, ": {}", finding.title);
            }
            out.push('\n');
            _ = writeln!(out, "  {}", finding.said);
            if finding.chain.len() > 1 {
                _ = writeln!(out, "  via {}", finding.chain.join(" → "));
            }
            match (&finding.fix, &finding.change) {
                (Some(fix), Some(change)) => {
                    _ = writeln!(out, "  fixed in {fix}: {change}");
                }
                (Some(fix), None) => {
                    _ = writeln!(out, "  fixed in {fix}, a semver-incompatible upgrade");
                }
                (None, _) if finding.advisory.is_some() && finding.rule != crate::UNMAINTAINED => {
                    _ = writeln!(out, "  no fixed version");
                }
                (None, _) => {}
            }
            if !finding.functions.is_empty() {
                _ = writeln!(out, "  affected functions: {} (whether the build reaches them was not checked)", finding.functions.join(", "));
            }
            if let Some(accept) = &finding.accepted {
                match &finding.expired {
                    Some(day) => {
                        _ = writeln!(out, "  accepted until {day}, which has passed: {}", accept.reason);
                    }
                    None => {
                        _ = writeln!(out, "  accepted until {}: {}", accept.until, accept.reason);
                    }
                }
            }
            if let Some(url) = &finding.url {
                _ = writeln!(out, "  {url}");
            }
        }
        if !self.not_checked.is_empty() {
            out.push('\n');
            for said in &self.not_checked {
                _ = writeln!(out, "not checked: {said}");
            }
        }
        out
    }
}
