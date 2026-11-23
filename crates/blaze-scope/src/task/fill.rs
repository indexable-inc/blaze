//! The one in-memory scheduler of names tasks, for the tests and the examples: it runs
//! [`solve`] to a fixpoint over the modules of one crate. Its only lasting table is the
//! finished scopes (and the failed modules); everything else it reads comes in through
//! [`FillEnv`]. The driver does not use it.
//!
//! Each wave runs every ready task (through [`Par`]); a task that ends in `Need` waits until
//! every module it named is finished, then is entered again from the top. When no task is
//! ready, each sink component of the wait graph becomes one group task, widened to the whole
//! component at once, so a group is entered seeing its whole cycle.

#![blaze::sans_io]

use blaze_hash::HashMap;
use blaze_id::{CrateItemKey, ItemKey};
use blaze_own::{AVec, Alloc};

use crate::facts::{Name, ScopeFacts};
use crate::resolver::MAX_MODULE_DEPTH;
use crate::scope::Resolved;

use super::answer::{Held, Records};
use super::prefetch::{prefetch, sccs};
use super::solve::{Expand, Outcome, Tally, TaskError, Want, solve};
use crate::error::ResolveError;

/// Runs `f` for each index below `n`, the results in index order: serially, or across threads.
pub trait Par {
    fn map<T, F: Fn(usize) -> T>(&self, n: usize, f: F, out: &mut AVec<'_, T>);

    /// Independent files discovered by the same module walk, in discovery order. A driver
    /// can distribute these texts without lending the crate's allocator to another worker.
    fn outline_files<'a>(
        &self,
        _krate: &'a str,
        files: &[(&'a str, &'a str)],
        edition: blaze_surface::Edition,
        alloc: Alloc<'a>,
        out: &mut AVec<'a, blaze_file_items::Outline<'a>>,
    ) -> Result<(), crate::resolve::NamesError<'a>> {
        for &(_, text) in files {
            out.push(blaze_file_items::outline_auto_extensions(text, edition, blaze_file_items::Bodies::Skipped, alloc));
        }
        Ok(())
    }

    /// The calling thread's solve passes into the stretch `stage` (rule 16: every stretch of
    /// a solve has a name the caller's clock can time). The stretch lasts until the thread's
    /// next mark. The library holds no clock: nothing by default.
    fn stage(&self, stage: Stage) {
        let _ = stage;
    }

    /// Inside [`Stage::Expand`]: the calling thread's expansion of a call of the macro named
    /// `macro_name` passes into `part`, which works on `size` units (bytes of text or tokens,
    /// by the part; 0 where a part has no size). The part lasts until the thread's next mark;
    /// [`ExpandPart::End`] closes the call. The library holds no clock: nothing by default.
    fn expand_part(&self, part: ExpandPart, macro_name: &str, size: usize) {
        let _ = (part, macro_name, size);
    }

    /// Inside [`Stage::Expand`]: the calling thread expands a call of the macro defined by the
    /// item `def` whose input text is `input` (for a caller that counts the calls a record
    /// keyed by the definition and the input would answer). Nothing by default.
    fn expand_call(&self, def: ItemKey, input: &str) {
        let _ = (def, input);
    }

    /// D116's equality check (drive.md "Names at module grain"): a clock in nanoseconds asks
    /// each crate's solve to decide every cell again on demand over its frozen tables and
    /// compare. The library holds no clock: `None` by default, and the check does not run.
    fn decide_check(&self) -> Option<&dyn Fn() -> u64> {
        None
    }

    /// The check's report for crate `krate`. Nothing by default.
    fn decided(&self, krate: &str, check: &crate::DecideCheck<'_, &str>) {
        let _ = (krate, check);
    }

    /// The on-demand decision of D116's check, planted wrong for a driver's control of its
    /// oracle (rule 23; blaze-cli names_oracle_tests.rs). Read only while [`Par::decide_check`]
    /// asks for the check, and it plants the decision alone: the whole solve, whose answers
    /// every later stage reads, is the same. [`DecidePlant::None`] by default.
    fn decide_plant(&self) -> DecidePlant {
        DecidePlant::None
    }

    /// E4: each crate's names written per group too ([`crate::CrateNames::groups`],
    /// cells/groups.rs). Off by default: only a caller that reads the groups asks (its tests,
    /// and the engine once it plans names by group); the command's `CrateScope` does not.
    fn names_groups(&self) -> bool {
        false
    }

    /// E4's one-group solve (cells/edit.rs `OneGroup`): solve only these modules from their
    /// text, every other module of the crate answered from its group's record, and write the
    /// members' groups alone. `None` by default: the whole crate.
    fn names_members(&self) -> Option<crate::cells::Members<'_>> {
        None
    }

    /// Inside [`Stage::Solve`]: the calling thread's cell solve passes into `part` (rule 16:
    /// the solve's own stretches, for the cost of a one-group solve). The part lasts until the
    /// thread's next mark. The library holds no clock: nothing by default.
    fn names_part(&self, part: NamesPart) {
        let _ = part;
    }

    /// A one-group solve decoded the crate's module tree record of `bytes` bytes
    /// (tree_record.rs): a count of the solve's cost (rule 16), nothing by default.
    fn tree_record_read(&self, bytes: usize) {
        let _ = bytes;
    }

    /// Where one macro call's working memory is held while it is expanded (resolve.rs
    /// `expand_decls`: its input, the matcher's expansion, the rendering, the origin column,
    /// the kept entry's bytes): [`Scratch::Reset`] when this Par ran `work` itself in a
    /// scratch allocator it resets once `work` returns (everything `work` keeps it copies into
    /// the crate's own allocator at its exact size first); [`Scratch::Kept`] when it did not
    /// run `work`, which the caller then runs in the crate's allocator, as before this hook.
    ///
    /// By default [`Scratch::Kept`]: the library holds no allocator of its own (sans-IO); a
    /// driver hands one through this method (blaze-cli names/metered.rs). The two answers
    /// write equal bytes (tests/call_scratch.rs); only the frame's peak differs.
    fn call_scratch(&self, work: &mut dyn FnMut(Alloc<'_>)) -> Scratch {
        let _ = work;
        Scratch::Kept
    }

    /// D189's strategy seam (rule 78): whether a body's blocks are modules of the crate
    /// ([`crate::surface::BlockScopes::Modules`], the default) or each body's own, solved by
    /// [`crate::body_scope`] after the crate's names are finished
    /// ([`crate::surface::BlockScopes::Bodies`]: the crate solve holds the bodies,
    /// `CrateNames::bodies`).
    fn block_scopes(&self) -> crate::surface::BlockScopes {
        crate::surface::BlockScopes::Modules
    }
}

