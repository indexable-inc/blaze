//! A check as plane tasks: one per body, and one per file module for its items' checks
//! (docs/v2/plan/design/frontend.md).
//!
//! [`Plan::new`] lists the bodies the collector meets at each file module's level, in the order
//! it meets them. [`Plan::run`] is one batch of [`Db::run_all`]: a module's task walks the
//! module as [`check_module`](super::check_module) does but leaves each listed body, marking its
//! place; a body's task checks that body. Each task's diagnostics go in its own slot, and the
//! result puts each body's at its mark, so it is the collector's walk of each module whole, in
//! def-map order, whatever order the tasks ran in.
//!
//! A build tells the plan the bodies its codegen reaches as its walk finds them
//! ([`Plan::reached`]); every task checks those first, so they are never queued behind bodies
//! nothing reaches. Which bodies are checked does not change: the batch returns only once every
//! listed body was, as rustc checks every body of a crate it compiles.
//!
//! A plan of every body ([`Plan::of_every_body`]) keeps what each check found as the database's
//! memo: whether the body, or the module's items, gave no diagnostic ([`body_check_clean`],
//! [`module_check_clean`]). A build after an edit asks each memo, which the query engine
//! verifies against what the check read (the body, its inference, its borrow check, the
//! signatures it names) without running anything, and only a body one of whose reads changed
//! is checked again. A clean body's task then reads no inference result and no MIR: until
//! 2026-11-10 every task ran the whole check, which read each body's inference and MIR by
//! value, and since a build lets those values go when it ends (blaze-cargo's front end
//! release) a one-line edit of one function inferred every body of the crate again and built
//! its MIR again (ripgrep, `rg::main`: `infer_body_query` executed 1,165 with 2 changed,
//! `mir_body_query` 1,166 with 1 changed).
//!
//! Such a plan also keeps, beside the database, what it listed and which checks gave a
//! diagnostic ([`Kept`]), and the next plan of the same crates starts from the engine's changed
//! set since then (docs/v2/plan/design/incremental-walk.md, "The changed set from the
//! engine"): it holds a slot and posts a task only for the bodies and modules the engine names
//! (`changed_since` of [`body_check_clean`], [`module_check_clean`], [`module_bodies`]), those
//! that were not clean, and the bodies new in a module listed again. Every other body's and
//! module's answer is the kept one, clean, with no task and no fetch, so the check costs the
//! edit and the modules it touches, not the crate ([`Work`]). Where nothing is kept (the first
//! check of its key) or a crate's file modules changed, the plan is whole and says why
//! ([`Whole`]). Where a check is kept and the engine cannot name what changed since it, there
//! is no plan: that is an error naming the crates and the engine's reason ([`NoChangedSet`]),
//! never a sweep of every body in its place (rule no-fallback).

use std::{
    fmt, mem, ptr,
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
};

use base_db::{Db, Nonce};
use blaze_core::lock::Lock;
use blaze_query::db::{AsId, Changed, ChangedUnknown, Id, View};
use blaze_hash::{HashMap, HashSet};
use blaze_ids::bits::AtomicBits;
use blaze_mem::Tag;
use hir_def::{
    AdtId, DefWithBodyId, GenericDefId, ModuleDefId, ModuleId,
    nameres::{
        DefMap,
        assoc::{ImplItems, TraitItems},
        crate_def_map,
    },
};
use std::alloc::Global;

use crate::{
    AnyDiagnostic, Crate, DefWithBody, Module, TypeOwnerId, diagnostics::DiagnosticsCollector,
};

#[cfg(test)]
mod tests;

/// The sweep's loop, as the pool's small-loop predictor knows it: an edit's one or two tasks
/// run on the thread that runs the plan.
pub(crate) static SWEEP: blaze_query::db::Small = blaze_query::db::Small::new("body check sweep");

/// No body: an empty place of the reached ring, a task checking none.
const NONE: u32 = u32::MAX;

/// A task's entry in [`Plan::tasks`] that is a module's (the module's index in its low bits);
/// any other entry is a body's index.
const MODULE: u32 = 1 << 31;

/// A body the plan checks as a task of its own, and whose generics its types are written in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Planned {
    def: DefWithBodyId,
    owner: TypeOwnerId,
}

/// What a module's task gives the collector: the bodies of the module the plan checks as tasks
/// of their own, in the order the collector meets them, and the place in the module's
/// diagnostics of each it met.
pub(crate) struct Defer<'a> {
    expected: &'a [Planned],
    marks: &'a mut Vec<u32>,
}

impl Defer<'_> {
    /// Whether `def`, met when the module's diagnostics are `at` long, is the next body the plan
    /// checks on its own; then its place is marked and the collector leaves it. Any other body
    /// is the collector's to check where it is.
    pub(crate) fn take(&mut self, def: DefWithBodyId, at: usize) -> bool {
        match self.expected.get(self.marks.len()) {
            Some(next) if next.def == def => {
                self.marks
                    .push(u32::try_from(at).expect("a module of fewer than 2^32 diagnostics"));
                true
            }
            _ => false,
        }
    }
}

/// What a plan's run did, in order: the tests' view of its scheduling.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ran {
    /// The item checks of the plan's `n`th file module.
    Module(u32),
    Body(DefWithBodyId),
}

/// Why a plan holds every module and body of its crates and posts a task for each, where a
/// plan from a kept check holds what the edit touched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Whole {
    /// The plan keeps nothing ([`Plan::new`]): its `asked` may leave bodies out, and the kept
    /// answers are of a check of every body.
    KeepsNothing,
    /// Nothing is kept for this database, these crates and these style lints: the first plan
    /// of them in this process, or another plan of them has not run yet.
    NothingKept,
    /// The file modules of this crate changed: every position the kept check holds is of the
    /// list before.
    Modules(Crate),
}

impl fmt::Display for Whole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Whole::KeepsNothing => f.write_str("the plan keeps nothing"),
            Whole::NothingKept => {
                f.write_str("nothing is kept for this database, these crates and style lints")
            }
            Whole::Modules(krate) => write!(f, "the file modules of {krate:?} changed"),
        }
    }
}

/// Why no plan of every body could be made from the check kept of its crates: the query
/// engine could not name what changed since that check. A plan that swept every body in its
/// place would be right and cost the crate on every edit with nothing saying so, so this is
/// the plan's error (rule no-fallback); [`Plan::of_every_body`] panics with it, which fails
/// the unit whose check it is and names it (rule panics-fail-one-job). The kept check is put
/// back first, as it was.
///
/// What can cause it, each a bug of its caller and none an edit's: a plan made inside a
/// query while the engine's walk of the changed set is due (a walk starts only outside one),
/// or a kept view that is after the database's (a view counter set back).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoChangedSet {
    pub crates: Vec<Crate>,
    pub style_lints: bool,
    /// The view the kept check ran in, which the engine was asked from.
    pub since: View,
    pub why: ChangedUnknown,
}

impl fmt::Display for NoChangedSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the check of every body of {:?} (style lints: {}) holds a kept check of view {} \
             and the query engine cannot name what changed since it: {}. A plan from a kept \
             check starts from the engine's changed set and has no sweep of every body to \
             take its place (hir::check::bodies, rule no-fallback)",
            self.crates,
            self.style_lints,
            self.since.as_u64(),
            self.why
        )
    }
}

