//! Where a command runs and what of the machine it sends with it: the checkout's root, the
//! config files cargo reads outside it and the environment. Nothing here reads a manifest: the
//! checkout is found with `std::fs::metadata` alone, and the daemon's load is the one load.
//!
//! These are the rules of blaze-cargo's `cli/place.rs`, which the CLI sends its command with;
//! a request made here names the same view and carries the same files as the CLI's from the
//! same directory. They are here a second time because this crate links none of blaze-cargo:
//! the CLI's are to become these, once its place moves onto this crate.

use std::{
    ffi::OsString,
    io,
    path::{Component, Path, PathBuf},
};

/// Where a command runs.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Place {
    /// The directory the command ran in, absolute, symlinks resolved.
    pub cwd: PathBuf,
    /// The root manifest, as cargo finds it.
    pub manifest: PathBuf,
    /// The checkout: what the view is of ([`blaze_api::client::checkout`]).
    pub root: PathBuf,
}

/// cargo's `normalize_path`: `.` dropped and `..` taken against what is before it, by the
/// path's words alone.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            part => out.push(part),
        }
    }
    out
}

/// The manifest of a command run in `cwd`: the one `manifest_path` names, else the
/// `Cargo.toml` of the nearest directory from `cwd` up that has one, with cargo's errors.
pub fn manifest(cwd: &Path, manifest_path: Option<&str>) -> Result<PathBuf, String> {
    if let Some(typed) = manifest_path {
        let path = normalize(&cwd.join(typed));
        if !path.ends_with("Cargo.toml") {
            return Err("the manifest-path must be a path to a Cargo.toml file".to_owned());
        }
        let Ok(found) = std::fs::metadata(&path) else {
            return Err(format!("manifest path `{typed}` does not exist"));
        };
        if found.is_dir() {
            return Err(format!("manifest path `{typed}` is a directory but expected a file"));
        }
        return Ok(path);
    }
    if let Some(dir) = cwd.ancestors().find(|dir| std::fs::metadata(dir.join("Cargo.toml")).is_ok())
    {
        return Ok(dir.join("Cargo.toml"));
    }
    Err(format!("could not find `Cargo.toml` in `{}` or any parent directory", cwd.display()))
}

/// Where a command run in `cwd` runs, every path of it canonical: symlinks resolved once,
/// here, so the view and every path the daemon derives from it agree on one form.
pub fn find(cwd: &Path, manifest_path: Option<&str>) -> Result<Place, String> {
    let cwd = std::fs::canonicalize(cwd)
        .map_err(|error| format!("resolving the directory `{}`: {error}", cwd.display()))?;
    let found = manifest(&cwd, manifest_path)?;
    let manifest = match (found.parent(), found.file_name()) {
        (Some(dir), Some(name)) => std::fs::canonicalize(dir)
            .map_err(|error| {
                format!("resolving the directory of the manifest `{}`: {error}", found.display())
            })?
            .join(name),
        _ => found,
    };
    let root = blaze_api::client::checkout(manifest.parent().unwrap_or(&manifest));
    Ok(Place { cwd, manifest, root })
}

/// A path as the API carries it.
pub fn utf8(path: &Path) -> Result<String, String> {
    path.to_str().map(str::to_owned).ok_or_else(|| {
        format!("the path {} is not UTF-8, which a request to the daemon can't carry", path.display())
    })
}

/// Reads `path` onto `files` unless it is not there; a file already among them is not read
/// again.
fn add(files: &mut Vec<(String, Vec<u8>)>, path: PathBuf) -> Result<(), String> {
    if files.iter().any(|(it, _)| Path::new(it) == path) {
        return Ok(());
    }
    match std::fs::read(&path) {
        Ok(bytes) => files.push((utf8(&path)?, bytes)),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::IsADirectory | io::ErrorKind::NotADirectory
            ) => {}
        Err(error) => return Err(format!("reading {}: {error}", path.display())),
    }
    Ok(())
}

