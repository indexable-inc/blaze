//! Functions compiled again while the program runs, at the level the time spent in them
//! earns them (issue #6). Every function starts at level 1, Cranelift's code, which is what
//! a build and an edit make. The daemon samples the running program and asks for the hot
//! ones at a higher level:
//!
//! 1. **Cranelift**: the code of every build.
//! 2. **LLVM -O3**: the same IR with the hot functions it calls built into it, across crates
//!    and the standard library alike, optimized by LLVM. It counts how often it is entered
//!    and which way each two-way branch goes, in [`ra_jit_live_counters`].
//! 3. **LLVM -O3 + PGO**: the same IR again, without the counting, with what level 2 counted
//!    given to LLVM as branch weights and an entry count (`!prof`).
//!
//! The code of a level above 1 goes into a patch like any edit's: the patch defines what
//! differs from the last full build, and a tiered function differs. It is not kept in the
//! store, as a profile is not something an entry can be keyed by.
//!
//! An edit wins. Code of a level above 1 is kept with the print of the Cranelift code of the
//! function and of every function built into it; when one of those is another, the function
//! is at level 1 again in that same build, and [`Tiers::take_demoted`] says so.
//!
//! [`ra_jit_live_counters`]: https://docs.rs/ra-jit-live

use std::{
    cell::RefCell,
    sync::{Arc, Mutex},
    time::Duration,
};

use cranelift_codegen::ir::{self, InstBuilder, Opcode, types};
use hir_def::{FunctionId, HasModule, attrs::AttrFlags};
use rustc_hash::{FxHashMap, FxHashSet};

use hir_ty::{db::HirDatabase, next_solver::StoredGenericArgs, traits::StoredParamEnvAndCrate};

use super::{
    CodegenError, Instance, InstanceDef, Result,
    fragment::{CompiledFunction, Recorder, Wants},
    instance_serial, root_serial,
};

/// The symbol of the function that hands out the counters, which `ra-jit-live` defines in
/// every program that takes edits.
pub const COUNTERS: &str = "ra_jit_live_counters";

/// How deep functions are built into each other; how many instructions a hot body built in
/// may have, and any other, counted with its locals out of the frame; how many a body LLVM
/// is shown to decide on itself may have; and how many all that may add up to.
const DEPTH: u32 = 10;
const HOT_INSTRUCTIONS: usize = 4000;
const SMALL_INSTRUCTIONS: usize = 600;
const SEEN_INSTRUCTIONS: usize = 2000;
const BUDGET: usize = 40_000;
const SEEN_BUDGET: usize = 10_000;
/// How many instructions a body a build's compile builds in may have ([`for_build`]): what
/// is larger is shown to LLVM, whose own measure of each call decides, so that what a
/// function calls first does not use up [`BUDGET`] before its loops are reached.
const BUILT_IN: usize = 120;
/// How many instructions the bodies a build's compile builds in and shows LLVM may add up to
/// for each call of the function itself, with all they call in turn, and for the whole
/// function. It shows them at any depth: the callees of a body it built in too. Each call has
/// its own share, so that what the function calls first does not leave its loops nothing.
const CALL_BUDGET: usize = 8_000;
const WHOLE_BUDGET: usize = 120_000;
/// How deep a build's compile builds in and shows bodies: an iterator's chain of adapters is
/// that deep before its loop reaches `Vec::push` and what that calls (`from_iter`, `extend`,
/// `for_each`, `fold`, two closures, `push`, `reserve`, `needs_to_grow`, `capacity`). At
/// [`DEPTH`] the build of the kernels was refused 4323 bodies for their depth, and `words`
/// took 1.43 times release's time rather than 1.36 (dev-aws-6, 2026-10-21).
const BUILD_DEPTH: u32 = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Level {
    Cranelift = 1,
    Llvm = 2,
    Pgo = 3,
}

impl Level {
    pub fn number(self) -> u8 {
        self as u8
    }
}

/// A function of the last full build, as the daemon asks for it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Node(pub(super) Kind);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum Kind {
    Root(FunctionId),
    Instance(Instance, StoredParamEnvAndCrate),
}

impl Node {
    fn serial(&self, db: &dyn HirDatabase) -> u128 {
        match &self.0 {
            Kind::Root(func) => root_serial(db, *func),
            Kind::Instance(instance, env) => instance_serial(db, instance.clone(), env.clone()),
        }
    }
}

/// What level 2 counted of a function: how often it was entered, and for each two-way branch
/// of its IR in order, how often it went each way.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Branches {
    pub entered: u64,
    pub taken: Vec<[u64; 2]>,
}

/// What the daemon asks of one function.
#[derive(Debug, Clone)]
pub struct Asked {
    pub node: Node,
    pub level: Level,
    /// The hot functions that may be built into it where it calls them.
    pub hot: Vec<Node>,
    /// At level 3, what level 2 counted.
    pub profile: Option<Branches>,
}

