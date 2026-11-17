//! One world for run and check (docs/v2/plan/design/typeck.md, "Checker at scale", lane 1).
//! Tonight it holds the names library's inputs: a project's unit graph made into one
//! [`CrateInput`] per crate, for `blaze_scope::resolve::resolve_crates`.
//!
//! - **Which crates.** Every library unit built for the target, every proc-macro unit (a crate
//!   whose items are macros; its host-side dependencies are not followed: they are the host's
//!   build, not this graph's), and the toolchain's library units they reach (core, alloc, Rust's
//!   `std` or bstd, `proc_macro`, and what those are built from). Bins, tests and build scripts
//!   are not crates of the names set. A held unit's dependency on a unit not held is the typed
//!   error [`WorldError::DependencyNotHeld`], never an index made up.
//! - **One crate per configuration.** Units with equal inputs (a library built for the host
//!   and for the target, the Mac's host being its target) are one crate: a crate's key is the
//!   digest of its package's identity, kind, edition, root, features, cfg and its dependencies'
//!   keys, computed before any file is read (rule 65).
//! - **Each crate's input.** Its root file, found by the names library's own walker from there;
//!   its extern prelude from the unit's dependency edges (renames as written) plus the implicit
//!   toolchain crates (blaze-unit-graph's `implicit`, through [`Sysroot`]); `proc_macro` as an
//!   edge of a proc-macro crate; its cfg the unit's atoms joined with its build script's
//!   `cargo::rustc-cfg` lines when [`ScriptOutputs`] holds them.
//! - **Where files are.** Every crate's paths are `/<tree>/<path in the tree>`: tree 0 the
//!   checkout (workspace members and path dependencies, one tree so a `#[path]` between them
//!   resolves as on disk), then one tree per toolchain pack, then one per fetched package's
//!   record. No path of this machine is in an input (rule 25).
//! - **Order.** The toolchain's crates first, in its manifest's order; then the graph's, sorted
//!   by key: the same graph gives byte-equal inputs ([`World::write`]) whatever order its units
//!   are listed in.
//!
//! The function is sans-IO: the trees are values passed in ([`Packages`] for the fetch store,
//! [`Toolchain::open`] over a `Shelf`, a [`Tree`] for the checkout).

#![no_std]
#![feature(allocator_api)]

#[cfg(feature = "std")]
extern crate std;

#[cfg(feature = "std")]
mod checkout;
pub mod fixture;
#[cfg(feature = "procs")]
pub mod procs;
mod script;
mod toolchain;
#[cfg(test)]
mod tests;

use core::fmt;

use blaze_build_script::outputs::OutputsRef;
use blaze_fetch::PackView;
use blaze_id::{ContentId, CrateItemKey};
use blaze_manifest::lock::{Lock, LockId};
use blaze_own::{AString, AVec, Alloc, BTreeMap, Box};
use blaze_scope::crate_env::{CrateEnv, EnvFacts, MemberDirError, SeenRoot};
use blaze_scope::graph::{CrateId, Sysroot};
use blaze_scope::resolve::{CrateInput, Names, NamesError, SourceTexts, resolve_crates};
use blaze_scope::task::Par;
use blaze_surface::Edition;
use blaze_unit_graph::{SourceError, StoreSources, UnitGraph, UnitId, UnitKind, implicit};
use blaze_unit_graph::sources::Records;

#[cfg(feature = "std")]
pub use checkout::Checkout;
pub use script::{ScriptRecords, ScriptRecordsError};
pub use toolchain::{Needs, Toolchain, ToolchainCrate};

/// A tree of files a crate is read from: a path is `/`-separated, relative to the tree's root,
/// without `.` or `..`.
pub trait Tree {
    /// The bytes of the file at `path`, or `None` when the tree has none.
    fn file(&self, path: &str) -> Option<&[u8]>;
}

impl Tree for PackView<'_> {
    fn file(&self, path: &str) -> Option<&[u8]> {
        self.find(path).map(|f| f.bytes)
    }
}

/// Where a package's files are.
#[derive(Clone, Copy)]
pub enum Place<'w> {
    /// In the checkout (tree 0), under this directory (relative to the checkout's root, no
    /// trailing `/`; `""` for the root).
    Checkout { dir: &'w str },
    /// In a record of the fetch store, under this directory of it (`""` for a crate; a git
    /// package's directory in its commit).
    Pack { view: PackView<'w>, dir: &'w str },
}

/// Where each locked package's files are: the caller's store and checkout.
pub trait Packages<'w> {
    fn place(&mut self, package: LockId) -> Result<Place<'w>, WorldError<'w>>;
}

/// The fetch store and the checkout's path packages: a registry or git package from its record
/// (`StoreSources::files`), a path package from the directory the caller's layout found it in.
pub struct Fetched<'s, 'l, 'w, R: Records<'w>> {
    pub store: &'s mut StoreSources<'l, 'w, 'w, R>,
    /// Each path package's directory in the checkout, by lock id.
    pub paths: &'s [(LockId, &'w str)],
    pub lock: &'l Lock<'w>,
    pub alloc: Alloc<'w>,
}