/// A planted bug of D116's on-demand decision ([`Par::decide_plant`]): a control, never the
/// command's choice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecidePlant {
    None,
    /// The walk keeps a node's value at once though it read a node still open on its stack
    /// (cells `CellPlant::DecideCachesOpenCycle`): a glob cycle keeps the bottom it saw, so a
    /// name reached through the cycle is not found on demand.
    CachesOpenCycle,
}

/// Where a macro call's working memory was held ([`Par::call_scratch`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scratch {
    /// In the crate's own allocator, kept until the crate's names are done: the work was not
    /// run by the Par (the path before lane N5, and the control of its test).
    Kept,
    /// In a scratch the Par reset once the call's results were copied out.
    Reset,
}

/// A driver's scratch, as one piece of work takes it: [`Par::call_scratch`] by another name,
/// for work that has no `Par` at hand (one held body's solve, `crate::body_scope`).
pub type ScratchHook<'r> = &'r dyn Fn(&mut dyn FnMut(Alloc<'_>)) -> Scratch;

impl Scratch {
    /// The hook of no driver: every piece of work runs in the caller's allocator.
    pub fn kept(work: &mut dyn FnMut(Alloc<'_>)) -> Scratch {
        let _ = work;
        Scratch::Kept
    }

    /// `work`'s answer, its working memory in `hook`'s scratch (reset once it returns), or in
    /// `home` when the hook kept none ([`Scratch::Kept`]). What `work` returns holds nothing of
    /// the scratch: its type names no lifetime of it. A hook that says [`Scratch::Reset`]
    /// without running `work` broke its contract: a bug that fails this job (rule 20).
    pub fn run<T>(hook: ScratchHook<'_>, home: Alloc<'_>, mut work: impl FnMut(Alloc<'_>) -> T) -> T {
        let mut out = None;
        let mut step = |scratch: Alloc<'_>| out = Some(work(scratch));
        if hook(&mut step) == Scratch::Kept {
            step(home);
        }
        match out {
            Some(answer) => answer,
            None => panic!("a scratch hook said Reset without running its work (blaze-scope task/fill.rs `Scratch::run`)"),
        }
    }
}

/// The stretches of one crate's solve finer than a [`Stage`] ([`Par::names_part`]), in the
/// order a solve passes them; `Outside` and `Run` repeat once per round of made modules.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NamesPart {
    /// Inside [`Stage::Files`]: the module tree walked over the files read so far.
    Walk,
    /// Inside [`Stage::Files`]: the files a walk named read and outlined.
    Outline,
    /// Inside [`Stage::Files`]: a one-group solve handed the module tree record found its
    /// inputs moved (a member in no row, a file gone, rows, lines, exports or macros not the
    /// record's) and makes the tree again from the files (tree_record.rs: the stated path).
    TreeAgain,
    /// The files' blocks made modules, before the cell core.
    Blocks,
    /// The cell core loaded: the module tree and the members' texts.
    Load,
    /// The modules outside a one-group solve's members filled from their groups' records.
    Outside,
    /// The cells decided (expansions included).
    Run,
    /// Every module's scope written from the decided cells.
    Scopes,
    /// The crate's groups found (`Par::names_groups`).
    Groups,
    /// Inside [`Stage::Publish`], after the root's exports and the refused calls: each group's
    /// record written over the final scopes.
    Records,
}

/// The stretches of one crate's solve, in the order a solve passes them ([`Par::stage`]).
/// `Expand` is entered from `Solve` and returns to it, once per macro call expanded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Stage {
    /// A try starts: the handed crates' records are gathered and the local graph is made. A
    /// try that ends in a `Need` is followed by another `Try` of the same crate, never `Done`.
    Try,
    /// Files read, outlined and walked into the module tree.
    Files,
    /// Each walked module's own text lowered to declarations; the crate root planned.
    Lower,
    /// The declaration records written and read back; the module chains.
    Decls,
    /// The fill: imports and macro paths solved to a fixed point (expansion excluded).
    Solve,
    /// One macro call expanded: the macro run, its output outlined and lowered.
    Expand,
    /// After the fill: exports published, tables merged.
    Publish,
    /// The exports and inner records written.
    Write,
    /// The read list encoded and handed to the store with the two records.
    ReadList,
    /// The names record encoded and handed to the store.
    NamesRecord,
    /// The items record encoded and handed to the store.
    ItemsRecord,
    /// The crate's kept expansions written as one set and handed to the store.
    ExpansionsRecord,
    /// The crate's own exports record decoded for its dependents.
    Decode,
    /// The crate is done on this thread.
    Done,
}

impl Stage {
    /// Every stage, in [`Stage`]'s order: `ALL[s as usize] == s`.
    pub const ALL: [Stage; 14] = [
        Stage::Try,
        Stage::Files,
        Stage::Lower,
        Stage::Decls,
        Stage::Solve,
        Stage::Expand,
        Stage::Publish,
        Stage::Write,
        Stage::ReadList,
        Stage::NamesRecord,
        Stage::ItemsRecord,
        Stage::ExpansionsRecord,
        Stage::Decode,
        Stage::Done,
    ];
}

/// The parts of one macro call's expansion ([`Stage::Expand`]), in the order a call passes
/// them ([`Par::expand_part`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ExpandPart {
    /// The call and its macro found in the crate's tables, or asked of the defining crate.
    Find,
    /// The call's key made from its text and looked up in the crate's kept expansions
    /// ([`crate::expansions`]); on a hit its read list asked again and its record read back, and
    /// the call passes to [`ExpandPart::Render`] (size: the input's bytes).
    Kept,
    /// The definition: found prepared, or else its text lexed and its rules compiled.
    Define,
    /// The call's input text lexed into tokens (size: its bytes).
    LexInput,
    /// The call's shape taken from the input: an attribute's item, a derive's facts.
    Shape,
    /// A `macro_rules!` or `macro` run: matching and transcription (size: input tokens).
    RunRules,
    /// A function-like builtin run, its eager nested calls included (size: input tokens).
    RunBuiltin,
    /// A builtin derive or attribute run (size: input tokens).
    RunDerive,
    /// A proc macro run by the host (size: input tokens).
    RunProc,
    /// A fresh expansion written as its kept entry (size: tokens).
    Keep,
    /// The expansion's tokens written as text (size: tokens).
    Render,
    /// The text outlined: lexed and parsed again (size: its bytes).
    Outline,
    /// The outline lowered to declarations (size: the text's bytes).
    Lower,
    /// The expansion's modules made.
    Modules,
    /// The lowered tables merged into the run's.
    Tables,
    /// The blocks of the expansion's bodies that hold impls made modules.
    Blocks,
    /// The call's result stored for the fill.
    Store,
    /// The call is over.
    End,
}

impl ExpandPart {
    /// Every part, in [`ExpandPart`]'s order: `ALL[p as usize] == p`.
    pub const ALL: [ExpandPart; 18] = [
        ExpandPart::Find,
        ExpandPart::Kept,
        ExpandPart::Define,
        ExpandPart::LexInput,
        ExpandPart::Shape,
        ExpandPart::RunRules,
        ExpandPart::RunBuiltin,
        ExpandPart::RunDerive,
        ExpandPart::RunProc,
        ExpandPart::Keep,
        ExpandPart::Render,
        ExpandPart::Outline,
        ExpandPart::Lower,
        ExpandPart::Modules,
        ExpandPart::Tables,
        ExpandPart::Blocks,
        ExpandPart::Store,
        ExpandPart::End,
    ];
}

/// One thread.
pub struct Serial;

impl Par for Serial {
    fn map<T, F: Fn(usize) -> T>(&self, n: usize, f: F, out: &mut AVec<'_, T>) {
        for at in 0..n {
            out.push(f(at));
        }
    }
}

/// What the fill reads that is not its own table: the facts, the crate, its modules' places,
/// the records of other crates and of enums, and the modules macros wrote since last asked.
pub trait FillEnv<N>: Expand {
    type Facts: ScopeFacts<Name = N>;

    fn facts(&self) -> &Self::Facts;

    fn here(&self) -> CrateItemKey;

    /// `Some(parent)` for a module of this crate (walked or written by a macro), else `None`.
    fn parent(&self, module: ItemKey) -> Option<Option<ItemKey>>;

    /// What is held of a key that is no module of this crate: an enum, another crate's module.
    fn outer(&self, key: ItemKey) -> Held<'_, N>;

    /// The modules macros wrote since the last call.
    fn made(&self, out: &mut AVec<'_, ItemKey>);

    /// A read of the fill was answered: see [`super::Records::asked`]; nothing by default.
    fn asked(&self, read: crate::read::Read<N>, own: &super::Own<'_, N>) {
        let _ = (read, own);
    }

    /// The text inside a proc-macro derive's `attributes(..)` (its helper attributes, read by
    /// [`crate::task::ProcAttr::helper_names`]), or `None` when `derive` is no proc derive held.
    fn derive_helpers(&self, derive: ItemKey) -> Option<&str> {
        let _ = derive;
        None
    }
}

/// The work of a fill: tasks entered, entries that ended in `Need` (their work is redone),
/// steps, and the steps of those entries. R = `needs / entries` (bottom.md 2).
#[derive(Clone, Copy, Default, Debug)]
pub struct FillReport {
    pub entries: u64,
    pub needs: u64,
    pub steps: u64,
    pub wasted_steps: u64,
    pub waves: u64,
    pub groups: u64,
    pub largest: usize,
    pub failed: u64,
    pub reads: u64,
    pub expansions: u64,
    pub failed_reads: u64,
    pub no_scope_reads: u64,
    pub unknown_reads: u64,
    /// The syntactic prefetch: its components, the largest, and its edges.
    pub prefetch_components: u64,
    pub prefetch_largest: usize,
    pub prefetch_edges: u64,
    /// The solver's counters, summed over the tasks that finished.
    pub fill: crate::resolver::FillStats,
}

enum State<'a> {
    Ready,
    Waiting(AVec<'a, ItemKey>),
    Done,
    Failed,
    Merged,
}

struct Task<'a> {
    members: AVec<'a, ItemKey>,
    state: State<'a>,
}

/// How a fill's first tasks are made.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum First {
    /// The components of the syntactic prefetch, in its order ([`super::prefetch`]).
    #[default]
    Prefetch,
    /// Each module alone, all at once: kept to measure the prefetch against (R before).
    Alone,
}