/// The code of a function at a level above 1.
pub struct Tiered {
    pub level: Level,
    code: CompiledFunction,
    /// The print of its Cranelift code, and of the code of each function built into it, when
    /// it was compiled: if one is another, an edit changed it.
    serial: u128,
    inlined: Vec<(Node, u128)>,
    /// How many two-way branches its IR has, and the first counter it counts in.
    pub branches: u32,
    pub counters: Option<u32>,
    /// How many functions were built into it, and how long it took.
    pub inlined_count: usize,
    pub took: Duration,
}

impl Tiered {
    /// How many counters from [`Tiered::counters`] on it uses: the entry, then two for each
    /// branch.
    pub fn counters_used(&self) -> u32 {
        self.counters.map_or(0, |_| 1 + 2 * self.branches)
    }

    /// What its code calls by the name of a symbol, such as the C library's functions.
    pub fn symbols(&self) -> impl Iterator<Item = &str> {
        self.code.requests.funcs.iter().filter_map(|it| match &it.target {
            super::fragment::FuncTarget::Symbol(name) => Some(name.as_str()),
            _ => None,
        })
    }
}

/// The functions of a program at a level above 1.
#[derive(Default, Clone)]
pub struct Tiers {
    nodes: FxHashMap<Node, Arc<Tiered>>,
    /// What was installed since a patch last held the code, with the code each replaced.
    fresh: Vec<(Node, Option<Arc<Tiered>>)>,
    /// Code installed together with code no patch could hold, to install again one by one.
    trial: Vec<(Node, Arc<Tiered>)>,
    /// The functions an edit sent back to level 1, and why, since this was last asked.
    demoted: Arc<Mutex<Vec<(Node, String)>>>,
    /// The functions whose code no patch could hold, the level they are back at, and why,
    /// since this was last asked.
    refused: Vec<(Node, Level, String)>,
}

impl Tiers {
    pub fn install(&mut self, node: Node, tiered: Tiered) {
        self.put(node, Arc::new(tiered));
    }

    fn put(&mut self, node: Node, tiered: Arc<Tiered>) {
        let was = self.nodes.insert(node.clone(), tiered);
        self.fresh.push((node, was));
    }

    /// A patch holds the code installed so far. `true` when code installed with code no
    /// patch could hold is installed again, one function of it, for a patch to hold next.
    pub fn held(&mut self) -> bool {
        self.fresh.clear();
        let Some((node, tiered)) = self.trial.pop() else { return false };
        self.put(node, tiered);
        true
    }

    /// Puts back the code that what was installed since a patch last held it replaced, as no
    /// patch can hold it: a build that is not a patch would link the program again and start
    /// it over. Of several functions installed together, which one it is is found by
    /// installing them again one by one, from the first here on. `false` when nothing was
    /// installed since.
    pub fn refuse(&mut self, why: &str) -> bool {
        if self.fresh.is_empty() {
            return false;
        }
        let alone = self.fresh.len() == 1;
        for (node, was) in std::mem::take(&mut self.fresh).into_iter().rev() {
            let level = was.as_ref().map_or(Level::Cranelift, |it| it.level);
            let tiered = match was {
                Some(was) => self.nodes.insert(node.clone(), was),
                None => self.nodes.remove(&node),
            };
            match tiered {
                Some(tiered) if !alone => self.trial.push((node, tiered)),
                _ => {
                    self.refused.retain(|it| it.0 != node);
                    self.refused.push((node, level, why.to_owned()));
                }
            }
        }
        if let Some((node, tiered)) = self.trial.pop() {
            self.put(node, tiered);
        }
        true
    }

    /// The functions whose code a patch could not hold since the last call.
    pub fn take_refused(&mut self) -> Vec<(Node, Level, String)> {
        std::mem::take(&mut self.refused)
    }

    pub fn level(&self, node: &Node) -> Level {
        self.nodes.get(node).map_or(Level::Cranelift, |it| it.level)
    }