impl<'w, R: Records<'w>> Packages<'w> for Fetched<'_, '_, 'w, R> {
    fn place(&mut self, package: LockId) -> Result<Place<'w>, WorldError<'w>> {
        let p = self.lock.package(package);
        if matches!(p.source, blaze_manifest::lock::LockSource::Path) {
            return match self.paths.iter().find(|(id, _)| *id == package) {
                Some((_, dir)) => Ok(Place::Checkout { dir }),
                None => Err(WorldError::PathNotPlaced { package: keep(&p.name, self.alloc) }),
            };
        }
        let (view, dir) = self.store.files(package).map_err(WorldError::Source)?;
        Ok(Place::Pack { view, dir: keep(&dir, self.alloc) })
    }
}

/// A build script's outputs that join its package's library, by the library unit (the record
/// the build-scripts lane writes).
pub trait ScriptOutputs<'w> {
    fn served(&self, lib: UnitId) -> Option<Served<'w>>;
}

/// One library's build-script outputs as its crate takes them.
#[derive(Clone, Copy)]
pub struct Served<'w> {
    /// The record, read in place: the expansion host's `OUT_DIR`, `env!` pairs and files.
    pub outputs: OutputsRef<'w>,
    /// Its `cargo::rustc-cfg` atoms, sorted and each once: joined to the unit's cfg.
    pub cfg: &'w [(&'w str, Option<&'w str>)],
    /// [`OutputsRef::crate_digest`]: in the crate's key.
    pub digest: ContentId,
}

/// What a crate is made from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Origin {
    /// A toolchain unit (its index in the toolchain manifest's unit list).
    Toolchain(u32),
    /// The first unit of the graph (in the graph's order) with these inputs.
    Unit(UnitId),
}

/// Whether a crate's build script's outputs are in its input.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Script {
    /// No build script.
    None,
    /// Its cfg atoms joined.
    Joined,
    /// It has one, whose outputs were not given: resolved without them.
    Missing,
}

/// One crate of the world.
#[derive(Clone, Copy)]
pub struct Crate<'w> {
    pub input: CrateInput<'w>,
    pub origin: Origin,
    /// How many units of the graph are this crate (0 for the toolchain's).
    pub units: u32,
    pub script: Script,
    pub proc_macro: bool,
    /// The package's name and version, as the lock writes them (the toolchain's crate's for its).
    pub package: &'w str,
    pub version: &'w str,
}

/// Every crate's input, and where their files are.
pub struct World<'w> {
    pub crates: AVec<'w, Crate<'w>>,
    pub texts: Texts<'w>,
    pub sysroot: Sysroot,
    /// The toolchain's crates are the first this many.
    pub toolchain_crates: u32,
    /// Per unit of the graph, the crate it is, `None` when it is not one of the names set.
    pub crate_of_unit: AVec<'w, Option<u32>>,
    /// The source corrections (D58) read, one per package, in placement order.
    pub corrections: AVec<'w, Corrected<'w>>,
}

impl<'w> World<'w> {
    /// The inputs `resolve_crates` takes, in crate order.
    pub fn inputs(&self, alloc: Alloc<'w>) -> AVec<'w, CrateInput<'w>> {
        let mut out = AVec::with_capacity_in(self.crates.len(), alloc);
        out.extend(self.crates.iter().map(|c| c.input));
        out
    }

    /// Every crate's names, resolved together.
    pub fn resolve<P: Par>(&self, inputs: &[CrateInput<'w>], par: &P, alloc: Alloc<'w>) -> Result<Names<'w>, NamesError<'w>> {
        resolve_crates(inputs, self.sysroot, &self.texts, par, alloc)
    }

    /// Every input as bytes, in crate order: what two builders of one graph write equal.
    pub fn write(&self, out: &mut AVec<'_, u8>) {
        let text = |out: &mut AVec<'_, u8>, s: &str| {
            out.extend_from_slice(&(s.len() as u32).to_le_bytes());
            out.extend_from_slice(s.as_bytes());
        };
        for id in self.sysroot.all() {
            out.extend_from_slice(&id.map_or(u32::MAX, |it| it.0).to_le_bytes());
        }
        for c in self.crates.iter() {
            let i = &c.input;
            text(out, i.name);
            out.extend_from_slice(&i.key.0.0);
            out.push(i.edition as u8);
            text(out, &alloc_free_debug(&i.implicit));
            text(out, i.root);
            text(out, i.shown_from);
            out.extend_from_slice(&(i.features.len() as u32).to_le_bytes());
            for f in i.features {
                text(out, f);
            }
            out.extend_from_slice(&(i.deps.len() as u32).to_le_bytes());
            for (name, at) in i.deps {
                text(out, name);
                out.extend_from_slice(&(*at as u32).to_le_bytes());
            }
            out.extend_from_slice(&(i.cfg.len() as u32).to_le_bytes());
            for (name, value) in i.cfg {
                text(out, name);
                match value {
                    Some(v) => {
                        out.push(1);
                        text(out, v);
                    }
                    None => out.push(0),
                }
            }
            out.push(c.script as u8);
            out.push(u8::from(c.proc_macro));
        }
    }
}

