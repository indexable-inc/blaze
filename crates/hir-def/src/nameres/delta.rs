//! Name resolution maintained by deltas (docs/v2/plan/design/dbsp.md §3–6, §9): maintainer
//! `100`, which every write to the database ends with. It keeps each crate's resolution in its
//! own state ([`state`]), reads what a write changed as a delta of round-0 logs ([`input`]),
//! runs again only the groups that delta and their readers reach ([`run`]), and publishes per
//! module ([`publish`]): `crate_local_def_map` of a maintained crate reads what it published.
//!
//! One write of one crate:
//!
//! 1. a group whose change only inserted ops that bind no name (an impl, a builtin derive impl,
//!    an unnamed `const`, an `extern` block) has them put into its module's published scope,
//!    with no run;
//! 2. the other groups whose log changed or that are new are the region; a change that removed
//!    an op, a group gone and a binding of a dependency that changed bring in their readers,
//!    transitively (DRed's over-delete, since a glob cycle can hold a binding up by itself);
//! 3. the region runs from its round-0 logs over the rest of the crate as the state has it, to
//!    the fixed point `collect_block`'s loop reaches; its rows alone are taken out of the
//!    crate's table and put back;
//! 4. each key the run changed (a name of a module, or the module whole) wakes the groups
//!    outside the region that read it, and the grown region runs again from scratch, until a
//!    run changes no key anyone outside it read: the cutoff;
//! 5. every scope that changed is set; each group of the region makes its share of the name
//!    records again, and the crate's resolution is set in the parts that changed.
//!
//! A change of the crate env (an `extern crate`, a `#[macro_use]`, the std prelude), which no
//! read notes, runs every group. A crate graph change, a crate first seen and a state lost to
//! a panic run the crate from empty: the cold run, which is the same computation over every
//! group.

mod assemble;
mod diff;
mod input;
mod publish;
pub(in crate::nameres) mod reads;
mod run;
mod state;
#[cfg(test)]
mod tests;

use base_db::{Crate, Db, Reads, Written};
use blaze_dbsp::{Pos, ReadIndex, Weight, Work};
use blaze_hash::{HashMap, HashSet};
use blaze_mem::{Frame, Tag};
use hir_expand::name::Name;

pub(in crate::nameres) use self::publish::{
    input_children, input_legacy_macro, input_name, input_scope, input_traits, published,
    published_module, published_modules, published_records,
};
pub(in crate::nameres) use self::state::State;
#[cfg(test)]
pub(in crate::nameres) use self::publish::Slices;
use self::{
    diff::{Changed, Items},
    input::Delta,
    reads::Read,
    run::RunOut,
    state::{CrateState, Group, Key, Scratch, over_delete},
};
use crate::{
    BuiltinDeriveImplId, EnumId, ImplId, ModuleDefId, ModuleId, TraitId,
    item_scope::ItemScope,
    nameres::{
        BuildingScope, ModuleData, ModulesMap, ScopeSlot,
        facts::{ops::DeclOp, queries},
        frozen,
    },
};

/// What the maintainer reports its work under ([`Db::report_work`]).
const NAME: &str = "nameres";

inventory::submit! {
    base_db::Maintainer { order: 100, name: NAME, reads: read_step, apply: apply_step }
}

/// What the last write changed of each maintained crate's resolution, in dependency order:
/// what the impls maintainer (`200`) reads. Cleared at the start of each write.
#[derive(Default)]
pub struct LastWrite {
    pub crates: Vec<CrateDelta>,
}

/// One crate's share of [`LastWrite`]; a crate the write left alone has none.
pub struct CrateDelta {
    pub krate: Crate,
    /// The crate was maintained from empty: every impl, derive impl and trait it has is an
    /// insert, and nothing held of it before stands.
    pub fresh: bool,
    /// Impls gone (`-1`, at the position they were handed over at) or new or moved (`+1`).
    pub impls: Vec<(ImplId, Pos, Weight)>,
    pub derive_impls: Vec<(BuiltinDeriveImplId, Pos, Weight)>,
    /// Traits declared (`+1`) or no longer (`-1`).
    pub traits: Vec<(TraitId, Weight)>,
    /// The names of modules whose meaning changed.
    pub bindings: Vec<(ModuleId, Name)>,
    /// The items and impls of the modules whose source was written or whose log changed: their
    /// headers and signatures may differ with no binding changed.
    pub touched: Vec<ModuleDefId>,
    pub touched_impls: Vec<ImplId>,
}

/// The maintainer's reads, two steps per layer of the crate graph (each crate one past its
/// deepest dependency), a job per crate on the database's workers: first each crate of the
/// layer reads its round-0 logs and runs its region to the cutoff on `&Db`, and the apply
/// publishes its scopes; then each makes its name records over the scopes published, and the
/// apply publishes its resolution. Its dependencies, all in earlier layers, are published by
/// then; a crate in its own layer it never reads.
fn read_step(db: &Db, _: &Written<'_>, reads: &mut Reads) {
    let step = reads.step() as usize;
    let at = step / 2;
    let more = db.lend_state::<LastWrite, _>(|last| {
        db.lend_state::<State, _>(|state| {
            if step == 0 {
                last.crates.clear();
                state.pass.modules_changed.clear();
                state.pass.scoped.clear();
                layer(db, &mut state.pass.layers);
            }
            if step % 2 == 1 {
                // The layer's records, over the scopes its first step published.
                reads.each(Tag::Nameres, std::mem::take(&mut state.pass.scoped), read_records);
                return at + 1 < state.pass.layers.len();
            }
            let Some(layer) = state.pass.layers.get_mut(at) else { return false };
            let layer = std::mem::take(layer);
            let runs: Vec<CrateRun> =
                layer.iter().map(|&krate| CrateRun::new(db, krate, state, &last.crates)).collect(); // alloc: a layer's jobs, once a step
            state.pass.layers[at] = layer;
            reads.each(Tag::Nameres, runs, read_crate);
            true
        })
    });
    if more {
        reads.again();
    }
}

