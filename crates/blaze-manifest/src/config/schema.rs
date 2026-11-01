//! The typed tables of the configuration, as cargo's `src/context/schema.rs` and
//! `src/sources/config.rs`: read with `Config::get`, so each field comes from the files or
//! its environment variable as cargo's does.

use std::borrow::Cow;
use std::ffi::OsStr;
use std::fmt;
use std::path::Path;

use blaze_hash::HashMap;
use serde::Deserialize;
use serde_untagged::UntaggedEnumVisitor;

use super::{ConfigRelativePath, OptValue, PathAndArgs, StringList, Value};

#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub struct CargoHttpConfig {
    pub proxy: Option<String>,
    pub low_speed_limit: Option<u32>,
    pub timeout: Option<u64>,
    pub cainfo: Option<ConfigRelativePath>,
    pub proxy_cainfo: Option<ConfigRelativePath>,
    pub check_revoke: Option<bool>,
    pub user_agent: Option<String>,
    pub debug: Option<bool>,
    pub multiplexing: Option<bool>,
    pub ssl_version: Option<SslVersionConfig>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SslVersionConfig {
    Single(String),
    Range(SslVersionConfigRange),
}

impl<'de> Deserialize<'de> for SslVersionConfig {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        UntaggedEnumVisitor::new()
            .string(|single| Ok(SslVersionConfig::Single(single.to_owned())))
            .map(|map| map.deserialize().map(SslVersionConfig::Range))
            .deserialize(deserializer)
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub struct SslVersionConfigRange {
    pub min: Option<String>,
    pub max: Option<String>,
}

/// `[net]`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct CargoNetConfig {
    pub retry: Option<u32>,
    pub offline: Option<bool>,
    pub git_fetch_with_cli: Option<bool>,
    pub ssh: Option<CargoSshConfig>,
}

/// `[unstable]`: the `-Z` flags blaze reads, in the form cargo's `CliUnstable` takes them from
/// configuration. Other keys are left unread, as cargo leaves keys it has no field for.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct CargoUnstableConfig {
    pub unstable_options: Option<bool>,
    pub bindeps: Option<bool>,
    pub build_std: Option<Vec<String>>,
    pub build_std_features: Option<Vec<String>>,
    pub codegen_backend: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct CargoSshConfig {
    pub known_hosts: Option<Vec<Value<String>>>,
}

/// `build.jobs`: a number, or `"default"`.
#[derive(Debug, Clone)]
pub enum JobsConfig {
    Integer(i32),
    String(String),
}

impl<'de> Deserialize<'de> for JobsConfig {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        UntaggedEnumVisitor::new()
            .i32(|int| Ok(JobsConfig::Integer(int)))
            .string(|string| Ok(JobsConfig::String(string.to_owned())))
            .deserialize(deserializer)
    }
}

/// `[build]`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct CargoBuildConfig {
    pub pipelining: Option<bool>,
    pub dep_info_basedir: Option<ConfigRelativePath>,
    pub target_dir: Option<ConfigRelativePath>,
    pub build_dir: Option<ConfigRelativePath>,
    pub incremental: Option<bool>,
    pub target: Option<BuildTargetConfig>,
    pub jobs: Option<JobsConfig>,
    pub rustflags: Option<StringList>,
    pub rustdocflags: Option<StringList>,
    pub rustc_wrapper: Option<ConfigRelativePath>,
    pub rustc_workspace_wrapper: Option<ConfigRelativePath>,
    pub rustc: Option<ConfigRelativePath>,
    pub rustdoc: Option<ConfigRelativePath>,
    pub artifact_dir: Option<ConfigRelativePath>,
    pub warnings: Option<WarningHandling>,
    pub sbom: Option<bool>,
    pub analysis: Option<CargoBuildAnalysis>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub struct CargoBuildAnalysis {
    pub enabled: bool,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum WarningHandling {
    #[default]
    Warn,
    Allow,
    Deny,
}

/// `build.target`: one target or a list, each a triple or the path of a target spec.
#[derive(Debug, Deserialize)]
#[serde(transparent)]
pub struct BuildTargetConfig {
    inner: Value<BuildTargetConfigInner>,
}

#[derive(Debug)]
enum BuildTargetConfigInner {
    One(String),
    Many(Vec<String>),
}

impl<'de> Deserialize<'de> for BuildTargetConfigInner {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        UntaggedEnumVisitor::new()
            .string(|one| Ok(BuildTargetConfigInner::One(one.to_owned())))
            .seq(|many| many.deserialize().map(BuildTargetConfigInner::Many))
            .deserialize(deserializer)
    }
}

