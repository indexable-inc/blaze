//! One manifest parsed ([`ManifestQuery`]: blaze-manifest's `TomlManifest`, kept, so no step
//! parses it again), and the workspace: its root, members and default members
//! ([`WorkspaceQuery`]). A later step reads the same workspace again in one pass
//! ([`in_workspace`]), every file through the database.

use std::path::Path;

use blaze_format::SchemaHash;
use blaze_ids::ContentId;
use blaze_manifest::files::Files;
use blaze_manifest::{Context, Workspace};
use blaze_query::{Cancelled, Cx, Query, QueryKind};
use cargo_util_schemas::manifest::TomlManifest;
use serde::{Deserialize, Serialize};

use super::config::{ConfigChain, ConfigOf, ConfigRead};
use super::files::{Found, PathKey, Snap, TomlFile, lent, read_through};
use super::invocation::Asking;
use super::load_error;
use super::key_id;
use crate::query::{Cbor, LoadError, Versioned, producer};

/// A manifest as its [`ManifestQuery`] parsed it.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub(crate) enum Parsed {
    Here(#[serde(with = "in_order")] TomlManifest),
    Absent,
    /// Under no mount of the view.
    Unmounted,
    /// Not UTF-8 or not a manifest: reading it again gives the error.
    Unparsed,
}

impl Versioned for Parsed {
    const SCHEMA: SchemaHash = blaze_format::schema("blaze v2 cargo manifest parsed 2");
}

/// A manifest kept with every table in the file's order. The deterministic encoding sorts a
/// map's keys, but `package.metadata` and `workspace.metadata` keep the file's order (as
/// `cargo metadata` prints them), so each table is kept as the list of its entries.
mod in_order {
    use cargo_util_schemas::manifest::TomlManifest;
    use serde::de::Error as _;
    use serde::ser::Error as _;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use toml::Value;

    #[derive(Serialize, Deserialize)]
    enum Kept {
        String(String),
        Integer(i64),
        /// The float's bits, so every one (a NaN's sign, `-0.0`) reads back as it was.
        Float(u64),
        Boolean(bool),
        Datetime(String),
        Array(Vec<Kept>),
        Table(Vec<(String, Kept)>),
    }

    fn kept(value: Value) -> Kept {
        match value {
            Value::String(it) => Kept::String(it),
            Value::Integer(it) => Kept::Integer(it),
            Value::Float(it) => Kept::Float(it.to_bits()),
            Value::Boolean(it) => Kept::Boolean(it),
            Value::Datetime(it) => Kept::Datetime(it.to_string()),
            Value::Array(it) => Kept::Array(it.into_iter().map(kept).collect()),
            Value::Table(it) => Kept::Table(it.into_iter().map(|(key, value)| (key, kept(value))).collect()),
        }
    }

    fn value(kept: Kept) -> Result<Value, String> {
        Ok(match kept {
            Kept::String(it) => Value::String(it),
            Kept::Integer(it) => Value::Integer(it),
            Kept::Float(it) => Value::Float(f64::from_bits(it)),
            Kept::Boolean(it) => Value::Boolean(it),
            Kept::Datetime(it) => Value::Datetime(it.parse().map_err(|e| format!("the kept datetime `{it}`: {e}"))?),
            Kept::Array(it) => Value::Array(it.into_iter().map(value).collect::<Result<_, _>>()?),
            Kept::Table(it) => {
                let mut table = toml::Table::with_capacity(it.len());
                for (key, kept) in it {
                    if table.insert(key.clone(), value(kept)?).is_some() {
                        return Err(format!("the kept table has the key `{key}` twice"));
                    }
                }
                Value::Table(table)
            }
        })
    }

    pub fn serialize<S: Serializer>(manifest: &TomlManifest, to: S) -> Result<S::Ok, S::Error> {
        kept(Value::try_from(manifest).map_err(S::Error::custom)?).serialize(to)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(from: D) -> Result<TomlManifest, D::Error> {
        let value = value(Kept::deserialize(from)?).map_err(D::Error::custom)?;
        value.try_into().map_err(D::Error::custom)
    }
}

/// A manifest by absolute path, read with a configuration's mounts, parsed as blaze-manifest's
/// `Files::manifest` parses it. It reads its [`TomlFile`], so the canonical form's bytes are
/// what an edit is seen by: a comment reruns the file and stops there.
pub(crate) struct ManifestQuery;

impl Query for ManifestQuery {
    const KIND: QueryKind = QueryKind(0x6016);
    const NAME: &'static str = "cargo manifest";
    const PRODUCER: ContentId = producer("blaze v2 cargo manifest 3");
    type Key = PathKey;
    type Value = Cbor<Parsed>;

