//! The crate list a world is made over: blaze-scope's units list
//! ([`blaze_scope::units::unit_crates`]), the one home of which units and toolchain units are
//! crates, on which side, with their edges by index (AGENTS.md rule 30). This module was the
//! second list (`held`, a walk of its own over the graph, and every proc macro kept without
//! its dependencies); it is now the layer over the one list: where each crate's files are (the
//! trees), its key, its environment and the order. A build script's record is not in it: the
//! crate's task reads it (decision D115 (B), [`crate::joined`]).
//!
//! - **Which crates.** The list's: the target-side units the workspace members reach (a path
//!   package placed in the checkout is a member, as D96 already took it), each proc macro, the
//!   toolchain's crates they need, and, for a MEMBER's proc macro, its dependency closure again
//!   on the macro side (`wasm32-wasip1`'s cfg, rule 10; [`Side::Macro`]) with that target's
//!   `core`, `alloc`, `std` and `proc_macro`. Without a `wasm32-wasip1` cut on the shelf, that
//!   proc macro is refused BY NAME as its own crate ([`World::refused`] holding
//!   [`blaze_scope::units::UnitsError::NoMacroToolchain`]): the world is still made, and the refusal is carried to
//!   whoever asks for that crate's names, never dropped (rule 20) and never answered with the
//!   host's or the target's crates instead (rule 3).
//! - **Toolchain.** The target side's toolchain crates are the opened [`Toolchain`]'s (its keys,
//!   its cfg with std's build-script atoms), matched to the list's by their unit; the macro
//!   side's are the list's, their packs read from the same shelf ([`open_list`]). Known
//!   duplicate, stated: the target side's toolchain crates are still chosen twice (here by
//!   [`Toolchain::open`], in the list by blaze-scope's `toolchain_crates`); the list's are a
//!   subset and only matched. Merging them waits on std's build-script cfg moving into the list.
//! - **Keys.** A crate's key is from content, before any file is read (rules 21, 65): the
//!   list's key (package, kind, binary, side, features), its crate name, root, implicit externs,
//!   edition, cfg (the unit's), its dependencies' keys, a member's directory (D96) and a source
//!   correction (D58). Never a build script's outputs: they are not known before the script
//!   runs, so they are a read of the crate's task (decision D115 (B)).
//!
//! Sans-IO but for [`open_list`], which reads the toolchain's manifest and packs from the
//! caller's `Shelf`.

use blaze_fetch::PackView;
use blaze_id::{ContentId, CrateItemKey};
use blaze_manifest::lock::Lock;
use blaze_own::{AString, AVec, Alloc, BTreeMap, BVec, Bump};
use blaze_scope::crate_env::{CrateEnv, EnvFacts, SeenRoot};
use blaze_scope::graph::{CrateId, Sysroot};
use blaze_scope::resolve::CrateInput;
use blaze_scope::units::{Member, Origin as ListOrigin, RefusedCrate, UnitCrate, UnitCrates, unit_crates};
use blaze_toolchain::{Shelf, ToolchainId, ToolchainView};
use blaze_unit_graph::{StdIs, Unit, UnitGraph, UnitKind};

use crate::{
    Corrected, Crate, Needs, Origin, PackOrigin, Packages, Place, Script, Texts, Toolchain, Tree, World, WorldError, alloc_free_debug, correct_pack, implicit_key,
    join, push_u32,
};

#[cfg(test)]
#[path = "list_tests.rs"]
mod tests;

/// Which side a crate is built for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Side {
    /// The target's (the program's crates and the toolchain's for its triple).
    Target,
    /// A member proc macro's (D89 (a)): built for `wasm32-wasip1` and run in Wasmtime (rule 10).
    Macro,
}

/// The crate list of a graph and the toolchain crates it needs, opened from the shelf: what
/// [`world`] is made over.
pub struct Listed<'w, B> {
    /// The one crate list (blaze-scope's units list).
    pub list: UnitCrates<'w>,
    /// The target side's toolchain crates, their packs read and verified once.
    pub toolchain: Toolchain<'w, B>,
    /// Each macro-side toolchain crate's pack: (its index in [`Self::list`], its version, the
    /// pack's bytes when loaded; `None` under [`open_list_light`]).
    side_packs: AVec<'w, (usize, &'w str, Option<B>)>,
}

impl<'w, B: AsRef<[u8]>> Listed<'w, B> {
    /// The pack and version of the macro-side toolchain crate at `at` in the list.
    fn side_pack(&self, at: usize) -> Option<(&[u8], &'w str)> {
        self.side_packs.iter().find(|(i, _, _)| *i == at).and_then(|(_, version, bytes)| bytes.as_ref().map(|b| (b.as_ref(), *version)))
    }

    /// The version of the macro-side toolchain crate at `at` in the list (available even without packs).
    fn side_version(&self, at: usize) -> Option<&'w str> {
        self.side_packs.iter().find(|(i, _, _)| *i == at).map(|(_, version, _)| *version)
    }
}

/// Opens toolchain `id` on `shelf` for `triple`, makes `graph`'s crate list over it
/// ([`crate_list`]), then reads the packs the list needs: the target side's through
/// [`Toolchain::open`] (by the list's [`needs`]), the macro side's by each crate's row.
pub fn open_list<'w, S: Shelf, P: Packages<'w>>(
    shelf: &mut S,
    id: ToolchainId,
    triple: &'w str,
    graph: &'w UnitGraph<'w>,
    lock: &'w Lock<'w>,
    packages: &mut P,
    alloc: Bump<'w>,
) -> Result<Listed<'w, S::Bytes>, WorldError<'w>>
where
    S::Bytes: AsRef<[u8]>,
{
    open_list_inner(shelf, id, triple, graph, lock, packages, true, alloc)
}

