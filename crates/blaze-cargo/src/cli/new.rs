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

/// The editions `--edition` takes: Rust's, and the blaze edition
/// (docs/v2/plan/design/std.md, "The blaze edition: opting in"; `the_editions_are_the_ones_known`
/// holds the last to `BlazeEdition::LATEST`).
const EDITIONS: [&str; 5] = ["2015", "2018", "2021", "2024", "blaze-2026.11"];

/// The editions `--edition` takes beside `--rust`: Rust's.
const RUST_EDITIONS: [&str; 4] = ["2015", "2018", "2021", "2024"];

/// The Rust edition `--rust` asks for when `--edition` names none: Rust's latest.
const RUST: &str = "2024";

/// cargo's shell on standard error.
struct Said {
    quiet: bool,
    color: bool,
}

impl Shell for Said {
    fn status(&mut self, word: &str, rest: &str) {
        if !self.quiet {
            blaze_report::err!("{}", say::status(word, rest, self.color));
        }
    }

    fn warn(&mut self, text: &str) {
        if !self.quiet {
            blaze_report::err!("{}", say::warning(text, self.color));
        }
    }

    fn note(&mut self, text: &str) {
        if !self.quiet {
            let mut out = String::with_capacity(text.len() + 24); // alloc: a note, said once
            say::note_into(&mut out, text, self.color);
            blaze_report::err!("{out}");
        }
    }

    fn line(&mut self, text: &str) {
        blaze_report::errln!("{text}");
    }
}

