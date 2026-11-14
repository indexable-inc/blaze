//! The grants table a workspace needs today ([`grants`]): what `blaze audit grants` prints for a
//! person to read, give reasons to and add to the root manifest.

use std::{alloc::Global, fmt::Write as _};

use super::{
    capability::Capability,
    facts::{Used, Workspace},
    grants::TABLE,
};

/// The grants a workspace needs for what its dependencies use now.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Table {
    /// One row per dependency that uses anything, by name then version.
    pub rows: Box<[Row]>,
}

/// One dependency's grant, as it would be written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub name: Box<str>,
    /// The exact version, pinned so a new version is not granted unseen.
    pub version: Option<Box<str>>,
    /// What it uses, each with one witness, in [`Capability::ALL`]'s order.
    pub uses: Box<[Used]>,
}

/// The grants table for what `workspace`'s dependencies use now: one row per dependency that
/// uses any capability, each capability with one witness; a dependency that uses nothing needs
/// no row. Pure: the same facts give the same table, byte for byte.
///
/// Caller: `blaze audit grants`, which calls it on [`super::facts::workspace`] and prints
/// [`Table::toml`], the TOML to add to the root manifest.
pub fn grants(workspace: &Workspace) -> Table {
    let mut rows: Vec<Row> = Vec::new_in(Global);
    for dep in &workspace.deps {
        if dep.uses.is_empty() {
            continue;
        }
        rows.push(Row { name: dep.name.clone(), version: dep.version.clone(), uses: dep.uses.clone() });
    }
    Table { rows: rows.into_boxed_slice() }
}

impl Table {
    /// The table as TOML: each grant with its witnesses as comments above it, and an empty
    /// `reason` the person must write (an empty reason is refused when the table is read, so
    /// nothing is granted without one). Two versions of one package are one entry: the TOML
    /// table holds a name once, so the entry has no pin and its comments name both versions.
    pub fn toml(&self) -> String {
        let mut out = String::new();
        out.push_str(TABLE);
        out.push('\n');
        let mut at = 0;
        while at < self.rows.len() {
            let name = &self.rows[at].name;
            let mut end = at + 1;
            while end < self.rows.len() && self.rows[end].name == *name {
                end += 1;
            }
            let rows = &self.rows[at..end];
            let mut allow: Vec<Capability> = Vec::new_in(Global);
            for row in rows {
                for used in &row.uses {
                    let version = row.version.as_deref().map_or(String::new(), |it| format!(" {it}"));
                    // `write!` to a `String` cannot fail.
                    let _ = writeln!(
                        out,
                        "# {}{version}: {}: {}",
                        row.name,
                        used.capability.word(),
                        used.witness
                    );
                    if !allow.contains(&used.capability) {
                        allow.push(used.capability);
                    }
                }
            }
            allow.sort();
            let mut words = String::new();
            for (at, capability) in allow.iter().enumerate() {
                if at > 0 {
                    words.push_str(", ");
                }
                let _ = write!(words, "\"{}\"", capability.word());
            }
            let pin = match rows {
                [only] => only.version.as_deref().map_or(String::new(), |it| format!(", version = \"={it}\"")),
                _ => String::new(),
            };
            let _ = writeln!(out, "{} = {{ allow = [{words}], reason = \"\"{pin} }}", key(name));
            at = end;
        }
        out
    }
}

/// `name` as a TOML key: bare when it can be, quoted when not.
fn key(name: &str) -> String {
    let bare = !name.is_empty()
        && name.bytes().all(|it| it.is_ascii_alphanumeric() || it == b'-' || it == b'_');
    if bare { name.to_owned() } else { format!("\"{}\"", name.replace('\\', "\\\\").replace('"', "\\\"")) }
}
