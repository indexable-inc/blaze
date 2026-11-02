//! A probe checked as `blaze check` checks a crate: one package of one library, its root the
//! probe's source, loaded with the standard library v2 builds against by the workspace load
//! ([`blaze_units::query::Plan`], which merges std's units into every build), lowered into the
//! front end ([`blaze_lower::lower`]) and checked by [`blaze_diagnostics::check_crate`], its
//! diagnostics written as rustc writes them.
//!
//! std is given what its build script gives it ([`std_script`]), which the load does not run.
//! What rustc gives the crate and cargo would not is set over what the load made of it: its
//! cfgs are those `rustc --print cfg` says for the probe's flags (blaze's target table), its
//! own variables none (rustc's `env!` reads the process's, which are the script's), and the
//! process's environment this one's. The standard library is built as rustc's own is, for the
//! probe's target with the release profile and none of the probe's flags.
//!
//! The package is in memory; `library/` is read from the disk file by file as the load and the
//! front end ask for it, so only what they read is read.

use std::{
    collections::HashMap,
    ffi::OsString,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context as _, anyhow, bail};
use base_db::{ContentId, Env, FileId};
use blaze_diagnostics::{Code, Diag, Files, Options, emit, lints::CommandLine};
use blaze_query::{Config, Database, Names, Tree};
use blaze_units::{
    query::{BuildConfig, Command, Invocation, Mount, Origin, Plan, Planned, Units},
    schema::Mode as UnitMode,
    stdlib,
};
use blaze_vfs::{Bytes, Corrupt, Mode, View};
use cfg::{CfgAtom, CfgOptions};
use intern::Symbol;
use parking_lot::Mutex;

/// Where the probe's package is named: under no directory of the machine's, so nothing of it is
/// read from the disk.
const DIR: &str = "/@blaze-probe";

/// `HOME` of the load, in the package's directory: no `$CARGO_HOME/config.toml` is there.
const HOME: &str = "/@blaze-probe/home";

/// The probe's root file in the package.
const ROOT_FILE: &str = "probe.rs";

const VIEW_DOMAIN: &str = "blaze v2 rustc stand-in probe view 1";
const LIBRARY_DOMAIN: &str = "blaze v2 rustc stand-in library root 1";

/// The editions rustc's `--edition` takes.
const EDITIONS: [&str; 4] = ["2015", "2018", "2021", "2024"];

/// The standard library's source on the disk, and the commit of rust-lang/rust it is
/// ([`stdlib::library_commit`]).
pub(super) struct Library {
    pub(super) dir: PathBuf,
    pub(super) commit: String,
}

impl Library {
    /// The library at `dir`, refused unless it is a copy of the rustc blaze is.
    pub(super) fn at(dir: PathBuf) -> anyhow::Result<Library> {
        let upstream = dir.join("UPSTREAM");
        let upstream = std::fs::read_to_string(&upstream).with_context(|| format!("reading {}", upstream.display()))?;
        let commit = stdlib::library_commit(&upstream).map_err(anyhow::Error::msg)?;
        Ok(Library { dir, commit })
    }
}

/// What a probe is checked as: what rustc was told of it.
pub(super) struct Asked<'a> {
    /// The crate's name, an identifier.
    pub(super) name: &'a str,
    pub(super) edition: &'a str,
    pub(super) triple: &'a str,
    pub(super) host: &'a str,
    /// The crate's cfgs, as `rustc --print cfg` prints them.
    pub(super) cfgs: &'a [String],
    /// rustc's arguments, whose lint flags, `--check-cfg`s and `--emit` the check reads.
    pub(super) args: &'a [String],
    pub(super) style: emit::Style,
    /// `--error-format=json`: each diagnostic a line of rustc's JSON.
    pub(super) json: bool,
}

/// What rustc would have said checking the probe, and whether it would have failed.
#[derive(Debug)]
pub(super) struct Checked {
    pub(super) failed: bool,
    pub(super) stderr: String,
}

