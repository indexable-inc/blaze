//! The shape of agents/annoyances/home-2.json: "`CARGO_HOME`, else `.cargo` in `HOME`" written
//! out in several functions (`crates/blaze-client/src/place.rs:179`,
//! `crates/blaze-manifest/src/config/mod.rs:1347`, `crates/blaze-codegen-code/src/lib.rs:1179`).

use std::path::{Path, PathBuf};

pub struct Env {
    pairs: Vec<(String, String)>,
}

impl Env {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }
}

// Found: the rule, once.
pub fn cargo_home(env: &Env) -> Option<PathBuf> {
    match env.get("CARGO_HOME").filter(|it| !it.is_empty()) {
        Some(home) => Some(PathBuf::from(home)),
        None => env.get("HOME").map(|home| Path::new(home).join(".cargo")),
    }
}

pub mod config {
    use super::Env;
    use std::path::{Path, PathBuf};

    // Found: the same rule again, in another module.
    pub fn home_of(env: &Env) -> Option<PathBuf> {
        let set = |key: &str| env.get(key).filter(|it| !it.is_empty());
        match set("CARGO_HOME") {
            Some(home) => Some(PathBuf::from(home)),
            None => set("HOME").map(|home| Path::new(home).join(".cargo")),
        }
    }
}

// Not found: names the variable to pass it on, and derives nothing from `HOME`.
pub fn forwarded(env: &Env) -> Vec<(String, String)> {
    ["CARGO_HOME", "RUSTUP_HOME"]
        .iter()
        .filter_map(|key| env.get(key).map(|v| ((*key).to_owned(), v.to_owned())))
        .collect()
}

// Not found: `.cargo` of a project directory, not of a home.
pub fn project_config(dir: &Path) -> PathBuf {
    dir.join(".cargo").join("config.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    // Not found: a test of the rule restates it as data.
    #[test]
    fn home_wins() {
        let env = Env { pairs: vec![("CARGO_HOME".to_owned(), "/c".to_owned())] };
        assert_eq!(cargo_home(&env), Some(PathBuf::from("/c")));
        assert_eq!(Path::new("/u").join(".cargo"), PathBuf::from("/u/.cargo"));
    }
}
