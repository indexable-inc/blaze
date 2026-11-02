//! `[target]` as the pinned nightly's cargo reads it (`src/context/target.rs`, and the runner,
//! linker and rustflags it picks in `compilation.rs` and `target_info.rs`), without its `-Z`
//! flags: no `[host]` table (`-Zhost-config`), `target-applies-to-host` always true
//! (`-Ztarget-applies-to-host`). The `cfg` a target has is the
//! caller's, from rustc's `--print cfg` answers.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::bail;
use cargo_platform::{Cfg, CfgExpr, Platform};
use serde::Deserialize;

use super::{Config, ConfigRelativePath, OptValue, PathAndArgs, StringList};
use crate::files::Files;

/// A `[target.'cfg(..)']` table.
#[derive(Debug, Deserialize)]
pub struct TargetCfgConfig {
    pub runner: OptValue<PathAndArgs>,
    pub rustflags: OptValue<StringList>,
    pub rustdocflags: OptValue<StringList>,
    pub linker: OptValue<ConfigRelativePath>,
    /// Whatever else a `[target]` table has: every one is read, `cfg(` or not.
    #[serde(flatten)]
    pub other: BTreeMap<String, toml::Value>,
}

/// A `[target.<triple>]` table. Its `links` overrides are not read yet.
#[derive(Debug, Clone, Default)]
pub struct TargetConfig {
    pub runner: OptValue<PathAndArgs>,
    pub rustflags: OptValue<StringList>,
    pub rustdocflags: OptValue<StringList>,
    pub linker: OptValue<ConfigRelativePath>,
}

/// What a unit is compiled for: the host, or a `--target`, by its short name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompileKind {
    Host,
    Target(String),
}

impl CompileKind {
    fn short_name<'a>(&'a self, host: &'a str) -> &'a str {
        match self {
            CompileKind::Host => host,
            CompileKind::Target(t) => t,
        }
    }
}

/// `RUSTFLAGS` and `rustflags`, or `RUSTDOCFLAGS` and `rustdocflags`.
#[derive(Debug, Clone, Copy)]
pub enum Flags {
    Rust,
    Rustdoc,
}

impl Flags {
    fn as_key(self) -> &'static str {
        match self {
            Flags::Rust => "rustflags",
            Flags::Rustdoc => "rustdocflags",
        }
    }

    fn as_env(self) -> &'static str {
        match self {
            Flags::Rust => "RUSTFLAGS",
            Flags::Rustdoc => "RUSTDOCFLAGS",
        }
    }
}

impl<F: Files> Config<F> {
    /// The `[target.'cfg(..)']` tables by name, as cargo's `GlobalContext::target_cfgs`.
    pub fn target_cfgs(&self) -> anyhow::Result<&Vec<(String, TargetCfgConfig)>> {
        if let Some(cfgs) = self.target_cfgs.get() {
            return Ok(cfgs);
        }
        let mut result = Vec::new();
        let target: BTreeMap<String, TargetCfgConfig> = self.get("target")?;
        for (key, cfg) in target {
            if let Ok(platform) = key.parse::<Platform>() {
                let mut warnings = Vec::new();
                platform.check_cfg_keywords(&mut warnings, Path::new(".cargo/config.toml"));
                for w in warnings {
                    self.warn(w);
                }
            }
            if key.starts_with("cfg(") {
                for other_key in cfg.other.keys() {
                    self.warn(format!("unused key `{other_key}` in [target] config table `{key}`"));
                }
                result.push((key, cfg));
            }
        }
        Ok(self.target_cfgs.get_or_init(|| result))
    }

    /// `[target.<triple>]`, each field from the files or its variable.
    pub fn target_cfg_triple(&self, triple: &str) -> anyhow::Result<TargetConfig> {
        let prefix = format!("target.{triple}");
        Ok(TargetConfig {
            runner: self.get(&format!("{prefix}.runner"))?,
            rustflags: self.get(&format!("{prefix}.rustflags"))?,
            rustdocflags: self.get(&format!("{prefix}.rustdocflags"))?,
            linker: self.get(&format!("{prefix}.linker"))?,
        })
    }