    pub fn get(&self, node: &Node) -> Option<&Tiered> {
        self.nodes.get(node).map(|it| &**it)
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Leaves the code installed as it is, with nothing more to install: a full build that may
    /// yet be undone is made without it, and [`Tiers::clear`] follows once it is kept.
    pub fn stop_trials(&mut self) {
        self.fresh.clear();
        self.trial.clear();
    }

    /// Sends every function back to level 1, as a full build does.
    pub fn clear(&mut self) {
        self.fresh.clear();
        self.trial.clear();
        let mut demoted = self.demoted.lock().unwrap_or_else(|e| e.into_inner());
        for (node, _) in self.nodes.drain() {
            demoted.push((node, "the program was linked again".to_owned()));
        }
    }

    /// The functions a build since the last call found edited, which are at level 1 again.
    pub fn take_demoted(&mut self) -> Vec<(Node, String)> {
        let demoted = std::mem::take(&mut *self.demoted.lock().unwrap_or_else(|e| e.into_inner()));
        for (node, _) in &demoted {
            self.nodes.remove(node);
            self.fresh.retain(|it| it.0 != *node);
            self.trial.retain(|it| it.0 != *node);
        }
        demoted
    }

    pub(super) fn nodes(&self) -> impl Iterator<Item = &Node> {
        self.nodes.keys()
    }

    /// The code of `node` at its level, unless it has none above 1 or an edit changed what
    /// that code was compiled from.
    pub(super) fn code(&self, db: &dyn HirDatabase, node: &Node) -> Option<&CompiledFunction> {
        let tiered = self.nodes.get(node)?;
        let edited = if node.serial(db) != tiered.serial {
            Some("edited")
        } else if tiered.inlined.iter().any(|(inlined, serial)| inlined.serial(db) != *serial) {
            Some("a function built into it was edited")
        } else {
            None
        };
        if let Some(why) = edited {
            let mut demoted = self.demoted.lock().unwrap_or_else(|e| e.into_inner());
            if !demoted.iter().any(|(it, _)| it == node) {
                demoted.push((node.clone(), why.to_owned()));
            }
            return None;
        }
        Some(&tiered.code)
    }
}

/// What a compile at a level above 1 builds in and counts.
pub(super) struct Request {
    level: Level,
    counters: Option<u32>,
    profile: Option<Branches>,
    branches: std::cell::Cell<u32>,
}

/// The hot functions the compile under way on this thread may build into what it compiles.
struct Inlining {
    roots: FxHashSet<FunctionId>,
    /// By what they are and their generic arguments: whether a call checks overflow is the
    /// caller's to say, which the last build may have settled otherwise.
    instances: FxHashSet<(InstanceDef, StoredGenericArgs)>,
    inlined: Vec<(Instance, StoredParamEnvAndCrate)>,
    /// Of a build's compile ([`for_build`]) rather than of one at a level above 1.
    whole: bool,
    /// What [`Inlining::spent`] and [`Inlining::seen`] added up to when the call of the
    /// function itself that is being built in or shown was reached.
    base: usize,
    /// How often a body was not built in or shown for want of room: a body built meanwhile
    /// is not kept for the next call of it, which may have room.
    starved: usize,
    depth: u32,
    /// How many instructions were built in so far, and shown to LLVM in bodies of their own.
    spent: usize,
    seen: usize,
    /// The bodies built so far, by what they are of: `None` for one that is not built in.
    built: FxHashMap<
        (InstanceDef, StoredGenericArgs, bool),
        Option<std::rc::Rc<super::fragment::Inlinable>>,
    >,
    /// The size of a body that was built and then found too large, which a build of it with
    /// more room only adds to: it is not built again for a call that allows no more. A body
    /// built while its callees found no room is not kept in [`Inlining::built`], and what it
    /// counted is given back ([`untally`]), so each call would build it again, and each of
    /// its callees, at every depth.
    oversized: FxHashMap<(InstanceDef, StoredGenericArgs, bool), usize>,
}

thread_local! {
    static INLINING: RefCell<Option<Inlining>> = const { RefCell::new(None) };
}

/// Runs `work` with no compile at a level above 1 under way on the thread, for what the
/// database keeps: its result must not depend on what that compile builds in.
pub(super) fn plain<T>(work: impl FnOnce() -> T) -> T {
    let held = INLINING.with(|it| it.borrow_mut().take());
    let out = work();
    INLINING.with(|it| *it.borrow_mut() = held);
    out
}

/// As [`plain`], for the compile of a function of a build. In a build of a whole program or
/// with `RA_JIT_INLINE=all` (see [`across`]), and LLVM at `-O2` or above, `work` builds in
/// and shows LLVM what the function calls of any crate, as a compile at a level above 1
/// does with no function hot: what it builds in then depends on the database alone. A build
/// whose compiles import what they call ([`super::imports`]) builds nothing in here.
pub(super) fn for_build<T>(work: impl FnOnce() -> T) -> T {
    #[cfg(feature = "llvm")]
    let imports = super::imports::enabled();
    #[cfg(not(feature = "llvm"))]
    let imports = false;
    let fresh = (across() && !imports).then(|| Inlining {
        roots: FxHashSet::default(),
        instances: FxHashSet::default(),
        inlined: Vec::new(),
        whole: true,
        base: 0,
        starved: 0,
        depth: 0,
        spent: 0,
        seen: 0,
        built: FxHashMap::default(),
        oversized: FxHashMap::default(),
    });
    let held = INLINING.with(|it| std::mem::replace(&mut *it.borrow_mut(), fresh));
    let out = work();
    INLINING.with(|it| *it.borrow_mut() = held);
    out
}

/// Whether a build's compiles build in what they call across crates: in a build of a whole
/// program unless `RA_JIT_INLINE=crate` (or `0`, which builds nothing in), and in any build
/// with `RA_JIT_INLINE=all`.
/// The builds of a process that builds edits of them do not, whatever it says: an entry of
/// the store, and a function an edit's patch compares with the build before, is keyed by its
/// own function alone, so a body built into a function of another crate would outlive an
/// edit of it.
pub(super) fn across() -> bool {
    static ASKED: std::sync::LazyLock<Option<bool>> =
        std::sync::LazyLock::new(|| match std::env::var("RA_JIT_INLINE").as_deref() {
            Ok("all") => Some(true),
            Ok("crate" | "0") => Some(false),
            _ => None,
        });
    !super::backend::edits_follow()
        && super::backend::optimizing()
        && ASKED.unwrap_or_else(super::backend::whole_program)
}

/// How large a body of `instance` the compile under way may build into its caller, if it may
/// build it in at all: a hot one up to [`HOT_INSTRUCTIONS`], any other up to
/// [`SMALL_INSTRUCTIONS`], as rustc builds in what is small. The body is then built with
/// [`hot_body`].
pub(super) fn inline_limit(db: &dyn HirDatabase, instance: &Instance) -> Option<usize> {
    let func = match instance.def {
        InstanceDef::Fn(func) => Some(func),
        InstanceDef::Closure(_) => None,
        _ => return None,
    };
    INLINING.with(|it| {
        let mut it = it.borrow_mut();
        let inlining = it.as_mut()?;
        let spent = if inlining.whole { !inlining.has_room() } else { inlining.spent >= BUDGET };
        if inlining.depth >= if inlining.whole { BUILD_DEPTH } else { DEPTH } || spent {
            inlining.starved += usize::from(inlining.whole);
            return None;
        }
        if func
            .is_some_and(|func| AttrFlags::query(db, func.into()).contains(AttrFlags::INLINE_NEVER))
        {
            return None;
        }
        if inlining.whole && !shared(db, instance) {
            return None;
        }
        Some(if inlining.whole {
            BUILT_IN
        } else if inlining.is_hot(instance) {
            HOT_INSTRUCTIONS
        } else {
            SMALL_INSTRUCTIONS
        })
    })
}

/// Whether a build may build `instance` in, or show it to LLVM: one of the standard library
/// only if it is generic or `#[inline]`, as rustc does across crates. The library keeps what
/// the rest of its functions refer to to itself (the `static`s of `std::rt`), and has them
/// compiled already.
pub(super) fn shared(db: &dyn HirDatabase, instance: &Instance) -> bool {
    let InstanceDef::Fn(func) = instance.def else { return true };
    !func.krate(db).data(db).origin.is_lang()
        || AttrFlags::query(db, func.into()).contains(AttrFlags::IS_INLINE)
        || instance.args.as_ref().iter().any(|it| it.region().is_none())
}

impl Inlining {
    /// Whether a build's compile may build in or show more, at this depth.
    fn has_room(&self) -> bool {
        let used = self.spent + self.seen;
        used < WHOLE_BUDGET && (self.depth == 0 || used - self.base < CALL_BUDGET)
    }

