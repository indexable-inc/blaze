//! A check as plane tasks: one per body, and one per file module for its items' checks
//! (docs/v2/plan/lanes/frontend/fast-bodies-tasks.md).
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

use std::{
    ptr,
    sync::{
        OnceLock,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
};

use base_db::Db;
use blaze_hash::HashMap;
use blaze_ids::bits::AtomicBits;
use blaze_mem::Tag;
use hir_def::{
    AdtId, DefWithBodyId, GenericDefId, ModuleDefId, ModuleId,
    nameres::{
        DefMap,
        assoc::{ImplItems, TraitItems},
    },
};

use super::file_modules;
use crate::{
    AnyDiagnostic, Crate, DefWithBody, Module, TypeOwnerId, diagnostics::DiagnosticsCollector,
};

#[cfg(test)]
mod tests;

/// No body: an empty place of the reached ring, a task checking none.
const NONE: u32 = u32::MAX;

/// A task's entry in [`Plan::tasks`] that is a module's (the module's index in its low bits);
/// any other entry is a body's index.
const MODULE: u32 = 1 << 31;

/// A body the plan checks as a task of its own, and whose generics its types are written in.
#[derive(Clone, Copy, Debug)]
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
                self.marks.push(u32::try_from(at).expect("a module of fewer than 2^32 diagnostics"));
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

/// The tasks of a check of some crates. See the module.
pub struct Plan<'f, F: Fn(DefWithBody) -> bool + Sync + ?Sized> {
    asked: &'f F,
    /// Every file module of the plan's crates, crate after crate, each in def-map order.
    modules: Vec<Module>,
    /// Where each crate's modules end in `modules`.
    crate_ends: Vec<u32>,
    /// Where each module's bodies start in `bodies`, and one more: where the last one's end.
    starts: Vec<u32>,
    bodies: Vec<Planned>,
    /// Each task's work, in the order the batch gives them: a module, then its bodies.
    tasks: Vec<u32>,
    /// Each listed body's index in `bodies`, made on the first [`Plan::reached`].
    index: OnceLock<HashMap<DefWithBodyId, u32>>,
    queue: Queue,
    started: AtomicBool,
    #[cfg(test)]
    ran: parking_lot::Mutex<Vec<Ran>>,
}