/// The maintainer's apply: each crate of the step's layer publishes what its job ran, in
/// dependency order, and hands the impls maintainer what changed ([`LastWrite`]).
fn apply_step(db: &mut Db, written: &Written<'_>, reads: &Reads) {
    db.with_state::<LastWrite, _>(|db, last| {
        db.with_state::<State, _>(|db, state| {
            if reads.step() == 0 && written.crates {
                drop_gone(db, state);
            }
            for run in reads.take::<CrateRun>() {
                finish_crate(db, state, &mut last.crates, run);
            }
            for scoped in reads.take::<Scoped>() {
                last.crates.push(finish_records(db, state, scoped));
            }
        })
    });
}

/// The crates of the graph by layer: a crate is one past its deepest dependency.
fn layer(db: &Db, layers: &mut Vec<Vec<Crate>>) {
    for layer in layers.iter_mut() {
        layer.clear();
    }
    let crates = base_db::all_crates(db);
    // alloc: each crate's layer, once per write
    let mut depth: HashMap<Crate, usize> = HashMap::default();
    depth.reserve(crates.len());
    for &krate in crates.iter() {
        let at = depth_of(db, krate, &mut depth);
        if layers.len() <= at {
            layers.resize_with(at + 1, Vec::new);
        }
        layers[at].push(krate);
    }
    while layers.last().is_some_and(Vec::is_empty) {
        layers.pop();
    }
}

/// `krate`'s layer, by a walk of its dependencies: `all_crates` keeps its order across a graph
/// change that keeps its crates, so a crate may come there before one it now depends on.
fn depth_of(db: &Db, krate: Crate, depth: &mut HashMap<Crate, usize>) -> usize {
    if let Some(&at) = depth.get(&krate) {
        return at;
    }
    let mut at = 0;
    for dep in krate.data(db).dependencies.iter() {
        at = at.max(depth_of(db, dep.crate_id, depth) + 1);
    }
    depth.insert(krate, at);
    at
}

/// Drops the state of every crate no longer in the graph, and its published inputs.
fn drop_gone(db: &mut Db, state: &mut State) {
    let crates = base_db::all_crates(db);
    let live: HashSet<Crate> = crates.iter().copied().collect(); // alloc: a crate graph change, once
    let gone: Vec<Crate> = state.crates.keys().copied().filter(|it| !live.contains(it)).collect(); // alloc: as above
    for krate in gone {
        state.crates.remove(&krate);
        publish::enter(db, krate, None);
    }
}

/// One crate's job in a step: its state out of [`State`] while the job runs on `&Db`, and what
/// the job ran for the apply to publish.
struct CrateRun {
    krate: Crate,
    /// Served by the frozen image: no job, and the apply drops what the crate had.
    served: bool,
    /// The state before the write, if the crate had one and no crate graph change dropped it.
    old: Option<CrateState>,
    /// Whether a crate before this one changed a binding or a module in this write.
    deps_changed: bool,
    /// The groups that read what an earlier crate's write changed.
    wakes: Vec<ModuleId>,
    scratch: Scratch,
    work: Work,
    ran: Option<Ran>,
}

/// What a crate's job ran: its state, the delta it read, and the cutoff of its region.
struct Ran {
    cs: CrateState,
    cold: bool,
    delta: Delta,
    moved: Vec<ModuleId>,
    region: HashSet<ModuleId>,
    cutoff: Option<Cutoff>,
    /// An edit of the root's `#![...]` alone wakes no group, and changes the crate data.
    attrs_changed: bool,
}

impl CrateRun {
    /// `krate`'s job, its state taken out of `state`, with what the crates applied before it
    /// (`earlier`, and the modules they changed) woke of it.
    fn new(db: &Db, krate: Crate, state: &mut State, earlier: &[CrateDelta]) -> CrateRun {
        let served = frozen::is_served(db, krate);
        let mut old = state.crates.remove(&krate);
        let modules_changed = &state.pass.modules_changed;
        let deps_changed =
            !modules_changed.is_empty() || earlier.iter().any(|it| it.fresh || !it.bindings.is_empty());
        let mut wakes = Vec::new(); // alloc: empty, allocates nothing
        if let Some(cs) = &mut old
            && !served
        {
            wakes = dependency_wakes(db, cs, earlier, modules_changed);
            for it in earlier {
                for &def in &it.touched {
                    wake_enum_globs(db, &cs.reads, &mut cs.variants_seen, def, &mut wakes);
                }
            }
        }
        CrateRun {
            krate,
            served,
            old,
            deps_changed,
            wakes,
            scratch: state.pass.scratch.pop().unwrap_or_default(),
            work: Work::default(),
            ran: None,
        }
    }
}