/// Opens toolchain `id`'s manifest on `shelf` and makes `graph`'s crate list without reading
/// or hashing any toolchain pack from the shelf. Paired with [`world_from_list`] so incremental
/// `Names` runs do not stat, read, or BLAKE3-hash unused toolchain packs.
pub fn open_list_light<'w, S: Shelf, P: Packages<'w>>(
    shelf: &mut S,
    id: ToolchainId,
    triple: &'w str,
    graph: &'w UnitGraph<'w>,
    lock: &'w Lock<'w>,
    packages: &mut P,
    alloc: Bump<'w>,
) -> Result<Listed<'w, S::Bytes>, WorldError<'w>>
where
    S::Bytes: AsRef<[u8]>,
{
    open_list_inner(shelf, id, triple, graph, lock, packages, false, alloc)
}

#[expect(clippy::too_many_arguments, reason = "shared open_list implementation for eager and manifest-only modes")]
fn open_list_inner<'w, S: Shelf, P: Packages<'w>>(
    shelf: &mut S,
    id: ToolchainId,
    triple: &'w str,
    graph: &'w UnitGraph<'w>,
    lock: &'w Lock<'w>,
    packages: &mut P,
    load_packs: bool,
    alloc: Bump<'w>,
) -> Result<Listed<'w, S::Bytes>, WorldError<'w>>
where
    S::Bytes: AsRef<[u8]>,
{
    let bump = alloc;
    let alloc = bump.alloc();
    let manifest = blaze_toolchain::shelf::open_manifest(shelf, id).map_err(WorldError::Toolchain)?;
    // The manifest is small (rows, no packs): copied into `bump` so the list may borrow it.
    let bytes: &'w [u8] = bump.alloc_slice_copy(manifest.as_ref());
    let view = ToolchainView::open(bytes, id).expect("open_manifest checked it");
    let list = crate_list(graph, lock, view, triple, packages, alloc)?;
    let toolchain = if load_packs {
        Toolchain::open(shelf, id, triple, needs(&list), bump)?
    } else {
        Toolchain::open_manifest_only(shelf, id, triple, needs(&list), bump)?
    };
    let mut side_packs = AVec::new_in(alloc);
    for (at, c) in list.crates.iter().enumerate().skip(list.macro_from) {
        if let ListOrigin::Toolchain { krate, .. } = c.origin {
            let row = view.krate(krate);
            let pack = if load_packs {
                Some(blaze_toolchain::shelf::open_pack(shelf, id, &row).map_err(WorldError::Toolchain)?)
            } else {
                None
            };
            side_packs.push((at, row.version, pack));
        }
    }
    Ok(Listed { list, toolchain, side_packs })
}

/// `graph`'s crate list: blaze-scope's units list, with every path package the graph reaches
/// that `packages` places in the checkout as a member at its directory (the root it sees,
/// D96 (3)); a package placed as a pack is a dependency.
pub fn crate_list<'w, P: Packages<'w>>(
    graph: &'w UnitGraph<'w>,
    lock: &'w Lock<'w>,
    view: ToolchainView<'w>,
    triple: &'w str,
    packages: &mut P,
    alloc: Alloc<'w>,
) -> Result<UnitCrates<'w>, WorldError<'w>> {
    let mut members: AVec<'w, Member<'w>> = AVec::new_in(alloc);
    let mut asked: AVec<'w, u32> = AVec::new_in(alloc);
    for unit in graph.units.iter() {
        if !lock.package(unit.package).source_text.is_empty() || asked.scan_for(&unit.package.0).is_some() {
            continue;
        }
        asked.push(unit.package.0);
        match packages.place(unit.package)? {
            Place::Checkout { dir } => members.push(Member::new(unit.package, dir).map_err(|error| WorldError::MemberDir { package: &unit.name, error })?),
            Place::Path { .. } | Place::Pack { .. } => {}
        }
    }
    unit_crates(graph, lock, &members, view, triple, alloc).map_err(WorldError::Units)
}

/// The roles the list's own crates need of the target's toolchain beyond core and alloc: Rust's
/// std for a crate given it, bstd for a blaze-edition crate, and `proc_macro` for a proc macro
/// (a dependency's names the target's). The toolchain crates' needs are their rows' edges
/// (blaze-scope units.rs `toolchain_crates` seeds the same: `rust_std`, `blaze`).
pub fn needs(list: &UnitCrates<'_>) -> Needs {
    let mut n = Needs { std: false, bstd: false, proc_macro: false };
    for c in list.crates.iter().filter(|c| matches!(c.origin, ListOrigin::Unit(_))) {
        n.std |= matches!(c.implicit.std, StdIs::Rust);
        n.bstd |= matches!(c.implicit.std, StdIs::Bstd(_));
        n.proc_macro |= c.implicit.proc_macro;
    }
    n
}

/// Two units placed under one crate key, `first` and `second` (each its crate name and
/// [`Script`]), read one build-script run, or the world is refused naming both
/// ([`WorldError::ScriptClash`]): one crate key, two runs would be two values under one key
/// (rule 21).
pub(crate) fn one_script<'w>(first: (&'w str, Script), second: (&'w str, Script)) -> Result<(), WorldError<'w>> {
    if first.1 == second.1 {
        return Ok(());
    }
    Err(WorldError::ScriptClash { first: first.0, second: second.0 })
}

/// One crate of the list past the target's toolchain crates, keyed, before its place is known.
struct Pending<'w> {
    /// Its index in the list (the first with this key).
    at: usize,
    key: CrateItemKey,
    cfg: &'w [(&'w str, Option<&'w str>)],
    script: Script,
    side: Side,
    /// How many units of the graph are this crate (0 for a macro-side toolchain crate).
    units: u32,
}

