//! Where a command runs and what of the machine it sends with it: the checkout's root, the
//! config files cargo reads outside it, the environment and whether diagnostics are colored.
//! Nothing here reads a manifest: the checkout is found with `std::fs::metadata` alone, and the
//! daemon's load is the one load.

use std::{
    ffi::OsString,
    io,
    path::{Path, PathBuf},
};

use blaze_api::{Cargo, Targets};
use blaze_manifest::paths::normalize_path;

use super::args::{Command, Invocation};

/// Where a command runs.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Place {
    /// The directory the command ran in, absolute.
    pub cwd: PathBuf,
    /// The root manifest, as cargo finds it.
    pub manifest: PathBuf,
    /// The checkout: what the view is of.
    pub root: PathBuf,
}

/// cargo's `root_manifest` (`util/command_prelude.rs`) and `find_root_manifest_for_wd`
/// (`util/important_paths.rs`), with their errors. `script` is `-Zscript`, under which a `.rs`
/// file or one without an extension is a manifest too.
pub fn manifest(cwd: &Path, manifest_path: Option<&str>, script: bool) -> Result<PathBuf, String> {
    if let Some(typed) = manifest_path {
        let path = normalize_path(&cwd.join(typed));
        let embedded = is_embedded(&path);
        if !path.ends_with("Cargo.toml") && !embedded {
            return Err("the manifest-path must be a path to a Cargo.toml file".to_owned());
        }
        let Ok(found) = std::fs::metadata(&path) else {
            return Err(format!("manifest path `{typed}` does not exist"));
        };
        if found.is_dir() {
            return Err(format!("manifest path `{typed}` is a directory but expected a file"));
        }
        if embedded && !script {
            return Err(format!("embedded manifest `{}` requires `-Zscript`", path.display()));
        }
        return Ok(path);
    }
    // One stat per directory on the way up; `cargo.toml` is looked for only once nothing was found.
    if let Some(dir) = cwd.ancestors().find(|dir| std::fs::metadata(dir.join("Cargo.toml")).is_ok())
    {
        return Ok(dir.join("Cargo.toml"));
    }
    let lowercase = cwd.ancestors().any(|dir| std::fs::metadata(dir.join("cargo.toml")).is_ok());
    let said =
        format!("could not find `Cargo.toml` in `{}` or any parent directory", cwd.display());
    match lowercase {
        true => Err(format!("{said}, but found cargo.toml please try to rename it to Cargo.toml")),
        false => Err(said),
    }
}

/// cargo's `toml::is_embedded`: a single-file package.
fn is_embedded(path: &Path) -> bool {
    let ext = path.extension();
    (ext.is_none() || ext.is_some_and(|it| it == "rs"))
        && std::fs::metadata(path).is_ok_and(|it| it.is_file())
}

/// Whether `-Zscript` is among the arguments.
pub fn script(args: &[String]) -> bool {
    let mut words = args.iter();
    while let Some(word) = words.next() {
        let flag = match word.strip_prefix("-Z") {
            Some("") => words.next().map(String::as_str),
            Some(rest) => Some(rest),
            None => None,
        };
        if flag == Some("script") {
            return true;
        }
    }
    false
}

/// The checkout of `manifest` ([`blaze_api::client::checkout`] of its directory).
pub fn root(manifest: &Path) -> PathBuf {
    blaze_api::client::checkout(manifest.parent().unwrap_or(manifest))
}

/// Where a command run in `cwd` runs, every path of it canonical: symlinks resolved once,
/// here, at the boundary (`/tmp` is `/private/tmp` on macOS), so the view, the watch of the
/// checkout (whose file system events name canonical paths) and every path the daemon's load
/// derives from them agree on one form of each directory.
pub fn find(cwd: &Path, manifest_path: Option<&str>, script: bool) -> Result<Place, String> {
    let cwd = std::fs::canonicalize(cwd).map_err(|error| format!("resolving the current directory `{}`: {error}", cwd.display()))?;
    let found = manifest(&cwd, manifest_path, script)?;
    let manifest = match (found.parent(), found.file_name()) {
        (Some(dir), Some(name)) => std::fs::canonicalize(dir)
            .map_err(|error| format!("resolving the directory of the manifest `{}`: {error}", found.display()))?
            .join(name),
        _ => found,
    };
    let root = root(&manifest);
    Ok(Place { cwd, manifest, root })
}

