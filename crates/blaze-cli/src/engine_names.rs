//! The `Names` stage of `blaze engine-check` (engine.md E6, second step): one `CrateScope` task a
//! crate of the names world (members, dependencies, the toolchain shelf's crates), in the phase
//! `Phase { stage: Names, wave: the crate's depth }`, so a crate is entered after every crate it
//! reads was merged and put to the store.
//!
//! The coordinator lends mapped dependency exports and the script output to each job. A
//! worker prepares the local crate indices and environment, resolves the crate, and returns
//! its record and engine cells. The coordinator publishes the results in engine task order.
//!
//! A crate's macro expansion rounds stay inside its task for now. A crate whose package has a
//! build script is handed its run's record after that producer settles (engine_scripts.rs); a
//! failed run fails the crate `Waited` on it, as the old path's do.

use core::convert::Infallible;
use std::sync::OnceLock;

use blaze_drive::cells::Cell;
use blaze_drive::{Input, Key, Kind, Locate, Name, Part, TaskFailure, Why};
use blaze_engine::{Buffer, CellId, Done, Phase, Planned, SVec, Stage, Task as EngineTask};
use blaze_ids::ContentId;
use blaze_own::{BTreeMap, Csr, Heap4, Share, SharedAlloc, Vec};
use blaze_scope::crate_env::EnvLent;
use blaze_scope::graph::{CrateId, Sysroot};
use blaze_scope::resolve::{CrateInput, SourceTexts};
use blaze_scope::scope_record::{CheckedHead, ModuleBitmap, Verified};
use bstd::cx::Cx;
use bstd::fs::{Dir, ReadWrite};
use bstd::mem::{Frame, Tag};
use bstd::task::Class;

use crate::engine_check::{EngineCheckError, Table};
use crate::engine_records::{EngineRecords, InPlace, ReadError};
use crate::engine_tasks::{CELLS, EXPORTS, Outlined, WHOLE};
use crate::engine_units::{Failure, Published};
use crate::fetch::FetchEnv;
use crate::names::{DepExports, Names, Resolution, Resolved, ScopeJob};
use crate::work;
use crate::CliError;

/// Where the rule that refuses a crate on this path is.
const RULE: &str = "engine_names.rs";

#[derive(Clone, Copy)]
struct NamePlan {
    depth: u16,
    has_readers: bool,
    /// Bottom-up critical-path priority: `est_weight[u] + max_{v in dependents[u]} prio[v]`.
    prio: u32,
}

/// Precomputed crate dependency DAG for `Stage::Names`: longest-path wave depths, bottom-up
/// critical-path priorities, forward/reverse CSR adjacency, and topological order.
pub(crate) struct NamesDag<'f> {
    entries: Vec<NamePlan, &'f Frame>,
    /// Direct dependencies of crate `at` (deduplicated, sorted by crate index).
    deps: Csr<u32, &'f Frame>,
    /// Direct dependents of crate `g` (deduplicated, sorted by crate index).
    dependents: Csr<u32, &'f Frame>,
    /// Crate indices in topological order (`deps` before `dependents`).
    topo: Vec<u32, &'f Frame>,
}

impl<'f> NamesDag<'f> {
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn deps_of(&self, at: usize) -> &[u32] {
        self.deps.row(at).unwrap_or(&[])
    }

    pub(crate) fn dependents_of(&self, g: usize) -> &[u32] {
        self.dependents.row(g).unwrap_or(&[])
    }

    pub(crate) fn topo(&self) -> &[u32] {
        self.topo.as_slice()
    }

    pub(crate) fn prio(&self, at: usize) -> u32 {
        self.entries.get(at).map_or(0, |entry| entry.prio)
    }
}

/// Builds the crate DAG in `O(V + E)`: forward/reverse CSR adjacency, Kahn topological order,
/// longest-path wave depths, and bottom-up critical-path priorities. Returns `Err(stuck_crate)`
/// if the graph contains a cycle (rule 18).
pub(crate) fn build_names_dag<'a, I>(
    count: usize,
    allowed: impl Fn(usize) -> bool,
    edges_of_crate: impl Fn(usize) -> I,
    frame: &'a Frame,
) -> Result<NamesDag<'a>, usize>
where
    I: IntoIterator<Item = usize>,
{
    let mut deps: Csr<u32, &'a Frame> = Csr::with_capacity_in(count, count.saturating_mul(4), frame);
    let mut in_deg: Vec<u32, &'a Frame> = Vec::from_elem_in(0, count, frame);
    let mut rev_deg: Vec<u32, &'a Frame> = Vec::from_elem_in(0, count, frame);
    let mut row_buf: Vec<u32, &'a Frame> = Vec::new_in(frame);
    let mut total_edges = 0usize;
    for at in 0..count {
        if !allowed(at) {
            deps.push_slice(&[]);
            continue;
        }
        row_buf.clear();
        for g in edges_of_crate(at) {
            if g < count && let Ok(g_u32) = u32::try_from(g) {
                row_buf.push(g_u32);
            }
        }
        row_buf.sort_unstable();
        row_buf.dedup();
        deps.push_slice(&row_buf);
        if let Some(slot) = in_deg.get_mut(at) {
            *slot = u32::try_from(row_buf.len()).unwrap_or(u32::MAX);
        }
        for &g_u32 in row_buf.iter() {
            if let Ok(g) = usize::try_from(g_u32)
                && let Some(slot) = rev_deg.get_mut(g)
            {
                *slot = slot.saturating_add(1);
            }
        }
        total_edges = total_edges.saturating_add(row_buf.len());
    }

    // Build reverse CSR `dependents` in two linear passes (strictly sorted per row, zero per-row Vecs).
    let mut offsets: Vec<usize, &'a Frame> = Vec::with_capacity_in(count.saturating_add(1), frame);
    let mut acc = 0usize;
    offsets.push(0);
    for &deg in rev_deg.iter() {
        acc = acc.saturating_add(usize::try_from(deg).unwrap_or(0));
        offsets.push(acc);
    }
    let mut cursor: Vec<usize, &'a Frame> = Vec::with_capacity_in(count, frame);
    if let Some(starts) = offsets.get(0..count) {
        cursor.extend_from_slice(starts);
    }
    let mut flat: Vec<u32, &'a Frame> = Vec::from_elem_in(0, total_edges, frame);
    for at in 0..count {
        let Ok(at_u32) = u32::try_from(at) else { continue };
        for &g_u32 in deps.row(at).unwrap_or(&[]) {
            if let Ok(g) = usize::try_from(g_u32)
                && let Some(pos) = cursor.get_mut(g)
                && let Some(dst) = flat.get_mut(*pos)
            {
                *dst = at_u32;
                *pos = pos.saturating_add(1);
            }
        }
    }
    let mut dependents: Csr<u32, &'a Frame> = Csr::with_capacity_in(count, total_edges, frame);
    for g in 0..count {
        let start = offsets.get(g).copied().unwrap_or(0);
        let end = offsets.get(g.saturating_add(1)).copied().unwrap_or(start);
        dependents.push_slice(flat.get(start..end).unwrap_or(&[]));
    }

    // Kahn topological sort + longest-path depth relaxation in O(V + E).
    let mut rem = in_deg;
    let mut topo: Vec<u32, &'a Frame> = Vec::with_capacity_in(count, frame);
    let mut entries: Vec<NamePlan, &'a Frame> = Vec::from_elem_in(NamePlan { depth: 0, has_readers: false, prio: 0 }, count, frame);
    for g in 0..count {
        if !dependents.row(g).unwrap_or(&[]).is_empty()
            && let Some(entry) = entries.get_mut(g)
        {
            entry.has_readers = true;
        }
        if rem.get(g).copied().unwrap_or(0) == 0
            && let Ok(g_u32) = u32::try_from(g)
        {
            topo.push(g_u32);
        }
    }
    let mut head = 0usize;
    while let Some(&u_u32) = topo.get(head) {
        head = head.saturating_add(1);
        let Ok(u) = usize::try_from(u_u32) else { continue };
        let next_depth = entries.get(u).map_or(0, |e| e.depth).saturating_add(1);
        for &v_u32 in dependents.row(u).unwrap_or(&[]) {
            let Ok(v) = usize::try_from(v_u32) else { continue };
            if let Some(ev) = entries.get_mut(v) {
                ev.depth = ev.depth.max(next_depth);
            }
            if let Some(left) = rem.get_mut(v) {
                *left = left.saturating_sub(1);
                if *left == 0 {
                    topo.push(v_u32);
                }
            }
        }
    }
    if topo.len() < count {
        let stuck = rem.iter().position(|&left| left > 0).unwrap_or(0);
        return Err(stuck);
    }

    // Bottom-up critical-path priority: est_weight[u] + max_{v in dependents[u]} prio[v].
    // Direct fanout boosts high-fanout hubs (core, alloc, std, syn, serde, tokio) over leaves.
    for &u_u32 in topo.iter().rev() {
        let Ok(u) = usize::try_from(u_u32) else { continue };
        let rev_row = dependents.row(u).unwrap_or(&[]);
        let fanout = u32::try_from(rev_row.len()).unwrap_or(u32::MAX);
        let est_weight = 1u32.saturating_add(fanout);
        let mut max_child = 0u32;
        for &v_u32 in rev_row {
            if let Ok(v) = usize::try_from(v_u32)
                && let Some(ev) = entries.get(v)
            {
                max_child = max_child.max(ev.prio);
            }
        }
        if let Some(eu) = entries.get_mut(u) {
            eu.prio = est_weight.saturating_add(max_child);
        }
    }
    Ok(NamesDag { entries, deps, dependents, topo })
}

enum Admission {
    Filed,
    Done,
}

/// A missing entry is pending. Both drivers keep this table through the request,
/// including entries whose resolution failed and therefore has no stored record.
struct Admissions<'f>(BTreeMap<Name, Admission, &'f Frame>);

impl Admissions<'_> {
    fn admit(&mut self, task: Name) -> Result<(), EngineCheckError> {
        use blaze_own::btree::Entry;
        match self.0.entry(task) {
            Entry::Vacant(pending) => {
                pending.insert(Admission::Filed);
                Ok(())
            }
            Entry::Occupied(_) => Err(EngineCheckError::Twice { task }),
        }
    }

    fn finish(&mut self, task: Name) -> Result<(), EngineCheckError> {
        match self.0.get_mut(&task) {
            Some(state @ Admission::Filed) => {
                *state = Admission::Done;
                Ok(())
            }
            Some(Admission::Done) => Err(EngineCheckError::Twice { task }),
            None => Err(EngineCheckError::Engine(blaze_engine::EngineError::Unplanned { name: task })),
        }
    }
}

/// The stage over one command's names world.
pub(crate) struct NamesStage<'f> {
    pub(crate) names: Option<Names<'f>>,
    crate_names: Vec<Name, &'f Frame>,
    plan: core::cell::OnceCell<NamesDag<'f>>,
    admissions: Admissions<'f>,
    /// Only the current wave's wire headers, until the store puts them.
    heads: Vec<(Name, Key, SVec<'static, u8>), &'f Frame>,
    failed_out: ContentId,
    /// Each crate entered so far, by name: its key and out, `None` when it failed.
    made: Vec<(Name, Option<(Key, ContentId)>), &'f Frame>,
    /// Each crate's result, in the order entered.
    pub(crate) results: Vec<Published<'f>, &'f Frame>,
    /// What the phases' frames held, for the profile's memory lines (engine_memory.rs).
    pub(crate) memory: crate::engine_memory::NamesMemory,
    /// `--names-oracle` ([`NamesOracle`]): each crate's comparison, sorted by crate name (rule
    /// 25: the lines listed are the same whichever order the crates finished in).
    oracle: Vec<OracleCrate, &'f Frame>,
    /// The crates filed with no comparison: failed or refused before their check reported.
    oracle_missed: Vec<Name, &'f Frame>,
    /// What each crate whose names answered this command said of the files found (D161), sorted
    /// by its task's name: read once by engine_check.rs `serve_found`, after the last names phase.
    found: Vec<FoundBy, &'f Frame>,
}

/// The phase-independent handles a names phase reads.
pub(crate) struct Handles<'h, 'f> {
    pub(crate) root: &'h Dir<ReadWrite>,
    pub(crate) lock: Option<&'f blaze_manifest::lock::Lock<'static>>,
    pub(crate) members: Option<&'h [crate::graph::CheckoutDir]>,
    pub(crate) env: &'h FetchEnv<'h>,
    pub(crate) side: Option<&'h crate::proc_runner::MacroSide<'h, 'h, 'h>>,
    pub(crate) outlined: &'h [Outlined<'h>],
    pub(crate) workers: core::num::NonZeroU32,
    /// Where each crate's resolution says it runs (engine_status.rs).
    pub(crate) board: &'h crate::engine_status::Board<'h>,
    /// Whether each crate's names are decided twice and compared (D158 step 1, [`NamesOracle`]).
    pub(crate) oracle: NamesOracle,
    /// The stamps the request's walk probed its files by (sweep.rs `FromMeta`): a file a crate's
    /// names find is probed by them too ([`Job::run`], engine_check.rs `outline_found`).
    pub(crate) stamps: crate::sweep::FromMeta,
    /// A test's plants for those outlines, read on the command's thread.
    pub(crate) plants: crate::engine_check::OutlinePlants,
    /// The found files the walk took from the last answer's places, sorted by name
    /// (engine_check.rs `PLACE_FOUND`): a crate's names say which of them they read now.
    pub(crate) found: &'h [(Name, blaze_engine::pages::FilePlace)],
}

/// What a crate's names worker is handed to serve the files its crate finds (D161,
/// [`Job::run`]): [`Handles`]' stamps, plants and found files.
#[derive(Clone, Copy)]
struct Serve<'j> {
    stamps: crate::sweep::FromMeta,
    plants: crate::engine_check::OutlinePlants,
    found: &'j [(Name, blaze_engine::pages::FilePlace)],
}

impl<'h> Handles<'h, '_> {
    fn serve(&self) -> Serve<'h> {
        Serve { stamps: self.stamps, plants: self.plants, found: self.found }
    }
}

/// What one crate's names said of the files found (D161), filed with its result
/// ([`NamesStage::found`]): each found file of the walk's it read (by name, sorted), and each
/// file it read that no outline held, outlined on its worker with the slot's probe. Kept for
/// every crate whose names ran and answered: a crate with none says it read none.
pub(crate) struct FoundBy {
    /// The crate's `CrateScope` task.
    pub(crate) unit: Name,
    pub(crate) read: SVec<'static, Name>,
    pub(crate) outlined: SVec<'static, Outlined<'static>>,
    /// Each of `outlined`'s slot, its probe and whether a module names it (its place's
    /// [`crate::engine_check::PLACE_MODULE`]), in the same order: the leaf rows the next
    /// request's sweep proves them unchanged by (sweep.rs `opened`).
    pub(crate) probes: SVec<'static, (SVec<'static, u8>, crate::sweep::Probed, bool)>,
}

/// D158 step 1: D116's equality check run by the command. Each `CrateScope` task decides every
/// cell its whole solve decided a second time, on demand (blaze-scope cells/decide.rs `check`),
/// and compares every name its written scopes answer with the module-grain records' lookup
/// (cells/agree.rs `links_agree`); the crate's answer is the whole solve's either way. Given by
/// the command's edge (`--names-oracle`, main.rs), never by the environment (rule 75).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NamesOracle {
    /// The command's default: decided once.
    Off,
    /// `--names-oracle`.
    On,
    /// `--names-oracle --block-scopes=bodies` (D189 step 4, proof 3): `On`, and each crate is
    /// solved again under both block-scope strategies with its impl tables compared (blaze-scope
    /// crate_scope.rs `block_scopes_differential`; names/scope.rs).
    Blocks,
    /// The oracle's control (rule 23, names_oracle_tests.rs): the on-demand decision planted
    /// wrong (blaze-scope task/fill.rs `DecidePlant::CachesOpenCycle`); the whole solve, and so
    /// the answer, as `On`'s.
    #[cfg(test)]
    Planted,
}

impl NamesOracle {
    pub(crate) fn on(self) -> bool {
        self != NamesOracle::Off
    }

    /// Whether D189's block-scopes differential runs per crate.
    pub(crate) fn blocks(self) -> bool {
        self == NamesOracle::Blocks
    }

    /// The plant a crate's resolution hands its on-demand decision.
    pub(crate) fn plant(self) -> blaze_scope::task::DecidePlant {
        match self {
            NamesOracle::Off | NamesOracle::On | NamesOracle::Blocks => blaze_scope::task::DecidePlant::None,
            #[cfg(test)]
            NamesOracle::Planted => blaze_scope::task::DecidePlant::CachesOpenCycle,
        }
    }
}

/// The most disagreement lines one run lists, by crate name, each crate's kept lines together
/// (at most 8 cells and its names-records line); the rest are counted (rule 18). Sized to list
/// every disagreeing crate of a run with its first cells (each line is formatted in the
/// request's frame, about 1 KiB): at 20, tempo's 23 crates (21 Nov 05:33) printed 20 lines and
/// hid 3 crates' evidence.
pub(crate) const ORACLE_LISTED: usize = 512;

/// One crate's comparison (names/scope.rs `FilePar::decided`), owned on the request's heap.
pub(crate) struct OracleCrate {
    pub(crate) krate: blaze_own::String<SharedAlloc<'static>>,
    /// Cells the whole solve decided, each decided again on demand.
    pub(crate) cells: u64,
    /// Cells whose two decisions differ, summed over decide.rs's forward, backward and text
    /// passes (one cell can count in more than one pass).
    pub(crate) differences: u64,
    /// Decisions on demand that waited on a call not grown (`Decided::Wait`).
    pub(crate) waits: u64,
    /// Reads the forward pass logged: the work its asks did.
    pub(crate) hops: u64,
    /// The crate's modules (walked and made).
    pub(crate) modules: u64,
    pub(crate) links: OracleLinks,
    /// Each disagreement kept (decide.rs keeps a crate's first 8 cells; agree.rs stops at its
    /// first), as a line naming the crate, module, namespace, name and both answers.
    pub(crate) lines: blaze_own::Vec<blaze_own::String<SharedAlloc<'static>>, SharedAlloc<'static>>,
    /// `--block-scopes=bodies` (D189): the crate's block-scopes differential, summed; `None`
    /// without the flag.
    pub(crate) blocks: Option<blaze_scope::body_scope::BlocksTally>,
    /// Its verdict line when the crate differs or its differential did not run.
    pub(crate) blocks_line: Option<blaze_own::String<SharedAlloc<'static>>>,
}

impl OracleCrate {
    /// Cells that differ plus a names-records disagreement.
    pub(crate) fn disagreements(&self) -> u64 {
        self.differences + u64::from(matches!(self.links, OracleLinks::Differ))
    }
}

/// What agree.rs's comparison of the written scopes with the module-grain records said.
#[derive(Clone, Copy, Debug)]
pub(crate) enum OracleLinks {
    /// The comparison did not report (the crate's check gave no `links`).
    NotRun,
    /// Every name compared agreed: `names` lookups of `modules` modules; `capped` glob-reached
    /// names not asked (agree.rs `MAX_GLOB_SOURCES`, `MAX_GLOB_NAMES`).
    Agreed { modules: u64, names: u64, capped: u64 },
    /// The first disagreement (agree.rs stops at it), in the crate's lines.
    Differ,
}

/// The run's sums over every crate compared.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct OracleTotals {
    /// Crates compared.
    pub(crate) crates: u64,
    /// Crates entered whose check did not report: failed, refused, or their check stopped.
    pub(crate) not_compared: u64,
    pub(crate) cells: u64,
    pub(crate) differences: u64,
    pub(crate) waits: u64,
    pub(crate) hops: u64,
    pub(crate) modules: u64,
    /// agree.rs: names compared, glob-reached names not asked, crates that disagreed.
    pub(crate) link_names: u64,
    pub(crate) link_capped: u64,
    pub(crate) link_differ: u64,
    /// `--block-scopes=bodies` (D189): the differential's sums; `None` without the flag.
    pub(crate) blocks: Option<blaze_scope::body_scope::BlocksTally>,
}

impl OracleTotals {
    pub(crate) fn disagreements(&self) -> u64 {
        self.differences + self.link_differ
    }

    /// The typed reason an oracle run fails (rule 61: never a pass for a check that did not
    /// run). A crate not compared (failed or refused before its check) is counted and named,
    /// and fails the run only when no crate at all was compared.
    pub(crate) fn finding(&self) -> Option<OracleFinding> {
        if self.disagreements() > 0 {
            Some(OracleFinding::Disagreed { cells: self.differences, links: self.link_differ })
        } else if let Some(b) = self.blocks.filter(|b| !b.agrees()) {
            Some(OracleFinding::Blocks { differ: b.differ, failed: b.failed })
        } else if self.crates == 0 {
            Some(OracleFinding::NoneCompared)
        } else {
            None
        }
    }
}

