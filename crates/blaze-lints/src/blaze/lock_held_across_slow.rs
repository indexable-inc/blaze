//! `lock_held_across_slow`: a lock guard live across a call that does IO or allocates on the
//! global heap (rule standard-structures: a lock is held only for a field update;
//! docs/v2/plan/design/named-locks.md, part 7). Every task that wants the lock queues behind
//! the holder while it waits on the disk, the network, a child or the allocator: the convoy
//! `lock_waits` ranks, found before it runs.
//!
//! A guard is [`lock_across_query::is_guard`]'s: `blaze_core::lock`'s `LockGuard`, `ReadGuard`
//! and `WriteGuard`, `lock_api`'s (`parking_lot`'s) or std's, itself or held in a tuple, an
//! array or a field. A call is slow when:
//!
//! - it reaches IO, through [`alloc::reaches_root`] with [`Roots::Io`] ([`io`]) and through the
//!   workspace's own functions: std's `fs`, `net`, `process` and stdio (`std::io::stdio`, so
//!   `Stdout`'s `Write` and `print!`), `libc`, `rustix` and `nix`, tokio and mio, and
//!   blaze-core's `Driver::{submit, poll}` (effects submitted and awaited, `run_until_idle`
//!   through them). std's locks, threads, clock, environment and panic machinery,
//!   `parking_lot`, `lock_api`, `tracing`, `log`, `blaze_mem` and blaze-core's `lock` and
//!   `wait` modules end the walk not reaching: a wait is `blocking_on_pool`'s, a clock read
//!   `wall_clock`'s;
//! - it is a method of std's `io::{Read, Write, BufRead}` on a receiver the body cannot see
//!   into (`dyn Write`, a type parameter): a stream is IO unless shown otherwise;
//! - it reaches the global allocator as `hot_alloc` finds it (the `__rust_alloc` family,
//!   `exchange_malloc`, `box_new`), not through the workspace's own functions and not a growth
//!   of a buffer the caller keeps (`guard.items.push(x)` is the field update the rule allows).
//!
//! A call of a lock's own (an acquire, a release, a guard's `Deref`) is never slow here: a
//! second lock taken under the first is lock order, not hold time. A call that may park the
//! fiber is the may-suspend check's and `held_across_wait`'s, and not repeated here.
//!
//! What is live across a call is the MIR's, as `lock_across_query` reads it
//! ([`hir_ty_mir::mir::MirBody::held_across_calls_in`] over the runtime body and its closures):
//! a guard dropped before the call is not held, however its scope is written.

use base_db::Db;
use hir::{AsAssocItem as _, HirDisplay as _};
use hir_def::{
    DefWithBodyId, FunctionId, HasModule as _,
    expr_store::Body,
    hir::{Expr, ExprId},
    signatures::FunctionSignature,
};
use hir_ty::{
    InferBodyId, ParamEnvAndCrate,
    db::HirDatabase as _,
    next_solver::{GenericArgs, TyKind},
};
use hir_ty_mir::mir::{BasicBlockId, MirBody, MirSpan, TerminatorKind};
use rustc_type_ir::inherent::{GenericArgs as _, IntoKind as _};

use super::{
    Finding, Lint,
    alloc::{self, Resolved, Roots},
    facts::{self, HOLDS_DEPTH},
    lock_across_query::is_guard,
    sans_io_core::{fn_name, root_path},
};
use crate::check::Cx;

/// The rule every message ends with.
const RULE: &str = "a lock is held only for a field update: rule standard-structures; \
                    docs/v2/plan/design/named-locks.md";

/// How many bodies (the body and its closures, nested) one body's walk reads at most.
const MOST_BODIES: usize = 256;

/// The most steps the walk from a call to the IO it reaches takes to name the root (the walk
/// itself is bounded by its memo; this bounds only the naming, rule bounded-progress).
const LONGEST_CHAIN: usize = 24;

/// The crates whose functions end the IO walk not reaching: locks, waits, tracing and frames.
const STOPS: &[&str] =
    &["parking_lot", "parking_lot_core", "lock_api", "tracing", "tracing_core", "log", "blaze_mem"];

/// What a function is to the IO walk.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Io {
    /// Walked into: its calls decide.
    Walk,
    /// Not followed: what it reaches is not IO (a lock, a wait, the clock, tracing).
    Stop,
    /// IO itself.
    Root(Kind),
}

