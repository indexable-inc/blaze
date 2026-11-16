//! A cargo workspace's crates for [`resolve_crates`](crate::resolve::resolve_crates)
//! (docs/v2/plan/design/bottom.md, section 6): the units blaze-unit-graph found, and the
//! toolchain's library units their `std`, `core` and `alloc` are, turned into one crate list.
//! Sans-IO: the caller reads the graph, the lock and the toolchain manifest, and serves the
//! texts at the paths given here.
//!
//! Which units: the target-side `Lib` and `Bin` units the workspace members reach, and the
//! `ProcMacro` units among them (a proc macro is a crate of the graph, since its macros are
//! what its dependents' paths name; what it is built from is the host's, so its dependencies
//! are not followed). Never a `Test` or a `BuildScript` unit. A kept unit depending on a unit
//! not kept is [`UnitsError::DependencyNotKept`], naming both.
//!
//! The toolchain's units are the closure of `std`, `core` and `alloc` (and bstd, when a kept
//! unit is of a blaze edition) over the target side's dependencies, as blaze-ladder's
//! crates.rs takes them.
//!
//! Paths. Each crate's files are under a path of its own, never the disk's: a workspace unit's
//! under `/package/<lock index>/` followed by the file's path in its package (every unit of a
//! package shares it), a toolchain unit's under `/toolchain/<unit>/` followed by its path in
//! its crate's pack. [`Origin`] says which, for the caller's `SourceTexts`.
//!
//! Keys. A crate's [`CrateItemKey`] is from content alone (rules 21 and 25), never a path: a
//! workspace unit's package name, version and source as the lock writes them, its kind (and
//! binary name), its side and its features; a toolchain unit's pack digest and features.
//!
//! cfg. A workspace unit's cfg is [`Unit::cfg`] as is. A unit with a build script is counted
//! apart ([`UnitCrates::build_scripts`]): its script's `rustc-cfg` is not joined yet.

#![blaze::sans_io]

use core::fmt;
use core::fmt::Write as _;

use blaze_id::{ContentId, CrateItemKey};
use blaze_manifest::lock::{Lock, LockId};
use blaze_own::{AVec, Alloc};
use blaze_surface::Edition;
use blaze_toolchain::{Role, ToolchainView, UnitKind as ToolchainKind};
use blaze_unit_graph::{Implicit, StdIs, Text, Unit, UnitGraph, UnitId, UnitKind, implicit};
use edition::PackageEdition;

use crate::graph::{CrateId, Sysroot};
use crate::resolve::CrateInput;

#[cfg(test)]
#[path = "tests/units.rs"]
mod tests;

/// The profile's atoms a toolchain unit is lowered under, beside its target's: `dev`, as the
/// unit graph gives a workspace unit.
const PROFILE: &[(&str, Option<&str>)] = &[("debug_assertions", None)];

/// Where a crate's files are.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Origin {
    /// A unit of the workspace's graph: its files are its package's, under
    /// `/package/<lock index>/`.
    Unit(UnitId),
    /// A toolchain unit (its index among the target's units): its files are its crate's pack,
    /// under `/toolchain/<unit>/`.
    Toolchain { unit: u32, krate: u32 },
}

/// One crate of the list.
pub struct UnitCrate<'a> {
    pub name: &'a str,
    pub origin: Origin,
    pub key: CrateItemKey,
    pub edition: Edition,
    pub implicit: Implicit,
    pub root: Text<'a>,
    pub shown_from: Text<'a>,
    pub features: AVec<'a, &'a str>,
    /// The name it calls each dependency, and that crate's index in [`UnitCrates::crates`].
    pub deps: AVec<'a, (&'a str, usize)>,
    pub cfg: AVec<'a, (&'a str, Option<&'a str>)>,
    /// A workspace unit with a build script, whose `rustc-cfg` is not in [`Self::cfg`].
    pub build_script: bool,
}

/// The crates of a workspace and its toolchain, by index (a [`CrateId`]).
pub struct UnitCrates<'a> {
    pub crates: AVec<'a, UnitCrate<'a>>,
    pub sysroot: Sysroot,
    /// How many toolchain crates lead [`Self::crates`]; the workspace's follow.
    pub toolchain: usize,
    /// Of the workspace's crates, those with a build script (its cfg not joined).
    pub build_scripts: usize,
    alloc: Alloc<'a>,
}

impl<'a> UnitCrates<'a> {
    /// The inputs of one `resolve_crates` call, by the same index.
    pub fn inputs(&self) -> AVec<'a, CrateInput<'_>> {
        let mut out = AVec::with_capacity_in(self.crates.len(), self.alloc);
        for c in self.crates.iter() {
            out.push(CrateInput {
                name: c.name,
                key: c.key,
                edition: c.edition,
                implicit: c.implicit,
                root: c.root.as_str(),
                shown_from: c.shown_from.as_str(),
                features: &c.features,
                deps: &c.deps,
                cfg: &c.cfg,
                script: None,
            });
        }
        out
    }
}

