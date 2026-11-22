//! `.cargo/config.toml` as cargo 1.99 reads it: the files from the current directory up and in
//! `$CARGO_HOME`, their `include`s, `--config` arguments and `CARGO_*` variables, merged by
//! cargo's rules (`src/context/mod.rs` and `config_value.rs` there).

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};

use blaze_hash::{FixedState, Map, MapOpts, Set, SetOpts};
use blaze_ids::radix;
use blaze_toml::{Document, Item, Kind, Limits};
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
mod profile;
pub mod schema;
mod source;
pub mod target;
mod value;

use error::Context as _;
pub use error::{ConfigError, ConfigWhat, TomlError};
pub use key::ConfigKey;
pub use path::{ConfigRelativePath, PathAndArgs, StringList};
pub use schema::*;
pub use source::{Replacement, SourceConfig};
pub use target::{CompileKind, Flags, TargetCfgConfig, TargetConfig};
pub use value::{Definition, Located, OptValue, Place, Value};

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
    Table(Map<String, ConfigValue, FixedState, Global>, Definition),
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
    /// The table `root` of a document read from the file (or `--config` argument) `def`: the
    /// root defined by `def` itself, each value under it at its own place in the text.
    fn from_item(def: &Definition, root: Item<'_, '_, Global>) -> Result<ConfigValue, ConfigError> {
        let mut path = Vec::new();
        Self::from_item_inner(def, def.clone(), root, &mut path).map_err(|kind| {
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
            ConfigError::new(ConfigWhat::UnsupportedType { kind })
                .context(ConfigWhat::ParseAt { key_path })
        })
    }

    /// `item`, defined at `here`; the name of its TOML type when the configuration has no
    /// place for it, with `path` the way to it, innermost first.
    fn from_item_inner(
        def: &Definition,
        here: Definition,
        item: Item<'_, '_, Global>,
        path: &mut Vec<KeyOrIdx>,
    ) -> Result<ConfigValue, &'static str> {
        let child = |item: Item<'_, '_, Global>| def.at(value::Place::of(item.line_col()));
        match item.kind() {
            Kind::String => match item.as_str() {
                Some(val) => Ok(CV::String(val.to_owned(), here)),
                None => unreachable!("a string item has its text"),
            },
            Kind::Bool => match item.as_bool() {
                Some(b) => Ok(CV::Boolean(b, here)),
                None => unreachable!("a bool item has its value"),
            },
            Kind::Integer => match item.as_integer() {
                Some(i) => Ok(CV::Integer(i, here)),
                None => unreachable!("an integer item has its value"),
            },
            Kind::Array => Ok(CV::List(
                item.children()
                    .enumerate()
                    .map(|(i, it)| {
                        CV::from_item_inner(def, child(it), it, path)
                            .inspect_err(|_| path.push(KeyOrIdx::Idx(i)))
                    })
                    .collect::<Result<_, _>>()?,
                here,
            )),
            Kind::Table => Ok(CV::Table(
                item.children()
                    .map(|it| match CV::from_item_inner(def, child(it), it, path) {
                        Ok(value) => Ok((it.key().to_owned(), value)),
                        Err(e) => {
                            path.push(KeyOrIdx::Key(it.key().to_owned()));
                            Err(e)
                        }
                    })
                    .collect::<Result<_, _>>()?,
                here,
            )),
            Kind::Float => Err("float"),
            Kind::Datetime => Err("datetime"),
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
    fn merge(&mut self, from: ConfigValue, force: bool) -> Result<(), ConfigError> {
        self.merge_helper(from, force, &mut Vec::new())
    }

    fn merge_helper(
        &mut self,
        from: ConfigValue,
        force: bool,
        parts: &mut Vec<String>,
    ) -> Result<(), ConfigError> {
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
                            result.context(|| ConfigWhat::MergeKey {
                                key: key.clone(),
                                into: entry.definition().clone(),
                                from: new_def,
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
                return Err(ConfigError::new(ConfigWhat::MergeType {
                    from: found.definition().clone(),
                    into: expected.definition().clone(),
                    expected: expected.desc(),
                    found: found.desc(),
                }));
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
    user: UserRecord,
    cli: Vec<String>,
    /// The files are read at the first value asked for, and again at the next while they fail:
    /// a broken config file is an error only to what reads configuration, as in cargo.
    #[expect(
        clippy::disallowed_types,
        reason = "the configuration's parts, each read the first time it is asked for: per config, not global"
    )]
    values: std::cell::OnceCell<Map<String, ConfigValue, FixedState, Global>>,
    /// `$CARGO_HOME/credentials.toml`, once [`Config::load_credentials`] read it: kept apart
    /// from the files' values, as cargo keeps it, for [`Config::credentials_token`] only.
    #[expect(
        clippy::disallowed_types,
        reason = "the configuration's parts, each read the first time it is asked for: per config, not global"
    )]
    credentials: std::cell::OnceCell<Map<String, ConfigValue, FixedState, Global>>,
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
    fn add(&mut self, flag: &str) -> Result<(), ConfigError> {
        let (name, value) = match flag.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (flag, None),
        };
        let name = name.replace('_', "-");
        let bool_flag = || match value {
            None | Some("yes") => Ok(true),
            Some("no") => Ok(false),
            Some(other) => Err(ConfigError::new(ConfigWhat::ZFlagValue {
                name: name.clone(),
                value: other.to_owned(),
            })),
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
            _ => return Err(ConfigError::new(ConfigWhat::ZFlagUnknown { name })),
        }
        Ok(())
    }
}

