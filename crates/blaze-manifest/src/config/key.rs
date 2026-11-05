//! A dotted config key and the environment variable it reads, as cargo's `ConfigKey`.

use std::borrow::Cow;
use std::fmt;

#[derive(Debug, Clone)]
pub struct ConfigKey {
    env: String,
    /// Each part, and the length of `env` before it.
    parts: Vec<(String, usize)>,
}

impl ConfigKey {
    pub fn new() -> ConfigKey {
        ConfigKey { env: "CARGO".to_owned(), parts: Vec::new() }
    }

    /// `build.target-dir` and so on. No TOML quoting: a part with a dot cannot be named.
    pub fn parse(key: &str) -> ConfigKey {
        let mut cfg = ConfigKey::new();
        for part in key.split('.') {
            cfg.push(part);
        }
        cfg
    }

    /// A part whose variable is upper-cased with `-` as `_`.
    pub fn push(&mut self, name: &str) {
        let env = name.replace('-', "_").to_uppercase();
        self.push_with_env(&env, name);
    }

    /// A part whose variable keeps its case, as a package name in `profile.dev.package.<name>`.
    pub fn push_sensitive(&mut self, name: &str) {
        self.push_with_env(name, name);
    }

    fn push_with_env(&mut self, env: &str, config: &str) {
        self.parts.push((config.to_owned(), self.env.len()));
        self.env.push('_');
        self.env.push_str(env);
    }

    pub fn pop(&mut self) {
        let (_, env) = self.parts.pop().unwrap();
        self.env.truncate(env);
    }

    pub fn as_env_key(&self) -> &str {
        &self.env
    }

    pub fn parts(&self) -> impl Iterator<Item = &str> {
        self.parts.iter().map(|p| p.0.as_str())
    }

    pub fn is_root(&self) -> bool {
        self.parts.is_empty()
    }

    /// Whether the key is `pattern`, where a `*` part matches any one part.
    pub fn matches(&self, pattern: &str) -> bool {
        let mut parts = self.parts();
        pattern.split('.').all(|pat| parts.next() == Some(pat) || pat == "*")
            && parts.next().is_none()
    }
}

impl Default for ConfigKey {
    fn default() -> ConfigKey {
        ConfigKey::new()
    }
}

impl fmt::Display for ConfigKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let parts: Vec<_> = self.parts().map(escape_key_part).collect();
        parts.join(".").fmt(f)
    }
}

#[derive(Debug, Clone)]
pub(crate) enum KeyOrIdx {
    Key(String),
    Idx(usize),
}

/// The path to a value inside an array item, which a TOML key cannot name: `a.b[3].c`. The
/// parts are pushed innermost first, as an error travels out.
#[derive(Debug, Clone)]
pub(crate) struct ArrayItemKeyPath {
    base: ConfigKey,
    path: Vec<KeyOrIdx>,
}

impl ArrayItemKeyPath {
    pub(crate) fn new(base: ConfigKey) -> Self {
        Self { base, path: Vec::new() }
    }

    pub(crate) fn push_key(&mut self, k: String) {
        self.path.push(KeyOrIdx::Key(k))
    }

    pub(crate) fn push_index(&mut self, i: usize) {
        self.path.push(KeyOrIdx::Idx(i))
    }
}

impl fmt::Display for ArrayItemKeyPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.base)?;
        for k in self.path.iter().rev() {
            match k {
                KeyOrIdx::Key(s) => {
                    f.write_str(".")?;
                    f.write_str(&escape_key_part(s))?;
                }
                KeyOrIdx::Idx(i) => write!(f, "[{i}]")?,
            }
        }
        Ok(())
    }
}

pub(crate) fn escape_key_part(part: &str) -> Cow<'_, str> {
    if part.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')) {
        Cow::Borrowed(part)
    } else {
        Cow::Owned(toml::Value::from(part).to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn array_item_key_path_display() {
        let mut key_path = ArrayItemKeyPath::new(ConfigKey::parse("array"));
        key_path.push_index(1);
        key_path.push_key("rustflags".into());
        key_path.push_key("thumbv8m.base-none-eabi".into());
        key_path.push_index(2);
        key_path.push_index(3);
        assert_eq!(key_path.to_string(), r#"array[3][2]."thumbv8m.base-none-eabi".rustflags[1]"#);
    }

    #[test]
    fn env_keys() {
        let mut key = ConfigKey::parse("profile.dev.package");
        key.push_sensitive("my-pkg");
        key.push("opt-level");
        assert_eq!(key.as_env_key(), "CARGO_PROFILE_DEV_PACKAGE_my-pkg_OPT_LEVEL");
        key.pop();
        assert_eq!(key.to_string(), "profile.dev.package.my-pkg");
    }
}