/// Why [`changes`] gives no patch of a kept check.
enum Unpatched {
    /// The plan is whole, by design, and says this.
    Whole(Whole),
    /// The engine cannot name the changed set: the plan's error.
    Unknown(ChangedUnknown),
}

/// What a plan holds and what it visits: on the spans `body check plan` and `body check
/// sweep`, and what a test holds to the edit. A regression shows here first: `visited_*` or
/// `slots_*` growing with the crate where the edit did not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Work {
    /// The file modules of the plan's crates, and the bodies they list.
    pub held_modules: u32,
    pub held_bodies: u32,
    /// The modules and bodies of the plan the engine named as changed since the kept check
    /// (`changed_since` of [`module_check_clean`] and [`body_check_clean`]); 0 in a whole plan.
    pub named_modules: u32,
    pub named_bodies: u32,
    /// The modules whose list of bodies is another than the kept one.
    pub relisted: u32,
    /// The modules and bodies the plan has a slot for: those of the modules it touches.
    pub slots_modules: u32,
    pub slots_bodies: u32,
    /// The tasks posted: the modules whose item checks run or are asked of the memo, and the
    /// bodies.
    pub visited_modules: u32,
    pub visited_bodies: u32,
    /// `None` for a plan from a kept check.
    pub whole: Option<Whole>,
}

impl Work {
    /// The modules and bodies whose kept answer, clean, is the plan's, with no task.
    pub fn kept_unvisited(&self) -> u32 {
        (self.held_modules - self.visited_modules) + (self.held_bodies - self.visited_bodies)
    }
}

/// A bug planted in a plan from a kept check, for the controls of its refinement test: each
/// is one safeguard taken out, which the oracle must catch.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Plant {
    /// This body gets no task even when the engine names it or it was not clean.
    Unvisited(DefWithBodyId),
    /// The bodies a run found not clean are kept as clean.
    KeepsNotClean,
}

/// What a kept check is of: its key. The database (its nonce: the IDs below are that
/// database's, and a view is one of its storage's), the crates in the plan's order, and the
/// style lints the checks ran with.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Of {
    database: Nonce,
    crates: Vec<Crate>,
    style_lints: bool,
}

/// What a plan of every body leaves for the next plan of the same key ([`Of`]): every file
/// module of the crates and the bodies each lists, and which module's and body's check gave a
/// diagnostic; every other one's gave none.
///
/// Why the key is complete (rule keys-cover-every-input). What a kept answer says is the value
/// of a query in view `view`: a crate's file modules ([`crate_file_modules`]), a module's
/// bodies ([`module_bodies`]), that a body's or a module's check is clean
/// ([`body_check_clean`], [`module_check_clean`], whose keys hold the style lints). The engine
/// records every read of each, so its changed set since `view` names every one of them whose
/// value is now another; the plan takes the kept answer only for one it does not name, and for
/// "clean" only: a body or module that was not clean is visited every time, since its
/// diagnostics are not kept. A row first made after `view` is not named, and there is none
/// among what is held: every module and body here had its queries asked by the plan that kept
/// it, in `view`. A database made again has another nonce, so no kept check.
///
/// Memory only: it names the database's IDs, and a restart begins with none (the first plan is
/// whole and says so, [`Whole::NothingKept`]).
struct Kept {
    /// The storage's view the plan that kept this ran in.
    view: View,
    /// Every file module of the crates, crate after crate, each in def-map order.
    modules: Vec<Module>,
    /// Where each crate's modules end in `modules`.
    crate_ends: Vec<u32>,
    /// Each module's listed bodies ([`module_bodies`]).
    lists: Vec<Vec<Planned>>,
    /// How many bodies `lists` holds.
    bodies: u32,
    /// Each module's place in `modules`, by the ID that is its key in the module queries.
    place: HashMap<Id, u32, Global>,
    /// The place in `modules` of the module that lists each body.
    home: HashMap<DefWithBodyId, u32, Global>,
    /// The modules whose item checks gave a diagnostic, by place, in order.
    not_clean_modules: Vec<u32>,
    /// The bodies whose check gave a diagnostic, in the plan's order.
    not_clean_bodies: Vec<DefWithBodyId>,
}

/// How many kept checks a process holds: the least recently used goes when one more is made.
/// A daemon's unit checks one crate, so a workspace of this many crates in one configuration
/// keeps them all; a database dropped leaves its kept checks here until others push them out.
const KEPT_CHECKS: usize = 64;

/// The kept checks, each with what it is of, the last used last. A plan takes its key's out
/// and its run puts the next in, so two plans of one key at once find one kept check between
/// them and the other is whole. Why here and not in the database: base-db cannot name hir's
/// IDs, and a table by the database's nonce is how the other hosts keep what is one
/// database's (hir-ty-codegen's kept walks).
static KEPT: Lock<Vec<(Of, Kept)>> = Lock::new("hir.check.kept", Vec::new());

/// Takes the kept check of `of` out, if there is one.
fn take_kept(of: &Of) -> Option<Kept> {
    let mut all = KEPT.lock();
    let at = all.iter().position(|(it, _)| it == of)?;
    Some(all.remove(at).1)
}

/// Keeps `kept` as the check of `of`, in place of any other of it.
fn put_kept(of: Of, kept: Kept) {
    let mut all = KEPT.lock();
    if let Some(at) = all.iter().position(|(it, _)| *it == of) {
        all.remove(at);
    } else if all.len() >= KEPT_CHECKS {
        all.remove(0);
    }
    // alloc: once a check, one entry per database, crates and style lints a process checks
    all.push((of, kept));
}

/// What a plan that keeps its check holds until its run: the key, and the kept check it was
/// made from with what it changes of it. Its run makes the next kept check of both; a plan
/// dropped before it ran puts the kept check back as it was, which its view still describes.
struct Carry {
    of: Of,
    /// `None` for a whole plan: the next kept check is made from the plan's own tables.
    last: Option<Last>,
}

struct Last {
    kept: Kept,
    /// The place in the kept check's modules of each module of the plan.
    places: Vec<u32>,
    /// The modules of the plan, by their index in it, whose list is another than the kept one.
    relisted: Vec<u32>,
}

/// What the engine's changed set and the kept check make of a plan: see [`changes`].
struct Patch<'db> {
    /// The modules the plan holds a slot for, by place in the kept check, in order: each one
    /// with a visited module check or body.
    touched: Vec<u32>,
    /// The modules whose item checks get a task, by place, in order.
    visit_modules: Vec<u32>,
    /// The bodies that get a task where a touched module lists them.
    visit_bodies: HashSet<DefWithBodyId, Global>,
    /// The modules whose list is another than the kept one, by place, in order, with the list.
    relisted: Vec<(u32, &'db [Planned])>,
    named_modules: u32,
    named_bodies: u32,
}