/// The fill of one crate's modules.
pub struct Fill<'a, N> {
    alloc: Alloc<'a>,
    /// How the first tasks are made.
    pub first: First,
    /// The finished scopes, by module.
    pub done: HashMap<ItemKey, Resolved<'a, N>, Alloc<'a>>,
    /// The modules whose task failed, and why.
    pub failed: HashMap<ItemKey, TaskError, Alloc<'a>>,
    /// Each module's chain up to its crate's root, itself first.
    chains: HashMap<ItemKey, AVec<'a, ItemKey>, Alloc<'a>>,
    tasks: AVec<'a, Task<'a>>,
    task_of: HashMap<ItemKey, usize, Alloc<'a>>,
    pub report: FillReport,
}

struct View<'v, 'a, N, E> {
    fill: &'v Fill<'a, N>,
    env: &'v E,
}

impl<N: Name, E: FillEnv<N>> Records<N> for View<'_, '_, N, E> {
    fn held(&self, key: ItemKey) -> Held<'_, N> {
        let chain = || self.fill.chains.get(&key).map(|c| c.as_slice());
        if let Some(r) = self.fill.done.get(&key) {
            return Held::Scope { scope: crate::scope_record::ScopeOf::Built(&r.scope), krate: self.env.here(), chain: chain() };
        }
        if self.fill.failed.contains_key(&key) {
            return Held::Failed { krate: self.env.here(), chain: chain() };
        }
        if self.env.parent(key).is_some() {
            return Held::Pending;
        }
        self.env.outer(key)
    }

    fn parent(&self, module: ItemKey) -> Option<Option<ItemKey>> {
        self.env.parent(module)
    }

    fn asked(&self, read: crate::read::Read<N>, own: &super::Own<'_, N>) {
        self.env.asked(read, own);
    }
}