/// The files of cargo's configuration outside the checkout, which the view does not have:
/// `.cargo/config` and `.cargo/config.toml` of each directory from `cwd` up that is not in
/// the checkout, `$CARGO_HOME`'s, and each `--config` that names a file outside it
/// (`GlobalContext::walk_tree`, `cli_args_as_table`). Never `credentials.toml`.
///
/// A directory without `.cargo` costs one stat; a file is read without one, its absence being the
/// read's `NotFound`.
pub fn config_files(
    place: &Place,
    configs: &[String],
    cargo_home: Option<&Path>,
) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    let mut add = |path: PathBuf| -> Result<(), String> {
        if path.starts_with(&place.root) {
            return Ok(());
        }
        if files.iter().any(|(it, _)| Path::new(it) == path) {
            return Ok(());
        }
        match std::fs::read(&path) {
            Ok(bytes) => files.push((utf8(&path)?, bytes)),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound
                        | io::ErrorKind::IsADirectory
                        | io::ErrorKind::NotADirectory
                ) => {}
            Err(error) => return Err(format!("reading {}: {error}", path.display())),
        }
        Ok(())
    };
    for dir in place.cwd.ancestors().filter(|it| !it.starts_with(&place.root)) {
        let cargo = dir.join(".cargo");
        if std::fs::metadata(&cargo).is_ok_and(|it| it.is_dir()) {
            add(cargo.join("config"))?;
            add(cargo.join("config.toml"))?;
        }
    }
    if let Some(home) = cargo_home {
        add(home.join("config"))?;
        add(home.join("config.toml"))?;
    }
    for config in configs.iter().filter(|it| !it.is_empty()) {
        add(normalize_path(&place.cwd.join(config)))?;
    }
    Ok(files)
}

/// The files clippy's configuration search reads above the checkout, which the view does not
/// have: `.clippy.toml` and `clippy.toml` of each directory above the root.
pub fn clippy_files(place: &Place) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut files = Vec::new();
    for dir in place.root.ancestors().skip(1) {
        for name in [".clippy.toml", "clippy.toml"] {
            let path = dir.join(name);
            match std::fs::read(&path) {
                Ok(bytes) => files.push((utf8(&path)?, bytes)),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::NotFound
                            | io::ErrorKind::IsADirectory
                            | io::ErrorKind::NotADirectory
                    ) => {}
                Err(error) => return Err(format!("reading {}: {error}", path.display())),
            }
        }
    }
    Ok(files)
}

/// A path as the API carries it.
pub fn utf8(path: &Path) -> Result<String, String> {
    path.to_str().map(api_path).ok_or_else(|| {
        format!(
            "the path {} is not UTF-8, which a request to the daemon can't carry",
            path.display()
        )
    })
}

/// A path as the daemon names it: rooted at `/` with `/` between its parts. On Windows the
/// drive is the first part (`C:\a` is `/C:/a`), so the daemon's one-root path model holds.
#[cfg(windows)]
pub fn api_path(text: &str) -> String {
    let text = text.strip_prefix(r"\\?\").unwrap_or(text).replace('\\', "/");
    let bytes = text.as_bytes();
    match bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        true => format!("/{}{}", char::from(bytes[0].to_ascii_uppercase()), &text[1..]),
        false => text,
    }
}

#[cfg(not(windows))]
pub fn api_path(text: &str) -> String {
    text.to_owned()
}

/// A path the daemon named, as this system opens it: [`api_path`]'s inverse.
#[cfg(windows)]
pub fn host_path(text: &str) -> PathBuf {
    let bytes = text.as_bytes();
    let drive = bytes.len() >= 3 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b':';
    PathBuf::from(text[usize::from(drive)..].replace('/', "\\"))
}

#[cfg(not(windows))]
pub fn host_path(text: &str) -> PathBuf {
    PathBuf::from(text)
}