/// Why no crate list: each names the crates and what of them.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum UnitsError<'a> {
    /// The toolchain manifest holds no sysroot for the triple.
    NoTarget { triple: &'a str },
    /// A kept crate depends on a unit this list does not keep.
    DependencyNotKept { krate: &'a str, kind: &'static str, dep: &'a str, dep_kind: &'static str, dep_for_host: bool },
    /// A toolchain crate's edition is none of 2015 to 2024.
    ToolchainEdition { krate: &'a str, written: &'a str },
    /// A unit's root file leaves its package's folder (`..` past it).
    RootLeavesPackage { krate: &'a str, root: &'a str },
}

impl fmt::Display for UnitsError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UnitsError::NoTarget { triple } => write!(f, "the toolchain holds no sysroot for `{triple}`"),
            UnitsError::DependencyNotKept { krate, kind, dep, dep_kind, dep_for_host } => write!(
                f,
                "crate `{krate}` ({kind}) depends on `{dep}`, a {dep_kind} unit (for host: {dep_for_host}) the workspace's crates do not keep"
            ),
            UnitsError::ToolchainEdition { krate, written } => {
                write!(f, "the toolchain crate `{krate}` has the edition `{written}`, no Rust edition")
            }
            UnitsError::RootLeavesPackage { krate, root } => write!(f, "crate `{krate}`: its root `{root}` leaves its package's folder"),
        }
    }
}

/// The workspace's crates and the toolchain's they need. `members` are the workspace's
/// packages (their target-side `Lib` and `Bin` units are the roots); `toolchain` the installed
/// toolchain's manifest, read for `triple`.
pub fn unit_crates<'a>(
    graph: &'a UnitGraph<'_>,
    lock: &'a Lock<'_>,
    members: &[LockId],
    toolchain: ToolchainView<'a>,
    triple: &'a str,
    alloc: Alloc<'a>,
) -> Result<UnitCrates<'a>, UnitsError<'a>> {
    let units = &graph.units;
    // The workspace's units, by a work list from the members' target-side libraries and binaries.
    let mut reached: AVec<'a, bool> = AVec::new_in(alloc);
    reached.resize(units.len(), false);
    let mut todo: AVec<'a, usize> = AVec::new_in(alloc);
    for (at, u) in units.iter().enumerate() {
        // A member's test unit is a root too (the graph makes one only when tests are asked:
        // `Ask::tests`): left out, `--tests` solved no unit with `test` in its cfg.
        if members.contains(&u.package) && !u.for_host && matches!(u.kind, UnitKind::Lib | UnitKind::Bin(_) | UnitKind::Test { .. }) {
            todo.push(at);
        }
    }
    while let Some(at) = todo.pop() {
        if reached[at] {
            continue;
        }
        reached[at] = true;
        if units[at].kind != UnitKind::ProcMacro {
            todo.extend(units[at].deps.iter().map(|d| d.unit.index()));
        }
    }
    let kept = |u: &Unit<'_>| {
        u.kind == UnitKind::ProcMacro || (!u.for_host && matches!(u.kind, UnitKind::Lib | UnitKind::Bin(_) | UnitKind::Test { .. }))
    };
    let blaze = units
        .iter()
        .enumerate()
        .any(|(at, u)| reached[at] && kept(u) && matches!(implicit(u.edition, &u.kind).std, StdIs::Bstd(_)));

    let mut crates: AVec<'a, UnitCrate<'a>> = AVec::new_in(alloc);
    let sysroot = toolchain_crates(toolchain, triple, blaze, &mut crates, alloc)?;
    let first = crates.len();

    let mut at_of: AVec<'a, usize> = AVec::new_in(alloc);
    at_of.resize(units.len(), usize::MAX);
    let mut next = first;
    for (at, u) in units.iter().enumerate() {
        if reached[at] && kept(u) {
            at_of[at] = next;
            next += 1;
        }
    }
    let mut build_scripts = 0;
    for (at, u) in units.iter().enumerate() {
        if at_of[at] == usize::MAX {
            continue;
        }
        let name = u.crate_name.as_str();
        let base = numbered("/package/", u.package.0, alloc);
        let root = joined_root(&base, u.root.as_str(), alloc).ok_or(UnitsError::RootLeavesPackage { krate: name, root: u.root.as_str() })?;
        let mut deps = AVec::new_in(alloc);
        // A proc macro's dependencies are the host's, not in this list.
        if u.kind != UnitKind::ProcMacro {
            for d in u.deps.iter() {
                let to = at_of[d.unit.index()];
                if to == usize::MAX {
                    let dep = &units[d.unit.index()];
                    return Err(UnitsError::DependencyNotKept {
                        krate: name,
                        kind: kind_name(&u.kind),
                        dep: d.name.as_str(),
                        dep_kind: kind_name(&dep.kind),
                        dep_for_host: dep.for_host,
                    });
                }
                deps.push((d.name.as_str(), to));
            }
        }
        let mut features = AVec::with_capacity_in(u.features.len(), alloc);
        features.extend(u.features.iter().map(|f| f.as_str()));
        let mut cfg = AVec::with_capacity_in(u.cfg.len(), alloc);
        cfg.extend(u.cfg.iter().map(|a| (a.name.as_str(), a.value.as_ref().map(|v| v.as_str()))));
        build_scripts += usize::from(u.build_script.is_some());
        crates.push(UnitCrate {
            name,
            origin: Origin::Unit(UnitId(at as u32)),
            key: unit_key(u, lock.package(u.package).source_text.as_str(), alloc),
            edition: edition_of(u.edition.syntax),
            implicit: implicit(u.edition, &u.kind),
            root,
            shown_from: base,
            features,
            deps,
            cfg,
            build_script: u.build_script.is_some(),
        });
    }
    Ok(UnitCrates { crates, sysroot, toolchain: first, build_scripts, alloc })
}