/// Keyed crate order produced from `listed`, shared by [`world`] and [`world_from_list`].
struct Keyed<'w> {
    opened: AVec<'w, u32>,
    crate_of: AVec<'w, Option<u32>>,
    pending: AVec<'w, Pending<'w>>,
    order: AVec<'w, u32>,
    rank: AVec<'w, u32>,
    early: BTreeMap<u32, Place<'w>, Alloc<'w>>,
    corrections: blaze_fetch::correct::Table<Alloc<'w>>,
}

fn key_list<'w, P: Packages<'w>, B: AsRef<[u8]>>(
    graph: &'w UnitGraph<'w>,
    lock: &'w Lock<'w>,
    listed: &'w Listed<'w, B>,
    packages: &mut P,
    bump: Bump<'w>,
) -> Result<Keyed<'w>, WorldError<'w>> {
    let alloc = bump.alloc();
    let list = &listed.list;
    let toolchain = &listed.toolchain;
    let units = &graph.units;
    let corrections = blaze_fetch::correct::Table::checked_in(alloc);
    let tc = toolchain.crates();
    let n = list.crates.len();
    let first = list.toolchain;

    let mut opened: AVec<'w, u32> = AVec::with_capacity_in(first, alloc);
    for c in list.crates[..first].iter() {
        let not_opened = WorldError::ToolchainCrateNotOpened { krate: c.name };
        let ListOrigin::Toolchain { unit, .. } = c.origin else { return Err(not_opened) };
        let at = tc.iter().position(|o| o.unit == unit).ok_or(not_opened)?;
        opened.push(at as u32);
    }

    let mut waiting: AVec<'w, u32> = AVec::with_capacity_in(n, alloc);
    let mut dependents: AVec<'w, AVec<'w, u32>> = AVec::with_capacity_in(n, alloc);
    for _ in 0..n {
        dependents.push(AVec::new_in(alloc));
    }
    for (at, c) in list.crates.iter().enumerate() {
        let mut count = 0;
        if at >= first {
            for &(_, d) in c.deps.iter() {
                if d >= first {
                    dependents[d].push(at as u32);
                    count += 1;
                }
            }
        }
        waiting.push(count);
    }
    let mut ready: AVec<'w, u32> = AVec::new_in(alloc);
    ready.extend((first as u32..n as u32).filter(|at| waiting[*at as usize] == 0));
    ready.reverse();

    let mut key_of: AVec<'w, Option<CrateItemKey>> = AVec::with_capacity_in(n, alloc);
    key_of.extend((0..n).map(|at| if at < first { Some(tc[opened[at] as usize].key) } else { None }));
    let mut crate_of: AVec<'w, Option<u32>> = AVec::with_capacity_in(n, alloc);
    crate_of.extend((0..n).map(|_| None));
    let mut pending: AVec<'w, Pending<'w>> = AVec::new_in(alloc);
    let mut by_key: BTreeMap<[u8; 32], u32, Alloc<'w>> = BTreeMap::new_in(alloc);
    let mut early: BTreeMap<u32, Place<'w>, Alloc<'w>> = BTreeMap::new_in(alloc);
    let mut keyed = 0usize;
    while let Some(at) = ready.pop() {
        let at = at as usize;
        let c = &list.crates[at];
        let side = if list.macro_side(at) { Side::Macro } else { Side::Target };
        let mut deps: AVec<'w, (&'w str, CrateItemKey)> = AVec::with_capacity_in(c.deps.len(), alloc);
        for &(name, d) in c.deps.iter() {
            deps.push((name, key_of[d].ok_or(WorldError::Cycle { krate: c.name })?));
        }
        let is_unit = matches!(c.origin, ListOrigin::Unit(_));
        let (key, cfg, script) = match c.origin {
            ListOrigin::Unit(id) => {
                let unit = &units[id.index()];
                let script = match unit.build_script {
                    Some(_) => Script::Read { subject: crate::script_subject(unit.name.as_str(), unit.version.as_str(), side, &c.features, alloc) },
                    None => Script::None,
                };
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
                        Place::Checkout { dir } | Place::Path { dir } => Some(dir),
                        Place::Pack { .. } => None,
                    }
                } else {
                    None
                };
                let mut cfg: BVec<'w, (&'w str, Option<&'w str>)> = BVec::with_capacity_in(c.cfg.len(), bump);
                cfg.extend_from_slice(&c.cfg);
                cfg.sort_unstable();
                cfg.dedup();
                let implicit_extern = implicit_key(toolchain, c.implicit)?;
                let parts = UnitKeyParts { cfg: &cfg, deps: &deps, implicit_extern, member };
                let key = unit_crate_key(c, unit, lock, &parts, &corrections, alloc);
                let cfg: &'w [(&'w str, Option<&'w str>)] = cfg.into_bump_slice();
                (key, cfg, script)
            }
            ListOrigin::Toolchain { .. } => (side_toolchain_key(c, &deps, alloc), &c.cfg[..], Script::None),
        };
        key_of[at] = Some(key);
        keyed += 1;
        match by_key.get(&key.0.0) {
            Some(&p) => {
                let first = &pending[p as usize];
                one_script((list.crates[first.at].name, first.script), (c.name, script))?;
                crate_of[at] = Some(p);
                pending[p as usize].units += u32::from(is_unit);
            }
            None => {
                let p = pending.len() as u32;
                by_key.insert(key.0.0, p);
                crate_of[at] = Some(p);
                pending.push(Pending { at, key, cfg, script, side, units: u32::from(is_unit) });
            }
        }
        for &d in dependents[at].iter() {
            waiting[d as usize] -= 1;
            if waiting[d as usize] == 0 {
                ready.push(d);
            }
        }
    }
    if keyed != n - first {
        let stuck = (first..n).find(|at| key_of[*at].is_none()).unwrap_or(first);
        return Err(WorldError::Cycle { krate: list.crates[stuck].name });
    }

    let mut order: AVec<'w, u32> = AVec::with_capacity_in(pending.len(), alloc);
    order.extend(0..pending.len() as u32);
    order.sort_unstable_by(|a, b| pending[*a as usize].key.0.0.cmp(&pending[*b as usize].key.0.0));
    let mut rank: AVec<'w, u32> = AVec::with_capacity_in(pending.len(), alloc);
    rank.extend((0..pending.len()).map(|_| 0));
    for (at, p) in order.iter().enumerate() {
        rank[*p as usize] = at as u32;
    }
    Ok(Keyed { opened, crate_of, pending, order, rank, early, corrections })
}