    fn input(key: &PathKey) -> ContentId {
        key_id("blaze v2 cargo manifest key 3", key)
    }

    fn execute(cx: &mut Cx<'_>, key: &PathKey) -> Result<Cbor<Parsed>, Cancelled> {
        let kept = cx.get::<TomlFile>(key)?.bytes();
        Ok(Cbor(match lent(kept) {
            Found::Here(bytes) => match blaze_manifest::manifest::parse_manifest(Path::new(&key.path), bytes) {
                Ok(manifest) => Parsed::Here(manifest),
                Err(_) => Parsed::Unparsed,
            },
            Found::Absent => Parsed::Absent,
            Found::Unmounted => Parsed::Unmounted,
        }))
    }
}

/// A workspace: from its manifest (none: the one cargo finds from the cwd, or
/// `--manifest-path`), with a configuration.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) struct WorkspaceKey {
    pub manifest: Option<String>,
    pub config: ConfigOf,
}

impl WorkspaceKey {
    pub fn view() -> WorkspaceKey {
        WorkspaceKey { manifest: None, config: ConfigOf::View }
    }
}

/// A workspace as read: where it is and its members.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub(crate) struct WorkspaceRead {
    pub config: ConfigRead,
    /// The manifest cargo was pointed at, and the root's.
    pub current: String,
    pub root_manifest: String,
    pub members: Vec<String>,
    pub default_members: Vec<String>,
    pub is_virtual: bool,
    /// cargo's warnings of the workspace and its manifests, as it prints them after `warning: `.
    pub warnings: Vec<String>,
}

impl Versioned for Result<WorkspaceRead, LoadError> {
    const SCHEMA: SchemaHash = blaze_format::schema("blaze v2 cargo workspace read 3");
}

pub(crate) struct WorkspaceQuery;

impl Query for WorkspaceQuery {
    const KIND: QueryKind = QueryKind(0x6017);
    const NAME: &'static str = "cargo workspace";
    const PRODUCER: ContentId = producer("blaze v2 cargo workspace 3");
    type Key = WorkspaceKey;
    type Value = Cbor<Result<WorkspaceRead, LoadError>>;

    fn input(key: &WorkspaceKey) -> ContentId {
        key_id("blaze v2 cargo workspace key 2", key)
    }

    fn execute(cx: &mut Cx<'_>, key: &WorkspaceKey) -> Result<Cbor<Result<WorkspaceRead, LoadError>>, Cancelled> {
        let config = match cx.get::<ConfigChain>(&key.config)?.get() {
            Ok(config) => config,
            Err(error) => return Ok(Cbor(Err(error))),
        };
        let manifest_path = match (&key.manifest, &key.config) {
            (Some(path), _) => Some(path.clone()),
            (None, ConfigOf::View) => match cx.get::<Asking>(&())?.get() {
                Ok(asked) => asked.manifest_path.map(|it| match it.starts_with('/') {
                    true => it,
                    false => format!("{}/{it}", asked.cwd),
                }),
                Err(error) => return Ok(Cbor(Err(error))),
            },
            (None, ConfigOf::Fixed { .. }) => None,
        };
        let found = read_through(cx, &key.config, |files| Ok(read_workspace(&config, files, manifest_path.as_deref())))?;
        Ok(Cbor(found.and_then(|it| it)))
    }
}

fn read_workspace(config: &ConfigRead, files: Snap<'_, '_>, manifest_path: Option<&str>) -> Result<WorkspaceRead, LoadError> {
    let ctx = Context::new(super::config::build(config, files).map_err(|error| load_error(&error, None))?);
    let current = root_manifest(manifest_path, &config.cwd, &files)?;
    let ws = Workspace::new(Path::new(&current), &ctx).map_err(|error| load_error(&error, Some(current.clone())))?;
    let show = |path: &Path| path.display().to_string();
    let members: Vec<String> = ws.members().map(|(path, _)| show(path)).collect();
    let single = members.len() == 1 && ws.root_manifest() == ws.current_manifest();
    let mut warnings: Vec<String> = ws.workspace_warnings().to_vec();
    for (path, warning) in ws.warnings() {
        warnings.push(match single {
            true => warning.to_owned(),
            false => format!("{}: {warning}", path.display()),
        });
    }
    Ok(WorkspaceRead {
        // clone: the configuration's inputs, which the workspace's value keeps.
        config: config.clone(),
        current: show(ws.current_manifest()),
        root_manifest: show(ws.root_manifest()),
        default_members: ws.default_members().map(|(path, _)| show(path)).collect(),
        members,
        is_virtual: ws.is_virtual(),
        warnings,
    })
}

