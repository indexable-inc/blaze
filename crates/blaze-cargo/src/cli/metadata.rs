//! `blaze metadata`: `cargo metadata --format-version 1`, made by blaze-metadata: the workspace
//! read by blaze-manifest and, unless `--no-deps`, every package of its resolve fetched by
//! blaze-registry and resolved by blaze-resolve. It runs in this process and asks nothing of
//! the daemon; the lock files are decided as a build's are (docs/cargo.md, "Lock files").

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

use blaze_core::exit::Exit;
use blaze_units::stdlib::target::{CfgArgs, target_cfgs};

use super::{
    own::{self, Flag, Read, switch, valued},
    say,
};

const USAGE: &str = "blaze metadata [--format-version 1] [--no-deps] [--filter-platform <TRIPLE>] \
                     [--features <FEATURES>] [--all-features] [--no-default-features] \
                     [--manifest-path <PATH>]";

const FLAGS: [Flag; 7] = [
    valued(&["--format-version"]),
    switch(&["--no-deps"]),
    valued(&["--filter-platform"]),
    valued(&["-F", "--features"]),
    switch(&["--all-features"]),
    switch(&["--no-default-features"]),
    valued(&["--manifest-path"]),
];

/// What `--format-version` says: nothing (cargo warns and prints version 1), `1`, or a
/// version there is none of.
#[derive(Debug, PartialEq, Eq)]
enum Version {
    Unsaid,
    One,
    Other(String),
}

fn version(read: &Read) -> Version {
    match read.value("--format-version") {
        None => Version::Unsaid,
        Some(it) if it == "1" => Version::One,
        Some(other) => Version::Other(other),
    }
}

/// The JSON `cargo metadata` prints for `read`, run in `cwd` with the variables `env`.
fn printed(read: &Read, cwd: &Path, env: Vec<(String, String)>) -> anyhow::Result<String> {
    let manifest_path = read.value("--manifest-path").map(PathBuf::from);
    let platforms = read.values("--filter-platform");
    let cfg;
    let platform = match &platforms[..] {
        [] => None,
        [triple] => {
            cfg = target_cfgs(triple, &CfgArgs::default()).map_err(anyhow::Error::msg)?;
            Some(blaze_metadata::TargetPlatform { triple, cfg: &cfg })
        }
        [_, _, ..] => anyhow::bail!(
            "blaze metadata filters by one platform: give one `--filter-platform`, not {}",
            platforms.len()
        ),
    };
    let config = read.values("--config");
    let features = read.features();
    let ignore_pin_for = blaze_units::query::env_var(&env, "BLAZE_IGNORE_PIN_FOR")
        .filter(|it| !it.is_empty())
        .map(str::to_owned);
    let lock = blaze_units::lock::Inputs {
        ask: own::lock_ask(read, &env).map_err(anyhow::Error::msg)?,
        running: own::running(),
        ignore_pin_for: ignore_pin_for.as_deref(),
        foreign: false,
    };
    let asked = blaze_metadata::Asked {
        cwd,
        env,
        manifest_path: manifest_path.as_deref(),
        config: &config,
        features: &features,
        all_features: read.has("--all-features"),
        no_default_features: read.has("--no-default-features"),
        build: None,
        platform,
        offline: read.has("--offline") || read.has("--frozen"),
        rustc_version: Some(crate::toolchain::ours_release()),
        lock,
    };
    match read.has("--no-deps") {
        true => blaze_metadata::metadata_no_deps(&asked),
        false => blaze_metadata::metadata(&asked),
    }
}

/// `blaze metadata WORDS`: its exit code.
pub fn main(words: Vec<OsString>, color: bool) -> i32 {
    let read = match own::read_or_refuse(words, &FLAGS, USAGE, 0, color) {
        Ok(read) => read,
        Err(code) => return code,
    };
    match version(&read) {
        Version::One => {}
        Version::Unsaid => {
            let what = "please specify `--format-version` flag explicitly to avoid compatibility problems";
            eprint!("{}", say::warning(what, color));
        }
        Version::Other(other) => {
            let what = format!(
                "invalid value '{other}' for '--format-version <VERSION>'\n  [possible values: 1]"
            );
            eprint!("{}", say::error(&what, color));
            return Exit::Usage.code();
        }
    }
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(error) => {
            let what = format!("blaze metadata: the current directory: {error}");
            eprint!("{}", say::error(&what, color));
            return Exit::Failed.code();
        }
    };
    match printed(&read, &cwd, own::env()) {
        Ok(json) => {
            println!("{json}");
            0
        }
        Err(error) => own::failed(&error, &cwd, color),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(words: &[&str]) -> Read {
        own::read(words.iter().map(OsString::from).collect(), &FLAGS).unwrap()
    }

    /// A home with no cargo configuration, so the machine's own decides nothing.
    fn env(home: &Path) -> Vec<(String, String)> {
        let home = home.to_str().unwrap().to_owned();
        vec![("HOME".to_owned(), home.clone()), ("CARGO_HOME".to_owned(), format!("{home}/.cargo"))]
    }

    #[test]
    fn the_format_version_is_one_or_refused() {
        assert_eq!(version(&read(&[])), Version::Unsaid);
        assert_eq!(version(&read(&["--format-version", "1"])), Version::One);
        assert_eq!(version(&read(&["--format-version=2"])), Version::Other("2".to_owned()));
    }

    #[test]
    fn no_deps_prints_the_workspaces_members_and_resolves_nothing() {
        crate::release::for_tests();
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let root = cwd.join("app");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nserde = \"1\"\n",
        )
        .unwrap();
        std::fs::write(root.join("src/lib.rs"), "").unwrap();
        let one = read(&["--format-version", "1", "--no-deps"]);
        let json = printed(&one, &root, env(&cwd)).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["version"], 1);
        assert_eq!(value["packages"][0]["name"], "app");
        assert_eq!(value["packages"][0]["dependencies"][0]["name"], "serde");
        assert_eq!(value["workspace_root"], root.to_str().unwrap());
        assert!(value["resolve"].is_null(), "`--no-deps` resolves nothing: {json}");
        assert!(!root.join("Cargo.lock").exists(), "`--no-deps` writes no lock file");

        let two = read(&["--filter-platform", "a", "--filter-platform", "b"]);
        let refused = printed(&two, &root, env(&cwd)).unwrap_err();
        assert!(format!("{refused:#}").contains("one `--filter-platform`"), "{refused:#}");
    }
}