/// A crate's job: its round-0 logs read, and its region run to the cutoff.
fn read_crate(db: &Db, written: &Written<'_>, run: &mut CrateRun) {
    if run.served {
        return;
    }
    let krate = run.krate;
    carry_maintaining();
    let _maintaining = Maintaining { before: replace_maintaining(Some(krate)) };
    let root = queries::crate_root(db, krate);
    let (cold, mut cs) = match run.old.take() {
        Some(it) if !written.crates && it.root == root => (false, it),
        old => (true, CrateState::cold(root, old)),
    };
    let work = &mut run.work;
    let mut delta = input::read(db, krate, &mut cs, written, cold, run.deps_changed);
    work.ops += delta.ops;
    // The root of a crate exporting procedural macros is published censored, and its state row
    // keeps the scope before the censor: a run makes both.
    let data = queries::crate_data(db, krate);
    if data.proc_macro
        && let Some(at) = delta.direct.iter().position(|(it, _)| *it == cs.root)
    {
        let (group, _) = delta.direct.swap_remove(at);
        delta.seeds.push(group);
    }
    // A crate attribute reaches any module's run, with no round-0 log changed: `#![feature]`
    // (trait item imports), `#![register_tool]`, `#![recursion_limit]`, and `#![crate_type]`
    // the root's censor. A write that changes one runs every group again.
    if cs.data_of.as_ref().is_some_and(|it| it != data) {
        delta.direct.clear();
        let seeded: HashSet<ModuleId> = delta.seeds.iter().copied().collect(); // alloc: once per write that changes a crate attribute
        delta.seeds.extend(cs.groups.keys().filter(|it| !seeded.contains(*it)));
        delta.removing.clear();
        delta.removing.extend(cs.groups.keys());
    }
    let moved = match delta.tree {
        true => diff::update_positions(&mut cs),
        false => Vec::new(), // alloc: empty, allocates nothing
    };
    let mut wakes = std::mem::take(&mut run.wakes);
    enum_glob_wakes(db, &mut cs, &delta, &mut wakes);
    let mut region = initial_region(&cs, &mut run.scratch, &delta, wakes, cold);
    if region.is_empty() && !delta.deleted.is_empty() {
        // The rows and inputs of a group gone go with a run, which reports their keys.
        region.insert(cs.root);
    }
    let cutoff = match region.is_empty() {
        true => None,
        false => {
            let rows = std::mem::replace(&mut cs.rows, ModulesMap::new());
            let scratch = &mut run.scratch;
            let deleted = &delta.deleted;
            let last = blaze_mem::frame::task(Tag::Nameres, |frame| {
                cutoff(db, frame, krate, &cs, scratch, deleted, &mut region, rows, work)
            });
            work.region += region.len() as u64;
            Some(last)
        }
    };
    let attrs_changed = cs.data_of.as_ref() != Some(queries::crate_data(db, krate));
    run.ran = Some(Ran { cs, cold, delta, moved, region, cutoff, attrs_changed });
}

/// Publishes the scopes of what `run`'s job ran, for its records step; or, for a crate whose
/// write ran nothing, puts its state back with what the write touched into `last`.
fn finish_crate(db: &mut Db, state: &mut State, last: &mut Vec<CrateDelta>, run: CrateRun) {
    let CrateRun { krate, served, old, work, ran, scratch, .. } = run;
    state.pass.scratch.push(scratch);
    if served {
        if old.is_some() {
            publish::enter(db, krate, None);
        }
        return;
    }
    let Ran { cs, cold, delta, moved, region, cutoff, attrs_changed } =
        ran.unwrap_or_else(|| panic!("nameres delta: crate {krate:?}'s job did not run"));
    carry_maintaining();
    let _maintaining = Maintaining { before: replace_maintaining(Some(krate)) };
    if cutoff.is_none() && delta.direct.is_empty() && moved.is_empty() && !attrs_changed {
        last.extend(touched_only(db, krate, &cs, &delta));
        state.crates.insert(krate, cs);
        db.report_work(NAME, krate, work);
        return;
    }
    let scoped = publish_scopes(db, krate, cs, delta, cutoff, &region, &moved, cold, work);
    state.pass.scoped.push(scoped);
}

thread_local! {
    static MAINTAINING: std::cell::Cell<Option<Crate>> = const { std::cell::Cell::new(None) };
}

/// Carries [`MAINTAINING`] with each job on a fiber across its waits ([`blaze_query::carry`]):
/// a maintainer run that waits on a query resumes maintaining its crate, wherever it resumes.
fn carry_maintaining() {
    static CARRIED: std::sync::Once = std::sync::Once::new();
    CARRIED.call_once(|| {
        blaze_query::carry(blaze_query::Carried {
            take: || stdx::lend::take_carried_cell(&MAINTAINING, Option::is_none),
            put: |it| stdx::lend::put_carried_cell(&MAINTAINING, Option::is_none, it),
        });
    });
}

/// Restores the crate maintained before when it drops.
struct Maintaining {
    before: Option<Crate>,
}

impl Drop for Maintaining {
    fn drop(&mut self) {
        replace_maintaining(self.before);
    }
}

// Each access to `MAINTAINING` is a call of its own, never inlined: a maintainer run that waits
// between two may resume on another thread, and an inlined access may reuse the address of the
// first thread's slot (docs/v2/plan/lanes/may-suspend.md).

#[inline(never)]
fn replace_maintaining(krate: Option<Crate>) -> Option<Crate> {
    MAINTAINING.replace(krate)
}

/// The crate the maintainer is resolving on this thread, if any.
#[inline(never)]
pub(in crate::nameres) fn maintaining() -> Option<Crate> {
    MAINTAINING.get()
}