/// What a plan from `kept` visits: the modules and bodies the engine names as changed since
/// the kept view, those that were not clean, and the bodies new in a module whose list
/// changed. `Err` where a crate's file modules changed (a whole plan) or the engine cannot
/// say (no plan).
fn changes<'db>(db: &'db Db, of: &Of, kept: &Kept) -> Result<Patch<'db>, Unpatched> {
    let since = kept.view;
    let keys = |changed: Changed<Id>| match changed {
        Changed::Keys(ids) => Ok(ids),
        Changed::Unknown(why) => Err(Unpatched::Unknown(why)),
    };
    let crates = keys(crate_file_modules::changed_since(db, since))?;
    for &krate in &of.crates {
        if crates.binary_search(&AsId::as_id(&krate.id)).is_ok() {
            return Err(Unpatched::Whole(Whole::Modules(krate)));
        }
    }
    // alloc: once a plan, a word per module or body the edit touched or that was not clean
    let mut touched = Vec::new();
    // alloc: as above
    let mut visit_modules = Vec::new();
    let mut visit_bodies = HashSet::new_in(Global);
    // alloc: once a plan, an entry per module whose list changed
    let mut relisted = Vec::new();
    let (mut named_modules, mut named_bodies) = (0, 0);
    for id in keys(module_bodies::changed_since(db, since))? {
        // A module of another crate, or of a check with other crates: not held here.
        let Some(&at) = kept.place.get(&id) else { continue };
        let list: &'db [Planned] = module_bodies(db, kept.modules[at as usize].id);
        if list == &kept.lists[at as usize][..] {
            continue;
        }
        // A body the kept check does not hold in this module is new: it has no kept answer.
        for planned in list {
            if kept.home.get(&planned.def) != Some(&at) {
                visit_bodies.insert(planned.def);
            }
        }
        relisted.push((at, list));
        touched.push(at);
    }
    relisted.sort_unstable_by_key(|it| it.0);
    for id in keys(module_check_clean::changed_since(db, since))? {
        let Some((module, style_lints)) = module_check_clean::arguments(db, id) else { continue };
        if style_lints != of.style_lints {
            continue;
        }
        if let Some(&at) = kept.place.get(&AsId::as_id(&module)) {
            named_modules += 1;
            visit_modules.push(at);
        }
    }
    for id in keys(body_check_clean::changed_since(db, since))? {
        let Some((def, style_lints)) = body_check_clean::arguments(db, id) else { continue };
        if style_lints != of.style_lints {
            continue;
        }
        // A body with no home is another check's, one its module's task checks inline, or
        // a new one, which its module's new list brought in above.
        if let Some(&at) = kept.home.get(&def) {
            named_bodies += 1;
            visit_bodies.insert(def);
            touched.push(at);
        }
    }
    // What was not clean is visited whatever the engine says: its diagnostics are not kept.
    visit_modules.extend_from_slice(&kept.not_clean_modules);
    for def in &kept.not_clean_bodies {
        if let Some(&at) = kept.home.get(def) {
            visit_bodies.insert(*def);
            touched.push(at);
        }
    }
    visit_modules.sort_unstable();
    visit_modules.dedup();
    touched.extend_from_slice(&visit_modules);
    touched.sort_unstable();
    touched.dedup();
    Ok(Patch { touched, visit_modules, visit_bodies, relisted, named_modules, named_bodies })
}

/// A plan's tables: see the fields of [`Plan`].
struct Tables {
    modules: Vec<Module>,
    crate_ends: Vec<u32>,
    starts: Vec<u32>,
    bodies: Vec<Planned>,
    tasks: Vec<u32>,
    unvisited_modules: Vec<u32>,
    unvisited_bodies: Vec<u32>,
}

impl Tables {
    /// The tables of a plan from `kept`: the touched modules, each with its whole list, and a
    /// task for each module and body `patch` visits.
    fn patched(kept: &Kept, patch: &Patch<'_>) -> Tables {
        let mut tables = Tables {
            modules: Vec::with_capacity(patch.touched.len()),
            crate_ends: Vec::with_capacity(kept.crate_ends.len()),
            starts: Vec::with_capacity(patch.touched.len() + 1),
            // alloc: once a plan, the bodies of the touched modules
            bodies: Vec::new(),
            // alloc: once a plan, a word per task
            tasks: Vec::new(),
            // alloc: once a plan, a word per touched module with no task
            unvisited_modules: Vec::new(),
            // alloc: once a plan, a word per body of a touched module with no task
            unvisited_bodies: Vec::new(),
        };
        for (local, &at) in patch.touched.iter().enumerate() {
            let local = local as u32;
            let list: &[Planned] = match patch.relisted.binary_search_by_key(&at, |it| it.0) {
                Ok(found) => patch.relisted[found].1,
                Err(_) => &kept.lists[at as usize],
            };
            let first =
                u32::try_from(tables.bodies.len()).expect("a check of fewer than 2^31 bodies");
            tables.starts.push(first);
            tables.modules.push(kept.modules[at as usize]);
            match patch.visit_modules.binary_search(&at) {
                Ok(_) => tables.tasks.push(MODULE | local),
                Err(_) => tables.unvisited_modules.push(local),
            }
            for (nth, planned) in list.iter().enumerate() {
                let body = first + nth as u32;
                if patch.visit_bodies.contains(&planned.def) {
                    tables.tasks.push(body);
                } else {
                    tables.unvisited_bodies.push(body);
                }
            }
            tables.bodies.extend_from_slice(list);
        }
        let end = u32::try_from(tables.bodies.len()).ok().filter(|&it| it < MODULE);
        tables.starts.push(end.expect("a check of fewer than 2^31 bodies"));
        // The plan's modules are the touched ones in the kept order, so a crate's end among
        // them is how many are before its end among all.
        for &end in &kept.crate_ends {
            tables.crate_ends.push(patch.touched.partition_point(|&at| at < end) as u32);
        }
        tables
    }
}

/// The tasks of a check of some crates. See the module.
pub struct Plan<'f, F: Fn(DefWithBody) -> bool + Sync + ?Sized> {
    asked: &'f F,
    /// The file modules the plan holds a slot for, crate after crate, each in def-map order:
    /// every one of the plan's crates in a whole plan, the touched ones in a plan from a kept
    /// check ([`Work::whole`]).
    modules: Vec<Module>,
    /// Where each crate's modules end in `modules`.
    crate_ends: Vec<u32>,
    /// Where each module's bodies start in `bodies`, and one more: where the last one's end.
    starts: Vec<u32>,
    /// Every listed body of each of `modules`.
    bodies: Vec<Planned>,
    /// Each task's work, in the order the batch gives them: a module, then its bodies. Every
    /// module and body in a whole plan; the visited ones in a plan from a kept check.
    tasks: Vec<u32>,
    /// The modules and bodies with a slot and no task: their answer is the kept one, clean.
    unvisited_modules: Vec<u32>,
    unvisited_bodies: Vec<u32>,
    /// The index in `bodies` of each body with a task, made on the first [`Plan::reached`].
    #[expect(clippy::disallowed_types, reason = "body index, set once per check plan: not global")]
    index: std::sync::OnceLock<HashMap<DefWithBodyId, u32, Global>>,
    queue: Queue,
    started: AtomicBool,
    /// With the style lints the plan runs with: what a check found clean is kept and asked as
    /// the database's memo ([`body_check_clean`], [`module_check_clean`]), and the plan
    /// starts from and leaves a kept check ([`Kept`]). Only for a plan whose `asked` accepts
    /// every body, which is what the memos were checked with ([`Plan::of_every_body`]).
    keep: Option<bool>,
    /// The database and the view the plan was made in: a plan that keeps runs in them.
    at: (Nonce, View),
    carry: Lock<Option<Carry>>,
    work: Work,
    #[cfg(test)]
    ran: Lock<Vec<Ran>>,
}