/// The names library's inputs for every crate of `listed`'s list: see the module's
/// documentation.
pub fn world<'w, P: Packages<'w>, B: AsRef<[u8]>>(
    graph: &'w UnitGraph<'w>,
    lock: &'w Lock<'w>,
    listed: &'w Listed<'w, B>,
    packages: &mut P,
    checkout: &'w dyn Tree,
    alloc: Bump<'w>,
) -> Result<World<'w>, WorldError<'w>> {
    world_inner(graph, lock, listed, None, packages, checkout, alloc)
}

/// Builds a [`World`] from `listed` (opened via [`open_list_light`] with zero pack opens) and an
/// existing standing [`crate::crate_list::CrateList`]: computes every crate's key, order, cfg,
/// deps, script, environment and sysroot identically to [`world`], taking non-member tree paths
/// and source corrections from `stored_list` without stat-ing, mapping, or hashing any fetched or
/// toolchain pack. `World::texts.packs()` is left empty; callers open packs lazily on demand.
#[expect(clippy::too_many_arguments, reason = "mirrors world() plus the standing CrateList for lazy pack resolution")]
pub fn world_from_list<'w, P: Packages<'w>, B: AsRef<[u8]>>(
    graph: &'w UnitGraph<'w>,
    lock: &'w Lock<'w>,
    listed: &'w Listed<'w, B>,
    stored_list: &crate::crate_list::CrateList<'w>,
    packages: &mut P,
    checkout: &'w dyn Tree,
    alloc: Bump<'w>,
) -> Result<World<'w>, WorldError<'w>> {
    world_inner(graph, lock, listed, Some(stored_list), packages, checkout, alloc)
}