/// The toolchain units reachable from std, core and alloc (and bstd when `blaze`) for
/// `triple`'s target side, pushed onto `out`; the sysroot's indices among them.
fn toolchain_crates<'a>(
    view: ToolchainView<'a>,
    triple: &'a str,
    blaze: bool,
    out: &mut AVec<'a, UnitCrate<'a>>,
    alloc: Alloc<'a>,
) -> Result<Sysroot, UnitsError<'a>> {
    let no_target = UnitsError::NoTarget { triple };
    let sys = blaze_toolchain::sysroot(&view, triple, false).ok_or(no_target.clone())?;
    let mut all = AVec::new_in(alloc);
    all.extend(view.units(triple).ok_or(no_target.clone())?);
    let mut keep: AVec<'a, bool> = AVec::new_in(alloc);
    keep.resize(all.len(), false);
    let mut todo: AVec<'a, usize> = AVec::new_in(alloc);
    todo.extend([sys.std.0 as usize, sys.core.0 as usize, sys.alloc.0 as usize]);
    if blaze {
        todo.push(sys.bstd.0 as usize);
    }
    while let Some(u) = todo.pop() {
        if keep[u] {
            continue;
        }
        keep[u] = true;
        if all[u].kind != ToolchainKind::ProcMacro {
            todo.extend(all[u].deps().map(|(_, d)| d as usize));
        }
    }
    let kept = |row: &blaze_toolchain::UnitRow<'_>| row.kind == ToolchainKind::ProcMacro || (row.kind == ToolchainKind::Lib && !row.for_host);
    let first = out.len();
    let mut at_of: AVec<'a, usize> = AVec::new_in(alloc);
    at_of.resize(all.len(), usize::MAX);
    let mut n = 0;
    for (u, row) in all.iter().enumerate() {
        if keep[u] && kept(row) {
            at_of[u] = first + n;
            n += 1;
        }
    }
    let rust = PackageEdition { syntax: edition::Edition::Edition2024, blaze: None };
    let target_cfg = view.cfg(triple).ok_or(no_target)?;
    let mut cfg_base: AVec<'a, (&'a str, Option<&'a str>)> = AVec::new_in(alloc);
    cfg_base.extend(target_cfg);
    cfg_base.extend(PROFILE.iter().copied());
    let mut sysroot = Sysroot { core: None, alloc: None, std: None, bstd: None };
    for (u, row) in all.iter().enumerate() {
        if at_of[u] == usize::MAX {
            continue;
        }
        let krate = view.krate(row.krate);
        let edition = edition_text(krate.edition).ok_or(UnitsError::ToolchainEdition { krate: krate.lib, written: krate.edition })?;
        let proc_macro = row.kind == ToolchainKind::ProcMacro;
        let base = numbered("/toolchain/", u as u32, alloc);
        // A tree crate's pack paths start with its directory; a registry crate's do not.
        let mut root = Text::from_str_in(base.as_str(), alloc);
        if !krate.dir.is_empty() && !row.root.starts_with(krate.dir) {
            root.push_str(krate.dir);
            root.push_str("/");
        }
        root.push_str(row.root);
        let mut deps = AVec::new_in(alloc);
        if !proc_macro {
            for (name, d) in row.deps() {
                let to = at_of[d as usize];
                if to == usize::MAX {
                    let dep = &all[d as usize];
                    return Err(UnitsError::DependencyNotKept {
                        krate: krate.lib,
                        kind: "lib",
                        dep: name,
                        dep_kind: toolchain_kind_name(dep.kind),
                        dep_for_host: dep.for_host,
                    });
                }
                deps.push((name, to));
            }
        }
        let mut features = AVec::new_in(alloc);
        features.extend(row.features());
        let mut cfg = AVec::with_capacity_in(cfg_base.len() + features.len(), alloc);
        cfg.extend(cfg_base.iter().copied());
        cfg.extend(features.iter().map(|f| ("feature", Some(*f))));
        let id = Some(CrateId(at_of[u] as u32));
        match krate.role {
            Role::Core if !row.for_host => sysroot.core = id,
            Role::Alloc if !row.for_host => sysroot.alloc = id,
            Role::Std if !row.for_host => sysroot.std = id,
            Role::Bstd if !row.for_host => sysroot.bstd = id,
            _ => {}
        }
        let mut key = AVec::new_in(alloc);
        key.extend_from_slice(&krate.pack);
        for f in features.iter() {
            key.extend_from_slice(f.as_bytes());
            key.push(0);
        }
        out.push(UnitCrate {
            name: krate.lib,
            origin: Origin::Toolchain { unit: u as u32, krate: row.krate },
            key: CrateItemKey(ContentId::of("blaze toolchain unit", &key)),
            edition,
            implicit: implicit(rust, if proc_macro { &UnitKind::ProcMacro } else { &UnitKind::Lib }),
            root,
            shown_from: base,
            features,
            deps,
            cfg,
            build_script: false,
        });
    }
    Ok(sysroot)
}