/// The probe `source`, named `shown` in its diagnostics, checked as `asked` says over the
/// standard library at `library`.
pub(super) fn check(asked: &Asked<'_>, shown: &str, source: &str, library: &Library) -> anyhow::Result<Checked> {
    if !EDITIONS.contains(&asked.edition) {
        bail!("argument for `--edition` must be one of: {}. (instead was `{}`)", EDITIONS.join("|"), asked.edition);
    }
    if asked.name.is_empty() || !asked.name.chars().all(|it| it.is_alphanumeric() || it == '_') {
        bail!("invalid character in crate name: `{}`", asked.name);
    }
    let manifest = format!(
        "[package]\nname = \"blaze-probe\"\nversion = \"0.0.0\"\nedition = \"{}\"\n\n\
         [lib]\nname = \"{}\"\npath = \"{ROOT_FILE}\"\ntest = false\ndoctest = false\nbench = false\n\n\
         [workspace]\n",
        asked.edition, asked.name
    );
    let tree = Arc::new(ProbeFiles::new(&manifest, source, library));
    let mut db = Database::new(Config { tree: Some(tree.clone()), dir: None, tags: 0 });
    blaze_units::query::register(&mut db);
    let mut mounts = vec![
        Mount { at: DIR.to_owned(), origin: Origin::View, under: String::new() },
        stdlib::library_mount(tree.library, &library.commit),
    ];
    mounts.sort();
    let config = BuildConfig {
        invocation: Invocation {
            command: Command::Check,
            args: ["--release", "--target", asked.triple].map(str::to_owned).to_vec(),
            root: DIR.to_owned(),
            cwd: DIR.to_owned(),
            env: vec![("HOME".to_owned(), HOME.to_owned())],
            files: Vec::new(),
        },
        mounts,
        host: asked.host.to_owned(),
        library: stdlib::library_at(&library.commit),
        std: format!("{DIR}/std"),
    };
    let view = db.open_with(View { root: tree.view }, &config.encode());
    let planned = db
        .pin()
        .get::<Plan>(view, &())
        .map_err(|error| anyhow!("loading the probe with the standard library: {error}"))?
        .get();
    tree.failed()?;
    let units = match planned {
        Planned::Ready(units) => units,
        Planned::Failed(error) => bail!("loading the probe with the standard library: {}", error.what),
        Planned::Needs(fetches) => {
            let named: Vec<String> = fetches.iter().map(|it| format!("`{}`", it.what)).collect();
            bail!("the probe's load reads what nothing fetches for it: {}", named.join(", "));
        }
    };
    let index = units
        .graph
        .units
        .iter()
        .position(|it| !it.is_std && it.mode != UnitMode::RunCustomBuild && it.target.is_lib())
        .context("the load made no unit of the probe's crate")?;
    let lower = blaze_lower::Lower { units: &units, config: &config, files: &*tree, root: tree.view, host_cpu: None };
    let mut lowered = blaze_lower::lower(lower).context("lowering the probe with the standard library")?;
    tree.failed()?;
    std_script(&mut lowered, &units, library)?;
    let krate = lowered.krate(index).context("the probe's unit has no crate in the front end")?;
    let cfg: CfgOptions = asked.cfgs.iter().map(|it| atom(it)).collect::<anyhow::Result<_>>()?;
    let process: Env = std::env::vars_os()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
        .collect();
    let db = lowered.served_db();
    krate.set_cfg_options(db, cfg);
    krate.set_env(db, Env::default());
    base_db::set_process_env(db, process);

    let db = lowered.db();
    let root_file = krate.data(db).root_file_id;
    let mut paths: HashMap<FileId, String> = (**lowered.paths()).clone();
    paths.insert(root_file, shown.to_owned());
    let lints = CommandLine::parse(asked.args).map_err(anyhow::Error::msg)?;
    // rustc gives the future-incompatibility report only for `--json=future-incompat`, which
    // the probe refuses.
    let options = Options { lints, keep_duplicates: false, future_breakage_report: false };
    let diags = blaze_diagnostics::check_crate(db, hir::Crate::from(krate), Files::new(&paths, ""), &options);
    if let Some(diag) = diags.iter().find(|it| it.code.as_ref().and_then(Code::error) == Some("E0583")) {
        bail!("blaze reads a probe's one file, and it declares a module in another: {}", diag.message);
    }
    Ok(Checked { failed: diags.iter().any(Diag::is_error), stderr: written(&diags, asked, shown, source, library)? })
}

