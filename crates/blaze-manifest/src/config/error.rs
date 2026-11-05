//! The error of reading a typed value from the configuration, as cargo's `ConfigError`: the
//! message, and where the value was defined when that is known.

use std::fmt;

use super::key::{ArrayItemKeyPath, ConfigKey};
use super::{ConfigValue, Definition};

#[derive(Debug)]
pub struct ConfigError {
    error: anyhow::Error,
    definition: Option<Definition>,
}

impl ConfigError {
    pub(crate) fn new(message: String, definition: Definition) -> ConfigError {
        ConfigError { error: anyhow::Error::msg(message), definition: Some(definition) }
    }

    pub(crate) fn expected(key: &ConfigKey, expected: &str, found: &ConfigValue) -> ConfigError {
        ConfigError {
            error: anyhow::anyhow!("`{key}` expected {expected}, but found a {}", found.desc()),
            definition: Some(found.definition().clone()),
        }
    }

    pub(crate) fn is_missing_field(&self) -> bool {
        self.error.downcast_ref::<MissingFieldError>().is_some()
    }

    pub(crate) fn missing(key: &ConfigKey) -> ConfigError {
        ConfigError { error: anyhow::anyhow!("missing config key `{key}`"), definition: None }
    }

    pub(crate) fn with_key_context(
        self,
        key: &ConfigKey,
        definition: Option<Definition>,
    ) -> ConfigError {
        ConfigError {
            error: anyhow::Error::from(self).context(format!("could not load config key `{key}`")),
            definition,
        }
    }

    pub(crate) fn with_array_item_key_context(
        self,
        key: &ArrayItemKeyPath,
        definition: Option<Definition>,
    ) -> ConfigError {
        ConfigError {
            error: anyhow::Error::from(self).context(format!("failed to parse config at `{key}`")),
            definition,
        }
    }
}

impl std::error::Error for ConfigError {
    // dyn: std's error chain, read only on an error path.
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.error.source()
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(definition) = &self.definition {
            write!(f, "error in {definition}: {}", self.error)
        } else {
            self.error.fmt(f)
        }
    }
}

#[derive(Debug)]
struct MissingFieldError(String);

impl fmt::Display for MissingFieldError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "missing field `{}`", self.0)
    }
}

impl std::error::Error for MissingFieldError {}

impl serde::de::Error for ConfigError {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        ConfigError { error: anyhow::Error::msg(msg.to_string()), definition: None }
    }

    fn missing_field(field: &'static str) -> Self {
        ConfigError {
            error: anyhow::Error::new(MissingFieldError(field.to_owned())),
            definition: None,
        }
    }
}

impl From<anyhow::Error> for ConfigError {
    fn from(error: anyhow::Error) -> Self {
        ConfigError { error, definition: None }
    }
}