    /// The program and arguments that run what is built for `kind`: `target.<triple>.runner`,
    /// else the one `target.'cfg(..)'.runner` that matches `cfg`.
    pub fn target_runner(
        &self,
        kind: &CompileKind,
        host: &str,
        cfg: &[Cfg],
    ) -> anyhow::Result<Option<(PathBuf, Vec<String>)>> {
        if let Some(runner) = self.target_cfg_triple(kind.short_name(host))?.runner {
            return Ok(Some((runner.val.path.resolve_program(self.cwd()), runner.val.args)));
        }
        let mut cfgs = self
            .target_cfgs()?
            .iter()
            .filter_map(|(key, c)| c.runner.as_ref().map(|runner| (key, runner)))
            .filter(|(key, _)| CfgExpr::matches_key(key, cfg));
        let matching = cfgs.next();
        if let Some((key, runner)) = cfgs.next() {
            let (first, first_runner) = matching.unwrap();
            bail!(
                "several matching instances of `target.'cfg(..)'.runner` in configurations\n\
                 first match `{first}` located in {}\n\
                 second match `{key}` located in {}",
                first_runner.definition,
                runner.definition
            );
        }
        Ok(matching.map(|(_, runner)| {
            (runner.val.path.resolve_program(self.cwd()), runner.val.args.clone())
        }))
    }

    /// The linker for `kind`: `target.<triple>.linker`, else the one
    /// `target.'cfg(..)'.linker` that matches `cfg`.
    pub fn target_linker(
        &self,
        kind: &CompileKind,
        host: &str,
        cfg: &[Cfg],
    ) -> anyhow::Result<Option<PathBuf>> {
        if let Some(linker) = self.target_cfg_triple(kind.short_name(host))?.linker {
            return Ok(Some(linker.val.resolve_program(self.cwd())));
        }
        let mut cfgs = self
            .target_cfgs()?
            .iter()
            .filter_map(|(key, c)| c.linker.as_ref().map(|linker| (key, linker)))
            .filter(|(key, _)| CfgExpr::matches_key(key, cfg));
        let matching = cfgs.next();
        if let Some((key, linker)) = cfgs.next() {
            let (first, first_linker) = matching.unwrap();
            bail!(
                "several matching instances of `target.'cfg(..)'.linker` in configurations\n\
                 first match `{first}` located in {}\n\
                 second match `{key}` located in {}",
                first_linker.definition,
                linker.definition
            );
        }
        Ok(matching.map(|(_, linker)| linker.val.resolve_program(self.cwd())))
    }

    /// The flags rustc or rustdoc gets for a unit of `kind` in a build that asked for
    /// `requested`, as cargo's `extra_args`: `CARGO_ENCODED_RUSTFLAGS`, else `RUSTFLAGS`, else
    /// `target.<triple>.rustflags` with every matching `target.'cfg(..)'.rustflags`, else
    /// `build.rustflags`. A host unit of a build with `--target` gets none, `[host]` being
    /// unstable.
    pub fn extra_args(
        &self,
        requested: &[CompileKind],
        host: &str,
        target_cfg: Option<&[Cfg]>,
        kind: &CompileKind,
        flags: Flags,
    ) -> anyhow::Result<Vec<String>> {
        if *kind == CompileKind::Host && requested != [CompileKind::Host] {
            return Ok(Vec::new());
        }
        if let Some(rustflags) = self.rustflags_from_env(flags) {
            return Ok(rustflags);
        }
        if let Some(rustflags) = self.rustflags_from_target(host, target_cfg, kind, flags)? {
            return Ok(rustflags);
        }
        let build = self.build_config()?;
        let list = match flags {
            Flags::Rust => &build.rustflags,
            Flags::Rustdoc => &build.rustdocflags,
        };
        Ok(list.as_ref().map(|l| l.as_slice().to_vec()).unwrap_or_default())
    }

    fn rustflags_from_env(&self, flags: Flags) -> Option<Vec<String>> {
        if let Some(a) = self.env(&format!("CARGO_ENCODED_{}", flags.as_env())) {
            if a.is_empty() {
                return Some(Vec::new());
            }
            return Some(a.split('\x1f').map(str::to_owned).collect());
        }
        let a = self.env(flags.as_env())?;
        Some(a.split(' ').map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).collect())
    }

    fn rustflags_from_target(
        &self,
        host: &str,
        target_cfg: Option<&[Cfg]>,
        kind: &CompileKind,
        flags: Flags,
    ) -> anyhow::Result<Option<Vec<String>>> {
        let mut rustflags = Vec::new();
        let key = format!("target.{}.{}", kind.short_name(host), flags.as_key());
        if let Some(args) = self.get::<Option<StringList>>(&key)? {
            rustflags.extend(args.as_slice().iter().cloned());
        }
        if let Some(target_cfg) = target_cfg {
            for (key, cfg) in self.target_cfgs()? {
                let list = match flags {
                    Flags::Rust => cfg.rustflags.as_ref(),
                    Flags::Rustdoc => cfg.rustdocflags.as_ref(),
                };
                if let Some(list) = list
                    && CfgExpr::matches_key(key, target_cfg)
                {
                    rustflags.extend(list.val.as_slice().iter().cloned());
                }
            }
        }
        Ok((!rustflags.is_empty()).then_some(rustflags))
    }
}
