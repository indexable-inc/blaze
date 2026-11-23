//! One world for run and check (docs/v2/plan/design/typeck.md, "Checker at scale", lane 1).
//! Tonight it holds the names library's inputs: a project's unit graph made into one
//! [`CrateInput`] per crate, for `blaze_scope::resolve::resolve_crates`.
//!
//! - **Which crates.** blaze-scope's units list, the one home of the crate list (AGENTS.md rule
//!   30; the `list` module's documentation): the target-side units the members reach, each proc
//!   macro, a MEMBER's proc macro's dependency closure again on the macro side (`wasm32-wasip1`,
//!   rule 10, [`Side::Macro`]) or its typed refusal ([`World::refused`]), and the toolchain's
//!   crates they need. Build scripts are not crates of the names set. A kept unit's dependency
//!   on a unit not kept is the list's typed error ([`WorldError::Units`]), never an index made up.
//! - **One crate per configuration.** Units with equal inputs (a library built for the host
//!   and for the target, the Mac's host being its target) are one crate: a crate's key is the
//!   digest of its package's identity, kind, edition, root, features, cfg and its dependencies'
//!   keys, computed before any file is read (rule 65).
//! - **Each crate's input.** Its root file, found by the names library's own walker from there;
//!   its extern prelude from the unit's dependency edges (renames as written) plus the implicit
//!   toolchain crates (blaze-unit-graph's `implicit`, through [`Sysroot`]); `proc_macro` as an
//!   edge of a proc-macro crate; its cfg the unit's atoms. A build script's outputs are NOT in
//!   the world (decision D115 (B)): the crate's task reads its unit's `Script` record and joins
//!   it then ([`joined`]: its `cargo::rustc-cfg` atoms, `CrateInput::script`).
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

#[cfg(any(test, feature = "std"))]
extern crate std;

#[cfg(feature = "std")]
mod checkout;
pub mod check_scope;
pub mod crate_list;
pub mod fixture;
pub mod lib_features_record;
#[cfg(feature = "procs")]
pub mod procs;
mod list;
pub mod panic;
mod script;
pub mod script_setting;
#[cfg(test)]
mod script_setting_tests;
#[cfg(feature = "std")]
mod stored_files;
mod toolchain;
#[cfg(test)]
mod tests;

use core::fmt;

use blaze_fetch::PackView;
use blaze_id::{ContentId, CrateItemKey};
use blaze_manifest::lock::{Lock, LockId};
use blaze_own::{AString, AVec, Alloc, Bump};
use blaze_scope::crate_env::{CrateEnv, MemberDirError};
use blaze_scope::graph::Sysroot;
use blaze_scope::resolve::{CrateInput, Names, NamesError, SourceTexts, resolve_crates};
use blaze_scope::task::Par;
use blaze_scope::units::{RefusedCrate, UnitsError};
use blaze_unit_graph::{SourceError, StoreSources, UnitId};
use blaze_unit_graph::sources::Records;

#[cfg(feature = "std")]
pub use checkout::Checkout;
pub use list::{Listed, ScriptWorld, Side, crate_list, needs, open_list, open_list_light, script_world, world, world_from_list};
pub use script::{ScriptRecords, ScriptRecordsError, joined, script_subject};
#[cfg(feature = "std")]
pub use stored_files::StoredFiles;
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

/// Whether a checkout path package is a workspace member or an external path package (from
/// `[patch.*]` or a path dependency outside `workspace.members`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PathKind {
    Member,
    External,
}

/// One path package in the checkout (tree 0): its lock entry, directory (relative to the
/// checkout root), and whether it is a workspace member or an external path dependency.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CheckoutPath<'w> {
    pub id: LockId,
    pub dir: &'w str,
    pub kind: PathKind,
}

/// Where a package's files are.
#[derive(Clone, Copy)]
pub enum Place<'w> {
    /// A workspace member in the checkout (tree 0), under this directory (relative to the
    /// checkout's root, no trailing `/`; `""` for the root).
    Checkout { dir: &'w str },
    /// A non-member path package in the checkout (tree 0), e.g. from `[patch.*]` or a path
    /// dependency outside `workspace.members`: files are read from tree 0 under `dir`, and it
    /// is treated as a dependency rather than a workspace member.
    Path { dir: &'w str },
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
    /// Each path package in the checkout, by lock id.
    pub paths: &'s [CheckoutPath<'w>],
    pub lock: &'l Lock<'w>,
    pub alloc: Bump<'w>,
}

