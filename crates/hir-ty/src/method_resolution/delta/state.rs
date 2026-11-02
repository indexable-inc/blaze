//! The maintainer's state and one write's run (design §7, §8): the impls held per crate with
//! their keys, the arrangements the slices are published from, and the read sets that say
//! which keys a write may have changed.
//!
//! A run goes over the crates not frozen. For each it applies name resolution's delta (impls
//! and derives added or removed, traits declared or gone), recomputes the keys of the impls the
//! write woke and the body parts of the modules it may have changed the bodies of, publishes
//! the slices whose rows changed, and hands the next maintainer what changed ([`ImplsDelta`]).
//! A key or body part is recomputed through the pure queries, so recomputing one that did not
//! change costs memo validations and changes nothing. What a run reads, the maintainer's reads
//! read first, across the crates on the database's workers ([`warm`], then [`warm_bodies`]):
//! the run diffs memos.
//!
//! A crate's work reads only its own state, the memos and what the run's start fixed (which
//! impls the write woke, and whose they were), so the crates are worked on at once on the
//! database's workers ([`Db::run_each`]), each into its own [`CrateState`] and [`CrateScratch`].
//! What they change of the state shared across crates (the read indexes, the impls' owners)
//! and of the database (the slices) is applied after, one crate at a time in the crates' order,
//! so no result depends on which worker ran which crate. Every new crate's directory is
//! registered with one write at the end: a write opens a view, and a view sweeps every `lru`
//! table, so a write per crate was a sweep per crate.

use base_db::{Crate, Db, Written};
use blaze_dbsp::{Arrangement, Pos, ReadIndex, Work};
use blaze_hash::{HashMap, HashSet};
use blaze_mem::Tag;
use std::alloc::Global;
use hir_def::{
    BuiltinDeriveImplId, HasModule, ImplId, ModuleDefId, ModuleId, TraitId,
    builtin_derive::BuiltinDeriveImplTrait,
    lang_item::lang_items,
    nameres::{
        crate_def_map,
        delta::{CrateDelta, LastWrite},
        module_impls,
    },
};
use hir_expand::name::Name;
use intern::Symbol;
use smallvec::SmallVec;

use super::{
    ImplsDelta,
    head::Head,
    key::{ImplKey, Reads, assoc_names, derive_key, derive_module, impl_key, impl_reads},
    publish::{self, CandidateRows, InherentRows, Published, SliceDirectory, TraitRows},
    read::{BodyPart, body_part},
};
use crate::method_resolution::{frozen, macros_writing_impls};

/// What the impls maintainer reports its work under ([`Db::report_work`]).
pub(crate) const NAME: &str = "impls";

/// A row of a trait's arrangement: an impl with its head, or a built-in derive impl.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum TraitEntry {
    Impl(ModuleId, ImplId, Option<Head>),
    Derive(ModuleId, BuiltinDeriveImplId, Head),
}

struct ImplRow {
    module: ModuleId,
    pos: Pos,
    key: ImplKey,
    trait_impl: bool,
}

struct DeriveRow {
    module: ModuleId,
    pos: Pos,
    key: Option<(TraitId, Head)>,
}

/// One crate's impls and traits, and its published inputs.
#[derive(Default)]
pub(crate) struct CrateState {
    impls: HashMap<ImplId, ImplRow, Global>,
    derives: HashMap<BuiltinDeriveImplId, DeriveRow, Global>,
    pub(crate) traits: Arrangement<TraitId, Pos, TraitEntry>,
    pub(crate) heads: Arrangement<Head, Pos, (ModuleId, ImplId)>,
    /// Each trait declared in the crate's modules with its item names (`AssocFn`).
    assoc: HashMap<TraitId, SmallVec<[Name; 8]>, Global>,
    /// Traits whose items a macro call writes: their names may change with no edit of theirs.
    wild_traits: HashSet<TraitId, Global>,
    pub(crate) candidates: Arrangement<Name, TraitId, TraitId>,
    /// The traits the crate's lang items give each built-in derive, by
    /// `BuiltinDeriveImplTrait::ALL` index, once a derive impl asked.
    lang: Option<SmallVec<[Option<TraitId>; 16]>>,
    /// The body part of each module whose bodies may hold an impl.
    bodies: HashMap<ModuleId, BodyPart, Global>,
    /// The modules whose bodies hold impls of each trait, of each head, and (under `()`) any
    /// inherent impl.
    body_traits: Arrangement<TraitId, ModuleId, ModuleId>,
    body_heads: Arrangement<Head, ModuleId, ModuleId>,
    body_inherent: Arrangement<(), ModuleId, ModuleId>,
    /// What `macros_writing_impls` gave the last write: which macro calls may write an impl
    /// in a body, so a change of it reads every module's bodies again.
    macros: Option<Box<[(Symbol, bool)]>>,
    published: Published,
    /// The crate's buffers, kept from one write to the next.
    scratch: CrateScratch,
}

/// Every crate's state, and the read sets across crates: an alias edited in a dependency wakes
/// the impls of its dependents that read it.
#[derive(Default)]
pub struct State {
    crates: HashMap<Crate, CrateState, Global>,
    owner: HashMap<ImplId, Crate, Global>,
    by_name: ReadIndex<Name, ImplId>,
    by_def: ReadIndex<ModuleDefId, ImplId>,
    wild: HashSet<ImplId, Global>,
    scratch: Scratch,
}

/// The run's buffers across crates, kept from one write to the next.
#[derive(Default)]
struct Scratch {
    deltas: HashMap<Crate, usize, Global>,
    names: HashSet<Name, Global>,
    defs: HashSet<ModuleDefId, Global>,
    woken: HashSet<ImplId, Global>,
    jobs: Vec<Job>,
    directories: Vec<(Crate, SliceDirectory)>,
}

