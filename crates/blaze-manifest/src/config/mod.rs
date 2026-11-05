//! `.cargo/config.toml` as cargo 1.99 reads it: the files from the current directory up and in
//! `$CARGO_HOME`, their `include`s, `--config` arguments and `CARGO_*` variables, merged by
//! cargo's rules (`src/context/mod.rs` and `config_value.rs` there).

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};

use annotate_snippets::{Level, Renderer};
use anyhow::{Context as _, anyhow, bail};
use blaze_hash::{HashMap, HashSet};
use blaze_ids::radix;
use std::alloc::Global;
use url::Url;

use crate::files::{Disk, Files};
use crate::paths;

mod de;
pub mod error;
pub mod get;
pub mod key;
pub mod path;
pub mod print;
pub mod schema;
mod source;
pub mod target;
mod value;

pub use error::ConfigError;
pub use key::ConfigKey;
pub use path::{ConfigRelativePath, PathAndArgs, StringList};
pub use schema::*;
pub use source::{Replacement, SourceConfig};
pub use target::{CompileKind, Flags, TargetCfgConfig, TargetConfig};
pub use value::{Definition, OptValue, Value};

macro_rules! get_value_typed {
    ($name:ident, $ty:ty, $variant:ident, $expected:expr) => {
        pub(crate) fn $name(&self, key: &ConfigKey) -> Result<OptValue<$ty>, ConfigError> {
            let cv = self.get_cv(key)?;
            let env = self.get_config_env::<$ty>(key)?;
            match (cv, env) {
                (Some(CV::$variant(val, definition)), Some(env)) => {
                    if definition.is_higher_priority(&env.definition) {
                        Ok(Some(Value { val, definition }))
                    } else {
                        Ok(Some(env))
                    }
                }
                (Some(CV::$variant(val, definition)), None) => Ok(Some(Value { val, definition })),
                (Some(cv), _) => Err(ConfigError::expected(key, $expected, &cv)),
                (None, Some(env)) => Ok(Some(env)),
                (None, None) => Ok(None),
            }
        }
    };
}

/// A config value as cargo keeps it: TOML's types less floats and dates, each with its
/// definition.
#[derive(Clone, Debug)]
pub enum ConfigValue {
    Integer(i64, Definition),
    String(String, Definition),
    List(Vec<ConfigValue>, Definition),
    Table(HashMap<String, ConfigValue, Global>, Definition),
    Boolean(bool, Definition),
}

use ConfigValue as CV;

/// Lists that replace rather than append when merged.
const NON_MERGEABLE_LISTS: &[&str] = &[
    "credential-alias.*",
    "doc.browser",
    "host.runner",
    "registries.*.credential-provider",
    "registry.credential-provider",
    "target.*.runner",
];

fn key_matches(parts: &[String], pattern: &str) -> bool {
    let mut parts = parts.iter();
    pattern.split('.').all(|pat| parts.next().is_some_and(|p| p == pat) || pat == "*")
        && parts.next().is_none()
}

fn is_nonmergeable_list(parts: &[String]) -> bool {
    NON_MERGEABLE_LISTS.iter().any(|l| key_matches(parts, l))
}

impl ConfigValue {
    fn from_toml(def: Definition, toml: toml::Value) -> anyhow::Result<ConfigValue> {
        let mut path = Vec::new();
        Self::from_toml_inner(def, toml, &mut path).with_context(|| {
            let mut key_path = String::new();
            let mut it = path.iter().rev().peekable();
            while let Some(k) = it.next() {
                match k {
                    KeyOrIdx::Key(s) => key_path.push_str(&key::escape_key_part(s)),
                    KeyOrIdx::Idx(i) => key_path.push_str(&format!("[{i}]")),
                }
                if matches!(it.peek(), Some(KeyOrIdx::Key(_))) {
                    key_path.push('.');
                }
            }
            format!("failed to parse config at `{key_path}`")
        })
    }

    fn from_toml_inner(
        def: Definition,
        toml: toml::Value,
        path: &mut Vec<KeyOrIdx>,
    ) -> anyhow::Result<ConfigValue> {
        match toml {
            toml::Value::String(val) => Ok(CV::String(val, def)),
            toml::Value::Boolean(b) => Ok(CV::Boolean(b, def)),
            toml::Value::Integer(i) => Ok(CV::Integer(i, def)),
            toml::Value::Array(val) => Ok(CV::List(
                val.into_iter()
                    .enumerate()
                    .map(|(i, toml)| {
                        CV::from_toml_inner(def.clone(), toml, path)
                            .inspect_err(|_| path.push(KeyOrIdx::Idx(i)))
                    })
                    .collect::<anyhow::Result<_>>()?,
                def,
            )),
            toml::Value::Table(val) => Ok(CV::Table(
                val.into_iter()
                    .map(|(key, value)| match CV::from_toml_inner(def.clone(), value, path) {
                        Ok(value) => Ok((key, value)),
                        Err(e) => {
                            path.push(KeyOrIdx::Key(key));
                            Err(e)
                        }
                    })
                    .collect::<anyhow::Result<_>>()?,
                def,
            )),
            v => bail!("unsupported TOML configuration type `{}`", v.type_str()),
        }
    }

    pub fn definition(&self) -> &Definition {
        match self {
            CV::Boolean(_, def)
            | CV::Integer(_, def)
            | CV::String(_, def)
            | CV::List(_, def)
            | CV::Table(_, def) => def,
        }
    }

