//! Paths and command lines in the configuration, relative to where they were defined, as
//! cargo's `src/context/path.rs`.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde::de::Error as _;

use super::Value;

/// A path relative to the directory above the `.cargo/` of the file that set it, or to the
/// current directory when the environment or `--config` did.
#[derive(Debug, Deserialize, PartialEq, Clone)]
#[serde(transparent)]
pub struct ConfigRelativePath(Value<String>);

impl ConfigRelativePath {
    pub fn new(path: Value<String>) -> ConfigRelativePath {
        ConfigRelativePath(path)
    }

    pub fn value(&self) -> &Value<String> {
        &self.0
    }

    pub fn raw_value(&self) -> &str {
        &self.0.val
    }

    pub fn resolve_path(&self, cwd: &Path) -> PathBuf {
        self.0.definition.root(cwd).join(&self.0.val)
    }

    /// A program: a path when it has a separator, else a name to look up in `PATH`.
    pub fn resolve_program(&self, cwd: &Path) -> PathBuf {
        string_to_path(&self.0.val, &self.0.definition, cwd)
    }
}

pub(crate) fn string_to_path(value: &str, definition: &super::Definition, cwd: &Path) -> PathBuf {
    let is_path = value.contains('/') || (cfg!(windows) && value.contains('\\'));
    if is_path { definition.root(cwd).join(value) } else { PathBuf::from(value) }
}

/// A program and its arguments, as a list or a string split at whitespace: `runner`, wrappers.
#[derive(Debug, Clone, PartialEq)]
pub struct PathAndArgs {
    pub path: ConfigRelativePath,
    pub args: Vec<String>,
}

impl<'de> Deserialize<'de> for PathAndArgs {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let vsl = Value::<StringList>::deserialize(deserializer)?;
        let mut strings = vsl.val.0;
        if strings.is_empty() {
            return Err(D::Error::invalid_length(0, &"at least one element"));
        }
        let first = strings.remove(0);
        Ok(PathAndArgs {
            path: ConfigRelativePath(Value { val: first, definition: vsl.definition }),
            args: strings,
        })
    }
}

impl PathAndArgs {
    pub fn from_whitespace_separated_string(p: &Value<String>) -> PathAndArgs {
        let mut iter = p.val.split_ascii_whitespace().map(str::to_owned);
        let val = iter.next().unwrap_or_default();
        PathAndArgs {
            path: ConfigRelativePath(Value { val, definition: p.definition.clone() }),
            args: iter.collect(),
        }
    }
}

/// A list of strings written as a list or as one string split at whitespace:
/// `a = 'a b c'` is `b = ['a', 'b', 'c']`. Its variable's words are appended.
#[derive(Debug, Deserialize, Clone, PartialEq)]
pub struct StringList(Vec<String>);

impl StringList {
    pub fn as_slice(&self) -> &[String] {
        &self.0
    }
}