/// One crate's buffers, kept from one write to the next: what its work in a run uses, and what
/// it hands the run's serial end ([`finish_crate`]).
#[derive(Default)]
struct CrateScratch {
    reads: Reads,
    done: HashSet<ImplId, Global>,
    kinds: HashMap<ImplId, bool, Global>,
    kinds_of: HashSet<ModuleId, Global>,
    woken: Vec<ImplId>,
    trait_keys: Vec<TraitId>,
    head_keys: Vec<Head>,
    name_keys: Vec<Name>,
    redo_traits: Vec<TraitId>,
    assoc: SmallVec<[Name; 8]>,
    body_modules: Vec<ModuleId>,
    units: Vec<()>,
    keys_changed: Vec<ImplId>,
    traits_changed: Vec<TraitId>,
    /// What the work did to the state shared across crates, in order: replayed by
    /// [`finish_crate`]. A `Note`'s names and defs are the next ones of `read_names` and
    /// `read_defs`.
    index: Vec<IndexOp>,
    read_names: Vec<Name>,
    read_defs: Vec<ModuleDefId>,
    /// The slices whose rows the work made, set by [`finish_crate`] unless they hold them.
    trait_rows: Vec<(TraitId, TraitRows)>,
    head_rows: Vec<(Head, InherentRows)>,
    name_rows: Vec<(Name, CandidateRows)>,
    /// The directory's body modules, when the work read them again.
    inherent_bodies: Option<Vec<ModuleId>>,
}

impl CrateScratch {
    /// Empties what one run's work fills.
    fn start(&mut self) {
        self.done.clear();
        self.kinds.clear();
        self.kinds_of.clear();
        self.keys_changed.clear();
        self.traits_changed.clear();
        debug_assert!(self.index.is_empty() && self.trait_rows.is_empty());
    }
}

/// A change to the state shared across crates, as a crate's work made it.
#[derive(Clone, Copy)]
enum IndexOp {
    /// `forget_reads`: the impl's read set dropped.
    Forget(ImplId),
    /// The impl is no longer the crate's.
    Disown(ImplId),
    /// The impl is the crate's.
    Own(ImplId),
    /// The impl's read set, `names` names and `defs` defs, and whether it is wild.
    Note { imp: ImplId, names: u32, defs: u32, wild: bool },
}

/// One crate's work in a run: its state out of [`State::crates`] while the run has it.
struct Job {
    krate: Crate,
    /// Its delta, by index into the write's `LastWrite::crates`.
    delta: Option<usize>,
    fresh: bool,
    crate_state: CrateState,
    scratch: CrateScratch,
    work: Work,
}

/// What every crate's work reads that is not its own: fixed at the run's start.
struct Shared<'a> {
    last: &'a LastWrite,
    owner: &'a HashMap<ImplId, Crate, Global>,
    woken: &'a HashSet<ImplId, Global>,
    crates_changed: bool,
    changed: bool,
}

impl State {
    /// Whether `krate` is maintained here.
    #[cfg(test)]
    pub(crate) fn crate_state(&self, krate: Crate) -> Option<&CrateState> {
        self.crates.get(&krate)
    }
}

/// One write's run over every crate; what changed of each, into `out`, in the crates' order.
pub(crate) fn run(
    db: &mut Db,
    state: &mut State,
    last: &LastWrite,
    written: &Written<'_>,
    out: &mut Vec<ImplsDelta>,
) {
    let crates = base_db::all_crates(&*db);
    if written.crates {
        drop_gone(state, &crates);
    }
    let scratch = &mut state.scratch;
    scratch.deltas.clear();
    scratch.names.clear();
    scratch.defs.clear();
    scratch.woken.clear();
    for (at, delta) in last.crates.iter().enumerate() {
        scratch.deltas.insert(delta.krate, at);
        scratch.names.extend(delta.bindings.iter().map(|&(_, name)| name));
        scratch.defs.extend(delta.touched.iter().copied());
        scratch.woken.extend(delta.touched_impls.iter().copied());
    }
    for name in &scratch.names {
        scratch.woken.extend(state.by_name.readers(*name));
    }
    for def in &scratch.defs {
        scratch.woken.extend(state.by_def.readers(*def));
    }
    let changed = !scratch.names.is_empty() || !scratch.defs.is_empty();
    if changed {
        scratch.woken.extend(state.wild.iter().copied());
    }
    let mut jobs = std::mem::take(&mut state.scratch.jobs);
    jobs.clear();
    for &krate in crates.iter() {
        if frozen::served(&*db, krate) {
            continue;
        }
        let delta = state.scratch.deltas.get(&krate).copied();
        jobs.push(start_crate(state, last, krate, delta));
    }
    {
        let _span = tracing::info_span!(target: "blaze", "impls work", count = jobs.len()).entered();
        let shared = Shared {
            last,
            owner: &state.owner,
            woken: &state.scratch.woken,
            crates_changed: written.crates,
            changed,
        };
        db.run_each(Tag::Infer, &mut jobs, |db, job| work_crate(db, &shared, job));
    }
    let mut directories = std::mem::take(&mut state.scratch.directories);
    directories.clear();
    for job in jobs.drain(..) {
        finish_crate(db, state, job, &mut directories, out);
    }
    {
        let _span =
            tracing::info_span!(target: "blaze", "impls register", count = directories.len()).entered();
        publish::register(db, &directories);
    }
    directories.clear();
    state.scratch.directories = directories;
    state.scratch.jobs = jobs;
}