impl Config<Disk> {
    /// The configuration seen from `cwd` on the machine's disk with the variables of `env` and
    /// the `--config` arguments `cli`, as `cargo` started there would: the driver's adapter
    /// over [`Config::load_with`], which reads the disk and the system's record of the user.
    /// `--config` reads the files at once.
    pub fn load(
        cwd: &Path,
        env: impl IntoIterator<Item = (String, String)>,
        cli: &[String],
    ) -> Result<Config<Disk>, ConfigError> {
        let mut config = Config::load_with(cwd, env, UserRecord::of_process(), Disk)?;
        config.merge_cli_args(cli)?;
        Ok(config)
    }

    /// The configuration this process sees: its current directory and environment.
    pub fn from_process() -> Result<Config<Disk>, ConfigError> {
        let cwd = std::env::current_dir()
            .map_err(|e| ConfigError::io(ConfigWhat::CurrentDir, e))?;
        Config::load(&cwd, std::env::vars(), &[])
    }
}

impl<F: Files> Config<F> {
    /// The configuration seen from `cwd` with the variables of `env` and the user's home as the
    /// system records it (`user`, asked only when `env` names neither `CARGO_HOME` nor a home),
    /// every file of it (`.cargo/config.toml` up from `cwd`, `$CARGO_HOME/config.toml`,
    /// `include`s, a `--config` file) read through `files` when a value is first asked for.
    /// Nothing of the process is read: every input is passed, so all of them can enter a key.
    pub fn load_with(
        cwd: &Path,
        env: impl IntoIterator<Item = (String, String)>,
        user: UserRecord,
        files: F,
    ) -> Result<Config<F>, ConfigError> {
        let env: BTreeMap<String, String> = env.into_iter().collect();
        let home = cargo_home(cwd, &env, &user)?;
        Ok(Config {
            files,
            cwd: cwd.to_path_buf(),
            home,
            env,
            user,
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
    pub fn cli_unstable(&mut self, flags: &[String]) -> Result<(), ConfigError> {
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
    pub fn start(&mut self, command: &str, cli: &[String]) -> Result<(), ConfigError> {
        if matches!(self.aliased_command(command), Ok(Some(alias)) if !alias.is_empty()) {
            self.warn(format!(
                "user-defined alias `{command}` is ignored, because it is shadowed by a built-in command"
            ));
        }
        self.merge_cli_args(cli)
    }

    fn aliased_command(&self, command: &str) -> Result<Option<Vec<String>>, ConfigError> {
        let alias_name = format!("alias.{command}");
        match self.get_string(&alias_name) {
            Ok(Some(record)) => {
                Ok(Some(record.val.split_whitespace().map(str::to_owned).collect()))
            }
            Ok(None) => Ok(None),
            Err(_) => self.get::<Option<Vec<String>>>(&alias_name),
        }
    }

    fn values(&self) -> Result<&Map<String, ConfigValue, FixedState, Global>, ConfigError> {
        if let Some(values) = self.values.get() {
            return Ok(values);
        }
        let values = self.load_values()?;
        Ok(self.values.get_or_init(|| values))
    }

    /// The same configuration read again, for a thread of its own to read: what reading the
    /// files warns of is this one's to say, not the copy's.
    pub fn reload(&self) -> Result<Config<F>, ConfigError>
    where
        F: Clone,
    {
        let mut copy = Config::load_with(&self.cwd, self.env.clone(), self.user.clone(), self.files.clone())?;
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

    /// The user's home as the system records it, as it was passed in.
    pub fn user(&self) -> &UserRecord {
        &self.user
    }

    fn load_values(&self) -> Result<Map<String, ConfigValue, FixedState, Global>, ConfigError> {
        let mut cfg = CV::Table(Map::new(MapOpts { hasher: FixedState, alloc: Global, capacity: None }), Definition::BuiltIn);
        self.walk_tree(&self.cwd, &self.home, |path| {
            let value = self.load_file(path, &mut Set::new(SetOpts { hasher: FixedState, alloc: Global, capacity: None }), true, false)?;
            cfg.merge(value, false)
                .context(|| ConfigWhat::MergeFile { path: path.to_path_buf() })
        })
        .context(|| ConfigWhat::CouldNotLoad)?;
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
        mut walk: impl FnMut(&Path) -> Result<(), ConfigError>,
    ) -> Result<(), ConfigError> {
        let mut seen_dir = Set::new(SetOpts { hasher: FixedState, alloc: Global, capacity: None });
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
        seen: &mut Set<PathBuf, FixedState, Global>,
        includes: bool,
        cli: bool,
    ) -> Result<ConfigValue, ConfigError> {
        if !seen.insert(path.to_path_buf()) {
            return Err(ConfigError::new(ConfigWhat::IncludeCycle { path: path.to_path_buf() }));
        }
        let read = || ConfigWhat::ReadFile { path: path.to_path_buf() };
        let bytes = self.files.read(path).map_err(|e| ConfigError::io(read(), e))?;
        let contents = std::str::from_utf8(&bytes).map_err(|_| {
            let invalid = std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "stream did not contain valid UTF-8",
            );
            ConfigError::io(read(), invalid)
        })?;
        let value = parse_file(path, contents, cli)?;
        if includes { self.load_includes(value, seen, cli) } else { Ok(value) }
    }

    fn load_includes(
        &self,
        mut value: ConfigValue,
        seen: &mut Set<PathBuf, FixedState, Global>,
        cli: bool,
    ) -> Result<ConfigValue, ConfigError> {
        let includes = include_paths(&mut value, true)?;
        let mut root = CV::Table(Map::new(MapOpts { hasher: FixedState, alloc: Global, capacity: None }), value.definition().clone());
        for include in includes {
            let Some(abs_path) = self.include_path(&include) else { continue };
            self.load_file(&abs_path, seen, true, cli)
                .and_then(|include| root.merge(include, true))
                .context(|| ConfigWhat::LoadInclude {
                    path: include.path.clone(),
                    from: include.def.clone(),
                })?;
        }
        root.merge(value, true)?;
        Ok(root)
    }

    /// Where an include points, or `None` for an optional one that is not there.
    fn include_path(&self, include: &Include) -> Option<PathBuf> {
        let base = match &include.def {
            Definition::Path(p, _) | Definition::Cli(Some(p), _) => p.parent().unwrap(),
            _ => &self.cwd,
        };
        let abs_path = paths::normalize_path(&base.join(&include.path));
        (!include.optional || self.files.exists(&abs_path)).then_some(abs_path)
    }

    /// Each config file on its own, includes before what includes them, as cargo's
    /// `load_values_unmerged`.
    pub fn load_values_unmerged(&self) -> Result<Vec<ConfigValue>, ConfigError> {
        let mut result = Vec::new();
        let mut seen = Set::new(SetOpts { hasher: FixedState, alloc: Global, capacity: None });
        self.walk_tree(&self.cwd, &self.home, |path| {
            let mut cv = self.load_file(path, &mut seen, false, false)?;
            self.load_unmerged_include(&mut cv, &mut seen, &mut result)?;
            result.push(cv);
            Ok(())
        })
        .context(|| ConfigWhat::CouldNotLoad)?;
        Ok(result)
    }

    fn load_unmerged_include(
        &self,
        cv: &mut ConfigValue,
        seen: &mut Set<PathBuf, FixedState, Global>,
        output: &mut Vec<ConfigValue>,
    ) -> Result<(), ConfigError> {
        for include in include_paths(cv, false)? {
            let Some(abs_path) = self.include_path(&include) else { continue };
            let mut cv = self.load_file(&abs_path, seen, false, false).context(|| {
                ConfigWhat::LoadInclude { path: include.path.clone(), from: include.def.clone() }
            })?;
            self.load_unmerged_include(&mut cv, seen, output)?;
            output.push(cv);
        }
        Ok(())
    }

    fn merge_cli_args(&mut self, cli: &[String]) -> Result<(), ConfigError> {
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
    pub fn cli_args_as_table(&self) -> Result<ConfigValue, ConfigError> {
        let mut loaded = CV::Table(Map::new(MapOpts { hasher: FixedState, alloc: Global, capacity: None }), Definition::Cli(None, None));
        for arg in &self.cli {
            let arg_as_path = self.cwd.join(arg);
            let table = if !arg.is_empty() && self.files.exists(&arg_as_path) {
                self.load_file(&arg_as_path, &mut Set::new(SetOpts { hasher: FixedState, alloc: Global, capacity: None }), true, true)
                    .context(|| ConfigWhat::LoadCliFile { path: arg_as_path.clone() })?
            } else {
                let doc = cli_dotted_keys(arg)?;
                let root = doc.root();
                if root.get("registry").and_then(|v| v.get("token")).is_some() {
                    return Err(ConfigError::new(ConfigWhat::CliToken));
                }
                if let Some(registry) = root
                    .get("registries")
                    .filter(|v| v.is_table())
                    .and_then(|t| t.children().find(|v| v.get("token").is_some()))
                {
                    let registry = registry.key().to_owned();
                    return Err(ConfigError::new(ConfigWhat::CliRegistryToken { registry }));
                }
                let value = CV::from_item(&Definition::Cli(None, None), root)
                    .context(|| ConfigWhat::CliConvert { arg: arg.clone() })?;
                self.load_includes(value, &mut Set::new(SetOpts { hasher: FixedState, alloc: Global, capacity: None }), true)
                    .context(|| ConfigWhat::CliInclude)?
            };
            loaded.merge(table, true).context(|| ConfigWhat::CliMerge { arg: arg.clone() })?;
        }
        Ok(loaded)
    }

    /// `T` from the value at the dotted `key`, each part of it from the files or its
    /// environment variable as cargo's `GlobalContext::get` takes it. `""` is the root.
    pub fn get<'de, T: serde::Deserialize<'de>>(&self, key: &str) -> Result<T, ConfigError> {
        let key = if key.is_empty() { ConfigKey::new() } else { ConfigKey::parse(key) };
        let d = de::Deserializer { config: self, key, env_prefix_ok: true };
        T::deserialize(d)
    }

    pub fn get_string(&self, key: &str) -> Result<OptValue<String>, ConfigError> {
        self.get::<OptValue<String>>(key)
    }

    /// Reads the files now if they are not read yet, as cargo does on start to see its
    /// `[term]` table.
    pub fn ensure_loaded(&self) -> Result<(), ConfigError> {
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
    pub fn get_cv(&self, key: &ConfigKey) -> Result<Option<ConfigValue>, ConfigError> {
        Self::get_cv_in(key, self.values()?)
    }

    fn get_cv_in(
        key: &ConfigKey,
        vals: &Map<String, ConfigValue, FixedState, Global>,
    ) -> Result<Option<ConfigValue>, ConfigError> {
        if key.is_root() {
            return Ok(Some(CV::Table(vals.clone(), Definition::Path(PathBuf::new(), None))));
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
                    return Err(ConfigError::new(ConfigWhat::NotATable {
                        key: key_so_far.to_string(),
                        found: other.desc(),
                        definition: other.definition().clone(),
                    }));
                }
            }
        }
        Ok(Some(val.clone()))
    }

    /// The value at `key` with its environment variable taken into account, as cargo's
    /// `get_cv_with_env`: the variable wins over files, a list gets the variable's words
    /// appended.
    pub fn get_cv_with_env(&self, key: &ConfigKey) -> Result<Option<ConfigValue>, ConfigError> {
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

    pub(crate) fn has_key(
        &self,
        key: &ConfigKey,
        env_prefix_ok: bool,
    ) -> Result<bool, ConfigError> {
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
    ) -> Result<OptValue<Map<String, ConfigValue, FixedState, Global>>, ConfigError> {
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
                    val: value.parse().map_err(|e| {
                        let what = ConfigWhat::EnvValue { message: format!("{e}") };
                        ConfigError::defined(what, definition.clone())
                    })?,
                    definition,
                }))
            }
            None => Ok(None),
        }
    }