    fn is_hot(&self, instance: &Instance) -> bool {
        self.instances.contains(&(instance.def, instance.args.clone()))
            || matches!(instance.def, InstanceDef::Fn(func)
                if instance.args.as_ref().iter().all(|it| it.region().is_some())
                    && self.roots.contains(&func))
    }
}

/// The body of a hot callee, built to be put where it is called, with the hot functions it
/// calls built into it in turn, if it has at most `limit` instructions once its locals are
/// out of the frame.
pub(super) fn hot_body(
    db: &dyn HirDatabase,
    instance: &Instance,
    env: &StoredParamEnvAndCrate,
    limit: usize,
    tracked: bool,
) -> Option<std::rc::Rc<super::fragment::Inlinable>> {
    if let Some(known) = oversized(instance, tracked, limit) {
        refused(db, instance, &format!("too large ({known} instructions)"));
        return None;
    }
    let before = tally();
    let Some(body) = body(db, instance, env, tracked) else {
        untally(before);
        return None;
    };
    if size(&body.func) > limit {
        untally(before);
        oversize(instance, tracked, size(&body.func));
        refused(db, instance, &format!("too large ({} instructions)", size(&body.func)));
        return None;
    }
    spend(instance, env, &body);
    Some(body)
}

/// How large a build of `instance` was found, if that is more than `limit`.
fn oversized(instance: &Instance, tracked: bool, limit: usize) -> Option<usize> {
    let key = (instance.def, instance.args.clone(), tracked);
    INLINING.with(|it| it.borrow().as_ref()?.oversized.get(&key).copied().filter(|&it| it > limit))
}

fn oversize(instance: &Instance, tracked: bool, size: usize) {
    let key = (instance.def, instance.args.clone(), tracked);
    INLINING.with(|it| {
        if let Some(it) = it.borrow_mut().as_mut() {
            let known = it.oversized.entry(key).or_default();
            *known = (*known).max(size);
        }
    });
}

/// What the compile under way has built in and shown so far.
fn tally() -> Option<(usize, usize)> {
    INLINING.with(|it| it.borrow().as_ref().map(|it| (it.spent, it.seen)))
}

/// Takes back what building a body counted of the bodies it built in and showed, when the
/// body is not used: else a body too large to build in, at the head of a long chain, uses
/// up the share of its call, and is then not shown either (`vec![false; n]`'s
/// `SpecFromElem::from_elem`, whose length LLVM then does not know).
fn untally(before: Option<(usize, usize)>) {
    INLINING.with(|it| {
        if let (Some(it), Some((spent, seen))) = (it.borrow_mut().as_mut(), before) {
            (it.spent, it.seen) = (spent, seen);
        }
    });
}

/// The body of a callee that the compile under way calls rather than builds in, for LLVM to
/// see what it does: one that is `#[inline(never)]` and hot, whose call LLVM keeps and knows
/// then what it reads and writes, or one too large for [`inline_limit`], which LLVM builds in
/// itself where its own measure, after optimization, finds it small enough. What its
/// `#[inline]` asks comes with it.
pub(super) fn seen_body(
    db: &dyn HirDatabase,
    instance: &Instance,
    env: &StoredParamEnvAndCrate,
    tracked: bool,
) -> Option<(std::rc::Rc<super::fragment::Inlinable>, super::fragment::Hint)> {
    let func = match instance.def {
        InstanceDef::Fn(func) => Some(func),
        InstanceDef::Closure(_) | InstanceDef::DropGlue => None,
        _ => return None,
    };
    // Bodies built in show theirs too: one call left opaque, such as a drop on the unwind path,
    // has LLVM take every pointer passed to it as escaped and load it again after each store.
    let (hot, room) = INLINING.with(|it| {
        let mut it = it.borrow_mut();
        let inlining = it.as_mut()?;
        let room = match inlining.whole {
            true if inlining.depth >= BUILD_DEPTH => Err("not shown: depth"),
            true if !inlining.has_room() => Err("not shown: budget"),
            true => Ok(shared(db, instance)),
            false if inlining.depth >= DEPTH => Err("not shown: depth"),
            false => Ok(inlining.seen < SEEN_BUDGET),
        };
        if room.is_err() && inlining.whole {
            inlining.starved += 1;
        }
        Some((inlining.is_hot(instance), room))
    })?;
    let room = match room {
        Ok(room) => room,
        Err(why) => {
            refused(db, instance, why);
            return None;
        }
    };
    if !room {
        refused(db, instance, "not shown: budget");
        return None;
    }
    let flags = func.map(|func| AttrFlags::query(db, func.into()));
    let has = |flag| flags.is_some_and(|it| it.contains(flag));
    let hint = if has(AttrFlags::INLINE_NEVER) {
        super::fragment::Hint::Never
    } else if has(AttrFlags::INLINE_ALWAYS) {
        super::fragment::Hint::Always
    } else if has(AttrFlags::IS_INLINE) {
        super::fragment::Hint::Inline
    } else {
        super::fragment::Hint::None
    };
    let limit = if hot { HOT_INSTRUCTIONS } else { SEEN_INSTRUCTIONS };
    if hint == super::fragment::Hint::Never && !hot {
        return None;
    }
    if let Some(known) = oversized(instance, tracked, limit) {
        refused(db, instance, &format!("not shown: {known} instructions"));
        return None;
    }
    let before = tally();
    let Some(body) = body(db, instance, env, tracked) else {
        untally(before);
        return None;
    };
    if size(&body.func) > limit {
        untally(before);
        oversize(instance, tracked, size(&body.func));
        refused(db, instance, &format!("not shown: {} instructions", size(&body.func)));
        return None;
    }
    INLINING.with(|it| {
        if let Some(it) = it.borrow_mut().as_mut() {
            it.seen += size(&body.func);
            if !it.inlined.iter().any(|(known, _)| known == instance) {
                it.inlined.push((instance.clone(), env.clone()));
            }
        }
    });
    Some((body, hint))
}

/// The body of `instance`, built once per compile.
fn body(
    db: &dyn HirDatabase,
    instance: &Instance,
    env: &StoredParamEnvAndCrate,
    tracked: bool,
) -> Option<std::rc::Rc<super::fragment::Inlinable>> {
    let key = (instance.def, instance.args.clone(), tracked);
    let known = INLINING.with(|it| it.borrow().as_ref()?.built.get(&key).cloned());
    match known {
        Some(body) => body,
        None => {
            // A body that calls itself, while it is built, is not built in again.
            let starved = INLINING.with(|it| {
                let mut it = it.borrow_mut();
                let it = it.as_mut()?;
                it.built.insert(key.clone(), None);
                Some(it.starved)
            });
            let body = build_body(db, instance, env, tracked).map(std::rc::Rc::new);
            INLINING.with(|it| {
                if let Some(it) = it.borrow_mut().as_mut() {
                    if Some(it.starved) == starved {
                        it.built.insert(key, body.clone());
                    } else {
                        it.built.remove(&key);
                    }
                }
            });
            body
        }
    }
}

/// How many instructions `func` has: those of its blocks, not those a pass took out.
fn size(func: &ir::Function) -> usize {
    func.layout.blocks().map(|block| func.layout.block_insts(block).count()).sum()
}

/// Has each two-way branch on a constant go where the constant sends it, and drops the blocks
/// no path reaches then: the checks `ub_checks()` turns off would else be built into every
/// caller of a body that has one, and counted.
pub(super) fn fold_known_branches(
    ctx: &mut cranelift_codegen::Context,
    isa: &dyn cranelift_codegen::isa::TargetIsa,
) {
    let func = &mut ctx.func;
    let mut folded = false;
    let blocks: Vec<ir::Block> = func.layout.blocks().collect();
    for block in blocks {
        let Some(inst) = func.layout.last_inst(block) else { continue };
        let ir::InstructionData::Brif { arg, blocks: [then, otherwise], .. } = func.dfg.insts[inst]
        else {
            continue;
        };
        let Some(known) = known_value(func, arg) else { continue };
        let taken = if known != 0 { then } else { otherwise };
        let pool = &func.dfg.value_lists;
        let (to, args) = (taken.block(pool), taken.args(pool).collect::<Vec<_>>());
        func.replace(inst).jump(to, &args);
        folded = true;
    }
    if folded {
        ctx.flowgraph();
        // Only a verifier enabled by the flags can fail it, on IR it took apart.
        _ = ctx.eliminate_unreachable_code(isa);
    }
}

/// The value of `value`, where it is a constant through extensions and comparisons with one.
fn known_value(func: &ir::Function, value: ir::Value) -> Option<i64> {
    use cranelift_codegen::ir::{InstructionData, condcodes::IntCC};
    let inst = func.dfg.value_def(value).inst()?;
    match func.dfg.insts[inst] {
        InstructionData::UnaryImm { opcode: Opcode::Iconst, imm } => {
            let bits = func.dfg.value_type(value).bits();
            let imm = imm.bits();
            Some(if bits >= 64 { imm } else { imm & ((1i64 << bits) - 1) })
        }
        InstructionData::Unary { opcode: Opcode::Uextend | Opcode::Ireduce, arg } => {
            let bits = func.dfg.value_type(value).bits();
            let known = known_value(func, arg)?;
            Some(if bits >= 64 { known } else { known & ((1i64 << bits) - 1) })
        }
        InstructionData::IntCompare { opcode: Opcode::Icmp, cond, args: [l, r] } => {
            let (l, r) = (known_value(func, l)?, known_value(func, r)?);
            let holds = match cond {
                IntCC::Equal => l == r,
                IntCC::NotEqual => l != r,
                _ => return None,
            };
            Some(i64::from(holds))
        }
        _ => None,
    }
}

/// Counts `body` as built into the compile under way, which an edit of it then sends back
/// to level 1.
fn spend(instance: &Instance, env: &StoredParamEnvAndCrate, body: &super::fragment::Inlinable) {
    INLINING.with(|it| {
        if let Some(it) = it.borrow_mut().as_mut() {
            it.spent += size(&body.func);
            if !it.inlined.iter().any(|(known, _)| known == instance) {
                it.inlined.push((instance.clone(), env.clone()));
            }
        }
    });
}

/// Says why a body is not built in, with `RA_JIT_DUMP_TIER`.
fn refused(db: &dyn HirDatabase, instance: &Instance, why: &str) {
    if std::env::var_os("RA_JIT_DUMP_TIER").is_none() {
        return;
    }
    let name = match instance.def {
        InstanceDef::Fn(func) => {
            hir_def::signatures::FunctionSignature::of(db, func).name.as_str().to_owned()
        }
        InstanceDef::DropGlue => "drop glue".to_owned(),
        _ => "a closure".to_owned(),
    };
    eprintln!("tier: {name} is not built in: {why}");
}

fn build_body(
    db: &dyn HirDatabase,
    instance: &Instance,
    env: &StoredParamEnvAndCrate,
    tracked: bool,
) -> Option<super::fragment::Inlinable> {
    let refused = |why: &str| refused(db, instance, why);
    if let InstanceDef::Fn(func) = instance.def {
        let generic = db.mir_body(func.into()).ok()?;
        if generic.coroutine.is_some() {
            refused("it is a coroutine");
            return None;
        }
    }
    // Drop glue is built from the type it drops, and has no MIR.
    let body = match instance.def {
        InstanceDef::DropGlue => None,
        _ => match super::instance_body(db, instance, env.clone()) {
            Ok(body) => Some(body),
            Err(_) => {
                refused("no body");
                return None;
            }
        },
    };
    INLINING.with(|it| {
        if let Some(it) = it.borrow_mut().as_mut() {
            if it.depth == 0 {
                it.base = it.spent + it.seen;
            }
            it.depth += 1;
        }
    });
    let built = (|| {
        let mut cg = Recorder::new(
            db,
            &**super::target().ok()?,
            super::selected_target().ok()?,
            instance.checked,
        );
        cg.unlocated = true;
        let args = instance.args.clone();
        let mut ctx =
            super::build(&mut cg, env.as_ref(db), body.as_deref(), tracked, |fx| match fx.body {
                Some(_) => {
                    fx.args = Some(args);
                    fx.codegen()
                }
                None => {
                    use rustc_type_ir::inherent::GenericArgs as _;
                    fx.drop_glue(args.as_ref().type_at(0))
                }
            })
            .map_err(|e| refused(&e.to_string()))
            .ok()?;
        if cg.naked.is_some() || cg.opaque {
            refused("naked or opaque");
            return None;
        }
        super::inline(&mut ctx, &cg.inlined).map_err(|e| refused(&e.to_string())).ok()?;
        // Its size is measured with its locals out of the frame, as the caller has them.
        super::promote::promote(&mut ctx.func, cg.named_slots());
        fold_known_branches(&mut ctx, cg.isa);
        let pointees = std::mem::take(&mut cg.pointees);
        let seen = std::mem::take(&mut cg.seen);
        let inlinable = super::fragment::Inlinable::of(ctx.func, cg.finish().0, pointees, seen);
        if inlinable.is_none() {
            refused("its data is out of order");
        }
        inlinable
    })();
    INLINING.with(|it| {
        if let Some(it) = it.borrow_mut().as_mut() {
            it.depth -= 1;
        }
    });
    built
}

/// Compiles the function `asked` names at the level it asks for: [`prepare`], then
/// [`Prepared::finish`] with no counters to count in.
pub fn compile(db: &dyn HirDatabase, asked: &Asked) -> std::result::Result<Tiered, String> {
    prepare(db, asked)?.finish(None)
}

/// A function of [`prepare`]: its IR, with all it took of the database.
pub struct Prepared {
    level: Level,
    profile: Option<Branches>,
    pending: super::Pending,
    serial: u128,
    inlined: Vec<(Node, u128)>,
    start: std::time::Instant,
}

/// Builds the IR of the function `asked` names, with what it builds in, for LLVM to compile
/// at the level asked for once the database is let go of (an edit is then applied at once).
pub fn prepare(db: &dyn HirDatabase, asked: &Asked) -> std::result::Result<Prepared, String> {
    let start = std::time::Instant::now();
    if asked.level == Level::Cranelift {
        return Err("level 1 is what every build compiles".to_owned());
    }
    let serial = asked.node.serial(db);
    if serial == 0 {
        return Err("Cranelift does not compile it".to_owned());
    }
    let mut inlining = Inlining {
        roots: FxHashSet::default(),
        instances: FxHashSet::default(),
        inlined: Vec::new(),
        whole: false,
        base: 0,
        starved: 0,
        depth: 0,
        spent: 0,
        seen: 0,
        built: FxHashMap::default(),
        oversized: FxHashMap::default(),
    };
    for node in &asked.hot {
        match &node.0 {
            Kind::Root(func) => _ = inlining.roots.insert(*func),
            Kind::Instance(instance, _) => {
                _ = inlining.instances.insert((instance.def, instance.args.clone()))
            }
        }
    }
    if std::env::var_os("RA_JIT_DUMP_TIER").is_some() {
        eprintln!(
            "tier: {:?} at {:?}: {} roots and {} instances may be built in",
            asked.node,
            asked.level,
            inlining.roots.len(),
            inlining.instances.len()
        );
        for (def, args) in &inlining.instances {
            if let InstanceDef::Fn(func) = def {
                let name =
                    hir_def::signatures::FunctionSignature::of(db, *func).name.as_str().to_owned();
                eprintln!("tier:   hot {name} {args:?}");
            }
        }
    }
    let held = INLINING.with(|it| it.borrow_mut().replace(inlining));
    let compiled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match &asked.node.0 {
        Kind::Root(func) => super::root_code(db, *func, true),
        Kind::Instance(instance, env) => {
            super::instance_code(db, instance.clone(), env.clone(), super::Detach::Tier)
        }
    }));
    let inlining = INLINING.with(|it| std::mem::replace(&mut *it.borrow_mut(), held));
    let pending = match compiled {
        Ok(Ok(super::Produced::Pending(pending))) => pending,
        Ok(Ok(super::Produced::Code(_))) => return Err("its code is its assembly".to_owned()),
        Ok(Err(e)) => return Err(e.to_string()),
        Err(panic) => std::panic::resume_unwind(panic),
    };
    let mut inlined = Vec::new();
    for (instance, env) in inlining.map(|it| it.inlined).unwrap_or_default() {
        let node = match instance.def {
            InstanceDef::Fn(func)
                if instance.args.as_ref().iter().all(|it| it.region().is_some()) =>
            {
                Node(Kind::Root(func))
            }
            _ => Node(Kind::Instance(instance, env)),
        };
        if !inlined.iter().any(|(it, _)| *it == node) {
            let serial = node.serial(db);
            inlined.push((node, serial));
        }
    }
    Ok(Prepared {
        level: asked.level,
        profile: asked.profile.clone().filter(|_| asked.level == Level::Pgo),
        pending,
        serial,
        inlined,
        start,
    })
}

