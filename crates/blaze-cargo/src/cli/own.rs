//! What the commands blaze answers in this process from the workspace's files share (`new` and
//! `init` in [`super::new`], `tree` in [`super::tree`], `metadata` in [`super::metadata`]): a
//! command line read as cargo's clap reads it (`--flag value`, `--flag=value`, `-Fvalue`, and
//! the flags every cargo command takes), a refusal said as cargo says it, and what the command
//! line and the environment say of the lock files. None of them asks the daemon anything.

use std::{ffi::OsString, path::Path};

use blaze_core::exit::Exit;
use blaze_units::lock::{self, LockAsk};

use super::say;

/// A flag a command takes, by each of its names, and whether it takes a value.
pub struct Flag {
    pub names: &'static [&'static str],
    pub valued: bool,
}

pub const fn switch(names: &'static [&'static str]) -> Flag {
    Flag { names, valued: false }
}

pub const fn valued(names: &'static [&'static str]) -> Flag {
    Flag { names, valued: true }
}

/// The flags of every cargo command, and blaze's `--no-locked` (docs/cargo.md, "Lock files").
const GLOBAL: &[Flag] = &[
    switch(&["-v", "--verbose", "-vv"]),
    switch(&["-q", "--quiet"]),
    valued(&["--color"]),
    valued(&["--config"]),
    valued(&["-Z"]),
    switch(&["--frozen"]),
    switch(&["--locked"]),
    switch(&["--no-locked"]),
    switch(&["--offline"]),
    switch(&["-h", "--help"]),
];

impl Flag {
    /// The name a flag is read back by: its long one.
    fn key(&self) -> &'static str {
        self.names.iter().find(|it| it.starts_with("--")).unwrap_or(&self.names[0])
    }
}

/// The flags read, by the long name of each, and the other words.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Read {
    flags: Vec<(&'static str, Option<String>)>,
    /// The words that are no flags, and every word after `--`.
    pub positional: Vec<String>,
}

impl Read {
    /// Whether the flag of which `name` is the long name was given.
    pub fn has(&self, name: &str) -> bool {
        self.flags.iter().any(|(it, _)| *it == name)
    }

    /// The values of the flag of which `name` is the long name, in the order given.
    pub fn values(&self, name: &str) -> Vec<String> {
        let given = self.flags.iter().filter(|(it, _)| *it == name);
        given.filter_map(|(_, value)| value.clone()).collect() // clone: a command line's values, once
    }

    /// The last value of the flag of which `name` is the long name.
    pub fn value(&self, name: &str) -> Option<String> {
        self.values(name).pop()
    }

    /// `--features`' values split as cargo splits them, by commas and spaces.
    pub fn features(&self) -> Vec<String> {
        let values = self.values("--features");
        let split = values.iter().flat_map(|it| it.split([',', ' ']));
        split.filter(|it| !it.is_empty()).map(str::to_owned).collect()
    }
}

/// The words after a command read against `flags` and cargo's global ones, or the line cargo
/// refuses the command line with. A word that is not UTF-8 is refused, naming it.
pub fn read(words: Vec<OsString>, flags: &[Flag]) -> Result<Read, String> {
    let mut typed = Vec::with_capacity(words.len()); // alloc: the command line, once
    for word in words {
        match word.into_string() {
            Ok(word) => typed.push(word),
            Err(word) => {
                return Err(format!("the argument {} is not UTF-8", word.to_string_lossy()));
            }
        }
    }
    let known = |name: &str| flags.iter().chain(GLOBAL).find(|it| it.names.contains(&name));
    let mut read = Read::default();
    let mut words = typed.into_iter();
    while let Some(word) = words.next() {
        if word == "--" {
            read.positional.extend(words);
            break;
        }
        if !word.starts_with('-') || word == "-" {
            read.positional.push(word);
            continue;
        }
        let (name, joined) = match word.split_once('=') {
            Some((name, value)) if name.starts_with("--") => (name, Some(value.to_owned())),
            _ if known(&word).is_some() => (word.as_str(), None),
            // `-Fa,b`, `-Zflag`.
            _ if !word.starts_with("--")
                && word.len() > 2
                && word.get(..2).and_then(known).is_some_and(|it| it.valued) =>
            {
                (&word[..2], Some(word[2..].to_owned()))
            }
            _ => (word.as_str(), None),
        };
        let Some(flag) = known(name) else {
            return Err(format!("unexpected argument '{word}' found"));
        };
        let value = match (flag.valued, joined) {
            (true, Some(value)) => Some(value),
            (true, None) => match words.next() {
                Some(value) => Some(value),
                None => {
                    return Err(format!(
                        "a value is required for '{name} <VALUE>' but none was supplied"
                    ));
                }
            },
            (false, Some(_)) => {
                return Err(format!("unexpected value for '{name}': it takes none"));
            }
            (false, None) => None,
        };
        read.flags.push((flag.key(), value));
    }
    Ok(read)
}

