//! `toolchain_cut <json>`: cuts the toolchain from a blaze tree (docs/v2/plan/design/toolchain.md).
//! A script, not library code: it runs once at install, so it lives here with the repository's
//! other tools, beside the reader it writes for (crates/blaze-toolchain, which opens no file).
//!
//! ```text
//! {"tree": "/path/to/blaze", "targets": ["aarch64-apple-darwin"],
//!  "registry": "~/.cargo/registry/cache/index.crates.io-…", "shelf": "~/.blaze/v2/toolchain",
//!  "write_pin": true, "check": false, "macro_side": false}
//! ```
//!
//! - Resolves, per target, the sysroot's units with blaze-unit-graph: `sysroot` (its default
//!   features and `SYSROOT_FEATURES`, as rustc's own build of std asks them) on
//!   `library/Cargo.lock`, and `blaze-std` (its default features) on the tree's
//!   `Cargo.lock`. A target's cfgs are those rustc printed for it, as `harness/answers.py`
//!   recorded them in `pins/answers/rustc.json` (no compiler is asked).
//! - Packs: one of `library/` (kind 3), one of the blaze crates beneath bstd (kind 3, each
//!   crate's folder under its path in the tree), and one per registry crate beneath bstd, made
//!   from the `.crate` the lock pins exactly as `Fetch` makes it (kind 1).
//! - Writes the manifest and every pack to the shelf (each file by its BLAKE3, written to a
//!   temporary name and renamed; a file already there is read and its digest compared), and
//!   with `write_pin` the id to `crates/blaze-toolchain/PINNED`.
//! - With `check`, writes nothing and fails when `PINNED` is not the id the tree cuts to.
//! - With `macro_side`, also cuts the macro side, `wasm32-wasip1` (blaze_toolchain::side): the
//!   library proc macros and build scripts are built against, kept apart from the target side's
//!   by type. The triples cut are the targets and the macro side's, sorted, each once; one not
//!   among the pinned nightly's recorded targets is refused by name before anything is resolved.
//! - With `cfront`, the path of an unpacked blaze LLVM prefix (`llvm.py fetch`), also puts its
//!   C toolchain helper on the shelf (D174, docs/v2/plan/design/c.md "The toolchain side"):
//!   `bin/blaze-cfront`, checked against `pins/cfront.json` for the prefix's target, as
//!   `<shelf>/cfront/<sha256>/blaze-cfront`, beside the clang shelf's `<shelf>/clang/<sha256>/`.

#![feature(allocator_api)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use blaze_fetch::pack::{Builder, Kind, Limits as PackLimits};
use blaze_fetch::tree::{self, Entry as TreeEntry};
use blaze_fetch::unpack::{self, Limits as UnpackLimits};
use blaze_fetch::{Blake3, Mode, PackView};
use blaze_manifest::layout::{Entry, Layout, Need, Pass as LayoutPass, Units};
use blaze_manifest::lock::{Checksum, Lock, LockId, LockSource};
use blaze_own::{Alloc, Vec as AVec};
use blaze_script::input::{self, Fields, Input, InputError};
use blaze_toolchain::implied::{ImplicitSysroot, SysrootUnits, crate_facts, first_in_cycle, implied_edges};
use blaze_toolchain::record::{self, CrateIn, Role, TargetIn, UnitIn, UnitKind};
use blaze_toolchain::side::{SideTriple, TargetSide, cut_triples};
use blaze_toolchain::{Hex, ToolchainView, parse_hex};
use blaze_unit_graph::{Ask, CfgAtom, Package, Pass, Resolution, Resolver, TargetDesc, UnitGraph, UnitKind as GraphKind};

const SCRIPT: &str = "toolchain_cut";

struct Args {
    tree: String,
    targets: Vec<String>,
    registry: String,
    shelf: String,
    write_pin: bool,
    check: bool,
    macro_side: bool,
    /// D174: an unpacked blaze LLVM prefix whose `bin/blaze-cfront` is put on the shelf
    /// (`place_cfront`); empty for none.
    cfront: String,
}

impl Input for Args {
    const FIELDS: &'static [&'static str] = &["tree", "targets", "registry", "shelf", "write_pin", "check", "macro_side", "cfront"];
    fn parse(f: &Fields<'_>) -> Result<Args, InputError> {
        Ok(Args {
            tree: f.str("tree", None)?,
            targets: f.strs_required("targets")?,
            registry: f.str("registry", None)?,
            shelf: f.str("shelf", None)?,
            write_pin: f.bool("write_pin", Some(false))?,
            check: f.bool("check", Some(false))?,
            macro_side: f.bool("macro_side", Some(false))?,
            cfront: f.str("cfront", Some(""))?,
        })
    }
}

fn fail(message: &str) -> ! {
    blaze_script::fail(SCRIPT, message)
}

/// Where a package's files are read from while its manifest is resolved.
enum Files<'p> {
    Disk(PathBuf),
    /// A registry crate's pack: its paths are relative to the crate's top.
    Pack(PackView<'p>),
}

impl Files<'_> {
    fn read(&self, path: &str) -> Option<String> {
        match self {
            Files::Disk(anchor) => std::fs::read_to_string(anchor.join(path)).ok(),
            Files::Pack(view) => view.find(path).and_then(|f| String::from_utf8(f.bytes.to_vec()).ok()),
        }
    }

    /// `dir`'s entries, sorted: (name, is a directory).
    fn list(&self, dir: &str) -> Option<Vec<(String, bool)>> {
        let mut out = match self {
            Files::Disk(anchor) => std::fs::read_dir(anchor.join(dir))
                .ok()?
                .flatten()
                .map(|e| (e.file_name().to_string_lossy().into_owned(), e.file_type().is_ok_and(|t| t.is_dir())))
                .collect::<Vec<_>>(),
            Files::Pack(view) => {
                let prefix = if dir.is_empty() { String::new() } else { format!("{dir}/") };
                let mut seen = BTreeMap::new();
                for file in view.files() {
                    let Some(rest) = file.path.strip_prefix(prefix.as_str()) else { continue };
                    match rest.split_once('/') {
                        Some((name, _)) => seen.insert(name.to_string(), true),
                        None => seen.insert(rest.to_string(), false),
                    };
                }
                if seen.is_empty() && !dir.is_empty() {
                    return None;
                }
                seen.into_iter().collect()
            }
        };
        out.sort();
        Some(out)
    }
}