impl Plan<'static, fn(DefWithBody) -> bool> {
    /// The check of every body of `crates` ([`Plan::new`] with
    /// [`every_body`](super::every_body)) with `style_lints` or without, a body or a module
    /// found clean kept as the database's memo and not checked again while nothing it read
    /// changed, and the plan made from the check the last such plan kept (the module).
    /// The diagnostics are those of a plan that keeps nothing: a memo or a kept answer says
    /// only that a check gave none, and a body or module that gave any is checked by its task
    /// as before. It runs with the `style_lints` it was made with, in the view it was made in.
    ///
    /// # Panics
    ///
    /// With [`NoChangedSet`]'s text, when a check is kept of these crates and the query
    /// engine cannot name what changed since it: there is no sweep of every body in the
    /// changed set's place.
    pub fn of_every_body(
        db: &Db,
        crates: &[Crate],
        style_lints: bool,
    ) -> Plan<'static, fn(DefWithBody) -> bool> {
        static EVERY: fn(DefWithBody) -> bool = super::every_body;
        Plan::make(db, crates, &EVERY, Some(style_lints))
    }
}

impl<F: Fn(DefWithBody) -> bool + Sync + ?Sized> Drop for Plan<'_, F> {
    fn drop(&mut self) {
        // A plan that never ran, or whose run unwound: the kept check it took is as it was.
        let carry = self.carry.lock().take();
        if let Some(Carry { of, last: Some(last) }) = carry {
            put_kept(of, last.kept);
        }
    }
}

impl<'f, F: Fn(DefWithBody) -> bool + Sync + ?Sized> Plan<'f, F> {
    /// The check of `crates`, of the bodies `asked` accepts (all with
    /// [`every_body`](super::every_body)), listed: one batch of the database's tasks, a file
    /// module each. It keeps nothing and starts from nothing kept.
    pub fn new(db: &Db, crates: &[Crate], asked: &'f F) -> Plan<'f, F> {
        Plan::make(db, crates, asked, None)
    }

    /// [`Plan::new`]; with `keep` (the style lints it will run with) a plan of every body
    /// that asks the memos, starts from the kept check of its key and leaves the next.
    fn make(db: &Db, crates: &[Crate], asked: &'f F, keep: Option<bool>) -> Plan<'f, F> {
        // First the engine's changed set: a walk from the inputs written since the last one
        // to the memos that changed, with early cutoff (blaze-query's `frontier`). After it
        // `changed_since` names the memos whose value is another, and a memo the edit did
        // not reach is marked unchanged where a task asks it, a few loads, with no walk of
        // what it read. Its span says what the engine did for this edit; a second plan of
        // the same view finds the walk done.
        {
            let settling = tracing::info_span!(
                target: "blaze",
                "body check: changed set",
                visited = tracing::field::Empty,
                edges = tracing::field::Empty,
                changed = tracing::field::Empty,
                unwound = tracing::field::Empty,
            )
            .entered();
            let work = blaze_query::db::plumbing::walk_changed_set(db);
            settling.record("visited", work.visited);
            settling.record("edges", work.edges);
            settling.record("changed", work.changed);
            settling.record("unwound", work.unwound);
        }
        // What of a unit's check is the plan and what the sweep: each its own span. A plan
        // from a kept check costs the engine's answer and the modules it touches; a whole
        // one lists every module of the check, and `why` says what made it whole.
        let listing = tracing::info_span!(
            target: "blaze",
            "body check plan",
            held_modules = tracing::field::Empty,
            held_bodies = tracing::field::Empty,
            named_modules = tracing::field::Empty,
            named_bodies = tracing::field::Empty,
            relisted = tracing::field::Empty,
            visited_modules = tracing::field::Empty,
            visited_bodies = tracing::field::Empty,
            kept_unvisited = tracing::field::Empty,
            whole = tracing::field::Empty,
            why = tracing::field::Empty,
        )
        .entered();
        let at = db.nonce_and_revision();
        let of = keep.map(|style_lints| Of {
            database: at.0,
            // alloc: once a plan, the key of what it keeps, a word per crate
            crates: crates.to_vec(),
            style_lints,
        });
        let from_kept = match &of {
            None => Err(Whole::KeepsNothing),
            Some(of) => match take_kept(of) {
                None => Err(Whole::NothingKept),
                Some(kept) => match changes(db, of, &kept) {
                    Ok(patch) => Ok((kept, patch)),
                    // A kept check whose crate's file modules changed is dropped here: the
                    // whole plan's run keeps the next.
                    Err(Unpatched::Whole(why)) => Err(why),
                    // No changed set: no plan. The kept check goes back as it was, so the
                    // error is the same until its cause is removed, never a whole plan the
                    // second time.
                    Err(Unpatched::Unknown(why)) => {
                        let refused = NoChangedSet {
                            // clone: the error's, once
                            crates: of.crates.clone(),
                            style_lints: of.style_lints,
                            since: kept.view,
                            why,
                        };
                        // clone: the error path, once
                        put_kept(of.clone(), kept);
                        panic!("{refused}");
                    }
                },
            },
        };
        let (tables, work, last) = match from_kept {
            Ok((kept, patch)) => {
                #[cfg(test)]
                let patch = {
                    let mut patch = patch;
                    if let Some(Plant::Unvisited(def)) = tests::planted(at.0) {
                        patch.visit_bodies.remove(&def);
                    }
                    patch
                };
                let tables = Tables::patched(&kept, &patch);
                let mut held_bodies = kept.bodies;
                // alloc: once a plan, a word per module whose list changed
                let mut relisted = Vec::with_capacity(patch.relisted.len());
                for &(place, list) in &patch.relisted {
                    held_bodies = held_bodies - kept.lists[place as usize].len() as u32
                        + list.len() as u32;
                    let local = patch.touched.binary_search(&place);
                    relisted.push(local.expect("a module listed again is touched") as u32);
                }
                let work = Work {
                    held_modules: kept.modules.len() as u32,
                    held_bodies,
                    named_modules: patch.named_modules,
                    named_bodies: patch.named_bodies,
                    relisted: relisted.len() as u32,
                    slots_modules: tables.modules.len() as u32,
                    slots_bodies: tables.bodies.len() as u32,
                    visited_modules: (tables.modules.len() - tables.unvisited_modules.len())
                        as u32,
                    visited_bodies: (tables.bodies.len() - tables.unvisited_bodies.len()) as u32,
                    whole: None,
                };
                (tables, work, Some(Last { kept, places: patch.touched, relisted }))
            }
            Err(why) => {
                let tables = Self::whole(db, crates, asked, keep.is_some());
                let work = Work {
                    held_modules: tables.modules.len() as u32,
                    held_bodies: tables.bodies.len() as u32,
                    named_modules: 0,
                    named_bodies: 0,
                    relisted: 0,
                    slots_modules: tables.modules.len() as u32,
                    slots_bodies: tables.bodies.len() as u32,
                    visited_modules: tables.modules.len() as u32,
                    visited_bodies: tables.bodies.len() as u32,
                    whole: Some(why),
                };
                (tables, work, None)
            }
        };
        listing.record("held_modules", work.held_modules);
        listing.record("held_bodies", work.held_bodies);
        listing.record("named_modules", work.named_modules);
        listing.record("named_bodies", work.named_bodies);
        listing.record("relisted", work.relisted);
        listing.record("visited_modules", work.visited_modules);
        listing.record("visited_bodies", work.visited_bodies);
        listing.record("kept_unvisited", work.kept_unvisited());
        listing.record("whole", work.whole.is_some());
        if let Some(why) = &work.whole {
            listing.record("why", tracing::field::display(why));
        }
        drop(listing);
        let Tables {
            modules,
            crate_ends,
            starts,
            bodies,
            tasks,
            unvisited_modules,
            unvisited_bodies,
        } = tables;
        let queue = Queue::new(bodies.len(), tasks.len());
        Plan {
            asked,
            modules,
            crate_ends,
            starts,
            bodies,
            tasks,
            unvisited_modules,
            unvisited_bodies,
            index: Default::default(),
            queue,
            started: AtomicBool::new(false),
            keep,
            at,
            carry: Lock::new("hir.check.carry", of.map(|of| Carry { of, last })),
            work,
            #[cfg(test)]
            ran: Lock::new("test.hir.check.ran", Vec::new()),
        }
    }

    /// The tables of a whole plan: every file module of `crates` and every body `asked`
    /// accepts of them, each with a task. With `memo` (a plan of every body that keeps) the
    /// lists are the database's ([`crate_file_modules`], [`module_bodies`]), so the engine
    /// names the ones an edit changes to the next plan.
    fn whole(db: &Db, crates: &[Crate], asked: &F, memo: bool) -> Tables {
        let mut modules = Vec::new();
        let mut crate_ends = Vec::with_capacity(crates.len());
        for &krate in crates {
            modules.extend(crate_file_modules(db, krate.id).iter().map(|&id| Module { id }));
            crate_ends
                .push(u32::try_from(modules.len()).expect("a check of fewer than 2^31 modules"));
        }
        assert!(modules.len() < MODULE as usize, "a check of fewer than 2^31 modules");
        #[expect(
            clippy::disallowed_types,
            reason = "body lists, set once per module of the check plan: not global"
        )]
        let lists: Vec<std::sync::OnceLock<Vec<Planned>>> =
            modules.iter().map(|_| std::sync::OnceLock::new()).collect();
        db.run_all(Tag::Infer, modules.len(), |db, at| {
            let out = if memo {
                // alloc: once a module of a whole plan, its list out of the database's
                module_bodies(db, modules[at].id).to_vec()
            } else {
                let mut out = Vec::new();
                list_module(db, modules[at].id, asked, &mut out);
                out
            };
            assert!(lists[at].set(out).is_ok(), "a module of a check was listed twice");
        });
        let mut starts = Vec::with_capacity(modules.len() + 1);
        let mut bodies = Vec::new();
        let mut tasks = Vec::with_capacity(modules.len());
        for (at, list) in lists.into_iter().enumerate() {
            let list = list
                .into_inner()
                .expect("`run_all` returned before a module of the check was listed");
            let first = u32::try_from(bodies.len()).expect("a check of fewer than 2^31 bodies");
            starts.push(first);
            tasks.push(MODULE | at as u32);
            tasks.extend((first..).take(list.len()));
            bodies.extend(list);
        }
        let end = u32::try_from(bodies.len()).ok().filter(|&it| it < MODULE);
        starts.push(end.expect("a check of fewer than 2^31 bodies"));
        Tables {
            modules,
            crate_ends,
            starts,
            bodies,
            tasks,
            // alloc: none, empty: every module and body of a whole plan has a task
            unvisited_modules: Vec::new(),
            unvisited_bodies: Vec::new(),
        }
    }