impl Io {
    pub(super) fn is_root(self) -> bool {
        matches!(self, Io::Root(_))
    }
}

/// Which IO a root is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Kind {
    Fs,
    Net,
    Process,
    Stdio,
    Syscall,
    Async,
    Effects,
}

impl Kind {
    fn say(self) -> &'static str {
        match self {
            Kind::Fs => "the file system",
            Kind::Net => "the network",
            Kind::Process => "a process",
            Kind::Stdio => "stdio",
            Kind::Syscall => "a syscall",
            Kind::Async => "an async runtime",
            Kind::Effects => "blaze-core's driver (effects submitted or awaited)",
        }
    }
}

/// What `f` is to the IO walk, by its crate, its module path and its name: once per function.
#[blaze_query::tracked(returns(copy))]
pub(crate) fn io(db: &Db, f: FunctionId) -> Io {
    let module = hir::Function::from(f).module(db);
    let krate = module.krate(db);
    let is = |name: &str| facts::crate_is(db, krate, name);
    if is("std") {
        return match top_module(db, module).as_ref().map(hir::Name::as_str) {
            Some("fs") => Io::Root(Kind::Fs),
            Some("net") => Io::Root(Kind::Net),
            Some("process") => Io::Root(Kind::Process),
            Some("io") if module.name(db).is_some_and(|it| it.as_str() == "stdio") => {
                Io::Root(Kind::Stdio)
            }
            Some("sync" | "thread" | "time" | "env" | "hash" | "panicking" | "rt" | "alloc") => {
                Io::Stop
            }
            _ => Io::Walk,
        };
    }
    if is("libc") || is("rustix") || is("nix") {
        return Io::Root(Kind::Syscall);
    }
    if is("tokio") || is("mio") {
        return Io::Root(Kind::Async);
    }
    if is("blaze_core") {
        if driver_effect(db, f) {
            return Io::Root(Kind::Effects);
        }
        let top = top_module(db, module);
        if matches!(top.as_ref().map(hir::Name::as_str), Some("lock" | "wait")) {
            return Io::Stop;
        }
    }
    if STOPS.iter().any(|&it| is(it)) {
        return Io::Stop;
    }
    Io::Walk
}

/// The name of the module under `module`'s crate root that holds it (`fs` for
/// `std::fs::File`'s), `None` at the root.
fn top_module(db: &Db, module: hir::Module) -> Option<hir::Name> {
    module.path_to_root(db).into_iter().rev().find_map(|it| it.name(db))
}

/// Whether `f` is `submit` or `poll` of blaze-core's `Driver`: the trait's own method (a call
/// through `impl Driver`) or an impl's.
fn driver_effect(db: &Db, f: FunctionId) -> bool {
    let name = FunctionSignature::of(db, f).name.as_str();
    if !matches!(name, "submit" | "poll") {
        return false;
    }
    let Some(item) = hir::Function::from(f).as_assoc_item(db) else { return false };
    let trait_ = match item.container(db) {
        hir::AssocItemContainer::Trait(it) => Some(it),
        hir::AssocItemContainer::Impl(imp) => imp.trait_(db),
    };
    trait_.is_some_and(|it| {
        it.name(db).as_str() == "Driver"
            && facts::crate_is(db, it.module(db).krate(db), "blaze_core")
    })
}

/// Whether a call of `f` is a lock's own: an acquire, a release, a guard's `Deref`.
fn of_a_lock(db: &Db, f: FunctionId) -> bool {
    let module = hir::Function::from(f).module(db);
    let krate = module.krate(db);
    let is = |name: &str| facts::crate_is(db, krate, name);
    if is("parking_lot") || is("parking_lot_core") || is("lock_api") {
        return true;
    }
    if is("blaze_core") {
        return top_module(db, module).as_ref().map(hir::Name::as_str) == Some("lock");
    }
    if is("std") {
        return top_module(db, module).as_ref().map(hir::Name::as_str) == Some("sync")
            && facts::impl_adt(db, f).is_some_and(|it| is_guard(db, it) || lock_name(db, it));
    }
    false
}