impl Prepared {
    /// LLVM's code of the function, which takes nothing of the database. Code of level 2
    /// counts in the counters from the first of `counters` on, if all it counts fits in the
    /// second, how many there are.
    pub fn finish(self, counters: Option<(u32, u32)>) -> std::result::Result<Tiered, String> {
        let Prepared { level, profile, pending, serial, inlined, start } = self;
        let branches = branches(pending.func()).len() as u32;
        let counters = counters
            .filter(|&(first, room)| level == Level::Llvm && first + 1 + 2 * branches <= room)
            .map(|(first, _)| first);
        let request = Request { level, counters, profile, branches: std::cell::Cell::new(0) };
        let code = pending.produce(&request).map_err(|e| e.to_string())?;
        Ok(Tiered {
            level,
            code,
            serial,
            inlined_count: inlined.len(),
            inlined,
            branches: request.branches.get(),
            counters: request.counters,
            took: start.elapsed(),
        })
    }
}

/// The two-way branches of `func`, in the order of its layout.
fn branches(func: &ir::Function) -> Vec<ir::Inst> {
    func.layout
        .blocks()
        .filter_map(|block| func.layout.last_inst(block))
        .filter(|&inst| func.dfg.insts[inst].opcode() == Opcode::Brif)
        .collect()
}