/// `implicit`'s fields as a short fixed text (no allocation): what [`World::write`] writes.
fn alloc_free_debug(i: &blaze_unit_graph::Implicit) -> FixedText {
    use blaze_unit_graph::{Prelude, StdIs};
    let mut t = FixedText::default();
    t.push(match i.std {
        StdIs::Rust => "rust-std ",
        StdIs::Bstd(_) => "bstd ",
    });
    t.push(match i.prelude {
        Prelude::Rust(_) | Prelude::Blaze(_) => i.prelude.module(),
    });
    t.push(if i.proc_macro { " proc_macro" } else { "" });
    t
}

/// A text of at most 64 bytes on the stack.
struct FixedText {
    bytes: [u8; 64],
    len: usize,
}

impl Default for FixedText {
    fn default() -> FixedText {
        FixedText { bytes: [0; 64], len: 0 }
    }
}

impl FixedText {
    fn push(&mut self, s: &str) {
        let end = (self.len + s.len()).min(64);
        self.bytes[self.len..end].copy_from_slice(&s.as_bytes()[..end - self.len]);
        self.len = end;
    }
}

impl core::ops::Deref for FixedText {
    type Target = str;
    fn deref(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.len]).unwrap_or("")
    }
}

/// The texts `resolve_crates` reads: tree 0 the checkout, then each pack.
pub struct Texts<'w> {
    checkout: &'w dyn Tree,
    packs: AVec<'w, PackView<'w>>,
    /// Files read corrected (D58): `(tree, path in it, corrected text)`, read in place of the
    /// pack's bytes.
    corrected: AVec<'w, (u32, &'w str, &'w str)>,
    /// Each crate's environment, by crate index ([`SourceTexts::crate_env`]): what its `env!`
    /// reads, `CARGO_MANIFEST_DIR` the root it sees (D96).
    envs: AVec<'w, CrateEnv<'w>>,
}

impl<'w> Texts<'w> {
    /// The trees after the checkout: the toolchain's packs, then the fetched packages'.
    pub fn packs(&self) -> &[PackView<'w>] {
        &self.packs
    }

    /// The files read corrected (D58), each `(tree, path in it, corrected text)`: what a reader
    /// of the packs without the checkout (a pool worker's, blaze-cli body_types.rs) reads in
    /// place of the pack's bytes, as [`SourceTexts::read`] here does.
    pub fn corrected(&self) -> &[(u32, &'w str, &'w str)] {
        &self.corrected
    }
}

impl<'w> SourceTexts<'w> for Texts<'w> {
    fn read(&self, _krate: usize, path: &str) -> Option<&'w str> {
        let rest = path.strip_prefix('/')?;
        let (tree, inner) = rest.split_once('/')?;
        let tree: usize = tree.parse().ok()?;
        if let Some((_, _, text)) = self.corrected.iter().find(|(t, p, _)| *t as usize == tree && *p == inner) {
            return Some(text);
        }
        let bytes = match tree {
            0 => self.checkout.file(inner)?,
            n => self.packs.get(n - 1)?.find(inner)?.bytes,
        };
        core::str::from_utf8(bytes).ok()
    }

    fn crate_env(&self, krate: usize) -> Option<&CrateEnv<'w>> {
        self.envs.get(krate)
    }
}

