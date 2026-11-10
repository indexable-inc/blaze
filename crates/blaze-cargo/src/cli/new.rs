//! `blaze new <PATH>` and `blaze init [PATH]`: blaze-new's package (cargo 1.101's
//! `ops/cargo_new.rs`), with the command line, the configuration and the shell of cargo's
//! `commands/new.rs` and `commands/init.rs`. It runs in this process and asks nothing of the
//! daemon: it writes the manifest, the source file, the ignore file and the repository, and adds
//! the package to the `members` of a workspace it is made in.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

use anyhow::Context as _;
use blaze_core::exit::Exit;
use blaze_manifest::{Config, Context, Workspace};
use blaze_new::{NewOptions, Settings, Shell, VersionControl};

use super::{
    own::{self, Read, switch, valued},
    say,
};

const EDITIONS: [&str; 4] = ["2015", "2018", "2021", "2024"];

/// cargo's shell on standard error.
struct Said {
    quiet: bool,
    color: bool,
}

impl Shell for Said {
    fn status(&mut self, word: &str, rest: &str) {
        if !self.quiet {
            eprint!("{}", say::status(word, rest, self.color));
        }
    }

    fn warn(&mut self, text: &str) {
        if !self.quiet {
            eprint!("{}", say::warning(text, self.color));
        }
    }

    fn note(&mut self, text: &str) {
        if !self.quiet {
            let mut out = String::with_capacity(text.len() + 24); // alloc: a note, said once
            say::note_into(&mut out, text, self.color);
            eprint!("{out}");
        }
    }

    fn line(&mut self, text: &str) {
        eprintln!("{text}");
    }
}

/// What `new` or `init` was asked, its values checked as clap checks them.
#[derive(Debug)]
struct Asked {
    /// The package's directory as typed: `init`'s is `.` when none is.
    path: String,
    vcs: Option<VersionControl>,
    edition: Option<String>,
}

/// A value clap refuses: the flag as its usage names it, the value, and what it may be.
#[derive(Debug, PartialEq, Eq)]
struct Invalid {
    flag: &'static str,
    value: String,
    possible: &'static [&'static str],
}

/// `read`'s path, `--vcs` and `--edition`; `Ok(Err(..))` is a value clap refuses, `Err(())` a
/// `new` with no path.
fn asked(init: bool, read: &Read) -> Result<Result<Asked, Invalid>, ()> {
    let path = match (read.positional.first(), init) {
        (Some(path), _) => path.clone(), // clone: the path, once
        (None, true) => ".".to_owned(),
        (None, false) => return Err(()),
    };
    let vcs = match read.value("--vcs") {
        None => None,
        Some(it) => match VersionControl::parse(&it) {
            Some(vcs) => Some(vcs),
            None => {
                return Ok(Err(Invalid {
                    flag: "--vcs <VCS>",
                    value: it,
                    possible: &VersionControl::VALUES,
                }));
            }
        },
    };
    let edition = read.value("--edition");
    if let Some(it) = &edition
        && !EDITIONS.contains(&it.as_str())
    {
        // clone: the refused value, once
        return Ok(Err(Invalid { flag: "--edition <YEAR>", value: it.clone(), possible: &EDITIONS }));
    }
    Ok(Ok(Asked { path, vcs, edition }))
}

