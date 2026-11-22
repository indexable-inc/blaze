//! The one error of the configuration: reading `.cargo/config.toml` and its includes, `--config`,
//! a typed value at a key (cargo's `ConfigError`), the tables built on them and `cargo config
//! get`. Each is a [`ConfigWhat`] (what is wrong, naming the file, the key path and where the
//! value was defined), the definition of the value it is about when that is known, and what
//! caused it: cargo prints the causes under `Caused by:`, so the chain is [`Error::source`].

use std::error::Error;
use std::fmt;
use std::path::PathBuf;

use super::key::{ArrayItemKeyPath, ConfigKey};
use super::{ConfigValue, Definition};

/// An error of the configuration. Its text is cargo's where cargo has one; a value's
/// definition is named with its line and column ([`Definition::located`]).
pub struct ConfigError(Box<Inner>);

struct Inner {
    what: ConfigWhat,
    /// Where the value the error is about was defined: `error in {that}: ` comes first.
    definition: Option<Definition>,
    cause: Option<Cause>,
}

enum Cause {
    Config(ConfigError),
    Io(std::io::Error),
    Toml(TomlError),
}

/// What is wrong.
pub enum ConfigWhat {
    // Reading the files.
    CouldNotLoad,
    CurrentDir,
    NoCargoHome { why: blaze_core::home::NoHome },
    ReadFile { path: PathBuf },
    ParseToml { path: PathBuf },
    LoadToml { path: PathBuf },
    /// A value under `key_path` could not be read (`a.b[1].c`).
    ParseAt { key_path: String },
    /// A float or datetime, which the configuration has no place for.
    UnsupportedType { kind: &'static str },
    MergeFile { path: PathBuf },
    MergeKey { key: String, into: Definition, from: Definition },
    MergeType { from: Definition, into: Definition, expected: &'static str, found: &'static str },
    // `include`.
    IncludeCycle { path: PathBuf },
    LoadInclude { path: PathBuf, from: Definition },
    IncludeNotList { found: &'static str, definition: Definition },
    IncludeItem { index: usize, found: &'static str, definition: Definition },
    IncludePathNotString { index: usize, found: &'static str, definition: Definition },
    IncludeMissingPath { index: usize, definition: Definition },
    IncludeOptionalNotBool { index: usize, found: &'static str, definition: Definition },
    IncludeNotToml { path: PathBuf, definition: Definition },
    IncludeGlob { path: PathBuf, definition: Definition },
    IncludeBraces { path: PathBuf, definition: Definition },
    // `--config`.
    LoadCliFile { path: PathBuf },
    CliParse { arg: String },
    CliNotDotted { arg: String },
    CliToken,
    CliRegistryToken { registry: String },
    CliConvert { arg: String },
    CliInclude,
    CliMerge { arg: String },
    // `-Z`.
    ZFlagValue { name: String, value: String },
    ZFlagUnknown { name: String },
    // A value at a key.
    NotATable { key: String, found: &'static str, definition: Definition },
    Missing { key: String },
    MissingField { field: &'static str },
    ExpectedTable,
    Expected { key: String, expected: &'static str, found: &'static str },
    InvalidFor { key: String, ty: &'static str, found: &'static str, definition: Definition },
    LoadKey { key: String },
    /// An environment variable that is no value of its type: the parse error's text.
    EnvValue { message: String },
    /// What a `Deserialize` impl refused, in serde's words.
    Serde { message: String },
    // The tables.
    EnvDisallowed { name: &'static str },
    TargetDirEmptyEnv,
    TargetDirEmpty { definition: Definition },
    NotUtf8 { what: &'static str },
    BuildDirHash { definition: Definition },
    BuildDirVariable { variable: String, value: String, help: String },
    BuildDirOpen { value: String },
    BuildDirClose { value: String },
    BuildDirEmpty { definition: Definition },
    RegistryIndexMissing { registry: String },
    RegistryIndexInvalid { registry: String, definition: Definition },
    InvalidUrl { url: String, error: url::ParseError },
    UrlPassword,
    RegistryIndexSet,
    UnsupportedProtocol { protocol: String, definition: Definition },
    InvalidSourceUrl { key: String, definition: Definition },
    SourceNeedsGit { name: String, key: &'static str, definition: Definition },
    SourceNoLocation { name: String },
    SourceTwoLocations { name: String },
    SourceNotFound { name: String, orig: String, location: String },
    SourceCycle { name: String, location: String },
    SeveralMatching {
        what: &'static str,
        first: String,
        first_in: Definition,
        second: String,
        second_in: Definition,
    },
    TargetEmpty,
    TargetSpec { target: String },
    NoHostLine { verbose: String },
    ParseCfg { output: String },
    // `$CARGO_HOME/credentials.toml`.
    CreateDir { path: PathBuf },
    Open { path: PathBuf },
    Lock { path: PathBuf },
    WriteFile { path: PathBuf },
    Permissions { path: PathBuf },
    CredentialsNotTable { what: String },
    // `cargo config get`.
    UnknownFormat { format: String },
    /// A command line cargo's argument parser refuses: printed as it is, exit code 1.
    Usage { text: String },
    ValueRequired { name: String },
    UnexpectedArgument { arg: String },
    ExpectedConfigGet { found: String },
    NoShowOrigin { format: &'static str },
    NoMergedNo { format: &'static str },
    NotSet { key: String },
    /// A `profile.<name>` value that does not fit the manifest's profile schema, in its words.
    Schema(crate::schema::SchemaError),
    /// A typed error of another crate (blaze-source's `SourceId`, the manifest's URL parsing,
    /// a registry name, blaze-toml's writer, the caller's rustc): its text and causes, as they
    /// are.
    // dyn: another crate's error, held only on an error path; its types are many.
    Foreign(Box<dyn Error + Send + Sync>),
}

impl fmt::Display for ConfigWhat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use ConfigWhat as W;
        match self {
            W::CouldNotLoad => f.write_str("could not load Cargo configuration"),
            W::CurrentDir => f.write_str("couldn't get the current directory of the process"),
            W::NoCargoHome { why } => write!(
                f,
                "CARGO_HOME is not set, and cargo's home is .cargo in your home directory: {why}"
            ),
            W::ReadFile { path } => {
                write!(f, "failed to read configuration file `{}`", path.display())
            }
            W::ParseToml { path } => {
                write!(f, "could not parse TOML configuration in `{}`", path.display())
            }
            W::LoadToml { path } => {
                write!(f, "failed to load TOML configuration from `{}`", path.display())
            }
            W::ParseAt { key_path } => write!(f, "failed to parse config at `{key_path}`"),
            W::UnsupportedType { kind } => {
                write!(f, "unsupported TOML configuration type `{kind}`")
            }
            W::MergeFile { path } => {
                write!(f, "failed to merge configuration at `{}`", path.display())
            }
            W::MergeKey { key, into, from } => {
                write!(f, "failed to merge key `{key}` between {into} and {from}")
            }
            W::MergeType { from, into, expected, found } => write!(
                f,
                "failed to merge config value from `{from}` into `{into}`: expected {expected}, \
                 but found {found}"
            ),
            W::IncludeCycle { path } => {
                write!(f, "config `include` cycle detected with path `{}`", path.display())
            }
            W::LoadInclude { path, from } => {
                write!(f, "failed to load config include `{}` from `{from}`", path.display())
            }
            // cargo 1.99's text, its unclosed backtick too.
            W::IncludeNotList { found, definition } => write!(
                f,
                "expected a list of strings or a list of tables, but found {found} at `include` \
                 in `{definition}"
            ),
            W::IncludeItem { index, found, definition } => write!(
                f,
                "expected a string or table, but found {found} at `include[{index}]` in \
                 {definition}"
            ),
            W::IncludePathNotString { index, found, definition } => write!(
                f,
                "expected a string, but found {found} at `include[{index}].path` in `{definition}`"
            ),
            W::IncludeMissingPath { index, definition } => {
                write!(f, "missing field `path` at `include[{index}]` in `{definition}`")
            }
            W::IncludeOptionalNotBool { index, found, definition } => write!(
                f,
                "expected a boolean, but found {found} at `include[{index}].optional` in \
                 `{definition}`"
            ),
            W::IncludeNotToml { path, definition } => write!(
                f,
                "expected a config include path ending with `.toml`, but found `{}` from \
                 `{definition}`",
                path.display()
            ),
            W::IncludeGlob { path, definition } => write!(
                f,
                "expected a config include path without glob patterns, but found `{}` from \
                 `{definition}`",
                path.display()
            ),
            W::IncludeBraces { path, definition } => write!(
                f,
                "expected a config include path without template braces, but found `{}` from \
                 `{definition}`",
                path.display()
            ),
            W::LoadCliFile { path } => write!(f, "failed to load config from `{}`", path.display()),
            W::CliParse { arg } => write!(
                f,
                "failed to parse value from --config argument `{arg}` as a dotted key expression"
            ),
            W::CliNotDotted { arg } => write!(
                f,
                "--config argument `{arg}` was not a TOML dotted key expression (such as \
                 `build.jobs = 2`)"
            ),
            W::CliToken => {
                f.write_str("registry.token cannot be set through --config for security reasons")
            }
            W::CliRegistryToken { registry } => write!(
                f,
                "registries.{registry}.token cannot be set through --config for security reasons"
            ),
            W::CliConvert { arg } => write!(f, "failed to convert --config argument `{arg}`"),
            W::CliInclude => f.write_str("failed to load --config include"),
            W::CliMerge { arg } => write!(f, "failed to merge --config argument `{arg}`"),
            W::ZFlagValue { name, value } => {
                write!(f, "flag -Z{name} expected `no` or `yes`, found: `{value}`")
            }
            W::ZFlagUnknown { name } => write!(f, "blaze does not read `-Z {name}` yet"),
            W::NotATable { key, found, definition } => write!(
                f,
                "expected table for configuration key `{key}`, but found {found} in {definition}"
            ),
            W::Missing { key } => write!(f, "missing config key `{key}`"),
            W::MissingField { field } => write!(f, "missing field `{field}`"),
            W::ExpectedTable => f.write_str("expected table"),
            W::Expected { key, expected, found } => {
                write!(f, "`{key}` expected {expected}, but found a {found}")
            }
            W::InvalidFor { key, ty, found, definition } => write!(
                f,
                "invalid configuration for key `{key}`\nexpected a {ty}, but found a {found} for \
                 `{key}` in {}",
                definition.located()
            ),
            W::LoadKey { key } => write!(f, "could not load config key `{key}`"),
            W::EnvValue { message } | W::Serde { message } => f.write_str(message),
            W::EnvDisallowed { name } => write!(
                f,
                "setting the `{name}` environment variable is not supported in the `[env]` \
                 configuration table"
            ),
            W::TargetDirEmptyEnv => f.write_str(
                "the target directory is set to an empty string in the `CARGO_TARGET_DIR` \
                 environment variable",
            ),
            W::TargetDirEmpty { definition } => write!(
                f,
                "the target directory is set to an empty string in {}",
                definition.located()
            ),
            W::NotUtf8 { what } => write!(f, "{what} was not valid utf-8"),
            W::BuildDirHash { definition } => write!(
                f,
                "`{{workspace-path-hash}}` in build.build-dir (defined in {}) is not supported \
                 by blaze yet",
                definition.located()
            ),
            W::BuildDirVariable { variable, value, help } => {
                write!(f, "unexpected variable `{variable}` in build.build-dir path `{value}`{help}")
            }
            W::BuildDirOpen { value } => {
                write!(f, "unexpected opening bracket `{{` in build.build-dir path `{value}`")
            }
            W::BuildDirClose { value } => {
                write!(f, "unexpected closing bracket `}}` in build.build-dir path `{value}`")
            }
            W::BuildDirEmpty { definition } => write!(
                f,
                "the build directory is set to an empty string in {}",
                definition.located()
            ),
            W::RegistryIndexMissing { registry } => {
                write!(f, "registry index was not found in any configuration: `{registry}`")
            }
            W::RegistryIndexInvalid { registry, definition } => write!(
                f,
                "invalid index URL for registry `{registry}` defined in {}",
                definition.located()
            ),
            W::InvalidUrl { url, error } => write!(f, "invalid url `{url}`: {error}"),
            W::UrlPassword => f.write_str("registry URLs may not contain passwords"),
            W::RegistryIndexSet => f.write_str(
                "the `registry.index` config value is no longer supported\n\
                 Use `[source]` replacement to alter the default index for crates.io.",
            ),
            W::UnsupportedProtocol { protocol, definition } => write!(
                f,
                "unsupported registry protocol `{protocol}` (defined in {})",
                definition.located()
            ),
            W::InvalidSourceUrl { key, definition } => write!(
                f,
                "configuration key `{key}` specified an invalid URL (in {definition})"
            ),
            W::SourceNeedsGit { name, key, definition } => write!(
                f,
                "source definition `source.{name}` specifies `{key}`, but that requires a `git` \
                 key to be specified (in {definition})"
            ),
            W::SourceNoLocation { name } => write!(
                f,
                "no source location specified for `source.{name}`, need `registry`, \
                 `local-registry`, `directory`, or `git` defined"
            ),
            W::SourceTwoLocations { name } => {
                write!(f, "more than one source location specified for `source.{name}`")
            }
            W::SourceNotFound { name, orig, location } => write!(
                f,
                "could not find a configured source with the name `{name}` when attempting to \
                 lookup `{orig}` (configuration in `{location}`)"
            ),
            W::SourceCycle { name, location } => write!(
                f,
                "detected a cycle of `replace-with` sources, the source `{name}` is eventually \
                 replaced with itself (configuration in `{location}`)"
            ),
            W::SeveralMatching { what, first, first_in, second, second_in } => write!(
                f,
                "several matching instances of `target.'cfg(..)'.{what}` in configurations\n\
                 first match `{first}` located in {first_in}\n\
                 second match `{second}` located in {second_in}"
            ),
            W::TargetEmpty => f.write_str("target was empty"),
            W::TargetSpec { target } => {
                write!(f, "blaze does not read a target spec file yet: `{target}`")
            }
            W::NoHostLine { verbose } => {
                write!(f, "`rustc -vV` didn't have a line for `host:`, got:\n{verbose}")
            }
            W::ParseCfg { output } => {
                write!(f, "failed to parse the cfg from `rustc --print=cfg`, got:\n{output}")
            }
            W::CreateDir { path } => write!(f, "failed to create directory `{}`", path.display()),
            W::Open { path } => write!(f, "failed to open: {}", path.display()),
            W::Lock { path } => write!(f, "failed to lock file: {}", path.display()),
            W::WriteFile { path } => write!(f, "failed to write to `{}`", path.display()),
            W::Permissions { path } => {
                write!(f, "failed to set permissions of `{}`", path.display())
            }
            W::CredentialsNotTable { what } => write!(f, "expected `{what}` to be a table"),
            W::UnknownFormat { format } => write!(f, "unknown config format `{format}`"),
            W::Usage { text } => f.write_str(text),
            W::ValueRequired { name } => write!(f, "a value is required for '{name} <VALUE>'"),
            W::UnexpectedArgument { arg } => write!(f, "unexpected argument '{arg}' found"),
            W::ExpectedConfigGet { found } => {
                write!(f, "expected `config get [KEY]`, found `{found}`")
            }
            W::NoShowOrigin { format } => write!(
                f,
                "the `{format}` format does not support --show-origin, try the `toml` format \
                 instead"
            ),
            W::NoMergedNo { format } => write!(
                f,
                "the `{format}` format does not support --merged=no, try the `toml` format instead"
            ),
            W::NotSet { key } => write!(f, "config value `{key}` is not set"),
            W::Schema(error) => error.fmt(f),
            W::Foreign(error) => error.fmt(f),
        }
    }
}

/// A text that is no TOML, as cargo shows it: the line and column, the line with a caret under
/// the place, then what blaze-toml found wrong.
pub struct TomlError {
    line: usize,
    column: usize,
    /// The line the error is on, without its newline.
    text: String,
    what: blaze_toml::What,
}

impl TomlError {
    pub(crate) fn new(error: &blaze_toml::Error, text: &str) -> TomlError {
        let (line, column) = error.line_col(text);
        let at = text.floor_char_boundary(error.at);
        let start = text[..at].rfind('\n').map_or(0, |i| i + 1);
        let end = text[at..].find('\n').map_or(text.len(), |i| at + i);
        let line_text = text[start..end].strip_suffix('\r').unwrap_or(&text[start..end]);
        TomlError { line, column, text: line_text.to_owned(), what: error.what }
    }
}

impl fmt::Display for TomlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let number = self.line.to_string();
        let gutter = " ".repeat(number.len());
        writeln!(f, "TOML parse error at line {}, column {}", self.line, self.column)?;
        writeln!(f, "{gutter} |")?;
        writeln!(f, "{number} | {}", self.text)?;
        writeln!(f, "{gutter} | {}^", " ".repeat(self.column.saturating_sub(1)))?;
        match cargo_wording(self.what) {
            Some(text) => f.write_str(text),
            None => write!(f, "{}", self.what),
        }
    }
}

/// cargo's words (the `toml` crate's) for the refusals a config file most often meets, so that
/// what cargo users know reads the same; blaze-toml's own for the rest.
fn cargo_wording(what: blaze_toml::What) -> Option<&'static str> {
    use blaze_toml::{Expected, What};
    match what {
        What::End { expected: Expected::CloseHeader }
        | What::Unexpected { expected: Expected::CloseHeader, .. } => {
            Some("unclosed table, expected `]`")
        }
        What::End { expected: Expected::CloseArrayHeader }
        | What::Unexpected { expected: Expected::CloseArrayHeader, .. } => {
            Some("unclosed array of tables, expected `]]`")
        }
        What::End { expected: Expected::Equals }
        | What::Unexpected { expected: Expected::Equals, .. } => {
            Some("key with no value, expected `=`")
        }
        _ => None,
    }
}

impl fmt::Debug for TomlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl Error for TomlError {}

impl ConfigError {
    pub(crate) fn new(what: ConfigWhat) -> ConfigError {
        ConfigError(Box::new(Inner { what, definition: None, cause: None }))
    }

    /// `what`, about a value defined at `definition`.
    pub(crate) fn defined(what: ConfigWhat, definition: Definition) -> ConfigError {
        ConfigError(Box::new(Inner { what, definition: Some(definition), cause: None }))
    }

    /// `what`, caused by `error`.
    pub(crate) fn io(what: ConfigWhat, error: std::io::Error) -> ConfigError {
        ConfigError(Box::new(Inner { what, definition: None, cause: Some(Cause::Io(error)) }))
    }

    /// `what`, caused by the TOML `text` that blaze-toml refused with `error`.
    pub(crate) fn toml(what: ConfigWhat, error: &blaze_toml::Error, text: &str) -> ConfigError {
        let cause = Cause::Toml(TomlError::new(error, text));
        ConfigError(Box::new(Inner { what, definition: None, cause: Some(cause) }))
    }

    /// Another crate's typed error as it is ([`ConfigWhat::Foreign`]). The bound is `Error`,
    /// not `Into<Box<dyn Error>>`, so that neither a string nor an `anyhow::Error` fits.
    pub(crate) fn foreign(error: impl Error + Send + Sync + 'static) -> ConfigError {
        ConfigError::new(ConfigWhat::Foreign(Box::new(error)))
    }

    /// A `profile.<name>` value the schema refused, as cargo reports a value its deserializer
    /// refused: `could not load config key` of the value's key path, where it was defined,
    /// caused by what is wrong.
    pub(crate) fn schema(
        error: crate::schema::SchemaError,
        definition: Option<Definition>,
    ) -> ConfigError {
        let key = error.key.clone();
        let mut error =
            ConfigError::new(ConfigWhat::Schema(error)).context(ConfigWhat::LoadKey { key });
        error.0.definition = definition;
        error
    }

    /// `what`, caused by this error.
    pub(crate) fn context(self, what: ConfigWhat) -> ConfigError {
        ConfigError(Box::new(Inner { what, definition: None, cause: Some(Cause::Config(self)) }))
    }

    pub fn what(&self) -> &ConfigWhat {
        &self.0.what
    }

    /// Where the value the error is about was defined, when known.
    pub fn definition(&self) -> Option<&Definition> {
        self.0.definition.as_ref()
    }

    pub(crate) fn expected(key: &ConfigKey, expected: &'static str, found: &ConfigValue) -> ConfigError {
        let what = ConfigWhat::Expected { key: key.to_string(), expected, found: found.desc() };
        ConfigError::defined(what, found.definition().clone())
    }

    pub(crate) fn is_missing_field(&self) -> bool {
        matches!(self.0.what, ConfigWhat::MissingField { .. })
    }

    pub(crate) fn missing(key: &ConfigKey) -> ConfigError {
        ConfigError::new(ConfigWhat::Missing { key: key.to_string() })
    }

    pub(crate) fn with_key_context(
        self,
        key: &ConfigKey,
        definition: Option<Definition>,
    ) -> ConfigError {
        let mut error = self.context(ConfigWhat::LoadKey { key: key.to_string() });
        error.0.definition = definition;
        error
    }

    pub(crate) fn with_array_item_key_context(
        self,
        key: &ArrayItemKeyPath,
        definition: Option<Definition>,
    ) -> ConfigError {
        let mut error = self.context(ConfigWhat::ParseAt { key_path: key.to_string() });
        error.0.definition = definition;
        error
    }

    /// As cargo prints an error after `error: `: this one's text, then each cause under
    /// `Caused by:`, indented.
    pub fn report(&self) -> String {
        let mut out = self.to_string();
        // dyn: std's error chain, read only on an error path.
        let mut cause: Option<&(dyn Error + 'static)> = self.source();
        while let Some(error) = cause {
            out.push_str("\n\nCaused by:");
            for line in error.to_string().lines() {
                out.push('\n');
                if !line.is_empty() {
                    out.push_str("  ");
                    out.push_str(line);
                }
            }
            cause = error.source();
        }
        out
    }
}

/// `.context(|| what)` on a result: the error, if any, becomes the cause of `what`.
pub(crate) trait Context<T> {
    fn context(self, what: impl FnOnce() -> ConfigWhat) -> Result<T, ConfigError>;
}

impl<T> Context<T> for Result<T, ConfigError> {
    fn context(self, what: impl FnOnce() -> ConfigWhat) -> Result<T, ConfigError> {
        self.map_err(|error| error.context(what()))
    }
}

impl Error for ConfigError {
    // dyn: std's error chain, read only on an error path.
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match &self.0.cause {
            Some(Cause::Config(error)) => Some(error),
            Some(Cause::Io(error)) => Some(error),
            Some(Cause::Toml(error)) => Some(error),
            None => match &self.0.what {
                ConfigWhat::Foreign(error) => error.source(),
                _ => None,
            },
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(definition) = &self.0.definition {
            write!(f, "error in {}: {}", definition.located(), self.0.what)
        } else {
            self.0.what.fmt(f)
        }
    }
}

impl fmt::Debug for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.report())
    }
}

impl serde::de::Error for ConfigError {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        ConfigError::new(ConfigWhat::Serde { message: msg.to_string() })
    }

    fn missing_field(field: &'static str) -> Self {
        ConfigError::new(ConfigWhat::MissingField { field })
    }
}