/// Why no world: each names the package or crate.
#[derive(Debug)]
pub enum WorldError<'w> {
    /// A fetched package's sources: not in the store, refused, or without its manifest.
    Source(SourceError<'w>),
    /// A path package the caller placed nowhere in the checkout.
    PathNotPlaced { package: &'w str },
    /// A held unit depends on a unit that is not a crate of the names set (a host-side
    /// library, a build script, a binary).
    DependencyNotHeld { krate: &'w str, dep: &'w str, kind: &'static str, for_host: bool },
    /// The held units' dependencies make a cycle through this crate.
    Cycle { krate: &'w str },
    /// The toolchain: its manifest or a pack.
    Toolchain(blaze_toolchain::ToolchainError),
    /// The toolchain holds no units for the triple.
    NoTarget { triple: &'w str },
    /// std's build-script cfg is not tabled for the triple (`toolchain::STD_SCRIPT_CFG`, interim).
    NoStdScriptCfg { triple: &'w str },
    /// The toolchain holds no crate of a role the graph needs.
    NoToolchainCrate { role: &'static str },
    /// A toolchain crate's edition is none of 2015 to 2024.
    ToolchainEdition { krate: &'w str, written: &'w str },
    /// A toolchain unit kept depends on a unit not kept (not a target-side library).
    ToolchainDependencyNotKept { krate: &'w str, dep: &'w str },
    /// The toolchain's kept crates depend on each other in a cycle, so none of them has a key.
    ToolchainCycle { krate: &'w str },
    /// A toolchain pack read and verified is no pack form.
    Pack { krate: &'w str },
    /// A source correction (D58, `blaze_fetch::correct`) refused this package: the words say
    /// which file and why (a stale row, a missing file).
    Correction { package: &'w str, version: &'w str, why: &'w str },
    /// A member's directory cannot be seen at `/workspace/<dir>` (D96).
    MemberDir { package: &'w str, error: MemberDirError<'w> },
    /// A crate's environment cannot be made (its version has not three parts).
    Env(blaze_scope::crate_env::CrateEnvError<'w>),
}

impl fmt::Display for WorldError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WorldError::Source(e) => write!(f, "{e}"),
            WorldError::MemberDir { package, error } => write!(f, "the member `{package}`: {error}"),
            WorldError::Env(e) => write!(f, "{e}"),
            WorldError::PathNotPlaced { package } => write!(f, "the path package `{package}` is in no directory the workspace's layout found"),
            WorldError::Correction { package, version, why } => write!(f, "{package} {version}: {why}"),
            WorldError::DependencyNotHeld { krate, dep, kind, for_host } => {
                write!(f, "crate `{krate}` depends on `{dep}`, a {kind} unit (for the host: {for_host}) that is not a crate of the names set")
            }
            WorldError::Cycle { krate } => write!(f, "the crates' dependencies make a cycle through `{krate}`"),
            WorldError::Toolchain(e) => write!(f, "{e}"),
            WorldError::NoTarget { triple } => write!(f, "the toolchain holds no units for `{triple}`"),
            WorldError::NoStdScriptCfg { triple } => {
                write!(f, "std's build-script cfg is not known for `{triple}` (blaze-world toolchain.rs STD_SCRIPT_CFG: its build script is not run yet)")
            }
            WorldError::NoToolchainCrate { role } => write!(f, "the toolchain holds no `{role}` crate, which the graph needs"),
            WorldError::ToolchainEdition { krate, written } => write!(f, "the toolchain crate `{krate}` has the edition `{written}`, no Rust edition"),
            WorldError::ToolchainCycle { krate } => write!(f, "the toolchain crate `{krate}` is in a dependency cycle among the toolchain's crates"),
            WorldError::ToolchainDependencyNotKept { krate, dep } => {
                write!(f, "the toolchain crate `{krate}` depends on `{dep}`, which is not a target-side library of the toolchain")
            }
            WorldError::Pack { krate } => write!(f, "the toolchain's pack of `{krate}` is no pack"),
        }
    }
}

/// `text` copied into `alloc` for as long as it lives.
fn keep<'w>(text: &str, alloc: Alloc<'w>) -> &'w str {
    Box::leak(AString::from_str_in(text, alloc).into_boxed_str())
}

/// `values` copied into `alloc` for as long as it lives.
fn keep_slice<'w, T: Copy>(values: &[T], alloc: Alloc<'w>) -> &'w [T] {
    AVec::from_slice_in(values, alloc).leak()
}

fn edition_of(e: edition::Edition) -> Edition {
    match e {
        edition::Edition::Edition2015 => Edition::E2015,
        edition::Edition::Edition2018 => Edition::E2018,
        edition::Edition::Edition2021 => Edition::E2021,
        edition::Edition::Edition2024 => Edition::E2024,
    }
}

fn kind_name(kind: &UnitKind<'_>) -> &'static str {
    match kind {
        UnitKind::Lib => "library",
        UnitKind::ProcMacro => "proc-macro",
        UnitKind::Bin(_) => "binary",
        UnitKind::BuildScript => "build-script",
        UnitKind::Test { .. } | UnitKind::IntegrationTest(_) => "test",
        UnitKind::Example(_) => "example",
        UnitKind::Bench(_) => "bench",
    }
}

/// Whether a unit is a crate of the names set: every target-side crate the graph builds (a
/// library, and a binary or test, whose own names a `check` answers for: leaving them out left
/// the project's own crate with no `CrateScope`, so a signature edit in it reran nothing), and a
/// proc macro. A build script is the host's build: not held.
fn held(kind: &UnitKind<'_>, for_host: bool) -> bool {
    match kind {
        UnitKind::Lib
        | UnitKind::Bin(_)
        | UnitKind::Test { .. }
        | UnitKind::IntegrationTest(_)
        | UnitKind::Example(_)
        | UnitKind::Bench(_) => !for_host,
        UnitKind::ProcMacro => true,
        UnitKind::BuildScript => false,
    }
}

/// `a` then `b`, joined by one `/` (none when either is empty).
fn join<'w>(a: &str, b: &str, alloc: Alloc<'w>) -> AString<'w> {
    let mut out = AString::with_capacity_in(a.len() + b.len() + 1, alloc);
    out.push_str(a);
    if !a.is_empty() && !b.is_empty() && !a.ends_with('/') {
        out.push_str("/");
    }
    out.push_str(b);
    out
}

/// One held unit's inputs before its crate's place is known.
struct Pending<'w> {
    unit: UnitId,
    key: CrateItemKey,
    /// (extern name, crate index among the graph's crates before sorting)
    deps: AVec<'w, (&'w str, u32)>,
    cfg: &'w [(&'w str, Option<&'w str>)],
    script: Script,
    outputs: Option<OutputsRef<'w>>,
    units: u32,
}