/// A `Layout` started at `start` within `files`, run to its end.
fn layout(files: &Files<'_>, start: &str, alloc: Alloc<'static>) -> Result<Units<'static>, String> {
    let mut reader = Layout::new(start, alloc).map_err(|e| e.to_string())?;
    for _ in 0..64 {
        let needs = match reader.pass().map_err(|e| e.to_string())? {
            LayoutPass::Done(units) => return Ok(units),
            LayoutPass::Need(needs) => needs,
        };
        for need in needs.iter() {
            match need {
                Need::Manifest(path) => {
                    let text = files.read(path.as_str());
                    reader.manifest(path, text.as_deref()).map_err(|e| format!("{}: {e}", path.as_str()))?;
                }
                Need::Listing(dir) => {
                    let listed = files.list(dir.as_str());
                    let entries: Option<Vec<Entry<'_>>> = listed.as_ref().map(|l| l.iter().map(|(name, is_dir)| Entry { name, is_dir: *is_dir }).collect());
                    reader.listing(dir, entries.as_deref());
                }
            }
        }
    }
    Err(format!("{start}: no layout after 64 passes"))
}

/// A target's cfgs as rustc printed them for the pinned nightly, from the recorded answers: as
/// blaze-unit-graph takes them, and as the record holds them ((name, value), sorted, each once).
fn target_desc(tree: &Path, triple: &str, alloc: Alloc<'static>) -> (TargetDesc<'static>, Vec<(String, Option<String>)>) {
    let path = tree.join(blaze_pins::ANSWERS_PATH);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| fail(&format!("{}: {e}", path.display())));
    let data: serde_json::Value = serde_json::from_str(&text).unwrap_or_else(|e| fail(&format!("{}: {e}", path.display())));
    let Some(target) =data.get("targets").and_then(|t| t.get(triple)) else {
        fail(&format!("{}: no recorded answers for the target `{triple}`", path.display()))
    };
    let strs = |key: &str| -> Vec<String> {
        target.get(key).and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect()).unwrap_or_default()
    };
    let Some(panic) = target.get("panic").and_then(|p| p.as_str()) else { fail(&format!("{}: `{triple}` has no `panic`", path.display())) };
    let mut lines = strs("cfg");
    lines.extend(strs("nightly_cfg"));
    lines.push(format!("panic=\"{panic}\""));
    lines.sort();
    lines.dedup();
    let mut atoms: Vec<(String, Option<String>)> = lines
        .iter()
        .map(|line| match line.split_once('=') {
            Some((name, value)) => (name.to_string(), Some(value.trim_matches('"').to_string())),
            None => (line.clone(), None),
        })
        .collect();
    atoms.sort();
    atoms.dedup();
    let mut cfg = AVec::new_in(alloc);
    for (name, value) in &atoms {
        cfg.push(match value {
            Some(value) => CfgAtom::pair(name, value, alloc),
            None => CfgAtom::flag(name, alloc),
        });
    }
    (TargetDesc::new(triple, cfg, alloc), atoms)
}

/// The targets the pinned nightly's answers are recorded for: the keys of `rustc.json`'s
/// `targets` (the same file `target_desc` reads a target's cfgs from).
fn recorded_targets(tree: &Path) -> Vec<String> {
    let path = tree.join(blaze_pins::ANSWERS_PATH);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| fail(&format!("{}: {e}", path.display())));
    let data: serde_json::Value = serde_json::from_str(&text).unwrap_or_else(|e| fail(&format!("{}: {e}", path.display())));
    let Some(targets) =data.get("targets").and_then(|t| t.as_object()) else {
        fail(&format!("{}: no `targets` object", path.display()))
    };
    targets.keys().cloned().collect()
}

/// Which tree a crate is from.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Origin {
    Library,
    Workspace,
}

/// A lock, its tree's layout, and the registry packs made while resolving on it.
struct Side {
    origin: Origin,
    dir: PathBuf,
    lock: &'static Lock<'static>,
    units: Units<'static>,
    /// A registry crate's pack, by lock id, with its manifest's layout.
    packs: BTreeMap<LockId, &'static [u8]>,
    /// A path package that is no member, by lock id: its folder under `dir`.
    folders: BTreeMap<LockId, String>,
}

fn side(origin: Origin, dir: &Path, alloc: Alloc<'static>) -> Side {
    let path = dir.join("Cargo.lock");
    let bytes: &'static [u8] = std::fs::read(&path).unwrap_or_else(|e| fail(&format!("{}: {e}", path.display()))).leak();
    let lock: &'static Lock<'static> = match Lock::read(bytes, alloc) {
        Ok(lock) => Box::leak(Box::new(lock)),
        Err(e) => fail(&format!("{}: {e}", path.display())),
    };
    let units = layout(&Files::Disk(dir.to_path_buf()), "", alloc).unwrap_or_else(|e| fail(&format!("{}: {e}", dir.display())));
    Side { origin, dir: dir.to_path_buf(), lock, units, packs: BTreeMap::new(), folders: BTreeMap::new() }
}