    /// What the plan holds and visits, and whether it is whole and why.
    pub fn work(&self) -> &Work {
        &self.work
    }

    /// Tells the check that a build reaches `defs`: each it has a task for and has not begun
    /// is checked before any body nothing reached. Any other is passed over: another crate's,
    /// one `asked` leaves out, one checked inline (an enum's variant, an item inside a body),
    /// one whose kept answer is the plan's. From any thread, before or while the plan runs.
    pub fn reached(&self, defs: &[DefWithBodyId]) {
        let index = self.index.get_or_init(|| {
            let mut index = HashMap::new_in(Global);
            index.reserve(self.tasks.len());
            let bodies = self.tasks.iter().filter(|&&task| task & MODULE == 0);
            index.extend(bodies.map(|&at| (self.bodies[at as usize].def, at)));
            index
        });
        for def in defs {
            if let Some(&at) = index.get(def) {
                self.queue.post(at);
            }
        }
    }

    /// The check's diagnostics, of each of its crates in their order: what
    /// [`check_module`](super::check_module) of each of their file modules gives, one after
    /// the other. A plan runs once.
    pub fn run<'db>(&self, db: &'db Db, style_lints: bool) -> Vec<Vec<AnyDiagnostic<'db>>> {
        // alloc: once a check, a word per crate
        self.run_marked(db, style_lints).into_iter().map(|it| it.diagnostics).collect()
    }

    /// [`Plan::run`], each crate's diagnostics marked by where they were found: in a body's
    /// task (inside a body: its inference, borrow and match checks, its macro calls, and the
    /// items its blocks declare) or not (the items' own checks: their signatures, impls,
    /// well-formedness, an enum's discriminants). What depends on a crate reads its items,
    /// never its bodies' errors, so a crate whose every error is in a body leaves its items
    /// usable (blaze-cargo's open failure under `--keep-going`).
    pub fn run_marked<'db>(&self, db: &'db Db, style_lints: bool) -> Vec<CrateFound<'db>> {
        assert!(!self.started.swap(true, Ordering::Relaxed), "a plan of a check runs once");
        if let Some(made_with) = self.keep {
            assert_eq!(
                made_with, style_lints,
                "a plan of every body runs with the style lints it was made with: its kept \
                 check is of them"
            );
            assert_eq!(
                db.nonce_and_revision(),
                self.at,
                "a plan of every body runs in the database and view it was made in: what it \
                 visits is the changed set up to that view"
            );
        }
        #[expect(
            clippy::disallowed_types,
            reason = "diagnostics and marks, set once per module of the check: not global"
        )]
        let modules: Vec<std::sync::OnceLock<Checked<'db>>> =
            self.modules.iter().map(|_| std::sync::OnceLock::new()).collect();
        #[expect(
            clippy::disallowed_types,
            reason = "diagnostics, set once per body of the check: not global"
        )]
        let bodies: Vec<std::sync::OnceLock<Vec<AnyDiagnostic<'db>>>> =
            self.bodies.iter().map(|_| std::sync::OnceLock::new()).collect();
        let slots = Slots { modules: &modules, bodies: &bodies };
        // A module or body with a slot and no task: its kept answer, clean. A clean module's
        // items gave nothing, so each of its listed bodies is marked at the start.
        for &at in &self.unvisited_modules {
            let at = at as usize;
            // alloc: a mark a listed body, once a touched module with no task
            let marks = vec![0; (self.starts[at + 1] - self.starts[at]) as usize];
            assert!(
                slots.modules[at].set((Vec::new(), marks)).is_ok(),
                "a module of a check was checked twice"
            );
        }
        for &at in &self.unvisited_bodies {
            assert!(
                slots.bodies[at as usize].set(Vec::new()).is_ok(),
                "a body of a check was checked twice"
            );
        }
        let home = ptr::from_ref(db).cast::<()>().addr();
        // The sweep: a task for each module and body the plan visits, which asks the engine
        // whether its memo holds ([`body_check_clean`], [`module_check_clean`]) and is
        // checked again only where it does not. A whole plan visits every one (on ripgrep's
        // binary, 2026-11-11: `unit bodies` 484 ms, 283 ms of CPU, `body_check_clean`
        // executed 6 and verified 1,149); a plan from a kept check visits what the engine
        // named, what was not clean and what is new, and `kept_unvisited` counts the rest,
        // which cost nothing here. This span holds the sweep's wall time, and the queries
        // that ran under it name the bodies checked again.
        let work = &self.work;
        let sweep = tracing::info_span!(
            target: "blaze",
            "body check sweep",
            swept_tasks = self.tasks.len(),
            held_modules = work.held_modules,
            held_bodies = work.held_bodies,
            named_modules = work.named_modules,
            named_bodies = work.named_bodies,
            visited_modules = work.visited_modules,
            visited_bodies = work.visited_bodies,
            kept_unvisited = work.kept_unvisited(),
            memos_kept = self.keep.is_some(),
            whole = work.whole.is_some(),
            why = tracing::field::Empty,
        )
        .entered();
        if let Some(why) = &work.whole {
            sweep.record("why", tracing::field::display(why));
        }
        // A plan from a kept check has the edit's few tasks: where they are predicted to
        // cost less than a dispatch they run on this thread, one after the other, with no
        // job submitted (`Db::run_all_small`); a whole plan's run on the pool.
        db.run_all_small(&SWEEP, Tag::Infer, self.tasks.len(), |task_db, task| {
            assert_eq!(
                ptr::from_ref(task_db).cast::<()>().addr(),
                home,
                "a task of a check ran on another database than the one checked"
            );
            // SAFETY: `task_db` is the object `db` points at (checked above), which is borrowed
            // for `'db` by this call, and `run_all` returns only once every task has run. `'db`
            // brands the diagnostics as that database's, which this one is.
            let task_db: &'db Db = unsafe { &*ptr::from_ref(task_db) };
            self.task(task_db, task, style_lints, slots);
        });
        drop(sweep);
        self.keep_check(slots);
        self.assemble(modules, bodies)
    }

    /// Leaves the kept check of this run for the next plan of its key: the kept check the
    /// plan was made from with the lists it changed, or the plan's own tables when it was
    /// whole; the view it ran in; and the modules and bodies whose task found a diagnostic.
    /// Every one not visited was clean and stays so. After the batch, so each task's slot is
    /// set; a run that unwound keeps nothing new ([`Plan`]'s drop).
    fn keep_check(&self, slots: Slots<'_, '_>) {
        const RAN: &str = "`run_all` returned before a task of the check ran";
        let carry = self.carry.lock().take();
        let Some(Carry { of, last }) = carry else { return };
        let (mut kept, places) = match last {
            Some(Last { mut kept, places, relisted }) => {
                for &local in &relisted {
                    let (local, at) = (local as usize, places[local as usize]);
                    let list =
                        &self.bodies[self.starts[local] as usize..self.starts[local + 1] as usize];
                    // alloc: once a module whose list changed, its list kept
                    let before = mem::replace(&mut kept.lists[at as usize], list.to_vec());
                    for planned in &before {
                        if kept.home.get(&planned.def) == Some(&at) {
                            kept.home.remove(&planned.def);
                        }
                    }
                    for planned in list {
                        kept.home.insert(planned.def, at);
                    }
                    kept.bodies = kept.bodies - before.len() as u32 + list.len() as u32;
                }
                (kept, Some(places))
            }
            None => {
                let mut place = HashMap::new_in(Global);
                place.reserve(self.modules.len());
                let mut home = HashMap::new_in(Global);
                home.reserve(self.bodies.len());
                let mut lists = Vec::with_capacity(self.modules.len());
                for (at, module) in self.modules.iter().enumerate() {
                    let list = &self.bodies[self.starts[at] as usize..self.starts[at + 1] as usize];
                    place.insert(AsId::as_id(&module.id), at as u32);
                    home.extend(list.iter().map(|it| (it.def, at as u32)));
                    // alloc: once a module of a whole plan, its list kept
                    lists.push(list.to_vec());
                }
                let kept = Kept {
                    view: self.at.1,
                    // clone: once a whole plan, the modules kept, a word each
                    modules: self.modules.clone(),
                    // clone: once a whole plan, a word per crate
                    crate_ends: self.crate_ends.clone(),
                    lists,
                    bodies: self.bodies.len() as u32,
                    place,
                    home,
                    // alloc: none, empty: filled below
                    not_clean_modules: Vec::new(),
                    not_clean_bodies: Vec::new(),
                };
                (kept, None)
            }
        };
        kept.view = self.at.1;
        kept.not_clean_modules.clear();
        kept.not_clean_bodies.clear();
        for &task in &self.tasks {
            if task & MODULE != 0 {
                let local = (task & !MODULE) as usize;
                if !slots.modules[local].get().expect(RAN).0.is_empty() {
                    let at = places.as_ref().map_or(local as u32, |places| places[local]);
                    kept.not_clean_modules.push(at);
                }
            } else if !slots.bodies[task as usize].get().expect(RAN).is_empty() {
                kept.not_clean_bodies.push(self.bodies[task as usize].def);
            }
        }
        #[cfg(test)]
        if tests::planted(self.at.0) == Some(Plant::KeepsNotClean) {
            kept.not_clean_bodies.clear();
        }
        put_kept(of, kept);
    }

    /// The work of the batch's `at`th task: the body it was checking when it last unwound,
    /// every reached body it can claim, then its own module or body.
    fn task<'db>(&self, db: &'db Db, at: usize, style_lints: bool, slots: Slots<'_, 'db>) {
        let queue = &self.queue;
        let resumed = queue.running[at].load(Ordering::Relaxed);
        if resumed != NONE {
            self.body(db, at, resumed, style_lints, slots);
        }
        while let Some(body) = queue.pop() {
            if queue.claim(body) {
                self.body(db, at, body, style_lints, slots);
            }
        }
        match self.tasks[at] {
            own if own & MODULE != 0 => self.module(db, own & !MODULE, style_lints, slots),
            own => {
                if queue.claim(own) {
                    self.body(db, at, own, style_lints, slots);
                }
            }
        }
    }

    /// Checks body `body`, claimed, as task `task`.
    fn body<'db>(
        &self,
        db: &'db Db,
        task: usize,
        body: u32,
        style_lints: bool,
        slots: Slots<'_, 'db>,
    ) {
        self.queue.running[task].store(body, Ordering::Relaxed);
        let planned = self.bodies[body as usize];
        #[cfg(test)]
        self.ran.lock().push(Ran::Body(planned.def));
        // alloc: the body's diagnostics; empty, so no allocation, for a clean body
        let mut found = Vec::new();
        // A body the memo says is clean gives none, and nothing of it is read. One that gave
        // any is checked here for them: its queries are the memo's run's, so this reads what
        // that run made.
        if !(self.keep.is_some() && body_check_clean(db, planned.def, style_lints)) {
            DiagnosticsCollector::collect_body(
                db,
                planned.def,
                planned.owner,
                &mut found,
                style_lints,
                self.asked,
            );
        }
        assert!(
            slots.bodies[body as usize].set(found).is_ok(),
            "a body of a check was checked twice"
        );
        self.queue.running[task].store(NONE, Ordering::Relaxed);
    }

    /// The item checks of file module `module`, its listed bodies left to their tasks.
    fn module<'db>(&self, db: &'db Db, module: u32, style_lints: bool, slots: Slots<'_, 'db>) {
        #[cfg(test)]
        self.ran.lock().push(Ran::Module(module));
        let at = module as usize;
        let expected = &self.bodies[self.starts[at] as usize..self.starts[at + 1] as usize];
        if self.keep.is_some() && module_check_clean(db, self.modules[at].id, style_lints) {
            // The module's items gave nothing, so each of its listed bodies' diagnostics is
            // at the start of the module's: what the walk would mark with none before it.
            // alloc: a mark a listed body, once a module
            let marks = vec![0; expected.len()];
            assert!(
                slots.modules[at].set((Vec::new(), marks)).is_ok(),
                "a module of a check was checked twice"
            );
            return;
        }
        let mut found = Vec::new();
        let mut marks = Vec::with_capacity(expected.len());
        let defer = Defer { expected, marks: &mut marks };
        DiagnosticsCollector::collect_deferred(
            db,
            self.modules[at].id,
            &mut found,
            style_lints,
            self.asked,
            defer,
        );
        if let Some(missed) = expected.get(marks.len()) {
            panic!(
                "the check of module {:?} did not meet body {:?} where the plan lists it: the \
                 plan's listing and the collector's walk disagree",
                self.modules[at].id, missed.def
            );
        }
        assert!(
            slots.modules[at].set((found, marks)).is_ok(),
            "a module of a check was checked twice"
        );
    }

    /// Each crate's diagnostics: each module's, with each of its bodies' at its mark.
    #[expect(
        clippy::disallowed_types,
        reason = "diagnostic slots, set once per module or body of the check: not global"
    )]
    fn assemble<'db>(
        &self,
        modules: Vec<std::sync::OnceLock<Checked<'db>>>,
        mut bodies: Vec<std::sync::OnceLock<Vec<AnyDiagnostic<'db>>>>,
    ) -> Vec<CrateFound<'db>> {
        let mut out = Vec::with_capacity(self.crate_ends.len());
        let mut modules = modules.into_iter().enumerate();
        let mut from = 0;
        for &end in &self.crate_ends {
            let mut found = Vec::new();
            // alloc: once a crate checked, a flag per diagnostic; none for a clean crate
            let mut in_body = Vec::new();
            for (at, slot) in modules.by_ref().take((end - from) as usize) {
                let (own, marks) =
                    slot.into_inner().expect("`run_all` returned before a task of the check ran");
                let first = self.starts[at] as usize;
                let mut own = own.into_iter();
                let mut done = 0;
                for (nth, &mark) in marks.iter().enumerate() {
                    found.extend(own.by_ref().take(mark as usize - done));
                    done = mark as usize;
                    let body = bodies[first + nth].take().unwrap_or_else(|| {
                        panic!(
                            "body {:?} of the check was never checked",
                            self.bodies[first + nth].def
                        )
                    });
                    in_body.resize(found.len(), false);
                    found.extend(body);
                    in_body.resize(found.len(), true);
                }
                found.extend(own);
            }
            in_body.resize(found.len(), false);
            out.push(CrateFound { diagnostics: found, in_body });
            from = end;
        }
        out
    }

    /// What the plan's run did, in order.
    #[cfg(test)]
    pub(crate) fn ran(&self) -> Vec<Ran> {
        self.ran.lock().clone()
    }

    /// The bodies the plan lists, in its order.
    #[cfg(test)]
    pub(crate) fn listed(&self) -> Vec<DefWithBodyId> {
        self.bodies.iter().map(|it| it.def).collect()
    }
}