/// The names library's inputs for every crate of `graph`: see the crate's documentation.
///
/// Per unit, the work is [`unit_input`]'s (its key, cfg and extern prelude, given its
/// dependencies' crates); the rest is the set's: which units are held, one crate per key, the
/// order, and the trees.
pub fn world<'w, P: Packages<'w>, B: AsRef<[u8]>>(
    graph: &'w UnitGraph<'w>,
    lock: &'w Lock<'w>,
    toolchain: &'w Toolchain<'w, B>,
    packages: &mut P,
    checkout: &'w dyn Tree,
    scripts: Option<&dyn ScriptOutputs<'w>>,
    alloc: Alloc<'w>,
) -> Result<World<'w>, WorldError<'w>> {
    let units = &graph.units;
    let n = units.len();
    // The source corrections (D58): checked-in data; a package a row names is read corrected.
    let corrections = blaze_fetch::correct::Table::checked_in(alloc.clone());
    let mut corrected: AVec<'w, (u32, &'w str, &'w str)> = AVec::new_in(alloc.clone());
    let mut used: AVec<'w, Corrected<'w>> = AVec::new_in(alloc.clone());
    let tc = toolchain.crates();
    let t = tc.len() as u32;
    let proc_macro_crate = toolchain.proc_macro().map(|i| i as u32);

    // Dependencies first, by a work list (Kahn's): each held unit's count of held dependencies
    // not yet keyed.
    let mut waiting: AVec<'w, u32> = AVec::with_capacity_in(n, alloc.clone());
    let mut dependents: AVec<'w, AVec<'w, u32>> = AVec::with_capacity_in(n, alloc.clone());
    for _ in 0..n {
        dependents.push(AVec::new_in(alloc.clone()));
    }
    for (u, unit) in units.iter().enumerate() {
        let is_held = held(&unit.kind, unit.for_host);
        let mut count = 0;
        // A proc macro's dependencies are the host's build: not followed.
        if is_held && unit.kind != UnitKind::ProcMacro {
            for dep in unit.deps.iter() {
                let d = &units[dep.unit.index()];
                if !held(&d.kind, d.for_host) {
                    return Err(WorldError::DependencyNotHeld {
                        krate: &unit.crate_name,
                        dep: &dep.name,
                        kind: kind_name(&d.kind),
                        for_host: d.for_host,
                    });
                }
                dependents[dep.unit.index()].push(u as u32);
                count += 1;
            }
        }
        waiting.push(if is_held { count } else { u32::MAX });
    }
    let mut ready: AVec<'w, u32> = AVec::new_in(alloc.clone());
    ready.extend((0..n as u32).filter(|u| waiting[*u as usize] == 0));
    // Ready units in the graph's order of ids, so the first unit of a key is the lowest id.
    ready.reverse();

    // Each held unit's crate (an index into `pending`), one per key.
    let mut crate_of: AVec<'w, Option<u32>> = AVec::with_capacity_in(n, alloc.clone());
    crate_of.extend((0..n).map(|_| None));
    let mut pending: AVec<'w, Pending<'w>> = AVec::new_in(alloc.clone());
    let mut by_key: BTreeMap<[u8; 32], u32, Alloc<'w>> = BTreeMap::new_in(alloc.clone());
    // Path packages placed while keying (by lock index), each asked of `packages` once.
    let mut early: BTreeMap<u32, Place<'w>, Alloc<'w>> = BTreeMap::new_in(alloc.clone());
    let mut keyed = 0usize;
    while let Some(u) = ready.pop() {
        let unit = &units[u as usize];
        let id = UnitId(u);
        // A record is taken only for a unit a build script serves.
        let served = unit.build_script.and_then(|_| scripts.and_then(|s| s.served(id)));
        let script = match (unit.build_script, served) {
            (None, _) => Script::None,
            (Some(_), Some(_)) => Script::Joined,
            (Some(_), None) => Script::Missing,
        };
        let implicit_key = implicit_key(toolchain, implicit(unit.edition, &unit.kind))?;
        // A path package is placed now: a member's directory is what it sees as its root
        // (`/workspace/<dir>`, D96 (3)), so it is in its key (rule 21).
        let member = if lock.package(unit.package).source_text.is_empty() {
            let place = match early.get(&unit.package.0) {
                Some(&place) => place,
                None => {
                    let place = packages.place(unit.package)?;
                    early.insert(unit.package.0, place);
                    place
                }
            };
            match place {
                Place::Checkout { dir } => Some(dir),
                Place::Pack { .. } => None,
            }
        } else {
            None
        };
        let input = unit_input(graph, lock, id, &crate_of, &|c| pending[c as usize].key, implicit_key, served, member, &corrections, alloc.clone());
        keyed += 1;
        match by_key.get(&input.key.0.0) {
            Some(&c) => {
                crate_of[u as usize] = Some(c);
                pending[c as usize].units += 1;
            }
            None => {
                let c = pending.len() as u32;
                by_key.insert(input.key.0.0, c);
                crate_of[u as usize] = Some(c);
                pending.push(Pending { unit: id, key: input.key, deps: input.deps, cfg: input.cfg, script, outputs: served.map(|s| s.outputs), units: 1 });
            }
        }
        for &d in dependents[u as usize].iter() {
            waiting[d as usize] -= 1;
            if waiting[d as usize] == 0 {
                ready.push(d);
            }
        }
    }
    let held_count = waiting.iter().filter(|w| **w != u32::MAX).count();
    if keyed != held_count {
        let stuck = waiting.iter().position(|w| *w != u32::MAX && *w != 0).unwrap_or(0);
        return Err(WorldError::Cycle { krate: &units[stuck].crate_name });
    }

    // The order: by key. `rank[c]` is pending crate c's place among the graph's crates.
    let mut order: AVec<'w, u32> = AVec::with_capacity_in(pending.len(), alloc.clone());
    order.extend(0..pending.len() as u32);
    order.sort_unstable_by(|a, b| pending[*a as usize].key.0.0.cmp(&pending[*b as usize].key.0.0));
    let mut rank: AVec<'w, u32> = AVec::with_capacity_in(pending.len(), alloc.clone());
    rank.extend((0..pending.len()).map(|_| 0));
    for (at, c) in order.iter().enumerate() {
        rank[*c as usize] = at as u32;
    }

    // The trees: the toolchain's packs (1..=t), then each fetched package's record in the order
    // its first crate is placed.
    let mut packs: AVec<'w, PackView<'w>> = AVec::with_capacity_in(tc.len(), alloc.clone());
    for c in tc {
        packs.push(PackView::open_verified(toolchain.pack(c)).map_err(|_| WorldError::Pack { krate: c.name })?);
    }
    let mut placed: BTreeMap<u32, (u32, &'w str), Alloc<'w>> = BTreeMap::new_in(alloc.clone());

    let mut crates: AVec<'w, Crate<'w>> = AVec::with_capacity_in(tc.len() + pending.len(), alloc.clone());
    let mut envs: AVec<'w, CrateEnv<'w>> = AVec::with_capacity_in(tc.len() + pending.len(), alloc.clone());
    for (i, c) in tc.iter().enumerate() {
        // A toolchain crate is a dependency: it sees `/package` (D96); it has no lock entry.
        let facts = EnvFacts {
            crate_name: c.name,
            package: None,
            bin: None,
            seen: SeenRoot::Package,
            read_folder: c.shown_from,
            build_script: false,
            script: None,
            test_or_bench: false,
        };
        envs.push(CrateEnv::of(facts, alloc.clone()).map_err(WorldError::Env)?);
        crates.push(Crate {
            input: CrateInput {
                name: c.name,
                key: c.key,
                edition: c.edition,
                implicit: c.implicit,
                root: c.root,
                shown_from: c.shown_from,
                features: c.features,
                deps: c.deps,
                cfg: c.cfg,
                script: None,
            },
            origin: Origin::Toolchain(c.unit),
            units: 0,
            script: Script::None,
            proc_macro: c.proc_macro,
            package: c.name,
            version: c.version,
        });
        debug_assert_eq!(crates.len(), i + 1);
    }
    for &c in order.iter() {
        let p = &pending[c as usize];
        let unit = &units[p.unit.index()];
        let (tree, dir) = match placed.get(&unit.package.0) {
            Some(&it) => it,
            None => {
                let place = match early.get(&unit.package.0) {
                    Some(&place) => place,
                    None => packages.place(unit.package)?,
                };
                let it = match place {
                    Place::Checkout { dir } => (0, dir),
                    Place::Pack { view, dir } => {
                        packs.push(view);
                        let tree = packs.len() as u32;
                        if let Some(row) = corrections.find_text(&unit.name, &unit.version) {
                            correct_pack(row, view, tree, dir, unit, &mut corrected, &mut used, alloc.clone())?;
                        }
                        (tree, dir)
                    }
                };
                placed.insert(unit.package.0, it);
                it
            }
        };
        let mut base = AString::with_capacity_in(dir.len() + 12, alloc.clone());
        base.push_str("/");
        push_u32(&mut base, tree);
        base.push_str("/");
        let shown = join(&base, dir, alloc.clone());
        let root = keep(&join(&shown, &unit.root, alloc.clone()), alloc.clone());
        let mut shown_from = shown;
        if !shown_from.ends_with('/') {
            shown_from.push_str("/");
        }
        let mut deps: AVec<'w, (&'w str, usize)> = AVec::with_capacity_in(p.deps.len() + 1, alloc.clone());
        for (name, d) in p.deps.iter() {
            deps.push((*name, (t + rank[*d as usize]) as usize));
        }
        let proc_macro = unit.kind == UnitKind::ProcMacro;
        if proc_macro {
            let pm = proc_macro_crate.ok_or(WorldError::NoToolchainCrate { role: "proc_macro" })?;
            deps.push(("proc_macro", pm as usize));
        }
        let mut features: AVec<'w, &'w str> = AVec::with_capacity_in(unit.features.len(), alloc.clone());
        features.extend(unit.features.iter().map(|f| f.as_str()));
        // What the crate sees as its root (D96): a member (placed in the checkout, tree 0) its
        // directory under the workspace, every fetched package the one root `/package`.
        let seen = if tree == 0 { SeenRoot::member(dir).map_err(|error| WorldError::MemberDir { package: &unit.name, error })? } else { SeenRoot::Package };
        let shown_from: &'w str = keep(&shown_from, alloc.clone());
        let facts = EnvFacts {
            crate_name: &unit.crate_name,
            package: Some((&unit.name, &unit.version)),
            bin: match &unit.kind {
                UnitKind::Bin(name) => Some(name.as_str()),
                _ => None,
            },
            seen,
            read_folder: shown_from,
            build_script: unit.build_script.is_some(),
            script: p.outputs,
            test_or_bench: matches!(unit.kind, UnitKind::IntegrationTest(_) | UnitKind::Bench(_)),
        };
        envs.push(CrateEnv::of(facts, alloc.clone()).map_err(WorldError::Env)?);
        crates.push(Crate {
            input: CrateInput {
                name: &unit.crate_name,
                key: p.key,
                edition: edition_of(unit.edition.syntax),
                implicit: implicit(unit.edition, &unit.kind),
                root,
                shown_from,
                features: features.leak(),
                deps: deps.leak(),
                cfg: p.cfg,
                script: p.outputs,
            },
            origin: Origin::Unit(p.unit),
            units: p.units,
            script: p.script,
            proc_macro,
            package: &unit.name,
            version: &unit.version,
        });
    }
    let mut crate_of_unit: AVec<'w, Option<u32>> = AVec::with_capacity_in(n, alloc.clone());
    crate_of_unit.extend(crate_of.iter().map(|c| c.map(|c| t + rank[c as usize])));
    let role = |i: Option<usize>| i.map(|i| CrateId(i as u32));
    let sysroot = Sysroot { core: role(toolchain.core()), alloc: role(toolchain.alloc()), std: role(toolchain.std()), bstd: role(toolchain.bstd()) };
    Ok(World { crates, texts: Texts { checkout, packs, corrected, envs }, sysroot, toolchain_crates: t, crate_of_unit, corrections: used })
}

