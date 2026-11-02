//! `[source]`, as cargo 1.99's `SourceConfigMap` reads it (`src/sources/config.rs`): each
//! source's one location and what replaces it. Following a replacement to the source that
//! serves it is `Config::replacement`; a source defined twice under two names is the
//! registry's to refuse, since that needs its `SourceId` display.

use std::collections::BTreeMap;

use anyhow::{Context as _, bail};

use super::{Config, SourceConfigDef, Value};
use crate::files::Files;
use crate::source::{self, CRATES_IO_REGISTRY, GitReference, SourceId, into_url};

/// A `[source.<name>]` table, validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceConfig {
    pub id: SourceId,
    /// The `replace-with` name and the definition it came from.
    pub replace_with: Option<(String, String)>,
}

/// Where `Config::replacement` lands for a source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Replacement {
    /// No `replace-with`: the source serves itself.
    None,
    /// The name of the `[source]` table that serves it.
    Source(String),
    /// A `[registries]` name that serves it.
    Registry(String),
}

impl<F: Files> Config<F> {
    /// crates.io as cargo reaches it: its sparse index unless
    /// `registries.crates-io.protocol = "git"`.
    pub fn crates_io_maybe_sparse_http(&self) -> anyhow::Result<SourceId> {
        let proto = self.get_string("registries.crates-io.protocol")?;
        let is_sparse = match proto.as_ref().map(|v| v.val.as_str()) {
            Some("sparse") | None => true,
            Some("git") => false,
            Some(unknown) => bail!(
                "unsupported registry protocol `{unknown}` (defined in {})",
                proto.as_ref().unwrap().definition
            ),
        };
        if is_sparse {
            self.check_registry_index_not_set()?;
            Ok(SourceId::crates_io_sparse())
        } else {
            source::crates_io(self)
        }
    }

    /// Every `[source]` table, with crates.io's built-in one, by name.
    pub fn source_configs(&self) -> anyhow::Result<BTreeMap<String, SourceConfig>> {
        let mut map = BTreeMap::new();
        map.insert(
            CRATES_IO_REGISTRY.to_owned(),
            SourceConfig { id: source::crates_io(self)?, replace_with: None },
        );
        let sources: Option<BTreeMap<String, SourceConfigDef>> = self.get("source")?;
        for (name, def) in sources.into_iter().flatten() {
            let config = self.source_config(&name, def)?;
            map.insert(name, config);
        }
        Ok(map)
    }

    fn source_config(&self, name: &str, def: SourceConfigDef) -> anyhow::Result<SourceConfig> {
        let url = |val: &Value<String>, key: &str| {
            into_url(&val.val).with_context(|| {
                format!(
                    "configuration key `{key}` specified an invalid URL (in {})",
                    val.definition
                )
            })
        };
        let mut srcs = Vec::new();
        if let Some(registry) = &def.registry {
            let url = url(registry, &format!("source.{name}.registry"))?;
            srcs.push(SourceId::for_source_replacement_registry(&url, name)?);
        }
        if let Some(local_registry) = &def.local_registry {
            srcs.push(SourceId::for_local_registry(&local_registry.resolve_path(self.cwd()))?);
        }
        if let Some(directory) = &def.directory {
            srcs.push(SourceId::for_directory(&directory.resolve_path(self.cwd()))?);
        }
        if let Some(git) = &def.git {
            let url = url(git, &format!("source.{name}.git"))?;
            let reference = match (def.branch, def.tag, def.rev) {
                (Some(b), _, _) => GitReference::Branch(b.val),
                (None, Some(t), _) => GitReference::Tag(t.val),
                (None, None, Some(r)) => GitReference::Rev(r.val),
                (None, None, None) => GitReference::DefaultBranch,
            };
            srcs.push(SourceId::for_git(&url, reference)?);
        } else {
            for (key, v) in [("branch", &def.branch), ("tag", &def.tag), ("rev", &def.rev)] {
                if let Some(val) = v {
                    bail!(
                        "source definition `source.{name}` specifies `{key}`, \
                         but that requires a `git` key to be specified (in {})",
                        val.definition
                    );
                }
            }
        }
        if name == CRATES_IO_REGISTRY && srcs.is_empty() {
            srcs.push(self.crates_io_maybe_sparse_http()?);
        }
        let id = match srcs.len() {
            0 => bail!(
                "no source location specified for `source.{name}`, need \
                 `registry`, `local-registry`, `directory`, or `git` defined"
            ),
            1 => srcs.pop().unwrap(),
            _ => bail!("more than one source location specified for `source.{name}`"),
        };
        let replace_with = def.replace_with.map(|val| (val.val, val.definition.to_string()));
        Ok(SourceConfig { id, replace_with })
    }

    /// What serves the source named `name`, following `replace-with` as cargo's
    /// `SourceConfigMap::load` does, with its errors for a missing name and a cycle.
    pub fn replacement(&self, name: &str) -> anyhow::Result<Replacement> {
        let cfgs = self.source_configs()?;
        let orig_name = name;
        let mut name = name;
        let mut cfg_loc = "";
        let mut replaced = false;
        loop {
            let Some(cfg) = cfgs.get(name) else {
                if source::alt_registry(self, name).is_ok() {
                    return Ok(Replacement::Registry(name.to_owned()));
                }
                bail!(
                    "could not find a configured source with the name `{name}` when attempting to \
                     lookup `{orig_name}` (configuration in `{cfg_loc}`)"
                );
            };
            match &cfg.replace_with {
                Some((s, c)) => {
                    name = s;
                    cfg_loc = c;
                    replaced = true;
                }
                None if !replaced => return Ok(Replacement::None),
                None => return Ok(Replacement::Source(name.to_owned())),
            }
            if name == orig_name {
                bail!(
                    "detected a cycle of `replace-with` sources, the source `{name}` is eventually \
                     replaced with itself (configuration in `{cfg_loc}`)"
                );
            }
        }
    }
}