/// Whether the check of body `def` alone ([`check_body`](super::check_body): its lowering's
/// and expansions' errors, inference, unsafety, match and borrow checks, its constants, the
/// items its blocks declare) gives no diagnostic, with `style_lints` or without. Kept by the
/// database, so the next build's plan verifies it where this build's ran it, and a body
/// nothing changed for is not checked again and none of its results is read
/// ([`Plan::of_every_body`]). Its value is one bit; the diagnostics of a body that has any are
/// made by its task, from the results this run left.
///
/// A check places a diagnostic only once it has found one, so a clean body's run reads no
/// source position and stays verified when an edit above it in its file moves it.
#[blaze_query::tracked(returns(copy))]
pub fn body_check_clean(db: &Db, def: DefWithBodyId, style_lints: bool) -> bool {
    #[cfg(test)]
    tests::CHECKED.lock().push((db.nonce_and_revision().0, def));
    super::check_body(db, def, style_lints).is_empty()
}

/// Whether the item checks of file module `module` (its declarations' signatures, its impls,
/// its visibilities and macro definitions, an enum's discriminants, the inline modules
/// written in it), the bodies a plan lists left to their own checks ([`list_module`]), give
/// no diagnostic. Kept by the database as [`body_check_clean`] is. A walk that does not meet
/// a listed body where the listing has it is the plan's bug, and panics as the task's does.
#[blaze_query::tracked(returns(copy))]
pub fn module_check_clean(db: &Db, module: ModuleId, style_lints: bool) -> bool {
    let every = &super::every_body;
    let expected: &[Planned] = module_bodies(db, module);
    // alloc: the diagnostics; empty, so no allocation, for a clean module
    let mut found = Vec::new();
    // alloc: a mark a listed body, once per run of its check
    let mut marks = Vec::with_capacity(expected.len());
    let defer = Defer { expected, marks: &mut marks };
    DiagnosticsCollector::collect_deferred(db, module, &mut found, style_lints, every, defer);
    if let Some(missed) = expected.get(marks.len()) {
        panic!(
            "the check of module {module:?} did not meet body {:?} where the plan lists it: the \
             plan's listing and the collector's walk disagree",
            missed.def
        );
    }
    found.is_empty()
}