/// The package a need names: a vendored copy under `library/vendor`, or a registry crate's
/// pack made from the `.crate` the lock pins.
fn answer(side: &mut Side, id: LockId, registry: &Path, alloc: Alloc<'static>) -> Package<'static> {
    let p = side.lock.package(id);
    let (name, version) = (p.name.as_str(), p.version.to_string());
    let what = format!("{name} {version} ({})", side.dir.join("Cargo.lock").display());
    match (&p.source, side.origin) {
        (LockSource::Registry { .. }, Origin::Library) => {
            let dir = side.dir.join("vendor").join(format!("{name}-{version}"));
            let units = layout(&Files::Disk(dir.clone()), "", alloc).unwrap_or_else(|e| fail(&format!("{what}: {}: {e}", dir.display())));
            member_named(&units, name, alloc).unwrap_or_else(|| fail(&format!("{what}: {} holds no package `{name}`", dir.display())))
        }
        (LockSource::Registry { .. }, Origin::Workspace) => {
            let Checksum::Sha256(hex) = &p.checksum else { fail(&format!("{what}: the lock has no checksum")) };
            let Some(sha256) = parse_hex(hex.as_str().as_bytes()) else { fail(&format!("{what}: a checksum that is not 64 hex digits")) };
            let file = registry.join(format!("{name}-{version}.crate"));
            let archive = std::fs::read(&file).unwrap_or_else(|e| fail(&format!("{what}: {}: {e}", file.display())));
            let found = blaze_fetch::Digest::of(blaze_fetch::Algo::Sha256, &archive);
            if found.bytes() != sha256 {
                fail(&format!("{what}: {} is not the `.crate` the lock pins (its SHA-256 is {})", file.display(), found.hex()));
            }
            let (pack, _) = unpack::crate_record(name, &version, sha256, &archive, &UnpackLimits::REAL, alloc)
                .unwrap_or_else(|e| fail(&format!("{what}: {}", blaze_text::Shown(&e))));
            let pack: &'static [u8] = pack.leak_static();
            let view = PackView::open_verified(pack).unwrap_or_else(|e| fail(&format!("{what}: {}", blaze_text::Shown(&e))));
            let units = layout(&Files::Pack(view), "", alloc).unwrap_or_else(|e| fail(&format!("{what}: {e}")));
            side.packs.insert(id, pack);
            member_named(&units, name, alloc).unwrap_or_else(|| fail(&format!("{what}: its `.crate` holds no package `{name}`")))
        }
        (LockSource::Git { .. }, _) => fail(&format!("{what}: a git package beneath the toolchain; the toolchain takes registry and path packages only")),
        (LockSource::Path, _) => {
            // A path package that is no member (a `[patch]`, or a path dependency of one): the
            // one folder under the tree whose manifest is that package.
            let Some(dir) = patched(&side.dir, name, alloc).or_else(|| package_folder(&side.dir, name, alloc)) else {
                fail(&format!("{what}: a path package outside its workspace's layout, its `[patch]` tables and its folders"))
            };
            // Read anchored at its own folder: a patch is no member of the root's workspace.
            let units = layout(&Files::Disk(side.dir.join(&dir)), "", alloc).unwrap_or_else(|e| fail(&format!("{what}: {dir}: {e}")));
            side.folders.insert(id, dir.clone());
            member_named(&units, name, alloc).unwrap_or_else(|| fail(&format!("{what}: {dir} holds no package `{name}`")))
        }
    }
}

/// The one folder under `root` (outside `vendor`, `target` and dot folders) whose `Cargo.toml`
/// has `[package] name = "<name>"`; two such folders stop the tool.
fn package_folder(root: &Path, name: &str, alloc: Alloc<'_>) -> Option<String> {
    let mut found = Vec::new();
    let mut stack = vec![String::new()];
    while let Some(dir) = stack.pop() {
        let at = root.join(&dir);
        let manifest = at.join("Cargo.toml");
        if let Ok(text) = std::fs::read_to_string(&manifest)
            && let Ok(doc) = blaze_toml::Document::parse_in(&text, blaze_toml::Limits::CARGO, alloc)
            && doc.root().get("package").and_then(|p| p.get("name")).and_then(|n| n.as_str()) == Some(name)
        {
            found.push(dir.clone());
        }
        let Ok(listed) = std::fs::read_dir(&at) else { continue };
        for entry in listed.flatten() {
            let n = entry.file_name().to_string_lossy().into_owned();
            if entry.file_type().is_ok_and(|t| t.is_dir()) && !n.starts_with('.') && n != "target" && !(dir.is_empty() && n == "vendor") {
                stack.push(if dir.is_empty() { n } else { format!("{dir}/{n}") });
            }
        }
    }
    found.sort();
    match found.as_slice() {
        [] => None,
        [one] => Some(one.clone()),
        more => fail(&format!("{}: {} folders hold a package `{name}`: {}", root.display(), more.len(), more.join(", "))),
    }
}

/// The folder (relative to `root`) a `[patch.<source>]` table of `root/Cargo.toml` gives `name`.
fn patched(root: &Path, name: &str, alloc: Alloc<'_>) -> Option<String> {
    let path = root.join("Cargo.toml");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| fail(&format!("{}: {e}", path.display())));
    let doc = blaze_toml::Document::parse_in(&text, blaze_toml::Limits::CARGO, alloc).unwrap_or_else(|e| fail(&format!("{}: {}", path.display(), e.display(&text))));
    let patches = doc.root().get("patch")?;
    let found = patches.children().find_map(|source| source.get(name)?.get("path")?.as_str().map(str::to_string));
    found.map(|dir| dir.trim_start_matches("./").trim_end_matches('/').to_string())
}

fn member_named(units: &Units<'static>, name: &str, alloc: Alloc<'static>) -> Option<Package<'static>> {
    units.members.iter().find(|m| m.name.as_str() == name).map(|m| Package::from_member(m, alloc))
}

/// The features asked of `sysroot` beyond its default (`panic-unwind`): those rustc's own build of
/// the prebuilt std turns on for every target (bootstrap's `std_features`: `backtrace` while
/// `rust.backtrace` is on, its default). std has no `default` feature of its own, so only an ask
/// turns `backtrace` on; it is `std/backtrace`, whose `addr2line/rustc-dep-of-std`,
/// `object/rustc-dep-of-std` and `miniz_oxide/rustc-dep-of-std` turn on those optional
/// dependencies (and through them `gimli` and `adler2`). Without it std's
/// `backtrace_rs::symbolize::gimli` names `addr2line`, which its extern prelude lacks.
/// `compiler-builtins-c` is left off: it compiles C, which the cut does not.
const SYSROOT_FEATURES: &[&str] = &["backtrace"];

