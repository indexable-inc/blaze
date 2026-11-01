//! What the daemon's checker answers a lint request with: for each crate of the packages
//! asked of, its diagnostics as rustc would write them in each build of it cargo makes, the
//! one without its tests and the one with them. The client says which builds there are and
//! writes them as cargo does.

use std::path::{Path, PathBuf};

use base_db::{Crate, Db, FileId};
use blaze_diagnostics::{
    emit::{self, Sources, Style, Unplaced},
    model::{Child, Diag},
};
use blaze_hash::{HashMap, HashSet};
use serde::{Deserialize, Serialize};

use crate::{
    check::{Found, Setup},
    conf::{self, Conf},
    level::CommandLine,
};

/// A lint request: the packages whose crates are linted, and how diagnostics are written.
#[derive(Serialize, Deserialize, Debug, Default)]
pub struct Ask {
    pub packages: Vec<Package>,
    /// `--error-format=short`, as `--message-format=short` and `json-diagnostic-short` ask.
    pub short: bool,
    /// In colour: cargo asks rustc for it when it writes to a terminal, or JSON asks for it.
    pub ansi: bool,
    /// The terminal's columns, when cargo would pass them to rustc.
    pub width: Option<usize>,
}

/// A package to lint, and what rustc would be given for it.
#[derive(Serialize, Deserialize, Debug, Default)]
pub struct Package {
    pub name: String,
    pub manifest_dir: PathBuf,
    /// The lint flags rustc would be given, in its order: the package's `[lints]`,
    /// `RUSTFLAGS`, then `cargo clippy -- …`'s.
    pub flags: Vec<String>,
    /// `CLIPPY_CONF_DIR`.
    pub conf_dir: Option<PathBuf>,
    /// The package's `rust-version`, which clippy takes as its MSRV.
    pub rust_version: Option<String>,
}

/// What the checker found.
#[derive(Serialize, Deserialize, Debug, Default)]
pub struct Answer {
    pub crates: Vec<Linted>,
    /// What stopped a package from being linted, as clippy says it, by package.
    pub failed: Vec<(String, String)>,
}

/// A crate linted: its root file, and its diagnostics in a build without its tests and in one
/// with them (`--test`), when it has them.
#[derive(Serialize, Deserialize, Debug, Default)]
pub struct Linted {
    pub package: String,
    pub root: PathBuf,
    pub plain: Vec<Message>,
    pub test: Option<Vec<Message>>,
}

/// A diagnostic as rustc writes it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// Its text as rustc writes it to stderr, the empty line after it included.
    pub rendered: String,
    /// rustc's JSON of it, with that text as `rendered`.
    pub json: String,
    pub error: bool,
    /// Whether `cargo clippy --fix` would apply it.
    pub fixable: bool,
}

/// What each package's flags and `clippy.toml` make of its lints, kept between requests by
/// whoever answers them: a request then parses no flags and reads no `clippy.toml` it has
/// already. [`Setups::forget`] is called when a `clippy.toml` or `.clippy.toml` changes.
#[derive(Debug, Default)]
pub struct Setups {
    /// By manifest directory: each setup made there, with what it was made of.
    by_dir: HashMap<PathBuf, Vec<(SetupOf, Result<Setup, String>)>>,
}

/// What of a [`Package`] its [`Setup`] is made from, besides its manifest directory.
#[derive(Debug, PartialEq, Eq)]
struct SetupOf {
    flags: Vec<String>,
    conf_dir: Option<PathBuf>,
    rust_version: Option<String>,
}

impl SetupOf {
    fn is(&self, package: &Package) -> bool {
        self.flags == package.flags
            && self.conf_dir == package.conf_dir
            && self.rust_version == package.rust_version
    }
}

impl Setups {
    /// Forgets every setup: a `clippy.toml` may have changed.
    pub fn forget(&mut self) {
        self.by_dir.clear();
    }

    /// Makes `package`'s setup unless one was made of the same.
    fn load(&mut self, package: &Package) {
        if self.by_dir.get(package.manifest_dir.as_path()).is_some_and(|it| it.iter().any(|(of, _)| of.is(package))) {
            return;
        }
        let of = SetupOf {
            // clone: what a setup was made of, once per package and change of it
            flags: package.flags.clone(),
            conf_dir: package.conf_dir.clone(),
            rust_version: package.rust_version.clone(),
        };
        // clone: the key, once per package and change of it
        self.by_dir.entry(package.manifest_dir.clone()).or_default().push((of, setup(package)));
    }

    /// The setup [`Setups::load`] made of `package`.
    fn get(&self, package: &Package) -> &Result<Setup, String> {
        self.by_dir
            .get(package.manifest_dir.as_path())
            .and_then(|it| it.iter().find(|(of, _)| of.is(package)))
            .map(|(_, setup)| setup)
            .expect("the package's setup is loaded")
    }
}

