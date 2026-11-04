//! blaze's user configuration: `config.toml` in blaze's home (`$BLAZE_HOME`, else `~/.blaze`,
//! where `versions/` is). The one place that finds blaze's home, names the file and reads it into
//! a table; each module reads its own table of it and says a bad setting through
//! [`UserConfigError::setting`]:
//!
//! - `[update]`: blaze's own updates (crates/blaze-engine/src/update.rs).
//! - `[terminal]`: what stderr's terminal does (crates/blaze-cargo/src/cli/terminal.rs).
//!
//! Not `home::root`: that is where the daemon keeps its state and the build directories
//! (`$BLAZE_HOME`, else a cache directory), which a user may delete; this is where what blaze
//! installs and what its user says live.

use std::fmt;
use std::path::{Path, PathBuf};

/// blaze's home: `$BLAZE_HOME`, else `.blaze` in the user's home (`HOME`, `USERPROFILE` on
/// Windows). None when no variable names one; the caller says what needed it.
pub fn home() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("BLAZE_HOME").filter(|it| !it.is_empty()) {
        return Some(PathBuf::from(home));
    }
    Some(user_home()?.join(".blaze"))
}

/// The user's home directory (`HOME`, `USERPROFILE` on Windows), none when unset or empty.
pub fn user_home() -> Option<PathBuf> {
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).filter(|it| !it.is_empty())?;
    Some(PathBuf::from(home))
}

/// `config.toml` under `home`.
pub fn file(home: &Path) -> PathBuf {
    home.join("config.toml")
}

/// `file` as a table, none when there is no file.
pub fn read(file: &Path) -> Result<Option<toml::Table>, UserConfigError> {
    let text = match std::fs::read_to_string(file) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(UserConfigError::Unread { file: file.to_owned(), error }),
    };
    toml::from_str(&text).map(Some).map_err(|error| UserConfigError::NotToml { file: file.to_owned(), error })
}

/// `[name]` of `table` (read from `file`): none when it has none; a `name` that is no table is
/// an error naming it.
pub fn section<'t>(table: &'t toml::Table, file: &Path, name: &str) -> Result<Option<&'t toml::Table>, UserConfigError> {
    match table.get(name) {
        None => Ok(None),
        Some(value) => value.as_table().map(Some).ok_or_else(|| UserConfigError::setting(file, name, "a table", value)),
    }
}

/// What is wrong with blaze's user configuration, naming the file and the setting.
#[derive(Debug)]
pub enum UserConfigError {
    /// The file is there and can't be read.
    Unread { file: PathBuf, error: std::io::Error },
    /// The file is not TOML.
    NotToml { file: PathBuf, error: toml::de::Error },
    /// `key` is set to `got`, which is not what it takes (`wants`).
    Setting { file: PathBuf, key: String, wants: String, got: String },
}

impl UserConfigError {
    pub fn setting(file: &Path, key: &str, wants: &str, got: &toml::Value) -> UserConfigError {
        UserConfigError::Setting { file: file.to_owned(), key: key.to_owned(), wants: wants.to_owned(), got: got.to_string() }
    }
}

impl fmt::Display for UserConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UserConfigError::Unread { file, error } => write!(f, "reading blaze's settings {}: {error}", file.display()),
            UserConfigError::NotToml { file, error } => {
                write!(f, "{} is not TOML, so blaze's settings in it can't be told: {error}", file.display())
            }
            UserConfigError::Setting { file, key, wants, got } => {
                write!(f, "{} sets `{key}` to {got}, which is not {wants}", file.display())
            }
        }
    }
}

impl std::error::Error for UserConfigError {}
