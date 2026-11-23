//! The crates a blaze-edition program is made of, and their names: the program's own crate
//! and the toolchain's library units it reaches (bstd as its `std`, core, alloc and what bstd
//! depends on), read from the installed toolchain's verified packs, then resolved together by
//! `blaze_scope::resolve::resolve_crates`.
//!
//! Each toolchain crate's files are under a path of its own, `/toolchain/<unit>/` followed by
//! the file's path in its pack: a tree crate's pack paths start with its directory, so a
//! `#[path]` that leaves the directory (core's `stdarch/..`) resolves as on disk.
//!
//! A crate's cfg is the target's atoms as the toolchain record states them
//! (`ToolchainView::cfg`), the profile's, and a `feature` atom per feature on.

use core::cell::RefCell;
use core::fmt;

use blaze_fetch::PackView;
use blaze_id::{ContentId, CrateItemKey};
use blaze_mem::{Frame, Tag};
use blaze_own::{AVec, Alloc, Bump, Erased};
use blaze_scope::graph::{CrateId, Sysroot};
use blaze_scope::resolve::{CrateInput, Names, NamesError, SourceTexts, resolve_crates};
use blaze_scope::task::{Par, Scratch, Serial};
use blaze_surface::Edition;
use blaze_toolchain::{Role, Shelf, ToolchainError, ToolchainId, ToolchainView, UnitKind};
use blaze_unit_graph::implicit;
use edition::PackageEdition;

pub mod library;

/// The one target the ladder runs on.
pub const TRIPLE: &str = "aarch64-apple-darwin";

/// The profile's atoms, beside the target's: the ladder builds as `dev`.
const PROFILE: &[(&str, Option<&str>)] = &[("debug_assertions", None)];

/// The installed toolchain's files: `<home>/.blaze/v2/toolchain/<64 hex digits>`.
pub struct DirShelf {
    place: String,
}

impl DirShelf {
    pub fn new(place: String) -> DirShelf {
        DirShelf { place }
    }
}

/// A file of the shelf mapped read-only and read in place: the pack's bytes are the file's
/// pages, never a heap copy. The shelf's files are written once by the toolchain's install and
/// only ever replaced by renaming another over them (`Mapping::map_file`'s contract).
pub struct ShelfFile(blaze_core::Mapping);

impl AsRef<[u8]> for ShelfFile {
    fn as_ref(&self) -> &[u8] {
        self.0.bytes()
    }
}

impl Shelf for DirShelf {
    type Error = std::io::Error;
    type Bytes = ShelfFile;

    fn place(&self) -> &str {
        &self.place
    }

    fn size(&mut self, name: &str) -> Result<Option<u64>, std::io::Error> {
        match std::fs::metadata(format!("{}/{name}", self.place)) {
            Ok(m) => Ok(Some(m.len())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn read(&mut self, name: &str, _size: u64) -> Result<ShelfFile, std::io::Error> {
        let path = format!("{}/{name}", self.place);
        blaze_core::Mapping::map_file(std::path::Path::new(&path)).map(ShelfFile)
    }
}

/// Why the program's crates could not be made.
#[derive(Debug)]
pub enum CratesError {
    Toolchain(ToolchainError),
    /// The manifest has no sysroot for [`TRIPLE`].
    NoTarget,
    /// A pack opened and verified but is no pack form (cannot happen after `open_pack`).
    Pack { krate: String },
    /// A toolchain crate's edition is none of 2015 to 2024.
    Edition { krate: String, written: String },
    /// A kept unit depends on a unit the closure does not keep (not a target-side library).
    DependencyNotKept { krate: String, dep: String, kind: String, for_host: bool },
}

impl fmt::Display for CratesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CratesError::Toolchain(e) => write!(f, "{e}"),
            CratesError::NoTarget => write!(f, "the toolchain holds no sysroot for `{TRIPLE}`"),
            CratesError::Pack { krate } => write!(f, "the toolchain's pack of `{krate}` is no pack"),
            CratesError::Edition { krate, written } => write!(f, "the toolchain crate `{krate}` has the edition `{written}`, no Rust edition"),
            CratesError::DependencyNotKept { krate, dep, kind, for_host } => {
                write!(f, "the toolchain crate `{krate}` depends on `{dep}`, a {kind} unit (for host: {for_host}) the program's crates do not hold")
            }
        }
    }
}