/// The file modules of `krate` ([`file_modules`](super::file_modules)): every module but the
/// inline ones, in the def map's order. Kept by the database, so a plan from a kept check
/// asks the engine whether the list it holds places in still stands
/// (`crate_file_modules::changed_since`) and lists no crate again while it does.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn crate_file_modules(db: &Db, krate: base_db::Crate) -> Box<[ModuleId]> {
    crate_def_map(db, krate)
        .modules(db)
        .filter(|(_, data)| !data.origin.is_inline())
        .map(|(id, _)| id)
        // alloc: once a crate and a change of its modules, the query's value, a word a module
        .collect()
}

/// The bodies a plan of every body checks as tasks of their own in file module `module`, in
/// the order the collector meets them ([`list_module`]). Kept by the database: a module's
/// check reads it ([`module_check_clean`]), and a plan from a kept check asks the engine
/// which modules' lists changed (`module_bodies::changed_since`) and lists only those again.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn module_bodies(db: &Db, module: ModuleId) -> Box<[Planned]> {
    // alloc: once a module and a change of its items, the query's value
    let mut out = Vec::new();
    list_module(db, module, &super::every_body, &mut out);
    out.into_boxed_slice()
}

/// One crate's diagnostics as [`Plan::run_marked`] gives them.
pub struct CrateFound<'db> {
    pub diagnostics: Vec<AnyDiagnostic<'db>>,
    /// By diagnostic: whether a body's task found it ([`Plan::run_marked`]).
    pub in_body: Vec<bool>,
}