    pub fn desc(&self) -> &'static str {
        match self {
            CV::Table(..) => "table",
            CV::List(..) => "array",
            CV::String(..) => "string",
            CV::Boolean(..) => "boolean",
            CV::Integer(..) => "integer",
        }
    }

    /// Merges `from` into `self`. With `force`, a value of the same priority replaces the old
    /// one and a list of the same priority goes after it.
    fn merge(&mut self, from: ConfigValue, force: bool) -> anyhow::Result<()> {
        self.merge_helper(from, force, &mut Vec::new())
    }

    fn merge_helper(
        &mut self,
        from: ConfigValue,
        force: bool,
        parts: &mut Vec<String>,
    ) -> anyhow::Result<()> {
        let is_higher_priority = from.definition().is_higher_priority(self.definition());
        match (self, from) {
            (CV::List(old, _), CV::List(mut new, _)) => {
                if is_nonmergeable_list(parts) {
                    if force || is_higher_priority {
                        std::mem::swap(&mut new, old);
                    }
                } else if force {
                    old.append(&mut new);
                } else {
                    new.append(old);
                    std::mem::swap(&mut new, old);
                }
                radix::sort_by_key(old, |v| v.definition().rank());
            }
            (CV::Table(old, _), CV::Table(new, _)) => {
                for (key, value) in new {
                    match old.get_mut(&key) {
                        Some(entry) => {
                            let new_def = value.definition().clone();
                            // The path of the value merged, popped once it is: cargo 1.99 pushes
                            // and never pops, so its path holds every key merged before it, in
                            // its `HashMap`'s order, and whether `target.<triple>.runner` or
                            // `registry.credential-provider` replaces (as cargo documents) or
                            // appends turns on that order. Here it always replaces.
                            parts.push(key.clone());
                            let result = entry.merge_helper(value, force, parts);
                            parts.pop();
                            result.with_context(|| {
                                format!(
                                    "failed to merge key `{key}` between {} and {new_def}",
                                    entry.definition(),
                                )
                            })?;
                        }
                        None => {
                            old.insert(key, value);
                        }
                    }
                }
            }
            (expected @ CV::List(..), found)
            | (expected @ CV::Table(..), found)
            | (expected, found @ CV::List(..))
            | (expected, found @ CV::Table(..)) => {
                return Err(anyhow!(
                    "failed to merge config value from `{}` into `{}`: expected {}, but found {}",
                    found.definition(),
                    expected.definition(),
                    expected.desc(),
                    found.desc()
                ));
            }
            (old, mut new) => {
                if force || new.definition().is_higher_priority(old.definition()) {
                    std::mem::swap(old, &mut new);
                }
            }
        }
        Ok(())
    }
}

enum KeyOrIdx {
    Key(String),
    Idx(usize),
}

/// The environment variable cargo reads for a key: `CARGO_` and the parts upper-cased, with
/// `-` and `.` as `_`.
pub fn env_key(key: &str) -> String {
    ConfigKey::parse(key).as_env_key().to_owned()
}

/// The configuration of one invocation: what cargo's `GlobalContext` holds of it, with the files
/// it reads them through.
#[derive(Debug)]
pub struct Config<F: Files> {
    files: F,
    cwd: PathBuf,
    home: PathBuf,
    env: BTreeMap<String, String>,
    cli: Vec<String>,
    /// The files are read at the first value asked for, and again at the next while they fail:
    /// a broken config file is an error only to what reads configuration, as in cargo.
    #[expect(
        clippy::disallowed_types,
        reason = "the configuration's parts, each read the first time it is asked for: per config, not global"
    )]
    values: std::cell::OnceCell<HashMap<String, ConfigValue, Global>>,
    /// `$CARGO_HOME/credentials.toml`, once [`Config::load_credentials`] read it: kept apart
    /// from the files' values, as cargo keeps it, for [`Config::credentials_token`] only.
    #[expect(
        clippy::disallowed_types,
        reason = "the configuration's parts, each read the first time it is asked for: per config, not global"
    )]
    credentials: std::cell::OnceCell<HashMap<String, ConfigValue, Global>>,
    #[expect(
        clippy::disallowed_types,
        reason = "the configuration's parts, each read the first time it is asked for: per config, not global"
    )]
    build_config: std::cell::OnceCell<CargoBuildConfig>,
    #[expect(
        clippy::disallowed_types,
        reason = "the configuration's parts, each read the first time it is asked for: per config, not global"
    )]
    target_cfgs: std::cell::OnceCell<Vec<(String, target::TargetCfgConfig)>>,
    unstable: CliUnstable,
    warnings: RefCell<Vec<String>>,
}

/// The `-Z` flags blaze reads, from `[unstable]` and then the command line, as cargo's
/// `CliUnstable` on a nightly.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CliUnstable {
    pub unstable_options: bool,
    pub bindeps: bool,
    pub build_std: Option<Vec<String>>,
    pub build_std_features: Option<Vec<String>>,
    /// `-Zcodegen-backend`: a profile may set `codegen-backend` without `cargo-features`.
    pub codegen_backend: bool,
}