#[expect(clippy::too_many_arguments, reason = "shared world builder for eager and lazy standing-list modes")]
fn world_inner<'w, P: Packages<'w>, B: AsRef<[u8]>>(
    graph: &'w UnitGraph<'w>,
    lock: &'w Lock<'w>,
    listed: &'w Listed<'w, B>,
    stored_list: Option<&crate::crate_list::CrateList<'w>>,
    packages: &mut P,
    checkout: &'w dyn Tree,
    alloc: Bump<'w>,
) -> Result<World<'w>, WorldError<'w>> {
    let bump = alloc;
    let alloc = bump.alloc();
    let list = &listed.list;
    let toolchain = &listed.toolchain;
    let units = &graph.units;
    let Keyed { opened, crate_of, pending, order, rank, early, corrections } = key_list(graph, lock, listed, packages, bump)?;
    let mut corrected: AVec<'w, (u32, &'w str, &'w str)> = AVec::new_in(alloc);
    let mut used: AVec<'w, Corrected<'w>> = AVec::new_in(alloc);
    let tc = toolchain.crates();
    let t = u32::try_from(tc.len()).unwrap_or(u32::MAX);
    let proc_macro_crate = toolchain.proc_macro().map(|i| i as u32);
    let first = list.toolchain;
    // A crate of the list (past the target's toolchain) to its index in the world.
    let world_at = |d: usize| -> Option<u32> { if d < first { Some(opened[d]) } else { crate_of[d].map(|p| t + rank[p as usize]) } };

    // The trees: the toolchain's packs (1..=t), then each fetched package's record and each
    // macro-side toolchain pack in the order its first crate is placed. Under `stored_list`
    // (lazy mode), packs are left empty and opened on demand by callers.
    let mut packs: AVec<'w, PackView<'w>> = AVec::with_capacity_in(if stored_list.is_some() { 0 } else { tc.len() }, alloc);
    let mut origins: AVec<'w, PackOrigin> = AVec::with_capacity_in(if stored_list.is_some() { 0 } else { tc.len() }, alloc);
    if let Some(stored) = stored_list {
        corrected.extend(stored.corrected());
    } else {
        for c in tc {
            let bytes = toolchain.pack(c);
            packs.push(PackView::open_verified(bytes).map_err(|_| WorldError::Pack { krate: c.name })?);
            origins.push(PackOrigin::Shelf { digest: footer(bytes).ok_or(WorldError::Pack { krate: c.name })? });
        }
    }
    // The lock's fetch plan, made at the first fetched package placed: which record holds it.
    let mut fetch_plan: Option<blaze_fetch::Plan<'w, 'w>> = None;
    let mut placed: BTreeMap<u32, (u32, &'w str, bool), Alloc<'w>> = BTreeMap::new_in(alloc);

    let mut crates: AVec<'w, Crate<'w>> = AVec::with_capacity_in(tc.len() + pending.len(), alloc);
    let mut envs: AVec<'w, CrateEnv<'w>> = AVec::with_capacity_in(tc.len() + pending.len(), alloc);
    for c in tc.iter() {
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
            bin_exes: blaze_scope::crate_env::BinNames::NONE,
            meta: None,
        };
        envs.push(CrateEnv::of(facts, alloc).map_err(WorldError::Env)?);
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
            member: false,
            package: c.name,
            version: c.version,
            side: Side::Target,
        });
    }
    for &p in order.iter() {
        let pend = &pending[p as usize];
        let c = &list.crates[pend.at];
        let (root, shown_from, facts, origin, proc_macro, is_member, package, version) = match c.origin {
            ListOrigin::Unit(id) => {
                let unit = &units[id.index()];
                let (root, shown_from, seen, is_member): (&'w str, &'w str, SeenRoot<'w>, bool) = if let Some(stored) = stored_list {
                    match early.get(&unit.package.0) {
                        Some(Place::Checkout { dir }) => {
                            let shown = join("/0/", dir, alloc);
                            let root: &'w str = bump.alloc_str(&join(&shown, &unit.root, alloc));
                            let mut shown_from = shown;
                            if !shown_from.ends_with('/') {
                                shown_from.push_str("/");
                            }
                            let seen = SeenRoot::member(dir).map_err(|error| WorldError::MemberDir { package: &unit.name, error })?;
                            (root, bump.alloc_str(&shown_from), seen, true)
                        }
                        Some(Place::Path { dir }) => {
                            let shown = join("/0/", dir, alloc);
                            let root: &'w str = bump.alloc_str(&join(&shown, &unit.root, alloc));
                            let mut shown_from = shown;
                            if !shown_from.ends_with('/') {
                                shown_from.push_str("/");
                            }
                            (root, bump.alloc_str(&shown_from), SeenRoot::Package, false)
                        }
                        _ => match stored.find(pend.key) {
                            Some(row) => (stored.root(row), stored.shown_from(row), SeenRoot::Package, false),
                            None => ("", "", SeenRoot::Package, false),
                        },
                    }
                } else {
                    let (tree, dir, is_member) = match placed.get(&unit.package.0) {
                        Some(&it) => it,
                        None => {
                            let place = match early.get(&unit.package.0) {
                                Some(&place) => place,
                                None => packages.place(unit.package)?,
                            };
                            let it = match place {
                                Place::Checkout { dir } => (0, dir, true),
                                Place::Path { dir } => (0, dir, false),
                                Place::Pack { view, dir } => {
                                    let key = source_key(&mut fetch_plan, view, lock, unit.package, &unit.name, alloc)?;
                                    packs.push(view);
                                    origins.push(PackOrigin::Source { key });
                                    let tree = packs.len() as u32;
                                    if let Some(row) = corrections.find_text(&unit.name, &unit.version) {
                                        correct_pack(row, view, tree, dir, unit, &mut corrected, &mut used, bump)?;
                                    }
                                    (tree, dir, false)
                                }
                            };
                            placed.insert(unit.package.0, it);
                            it
                        }
                    };
                    let mut base = AString::with_capacity_in(dir.len() + 12, alloc);
                    base.push_str("/");
                    push_u32(&mut base, tree);
                    base.push_str("/");
                    let shown = join(&base, dir, alloc);
                    let root: &'w str = bump.alloc_str(&join(&shown, &unit.root, alloc));
                    let mut shown_from = shown;
                    if !shown_from.ends_with('/') {
                        shown_from.push_str("/");
                    }
                    let seen = if is_member { SeenRoot::member(dir).map_err(|error| WorldError::MemberDir { package: &unit.name, error })? } else { SeenRoot::Package };
                    (root, bump.alloc_str(&shown_from), seen, is_member)
                };
                let test_or_bench = matches!(unit.kind, UnitKind::IntegrationTest(_) | UnitKind::Bench(_));
                let bin_exes = match test_or_bench {
                    true => blaze_scope::crate_env::package_bins(graph, unit.package, unit.crate_name.as_str(), bump).map_err(WorldError::Env)?,
                    false => blaze_scope::crate_env::BinNames::NONE,
                };
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
                    script: None,
                    test_or_bench,
                    bin_exes,
                    meta: graph.meta_of(unit.package).map(|meta| meta.vars()),
                };
                (root, shown_from, facts, Origin::Unit(id), unit.kind == UnitKind::ProcMacro, is_member, unit.name.as_str(), unit.version.as_str())
            }
            ListOrigin::Toolchain { unit, .. } => {
                let (root, shown_from, version): (&'w str, &'w str, &'w str) = if let Some(stored) = stored_list {
                    let version = listed.side_version(pend.at).ok_or(WorldError::ToolchainCrateNotOpened { krate: c.name })?;
                    let (root, shown_from) = match stored.find(pend.key) {
                        Some(row) => (stored.root(row), stored.shown_from(row)),
                        None => ("", ""),
                    };
                    (root, shown_from, version)
                } else {
                    // A macro-side toolchain crate: a tree of its own, its pack read by `open_list`.
                    let (bytes, version) = listed.side_pack(pend.at).ok_or(WorldError::ToolchainCrateNotOpened { krate: c.name })?;
                    packs.push(PackView::open_verified(bytes).map_err(|_| WorldError::Pack { krate: c.name })?);
                    origins.push(PackOrigin::Shelf { digest: footer(bytes).ok_or(WorldError::Pack { krate: c.name })? });
                    let tree = packs.len() as u32;
                    let inner = c.root.as_str().strip_prefix(c.shown_from.as_str()).ok_or(WorldError::ToolchainRoot { krate: c.name, root: c.root.as_str() })?;
                    let mut base = AString::with_capacity_in(12, alloc);
                    base.push_str("/");
                    push_u32(&mut base, tree);
                    base.push_str("/");
                    let root: &'w str = bump.alloc_str(&join(&base, inner, alloc));
                    let shown_from: &'w str = bump.alloc_str(&base);
                    (root, shown_from, version)
                };
                let facts = EnvFacts {
                    crate_name: c.name,
                    package: None,
                    bin: None,
                    seen: SeenRoot::Package,
                    read_folder: shown_from,
                    build_script: false,
                    script: None,
                    test_or_bench: false,
                    bin_exes: blaze_scope::crate_env::BinNames::NONE,
                    meta: None,
                };
                (root, shown_from, facts, Origin::Toolchain(unit), false, false, c.name, version)
            }
        };
        let mut deps: BVec<'w, (&'w str, usize)> = BVec::with_capacity_in(c.deps.len() + 1, bump);
        for &(name, d) in c.deps.iter() {
            deps.push((name, world_at(d).ok_or(WorldError::Cycle { krate: c.name })? as usize));
        }
        // A proc macro the list left without `proc_macro` (a dependency's, whose macros come
        // from the module store) names the target toolchain's, as it did. A member's proc macro
        // has the macro side's, or, refused for want of that side, none: another side's crates
        // are never taken in its place (rule 3).
        if proc_macro && !deps.iter().any(|(name, _)| *name == "proc_macro") && list.refusal(pend.at).is_none() {
            let pm = proc_macro_crate.ok_or(WorldError::NoToolchainCrate { role: "proc_macro" })?;
            deps.push(("proc_macro", pm as usize));
        }
        envs.push(CrateEnv::of(facts, alloc).map_err(WorldError::Env)?);
        crates.push(Crate {
            input: CrateInput {
                name: c.name,
                key: pend.key,
                edition: c.edition,
                implicit: c.implicit,
                root,
                shown_from,
                features: &c.features,
                deps: deps.into_bump_slice(),
                cfg: pend.cfg,
                script: None,
            },
            origin,
            units: pend.units,
            script: pend.script,
            proc_macro,
            member: is_member,
            package,
            version,
            side: pend.side,
        });
    }
    // Per unit of the graph, its target-side crate (a macro-side copy is not this map's).
    let mut crate_of_unit: AVec<'w, Option<u32>> = AVec::with_capacity_in(units.len(), alloc);
    crate_of_unit.extend((0..units.len()).map(|_| None));
    for at in first..list.macro_from {
        if let ListOrigin::Unit(id) = list.crates[at].origin {
            crate_of_unit[id.index()] = world_at(at);
        }
    }
    // Each crate the list refused, at its index in the world, with its typed refusal.
    let mut refused: AVec<'w, RefusedCrate<'w>> = AVec::with_capacity_in(list.refused.len(), alloc);
    for r in list.refused.iter() {
        let krate = world_at(r.krate).ok_or(WorldError::Cycle { krate: list.crates[r.krate].name })?;
        refused.push(RefusedCrate { krate: krate as usize, why: r.why.clone() });
    }
    refused.sort_unstable_by_key(|r| r.krate);
    let role = |i: Option<usize>| i.map(|i| CrateId(i as u32));
    let sysroot = Sysroot { core: role(toolchain.core()), alloc: role(toolchain.alloc()), std: role(toolchain.std()), bstd: role(toolchain.bstd()) };
    Ok(World { crates, texts: Texts { checkout, packs, origins, corrected, envs }, sysroot, toolchain_crates: t, crate_of_unit, corrections: used, refused })
}