/// Has `func` count, in the counters from `first` on, how often it is entered and which way
/// each of `branches` goes. The counters are found by a call at the entry.
fn count(
    recorder: &mut Wants,
    func: &mut ir::Function,
    first: u32,
    branches: &[ir::Inst],
) -> Result<()> {
    use cranelift_codegen::cursor::{Cursor as _, FuncCursor};
    let pointer = super::ptr();
    let mut sig = recorder.make_signature();
    sig.returns.push(ir::AbiParam::new(pointer));
    let id = recorder.import(COUNTERS, &sig)?;
    let callee = recorder.declare_func_in_func(id, func);
    let Some(entry) = func.layout.entry_block() else {
        return Err(CodegenError::Unsupported("a function without blocks".to_owned()));
    };
    let flags = super::MEM;
    let mut cursor = FuncCursor::new(func).at_first_insertion_point(entry);
    let call = cursor.ins().call(callee, &[]);
    let base = cursor.func.dfg.first_result(call);
    let add = |cursor: &mut FuncCursor<'_>, offset: ir::Value| {
        let at = cursor.ins().iadd(base, offset);
        let count = cursor.ins().load(types::I64, flags, at, 0);
        let count = cursor.ins().iadd_imm_s(count, 1);
        cursor.ins().store(flags, count, at, 0);
    };
    let entered = cursor.ins().iconst(pointer, i64::from(first) * 8);
    add(&mut cursor, entered);
    for (nth, &inst) in branches.iter().enumerate() {
        let slot = i64::from(first) + 1 + 2 * nth as i64;
        cursor.goto_inst(inst);
        let condition = cursor.func.dfg.inst_args(inst)[0];
        let then = cursor.ins().iconst(pointer, slot * 8);
        let otherwise = cursor.ins().iconst(pointer, (slot + 1) * 8);
        let offset = cursor.ins().select(condition, then, otherwise);
        add(&mut cursor, offset);
    }
    Ok(())
}