impl CliUnstable {
    fn from_table(table: CargoUnstableConfig) -> CliUnstable {
        let list = |it: Vec<String>| {
            it.iter()
                .flat_map(|it| it.split(','))
                .filter(|it| !it.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        CliUnstable {
            unstable_options: table.unstable_options.unwrap_or(false),
            bindeps: table.bindeps.unwrap_or(false),
            build_std: table.build_std.map(list),
            build_std_features: table.build_std_features.map(list),
            codegen_backend: table.codegen_backend.unwrap_or(false),
        }
    }

    /// One `-Z` flag as given after `-Z`: `name` or `name=value`, `_` read as `-`.
    fn add(&mut self, flag: &str) -> anyhow::Result<()> {
        let (name, value) = match flag.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (flag, None),
        };
        let name = name.replace('_', "-");
        let bool_flag = || match value {
            None | Some("yes") => Ok(true),
            Some("no") => Ok(false),
            Some(other) => Err(anyhow!("flag -Z{name} expected `no` or `yes`, found: `{other}`")),
        };
        let list = || {
            value
                .unwrap_or("")
                .split(',')
                .filter(|it| !it.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        match name.as_str() {
            "unstable-options" => self.unstable_options = bool_flag()?,
            "bindeps" => self.bindeps = bool_flag()?,
            "build-std" => self.build_std = Some(list()),
            "build-std-features" => self.build_std_features = Some(list()),
            "codegen-backend" => self.codegen_backend = bool_flag()?,
            // blaze's own, and a no-op: every build runs its proc macros as wasm modules now
            // (`blaze_units::load`); kept so that configurations that set it still load.
            "wasm-macros" => {}
            _ => bail!("blaze does not read `-Z {name}` yet"),
        }
        Ok(())
    }
}

impl Config<Disk> {
    /// The configuration seen from `cwd` on the machine's disk with the variables of `env` and
    /// the `--config` arguments `cli`, as `cargo` started there would. `--config` reads the
    /// files at once.
    pub fn load(
        cwd: &Path,
        env: impl IntoIterator<Item = (String, String)>,
        cli: &[String],
    ) -> anyhow::Result<Config<Disk>> {
        let mut config = Config::load_with(cwd, env, Disk)?;
        config.merge_cli_args(cli)?;
        Ok(config)
    }

    /// The configuration this process sees: its current directory and environment.
    pub fn from_process() -> anyhow::Result<Config<Disk>> {
        let cwd =
            std::env::current_dir().context("couldn't get the current directory of the process")?;
        Config::load(&cwd, std::env::vars(), &[])
    }
}

impl<F: Files> Config<F> {
    /// The configuration seen from `cwd` with the variables of `env`, every file of it
    /// (`.cargo/config.toml` up from `cwd`, `$CARGO_HOME/config.toml`, `include`s, a `--config`
    /// file) read through `files` when a value is first asked for.
    pub fn load_with(
        cwd: &Path,
        env: impl IntoIterator<Item = (String, String)>,
        files: F,
    ) -> anyhow::Result<Config<F>> {
        let env: BTreeMap<String, String> = env.into_iter().collect();
        let home = cargo_home(cwd, &env)?;
        Ok(Config {
            files,
            cwd: cwd.to_path_buf(),
            home,
            env,
            cli: Vec::new(),
            values: Default::default(),
            credentials: Default::default(),
            build_config: Default::default(),
            target_cfgs: Default::default(),
            unstable: CliUnstable::default(),
            warnings: RefCell::new(Vec::new()),
        })
    }

    /// The `-Z` flags `flags` (each as given after `-Z`) over the `[unstable]` table, as cargo's
    /// `GlobalContext::configure` reads them on a nightly: the table first, each flag after it.
    pub fn cli_unstable(&mut self, flags: &[String]) -> anyhow::Result<()> {
        let mut unstable = match self.get::<Option<CargoUnstableConfig>>("unstable")? {
            Some(table) => CliUnstable::from_table(table),
            None => CliUnstable::default(),
        };
        for flag in flags {
            unstable.add(flag)?;
        }
        self.unstable = unstable;
        Ok(())
    }

    pub fn unstable(&self) -> &CliUnstable {
        &self.unstable
    }

    /// `-Z bindeps`.
    pub fn bindeps(&self) -> bool {
        self.unstable.bindeps
    }

    /// What every file of this configuration is read through.
    pub fn files(&self) -> &F {
        &self.files
    }

    /// What `cargo <command>` does with its configuration on start, with `--config` arguments
    /// `cli`: it looks up `alias.<command>`, which reads the files (an error there is no error
    /// yet) and warns of an alias the built-in command shadows, then merges `cli`.
    pub fn start(&mut self, command: &str, cli: &[String]) -> anyhow::Result<()> {
        if matches!(self.aliased_command(command), Ok(Some(alias)) if !alias.is_empty()) {
            self.warn(format!(
                "user-defined alias `{command}` is ignored, because it is shadowed by a built-in command"
            ));
        }
        self.merge_cli_args(cli)
    }

    fn aliased_command(&self, command: &str) -> anyhow::Result<Option<Vec<String>>> {
        let alias_name = format!("alias.{command}");
        match self.get_string(&alias_name) {
            Ok(Some(record)) => {
                Ok(Some(record.val.split_whitespace().map(str::to_owned).collect()))
            }
            Ok(None) => Ok(None),
            Err(_) => self.get::<Option<Vec<String>>>(&alias_name),
        }
    }

    fn values(&self) -> anyhow::Result<&HashMap<String, ConfigValue, Global>> {
        if let Some(values) = self.values.get() {
            return Ok(values);
        }
        let values = self.load_values()?;
        Ok(self.values.get_or_init(|| values))
    }

    /// The same configuration read again, for a thread of its own to read: what reading the
    /// files warns of is this one's to say, not the copy's.
    pub fn reload(&self) -> anyhow::Result<Config<F>>
    where
        F: Clone,
    {
        let mut copy = Config::load_with(&self.cwd, self.env.clone(), self.files.clone())?;
        copy.merge_cli_args(&self.cli)?;
        copy.unstable = self.unstable.clone();
        _ = copy.values();
        copy.warnings.take();
        Ok(copy)
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// `$CARGO_HOME`, or `~/.cargo`.
    pub fn home(&self) -> &Path {
        &self.home
    }

    pub fn env(&self, key: &str) -> Option<&str> {
        self.env.get(key).map(String::as_str)
    }

    fn load_values(&self) -> anyhow::Result<HashMap<String, ConfigValue, Global>> {
        let mut cfg = CV::Table(HashMap::new_in(Global), Definition::BuiltIn);
        self.walk_tree(&self.cwd, &self.home, |path| {
            let value = self.load_file(path, &mut HashSet::new_in(Global), true, false)?;
            cfg.merge(value, false)
                .with_context(|| format!("failed to merge configuration at `{}`", path.display()))
        })
        .context("could not load Cargo configuration")?;
        match cfg {
            CV::Table(map, _) => Ok(map),
            _ => unreachable!(),
        }
    }

    /// The config files cargo reads, nearest first: `.cargo/config` or `.cargo/config.toml` in
    /// `pwd` and each directory above it, then the one in `home` unless that was one of them.
    fn walk_tree(
        &self,
        pwd: &Path,
        home: &Path,
        mut walk: impl FnMut(&Path) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let mut seen_dir = HashSet::new_in(Global);
        let stop = self.env("__CARGO_TEST_ROOT").map(PathBuf::from);
        for current in paths::ancestors(pwd, stop.as_deref()) {
            let config_root = current.join(".cargo");
            if let Some(path) = self.get_file_path(&config_root, "config") {
                walk(&path)?;
            }
            let canonical_root = self.files.canonicalize(&config_root).unwrap_or(config_root);
            seen_dir.insert(canonical_root);
        }
        let canonical_home = self.files.canonicalize(home).unwrap_or(home.to_path_buf());
        if !seen_dir.contains(&canonical_home)
            && !seen_dir.contains(home)
            && let Some(path) = self.get_file_path(home, "config")
        {
            walk(&path)?;
        }
        Ok(())
    }

    fn load_file(
        &self,
        path: &Path,
        seen: &mut HashSet<PathBuf, Global>,
        includes: bool,
        cli: bool,
    ) -> anyhow::Result<ConfigValue> {
        if !seen.insert(path.to_path_buf()) {
            bail!("config `include` cycle detected with path `{}`", path.display());
        }
        // alloc: an error's context, made only when the read fails.
        let read = || format!("failed to read configuration file `{}`", path.display());
        let bytes = self.files.read(path).with_context(read)?;
        let contents = std::str::from_utf8(&bytes)
            .map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "stream did not contain valid UTF-8",
                )
            })
            .with_context(read)?;
        let value = parse_file(path, contents, cli)?;
        if includes { self.load_includes(value, seen, cli) } else { Ok(value) }
    }

    fn load_includes(
        &self,
        mut value: ConfigValue,
        seen: &mut HashSet<PathBuf, Global>,
        cli: bool,
    ) -> anyhow::Result<ConfigValue> {
        let includes = include_paths(&mut value, true)?;
        let mut root = CV::Table(HashMap::new_in(Global), value.definition().clone());
        for include in includes {
            let Some(abs_path) = self.include_path(&include) else { continue };
            self.load_file(&abs_path, seen, true, cli)
                .and_then(|include| root.merge(include, true))
                .with_context(|| {
                    format!(
                        "failed to load config include `{}` from `{}`",
                        include.path.display(),
                        include.def
                    )
                })?;
        }
        root.merge(value, true)?;
        Ok(root)
    }

    /// Where an include points, or `None` for an optional one that is not there.
    fn include_path(&self, include: &Include) -> Option<PathBuf> {
        let base = match &include.def {
            Definition::Path(p) | Definition::Cli(Some(p)) => p.parent().unwrap(),
            _ => &self.cwd,
        };
        let abs_path = paths::normalize_path(&base.join(&include.path));
        (!include.optional || self.files.exists(&abs_path)).then_some(abs_path)
    }

    /// Each config file on its own, includes before what includes them, as cargo's
    /// `load_values_unmerged`.
    pub fn load_values_unmerged(&self) -> anyhow::Result<Vec<ConfigValue>> {
        let mut result = Vec::new();
        let mut seen = HashSet::new_in(Global);
        self.walk_tree(&self.cwd, &self.home, |path| {
            let mut cv = self.load_file(path, &mut seen, false, false)?;
            self.load_unmerged_include(&mut cv, &mut seen, &mut result)?;
            result.push(cv);
            Ok(())
        })
        .context("could not load Cargo configuration")?;
        Ok(result)
    }

    fn load_unmerged_include(
        &self,
        cv: &mut ConfigValue,
        seen: &mut HashSet<PathBuf, Global>,
        output: &mut Vec<ConfigValue>,
    ) -> anyhow::Result<()> {
        for include in include_paths(cv, false)? {
            let Some(abs_path) = self.include_path(&include) else { continue };
            let mut cv = self.load_file(&abs_path, seen, false, false).with_context(|| {
                format!(
                    "failed to load config include `{}` from `{}`",
                    include.path.display(),
                    include.def
                )
            })?;
            self.load_unmerged_include(&mut cv, seen, output)?;
            output.push(cv);
        }
        Ok(())
    }

    fn merge_cli_args(&mut self, cli: &[String]) -> anyhow::Result<()> {
        if cli.is_empty() {
            return Ok(());
        }
        self.cli = cli.to_vec();
        let loaded = self.cli_args_as_table()?;
        self.values()?;
        let values = self.values.take().unwrap_or_default();
        let mut root = CV::Table(values, Definition::BuiltIn);
        root.merge(loaded, true)?;
        match root {
            CV::Table(map, _) => self.values = map.into(),
            _ => unreachable!(),
        }
        Ok(())
    }

    /// The `--config` arguments merged into one table.
    pub fn cli_args_as_table(&self) -> anyhow::Result<ConfigValue> {
        let mut loaded = CV::Table(HashMap::new_in(Global), Definition::Cli(None));
        for arg in &self.cli {
            let arg_as_path = self.cwd.join(arg);
            let table = if !arg.is_empty() && self.files.exists(&arg_as_path) {
                self.load_file(&arg_as_path, &mut HashSet::new_in(Global), true, true)
                    .with_context(|| {
                        format!("failed to load config from `{}`", arg_as_path.display())
                    })?
            } else {
                let doc = cli_dotted_keys(arg)?;
                if doc.get("registry").and_then(|v| v.get("token")).is_some() {
                    bail!("registry.token cannot be set through --config for security reasons");
                }
                if let Some((k, _)) = doc
                    .get("registries")
                    .and_then(|v| v.as_table())
                    .and_then(|t| t.iter().find(|(_, v)| v.get("token").is_some()))
                {
                    bail!(
                        "registries.{k}.token cannot be set through --config for security reasons"
                    );
                }
                let value = CV::from_toml(Definition::Cli(None), toml::Value::Table(doc))
                    .with_context(|| format!("failed to convert --config argument `{arg}`"))?;
                self.load_includes(value, &mut HashSet::new_in(Global), true)
                    .context("failed to load --config include")?
            };
            loaded
                .merge(table, true)
                .with_context(|| format!("failed to merge --config argument `{arg}`"))?;
        }
        Ok(loaded)
    }

    /// `T` from the value at the dotted `key`, each part of it from the files or its
    /// environment variable as cargo's `GlobalContext::get` takes it. `""` is the root.
    pub fn get<'de, T: serde::Deserialize<'de>>(&self, key: &str) -> anyhow::Result<T> {
        let key = if key.is_empty() { ConfigKey::new() } else { ConfigKey::parse(key) };
        let d = de::Deserializer { config: self, key, env_prefix_ok: true };
        T::deserialize(d).map_err(|e| e.into())
    }

    pub fn get_string(&self, key: &str) -> anyhow::Result<OptValue<String>> {
        self.get::<OptValue<String>>(key)
    }

    /// Reads the files now if they are not read yet, as cargo does on start to see its
    /// `[term]` table.
    pub fn ensure_loaded(&self) -> anyhow::Result<()> {
        self.values().map(|_| ())
    }

    /// The warnings so far, in order: what cargo prints after `warning: ` when it reads the
    /// files and the tables it does not know all the keys of.
    pub fn take_warnings(&self) -> Vec<String> {
        std::mem::take(&mut self.warnings.borrow_mut())
    }

    pub(crate) fn warn(&self, warning: String) {
        self.warnings.borrow_mut().push(warning);
    }

    /// The merged value at `key` from files and `--config`, without the environment.
    pub fn get_cv(&self, key: &ConfigKey) -> anyhow::Result<Option<ConfigValue>> {
        Self::get_cv_in(key, self.values()?)
    }

    fn get_cv_in(
        key: &ConfigKey,
        vals: &HashMap<String, ConfigValue, Global>,
    ) -> anyhow::Result<Option<ConfigValue>> {
        if key.is_root() {
            return Ok(Some(CV::Table(vals.clone(), Definition::Path(PathBuf::new()))));
        }
        let mut parts = key.parts().enumerate();
        let Some(mut val) = vals.get(parts.next().unwrap().1) else { return Ok(None) };
        for (i, part) in parts {
            match val {
                CV::Table(map, _) => match map.get(part) {
                    Some(v) => val = v,
                    None => return Ok(None),
                },
                other => {
                    let mut key_so_far = ConfigKey::new();
                    for part in key.parts().take(i) {
                        key_so_far.push(part);
                    }
                    bail!(
                        "expected table for configuration key `{key_so_far}`, but found {} in {}",
                        other.desc(),
                        other.definition()
                    )
                }
            }
        }
        Ok(Some(val.clone()))
    }

    /// The value at `key` with its environment variable taken into account, as cargo's
    /// `get_cv_with_env`: the variable wins over files, a list gets the variable's words
    /// appended.
    pub fn get_cv_with_env(&self, key: &ConfigKey) -> anyhow::Result<Option<ConfigValue>> {
        let cv = self.get_cv(key)?;
        if key.is_root() {
            return Ok(cv);
        }
        let env = self.env(key.as_env_key());
        let env_def = Definition::Environment(key.as_env_key().to_owned());
        let use_env = match (&cv, env) {
            (Some(CV::List(..)), Some(_)) => true,
            (Some(cv), Some(_)) => env_def.is_higher_priority(cv.definition()),
            (None, Some(_)) => true,
            _ => false,
        };
        if !use_env {
            return Ok(cv);
        }
        let env = env.unwrap();
        if env == "true" {
            Ok(Some(CV::Boolean(true, env_def)))
        } else if env == "false" {
            Ok(Some(CV::Boolean(false, env_def)))
        } else if let Ok(i) = env.parse::<i64>() {
            Ok(Some(CV::Integer(i, env_def)))
        } else {
            match cv {
                Some(CV::List(mut list, def)) => {
                    self.get_env_list(key, &mut list);
                    Ok(Some(CV::List(list, def)))
                }
                _ => Ok(Some(CV::String(env.to_owned(), env_def))),
            }
        }
    }

    /// The variable of a list key, its words added to `output`; for a list that does not merge,
    /// in place of what is there unless that came from `--config`.
    pub(crate) fn get_env_list(&self, key: &ConfigKey, output: &mut Vec<ConfigValue>) {
        let Some(env_val) = self.env(key.as_env_key()) else { return };
        let env_def = Definition::Environment(key.as_env_key().to_owned());
        if NON_MERGEABLE_LISTS.iter().any(|l| key.matches(l)) {
            if output.first().is_some_and(|o| o.definition() > &env_def) {
                return;
            }
            output.clear();
        }
        output
            .extend(env_val.split_whitespace().map(|s| CV::String(s.to_owned(), env_def.clone())));
        output.sort_by(|a, b| a.definition().cmp(b.definition()));
    }

    pub(crate) fn env_keys(&self) -> impl Iterator<Item = &str> {
        self.env.keys().map(String::as_str)
    }

    /// The environment, sorted by name.
    pub fn env_vars(&self) -> impl Iterator<Item = (&str, &str)> {
        self.env.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    pub(crate) fn has_key(&self, key: &ConfigKey, env_prefix_ok: bool) -> anyhow::Result<bool> {
        if self.env.contains_key(key.as_env_key()) {
            return Ok(true);
        }
        if env_prefix_ok {
            let env_prefix = format!("{}_", key.as_env_key());
            if self.env_keys().any(|k| k.starts_with(&env_prefix)) {
                return Ok(true);
            }
        }
        Ok(self.get_cv(key)?.is_some())
    }

    pub(crate) fn get_table(
        &self,
        key: &ConfigKey,
    ) -> anyhow::Result<OptValue<HashMap<String, ConfigValue, Global>>> {
        match self.get_cv(key)? {
            Some(CV::Table(val, definition)) => Ok(Some(Value { val, definition })),
            Some(val) => Err(self.expected("table", key, &val)),
            None => Ok(None),
        }
    }

    fn get_config_env<T>(&self, key: &ConfigKey) -> Result<OptValue<T>, ConfigError>
    where
        T: std::str::FromStr,
        T::Err: fmt::Display,
    {
        match self.env(key.as_env_key()) {
            Some(value) => {
                let definition = Definition::Environment(key.as_env_key().to_owned());
                Ok(Some(Value {
                    val: value
                        .parse()
                        .map_err(|e| ConfigError::new(format!("{e}"), definition.clone()))?,
                    definition,
                }))
            }
            None => Ok(None),
        }
    }

    pub(crate) fn expected(&self, ty: &str, key: &ConfigKey, val: &ConfigValue) -> anyhow::Error {
        anyhow!(
            "invalid configuration for key `{key}`\nexpected a {ty}, but found a {} for `{key}` in {}",
            val.desc(),
            val.definition()
        )
    }

    get_value_typed! {get_integer_value, i64, Integer, "an integer"}
    get_value_typed! {get_bool_value, bool, Boolean, "true/false"}
    get_value_typed! {get_string_value, String, String, "a string"}

    /// `[build]`, as cargo's `GlobalContext::build_config`.
    pub fn build_config(&self) -> anyhow::Result<&CargoBuildConfig> {
        if let Some(build) = self.build_config.get() {
            return Ok(build);
        }
        let build = self.get::<CargoBuildConfig>("build")?;
        Ok(self.build_config.get_or_init(|| build))
    }

    /// `[net]`, as cargo's `GlobalContext::net_config`.
    pub fn net_config(&self) -> anyhow::Result<CargoNetConfig> {
        self.get::<CargoNetConfig>("net")
    }

    /// `[profile.<name>]` of the configuration, as cargo's `get_config_profile` reads it before
    /// validating it as a profile.
    pub fn config_profile(
        &self,
        name: &str,
    ) -> anyhow::Result<OptValue<cargo_util_schemas::manifest::TomlProfile>> {
        self.get(&format!("profile.{name}"))
    }

    /// The variables `[env]` adds to what cargo runs, as cargo's `GlobalContext::env_config`:
    /// each unless the process has it already or it is `force`d.
    pub fn env_config(&self) -> anyhow::Result<BTreeMap<String, OsString>> {
        let env_config = self.get::<EnvConfig>("env")?;
        for disallowed in ["CARGO_HOME", "RUSTUP_HOME", "RUSTUP_TOOLCHAIN"] {
            if env_config.contains_key(disallowed) {
                bail!(
                    "setting the `{disallowed}` environment variable is not supported \
                     in the `[env]` configuration table"
                );
            }
        }
        Ok(env_config
            .into_iter()
            .filter(|(k, v)| v.is_force() || self.env(k).is_none())
            .map(|(k, v)| {
                let value = v.resolve(&self.cwd).into_owned();
                (k, value)
            })
            .collect())
    }

    /// The `build.target-dir` of `CARGO_TARGET_DIR` or the config, as cargo's
    /// `GlobalContext::target_dir`.
    pub fn target_dir(&self) -> anyhow::Result<Option<PathBuf>> {
        if let Some(dir) = self.env("CARGO_TARGET_DIR") {
            if dir.is_empty() {
                bail!(
                    "the target directory is set to an empty string in the `CARGO_TARGET_DIR` environment variable"
                )
            }
            return Ok(Some(self.cwd.join(dir)));
        }
        let Some(val) = &self.build_config()?.target_dir else { return Ok(None) };
        if val.raw_value().is_empty() {
            bail!("the target directory is set to an empty string in {}", val.value().definition)
        }
        Ok(Some(val.resolve_path(&self.cwd)))
    }

    /// `build.build-dir` with its templates filled in, as cargo's `GlobalContext::build_dir`.
    pub fn build_dir(&self, workspace_manifest_path: &Path) -> anyhow::Result<Option<PathBuf>> {
        let Some(val) = &self.build_config()?.build_dir else { return Ok(None) };
        let root = workspace_manifest_path.parent().unwrap();
        let replacements = [
            (
                "{workspace-root}",
                root.to_str().context("workspace root was not valid utf-8")?.to_owned(),
            ),
            (
                "{cargo-cache-home}",
                self.home.to_str().context("cargo home was not valid utf-8")?.to_owned(),
            ),
        ];
        if val.raw_value().contains("{workspace-path-hash}") {
            // cargo's hash of the path is its `StableHasher` over `Path`'s `Hash`, not ported yet.
            bail!(
                "`{{workspace-path-hash}}` in build.build-dir (defined in {}) is not supported by blaze yet",
                val.value().definition
            );
        }
        let mut value = val.raw_value().to_owned();
        for (from, to) in &replacements {
            value = value.replace(from, to);
        }
        if let (Some(open), Some(close)) = (value.find('{'), value.rfind('}'))
            && open < close
        {
            let variable = &value[open + 1..close];
            let names = ["workspace-root", "cargo-cache-home", "workspace-path-hash"];
            let suggestion = crate::edit_distance::closest_msg(
                variable,
                names.iter(),
                |k| k,
                "template variable",
            );
            let suggestion = if suggestion.is_empty() {
                format!(
                    "\n\nhelp: available template variables are {}",
                    names.iter().map(|v| format!("`{{{v}}}`")).collect::<Vec<_>>().join(", ")
                )
            } else {
                suggestion
            };
            bail!(
                "unexpected variable `{variable}` in build.build-dir path `{}`{suggestion}",
                val.raw_value()
            );
        }
        if value.contains('{') {
            bail!("unexpected opening bracket `{{` in build.build-dir path `{}`", val.raw_value());
        }
        if value.contains('}') {
            bail!("unexpected closing bracket `}}` in build.build-dir path `{}`", val.raw_value());
        }
        if val.raw_value().is_empty() {
            bail!("the build directory is set to an empty string in {}", val.value().definition)
        }
        Ok(Some(val.value().definition.root(&self.cwd).join(value)))
    }

    /// `registries.<name>.index`, as cargo's `GlobalContext::get_registry_index`.
    pub fn registry_index(&self, registry: &str) -> anyhow::Result<Url> {
        cargo_util_schemas::manifest::RegistryName::new(registry)?;
        let Some(index) = self.get_string(&format!("registries.{registry}.index"))? else {
            bail!("registry index was not found in any configuration: `{registry}`");
        };
        self.resolve_registry_index(&index).with_context(|| {
            format!("invalid index URL for registry `{registry}` defined in {}", index.definition)
        })
    }

    fn resolve_registry_index(&self, index: &Value<String>) -> anyhow::Result<Url> {
        let base = index.definition.root(&self.cwd).join("truncated-by-url_with_base");
        crate::source::into_url(&index.val)?;
        let base = blaze_source::file_url(&base).ok();
        let url = Url::options()
            .base_url(base.as_ref())
            .parse(&index.val)
            .map_err(|e| anyhow!("invalid url `{}`: {}", index.val, e))?;
        if url.password().is_some() {
            bail!("registry URLs may not contain passwords");
        }
        Ok(url)
    }

    /// Reads `$CARGO_HOME/credentials.toml` (or the old `credentials`) once, as cargo's
    /// `load_credentials`: a top-level `token` is the old spelling of `registry.token`.
    pub fn load_credentials(&self) -> anyhow::Result<()> {
        self.credentials().map(|_| ())
    }

    fn credentials(&self) -> anyhow::Result<&HashMap<String, ConfigValue, Global>> {
        if let Some(credentials) = self.credentials.get() {
            return Ok(credentials);
        }
        let mut credentials = HashMap::new_in(Global);
        if let Some(path) = self.get_file_path(&self.home, "credentials") {
            let value = self.load_file(&path, &mut HashSet::new_in(Global), true, false)?;
            let CV::Table(mut map, def) = value else { unreachable!() };
            if let Some(token) = map.remove("token") {
                map.entry("registry".to_owned()).or_insert_with(|| {
                    CV::Table(HashMap::from_iter_in([("token".to_owned(), token)], Global), def)
                });
            }
            credentials = map;
        }
        Ok(self.credentials.get_or_init(|| credentials))
    }

    /// The token of `table` (`registry` for crates.io, `registries.<name>`) as cargo's
    /// `registry_credentials` takes it: `credentials.toml`'s, unless the configuration's
    /// comes from a place cargo ranks above that file (its environment variable).
    pub fn credentials_token(&self, table: &str) -> anyhow::Result<OptValue<String>> {
        let key = ConfigKey::parse(&format!("{table}.token"));
        let from_file = match Self::get_cv_in(&key, self.credentials()?)? {
            Some(CV::String(val, definition)) => Some(Value { val, definition }),
            Some(cv) => return Err(ConfigError::expected(&key, "a string", &cv).into()),
            None => None,
        };
        let configured = self.get::<OptValue<String>>(&format!("{table}.token"))?;
        Ok(match (configured, from_file) {
            (Some(configured), Some(from_file))
                if configured.definition.is_higher_priority(&from_file.definition) =>
            {
                Some(configured)
            }
            (_, Some(from_file)) => Some(from_file),
            (configured, None) => configured,
        })
    }
}

impl Config<Disk> {
    /// Writes `token` for `registry` (crates.io when `None`) to the credentials file, or with
    /// no token removes the registry's, as cargo's `save_credentials`: the old `credentials`
    /// if it is there, else `credentials.toml`, mode 0600.
    pub fn save_credentials(
        &self,
        token: Option<&str>,
        registry: Option<&str>,
    ) -> anyhow::Result<()> {
        let name = match self.get_file_path_quiet(&self.home, "credentials") {
            Some(path) => path.file_name().map_or("credentials.toml".into(), PathBuf::from),
            None => PathBuf::from("credentials.toml"),
        };
        std::fs::create_dir_all(&self.home)
            .with_context(|| format!("failed to create directory `{}`", self.home.display()))?;
        let path = self.home.join(name);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("failed to open: {}", path.display()))?;
        let mut file = stdx::file_lock::Locked::lock(file)
            .with_context(|| format!("failed to lock file: {}", path.display()))?;
        let mut contents = String::new();
        std::io::Read::read_to_string(&mut *file, &mut contents)
            .with_context(|| format!("failed to read configuration file `{}`", path.display()))?;
        let mut toml: toml::Table = contents.parse()?;
        if let Some(token) = toml.remove("token") {
            toml.insert(
                "registry".into(),
                toml::Table::from_iter([("token".into(), token)]).into(),
            );
        }
        match token {
            Some(token) => {
                let def = Definition::Path(path.clone());
                let table = CV::Table(
                    HashMap::from_iter_in(
                        [("token".to_owned(), CV::String(token.to_owned(), def.clone()))],
                        Global,
                    ),
                    def.clone(),
                );
                let (key, mut value) = match registry {
                    Some(registry) => (
                        "registries",
                        CV::Table(
                            HashMap::from_iter_in([(registry.to_owned(), table)], Global),
                            def.clone(),
                        ),
                    ),
                    None => ("registry", table),
                };
                if registry.is_some()
                    && let Some(table) = toml.remove("registries")
                {
                    value.merge(CV::from_toml(def, table)?, false)?;
                }
                toml.insert(key.into(), value.into_toml());
            }
            None => {
                let table = match registry {
                    Some(registry) => {
                        toml.get_mut("registries").and_then(|it| it.get_mut(registry))
                    }
                    None => toml.get_mut("registry"),
                };
                if let Some(table) = table {
                    let what =
                        registry.map_or("[registry]".to_owned(), |it| format!("[registries.{it}]"));
                    let table = table
                        .as_table_mut()
                        .with_context(|| format!("expected `{what}` to be a table"))?;
                    table.remove("token");
                }
            }
        }
        let contents = toml.to_string();
        use std::io::{Seek as _, Write as _};
        file.seek(std::io::SeekFrom::Start(0))?;
        file.write_all(contents.as_bytes())
            .with_context(|| format!("failed to write to `{}`", path.display()))?;
        file.set_len(contents.len() as u64)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .with_context(|| format!("failed to set permissions of `{}`", path.display()))?;
        }
        Ok(())
    }
}

