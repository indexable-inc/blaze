//! What a workspace grants its dependencies, read from its root manifest's
//! `[workspace.metadata.blaze.trust]` ([`Grants`]): one entry per dependency, by its package
//! name, with the capabilities it may use, why, and optionally the one version the grant is for.
//!
//! ```toml
//! [workspace.metadata.blaze.trust]
//! serde_json = { allow = [], reason = "parses JSON; no IO" }
//! libc = { allow = ["unsafe", "ffi"], reason = "the C library's bindings" }
//! left-pad = { allow = [], reason = "pads", version = "=1.3.0" }
//! ```
//!
//! A name is read with `-` and `_` the same, as cargo reads a package name. Every malformed
//! entry is an error naming the manifest, the entry and what is wrong: nothing is skipped.

use std::alloc::Global;

use super::capability::{Capability, Caps};

/// Where the table is, as a person writes it.
pub const TABLE: &str = "[workspace.metadata.blaze.trust]";

/// The crates the toolchain grants every capability: never written in the table.
pub const TOOLCHAIN: &[&str] = &["std", "core", "alloc", "bstd", "proc_macro", "test"];

/// What a workspace's root manifest grants.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Grants {
    /// The root manifest's path, as findings name it.
    pub manifest: Box<str>,
    /// The table's entries, sorted by name; `None` when the manifest has no table.
    pub table: Option<Box<[Grant]>>,
}

/// One dependency's grant.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Grant {
    /// The package name as written.
    pub name: Box<str>,
    pub allow: Caps,
    pub reason: Box<str>,
    /// The exact version the grant is for, without its `=`: another version is not granted.
    pub version: Option<Box<str>>,
}

impl Grants {
    /// The grants of the manifest at `manifest` whose `[workspace.metadata]` is `metadata`.
    pub fn of_metadata(manifest: &str, metadata: Option<&toml::Value>) -> Result<Grants, String> {
        let table = metadata.and_then(|it| it.get("blaze")).and_then(|it| it.get("trust"));
        let Some(table) = table else { return Ok(Grants { manifest: manifest.into(), table: None }) };
        let wrong = |what: String| format!("{manifest}: {TABLE}: {what}");
        let Some(table) = table.as_table() else {
            return Err(wrong(format!("is a {}, not a table", table.type_str())));
        };
        let mut grants: Vec<Grant> = Vec::with_capacity_in(table.len(), Global);
        for (name, entry) in table {
            let grant = Grant::of(name, entry).map_err(|why| wrong(format!("`{name}`: {why}")))?;
            if let Some(other) = grants.iter().find(|it| same_name(&it.name, name)) {
                return Err(wrong(format!(
                    "`{name}` and `{}` name one package: write one entry",
                    other.name
                )));
            }
            grants.push(grant);
        }
        grants.sort_by(|a, b| order(&a.name, &b.name));
        Ok(Grants { manifest: manifest.into(), table: Some(grants.into_boxed_slice()) })
    }

    /// The grants of the root manifest at `manifest`, whose text is `text`.
    pub fn of_text(manifest: &str, text: &str) -> Result<Grants, String> {
        let root: toml::Table = text.parse().map_err(|why| format!("{manifest}: {why}"))?;
        let metadata = root.get("workspace").and_then(|it| it.get("metadata"));
        Grants::of_metadata(manifest, metadata)
    }

    /// The grant of the package `name`, read with `-` and `_` the same.
    pub fn of(&self, name: &str) -> Option<&Grant> {
        self.table.as_deref()?.iter().find(|it| same_name(&it.name, name))
    }
}

impl Grant {
    fn of(name: &str, entry: &toml::Value) -> Result<Grant, String> {
        let Some(entry) = entry.as_table() else {
            return Err(format!(
                "is a {}, not `{{ allow = [..], reason = \"..\" }}`",
                entry.type_str()
            ));
        };
        let known = |key: &String| matches!(key.as_str(), "allow" | "reason" | "version");
        if let Some(key) = entry.keys().find(|it| !known(it)) {
            return Err(format!(
                "`{key}` is no key of a grant: `allow`, `reason` and `version` are"
            ));
        }
        if TOOLCHAIN.iter().any(|it| same_name(it, name)) {
            return Err(
                "is the toolchain's, which grants it every capability: remove the entry".to_owned()
            );
        }
        let allow = match entry.get("allow") {
            None => return Err("has no `allow`: write `allow = []` for none".to_owned()),
            Some(toml::Value::Array(words)) => {
                let mut allow = Caps::EMPTY;
                for word in words {
                    let Some(word) = word.as_str() else {
                        return Err(format!("`allow` holds a {}, not a capability", word.type_str()));
                    };
                    let Some(capability) = Capability::of_word(word) else {
                        return Err(format!("`{word}` is no capability: {}", words_known()));
                    };
                    allow = allow.union(Caps::one(capability));
                }
                allow
            }
            Some(other) => return Err(format!("`allow` is a {}, not a list", other.type_str())),
        };
        let reason = match entry.get("reason") {
            Some(toml::Value::String(reason)) if !reason.trim().is_empty() => reason.as_str(),
            Some(toml::Value::String(_)) | None => {
                return Err(
                    "has no `reason`: say why the package is trusted with what it may do"
                        .to_owned(),
                );
            }
            Some(other) => return Err(format!("`reason` is a {}, not a string", other.type_str())),
        };
        let version = match entry.get("version") {
            None => None,
            Some(toml::Value::String(version)) => match version.strip_prefix('=') {
                Some(exact) if !exact.trim().is_empty() => Some(exact.trim().into()),
                _ => {
                    return Err(format!(
                        "`version = \"{version}\"` is no exact version: write \
                         `version = \"={version}\"`, the one version the grant is for"
                    ));
                }
            },
            Some(other) => return Err(format!("`version` is a {}, not a string", other.type_str())),
        };
        Ok(Grant { name: name.into(), allow, reason: reason.into(), version })
    }

    /// Whether the grant is for `version`, the dependency's version as the build has it.
    pub fn covers(&self, version: Option<&str>) -> bool {
        match &self.version {
            None => true,
            Some(exact) => version == Some(&**exact),
        }
    }
}

/// The capability words, for an error.
fn words_known() -> String {
    let mut out = String::from("the capabilities are ");
    for (at, capability) in Capability::ALL.iter().enumerate() {
        if at > 0 {
            out.push_str(", ");
        }
        out.push_str(capability.word());
    }
    out
}

/// A byte of a package name, `-` read as `_`.
const fn folded(byte: u8) -> u8 {
    if byte == b'-' { b'_' } else { byte }
}

/// Whether `a` and `b` name one package: `-` and `_` the same.
pub fn same_name(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).all(|(a, b)| folded(a) == folded(b))
}

/// The order of package names: `-` read as `_`.
pub fn order(a: &str, b: &str) -> std::cmp::Ordering {
    a.bytes().map(folded).cmp(b.bytes().map(folded))
}