/// The groups of this crate that read a binding an earlier crate's write changed.
fn dependency_wakes(
    db: &Db,
    cs: &CrateState,
    earlier: &[CrateDelta],
    modules_changed: &[ModuleId],
) -> Vec<ModuleId> {
    let mut out = Vec::new(); // alloc: the groups woken across crates, once per write
    for delta in earlier {
        if delta.fresh {
            // Every module of that crate may be new: each group that read any is woken.
            for &group in cs.groups.keys() {
                if cs.module_readers.keys(group).any(|it| it.krate(db) == delta.krate) {
                    out.push(group);
                }
            }
            continue;
        }
        for &(module, name) in &delta.bindings {
            out.extend(cs.reads.readers((module, Some(name))));
            out.extend(cs.reads.readers((module, None)));
        }
    }
    for &module in modules_changed {
        out.extend(cs.reads.readers((module, None)));
    }
    out
}

/// Into `out`, the groups that glob-imported an enum declared by a group of this crate the
/// write read again; one an earlier crate's write touched, [`CrateRun::new`] wakes. A variant
/// is no binding of the enum's module, so nothing else wakes them; the glob is noted reading
/// [`reads::enum_glob`] there, which no glob of the module reads. The same for the imports of a
/// trait's items ([`reads::trait_import`]).
fn enum_glob_wakes(db: &Db, cs: &mut CrateState, delta: &Delta, out: &mut Vec<ModuleId>) {
    let direct = delta.direct.iter().map(|(it, _)| it);
    for group in delta.touched.iter().chain(&delta.seeds).chain(direct) {
        let Some(g) = cs.groups.get(group) else { continue };
        for module in &g.modules {
            let Some(row) = cs.rows.get(module) else { continue };
            for def in row.scope(db).declarations() {
                wake_enum_globs(db, &cs.reads, &mut cs.variants_seen, def, out);
            }
        }
    }
}

/// The groups that glob-imported `def`, if it is an enum whose variants differ from those they
/// last woke on (`seen`), or imported its items, if it is a trait ([`reads::trait_import`]).
fn wake_enum_globs(
    db: &Db,
    index: &ReadIndex<Key, ModuleId>,
    seen: &mut HashMap<EnumId, u64>,
    def: ModuleDefId,
    out: &mut Vec<ModuleId>,
) {
    match def {
        ModuleDefId::AdtId(crate::AdtId::EnumId(it)) => {
            let loc = crate::Lookup::lookup(&it, db);
            let read = (loc.container, Some(reads::enum_glob()));
            if index.readers(read).next().is_none() {
                // A reader that comes later reads the variants then; the next touch wakes it.
                seen.remove(&it);
                return;
            }
            // The rows are as of the last run: an enum the write removed is still declared
            // there, has no syntax to read variants from, and its globs bind nothing now.
            if loc.id.file_id.ast_id_map(db).try_get_erased(loc.id.value.erase()).is_none() {
                seen.remove(&it);
                out.extend(index.readers(read));
                return;
            }
            let print = variants_print(db, it);
            if seen.insert(it, print) != Some(print) {
                out.extend(index.readers(read));
            }
        }
        ModuleDefId::TraitId(it) => {
            out.extend(index.readers((crate::Lookup::lookup(&it, db).container, Some(reads::trait_import()))));
        }
        _ => {}
    }
}

/// What a glob of `it` binds: each variant's name, id and shape, in order.
fn variants_print(db: &Db, it: EnumId) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = blaze_hash::FixedHasher::default();
    for (name, &(variant, shape)) in &it.enum_variants(db).variants {
        (name, variant, shape).hash(&mut hasher);
    }
    hasher.finish()
}

/// The groups the write runs first: every one when cold; else the changed and new ones, and
/// the readers of what a removal, a group gone or a dependency's change may have taken away,
/// transitively.
fn initial_region(
    cs: &CrateState,
    scratch: &mut Scratch,
    delta: &Delta,
    wakes: Vec<ModuleId>,
    cold: bool,
) -> HashSet<ModuleId> {
    let mut region = HashSet::default();
    if cold {
        region.extend(cs.groups.keys().copied());
        return region;
    }
    region.extend(delta.seeds.iter().copied());
    let mut redo = wakes;
    redo.extend(delta.removing.iter().copied());
    for (_, group) in &delta.deleted {
        for &module in &group.modules {
            redo.extend(cs.module_readers.readers(module));
        }
    }
    over_delete(cs, scratch, &redo, &mut region);
    if delta.root_ops {
        region.insert(cs.root);
    }
    region.retain(|it| cs.groups.contains_key(it));
    region
}

/// The root runs with every group whose expansions write its scope, and those with it.
fn close_root(cs: &CrateState, region: &mut HashSet<ModuleId>) {
    let writers = || cs.groups.iter().filter(|(_, it)| it.out.writes_root).map(|(&it, _)| it);
    if !region.contains(&cs.root) && !writers().any(|it| region.contains(&it)) {
        return;
    }
    region.insert(cs.root);
    region.extend(writers());
}

/// What the last run of the cutoff loop made, with the rows it replaced.
struct Cutoff {
    out: RunOut,
    /// The rows before the write of the region's modules and of the groups gone.
    old: HashMap<ModuleId, ModuleData>,
    /// Every other row.
    outside: ModulesMap,
    changed: Vec<Changed>,
    /// The region, in group order.
    order: Vec<ModuleId>,
}