/// Lints the crates of `crates` the packages of `ask` have. Files are named from `root`, the
/// workspace's, when they are below it; `path_of` is where a file is.
pub fn answer<P: AsRef<Path>>(
    db: &Db,
    crates: &[Crate],
    root: &Path,
    path_of: &impl Fn(FileId) -> Option<P>,
    ask: &Ask,
) -> Answer {
    answer_with(db, crates, root, path_of, ask, &mut Setups::default())
}

/// [`answer`], with the setups made for earlier requests.
pub fn answer_with<P: AsRef<Path>>(
    db: &Db,
    crates: &[Crate],
    root: &Path,
    path_of: &impl Fn(FileId) -> Option<P>,
    ask: &Ask,
    setups: &mut Setups,
) -> Answer {
    let name_of = |file: FileId| -> Option<String> {
        let path = path_of(file)?;
        let path = path.as_ref();
        Some(match path.strip_prefix(root) {
            Ok(relative) => relative.to_string_lossy().into_owned(),
            Err(_) => path.to_string_lossy().into_owned(),
        })
    };
    let style = Style { short: ask.short, ansi: ask.ansi, width: ask.width.unwrap_or(emit::WIDTH), ui_testing: false };
    let read = |name: &str| std::fs::read_to_string(root.join(name)).ok();
    let mut sources = Sources::default();
    let mut answer = Answer::default();
    for package in &ask.packages {
        setups.load(package);
    }
    let setups = &*setups;
    // alloc: a request's packages, once per request
    let mut chosen: Vec<Option<&Setup>> = Vec::with_capacity(ask.packages.len());
    let mut by_dir: HashMap<&Path, usize> = HashMap::default();
    for (at, package) in ask.packages.iter().enumerate() {
        by_dir.entry(package.manifest_dir.as_path()).or_insert(at);
        match setups.get(package) {
            Ok(setup) => chosen.push(Some(setup)),
            Err(why) => {
                // clone: a package's error, once per request it fails in
                answer.failed.push((package.name.clone(), why.clone()));
                chosen.push(None);
            }
        }
    }
    let has_tests = |krate: Crate| {
        hir::attach_db(db, || {
            hir::Crate::from(krate).cfg(db).check_atom(&cfg::CfgAtom::Flag(intern::sym::test))
        })
    };
    // A crate built for its tests as well is linted once, as that build: what it finds outside
    // `cfg(test)` is what the plain build finds.
    let tested: HashSet<FileId> = crates
        .iter()
        .filter(|&&it| has_tests(it))
        .map(|&it| hir::Crate::from(it).root_file(db))
        .collect();
    for &krate in crates {
        let has_tests = has_tests(krate);
        if !has_tests && tested.contains(&hir::Crate::from(krate).root_file(db)) {
            continue;
        }
        let env = krate.env(db);
        let Some(dir) = env.get("CARGO_MANIFEST_DIR") else { continue };
        let Some(&at) = by_dir.get(Path::new(&dir)) else { continue };
        let Some(setup) = chosen[at] else { continue };
        let hir_crate = hir::Crate::from(krate);
        let Some(root_path) = path_of(hir_crate.root_file(db)) else { continue };
        let root_path = root_path.as_ref().to_path_buf();
        let found = match crate::check(db, hir_crate, setup, &name_of) {
            Ok(found) => found,
            Err(unnamed) => {
                // clone: a package's error, once per request it fails in
                answer.failed.push((ask.packages[at].name.clone(), unnamed.to_string()));
                continue;
            }
        };
        let plain: Vec<&Found> = found.iter().filter(|it| !it.test_only).collect();
        let lints = (|| {
            let test = has_tests.then(|| written(found.iter(), &mut sources, &read, style)).transpose()?;
            Ok::<_, Unplaced>((written(plain.into_iter(), &mut sources, &read, style)?, test))
        })();
        match lints {
            Ok((plain, test)) => answer.crates.push(Linted {
                package: ask.packages[at].name.clone(),
                root: root_path,
                plain,
                test,
            }),
            // clone: a package's error, once per request it fails in
            Err(why) => answer.failed.push((ask.packages[at].name.clone(), why.to_string())),
        }
    }
    answer
}

fn setup(package: &Package) -> Result<Setup, String> {
    let command_line = CommandLine::parse(&package.flags)?;
    let path = conf::find(package.conf_dir.as_deref(), &package.manifest_dir);
    let conf = Conf::load(path, package.rust_version.as_deref())?;
    Ok(Setup { command_line, conf })
}

