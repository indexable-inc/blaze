//! `Cargo.lock`'s rows, read from TEXT: what a row digests for each crate from outside the
//! workspace its closure reaches.

use blaze_own::{Alloc, Vec};
use blaze_toml::{Document, Kind, Limits};

use crate::{Refused, Text, text};

const PATH: &str = "Cargo.lock";

/// One `[[package]]` row.
pub(crate) struct LockRow<'a> {
    pub name: Text<'a>,
    pub version: Text<'a>,
    /// `None` for a workspace crate (its source is digested as files, not by the lock).
    pub source: Option<Text<'a>>,
    pub checksum: Option<Text<'a>>,
    /// The names it depends on (an entry `"name"`, `"name version"` or `"name version (source)"`
    /// gives `name`): every version of that name is followed.
    pub deps: Vec<Text<'a>, Alloc<'a>>,
}

/// The lock's rows, sorted by (name, version, source).
pub(crate) fn rows<'a>(source: &str, alloc: Alloc<'a>) -> Result<Vec<LockRow<'a>, Alloc<'a>>, Refused<'a>> {
    let doc =
        Document::parse_in(source, Limits::CARGO, alloc).map_err(|error| Refused::Toml { path: text(PATH, alloc), error })?;
    let shape = |what| Refused::Shape { path: text(PATH, alloc), what };
    let mut out = Vec::new_in(alloc);
    let Some(packages) = doc.root().get("package") else { return Ok(out) };
    if packages.kind() != Kind::Array {
        return Err(shape("`package` as an array of tables"));
    }
    for package in packages.children() {
        let string = |key: &str| package.get(key).and_then(|v| v.as_str());
        let (Some(name), Some(version)) = (string("name"), string("version")) else {
            return Err(shape("a `[[package]]` row with a `name` and a `version` string"));
        };
        let mut deps = Vec::new_in(alloc);
        if let Some(list) = package.get("dependencies") {
            for entry in list.children() {
                let Some(entry) = entry.as_str() else { return Err(shape("`dependencies` as an array of strings")) };
                let name = entry.split(' ').next().unwrap_or(entry);
                deps.push(text(name, alloc));
            }
        }
        out.push(LockRow {
            name: text(name, alloc),
            version: text(version, alloc),
            source: string("source").map(|s| text(s, alloc)),
            checksum: string("checksum").map(|s| text(s, alloc)),
            deps,
        });
    }
    out.sort_unstable_by(|a, b| {
        (a.name.as_str(), a.version.as_str(), a.source.as_deref()).cmp(&(
            b.name.as_str(),
            b.version.as_str(),
            b.source.as_deref(),
        ))
    });
    Ok(out)
}