/// `root`'s unit graph on `side` for `target`, with `features` asked of it besides its
/// default ones: what the sysroot's part from that tree is.
fn resolve(side: &mut Side, root: &str, features: &[&str], target: &'static TargetDesc<'static>, registry: &Path, alloc: Alloc<'static>) -> UnitGraph<'static> {
    let written = side.units.resolver.as_ref().map(|(r, _)| r.as_str());
    let resolver = Resolver::of(written, side.units.root_edition).unwrap_or_else(|_| fail(&format!("{}: an unknown resolver {written:?}", side.dir.display())));
    let lock = side.lock;
    let mut roots = AVec::new_in(alloc);
    let mut members = Vec::new();
    for m in side.units.members.iter() {
        let Some(id) = lock.named(&m.name).find(|id| matches!(lock.package(*id).source, LockSource::Path)) else { continue };
        if m.name.as_str() == root {
            roots.push(id);
        }
        members.push((id, Package::from_member(m, alloc)));
    }
    if roots.is_empty() {
        fail(&format!("{}: no member `{root}` in its lock", side.dir.display()));
    }
    let mut asked = AVec::new_in(alloc);
    for f in features {
        asked.push(blaze_own::String::from_str_in(f, alloc));
    }
    let ask: &'static Ask<'static> = Box::leak(Box::new(Ask { roots, features: asked, all_features: false, no_default_features: false, tests: false }));
    let mut res = Resolution::new(lock, target, target, ask, resolver, alloc);
    for (id, p) in members {
        res.member(id, p).unwrap_or_else(|e| fail(&format!("{}: {e}", side.dir.display())));
    }
    for _ in 0..64 {
        match res.pass().unwrap_or_else(|e| fail(&format!("{}: {e}", side.dir.display()))) {
            Pass::Done(graph) => return graph,
            // A package the graph reaches has no manifest: the cut cannot be made (the graph
            // offers no units then; blaze-unit-graph `PartialGraph`).
            Pass::Partial(partial) => {
                let p = lock.package(partial.missing()[0]);
                fail(&format!("{}: no manifest for {} {} ({} package(s) missing in all)", side.dir.display(), p.name, p.version, partial.missing().len()));
            }
            Pass::Need(needs) => {
                for id in needs.iter() {
                    let package = answer(side, *id, registry, alloc);
                    res.package(*id, Some(package)).unwrap_or_else(|e| fail(&format!("{}: {e}", side.dir.display())));
                }
            }
        }
    }
    fail(&format!("{}: no unit graph after 64 passes", side.dir.display()))
}

/// Every file under `dir` (relative to `top`), skipping dot folders and `target`: (path, bytes, mode).
fn walk(top: &Path, dir: &str, out: &mut Vec<(String, Vec<u8>, Mode)>) {
    let at = top.join(dir);
    let mut listed: Vec<_> = std::fs::read_dir(&at).unwrap_or_else(|e| fail(&format!("{}: {e}", at.display()))).flatten().collect();
    listed.sort_by_key(|e| e.file_name());
    for entry in listed {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = if dir.is_empty() { name.clone() } else { format!("{dir}/{name}") };
        let kind = entry.file_type().unwrap_or_else(|e| fail(&format!("{}: {e}", entry.path().display())));
        if kind.is_dir() {
            if name.starts_with('.') || name == "target" {
                continue;
            }
            walk(top, &path, out);
        } else if kind.is_symlink() {
            let target = std::fs::read_link(entry.path()).unwrap_or_else(|e| fail(&format!("{}: {e}", entry.path().display())));
            out.push((path, target.to_string_lossy().into_owned().into_bytes(), Mode::Link));
        } else {
            let bytes = std::fs::read(entry.path()).unwrap_or_else(|e| fail(&format!("{}: {e}", entry.path().display())));
            let exec = std::os::unix::fs::PermissionsExt::mode(&entry.metadata().unwrap_or_else(|e| fail(&format!("{}: {e}", entry.path().display()))).permissions()) & 0o111 != 0;
            out.push((path, bytes, if exec { Mode::Exec } else { Mode::File }));
        }
    }
}

/// A tree crate's files: its folder `dir` under `top`, and what its sources reach outside it,
/// found by their text: a `#[path = ".."]` module brings the package folder holding its file (a
/// module tree: core's `stdarch/crates/core_arch`), an `include!`, `include_str!` or
/// `include_bytes!` brings its one file (std's `core/src/primitive_docs.rs`). What is brought is
/// read the same way, to a fixed point. Paths are relative to `top`; sorted, each once.
fn crate_files(top: &Path, dir: &str) -> Vec<(String, Vec<u8>, Mode)> {
    let mut files: BTreeMap<String, (Vec<u8>, Mode)> = BTreeMap::new();
    let mut folders = vec![dir.to_string()];
    let mut singles: Vec<String> = Vec::new();
    let mut done_folders: Vec<String> = Vec::new();
    while !folders.is_empty() || !singles.is_empty() {
        let mut fresh: Vec<String> = Vec::new();
        for folder in std::mem::take(&mut folders) {
            if done_folders.iter().any(|done| within(&folder, done)) {
                continue;
            }
            done_folders.push(folder.clone());
            let mut found = Vec::new();
            walk(top, &folder, &mut found);
            for (path, bytes, mode) in found {
                fresh.push(path.clone());
                files.insert(path, (bytes, mode));
            }
        }
        for single in std::mem::take(&mut singles) {
            if files.contains_key(&single) {
                continue;
            }
            let Ok(bytes) = std::fs::read(top.join(&single)) else { continue };
            files.insert(single.clone(), (bytes, Mode::File));
            fresh.push(single);
        }
        for path in fresh.iter().filter(|p| p.ends_with(".rs")) {
            let text = String::from_utf8_lossy(&files[path].0).into_owned();
            let here = path.rsplit_once('/').map_or("", |(d, _)| d);
            for (attr, target) in reaches(&text) {
                let Some(joined) = join(here, target) else { continue };
                if done_folders.iter().any(|done| within(&joined, done)) {
                    continue;
                }
                if attr {
                    if let Some(package) = package_of(top, &joined) {
                        folders.push(package);
                    }
                } else {
                    singles.push(joined);
                }
            }
        }
    }
    files.into_iter().map(|(path, (bytes, mode))| (path, bytes, mode)).collect()
}

/// Whether `path` is `folder` or beneath it.
fn within(path: &str, folder: &str) -> bool {
    folder.is_empty() || path == folder || path.strip_prefix(folder).is_some_and(|rest| rest.starts_with('/'))
}

/// `here/target` with `.` and `..` resolved; `None` above the top.
fn join(here: &str, target: &str) -> Option<String> {
    let mut parts: Vec<&str> = here.split('/').filter(|p| !p.is_empty()).collect();
    for part in target.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            _ => parts.push(part),
        }
    }
    Some(parts.join("/"))
}