    pub(crate) fn expected(
        &self,
        ty: &'static str,
        key: &ConfigKey,
        val: &ConfigValue,
    ) -> ConfigError {
        ConfigError::new(ConfigWhat::InvalidFor {
            key: key.to_string(),
            ty,
            found: val.desc(),
            definition: val.definition().clone(),
        })
    }

    get_value_typed! {get_integer_value, i64, Integer, "an integer"}
    get_value_typed! {get_bool_value, bool, Boolean, "true/false"}
    get_value_typed! {get_string_value, String, String, "a string"}

    /// `[build]`, as cargo's `GlobalContext::build_config`.
    pub fn build_config(&self) -> Result<&CargoBuildConfig, ConfigError> {
        if let Some(build) = self.build_config.get() {
            return Ok(build);
        }
        let build = self.get::<CargoBuildConfig>("build")?;
        Ok(self.build_config.get_or_init(|| build))
    }

    /// `[net]`, as cargo's `GlobalContext::net_config`.
    pub fn net_config(&self) -> Result<CargoNetConfig, ConfigError> {
        self.get::<CargoNetConfig>("net")
    }

    /// `[profile.<name>]` of the configuration, as cargo's `get_config_profile` reads it before
    /// validating it as a profile.
    pub fn config_profile(
        &self,
        name: &str,
    ) -> Result<OptValue<crate::schema::TomlProfile>, ConfigError> {
        let key = format!("profile.{name}");
        profile::read(self.get(&key)?, &key)
    }

