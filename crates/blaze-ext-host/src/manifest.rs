//! Which extensions a workspace uses: `[workspace.metadata.blaze.extensions]` of its root
//! manifest.
//!
//! ```toml
//! [workspace.metadata.blaze.extensions.panic-in-drop]
//! path = "extensions/panic-in-drop"   # or: version = "0.3.1"   or: git = "..", rev = ".."
//! grants = []                         # capabilities by name; none by default
//! replay = false                      # run every job twice and compare
//! lints = { panic_in_drop_deep = "warn" }
//! ```
//!
//! A table under `metadata`, not a dependency kind: cargo and every tool that reads a
//! manifest skip `metadata` and choke on a table they do not know, and an extension is not a
//! dependency of any crate of the workspace (nothing links it, its features unify with
//! nothing). An unknown key is refused, never skipped. The text is given, not read here.

use std::fmt;

use blaze_ext::Level;

/// One extension a workspace names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Declared {
    pub name: String,
    pub source: Source,
    /// The capabilities granted, by name, sorted: part of the build's key.
    pub grants: Vec<String>,
    /// The levels the workspace sets, by lint name, sorted.
    pub levels: Vec<(String, Level)>,
    /// Every job runs twice, the arena at other addresses the second time, and a difference
    /// is an error naming the job.
    pub replay: bool,
}

/// Where an extension's source is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// A directory, from the workspace's root.
    Path(String),
    /// A registry's package at a version, pinned in `blaze.lock`.
    Registry { version: String },
    /// A git revision.
    Git { url: String, rev: String },
}

/// A manifest whose extensions do not read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManifestError {
    /// The manifest, as the caller shows it.
    pub manifest: String,
    /// The key, from the manifest's root.
    pub at: String,
    pub why: String,
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: `{}`: {}", self.manifest, self.at, self.why)
    }
}

impl std::error::Error for ManifestError {}

/// The capabilities a manifest can grant, each by the word before its `:`: what bstd's
/// capability types are (docs/v2/plan/design/std.md). A name outside the list is a refusal.
pub const GRANTS: &[&str] = &["fs-read", "fs-write", "net", "clock", "env", "process", "out"];

const TABLE: &str = "workspace.metadata.blaze.extensions";