/// Why `--names-oracle` exits nonzero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OracleFinding {
    /// The two decisions differ: `cells` cell differences, `links` crates whose names records
    /// answered a name otherwise.
    Disagreed { cells: u64, links: u64 },
    /// No `CrateScope` task reported its check.
    NoneCompared,
    /// `--block-scopes=bodies` (D189): `differ` crates' impl tables differ under the two
    /// block-scope strategies (or a held body was refused); `failed` crates' differential did
    /// not run.
    Blocks { differ: u64, failed: u64 },
}

impl core::fmt::Display for OracleFinding {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            OracleFinding::Disagreed { cells, links } => write!(f, "names oracle: the whole solve and the on-demand decision disagree ({cells} cell difference(s), {links} crate(s) whose names records answer otherwise; D116, D158)"),
            OracleFinding::NoneCompared => write!(f, "names oracle: no crate was compared (no `CrateScope` task reported its check; D158)"),
            OracleFinding::Blocks { differ, failed } => write!(f, "names oracle: the block-scopes differential: {differ} crate(s) differ and {failed} did not run (D189)"),
        }
    }
}

impl<'f> NamesStage<'f> {
    pub(crate) fn new(names: Names<'f>, frame: &'f Frame) -> NamesStage<'f> {
        // From the crate list: no world is made to plan (names.rs `made_or_make`).
        let list = names.list();
        let mut crate_names = Vec::with_capacity_in(blaze_world::crate_list::CrateGraph::len(list), frame);
        crate_names.extend((0..blaze_world::crate_list::CrateGraph::len(list)).map(|r| blaze_drive::key::crate_scope_name(&list.key(r))));
        NamesStage { names: Some(names), crate_names, plan: core::cell::OnceCell::new(), admissions: Admissions(BTreeMap::new_in(frame)), heads: Vec::new_in(frame), failed_out: work::failed_out(), made: Vec::new_in(frame), results: Vec::new_in(frame), memory: crate::engine_memory::NamesMemory::new(), oracle: Vec::new_in(frame), oracle_missed: Vec::new_in(frame), found: Vec::new_in(frame) }
    }

    /// What each crate whose names answered this command said of the files found (D161,
    /// [`FoundBy`]), sorted by its task's name.
    pub(crate) fn found(&self) -> &[FoundBy] {
        &self.found
    }

    /// Drops the per-crate found-file outlines and probes once `serve_found` has applied them.
    pub(crate) fn clear_found(&mut self) {
        self.found.clear();
    }

    /// The oracle's sums over every crate filed.
    pub(crate) fn oracle_totals(&self) -> OracleTotals {
        let mut t = OracleTotals { crates: self.oracle.len() as u64, not_compared: self.oracle_missed.len() as u64, ..OracleTotals::default() };
        for one in self.oracle.iter() {
            t.cells += one.cells;
            t.differences += one.differences;
            t.waits += one.waits;
            t.hops += one.hops;
            t.modules += one.modules;
            match one.links {
                OracleLinks::NotRun => {}
                OracleLinks::Agreed { names, capped, .. } => {
                    t.link_names += names;
                    t.link_capped += capped;
                }
                OracleLinks::Differ => t.link_differ += 1,
            }
            if let Some(b) = one.blocks {
                t.blocks.get_or_insert_default().add(b);
            }
        }
        t
    }

    /// The block-scopes differential's first [`ORACLE_LISTED`] verdict lines (D189), by crate
    /// name, apart from the cells' lines, and how many more were not listed.
    pub(crate) fn blocks_listed(&self) -> (impl Iterator<Item = &str>, u64) {
        let kept = self.oracle.iter().filter(|one| one.blocks_line.is_some()).count();
        (self.oracle.iter().filter_map(|one| one.blocks_line.as_ref().map(|line| line.as_str())).take(ORACLE_LISTED), kept.saturating_sub(ORACLE_LISTED) as u64)
    }

    /// The run's first [`ORACLE_LISTED`] disagreements, by crate name, and how many more were
    /// counted than listed (past [`ORACLE_LISTED`], or past what one crate's check keeps).
    pub(crate) fn oracle_listed(&self) -> (impl Iterator<Item = &str>, u64) {
        let kept: u64 = self.oracle.iter().map(|one| one.lines.len() as u64).sum();
        let listed = kept.min(ORACLE_LISTED as u64);
        let total = self.oracle_totals().disagreements();
        (self.oracle.iter().flat_map(|one| one.lines.iter().map(|line| line.as_str())).take(ORACLE_LISTED), total.saturating_sub(listed))
    }

    /// The crates filed with no comparison, by name.
    pub(crate) fn oracle_missed(&self) -> impl Iterator<Item = &str> {
        let names = self.names.as_ref();
        self.oracle_missed.iter().map(move |name| {
            names.and_then(|names| names.crate_at(name.subject).map(|at| names.crate_label(at).0)).unwrap_or("?")
        })
    }

    /// The profile's oracle lines: the totals, D116's counts, then one line a crate.
    pub(crate) fn oracle_profile(&self, text: &mut impl core::fmt::Write) {
        use core::fmt::Write as _;
        let t = self.oracle_totals();
        // A full frame cuts the profile short where it stood (as engine_check.rs `profile_text`).
        let _ = writeln!(text, "names oracle (D158 step 1, D116): {} crate(s) compared, {} not compared; {} cell(s) compared, {} difference(s); {} name(s) compared against the names records ({} glob-reached not asked), {} crate(s) disagreeing", t.crates, t.not_compared, t.cells, t.differences, t.link_names, t.link_capped, t.link_differ);
        let _ = writeln!(text, "names oracle counts: {} wait(s) on calls not grown, {} hop(s) for {} cell(s) ({} per hundred cells), {} module(s); contradictions, reruns, cycle groups and modules loaded per ask are not counted by decide.rs", t.waits, t.hops, t.cells, (t.hops * 100).checked_div(t.cells).unwrap_or(0), t.modules);
        // `--block-scopes=bodies` (D189): the summed tally, its own line (BlocksTally's Display,
        // with "modules made for body solves: N; bodies: M").
        if let Some(blocks) = t.blocks {
            let _ = writeln!(text, "names oracle {blocks}");
        }
        for one in self.oracle.iter() {
            let _ = write!(text, "names oracle crate `{}`: {} cell(s), {} difference(s), {} wait(s), {} hop(s), {} module(s); names records: ", one.krate, one.cells, one.differences, one.waits, one.hops, one.modules);
            let _ = match one.links {
                OracleLinks::NotRun => writeln!(text, "not run"),
                OracleLinks::Agreed { modules, names, capped } => writeln!(text, "{names} agreed over {modules} module(s), {capped} capped"),
                OracleLinks::Differ => writeln!(text, "disagreed"),
            };
            if let Some(blocks) = one.blocks {
                let _ = writeln!(text, "names oracle crate `{}`: {blocks}", one.krate);
            }
            if let Some(line) = one.blocks_line.as_ref() {
                let _ = writeln!(text, "names oracle crate `{}`: {}", one.krate, line.as_str());
            }
        }
        for krate in self.oracle_missed() {
            let _ = writeln!(text, "names oracle crate `{krate}`: not compared");
        }
    }

    pub(crate) fn dag(&self) -> Option<&NamesDag<'f>> {
        self.plan.get()
    }

    pub(crate) fn crate_name(&self, at: usize) -> Option<Name> {
        self.crate_names.get(at).copied()
    }

    pub(crate) fn prio(&self, name: Name) -> u32 {
        self.names
            .as_ref()
            .and_then(|names| names.crate_at(name.subject))
            .and_then(|at| self.plan.get().map(|dag| dag.prio(at)))
            .unwrap_or(0)
    }

    pub(crate) fn feeds_names(&self, name: Name) -> bool {
        self.names
            .as_ref()
            .and_then(|names| names.crate_at(name.subject))
            .and_then(|at| self.plan.get()?.entries.get(at))
            .is_some_and(|entry| entry.has_readers)
    }

    /// A standing dependency can still change when one of its own producers publishes.
    /// Follow the same crate and sysroot edges that a resolution will read.
    pub(crate) fn producers_ready(&self, name: Name, pending: &mut dyn FnMut(Name) -> bool, walk: &mut ProducerWalk<'_>) -> bool {
        let Some(names) = self.names.as_ref() else { return true };
        let Some(at) = names.crate_at(name.subject) else { return true };
        let list = names.list();
        // Each crate's inputs are visited once per frontier, even when every crate of
        // a Names wave shares the same sysroot. `plan` already checked the edges for cycles.
        walk.ready.resize(blaze_world::crate_list::CrateGraph::len(list), (0, false));
        walk.post.clear();
        walk.post.push((at, false));
        while let Some((index, expanded)) = walk.post.pop() {
            if walk.ready[index].0 == walk.generation { continue; }
            let edges = || edges_listed(list, index);
            let settled = if expanded {
                edges().all(|g| walk.ready[g] == (walk.generation, true))
            } else if names.script_of(index).is_some_and(&mut *pending) || edges().any(|g| pending(self.crate_names[g])) {
                false
            } else {
                walk.post.push((index, true));
                walk.post.extend(edges().filter(|&g| walk.ready[g].0 != walk.generation).map(|g| (g, false)));
                continue;
            };
            walk.ready[index] = (walk.generation, settled);
        }
        walk.ready[at].1
    }

    /// One `CrateScope` task a crate of the world (or the active scoped closure), at its depth
    /// (sorted by name).
    pub(crate) fn plan<'h>(&self, scoped: Option<&crate::engine_check::ScopedUnits>, alloc: SharedAlloc<'h>, _frame: &Frame) -> Result<SVec<'h, Planned>, CliError> {
        let names = self.names.as_ref().ok_or(CliError::Text { what: "the names world, made by the planner, is gone (engine_names.rs `plan`)" })?;
        // Planned from the crate list: no world is made to plan (names.rs `made_or_make`).
        let list = names.list();
        let count = blaze_world::crate_list::CrateGraph::len(list);
        let dag = build_names_dag(
            count,
            |at| scoped.is_none_or(|s| s.allows(at)),
            |at| edges_listed(list, at),
            *self.crate_names.allocator(),
        )
        .map_err(|stuck| EngineCheckError::Cycle { krate: blaze_drive::key::crate_scope_name(&list.key(stuck)) })?;
        let mut planned = SVec::new_in(alloc);
        for (name, source) in crate::names::parts(names).iter() {
            let at = names.crate_at(name.subject).ok_or(EngineCheckError::MissingEntry { task: *name, table: Table::WorldCrates, key: *name })?;
            if scoped.is_some_and(|s| !s.allows(at)) {
                continue;
            }
            let depth = dag.entries.get(at).map_or(0, |entry| entry.depth);
            planned.push(Planned { name: *name, phase: Phase::new(Stage::Names, depth), source: *source, file: *name });
        }
        planned.sort_unstable_by_key(|one| one.name);
        self.plan.set(dag).map_err(|_| CliError::Text { what: "the Names consumer index was planned twice (engine_names.rs)" })?;
        Ok(planned)
    }

    /// One names phase: every task entered once, its result in `buffer`, each resolution's
    /// time on its worker in `times`.
    #[expect(clippy::too_many_arguments, reason = "the phase loop's handles, each named")]
    pub(crate) fn run_phase<'a, 'r>(
        &mut self,
        cx: &Cx,
        store: &mut dyn crate::store::Store<'r>,
        read: &mut EngineRecords<'r>,
        view: blaze_engine::View<'_, '_>,
        handles: &Handles<'_, 'f>,
        tasks: &[EngineTask],
        buffer: &mut Buffer<'a>,
        times: &mut crate::engine_times::Times,
        alloc: SharedAlloc<'a>,
        frame: &'f Frame,
    ) -> Result<(), CliError> {
        let cx = cx.span("engine: one names phase");
        self.heads.clear();
        let own = Frame::with_reserve(crate::names::SCOPE_JOBS, Tag::Nameres)
            .map_err(|_| CliError::Text { what: "a names phase's frame could not be reserved" })?;
        let erased = blaze_own::Erased::new(&own);
        let wave = erased.handle();
        let names = self.names.as_ref().ok_or(CliError::Text { what: "the names world is gone (engine_names.rs `run_phase`)" })?;
        // A names task runs: the world is made now if no task made it yet (one a request, under
        // the span "names: make the world (needed by a names task)"); a world that cannot be
        // made stops the request by a typed error (rule 3).
        let made = match names.made_or_make(&cx, handles.root, store, handles.env, crate::names::NeededBy::NamesTask)? {
            Ok(made) => made,
            Err(why) => return Err(EngineCheckError::NamesWorld(why).into()),
        };
        let runner = Self::runner(names, made, handles, frame);
        let world = made.world();
        let crates = world.crates.as_slice();
        let all = world.sysroot;
        let packs = made.packs(&handles.env.fs);
        let corrected = world.texts.corrected();
        let outlined = handles.outlined;
        let root = handles.root;
        let clock = &handles.env.clock;
        let home = handles.env.home;
        let board = handles.board;
        let workers = handles.workers;
        let kept = handles.env.heap.shared();
        let shared_prepares = crate::names::SharedPrepares::new(kept).ok();
        let oracle = handles.oracle;
        let serve = handles.serve();
        let failed_out = self.failed_out;
        let mut exports = ReadExports { read, route: crate::engine_memory::route_now() };
        let phase_snap = store.snapshot()?;
        let mut stack = Vec::new_in(&own);
        let mut scratch = Vec::new_in(&own);
        let mut answered = Vec::with_capacity_in(tasks.len(), &own);
        {
            let slots = Vec::from_iter_in((0..crates.len()).map(|_| OnceLock::new()), wave);
            let inputs = WaveInputs { crates, all, names: &self.crate_names, made: &self.made, failed_out, slots: &slots };
            let inputs = &inputs;
            let waiter = blaze_core::wait::Parker::new();
            let mut live: Vec<&bstd::sync::Flag, &Frame> = Vec::new_in(&own);
            let scope = cx.scope();
            // The admission count enforces a one-worker request. A threaded scope
            // narrowed to one would run even submitted work on its coordinator.
            scope.narrow_workers(handles.workers.max(core::num::NonZeroU32::new(2).expect("two workers")), Class::Foreground);
            let width = handles.workers.get().min(scope.workers()) as usize;
            let mut running = Vec::with_capacity_in(tasks.len(), &own);
            // Store reads require the coordinator's exclusive handle. Queue each crate as soon
            // as its last mapping is available, including while later crates are being read.
            let dispatch = (|| -> Result<(), CliError> {
                for (i, task) in tasks.iter().enumerate() {
                    if board.stopping().is_some() { break; }
                    let name = task.planned.name;
                    self.admissions.admit(name)?;
                    let Some(at) = names.crate_at(name.subject) else {
                        answered.push((i, Completion { done: crate::engine_tasks::failed(task, alloc), result: Err(refusal(name, "a `CrateScope` task of a crate the world does not hold")), scope_bytes: crate::names::metered::ScopeBytes::default() }));
                        continue;
                    };
                    #[cfg(test)]
                    let end_tick = crate::engine_scripts::runner_clock().map(|clock| {
                        clock.names_entered(name, crates[at].input.name);
                        clock.names_admitted(name, crates[at].input.name)
                    });
                    let script = names.script_of(at).map(|script| lookup(&self.made, name, script, failed_out)).transpose()?;
                    let macro_module = {
                        use blaze_world::crate_list::CrateGraph as _;
                        let list = names.list();
                        if list.proc_macro(at) {
                            let mm = Name::new(Kind::MacroModule, crate::macros::subject_of(list.package(at), list.version(at)));
                            self.made.binary_search_by_key(&mm, |(n, _)| *n).ok().and_then(|idx| self.made[idx].1).map(|(key, out)| Input { name: mm, at: key, out })
                        } else {
                            None
                        }
                    };
                    let gate = match (runner, world.refusal(at), script) {
                        (Err(words), _, _) => Err(Blocked::Runner(words)),
                        (_, Some(why), _) => Err(Blocked::Shelf(why)),
                        (_, _, Some((input, true))) => Err(Blocked::Script(input.name)),
                        (Ok(procs), None, _) => Ok(procs),
                    };
                    let script_bytes = match (gate.is_ok(), script) {
                        (true, Some((input, _))) => {
                            let read = work::payload(&cx, store, &input, &own)?;
                            Some((input, own.alloc(read).payload()))
                        }
                        _ => None,
                    };
                    // The first visit maps this branch for the entire wave. A worker starts
                    // only after every slot reachable from its crate has been published.
                    if gate.is_ok() { stack.extend(edges_of(&crates[at], all, at)); }
                    while let Some(g) = stack.pop() {
                        if slots[g].get().is_some() { continue; }
                        let (input, _) = lookup(inputs.made, name, inputs.names[g], failed_out)?;
                        let bytes = if input.out == failed_out {
                            Mapped::Failed
                        } else {
                            let krate_g = crates.get(g).map_or("", |c| c.input.name);
                            let got = match exports.exports_committed(&cx, store, &input, krate_g, wave) {
                                Ok(Some(bytes)) => Ok(bytes),
                                Ok(None) => match phase_snap.as_ref() {
                                    Some(snap) => exports.exports_from_snapshot(snap.as_ref(), &input, krate_g, wave).map(|(bytes, _, _)| bytes),
                                    None => exports.exports(&cx, store, &input, krate_g, &own, wave),
                                },
                                Err(error) => Err(error),
                            };
                            match got {
                                Ok(bytes) => {
                                    let cell_digest = blaze_engine::CellDigests::cell(&view, &CellId { of: input.name, cell: EXPORTS });
                                    let has_exports = cell_digest != blaze_drive::key::ABSENT;
                                    let known_digest = has_exports.then_some(cell_digest);
                                    Mapped::Lazy(own.alloc(LazyExports::new(bytes, krate_g, known_digest)), has_exports)
                                }
                                Err(ReadError::Load(crate::front_load::LoadError::Store(error))) => return Err(error),
                                Err(error) => Mapped::Unreadable(blaze_own::format_in!(&own, "{error}").into_bump_str()),
                            }
                        };
                        slots[g].set(bytes).map_err(|_| EngineCheckError::Twice { task: input.name })?;
                        stack.extend(edges_of(&crates[g], all, g));
                    }
                    let label = crate::engine_status::Label::of(crates[at].input.name);
                    let env = world.texts.crate_env(at).map(EnvLent::new);
                    if live.len() == width {
                        crate::engine_scripts::wait_finished(&cx, clock, &waiter, || live.iter().any(|flag| flag.is_set()), "a Names worker slot", live.len())?;
                        live.retain(|flag| !flag.is_set());
                    }
                    let finished: &bstd::sync::Flag = own.alloc(bstd::sync::Flag::new());
                    let notice = crate::engine_scripts::Notice { finished, waiter: &waiter };
                    live.push(finished);
                    let job_prepares = shared_prepares.as_ref().map(crate::names::SharedPrepares::share);
                    running.push((i, scope.may_overlap("crate scope", crate::names::SCOPE_FRAME, move |cx: &Cx| {
                        let _notice = notice;
                        let began = clock.now();
                        let shown = board.show(Stage::Names, label, 0, board.now(clock));
                        let result = work::unwinding(|| {
                            let scratch = blaze_own::Erased::new(cx.frame());
                            let mut deps = inputs.dependencies(at, name, scratch.handle())?;
                            let fail = |why, words| WorkerAnswer {
                                done: crate::engine_tasks::failed_with(task, failure_reads(&deps, None, script, alloc), alloc),
                                result: Err((TaskFailure { name, why }, words)),
                                scope_bytes: crate::names::metered::ScopeBytes::default(),
                            };
                            let procs = match gate {
                                Err(Blocked::Runner(message)) => {
                                    let why = Why::Refused { what: "the proc-macro runner could not be made", rule: crate::names::RULE };
                                    return Ok(fail(why, Some(blaze_own::String::from_str_in(message, alloc))));
                                }
                                Err(Blocked::Shelf(reason)) => {
                                    let why = Why::Refused { what: "a crate the toolchain shelf cannot build", rule: crate::names::RULE };
                                    return Ok(fail(why, Some(blaze_own::format_in!(alloc, "crate `{}`: {reason}", crates[at].input.name))));
                                }
                                Err(Blocked::Script(on)) => return Ok(fail(Why::Waited { on }, None)),
                                Ok(procs) => procs,
                            };
                            for dep in &mut deps {
                                let g = dep.index;
                                match inputs.slots[g].get().ok_or(EngineCheckError::MissingEntry { task: name, table: Table::MappedExports, key: dep.input.name })? {
                                    Mapped::Lazy(lazy, has_exports) => {
                                        dep.exports = Some(ExportRef::Lazy(lazy));
                                        dep.has_exports = *has_exports;
                                    }
                                    Mapped::Failed => {}
                                    Mapped::Unreadable(why) => {
                                        let words = blaze_own::format_in!(alloc, "crate `{}`: the record of its dependency `{}`: {why}", crates[at].input.name, crates[g].input.name);
                                        let failed = TaskFailure { name, why: Why::Refused { what: "a dependency's record", rule: crate::names::RULE } };
                                        return Ok(WorkerAnswer { done: crate::engine_tasks::failed_with(task, failure_reads(&deps, None, script, alloc), alloc), result: Err((failed, Some(words))), scope_bytes: crate::names::metered::ScopeBytes::default() });
                                    }
                                }
                            }
                            let job = Job { at, crates, all, deps: &deps, uncommitted_snap: None, script: script_bytes, macro_module, home, env, packs, corrected, procs, kept, shared_prepares: job_prepares, workers, oracle, clock, board, serve };
                            job.run(cx, root, task, outlined, alloc)
                        });
                        shown.end(board.now(clock));
                        Ok::<_, Infallible>((result, crate::engine_times::ns(began.since_start()), crate::engine_times::ns(clock.now().since_start()), cx.frame().committed() as u64))
                    }), at, script, {
                        #[cfg(test)] { end_tick }
                        #[cfg(not(test))] { () }
                    }));
                }
                Ok(())
            })();
            let waited = crate::engine_scripts::wait_finished(&cx, clock, &waiter, || live.iter().all(|flag| flag.is_set()), "the Names wave's workers to end", live.len());
            #[cfg(test)]
            if schedule_test::reverse_delivery() { running.reverse(); }
            #[cfg(test)]
            if running.iter().any(|(_, _, _, _, end)| end.is_some_and(|tick| tick > 0)) {
                running.sort_unstable_by_key(|(i, _, _, _, end)| (*end, tasks[*i].planned.name));
            }
            let mut stopped = None;
            let mut error = None;
            for (i, handle, at, script, _end_tick) in running {
                let task = &tasks[i];
                let name = task.planned.name;
                match handle.join(&cx) {
                    Ok((result, start, end, committed)) => {
                        times.interval(name, start, end);
                        #[cfg(test)]
                        if let (Some(clock), Some(tick)) = (crate::engine_scripts::runner_clock(), _end_tick) {
                            clock.names_finished(name, tick);
                        }
                        scratch.push((committed, name));
                        match result {
                            Ok(Ok(result)) => match result.into_completion(frame) {
                                Ok(answer) => {
                                    self.memory.scope_bytes(answer.scope_bytes);
                                    answered.push((i, answer));
                                }
                                Err(why) => { error.get_or_insert(why); }
                            },
                            Ok(Err(why)) => { error.get_or_insert(why.into()); }
                            Err(work::Panicked) => {
                                let deps = match inputs.dependencies(at, name, wave) {
                                    Ok(deps) => deps,
                                    Err(why) => { error.get_or_insert(why.into()); continue; }
                                };
                                let reads = failure_reads(&deps, None, script, alloc);
                                answered.push((i, Completion {
                                    done: crate::engine_tasks::failed_with(task, reads, alloc),
                                    result: Err(Failure { failure: TaskFailure { name, why: Why::Panicked { used: committed, limit: crate::names::SCOPE_FRAME as u64 } }, words: None }),
                                    scope_bytes: crate::names::metered::ScopeBytes::default(),
                                }));
                            }
                        }
                    }
                    Err(why) => { stopped.get_or_insert(why); }
                }
            }
            scope.join(&cx).map_err(CliError::Pool)?;
            dispatch?;
            waited?;
            if let Some(why) = stopped { return Err(CliError::Pool(why)); }
            if let Some(why) = error { return Err(why); }
        }
        order_completions(&mut answered);
        for (i, answer) in answered {
            let task = &tasks[i];
            self.file(task, answer, buffer)?;
        }
        let depth = tasks.first().map_or(0, |task| task.planned.phase.wave);
        self.memory.wave(depth, own.committed() as u64, &mut scratch, handles.workers.get());
        Ok(())
    }

    fn runner(names: &Names<'f>, made: &crate::names::NamesWorld<'_>, handles: &Handles<'_, 'f>, frame: &'f Frame) -> Result<Option<&'static crate::proc_runner::ProcRunner<'static>>, &'f str> {
        if !made.world().crates.iter().any(|c| c.input.implicit.proc_macro) {
            return Ok(None);
        }
        match (names.procs(), handles.side) {
            (None, Some(side)) => match crate::proc_runner::make(made, *names.list(), side) {
                Ok(runner) => {
                    names.set_procs(runner);
                    Ok(Some(runner))
                }
                Err(error) => {
                    let words = bstd::mem::format_in!(frame, "names not resolved: the proc-macro runner: {error}");
                    Err(work::held_str(words.as_str(), frame))
                }
            },
            (runner, _) => Ok(runner),
        }
    }

    fn file<'a>(&mut self, task: &EngineTask, answer: Completion<'a, 'f>, buffer: &mut Buffer<'a>) -> Result<(), CliError> {
        let name = task.planned.name;
        self.admissions.finish(name)?;
                let (made, key, header) = match answer.result {
            Ok(Finished { record, resolved, ok, unresolved, key, head, oracle, found, .. }) => {
                let at = self.found.partition_point(|one| one.unit < found.unit);
                if self.found.get(at).is_some_and(|one| one.unit == found.unit) {
                    return Err(EngineCheckError::Twice { task: name }.into());
                }
                self.found.insert(at, found);
                match oracle {
                    Some(one) => {
                        let at = self.oracle.partition_point(|filed| filed.krate.as_str() < one.krate.as_str());
                        self.oracle.insert(at, one);
                    }
                    None => self.oracle_missed.push(name),
                }
                let names = self.names.as_mut().ok_or(EngineCheckError::MissingEntry { task: name, table: Table::NamesWorld, key: name })?;
                names.keep_resolution(name, resolved, ok, unresolved).map_err(|_| EngineCheckError::Twice { task: name })?;
                let at = self.heads.partition_point(|entry| entry.0 < name);
                self.heads.insert(at, (name, key, head));
                (Ok(record), Some(key), Some(at))
            }
            Err(failure) => {
                self.oracle_missed.push(name);
                (Err(failure), None, None)
            }
        };
        self.mark(name, key.zip(made.as_ref().ok().map(|record| record.out)));
        #[cfg(test)]
        schedule_test::merged(task, &made, &answer.done, key, header.map(|at| self.heads[at].2.as_slice()));
        #[cfg(not(test))]
        let _ = header;
        buffer.done.push(answer.done);
        self.results.push(Published { name, source: task.planned.source, wave: task.planned.phase.wave, made });
        Ok(())
    }
    /// The key and wire header for this phase's put, including filed resolution failures.
    pub(crate) fn publication(&self, name: Name) -> Result<(Key, &[u8]), EngineCheckError> {
        let index = self.heads.binary_search_by_key(&name, |entry| entry.0).map_err(|_| EngineCheckError::MissingEntry { task: name, table: Table::WaveHeads, key: name })?;
        let (_, key, bytes) = &self.heads[index];
        Ok((*key, bytes.as_slice()))
    }

    pub(crate) fn put_finished(&mut self) {
        self.heads.clear();
    }

    /// `name` entered: its key and out, `None` when it failed. The `Scripts` stage marks each
    /// run here too, so a crate's task finds its script's record as it finds its dependencies'.
    pub(crate) fn mark(&mut self, name: Name, made: Option<(Key, ContentId)>) {
        match self.made.binary_search_by_key(&name, |(name, _)| *name) {
            Ok(at) => self.made[at].1 = made,
            Err(at) => self.made.insert(at, (name, made)),
        }
    }
}