fn edition_of(written: &str) -> Option<Edition> {
    Some(match written {
        "2015" => Edition::E2015,
        "2018" => Edition::E2018,
        "2021" => Edition::E2021,
        "2024" => Edition::E2024,
        _ => return None,
    })
}

/// The program's own crate: its files (paths absolute), its root, its edition.
pub struct UserCrate<'u> {
    pub name: &'u str,
    pub dir: &'u str,
    pub root: &'u str,
    pub files: &'u [(String, String)],
    pub edition: PackageEdition,
    /// The program's ONE panic strategy (its profile's `panic`; UNWIND.md 3.1): the one
    /// carrier of that fact. Crate 0's `cfg(panic)` is derived from it ([`Inputs::new`]) and
    /// front2 reads it back from there (`panic_strategy::of_program`). The Reference,
    /// linkage.html [link.unwinding.prohibited]: "If a Rust artifact is potentially unwinding,
    /// then all its crates must be built with the unwind panic strategy."
    pub panic: blaze_build::Panic,
}

/// One toolchain unit the program reaches: its crate's name and pack, its root, features and
/// dependencies (by index among the units kept).
struct Unit {
    name: String,
    edition: Edition,
    root: String,
    shown_from: String,
    features: Vec<String>,
    deps: Vec<(String, usize)>,
    /// The pack, mapped from the shelf (read in place).
    pack: ShelfFile,
    key: CrateItemKey,
    role: Role,
    /// A proc-macro unit: Rust's edition rule (`std` is Rust's, `proc_macro` in its prelude),
    /// its dependencies not in the graph.
    proc_macro: bool,
}

/// The toolchain units a program reaches, their packs read and verified once.
pub struct Toolchain {
    units: Vec<Unit>,
    /// The target's cfg atoms, as the record states them.
    cfg: Vec<(String, Option<String>)>,
}

impl Toolchain {
    /// No toolchain crate: for a `#![no_core]` program, which names nothing outside itself (no
    /// toolchain resolve per run). A program that names `core` or `std` over it does not resolve.
    /// Its cfg is still [`TRIPLE`]'s whole target set ([`target_cfg`]): a `no_core` program's
    /// pattern type (`usize is 0..=HALF`) reads `target_pointer_width` as any crate's does.
    /// `alloc` is the caller's scratch for the target's cfg, copied out before this returns.
    pub fn none(alloc: Alloc<'_>) -> Toolchain {
        Toolchain { units: Vec::new(), cfg: target_cfg(alloc) }
    }