/// [`read`], with what cargo does about it: `--help` prints `usage` and ends with 0, a command
/// line it refuses or a word past the `positionals` it takes is said with the usage and ends
/// with [`Exit::Usage`]. `Err` is the exit code.
pub fn read_or_refuse(
    words: Vec<OsString>,
    flags: &[Flag],
    usage: &str,
    positionals: usize,
    color: bool,
) -> Result<Read, i32> {
    let refused = |why: &str| {
        eprint!("{}", say::error(why, color));
        eprintln!("\nUsage: {usage}\n\nFor more information, try '--help'.");
        Exit::Usage.code()
    };
    match read(words, flags) {
        Ok(read) if read.has("--help") => {
            println!("Usage: {usage}");
            Err(0)
        }
        Ok(read) => match read.positional.get(positionals) {
            Some(word) => Err(refused(&format!("unexpected argument '{word}' found"))),
            None => Ok(read),
        },
        Err(why) => Err(refused(&why)),
    }
}

/// A command that failed with `error`, said as cargo says it (a manifest's diagnostic, or the
/// error with each cause under `Caused by:`): [`Exit::Failed`]'s code.
pub fn failed(error: &anyhow::Error, cwd: &Path, color: bool) -> i32 {
    eprint!("{}", say::error(&blaze_manifest::diagnostic::rendered(error, cwd), color));
    Exit::Failed.code()
}

/// The process's variables cargo reads: its variables are UTF-8 (`std::env::var`).
pub fn env() -> Vec<(String, String)> {
    std::env::vars_os()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
        .collect()
}

/// What the command line and the environment `env` say of the lock files, as a build's load
/// reads them (`blaze_units::load`'s `configure`): `--locked`, `--frozen`, `--no-locked`, `CI`
/// and `BLAZE_LOCK`.
pub fn lock_ask(read: &Read, env: &[(String, String)]) -> Result<LockAsk, String> {
    let var = |name: &str| env.iter().find(|(it, _)| it == name).map(|(_, value)| value.as_str());
    let flag = match () {
        _ if read.has("--frozen") => Some("--frozen"),
        _ if read.has("--locked") => Some("--locked"),
        _ => None,
    };
    Ok(LockAsk {
        flag,
        no_locked: read.has("--no-locked"),
        ci: lock::ci_says(var("CI"), var("TF_BUILD")),
        env_writes: lock::env_writes(var(lock::LOCK_ENV))?,
    })
}

/// The release of the blaze that runs, as the lock policy takes it: none for a build of the
/// tree (`blaze_units::lock::is_version`).
pub fn running() -> Option<&'static str> {
    Some(crate::release::get().version).filter(|it| lock::is_version(it))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(text: &str) -> Vec<OsString> {
        text.split_whitespace().map(OsString::from).collect()
    }

    #[test]
    fn both_spellings_and_cargos_own_flags() {
        let flags = [valued(&["-F", "--features"]), switch(&["--no-deps"])];
        let it =
            read(os("--no-deps -Fa,b --features=c --features d -q --config x=1"), &flags).unwrap();
        assert!(it.has("--no-deps") && it.has("--quiet"));
        assert_eq!(it.features(), ["a", "b", "c", "d"]);
        assert_eq!(it.values("--config"), ["x=1"]);
        assert!(read(os("-vv"), &flags).unwrap().has("--verbose"));
        let it = read(os("a -- b --c"), &flags).unwrap();
        assert_eq!(it.positional, ["a", "b", "--c"]);
    }

    #[test]
    fn what_cargo_refuses() {
        assert!(read(os("--frobnicate"), &[]).unwrap_err().contains("'--frobnicate'"));
        assert!(read(os("--features"), &[valued(&["--features"])]).unwrap_err().contains("value"));
        assert!(read(os("--quiet=yes"), &[]).is_err());
    }

    #[test]
    fn the_lock_ask_is_the_flags_and_the_environments() {
        let env = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
            pairs.iter().map(|&(name, value)| (name.to_owned(), value.to_owned())).collect()
        };
        let typed = read(os("--frozen --locked"), &[]).unwrap();
        let ask = lock_ask(&typed, &env(&[("CI", "true")])).unwrap();
        assert_eq!(
            ask,
            LockAsk { flag: Some("--frozen"), no_locked: false, ci: true, env_writes: false }
        );
        let typed = read(os("--no-locked"), &[]).unwrap();
        let ask = lock_ask(&typed, &env(&[("BLAZE_LOCK", "write")])).unwrap();
        assert_eq!(ask, LockAsk { flag: None, no_locked: true, ci: false, env_writes: true });
        let refused = lock_ask(&typed, &env(&[("BLAZE_LOCK", "yes")])).unwrap_err();
        assert!(refused.contains("BLAZE_LOCK"), "{refused}");
    }
}