    /// The variables `[env]` adds to what cargo runs, as cargo's `GlobalContext::env_config`:
    /// each unless the process has it already or it is `force`d.
    pub fn env_config(&self) -> Result<BTreeMap<String, OsString>, ConfigError> {
        let env_config = self.get::<EnvConfig>("env")?;
        for disallowed in ["CARGO_HOME", "RUSTUP_HOME", "RUSTUP_TOOLCHAIN"] {
            if env_config.contains_key(disallowed) {
                return Err(ConfigError::new(ConfigWhat::EnvDisallowed { name: disallowed }));
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
    pub fn target_dir(&self) -> Result<Option<PathBuf>, ConfigError> {
        if let Some(dir) = self.env("CARGO_TARGET_DIR") {
            if dir.is_empty() {
                return Err(ConfigError::new(ConfigWhat::TargetDirEmptyEnv));
            }
            return Ok(Some(self.cwd.join(dir)));
        }
        let Some(val) = &self.build_config()?.target_dir else { return Ok(None) };
        if val.raw_value().is_empty() {
            let definition = val.value().definition.clone();
            return Err(ConfigError::new(ConfigWhat::TargetDirEmpty { definition }));
        }
        Ok(Some(val.resolve_path(&self.cwd)))
    }

    /// `build.build-dir` with its templates filled in, as cargo's `GlobalContext::build_dir`.
    pub fn build_dir(
        &self,
        workspace_manifest_path: &Path,
    ) -> Result<Option<PathBuf>, ConfigError> {
        let Some(val) = &self.build_config()?.build_dir else { return Ok(None) };
        let root = workspace_manifest_path.parent().unwrap();
        let utf8 = |path: &Path, what: &'static str| match path.to_str() {
            Some(path) => Ok(path.to_owned()),
            None => Err(ConfigError::new(ConfigWhat::NotUtf8 { what })),
        };
        let replacements = [
            ("{workspace-root}", utf8(root, "workspace root")?),
            ("{cargo-cache-home}", utf8(&self.home, "cargo home")?),
        ];
        if val.raw_value().contains("{workspace-path-hash}") {
            // cargo's hash of the path is its `StableHasher` over `Path`'s `Hash`, not ported yet.
            let definition = val.value().definition.clone();
            return Err(ConfigError::new(ConfigWhat::BuildDirHash { definition }));
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
            let what = ConfigWhat::BuildDirVariable {
                variable: variable.to_owned(),
                value: val.raw_value().to_owned(),
                help: suggestion,
            };
            return Err(ConfigError::new(what));
        }
        if value.contains('{') {
            let value = val.raw_value().to_owned();
            return Err(ConfigError::new(ConfigWhat::BuildDirOpen { value }));
        }
        if value.contains('}') {
            let value = val.raw_value().to_owned();
            return Err(ConfigError::new(ConfigWhat::BuildDirClose { value }));
        }
        if val.raw_value().is_empty() {
            let definition = val.value().definition.clone();
            return Err(ConfigError::new(ConfigWhat::BuildDirEmpty { definition }));
        }
        Ok(Some(val.value().definition.root(&self.cwd).join(value)))
    }

    /// `registries.<name>.index`, as cargo's `GlobalContext::get_registry_index`.
    pub fn registry_index(&self, registry: &str) -> Result<Url, ConfigError> {
        crate::schema::RegistryName::new(registry).map_err(ConfigError::foreign)?;
        let Some(index) = self.get_string(&format!("registries.{registry}.index"))? else {
            let registry = registry.to_owned();
            return Err(ConfigError::new(ConfigWhat::RegistryIndexMissing { registry }));
        };
        self.resolve_registry_index(&index).context(|| ConfigWhat::RegistryIndexInvalid {
            registry: registry.to_owned(),
            definition: index.definition.clone(),
        })
    }

    fn resolve_registry_index(&self, index: &Value<String>) -> Result<Url, ConfigError> {
        let base = index.definition.root(&self.cwd).join("truncated-by-url_with_base");
        crate::source::into_url(&index.val).map_err(ConfigError::foreign)?;
        let base = blaze_source::file_url(&base).ok();
        let url = Url::options().base_url(base.as_ref()).parse(&index.val).map_err(|error| {
            ConfigError::new(ConfigWhat::InvalidUrl { url: index.val.clone(), error })
        })?;
        if url.password().is_some() {
            return Err(ConfigError::new(ConfigWhat::UrlPassword));
        }
        Ok(url)
    }

    /// Reads `$CARGO_HOME/credentials.toml` (or the old `credentials`) once, as cargo's
    /// `load_credentials`: a top-level `token` is the old spelling of `registry.token`.
    pub fn load_credentials(&self) -> Result<(), ConfigError> {
        self.credentials().map(|_| ())
    }

    fn credentials(&self) -> Result<&Map<String, ConfigValue, FixedState, Global>, ConfigError> {
        if let Some(credentials) = self.credentials.get() {
            return Ok(credentials);
        }
        let mut credentials = Map::new(MapOpts { hasher: FixedState, alloc: Global, capacity: None });
        if let Some(path) = self.get_file_path(&self.home, "credentials") {
            let value = self.load_file(&path, &mut Set::new(SetOpts { hasher: FixedState, alloc: Global, capacity: None }), true, false)?;
            let CV::Table(mut map, def) = value else { unreachable!() };
            if let Some(token) = map.remove("token") {
                map.entry("registry".to_owned()).or_insert_with(|| {
                    CV::Table({
                        let iter = IntoIterator::into_iter([("token".to_owned(), token)]);
                        let capacity = Some(iter.size_hint().0);
                        let mut map = Map::new(MapOpts { hasher: FixedState, alloc: Global, capacity });
                        map.extend(iter);
                        map
                    }, def)
                });
            }
            credentials = map;
        }
        Ok(self.credentials.get_or_init(|| credentials))
    }

    /// The token of `table` (`registry` for crates.io, `registries.<name>`) as cargo's
    /// `registry_credentials` takes it: `credentials.toml`'s, unless the configuration's
    /// comes from a place cargo ranks above that file (its environment variable).
    pub fn credentials_token(&self, table: &str) -> Result<OptValue<String>, ConfigError> {
        let key = ConfigKey::parse(&format!("{table}.token"));
        let from_file = match Self::get_cv_in(&key, self.credentials()?)? {
            Some(CV::String(val, definition)) => Some(Value { val, definition }),
            Some(cv) => return Err(ConfigError::expected(&key, "a string", &cv)),
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
    ) -> Result<(), ConfigError> {
        use blaze_data::{Map, Value as Data};
        use blaze_own::String as Text;

        let name = match self.get_file_path_quiet(&self.home, "credentials") {
            Some(path) => path.file_name().map_or("credentials.toml".into(), PathBuf::from),
            None => PathBuf::from("credentials.toml"),
        };
        std::fs::create_dir_all(&self.home).map_err(|e| {
            ConfigError::io(ConfigWhat::CreateDir { path: self.home.clone() }, e)
        })?;
        let path = self.home.join(name);
        let io = |what: fn(PathBuf) -> ConfigWhat| {
            let path = path.clone();
            move |e| ConfigError::io(what(path.clone()), e)
        };
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(io(|path| ConfigWhat::Open { path }))?;
        let mut file = stdx::file_lock::Locked::lock(file)
            .map_err(io(|path| ConfigWhat::Lock { path }))?;
        let mut contents = String::new();
        std::io::Read::read_to_string(&mut *file, &mut contents)
            .map_err(io(|path| ConfigWhat::ReadFile { path }))?;
        let doc = Document::parse_in(&contents, Limits::CARGO, Global).map_err(|e| {
            ConfigError::toml(ConfigWhat::ParseToml { path: path.clone() }, &e, &contents)
        })?;
        let mut toml = doc.to_map_in(Global);
        let text = |s: &str| Text::from_str_in(s, Global);
        let table_of = |key: &str, value: Data<Global>| {
            let mut map = Map::new_in(Global);
            map.insert(text(key), value);
            Data::Map(map)
        };
        if let Some(token) = toml.remove("token") {
            toml.insert(text("registry"), table_of("token", token));
        }
        let not_table = |what: String| ConfigError::new(ConfigWhat::CredentialsNotTable { what });
        match (token, registry) {
            (Some(token), Some(registry)) => {
                // Into what `registries` holds, the token of this registry replacing its old one.
                if !toml.contains_key("registries") {
                    toml.insert(text("registries"), Data::Map(Map::new_in(Global)));
                }
                let Some(Data::Map(registries)) = toml.get_mut("registries") else {
                    return Err(not_table("[registries]".to_owned()));
                };
                if !registries.contains_key(registry) {
                    registries.insert(text(registry), Data::Map(Map::new_in(Global)));
                }
                let Some(Data::Map(table)) = registries.get_mut(registry) else {
                    return Err(not_table(format!("[registries.{registry}]")));
                };
                table.insert(text("token"), Data::Text(text(token)));
            }
            (Some(token), None) => {
                toml.insert(text("registry"), table_of("token", Data::Text(text(token))));
            }
            (None, registry) => {
                let table = match registry {
                    Some(registry) => match toml.get_mut("registries") {
                        Some(Data::Map(registries)) => registries.get_mut(registry),
                        Some(_) => return Err(not_table("[registries]".to_owned())),
                        None => None,
                    },
                    None => toml.get_mut("registry"),
                };
                if let Some(table) = table {
                    let Data::Map(table) = table else {
                        let what = registry
                            .map_or("[registry]".to_owned(), |it| format!("[registries.{it}]"));
                        return Err(not_table(what));
                    };
                    table.remove("token");
                }
            }
        }
        // In the keys' order, as cargo's `toml::Table` writes them; every table under a header.
        let mut contents = Text::new_in(Global);
        let options = blaze_toml::DocumentOptions { inline_from_depth: usize::MAX };
        blaze_toml::write_document(&sorted(&toml), &mut contents, options)
            .map_err(ConfigError::foreign)?;
        use std::io::{Seek as _, Write as _};
        let write = io(|path| ConfigWhat::WriteFile { path });
        file.seek(std::io::SeekFrom::Start(0)).map_err(&write)?;
        file.write_all(contents.as_bytes()).map_err(&write)?;
        file.set_len(contents.len() as u64).map_err(&write)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(io(|path| ConfigWhat::Permissions { path }))?;
        }
        Ok(())
    }
}

/// `map` with every table's keys in order, lists' tables too.
fn sorted(map: &blaze_data::Map<Global>) -> blaze_data::Map<Global> {
    use blaze_data::{Map, Value as Data};
    fn value(v: &Data<Global>) -> Data<Global> {
        match v {
            Data::Map(map) => Data::Map(sorted(map)),
            Data::List(list) => {
                let mut out = blaze_own::Vec::with_capacity_in(list.len(), Global);
                for item in list.iter() {
                    out.push(value(item));
                }
                Data::List(out)
            }
            other => other.clone(),
        }
    }
    let mut out = Map::new_in(Global);
    for (key, v) in map.sorted() {
        out.insert(blaze_own::String::from_str_in(key, Global), value(v));
    }
    out
}

impl<F: Files> Config<F> {
    /// `registry.default`, the registry `--registry` defaults to.
    pub fn default_registry(&self) -> Result<Option<String>, ConfigError> {
        Ok(self.get_string("registry.default")?.map(|it| it.val))
    }

    /// Fails if `registry.index` is set, as cargo does since it stopped supporting it.
    pub fn check_registry_index_not_set(&self) -> Result<(), ConfigError> {
        if self.get_string("registry.index")?.is_some() {
            return Err(ConfigError::new(ConfigWhat::RegistryIndexSet));
        }
        Ok(())
    }
}

/// One config file's text as a value defined at `path` (from `--config` with `cli`).
fn parse_file(path: &Path, contents: &str, cli: bool) -> Result<ConfigValue, ConfigError> {
    let doc = Document::parse_in(contents, Limits::CARGO, Global).map_err(|e| {
        ConfigError::toml(ConfigWhat::ParseToml { path: path.to_path_buf() }, &e, contents)
    })?;
    let def = match cli {
        true => Definition::Cli(Some(path.into()), None),
        false => Definition::Path(path.into(), None),
    };
    CV::from_item(&def, doc.root())
        .context(|| ConfigWhat::LoadToml { path: path.to_path_buf() })
}

struct Include {
    path: PathBuf,
    def: Definition,
    optional: bool,
}

fn refuse<T>(what: ConfigWhat) -> Result<T, ConfigError> {
    Err(ConfigError::new(what))
}

fn include_paths(cv: &mut ConfigValue, remove: bool) -> Result<Vec<Include>, ConfigError> {
    let CV::Table(table, _) = cv else { unreachable!() };
    let include = if remove { table.remove("include") } else { table.get("include").cloned() };
    let includes = match include {
        Some(CV::List(list, _)) => list
            .into_iter()
            .enumerate()
            .map(|(index, cv)| match cv {
                CV::String(s, def) => Ok(Include { path: s.into(), def, optional: false }),
                CV::Table(mut table, def) => {
                    let path = match table.remove("path") {
                        Some(CV::String(s, _)) => s,
                        Some(other) => {
                            let found = other.desc();
                            return refuse(ConfigWhat::IncludePathNotString {
                                index,
                                found,
                                definition: def,
                            });
                        }
                        None => {
                            return refuse(ConfigWhat::IncludeMissingPath { index, definition: def });
                        }
                    };
                    let optional = match table.remove("optional") {
                        Some(CV::Boolean(b, _)) => b,
                        Some(other) => {
                            let found = other.desc();
                            return refuse(ConfigWhat::IncludeOptionalNotBool {
                                index,
                                found,
                                definition: def,
                            });
                        }
                        None => false,
                    };
                    Ok(Include { path: path.into(), def, optional })
                }
                other => refuse(ConfigWhat::IncludeItem {
                    index,
                    found: other.desc(),
                    definition: other.definition().clone(),
                }),
            })
            .collect::<Result<Vec<_>, _>>()?,
        Some(other) => {
            return refuse(ConfigWhat::IncludeNotList {
                found: other.desc(),
                definition: other.definition().clone(),
            });
        }
        None => return Ok(Vec::new()),
    };
    for include in &includes {
        let (path, definition) = (include.path.clone(), include.def.clone());
        if include.path.extension() != Some(std::ffi::OsStr::new("toml")) {
            return refuse(ConfigWhat::IncludeNotToml { path, definition });
        }
        if let Some(text) = include.path.to_str() {
            if text.contains(['*', '?', '[', ']']) {
                return refuse(ConfigWhat::IncludeGlob { path, definition });
            }
            if text.contains(['{', '}']) {
                return refuse(ConfigWhat::IncludeBraces { path, definition });
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
                    let rendered = crate::snippet::with_notes(
                        crate::snippet::Level::Warning,
                        &title,
                        &[(crate::snippet::Level::Help, &help)],
                    );
                    self.warn(rendered.strip_prefix("warning: ").unwrap_or(&rendered).to_owned());
                }
            }
            return Some(possible);
        }
        self.files.exists(&with_extension).then_some(with_extension)
    }
}

/// A `--config KEY=VALUE` argument, which must be one dotted key and a value.
fn cli_dotted_keys(arg: &str) -> Result<Document<'_, Global>, ConfigError> {
    let fail = || ConfigError::new(ConfigWhat::CliNotDotted { arg: arg.to_owned() });
    let doc = Document::parse_in(arg, Limits::CARGO, Global).map_err(|e| {
        ConfigError::toml(ConfigWhat::CliParse { arg: arg.to_owned() }, &e, arg)
    })?;
    // Exactly one key path ending in a value that is not a table.
    let mut current = doc.root();
    loop {
        if current.len() != 1 {
            return Err(fail());
        }
        // `at` is an array's; a table's one entry is its first child.
        let Some(value) = current.children().next() else {
            unreachable!("a table of one has its first")
        };
        match value.kind() {
            Kind::Table => current = value,
            _ => break,
        }
    }
    if arg.contains('\n') || arg.trim_start().starts_with('[') {
        return Err(fail());
    }
    Ok(doc)
}