    /// Opens the shelf's pinned manifest and reads the packs of every unit reachable from bstd,
    /// core and alloc for [`TRIPLE`] (not the host side).
    pub fn open(shelf: &mut DirShelf) -> Result<Toolchain, CratesError> {
        let id: ToolchainId = blaze_toolchain::PINNED;
        let manifest = blaze_toolchain::shelf::open_manifest(shelf, id).map_err(CratesError::Toolchain)?;
        let view = ToolchainView::open(manifest.as_ref(), id).expect("open_manifest checked it");
        let sys = blaze_toolchain::sysroot(&view, TRIPLE, false).ok_or(CratesError::NoTarget)?;
        let all: Vec<_> = view.units(TRIPLE).ok_or(CratesError::NoTarget)?.collect();
        // The closure of bstd, core, alloc and Rust's std over dependencies, by a work list:
        // std because the Rust-edition crates beneath bstd (blaze_mem) name it implicitly.
        let mut keep = vec![false; all.len()];
        let mut todo = vec![sys.bstd.0 as usize, sys.core.0 as usize, sys.alloc.0 as usize, sys.std.0 as usize];
        while let Some(u) = todo.pop() {
            if keep[u] {
                continue;
            }
            keep[u] = true;
            // A proc macro is a crate of the graph (its macros are what its dependents' paths
            // name), but what it is built from is the host's: not followed.
            if all[u].kind != UnitKind::ProcMacro {
                todo.extend(all[u].deps().map(|(_, d)| d as usize));
            }
        }
        let mut at = vec![usize::MAX; all.len()];
        let mut units = Vec::new();
        for (u, row) in all.iter().enumerate() {
            let proc_macro = row.kind == UnitKind::ProcMacro;
            if !keep[u] || !(proc_macro || (row.kind == UnitKind::Lib && !row.for_host)) {
                continue;
            }
            let krate = view.krate(row.krate);
            let edition = edition_of(krate.edition)
                .ok_or_else(|| CratesError::Edition { krate: krate.name.into(), written: krate.edition.into() })?;
            let pack = blaze_toolchain::shelf::open_pack(shelf, id, &krate).map_err(CratesError::Toolchain)?;
            let base = format!("/toolchain/{u}/");
            let root = if krate.dir.is_empty() || row.root.starts_with(krate.dir) {
                format!("{base}{}", row.root)
            } else {
                format!("{base}{}/{}", krate.dir, row.root)
            };
            at[u] = units.len();
            units.push(Unit {
                name: krate.lib.into(),
                edition,
                root,
                shown_from: base,
                features: row.features().map(str::to_owned).collect(),
                // A proc macro's dependencies are the host's, not in this graph.
                deps: if proc_macro { Vec::new() } else { row.deps().map(|(n, d)| (n.to_owned(), d as usize)).collect() },
                proc_macro,
                key: CrateItemKey(ContentId::of("blaze toolchain unit", &unit_key(&krate.pack, row.features()))),
                pack,
                role: krate.role,
            });
        }
        // Dependencies by index among the kept units: a dependency on a unit not kept (a host
        // unit, a proc macro) is refused by name, never an index past the kept ones.
        for unit in &mut units {
            for dep in &mut unit.deps {
                let kept = at[dep.1];
                if kept == usize::MAX {
                    let row = &all[dep.1];
                    return Err(CratesError::DependencyNotKept {
                        krate: unit.name.clone(),
                        dep: dep.0.clone(),
                        kind: format!("{:?}", row.kind),
                        for_host: row.for_host,
                    });
                }
                dep.1 = kept;
            }
        }
        let cfg = view.cfg(TRIPLE).ok_or(CratesError::NoTarget)?.map(|(n, v)| (n.to_owned(), v.map(str::to_owned))).collect();
        Ok(Toolchain { units, cfg })
    }
}

/// [`TRIPLE`]'s cfg atoms with no toolchain record open: blaze-scope's target list
/// (`Cfg::of(&Target::AARCH64_APPLE_DARWIN, ..)`, crates/blaze-scope/src/task/lower.rs), the
/// one list that names every target atom (`target_pointer_width`, `target_os`, ..). Asserted
/// equal in triple by the constant's name: [`TRIPLE`] is `aarch64-apple-darwin`.
fn target_cfg(alloc: Alloc<'_>) -> Vec<(String, Option<String>)> {
    let target = blaze_scope::task::Target::AARCH64_APPLE_DARWIN;
    let cfg = blaze_scope::task::Cfg::of(&target, &[], alloc);
    cfg.atoms().iter().map(|&(n, v)| (n.to_owned(), v.map(str::to_owned))).collect()
}

/// A unit's key: its crate's pack digest and its features.
fn unit_key<'f>(pack: &[u8; 32], features: impl Iterator<Item = &'f str>) -> Vec<u8> {
    let mut out = pack.to_vec();
    for f in features {
        out.extend_from_slice(f.as_bytes());
        out.push(0);
    }
    out
}

/// Either owned by [`Inputs::new`] or borrowed from a solved [`library::Library`].
pub(crate) enum Shared<'t, T> {
    Owned(Vec<T>),
    Borrowed(&'t [T]),
}

impl<T> core::ops::Deref for Shared<'_, T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        match self {
            Self::Owned(v) => v,
            Self::Borrowed(s) => s,
        }
    }
}

/// The texts `resolve_crates` reads: the program's files, then each unit's pack.
pub struct Texts<'t> {
    user: &'t [(String, String)],
    packs: Shared<'t, PackView<'t>>,
}