/// Takes `krate`'s state out for its work, forgetting what it held first when name resolution
/// maintains it from empty (`fresh`).
fn start_crate(state: &mut State, last: &LastWrite, krate: Crate, delta: Option<usize>) -> Job {
    let fresh = delta.is_some_and(|at| last.crates[at].fresh);
    let Some(mut crate_state) = state.crates.remove(&krate) else {
        if !fresh {
            panic!(
                "impls: crate {krate:?} has no maintained impl tables and name resolution handed \
                 it no fresh delta"
            );
        }
        let mut scratch = CrateScratch::default();
        scratch.start();
        return Job { krate, delta, fresh, crate_state: CrateState::default(), scratch, work: Work::default() };
    };
    let mut scratch = std::mem::take(&mut crate_state.scratch);
    scratch.start();
    if fresh {
        crate_state = reset(state, crate_state, &mut scratch);
        let (traits, heads, names) = crate_state.published.keys();
        scratch.trait_keys.extend(traits);
        scratch.head_keys.extend(heads);
        scratch.name_keys.extend(names);
    }
    Job { krate, delta, fresh, crate_state, scratch, work: Work::default() }
}

/// Drops the state of every crate no longer in the graph, with its impls' read sets.
fn drop_gone(state: &mut State, crates: &[Crate]) {
    // alloc: a crate graph change, once
    let live: HashSet<Crate, Global> = crates.iter().copied().collect();
    let gone: Vec<Crate> =
        state.crates.keys().copied().filter(|it| !live.contains(it)).collect(); // alloc: as above
    for krate in gone {
        let Some(crate_state) = state.crates.remove(&krate) else { continue };
        for imp in crate_state.impls.keys() {
            forget_reads(state, *imp);
            state.owner.remove(imp);
        }
    }
}

fn forget_reads(state: &mut State, imp: ImplId) {
    state.by_name.forget(imp);
    state.by_def.forget(imp);
    state.wild.remove(&imp);
}

/// One crate's work: its delta applied, its woken impls rekeyed, its traits' names and its
/// bodies read again, and the rows of the slices that changed made. It reads memos and its own
/// state alone, so the crates' works run at once.
fn work_crate(db: &Db, shared: &Shared<'_>, job: &mut Job) {
    let krate = job.krate;
    let _span = tracing::info_span!(
        target: "blaze",
        "impls crate",
        krate = hir_def::span_names::crate_name(db, krate)
    )
    .entered();
    macro_rules! phase {
        ($name:literal, $run:expr) => {{
            let _phase = tracing::info_span!(target: "blaze", $name).entered();
            $run
        }};
    }
    let Job { delta, fresh, crate_state, scratch, work, .. } = job;
    let (fresh, crates_changed, changed) = (*fresh, shared.crates_changed, shared.changed);
    let delta = delta.map(|at| &shared.last.crates[at]);
    if let Some(delta) = delta {
        phase!("impls apply_impls", apply_impls(db, scratch, crate_state, delta, work));
        phase!("impls apply_derives", apply_derives(db, crate_state, delta, work));
        phase!("impls apply_traits", apply_traits(db, scratch, crate_state, delta, work));
    }
    phase!("impls rekey", rekey(db, shared, scratch, crate_state, krate, crates_changed, work));
    if crates_changed || changed {
        phase!("impls rename_wild_traits", rename_wild_traits(db, scratch, crate_state, work));
    }
    let lang_check = fresh || crates_changed || delta.is_some_and(touches_traits);
    phase!("impls relang", relang(db, crate_state, krate, lang_check, work));
    let sweep = fresh || crates_changed;
    if sweep || changed || delta.is_some() {
        apply_bodies(db, scratch, crate_state, krate, delta, sweep, work);
    }
    phase!("impls rows", rows(scratch, crate_state, fresh));
}

/// The serial end of one crate's work, in the crates' order: what it changed of the shared
/// state replayed, its slices set, its state put back, its delta handed on, its work reported,
/// and its directory, when it is the crate's first, kept for [`publish::register`].
fn finish_crate(
    db: &mut Db,
    state: &mut State,
    job: Job,
    directories: &mut Vec<(Crate, SliceDirectory)>,
    out: &mut Vec<ImplsDelta>,
) {
    let Job { krate, mut crate_state, mut scratch, mut work, .. } = job;
    let _span = tracing::info_span!(
        target: "blaze",
        "impls publish",
        krate = hir_def::span_names::crate_name(&*db, krate)
    )
    .entered();
    replay(state, &mut scratch, krate);
    if let Some(directory) = publish(db, &mut scratch, &mut crate_state, krate, &mut work) {
        directories.push((krate, directory));
    }
    if !scratch.keys_changed.is_empty() || !scratch.traits_changed.is_empty() {
        scratch.keys_changed.sort_unstable();
        scratch.keys_changed.dedup();
        scratch.traits_changed.sort_unstable();
        scratch.traits_changed.dedup();
        out.push(ImplsDelta {
            krate,
            // alloc: the columns move into the delta; the next write's grow anew
            keys_changed: std::mem::take(&mut scratch.keys_changed),
            traits_changed: std::mem::take(&mut scratch.traits_changed),
        });
    }
    crate_state.scratch = scratch;
    state.crates.insert(krate, crate_state);
    db.report_work(NAME, krate, work);
}