impl<F: Files> Config<F> {
    /// `registry.default`, the registry `--registry` defaults to.
    pub fn default_registry(&self) -> anyhow::Result<Option<String>> {
        Ok(self.get_string("registry.default")?.map(|it| it.val))
    }

    /// Fails if `registry.index` is set, as cargo does since it stopped supporting it.
    pub fn check_registry_index_not_set(&self) -> anyhow::Result<()> {
        if self.get_string("registry.index")?.is_some() {
            bail!(
                "the `registry.index` config value is no longer supported\n\
                 Use `[source]` replacement to alter the default index for crates.io."
            );
        }
        Ok(())
    }
}

/// One config file's text as a value defined at `path` (from `--config` with `cli`).
fn parse_file(path: &Path, contents: &str, cli: bool) -> anyhow::Result<ConfigValue> {
    let toml: toml::Table = contents
        .parse()
        .with_context(|| format!("could not parse TOML configuration in `{}`", path.display()))?;
    let def = if cli { Definition::Cli(Some(path.into())) } else { Definition::Path(path.into()) };
    CV::from_toml(def, toml::Value::Table(toml))
        .with_context(|| format!("failed to load TOML configuration from `{}`", path.display()))
}

struct Include {
    path: PathBuf,
    def: Definition,
    optional: bool,
}