impl<'t> SourceTexts<'t> for Texts<'t> {
    fn read(&self, krate: usize, path: &str) -> Option<&'t str> {
        if krate == 0 {
            return self.user.iter().find(|(p, _)| p == path).map(|(_, t)| t.as_str());
        }
        let rest = path.strip_prefix("/toolchain/")?;
        let (_, inner) = rest.split_once('/')?;
        let file = self.packs.get(krate - 1)?.find(inner)?;
        core::str::from_utf8(file.bytes).ok()
    }
}

/// The syntax edition a package's edition is read in.
pub fn syntax_of(e: PackageEdition) -> Edition {
    match e.syntax {
        edition::Edition::Edition2015 => Edition::E2015,
        edition::Edition::Edition2018 => Edition::E2018,
        edition::Edition::Edition2021 => Edition::E2021,
        edition::Edition::Edition2024 => Edition::E2024,
    }
}

/// The program crate's `Implicit`: a binary of its package's edition (under a blaze edition its
/// root's `fn main` is `std::main`'s input, D104). The one home of it: [`Inputs::crates`] gives
/// it to the names library, and `blaze run`'s choice of process (run.rs `entry_start`) reads
/// its `std` before any name is resolved.
pub fn program_implicit(edition: PackageEdition) -> blaze_unit_graph::Implicit {
    implicit(edition, &blaze_unit_graph::UnitKind::Lib).binary()
}

impl Toolchain {
    /// The program crate's cfg atoms (the target's and the profile's; it has no features), as
    /// [`Inputs::crates`] gives them: what the entry's choice (front2 `entry`) lowers `main` by
    /// before names resolve. `panic` is the program's strategy: its `cfg(panic)` is derived from
    /// it ([`crate::panic_strategy::derive_cfg`]), never the target list's.
    pub fn program_cfg(&self, panic: blaze_build::Panic) -> Vec<(&str, Option<&str>)> {
        let mut atoms: Vec<(&str, Option<&str>)> = self.cfg.iter().map(|(n, v)| (n.as_str(), v.as_deref())).chain(PROFILE.iter().copied()).collect();
        crate::panic_strategy::derive_cfg(&mut atoms, panic);
        atoms
    }
}

/// The inputs of one `resolve_crates` call: the program's crate first, then the units.
pub struct Inputs<'t> {
    pub texts: Texts<'t>,
    features: Shared<'t, Vec<&'t str>>,
    /// Each unit's cfg: the target's atoms and a `feature` atom per feature on.
    cfgs: Shared<'t, Vec<(&'t str, Option<&'t str>)>>,
    deps: Shared<'t, Vec<(&'t str, usize)>>,
    user: &'t UserCrate<'t>,
    tc: &'t Toolchain,
    sysroot: Sysroot,
}