/// What `new` or `init` was asked, its values checked as clap checks them.
#[derive(Debug)]
struct Asked {
    /// The package's directory as typed: `init`'s is `.` when none is.
    path: String,
    vcs: Option<VersionControl>,
    edition: Option<String>,
    /// `--bstd <PATH>`: where bstd's package is, for a package of the blaze edition.
    bstd: Option<String>,
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
    // `--rust`: a plain Rust package, whatever the default is. With `--edition` it only says
    // the edition must be Rust's; alone it is Rust's latest.
    let rust = read.has("--rust");
    if rust
        && let Some(it) = &edition
        && it.starts_with(span::BlazeEdition::MANIFEST_PREFIX)
    {
        // clone: the refused value, once
        return Ok(Err(Invalid { flag: "--edition <YEAR>", value: it.clone(), possible: &RUST_EDITIONS }));
    }
    let edition = match (edition, rust) {
        (None, true) => Some(RUST.to_owned()), // alloc: the edition, once
        (edition, _) => edition,
    };
    Ok(Ok(Asked { path, vcs, edition, bstd: read.value("--bstd") }))
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
    let mut opts = NewOptions::new(
        asked.vcs,
        read.has("--bin"),
        read.has("--lib"),
        cwd.join(&asked.path),
        read.value("--name"),
        asked.edition,
        registry,
    )?;
    opts.bstd = asked.bstd;
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
        switch(&["--rust"]),
        valued(&["--bstd"]),
        valued(&["--name"]),
        valued(&["--registry"]),
    ];
    let usage = format!("blaze {name} [OPTIONS] {}", if init { "[PATH]" } else { "<PATH>" });
    let read = match own::read_or_refuse(words, &flags, &usage, 1, color) {
        Ok(read) => read,
        Err(code) => return code,
    };
    let said_usage = || {
        blaze_report::errln!("\nUsage: {usage}\n\nFor more information, try '--help'.");
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
            blaze_report::err!("{}", say::error(&what, color));
            if let Some(similar) = similar(&invalid.value, invalid.possible) {
                blaze_report::errln!("\n  tip: a similar value exists: '{similar}'");
            }
            return said_usage();
        }
        Err(()) => {
            let what = "the following required arguments were not provided:\n  <PATH>";
            blaze_report::err!("{}", say::error(what, color));
            return said_usage();
        }
    };
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(error) => {
            let what = format!("blaze {name}: the current directory: {error}");
            blaze_report::err!("{}", say::error(&what, color));
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

    const FLAGS: [own::Flag; 8] = [
        valued(&["--vcs"]),
        switch(&["--bin"]),
        switch(&["--lib"]),
        valued(&["--edition"]),
        switch(&["--rust"]),
        valued(&["--bstd"]),
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

    /// `new` with `--bstd` writes a package of the blaze edition: the opt-in in the manifest,
    /// bstd named where it was said to be, and a `main` that takes its world and writes its
    /// line through `Out`.
    #[test]
    fn new_writes_a_package_of_the_blaze_edition_where_bstd_is_named() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let read = read(&["--vcs", "none", "--bstd", "/src/blaze/crates/blaze-std", "hello"]);
        let asked = asked(false, &read).unwrap().unwrap();
        make(false, &read, asked, &cwd, env(&cwd), &mut Told::default()).unwrap();
        let manifest = std::fs::read_to_string(cwd.join("hello/Cargo.toml")).unwrap();
        assert!(manifest.contains("edition = \"blaze-2026.11\""), "{manifest}");
        assert!(
            manifest.contains("bstd = { package = \"blaze-std\", path = \"/src/blaze/crates/blaze-std\" }"),
            "{manifest}"
        );
        let source = std::fs::read_to_string(cwd.join("hello/src/main.rs")).unwrap();
        assert!(source.contains("fn main(cx: &Cx, io: Io) -> Result<(), Error>"), "{source}");
        assert!(source.contains("io.need_out()?.line(cx, \"Hello, world!\")?;"), "{source}");
        assert!(!source.contains("println"), "nothing ambient: {source}");
        // A library of the edition has no test module yet.
        let read = self::read(&["--vcs", "none", "--lib", "--bstd", "../bstd", "shelf"]);
        let asked = self::asked(false, &read).unwrap().unwrap();
        make(false, &read, asked, &cwd, env(&cwd), &mut Told::default()).unwrap();
        let source = std::fs::read_to_string(cwd.join("shelf/src/lib.rs")).unwrap();
        assert!(source.contains("pub fn add") && !source.contains("#[test]"), "{source}");
    }

    /// `--rust` is the plain package, byte for byte what `new` wrote before the edition, even
    /// where bstd is named; and it refuses a blaze edition asked beside it.
    #[test]
    fn rust_asks_for_the_plain_package() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        for (name, words) in [
            ("plain", &["--vcs", "none", "plain"][..]),
            ("rust", &["--vcs", "none", "--rust", "rust"]),
            ("both", &["--vcs", "none", "--rust", "--bstd", "/x", "both"]),
        ] {
            let read = read(words);
            let asked = asked(false, &read).unwrap().unwrap();
            make(false, &read, asked, &cwd, env(&cwd), &mut Told::default()).unwrap();
            let manifest = std::fs::read_to_string(cwd.join(name).join("Cargo.toml")).unwrap();
            assert_eq!(
                manifest,
                format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\n"),
            );
            let source = std::fs::read_to_string(cwd.join(name).join("src/main.rs")).unwrap();
            assert_eq!(source, "fn main() {\n    println!(\"Hello, world!\");\n}\n");
        }
        let refused =
            asked(false, &read(&["--rust", "--edition", "blaze-2026.11", "x"])).unwrap().unwrap_err();
        assert_eq!((refused.flag, refused.value.as_str()), ("--edition <YEAR>", "blaze-2026.11"));
        assert_eq!(refused.possible, RUST_EDITIONS);
    }

    /// The blaze edition asked where blaze cannot say where bstd is stops, naming the flag
    /// and the row, and writes no package that could not build. (Until `BSTD_SHIPS`.)
    #[test]
    fn the_blaze_edition_without_bstd_is_refused_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let refused = asked_again(&cwd, &["--vcs", "none", "--edition", "blaze-2026.11", "hello"]);
        let said = format!("{refused:#}");
        assert!(!blaze_new::BSTD_SHIPS, "this test is of the time before bstd ships");
        assert!(said.contains("--bstd <PATH>") && said.contains("S5d"), "{said}");
        assert!(!cwd.join("hello/Cargo.toml").exists(), "no manifest of a package that cannot build");
    }

    /// The command line's editions are the ones blaze knows: Rust's four and every blaze
    /// edition.
    #[test]
    fn the_editions_are_the_ones_known() {
        let blaze: Vec<&str> = span::BlazeEdition::ALL.iter().map(|it| it.manifest()).collect();
        assert_eq!(EDITIONS[..4], RUST_EDITIONS);
        assert_eq!(EDITIONS[4..], blaze[..]);
        assert_eq!(blaze_new::default_edition(true), span::BlazeEdition::LATEST.manifest());
        assert_eq!(blaze_new::default_edition(false), if blaze_new::BSTD_SHIPS { "blaze-2026.11" } else { RUST });
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