/// One package's source corrections (D58) applied: each corrected file's text kept under
/// `(tree, path in the pack)` for [`Texts`] to read in place of the pack's, and what was done
/// noted for the output. A row that does not apply cleanly is the package's error.
#[expect(clippy::too_many_arguments, reason = "one call site, the placement loop's state")]
fn correct_pack<'w>(
    row: &blaze_fetch::correct::Row<Alloc<'w>>,
    view: PackView<'w>,
    tree: u32,
    dir: &'w str,
    unit: &'w blaze_unit_graph::Unit<'w>,
    corrected: &mut AVec<'w, (u32, &'w str, &'w str)>,
    used: &mut AVec<'w, Corrected<'w>>,
    alloc: Alloc<'w>,
) -> Result<(), WorldError<'w>> {
    let path_of = |file: &str| -> AString<'w> {
        let mut path = AString::with_capacity_in(dir.len() + file.len() + 1, alloc.clone());
        if !dir.is_empty() {
            path.push_str(dir);
            path.push_str("/");
        }
        path.push_str(file);
        path
    };
    let files = row
        .correct_crate(&unit.version, |file| view.find(&path_of(file)).map(|f| f.bytes), blaze_lex::Cpu::BASELINE, alloc.clone())
        .map_err(|why| {
            let mut words = AString::new_in(alloc.clone());
            let _ = fmt::write(&mut Words(&mut words), format_args!("{why}"));
            WorldError::Correction { package: &unit.name, version: &unit.version, why: keep(&words, alloc.clone()) }
        })?;
    let mut predicates = 0;
    for (file, fixed) in files.iter() {
        predicates += fixed.replaced.len();
        corrected.push((tree, keep(&path_of(file), alloc.clone()), keep(fixed.text.as_str(), alloc.clone())));
    }
    used.push(Corrected {
        package: &unit.name,
        version: &unit.version,
        files: files.len(),
        predicates,
        decision: keep(&row.decision, alloc.clone()),
    });
    Ok(())
}