/// The home directory the operating system records for the user ([`blaze_core::home::record`]):
/// the one input of a configuration that is no variable, read by the driver and passed in so
/// it can enter a key. `Err` says why the system has none.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: a path, copied when a configuration is read again
pub struct UserRecord(pub Result<PathBuf, blaze_core::home::NoRecord>);

impl UserRecord {
    /// The record of the user this process runs as: for a driver, at its top.
    pub fn of_process() -> UserRecord {
        UserRecord(blaze_core::home::record())
    }
}

/// `$CARGO_HOME` relative to `cwd`, or `.cargo` in the home directory, as the `home` crate
/// cargo uses finds it.
fn cargo_home(cwd: &Path, env: &BTreeMap<String, String>, user: &UserRecord) -> Result<PathBuf, ConfigError> {
    match env.get("CARGO_HOME").filter(|h| !h.is_empty()) {
        Some(home) => Ok(cwd.join(home)),
        None => {
            // The home of the user whose environment `env` is ([`blaze_core::home::decide`],
            // the one answer): its `HOME`, else the system's record of the user as passed.
            let dir = blaze_core::home::decide(|name| env.get(name).map(std::ffi::OsString::from), || user.0.clone())
                .map_err(|why| ConfigError::new(ConfigWhat::NoCargoHome { why }))?;
            Ok(dir.join(".cargo"))
        }
    }
}