/// A module's diagnostics, and the place in them of each of its listed bodies'.
type Checked<'db> = (Vec<AnyDiagnostic<'db>>, Vec<u32>);

/// The slots a run's tasks write, each once.
#[derive(Clone, Copy)]
struct Slots<'a, 'db> {
    #[expect(
        clippy::disallowed_types,
        reason = "diagnostics and marks, set once per module of the check: not global"
    )]
    modules: &'a [std::sync::OnceLock<Checked<'db>>],
    #[expect(
        clippy::disallowed_types,
        reason = "diagnostics, set once per body of the check: not global"
    )]
    bodies: &'a [std::sync::OnceLock<Vec<AnyDiagnostic<'db>>>],
}

/// Appends to `out` the bodies the collector meets at `module`'s level, in its order
/// (`DiagnosticsCollector::collect_module`): its declarations and unnamed constants, inline
/// modules within, then its impls' items.
fn list_module(
    db: &Db,
    module: ModuleId,
    asked: &(impl Fn(DefWithBody) -> bool + ?Sized),
    out: &mut Vec<Planned>,
) {
    let def_map = module.def_map(db);
    let scope = def_map.module(db, module).scope(db);
    for def in scope.declarations().chain(scope.unnamed_consts().map(ModuleDefId::ConstId)) {
        list_def(db, def, def_map, asked, out);
    }
    for impl_ in scope.impls() {
        for &(_, item) in ImplItems::of(db, impl_).0.items.iter() {
            list_def(db, item.into(), def_map, asked, out);
        }
    }
}

/// [`list_module`] of one declaration (`DiagnosticsCollector::collect_module_def`). An enum's
/// variants are bodies too, but each is at most a discriminant, far under what a task is sized
/// for: they stay in their module's task.
fn list_def(
    db: &Db,
    def: ModuleDefId,
    def_map: &DefMap,
    asked: &(impl Fn(DefWithBody) -> bool + ?Sized),
    out: &mut Vec<Planned>,
) {
    match def {
        ModuleDefId::ModuleId(it) => {
            if def_map.module(db, it).origin.is_inline() {
                list_module(db, it, asked, out);
            }
        }
        ModuleDefId::TraitId(it) => {
            for &(_, item) in TraitItems::query_with_diagnostics(db, it).0.items.iter() {
                list_def(db, item.into(), def_map, asked, out);
            }
        }
        ModuleDefId::FunctionId(it) => push(out, asked, it.into(), GenericDefId::from(it).into()),
        ModuleDefId::ConstId(it) => push(out, asked, it.into(), GenericDefId::from(it).into()),
        ModuleDefId::StaticId(it) => push(out, asked, it.into(), GenericDefId::from(it).into()),
        ModuleDefId::EnumVariantId(_)
        | ModuleDefId::AdtId(AdtId::EnumId(_) | AdtId::StructId(_) | AdtId::UnionId(_))
        | ModuleDefId::MacroId(_)
        | ModuleDefId::TypeAliasId(_)
        | ModuleDefId::BuiltinType(_) => {}
    }
}

fn push(
    out: &mut Vec<Planned>,
    asked: &(impl Fn(DefWithBody) -> bool + ?Sized),
    def: DefWithBodyId,
    owner: TypeOwnerId,
) {
    if asked(def.into()) {
        out.push(Planned { def, owner });
    }
}

/// A line of its own for an atomic that one side writes and every other reads.
#[repr(align(128))]
struct Line(AtomicU32);

/// Which bodies are claimed, and the ring of reached ones. Nothing locks: a body is claimed by
/// one `fetch_or`, posted once by another, and the ring's places are written once each.
struct Queue {
    /// A bit per body: some task has checked it or is checking it.
    claimed: AtomicBits,
    /// A bit per body: it is in the ring, or was.
    posted: AtomicBits,
    /// The reached bodies, in the order posted: `NONE` until written. As long as the bodies,
    /// since each is posted at most once.
    reached: Box<[AtomicU32]>,
    /// The next place of `reached` to take.
    head: Line,
    /// The next place of `reached` to write.
    tail: Line,
    /// A word per task: the body it is checking, `NONE` for none.
    running: Box<[AtomicU32]>,
}

impl Queue {
    fn new(bodies: usize, tasks: usize) -> Queue {
        Queue {
            claimed: AtomicBits::new(bodies),
            posted: AtomicBits::new(bodies),
            // alloc: once a check, a place per body
            reached: (0..bodies).map(|_| AtomicU32::new(NONE)).collect(),
            head: Line(AtomicU32::new(0)),
            tail: Line(AtomicU32::new(0)),
            // alloc: once a check, a word per task
            running: (0..tasks).map(|_| AtomicU32::new(NONE)).collect(),
        }
    }

    /// Whether this call claims body `at`: then the caller checks it, and no one else does.
    fn claim(&self, at: u32) -> bool {
        self.claimed.insert(at as usize, Ordering::AcqRel)
    }

    /// Puts body `at` in the ring, unless it is claimed already or was put there before.
    fn post(&self, at: u32) {
        if self.claimed.contains(at as usize, Ordering::Acquire)
            || !self.posted.insert(at as usize, Ordering::AcqRel)
        {
            return;
        }
        let place = self.tail.0.fetch_add(1, Ordering::Relaxed);
        self.reached[place as usize].store(at, Ordering::Release);
    }

    /// The first reached body not yet taken. A place reserved but not yet written is as the
    /// ring's end: its body is taken by the next pop after it is written.
    fn pop(&self) -> Option<u32> {
        loop {
            let head = self.head.0.load(Ordering::Acquire);
            if head >= self.tail.0.load(Ordering::Acquire) {
                return None;
            }
            let at = self.reached[head as usize].load(Ordering::Acquire);
            if at == NONE {
                return None;
            }
            if self
                .head
                .0
                .compare_exchange_weak(head, head + 1, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return Some(at);
            }
        }
    }
}