/// The package made in `cwd` with the variables `env`, what cargo says on its way said on
/// `shell`.
fn make(
    init: bool,
    read: &Read,
    asked: Asked,
    cwd: &Path,
    env: Vec<(String, String)>,
    shell: &mut impl Shell,
) -> anyhow::Result<()> {
    let name = if init { "init" } else { "new" };
    let home = env.iter().find(|(it, _)| it == "HOME").map(|(_, home)| PathBuf::from(home));
    let mut config = Config::load(cwd, env, &[])?;
    config.start(name, &read.values("--config"))?;
    let registry = match read.value("--registry") {
        Some(registry) => Some(registry),
        None => config.get_string("registry.default")?.map(|it| it.val),
    };
    let opts = NewOptions::new(
        asked.vcs,
        read.has("--bin"),
        read.has("--lib"),
        cwd.join(&asked.path),
        read.value("--name"),
        asked.edition,
        registry,
    )?;
    let configured = match config.get_string("cargo-new.vcs")? {
        None => None,
        Some(it) => Some(VersionControl::parse(&it.val).with_context(|| {
            format!(
                "error in {}: `cargo-new.vcs` expected one of `git`, `hg`, `pijul`, `fossil`, `none`, found `{}`",
                it.definition, it.val
            )
        })?),
    };
    let quiet = read.has("--quiet");
    let ctx = Context::new(config);
    // Why the new manifest's workspace does not read, and its warnings said: what cargo checks
    // after it writes the manifest.
    let warned = std::cell::RefCell::new(Vec::new()); // alloc: the workspace's warnings, once
    let check_workspace = |manifest: &Path| match Workspace::new(manifest, &ctx) {
        Ok(ws) => {
            if !quiet {
                // clone: a workspace's warnings, once per `new`
                warned.borrow_mut().extend(ws.workspace_warnings().iter().cloned());
            }
            None
        }
        Err(error) => Some(blaze_manifest::diagnostic::rendered(&error, cwd)),
    };
    let settings =
        Settings { cwd, home: home.as_deref(), vcs: configured, check_workspace: &check_workspace };
    let made = match init {
        true => blaze_new::init(&opts, &settings, shell).map(drop),
        false => blaze_new::new(&opts, &settings, shell),
    };
    for warning in warned.borrow().iter() {
        shell.warn(warning);
    }
    made
}

/// `blaze new WORDS`, or with `init` `blaze init WORDS`: its exit code.
pub fn main(init: bool, words: Vec<OsString>, color: bool) -> i32 {
    let name = if init { "init" } else { "new" };
    let flags = [
        valued(&["--vcs"]),
        switch(&["--bin"]),
        switch(&["--lib"]),
        valued(&["--edition"]),
        valued(&["--name"]),
        valued(&["--registry"]),
    ];
    let usage = format!("blaze {name} [OPTIONS] {}", if init { "[PATH]" } else { "<PATH>" });
    let read = match own::read_or_refuse(words, &flags, &usage, 1, color) {
        Ok(read) => read,
        Err(code) => return code,
    };
    let said_usage = || {
        eprintln!("\nUsage: {usage}\n\nFor more information, try '--help'.");
        Exit::Usage.code()
    };
    let asked = match asked(init, &read) {
        Ok(Ok(asked)) => asked,
        Ok(Err(invalid)) => {
            let what = format!(
                "invalid value '{}' for '{}'\n  [possible values: {}]",
                invalid.value,
                invalid.flag,
                invalid.possible.join(", ")
            );
            eprint!("{}", say::error(&what, color));
            if let Some(similar) = similar(&invalid.value, invalid.possible) {
                eprintln!("\n  tip: a similar value exists: '{similar}'");
            }
            return said_usage();
        }
        Err(()) => {
            let what = "the following required arguments were not provided:\n  <PATH>";
            eprint!("{}", say::error(what, color));
            return said_usage();
        }
    };
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(error) => {
            let what = format!("blaze {name}: the current directory: {error}");
            eprint!("{}", say::error(&what, color));
            return Exit::Failed.code();
        }
    };
    let mut said = Said { quiet: read.has("--quiet"), color };
    match make(init, &read, asked, &cwd, own::env(), &mut said) {
        Ok(()) => 0,
        Err(error) => own::failed(&error, &cwd, color),
    }
}

/// The possible value clap's tip names for `value`: the last of the most similar by Jaro
/// similarity, if more than 0.7.
fn similar<'a>(value: &str, possible: &[&'a str]) -> Option<&'a str> {
    let mut best: Option<(f64, &str)> = None;
    for &it in possible {
        let confidence = jaro(value, it);
        if confidence > 0.7 && best.is_none_or(|(most, _)| confidence >= most) {
            best = Some((confidence, it));
        }
    }
    best.map(|(_, it)| it)
}