/// The manifest cargo reads: `--manifest-path`'s, or the nearest `Cargo.toml` from the cwd up,
/// as cargo's `find_root_manifest_for_wd` and `root_manifest` say it.
fn root_manifest<F: Files>(manifest_path: Option<&str>, cwd: &str, files: &F) -> Result<String, LoadError> {
    use blaze_manifest::files::FileKind;
    if let Some(path) = manifest_path {
        let path = super::files::normal(Path::new(path)).unwrap_or_else(|| path.to_owned());
        if !path.ends_with("Cargo.toml") {
            return Err(LoadError { what: "the manifest-path must be a path to a Cargo.toml file".to_owned(), at: Some(path) });
        }
        if files.kind(Path::new(&path)).is_none() {
            return Err(LoadError { what: format!("manifest path `{path}` does not exist"), at: Some(path) });
        }
        return Ok(path);
    }
    let mut dir = Some(cwd.to_owned());
    while let Some(at) = dir {
        let candidate = match at.as_str() {
            "/" => "/Cargo.toml".to_owned(),
            _ => format!("{at}/Cargo.toml"),
        };
        if files.kind(Path::new(&candidate)) == Some(FileKind::File) {
            return Ok(candidate);
        }
        let lower = match at.as_str() {
            "/" => "/cargo.toml".to_owned(),
            _ => format!("{at}/cargo.toml"),
        };
        if files.kind(Path::new(&lower)) == Some(FileKind::File) {
            return Err(LoadError {
                what: format!(
                    "could not find `Cargo.toml` in `{cwd}` or any parent directory, but found cargo.toml please try to rename it to Cargo.toml"
                ),
                at: None,
            });
        }
        dir = super::files::ancestors(&format!("{at}/x")).into_iter().nth(1);
        if at == "/" {
            break;
        }
    }
    Err(LoadError { what: format!("could not find `Cargo.toml` in `{cwd}` or any parent directory"), at: None })
}

/// The workspace `read` describes, read again in one step over `cx` through `config_of`'s
/// mounts and handed to `then` with its context. Its manifests are their [`ManifestQuery`]s,
/// parsed already; what `then` reads past them is read when it asks.
pub(crate) fn in_workspace<'db, R>(
    cx: &mut Cx<'db>,
    read: &WorkspaceRead,
    config_of: &ConfigOf,
    then: impl for<'r, 'a> FnOnce(&'a Context<Snap<'r, 'db>>, &'a Workspace<'a, Snap<'r, 'db>>) -> Result<R, Cancelled>,
) -> Result<Result<R, LoadError>, Cancelled> {
    let answer = read_through(cx, config_of, |files| {
        let config = match super::config::build(&read.config, files) {
            Ok(config) => config,
            Err(error) => return Ok(Err(load_error(&error, None))),
        };
        let ctx = Context::new(config);
        let ws = match Workspace::new(Path::new(&read.current), &ctx) {
            Ok(ws) => ws,
            // clone: the path an error names.
            Err(error) => return Ok(Err(load_error(&error, Some(read.current.clone())))),
        };
        then(&ctx, &ws).map(Ok)
    })?;
    Ok(answer.and_then(|it| it))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A manifest kept in CBOR reads back as the same manifest, so a step that takes it from
    /// its query sees what parsing the file would have given.
    #[test]
    fn a_parsed_manifest_is_kept_whole() {
        let text = r#"
[package]
name = "a"
version = "0.1.0"
edition = "2021"
authors = ["A <a@example.com>"]
build = false
links = "z"

[package.metadata.blaze]
tests = false
when = 1979-05-27T07:32:00Z

[lib]
path = "src/lib.rs"
crate-type = ["rlib", "cdylib"]

[[bin]]
name = "b"
required-features = ["x"]

[features]
default = ["x"]
x = ["dep:serde", "serde?/derive"]

[dependencies]
serde = { version = "1", optional = true, default-features = false }
local = { path = "../local" }
git = { git = "https://example.com/g", branch = "main" }

[target.'cfg(unix)'.dependencies]
libc = "0.2"

[dev-dependencies.inherited]
workspace = true

[profile.release]
lto = "thin"
opt-level = 3
debug = 1

[lints.rust]
unused = { level = "warn", priority = -1 }

[workspace]
members = ["m/*"]
"#;
        let parsed = blaze_manifest::manifest::parse_manifest(Path::new("/w/Cargo.toml"), text.as_bytes()).unwrap();
        let kept = blaze_protocol::cbor::to_vec(&Parsed::Here(parsed.clone())).unwrap();
        let Parsed::Here(read) = blaze_protocol::cbor::from_slice::<Parsed>(&kept).unwrap() else { panic!("not here") };
        assert_eq!(toml::to_string(&read).unwrap(), toml::to_string(&parsed).unwrap());
    }
}