#[allow(clippy::too_many_arguments)]
fn cutoff(
    db: &Db,
    frame: &Frame,
    krate: Crate,
    cs: &CrateState,
    scratch: &mut Scratch,
    deleted: &[(ModuleId, Group)],
    region: &mut HashSet<ModuleId>,
    mut rows: ModulesMap,
    work: &mut Work,
) -> Cutoff {
    let root = cs.root;
    loop {
        close_root(cs, region);
        let order = region_order(cs, region);
        let (outside, old) = split(cs, rows, &order, deleted, work);
        let (out, outside) = run::run(db, frame, krate, cs, &order, outside);
        work.rebuilt += out.rows.len() as u64;

        let every = order.len() == cs.groups.len();
        let widen = if out.missed_root && !region.contains(&root) {
            region.insert(root);
            true
        } else if !every && (out.env != cs.env || out.prelude != cs.prelude) {
            // The crate env is read by every path and noted by none.
            region.extend(cs.groups.keys().copied());
            true
        } else {
            false
        };
        if widen {
            rows = join(outside, old, work);
            continue;
        }

        let mut changed = Vec::new(); // alloc: the keys the run changed, once per run
        for (&module, row) in &old {
            diff::keys(db, module, Some(row), out.rows.get(&module), &mut changed);
        }
        for (module, row) in out.rows.iter() {
            if !old.contains_key(&module) {
                diff::keys(db, module, None, Some(row), &mut changed);
            }
        }
        // Each reader once: a group noted under a name and under the module whole, or woken by
        // an addition and a removal, counts once, as a removal's reader.
        let (mut woken, mut redo) = (HashSet::default(), HashSet::default()); // alloc: the readers outside the region, once per run
        for &((module, name), present) in &changed {
            let into = if present { &mut redo } else { &mut woken };
            readers_outside(cs, region, (module, name), into);
            if name.is_some() {
                readers_outside(cs, region, (module, None), into);
            }
        }
        woken.retain(|it| !redo.contains(it));
        if woken.is_empty() && redo.is_empty() {
            return Cutoff { out, old, outside, changed, order };
        }
        work.woken += (woken.len() + redo.len()) as u64;
        region.extend(woken);
        let redo: Vec<ModuleId> = redo.into_iter().collect(); // alloc: as above
        over_delete(cs, scratch, &redo, region);
        region.retain(|it| cs.groups.contains_key(it));
        rows = join(outside, old, work);
    }
}

/// The region in group order.
fn region_order(cs: &CrateState, region: &HashSet<ModuleId>) -> Vec<ModuleId> {
    let mut order: Vec<ModuleId> = region.iter().copied().collect(); // alloc: the region, once per run
    order.sort_unstable_by_key(|it| cs.groups.get_index_of(it));
    order
}

fn readers_outside(cs: &CrateState, region: &HashSet<ModuleId>, key: Key, into: &mut HashSet<ModuleId>) {
    into.extend(cs.reads.readers(key).filter(|it| !region.contains(it) && cs.groups.contains_key(it)));
}

/// The rows of the region's modules and of the groups gone taken out of `rows`, which is then
/// what a run of the region reads: each a swap with the table's last row, so a run moves rows
/// as many as its region has, whatever the crate's size.
fn split(
    cs: &CrateState,
    mut rows: ModulesMap,
    order: &[ModuleId],
    deleted: &[(ModuleId, Group)],
    work: &mut Work,
) -> (ModulesMap, HashMap<ModuleId, ModuleData>) {
    let mut old = HashMap::default();
    let ran = order.iter().flat_map(|group| &cs.groups[group].modules);
    for &module in ran.chain(deleted.iter().flat_map(|(_, group)| &group.modules)) {
        if let Some(row) = rows.swap_remove(&module) {
            work.moved += 1;
            old.insert(module, row);
        }
    }
    (rows, old)
}

fn join(mut outside: ModulesMap, old: HashMap<ModuleId, ModuleData>, work: &mut Work) -> ModulesMap {
    work.moved += old.len() as u64;
    outside.extend(old);
    outside
}

/// What a crate's first step published, for its second: the name records, which read the
/// scopes published, and the resolution made of them.
struct Scoped {
    krate: Crate,
    cs: CrateState,
    delta: Delta,
    cold: bool,
    proc_macro: bool,
    /// What the impls maintainer is handed, before: the items of every module the write replaced.
    old_items: Items,
    /// The groups whose items the impls maintainer is handed, now.
    emit: Vec<ModuleId>,
    changed: Vec<Changed>,
    /// The region in group order and what its run read, when one ran.
    ran: Option<(Vec<ModuleId>, Vec<Read>)>,
    rows_changed: bool,
    records_changed: bool,
    data_changed: bool,
    env_changed: bool,
    work: Work,
    /// The crate's resolution as the second step's reads made it, when a part of it changed.
    assembled: Option<publish::Assembled>,
}