/// The machine code of a function compiled at a level above 1: LLVM's, or an error where
/// LLVM has none, as the function then stays where it is.
pub(super) fn emit(
    recorder: &mut Wants,
    named: &[ir::StackSlot],
    func: &mut ir::Function,
    request: &Request,
) -> Result<super::fragment::Code> {
    let found = branches(func);
    request.branches.set(found.len() as u32);
    let weights = match &request.profile {
        Some(profile) if profile.taken.len() == found.len() => Some(Weights {
            entered: profile.entered,
            taken: found.iter().copied().zip(profile.taken.iter().copied()).collect(),
        }),
        _ => None,
    };
    if let Some(first) = request.counters {
        count(recorder, func, first, &found)?;
    }
    if std::env::var_os("RA_JIT_DUMP_TIER").is_some() {
        eprintln!("tier {:?}:\n{}", request.level, func.display());
    }
    produce(recorder, named, func, weights.as_ref())
}

/// What level 3 tells LLVM: how often the function is entered, and how often each branch
/// goes each way.
#[cfg_attr(not(feature = "llvm"), allow(dead_code))]
pub(super) struct Weights {
    pub(super) entered: u64,
    pub(super) taken: FxHashMap<ir::Inst, [u64; 2]>,
}

#[cfg(feature = "llvm")]
fn produce(
    recorder: &mut Wants,
    named: &[ir::StackSlot],
    func: &ir::Function,
    weights: Option<&Weights>,
) -> Result<super::fragment::Code> {
    use super::backend::{Optimization, Passes, Selection};
    let optimization = Optimization { passes: Passes::Aggressive, selection: Selection::Optimized };
    let code = super::llvm::emit(recorder, named, func, optimization, weights, &[])
        .map_err(|why| CodegenError::Unsupported(format!("LLVM has no code for it: {why}")))?;
    super::backend::pad(code)
}

#[cfg(not(feature = "llvm"))]
fn produce(
    _: &mut Wants,
    _: &[ir::StackSlot],
    _: &ir::Function,
    _: Option<&Weights>,
) -> Result<super::fragment::Code> {
    Err(CodegenError::Unsupported("ra-jit was built without `--features llvm`".to_owned()))
}