impl BuildTargetConfig {
    /// The targets, a `.json` spec resolved against where it was set.
    pub fn values(&self, cwd: &Path) -> Vec<String> {
        let map = |s: &String| {
            if s.ends_with(".json") {
                self.inner
                    .definition
                    .root(cwd)
                    .join(s)
                    .to_str()
                    .expect("must be utf-8 in toml")
                    .to_owned()
            } else {
                s.clone()
            }
        };
        match &self.inner.val {
            BuildTargetConfigInner::One(s) => vec![map(s)],
            BuildTargetConfigInner::Many(v) => v.iter().map(map).collect(),
        }
    }
}

/// `[resolver]`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct CargoResolverConfig {
    pub incompatible_rust_versions: Option<IncompatibleRustVersions>,
    pub incompatible_publish_age: Option<IncompatiblePublishAge>,
    pub feature_unification: Option<FeatureUnification>,
    pub lockfile_path: Option<ConfigRelativePath>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum IncompatibleRustVersions {
    Allow,
    Fallback,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum IncompatiblePublishAge {
    Allow,
    Deny,
}

#[derive(Copy, Clone, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FeatureUnification {
    Package,
    Selected,
    Workspace,
}

/// A variable of `[env]`: a string, or `{ value, force, relative }`.
#[derive(Debug, Deserialize)]
#[serde(transparent)]
pub struct EnvConfigValue {
    inner: EnvConfigValueInner,
}

#[derive(Debug)]
enum EnvConfigValueInner {
    Simple(String),
    WithOptions { value: ConfigRelativePath, force: bool, relative: bool },
}

impl<'de> Deserialize<'de> for EnvConfigValueInner {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct WithOptions {
            value: ConfigRelativePath,
            #[serde(default)]
            force: bool,
            #[serde(default)]
            relative: bool,
        }

        UntaggedEnumVisitor::new()
            .string(|simple| Ok(EnvConfigValueInner::Simple(simple.to_owned())))
            .map(|map| {
                let w: WithOptions = map.deserialize()?;
                Ok(EnvConfigValueInner::WithOptions {
                    value: w.value,
                    force: w.force,
                    relative: w.relative,
                })
            })
            .deserialize(deserializer)
    }
}

impl EnvConfigValue {
    /// Whether it replaces a variable the process already has.
    pub fn is_force(&self) -> bool {
        match self.inner {
            EnvConfigValueInner::Simple(_) => false,
            EnvConfigValueInner::WithOptions { force, .. } => force,
        }
    }

    pub fn resolve<'a>(&'a self, cwd: &Path) -> Cow<'a, OsStr> {
        match &self.inner {
            EnvConfigValueInner::Simple(s) => Cow::Borrowed(OsStr::new(s.as_str())),
            EnvConfigValueInner::WithOptions { value, relative, .. } => {
                if *relative {
                    let p = value.value().definition.root(cwd).join(value.raw_value());
                    Cow::Owned(p.into_os_string())
                } else {
                    Cow::Borrowed(OsStr::new(value.raw_value()))
                }
            }
        }
    }
}

pub type EnvConfig = HashMap<String, EnvConfigValue>;

/// A token or key, which `Debug` does not show.
#[derive(Clone, Deserialize)]
#[serde(transparent)]
pub struct Secret<T>(T);

impl<T> Secret<T> {
    pub fn expose(&self) -> &T {
        &self.0
    }
}

impl<T> fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret { inner: REDACTED }")
    }
}

/// `[registries.<name>]`.
#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "kebab-case")]
pub struct RegistryConfig {
    pub index: Option<String>,
    pub token: OptValue<Secret<String>>,
    pub credential_provider: Option<PathAndArgs>,
    pub min_publish_age: Option<String>,
    #[serde(rename = "protocol")]
    _protocol: Option<String>,
}

/// `[registry]`, the settings of crates.io and the defaults of every registry.
#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct GlobalRegistryConfig {
    pub index: Option<String>,
    pub token: OptValue<Secret<String>>,
    pub credential_provider: Option<PathAndArgs>,
    pub min_publish_age: Option<String>,
    pub global_min_publish_age: Option<String>,
    #[serde(rename = "default")]
    _default: Option<String>,
    #[serde(rename = "global-credential-providers")]
    _global_credential_providers: Option<Vec<String>>,
}

impl GlobalRegistryConfig {
    pub fn to_registry_config(self) -> RegistryConfig {
        RegistryConfig {
            index: self.index,
            token: self.token,
            credential_provider: self.credential_provider,
            min_publish_age: self.min_publish_age,
            _protocol: None,
        }
    }
}

/// `[source.<name>]`, a source and what replaces it.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct SourceConfigDef {
    pub replace_with: OptValue<String>,
    pub directory: Option<ConfigRelativePath>,
    pub registry: OptValue<String>,
    pub local_registry: Option<ConfigRelativePath>,
    pub git: OptValue<String>,
    pub branch: OptValue<String>,
    pub tag: OptValue<String>,
    pub rev: OptValue<String>,
}