/// The first step's apply of a crate whose write ran: every scope that changed set, the
/// groups' outputs and the crate data put in its state, and the name-free inserts of the
/// groups that did not run.
#[allow(clippy::too_many_arguments)]
fn publish_scopes(
    db: &mut Db,
    krate: Crate,
    mut cs: CrateState,
    delta: Delta,
    cutoff: Option<Cutoff>,
    region: &HashSet<ModuleId>,
    moved: &[ModuleId],
    cold: bool,
    mut work: Work,
) -> Scoped {
    // clone: a count of the crate's attributes, held across the writes to `db` below
    let attrs = queries::crate_data(db, krate).clone();
    let proc_macro = attrs.proc_macro;
    let root = cs.root;
    let mut old_items = Items::default();
    let mut changed = Vec::new(); // alloc: empty, allocates nothing without a run
    let mut order = Vec::new(); // alloc: as above
    let mut ran_reads = None;
    // Which parts of the crate's resolution the write may have changed.
    let mut rows_changed = cold || delta.tree;
    // A cold run keeps the old state's inputs, not what they were made from.
    let mut records_changed = cold || !delta.deleted.is_empty();
    let mut data_changed = cold
        || cs.data.is_none()
        || cs.data_of.as_ref() != Some(&attrs)
        || delta.deleted.iter().any(|(_, it)| !it.out.crate_ops.is_empty());
    let mut env_changed = false;

    if let Some(Cutoff { out: mut run, old, mut outside, changed: keys, order: ran }) = cutoff {
        for row in old.values() {
            old_items.add(row.scope(db), &Pos::root());
        }

        // Each scope the run made, set where it changed, and its row put back.
        let mut new_rows = std::mem::replace(&mut run.rows, ModulesMap::new());
        for (module, row) in new_rows.iter_mut() {
            let ScopeSlot::Building(building) = &mut row.scope else {
                unreachable!("nameres delta: a run's row is `ScopeSlot::Building`")
            };
            // A crate exporting procedural macros exports nothing else: its root is published
            // censored, and its own paths read the scope before the censor, which the state keeps.
            let censored = module == root && proc_macro;
            let BuildingScope { mut scope, children } = match censored {
                // clone: the proc-macro root's scope, of which the state keeps the uncensored one
                true => BuildingScope::clone(building),
                false => std::mem::take(&mut **building),
            };
            if censored {
                scope.censor_non_proc_macros(krate);
            }
            let mut slot = cs.inputs.get(&module).copied();
            work.slices += u64::from(publish::set_scope(db, &mut slot, module, scope, children));
            let input = slot.expect("nameres delta: `set_scope` makes the input");
            cs.inputs.insert(module, input);
            if !censored {
                row.scope = ScopeSlot::Input(publish::scope_id(input));
            }
        }
        for (module, row) in new_rows.drain(..) {
            rows_changed |= old.get(&module) != Some(&row);
            outside.insert(module, row);
            work.moved += 1;
        }
        rows_changed |= old.keys().any(|it| !outside.contains_key(it));
        cs.rows = outside;

        // The groups' modules and outputs.
        for (group, gone) in &delta.deleted {
            for module in gone.modules.iter().filter(|it| !run.group_of.contains_key(it)) {
                cs.inputs.remove(module);
                cs.group_of.remove(module);
            }
            cs.reads.forget(*group);
            cs.module_readers.forget(*group);
        }
        for &group in &ran {
            let (out, modules) = run.outs.remove(&group).unwrap_or_default();
            let g = cs.groups.get_mut(&group).expect("nameres delta: the region is of the crate's groups");
            for module in &g.modules {
                if !run.group_of.contains_key(module) {
                    cs.inputs.remove(module);
                    cs.group_of.remove(module);
                }
            }
            records_changed |= g.out != out;
            data_changed |= g.out.crate_ops != out.crate_ops;
            rows_changed |= g.modules != modules;
            g.out = out;
            g.modules = modules;
        }
        cs.group_of.extend(run.group_of.iter().map(|(&module, &group)| (module, group)));

        env_changed = run.env != cs.env || run.prelude != cs.prelude;
        cs.env = run.env;
        cs.prelude = run.prelude;
        cs.prelude_holder = run.prelude_holder;
        if data_changed {
            cs.data = Some(assemble::data(db, krate, &cs));
            cs.data_of = Some(attrs.clone()); // clone: the run keeps its own for the rows after
        }
        changed = keys;
        // clone: the region's groups, once per write: the records step reads them again
        order.clone_from(&ran);
        ran_reads = Some((ran, std::mem::take(&mut run.reads)));
    } else if data_changed {
        cs.data = Some(assemble::data(db, krate, &cs));
        cs.data_of = Some(attrs.clone()); // clone: a count of the crate's attributes
    }

    // The name-free inserts of the groups that did not run.
    let mut emit: Vec<ModuleId> = order;
    for (group, places) in &delta.direct {
        if !region.contains(group) {
            apply_direct(db, &cs, *group, places, &mut old_items, &mut work);
            emit.push(*group);
        }
    }
    emit.extend(moved.iter().copied().filter(|it| !region.contains(it)));
    emit.sort_unstable_by_key(|it| cs.groups.get_index_of(it));
    emit.dedup();
    Scoped {
        krate,
        cs,
        delta,
        cold,
        proc_macro,
        old_items,
        emit,
        changed,
        ran: ran_reads,
        rows_changed,
        records_changed,
        data_changed,
        env_changed,
        work,
        assembled: None,
    }
}