/// `fmt::Write` into an arena string.
struct Words<'s, 'w>(&'s mut AString<'w>);

impl fmt::Write for Words<'_, '_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.0.push_str(s);
        Ok(())
    }
}

/// A source correction a world used (D58): named in the output.
#[derive(Clone, Copy, Debug)]
pub struct Corrected<'w> {
    pub package: &'w str,
    pub version: &'w str,
    pub files: usize,
    pub predicates: usize,
    pub decision: &'w str,
}

impl fmt::Display for Corrected<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "corrected: {} {}: {} predicates ({})", self.package, self.version, self.predicates, self.decision)
    }
}

fn push_u32(out: &mut AString<'_>, mut n: u32) {
    let mut digits = [0u8; 10];
    let mut at = digits.len();
    loop {
        at -= 1;
        digits[at] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    out.push_str(core::str::from_utf8(&digits[at..]).unwrap_or("0"));
}

/// One unit's own part of its input, given the crates its dependencies already are: its key,
/// its cfg (the unit's atoms joined with its build script's), its dependency edges by the
/// crate each is. What the driver's names task computes per unit.
pub struct UnitInput<'w> {
    pub key: CrateItemKey,
    pub cfg: &'w [(&'w str, Option<&'w str>)],
    /// (extern name, the dependency's crate as `crate_of` gave it)
    pub deps: AVec<'w, (&'w str, u32)>,
}

/// The key of what a unit given `implicit` reaches of the toolchain without naming it: its
/// std (Rust's `std` or `bstd`, whose key holds `alloc`'s and `core`'s) and, for a proc-macro
/// unit, `proc_macro`. A toolchain change moves it, so it moves every unit's key.
pub fn implicit_key<'w, B: AsRef<[u8]>>(toolchain: &Toolchain<'w, B>, implicit: blaze_unit_graph::Implicit) -> Result<CrateItemKey, WorldError<'w>> {
    let (std, role) = match implicit.std {
        blaze_unit_graph::StdIs::Rust => (toolchain.std(), "std"),
        blaze_unit_graph::StdIs::Bstd(_) => (toolchain.bstd(), "bstd"),
    };
    let std = std.ok_or(WorldError::NoToolchainCrate { role })?;
    let mut bytes = [0u8; 65];
    bytes[..32].copy_from_slice(&toolchain.crates()[std].key.0.0);
    if implicit.proc_macro {
        let pm = toolchain.proc_macro().ok_or(WorldError::NoToolchainCrate { role: "proc_macro" })?;
        bytes[32..64].copy_from_slice(&toolchain.crates()[pm].key.0.0);
        bytes[64] = 1;
    }
    Ok(CrateItemKey(ContentId::of("blaze world implicit 1", &bytes)))
}