fn include_paths(cv: &mut ConfigValue, remove: bool) -> anyhow::Result<Vec<Include>> {
    let CV::Table(table, _) = cv else { unreachable!() };
    let include = if remove { table.remove("include") } else { table.get("include").cloned() };
    let includes = match include {
        Some(CV::List(list, _)) => list
            .into_iter()
            .enumerate()
            .map(|(idx, cv)| match cv {
                CV::String(s, def) => Ok(Include { path: s.into(), def, optional: false }),
                CV::Table(mut table, def) => {
                    let path = match table.remove("path") {
                        Some(CV::String(s, _)) => s,
                        Some(other) => bail!(
                            "expected a string, but found {} at `include[{idx}].path` in `{def}`",
                            other.desc()
                        ),
                        None => bail!("missing field `path` at `include[{idx}]` in `{def}`"),
                    };
                    let optional = match table.remove("optional") {
                        Some(CV::Boolean(b, _)) => b,
                        Some(other) => bail!(
                            "expected a boolean, but found {} at `include[{idx}].optional` in `{def}`",
                            other.desc()
                        ),
                        None => false,
                    };
                    Ok(Include { path: path.into(), def, optional })
                }
                other => bail!(
                    "expected a string or table, but found {} at `include[{idx}]` in {}",
                    other.desc(),
                    other.definition(),
                ),
            })
            .collect::<anyhow::Result<Vec<_>>>()?,
        Some(other) => bail!(
            "expected a list of strings or a list of tables, but found {} at `include` in `{}",
            other.desc(),
            other.definition()
        ),
        None => return Ok(Vec::new()),
    };
    for include in &includes {
        if include.path.extension() != Some(std::ffi::OsStr::new("toml")) {
            bail!(
                "expected a config include path ending with `.toml`, but found `{}` from `{}`",
                include.path.display(),
                include.def,
            )
        }
        if let Some(path) = include.path.to_str() {
            if path.contains(['*', '?', '[', ']']) {
                bail!(
                    "expected a config include path without glob patterns, but found `{}` from `{}`",
                    include.path.display(),
                    include.def,
                )
            }
            if path.contains(['{', '}']) {
                bail!(
                    "expected a config include path without template braces, but found `{}` from `{}`",
                    include.path.display(),
                    include.def,
                )
            }
        }
    }
    Ok(includes)
}