/// The command's environment, sorted by name. A variable that is not UTF-8 is refused by its
/// name alone: its value is never shown.
pub fn env(
    vars: impl Iterator<Item = (OsString, OsString)>,
) -> Result<Vec<(String, String)>, String> {
    let mut env = Vec::with_capacity(vars.size_hint().0);
    let refused = |named: &str| {
        format!(
            "the environment variable `{named}` is not UTF-8, which a request to the daemon can't carry"
        )
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

/// Whether what cargo writes to stderr is colored: `--color` (the last one), else
/// `CARGO_TERM_COLOR`, else `auto`, which is anstream's choice for a stream that is a
/// `terminal` or not (`NO_COLOR`, `CLICOLOR_FORCE`, `CLICOLOR`, `TERM`, `CI`).
pub fn color(
    flag: Option<&str>,
    var: impl Fn(&str) -> Option<String>,
    terminal: bool,
) -> Result<bool, String> {
    let when = match flag {
        Some(flag) => Some(flag.to_owned()),
        None => var("CARGO_TERM_COLOR"),
    };
    match when.as_deref() {
        Some("always") => Ok(true),
        Some("never") => Ok(false),
        Some("auto") | None => Ok(auto(&var, terminal)),
        // cargo's `ColorChoice::from_str`, of `--color` (which clap took in any case) or
        // `term.color`.
        Some(other) => {
            Err(format!("argument for --color must be auto, always, or never, but found `{other}`"))
        }
    }
}

/// anstream's `AutoStream::choice`.
fn auto(var: &impl Fn(&str) -> Option<String>, terminal: bool) -> bool {
    let clicolor = var("CLICOLOR").map(|it| it != "0");
    if var("NO_COLOR").is_some_and(|it| !it.is_empty()) {
        return false;
    }
    if var("CLICOLOR_FORCE").is_some_and(|it| it != "0") {
        return true;
    }
    if clicolor == Some(false) {
        return false;
    }
    let term = cfg!(windows) || var("TERM").is_some_and(|it| it != "dumb");
    terminal && (term || clicolor == Some(true) || var("CI").is_some())
}

/// The columns diagnostics are rendered in: cargo's `Shell::err_width().diagnostic_terminal_width()`.
/// cargo's own test override `__CARGO_TEST_TTY_WIDTH_DO_NOT_USE_THIS` first, then the width of
/// the terminal stderr is (`columns`, asked only when stderr is a `terminal`), else `None`, which
/// is rustc's own default. A terminal whose width is unknown is `None` too, as cargo's `NoTty`.
pub fn width(
    var: impl Fn(&str) -> Option<String>,
    terminal: bool,
    columns: impl FnOnce() -> Option<u16>,
) -> Result<Option<u32>, String> {
    const TEST: &str = "__CARGO_TEST_TTY_WIDTH_DO_NOT_USE_THIS";
    if let Some(width) = var(TEST) {
        return match width.parse::<u32>() {
            Ok(width) => Ok(Some(width)),
            Err(_) => Err(format!("`{TEST}` is `{width}`, which is no number of columns")),
        };
    }
    Ok(terminal.then(columns).flatten().map(u32::from))
}

/// The columns of the terminal stderr is, as the system says.
#[cfg(unix)]
pub fn stderr_columns() -> Option<u16> {
    crate::raw::system::stderr_columns()
}

/// The columns of the terminal stderr is: blaze's system calls ask it of Unix's `TIOCGWINSZ`
/// only, so on Windows rustc renders in its own default width.
#[cfg(not(unix))]
pub fn stderr_columns() -> Option<u16> {
    None
}

/// cargo's `Verbosity` as a count: how many `-v`, 0 under `-q`.
pub fn verbose(asked: &Invocation) -> u8 {
    match asked.flags.quiet {
        true => 0,
        false => u8::try_from(asked.flags.verbose).unwrap_or(u8::MAX),
    }
}

/// What the command sends of itself (`Targets::cargo`). `width` is [`width`]'s.
pub fn cargo(
    asked: &Invocation,
    place: &Place,
    env: Vec<(String, String)>,
    files: Vec<(String, Vec<u8>)>,
    color: bool,
    width: Option<u32>,
) -> Result<Cargo, String> {
    let mut args = asked.args.clone();
    // `check --fix` makes the suggestions of every target when none is named, as `cargo fix`.
    if asked.command() == Command::Check && asked.flags.fix && !asked.flags.targeted {
        args.push("--all-targets".to_owned());
    }
    Ok(Cargo {
        command: asked.command().name().to_owned(),
        args,
        root: utf8(&place.root)?,
        cwd: utf8(&place.cwd)?,
        env,
        files,
        color,
        width,
        verbose: verbose(asked),
        refused: asked.flags.refused.unwrap_or(Some(crate::REFUSED_SHOWN)),
    })
}

/// What blaze refuses of a command line cargo would take.
#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    /// More than one `--target`: a request to the daemon builds for one.
    Targets(Vec<String>),
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::Targets(given) => {
                write!(
                    f,
                    "blaze builds for one `--target` a command, and was given {}:",
                    given.len()
                )?;
                given.iter().try_for_each(|it| write!(f, " `{it}`"))
            }
        }
    }
}

impl std::error::Error for Refused {}

/// The six fields of `Targets` the flags say, and the whole command line.
pub fn targets(asked: &Invocation, cargo: Cargo) -> Result<Targets, Refused> {
    let flags = &asked.flags;
    let target = match &flags.targets[..] {
        [] => None,
        [one] => Some(one.clone()),
        // alloc: error path, the refusal.
        several => return Err(Refused::Targets(several.to_vec())),
    };
    Ok(Targets {
        packages: flags.packages.clone(),
        profile: asked.profile(),
        target,
        features: flags.features.clone(),
        all_features: flags.all_features,
        no_default_features: flags.no_default_features,
        cargo: Some(cargo),
    })
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod refusals {
    use super::*;

    #[test]
    fn a_second_target_is_refused_not_dropped() {
        let place = Place { cwd: "/w".into(), manifest: "/w/Cargo.toml".into(), root: "/w".into() };
        let made =
            |asked: &Invocation| cargo(asked, &place, Vec::new(), Vec::new(), false, None).unwrap();
        let mut asked = Invocation::default();
        asked.flags.targets =
            vec!["aarch64-apple-darwin".to_owned(), "x86_64-unknown-linux-gnu".to_owned()];
        let refused = targets(&asked, made(&asked)).unwrap_err();
        assert_eq!(refused, Refused::Targets(asked.flags.targets.clone()));
        assert_eq!(
            refused.to_string(),
            "blaze builds for one `--target` a command, and was given 2: `aarch64-apple-darwin` `x86_64-unknown-linux-gnu`"
        );
        asked.flags.targets.pop();
        assert_eq!(
            targets(&asked, made(&asked)).unwrap().target.as_deref(),
            Some("aarch64-apple-darwin")
        );
        asked.flags.targets.clear();
        assert_eq!(targets(&asked, made(&asked)).unwrap().target, None);
    }
}