/// The diagnostics of one build, as rustc writes them: what is said once a build is said with
/// the first diagnostic that has it, and a diagnostic said again is not. The files they point
/// into that `sources` has not read yet are read by `read`; a span of one it can't read, or
/// past its end, fails them all.
pub(crate) fn written<'a>(
    found: impl Iterator<Item = &'a Found>,
    sources: &mut Sources,
    read: impl Fn(&str) -> Option<String>,
    style: Style,
) -> Result<Vec<Message>, Unplaced> {
    let found: Vec<&Found> = found.collect();
    for name in sources.unread(found.iter().map(|it| &it.diagnostic)) {
        sources.add(name, read(name));
    }
    let mut once: HashSet<Child> = HashSet::default();
    let mut seen = HashSet::default();
    let mut messages = Vec::new();
    for Found { diagnostic, .. } in found {
        // clone: a diagnostic of the crate, once per build it is written in
        let mut diagnostic: Diag = diagnostic.clone();
        diagnostic.children.retain(|child| !child.once || once.insert(child.clone()));
        let rendered = emit::written(&diagnostic, sources, style)?;
        if !seen.insert(rendered.clone()) {
            continue;
        }
        let json = emit::json_rendered(&diagnostic, sources, rendered.clone())?;
        messages.push(Message {
            json: if style.ansi { json } else { cargo_order(&json) },
            rendered,
            error: diagnostic.is_error(),
            fixable: diagnostic.is_fixable(),
        });
    }
    Ok(messages)
}

/// rustc's JSON line `line` as cargo writes it again when it takes the colours out of
/// `rendered` (`on_stderr_line_inner` under `MessageFormat::Json`): `rendered`, every other key
/// but `code` sorted at every depth, then `code`. With colours cargo passes rustc's line as it
/// is.
fn cargo_order(line: &str) -> String {
    #[derive(Deserialize, Serialize)]
    struct CompilerMessage {
        rendered: String,
        #[serde(flatten)]
        other: std::collections::BTreeMap<String, serde_json::Value>,
        code: Option<serde_json::Value>,
    }
    let mut message: CompilerMessage = serde_json::from_str(line)
        .unwrap_or_else(|error| panic!("blaze-lints: rustc's JSON of a diagnostic: {error}"));
    message.other.values_mut().for_each(sort_keys);
    message.code.iter_mut().for_each(sort_keys);
    serde_json::to_string(&message)
        .unwrap_or_else(|error| panic!("blaze-lints: a diagnostic as cargo writes it: {error}"))
}

/// Sorts the keys of every object in `value`, as serde_json's `Map` without `preserve_order`
/// has them, whatever features the build unifies.
fn sort_keys(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            let mut entries: Vec<(String, serde_json::Value)> = std::mem::take(map).into_iter().collect();
            entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
            for (key, mut it) in entries {
                sort_keys(&mut it);
                map.insert(key, it);
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(sort_keys),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_diagnostic_is_written_as_cargo_writes_it_again() {
        let line = concat!(
            r#"{"$message_type":"diagnostic","message":"m","code":{"code":"clippy::x","explanation":null},"#,
            r#""level":"warning","spans":[{"file_name":"a.rs","byte_start":0,"expansion":null}],"children":[],"rendered":"r"}"#,
        );
        let expected = concat!(
            r#"{"rendered":"r","$message_type":"diagnostic","children":[],"level":"warning","message":"m","#,
            r#""spans":[{"byte_start":0,"expansion":null,"file_name":"a.rs"}],"code":{"code":"clippy::x","explanation":null}}"#,
        );
        assert_eq!(cargo_order(line), expected);
    }

    /// A setup is made once per package and kept until forgotten: a second request reads no
    /// `clippy.toml`, and one after [`Setups::forget`] reads it again.
    #[test]
    fn a_setup_is_kept_until_forgotten() {
        let dir = std::env::temp_dir().join(format!("blaze-lints-setups-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("clippy.toml"), "msrv = \"1.60\"\n").unwrap();
        let package = |flags: &[&str]| Package {
            name: "p".to_owned(),
            manifest_dir: dir.clone(),
            flags: flags.iter().map(|it| (*it).to_owned()).collect(),
            conf_dir: None,
            rust_version: None,
        };
        let msrv = |setups: &Setups, package: &Package| {
            setups.get(package).as_ref().map(|it| it.conf.msrv).map_err(String::clone)
        };
        let (plain, warned) = (package(&[]), package(&["-W", "clippy::all"]));
        let mut setups = Setups::default();
        setups.load(&plain);
        setups.load(&warned);
        let first = Ok(Some(crate::conf::Version(1, 60, 0)));
        assert_eq!(msrv(&setups, &plain), first);
        assert_eq!(msrv(&setups, &warned), first);
        assert_eq!(setups.get(&plain).as_ref().unwrap().command_line.lints.opts, setup(&plain).unwrap().command_line.lints.opts);
        assert_ne!(
            setups.get(&warned).as_ref().unwrap().command_line.lints.opts,
            setups.get(&plain).as_ref().unwrap().command_line.lints.opts,
        );
        std::fs::write(dir.join("clippy.toml"), "msrv = \"1.70\"\n").unwrap();
        setups.load(&plain);
        assert_eq!(msrv(&setups, &plain), first, "kept: the file is not read again");
        setups.forget();
        setups.load(&plain);
        assert_eq!(msrv(&setups, &plain), Ok(Some(crate::conf::Version(1, 70, 0))));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