/// What std's build script (`library/std/build.rs`) gives std, set on its crate: the probe's
/// load runs no build script, and `restricted_std` decides whether a probe on a bare-metal
/// target may name std. The output is the build's own ([`stdlib::scripts`], whose pin of the
/// script stops the check when it differs), run with the target's cfg variables and none of the
/// script's own (`RUSTC_STD_NETBSD10`, `WIND_RELEASE_ID`, `RUSTC_BOOTSTRAP_SYNTHETIC_TARGET`),
/// as for the library rustc ships.
fn std_script(lowered: &mut blaze_lower::Lowered, units: &Units, library: &Library) -> anyhow::Result<()> {
    let root = library.dir.join("std/build.rs");
    let root = root.to_str().with_context(|| format!("{} is not UTF-8", root.display()))?;
    let read = |path: &str| std::fs::read(path).map_err(|e| format!("reading {path}: {e}"));
    let script = stdlib::scripts::identify("std", root, read).map_err(anyhow::Error::msg)?;
    let index = units
        .graph
        .units
        .iter()
        .position(|it| it.is_std && it.target.name == "std" && it.target.is_lib() && it.mode != UnitMode::RunCustomBuild)
        .context("the probe's load made no unit of std")?;
    let krate = lowered.krate(index).context("std's unit has no crate in the front end")?;
    let mut cfg = krate.cfg_options(lowered.db()).clone();
    let mut env = krate.env(lowered.db()).clone();
    let value = |key: &str| cfg.get_cfg_values(key).next().map_or("", |it| it.as_str());
    let vars = [
        ("CARGO_CFG_TARGET_ARCH", value("target_arch")),
        ("CARGO_CFG_TARGET_ENV", value("target_env")),
        ("CARGO_CFG_TARGET_OS", value("target_os")),
        ("CARGO_CFG_TARGET_VENDOR", value("target_vendor")),
    ]
    .map(|(name, value)| (OsString::from(name), OsString::from(value)));
    let dir = library.dir.join("std");
    let printed = stdlib::scripts::printed(script, &vars, &dir).map_err(anyhow::Error::msg)?;
    let printed = String::from_utf8(printed).context("std's build script printed other than UTF-8")?;
    for line in printed.lines() {
        let directive = line.strip_prefix("cargo::").or_else(|| line.strip_prefix("cargo:"));
        let Some((key, value)) = directive.and_then(|it| it.split_once('=')) else {
            bail!("std's build script printed `{line}`, which the rustc stand-in does not read");
        };
        match key {
            "rerun-if-changed" | "rustc-check-cfg" => {}
            "rustc-cfg" if !value.contains('=') => cfg.insert_atom(Symbol::intern(value)),
            "rustc-env" => {
                let (name, value) = value.split_once('=').with_context(|| format!("std's build script printed `{line}`"))?;
                env.set(name, value);
            }
            _ => bail!("std's build script printed `{line}`, which the rustc stand-in does not read"),
        }
    }
    let db = lowered.served_db();
    krate.set_cfg_options(db, cfg);
    krate.set_env(db, env);
    Ok(())
}