/// Applies a crate's changes to the state shared across crates, in the order its work made
/// them.
fn replay(state: &mut State, scratch: &mut CrateScratch, krate: Crate) {
    let (mut names, mut defs) = (0, 0);
    for op in scratch.index.drain(..) {
        match op {
            IndexOp::Forget(imp) => forget_reads(state, imp),
            IndexOp::Disown(imp) => _ = state.owner.remove(&imp),
            IndexOp::Own(imp) => _ = state.owner.insert(imp, krate),
            IndexOp::Note { imp, names: n, defs: d, wild } => {
                let (n, d) = (n as usize, d as usize);
                for &name in &scratch.read_names[names..names + n] {
                    state.by_name.add(name, imp);
                }
                for &def in &scratch.read_defs[defs..defs + d] {
                    state.by_def.add(def, imp);
                }
                if wild {
                    state.wild.insert(imp);
                }
                names += n;
                defs += d;
            }
        }
    }
    scratch.read_names.clear();
    scratch.read_defs.clear();
}

/// Forgets what a crate held, keeping its published inputs and buffers: a crate maintained
/// from empty sets every slice it had anew, emptied where it has no rows now.
fn reset(state: &mut State, old: CrateState, scratch: &mut CrateScratch) -> CrateState {
    for imp in old.impls.keys() {
        forget_reads(state, *imp);
        state.owner.remove(imp);
    }
    scratch.keys_changed.extend(old.impls.keys().copied());
    scratch.traits_changed.extend(old.assoc.keys().copied());
    CrateState { published: old.published, ..CrateState::default() }
}

fn touches_traits(delta: &CrateDelta) -> bool {
    !delta.traits.is_empty() || delta.touched.iter().any(|it| matches!(it, ModuleDefId::TraitId(_)))
}

/// Whether `imp` is a trait impl, by its module's scope: the split the tables take.
fn is_trait_impl(db: &Db, scratch: &mut CrateScratch, module: ModuleId, imp: ImplId) -> bool {
    if scratch.kinds_of.insert(module) {
        let impls = module_impls(db, module);
        scratch.kinds.extend(impls.trait_impls.iter().map(|&it| (it, true)));
        scratch.kinds.extend(impls.inherent_impls.iter().map(|&it| (it, false)));
    }
    *scratch.kinds.get(&imp).unwrap_or_else(|| {
        panic!("impls: {imp:?} is not among the impls of its module {module:?}")
    })
}

fn apply_impls(
    db: &Db,
    scratch: &mut CrateScratch,
    crate_state: &mut CrateState,
    delta: &CrateDelta,
    work: &mut Work,
) {
    for (imp, pos, _) in delta.impls.iter().filter(|it| it.2 < 0) {
        work.ops += 1;
        if pos_of(crate_state, *imp).is_some_and(|it| it == pos)
            && remove_impl(scratch, crate_state, *imp)
        {
            scratch.keys_changed.push(*imp);
        }
    }
    for (imp, pos, _) in delta.impls.iter().filter(|it| it.2 > 0) {
        work.ops += 1;
        remove_impl(scratch, crate_state, *imp);
        scratch.keys_changed.push(*imp);
        let module = imp.loc(db).container;
        let trait_impl = is_trait_impl(db, scratch, module, *imp);
        let key = impl_key(db, *imp, trait_impl);
        work.rebuilt += 1;
        work.keys += 1;
        // clone: a position, inline to a depth of six
        let row = ImplRow { module, pos: pos.clone(), key, trait_impl };
        arrange(crate_state, *imp, &row, 1);
        crate_state.impls.insert(*imp, row);
        scratch.index.push(IndexOp::Own(*imp));
        note_reads(db, scratch, *imp);
        scratch.done.insert(*imp);
    }
}

fn pos_of(crate_state: &CrateState, imp: ImplId) -> Option<&Pos> {
    crate_state.impls.get(&imp).map(|row| &row.pos)
}

/// Drops `imp`'s row and read set; whether it had one.
fn remove_impl(scratch: &mut CrateScratch, crate_state: &mut CrateState, imp: ImplId) -> bool {
    let Some(row) = crate_state.impls.remove(&imp) else { return false };
    arrange(crate_state, imp, &row, -1);
    scratch.index.push(IndexOp::Forget(imp));
    scratch.index.push(IndexOp::Disown(imp));
    true
}

/// Adds `row` to the arrangements its key is in, with `weight`.
fn arrange(crate_state: &mut CrateState, imp: ImplId, row: &ImplRow, weight: i32) {
    match row.key {
        ImplKey::Skip => {}
        ImplKey::Trait { trait_, head } => crate_state.traits.update(
            trait_,
            // clone: a position, inline to a depth of six
            row.pos.clone(),
            TraitEntry::Impl(row.module, imp, head),
            weight,
        ),
        ImplKey::Inherent(head) => {
            // clone: a position, inline to a depth of six
            crate_state.heads.update(head, row.pos.clone(), (row.module, imp), weight)
        }
    }
}

fn arrange_derive(
    crate_state: &mut CrateState,
    imp: BuiltinDeriveImplId,
    row: &DeriveRow,
    weight: i32,
) {
    if let Some((trait_, head)) = row.key {
        let entry = TraitEntry::Derive(row.module, imp, head);
        // clone: a position, inline to a depth of six
        crate_state.traits.update(trait_, row.pos.clone(), entry, weight);
    }
}

/// Notes what `imp`'s key read, for the read indexes ([`replay`]).
fn note_reads(db: &Db, scratch: &mut CrateScratch, imp: ImplId) {
    let reads = &mut scratch.reads;
    impl_reads(db, imp, reads);
    scratch.read_names.extend_from_slice(&reads.names);
    scratch.read_defs.extend_from_slice(&reads.defs);
    scratch.index.push(IndexOp::Note {
        imp,
        names: reads.names.len() as u32,
        defs: reads.defs.len() as u32,
        wild: reads.wild,
    });
}

