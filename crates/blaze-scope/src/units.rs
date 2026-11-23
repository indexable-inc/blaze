//! A cargo workspace's crates for [`resolve_crates`](crate::resolve::resolve_crates)
//! (docs/v2/plan/design/bottom.md, section 6): the units blaze-unit-graph found, and the
//! toolchain's library units their `std`, `core` and `alloc` are, turned into one crate list.
//! Sans-IO: the caller reads the graph, the lock and the toolchain manifest, and serves the
//! texts at the paths given here.
//!
//! Which units: the target-side `Lib` and `Bin` units the workspace members reach, and the
//! `ProcMacro` units among them (a proc macro is a crate of the graph, since its macros are
//! what its dependents' paths name). A dependency's proc macro is kept without its
//! dependencies (its records come from the module store); a MEMBER's proc macro has its
//! dependency closure again as crates of the macro side, `wasm32-wasip1`'s (D89 (a), rule 10:
//! the `macro_side` module), or, with no such toolchain cut, is refused as its own crate
//! ([`UnitCrates::refused`]). Never a `Test` or a `BuildScript` unit. A kept unit depending on
//! a unit not kept is [`UnitsError::DependencyNotKept`], naming both.
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
//! cfg. A workspace unit's cfg is [`Unit::cfg`] (the target's atoms, the profile's and the
//! unit's features) and, for a unit with a build script, its script's `rustc-cfg` atoms once
//! [`UnitCrates::join_scripts`] is given the script's outputs record; a unit with a build
//! script and no record joined is counted apart ([`UnitCrates::build_scripts`]).

#![blaze::sans_io]

use core::fmt;
use core::fmt::Write as _;

use blaze_build_script::OutputsRef;
use blaze_expand::seen::{MemberDirError, SeenRoot};
use blaze_id::{ContentId, CrateItemKey};
use blaze_manifest::lock::{Lock, LockId};
use blaze_own::{AVec, Alloc};
use blaze_surface::Edition;
use blaze_toolchain::{Role, ToolchainView, UnitKind as ToolchainKind};
use blaze_unit_graph::{Implicit, StdIs, Text, Unit, UnitGraph, UnitId, UnitKind, implicit};
use edition::PackageEdition;

use crate::graph::{CrateId, Sysroot};
use crate::resolve::CrateInput;

mod macro_crate;
mod macro_side;
mod names_refusal;

pub use macro_side::{MACRO_TARGET, RefusedCrate};

#[cfg(test)]
#[path = "tests/units.rs"]
mod tests;

#[cfg(test)]
#[path = "tests/units_script.rs"]
mod script_tests;

#[cfg(test)]
#[path = "tests/macro_side_names.rs"]
mod macro_side_names;

#[cfg(test)]
#[path = "tests/no_macro_toolchain_named.rs"]
mod no_macro_toolchain_named;

#[cfg(test)]
#[path = "tests/macro_crate_prelude.rs"]
mod macro_crate_prelude;

#[cfg(test)]
#[path = "tests/macro_side_prelude.rs"]
mod macro_side_prelude;

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

/// A workspace member: its package in the lock and its directory under the workspace, the
/// one it sees itself at (`/workspace/<dir>`, D96 (3)).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Member<'m> {
    id: LockId,
    seen: SeenRoot<'m>,
}

impl<'m> Member<'m> {
    /// The member `id` at `dir` under the workspace (`""` for the root package); a `dir` that
    /// is not a plain path under it is refused by name.
    pub fn new(id: LockId, dir: &'m str) -> Result<Member<'m>, MemberDirError<'m>> {
        Ok(Member { id, seen: SeenRoot::member(dir)? })
    }

    /// Its package in the lock.
    pub fn id(&self) -> LockId {
        self.id
    }
}

/// The root a unit of package `package` sees: its member's, else `/package` (D96).
pub(crate) fn seen_of<'m>(members: &[Member<'m>], package: LockId) -> SeenRoot<'m> {
    members.iter().find(|m| m.id == package).map_or(SeenRoot::Package, |m| m.seen)
}

/// One crate of the list.
pub struct UnitCrate<'a> {
    pub name: &'a str,
    pub origin: Origin,
    pub key: CrateItemKey,
    pub edition: Edition,
    pub implicit: Implicit,
    pub root: Text<'a>,
    /// The folder its files are read under (`/package/<lock index>/`): the reader's path space.
    pub shown_from: Text<'a>,
    /// The root the crate sees (D96): `/package` for a dependency or a toolchain crate,
    /// `/workspace/<dir>` for a member ([`Member`]).
    pub seen: SeenRoot<'a>,
    pub features: AVec<'a, &'a str>,
    /// The name it calls each dependency, and that crate's index in [`UnitCrates::crates`].
    pub deps: AVec<'a, (&'a str, usize)>,
    pub cfg: AVec<'a, (&'a str, Option<&'a str>)>,
    /// A workspace unit with a build script. Its script's `rustc-cfg` atoms are in
    /// [`Self::cfg`] exactly when [`Self::script`] is `Some` ([`UnitCrates::join_scripts`]).
    pub build_script: bool,
    /// Its build script's outputs record, once joined: its cfg atoms are in [`Self::cfg`] and
    /// its crate digest in [`Self::key`].
    pub script: Option<OutputsRef<'a>>,
}