/// Where a crate's names input is taken from ([`NamesInputs`]): the command's own entries
/// alone (the phase path plans every `Names` and `Scripts` task, so an entry is made or marked
/// for each), or, beside them, the rows the request STANDS on (the ready driver's: it plans no
/// `Names` task, and `admit` entered none, so every crate's names row and script row is the
/// stored answer's, unrun and unmoved: engine_ready_edit.rs `admit`, `READY_KINDS`).
#[derive(Clone, Copy)]
pub(crate) enum NamesFrom<'h> {
    /// Entered or marked this command: no entry is no input.
    Ran,
    /// A name with no entry this command stands: its row and record key read from the stored
    /// answer by need (its page read once a request, blaze-engine headers.rs `Headers::header`).
    Stood(Stood<'h>),
}

/// The stored answer the ready driver stands on, and the slot its one typed refusal is kept in:
/// a standing row that is neither failed nor a `Given` and names no record (a damaged answer;
/// the phase path's `MissingRecord`). The caller reads [`Stood::missing`] after the setup and
/// refuses the request by it (rule 3), as it reads the pages' own refusal (`StoredPages::refusal`).
#[derive(Clone, Copy)]
pub(crate) struct Stood<'h> {
    pub(crate) pages: &'h dyn blaze_engine::StoredPages,
    pub(crate) missing: &'h OnceLock<Name>,
}

impl Stood<'_> {
    /// THE ONE RULE of a standing names row (every reader on the ready driver asks it here:
    /// [`NamesInputs`], engine_seeds.rs `RequestTable`'s crates by need): `name`'s row read by
    /// need, `Ok(input)` (its record key and out); `Err(true)`: the row failed (a failed row, the
    /// failed out, or a `Given`), as the phase path's marks give it; `Err(false)`: no row. A row
    /// naming no record is kept in [`Stood::missing`], the caller's typed refusal.
    pub(crate) fn input(&self, name: Name) -> Result<Input, bool> {
        // A page that does not read is kept as the pages' refusal (engine_answer.rs `page_at`),
        // which the caller raises after the setup.
        let Ok(Some(header)) = blaze_engine::StoredPages::header(self.pages, name) else { return Err(false) };
        if header.facts.failed || header.facts.out == work::failed_out() || name.kind == Kind::Given {
            return Err(true);
        }
        match header.record {
            Some(at) => Ok(Input { name, at, out: header.facts.out }),
            None => {
                let _ = self.missing.set(name);
                Err(false)
            }
        }
    }
}

/// The names inputs seam's ONE rule ([`NamesInputs`] asks it here), apart from the stage, which
/// is not `Sync` (its plan is a `OnceCell`): what a reader shared by the body workers holds
/// (engine_seeds.rs `RequestTable`'s crates by need). `made`: the stage's entries this command,
/// sorted by name; `stood`: the stored answer the ready driver stands on, `None` on the phase path.
#[derive(Clone, Copy)]
pub(crate) struct NamesKeys<'h> {
    made: &'h [(Name, Option<(Key, ContentId)>)],
    failed_out: ContentId,
    stood: Option<Stood<'h>>,
}

impl NamesKeys<'_> {
    /// `name`'s entry this command (`Some(None)`: entered and failed), or `None`: not entered.
    fn entered(&self, name: Name) -> Option<Option<Input>> {
        let at = self.made.binary_search_by_key(&name, |(name, _)| *name).ok()?;
        Some(self.made[at].1.filter(|(_, out)| *out != self.failed_out).map(|(at, out)| Input { name, at, out }))
    }

    /// `name`'s standing row, read by need ([`Stood::input`]); `Err(false)` on the phase path.
    fn standing(&self, name: Name) -> Result<Input, bool> {
        let Some(stood) = self.stood else { return Err(false) };
        stood.input(name)
    }

    /// `name`'s input this command: entered, or standing (as [`NamesInputs::input_of`]).
    pub(crate) fn input_of(&self, name: Name) -> Option<Input> {
        match self.entered(name) {
            Some(entered) => entered,
            None => self.standing(name).ok(),
        }
    }
}

/// THE NAMES INPUTS SEAM: what a crate's (or a build script's) names input is this command, the
/// one way every reader asks it (the Bodies setup, the check scope, the dependency heads). A
/// name entered this command answers from the stage's entries ([`NamesStage::mark`]); else, under
/// [`NamesFrom::Stood`], from its standing row: its record key and out, `None` when it failed (a
/// failed row, or the failed out), as the phase path's marks give it (engine_check.rs restores
/// each from the same row). A crate with no names row at all has no input under either: it is
/// refused by name, never taken as checked. Methods named as the stage's, so a reader holding
/// this seam asks it, never the stage (inherent methods are found before the `Deref`).
#[derive(Clone, Copy)]
pub(crate) struct NamesInputs<'h, 'n> {
    stage: &'h NamesStage<'n>,
    from: NamesFrom<'h>,
}

impl<'h, 'n> NamesInputs<'h, 'n> {
    /// The phase path's: the stage's entries alone.
    pub(crate) fn ran(stage: &'h NamesStage<'n>) -> NamesInputs<'h, 'n> {
        NamesInputs { stage, from: NamesFrom::Ran }
    }

    /// The ready driver's: the stage's entries, and every other name as its standing row.
    pub(crate) fn stood(stage: &'h NamesStage<'n>, stood: Stood<'h>) -> NamesInputs<'h, 'n> {
        NamesInputs { stage, from: NamesFrom::Stood(stood) }
    }

    /// `name`'s entry this command (`Some(None)`: entered and failed), or `None`: not entered.
    fn entered(&self, name: Name) -> Option<Option<Input>> {
        self.keys().entered(name)
    }

    /// `name`'s standing row, read by need ([`Stood::input`]); `Err(false)` on the phase path.
    fn standing(&self, name: Name) -> Result<Input, bool> {
        self.keys().standing(name)
    }

    /// This seam without the stage: what a `Sync` reader holds ([`NamesKeys`]).
    pub(crate) fn keys(&self) -> NamesKeys<'h> {
        let stood = match self.from {
            NamesFrom::Ran => None,
            NamesFrom::Stood(stood) => Some(stood),
        };
        NamesKeys { made: &self.stage.made, failed_out: self.stage.failed_out, stood }
    }

    /// `name`'s entry this command: `Some(Some(input))` entered, `Some(None)` entered and failed,
    /// `None` not entered (on the ready driver: its row is the stored answer's, unread).
    pub(crate) fn entry(&self, name: Name) -> Option<Option<Input>> {
        self.entered(name)
    }

    /// The stored answer this seam stands on, when it is the ready driver's.
    pub(crate) fn stood_on(&self) -> Option<Stood<'h>> {
        match self.from {
            NamesFrom::Ran => None,
            NamesFrom::Stood(stood) => Some(stood),
        }
    }

    /// `name`'s input this command: entered, or standing ([`NamesFrom`]).
    pub(crate) fn input_of(&self, name: Name) -> Option<Input> {
        match self.entered(name) {
            Some(entered) => entered,
            None => self.standing(name).ok(),
        }
    }

    /// Whether `name` has a names row this command that failed (entered and failed, or its
    /// standing row failed): what a body's refusal names.
    pub(crate) fn failed(&self, name: Name) -> bool {
        match self.entered(name) {
            Some(entered) => entered.is_none(),
            None => self.standing(name).err().unwrap_or(false),
        }
    }

    /// The stage itself for its world and plan (never an input: those are this seam's), for the
    /// seam's whole borrow.
    pub(crate) fn stage(self) -> &'h NamesStage<'n> {
        self.stage
    }
}

impl<'n> core::ops::Deref for NamesInputs<'_, 'n> {
    type Target = NamesStage<'n>;
    fn deref(&self) -> &NamesStage<'n> {
        self.stage
    }
}

/// The names stage's route to its dependencies' exports sections (names/scope.rs
/// [`crate::names::DepExports`]): the request's one reader of records (engine_records.rs
/// `EngineRecords::exports`), by route [`InPlace`] in every command; the tests' control
/// ([`crate::engine_memory::with_copying_route`]) takes the counted copying route instead, so
/// the "copied" count of the in-place route is seen to be a count (rule 78).
struct ReadExports<'e, 'r> {
    read: &'e mut EngineRecords<'r>,
    route: crate::engine_memory::ExportsRoute,
}

impl<'e, 'r> ReadExports<'e, 'r> {
    fn exports_committed<'j>(&mut self, cx: &Cx, store: &mut dyn crate::store::Store<'r>, input: &Input, krate: &str, alloc: blaze_own::Alloc<'j>) -> Result<Option<&'j [u8]>, ReadError>
    where
        'r: 'j,
    {
        let read = match self.route {
            crate::engine_memory::ExportsRoute::InPlace => self.read.exports_committed::<InPlace>(cx, store, input, alloc)?,
            #[cfg(test)]
            crate::engine_memory::ExportsRoute::Copying => self.read.exports_committed::<crate::engine_records::Copying>(cx, store, input, alloc)?,
        };
        let Some(read) = read else { return Ok(None) };
        #[cfg(test)]
        crate::names::sections::watch(krate, read.payload);
        #[cfg(not(test))]
        let _ = krate;
        Ok(Some(read.exports))
    }

    fn exports_from_snapshot<'j>(&mut self, snap: &'j crate::store::StoreSnapshot, input: &Input, krate: &str, alloc: blaze_own::Alloc<'j>) -> Result<(&'j [u8], usize, usize), ReadError> {
        let (read, offset, len) = match self.route {
            crate::engine_memory::ExportsRoute::InPlace => self.read.exports_from_snapshot::<InPlace>(snap, input, alloc)?,
            #[cfg(test)]
            crate::engine_memory::ExportsRoute::Copying => self.read.exports_from_snapshot::<crate::engine_records::Copying>(snap, input, alloc)?,
        };
        #[cfg(test)]
        crate::names::sections::watch(krate, read.payload);
        #[cfg(not(test))]
        let _ = krate;
        Ok((read.exports, offset, len))
    }
}

impl<'j, 'r: 'j> crate::names::DepExports<'j, 'r> for ReadExports<'_, 'r> {
    fn exports(&mut self, cx: &Cx, store: &mut dyn crate::store::Store<'r>, input: &Input, krate: &str, _frame: &'j Frame, alloc: blaze_own::Alloc<'j>) -> Result<&'j [u8], ReadError> {
        let read = match self.route {
            crate::engine_memory::ExportsRoute::InPlace => self.read.exports::<InPlace>(cx, store, input, alloc)?,
            #[cfg(test)]
            crate::engine_memory::ExportsRoute::Copying => self.read.exports::<crate::engine_records::Copying>(cx, store, input, alloc)?,
        };
        #[cfg(test)]
        crate::names::sections::watch(krate, read.payload);
        #[cfg(not(test))]
        let _ = krate;
        Ok(read.exports)
    }
}

/// The domain of the [`CELLS`] mark's digest: the cell table's bytes.
const CELLS_DOMAIN: &str = "blaze v2 engine names cell table 1";
/// The domain of the [`EXPORTS`] cell's digest: the exports section's bytes.
const EXPORTS_DOMAIN: &str = "blaze-cli engine names exports 1";

fn exports_digest(bytes: &[u8]) -> ContentId {
    ContentId::of(EXPORTS_DOMAIN, bytes)
}

/// What the crate `name`'s `CrateScope` task defines (engine.md E4): its record's [`WHOLE`]
/// cell, its [`EXPORTS`] cell (what downstream `CrateScope` tasks read), and, for a crate whose
/// names read a file of the checkout (a `Surface` read: the crates an edit can move), the
/// [`CELLS`] mark and every row of its payload's cell table (names/sections.rs `cells`,
/// blaze-scope names_cells.rs `crate_table`) as its own cell with its digest. The engine then
/// moves only the cells whose digest changed (blaze-engine engine.rs `apply`), so a body that
/// read none of them is not entered, and a downstream crate's names are not entered when the
/// exports section is unchanged. Sorted by cell, each once (the engine's merge walk; rule 25).
fn cells_of<'a>(name: Name, record: &work::Record<SharedAlloc<'_>>, exports: &[u8], table: &[u8], alloc: SharedAlloc<'a>) -> Result<(SVec<'a, (Cell, ContentId)>, ContentId), EngineCheckError> {
    const WHAT: &str = "the cells a `CrateScope` task defines (engine_names.rs `cells_of`)";
    // Each list sized once, to the cells it will hold, before its first push: the engine keeps
    // it for the rest of the command, so it is one block of its own length (rule 67's floor),
    // never a vector grown by doubling.
    let sized = |cells: usize| SVec::try_with_capacity_in(cells, alloc).map_err(|_| EngineCheckError::Grow { what: WHAT, had: 0 });
    let exports_id = exports_digest(exports);
    if !record.reads.iter().any(|(read, _)| read.kind == Kind::Surface) {
        let mut defines: SVec<'a, (Cell, ContentId)> = sized(2)?;
        EngineCheckError::push(&mut defines, (WHOLE, record.out), WHAT)?;
        EngineCheckError::push(&mut defines, (EXPORTS, exports_id), WHAT)?;
        return Ok((defines, exports_id));
    }
    let row = blaze_scope::names_cells::TABLE_ROW;
    if table.len() % row != 0 {
        return Err(EngineCheckError::CellTable { task: name, what: "is not whole rows" });
    }
    // WHOLE, CELLS, EXPORTS, then one cell a row of the table.
    let mut defines: SVec<'a, (Cell, ContentId)> = sized(3 + table.len() / row)?;
    EngineCheckError::push(&mut defines, (WHOLE, record.out), WHAT)?;
    EngineCheckError::push(&mut defines, (CELLS, ContentId::of(CELLS_DOMAIN, table)), WHAT)?;
    EngineCheckError::push(&mut defines, (EXPORTS, exports_id), WHAT)?;
    let mut last: Option<[u8; 16]> = None;
    for bytes in table.chunks_exact(row) {
        let mut key = [0u8; 16];
        key.copy_from_slice(&bytes[..16]);
        let mut digest = [0u8; 16];
        digest.copy_from_slice(&bytes[16..]);
        // The table is sorted by key, each once (names_cells.rs `table`): a row out of order
        // would be a reader's binary search missing it, so it is refused here, not sorted.
        if last.is_some_and(|last| last >= key) {
            return Err(EngineCheckError::CellTable { task: name, what: "is not sorted by key, each once" });
        }
        last = Some(key);
        // The engine's own cells of a record are all but their first byte zero; a hashed key
        // equal to one would stand for two cells.
        if key[1..] == [0u8; 15] {
            return Err(EngineCheckError::CellTable { task: name, what: "holds a key equal to one of the engine's own cells" });
        }
        EngineCheckError::push(&mut defines, (Cell(key), blaze_scope::names_cells::expand_digest(digest)), WHAT)?;
    }
    defines.sort_unstable_by_key(|(cell, _)| *cell);
    Ok((defines, exports_id))
}