/// Recomputes the key of every impl of `krate` the write woke (all of them when the crate
/// graph changed): a changed key moves its row. Which impls are `krate`'s is as the run found
/// them at its start: one this write added is keyed already (`done`), and one it removed has no
/// row.
fn rekey(
    db: &Db,
    shared: &Shared<'_>,
    scratch: &mut CrateScratch,
    crate_state: &mut CrateState,
    krate: Crate,
    all: bool,
    work: &mut Work,
) {
    let mut woken = std::mem::take(&mut scratch.woken);
    woken.clear();
    if all {
        woken.extend(crate_state.impls.keys().copied());
    } else {
        let owner = shared.owner;
        woken.extend(shared.woken.iter().copied().filter(|imp| owner.get(imp) == Some(&krate)));
    }
    for &imp in &woken {
        if !scratch.done.insert(imp) {
            continue;
        }
        let Some(row) = crate_state.impls.get(&imp) else { continue };
        work.woken += 1;
        work.rebuilt += 1;
        let trait_impl = is_trait_impl(db, scratch, row.module, imp);
        let key = impl_key(db, imp, trait_impl);
        scratch.index.push(IndexOp::Forget(imp));
        note_reads(db, scratch, imp);
        let row = crate_state.impls.remove(&imp).expect("the row just read");
        if key == row.key && trait_impl == row.trait_impl {
            crate_state.impls.insert(imp, row);
            continue;
        }
        work.keys += 1;
        scratch.keys_changed.push(imp);
        arrange(crate_state, imp, &row, -1);
        let row = ImplRow { key, trait_impl, ..row };
        arrange(crate_state, imp, &row, 1);
        crate_state.impls.insert(imp, row);
    }
    scratch.woken = woken;
}

fn apply_derives(db: &Db, crate_state: &mut CrateState, delta: &CrateDelta, work: &mut Work) {
    for (imp, pos, _) in delta.derive_impls.iter().filter(|it| it.2 < 0) {
        work.ops += 1;
        if crate_state.derives.get(imp).is_some_and(|row| row.pos == *pos)
            && let Some(row) = crate_state.derives.remove(imp)
        {
            arrange_derive(crate_state, *imp, &row, -1);
        }
    }
    for (imp, pos, _) in delta.derive_impls.iter().filter(|it| it.2 > 0) {
        work.ops += 1;
        if let Some(row) = crate_state.derives.remove(imp) {
            arrange_derive(crate_state, *imp, &row, -1);
        }
        let module = derive_module(db, *imp);
        let krate = module.krate(db);
        let key = derive_key(db, *imp, lang_items(db, krate));
        work.rebuilt += 1;
        work.keys += 1;
        // clone: a position, inline to a depth of six
        let row = DeriveRow { module, pos: pos.clone(), key };
        arrange_derive(crate_state, *imp, &row, 1);
        crate_state.derives.insert(*imp, row);
    }
}

/// Re-reads the traits the lang items give the built-in derives when a write may have changed
/// them (a trait declared, gone or re-read, or the crate graph); a changed one rekeys the
/// derive impls of that derive.
fn relang(db: &Db, crate_state: &mut CrateState, krate: Crate, check: bool, work: &mut Work) {
    if crate_state.derives.is_empty() || !(check || crate_state.lang.is_none()) {
        return;
    }
    let items = lang_items(db, krate);
    let now: SmallVec<[Option<TraitId>; 16]> =
        BuiltinDeriveImplTrait::ALL.iter().map(|it| it.get_id(items)).collect(); // alloc: inline
    if crate_state.lang.as_ref() == Some(&now) {
        return;
    }
    crate_state.lang = Some(now);
    // alloc: the derive impls of a crate whose lang items changed, once per such write
    let derives: Vec<BuiltinDeriveImplId> = crate_state.derives.keys().copied().collect();
    for imp in derives {
        let key = derive_key(db, imp, items);
        let row = crate_state.derives.remove(&imp).expect("a derive impl just listed");
        work.rebuilt += 1;
        if key == row.key {
            crate_state.derives.insert(imp, row);
            continue;
        }
        work.keys += 1;
        arrange_derive(crate_state, imp, &row, -1);
        let row = DeriveRow { key, ..row };
        arrange_derive(crate_state, imp, &row, 1);
        crate_state.derives.insert(imp, row);
    }
}

/// `AssocFn` (design §8): the item names of each trait declared, gone or re-read.
fn apply_traits(
    db: &Db,
    scratch: &mut CrateScratch,
    crate_state: &mut CrateState,
    delta: &CrateDelta,
    work: &mut Work,
) {
    for &(trait_, weight) in delta.traits.iter() {
        work.ops += 1;
        if weight < 0 {
            set_names(db, scratch, crate_state, trait_, false, work);
        }
    }
    for &(trait_, weight) in delta.traits.iter() {
        if weight > 0 {
            set_names(db, scratch, crate_state, trait_, true, work);
        }
    }
    scratch.redo_traits.clear();
    scratch.redo_traits.extend(delta.touched.iter().filter_map(|it| match *it {
        ModuleDefId::TraitId(it) => Some(it),
        _ => None,
    }));
    let redo = std::mem::take(&mut scratch.redo_traits);
    for &trait_ in &redo {
        if crate_state.assoc.contains_key(&trait_) {
            set_names(db, scratch, crate_state, trait_, true, work);
        }
    }
    scratch.redo_traits = redo;
}