/// `diags` as rustc writes them to standard error, the files they point into read: the probe's
/// own as `source`, the library's from its directory. A span of a file neither, or past its
/// end, fails the probe.
fn written(
    diags: &[Diag],
    asked: &Asked<'_>,
    shown: &str,
    source: &str,
    library: &Library,
) -> Result<String, emit::Unplaced> {
    let mut sources = emit::Sources::default();
    let named = stdlib::library_at(&library.commit);
    for name in sources.unread(diags) {
        let text = match name == shown {
            true => Some(source.to_owned()),
            false => name
                .strip_prefix(named.as_str())
                .and_then(|rest| rest.strip_prefix('/'))
                .and_then(|rest| std::fs::read_to_string(library.dir.join(rest)).ok()),
        };
        sources.add(name, text);
    }
    let mut out = String::new();
    for diag in diags {
        match asked.json {
            true => {
                let line = emit::json(diag, &sources, asked.style)?;
                if !line.is_empty() {
                    out += &line;
                    out.push('\n');
                }
            }
            false => out += &emit::written(diag, &sources, asked.style)?,
        }
    }
    Ok(out)
}

/// A cfg as `--print cfg` prints it (`unix`, `target_os="linux"`).
fn atom(cfg: &str) -> anyhow::Result<CfgAtom> {
    let (name, value) = match cfg.split_once('=') {
        Some((name, value)) => (name.trim(), Some(value.trim())),
        None => (cfg.trim(), None),
    };
    let name = name.strip_prefix("r#").unwrap_or(name);
    let is_name = name.starts_with(|c: char| c.is_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_alphanumeric() || c == '_');
    if !is_name {
        bail!("`--cfg {cfg}`: `{name}` is not a cfg's name");
    }
    let Some(value) = value else { return Ok(CfgAtom::Flag(Symbol::intern(name))) };
    let Some(inner) = value.strip_prefix('"').and_then(|it| it.strip_suffix('"')) else {
        bail!("`--cfg {cfg}`: a cfg's value is a string in quotes, as `{name}=\"value\"`");
    };
    let mut text = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            text.push(c);
            continue;
        }
        text.push(match chars.next() {
            Some('n') => '\n',
            Some('r') => '\r',
            Some('t') => '\t',
            Some('0') => '\0',
            Some(c @ ('\\' | '"' | '\'')) => c,
            _ => bail!("`--cfg {cfg}`: the value has an escape rustc does not take"),
        });
    }
    Ok(CfgAtom::KeyValue { key: Symbol::intern(name), value: Symbol::intern(&text) })
}