/// A workspace unit's key: its package as the lock writes it, its kind, side and features.
fn unit_key(u: &Unit<'_>, source: &str, alloc: Alloc<'_>) -> CrateItemKey {
    let mut bytes = AVec::new_in(alloc);
    let bin = match &u.kind {
        // A binary's test unit is keyed by its binary too: two of one package are two crates.
        UnitKind::Bin(name) | UnitKind::Test { bin: Some(name) } => name.as_str(),
        _ => "",
    };
    for part in [u.name.as_str(), u.version.as_str(), source, kind_name(&u.kind), bin] {
        bytes.extend_from_slice(part.as_bytes());
        bytes.push(0);
    }
    bytes.push(u8::from(u.for_host));
    for f in u.features.iter() {
        bytes.extend_from_slice(f.as_bytes());
        bytes.push(0);
    }
    CrateItemKey(ContentId::of("blaze workspace unit 1", &bytes))
}

/// `prefix` followed by `n` and a `/`.
fn numbered<'a>(prefix: &str, n: u32, alloc: Alloc<'a>) -> Text<'a> {
    let mut out = Text::from_str_in(prefix, alloc);
    // Writing to a `String` cannot fail.
    let _ = write!(out, "{n}/");
    out
}

/// `base` (ending in `/`) joined with `root`, its `.` and `..` taken out by text; `None` when a
/// `..` leaves `base`.
fn joined_root<'a>(base: &str, root: &str, alloc: Alloc<'a>) -> Option<Text<'a>> {
    let mut parts: AVec<'_, &str> = AVec::new_in(alloc);
    for part in root.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    let mut out = Text::from_str_in(base, alloc);
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            out.push_str("/");
        }
        out.push_str(part);
    }
    Some(out)
}

/// A Rust edition as the item layer names it.
pub fn edition_of(e: edition::Edition) -> Edition {
    match e {
        edition::Edition::Edition2015 => Edition::E2015,
        edition::Edition::Edition2018 => Edition::E2018,
        edition::Edition::Edition2021 => Edition::E2021,
        edition::Edition::Edition2024 => Edition::E2024,
    }
}

fn edition_text(written: &str) -> Option<Edition> {
    Some(match written {
        "2015" => Edition::E2015,
        "2018" => Edition::E2018,
        "2021" => Edition::E2021,
        "2024" => Edition::E2024,
        _ => return None,
    })
}

fn kind_name(kind: &UnitKind<'_>) -> &'static str {
    match kind {
        UnitKind::Lib => "lib",
        UnitKind::ProcMacro => "proc-macro",
        UnitKind::Bin(_) => "bin",
        UnitKind::BuildScript => "build script",
        UnitKind::Test { .. } => "test",
    }
}

fn toolchain_kind_name(kind: ToolchainKind) -> &'static str {
    match kind {
        ToolchainKind::Lib => "lib",
        ToolchainKind::ProcMacro => "proc-macro",
        ToolchainKind::BuildScript => "build script",
    }
}