/// Worker-side once-verified exports slot for committed (or copying-route) bytes: allocated once
/// per crate in the coordinator's frame, verified lazily on the first dependent worker thread
/// via `OnceLock` (rule 74), checking only header + section bounds + crate name in O(1) so
/// untouched pages of `core`/`alloc`/`std` CAS records are never faulted on edits.
struct LazyExports<'j> {
    bytes: &'j [u8],
    krate: &'j str,
    verified: OnceLock<Result<Verified<'j>, blaze_scope::crate_scope::RecordRead>>,
    bitmap: ModuleBitmap<'j>,
    digest: OnceLock<ContentId>,
}

impl<'j> LazyExports<'j> {
    fn new(bytes: &'j [u8], krate: &'j str, early_digest: Option<ContentId>) -> Self {
        let digest = OnceLock::new();
        if let Some(id) = early_digest {
            let _ = digest.set(id);
        }
        Self { bytes, krate, verified: OnceLock::new(), bitmap: ModuleBitmap::for_bytes(bytes), digest }
    }

    fn verify(&'j self) -> Result<Verified<'j>, &'j blaze_scope::crate_scope::RecordRead> {
        self.verified
            .get_or_init(|| {
                record_checked_test(self.krate);
                Verified::check_header_with_bitmap(self.bytes, &self.bitmap)
            })
            .as_ref()
            .map(|proof| *proof)
    }

    fn digest(&self) -> ContentId {
        *self.digest.get_or_init(|| exports_digest(self.bytes))
    }
}

/// Worker-side once-verified exports slot for an uncommitted shared-arena record in a
/// `StoreSnapshot`: `CheckedHead::check_header` runs lazily once on the first dependent worker
/// thread and subsequent readers rebind in O(1) without pinning `Lent::kept_arenas`.
struct LazyUncommitted<'j> {
    offset: usize,
    len: usize,
    krate: &'j str,
    checked: OnceLock<Result<(CheckedHead, ContentId), blaze_scope::crate_scope::RecordRead>>,
    early_digest: Option<ContentId>,
}

impl<'j> LazyUncommitted<'j> {
    fn new(offset: usize, len: usize, krate: &'j str, early_digest: Option<ContentId>) -> Self {
        Self { offset, len, krate, checked: OnceLock::new(), early_digest }
    }

    fn verify(&self, slice: &[u8]) -> Result<(CheckedHead, ContentId), &blaze_scope::crate_scope::RecordRead> {
        self.checked
            .get_or_init(|| {
                record_checked_test(self.krate);
                let head = CheckedHead::check_header(slice)?;
                let digest = self.early_digest.unwrap_or_else(|| exports_digest(slice));
                Ok((head, digest))
            })
            .as_ref()
            .map(|pair| *pair)
    }

    fn digest_with(&self, snap: Option<&crate::store::StoreSnapshot>, key: Key) -> Option<ContentId> {
        if let Some(id) = self.early_digest {
            return Some(id);
        }
        if let Some(Ok((_, digest))) = self.checked.get() {
            return Some(*digest);
        }
        let record = snap?.get(key).ok().flatten()?;
        let end = self.offset.checked_add(self.len)?;
        let slice = record.get(self.offset..end)?;
        Some(exports_digest(slice))
    }
}

/// How a dependency's exports reach `Job::run`: verified lazily once on the first dependent
/// worker thread from a committed mapping ([`LazyExports`]) or short-lived uncommitted snapshot
/// ([`LazyUncommitted`]).
#[derive(Clone, Copy)]
enum ExportRef<'j> {
    Lazy(&'j LazyExports<'j>),
    LazyInSnapshot(&'j LazyUncommitted<'j>),
}

#[derive(Clone, Copy)]
enum UncommittedMapped<'w> {
    Lazy(&'w LazyUncommitted<'w>, bool),
    Unreadable(&'w str),
}

/// The mapped exports of a dependency, checked once ([`checked`] or [`checked_head`]), or its
/// failed out (the worker makes an error crate).
#[derive(Clone, Copy)]
struct Dependency<'j> {
    index: usize,
    input: Input,
    absent: bool,
    has_exports: bool,
    exports: Option<ExportRef<'j>>,
}

/// Scratch shared by successive producer walks. Only visited indices need clearing.
pub(crate) struct ProducerWalk<'j> {
    stack: Vec<usize, &'j Frame>,
    touched: Vec<usize, &'j Frame>,
    seen: Vec<bool, &'j Frame>,
    post: Vec<(usize, bool), &'j Frame>,
    ready: Vec<(u64, bool), &'j Frame>,
    generation: u64,
}

impl<'j> ProducerWalk<'j> {
    pub(crate) fn new(frame: &'j Frame) -> Self {
        Self { stack: Vec::new_in(frame), touched: Vec::new_in(frame), seen: Vec::new_in(frame),
            post: Vec::new_in(frame), ready: Vec::new_in(frame), generation: 1 }
    }

    /// A publication changed the engine's frontier.
    pub(crate) fn clear(&mut self) { self.generation += 1; }

    fn start(&mut self, at: usize, count: usize) {
        self.seen.resize(count, false);
        for index in self.touched.drain(..) {
            self.seen[index] = false;
        }
        self.stack.clear();
        self.stack.push(at);
    }

    fn pop(&mut self) -> Option<usize> {
        while let Some(index) = self.stack.pop() {
            if !core::mem::replace(&mut self.seen[index], true) {
                self.touched.push(index);
                return Some(index);
            }
        }
        None
    }
}

/// The immutable world columns a worker indexes. A mixed request keeps lending them
/// while the coordinator files new resolutions in `Names`; the slices contain only
/// descriptors, and the records and pack bytes stay in place.
#[derive(Clone, Copy)]
struct WorldColumns<'j> {
    crates: &'j [blaze_world::Crate<'j>],
    packs: crate::job_texts::SyncPacks<'j>,
    corrected: &'j [(u32, &'j str, &'j str)],
}

struct TimedName<'a> {
    answer: Result<WorkerAnswer<'a>, WorkerError>,
    /// When the coordinator admitted it ([`NamesQueue::submit`]): `start` minus this is the
    /// time it was queued for a worker slot (engine_times.rs `Times::queued`).
    submitted: u64,
    start: u64,
    end: u64,
    scratch: u64,
}

struct RunningName<'j, 'a> {
    task: EngineTask,
    done: &'j bstd::sync::Flag,
    waiter: &'j blaze_core::wait::Parker,
    clock: &'j bstd::clock::Clock,
    output: &'j blaze_core::lock::Lock<Option<TimedName<'a>>>,
    handle: bstd::task::Task<'j, ()>,
    #[cfg(test)]
    end_tick: Option<u64>,
}

struct WaitingName<'j, 'a> {
    task: EngineTask,
    /// Bottom-up critical-path priority from [`NamesDag`]: higher dispatches first.
    prio: u32,
    scope: &'j bstd::task::Scope<'j>,
    job: Job<'j, 'j>,
    script: Option<(Input, bool)>,
    blocked: Option<(Why, Option<blaze_own::String<SharedAlloc<'a>>>)>,
    root: &'j Dir<ReadWrite>,
    clock: &'j bstd::clock::Clock,
    board: &'j crate::engine_status::Board<'j>,
    outlined: &'j [Outlined<'j>],
    alloc: SharedAlloc<'a>,
    waiter: &'j blaze_core::wait::Parker,
    /// When it was admitted, ns since the clock's start ([`TimedName::submitted`]).
    submitted: u64,
}

impl PartialEq for WaitingName<'_, '_> {
    fn eq(&self, other: &Self) -> bool {
        self.prio == other.prio && self.job.at == other.job.at && self.task.planned.name == other.task.planned.name
    }
}

impl Eq for WaitingName<'_, '_> {}

impl PartialOrd for WaitingName<'_, '_> {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for WaitingName<'_, '_> {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        // `Heap4` is a min-heap: dispatch highest critical-path `prio` first, deterministic
        // tie-break on crate index `job.at` then task `Name` (rule 25).
        other
            .prio
            .cmp(&self.prio)
            .then_with(|| self.job.at.cmp(&other.job.at))
            .then_with(|| self.task.planned.name.cmp(&other.task.planned.name))
    }
}

impl<'j, 'a: 'j> WaitingName<'j, 'a> {
    fn start(self, frame: &'j Frame) -> RunningName<'j, 'a> {
        let Self { task, prio: _, scope, job, script, blocked, root, clock, board, outlined, alloc, waiter, submitted } = self;
        let name = task.planned.name;
        #[cfg(test)]
        let end_tick = crate::engine_scripts::runner_clock().map(|sim| sim.names_admitted(name, job.crates[job.at].input.name)).filter(|tick| *tick > 0);
        let done: &'j bstd::sync::Flag = frame.alloc(bstd::sync::Flag::new());
        let output: &'j blaze_core::lock::Lock<Option<TimedName<'a>>> = frame.alloc(blaze_core::lock::Lock::new("engine.names.output", None));
        let notice = crate::engine_scripts::Notice { finished: done, waiter };
        let label = crate::engine_status::Label::of(job.crates[job.at].input.name);
        let handle = scope.may_overlap("crate scope", crate::names::SCOPE_FRAME, move |cx| {
            let _notice = notice;
            let start = crate::engine_times::ns(clock.now().since_start());
            let shown = board.show(Stage::Names, label, 0, board.now(clock));
            let failure = |why, words| WorkerAnswer {
                done: crate::engine_tasks::failed_with(&task, failure_reads(job.deps, job.uncommitted_snap.as_deref(), script, alloc), alloc),
                result: Err((TaskFailure { name, why }, words)),
                scope_bytes: crate::names::metered::ScopeBytes::default(),
            };
            let answer = match work::unwinding(|| match blocked {
                None => job.run(cx, root, &task, outlined, alloc),
                Some((why, words)) => Ok(failure(why, words)),
            }) {
                Ok(answer) => answer,
                Err(work::Panicked) => Ok(failure(Why::Panicked { used: cx.frame().committed() as u64, limit: crate::names::SCOPE_FRAME as u64 }, None)),
            };
            shown.end(board.now(clock));
            *output.lock() = Some(TimedName { answer, submitted, start, end: crate::engine_times::ns(clock.now().since_start()), scratch: cx.frame().committed() as u64 });
            Ok::<(), Infallible>(())
        });
        RunningName { task, done, waiter, clock, output, handle, #[cfg(test)] end_tick }
    }
}