/// The crates of a workspace and its toolchain, by index (a [`CrateId`]).
pub struct UnitCrates<'a> {
    pub crates: AVec<'a, UnitCrate<'a>>,
    pub sysroot: Sysroot,
    /// How many toolchain crates lead [`Self::crates`]; the workspace's follow.
    pub toolchain: usize,
    /// Of the workspace's crates, those with a build script whose record is not joined.
    pub build_scripts: usize,
    /// The crates of [`Self::crates`] that cannot be made, each with why (rule 20: the list
    /// is still made; whoever asks for one of these crates' names is given its refusal, by
    /// [`Self::refusal`]). Sorted by index, each once.
    pub refused: AVec<'a, RefusedCrate<'a>>,
    /// The crates at this index and past it are the macro side's (`macro_side`: a member proc
    /// macro's dependency closure under `wasm32-wasip1`'s cfg, and that target's toolchain
    /// crates); [`Self::crates`]'s length when there are none. A loader gives such a crate
    /// only facts of that side (never the host's build-script record).
    pub macro_from: usize,
    /// The crates before [`Self::macro_from`] built for the macro side all the same: each member
    /// proc macro and its library test harness (`macro_side`), whose dependencies are that
    /// side's copies and whose cfg is that side's. Sorted, each once.
    pub built_for_macro: AVec<'a, usize>,
    alloc: Alloc<'a>,
}

impl<'a> UnitCrates<'a> {
    /// Whether the crate at `krate` is built for the macro side: one past
    /// [`Self::macro_from`], or a member's proc macro or its harness
    /// ([`Self::built_for_macro`]). The one answer of a crate's side (rule 30).
    pub fn macro_side(&self, krate: usize) -> bool {
        krate >= self.macro_from || self.built_for_macro.binary_search(&krate).is_ok()
    }