/// The second step's job of a crate: each group of the region's share of the records, on the
/// crate as its first step published it, and the crate's resolution where a part changed.
fn read_records(db: &Db, _: &Written<'_>, scoped: &mut Scoped) {
    let Scoped { krate, cs, ran, rows_changed, records_changed, data_changed, env_changed, proc_macro, assembled, .. } =
        scoped;
    let krate = *krate;
    carry_maintaining();
    let _maintaining = Maintaining { before: replace_maintaining(Some(krate)) };
    if let Some((ran, run_reads)) = ran {
        // What a group's share reads is noted as its own, so a key that changes later wakes it
        // like its run's reads.
        let local = cs.env.local_def_map();
        let rows = std::mem::replace(&mut cs.rows, ModulesMap::new());
        let view = assemble::view(krate, cs, rows);
        let shares = assemble::shares(db, cs, &view, &local, ran, run_reads);
        cs.rows = assemble::rows_of(view);
        for (&group, share) in ran.iter().zip(shares) {
            let g = cs.groups.get_mut(&group).expect("nameres delta: the region is of the crate's groups");
            *records_changed |= g.records != share;
            g.records = share;
        }

        // Who read what, the region's reads replacing its old ones. A group's reads of its own
        // modules are not kept: it runs whenever they change. An enum's variants and a trait's
        // items are in no round-0 log, so an edit of them only touches the group, and its own
        // globs of them are kept for `enum_glob_wakes`.
        for &group in ran.iter() {
            cs.reads.forget(group);
            cs.module_readers.forget(group);
        }
        let (enum_glob, trait_import) = (reads::enum_glob(), reads::trait_import());
        for &(reader, (module, name)) in run_reads.iter() {
            let owner = cs.group_of.get(&module).copied();
            let item_read = name.as_ref().is_some_and(|it| *it == enum_glob || *it == trait_import);
            let mut note = |group: ModuleId| {
                if owner != Some(group) {
                    cs.reads.add((module, name), group);
                    cs.module_readers.add(module, group);
                } else if item_read {
                    cs.reads.add((module, name), group);
                }
            };
            match reader.and_then(|it| cs.group_of.get(&it).copied()) {
                Some(group) => note(group),
                None => ran.iter().for_each(|&it| note(it)),
            }
        }
    }

    if cs.resolution.is_none() || *rows_changed || *records_changed || *data_changed || *env_changed {
        let root = cs.root;
        let root_input = proc_macro.then(|| publish::scope_id(cs.inputs[&root]));
        let (data, session_macros) = cs.data.as_ref().expect("nameres delta: the crate data is made with a run");
        *assembled = Some(publish::Assembled {
            root,
            modules: (*rows_changed || cs.resolution.is_none()).then(|| assemble::modules(cs, root_input)),
            local: cs.env.local_def_map(),
            // clone: one count of the crate's shared data
            data: data.clone(),
            prelude: cs.prelude,
            macro_use_prelude: assemble::macro_use_prelude(krate, cs),
            session_macros: *session_macros,
            records: (*records_changed || cs.resolution.is_none()).then(|| assemble::records(db, cs)),
        });
    }
}

/// The second step's apply of a crate: its resolution set, its state put back, and what changed
/// of it for the impls maintainer.
fn finish_records(db: &mut Db, state: &mut State, scoped: Scoped) -> CrateDelta {
    let Scoped { krate, mut cs, delta, cold, old_items, emit, changed, mut work, assembled, .. } = scoped;
    carry_maintaining();
    let _maintaining = Maintaining { before: replace_maintaining(Some(krate)) };
    if let Some(assembled) = assembled {
        let mut slot = cs.resolution;
        work.slices += publish::set_resolution(db, krate, &mut slot, assembled);
        cs.resolution = slot;
    }
    work.keys += changed.len() as u64;

    let mut crate_delta = touched_only(db, krate, &cs, &delta).unwrap_or_else(|| empty_delta(krate));
    crate_delta.fresh = cold;
    let mut new_items = Items::default();
    for group in &emit {
        let pos = cs.group_pos.get(group).expect("nameres delta: every group has a position");
        diff::items_of(db, &cs.rows, &cs.groups[group].modules, pos, &mut new_items);
    }
    let CrateDelta { impls, derive_impls, traits, .. } = &mut crate_delta;
    diff::items(&mut cs, &old_items, new_items, impls, derive_impls, traits);
    for &((module, name), _) in &changed {
        match name {
            Some(name) => crate_delta.bindings.push((module, name)),
            None => state.pass.modules_changed.push(module),
        }
    }
    state.crates.insert(krate, cs);
    db.report_work(NAME, krate, work);
    crate_delta
}

/// Puts a group's name-free inserts (`places` in its new log) into its module's published
/// scope, where a run would put them: a round-0 op of a list is replayed before any expansion
/// adds to it, so its index is the number of the same ops before it in the log.
fn apply_direct(db: &mut Db, cs: &CrateState, group: ModuleId, places: &[u32], old_items: &mut Items, work: &mut Work) {
    let input = *cs.inputs.get(&group).unwrap_or_else(|| panic!("nameres delta: no scope input of {group:?}"));
    // clone: the module's scope, which the inserts change, once per module a write inserts into
    let mut scope = ItemScope::clone(input.scope(db));
    old_items.add(&scope, &Pos::root());
    let log = &cs.groups[&group].log;
    for &at in places {
        let at = at as usize;
        let before = &log[..at];
        let index = |same: fn(&DeclOp) -> bool| before.iter().filter(|op| same(op) && op.target() == Some(group)).count();
        match log[at] {
            DeclOp::DefineImpl { module, imp, is_trait_impl, is_reservation } if module == group => {
                let at = index(|op| matches!(op, DeclOp::DefineImpl { .. }));
                insert_impl(&mut scope, at, imp, is_trait_impl, is_reservation);
            }
            DeclOp::DefineBuiltinDeriveImpl { module, imp } if module == group => {
                let at = index(|op| matches!(op, DeclOp::DefineBuiltinDeriveImpl { .. }));
                scope.builtin_derive_impls.insert(at, imp);
            }
            DeclOp::DefineUnnamedConst { module, id } if module == group => {
                let at = index(|op| matches!(op, DeclOp::DefineUnnamedConst { .. }));
                scope.unnamed_consts.insert(at, id);
            }
            DeclOp::DefineExternBlock { module, id } if module == group => {
                let at = index(|op| matches!(op, DeclOp::DefineExternBlock { .. }));
                scope.extern_blocks.insert(at, id);
            }
            // An op of another module, which the group's run does not replay either.
            _ => {}
        }
    }
    work.slices += u64::from(publish::set_scope_alone(db, input, scope));
}