impl<'w, R: Records<'w>> Packages<'w> for Fetched<'_, '_, 'w, R> {
    fn place(&mut self, package: LockId) -> Result<Place<'w>, WorldError<'w>> {
        let p = self.lock.package(package);
        if matches!(p.source, blaze_manifest::lock::LockSource::Path) {
            return match self.paths.iter().find(|entry| entry.id == package) {
                Some(CheckoutPath { dir, kind: PathKind::Member, .. }) => Ok(Place::Checkout { dir }),
                Some(CheckoutPath { dir, kind: PathKind::External, .. }) => Ok(Place::Path { dir }),
                None => Err(WorldError::PathNotPlaced { package: self.alloc.alloc_str(&p.name) }),
            };
        }
        let (view, dir) = self.store.files(package).map_err(WorldError::Source)?;
        Ok(Place::Pack { view, dir: self.alloc.alloc_str(&dir) })
    }
}

/// What a crate is made from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Origin {
    /// A toolchain unit (its index in the toolchain manifest's unit list).
    Toolchain(u32),
    /// The first unit of the graph (in the graph's order) with these inputs.
    Unit(UnitId),
}

/// Whether a crate has a build script whose outputs its tasks read (decision D115 (B)).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Script {
    /// Its package has no build script.
    None,
    /// It has one, run for THIS crate's side (ruling D115 (2): a macro-side copy reads its
    /// macro-side run, TARGET wasm32-wasip1): each task of the crate Needs the `Script` record
    /// named by `subject` ([`script_subject`]) and joins it ([`joined`]); the world's input
    /// holds the unit's cfg alone and no record.
    Read { subject: ContentId },
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
    /// Whether this crate belongs to a workspace member (`Place::Checkout`), as opposed to an
    /// external path package (`Place::Path`), fetched package, or toolchain crate.
    pub member: bool,
    /// The package's name and version, as the lock writes them (the toolchain's crate's for its).
    pub package: &'w str,
    pub version: &'w str,
    /// Which side it is built for: a member proc macro, its library test harness, their
    /// dependencies (and that side's toolchain crates) are [`Side::Macro`]'s.
    pub side: Side,
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
    /// The crates that cannot be made, each at its index in [`Self::crates`] with its typed
    /// refusal (the list's: a member proc macro with no `wasm32-wasip1` cut is
    /// [`UnitsError::NoMacroToolchain`]). Sorted by index, each once. Whoever asks for such a
    /// crate's names is given this, naming it (rule 20); it is never dropped.
    pub refused: AVec<'w, RefusedCrate<'w>>,
}