/// Jobs running beside Scripts. Nothing in a worker borrows the coordinator's mutable
/// names table, the store handle, or the engine's set of entered tasks.
pub(crate) struct NamesQueue<'j, 'a> {
    frame: &'j Frame,
    bump: blaze_own::Bump<'j>,
    local: blaze_own::Alloc<'j>,
    slots: &'j crate::engine_scripts::PoolSlots,
    columns: Option<WorldColumns<'j>>,
    walk: ProducerWalk<'j>,
    mapped: Vec<Option<(Input, Mapped<'j>)>, &'j Frame>,
    /// Short-lived snapshot of the current uncommitted shared arena, keyed by `store.commits()`:
    /// dropped as soon as a commit advances so `Arc<Arena>` frees without pinning `kept_arenas`.
    uncommitted_snap: Option<(u32, crate::store::SharedSnapshot)>,
    uncommitted_mapped: Vec<Option<(Input, UncommittedMapped<'j>)>, &'j Frame>,
    /// Exports digests already computed on the producing crate's worker (`finish`), indexed by
    /// crate index `at`, so downstream crates skip re-hashing `exports_digest`.
    early_digests: Vec<Option<(Key, ContentId)>, &'j Frame>,
    /// Stage-scoped compiled declarative macro cache shared across all `Names` jobs in this run.
    shared_prepares: Option<crate::names::SharedPrepares>,
    width: usize,
    order: BTreeMap<(bool, core::cmp::Reverse<u32>, u16, Name), EngineTask, &'j Frame>,
    waiting: Heap4<WaitingName<'j, 'a>, &'j Frame>,
    running: BTreeMap<Name, RunningName<'j, 'a>, &'j Frame>,
    ended: Vec<bstd::task::Task<'j, ()>, &'j Frame>,
    answered: BTreeMap<Name, (EngineTask, TimedName<'a>), &'j Frame>,
    scratch: BTreeMap<u16, Vec<(u64, Name), &'j Frame>, &'j Frame>,
    /// Each admitted task's producers, (the task, a producer), handed to the times as tasks
    /// are filed ([`NamesQueue::finish`]).
    edges: Vec<(Name, Name), &'j Frame>,
}

impl<'j, 'a: 'j> NamesQueue<'j, 'a> {
    pub(crate) fn new(frame: &'j Frame, local: blaze_own::Alloc<'j>, slots: &'j crate::engine_scripts::PoolSlots) -> Self {
        let bump = frame.alloc(blaze_own::Erased::new(frame)).bump();
        Self { frame, bump, local, slots, columns: None, walk: ProducerWalk::new(frame), mapped: Vec::new_in(frame),
            uncommitted_snap: None, uncommitted_mapped: Vec::new_in(frame), early_digests: Vec::new_in(frame), shared_prepares: None,
            width: 1, order: BTreeMap::new_in(frame), waiting: Heap4::new_in(frame), running: BTreeMap::new_in(frame), answered: BTreeMap::new_in(frame),
            scratch: BTreeMap::new_in(frame), ended: Vec::new_in(frame), edges: Vec::new_in(frame) }
    }

    pub(crate) fn is_empty(&self) -> bool { self.order.is_empty() }

    pub(crate) fn has_completion(&self) -> bool {
        self.running.values().any(|r| r.done.is_set())
    }

    /// Filing priority is computed from the plan alone. File a non-leaf to expose
    /// downstream work before waiting on a leaf.
    pub(crate) fn first(&self) -> Option<EngineTask> {
        self.order.first_key_value().map(|(_, task)| *task)
    }

    /// Highest-priority ALREADY-FINISHED `Names` task (non-leaf highest critical-path priority,
    /// lowest wave, then Name), eliminating wave-wide and hash-order head-of-line blocking.
    pub(crate) fn first_ready(&self) -> Option<EngineTask> {
        self.order
            .iter()
            .find(|((_, _, _, name), _)| self.answered.contains_key(name))
            .map(|(_, task)| *task)
    }

    /// Next ALREADY-FINISHED `Names` task in `phase` (if `feeds_only`, restricted to non-leaf
    /// tasks so leaf crates do not delay admitting newly unblocked downstream waves).
    pub(crate) fn next_ready_in_phase(&self, phase: blaze_engine::Phase, feeds_only: bool) -> Option<EngineTask> {
        self.order
            .iter()
            .find(|((is_leaf, _, _, name), task)| {
                task.planned.phase == phase && (!feeds_only || !*is_leaf) && self.answered.contains_key(name)
            })
            .map(|(_, task)| *task)
    }

    #[cfg(test)]
    fn next_virtual(&self) -> Option<(u64, Name)> {
        self.running.values().filter_map(|r| r.end_tick.map(|tick| (tick, r.task.planned.name))).min()
    }

    #[cfg(test)]
    pub(crate) fn next_tick(&self) -> Option<u64> {
        self.next_virtual().map(|(tick, _)| tick)
    }

    pub(crate) fn dispatch(&mut self) {
        while self.running.len() < self.width && self.slots.free() && let Some(waiting) = self.waiting.pop() {
            self.slots.take();
            let name = waiting.task.planned.name;
            self.running.insert(name, waiting.start(self.frame));
        }
    }

    /// Return a worker slot without publishing. Either stage can take the slot when
    /// the coordinator next dispatches; `first` decides publication.
    pub(crate) fn observe_next(&mut self, cx: &Cx) -> Result<(), CliError> {
        let next = || -> Result<Name, CliError> {
            let run = self.running.values().next().ok_or(CliError::Text { what: "the Names completion queue has no crate worker (engine_names.rs)" })?;
            crate::engine_scripts::wait_finished(cx, run.clock, run.waiter, || self.running.values().any(|r| r.done.is_set()), "a ready crate scope to finish", self.running.len())?;
            Ok(*self.running.iter().find(|(_, r)| r.done.is_set()).expect("a finished crate remains ready").0)
        };
        #[cfg(test)]
        let name = if let Some((tick, name)) = self.next_virtual() {
            crate::engine_scripts::runner_clock().expect("a Names task with a simulated end has a clock").names_finished(name, tick);
            name
        } else { next()? };
        #[cfg(not(test))]
        let name = next()?;
        let run = self.running.get(&name).ok_or(EngineCheckError::MissingEntry { task: name, table: Table::NamesRunning, key: name })?;
        crate::engine_scripts::wait_finished(cx, run.clock, run.waiter, || run.done.is_set(), "the selected Names output", self.running.len())?;
        let run = self.running.remove(&name).expect("the selected Names output");
        // A bstd join may run unrelated jobs while the signalled closure is
        // returning. Take its published result now, join after both stages end.
        self.ended.push(run.handle);
        self.slots.release();
        let timed = run.output.lock().take().ok_or(CliError::Text { what: "a Names worker ended before publishing its output (engine_names.rs)" })?;
        self.answered.insert(name, (run.task, timed));
        Ok(())
    }

    pub(crate) fn is_finished(&self, name: Name) -> bool {
        self.answered.contains_key(&name)
    }

    pub(crate) fn set_width(&mut self, scope: &bstd::task::Scope<'_>, handles: &Handles<'_, '_>, _runnable_scripts: usize) -> core::num::NonZeroU32 {
        let width = handles.workers.get().min(scope.workers());
        #[cfg(test)]
        let width = if crate::engine_scripts::runner_clock().is_some() { handles.workers.get() } else { width };
        let workers = core::num::NonZeroU32::new(width).unwrap_or(core::num::NonZeroU32::MIN);
        // Scripts always dispatch first in `Pipeline::admit` and `Pipeline::collect`, taking every
        // pool slot runnable scripts can actually use; `Names` fills every remaining free slot up
        // to the full pool `width` without an artificial `width / 2` throttle.
        self.width = usize::try_from(workers.get()).unwrap_or(1);
        workers
    }

    /// Snapshot the inputs the selected task can read before admitting it. The worker
    /// then needs neither a producer wait nor access to a changing input table.
    #[expect(clippy::too_many_arguments, reason = "one worker scope and the request's store, names, and timing handles")]
    pub(crate) fn submit<'r: 'j, 'f: 'j>(
        &mut self, scope: &'j bstd::task::Scope<'j>, cx: &Cx, stage: &mut NamesStage<'f>,
        store: &mut dyn crate::store::Store<'r>, read: &mut EngineRecords<'r>, view: blaze_engine::View<'_, '_>,
        handles: &Handles<'j, 'f>, task: EngineTask, alloc: SharedAlloc<'a>, frame: &'f Frame,
        waiter: &'j blaze_core::wait::Parker, runnable_scripts: usize,
    ) -> Result<(), CliError> {
        let name = task.planned.name;
        stage.admissions.admit(name)?;
        let feeds_names = stage.feeds_names(name);
        let prio = stage.prio(name);
        let names = stage.names.as_ref().ok_or(EngineCheckError::MissingEntry { task: name, table: Table::NamesWorld, key: name })?;
        // A names task runs: the world is made now if no task made it yet (as `run_phase`).
        let made = match names.made_or_make(cx, handles.root, store, handles.env, crate::names::NeededBy::NamesTask)? {
            Ok(made) => made,
            Err(why) => return Err(EngineCheckError::NamesWorld(why).into()),
        };
        let runner = NamesStage::runner(names, made, handles, frame);
        let world = made.world();
        let at = names.crate_at(name.subject).ok_or(EngineCheckError::MissingEntry { task: name, table: Table::WorldCrates, key: name })?;
        let columns = *self.columns.get_or_insert_with(|| WorldColumns {
            crates: self.frame.alloc_slice_copy(&world.crates),
            packs: match made.lazy_trees() {
                Some(lazy) => crate::job_texts::SyncPacks::SyncLazy(lazy, &handles.env.fs),
                None => crate::job_texts::SyncPacks::Open(self.frame.alloc_slice_copy(world.texts.packs())),
            },
            corrected: self.frame.alloc_slice_copy(world.texts.corrected()),
        });
        let krate = columns.crates[at].input.name;
        #[cfg(test)]
        if let Some(clock) = crate::engine_scripts::runner_clock() { clock.names_entered(name, krate); }
        let script = names.script_of(at).map(|script| lookup(&stage.made, name, script, stage.failed_out)).transpose()?;
        let macro_module = {
            use blaze_world::crate_list::CrateGraph as _;
            let list = names.list();
            if list.proc_macro(at) {
                let mm = Name::new(Kind::MacroModule, crate::macros::subject_of(list.package(at), list.version(at)));
                stage.made.binary_search_by_key(&mm, |(n, _)| *n).ok().and_then(|idx| stage.made[idx].1).map(|(key, out)| Input { name: mm, at: key, out })
            } else {
                None
            }
        };
        let refused = |what| Why::Refused { what, rule: crate::names::RULE };
        let mut gate = match (runner, world.refusal(at), script) {
            (Err(words), _, _) => Err((refused("the proc-macro runner could not be made"), Some(blaze_own::String::from_str_in(words, alloc)))),
            (_, Some(reason), _) => Err((refused("a crate the toolchain shelf cannot build"), Some(blaze_own::format_in!(alloc, "crate `{krate}`: {reason}")))),
            (_, _, Some((input, true))) => Err((Why::Waited { on: input.name }, None)),
            (Ok(procs), None, _) => Ok(procs),
        };
        let script_bytes = match (gate.is_ok(), script) {
            (true, Some((input, _))) => Some((input, self.frame.alloc(work::payload(cx, store, &input, self.frame)?).payload())),
            _ => None,
        };
        let all = world.sysroot;
        // Its producers for the stage's critical path (engine_times.rs `Times::names_path`):
        // each direct dependency's Names task and its crate's Scripts task.
        for g in edges_of(&columns.crates[at], all, at) {
            self.edges.push((name, stage.crate_names[g]));
        }
        if let Some((input, _)) = script {
            self.edges.push((name, input.name));
        }
        let commits = store.commits();
        if self.uncommitted_snap.as_ref().is_some_and(|(was, _)| *was != commits) {
            self.uncommitted_snap = None;
            self.uncommitted_mapped.clear();
        }
        self.mapped.resize(columns.crates.len(), None);
        self.uncommitted_mapped.resize(columns.crates.len(), None);
        self.early_digests.resize(columns.crates.len(), None);
        self.walk.start(at, columns.crates.len());
        let mut deps = Vec::new_in(self.frame);
        let route = crate::engine_memory::route_now();
        let mut exports = ReadExports { read, route };
        let mut unreadable: Option<(usize, &str)> = None;
        let mut needs_uncommitted_snap = false;
        while let Some(g) = self.walk.pop() {
            self.walk.stack.extend(edges_of(&columns.crates[g], all, g));
            if g == at { continue; }
            let (input, absent) = lookup(&stage.made, name, stage.crate_names[g], stage.failed_out)?;
            let mut dep = Dependency { index: g, input, absent, has_exports: false, exports: None };
            if gate.is_ok() {
                let krate_g = columns.crates.get(g).map_or("", |c| c.input.name);
                let early_digest = self
                    .early_digests
                    .get(g)
                    .copied()
                    .flatten()
                    .and_then(|(key, id)| (key == input.at).then_some(id));
                if let Some((previous, mapped)) = self.mapped.get(g).copied().flatten()
                    && previous == input
                {
                    match mapped {
                        Mapped::Lazy(lazy, has_exports) => {
                            dep.exports = Some(ExportRef::Lazy(lazy));
                            dep.has_exports = has_exports;
                        }
                        Mapped::Failed => {}
                        Mapped::Unreadable(why) => {
                            if unreadable.is_none_or(|(first, _)| g < first) {
                                unreadable = Some((g, why));
                            }
                        }
                    }
                } else if let Some((previous, mapped)) = self.uncommitted_mapped.get(g).copied().flatten()
                    && previous == input
                {
                    match mapped {
                        UncommittedMapped::Lazy(lazy, has_exports) => {
                            dep.exports = Some(ExportRef::LazyInSnapshot(lazy));
                            dep.has_exports = has_exports;
                            needs_uncommitted_snap = true;
                        }
                        UncommittedMapped::Unreadable(why) => {
                            if unreadable.is_none_or(|(first, _)| g < first) {
                                unreadable = Some((g, why));
                            }
                        }
                    }
                } else if input.out == stage.failed_out {
                    if let Some(slot) = self.mapped.get_mut(g) {
                        *slot = Some((input, Mapped::Failed));
                    }
                } else {
                    match exports.exports_committed(cx, store, &input, krate_g, self.local) {
                        Ok(Some(bytes)) => {
                            let cell_digest = blaze_engine::CellDigests::cell(&view, &CellId { of: input.name, cell: EXPORTS });
                            let has_exports = cell_digest != blaze_drive::key::ABSENT;
                            let known_digest = early_digest.or_else(|| has_exports.then_some(cell_digest));
                            let lazy = self.frame.alloc(LazyExports::new(bytes, krate_g, known_digest));
                            if let Some(slot) = self.mapped.get_mut(g) {
                                *slot = Some((input, Mapped::Lazy(lazy, has_exports)));
                            }
                            dep.exports = Some(ExportRef::Lazy(lazy));
                            dep.has_exports = has_exports;
                        }
                        Ok(None) => {
                            // Uncommitted shared-arena record: read via a short-lived snapshot so
                            // `Lent::shared_lent` stays false and the arena frees on commit.
                            let use_snapshot = matches!(route, crate::engine_memory::ExportsRoute::InPlace);
                            if use_snapshot {
                                let has_key = match &self.uncommitted_snap {
                                    Some((_, snap)) => snap.get(input.at).ok().flatten().is_some(),
                                    None => false,
                                };
                                if !has_key {
                                    self.uncommitted_snap = store.snapshot()?.map(|snap| (commits, snap));
                                }
                            }
                            if use_snapshot && let Some((_, snap)) = self.uncommitted_snap.as_ref() {
                                match exports.exports_from_snapshot(snap.as_ref(), &input, krate_g, self.local) {
                                    Ok((_bytes, offset, len)) => {
                                        let cell_digest = blaze_engine::CellDigests::cell(&view, &CellId { of: input.name, cell: EXPORTS });
                                        let has_exports = cell_digest != blaze_drive::key::ABSENT;
                                        let known_digest = early_digest.or_else(|| has_exports.then_some(cell_digest));
                                        let lazy = self.frame.alloc(LazyUncommitted::new(offset, len, krate_g, known_digest));
                                        if let Some(slot) = self.uncommitted_mapped.get_mut(g) {
                                            *slot = Some((input, UncommittedMapped::Lazy(lazy, has_exports)));
                                        }
                                        dep.exports = Some(ExportRef::LazyInSnapshot(lazy));
                                        dep.has_exports = has_exports;
                                        needs_uncommitted_snap = true;
                                    }
                                    Err(ReadError::Load(crate::front_load::LoadError::Store(error))) => return Err(error),
                                    Err(error) => {
                                        let why = blaze_own::format_in!(self.frame, "{error}").into_bump_str();
                                        if let Some(slot) = self.uncommitted_mapped.get_mut(g) {
                                            *slot = Some((input, UncommittedMapped::Unreadable(why)));
                                        }
                                        if unreadable.is_none_or(|(first, _)| g < first) {
                                            unreadable = Some((g, why));
                                        }
                                    }
                                }
                            } else {
                                // `Copying` test control (or non-snapshot test store).
                                match exports.exports(cx, store, &input, krate_g, self.frame, self.local) {
                                    Ok(bytes) => {
                                        let cell_digest = blaze_engine::CellDigests::cell(&view, &CellId { of: input.name, cell: EXPORTS });
                                        let has_exports = cell_digest != blaze_drive::key::ABSENT;
                                        let known_digest = early_digest.or_else(|| has_exports.then_some(cell_digest));
                                        let lazy = self.frame.alloc(LazyExports::new(bytes, krate_g, known_digest));
                                        if let Some(slot) = self.mapped.get_mut(g) {
                                            *slot = Some((input, Mapped::Lazy(lazy, has_exports)));
                                        }
                                        dep.exports = Some(ExportRef::Lazy(lazy));
                                        dep.has_exports = has_exports;
                                    }
                                    Err(ReadError::Load(crate::front_load::LoadError::Store(error))) => return Err(error),
                                    Err(error) => {
                                        let why = blaze_own::format_in!(self.frame, "{error}").into_bump_str();
                                        if let Some(slot) = self.mapped.get_mut(g) {
                                            *slot = Some((input, Mapped::Unreadable(why)));
                                        }
                                        if unreadable.is_none_or(|(first, _)| g < first) {
                                            unreadable = Some((g, why));
                                        }
                                    }
                                }
                            }
                        }
                        Err(ReadError::Load(crate::front_load::LoadError::Store(error))) => return Err(error),
                        Err(error) => {
                            let why = blaze_own::format_in!(self.frame, "{error}").into_bump_str();
                            if let Some(slot) = self.mapped.get_mut(g) {
                                *slot = Some((input, Mapped::Unreadable(why)));
                            }
                            if unreadable.is_none_or(|(first, _)| g < first) {
                                unreadable = Some((g, why));
                            }
                        }
                    }
                }
            }
            deps.push(dep);
        }
        deps.sort_unstable_by_key(|dep| dep.index);
        let deps: &'j [Dependency<'j>] = deps.into_bump_slice();
        if let Some((g, why)) = unreadable {
            let words = blaze_own::format_in!(alloc, "crate `{krate}`: the record of its dependency `{}`: {why}", columns.crates[g].input.name);
            gate = Err((refused("a dependency's record"), Some(words)));
        }
        let env = if gate.is_ok() && let Some(source) = world.texts.crate_env(at) {
            match EnvLent::new(source).copy_in(self.bump) {
                Ok(copy) => Some(EnvLent::new(self.frame.alloc(copy))),
                Err(error) => {
                    gate = Err((refused("a crate's environment"), Some(blaze_own::format_in!(alloc, "crate `{krate}`: its environment's build-script record did not open again: {error:?}"))));
                    None
                }
            }
        } else { None };
        let workers = self.set_width(scope, handles, runnable_scripts);
        let uncommitted_snap = if gate.is_ok() && needs_uncommitted_snap {
            self.uncommitted_snap.as_ref().map(|(_, snap)| Share::share(snap))
        } else {
            None
        };
        if self.shared_prepares.is_none() {
            self.shared_prepares = crate::names::SharedPrepares::new(handles.env.heap.shared()).ok();
        }
        let job = Job { at, crates: columns.crates, all, deps, uncommitted_snap, script: script_bytes, macro_module, home: handles.env.home, env, packs: columns.packs,
            corrected: columns.corrected, procs: gate.as_ref().ok().copied().flatten(), kept: handles.env.heap.shared(),
            shared_prepares: self.shared_prepares.as_ref().map(crate::names::SharedPrepares::share),
            workers, oracle: handles.oracle, clock: &handles.env.clock, board: handles.board, serve: handles.serve() };
        self.order.insert((!feeds_names, core::cmp::Reverse(prio), task.planned.phase.wave, name), task);
        self.waiting.push(WaitingName { task, prio, scope, job, script, blocked: gate.err(), root: handles.root, clock: &handles.env.clock,
            board: handles.board, outlined: handles.outlined, alloc, waiter, submitted: crate::engine_times::ns(handles.env.clock.now().since_start()) });
        Ok(())
    }

    pub(crate) fn finish<'f>(
        &mut self, _cx: &Cx, name: Name, stage: &mut NamesStage<'f>, buffer: &mut Buffer<'a>,
        times: &mut crate::engine_times::Times, frame: &'f Frame,
    ) -> Result<(), CliError> {
        let (task, timed) = self.answered.remove(&name).ok_or(EngineCheckError::MissingEntry { task: name, table: Table::NamesAnswered, key: name })?;
        let key = (!stage.feeds_names(name), core::cmp::Reverse(stage.prio(name)), task.planned.phase.wave, name);
        self.order.remove(&key).ok_or(EngineCheckError::MissingEntry { task: name, table: Table::NamesOrder, key: name })?;
        times.interval(name, timed.start, timed.end);
        times.queued(name, timed.submitted, timed.start);
        times.depends(&self.edges);
        self.edges.clear();
        times.names_width(u32::try_from(self.width).unwrap_or(u32::MAX));
        let answer = timed.answer?.into_completion(frame)?;
        if let Ok(finished) = &answer.result
            && let Some(at) = stage.names.as_ref().and_then(|names| names.crate_at(name.subject))
        {
            if self.early_digests.len() <= at {
                self.early_digests.resize(at.saturating_add(1), None);
            }
            if let Some(slot) = self.early_digests.get_mut(at) {
                *slot = Some((finished.key, finished.exports_id));
            }
        }
        stage.memory.scope_bytes(answer.scope_bytes);
        stage.file(&task, answer, buffer)?;
        self.scratch.entry(task.planned.phase.wave).or_insert_with(|| Vec::new_in(self.frame)).push((timed.scratch, name));
        if self.order.is_empty() {
            while let Some((wave, mut scratch)) = self.scratch.pop_first() {
                stage.memory.wave(wave, self.frame.committed() as u64, &mut scratch, self.width as u32);
            }
        }
        Ok(())
    }

    pub(crate) fn wait_running(&self, cx: &Cx) -> Result<(), CliError> {
        if let Some(run) = self.running.values().next() {
            crate::engine_scripts::wait_finished(cx, run.clock, run.waiter, || self.running.values().all(|r| r.done.is_set()), "the submitted Names workers to end", self.running.len())?;
        }
        Ok(())
    }

    /// Close every linear handle after both stages have published their final
    /// output slots. An error's unfiled worker output owns allocations too.
    pub(crate) fn drain(&mut self, cx: &Cx) -> Result<(), CliError> {
        let mut error = None;
        while let Some((_, run)) = self.running.pop_first() {
            if let Err(why) = run.handle.join(cx) { error.get_or_insert(CliError::Pool(why)); }
            drop(run.output.lock().take());
            self.slots.release();
        }
        while let Some(handle) = self.ended.pop() {
            if let Err(why) = handle.join(cx) { error.get_or_insert(CliError::Pool(why)); }
        }
        self.uncommitted_snap = None;
        self.uncommitted_mapped.clear();
        error.map_or(Ok(()), Err)
    }
}

enum Blocked<'j> {
    Runner(&'j str),
    Shelf(&'j blaze_scope::units::UnitsError<'j>),
    Script(Name),
}

/// The input `name` (a dependency's names or the crate's script) as made so far. A name not in
/// `made` is a NEED, not a table's slip: `task` reads an input its plan did not hand it (a
/// producer not yet ended, the planner's bug), so it is [`EngineCheckError::Unseen`] naming that
/// read (engine_script_reads_e2e_tests.rs `control_early_names_admission_reads_a_missing_script_cfg`).
fn lookup(made: &[(Name, Option<(Key, ContentId)>)], task: Name, name: Name, failed: ContentId) -> Result<(Input, bool), EngineCheckError> {
    let i = made.binary_search_by_key(&name, |row| row.0).map_err(|_| EngineCheckError::Unseen { task, read: name })?;
    let value = made[i].1;
    let (at, out) = value.unwrap_or((Key::default(), failed));
    Ok((Input { name, at, out }, value.is_none()))
}

fn input_cell(input: Input, absent: bool) -> (CellId, ContentId) {
    (CellId { of: input.name, cell: WHOLE }, if absent { blaze_drive::key::ABSENT } else { input.out })
}

fn dep_input_cell(dep: &Dependency<'_>, snap: Option<&crate::store::StoreSnapshot>) -> (CellId, ContentId) {
    match (dep.absent, dep.has_exports, dep.exports) {
        (false, true, Some(ExportRef::Lazy(lazy))) => (CellId { of: dep.input.name, cell: EXPORTS }, lazy.digest()),
        (false, true, Some(ExportRef::LazyInSnapshot(lazy))) => match lazy.digest_with(snap, dep.input.at) {
            Some(digest) => (CellId { of: dep.input.name, cell: EXPORTS }, digest),
            None => input_cell(dep.input, dep.absent),
        },
        _ => input_cell(dep.input, dep.absent),
    }
}

fn failure_reads<'a>(
    deps: &[Dependency<'_>],
    snap: Option<&crate::store::StoreSnapshot>,
    script: Option<(Input, bool)>,
    alloc: SharedAlloc<'a>,
) -> SVec<'a, (CellId, ContentId)> {
    let mut reads = SVec::with_capacity_in(deps.len() + usize::from(script.is_some()), alloc);
    reads.extend(deps.iter().map(|dep| dep_input_cell(dep, snap)));
    reads.extend(script.map(|(input, absent)| input_cell(input, absent)));
    reads
}

fn failure_reads_verified<'a>(
    deps: &[Dependency<'_>],
    verified: &[Option<Verified<'_>>],
    snap: Option<&crate::store::StoreSnapshot>,
    script: Option<(Input, bool)>,
    alloc: SharedAlloc<'a>,
) -> SVec<'a, (CellId, ContentId)> {
    let mut reads = SVec::with_capacity_in(deps.len() + usize::from(script.is_some()), alloc);
    for (dep, checked) in deps.iter().zip(verified.iter()) {
        if checked.is_some() {
            reads.push(dep_input_cell(dep, snap));
        } else {
            reads.push(input_cell(dep.input, dep.absent));
        }
    }
    reads.extend(script.map(|(input, absent)| input_cell(input, absent)));
    reads
}

struct Job<'j, 'd> {
    at: usize,
    crates: &'j [blaze_world::Crate<'j>],
    all: Sysroot,
    deps: &'d [Dependency<'j>],
    /// Short-lived snapshot keeping any `ExportRef::InSnapshot` arena alive for this job alone.
    uncommitted_snap: Option<crate::store::SharedSnapshot>,
    script: Option<(Input, &'j [u8])>,
    /// For a proc-macro crate, its settled `Kind::MacroModule` task so any change to its compiled
    /// module (including edits to its body or path dependencies) wakes this `CrateScope` and
    /// refreshes the `proc_macro_key` embedded in its `items`/`exports` records (Rule 21).
    macro_module: Option<Input>,
    home: Option<&'j str>,
    env: Option<EnvLent<'j, 'j>>,
    packs: crate::job_texts::SyncPacks<'j>,
    corrected: &'j [(u32, &'j str, &'j str)],
    procs: Option<&'j crate::proc_runner::ProcRunner<'static>>,
    kept: SharedAlloc<'static>,
    shared_prepares: Option<crate::names::SharedPrepares>,
    workers: core::num::NonZeroU32,
    /// `--names-oracle` ([`NamesOracle`]) and the clock its check is timed by (rule 75).
    oracle: NamesOracle,
    clock: &'j bstd::clock::Clock,
    board: &'j crate::engine_status::Board<'j>,
    /// What it serves the files its crate finds by ([`Serve`]).
    serve: Serve<'j>,
}

struct WorkerAnswer<'a> {
    done: Done<'a>,
    result: Result<Finished<work::Record<SharedAlloc<'static>>>, (TaskFailure, Option<blaze_own::String<SharedAlloc<'a>>>)>,
    scope_bytes: crate::names::metered::ScopeBytes,
}

struct Finished<R> {
    record: R,
    resolved: Resolved,
    ok: bool,
    unresolved: u32,
    key: Key,
    exports_id: ContentId,
    head: SVec<'static, u8>,
    /// The crate's oracle comparison, under `--names-oracle` when its check reported.
    oracle: Option<OracleCrate>,
    /// What its names said of the files found (D161).
    found: FoundBy,
}

struct Completion<'a, 'f> {
    done: Done<'a>,
    result: Result<Finished<work::Made<'f>>, Failure<'f>>,
    scope_bytes: crate::names::metered::ScopeBytes,
}

impl<'a> WorkerAnswer<'a> {
    fn into_completion<'f>(self, frame: &'f Frame) -> Result<Completion<'a, 'f>, CliError> {
        let result = match self.result {
            Ok(Finished { record: header, mut resolved, ok, unresolved, key, exports_id, head, oracle, found }) => {
                // Only the small record columns cross into the coordinator's frame. The
                // payload and expansion buffers keep their worker allocation through the put.
                let mut made = header.clone_in(frame).map_err(|_| EngineCheckError::Grow { what: work::HELD_REFUSED, had: header.reads.len() })?;
                crate::names::hand_payload(&mut resolved, &mut made)?;
                Ok(Finished { record: made, resolved, ok, unresolved, key, exports_id, head, oracle, found })
            }
            Err((failure, words)) => Err(Failure { failure, words: words.map(|s| work::held_str(s.as_str(), frame)) }),
        };
        Ok(Completion { done: self.done, result, scope_bytes: self.scope_bytes })
    }
}