impl<'f, F: Fn(DefWithBody) -> bool + Sync + ?Sized> Plan<'f, F> {
    /// The check of `crates`, of the bodies `asked` accepts (all with
    /// [`every_body`](super::every_body)), listed: one batch of the database's tasks, a file
    /// module each.
    pub fn new(db: &Db, crates: &[Crate], asked: &'f F) -> Plan<'f, F> {
        let mut modules = Vec::new();
        let mut crate_ends = Vec::with_capacity(crates.len());
        for &krate in crates {
            modules.extend(file_modules(db, krate));
            crate_ends.push(u32::try_from(modules.len()).expect("a check of fewer than 2^31 modules"));
        }
        assert!(modules.len() < MODULE as usize, "a check of fewer than 2^31 modules");
        let lists: Vec<OnceLock<Vec<Planned>>> = modules.iter().map(|_| OnceLock::new()).collect();
        db.run_all(Tag::Infer, modules.len(), |db, at| {
            let mut out = Vec::new();
            list_module(db, modules[at].id, asked, &mut out);
            assert!(lists[at].set(out).is_ok(), "a module of a check was listed twice");
        });
        let mut starts = Vec::with_capacity(modules.len() + 1);
        let mut bodies = Vec::new();
        let mut tasks = Vec::with_capacity(modules.len());
        for (at, list) in lists.into_iter().enumerate() {
            let list = list.into_inner().expect("`run_all` returned before a module of the check was listed");
            let first = u32::try_from(bodies.len()).expect("a check of fewer than 2^31 bodies");
            starts.push(first);
            tasks.push(MODULE | at as u32);
            tasks.extend((first..).take(list.len()));
            bodies.extend(list);
        }
        let end = u32::try_from(bodies.len()).ok().filter(|&it| it < MODULE);
        starts.push(end.expect("a check of fewer than 2^31 bodies"));
        let queue = Queue::new(bodies.len(), tasks.len());
        Plan {
            asked,
            modules,
            crate_ends,
            starts,
            bodies,
            tasks,
            index: OnceLock::new(),
            queue,
            started: AtomicBool::new(false),
            #[cfg(test)]
            ran: parking_lot::Mutex::new(Vec::new()),
        }
    }

    /// Tells the check that a build reaches `defs`: each it lists and has not begun is checked
    /// before any body nothing reached. Any other is passed over: another crate's, one `asked`
    /// leaves out, one checked inline (an enum's variant, an item inside a body). From any
    /// thread, before or while the plan runs.
    pub fn reached(&self, defs: &[DefWithBodyId]) {
        let index = self.index.get_or_init(|| {
            let mut index = HashMap::default();
            index.reserve(self.bodies.len());
            index.extend(self.bodies.iter().enumerate().map(|(at, it)| (it.def, at as u32)));
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
        assert!(!self.started.swap(true, Ordering::Relaxed), "a plan of a check runs once");
        let modules: Vec<OnceLock<Checked<'db>>> = self.modules.iter().map(|_| OnceLock::new()).collect();
        let bodies: Vec<OnceLock<Vec<AnyDiagnostic<'db>>>> =
            self.bodies.iter().map(|_| OnceLock::new()).collect();
        let slots = Slots { modules: &modules, bodies: &bodies };
        let home = ptr::from_ref(db).cast::<()>().addr();
        db.run_all(Tag::Infer, self.tasks.len(), |task_db, task| {
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
        self.assemble(modules, bodies)
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
    fn body<'db>(&self, db: &'db Db, task: usize, body: u32, style_lints: bool, slots: Slots<'_, 'db>) {
        self.queue.running[task].store(body, Ordering::Relaxed);
        let planned = self.bodies[body as usize];
        #[cfg(test)]
        self.ran.lock().push(Ran::Body(planned.def));
        let mut found = Vec::new();
        DiagnosticsCollector::collect_body(db, planned.def, planned.owner, &mut found, style_lints, self.asked);
        assert!(slots.bodies[body as usize].set(found).is_ok(), "a body of a check was checked twice");
        self.queue.running[task].store(NONE, Ordering::Relaxed);
    }

    /// The item checks of file module `module`, its listed bodies left to their tasks.
    fn module<'db>(&self, db: &'db Db, module: u32, style_lints: bool, slots: Slots<'_, 'db>) {
        #[cfg(test)]
        self.ran.lock().push(Ran::Module(module));
        let at = module as usize;
        let expected = &self.bodies[self.starts[at] as usize..self.starts[at + 1] as usize];
        let mut found = Vec::new();
        let mut marks = Vec::with_capacity(expected.len());
        let defer = Defer { expected, marks: &mut marks };
        DiagnosticsCollector::collect_deferred(db, self.modules[at].id, &mut found, style_lints, self.asked, defer);
        if let Some(missed) = expected.get(marks.len()) {
            panic!(
                "the check of module {:?} did not meet body {:?} where the plan lists it: the \
                 plan's listing and the collector's walk disagree",
                self.modules[at].id, missed.def
            );
        }
        assert!(slots.modules[at].set((found, marks)).is_ok(), "a module of a check was checked twice");
    }

    /// Each crate's diagnostics: each module's, with each of its bodies' at its mark.
    fn assemble<'db>(
        &self,
        modules: Vec<OnceLock<Checked<'db>>>,
        mut bodies: Vec<OnceLock<Vec<AnyDiagnostic<'db>>>>,
    ) -> Vec<Vec<AnyDiagnostic<'db>>> {
        let mut out = Vec::with_capacity(self.crate_ends.len());
        let mut modules = modules.into_iter().enumerate();
        let mut from = 0;
        for &end in &self.crate_ends {
            let mut found = Vec::new();
            for (at, slot) in modules.by_ref().take((end - from) as usize) {
                let (own, marks) = slot.into_inner().expect("`run_all` returned before a task of the check ran");
                let first = self.starts[at] as usize;
                let mut own = own.into_iter();
                let mut done = 0;
                for (nth, &mark) in marks.iter().enumerate() {
                    found.extend(own.by_ref().take(mark as usize - done));
                    done = mark as usize;
                    let body = bodies[first + nth].take().unwrap_or_else(|| {
                        panic!("body {:?} of the check was never checked", self.bodies[first + nth].def)
                    });
                    found.extend(body);
                }
                found.extend(own);
            }
            out.push(found);
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

/// A module's diagnostics, and the place in them of each of its listed bodies'.
type Checked<'db> = (Vec<AnyDiagnostic<'db>>, Vec<u32>);

/// The slots a run's tasks write, each once.
#[derive(Clone, Copy)]
struct Slots<'a, 'db> {
    modules: &'a [OnceLock<Checked<'db>>],
    bodies: &'a [OnceLock<Vec<AnyDiagnostic<'db>>>],
}

/// Appends to `out` the bodies the collector meets at `module`'s level, in its order
/// (`DiagnosticsCollector::collect_module`): its declarations and unnamed constants, inline
/// modules within, then its impls' items.
fn list_module(db: &Db, module: ModuleId, asked: &(impl Fn(DefWithBody) -> bool + ?Sized), out: &mut Vec<Planned>) {
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
            if self.head.0.compare_exchange_weak(head, head + 1, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
                return Some(at);
            }
        }
    }
}