/// The traits whose items a macro writes, re-read on a write that changed any binding or def.
fn rename_wild_traits(
    db: &Db,
    scratch: &mut CrateScratch,
    crate_state: &mut CrateState,
    work: &mut Work,
) {
    if crate_state.wild_traits.is_empty() {
        return;
    }
    scratch.redo_traits.clear();
    scratch.redo_traits.extend(crate_state.wild_traits.iter().copied());
    let redo = std::mem::take(&mut scratch.redo_traits);
    for &trait_ in &redo {
        work.woken += 1;
        set_names(db, scratch, crate_state, trait_, true, work);
    }
    scratch.redo_traits = redo;
}

/// Sets `trait_`'s names to its items' now (`present`) or to none, moving its candidate rows.
fn set_names(
    db: &Db,
    scratch: &mut CrateScratch,
    crate_state: &mut CrateState,
    trait_: TraitId,
    present: bool,
    work: &mut Work,
) {
    let new = &mut scratch.assoc;
    new.clear();
    if present {
        assoc_names(db, trait_, new);
        if trait_.trait_items(db).macro_calls.is_empty() {
            crate_state.wild_traits.remove(&trait_);
        } else {
            crate_state.wild_traits.insert(trait_);
        }
    } else {
        crate_state.wild_traits.remove(&trait_);
    }
    work.rebuilt += 1;
    let old = crate_state.assoc.remove(&trait_).unwrap_or_default();
    if old == *new {
        if present {
            crate_state.assoc.insert(trait_, old);
        }
        return;
    }
    work.keys += 1;
    scratch.traits_changed.push(trait_);
    for &name in old.iter().filter(|it| new.binary_search(it).is_err()) {
        crate_state.candidates.update(name, trait_, trait_, -1);
    }
    for &name in new.iter().filter(|it| old.binary_search(it).is_err()) {
        crate_state.candidates.update(name, trait_, trait_, 1);
    }
    if present {
        crate_state.assoc.insert(trait_, new.clone()); // clone: a trait's names, inline to eight
    }
}

/// The rows of every slice whose arrangement changed, and the directory's body modules when
/// they may have: what [`publish`] sets.
fn rows(scratch: &mut CrateScratch, crate_state: &mut CrateState, fresh: bool) {
    crate_state.traits.take_changed(&mut scratch.trait_keys);
    crate_state.body_traits.take_changed(&mut scratch.trait_keys);
    scratch.trait_keys.sort_unstable();
    scratch.trait_keys.dedup();
    crate_state.heads.take_changed(&mut scratch.head_keys);
    crate_state.body_heads.take_changed(&mut scratch.head_keys);
    scratch.head_keys.sort_unstable();
    scratch.head_keys.dedup();
    crate_state.candidates.take_changed(&mut scratch.name_keys);
    for trait_ in scratch.trait_keys.drain(..) {
        let rows = trait_rows(&crate_state.traits, &crate_state.body_traits, trait_);
        scratch.trait_rows.push((trait_, rows));
    }
    for head in scratch.head_keys.drain(..) {
        let rows = InherentRows {
            impls: crate_state
                .heads
                .rows(head)
                // clone: each row's position, inline to a depth of six
                .map(|(pos, (module, imp), _)| (module, imp, pos.clone()))
                // alloc: a changed slice's content, once at its length
                .collect(),
            // alloc: as above
            bodies: crate_state.body_heads.get(head).collect(),
        };
        scratch.head_rows.push((head, rows));
    }
    crate_state.body_inherent.take_changed(&mut scratch.units);
    if !scratch.units.is_empty() || fresh {
        scratch.units.clear();
        // alloc: the crate's inherent body modules, once per write that may have changed them
        scratch.inherent_bodies = Some(crate_state.body_inherent.get(()).collect());
    }
    for name in scratch.name_keys.drain(..) {
        let rows = CandidateRows {
            // alloc: a changed slice's content, once at its length
            traits: crate_state.candidates.get(name).collect(),
        };
        scratch.name_rows.push((name, rows));
    }
}

/// Sets every slice whose rows changed, then the directory's new keys and body modules; the
/// crate's first directory, which the run registers ([`publish::register`]).
fn publish(
    db: &mut Db,
    scratch: &mut CrateScratch,
    crate_state: &mut CrateState,
    krate: Crate,
    work: &mut Work,
) -> Option<SliceDirectory> {
    let published = &mut crate_state.published;
    for (trait_, rows) in scratch.trait_rows.drain(..) {
        work.slices += u64::from(published.set_trait(db, krate, trait_, rows));
    }
    for (head, rows) in scratch.head_rows.drain(..) {
        work.slices += u64::from(published.set_head(db, krate, head, rows));
    }
    if let Some(modules) = scratch.inherent_bodies.take() {
        published.set_body_modules(&modules);
    }
    for (name, rows) in scratch.name_rows.drain(..) {
        work.slices += u64::from(published.set_name(db, krate, name, rows));
    }
    let (set, made) = published.finish(db, krate);
    work.slices += set;
    made
}

/// The rows of `trait_` in position order and its body modules, as a slice holds them.
pub(crate) fn trait_rows(
    traits: &Arrangement<TraitId, Pos, TraitEntry>,
    bodies: &Arrangement<TraitId, ModuleId, ModuleId>,
    trait_: TraitId,
) -> TraitRows {
    let (mut impls, mut derives) = (0, 0);
    for entry in traits.get(trait_) {
        match entry {
            TraitEntry::Impl(..) => impls += 1,
            TraitEntry::Derive(..) => derives += 1,
        }
    }
    // alloc: a changed slice's content, each column once at its length
    let mut impl_rows = Vec::with_capacity(impls);
    // alloc: as above
    let mut derive_rows = Vec::with_capacity(derives);
    for entry in traits.get(trait_) {
        match entry {
            TraitEntry::Impl(module, imp, head) => impl_rows.push((module, imp, head)),
            TraitEntry::Derive(module, imp, head) => derive_rows.push((module, imp, head)),
        }
    }
    TraitRows {
        impls: impl_rows.into_boxed_slice(),
        derives: derive_rows.into_boxed_slice(),
        // alloc: as above
        bodies: bodies.get(trait_).collect(),
    }
}