impl<'t> Inputs<'t> {
    pub fn new(user: &'t UserCrate<'t>, tc: &'t Toolchain) -> Result<Inputs<'t>, CratesError> {
        let mut packs = Vec::with_capacity(tc.units.len());
        for u in &tc.units {
            packs.push(PackView::open_verified(u.pack.as_ref()).map_err(|_| CratesError::Pack { krate: u.name.clone() })?);
        }
        let role = |r: Role| tc.units.iter().position(|u| u.role == r).map(|i| CrateId(i as u32 + 1));
        let sysroot = Sysroot { core: role(Role::Core), alloc: role(Role::Alloc), std: role(Role::Std), bstd: role(Role::Bstd) };
        let mut features = vec![Vec::new()];
        let mut deps = vec![Vec::new()];
        for u in &tc.units {
            features.push(u.features.iter().map(String::as_str).collect());
            deps.push(u.deps.iter().map(|(n, d)| (n.as_str(), d + 1)).collect());
        }
        let mut cfgs: Vec<Vec<(&str, Option<&str>)>> = features
            .iter()
            .map(|fs| {
                let target = tc.cfg.iter().map(|(n, v)| (n.as_str(), v.as_deref()));
                target.chain(PROFILE.iter().copied()).chain(fs.iter().map(|f| ("feature", Some(*f)))).collect()
            })
            .collect();
        // The program crate's `cfg(panic)` is its inputs' strategy; the toolchain's crates keep
        // the strategy their records were cut under (the target list's), as Rust does with std.
        crate::panic_strategy::derive_cfg(&mut cfgs[0], user.panic);
        Ok(Inputs {
            texts: Texts { user: user.files, packs: Shared::Owned(packs) },
            features: Shared::Owned(features),
            cfgs: Shared::Owned(cfgs),
            deps: Shared::Owned(deps),
            user,
            tc,
            sysroot,
        })
    }

    /// The crate inputs, the program's at index 0.
    pub fn crates(&'t self) -> Vec<CrateInput<'t>> {
        let rust = PackageEdition { syntax: edition::Edition::Edition2024, blaze: None };
        let lib = implicit(rust, &blaze_unit_graph::UnitKind::Lib);
        let proc_macro = implicit(rust, &blaze_unit_graph::UnitKind::ProcMacro);
        let mut out = Vec::with_capacity(self.tc.units.len() + 1);
        let syntax = syntax_of(self.user.edition);
        out.push(CrateInput {
            name: self.user.name,
            key: CrateItemKey(ContentId::of("blaze program crate", self.user.name.as_bytes())),
            edition: syntax,
            // The program crate is a binary: under a blaze edition its root's `fn main` is
            // `std::main`'s input (D104, `Implicit::binary`).
            implicit: program_implicit(self.user.edition),
            root: self.user.root,
            shown_from: self.user.dir,
            features: &[],
            deps: &self.deps[0],
            cfg: &self.cfgs[0],
            script: None,
        });
        for (i, u) in self.tc.units.iter().enumerate() {
            out.push(CrateInput {
                name: &u.name,
                key: u.key,
                edition: u.edition,
                implicit: if u.proc_macro { proc_macro } else { lib },
                root: &u.root,
                shown_from: &u.shown_from,
                features: &self.features[i + 1],
                deps: &self.deps[i + 1],
                cfg: &self.cfgs[i + 1],
                script: None,
            });
        }
        out
    }

    /// Every crate's names, resolved together, held in `bump` (the caller's arena, alive as long as
    /// the names), with per-call macro scratch rolled back after each expansion.
    pub fn resolve(&'t self, crates: &[CrateInput<'t>], bump: Bump<'t>) -> Result<Names<'t>, NamesError<'t>> {
        let par = ScratchSerial::new();
        self.resolve_with(crates, &par, bump)
    }

    /// [`Inputs::resolve`] under the caller's `par` (one that asks for each crate's names
    /// groups, `Par::names_groups`).
    pub fn resolve_with<P: blaze_scope::task::Par>(&'t self, crates: &[CrateInput<'t>], par: &P, bump: Bump<'t>) -> Result<Names<'t>, NamesError<'t>> {
        resolve_crates(crates, self.sysroot, &self.texts, par, bump)
    }

    /// The names with `roots` (crate indices) solved whole and every other crate's groups on
    /// demand (E13, blaze-scope cells/lazy_deps.rs).
    pub fn names_on_demand(&'t self, crates: &[CrateInput<'t>], roots: &[usize], bump: Bump<'t>) -> Result<blaze_scope::NamesOnDemand<'t>, blaze_scope::NamesOnDemandError<'t>> {
        blaze_scope::names_on_demand(crates, self.sysroot, &self.texts, roots, bump)
    }
}

/// Serial `Par` whose macro-expansion work runs in a reusable scratch `Frame` reset after each
/// call (`Par::call_scratch`), preventing bump-arena exhaustion when resolving the toolchain.
pub(crate) struct ScratchSerial {
    frame: RefCell<Option<Frame>>,
}

impl ScratchSerial {
    pub(crate) fn new() -> Self {
        Self { frame: RefCell::new(Frame::new(Tag::Expand).ok()) }
    }
}

impl Par for ScratchSerial {
    fn map<T, F: Fn(usize) -> T>(&self, n: usize, f: F, out: &mut AVec<'_, T>) {
        Serial.map(n, f, out);
    }

    fn call_scratch(&self, work: &mut dyn FnMut(Alloc<'_>)) -> Scratch {
        let Ok(mut slot) = self.frame.try_borrow_mut() else {
            return Scratch::Kept;
        };
        let Some(frame) = slot.as_mut() else {
            return Scratch::Kept;
        };
        frame.scope(|f| {
            let erased = Erased::new(f);
            work(erased.handle());
        });
        Scratch::Reset
    }
}