impl<F: Files> Config<F> {
    /// [`Config::get_file_path`] without its warnings, as cargo looks again on saving.
    fn get_file_path_quiet(&self, dir: &Path, name: &str) -> Option<PathBuf> {
        let possible = dir.join(name);
        if self.files.exists(&possible) {
            return Some(possible);
        }
        let with_extension = dir.join(format!("{name}.toml"));
        self.files.exists(&with_extension).then_some(with_extension)
    }

    /// `config` if it exists, else `config.toml`: cargo prefers the old name, with a warning,
    /// unless it is the same file.
    fn get_file_path(&self, dir: &Path, name: &str) -> Option<PathBuf> {
        let possible = dir.join(name);
        let with_extension = dir.join(format!("{name}.toml"));
        if let Ok(canonical) = self.files.canonicalize(&possible) {
            match self.files.canonicalize(&with_extension) {
                Ok(other) if other != canonical => self.warn(format!(
                    "both `{}` and `{}` exist. Using `{}`",
                    possible.display(),
                    with_extension.display(),
                    possible.display()
                )),
                Ok(_) => {}
                Err(_) => {
                    let title =
                        format!("`{}` is deprecated in favor of `{name}.toml`", possible.display());
                    let help = format!(
                        "if you need to support cargo 1.38 or earlier, you can symlink `{name}` to `{name}.toml`"
                    );
                    let report =
                        Level::WARNING.secondary_title(title).element(Level::HELP.message(help));
                    let rendered = Renderer::plain().render(&[report]);
                    self.warn(rendered.strip_prefix("warning: ").unwrap_or(&rendered).to_owned());
                }
            }
            return Some(possible);
        }
        self.files.exists(&with_extension).then_some(with_extension)
    }
}