enum WorkerError {
    Engine(EngineCheckError),
    Finding(&'static str),
    /// A file the crate's names found could not be read to be outlined ([`Job::run`]): the
    /// request stops on it, as the walk's plan does on a file it reaches (engine_check.rs `plan`).
    Unread(crate::work::Unread),
}

impl From<EngineCheckError> for WorkerError {
    fn from(error: EngineCheckError) -> Self { Self::Engine(error) }
}

impl From<WorkerError> for CliError {
    fn from(error: WorkerError) -> Self {
        match error {
            WorkerError::Engine(error) => error.into(),
            WorkerError::Finding(what) => CliError::Text { what },
            WorkerError::Unread(error) => error.into(),
        }
    }
}

impl Job<'_, '_> {
    fn run<'a>(&self, cx: &Cx, root: &Dir<ReadWrite>, task: &EngineTask, outlined: &[Outlined<'_>], alloc: SharedAlloc<'a>) -> Result<WorkerAnswer<'a>, WorkerError> {
        let scratch = blaze_own::Erased::new(cx.frame());
        let bump = scratch.bump();
        let local = bump.alloc();
        let krate = self.crates[self.at];
        let name = task.planned.name;
        let script_input = self.script.map(|(input, _)| input);
        let snap_ref = self.uncommitted_snap.as_deref();
        // Pre-build branded per-module verification bitmaps for any uncommitted snapshot
        // dependencies so their immutable borrow outlives `resolve_with_prepares` (0 alloc when
        // all deps are committed, e.g. on every edit).
        let uncommitted_count = if snap_ref.is_some() {
            self.deps.iter().filter(|d| matches!(d.exports, Some(ExportRef::LazyInSnapshot(_)))).count()
        } else {
            0
        };
        let mut uncommitted_bitmaps: Vec<(usize, ModuleBitmap<'_>), _> =
            Vec::with_capacity_in(uncommitted_count, alloc);
        if let Some(snap) = snap_ref {
            for (idx, dep) in self.deps.iter().enumerate() {
                if let Some(ExportRef::LazyInSnapshot(lazy)) = dep.exports
                    && let Ok(Some(record)) = snap.get(dep.input.at)
                    && let Some(slice) = lazy.offset.checked_add(lazy.len).and_then(|end| record.get(lazy.offset..end))
                {
                    uncommitted_bitmaps.push((idx, ModuleBitmap::for_bytes(slice)));
                }
            }
        }
        // Verify dependency exports lazily on worker threads (once per crate via `OnceLock`,
        // rule 74), in sorted `dep.index` order so any unreadable dependency stops deterministically.
        let mut verified_deps: Vec<Option<Verified<'_>>, _> = Vec::with_capacity_in(self.deps.len(), local);
        let mut first_bad: Option<blaze_own::String<SharedAlloc<'a>>> = None;
        for (idx, dep) in self.deps.iter().enumerate() {
            match dep.exports {
                Some(ExportRef::Lazy(lazy)) => match lazy.verify() {
                    Ok(proof) => verified_deps.push(Some(proof)),
                    Err(error) => {
                        verified_deps.push(None);
                        if first_bad.is_none() {
                            let dep_krate = self.crates.get(dep.index).map_or("", |c| c.input.name);
                            first_bad = Some(blaze_own::format_in!(
                                alloc,
                                "crate `{}`: the record of its dependency `{dep_krate}`: its exports record did not check: {error:?}",
                                krate.input.name
                            ));
                        }
                    }
                },
                Some(ExportRef::LazyInSnapshot(lazy)) => {
                    let dep_krate = self.crates.get(dep.index).map_or("", |c| c.input.name);
                    let bitmap = uncommitted_bitmaps.iter().find(|(i, _)| *i == idx).map(|(_, b)| b);
                    let res: Result<Verified<'_>, blaze_own::String<SharedAlloc<'a>>> = (|| {
                        let Some(snap) = snap_ref else {
                            return Err(blaze_own::format_in!(alloc, "crate {}: missing snapshot for uncommitted dependency {dep_krate}", krate.input.name));
                        };
                        let record = match snap.get(dep.input.at) {
                            Ok(Some(bytes)) => bytes,
                            Ok(None) => return Err(blaze_own::format_in!(alloc, "crate {}: uncommitted dependency {dep_krate} absent from snapshot", krate.input.name)),
                            Err(why) => return Err(blaze_own::format_in!(alloc, "crate {}: uncommitted dependency {dep_krate} snapshot error: {why}", krate.input.name)),
                        };
                        let Some(slice) = lazy.offset.checked_add(lazy.len).and_then(|end| record.get(lazy.offset..end)) else {
                            return Err(blaze_own::format_in!(alloc, "crate {}: uncommitted dependency {dep_krate} exports slice out of bounds", krate.input.name));
                        };
                        match lazy.verify(slice) {
                            Ok((head, _digest)) => {
                                let rebound = match bitmap {
                                    Some(bitmap) => head.rebind_with_bitmap(slice, bitmap),
                                    None => head.rebind(slice),
                                };
                                rebound.ok_or_else(|| {
                                    blaze_own::format_in!(alloc, "crate {}: uncommitted dependency {dep_krate} exports identity mismatch", krate.input.name)
                                })
                            }
                            Err(error) => Err(blaze_own::format_in!(
                                alloc,
                                "crate `{}`: the record of its dependency `{dep_krate}`: its exports record did not check: {error:?}",
                                krate.input.name
                            )),
                        }
                    })();
                    match res {
                        Ok(proof) => verified_deps.push(Some(proof)),
                        Err(words) => {
                            verified_deps.push(None);
                            if first_bad.is_none() {
                                first_bad = Some(words);
                            }
                        }
                    }
                }
                None => verified_deps.push(None),
            }
        }
        if let Some(words) = first_bad {
            let reads = failure_reads_verified(self.deps, &verified_deps, snap_ref, script_input.map(|s| (s, false)), alloc);
            return Ok(WorkerAnswer {
                done: crate::engine_tasks::failed_with(task, reads, alloc),
                result: Err((TaskFailure { name, why: Why::Refused { what: "a dependency's record", rule: crate::names::RULE } }, Some(words))),
                scope_bytes: crate::names::metered::ScopeBytes::default(),
            });
        }
        let refuse = |what, words| WorkerAnswer {
            done: crate::engine_tasks::failed_with(task, failure_reads(self.deps, snap_ref, script_input.map(|s| (s, false)), alloc), alloc),
            result: Err((TaskFailure { name, why: Why::Refused { what, rule: crate::names::RULE } }, Some(words))),
            scope_bytes: crate::names::metered::ScopeBytes::default(),
        };
        let area;
        let outputs = if let Some((script, bytes)) = self.script {
            let record = crate::scripts::payload_parts(bytes).ok_or(blaze_build_script::outputs::RecordError::Layout);
            match record.and_then(|(_, record)| blaze_build_script::outputs::OutputsRef::open(record)) {
                Ok(outputs) => {
                    area = crate::scripts::file_area(self.home, &outputs);
                    Some(outputs.with_area(&area))
                }
                Err(error) => return Ok(refuse("a build script's record", blaze_own::format_in!(alloc, "crate `{}`: its build script's record ({:?}) is not an outputs record: {error:?}", krate.input.name, script.name))),
            }
        } else { None };
        let input = outputs.map_or(krate.input, |outputs| blaze_world::joined(&krate.input, outputs, bump));
        let mut order = Vec::with_capacity_in(self.deps.len(), local);
        let mut order_proofs = Vec::with_capacity_in(self.deps.len(), local);
        let mut positions = Vec::from_elem_in(None, self.deps.len(), local);
        let mut place = |g: usize| -> Result<usize, EngineCheckError> {
            let index = self.deps.binary_search_by_key(&g, |d| d.index)
                .map_err(|_| EngineCheckError::Unseen { task: name, read: blaze_drive::key::crate_scope_name(&self.crates[g].input.key) })?;
            Ok(*positions[index].get_or_insert_with(|| {
                let next = order.len();
                order.push(self.deps[index]);
                order_proofs.push(verified_deps.get(index).copied().flatten());
                next
            }))
        };
        let mut own_deps = Vec::with_capacity_in(input.deps.len(), local);
        for &(alias, g) in input.deps {
            own_deps.push((alias, place(g)?));
        }
        let all = [self.all.core, self.all.alloc, self.all.std, self.all.bstd];
        let given = crate::names::sysroot_given(matches!(krate.origin, blaze_world::Origin::Toolchain(_)), self.at, all.map(|id| id.map(CrateId::index)), input.deps);
        let mut ids = [None; 4];
        for ((slot, id), give) in ids.iter_mut().zip(all).zip(given) {
            if let Some(id) = id {
                if give { *slot = Some(CrateId(place(id.index())? as u32)); }
                else if id.index() == self.at { *slot = Some(CrateId(self.deps.len() as u32)); }
            }
        }
        for dep in self.deps { place(dep.index)?; }
        let sysroot = Sysroot { core: ids[0], alloc: ids[1], std: ids[2], bstd: ids[3] };
        let mut exports = Vec::with_capacity_in(order.len(), local);
        for (dep, maybe_proof) in order.iter().zip(order_proofs.iter().copied()) {
            let proof = match maybe_proof {
                Some(proof) => proof,
                None => {
                    // A failed dependency's error record: a header and one empty root, made and
                    // checked here, for this crate alone.
                    let source = self.crates[dep.index].input;
                    let record = match blaze_scope::crate_scope::error_record(source.key, source.name, source.edition, local) {
                        Ok(record) => blaze_scope::resolve::aligned(&record, bump),
                        Err(error) => return Ok(refuse("a failed dependency's error record", blaze_own::format_in!(alloc, "crate `{}`: the error record of its failed dependency `{}`: {error:?}", input.name, source.name))),
                    };
                    match Verified::check(record) {
                        Ok(proof) => proof,
                        Err(error) => return Ok(refuse("a failed dependency's error record", blaze_own::format_in!(alloc, "crate `{}`: the error record of its failed dependency `{}` did not check: {error:?}", input.name, source.name))),
                    }
                }
            };
            exports.push(proof);
        }
        let heads = Vec::from_iter_in(exports.iter().map(|proof| proof.bytes()), local);
        let need = |keys: &[blaze_id::CrateItemKey]| {
            for key in keys {
                if !self.crates.iter().any(|krate| krate.input.key == *key) {
                    return Ok(refuse("a needed crate the world does not hold", blaze_own::String::from_str_in("the resolution needs a crate's record this command's world does not hold", alloc)));
                }
            }
            Err(EngineCheckError::Unseen { task: name, read: keys.first().map_or(name, blaze_drive::key::crate_scope_name) }.into())
        };
        match blaze_scope::crate_scope::unmet_reaches(&heads, local) {
            Ok(keys) if keys.is_empty() => {}
            Ok(keys) => return need(&keys),
            Err(error) => return Ok(refuse("a dependency's record", blaze_own::format_in!(alloc, "crate `{}`: a dependency's record: {error:?}", input.name))),
        }
        let joined;
        let env = match (self.env, outputs) {
            (Some(lent), Some(outputs)) => {
                let env = match lent.copy_in(bump) {
                    Ok(env) => env,
                    Err(error) => return Ok(refuse("a crate's environment", blaze_own::format_in!(alloc, "crate `{}`: its environment's build-script record did not open again: {error:?}", input.name))),
                };
                joined = env.joined(outputs, bump);
                Some(EnvLent::new(&joined))
            }
            (env, _) => env,
        };
        let job = ScopeJob { workers: self.workers, kept: self.kept, at: self.at, member: krate.member, input: CrateInput { deps: &own_deps, ..input }, deps: &exports, sysroot, packs: self.packs.into(), corrected: self.corrected, env, procs: self.procs, oracle: self.oracle, clock: self.clock, board: Some(self.board) };
        let (resolution, scope_bytes) = crate::names::resolve_with_prepares(cx, root, &job, self.shared_prepares.as_ref(), cx.frame());
        let (resolved, ok, unresolved, oracle) = match resolution {
            Resolution::Made { resolved, ok, unresolved, oracle } => (resolved, ok, unresolved, oracle),
            Resolution::Need(keys) => return need(&keys),
        };
        // THE NEED (D161): the files the resolution read that no outline holds, with the resolved
        // crate still in hand, served here: each outlined on this worker as the walk outlines a
        // file it reaches (engine_check.rs `outline_found`, one code), `finish` run over them, and
        // the outlines filed after the names by `Engine::plan_file` (engine_check.rs
        // `serve_found`). The control's plant serves none: the first is the stop that names it.
        let needed = unoutlined(name, input.name, &resolved, outlined, self.serve.plants.found_cap(), self.kept)?;
        if let Some(first) = needed.first()
            && !self.serve.plants.serves()
        {
            return Err(unseen_file(name, first.read, input.name, first.path, self.kept).into());
        }
        let mut found = FoundBy { unit: name, read: SVec::new_in(self.kept), outlined: SVec::new_in(self.kept), probes: SVec::new_in(self.kept) };
        if !needed.is_empty() {
            // A found file is its crate's: its edition and root are the crate root's outline's.
            // Its root's path in the checkout (the crate list's is under `/0/`, engine_body_plan.rs `root_file`).
            let root_path = input.root.strip_prefix("/0/").unwrap_or(input.root);
            let root_file = Name::file(root_path.as_bytes());
            let Some(crate_root) = outline_at(outlined, root_file, root_path) else {
                return Err(EngineCheckError::Unseen { task: name, read: root_file }.into());
            };
            let (unit_root, edition) = (crate_root.name, crate_root.edition);
            for need in needed.iter() {
                // A module's file is read at its slot `x.rs` or at `x/mod.rs`, the slot named
                // `x.rs` either way (sweep.rs `real_path`); a data file at its own path.
                let module = need.read.kind == Kind::Surface;
                let slot = match (module, need.path.strip_suffix("/mod.rs")) {
                    (true, Some(dir)) => blaze_own::format_in!(local, "{dir}.rs"),
                    _ => blaze_own::String::from_str_in(need.path, local),
                };
                let want = crate::engine_check::FoundWant { slot: slot.as_str(), module, edition: if module { edition } else { blaze_drive::record::DATA_FILE }, root: unit_root, external: !krate.member };
                let (one, probed) = crate::engine_check::outline_found(cx, root, &self.serve.stamps, &want, self.serve.plants, self.kept).map_err(WorkerError::Unread)?;
                found.read.push(one.name);
                found.probes.push((SVec::from_slice_in(slot.as_bytes(), self.kept), probed, module));
                found.outlined.push(one);
            }
        }
        // Its borrow of `resolved` ends before the crate moves into `finish`.
        drop(needed);
        // The walk's found files this crate read: by name, or by the path it read one at.
        // Only match a found file whose role agrees with the read (a Rust module read against a
        // non-data found file; a data include read against a data found file), so a crate that
        // `include_str!`s another crate's macro-written module does not claim ownership of it.
        if !self.serve.found.is_empty() {
            let (files, _, data) = resolved.checkout_reads();
            for (path, is_data_read) in files.iter().map(|path| (path.as_str(), false)).chain(data.iter().map(|(path, _)| (path.as_str(), true))) {
                let Some(one) = outline_at(outlined, Name::file(path.as_bytes()), path) else { continue };
                if let Ok(at) = self.serve.found.binary_search_by_key(&one.name, |(file, _)| *file)
                    && self.serve.found.get(at).is_some_and(|(_, place)| crate::data_include::is_data(place.edition) == is_data_read)
                {
                    found.read.push(one.name);
                }
            }
        }
        found.read.sort_unstable();
        found.read.dedup();
        // By name, the probes in the same order (rule 25: the order the crate read them in is not
        // in what it answers).
        found.outlined.sort_unstable_by_key(|one| one.name);
        found.probes.sort_unstable_by_key(|(slot, ..)| Name::file(slot));
        let implicit_entry_root = input.implicit.entry.then(|| input.root.strip_prefix("/0/")).flatten();
        let mut answer = finish(task, input.name, implicit_entry_root, resolved, ok, unresolved, oracle, &order, &exports, snap_ref, script_input, self.macro_module, outlined, found, alloc, local, bump, self.kept)?;
        answer.scope_bytes = scope_bytes;
        Ok(answer)
    }
}

/// The request's outline of the checkout file at `path`: by its name, else by the path it was
/// read at (a module's `x/mod.rs`, named by its slot `x.rs`).
fn outline_at<'o, 'h>(outlined: &'o [Outlined<'h>], name: Name, path: &str) -> Option<&'o Outlined<'h>> {
    match outlined.binary_search_by(|one| one.name.cmp(&name)) {
        Ok(at) => Some(&outlined[at]),
        Err(_) => outlined.iter().find(|one| one.path.as_slice() == path.as_bytes()),
    }
}

/// A file a crate's names read that no outline of the request holds: the cell it reads (a
/// module file's `Surface`, a data file's outline) and the path it read it at.
#[derive(Clone, Copy)]
struct NeedFile<'p> {
    read: Name,
    path: &'p str,
}

/// Every file `resolved` read that `outlined` holds no outline of, in read order, each once: a
/// module file a macro wrote (`cfg_if!`'s `mod unix;`, `mod $n;`), a data file of a computed
/// path. At most `cap` ([`crate::engine_check::FOUND_CAP`], rule 18): past it
/// [`EngineCheckError::FoundTooMany`] names the crate, the count and the first and last file.
fn unoutlined<'p>(task: Name, krate: &str, resolved: &'p Resolved, outlined: &[Outlined<'_>], cap: usize, heap: SharedAlloc<'static>) -> Result<blaze_own::Vec<NeedFile<'p>, SharedAlloc<'static>>, EngineCheckError> {
    let (files, _, data) = resolved.checkout_reads();
    let mut needed: blaze_own::Vec<NeedFile<'p>, SharedAlloc<'static>> = blaze_own::Vec::new_in(heap.clone());
    let mut count = 0u32;
    let mut last = None;
    let modules = files.iter().map(|path| (Name::new(Kind::Surface, Name::file(path.as_bytes()).subject), path.as_str()));
    let datas = data.iter().map(|(path, _)| (Name::file(path.as_bytes()), path.as_str()));
    for (read, path) in modules.chain(datas) {
        // Held, even as a failed outline (that failure is its own row's, `finish` stops on it):
        // only a file no entry holds is a need. A module read (`Kind::Surface`) is NOT satisfied
        // by a `DATA_FILE` outline (no Rust surface): it must be outlined as Rust.
        let file = Name::file(path.as_bytes());
        let need_surface = read.kind == Kind::Surface;
        let ok_outline = |one: &Outlined<'_>| !need_surface || !crate::data_include::is_data(one.edition);
        let held = outlined.binary_search_by(|one| one.name.cmp(&file)).ok().and_then(|at| outlined.get(at)).is_some_and(ok_outline)
            || outlined.iter().any(|one| one.path.as_slice() == path.as_bytes() && ok_outline(one));
        if held || needed.iter().any(|one| one.path == path) {
            continue;
        }
        count = count.saturating_add(1);
        last = Some(path);
        if needed.len() < cap {
            needed.push(NeedFile { read, path });
        }
    }
    if count as usize > cap {
        return Err(EngineCheckError::FoundTooMany {
            task,
            krate: blaze_own::String::from_str_in(krate, heap.clone()),
            count,
            cap: u32::try_from(cap).unwrap_or(u32::MAX),
            first: blaze_own::String::from_str_in(needed.first().map_or(last.unwrap_or(""), |one| one.path), heap.clone()),
            last: blaze_own::String::from_str_in(last.unwrap_or(""), heap),
        });
    }
    Ok(needed)
}

/// The stop for a file read the plan did not hand: named by the crate and the path (rule 19),
/// the strings made only on that stop.
fn unseen_file(task: Name, read: Name, krate: &str, path: &str, heap: SharedAlloc<'static>) -> EngineCheckError {
    EngineCheckError::UnseenFile { task, read, krate: blaze_own::String::from_str_in(krate, heap.clone()), path: blaze_own::String::from_str_in(path, heap) }
}