/// Lightweight world for build-script execution: holds only the toolchain crates (target and
/// macro sides, for `script_setting::ScriptSetup::open_recorded`) plus each script-reading
/// crate's `(script_subject, side, cfg)`, without opening or placing any fetched package.
pub struct ScriptWorld<'w> {
    pub world: World<'w>,
    pub script_cfgs: AVec<'w, (ContentId, Side, &'w [(&'w str, Option<&'w str>)])>,
}

/// Builds a [`ScriptWorld`] from `listed` (already opened without touching any fetched package).
pub fn script_world<'w, B: AsRef<[u8]>>(
    graph: &'w UnitGraph<'w>,
    listed: &'w Listed<'w, B>,
    checkout: &'w dyn Tree,
    alloc: Bump<'w>,
) -> Result<ScriptWorld<'w>, WorldError<'w>> {
    let bump = alloc;
    let alloc = bump.alloc();
    let list = &listed.list;
    let toolchain = &listed.toolchain;
    let units = &graph.units;
    let tc = toolchain.crates();
    let t = u32::try_from(tc.len()).unwrap_or(u32::MAX);

    let mut packs: AVec<'w, PackView<'w>> = AVec::with_capacity_in(tc.len() + listed.side_packs.len(), alloc);
    let mut origins: AVec<'w, PackOrigin> = AVec::with_capacity_in(tc.len() + listed.side_packs.len(), alloc);
    for c in tc {
        let bytes = toolchain.pack(c);
        packs.push(PackView::open_verified(bytes).map_err(|_| WorldError::Pack { krate: c.name })?);
        origins.push(PackOrigin::Shelf { digest: footer(bytes).ok_or(WorldError::Pack { krate: c.name })? });
    }

    let mut crates: AVec<'w, Crate<'w>> = AVec::with_capacity_in(tc.len() + listed.side_packs.len(), alloc);
    let mut envs: AVec<'w, CrateEnv<'w>> = AVec::with_capacity_in(tc.len() + listed.side_packs.len(), alloc);
    for c in tc.iter() {
        let facts = EnvFacts {
            crate_name: c.name,
            package: None,
            bin: None,
            seen: SeenRoot::Package,
            read_folder: c.shown_from,
            build_script: false,
            script: None,
            test_or_bench: false,
            bin_exes: blaze_scope::crate_env::BinNames::NONE,
            meta: None,
        };
        envs.push(CrateEnv::of(facts, alloc).map_err(WorldError::Env)?);
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
            member: false,
            package: c.name,
            version: c.version,
            side: Side::Target,
        });
    }

    // Macro-side toolchain crates: assign their world indices first so deps remap cleanly.
    let mut side_index: AVec<'w, Option<usize>> = AVec::with_capacity_in(list.crates.len(), alloc);
    side_index.resize(list.crates.len(), None);
    let mut next_idx = tc.len();
    for (at, c) in list.crates.iter().enumerate().skip(list.macro_from) {
        if matches!(c.origin, ListOrigin::Toolchain { .. })
            && let Some(slot) = side_index.get_mut(at)
        {
            *slot = Some(next_idx);
            next_idx += 1;
        }
    }
    for (at, c) in list.crates.iter().enumerate().skip(list.macro_from) {
        let ListOrigin::Toolchain { unit, .. } = c.origin else { continue };
        let (bytes, version) = listed.side_pack(at).ok_or(WorldError::ToolchainCrateNotOpened { krate: c.name })?;
        packs.push(PackView::open_verified(bytes).map_err(|_| WorldError::Pack { krate: c.name })?);
        origins.push(PackOrigin::Shelf { digest: footer(bytes).ok_or(WorldError::Pack { krate: c.name })? });
        let tree = u32::try_from(packs.len()).unwrap_or(u32::MAX);
        let inner = c.root.as_str().strip_prefix(c.shown_from.as_str()).ok_or(WorldError::ToolchainRoot { krate: c.name, root: c.root.as_str() })?;
        let mut base = AString::with_capacity_in(12, alloc);
        base.push_str("/");
        push_u32(&mut base, tree);
        base.push_str("/");
        let root = bump.alloc_str(&join(&base, inner, alloc));
        let shown_from: &'w str = bump.alloc_str(&base);
        let facts = EnvFacts {
            crate_name: c.name,
            package: None,
            bin: None,
            seen: SeenRoot::Package,
            read_folder: shown_from,
            build_script: false,
            script: None,
            test_or_bench: false,
            bin_exes: blaze_scope::crate_env::BinNames::NONE,
            meta: None,
        };
        let mut deps: BVec<'w, (&'w str, usize)> = BVec::with_capacity_in(c.deps.len(), bump);
        for &(name, d) in c.deps.iter() {
            if let Some(target_idx) = side_index.get(d).copied().flatten() {
                deps.push((name, target_idx));
            }
        }
        envs.push(CrateEnv::of(facts, alloc).map_err(WorldError::Env)?);
        crates.push(Crate {
            input: CrateInput {
                name: c.name,
                key: c.key,
                edition: c.edition,
                implicit: c.implicit,
                root,
                shown_from,
                features: &c.features,
                deps: deps.into_bump_slice(),
                cfg: &c.cfg,
                script: None,
            },
            origin: Origin::Toolchain(unit),
            units: 0,
            script: Script::None,
            proc_macro: false,
            member: false,
            package: c.name,
            version,
            side: Side::Macro,
        });
    }

    let mut script_cfgs: AVec<'w, (ContentId, Side, &'w [(&'w str, Option<&'w str>)])> = AVec::new_in(alloc);
    for (at, c) in list.crates.iter().enumerate().skip(list.toolchain) {
        let ListOrigin::Unit(id) = c.origin else { continue };
        let Some(unit) = units.get(id.index()) else { continue };
        if unit.build_script.is_none() {
            continue;
        }
        let side = if list.macro_side(at) { Side::Macro } else { Side::Target };
        let subject = crate::script_subject(unit.name.as_str(), unit.version.as_str(), side, &c.features, alloc);
        if script_cfgs.iter().any(|(s, sd, _)| *s == subject && *sd == side) {
            continue;
        }
        let mut cfg: BVec<'w, (&'w str, Option<&'w str>)> = BVec::with_capacity_in(c.cfg.len(), bump);
        cfg.extend_from_slice(&c.cfg);
        cfg.sort_unstable();
        cfg.dedup();
        script_cfgs.push((subject, side, cfg.into_bump_slice()));
    }

    let role = |i: Option<usize>| i.and_then(|i| u32::try_from(i).ok()).map(CrateId);
    let sysroot = Sysroot { core: role(toolchain.core()), alloc: role(toolchain.alloc()), std: role(toolchain.std()), bstd: role(toolchain.bstd()) };
    let world = World {
        crates,
        texts: Texts { checkout, packs, origins, corrected: AVec::new_in(alloc), envs },
        sysroot,
        toolchain_crates: t,
        crate_of_unit: AVec::new_in(alloc),
        corrections: AVec::new_in(alloc),
        refused: AVec::new_in(alloc),
    };
    Ok(ScriptWorld { world, script_cfgs })
}