    /// Why the crate at `krate` cannot be made, when it cannot: its names are refused with
    /// this error, naming it, instead of resolved.
    pub fn refusal(&self, krate: usize) -> Option<&UnitsError<'a>> {
        self.refused.iter().find(|r| r.krate == krate).map(|r| &r.why)
    }
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
                script: c.script,
            });
        }
        out
    }

    /// Joins each crate's build-script outputs record, by the same index (`None`: no record
    /// for that crate): the record's `cargo::rustc-cfg` atoms go into the crate's cfg (bare
    /// names and `key="value"` pairs alike), its crate digest into the crate's key (rule 21:
    /// the cfg is an input of the names), and the record itself into [`UnitCrate::script`],
    /// which [`Self::inputs`] hands on. This is the one place a script's cfg reaches a
    /// crate of this list; a crate is matched by its index, so two versions of one package
    /// take each its own record.
    ///
    /// Refused: a list of another length, a record for a crate with no build script, and a
    /// crate joined twice.
    pub fn join_scripts(&mut self, scripts: &[Option<OutputsRef<'a>>]) -> Result<(), UnitsError<'a>> {
        if scripts.len() != self.crates.len() {
            return Err(UnitsError::ScriptsCount { crates: self.crates.len(), given: scripts.len() });
        }
        let alloc = self.alloc;
        let mut joined = 0;
        for (c, record) in self.crates.iter_mut().zip(scripts) {
            let Some(record) = *record else { continue };
            if !c.build_script {
                return Err(UnitsError::RecordWithoutScript { krate: c.name });
            }
            if c.script.is_some() {
                return Err(UnitsError::ScriptJoinedTwice { krate: c.name });
            }
            let (cfg, key) = script_joined(&c.cfg, c.key, record, alloc);
            c.cfg = cfg;
            c.key = key;
            c.script = Some(record);
            joined += 1;
        }
        self.build_scripts -= joined;
        Ok(())
    }
}

/// A crate's cfg and key with its build script's outputs `record` joined: the cfg is the
/// unit's atoms and the record's `cargo::rustc-cfg` atoms (a bare name, or a name with its
/// value: `curve25519_dalek_bits="64"`), sorted and each once; the key is the unit's key and
/// the record's crate digest ([`OutputsRef::crate_digest`]: what of the record the crate's
/// text can read, its cfgs among it).
pub fn script_joined<'a>(
    cfg: &[(&'a str, Option<&'a str>)],
    key: CrateItemKey,
    record: OutputsRef<'a>,
    alloc: Alloc<'a>,
) -> (AVec<'a, (&'a str, Option<&'a str>)>, CrateItemKey) {
    let mut joined = AVec::with_capacity_in(cfg.len(), alloc);
    joined.extend(cfg.iter().copied());
    joined.extend(record.cfgs());
    joined.sort_unstable();
    joined.dedup();
    let mut bytes = AVec::with_capacity_in(64, alloc);
    bytes.extend_from_slice(&key.0.0);
    bytes.extend_from_slice(&record.crate_digest().0);
    (joined, CrateItemKey(ContentId::of("blaze workspace unit with script 1", &bytes)))
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
    /// [`UnitCrates::join_scripts`] was given a list not as long as the crate list.
    ScriptsCount { crates: usize, given: usize },
    /// A build-script outputs record was given for a crate with no build script.
    RecordWithoutScript { krate: &'a str },
    /// A crate was given a build-script outputs record after one was joined.
    ScriptJoinedTwice { krate: &'a str },
    /// A member's proc macro `krate` needs the macro side's toolchain crates (`core`, `alloc`,
    /// `std`, `proc_macro` for `target`, rule 10) and the toolchain was not cut with them.
    /// A refusal of that one crate ([`UnitCrates::refused`]), never of the list.
    NoMacroToolchain { krate: &'a str, target: &'static str },
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
            UnitsError::ScriptsCount { crates, given } => {
                write!(f, "build-script records were given for {given} crates; the crate list holds {crates} (one entry per crate, by index)")
            }
            UnitsError::RecordWithoutScript { krate } => {
                write!(f, "crate `{krate}` was given a build-script outputs record and has no build script")
            }
            UnitsError::ScriptJoinedTwice { krate } => {
                write!(f, "crate `{krate}` was given a build-script outputs record twice (rule 21: one record per crate)")
            }
            UnitsError::NoMacroToolchain { krate, target } => write!(
                f,
                "the proc macro `{krate}` is built for `{target}` (rule 10) and the toolchain shelf has no {target} side (its `core`, `alloc`, `std` and `proc_macro`): its names are not resolved without them, and the host's crates are not taken instead; cut the shelf with its macro side (scripts/src/bin/toolchain_cut.rs with `\"macro_side\": true`, D89 (a))"
            ),
        }
    }
}

/// The workspace's crates and the toolchain's they need. `members` are the workspace's
/// packages (their target-side `Lib` and `Bin` units are the roots); `toolchain` the installed
/// toolchain's manifest, read for `triple`.
pub fn unit_crates<'a>(
    graph: &'a UnitGraph<'_>,
    lock: &'a Lock<'_>,
    members: &[Member<'a>],
    toolchain: ToolchainView<'a>,
    triple: &'a str,
    alloc: Alloc<'a>,
) -> Result<UnitCrates<'a>, UnitsError<'a>> {
    let units = &graph.units;
    // The workspace's units, by a work list from the members' target-side libraries and binaries.
    let mut reached: AVec<'a, bool> = AVec::new_in(alloc);
    reached.resize(units.len(), false);
    let mut todo: AVec<'a, usize> = AVec::new_in(alloc);
    // A proc macro's code (the proc macro and its library test harness, both built for the
    // host by the unit graph: units/macro_crate.rs) is a crate of this list whose dependencies
    // are not: a member's are the macro side's (`macro_side`), a dependency's come from the
    // module store.
    let macro_code = |at: usize| macro_crate::is_macro_crate(units, at);
    let target_kind = |u: &Unit<'_>| matches!(u.kind, UnitKind::Lib | UnitKind::Bin(_) | UnitKind::Test { .. } | UnitKind::IntegrationTest(_) | UnitKind::Example(_) | UnitKind::Bench(_));
    for (at, u) in units.iter().enumerate() {
        // A member's test unit is a root too (the graph makes one only when tests are asked:
        // `Ask::tests`): left out, `--tests` solved no unit with `test` in its cfg. A member
        // proc macro's harness is one though it is a host unit.
        if members.iter().any(|m| m.id == u.package) && ((!u.for_host && target_kind(u)) || (u.kind != UnitKind::ProcMacro && macro_code(at))) {
            todo.push(at);
        }
    }
    while let Some(at) = todo.pop() {
        if reached[at] {
            continue;
        }
        reached[at] = true;
        if !macro_code(at) {
            todo.extend(units[at].deps.iter().map(|d| d.unit.index()));
        }
    }
    let kept = |at: usize| {
        let u = &units[at];
        u.kind == UnitKind::ProcMacro || macro_code(at) || (!u.for_host && target_kind(u))
    };
    let blaze = units.iter().enumerate().any(|(at, _)| reached[at] && kept(at) && matches!(macro_crate::unit_implicit(units, at).std, StdIs::Bstd(_)));
    // The program's own need of Rust's std is its own edge; a toolchain crate's is its row's
    // (blaze_toolchain::implied): std is listed iff a crate of the closure names it.
    let rust_std = units
        .iter()
        .enumerate()
        .any(|(at, _)| reached[at] && kept(at) && matches!(macro_crate::unit_implicit(units, at).std, StdIs::Rust));

    let mut crates: AVec<'a, UnitCrate<'a>> = AVec::new_in(alloc);
    let sysroot = toolchain_crates(toolchain, triple, rust_std, blaze, &mut crates, alloc)?;
    let first = crates.len();

    let mut at_of: AVec<'a, usize> = AVec::new_in(alloc);
    at_of.resize(units.len(), usize::MAX);
    let mut next = first;
    for at in 0..units.len() {
        if reached[at] && kept(at) {
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
        // A proc macro's code's dependencies are the host's, not in this list (a member's are
        // pointed at the macro side's copies by `macro_side`).
        if !macro_code(at) {
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
            // A proc macro's library test harness is given what the proc macro is (`proc_macro`
            // in its extern prelude): units/macro_crate.rs, the one rule.
            implicit: macro_crate::unit_implicit(units, at),
            root,
            shown_from: base,
            seen: seen_of(members, u.package),
            features,
            deps,
            cfg,
            build_script: u.build_script.is_some(),
            script: None,
        });
    }
    // A member's proc macro is built from the macro side's crates (D89 (a)), or refused alone.
    let mut refused = AVec::new_in(alloc);
    let macro_from = crates.len();
    let mut built_for_macro = AVec::new_in(alloc);
    macro_side::add_macro_side(graph, lock, members, toolchain, &at_of, &mut crates, &mut build_scripts, &mut refused, &mut built_for_macro, alloc)?;
    built_for_macro.sort_unstable();
    built_for_macro.dedup();
    Ok(UnitCrates { crates, sysroot, toolchain: first, build_scripts, refused, macro_from, built_for_macro, alloc })
}