/// Whether `adt` is one of std's locks.
fn lock_name(db: &Db, adt: hir::Adt) -> bool {
    matches!(adt.name(db).as_str(), "Mutex" | "RwLock" | "ReentrantLock")
}

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(Lint::LockHeldAcrossSlow) {
        return;
    }
    let found = find(cx);
    facts::report(cx, found);
}

/// Why a call is slow, as a finding says it.
enum Slow {
    /// It reaches IO: the root's path and which IO it is, when the root is at most
    /// [`LONGEST_CHAIN`] calls down.
    Io(Option<(String, Kind)>),
    /// A method of this std stream trait on a receiver the body cannot see into.
    Stream(&'static str),
    /// It reaches the global allocator.
    Alloc,
}

impl Slow {
    fn say(&self) -> String {
        match self {
            Slow::Io(Some((root, kind))) => {
                format!("which reaches {} through `{root}`", kind.say())
            }
            Slow::Io(None) => format!("which reaches IO more than {LONGEST_CHAIN} calls down"),
            Slow::Stream(trait_) => {
                format!("which reads or writes through `{trait_}` on a receiver it cannot see into")
            }
            // alloc: a message's part
            Slow::Alloc => "which allocates on the global heap".to_owned(),
        }
    }
}

fn find(cx: &Cx<'_, '_>) -> Vec<Finding> {
    // alloc: the findings of one body, reported at the edge
    let mut found = Vec::new();
    let Some(analysis) = cx.analysis() else { return found };
    let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return found };
    let db = cx.db();
    let body = analysis.body();
    let infer = analysis.infer();
    let env = ParamEnvAndCrate {
        param_env: db.trait_environment(def.generic_def(db)),
        krate: def.krate(db),
    };
    // alloc: the slow calls of one body, a few
    let mut slow: Vec<(ExprId, FunctionId, Slow)> = Vec::new();
    for (id, expr) in body.exprs() {
        if !matches!(expr, Expr::Call { .. } | Expr::MethodCall { .. }) {
            continue;
        }
        let Some((f, args)) = alloc::written_callee(infer, body, id) else { continue };
        if let Some(why) = slow_call(db, env, body, id, f, args) {
            slow.push((id, f, why));
        }
    }
    if slow.is_empty() {
        return found;
    }
    // A call that may park the fiber is the may-suspend check's and `held_across_wait`'s: it is
    // left to them, and a crate whose parks can't be known is refused there, never guessed.
    if !cx.suspend_checked() {
        return found;
    }
    let points = hir_ty_suspend::suspend_points(db, def.krate(db), def);
    slow.retain(|(id, ..)| {
        !points.iter().any(|point| {
            matches!(point.call.span, MirSpan::ExprId(it) | MirSpan::ExprEnd(it) if it == *id)
        })
    });
    if slow.is_empty() {
        return found;
    }
    let Ok(root) = db.runtime_mir_body(InferBodyId::DefWithBodyId(def)) else { return found };
    // alloc: the body's MIR and its closures', a few
    let mut bodies: Vec<&MirBody<'_>> = vec![root];
    let mut at = 0;
    while at < bodies.len() && bodies.len() < MOST_BODIES {
        let mir = bodies[at];
        for &closure in &mir.closures {
            if let Ok(it) = db.runtime_mir_body_for_closure(closure) {
                bodies.push(it);
            }
        }
        at += 1;
    }
    let target = cx.krate().to_display_target(db);
    for mir in bodies {
        // alloc: the slow calls of one MIR body, a few
        let mut blocks: Vec<BasicBlockId> = Vec::new();
        // alloc: as above
        let mut calls: Vec<(ExprId, FunctionId, &Slow)> = Vec::new();
        for (block, data) in mir.basic_blocks.iter() {
            let Some(terminator) = &data.terminator else { continue };
            let (TerminatorKind::Call { .. }, MirSpan::ExprId(expr)) =
                (&terminator.kind, terminator.span)
            else {
                continue;
            };
            let Some((_, f, why)) = slow.iter().find(|(it, ..)| *it == expr) else { continue };
            blocks.push(block);
            calls.push((expr, *f, why));
        }
        if blocks.is_empty() {
            continue;
        }
        blaze_mem::frame::task(blaze_mem::Tag::Diagnostics, |frame| {
            let across = mir.held_across_calls_in(frame, &blocks);
            for (&(expr, f, why), locals) in calls.iter().zip(across.iter()) {
                // alloc: the guards one finding names, one or two
                let mut guards: Vec<String> = Vec::new();
                for &local in locals.iter() {
                    let ty = analysis.ty(mir.locals[local].ty.as_ref());
                    if ty.is_reference()
                        || facts::holds(db, &ty, HOLDS_DEPTH, &mut |adt| is_guard(db, adt))
                            .is_none()
                    {
                        continue;
                    }
                    let binding = mir
                        .binding_locals
                        .iter()
                        .find_map(|(binding, &it)| (it == local).then_some(binding));
                    let ty = ty.display(db, target);
                    let said = match binding {
                        Some(binding) => format!("`{}` (`{ty}`)", body[binding].name.as_str()),
                        None => format!("a temporary `{ty}`"),
                    };
                    if !guards.contains(&said) {
                        guards.push(said);
                    }
                }
                let Some(first) = guards.first() else { continue };
                let Some(range) = analysis.expr_range(expr) else { continue };
                let call = fn_name(db, f);
                let message = match &*guards {
                    [_] => format!(
                        "the lock guard {first} is held across `{call}`, {}: {RULE}",
                        why.say()
                    ),
                    _ => format!(
                        "the lock guards {} are held across `{call}`, {}: {RULE}",
                        guards.join(", "),
                        why.say()
                    ),
                };
                let note = "every task that wants the lock queues behind its holder while the call waits on IO or \
                            the allocator; `blaze debug locks` ranks the convoys this makes";
                facts::push(
                    &mut found,
                    Lint::LockHeldAcrossSlow,
                    range,
                    message,
                    Some(note.to_owned()), // alloc: a finding's note
                );
            }
        });
    }
    found
}