/// The BLAKE3 a toolchain pack is named by on the shelf: its 32-byte footer, which
/// `blaze_toolchain::shelf::open_pack` checked is its bytes' digest and its manifest row's name.
fn footer(pack: &[u8]) -> Option<[u8; 32]> {
    let at = pack.len().checked_sub(32)?;
    pack[at..].try_into().ok()
}

/// The key of the sources store's record a fetched package's files were placed from (`view`,
/// for locked package `package`, named `name`): the lock's fetch plan's unit of it, the one home
/// of which record holds a package (`blaze_fetch::plan`: a crate's own record, a git package's
/// commit's), whose key is `SourceId::key`. The view is that record (`StoreSources::files`
/// opened it under the same plan and checked its source), so a crate's unit is found by its
/// package and a git package's by the view's commit (a git record's source is its commit and
/// 12 zero bytes). The plan is made once, at the first ask. The key is a name, not a proof:
/// the opener checks the record it finds holds these files (`TreeRow::files`).
fn source_key<'w>(
    fetch_plan: &mut Option<blaze_fetch::Plan<'w, 'w>>,
    view: PackView<'w>,
    lock: &'w Lock<'w>,
    package: blaze_manifest::lock::LockId,
    name: &'w str,
    alloc: Alloc<'w>,
) -> Result<ContentId, WorldError<'w>> {
    if fetch_plan.is_none() {
        *fetch_plan = Some(blaze_fetch::plan::plan(lock, alloc).map_err(|_| WorldError::SourceRecord { package: name })?);
    }
    let plan = fetch_plan.as_ref().ok_or(WorldError::SourceRecord { package: name })?;
    let own = plan.units.iter().find(|u| matches!(u, blaze_fetch::Unit::Crate { package: p, .. } if *p == package));
    let unit = own.or_else(|| {
        let commit = view.source().get(..20)?;
        plan.units.iter().find(|u| matches!(u, blaze_fetch::Unit::Git { commit: c, .. } if c.0[..] == *commit))
    });
    unit.map(|u| u.key()).ok_or(WorldError::SourceRecord { package: name })
}