/// The nearest folder at or above `path`'s holding a `Cargo.toml`, under `top`.
fn package_of(top: &Path, path: &str) -> Option<String> {
    let mut at = path.rsplit_once('/').map(|(d, _)| d.to_string());
    while let Some(dir) = at {
        if top.join(&dir).join("Cargo.toml").is_file() {
            return Some(dir);
        }
        at = dir.rsplit_once('/').map(|(d, _)| d.to_string());
    }
    None
}

/// The string literals of a file's `#[path = "…"]` (true) and `include*!("…")` (false).
fn reaches(text: &str) -> Vec<(bool, &str)> {
    let mut out = Vec::new();
    for (opener, attr) in [("#[path = \"", true), ("include!(\"", false), ("include_str!(\"", false), ("include_bytes!(\"", false)] {
        let mut rest = text;
        while let Some(at) = rest.find(opener) {
            rest = &rest[at + opener.len()..];
            if let Some(end) = rest.find('"') {
                out.push((attr, &rest[..end]));
            }
        }
    }
    out
}

/// A kind-3 pack of `files`: its source digest is its own listing's BLAKE3.
fn tree_pack(what: &str, files: &[(String, Vec<u8>, Mode)], alloc: Alloc<'static>) -> &'static [u8] {
    let mut entries: Vec<TreeEntry> =
        files.iter().map(|(path, bytes, mode)| TreeEntry { path: path.clone(), kind: *mode, blake3: Blake3::of(bytes), len: bytes.len() as u64 }).collect();
    let listing = tree::listing(&mut entries).unwrap_or_else(|e| fail(&format!("{what}: {e:?}")));
    let source = Blake3::of(&listing).0;
    let data: u64 = files.iter().map(|(_, b, _)| b.len() as u64).sum();
    let limits = PackLimits { most_files: 200_000, most_bytes: 1 << 30, most_path: 4096 };
    let mut builder = Builder::new(Kind::Tree, source, limits, data, alloc).unwrap_or_else(|e| fail(&format!("{what}: {}", blaze_text::Shown(&e))));
    for (path, bytes, mode) in files {
        builder.file(path, *mode, bytes).unwrap_or_else(|e| fail(&format!("{what}: {path}: {}", blaze_text::Shown(&e))));
    }
    builder.finish().unwrap_or_else(|e| fail(&format!("{what}: {}", blaze_text::Shown(&e)))).leak_static()
}

/// A crate's key across targets: its tree and its lock id there.
type CrateKey = (Origin, LockId);

fn role_of(origin: Origin, name: &str, source: &LockSource<'_>) -> Role {
    match (origin, name, source) {
        (Origin::Library, "core", _) => Role::Core,
        (Origin::Library, "alloc", _) => Role::Alloc,
        (Origin::Library, "std", _) => Role::Std,
        (Origin::Library, "proc_macro", _) => Role::ProcMacro,
        (Origin::Library, _, _) => Role::Library,
        (Origin::Workspace, "blaze-std", _) => Role::Bstd,
        (Origin::Workspace, _, LockSource::Registry { .. }) => Role::Registry,
        (Origin::Workspace, _, _) => Role::Blaze,
    }
}

fn kind_of(kind: &GraphKind<'_>) -> Option<UnitKind> {
    match kind {
        GraphKind::Lib => Some(UnitKind::Lib),
        GraphKind::ProcMacro => Some(UnitKind::ProcMacro),
        GraphKind::BuildScript => Some(UnitKind::BuildScript),
        GraphKind::Bin(_)
        | GraphKind::Test { .. }
        | GraphKind::IntegrationTest(_)
        | GraphKind::Example(_)
        | GraphKind::Bench(_) => None,
    }
}

fn edition_text(edition: edition::PackageEdition) -> String {
    match edition.blaze {
        Some(blaze) => blaze.manifest().to_string(),
        None => edition.syntax.to_string(),
    }
}

/// A unit as the record holds it, before crate indices are known.
struct CutUnit {
    krate: CrateKey,
    kind: UnitKind,
    for_host: bool,
    root: String,
    features: Vec<String>,
    deps: Vec<(String, u32)>,
    /// Read from its root's text once the packs are made (`main`).
    sysroot: Option<ImplicitSysroot>,
}

/// One target's units: library's, then the workspace's, dependency indices made one list's.
fn cut_target(graphs: &[(Origin, &UnitGraph<'static>, &Side)], facts: &mut BTreeMap<CrateKey, (String, String, String, String, String, String)>) -> Vec<CutUnit> {
    let mut out = Vec::new();
    for (origin, graph, side) in graphs {
        let base = out.len() as u32;
        // Old unit id -> new index, for the units kept.
        let mut index = vec![None; graph.units.len()];
        let mut kept = 0u32;
        for (at, u) in graph.units.iter().enumerate() {
            if kind_of(&u.kind).is_some() {
                index[at] = Some(base + kept);
                kept += 1;
            }
        }
        for u in graph.units.iter() {
            let Some(kind) = kind_of(&u.kind) else { continue };
            let key = (*origin, u.package);
            let p = side.lock.package(u.package);
            let member = side.units.members.iter().find(|m| m.name.as_str() == p.name.as_str());
            let dir = match (origin, &p.source) {
                (Origin::Library, LockSource::Registry { .. }) => format!("vendor/{}-{}", p.name, p.version),
                (Origin::Workspace, LockSource::Registry { .. }) => String::new(),
                _ => member.map(|m| m.dir.as_str().to_string()).or_else(|| side.folders.get(&u.package).cloned()).unwrap_or_else(|| fail(&format!("{} {}: a path package with no folder", p.name, p.version))),
            };
            if kind == UnitKind::Lib || kind == UnitKind::ProcMacro {
                facts.insert(key, (u.name.to_string(), u.version.to_string(), edition_text(u.edition), u.crate_name.to_string(), u.root.to_string(), dir));
            } else {
                facts.entry(key).or_insert_with(|| (u.name.to_string(), u.version.to_string(), edition_text(u.edition), String::new(), String::new(), dir));
            }
            let deps = u
                .deps
                .iter()
                .map(|d| (d.name.to_string(), index[d.unit.index()].unwrap_or_else(|| fail(&format!("{}: a dependency on a unit that is no library", u.name)))))
                .collect();
            out.push(CutUnit {
                krate: key,
                kind,
                for_host: u.for_host,
                root: u.root.to_string(),
                features: u.features.iter().map(|f| f.to_string()).collect(),
                deps,
                sysroot: None,
            });
        }
    }
    out
}

/// `bytes` on the shelf under its digest: written to a temporary name and renamed, or, when a
/// file of that name is there, read and compared.
fn place(shelf: &Path, digest: &[u8; 32], bytes: &[u8]) -> bool {
    let name = Hex::of(digest);
    let path = shelf.join(name.as_str());
    if let Ok(there) = std::fs::read(&path) {
        if there != bytes {
            fail(&format!("{}: a file under this digest holds other bytes; remove it", path.display()));
        }
        return false;
    }
    let temporary = shelf.join(format!(".{name}.{}", std::process::id()));
    std::fs::write(&temporary, bytes).unwrap_or_else(|e| fail(&format!("{}: {e}", temporary.display())));
    std::fs::rename(&temporary, &path).unwrap_or_else(|e| fail(&format!("{}: {e}", path.display())));
    true
}

/// D174: the blaze-cfront helper of the unpacked blaze LLVM prefix `prefix` on the shelf, as
/// `<shelf>/cfront/<sha256>/blaze-cfront` (mode 0755). The prefix's `blaze-llvm.json` names its
/// target; `pins/cfront.json` pins the helper's sha256 for that target, which the bytes must
/// have. Written to a temporary name and renamed; a helper already there is compared.
fn place_cfront(tree: &Path, shelf: &Path, prefix: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let json = |path: &Path| -> serde_json::Value {
        let text = std::fs::read_to_string(path).unwrap_or_else(|e| fail(&format!("{}: {e}", path.display())));
        serde_json::from_str(&text).unwrap_or_else(|e| fail(&format!("{}: {e}", path.display())))
    };
    let marker = prefix.join("blaze-llvm.json");
    let built = json(&marker);
    let Some(target) = built["target"].as_str() else {
        fail(&format!("{}: no `target`: not a blaze LLVM prefix", marker.display()))
    };
    let pin_path = tree.join("pins/cfront.json");
    let pin = json(&pin_path);
    let Some(member) = pin["member"].as_str() else { fail(&format!("{}: no `member`", pin_path.display())) };
    let Some(want) = pin["targets"][target]["sha256"].as_str() else {
        fail(&format!(
            "{} pins no helper for {target}: build the LLVM archive with the llvm workflow, then `llvm.py pin`",
            pin_path.display()
        ))
    };
    let from = prefix.join(member);
    let bytes = std::fs::read(&from).unwrap_or_else(|e| fail(&format!("{}: {e}", from.display())));
    let got = Hex::of(&blaze_sha256::digest(&bytes));
    if got.as_str() != want {
        fail(&format!("{}: sha256 {got}, {} pins {want} for {target}", from.display(), pin_path.display()));
    }
    let dir = shelf.join("cfront").join(want);
    std::fs::create_dir_all(&dir).unwrap_or_else(|e| fail(&format!("{}: {e}", dir.display())));
    let path = dir.join("blaze-cfront");
    if let Ok(there) = std::fs::read(&path) {
        if there != bytes {
            fail(&format!("{}: a helper under this digest holds other bytes; remove it", path.display()));
        }
        println!("cfront        {} (already there)", path.display());
        return;
    }
    let temporary = dir.join(format!(".blaze-cfront.{}", std::process::id()));
    std::fs::write(&temporary, &bytes).unwrap_or_else(|e| fail(&format!("{}: {e}", temporary.display())));
    std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o755))
        .unwrap_or_else(|e| fail(&format!("{}: {e}", temporary.display())));
    std::fs::rename(&temporary, &path).unwrap_or_else(|e| fail(&format!("{}: {e}", path.display())));
    println!("cfront        {}", path.display());
}

fn main() {
    let argument = std::env::args().nth(1).unwrap_or_else(|| fail("usage: toolchain_cut <json>"));
    let args: Args = input::parse(&argument, SCRIPT);
    // An entry that takes no grant yet (a plain `fn main`): the run's memory is one frame
    // taken from the OS here, kept for the process's life since the graphs below are
    // `'static`, and handed down.
    let frame: &'static blaze_mem::Frame =
        Box::leak(Box::new(blaze_mem::Frame::with_reserve(1 << 36, blaze_mem::Tag::Other).unwrap_or_else(|e| fail(&format!("the entry's frame: {e:?}")))));
    let erased: &'static blaze_own::Erased<&'static blaze_mem::Frame> = Box::leak(Box::new(blaze_own::Erased::new(frame)));
    let alloc: Alloc<'static> = erased.handle();
    let started = Instant::now();
    let tree = PathBuf::from(&args.tree);
    let registry = PathBuf::from(&args.registry);
    let mut library = side(Origin::Library, &tree.join("library"), alloc);
    let mut workspace = side(Origin::Workspace, &tree, alloc);

    // Per target: the two graphs, cut into one list of units.
    let mut facts = BTreeMap::new();
    let mut cut: Vec<(String, Vec<CutUnit>)> = Vec::new();
    let mut target_cfgs: Vec<Vec<(String, Option<String>)>> = Vec::new();
    let recorded = recorded_targets(&tree);
    let recorded: Vec<&str> = recorded.iter().map(String::as_str).collect();
    let asked: Vec<SideTriple<'_, TargetSide>> = args.targets.iter().map(|t| SideTriple::target(t.as_str())).collect();
    let triples = cut_triples(&asked, args.macro_side.then(SideTriple::macros), &recorded, alloc).unwrap_or_else(|e| fail(&e.to_string()));
    for triple in triples.iter().copied() {
        let (desc, atoms) = target_desc(&tree, triple, alloc);
        target_cfgs.push(atoms);
        let target: &'static TargetDesc<'static> = Box::leak(Box::new(desc));
        let lib_graph = resolve(&mut library, "sysroot", SYSROOT_FEATURES, target, &registry, alloc);
        let ws_graph = resolve(&mut workspace, "blaze-std", &[], target, &registry, alloc);
        let units = cut_target(&[(Origin::Library, &lib_graph, &library), (Origin::Workspace, &ws_graph, &workspace)], &mut facts);
        cut.push((triple.to_string(), units));
    }
    let resolved = started.elapsed();

    // The packs, one per crate: a registry crate's from its `.crate`; a tree crate's folder,
    // with what its sources reach outside it (`crate_files`), at paths relative to its tree.
    let mut tree_packs: BTreeMap<CrateKey, &'static [u8]> = BTreeMap::new();
    for (key, facts) in &facts {
        if key.0 == Origin::Workspace && workspace.packs.contains_key(&key.1) {
            continue;
        }
        let top = match key.0 {
            Origin::Library => &library.dir,
            Origin::Workspace => &tree,
        };
        let files = crate_files(top, &facts.5);
        tree_packs.insert(*key, tree_pack(&format!("{} {} ({})", facts.0, facts.1, facts.5), &files, alloc));
    }
    let pack_of = |key: &CrateKey| -> (&'static [u8], u8) {
        match (key.0, workspace.packs.get(&key.1)) {
            (Origin::Workspace, Some(pack)) => (pack, 1),
            _ => (tree_packs[key], 3),
        }
    };

    // The crates, in key order; then each target's units by crate index.
    let keys: Vec<CrateKey> = facts.keys().copied().collect();
    let crates: Vec<CrateIn<'_>> = facts
        .iter()
        .map(|(key, (name, version, edition, lib, root, dir))| {
            let (pack, pack_kind) = pack_of(key);
            let source = match key.0 {
                Origin::Library => &library.lock.package(key.1).source,
                Origin::Workspace => &workspace.lock.package(key.1).source,
            };
            CrateIn {
                name,
                version,
                edition,
                lib,
                root,
                dir,
                role: role_of(key.0, name, source),
                pack_kind,
                pack_len: pack.len() as u64,
                pack: pack[pack.len() - 32..].try_into().expect("a pack's footer"),
            }
        })
        .collect();
    let index_of = |key: &CrateKey| keys.binary_search(key).expect("every unit's crate has facts") as u32;
    // The implicit sysroot, a fact of each unit's root text (blaze_toolchain::implied): its
    // value on the unit's row, and the sysroot crates a target-side library names as real
    // edges, so a reader opens Rust's std exactly when a crate of its closure names it.
    let mut opened: BTreeMap<CrateKey, PackView<'static>> = BTreeMap::new();
    for (t, (triple, units)) in cut.iter_mut().enumerate() {
        let role_at = |role: Role| units.iter().position(|u| u.kind == UnitKind::Lib && !u.for_host && crates[index_of(&u.krate) as usize].role == role).map(|at| at as u32);
        let (Some(core), Some(alloc_unit), Some(std_unit)) = (role_at(Role::Core), role_at(Role::Alloc), role_at(Role::Std)) else {
            fail(&format!("{triple}: its sysroot graph has no core, alloc or std library unit"));
        };
        let sys = SysrootUnits { core, alloc: alloc_unit, std: std_unit };
        for u in units.iter_mut() {
            let key = u.krate;
            let (name, version, dir) = (&facts[&key].0, &facts[&key].1, &facts[&key].5);
            let view = *opened.entry(key).or_insert_with(|| {
                let (pack, _) = pack_of(&key);
                PackView::open_verified(pack).unwrap_or_else(|e| fail(&format!("{name} {version}: {}", blaze_text::Shown(&e))))
            });
            let path = if dir.is_empty() || u.root.starts_with(dir.as_str()) { u.root.clone() } else { format!("{dir}/{}", u.root) };
            let mut cfg: Vec<(&str, Option<&str>)> = target_cfgs[t].iter().map(|(n, v)| (n.as_str(), v.as_deref())).collect();
            cfg.extend(u.features.iter().map(|f| ("feature", Some(f.as_str()))));
            // Every module file of the crate, from its root (an `extern crate` asks for its
            // crate in any module: bstd's `extern crate std;` is in `mod rt`).
            let file = |p: &str| view.find(p).and_then(|f| std::str::from_utf8(f.bytes).ok());
            let read = crate_facts(&path, file, &cfg, alloc).unwrap_or_else(|e| fail(&format!("{triple}: {name} {version} (root {path}): {e}")));
            u.sysroot = Some(read.sysroot);
            if u.kind == UnitKind::Lib && !u.for_host {
                let explicit: Vec<String> = u.deps.iter().map(|(n, _)| n.clone()).collect();
                for (edge, unit) in implied_edges(&read, sys, |n| explicit.iter().any(|e| e == n)).into_iter().flatten() {
                    u.deps.push((edge.to_string(), unit));
                }
            }
        }
        let deps: Vec<Vec<u32>> = units.iter().map(|u| u.deps.iter().map(|(_, d)| *d).collect()).collect();
        let slices: Vec<&[u32]> = deps.iter().map(Vec::as_slice).collect();
        if let Some(at) = first_in_cycle(&slices, alloc).unwrap_or_else(|e| fail(&e.to_string())) {
            let key = units[at].krate;
            fail(&format!("{triple}: {} {} is on or above a dependency cycle once each crate's implied sysroot edges are added", facts[&key].0, facts[&key].1));
        }
    }
    let unit_rows: Vec<(String, Vec<Vec<&str>>, Vec<Vec<(&str, u32)>>)> = cut
        .iter()
        .map(|(triple, units)| {
            (
                triple.clone(),
                units.iter().map(|u| u.features.iter().map(String::as_str).collect()).collect(),
                units.iter().map(|u| u.deps.iter().map(|(n, i)| (n.as_str(), *i)).collect()).collect(),
            )
        })
        .collect();
    let targets_units: Vec<Vec<UnitIn<'_>>> = cut
        .iter()
        .zip(&unit_rows)
        .map(|((_, units), (_, features, deps))| {
            units
                .iter()
                .enumerate()
                .map(|(at, u)| UnitIn {
                    krate: index_of(&u.krate),
                    kind: u.kind,
                    for_host: u.for_host,
                    root: &u.root,
                    features: &features[at],
                    deps: &deps[at],
                    sysroot: u.sysroot.expect("read for every unit above"),
                })
                .collect()
        })
        .collect();
    let cfg_rows: Vec<Vec<(&str, Option<&str>)>> =
        target_cfgs.iter().map(|atoms| atoms.iter().map(|(name, value)| (name.as_str(), value.as_deref())).collect()).collect();
    let targets: Vec<TargetIn<'_>> =
        cut.iter().zip(&targets_units).zip(&cfg_rows).map(|(((triple, _), units), cfg)| TargetIn { triple, units, cfg }).collect();
    let manifest = record::write(&crates, &targets, alloc).unwrap_or_else(|e| fail(&e.to_string()));
    let id = ToolchainView::id_of_bytes(&manifest);
    ToolchainView::open(&manifest, id).unwrap_or_else(|e| fail(&format!("the manifest just written: {e}")));

    // Every distinct pack, once.
    let mut packs: BTreeMap<[u8; 32], &[u8]> = BTreeMap::new();
    for key in &keys {
        let (pack, _) = pack_of(key);
        packs.insert(pack[pack.len() - 32..].try_into().expect("a pack's footer"), pack);
    }
    let pack_bytes: usize = packs.values().map(|p| p.len()).sum();
    println!("toolchain     {id}");
    println!("crates        {} ({} from library/, {} from the workspace)", crates.len(), keys.iter().filter(|k| k.0 == Origin::Library).count(), keys.iter().filter(|k| k.0 == Origin::Workspace).count());
    // Read back from the record as written: what a reader of it sees.
    let view = ToolchainView::open(&manifest, id).unwrap_or_else(|e| fail(&format!("the manifest just written: {e}")));
    for triple in view.triples() {
        let units = view.units(triple).map_or(0, Iterator::count);
        let cfg: Vec<String> = view
            .cfg(triple)
            .into_iter()
            .flatten()
            .filter(|(name, _)| matches!(*name, "target_os" | "target_arch" | "panic"))
            .map(|(name, value)| format!("{name}={}", value.unwrap_or("")))
            .collect();
        println!("target        {triple}: {units} units, {} cfg atoms ({})", view.cfg(triple).map_or(0, Iterator::count), cfg.join(" "));
    }
    println!("packs         {} ({:.1} MiB), manifest {} bytes", packs.len(), pack_bytes as f64 / 1048576.0, manifest.len());
    println!("time          {:.0} ms resolving, {:.0} ms in all", resolved.as_secs_f64() * 1e3, started.elapsed().as_secs_f64() * 1e3);

    let pinned_path = tree.join("crates/blaze-toolchain/PINNED");
    if args.check {
        let pinned = std::fs::read(&pinned_path).unwrap_or_else(|e| fail(&format!("{}: {e}", pinned_path.display())));
        if parse_hex(&pinned) != Some(id.0) {
            fail(&format!("{} is not {id}: run toolchain_cut with \"write_pin\": true", pinned_path.display()));
        }
        println!("check         {} is the tree's toolchain", pinned_path.display());
        return;
    }
    let shelf = PathBuf::from(&args.shelf);
    std::fs::create_dir_all(&shelf).unwrap_or_else(|e| fail(&format!("{}: {e}", shelf.display())));
    let mut written = 0;
    for (digest, pack) in &packs {
        written += usize::from(place(&shelf, digest, pack));
    }
    written += usize::from(place(&shelf, &id.0, &manifest));
    println!("shelf         {}: {written} files written, {} already there", shelf.display(), packs.len() + 1 - written);
    if !args.cfront.is_empty() {
        place_cfront(&tree, &shelf, Path::new(&args.cfront));
    }
    if args.write_pin {
        std::fs::write(&pinned_path, format!("{id}\n")).unwrap_or_else(|e| fail(&format!("{}: {e}", pinned_path.display())));
        println!("pinned        {}", pinned_path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tree's `library/` resolved as the cut resolves it, for `triple`, with `features`
    /// asked of `sysroot`: std's target library unit's dependency names, and every unit's
    /// package name.
    fn std_deps(features: &[&str], triple: &str) -> (Vec<String>, Vec<String>) {
        // The test's one frame (memory from the OS), kept for the process's life since the
        // graph is `'static`.
        let frame: &'static blaze_mem::Frame = Box::leak(Box::new(blaze_mem::Frame::with_reserve(1 << 34, blaze_mem::Tag::Other).expect("the test's frame")));
        let erased: &'static blaze_own::Erased<&'static blaze_mem::Frame> = Box::leak(Box::new(blaze_own::Erased::new(frame)));
        let alloc: Alloc<'static> = erased.handle();
        let tree = Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("scripts/ is a folder of the tree");
        let mut library = side(Origin::Library, &tree.join("library"), alloc);
        let (desc, _) = target_desc(tree, triple, alloc);
        let target: &'static TargetDesc<'static> = Box::leak(Box::new(desc));
        // The library side reads its registry crates from `library/vendor`, never the registry.
        let graph = resolve(&mut library, "sysroot", features, target, Path::new("/registry-unused"), alloc);
        let std = graph
            .units
            .iter()
            .find(|u| u.name.as_str() == "std" && matches!(u.kind, GraphKind::Lib) && !u.for_host)
            .expect("the sysroot graph has std's library unit");
        let deps = std.deps.iter().map(|d| d.name.to_string()).collect();
        let names = graph.units.iter().map(|u| u.name.to_string()).collect();
        (deps, names)
    }

    /// The cut's ask turns on std's `backtrace`, so std's library unit depends on `addr2line`,
    /// `miniz_oxide` and `object`, and `gimli` and `adler2` are in the graph; the default ask
    /// alone (the cause removed) has none of them. Prints, per triple,
    /// `<triple>: std -> addr2line, miniz_oxide, object; gimli, adler2 in the graph; none without backtrace`.
    #[test]
    fn sysroot_ask_brings_std_backtrace_crates() {
        for triple in ["aarch64-apple-darwin", "aarch64-unknown-linux-gnu", "wasm32-wasip1"] {
            let (deps, names) = std_deps(SYSROOT_FEATURES, triple);
            for dep in ["addr2line", "miniz_oxide", "object"] {
                assert!(deps.iter().any(|d| d == dep), "{triple}: std has no dependency `{dep}`: {deps:?}");
            }
            for unit in ["gimli", "adler2"] {
                assert!(names.iter().any(|n| n == unit), "{triple}: no `{unit}` unit: {names:?}");
            }
            let (bare, _) = std_deps(&[], triple);
            assert!(!bare.iter().any(|d| d == "addr2line"), "{triple}: std has `addr2line` with `backtrace` off: {bare:?}");
            println!("{triple}: std -> addr2line, miniz_oxide, object; gimli, adler2 in the graph; none without backtrace");
        }
    }
}