/// The body parts of the modules a write may have changed the bodies of (design §7): the
/// modules of the defs and impls it touched (a file of theirs written, or their log changed)
/// and of its changed bindings (an item a macro writes), and every module whose bodies may hold
/// an impl, whose impls' paths or macros a change elsewhere may move. A `sweep` (the crate from
/// empty, a crate graph change) or a change of which macros may write impls reads every module.
/// A module no longer in the crate's `DefMap` has an empty part.
fn apply_bodies(
    db: &Db,
    scratch: &mut CrateScratch,
    crate_state: &mut CrateState,
    krate: Crate,
    delta: Option<&CrateDelta>,
    sweep: bool,
    work: &mut Work,
) {
    let _span = tracing::info_span!(target: "blaze", "apply_bodies", sweep).entered();
    let def_map = crate_def_map(db, krate);
    let macros = macros_writing_impls(db, krate);
    let sweep = sweep || crate_state.macros.as_deref() != Some(macros);
    if crate_state.macros.as_deref() != Some(macros) {
        // alloc: clone: the crate's macro names, once per write that changed them
        crate_state.macros = Some(macros.into());
    }
    let mut modules = std::mem::take(&mut scratch.body_modules);
    modules.clear();
    if sweep {
        modules.extend(def_map.modules(db).map(|(module, _)| module));
    } else if let Some(delta) = delta {
        modules.extend(delta.touched.iter().filter_map(|def| def.module(db)));
        modules.extend(delta.touched_impls.iter().map(|imp| imp.loc(db).container));
        modules.extend(delta.bindings.iter().map(|&(module, _)| module));
    }
    modules.extend(crate_state.bodies.keys().copied());
    modules.sort_unstable();
    modules.dedup();
    // The parts are the maintainer's second reads ([`warm_bodies`]): the loop diffs their
    // memos in module order.
    let empty = BodyPart::default();
    for &module in modules.iter() {
        let now = match def_map.module_rank(db, module) {
            Some(_) => body_part(db, module),
            None => &empty,
        };
        work.woken += 1;
        let old = crate_state.bodies.remove(&module).unwrap_or_default();
        if old == *now {
            if old.holds {
                crate_state.bodies.insert(module, old);
            }
            continue;
        }
        work.keys += 1;
        for (trait_, weight) in moved(&old.traits, &now.traits) {
            crate_state.body_traits.update(trait_, module, module, weight);
        }
        for (head, weight) in moved(&old.heads, &now.heads) {
            crate_state.body_heads.update(head, module, module, weight);
        }
        match (old.heads.is_empty(), now.heads.is_empty()) {
            (true, false) => crate_state.body_inherent.update((), module, module, 1),
            (false, true) => crate_state.body_inherent.update((), module, module, -1),
            _ => {}
        }
        if now.holds {
            // clone: alloc: a changed part, once per module whose part changed
            crate_state.bodies.insert(module, now.clone());
        }
    }
    scratch.body_modules = modules;
}

/// The keys of `old` not in `new` (`-1`) and of `new` not in `old` (`+1`), both sorted.
fn moved<'a, K: Ord + Copy>(old: &'a [K], new: &'a [K]) -> impl Iterator<Item = (K, i32)> + 'a {
    let gone = old.iter().filter(|it| new.binary_search(it).is_err()).map(|&it| (it, -1));
    gone.chain(new.iter().filter(|it| old.binary_search(it).is_err()).map(|&it| (it, 1)))
}

/// A key of the impls maintainer's reads: what its apply reads of it is memoized by then.
pub(crate) enum Warm {
    /// What a crate's run reads of the crate: its def map, the macros that may write an impl in
    /// a body, its lang items.
    Crate(Crate),
    /// An impl's kind, key and read set.
    Impl(ImplId),
    /// A built-in derive impl's key.
    Derive(BuiltinDeriveImplId),
    /// A trait's item names.
    Trait(TraitId),
    /// A module's body part, where its crate's def map has the module.
    Body(Crate, ModuleId),
}

/// Reads what the apply will read of `warm`.
pub(crate) fn warm_one(db: &Db, _: &Written<'_>, warm: &mut Warm) {
    match *warm {
        Warm::Crate(krate) => {
            _ = crate_def_map(db, krate);
            _ = macros_writing_impls(db, krate);
            _ = lang_items(db, krate);
            _ = hir_def::span_names::crate_name(db, krate);
        }
        Warm::Impl(imp) => {
            // An impl the write removed, or whose module it removed, is woken by what it read and
            // keyed by no run: a library crate's self types are one batch of the impls its
            // modules hold now, which has no row for it.
            let module = imp.loc(db).container;
            if module.def_map(db).module_rank(db, module).is_none() {
                return;
            }
            let impls = module_impls(db, module);
            let trait_impl = impls.trait_impls.contains(&imp);
            if !trait_impl && !impls.inherent_impls.contains(&imp) {
                return;
            }
            _ = impl_key(db, imp, trait_impl);
            impl_reads(db, imp, &mut Reads::default()); // alloc: an impl's read set, dropped with it
        }
        Warm::Derive(imp) => {
            // As an impl: one the write removed is keyed by no run.
            let module = derive_module(db, imp);
            if module.def_map(db).module_rank(db, module).is_none()
                || !module_impls(db, module).builtin_derive_impls.contains(&imp)
            {
                return;
            }
            _ = derive_key(db, imp, lang_items(db, module.krate(db)));
        }
        Warm::Trait(trait_) => {
            assoc_names(db, trait_, &mut SmallVec::new());
            _ = trait_.trait_items(db);
        }
        Warm::Body(krate, module) => {
            if crate_def_map(db, krate).module_rank(db, module).is_some() {
                _ = body_part(db, module);
            }
        }
    }
}

