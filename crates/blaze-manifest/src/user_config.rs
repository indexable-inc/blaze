//! blaze's user configuration: `config.toml` in blaze's home (`$BLAZE_HOME`, else `~/.blaze`,
//! where `versions/` is). The one place that finds blaze's home, names the file and parses its
//! text (the caller reads it) into a table; each module reads its own table of it and says a bad setting through
//! [`UserConfigError::setting`]:
//!
//! - `[update]`: blaze's own updates (crates/blaze-engine/src/update.rs).
//! - `[terminal]`: what stderr's terminal does (crates/blaze-cargo/src/cli/terminal.rs).
//!
//! Not `home::root`: that is where the daemon keeps its state and the build directories
//! (`$BLAZE_HOME`, else a cache directory), which a user may delete; this is where what blaze
//! installs and what its user says live.

use std::alloc::Global;
use std::fmt;
use std::path::{Path, PathBuf};

use blaze_toml::{Document, Item, Kind, Limits};

/// blaze's home from the variables `var` reads and the system's record of the user (`record`,
/// asked only when no variable answers): `$BLAZE_HOME`, else `.blaze` in the user's home
/// ([`blaze_core::home::decide`]: `HOME`, else the record). A driver passes its process's
/// (`|name| std::env::var_os(name)`, [`blaze_core::home::record`]).
///
/// # Errors
///
/// [`blaze_core::home::NoHome`] when `BLAZE_HOME` is unset and the user has no home, saying
/// what was tried; the caller says what needed it.
pub fn home(
    var: impl Fn(&str) -> Option<std::ffi::OsString>,
    record: impl FnOnce() -> Result<PathBuf, blaze_core::home::NoRecord>,
) -> Result<PathBuf, blaze_core::home::NoHome> {
    if let Some(home) = var("BLAZE_HOME").filter(|it| !it.is_empty()) {
        return Ok(PathBuf::from(home));
    }
    Ok(blaze_core::home::decide(&var, record)?.join(".blaze"))
}

/// `config.toml` under `home`.
pub fn file(home: &Path) -> PathBuf {
    home.join("config.toml")
}

/// `text`, read from `file` by the caller (through the effect layer: this crate opens no file),
/// as a TOML document; its root is the table [`section`] looks in.
pub fn parse<'t>(file: &Path, text: &'t str) -> Result<Document<'t, Global>, UserConfigError> {
    Document::parse_in(text, Limits::CARGO, Global).map_err(|error| {
        let (line, column) = error.line_col(text);
        UserConfigError::NotToml { file: file.to_owned(), error, line, column }
    })
}

/// `[name]` of `table` (read from `file`): none when it has none; a `name` that is no table is
/// an error naming it.
pub fn section<'d, 't>(
    table: Item<'d, 't, Global>,
    file: &Path,
    name: &str,
) -> Result<Option<Item<'d, 't, Global>>, UserConfigError> {
    match table.get(name) {
        None => Ok(None),
        Some(value) if value.kind() == Kind::Table => Ok(Some(value)),
        Some(value) => Err(UserConfigError::setting(file, name, "a table", value)),
    }
}

/// What is wrong with blaze's user configuration, naming the file and the setting.
#[derive(Debug)]
pub enum UserConfigError {
    /// The file is not TOML: what blaze-toml refused, at `line:column` (1-based).
    NotToml { file: PathBuf, error: blaze_toml::Error, line: usize, column: usize },
    /// `key`, at `line:column`, is set to `got` (its text in the file), which is not what it
    /// takes (`wants`).
    Setting { file: PathBuf, key: String, wants: String, got: String, line: usize, column: usize },
}

impl UserConfigError {
    /// `key` set to `got`, an item of the document read from `file`, which is not `wants`.
    pub fn setting(file: &Path, key: &str, wants: &str, got: Item<'_, '_, Global>) -> UserConfigError {
        let (line, column) = got.line_col();
        let mut text = blaze_own::String::new_in(Global);
        if blaze_toml::write_inline(&got.to_value_in(Global), &mut text).is_err() {
            unreachable!("a value read from TOML is one TOML can write");
        }
        UserConfigError::Setting {
            file: file.to_owned(),
            key: key.to_owned(),
            wants: wants.to_owned(),
            got: text.as_str().to_owned(),
            line,
            column,
        }
    }
}

impl fmt::Display for UserConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UserConfigError::NotToml { file, error, line, column } => write!(
                f,
                "{}:{line}:{column} is not TOML, so blaze's settings in it can't be told: {}",
                file.display(),
                error.what
            ),
            UserConfigError::Setting { file, key, wants, got, line, column } => {
                write!(f, "{}:{line}:{column} sets `{key}` to {got}, which is not {wants}", file.display())
            }
        }
    }
}

impl std::error::Error for UserConfigError {}