/// strsim's `jaro`.
fn jaro(a: &str, b: &str) -> f64 {
    // alloc: two short words of a refused command line
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let window = (a.len().max(b.len()) / 2).saturating_sub(1);
    let mut b_used = vec![false; b.len()]; // alloc: a refused command line's, once
    let (mut matches, mut transpositions, mut b_consumed) = (0usize, 0usize, 0usize);
    for (i, ca) in a.iter().enumerate() {
        let from = i.saturating_sub(window);
        let to = (i + window + 1).min(b.len());
        if let Some(j) = (from..to).find(|&j| !b_used[j] && b[j] == *ca) {
            b_used[j] = true;
            matches += 1;
            if j < b_consumed {
                transpositions += 1;
            }
            b_consumed = j;
        }
    }
    if matches == 0 {
        return 0.0;
    }
    let m = matches as f64;
    (m / a.len() as f64 + m / b.len() as f64 + (m - transpositions as f64) / m) / 3.0
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;

    /// What the shell was told, a line each.
    #[derive(Default)]
    struct Told(Vec<String>);

    impl Shell for Told {
        fn status(&mut self, word: &str, rest: &str) {
            self.0.push(format!("{word} {rest}"));
        }

        fn warn(&mut self, text: &str) {
            self.0.push(format!("warning: {text}"));
        }

        fn note(&mut self, text: &str) {
            self.0.push(format!("note: {text}"));
        }

        fn line(&mut self, text: &str) {
            self.0.push(text.to_owned());
        }
    }

    const FLAGS: [own::Flag; 6] = [
        valued(&["--vcs"]),
        switch(&["--bin"]),
        switch(&["--lib"]),
        valued(&["--edition"]),
        valued(&["--name"]),
        valued(&["--registry"]),
    ];

    fn read(words: &[&str]) -> Read {
        own::read(words.iter().map(OsString::from).collect(), &FLAGS).unwrap()
    }

    /// A home with no cargo configuration, so the machine's own decides nothing.
    fn env(home: &Path) -> Vec<(String, String)> {
        let home = home.to_str().unwrap().to_owned();
        vec![("HOME".to_owned(), home.clone()), ("CARGO_HOME".to_owned(), format!("{home}/.cargo"))]
    }

    #[test]
    fn new_writes_a_binary_package_where_it_is_asked() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let read = read(&["--vcs", "none", "hello"]);
        let asked = asked(false, &read).unwrap().unwrap();
        let mut told = Told::default();
        make(false, &read, asked, &cwd, env(&cwd), &mut told).unwrap();
        let manifest = std::fs::read_to_string(cwd.join("hello/Cargo.toml")).unwrap();
        assert!(manifest.contains("name = \"hello\""), "{manifest}");
        assert!(manifest.contains("edition = \"2024\""), "{manifest}");
        let source = std::fs::read_to_string(cwd.join("hello/src/main.rs")).unwrap();
        assert!(source.contains("Hello, world!"), "{source}");
        assert!(!cwd.join("hello/.git").exists(), "`--vcs none` makes no repository");
        assert_eq!(told.0.first().map(String::as_str), Some("Creating binary (application) `hello` package"));

        let again = asked_again(&cwd, &["--vcs", "none", "hello"]);
        assert!(format!("{again:#}").contains("already exists"), "{again:#}");
    }

    fn asked_again(cwd: &Path, words: &[&str]) -> anyhow::Error {
        let read = read(words);
        let asked = asked(false, &read).unwrap().unwrap();
        make(false, &read, asked, cwd, env(cwd), &mut Told::default()).unwrap_err()
    }

    #[test]
    fn init_makes_a_library_of_the_directory_it_is_run_in() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap().join("pkg");
        std::fs::create_dir(&cwd).unwrap();
        let read = read(&["--lib", "--vcs", "none", "--edition", "2021"]);
        let asked = asked(true, &read).unwrap().unwrap();
        assert_eq!(asked.path, ".");
        make(true, &read, asked, &cwd, env(cwd.parent().unwrap()), &mut Told::default()).unwrap();
        let manifest = std::fs::read_to_string(cwd.join("Cargo.toml")).unwrap();
        assert!(manifest.contains("name = \"pkg\""), "{manifest}");
        assert!(manifest.contains("edition = \"2021\""), "{manifest}");
        assert!(cwd.join("src/lib.rs").is_file());
        assert!(!cwd.join("src/main.rs").exists());
    }

    #[test]
    fn a_value_clap_refuses_names_its_flag_and_new_needs_a_path() {
        let refused = asked(false, &read(&["--edition", "2019", "x"])).unwrap().unwrap_err();
        assert_eq!((refused.flag, refused.value.as_str()), ("--edition <YEAR>", "2019"));
        assert_eq!(similar("2019", &EDITIONS), Some("2021"));
        assert_eq!(similar("2023", &EDITIONS), Some("2024"));
        let refused = asked(true, &read(&["--vcs", "svn"])).unwrap().unwrap_err();
        assert_eq!(refused.flag, "--vcs <VCS>");
        assert_eq!(similar("svn", &VersionControl::VALUES), None);
        assert!(asked(false, &read(&[])).is_err(), "`new` with no path");
    }
}