impl ConfigValue {
    fn into_toml(self) -> toml::Value {
        match self {
            CV::Boolean(it, _) => toml::Value::Boolean(it),
            CV::String(it, _) => toml::Value::String(it),
            CV::Integer(it, _) => toml::Value::Integer(it),
            CV::List(it, _) => toml::Value::Array(it.into_iter().map(CV::into_toml).collect()),
            // cargo's tables are a `HashMap`, in no order; these are in the keys' order.
            CV::Table(it, _) => {
                let mut entries: Vec<_> = it.into_iter().collect();
                entries.sort_by(|a, b| a.0.cmp(&b.0));
                toml::Value::Table(entries.into_iter().map(|(k, v)| (k, v.into_toml())).collect())
            }
        }
    }
}

/// A `--config KEY=VALUE` argument, which must be one dotted key and a value.
fn cli_dotted_keys(arg: &str) -> anyhow::Result<toml::Table> {
    const HELP: &str =
        "--config argument `{arg}` was not a TOML dotted key expression (such as `build.jobs = 2`)";
    let fail = || anyhow!(HELP.replace("{arg}", arg));
    let table: toml::Table = arg.parse().map_err(|e: toml::de::Error| {
        anyhow!(e.to_string()).context(format!(
            "failed to parse value from --config argument `{arg}` as a dotted key expression"
        ))
    })?;
    // Exactly one key path ending in a value that is not a table.
    let mut current = &table;
    loop {
        if current.len() != 1 {
            return Err(fail());
        }
        let (_, value) = current.iter().next().unwrap();
        match value {
            toml::Value::Table(t) => current = t,
            _ => break,
        }
    }
    if arg.contains('\n') || arg.trim_start().starts_with('[') {
        return Err(fail());
    }
    Ok(table)
}

/// `$CARGO_HOME` relative to `cwd`, or `.cargo` in the home directory, as the `home` crate
/// cargo uses finds it.
fn cargo_home(cwd: &Path, env: &BTreeMap<String, String>) -> anyhow::Result<PathBuf> {
    match env.get("CARGO_HOME").filter(|h| !h.is_empty()) {
        Some(home) => Ok(cwd.join(home)),
        None => {
            let dir = home_dir(env)
                .ok_or_else(|| anyhow!("Cargo couldn't find your home directory. This probably means that $HOME was not set."))?;
            Ok(dir.join(".cargo"))
        }
    }
}

fn home_dir(env: &BTreeMap<String, String>) -> Option<PathBuf> {
    if cfg!(windows) {
        env.get("USERPROFILE")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from)
            .or_else(std::env::home_dir)
    } else {
        env.get("HOME").filter(|h| !h.is_empty()).map(PathBuf::from).or_else(std::env::home_dir)
    }
}