/// Each maintained crate's delta of `last`, by crate.
fn deltas(last: &LastWrite) -> HashMap<Crate, &CrateDelta, Global> {
    last.crates.iter().map(|it| (it.krate, it)).collect() // alloc: a crate's delta each, once per write
}

/// Whether the write changed a binding or re-read a def: what wakes the impls that read none
/// in particular (`wild`), and the traits a macro writes the items of.
fn changed(last: &LastWrite) -> bool {
    last.crates.iter().any(|it| !it.bindings.is_empty() || !it.touched.is_empty())
}

/// The first reads of a write ([`run`]'s, but for the body parts): each maintained crate, the
/// impls the write woke or added, the derive impls and traits it may rekey.
pub(crate) fn warm(db: &Db, state: &State, last: &LastWrite, written: &Written<'_>) -> Vec<Warm> {
    let deltas = deltas(last);
    let changed = changed(last);
    let mut impls: HashSet<ImplId, Global> = HashSet::new_in(Global); // alloc: the impls the write reads, once per write
    for delta in &last.crates {
        impls.extend(delta.touched_impls.iter().copied());
        for &(_, name) in &delta.bindings {
            impls.extend(state.by_name.readers(name));
        }
        for &def in &delta.touched {
            impls.extend(state.by_def.readers(def));
        }
    }
    if changed {
        impls.extend(state.wild.iter().copied());
    }
    let mut out = Vec::new(); // alloc: the keys the write reads, once per write
    for &krate in base_db::all_crates(db).iter() {
        if frozen::served(db, krate) {
            continue;
        }
        out.push(Warm::Crate(krate));
        let delta = deltas.get(&krate).copied();
        let crate_state = state.crates.get(&krate);
        if let Some(delta) = delta {
            impls.extend(delta.impls.iter().filter(|it| it.2 > 0).map(|it| it.0));
            out.extend(delta.derive_impls.iter().filter(|it| it.2 > 0).map(|it| Warm::Derive(it.0)));
            out.extend(delta.traits.iter().filter(|it| it.1 > 0).map(|it| Warm::Trait(it.0)));
            out.extend(delta.touched.iter().filter_map(|it| match *it {
                ModuleDefId::TraitId(it) => Some(Warm::Trait(it)),
                _ => None,
            }));
        }
        let Some(crate_state) = crate_state else { continue };
        if written.crates {
            impls.extend(crate_state.impls.keys().copied());
        }
        if written.crates || changed {
            // A trait the write removed leaves the wild ones before they are read again.
            let gone = |it: &TraitId| {
                delta.is_some_and(|delta| {
                    delta.traits.iter().any(|&(t, w)| t == *it && w < 0)
                        && !delta.traits.iter().any(|&(t, w)| t == *it && w > 0)
                })
            };
            out.extend(crate_state.wild_traits.iter().filter(|it| !gone(it)).map(|&it| Warm::Trait(it)));
        }
        let fresh = delta.is_some_and(|it| it.fresh);
        if fresh || written.crates || delta.is_some_and(touches_traits) || crate_state.lang.is_none() {
            out.extend(crate_state.derives.keys().map(|&it| Warm::Derive(it)));
        }
    }
    out.extend(impls.into_iter().map(Warm::Impl));
    out
}

/// The second reads of a write: the body parts of the modules each crate's run reads
/// ([`apply_bodies`]), by the def maps and macros the first reads read.
pub(crate) fn warm_bodies(db: &Db, state: &State, last: &LastWrite, written: &Written<'_>) -> Vec<Warm> {
    let deltas = deltas(last);
    let changed = changed(last);
    let mut out = Vec::new(); // alloc: the modules the write reads, once per write
    let mut modules = Vec::new(); // alloc: a crate's modules, the buffer reused across crates
    for &krate in base_db::all_crates(db).iter() {
        if frozen::served(db, krate) {
            continue;
        }
        let delta = deltas.get(&krate).copied();
        let fresh = delta.is_some_and(|it| it.fresh);
        let crate_state = state.crates.get(&krate);
        let sweep = fresh || written.crates;
        if !(sweep || changed || delta.is_some()) {
            continue;
        }
        let def_map = crate_def_map(db, krate);
        let macros = macros_writing_impls(db, krate);
        modules.clear();
        if sweep || crate_state.and_then(|it| it.macros.as_deref()) != Some(macros) {
            modules.extend(def_map.modules(db).map(|(module, _)| module));
        } else if let Some(delta) = delta {
            modules.extend(delta.touched.iter().filter_map(|def| def.module(db)));
            modules.extend(delta.touched_impls.iter().map(|imp| imp.loc(db).container));
            modules.extend(delta.bindings.iter().map(|&(module, _)| module));
        }
        if let Some(crate_state) = crate_state {
            modules.extend(crate_state.bodies.keys().copied());
        }
        // In module order, as the run diffs them: a test database reads them in this order.
        modules.sort_unstable();
        modules.dedup();
        out.extend(modules.iter().map(|&module| Warm::Body(krate, module)));
    }
    out
}