#[expect(clippy::too_many_arguments, reason = "one resolved crate and the inputs it was handed")]
fn finish<'a>(task: &EngineTask, krate: &str, implicit_entry_root: Option<&str>, resolved: Resolved, ok: bool, unresolved: u32, oracle: Option<OracleCrate>, deps: &[Dependency<'_>], dep_exports: &[Verified<'_>], snap: Option<&crate::store::StoreSnapshot>, script: Option<Input>, macro_module: Option<Input>, outlined: &[Outlined<'_>], found: FoundBy, alloc: SharedAlloc<'a>, scratch: blaze_own::Alloc<'_>, bump: blaze_own::Bump<'_>, heap: SharedAlloc<'static>) -> Result<WorkerAnswer<'a>, WorkerError> {
    let name = task.planned.name;
    // A file this crate found and its worker outlined (which may upgrade a walk `DATA_FILE` to a
    // Rust outline), else one the request's outlines hold.
    let outline_of = |file: Name, path: &str| outline_of(&found.outlined, file, path).or_else(|| outline_of(outlined, file, path));
    let surface_outline_of = |file: Name, path: &str| surface_outline_of(&found.outlined, file, path).or_else(|| surface_outline_of(outlined, file, path));
    let mut record = work::Record::new_in(name, heap);
    record.out = resolved.out;
    resolved.file_findings(&mut record).map_err(|crate::names::FindingText(what)| WorkerError::Finding(what))?;
    let (files, absent, data) = resolved.checkout_reads();
    let count = files.len() + absent.len() + data.len() + deps.len() + usize::from(script.is_some()) + usize::from(macro_module.is_some());
    record.reads.reserve(count);
    let mut reads = SVec::with_capacity_in(count, alloc);
    // A file read the plan did not hand ([`unoutlined`] found none before this call; kept so a
    // read that slips past it still stops by name, rule 3).
    let unseen = |read: Name, path: &str| unseen_file(name, read, krate, path, heap.clone());
    for path in files {
        let file = Name::file(path.as_bytes());
        let surface = Name::new(Kind::Surface, file.subject);
        if let Some(record_of) = surface_outline_of(file, path)
            && let Some(digest) = crate::engine_tasks::surface_of(record_of)
        {
            record.reads.push((surface, digest));
            reads.push((CellId { of: record_of.name, cell: crate::engine_tasks::SURFACE }, digest));
        } else if let Some(record_of) = outline_of(file, path)
            && record_of.out == crate::work::failed_out()
        {
            // A failed outline (e.g. non-UTF-8 `FILE_NOT_TEXT`) defines `WHOLE = failed_out()`
            // and no `SURFACE` cell; bind to `WHOLE` so fixing the file wakes this crate.
            record.reads.push((file, record_of.out));
            reads.push((CellId { of: record_of.name, cell: WHOLE }, record_of.out));
        } else {
            return Err(unseen(surface, path).into());
        }
    }
    // D104 (rule 21): a blaze-edition binary's root top-level `fn main` is the whole-item input
    // of the implicit `#[::std::main]` call (`blaze-scope` task/lower.rs), whose expansion is
    // stored in this `CrateScope` record. `Outline::surface()` omits plain fn bodies because
    // `blaze-file-items` does not know `Implicit::entry`, so record `definition_cell` for every
    // top-level `fn main` in the crate root file.
    if let Some(root_path) = implicit_entry_root {
        let root_file = Name::file(root_path.as_bytes());
        if let Some((root_outlined, root_record)) =
            surface_outlined(&found.outlined, root_file, root_path).or_else(|| surface_outlined(outlined, root_file, root_path))
            && let Ok(payload) = root_record.payload_bytes()
            && let Ok(view) = blaze_surface::FileItemsView::read(payload)
        {
            let sig_file = crate::signature::SignatureFile::new(&root_outlined.path);
            for (index, entry) in view.items().enumerate() {
                if entry.item.parent.is_none()
                    && entry.item.kind() == blaze_surface::ItemKind::Function
                    && blaze_scope::task::unraw(entry.name) == "main"
                    && let Ok(index) = u32::try_from(index)
                {
                    let entry_name = sig_file.entry_from_chain(crate::signature::stored_chain(view, blaze_surface::ItemIndex(index)));
                    record.reads.push((entry_name, entry.item.hash));
                    reads.push((CellId { of: root_outlined.name, cell: crate::engine_tasks::definition_cell(entry_name) }, entry.item.hash));
                }
            }
        }
    }
    let payload = resolved.payload();
    let parts = crate::names::sections::sections(payload).map_err(|_| EngineCheckError::CellTable { task: name, what: "is in a payload whose sections do not read" })?;
    // Rule 21: `blaze-file-items` omits bare `#[test]` / `#[bench]` plain-fn bodies and bare
    // `STD_EXPR_MACROS` body text from `Outline::surface()`. When a non-stdlib crate shadows
    // `test`/`bench` with a user attribute macro, or shadows/imports/defines any name in
    // `STD_EXPR_MACROS` (or enables `#[no_implicit_prelude]` or emitted a failed macro call),
    // record `definition_cell(entry_name)` reads so body edits still wake `CrateScope`.
    let non_stdlib = !matches!(krate, "core" | "std" | "alloc");
    let (shadowed_test, mut shadowed_expr) = if non_stdlib {
        shadowed_in_names(parts.names, dep_exports, bump)
    } else {
        (false, false)
    };
    if record.findings.iter().any(|(rule, ..)| *rule == crate::names::failed_call::FAILED_CALL) {
        shadowed_expr = true;
    }
    if shadowed_test || shadowed_expr {
        for path in files {
            let file = Name::file(path.as_bytes());
            if let Some((file_outlined, file_record)) =
                surface_outlined(&found.outlined, file, path).or_else(|| surface_outlined(outlined, file, path))
                && let Ok(file_payload) = file_record.payload_bytes()
                && let Ok(view) = blaze_surface::FileItemsView::read(file_payload)
            {
                record_shadowed_definition_reads(
                    file_outlined.name,
                    &file_outlined.path,
                    view,
                    shadowed_test,
                    shadowed_expr,
                    &mut record.reads,
                    &mut reads,
                );
            }
        }
    }
    for path in absent {
        let file = Name::file(path.as_bytes());
        record.reads.push((Name::new(Kind::Surface, file.subject), blaze_drive::key::ABSENT));
        let digest = outlined.binary_search_by_key(&file, |o| o.name).ok().and_then(|i| outlined[i].made.as_ref().ok())
            .and_then(crate::engine_tasks::surface_of).unwrap_or(blaze_drive::key::ABSENT);
        reads.push((CellId { of: file, cell: crate::engine_tasks::SURFACE }, digest));
    }
    for (path, _) in data {
        let file = Name::file(path.as_bytes());
        let record_of = outline_of(file, path).ok_or_else(|| unseen(file, path))?;
        // A dual-included Rust source file defines its raw bytes digest on `DATA` (`WHOLE` is its
        // outline parts digest, which comment/whitespace edits do not move); a pure `DATA_FILE`
        // defines `data_out` / `absent_out` directly on `WHOLE`.
        let (cell, out) = match crate::engine_tasks::data_digest_of(record_of) {
            Some(digest) => (crate::engine_tasks::DATA, digest),
            None => (WHOLE, record_of.out),
        };
        record.reads.push((file, out));
        reads.push((CellId { of: record_of.name, cell }, out));
    }
    for dep in deps {
        record.reads.push((dep.input.name, dep.input.out));
        reads.push(dep_input_cell(dep, snap));
    }
    if let Some(input) = script {
        record.reads.push((input.name, input.out));
        reads.push(input_cell(input, false));
    }
    if let Some(input) = macro_module {
        record.reads.push((input.name, input.out));
        reads.push(input_cell(input, false));
    }
    reads.sort_unstable_by_key(|row| row.0);
    reads.dedup_by_key(|row| row.0);
    record.parts.push(Part { name: Name::links_of(name), locate: Locate::Source(blaze_ladder::native_links::table_digest(parts.links)), place: 0 });
    let (defines, exports_id) = cells_of(name, &record, parts.exports, parts.cells, alloc)?;
    let key = crate::engine_store::record_key(task.key, record.out, &record.reads, payload);
    let done = Done { name, key: task.key, out: record.out, defines, reads, findings: u32::try_from(record.findings.len()).unwrap_or(u32::MAX), failed: false };
    let header = blaze_drive::record::write_plain_head(name, task.planned.source, record.out, &record.reads, payload.len(), scratch);
    let head = SVec::from_slice_in(&header, heap);
    Ok(WorkerAnswer { done, result: Ok(Finished { record, resolved, ok, unresolved, key, exports_id, head, oracle, found }), scope_bytes: crate::names::metered::ScopeBytes::default() })
}