/// The files of the probe's load: the package's, in memory, and the library's, read from its
/// directory when asked for. A file of the library that is there and can't be read is kept as
/// the error the check stops with, never taken for one that is not there.
struct ProbeFiles {
    view: ContentId,
    /// Sorted by name.
    package: [(&'static str, ContentId, Bytes); 2],
    library: ContentId,
    dir: PathBuf,
    failed: Mutex<Option<String>>,
}

impl ProbeFiles {
    fn new(manifest: &str, source: &str, library: &Library) -> ProbeFiles {
        let file = |bytes: &[u8]| {
            (ContentId::of(blaze_vfs::FILE_DOMAIN, bytes), Bytes::from(Arc::<[u8]>::from(bytes)))
        };
        let (manifest_id, manifest_bytes) = file(manifest.as_bytes());
        let (source_id, source_bytes) = file(source.as_bytes());
        let mut both = Vec::with_capacity(64);
        both.extend_from_slice(&manifest_id.0);
        both.extend_from_slice(&source_id.0);
        ProbeFiles {
            view: ContentId::of(VIEW_DOMAIN, &both),
            package: [("Cargo.toml", manifest_id, manifest_bytes), (ROOT_FILE, source_id, source_bytes)],
            library: ContentId::of(LIBRARY_DOMAIN, library.commit.as_bytes()),
            dir: library.dir.clone(),
            failed: Mutex::new(None),
        }
    }

    fn fail(&self, path: &Path, error: std::io::Error) {
        let mut failed = self.failed.lock();
        failed.get_or_insert_with(|| format!("reading {}: {error}", path.display()));
    }

    /// The first file of the library that could not be read, as an error.
    fn failed(&self) -> anyhow::Result<()> {
        match self.failed.lock().as_ref() {
            Some(why) => bail!("the standard library's source: {why}"),
            None => Ok(()),
        }
    }

    fn library_file(&self, path: &str) -> Option<(ContentId, Mode, Bytes)> {
        if !stdlib::in_library(path) {
            return None;
        }
        let at = self.dir.join(path);
        let meta = match std::fs::symlink_metadata(&at) {
            Ok(meta) => meta,
            Err(error) if gone(&error) => return None,
            Err(error) => {
                self.fail(&at, error);
                return None;
            }
        };
        let (mode, bytes) = if meta.file_type().is_symlink() {
            match std::fs::read_link(&at) {
                Ok(to) => (Mode::Symlink, to.to_string_lossy().into_owned().into_bytes()),
                Err(error) => {
                    self.fail(&at, error);
                    return None;
                }
            }
        } else if meta.is_file() {
            match std::fs::read(&at) {
                Ok(bytes) => (if executable(&meta) { Mode::Executable } else { Mode::Regular }, bytes),
                Err(error) => {
                    self.fail(&at, error);
                    return None;
                }
            }
        } else {
            return None;
        };
        let id = ContentId::of(blaze_vfs::FILE_DOMAIN, &bytes);
        Some((id, mode, Bytes::from(Arc::<[u8]>::from(bytes))))
    }

    fn library_names(&self, path: &str) -> Option<Vec<(String, bool)>> {
        let at = self.dir.join(path);
        let entries = match std::fs::read_dir(&at) {
            Ok(entries) => entries,
            Err(error) if gone(&error) => return None,
            Err(error) => {
                self.fail(&at, error);
                return None;
            }
        };
        let mut names = Vec::new();
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    self.fail(&at, error);
                    return None;
                }
            };
            let name = match entry.file_name().into_string() {
                Ok(name) => name,
                Err(_) => {
                    let error = std::io::Error::new(std::io::ErrorKind::InvalidData, "a name that is not UTF-8");
                    self.fail(&entry.path(), error);
                    return None;
                }
            };
            if path.is_empty() && !stdlib::in_library(&name) {
                continue;
            }
            let dir = match std::fs::metadata(entry.path()) {
                Ok(meta) => meta.is_dir(),
                Err(error) => {
                    self.fail(&entry.path(), error);
                    return None;
                }
            };
            names.push((name, dir));
        }
        names.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        Some(names)
    }
}

fn gone(error: &std::io::Error) -> bool {
    matches!(error.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory)
}

#[cfg(unix)]
fn executable(meta: &std::fs::Metadata) -> bool {
    std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o111 != 0
}

#[cfg(not(unix))]
fn executable(_: &std::fs::Metadata) -> bool {
    false
}

impl Tree for ProbeFiles {
    fn file(&self, root: ContentId, path: &str) -> Result<Option<(ContentId, Mode, Bytes)>, Corrupt> {
        if root == self.view {
            let found = self.package.iter().find(|(name, ..)| *name == path);
            return Ok(found.map(|(_, id, bytes)| (*id, Mode::Regular, bytes.clone())));
        }
        if root == self.library {
            return Ok(self.library_file(path));
        }
        Ok(None)
    }

    fn read_dir(&self, root: ContentId, path: &str, read: &mut dyn FnMut(Names<'_, '_>)) -> bool { // dyn: `Tree`'s, once a directory
        if root == self.view {
            if !path.is_empty() {
                return false;
            }
            read(&mut self.package.iter().map(|(name, ..)| (*name, false)));
            return true;
        }
        if root != self.library {
            return false;
        }
        let Some(names) = self.library_names(path) else { return false };
        read(&mut names.iter().map(|(name, dir)| (name.as_str(), *dir)));
        true
    }
}