/// `scope` with `imp` at `at` of its impls, its bits as `define_impl` in order makes them.
fn insert_impl(scope: &mut ItemScope, at: usize, imp: ImplId, is_trait_impl: bool, is_reservation: bool) {
    let tail: Vec<(ImplId, bool, bool)> = (at..scope.impls.len())
        .map(|it| (scope.impls[it], scope.trait_impl.contains(it), scope.reservation_impl.contains(it)))
        .collect(); // alloc: the impls after `at`, once per impl inserted
    scope.impls.truncate(at);
    scope.define_impl(imp, is_trait_impl, is_reservation);
    for (imp, is_trait_impl, is_reservation) in tail {
        scope.define_impl(imp, is_trait_impl, is_reservation);
    }
}

/// The crate's def map as a maintainer from empty makes it, outside the database's state and
/// its published inputs: the cold run, which the tests compare a maintained map with. Its
/// scopes are being built (`ModulesSlot::Building`), its records in the map.
#[cfg(test)]
fn cold(db: &Db, frame: &Frame, krate: Crate) -> (crate::nameres::DefMap, crate::nameres::LocalDefMap) {
    use crate::nameres::{DefMap, DiagnosticsByModule, ModulesSlot, OwnedRecords};

    let root = queries::crate_root(db, krate);
    let mut cs = CrateState::new(root);
    let none = Written { set: &[], added: &[], removed: &[], roots: &[], crates: true, included: &[], proc_macros: &[] };
    input::read(db, krate, &mut cs, &none, true, false);
    let mut region: HashSet<ModuleId> = cs.groups.keys().copied().collect();
    let mut work = Work::default();
    let Cutoff { mut out, mut outside, order, .. } =
        cutoff(db, frame, krate, &cs, &mut Scratch::default(), &[], &mut region, ModulesMap::new(), &mut work);
    for &group in &order {
        let (share, modules) = out.outs.remove(&group).unwrap_or_default();
        let g = cs.groups.get_mut(&group).expect("nameres delta: the region is of the crate's groups");
        g.out = share;
        g.modules = modules;
    }
    cs.group_of = std::mem::take(&mut out.group_of);
    outside.extend(out.rows.drain(..));
    cs.rows = outside;
    cs.env = out.env;
    cs.prelude = out.prelude;
    cs.data = Some(assemble::data(db, krate, &cs));
    cs.data_of = Some(queries::crate_data(db, krate).clone()); // clone: a count of the crate's attributes

    let local = cs.env.local_def_map();
    let rows = std::mem::replace(&mut cs.rows, ModulesMap::new());
    let view = assemble::view(krate, &cs, rows);
    let shares = assemble::shares(db, &cs, &view, &local, &order, &mut Vec::new());
    cs.rows = assemble::rows_of(view);
    for (&group, share) in order.iter().zip(shares) {
        cs.groups.get_mut(&group).expect("nameres delta: the region is of the crate's groups").records = share;
    }
    let OwnedRecords { names, diagnostics, derive_helpers_in_scope, .. } = assemble::records(db, &cs);
    let mut modules = cs.ordered_rows();
    if queries::crate_data(db, krate).proc_macro {
        modules[root].scope_mut().censor_non_proc_macros(krate);
    }
    let macro_use_prelude = assemble::macro_use_prelude(krate, &cs);
    let (data, session_macros) = cs.data.take().expect("nameres delta: made above");
    let def_map = DefMap {
        krate,
        block: None,
        root,
        modules: ModulesSlot::Building(modules),
        prelude: cs.prelude,
        macro_use_prelude,
        derive_helpers_in_scope,
        session_macros,
        names,
        diagnostics,
        diagnostics_by_module: DiagnosticsByModule::default(),
        records: None,
        data,
    };
    (def_map, local)
}

fn empty_delta(krate: Crate) -> CrateDelta {
    CrateDelta {
        krate,
        fresh: false,
        impls: Vec::new(),         // alloc: empty, allocates nothing
        derive_impls: Vec::new(),  // alloc: as above
        traits: Vec::new(),        // alloc: as above
        bindings: Vec::new(),      // alloc: as above
        touched: Vec::new(),       // alloc: as above
        touched_impls: Vec::new(), // alloc: as above
    }
}

/// The items and impls of the groups the write read again (a file of theirs written, or their
/// log changed), as the state has them: `None` when there are none.
fn touched_only(db: &Db, krate: Crate, cs: &CrateState, delta: &Delta) -> Option<CrateDelta> {
    let direct = delta.direct.iter().map(|(it, _)| it);
    let mut groups: Vec<ModuleId> = delta.touched.iter().chain(&delta.seeds).chain(direct).copied().collect(); // alloc: the groups a write read again
    groups.sort_by_key(|it| cs.groups.get_index_of(it));
    groups.dedup();
    let mut out = empty_delta(krate);
    for group in groups {
        let Some(g) = cs.groups.get(&group) else { continue };
        for module in &g.modules {
            let Some(row) = cs.rows.get(module) else { continue };
            let scope = row.scope(db);
            out.touched.extend(scope.declarations());
            out.touched_impls.extend(scope.impls());
        }
    }
    (!out.touched.is_empty() || !out.touched_impls.is_empty()).then_some(out)
}