impl<'w> World<'w> {
    /// Why the crate at `krate` cannot be made, when it cannot ([`Self::refused`]).
    pub fn refusal(&self, krate: usize) -> Option<&UnitsError<'w>> {
        self.refused.iter().find(|r| r.krate == krate).map(|r| &r.why)
    }

    /// The inputs `resolve_crates` takes, in crate order.
    pub fn inputs(&self, alloc: Alloc<'w>) -> AVec<'w, CrateInput<'w>> {
        let mut out = AVec::with_capacity_in(self.crates.len(), alloc);
        out.extend(self.crates.iter().map(|c| c.input));
        out
    }

    /// Every crate's names, resolved together.
    pub fn resolve<P: Par>(&self, inputs: &[CrateInput<'w>], par: &P, alloc: Bump<'w>) -> Result<Names<'w>, NamesError<'w>> {
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
            match c.script {
                Script::None => out.push(0),
                Script::Read { subject } => {
                    out.push(1);
                    out.extend_from_slice(&subject.0);
                }
            }
            out.push(u8::from(c.proc_macro));
            out.push(c.side as u8);
        }
        out.extend_from_slice(&(self.refused.len() as u32).to_le_bytes());
        for r in self.refused.iter() {
            out.extend_from_slice(&(r.krate as u32).to_le_bytes());
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

/// Where a tree's pack is kept, by content, never by a path of this machine (rule 25): what a
/// reader that makes no world opens it by, through the crate list's tree table
/// (crate_list.rs, blaze-cli body_types.rs `Kept::packs`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PackOrigin {
    /// A toolchain crate's pack on the toolchain's shelf: the file named by its BLAKE3, which is
    /// the pack's footer (`blaze_toolchain::shelf::open_pack` checked the two are one).
    Shelf { digest: [u8; 32] },
    /// A fetched package's record in the machine's sources store, at the key the lock's fetch
    /// plan gives its unit (`blaze_fetch::SourceId::key`).
    Source { key: ContentId },
}

/// The texts `resolve_crates` reads: tree 0 the checkout, then each pack.
pub struct Texts<'w> {
    checkout: &'w dyn Tree,
    packs: AVec<'w, PackView<'w>>,
    /// Each pack's origin, in [`Self::packs`]'s order: tree `k` (1 and up) is `origins[k - 1]`.
    origins: AVec<'w, PackOrigin>,
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

    /// Where each pack of [`Self::packs`] is kept, in the same order.
    pub fn origins(&self) -> &[PackOrigin] {
        &self.origins
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
    /// The crate list (blaze-scope's units list) cannot be made: a kept unit depends on one not
    /// kept, a root leaves its package, no sysroot for the triple (each names the crate).
    Units(UnitsError<'w>),
    /// A crate of the list whose toolchain crate was not opened ([`Listed`]'s packs).
    ToolchainCrateNotOpened { krate: &'w str },
    /// A macro-side toolchain crate's root is not under the folder it is read from.
    ToolchainRoot { krate: &'w str, root: &'w str },
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
    /// A fetched package's record whose key the lock's fetch plan does not give (no unit of
    /// the plan is that package's, or its record is of no fetched kind): the crate list's tree
    /// table names each fetched tree by that key ([`PackOrigin::Source`]).
    SourceRecord { package: &'w str },
    /// A source correction (D58, `blaze_fetch::correct`) refused this package: the words say
    /// which file and why (a stale row, a missing file).
    Correction { package: &'w str, version: &'w str, why: &'w str },
    /// A member's directory cannot be seen at `/workspace/<dir>` (D96).
    MemberDir { package: &'w str, error: MemberDirError<'w> },
    /// A crate's environment cannot be made (its version has not three parts).
    Env(blaze_scope::crate_env::CrateEnvError<'w>),
    /// Two units under one crate key name two build-script runs ([`script_subject`]): the key
    /// misses what tells the runs apart (rule 21). Unreachable by construction.
    ScriptClash { first: &'w str, second: &'w str },
}

impl fmt::Display for WorldError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WorldError::Source(e) => write!(f, "{e}"),
            WorldError::MemberDir { package, error } => write!(f, "the member `{package}`: {error}"),
            WorldError::Env(e) => write!(f, "{e}"),
            WorldError::ScriptClash { first, second } => write!(
                f,
                "the crates `{first}` and `{second}` have one crate key but name two build-script runs: the key misses what tells the runs apart (rule 21; blaze-world `script_subject`)"
            ),
            WorldError::PathNotPlaced { package } => write!(f, "the path package `{package}` is in no directory the workspace's layout found"),
            WorldError::Correction { package, version, why } => write!(f, "{package} {version}: {why}"),
            WorldError::Units(e) => write!(f, "the crate list: {e}"),
            WorldError::ToolchainCrateNotOpened { krate } => write!(f, "the toolchain crate `{krate}` of the crate list was not opened from the shelf"),
            WorldError::ToolchainRoot { krate, root } => write!(f, "the macro side's toolchain crate `{krate}`: its root `{root}` is not under its folder"),
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
            WorldError::SourceRecord { package } => write!(
                f,
                "the package `{package}`'s files were placed from a fetched record the lock's fetch plan holds no unit of (blaze-fetch `plan`), so the crate list cannot name the record's key"
            ),
        }
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
    bump: Bump<'w>,
) -> Result<(), WorldError<'w>> {
    let alloc = bump.alloc();
    let path_of = |file: &str| -> AString<'w> {
        let mut path = AString::with_capacity_in(dir.len() + file.len() + 1, alloc);
        if !dir.is_empty() {
            path.push_str(dir);
            path.push_str("/");
        }
        path.push_str(file);
        path
    };
    let files = row
        .correct_crate(&unit.version, |file| view.find(&path_of(file)).map(|f| f.bytes), blaze_lex::Cpu::BASELINE, alloc)
        .map_err(|why| {
            let mut words = AString::new_in(alloc);
            let _ = fmt::write(&mut Words(&mut words), format_args!("{why}"));
            WorldError::Correction { package: &unit.name, version: &unit.version, why: bump.alloc_str(&words) }
        })?;
    let mut predicates = 0;
    for (file, fixed) in files.iter() {
        predicates += fixed.replaced.len();
        corrected.push((tree, bump.alloc_str(&path_of(file)), bump.alloc_str(fixed.text.as_str())));
    }
    used.push(Corrected {
        package: &unit.name,
        version: &unit.version,
        files: files.len(),
        predicates,
        decision: bump.alloc_str(&row.decision),
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