/// [`UnitInput`] of unit `id`: every dependency of it must have its crate in `crate_of`, and
/// `crate_key` gives each crate's key; `implicit_extern` is [`implicit_key`] of the unit's implicit
/// externs; `script` is the build script's outputs when known: its atoms join the cfg and its
/// [`Served::digest`] (everything of the record the crate can read: cfgs, env pairs, generated
/// files) is in the key; `member` is a workspace member's directory in the checkout (the root
/// it sees, D96), in the key.
///
/// The key is the crate's IDENTITY, never its text: its item ids are made from it, so a body
/// or `pub fn` edit must leave it equal. Its text is pinned by its package's version and
/// source for a fetched crate; a workspace crate's text is read by the work keyed on it (the
/// driver's `CrateScope` key adds the crate's surface digest).
#[allow(clippy::too_many_arguments)]
pub fn unit_input<'w>(
    graph: &'w UnitGraph<'w>,
    lock: &Lock<'w>,
    id: UnitId,
    crate_of: &[Option<u32>],
    crate_key: &dyn Fn(u32) -> CrateItemKey,
    implicit_extern: CrateItemKey,
    script: Option<Served<'w>>,
    member: Option<&str>,
    corrections: &blaze_fetch::correct::Table<Alloc<'w>>,
    alloc: Alloc<'w>,
) -> UnitInput<'w> {
    let unit = &graph.units[id.index()];
    let mut cfg: AVec<'w, (&'w str, Option<&'w str>)> = AVec::with_capacity_in(unit.cfg.len(), alloc.clone());
    cfg.extend(unit.cfg.iter().map(|a| (a.name.as_str(), a.value.as_deref())));
    if let Some(served) = script {
        cfg.extend_from_slice(served.cfg);
    }
    cfg.sort_unstable();
    cfg.dedup();
    let mut deps: AVec<'w, (&'w str, u32)> = AVec::new_in(alloc.clone());
    if unit.kind != UnitKind::ProcMacro {
        for dep in unit.deps.iter() {
            let c = crate_of[dep.unit.index()].expect("a dependency is keyed before its dependents");
            deps.push((dep.name.as_str(), c));
        }
    }
    let package = lock.package(unit.package);
    let mut bytes: AVec<'_, u8> = AVec::new_in(alloc.clone());
    fn put(bytes: &mut AVec<'_, u8>, s: &[u8]) {
        bytes.extend_from_slice(&(s.len() as u32).to_le_bytes());
        bytes.extend_from_slice(s);
    }
    let text = &mut |s: &str| put(&mut bytes, s.as_bytes());
    text(&package.name);
    text(&unit.version);
    text(&package.source_text);
    text(kind_name(&unit.kind));
    text(&unit.crate_name);
    text(&unit.root);
    text(&alloc_free_debug(&implicit(unit.edition, &unit.kind)));
    text(match unit.edition.syntax {
        edition::Edition::Edition2015 => "2015",
        edition::Edition::Edition2018 => "2018",
        edition::Edition::Edition2021 => "2021",
        edition::Edition::Edition2024 => "2024",
    });
    for f in unit.features.iter() {
        text(f);
    }
    text("\u{0}cfg");
    for (name, value) in cfg.iter() {
        text(name);
        text(value.unwrap_or("\u{0}none"));
    }
    text("\u{0}deps");
    for (name, c) in deps.iter() {
        put(&mut bytes, name.as_bytes());
        put(&mut bytes, &crate_key(*c).0.0);
    }
    put(&mut bytes, b"\0implicit");
    put(&mut bytes, &implicit_extern.0.0);
    if let Some(served) = script {
        put(&mut bytes, b"\0script");
        put(&mut bytes, &served.digest.0);
    }
    // A member's directory: the root it sees is `/workspace/<dir>` (D96 (3), rule 21).
    if let Some(dir) = member {
        put(&mut bytes, b"\0member");
        put(&mut bytes, dir.as_bytes());
    }
    // A package read corrected (D58) is keyed by the row too.
    if let Some(row) = corrections.find_text(&package.name, &unit.version) {
        put(&mut bytes, b"\0corrected");
        put(&mut bytes, &row.digest());
    }
    let key = CrateItemKey(ContentId::of("blaze world crate 2", &bytes));
    UnitInput { key, cfg: cfg.leak(), deps }
}

/// The roles a graph needs of the toolchain.
pub fn needs(graph: &UnitGraph<'_>) -> Needs {
    let mut n = Needs { std: false, bstd: false, proc_macro: false };
    for u in graph.units.iter().filter(|u| held(&u.kind, u.for_host)) {
        match implicit(u.edition, &u.kind).std {
            blaze_unit_graph::StdIs::Rust => n.std = true,
            blaze_unit_graph::StdIs::Bstd(_) => n.bstd = true,
        }
        n.proc_macro |= u.kind == UnitKind::ProcMacro;
    }
    n
}