/// The files of cargo's configuration outside the checkout, which the view does not have:
/// `.cargo/config` and `.cargo/config.toml` of each directory from `cwd` up that is not in the
/// checkout, and `cargo_home`'s. Never `credentials.toml`.
pub fn config_files(
    place: &Place,
    cargo_home: Option<&Path>,
) -> Result<Vec<(String, Vec<u8>)>, String> {
    // alloc: the request's files, once a command.
    let mut files = Vec::new();
    for dir in place.cwd.ancestors().filter(|it| !it.starts_with(&place.root)) {
        let cargo = dir.join(".cargo");
        if std::fs::metadata(&cargo).is_ok_and(|it| it.is_dir()) {
            add(&mut files, cargo.join("config"))?;
            add(&mut files, cargo.join("config.toml"))?;
        }
    }
    if let Some(home) = cargo_home.filter(|it| !it.starts_with(&place.root)) {
        add(&mut files, home.join("config"))?;
        add(&mut files, home.join("config.toml"))?;
    }
    Ok(files)
}

/// The files clippy's configuration search reads above the checkout: `.clippy.toml` and
/// `clippy.toml` of each directory above the root. A check sends them.
pub fn clippy_files(place: &Place) -> Result<Vec<(String, Vec<u8>)>, String> {
    // alloc: the request's files, once a command.
    let mut files = Vec::new();
    for dir in place.root.ancestors().skip(1) {
        for name in [".clippy.toml", "clippy.toml"] {
            add(&mut files, dir.join(name))?;
        }
    }
    Ok(files)
}

/// An environment, sorted by name. A variable that is not UTF-8 is refused by its name alone:
/// its value is never shown.
pub fn env(vars: impl Iterator<Item = (OsString, OsString)>) -> Result<Vec<(String, String)>, String> {
    // alloc: the request's environment, once a command.
    let mut env = Vec::with_capacity(vars.size_hint().0);
    let refused = |named: &str| {
        format!("the environment variable `{named}` is not UTF-8, which a request to the daemon can't carry")
    };
    for (name, value) in vars {
        match (name.into_string(), value.into_string()) {
            (Ok(name), Ok(value)) => env.push((name, value)),
            (Ok(name), Err(_)) => return Err(refused(&name)),
            (Err(name), _) => return Err(refused(&name.to_string_lossy())),
        }
    }
    env.sort_unstable();
    Ok(env)
}

/// The environment a command is sent with: `given` when the caller named one, sorted, else
/// this process's, read here.
pub fn env_of(given: Option<&[(String, String)]>) -> Result<Vec<(String, String)>, String> {
    match given {
        Some(given) => {
            // clone: the request owns its environment.
            let mut env = given.to_vec();
            env.sort_unstable();
            Ok(env)
        }
        None => env(std::env::vars_os()),
    }
}

/// cargo's home in `env`: `CARGO_HOME` (against `cwd` when relative), else `.cargo` in `HOME`.
pub fn cargo_home(env: &[(String, String)], cwd: &Path) -> Option<PathBuf> {
    let set = |name: &str| {
        env.iter().find(|(it, value)| it == name && !value.is_empty()).map(|(_, value)| value)
    };
    match set("CARGO_HOME") {
        Some(home) => Some(cwd.join(home)),
        None => set("HOME").map(|home| Path::new(home).join(".cargo")),
    }
}

/// The columns of the terminal stderr is; `None` when it is none.
#[cfg(unix)]
pub fn stderr_columns() -> Option<u16> {
    crate::sys::stderr_columns()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_is_normalized_by_its_words() {
        assert_eq!(normalize(Path::new("/w/a/../b/./Cargo.toml")), Path::new("/w/b/Cargo.toml"));
    }

    #[test]
    fn cargo_home_is_the_environments() {
        let env = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
            pairs.iter().map(|(name, value)| ((*name).to_owned(), (*value).to_owned())).collect()
        };
        let cwd = Path::new("/w");
        assert_eq!(cargo_home(&env(&[("HOME", "/u")]), cwd), Some(PathBuf::from("/u/.cargo")));
        let named = env(&[("CARGO_HOME", "/c"), ("HOME", "/u")]);
        assert_eq!(cargo_home(&named, cwd), Some(PathBuf::from("/c")));
        assert_eq!(cargo_home(&env(&[("CARGO_HOME", "c")]), cwd), Some(PathBuf::from("/w/c")));
        assert_eq!(cargo_home(&env(&[("CARGO_HOME", "")]), cwd), None);
    }
}