/// The parts of a unit crate's key the world adds to the list's.
struct UnitKeyParts<'k, 'w> {
    /// Its cfg (the unit's; never a build script's atoms, D115 (B)), sorted and each once.
    cfg: &'k [(&'w str, Option<&'w str>)],
    /// Each dependency's extern name and key.
    deps: &'k [(&'w str, CrateItemKey)],
    /// [`implicit_key`] of its implicit externs.
    implicit_extern: CrateItemKey,
    /// A member's directory in the checkout (the root it sees, D96).
    member: Option<&'w str>,
}

/// Length-prefixed bytes, so no two inputs write the same stream.
fn put(bytes: &mut AVec<'_, u8>, s: &[u8]) {
    bytes.extend_from_slice(&(s.len() as u32).to_le_bytes());
    bytes.extend_from_slice(s);
}

/// A unit crate's key: its IDENTITY, never its text (its item ids are made from it, so a body
/// or `pub fn` edit leaves it equal). The list's key (package as the lock writes it, kind,
/// binary, side, features), then what the world adds.
fn unit_crate_key<'w>(
    c: &UnitCrate<'w>,
    unit: &Unit<'w>,
    lock: &Lock<'w>,
    parts: &UnitKeyParts<'_, 'w>,
    corrections: &blaze_fetch::correct::Table<Alloc<'w>>,
    alloc: Alloc<'w>,
) -> CrateItemKey {
    let mut bytes: AVec<'_, u8> = AVec::new_in(alloc);
    put(&mut bytes, &c.key.0.0);
    put(&mut bytes, unit.crate_name.as_bytes());
    put(&mut bytes, unit.root.as_bytes());
    put(&mut bytes, alloc_free_debug(&c.implicit).as_bytes());
    put(&mut bytes, &[c.edition as u8]);
    put(&mut bytes, b"\0cfg");
    for (name, value) in parts.cfg.iter() {
        put(&mut bytes, name.as_bytes());
        put(&mut bytes, value.unwrap_or("\u{0}none").as_bytes());
    }
    put(&mut bytes, b"\0deps");
    for (name, key) in parts.deps.iter() {
        put(&mut bytes, name.as_bytes());
        put(&mut bytes, &key.0.0);
    }
    put(&mut bytes, b"\0implicit");
    put(&mut bytes, &parts.implicit_extern.0.0);
    // A member's directory: the root it sees is `/workspace/<dir>` (D96 (3), rule 21).
    if let Some(dir) = parts.member {
        put(&mut bytes, b"\0member");
        put(&mut bytes, dir.as_bytes());
    }
    // A package read corrected (D58) is keyed by the row too.
    if let Some(row) = corrections.find_text(&lock.package(unit.package).name, &unit.version) {
        put(&mut bytes, b"\0corrected");
        put(&mut bytes, &row.digest());
    }
    // A registry crate's archive checksum, as the lock writes it (rule 21: a key covers every
    // input): name, version and registry do not say WHICH bytes; an archive replaced under one
    // version must not find the old names record (lane BD4, found by its archive control).
    let package = lock.package(unit.package);
    if let (blaze_manifest::lock::LockSource::Registry { .. }, blaze_manifest::lock::Checksum::Sha256(sum)) = (&package.source, &package.checksum) {
        put(&mut bytes, b"\0registry archive sha256");
        put(&mut bytes, sum.as_bytes());
    }
    CrateItemKey(ContentId::of("blaze world crate 3", &bytes))
}

/// A macro-side toolchain crate's key: the list's (its pack and features under the side's
/// domain), its name, cfg (the side's target atoms) and its dependencies' keys.
fn side_toolchain_key<'w>(c: &UnitCrate<'w>, deps: &[(&'w str, CrateItemKey)], alloc: Alloc<'w>) -> CrateItemKey {
    let mut bytes: AVec<'_, u8> = AVec::new_in(alloc);
    put(&mut bytes, &c.key.0.0);
    put(&mut bytes, c.name.as_bytes());
    put(&mut bytes, b"\0cfg");
    for (name, value) in c.cfg.iter() {
        put(&mut bytes, name.as_bytes());
        put(&mut bytes, value.unwrap_or("\u{0}none").as_bytes());
    }
    put(&mut bytes, b"\0deps");
    for (name, key) in deps.iter() {
        put(&mut bytes, name.as_bytes());
        put(&mut bytes, &key.0.0);
    }
    CrateItemKey(ContentId::of("blaze world macro-side toolchain crate 1", &bytes))
}