/// Single-pass shadow check over `names_bytes` (a `CrateScope` payload's `names` section) and
/// `dep_exports` for both `#[test]`/`#[bench]` attribute macros and [`blaze_file_items::nested::STD_EXPR_MACROS`]
/// function-like macros.
///
/// Returns `(shadowed_test_attr, shadowed_expr_macro)`.
///
/// Rule 21: `blaze-file-items` omits bare `#[test]` / `#[bench]` plain-fn bodies and bare
/// `STD_EXPR_MACROS` call arguments from `Outline::surface()`. Whenever a non-stdlib crate
/// defines, imports, or `#[macro_use]`s any shadowed name (or uses `#[no_implicit_prelude]`),
/// [`finish`] calls [`record_shadowed_definition_reads`] so body edits still wake `CrateScope`.
fn shadowed_in_names(names_bytes: &[u8], dep_exports: &[Verified<'_>], bump: blaze_own::Bump<'_>) -> (bool, bool) {
    if names_bytes.is_empty() {
        return (false, false);
    }
    let aligned = if names_bytes.as_ptr().align_offset(16) == 0 {
        names_bytes
    } else {
        blaze_scope::resolve::aligned(names_bytes, bump)
    };
    let Ok(verified) = blaze_scope::names_record::Verified::check_header(aligned) else {
        return (false, false);
    };
    let names = blaze_scope::names_record::NamesRecord::open(verified);
    let is_attr_macro = |def: blaze_surface::Def| {
        matches!(def.kind, blaze_surface::DefKind::Macro(shapes) if shapes.accepts(blaze_surface::CallShape::Attr))
    };
    let is_fn_macro = |def: blaze_surface::Def| {
        matches!(def.kind, blaze_surface::DefKind::Macro(shapes) if shapes.accepts(blaze_surface::CallShape::FnLike))
    };

    let mut shadowed_test = false;
    let mut shadowed_expr = false;

    let root = names.crate_root();
    for (name, _, shapes) in root.macro_exports() {
        if !shadowed_test && shapes.accepts(blaze_surface::CallShape::Attr) && matches!(name, "test" | "bench") {
            shadowed_test = true;
        }
        if !shadowed_expr && shapes.accepts(blaze_surface::CallShape::FnLike) && blaze_file_items::nested::is_std_expr_macro(name) {
            shadowed_expr = true;
        }
        if shadowed_test && shadowed_expr {
            break;
        }
    }
    let is_stdlib_expr_macro = |mac: &str, key: blaze_id::ItemKey| -> bool {
        dep_exports.iter().any(|proof| {
            let dep = blaze_scope::scope_record::DepRecord::open(*proof);
            if !matches!(dep.name(), "core" | "alloc" | "std") {
                return false;
            }
            let Some(scope) = dep.scope(dep.root()) else {
                return false;
            };
            let view = scope.view();
            view.get(blaze_surface::Namespace::Macros, mac).is_some_and(|b| b.def.key == key && is_fn_macro(b.def))
                || view.macro_exports().any(|def| def.name == mac && def.def == key && def.shapes.accepts(blaze_surface::CallShape::FnLike))
        })
    };

    // Check `#[macro_use] extern crate ...` roots from non-stdlib dependencies.
    let self_root = names.root_module();
    for root_key in root.macro_use() {
        if shadowed_test && shadowed_expr {
            break;
        }
        if root_key == self_root {
            continue;
        }
        let dep = dep_exports
            .iter()
            .map(|proof| blaze_scope::scope_record::DepRecord::open(*proof))
            .find(|dep| dep.root() == root_key);
        let Some(dep) = dep else {
            // Unknown `#[macro_use]` root: conservatively treat expr macros as shadowed.
            shadowed_expr = true;
            continue;
        };
        if matches!(dep.name(), "core" | "alloc" | "std") {
            continue;
        }
        if let Some(scope) = dep.scope(root_key) {
            let view = scope.view();
            if !shadowed_test {
                for attr in ["test", "bench"] {
                    if view.get(blaze_surface::Namespace::Macros, attr).is_some_and(|b| is_attr_macro(b.def)) {
                        shadowed_test = true;
                        break;
                    }
                }
            }
            if !shadowed_expr {
                for &mac in blaze_file_items::nested::STD_EXPR_MACROS.iter() {
                    if view.get(blaze_surface::Namespace::Macros, mac).is_some_and(|b| is_fn_macro(b.def)) {
                        shadowed_expr = true;
                        break;
                    }
                }
            }
            if !shadowed_test || !shadowed_expr {
                for def in view.macro_exports() {
                    if !shadowed_test && def.shapes.accepts(blaze_surface::CallShape::Attr) && matches!(def.name, "test" | "bench") {
                        shadowed_test = true;
                    }
                    if !shadowed_expr && def.shapes.accepts(blaze_surface::CallShape::FnLike) && blaze_file_items::nested::is_std_expr_macro(def.name) {
                        shadowed_expr = true;
                    }
                }
            }
        }
    }

    for module in names.modules() {
        if shadowed_test && shadowed_expr {
            break;
        }
        if !shadowed_expr && names.no_implicit_prelude(module) {
            shadowed_expr = true;
        }
        let Some(scope) = names.scope(module) else { continue };
        if !shadowed_test || !shadowed_expr {
            for def in scope.textual().chain(scope.macro_exports()) {
                if !shadowed_test && def.shapes.accepts(blaze_surface::CallShape::Attr) && matches!(def.name, "test" | "bench") {
                    shadowed_test = true;
                }
                if !shadowed_expr && def.shapes.accepts(blaze_surface::CallShape::FnLike) && blaze_file_items::nested::is_std_expr_macro(def.name) {
                    shadowed_expr = true;
                }
                if shadowed_test && shadowed_expr {
                    break;
                }
            }
        }
        if !shadowed_test {
            for attr in ["test", "bench"] {
                if let Some(binding) = scope.get(blaze_surface::Namespace::Macros, attr)
                    && is_attr_macro(binding.def)
                {
                    shadowed_test = true;
                    break;
                }
            }
        }
        if !shadowed_expr {
            for &mac in blaze_file_items::nested::STD_EXPR_MACROS.iter() {
                if let Some(binding) = scope.get(blaze_surface::Namespace::Macros, mac)
                    && is_fn_macro(binding.def)
                    && !is_stdlib_expr_macro(mac, binding.def.key)
                {
                    shadowed_expr = true;
                    break;
                }
            }
        }
    }
    (shadowed_test, shadowed_expr)
}

#[cfg(test)]
fn has_shadowed_test_attr(names_bytes: &[u8], bump: blaze_own::Bump<'_>) -> bool {
    shadowed_in_names(names_bytes, &[], bump).0
}

#[cfg(test)]
fn has_shadowed_expr_macro(names_bytes: &[u8], dep_exports: &[Verified<'_>], bump: blaze_own::Bump<'_>) -> bool {
    shadowed_in_names(names_bytes, dep_exports, bump).1
}

/// Records `definition_cell(entry_name)` reads for entries whose bodies `Outline::surface()`
/// omits when shadowed: attributed functions (`shadowed_test`) and `Fn`/`Const`/`Static`
/// items with `body_holds_items == true` (`shadowed_expr`). Each matching entry is recorded once.
fn record_shadowed_definition_reads<A: core::alloc::Allocator>(
    file_name: Name,
    file_path: &[u8],
    view: blaze_surface::FileItemsView<'_>,
    shadowed_test: bool,
    shadowed_expr: bool,
    record_reads: &mut Vec<(Name, ContentId), A>,
    reads: &mut SVec<'_, (CellId, ContentId)>,
) {
    let sig_file = crate::signature::SignatureFile::new(file_path);
    for (index, entry) in view.items().enumerate() {
        let want_test = shadowed_test
            && entry.item.kind() == blaze_surface::ItemKind::Function
            && view.attrs(&entry.item).any(|attr| attr.kind == blaze_surface::AttrKind::Other);
        let want_expr = shadowed_expr
            && matches!(
                entry.item.detail,
                blaze_surface::ItemDetail::Fn { body_holds_items: true }
                    | blaze_surface::ItemDetail::Const { body_holds_items: true }
                    | blaze_surface::ItemDetail::Static { body_holds_items: true }
            );
        if (want_test || want_expr)
            && let Ok(index) = u32::try_from(index)
        {
            let entry_name = sig_file.entry_from_chain(crate::signature::stored_chain(view, blaze_surface::ItemIndex(index)));
            record_reads.push((entry_name, entry.item.hash));
            reads.push((CellId { of: file_name, cell: crate::engine_tasks::definition_cell(entry_name) }, entry.item.hash));
        }
    }
}

/// Records `definition_cell(entry_name)` reads for every function in `view` carrying an
/// `AttrKind::Other` attribute (which includes bare `#[test]` and `#[bench]`).
#[cfg(test)]
fn record_attr_fn_definition_reads<A: core::alloc::Allocator>(
    file_name: Name,
    file_path: &[u8],
    view: blaze_surface::FileItemsView<'_>,
    record_reads: &mut Vec<(Name, ContentId), A>,
    reads: &mut SVec<'_, (CellId, ContentId)>,
) {
    record_shadowed_definition_reads(file_name, file_path, view, true, false, record_reads, reads);
}

/// Results may arrive in any order. The engine's task index orders every observable merge.
fn order_completions<T>(answers: &mut [(usize, T)]) {
    #[cfg(test)]
    if schedule_test::finish_order() { return; }
    answers.sort_unstable_by_key(|(index, _)| *index);
}

#[cfg(test)]
pub(crate) mod schedule_test {
    use super::*;
    use core::cell::RefCell;
    use std::vec::Vec as List;

    #[test]
    fn engine_script_names_refuses_readmission_before_and_after_filing() {
        let frame = Frame::with_reserve(1 << 20, Tag::Nameres).expect("the admissions table's frame");
        let mut admissions = Admissions(BTreeMap::new_in(&frame));
        let task = Name::new(Kind::CrateScope, Name::file(b"free1/src/lib.rs").subject);
        admissions.admit(task).expect("first entry");
        assert!(matches!(admissions.admit(task), Err(EngineCheckError::Twice { task: refused }) if refused == task));
        admissions.finish(task).expect("the first entry can still file its result");
        assert!(matches!(admissions.admit(task), Err(EngineCheckError::Twice { task: refused }) if refused == task));
        println!("duplicate Names admission refused before and after filing");
    }

    #[test]
    fn names_dag_critical_path_priority_ranks_hubs_ahead_of_leaves_and_breaks_ties_by_index() {
        // Graph:
        //   0 (leaf_a): no dependents
        //   1 (core):   depended on by 2 (alloc), 3 (std), 4 (syn), 5 (app_a), 6 (app_b)
        //   2 (alloc):  depends on 1; depended on by 3 (std)
        //   3 (std):    depends on 1, 2; depended on by 4 (syn), 5 (app_a), 6 (app_b)
        //   4 (syn):    depends on 1, 3; depended on by 5 (app_a), 6 (app_b)
        //   5 (app_a):  depends on 1, 3, 4 (leaf)
        //   6 (app_b):  depends on 1, 3, 4 (leaf)
        //   7 (leaf_b): no dependents
        let edges: [&[usize]; 8] = [
            &[],
            &[],
            &[1],
            &[1, 2],
            &[1, 3],
            &[1, 3, 4],
            &[1, 3, 4],
            &[],
        ];
        let frame = Frame::with_reserve(1 << 20, Tag::Nameres).expect("dag test frame");
        let dag = build_names_dag(edges.len(), |_| true, |at| edges[at].iter().copied(), &frame).expect("acyclic dag");

        // Wave depths match longest path from roots.
        assert_eq!(dag.entries[0].depth, 0);
        assert_eq!(dag.entries[1].depth, 0);
        assert_eq!(dag.entries[2].depth, 1);
        assert_eq!(dag.entries[3].depth, 2);
        assert_eq!(dag.entries[4].depth, 3);
        assert_eq!(dag.entries[5].depth, 4);
        assert_eq!(dag.entries[6].depth, 4);
        assert_eq!(dag.entries[7].depth, 0);

        // Critical-path priority strictly prioritizes hubs on the critical path over leaves.
        assert!(dag.prio(1) > dag.prio(2), "core > alloc");
        assert!(dag.prio(2) > dag.prio(3), "alloc > std");
        assert!(dag.prio(3) > dag.prio(4), "std > syn");
        assert!(dag.prio(4) > dag.prio(5), "syn > app_a");
        assert_eq!(dag.prio(0), 1);
        assert_eq!(dag.prio(7), 1);
        assert!(dag.prio(1) > dag.prio(0), "hub core must outrank wave-0 leaf");

        // Verify Heap4 priority ordering on wave-0 crates: core(1) pops before leaf_a(0) and leaf_b(7),
        // with equal-priority leaves broken deterministically by crate index (0 before 7).
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        struct PrioKey {
            prio: u32,
            at: usize,
        }
        impl PartialOrd for PrioKey {
            fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
                Some(self.cmp(other))
            }
        }
        impl Ord for PrioKey {
            fn cmp(&self, other: &Self) -> core::cmp::Ordering {
                other.prio.cmp(&self.prio).then_with(|| self.at.cmp(&other.at))
            }
        }
        let mut heap: Heap4<PrioKey, &Frame> = Heap4::new_in(&frame);
        for at in [7usize, 0, 1] {
            heap.push(PrioKey { prio: dag.prio(at), at });
        }
        assert_eq!(heap.pop().map(|k| k.at), Some(1));
        assert_eq!(heap.pop().map(|k| k.at), Some(0));
        assert_eq!(heap.pop().map(|k| k.at), Some(7));
        assert_eq!(heap.pop(), None);

        // Reverse CSR `dependents_of` lists exact sorted dependents.
        assert_eq!(dag.dependents_of(1), &[2, 3, 4, 5, 6]);
        assert_eq!(dag.dependents_of(4), &[5, 6]);
        assert!(dag.dependents_of(0).is_empty());

        // Cycle detection returns Err.
        let cyclic: [&[usize]; 2] = [&[1], &[0]];
        assert!(build_names_dag(cyclic.len(), |_| true, |at| cyclic[at].iter().copied(), &frame).is_err());
    }

    #[test]
    fn lazy_exports_verifies_once_across_concurrent_workers() {
        let frame = Frame::with_reserve(2 << 20, Tag::Nameres).expect("lazy exports frame");
        let erased = blaze_own::Erased::new(&frame);
        let bump = erased.bump();
        let local = bump.alloc();
        let key = blaze_id::CrateItemKey(ContentId::of("test lazy_exports_once", b"lazy_hub"));
        let raw = blaze_scope::crate_scope::error_record(key, "lazy_hub", blaze_surface::Edition::E2024, local).expect("error_record");
        let aligned = blaze_scope::resolve::aligned(&raw, bump);
        let lazy: &LazyExports<'_> = frame.alloc(LazyExports::new(aligned, "lazy_hub_concurrent", None));

        {
            let mut checks = CHECKED.lock();
            checks.krate = "lazy_hub_concurrent";
            checks.count = 0;
        }
        let io = bstd::io::Io::simulated(bstd::io::SimWorld::new(41, bstd::io::Limits::USUAL));
        let cx = io.cx(bstd::test::BUDGET, 4 << 20).expect("root context");
        let scope = cx.scope();
        scope.narrow_workers(core::num::NonZeroU32::new(8).expect("nonzero"), Class::Foreground);
        let mut handles = Vec::with_capacity_in(16, &frame);
        for _ in 0..16 {
            handles.push(scope.may_overlap("lazy exports verify test", 64 << 10, move |_cx: &Cx| {
                let proof = lazy.verify().expect("valid exports record");
                assert_eq!(proof.key(), key);
                Ok::<_, Infallible>(lazy.digest())
            }));
        }
        let expected_digest = exports_digest(aligned);
        for h in handles.into_iter() {
            assert_eq!(h.join(&cx).expect("join worker"), expected_digest);
        }
        scope.join(&cx).expect("join scope");
        let count = {
            let mut checks = CHECKED.lock();
            checks.krate = "";
            core::mem::replace(&mut checks.count, 0)
        };
        assert_eq!(count, 1, "LazyExports must verify exactly once across 16 concurrent workers");
    }

    #[test]
    fn shadowed_test_proc_macro_records_definition_cell_while_builtin_test_does_not() {
        use blaze_id::CrateItemKey;
        use blaze_scope::resolve::{CrateInput, SourceTexts, resolve_crates};
        use blaze_surface::Edition;
        use blaze_unit_graph::{Implicit, Prelude, StdIs};

        struct MemSources<'a>(&'a [(usize, &'a str, &'a str)]);
        impl<'a> SourceTexts<'a> for MemSources<'a> {
            fn read(&self, krate: usize, path: &str) -> Option<&'a str> {
                self.0.iter().find(|&&(k, p, _)| k == krate && p == path).map(|&(_, _, text)| text)
            }
        }

        let proc_src = "#![no_core]\n#[proc_macro_attribute]\npub fn test(_attr: TokenStream, item: TokenStream) -> TokenStream { item }\n";
        let user_src = "#![no_core]\nuse my_macros::test;\n#[test]\nfn shadowed_test() { let x = 1; }\nfn plain_fn() { let y = 2; }\n";
        let normal_src = "#![no_core]\n#[test]\nfn builtin_test() { let x = 1; }\nfn plain_fn() { let y = 2; }\n";

        let frame = Frame::with_reserve(8 << 20, Tag::Nameres).expect("test frame");
        let erased = blaze_own::Erased::new(&frame);
        let bump = erased.bump();
        let alloc = bump.alloc();
        let rust = Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2021), proc_macro: false, entry: false };
        let proc_implicit = Implicit { proc_macro: true, ..rust };
        let sources = MemSources(&[
            (0, "/macros/lib.rs", proc_src),
            (1, "/user/lib.rs", user_src),
            (2, "/normal/lib.rs", normal_src),
        ]);
        let deps_user = [("my_macros", 0usize)];
        let test_cfg = [("test", None)];
        let crates = [
            CrateInput {
                name: "my_macros",
                key: CrateItemKey(ContentId::of("test shadowed_test proc_macro", b"my_macros")),
                edition: Edition::E2021,
                implicit: proc_implicit,
                root: "/macros/lib.rs",
                shown_from: "/",
                features: &[],
                deps: &[],
                cfg: &[],
                script: None,
            },
            CrateInput {
                name: "user",
                key: CrateItemKey(ContentId::of("test shadowed_test user", b"user")),
                edition: Edition::E2021,
                implicit: rust,
                root: "/user/lib.rs",
                shown_from: "/",
                features: &[],
                deps: &deps_user,
                cfg: &test_cfg,
                script: None,
            },
            CrateInput {
                name: "normal",
                key: CrateItemKey(ContentId::of("test shadowed_test normal", b"normal")),
                edition: Edition::E2021,
                implicit: rust,
                root: "/normal/lib.rs",
                shown_from: "/",
                features: &[],
                deps: &[],
                cfg: &test_cfg,
                script: None,
            },
        ];
        let solved = resolve_crates(&crates, Sysroot::default(), &sources, &blaze_scope::task::Serial, bump).expect("resolve_crates");
        let user_names = blaze_scope::names_record::encode(solved.krate(1), alloc).expect("encode user names");
        let normal_names = blaze_scope::names_record::encode(solved.krate(2), alloc).expect("encode normal names");

        assert!(has_shadowed_test_attr(&user_names, bump), "crate importing user #[proc_macro_attribute] pub fn test must trigger definition_cell reads");
        assert!(!has_shadowed_test_attr(&normal_names, bump), "normal crate using built-in #[test] must not trigger definition_cell reads");

        let outline = blaze_file_items::outline(user_src, Edition::E2021, alloc);
        let file_items_bytes = outline.to_file_items(user_src, ContentId::of(crate::sweep::FILE_DOMAIN, user_src.as_bytes()), alloc).expect("to_file_items");
        let file_items_aligned = blaze_scope::resolve::aligned(&file_items_bytes, bump);
        let view = blaze_surface::FileItemsView::read(file_items_aligned).expect("FileItemsView::read");
        let file_name = Name::file(b"user/lib.rs");
        let heap = blaze_own::testing::heap();
        let mut record_reads: Vec<(Name, ContentId), SharedAlloc<'_>> = Vec::new_in(heap.shared());
        let mut reads: SVec<'_, (CellId, ContentId)> = SVec::new_in(heap.shared());
        record_attr_fn_definition_reads(file_name, b"user/lib.rs", view, &mut record_reads, &mut reads);

        assert_eq!(reads.len(), 1, "only attributed #[test] fn must record definition_cell, not plain_fn");
        let sig_file = crate::signature::SignatureFile::new(b"user/lib.rs");
        let shadowed_item = view.items().enumerate().find(|(_, e)| e.name == "shadowed_test").expect("shadowed_test entry");
        let expected_entry = sig_file.entry_from_chain(crate::signature::stored_chain(view, blaze_surface::ItemIndex(u32::try_from(shadowed_item.0).expect("small"))));
        let expected_cell = CellId { of: file_name, cell: crate::engine_tasks::definition_cell(expected_entry) };
        assert_eq!(reads[0], (expected_cell, shadowed_item.1.item.hash));
        assert_eq!(record_reads[0], (expected_entry, shadowed_item.1.item.hash));
    }

    #[test]
    fn shadowed_std_expr_macro_records_definition_cell_while_normal_crate_does_not() {
        use blaze_id::CrateItemKey;
        use blaze_scope::resolve::{CrateInput, SourceTexts, resolve_crates};
        use blaze_surface::Edition;
        use blaze_unit_graph::{Implicit, Prelude, StdIs};

        struct MemSources<'a>(&'a [(usize, &'a str, &'a str)]);
        impl<'a> SourceTexts<'a> for MemSources<'a> {
            fn read(&self, krate: usize, path: &str) -> Option<&'a str> {
                self.0.iter().find(|&&(k, p, _)| k == krate && p == path).map(|&(_, _, text)| text)
            }
        }

        let macro_dep_src = "#![no_core]\n#[macro_export]\nmacro_rules! println { ($($t:tt)*) => {}; }\n";
        let local_shadow_src = "#![no_core]\nmacro_rules! assert_eq { ($a:expr, $b:expr) => {}; }\nfn calls_assert() { assert_eq!(1, 1); }\nfn plain_fn() { let y = 2; }\n";
        let import_shadow_src = "#![no_core]\nuse dep_macros::println;\nfn calls_println() { println!(\"hi\"); }\n";
        let no_prelude_src = "#![no_core]\n#![no_implicit_prelude]\nfn plain() { let x = 1; }\n";
        let normal_src = "#![no_core]\nfn plain() { let x = 1; }\n";

        let frame = Frame::with_reserve(8 << 20, Tag::Nameres).expect("test frame");
        let erased = blaze_own::Erased::new(&frame);
        let bump = erased.bump();
        let alloc = bump.alloc();
        let rust = Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2021), proc_macro: false, entry: false };
        let sources = MemSources(&[
            (0, "/dep/lib.rs", macro_dep_src),
            (1, "/local/lib.rs", local_shadow_src),
            (2, "/imported/lib.rs", import_shadow_src),
            (3, "/noprelude/lib.rs", no_prelude_src),
            (4, "/normal/lib.rs", normal_src),
        ]);
        const DEPS_IMPORTED: &[(&str, usize)] = &[("dep_macros", 0usize)];
        let mk = |name: &'static str, root: &'static str, deps: &'static [(&'static str, usize)]| CrateInput {
            name,
            key: CrateItemKey(ContentId::of("test shadowed_expr_macro", name.as_bytes())),
            edition: Edition::E2021,
            implicit: rust,
            root,
            shown_from: "/",
            features: &[],
            deps,
            cfg: &[],
            script: None,
        };
        let crates = [
            mk("dep_macros", "/dep/lib.rs", &[]),
            mk("local_shadow", "/local/lib.rs", &[]),
            mk("import_shadow", "/imported/lib.rs", DEPS_IMPORTED),
            mk("no_prelude", "/noprelude/lib.rs", &[]),
            mk("normal", "/normal/lib.rs", &[]),
        ];
        let solved = resolve_crates(&crates, Sysroot::default(), &sources, &blaze_scope::task::Serial, bump).expect("resolve_crates");
        let local_names = blaze_scope::names_record::encode(solved.krate(1), alloc).expect("encode local");
        let import_names = blaze_scope::names_record::encode(solved.krate(2), alloc).expect("encode import");
        let noprelude_names = blaze_scope::names_record::encode(solved.krate(3), alloc).expect("encode noprelude");
        let normal_names = blaze_scope::names_record::encode(solved.krate(4), alloc).expect("encode normal");

        assert!(has_shadowed_expr_macro(&local_names, &[], bump), "local macro_rules! assert_eq must trigger expr-macro shadow guard");
        assert!(has_shadowed_expr_macro(&import_names, &[], bump), "imported use dep_macros::println must trigger expr-macro shadow guard");
        assert!(has_shadowed_expr_macro(&noprelude_names, &[], bump), "#![no_implicit_prelude] must trigger expr-macro shadow guard");
        assert!(!has_shadowed_expr_macro(&normal_names, &[], bump), "normal crate must not trigger expr-macro shadow guard");

        // `record_shadowed_definition_reads` records `definition_cell` ONLY for items with `body_holds_items == true`.
        let outline = blaze_file_items::outline(local_shadow_src, Edition::E2021, alloc);
        let file_items_bytes = outline.to_file_items(local_shadow_src, ContentId::of(crate::sweep::FILE_DOMAIN, local_shadow_src.as_bytes()), alloc).expect("to_file_items");
        let file_items_aligned = blaze_scope::resolve::aligned(&file_items_bytes, bump);
        let view = blaze_surface::FileItemsView::read(file_items_aligned).expect("FileItemsView::read");
        let file_name = Name::file(b"local/lib.rs");
        let heap = blaze_own::testing::heap();
        let mut record_reads: Vec<(Name, ContentId), SharedAlloc<'_>> = Vec::new_in(heap.shared());
        let mut reads: SVec<'_, (CellId, ContentId)> = SVec::new_in(heap.shared());
        record_shadowed_definition_reads(file_name, b"local/lib.rs", view, false, true, &mut record_reads, &mut reads);

        assert_eq!(reads.len(), 1, "only fn with body_holds_items=true must record definition_cell, not plain_fn");
        let sig_file = crate::signature::SignatureFile::new(b"local/lib.rs");
        let calls_item = view.items().enumerate().find(|(_, e)| e.name == "calls_assert").expect("calls_assert entry");
        let expected_entry = sig_file.entry_from_chain(crate::signature::stored_chain(view, blaze_surface::ItemIndex(u32::try_from(calls_item.0).expect("small"))));
        let expected_cell = CellId { of: file_name, cell: crate::engine_tasks::definition_cell(expected_entry) };
        assert_eq!(reads[0], (expected_cell, calls_item.1.item.hash));
        assert_eq!(record_reads[0], (expected_entry, calls_item.1.item.hash));
    }

    #[derive(Debug, PartialEq, Eq)]
    pub(crate) struct Row {
        pub(crate) name: Name,
        wave: u16,
        reads: List<(CellId, ContentId)>,
        defines: List<(Cell, ContentId)>,
        pub(crate) key: Option<Key>,
        pub(crate) wire: List<u8>,
        pub(crate) extras: List<(ContentId, List<u8>)>,
        failure: Option<(TaskFailure, Option<std::string::String>)>,
    }

    struct Capture {
        reverse: bool,
        by_finish: bool,
        rows: List<Row>,
    }
    std::thread_local! {
        static CAPTURE: RefCell<Option<Capture>> = const { RefCell::new(None) };
    }

    pub(crate) fn capture<R>(reverse: bool, by_finish: bool, run: impl FnOnce() -> R) -> (R, List<Row>) {
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) { CAPTURE.with(|c| { c.borrow_mut().take(); }); }
        }
        let _reset = Reset;
        CAPTURE.with(|c| *c.borrow_mut() = Some(Capture { reverse, by_finish, rows: List::new() }));
        let value = run();
        let rows = CAPTURE.with(|c| c.borrow_mut().take().expect("the running capture").rows);
        (value, rows)
    }

    pub(super) fn reverse_delivery() -> bool {
        CAPTURE.with(|c| c.borrow().as_ref().is_some_and(|c| c.reverse))
    }

    pub(super) fn finish_order() -> bool {
        CAPTURE.with(|c| c.borrow().as_ref().is_some_and(|c| c.by_finish))
    }

    pub(super) fn merged(task: &EngineTask, made: &Result<work::Made<'_>, Failure<'_>>, done: &Done<'_>, key: Option<Key>, head: Option<&[u8]>) {
        CAPTURE.with(|slot| {
            let mut slot = slot.borrow_mut();
            let Some(capture) = slot.as_mut() else { return };
            let mut row = Row { name: done.name, wave: task.planned.phase.wave, reads: done.reads.to_vec(), defines: done.defines.to_vec(), key, wire: List::new(), extras: List::new(), failure: None };
            match made {
                Ok(record) => {
                    let payload = record.payload_bytes().expect("capture before the put");
                    row.wire.extend_from_slice(head.expect("a made record has its worker's header"));
                    row.wire.extend_from_slice(payload);
                    row.extras.extend(record.extras.records.iter().map(|(key, bytes)| (*key, bytes.to_vec())));
                }
                Err(failure) => row.failure = Some((failure.failure, failure.words.map(str::to_owned))),
            }
            capture.rows.push(row);
        });
    }
}

fn refusal(name: Name, what: &'static str) -> Failure<'static> {
    Failure { failure: TaskFailure { name, why: Why::Refused { what, rule: RULE } }, words: None }
}

/// The outline of the checkout file at `path`: by its name, else by the path it was read at.
fn outline_of<'o, 'h>(outlined: &'o [Outlined<'h>], name: Name, path: &str) -> Option<&'o work::Record<SharedAlloc<'h>>> {
    let one = match outlined.binary_search_by(|one| one.name.cmp(&name)) {
        Ok(at) => &outlined[at],
        Err(_) => outlined.iter().find(|one| one.path.as_slice() == path.as_bytes())?,
    };
    one.made.as_ref().ok()
}

/// The Rust (surface-bearing) outline entry and record of the checkout file at `path`: by its
/// name if that record holds a `Surface`, else by its physical path (e.g. a `mod.rs` whose
/// `Name::file` was also listed as a `DATA_FILE` beside the `mod x;` slot outline).
fn surface_outlined<'o, 'h>(outlined: &'o [Outlined<'h>], name: Name, path: &str) -> Option<(&'o Outlined<'h>, &'o work::Record<SharedAlloc<'h>>)> {
    if let Ok(at) = outlined.binary_search_by(|one| one.name.cmp(&name))
        && let Some(one) = outlined.get(at)
        && let Ok(record) = &one.made
        && crate::engine_tasks::surface_of(record).is_some()
    {
        return Some((one, record));
    }
    outlined.iter().find_map(|one| {
        if one.path.as_slice() != path.as_bytes() {
            return None;
        }
        let record = one.made.as_ref().ok()?;
        crate::engine_tasks::surface_of(record).map(|_| (one, record))
    })
}

/// The Rust (surface-bearing) outline of the checkout file at `path`.
fn surface_outline_of<'o, 'h>(outlined: &'o [Outlined<'h>], name: Name, path: &str) -> Option<&'o work::Record<SharedAlloc<'h>>> {
    surface_outlined(outlined, name, path).map(|(_, record)| record)
}

/// The crates the crate at `at` reads directly: its dependencies and, for a program crate, the
/// sysroot crates (names/scope.rs `locals`: a toolchain crate is given what its row names).
/// [`edges_of`] over the crate list's rows: the same edges in the same order (the list's deps
/// are the world's, its sysroot the world's; names.rs `list_differences` holds them equal).
fn edges_listed<'l>(list: &'l blaze_world::crate_list::CrateList<'static>, at: usize) -> impl Iterator<Item = usize> + 'l {
    use blaze_world::crate_list::CrateGraph;
    let toolchain = list.toolchain(at);
    let sysroot = list.sysroot().into_iter().flatten().filter(move |&g| !toolchain && g != at);
    CrateGraph::deps(list, at).chain(sysroot)
}

fn edges_of<'w>(krate: &blaze_world::Crate<'w>, all: Sysroot, at: usize) -> impl Iterator<Item = usize> + use<'w> {
    let toolchain = matches!(krate.origin, blaze_world::Origin::Toolchain(_));
    let sysroot = [all.core, all.alloc, all.std, all.bstd].into_iter().flatten()
        .map(|id| id.index()).filter(move |&g| !toolchain && g != at);
    krate.input.deps.iter().map(|&(_, g)| g).chain(sysroot)
}

#[derive(Clone, Copy)]
enum Mapped<'w> {
    /// Its exports record, checked whole lazily once on the first dependent worker thread
    /// ([`LazyExports`]): every dependent the slot serves opens the proof in O(1) and checks
    /// nothing again (rule 74). The boolean records whether the dependency's `CrateScope` row
    /// defines the [`EXPORTS`] cell.
    Lazy(&'w LazyExports<'w>, bool),
    Failed,
    Unreadable(&'w str),
}

/// Records a verification check for `krate` in tests (`tests/names_verified_once_tests.rs`).
fn record_checked_test(krate: &str) {
    #[cfg(test)]
    {
        let mut checks = CHECKED.lock();
        if !checks.krate.is_empty() && checks.krate == krate {
            checks.count += 1;
        }
    }
    #[cfg(not(test))]
    let _ = krate;
}

/// Tests: how many times the exports record of the crate a test names was checked
/// ([`checked`]; tests/names_verified_once_tests.rs).
#[cfg(test)]
pub(crate) static CHECKED: blaze_core::lock::Lock<Checks> = blaze_core::lock::Lock::new("names.checked (tests)", Checks { krate: "", count: 0 });

/// The crate a test watches and how many times its exports record was checked.
#[cfg(test)]
pub(crate) struct Checks {
    pub(crate) krate: &'static str,
    pub(crate) count: u32,
}

struct WaveInputs<'w, 'm> {
    crates: &'w [blaze_world::Crate<'w>],
    all: Sysroot,
    names: &'w [Name],
    made: &'w [(Name, Option<(Key, ContentId)>)],
    failed_out: ContentId,
    /// A single coordinator writer. Workers only read their already published slots.
    slots: &'w [OnceLock<Mapped<'m>>],
}

impl WaveInputs<'_, '_> {
    /// In world-index order, as the engine handed the old resolver its additional inputs.
    fn dependencies<'d, 'a>(&self, at: usize, task: Name, alloc: blaze_own::Alloc<'a>) -> Result<Vec<Dependency<'d>, blaze_own::Alloc<'a>>, EngineCheckError> {
        let mut deps = Vec::new_in(alloc);
        let mut stack = Vec::from_iter_in(edges_of(&self.crates[at], self.all, at), alloc);
        let mut seen = Vec::from_elem_in(false, self.crates.len(), alloc);
        seen[at] = true;
        while let Some(g) = stack.pop() {
            if core::mem::replace(&mut seen[g], true) { continue; }
            let (input, absent) = lookup(self.made, task, self.names[g], self.failed_out)?;
            deps.push(Dependency { index: g, input, absent, has_exports: false, exports: None });
            stack.extend(edges_of(&self.crates[g], self.all, g));
        }
        deps.sort_unstable_by_key(|dep| dep.index);
        Ok(deps)
    }
}