/// The extensions the manifest `text` names, sorted by name. `shown` is how an error names
/// the manifest.
pub fn declared(text: &str, shown: &str) -> Result<Vec<Declared>, ManifestError> {
    // alloc: an error's text, the edge
    let wrong = |at: String, why: String| ManifestError { manifest: shown.to_owned(), at, why };
    let root = text
        .parse::<toml::Table>()
        .map_err(|error| wrong(String::new(), error.message().to_owned()))?; // alloc: error path
    let table = root
        .get("workspace")
        .and_then(|it| it.get("metadata"))
        .and_then(|it| it.get("blaze"))
        .and_then(|it| it.get("extensions"));
    let Some(table) = table else { return Ok(Vec::new()) }; // alloc: none
    let Some(table) = table.as_table() else {
        return Err(wrong(TABLE.to_owned(), "expected a table of extensions".to_owned())); // alloc: error path
    };
    let mut found = Vec::with_capacity(table.len()); // alloc: the workspace's extensions, a few
    for (name, of) in table {
        let at = format!("{TABLE}.{name}");
        let Some(of) = of.as_table() else {
            return Err(wrong(at, "expected a table: `{ path = \"..\" }`".to_owned())); // alloc: error path
        };
        for key in of.keys() {
            if !["path", "version", "git", "rev", "grants", "replay", "lints"]
                .contains(&key.as_str())
            {
                return Err(wrong(
                    format!("{at}.{key}"),
                    "not a key of an extension: `path`, `version`, `git`, `rev`, `grants`, `replay`, `lints`".to_owned(), // alloc: error path
                ));
            }
        }
        let text_of = |key: &str| -> Result<Option<String>, ManifestError> {
            match of.get(key) {
                None => Ok(None),
                Some(it) => match it.as_str() {
                    Some(it) => Ok(Some(it.to_owned())), // alloc: the manifest's value, kept
                    None => Err(wrong(format!("{at}.{key}"), "expected a string".to_owned())), // alloc: error path
                },
            }
        };
        let source = match (text_of("path")?, text_of("version")?, text_of("git")?, text_of("rev")?)
        {
            (Some(path), None, None, None) => Source::Path(path),
            (None, Some(version), None, None) => Source::Registry { version },
            (None, None, Some(url), Some(rev)) => Source::Git { url, rev },
            _ => {
                return Err(wrong(
                    at,
                    "expected exactly one source: `path`, or `version`, or `git` with `rev`"
                        .to_owned(), // alloc: error path
                ));
            }
        };
        let mut grants = Vec::new(); // alloc: an extension's grants, a few
        if let Some(listed) = of.get("grants") {
            let Some(listed) = listed.as_array() else {
                return Err(wrong(
                    format!("{at}.grants"),
                    "expected an array of strings".to_owned(),
                )); // alloc: error path
            };
            for grant in listed {
                let Some(grant) = grant.as_str() else {
                    return Err(wrong(
                        format!("{at}.grants"),
                        "expected an array of strings".to_owned(),
                    )); // alloc: error path
                };
                let kind = grant.split(':').next().unwrap_or(grant);
                if !GRANTS.contains(&kind) {
                    return Err(wrong(
                        format!("{at}.grants"),
                        format!("`{grant}` is not a capability: one of {}", GRANTS.join(", ")),
                    ));
                }
                grants.push(grant.to_owned()); // alloc: the manifest's value, kept
            }
        }
        grants.sort_unstable();
        grants.dedup();
        let replay = match of.get("replay") {
            None => false,
            Some(it) => match it.as_bool() {
                Some(it) => it,
                None => {
                    return Err(wrong(
                        format!("{at}.replay"),
                        "expected `true` or `false`".to_owned(),
                    ));
                } // alloc: error path
            },
        };
        let mut levels = Vec::new(); // alloc: the levels a workspace sets, a few
        if let Some(lints) = of.get("lints") {
            let Some(lints) = lints.as_table() else {
                return Err(wrong(format!("{at}.lints"), "expected a table of levels".to_owned())); // alloc: error path
            };
            for (lint, level) in lints {
                let level = level.as_str().and_then(Level::named);
                let Some(level) = level else {
                    return Err(wrong(
                        format!("{at}.lints.{lint}"),
                        "expected a level: `allow`, `warn` or `deny`".to_owned(), // alloc: error path
                    ));
                };
                levels.push((lint.clone(), level)); // clone: the manifest's key, kept
            }
        }
        levels.sort_unstable();
        found.push(Declared { name: name.clone(), source, grants, levels, replay }); // clone: the manifest's key, kept
    }
    found.sort_unstable_by(|a, b| a.name.cmp(&b.name));
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_workspace_names_its_extensions_and_an_unknown_key_is_refused() {
        let text = r#"
[workspace.metadata.blaze.extensions.panic-in-drop]
path = "extensions/panic-in-drop"
lints = { panic_in_drop_deep = "deny" }

[workspace.metadata.blaze.extensions.audit]
git = "https://example.com/audit"
rev = "0123abc"
grants = ["out:lean", "clock"]
replay = true
"#;
        let found = declared(text, "Cargo.toml").expect("the manifest reads");
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].name, "audit");
        assert_eq!(found[0].grants, ["clock", "out:lean"]);
        assert!(found[0].replay);
        assert_eq!(found[1].source, Source::Path("extensions/panic-in-drop".to_owned()));
        assert_eq!(found[1].levels, [("panic_in_drop_deep".to_owned(), Level::Deny)]);
        assert_eq!(declared("[package]\nname = \"x\"\n", "Cargo.toml"), Ok(Vec::new()));

        let unknown = "[workspace.metadata.blaze.extensions.x]\npath = \"x\"\nsandbox = false\n";
        let error = declared(unknown, "Cargo.toml").expect_err("an unknown key");
        assert_eq!(error.at, "workspace.metadata.blaze.extensions.x.sandbox");
        let grant = "[workspace.metadata.blaze.extensions.x]\npath = \"x\"\ngrants = [\"root\"]\n";
        let error = declared(grant, "Cargo.toml").expect_err("an unknown capability");
        assert!(error.to_string().contains("`root` is not a capability"), "{error}");
        let two = "[workspace.metadata.blaze.extensions.x]\npath = \"x\"\nversion = \"1\"\n";
        assert!(declared(two, "Cargo.toml").is_err());
    }
}
