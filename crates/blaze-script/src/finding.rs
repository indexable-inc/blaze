//! A finding: one place a ratchet counts, as the record `advise` ranks (AGENTS.md
//! findings-are-records, rule 81; docs/v2/plan/design/std.md, "What an author that does not tire
//! can write"). Every ratchet with sites (`ambient`, `allocs`, `casts`, `clones`, `recursion`,
//! `stops`, `untyped`) answers `{"command": "findings"}` with one [`document`] on standard output;
//! scripts/src/bin/advise.rs reads them all, refines and ranks them. This module is the one home
//! of the schema both sides share.
//!
//! The document, [`SCHEMA`]:
//!
//! ```text
//! {"schema": "blaze-findings/1", "source": "<ratchet>",
//!  "kinds": [{"name", "label", "rule", "instead", "mechanical"}, ..],
//!  "findings": [{"crate", "file", "line", "kind", "detail"}, ..]}
//! ```
//!
//! sorted by crate, file, line, kind and detail, so two runs over one tree print equal bytes. A
//! finding's `kind` is one of its document's kinds' names; `detail` is what the ratchet knows of
//! the site beyond its kind (a clone's `// clone:` comment, a recursive function's name), or null.

use serde_json::{Map, Value};

/// The schema every ratchet's `findings` document says it is.
pub const SCHEMA: &str = "blaze-findings/1";

/// A kind of finding: its name, its label in a sentence ("bare casts"), the rule it comes
/// from, what to write instead in one line, and whether its fix is mechanical: a rewrite a tool
/// makes from the text alone, with no judgement about the program.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Kind {
    pub name: &'static str,
    pub label: &'static str,
    pub rule: &'static str,
    pub instead: &'static str,
    pub mechanical: bool,
}

/// One site: its crate (by its directory under crates/), its file from the root, its line from 1,
/// its kind's name, and what else the ratchet knows of it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Finding {
    pub krate: String,
    pub file: String,
    pub line: usize,
    pub kind: &'static str,
    pub detail: Option<String>,
}

/// The document of `source`'s findings: `kinds` as given, `findings` sorted.
pub fn document(source: &str, kinds: &[Kind], mut findings: Vec<Finding>) -> Value {
    findings.sort();
    let mut out = Map::new();
    out.insert("schema".to_owned(), Value::from(SCHEMA));
    out.insert("source".to_owned(), Value::from(source));
    let kinds = kinds
        .iter()
        .map(|it| {
            let mut kind = Map::new();
            kind.insert("name".to_owned(), Value::from(it.name));
            kind.insert("label".to_owned(), Value::from(it.label));
            kind.insert("rule".to_owned(), Value::from(it.rule));
            kind.insert("instead".to_owned(), Value::from(it.instead));
            kind.insert("mechanical".to_owned(), Value::from(it.mechanical));
            Value::Object(kind)
        })
        .collect();
    out.insert("kinds".to_owned(), Value::Array(kinds));
    let findings = findings
        .into_iter()
        .map(|it| {
            let mut finding = Map::new();
            finding.insert("crate".to_owned(), Value::from(it.krate));
            finding.insert("file".to_owned(), Value::from(it.file));
            finding.insert("line".to_owned(), Value::from(it.line));
            finding.insert("kind".to_owned(), Value::from(it.kind));
            finding.insert("detail".to_owned(), it.detail.map_or(Value::Null, Value::from));
            Value::Object(finding)
        })
        .collect();
    out.insert("findings".to_owned(), Value::Array(findings));
    Value::Object(out)
}

/// Prints [`document`] on standard output, on one line.
pub fn print(source: &str, kinds: &[Kind], findings: Vec<Finding>) {
    println!("{}", document(source, kinds, findings));
}