/// The most waves one fill runs (rule 18): each finishes or groups at least one task.
const MOST_WAVES: usize = 1 << 24;

impl<'a, N: Name> Fill<'a, N> {
    pub fn new_in(alloc: Alloc<'a>) -> Fill<'a, N> {
        Fill {
            alloc,
            first: First::Prefetch,
            done: HashMap::new_in(alloc),
            failed: HashMap::new_in(alloc),
            chains: HashMap::new_in(alloc),
            tasks: AVec::new_in(alloc),
            task_of: HashMap::new_in(alloc),
            report: FillReport::default(),
        }
    }

    /// A module's chain up to its crate's root, itself first.
    pub fn chain(&self, module: ItemKey) -> Option<&[ItemKey]> {
        self.chains.get(&module).map(|c| c.as_slice())
    }

    fn add<E: FillEnv<N>>(&mut self, module: ItemKey, env: &E) {
        if self.task_of.contains_key(&module) || self.done.contains_key(&module) {
            return;
        }
        let mut members = AVec::new_in(self.alloc);
        members.push(module);
        self.add_task(members, None, env);
    }

    /// One task of `members` (sorted, none known yet), first entered once every module in `on`
    /// is finished.
    fn add_task<E: FillEnv<N>>(&mut self, members: AVec<'a, ItemKey>, on: Option<AVec<'a, ItemKey>>, env: &E) {
        for &module in members.iter() {
            let mut chain = AVec::new_in(self.alloc);
            chain.push(module);
            let mut at = module;
            for _ in 0..MAX_MODULE_DEPTH {
                let Some(Some(parent)) = env.parent(at) else { break };
                chain.push(parent);
                at = parent;
            }
            self.chains.insert(module, chain);
            self.task_of.insert(module, self.tasks.len());
        }
        if members.len() > 1 {
            self.report.groups += 1;
            self.report.largest = self.report.largest.max(members.len());
        }
        let state = match on {
            Some(on) => State::Waiting(on),
            None => State::Ready,
        };
        self.tasks.push(Task { members, state });
    }

    /// The first tasks: the components of the syntactic prefetch, each entered once the
    /// components it reads are finished.
    fn add_prefetched<E: FillEnv<N>>(&mut self, env: &E, modules: &[ItemKey]) {
        let alloc = self.alloc;
        let mut fresh = AVec::with_capacity_in(modules.len(), alloc);
        fresh.extend(modules.iter().copied().filter(|m| !self.task_of.contains_key(m) && !self.done.contains_key(m)));
        fresh.sort_unstable();
        fresh.dedup();
        let pre = match prefetch(&fresh, env.facts(), alloc) {
            Ok(pre) => pre,
            Err(error) => {
                for &module in fresh.iter() {
                    let mut members = AVec::new_in(alloc);
                    members.push(module);
                    self.add_task(members, None, env);
                    let at = self.tasks.len() - 1;
                    self.fail(at, TaskError::Resolve(ResolveError::Surface { module, error }));
                }
                return;
            }
        };
        self.report.prefetch_components += pre.components.len() as u64;
        self.report.prefetch_largest = self.report.prefetch_largest.max(pre.largest());
        self.report.prefetch_edges += pre.edges;
        for (c, component) in pre.components.iter().enumerate() {
            let mut on = AVec::with_capacity_in(pre.reads[c].len(), alloc);
            on.extend(pre.reads[c].iter().map(|&d| pre.components[d as usize][0]));
            let on = if on.is_empty() { None } else { Some(on) };
            self.add_task(AVec::from_slice_in(component, alloc), on, env);
        }
    }

    fn fail(&mut self, task: usize, error: TaskError) {
        self.report.failed += self.tasks[task].members.len() as u64;
        for &m in self.tasks[task].members.iter() {
            self.failed.insert(m, error);
        }
        self.tasks[task].state = State::Failed;
    }

    fn finished(&self, module: ItemKey) -> bool {
        self.done.contains_key(&module) || self.failed.contains_key(&module)
    }

    /// Fills `modules` and every module a macro writes on the way, each finished or failed.
    pub fn run<E: FillEnv<N>, P: Par>(&mut self, env: &E, par: &P, modules: &[ItemKey]) {
        let alloc = self.alloc;
        match self.first {
            First::Prefetch => self.add_prefetched(env, modules),
            First::Alone => {
                for &m in modules {
                    self.add(m, env);
                }
            }
        }
        let mut made = AVec::new_in(alloc);
        for _ in 0..MOST_WAVES {
            made.clear();
            env.made(&mut made);
            for &m in made.iter() {
                self.add(m, env);
            }
            let mut ready: AVec<'_, usize> = AVec::new_in(alloc);
            let mut waiting = 0usize;
            for (at, task) in self.tasks.iter().enumerate() {
                match &task.state {
                    State::Ready => ready.push(at),
                    State::Waiting(on) => {
                        if on.iter().all(|m| self.finished(*m)) {
                            ready.push(at);
                        } else {
                            waiting += 1;
                        }
                    }
                    State::Done | State::Failed | State::Merged => {}
                }
            }
            if !ready.is_empty() {
                self.wave::<E, P>(env, par, &ready);
                continue;
            }
            if waiting == 0 {
                return;
            }
            self.group();
        }
        for at in 0..self.tasks.len() {
            if matches!(self.tasks[at].state, State::Ready | State::Waiting(_)) {
                let module = self.tasks[at].members[0];
                self.fail(at, TaskError::Stalled { module });
            }
        }
    }

    fn wave<E: FillEnv<N>, P: Par>(&mut self, env: &E, par: &P, ready: &[usize]) {
        let alloc = self.alloc;
        self.report.waves += 1;
        let mut results = AVec::with_capacity_in(ready.len(), alloc);
        {
            let view = View { fill: &*self, env };
            let tasks = &self.tasks;
            par.map(
                ready.len(),
                |j| solve::<N, E::Facts, View<'_, '_, N, E>, E>(&tasks[ready[j]].members, env.facts(), &view, env, env.here(), alloc),
                &mut results,
            );
        }
        for (j, result) in results.into_iter().enumerate() {
            let at = ready[j];
            self.report.entries += 1;
            match result {
                Err(error) => self.fail(at, error),
                Ok((outcome, tally)) => {
                    self.count(&tally);
                    match outcome {
                        Outcome::Done(all) => {
                            for r in all {
                                self.done.insert(r.scope.module, r);
                            }
                            self.tasks[at].state = State::Done;
                        }
                        Outcome::Need(wants) => {
                            self.report.needs += 1;
                            self.report.wasted_steps += tally.steps;
                            let mut on = AVec::new_in(alloc);
                            let mut unexpanded = None;
                            for want in wants.iter() {
                                match *want {
                                    Want::Scope(m) => on.push(m),
                                    Want::Expansion(request) => unexpanded = Some(request.call),
                                }
                            }
                            match unexpanded {
                                Some(call) => {
                                    let module = self.tasks[at].members[0];
                                    self.fail(at, TaskError::Unexpanded { module, call });
                                }
                                None => self.tasks[at].state = State::Waiting(on),
                            }
                        }
                    }
                }
            }
        }
    }

    fn count(&mut self, tally: &Tally) {
        let r = &mut self.report;
        r.steps += tally.steps;
        r.reads += tally.reads;
        r.expansions += tally.expansions;
        r.failed_reads += tally.failed_reads;
        r.no_scope_reads += tally.no_scope_reads;
        r.unknown_reads += tally.unknown_reads;
        r.fill.add(&tally.fill);
    }

    /// Nothing is ready: each sink component of the wait graph among the waiting tasks becomes
    /// one group task; a task that waits on no task is failed.
    fn group(&mut self) {
        let alloc = self.alloc;
        let mut pending: AVec<'_, usize> = AVec::new_in(alloc);
        let mut local: HashMap<usize, usize, Alloc<'_>> = HashMap::new_in(alloc);
        for (at, task) in self.tasks.iter().enumerate() {
            if let State::Waiting(_) = task.state {
                local.insert(at, pending.len());
                pending.push(at);
            }
        }
        let mut edges: AVec<'_, AVec<'_, usize>> = AVec::with_capacity_in(pending.len(), alloc);
        for &at in pending.iter() {
            let mut out = AVec::new_in(alloc);
            if let State::Waiting(on) = &self.tasks[at].state {
                for m in on.iter() {
                    if self.finished(*m) {
                        continue;
                    }
                    if let Some(t) = self.task_of.get(m).and_then(|t| local.get(t)) {
                        out.push(*t);
                    }
                }
            }
            out.sort_unstable();
            out.dedup();
            edges.push(out);
        }
        let components = sccs(&edges, alloc);
        let mut grouped = false;
        let mut in_component = AVec::with_capacity_in(pending.len(), alloc);
        for _ in 0..pending.len() {
            in_component.push(usize::MAX);
        }
        for (c, component) in components.iter().enumerate() {
            for &v in component.iter() {
                in_component[v] = c;
            }
        }
        for (c, component) in components.iter().enumerate() {
            let sink = component.iter().all(|&v| edges[v].iter().all(|&t| in_component[t] == c));
            if !sink {
                continue;
            }
            grouped = true;
            if component.len() == 1 && edges[component[0]].is_empty() {
                let at = pending[component[0]];
                let module = self.tasks[at].members[0];
                self.fail(at, TaskError::WaitsOnNothing { module });
                continue;
            }
            let first = pending[component[0]];
            let mut members = AVec::new_in(alloc);
            for &v in component.iter() {
                let at = pending[v];
                members.extend_from_slice(&self.tasks[at].members);
                if at != first {
                    self.tasks[at].state = State::Merged;
                    self.tasks[at].members.clear();
                }
            }
            members.sort_unstable();
            members.dedup();
            for &m in members.iter() {
                self.task_of.insert(m, first);
            }
            self.report.groups += 1;
            self.report.largest = self.report.largest.max(members.len());
            self.tasks[first].members = members;
            self.tasks[first].state = State::Ready;
        }
        if !grouped {
            for &at in pending.iter() {
                let module = self.tasks[at].members[0];
                self.fail(at, TaskError::Stalled { module });
            }
        }
    }
}