/// Why the call `id` of `f` with `args` is slow, if it is.
fn slow_call<'db>(
    db: &'db Db,
    env: ParamEnvAndCrate<'db>,
    body: &Body,
    id: ExprId,
    f: FunctionId,
    args: GenericArgs<'db>,
) -> Option<Slow> {
    if of_a_lock(db, f) {
        return None;
    }
    if let Some(trait_) = unseen_stream(db, f, args) {
        return Some(Slow::Stream(trait_));
    }
    let resolved = alloc::resolve(db, env, f, args);
    if alloc::instance_reaches_root(db, env, resolved, true, Roots::Io) {
        return Some(Slow::Io(io_root(db, env, resolved)));
    }
    let allocates = alloc::instance_reaches(db, env, resolved, false)
        && !alloc::grows_kept_buffer(db, body, id, f);
    allocates.then_some(Slow::Alloc)
}

/// The IO root the walk from `first` reaches, its path and kind; `None` when the chain is
/// longer than [`LONGEST_CHAIN`] or ends at no root.
fn io_root<'db>(
    db: &'db Db,
    env: ParamEnvAndCrate<'db>,
    first: Resolved<'db>,
) -> Option<(String, Kind)> {
    let mut at = first;
    for _ in 0..LONGEST_CHAIN {
        if let Resolved::Function(f, _) = at
            && let Io::Root(kind) = io(db, f)
        {
            return Some((root_path(db, f), kind));
        }
        at = alloc::next_step(db, env, at, true, Roots::Io)?;
    }
    None
}

/// The std stream trait (`std::io::Read`, `Write` or `BufRead`) `f` is a method of, when the
/// call's receiver is one the body cannot see into: a `dyn` or a type parameter, behind any
/// references.
fn unseen_stream(db: &Db, f: FunctionId, args: GenericArgs<'_>) -> Option<&'static str> {
    let item = hir::Function::from(f).as_assoc_item(db)?;
    let hir::AssocItemContainer::Trait(trait_) = item.container(db) else { return None };
    let module = trait_.module(db);
    if !facts::crate_is(db, module.krate(db), "std")
        || module.name(db).is_none_or(|it| it.as_str() != "io")
    {
        return None;
    }
    let said = match trait_.name(db).as_str() {
        "Read" => "std::io::Read",
        "Write" => "std::io::Write",
        "BufRead" => "std::io::BufRead",
        _ => return None,
    };
    let mut receiver = args.type_at(0);
    while let TyKind::Ref(_, inner, _) = receiver.kind() {
        receiver = inner;
    }
    matches!(receiver.kind(), TyKind::Dynamic(..) | TyKind::Param(_)).then_some(said)
}