/// The toolchain units reachable from core and alloc (and Rust's std when `rust_std`, bstd
/// when `blaze`) for `triple`'s target side, by the rows' edges (a crate's implied sysroot
/// edges among them, so std comes in when a crate of the closure names it), pushed onto
/// `out`; the sysroot's indices among them.
fn toolchain_crates<'a>(
    view: ToolchainView<'a>,
    triple: &'a str,
    rust_std: bool,
    blaze: bool,
    out: &mut AVec<'a, UnitCrate<'a>>,
    alloc: Alloc<'a>,
) -> Result<Sysroot, UnitsError<'a>> {
    let no_target = || UnitsError::NoTarget { triple };
    let sys = blaze_toolchain::sysroot(&view, triple, false).ok_or_else(no_target)?;
    let mut all = AVec::new_in(alloc);
    all.extend(view.units(triple).ok_or_else(no_target)?);
    let mut keep: AVec<'a, bool> = AVec::new_in(alloc);
    keep.resize(all.len(), false);
    let mut todo: AVec<'a, usize> = AVec::new_in(alloc);
    todo.extend([sys.core.0 as usize, sys.alloc.0 as usize]);
    if rust_std {
        todo.push(sys.std.0 as usize);
    }
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
    let target_cfg = view.cfg(triple).ok_or_else(no_target)?;
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
        // Its implicit is its own: its crate's edition (the prelude's `rust_20xx`), not one
        // uniform Rust 2024; what it is given from the sysroot is its row's edges.
        let own = PackageEdition::of_manifest(krate.edition).map_err(|_| UnitsError::ToolchainEdition { krate: krate.lib, written: krate.edition })?;
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
            implicit: implicit(own, if proc_macro { &UnitKind::ProcMacro } else { &UnitKind::Lib }),
            root,
            shown_from: base,
            seen: SeenRoot::Package,
            features,
            deps,
            cfg,
            build_script: false,
            script: None,
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
        // Two integration tests (or examples, benches) of one package are two crates.
        UnitKind::IntegrationTest(x) | UnitKind::Example(x) | UnitKind::Bench(x) => x.name.as_str(),
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
        UnitKind::Test { .. } | UnitKind::IntegrationTest(_) => "test",
        UnitKind::Example(_) => "example",
        UnitKind::Bench(_) => "bench",
    }
}

fn toolchain_kind_name(kind: ToolchainKind) -> &'static str {
    match kind {
        ToolchainKind::Lib => "lib",
        ToolchainKind::ProcMacro => "proc-macro",
        ToolchainKind::BuildScript => "build script",
    }
}
