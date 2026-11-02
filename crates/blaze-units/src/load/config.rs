//! The config chain (cargo's `GlobalContext` of one invocation): `.cargo/config.toml` from the
//! cwd up, `$CARGO_HOME/config.toml`, `--config`, the `CARGO_*` variables. [`ConfigChain`]
//! checks it once; each later step builds it again ([`build`]) from the same inputs, its files
//! the database's [`super::files::TomlFile`]s.

use std::path::Path;

use blaze_format::SchemaHash;
use blaze_ids::ContentId;
use blaze_manifest::Config;
use blaze_query::{Cancelled, Cx, Query, QueryKind};
use serde::{Deserialize, Serialize};

use super::files::{FixedFile, FixedPath, Found, MountTable, Snap, ancestors, read_file, read_through};
use super::invocation::Asking;
use super::load_error;
use super::key_id;
use crate::query::{Cbor, LoadError, Mount, Versioned, producer};

/// Which configuration a workspace is read with.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) enum ConfigOf {
    /// The view's invocation: its cwd, the variables cargo reads, its `--config`s, its mounts.
    View,
    /// A configuration keyed by nothing of the view (the standard library's): it reads only
    /// under `mounts` (none of the view's own files), from `cwd` with the variables `env`.
    /// Outside them a config file of `cwd`, a directory above it or `$CARGO_HOME`, and a
    /// `Cargo.toml` above it, are absent; any other path is under no mount.
    Fixed { mounts: Vec<Mount>, cwd: String, env: Vec<(String, String)> },
}

impl ConfigOf {
    pub(crate) fn table(mounts: &[Mount], cwd: &str, env: &[(String, String)]) -> MountTable {
        let mut known = vec![cwd.to_owned()];
        known.extend(ancestors(cwd));
        if let Some((_, home)) = env.iter().find(|(name, _)| name == "CARGO_HOME") {
            known.push(home.clone());
        }
        known.sort();
        known.dedup();
        let mut mounts = mounts.to_vec();
        mounts.sort();
        MountTable { mounts, outside: Vec::new(), known }
    }

    pub fn read_file(&self, cx: &mut Cx<'_>, path: &str) -> Result<Found<Vec<u8>>, Cancelled> {
        match self {
            ConfigOf::View => read_file(cx, path),
            ConfigOf::Fixed { mounts, cwd, env } => {
                let key = FixedPath { table: ConfigOf::table(mounts, cwd, env), path: path.to_owned() };
                Ok(cx.get::<FixedFile>(&key)?.get())
            }
        }
    }
}

/// A configuration as read: what to build it again from, and its warnings.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub(crate) struct ConfigRead {
    pub cwd: String,
    pub env: Vec<(String, String)>,
    pub command: String,
    pub cli: Vec<String>,
    /// `-Z`, as given: cargo's `CliUnstable`, which decides what manifests may say.
    pub unstable: Vec<String>,
    pub warnings: Vec<String>,
}

impl Versioned for Result<ConfigRead, LoadError> {
    const SCHEMA: SchemaHash = blaze_format::schema("blaze v2 cargo config read 4");
}

pub(crate) struct ConfigChain;

impl Query for ConfigChain {
    const KIND: QueryKind = QueryKind(0x6015);
    const NAME: &'static str = "cargo config";
    const PRODUCER: ContentId = producer("blaze v2 cargo config 3");
    type Key = ConfigOf;
    type Value = Cbor<Result<ConfigRead, LoadError>>;

    fn input(key: &ConfigOf) -> ContentId {
        key_id("blaze v2 cargo config key 2", key)
    }

    fn execute(cx: &mut Cx<'_>, key: &ConfigOf) -> Result<Cbor<Result<ConfigRead, LoadError>>, Cancelled> {
        let (cwd, env, command, cli, unstable) = match key {
            ConfigOf::View => match cx.get::<Asking>(&())?.get() {
                Ok(asked) => (
                    asked.cwd.clone(),
                    asked.env.clone(),
                    asked.command_name().to_owned(),
                    asked.config.clone(),
                    asked.unstable.clone(),
                ),
                Err(error) => return Ok(Cbor(Err(error))),
            },
            ConfigOf::Fixed { cwd, env, .. } => (cwd.clone(), env.clone(), "build".to_owned(), Vec::new(), Vec::new()),
        };
        let read = ConfigRead { cwd, env, command, cli, unstable, warnings: Vec::new() };
        let loaded = read_through(cx, key, |files| Ok(build(&read, files).map(|config| config.take_warnings())))?;
        Ok(Cbor(match loaded {
            Ok(Ok(warnings)) => Ok(ConfigRead { warnings, ..read }),
            Ok(Err(error)) => Err(load_error(&error, None)),
            Err(unmounted) => Err(unmounted),
        }))
    }
}

/// The configuration `read` describes, reading its files through `files`: every file read at
/// once, as cargo's `GlobalContext` does.
pub(crate) fn build<'r, 'db>(read: &ConfigRead, files: Snap<'r, 'db>) -> anyhow::Result<Config<Snap<'r, 'db>>> {
    // clone: the variables, which the configuration takes owned.
    let mut config = Config::load_with(Path::new(&read.cwd), read.env.clone(), files)?;
    config.start(&read.command, &read.cli)?;
    config.cli_unstable(&read.unstable)?;
    config.ensure_loaded()?;
    Ok(config)
}
